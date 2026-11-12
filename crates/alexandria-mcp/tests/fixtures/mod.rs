//! Shared fixture machinery for Task 5's transport tests: real fixture accounts, a recording MCP service,
//! and an axum router that puts the production gate in front of the production rmcp service.
//!
//! Nothing here stands in for the thing under test. The gate is
//! [`AuthMiddleware`](alexandria_mcp::auth::http::AuthMiddleware), the transport is rmcp's own
//! `StreamableHttpService` over a `LocalSessionManager` this harness also holds, and every identity is a row
//! written by storage's enrollment and credential paths. A case that passes here passed against the machinery
//! a client will actually reach.
//!
//! The fixture *handler* is deliberately not `AlexandriaServer`: these tests are about what the transport
//! does to a request, and a handler that records the principal it was dispatched with — and can be held
//! mid-call — says that directly. Wiring the real tools to a principal is Task 6's change.
//!
//! Shared by two test binaries, so `dead_code` is allowed at the top of this module: an item one of
//! them does not use is not dead in the other, and the alternative is duplicating the whole harness.

#![allow(dead_code)]

//! Bodies are read with `to_bytes`, which returns only when a stream ends. That is what makes the
//! cancellation cases workable without a stream-polling dependency: "still held open" is a read that has not
//! returned, "revocation ended it" is the same read returning, and "no frame after the revocation" is a
//! result-free body.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use alexandria_mcp::auth::admission::{self, Admission, enrollable_login};
use alexandria_mcp::auth::http::{AuthMiddleware, admit};
use alexandria_mcp::auth::{
    AuthEvents, AuthService, CredentialKind, CredentialLease, McpSessionRegistry, StdioIdentity,
};
use alexandria_mcp::error::ServiceError;
use alexandria_storage::access::{AccessRepo, OwnerContext, Principal};
use alexandria_storage::models::auth::PrincipalIds;
use alexandria_storage::models::identity::{AccountId, CredentialId, OperationId};
use alexandria_storage::repos::{AuthRepo, IdentityRepo, PasswordDigest};
use alexandria_storage::{Database, record_id_to_string, schema};
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use chrono::{DateTime, Utc};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::SessionManager;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde_json::{Value as Json, json};
use tokio::sync::Semaphore;
use tokio::time::{Duration, timeout};

/// One shared password for every fixture owner.
///
/// A fixture secret, never a real one. Its digest is built once per test binary because the policy costs
/// ~19 MiB and several cases below enrol more than one owner.
pub const PASSWORD: &str = "correct-horse-battery-staple-task5";

/// A second fixture password, for the cases that rotate or enroll.
pub const ROTATED: &str = "rotated-correct-horse-battery-staple";

/// How long a body read waits before declaring a stream still open.
pub const STREAM_WAIT: Duration = Duration::from_millis(900);

/// How long a request may take to produce its response head at all.
pub const REQUEST_WAIT: Duration = Duration::from_secs(4);

/// The path the gated MCP service is served on.
pub const MCP_PATH: &str = "/mcp";

/// The source label a fixture request carries when no proxy is in front of it.
pub const UNPROXIED_SOURCE: &str = "unproxied";

/// The body limit the gate enforces, re-exported so a test states the number once.
pub const MAX_BODY_BYTES: usize = admission::MAX_BODY_BYTES;

/// The Argon2id digest of [`PASSWORD`], paid for once per test binary.
pub async fn owner_digest() -> &'static PasswordDigest {
    static DIGEST: tokio::sync::OnceCell<PasswordDigest> = tokio::sync::OnceCell::const_new();
    DIGEST
        .get_or_init(|| async {
            AuthRepo::create_password_digest(PASSWORD)
                .await
                .expect("the policy must produce a digest")
        })
        .await
}

/// What the fixture MCP service recorded about the requests it was allowed to handle.
#[derive(Clone, Default)]
pub struct FixtureCalls {
    /// One entry per dispatched tool call, in dispatch order: `"<tool> <account id>"`.
    entered: Arc<Mutex<Vec<String>>>,
    /// Calls that ran to completion. A cancelled stream may leave this below the dispatch count, which is
    /// the difference between stopping a delivery and stopping the work.
    completed: Arc<AtomicUsize>,
    /// One gate per `hold` key, so a test can release one in-flight call without releasing another.
    gates: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}

impl FixtureCalls {
    /// The account ids `<tool>` was dispatched for, in dispatch order.
    pub fn accounts_for(&self, tool: &str) -> Vec<String> {
        let prefix = format!("{tool} ");
        let entered = self.entered.lock().expect("fixture lock poisoned");
        entered
            .iter()
            .filter(|recorded| recorded.starts_with(&prefix))
            .map(|recorded| recorded[prefix.len()..].to_string())
            .collect()
    }

    /// How many times `<tool>` was dispatched at all.
    pub fn count(&self, tool: &str) -> usize {
        self.accounts_for(tool).len()
    }

    /// How many calls ran to completion.
    pub fn completed(&self) -> usize {
        self.completed.load(Ordering::Acquire)
    }

    fn record(&self, tool: &str, account: &str) {
        self.entered
            .lock()
            .expect("fixture lock poisoned")
            .push(format!("{tool} {account}"));
    }

    async fn wait_for_release(&self, key: &str) {
        let gate = {
            let mut gates = self.gates.lock().expect("fixture lock poisoned");
            Arc::clone(
                gates
                    .entry(key.to_string())
                    .or_insert_with(|| Arc::new(Semaphore::new(0))),
            )
        };
        let _permit = gate.acquire().await.expect("a gate is never closed");
    }

    /// Lets one armed `hold` call finish.
    pub fn release(&self, key: &str) {
        let gates = self.gates.lock().expect("fixture lock poisoned");
        if let Some(gate) = gates.get(key) {
            gate.add_permits(1);
        }
    }

    /// Waits until `tool` has been dispatched at least `wanted` times in total.
    pub async fn wait_until_entered(&self, tool: &str, wanted: usize) {
        for _ in 0..600 {
            if self.count(tool) >= wanted {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "`{tool}` was dispatched {} time(s), wanted {wanted}: {:?}",
            self.count(tool),
            self.accounts_for(tool)
        );
    }
}

/// A recording MCP service: the handler side of every transport assertion here.
#[derive(Clone)]
pub struct FixtureServer {
    tool_router: ToolRouter<Self>,
    calls: FixtureCalls,
}

#[tool_router]
impl FixtureServer {
    pub fn new(calls: FixtureCalls) -> Self {
        Self {
            tool_router: Self::tool_router(),
            calls,
        }
    }

    /// Reports the account the transport dispatched this call for.
    ///
    /// The principal is read out of the request context, which is the only way a handler in this design can
    /// learn who is asking — so a wrong or absent identity here is a transport defect, not a fixture one.
    #[tool(description = "Report the account the caller was admitted as.")]
    async fn whoami(&self, ctx: RequestContext<RoleServer>) -> Result<String, ErrorData> {
        let account = match alexandria_mcp::auth::principal_from_request(&ctx) {
            Ok(principal) => record_id_to_string(principal.account().as_record_id()),
            Err(error) => {
                self.calls.record("whoami-refused", "none");
                return Err(ErrorData::invalid_request(error.to_string(), None));
            }
        };
        self.calls.record("whoami", &account);
        self.finish();
        Ok(account)
    }

    /// Enters, then waits until the test releases it.
    ///
    /// The shape an in-flight request needs: the handler is demonstrably running, so a revocation that
    /// lands now lands *during* work rather than before it.
    ///
    /// It also watches `ctx.ct`, the per-request cancellation token rmcp hands every handler: a
    /// revoked-and-closed session must stop the *work*, not merely the delivery, and the only way to
    /// tell those apart is for the handler to notice. The fixture records which half happened.
    #[tool(description = "Wait until the test releases it.")]
    async fn hold(&self, ctx: RequestContext<RoleServer>) -> Result<String, ErrorData> {
        let principal = alexandria_mcp::auth::principal_from_request(&ctx)
            .map_err(|error| ErrorData::invalid_request(error.to_string(), None))?;
        let account = record_id_to_string(principal.account().as_record_id());
        self.calls.record("hold", &account);
        // The lease token, not `ctx.ct`: rmcp cancels a request's own token when *its response* is sent
        // or when the client cancels, and a session closed under a revocation does neither. A handler
        // that gated on `ctx.ct` would finish and commit after its credential was revoked, which is what
        // Task 6's writes ride on.
        let lease = alexandria_mcp::auth::lease_from_request(&ctx)
            .map_err(|error| ErrorData::invalid_request(error.to_string(), None))?;
        tokio::select! {
            _ = self.calls.wait_for_release(&account) => {
                self.finish();
                Ok("released".to_string())
            }
            _ = lease.cancellation().cancelled() => {
                self.calls.record("hold-cancelled", &account);
                Err(ErrorData::invalid_request(
                    "the credential behind the request was revoked before the handler finished".to_string(),
                    None,
                ))
            }
        }
    }

    fn finish(&self) {
        self.calls.completed.fetch_add(1, Ordering::AcqRel);
    }
}

#[tool_handler]
impl ServerHandler for FixtureServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("task5-fixture", "0.0.0"))
    }
}

/// Two enrolled owners with live agent tokens.
pub struct Accounts {
    pub alice: FixtureAccount,
    pub bob: FixtureAccount,
}

/// One fixture account: the ids it owns, the bearer text that reaches the transport, and the credential
/// row behind that text.
pub struct FixtureAccount {
    pub ids: PrincipalIds,
    pub bearer: String,
    pub credential: CredentialId,
}

impl FixtureAccount {
    /// The account id as it renders, which is what the fixture handler reports.
    pub fn account_id(&self) -> String {
        record_id_to_string(self.ids.account.as_record_id())
    }
}

/// A store, a gate, a router, and the handles the tests need to drive both.
pub struct Harness {
    pub db: Arc<Database>,
    pub service: Arc<AuthService>,
    pub events: AuthEvents,
    pub admission: Admission,
    pub registry: McpSessionRegistry,
    pub gate: AuthMiddleware,
    pub router: Router,
    pub calls: FixtureCalls,
    /// The SDK session manager this harness's gate closes on revocation, so a test can ask the SDK
    /// whether a session still exists instead of inferring it from a map.
    pub sessions: Arc<LocalSessionManager>,
    /// This store's single system capability, claimed at construction in the order startup uses: connect,
    /// admit, migrate, claim, then admit requests. It is what issuing a trusted-local fixture principal
    /// needs, and nothing on the request path can reach it.
    system: alexandria_storage::access::SystemContext,
    enrolled: Arc<Mutex<HashMap<String, PrincipalIds>>>,
}

impl Harness {
    /// A hosted harness with the transport config the binary uses: legacy sessions on, so the handshake
    /// path and the stateless path are both served, as they are in production.
    pub async fn hosted() -> Self {
        Self::built(StreamableHttpServerConfig::default()).await
    }

    /// A harness with sessions off, so every request takes the stateless path.
    pub async fn stateless() -> Self {
        Self::built(
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(false)
                .with_json_response(true),
        )
        .await
    }

    /// A harness over an arbitrary transport config.
    pub async fn built(config: StreamableHttpServerConfig) -> Self {
        let mut db = Database::connect_embedded()
            .await
            .expect("an in-memory store");
        schema::migrate(db.inner())
            .await
            .expect("the schema, including v009, must apply");
        let system = db
            .claim_system_authority()
            .expect("a fresh store must issue its system capability once");
        let db = Arc::new(db);
        let sessions = Arc::new(LocalSessionManager::default());
        let events = AuthEvents::closing_sessions_in(Arc::clone(&sessions));
        let admission = Admission::bound_to(events.clone());
        let service = Arc::new(AuthService::admitting(Arc::clone(&db), events.clone()));
        let registry = McpSessionRegistry::over(&events);
        let gate =
            AuthMiddleware::admitting(Arc::clone(&service), admission.clone(), registry.clone());
        let calls = FixtureCalls::default();
        let fixture = FixtureServer::new(calls.clone());
        let mcp: StreamableHttpService<FixtureServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(fixture.clone()),
                Arc::clone(&sessions),
                config.with_sse_keep_alive(Some(Duration::from_millis(50))),
            );
        let router = Router::new()
            .nest_service(MCP_PATH, mcp)
            .layer(axum::middleware::from_fn_with_state(gate.clone(), admit));
        Self {
            db,
            service,
            events,
            admission,
            registry,
            gate,
            router,
            calls,
            sessions,
            system,
            enrolled: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The clock the fixture quotes when it calls storage directly. The transport never sees it.
    pub fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    /// Enrols an owner through the public enrollment path with a policy digest, once per login.
    pub async fn enroll(&self, login: &str) -> PrincipalIds {
        if let Some(ids) = self
            .enrolled
            .lock()
            .expect("enrolled map poisoned")
            .get(login)
        {
            return ids.clone();
        }
        let ids = IdentityRepo::new(self.db.inner())
            .bootstrap_owner(
                login,
                owner_digest().await.as_str(),
                &OperationId::new(format!("task5-{login}")),
            )
            .await
            .unwrap_or_else(|error| panic!("enrolling `{login}` failed: {error:?}"));
        self.enrolled
            .lock()
            .expect("enrolled map poisoned")
            .insert(login.to_string(), ids.clone());
        ids
    }

    /// Signs `login` in and authorizes the result: the pair of calls a transport makes, and what minting a
    /// token needs.
    pub async fn owner_context(&self, login: &str) -> (PrincipalIds, OwnerContext) {
        let ids = self.enroll(login).await;
        let grant = self
            .service
            .login(login, PASSWORD, self.now())
            .await
            .unwrap_or_else(|error| panic!("a valid login for `{login}` failed: {error:?}"));
        let principal = self
            .service
            .authenticate_browser(grant.expose_cookie(), self.now())
            .await
            .expect("the session just granted must authenticate");
        let owner = self.authorize(&principal).await;
        (ids, owner)
    }

    pub async fn authorize(&self, principal: &Principal) -> OwnerContext {
        AccessRepo::new(&self.db)
            .authorize(principal)
            .await
            .expect("the authenticated principal should authorize")
    }

    /// The principal a bearer text authenticates to, without a lease. Used to build the `OwnerContext` a
    /// revocation needs.
    pub async fn principal_of(&self, bearer: &str) -> Principal {
        self.service
            .authenticate_agent(bearer, self.now())
            .await
            .expect("the fixture token must authenticate")
    }

    /// Enrols `login` and mints one agent token for it.
    pub async fn agent(&self, login: &str, name: &str) -> FixtureAccount {
        let (ids, owner) = self.owner_context(login).await;
        let token = self
            .service
            .create_agent_token(&owner, name, self.now())
            .await
            .expect("an owner may mint their own token");
        FixtureAccount {
            ids,
            bearer: token.expose().to_string(),
            credential: token.credential().clone(),
        }
    }

    /// Two owners, `task5-alice` and `task5-bob`, with live agent tokens.
    pub async fn pair(&self) -> Accounts {
        Accounts {
            alice: self.agent("task5-alice", "alice-laptop").await,
            bob: self.agent("task5-bob", "bob-laptop").await,
        }
    }

    /// The trusted-local identity of `login`, issued exactly the way the binary's stdio path issues it:
    /// this instance's startup capability plus an account that exists and is active here.
    pub async fn stdio_identity(&self, login: &str) -> StdioIdentity {
        let ids = self.enroll(login).await;
        let principal = AccessRepo::new(&self.db)
            .local_principal(&self.system, &ids.account)
            .await
            .expect("the configured local account must issue a trusted-local principal");
        StdioIdentity::issued(principal)
    }

    /// Rebuilds this harness's gate with a configured stdio identity, as `main.rs` would.
    pub fn with_stdio_identity(mut self, identity: StdioIdentity) -> Self {
        let gate = self.gate.clone().with_stdio_identity(identity);
        let calls = self.calls.clone();
        let sessions = Arc::clone(&self.sessions);
        self.router = Router::new()
            .nest_service(
                MCP_PATH,
                StreamableHttpService::new(
                    move || Ok(FixtureServer::new(calls.clone())),
                    sessions,
                    StreamableHttpServerConfig::default()
                        .with_sse_keep_alive(Some(Duration::from_millis(50))),
                ),
            )
            .layer(axum::middleware::from_fn_with_state(gate.clone(), admit));
        self.gate = gate;
        self
    }

    /// The credential row behind a credential id, as stored.
    pub async fn row(&self, credential: &CredentialId) -> Json {
        let mut response = self
            .db
            .inner()
            .query("SELECT * FROM ONLY $target;")
            .bind(("target", credential.as_record_id().clone()))
            .await
            .expect("a readable credential row")
            .check()
            .expect("a readable credential row");
        let rows: Vec<Json> = response.take(0).expect("a readable credential row");
        assert_eq!(rows.len(), 1, "no row behind the credential");
        rows.into_iter().next().expect("one row")
    }

    /// Sets columns on a credential row, for the cases that must put a row past a deadline without waiting
    /// for one.
    ///
    /// A `datetime` bind is passed as an RFC 3339 instant and cast in the statement: binding the text alone
    /// reaches the schema as a string, which `credential.expires_at`'s type assertion correctly refuses.
    pub async fn set_credential(&self, id: &CredentialId, sets: &str, binds: Vec<(&str, Json)>) {
        let mut query = self
            .db
            .inner()
            .query(format!("UPDATE ONLY $target SET {sets} RETURN NONE;"));
        query = query.bind(("target", id.as_record_id().clone()));
        for (name, value) in binds {
            query = query.bind((name, value));
        }
        query
            .await
            .expect("a writable credential row")
            .check()
            .expect("a writable credential row");
    }

    /// Sets columns on an account row — the suspension and epoch-bump states a hosted request has to feel.
    pub async fn set_account(&self, account: &AccountId, sets: &str, binds: Vec<(&str, Json)>) {
        let mut query = self
            .db
            .inner()
            .query(format!("UPDATE ONLY $target SET {sets} RETURN NONE;"));
        query = query.bind(("target", account.as_record_id().clone()));
        for (name, value) in binds {
            query = query.bind((name, value));
        }
        query
            .await
            .expect("a writable account row")
            .check()
            .expect("a writable account row");
    }

    /// How many accounts the store holds, so an enrollment refusal can be proven to have written nothing.
    pub async fn account_count(&self) -> usize {
        self.accounts_named().await.len()
    }

    /// The logins the store holds.
    pub async fn accounts_named(&self) -> Vec<String> {
        let rows: Vec<Json> = self
            .db
            .inner()
            .query("SELECT login FROM account;")
            .await
            .expect("a readable account table")
            .check()
            .expect("a readable account table")
            .take(0)
            .expect("a readable account table");
        let mut logins: Vec<String> = rows
            .iter()
            .filter_map(|row| row["login"].as_str().map(str::to_string))
            .collect();
        logins.sort();
        logins
    }

    /// Whether the store holds an account with this login, checked through storage's own lookup.
    pub async fn account_by_login_exists(&self, login: &str) -> bool {
        AuthRepo::new(self.db.inner())
            .account_by_login(login)
            .await
            .expect("a readable account table")
            .is_some()
    }

    /// Whether storage's own validator would accept this login, and how it normalises it.
    ///
    /// `account_by_login` runs storage's enrollment normaliser before anything else, so its
    /// `InvalidInput` *is* storage's answer to "would the schema accept this login" — which is the half
    /// the MCP gate restates, and the reason the two can be compared input by input.
    pub async fn store_accepts_login(&self, login: &str) -> Result<String, ServiceError> {
        AuthRepo::new(self.db.inner())
            .account_by_login(login)
            .await
            .map(|_| alexandria_storage::models::auth::normalize_login(login))
            .map_err(Into::into)
    }

    /// A live lease for `bearer`, through the same call the transport makes.
    pub async fn lease(&self, bearer: &str) -> Result<CredentialLease, ServiceError> {
        self.service
            .admit(bearer, CredentialKind::Agent, self.now())
            .await
    }

    /// The commit-time recheck, over this harness's store.
    pub async fn revalidate(&self, lease: &CredentialLease) -> Result<OwnerContext, ServiceError> {
        alexandria_mcp::auth::revalidate(lease, &AccessRepo::new(&self.db)).await
    }

    /// Revokes a credential through the service, which is the path that must cancel live work.
    pub async fn revoke(&self, owner: &OwnerContext, credential: &CredentialId) {
        self.service
            .revoke_credential(owner, credential, self.now())
            .await
            .expect("the owner may revoke their own credential");
    }

    /// Whether the SDK still holds a session.
    pub async fn sdk_has_session(&self, session: &str) -> bool {
        self.sessions
            .has_session(&session.into())
            .await
            .expect("the session manager answers")
    }

    // -- wire helpers -----------------------------------------------------------------------------

    /// Sends one request through the router, bounded by [`REQUEST_WAIT`].
    ///
    /// The bound exists because a refused-for-budget request parks in the queue rather than answering: a
    /// test that expected a refusal would otherwise hang the whole suite, and a hang tells you nothing
    /// about which assertion was right.
    pub async fn send(&self, request: Request) -> axum::response::Response {
        use tower::ServiceExt;
        match timeout(REQUEST_WAIT, self.router.clone().oneshot(request)).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => panic!("the router refused the request: {error}"),
            Err(_) => panic!("the router had not answered within the request wait"),
        }
    }

    /// Sends one request and reads its whole body, bounded by [`STREAM_WAIT`].
    pub async fn send_and_read(&self, request: Request) -> (StatusCode, HeaderMap, Vec<u8>) {
        let response = self.send(request).await;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = read_body(response.into_body()).await;
        (status, headers, bytes)
    }

    /// Sends one request and reads its body as text.
    pub async fn send_and_read_text(&self, request: Request) -> (StatusCode, String) {
        let (status, _, bytes) = self.send_and_read(request).await;
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A JSON-RPC POST with the headers a real MCP client sends.
    pub fn post(&self, body: Json) -> Request<Body> {
        let bytes = serde_json::to_vec(&body).expect("serializable");
        Request::builder()
            .method("POST")
            .uri(MCP_PATH)
            .header("host", "localhost")
            .header(CONTENT_TYPE, "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(bytes))
            .expect("a valid request")
    }

    /// A GET that attaches an SSE stream to a legacy session.
    pub fn get_stream(&self, session: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .method("GET")
            .uri(MCP_PATH)
            .header("host", "localhost")
            .header("accept", "text/event-stream")
            .header(
                "mcp-protocol-version",
                ProtocolVersion::V_2025_06_18.as_str(),
            );
        if !session.is_empty() {
            builder = builder.header(alexandria_mcp::auth::SESSION_ID_HEADER, session);
        }
        builder.body(Body::empty()).expect("a valid request")
    }

    /// A DELETE that ends a legacy session.
    pub fn delete(&self, session: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(MCP_PATH)
            .header("host", "localhost")
            .header(alexandria_mcp::auth::SESSION_ID_HEADER, session)
            .header(
                "mcp-protocol-version",
                ProtocolVersion::V_2025_06_18.as_str(),
            )
            .body(Body::empty())
            .expect("a valid request")
    }

    /// Performs the handshake and the `initialized` notification, and returns the session id rmcp issued.
    ///
    /// The notification is asserted as accepted: a binding that only covered the request kind would show up
    /// here first.
    pub async fn handshake(&self, bearer: &str) -> String {
        let response = self
            .send(with_bearer(self.post(initialize(1)), bearer))
            .await;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = read_body(response.into_body()).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the initialize handshake was not accepted: {}",
            String::from_utf8_lossy(&bytes)
        );
        let frames = frames_of(&bytes);
        assert!(
            frames
                .iter()
                .any(|frame| frame.get("result").is_some() && frame.get("error").is_none()),
            "the handshake produced no result: {frames:?}"
        );
        let session = session_of(&headers).expect("initialize must return a session id");
        let notified = self
            .send(with_bearer(
                with_session(self.post(initialized()), &session),
                bearer,
            ))
            .await;
        assert_eq!(
            notified.status(),
            StatusCode::ACCEPTED,
            "the initialized notification was not accepted"
        );
        session
    }

    /// Calls a tool inside a legacy session and returns the frames that came back.
    pub async fn call_in_session(
        &self,
        bearer: &str,
        session: &str,
        name: &'static str,
    ) -> Vec<Json> {
        let response = self
            .send(with_bearer(
                with_session(self.post(tool_call(2, name)), session),
                bearer,
            ))
            .await;
        let bytes = read_body(response.into_body()).await;
        frames_of(&bytes)
    }

    /// Sends a tool call without waiting for it, for the cases that must act *while* the handler runs.
    pub fn spawn_call(
        &self,
        account: &FixtureAccount,
        session: &str,
        name: &'static str,
    ) -> tokio::task::JoinHandle<(StatusCode, Vec<Json>)> {
        let router = self.router.clone();
        let bearer = account.bearer.clone();
        let session = session.to_string();
        tokio::spawn(async move {
            use tower::ServiceExt;
            let request = with_bearer(
                with_session(router_post(tool_call(3, name)), &session),
                &bearer,
            );
            let response = router.oneshot(request).await.expect("the router serves");
            let status = response.status();
            (status, frames_of(&read_body(response.into_body()).await))
        })
    }

    /// Waits until `wanted` expensive requests are waiting in the admission queue.
    /// Waits for the admission scheduler to return to fully idle, then fails by name if it does not.
    ///
    /// Why this exists rather than a direct assertion: the permit for a request is released on the
    /// *server-side* task when its last handle dies, not inside the handler, so a client that has
    /// already joined its own futures can still be racing that release. `expensive_active() == 0` read
    /// immediately after `tokio::join!` therefore fails intermittently under load while no slot is
    /// actually leaking — the shape Task 5's review recorded and carried forward as a Task 6 named
    /// precondition rather than leaving as an observation.
    ///
    /// This is a *bounded settle*, not a sleep and not a weakened assertion. The invariant being proven
    /// is "capacity returns", and a genuine leak never settles, so the same assertion still fails after
    /// the deadline — with a message that names the counters. Same 600 x 5 ms shape as
    /// [`Self::wait_admission_queued`] and `wait_until_entered`, so the three helpers cannot disagree
    /// about how long a healthy store is allowed to take.
    pub async fn wait_admission_idle(&self) {
        for _ in 0..600 {
            if self.admission.expensive_active() == 0 && self.admission.expensive_queued() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "admission never returned to idle: {} active permit(s), {} queued waiter(s)",
            self.admission.expensive_active(),
            self.admission.expensive_queued()
        );
    }

    pub async fn wait_admission_queued(&self, wanted: usize) {
        for _ in 0..600 {
            if self.admission.expensive_queued() >= wanted {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "the queue held {} waiter(s), wanted {wanted}",
            self.admission.expensive_queued()
        );
    }
}

use rmcp::transport::streamable_http_server::StreamableHttpService;

/// Attaches a bearer token to a request.
pub fn with_bearer(mut request: Request<Body>, token: &str) -> Request<Body> {
    request.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {token}").parse().expect("a bearer header"),
    );
    request
}

/// Attaches a session id to a request.
pub fn with_session(mut request: Request<Body>, session: &str) -> Request<Body> {
    request.headers_mut().insert(
        alexandria_mcp::auth::SESSION_ID_HEADER,
        session.parse().expect("a session header"),
    );
    request
}

/// Sets one header on a request, for the cases that must lie about size or date.
pub fn with_header(mut request: Request<Body>, name: &str, value: &str) -> Request<Body> {
    request.headers_mut().insert(
        axum::http::HeaderName::try_from(name).expect("a header name"),
        value.parse().expect("a header value"),
    );
    request
}

/// Attaches a cookie carrying a browser session, for the case that says a cookie is not a bearer token.
pub fn with_cookie(mut request: Request<Body>, cookie: &str) -> Request<Body> {
    request.headers_mut().insert(
        axum::http::header::COOKIE,
        format!("alexandria_session={cookie}")
            .parse()
            .expect("a cookie header"),
    );
    request
}

/// Adds the SEP-2243 standard headers a stateless request must carry.
pub fn with_standard_headers(
    mut request: Request<Body>,
    method: Option<&str>,
    name: Option<&str>,
) -> Request<Body> {
    request.headers_mut().insert(
        "mcp-protocol-version",
        ProtocolVersion::V_2026_07_28
            .as_str()
            .parse()
            .expect("a version header"),
    );
    if let Some(method) = method {
        request
            .headers_mut()
            .insert("mcp-method", method.parse().expect("a method header"));
    }
    if let Some(name) = name {
        request
            .headers_mut()
            .insert("mcp-name", name.parse().expect("a name header"));
    }
    request
}

/// A POST built without a harness, for a spawned request.
pub fn router_post(body: Json) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(MCP_PATH)
        .header("host", "localhost")
        .header(CONTENT_TYPE, "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(serde_json::to_vec(&body).expect("serializable")))
        .expect("a valid request")
}

/// Reads a body to the end, giving up after [`STREAM_WAIT`]. `None` means the stream was still open when
/// the wait expired, which is how "held open" and "ended" are told apart.
pub async fn read_body_opt(body: Body) -> Option<Vec<u8>> {
    match timeout(STREAM_WAIT, axum::body::to_bytes(body, usize::MAX)).await {
        Ok(Ok(bytes)) => Some(bytes.to_vec()),
        Ok(Err(_)) => Some(Vec::new()),
        Err(_) => None,
    }
}

/// Reads a body, requiring that it ended.
pub async fn read_body(body: Body) -> Vec<u8> {
    read_body_opt(body)
        .await
        .expect("the response body ended within the wait")
}

/// The JSON payloads of an SSE body, in arrival order.
pub fn sse_data(bytes: &[u8]) -> Vec<Json> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|data| !data.is_empty())
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

/// The JSON-RPC payloads of a response, whether it arrived as an SSE stream or as one JSON document: the
/// legacy path answers with the first and the stateless JSON path with the second, and an assertion about
/// "no frame" has to cover both.
pub fn frames_of(bytes: &[u8]) -> Vec<Json> {
    let streamed = sse_data(bytes);
    if !streamed.is_empty() {
        return streamed;
    }
    serde_json::from_slice::<Json>(bytes)
        .map(|single| vec![single])
        .unwrap_or_default()
}

/// The text content of every successful tool result in a frame set.
pub fn result_text(frames: &[Json]) -> Vec<String> {
    let mut texts = Vec::new();
    for frame in frames {
        let Some(result) = frame.get("result") else {
            continue;
        };
        let Some(content) = result.get("content").and_then(|content| content.as_array()) else {
            continue;
        };
        for block in content {
            if let Some(text) = block.get("text").and_then(|text| text.as_str()) {
                texts.push(text.to_string());
            }
        }
    }
    texts
}

/// The `initialize` body, at the version that gets a session.
pub fn initialize(id: i64) -> Json {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": ProtocolVersion::V_2025_06_18.as_str(),
            "capabilities": {},
            "clientInfo": {"name": "task5-fixture-client", "version": "0.0.0"},
        }
    })
}

/// The `notifications/initialized` body a legacy client sends after the handshake.
pub fn initialized() -> Json {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    })
}

/// A `tools/call` body.
pub fn tool_call(id: i64, name: &str) -> Json {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": {}},
    })
}

/// A stateless `tools/call` at the version that requires the standard headers, carrying the `_meta` the SDK
/// requires with it.
pub fn stateless_tool_call(id: i64, name: &str) -> Json {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": name,
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": ProtocolVersion::V_2026_07_28.as_str(),
                "io.modelcontextprotocol/clientInfo": {
                    "name": "task5-fixture-client", "version": "0.0.0"
                },
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        },
    })
}

/// The session id a response carries, if any.
pub fn session_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(alexandria_mcp::auth::SESSION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// The rendered id of a credential.
pub fn id_of_credential(credential: &CredentialId) -> String {
    record_id_to_string(credential.as_record_id())
}

/// The enrollment order under test, driven through the gate's own method so a route and a test cannot
/// disagree about it.
pub async fn enroll_over_http(
    harness: &Harness,
    secret: &str,
    login: &str,
    password: &str,
) -> Result<PrincipalIds, ServiceError> {
    harness
        .gate
        .enroll(UNPROXIED_SOURCE, secret, login, password)
        .await
}

/// The gate's login gate, re-exported so the drift test compares it against storage.
pub fn gate_login(login: &str) -> Result<String, ServiceError> {
    enrollable_login(login)
}
