//! The browser half of the authentication boundary.
//!
//! This module is where a *web* request becomes an authority, and it is the only place that
//! transformation happens outside `AuthService`. Three types carry it, and each exists because the
//! alternative was a hole:
//!
//! * [`WebPolicy`] — where this deployment is published, which `Host` values are its own, and whether
//!   cookies may be sent without `Secure`. A **policy**, never a diagnostic: the pre-Task 9 guard
//!   derived its Host half from `Option<DebugContext>`, so any caller that omitted the dashboard's
//!   config silently got the weaker build. Authentication and diagnostics are injected separately from
//!   here on, and "the operator turned off the dashboard panel" cannot be a way to turn off a check.
//! * [`WebBoundary`] — the auth service, the store and the policy, as one handle the router carries.
//!   Handlers obtain it through the [`Boundary`] extractor and never assemble it.
//! * [`BrowserSession`] — the authenticated owner, inserted as a request extension by `guard` only
//!   after the cookie authenticated **and** `AccessRepo::authorize` re-read the live rows. A handler
//!   that finds no extension is rejected by the extractor, so a route registered outside the guard
//!   layer answers 403 instead of defaulting to somebody.
//!
//! ## Two cookies, because two secrets
//!
//! `alexandria_session` is the credential and is `HttpOnly`. `alexandria_csrf` is the value a form
//! embeds and is `HttpOnly` too: the server echoes the token into the page from the request's own
//! cookie (`BrowserSession::csrf`, rendered by `html::page` into the hidden input), so nothing has to
//! read `document.cookie` to fill a form in — which means the attribute costs the page nothing. It is
//! still not a credential: storage keeps its digest in `credential.csrf_digest`, a column the
//! session-resolution query never reads, so neither half can stand in for the other
//! (`AuthRepo::browser_session_matches_csrf` is what pairs them). One cookie doing both jobs is exactly
//! the property CSRF protection exists to deny.
//!
//! ## What a pre-auth token is and is not
//!
//! Login, invitation-claim and recovery-claim happen with no session, so there is no digest to check a
//! token against. [`PreAuthTokens`] is a bounded, expiring, single-use, page-scoped issuer held by this
//! process: it proves the form was served by *this* server, which is what makes login CSRF — an
//! attacker force-authenticating a victim into an account the attacker chose — impossible here. It
//! grants nothing else, is not a session, and is deliberately not durable: a restart drops pending
//! forms, which costs a page reload where the alternative is a token that outlives the process that
//! meant it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::http::header::{HeaderName, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use alexandria_storage::Database;
use alexandria_storage::access::{
    AccessRepo, AdminCapability, OwnerContext, Principal, ScopedContext,
};
use alexandria_storage::models::Credential;
use alexandria_storage::models::ReadSelection;
use alexandria_storage::models::identity::{AccountId, CredentialId};
use alexandria_storage::repos::MaintenanceRunSummary;
use chrono::Utc;

use crate::auth::AuthService;
use crate::auth::credentials::BrowserGrant;
use crate::auth::http::AuthMiddleware;
use crate::error::ServiceError;

/// The session cookie: `HttpOnly`, `Path=/`, and the only header this boundary reads to decide *who* a
/// browser request is from.
pub const SESSION_COOKIE: &str = "alexandria_session";
/// The CSRF half. `HttpOnly` for the reason in this module's header, and never a credential: only
/// `alexandria_session` is ever resolved to a principal.
pub const CSRF_COOKIE: &str = "alexandria_csrf";
/// The header a client may present the CSRF value in instead of a form field, as a typed name.
pub const CSRF_HEADER: axum::http::HeaderName = axum::http::HeaderName::from_static("x-csrf-token");
/// The form field every mutating page carries.
pub const CSRF_FIELD: &str = "csrf";

/// How long a pre-auth token is honourable: the browser-session idle window, so a form left open past
/// that would have lost its session too and deserves the same refresh.
const PRE_AUTH_TTL: Duration = Duration::from_secs(30 * 60);
/// How many outstanding pre-auth tokens one process will hold. A bound, not a tuning knob: the issuer
/// is in-process state an anonymous visitor grows by fetching pages, so an unbounded map is a memory
/// leak with a login form attached. At the cap the oldest entry goes, which costs whoever held it a
/// reload.
const PRE_AUTH_CAPACITY: usize = 4_096;

// -- policy -------------------------------------------------------------------------------------------

/// Where this deployment is published, and what it will accept as itself.
#[derive(Debug, Clone)]
pub struct WebPolicy {
    /// The canonical origin, validated and normalised at construction: `scheme://host[:port]`, no path,
    /// no trailing slash, lowercase host.
    public_origin: String,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    /// The explicit loopback-development exception, and the only state in which an `http` origin is
    /// accepted and cookies go out without `Secure`.
    loopback_development: bool,
}

/// One problem with a hosted configuration, worded with the key that caused it.
pub type PolicyProblem = String;

/// A parsed `scheme://host[:port]`. Scheme and host are lowercased; a port is kept, because
/// `localhost:3000` and `localhost:4000` are different origins to a browser.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: Option<String>,
}

impl Origin {
    /// The canonical form, with the `//` an origin always carries.
    ///
    /// Written once and compared as text everywhere, so the parse above and the `allowed_origins`
    /// comparison cannot disagree about what `https://host` means. (The first version of this built
    /// `https:host` — which `origin_allowed` then compared consistently against itself and so only the
    /// fixture's literal config could catch. It is here because a test caught it, not in spite of one.)
    fn as_str(&self) -> String {
        match &self.port {
            Some(port) => format!("{}://{}:{}", self.scheme, self.host, port),
            None => format!("{}://{}", self.scheme, self.host),
        }
    }
}

/// `scheme://host[:port]`, and nothing else.
///
/// Strict in one specific way that matters: a path, query, fragment or credential makes the string *not*
/// an origin, so it is refused rather than truncated — reading `https://host/login` as `https://host`
/// would silently accept an `Origin` the operator never configured.
fn parse_origin(raw: &str) -> Option<Origin> {
    let (scheme, rest) = raw.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    // A path, query or fragment means the string is not an origin. It is refused rather than truncated:
    // reading `https://host/login` as `https://host` would accept an `Origin` the operator never set.
    if rest[rest.find(['/', '?', '#']).unwrap_or(rest.len())..]
        .chars()
        .any(|ch| !ch.is_whitespace())
    {
        return None;
    }
    // Anything after the authority is a path, query or fragment, and the string is then **not** an
    // origin. Refused rather than truncated: reading `https://host/login` as `https://host` would
    // accept an `Origin` header the operator never configured, which is the whole origin check.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if !rest[authority_end..].chars().all(char::is_whitespace) {
        return None;
    }
    let authority = rest[..authority_end].trim();
    if authority.is_empty() || authority.contains('@') || rest.contains("..") {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = match rest.split_once(']') {
            Some((host, after)) => {
                let port = after.strip_prefix(':').filter(|port| numeric(port))?;
                (format!("[{host}]"), Some(port.to_string()))
            }
            None => {
                if !rest.ends_with(']') {
                    return None;
                }
                (format!("[{}]", rest.trim_end_matches(']')), None)
            }
        };
        if host.trim_matches(|c| c == '[' || c == ']').is_empty() {
            return None;
        }
        return Some(Origin {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
        });
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && numeric(port) => {
            (host.to_ascii_lowercase(), Some(port.to_string()))
        }
        _ => (authority.to_ascii_lowercase(), None),
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return None;
    }
    Some(Origin { scheme, host, port })
}

fn numeric(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Whether a host literal is loopback: `localhost`, `127.0.0.1`, `::1`.
fn loopback_host(host: &str) -> bool {
    matches!(
        host.trim_matches(|c| c == '[' || c == ']'),
        "localhost" | "127.0.0.1" | "::1"
    )
}

impl WebPolicy {
    /// Validates a hosted configuration, and refuses everything that cannot mean what it says.
    ///
    /// The rules, each a distinct failure mode:
    ///
    /// * `public_origin` must be an absolute `https` origin. `http` is accepted **only** with
    ///   `loopback_development`, and then only for a loopback host — a public `http` origin is a
    ///   credential sent in the clear, which no `Secure` flag can fix after the fact.
    /// * `allowed_hosts` must be finite and contain the origin's host. A wildcard or an empty list is
    ///   the DNS-rebinding hole the Host check exists to close, so it is not a configuration here at
    ///   all: a mounted boundary always checks `Host`, and `missing Host under an armed check` therefore
    ///   has no off switch to be tested against.
    /// * `allowed_origins` must be finite and contain `public_origin` exactly, because that is what the
    ///   mutation check compares against; a list omitting it would refuse every real form.
    ///
    /// Nothing here reads a request. Forwarded headers are the client's and the origin is the
    /// operator's, and the two never meet in this function — which is what makes "forwarded headers
    /// never establish the origin" true by construction rather than by a check that could be forgotten.
    pub fn hosted(
        public_origin: &str,
        allowed_hosts: &[String],
        allowed_origins: &[String],
        loopback_development: bool,
    ) -> Result<Self, Vec<PolicyProblem>> {
        const ORIGIN_RULE: &str = "server.public_origin = must be an explicit https:// origin \
             (scheme + host + optional port, no path); an empty or non-https value is accepted only \
             with server.loopback_development = true";
        const EXCEPTION_RULE: &str = "server.loopback_development = true is the explicit \
             loopback-development exception, requires a loopback host (localhost, 127.0.0.1 or ::1), \
             and is the only configuration that serves cookies without the Secure flag";
        let mut problems: Vec<PolicyProblem> = Vec::new();
        let parsed = parse_origin(public_origin);
        let host = parsed
            .as_ref()
            .map(|origin| origin.host.clone())
            .unwrap_or_default();

        match parsed.as_ref() {
            None => problems.push(if public_origin.trim().is_empty() {
                ORIGIN_RULE.to_string()
            } else {
                format!(
                    "server.public_origin = `{public_origin}` is not an origin; expected https://host \
                     or https://host:port with no path, query, fragment or credentials"
                )
            }),
            Some(origin) => {
                // Plain HTTP is admissible in exactly one state — the exception, on a loopback host —
                // and that state pushes no problem at all. Anything else names both the origin rule and
                // the exception, because an operator reading only the message has to learn the second
                // key exists.
                if origin.scheme != "https" && !(loopback_development && loopback_host(&origin.host)) {
                    problems.push(ORIGIN_RULE.to_string());
                    problems.push(EXCEPTION_RULE.to_string());
                }
            }
        }
        if loopback_development
            && matches!(parsed.as_ref(), Some(origin) if !loopback_host(&origin.host))
        {
            problems.push(EXCEPTION_RULE.to_string());
        }

        let unset =
            |list: &[String]| list.is_empty() || list.iter().any(|entry| entry.trim() == "*");
        if unset(allowed_hosts) {
            problems.push(
                "server.allowed_hosts = must list this host explicitly; no \"*\" and no empty list"
                    .to_string(),
            );
        } else if !host.is_empty()
            && !allowed_hosts
                .iter()
                .any(|entry| entry.trim().eq_ignore_ascii_case(&host))
        {
            problems.push(format!(
                "server.allowed_hosts = must contain server.public_origin's host `{host}`; it \
                 currently lists {allowed_hosts:?}"
            ));
        }
        if unset(allowed_origins) {
            problems.push(
                "server.allowed_origins = must list server.public_origin exactly; no \"*\" and no \
                 empty list"
                    .to_string(),
            );
        } else if let Some(origin) = parsed.as_ref() {
            let want = origin.as_str();
            if !allowed_origins.iter().any(|entry| entry.trim() == want) {
                problems.push(format!(
                    "server.allowed_origins = must contain server.public_origin (`{want}`) exactly; \
                     it currently lists {allowed_origins:?}"
                ));
            }
        }

        if !problems.is_empty() {
            return Err(problems);
        }
        let origin = parsed.expect("checked above");
        Ok(Self {
            public_origin: origin.as_str(),
            allowed_hosts: trim_all(allowed_hosts),
            allowed_origins: trim_all(allowed_origins),
            loopback_development,
        })
    }

    /// The configured canonical origin, in the spelling a browser puts in an `Origin` header.
    pub fn public_origin(&self) -> &str {
        &self.public_origin
    }

    /// Whether cookies carry `Secure`. The loopback exception is the only false answer, and it is set by
    /// a line in a config file — never by a request header.
    pub fn secure_cookies(&self) -> bool {
        !self.loopback_development
    }

    /// Whether the explicit loopback-development exception is in force.
    pub fn loopback_development(&self) -> bool {
        self.loopback_development
    }

    /// The hosts this deployment calls its own.
    pub fn allowed_hosts(&self) -> &[String] {
        &self.allowed_hosts
    }

    /// Whether a `Host` header value is one this deployment calls its own.
    ///
    /// Matched with or without the port, case-insensitively, because operators write
    /// `allowed_hosts = ["localhost"]` while browsers send `Host: localhost:3000` — the rule the
    /// pre-Task 9 guard followed, kept so an existing deployment is not locked out by the upgrade. An
    /// absent `Host` is a refusal: HTTP/1.1 requires it, so there is nothing to vouch for.
    pub fn host_allowed(&self, host: Option<&str>) -> bool {
        let Some(host) = host.map(str::trim).filter(|value| !value.is_empty()) else {
            return false;
        };
        let bare = strip_port(host);
        self.allowed_hosts
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(host) || entry.eq_ignore_ascii_case(bare))
    }

    /// Whether an `Origin` header value is this deployment's own.
    ///
    /// Compared as a parsed origin rather than as text, so `HTTP://Host` and `http://host` are one
    /// origin while a different port, or a path, is not.
    pub fn origin_allowed(&self, origin: Option<&str>) -> bool {
        let Some(presented) = origin.map(str::trim).filter(|value| !value.is_empty()) else {
            return false;
        };
        let Some(parsed) = parse_origin(presented) else {
            return false;
        };
        self.allowed_origins.iter().any(|entry| {
            parse_origin(entry)
                .as_ref()
                .is_some_and(|entry| entry.as_str() == parsed.as_str())
        })
    }

    /// The attribute tail every cookie this boundary issues carries. The idle window is the session's
    /// own, so a cookie cannot outlive the row behind it and mislead a browser into re-sending a dead
    /// secret.
    fn attributes(&self) -> String {
        format!(
            "Path=/; HttpOnly; SameSite=Lax{}; Max-Age={}",
            if self.secure_cookies() {
                "; Secure"
            } else {
                ""
            },
            crate::auth::credentials::BROWSER_IDLE.num_seconds(),
        )
    }
}

fn trim_all(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim().to_string())
        .collect()
}

/// Strips an authority's port without mangling a bracketed IPv6 literal.
pub(crate) fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.find(']') {
            Some(end) => &host[..end + 2],
            None => host,
        };
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !name.is_empty() && numeric(port) => name,
        _ => host,
    }
}

// -- page labels --------------------------------------------------------------------------------------

/// Which public page a pre-auth token was issued for.
///
/// Recorded with the token so the login form cannot spend a recovery page's token: the pages mint the
/// same shape of value, and what differs is the intent it was minted under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPage {
    Login,
    Invite,
    Recover,
}

impl AuthPage {
    /// The path the page is served on, which is also what the guard maps a request onto.
    pub fn path(self) -> &'static str {
        match self {
            Self::Login => "/debug/login",
            Self::Invite => "/debug/invite",
            Self::Recover => "/debug/recover",
        }
    }

    /// The label a form posts back. Only `path` decides which page a request is on — the label is what
    /// the page renders and the guard re-derives, so a posted `page` field is never trusted.
    pub fn label(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Invite => "invite",
            Self::Recover => "recover",
        }
    }

    fn from_path(path: &str) -> Option<Self> {
        match path {
            "/debug/login" => Some(Self::Login),
            "/debug/invite" => Some(Self::Invite),
            "/debug/recover" => Some(Self::Recover),
            _ => None,
        }
    }
}

// -- route classification -----------------------------------------------------------------------------

/// Which policy applies to a path, decided from the path alone.
///
/// Three arms, and the middle one is the reason this is an enum rather than a boolean: the auth pages
/// must be reachable with no session *and* must still be Host/origin/Fetch-Metadata/token-checked, so
/// "public" is not one thing. Assets are covered by the closed allowlist in `assets.rs`, which is why
/// classifying their prefix is safe — an unknown name 404s there rather than reaching anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// The vendored asset allowlist. Public, read-only, content-free by construction.
    Asset,
    /// `/debug/login`, `/debug/invite`, `/debug/recover`.
    PublicAuth,
    /// Everything else: private, owner-scoped, unreachable without a session.
    Private,
}

pub(crate) fn classify(path: &str) -> Route {
    if path == "/debug/assets" || path.starts_with("/debug/assets/") {
        return Route::Asset;
    }
    if AuthPage::from_path(path).is_some() {
        return Route::PublicAuth;
    }
    Route::Private
}

// -- session and boundary -----------------------------------------------------------------------------

/// An authenticated browser session: who the cookie authenticated as, what the database says they may
/// act on, and the CSRF value their own pages may embed.
#[derive(Clone)]
pub struct BrowserSession {
    principal: Principal,
    owner: OwnerContext,
    /// The CSRF half this request presented. A copy of what the caller already sent, never a secret this
    /// process minted — which is what makes echoing it into a form safe: a cross-site document cannot
    /// read it, and so cannot put it in a body.
    csrf: String,
    admin: Option<AdminCapability>,
}

/// Hand-written, and the reason is the field it must not print: a derived `Debug` here would put the
/// caller's CSRF value — and, if the session ever carried a bearer text again, the session cookie —
/// into any log line that renders one. The pair is verified against storage's digests, so no handler
/// needs either string, and none can read it out of this type.
impl std::fmt::Debug for BrowserSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserSession")
            .field("account", &self.owner.account())
            .field("csrf", &"<csrf token>")
            .field("admin", &self.admin.is_some())
            .finish()
    }
}

impl BrowserSession {
    /// The caller's own workspace authority. Every scoped read on every debug page is issued from this.
    pub fn owner(&self) -> &OwnerContext {
        &self.owner
    }

    pub fn account(&self) -> &AccountId {
        self.owner.account()
    }

    /// The credential row behind the session cookie, which is what a logout revokes.
    pub fn credential(&self) -> Option<&CredentialId> {
        self.principal.credential()
    }

    /// The account-management capability, when this account's live row says `admin`.
    pub fn admin(&self) -> Option<&AdminCapability> {
        self.admin.as_ref()
    }

    /// The CSRF half a page embeds in its own forms.
    pub fn csrf(&self) -> &str {
        &self.csrf
    }

    /// Resolves a read population for this caller. `AccessRepo::select` re-reads the account and the
    /// workspace link, so a page's population is the owner's and never a client's word.
    pub async fn read(
        &self,
        boundary: &WebBoundary,
        selection: ReadSelection,
    ) -> Result<ScopedContext, ServiceError> {
        Ok(AccessRepo::new(&boundary.db)
            .select(&self.owner, selection)
            .await?)
    }
}

/// The auth service, the store, and the policy — as one handle the router carries.
#[derive(Clone)]
pub struct WebBoundary {
    gate: AuthMiddleware,
    db: Arc<Database>,
    policy: Arc<WebPolicy>,
    preauth: Arc<PreAuthTokens>,
}

impl std::fmt::Debug for WebBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebBoundary")
            .field("public_origin", &self.policy.public_origin())
            .field("secure_cookies", &self.policy.secure_cookies())
            .field("outstanding_preauth_tokens", &self.preauth.len())
            .finish_non_exhaustive()
    }
}

impl WebBoundary {
    /// Wraps the **same** gate `/mcp` admits through, so login throttling, the Argon2 concurrency bound
    /// and the revocation bus are one set of objects rather than a browser-shaped copy of them.
    pub fn new(gate: AuthMiddleware, db: Arc<Database>, policy: WebPolicy) -> Self {
        Self {
            gate,
            db,
            policy: Arc::new(policy),
            preauth: Arc::default(),
        }
    }

    pub fn policy(&self) -> &WebPolicy {
        &self.policy
    }

    pub fn service(&self) -> &Arc<AuthService> {
        self.gate.service()
    }

    pub fn db(&self) -> &Arc<Database> {
        &self.db
    }

    /// The `/mcp` admission gate, for the login/invite/recover order its methods own.
    pub(crate) fn gate(&self) -> &AuthMiddleware {
        &self.gate
    }

    /// Who this request's cookie belongs to, with the live-rows re-read both halves need.
    ///
    /// Two calls in a deliberate order. `authenticate_browser` verifies the secret and rolls the idle
    /// deadline; `AccessRepo::authorize` then decides what that principal may act on. The second is not
    /// redundant with the first — it is where a suspension, a lost workspace and a superseded epoch are
    /// noticed, and it is why a session that was live when a page rendered cannot act after the account
    /// stopped being entitled, without anything having to notice in between.
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<BrowserSession, ServiceError> {
        let cookie = cookie_value(headers, SESSION_COOKIE).ok_or(ServiceError::Stale)?;
        let principal = self
            .service()
            .authenticate_browser(&cookie, Utc::now())
            .await?;
        let owner = AccessRepo::new(&self.db).authorize(&principal).await?;
        // The admin capability is optional and per-request: `authorize_admin` re-reads `account.role`,
        // so a demotion cannot be outrun by a page rendered while the account still had it. A refusal
        // is not an error at this point — it is "this caller is an ordinary owner".
        let admin = AccessRepo::new(&self.db)
            .authorize_admin(&principal)
            .await
            .ok();
        Ok(BrowserSession {
            principal,
            owner,
            csrf: cookie_value(headers, CSRF_COOKIE).unwrap_or_default(),
            admin,
        })
    }

    /// Whether `presented` is this session's own CSRF proof.
    ///
    /// Two halves, both required: the value must be the one this request's own cookie carried (so a
    /// forged body cannot supply a token in a shape nobody held), and it must match the digest storage
    /// keeps for that session (so a made-up value and a retired session's value both answer no).
    ///
    /// The lookup is by the session's **credential id**, which only `authenticate` can produce: it
    /// resolves the presented bearer text to a row, so a request cannot name one credential and be
    /// checked against another. Storage's `browser_session_matches_csrf` then re-applies the same liveness
    /// predicate `AccessRepo::authorize` used a moment earlier, so a session revoked or aged out between
    /// the two calls cannot carry a mutation either.
    pub async fn verify_csrf(&self, session: &BrowserSession, presented: &str) -> bool {
        if presented.is_empty() || presented != session.csrf() {
            return false;
        }
        let Some(credential) = session.credential() else {
            // A session with no credential row behind it is not a session; nothing can pair a token
            // with it.
            return false;
        };
        self.service()
            .verify_csrf(credential, presented, Utc::now())
            .await
            .unwrap_or(false)
    }

    /// The two `Set-Cookie` values that establish a fresh session.
    pub fn cookies_for(&self, grant: &BrowserGrant) -> Vec<HeaderValue> {
        let attributes = self.policy.attributes();
        [
            (SESSION_COOKIE, grant.expose_cookie()),
            (CSRF_COOKIE, grant.expose_csrf()),
        ]
        .into_iter()
        .filter_map(|(name, value)| {
            HeaderValue::from_str(&format!("{name}={value}; {attributes}")).ok()
        })
        .collect()
    }

    /// The two `Set-Cookie` values that expire both halves.
    pub fn cleared_cookies(&self) -> Vec<HeaderValue> {
        let tail = format!(
            "Path=/; HttpOnly; SameSite=Lax{}; Max-Age=0",
            if self.policy.secure_cookies() {
                "; Secure"
            } else {
                ""
            }
        );
        [SESSION_COOKIE, CSRF_COOKIE]
            .into_iter()
            .filter_map(|name| HeaderValue::from_str(&format!("{name}=; {tail}")).ok())
            .collect()
    }

    /// Ends a browser session: revokes the credential behind its cookie, then produces the headers that
    /// clear both halves.
    ///
    /// Revocation rather than forgetting, because a cookie the browser keeps is a credential that still
    /// works. `AuthService::revoke_credential` publishes on the Task 5 bus after its write commits, so a
    /// live MCP stream or lease under the same credential dies with it.
    pub async fn logout(&self, session: &BrowserSession) -> Result<Vec<HeaderValue>, ServiceError> {
        if let Some(credential) = session.credential() {
            self.service()
                .revoke_credential(session.owner(), credential, Utc::now())
                .await?;
        }
        Ok(self.cleared_cookies())
    }

    /// Issues a pre-auth token for one public page.
    pub fn issue_preauth(&self, page: AuthPage) -> String {
        self.preauth.issue(page)
    }

    /// Consumes a pre-auth token, one use and page-scoped.
    pub fn take_preauth(&self, page: AuthPage, presented: &str) -> bool {
        self.preauth.take(page, presented)
    }

    /// The account name behind a session, read from the row.
    ///
    /// Needed to render "signed in as" and to refuse to name the account an administrator is about to
    /// suspend. A missing row is not a page-wide failure, so this degrades to the caller's own id rather
    /// than to an error.
    /// The login behind an account id, for the "signed in as" line an owner and an administrator both
    /// need. A row that cannot be read yields the empty string rather than an error: the field is a
    /// label on a page, never an authority.
    pub async fn login_of(&self, account: &AccountId) -> Result<String, ServiceError> {
        Ok(alexandria_storage::repos::AuthRepo::new(self.db.inner())
            .account_of(account)
            .await?
            .map(|account| account.login)
            .unwrap_or_default())
    }

    /// The caller's own credential rows, for the account page.
    ///
    /// Labels and dates, never digests: `Credential`'s own `Debug` and serde impls already hide the two
    /// digest columns, and this read names the kinds an owner may act on so a leaked invitation or
    /// recovery link cannot be listed as if it were a session.
    pub async fn own_credentials(
        &self,
        owner: &OwnerContext,
    ) -> Result<Vec<Credential>, ServiceError> {
        // Labels, kinds and dates only; see `AuthRepo::list_own_credentials` for why the kinds are
        // named here rather than listed as whatever the account holds.
        Ok(alexandria_storage::repos::AuthRepo::new(self.db.inner())
            .list_own_credentials(owner)
            .await?)
    }

    /// Revokes one named credential of the caller's own account. A row that is not theirs, and a row
    /// that does not exist, are the same `NotFound`.
    pub async fn revoke_own_credential(
        &self,
        owner: &OwnerContext,
        credential: &CredentialId,
    ) -> Result<(), ServiceError> {
        self.service()
            .revoke_credential(owner, credential, Utc::now())
            .await
    }

    /// Invalidates every live lease and session of an account whose row this process just changed.
    ///
    /// The call Task 5's `recover` and suspension documentation left for the surfaces that *know the
    /// account*: a store-side epoch bump retires credentials on their next request, and only this
    /// reaches a stream that is already open.
    pub async fn invalidate_account(&self, account: &AccountId) -> usize {
        self.service().invalidate_account(account).await
    }

    /// The account a recovery link is addressed to, resolved without consuming it.
    ///
    /// `None` for a secret that is not a live recovery link, which is the same answer a spent or expired
    /// one gives: this path is reachable without a session and must not become a probe.
    pub async fn recovery_target(&self, secret: &str) -> Option<AccountId> {
        alexandria_storage::repos::AuthRepo::new(self.db.inner())
            .recovery_link_target(secret)
            .await
            .ok()
            .flatten()
    }

    /// The non-content scheduler health an administrator may read.
    pub async fn run_rollup(
        &self,
        admin: &AdminCapability,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MaintenanceRunSummary>, ServiceError> {
        // `_admin` is a type-level gate, not a query predicate: `maintenance_log` has no owner column to
        // scope by (decision B), so the statement is store-wide by necessity and the *only* thing that
        // keeps it an admin surface is that reaching it requires a capability nothing a client can forge.
        // `MaintenanceRepo`'s own documentation carries the same note from the storage side.
        Ok(
            alexandria_storage::repos::MaintenanceRepo::new(self.db.inner())
                .run_rollup(admin, limit, offset)
                .await?,
        )
    }

    /// The account-management repo, borrowed from this boundary's store.
    pub fn admin_repo(&self) -> alexandria_storage::repos::AdminRepo<'_> {
        alexandria_storage::repos::AdminRepo::new(self.db.inner())
    }
}

// -- extractors ----------------------------------------------------------------------------------------

/// The boundary, as a handler extractor.
///
/// Carried as a request extension rather than as router state on purpose: the state is
/// [`AlexandriaServer`], the MCP tool surface, which must not grow a debug-only field — and every
/// existing handler's `State(server)` extractor stays exactly as it was. A request that reaches a
/// handler with no boundary is a route registered outside the guard layer, and the answer is 403 rather
/// than a panic or a default owner.
#[derive(Clone)]
pub(crate) struct Boundary(pub(crate) Arc<WebBoundary>);

impl std::ops::Deref for Boundary {
    type Target = WebBoundary;
    fn deref(&self) -> &WebBoundary {
        &self.0
    }
}

/// The extension the guard inserts once it has authenticated a session.
#[derive(Clone)]
pub(crate) struct SharedBoundary(pub(crate) Arc<WebBoundary>);

impl<S: Send + Sync> FromRequestParts<S> for Boundary {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<SharedBoundary>()
            .map(|shared| Self(Arc::clone(&shared.0)))
            .ok_or_else(|| plain_forbidden(NO_BOUNDARY))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for BrowserSession {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Self>()
            .cloned()
            // Belt and braces: the guard denies a private route before its handler runs, so a missing
            // extension means a route was mounted outside the layer. Refusing here is the second lock.
            .ok_or_else(|| plain_forbidden(NO_SESSION))
    }
}

/// The peer address the guard recorded, as the attempt limiter's source key.
///
/// Built from the connection, never from `X-Forwarded-For`: a limiter keyed on a header the client
/// chooses is a limiter the client sets. Production supplies it through axum's `ConnectInfo`; a test
/// that drives the router with `oneshot` has no peer, and the limiter then sees
/// [`UNATTRIBUTED_SOURCE`] — one honest bucket rather than a fabricated per-connection one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerAddress(pub(crate) String);

/// The source label for a request this process cannot attribute to a peer.
pub(crate) const UNATTRIBUTED_SOURCE: &str = "unattributed";

impl PeerAddress {
    /// The address to key an attempt window on, as text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A request-parts extractor, so a handler can name the peer in its signature instead of holding the
/// whole `Request` — which matters because `Form` is a body extractor, and a `Request` argument before
/// it is not a handler axum will mount.
///
/// Missing is not a rejection: a request driven straight through `oneshot` in a test has no
/// connection and so no peer, and the extractor then yields [`UNATTRIBUTED_SOURCE`] rather than
/// fabricating an address. That is the honest spelling of "this process cannot tell", and it is the same
/// label the Task 5 fixtures use for the same case.
impl<S: Send + Sync> FromRequestParts<S> for PeerAddress {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<Self>()
            .cloned()
            .unwrap_or_else(|| PeerAddress(UNATTRIBUTED_SOURCE.to_string())))
    }
}

/// The refusal a no-boundary build answers with. One sentence, because one cause: this router has no
/// auth service, so nothing private exists here.
pub(crate) const NO_BOUNDARY: &str = "403 Forbidden: this build has no authentication context, so no \
    private route is available (see debug/guard.rs)";
/// The refusal for a private route whose session is missing, absent or no longer entitling.
pub(crate) const NO_SESSION: &str = "403 Forbidden: this page requires an authenticated owner session \
    (see debug/guard.rs)";
/// The refusal for a mutation with no CSRF proof at all.
pub(crate) const CSRF_MISSING: &str = "403 Forbidden: this mutation carries no CSRF token (see \
    debug/guard.rs)";
/// The refusal for a CSRF proof that does not match the session.
pub(crate) const CSRF_INVALID: &str = "403 Forbidden: the CSRF token does not match this session (see \
    debug/guard.rs)";
/// The refusal for an `Origin` that is not the published one.
pub(crate) const ORIGIN_UNSETTLED: &str = "403 Forbidden: Origin is not server.public_origin (CSRF \
    defence; see debug/guard.rs)";
/// The refusal for a `Host` outside the allowlist.
pub(crate) const HOST_UNSETTLED: &str = "403 Forbidden: Host header is not in [server] allowed_hosts \
    (DNS rebinding defence; see debug/guard.rs)";
/// The refusal for a request whose Fetch-Metadata marks it cross-site.
pub(crate) const CROSS_SITE: &str = "403 Forbidden: cross-site submissions to this UI are not allowed \
    (see debug/guard.rs)";

/// A short plain-text 403: whoever hits it — an operator behind a misconfigured proxy, or someone
/// reading a rebinding repro — sees which layer refused, and no page markup proves no handler ran.
pub(crate) fn plain_forbidden(reason: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        reason.to_string(),
    )
        .into_response()
}

// -- pre-auth tokens -----------------------------------------------------------------------------------

/// The bounded, in-process, page-scoped pre-auth token issuer.
#[derive(Default)]
struct PreAuthTokens {
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug)]
struct Entry {
    page: AuthPage,
    secret: String,
    issued: Instant,
}

impl PreAuthTokens {
    fn issue(&self, page: AuthPage) -> String {
        let id = random_hex();
        let secret = random_hex();
        let mut entries = self.lock();
        if entries.len() >= PRE_AUTH_CAPACITY {
            // Drop the oldest rather than refuse service: the cap exists to bound memory, not to lock
            // an operator out of their own login form.
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.issued)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        entries.insert(
            id.clone(),
            Entry {
                page,
                secret: secret.clone(),
                issued: Instant::now(),
            },
        );
        format!("{id}.{secret}")
    }

    fn take(&self, page: AuthPage, presented: &str) -> bool {
        let Some((id, secret)) = presented.split_once('.') else {
            return false;
        };
        let mut entries = self.lock();
        let Some(entry) = entries.remove(id) else {
            return false;
        };
        // Removed before the checks either way: an expired, wrong-page or wrong-secret token is spent,
        // so a refusal cannot be retried with the same value.
        entry.page == page
            && entry.issued.elapsed() <= PRE_AUTH_TTL
            && constant_time_eq(entry.secret.as_bytes(), secret.as_bytes())
    }

    fn len(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().expect("pre-auth token map poisoned")
    }
}

/// Length-independent, branch-light comparison.
///
/// A pre-auth token is not a password — it is a 122-bit value this process minted and can re-issue — so
/// this is hygiene rather than a control, and it is here rather than a dependency on a `subtle` crate
/// the tree does not carry.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in a.iter().zip(b) {
        difference |= left ^ right;
    }
    difference == 0
}

/// A hex token from `uuid`'s v4 generator, which is the process CSPRNG.
///
/// One UUID carries 122 bits of randomness (the version and variant nibbles are fixed) and renders as
/// 32 lowercase hex characters. `AuthRepo`'s bearer secrets use `OsRng` through `argon2`'s re-export;
/// this crate does not — and should not — depend on a password hasher to mint a form token, and the
/// values here are single-use, 30-minute, non-credential proofs rather than 256-bit secrets.
fn random_hex() -> String {
    format!("{:x}", uuid::Uuid::new_v4())
}

// -- header and body readers ----------------------------------------------------------------------------

/// The value of one cookie in a request's `Cookie` header.
///
/// A repeated name is a refusal rather than a choice: two `alexandria_session` cookies is a request
/// that proxies disagree about, which is exactly the shape that must not be resolved by whichever one a
/// parser read first.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    const COOKIE: HeaderName = HeaderName::from_static("cookie");
    let mut found: Option<String> = None;
    for header in headers.get_all(COOKIE).iter() {
        let Ok(raw) = header.to_str() else {
            continue;
        };
        for pair in raw.split(';') {
            let Some((key, value)) = pair.trim().split_once('=') else {
                continue;
            };
            let value = value.trim();
            if key.trim() != name || value.is_empty() {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(value.to_string());
        }
    }
    found
}

/// The value of one `application/x-www-form-urlencoded` field.
///
/// Parsed by hand rather than with `serde_urlencoded` because this runs *before* any handler's own
/// extractor and must not fail the request it is inspecting: a body that does not parse is a body
/// without a token, which is a refusal the guard renders in its own words rather than a 422 from a
/// deserializer.
pub(crate) fn form_field(body: &[u8], field: &str) -> Option<String> {
    form_fields(body, field).into_iter().next()
}

/// **Every** value a repeated form field carried, in the order the body presented them.
///
/// `form_field` returns the first, and for the CSRF token that is the whole question. It is not enough for
/// a browser's checkbox group: `<input type="checkbox" name="selected">` twice in a form is two `selected`
/// pairs in the body, and a reader that stops at the first silently drops the owner's other decisions. A
/// real HTML form has no other way to send a multi-value field, and axum's own `Form<T>` decoder is
/// narrower still — serde_urlencoded 0.7 feeds duplicate keys to serde as a map with a repeated field, and
/// serde_derive answers `duplicate field \`selected\``, so a two-box approval is a **422 before the
/// handler runs**. The single-selection tests that passed therefore never proved the multi-select control
/// worked; this is the path that does, on the same hand-rolled decoder the token check already relies on
/// and with no new dependency.
///
/// A non-UTF-8 body is not a form; `None`-as-empty is the honest answer, and a value with a malformed
/// percent escape is kept as data rather than dropped, matching `percent_decode`'s tolerance.
pub(crate) fn form_fields(body: &[u8], field: &str) -> Vec<String> {
    let Ok(text) = std::str::from_utf8(body) else {
        return Vec::new();
    };
    text.split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| *key == field)
        .filter_map(|(_, value)| percent_decode(value))
        .collect()
}

/// Percent-decoding, tolerant by design: a malformed escape is data, not an error.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match u8::from_str_radix(&raw[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    // A value that is not UTF-8 is not a token; it is refused rather than lossily converted, so two
    // different bytes cannot render the same string and then compare equal.
    String::from_utf8(out).ok()
}

/// The `csrf` field a form body carried, if it named one at all.
///
/// A handler's own `Form` struct declares the same field so the body deserializes; it is *this*
/// function's answer that decides whether the request mutates anything, which is why the field is not
/// read twice and why "the guard is the CSRF authority" is a property of the router rather than a
/// convention each page has to remember.
pub(crate) fn csrf_form_field(body: &[u8]) -> Option<String> {
    form_field(body, CSRF_FIELD)
}

/// Appends `Set-Cookie` values to a response.
pub(crate) fn push_cookies(response: &mut Response, cookies: Vec<HeaderValue>) {
    for cookie in cookies {
        response.headers_mut().append(SET_COOKIE, cookie);
    }
}

/// Marks a response uncacheable. Private pages and token-bearing pages both need it; a public page's
/// answer to a wrong credential is a page that must not be stored either.
pub(crate) fn no_store(response: &mut Response) {
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
}

impl Boundary {
    /// The caller's whole own corpus — Personal plus every project — as the pair of contexts an
    /// owner-facing page reads.
    ///
    /// `Assigned` is the selection this convenience resolves, and it deliberately excludes the Unassigned
    /// partition: reading those rows is a review act with a selection of its own
    /// (`ReadSelection::UnassignedReview`), so a page that reached for it quietly would show quarantined
    /// memories the owner never asked to look at. Task 11's review page asks for it **by name**, through
    /// [`Boundary::corpus_selection`] — which is why that method exists rather than a widened default here.
    ///
    /// A refusal renders as 403. `AccessRepo::select` re-reads the account and the workspace link on
    /// every call, so a caller who authenticated a moment ago and now resolves nothing has a
    /// suspended, demoted or reassigned account — precisely the state the guard's check can race with.
    pub(crate) async fn corpus(
        &self,
        session: &BrowserSession,
    ) -> Result<(OwnerContext, ScopedContext), Box<Response>> {
        self.corpus_selection(session, ReadSelection::Assigned)
            .await
    }

    /// The caller's own contexts under a **named** read selection.
    ///
    /// The selection is a handler's choice made from a closed [`ReadSelection`] value, never a string from
    /// the request: what a page may enumerate is decided in code, and the account/workspace population
    /// still comes from `AccessRepo::select` re-reading the live rows behind this session. So widening the
    /// population to Unassigned is possible only for a handler that says so at the call site, which is the
    /// property the review page needs without making it some other page's default.
    pub(crate) async fn corpus_selection(
        &self,
        session: &BrowserSession,
        selection: ReadSelection,
    ) -> Result<(OwnerContext, ScopedContext), Box<Response>> {
        let read = session
            .read(self, selection)
            .await
            .map_err(|_| Box::new(plain_forbidden(NO_SESSION)))?;
        Ok((session.owner().clone(), read))
    }
}
