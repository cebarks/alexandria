//! Task 7: `recall` answers as *somebody's own clusters*, and the handle it hands back is a
//! **locator plus a version** — never authority, never a snapshot that can be pretended.
//!
//! Three claims, each with a case that can fail:
//!
//! * **Population.** Broad recall lists only the caller's clusters, and a focused replay resolves the
//!   named cluster under the caller's *current* context before any member is read. A `scope_handle` is
//!   an unauthenticated string a client echoes; if the server trusted what it named, a handle that was
//!   logged, guessed or handed over would read somebody else's memories.
//! * **The handle's own contract.** It carries the cluster it locates **and the version that cluster
//!   was at when the handle was issued**: the row's stored `membership_revision` (the guard v009
//!   reserves for exactly this) plus the live member count the caller's own read saw. It carries no
//!   account, no credential, no token — none of that may travel in a string the client holds.
//! * **Response compatibility.** Both arms keep the keys clients already parse. The version travels
//!   inside the opaque handle, so the narrowing loop needs no new public field, and nothing internal
//!   (decoder text, SQL, workspace/account handles, another account's counts) ever appears.
//!
//! Stale behaviour, decided from source and pinned below: **a replay always answers with current
//! rows.** The handle's version is verified, never trusted — nothing is served *from* it — so a
//! cluster that moved since issuance is served as it now is. Rejecting instead would turn the
//! documented two-call progressive loop into an error whenever a store joined the cluster in between.
//! What the version does make impossible is the opposite claim: the server re-reads the row's guard
//! and the live membership on every replay, so it cannot answer from a snapshot it merely assumes is
//! unchanged.
//!
//! Everything runs over the real auth gate and the real rmcp transport against the real
//! [`AlexandriaServer`]: owners from `bootstrap_owner` → `local_principal` → `mint_agent_token` →
//! `authenticate_agent_token`, clusters from `ScopedClusterRepo::create` — the seam `store_memory`
//! uses — members linked by `contains_memory`. No test-only authority constructor is used because
//! none may exist, and `rmcp`'s `Peer::new` is `pub(crate)`, so no handler could be handed a
//! fabricated request context anyway.

mod fixtures;

use alexandria_mcp::auth::mcp_transport::AuthEvents;
use alexandria_mcp::auth::{Admission, AuthMiddleware, AuthService, McpSessionRegistry};
use alexandria_mcp::server::{AlexandriaServer, HeatSettings};
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::access::{AccessRepo, OwnerContext, ScopedContext, SystemContext};
use alexandria_storage::models::{OperationId, ReadSelection};
use alexandria_storage::repos::{AuthRepo, IdentityRepo, ScopedClusterRepo};
use alexandria_storage::{Database, record_id_to_string, schema};
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
use std::time::Duration;
use tower::ServiceExt;

/// A provider whose vectors all point the same way.
///
/// `recall`'s gate is centroid similarity ≥ `min_similarity` (0.75 here) and member ranking is exact
/// cosine, so a constant unit vector keeps every seeded cluster a real candidate. That matters: a
/// fixture whose candidate list is empty makes every "no foreign cluster" assertion pass vacuously,
/// which is exactly how Task 6's unowned cluster create hid (review P1-3).
struct FlatEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for FlatEmbedding {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![1.0f32, 0.0f32]).collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "task7-recall"
    }
}

/// One enrolled owner: the handles enrollment produced, and the bearer a hosted request presents.
struct Owner {
    account: String,
    workspace: String,
    bearer: String,
}

/// The real server behind the real gate, over one store holding every account a case names.
struct Recall {
    db: Arc<Database>,
    router: Router,
    system: SystemContext,
}

impl Recall {
    async fn hosted() -> Self {
        let mut db = Database::connect_embedded().await.unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let system = db.claim_system_authority().unwrap();
        let db = Arc::new(db);

        let server = AlexandriaServer::new(
            Arc::clone(&db),
            Arc::new(FlatEmbedding),
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

    /// Enrolls an owner with a live agent token, through the sealed chain and nothing else.
    async fn owner(&self, login: &str) -> Owner {
        let ids = IdentityRepo::new(self.db.inner())
            .bootstrap_owner(
                login,
                &format!("$argon2id$fixture-prepared-hash-{login}-padding"),
                &OperationId::new(format!("task7-recall-{login}")),
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
            .mint_agent_token(&context, "task7-recall-agent", Utc::now())
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

        Owner {
            account: record_id_to_string(ids.account.as_record_id()),
            workspace,
            bearer,
        }
    }

    /// The `OwnerContext` + `ScopedContext` pair a request of this owner's is handed, produced by the
    /// same sealed chain: authenticate the bearer, authorize the principal it yields, select the
    /// automatic population. Crate-private producers only — no test-only authority constructor.
    async fn contexts(&self, owner: &Owner) -> (OwnerContext, ScopedContext) {
        let access = AccessRepo::new(&self.db);
        let principal = AuthRepo::new(self.db.inner())
            .authenticate_agent_token(&owner.bearer, Utc::now())
            .await
            .unwrap();
        let owner_context = access.authorize(&principal).await.unwrap();
        let read = access
            .select(&owner_context, ReadSelection::Auto(None))
            .await
            .unwrap();
        (owner_context, read)
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
                        "clientInfo": {"name": "task7-recall", "version": "0"}
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

    /// Calls `recall` and returns the JSON-RPC result frame.
    ///
    /// A panic here carries the raw status and body: a transport-level non-answer must not be
    /// mis-read as "the tool refused", which is how a leak looks identical to a refusal.
    async fn call(&self, owner: &Owner, session: &str, arguments: Json) -> Json {
        let request = with_protocol(with_bearer(
            with_session(
                router_post(json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": "recall", "arguments": arguments},
                })),
                session,
            ),
            &owner.bearer,
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
                    "recall: no JSON-RPC result frame; status {status}, body {}",
                    String::from_utf8_lossy(&bytes)
                )
            })
    }

    /// One broad pass for this owner, as the tool answered it.
    async fn broad(&self, owner: &Owner, session: &str, query: &str) -> Json {
        self.call(
            owner,
            session,
            json!({"query": query, "scope_handle": null}),
        )
        .await
    }

    /// One replay of `handle` for this owner, as the tool answered it.
    async fn replay(&self, owner: &Owner, session: &str, query: &str, handle: &str) -> Json {
        self.call(
            owner,
            session,
            json!({"query": query, "scope_handle": handle}),
        )
        .await
    }

    /// The advertised tool surface, as the transport lists it.
    ///
    /// `tools/list` is what an LLM client actually sees, so this is the surface a new knob would
    /// leak through: the assertion below reads it rather than trusting a struct definition.
    async fn listed_tools(&self, owner: &Owner, session: &str) -> Vec<Json> {
        let request = with_protocol(with_bearer(
            with_session(
                router_post(json!({
                    "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {},
                })),
                session,
            ),
            &owner.bearer,
        ));
        let response = self.router.clone().oneshot(request).await.expect("served");
        let bytes = read_body(response.into_body()).await;
        fixtures::frames_of(&bytes)
            .into_iter()
            .filter_map(|frame| frame["result"]["tools"].as_array().cloned())
            .next()
            .unwrap_or_default()
    }

    /// Seeds one live, owned, Personal fact and returns its handle.
    ///
    /// Ownership columns are written explicitly because the request-facing store is refused for hosted
    /// bearers until Task 8 (owner decision D4); the stamp is the same one `store_memory` writes.
    async fn seed_fact(&self, key: &str, content: &str, owner: &Owner) -> String {
        let mut response = self
            .db
            .inner()
            .query(
                "CREATE type::record('fact', $key) SET content = $content, confidence = 0.5, \
                 embedding = [1.0, 0.0], tags = [], deleted = false, account = $account, \
                 workspace = $workspace, scope = 'personal' RETURN AFTER;",
            )
            .bind(("key", key.to_string()))
            .bind(("content", content.to_string()))
            .bind(("account", record(&owner.account)))
            .bind(("workspace", record(&owner.workspace)))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Json> = response.take(0).unwrap();
        rows[0]["id"].as_str().unwrap().to_string()
    }

    /// Seeds an owned, labelled cluster **through the seam production uses**, and returns its handle.
    async fn seed_cluster(&self, label: &str, owner: &Owner) -> String {
        let (owner_context, read) = self.contexts(owner).await;
        let created = ScopedClusterRepo::new(self.db.inner(), &owner_context, &read)
            .create(Some(label), &[1.0, 0.0], Utc::now())
            .await
            .unwrap();
        record_id_to_string(created.id.as_ref().expect("a created cluster has an id"))
    }

    async fn link_member(&self, cluster: &str, fact: &str) {
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

    /// The stored `membership_revision` on one of the owner's own clusters.
    async fn stored_revision(&self, _owner: &Owner, cluster: &str) -> i64 {
        let mut response = self
            .db
            .inner()
            .query(
                "SELECT membership_revision FROM type::record('cluster', $key) WHERE id = \
                     $id LIMIT 1;",
            )
            .bind(("key", key_of(cluster)))
            .bind(("id", record(cluster)))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Json> = response.take(0).unwrap();
        assert!(
            !rows.is_empty(),
            "the cluster row this file seeded cannot be read back: {rows:?}"
        );
        rows[0]["membership_revision"].as_i64().unwrap_or(-1)
    }

    /// Increments the stored `membership_revision` guard on one cluster.
    ///
    /// **This state is not reachable through any request-facing writer yet.** v009 defines the guard,
    /// `ScopedClusterRepo::create` leaves it at the schema `DEFAULT 0`, and the writer that increments
    /// it inside its own transaction is Task 12's cluster maintenance. A case that wants to show the
    /// handle carries the *stored* guard — rather than a number that can never move — has to move the
    /// column by hand, which is what this does. The member-count half of the version needs no such
    /// help: linking a member moves it through the production relation.
    async fn bump_membership_revision(&self, cluster: &str) {
        self.db
            .inner()
            .query(
                "UPDATE type::record('cluster', $key) SET membership_revision = \
                    membership_revision + 1;",
            )
            .bind(("key", key_of(cluster)))
            .await
            .unwrap()
            .check()
            .unwrap();
    }
}

fn record(text: &str) -> surrealdb::types::RecordId {
    surrealdb::types::RecordId::parse_simple(text).expect("a handle this file produced parses")
}

/// The opaque key of a `table:key` handle, for `type::record('table', $key)`.
fn key_of(text: &str) -> String {
    let (_, key) = text.split_once(':').expect("a record handle has a table");
    key.trim_matches('`').to_string()
}

fn with_protocol(request: axum::extract::Request<Body>) -> axum::extract::Request<Body> {
    let mut request = request;
    request.headers_mut().insert(
        "mcp-protocol-version",
        HeaderValue::from_static("2025-06-18"),
    );
    request
}

/// The `structuredContent` the tool answered with.
fn content(result: &Json) -> Json {
    result["structuredContent"].clone()
}

/// Everything a client can read off an answer, as one string: the structured payload *and* the text
/// blocks, because `CallToolResult::structured` puts the same JSON in a text block for clients that
/// only read `content[].text`.
fn text_of(result: &Json) -> String {
    let blocks = result
        .get("content")
        .and_then(Json::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Json::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    format!("{blocks} {result}")
}

/// The handle a broad pass issued for its first candidate cluster.
fn issued_handle(result: &Json) -> String {
    content(result)["clusters"][0]["scope_handle"]
        .as_str()
        .unwrap_or_else(|| panic!("broad recall issued no handle: {}", text_of(result)))
        .to_string()
}

/// Decodes the opaque handle back to the JSON the server wrote into it.
///
/// The handle is base64url without padding, and this suite decodes it by hand rather than through
/// `ScopeHandle`/`RecallHandle`: **the payload's own shape is the contract under test**, so the case
/// must keep compiling — and keep failing for the right reason — on both sides of the change that
/// replaces the type. A case only the new type could satisfy would be a compile error, not a red run.
fn decode_handle(handle: &str) -> Json {
    let text = String::from_utf8(base64url_decode(handle)).expect("a utf-8 handle payload");
    serde_json::from_str(&text).expect("the handle payload is the JSON the server encoded")
}

/// base64url (no padding) — the alphabet the handle encoder uses, both directions.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut group = [0u8; 3];
        group[..chunk.len()].copy_from_slice(chunk);
        let bits = (group[0] as u32) << 16 | (group[1] as u32) << 8 | (group[2] as u32);
        for index in 0..(chunk.len() + 1) {
            out.push(
                BASE64_ALPHABET[((bits >> (18 - 6 * index as u32)) & 0b11_1111) as usize] as char,
            );
        }
    }
    out
}

fn base64url_decode(text: &str) -> Vec<u8> {
    let mut bits: u32 = 0;
    let mut filled = 0u32;
    let mut bytes = Vec::with_capacity(text.len());
    for byte in text.as_bytes() {
        if *byte == b'=' {
            continue;
        }
        let value = BASE64_ALPHABET
            .iter()
            .position(|slot| slot == byte)
            .unwrap_or_else(|| panic!("not a base64url character in {text}"))
            as u32;
        bits = (bits << 6) | value;
        filled += 6;
        if filled >= 8 {
            filled -= 8;
            bytes.push((bits >> filled) as u8);
        }
    }
    bytes
}

/// The sorted key set of a JSON object, for "the published shape did not move" assertions.
fn keys(value: &Json) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

// ---------------------------------------------------------------------------------------------
// Population: broad recall answers the caller's clusters and nobody else's
// ---------------------------------------------------------------------------------------------

/// Broad recall answers the caller's own cluster with its own content, and nothing of a neighbour's.
///
/// The positive half is the one that matters: absence-only assertions are how an unowned cluster
/// create stayed invisible for four lanes — every candidate list was empty for everybody, so "no
/// foreign label" passed vacuously. Here alice *and* bob each hold a cluster and a member, so
/// "alice gets exactly alice's" is a statement about a filtered population.
#[tokio::test(flavor = "multi_thread")]
async fn broad_recall_answers_the_callers_own_cluster_and_nothing_of_a_foreign_one() {
    let tools = Recall::hosted().await;
    let alice = tools.owner("t7-rec-alice").await;
    let bob = tools.owner("t7-rec-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;

    let alice_fact = tools
        .seed_fact(
            "t7-rec-alice-fact",
            "alice rotates the api gateway keys",
            &alice,
        )
        .await;
    let alice_cluster = tools.seed_cluster("alice-gateway-history", &alice).await;
    tools.link_member(&alice_cluster, &alice_fact).await;

    let bob_fact = tools
        .seed_fact(
            "t7-rec-bob-fact",
            "bob ships the checkout service nightly",
            &bob,
        )
        .await;
    let bob_cluster = tools.seed_cluster("bob-checkout-history", &bob).await;
    tools.link_member(&bob_cluster, &bob_fact).await;

    let mine = tools
        .broad(&alice, &alice_session, "gateway key rotation")
        .await;
    let clusters = content(&mine)["clusters"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        clusters.len(),
        1,
        "alice's broad recall did not answer exactly her own one cluster: {}",
        text_of(&mine)
    );
    assert_eq!(
        clusters[0]["cluster_id"].as_str(),
        Some(alice_cluster.as_str()),
        "alice's candidate list named somebody else's cluster: {clusters:?}"
    );
    let rendered = text_of(&mine);
    assert!(
        rendered.contains("alice rotates the api gateway keys"),
        "the owner's own representative memory was missing: {rendered}"
    );
    assert!(
        !rendered.contains("bob-checkout-history")
            && !rendered.contains("bob ships the checkout service nightly"),
        "broad recall leaked bob's label or content to alice: {rendered}"
    );

    // The mirror image for bob, over the same store and the same query.
    let theirs = tools
        .broad(&bob, &bob_session, "gateway key rotation")
        .await;
    let theirs_clusters = content(&theirs)["clusters"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        theirs_clusters.len(),
        1,
        "bob lost his own cluster: {}",
        text_of(&theirs)
    );
    assert_eq!(
        theirs_clusters[0]["cluster_id"].as_str(),
        Some(bob_cluster.as_str())
    );
}

// ---------------------------------------------------------------------------------------------
// The handle: a locator *and* a version, never authority
// ---------------------------------------------------------------------------------------------

/// The issued handle carries the cluster's version, not just its name.
///
/// v009 gives `cluster` three revision guards and `ScopedClusterRepo::create` leaves them at the
/// schema default, so the stored guard is the number a replay has to be able to *notice moving*; the
/// live member count is the half that moves on today's paths, because the store path links a member
/// without touching the guard (that writer is Task 12's). A handle carrying neither would let a bare
/// locator imply a snapshot it never read.
#[tokio::test(flavor = "multi_thread")]
async fn the_issued_handle_is_a_locator_and_a_version_not_an_authority_or_a_clock() {
    let tools = Recall::hosted().await;
    let alice = tools.owner("t7-ver-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let fact = tools
        .seed_fact("t7-ver-fact", "alice versioned cluster note", &alice)
        .await;
    let cluster = tools.seed_cluster("alice-versioned", &alice).await;
    tools.link_member(&cluster, &fact).await;

    let broad = tools
        .broad(&alice, &session, "versioned cluster note")
        .await;
    let payload = decode_handle(&issued_handle(&broad));

    assert_eq!(
        payload["cluster_id"].as_str(),
        Some(cluster.as_str()),
        "the handle stopped naming the cluster it locates: {payload}"
    );
    assert_eq!(
        payload["version"]["members"].as_u64(),
        Some(1),
        "the handle carries no live member count, so a bare locator is presented as a snapshot: \
         {payload}"
    );
    assert_eq!(
        payload["version"]["membership_revision"].as_u64(),
        Some(tools.stored_revision(&alice, &cluster).await as u64),
        "the handle does not carry the cluster row's stored membership guard: {payload}"
    );

    // Nothing that could act as authority, or be mistaken for a token, travels in it.
    let rendered = payload.to_string();
    for forbidden in [
        "account",
        "credential",
        "token",
        "secret",
        "bearer",
        "epoch",
        "workspace",
        "scope_projects",
        "signature",
        "issued_at",
        "sql",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "the handle payload names {forbidden:?}, which is neither a locator nor a version: \
             {rendered}"
        );
    }
}

/// A link moves the handle's version; the passage of time does not.
///
/// Both halves are the discrimination. A handle stamped with a wall clock changes between two
/// identical calls, which proves nothing about the cluster; a handle that ignores membership changes
/// is a locator pretending to be a snapshot. Linking a real member must change the handle, and two
/// passes over an unchanged cluster must not.
#[tokio::test(flavor = "multi_thread")]
async fn the_handle_version_moves_with_the_cluster_and_only_with_the_cluster() {
    let tools = Recall::hosted().await;
    let alice = tools.owner("t7-vmv-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let first = tools
        .seed_fact("t7-vmv-fact-one", "alice measured handle note", &alice)
        .await;
    let cluster = tools.seed_cluster("alice-measured", &alice).await;
    tools.link_member(&cluster, &first).await;

    let unchanged_a = issued_handle(&tools.broad(&alice, &session, "measured handle note").await);
    let unchanged_b = issued_handle(&tools.broad(&alice, &session, "measured handle note").await);
    assert_eq!(
        unchanged_a, unchanged_b,
        "two broad passes over an unchanged cluster issued different handles, so the handle carries \
         something that is not the cluster's state"
    );

    let second = tools
        .seed_fact("t7-vmv-fact-two", "alice second measured note", &alice)
        .await;
    tools.link_member(&cluster, &second).await;
    let moved = issued_handle(&tools.broad(&alice, &session, "measured handle note").await);
    assert_ne!(
        unchanged_a, moved,
        "the cluster's membership moved and the handle did not, so the handle carries no version"
    );
}

/// The stored guard moves the handle too, and a replay of the older handle still answers current rows.
///
/// This is the lane's staleness decision, recorded where a reader can argue with it: **serving
/// current data**, never a rejection. What the guard half of the version buys is the proof that the
/// handle names a *versioned* state rather than a live pointer the server might otherwise trust —
/// and the proof survives because the guard is read from the row, not recomputed.
#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_handle_answers_with_current_rows_after_the_cluster_moves() {
    let tools = Recall::hosted().await;
    let alice = tools.owner("t7-cur-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let original = tools
        .seed_fact("t7-cur-fact-one", "alice original member text", &alice)
        .await;
    let cluster = tools.seed_cluster("alice-current", &alice).await;
    tools.link_member(&cluster, &original).await;

    let broad = tools.broad(&alice, &session, "original member text").await;
    let handle = issued_handle(&broad);

    let later = tools
        .seed_fact("t7-cur-fact-two", "alice later member text", &alice)
        .await;
    tools.link_member(&cluster, &later).await;
    tools.bump_membership_revision(&cluster).await;

    let focused = tools
        .replay(&alice, &session, "alice member text", &handle)
        .await;
    let rendered = text_of(&focused);
    assert_eq!(content(&focused)["mode"].as_str(), Some("focused"));
    assert!(
        rendered.contains("alice later member text"),
        "a handle issued before the cluster moved was answered as the snapshot it named instead of \
         the cluster as it now is: {rendered}"
    );
    assert!(
        rendered.contains("alice original member text"),
        "narrowing lost the member the handle was issued against: {rendered}"
    );
    assert_eq!(
        content(&focused)["memories"]
            .as_array()
            .map(|m| m.len())
            .unwrap_or(0),
        2,
        "the focused pass did not read the cluster's current membership: {rendered}"
    );

    // The version the *cluster* is at now is what a fresh handle carries: the guard was read, not
    // assumed, and it moved from the 0 a created row starts at.
    let fresh = decode_handle(&issued_handle(
        &tools.broad(&alice, &session, "member text").await,
    ));
    assert_eq!(
        fresh["version"]["membership_revision"].as_u64(),
        Some(tools.stored_revision(&alice, &cluster).await as u64),
        "the reissued handle does not report the guard the row now carries: {fresh}"
    );
    assert_ne!(
        fresh["version"]["membership_revision"].as_u64(),
        Some(0),
        "the fixture never moved the stored guard, so this case would prove nothing about it"
    );
}

/// A hand-written handle with a forged version reads no more, and no less, than an issued one.
///
/// The handle is unsigned, by design: it must not be authority. So a caller that invents a version
/// must be answered from the live cluster exactly as its own issued handle would be — which is what
/// makes the version a disclosure rather than a credential.
#[tokio::test(flavor = "multi_thread")]
async fn a_forged_handle_version_grants_nothing_and_hides_nothing() {
    let tools = Recall::hosted().await;
    let alice = tools.owner("t7-for-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let fact = tools
        .seed_fact("t7-for-fact", "alice forged version member", &alice)
        .await;
    let cluster = tools.seed_cluster("alice-forged", &alice).await;
    tools.link_member(&cluster, &fact).await;

    let issued = issued_handle(&tools.broad(&alice, &session, "forged version member").await);
    let from_server = tools
        .replay(&alice, &session, "forged version member", &issued)
        .await;

    let forged = base64url_encode(
        json!({"cluster_id": cluster, "version": {"membership_revision": 4_242, "members": 99}})
            .to_string()
            .as_bytes(),
    );
    let answer = tools
        .replay(&alice, &session, "forged version member", &forged)
        .await;

    assert_eq!(
        content(&answer),
        content(&from_server),
        "a hand-written version changed what the replay read, so the handle is doing the \
         authorising, not the caller's context"
    );
}

/// A handle issued before the version field existed still replays, and still grants nothing.
///
/// Deployed clients hold handles across an upgrade, and the narrowing loop is two calls inside one
/// conversation. Refusing a pre-version handle would be a public break of exactly the kind the brief
/// says not to invent, for no safety gain: the handle was never authority. So an absent `version`
/// decodes to the starting stamp — "nothing was read" — and the replay is answered from the caller's
/// live cluster, byte-identical to what a freshly issued handle gets. That equality is the whole
/// claim: if the absent stamp were read as a version-zero snapshot, or as a reason to refuse, this
/// fails, and the two-call loop a client is mid-way through breaks the moment the server restarts.
#[tokio::test(flavor = "multi_thread")]
async fn a_handle_issued_before_the_version_field_still_narrows_the_live_cluster() {
    let tools = Recall::hosted().await;
    let alice = tools.owner("t7-leg-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let fact = tools
        .seed_fact("t7-leg-fact", "alice legacy handle member", &alice)
        .await;
    let cluster = tools.seed_cluster("alice-legacy", &alice).await;
    tools.link_member(&cluster, &fact).await;

    let issued = issued_handle(&tools.broad(&alice, &session, "legacy handle member").await);

    // The exact pre-Task-7 wire shape: a locator, a `depth` that was always zero, and a wall clock.
    let legacy = base64url_encode(
        json!({ "cluster_id": cluster, "depth": 0, "issued_at": 1_792_622_489 })
            .to_string()
            .as_bytes(),
    );
    let replayed = tools
        .replay(&alice, &session, "legacy handle member", &legacy)
        .await;

    assert_eq!(
        content(&replayed)["mode"].as_str(),
        Some("focused"),
        "a handle minted by the previous server was refused, which breaks an in-flight narrowing loop: \
         {}",
        text_of(&replayed)
    );
    assert_eq!(
        content(&replayed),
        content(
            &tools
                .replay(&alice, &session, "legacy handle member", &issued)
                .await
        ),
        "a pre-version handle and a current handle read different rows, so the absent stamp is being \
         treated as a snapshot rather than as no claim at all"
    );
    assert_eq!(
        keys(&content(&replayed)),
        vec!["due_reminders", "memories", "mode", "scope"],
        "the legacy arm answered with a different key set: {}",
        text_of(&replayed)
    );
    let rendered = text_of(&replayed);
    assert!(
        rendered.contains("alice legacy handle member"),
        "the legacy replay did not serve the caller's own current member: {rendered}"
    );
    // The two keys the old payload carried are not echoed back and are not read as anything.
    for forbidden in ["issued_at", "depth", "membership_revision"] {
        assert!(
            !rendered.contains(forbidden),
            "the response names {forbidden:?}, which belongs to the handle's encoding and not to a \
             memory: {rendered}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Replay is not authority: foreign, absent and unreadable handles
// ---------------------------------------------------------------------------------------------

/// A foreign handle and an absent cluster answer identically.
///
/// Both ride the same opaque string a client controls. If they differed, `recall` would be an oracle
/// for whether some other account holds a given cluster id — the same reason `delete_memory` answers a
/// foreign id exactly as a missing one.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_handle_and_an_absent_cluster_answer_identically() {
    let tools = Recall::hosted().await;
    let alice = tools.owner("t7-eig-alice").await;
    let bob = tools.owner("t7-eig-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;

    let bob_fact = tools
        .seed_fact("t7-eig-bob-fact", "bob private vault rotation note", &bob)
        .await;
    let bob_cluster = tools.seed_cluster("bob-vault", &bob).await;
    tools.link_member(&bob_cluster, &bob_fact).await;

    let stolen = issued_handle(
        &tools
            .broad(&bob, &tools.handshake(&bob.bearer).await, "vault rotation")
            .await,
    );

    let replay = tools
        .replay(&alice, &alice_session, "vault rotation", &stolen)
        .await;

    // The other half: a handle that *used to* name a cluster which is now gone. The row is deleted
    // the way the dreaming merge path deletes one, so "absent" is a state production makes rather
    // than a string invented here — and the answer must be the same shape as the foreign one.
    let alice_fact = tools
        .seed_fact("t7-eig-alice-fact", "alice own gone vault note", &alice)
        .await;
    let gone = tools.seed_cluster("alice-gone", &alice).await;
    tools.link_member(&gone, &alice_fact).await;
    let formerly = issued_handle(&tools.broad(&alice, &alice_session, "gone vault note").await);
    tools
        .db
        .inner()
        .query("DELETE type::record('cluster', $key);")
        .bind(("key", key_of(&gone)))
        .await
        .unwrap()
        .check()
        .unwrap();
    let absent = tools
        .replay(&alice, &alice_session, "gone vault note", &formerly)
        .await;

    let rendered = text_of(&replay);
    assert!(
        !rendered.contains("bob private vault rotation note"),
        "alice replayed bob's handle and read bob's memory: {rendered}"
    );
    assert_eq!(
        content(&replay)["memories"],
        content(&absent)["memories"],
        "a foreign handle and a handle whose cluster is gone answered differently, which makes the \
         first an existence oracle: {} vs {}",
        text_of(&replay),
        text_of(&absent)
    );
    assert_eq!(
        content(&replay)["memories"]
            .as_array()
            .map(|m| m.len())
            .unwrap_or(1),
        0,
        "the replay read members it should not have: {rendered}"
    );
}

/// Every handle the server cannot decode gets the same refusal, and names nothing it hit on the way.
///
/// A blank string, whitespace, a non-base64 string and valid base64 of the wrong JSON reach four
/// different engine messages today — a base64 complaint and a JSON complaint are distinguishable, and
/// both quote the decoder. One text for all four keeps the outward answer flat and keeps the decoder
/// out of the response.
#[tokio::test(flavor = "multi_thread")]
async fn every_undecodable_handle_gets_the_same_refusal_and_no_decoder_text() {
    let tools = Recall::hosted().await;
    let alice = tools.owner("t7-mal-alice").await;
    let session = tools.handshake(&alice.bearer).await;

    let wrong_shape = base64url_encode(b"{\"nonsense\":true}");
    let right_shape_wrong_types = base64url_encode(
        json!({"cluster_id": 7, "version": {"membership_revision": "many", "members": []}})
            .to_string()
            .as_bytes(),
    );

    let mut answers = Vec::new();
    for handle in [
        "",
        "   ",
        "not base64!!",
        &wrong_shape,
        &right_shape_wrong_types,
    ] {
        let result = tools
            .replay(&alice, &session, "malformed handle probe", handle)
            .await;
        let rendered = text_of(&result);
        for forbidden in [
            "base64",
            "Base64",
            "Invalid symbol",
            "expected value",
            "serde",
            "line 1",
            "column",
            "is not a",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "the refusal for {handle:?} leaked decoder text {forbidden:?}: {rendered}"
            );
        }
        answers.push((handle.to_string(), rendered));
    }
    let (_, first) = &answers[0];
    for (handle, answer) in answers.iter().skip(1) {
        assert_eq!(
            first, answer,
            "the handle {handle:?} answered differently from a blank one, so the refusal is an \
            oracle for how far decoding got"
        );
    }
}

/// A handle that decodes cleanly but names something that is not a cluster is refused as the handle it
/// is: unreadable.
///
/// `RecallHandle::decode` reads the envelope; the `cluster_id` inside it is still caller text, and the
/// scoped cluster read rejects `fact:1` — or a string that is not a record id at all — as *input*. That
/// rejection used to reach the client as `invalid input: malformed cluster id`: a second refusal text
/// for one mistake, making the answer a probe for how far the server got with the caller's own string,
/// and quoting the storage layer while doing it. Both classes now answer with exactly the text
/// [`every_undecodable_handle_gets_the_same_refusal_and_no_decoder_text`] pins, and this case keeps the
/// two halves apart that must stay apart: a cluster the caller cannot read is answered, not refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_decoded_handle_naming_an_unusable_cluster_refuses_like_an_undecodable_one() {
    let tools = Recall::hosted().await;
    let alice = tools.owner("t7-badid-alice").await;
    let session = tools.handshake(&alice.bearer).await;

    // The baseline: one text for a handle the server cannot read at all.
    let baseline = text_of(
        &tools
            .replay(&alice, &session, "unusable id probe", "not base64!!")
            .await,
    );

    // The positive control the refusal equality below needs: this fixture *can* answer a replay, so
    // "every handle is refused" is not what is being proven.
    let alice_fact = tools
        .seed_fact("t7-badid-alice-fact", "alice own rotation note", &alice)
        .await;
    let alice_cluster = tools.seed_cluster("alice-rotation", &alice).await;
    tools.link_member(&alice_cluster, &alice_fact).await;
    let own = issued_handle(&tools.broad(&alice, &session, "rotation note").await);
    let served = tools.replay(&alice, &session, "rotation note", &own).await;
    assert_eq!(
        content(&served)["mode"],
        "focused",
        "the caller's own handle did not answer the focused arm, so the refusals below prove \
         nothing: {}",
        text_of(&served)
    );

    // Every shape that cannot name a usable cluster row — including the empty key and a non-cluster
    // table — gets the one handle refusal. The caller chose the string; the server must not reveal
    // which storage predicate rejected it.
    for cluster_id in [
        "not a record id",
        "",
        "cluster:",
        "fact:1",
        "account:7",
        "workspace:alice",
    ] {
        let handle = base64url_encode(json!({ "cluster_id": cluster_id }).to_string().as_bytes());
        let answer = tools
            .replay(&alice, &session, "rotation note", &handle)
            .await;
        let rendered = text_of(&answer);
        assert_eq!(
            baseline, rendered,
            "a cleanly decoded handle naming {cluster_id:?} answered differently from an unreadable \
             handle, so the refusal is an oracle for which layer gave up"
        );
        for forbidden in [
            "malformed cluster id",
            "invalid input",
            "StorageError",
            "InvalidInput",
            "record",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "the refusal for {cluster_id:?} leaked storage text {forbidden:?}: {rendered}"
            );
        }
    }

    // The half that must *not* become a refusal: a well-formed `cluster` id the caller cannot read
    // answers exactly as one that does not exist, and neither answers as an unreadable handle. The
    // made-up keys are bare identifiers on purpose: a hyphenated key would not parse as one record
    // literal, which is the refused class the loop above already covers.
    let foreign = tools
        .replay(
            &alice,
            &session,
            "rotation note",
            &base64url_encode(
                json!({ "cluster_id": "cluster:notshersatall" })
                    .to_string()
                    .as_bytes(),
            ),
        )
        .await;
    let absent = tools
        .replay(
            &alice,
            &session,
            "rotation note",
            &base64url_encode(
                json!({ "cluster_id": "cluster:deletedlongago" })
                    .to_string()
                    .as_bytes(),
            ),
        )
        .await;
    assert_eq!(
        text_of(&foreign),
        text_of(&absent),
        "a well-formed id the caller cannot read stopped answering as an absent one"
    );
    assert_eq!(
        content(&foreign)["memories"]
            .as_array()
            .map(|m| m.len())
            .unwrap_or(1),
        0,
        "the replay read members it should not have: {}",
        text_of(&foreign)
    );
    assert_ne!(
        baseline,
        text_of(&foreign),
        "an unreadable cluster id now refuses the same way an unreadable *handle* does, which is \
         right; a readable id for a cluster that is simply not the caller's must not join it"
    );
}

// ---------------------------------------------------------------------------------------------
// Response compatibility and the privacy surface
// ---------------------------------------------------------------------------------------------

/// Both arms keep the published shape, and neither carries an internal thing.
///
/// The version travels inside the opaque handle a client already echoes, so the narrowing loop needs
/// no new public field; this test is the guard that keeps that true *and* the negative control for the
/// leak claims: no workspace handle, no account handle, no SQL, no other account's counts.
#[tokio::test(flavor = "multi_thread")]
async fn recall_responses_keep_their_published_shape_and_leak_no_internals() {
    let tools = Recall::hosted().await;
    let alice = tools.owner("t7-shp-alice").await;
    let session = tools.handshake(&alice.bearer).await;
    let fact = tools
        .seed_fact("t7-shp-fact", "alice shape probe memory", &alice)
        .await;
    let cluster = tools.seed_cluster("alice-shape", &alice).await;
    tools.link_member(&cluster, &fact).await;

    let broad = tools.broad(&alice, &session, "shape probe").await;
    assert_eq!(
        keys(&content(&broad)),
        vec!["clusters", "due_reminders", "mode", "scope"],
        "the broad response's published key set moved"
    );
    let clusters = content(&broad)["clusters"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        keys(&clusters[0]),
        vec![
            "cluster_id",
            "representative_memories",
            "scope_handle",
            "similarity"
        ],
        "a broad candidate's published key set moved"
    );

    let focused = tools
        .replay(&alice, &session, "shape probe", &issued_handle(&broad))
        .await;
    assert_eq!(
        keys(&content(&focused)),
        vec!["due_reminders", "memories", "mode", "scope"],
        "the focused response's published key set moved"
    );
    // `scope` is the selection that actually ran, and it is a *label*: the caller asked for nothing here,
    // so the default shape is named back. It must never become a partition handle.
    assert_eq!(
        content(&broad)["scope"].as_str(),
        Some("personal"),
        "the broad arm did not name the population it read: {}",
        text_of(&broad)
    );
    for body in [&broad, &focused] {
        let said = text_of(body);
        for forbidden in ["project:", "workspace:", "account:"] {
            assert!(
                !said.contains(forbidden),
                "a response disclosed a {forbidden:?} handle: {said}"
            );
        }
    }

    let rendered = format!("{} {}", text_of(&broad), text_of(&focused));
    for forbidden in [
        "SELECT",
        "contains_memory",
        "membership_revision",
        "scope_projects",
        "quarantined_at",
        "workspace",
        &alice.account,
        &alice.workspace,
    ] {
        assert!(
            !rendered.contains(forbidden),
            "a recall response names {forbidden:?}, which is not memory content: {rendered}"
        );
    }

    // The advertised input schema is the flat four-parameter shape Task 8 published: `query`,
    // `scope_handle`, plus `scope` and `project`. Still no knob for picking an index or asking for
    // internals — which is why this stays an exact key set rather than an absence check: adding a
    // parameter is a public contract change and it has to be *this* test's business to notice. Read
    // from `tools/list`, the transport's own answer, rather than trusted from the params struct.
    let recall = tools
        .listed_tools(&alice, &session)
        .await
        .into_iter()
        .find(|tool| tool["name"].as_str() == Some("recall"))
        .expect("recall is advertised");
    let mut properties: Vec<String> = recall["inputSchema"]["properties"]
        .as_object()
        .expect("an input schema")
        .keys()
        .cloned()
        .collect();
    properties.sort();
    assert_eq!(
        properties,
        vec![
            "project".to_string(),
            "query".to_string(),
            "scope".to_string(),
            "scope_handle".to_string(),
        ],
        "the recall tool's advertised parameters moved, which is a public schema change: {recall}"
    );
    // The two new parameters are names for a partition and nothing else: no strategy knob, and the
    // `scope` description must not promise an index choice.
    let properties_object = recall["inputSchema"]["properties"]
        .as_object()
        .expect("an input schema");
    for forbidden in ["strategy", "indexed", "use_index", "limit"] {
        assert!(
            !properties_object.contains_key(forbidden),
            "recall grew a {forbidden:?} parameter: {recall}"
        );
    }
}
