//! Liveness endpoint for external supervisors (process managers, container
//! healthchecks, agent memory-status panels).
//!
//! Deliberately separate from the debug UI: the dashboard is a human-facing surface
//! with templates and state, while `/health` is a machine contract — stable shape,
//! no auth (this server has none anywhere), and nothing sensitive in the body.
//! A 200 means one trivial query round-tripped through the engine at request time,
//! not that the process merely exists; a wedged or closed store answers 503.

use std::sync::Arc;

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use alexandria_storage::Database;

/// Router carrying the top-level `/health` route. Merged alongside the MCP service
/// and the debug router in the binary's HTTP setup.
pub fn router(db: Arc<Database>) -> Router {
    Router::new().route(
        "/health",
        get(move || {
            let db = Arc::clone(&db);
            async move { check(db).await }
        }),
    )
}

async fn check(db: Arc<Database>) -> Response {
    let (status, body) = match db.ping().await {
        Ok(()) => (
            StatusCode::OK,
            serde_json::json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
            }),
        ),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({
                "status": "unavailable",
                "error": err.to_string(),
            }),
        ),
    };
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn get_health(db: Arc<Database>) -> (StatusCode, String) {
        let response = router(db)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn health_is_200_with_version_on_a_live_store() {
        let db = Arc::new(Database::connect_embedded().await.unwrap());
        let (status, body) = get_health(db).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn health_is_503_when_the_engine_stops_answering() {
        let db = Arc::new(Database::connect_embedded().await.unwrap());
        // 3.3's client API has no close(); repointing the handle at a namespace that
        // does not exist makes every subsequent query fail — the wedged-engine
        // stand-in this endpoint must catch.
        db.inner()
            .use_ns("alexandria-health-probe-missing")
            .await
            .unwrap();
        let (status, body) = get_health(db).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["status"], "unavailable");
        assert!(json["error"].is_string());
    }
}
