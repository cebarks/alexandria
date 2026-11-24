//! The ordinary MCP writers go through Task 10's journal, not their own round trips.
//!
//! Before this file the four request-facing writers issued five to seven separate statements each: a store
//! created the fact, then its heat row, then a provenance row, then the cluster membership, then that
//! cluster's centroid, then the session link, with no transaction across them and `.ok()` on the interim
//! lineage edge. A refusal in the middle left a live memory whose text described one thing and whose vector
//! another, or a hidden snapshot nothing pointed back from, and an owner asking "what did this say before?"
//! had no answer at all, because nothing was recorded.
//!
//! Every case drives the **production transport**, so the principal, the lease and the request context are
//! rmcp's own and there is no test-only authority constructor in the path: owners come from `bootstrap_owner`,
//! the bearer from `mint_agent_token` plus `authenticate_agent_token`, and every context from
//! `AccessRepo::authorize`. Assertions are read back from the engine rather than from a response, because a
//! response that *describes* a journal row is not evidence that one exists.
//!
//! The centroid case is the reason this file carries its own scripted provider: a provider that answers every
//! text with the same vector is numerically blind to which population a centroid was averaged over, and the
//! whole distinction between "recompute from the members" and "nudge the stored number" is numerical.

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
use axum::http::{HeaderValue, header::CONTENT_TYPE};
use chrono::Utc;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value as Json, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use surrealdb::types::RecordId;
use tower::ServiceExt;

mod fixtures;
#[path = "journaled_writers_test/typed_destination.rs"]
mod typed_destination;

use fixtures::{MCP_PATH, read_body, router_post, with_bearer, with_session};

const PASSWORD_HASH: &str = "$argon2id$fixture-prepared-hash-journal-padding-padding";

/// A provider that answers each known text with its own vector, and anything unknown with `[1,0]`.
///
/// Distinct vectors matter here, not just a count: the centroid assertions below compare a number that only
/// moves when a *different* member joins.
#[derive(Clone)]
struct Scripted {
    vectors: HashMap<&'static str, Vec<f32>>,
}

#[async_trait::async_trait]
impl EmbeddingProvider for Scripted {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
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
        "journal-scripted"
    }
}

struct Owner {
    bearer: String,
    workspace: String,
}

struct Tools {
    db: Arc<Database>,
    router: Router,
    system: SystemContext,
}

impl Tools {
    async fn built(vectors: Vec<(&'static str, Vec<f32>)>) -> Self {
        let mut db = Database::connect_embedded().await.unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let system = db.claim_system_authority().unwrap();
        let db = Arc::new(db);
        let scripted = Scripted {
            vectors: vectors.into_iter().collect(),
        };
        let server = AlexandriaServer::new(
            Arc::clone(&db),
            Arc::new(scripted),
            0.75,
            HeatSettings::default(),
        );

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
        Self { db, router, system }
    }

    /// Enrolls an owner, mints it an agent token, and hands back the workspace the journal is keyed on.
    async fn owner(&self, login: &str) -> Owner {
        let ids = IdentityRepo::new(self.db.inner())
            .bootstrap_owner(
                login,
                PASSWORD_HASH,
                &OperationId::new(format!("jr-{login}")),
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
            .mint_agent_token(&context, "journal-agent", Utc::now())
            .await
            .unwrap();
        let bearer = token.secret.render().expose().to_string();
        // Authenticating the minted bearer is not decorative: it proves a refusal below is about the request
        // path and not about a credential that never resolved to a principal.
        AuthRepo::new(self.db.inner())
            .authenticate_agent_token(&bearer, Utc::now())
            .await
            .unwrap();
        let rows = self
            .rows(
                "SELECT id FROM workspace WHERE owner = $owner LIMIT 1;",
                vec![("owner", ids.account.as_record_id().clone())],
            )
            .await;
        Owner {
            bearer,
            workspace: rows[0]["id"].as_str().expect("a workspace id").to_string(),
        }
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
            .header("host", "127.0.0.1:3000")
            .body(Body::from(
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": {"name": "journal", "version": "0"}
                    }
                })
                .to_string(),
            ))
            .unwrap();
        request
            .headers_mut()
            .insert("host", HeaderValue::from_static("127.0.0.1:3000"));
        let response = self.router.clone().oneshot(request).await.unwrap();
        let headers = response.headers().clone();
        let bytes = read_body(response.into_body()).await;
        headers
            .get(alexandria_mcp::auth::SESSION_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_else(|| panic!("no session id in handshake body {bytes:?}"))
            .to_string()
    }

    async fn call(&self, owner: &Owner, session: &str, name: &str, arguments: Json) -> Json {
        let body = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": name, "arguments": arguments
        }});
        let request = with_protocol(with_bearer(
            with_session(router_post(body), session),
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
            .unwrap_or_else(|| panic!("{name}: no result frame at {status}, body {bytes:?}"))
    }

    /// A read straight from the engine, with **typed** `RecordId` binds.
    ///
    /// Typed rather than textual because a bound `RecordId` needs no re-parsing, and the point of reading the
    /// store here is to see what it holds for the ids the server wrote — not to watch a display form fail to
    /// round trip (`AGENTS.md`: `record_id_to_string` is **not** a parser-safe SQL form).
    async fn rows(&self, sql: &str, binds: Vec<(&str, RecordId)>) -> Vec<Json> {
        let mut query = self.db.inner().query(sql);
        for (name, value) in binds {
            query = query.bind((name, value));
        }
        let mut response = query.await.unwrap().check().unwrap();
        response.take(0).unwrap()
    }

    /// One row of one record, by handle — the read every "what does the store actually hold" assertion uses.
    async fn record_row(&self, handle: &str) -> Json {
        let rows = self
            .rows("SELECT * FROM $record;", vec![("record", record(handle))])
            .await;
        rows.into_iter()
            .next()
            .unwrap_or_else(|| panic!("no stored row for {handle}"))
    }

    /// A count over the population one handle reaches.
    async fn count_where(&self, sql: &str, handle: &str) -> i64 {
        let mut response = self
            .db
            .inner()
            .query(sql)
            .bind(("record", record(handle)))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Json> = response.take(0).unwrap();
        rows.first().and_then(|row| row["n"].as_i64()).unwrap_or(0)
    }

    /// A count of a whole table, for the cases where the population is the question.
    async fn count_all(&self, sql: &str) -> i64 {
        let mut response = self.db.inner().query(sql).await.unwrap().check().unwrap();
        let rows: Vec<Json> = response.take(0).unwrap();
        rows.first().and_then(|row| row["n"].as_i64()).unwrap_or(0)
    }

    /// The caller's own journal rows, oldest first — how the owner history page resolves them.
    async fn operations(&self, workspace: &str) -> Vec<Json> {
        self.rows(
            "SELECT kind, expected_effects, actual_effects, preimages, postimages FROM \
             semantic_operation WHERE workspace = $workspace ORDER BY created_at, id;",
            vec![("workspace", record(workspace))],
        )
        .await
    }

    /// The stored centroid of one cluster, read from the row rather than from any response.
    async fn centroid(&self, cluster: &str) -> Vec<f32> {
        self.record_row(cluster).await["centroid"]
            .as_array()
            .expect("a cluster has a centroid")
            .iter()
            .map(|value| value.as_f64().expect("numeric") as f32)
            .collect()
    }

    async fn first_cluster(&self, workspace: &str) -> String {
        let rows = self
            .rows(
                "SELECT id FROM cluster WHERE workspace = $workspace ORDER BY id LIMIT 1;",
                vec![("workspace", record(workspace))],
            )
            .await;
        rows.first()
            .and_then(|row| row["id"].as_str())
            .unwrap_or_default()
            .to_string()
    }
}

/// Parses a `table:key` handle into the typed form the engine binds.
fn record(handle: &str) -> RecordId {
    RecordId::parse_simple(handle).unwrap_or_else(|_| panic!("{handle} is not a record id"))
}

/// Overwrites a stored cluster centroid with a number its members do not imply.
///
/// This is the injection that separates the two centroid algorithms. A running average *seeded from the
/// stored value* inherits the corruption; a recomputation from the members ignores it. There is no seam in
/// the product to fake a concurrent writer with, so the race is produced the only way that is real: by
/// making the stored derived value wrong, which is exactly the state a concurrent write leaves behind.
async fn corrupt_centroid(tools: &Tools, cluster: &str, centroid: &[f32]) {
    tools
        .db
        .inner()
        .query("UPDATE $record SET centroid = $centroid;")
        .bind(("record", record(cluster)))
        .bind(("centroid", centroid.to_vec()))
        .await
        .unwrap()
        .check()
        .unwrap();
}

fn with_protocol(mut request: axum::http::Request<Body>) -> axum::http::Request<Body> {
    request.headers_mut().insert(
        "mcp-protocol-version",
        HeaderValue::from_static("2025-06-18"),
    );
    request
}

/// The id a successful tool call reported, from either response shape the server uses.
fn stored_id(result: &Json) -> String {
    let from_structured = result["structuredContent"]["id"]
        .as_str()
        .map(str::to_string);
    from_structured
        .or_else(|| {
            result["content"]
                .as_array()
                .and_then(|parts| parts.first())
                .and_then(|part| part["text"].as_str())
                .and_then(|text| serde_json::from_str::<Json>(text).ok())
                .and_then(|body| body["id"].as_str().map(str::to_string))
        })
        .unwrap_or_else(|| panic!("no stored id in {result}"))
}

fn refused(result: &Json) -> bool {
    result["isError"].as_bool() == Some(true)
}

fn effect_labels(row: &Json) -> Vec<String> {
    row["expected_effects"]
        .as_array()
        .expect("an operation states its effects")
        .iter()
        .map(|value| value.as_str().expect("effects are text").to_string())
        .collect()
}

fn nearly(actual: &[f32], expected: &[f32]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(a, e)| (a - e).abs() < 0.0001)
}

// -- the journaled store ----------------------------------------------------------------------------

/// A stored memory is **one** journaled operation, and the row it describes is stamped as journaled.
///
/// The observable claim Task 10 makes is that a store is a single semantic change. Before it nothing reached
/// `semantic_operation` at all, so the count going from zero to one is the whole point of the case; the
/// positive control (a second store makes it two) is what proves the counter counts rather than sitting
/// empty because the table is unreadable.
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_memory_journals_exactly_one_operation() {
    let tools = Tools::built(vec![("journal store", vec![1.0f32, 0.0f32])]).await;
    let owner = tools.owner("journal-store").await;
    let session = tools.handshake(&owner.bearer).await;

    let stored = tools
        .call(
            &owner,
            &session,
            "store_memory",
            json!({"content": "journal store", "scope": "personal", "tags": ["kept"]}),
        )
        .await;
    assert!(!refused(&stored), "a lawful store was refused: {stored}");
    let id = stored_id(&stored);

    let operations = tools.operations(&owner.workspace).await;
    assert_eq!(
        operations.len(),
        1,
        "a store wrote {} journal rows, expected exactly one: {operations:?}",
        operations.len()
    );
    assert_eq!(operations[0]["kind"].as_str(), Some("store"));
    let effects = effect_labels(&operations[0]);
    for wanted in ["store:memory", "vector", "provenance"] {
        assert!(
            effects.iter().any(|label| label == wanted),
            "a store journaled {effects:?}, which does not name {wanted:?}"
        );
    }

    // The row itself says it was written under the journal: `semantic_revision = 1` is what storage's
    // `CREATE_FACT_SQL` stamps, and a row written by an unjournaled writer keeps 0.
    let stamped = tools.record_row(&id).await;
    assert_eq!(
        stamped["semantic_revision"].as_i64(),
        Some(1),
        "the stored row is not stamped as journaled: {stamped:?}"
    );

    // The rows the change owes are present because they are part of the same commit: the provenance is
    // *linked* to the memory it describes, and a heat row exists at all.
    //
    // `in` is the memory and `out` is the provenance row: `RELATE $fact->has_provenance->$provenance` puts the
    // relation's FROM end in `in`, which is the direction v001's `TYPE RELATION` declares and the opposite of
    // what a first reading of "the fact has provenance" suggests.
    assert_eq!(
        tools
            .count_where(
                "SELECT count() AS n FROM has_provenance WHERE in = $record GROUP ALL;",
                &id,
            )
            .await,
        1,
        "the stored memory has no provenance linked to it"
    );
    assert_eq!(
        tools
            .count_where(
                "SELECT count() AS n FROM heat_state WHERE memory = $record GROUP ALL;",
                &id,
            )
            .await,
        1,
        "the stored memory has no heat row"
    );

    // Positive control: a second store adds exactly one more journal row.
    tools
        .call(
            &owner,
            &session,
            "store_memory",
            json!({"content": "journal store", "scope": "personal"}),
        )
        .await;
    assert_eq!(
        tools.operations(&owner.workspace).await.len(),
        2,
        "the journal does not accumulate one row per store"
    );
}

/// The centroid a memory joins is recomputed from the cluster's members, not nudged from its stored value.
///
/// This is what "engine-side real cluster recomputation" means in practice, and it is only visible when the
/// stored centroid is already wrong — which is the state a concurrent writer leaves behind, since its mean was
/// derived over the population it read a moment before it committed. Seeding a mild error is the honest
/// stand-in for that race: the product has no test seam, and asserting the running average's own answer would
/// pass against the approximation this replaces.
///
/// The corruption is deliberately *near* the members rather than far from them. Ranking a joining vector uses
/// the stored centroid, so a wildly wrong one (say `[0.1,0.9]` against members near `[1,0]`) would make the new
/// vector match no cluster and mint its own — a correct outcome for a different question, and the failure the
/// first draft of this case actually measured. `[0.7,0.3]` still attracts the join and still differs from the
/// mean of the members by enough to tell the two algorithms apart:
///
/// * recomputed from the members `[1,0]` plus the joining `[0.8,0.2]`: the mean is `[0.9,0.1]`;
/// * the O(1) running average seeded from the stored `[0.7,0.3]` with `n = 1`: `[0.75,0.25]`.
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_memory_recomputes_the_cluster_centroid_from_its_members() {
    let tools = Tools::built(vec![
        ("centroid member one", vec![1.0f32, 0.0f32]),
        ("centroid member two", vec![0.8f32, 0.2f32]),
    ])
    .await;
    let owner = tools.owner("journal-centroid").await;
    let session = tools.handshake(&owner.bearer).await;

    tools
        .call(
            &owner,
            &session,
            "store_memory",
            json!({"content": "centroid member one", "scope": "personal"}),
        )
        .await;
    let cluster = tools.first_cluster(&owner.workspace).await;
    assert!(!cluster.is_empty(), "the first store created no cluster");
    assert!(
        nearly(&tools.centroid(&cluster).await, &[1.0, 0.0]),
        "a one-member cluster's centroid is not its member: {:?}",
        tools.centroid(&cluster).await
    );

    // Nudge the derived value, then join a member the cluster will still accept.
    corrupt_centroid(&tools, &cluster, &[0.7, 0.3]).await;
    let second = tools
        .call(
            &owner,
            &session,
            "store_memory",
            json!({"content": "centroid member two", "scope": "personal"}),
        )
        .await;
    assert!(!refused(&second), "the joining store was refused: {second}");

    let centroid = tools.centroid(&cluster).await;
    assert!(
        nearly(&centroid, &[0.9, 0.1]),
        "the centroid was seeded from the stored value instead of recomputed from the members: \
         got {centroid:?}; the mean of the members [1,0] and the joining [0.8,0.2] is [0.9,0.1], \
         while the running average seeded from the injected [0.7,0.3] would answer [0.75,0.25]"
    );
    // The join really did join this cluster rather than minting another, or the assertion above would be
    // measuring a cluster that was never touched.
    let members = tools
        .count_where(
            "SELECT count() AS n FROM contains_memory WHERE in = $record GROUP ALL;",
            &cluster,
        )
        .await;
    assert_eq!(
        members, 2,
        "the joining vector did not attach to the existing cluster"
    );
}

// -- the journaled update ----------------------------------------------------------------------------

/// A content update journals the exact before and after, with the lineage edge in the same transaction.
///
/// Three of those four things used to be separate round trips, and the edge was the one that could be lost:
/// `.ok()` on a failed `create_edge` left a hidden snapshot with no path back from the row that replaced it.
#[tokio::test(flavor = "multi_thread")]
async fn an_updated_memory_journals_the_before_and_after() {
    let tools = Tools::built(vec![
        ("before the edit", vec![1.0f32, 0.0f32]),
        ("after the edit", vec![0.0f32, 1.0f32]),
    ])
    .await;
    let owner = tools.owner("journal-update").await;
    let session = tools.handshake(&owner.bearer).await;

    let stored = tools
        .call(
            &owner,
            &session,
            "store_memory",
            json!({"content": "before the edit", "scope": "personal"}),
        )
        .await;
    let id = stored_id(&stored);

    let updated = tools
        .call(
            &owner,
            &session,
            "update_memory",
            json!({"id": id, "content": "after the edit"}),
        )
        .await;
    assert!(!refused(&updated), "a lawful update was refused: {updated}");

    let operations = tools.operations(&owner.workspace).await;
    let kinds: Vec<&str> = operations
        .iter()
        .filter_map(|row| row["kind"].as_str())
        .collect();
    assert_eq!(
        kinds,
        vec!["store", "update"],
        "the update did not journal exactly one `update` operation"
    );
    let effects = effect_labels(operations.last().expect("the update row"));
    for wanted in ["content", "vector", "lineage:snapshot"] {
        assert!(
            effects.iter().any(|label| label == wanted),
            "a content update journaled {effects:?}, which does not name {wanted:?}"
        );
    }

    // The superseded text left a hidden snapshot, and the row that replaced it points at it.
    let snapshots = tools
        .rows(
            "SELECT content, deleted FROM fact WHERE deleted = true AND content = 'before the edit';",
            vec![],
        )
        .await;
    assert_eq!(
        snapshots.len(),
        1,
        "the superseded text left no hidden snapshot: {snapshots:?}"
    );
    let linked = tools
        .count_where(
            "SELECT count() AS n FROM memory_edge WHERE in = $record AND edge_type = 'derived_from' GROUP ALL;",
            &id,
        )
        .await;
    assert_eq!(
        linked, 1,
        "the lineage edge was not written with the update"
    );

    // The live row carries the new text, so the journal describes a change that actually landed.
    assert_eq!(
        tools.record_row(&id).await["content"].as_str(),
        Some("after the edit")
    );
}

/// An update of a row the caller cannot write is refused, journals nothing, and moves nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_update_journals_nothing_and_changes_nothing() {
    let tools = Tools::built(vec![("durable text", vec![1.0f32, 0.0f32])]).await;
    let owner = tools.owner("journal-stale").await;
    let session = tools.handshake(&owner.bearer).await;

    let stored = tools
        .call(
            &owner,
            &session,
            "store_memory",
            json!({"content": "durable text", "scope": "personal"}),
        )
        .await;
    let id = stored_id(&stored);
    let before = tools.operations(&owner.workspace).await;
    assert_eq!(before.len(), 1, "one store should be one journal row");

    let missing = tools
        .call(
            &owner,
            &session,
            "update_memory",
            json!({"id": "fact:never-stored-here", "content": "anything at all"}),
        )
        .await;
    assert!(
        refused(&missing),
        "an update of a row that does not exist answered success: {missing}"
    );

    assert_eq!(
        tools.operations(&owner.workspace).await.len(),
        1,
        "a refused update wrote a journal row"
    );
    assert_eq!(
        tools.record_row(&id).await["content"].as_str(),
        Some("durable text"),
        "a refused update moved the live row"
    );
}

// -- the journaled delete and import -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn deletion_reconciles_live_centroid_and_retains_historical_membership() {
    let tools = Tools::built(vec![
        ("delete contribution", vec![1.0, 0.0]),
        ("surviving contribution", vec![0.8, 0.6]),
    ])
    .await;
    let owner = tools.owner("delete-live-centroid").await;
    let session = tools.handshake(&owner.bearer).await;
    let mut ids = Vec::new();
    for content in ["delete contribution", "surviving contribution"] {
        let stored = tools
            .call(
                &owner,
                &session,
                "store_memory",
                json!({"content": content, "scope": "personal"}),
            )
            .await;
        ids.push(stored_id(&stored));
    }
    let cluster = tools.first_cluster(&owner.workspace).await;
    let members = tools
        .rows(
            "SELECT out FROM contains_memory WHERE in = $record ORDER BY out;",
            vec![("record", record(&cluster))],
        )
        .await;
    let mut member_ids: Vec<_> = members
        .iter()
        .map(|row| row["out"].as_str().unwrap().to_string())
        .collect();
    member_ids.sort();
    let mut expected_ids = ids.clone();
    expected_ids.sort();
    assert_eq!(
        member_ids, expected_ids,
        "both different vectors must share this cluster"
    );
    assert!(
        !nearly(&tools.centroid(&cluster).await, &[0.8, 0.6]),
        "fixture must require a real centroid change"
    );
    let before_fact = tools.record_row(&ids[0]).await;
    let semantic_edges = tools
        .rows(
            "SELECT * FROM memory_edge WHERE in = $record OR out = $record ORDER BY id;",
            vec![("record", record(&ids[0]))],
        )
        .await;
    let mut semantic_ids: Vec<_> = semantic_edges
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect();
    semantic_ids.sort();
    let survivor = tools.record_row(&ids[1]).await;
    let before_cluster = tools.record_row(&cluster).await;
    let before_operations = tools.operations(&owner.workspace).await.len();

    let deleted = tools
        .call(&owner, &session, "delete_memory", json!({"id": ids[0]}))
        .await;
    assert!(!refused(&deleted), "lawful delete refused: {deleted}");
    let after_fact = tools.record_row(&ids[0]).await;
    let mut expected_fact = before_fact.clone();
    expected_fact["deleted"] = json!(true);
    expected_fact["semantic_revision"] =
        json!(before_fact["semantic_revision"].as_u64().unwrap() + 1);
    assert_eq!(
        after_fact, expected_fact,
        "only visibility and its revision may change"
    );
    assert_eq!(tools.record_row(&ids[1]).await, survivor);
    assert_eq!(
        tools
            .rows(
                "SELECT out FROM contains_memory WHERE in = $record ORDER BY out;",
                vec![("record", record(&cluster))],
            )
            .await,
        members,
        "deletion must retain historical membership"
    );
    assert_eq!(
        tools
            .rows(
                "SELECT * FROM memory_edge WHERE in = $record OR out = $record ORDER BY id;",
                vec![("record", record(&ids[0]))],
            )
            .await,
        semantic_edges
    );
    let actual = tools.centroid(&cluster).await;
    assert!(
        nearly(&actual, &[0.8, 0.6]),
        "deleted contribution remains in live centroid: {actual:?}"
    );
    let after_cluster = tools.record_row(&cluster).await;
    let mut expected_cluster = before_cluster.clone();
    expected_cluster["centroid"] = after_cluster["centroid"].clone();
    expected_cluster["centroid_revision"] =
        json!(before_cluster["centroid_revision"].as_u64().unwrap_or(0) + 1);
    assert_eq!(
        after_cluster, expected_cluster,
        "membership and embedding revisions must not change"
    );
    let operations = tools.operations(&owner.workspace).await;
    assert_eq!(operations.len(), before_operations + 1);
    let operation = operations.last().unwrap();
    assert_eq!(operation["kind"], "delete");
    assert_eq!(operation["expected_effects"], operation["actual_effects"]);
    let mut effects = effect_labels(operation);
    effects.sort();
    assert_eq!(effects, vec!["cluster:centroid", "visibility:delete"]);
    assert_eq!(operation["preimages"].as_array().unwrap().len(), 2);
    assert_eq!(operation["postimages"].as_array().unwrap().len(), 2);
    for (phase, fact, cluster_row) in [
        ("preimages", &before_fact, &before_cluster),
        ("postimages", &after_fact, &after_cluster),
    ] {
        let mut images = Vec::new();
        for reference in operation[phase].as_array().unwrap() {
            images.push(tools.record_row(reference.as_str().unwrap()).await);
        }
        let fact_image = images
            .iter()
            .find(|row| row["fact"].as_str() == Some(ids[0].as_str()))
            .unwrap();
        for (snapshot_field, stored_field) in [
            ("fact_content", "content"),
            ("fact_tags", "tags"),
            ("fact_confidence", "confidence"),
            ("fact_scope", "scope"),
            ("fact_project", "project"),
            ("fact_revision", "semantic_revision"),
            ("fact_deleted", "deleted"),
            ("fact_quarantined_at", "quarantined_at"),
            ("fact_embedding", "embedding"),
        ] {
            assert_eq!(
                fact_image[snapshot_field], fact[stored_field],
                "{phase}: {snapshot_field}"
            );
        }
        let mut relations: Vec<_> = fact_image["fact_relations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect();
        relations.sort();
        assert_eq!(
            relations, semantic_ids,
            "{phase}: actual semantic incident edges"
        );
        assert_eq!(fact_image["fact_embedding_dimensions"], 2);
        assert!(
            fact_image
                .get("fact_embedding_model")
                .is_none_or(Json::is_null)
        );
        let cluster_image = images
            .iter()
            .find(|row| row["cluster"].as_str() == Some(cluster.as_str()))
            .unwrap();
        for (snapshot_field, stored_field) in [
            ("cluster_label", "label"),
            ("cluster_depth", "depth"),
            ("cluster_centroid", "centroid"),
        ] {
            assert_eq!(
                cluster_image[snapshot_field], cluster_row[stored_field],
                "{phase}: {snapshot_field}"
            );
        }
        for (snapshot_field, stored_field) in [
            ("cluster_membership_revision", "membership_revision"),
            (
                "cluster_member_embedding_revision",
                "member_embedding_revision",
            ),
            ("cluster_centroid_revision", "centroid_revision"),
        ] {
            assert_eq!(
                cluster_image[snapshot_field].as_i64().unwrap_or(0),
                cluster_row[stored_field].as_i64().unwrap_or(0),
                "{phase}: {snapshot_field}"
            );
        }
        let mut retained: Vec<_> = cluster_image["cluster_members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect();
        retained.sort();
        assert_eq!(retained, member_ids, "{phase}: full retained membership");
    }
}

/// A soft delete is a journaled visibility change, not a bare `UPDATE`.
#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_memory_journals_the_visibility_change() {
    let tools = Tools::built(vec![("to be forgotten", vec![1.0f32, 0.0f32])]).await;
    let owner = tools.owner("journal-delete").await;
    let session = tools.handshake(&owner.bearer).await;

    let stored = tools
        .call(
            &owner,
            &session,
            "store_memory",
            json!({"content": "to be forgotten", "scope": "personal"}),
        )
        .await;
    let id = stored_id(&stored);
    let cluster = tools.first_cluster(&owner.workspace).await;
    let before_cluster = tools.record_row(&cluster).await;
    let members = tools
        .rows(
            "SELECT * FROM contains_memory WHERE in = $record ORDER BY id;",
            vec![("record", record(&cluster))],
        )
        .await;
    assert_eq!(members.len(), 1);

    let deleted = tools
        .call(&owner, &session, "delete_memory", json!({"id": id}))
        .await;
    assert!(!refused(&deleted), "a lawful delete was refused: {deleted}");

    let operations = tools.operations(&owner.workspace).await;
    let kinds: Vec<&str> = operations
        .iter()
        .filter_map(|row| row["kind"].as_str())
        .collect();
    assert_eq!(kinds, vec!["store", "delete"], "{operations:?}");
    let effects = effect_labels(&operations[1]);
    assert_eq!(effects, vec!["visibility:delete"]);
    assert_eq!(
        tools.record_row(&cluster).await,
        before_cluster,
        "last eligible deletion retains the centre and all guards"
    );
    assert_eq!(
        tools
            .rows(
                "SELECT * FROM contains_memory WHERE in = $record ORDER BY id;",
                vec![("record", record(&cluster))]
            )
            .await,
        members
    );
    assert_eq!(operations[1]["preimages"].as_array().unwrap().len(), 1);
    assert_eq!(operations[1]["postimages"].as_array().unwrap().len(), 1);
    assert!(
        effects.iter().any(|label| label == "visibility:delete"),
        "a delete journaled {effects:?}"
    );

    assert_eq!(
        tools.record_row(&id).await["deleted"].as_bool(),
        Some(true),
        "the journaled delete did not hide the row"
    );

    // The journal states *both* sides, which is what makes the owner's history page able to answer "what did
    // this say before": the preimage is the live text, the postimage the hidden row.
    assert!(
        !operations[1]["preimages"]
            .as_array()
            .expect("an operation carries its preimages")
            .is_empty(),
        "a delete journaled no before-state"
    );
}

/// An import journals every chunk it writes.
///
/// One operation per chunk rather than one per document, which is what classification approval does for a
/// classification batch: the approved effect set of a move is that one memory, and a transaction that spans
/// a whole document cannot state which entries a reviewer approved. Chunk atomicity is still whole — a chunk
/// that lands carries its vector, its heat, its source link and its cluster membership together — and a
/// refusal leaves that chunk's rows behind rather than half of them.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_journals_its_chunks() {
    let tools = Tools::built(vec![]).await;
    let owner = tools.owner("journal-import").await;
    let session = tools.handshake(&owner.bearer).await;

    let imported = tools
        .call(
            &owner,
            &session,
            "import_document",
            json!({
                "content": "# First section\n\nThe body of the first section.\n\n# Second section\n\nThe body of the second section.",
                "scope": "personal",
                "mode": "chunk",
                "chunk_strategy": "heading",
            }),
        )
        .await;
    assert!(
        !refused(&imported),
        "a lawful import was refused: {imported}"
    );
    let count = imported["structuredContent"]["count"]
        .as_i64()
        .unwrap_or_default();
    assert!(count >= 2, "the fixture document chunked into {count}");

    let operations = tools.operations(&owner.workspace).await;
    assert_eq!(
        operations.len(),
        count as usize,
        "an import of {count} chunks wrote {} journal rows",
        operations.len()
    );
    for row in &operations {
        assert_eq!(row["kind"].as_str(), Some("import"), "{row:?}");
        let effects = effect_labels(row);
        assert!(
            effects.iter().any(|label| label == "import:chunk"),
            "an imported chunk journaled {effects:?}"
        );
    }

    // Every chunk reaches the one source document it was cut from.
    let linked = tools
        .count_all("SELECT count() AS n FROM extracted_from GROUP ALL;")
        .await;
    assert_eq!(
        linked, count,
        "only {linked} of {count} chunks reached a source document"
    );
    let raws = tools
        .count_all("SELECT count() AS n FROM raw GROUP ALL;")
        .await;
    assert_eq!(raws, 1, "one document wrote {raws} raw rows");
}

/// An import that lands no chunk leaves no source document behind.
///
/// The scoped path used to create the `raw` row before the first embed, so an import whose every chunk
/// failed — a refusal, a revoked credential, an engine error on the first statement — left a document
/// nothing links to and nothing journals: invisible to owner history, reachable only by listing the table.
/// The raw row now rides in the **first chunk's own transaction**, so "no chunks" and "no document" are the
/// same outcome. The positive control is the same store with a chunk that does land.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_that_lands_no_chunk_leaves_no_source_document() {
    let tools = Tools::built(vec![]).await;
    let owner = tools.owner("orphan-import").await;
    let session = tools.handshake(&owner.bearer).await;

    // A refusal that happens *before* any chunk can land: no destination for a hosted write. The
    // statement is the one the design's own ordering test uses — refused before the first embed.
    let refused_call = tools
        .call(
            &owner,
            &session,
            "import_document",
            json!({"content": "# Only section\n\nThe body.", "mode": "chunk"}),
        )
        .await;
    assert!(
        refused(&refused_call),
        "the untargeted import should refuse: {refused_call}"
    );
    assert_eq!(
        tools
            .count_all("SELECT count() AS n FROM raw GROUP ALL;")
            .await,
        0,
        "an import that stored nothing still left a source document behind"
    );
    assert_eq!(
        tools
            .count_all("SELECT count() AS n FROM fact GROUP ALL;")
            .await,
        0,
        "the refused import stored a memory"
    );
    assert_eq!(
        tools.operations(&owner.workspace).await.len(),
        0,
        "the refused import journalled an operation"
    );

    // Positive control on the same store and owner: with a target, the document exists *because* a chunk
    // exists, and the two are counted together.
    let landed = tools
        .call(
            &owner,
            &session,
            "import_document",
            json!({"content": "# Only section\n\nThe body.", "scope": "personal", "mode": "chunk"}),
        )
        .await;
    assert!(
        !refused(&landed),
        "the control import was refused: {landed}"
    );
    let chunks = landed["structuredContent"]["count"]
        .as_i64()
        .unwrap_or_default();
    assert!(chunks >= 1, "the control import landed {chunks} chunks");
    assert_eq!(
        tools
            .count_all("SELECT count() AS n FROM raw GROUP ALL;")
            .await,
        1,
        "one document that landed should hold exactly one source row"
    );
    let linked = tools
        .count_all("SELECT count() AS n FROM extracted_from GROUP ALL;")
        .await;
    assert_eq!(
        linked, chunks,
        "{linked} of {chunks} chunks reached the source document"
    );
    let operations = tools.operations(&owner.workspace).await;
    assert_eq!(
        operations.len(),
        chunks as usize,
        "one journaled operation per landed chunk expected, got {}",
        operations.len()
    );

    // The journal test proper: the first chunk's operation **names the source document among the rows it
    // affected**. Before this change the `raw` row was created by its own round trip outside any
    // transaction, so no operation described it and an orphan would have been invisible to owner history as
    // well as to the corpus — "journaled coherently with the first real chunk" is exactly the claim this
    // asserts, and it is the discriminator the refusal count alone could not provide.
    let affected: Vec<Json> = tools
        .rows(
            "SELECT affected_ids FROM semantic_operation WHERE workspace = $workspace ORDER BY \
             created_at, id;",
            vec![("workspace", record(&owner.workspace))],
        )
        .await;
    let journaled_raws = affected
        .iter()
        .flat_map(|row| row["affected_ids"].as_array().cloned().unwrap_or_default())
        .filter_map(|value| value.as_str().map(str::to_string))
        .filter(|handle| handle.starts_with("raw:"))
        .collect::<Vec<_>>();
    assert_eq!(
        journaled_raws.len(),
        1,
        "the source document was named by {} operations; exactly one (its first chunk's) must name it",
        journaled_raws.len()
    );
    // The stored `raw` row and the journaled one are the same row, not two.
    let stored_raw = tools
        .rows(
            "SELECT meta::id(id) AS key FROM raw WHERE workspace = $workspace;",
            vec![("workspace", record(&owner.workspace))],
        )
        .await;
    assert_eq!(
        stored_raw.len(),
        1,
        "one document should hold exactly one source row, got {}",
        stored_raw.len()
    );
}

// -- the journal belongs to its owner ----------------------------------------------------------------

/// One owner's journal never appears in another's, and a foreign id answers as an absent one.
#[tokio::test(flavor = "multi_thread")]
async fn one_owners_journal_is_never_another_owners() {
    let tools = Tools::built(vec![("private body", vec![1.0f32, 0.0f32])]).await;
    let alice = tools.owner("journal-alice").await;
    let bob = tools.owner("journal-bob").await;
    let alice_session = tools.handshake(&alice.bearer).await;
    let bob_session = tools.handshake(&bob.bearer).await;

    let stored = tools
        .call(
            &alice,
            &alice_session,
            "store_memory",
            json!({"content": "private body", "scope": "personal"}),
        )
        .await;
    let id = stored_id(&stored);

    tools
        .call(
            &bob,
            &bob_session,
            "store_memory",
            json!({"content": "private body", "scope": "personal"}),
        )
        .await;
    assert_eq!(tools.operations(&bob.workspace).await.len(), 1);
    assert_eq!(tools.operations(&alice.workspace).await.len(), 1);

    // Bob editing Alice's row and editing nothing must be indistinguishable from the outside, or the journal
    // becomes a way to test whether some other account holds a given id.
    let probe = tools
        .call(
            &bob,
            &bob_session,
            "update_memory",
            json!({"id": id, "content": "not yours"}),
        )
        .await;
    let absent = tools
        .call(
            &bob,
            &bob_session,
            "update_memory",
            json!({"id": "fact:nothing-here-at-all", "content": "not yours"}),
        )
        .await;
    assert_eq!(
        probe["content"][0]["text"].as_str(),
        absent["content"][0]["text"].as_str(),
        "a foreign row and an absent one answered differently"
    );
    // And Alice's own history is untouched by the attempt.
    assert_eq!(tools.operations(&alice.workspace).await.len(), 1);
}
