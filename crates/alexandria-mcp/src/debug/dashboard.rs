use askama::Template;
use axum::extract::State;
use axum::response::Response;

use super::html::{error_page, page};
use crate::AlexandriaServer;

/// Counts come from `alexandria_storage::stats::Stats`, flattened into plain fields so the
/// template does not depend on a storage type. Every value is a `usize`, so auto-escaping is
/// a no-op here; the shape is what the legacy `format!` page rendered.
#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    nav: &'static str,
    fact_count: usize,
    deleted_fact_count: usize,
    cluster_count: usize,
    edge_count: usize,
    raw_count: usize,
}

pub async fn handler(State(server): State<AlexandriaServer>) -> Response {
    let stats = match alexandria_storage::stats::gather(server.db.inner()).await {
        Ok(stats) => stats,
        // The legacy page prefixed the raw error, so the prefix travels with the message.
        Err(e) => return error_page("dashboard", &format!("Failed to load stats: {e}")),
    };

    page(DashboardTemplate {
        nav: "dashboard",
        fact_count: stats.fact_count,
        deleted_fact_count: stats.deleted_fact_count,
        cluster_count: stats.cluster_count,
        edge_count: stats.edge_count,
        raw_count: stats.raw_count,
    })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_dashboard_returns_200_with_stats() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("Alexandria Debug Dashboard"));
        assert!(text.contains("Facts (active)"));
    }
}
