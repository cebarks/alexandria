use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};

use super::html::{esc, layout};
use crate::server::record_id_to_string;
use crate::AlexandriaServer;

/// Build a URL back to the memories list with the given params.
/// Simple encoding — debug UI only, not production.
fn memories_url(
    search: Option<&str>,
    tag: Option<&str>,
    include_deleted: bool,
    limit: usize,
    offset: usize,
) -> String {
    let mut parts = vec![format!("limit={limit}"), format!("offset={offset}")];
    if let Some(s) = search {
        parts.push(format!("search={}", s.replace('&', "%26").replace(' ', "+")));
    }
    if let Some(t) = tag {
        parts.push(format!("tag={}", t.replace('&', "%26").replace(' ', "+")));
    }
    if include_deleted {
        parts.push("include_deleted=true".to_string());
    }
    format!("/debug/memories?{}", parts.join("&"))
}

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

    let total = match repo
        .count(
            search.map(|s| s.as_str()),
            tag.map(|s| s.as_str()),
            include_deleted,
        )
        .await
    {
        Ok(n) => n,
        Err(_) => rows.len(), // graceful fallback
    };

    let mut rows_html = String::new();
    for fact in &rows {
        let id = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
        let id_esc = esc(&id);
        let tags = fact
            .tags
            .iter()
            .map(|t| format!(r#"<span class="badge">{}</span>"#, esc(t)))
            .collect::<String>();

        // Add "…" if content was truncated (single-pass: peek 121st char to detect overflow)
        let content_preview = {
            let mut chars = fact.content.chars();
            let preview: String = chars.by_ref().take(120).collect();
            if chars.next().is_some() {
                        format!("{}…", preview)
            } else {
                        preview
            }
        };

        // Format created_at as "YYYY-MM-DD HH:MM UTC", fallback to "—"
        let created = fact
            .created_at
            .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "—".to_string());

        // Deleted rows get a CSS class for dimming
        let row_class = if fact.deleted { r#" class="deleted""# } else { "" };

        rows_html.push_str(&format!(
            r#"<tr{row_class}><td><a class="link" href="/debug/memories/{id_esc}">{id_esc}</a></td><td>{}</td><td>{}</td><td>{:.2}</td><td>{created}</td></tr>"#,
            esc(&content_preview),
            tags,
            fact.confidence,
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
<tr><th>ID</th><th>Content</th><th>Tags</th><th>Confidence</th><th>Created</th></tr>
{rows_html}
</table>
<div class="pagination"><span>{summary}</span><span>{prev_link} {next_link}</span></div>
</div>"##,
        esc(search.map(|s| s.as_str()).unwrap_or("")),
        esc(tag.map(|s| s.as_str()).unwrap_or("")),
        if include_deleted { "checked" } else { "" },
        summary = {
            let showing_from = if rows.is_empty() { 0 } else { offset + 1 };
            let showing_to = offset + rows.len();
            format!("Showing {showing_from}\u{2013}{showing_to} of {total} memories")
        },
        prev_link = if offset > 0 {
            let prev_offset = offset.saturating_sub(limit);
            format!(
                r#"<a class="link" href="{}">← Prev</a>"#,
                esc(&memories_url(search.map(|s| s.as_str()), tag.map(|s| s.as_str()), include_deleted, limit, prev_offset))
            )
        } else {
            String::new()
        },
        next_link = if offset + rows.len() < total {
            format!(
                r#"<a class="link" href="{}">Next →</a>"#,
                esc(&memories_url(search.map(|s| s.as_str()), tag.map(|s| s.as_str()), include_deleted, limit, offset + limit))
            )
        } else {
            String::new()
        },
    );

    Html(layout("Memories", &body))
}

pub async fn detail(
    State(server): State<AlexandriaServer>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
    let fact = match repo.get_fact(&id).await {
        Ok(Some(f)) => f,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html(layout("Not Found", "<p>Memory not found.</p>")),
            )
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(layout(
                    "Error",
                    &format!(r#"<p class="error">{}</p>"#, esc(&e.to_string())),
                )),
            )
        }
    };

    let heat_repo = alexandria_storage::repos::HeatRepo::new(server.db.inner());
    let heat = heat_repo.get(&id).await.ok().flatten();
    let heat_html = match &heat {
        Some(h) => {
            let last_touched = h
                .last_touched
                .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "—".to_string());
            format!(
                r#"<dl class="fact-meta">
  <dt>Heat</dt><dd>{:.3}</dd>
  <dt>Stability</dt><dd>{:.3}</dd>
  <dt>Access count</dt><dd>{}</dd>
  <dt>Last touched</dt><dd>{}</dd>
</dl>"#,
                h.heat, h.stability, h.access_count, last_touched
            )
        }
        None => "<p>No heat state recorded.</p>".to_string(),
    };

    let cluster = repo.cluster_for_fact(&id).await.ok().flatten();
    let cluster_html = match &cluster {
        Some(c) => format!(
            "{} ({})",
            esc(c.label.as_deref().unwrap_or("unlabeled")),
            esc(&c.id.as_ref().map(record_id_to_string).unwrap_or_default())
        ),
        None => "none".to_string(),
    };

    let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());
    let edges = edge_repo.get_edges_for(&id).await.unwrap_or_default();
    let edges_html: String = edges
        .iter()
        .map(|e| {
            format!(
                "<li>{} — {} → {} (strength {:.2})</li>",
                esc(&e.edge_type),
                esc(&e.in_node.as_ref().map(record_id_to_string).unwrap_or_default()),
                esc(&e.out_node.as_ref().map(record_id_to_string).unwrap_or_default()),
                e.strength
            )
        })
        .collect();

    let tags: String = fact
        .tags
        .iter()
        .map(|t| format!(r#"<span class="badge">{}</span>"#, esc(t)))
        .collect();

    // Deleted badge — shown prominently if the fact is deleted
    let deleted_badge = if fact.deleted {
        r#"<span class="badge-deleted">⚠ Deleted</span>"#.to_string()
    } else {
        String::new()
    };

    // Formatted created_at
    let created_at_str = fact
        .created_at
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "—".to_string());

    let body = format!(
        r##"<p><a class="link" href="/debug/memories">← Back to memories</a></p>
<h1>Memory {id_esc} {deleted_badge}</h1>
<p><a class="link" href="/debug/graph/{id_esc}">View graph →</a></p>
<pre class="content-block">{content_esc}</pre>
<dl class="fact-meta">
  <dt>Tags</dt><dd>{tags}</dd>
  <dt>Confidence</dt><dd>{confidence:.2}</dd>
  <dt>Created</dt><dd>{created_at}</dd>
</dl>
<h2>Heat</h2>
{heat_section}
<h2>Cluster</h2>
<p>{cluster_html}</p>
<h2>Edges</h2>
<ul>{edges_html}</ul>"##,
        id_esc = esc(&id),
        deleted_badge = deleted_badge,
        content_esc = esc(&fact.content),
        tags = tags,
        confidence = fact.confidence,
        created_at = created_at_str,
        heat_section = heat_html,
        cluster_html = cluster_html,
        edges_html = edges_html,
    );

    (StatusCode::OK, Html(layout("Memory Detail", &body)))
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
    async fn test_memories_list_escapes_xss_payload_in_content_and_tags() {
        // Security regression test: stored content/tags must render through esc() and never
        // reach the response as raw executable HTML.
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        repo.create_fact(
            "<script>alert(1)</script>",
            0.5,
            &[0.1, 0.2],
            &["<img src=x onerror=alert(2)>".to_string()],
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

        assert!(
            !text.contains("<script>alert(1)</script>"),
            "raw <script> tag must not appear unescaped in the response"
        );
        assert!(
            !text.contains("<img src=x onerror=alert(2)>"),
            "raw <img onerror> tag must not appear unescaped in the response"
        );
        assert!(
            text.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "content should appear HTML-escaped"
        );
        assert!(
            text.contains("&lt;img src=x onerror=alert(2)&gt;"),
            "tag should appear HTML-escaped"
        );
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

    #[tokio::test]
    async fn test_memory_detail_shows_content_and_heat() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let heat_repo = alexandria_storage::repos::HeatRepo::new(server.db.inner());
        let id = repo
            .create_fact(
                "detailed memory content",
                0.7,
                &[0.1, 0.2],
                &["x".to_string()],
            )
            .await
            .unwrap();
        heat_repo.create_for_memory(&id, 1.5).await.unwrap();

        let app = crate::debug::router(server);
        let uri = format!("/debug/memories/{}", id.replace(':', "%3A"));
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("detailed memory content"));
    }

    #[tokio::test]
    async fn test_memory_detail_escapes_xss_payload_in_content_and_tags() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let id = repo
            .create_fact(
                "<script>alert(1)</script>",
                0.5,
                &[0.1, 0.2],
                &["<img src=x onerror=alert(2)>".to_string()],
            )
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let uri = format!("/debug/memories/{}", id.replace(':', "%3A"));
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(!text.contains("<script>alert(1)</script>"));
        assert!(!text.contains("<img src=x onerror=alert(2)>"));
        assert!(text.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(text.contains("&lt;img src=x onerror=alert(2)&gt;"));
    }

    #[tokio::test]
    async fn test_memories_list_shows_total_count_and_pagination() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        // Create 3 facts, request limit=2 so we need pagination
        repo.create_fact("first memory", 0.5, &[0.1, 0.2], &[]).await.unwrap();
        repo.create_fact("second memory", 0.5, &[0.3, 0.4], &[]).await.unwrap();
        repo.create_fact("third memory", 0.5, &[0.5, 0.6], &[]).await.unwrap();

        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/memories?limit=2&offset=0")
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
        // Should show a total of 3 (not just the 2 on this page)
        assert!(text.contains("of 3"), "expected total count in pagination summary");
        // Should have a Next link (there are more rows)
        assert!(text.contains("Next"), "expected a Next pagination link");
        // Should NOT have a Prev link (we're on page 1)
        assert!(!text.contains("Prev"), "should not have Prev link on first page");
    }

    #[tokio::test]
    async fn test_memories_list_prev_link_on_second_page() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        repo.create_fact("alpha", 0.5, &[0.1, 0.2], &[]).await.unwrap();
        repo.create_fact("beta", 0.5, &[0.3, 0.4], &[]).await.unwrap();
        repo.create_fact("gamma", 0.5, &[0.5, 0.6], &[]).await.unwrap();

        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/memories?limit=2&offset=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("Prev"), "expected Prev link on second page");
        assert!(!text.contains("Next"), "should not have Next link on last page");
    }

    #[tokio::test]
    async fn test_memories_list_shows_created_at() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        repo.create_fact("timestamped memory", 0.5, &[0.1, 0.2], &[])
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        // created_at is written by SurrealDB as current timestamp — year will be present
        // Check for "UTC" which every formatted created_at includes (format: "YYYY-MM-DD HH:MM UTC")
        assert!(text.contains("UTC"), "expected formatted created_at (with UTC suffix) in list");
        // Table header should include Created
        assert!(text.contains("Created"), "expected Created column header");
    }

    #[tokio::test]
    async fn test_memories_list_deleted_row_has_class() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let id = repo
            .create_fact("to be deleted", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        repo.soft_delete_fact(&id).await.unwrap();

        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/memories?include_deleted=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains(r#"class="deleted""#),
            "expected deleted class on row"
        );
    }

    #[tokio::test]
    async fn test_memory_detail_has_back_link() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let id = repo
            .create_fact("nav test", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let uri = format!("/debug/memories/{}", id.replace(':', "%3A"));
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("/debug/memories"),
            "expected back link to memories list"
        );
        assert!(text.contains("Back"), "expected Back text in link");
    }

    #[tokio::test]
    async fn test_memory_detail_shows_deleted_badge_for_deleted_fact() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let id = repo
            .create_fact("deleted fact", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        repo.soft_delete_fact(&id).await.unwrap();

        let app = crate::debug::router(server);
        let uri = format!("/debug/memories/{}", id.replace(':', "%3A"));
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("Deleted"),
            "expected deleted badge on detail page for deleted fact"
        );
    }

    #[tokio::test]
    async fn test_memory_detail_shows_created_at() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let id = repo
            .create_fact("timestamp check", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let uri = format!("/debug/memories/{}", id.replace(':', "%3A"));
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("Created"), "expected Created label");
        assert!(text.contains("UTC"), "expected UTC timestamp in created_at");
    }

    #[tokio::test]
    async fn test_memory_detail_404_for_missing_id() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/memories/fact%3Anonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }
}
