use axum::extract::{Query, State};
use axum::response::Html;

use super::html::{esc, layout};
use crate::AlexandriaServer;
use alexandria_storage::record_id_to_string;

#[derive(serde::Deserialize)]
pub struct Pagination {
    pub page: Option<usize>,
}

const PAGE_SIZE: usize = 50;

pub async fn list(
    State(server): State<AlexandriaServer>,
    Query(params): Query<Pagination>,
) -> Html<String> {
    let page = params.page.unwrap_or(1).max(1);
    let offset = (page - 1) * PAGE_SIZE;

    let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
    let total = cluster_repo.count_maintenance_logs().await.unwrap_or(0);
    let logs = match cluster_repo.list_maintenance_logs(PAGE_SIZE, offset).await {
        Ok(l) => l,
        Err(e) => {
            return Html(layout(
                "Maintenance Log",
                &format!(r#"<p class="error">{}</p>"#, esc(&e.to_string())),
            ))
        }
    };

    let mut rows_html = String::new();
    for log in &logs {
        let _id = log
            .id
            .as_ref()
            .map(record_id_to_string)
            .unwrap_or_default();
        let action_badge = match log.action.as_str() {
            "merge" => r#"<span class="badge" style="background:#1a3d2a;color:#3fb950;">merge</span>"#,
            "split" => r#"<span class="badge" style="background:#3d2f1a;color:#d29922;">split</span>"#,
            _ => r#"<span class="badge">unknown</span>"#,
        };

        let target_links: Vec<String> = log.target_ids.iter().map(|tid| {
            let encoded = tid.replace(':', "%3A");
            format!(r#"<a class="link" href="/debug/clusters/{encoded}">{}</a>"#, esc(tid))
        }).collect();
        let targets = target_links.join(", ");

        let timestamp = log
            .created_at
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_default();

        rows_html.push_str(&format!(
            r#"<tr>
<td>{action_badge}</td>
<td>{}</td>
<td>{targets}</td>
<td>{}</td>
<td>{timestamp}</td>
</tr>"#,
            esc(&log.source_id),
            log.members_moved,
        ));
    }

    let total_pages = total.div_ceil(PAGE_SIZE);
    let pagination = if total_pages > 1 {
        let prev = if page > 1 {
            format!(r#"<a class="link" href="/debug/maintenance?page={}">← Prev</a>"#, page - 1)
        } else {
            String::new()
        };
        let next = if page < total_pages {
            format!(r#"<a class="link" href="/debug/maintenance?page={}">Next →</a>"#, page + 1)
        } else {
            String::new()
        };
        format!(
            r#"<div class="pagination">{prev} <span>Page {page} of {total_pages} ({total} entries)</span> {next}</div>"#
        )
    } else {
        format!(r#"<p>{total} entries</p>"#)
    };

    let body = format!(
        r#"<h1>Maintenance Log</h1>
<table>
<tr><th>Action</th><th>Source</th><th>Target(s)</th><th>Members Moved</th><th>Time</th></tr>
{rows_html}
</table>
{pagination}"#
    );

    Html(layout("Maintenance Log", &body))
}

#[cfg(test)]
mod tests {
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
        let c1 = cluster_repo.create(Some("keep"), &[1.0, 0.0]).await.unwrap();
        let c2 = cluster_repo.create(Some("remove"), &[0.98, 0.02]).await.unwrap();
        let f1 = memory_repo.create_fact("fact1", 0.5, &[1.0, 0.0], &[]).await.unwrap();
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
}
