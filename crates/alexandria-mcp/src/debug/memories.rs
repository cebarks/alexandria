use std::collections::HashMap;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::html::{error_page, page};
use crate::AlexandriaServer;
use crate::server::record_id_to_string;

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
        parts.push(format!(
            "search={}",
            s.replace('&', "%26").replace(' ', "+")
        ));
    }
    if let Some(t) = tag {
        parts.push(format!("tag={}", t.replace('&', "%26").replace(' ', "+")));
    }
    if include_deleted {
        parts.push("include_deleted=true".to_string());
    }
    format!("/debug/memories?{}", parts.join("&"))
}

/// One row of the memories table, flattened out of `Fact` so the template never has to deal
/// with `Option<RecordId>`, with content truncation, or with how a float / timestamp renders.
struct MemoryRow {
    id: String,
    /// The 120-char preview, with the trailing `…` already appended if it was truncated.
    preview: String,
    tags: Vec<String>,
    confidence: String,
    created: String,
    deleted: bool,
}

/// `prev_href` / `next_href` / `summary` feed `templates/_pagination.html`'s `pager` macro,
/// which is presentational: this page paginates by offset rather than by page number, so the
/// full hrefs are built here via [`memories_url`], which is what carries the search / tag /
/// include_deleted filters through the hop. An empty href means no link on that side.
#[derive(Template)]
#[template(path = "memories.html")]
struct MemoriesTemplate {
    nav: &'static str,
    search: String,
    tag: String,
    include_deleted: bool,
    rows: Vec<MemoryRow>,
    prev_href: String,
    next_href: String,
    summary: String,
}

pub async fn list(
    State(server): State<AlexandriaServer>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
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
        Err(e) => return error_page("memories", &e.to_string()),
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

    let rows_view = rows
        .iter()
        .map(|fact| {
            // Add "…" if content was truncated (single-pass: peek 121st char to detect overflow)
            let preview = {
                let mut chars = fact.content.chars();
                let taken: String = chars.by_ref().take(120).collect();
                if chars.next().is_some() {
                    format!("{taken}…")
                } else {
                    taken
                }
            };

            // Format created_at as "YYYY-MM-DD HH:MM UTC", fallback to "—"
            let created = fact
                .created_at
                .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "—".to_string());

            MemoryRow {
                id: fact
                    .id
                    .as_ref()
                    .map(record_id_to_string)
                    .unwrap_or_default(),
                preview,
                tags: fact.tags.clone(),
                confidence: format!("{:.2}", fact.confidence),
                created,
                // Deleted rows get a CSS class for dimming
                deleted: fact.deleted,
            }
        })
        .collect();

    let showing_from = if rows.is_empty() { 0 } else { offset + 1 };
    let showing_to = offset + rows.len();
    let summary = format!("Showing {showing_from}\u{2013}{showing_to} of {total} memories");

    let prev_href = if offset > 0 {
        memories_url(
            search.map(|s| s.as_str()),
            tag.map(|s| s.as_str()),
            include_deleted,
            limit,
            offset.saturating_sub(limit),
        )
    } else {
        String::new()
    };
    let next_href = if offset + rows.len() < total {
        memories_url(
            search.map(|s| s.as_str()),
            tag.map(|s| s.as_str()),
            include_deleted,
            limit,
            offset + limit,
        )
    } else {
        String::new()
    };

    page(MemoriesTemplate {
        nav: "memories",
        search: search.cloned().unwrap_or_default(),
        tag: tag.cloned().unwrap_or_default(),
        include_deleted,
        rows: rows_view,
        prev_href,
        next_href,
        summary,
    })
}

/// The heat block, pre-formatted. `{:.3}` has to happen in Rust because askama renders an
/// `f64` with plain `Display`, which would print `1.5` where the page wants `1.500`.
struct HeatView {
    heat: String,
    stability: String,
    access_count: i64,
    last_touched: String,
}

/// The cluster a fact belongs to, rendered as a link plus an id badge.
struct ClusterView {
    label: String,
    id: String,
}

/// One incident edge. `in_id` / `out_id` are raw record ids: the template urlencodes them for
/// the href and prints them as-is for the link text.
struct EdgeView {
    edge_type: String,
    in_id: String,
    out_id: String,
    strength: String,
}

#[derive(Template)]
#[template(path = "memory_detail.html")]
struct MemoryDetailTemplate {
    nav: &'static str,
    id: String,
    deleted: bool,
    content: String,
    tags: Vec<String>,
    confidence: String,
    created_at: String,
    heat: Option<HeatView>,
    cluster: Option<ClusterView>,
    edges: Vec<EdgeView>,
    /// Pretty-printed (`{:#?}`) metadata JSON, already rendered to text; `None` prints "None".
    metadata: Option<String>,
}

pub async fn detail(State(server): State<AlexandriaServer>, Path(id): Path<String>) -> Response {
    let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
    let fact = match repo.get_fact(&id).await {
        Ok(Some(f)) => f,
        Ok(None) => {
            // The detail page has always answered a missing id with 404, so keep the status
            // rather than taking `error_page`'s 200.
            let mut response = error_page("memories", "Memory not found.");
            *response.status_mut() = StatusCode::NOT_FOUND;
            return response;
        }
        Err(e) => {
            // Same for a data-layer failure: legacy returned 500 here.
            let mut response = error_page("memories", &e.to_string());
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            return response;
        }
    };

    let heat_repo = alexandria_storage::repos::HeatRepo::new(server.db.inner());
    let heat = heat_repo.get(&id).await.ok().flatten().map(|h| HeatView {
        heat: format!("{:.3}", h.heat),
        stability: format!("{:.3}", h.stability),
        access_count: h.access_count,
        last_touched: h
            .last_touched
            .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "—".to_string()),
    });

    let cluster = repo
        .cluster_for_fact(&id)
        .await
        .ok()
        .flatten()
        .map(|c| ClusterView {
            label: c.label.unwrap_or_else(|| "unlabeled".to_string()),
            id: c.id.as_ref().map(record_id_to_string).unwrap_or_default(),
        });

    let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());
    let edges = edge_repo
        .get_edges_for(&id)
        .await
        .unwrap_or_default()
        .iter()
        .map(|e| EdgeView {
            edge_type: e.edge_type.clone(),
            in_id: e
                .in_node
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default(),
            out_id: e
                .out_node
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default(),
            strength: format!("{:.2}", e.strength),
        })
        .collect();

    let created_at = fact
        .created_at
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "—".to_string());

    page(MemoryDetailTemplate {
        nav: "memories",
        id,
        // Deleted badge is shown prominently next to the heading.
        deleted: fact.deleted,
        content: fact.content,
        tags: fact.tags,
        confidence: format!("{:.2}", fact.confidence),
        created_at,
        heat,
        cluster,
        edges,
        metadata: fact.metadata.as_ref().map(|m| format!("{m:#?}")),
    })
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
        // Security regression test: stored content/tags must render escaped and never reach
        // the response as raw executable HTML. askama auto-escapes every `{{ }}`, so the
        // assertions below are the guard — there is no call site that could forget to escape.
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
            text.contains(r#"<td>&#60;script&#62;alert(1)&#60;/script&#62;</td>"#),
            "content should appear HTML-escaped"
        );
        assert!(
            text.contains(r#"<span class="badge">&#60;img src=x onerror=alert(2)&#62;</span>"#),
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
        assert!(text.contains(
            r#"<pre class="content-block">&#60;script&#62;alert(1)&#60;/script&#62;</pre>"#
        ));
        assert!(
            text.contains(r#"<span class="badge">&#60;img src=x onerror=alert(2)&#62;</span>"#)
        );
    }

    #[tokio::test]
    async fn test_memories_list_shows_total_count_and_pagination() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        // Create 3 facts, request limit=2 so we need pagination
        repo.create_fact("first memory", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        repo.create_fact("second memory", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        repo.create_fact("third memory", 0.5, &[0.5, 0.6], &[])
            .await
            .unwrap();

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
        assert!(
            text.contains("of 3"),
            "expected total count in pagination summary"
        );
        // Should have a Next link (there are more rows)
        assert!(text.contains("Next"), "expected a Next pagination link");
        // Should NOT have a Prev link (we're on page 1)
        assert!(
            !text.contains("Prev"),
            "should not have Prev link on first page"
        );
    }

    #[tokio::test]
    async fn test_memories_list_prev_link_on_second_page() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        repo.create_fact("alpha", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        repo.create_fact("beta", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        repo.create_fact("gamma", 0.5, &[0.5, 0.6], &[])
            .await
            .unwrap();

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
        assert!(
            !text.contains("Next"),
            "should not have Next link on last page"
        );
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
        assert!(
            text.contains("UTC"),
            "expected formatted created_at (with UTC suffix) in list"
        );
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
    async fn test_memory_detail_cluster_is_a_link() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());

        let fact_id = repo
            .create_fact("linked cluster test", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let cluster_id = cluster_repo
            .create(Some("my cluster"), &[0.1, 0.2])
            .await
            .unwrap();
        cluster_repo
            .add_member(&cluster_id, &fact_id)
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let uri = format!("/debug/memories/{}", fact_id.replace(':', "%3A"));
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("/debug/clusters/"),
            "expected cluster to be a link to /debug/clusters/:id"
        );
        assert!(
            text.contains("my cluster"),
            "expected cluster label in link"
        );
    }

    #[tokio::test]
    async fn test_memory_detail_edges_are_links() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());

        let id_a = repo
            .create_fact("edge source", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let id_b = repo
            .create_fact("edge target", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        edge_repo
            .create_edge(&id_a, &id_b, "related", 0.9)
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let uri = format!("/debug/memories/{}", id_a.replace(':', "%3A"));
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("/debug/memories/"),
            "expected edge nodes to be links to memory detail pages"
        );
    }

    #[tokio::test]
    async fn test_memory_detail_shows_metadata_section() {
        // The metadata section header always renders (shows "None" when metadata is null,
        // which is the case for facts created via create_fact which doesn't set metadata).
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let id = repo
            .create_fact("metadata check", 0.5, &[0.1, 0.2], &[])
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
            text.contains("Metadata"),
            "expected Metadata section on detail page"
        );
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
