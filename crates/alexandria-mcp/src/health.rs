//! Liveness endpoint for external supervisors (process managers, container
//! healthchecks, agent memory-status panels).
//!
//! Deliberately separate from the debug UI: the dashboard is a human-facing surface
//! with templates and state, while `/health` is a machine contract — stable shape,
//! no auth (this server has none anywhere), and nothing sensitive in the body.
//! A 200 means one trivial query round-tripped through the engine at request time,
//! not that the process merely exists; a wedged or closed store answers 503.

use std::{future::Future, sync::Arc, time::Duration};

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
    probe_response(
        async { db.ping().await.map_err(|err| err.to_string()) },
        Duration::from_secs(1),
    )
    .await
}

async fn probe_response(
    probe: impl Future<Output = Result<(), String>>,
    deadline: Duration,
) -> Response {
    let result = tokio::time::timeout(deadline, probe)
        .await
        .unwrap_or_else(|_| Err("storage health probe timed out".to_owned()));
    let (status, body) = match result {
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
    async fn health_is_503_when_the_probe_never_completes() {
        let response = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            probe_response(
                std::future::pending::<Result<(), String>>(),
                std::time::Duration::from_millis(1),
            ),
        )
        .await
        .expect("the health deadline must bound a pending probe");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "unavailable");
        assert!(json["error"].as_str().unwrap().contains("timed out"));
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
