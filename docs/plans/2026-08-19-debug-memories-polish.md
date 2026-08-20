# Debug Memories Polish Implementation Plan

> **REQUIRED SUB-SKILL:** Use the executing-plans skill to implement this plan task-by-task.

**Goal:** Polish the `/debug/memories` list and detail pages — fix missing total count + pagination UI,
add timestamps, improve visual hierarchy, and make cluster/edge references navigable links.

**Architecture:** All changes are confined to `crates/alexandria-mcp/src/debug/memories.rs` and
`crates/alexandria-mcp/src/debug/html.rs`. No new storage methods needed — `MemoryRepo::count()`,
`Fact::created_at`, `HeatState::last_touched` and cluster record IDs are already available; this is a
pure rendering/layout pass. The CSS for new elements (`dl/dt/dd`, `.deleted`, `.badge-deleted`) gets
added to the shared `layout()` in `html.rs`.

**Tech Stack:** Rust, axum 0.8, htmx (existing CDN), `chrono::DateTime<Utc>` (already in scope via the
`Fact` and `HeatState` models), existing `record_id_to_string` helper.

---

## Task 1: List page — total count + pagination UI

**Files:**

- Modify: `crates/alexandria-mcp/src/debug/memories.rs`

The `list` handler calls `repo.list(...)` but never calls `repo.count(...)`. Fix that, then build
Prev/Next links from the result.

**Step 1: Write the failing test**

In the existing `#[cfg(test)] mod tests` block in `memories.rs`, add:

```rust
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
```

**Step 2: Run to verify failure**

```
cargo test -p alexandria-mcp test_memories_list_shows_total_count -- --nocapture
```

Expected: FAIL — "of 3" and "Next" not found.

**Step 3: Implement**

In `memories.rs`, update the `list` handler. After fetching `rows`, also call `repo.count()`:

```rust
// after fetching rows...
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
```

Build pagination summary and links. Add a small helper at the module level (outside `pub async fn list`):

```rust
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
```

Then in the `list` body, replace the current `<p>{} results</p>` section with:

```rust
let showing_from = if rows.is_empty() { 0 } else { offset + 1 };
let showing_to = offset + rows.len();
let summary = format!("Showing {showing_from}–{showing_to} of {total} memories");

let prev_link = if offset > 0 {
    let prev_offset = offset.saturating_sub(limit);
    format!(
        r#"<a class="link" href="{}">← Prev</a>"#,
        memories_url(search.map(|s| s.as_str()), tag.map(|s| s.as_str()), include_deleted, limit, prev_offset)
    )
} else {
    String::new()
};

let next_link = if offset + rows.len() < total {
    format!(
        r#"<a class="link" href="{}">Next →</a>"#,
        memories_url(search.map(|s| s.as_str()), tag.map(|s| s.as_str()), include_deleted, limit, offset + limit)
    )
} else {
    String::new()
};

let pagination = format!(
    r#"<div class="pagination"><span>{summary}</span><span>{prev_link} {next_link}</span></div>"#
);
```

Replace `<p>{} results</p>` with `{pagination}` in the format string.

**Step 4: Run tests**

```
cargo test -p alexandria-mcp test_memories_list_shows_total_count test_memories_list_prev_link -- --nocapture
```

Expected: both PASS.

**Step 5: Commit**

```bash
git add crates/alexandria-mcp/src/debug/memories.rs
git commit -m "feat(debug): add total count and Prev/Next pagination to memories list"
```

---

## Task 2: List page — created_at column + deleted row styling + content ellipsis

**Files:**

- Modify: `crates/alexandria-mcp/src/debug/memories.rs`
- Modify: `crates/alexandria-mcp/src/debug/html.rs`

**Step 1: Write the failing tests**

Add to the `mod tests` block in `memories.rs`:

```rust
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
    assert!(text.contains("2026"), "expected year from created_at in list");
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
```

**Step 2: Run to verify failure**

```
cargo test -p alexandria-mcp test_memories_list_shows_created_at test_memories_list_deleted_row -- --nocapture
```

Expected: FAIL.

**Step 3: Implement**

In `memories.rs`, update the row-building loop:

```rust
for fact in &rows {
    let id = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
    let tags = fact
        .tags
        .iter()
        .map(|t| format!(r#"<span class="badge">{}</span>"#, esc(t)))
        .collect::<String>();

    // Add "…" if content was truncated
    let raw_preview: String = fact.content.chars().take(120).collect();
    let content_preview = if fact.content.chars().count() > 120 {
        format!("{}…", raw_preview)
    } else {
        raw_preview
    };

    // Format created_at as "YYYY-MM-DD HH:MM UTC", fallback to "—"
    let created = fact
        .created_at
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "—".to_string());

    // Deleted rows get a CSS class for dimming
    let row_class = if fact.deleted { r#" class="deleted""# } else { "" };

    rows_html.push_str(&format!(
        r#"<tr{row_class}><td><a class="link" href="/debug/memories/{id}">{id}</a></td><td>{}</td><td>{}</td><td>{:.2}</td><td>{created}</td></tr>"#,
        esc(&content_preview),
        tags,
        fact.confidence,
    ));
}
```

Note the `Deleted` column is now gone from the table body — deleted status is conveyed by row styling
instead. Update the table header to match:

```rust
// Old: <tr><th>ID</th><th>Content</th><th>Tags</th><th>Confidence</th><th>Deleted</th></tr>
// New:
"<tr><th>ID</th><th>Content</th><th>Tags</th><th>Confidence</th><th>Created</th></tr>"
```

In `html.rs`, add CSS for `.deleted` rows and `.pagination`:

```css
tr.deleted td { opacity: 0.45; text-decoration: line-through; }
tr.deleted td:first-child { text-decoration: none; } /* keep ID link readable */
.pagination { display: flex; justify-content: space-between; align-items: center; margin-top: 12px; color: #8b949e; font-size: 14px; }
.pagination a { margin: 0 4px; }
```

Add these to the `<style>` block inside the `layout()` function, before the closing `</style>` tag.

**Step 4: Run tests**

```
cargo test -p alexandria-mcp test_memories_list -- --nocapture
```

Expected: all `test_memories_list_*` tests PASS.

**Step 5: Commit**

```bash
git add crates/alexandria-mcp/src/debug/memories.rs crates/alexandria-mcp/src/debug/html.rs
git commit -m "feat(debug): add created_at column, deleted row styling, and content ellipsis to memories list"
```

---

## Task 3: Detail page — navigation + deleted badge + timestamps

**Files:**

- Modify: `crates/alexandria-mcp/src/debug/memories.rs`

**Step 1: Write the failing tests**

Add to `mod tests`:

```rust
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
    assert!(text.contains("2026"), "expected year in created_at");
}
```

**Step 2: Run to verify failure**

```
cargo test -p alexandria-mcp test_memory_detail_has_back_link test_memory_detail_shows_deleted_badge test_memory_detail_shows_created_at -- --nocapture
```

Expected: FAIL.

**Step 3: Implement**

Replace the `body` format string in `detail()` with the improved version:

```rust
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
<h1>Memory {id_display} {deleted_badge}</h1>
<p><a class="link" href="/debug/graph/{graph_id}">View graph →</a></p>
<pre class="content-block">{content}</pre>
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
    id_display = esc(&id),
    deleted_badge = deleted_badge,
    graph_id = esc(&id),
    content = esc(&fact.content),
    tags = tags,
    confidence = fact.confidence,
    created_at = created_at_str,
    heat_section = heat_html,
    cluster_html = cluster_html,
    edges_html = edges_html,
);
```

For `heat_html`, replace the single-string format with a `<dl>`:

```rust
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
```

Add `.badge-deleted` and `dl.fact-meta` styles to `html.rs` (in the `<style>` block):

```css
.badge-deleted { background: #3d1a1a; color: #f85149; border: 1px solid #6e2020; padding: 2px 10px; border-radius: 12px; font-size: 13px; margin-left: 8px; }
pre.content-block { background: #161b22; border: 1px solid #30363d; border-radius: 6px; padding: 16px; white-space: pre-wrap; word-break: break-word; max-height: 400px; overflow-y: auto; }
dl.fact-meta { display: grid; grid-template-columns: 160px 1fr; gap: 6px 16px; margin: 12px 0; }
dl.fact-meta dt { color: #8b949e; font-weight: 600; }
dl.fact-meta dd { margin: 0; }
```

**Step 4: Run tests**

```
cargo test -p alexandria-mcp test_memory_detail -- --nocapture
```

Expected: all `test_memory_detail_*` PASS, including the pre-existing XSS and 404 tests.

**Step 5: Commit**

```bash
git add crates/alexandria-mcp/src/debug/memories.rs crates/alexandria-mcp/src/debug/html.rs
git commit -m "feat(debug): add back link, deleted badge, timestamps, and structured heat to memory detail"
```

---

## Task 4: Detail page — navigable cluster and edge links

**Files:**

- Modify: `crates/alexandria-mcp/src/debug/memories.rs`

The cluster and edges sections currently display plain text record IDs. Make them links.

**Step 1: Write the failing tests**

Add to `mod tests`:

```rust
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
    cluster_repo.add_member(&cluster_id, &fact_id).await.unwrap();

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
    // The cluster section should contain a link to /debug/clusters/...
    assert!(
        text.contains("/debug/clusters/"),
        "expected cluster to be a link to /debug/clusters/:id"
    );
    assert!(text.contains("my cluster"), "expected cluster label in link");
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
    // Edge out-node should be a link to the other memory's detail page
    assert!(
        text.contains("/debug/memories/"),
        "expected edge nodes to be links to memory detail pages"
    );
}
```

**Step 2: Run to verify failure**

```
cargo test -p alexandria-mcp test_memory_detail_cluster_is_a_link test_memory_detail_edges_are_links -- --nocapture
```

Expected: FAIL.

**Step 3: Implement**

In `detail()`, replace `cluster_html` construction:

```rust
// Old: plain text
// let cluster_html = match &cluster {
//     Some(c) => format!("{} ({})", ...),
//     None => "none".to_string(),
// };

let cluster_html = match &cluster {
    Some(c) => {
        let cluster_id = c.id.as_ref().map(record_id_to_string).unwrap_or_default();
        let label = esc(c.label.as_deref().unwrap_or("unlabeled"));
        let encoded_id = cluster_id.replace(':', "%3A");
        format!(
            r#"<a class="link" href="/debug/clusters/{encoded_id}">{label}</a> <span class="badge">{cluster_id}</span>"#
        )
    }
    None => "None".to_string(),
};
```

Replace `edges_html` construction:

```rust
let edges_html: String = edges
    .iter()
    .map(|e| {
        let in_id = e.in_node.as_ref().map(record_id_to_string).unwrap_or_default();
        let out_id = e.out_node.as_ref().map(record_id_to_string).unwrap_or_default();
        let in_encoded = in_id.replace(':', "%3A");
        let out_encoded = out_id.replace(':', "%3A");
        format!(
            r#"<li><span class="badge">{}</span> <a class="link" href="/debug/memories/{in_encoded}">{in_id}</a> → <a class="link" href="/debug/memories/{out_encoded}">{out_id}</a> (strength {:.2})</li>"#,
            esc(&e.edge_type),
            e.strength
        )
    })
    .collect();
```

When the edge list is empty, show a placeholder rather than an empty `<ul>`:

```rust
let edges_section = if edges.is_empty() {
    "<p>No edges.</p>".to_string()
} else {
    format!("<ul>{edges_html}</ul>")
};
// Use {edges_section} in the body format string instead of <ul>{edges_html}</ul>
```

**Step 4: Run tests**

```
cargo test -p alexandria-mcp test_memory_detail -- --nocapture
```

Expected: all `test_memory_detail_*` PASS.

**Step 5: Commit**

```bash
git add crates/alexandria-mcp/src/debug/memories.rs
git commit -m "feat(debug): make cluster and edge IDs navigable links on memory detail"
```

---

## Task 5: Metadata display on detail

**Files:**

- Modify: `crates/alexandria-mcp/src/debug/memories.rs`

`Fact::metadata` is `Option<surrealdb::types::Value>`. Show it as a formatted JSON block if present.

**Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[tokio::test]
async fn test_memory_detail_shows_metadata_when_present() {
    // Create a fact with metadata by using the raw SurrealDB query
    // (create_fact doesn't expose metadata, but the field exists on the model)
    // We verify the metadata section header at minimum renders.
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
    // Section header always renders (shows "none" when metadata is null)
    assert!(
        text.contains("Metadata"),
        "expected Metadata section on detail page"
    );
}
```

**Step 2: Run to verify failure**

```
cargo test -p alexandria-mcp test_memory_detail_shows_metadata -- --nocapture
```

Expected: FAIL — "Metadata" not found.

**Step 3: Implement**

In `detail()`, after the edges section, build a metadata block. `surrealdb::types::Value` implements
`std::fmt::Debug`; for a readable display, format it with `{:?}` and wrap in a `<pre>`:

```rust
let metadata_section = match &fact.metadata {
    Some(m) => format!(
        "<h2>Metadata</h2><pre class=\"content-block\">{}</pre>",
        esc(&format!("{m:#?}"))
    ),
    None => "<h2>Metadata</h2><p>None</p>".to_string(),
};
```

Add `{metadata_section}` to the `body` format string after the edges section.

Note: `surrealdb::types::Value` may not implement `serde_json::Serialize` directly. If `{m:#?}` produces
ugly Rust debug output (e.g. `Object({...})` wrapping), try `m.into_json()` or converting to
`serde_json::Value` first — check the actual type's trait impls. The goal is human-readable JSON; adjust
the formatting if the debug output is illegible.

**Step 4: Run tests**

```
cargo test -p alexandria-mcp test_memory_detail -- --nocapture
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/alexandria-mcp/src/debug/memories.rs
git commit -m "feat(debug): add metadata section to memory detail page"
```

---

## Final verification

Run the full workspace test suite and clippy to confirm no regressions:

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Expected: clean. If a new clippy warning fires on the `memories_url` helper (e.g. "function can be made
more generic"), address it or allow it with a comment.

Then optionally do a quick smoke test against a running instance:

```bash
# Start with HTTP transport
cargo run -p alexandria &
curl -s http://localhost:3000/debug/memories | grep -o "Showing.*memories"
curl -s http://localhost:3000/debug/memories?limit=2 | grep -o "Next\|Prev"
```
