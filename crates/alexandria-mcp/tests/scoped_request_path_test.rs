//! Task 6 (D4) — the hosted write target, and the cross-account tool surface.
//!
//! Owner decision D4 binds three rules, and every case here is one of them:
//!
//! 1. A hosted `store_memory` / `import_document` whose partition target cannot be derived from the
//!    public parameters that exist today returns `scope required` **before embedding and before any
//!    write**. The refusal is not a note attached to a completed store: an `embed` call is provider work
//!    and a `fact` row is durable state, and paying either to discover the request was unanswerable is
//!    the failure D4 exists to prevent.
//! 2. A **configured trusted-local stdio** principal may write `Personal`, because that account was
//!    established at startup rather than inferred from a bearer request.
//! 3. No fallback to Personal for hosted bearer traffic, and no Unassigned staging queue. An existing row
//!    whose owner and partition can be authorized may still be updated or deleted, because there the
//!    target is *derived from the row* rather than guessed.
//!
//! Every case drives the **real** [`AlexandriaServer`] through the real auth gate over the MCP transport,
//! so the principal, the lease and the request context are the transport's own. Nothing here hands a
//! handler a context it could not lawfully obtain, and there is no test-only authority constructor
//! anywhere in the path: owners come from `bootstrap_owner`, a hosted bearer from `mint_agent_token` plus
//! `authenticate_agent_token`, a trusted-local principal from `local_principal` under this store's claimed
//! startup capability, and every context from `AccessRepo::authorize`/`select`.
//!
//! These tests are red against the pre-D4 server on purpose: today a hosted `store_memory` happily embeds
//! and writes an unowned row, which is exactly the thing the decision forbids.

mod fixtures;

use alexandria_mcp::auth::http::admit;
use alexandria_mcp::auth::{
    Admission, AuthEvents, AuthMiddleware, AuthService, McpSessionRegistry,
};
use alexandria_mcp::server::{AlexandriaServer, HeatSettings};
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::access::{AccessRepo, SystemContext};
use alexandria_storage::models::{OperationId, PrincipalIds};
use alexandria_storage::repos::{AuthRepo, IdentityRepo};
use alexandria_storage::{Database, schema};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header::CONTENT_TYPE};
use chrono::Utc;
use fixtures::{MCP_PATH, read_body, with_bearer, with_session};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::{Value as Json, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tower::ServiceExt;

/// An [`EmbeddingProvider`] that counts `embed` calls.
///
/// The counter is what makes rule 1 checkable: "refused **before** embedding" cannot be observed from a
/// response alone, only from a provider that reports how often it was asked. The vector is constant and
/// two-dimensional, matching the store's other fixtures.
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
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![1.0f32, 0.0f32]).collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "task6-counting"
    }
}

/// One enrolled owner: the ids enrollment produced and the bearer a hosted request presents.
///
/// The context the bearer resolves to is deliberately *not* carried. Every case here asserts what the
/// transport derives from the bearer, so holding a second, test-built `OwnerContext` would let a case pass
/// on an authority the request itself never presented.
struct Owner {
    ids: PrincipalIds,
    bearer: String,
}

/// The real server behind the real gate, over one store shared by every account in the test.
struct Tools {
    db: Arc<Database>,
    router: Router,
    embed_calls: Arc<AtomicUsize>,
    system: SystemContext,
}

impl Tools {
    /// Builds the harness. `stdio` is carried exactly as `main.rs` carries it on the stdio arm: the
    /// transport may hold a configured local identity, and a hosted bearer request never consults it.
    async fn built(stdio: Option<alexandria_mcp::auth::StdioIdentity>) -> Self {
        let mut db = Database::connect_embedded().await.unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let system = db.claim_system_authority().unwrap();
        let db = Arc::new(db);

        let (embedding, embed_calls) = CountingEmbedding::shared();
        let mut server = AlexandriaServer::new(
            Arc::clone(&db),
            Arc::new(embedding),
            0.75,
            HeatSettings::default(),
        );
        if let Some(identity) = stdio {
            server = server.with_stdio_identity(identity);
        }

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

    /// Enrolls an owner and mints it an agent token.
    async fn owner(&self, login: &str) -> Owner {
        let ids = IdentityRepo::new(self.db.inner())
            .bootstrap_owner(
                login,
                &format!("$argon2id$fixture-prepared-hash-{login}-padding"),
                &OperationId::new(format!("task6-{login}")),
            )
            .await
            .unwrap();
        let access = AccessRepo::new(&self.db);
        let local = access
            .local_principal(&self.system, &ids.account)
            .await
            .unwrap();
        let bootstrap_context = access.authorize(&local).await.unwrap();
        let token = AuthRepo::new(self.db.inner())
            .mint_agent_token(&bootstrap_context, "task6-agent", Utc::now())
            .await
            .unwrap();
        let secret = token.secret.render();
        let bearer = secret.expose().to_string();
        // Authenticating the minted token is not decorative: it proves the bearer this fixture hands to
        // the transport really does resolve to a principal, so a refusal below is about the request path
        // and not about a credential that never worked in the first place.
        AuthRepo::new(self.db.inner())
            .authenticate_agent_token(&bearer, Utc::now())
            .await
            .unwrap();
        Owner { ids, bearer }
    }

    /// Opens a session for a bearer and returns its id.
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
                        "clientInfo": {"name": "task6-d4", "version": "0"}
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
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .expect("a session id");
        let _ = read_body(response.into_body()).await;
        // The initialized notification every real client sends before calling a tool.
        let notice = with_protocol(with_bearer(
            with_session(
                post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
                &session,
            ),
            bearer,
        ));
        let response = self.router.clone().oneshot(notice).await.expect("served");
        let _ = read_body(response.into_body()).await;
        session
    }

    /// Calls a tool with arguments over the transport and returns the result payload.
    async fn call(
        &self,
        bearer: &str,
        session: &str,
        name: &str,
        arguments: Json,
    ) -> (StatusCode, Json) {
        let request = with_protocol(with_bearer(
            with_session(post(tool_body(name, arguments)), session),
            bearer,
        ));
        let response = self.router.clone().oneshot(request).await.expect("served");
        let status = response.status();
        let bytes = read_body(response.into_body()).await;
        let payload = fixtures::frames_of(&bytes)
            .into_iter()
            .filter_map(|frame| frame.get("result").cloned())
            .next()
            .unwrap_or(Json::Null);
        (status, payload)
    }

    /// Rows of one table, as stored.
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

    async fn count(&self, sql: &str) -> i64 {
        self.rows(sql)
            .await
            .first()
            .and_then(|row| row.get("count"))
            .and_then(Json::as_i64)
            .unwrap_or(0)
    }

    async fn facts(&self) -> Vec<Json> {
        self.rows(
            "SELECT id, content, account, workspace, scope, project, deleted FROM fact ORDER BY content;",
        )
        .await
    }
}

/// `tools/call` with an argument object, where the Task 5 fixture's `tool_call` sends none.
fn tool_body(name: &str, arguments: Json) -> Json {
    json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    })
}

fn post(body: Json) -> axum::extract::Request<Body> {
    let mut builder = axum::http::Request::builder()
        .method("POST")
        .uri(MCP_PATH)
        .header(CONTENT_TYPE, "application/json")
        .header("accept", "application/json, text/event-stream");
    // `builder.insert` is the HeaderMap form; `header` above is the typed-key convenience. Either way
    // the headers are set before the body is attached, so the request is exactly what a real client sends.
    builder = builder.header("host", "127.0.0.1:3000");
    let request = builder.body(Body::from(body.to_string())).unwrap();
    let mut request: axum::extract::Request<Body> = request;
    request.headers_mut().insert(
        "mcp-protocol-version",
        HeaderValue::from_static("2025-06-18"),
    );
    request
}

/// The protocol-version header the legacy-session gate expects on every request after the handshake.
fn with_protocol(mut request: axum::extract::Request<Body>) -> axum::extract::Request<Body> {
    request.headers_mut().insert(
        "mcp-protocol-version",
        HeaderValue::from_static("2025-06-18"),
    );
    request
}

/// The text a tool result puts in front of a client, whether it came back structured or as content.
fn text_of(result: &Json) -> String {
    if let Some(message) = result
        .get("structuredContent")
        .and_then(|content| content.get("message"))
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
                .filter_map(|block| block.get("text").and_then(Json::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| result.to_string())
}

/// Whether the tool answered with a refusal rather than a result.
fn refused(result: &Json) -> bool {
    result["isError"].as_bool().unwrap_or(false)
        || result
            .get("structuredContent")
            .and_then(|content| content.get("status"))
            .and_then(Json::as_str)
            == Some("error")
}

// ---------------------------------------------------------------------------------------------
// D4 rule 1 — the refusal comes before the cost
// ---------------------------------------------------------------------------------------------

/// A hosted `store_memory` with no lawful partition target is refused before embedding and before any
/// write.
#[tokio::test(flavor = "multi_thread")]
async fn a_hosted_store_with_no_partition_target_is_refused_before_any_side_effect() {
    let tools = Tools::built(None).await;
    let owner = tools.owner("d4-store-alice").await;
    let session = tools.handshake(&owner.bearer).await;

    tools.embed_calls.store(0, Ordering::SeqCst);
    let (status, result) = tools
        .call(
            &owner.bearer,
            &session,
            "store_memory",
            json!({"content": "a durable fact from a hosted bearer"}),
        )
        .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "a refusal is a tool-level error, not a transport failure"
    );
    let message = text_of(&result);
    assert!(
        refused(&result),
        "an untargeted hosted store must refuse rather than succeed: {result}"
    );
    assert!(
        message.contains("scope required"),
        "the refusal must name the missing thing so a client can act on it, got: {message}"
    );
    // The ordering *is* the requirement, and it is only observable through the counter.
    assert_eq!(
        tools.embed_calls.load(Ordering::SeqCst),
        0,
        "the request reached the embedding provider before it was refused"
    );
    assert!(
        tools.facts().await.is_empty(),
        "a refused store still wrote a fact row: {:?}",
        tools.facts().await
    );
    assert_eq!(
        tools
            .count("SELECT count() FROM `session` GROUP ALL;")
            .await,
        0,
        "a refused store still created a session"
    );
    assert_eq!(
        tools
            .count("SELECT count() FROM heat_state GROUP ALL;")
            .await,
        0,
        "a refused store still created heat state"
    );
    assert_eq!(
        tools
            .count("SELECT count() FROM provenance GROUP ALL;")
            .await,
        0,
        "a refused store still wrote a provenance row"
    );
}

/// A session handle is not a partition target.
///
/// `store_memory(session_id)` is the shape the pi skill sends, so this is the case most likely to be
/// "helpfully" inferred. D4 forbids the inference: a handle groups a conversation, it does not name a
/// `(workspace, scope)` partition.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_id_is_not_a_partition_target() {
    let tools = Tools::built(None).await;
    let owner = tools.owner("d4-session-alice").await;
    let session = tools.handshake(&owner.bearer).await;

    tools.embed_calls.store(0, Ordering::SeqCst);
    let (_status, result) = tools
        .call(
            &owner.bearer,
            &session,
            "store_memory",
            json!({"content": "a fact with a session handle but no partition",
                   "session_id": "pi:scope-session",
                   "agent_id": "pi",
                   "tags": ["d4"]}),
        )
        .await;
    assert!(
        refused(&result) && text_of(&result).contains("scope required"),
        "a session handle was accepted as a partition target: {result}"
    );
    assert_eq!(
        tools.embed_calls.load(Ordering::SeqCst),
        0,
        "the provider ran before the refusal"
    );
    assert!(tools.facts().await.is_empty());
    assert_eq!(
        tools
            .count("SELECT count() FROM `session` GROUP ALL;")
            .await,
        0
    );
}

/// `import_document` is the more expensive refusal: untargeted it would embed a whole batch, write a
/// `raw` document and link chunk rows to it.
#[tokio::test(flavor = "multi_thread")]
async fn a_hosted_import_with_no_partition_target_is_refused_before_batch_embedding() {
    let tools = Tools::built(None).await;
    let owner = tools.owner("d4-import-alice").await;
    let session = tools.handshake(&owner.bearer).await;

    tools.embed_calls.store(0, Ordering::SeqCst);
    let (_status, result) = tools
        .call(
            &owner.bearer,
            &session,
            "import_document",
            json!({"content": "# Heading\n\nReference material worth keeping for later.",
                   "mode": "chunk"}),
        )
        .await;
    assert!(
        refused(&result) && text_of(&result).contains("scope required"),
        "import_document must refuse on the same rule as store_memory, got: {result}"
    );
    assert_eq!(
        tools.embed_calls.load(Ordering::SeqCst),
        0,
        "an import embedded chunks before discovering it had no target"
    );
    assert!(
        tools.facts().await.is_empty(),
        "a refused import wrote chunk rows: {:?}",
        tools.facts().await
    );
    assert_eq!(
        tools.count("SELECT count() FROM raw GROUP ALL;").await,
        0,
        "a refused import wrote a `raw` document row"
    );
    assert_eq!(
        tools
            .count("SELECT count() FROM memory_edge GROUP ALL;")
            .await,
        0,
        "a refused import wrote lineage edges"
    );
}

/// The refusal is a tool error inside a live session, not a closed session: the same credential can still
/// read afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn refusing_an_untargeted_store_leaves_the_session_usable() {
    let tools = Tools::built(None).await;
    let owner = tools.owner("d4-reuse-alice").await;
    let session = tools.handshake(&owner.bearer).await;

    let (_status, refused_result) = tools
        .call(
            &owner.bearer,
            &session,
            "store_memory",
            json!({"content": "no target"}),
        )
        .await;
    assert!(refused(&refused_result));

    let (status, read) = tools
        .call(
            &owner.bearer,
            &session,
            "list_sessions",
            json!({"limit": 5}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the session survived the refusal");
    assert!(!read.is_null(), "a refusal poisoned the session: {read}");
}

// ---------------------------------------------------------------------------------------------
// D4 rule 2 — trusted-local stdio may write Personal
// ---------------------------------------------------------------------------------------------
//
// Not reachable over this transport: the HTTP gate always attaches an admitted *hosted* principal, and a
// hosted principal wins over the configured stdio identity, so a request here can never become
// trusted-local — which is correct, and is why rule 1 above is the hosted answer. The positive half of
// rule 2 is therefore asserted at the seam the handler calls, in crate:
// `server::get_info_tests::a_trusted_local_store_is_retrievable_by_its_own_reader` (fact, session,
// cluster and provenance all stamped, and the author's own scoped retrieval finds the row), with
// `a_stored_memory_never_reaches_a_foreign_cluster` for the cross-account half rule 2 also governs.

// ---------------------------------------------------------------------------------------------
// D4 rule 3 — no cross-account leak, and a refusal that identifies nothing
// ---------------------------------------------------------------------------------------------

/// Seeded through storage so the fixture does not depend on the write path under test, every hosted read
/// tool must show only the caller's own corpus.
///
/// This is the §195 leak matrix's request-facing half: the tools a client actually calls, not the
/// repository methods underneath them.
#[tokio::test(flavor = "multi_thread")]
async fn hosted_reads_never_surface_another_accounts_corpus() {
    let tools = Tools::built(None).await;
    let alice = tools.owner("leak-alice").await;
    let bob = tools.owner("leak-bob").await;

    // One Personal fact per account, written directly, each carrying a distinctive tag.
    for owner in [&alice, &bob] {
        let mut created = tools
            .db
            .inner()
            .query(
                "CREATE fact SET content = $content, confidence = 0.5, embedding = [1.0, 0.0], \
                 tags = $tags, deleted = false, account = $account, workspace = $workspace, \
                 scope = 'personal';",
            )
            .bind((
                "content",
                format!("the private note of {}", owner.ids.login_label()),
            ))
            .bind(("tags", vec![format!("tag-{}", owner.ids.login_label())]))
            .bind(("account", owner.ids.account.as_record_id().clone()))
            .bind(("workspace", owner.ids.workspace.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Json> = created.take(0).unwrap();
        let fact = rows[0]["id"].clone();
        // A heat row, so a retrieval that records an access has something to write. The fact handle is
        // bound as the parsed `RecordId` the engine returned, not re-rendered as text.
        tools
            .db
            .inner()
            .query("CREATE heat_state SET memory = $fact, heat = 1.0, stability = 1.0, access_count = 0;")
            .bind((
                "fact",
                surrealdb::types::RecordId::parse_simple(fact.as_str().unwrap()).unwrap(),
            ))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    for (caller, other) in [(&alice, &bob), (&bob, &alice)] {
        let session = tools.handshake(&caller.bearer).await;
        let (_status, result) = tools
            .call(
                &caller.bearer,
                &session,
                "retrieve_memories",
                json!({"query": "the private note", "limit": 10}),
            )
            .await;
        let rendered = result.to_string();
        assert!(
            !rendered.contains(&format!("tag-{}", other.ids.login_label())),
            "{}'s retrieval surfaced {}'s tag",
            caller.ids.login_label(),
            other.ids.login_label()
        );

        let (_status, listed) = tools
            .call(&caller.bearer, &session, "list_sessions", json!({}))
            .await;
        assert!(
            !listed.to_string().contains(&other.ids.login_label()),
            "{}'s session list named {}'s corpus",
            caller.ids.login_label(),
            other.ids.login_label()
        );

        // A reminder scoped to the other account's project must not be delivered or listed either.
        let (_status, reminders) = tools
            .call(&caller.bearer, &session, "list_reminders", json!({}))
            .await;
        assert!(
            !reminders.to_string().contains(&other.ids.login_label()),
            "{}'s reminder list reached {}'s rows",
            caller.ids.login_label(),
            other.ids.login_label()
        );
    }
}

/// A foreign id and a missing id answer identically.
///
/// The tool response is the outward surface, so "is this id somebody else's or nobody's?" must not be
/// answerable from it. Both callers get the same bytes for both cases.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_id_and_a_missing_id_are_indistinguishable() {
    let tools = Tools::built(None).await;
    let alice = tools.owner("edge-alice").await;
    let bob = tools.owner("edge-bob").await;

    // A fact that is Bob's, and an id that belongs to nobody.
    let mut created = tools
        .db
        .inner()
        .query(
            "CREATE fact SET content = 'bob only', confidence = 0.5, embedding = [1.0, 0.0], \
             tags = [], deleted = false, account = $account, workspace = $workspace, \
             scope = 'personal';",
        )
        .bind(("account", bob.ids.account.as_record_id().clone()))
        .bind(("workspace", bob.ids.workspace.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<Json> = created.take(0).unwrap();
    let foreign = rows[0]["id"].as_str().unwrap().to_string();

    for name in ["delete_memory", "update_memory"] {
        let alice_session = tools.handshake(&alice.bearer).await;
        let (_status, on_foreign) = tools
            .call(
                &alice.bearer,
                &alice_session,
                name,
                json!({"id": foreign, "content": "not yours"}),
            )
            .await;
        let (_status, on_missing) = tools
            .call(
                &alice.bearer,
                &alice_session,
                name,
                json!({"id": "fact:never-existed-anywhere", "content": "not yours"}),
            )
            .await;
        assert_eq!(
            text_of(&on_foreign),
            text_of(&on_missing),
            "{name} answers differently for a foreign id than for an absent one: foreign={} missing={}",
            text_of(&on_foreign),
            text_of(&on_missing)
        );
        assert_eq!(
            refused(&on_foreign),
            refused(&on_missing),
            "{name} differs in refusal *shape* between foreign and missing ids"
        );
    }

    // And neither write happened.
    assert_eq!(
        tools.facts().await.len(),
        1,
        "a cross-account write changed the corpus"
    );
}

// ---------------------------------------------------------------------------------------------
// The transactional guard, still green through the new path
// ---------------------------------------------------------------------------------------------

/// A login label for assertions, derived from the typed handle rather than stored twice.
trait LoginLabel {
    fn login_label(&self) -> String;
}

impl LoginLabel for PrincipalIds {
    fn login_label(&self) -> String {
        alexandria_storage::record_id_to_string(self.account.as_record_id())
            .split_once(':')
            .map(|(_, key)| key.to_string())
            .unwrap_or_default()
    }
}
