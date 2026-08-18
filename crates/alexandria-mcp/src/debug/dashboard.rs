use axum::extract::State;
use axum::response::Html;

use super::html::{esc, layout};
use crate::AlexandriaServer;

pub async fn handler(State(server): State<AlexandriaServer>) -> Html<String> {
    let body = match alexandria_storage::stats::gather(server.db.inner()).await {
        Ok(stats) => format!(
            r#"<h1>Alexandria Debug Dashboard</h1>
<table>
<tr><th>Facts (active)</th><td>{}</td></tr>
<tr><th>Facts (deleted)</th><td>{}</td></tr>
<tr><th>Clusters</th><td>{}</td></tr>
<tr><th>Edges</th><td>{}</td></tr>
<tr><th>Raw documents</th><td>{}</td></tr>
</table>"#,
            stats.fact_count,
            stats.deleted_fact_count,
            stats.cluster_count,
            stats.edge_count,
            stats.raw_count,
        ),
        Err(e) => format!(
            r#"<p class="error">Failed to load stats: {}</p>"#,
            esc(&e.to_string())
        ),
    };
    Html(layout("Dashboard", &body))
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
