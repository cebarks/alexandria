//! Task 6 third lane — the remaining eight `#[tool]` handlers must answer as * somebody *, never as
//! "wherever this row happens to be".
//!
//! Tasks 6 lanes one and two scoped `store_memory`, `import_document`, `retrieve_memories`,
//! `update_memory` and `delete_memory`. This file is the other eight: `recall`, `get_session`,
//! `list_sessions`, `finalize_session`, `set_reminder`, `check_reminders`, `list_reminders`,
//! `cancel_reminder`. Each is driven through the **real** auth gate against the **real**
//! [`AlexandriaServer`], so the principal is the transport's own — there is no fixture that hands a
//! handler a context, and `rmcp`'s `Peer::new` being `pub(crate)` means none could.
//!
//! Two claims per tool, both necessary:
//!
//! * **Isolation.** A row belonging to another account is invisible, and a foreign id answers exactly as
//!   an absent one. A tool that merely returns fewer rows is not enough — the *count*, the *labels* and
//!   the *sample* each have to be the caller's own, because those are the surfaces that leak without ever
//!   showing a body.
//! * **Fail-closed admission.** A request the gate did not admit as anybody must be refused rather than
//!   served from a default. `every_remaining_handler_refuses_a_request_with_no_principal` is the case
//!   that would otherwise be a silent fallback.
//!
//! The session cases are also the closure of the v010 regression: lane one re-keyed the handle index to
//! `(account, external_id)` and lane two left the session tools on the unscoped repo, so hosted sessions
//! were written with no owner and — measured, not assumed — a UNIQUE index over a `NONE` component
//! deduplicates nothing. `hosted_session_writes_are_account_local_and_deduplicated` is the assertion that
//! that is no longer true.

mod fixtures;
#[path = "scoped_request_tools_test/import_partition.rs"]
mod import_partition;
#[path = "scoped_request_tools_test/postcommit_activity.rs"]
mod postcommit_activity;

use alexandria_mcp::auth::mcp_transport::AuthEvents;
use alexandria_mcp::auth::{Admission, AuthMiddleware, AuthService, McpSessionRegistry};
use alexandria_mcp::server::{AlexandriaServer, HeatSettings};
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::access::SystemContext;
use alexandria_storage::models::OperationId;
use alexandria_storage::repos::{AuthRepo, IdentityRepo, ScopedSessionRepo};
use alexandria_storage::{Database, schema};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header::CONTENT_TYPE};
use chrono::Utc;
use fixtures::{MCP_PATH, read_body, router_post, with_bearer, with_session};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::{Value as Json, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tower::ServiceExt;

/// A provider that counts calls and returns a fixed unit vector.
#[derive(Clone)]
struct CountingEmbedding {
    calls: Arc<AtomicUsize>,
}

impl CountingEmbedding {
    fn shared() -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                calls: Arc::clone(&calls),
            },
            calls,
        )
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for CountingEmbedding {
    /// Vectors keyed on the text, **not** one constant.
    ///
    /// A constant vector makes every cosine similarity 1.0, so all seeded rows tie and everything comes
    /// back inside the result limit. That quietly breaks any assertion of the form "this row's heat
    /// moved, therefore activation ran": a *returned* row has its access recorded and its heat reset to
    /// 1.0 by `record_accesses`, which is a different writer with a different reason. Here only the text
    /// carrying `WARM_QUERY_MARK` points at `[1, 0]`; the neighbours point away from the query, so they
    /// are reached through the edge and nowhere else — which is exactly what makes the warm attributable.
    /// (AGENTS.md documents the same trap for `StubEmbedding` in the debug suite.)
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts
            .iter()
            .map(|text| {
                if text.contains(WARM_QUERY_MARK) {
                    vec![1.0f32, 0.0f32]
                } else {
                    vec![0.0f32, 1.0f32]
                }
            })
            .collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "task6-third"
    }
}

/// One enrolled owner: the handles enrollment produced, and the bearer a hosted request presents.
struct Owner {
    account: String,
    workspace: String,
    bearer: String,
    /// Two projects, so "the owner's *other* project" is a real row rather than a made-up id: a foreign
    /// project id that does not exist would pass the cross-project assertions for the wrong reason.
    project: String,
    #[allow(dead_code)]
    project_other: String,
}

/// The real server behind the real gate, over one store holding every account a case names.
struct Tools {
    db: Arc<Database>,
    router: Router,
    system: SystemContext,
}

impl Tools {
    async fn hosted() -> Self {
        let mut db = Database::connect_embedded().await.unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let system = db.claim_system_authority().unwrap();
        let db = Arc::new(db);

        let embedding = CountingEmbedding::shared();
        let server = AlexandriaServer::new(
            Arc::clone(&db),
            Arc::new(embedding.0),
            0.75,
            HeatSettings::default(),
        );

        let sessions = Arc::new(LocalSessionManager::default());
        let events = AuthEvents::closing_sessions_in(Arc::clone(&sessions));
        let service = Arc::new(AuthService::admitting(Arc::clone(&db), events.clone()));
        let admission = Admission::bound_to(events.clone());
        let registry = McpSessionRegistry::over(&events);
        let gate = AuthMiddleware::admitting(service, admission, registry);

        let mcp: StreamableHttpService<AlexandriaServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(server.clone()),
                Arc::clone(&sessions),
                StreamableHttpServerConfig::default()
                    .with_sse_keep_alive(Some(Duration::from_millis(50))),
            );
        let router =
            Router::new()
                .nest_service(MCP_PATH, mcp)
                .layer(axum::middleware::from_fn_with_state(
                    gate,
                    alexandria_mcp::auth::http::admit,
                ));

        Self { db, router, system }
    }

    /// Enrolls an owner with a live agent token plus one project it owns.
    ///
    /// The chain is the sealed one and nothing else: `bootstrap_owner` writes the rows,
    /// `local_principal` under this store's claimed capability yields the trusted-local principal that is
    /// allowed to *mint*, `mint_agent_token` writes a real credential row, and
    /// `authenticate_agent_token` turns it into the hosted principal the transport will resolve. The
    /// authentication is not decorative — it proves the bearer handed to the router works, so a refusal
    /// below is about the request path and not about a credential that never existed.
    async fn owner(&self, login: &str) -> Owner {
        let ids = IdentityRepo::new(self.db.inner())
            .bootstrap_owner(
                login,
                &format!("$argon2id$fixture-prepared-hash-{login}-padding"),
                &OperationId::new(format!("task6-third-{login}")),
            )
            .await
            .unwrap();
        let access = alexandria_storage::access::AccessRepo::new(&self.db);
        let local = access
            .local_principal(&self.system, &ids.account)
            .await
            .unwrap();
        let context = access.authorize(&local).await.unwrap();
        let token = AuthRepo::new(self.db.inner())
            .mint_agent_token(&context, "task6-third-agent", Utc::now())
            .await
            .unwrap();
        let bearer = token.secret.render().expose().to_string();
        AuthRepo::new(self.db.inner())
            .authenticate_agent_token(&bearer, Utc::now())
            .await
            .unwrap();

        let workspace = {
            let mut r = self
                .db
                .inner()
                .query("SELECT id FROM workspace WHERE owner = $owner LIMIT 1;")
                .bind(("owner", ids.account.as_record_id().clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
            let rows: Vec<Json> = r.take(0).unwrap();
            rows[0]["id"].as_str().unwrap().to_string()
        };
        let projects = self.tools_project_pair(&workspace).await;
        Owner {
            account: alexandria_storage::record_id_to_string(ids.account.as_record_id()),
            workspace,
            bearer,
            project: projects.0,
            project_other: projects.1,
        }
    }

    /// The hosted `OwnerContext` behind a bearer, produced by the same sealed chain the transport runs:
    /// authenticate the token, then `authorize` the principal it yields.
    async fn owner_context(&self, owner: &Owner) -> alexandria_storage::access::OwnerContext {
        let principal = AuthRepo::new(self.db.inner())
            .authenticate_agent_token(&owner.bearer, Utc::now())
            .await
            .unwrap();
        alexandria_storage::access::AccessRepo::new(&self.db)
            .authorize(&principal)
            .await
            .unwrap()
    }

    /// The hosted `OwnerContext` behind a bearer, exactly as `authorize` issues it for a request.
    async fn owner_for(&self, owner: &Owner) -> alexandria_storage::access::OwnerContext {
        let principal = AuthRepo::new(self.db.inner())
            .authenticate_agent_token(&owner.bearer, Utc::now())
            .await
            .unwrap();
        alexandria_storage::access::AccessRepo::new(&self.db)
            .authorize(&principal)
            .await
            .unwrap()
    }

    /// Issues the same pair of contexts `AccessRepo` hands the request path, from the bearer's own
    /// authenticated principal. Crate-private producers only — no test-only authority constructor.
    async fn scope_for(&self, owner: &Owner) -> alexandria_storage::access::ScopedContext {
        let access = alexandria_storage::access::AccessRepo::new(&self.db);
        let principal = AuthRepo::new(self.db.inner())
            .authenticate_agent_token(&owner.bearer, Utc::now())
            .await
            .unwrap();
        let context = access.authorize(&principal).await.unwrap();
        access
            .select(
                &context,
                alexandria_storage::models::ReadSelection::Auto(None),
            )
            .await
            .unwrap()
    }

    async fn handshake(&self, bearer: &str) -> String {
        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri(MCP_PATH)
            .header("authorization", format!("Bearer {bearer}"))
            .header(CONTENT_TYPE, "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .body(Body::from(
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": {"name": "task6-third", "version": "0"}
                    }
                })
                .to_string(),
            ))
            .unwrap();
        request
            .headers_mut()
            .insert("host", HeaderValue::from_static("127.0.0.1:3000"));
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("the router serves");
        assert_eq!(response.status(), StatusCode::OK, "handshake admitted");
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .expect("a session id");
        let _ = read_body(response.into_body()).await;
        let notice = with_protocol(with_bearer(
            with_session(
                router_post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
                &session,
            ),
            bearer,
        ));
        let response = self.router.clone().oneshot(notice).await.expect("served");
        let _ = read_body(response.into_body()).await;
        session
    }

    /// Calls a tool and returns its result payload.
    ///
    /// A `Json::Null` here means *no frame parsed at all*, which is a transport-level outcome rather than a
    /// tool answer, so the raw status and body are carried in the panic message. Without that, a broken
    /// request reads as "the tool refused", and a cross-account leak looks identical to a refused call.
    async fn call(&self, bearer: &str, session: &str, name: &str, arguments: Json) -> Json {
        let request = with_protocol(with_bearer(
            with_session(router_post(tool_body(name, arguments)), session),
            bearer,
        ));
        let response = self.router.clone().oneshot(request).await.expect("served");
        let status = response.status();
        let bytes = read_body(response.into_body()).await;
        fixtures::frames_of(&bytes)
            .into_iter()
            .filter_map(|frame| {
                frame
                    .get("result")
                    .cloned()
                    .or_else(|| frame.get("error").cloned())
            })
            .next()
            .unwrap_or_else(|| {
                panic!(
                    "{name}: no JSON-RPC result frame; status {status}, body {}",
                    String::from_utf8_lossy(&bytes)
                )
            })
    }

    /// Domain state relevant to refused foreign-ID probes; auth lease bookkeeping is separate.
    async fn foreign_probe_state(&self) -> Vec<Vec<Json>> {
        let mut state = Vec::new();
        for sql in [
            "SELECT * FROM session ORDER BY id;",
            "SELECT * FROM reminder ORDER BY id;",
            "SELECT * FROM fact ORDER BY id;",
            "SELECT * FROM memory_edge ORDER BY id;",
            "SELECT * FROM contains_memory ORDER BY id;",
            "SELECT * FROM contains_session_memory ORDER BY id;",
            "SELECT * FROM extracted_from ORDER BY id;",
            "SELECT * FROM has_provenance ORDER BY id;",
            "SELECT * FROM provenance ORDER BY id;",
            "SELECT * FROM cluster ORDER BY id;",
            "SELECT * FROM raw ORDER BY id;",
            "SELECT * FROM semantic_operation ORDER BY id;",
            "SELECT * FROM semantic_snapshot ORDER BY id;",
            "SELECT * FROM classification_proposal ORDER BY id;",
            "SELECT * FROM maintenance_log ORDER BY id;",
            "SELECT * FROM heat_state ORDER BY id;",
        ] {
            state.push(self.rows(sql).await);
        }
        state
    }

    async fn rows(&self, sql: &str) -> Vec<Json> {
        let mut response = self
            .db
            .inner()
            .query(sql.to_string())
            .await
            .unwrap()
            .check()
            .unwrap();
        response.take(0).unwrap()
    }

    /// Seeds an owned, labelled cluster **through the seam production uses**.
    ///
    /// This used to be a `CREATE … SET account = $account` with the fixture's own ids, and that is the
    /// shape the review caught: production could not make a row like it (no scoped cluster create existed
    ///), so the owner's cluster list was empty in every case and the "no foreign label" assertion passed
    /// for an owner who had no cluster to lose. `ScopedClusterRepo::create` is what `store_memory` and
    /// `import_document` call, so the row below is one a real request could have written.
    async fn seed_cluster(&self, label: &str, owner: &Owner) -> String {
        let owner_context = self.owner_for(owner).await;
        let scope = self.scope_for(owner).await;
        let created = alexandria_storage::repos::ScopedClusterRepo::new(
            self.db.inner(),
            &owner_context,
            &scope,
        )
        .create(Some(label), &[1.0, 0.0], Utc::now())
        .await
        .unwrap();
        alexandria_storage::record_id_to_string(
            created.id.as_ref().expect("a created cluster has an id"),
        )
    }

    /// Links a cluster to a fact through the `contains_memory` relation.
    async fn link_cluster(&self, cluster: &str, fact: &str) {
        self.db
            .inner()
            .query("RELATE $cluster->contains_memory->$fact;")
            .bind(("cluster", record(cluster)))
            .bind(("fact", record(fact)))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    /// Creates the two projects an owner needs, and returns their handles.
    ///
    /// The second one is what makes "the owner's other project" a real row rather than a made-up id; a
    /// project that does not exist would pass the cross-project assertions for the wrong reason.
    async fn tools_project_pair(&self, workspace: &str) -> (String, String) {
        let mut made = Vec::with_capacity(2);
        for alias in ["alpha", "beta"] {
            let mut response = self
                .db
                .inner()
                .query(
                    "CREATE project SET workspace = $ws, normalized_alias = $alias RETURN AFTER;",
                )
                .bind(("ws", record(workspace)))
                .bind(("alias", alias.to_string()))
                .await
                .unwrap()
                .check()
                .unwrap();
            let rows: Vec<Json> = response.take(0).unwrap();
            made.push(rows[0]["id"].as_str().unwrap().to_string());
        }
        (made.remove(0), made.remove(0))
    }

    /// Links two facts with an edge activation can walk. Strength 1.0 so the propagated delta is
    /// unambiguously above the engine's negligible floor at one hop.
    async fn link_edge(&self, from: &str, to: &str) {
        self.db
            .inner()
            .query("RELATE $from->memory_edge->$to SET edge_type = 'related', strength = 1.0;")
            .bind(("from", record(from)))
            .bind(("to", record(to)))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    /// A same-account fact that is soft-deleted: same partition, fails the live predicate.
    async fn seed_fact_deleted(
        &self,
        key: &str,
        content: &str,
        owner: &Owner,
        scope: &str,
    ) -> String {
        let mut response = self
            .db
            .inner()
            .query(
                "CREATE type::record('fact', $key) SET content = $content, confidence = 0.5, \
                 embedding = [1.0, 0.0], tags = [], deleted = true, account = $account, \
                 workspace = $workspace, scope = $scope RETURN AFTER;",
            )
            .bind(("key", key.to_string()))
            .bind(("content", content.to_string()))
            .bind(("account", record(&owner.account)))
            .bind(("workspace", record(&owner.workspace)))
            .bind(("scope", scope.to_string()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Json> = response.take(0).unwrap();
        rows[0]["id"].as_str().unwrap().to_string()
    }

    /// A same-account, same-partition fact that is quarantined.
    async fn seed_fact_quarantined(
        &self,
        key: &str,
        content: &str,
        owner: &Owner,
        scope: &str,
    ) -> String {
        let mut response = self
            .db
            .inner()
            .query(
                "CREATE type::record('fact', $key) SET content = $content, confidence = 0.5, \
                 embedding = [1.0, 0.0], tags = [], deleted = false, quarantined_at = time::now(), \
                 account = $account, workspace = $workspace, scope = $scope RETURN AFTER;",
            )
            .bind(("key", key.to_string()))
            .bind(("content", content.to_string()))
            .bind(("account", record(&owner.account)))
            .bind(("workspace", record(&owner.workspace)))
            .bind(("scope", scope.to_string()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Json> = response.take(0).unwrap();
        rows[0]["id"].as_str().unwrap().to_string()
    }

    /// `rows` with a **typed** record bind.
    ///
    /// `account` is a `record<account>` column, so comparing it to a string in SurrealQL matches
    /// nothing — a `Json::String` bind silently returns zero rows and would make every "did not leak"
    /// assertion below pass vacuously. The record is bound as the `RecordId` the store handed out.
    async fn rows_for_account(&self, sql: &str, account: &str) -> Vec<Json> {
        let mut response = self
            .db
            .inner()
            .query(sql.to_string())
            .bind(("acc", record(account)))
            .await
            .unwrap()
            .check()
            .unwrap();
        response.take(0).unwrap()
    }

    async fn count(&self, sql: &str) -> i64 {
        self.rows(sql)
            .await
            .first()
            .and_then(|row| row.get("count"))
            .and_then(Json::as_i64)
            .unwrap_or(0)
    }

    /// Seeds a live, owner-stamped fact. Clustering and the tag/label surfaces need real rows.
    async fn seed_fact(
        &self,
        key: &str,
        content: &str,
        owner: &Owner,
        scope: &str,
        project: Option<&str>,
        tags: &[&str],
    ) -> String {
        let mut q = String::from(
            "CREATE type::record('fact', $key) SET content = $content, confidence = 0.5, \
             embedding = [1.0, 0.0], tags = $tags, deleted = false, account = $account, \
             workspace = $workspace, scope = $scope",
        );
        if project.is_some() {
            q.push_str(", project = $project");
        }
        q.push(';');
        let mut bind = self
            .db
            .inner()
            .query(q)
            .bind(("key", key.to_string()))
            .bind(("content", content.to_string()))
            .bind((
                "tags",
                tags.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            ))
            .bind(("account", record(&owner.account)))
            .bind(("workspace", record(&owner.workspace)))
            .bind(("scope", scope.to_string()));
        if let Some(project) = project {
            bind = bind.bind(("project", record(project)));
        }
        let mut created = bind.await.unwrap().check().unwrap();
        let rows: Vec<Json> = created.take(0).unwrap();
        rows[0]["id"].as_str().unwrap().to_string()
    }

    /// Seeds an owned session for a handle.
    async fn seed_session(&self, key: &str, handle: &str, owner: &Owner) -> String {
        let mut created = self
            .db
            .inner()
            .query(
                "CREATE type::record('session', $key) SET external_id = $handle, tags = [], \
                 account = $account, workspace = $workspace;",
            )
            .bind(("key", key.to_string()))
            .bind(("handle", handle.to_string()))
            .bind(("account", record(&owner.account)))
            .bind(("workspace", record(&owner.workspace)))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Json> = created.take(0).unwrap();
        rows[0]["id"].as_str().unwrap().to_string()
    }

    /// Seeds an owned pending reminder due now.
    async fn seed_reminder(
        &self,
        key: &str,
        message: &str,
        owner: &Owner,
        target: Option<&str>,
    ) -> String {
        let mut q = String::from(
            "CREATE type::record('reminder', $key) SET message = $message, schedule_kind = 'once', \
             due_at = d'2026-01-01T00:00:00Z', next_due_at = d'2026-01-01T00:00:00Z', \
             status = 'pending', delivered_count = 0, account = $account",
        );
        if target.is_some() {
            q.push_str(", legacy_target_project = $target");
        }
        q.push(';');
        let mut bind = self
            .db
            .inner()
            .query(q)
            .bind(("key", key.to_string()))
            .bind(("message", message.to_string()))
            .bind(("account", record(&owner.account)));
        if let Some(target) = target {
            bind = bind.bind(("target", target.to_string()));
        }
        let mut created = bind.await.unwrap().check().unwrap();
        let rows: Vec<Json> = created.take(0).unwrap();
        rows[0]["id"].as_str().unwrap().to_string()
    }
}

/// Bob's due reminder's consuming state, read straight from the store.
///
/// The message is a bound parameter rather than quoted into the statement: the value contains an
/// apostrophe, and hand-escaping it into SurrealQL is exactly the kind of thing that silently breaks the
/// fixture while the assertion still appears to pass.
async fn reminder_state(tools: &Tools) -> Vec<Json> {
    let mut response = tools
        .db
        .inner()
        .query("SELECT delivered_count, next_due_at, status FROM reminder WHERE message = $m;")
        .bind(("m", BOB_REMINDER.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    response.take(0).unwrap()
}

/// The message bob's fixture reminder carries. One spelling, so the seed and the readers cannot drift.
const BOB_REMINDER: &str = "bob private overdue thing";

/// The same for a reminder addressed by its record key.
async fn reminder_state_named(tools: &Tools, key: &str) -> Vec<Json> {
    let mut response = tools
        .db
        .inner()
        .query("SELECT status FROM reminder WHERE meta::id(id) = $key;")
        .bind(("key", key.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    response.take(0).unwrap()
}

/// Parses a `table:key` text handle back into the `RecordId` a bind needs.
fn record(text: &str) -> surrealdb::types::RecordId {
    surrealdb::types::RecordId::parse_simple(text).expect("a handle this file produced parses")
}

fn tool_body(name: &str, arguments: Json) -> Json {
    json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    })
}

fn with_protocol(request: axum::extract::Request<Body>) -> axum::extract::Request<Body> {
    let mut request = request;
    request.headers_mut().insert(
        "mcp-protocol-version",
        HeaderValue::from_static("2025-06-18"),
    );
    request
}

fn text_of(result: &Json) -> String {
    if let Some(message) = result
        .get("structuredContent")
        .and_then(|c| c.get("message"))
        .and_then(Json::as_str)
    {
        return message.to_string();
    }
    result
        .get("content")
        .and_then(Json::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Json::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| result.to_string())
}

fn refused(result: &Json) -> bool {
    result["isError"].as_bool().unwrap_or(false)
        || result
            .get("structuredContent")
            .and_then(|c| c.get("status"))
            .and_then(Json::as_str)
            == Some("error")
}

// ---------------------------------------------------------------------------------------------
// The session tools: the v010 regression closure
// ---------------------------------------------------------------------------------------------

/// Hosted session writes are account-local, and the deduplication v010's index promises is real.
///
/// The premise matters: `finalize_session` updates a session and never creates one, so the handle has to
/// exist first. It is created through the path that *does* own implicit creation — `store_memory` with a
/// session id — which is why the store below carries an explicit partition target for the trusted-local
/// case and, for a hosted bearer, is refused before any write (D4). The assertions are then about the
/// session rows: each owner's handle resolves to their own row, a repeat store reuses it rather than
/// adding a second, and no row is written with a null owner.
///
/// The handle is created through [`alexandria_storage::repos::ScopedSessionRepo::find_or_create`] — the
/// seam the write handlers use — because a hosted request is refused by D4 before it can store anything,
/// so the transport cannot reach implicit creation. That is a narrowing, not a substitute: the tool-level
/// store path is asserted in-crate beside the import pairing test.
#[tokio::test(flavor = "multi_thread")]
async fn hosted_session_writes_are_account_local_and_deduplicated() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t3-sess-alice").await;
    let bob = tools.owner("t3-sess-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;

    // The session tools' own repository, which is what every `#[tool]` session handler and *both* write
    // handlers' linkage now run on. Not `do_store_memory`: that is the interim unscoped arm, and since
    // the fifth lane the store tool creates its session through `ScopedSessionRepo` too — which is why
    // this fixture goes through the repo directly rather than pretending a hosted bearer can store. Under
    // D4 a hosted request is refused before any write, so the store→session path itself is pinned
    // in-crate, in `a_trusted_local_store_is_retrievable_by_its_own_reader`.
    for (owner, handle) in [(&alice, "pi:shared-handle"), (&bob, "pi:shared-handle")] {
        let repo = alexandria_storage::repos::ScopedSessionRepo::new(
            tools.db.inner(),
            tools.owner_context(owner).await,
        );
        repo.find_or_create(handle, Some("pi"), Some("model"))
            .await
            .expect("a scoped first store must create the caller's own session");
        // A second store under the same handle must reuse the row, not collide and not duplicate.
        let again = repo
            .find_or_create(handle, Some("other-agent"), None)
            .await
            .expect("a repeat store must reuse the caller's own session");
        assert_eq!(again.external_id, handle);
    }

    let rows = tools
        .rows("SELECT id, external_id, account FROM `session` WHERE external_id = 'pi:shared-handle';")
        .await;
    assert_eq!(
        rows.len(),
        2,
        "one handle across two accounts must be two account-local sessions, not one shared row"
    );
    for row in &rows {
        assert!(
            !row["account"].is_null(),
            "a scoped session was written with no owner: {row:?}"
        );
    }

    // Each owner's list shows the handle once — their own row — and their finalize lands on it.
    for (bearer, session, owner) in [
        (&alice.bearer, &alice_session, &alice),
        (&bob.bearer, &bob_session, &bob),
    ] {
        let listed = tools
            .call(bearer, session, "list_sessions", json!({}))
            .await;
        let rendered = listed.to_string();
        assert!(
            rendered.contains("pi:shared-handle"),
            "an owner's own session list omitted their own handle"
        );
        let mine = {
            let mut r = tools
                .db
                .inner()
                .query(
                    "SELECT count() FROM `session` WHERE external_id = 'pi:shared-handle' AND \
                     account = $acc GROUP ALL;",
                )
                .bind(("acc", record(&owner.account)))
                .await
                .unwrap()
                .check()
                .unwrap();
            r.take::<Vec<Json>>(0).unwrap()
        };
        assert_eq!(
            mine[0]["count"].as_i64(),
            Some(1),
            "one account must hold exactly one row for the shared handle"
        );
    }
    let total = tools
        .count("SELECT count() FROM `session` WHERE external_id = 'pi:shared-handle' GROUP ALL;")
        .await;
    assert_eq!(
        total, 2,
        "a shared handle must be two sessions, not one global row"
    );
}

/// A foreign session handle is indistinguishable from an absent one.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_session_handle_answers_as_an_absent_one() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t3-edge-alice").await;
    let bob = tools.owner("t3-edge-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;

    tools
        .seed_session("bob-owned-sess", "pi:bob-only", &bob)
        .await;

    let foreign = tools
        .call(
            &alice.bearer,
            &alice_session,
            "get_session",
            json!({"session_id": "pi:bob-only"}),
        )
        .await;
    let absent = tools
        .call(
            &alice.bearer,
            &alice_session,
            "get_session",
            json!({"session_id": "pi:nobody-has-this"}),
        )
        .await;
    assert_eq!(
        text_of(&foreign),
        text_of(&absent),
        "get_session distinguishes a foreign handle from an absent one"
    );
    assert_eq!(
        refused(&foreign),
        refused(&absent),
        "get_session differs in refusal shape between foreign and absent handles"
    );
    // And Bob's row is untouched by Alice's attempt to finalize it.
    let after = tools
        .rows("SELECT summary FROM `session` WHERE external_id = 'pi:bob-only';")
        .await;
    assert!(
        after[0]["summary"].is_null(),
        "alice's finalize reached bob's session row: {after:?}"
    );
}

/// `finalize_session` on a foreign handle must not write, and must not say it wrote.
#[tokio::test(flavor = "multi_thread")]
async fn finalizing_a_foreign_session_is_refused_and_writes_nothing() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t3-fin-alice").await;
    let bob = tools.owner("t3-fin-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;
    tools
        .seed_session("bob-finned", "pi:bob-finned", &bob)
        .await;

    let result = tools
        .call(
            &alice.bearer,
            &alice_session,
            "finalize_session",
            json!({"session_id": "pi:bob-finned", "summary": "alice was here"}),
        )
        .await;
    assert!(
        refused(&result),
        "finalizing bob's session reported success: {result}"
    );
    let stored = tools
        .rows("SELECT summary, tags FROM `session` WHERE external_id = 'pi:bob-finned';")
        .await;
    assert!(
        stored[0]["summary"].is_null(),
        "the refusal still wrote bob's summary: {stored:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The reminder tools
// ---------------------------------------------------------------------------------------------

/// The reminder list, the due sample and the delivery consumption are all the caller's own.
///
/// Three separate leaks live in these four tools: `list_reminders` enumerating another account's rows,
/// `check_reminders` *consuming* them (advancing `next_due_at` and bumping `delivered_count` on a row the
/// caller never created, which is destructive and invisible), and the read-only `due_reminders` piggyback
/// surfacing another account's message text on every lookup.
#[tokio::test(flavor = "multi_thread")]
async fn reminder_tools_never_list_deliver_or_consume_another_accounts_rows() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t3-rem-alice").await;
    let bob = tools.owner("t3-rem-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;

    // Bob owns the only due reminder. It is deliberately the *only* due row, so if alice's check
    // consumes anything, what it consumed can only have been bob's.
    let bobs_reminder = tools
        .seed_reminder("bobs-due", BOB_REMINDER, &bob, None)
        .await;
    let bob_state_before = reminder_state(&tools).await;
    assert_eq!(
        bob_state_before.len(),
        1,
        "bob's reminder was not seeded, so nothing below can prove it survived"
    );

    let listed = tools
        .call(&alice.bearer, &alice_session, "list_reminders", json!({}))
        .await;
    assert!(
        !listed.to_string().contains(BOB_REMINDER),
        "list_reminders surfaced bob's reminder text to alice"
    );
    assert_eq!(
        tools
            .rows_for_account(
                "SELECT id FROM reminder WHERE account = $acc;",
                &bob.account
            )
            .await
            .len(),
        1,
        "the store should hold exactly bob's row for this case to be meaningful"
    );

    let checked = tools
        .call(&alice.bearer, &alice_session, "check_reminders", json!({}))
        .await;
    assert!(
        !checked.to_string().contains(BOB_REMINDER),
        "check_reminders delivered bob's reminder to alice"
    );
    let bob_after = reminder_state(&tools).await;
    assert_eq!(
        bob_after, bob_state_before,
        "alice's check_reminders consumed bob's row"
    );

    // The read-only piggyback on a retrieval must not carry it either.
    let retrieved = tools
        .call(
            &alice.bearer,
            &alice_session,
            "retrieve_memories",
            json!({"query": "anything at all", "limit": 5}),
        )
        .await;
    assert!(
        !retrieved.to_string().contains(BOB_REMINDER),
        "the due_reminders piggyback leaked bob's message text"
    );
    let _ = bobs_reminder;
}

/// Trailing input on a valid owned reminder id must not cancel the prefix record.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_reminder_id_with_trailing_input_is_refused() {
    let tools = Tools::hosted().await;
    let owner = tools.owner("t3-can-trailing").await;
    let session = tools.handshake(&owner.bearer).await;
    let id = tools
        .seed_reminder("trailing-input-target", "still pending", &owner, None)
        .await;
    let malformed_id = format!("{id}; junk");

    let result = tools
        .call(
            &owner.bearer,
            &session,
            "cancel_reminder",
            json!({"id": malformed_id}),
        )
        .await;

    assert!(
        refused(&result),
        "cancel_reminder accepted trailing input and cancelled the valid prefix id: {result}"
    );
    let stored = reminder_state_named(&tools, "trailing-input-target").await;
    assert_eq!(
        stored[0]["status"].as_str(),
        Some("pending"),
        "a refused malformed id must leave the prefix reminder pending"
    );
}

/// A foreign reminder id answers as an absent one, and cancelling it changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_foreign_reminder_is_refused_and_changes_nothing() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t3-can-alice").await;
    let bob = tools.owner("t3-can-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;
    let bobs = tools
        .seed_reminder("bobs-target", "bob's reminder to cancel", &bob, None)
        .await;

    let foreign = tools
        .call(
            &alice.bearer,
            &alice_session,
            "cancel_reminder",
            json!({"id": bobs}),
        )
        .await;
    let absent = tools
        .call(
            &alice.bearer,
            &alice_session,
            "cancel_reminder",
            json!({"id": "reminder:never-existed-at-all"}),
        )
        .await;
    assert!(
        refused(&foreign),
        "cancelling bob's reminder reported success to alice: {foreign}"
    );
    assert_eq!(
        text_of(&foreign),
        text_of(&absent),
        "cancel_reminder distinguishes a foreign id from an absent one"
    );
    // The handler is guarded twice — an ownership lookup, then the write's own predicate — so an
    // assertion on the response alone passes vacuously when either layer is removed. This pins the
    // write guard at the layer it owns: the same `cancel` the handler calls, aimed directly at a row the
    // caller cannot see.
    let bob_scope = tools.scope_for(&bob).await;
    let alice_scope = tools.scope_for(&alice).await;
    assert!(
        !alexandria_storage::repos::ScopedReminderRepo::new(
            tools.db.inner(),
            tools.owner_for(&alice).await
        )
        .cancel(&bobs, Utc::now())
        .await
        .unwrap(),
        "the scoped cancel wrote a row outside its bound account"
    );
    let _ = (&bob_scope, &alice_scope);
    let stored = reminder_state_named(&tools, "bobs-target").await;
    assert_eq!(
        stored[0]["status"].as_str(),
        Some("pending"),
        "alice's cancel moved bob's reminder: {stored:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// recall, and the cluster label surface
// ---------------------------------------------------------------------------------------------

/// `recall` walks clusters, so its leak is a *label* and a membership count rather than a body.
///
/// A cluster name or a member count derived from another account's facts tells alice how many memories
/// somebody else stored and what they are called. Both come from the same unscoped counting path, so the
/// assertion is on the label text and on the count, not only on returned memories.
#[tokio::test(flavor = "multi_thread")]
async fn recall_surfaces_no_foreign_cluster_label_or_membership() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t3-rec-alice").await;
    let bob = tools.owner("t3-rec-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;

    // Bob's corpus, with its own cluster carrying an identifying label.
    let mut bobs_fact = String::new();
    for i in 0..3 {
        let id = tools
            .seed_fact(
                &format!("bob-fact-{i}"),
                &format!("bob private deployment note {i}"),
                &bob,
                "personal",
                None,
                &["bob-deployments"],
            )
            .await;
        if i == 0 {
            bobs_fact = id;
        }
    }
    let bobs_cluster = tools.seed_cluster("bob-deployment-history", &bob).await;
    let _ = &bobs_fact;
    tools.link_cluster(&bobs_cluster, &bobs_fact).await;

    let result = tools
        .call(
            &alice.bearer,
            &alice_session,
            "recall",
            json!({"query": "deployment history", "scope_handle": null}),
        )
        .await;
    let rendered = result.to_string();

    assert!(
        !rendered.contains("bob-deployment-history"),
        "recall named bob's cluster to alice: {rendered}"
    );

    // The layer above is guarded twice (the cluster list *and* the member read), so an assertion that
    // only checks the rendered response passes vacuously when one guard fails. These two pin each
    // guard at the layer it owns, through the same scoped reader the handler uses.
    let alice_scope = tools.scope_for(&alice).await;
    let labels = alexandria_storage::repos::ScopedClusterRepo::new(
        tools.db.inner(),
        &tools.owner_for(&alice).await,
        &alice_scope,
    )
    .list_with_counts()
    .await
    .unwrap();
    assert!(
        labels.is_empty(),
        "the scoped cluster list surfaced {} foreign cluster(s) with labels: {labels:?}",
        labels.len()
    );
    let replayed_handle = bobs_cluster.as_str();
    let bobs_scope = tools.scope_for(&bob).await;
    let bobs_members = alexandria_storage::repos::ScopedClusterRepo::new(
        tools.db.inner(),
        &tools.owner_for(&bob).await,
        &bobs_scope,
    )
    .get_members(replayed_handle)
    .await
    .unwrap();
    assert_eq!(
        bobs_members.len(),
        1,
        "the same replayed handle must contain Bob's seeded member"
    );
    let seeded_member = record(&bobs_fact);
    assert_eq!(bobs_members[0].id.as_ref(), Some(&seeded_member));
    let replayed = alexandria_storage::repos::ScopedClusterRepo::new(
        tools.db.inner(),
        &tools.owner_for(&alice).await,
        &alice_scope,
    )
    .get_members(replayed_handle)
    .await
    .unwrap();
    assert!(
        replayed.is_empty(),
        "alice replayed bob's cluster handle and read {} of bob's memories",
        replayed.len()
    );
    assert!(
        !rendered.contains("bob-deployments"),
        "recall surfaced bob's tag as a suggestion to alice"
    );
}

/// A positive `recall` control: the owner's own cluster, made by the seam production uses, comes back.
///
/// The leak test above asserts an *empty* answer for a caller who owns nothing, which is true whether or
/// not the workspace filter works — and that is exactly how the unscoped cluster create hid for four
/// lanes. Every cluster the request path made was created by `ClusterRepo::create`, which stamps
/// `label`/`centroid`/`depth` and no owner, so `ScopedClusterRepo::list` filtered all of them out and
/// broad `recall` returned zero candidates for every account in the store. A test that only checks for
/// absence cannot see a feature that died: this one checks presence.
#[tokio::test(flavor = "multi_thread")]
async fn recall_surfaces_the_callers_own_cluster_to_nobody_else() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t9-rec-alice").await;
    let bob = tools.owner("t9-rec-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;

    // Bob's cluster, created through `ScopedClusterRepo::create` — the same call the store and import
    // paths make — holding one of bob's own live memories.
    let bobs_fact = tools
        .seed_fact(
            "bob-owned-fact",
            "bob's private gateway rotation note",
            &bob,
            "personal",
            None,
            &["bob-gateways"],
        )
        .await;
    let bobs_cluster = tools.seed_cluster("bob-deployment-history", &bob).await;
    tools.link_cluster(&bobs_cluster, &bobs_fact).await;

    // Bob sees his own cluster, by name and by membership.
    let bob_sees = alexandria_storage::repos::ScopedClusterRepo::new(
        tools.db.inner(),
        &tools.owner_for(&bob).await,
        &tools.scope_for(&bob).await,
    )
    .list_with_counts()
    .await
    .unwrap();
    assert_eq!(
        bob_sees.len(),
        1,
        "the owner's own scoped cluster list found nothing the owner could have made"
    );
    assert_eq!(
        bob_sees[0].0.label.as_deref(),
        Some("bob-deployment-history"),
        "the owner cannot read the label of the cluster they created: {:?}",
        bob_sees[0].0
    );
    assert_eq!(bob_sees[0].1, 1, "bob's own cluster reports no members");

    let broad = tools
        .call(
            &bob.bearer,
            &bob_session,
            "recall",
            json!({"query": format!("{WARM_QUERY_MARK} gateway rotation"), "scope_handle": null}),
        )
        .await;
    let rendered = text_of(&broad);
    assert!(
        broad["structuredContent"]["clusters"]
            .as_array()
            .is_some_and(|clusters| !clusters.is_empty()),
        "broad `recall` returned no candidate cluster for the account that owns one: {rendered}"
    );
    assert!(
        rendered.contains("bob's private gateway rotation note"),
        "recall did not surface the owner's own representative memory: {rendered}"
    );

    // Alice, over the same store, gets nothing — and nothing that names bob's cluster or its member.
    let foreign = tools
        .call(
            &alice.bearer,
            &alice_session,
            "recall",
            json!({"query": format!("{WARM_QUERY_MARK} gateway rotation"), "scope_handle": null}),
        )
        .await;
    let foreign_text = text_of(&foreign);
    let foreign_clusters = foreign["structuredContent"]["clusters"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        foreign_clusters.is_empty(),
        "alice's broad recall returned {} candidate cluster(s), none of which can be hers: \
         {foreign_text}",
        foreign_clusters.len()
    );
    assert!(
        !foreign_text.contains("bob-deployment-history")
            && !foreign_text.contains("bob's private gateway rotation note"),
        "recall surfaced bob's cluster or memory to alice: {foreign_text}"
    );
}

// ---------------------------------------------------------------------------------------------
// Fail-closed admission
// ---------------------------------------------------------------------------------------------

/// A request the gate did not admit as anybody is refused by every handler, not served from a default.
///
/// This is the case the sealed-context design exists for: an unadmitted request has no principal, and
/// "no principal" must not be readable as "the configured local operator" or as "the whole store". The
/// assertion is deliberately weaker in shape than the others and stronger in reach — every one of the
/// eight tools is called, because a single handler keeping a legacy passthrough is the bug.
/// No request reaches any of the eight remaining handlers without a principal.
#[tokio::test(flavor = "multi_thread")]
async fn every_remaining_handler_refuses_a_request_with_no_principal() {
    let tools = Tools::hosted().await;
    // The claim a sealed-context design has to survive is the negative one: "no credential was
    // presented" must not become "then act as the configured local operator", and it must not become
    // "then read the whole store". Each of the eight tools is driven with NO bearer at all, and the gate
    // must refuse before handler code runs. A 401 per tool rather than an empty-but-successful payload,
    // because an empty payload is exactly what a silent fallback eventually looks like from outside.
    for (name, args) in [
        ("recall", json!({"query": "anything"})),
        ("get_session", json!({"session_id": "pi:any"})),
        ("list_sessions", json!({})),
        (
            "finalize_session",
            json!({"session_id": "pi:any", "summary": "x"}),
        ),
        (
            "set_reminder",
            json!({"message": "later", "due_at": "2030-01-01T00:00:00Z"}),
        ),
        ("check_reminders", json!({})),
        ("list_reminders", json!({})),
        ("cancel_reminder", json!({"id": "reminder:any"})),
    ] {
        let request = with_protocol(router_post(tool_body(name, args)));
        let response = tools.router.clone().oneshot(request).await.expect("served");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{name} answered a request that presented no credential"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Activation: `trigger_activation` reached through the `retrieve_memories` tool
// ---------------------------------------------------------------------------------------------

/// The phrase only the focal record and the query carry, so retrieval surfaces the focal record and not
/// its neighbours.
const WARM_QUERY_MARK: &str = "activation-focal-mark";

/// A `heat_state` row at a known, non-zero, distinguishable level.
///
/// The level matters: activation only *raises* heat, so starting everything at 1.0 would let a warm that
/// silently did nothing still read as "no change", and starting at 0.0 would make `None` (a missing row)
/// and `Some(0.0)` indistinguishable. 0.25 is neither, and it is the value every row of the lattice
/// starts at, so any row that moved is a row that was written.
async fn seed_heat(tools: &Tools, fact: &str) {
    tools
        .db
        .inner()
        .query(
            "CREATE heat_state SET memory = $fact, heat = 0.25, stability = 1.0, access_count = 0, \
             last_touched = d'2026-01-01T00:00:00Z', last_accessed_at = d'2026-01-01T00:00:00Z';",
        )
        .bind(("fact", record(fact)))
        .await
        .unwrap()
        .check()
        .unwrap();
}

/// Stored heat of one fact.
async fn heat_of(tools: &Tools, fact: &str) -> f64 {
    let mut response = tools
        .db
        .inner()
        .query("SELECT heat FROM heat_state WHERE memory = $fact LIMIT 1;")
        .bind(("fact", record(fact)))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<Json> = response.take(0).unwrap();
    rows.first()
        .and_then(|r| r["heat"].as_f64())
        .unwrap_or_else(|| panic!("no heat_state row for {fact} — the fixture must seed one"))
}

/// `retrieve_memories` must reach `trigger_activation`, and the warm must honour the partition.
///
/// This is the lane-three gap closed. Every piece already existed — `ScopedGraphReader`,
/// `ScopedHeatWriter`, `trigger_activation(scope, ..)` — but nothing proved the **request path** drove
/// them, so the scoped warm could have been dead code while every primitive test still passed.
///
/// The lattice is the brief's, narrowed to what a handler can lawfully reach: every tool admits
/// `ReadSelection::Auto(None)`, which is Personal only, because no public parameter names a project until
/// Task 8 and D4 forbids inferring one. So a *tool* retrieval can only surface Personal rows, and the
/// project-focal half of the matrix lives in `scoped_activation_test.rs`, which drives the same partition
/// rules one layer down with a real project partition. Claiming a single tool retrieval spanned both
/// partitions would be testing a population the request path cannot ask for.
///
/// Every row — allowed and forbidden alike — starts at the same 0.25, so "byte-stable" cannot be
/// satisfied by a row that was never warmable, and a missing `heat_state` row is a panic, not a `None`.
#[tokio::test(flavor = "multi_thread")]
async fn retrieve_memories_warms_only_same_partition_neighbors() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t4-act-alice").await;
    let bob = tools.owner("t4-act-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;

    let personal_focal = tools
        .seed_fact("a-focal", WARM_QUERY_MARK, &alice, "personal", None, &[])
        .await;
    // Allowed: same owner, same workspace, same partition, live.
    let allowed = tools
        .seed_fact(
            "a-allowed",
            "personal neighbour",
            &alice,
            "personal",
            None,
            &[],
        )
        .await;
    // Forbidden, one per reason the brief names.
    let project_off_personal = tools
        .seed_fact(
            "a-project-off-personal",
            "project row",
            &alice,
            "project",
            Some(&alice.project),
            &[],
        )
        .await;
    let unassigned = tools
        .seed_fact(
            "a-unassigned",
            "unassigned row",
            &alice,
            "unassigned",
            None,
            &[],
        )
        .await;
    let foreign = tools
        .seed_fact(
            "a-foreign",
            "another account row",
            &bob,
            "personal",
            None,
            &[],
        )
        .await;
    let deleted = tools
        .seed_fact_deleted("a-deleted", "deleted row", &alice, "personal")
        .await;
    let quarantined = tools
        .seed_fact_quarantined("a-quarantined", "quarantined row", &alice, "personal")
        .await;

    // One edge per case, from the same focal record, so a warm on any of them is a warm the partition
    // and liveness predicates had to stop rather than a neighbour that was never reachable.
    for neighbour in [
        &allowed,
        &project_off_personal,
        &unassigned,
        &foreign,
        &deleted,
        &quarantined,
    ] {
        tools.link_edge(&personal_focal, neighbour).await;
    }

    let all = [
        &personal_focal,
        &allowed,
        &project_off_personal,
        &unassigned,
        &foreign,
        &deleted,
        &quarantined,
    ];
    for id in &all {
        seed_heat(&tools, id).await;
    }
    for id in &all {
        assert_eq!(
            heat_of(&tools, id).await,
            0.25,
            "{id} did not start at the fixture level, so the comparison below is meaningless"
        );
    }

    let result = tools
        .call(
            &alice.bearer,
            &alice_session,
            "retrieve_memories",
            json!({"query": WARM_QUERY_MARK, "limit": 10}),
        )
        .await;
    assert!(
        result.to_string().contains(WARM_QUERY_MARK),
        "the retrieval surfaced nothing, so activation had no focal record: {result}"
    );

    // Positive control: the request path really did warm. If this fails, `trigger_activation` is
    // unreachable from `retrieve_memories` and the scoped writer is dead code.
    let allowed_heat = heat_of(&tools, &allowed).await;
    assert!(
        allowed_heat > 0.25,
        "retrieve_memories did not warm the focal record's own same-partition neighbour — the scoped \
         activation path is not reachable from the tool"
    );
    // The neighbour was reached through the edge and not returned, so it must carry an activation delta
    // rather than an access reset. 1.0 is what `record_accesses` writes, and landing on it here would
    // mean the row was surfaced — i.e. the positive control above passed for the wrong reason.
    assert!(
        (allowed_heat - 1.0).abs() > 1e-9,
        "the neighbour's heat is exactly the access-reset value, so it was surfaced by the retrieval \
         and the warm assertion below proves nothing: {allowed_heat}"
    );

    for (label, id) in [
        ("project row off a Personal focal", &project_off_personal),
        ("Unassigned row", &unassigned),
        ("another account's row", &foreign),
        ("soft-deleted row", &deleted),
        ("quarantined row", &quarantined),
    ] {
        assert_eq!(
            heat_of(&tools, id).await,
            0.25,
            "{label} was warmed by the tool-driven activation"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// §195 — surfaces a trusted-local write reaches through the unscoped repos
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// §195 leak/error matrix — the surfaces the request tools actually return
// ---------------------------------------------------------------------------------------------

/// Every request-facing read surface is the caller's own: tags, counts, session detail, memory
/// bodies, cluster labels.
///
/// The point of doing this as one matrix rather than one test per tool is that the leak shapes differ
/// per surface and a single-row assertion would let most of them pass unnoticed:
///
/// * **Tags** are content in miniature — `list_sessions`/`get_session` echo the owner's tag arrays, and a
///   distinctive tag is enough to identify what another account stored.
/// * **Counts** leak without returning a byte of content. `get_session`'s `memory_count` and
///   `list_sessions`' per-row count are derived from graph traversals, so a count that includes another
///   account's rows is a leak the response cannot be filtered down to fix.
/// * **Cluster labels** were covered separately (`recall_surfaces_no_foreign_cluster_label_or_membership`)
///   because they arrive through a different repo.
///
/// Each assertion has a matching positive control: the other account's row is proven present in the store
/// and proven visible to *its own* owner, so an empty result cannot be a fixture that seeded nothing.
#[tokio::test(flavor = "multi_thread")]
async fn no_request_tool_leaks_tags_counts_or_bodies_across_accounts() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t5-leak-alice").await;
    let bob = tools.owner("t5-leak-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;

    // Distinctive tags, one per account, plus one shared handle so the collision case is exercised too.
    let bob_fact = tools
        .seed_fact(
            "t5-bob-fact",
            "bob private body text",
            &bob,
            "personal",
            None,
            &["bob-only-tag"],
        )
        .await;
    let _ = &bob_fact;
    let alice_fact = tools
        .seed_fact(
            "t5-alice-fact",
            "alice private body text",
            &alice,
            "personal",
            None,
            &["alice-only-tag"],
        )
        .await;

    // A session per account on the SAME handle, each with one memory and its own tags.
    for (owner, handle, fact) in [
        (&alice, "t5:shared", &alice_fact),
        (&bob, "t5:shared", &bob_fact),
    ] {
        let repo = ScopedSessionRepo::new(tools.db.inner(), tools.owner_for(owner).await);
        repo.find_or_create(handle, Some("pi"), None).await.unwrap();
        repo.add_memory(handle, fact).await.unwrap();
        let summary = format!("{} summary", owner.account);
        let tag = format!("tag-{}", owner.account);
        repo.finalize(handle, Some(&summary), Some(&[tag]))
            .await
            .unwrap();
    }

    // Positive control: the rows exist, and each owner can see their own.
    assert_eq!(
        tools
            .rows_for_account("SELECT id FROM fact WHERE account = $acc;", &alice.account)
            .await
            .len(),
        1,
        "alice has no row of her own, so every 'did not leak' assertion below is vacuous"
    );
    assert_eq!(
        tools
            .rows_for_account("SELECT id FROM fact WHERE account = $acc;", &bob.account)
            .await
            .len(),
        1,
        "bob has no row of her own, so every 'did not leak' assertion below is vacuous"
    );

    for (viewer, other, session) in [(&alice, &bob, &alice_session), (&bob, &alice, &bob_session)] {
        let viewer_name = viewer.account.as_str();
        let other_name = other.account.as_str();
        let marker = format!("tag-{other_name}");
        let other_body = if other_name == bob.account.as_str() {
            "bob private body text"
        } else {
            "alice private body text"
        };

        let own_body = if viewer.account == alice.account {
            "alice private body text"
        } else {
            "bob private body text"
        };

        // get_session: detail, tags, live count and bodies are the viewer's own.
        //
        // Two calls, because Task 8 split this tool into two views and each has to be discriminated
        // separately: the scoped (default) view must *withhold* the account-private prose from everybody
        // including its owner, and the archive view must give it back to the owner and only the owner. A
        // test that read only the default view could not tell "withheld" from "broken", and one that read
        // only the archive could not see the withholding at all.
        let detail = tools
            .call(
                &viewer.bearer,
                session,
                "get_session",
                json!({"session_id": "t5:shared"}),
            )
            .await;
        let rendered = detail.to_string();
        assert!(
            !rendered.contains(&format!("tag-{viewer_name}")),
            "the scoped view exposed the viewer's own session tag, so `view` is decorative: {rendered}"
        );
        let archived = tools
            .call(
                &viewer.bearer,
                session,
                "get_session",
                json!({"session_id": "t5:shared", "view": "archive"}),
            )
            .await;
        let archived_rendered = archived.to_string();
        assert!(
            archived_rendered.contains(&format!("tag-{viewer_name}")),
            "{viewer_name}'s archive view omitted their own tag: {archived_rendered}"
        );
        assert!(
            !rendered.contains(&marker) && !archived_rendered.contains(&marker),
            "get_session leaked {other_name}'s session tag to {viewer_name}"
        );
        assert!(
            !rendered.contains(other_body),
            "get_session leaked {other_name}'s memory body to {viewer_name}"
        );
        // Exactly one live memory: two would mean the count absorbed the other account's row through
        // the shared handle, which is the count leak rather than a content leak.
        assert_eq!(
            detail["structuredContent"]["session"]["memory_count"].as_i64(),
            Some(1),
            "get_session's live count is not account-local for {viewer_name}: {detail}"
        );
        assert_eq!(
            archived["structuredContent"]["session"]["memory_count"].as_i64(),
            Some(1),
            "the archive count disagreed with the scoped count for {viewer_name}: {archived}"
        );

        // list_sessions: one row for the shared handle. The scoped page carries no prose at all; the
        // archive page carries the viewer's own and never the other account's.
        let listed = tools
            .call(&viewer.bearer, session, "list_sessions", json!({}))
            .await;
        let listed_rendered = listed.to_string();
        assert!(
            listed_rendered.contains("t5:shared"),
            "{viewer_name}'s own session list omitted their own handle"
        );
        assert!(
            !listed_rendered.contains(&format!("tag-{viewer_name}")),
            "the scoped session list exposed session prose to {viewer_name}: {listed_rendered}"
        );
        let archived_list = tools
            .call(
                &viewer.bearer,
                session,
                "list_sessions",
                json!({"view": "archive"}),
            )
            .await;
        let archived_rendered = archived_list.to_string();
        assert!(
            archived_rendered.contains(&format!("tag-{viewer_name}")),
            "the archive list omitted {viewer_name}'s own tag: {archived_rendered}"
        );
        assert!(
            !listed_rendered.contains(&marker) && !archived_rendered.contains(&marker),
            "list_sessions leaked {other_name}'s tag to {viewer_name}"
        );
        assert_eq!(
            listed["structuredContent"]["sessions"]
                .as_array()
                .map(Vec::len)
                .unwrap_or_default(),
            1,
            "list_sessions returned another account's session row for {viewer_name}: {listed}"
        );
        assert_eq!(
            listed["structuredContent"]["count"].as_i64(),
            Some(1),
            "list_sessions' count is not account-local for {viewer_name}"
        );
        assert_eq!(
            archived_list["structuredContent"]["count"].as_i64(),
            Some(1),
            "the archive list is not account-local for {viewer_name}: {archived_list}"
        );
        // A `tag` filter searches the withheld prose, so the scoped path refuses it rather than
        // answering with a wider page than the caller asked for.
        let tag_on_scoped = tools
            .call(
                &viewer.bearer,
                session,
                "list_sessions",
                json!({"tag": format!("tag-{viewer_name}")}),
            )
            .await;
        assert!(
            refused(&tag_on_scoped) || tag_on_scoped.to_string().contains("archive"),
            "a scoped list accepted a tag filter silently: {tag_on_scoped}"
        );
        let tag_on_archive = tools
            .call(
                &viewer.bearer,
                session,
                "list_sessions",
                json!({"view": "archive", "tag": format!("tag-{viewer_name}")}),
            )
            .await;
        assert!(
            tag_on_archive.to_string().contains("t5:shared"),
            "the archive view cannot filter by its own tag: {tag_on_archive}"
        );

        // retrieve / recall: no foreign body.
        let retrieved = tools
            .call(
                &viewer.bearer,
                session,
                "retrieve_memories",
                json!({"query": format!("{WARM_QUERY_MARK} private body text"), "limit": 10}),
            )
            .await;
        assert!(
            !refused(&retrieved),
            "the caller's own retrieval must succeed: {retrieved}"
        );
        assert!(
            retrieved.to_string().contains(own_body),
            "the privacy query must actually return the caller's own seeded body: {retrieved}"
        );
        assert!(
            !retrieved.to_string().contains(other_body),
            "retrieve_memories leaked {other_name}'s memory body to {viewer_name}"
        );
        let recalled = tools
            .call(
                &viewer.bearer,
                session,
                "recall",
                json!({"query": format!("{WARM_QUERY_MARK} private body text")}),
            )
            .await;
        assert!(
            !recalled.to_string().contains(other_body),
            "recall leaked {other_name}'s memory body to {viewer_name}"
        );
    }
}

/// Foreign and missing must be indistinguishable on **every** id-taking tool, not just the two the
/// earlier lanes covered. Each pair is driven twice — once with an id that exists and belongs to another
/// account, once with an id that exists nowhere — and the two responses must agree byte for byte.
#[tokio::test(flavor = "multi_thread")]
async fn every_id_taking_tool_answers_a_foreign_id_as_an_absent_one() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t6-probe-alice").await;
    let bob = tools.owner("t6-probe-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;

    let bob_session_id = tools
        .seed_session("t6-bob-sess", "t6-owned-handle", &bob)
        .await;
    let bob_reminder = tools
        .seed_reminder("t6-bob-rem", "bob reminder to probe", &bob, None)
        .await;
    let bob_fact = tools
        .seed_fact(
            "t6-bob-fact",
            "bob fact to probe",
            &bob,
            "personal",
            None,
            &[],
        )
        .await;
    assert!(!bob_session_id.is_empty() && !bob_reminder.is_empty() && !bob_fact.is_empty());

    // Positive control: bob can act on his own rows, so a refusal for alice is about ownership and not
    // about the id being malformed or the tool being broken.
    let bob_session = tools.handshake(&bob.bearer).await;
    let own = tools
        .call(
            &bob.bearer,
            &bob_session,
            "get_session",
            json!({"session_id": "t6-owned-handle"}),
        )
        .await;
    assert!(
        !refused(&own),
        "bob cannot read his own session, so the comparison below is meaningless: {}",
        text_of(&own)
    );

    let before_probes = tools.foreign_probe_state().await;
    tools
        .db
        .inner()
        .query("UPDATE $fact SET deleted = true;")
        .bind(("fact", record(&bob_fact)))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_ne!(
        tools.foreign_probe_state().await,
        before_probes,
        "the refused-write observer must detect a real soft-delete despite unchanged tags"
    );
    tools
        .db
        .inner()
        .query("UPDATE $fact SET deleted = false;")
        .bind(("fact", record(&bob_fact)))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(
        tools.foreign_probe_state().await,
        before_probes,
        "calibration restores the exact domain state before probes"
    );

    for (name, foreign_args, absent_args, label) in [
        (
            "get_session",
            json!({"session_id": "t6-owned-handle"}),
            json!({"session_id": "t6-no-such-handle"}),
            "session handle",
        ),
        (
            "finalize_session",
            json!({"session_id": "t6-owned-handle", "summary": "x"}),
            json!({"session_id": "t6-no-such-handle", "summary": "x"}),
            "session handle",
        ),
        (
            "delete_memory",
            json!({"id": &bob_fact}),
            json!({"id": "fact:t6-never-created"}),
            "fact id",
        ),
        (
            "update_memory",
            json!({"id": &bob_fact, "tags": ["tampered"]}),
            json!({"id": "fact:t6-never-created", "tags": ["tampered"]}),
            "fact id",
        ),
        (
            "cancel_reminder",
            json!({"id": &bob_reminder}),
            json!({"id": "reminder:t6-never-created"}),
            "reminder id",
        ),
    ] {
        let foreign = tools
            .call(&alice.bearer, &alice_session, name, foreign_args)
            .await;
        let absent = tools
            .call(&alice.bearer, &alice_session, name, absent_args)
            .await;
        assert!(
            refused(&foreign),
            "{name} reported success for another account's {label}: {foreign}"
        );
        assert_eq!(
            refused(&foreign),
            refused(&absent),
            "{name} differs in refusal shape between a foreign {label} and an absent one"
        );
        assert_eq!(
            text_of(&foreign),
            text_of(&absent),
            "{name} distinguishes a foreign {label} from an absent one: foreign={} absent={}",
            text_of(&foreign),
            text_of(&absent)
        );
    }

    // Authentication bookkeeping is outside this domain-state equality contract.
    assert_eq!(
        tools.foreign_probe_state().await,
        before_probes,
        "refused foreign/absent probes changed corpus, visibility, revision, relationship or journal state"
    );
    let bob_state = tools
        .rows("SELECT summary, tags FROM `session` WHERE meta::id(id) = 't6-bob-sess';")
        .await;
    assert!(
        bob_state[0]["summary"].is_null(),
        "alice's probe wrote into bob's session: {bob_state:?}"
    );
    let rem = tools
        .rows("SELECT status FROM reminder WHERE meta::id(id) = 't6-bob-rem';")
        .await;
    assert_eq!(
        rem[0]["status"].as_str(),
        Some("pending"),
        "alice's cancel moved bob's reminder: {rem:?}"
    );
    let fact = tools
        .rows("SELECT tags FROM fact WHERE meta::id(id) = 't6-bob-fact';")
        .await;
    assert!(
        fact[0]["tags"].as_array().is_none_or(|t| t.is_empty()),
        "alice's update wrote into bob's fact: {fact:?}"
    );
}

/// An unowned, migration-pending row is reachable by nobody.
///
/// The fail-closed half of the reminder surface: a row with no account must be unservable rather than
/// servable to whoever happens to ask. The request path cannot leave one any more (the scoped create
/// stamps the owner in its own statement), but the interim `do_set_reminder` and Task 14's backfill can,
/// so this is the property that makes an unowned reminder a durability concern instead of an
/// access-control one — and the regression the Task 10 journal carry has to preserve.
#[tokio::test(flavor = "multi_thread")]
async fn an_unowned_row_is_invisible_and_undeliverable_to_every_account() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t7-orphan-alice").await;
    let bob = tools.owner("t7-orphan-bob").await;
    tools
        .seed_fact(
            "t7-alice-own",
            "alice own retrieval control",
            &alice,
            "personal",
            None,
            &[],
        )
        .await;
    tools
        .seed_fact(
            "t7-bob-own",
            "bob own retrieval control",
            &bob,
            "personal",
            None,
            &[],
        )
        .await;
    let alice_session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;

    // Rows with no owner: the shape a create that never stamped could leave behind.
    tools
        .db
        .inner()
        .query(
            "CREATE type::record('fact', 'orphan-fact') SET content = 'orphan body text', \
             confidence = 0.5, embedding = [1.0, 0.0], tags = ['orphan-tag'], deleted = false;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    tools
        .db
        .inner()
        .query(
            "CREATE type::record('session', 'orphan-session') SET external_id = 'orphan:handle', \
             tags = ['orphan-session-tag'];",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    tools
        .db
        .inner()
        .query(
            "CREATE type::record('reminder', 'orphan-reminder') SET message = 'orphan reminder body', \
             schedule_kind = 'once', due_at = d'2026-01-01T00:00:00Z', \
             next_due_at = d'2026-01-01T00:00:00Z', status = 'pending', delivered_count = 0;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

    // Positive control: the rows really are there, so a below-empty result is a refusal and not an
    // empty store.
    assert_eq!(
        tools
            .rows("SELECT id FROM fact WHERE meta::id(id) = 'orphan-fact';")
            .await
            .len(),
        1,
        "the orphan fixture wrote nothing, so the assertions below prove nothing"
    );
    assert_eq!(
        tools
            .rows("SELECT id FROM reminder WHERE meta::id(id) = 'orphan-reminder';")
            .await
            .len(),
        1
    );
    assert_eq!(
        tools
            .count("SELECT count() FROM `session` WHERE meta::id(id) = 'orphan-session';")
            .await,
        1
    );

    for (who, session) in [(&alice, &alice_session), (&bob, &bob_session)] {
        for (name, args) in [
            ("get_session", json!({"session_id": "orphan:handle"})),
            (
                "retrieve_memories",
                json!({"query": format!("{WARM_QUERY_MARK} orphan body text"), "limit": 10}),
            ),
            (
                "recall",
                json!({"query": format!("{WARM_QUERY_MARK} orphan body text")}),
            ),
            ("list_sessions", json!({})),
            ("list_reminders", json!({})),
            ("check_reminders", json!({})),
        ] {
            let result = tools.call(&who.bearer, session, name, args).await;
            let rendered = result.to_string();
            if name == "retrieve_memories" {
                let own_body = if who.account == alice.account {
                    "alice own retrieval control"
                } else {
                    "bob own retrieval control"
                };
                assert!(
                    !refused(&result) && rendered.contains(own_body),
                    "orphan exclusion needs a returned own-body control: {rendered}"
                );
            }
            for marker in [
                "orphan body text",
                "orphan reminder body",
                "orphan-tag",
                "orphan-session-tag",
                "orphan:handle",
            ] {
                assert!(
                    !rendered.contains(marker),
                    "{name} surfaced the unowned marker {marker:?} to {}",
                    who.account
                );
            }
        }
        // Consumption has to fail closed too: an unowned reminder must not be deliverable by anyone, so
        // a row that lost its owner cannot turn into a cross-account delivery.
        assert_eq!(
            tools
                .count("SELECT delivered_count, status FROM reminder WHERE meta::id(id) = 'orphan-reminder' AND delivered_count > 0 GROUP ALL;")
                .await,
            0,
            "an unowned reminder was consumed by {}",
            who.account
        );
    }

    // The read layer, asserted directly. The response-level assertions above cannot see a dropped
    // account predicate on `list_due`: delivery is *also* blocked by the account-bound conditional
    // claim, so a mutation removing the read guard alone survives — a control run confirmed it. That is
    // defence in depth doing its job for outcomes, and it is also why the read must be pinned on its own:
    // a future change that surfaces due rows without consuming them (a preview, a count, a piggyback
    // with a different shape) would leak through the claim guard entirely.
    let repo = alexandria_storage::repos::ScopedReminderRepo::new(
        tools.db.inner(),
        tools.owner_for(&alice).await,
    );
    let due = repo
        .list_due(chrono::Utc::now())
        .await
        .expect("the scoped due read runs");
    let messages: Vec<&str> = due.iter().map(|row| row.message.as_str()).collect();
    assert!(
        !messages.contains(&"orphan reminder body"),
        "the scoped due read returned an unowned reminder: {messages:?}"
    );
    assert!(
        due.is_empty(),
        "alice's due read saw rows that are not hers: {messages:?}"
    );

    // And it is still there, unconsumed and unchanged.
    let still = tools
        .rows(
            "SELECT status, delivered_count FROM reminder WHERE meta::id(id) = 'orphan-reminder';",
        )
        .await;
    assert_eq!(still[0]["status"].as_str(), Some("pending"));
    assert_eq!(still[0]["delivered_count"].as_i64(), Some(0));
}

/// Counts non-overlapping occurrences of `needle` in `haystack`.
///
/// Written out because the scan subtracts the scoped spelling from the unscoped one, and that only works
/// if each occurrence is counted exactly once: `ScopedSessionRepo::new` contains `SessionRepo::new` once,
/// so `count(unscoped) - count(scoped)` is the number of *bare* constructions in a handler body.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// The seven private bodies in `src/server.rs` that take `Option<&RequestScope>`.
///
/// Four are the `*_body` request bodies — `store_response_body`, `store_body`, `import_body`,
/// `set_reminder_body` — whose scoped arm **is** the request path, so what offends there is a `None` in
/// the scope position. The other three resolve a store-wide population on their `None` arm and each
/// already has a scoped sibling a handler is meant to call instead: `retrieve_core` (a store-wide kNN
/// read; call `retrieve_scoped`), `due_reminders_summary_scoped` (every account's reminder text; call
/// `due_reminders_summary_for`) and `assign_to_cluster_and_update` (Task 6's P0-2 surface —
/// `ClusterRepo::list_with_counts()`; call `cluster_infos_for`). For those three the *name* is the
/// offence, whatever the arguments, because reaching the Option seam at all is reaching past admission.
///
/// `scope_argument` is the position of the `Option<&RequestScope>` parameter, and only matters for the
/// seams where an argument decides the verdict.
const SCOPE_SEAMS: [(&str, usize, bool); 7] = [
    ("store_response_body", 0, false),
    ("store_body", 0, false),
    ("import_body", 0, false),
    ("set_reminder_body", 0, false),
    ("retrieve_core", 2, true),
    ("due_reminders_summary_scoped", 0, true),
    ("assign_to_cluster_and_update", 0, true),
];

/// Everything a set of handler bodies reaches that it must not.
///
/// A function rather than an inline loop so the same rules can be run against a synthetic snippet and
/// shown to fire: `request_facing_modules_never_reach_an_interim_or_unscoped_surface` asserts production
/// is clean here and that a forged handler is not, which is the difference between a guard and a comment.
///
/// Three kinds of offender. The interim arms are named by needle (`*_interim` and the `do_*` bodies).
/// The `Option<&RequestScope>` seams are read as *calls* — see [`SCOPE_SEAMS`] and
/// [`scope_seam_reaches`]. And the unscoped repo constructors are counted, scoped spelling subtracted,
/// because `SessionRepo`'s own documentation claims this scan fails if a handler starts building it
/// again — a claim that was false of direct construction until this counter existed.
fn interim_or_unscoped_reaches(handlers: &[(String, String)], interim: &[&str]) -> Vec<String> {
    let unscoped_repos = [
        "SessionRepo::new",
        "MemoryRepo::new",
        "ClusterRepo::new",
        "ReminderRepo::new",
        "HeatRepo::new",
        "EdgeRepo::new",
    ];
    let mut reached = scope_seam_reaches(handlers);
    for (name, body) in handlers {
        for needle in interim {
            if body.contains(needle) {
                reached.push(format!("{name}: {needle}"));
            }
        }
        for unscoped in unscoped_repos {
            let bare = count_occurrences(body, unscoped)
                - count_occurrences(body, &format!("Scoped{unscoped}"));
            if bare > 0 {
                reached.push(format!(
                    "{name}: {bare} x {unscoped} (not reached through a Scoped constructor)"
                ));
            }
        }
    }
    reached
}

/// `//` line comments and `///` doc comments removed, so a needle only matches code. Block comments are
/// not stripped: this tree writes none around the surfaces the scan names, and a naive scanner would
/// have to become a lexer to do it safely.
fn strip_line_comments(text: &str) -> String {
    text.lines()
        .map(|line| match line.find("//") {
            Some(cut) => &line[..cut],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `Option<&RequestScope>` seam a handler body reaches, and how.
///
/// This replaced the `"_body(None"` needle, which was a claim about whitespace rather than about the
/// code: `src/server.rs` already calls `import_body(` with `Some(scope),` on its own line, so the same
/// break around a `None` — or `let nobody = None; …_body(nobody, …)` — satisfied every needle in this
/// file while still answering as nobody. The calls are therefore *parsed*: [`callable_text`] blanks
/// comments and string contents, whitespace is gone by construction, [`calls_to`] finds each balanced
/// argument list, and [`none_bindings`] resolves a local bound to `None`. What this still cannot see is
/// a `None` laundered through a function call or a struct field, and the name rule on the three
/// store-wide seams is what closes the dangerous half of that gap.
fn scope_seam_reaches(handlers: &[(String, String)]) -> Vec<String> {
    let mut reached = Vec::new();
    for (name, body) in handlers {
        let code = callable_text(body);
        for (seam, scope_argument, name_is_offence) in SCOPE_SEAMS {
            let bound = if name_is_offence {
                Vec::new()
            } else {
                none_bindings(&code)
            };
            for arguments in calls_to(&code, seam) {
                if name_is_offence {
                    reached.push(format!(
                        "{name}: reaches the store-wide seam {seam} (the scoped sibling is the request path)"
                    ));
                    continue;
                }
                let Some(argument) = arguments.get(scope_argument) else {
                    continue;
                };
                if resolves_to_none(argument, &bound) {
                    reached.push(format!(
                        "{name}: passes None to {seam} (argument {scope_argument} is {argument})"
                    ));
                }
            }
        }
    }
    reached
}

/// Whether an argument denotes the interim arm: `None`, `Option::None`, a `None::<T>` turbofish, or a
/// local the body bound to `None`.
///
/// The turbofish spelling matters for the same reason the whitespace case did: `store_body(None::<&
/// RequestScope>, params)` is the interim arm written exactly as a caller would write it when the
/// inference needs help, and reading it as "not None" would be a false negative in the one seam list
/// whose whole job is having no false negatives. `NoneLike` must still not match, which is what the
/// next-character test buys.
fn resolves_to_none(argument: &str, bound: &[String]) -> bool {
    argument == "None"
        || argument.ends_with("::None")
        || starts_with_none_token(argument)
        || bound.iter().any(|name| name == argument)
}

/// Whether `text` begins with the token `None` and not with a longer identifier that starts with it.
fn starts_with_none_token(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("None") else {
        return false;
    };
    // `None::<&RequestScope>` and `None?`-style suffixes are still the `None` token; `NoneLike` is not.
    !rest.chars().next().is_some_and(is_ident_char)
}

/// The names a handler body binds to a literal `None` — `let nobody = None`, with or without a type
/// ascription. A `let` bound to something *else* is deliberately not followed: `None` laundered through
/// a call is invisible to any textual scan, and saying so beats a scan that looks deeper than it reads.
fn none_bindings(code: &str) -> Vec<String> {
    let bytes = code.as_bytes();
    let mut bound = Vec::new();
    let mut search = 0usize;
    while let Some(found) = code[search..].find("let") {
        let after_let = search + found + "let".len();
        if found + search > 0 && is_ident_byte(bytes[search + found - 1]) {
            search = after_let;
            continue; // `delete`, `is_empty`: not the `let` keyword
        }
        let token: String = code[after_let..]
            .chars()
            .take_while(|ch| is_ident_char(*ch))
            .collect();
        search = after_let + token.len();
        if token.is_empty() {
            continue;
        }
        // Squeezing whitespace out of the text has eaten the space in `let mut x`, so register both
        // spellings rather than guess which one an argument uses.
        let mut spellings = vec![token.clone()];
        if let Some(rest) = token.strip_prefix("mut").filter(|rest| !rest.is_empty()) {
            spellings.push(rest.to_string());
        }
        let Some(declaration) = code[search..].split(';').next() else {
            continue;
        };
        let Some(equals) = declaration
            .find('=')
            .filter(|index| !declaration[*index + 1..].starts_with('='))
        else {
            continue;
        };
        let value = &declaration[equals + 1..];
        let longer_name = value
            .strip_prefix("None")
            .is_some_and(|rest| rest.chars().next().is_some_and(is_ident_char));
        if value.starts_with("None") && !longer_name {
            bound.extend(spellings);
        }
    }
    bound
}

/// The handler body as the seam scan reads it: `//` and `/* */` comments and the inside of string
/// literals removed, whitespace squeezed out.
///
/// Prose must not be able to reach a seam — this file's own comments name these bodies — and a
/// whitespace-free text is what makes the scan independent of how rustfmt broke the call. Char literals
/// and lifetimes are read as ordinary code, exactly as [`calls_to`] does: mistaking `&'a str` for the
/// start of a literal would swallow real code, which is the worse of the two failures.
///
/// The cost of that choice is stated rather than glossed, because it is not one-directional. An
/// unbalanced bracket inside what this reader treats as code — a `'{'` in a char literal, say — makes
/// [`calls_to`] fail to close the argument list, and it then **skips the call** rather than guessing at
/// it. A skipped call is a false *negative*: a `None` reaching a seam through such an argument would go
/// unreported. That is reachable only by a bracket char literal inside a seam call's own argument list,
/// which `src/server.rs` does not contain; if one is ever added, the fix is to skip `'…'` pairs here
/// rather than to widen a needle list.
fn callable_text(body: &str) -> String {
    let mut code = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    let mut line_comment = false;
    let mut block_depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        let peeked = chars.peek().copied();
        if line_comment {
            line_comment = ch != '\n';
            continue;
        }
        if block_depth > 0 {
            match (ch, peeked) {
                ('/', Some('*')) => {
                    block_depth += 1;
                    chars.next();
                }
                ('*', Some('/')) => {
                    block_depth -= 1;
                    chars.next();
                }
                _ => {}
            }
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match (ch, peeked) {
            ('/', Some('/')) => {
                line_comment = true;
                chars.next();
            }
            ('/', Some('*')) => {
                block_depth = 1;
                chars.next();
            }
            ('"', _) => in_string = true,
            (other, _) => {
                if !other.is_whitespace() {
                    code.push(other);
                }
            }
        }
    }
    code
}

/// The argument list of every call to `seam` in `code`, which must be [`callable_text`] output.
///
/// `seam` counts as a name only where it is a whole token (`self.store_body(`, `Self::store_body(` —
/// not `do_store_body(`), and the arguments are the comma-separated terms of the balanced parentheses
/// that follow it. An unbalanced tail is not a call this scan can read, and is skipped rather than
/// guessed.
fn calls_to(code: &str, seam: &str) -> Vec<Vec<String>> {
    let bytes = code.as_bytes();
    let mut calls = Vec::new();
    let mut search = 0usize;
    while let Some(found) = code[search..].find(seam) {
        let start = search + found;
        let open = start + seam.len();
        search = open;
        if start > 0 && is_ident_byte(bytes[start - 1]) {
            continue;
        }
        if bytes.get(open) != Some(&b'(') {
            continue;
        }
        let mut depth = 0usize;
        let mut current = String::new();
        let mut arguments: Vec<String> = Vec::new();
        let mut balanced = false;
        for ch in code[open..].chars() {
            match ch {
                '(' | '[' | '{' => {
                    depth += 1;
                    if depth == 1 {
                        continue;
                    }
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        arguments.push(current.clone());
                        balanced = true;
                        break;
                    }
                }
                ',' if depth == 1 => {
                    arguments.push(current.clone());
                    current.clear();
                    continue;
                }
                _ => {}
            }
            current.push(ch);
        }
        if balanced {
            calls.push(arguments);
        }
    }
    calls
}

/// The parameter list of `async fn <name>(`, up to its matching close paren, from `src/server.rs`.
fn async_fn_signature(text: &str, name: &str) -> Option<String> {
    let marker = format!("async fn {name}(");
    let start = text.find(&marker)? + marker.len();
    let bytes = text.as_bytes();
    let mut depth = 1usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..i].to_string());
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// A signature's parameters, split on top-level commas only.
///
/// Generics, closures and nested tuples all carry commas at depth > 0, and a naive split turns one
/// parameter into two — which would misreport the position the seam list is checking.
fn split_parameters(signature: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for ch in signature.chars() {
        match ch {
            '<' | '(' | '[' => {
                depth += 1;
                current.push(ch);
            }
            '>' | ')' | ']' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(current.trim().to_string());
                current = String::new();
            }
            other => current.push(other),
        }
    }
    let last = current.trim().to_string();
    if !last.is_empty() {
        parts.push(last);
    }
    parts
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The unscoped and interim surfaces are unreachable from any request handler.
///
/// D1 defers the debug UI to Task 9 and D3 defers cross-partition work, so `MaintenanceRepo`,
/// `stats::gather` and the `*_interim` / unscoped `do_*` bodies stay unscoped on purpose. "Unreachable"
/// is a claim about source structure and it has to be checked rather than assumed: nothing in the type
/// system stops a later `#[tool]` handler from calling `MaintenanceRepo::new(self.db.inner())` the way
/// the session tools used to, or `do_import_document_interim` the way the store path used to call
/// `do_store_memory`.
///
/// Two scans, because the earlier version of this test scanned whole files and claimed a pin it did not
/// have. The first walks every **request-facing module** — `src/` minus the debug tree, which D1 defers —
/// for the unscoped maintenance/stats constructors. The second extracts every `#[tool]` handler *body*
/// from `src/server.rs` and asserts none of them reaches an interim or unscoped surface, which is what
/// "no `#[tool]` path may call `None`" means when anyone but the reviewer has to keep it true.
///
/// A failure means a request path grew a dependency on an unscoped surface. The fix is to scope it or
/// route it through the caller's own context, never to widen a list below.
#[test]
fn request_facing_modules_never_reach_an_interim_or_unscoped_surface() {
    // --- scan 1: no unscoped maintenance or stats surface anywhere in the crate --------------
    //
    // Task 8 left this scan skipping `/debug/` with a note that the debug population was "Task 9's, and
    // it is disclosed as untrusted". Task 9 closed that disclosure: every debug data route now reads
    // through a `Scoped*Repo` under an authenticated owner session, so the carve-out is removed rather
    // than renewed — and a future route that reaches for a corpus constructor (`MemoryRepo::new`,
    // `SessionRepo::new`, `ClusterRepo::new`, `EdgeRepo::new`, `HeatRepo::new`, `ReminderRepo::new`) or
    // for the old per-row audit reads fails here instead of widening the one hole the scan used to paper
    // over. Be exact about what is *not* covered in the debug half, because the exempt slice below is
    // `unscoped[..3]` and not a per-needle judgement: the three maintenance/stats needles
    // (`MaintenanceRepo::new`, `MaintenanceRepo::with_audit`, `stats::gather`) are waived for
    // `src/debug/`, since the admin-only aggregate page legitimately builds `MaintenanceRepo`. That
    // waiver is why a debug route that reached for `stats::gather` again would **not** fail this scan —
    // the store-wide count is kept off the dashboard by `dashboard.rs` reading scoped counts, and by its
    // own suite, not by a needle here. Do not describe this scan as guarding the debug half against
    // `stats::gather` when it does not, and do not widen the waiver to a corpus constructor.
    let unscoped = [
        "MaintenanceRepo::new",
        "MaintenanceRepo::with_audit",
        "stats::gather",
        "list_maintenance_logs",
        "count_maintenance_logs",
        // The constructors the scoped reads replaced. `AdminRepo` and `MaintenanceRepo` are absent from
        // this list on purpose where they are *permitted*: the admin surface is capability-gated, not
        // row-filtered, and maintenance's rollup is the admin-only aggregate page. What is forbidden is
        // a **corpus** read that carries no owner predicate.
        "MemoryRepo::new",
        "SessionRepo::new",
        "ClusterRepo::new",
        "EdgeRepo::new",
        "HeatRepo::new",
        "ReminderRepo::new",
    ];
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut request_facing = 0;
    let mut debug_modules = 0;
    for entry in walk_rust_files(&src) {
        // `migrate.rs` is the offline corpus pass, not a request-facing module: it must re-embed *every*
        // row including the soft-deleted ones (`all_ids_and_content` carries them on purpose), so an
        // owner predicate would be a correctness bug there rather than a leak. It is named, not skipped
        // by a directory rule, so a new request path cannot borrow the exemption.
        if entry.ends_with("/src/migrate.rs") {
            continue;
        }
        let is_debug = entry.contains("/debug/");
        request_facing += 1;
        if is_debug {
            debug_modules += 1;
        }
        let full = std::fs::read_to_string(&entry).expect("readable module");
        // A file that is test-only by construction (`#![cfg(test)]` at the top, like the debug fixture)
        // is not a request path at all, and its whole purpose is to reach whichever repo a page test
        // needs in order to seed.
        // `debug/test_support.rs` is declared `#[cfg(test)] mod test_support;` by its parent, so the
        // attribute is on the `mod` item and not at the top of the file — the fixture is nonetheless
        // compiled out of the product, and its job is to seed through whichever repo a page test needs.
        if full.starts_with("#![cfg(test)]") || entry.ends_with("/debug/test_support.rs") {
            continue;
        }
        // Comments are prose about the code, not a call: the migrated dashboard says out loud that
        // `stats::gather` was the leak, and a scan that reads that as an offender trains the next
        // author to delete the sentence rather than to keep the guarantee.
        let code_only = strip_line_comments(&full);
        // Only the product half. A `#[cfg(test)] mod tests` at the bottom of a debug file seeds through
        // whichever repo the fixture needs, and an admin-only aggregate page's test has to write
        // unattributed maintenance rows to have something to aggregate — that is fixture work, not a
        // request path, and counting it would train everybody to add an exception to the scan.
        let text = match code_only.find("#[cfg(test)]") {
            Some(cut) => &code_only[..cut],
            None => &code_only,
        };
        // The exempt needles are the *pre-Task 9* audit surface plus the store-wide count that rode in
        // with it. This tree still reads `maintenance_log`, but only through the admin-only aggregate
        // whose capability is a type-level gate (`run_rollup(admin, …)` cannot be called by an owner
        // session at all), so the needles that named the old page are waived for the debug half and kept,
        // unchanged, for every request-facing module outside it. The slice is positional — `unscoped[..3]`
        // is `MaintenanceRepo::new`, `MaintenanceRepo::with_audit` **and `stats::gather`** — so the waiver
        // says nothing about whether `stats::gather` belongs in a page; it only says this scan will not
        // notice. Reorder `unscoped` above and this line silently changes meaning, which is why the three
        // names are spelled out here rather than counted.
        let needles: &[&str] = if is_debug {
            &unscoped[3..]
        } else {
            &unscoped[..]
        };
        for needle in needles {
            // `count_occurrences`, not `contains`: `ClusterRepo::new` is a substring of
            // `ScopedClusterRepo::new`, and the scoped spelling is precisely what the scan demands. The
            // same subtraction `interim_or_unscoped_reaches` applies for the same reason — without it
            // every migrated module would read as an offender and the fix would be to widen the skip,
            // which is how a scan like this gets switched off.
            let bare = count_occurrences(text, needle)
                - count_occurrences(text, &format!("Scoped{}", needle));
            if bare > 0 {
                offenders.push(format!("{entry}: {bare} x {needle}"));
            }
        }
    }
    assert!(
        debug_modules >= 10,
        "the scan saw {debug_modules} modules under `src/debug/`; the tree moved and the debug half of \
         this guarantee is now untested"
    );
    assert!(
        request_facing >= 8,
        "the request-facing scan found {request_facing} modules; the tree moved and the scan is now \
         vacuous"
    );
    assert!(
        offenders.is_empty(),
        "a request-facing module touches an unscoped maintenance/stats/corpus surface: {offenders:?}"
    );

    // --- scan 2: no `#[tool]` handler body reaches an interim or unscoped write surface -------
    let server = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/server.rs")
        .with_extension("rs");
    let text = std::fs::read_to_string(&server).expect("readable src/server.rs");
    let handlers = tool_handler_bodies(&text);
    assert_eq!(
        handlers.len(),
        17,
        "the scan saw {} `#[tool]` handlers; the closed seventeen-tool surface moved, and this test's \
         guarantee moved with it",
        handlers.len()
    );
    let interim_do = [
        "_interim",
        "do_store_memory(",
        "do_import_document(",
        "do_get_session(",
        "do_list_sessions(",
        "do_finalize_session(",
        "do_set_reminder(",
        "do_check_reminders(",
        "do_list_reminders(",
        "do_cancel_reminder(",
        "do_recall(",
        "do_retrieve_memories(",
    ];
    // The seven `Option<&RequestScope>` bodies are *not* needles: `store_response_body`, `store_body`,
    // `import_body`, `set_reminder_body`, `retrieve_core`, `due_reminders_summary_scoped` and
    // `assign_to_cluster_and_update` are read as calls by [`scope_seam_reaches`], because the needle
    // this replaced (`"_body(None"`) was a claim about whitespace — rustfmt already writes
    // `import_body(` with `Some(scope),` on its own line, and a handler could hand over
    // `let nobody = None;` — so it missed three of the seven seams outright and any formatting of the
    // other four.
    let interim: Vec<&str> = interim_do
        .iter()
        .copied()
        .chain(["MaintenanceRepo", "stats::gather"])
        .collect();
    let reached = interim_or_unscoped_reaches(&handlers, &interim);
    assert!(
        reached.is_empty(),
        "a `#[tool]` handler calls an interim or unscoped surface — the arm that writes rows no \
         ownership predicate covers: {reached:?}"
    );

    // The same function, run over three synthetic bodies, is asserted *non*-empty below: a scan whose
    // needles had all been renamed away would pass production and fail there, which is the only form of
    // evidence that the guard can fail at all.

    // Positive controls, so neither scan can pass by matching nothing.
    let debug_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/debug");
    let debug_hits = walk_rust_files(&debug_dir)
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter(|text| unscoped.iter().any(|needle| text.contains(needle)))
        .count();
    assert!(
        debug_hits > 0,
        "the maintenance/stats needles match nowhere in the tree, so scan 1 proves nothing"
    );
    let interim_uses = interim_do
        .iter()
        .filter(|needle| text.contains(**needle))
        .count();
    assert!(
        interim_uses > 0,
        "the interim needles match nothing in src/server.rs at all, so scan 2 proves nothing: the \
         interim surface has been renamed or deleted and this test has not been retargeted"
    );
    // And the seam list is live in both directions. Every name in `SCOPE_SEAMS` is still a defined body
    // in src/server.rs, and the number of `Option<&RequestScope>` parameters in the file still equals
    // the list's length — which is the claim this file used to carry as prose ("the four private
    // bodies") and could not check. A seam renamed away fails the first half; a fifth arm added fails
    // the second. Either way the fix is to retarget `SCOPE_SEAMS`, never to delete a case.
    // Each recorded argument position is checked against the signature it claims to read. Without this,
    // reordering one parameter — which Task 8 does to exactly these bodies, by adding scope/project
    // fields — would silently point the scan at the wrong argument, and a `None` in the scope position
    // would go unreported with the test still green.
    for (seam, argument, _store_wide) in SCOPE_SEAMS {
        let signature = async_fn_signature(&text, seam).unwrap_or_else(|| {
            panic!(
                "SCOPE_SEAMS names {seam}, which src/server.rs no longer defines: the seam scan is \
                 reading a surface that has moved"
            )
        });
        let parameters = split_parameters(&signature);
        // `&self` is not a call argument, so the call's argument 0 is the signature's parameter 1.
        let position = 1 + argument;
        let actual = parameters.get(position).unwrap_or_else(|| {
            panic!(
                "{seam} carries {} parameters; SCOPE_SEAMS records the scope at call argument {argument} \
                 (parameter {position}), which is past the end of the signature `{signature}`",
                parameters.len()
            )
        });
        assert!(
            actual.contains("Option<&RequestScope>"),
            "SCOPE_SEAMS records {seam}'s scope at call argument {argument}, but parameter {position} \
             of `{signature}` is `{actual}`: the scan would be reading a different argument than the one \
             it reports"
        );
    }
    assert_eq!(
        text.matches("Option<&RequestScope>").count(),
        SCOPE_SEAMS.len(),
        "src/server.rs carries {} `Option<&RequestScope>` parameters while `SCOPE_SEAMS` lists {}; \
         the scan below can only be as complete as the list it reads, and the interim arm of any seam \
         left off it is an unscoped surface a handler could reach in silence",
        text.matches("Option<&RequestScope>").count(),
        SCOPE_SEAMS.len()
    );
    // The extractor really does extract bodies, and really does see the needles inside them when they
    // are there — asserted against a snippet rather than against production source.
    let synthetic = r#"
        #[tool_router]
        impl X {
            #[tool(description = "a")]
            async fn a(&self) -> String { self.do_store_memory_interim(p).await }
            #[tool(description = "b")]
            async fn b(&self) -> String { "ok".to_string() }
        }
    "#;
    let forged = r#"
        #[tool_router]
        impl X {
            #[tool(description = "a")]
            async fn a(&self) -> String { self.store_body(None, p).await }
            #[tool(description = "b")]
            async fn b(&self) -> String {
                let repo = SessionRepo::new(self.db.inner());
                repo.get_memories("h").await
            }
            #[tool(description = "c")]
            async fn c(&self) -> String {
                ScopedSessionRepo::new(self.db.inner(), scope.owner().clone()).touch("h").await
            }
            // d: a store-wide seam, reached the way rustfmt writes a long call — no literal `None`
            // anywhere, and an admitted scope at that. Reaching it at all is the offender.
            #[tool(description = "d")]
            async fn d(&self) -> String {
                self.retrieve_core(
                    params,
                    options,
                    Some(scope),
                )
                .await
            }
            // e: a second store-wide seam, with the interim arm handed over through a local.
            #[tool(description = "e")]
            async fn e(&self) -> String {
                let nobody = None;
                self.due_reminders_summary_scoped(nobody, 5).await
            }
            // f: a `*_body` seam whose `None` is an intermediate variable — the shape a literal
            // needle could not see, because rustfmt writes the argument on its own line.
            #[tool(description = "f")]
            async fn f(&self) -> String {
                let nobody = None;
                self.set_reminder_body(nobody, params).await
            }
            // g: the clean case for the argument rule — a `*_body` reached with the request's own
            // scope. Must not be reported, or the scan is just banning the wrapper.
            #[tool(description = "g")]
            async fn g(&self) -> String {
                self.import_body(Some(scope), params).await
            }
            // h: the interim arm spelled with a turbofish, which is exactly how a real call site writes
            // it once the argument type has to be pinned (`store_body(None::<&RequestScope>, p)`). The
            // scan reads arguments as calls, so a literal needle on `_body(None` would have seen this
            // one and missed the whitespace-folded cases; it must be reported.
            #[tool(description = "h")]
            async fn h(&self) -> String {
                self.store_body(None::<&RequestScope>, params).await
            }
            // i: the negative control for the same rule. `NoneLike` *starts* with the token `None`, and
            // a prefix match would report a handler that passes an admitted scope under a different
            // name. Must not be reported, or the scan bans a spelling rather than a reach.
            #[tool(description = "i")]
            async fn i(&self) -> String {
                self.import_body(NoneLike, params).await
            }
        }
    "#;
    let forged = tool_handler_bodies(forged);
    assert_eq!(
        forged.len(),
        9,
        "the extractor lost one of the nine synthetic handlers ({} found)",
        forged.len()
    );
    let caught = interim_or_unscoped_reaches(&forged, &interim);
    let mut offenders: Vec<&str> = caught.iter().map(String::as_str).collect();
    offenders.sort();
    // The names carry the extracted signature token (`a(&self)`), because that is what
    // `tool_handler_bodies` reads from a one-line `async fn a(&self)` — the offender identity is the
    // handler, not a prettified label.
    assert_eq!(
        offenders,
        [
            "a(&self): passes None to store_body (argument 0 is None)",
            "b(&self): 1 x SessionRepo::new (not reached through a Scoped constructor)",
            "d(&self): reaches the store-wide seam retrieve_core (the scoped sibling is the request path)",
            "e(&self): reaches the store-wide seam due_reminders_summary_scoped (the scoped sibling is the request path)",
            "f(&self): passes None to set_reminder_body (argument 0 is nobody)",
            "h(&self): passes None to store_body (argument 0 is None::<&RequestScope>)",
        ],
        "the scan caught {caught:?}; handlers c, g and i construct and call through the scoped \
         surfaces (or through an admitted scope under a name that merely starts with `None`) and must \
         not appear at all, and every other seam must appear exactly once"
    );
    // The needle this scan replaced, evaluated over the same bodies, kept as an assertion so the
    // comparison stays measurable if the scan is ever "simplified" back toward a literal. The two
    // directions it gets wrong are different kinds of wrong, which is why both halves are pinned:
    //
    //  * it **misses** the folded and indirect shapes — `d`/`e` reach store-wide seams with no `None` at
    //    all, and `f` hands the interim arm over through a local — so it under-reports;
    //  * it **over-reports** a handler that passes an admitted scope under a name merely beginning with
    //    the token (`i`: `import_body(NoneLike, …)`), which trains a caller to distrust the gate.
    //
    // The seam scan reports a, d, e, f and h, and does not report i.
    let mut legacy: Vec<String> = forged
        .iter()
        .filter(|(_, body)| body.contains("_body(None"))
        .map(|(name, _)| name.clone())
        .collect();
    legacy.sort();
    assert_eq!(
        legacy,
        vec!["a(&self)", "h(&self)", "i(&self)"],
        "the literal `name(None` needle is expected to see exactly the three handlers whose argument \
         text begins with `None` against the open paren — including the false positive on `NoneLike`, \
         which is the half of the comparison that shows the seam scan is not simply more permissive"
    );
    assert!(
        !caught.iter().any(|offence| offence.starts_with("i(&self)")),
        "the seam scan reported a handler that passes an admitted scope under a `None`-prefixed name"
    );

    let extracted = tool_handler_bodies(synthetic);
    assert_eq!(
        extracted.len(),
        2,
        "the handler extractor found {} bodies in a two-handler snippet",
        extracted.len()
    );
    assert!(
        extracted[0].1.contains("do_store_memory_interim"),
        "the extractor is not returning the handler body it was given: {:?}",
        extracted[0].1
    );
    assert!(
        !extracted[1].1.contains("do_store_memory_interim"),
        "the extractor is returning text outside the handler it was asked for"
    );
}

/// Every `#[tool]` handler body in `text`, with its name.
///
/// Attribute, signature, body: the body is brace-matched from the handler's opening `{`, ignoring
/// line comments and string literals so a brace inside a message cannot end the region early. Deliberately
/// textual rather than `syn`-based — this is a boundary check inside a test, not a parser — and the
/// synthetic control above is what keeps the extraction honest when the file changes shape.
fn tool_handler_bodies(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (at, _) in text.match_indices("#[tool(") {
        let region = &text[at..];
        let after_attribute = match region[("#[tool(").len()..].find(')') {
            Some(close) => &region[("#[tool(").len() + close + 1..],
            None => continue,
        };
        let name = after_attribute
            .split_whitespace()
            .skip_while(|token| *token != "fn")
            .nth(1)
            .unwrap_or_default()
            .trim_end_matches('(')
            .to_string();
        let brace = match after_attribute.find('{') {
            Some(brace) => brace,
            None => continue,
        };
        out.push((name, balanced_body(&after_attribute[brace..])));
    }
    out
}

/// The text from the first `{` to its matching `}`, skipping `//` comments and string literals.
fn balanced_body(from_brace: &str) -> String {
    let bytes = from_brace.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut line_comment = false;
    let mut index = 0;
    while index < bytes.len() {
        let ch = bytes[index] as char;
        if line_comment {
            if ch == '\n' {
                line_comment = false;
            }
        } else if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return from_brace[..=index].to_string();
                    }
                }
                '"' => in_string = true,
                '/' if bytes.get(index + 1) == Some(&b'/') => line_comment = true,
                _ => {}
            }
        }
        index += 1;
    }
    from_brace.to_string()
}

/// Every `.rs` file under `root`, as absolute paths.
fn walk_rust_files(root: &std::path::Path) -> impl Iterator<Item = String> {
    let mut pending = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path.display().to_string());
            }
        }
    }
    found.into_iter()
}

/// A reminder row with no owner is served to nobody — and consumed by nobody.
///
/// The request path no longer *produces* this state: `reminder_create` runs one
/// `ScopedReminderRepo::create`, which stamps `account` in the statement that creates the row, so there is
/// no second statement to interrupt and no stamp to leave behind. What the test still pins is the
/// consequence, which outlives that mechanism — Task 14's backfill, a hand-repaired row, or the interim
/// unscoped `do_set_reminder` can each leave a reminder with no owner, and such a row must be unreachable
/// rather than servable to whoever asks next. That is what makes an unowned reminder a durability wart
/// rather than an access-control hole, and it is the property the Task 10 journal carry has to preserve.
///
/// The row is produced by `UNSET account` rather than by an interruption: aborting between two statements
/// has no supported seam, and building a production fault injector for a test is not a trade to make. The
/// stored state is byte-for-byte the one the old mechanism left behind, and the assertions are about who
/// can reach it, not about how it got there.
#[tokio::test(flavor = "multi_thread")]
async fn a_reminder_left_unowned_is_served_to_nobody() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-stamp-alice").await;
    let bob = tools.owner("t8-stamp-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;

    // Alice creates a reminder, then its owner is removed — the state the interrupted write used to leave.
    let created = tools
        .call(
            &alice.bearer,
            &alice_session,
            "set_reminder",
            json!({"message": "stamp-interrupted reminder", "due_at": "2030-01-01T00:00:00Z"}),
        )
        .await;
    assert!(
        !refused(&created),
        "the fixture reminder was not created: {}",
        text_of(&created)
    );
    let id = created["structuredContent"]["id"]
        .as_str()
        .expect("a created reminder reports its id")
        .to_string();
    tools
        .db
        .inner()
        .query("UPDATE type::record('reminder', $key) UNSET account;")
        .bind(("key", record_key_of(&id)))
        .await
        .unwrap()
        .check()
        .unwrap();

    // Positive control: the row exists and is due, so the refusals below are not an empty store.
    let exists = tools
        .rows("SELECT id, status FROM reminder WHERE meta::id(id) = $key;")
        .await;
    let _ = exists;
    let present = tools
        .db
        .inner()
        .query("SELECT id, account, status, next_due_at FROM reminder WHERE meta::id(id) = $key;")
        .bind(("key", record_key_of(&id)))
        .await
        .unwrap()
        .check()
        .unwrap()
        .take::<Vec<Json>>(0)
        .unwrap();
    assert_eq!(
        present.len(),
        1,
        "unowning the row destroyed it, not just its owner"
    );
    assert!(
        present[0]["account"].is_null(),
        "UNSET account did not clear the owner: {:?}",
        present[0]
    );
    // Make it due now as well, so the delivery path below has a row it *could* take if the ownership
    // guard were missing. Left in 2030 the reminder would be skipped for a reason that has nothing to
    // do with ownership, and the test would pass without proving anything.
    tools
        .db
        .inner()
        .query("UPDATE type::record('reminder', $key) SET next_due_at = time::now();")
        .bind(("key", record_key_of(&id)))
        .await
        .unwrap()
        .check()
        .unwrap();

    // Neither its creator nor anybody else can list or consume it.
    for (who, session) in [(&alice, &alice_session), (&bob, &bob_session)] {
        let listed = tools
            .call(
                &who.bearer,
                session,
                "list_reminders",
                json!({"status": "all"}),
            )
            .await;
        assert!(
            !listed.to_string().contains("stamp-interrupted reminder"),
            "{} listed a reminder with no owner",
            who.account
        );
        let checked = tools
            .call(&who.bearer, session, "check_reminders", json!({}))
            .await;
        assert!(
            !checked.to_string().contains("stamp-interrupted reminder"),
            "{} was served a reminder with no owner",
            who.account
        );
    }

    // And it stays unconsumed: the claim is account-bound, so no bearer can advance it.
    let still = tools
        .db
        .inner()
        .query("SELECT status, delivered_count FROM reminder WHERE meta::id(id) = $key;")
        .bind(("key", record_key_of(&id)))
        .await
        .unwrap()
        .check()
        .unwrap()
        .take::<Vec<Json>>(0)
        .unwrap();
    assert_eq!(
        still[0]["status"].as_str(),
        Some("pending"),
        "an unowned reminder was consumed by somebody"
    );
    assert_eq!(still[0]["delivered_count"].as_i64(), Some(0));
}

/// The owner-less row is invisible to the scoped repository layer too, not only to the tool responses.
#[tokio::test(flavor = "multi_thread")]
async fn the_scoped_reminder_read_layer_skips_unowned_rows() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-layer-alice").await;
    tools
        .db
        .inner()
        .query(
            "CREATE type::record('reminder', 'layer-orphan') SET message = 'layer orphan body', \
             schedule_kind = 'once', due_at = d'2026-01-01T00:00:00Z', \
             next_due_at = d'2026-01-01T00:00:00Z', status = 'pending', delivered_count = 0;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    // Positive control: the row is there and due now.
    assert_eq!(
        tools
            .count("SELECT count() FROM reminder WHERE meta::id(id) = 'layer-orphan' AND next_due_at <= time::now();")
            .await,
        1,
        "the fixture row is not present-and-due"
    );

    let repo = alexandria_storage::repos::ScopedReminderRepo::new(
        tools.db.inner(),
        tools.owner_for(&alice).await,
    );
    assert!(
        repo.list_due(chrono::Utc::now())
            .await
            .expect("due read runs")
            .is_empty(),
        "the scoped due read returned an unowned row"
    );
    assert!(
        repo.get("reminder:layer-orphan")
            .await
            .expect("get runs")
            .is_none(),
        "the scoped get returned an unowned row"
    );
    // The claim refuses it as well, so even a caller that guessed the id cannot consume it.
    assert!(
        !repo
            .record_delivery(
                "reminder:layer-orphan",
                None,
                Some(
                    chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                        .unwrap()
                        .with_timezone(&chrono::Utc)
                ),
                Utc::now(),
            )
            .await
            .expect("claim runs"),
        "an unowned reminder was consumable through the scoped claim"
    );
}

/// The key half of a `table:key` handle, for `meta::id(...)` comparisons.
fn record_key_of(handle: &str) -> String {
    handle
        .split_once(':')
        .map(|(_, key)| key.trim_matches('`').to_string())
        .unwrap_or_else(|| handle.to_string())
}
