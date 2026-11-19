//! The request boundary in front of every debug route: authentication, CSRF, Host and origin.
//!
//! # Threat model
//!
//! Before Task 9 this module guarded an **unauthenticated** surface and could only narrow it: a
//! cross-site form post could write heat, and a DNS-rebinding page could read session summaries
//! same-origin. Both attacks are now closed at their root — a private route requires an authenticated
//! owner context, and a mutation requires a CSRF value only a page this server rendered can supply —
//! but the checks that made the old surface survivable are kept, because they are defence in depth for
//! the new one and because the build without an auth boundary is still the build a stdio caller or an
//! old test mounts.
//!
//! Four layers, and the order is the security property:
//!
//! 1. **Route class.** [`web::classify`] decides asset / public-auth-page / private from the path
//!    alone. The asset allowlist is content-free by construction; the auth pages are reachable with no
//!    session; everything else needs one.
//! 2. **Host.** Checked on every route the moment a policy is armed — including the public ones,
//!    because a rebound name is exactly what a forged login POST arrives under, and one rule for all
//!    three classes is a rule that cannot be forgotten when a fourth public route appears. An absent
//!    `Host` under an armed policy is a refusal.
//! 3. **Session.** A private route authenticates the `alexandria_session` cookie through
//!    [`WebBoundary::authenticate`] and inserts the resulting [`BrowserSession`] plus the
//!    [`WebBoundary`] itself. No cookie, a dead cookie, or a refusal from `AccessRepo::authorize` is a
//!    403 **before the handler runs**, which is what makes "no browser route reaches an unscoped read"
//!    a property of the router rather than of each handler's diligence. `POST /debug/admin*` additionally
//!    requires the account-management capability, refused 403 here rather than rendered empty there.
//! 4. **Mutation.** Every non-`GET`/`HEAD` request owes: a matching `Origin` when one is present, the
//!    Fetch-Metadata rule (a hostile `Sec-Fetch-Site` is fatal *with* a valid token), and a CSRF proof —
//!    a session request must present the token that equals its own CSRF cookie **and** matches the
//!    digest storage holds for that session; a pre-auth page must present a token this process issued
//!    for that page. Absent `Sec-Fetch-Site` is not a bypass: it is the shape a curl or a scripted
//!    client takes, and those can carry a token.
//!
//! ## Why the CSRF half is unconditional and the Host half follows the policy
//!
//! Unchanged from before, and still worth stating: deriving the mutation rule from the presence of a
//! configuration value would hand every caller that forgot to pass one the weaker build. So the
//! mutation rule applies to every router this module is layered onto. The Host rule follows policy,
//! with one Task 9 addition — once a [`WebBoundary`] is mounted, its [`WebPolicy`] cannot be
//! constructed with a wildcard or an empty list at all, so the authenticated build has no
//! Host-check-off state to fall into. Only the fail-closed legacy build can skip the check, and it
//! serves no private data to skip it for.
//!
//! ## Positive controls, not absence
//!
//! A 403 proves nothing without a 200 on the same route from the same fixture. Every denial case in
//! this file's tests is paired with the authenticated read that succeeds, and the suite-level version
//! of that rule is `the_secured_router_serves_the_same_reads_to_a_signed_in_owner` in
//! `tests/web_auth_test.rs`.

use axum::body::Body;
use axum::extract::Request;
use axum::extract::State;
use axum::extract::connect_info::ConnectInfo;
use axum::http::header::HOST;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use std::sync::Arc;

use super::web::{
    self, CROSS_SITE as CROSS_SITE_REASON, CSRF_HEADER, CSRF_INVALID, CSRF_MISSING, HOST_UNSETTLED,
    NO_BOUNDARY, NO_SESSION, ORIGIN_UNSETTLED, PeerAddress, Route, SharedBoundary, WebBoundary,
    no_store, plain_forbidden,
};
use crate::debug::BrowserSession;

/// The guard's state.
///
/// `boundary: None` is the **fail-closed** build: the legacy `router(server)` /
/// `router_with_context(server, ctx)` assembly, which mounts no auth service and therefore serves no
/// private data. That is not "authentication off" — it is "nothing private is available", which is the
/// difference this task exists to make structural.
#[derive(Clone, Default)]
pub(super) struct Guard {
    pub(super) boundary: Option<Arc<WebBoundary>>,
    /// `[server] allowed_hosts` for the no-boundary build only, verbatim from [`DebugContext`](super::DebugContext).
    ///
    /// The pre-Task 9 arrangement, kept on purpose: `router_with_context(server, Some(ctx))` was the
    /// only way the Host check was armed, and every caller that passes `None` — stdio mode and the
    /// whole legacy test surface — still expects a wildcard list to mean "skip it". Once a boundary is
    /// mounted, its policy owns the question and this field is never read.
    pub(super) legacy_hosts: Vec<String>,
}

/// Methods that need no CSRF proof. `HEAD` for completeness; `OPTIONS` is *not* exempt — this router
/// advertises no CORS configuration, so a cross-site preflight here is the attack rather than a use case.
fn is_read_only(method: &axum::http::Method) -> bool {
    *method == axum::http::Method::GET || *method == axum::http::Method::HEAD
}

/// Bodies are read only to look for one field, and only this far. The debug UI's largest form is the
/// Query Tester's handful of fields, so this is the transport's own body ceiling reused rather than a
/// new number: the guard cannot be talked into buffering more than `/mcp` would have accepted.
const MUTATION_BODY_LIMIT: usize = crate::auth::admission::MAX_BODY_BYTES;

pub(super) async fn enforce(
    State(guard): State<Guard>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let route = web::classify(&path);

    // Record the peer before anything else reads it, so the attempt limiter has a key the client did
    // not choose. `ConnectInfo` is present only where `main.rs` mounted the service with
    // `into_make_service_with_connect_info`; a `oneshot` test has none, and gets the honest
    // unattributed bucket rather than a fabricated address.
    // Copy the address out before borrowing the request mutably: `extensions()` holds an immutable
    // borrow of the same request the insert needs to mutate.
    let peer = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|peer| peer.0.to_string());
    if let Some(peer) = peer {
        request.extensions_mut().insert(PeerAddress(peer));
    }

    // Layer 2: Host, whenever a policy is in force — public routes included.
    let host = header(request.headers(), HOST);
    let host_ok = match &guard.boundary {
        Some(boundary) => boundary.policy().host_allowed(host),
        None => {
            if legacy_check_active(&guard.legacy_hosts) {
                legacy_host_in_list(&guard.legacy_hosts, host)
            } else {
                true
            }
        }
    };
    if !host_ok {
        return plain_forbidden(HOST_UNSETTLED);
    }

    let Some(boundary) = guard.boundary.clone() else {
        // Layer 3, in the build with nothing to authenticate against: every private route and every
        // mutation is denied before its handler is reached. Assets stay available (the whole
        // `assets.rs` allowlist is content-free) and the auth pages have no service to serve them.
        if route == Route::Asset && is_read_only(&method) {
            return next.run(request).await;
        }
        return plain_forbidden(NO_BOUNDARY);
    };
    // Handlers reach the boundary through the extractor, so a route that escaped this layer still has
    // nothing to act with. Inserting it here rather than as router state keeps `AlexandriaServer` —
    // the MCP tool surface — free of any debug-only field.
    request
        .extensions_mut()
        .insert(SharedBoundary(Arc::clone(&boundary)));

    match route {
        Route::Asset => next.run(request).await,
        Route::PublicAuth => {
            if is_read_only(&method) {
                let mut response = next.run(request).await;
                // A login page carries a token and must not be stored by a shared cache.
                no_store(&mut response);
                response
            } else {
                public_mutation(request, &boundary, &path, next).await
            }
        }
        Route::Private => {
            let session = match boundary.authenticate(request.headers()).await {
                Ok(session) => session,
                Err(_) => return plain_forbidden(NO_SESSION),
            };
            if admin_route(&path) && session.admin().is_none() {
                return plain_forbidden(
                    "403 Forbidden: this page manages accounts, which needs the administrator role \
                     (see debug/guard.rs)",
                );
            }
            if is_read_only(&method) {
                request.extensions_mut().insert(session);
                let mut response = next.run(request).await;
                // One owner's corpus, behind a cookie: never cacheable, and an asset's `immutable`
                // policy is left alone because this branch cannot produce one.
                no_store(&mut response);
                response
            } else {
                private_mutation(request, &boundary, session, next).await
            }
        }
    }
}

/// A public auth page's `POST`: origin and Fetch-Metadata rules as for any mutation, plus the pre-auth
/// token this process issued for *this* page.
async fn public_mutation(
    request: Request,
    boundary: &WebBoundary,
    path: &str,
    next: Next,
) -> Response {
    if let Some(rejection) = pre_token_checks(request.headers(), boundary) {
        return rejection;
    }
    let Some(page) = auth_page(path) else {
        // Unreachable through `classify`, and refused rather than assumed: an unclassified public POST
        // is a route that was added to the classifier and not to this match.
        return plain_forbidden(NO_BOUNDARY);
    };
    let (request, body) = match read_body(request).await {
        Ok(pair) => pair,
        Err(response) => return *response,
    };
    let Some(presented) = mutation_token(&body, request.headers()) else {
        return plain_forbidden(CSRF_MISSING);
    };
    if !boundary.take_preauth(page, &presented) {
        return plain_forbidden(CSRF_INVALID);
    }
    // The token is spent the moment it is accepted, and the body the guard read is handed back whole:
    // this is a look, not a consumption.
    let _ = page;
    let mut response = next.run(request).await;
    no_store(&mut response);
    response
}

/// A private route's mutation: the CSRF pair, then the handler.
async fn private_mutation(
    request: Request,
    boundary: &Arc<WebBoundary>,
    session: BrowserSession,
    next: Next,
) -> Response {
    if let Some(rejection) = pre_token_checks(request.headers(), boundary) {
        return rejection;
    }
    let (mut request, body) = match read_body(request).await {
        Ok(pair) => pair,
        Err(response) => return *response,
    };
    // The form's own token field is authoritative over the header, and reading it here is what makes the
    // handlers' `csrf` field a real (if unread-by-them) part of the request rather than dead weight: the
    // guard is the single CSRF authority, and a handler that re-checked it would be a second rule to
    // drift. See `crate::auth::csrf_form_field`.
    let Some(presented) = mutation_token(&body, request.headers()) else {
        return plain_forbidden(CSRF_MISSING);
    };
    if !boundary.verify_csrf(&session, &presented).await {
        return plain_forbidden(CSRF_INVALID);
    }
    request.extensions_mut().insert(session);
    let mut response = next.run(request).await;
    no_store(&mut response);
    response
}

/// The two mutation checks that need no credential: Fetch Metadata, then `Origin`.
fn pre_token_checks(headers: &HeaderMap, boundary: &WebBoundary) -> Option<Response> {
    if fetch_metadata_is_hostile(headers) {
        return Some(plain_forbidden(CROSS_SITE_REASON));
    }
    // An `Origin` that is present must be the published one. Browsers send it on every cross-origin
    // request and on `POST`s from modern engines, so its *absence* is what a curl or a scripted client
    // looks like — which is why the token, not the header, is the proof.
    if let Some(origin) = header(headers, HeaderName::from_static("origin"))
        && !boundary.policy().origin_allowed(Some(origin))
    {
        return Some(plain_forbidden(ORIGIN_UNSETTLED));
    }
    None
}

/// The CSRF value a mutation presented: the `X-CSRF-Token` header if it carried one, else the `csrf`
/// field of a form-urlencoded body. Header first, because a JSON client has no form field to put it in.
fn mutation_token(body: &[u8], headers: &HeaderMap) -> Option<String> {
    if let Some(value) = header(headers, CSRF_HEADER.clone()) {
        return Some(value.to_string());
    }
    web::csrf_form_field(body)
}

/// Reads a mutation's body and rebuilds the request around it.
async fn read_body(request: Request) -> Result<(Request, Vec<u8>), Box<Response>> {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, MUTATION_BODY_LIMIT).await {
        Ok(bytes) => bytes.to_vec(),
        Err(_) => {
            return Err(Box::new(plain_with(
                StatusCode::PAYLOAD_TOO_LARGE,
                "413 Payload Too Large: this form is larger than the request limit",
            )));
        }
    };
    Ok((Request::from_parts(parts, Body::from(bytes.clone())), bytes))
}

/// The Fetch-Metadata rule, as a pure function of the headers.
fn fetch_metadata_is_hostile(headers: &HeaderMap) -> bool {
    sec_fetch_site_values(headers).any(|site| {
        site.eq_ignore_ascii_case("cross-site") || site.eq_ignore_ascii_case("cross-origin")
    })
}

/// Every `Sec-Fetch-Site` token across repeated headers and comma-separated values, trimmed.
///
/// The header is a structured field whose items are comma-separated and this is a security check:
/// comparing only `headers.get(...)` would let `"same-origin, cross-site"` through. The value comes
/// from the browser and is not attacker-controllable, but tolerance here costs one iterator and closes
/// a parsing-shaped hole.
fn sec_fetch_site_values(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");
    headers
        .get_all(SEC_FETCH_SITE)
        .iter()
        .filter_map(|value| std::str::from_utf8(value.as_bytes()).ok())
        .flat_map(|raw| raw.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// Which admin routes exist. One list, so a new admin page cannot be mounted without the gate.
fn admin_route(path: &str) -> bool {
    matches!(
        path,
        "/debug/admin"
            | "/debug/admin/invitations"
            | "/debug/admin/recovery"
            | "/debug/admin/suspend"
            | "/debug/admin/activate"
            | "/debug/maintenance"
    )
}

/// The auth page a path belongs to, shared with the classifier so the two cannot disagree.
fn auth_page(path: &str) -> Option<web::AuthPage> {
    match path {
        "/debug/login" => Some(web::AuthPage::Login),
        "/debug/invite" => Some(web::AuthPage::Invite),
        "/debug/recover" => Some(web::AuthPage::Recover),
        _ => None,
    }
}

/// A refusal with a status other than 403. Same plain-text shape as the guard's 403s, so a body that
/// proves no handler ran is recognisable whichever code it carries.
fn plain_with(status: StatusCode, reason: &str) -> Response {
    (
        status,
        [(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        reason.to_string(),
    )
        .into_response()
}

fn header(headers: &HeaderMap, name: HeaderName) -> Option<&str> {
    headers
        .get(name)
        .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// `true` unless the legacy list is empty or wildcarded — the rule `WebPolicy` encodes as a refusal to
/// be constructed at all.
fn legacy_check_active(allowed_hosts: &[String]) -> bool {
    !allowed_hosts.is_empty() && !allowed_hosts.iter().any(|entry| entry.trim() == "*")
}

/// The legacy match: an entry equals the header exactly, or equals it with the port stripped, because
/// operators write `allowed_hosts = ["localhost"]` while the browser sends `localhost:3000` and an
/// IPv6 literal must keep its brackets. A missing `Host` under an active check is a refusal.
fn legacy_host_in_list(allowed_hosts: &[String], host: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    let bare = web::strip_port(host);
    allowed_hosts
        .iter()
        .any(|entry| entry.eq_ignore_ascii_case(host) || entry.eq_ignore_ascii_case(bare))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::test_support as support;
    use tower::ServiceExt;

    /// Every route the router serves, from one list. A new route belongs here: this is the tripwire for
    /// registering one above the guard layer instead of below it, which would otherwise escape every
    /// check in this file silently.
    const GUARDED_PATHS: &[&str] = &[
        "/debug",
        "/debug/memories",
        "/debug/memories/fact%3Aguard",
        "/debug/clusters",
        "/debug/clusters/cluster%3Aguard",
        "/debug/sessions",
        "/debug/sessions/sess-guard",
        "/debug/graph/fact%3Aguard",
        "/debug/api/graph/fact%3Aguard",
        "/debug/maintenance",
        "/debug/reminder-mapping",
        "/debug/operations",
        // A braced mount is covered only by a concrete sample below its head (`matrix_covers`), so the
        // history detail page needs its own entry just as the memories detail page does.
        "/debug/operations/semantic_operation%3Aguard",
        // Task 11's owner surface: three reads and three mutations. The reads belong here because they
        // enumerate the caller's corpus, the mutations because they write it.
        "/debug/collections",
        "/debug/collections/archive",
        "/debug/classification",
        "/debug/refinement",
        "/debug/refinement/settings",
        "/debug/refinement/proposals/refinement_proposal%3Aguard",
        "/debug/refinement/attempts/refinement_attempt%3Aguard",
        "/debug/query",
        "/debug/account",
        "/debug/admin",
        "/debug/assets/htmx-2.0.10.min.js",
        "/debug/login",
        "/debug/invite",
        "/debug/recover",
    ];

    /// The mutations the router mounts. Enumerated apart from the reads because the guard treats them
    /// differently, and a new one escaping the CSRF half is the failure this list exists to catch.
    const GUARDED_MUTATIONS: &[&str] = &[
        "/debug/query/run",
        "/debug/reminder-mapping/release",
        "/debug/collections/edit",
        "/debug/collections/move",
        "/debug/classification/decide",
        "/debug/classification/revise",
        "/debug/refinement/consent",
        "/debug/refinement/mode",
        "/debug/refinement/enqueue",
        "/debug/refinement/proposals/{id}/approve",
        "/debug/refinement/proposals/{id}/dismiss",
        "/debug/refinement/proposals/{id}/defer",
        "/debug/refinement/proposals/{id}/revert",
        "/debug/logout",
        "/debug/account/tokens",
        "/debug/account/credentials/revoke",
        "/debug/account/recovery",
        "/debug/admin/invitations",
        "/debug/admin/recovery",
        "/debug/admin/suspend",
        "/debug/admin/activate",
        "/debug/login",
        "/debug/invite",
        "/debug/recover",
    ];

    fn is_guarded(path: &str) -> bool {
        GUARDED_PATHS.contains(&path) || GUARDED_MUTATIONS.contains(&path)
    }

    /// The coverage rule, lifted out of `every_route_the_router_mounts_is_in_the_matrix` so the rule can
    /// be tested and not only applied.
    ///
    /// A **literal** mount is covered only by an exact entry in one of the matrices. A **braced** mount
    /// covers a subtree, so it is covered when some matrix entry sits under its head — which is why
    /// `GUARDED_PATHS` carries a concrete sample id per braced route: `/debug/memories/{id}` is covered by
    /// `/debug/memories/fact%3Aguard`, and a braced mount with no listed sample is a mount nobody read
    /// yet. Nothing is covered by sharing *bytes* with an entry: `/debug/admin-secret` and `/debug/admin`
    /// are two routes, and a rule that accepted that neighbour would go silent at exactly the moment the
    /// tripwire exists to fire — a new page mounted beside an old one, gated by nothing but its own name.
    fn matrix_covers(path: &str) -> bool {
        match path.split_once('{') {
            Some((head, _)) => GUARDED_PATHS.iter().any(|known| known.starts_with(head)),
            None => is_guarded(path),
        }
    }

    #[test]
    fn prefix_sharing_is_not_coverage() {
        // The tripwire exists to shout when a route is mounted outside both matrices. A new admin page
        // named `/debug/admin-secret` is a *distinct* route, and the matrix carries `/debug/admin` — while
        // the guard's own `admin_route` list is a separate `matches!` that would not include it, so a
        // mount like that is a page no admin gate covers and no matrix entry names.
        for neighbour in [
            "/debug/admin-secret",
            "/debug/administrator",
            "/debug/query-runner",
            "/debug/accounting",
            "/debug/logged-out",
        ] {
            assert!(
                !matrix_covers(neighbour),
                "`{neighbour}` is not a mounted route and shares only a prefix with a matrix entry, so \
                 the matrix must not report it as covered"
            );
        }

        // The positives, which are the shape of the router rather than of the rule: an exact read entry,
        // an exact admin entry, an exact mutation entry, and every braced mount the router actually has.
        // A tightening that fails these would trade a silent tripwire for a screaming one.
        assert!(
            matrix_covers("/debug/admin"),
            "an exact matrix entry covers"
        );
        assert!(
            matrix_covers("/debug/maintenance"),
            "an exact admin-only entry covers"
        );
        assert!(
            matrix_covers("/debug/query/run"),
            "a mounted mutation is covered by its own entry"
        );
        for braced in [
            "/debug/memories/{id}",
            "/debug/clusters/{id}",
            "/debug/sessions/{external_id}",
            "/debug/graph/{id}",
            "/debug/api/graph/{id}",
            "/debug/refinement/proposals/{id}",
            "/debug/refinement/attempts/{id}",
            "/debug/assets/{name}",
        ] {
            assert!(
                matrix_covers(braced),
                "`{braced}` is mounted with a parameter and its matrices list a sample below its head"
            );
        }
    }

    #[test]
    fn every_route_the_router_mounts_is_in_the_matrix() {
        // The router's own table, read as text: a route added to `debug::build` and not to the lists
        // above is the exact drift this asserts against.
        let source = include_str!("mod.rs");
        let mut mounted = Vec::new();
        for line in source.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix(".route(")
                && let Some(path) = rest.split('"').nth(1)
            {
                mounted.push(path.to_string());
            }
        }
        assert!(
            mounted.len() >= 20,
            "the scan found {} routes; the router moved shape and this test is now vacuous: {mounted:?}",
            mounted.len()
        );
        for path in mounted {
            assert!(
                matrix_covers(&path),
                "`{path}` is mounted by `debug::build` and is in neither guard matrix"
            );
        }
    }

    #[tokio::test]
    async fn no_context_router_denies_every_private_route_and_serves_only_assets() {
        let app = support::no_context_router().await;
        for path in GUARDED_PATHS {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(*path)
                        .header(axum::http::header::HOST, "localhost")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let content_type = response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let body = text(response).await;
            if path.starts_with("/debug/assets/") {
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "the closed asset allowlist must survive a no-auth build: {body}"
                );
                continue;
            }
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "GET {path} was served with no auth context: {body}"
            );
            assert!(
                content_type.starts_with("text/plain"),
                "GET {path} answered {content_type}, which means a handler rendered markup"
            );
            assert!(
                !body.contains("<nav") && !body.contains("<table"),
                "GET {path} rendered page markup: {}",
                &body[..body.len().min(200)]
            );
        }
        for path in GUARDED_MUTATIONS {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(*path)
                        .header(axum::http::header::HOST, "localhost")
                        .header(
                            axum::http::header::CONTENT_TYPE,
                            "application/x-www-form-urlencoded",
                        )
                        .body(Body::from("csrf=x&mode=retrieve&query=y"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "POST {path} was served with no auth context"
            );
        }
    }

    #[tokio::test]
    async fn an_authenticated_owner_reads_their_pages_and_is_denied_the_admin_ones() {
        // The positive control the denial above needs, in the same file: same router shape, real
        // session, 200s.
        let fixture = support::authenticated_app(true).await;
        for path in [
            "/debug",
            "/debug/memories",
            "/debug/clusters",
            "/debug/sessions",
            "/debug/query",
            "/debug/account",
        ] {
            let (status, body) = fixture.get(path).await;
            assert_eq!(status, StatusCode::OK, "{path}: {}", support::clip(&body));
        }
        for path in ["/debug/admin", "/debug/maintenance"] {
            let (status, _) = fixture.get(path).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{path} served to a plain owner"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_host_denies_under_an_armed_policy_and_a_valid_host_reads() {
        let fixture = support::authenticated_app(true).await;
        let (status, _) = fixture.get("/debug/memories").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "positive control with the configured Host"
        );
        let response = fixture
            .get_with_host("/debug/memories", "evil.example", Some(&fixture.session))
            .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // …and an absent `Host` is a refusal once a policy is armed, which is the reversal of the
        // pre-Task 9 `test_missing_host_is_allowed_when_the_check_is_off`.
        let mut request = Request::builder()
            .method("GET")
            .uri("/debug/memories")
            .body(Body::empty())
            .unwrap();
        request.headers_mut().remove(axum::http::header::HOST);
        let response = fixture
            .router_only()
            .oneshot(with_cookie(request, &fixture))
            .await
            .expect("the router served");
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "an absent Host was served under an armed policy"
        );
    }

    #[tokio::test]
    async fn the_legacy_build_keeps_the_wildcard_host_skip_it_earned() {
        // Reversal, stated as a boundary rather than a permission: `router_with_context(server,
        // Some(ctx))` with `["*"]` still skips the *Host* check — because there is no policy to arm it
        // — and still serves nothing, because the absence of an auth context is what denies. The
        // pre-Task 9 expectation was "wildcard Host ⇒ private data readable"; the surviving half is
        // only "wildcard Host ⇒ no Host refusal", which now has no data behind it.
        let app = support::legacy_router_with_wildcard_hosts().await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/debug")
                    .header(axum::http::header::HOST, "evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = text(response).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "private data survived: {body}"
        );
        assert!(
            body.contains("no authentication context"),
            "the refusal should name the missing boundary, not the Host: {body}"
        );
    }

    #[tokio::test]
    async fn cross_site_is_refused_even_with_a_valid_token_and_direct_navigation_is_not() {
        let fixture = support::authenticated_app(true).await;
        for site in ["cross-site", "cross-origin", "same-origin, cross-site"] {
            let (status, body) = fixture
                .post_with_site("/debug/query/run", "mode=retrieve&query=q", site)
                .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "Sec-Fetch-Site {site} mutated with a valid token: {}",
                support::clip(&body)
            );
        }
        let (status, body) = fixture
            .post_with_site("/debug/query/run", "mode=retrieve&query=q", "none")
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "`none` is direct navigation: {}",
            support::clip(&body)
        );
    }

    #[tokio::test]
    async fn a_get_with_a_valid_session_still_needs_no_token_and_a_cross_site_get_needs_a_session()
    {
        // The reversal of `test_get_with_cross_site_is_allowed`, said precisely: a cross-site *GET* of
        // a private page is refused — not because of the header, but because a private route needs an
        // authenticated owner and an unauthenticated one gets 403. A same-origin cross-site-labelled
        // GET *with* a session is not a CSRF question at all, and renders.
        let fixture = support::authenticated_app(true).await;
        let anonymous = fixture.get_with_cookie("/debug/memories", "").await;
        assert_eq!(
            anonymous.0,
            StatusCode::FORBIDDEN,
            "an anonymous private read succeeded"
        );
        let (status, body) = fixture.get_with_site("/debug/memories", "cross-site").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a signed-in GET is not a submission: {}",
            support::clip(&body)
        );
    }

    #[tokio::test]
    async fn the_fetch_metadata_and_origin_rules_are_pure_functions_of_headers() {
        // The table, without a server: these three are the checks whose *combination* the router tests
        // above exercise, pinned individually so a change to one is visible in one name.
        let mut hostile = HeaderMap::new();
        hostile.insert(
            HeaderName::from_static("sec-fetch-site"),
            "Same-Origin, CROSS-SITE".parse().unwrap(),
        );
        assert!(fetch_metadata_is_hostile(&hostile));
        let mut friendly = HeaderMap::new();
        friendly.insert(
            HeaderName::from_static("sec-fetch-site"),
            "same-origin, none".parse().unwrap(),
        );
        assert!(!fetch_metadata_is_hostile(&friendly));
        assert!(
            !fetch_metadata_is_hostile(&HeaderMap::new()),
            "absent metadata is not hostile"
        );

        assert!(is_read_only(&axum::http::Method::GET));
        assert!(is_read_only(&axum::http::Method::HEAD));
        assert!(!is_read_only(&axum::http::Method::POST));
        // `OPTIONS` is not exempt: this router advertises no CORS configuration, so a preflight here is
        // the attack rather than a use case.
        assert!(!is_read_only(&axum::http::Method::OPTIONS));

        assert!(legacy_check_active(&["localhost".to_string()]));
        assert!(!legacy_check_active(&[]));
        assert!(!legacy_check_active(&["  ".to_string(), "*".to_string()]));
        assert!(legacy_host_in_list(
            &["localhost".to_string()],
            Some("localhost:3000")
        ));
        assert!(legacy_host_in_list(
            &["[::1]".to_string()],
            Some("[::1]:3000")
        ));
        assert!(!legacy_host_in_list(&["localhost".to_string()], None));
    }

    async fn text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a readable body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn with_cookie(mut request: Request, fixture: &support::AuthenticatedTestApp) -> Request {
        request.headers_mut().insert(
            axum::http::header::COOKIE,
            fixture
                .session
                .cookie_header()
                .parse()
                .expect("a cookie header"),
        );
        request
    }
}
