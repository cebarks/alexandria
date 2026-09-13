use askama::Template;
use axum::extract::{Query, State};
use axum::response::Response;

use super::html::{error_page, page};
use crate::AlexandriaServer;

#[derive(serde::Deserialize)]
pub struct Pagination {
    pub page: Option<usize>,
}

const PAGE_SIZE: usize = 50;

/// One `maintenance_log` row, flattened for display. The action badge is markup, so it lives
/// in the template and this carries the raw `action` string it switches on; `targets` are the
/// raw record ids and the template links them.
struct MaintenanceRow {
    action: String,
    source_id: String,
    targets: Vec<String>,
    members_moved: i64,
    timestamp: String,
}

/// `prev_href` / `next_href` / `summary` feed `templates/_pagination.html`'s `pager` macro,
/// which is presentational: this page paginates by `?page=N`, so the full hrefs are built
/// here and an empty string means no link on that side.
#[derive(Template)]
#[template(path = "maintenance.html")]
struct MaintenanceTemplate {
    nav: &'static str,
    logs: Vec<MaintenanceRow>,
    prev_href: String,
    next_href: String,
    summary: String,
    total_pages: usize,
    total: usize,
}

pub async fn list(
    State(server): State<AlexandriaServer>,
    Query(params): Query<Pagination>,
) -> Response {
    // `current_page`, not `page`: the render helper `html::page` is in scope in this module.
    let current_page = params.page.unwrap_or(1).max(1);
    let offset = (current_page - 1) * PAGE_SIZE;

    let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
    let total = cluster_repo.count_maintenance_logs().await.unwrap_or(0);
    let logs = match cluster_repo.list_maintenance_logs(PAGE_SIZE, offset).await {
        Ok(l) => l,
        Err(e) => return error_page("maintenance", &e.to_string()),
    };

    let rows = logs
        .into_iter()
        .map(|log| MaintenanceRow {
            action: log.action,
            source_id: log.source_id,
            targets: log.target_ids,
            members_moved: log.members_moved,
            timestamp: log
                .created_at
                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_default(),
        })
        .collect();

    let total_pages = total.div_ceil(PAGE_SIZE);
    let prev_href = if current_page > 1 {
        format!("/debug/maintenance?page={}", current_page - 1)
    } else {
        String::new()
    };
    let next_href = if current_page < total_pages {
        format!("/debug/maintenance?page={}", current_page + 1)
    } else {
        String::new()
    };
    let summary = format!("Page {current_page} of {total_pages} ({total} entries)");

    page(MaintenanceTemplate {
        nav: "maintenance",
        logs: rows,
        prev_href,
        next_href,
        summary,
        total_pages,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::MaintenanceTemplate;
    use askama::Template;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_maintenance_log_empty() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/maintenance")
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
        assert!(text.contains("Maintenance Log"));
        assert!(text.contains("0 entries"));
    }

    #[tokio::test]
    async fn test_maintenance_log_shows_merge() {
        let server = super::super::test_support::test_server().await;
        let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());

        // Set up two clusters and merge them
        let c1 = cluster_repo
            .create(Some("keep"), &[1.0, 0.0])
            .await
            .unwrap();
        let c2 = cluster_repo
            .create(Some("remove"), &[0.98, 0.02])
            .await
            .unwrap();
        let f1 = memory_repo
            .create_fact("fact1", 0.5, &[1.0, 0.0], &[])
            .await
            .unwrap();
        cluster_repo.add_member(&c2, &f1).await.unwrap();

        cluster_repo
            .execute_merge(&c1, &c2, &[0.99, 0.01])
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/maintenance")
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
        assert!(text.contains("merge"));
        assert!(text.contains("1 entries"));
    }

    /// Both integration tests above create at most one log, so `total_pages > 1` is never true
    /// and the `pager` call site is compiled but never rendered — and since both hrefs are
    /// `String`, a swapped prev/next would not be caught by the type checker either.
    #[test]
    fn test_maintenance_template_renders_pager_with_correct_sides() {
        let html = MaintenanceTemplate {
            nav: "maintenance",
            logs: vec![],
            prev_href: "/debug/maintenance?page=1".to_string(),
            next_href: "/debug/maintenance?page=3".to_string(),
            summary: "Page 2 of 3 (101 entries)".to_string(),
            total_pages: 3,
            total: 101,
        }
        .render()
        .unwrap();
        assert!(
            html.contains(r#"href="/debug/maintenance?page=1">← Prev"#),
            "prev must carry the lower page number; got: {html}"
        );
        assert!(
            html.contains(r#"href="/debug/maintenance?page=3">Next →"#),
            "next must carry the higher page number; got: {html}"
        );
        assert!(html.contains("Page 2 of 3 (101 entries)"), "got: {html}");
        assert!(
            !html.contains("<p>101 entries</p>"),
            "multi-page render must not also emit the single-page fallback; got: {html}"
        );
    }
}
