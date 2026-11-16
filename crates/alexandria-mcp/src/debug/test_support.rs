//! Shared test helpers for debug route tests. `pub(super)` — only visible within `debug`.
//!
//! Two families live here, and the difference between them is Task 9:
//!
//! * `test_server()` / `banded_server()` build a bare [`AlexandriaServer`]. They are the embedding-stub
//!   fixtures the storage-facing assertions use, and the router they can reach is the **fail-closed**
//!   one: `debug::router(server)` serves no private data, so a test that wants a page must sign in.
//! * [`authenticated_app`] does that: it enrols a real account through the same sealed chain
//!   production uses, signs in **over the router**, and hands back the router plus the cookie pair the
//!   response set. There is no test-only authority, no default owner, and nothing here that can turn a
//!   guard off — a migrated test that expects a page must earn one the way a browser would.
//!
//! Seeding is deliberately the scoped writers (`ScopedMemoryRepo`, `ScopedSessionRepo`,
//! `ScopedClusterRepo`), because a row written without an owner is invisible to every scoped reader:
//! seeding with `MemoryRepo::create_fact` would leave a "no foreign row appears" assertion passing while
//! the page showed nothing at all, which is the vacuity trap this crate has already been bitten by once.
//! [`Fixture::seed_personal`] is the one way these tests put a row in, and it takes the fixture's own
//! owner context.

#[path = "test_support/adoption_tests.rs"]
mod adoption_tests;

use std::sync::Arc;

use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::access::{AccessRepo, OwnerContext, ScopedContext};
use alexandria_storage::models::identity::{AccountId, OperationId};
use alexandria_storage::models::{MemoryScope, ReadSelection, Version};
use alexandria_storage::repos::{
    AuthRepo, ClusterRepo, IdentityRepo, PasswordDigest, ScopedClusterRepo, ScopedMemoryRepo,
    ScopedProjectRepo, ScopedSessionRepo, WriteTarget,
};
use alexandria_storage::{Database, record_id_to_string, schema};
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{CONTENT_TYPE, COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use chrono::Utc;
use surrealdb::types::RecordId;
use tower::ServiceExt;

use super::DebugContext;
use super::web::{CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, WebBoundary, WebPolicy};
use crate::AlexandriaServer;
use crate::auth::admission::Admission;
use crate::auth::http::AuthMiddleware;
use crate::auth::mcp_transport::{AuthEvents, McpSessionRegistry};
use crate::auth::service::AuthService;

/// Minimal stub embedding provider for router-level tests (no model download needed).
pub(super) struct StubEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for StubEmbedding {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2]).collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "stub"
    }
}

pub(super) async fn test_server() -> AlexandriaServer {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner())
        .await
        .unwrap();
    AlexandriaServer::new(
        Arc::new(db),
        Arc::new(StubEmbedding),
        0.75,
        crate::server::HeatSettings::default(),
    )
}

/// The floor [`banded_server`] runs with.
///
/// `StubEmbedding` returns one constant vector, so every similarity in those tests is 1.0 and the
/// floor can never filter anything — which makes it useless for testing *what the floor drops*.
/// [`BandedEmbedding`] exists for exactly that, and `banded_server` pairs it with a floor its
/// bands straddle.
pub(super) const BANDED_FLOOR: f32 = 0.30;

/// Stub that turns a keyword in the text into an exact cosine similarity to the query.
///
/// Every vector is the 2-D unit vector `[s, sqrt(1 - s²)]` and an unlabelled text — which is what
/// a query is — gets `s = 1`, i.e. `[1, 0]`. Cosine against `[1, 0]` is just `s`, so the
/// similarities are hand-computable and exact: `"strong"` scores [`STRONG_SIMILARITY`], `"weak"`
/// scores [`WEAK_SIMILARITY`], and those two straddle [`BANDED_FLOOR`] by 0.30 either side.
///
/// Deliberately a separate stub from [`StubEmbedding`]/`test_server()`: 40+ existing debug tests
/// assume every similarity is 1.0.
pub(super) struct BandedEmbedding;

/// Cosine similarity a text containing "strong" gets against the query.
pub(super) const STRONG_SIMILARITY: f32 = 0.60;
/// Cosine similarity a text containing "weak" gets against the query — below [`BANDED_FLOOR`].
pub(super) const WEAK_SIMILARITY: f32 = 0.20;

#[async_trait::async_trait]
impl EmbeddingProvider for BandedEmbedding {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let s = if t.contains("strong") {
                    STRONG_SIMILARITY
                } else if t.contains("weak") {
                    WEAK_SIMILARITY
                } else {
                    1.0
                };
                vec![s, (1.0 - s * s).sqrt()]
            })
            .collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "banded-stub"
    }
}

/// A server whose retrieval scores are predictable *and* split by the floor: content naming
/// "strong" lands at 0.60, content naming "weak" at 0.20, against a 0.30 floor.
///
/// The stored memories are written through this same stub, so their embeddings are the band
/// vectors — the ranking is deterministic from the content alone.
pub(super) async fn banded_server() -> AlexandriaServer {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner())
        .await
        .unwrap();
    AlexandriaServer::new(
        Arc::new(db),
        Arc::new(BandedEmbedding),
        0.75,
        crate::server::HeatSettings::default(),
    )
    .with_retrieve_min_similarity(BANDED_FLOOR)
}

/// The plain average of a set of embeddings — the centroid the cluster detail page *used* to
/// judge cohesion with, before it was pointed at the stored one.
///
/// Kept available to tests on purpose: it is how a test proves its own fixture is meaningful,
/// by showing the two centroids disagree. Production code must not use it (see
/// [`super::super::clusters::cohesion_of`]).
pub(super) fn average_centroid(members: &[Vec<f32>]) -> Vec<f32> {
    let dims = members.first().map(|m| m.len()).unwrap_or(0);
    let mut centroid = vec![0.0f32; dims];
    for member in members {
        for (index, value) in member.iter().enumerate() {
            centroid[index] += value;
        }
    }
    let n = members.len() as f32;
    for value in centroid.iter_mut() {
        *value /= n;
    }
    centroid
}

/// Creates a four-member cluster whose **stored** centroid and the average of its members
/// reach opposite cohesion verdicts, and returns its id.
///
/// Three members are unit vectors along +x; the fourth is 100× as long and 80° off that axis.
/// The stored centroid `[1, 0]` averages 0.79 cosine across members (Healthy at the default
/// 0.6 floor), while the member average is dragged out to roughly `[0.2, 0.98]`, which
/// averages 0.40 (Needs split). A fixture where the two agree would pass either implementation
/// and prove nothing — that agreement is exactly why the bug survived.
pub(super) async fn disagreeing_cluster(server: &AlexandriaServer) -> surrealdb::types::RecordId {
    use alexandria_storage::repos::{ClusterRepo, MemoryRepo};

    let cluster_repo = ClusterRepo::new(server.db.inner());
    let memory_repo = MemoryRepo::new(server.db.inner());
    let cid = cluster_repo
        .create(Some("stored centroid disagrees"), &[1.0, 0.0])
        .await
        .unwrap();
    for (index, embedding) in [
        [1.0_f32, 0.0],
        [1.0, 0.0],
        [1.0, 0.0],
        // 100 * [cos 80°, sin 80°]
        [17.364_816, 98.480_78],
    ]
    .iter()
    .enumerate()
    {
        let (_fact, fact_t) = memory_repo
            .create_fact_with_id(&format!("disagreeing member {index}"), 0.5, embedding, &[])
            .await
            .unwrap();
        cluster_repo.add_member(&cid, &fact_t).await.unwrap();
    }
    cid
}

// -- the authenticated fixture ------------------------------------------------------------------------

/// The fixture password. Enrolled at a real policy digest, because the login path verifies one and a
/// stub hash would mean the boundary under test is not the boundary production mounts.
pub(super) const PASSWORD: &str = "debug-fixture-password-debug-fixture-padding";

/// The hosted fixture origin. `WebPolicy::hosted` refuses a wildcard or an empty list, so every
/// authenticated test carries a real one.
pub(super) const ORIGIN: &str = "https://debug.example.test";
/// The request header name, spelled once for every suite's builder.
pub(crate) const HOST_HEADER: axum::http::HeaderName = axum::http::header::HOST;

/// The fixture's `Host`/`Origin` values. Named apart from the header constant of the same spelling.
pub(super) const HOST: &str = "debug.example.test";

/// One signed-in browser and the router it can reach.
pub(super) struct AuthenticatedTestApp {
    pub(super) router: axum::Router,
    pub(super) db: Arc<Database>,
    /// Kept because the *suite-local* fixtures that mint credentials (the query tester's heat case)
    /// reach the service through it; `dead_code` is allowed on the field rather than deleting it out
    /// from under a suite that needs it next.
    #[allow(dead_code)]
    pub(super) service: Arc<AuthService>,
    pub(super) session: Session,
    /// Tables **this** fixture removed, so its adoption layer skips them instead of reporting a failure
    /// the test caused. Per-fixture rather than process-global on purpose: these suites run in one
    /// binary and in parallel, so a shared list would silently disable adoption for every other test
    /// that happened to be mid-request.
    removed: Arc<std::sync::Mutex<Vec<String>>>,
    /// The caller's authorized context, for the cases that seed or assert through storage.
    pub(super) owner: OwnerContext,
}

/// The two cookie halves plus the rows they authenticate as.
#[derive(Clone)]
pub(super) struct Session {
    pub(super) cookie: String,
    pub(super) csrf: String,
}

impl Session {
    /// The `Cookie` header a browser would send.
    pub(super) fn cookie_header(&self) -> String {
        format!(
            "{SESSION_COOKIE}={}; {CSRF_COOKIE}={}",
            self.cookie, self.csrf
        )
    }
}

/// The record **key** half of a `RecordId`, which is what `type::record('table', $key)` binds.
///
/// Duplicated from storage's crate-private `record_key` rather than re-derived from
/// [`record_id_to_string`], because that formatter is a display form and is explicitly **not** a
/// parser-safe round trip for numeric or quoted opaque keys (`AGENTS.md`) — a fixture that split a string
/// could aim its own UPDATE at the wrong row and then "prove" a quarantine that was never written.
fn record_key_of(id: &surrealdb::types::RecordId) -> String {
    use surrealdb::types::RecordIdKey;
    match &id.key {
        RecordIdKey::String(text) => text.clone(),
        RecordIdKey::Number(number) => number.to_string(),
        RecordIdKey::Uuid(uuid) => uuid.to_string(),
        other => format!("{other:?}"),
    }
}

/// Percent-encode one form value, on the `application/x-www-form-urlencoded` rules.
fn form_encoded(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// One digest, paid for once per test binary: the policy costs ~19 MiB per hash and these suites enrol
/// several owners.
async fn digest() -> &'static PasswordDigest {
    static DIGEST: tokio::sync::OnceCell<PasswordDigest> = tokio::sync::OnceCell::const_new();
    DIGEST
        .get_or_init(|| async {
            AuthRepo::create_password_digest(PASSWORD)
                .await
                .expect("the policy must produce a digest")
        })
        .await
}

impl AuthenticatedTestApp {
    /// A fixture over an **already built** server, so a suite keeps the embedding stub and the
    /// retrieve floor it constructed the server with (`banded_server()`'s floor behaviour depends on
    /// exactly that) while still reaching the pages as an authenticated owner.
    pub(super) async fn on_server(server: AlexandriaServer, login: &str) -> Self {
        Self::on_server_with(server, login, None).await
    }

    /// The same, with a suite's own [`DebugContext`] mounted, so the dashboard's config-panel cases keep
    /// describing their diagnostics per case and still reach the page as an owner.
    pub(super) async fn on_server_with(
        server: AlexandriaServer,
        login: &str,
        ctx: Option<DebugContext>,
    ) -> Self {
        let db = Arc::clone(&server.db);
        let service = Arc::new(AuthService::admitting(
            Arc::clone(&db),
            AuthEvents::without_sessions(),
        ));
        // The router has to own *this* server: a suite that passed `banded_server()` must keep its
        // retrieve floor, and the debug pages read that floor off the state the router carries.
        let router = super::router_secured(server, ctx, boundary_for(&db, &service));
        // **One fixture owner per store.** Several suites build more than one fixture over the same
        // server (a page helper plus a case-local control), and the adoption layer claims each orphan
        // row for whichever owner asked first — so a second fixture would render an empty corpus and
        // turn "the page shows the member" into a 404, or worse, into a vacuous pass. Signing in
        // before enrolling makes the store's fixture owner stable and identical across every call.
        let _owner_account = Self::enroll_or_sign_in(&db, login).await;
        let session = Self::sign_in_on(&router, &service, login).await;
        let owner = AccessRepo::new(&db)
            .authorize(&Self::principal_on(&service, &session).await)
            .await
            .expect("the fixture principal authorizes");
        Self {
            router,
            db,
            service,
            session,
            removed: Arc::new(std::sync::Mutex::new(Vec::new())),
            owner,
        }
    }

    /// The full build: `with_diagnostics` chooses whether the dashboard's [`DebugContext`] is supplied,
    /// which is the pair the guard-independence cases assert.
    async fn built(login: &str, with_diagnostics: bool) -> Self {
        Self::signed_in(login, with_diagnostics, &[HOST]).await
    }

    async fn signed_in(login: &str, with_diagnostics: bool, allowed_hosts: &[&str]) -> Self {
        let mut db = Database::connect_embedded()
            .await
            .expect("an in-memory store");
        schema::migrate(db.inner()).await.expect("the schema");
        let _system = db
            .claim_system_authority()
            .expect("a fresh store issues its startup capability once");
        let db = Arc::new(db);
        let (router, service, db) = Self::router_on_store(&db, with_diagnostics, allowed_hosts);
        Self::enroll_on(&db, login).await;
        let session = Self::sign_in_on(&router, &service, login).await;
        let owner = AccessRepo::new(&db)
            .authorize(&Self::principal_on(&service, &session).await)
            .await
            .expect("the fixture principal authorizes");
        Self {
            router,
            db,
            service,
            session,
            removed: Arc::new(std::sync::Mutex::new(Vec::new())),
            owner,
        }
    }

    /// The pieces a caller needs before it can sign anybody in: the same router `main.rs` mounts, plus
    /// the service the fixture uses for the storage-side half of its assertions.
    fn router_on_store(
        db: &Arc<Database>,
        with_diagnostics: bool,
        allowed_hosts: &[&str],
    ) -> (axum::Router, Arc<AuthService>, Arc<Database>) {
        let service = Arc::new(AuthService::admitting(
            Arc::clone(db),
            AuthEvents::without_sessions(),
        ));
        let _ = &service;
        let service = Arc::new(AuthService::admitting(
            Arc::clone(db),
            AuthEvents::without_sessions(),
        ));
        let boundary = boundary_for(db, &service);
        let server = AlexandriaServer::new(
            Arc::clone(db),
            Arc::new(StubEmbedding),
            0.75,
            crate::server::HeatSettings::default(),
        );
        let ctx = with_diagnostics.then(|| DebugContext {
            embedding_model: "stub".to_string(),
            embedding_device: "cpu".to_string(),
            transport: "http".to_string(),
            bind_host: "127.0.0.1".to_string(),
            bind_port: 3000,
            data_dir: ":memory:".to_string(),
            cluster_merge_threshold: 0.7,
            dreaming_summary: String::new(),
            dreaming_liveness: None,
            allowed_hosts: allowed_hosts.iter().map(|host| host.to_string()).collect(),
        });
        let router = super::router_secured(server, ctx, boundary);
        (router, service, Arc::clone(db))
    }

    fn principal_on(
        service: &Arc<AuthService>,
        session: &Session,
    ) -> impl std::future::Future<Output = alexandria_storage::access::Principal> {
        let service = Arc::clone(service);
        let cookie = session.cookie.clone();
        async move {
            service
                .authenticate_browser(&cookie, Utc::now())
                .await
                .expect("the fixture session authenticates")
        }
    }

    // -- seeding, through the scoped writers the pages read ------------------------------------------

    /// The caller's own corpus population: Personal plus every project, which is the one selection an
    /// owner-facing debug page is allowed to resolve.
    /// The store's fixture owner, created once and reused: enrols `login` only when no account by that
    /// login exists yet, and otherwise returns the row the first fixture already made.
    ///
    /// It reads through `AuthRepo::account_by_login` rather than issuing its own `SELECT`, because storage
    /// owns database access and this fixture lives in the MCP crate.
    async fn enroll_or_sign_in(db: &Arc<Database>, login: &str) -> AccountId {
        let existing = AuthRepo::new(db.inner())
            .account_by_login(login)
            .await
            .expect("a lookup of the fixture's own login");
        match existing.and_then(|account| account.id) {
            Some(id) => AccountId::try_from(id)
                .unwrap_or_else(|_| panic!("the fixture's account row is an account record")),
            None => Self::enroll_on(db, login).await,
        }
    }

    async fn enroll_on(db: &Arc<Database>, login: &str) -> AccountId {
        IdentityRepo::new(db.inner())
            .bootstrap_owner(
                login,
                digest().await.as_str(),
                &OperationId::new(format!("debug-{login}")),
            )
            .await
            .unwrap_or_else(|error| panic!("enrolling `{login}`: {error:?}"))
            .account
    }

    /// Signs in **through the router**, so the fixture's session is exactly what a browser's would be:
    /// the pre-auth token comes from the rendered page, the credentials travel as a form post, and the
    /// cookies are read back off the response the guard set. A fixture that minted a session straight
    /// from storage would never exercise the half of Task 9 that these suites exist to cover.
    async fn sign_in_on(router: &axum::Router, service: &Arc<AuthService>, login: &str) -> Session {
        let token = preauth_token(router).await;
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/login")
                    .header(HOST_HEADER, HOST)
                    .header("origin", ORIGIN)
                    .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "login={login}&password={PASSWORD}&csrf={token}"
                    )))
                    .expect("a valid request"),
            )
            .await
            .expect("the login page is served");
        let status = response.status();
        let headers = response.headers().clone();
        let refusal = body_of(response).await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "a valid login must open a session (headers: {headers:?}, body: {refusal})"
        );
        let cookie = set_cookie(&headers, SESSION_COOKIE)
            .unwrap_or_else(|| panic!("no session cookie was set: {headers:?}"));
        let csrf = set_cookie(&headers, CSRF_COOKIE)
            .unwrap_or_else(|| panic!("no CSRF cookie was set: {headers:?}"));
        // The bare token, exactly as the guard presents it: `authenticate_browser` takes the session
        // credential itself, not a `Cookie` header — the guard splits the header first (`web.rs`
        // `cookie_value`), and handing it the whole header is the shape that reads as `NotFound`.
        let session = Session { cookie, csrf };
        service
            .authenticate_browser(&session.cookie, Utc::now())
            .await
            .unwrap_or_else(|error| panic!("the minted session authenticates: {error:?} cookie={:?} csrf={:?} raw-set-cookie={:?}", session.cookie, session.csrf, headers.get_all(axum::http::header::SET_COOKIE)));
        session
    }

    pub(super) async fn population(&self) -> ScopedContext {
        AccessRepo::new(&self.db)
            .select(&self.owner, ReadSelection::Assigned)
            .await
            .expect("the owner's own corpus resolves")
    }

    /// One live fact in the caller's Personal partition, returned as its record id.
    pub(super) async fn seed_personal(
        &self,
        content: &str,
    ) -> (String, surrealdb::types::RecordId) {
        self.seed_into(&self.owner.clone(), content).await
    }

    /// One live, **quarantined** fact in the caller's Unassigned partition, as a record id.
    ///
    /// The state a collapse or an operator leaves behind, and the one Task 11's review view exists for: a
    /// proposal about it must file the memory **without** clearing the quarantine, so the fixture has to be
    /// able to make it. Written through the review population rather than a raw `CREATE`, so the row carries
    /// the owner and workspace stamps a scoped reader requires — a row with no `account` is invisible to
    /// `ScopeFilter`, and a test asserting "the owner sees it" against such a row would pass vacuously the
    /// moment the assertion were flipped to "nothing".
    pub(super) async fn seed_quarantined_unassigned(
        &self,
        content: &str,
    ) -> (String, surrealdb::types::RecordId) {
        let read = AccessRepo::new(&self.db)
            .select(&self.owner, ReadSelection::UnassignedReview)
            .await
            .expect("the review population resolves");
        let fact = ScopedMemoryRepo::new(self.db.inner(), &self.owner, &read)
            .create_fact(
                content,
                0.5,
                &[0.1, 0.2],
                &[],
                WriteTarget::Unassigned,
                Utc::now(),
            )
            .await
            .expect("seeding a review fact")
            .id
            .expect("a created fact has an id");
        self.db
            .inner()
            .query("UPDATE type::record('fact', $key) SET quarantined_at = $at WHERE id = $id;")
            .bind(("key", record_key_of(&fact)))
            .bind(("id", fact.clone()))
            .bind(("at", Utc::now()))
            .await
            .expect("quarantining")
            .check()
            .expect("the quarantine write ran");
        (alexandria_storage::record_id_to_string(&fact), fact)
    }

    async fn seed_into(
        &self,
        owner: &OwnerContext,
        content: &str,
    ) -> (String, surrealdb::types::RecordId) {
        let read = AccessRepo::new(&self.db)
            .select(owner, ReadSelection::Assigned)
            .await
            .expect("the owner's own corpus resolves");
        let id = ScopedMemoryRepo::new(self.db.inner(), owner, &read)
            .create_fact(
                content,
                0.5,
                &[0.1, 0.2],
                &[],
                WriteTarget::Personal,
                Utc::now(),
            )
            .await
            .expect("a seeded fact")
            .id
            .expect("a created fact has an id");
        (record_id_to_string(&id), id)
    }

    /// The caller's own request scope: the pair a journaled writer is admitted with.
    ///
    /// A fixture needs this to drive a *service* entry point directly (the delete-authority cases compare
    /// the tool path's refusal against the page path's permission), and the only honest way to build one is
    /// from the same two contexts a browser session resolves.
    pub(super) async fn corpus_scope(&self) -> crate::server::RequestScope {
        crate::server::RequestScope::issued_for_debug(self.owner.clone(), self.population().await)
    }

    /// A server over the same store and the same stub embedding the router carries.
    pub(super) fn server(&self) -> crate::AlexandriaServer {
        crate::AlexandriaServer::new(
            Arc::clone(&self.db),
            Arc::new(StubEmbedding),
            0.75,
            crate::server::HeatSettings::default(),
        )
    }

    /// One of the caller's own projects, established for the cases that move a memory into one.
    ///
    /// `establish` is storage's create, reached through the scoped repo, so the row carries the caller's
    /// account and workspace and the resolver can find it by handle — which is what a *page* posts back.
    pub(super) async fn own_project(&self, alias: &str) -> String {
        let established = ScopedProjectRepo::new(self.db.inner(), &self.owner)
            .establish(alias, None)
            .await
            .expect("establishing a fixture project");
        record_id_to_string(established.project.id.as_record_id())
    }

    /// Attach one of the caller's facts to one of their sessions, the way a store's link does.
    pub(super) async fn link_session(&self, external_id: &str, fact_id: &str) {
        ScopedSessionRepo::new(self.db.inner(), self.owner.clone())
            .add_memory(external_id, fact_id)
            .await
            .expect("linking a fixture memory to a session");
    }

    /// Stamp tags onto one of the caller's rows, for the cases that must see a tag set survive an edit.
    ///
    /// A raw write, deliberately: seeding tags through the *journaled* update path would make the case that
    /// checks what the journaled update path does to tags depend on itself.
    pub(super) async fn set_tags(&self, fact_id: &str, tags: &[String]) {
        let target = RecordId::parse_simple(fact_id).expect("a fixture fact handle");
        self.db
            .inner()
            .query("UPDATE type::record('fact', $key) SET tags = $tags WHERE id = $id;")
            .bind(("key", record_key_of(&target)))
            .bind(("id", target))
            .bind(("tags", tags.to_vec()))
            .await
            .expect("stamping tags")
            .check()
            .expect("the stamp ran");
    }

    /// Which partitions a session's live memories currently sit in, as the archive renders them.
    pub(super) async fn session_scopes(&self, external_id: &str) -> Vec<String> {
        ScopedSessionRepo::new(self.db.inner(), self.owner.clone())
            .archive(50, 0)
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|row| row.external_id == external_id)
            .map(|row| row.scopes)
            .unwrap_or_default()
    }

    /// The revision a row carries *now*, as the page would have rendered it.
    ///
    /// Read from the store rather than from a rendered row because a fixture host that has to print the
    /// setup cannot also drive the browser to read a column; the journey itself then verifies the rendered
    /// value by posting it.
    pub(super) async fn semantic_revision(&self, fact_id: &str) -> u64 {
        let target = RecordId::parse_simple(fact_id).expect("a fixture fact handle");
        let rows: Vec<serde_json::Value> = self
            .db
            .inner()
            .query(
                "SELECT semantic_revision FROM type::record('fact', $key) WHERE id = $id LIMIT 1;",
            )
            .bind(("key", record_key_of(&target)))
            .bind(("id", target))
            .await
            .expect("reading the revision")
            .check()
            .expect("the revision read ran")
            .take(0)
            .expect("the revision decodes");
        rows.first()
            .and_then(|row| row["semantic_revision"].as_i64())
            .unwrap_or(0)
            .max(0) as u64
    }

    /// File one of the caller's rows into a partition, through the journaled move the page uses.
    ///
    /// Goes through `MutationRepo::move_scope` rather than an `UPDATE` because a fixture that hand-wrote
    /// `scope` would produce a row no journal describes, and the archive/move journeys would then be
    /// observing fixture noise instead of product state.
    pub(super) async fn move_row(
        &self,
        fact_id: &str,
        destination: MemoryScope,
        key: &str,
    ) -> bool {
        let target = match RecordId::parse_simple(fact_id) {
            Ok(target) => target,
            Err(_) => return false,
        };
        let Ok(fact) = alexandria_storage::models::FactId::try_from(target) else {
            return false;
        };
        let revision = self.semantic_revision(fact_id).await;
        alexandria_storage::repos::MutationRepo::new(self.db.inner())
            .move_scope(
                &self.owner,
                fact,
                Version::new(revision),
                destination,
                &OperationId::new(key.to_string()),
            )
            .await
            .is_ok()
    }

    /// Move one row's semantic revision forward behind the page's back — a second writer won the race.
    ///
    /// This is the only way to test a stale-conflict control against *the revision the page rendered*:
    /// bumping the store without touching the form's copy is the whole scenario, and re-reading the
    /// current version at POST time is the bug this pins shut.
    pub(super) async fn bump_semantic_revision(&self, fact_id: &str) {
        let target = RecordId::parse_simple(fact_id).expect("a fixture fact handle");
        self.db
            .inner()
            .query(
                "UPDATE type::record('fact', $key) SET semantic_revision = (semantic_revision \
                 + 1) WHERE id = $id;",
            )
            .bind(("key", record_key_of(&target)))
            .bind(("id", target))
            .await
            .expect("bumping the revision")
            .check()
            .expect("the bump ran");
    }

    /// A session with one live memory attached and a summary, which is what "finalized" means here.
    pub(super) async fn seed_session(&self, external_id: &str, summary: Option<&str>) {
        let repo = ScopedSessionRepo::new(self.db.inner(), self.owner.clone());
        repo.find_or_create(external_id, Some("fixture-agent"), Some("fixture-model"))
            .await
            .expect("a seeded session");
        if let Some(summary) = summary {
            repo.finalize(external_id, Some(summary), None)
                .await
                .expect("a seeded session finalizes");
        }
    }

    pub(super) async fn seed_cluster(
        &self,
        label: Option<&str>,
        centroid: &[f32],
    ) -> surrealdb::types::RecordId {
        let read = self.population().await;
        let repo = ScopedClusterRepo::new(self.db.inner(), &self.owner, &read);
        repo.create(label, centroid, Utc::now())
            .await
            .expect("a seeded cluster")
            .id
            .expect("a created cluster has an id")
    }

    /// Fully typed, matching the production `ClusterRepo` boundary — the deferred conversion
    /// from run 52 is now gone: no display string is parsed anywhere in the fixture support.
    pub(super) async fn add_cluster_member(
        &self,
        cluster: &surrealdb::types::RecordId,
        fact: &surrealdb::types::RecordId,
    ) {
        ClusterRepo::new(self.db.inner())
            .add_member(cluster, fact)
            .await
            .expect("a seeded membership");
    }

    /// A second owner, with their own authorized context: every privacy assertion needs a foreign row
    /// that really is somebody's, so "no foreign row appears" is a measurement and not a tautology.
    /// An administrator: the role write is the operator's, through the row the store reports, and the
    /// capability under test still comes only from `authorize_admin` re-reading it.
    /// A **second, distinct owner** over the same store, signed in and ready to request pages.
    ///
    /// This is what makes an isolation assertion mean something: seeding everything as one owner and
    /// then checking "the other owner's row is absent" is vacuous, because there is no other owner.
    /// The new account is enrolled through `IdentityRepo` (storage owns the write) with its own login,
    /// so `enroll_or_sign_in`'s per-store reuse does not collapse it back onto the first fixture, and
    /// its rows are never adopted: adoption claims only `account = NONE` rows, so whatever this owner
    /// writes stays this owner's, which is exactly the population the first owner must not see.
    pub(super) async fn second_owner(&self, login: &str) -> Self {
        let digest = AuthRepo::create_password_digest(PASSWORD)
            .await
            .expect("a fixture password digest");
        IdentityRepo::new(self.db.inner())
            .bootstrap_owner(
                login,
                digest.as_str(),
                &OperationId::new(format!("debug-{login}")),
            )
            .await
            .unwrap_or_else(|error| panic!("enrolling the second owner `{login}`: {error:?}"));
        let session = Self::sign_in_on(&self.router, &self.service, login).await;
        let owner = AccessRepo::new(&self.db)
            .authorize(&Self::principal_on(&self.service, &session).await)
            .await
            .expect("the second owner authorizes");
        Self {
            router: self.router.clone(),
            db: Arc::clone(&self.db),
            service: Arc::clone(&self.service),
            session,
            owner,
            removed: Arc::clone(&self.removed),
        }
    }

    // -- requests ---------------------------------------------------------------------------------

    /// A signed-in `GET`.
    pub(super) async fn get(&self, path: &str) -> (StatusCode, String) {
        self.get_with_cookie(path, &self.session.cookie_header())
            .await
    }

    /// A `GET` under an explicit `Host`, which is how a case asserts the rebinding refusal without
    /// having to forge a cookie.
    pub(super) async fn get_with_host(
        &self,
        path: &str,
        host: &str,
        session: Option<&Session>,
    ) -> Response {
        let mut builder = Request::builder()
            .method("GET")
            .uri(path)
            .header(HOST_HEADER, host);
        if let Some(session) = session {
            builder = builder.header(
                COOKIE,
                session
                    .cookie_header()
                    .parse::<axum::http::HeaderValue>()
                    .expect("a cookie header"),
            );
        }
        self.router
            .clone()
            .oneshot(builder.body(Body::empty()).expect("a valid request"))
            .await
            .expect("the router served")
    }

    /// A `GET` with an explicit `Cookie` header and an explicit `Sec-Fetch-Site` value.
    pub(super) async fn get_with_site(&self, path: &str, site: &str) -> (StatusCode, String) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .header(HOST_HEADER, HOST)
                    .header(
                        COOKIE,
                        self.session
                            .cookie_header()
                            .parse::<axum::http::HeaderValue>()
                            .expect("cookie"),
                    )
                    .header("sec-fetch-site", site)
                    .body(Body::empty())
                    .expect("a valid request"),
            )
            .await
            .expect("the router served");
        let status = response.status();
        (status, body_of(response).await)
    }

    /// A mutation with an explicit `Sec-Fetch-Site` value and the session's own token.
    pub(super) async fn post_with_site(
        &self,
        path: &str,
        fields: &str,
        site: &str,
    ) -> (StatusCode, String) {
        self.post_with(
            "POST",
            path,
            &format!("{fields}&csrf={}", self.session.csrf),
            site,
        )
        .await
    }

    async fn post_with(
        &self,
        _method: &str,
        path: &str,
        body: &str,
        site: &str,
    ) -> (StatusCode, String) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(HOST_HEADER, HOST)
                    .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("origin", ORIGIN)
                    .header("sec-fetch-site", site)
                    .header(
                        COOKIE,
                        self.session
                            .cookie_header()
                            .parse::<axum::http::HeaderValue>()
                            .expect("cookie"),
                    )
                    .body(Body::from(body.to_string()))
                    .expect("a valid request"),
            )
            .await
            .expect("the router served");
        let status = response.status();
        (status, body_of(response).await)
    }

    /// A `GET` with an explicit `Cookie` header, for the cases that need a session other than the
    /// fixture's own or no cookie at all.
    /// A `GET` with the fixture's router but **no cookie at all** — the shape an anonymous visitor
    /// makes. Kept apart from `get_with_cookie(path, "")` because the latter still runs the adoption
    /// layer, which would let an "anonymous" case quietly own the corpus it is supposed to be denied.
    pub(super) async fn get_anonymous(&self, path: &str) -> (StatusCode, String) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .header(HOST_HEADER, HOST)
                    .body(Body::empty())
                    .expect("a valid request"),
            )
            .await
            .expect("the router served");
        let status = response.status();
        (status, body_of(response).await)
    }

    pub(super) async fn get_with_cookie(&self, path: &str, cookie: &str) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("GET")
            .uri(path)
            .header(HOST_HEADER, HOST);
        if !cookie.is_empty() {
            builder = builder.header(
                COOKIE,
                cookie
                    .parse::<axum::http::HeaderValue>()
                    .expect("a cookie header"),
            );
        }
        self.round_trip(builder.body(Body::empty()).expect("a valid request"))
            .await
    }

    /// A signed-in mutation with the token the fixture holds.
    pub(super) async fn post(&self, path: &str, fields: &[(&str, &str)]) -> (StatusCode, String) {
        // Values are percent-encoded the way a browser encodes them. `format!("{k}={v}")` unencoded is a
        // trap for any field carrying a record handle: `classification_proposal:x:0` contains `:` and the
        // form decoder then sees a value it cannot split back out, so a perfectly lawful decision reads as a
        // 422 and the test blames the handler.
        let mut body = fields
            .iter()
            .map(|(k, v)| format!("{k}={}", form_encoded(v)))
            .collect::<Vec<_>>()
            .join("&");
        if !body.is_empty() {
            body.push('&');
        }
        body.push_str(&format!("csrf={}", self.session.csrf));
        self.post_with_cookie(path, &body, &self.session.cookie_header())
            .await
    }

    /// A mutation whose body is spelled as **pairs that may repeat**, exactly as a browser sends a form.
    ///
    /// [`Self::post`] takes `&[(&str, &str)]` and joins them, so a caller *can* already emit a repeated
    /// key — but nothing in the suite did, which is how a two-checkbox approval went untested while eight
    /// single-checkbox tests passed. This shape states the wire format once (percent-encoding plus the
    /// token as the final pair, which is what a page's `<form>` with a hidden `csrf` produces) and is the
    /// only way these suites post a multi-value field.
    pub(super) async fn post_form(
        &self,
        path: &str,
        pairs: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let (status, _, _) = self.post_form_located(path, pairs).await;
        (status, String::new())
    }

    /// The same post, answered with the **redirect target** rather than an empty 303 body.
    ///
    /// A `See Other` carries its whole message in `Location`, so an assertion about *what the owner was
    /// told* — "2 moved", "select at least one proposal", "that memory changed since this page loaded" —
    /// has to read the header. Returning only `(status, body)` is what forced the first version of these
    /// cases to re-fetch a hand-guessed query string, which tests the page's renderer rather than the
    /// handler's answer.
    pub(super) async fn post_form_located(
        &self,
        path: &str,
        pairs: &[(&str, &str)],
    ) -> (StatusCode, String, String) {
        let mut body = pairs
            .iter()
            .map(|(key, value)| format!("{key}={}", form_encoded(value)))
            .collect::<Vec<_>>()
            .join("&");
        if !body.is_empty() {
            body.push('&');
        }
        body.push_str(&format!("csrf={}", self.session.csrf));
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(HOST_HEADER, HOST)
                    .header("origin", ORIGIN)
                    .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(
                        COOKIE,
                        self.session
                            .cookie_header()
                            .parse::<axum::http::HeaderValue>()
                            .expect("cookie"),
                    )
                    .body(Body::from(body))
                    .expect("a valid request"),
            )
            .await
            .expect("the router served");
        let status = response.status();
        let location = response
            .headers()
            .get(axum::http::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        (status, location, body_of(response).await)
    }

    /// A mutation whose body is **exactly** the bytes given, with no token appended.
    ///
    /// `post_form` adds the fixture's own CSRF pair, which is a convenience until a test has to prove a
    /// rendered form is *self-sufficient*: the B1 defect is a control the page never emitted, and a harness
    /// that supplies the missing field from its own copy passes while the browser keeps 422-ing. A caller
    /// that posts extracted-from-the-page fields must be able to post them whole, so the shape a real
    /// browser sends — page-rendered `csrf` and all — is expressible here.
    pub(super) async fn post_exact(
        &self,
        path: &str,
        body: String,
    ) -> (StatusCode, String, String) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(HOST_HEADER, HOST)
                    .header("origin", ORIGIN)
                    .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(
                        COOKIE,
                        self.session
                            .cookie_header()
                            .parse::<axum::http::HeaderValue>()
                            .expect("cookie"),
                    )
                    .body(Body::from(body))
                    .expect("a valid request"),
            )
            .await
            .expect("the router served");
        let status = response.status();
        let location = response
            .headers()
            .get(axum::http::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        (status, location, body_of(response).await)
    }

    /// A mutation with the body spelled out, for the cases that must omit or corrupt the token.
    pub(super) async fn post_with_cookie(
        &self,
        path: &str,
        body: &str,
        cookie: &str,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header(HOST_HEADER, HOST)
            .header("origin", ORIGIN)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded");
        if !cookie.is_empty() {
            builder = builder.header(COOKIE, cookie);
        }
        self.round_trip(
            builder
                .body(Body::from(body.to_string()))
                .expect("a valid request"),
        )
        .await
    }

    /// The bare secured router, with no cookie and no adoption layer: for the cases that assert a
    /// public surface answers to an anonymous reader.
    pub(super) fn router_only(&self) -> axum::Router {
        self.router.clone()
    }

    /// A router that carries this session on every request, so a migrated test changes one line
    /// rather than every request builder it contains.
    ///
    /// The layer runs **outside** the guard, and it does two things per request: attach the fixture's
    /// own cookie pair, and adopt any row the test seeded through the unscoped writers into this
    /// owner's population. The second half is a fixture standing in for Task 14's ownership backfill —
    /// a row written before a store has owners is migration-pending, and every scoped reader in this
    /// crate deliberately refuses to see it, so a test that seeds with `MemoryRepo::create_fact`
    /// would otherwise render an honest empty page and pass for the wrong reason. Running it per
    /// request means a test that seeds between two reads still sees its own second row.
    pub(super) fn session_router(&self) -> axum::Router {
        let cookie = self.session.cookie_header();
        let db = Arc::clone(&self.db);
        let owner = self.owner.clone();
        let removed = Arc::clone(&self.removed);
        super::Router::new()
            .fallback_service(self.router.clone())
            .layer(axum::middleware::from_fn(
                move |mut request: Request, next: axum::middleware::Next| {
                    let db = Arc::clone(&db);
                    let owner = owner.clone();
                    let cookie = cookie.clone();
                    // Cloned *before* the async block, as the others are: referring to the capture
                    // inside `async move` would move it into the future and make this closure FnOnce,
                    // which `layer` rejects.
                    let removed = Arc::clone(&removed);
                    async move {
                        // Re-read per request, so a `drop_table` between requests is seen.
                        let skipped = removed.lock().unwrap().clone();
                        adopt_unowned_rows(&db, &owner, &skipped).await;
                        let headers = request.headers_mut();
                        headers.insert(COOKIE, cookie.parse().expect("the fixture cookie pair"));
                        // The `Host` half too: a pre-Task 9 test built its request with only a path,
                        // which axum turns into no `Host` header at all, and an authenticated policy
                        // refuses that. Injecting it here is the fixture standing in for the transport
                        // those request builders never went through — not a guard that was turned off,
                        // since every real Host check still runs on the value.
                        if !headers.contains_key(HOST_HEADER) {
                            headers.insert(HOST_HEADER, HOST.parse().expect("the fixture host"));
                        }
                        // A migrated test posts a form it wrote before the boundary existed, with no
                        // token in it. The header carries the fixture's own token, which is the same
                        // value its cookie holds — so the mutation satisfies the guard the way a page
                        // this server rendered would, and a *wrong* token is still refused below.
                        if let Some(token) = csrf_token(&cookie) {
                            headers.insert(
                                CSRF_HEADER,
                                token.parse().expect("a token is a header value"),
                            );
                        }
                        next.run(request).await
                    }
                },
            ))
    }

    /// The same, for a suite that wants a page as a *different* owner: the cookie is supplied and the
    /// adoption targets the caller's context.
    /// Removes one table from the fixture's store, so a suite can make a real storage read fail rather
    /// than pretend to. The `IF OVERWRITE`-free spelling is the one this engine accepts for a removal of
    /// a defined table, and the failure a handler then sees is the engine's own.
    pub(super) async fn drop_table(&self, table: &str) {
        self.removed.lock().unwrap().push(table.to_string());
        let mut response = self
            .db
            .inner()
            .query(format!("REMOVE TABLE {table};"))
            .await
            .expect("a removable table")
            .check()
            .expect("the removal");
        let _: Vec<serde_json::Value> = response.take(0).unwrap_or_default();
    }

    async fn round_trip(&self, request: Request) -> (StatusCode, String) {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("the router served");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a readable body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// The signed-in fixture over a server the suite already built, so its embedding stub and thresholds
/// survive the migration to an authenticated page.
pub(super) async fn authenticated_on_server(server: AlexandriaServer) -> AuthenticatedTestApp {
    AuthenticatedTestApp::on_server(server, "debug-fixture-owner").await
}

/// A fixture that carries a suite's own [`DebugContext`] — the dashboard's config-panel tests build one
/// per case, and the diagnostics must survive the move to an authenticated router without the test
/// having to re-describe the policy.
pub(super) async fn authenticated_with_context(
    server: AlexandriaServer,
    ctx: DebugContext,
) -> AuthenticatedTestApp {
    AuthenticatedTestApp::on_server_with(server, "debug-fixture-owner", Some(ctx)).await
}

/// The legacy, no-auth router: `debug::router(server)` with a server of the caller's own.
pub(super) async fn no_context_router() -> axum::Router {
    let server = test_server().await;
    super::router(server)
}

/// The legacy build *with* diagnostics and a wildcard host list — the configuration that used to mean
/// "no Host check and no auth check", and now means only "no Host check", because there is nothing
/// behind it to read.
pub(super) async fn legacy_router_with_wildcard_hosts() -> axum::Router {
    let server = test_server().await;
    super::router_with_context(
        server,
        Some(DebugContext {
            embedding_model: "stub".to_string(),
            embedding_device: "cpu".to_string(),
            transport: "http".to_string(),
            bind_host: "127.0.0.1".to_string(),
            bind_port: 3000,
            data_dir: ":memory:".to_string(),
            cluster_merge_threshold: 0.7,
            dreaming_summary: String::new(),
            dreaming_liveness: None,
            allowed_hosts: vec!["*".to_string()],
        }),
    )
}

/// The number on a stats row rendered as `<tr><th>{label}</th><td>{n}</td></tr>`, which is the shape
/// every dashboard rollup uses. Reading the labelled row rather than scanning the page for a digit is
/// what makes a count assertion mean the count: a dashboard also carries timestamps and config values,
/// so "does 3 appear anywhere" fails on an honest page and passes on a wrong one.
pub(super) fn stat_value(page: &str, label: &str) -> Option<usize> {
    let (_, rest) = page.split_once(&format!("<th>{label}</th><td>"))?;
    let (digits, _) = rest.split_once('<')?;
    digits.trim().parse().ok()
}

/// A failure line, not a document.
pub(crate) fn clip(body: &str) -> String {
    const MAX: usize = 240;
    let flat: String = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if flat.len() <= MAX {
        return flat;
    }
    format!("{}… (+{} bytes)", &flat[..MAX], flat.len() - MAX)
}

async fn body_of(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a readable body");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The signed-in fixture: a real account, a real session minted by the router, and the same build
/// `main.rs` mounts.
pub(super) async fn authenticated_app(with_diagnostics: bool) -> AuthenticatedTestApp {
    AuthenticatedTestApp::built("debug-fixture-owner", with_diagnostics).await
}

/// One cookie off a response's `Set-Cookie` headers, by name — the value a browser keeps and sends
/// back, without the attributes.
fn set_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with(&format!("{name}=")))
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .split_once('=')
                .map(|(_, v)| v.to_string())
                .unwrap_or_default()
        })
}

/// The boundary the fixture's router serves under: the same `AuthMiddleware` gate `/mcp` admits
/// through, so login throttling and the revocation bus are shared rather than re-simulated.
///
/// `ORIGIN` is a loopback https origin and the policy is built with `WebPolicy::hosted`, which is the
/// production constructor; the fixture deliberately does **not** use the loopback-development
/// exception, so every cookie the tests observe carries `Secure` and a test that relied on its absence
/// would fail loudly here instead of drifting.
fn boundary_for(db: &Arc<Database>, service: &Arc<AuthService>) -> WebBoundary {
    let policy = WebPolicy::hosted(ORIGIN, &[HOST.to_string()], &[ORIGIN.to_string()], false)
        .expect("the fixture origin is a well-formed hosted origin");
    // Built in the order `main.rs` builds it, over the same service and bus: the events the gate
    // carries have to be the events `AuthService` publishes on, or a revocation would silence the
    // MCP streams and leave the browser sessions reading as still authorized.
    let gate = AuthMiddleware::admitting(
        Arc::clone(service),
        Admission::bound_to(service.events()),
        McpSessionRegistry::over(&service.events()),
    );
    WebBoundary::new(gate, Arc::clone(db), policy)
}

/// A pre-auth token for `/debug/login`, earned by asking the router for the page rather than by
/// minting one: the token is issued by the boundary, is page-scoped and single-use, and a fixture that
/// conjured one would not be testing the same thing a browser does.
///
/// It is the form's `csrf` field that carries it — the guard reads a public-page mutation through the
/// same `csrf_form_field` it uses for authenticated ones, so the field name is shared and the *store*
/// behind it differs (a spent-one-use page token here, a session pair there). The `page` input is a
/// label the handler echoes back, not the secret.
async fn preauth_token(router: &axum::Router) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/debug/login")
                .header(HOST_HEADER, HOST)
                .body(Body::empty())
                .expect("a valid request"),
        )
        .await
        .expect("the login page served");
    let body = body_of(response).await;
    hidden_field(&body, "csrf").expect("the login page carries a pre-auth token")
}

/// The CSRF half of a cookie pair, which is the only form in which a test can read it back.
fn csrf_token(cookie_header: &str) -> Option<String> {
    cookie_header
        .split("; ")
        .find_map(|pair| pair.strip_prefix(&format!("{CSRF_COOKIE}=")))
        .map(str::to_string)
}

/// `value="…"` of `<input type="hidden" name="{field}" …>`, tolerant of attribute order.
fn hidden_field(body: &str, field: &str) -> Option<String> {
    let needle = format!("name=\"{field}\"");
    let (_, after) = body.split_once(&needle)?;
    let (_, rest) = after.split_once("value=\"")?;
    rest.split_once('"').map(|(value, _)| value.to_string())
}

/// Adopts rows that carry no owner for the fixture's owner.
///
/// Pre-Task 9 debug fixtures wrote facts, sessions and clusters through the unscoped repos, which
/// leave `account = NONE`. Task 9's reads are population-filtered, so an unmigrated page test would
/// render an empty corpus and *every* isolation assertion would pass vacuously. Claiming the orphans
/// once per request — the `session_router()` layer does this — keeps those controls useful. Adoption
/// requires every ownership marker to be absent, not merely a missing scope or account. The sole
/// scope-repair case already matches this owner's account AND workspace with no project. Foreign or
/// partial ownership is preserved exactly so this helper cannot hide a leak by reassigning its row.
/// This is test-only compatibility, not Task 14's production migration policy.
async fn adopt_unowned_rows(
    db: &Arc<Database>,
    owner: &alexandria_storage::access::OwnerContext,
    removed: &[String],
) {
    let account = owner.account().as_record_id().clone();
    let workspace = owner.workspace().as_record_id().clone();
    // One statement per table, and only for tables that still exist: a single multi-statement query
    // fails whole when one table is gone, which would turn a deliberate removal (a storage-failure
    // case) into a panic inside the fixture, before the handler's own status could be observed.
    for table in ["fact", "session", "cluster", "raw"] {
        if removed.iter().any(|gone| gone == table) {
            continue;
        }
        // A fact needs its `scope` stamped as well: the scoped reads this router serves filter on the
        // scope column, so a row that is owned but scope-less would still answer "not found" and the
        // page tests would be asserting against a corpus they cannot see.
        let sql = match table {
            "fact" => {
                "UPDATE fact SET account = $a, workspace = $w, scope = 'personal' \
                       WHERE (account = NONE AND workspace = NONE AND scope = NONE AND project = NONE) \
                       OR (account = $a AND workspace = $w AND scope = NONE AND project = NONE);"
            }
            "session" => {
                "UPDATE session SET account = $a, workspace = $w WHERE account = NONE AND workspace = NONE;"
            }
            "cluster" => {
                "UPDATE cluster SET account = $a, workspace = $w WHERE account = NONE AND workspace = NONE;"
            }
            _ => {
                "UPDATE raw SET account = $a, workspace = $w WHERE account = NONE AND workspace = NONE;"
            }
        };
        let statement = db
            .inner()
            .query(sql.to_string())
            .bind(("a", account.clone()))
            .bind(("w", workspace.clone()));
        let response = statement
            .await
            .unwrap_or_else(|error| panic!("fixture adoption on `{table}` failed: {error}"));
        let mut response = response
            .check()
            .unwrap_or_else(|error| panic!("fixture adoption on `{table}` failed: {error}"));
        let _: Vec<serde_json::Value> = response.take(0).unwrap_or_default();
    }
}
