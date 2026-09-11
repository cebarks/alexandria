//! Vendored debug-UI assets, embedded at compile time.
//!
//! Serving is a closed `match` over a `const` allowlist: a request can only ever
//! return one of the two embedded blobs, so path traversal is impossible by
//! construction rather than by sanitization. Filenames carry the upstream version
//! so `immutable` caching is safe.

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

const HTMX: &[u8] = include_bytes!("../../assets/htmx-2.0.10.min.js");
const VIS_NETWORK: &[u8] = include_bytes!("../../assets/vis-network-10.1.2.min.js");

/// Path served to the browser, matching the `<script src>` in `layout.html`.
pub const HTMX_URL: &str = "/debug/assets/htmx-2.0.10.min.js";
pub const VIS_NETWORK_URL: &str = "/debug/assets/vis-network-10.1.2.min.js";

const JS: &str = "text/javascript; charset=utf-8";
const CACHE: &str = "public, max-age=31536000, immutable";

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
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn get(uri: &str) -> (u16, Vec<u8>, String) {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, bytes, ct)
    }

    #[tokio::test]
    async fn test_asset_serves_htmx_with_js_content_type() {
        let (status, bytes, ct) = get("/debug/assets/htmx-2.0.10.min.js").await;
        assert_eq!(status, 200);
        assert!(ct.starts_with("text/javascript"), "got {ct}");
        assert!(bytes.len() > 40_000, "suspiciously small: {}", bytes.len());
        assert!(String::from_utf8_lossy(&bytes).contains("htmx"));
    }

    #[tokio::test]
    async fn test_asset_serves_vis_network() {
        let (status, bytes, _) = get("/debug/assets/vis-network-10.1.2.min.js").await;
        assert_eq!(status, 200);
        assert!(bytes.len() > 500_000, "suspiciously small: {}", bytes.len());
    }

    #[tokio::test]
    async fn test_unknown_asset_is_404() {
        let (status, _, _) = get("/debug/assets/nope.js").await;
        assert_eq!(status, 404);
    }

    /// Traversal must be impossible by construction, not by filtering.
    #[tokio::test]
    async fn test_traversal_attempt_is_404_not_a_file() {
        for name in [
            "..%2F..%2F..%2FCargo.toml",
            "%2e%2e%2fCargo.toml",
            "SHA256SUMS",
            "README.md",
        ] {
            let (status, _, _) = get(&format!("/debug/assets/{name}")).await;
            assert_eq!(status, 404, "{name} must not resolve");
        }
    }
}
