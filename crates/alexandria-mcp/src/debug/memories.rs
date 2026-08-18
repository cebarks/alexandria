use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::response::Html;

use super::html::{esc, layout};
use crate::server::record_id_to_string;
use crate::AlexandriaServer;

pub async fn list(
    State(server): State<AlexandriaServer>,
    Query(params): Query<HashMap<String, String>>,
) -> Html<String> {
    let search = params.get("search").filter(|s| !s.is_empty());
    let tag = params.get("tag").filter(|s| !s.is_empty());
    let include_deleted = params
        .get("include_deleted")
        .map(|v| v == "true")
        .unwrap_or(false);
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let offset: usize = params
        .get("offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
    let rows = match repo
        .list(
            search.map(|s| s.as_str()),
            tag.map(|s| s.as_str()),
            include_deleted,
            limit,
            offset,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Html(layout(
                "Memories",
                &format!(r#"<p class="error">{}</p>"#, esc(&e.to_string())),
            ))
        }
    };

    let mut rows_html = String::new();
    for fact in &rows {
        let id = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
        let tags = fact
            .tags
            .iter()
            .map(|t| format!(r#"<span class="badge">{}</span>"#, esc(t)))
            .collect::<String>();
        let content_preview: String = fact.content.chars().take(120).collect();
        rows_html.push_str(&format!(
            r#"<tr><td><a class="link" href="/debug/memories/{id}">{id}</a></td><td>{}</td><td>{}</td><td>{:.2}</td><td>{}</td></tr>"#,
            esc(&content_preview),
            tags,
            fact.confidence,
            fact.deleted,
        ));
    }

    let body = format!(
        r##"<h1>Memories</h1>
<form hx-get="/debug/memories" hx-target="#memory-results" hx-trigger="input changed delay:300ms from:input, change from:select">
<input type="text" name="search" placeholder="search content..." value="{}">
<input type="text" name="tag" placeholder="tag" value="{}">
<label><input type="checkbox" name="include_deleted" value="true" {}> include deleted</label>
</form>
<div id="memory-results">
<table>
<tr><th>ID</th><th>Content</th><th>Tags</th><th>Confidence</th><th>Deleted</th></tr>
{rows_html}
</table>
<p>{} results</p>
</div>"##,
        esc(search.map(|s| s.as_str()).unwrap_or("")),
        esc(tag.map(|s| s.as_str()).unwrap_or("")),
        if include_deleted { "checked" } else { "" },
        rows.len(),
    );

    Html(layout("Memories", &body))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_memories_list_shows_created_fact() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        repo.create_fact(
            "a searchable memory",
            0.5,
            &[0.1, 0.2],
            &["demo".to_string()],
        )
        .await
        .unwrap();

        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/memories")
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
        assert!(text.contains("a searchable memory"));
        assert!(text.contains("demo"));
    }

    #[tokio::test]
    async fn test_memories_search_query_param_filters() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        repo.create_fact("apple pie", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        repo.create_fact("banana bread", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/memories?search=apple")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("apple pie"));
        assert!(!text.contains("banana bread"));
    }
}
