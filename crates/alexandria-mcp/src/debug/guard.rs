//! Two narrow request guards over the debug router.
//!
//! # Threat model
//!
//! The debug UI is merged into the HTTP app with no authentication, on the argument that it is
//! an operator-local surface (see README, "Publish the port only behind a reverse proxy"). Two
//! attacks survive that argument, and this module is what stops them:
//!
//! 1. **CSRF → heat write.** `/debug/query/run` is the one sanctioned *write* route: a
//!    non-dry `retrieve` runs spreading activation and bumps heat on the top-N results. The form
//!    posts `application/x-www-form-urlencoded` with no custom header, so it is a CORS *simple
//!    request* — no preflight, and `Content-Type: application/x-www-form-urlencoded` needs no
//!    custom header either. A hidden auto-submitting form on any page the operator visits can
//!    therefore drive `mode=retrieve&query=<attacker-chosen>`. The attacker cannot read the
//!    response, but heat feeds ranking, so the integrity of retrieval is damaged without any
//!    readable response at all.
//! 2. **DNS rebinding → same-origin read.** A remote page's own domain can resolve to
//!    `127.0.0.1`, after which the page fetches `/debug/sessions` *same-origin* and reads it in
//!    full — including LLM-written session summaries and tags, the most sensitive prose in the
//!    system. **Binding to loopback does not help**: the request arrives from the operator's own
//!    browser, which is exactly what makes it look local. The only server-side signal that
//!    distinguishes it is the `Host` header, which is why an opt-in `allowed_hosts` check lives
//!    here as well as in rmcp's `/mcp` config.
//!
//! # Decision table
//!
//! The CSRF half is **unconditional** and the Host half is **opt-in**:
//!
//! | condition | result |
//! |---|---|
//! | method is GET or HEAD | Host half only |
//! | `Sec-Fetch-Site` absent (curl, tests, non-browser clients) | allowed |
//! | `Sec-Fetch-Site` `same-origin` / `same-site` / `none` | allowed |
//! | other method + `Sec-Fetch-Site` `cross-site` or `cross-origin` | 403 |
//! | `allowed_hosts` empty or containing `"*"` (today's default) | Host check skipped |
//! | `allowed_hosts` set, `Host` matches an entry with or without port | allowed |
//! | `allowed_hosts` set, `Host` mismatches or is missing | 403 |
//!
//! The CSRF half deliberately allows an *absent* `Sec-Fetch-Site`: that is the shape of every
//! curl call, every test, and every non-browser client, and it is not the shape the attack takes
//! (a browser-driven form post always carries the header). Requiring presence would break the
//! surface for no additional safety, because the attacker cannot make a browser omit it.
//!
//! `none` must be allowed: that is direct navigation — typed URL, bookmark, opening a fresh tab.
//! Blocking it would make the UI unusable while doing nothing against either attack (a rebound
//! fetch is `same-origin`, and a hidden form post is `cross-site`).

use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::HOST;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;

/// The guard's configuration. `Vec<String>` would do as the middleware state, but the name is the
/// documentation: this is the opt-in half of [`enforce`], and an empty list means "off".
#[derive(Debug, Clone, Default)]
pub(super) struct Guard {
    /// Mirrors `[server] allowed_hosts`, which is also what rmcp guards `/mcp` with. Populated in
    /// `main.rs` from the same `Config`, so the debug UI and `/mcp` cannot disagree about what a
    /// legitimate host is.
    pub(super) allowed_hosts: Vec<String>,
}

/// The value of `Sec-Fetch-Site` that means "some other site caused this request".
const CROSS_SITE: &str = "cross-site";
/// The older/`Sec-Fetch-*` spelling some engines use for the same thing. Both are blocked because
/// the header is advisory and we would rather have the stricter reading win.
const CROSS_ORIGIN: &str = "cross-origin";

pub(super) async fn enforce(State(guard): State<Guard>, request: Request, next: Next) -> Response {
    match rejection(
        guard.allowed_hosts.as_slice(),
        request.method(),
        request.headers(),
    ) {
        Some(reason) => rejection_response(reason),
        None => next.run(request).await,
    }
}

/// The whole policy, as a pure function of the parts of a request the guards can see — so the
/// tests can assert the table above without a server, a database or an `async` round trip.
fn rejection(
    allowed_hosts: &[String],
    method: &Method,
    headers: &HeaderMap,
) -> Option<&'static str> {
    // Half 1: CSRF. Applies to every caller, configured or not, so a future `router()` caller
    // cannot get the weaker build by forgetting to pass a DebugContext.
    if !is_read_only(method) {
        for site in sec_fetch_site_values(headers) {
            // Case-insensitive and comma-tolerant rather than clever: the spec's tokens are
            // lowercase, but a check that misses `Cross-Site` misses the whole attack.
            if site.eq_ignore_ascii_case(CROSS_SITE) || site.eq_ignore_ascii_case(CROSS_ORIGIN) {
                return Some(
                    "403 Forbidden: cross-site submissions to the debug UI are not allowed \
                     (the Query Tester writes heat; see debug/guard.rs)",
                );
            }
        }
    }

    // Half 2: Host / DNS rebinding. Opt-in, so the shipped default (["*"]) behaves exactly as it
    // did before this guard existed and nobody can be locked out by an upgrade.
    if host_check_active(allowed_hosts) {
        let host = headers
            .get(HOST)
            .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        match host {
            // Hostnames are case-insensitive and operators write `allowed_hosts = ["localhost"]`
            // while the browser sends `Host: localhost:3000`, so both forms are accepted: an entry
            // matches the header exactly, or matches it with the port stripped. Without the second
            // form the common configuration is a self-inflicted lockout — and an IPv6 literal
            // still needs the brackets to survive the strip (see [`strip_port`]).
            Some(value) if allowed_hosts.iter().any(|entry| host_matches(entry, value)) => {}
            Some(_) => {
                return Some(
                    "403 Forbidden: Host header is not in [server] allowed_hosts (DNS rebinding \
                     defence; see debug/guard.rs)",
                );
            }
            // HTTP/1.1 requires `Host`, so its absence is either a non-conforming client or a
            // rebinding artefact. Either way, when the check is armed there is nothing to match.
            None => {
                return Some(
                    "403 Forbidden: request has no Host header to check against allowed_hosts",
                );
            }
        }
    }

    None
}

/// Only reads are CSRF-safe. `HEAD` for completeness; `OPTIONS` is *not* exempt — we advertise no
/// CORS configuration, so a cross-site preflight here is the attack, not a use case.
fn is_read_only(method: &Method) -> bool {
    *method == Method::GET || *method == Method::HEAD
}

/// `true` unless the list is empty or wildcarded. Same wildcard semantics as `main.rs`'s
/// `disable_allowed_hosts()` branch for `/mcp`, so one config value means one thing.
fn host_check_active(allowed_hosts: &[String]) -> bool {
    !allowed_hosts.is_empty() && !allowed_hosts.iter().any(|entry| entry.trim() == "*")
}

fn host_matches(entry: &str, host: &str) -> bool {
    let entry = entry.trim();
    !entry.is_empty()
        && (entry.eq_ignore_ascii_case(host) || entry.eq_ignore_ascii_case(strip_port(host)))
}

/// Strip the authority's port, leaving an IPv6 literal's brackets intact.
///
/// `localhost:3000` → `localhost`, `[::1]:3000` → `[::1]`, `[::1]` → `[::1]`, `localhost` →
/// `localhost`. The naive `rsplit_once(':')` mangles the bracketed forms, which are exactly what
/// an operator binding to `::1` would put in `allowed_hosts`.
fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.find(']') {
            // `end` indexes into `rest`, which is one byte into `host`, so +2 is the closing
            // bracket's position + 1 — the slice that still includes both brackets.
            Some(end) => &host[..end + 2],
            None => host,
        };
    }
    match host.rsplit_once(':') {
        Some((name, port))
            if !name.is_empty()
                && !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            name
        }
        _ => host,
    }
}

/// Every `Sec-Fetch-Site` token across repeated headers and comma-separated values, trimmed.
///
/// The header is a structured field whose items are comma-separated, and this is a security
/// check: comparing only `headers.get(...)` would let `"same-origin, cross-site"` through. The
/// value comes from the browser and is not attacker-controllable, but tolerance here costs one
/// iterator and closes a parsing-shaped hole.
fn sec_fetch_site_values(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");
    headers
        .get_all(SEC_FETCH_SITE)
        .iter()
        // `HeaderValue::as_bytes` is visible ASCII or the value could not have been constructed;
        // `from_utf8` is a total conversion in practice, and `unwrap_or("")` keeps it that way
        // rather than panicking on a malformed request.
        .filter_map(|value| std::str::from_utf8(value.as_bytes()).ok())
        .flat_map(|raw| raw.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// A short plain-text 403 rather than an empty body: whoever hits it — an operator behind a
/// misconfigured reverse proxy, or someone reading a rebinding repro — sees which half refused.
fn rejection_response(reason: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        reason.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::DebugContext;
    use axum::body::Body;
    use axum::body::to_bytes;
    use axum::http::Uri;
    use tower::ServiceExt;

    /// The exact body the Query Tester posts, so an allowed POST is allowed *for the right
    /// reason* — it reaches `query::run` and returns its fragment, not a 400 from serde.
    const FORM_BODY: &str = "mode=retrieve&query=guard-probe&limit=5";

    fn ctx(allowed_hosts: &[&str]) -> DebugContext {
        DebugContext {
            embedding_model: "guard-test-model".to_string(),
            embedding_device: "cpu".to_string(),
            transport: "http".to_string(),
            bind_host: "127.0.0.1".to_string(),
            bind_port: 3999,
            data_dir: ":memory:".to_string(),
            cluster_merge_threshold: 0.7,
            maintenance_interval_secs: 300,
            allowed_hosts: allowed_hosts.iter().map(|h| (*h).to_string()).collect(),
        }
    }

    async fn app_with(allowed_hosts: &[&str]) -> axum::Router {
        let server = crate::debug::test_support::test_server().await;
        crate::debug::router_with_context(server, Some(ctx(allowed_hosts)))
    }

    /// Send one request and report status plus body text, without asserting anything — the
    /// `Request::builder()` idiom the other debug tests use, table-driven.
    async fn send(
        app: axum::Router,
        method: &str,
        uri: &str,
        sec_fetch_site: Option<&str>,
        host: Option<&str>,
        form: bool,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri.parse::<Uri>().unwrap());
        if let Some(site) = sec_fetch_site {
            builder = builder.header("sec-fetch-site", site);
        }
        if let Some(host) = host {
            builder = builder.header("host", host);
        }
        if form {
            builder = builder.header("content-type", "application/x-www-form-urlencoded");
        }
        let body = if form {
            Body::from(FORM_BODY)
        } else {
            Body::from("")
        };
        let response = app.oneshot(builder.body(body).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn post_run(app: axum::Router, sec_fetch_site: Option<&str>) -> StatusCode {
        send(
            app,
            "POST",
            "http://localhost/debug/query/run",
            sec_fetch_site,
            Some("localhost"),
            true,
        )
        .await
        .0
    }

    async fn get(app: axum::Router, sec_fetch_site: Option<&str>) -> StatusCode {
        send(
            app,
            "GET",
            "http://localhost/debug/sessions",
            sec_fetch_site,
            Some("localhost"),
            false,
        )
        .await
        .0
    }
    // ---- half 1: CSRF, unconditional -------------------------------------------------------

    #[tokio::test]
    async fn test_cross_site_post_is_rejected() {
        // The attack: a hidden auto-submitting form on a remote page. Run against three Host
        // configurations — off, wildcarded, and restrictive-but-satisfied — because the CSRF half
        // must not care about any of them.
        for hostile in [CROSS_SITE, CROSS_ORIGIN] {
            for hosts in [vec![], vec!["*"], vec!["localhost"]] {
                let app = app_with(&hosts).await;
                let (status, body) = send(
                    app,
                    "POST",
                    "http://localhost/debug/query/run",
                    Some(hostile),
                    Some("localhost"),
                    true,
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "Sec-Fetch-Site: {hostile} with allowed_hosts={hosts:?}"
                );
                assert!(
                    body.contains("cross-site"),
                    "rejection should explain itself, got {body}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_rejection_is_plain_text() {
        let app = app_with(&[]).await;
        let (status, body) = send(
            app,
            "POST",
            "http://localhost/debug/query/run",
            Some(CROSS_SITE),
            Some("localhost"),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            body.contains("debug/guard.rs"),
            "the reason should point at the code that produced it, got {body}"
        );
    }

    #[tokio::test]
    async fn test_same_origin_same_site_and_none_posts_are_allowed() {
        for site in ["same-origin", "same-site", "none"] {
            let app = app_with(&[]).await;
            assert_eq!(
                post_run(app, Some(site)).await,
                StatusCode::OK,
                "Sec-Fetch-Site: {site} must not be blocked"
            );
        }
    }

    #[tokio::test]
    async fn test_post_without_sec_fetch_site_is_allowed() {
        // The curl/test/non-browser path. It carries no `Sec-Fetch-Site` at all, and must keep
        // working — that is why the guard blocks on presence-of-hostile-value, not absence.
        let app = app_with(&[]).await;
        assert_eq!(post_run(app, None).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_with_cross_site_is_allowed() {
        // Only state-changing methods are CSRF-relevant; blocking GET would break following
        // links out of chat clients and documents.
        let app = app_with(&[]).await;
        assert_eq!(
            get(app, Some(CROSS_SITE)).await,
            StatusCode::OK,
            "a cross-site GET must still render"
        );
    }

    #[tokio::test]
    async fn test_header_is_matched_case_insensitively_and_ignoring_whitespace() {
        for variant in ["Cross-Site", "  cross-site  ", "SAME-ORIGIN, cross-origin"] {
            let app = app_with(&[]).await;
            assert_eq!(
                post_run(app, Some(variant)).await,
                StatusCode::FORBIDDEN,
                "{variant:?} must be rejected"
            );
        }
    }

    /// The CSRF half must not depend on `DebugContext` being supplied — `router(server)` passes
    /// `None`, and that is what every stdio-mode and test caller uses.
    #[tokio::test]
    async fn test_csrf_guard_applies_without_debug_context() {
        let server = crate::debug::test_support::test_server().await;
        let app = crate::debug::router(server);
        assert_eq!(post_run(app, Some(CROSS_SITE)).await, StatusCode::FORBIDDEN);
        let app = crate::debug::router(crate::debug::test_support::test_server().await);
        assert_eq!(post_run(app, None).await, StatusCode::OK);
    }

    // ---- half 2: Host / DNS rebinding, opt-in ---------------------------------------------

    #[tokio::test]
    async fn test_wildcard_and_empty_allowed_hosts_skip_the_host_check() {
        // Both spellings of "off", and both must let the rebinding Host through unchanged: this
        // is exactly today's shipped default, and the fix may not alter it.
        for configured in [vec!["*"], vec![]] {
            let app = app_with(&configured).await;
            let (status, _) = send(
                app,
                "GET",
                "http://evil.example/debug/sessions",
                None,
                Some("evil.example"),
                false,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "allowed_hosts {configured:?}");
        }
    }

    #[tokio::test]
    async fn test_restrictive_allowed_hosts_accepts_both_host_forms() {
        // `localhost` (no port) is what operators write; the browser sends `localhost:3000`.
        // Accepting only the literal would be a self-inflicted lockout.
        for (entry, header) in [
            ("localhost", "localhost"),
            ("localhost", "localhost:3000"),
            ("localhost:3000", "localhost:3000"),
            ("LOCALHOST", "localhost"),
            ("[::1]", "[::1]:3000"),
            ("[::1]:4567", "[::1]:4567"),
        ] {
            let app = app_with(&[entry]).await;
            let (status, body) = send(
                app,
                "GET",
                &format!("http://{header}/debug/sessions"),
                None,
                Some(header),
                false,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{entry} should match {header}, got {body}"
            );
        }
    }

    #[tokio::test]
    async fn test_restrictive_allowed_hosts_rejects_foreign_and_missing_host() {
        let app = app_with(&["localhost"]).await;
        let (status, body) = send(
            app,
            "GET",
            "http://evil.example/debug/sessions",
            None,
            Some("evil.example"),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("allowed_hosts"), "got {body}");

        // A POST, so the rejection cannot be attributed to the method.
        let app = app_with(&["localhost"]).await;
        let (status, _) = send(
            app,
            "POST",
            "http://evil.example/debug/query/run",
            None,
            Some("evil.example"),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // No `Host` header at all: HTTP/1.1 requires it, so its absence is not something an armed
        // check can vouch for. Origin-form URI, no authority → the header really is missing.
        let app = app_with(&["localhost"]).await;
        let (status, body) = send(app, "GET", "/debug/sessions", None, None, false).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("no Host header"), "got {body}");
    }

    #[tokio::test]
    async fn test_missing_host_is_allowed_when_the_check_is_off() {
        // Back-compat pin: the many router tests that build requests with `Request::builder()` and
        // never set a Host must not start failing.
        let app = app_with(&[]).await;
        let (status, _) = send(app, "GET", "/debug/sessions", None, None, false).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// The layer is over the **whole** router, so a route cannot escape it by being added to the
    /// wrong place. `routes` is every path `router_with_context` serves; when you add a route
    /// there, add it here — a 403 on all of them is the proof that the layer, not a per-handler
    /// check, is doing the work.
    #[tokio::test]
    async fn test_every_debug_route_is_guarded() {
        let guarded = [
            "/debug",
            "/debug/assets/layout.css",
            "/debug/memories",
            "/debug/memories/mem%3Aguard",
            "/debug/clusters",
            "/debug/clusters/clu%3Aguard",
            "/debug/sessions",
            "/debug/sessions/sess-guard",
            "/debug/graph/mem%3Aguard",
            "/debug/api/graph/mem%3Aguard",
            "/debug/maintenance",
            "/debug/query",
        ];
        for path in guarded {
            let app = app_with(&["localhost"]).await;
            let (status, _) = send(
                app,
                "GET",
                &format!("http://evil.example{path}"),
                None,
                Some("evil.example"),
                false,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{path} is not covered by the guard layer — it was added outside \
                 `router_with_context`'s layer"
            );
        }
        // The POST is guarded too, via the Host half, so the layer provably covers both halves on
        // every method rather than only the CSRF check reaching it.
        let app = app_with(&["localhost"]).await;
        let (status, _) = send(
            app,
            "POST",
            "http://evil.example/debug/query/run",
            None,
            Some("evil.example"),
            true,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "/debug/query/run must be guarded"
        );
    }

    // ---- pure-function table, no HTTP ------------------------------------------------------

    #[test]
    fn test_strip_port_leaves_ipv6_brackets_intact() {
        for (host, expected) in [
            ("localhost", "localhost"),
            ("localhost:3000", "localhost"),
            ("127.0.0.1:3000", "127.0.0.1"),
            ("[::1]", "[::1]"),
            ("[::1]:3000", "[::1]"),
            ("[::ffff:1.2.3.4]:8080", "[::ffff:1.2.3.4]"),
            ("host:notaport", "host:notaport"),
        ] {
            assert_eq!(strip_port(host), expected, "strip_port({host})");
        }
    }

    #[test]
    fn test_host_check_active_only_when_configured() {
        assert!(!host_check_active(&[]));
        assert!(!host_check_active(&["*".to_string()]));
        assert!(!host_check_active(&[" ".to_string(), "*".to_string()]));
        assert!(host_check_active(&["localhost".to_string()]));
    }
}
