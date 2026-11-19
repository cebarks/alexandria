//! Task 9: the browser authentication, CSRF, cookie and account/admin boundary.
//!
//! Every case drives the router production mounts
//! ([`router_secured`](alexandria_mcp::debug::router_secured)) through the real middleware stack: the
//! guard, [`AuthService`](alexandria_mcp::auth::AuthService), the cookie pair, the CSRF pair and the
//! sealed contexts `AccessRepo::authorize` / `::authorize_admin` issue. Nothing here stubs the boundary:
//! there is no fixture that disables a guard, no test-only owner, and no route asserted through a
//! handler call rather than through the router.
//!
//! The fixture chain is the production chain. Owners come from `IdentityRepo::bootstrap_owner` at one
//! shared policy Argon2id digest, sessions from `AuthService::login` **over the router**, and an
//! administrator is made by an ordinary `UPDATE account SET role = 'admin'` against the store this test
//! owns — the same raw-SQL fixture idiom `tests/fixtures/mod.rs` and storage's own authorization tests
//! use. That is deliberately not a public enrollment path: the design forbids anonymous web signup, and
//! the first-account command is Task 14's CLI. The capability under test still comes only from
//! `authorize_admin` re-reading that row.
//!
//! Four properties organise the file, and each has a positive control beside it because a denial that
//! nothing else could satisfy is not evidence:
//!
//! * **A private route with no authenticated context is a 403 before its handler runs** — proven by the
//!   guard's own plain-text body and content type, and paired with the same route answering 200 for a
//!   signed-in owner.
//! * **A mutation owes CSRF proof; absence is not proof.** A `POST` with a valid session and no
//!   `Sec-Fetch-Site` header at all — the shape the pre-Task 9 guard waved through — now needs the token
//!   pair, and the pre-auth pages need a token this process issued.
//! * **The cookie pair and the bearer path are different credentials in both directions.**
//! * **Admin is account management and content-free health, never a corpus read.**

use std::collections::HashSet;
use std::sync::Arc;

use alexandria_mcp::auth::admission::Admission;
use alexandria_mcp::auth::http::AuthMiddleware;
use alexandria_mcp::auth::mcp_transport::{AuthEvents, CredentialKind, McpSessionRegistry};
use alexandria_mcp::auth::service::AuthService;
use alexandria_mcp::debug::{self, DebugContext, WebBoundary, WebPolicy};
use alexandria_mcp::server::{AlexandriaServer, HeatSettings};
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::access::{AccessRepo, OwnerContext, Principal};
use alexandria_storage::models::identity::{AccountId, CredentialId, OperationId};
use alexandria_storage::models::{HeatState, ReadSelection};
use alexandria_storage::repos::{
    AuthRepo, ClusterRepo, EdgeRepo, HeatRepo, IdentityRepo, LogEntry, MaintenanceRepo, MemoryRepo,
    PasswordDigest, ScopedClusterRepo, ScopedMemoryRepo, ScopedSessionRepo, WriteTarget,
};
use alexandria_storage::{Database, record_id_to_string, schema};
use axum::body::Body;
use axum::extract::Request;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::{CONTENT_TYPE, COOKIE, HOST};
use axum::response::Response;
use chrono::Utc;
use tower::ServiceExt;

/// The one fixture password. A fixture secret, never a real one; its digest is paid once per binary
/// because the policy costs ~19 MiB per hash and these cases enrol a dozen owners.
const PASSWORD: &str = "correct-horse-battery-staple-task9";

/// The hosted fixture's configured origin, and the loopback-development one.
const HOSTED_ORIGIN: &str = "https://memory.example.test";
const HOSTED_HOST: &str = "memory.example.test";
const LOOPBACK_ORIGIN: &str = "http://localhost:3000";
const LOOPBACK_HOST: &str = "localhost:3000";

/// An invitation's claim secret, posted by the fixture that needs one.
const INVITED_PASSWORD: &str = "invited-password-long-enough";

async fn owner_digest() -> &'static PasswordDigest {
    static DIGEST: tokio::sync::OnceCell<PasswordDigest> = tokio::sync::OnceCell::const_new();
    DIGEST
        .get_or_init(|| async {
            AuthRepo::create_password_digest(PASSWORD)
                .await
                .expect("the policy must produce a digest")
        })
        .await
}

/// The constant-vector stub the debug suites use: no model download. Ranking and floor behaviour is the
/// in-crate suites' problem (they have `BandedEmbedding`); this file is about the boundary.
struct StubProvider;

#[async_trait::async_trait]
impl EmbeddingProvider for StubProvider {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1_f32, 0.2]).collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "task9-stub"
    }
}

fn stub() -> Arc<dyn EmbeddingProvider> {
    Arc::new(StubProvider)
}

/// One signed-in browser: the two cookie halves and the rows they authenticate as.
struct Session {
    account: AccountId,
    cookie: String,
    csrf: String,
    credential: CredentialId,
}

/// A store, a service, a boundary, and the router a browser would reach.
#[derive(Clone)]
struct Web {
    router: axum::Router,
    db: Arc<Database>,
    service: Arc<AuthService>,
    host: String,
    origin: String,
}

impl Web {
    /// The auth service, for the cases that must act as the transport would rather than through a page.
    fn service(&self) -> &Arc<AuthService> {
        &self.service
    }
}

impl Web {
    /// The hosted build: an explicit HTTPS origin and finite host/origin lists.
    async fn hosted() -> Self {
        Self::built(
            HOSTED_ORIGIN,
            HOSTED_HOST,
            &[HOSTED_HOST],
            &[HOSTED_ORIGIN],
            false,
            Some(debug_context(&[HOSTED_HOST])),
        )
        .await
    }

    /// The same store and boundary with the dashboard's diagnostics absent — the case that must not
    /// weaken a single check.
    async fn hosted_without_diagnostics() -> Self {
        Self::built(
            HOSTED_ORIGIN,
            HOSTED_HOST,
            &[HOSTED_HOST],
            &[HOSTED_ORIGIN],
            false,
            None,
        )
        .await
    }

    /// The loopback-development build: the one configuration that may serve cookies without `Secure`.
    async fn loopback() -> Self {
        Self::built(
            LOOPBACK_ORIGIN,
            LOOPBACK_HOST,
            &["localhost"],
            &[LOOPBACK_ORIGIN],
            true,
            Some(debug_context(&["localhost"])),
        )
        .await
    }

    async fn built(
        public_origin: &str,
        host: &str,
        allowed_hosts: &[&str],
        allowed_origins: &[&str],
        loopback_development: bool,
        ctx: Option<DebugContext>,
    ) -> Self {
        let mut db = Database::connect_embedded()
            .await
            .expect("an in-memory store");
        schema::migrate(db.inner())
            .await
            .expect("the schema, v009 and v010 included");
        let _system = db
            .claim_system_authority()
            .expect("a fresh store issues its startup capability once");
        let db = Arc::new(db);

        let policy = WebPolicy::hosted(
            public_origin,
            &to_list(allowed_hosts),
            &to_list(allowed_origins),
            loopback_development,
        )
        .unwrap_or_else(|problems| {
            panic!("fixture origin `{public_origin}` refused: {problems:?}")
        });

        let events = AuthEvents::without_sessions();
        let service = Arc::new(AuthService::admitting(Arc::clone(&db), events.clone()));
        let gate = AuthMiddleware::admitting(
            Arc::clone(&service),
            Admission::bound_to(events),
            McpSessionRegistry::over(&service.events()),
        );
        let boundary = WebBoundary::new(gate, Arc::clone(&db), policy);
        let server = AlexandriaServer::new(Arc::clone(&db), stub(), 0.75, HeatSettings::default());
        let router = debug::router_secured(server, ctx, boundary);

        Self {
            router,
            db,
            service,
            host: host.to_string(),
            origin: public_origin.to_string(),
        }
    }

    /// The legacy, no-auth build: `router(server)`, which a stdio caller or an old test reaches. It must
    /// deny rather than serve.
    async fn no_context() -> axum::Router {
        let db = Database::connect_embedded()
            .await
            .expect("an in-memory store");
        schema::migrate(db.inner()).await.expect("the schema");
        debug::router(AlexandriaServer::new(
            Arc::new(db),
            stub(),
            0.75,
            HeatSettings::default(),
        ))
    }

    /// The same store behind `router_with_context(server, Some(ctx))`: diagnostics supplied, no auth
    /// service, and therefore still no private data.
    async fn diagnostics_without_auth() -> axum::Router {
        let db = Database::connect_embedded()
            .await
            .expect("an in-memory store");
        schema::migrate(db.inner()).await.expect("the schema");
        debug::router_with_context(
            AlexandriaServer::new(Arc::new(db), stub(), 0.75, HeatSettings::default()),
            Some(debug_context(&["localhost"])),
        )
    }

    // -- enrollment, sign-in, authority ---------------------------------------------------------------

    async fn enroll(&self, login: &str) -> AccountId {
        IdentityRepo::new(self.db.inner())
            .bootstrap_owner(
                login,
                owner_digest().await.as_str(),
                &OperationId::new(format!("task9-{login}")),
            )
            .await
            .unwrap_or_else(|error| panic!("enrolling `{login}`: {error:?}"))
            .account
    }

    /// Signs in **over the router** — the path a browser takes — and returns the cookies the response
    /// set. A service call would hand back the same secrets without exercising the boundary that issues
    /// them.
    async fn sign_in(&self, login: &str) -> Session {
        let account = self.enroll(login).await;
        let token = self.preauth_token("/debug/login").await;
        let response = self
            .post_raw(
                "/debug/login",
                &format!("login={login}&password={PASSWORD}&csrf={token}"),
                None,
                Some(&self.host),
                Some(&self.origin),
                &[],
            )
            .await;
        let status = response.status();
        let headers = response.headers().clone();
        let body = short(&body_of(response).await);
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "a valid login must open a session and redirect, got: {body}"
        );
        let cookie = cookie_value(&headers, debug::SESSION_COOKIE)
            .expect("the login response set a session cookie");
        let csrf = cookie_value(&headers, debug::CSRF_COOKIE)
            .expect("the login response set a CSRF cookie");
        // The credential row is read back through storage's own verification path rather than parsed
        // out of the secret, because the secret is not a record id and `record_id_to_string` is not a
        // parser-safe round trip in either direction.
        let credential = self.principal_of(&cookie).await;
        Session {
            account,
            cookie,
            csrf,
            credential,
        }
    }

    /// Signs in an administrator: the same enrollment, then the role write an operator would perform,
    /// then the same router sign-in.
    async fn sign_in_admin(&self, login: &str) -> Session {
        let account = self.enroll(login).await;
        self.set_role(&account, "admin").await;
        self.sign_in_existing(login).await
    }

    /// Signs in an account that is already enrolled.
    async fn sign_in_existing(&self, login: &str) -> Session {
        let token = self.preauth_token("/debug/login").await;
        let response = self
            .post_raw(
                "/debug/login",
                &format!("login={login}&password={PASSWORD}&csrf={token}"),
                None,
                Some(&self.host),
                Some(&self.origin),
                &[],
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "login failed");
        let headers = response.headers().clone();
        let cookie = cookie_value(&headers, debug::SESSION_COOKIE).expect("a session cookie");
        let csrf = cookie_value(&headers, debug::CSRF_COOKIE).expect("a CSRF cookie");
        let credential = self.principal_of(&cookie).await;
        let account = self
            .account_of_cookie(&cookie)
            .await
            .expect("the session's own account");
        Session {
            account,
            cookie,
            csrf,
            credential,
        }
    }

    async fn set_role(&self, account: &AccountId, role: &str) {
        let mut response = self
            .db
            .inner()
            .query("UPDATE ONLY $target SET role = $role RETURN AFTER;")
            .bind(("target", account.as_record_id().clone()))
            .bind(("role", role.to_string()))
            .await
            .expect("a writable account row")
            .check()
            .expect("the role write");
        let rows: Vec<serde_json::Value> = response.take(0).expect("the role write");
        assert_eq!(rows.len(), 1, "the role write matched no row");
    }

    /// The credential the cookie authenticates to, as the guard sees it.
    async fn principal_of(&self, cookie: &str) -> CredentialId {
        let principal = self.principal(cookie).await;
        principal
            .credential()
            .cloned()
            .expect("a cookie-authenticated principal names a credential")
    }

    async fn principal(&self, cookie: &str) -> Principal {
        self.service
            .authenticate_browser(cookie, Utc::now())
            .await
            .expect("the fixture session authenticates")
    }

    async fn account_of_cookie(&self, cookie: &str) -> Option<AccountId> {
        let principal = self.principal(cookie).await;
        Some(principal.account().clone())
    }

    async fn authorize(&self, session: &Session) -> OwnerContext {
        let principal = self.principal(&session.cookie).await;
        AccessRepo::new(&self.db)
            .authorize(&principal)
            .await
            .expect("the fixture principal authorizes")
    }

    /// The caller's whole own corpus — Personal plus every project — which is the population every
    /// owner-facing debug page reads.
    async fn scope(&self, session: &Session) -> alexandria_storage::access::ScopedContext {
        let owner = self.authorize(session).await;
        AccessRepo::new(&self.db)
            .select(&owner, ReadSelection::Assigned)
            .await
            .expect("the owner's own corpus resolves")
    }

    // -- seeding, through the scoped writers the pages read -------------------------------------------

    /// One live fact inside the caller's own population. Seeding through `MemoryRepo::create_fact`
    /// would write an unowned row that no scoped reader can see, and the test would pass for the wrong
    /// reason.
    async fn seed_fact(&self, session: &Session, content: &str) -> String {
        let owner = self.authorize(session).await;
        let scope = self.scope(session).await;
        ScopedMemoryRepo::new(self.db.inner(), &owner, &scope)
            .create_fact(
                content,
                0.5,
                &[0.1, 0.2],
                &["task9".to_string()],
                WriteTarget::Personal,
                Utc::now(),
            )
            .await
            .expect("a seeded fact")
            .id
            .map(|id| record_id_to_string(&id))
            .expect("a created fact has an id")
    }

    async fn seed_session(&self, session: &Session, external_id: &str, summary: &str) {
        let owner = self.authorize(session).await;
        let repo = ScopedSessionRepo::new(self.db.inner(), owner);
        repo.find_or_create(external_id, Some("task9-agent"), Some("task9-model"))
            .await
            .expect("a seeded session");
        repo.finalize(external_id, Some(summary), None)
            .await
            .expect("a seeded session finalizes");
    }

    async fn seed_cluster(&self, session: &Session, label: &str) -> String {
        let owner = self.authorize(session).await;
        let scope = self.scope(session).await;
        let clusters = ScopedClusterRepo::new(self.db.inner(), &owner, &scope);
        let member = ScopedMemoryRepo::new(self.db.inner(), &owner, &scope)
            .create_fact(
                &format!("{label} member"),
                0.5,
                &[0.1, 0.2],
                &[],
                WriteTarget::Personal,
                Utc::now(),
            )
            .await
            .expect("a seeded cluster member");
        let cluster = clusters
            .create(Some(label), &[0.1, 0.2], Utc::now())
            .await
            .expect("a seeded cluster");
        let cluster_id = cluster
            .id
            .as_ref()
            .map(record_id_to_string)
            .expect("cluster id");
        ClusterRepo::new(self.db.inner())
            .add_member(
                cluster.id.as_ref().expect("cluster id"),
                member.id.as_ref().expect("member id"),
            )
            .await
            .expect("a seeded membership");
        cluster_id
    }

    /// An edge between two records. `memory_edge` carries no owner column (v009 leaves ownership to the
    /// endpoints), so relating is not an authority decision — the scoped graph read is the one that has
    /// to keep both ends inside the caller's population.
    async fn seed_edge(&self, from: &str, to: &str, edge_type: &str) {
        EdgeRepo::new(self.db.inner())
            .create_edge(from, to, edge_type, 0.5)
            .await
            .expect("a seeded edge");
    }

    async fn seed_maintenance_rows(&self) {
        use alexandria_storage::models::maintenance::job;
        for (run, job_name, action, moved) in [
            ("run-task9-a", job::CLUSTER, "split", 4_i64),
            ("run-task9-a", job::APPRAISE, "demote", 0),
            ("run-task9-b", job::MERGE, "merge", 7),
        ] {
            MaintenanceRepo::with_audit(
                self.db.inner(),
                alexandria_storage::repos::AuditContext::dreaming(run, job_name),
            )
            .record(&LogEntry {
                action: action.to_string(),
                source_id: "cluster:seed-source".to_string(),
                target_ids: vec!["cluster:seed-target".to_string()],
                members_moved: moved,
                disposition: None,
                previous_value: Some(0.42),
            })
            .await
            .expect("a seeded audit row");
        }
    }

    /// Creates the `heat_state` row a stored memory would have, so the heat assertions below compare a
    /// row that exists against a change that must not happen. `HeatRepo::add_heat` and `record_access`
    /// silently match zero rows without one, which is the trap this fixture exists to avoid.
    ///
    /// Idempotent on purpose: `HeatRepo::get` reads *one* row per memory, so a second insert for the
    /// same fact would make the fixture's own read fail with "multiple results" — a confusing failure
    /// that looks like a storage bug. A row that is already there is left exactly as it is, so no test
    /// can mask a warm by re-seeding it.
    async fn seed_heat(&self, fact_id: &str) {
        if self.heat(fact_id).await.is_some() {
            return;
        }
        alexandria_storage::repos::HeatRepo::new(self.db.inner())
            .create_for_memory(fact_id, 1.0)
            .await
            .expect("a seeded heat row");
    }

    /// Heat for one fact, read straight from storage: these cases assert a *caller's* query did not
    /// move *another* account's row, which needs the unscoped view of the number.
    async fn heat(&self, fact_id: &str) -> Option<HeatState> {
        HeatRepo::new(self.db.inner())
            .get(fact_id)
            .await
            .ok()
            .flatten()
    }

    async fn fact_row(&self, fact_id: &str) -> Option<alexandria_storage::models::Fact> {
        MemoryRepo::new(self.db.inner())
            .get_fact(fact_id)
            .await
            .ok()
            .flatten()
    }

    // -- requests ---------------------------------------------------------------------------------------

    /// The pre-auth CSRF token a freshly rendered public page carries.
    async fn preauth_token(&self, path: &str) -> String {
        let response = self
            .clone()
            .router
            .oneshot(get_request(path, &self.host, None))
            .await
            .expect("the auth page is public");
        // The full body is what the extractor searches; only the failure message is shortened.
        let body = body_of(response).await;
        hidden_csrf_value(&body)
            .unwrap_or_else(|| panic!("{path} rendered no pre-auth CSRF token:\n{}", short(&body)))
    }

    /// A signed-in read, with the body shortened for assertion messages. A case that *searches* the
    /// page uses [`Self::get_full`]: truncating what an assertion scans would make a green result mean
    /// "the marker was past the cut", which is the failure mode of every `contains` on a large page.
    async fn get(&self, path: &str, session: Option<&Session>) -> (StatusCode, String, HeaderMap) {
        let response = self
            .clone()
            .router
            .oneshot(get_request(path, &self.host, session))
            .await
            .expect("the router served");
        let status = response.status();
        let headers = response.headers().clone();
        let body = body_of(response).await;
        (status, short(&body), headers)
    }

    /// A signed-in read that returns the whole page.
    async fn get_full(&self, path: &str, session: Option<&Session>) -> (StatusCode, String) {
        let response = self
            .clone()
            .router
            .oneshot(get_request(path, &self.host, session))
            .await
            .expect("the router served");
        let status = response.status();
        (status, body_of(response).await)
    }

    /// A read with an explicit `Host`, returned whole: these cases assert on the status only, but a
    /// failure line has to be readable, so the caller shortens it.
    async fn get_with_host(&self, path: &str, host: &str, session: Option<&Session>) -> Response {
        self.clone()
            .router
            .oneshot(get_request(path, host, session))
            .await
            .expect("the router served")
    }

    async fn post(
        &self,
        path: &str,
        form: &str,
        session: Option<&Session>,
    ) -> (StatusCode, String) {
        let response = self
            .post_raw(
                path,
                form,
                session,
                Some(&self.host),
                Some(&self.origin),
                &[],
            )
            .await;
        let status = response.status();
        (status, short(&body_of(response).await))
    }

    async fn post_with_host(
        &self,
        path: &str,
        form: &str,
        session: Option<&Session>,
        host: &str,
    ) -> (StatusCode, String) {
        let response = self
            .post_raw(path, form, session, Some(host), Some(&self.origin), &[])
            .await;
        let status = response.status();
        (status, short(&body_of(response).await))
    }

    async fn post_with_origin(
        &self,
        path: &str,
        form: &str,
        session: Option<&Session>,
        origin: &str,
    ) -> (StatusCode, String) {
        let response = self
            .post_raw(path, form, session, Some(&self.host), Some(origin), &[])
            .await;
        let status = response.status();
        (status, short(&body_of(response).await))
    }

    async fn post_with_headers(
        &self,
        path: &str,
        form: &str,
        session: Option<&Session>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let response = self
            .post_raw(
                path,
                form,
                session,
                Some(&self.host),
                Some(&self.origin),
                headers,
            )
            .await;
        let status = response.status();
        (status, short(&body_of(response).await))
    }

    /// A mutation whose whole response body is returned, for the cases that search a rendered page.
    async fn post_full(
        &self,
        path: &str,
        form: &str,
        session: Option<&Session>,
    ) -> (StatusCode, String) {
        let response = self
            .post_raw(
                path,
                form,
                session,
                Some(&self.host),
                Some(&self.origin),
                &[],
            )
            .await;
        let status = response.status();
        (status, body_of(response).await)
    }

    /// The one POST primitive: every other `post_*` is this with fewer defaults spelled out.
    async fn post_raw(
        &self,
        path: &str,
        form: &str,
        session: Option<&Session>,
        host: Option<&str>,
        origin: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Response {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header(HOST, host.unwrap_or("placeholder.invalid"))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(origin) = origin {
            builder = builder.header("origin", origin);
        }
        if let Some(session) = session {
            builder = builder.header(COOKIE, cookie_header(session));
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        self.clone()
            .router
            .oneshot(
                builder
                    .body(Body::from(form.to_string()))
                    .expect("a valid request"),
            )
            .await
            .expect("the router served")
    }

    /// A session cookie **without** its CSRF partner: the shape a leaked cookie alone takes, and also
    /// the shape a test uses to prove the header-only path is not the only way to present a token.
    async fn get_cookie_only(&self, path: &str, session: &Session) -> (StatusCode, String) {
        let request = Request::builder()
            .method("GET")
            .uri(path)
            .header(HOST, &self.host)
            .header(
                COOKIE,
                format!("{}={}", debug::SESSION_COOKIE, session.cookie),
            )
            .body(Body::empty())
            .expect("a valid request");
        let response = self.clone().router.oneshot(request).await.expect("served");
        let status = response.status();
        (status, short(&body_of(response).await))
    }
}

fn to_list(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn debug_context(allowed_hosts: &[&str]) -> DebugContext {
    DebugContext {
        embedding_model: "task9-stub".to_string(),
        embedding_device: "cpu".to_string(),
        transport: "http".to_string(),
        bind_host: "127.0.0.1".to_string(),
        bind_port: 3000,
        data_dir: ":memory:".to_string(),
        cluster_merge_threshold: 0.7,
        dreaming_summary: String::new(),
        dreaming_liveness: None,
        allowed_hosts: to_list(allowed_hosts),
    }
}

fn cookie_header(session: &Session) -> String {
    format!(
        "{}={}; {}={}",
        debug::SESSION_COOKIE,
        session.cookie,
        debug::CSRF_COOKIE,
        session.csrf
    )
}

fn get_request(path: &str, host: &str, session: Option<&Session>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(path)
        .header(HOST, host);
    if let Some(session) = session {
        builder = builder.header(COOKIE, cookie_header(session));
    }
    builder.body(Body::empty()).expect("a valid request")
}

async fn body_of(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a readable body");
    String::from_utf8_lossy(&bytes).into_owned()
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// The `Set-Cookie` line for one name.
fn cookie_line(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with(&format!("{name}=")))
        .map(str::to_string)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let line = cookie_line(headers, name)?;
    let (_, rest) = line.split_once('=')?;
    let (value, _) = rest.split_once(';')?;
    (!value.is_empty()).then(|| value.to_string())
}

fn cookie_count(headers: &HeaderMap, name: &str) -> usize {
    headers
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter(|value| value.starts_with(&format!("{name}=")))
        .count()
}

/// The value of a rendered form's hidden `csrf` input.
fn hidden_csrf_value(body: &str) -> Option<String> {
    let (_, rest) = body.split_once("name=\"csrf\" value=\"")?;
    let (value, _) = rest.split_once('"')?;
    (!value.is_empty()).then(|| value.to_string())
}

/// The one-time secret a minting response renders, read out of the marked element rather than by
/// guessing where in the page it landed.
fn one_time_secret(body: &str) -> Option<String> {
    let (_, rest) = body.split_once("data-one-time-secret=\"")?;
    let (value, _) = rest.split_once('"')?;
    (!value.is_empty()).then(|| value.to_string())
}

/// A failure line, not a document: pages here are tens of kilobytes and a raw body buries the
/// assertion in the log. Kept markers are the ones these cases actually branch on.
fn short(body: &str) -> String {
    const MAX: usize = 300;
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

fn listed_debug(rows: &[(String, String)]) -> String {
    rows.iter()
        .map(|(id, label)| format!("{id}={label}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Percent-encoding for a path segment, the way the templates do it.
fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// A plain-text refusal proves the guard answered; page markup would prove the handler ran.
fn is_plain_denial(body: &str) -> bool {
    !body.contains("<nav") && !body.contains("<table")
}

/// Every private route, from one list. `no_context_router_serves_no_private_data` walks it against the
/// legacy build and the authenticated control walks it against the secured build, so a new route cannot
/// be missing from one and present in the other.
fn private_routes() -> Vec<(&'static str, &'static str)> {
    vec![
        ("GET", "/debug"),
        ("GET", "/debug/memories"),
        ("GET", "/debug/memories/fact%3Aprobe"),
        ("GET", "/debug/clusters"),
        ("GET", "/debug/clusters/cluster%3Aprobe"),
        ("GET", "/debug/sessions"),
        ("GET", "/debug/sessions/probe-session"),
        ("GET", "/debug/graph/fact%3Aprobe"),
        ("GET", "/debug/api/graph/fact%3Aprobe"),
        ("GET", "/debug/maintenance"),
        ("GET", "/debug/reminder-mapping"),
        ("POST", "/debug/reminder-mapping/release"),
        ("GET", "/debug/query"),
        ("GET", "/debug/account"),
        ("GET", "/debug/admin"),
        // Task 10/11/14: the private journal, collections/classification surface and reminder mapping.
        // Each is listed here as well as in `guard.rs`'s matrices because this walk proves something the guard's
        // own unit test cannot — that the **unauthenticated build** refuses them at the boundary rather
        // than rendering a page that then fails to find a session.
        ("GET", "/debug/operations"),
        ("GET", "/debug/operations/semantic_operation%3Aprobe"),
        ("GET", "/debug/collections"),
        // Task 11's whole-session archive: the only route in the debug surface that renders a session's
        // own prose, so it is enumerated exactly like the pages that withhold it.
        ("GET", "/debug/collections/archive"),
        ("GET", "/debug/classification"),
        ("GET", "/debug/refinement"),
        ("GET", "/debug/refinement/settings"),
        (
            "GET",
            "/debug/refinement/proposals/refinement_proposal%3Aprobe",
        ),
        (
            "GET",
            "/debug/refinement/attempts/refinement_attempt%3Aprobe",
        ),
        ("POST", "/debug/refinement/consent"),
        ("POST", "/debug/refinement/mode"),
        ("POST", "/debug/refinement/enqueue"),
        (
            "POST",
            "/debug/refinement/proposals/refinement_proposal%3Aprobe/approve",
        ),
        (
            "POST",
            "/debug/refinement/proposals/refinement_proposal%3Aprobe/dismiss",
        ),
        (
            "POST",
            "/debug/refinement/proposals/refinement_proposal%3Aprobe/defer",
        ),
        (
            "POST",
            "/debug/refinement/proposals/refinement_proposal%3Aprobe/revert",
        ),
        ("POST", "/debug/query/run"),
        ("POST", "/debug/logout"),
        ("POST", "/debug/account/tokens"),
        ("POST", "/debug/account/credentials/revoke"),
        ("POST", "/debug/account/recovery"),
        ("POST", "/debug/admin/invitations"),
        ("POST", "/debug/admin/recovery"),
        ("POST", "/debug/admin/suspend"),
        ("POST", "/debug/admin/activate"),
        ("POST", "/debug/collections/edit"),
        ("POST", "/debug/collections/move"),
        ("POST", "/debug/classification/decide"),
        ("POST", "/debug/classification/revise"),
    ]
}

// ============================================================================================
// The fail-closed legacy build
// ============================================================================================

#[tokio::test]
async fn no_context_router_serves_no_private_data() {
    for (app, label) in [
        (Web::no_context().await, "router(server)"),
        (
            Web::diagnostics_without_auth().await,
            "router_with_context(server, Some(ctx))",
        ),
    ] {
        for (method, path) in private_routes() {
            let mut builder = Request::builder()
                .method(method)
                .uri(path)
                .header(HOST, "localhost");
            if method == "POST" {
                // The shape that used to reach `query::run`: a form post that carries no hostile
                // Fetch-Metadata header at all.
                builder = builder.header(CONTENT_TYPE, "application/x-www-form-urlencoded");
            }
            let response = app
                .clone()
                .oneshot(
                    builder
                        .body(Body::from("mode=retrieve&query=x"))
                        .expect("valid"),
                )
                .await
                .expect("the router served");
            let status = response.status();
            let content_type = header_value(response.headers(), "content-type").unwrap_or_default();
            let body = short(&body_of(response).await);
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{label}: {method} {path} was served without an auth context: {body}"
            );
            assert!(
                content_type.starts_with("text/plain"),
                "{label}: {method} {path} answered {content_type}, so a handler rendered a page: {body}"
            );
            assert!(
                body.contains("authentication context"),
                "{label}: {method} {path} did not say what was missing: {body}"
            );
            assert!(
                is_plain_denial(&body),
                "{label}: {method} {path} rendered markup, so its handler ran: {body}"
            );
        }
    }
}

#[tokio::test]
async fn the_secured_router_serves_the_same_reads_to_a_signed_in_owner() {
    // The control the denial suite above needs: the fixture can reach every owner-facing route with
    // real markup, so a 403 anywhere else in this file means the boundary refused and not that the
    // page is broken.
    let web = Web::hosted().await;
    let session = web.sign_in("task9-control-owner").await;
    for path in [
        "/debug",
        "/debug/memories",
        "/debug/clusters",
        "/debug/sessions",
        "/debug/reminder-mapping",
        "/debug/query",
        "/debug/account",
    ] {
        let (status, body) = web.get_full(path, Some(&session)).await;
        assert_eq!(status, StatusCode::OK, "{path}: {}", short(&body));
        assert!(
            body.contains("<nav"),
            "{path} did not render the layout: {}",
            short(&body)
        );
    }
    // Admin routes are not owner routes: the same signed-in plain owner is refused there.
    for path in ["/debug/admin", "/debug/maintenance"] {
        let (status, _, _) = web.get(path, Some(&session)).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{path} served to a plain owner"
        );
    }
}

#[tokio::test]
async fn a_hosted_page_still_needs_a_valid_host_under_the_secured_build() {
    // The old `test_wildcard_and_empty_allowed_hosts_skip_the_host_check` /
    // `test_missing_host_is_allowed_when_the_check_is_off` expectations are reversed here on purpose:
    // once an auth boundary is mounted, its policy cannot be wildcarded or emptied at all, so a
    // rebinding Host and an absent Host are both refusals on every route.
    let web = Web::hosted().await;
    let session = web.sign_in("task9-host-owner").await;
    let (status, _, _) = web.get("/debug/memories", Some(&session)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "positive control with the configured Host"
    );

    let response = web
        .get_with_host("/debug/memories", "evil.example", Some(&session))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "a foreign Host was served an owner page"
    );

    let mut request = get_request("/debug/memories", "ignored", Some(&session));
    request.headers_mut().remove(HOST);
    let response = web.clone().router.oneshot(request).await.expect("served");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "an absent Host was served under an armed policy"
    );
}

// ============================================================================================
// Cookies, rotation, no-store
// ============================================================================================

#[tokio::test]
async fn login_sets_the_disclosed_cookie_pair_and_rotates_the_session() {
    let web = Web::hosted().await;
    web.enroll("task9-rotating").await;
    let token = web.preauth_token("/debug/login").await;
    let response = web
        .post_raw(
            "/debug/login",
            &format!("login=task9-rotating&password={PASSWORD}&csrf={token}"),
            None,
            Some(HOSTED_HOST),
            Some(HOSTED_ORIGIN),
            &[],
        )
        .await;
    let status = response.status();
    let headers = response.headers().clone();
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "a valid login should redirect"
    );

    let session_line = cookie_line(&headers, debug::SESSION_COOKIE).expect("a session cookie");
    let csrf_line = cookie_line(&headers, debug::CSRF_COOKIE).expect("a CSRF cookie");
    assert!(
        session_line.contains("HttpOnly"),
        "the session cookie must be HttpOnly: {session_line}"
    );
    assert!(
        session_line.contains("Secure"),
        "hosted cookies must be Secure: {session_line}"
    );
    assert!(
        session_line.contains("SameSite=Lax"),
        "the session cookie must declare SameSite: {session_line}"
    );
    assert!(
        session_line.contains("Path=/"),
        "the cookie is app-scoped, not directory-scoped: {session_line}"
    );
    assert!(
        !session_line.contains("Domain="),
        "a Domain attribute would widen the cookie to every subdomain: {session_line}"
    );
    assert!(
        csrf_line.contains("SameSite=Lax")
            && csrf_line.contains("Secure")
            && csrf_line.contains("Path=/"),
        "the CSRF half carries the same site policy: {csrf_line}"
    );
    assert!(
        csrf_line.contains("HttpOnly"),
        "the CSRF half is echoed by the server from the request's own cookie, so a script never needs \
         to read it and HttpOnly costs nothing: {csrf_line}"
    );
    let session_value = cookie_value(&headers, debug::SESSION_COOKIE).expect("a session value");
    let csrf_value = cookie_value(&headers, debug::CSRF_COOKIE).expect("a CSRF value");
    assert_ne!(
        session_value, csrf_value,
        "the session and CSRF halves must be two different secrets"
    );

    // A second login mints a *different* session, so a value that ever left the process is not the one
    // that survives the sign-in.
    let second_token = web.preauth_token("/debug/login").await;
    let second = web
        .post_raw(
            "/debug/login",
            &format!("login=task9-rotating&password={PASSWORD}&csrf={second_token}"),
            None,
            Some(HOSTED_HOST),
            Some(HOSTED_ORIGIN),
            &[],
        )
        .await;
    let rotated = cookie_value(second.headers(), debug::SESSION_COOKIE).expect("a second session");
    assert_ne!(
        rotated, session_value,
        "a login must rotate the session value, not reuse the presented one"
    );
    // Both sessions are then live: a browser session is minted rather than replaced, and each ends at
    // its own revocation or deadline. One login does not silently sign the other device out — which is
    // what makes the rotation above a *rotation* of what this browser holds rather than a global reset
    // somebody else's tab would have to survive.
    let first = Session {
        account: self_account(&web, &session_value).await,
        credential: web.principal_of(&session_value).await,
        cookie: session_value,
        csrf: csrf_value,
    };
    let (status, body, _) = web.get("/debug/account", Some(&first)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the pre-rotation session stopped working: {body}"
    );
}

/// The account a session cookie authenticates to.
async fn self_account(web: &Web, cookie: &str) -> AccountId {
    web.principal(cookie).await.account().clone()
}

#[tokio::test]
async fn a_second_valid_cookie_from_the_same_account_reads_the_same_corpus() {
    // Two live sessions of one owner are two rows and one population; this also pins that the page's
    // "this device" marker is per credential rather than per account.
    let web = Web::hosted().await;
    web.enroll("task9-two-devices").await;
    let first = web.sign_in_existing("task9-two-devices").await;
    let second = web.sign_in_existing("task9-two-devices").await;
    assert_ne!(
        first.cookie, second.cookie,
        "one login per device is the policy"
    );
    let fact = web.seed_fact(&first, "shared corpus row").await;
    for session in [&first, &second] {
        let (status, body) = web.get_full("/debug/memories", Some(session)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(&fact),
            "the owner's own row is missing: {}",
            short(&body)
        );
    }
}

#[tokio::test]
async fn private_pages_and_auth_pages_send_no_store() {
    let web = Web::hosted().await;
    let session = web.sign_in("task9-nostore").await;
    for path in ["/debug", "/debug/memories", "/debug/account"] {
        let (status, _, headers) = web.get(path, Some(&session)).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(
            header_value(&headers, "cache-control").as_deref(),
            Some("no-store"),
            "{path} must not be cacheable: {headers:?}"
        );
    }
    for path in ["/debug/login", "/debug/invite", "/debug/recover"] {
        let (status, _, headers) = web.get(path, None).await;
        assert_eq!(status, StatusCode::OK, "{path} is public");
        assert_eq!(
            header_value(&headers, "cache-control").as_deref(),
            Some("no-store"),
            "{path} issues a pre-auth token and must not be cached: {headers:?}"
        );
    }
    // An asset keeps its year of `immutable`: the filename pins the bytes, and the guard must not
    // "helpfully" downgrade that.
    let (_, _, headers) = web.get(debug::assets::HTMX_URL, None).await;
    assert_eq!(
        header_value(&headers, "cache-control").as_deref(),
        Some("public, max-age=31536000, immutable"),
        "the asset cache contract changed: {headers:?}"
    );
}

#[tokio::test]
async fn the_loopback_exception_relaxes_secure_and_nothing_else() {
    let web = Web::loopback().await;
    let session = web.sign_in("task9-loopback-owner").await;
    let (status, _, _) = web.get("/debug", Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "the loopback build serves");

    // The policy, not a header, is what turned `Secure` off — and the two refusals below prove the
    // exception cannot be widened by configuration or claimed by a request.
    let refused = WebPolicy::hosted(
        LOOPBACK_ORIGIN,
        &["localhost".to_string()],
        &[LOOPBACK_ORIGIN.to_string()],
        false,
    )
    .expect_err("plain HTTP without the exception must be refused");
    assert!(
        refused
            .iter()
            .any(|problem| problem.contains("server.public_origin")),
        "the refusal should name the origin key: {refused:?}"
    );
    assert!(
        refused
            .iter()
            .any(|problem| problem.contains("server.loopback_development")),
        "and the exception: {refused:?}"
    );
    let public_http = WebPolicy::hosted(
        "http://memory.example.test",
        &["memory.example.test".to_string()],
        &["http://memory.example.test".to_string()],
        true,
    )
    .expect_err("the loopback exception cannot authorize a public host");
    assert!(
        public_http
            .iter()
            .any(|problem| problem.contains("loopback")),
        "{public_http:?}"
    );

    // ...and the relaxation it does authorize is exactly one attribute: a real successful loopback login
    // issues both halves without `Secure`, while `HttpOnly`, `SameSite` and `Path` stay. The hosted build
    // pins the presence of `Secure`; nothing here pinned its absence on the build that is allowed to drop
    // it, so a policy that relaxed more than `Secure` — or one that relaxed nothing at all — was invisible.
    let headers = loopback_login_headers(&web, "task9-loopback-pair", &[]).await;
    assert_pair_without_secure(&headers, "the loopback-development exception");
}

/// Signs an enrolled account in on the loopback build through the router and returns the headers of the
/// redirect that issued the cookie pair.
///
/// The login itself is asserted. A response that refused a wrong password sets no cookies, so reading
/// attributes off it proves nothing about them — which is the hole this helper exists to close, and the
/// reason the caller enrols rather than relying on `sign_in` (which discards the headers it used).
async fn loopback_login_headers(web: &Web, login: &str, headers: &[(&str, &str)]) -> HeaderMap {
    web.enroll(login).await;
    let token = web.preauth_token("/debug/login").await;
    let response = web
        .post_raw(
            "/debug/login",
            &format!("login={login}&password={PASSWORD}&csrf={token}"),
            None,
            Some(LOOPBACK_HOST),
            Some(LOOPBACK_ORIGIN),
            headers,
        )
        .await;
    let status = response.status();
    let headers = response.headers().clone();
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "a valid loopback login must redirect and set the cookie pair, got {status}"
    );
    cookie_line(&headers, debug::SESSION_COOKIE).expect("the loopback login set a session cookie");
    headers
}

/// The one attribute the loopback exception is permitted to drop, and only from both halves.
fn assert_pair_without_secure(headers: &HeaderMap, cause: &str) {
    for name in [debug::SESSION_COOKIE, debug::CSRF_COOKIE] {
        let line = cookie_line(headers, name)
            .unwrap_or_else(|| panic!("the loopback login set no `{name}` cookie ({cause})"));
        assert!(
            !line.contains("Secure"),
            "`{name}` carries `Secure` after {cause}: {line}"
        );
        assert!(
            line.contains("HttpOnly"),
            "`{name}` lost `HttpOnly` along with `Secure`, which {cause} must not do: {line}"
        );
        assert!(
            line.contains("SameSite=Lax"),
            "`{name}` lost its site policy along with `Secure`, which {cause} must not do: {line}"
        );
        assert!(
            line.contains("Path=/"),
            "`{name}` lost its path scope, which {cause} must not do: {line}"
        );
        assert!(
            !line.contains("Domain="),
            "`{name}` widened to a domain, which {cause} must not do: {line}"
        );
    }
}

#[tokio::test]
async fn logout_revokes_the_session_credential_and_terminates_its_live_lease() {
    let web = Web::hosted().await;
    let session = web.sign_in("task9-logout").await;
    let (before, _, _) = web.get("/debug/account", Some(&session)).await;
    assert_eq!(before, StatusCode::OK, "positive control before logout");

    // Logout is a mutation, so it owes the token first.
    let (denied, body) = web.post("/debug/logout", "", Some(&session)).await;
    assert_eq!(denied, StatusCode::FORBIDDEN, "logout without CSRF: {body}");
    assert!(body.contains("CSRF"), "the refusal should say why: {body}");

    // A lease on **this session's own** credential, held open: logout revokes that row, and the
    // Task 5 bus is what has to end a lease already issued from it. An unrelated agent token would
    // prove nothing about the credential logout touches.
    let lease = web
        .service()
        .admit(&session.cookie, CredentialKind::Browser, Utc::now())
        .await
        .expect("the session cookie admits a lease");
    assert!(!lease.is_cancelled(), "the lease started out cancelled");

    let (status, headers) = {
        let response = web
            .post_raw(
                "/debug/logout",
                &format!("csrf={}", session.csrf),
                Some(&session),
                Some(HOSTED_HOST),
                Some(HOSTED_ORIGIN),
                &[],
            )
            .await;
        let status = response.status();
        let headers = response.headers().clone();
        (status, headers)
    };
    assert_eq!(status, StatusCode::SEE_OTHER, "logout with CSRF");
    assert_eq!(
        cookie_count(&headers, debug::SESSION_COOKIE),
        1,
        "logout must expire the session cookie"
    );
    assert!(
        cookie_line(&headers, debug::SESSION_COOKIE)
            .unwrap_or_default()
            .contains("Max-Age=0"),
        "the cleared cookie must expire: {:?}",
        cookie_line(&headers, debug::SESSION_COOKIE)
    );

    let (after, body, _) = web.get("/debug/account", Some(&session)).await;
    assert_eq!(
        after,
        StatusCode::FORBIDDEN,
        "a logged-out cookie still reads: {body}"
    );
    let row = AuthRepo::new(web.db.inner())
        .credential_of(&session.credential)
        .await
        .expect("a readable credential row")
        .expect("the session row still exists");
    assert_eq!(
        row.revoked,
        Some(true),
        "logout must revoke the session row"
    );
    assert!(
        lease.is_cancelled(),
        "logout revoked the credential but left its live lease running"
    );
    assert!(
        web.service()
            .authenticate_browser(&session.cookie, Utc::now())
            .await
            .is_err(),
        "a revoked session cookie still authenticates"
    );
}

#[tokio::test]
async fn revocation_and_suspension_end_a_live_browser_session_mid_stream() {
    let web = Web::hosted().await;

    // (a) Revoking the session's own credential ends it — and cancels a lease held on that credential.
    let session = web.sign_in("task9-revoked").await;
    let lease = web
        .service()
        .admit(&session.cookie, CredentialKind::Browser, Utc::now())
        .await
        .expect("a browser session admits a lease");
    let (before, _, _) = web.get("/debug", Some(&session)).await;
    assert_eq!(before, StatusCode::OK, "positive control before revocation");
    let owner = web.authorize(&session).await;
    web.service()
        .revoke_credential(&owner, &session.credential, Utc::now())
        .await
        .expect("an owner may revoke their own session");
    let (status, body, _) = web.get("/debug", Some(&session)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a revoked session still reads: {body}"
    );
    assert!(
        lease.is_cancelled(),
        "the revocation published but did not cancel the open lease"
    );

    // (b) Suspending the *account* ends a different live session of the same owner, through the admin
    //     route rather than a row edit: that is the surface the task ships.
    let victim = web.sign_in("task9-suspended").await;
    let victim_lease = web
        .service()
        .admit(&victim.cookie, CredentialKind::Browser, Utc::now())
        .await
        .expect("the victim's session admits a lease");
    let (before, _, _) = web.get("/debug", Some(&victim)).await;
    assert_eq!(before, StatusCode::OK, "positive control before suspension");
    let admin = web.sign_in_admin("task9-suspender").await;
    let target = record_id_to_string(victim.account.as_record_id());
    let (status, body) = web
        .post(
            "/debug/admin/suspend",
            &format!("account={}&csrf={}", urlencode(&target), admin.csrf),
            Some(&admin),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the suspension was refused: {body}"
    );
    let (status, body, _) = web.get("/debug", Some(&victim)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a suspended account's live session still reads: {body}"
    );
    assert!(
        victim_lease.is_cancelled(),
        "suspension did not close the account's open lease"
    );

    // The admin kept their own session, and restoring the account does not resurrect the dead one.
    let (status, _, _) = web.get("/debug/admin", Some(&admin)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the suspending admin lost their own session"
    );
    let (status, body) = web
        .post(
            "/debug/admin/activate",
            &format!("account={}&csrf={}", urlencode(&target), admin.csrf),
            Some(&admin),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "reactivation refused: {body}"
    );
    let (status, _, _) = web.get("/debug", Some(&victim)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a restore must not reauthenticate a revoked session"
    );
    let revived = web.sign_in_existing("task9-suspended").await;
    let (status, body, _) = web.get("/debug", Some(&revived)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a restored account cannot sign in again: {body}"
    );
}

// ============================================================================================
// CSRF
// ============================================================================================

#[tokio::test]
async fn a_mutation_without_csrf_is_refused_even_with_a_valid_session_and_no_fetch_metadata() {
    let web = Web::hosted().await;
    let alice = web.sign_in("task9-csrf-absent").await;
    let fact = web.seed_fact(&alice, "csrf probe row").await;

    // The exact shape the pre-Task 9 guard waved through: a form POST with no `Sec-Fetch-Site` header
    // and no token, from a browser-shaped Origin.
    let (status, body) = web
        .post(
            "/debug/query/run",
            "mode=retrieve&query=csrf+probe+row&limit=5",
            Some(&alice),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a session alone was allowed to mutate: {body}"
    );
    assert!(
        body.contains("CSRF"),
        "the refusal should name the missing proof: {body}"
    );

    // And the same request with the token runs, finding the owner's own row. Without this half the
    // refusal above could be a broken route rather than a refused mutation.
    let (status, body) = web
        .post_full(
            "/debug/query/run",
            &format!(
                "mode=retrieve&query=csrf+probe+row&limit=5&csrf={}",
                alice.csrf
            ),
            Some(&alice),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the same mutation with a token: {body}"
    );
    assert!(
        body.contains(&fact),
        "the run missed the owner's row: {body}"
    );
}

#[tokio::test]
async fn a_wrong_or_foreign_csrf_token_does_not_mutate() {
    let web = Web::hosted().await;
    let alice = web.sign_in("task9-csrf-wrong").await;
    let bob = web.sign_in("task9-csrf-bob").await;
    let fact = web.seed_fact(&bob, "bob owns this row").await;

    for (label, token) in [
        ("empty", ""),
        ("junk", "not-a-real-token-0000000000000000"),
        ("another session's token", bob.csrf.as_str()),
    ] {
        let (status, body) = web
            .post(
                "/debug/query/run",
                &format!("mode=retrieve&query=bob+owns&limit=5&csrf={token}"),
                Some(&alice),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{label} was accepted: {body}"
        );
    }

    // Nobody was logged out by a refused token, and the right token still works.
    let (status, _, _) = web.get("/debug/account", Some(&alice)).await;
    assert_eq!(status, StatusCode::OK, "alice was ended by a refused token");
    let (status, _, _) = web.get("/debug/account", Some(&bob)).await;
    assert_eq!(status, StatusCode::OK, "bob was ended by alice's attempts");
    let (status, body) = web
        .post_full(
            "/debug/query/run",
            &format!("mode=retrieve&query=bob+owns&limit=5&csrf={}", bob.csrf),
            Some(&bob),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bob's own token was refused: {}",
        short(&body)
    );
    assert!(body.contains(&fact), "{}", short(&body));
}

#[tokio::test]
async fn a_cross_site_mutation_is_refused_even_with_a_valid_csrf_token() {
    let web = Web::hosted().await;
    let alice = web.sign_in("task9-cross-site").await;
    let form = format!("mode=retrieve&query=quiet&limit=5&csrf={}", alice.csrf);
    for site in [
        "cross-site",
        "cross-origin",
        "  Cross-Site  ",
        "same-origin, cross-site",
    ] {
        let (status, body) = web
            .post_with_headers(
                "/debug/query/run",
                &form,
                Some(&alice),
                &[("sec-fetch-site", site)],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "Sec-Fetch-Site: {site} mutated with a valid token: {body}"
        );
    }
    // `none` is direct navigation — typed URL, bookmark, new tab — and stays allowed, as the decision
    // table says.
    let (status, body) = web
        .post_with_headers(
            "/debug/query/run",
            &form,
            Some(&alice),
            &[("sec-fetch-site", "none")],
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "direct navigation was refused: {body}"
    );
}

#[tokio::test]
async fn an_origin_that_is_not_the_public_origin_does_not_mutate() {
    let web = Web::hosted().await;
    let alice = web.sign_in("task9-origin").await;
    let form = format!("mode=retrieve&query=quiet&limit=5&csrf={}", alice.csrf);
    for (label, origin) in [
        ("foreign", "https://evil.example"),
        ("wrong scheme", "http://memory.example.test"),
        ("wrong port", "https://memory.example.test:8443"),
        ("with a path", "https://memory.example.test/login"),
    ] {
        let (status, text) = web
            .post_with_origin("/debug/query/run", &form, Some(&alice), origin)
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{label} origin mutated: {text}"
        );
    }
    let (status, text) = web
        .post_with_origin("/debug/query/run", &form, Some(&alice), HOSTED_ORIGIN)
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the configured origin was refused: {text}"
    );
}

#[tokio::test]
async fn forwarded_headers_never_establish_origin_host_or_the_loopback_exception() {
    let web = Web::hosted().await;
    let alice = web.sign_in("task9-forwarded").await;
    let form = format!("mode=retrieve&query=quiet&limit=5&csrf={}", alice.csrf);

    // A forged `X-Forwarded-Host` claiming the configured host, with a real `Host` that does not.
    let (status, body) = web
        .post_with_headers(
            "/debug/query/run",
            &form,
            Some(&alice),
            &[
                ("x-forwarded-host", HOSTED_HOST),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-for", "127.0.0.1"),
                (
                    "forwarded",
                    "for=127.0.0.1;proto=https;host=memory.example.test",
                ),
            ],
        )
        .await;
    // The Host check uses the real header, which here is the fixture's own configured host, so this
    // particular request is admissible on Host grounds and refused or accepted on CSRF grounds only:
    // what the case has to establish is that the forged headers changed nothing.
    let (control_status, control_body) = web
        .post_with_headers("/debug/query/run", &form, Some(&alice), &[])
        .await;
    assert_eq!(
        status, control_status,
        "a forwarded header changed the outcome (with={status} / without={control_status}): {body} / {control_body}"
    );

    // A hostile real Host with a friendly forged pair is still refused.
    let (status, body) = web
        .post_with_host("/debug/query/run", &form, Some(&alice), "evil.example")
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a rebound Host mutated: {body}"
    );

    // A forwarded origin cannot satisfy the origin check.
    let (status, body) = web
        .post_with_headers(
            "/debug/query/run",
            &form,
            Some(&alice),
            &[("x-forwarded-origin", HOSTED_ORIGIN)],
        )
        .await;
    // The fixture's `Origin` header is the configured one for this call, so the request must still be
    // admissible: the forged forwarded header is not consulted either way.
    assert_eq!(
        status,
        StatusCode::OK,
        "a forwarded origin changed the answer: {body}"
    );

    // On the loopback build, a claimed `X-Forwarded-Proto: https` does not turn `Secure` on. Read the
    // cookie pair the successful login actually issued rather than discarding the status: this is the
    // direction the policy is allowed to relax, and it had no positive assertion before.
    let loopback = Web::loopback().await;
    let headers = loopback_login_headers(
        &loopback,
        "task9-forwarded-loopback",
        &[("x-forwarded-proto", "https")],
    )
    .await;
    assert_pair_without_secure(&headers, "a forged `X-Forwarded-Proto: https`");
}

#[tokio::test]
async fn preauth_forms_require_their_own_csrf_proof() {
    let web = Web::hosted().await;
    web.enroll("task9-preauth").await;

    for (label, form) in [
        (
            "no token at all",
            "login=task9-preauth&password=nope".to_string(),
        ),
        (
            "a token nobody issued",
            "login=task9-preauth&password=nope&csrf=forged-0000000000000000".to_string(),
        ),
    ] {
        let (status, body) = web
            .post_with_origin("/debug/login", &form, None, HOSTED_ORIGIN)
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{label} reached the login path: {body}"
        );
        assert!(
            body.contains("CSRF"),
            "{label}: the refusal should name the missing proof: {body}"
        );
    }

    // The issued token works, and is spent: the same value cannot drive a second mutation.
    let token = web.preauth_token("/debug/login").await;
    let (status, _) = web
        .post_with_origin(
            "/debug/login",
            &format!("login=task9-preauth&password={PASSWORD}&csrf={token}"),
            None,
            HOSTED_ORIGIN,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the issued token should open a session"
    );
    let (status, body) = web
        .post_with_origin(
            "/debug/login",
            &format!("login=task9-preauth&password={PASSWORD}&csrf={token}"),
            None,
            HOSTED_ORIGIN,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a consumed token was reused: {body}"
    );

    // And the token is page-scoped: a recovery page's token cannot log anybody in.
    let recovery_token = web.preauth_token("/debug/recover").await;
    let (status, _) = web
        .post_with_origin(
            "/debug/login",
            &format!("login=task9-preauth&password={PASSWORD}&csrf={recovery_token}"),
            None,
            HOSTED_ORIGIN,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a recovery-page token drove the login path"
    );
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_login_are_one_refusal_with_no_cookie() {
    let web = Web::hosted().await;
    web.enroll("task9-bad-password").await;
    let mut bodies = Vec::new();
    for login in ["task9-bad-password", "task9-nobody-enrolled-here"] {
        let token = web.preauth_token("/debug/login").await;
        let response = web
            .post_raw(
                "/debug/login",
                &format!("login={login}&password=definitely-not-it&csrf={token}"),
                None,
                Some(HOSTED_HOST),
                Some(HOSTED_ORIGIN),
                &[],
            )
            .await;
        let status = response.status();
        let headers = response.headers().clone();
        let body = body_of(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{login}: {body}");
        assert_eq!(
            cookie_count(&headers, debug::SESSION_COOKIE),
            0,
            "a refused login set a session cookie"
        );
        assert!(
            !body.contains(login),
            "the refusal echoed the login: {body}"
        );
        bodies.push(text_of(&body));
    }
    assert_eq!(
        bodies[0], bodies[1],
        "the two refusals are distinguishable, so this endpoint enumerates accounts: {:?}",
        bodies
    );
}

/// The visible prose of a page, with every attribute and tag stripped so two refusals can be compared
/// without the page's random token value deciding the answer.
fn text_of(body: &str) -> String {
    let mut out = String::new();
    let mut inside_tag = false;
    for ch in body.chars() {
        match ch {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if inside_tag || ch.is_whitespace() => {}
            other => out.push(other),
        }
    }
    out
}

// ============================================================================================
// The two credential kinds, and the public surface
// ============================================================================================

#[tokio::test]
async fn a_browser_cookie_is_not_a_bearer_token_and_a_bearer_token_is_not_a_cookie() {
    let web = Web::hosted().await;
    let session = web.sign_in("task9-kinds").await;
    let token = web
        .service()
        .create_agent_token(&web.authorize(&session).await, "task9-agent", Utc::now())
        .await
        .expect("an owner may mint their own token");
    let bearer = token.expose().to_string();

    assert!(
        web.service()
            .admit(&session.cookie, CredentialKind::Agent, Utc::now())
            .await
            .is_err(),
        "a browser session cookie authenticated as an agent token"
    );
    assert!(
        web.service()
            .authenticate_browser(&bearer, Utc::now())
            .await
            .is_err(),
        "an agent token authenticated as a browser session"
    );

    // Neither half unlocks the other's surface.
    let forged = Session {
        account: session.account.clone(),
        cookie: bearer.clone(),
        csrf: session.csrf.clone(),
        credential: session.credential.clone(),
    };
    let (status, body, _) = web.get("/debug/memories", Some(&forged)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a bearer token read a browser page: {body}"
    );
    let swapped = Session {
        account: session.account.clone(),
        cookie: session.csrf.clone(),
        csrf: session.csrf.clone(),
        credential: session.credential.clone(),
    };
    let (status, _, _) = web.get("/debug/memories", Some(&swapped)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the CSRF half authenticated a session"
    );
    // A session cookie without its CSRF partner still reads: the pair is what a *mutation* owes, and a
    // page cannot be refused for the absence of a value no read asked for.
    let (status, body) = web.get_cookie_only("/debug/memories", &session).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a cookie-only read was refused: {body}"
    );
    let (status, _, _) = web.get("/debug/memories", Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "the real pair stopped working");
}

#[tokio::test]
async fn only_the_closed_asset_allowlist_and_the_auth_pages_are_public() {
    let web = Web::hosted().await;
    for path in [debug::assets::HTMX_URL, debug::assets::VIS_NETWORK_URL] {
        let (status, _, headers) = web.get(path, None).await;
        assert_eq!(status, StatusCode::OK, "{path} is a closed-allowlist asset");
        assert_eq!(
            header_value(&headers, "content-type").as_deref(),
            Some("text/javascript; charset=utf-8"),
            "{path}"
        );
    }
    for path in ["/debug/login", "/debug/invite", "/debug/recover"] {
        let (status, body) = web.get_full(path, None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path} is an intentional public page: {}",
            short(&body)
        );
        assert!(
            !body.contains("/debug/memories") && !body.contains("/debug/admin"),
            "{path} must not offer a signed-out visitor a private link: {}",
            short(&body)
        );
        assert!(
            hidden_csrf_value(&body).is_some(),
            "{path} has no CSRF input: {}",
            short(&body)
        );
    }
    for path in [
        "/debug/assets/nope.js",
        "/debug/assets/..%2F..%2FCargo.toml",
        "/debug/assets/SHA256SUMS",
    ] {
        let (status, _, _) = web.get(path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} must not resolve");
    }
    let mut private: HashSet<&str> = HashSet::new();
    for (_, path) in private_routes() {
        private.insert(path);
    }
    assert!(
        private.contains("/debug/admin"),
        "the admin routes are in the private list"
    );
    for path in [
        "/debug/admin",
        "/debug/account",
        "/debug/maintenance",
        "/debug/query",
    ] {
        let (status, _, headers) = web.get(path, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path} was public");
        assert_eq!(
            cookie_count(&headers, debug::SESSION_COOKIE),
            0,
            "a denial must not mint a session for {path}"
        );
    }
}

// ============================================================================================
// Owner isolation, and the admin boundary that must not read a corpus
// ============================================================================================

#[tokio::test]
async fn the_memories_pages_never_render_another_owners_rows() {
    let web = Web::hosted().await;
    let alice = web.sign_in("task9-alice").await;
    let bob = web.sign_in("task9-bob").await;
    let alice_marker = "alice-only-marker-content";
    let bob_marker = "bob-only-marker-content";
    let alice_id = web.seed_fact(&alice, alice_marker).await;
    let bob_id = web.seed_fact(&bob, bob_marker).await;

    let (status, body) = web.get_full("/debug/memories", Some(&alice)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(alice_marker),
        "alice's own row is missing: {}",
        short(&body)
    );
    assert!(
        !body.contains(bob_marker),
        "bob's row leaked to alice: {}",
        short(&body)
    );
    assert!(
        !body.contains(&urlencode(&bob_id)) && !body.contains(&bob_id),
        "bob's record id leaked: {body}"
    );

    // A foreign id and an absent id answer alike, and neither renders content.
    let (foreign_status, foreign_body, _) = web
        .get(
            &format!("/debug/memories/{}", urlencode(&bob_id)),
            Some(&alice),
        )
        .await;
    let (absent_status, absent_body, _) = web
        .get("/debug/memories/fact%3Anope-404", Some(&alice))
        .await;
    assert_eq!(
        foreign_status, absent_status,
        "a foreign id is distinguishable from an absent one: {foreign_status}/{absent_status}"
    );
    assert_eq!(
        foreign_status,
        StatusCode::NOT_FOUND,
        "expected a 404: {foreign_body}"
    );
    assert!(
        !foreign_body.contains(bob_marker) && !absent_body.contains(bob_marker),
        "a detail page rendered another owner's content"
    );
    // The positive control for the 404 pair: alice's own id renders her own content.
    let (status, body) = web
        .get_full(
            &format!("/debug/memories/{}", urlencode(&alice_id)),
            Some(&alice),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "alice's own detail page 404s: {}",
        short(&body)
    );
    assert!(
        body.contains(alice_marker),
        "the owner's own detail page lost its content: {}",
        short(&body)
    );
    // …and bob's row is not on it, which is the same boundary asserted from the page that does render.
    assert!(
        !body.contains(bob_marker),
        "alice's detail page rendered bob's content: {}",
        short(&body)
    );
}

#[tokio::test]
async fn the_sessions_clusters_and_graph_pages_stay_inside_the_boundary() {
    let web = Web::hosted().await;
    let alice = web.sign_in("task9-edge-alice").await;
    let bob = web.sign_in("task9-edge-bob").await;

    // The same external handle in two accounts is two rows, and each page shows one.
    web.seed_session(&alice, "shared-session-handle", "alice session prose")
        .await;
    web.seed_session(&bob, "shared-session-handle", "bob session prose")
        .await;
    let (status, body) = web.get_full("/debug/sessions", Some(&alice)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("shared-session-handle"),
        "alice's own session missing: {}",
        short(&body)
    );
    assert!(
        !body.contains("bob session prose"),
        "bob's summary leaked: {body}"
    );
    let (status, body) = web
        .get_full("/debug/sessions/shared-session-handle", Some(&alice))
        .await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert!(body.contains("alice session prose"), "{body}");
    assert!(
        !body.contains("bob session prose"),
        "the detail page showed bob's summary: {body}"
    );

    // Clusters.
    web.seed_cluster(&alice, "alice cluster label").await;
    web.seed_cluster(&bob, "bob cluster label").await;
    let (status, body) = web.get_full("/debug/clusters", Some(&alice)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("alice cluster label"),
        "alice's cluster missing: {}",
        short(&body)
    );
    assert!(
        !body.contains("bob cluster label"),
        "bob's cluster label leaked: {body}"
    );

    // Graph.
    let bob_fact = web.seed_fact(&bob, "bob graph node content").await;
    let alice_fact = web.seed_fact(&alice, "alice graph root").await;
    // An edge *between* the two accounts' rows: exactly the shape that would let a foreign node ride
    // into a caller's picture if the neighbour filter looked at the edge instead of the endpoint.
    web.seed_edge(&alice_fact, &bob_fact, "relates_to").await;
    let (status, body, _) = web
        .get(
            &format!("/debug/api/graph/{}", urlencode(&bob_fact)),
            Some(&alice),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a foreign graph root answered with data: {body}"
    );
    let (status, body, _) = web
        .get(
            &format!("/debug/api/graph/{}", urlencode(&alice_fact)),
            Some(&alice),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "alice's own graph failed: {body}");
    assert!(body.contains(&alice_fact), "{body}");
    assert!(
        !body.contains(&bob_fact),
        "bob's node appeared in alice's graph through a shared edge: {body}"
    );
    // The edge is real, so the absence above is the boundary and not a seeding failure.
    assert!(
        web.fact_row(&bob_fact).await.is_some(),
        "bob's row is missing, so the graph result proves nothing"
    );
}

#[tokio::test]
async fn the_dashboard_counts_one_owner_not_the_store() {
    let web = Web::hosted().await;
    let alice = web.sign_in("task9-dash-alice").await;
    let bob = web.sign_in("task9-dash-bob").await;
    for index in 0..3 {
        web.seed_fact(&alice, &format!("alice dashboard row {index}"))
            .await;
    }
    web.seed_fact(&bob, "bob dashboard row").await;
    web.seed_session(&bob, "bob-dash-session", "bob summary prose")
        .await;

    let (status, body) = web.get_full("/debug", Some(&alice)).await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert!(
        !body.contains("bob summary prose") && !body.contains("bob-dash-session"),
        "the dashboard carried another owner's prose: {body}"
    );
    assert!(
        !body.contains("bob dashboard row"),
        "the dashboard carried another owner's memory text: {body}"
    );
    // Each labelled row, not a digit scan: the page also carries timestamps, port numbers and the
    // config panel, so "does 4 appear anywhere" would fail on an honest page and pass on a wrong one.
    assert_eq!(
        dashboard_count(&body, "Facts (active)"),
        Some(3),
        "alice's active-fact count is wrong or store-wide: {}",
        short(&body)
    );
    assert_eq!(
        dashboard_count(&body, "Sessions"),
        Some(0),
        "the session count leaked bob's session: {}",
        short(&body)
    );
    assert_eq!(
        dashboard_count(&body, "Clusters"),
        Some(0),
        "the cluster count is store-wide: {}",
        short(&body)
    );
}

/// The number on a `<tr><th>{label}</th><td>{n}</td></tr>` stats row.
fn dashboard_count(body: &str, label: &str) -> Option<usize> {
    let (_, rest) = body.split_once(&format!("<th>{label}</th><td>"))?;
    let (digits, _) = rest.split_once("<")?;
    digits.trim().parse().ok()
}

#[tokio::test]
async fn an_admin_sees_accounts_and_health_and_no_other_owner_content() {
    let web = Web::hosted().await;
    let owner = web.sign_in("task9-plain-owner").await;
    let foreign = web.sign_in("task9-foreign-owner").await;
    let secret_text = "only-the-foreign-owner-reads-this";
    let foreign_fact = web.seed_fact(&foreign, secret_text).await;
    web.seed_session(&foreign, "foreign-private-session", "foreign session prose")
        .await;
    web.seed_maintenance_rows().await;

    // (1) A plain owner cannot reach either admin surface.
    for path in ["/debug/admin", "/debug/maintenance"] {
        let (status, body, _) = web.get(path, Some(&owner)).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{path} served to a plain owner: {body}"
        );
    }
    let admin = web.sign_in_admin("task9-admin").await;

    // (2) The account list is metadata, and carries no corpus.
    let (status, body) = web.get_full("/debug/admin", Some(&admin)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an admin was refused their page: {}",
        short(&body)
    );
    assert!(
        body.contains("task9-foreign-owner"),
        "the listing should name the account: {body}"
    );
    assert!(
        !body.contains(secret_text),
        "the admin page carried memory text: {body}"
    );
    assert!(
        !body.contains("foreign session prose"),
        "the admin page carried a session summary: {body}"
    );
    assert!(
        !body.contains(&urlencode(&foreign_fact)) && !body.contains(&foreign_fact),
        "the admin page carried a fact record id: {body}"
    );

    // (3) The health page is aggregates: no record identity, no previous value, no target list.
    let (status, body) = web.get_full("/debug/maintenance", Some(&admin)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an admin was refused health: {}",
        short(&body)
    );
    assert!(
        body.contains("run-task9-a") && body.contains("run-task9-b"),
        "the rollup should name the runs: {body}"
    );
    for leak in ["fact:", "cluster:seed", "session:", "0.42", secret_text] {
        assert!(
            !body.contains(leak),
            "the health page carried {leak}, which is a record identity or a stored value: {body}"
        );
    }

    // (4) An admin's own corpus pages are the ordinary owner path: their rows, and not the foreign one.
    let (status, body) = web.get_full("/debug/memories", Some(&admin)).await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert!(
        !body.contains(secret_text),
        "an admin read a corpus row through the owner page: {}",
        short(&body)
    );
    let own = web.seed_fact(&admin, "the admin's own memory").await;
    let (status, body) = web.get_full("/debug/memories", Some(&admin)).await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert!(
        body.contains(&own),
        "the admin's own row is missing: {}",
        short(&body)
    );
}

#[tokio::test]
async fn the_query_tester_runs_inside_the_callers_population_and_writes_only_owner_heat() {
    let web = Web::hosted().await;
    let alice = web.sign_in("task9-qt-alice").await;
    let bob = web.sign_in("task9-qt-bob").await;
    let alice_fact = web.seed_fact(&alice, "strong shared word").await;
    let bob_fact = web.seed_fact(&bob, "strong shared word in bob").await;
    let form = |csrf: &str, extra: &str| {
        format!("mode=retrieve&query=strong+shared+word&limit=10&{extra}csrf={csrf}")
    };

    let (status, body) = web
        .post_full("/debug/query/run", &form(&alice.csrf, ""), Some(&alice))
        .await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert!(
        body.contains(&alice_fact),
        "alice's own row is missing: {}",
        short(&body)
    );
    assert!(
        !body.contains(&bob_fact),
        "bob's row was returned to alice: {}",
        short(&body)
    );

    // The paired heat controls. A fact written by `create_fact` — scoped or not — has **no**
    // `heat_state` row at all (the store path adds it, not the create), so an assertion that a row was
    // *not* warmed would pass vacuously. Seeding heat explicitly is what makes both halves of the pair
    // mean something, and `heat` below asserts the seed landed before anything is compared.
    web.seed_heat(&alice_fact).await;
    web.seed_heat(&bob_fact).await;
    let alice_before = web.heat(&alice_fact).await.expect("alice's heat row");
    let bob_before = web.heat(&bob_fact).await.expect("bob's heat row");

    let (status, _) = web
        .post(
            "/debug/query/run",
            &form(&alice.csrf, "dry_run=true&"),
            Some(&alice),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let dry = web.heat(&alice_fact).await.expect("alice's heat row");
    assert_eq!(
        dry.access_count, alice_before.access_count,
        "the dry arm recorded an access"
    );

    let (status, _) = web
        .post("/debug/query/run", &form(&alice.csrf, ""), Some(&alice))
        .await;
    assert_eq!(status, StatusCode::OK);
    let wet = web.heat(&alice_fact).await.expect("alice's heat row");
    assert!(
        wet.access_count > alice_before.access_count,
        "the non-dry arm wrote no access, so the fixture proves nothing about the heat write"
    );
    let bob_after = web.heat(&bob_fact).await.expect("bob's heat row");
    assert_eq!(
        bob_after.access_count, bob_before.access_count,
        "alice's query warmed bob's row — the interim store-wide activation arm is still reachable"
    );

    // Recall mode: the same boundary on the cluster path, and no foreign label.
    let (status, body) = web
        .post(
            "/debug/query/run",
            &format!("mode=recall&query=strong+shared+word&csrf={}", alice.csrf),
            Some(&alice),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !body.contains("strong shared word in bob"),
        "recall returned bob's content: {body}"
    );
    assert!(
        !body.contains(&bob_fact),
        "recall carried bob's record id: {body}"
    );
}

#[tokio::test]
async fn the_account_page_lists_only_the_owners_credentials_and_shows_a_secret_once() {
    let web = Web::hosted().await;
    let alice = web.sign_in("task9-acct-alice").await;
    let bob = web.sign_in("task9-acct-bob").await;
    let bob_token = web
        .service()
        .create_agent_token(&web.authorize(&bob).await, "bob-laptop", Utc::now())
        .await
        .expect("bob may mint a token");
    let bob_secret = bob_token.expose().to_string();

    let (status, body) = web.get_full("/debug/account", Some(&alice)).await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert!(
        !body.contains("bob-laptop"),
        "bob's token label leaked: {}",
        short(&body)
    );
    assert!(
        !body.contains(&bob_secret),
        "bob's token secret leaked: {body}"
    );
    assert!(
        !body.contains(&record_id_to_string(bob_token.credential().as_record_id())),
        "a foreign credential id was listed on alice's page: {}",
        short(&body)
    );

    // Minting shows the secret exactly once, on the response that minted it.
    let (status, body) = web
        .post_full(
            "/debug/account/tokens",
            &format!("name=my-laptop&csrf={}", alice.csrf),
            Some(&alice),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert!(
        body.contains("my-laptop"),
        "the minted label is not on the page: {body}"
    );
    let secret = one_time_secret(&body).expect("the minting response carries its secret once");
    assert!(!secret.is_empty());

    let (status, later) = web.get_full("/debug/account", Some(&alice)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !later.contains(&secret),
        "a one-time secret was redisplayed: {}",
        short(&later)
    );
    assert!(
        later.contains("my-laptop"),
        "the token's label should still be listed: {later}"
    );
    assert!(
        !later.contains("data-one-time-secret"),
        "the account page still shows a secret block: {later}"
    );

    // Revoking it works, and the same secret then stops authenticating.
    let listed = credential_rows(&later);
    let token_id = listed
        .iter()
        .find(|(_, label)| label == "my-laptop")
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| panic!("the minted token is not listed: {}", listed_debug(&listed)));
    let (status, body) = web
        .post(
            "/debug/account/credentials/revoke",
            &format!("credential={}&csrf={}", urlencode(&token_id), alice.csrf),
            Some(&alice),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        web.service()
            .authenticate_agent(&secret, Utc::now())
            .await
            .is_err(),
        "a revoked agent token still authenticates"
    );

    // A foreign credential id is not alice's to revoke, and the answer is the same as for an absent one.
    let bob_id = record_id_to_string(bob_token.credential().as_record_id());
    for (label, presented) in [
        ("foreign", bob_id.clone()),
        ("absent", "credential:nope-not-here".to_string()),
    ] {
        let (status, body) = web
            .post(
                "/debug/account/credentials/revoke",
                &format!("credential={}&csrf={}", urlencode(&presented), alice.csrf),
                Some(&alice),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{label} credential id was accepted: {body}"
        );
    }
    assert!(
        web.service()
            .authenticate_agent(&bob_secret, Utc::now())
            .await
            .is_ok(),
        "alice's attempt changed bob's row"
    );
}

/// `(credential id, label)` pairs from the account page's credential table.
fn credential_rows(body: &str) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for chunk in body.split("data-credential=\"").skip(1) {
        let Some((id, rest)) = chunk.split_once('"') else {
            continue;
        };
        let label = rest
            .split("data-label=\"")
            .nth(1)
            .and_then(|tail| tail.split('"').next())
            .unwrap_or_default()
            .to_string();
        rows.push((id.to_string(), label));
    }
    rows
}

// ============================================================================================
// Invitations: an administrator capability, retired from the owner account page (fix F1)
// ============================================================================================

/// The policy this case pins: **invitations are administrator-only**. An owner may recover their own
/// account, but no authenticated owner may widen the account population, because the design reserves
/// account management to the administrator role and the README says an invitation needs one. There is
/// therefore no owner-facing mint at all: `POST /debug/account/invitations` is not mounted, the admin
/// endpoint is the only one, and the owner's account page offers no invitation form. Each of those three
/// is asserted below, because the first without the other two would be a page advertising a route that
/// answers 404.
#[tokio::test]
async fn invitations_are_admin_only_and_no_owner_endpoint_mints_one() {
    let web = Web::hosted().await;
    let owner = web.sign_in("task9-f1-owner").await;
    let admin = web.sign_in_admin("task9-f1-admin").await;

    // (1) The retired owner endpoint mints nothing — for a plain owner *and* for an administrator. A
    //     live route would answer 200 with a secret; a removed one is the router's own 404, or 405 if a
    //     path were kept behind another method. Both refusals are acceptable, a page is not, and testing
    //     the admin here as well is what proves the route is gone rather than merely gated by role.
    for (label, session) in [("owner", &owner), ("admin", &admin)] {
        let (status, body) = web
            .post_full(
                "/debug/account/invitations",
                &format!("csrf={}", session.csrf),
                Some(session),
            )
            .await;
        assert!(
            matches!(
                status,
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "the {label} reached the owner invitation endpoint: {status} / {}",
            short(&body)
        );
        assert!(
            one_time_secret(&body).is_none(),
            "the {label} was handed an invitation secret from the owner endpoint: {}",
            short(&body)
        );
    }

    // (2) A plain owner at the admin endpoint is refused for the role, not for the token: the CSRF half
    //     is presented, so the mutation check cannot be what satisfied this assertion. The guard's
    //     `admin_route` list answers first and `admin_invitation` re-checks the capability itself, so the
    //     refusal text is asserted rather than the status alone — a bare 403 would also be what a missing
    //     token looks like, and would not distinguish the two gates from each other.
    let (status, body) = web
        .post_with_headers(
            "/debug/admin/invitations",
            &format!("csrf={}", owner.csrf),
            Some(&owner),
            &[],
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a plain owner minted an invitation through the admin endpoint: {}",
        short(&body)
    );
    assert!(
        body.contains("administrator"),
        "the refusal should name the missing role, not a missing token: {}",
        short(&body)
    );
    assert!(
        is_plain_denial(&body),
        "the admin handler ran for a plain owner: {}",
        short(&body)
    );

    // (3) The mint moved rather than vanished: the owner's account page offers no invitation form and
    //     still offers self-recovery, while the admin page does offer the form. Each half is the positive
    //     control for the other, so a blanked-out template cannot pass this case.
    let (status, page) = web.get_full("/debug/account", Some(&owner)).await;
    assert_eq!(status, StatusCode::OK, "the owner's own account page broke");
    assert!(
        !page.contains("/debug/account/invitations"),
        "the account page still posts an invitation form: {}",
        short(&page)
    );
    assert!(
        !page.contains("Create an invitation"),
        "the account page still labels a button that creates an invitation: {}",
        short(&page)
    );
    assert!(
        page.contains("action=\"/debug/account/recovery\""),
        "the owner lost self-recovery: {}",
        short(&page)
    );

    let (status, admin_page) = web.get_full("/debug/admin", Some(&admin)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the admin page broke: {}",
        short(&admin_page)
    );
    assert!(
        admin_page.contains("action=\"/debug/admin/invitations\""),
        "the admin page no longer offers the invitation mint: {}",
        short(&admin_page)
    );
}

/// The invitation happy path, on the one endpoint allowed to perform it: minted by an administrator,
/// shown once, and claimable exactly once through the public page. Kept separate from the recovery
/// case because the two mints are different capabilities — an invitation widens the account population,
/// a recovery link only touches the caller's own password — and the owner decision puts the first with
/// the administrator and leaves the second with the owner.
#[tokio::test]
async fn an_admin_mints_an_invitation_and_the_link_claims_once() {
    let web = Web::hosted().await;
    let admin = web.sign_in_admin("task9-invite-admin").await;

    let (status, body) = web
        .post_full(
            "/debug/admin/invitations",
            &format!("csrf={}", admin.csrf),
            Some(&admin),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    let invite = one_time_secret(&body).expect("the invitation response carries its secret once");

    // `mint_link` renders the issuer's **account** page even on the admin route, so "shown once" is
    // measured against the page that actually carried it, and against the admin page it was minted from.
    let (status, later) = web.get_full("/debug/account", Some(&admin)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !later.contains(&invite),
        "the invitation secret was redisplayed on the account page: {}",
        short(&later)
    );
    let (status, admin_page) = web.get_full("/debug/admin", Some(&admin)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !admin_page.contains(&invite),
        "the invitation secret was redisplayed on the admin page: {}",
        short(&admin_page)
    );

    // The link enrols exactly one owner, through the public page.
    let token = web.preauth_token("/debug/invite").await;
    let (status, body) = web
        .post_with_origin(
            "/debug/invite",
            &format!(
                "secret={invite}&login=task9-invited-one&password={INVITED_PASSWORD}&csrf={token}"
            ),
            None,
            HOSTED_ORIGIN,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the invitation was not claimable: {body}"
    );
    let token = web.preauth_token("/debug/invite").await;
    let (status, body) = web
        .post_with_origin(
            "/debug/invite",
            &format!(
                "secret={invite}&login=task9-invited-two&password={INVITED_PASSWORD}&csrf={token}"
            ),
            None,
            HOSTED_ORIGIN,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a spent invitation was claimed twice: {body}"
    );
}

/// Owner self-recovery: an owner mints a recovery link for **their own** account, it rotates the
/// password, ends the live sessions, and the old password dies. Retirement of the owner invitation mint
/// must not take this with it — it acts on no other account, so it stays an owner route.
#[tokio::test]
async fn an_owner_mints_a_recovery_link_for_their_own_account() {
    let web = Web::hosted().await;
    let alice = web.sign_in("task9-invite-alice").await;

    let (status, body) = web
        .post_full(
            "/debug/account/recovery",
            &format!("csrf={}", alice.csrf),
            Some(&alice),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    let recovery = one_time_secret(&body).expect("the recovery response carries its secret once");
    let token = web.preauth_token("/debug/recover").await;
    let (status, body) = web
        .post_with_origin(
            "/debug/recover",
            &format!("secret={recovery}&password=rotated-password-long-enough&csrf={token}"),
            None,
            HOSTED_ORIGIN,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the recovery did not complete: {body}"
    );
    let (status, _, _) = web.get("/debug", Some(&alice)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a pre-recovery session still reads the corpus"
    );
    assert!(
        web.service()
            .login("task9-invite-alice", PASSWORD, Utc::now())
            .await
            .is_err(),
        "the old password still authenticates after recovery"
    );
    let rotated = web
        .service()
        .login(
            "task9-invite-alice",
            "rotated-password-long-enough",
            Utc::now(),
        )
        .await;
    assert!(rotated.is_ok(), "the new password does not authenticate");
}

#[tokio::test]
async fn csrf_guard_is_independent_of_the_diagnostic_context() {
    // Same store, same boundary, `DebugContext` absent. Every check that applies with diagnostics must
    // apply without them, and the dashboard must still render — its config rows are the only thing
    // that may differ.
    let web = Web::hosted_without_diagnostics().await;
    let session = web.sign_in("task9-no-diagnostics").await;
    let (status, body) = web.get_full("/debug", Some(&session)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the dashboard stopped rendering without diagnostics: {}",
        short(&body)
    );
    assert!(
        !body.contains("task9-stub") || body.contains("unavailable"),
        "the config panel should show its unavailable state, not a fake value: {}",
        short(&body)
    );

    let form = format!("mode=retrieve&query=quiet&limit=5&csrf={}", session.csrf);
    let (with_token, _) = web.post("/debug/query/run", &form, Some(&session)).await;
    assert_eq!(
        with_token,
        StatusCode::OK,
        "a valid mutation was refused without diagnostics"
    );
    let (without, _) = web
        .post(
            "/debug/query/run",
            "mode=retrieve&query=quiet&limit=5",
            Some(&session),
        )
        .await;
    assert_eq!(
        without,
        StatusCode::FORBIDDEN,
        "a missing token passed without diagnostics"
    );
    let (foreign, _) = web
        .post_with_origin(
            "/debug/query/run",
            &form,
            Some(&session),
            "https://evil.example",
        )
        .await;
    assert_eq!(
        foreign,
        StatusCode::FORBIDDEN,
        "a foreign origin passed without diagnostics"
    );
    let (hostile, _) = web
        .post_with_headers(
            "/debug/query/run",
            &form,
            Some(&session),
            &[("sec-fetch-site", "cross-site")],
        )
        .await;
    assert_eq!(
        hostile,
        StatusCode::FORBIDDEN,
        "cross-site metadata passed without diagnostics"
    );
    let response = web
        .get_with_host("/debug/memories", "evil.example", Some(&session))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "a foreign Host passed without diagnostics"
    );
}

#[tokio::test]
async fn maintenance_rows_predating_attribution_are_shown_to_no_browser_role() {
    // Decision B: the per-row corpus view is gone, and a row that no run owns is reported only as part
    // of the unattributed aggregate — never as a record identity.
    let web = Web::hosted().await;
    MaintenanceRepo::new(web.db.inner())
        .record(&LogEntry {
            action: "collapse".to_string(),
            source_id: "fact:predating-row".to_string(),
            target_ids: vec!["fact:survivor".to_string()],
            members_moved: 1,
            disposition: Some("soft_delete".to_string()),
            previous_value: None,
        })
        .await
        .expect("a seeded unattributed row");
    let admin = web.sign_in_admin("task9-maintenance-admin").await;
    let (status, body) = web.get_full("/debug/maintenance", Some(&admin)).await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert!(
        !body.contains("fact:predating-row") && !body.contains("fact:survivor"),
        "an unattributed row's identity reached the page: {}",
        short(&body)
    );
    assert!(
        !body.contains("soft_delete"),
        "a per-row disposition reached the health page: {}",
        short(&body)
    );
    // The owner-facing route is the same refusal as for any plain owner.
    let owner = web.sign_in("task9-maintenance-owner").await;
    let (status, _, _) = web.get("/debug/maintenance", Some(&owner)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
