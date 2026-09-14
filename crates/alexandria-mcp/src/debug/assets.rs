//! Vendored debug-UI assets, embedded at compile time.
//!
//! Serving is a closed `match` over a `const` allowlist: a request can only ever
//! return one of the two embedded blobs, so path traversal is impossible by
//! construction rather than by sanitization. Filenames carry the upstream version
//! so `immutable` caching is safe.

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

const HTMX: &[u8] = include_bytes!("../../assets/htmx-2.0.10.min.js");
const VIS_NETWORK: &[u8] = include_bytes!("../../assets/vis-network-10.1.2.min.js");

/// URLs the debug UI embeds via `<script src>`, kept next to the `match` arms
/// below so that renaming an asset has one obvious place to update.
pub const HTMX_URL: &str = "/debug/assets/htmx-2.0.10.min.js";
pub const VIS_NETWORK_URL: &str = "/debug/assets/vis-network-10.1.2.min.js";

// `from_static` is `const`, so an illegal header value is a build error rather
// than a runtime 500 — the same make-it-unrepresentable idiom as the `match`.
const JS: HeaderValue = HeaderValue::from_static("text/javascript; charset=utf-8");
const CACHE: HeaderValue = HeaderValue::from_static("public, max-age=31536000, immutable");

pub async fn asset(Path(name): Path<String>) -> Response {
    let bytes: &'static [u8] = match name.as_str() {
        "htmx-2.0.10.min.js" => HTMX,
        "vis-network-10.1.2.min.js" => VIS_NETWORK,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [(header::CONTENT_TYPE, JS), (header::CACHE_CONTROL, CACHE)],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request};
    use tower::ServiceExt;

    /// Builds the real debug router once per test. Going through
    /// `crate::debug::router` (rather than a hand-routed `Router`) is what makes
    /// these tests fail if the `/debug/assets/{name}` route is ever dropped.
    async fn build_router() -> Router {
        let server = super::super::test_support::test_server().await;
        crate::debug::router(server)
    }

    async fn get(app: &Router, uri: &str) -> (u16, Vec<u8>, HeaderMap) {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        // Clone the headers before `into_body` consumes the response.
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, bytes, headers)
    }

    #[tokio::test]
    async fn test_asset_serves_htmx_with_js_content_type() {
        let app = build_router().await;
        let (status, bytes, headers) = get(&app, super::HTMX_URL).await;
        assert_eq!(status, 200);
        let ct = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(ct, "text/javascript; charset=utf-8");
        assert_eq!(
            headers.get("cache-control").and_then(|v| v.to_str().ok()),
            Some("public, max-age=31536000, immutable"),
            "immutable caching is only safe because the version is in the filename"
        );
        assert!(bytes.len() > 40_000, "suspiciously small: {}", bytes.len());
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("htmx"), "wrong payload served");
        assert!(
            body.contains("2.0.10"),
            "served version must match the version in the filename"
        );
    }

    /// Both `match` arms share one header array, so the assertions in the htmx
    /// test above already cover it; here only the payload is checked.
    #[tokio::test]
    async fn test_asset_serves_vis_network() {
        let app = build_router().await;
        let (status, bytes, _) = get(&app, super::VIS_NETWORK_URL).await;
        assert_eq!(status, 200);
        assert!(bytes.len() > 500_000, "suspiciously small: {}", bytes.len());
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("vis-network"), "wrong payload served");
        assert!(
            body.contains("10.1.2"),
            "served version must match the version in the filename"
        );
    }

    #[tokio::test]
    async fn test_unknown_asset_is_404() {
        let app = build_router().await;
        let (status, _, _) = get(&app, "/debug/assets/nope.js").await;
        assert_eq!(status, 404);
    }

    /// Traversal must be impossible by construction, not by filtering.
    #[tokio::test]
    async fn test_traversal_attempt_is_404_not_a_file() {
        let app = build_router().await;
        for name in [
            "..%2F..%2F..%2FCargo.toml",
            "%2e%2e%2fCargo.toml",
            "SHA256SUMS",
            "README.md",
            "..",
            ".",
            "%2Fetc%2Fpasswd",
        ] {
            let (status, _, _) = get(&app, &format!("/debug/assets/{name}")).await;
            assert_eq!(status, 404, "{name} must not resolve");
        }
    }
}
