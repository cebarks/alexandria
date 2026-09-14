use std::collections::HashMap;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::html::{self, error_page, page, unavailable};
use crate::AlexandriaServer;
use crate::server::record_id_to_string;
use alexandria_storage::repos::{DEFAULT_FACT_LIST_LIMIT, FactListQuery, FactSort, SortDir};

/// What the page sorts by when `?sort=` / `?dir=` are absent, empty or unrecognised — i.e. the
/// ordering the memories table had before column sorting existed, so every existing bookmark,
/// test and hand-typed URL keeps working.
const DEFAULT_SORT: FactSort = FactSort::CreatedAt;
const DEFAULT_DIR: SortDir = SortDir::Desc;

/// The wire form of a [`FactSort`] — the value `?sort=` takes and the key the template matches its
/// active column against. One mapping, used by both the encoder ([`memories_url`]) and the parser
/// ([`parse_sort`]), so a header href and a hand-typed URL cannot drift apart.
fn sort_key(sort: FactSort) -> &'static str {
    match sort {
        FactSort::CreatedAt => "created",
        FactSort::Confidence => "confidence",
        FactSort::Content => "content",
        FactSort::Id => "id",
        FactSort::TagCount => "tags",
    }
}

fn dir_key(dir: SortDir) -> &'static str {
    match dir {
        SortDir::Asc => "asc",
        SortDir::Desc => "desc",
    }
}

/// `?sort=` → [`FactSort`] over a **closed allowlist**.
///
/// This match is the only bridge between a caller-controlled string and the query's ORDER BY, and
/// it is total: every arm names a `FactSort` variant and the catch-all yields [`DEFAULT_SORT`], so
/// no input — junk, empty, or `created_at DESC; DELETE fact` — can reach storage as text. The
/// storage layer's own `FactSort::order_expr` selects from a closed set of literals, so with this
/// function in front of it there is no string path into SQL at any depth.
///
/// Matching is exact and lowercase because the only producer of these values is [`sort_key`].
fn parse_sort(raw: Option<&String>) -> FactSort {
    match raw.map(String::as_str) {
        Some("created") => FactSort::CreatedAt,
        Some("confidence") => FactSort::Confidence,
        Some("content") => FactSort::Content,
        Some("id") => FactSort::Id,
        Some("tags") => FactSort::TagCount,
        _ => DEFAULT_SORT,
    }
}

/// `?dir=` → [`SortDir`], same closed-allowlist shape and same total fallback as [`parse_sort`].
fn parse_dir(raw: Option<&String>) -> SortDir {
    match raw.map(String::as_str) {
        Some("asc") => SortDir::Asc,
        Some("desc") => SortDir::Desc,
        _ => DEFAULT_DIR,
    }
}

/// Percent-encodes one query-component value per RFC 3986: everything outside the unreserved set
/// `A-Za-z0-9-_.~` becomes `%XX` (upper-case hex), UTF-8 byte by byte.
///
/// The previous implementation escaped only `&` and turned spaces into `+`, which corrupted every
/// other value: a search containing `%` was reinterpreted as an escape sequence by axum's query
/// decoder (a stray `%zz` is a 400), `#` truncated the URL into a fragment, and a literal `+`
/// round-tripped as a space — so Prev/Next links dropped rows the operator had just filtered for.
/// Encoding all reserved bytes removes the whole class rather than appending to an escape list.
///
/// Space is `%20`, not `+`: `+` means a space only in `application/x-www-form-urlencoded` bodies,
/// while `%20` means a space in every query-component reader, so it is unambiguous in both.
///
/// Hand-rolled rather than via `percent_encoding::utf8_percent_encode` because that crate is not a
/// direct dependency of this one (askama pulls it in transitively) and adding a dependency for eight
/// lines is not worth it.
fn encode_query_value(s: &str) -> String {
    /// Upper-case hex per RFC 3986 §2.1, which also makes the output stable for assertions.
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(*byte));
            }
            _ => {
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    out
}

/// Build a URL back to the memories list with the given params.
///
/// `sort` / `dir` are emitted only when they differ from [`DEFAULT_SORT`] / [`DEFAULT_DIR`], which
/// keeps the ordinary hrefs short *and* keeps every existing assertion on href shape honest: a
/// sorted page's Prev/Next hop preserves the sort, which is the entire point of sorting server-side
/// (a sorted table whose "next page" reverts to newest-first is worse than an unsorted one).
fn memories_url(
    search: Option<&str>,
    tag: Option<&str>,
    include_deleted: bool,
    sort: FactSort,
    dir: SortDir,
    limit: usize,
    offset: usize,
) -> String {
    let mut parts = vec![format!("limit={limit}"), format!("offset={offset}")];
    if let Some(s) = search {
        parts.push(format!("search={}", encode_query_value(s)));
    }
    if let Some(t) = tag {
        parts.push(format!("tag={}", encode_query_value(t)));
    }
    if include_deleted {
        parts.push("include_deleted=true".to_string());
    }
    if sort != DEFAULT_SORT {
        parts.push(format!("sort={}", sort_key(sort)));
    }
    if dir != DEFAULT_DIR {
        parts.push(format!("dir={}", dir_key(dir)));
    }
    format!("/debug/memories?{}", parts.join("&"))
}

/// The five sortable column headers, with every href built in Rust: the template only renders
/// what is in here, because askama cannot call [`memories_url`] and a second URL implementation
/// in markup is exactly the drift the encoder fix is about.
struct SortLinks {
    id: String,
    content: String,
    tags: String,
    confidence: String,
    created: String,
    /// [`sort_key`] of the column the page is currently ordered by, so the template can mark it
    /// without knowing about `FactSort`.
    active: &'static str,
    /// [`dir_key`] of the active column — `asc`/`desc`, the same wire form `?dir=` takes, which is
    /// also what the live-filter form posts back in a hidden input.
    dir: &'static str,
}

/// One header's href: sort by `column`, returning to the first page.
///
/// Clicking the *active* column flips its direction; clicking any other column sorts it
/// **descending**, matching [`DEFAULT_DIR`] and the way users expect a first click on a date column
/// to behave (newest first). Offsets deliberately do not survive a sort change — page 2 of a
/// different ordering would show arbitrary rows.
fn sort_link(
    search: Option<&str>,
    tag: Option<&str>,
    include_deleted: bool,
    limit: usize,
    column: FactSort,
    active: FactSort,
    dir: SortDir,
) -> String {
    let target = if column == active {
        match dir {
            SortDir::Asc => SortDir::Desc,
            SortDir::Desc => SortDir::Asc,
        }
    } else {
        SortDir::Desc
    };
    memories_url(search, tag, include_deleted, column, target, limit, 0)
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
/// include_deleted **and the active sort** through the hop. An empty href means no link on that
/// side.
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
    sorts: SortLinks,
}

pub async fn list(
    State(server): State<AlexandriaServer>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // `Option<&str>` rather than the `Option<&String>` the map hands back, because every consumer
    // here — the repo calls, the pager hrefs, the header hrefs — takes `&str`.
    let search = params
        .get("search")
        .filter(|s| !s.is_empty())
        .map(String::as_str);
    let tag = params
        .get("tag")
        .filter(|s| !s.is_empty())
        .map(String::as_str);
    let include_deleted = params
        .get("include_deleted")
        .map(|v| v == "true")
        .unwrap_or(false);
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_FACT_LIST_LIMIT);
    let offset: usize = params
        .get("offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // Tolerant like `limit`/`offset` above, and for the same reason: a hand-typed or stale
    // `?sort=` is a typo, not a malformed request, and a 400 on a diagnostic page is a silent dead
    // end (htmx will not even render it). Anything unrecognised orders the table the way it did
    // before sorting existed. The strings never get further than `parse_sort`/`parse_dir`.
    let sort = parse_sort(params.get("sort"));
    let dir = parse_dir(params.get("dir"));

    let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
    let rows = match repo
        .list(FactListQuery {
            search,
            tag,
            include_deleted,
            sort,
            dir,
            limit,
            offset,
        })
        .await
    {
        Ok(r) => r,
        Err(e) => return error_page("memories", &e.to_string()),
    };

    let total = match repo.count(search, tag, include_deleted).await {
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

            // Shared timestamp rendering: `html::format_dt` is the only format the debug UI
            // prints, and `—` the only marker it prints for "never".
            let created = html::format_dt(fact.created_at);

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
            search,
            tag,
            include_deleted,
            sort,
            dir,
            limit,
            offset.saturating_sub(limit),
        )
    } else {
        String::new()
    };
    let next_href = if offset + rows.len() < total {
        memories_url(
            search,
            tag,
            include_deleted,
            sort,
            dir,
            limit,
            offset + limit,
        )
    } else {
        String::new()
    };

    // Built after the page's own sort is known, and from the same `memories_url`, so the headers
    // and the pager can never disagree about what the current sort is.
    let sorts = SortLinks {
        id: sort_link(search, tag, include_deleted, limit, FactSort::Id, sort, dir),
        content: sort_link(
            search,
            tag,
            include_deleted,
            limit,
            FactSort::Content,
            sort,
            dir,
        ),
        tags: sort_link(
            search,
            tag,
            include_deleted,
            limit,
            FactSort::TagCount,
            sort,
            dir,
        ),
        confidence: sort_link(
            search,
            tag,
            include_deleted,
            limit,
            FactSort::Confidence,
            sort,
            dir,
        ),
        created: sort_link(
            search,
            tag,
            include_deleted,
            limit,
            FactSort::CreatedAt,
            sort,
            dir,
        ),
        active: sort_key(sort),
        dir: dir_key(dir),
    };

    page(MemoriesTemplate {
        nav: "memories",
        search: search.unwrap_or_default().to_string(),
        tag: tag.unwrap_or_default().to_string(),
        include_deleted,
        rows: rows_view,
        prev_href,
        next_href,
        summary,
        sorts,
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
        // A data-layer failure keeps the 500 this page has always returned, but through the shared
        // helper rather than a hand-inlined `status_mut`.
        Err(e) => return unavailable("memories", "memory", e),
    };

    let heat_repo = alexandria_storage::repos::HeatRepo::new(server.db.inner());
    let heat = heat_repo.get(&id).await.ok().flatten().map(|h| HeatView {
        heat: format!("{:.3}", h.heat),
        stability: format!("{:.3}", h.stability),
        access_count: h.access_count,
        last_touched: html::format_dt(h.last_touched),
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

    let created_at = html::format_dt(fact.created_at);

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
        // Handler-level: these bytes are what `memories::list` formatted, so this is the
        // assertion that fails if the page ever drifts off `html::format_dt`.
        super::super::html::assert_shared_timestamp_cells(&text, "GET /debug/memories", 1);
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

    // ---- column sorting: the handler's `?sort=`/`?dir=`, the encoder, the headers ----

    use super::{
        FactListQuery, FactSort, MemoriesTemplate, MemoryDetailTemplate, MemoryRow, SortDir,
        SortLinks, dir_key, encode_query_value, parse_dir, parse_sort, sort_key, sort_link,
    };
    use askama::Template;

    /// Fetch a debug route and return the body, asserting a 200 on the way. A non-200 fails here
    /// rather than in whichever substring assertion comes next, so the message names the URI.
    async fn body_of(app: &axum::Router, uri: &str) -> String {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "unexpected status for {uri}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    /// Three facts with distinct content, distinct confidence and 0/1/3 tags, created in the order
    /// listed. Every sort key but `created_at` is therefore unambiguous, so a rendered row order
    /// proves the sort ran rather than merely parsed.
    async fn seed_sortable(server: &crate::AlexandriaServer) {
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        repo.create_fact(
            "aaa",
            0.9,
            &[0.1, 0.2],
            &["t".into(), "u".into(), "v".into()],
        )
        .await
        .unwrap();
        repo.create_fact("bbb", 0.1, &[0.3, 0.4], &[])
            .await
            .unwrap();
        repo.create_fact("ccc", 0.5, &[0.5, 0.6], &["t".into()])
            .await
            .unwrap();
    }

    /// Asserts the content previews appear in the given order (by column position in the page).
    /// Position comparison, not set membership: "the page contains all three" would pass on an
    /// unsorted table and prove nothing about the ORDER BY.
    fn assert_rows_in_order(text: &str, ordered: &[&str]) {
        let mut previous = usize::MAX;
        for content in ordered {
            let cell = format!("<td>{content}</td>");
            let at = text
                .find(&cell)
                .unwrap_or_else(|| panic!("missing row cell {cell}; got: {text}"));
            assert!(
                previous == usize::MAX || at > previous,
                "rows out of order: {content:?} sits at {at}, not after the previous row at                  {previous}; got: {text}"
            );
            previous = at;
        }
    }

    /// The whole rendered header row, so a failure prints the actual hrefs and `aria-sort`s.
    /// The header row is the only `<tr>` whose first cell is a `<th>`.
    fn header_row(text: &str) -> String {
        let start = text
            .find("<tr><th")
            .unwrap_or_else(|| panic!("no header row in: {text}"));
        text[start..].split("</tr>").next().unwrap().to_string() + "</tr>"
    }

    /// The value of the `href="..."` immediately preceding `anchor`, turned back into a wire URI.
    ///
    /// askama escapes the `&` separators to `&#38;`, which is correct inside an attribute and which
    /// a browser decodes before it sends the request — so an assertion that only looks at the Rust
    /// string would miss a broken *link*. Extracting and following the rendered href is what proves
    /// the encoder fix end to end.
    fn href_before(text: &str, anchor: &str) -> String {
        let at = text
            .find(anchor)
            .unwrap_or_else(|| panic!("no {anchor:?} in: {text}"));
        let open = text[..at]
            .rfind("href=\"")
            .unwrap_or_else(|| panic!("no href before {anchor:?} in: {text}"))
            + 6;
        let value = &text[open..at];
        let value = value.strip_suffix('"').unwrap_or(value);
        value.replace("&#38;", "&")
    }

    #[tokio::test]
    async fn test_sort_orders_rows_by_the_requested_column() {
        let server = super::super::test_support::test_server().await;
        seed_sortable(&server).await;
        let app = crate::debug::router(server);

        // confidence: 0.1 / 0.5 / 0.9 are distinct, so no tie-break is doing this work.
        let asc = body_of(&app, "/debug/memories?sort=confidence&dir=asc").await;
        assert_rows_in_order(&asc, &["bbb", "ccc", "aaa"]);
        let desc = body_of(&app, "/debug/memories?sort=confidence&dir=desc").await;
        assert_rows_in_order(&desc, &["aaa", "ccc", "bbb"]);

        // content: alphabetical.
        let content = body_of(&app, "/debug/memories?sort=content&dir=asc").await;
        assert_rows_in_order(&content, &["aaa", "bbb", "ccc"]);

        // tags: 3 / 0 / 1 — the count, which is why the header says "Tags (count)".
        let tags = body_of(&app, "/debug/memories?sort=tags&dir=asc").await;
        assert_rows_in_order(&tags, &["bbb", "ccc", "aaa"]);

        // id: unique, so the two directions are exact reversals of each other.
        let id_asc = body_of(&app, "/debug/memories?sort=id&dir=asc").await;
        let id_desc = body_of(&app, "/debug/memories?sort=id&dir=desc").await;
        let seq = |page: &str| {
            let mut pairs = [
                ("aaa", page.find("<td>aaa</td>").unwrap()),
                ("bbb", page.find("<td>bbb</td>").unwrap()),
                ("ccc", page.find("<td>ccc</td>").unwrap()),
            ];
            pairs.sort_by_key(|(_, at)| *at);
            pairs
                .iter()
                .map(|(c, _)| c.to_string())
                .collect::<Vec<_>>()
                .join("")
        };
        let a = seq(&id_asc);
        let b = seq(&id_desc);
        assert_eq!(
            a.chars().rev().collect::<String>(),
            b,
            "id ascending and descending must be exact reverses; got {a} / {b}"
        );
    }

    #[tokio::test]
    async fn test_junk_sort_params_behave_exactly_like_absent_ones() {
        // The tolerance guarantee, stated as equality: a junk param does not render "some" default
        // page, it renders *the same bytes* as the absent one, and a valid sort paired with a junk
        // direction renders the same bytes as that sort with its direction spelled out. Anything
        // else would mean the fallback depends on what the junk looked like.
        let server = super::super::test_support::test_server().await;
        seed_sortable(&server).await;
        let app = crate::debug::router(server);

        let plain = body_of(&app, "/debug/memories").await;
        for uri in [
            "/debug/memories?sort=&dir=",
            "/debug/memories?sort=bogus&dir=bogus",
            "/debug/memories?sort=CONFIDENCE&dir=ASC",
            "/debug/memories?sort=created_at&dir=desc",
        ] {
            assert_eq!(
                body_of(&app, uri).await,
                plain,
                "{uri} must render the default page verbatim"
            );
        }

        let by_confidence = body_of(&app, "/debug/memories?sort=confidence&dir=desc").await;
        // Junk alongside a recognised sort keeps the sort and defaults the direction — and the
        // reference page differs from the default one, so these three assertions are not vacuous.
        assert_ne!(
            by_confidence, plain,
            "fixture: ?sort=confidence must actually reorder the table"
        );
        for uri in [
            "/debug/memories?sort=confidence",
            "/debug/memories?sort=confidence&dir=",
            "/debug/memories?sort=confidence&dir=sideways",
        ] {
            assert_eq!(
                body_of(&app, uri).await,
                by_confidence,
                "{uri} must equal ?sort=confidence&dir=desc"
            );
        }
    }

    #[tokio::test]
    async fn test_sort_injection_attempts_fall_back_to_the_default_page() {
        // Security-critical. The defence is closed-set in two places: `parse_sort`/`parse_dir` here,
        // and `FactSort::order_expr` in storage, which selects from `&'static str` literals. What
        // this test pins is the *observable* half: the payload is refused (default page, 200) and
        // inert (the data is untouched afterwards), not merely tolerated.
        let server = super::super::test_support::test_server().await;
        seed_sortable(&server).await;
        // The router takes the server by value, so it gets a clone: this test reads the table back
        // through `server` afterwards to prove the payload was inert rather than merely refused.
        let app = crate::debug::router(server.clone());
        let plain = body_of(&app, "/debug/memories").await;

        for uri in [
            "/debug/memories?sort=created_at%20DESC%3B%20DELETE%20fact",
            "/debug/memories?sort=id--",
            "/debug/memories?sort=",
            "/debug/memories?sort=../../etc/passwd",
            "/debug/memories?dir=asc%3B%20DROP",
            "/debug/memories?sort=%27%27%2C%28SELECT%201%29--",
        ] {
            let page = body_of(&app, uri).await;
            assert_eq!(
                page, plain,
                "{uri} must fall back to the default page verbatim"
            );
            // The default active column is Created, descending — the fallback is visible, not just
            // implied by a 200.
            let headers = header_row(&page);
            assert_eq!(
                headers.matches("aria-sort").count(),
                1,
                "exactly one active column expected; got: {headers}"
            );
            assert!(
                headers.contains(r#"<th aria-sort="descending">"#),
                "the fallback must mark Created descending active; got: {headers}"
            );
            // The active header spells out its own URL — `?limit=50&offset=0&dir=asc`, with no
            // `sort=` because CreatedAt *is* the default. That is the visible proof the payload was
            // mapped onto the default column rather than onto a real one it would have to name.
            assert!(
                headers.contains(
                    "<th aria-sort=\"descending\"><a class=\"link\" href=\"/debug/memories?limit=50&#38;offset=0&#38;dir=asc\">Created \u{25bc}</a></th>"
                ),
                "the fallback must leave Created active, linking to ascending; got: {headers}"
            );
        }

        // Inert, not merely unparseable: the DELETE / DROP payloads ran, the three facts are still
        // there, with their content and confidence unchanged.
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let left = repo
            .list(FactListQuery {
                include_deleted: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            left.len(),
            3,
            "injection payload removed or altered rows: {left:?}"
        );
        let mut contents: Vec<&str> = left.iter().map(|f| f.content.as_str()).collect();
        contents.sort_unstable();
        assert_eq!(contents, vec!["aaa", "bbb", "ccc"]);
    }

    #[test]
    fn test_sort_wire_keys_round_trip_through_the_allowlist() {
        // `sort_key`/`dir_key` build the URLs and `parse_sort`/`parse_dir` read them. If the two
        // maps drift, every header link silently sorts by the wrong column, so this pins agreement
        // over the whole closed set rather than at one hand-picked value.
        for sort in [
            FactSort::CreatedAt,
            FactSort::Confidence,
            FactSort::Content,
            FactSort::Id,
            FactSort::TagCount,
        ] {
            let key = sort_key(sort);
            assert_eq!(
                parse_sort(Some(&key.to_string())),
                sort,
                "{key:?} must parse back to {sort:?}"
            );
        }
        for dir in [SortDir::Asc, SortDir::Desc] {
            let key = dir_key(dir);
            assert_eq!(
                parse_dir(Some(&key.to_string())),
                dir,
                "{key:?} must parse back to {dir:?}"
            );
        }
        // And anything else is the default — the catch-all arm is a total function, not a filter.
        for junk in ["", " ", "created_at", "CREATED", "id ASC", "tags)", "~"] {
            assert_eq!(
                parse_sort(Some(&junk.to_string())),
                FactSort::CreatedAt,
                "{junk:?} must fall back"
            );
        }
        assert_eq!(parse_sort(None), FactSort::CreatedAt);
        assert_eq!(parse_dir(None), SortDir::Desc);
        assert_eq!(parse_dir(Some(&"asc; DROP".to_string())), SortDir::Desc);
    }

    #[test]
    fn test_encode_query_value_percent_encodes_every_reserved_byte() {
        // The table is the unit half of the proof; `test_search_href_with_reserved_chars_round_trips`
        // is the end-to-end half. Neither alone is enough: the table cannot show that a browser (and
        // axum's decoder) agree on the encoding, and the round trip cannot show that `:` or `/` or a
        // multi-byte char are covered, because no test URI happens to contain them.
        for (input, want) in [
            ("%", "%25"),
            ("#", "%23"),
            ("+", "%2B"),
            ("&", "%26"),
            ("=", "%3D"),
            (" ", "%20"),
            (":", "%3A"),
            ("/", "%2F"),
            ("?", "%3F"),
            ("é", "%C3%A9"),
            ("<>", "%3C%3E"),
            ("'", "%27"),
            ("a b%c#d+e&f=g", "a%20b%25c%23d%2Be%26f%3Dg"),
            // The RFC 3986 unreserved set passes through untouched.
            ("AZaz09-_.~", "AZaz09-_.~"),
        ] {
            assert_eq!(encode_query_value(input), want, "encoding {input:?}");
        }
    }

    #[tokio::test]
    async fn test_search_href_with_reserved_chars_round_trips() {
        // The end-to-end proof the encoder fix needed: a search containing `%`, `#`, `+` and a
        // space must survive *as a link* — build the page, lift the Next href out of the rendered
        // HTML, follow it, and get the same two rows back. The old `&`-and-space-only escaping
        // produced an href that 400'd (`%`), truncated into a fragment (`#`) or decoded to a space
        // (`+`), so this fails against it in three separate ways.
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        for content in [
            "alpha 50% #hash plus+ sign",
            "beta 50% #hash plus+ sign",
            "nothing to see",
        ] {
            repo.create_fact(content, 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
        }
        let app = crate::debug::router(server);
        let needle = "50% #hash plus+ sign";
        let first = body_of(
            &app,
            &format!(
                "/debug/memories?limit=1&search={}",
                encode_query_value(needle)
            ),
        )
        .await;
        assert!(
            first.contains("of 2 memories"),
            "the search must match exactly the two payload rows, not all three; got: {first}"
        );
        assert!(
            first.contains("50% #hash plus+ sign"),
            "page 1 must show a matching row; got: {first}"
        );
        assert!(
            !first.contains("nothing to see"),
            "the filter must still exclude the non-matching row; got: {first}"
        );

        // Lift the Next href out of the pager and follow it as a browser would.
        let wire = href_before(&first, ">Next \u{2192}");
        assert!(
            !wire.contains(' ')
                && wire.contains("%25")
                && wire.contains("%23")
                && wire.contains("%2B"),
            "the href must carry a fully percent-encoded search; got: {wire}"
        );

        let second = body_of(&app, &wire).await;
        assert!(
            second.contains("of 2 memories"),
            "following the encoded href must re-apply the same filter (total 2); got: {second}"
        );
        assert!(
            !second.contains("nothing to see"),
            "the followed page must still be filtered; got: {second}"
        );
        // Exactly one row remains on the second page, and it is one of the two matches.
        assert!(
            second.contains("50% #hash plus+ sign"),
            "the same fact must come back; got: {second}"
        );
        assert!(
            second.contains(r#"value="50% #hash plus+ sign""#),
            "the search box must echo the decoded query; got: {second}"
        );
    }

    #[tokio::test]
    async fn test_pagination_and_headers_carry_the_active_sort() {
        let server = super::super::test_support::test_server().await;
        seed_sortable(&server).await;
        let app = crate::debug::router(server);

        let page = body_of(
            &app,
            "/debug/memories?limit=2&offset=0&sort=confidence&dir=asc",
        )
        .await;
        // Pagination must not drop the sort — the whole point of sorting server-side is that page 2
        // continues the same ordering.
        assert!(
            page.contains(
                r##"href="/debug/memories?limit=2&#38;offset=2&#38;sort=confidence&#38;dir=asc">Next →"##
            ),
            "Next must preserve sort and dir; got: {}",
            header_row(&page)
        );
        let headers = header_row(&page);
        // Active column: confidence, ascending, with its own link offering the flip.
        assert!(
            headers.contains(r#"<th aria-sort="ascending"><a class="link" href="/debug/memories?limit=2&#38;offset=0&#38;sort=confidence">Confidence ▲</a></th>"#),
            "the active header must link to the flipped direction; got: {headers}"
        );
        // An inactive column links to itself, descending (the first-click convention).
        assert!(
            headers.contains(r#"<th><a class="link" href="/debug/memories?limit=2&#38;offset=0&#38;sort=content">Content</a></th>"#),
            "an inactive header must link to that column descending; got: {headers}"
        );
        // The default column/direction pair emits no `sort`/`dir` at all, keeping ordinary URLs
        // short and the existing href assertions valid.
        assert!(
            headers.contains(r#"<th><a class="link" href="/debug/memories?limit=2&#38;offset=0">Created</a></th>"#),
            "a link back to the defaults must not spell them out; got: {headers}"
        );
        assert_eq!(
            headers.matches("aria-sort").count(),
            1,
            "exactly one active column; got: {headers}"
        );
        assert!(
            headers.contains("Tags (count)"),
            "the tags header must name the count; got: {headers}"
        );
        // And the live-filter form carries the sort through the htmx hop.
        assert!(
            page.contains(r#"<input type="hidden" name="sort" value="confidence">"#),
            "the filter form must post the active sort; got: {page}"
        );
        assert!(
            page.contains(r#"<input type="hidden" name="dir" value="asc">"#),
            "the filter form must post the active direction; got: {page}"
        );
    }

    #[tokio::test]
    async fn test_flipping_the_active_column_and_switching_columns() {
        let server = super::super::test_support::test_server().await;
        seed_sortable(&server).await;
        let app = crate::debug::router(server);

        // Descending confidence: its header offers ascending.
        let page = body_of(&app, "/debug/memories?limit=20&sort=confidence&dir=desc").await;
        let headers = header_row(&page);
        assert!(
            headers.contains(r#"<th aria-sort="descending"><a class="link" href="/debug/memories?limit=20&#38;offset=0&#38;sort=confidence&#38;dir=asc">Confidence ▼</a></th>"#),
            "the active header must offer asc; got: {headers}"
        );

        // Follow the tags header from that page: it must land on tags/descending, and the resulting
        // page must mark *tags* active.
        let href = href_before(&headers, ">Tags (count)<");
        assert_eq!(
            href, "/debug/memories?limit=20&offset=0&sort=tags",
            "a first click on a new column sorts it descending"
        );
        let followed = body_of(&app, &href).await;
        let followed_headers = header_row(&followed);
        assert!(
            followed_headers
            .contains(r#"<th aria-sort="descending"><a class="link" href="/debug/memories?limit=20&#38;offset=0&#38;sort=tags&#38;dir=asc">Tags (count) ▼</a></th>"#),
            "following the tags header must make tags the active column; got: {followed_headers}"
        );
        // Tag counts are 3 / 0 / 1, so descending is aaa, ccc, bbb.
        assert_rows_in_order(&followed, &["aaa", "ccc", "bbb"]);
    }

    #[test]
    fn test_sort_link_resets_the_offset_and_toggles() {
        // Direct over the builder, so the "a sort change returns to page 1" rule is pinned even
        // where no page happens to expose it.
        let link = sort_link(
            Some("hello world"),
            None,
            true,
            20,
            FactSort::Confidence,
            FactSort::Confidence,
            SortDir::Asc,
        );
        assert_eq!(
            link,
            "/debug/memories?limit=20&offset=0&search=hello%20world&include_deleted=true&sort=confidence",
            "the active column flips to descending and the offset resets"
        );
        let other = sort_link(
            None,
            Some("a&b"),
            false,
            20,
            FactSort::TagCount,
            FactSort::Id,
            SortDir::Desc,
        );
        assert_eq!(
            other, "/debug/memories?limit=20&offset=0&tag=a%26b&sort=tags",
            "an inactive column sorts descending; the default direction is not spelled out"
        );
    }

    /// A `SortLinks` with no URL building at all: these hrefs are opaque strings to the template,
    /// which is the point — the render assertions below cannot be satisfied by the Rust side and
    /// vice versa.
    fn links(active: &'static str, dir: &'static str) -> SortLinks {
        SortLinks {
            id: "H-id".into(),
            content: "H-content".into(),
            tags: "H-tags".into(),
            confidence: "H-confidence".into(),
            created: "H-created".into(),
            active,
            dir,
        }
    }

    /// The shared timestamp contract, asserted on this page's markup rather than on
    /// `html::format_dt` itself: a page that grew a private formatter would still pass the unit
    /// test, which is why `test_memories_list_shows_created_at` asserts the same shape on bytes the
    /// handler produced. This test's job is the template: both markers, in the right cell. `/debug/memories` is both the list cell and the detail cell, because the list used to
    /// build its string inline (`map(|dt| …).unwrap_or_else(|| "—".to_string())`) and the detail
    /// page repeated that inline block twice (created_at, heat.last_touched).
    #[test]
    fn test_memories_pages_use_the_shared_timestamp_format_and_absent_marker() {
        use crate::debug::html;

        let with_time = memories_template(links("created", "desc"));
        let with_time = {
            let mut tpl = with_time;
            tpl.rows[0].created = html::format_dt(Some(html::example_dt()));
            tpl
        }
        .render()
        .unwrap();
        assert!(
            with_time.contains("<td>2026-07-05 11:50 UTC</td>"),
            "the shared minute form must appear verbatim in the list cell; got: {with_time}"
        );
        html::assert_no_seconds_timestamp(&with_time, "memories.html");

        let mut absent = memories_template(links("created", "desc"));
        absent.rows[0].created = html::format_dt(None);
        let absent = absent.render().unwrap();
        assert!(
            absent.contains(&format!("<td>{}</td>", html::ABSENT)),
            "an absent created_at must render the shared marker, not an empty cell; got: {absent}"
        );
        assert!(
            !absent.contains("<td></td>"),
            "the list must not emit an empty cell; got: {absent}"
        );

        // The detail page: same two markers, same helper.
        let detail = MemoryDetailTemplate {
            nav: "memories",
            id: "fact:abc".into(),
            deleted: false,
            content: "a body".into(),
            tags: vec![],
            confidence: "0.50".into(),
            created_at: html::format_dt(Some(html::example_dt())),
            heat: None,
            cluster: None,
            edges: vec![],
            metadata: None,
        }
        .render()
        .unwrap();
        assert!(detail.contains("2026-07-05 11:50 UTC"), "got: {detail}");
        html::assert_no_seconds_timestamp(&detail, "memory_detail.html");
    }

    fn memories_template(sorts: SortLinks) -> MemoriesTemplate {
        MemoriesTemplate {
            nav: "memories",
            search: String::new(),
            tag: String::new(),
            include_deleted: false,
            rows: vec![MemoryRow {
                id: "fact:abc".into(),
                preview: "a preview".into(),
                tags: vec!["t".into()],
                confidence: "0.50".into(),
                created: "2026-09-11 09:00 UTC".into(),
                deleted: false,
            }],
            prev_href: String::new(),
            next_href: String::new(),
            summary: "Showing 1\u{2013}1 of 1 memories".into(),
            sorts,
        }
    }

    #[test]
    fn test_memories_template_renders_one_active_header_and_tags_count() {
        let html = memories_template(links("tags", "asc")).render().unwrap();
        let headers = header_row(&html);

        // The label is deliberately "Tags (count)": the column orders by number of tags, and a bare
        // "Tags" would imply an alphabetical ordering that does not exist.
        assert!(headers.contains("Tags (count)"), "got: {headers}");
        assert!(
            !headers.contains(r#"<th><a class="link" href="H-tags">Tags</a></th>"#),
            "the bare \"Tags\" label must not come back; got: {headers}"
        );
        assert_eq!(
            html.matches("aria-sort").count(),
            1,
            "exactly one column may claim a sort; got: {headers}"
        );
        assert!(
            headers.contains(
                r#"<th aria-sort="ascending"><a class="link" href="H-tags">Tags (count) ▲</a></th>"#
            ),
            "got: {headers}"
        );
        // Inactive headers: no attribute, no marker.
        for (href, label) in [
            ("H-id", "ID"),
            ("H-content", "Content"),
            ("H-confidence", "Confidence"),
            ("H-created", "Created"),
        ] {
            assert!(
                headers.contains(&format!(
                    r#"<th><a class="link" href="{href}">{label}</a></th>"#
                )),
                "inactive header {label} must render plain; got: {headers}"
            );
        }
        // The hidden inputs echo the wire forms, so an htmx filter keeps the sort.
        assert!(
            html.contains(r#"<input type="hidden" name="sort" value="tags">"#),
            "got: {html}"
        );
        assert!(
            html.contains(r#"<input type="hidden" name="dir" value="asc">"#),
            "got: {html}"
        );
    }

    #[test]
    fn test_memories_template_marks_only_the_default_column_active() {
        let html = memories_template(links("created", "desc"))
            .render()
            .unwrap();
        let headers = header_row(&html);
        assert_eq!(html.matches("aria-sort").count(), 1, "got: {headers}");
        assert!(
            headers.contains(
                r#"<th aria-sort="descending"><a class="link" href="H-created">Created ▼</a></th>"#
            ),
            "got: {headers}"
        );
        // Nothing resembling an arrow on the inactive columns.
        assert_eq!(
            headers.matches('▲').count() + headers.matches('▼').count(),
            1,
            "got: {headers}"
        );
    }

    /// Storage failure ⇒ `html::UNAVAILABLE_STATUS`, asserted on bytes the handler produced.
    ///
    /// Forced by handing the handler a database that was never migrated: the first repo call then
    /// fails for real, with no mock and no change to `alexandria-storage`. The body assertion matters
    /// as much as the status one — it is what proves this page goes through `html::unavailable`
    /// rather than re-inlining `error_page` plus a `status_mut` (the old `memories.rs` shape), which
    /// would keep the status identical and only change the wording.
    #[tokio::test]
    async fn test_memory_storage_failure_answers_with_the_one_status() {
        let db = alexandria_storage::Database::connect_embedded()
            .await
            .unwrap();
        let server = crate::AlexandriaServer::new(
            std::sync::Arc::new(db),
            std::sync::Arc::new(super::super::test_support::StubEmbedding),
            0.75,
            86400.0,
        );
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/memories/fact:absent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // The literal, not `html::UNAVAILABLE_STATUS`: asserting against the constant would pass no
        // matter what status it names, which is the opposite of pinning one.
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "a storage failure must answer with the one detail-page status"
        );
        assert_eq!(
            response.status(),
            super::super::html::UNAVAILABLE_STATUS,
            "…and the shared constant must still name it"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("storage error while loading memory"),
            "the page must name what failed, via the shared helper; got: {text}"
        );
    }
}
