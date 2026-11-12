//! The HTTP admission gate: every request authenticated, every principal sealed, every stream owned.
//!
//! This is the seam the plan's D1 posture puts in front of the MCP service. rmcp's
//! `StreamableHttpService` is unauthenticated by design — it answers an `initialize` from anybody who can
//! open a socket — so `main.rs` mounts it behind [`admit`], an axum middleware that decides, per request:
//! who is presenting, whether their credential is still live, whether the session id they name is theirs,
//! and whether this process can take the work at all. Only then is the request forwarded, with the
//! resulting [`Principal`](alexandria_storage::access::Principal) attached to the HTTP `Parts` that rmcp hands every handler context.
//!
//! Four rules hold the module together, and each is a boundary the design chose rather than a taste.
//!
//! **The clock is the server's.** [`AuthService::admit`] takes `now`, and every call here passes
//! `Utc::now()` — no request body, header, or forwarded date reaches it. A client that could name the
//! instant could roll an idle window forward and keep a session that had already expired, so the
//! transport gives it no way to say what time it is
//! (`mcp_auth_test::a_client_supplied_clock_moves_no_deadline`).
//!
//! **A bearer header is the only presentation.** `Authorization: Bearer <agent token>`, and nothing else.
//! A browser session cookie authenticates a *browser*; reading one here would turn a victim's cookie jar
//! into a machine credential, so the `Cookie` header is not parsed at all. A cookie value presented *as* a
//! bearer token fails because storage keeps the two kinds in separate rows and this path asks for the agent
//! kind — the boundary is the credential table, not a header-name heuristic that could be made to agree by
//! a future refactor.
//!
//! **A response is gated by the lifetime that admitted it**, in two places, because they cover different
//! windows. A request whose lease is cancelled *before* the transport has produced an answer is dropped
//! rather than answered — and dropping the forwarded future is exactly the signal rmcp's stateless path
//! treats as a disconnect, which cancels its handler. Once an answer is streaming, the gate is the session
//! binding: a revocation cancels the binding's token and closes the session through rmcp's own session
//! manager, which ends the session worker and therefore every stream it owns, so a frame that was already
//! queued cannot reach a caller whose authority has ended.
//!
//! What that does **not** do is cut an in-flight *stateless* response between its first byte and its
//! answer: this gate deliberately does not re-implement the SDK's body stream, and no dependency already
//! in the tree exposes a stream combinator to do it with. A revoked stateless caller can therefore still
//! receive the one answer its request was producing. It cannot start another one, cannot act on a session
//! it does not own, and cannot commit a write whose recheck comes after the revocation — which is why
//! [`revalidate`](crate::auth::mcp_transport::revalidate) and Task 6's in-transaction epoch predicate are
//! the other half of "an in-flight revoked request cannot commit", rather than a nicety.
//! `mcp_auth_test::revocation_ends_an_open_stream_and_delivers_no_frame_after_it` exercises a real held
//! stream, as the brief requires.
//!
//! **Work is bounded before it is started.** The body is read here, not left to the SDK, for one reason:
//! the queue cannot classify a request it has not looked at, and a 429 that arrives after the embedding
//! provider has been called is not a bound. [`classify`] therefore maps a JSON-RPC `tools/call` to the
//! expensive class and refuses over-budget arrivals with [`ServiceError::Quota`] — which the caller sees as
//! a `429` with no handler invocation, asserted by
//! `mcp_auth_test::a_saturated_account_is_refused_before_the_handler_runs`.
//!
//! The 401 / 404 / 413 / 429 bodies are rendered by [`Denial`], which carries a status and a
//! [`ServiceError`] and nothing else: no SQL, no cause chain, no table name, no account or credential id,
//! no digest. That is the client-visible half of P2-8, and
//! `mcp_auth_test::every_transport_denial_body_carries_no_internals_or_secrets` scans these bodies as they
//! actually leave this handler.

use crate::auth::admission::{Admission, MAX_BODY_BYTES, RequestClass, enrollable_login};
use crate::auth::mcp_transport::{
    AuthEvents, CredentialKind, CredentialLease, McpSessionRegistry, Revocation, StdioIdentity,
};
use crate::auth::service::AuthService;
use crate::error::ServiceError;
use axum::body::Body;
use axum::body::to_bytes;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::header::CONTENT_LENGTH;
use axum::http::header::CONTENT_TYPE;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use chrono::Utc;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use std::sync::Arc;

/// The `Mcp-Session-Id` header, spelled once.
///
/// A literal rather than rmcp's constant because the pinned version does not expose it publicly;
/// `mcp_auth_test::the_session_header_is_the_one_rmcp_writes` asserts this string against the header rmcp
/// actually sets on an `initialize` response, so the two cannot drift quietly.
pub const SESSION_ID_HEADER: &str = "mcp-session-id";

/// The bearer scheme, compared case-insensitively as RFC 9110 requires.
const BEARER: &str = "bearer";

/// Everything the admission gate needs, and the only thing `main.rs` builds for `/mcp`.
///
/// Cloneable and cheap: the service, the event bus, the session registry and the admission control are
/// shared handles over one state each, so a cloned gate and the gate it came from cannot disagree about who
/// is admitted.
#[derive(Clone)]
pub struct AuthMiddleware {
    inner: Arc<Inner>,
}

/// `Clone` so the refinement builders below can adjust a gate that has already been shared without
/// mutating somebody else's policy; every field is itself a shared handle, so cloning copies no truth.
#[derive(Clone)]
struct Inner {
    service: Arc<AuthService>,
    /// The bus the service mints leases into, the registry binds sessions against, and the admission
    /// control asks whether queued work is still entitled. Held here so one object owns the boundary.
    events: AuthEvents,
    sessions: McpSessionRegistry,
    admission: Admission,
    /// The identity a stdio transport acts as, when the operator configured one. Carried so one object
    /// holds both hosted and local admission policy — and never consulted by the HTTP path, where a
    /// request that presented no credential is refused rather than mapped onto the operator.
    stdio: Option<StdioIdentity>,
    /// Largest body this gate will read, and therefore the largest `Content-Length` it will accept.
    max_body_bytes: usize,
}

impl std::fmt::Debug for AuthMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthMiddleware")
            .field("stdio_configured", &self.inner.stdio.is_some())
            .field("max_body_bytes", &self.inner.max_body_bytes)
            .field("sessions_bound", &self.inner.sessions.bound())
            .finish()
    }
}

impl AuthMiddleware {
    /// A gate that admits through `service`, queues through `admission`, and terminates the sessions the
    /// registry holds.
    ///
    /// All three must share one [`AuthEvents`] — which is why the registry and the admission control are
    /// parameters rather than things this constructor invents. Two buses would be two answers to "who has
    /// been revoked?", and the one the streams are gated by could be the one that never learns. Build them
    /// in that order: `AuthEvents` → `AuthService::admitting` → `Admission::bound_to` →
    /// `McpSessionRegistry::over` → this.
    pub fn admitting(
        service: Arc<AuthService>,
        admission: Admission,
        sessions: McpSessionRegistry,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                events: service.events(),
                stdio: None,
                max_body_bytes: MAX_BODY_BYTES,
                service,
                admission,
                sessions,
            }),
        }
    }

    /// The same gate with a configured stdio identity.
    ///
    /// The value can only have come from
    /// [`AccessRepo::local_principal`](alexandria_storage::access::AccessRepo::local_principal), which
    /// needs this database instance's startup capability; the gate cannot invent one, and the HTTP path
    /// never reads it.
    pub fn with_stdio_identity(mut self, identity: StdioIdentity) -> Self {
        Arc::make_mut(&mut self.inner).stdio = Some(identity);
        self
    }

    /// The same gate with a narrower body limit.
    ///
    /// Widening [`MAX_BODY_BYTES`] is not offered: the limit is a tunable the brief sets once and raises
    /// from measurements, not a deployment knob.
    pub fn with_body_limit(mut self, bytes: usize) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        inner.max_body_bytes = bytes.min(MAX_BODY_BYTES);
        self
    }

    /// The authentication service behind this gate.
    pub fn service(&self) -> &Arc<AuthService> {
        &self.inner.service
    }

    /// The invalidation bus this gate publishes through.
    pub fn events(&self) -> &AuthEvents {
        &self.inner.events
    }

    /// The session-ownership registry this gate checks and binds.
    pub fn sessions(&self) -> &McpSessionRegistry {
        &self.inner.sessions
    }

    /// The work bound this gate queues through.
    pub fn admission(&self) -> &Admission {
        &self.inner.admission
    }

    /// The configured stdio identity, if the operator configured one.
    pub fn stdio_identity(&self) -> Option<&StdioIdentity> {
        self.inner.stdio.as_ref()
    }

    /// The body limit in force.
    pub fn max_body_bytes(&self) -> usize {
        self.inner.max_body_bytes
    }

    /// Verifies a presented bearer secret and registers the lease it entitles.
    ///
    /// Split out from [`admit`] because the two steps are the seam the revocation race is measured at: a
    /// test can hold a live lease, revoke underneath it, and then ask the registry to register — which is
    /// what the middleware does after rmcp returns a session id.
    pub async fn admit_presentation(
        &self,
        presented: &str,
    ) -> Result<CredentialLease, ServiceError> {
        self.inner
            .service
            .admit(presented, CredentialKind::Agent, Utc::now())
            .await
    }

    /// Whether `lease` may act on `session` right now.
    pub fn check_session(
        &self,
        session: &str,
        lease: &CredentialLease,
    ) -> Result<(), ServiceError> {
        self.inner.sessions.check(session, lease)
    }

    /// Binds a session id rmcp returned to `lease`, refusing a lease whose generation has moved.
    pub fn register_session(
        &self,
        session: &str,
        lease: &CredentialLease,
    ) -> Result<(), ServiceError> {
        self.inner.sessions.bind(session, lease)
    }

    /// Revokes a credential or an account: cancels its leases, drops its bindings, closes its sessions.
    pub async fn revoke(&self, target: &Revocation) -> usize {
        self.inner.sessions.revoke(target).await
    }

    /// Refuses a transport config that would restore sessions this gate cannot authorise.
    ///
    /// An external session store replays an `initialize` for a session id another process created and
    /// persists *no* account or credential alongside it, so the restored session would be a bare rmcp id —
    /// data, not authority, and precisely what this module exists to refuse. The pinned version exposes the
    /// store knob on the public config, so the honest answer is to check it at wiring time and name what
    /// would make it acceptable rather than silently ignore it.
    pub fn refuses_unbound_session_restore(
        config: &StreamableHttpServerConfig,
    ) -> Result<(), ServiceError> {
        if config.session_store.is_some() {
            return Err(ServiceError::InvalidInput(
                "cross-instance session restore needs a persisted account and credential binding; \
                 configure no session store until that binding exists"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// The enrollment order Task 9's network-facing routes must follow, in one place so a route and a test
    /// cannot disagree about it.
    ///
    /// Cheapest first: the attempt budget, then the login shape, then the Argon2 slot, and only then the
    /// path that reads a link and hashes a password. Reordering the first two is F4-15 — a held, unspent
    /// link plus a login that could never be stored would otherwise buy one ~19 MiB digest per attempt for
    /// the rest of the link's lifetime, because a refused login does not spend the link.
    ///
    /// `source` must be a value this process derived (the peer address), never one a client asserted.
    pub async fn enroll(
        &self,
        source: &str,
        secret: &str,
        login: &str,
        password: &str,
    ) -> Result<alexandria_storage::models::auth::PrincipalIds, ServiceError> {
        self.inner
            .admission
            .limit_auth_attempt(source, login, Utc::now())?;
        let login = enrollable_login(login)?;
        let _hash = self.inner.admission.hash_permit().await?;
        self.inner
            .service
            .accept_invite(secret, &login, password, Utc::now())
            .await
    }

    /// A login gated exactly as [`AuthMiddleware::enroll`] gates it, for the login path — which pays the
    /// same Argon2 verify and is bounded the same way.
    pub async fn login(
        &self,
        source: &str,
        login: &str,
        password: &str,
    ) -> Result<crate::auth::credentials::BrowserGrant, ServiceError> {
        self.inner
            .admission
            .limit_auth_attempt(source, login, Utc::now())?;
        let login = enrollable_login(login)?;
        let _hash = self.inner.admission.hash_permit().await?;
        self.inner.service.login(&login, password, Utc::now()).await
    }

    /// A password recovery, gated as the other two password paths are.
    ///
    /// **What a caller owes afterwards, and why this cannot pay it itself:** a recovery raises
    /// `account.auth_epoch`, which retires every credential that account holds *in the store* — the next
    /// authorisation and the next admission both refuse them. It does not cancel a lease that this process
    /// already issued, because `AuthRepo::recover` returns no account to name, and inventing one (a
    /// lookup by the consumed link's row, or a wider storage return type) is not this gate's to do. So the
    /// caller that knows the account — the recovery flow that resolved the link's owner before handing it
    /// over, or the admin surface that rotated the password — must call
    /// [`AuthService::invalidate_account`](crate::auth::service::AuthService::invalidate_account) after
    /// this returns `Ok`. Until something does, an open stream from before the recovery keeps serving
    /// until its next request, which is then refused. Task 6/9 own closing that loop; report §11.4 records
    /// it as a limit rather than a surprise.
    pub async fn recover(
        &self,
        source: &str,
        secret: &str,
        new_password: &str,
    ) -> Result<(), ServiceError> {
        self.inner
            .admission
            .limit_auth_attempt(source, secret, Utc::now())?;
        let _hash = self.inner.admission.hash_permit().await?;
        self.inner
            .service
            .recover(secret, new_password, Utc::now())
            .await
    }
}

/// A refusal, and the only body this transport renders for a request it did not admit.
///
/// It carries a status and a [`ServiceError`], which is the whole vocabulary a caller is allowed to learn:
/// a class, and a fixed sentence. The error's `Display` is the sanitized one and its `Debug` is
/// hand-written to have no cause chain, so there is no path from here to SQL, to a table name, to an
/// account or credential id, or to any secret material.
pub struct Denial {
    status: StatusCode,
    error: ServiceError,
}

impl Denial {
    /// The refusal a caller hears for an error from the admission path.
    pub fn new(error: ServiceError) -> Self {
        let status = match &error {
            // Absent and expired are the same answer, and it is 401: present a credential.
            ServiceError::Stale => StatusCode::UNAUTHORIZED,
            // A session id that is unknown or somebody else's. One answer, so it is not probeable.
            ServiceError::NotFound => StatusCode::NOT_FOUND,
            ServiceError::InvalidInput(_) => StatusCode::BAD_REQUEST,
            // A budget was reached: the request was not malformed and its authority is intact.
            ServiceError::Quota => StatusCode::TOO_MANY_REQUESTS,
            ServiceError::RetryableBookkeepingConflict => StatusCode::CONFLICT,
            ServiceError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ServiceError::StoreIntegrity | ServiceError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        Self { status, error }
    }

    /// A body larger than the gate will read: 413, because that is what happened.
    ///
    /// A distinct status from the `400` a malformed body gets and from the `401`/`404` an authority failure
    /// gets, so a client and a test can tell the three apart without reading prose.
    pub fn too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            error: ServiceError::InvalidInput("request body exceeds the limit".to_string()),
        }
    }

    /// No `Authorization: Bearer` on a hosted request.
    pub fn credential_required() -> Self {
        Self::new(ServiceError::Stale)
    }

    /// A refusal of the *presentation*, whatever storage called it.
    ///
    /// An unknown credential is `NotFound` in storage's vocabulary — no row resolves to that digest of that
    /// kind — but on a request that never authenticated, a `404` would describe the endpoint rather than the
    /// caller's problem. Every admission failure is therefore 401, while an unknown or foreign *session id*
    /// keeps its 404: that one really is a handle this caller does not have.
    pub fn authentication(error: ServiceError) -> Self {
        let classified = Self::new(error);
        match classified.status {
            StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED => Self {
                status: StatusCode::UNAUTHORIZED,
                ..classified
            },
            _ => classified,
        }
    }

    /// The credential was admitted and then revoked before this request could be answered.
    pub fn revoked() -> Self {
        Self::new(ServiceError::Stale)
    }

    /// The status this refusal renders with.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The message a caller sees. Never anything but the error's own `Display`.
    pub fn message(&self) -> String {
        self.error.to_string()
    }

    /// The refusal's class, for a log line or an assertion.
    pub fn error(&self) -> &ServiceError {
        &self.error
    }
}

impl From<ServiceError> for Denial {
    fn from(error: ServiceError) -> Self {
        Self::new(error)
    }
}

impl IntoResponse for Denial {
    fn into_response(self) -> Response {
        let body = serde_json::to_vec(&serde_json::json!({ "error": self.error.to_string() }))
            .unwrap_or_else(|_| br#"{"error":"internal error"}"#.to_vec());
        (
            self.status,
            [(CONTENT_TYPE, "application/json")],
            Body::from(body),
        )
            .into_response()
    }
}

/// The admission gate, as an axum middleware handler.
///
/// Mount it with `axum::middleware::from_fn_with_state(gate.clone(), admit)` on the router that nests the
/// MCP service. It is the whole of `/mcp`'s authentication: a request that does not come through it is a
/// request nobody admitted, which is why the debug router's own guard is a separate layer and a new private
/// route must not be bolted on below this one.
pub async fn admit(
    State(gate): State<AuthMiddleware>,
    request: Request,
    next: Next,
) -> Result<Response, Denial> {
    let method = request.method().clone();
    let session = session_id(request.headers());
    // The credential first, before anything is read: verifying is a couple of indexed reads, while
    // collecting a body is a copy of up to the whole limit. An unauthenticated caller must not be able to
    // make this process move bytes it will never look at.
    let presented = bearer(request.headers()).ok_or_else(Denial::credential_required)?;
    let lease = gate
        .admit_presentation(presented)
        .await
        .map_err(Denial::authentication)?;
    if let Some(session) = session.as_deref() {
        gate.check_session(session, &lease)?;
    }
    // The body is read here rather than left to the SDK so the queue can classify it and so the 1 MiB limit
    // is this gate's own decision rather than a default it happens to inherit. rmcp still parses, validates
    // and length-limits the bytes it receives afterwards.
    let (mut request, class) = if method == Method::POST {
        let limit = gate.max_body_bytes();
        if declared_too_large(request.headers(), limit) {
            return Err(Denial::too_large());
        }
        let (parts, body) = request.into_parts();
        let bytes = read_bounded(body, limit)
            .await
            .map_err(|_| Denial::too_large())?;
        let class = classify(&bytes);
        (Request::from_parts(parts, Body::from(bytes)), class)
    } else {
        (request, RequestClass::Protocol)
    };
    // Queued before dispatch, so work the bound refuses never starts — and work whose credential is revoked
    // while it waits is never dispatched either, because `Admission` re-asks the bus at grant.
    let permit = gate.admission().acquire(lease.principal(), class).await?;
    // The sealed actor, into the parts rmcp copies into every handler context, and the slot beside it: a
    // handler that outlives this function's own handle still holds the account's expensive capacity until
    // its context is dropped, which is the only way "two active expensive requests per account" describes
    // work in flight rather than response headers.
    request.extensions_mut().insert(lease.principal().clone());
    request.extensions_mut().insert(permit.clone());
    // And the lease itself, because a handler needs the cancellation handle too. rmcp's own per-request
    // `ctx.ct` is *not* it: the SDK cancels that when the request's response is sent or when the client
    // cancels, not when a session is closed under a revocation, so a writer that aborted on `ctx.ct`
    // would happily commit after its credential was revoked. See
    // [`lease_from_request`](crate::auth::mcp_transport::lease_from_request).
    request.extensions_mut().insert(lease.clone());
    // A request whose lifetime has ended is not answered. Dropping the forwarded future is also what tells
    // the SDK the client went away, which is the stateless path's own handler-cancellation signal.
    let response = tokio::select! {
        biased;
        _ = lease.cancellation().cancelled() => return Err(Denial::revoked()),
        response = next.run(request) => response,
    };
    let response = match session {
        None => bind_returned_session(&gate, response, &lease).await?,
        Some(session) => end_session(&gate, response, &session, &method),
    };
    drop(permit);
    Ok(response)
}

/// The bearer token a hosted request presented, or `None`.
///
/// Only `Authorization` is read; a `Cookie` header is not consulted at all, which is the point of keeping
/// the two kinds of credential apart.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next()?;
    // Two `Authorization` headers is not two chances: it is a request that proxies disagree about, so it is
    // refused rather than resolved by whichever one happens to be read first.
    if values.next().is_some() {
        return None;
    }
    let value = value.to_str().ok()?;
    let (scheme, rest) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case(BEARER) {
        return None;
    }
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

/// The session id a request named, if any.
pub fn session_id(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all(SESSION_ID_HEADER).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let session = first.to_str().ok()?.trim();
    (!session.is_empty()).then(|| session.to_string())
}

/// The JSON-RPC method and tool name a body carries, as far as a budget decision needs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The JSON-RPC `method`, if the body carried one.
    pub method: Option<String>,
    /// `params.name`, for a tool call.
    pub tool: Option<String>,
}

/// Reads the method and tool name out of a JSON-RPC body.
///
/// Lenient on purpose: this is a routing and budget question, not protocol validation, and rmcp still
/// validates the same bytes afterwards. A body that does not parse has no method, classifies as protocol
/// work, and is answered by the SDK — a malformed body is never an authority answer.
pub fn invocation(body: &[u8]) -> Invocation {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Invocation {
            method: None,
            tool: None,
        };
    };
    Invocation {
        method: parsed
            .get("method")
            .and_then(|method| method.as_str())
            .map(str::to_string),
        tool: parsed
            .get("params")
            .and_then(|params| params.get("name"))
            .and_then(|name| name.as_str())
            .map(str::to_string),
    }
}

/// Which budget class a request body belongs to.
pub fn classify(body: &[u8]) -> RequestClass {
    match invocation(body).method.as_deref() {
        // Every tool call is expensive work. A tool that only reads a session is still a database round
        // trip behind the same worker, and treating "not known to be embedding-backed" as cheap would be a
        // budget that silently widens every time a tool is added.
        Some("tools/call") => RequestClass::Expensive,
        _ => RequestClass::Protocol,
    }
}

/// Binds the session id rmcp put on a response, before that response is released.
///
/// The order is the security property: an `initialize` that has already created a session must not be
/// handed to the client as a live session if this gate cannot bind it — which is exactly the case where the
/// credential was revoked between verification and registration. The refusal also closes the session the
/// SDK created, because an unbound session is one no later revocation would reach, and awaiting the close
/// here means a caller that was refused cannot leave a live session behind.
async fn bind_returned_session(
    gate: &AuthMiddleware,
    response: Response,
    lease: &CredentialLease,
) -> Result<Response, Denial> {
    let Some(session) = response
        .headers()
        .get(SESSION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    else {
        return Ok(response);
    };
    match gate.register_session(&session, lease) {
        Ok(()) => Ok(response),
        Err(error) => {
            gate.events().close_sessions(vec![session.into()]).await;
            Err(Denial::new(error))
        }
    }
}

/// Drops the binding when a client ends its own session with a DELETE the SDK accepted.
fn end_session(
    gate: &AuthMiddleware,
    response: Response,
    session: &str,
    method: &Method,
) -> Response {
    if method == Method::DELETE && response.status().is_success() {
        gate.sessions().unbind(session);
    }
    response
}

/// Reads a body no larger than `limit`.
async fn read_bounded(body: Body, limit: usize) -> Result<Vec<u8>, ServiceError> {
    to_bytes(body, limit)
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|_| ServiceError::InvalidInput("request body exceeds the limit".to_string()))
}

/// Whether a declared length is one this gate will not read.
///
/// A `Content-Length` the process will not honour is refused before the bytes arrive, so a caller cannot
/// make this read a body it was never going to accept. A missing length is not a refusal: chunked bodies
/// are bounded by [`read_bounded`] as they stream in.
fn declared_too_large(headers: &HeaderMap, limit: usize) -> bool {
    let Some(declared) = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    match declared.parse::<u64>() {
        Ok(declared) => declared > limit as u64,
        // A length this process cannot read is not a length this process will buffer.
        Err(_) => true,
    }
}
