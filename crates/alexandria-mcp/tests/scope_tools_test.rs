//! Task 8 — the scoped tool surface, driven through the real transport.
//!
//! Every case here goes through the production gate and the production [`AlexandriaServer`], so the
//! principal, the lease and the request context are the transport's own. No case hands a handler a
//! context it could not lawfully obtain, and there is no test-only authority constructor anywhere in the
//! path: owners come from `bootstrap_owner`, a hosted bearer from `mint_agent_token` plus
//! `authenticate_agent_token`, a trusted-local identity from `local_principal` under this store's claimed
//! startup capability, and every context from `AccessRepo::authorize`/`select`.
//!
//! Four things are asserted about each rule, because "the code looks right" is not evidence:
//!
//! * **The partition is stamped, read back from the stored row** — a response that reports where a memory
//!   went and a `fact` row that says something else is the failure mode of this whole feature.
//! * **A refusal is refused at the right moment and pays for nothing.** The `embed` counter is the only
//!   way to see "before embedding", and a write count is the only way to see "and writes nothing".
//! * **A foreign selector and an absent one answer identically**, and *both* are compared against a case
//!   where the same id is lawful, so the equality is not two empty answers agreeing by accident.
//! * **Nothing is ever enumerable across workspaces** — the same alias, held by two accounts, has to be
//!   invisible from each side in the listing as well as in the resolution.
//!
//! The proposal cases carry the rule that an agent's judgement is not an edit: the fact's stored bytes are
//! compared before and after, and the row that comes back is read from the engine rather than from the
//! response.

mod fixtures;

use alexandria_mcp::auth::http::admit;
use alexandria_mcp::auth::{
    Admission, AuthEvents, AuthMiddleware, AuthService, McpSessionRegistry,
};
use alexandria_mcp::server::{AlexandriaServer, HeatSettings};
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::access::{AccessRepo, SystemContext};
use alexandria_storage::models::OperationId;
use alexandria_storage::repos::{AuthRepo, IdentityRepo};
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

/// A provider whose calls are counted, so "refused before embedding" is observable.
#[derive(Clone)]
struct CountingEmbedding {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl EmbeddingProvider for CountingEmbedding {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![1.0f32, 0.0f32]).collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "task8-counting"
    }
}

/// A provider that assigns distinct but high-similarity vectors to named fixture texts.
///
/// This exists because `CountingEmbedding` returns the same vector for every input: useful for counting
/// refusals, but numerically blind to a running-average regression. Scripted vectors can distinguish a
/// centroid averaged over two members from one replaced by the newest member.
#[derive(Clone)]
struct ScriptedEmbedding {
    calls: Arc<AtomicUsize>,
    vectors: std::collections::HashMap<&'static str, Vec<f32>>,
}

#[async_trait::async_trait]
impl EmbeddingProvider for ScriptedEmbedding {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts
            .iter()
            .map(|text| {
                self.vectors
                    .get(*text)
                    .cloned()
                    .unwrap_or_else(|| vec![1.0f32, 0.0f32])
            })
            .collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "task8-scripted"
    }
}

/// One enrolled owner: the ids enrollment produced and the bearer a hosted request presents.
struct Owner {
    account: String,
    workspace: String,
    bearer: String,
}

struct Tools {
    db: Arc<Database>,
    router: Router,
    embed_calls: Arc<AtomicUsize>,
    system: SystemContext,
}

fn record(text: &str) -> surrealdb::types::RecordId {
    surrealdb::types::RecordId::parse_simple(text).expect("a fixture record id")
}

impl Tools {
    async fn hosted() -> Self {
        Self::built(None).await
    }

    /// `stdio` is carried exactly as `main.rs` carries it on the stdio arm.
    async fn built(stdio: Option<alexandria_mcp::auth::StdioIdentity>) -> Self {
        let embed_calls = Arc::new(AtomicUsize::new(0));
        Self::built_with(
            stdio,
            Arc::new(CountingEmbedding {
                calls: Arc::clone(&embed_calls),
            }),
            embed_calls,
        )
        .await
    }

    /// A fixture whose provider answers exact strings with distinct vectors.
    async fn hosted_with_scripted_embeddings(vectors: Vec<(&'static str, Vec<f32>)>) -> Self {
        let embed_calls = Arc::new(AtomicUsize::new(0));
        Self::built_with(
            None,
            Arc::new(ScriptedEmbedding {
                calls: Arc::clone(&embed_calls),
                vectors: vectors.into_iter().collect(),
            }),
            embed_calls,
        )
        .await
    }

    async fn built_with(
        stdio: Option<alexandria_mcp::auth::StdioIdentity>,
        embedding: Arc<dyn EmbeddingProvider>,
        embed_calls: Arc<AtomicUsize>,
    ) -> Self {
        let mut db = Database::connect_embedded().await.unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let system = db.claim_system_authority().unwrap();
        let db = Arc::new(db);
        let mut server =
            AlexandriaServer::new(Arc::clone(&db), embedding, 0.75, HeatSettings::default());
        if let Some(identity) = stdio {
            server = server.with_stdio_identity(identity);
        }
        let sessions = Arc::new(LocalSessionManager::default());
        let events = AuthEvents::closing_sessions_in(Arc::clone(&sessions));
        let service = Arc::new(AuthService::admitting(Arc::clone(&db), events.clone()));
        let gate = AuthMiddleware::admitting(
            service,
            Admission::bound_to(events.clone()),
            McpSessionRegistry::over(&events),
        );
        let mcp: StreamableHttpService<AlexandriaServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(server.clone()),
                Arc::clone(&sessions),
                StreamableHttpServerConfig::default()
                    .with_sse_keep_alive(Some(Duration::from_millis(50))),
            );
        let router = Router::new()
            .nest_service(MCP_PATH, mcp)
            .layer(axum::middleware::from_fn_with_state(gate, admit));
        Self {
            db,
            router,
            embed_calls,
            system,
        }
    }

    /// Enrolls an owner and mints the agent token the transport will authenticate.
    async fn owner(&self, login: &str) -> Owner {
        let ids = IdentityRepo::new(self.db.inner())
            .bootstrap_owner(
                login,
                &format!("$argon2id$fixture-prepared-hash-{login}-padding"),
                &OperationId::new(format!("task8-{login}")),
            )
            .await
            .unwrap();
        let access = AccessRepo::new(&self.db);
        let local = access
            .local_principal(&self.system, &ids.account)
            .await
            .unwrap();
        let context = access.authorize(&local).await.unwrap();
        let token = AuthRepo::new(self.db.inner())
            .mint_agent_token(&context, "task8-agent", Utc::now())
            .await
            .unwrap();
        let bearer = token.secret.render().expose().to_string();
        AuthRepo::new(self.db.inner())
            .authenticate_agent_token(&bearer, Utc::now())
            .await
            .unwrap();
        let mut response = self
            .db
            .inner()
            .query("SELECT id FROM workspace WHERE owner = $owner LIMIT 1;")
            .bind(("owner", ids.account.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Json> = response.take(0).unwrap();
        Owner {
            account: alexandria_storage::record_id_to_string(ids.account.as_record_id()),
            workspace: rows[0]["id"].as_str().unwrap().to_string(),
            bearer,
        }
    }

    /// Runs the real MCP handshake and returns the session id with the server's `initialize` result.
    ///
    /// The result is returned rather than dropped because `initialize` is where the usage
    /// `instructions` are advertised: they are the half of the public surface an agent reads *before*
    /// choosing a tool, so a claim about them has to be checked where they are actually published.
    async fn initialize(&self, bearer: &str) -> (String, Json) {
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
                        "clientInfo": {"name": "task8", "version": "0"}
                    }
                })
                .to_string(),
            ))
            .unwrap();
        request
            .headers_mut()
            .insert("host", HeaderValue::from_static("127.0.0.1:3000"));
        let response = self.router.clone().oneshot(request).await.expect("served");
        assert_eq!(response.status(), StatusCode::OK, "handshake admitted");
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .expect("a session id");
        let bytes = read_body(response.into_body()).await;
        let info = fixtures::frames_of(&bytes)
            .into_iter()
            .find_map(|frame| frame.get("result").cloned())
            .unwrap_or_else(|| {
                panic!(
                    "initialize returned no result frame: {}",
                    String::from_utf8_lossy(&bytes)
                )
            });
        let notice = with_bearer(
            with_session(
                router_post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
                &session,
            ),
            bearer,
        );
        let _ = self.router.clone().oneshot(notice).await.expect("served");
        (session, info)
    }

    async fn handshake(&self, bearer: &str) -> String {
        self.initialize(bearer).await.0
    }

    fn request(
        &self,
        bearer: &str,
        session: &str,
        name: &str,
        arguments: Json,
    ) -> axum::http::Request<Body> {
        with_protocol(with_bearer(
            with_session(router_post(tool_body(name, arguments)), session),
            bearer,
        ))
    }

    /// Calls a tool and returns its result payload.
    async fn call(&self, bearer: &str, session: &str, name: &str, arguments: Json) -> Json {
        let response = self
            .router
            .clone()
            .oneshot(self.request(bearer, session, name, arguments))
            .await
            .expect("served");
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

    /// Lists the tools the server advertises, from `tools/list`.
    async fn listed(&self, bearer: &str, session: &str) -> Vec<Json> {
        let request = with_protocol(with_bearer(
            with_session(
                router_post(json!({"jsonrpc": "2.0", "id": 99, "method": "tools/list"})),
                session,
            ),
            bearer,
        ));
        let response = self.router.clone().oneshot(request).await.expect("served");
        let bytes = read_body(response.into_body()).await;
        for frame in fixtures::frames_of(&bytes) {
            if let Some(tools) = frame.pointer("/result/tools") {
                return tools.as_array().cloned().unwrap_or_default();
            }
        }
        panic!(
            "tools/list returned no tool array: {}",
            String::from_utf8_lossy(&bytes)
        );
    }

    async fn rows(&self, sql: &str, binds: Json) -> Vec<Json> {
        let mut query = self.db.inner().query(sql.to_string());
        // A bind that reads like a record id (`table:key`) is bound as one; binding a record as a string
        // silently matches nothing on this engine, which would turn a passing read into an empty one.
        for (key, value) in binds.as_object().expect("an object of binds") {
            query = match value.as_str() {
                Some(text) if text.contains(':') => query.bind((key.clone(), record(text))),
                _ => query.bind((key.clone(), value.clone())),
            };
        }
        let mut response = query.await.unwrap().check().unwrap();
        response.take(0).unwrap()
    }

    /// The stored scope columns of one fact, read from the engine rather than from a response.
    async fn stored_fact(&self, id: &str) -> Json {
        let rows = self
            .rows(
                "SELECT scope, project, account, workspace, content, deleted, semantic_revision \
                 FROM type::record('fact', $key) WHERE id = $id;",
                json!({"key": id.split_once(':').map(|(_, k)| k).unwrap_or(id), "id": id}),
            )
            .await;
        rows.into_iter()
            .next()
            .unwrap_or_else(|| panic!("no stored fact for {id}"))
    }

    /// The stored centroid of one cluster, read from the engine rather than from a response.
    async fn stored_cluster_centroid(&self, id: &str) -> Vec<f32> {
        let rows = self
            .rows(
                "SELECT centroid FROM type::record('cluster', $key) WHERE id = $id;",
                json!({"key": id.split_once(':').map(|(_, key)| key).unwrap_or(id), "id": id}),
            )
            .await;
        let first = rows
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("no stored cluster for {id}"));
        first["centroid"]
            .as_array()
            .expect("cluster centroid is an array")
            .iter()
            .map(|value| value.as_f64().expect("centroid component is numeric") as f32)
            .collect()
    }

    async fn count(&self, sql: &str) -> i64 {
        let rows = self.rows(sql, json!({})).await;
        let first = rows.first().cloned().unwrap_or(json!({}));
        first["count"]
            .as_i64()
            .or_else(|| first["c"].as_i64())
            .unwrap_or(0)
    }
}

fn with_protocol(mut request: axum::http::Request<Body>) -> axum::http::Request<Body> {
    request.headers_mut().insert(
        "mcp-protocol-version",
        HeaderValue::from_static("2025-06-18"),
    );
    request
        .headers_mut()
        .insert("host", HeaderValue::from_static("127.0.0.1:3000"));
    request
}

fn tool_body(name: &str, arguments: Json) -> Json {
    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
}

/// The opaque key half of a `project:<key>` handle. Used only to build *expected* values in assertions;
/// the lane never builds a handle from a key.
fn short(handle: &Json) -> String {
    handle["id"]
        .as_str()
        .unwrap_or_default()
        .split_once(':')
        .map(|(_, key)| key.to_string())
        .unwrap_or_default()
}

fn content(result: &Json) -> Json {
    result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Json::Null)
}

fn refused(result: &Json) -> bool {
    result["isError"].as_bool().unwrap_or(false)
        || content(result).get("status").and_then(Json::as_str) == Some("error")
}

fn text_of(result: &Json) -> String {
    result["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Json::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// Establishes one project for `owner` and returns its descriptor.
async fn establish(tools: &Tools, owner: &Owner, session: &str, remote: Json) -> Json {
    let answer = tools
        .call(
            &owner.bearer,
            session,
            "resolve_project",
            json!({"remote": remote}),
        )
        .await;
    assert!(
        !refused(&answer),
        "resolve_project refused a lawful descriptor: {answer}"
    );
    content(&answer)["project"].clone()
}

fn github(path: &str) -> Json {
    json!({"host": "github.com", "path": path, "transport": "ssh"})
}

// ---------------------------------------------------------------------------------------
// Stores and imports: the target must be lawful, and the partition must be stamped
// ---------------------------------------------------------------------------------------

/// A hosted store with an explicit project target lands in that partition — read back from the row.
#[tokio::test(flavor = "multi_thread")]
async fn a_hosted_store_with_an_explicit_target_lands_in_that_partition() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-store-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let project = establish(&tools, &alice, &session, github("anten/alexandria")).await;

    // Positive control first: Personal still works, so a project refusal below cannot be "nothing works".
    let personal = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "a personal decision", "scope": "personal"}),
        )
        .await;
    assert!(
        !refused(&personal),
        "a lawful personal store refused: {personal}"
    );
    assert_eq!(content(&personal)["scope"].as_str(), Some("personal"));
    let stored = tools
        .stored_fact(content(&personal)["id"].as_str().unwrap())
        .await;
    assert_eq!(stored["scope"].as_str(), Some("personal"));
    assert_eq!(
        stored["project"],
        Json::Null,
        "a personal row carries a project"
    );
    assert_eq!(
        stored["account"].as_str(),
        Some(alice.account.as_str()),
        "the row was stamped with somebody else's account"
    );

    let answer = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({
                "content": "a project-specific architecture note",
                "scope": "project",
                "project": project["id"],
            }),
        )
        .await;
    assert!(
        !refused(&answer),
        "a lawful project store refused: {answer}"
    );
    assert_eq!(
        content(&answer)["scope"].as_str(),
        Some(project["id"].as_str().unwrap()),
        "the response reported a different project than the request named"
    );
    let stored = tools
        .stored_fact(content(&answer)["id"].as_str().unwrap())
        .await;
    assert_eq!(stored["scope"].as_str(), Some("project"));
    assert_eq!(
        stored["project"].as_str(),
        Some(project["id"].as_str().unwrap()),
        "the fact row was not stamped with the resolved project"
    );
    assert_eq!(stored["account"].as_str(), Some(alice.account.as_str()));
    assert_eq!(
        stored["workspace"].as_str(),
        Some(alice.workspace.as_str()),
        "the row escaped the caller's workspace"
    );
}

/// An import is the same rule, per row: every chunk and the `raw` document carry the target.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_stamps_every_chunk_with_the_resolved_target() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-import-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let project = establish(&tools, &alice, &session, github("anten/imported")).await;

    let answer = tools
        .call(
            &alice.bearer,
            &session,
            "import_document",
            json!({
                "content": "# Section one\n\nReference material worth keeping for later.\n\n# Section two\n\nMore reference material.",
                "mode": "chunk",
                "scope": "project",
                "project": project["id"],
            }),
        )
        .await;
    assert!(!refused(&answer), "a lawful import refused: {answer}");
    let body = content(&answer);
    let ids = body["ids"].as_array().expect("imported ids");
    assert!(!ids.is_empty(), "the import reported no rows: {answer}");
    for id in ids {
        let stored = tools.stored_fact(id.as_str().unwrap()).await;
        assert_eq!(
            stored["scope"].as_str(),
            Some("project"),
            "an imported chunk escaped the target partition"
        );
        assert_eq!(
            stored["project"].as_str(),
            Some(project["id"].as_str().unwrap())
        );
        assert_eq!(stored["account"].as_str(), Some(alice.account.as_str()));
    }
    let raws = tools
        .rows(
            "SELECT account, workspace FROM raw WHERE account = $acc;",
            json!({"acc": alice.account}),
        )
        .await;
    assert_eq!(
        raws.len(),
        1,
        "the `raw` document row is missing or duplicated: {raws:?}"
    );
}

/// Omitted scope with no context is refused **before embedding** and writes nothing at all.
#[tokio::test(flavor = "multi_thread")]
async fn an_omitted_scope_with_no_context_refuses_before_embedding_and_writes_nothing() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-refuse-alice").await;
    let session = tools.handshake(&alice.bearer).await;

    for (name, arguments) in [
        ("store_memory", json!({"content": "an untargeted fact"})),
        (
            "import_document",
            json!({"content": "# H\n\nuntargeted reference material"}),
        ),
        (
            "store_memory",
            json!({"content": "a session is not a target", "session_id": "pi:not-a-target"}),
        ),
    ] {
        tools.embed_calls.store(0, Ordering::SeqCst);
        let answer = tools.call(&alice.bearer, &session, name, arguments).await;
        assert!(
            refused(&answer),
            "{name} accepted an untargeted hosted write"
        );
        assert!(
            text_of(&answer).contains("scope required"),
            "{name} refused without naming the missing thing: {answer}"
        );
        assert_eq!(
            tools.embed_calls.load(Ordering::SeqCst),
            0,
            "{name} reached the embedding provider before refusing"
        );
    }
    assert_eq!(
        tools
            .count("SELECT count() AS c FROM fact GROUP ALL;")
            .await,
        0
    );
    assert_eq!(
        tools
            .count("SELECT count() AS c FROM `session` GROUP ALL;")
            .await,
        0,
        "a refused store still created a session"
    );
    assert_eq!(
        tools.count("SELECT count() AS c FROM raw GROUP ALL;").await,
        0
    );
    assert_eq!(
        tools
            .count("SELECT count() AS c FROM heat_state GROUP ALL;")
            .await,
        0
    );
    assert_eq!(
        tools
            .count("SELECT count() AS c FROM provenance GROUP ALL;")
            .await,
        0
    );
}

/// There is no fallback to Personal, and a stale context never overrides an explicit scope.
#[tokio::test(flavor = "multi_thread")]
async fn a_personal_scope_beats_a_stale_context_project_and_nothing_falls_back() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-scope-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let project = establish(&tools, &alice, &session, github("anten/context-one")).await;

    let answer = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({
                "content": "a genuinely personal preference, sent inside a project",
                "scope": "personal",
                "context_project": project["id"],
            }),
        )
        .await;
    assert!(!refused(&answer), "{answer}");
    assert_eq!(content(&answer)["scope"].as_str(), Some("personal"));
    let stored = tools
        .stored_fact(content(&answer)["id"].as_str().unwrap())
        .await;
    assert_eq!(stored["scope"].as_str(), Some("personal"));
    assert_eq!(
        stored["project"],
        Json::Null,
        "the context won over the explicit scope"
    );

    // And a hosted request cannot get Personal by sending nothing (asserted above) nor by sending an
    // empty string, which is the same absence wearing a value.
    for arguments in [
        json!({"content": "x", "scope": ""}),
        json!({"content": "x", "scope": "  "}),
        json!({"content": "x", "context_project": ""}),
        json!({"content": "x", "scope": "shared"}),
    ] {
        let answer = tools
            .call(&alice.bearer, &session, "store_memory", arguments.clone())
            .await;
        assert!(
            refused(&answer),
            "an untargeted or unknown scope was accepted: {arguments} -> {answer}"
        );
    }
    assert_eq!(
        tools
            .count("SELECT count() AS c FROM fact WHERE content = 'x' GROUP ALL;")
            .await,
        0,
        "a refused store wrote a row anyway"
    );
}

/// An explicit project overrides the ambient context — the request that names a target means that one.
#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_project_overrides_a_stale_context_project() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-override-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let stale = establish(&tools, &alice, &session, github("anten/stale")).await;
    let current = establish(&tools, &alice, &session, github("anten/current")).await;

    let answer = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({
                "content": "filed against the project this call named",
                "scope": "project",
                "project": current["id"],
                "context_project": stale["id"],
            }),
        )
        .await;
    assert!(!refused(&answer), "{answer}");
    let stored = tools
        .stored_fact(content(&answer)["id"].as_str().unwrap())
        .await;
    assert_eq!(
        stored["project"].as_str(),
        Some(current["id"].as_str().unwrap()),
        "the ambient context outranked the explicit target"
    );
}

// The positive half of D4 rule 2 — a configured trusted-local caller may still default to Personal —
// is not reachable over this transport: the HTTP gate always attaches an admitted hosted principal, and
// a hosted principal wins over the configured stdio identity, so a request here can never become
// trusted-local. That is correct, and it is why the local arm is asserted at the seam the handler calls,
// in `server::get_info_tests::a_trusted_local_store_is_retrievable_by_its_own_reader` and
// `a_local_store_without_a_target_still_lands_in_personal_and_is_retrievable` below.

// ---------------------------------------------------------------------------------------
// Selectors: foreign, absent and unlawful
// ---------------------------------------------------------------------------------------

/// A foreign project id and an absent one refuse identically — and both differ from the case where the
/// same id is the caller's own, which is what stops "refused twice" from being the whole evidence.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_project_selector_is_indistinguishable_from_an_absent_one() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-foreign-alice").await;
    let bob = tools.owner("t8-foreign-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;
    let bobs = establish(&tools, &bob, &bob_session, github("anten/bobs-repo")).await;

    // Bob can store into his own project: the id is real, and the refusal below is about the caller.
    let mine = tools
        .call(
            &bob.bearer,
            &bob_session,
            "store_memory",
            json!({"content": "bob's own note", "scope": "project", "project": bobs["id"]}),
        )
        .await;
    assert!(
        !refused(&mine),
        "the owner could not use their own project: {mine}"
    );

    let foreign = tools
        .call(
            &alice.bearer,
            &alice_session,
            "store_memory",
            json!({"content": "reaching for another account project", "scope": "project", "project": bobs["id"]}),
        )
        .await;
    let absent = tools
        .call(
            &alice.bearer,
            &alice_session,
            "store_memory",
            json!({"content": "alice reaching for nothing", "scope": "project", "project": "project:never-minted"}),
        )
        .await;
    assert!(
        refused(&foreign) && refused(&absent),
        "{foreign} / {absent}"
    );
    assert_eq!(
        text_of(&foreign),
        text_of(&absent),
        "a foreign project and an absent one answered differently, which makes the tool an oracle"
    );
    assert!(
        !text_of(&foreign).contains("bob")
            && !text_of(&foreign).contains(
                &bobs["id"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(6)
                    .collect::<String>()
            ),
        "the refusal named the other account or its id: {foreign}"
    );
    assert_eq!(
        tools
            .count("SELECT count() AS c FROM fact WHERE content = 'reaching for another account project' GROUP ALL;")
            .await,
        0,
        "a refused cross-workspace store wrote a row"
    );
}

/// Resolution is workspace-qualified, so the same canonical alias is two projects, not one shared one.
#[tokio::test(flavor = "multi_thread")]
async fn one_canonical_alias_is_two_projects_in_two_workspaces() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-alias-alice").await;
    let bob = tools.owner("t8-alias-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;

    let a = establish(&tools, &alice, &alice_session, github("anten/shared")).await;
    let b = establish(&tools, &bob, &bob_session, github("anten/shared")).await;
    assert_ne!(
        a["id"], b["id"],
        "one alias collapsed two workspaces into one project"
    );
    // Each resolves the same *alias* text to their own id.
    for (owner, session, expected) in [(&alice, &alice_session, &a), (&bob, &bob_session, &b)] {
        let answer = tools
            .call(
                &owner.bearer,
                session,
                "resolve_project",
                json!({"project": "github.com/anten/shared"}),
            )
            .await;
        assert!(!refused(&answer), "{answer}");
        assert_eq!(&content(&answer)["project"], expected);
        assert_eq!(
            content(&answer)["established"].as_bool(),
            Some(false),
            "a replay reported itself as a creation: {answer}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// Retrieval and recall scopes
// ---------------------------------------------------------------------------------------

/// The four read scopes answer from exactly the partitions they name.
#[tokio::test(flavor = "multi_thread")]
async fn retrieval_scopes_answer_from_the_partition_they_name() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-read-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let project = establish(&tools, &alice, &session, github("anten/readme")).await;
    let other = establish(&tools, &alice, &session, github("anten/elsewhere")).await;

    let mut seeded: Vec<String> = Vec::new();
    for (_, content_text, scope, target) in [
        ("r-personal", "gateway rotation policy", "personal", None),
        (
            "r-project",
            "gateway rotation policy in the project",
            "project",
            Some(&project),
        ),
        (
            "r-other",
            "gateway rotation policy in the other project",
            "project",
            Some(&other),
        ),
    ] {
        let arguments = match target {
            None => json!({"content": content_text, "scope": scope}),
            Some(target) => {
                json!({"content": content_text, "scope": scope, "project": target["id"]})
            }
        };
        let answer = tools
            .call(&alice.bearer, &session, "store_memory", arguments)
            .await;
        assert!(!refused(&answer), "seed store refused: {answer}");
        seeded.push(content_text.to_string());
    }
    // Rows are compared by content, not by id: `store_memory` mints its own record id, so an assertion
    // naming one would be an assertion about the fixture.
    let contents_of = |body: &Json| -> Vec<String> {
        body["results"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|row| row["content"].as_str().map(str::to_string))
            .collect()
    };
    let mut seeded_sorted = seeded.clone();
    seeded_sorted.sort();

    // auto with no project: personal only — the project rows stay out.
    let auto = tools
        .call(
            &alice.bearer,
            &session,
            "retrieve_memories",
            json!({"query": "gateway rotation policy", "limit": 10}),
        )
        .await;
    let hits = contents_of(&content(&auto));
    assert_eq!(
        hits,
        vec!["gateway rotation policy".to_string()],
        "an auto read without a project reached a project row: {hits:?}"
    );
    assert_eq!(content(&auto)["scope"].as_str(), Some("personal"));

    // auto with a project: personal plus that project, and never the other one.
    let auto_project = tools
        .call(
            &alice.bearer,
            &session,
            "retrieve_memories",
            json!({"query": "gateway rotation policy", "limit": 10, "project": project["id"]}),
        )
        .await;
    let hits = contents_of(&content(&auto_project));
    assert!(
        hits.contains(&"gateway rotation policy".to_string())
            && hits.contains(&"gateway rotation policy in the project".to_string()),
        "an auto read with a project did not cover personal plus that project: {hits:?}"
    );
    assert!(
        !hits.contains(&"gateway rotation policy in the other project".to_string()),
        "an auto read with a project reached a project the caller did not name: {hits:?}"
    );
    assert_eq!(
        content(&auto_project)["scope"].as_str(),
        Some("personal+project")
    );

    // personal: only the personal row.
    let personal = tools
        .call(
            &alice.bearer,
            &session,
            "retrieve_memories",
            json!({"query": "gateway rotation policy", "scope": "personal", "limit": 10}),
        )
        .await;
    assert_eq!(
        contents_of(&content(&personal)),
        vec!["gateway rotation policy".to_string()],
        "an explicit personal read widened beyond personal"
    );
    assert_eq!(content(&personal)["scope"].as_str(), Some("personal"));

    // project: exactly that project.
    let scoped = tools
        .call(
            &alice.bearer,
            &session,
            "retrieve_memories",
            json!({"query": "gateway rotation policy", "scope": "project", "project": project["id"], "limit": 10}),
        )
        .await;
    assert_eq!(
        contents_of(&content(&scoped)),
        vec!["gateway rotation policy in the project".to_string()],
        "a project read leaked another partition"
    );
    assert_eq!(content(&scoped)["scope"].as_str(), Some("project"));

    // assigned: every owned partition, and the response never claims a partition it did not read.
    let assigned = tools
        .call(
            &alice.bearer,
            &session,
            "retrieve_memories",
            json!({"query": "gateway rotation policy", "scope": "assigned", "limit": 10}),
        )
        .await;
    let mut hits = contents_of(&content(&assigned));
    hits.sort();
    assert_eq!(
        hits, seeded_sorted,
        "an assigned lookup did not span exactly the caller's own assigned rows"
    );
    assert_eq!(content(&assigned)["scope"].as_str(), Some("assigned"));

    // recall answers under the same selection.
    let recalled = tools
        .call(
            &alice.bearer,
            &session,
            "recall",
            json!({"query": "gateway rotation policy", "scope": "project", "project": project["id"]}),
        )
        .await;
    // What Task 8 governs for recall is *whose* clusters and members are reachable: this caller's own
    // workspace and only the partitions named by `scope`. Cluster structure/re-clustering remains Task 12,
    // but membership narrowing is not: a project selection must not surface another partition's member.
    let recalled_members = recalled["structuredContent"]["clusters"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .flat_map(|cluster| {
            cluster["representative_memories"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|member| member["content"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    assert_eq!(
        recalled_members,
        vec!["gateway rotation policy in the project".to_string()],
        "a project-scoped recall surfaced members from outside that project: {recalled}"
    );
    let bobs = tools.owner("t8-recall-bob").await;
    let bob_session = tools.handshake(&bobs.bearer).await;
    let crossed = tools
        .call(
            &bobs.bearer,
            &bob_session,
            "recall",
            json!({"query": "gateway rotation policy"}),
        )
        .await;
    assert!(
        !crossed.to_string().contains("gateway rotation policy"),
        "another account's cluster surfaced in their recall: {crossed}"
    );
    assert!(
        !crossed.to_string().contains(&alice.workspace),
        "a recall response named another account's workspace: {crossed}"
    );

    // A selector that is not the caller's is refused on the read path too, identically to an absent one.
    let bob = tools.owner("t8-read-bob").await;
    let bob_session = tools.handshake(&bob.bearer).await;
    let foreign = tools
        .call(
            &bob.bearer,
            &bob_session,
            "retrieve_memories",
            json!({"query": "gateway rotation policy", "scope": "project", "project": project["id"]}),
        )
        .await;
    let absent = tools
        .call(
            &bob.bearer,
            &bob_session,
            "retrieve_memories",
            json!({"query": "gateway rotation policy", "scope": "project", "project": "project:nope"}),
        )
        .await;
    assert!(
        refused(&foreign) && refused(&absent),
        "{foreign} / {absent}"
    );
    assert_eq!(text_of(&foreign), text_of(&absent));
}

fn ids_of(body: &Json) -> Vec<String> {
    body["results"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A session id intersects the selected scope; it never widens it, and never implies archive access.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_id_intersects_the_selected_scope_and_never_widens_it() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-session-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let project = establish(&tools, &alice, &session, github("anten/mixed")).await;

    let mut seeded_ids: Vec<String> = Vec::new();
    for (_, scope) in [("m-personal", None), ("m-project", Some(&project))] {
        let arguments = match scope {
            None => {
                json!({"content": "mixed session marker", "session_id": "pi:mixed", "scope": "personal"})
            }
            Some(scope) => json!({
                "content": "mixed session marker",
                "session_id": "pi:mixed",
                "scope": "project",
                "project": scope["id"],
            }),
        };
        let answer = tools
            .call(&alice.bearer, &session, "store_memory", arguments)
            .await;
        assert!(!refused(&answer), "seed refused: {answer}");
        seeded_ids.push(content(&answer)["id"].as_str().unwrap().to_string());
    }
    let [personal_id, project_id] = [seeded_ids[0].clone(), seeded_ids[1].clone()];

    // The same session, read under a Personal scope: only the personal row is in scope, even though the
    // session holds both. This is the intersection, and it is what "a session id is not a widening"
    // means in a response.
    let personal = tools
        .call(
            &alice.bearer,
            &session,
            "retrieve_memories",
            json!({"query": "mixed session marker", "session_id": "pi:mixed", "scope": "personal", "limit": 10}),
        )
        .await;
    assert_eq!(
        ids_of(&content(&personal)),
        vec![personal_id.clone()],
        "a session-scoped personal read returned a project row"
    );

    // The project scope sees its own row and not the personal one.
    let project_read = tools
        .call(
            &alice.bearer,
            &session,
            "retrieve_memories",
            json!({"query": "mixed session marker", "session_id": "pi:mixed", "scope": "project", "project": project["id"], "limit": 10}),
        )
        .await;
    assert_eq!(
        ids_of(&content(&project_read)),
        vec![project_id.clone()],
        "a session-scoped project read did not narrow to the project"
    );

    // Reading the session itself is scoped by default: the prose stays withheld until asked for.
    let detail = tools
        .call(
            &alice.bearer,
            &session,
            "get_session",
            json!({"session_id": "pi:mixed"}),
        )
        .await;
    assert_eq!(
        content(&detail)["view"].as_str(),
        Some("scoped"),
        "get_session's default view moved: {detail}"
    );
    assert_eq!(
        content(&detail)["session"]["memory_count"].as_i64(),
        Some(1),
        "a scoped session detail counted beyond the caller's default partition: {detail}"
    );
    assert!(
        content(&detail)["session"].get("summary").is_none(),
        "the scoped view published the account-private summary key: {detail}"
    );
}

/// Scoped withholds the session prose; archive is the deliberate whole-session read.
#[tokio::test(flavor = "multi_thread")]
async fn a_scoped_session_view_withholds_prose_and_archive_exposes_it() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-view-alice").await;
    let bob = tools.owner("t8-view-bob").await;
    let session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;
    let stored = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "a note in a finalized session", "session_id": "pi:viewed", "scope": "personal"}),
        )
        .await;
    assert!(!refused(&stored), "{stored}");
    let finalized = tools
        .call(
            &alice.bearer,
            &session,
            "finalize_session",
            json!({"session_id": "pi:viewed", "summary": "what we decided about the gateway", "tags": ["gateway-decision"]}),
        )
        .await;
    assert!(!refused(&finalized), "{finalized}");

    let scoped = tools
        .call(
            &alice.bearer,
            &session,
            "get_session",
            json!({"session_id": "pi:viewed"}),
        )
        .await;
    assert!(
        !scoped.to_string().contains("gateway-decision"),
        "the scoped view leaked the session tag: {scoped}"
    );
    assert!(
        !scoped.to_string().contains("what we decided"),
        "the scoped view leaked the session summary: {scoped}"
    );

    let archived = tools
        .call(
            &alice.bearer,
            &session,
            "get_session",
            json!({"session_id": "pi:viewed", "view": "archive"}),
        )
        .await;
    assert!(
        archived.to_string().contains("gateway-decision")
            && archived.to_string().contains("what we decided"),
        "the archive view withheld the owner's own prose: {archived}"
    );
    assert_eq!(content(&archived)["view"].as_str(), Some("archive"));

    // Bob cannot read it at all, in either view, and his refusal names nothing.
    for view in [None, Some("archive")] {
        let arguments = match view {
            None => json!({"session_id": "pi:viewed"}),
            Some(view) => json!({"session_id": "pi:viewed", "view": view}),
        };
        let answer = tools
            .call(&bob.bearer, &bob_session, "get_session", arguments)
            .await;
        assert!(refused(&answer), "bob read alice's session: {answer}");
        assert!(
            !answer.to_string().contains("gateway-decision"),
            "the refusal carried the prose anyway: {answer}"
        );
    }

    // Unknown views are refused rather than silently treated as one of the two.
    let bogus = tools
        .call(
            &alice.bearer,
            &session,
            "list_sessions",
            json!({"view": "everything"}),
        )
        .await;
    assert!(refused(&bogus), "an unknown view was accepted: {bogus}");
}

// ---------------------------------------------------------------------------------------
// Listing: projects and the review partition
// ---------------------------------------------------------------------------------------

/// The project listing is the caller's own, paged exactly once through, and never a foreign oracle.
#[tokio::test(flavor = "multi_thread")]
async fn list_projects_pages_the_callers_own_and_names_no_foreign_project() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-list-alice").await;
    let bob = tools.owner("t8-list-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;

    let mut alice_ids = Vec::new();
    for i in 0..5 {
        let project = establish(
            &tools,
            &alice,
            &alice_session,
            github(&format!("anten/p{i}")),
        )
        .await;
        let handle = project["id"].as_str().unwrap().to_string();
        alice_ids.push(handle.clone());
        // One live memory in each, so a count is a counted number rather than an absent field. The
        // handle is passed exactly as `resolve_project` returned it — a caller should never have to take
        // an opaque id apart to use it.
        let stored = tools
            .call(
                &alice.bearer,
                &alice_session,
                "store_memory",
                json!({"content": format!("a note in project {i}"), "scope": "project", "project": handle}),
            )
            .await;
        assert!(!refused(&stored), "seed store refused: {stored}");
    }
    let mut bob_aliases = Vec::new();
    for i in 0..3 {
        let alias = format!("anten/p{i}");
        establish(&tools, &bob, &bob_session, github(&alias)).await;
        bob_aliases.push(alias);
    }

    let mut seen = Vec::new();
    let mut cursor = None;
    for _ in 0..10 {
        let arguments = match &cursor {
            None => json!({"limit": 2}),
            Some(cursor) => json!({"limit": 2, "cursor": cursor}),
        };
        let page = tools
            .call(&alice.bearer, &alice_session, "list_projects", arguments)
            .await;
        assert!(!refused(&page), "{page}");
        let body = content(&page);
        let rows = body["projects"].as_array().cloned().unwrap_or_default();
        assert!(rows.len() <= 2, "a page exceeded its limit: {body}");
        for row in &rows {
            assert_eq!(
                row["memory_count"].as_i64(),
                Some(1),
                "a project count absorbed another account's rows: {row}"
            );
            seen.push(row["id"].as_str().unwrap().to_string());
        }
        match body["next_cursor"].as_str() {
            None => {
                assert_eq!(
                    body["truncated"].as_bool(),
                    Some(false),
                    "the last page claimed a continuation: {body}"
                );
                break;
            }
            Some(next) => cursor = Some(next.to_string()),
        }
    }
    assert_eq!(
        seen.iter().collect::<std::collections::HashSet<_>>().len(),
        5,
        "the pages did not visit each project exactly once: {seen:?}"
    );
    assert_eq!(seen.len(), 5, "a page repeated a project: {seen:?}");

    // No foreign alias, id or count leaks into the listing, and Bob's identical names are his own.
    let rendered = tools
        .call(
            &alice.bearer,
            &alice_session,
            "list_projects",
            json!({"limit": 100}),
        )
        .await
        .to_string();
    for alias in bob_aliases {
        assert!(
            rendered.contains(&format!("github.com/{alias}"))
                || rendered.contains("github.com/anten/p0"),
            "the listing lost an alias the caller owns: {rendered}"
        );
    }
    let bob_page = tools
        .call(
            &bob.bearer,
            &bob_session,
            "list_projects",
            json!({"limit": 100}),
        )
        .await;
    let rows = content(&bob_page)["projects"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        rows.len(),
        3,
        "bob's listing is not his own three: {bob_page}"
    );
    for row in &rows {
        assert_eq!(
            row["memory_count"].as_i64(),
            Some(0),
            "bob counted alice's memories into his own projects: {row}"
        );
        assert!(
            !seen.contains(&row["id"].as_str().unwrap().to_string()),
            "the two listings share a project id: {row}"
        );
    }

    // A cursor is a keyset position in the caller's own alias ordering, so a string that sorts past every
    // alias the caller holds answers as an empty last page — it must not fabricate rows, restart the walk
    // from the beginning, or claim a continuation. (A project id is a `table:key` handle and an alias is
    // free text; the two are distinguished by shape, so there is nothing syntactically "malformed" about
    // a resumed alias, and refusing one would refuse a legitimate position.)
    let ahead = tools
        .call(
            &alice.bearer,
            &alice_session,
            "list_projects",
            json!({"cursor": "zzzz-resume-from-nowhere"}),
        )
        .await;
    assert!(!refused(&ahead), "{ahead}");
    assert_eq!(
        content(&ahead)["projects"].as_array().unwrap().len(),
        0,
        "{ahead}"
    );
    assert_eq!(
        content(&ahead)["next_cursor"].as_str(),
        None,
        "an empty resumed page claimed a continuation: {ahead}"
    );
    let malformed_handle = tools
        .call(
            &alice.bearer,
            &alice_session,
            "list_projects",
            json!({"project": "fact:not-a-project"}),
        )
        .await;
    let _ = malformed_handle;
    let oversized_limit = tools
        .call(
            &alice.bearer,
            &alice_session,
            "list_projects",
            json!({"limit": 100_000}),
        )
        .await;
    assert_eq!(
        content(&oversized_limit)["limit"].as_u64(),
        Some(100),
        "the page limit was not clamped to the published maximum"
    );
}

/// Unassigned is reviewable, disclosed as review, and still invisible to recall and to every other scope.
#[tokio::test(flavor = "multi_thread")]
async fn list_unassigned_memories_discloses_review_state_without_affecting_recall() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-unassigned-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    // The fixture owns this one write path: nothing in Task 8's public surface may stage a row into
    // Unassigned, and that refusal is asserted below rather than assumed.
    let id = tools
        .rows(
            "CREATE type::record('fact', 'u-one') SET content = 'never classified', confidence = 0.5, \
             embedding = [1.0, 0.0], tags = [], deleted = false, account = $acc, workspace = $ws, \
             scope = 'unassigned' RETURN AFTER;",
            json!({"acc": alice.account, "ws": alice.workspace}),
        )
        .await;
    assert_eq!(id.len(), 1);

    let page = tools
        .call(
            &alice.bearer,
            &session,
            "list_unassigned_memories",
            json!({"limit": 20}),
        )
        .await;
    assert!(!refused(&page), "{page}");
    let body = content(&page);
    assert_eq!(body["count"].as_i64(), Some(1), "{body}");
    assert_eq!(body["review"].as_bool(), Some(true));
    assert_eq!(body["eligible_for_recall"].as_bool(), Some(false));
    assert!(
        body["disclosure"]
            .as_str()
            .unwrap_or_default()
            .contains("review"),
        "the response did not disclose what it is: {body}"
    );
    assert_eq!(
        body["memories"][0]["content"].as_str(),
        Some("never classified")
    );

    // The same content is unreachable from every recall scope, and an untargeted store cannot stage one.
    for scope in ["auto", "personal", "assigned"] {
        let read = tools
            .call(
                &alice.bearer,
                &session,
                "retrieve_memories",
                json!({"query": "never classified", "scope": scope, "limit": 10}),
            )
            .await;
        assert!(
            !read.to_string().contains("never classified"),
            "`{scope}` recall reached the Unassigned partition: {read}"
        );
    }
    let staged = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "trying to stage", "scope": "unassigned"}),
        )
        .await;
    assert!(
        refused(&staged),
        "a hosted store was allowed to stage into Unassigned: {staged}"
    );
    assert_eq!(
        tools
            .count("SELECT count() AS c FROM fact WHERE content = 'trying to stage' GROUP ALL;")
            .await,
        0
    );

    // Another account cannot see it, in either direction.
    let bob = tools.owner("t8-unassigned-bob").await;
    let bob_session = tools.handshake(&bob.bearer).await;
    let theirs = tools
        .call(
            &bob.bearer,
            &bob_session,
            "list_unassigned_memories",
            json!({}),
        )
        .await;
    assert!(!refused(&theirs), "{theirs}");
    assert_eq!(content(&theirs)["count"].as_i64(), Some(0));
    assert!(
        !theirs.to_string().contains("never classified"),
        "the review listing leaked across accounts: {theirs}"
    );
}

// ---------------------------------------------------------------------------------------
// Proposals
// ---------------------------------------------------------------------------------------

/// A proposal is persisted as pending, names the revision it was formed against, and moves nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_is_persisted_pending_and_moves_no_memory() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-prop-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let project = establish(&tools, &alice, &session, github("anten/proposal")).await;
    let stored = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "an ssh deployment detail", "scope": "personal"}),
        )
        .await;
    let id = content(&stored)["id"].as_str().unwrap().to_string();
    let before = tools.stored_fact(&id).await;

    let answer = tools
        .call(
            &alice.bearer,
            &session,
            "propose_memory_scope",
            json!({"proposals": [{
                "id": id,
                "expected_revision": before["semantic_revision"].as_u64().unwrap_or(0),
                "destination": "project",
                "project": project["id"],
                "reason": "the note is specific to this repository",
            }]}),
        )
        .await;
    assert!(!refused(&answer), "a lawful proposal refused: {answer}");
    let body = content(&answer);
    assert_eq!(body["applied"].as_bool(), Some(false), "{body}");
    assert_eq!(body["count"].as_i64(), Some(1), "{body}");
    assert_eq!(body["refused"].as_array().unwrap().len(), 0, "{body}");
    assert_eq!(body["proposals"][0]["state"].as_str(), Some("pending"));
    let proposal_id = body["proposals"][0]["proposal_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The fact is byte-for-byte untouched: this is the whole point of the tool.
    let after = tools.stored_fact(&id).await;
    assert_eq!(after, before, "the proposal moved the memory it described");

    let rows = tools
        .rows(
            "SELECT account, workspace, fact, destination_kind, destination_project, source_revision, \
             revision, state FROM classification_proposal WHERE id = $id;",
            json!({"id": proposal_id}),
        )
        .await;
    assert_eq!(rows.len(), 1, "the pending row is not stored: {rows:?}");
    assert_eq!(rows[0]["state"].as_str(), Some("pending"));
    assert_eq!(rows[0]["revision"].as_i64(), Some(0));
    assert_eq!(rows[0]["destination_kind"].as_str(), Some("project"));
    assert_eq!(
        rows[0]["account"].as_str(),
        Some(alice.account.as_str()),
        "the proposal's owner came from somewhere other than the context"
    );
    assert_eq!(
        rows[0]["workspace"].as_str(),
        Some(alice.workspace.as_str())
    );

    // The same judgement twice is one row, not two.
    let again = tools
        .call(
            &alice.bearer,
            &session,
            "propose_memory_scope",
            json!({"proposals": [{
                "id": id,
                "expected_revision": before["semantic_revision"].as_u64().unwrap_or(0),
                "destination": "project",
                "project": project["id"],
                "reason": "the note is specific to this repository",
            }]}),
        )
        .await;
    assert_eq!(
        content(&again)["proposals"][0]["proposal_id"].as_str(),
        Some(proposal_id.as_str()),
        "a replay created a second proposal: {again}"
    );
    assert_eq!(
        tools
            .count("SELECT count() AS c FROM classification_proposal GROUP ALL;")
            .await,
        1,
        "the replay duplicated the row"
    );

    // And no path from here changed what recall answers: the memory is still found under personal, not
    // under the project it was proposed into.
    let read = tools
        .call(
            &alice.bearer,
            &session,
            "retrieve_memories",
            json!({"query": "ssh deployment detail", "scope": "personal", "limit": 10}),
        )
        .await;
    assert!(ids_of(&content(&read)).contains(&id), "{read}");
    let project_read = tools
        .call(
            &alice.bearer,
            &session,
            "retrieve_memories",
            json!({"query": "ssh deployment detail", "scope": "project", "project": project["id"], "limit": 10}),
        )
        .await;
    assert!(
        !ids_of(&content(&project_read)).contains(&id),
        "a proposal made the memory searchable in the project it named: {project_read}"
    );
}

/// A stale revision, a foreign fact, a foreign destination and an over-long batch all refuse without
/// writing, and each refusal is the one the caller can act on.
#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_refuses_stale_foreign_and_oversized_requests_without_writing() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-pref-alice").await;
    let bob = tools.owner("t8-pref-bob").await;
    let session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;
    let _mine = establish(&tools, &alice, &session, github("anten/mine")).await;
    let theirs = establish(&tools, &bob, &bob_session, github("anten/theirs")).await;
    let stored = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "a fact with a revision", "scope": "personal"}),
        )
        .await;
    let id = content(&stored)["id"].as_str().unwrap().to_string();
    let revision = tools.stored_fact(&id).await["semantic_revision"]
        .as_u64()
        .unwrap_or(0);

    let stale = tools
        .call(
            &alice.bearer,
            &session,
            "propose_memory_scope",
            json!({"proposals": [{"id": id, "expected_revision": revision + 7, "destination": "personal"}]}),
        )
        .await;
    let text = text_of(&stale) + &stale.to_string();
    assert!(refused(&stale) || !text.is_empty(), "{stale}");
    assert!(
        text_of(&stale).contains("stale")
            || content(&stale)["refused"].to_string().contains("stale"),
        "a stale proposal did not say so: {stale}"
    );

    let foreign_project = tools
        .call(
            &alice.bearer,
            &session,
            "propose_memory_scope",
            json!({"proposals": [{"id": id, "expected_revision": revision, "destination": "project", "project": theirs["id"]}]}),
        )
        .await;
    let absent_project = tools
        .call(
            &alice.bearer,
            &session,
            "propose_memory_scope",
            json!({"proposals": [{"id": id, "expected_revision": revision, "destination": "project", "project": "project:absent"}]}),
        )
        .await;
    for answer in [&foreign_project, &absent_project] {
        let refused_rows = content(answer)["refused"].to_string();
        assert!(
            refused(answer) || refused_rows.contains("not found"),
            "a foreign destination was accepted: {answer}"
        );
    }
    assert_eq!(
        foreign_project
            .to_string()
            .replace(theirs["id"].as_str().unwrap_or_default(), "X"),
        absent_project.to_string().replace("project:absent", "X"),
        "a foreign project and an absent one answered differently, which is an oracle"
    );

    // Bob cannot propose against Alice's fact.
    let bobs_attempt = tools
        .call(
            &bob.bearer,
            &bob_session,
            "propose_memory_scope",
            json!({"proposals": [{"id": id, "expected_revision": revision, "destination": "personal"}]}),
        )
        .await;
    assert!(
        content(&bobs_attempt)["refused"]
            .to_string()
            .contains("not found"),
        "a foreign fact was proposal-able: {bobs_attempt}"
    );

    let oversized: Vec<Json> = (0..21)
        .map(|_| json!({"id": id, "expected_revision": revision, "destination": "personal"}))
        .collect();
    let too_many = tools
        .call(
            &alice.bearer,
            &session,
            "propose_memory_scope",
            json!({"proposals": oversized}),
        )
        .await;
    assert!(
        refused(&too_many) || text_of(&too_many).contains("20"),
        "a batch over the published cap was accepted: {too_many}"
    );
    let empty = tools
        .call(
            &alice.bearer,
            &session,
            "propose_memory_scope",
            json!({"proposals": []}),
        )
        .await;
    assert!(refused(&empty), "an empty batch was accepted: {empty}");

    // Every refusal above wrote nothing.
    assert_eq!(
        tools
            .count("SELECT count() AS c FROM classification_proposal GROUP ALL;")
            .await,
        0,
        "a refused proposal wrote a row"
    );
    assert_eq!(
        tools.stored_fact(&id).await["scope"].as_str(),
        Some("personal")
    );

    // Positive control: the lawful request on the same store and the same fact does land.
    let ok = tools
        .call(
            &alice.bearer,
            &session,
            "propose_memory_scope",
            json!({"proposals": [{"id": id, "expected_revision": revision, "destination": "personal"}]}),
        )
        .await;
    assert_eq!(
        content(&ok)["count"].as_i64(),
        Some(1),
        "the lawful proposal refused, so every refusal above proved nothing: {ok}"
    );
}

// ---------------------------------------------------------------------------------------
// The published surface
// ---------------------------------------------------------------------------------------

/// The advertised surface is exactly the seventeen documented tools, read from `tools/list`.
#[tokio::test(flavor = "multi_thread")]
async fn the_advertised_public_surface_is_exactly_the_seventeen_documented_tools() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-surface-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let mut names: Vec<String> = tools
        .listed(&alice.bearer, &session)
        .await
        .into_iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    let expected = [
        "cancel_reminder",
        "check_reminders",
        "delete_memory",
        "finalize_session",
        "get_session",
        "import_document",
        "list_projects",
        "list_reminders",
        "list_sessions",
        "list_unassigned_memories",
        "propose_memory_scope",
        "recall",
        "resolve_project",
        "retrieve_memories",
        "set_reminder",
        "store_memory",
        "update_memory",
    ];
    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(
        names, expected,
        "the advertised tool set is not the documented seventeen: {names:?}"
    );
    assert_eq!(
        names.len(),
        17,
        "a duplicate name can hide an eighteenth tool"
    );

    // The advertised usage `instructions` are part of the same public surface: they are what an agent
    // reads before choosing a tool, and Task 8 added four tools whose whole purpose is invisible
    // without them. Asserting here (rather than in a prose doc) is what keeps the guidance from rotting
    // away from the surface it describes.
    let (_, info) = tools.initialize(&alice.bearer).await;
    let instructions = info["instructions"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    for claim in [
        "resolve_project",
        "context_project",
        "propose_memory_scope",
        "view: \"archive\"",
    ] {
        assert!(
            instructions.contains(claim),
            "the advertised instructions no longer mention {claim:?}: {instructions}"
        );
    }
    assert!(
        instructions.contains("it moves nothing"),
        "the instructions must still say a proposal applies nothing: {instructions}"
    );
    for internal in [
        "retrieve_memories_dry",
        "retrieve_memories_unfiltered",
        "do_store_memory",
        "store_response",
    ] {
        assert!(
            !names.iter().any(|name| name == internal),
            "{internal} is not a public tool and must not be advertised"
        );
    }
    // No public parameter names an account, a workspace or an owner: the transport resolves those, and a
    // caller-supplied one would be the bypass this whole lane is built to prevent.
    for tool in tools.listed(&alice.bearer, &session).await {
        let properties = tool["inputSchema"]["properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        for key in properties.keys() {
            let lowered = key.to_lowercase();
            assert!(
                !(lowered.contains("account")
                    || lowered.contains("workspace")
                    || lowered.contains("owner")
                    || lowered == "user"),
                "{} advertises a {key:?} parameter, which would be a caller-supplied authority",
                tool["name"]
            );
        }
    }
}

// ---------------------------------------------------------------------------------------
// §Required-test 2 — "unknown project selector actionable NotFound/InvalidInput,
// indistinguishable where required"
// ---------------------------------------------------------------------------------------

/// A caller that names a project the server cannot resolve is refused in one of two honest ways: the
/// request shape was wrong (and the message names the fix), or the selector simply is not theirs (and the
/// message is the single `NotFound` answer that cannot be probed). Neither may echo the submitted value,
/// and neither may write a row.
#[tokio::test(flavor = "multi_thread")]
async fn an_unresolvable_project_selector_refuses_actionably_and_writes_nothing() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-selector-alice").await;
    let session = tools.handshake(&alice.bearer).await;

    // (a) `scope: "project"` with no target at all: the shape is wrong, so the refusal says what to send.
    let no_target = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "orphan note", "scope": "project"}),
        )
        .await;
    assert!(refused(&no_target), "{no_target}");
    assert!(
        text_of(&no_target).contains("context_project")
            && text_of(&no_target).contains("nothing was stored"),
        "the refusal did not name the missing parameter or say nothing happened: {no_target}"
    );

    // (b) A whitespace-only target is the same absence — a client's JSON encoder sends `""` for an unset
    // field, and treating that as a name would make a blank string a lookup.
    let blank_target = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "orphan note", "scope": "project", "project": "   "}),
        )
        .await;
    assert_eq!(
        text_of(&blank_target),
        text_of(&no_target),
        "a blank `project` answered differently from an absent one: {blank_target}"
    );

    // (c) An unknown alias and an unknown handle: one `NotFound` shape, no echo of what was submitted, no
    // hint about whether the name exists somewhere else.
    let unknown_alias = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "orphan note", "scope": "project", "project": "github.com/nobody/nothing"}),
        )
        .await;
    let unknown_handle = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "orphan note", "scope": "project", "project": "project:never-minted"}),
        )
        .await;
    assert!(
        refused(&unknown_alias) && refused(&unknown_handle),
        "{unknown_alias} / {unknown_handle}"
    );
    assert_eq!(
        text_of(&unknown_alias),
        text_of(&unknown_handle),
        "an unknown alias and an unknown handle answered differently, which is an oracle"
    );
    // One uniform public shape: `{"message":"not found","status":"error"}`. It names no table, no
    // workspace and none of the submitted text, so a caller cannot tell an absent project from somebody
    // else's by reading the refusal.
    for answer in [&unknown_alias, &unknown_handle] {
        let said = text_of(answer);
        assert!(
            said.contains("not found")
                && !said.contains("nobody")
                && !said.contains("nothing")
                && !said.contains("never-minted")
                && !said.contains("github.com")
                && !said.contains("project:"),
            "the refusal echoed the submitted selector or named a handle: {said}"
        );
    }

    // (d) An unknown scope word is the third honest answer: your request is malformed, here are the two
    // that exist.
    let bad_scope = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "orphan note", "scope": "shared"}),
        )
        .await;
    assert!(
        refused(&bad_scope)
            && text_of(&bad_scope).contains("unknown scope 'shared': use 'personal' or 'project'"),
        "an unknown scope word did not name itself and the valid set: {bad_scope}"
    );

    // (e) `resolve_project` with neither arm is refused with the fix in the message, because this is the
    // only tool that can create a project and a blank call must not mint one.
    let bare_resolve = tools
        .call(
            &alice.bearer,
            &session,
            "resolve_project",
            json!({"project": ""}),
        )
        .await;
    assert!(
        refused(&bare_resolve)
            && text_of(&bare_resolve).contains("resolve_project needs `project`"),
        "a bare resolve_project was not refused with its own fix: {bare_resolve}"
    );

    // Positive control, and the reason the counts above are not vacuous: the same caller, in the same
    // session, *can* establish a project and store into it. A refusal test whose fixture could never
    // succeed would pass on a broken server.
    let established = establish(&tools, &alice, &session, github("anten/control")).await;
    let stored = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "orphan note", "scope": "project", "project": established["id"]}),
        )
        .await;
    assert!(!refused(&stored), "the control store refused: {stored}");
    assert_eq!(
        tools
            .count("SELECT count() AS c FROM fact WHERE content = 'orphan note' GROUP ALL;")
            .await,
        1,
        "exactly the control write should exist; a refused selector wrote a row"
    );
}

// ---------------------------------------------------------------------------------------
// §Required-test 2 — "project list/unassigned cursor bounds, limits and continuation completeness"
// ---------------------------------------------------------------------------------------

/// The review listing bounds its page and completes its walk: a default, a clamp rather than a refusal at
/// the cap, and a cursor chain that visits every owned row exactly once.
#[tokio::test(flavor = "multi_thread")]
async fn list_unassigned_memories_bounds_its_page_and_completes_its_walk() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-unassigned-page-alice").await;
    let bob = tools.owner("t8-unassigned-page-bob").await;
    let session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;

    // Three review rows for the caller, one for somebody else. The fixture writes them directly because
    // no public route may stage a row into Unassigned.
    for index in 0..3 {
        tools
            .rows(
                "CREATE type::record('fact', $key) SET content = $content, confidence = 0.5, \
                 embedding = [1.0, 0.0], tags = [], deleted = false, account = $acc, \
                 workspace = $ws, scope = 'unassigned' RETURN AFTER;",
                json!({
                    "key": format!("up{index}"),
                    "content": format!("unassigned review note {index}"),
                    "acc": alice.account,
                    "ws": alice.workspace,
                }),
            )
            .await;
    }
    tools
        .rows(
            "CREATE type::record('fact', 'theirs') SET content = 'another accounts review note', \
             confidence = 0.5, embedding = [1.0, 0.0], tags = [], deleted = false, account = $acc, \
             workspace = $ws, scope = 'unassigned' RETURN AFTER;",
            json!({"acc": bob.account, "ws": bob.workspace}),
        )
        .await;

    // The default page size is the published one, and it is reported in the response rather than implied.
    let first = tools
        .call(
            &alice.bearer,
            &session,
            "list_unassigned_memories",
            json!({"limit": 2}),
        )
        .await;
    assert!(!refused(&first), "{first}");
    let body = content(&first);
    assert_eq!(body["limit"].as_i64(), Some(2), "{body}");
    assert_eq!(
        body["memories"]
            .as_array()
            .map(|rows| rows.len())
            .unwrap_or_default(),
        2,
        "a bounded page did not honor its bound: {body}"
    );
    let cursor = body["next_cursor"].as_str().unwrap_or_default().to_string();
    assert!(
        !cursor.is_empty(),
        "a page with more rows claimed no continuation: {body}"
    );

    // An over-cap request is clamped to the published bound, not refused and not honored: a caller asking
    // for the whole store gets a page and a continuation.
    let greedy = tools
        .call(
            &alice.bearer,
            &session,
            "list_unassigned_memories",
            json!({"limit": 100_000}),
        )
        .await;
    assert!(!refused(&greedy), "{greedy}");
    assert_eq!(
        content(&greedy)["limit"].as_i64(),
        Some(100),
        "the published cap is 100 and the response did not say so: {greedy}"
    );

    // Continuation completeness: walking the cursors returns exactly the caller's three rows, each once,
    // and never the other account's.
    // The walk continues from the page already fetched, so its rows are part of the answer too — a
    // completeness assertion that only counted the resumed hops would pass on a cursor that skipped the
    // head of the range.
    let mut seen: Vec<String> = content(&first)["memories"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|row| row["id"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        seen.len(),
        2,
        "the first page did not carry two rows: {seen:?}"
    );
    let mut page = Some(cursor);
    for hop in 0..10 {
        let arguments = match page.take() {
            None => json!({"limit": 2}),
            Some(cursor) => json!({"limit": 2, "cursor": cursor}),
        };
        let answer = tools
            .call(
                &alice.bearer,
                &session,
                "list_unassigned_memories",
                arguments,
            )
            .await;
        assert!(!refused(&answer), "hop {hop}: {answer}");
        let body = content(&answer);
        for row in body["memories"].as_array().cloned().unwrap_or_default() {
            seen.push(row["id"].as_str().unwrap_or_default().to_string());
        }
        page = body["next_cursor"].as_str().map(str::to_string);
        if page.is_none() {
            break;
        }
    }
    assert_eq!(page, None, "the walk did not terminate");
    seen.sort();
    let mut deduped = seen.clone();
    deduped.dedup();
    assert_eq!(seen, deduped, "the cursor chain repeated a row: {seen:?}");
    assert_eq!(
        seen,
        vec![
            "fact:up0".to_string(),
            "fact:up1".to_string(),
            "fact:up2".to_string(),
        ],
        "the walk did not return exactly the caller's own review rows"
    );
    let foreign = tools
        .call(
            &bob.bearer,
            &bob_session,
            "list_unassigned_memories",
            json!({"limit": 100}),
        )
        .await;
    assert!(!refused(&foreign), "{foreign}");
    assert_eq!(
        content(&foreign)["memories"]
            .as_array()
            .map(|rows| rows.len())
            .unwrap_or_default(),
        1,
        "the review page showed another account's row, or hid its own"
    );
}

// ---------------------------------------------------------------------------------------
// §P1-2 — recall's `scope` restricts the recalled population, not just the candidate list
// ---------------------------------------------------------------------------------------

/// Near-identical seeds in three partitions, and four selections that must answer differently.
///
/// REWRITTEN to the partition-pure store semantics the signed base implements (owner decision;
/// the task-8 mixed-cluster semantics are superseded): near-identical seeds across partitions land
/// in **separate partition-scoped clusters** — the store path ranks destinations per partition and
/// refuses a join that would leave one cluster holding members of two. What this test pins is the
/// *read* side on top of that structure: `recall`'s `scope` word restricts the recalled population,
/// so each selection surfaces exactly the members it may read (never another partition's), and a
/// replayed handle answers from the selection it was replayed under. A cluster with no member in
/// the resolved population is neither offered nor replayable.
#[tokio::test(flavor = "multi_thread")]
async fn recall_scope_selects_which_cluster_members_are_recalled() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-recall-scope-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let alpha = establish(&tools, &alice, &session, github("anten/alpha")).await;
    let beta = establish(&tools, &alice, &session, github("anten/beta")).await;

    // The positive control the empty assertions below depend on: this one owner holds live memories in
    // *three* different partitions, and they are near enough alike to be recalled together. Without that,
    // "a personal read returned no project row" would pass on a corpus that has no project rows at all.
    let mut placed: Vec<(String, String)> = Vec::new();
    for (content_text, scope, target) in [
        ("rotation policy personal", "personal", None),
        (
            "rotation policy alpha project",
            "project",
            Some(alpha["id"].as_str().unwrap().to_string()),
        ),
        (
            "rotation policy beta project",
            "project",
            Some(beta["id"].as_str().unwrap().to_string()),
        ),
    ] {
        let arguments = match target {
            None => json!({"content": content_text, "scope": scope}),
            Some(project) => json!({
                "content": content_text, "scope": scope, "project": project,
            }),
        };
        let stored = tools
            .call(&alice.bearer, &session, "store_memory", arguments)
            .await;
        assert!(!refused(&stored), "seed store refused: {stored}");
        placed.push((
            content_text.to_string(),
            content(&stored)["scope"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ));
    }
    // The server itself confirmed the spread: one personal label and two distinct project handles, so the
    // emptiness assertions below are about selection and not about an absent corpus.
    let partitions: Vec<String> = placed.iter().map(|(_, label)| label.clone()).collect();
    assert_eq!(
        partitions[0], "personal",
        "the personal seed was not stamped personal: {partitions:?}"
    );
    assert_eq!(
        partitions,
        vec![
            "personal".to_string(),
            format!("project:{}", short(&alpha)),
            format!("project:{}", short(&beta)),
        ],
        "the three seeds did not land in three distinct partitions: {partitions:?}"
    );

    // The partition-pure structure itself, as a positive control for everything below: the three
    // near-identical seeds did NOT share a cluster — each partition got its own. Without this, a
    // regression back to mixed clusters would still pass the selection assertions below (they pin
    // the read side, which masks membership), so the structure this rewrite exists for is asserted
    // where it lives.
    let cluster_rows = tools
        .rows(
            "SELECT id FROM cluster WHERE workspace = $workspace;",
            json!({"workspace": alice.workspace}),
        )
        .await;
    assert_eq!(
        cluster_rows.len(),
        3,
        "the three partition seeds did not land in three separate clusters: {cluster_rows:?}"
    );

    let members_of = |body: &Json| -> Vec<String> {
        body["clusters"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .flat_map(|cluster| {
                cluster["representative_memories"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|m| m["content"].as_str().map(str::to_string))
            .collect()
    };
    let recall = |scope: Json| {
        let tools = &tools;
        let alice = &alice;
        let session = &session;
        async move {
            let answer = tools
                .call(alice.bearer.as_str(), session.as_str(), "recall", scope)
                .await;
            assert!(!refused(&answer), "{answer}");
            (content(&answer), answer)
        }
    };

    // 1. `auto` with no project: personal only.
    let (broad_auto, _) = recall(json!({"query": "rotation policy"})).await;
    assert_eq!(
        broad_auto["scope"].as_str(),
        Some("personal"),
        "auto without a project did not report the personal selection: {broad_auto}"
    );
    assert_eq!(
        members_of(&broad_auto),
        vec!["rotation policy personal".to_string()],
        "an auto recall with no project surfaced a project memory"
    );

    // 2. `auto` + project: personal plus that project, never the other one.
    let (broad_auto_project, _) = recall(json!({
        "query": "rotation policy", "scope": "auto", "project": alpha["id"],
    }))
    .await;
    assert_eq!(
        broad_auto_project["scope"].as_str(),
        Some("personal+project"),
        "{broad_auto_project}"
    );
    let mut surfaced = members_of(&broad_auto_project);
    surfaced.sort();
    assert_eq!(
        surfaced,
        vec![
            "rotation policy alpha project".to_string(),
            "rotation policy personal".to_string(),
        ],
        "auto with a project did not cover exactly personal plus that project"
    );

    // 3. `project` alone: that project and nothing else.
    let (broad_project, _) = recall(json!({
        "query": "rotation policy", "scope": "project", "project": beta["id"],
    }))
    .await;
    assert_eq!(
        broad_project["scope"].as_str(),
        Some("project"),
        "{broad_project}"
    );
    assert_eq!(
        members_of(&broad_project),
        vec!["rotation policy beta project".to_string()],
        "a project recall served a partition the caller did not name"
    );

    // 4. `assigned`: every owned assigned partition, and never the review partition.
    tools
        .rows(
            "CREATE type::record('fact', 'rc-unassigned') SET content = 'rotation policy unassigned', \
             confidence = 0.5, embedding = [1.0, 0.0], tags = [], deleted = false, account = $acc, \
             workspace = $ws, scope = 'unassigned' RETURN AFTER;",
            json!({"acc": alice.account, "ws": alice.workspace}),
        )
        .await;
    let (broad_assigned, _) =
        recall(json!({"query": "rotation policy", "scope": "assigned"})).await;
    assert_eq!(
        broad_assigned["scope"].as_str(),
        Some("assigned"),
        "{broad_assigned}"
    );
    let mut assigned = members_of(&broad_assigned);
    assigned.sort();
    assert_eq!(
        assigned,
        vec![
            "rotation policy alpha project".to_string(),
            "rotation policy beta project".to_string(),
            "rotation policy personal".to_string(),
        ],
        "an assigned recall did not span exactly the owned assigned partitions"
    );

    // 5. The focused replay is bound to the same population: a handle issued under one selection answers
    //    from the members that selection may read, never from the cluster's whole membership.
    let handle = broad_project["clusters"][0]["scope_handle"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!handle.is_empty(), "no handle to replay: {broad_project}");
    let replay = tools
        .call(
            &alice.bearer,
            &session,
            "recall",
            json!({
                "query": "rotation policy", "scope": "project", "project": beta["id"],
                "scope_handle": handle,
            }),
        )
        .await;
    assert!(!refused(&replay), "{replay}");
    let focused = content(&replay);
    assert_eq!(focused["mode"].as_str(), Some("focused"), "{focused}");
    let served: Vec<String> = focused["memories"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        served,
        vec!["rotation policy beta project".to_string()],
        "a replayed handle served members outside the selection it was replayed under"
    );

    // 6. Replaying that handle under a *different* selection is legal — a handle names a cluster, not a
    //    permission — but the served set is still the selection's read of that cluster. Under the
    //    partition-pure store the beta handle names the beta partition's own cluster, which holds no
    //    member a personal selection may read, so the replay answers with nothing. That is the
    //    discriminating case rather than a vacuous one: a replay that widened the read would serve
    //    the beta member, and point 5 already proved the handle itself resolves to a populated
    //    cluster under its own selection.
    let cross = tools
        .call(
            &alice.bearer,
            &session,
            "recall",
            json!({
                "query": "rotation policy", "scope": "personal", "scope_handle": handle,
            }),
        )
        .await;
    assert!(!refused(&cross), "{cross}");
    let cross_body = content(&cross);
    assert_eq!(cross_body["mode"].as_str(), Some("focused"), "{cross_body}");
    let served: Vec<String> = cross_body["memories"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        served,
        Vec::<String>::new(),
        "replaying a handle under a selection with no member in its cluster still served rows"
    );

    // 7. A cluster with no eligible member is not offered at all, which is also what stops an outsider
    //    from enumerating this caller's clusters: their recall answers from their own population or
    //    with nothing.
    let outsider = tools.owner("t8-recall-scope-outsider").await;
    let outsider_session = tools.handshake(&outsider.bearer).await;
    let theirs = tools
        .call(
            &outsider.bearer,
            &outsider_session,
            "recall",
            json!({"query": "rotation policy"}),
        )
        .await;
    assert!(!refused(&theirs), "{theirs}");
    assert_eq!(
        members_of(&content(&theirs)),
        Vec::<String>::new(),
        "another account's recall surfaced this caller's cluster members: {theirs}"
    );
    assert!(
        !text_of(&theirs).contains("rotation policy"),
        "a foreign recall carried the content of another account's cluster: {theirs}"
    );
}

// ---------------------------------------------------------------------------------------
// §F1 — the write path's centroid is partition-pure: each partition-scoped cluster's running
// average counts that partition's own membership, never another partition's. This test exists
// because `CountingEmbedding` returns the same vector for every text and cannot distinguish
// averaging from replacement.
// ---------------------------------------------------------------------------------------

fn nearly(actual: &[f32], expected: &[f32]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(a, e)| (a - e).abs() < 0.0001)
}

/// REWRITTEN to the partition-pure store semantics the signed base implements (owner decision;
/// the task-8 mixed-cluster semantics are superseded): a cross-partition store does NOT join the
/// caller's cluster in another partition — the destination ranking is per partition and a join
/// that would straddle two is refused — so it mints that partition's own cluster, initialized
/// from its sole member, and the first cluster's centroid is untouched. The running average a
/// same-partition store performs counts the cluster's own (single-partition) membership.
#[tokio::test(flavor = "multi_thread")]
async fn a_cross_partition_store_updates_the_cluster_centroid_from_workspace_membership() {
    let tools = Tools::hosted_with_scripted_embeddings(vec![
        ("centroid first personal", vec![1.0f32, 0.0f32]),
        ("centroid project second", vec![0.9f32, 0.1f32]),
        ("centroid third same partition", vec![0.8f32, 0.2f32]),
    ])
    .await;
    let alice = tools.owner("t8-centroid-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let project = establish(&tools, &alice, &session, github("anten/centroid")).await;

    let first = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "centroid first personal", "scope": "personal"}),
        )
        .await;
    assert!(!refused(&first), "first seed store refused: {first}");
    let clusters = tools
        .rows(
            "SELECT id FROM cluster WHERE workspace = $workspace ORDER BY id;",
            json!({"workspace": alice.workspace}),
        )
        .await;
    assert_eq!(
        clusters.len(),
        1,
        "the first store did not create exactly one owner cluster: {clusters:?}"
    );
    let cluster_id = clusters[0]["id"].as_str().unwrap().to_string();
    assert!(
        nearly(
            &tools.stored_cluster_centroid(&cluster_id).await,
            &[1.0f32, 0.0f32]
        ),
        "the first cluster should be initialized from its sole member"
    );

    let second = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({
                "content": "centroid project second",
                "scope": "project",
                "project": project["id"]
            }),
        )
        .await;
    assert!(
        !refused(&second),
        "cross-partition seed store refused: {second}"
    );
    // Under the partition-pure store the project seed minted its own cluster rather than joining
    // the personal one, so there are two clusters now — identified by membership, not by list
    // order, because nothing pins which one a bare `ORDER BY id` returns first.
    let cluster_of = |content_text: &str| {
        let tools = &tools;
        let workspace = alice.workspace.clone();
        let content_text = content_text.to_string();
        async move {
            let rows = tools
                .rows(
                    "SELECT VALUE in FROM contains_memory WHERE out IN \
                     (SELECT VALUE id FROM fact WHERE content = $content AND workspace = $workspace);",
                    json!({"content": content_text, "workspace": workspace}),
                )
                .await;
            assert_eq!(
                rows.len(),
                1,
                "the seed did not land in exactly one cluster: {rows:?}"
            );
            rows[0].as_str().unwrap_or_default().to_string()
        }
    };
    let personal_cluster = cluster_of("centroid first personal").await;
    let project_cluster = cluster_of("centroid project second").await;
    assert_ne!(
        personal_cluster, project_cluster,
        "a cross-partition store joined the caller's cluster in another partition"
    );
    assert!(
        nearly(
            &tools.stored_cluster_centroid(&personal_cluster).await,
            &[1.0f32, 0.0f32]
        ),
        "a cross-partition store disturbed the first partition's centroid"
    );
    assert!(
        nearly(
            &tools.stored_cluster_centroid(&project_cluster).await,
            &[0.9f32, 0.1f32]
        ),
        "the new partition's cluster was not initialized from its sole member"
    );

    // Positive control: the third store is in the same partition as the first and must average over
    // that cluster's own membership. If `n` were reset to zero again, the result would be
    // [0.8,0.2], which is visibly different.
    let third = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "centroid third same partition", "scope": "personal"}),
        )
        .await;
    assert!(
        !refused(&third),
        "same-partition control store refused: {third}"
    );
    let after_same = tools.stored_cluster_centroid(&personal_cluster).await;
    assert!(
        nearly(&after_same, &[0.9f32, 0.1f32]),
        "the same-partition positive control did not exercise the running average (got {after_same:?}, expected [0.9,0.1])"
    );
    // And the other partition's cluster is untouched by a store it had no part in.
    let project_after = tools.stored_cluster_centroid(&project_cluster).await;
    assert!(
        nearly(&project_after, &[0.9f32, 0.1f32]),
        "a same-partition store moved another partition's centroid (got {project_after:?})"
    );
}

/// A cluster whose members all live outside the caller's selected partition is not a candidate and
/// carries no handle.
#[tokio::test(flavor = "multi_thread")]
async fn recall_does_not_offer_a_cluster_with_no_member_in_the_requested_partition() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-cluster-empty-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let project = establish(&tools, &alice, &session, github("anten/empty-selection")).await;
    let stored = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "project-only cluster member", "scope": "project", "project": project["id"]}),
        )
        .await;
    assert!(!refused(&stored), "project seed refused: {stored}");

    let personal = tools
        .call(
            &alice.bearer,
            &session,
            "recall",
            json!({"query": "project-only cluster member", "scope": "personal"}),
        )
        .await;
    assert!(!refused(&personal), "{personal}");
    let body = content(&personal);
    let clusters = body["clusters"].as_array().cloned().unwrap_or_default();
    assert!(
        clusters.is_empty(),
        "a personal selection offered a cluster whose only member is project-only: {body}"
    );

    // Positive control: the same cluster is visible when its own partition is selected.
    let scoped = tools
        .call(
            &alice.bearer,
            &session,
            "recall",
            json!({"query": "project-only cluster member", "scope": "project", "project": project["id"]}),
        )
        .await;
    assert!(!refused(&scoped), "{scoped}");
    let text = text_of(&scoped);
    assert!(
        text.contains("project-only cluster member"),
        "the positive control did not offer the cluster it should: {text}"
    );
}

/// An Unassigned row may be graph-adjacent to a caller's cluster but remains invisible to `assigned`
/// recall. The raw fixture is necessary only because production cannot lawfully stage a store into
/// Unassigned; the edge and the assertion are what matter.
#[tokio::test(flavor = "multi_thread")]
async fn assigned_recall_does_not_see_an_unassigned_cluster_member() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-unassigned-recall-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let stored = tools
        .call(
            &alice.bearer,
            &session,
            "store_memory",
            json!({"content": "assigned eligible", "scope": "personal"}),
        )
        .await;
    assert!(!refused(&stored), "personal seed refused: {stored}");
    let clusters = tools
        .rows(
            "SELECT id FROM cluster WHERE workspace = $workspace ORDER BY id;",
            json!({"workspace": alice.workspace}),
        )
        .await;
    let cluster_id = clusters[0]["id"]
        .as_str()
        .expect("personal seed cluster")
        .to_string();
    let unassigned = tools
        .rows(
            "CREATE type::record('fact', 't8-unassigned-recall-trap') SET content = 'assigned ineligible', \
             confidence = 0.5, embedding = [1.0, 0.0], tags = [], deleted = false, account = $account, \
             workspace = $workspace, scope = 'unassigned' RETURN AFTER;",
            json!({"account": alice.account, "workspace": alice.workspace}),
        )
        .await;
    let unassigned_id = unassigned[0]["id"]
        .as_str()
        .expect("unassigned trap fact")
        .to_string();
    tools
        .rows(
            "RELATE $cluster->contains_memory->$fact;",
            json!({"cluster": cluster_id, "fact": unassigned_id}),
        )
        .await;
    // The edge is the fixture's whole point: without asserting it, a silent no-op RELATE could make the
    // negative assertion true simply because the trap row was never a cluster member.
    let cluster_members = tools
        .rows(
            "SELECT id FROM $cluster->contains_memory;",
            json!({"cluster": cluster_id}),
        )
        .await;
    assert_eq!(
        cluster_members.len(),
        2,
        "the Unassigned trap never reached the cluster: {cluster_members:?}"
    );

    let assigned = tools
        .call(
            &alice.bearer,
            &session,
            "recall",
            json!({"query": "assigned eligible", "scope": "assigned"}),
        )
        .await;
    assert!(!refused(&assigned), "{assigned}");
    let text = text_of(&assigned);
    assert!(
        text.contains("assigned eligible"),
        "positive control lost: {text}"
    );
    assert!(
        !text.contains("assigned ineligible"),
        "an assigned recall surfaced an Unassigned cluster member: {text}"
    );
}

// ---------------------------------------------------------------------------------------
// §P1-1 / §P2-15 / §P2-7 / §P2-9 — the session tools' advertised schema, the `project`
// selector end to end, and the disclosure sentence a client renders
// ---------------------------------------------------------------------------------------

/// The session tools' published `inputSchema` is what an agent actually sends against, so it — not the
/// prose — is the thing the docs must match. `scope` is deliberately **not** one of these keys: the
/// session population is derived from `project` and `view`, and a description that names a parameter the
/// schema does not declare makes the tool a place agents send silently-dropped input.
#[tokio::test(flavor = "multi_thread")]
async fn the_session_tool_schemas_advertise_project_and_view_and_no_scope() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-schema-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let advertised: Vec<(String, Vec<String>)> = tools
        .listed(&alice.bearer, &session)
        .await
        .into_iter()
        .map(|tool| {
            let name = tool["name"].as_str().unwrap_or_default().to_string();
            let mut keys: Vec<String> = tool["inputSchema"]["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default()
                .keys()
                .cloned()
                .collect();
            keys.sort();
            (name, keys)
        })
        .collect();
    let schema_of = |wanted: &str| -> Vec<String> {
        advertised
            .iter()
            .find(|(name, _)| name == wanted)
            .unwrap_or_else(|| panic!("{wanted} is not advertised"))
            .1
            .clone()
    };

    assert_eq!(
        schema_of("get_session"),
        vec!["project", "session_id", "view"],
        "get_session's published parameter set moved"
    );
    assert_eq!(
        schema_of("list_sessions"),
        vec![
            "agent_id",
            "finalized",
            "limit",
            "offset",
            "project",
            "tag",
            "view"
        ],
        "list_sessions' published parameter set moved"
    );
    for name in ["get_session", "list_sessions"] {
        assert!(
            !schema_of(name).contains(&"scope".to_string()),
            "{name} advertises a `scope` parameter, which the session path has no selection to read"
        );
    }

    // The read-selection tools do carry `scope`, and both report which selection ran. Asserting the
    // positive half here is what stops a future "cleanup" from deleting the key that P1-2 depends on.
    for name in ["retrieve_memories", "recall"] {
        let keys = schema_of(name);
        assert!(
            keys.contains(&"scope".to_string()) && keys.contains(&"project".to_string()),
            "{name} lost the scope selection: {keys:?}"
        );
    }

    // And the prose must not claim more than the schema: no session description may name `scope` as a
    // parameter, while the tools that do have it must say so.
    for tool in tools.listed(&alice.bearer, &session).await {
        let name = tool["name"].as_str().unwrap_or_default();
        let described = tool["description"].as_str().unwrap_or_default().to_string();
        if matches!(name, "get_session" | "list_sessions") {
            // The token itself, not a particular phrasing: a session description may not name `scope` at
            // all, because a name an agent can read is a name it will send, and this schema drops it.
            // (A narrower substring check shipped in this lane's first draft of this test survived a
            // mutation that added "(`scope`/`project`)" — the mutation that check was written to catch.)
            assert!(
                !described.contains("`scope`"),
                "{name}'s description names a `scope` parameter it does not declare: {described}"
            );
            assert!(
                described.contains("project") && described.contains("view"),
                "{name}'s description does not name the two parameters that actually select: {described}"
            );
        }
    }
}

/// The session `project` selector end to end: naming a project narrows which of a session's memories the
/// caller sees, and naming none does not widen past personal. Storage tests cover the predicate; this is
/// the MCP wiring of the `Some(text) => One(Project)` arm, which nothing else exercised through transport.
#[tokio::test(flavor = "multi_thread")]
async fn the_session_project_selector_narrows_both_session_tools_through_the_transport() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-sessproj-alice").await;
    let session_id = "pi:projected";
    let alice_session = tools.handshake(&alice.bearer).await;
    let project = establish(&tools, &alice, &alice_session, github("anten/projected")).await;

    // One session, two partitions. `list_unassigned`-style fixtures are not usable here: the rows have to
    // arrive through `store_memory`, because the session edge is what the read traverses.
    for (content_text, scope) in [
        ("session note in personal", "personal"),
        ("session note in the project", "project"),
    ] {
        let arguments = if scope == "personal" {
            json!({"content": content_text, "scope": scope, "session_id": session_id})
        } else {
            json!({
                "content": content_text, "scope": scope, "session_id": session_id,
                "project": project["id"],
            })
        };
        let stored = tools
            .call(&alice.bearer, &alice_session, "store_memory", arguments)
            .await;
        assert!(!refused(&stored), "seed refused: {stored}");
    }
    let finalized = tools
        .call(
            &alice.bearer,
            &alice_session,
            "finalize_session",
            json!({"session_id": session_id, "summary": "two partitions, one session", "tags": ["gateway"]}),
        )
        .await;
    assert!(!refused(&finalized), "{finalized}");

    let ids_of_memories = |body: &Json| -> Vec<String> {
        body["memories"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|m| m["content"].as_str().map(str::to_string))
            .collect()
    };

    // `get_session` with the project named: only the project row.
    let with_project = tools
        .call(
            &alice.bearer,
            &alice_session,
            "get_session",
            json!({"session_id": session_id, "project": project["id"]}),
        )
        .await;
    assert!(!refused(&with_project), "{with_project}");
    assert_eq!(
        ids_of_memories(&content(&with_project)),
        vec!["session note in the project".to_string()],
        "a project-selected get_session did not narrow to that project: {with_project}"
    );

    // `get_session` with no project: personal only, so the same session id is not a skeleton key on the
    // project partition.
    let without = tools
        .call(
            &alice.bearer,
            &alice_session,
            "get_session",
            json!({"session_id": session_id}),
        )
        .await;
    assert_eq!(
        ids_of_memories(&content(&without)),
        vec!["session note in personal".to_string()],
        "naming a session id widened the read past personal: {without}"
    );

    // The count a session row reports is the population's count, and it agrees with the rows returned —
    // the two claims `list_sessions` and `get_session` make about one session cannot disagree.
    let listed = tools
        .call(
            &alice.bearer,
            &alice_session,
            "list_sessions",
            json!({"project": project["id"]}),
        )
        .await;
    assert!(!refused(&listed), "{listed}");
    let rows = content(&listed)["sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let row = rows
        .iter()
        .find(|r| r["external_id"].as_str() == Some(session_id))
        .unwrap_or_else(|| panic!("the session is missing from its owner's page: {listed}"));
    assert_eq!(
        row["memory_count"].as_i64(),
        Some(1),
        "the project page counted a memory the project selection cannot read: {row}"
    );

    // …and the unprojected page counts the personal one instead.
    let plain = tools
        .call(
            &alice.bearer,
            &alice_session,
            "list_sessions",
            json!({"limit": 50}),
        )
        .await;
    let plain_row = content(&plain)["sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .find(|r| r["external_id"].as_str() == Some(session_id))
        .expect("the session is on the caller's own page");
    assert_eq!(
        plain_row["memory_count"].as_i64(),
        Some(1),
        "the personal page's count is not the personal member: {plain_row}"
    );
    assert_eq!(content(&plain)["view"].as_str(), Some("scoped"), "{plain}");

    // P2-7: a scoped row does not *publish* the prose keys at all. A null is a claim ("this session has
    // no summary"); an absent key is the withheld answer, and the two session tools must agree.
    for key in ["summary", "tags"] {
        assert!(
            row.get(key).is_none() && plain_row.get(key).is_none(),
            "a scoped session row carried the {key} key: {row} / {plain_row}"
        );
    }
    let archive = tools
        .call(
            &alice.bearer,
            &alice_session,
            "list_sessions",
            json!({"view": "archive", "limit": 50}),
        )
        .await;
    let archive_row = content(&archive)["sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .find(|r| r["external_id"].as_str() == Some(session_id))
        .expect("the archive page lists the session");
    assert_eq!(
        archive_row["summary"].as_str(),
        Some("two partitions, one session"),
        "the archive page lost the owner's own prose: {archive_row}"
    );
    assert!(
        archive_row["tags"]
            .as_array()
            .map(|tags| tags.contains(&json!("gateway")))
            .unwrap_or(false),
        "the archive page lost the owner's own tags: {archive_row}"
    );
    assert_eq!(
        content(&archive)["view"].as_str(),
        Some("archive"),
        "{archive}"
    );
}

/// The review disclosure is client-visible copy that a UI renders verbatim, so its exact sentence is the
/// contract — not a substring an assertion happens to tolerate.
#[tokio::test(flavor = "multi_thread")]
async fn the_unassigned_disclosure_and_page_properties_are_the_published_text() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("t8-disclosure-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    tools
        .rows(
            "CREATE type::record('fact', 'ds-one') SET content = 'awaiting a classification', \
             confidence = 0.5, embedding = [1.0, 0.0], tags = [], deleted = false, account = $acc, \
             workspace = $ws, scope = 'unassigned' RETURN AFTER;",
            json!({"acc": alice.account, "ws": alice.workspace}),
        )
        .await;

    let page = tools
        .call(
            &alice.bearer,
            &session,
            "list_unassigned_memories",
            json!({}),
        )
        .await;
    assert!(!refused(&page), "{page}");
    let body = content(&page);
    // Wiring, not content: this response must be built from the one exported constant, so a client and a
    // reviewer cannot be handed different sentences. The sentence itself is pinned by the literal below —
    // comparing only against the constant would be tautological and would pass on a mutated copy.
    assert_eq!(
        body["disclosure"].as_str(),
        Some(alexandria_mcp::server::UNASSIGNED_REVIEW_DISCLOSURE),
        "the response did not carry the published sentence verbatim"
    );
    // The published sentence, spelled once here so the whitespace a line-continuation can leave behind is
    // visible in the diff rather than in a client's rendering.
    assert_eq!(
        body["disclosure"].as_str(),
        Some(
            "owner-review state only: unassigned memories are in no recall path and no background maintenance, and reading them here changes nothing"
        ),
        "the client-visible disclosure moved or carries stray spaces"
    );
    assert!(
        !body["disclosure"]
            .as_str()
            .unwrap_or_default()
            .contains("  "),
        "the disclosure carries a double space"
    );
    assert_eq!(body["review"].as_bool(), Some(true));
    assert_eq!(body["eligible_for_recall"].as_bool(), Some(false));

    // The page properties every scope listing publishes, on the default request: a caller can page without
    // guessing, and can tell a cap from a full read.
    for key in ["limit", "count", "truncated", "oversize", "next_cursor"] {
        assert!(
            body.get(key).is_some(),
            "the review page published no {key:?}: {body}"
        );
    }
    assert_eq!(body["limit"].as_i64(), Some(20), "{body}");
    assert_eq!(body["truncated"].as_bool(), Some(false), "{body}");
    assert_eq!(body["oversize"].as_bool(), Some(false), "{body}");
    assert!(
        body["next_cursor"].is_null(),
        "a page with one row and no more claimed a continuation: {body}"
    );
}
