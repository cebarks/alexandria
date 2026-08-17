# Debug Web UI Implementation Plan

> **REQUIRED SUB-SKILL:** Use the executing-plans skill to implement this plan task-by-task.

**Goal:** Add a read-only, server-rendered debug web UI (mounted at `/debug` on the existing HTTP
transport) that exposes memory, cluster, edge/graph, and live-query visibility into Alexandria's
SurrealDB-backed memory store.

**Architecture:** New `debug` module inside `alexandria-mcp` (the only crate that already knows both
`alexandria-engine` and `alexandria-storage`). It exposes `debug::router(AlexandriaServer) -> axum::Router`,
built from small hand-written HTML string helpers (no template engine) plus htmx (CDN) for
interactivity and vis-network (CDN) for the one graph page. New read-only query methods are added to
`alexandria-storage` repos; no existing write paths change. `crates/alexandria/src/main.rs` nests the
router at `/debug` only when `transport = "http"`.

**Tech Stack:** Rust, axum 0.8, SurrealDB 3.2 (surrealdb crate, `Surreal<Any>`), htmx (CDN), vis-network
(CDN), tokio, existing `AlexandriaServer` / repo types.

Design reference: `docs/plans/2026-08-17-debug-webui-design.md`

---

## Task 0: Add axum dev-dependency to alexandria-mcp for router tests

**Files:**

- Modify: `crates/alexandria-mcp/Cargo.toml`

**Step 1:** Add to `[dependencies]`: `axum = "0.8"` (needed at non-dev level since `debug::router` returns
`axum::Router` from library code, not just tests). Add to `[dev-dependencies]`: `tower = { version = "0.5", features = ["util"] }`
for `oneshot` in tests.

**Step 2:** Run `cargo check -p alexandria-mcp` — expect it to succeed (no code uses axum yet, just confirms
the dependency resolves).

**Step 3: Commit**

```bash
git add crates/alexandria-mcp/Cargo.toml Cargo.lock
git commit -m "chore: add axum dependency to alexandria-mcp for debug UI"
```

---

## Task 1: Storage — `MemoryRepo::list` and `count`

**Files:**

- Modify: `crates/alexandria-storage/src/repos/memory_repo.rs`
- Test: same file, `#[cfg(test)] mod tests` block at the bottom (create if absent)

**Step 1: Write the failing test**

Add to `crates/alexandria-storage/src/repos/memory_repo.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;

    #[tokio::test]
    async fn test_list_and_count_facts() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());

        repo.create_fact("alpha content", 0.5, &[0.1, 0.2], &["tag1".to_string()]).await.unwrap();
        repo.create_fact("beta content", 0.5, &[0.3, 0.4], &["tag2".to_string()]).await.unwrap();
        let deleted_id = repo.create_fact("gamma content", 0.5, &[0.5, 0.6], &[]).await.unwrap();
        repo.soft_delete_fact(&deleted_id).await.unwrap();

        // Default: excludes deleted
        let all = repo.list(None, None, false, 10, 0).await.unwrap();
        assert_eq!(all.len(), 2);

        // include_deleted = true picks up all 3
        let with_deleted = repo.list(None, None, true, 10, 0).await.unwrap();
        assert_eq!(with_deleted.len(), 3);

        // search filters by content substring
        let searched = repo.list(Some("alpha"), None, false, 10, 0).await.unwrap();
        assert_eq!(searched.len(), 1);
        assert_eq!(searched[0].content, "alpha content");

        // tag filters
        let tagged = repo.list(None, Some("tag2"), false, 10, 0).await.unwrap();
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].content, "beta content");

        // count matches list length for same filters
        let count = repo.count(None, None, false).await.unwrap();
        assert_eq!(count, 2);

        // limit/offset paginate
        let page1 = repo.list(None, None, false, 1, 0).await.unwrap();
        let page2 = repo.list(None, None, false, 1, 1).await.unwrap();
        assert_eq!(page1.len(), 1);
        assert_eq!(page2.len(), 1);
        assert_ne!(page1[0].content, page2[0].content);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p alexandria-storage test_list_and_count_facts`
Expected: FAIL to compile — `list`/`count` not found on `MemoryRepo`.

**Step 3: Write minimal implementation**

Add to `impl<'a> MemoryRepo<'a>` in the same file:

```rust
/// List facts with optional content search, tag filter, and deleted-inclusion.
/// `search` does a case-insensitive substring match against content.
pub async fn list(
    &self,
    search: Option<&str>,
    tag: Option<&str>,
    include_deleted: bool,
    limit: usize,
    offset: usize,
) -> Result<Vec<Fact>> {
    let mut conditions = Vec::new();
    if !include_deleted {
        conditions.push("deleted = false".to_string());
    }
    if search.is_some() {
        conditions.push("string::lowercase(content) CONTAINS string::lowercase($search)".to_string());
    }
    if tag.is_some() {
        conditions.push("$tag IN tags".to_string());
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let query = format!(
        "SELECT * FROM fact {where_clause} ORDER BY created_at DESC LIMIT $limit START $offset"
    );

    let mut q = self
        .db
        .query(&query)
        .bind(("limit", limit as i64))
        .bind(("offset", offset as i64));
    if let Some(s) = search {
        q = q.bind(("search", s.to_string()));
    }
    if let Some(t) = tag {
        q = q.bind(("tag", t.to_string()));
    }

    let mut response = q.await?;
    let facts: Vec<Fact> = response.take(0)?;
    Ok(facts)
}

/// Count facts matching the same filters as `list` (ignoring limit/offset).
pub async fn count(
    &self,
    search: Option<&str>,
    tag: Option<&str>,
    include_deleted: bool,
) -> Result<usize> {
    let mut conditions = Vec::new();
    if !include_deleted {
        conditions.push("deleted = false".to_string());
    }
    if search.is_some() {
        conditions.push("string::lowercase(content) CONTAINS string::lowercase($search)".to_string());
    }
    if tag.is_some() {
        conditions.push("$tag IN tags".to_string());
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let query = format!("SELECT count() FROM fact {where_clause} GROUP ALL");

    let mut q = self.db.query(&query);
    if let Some(s) = search {
        q = q.bind(("search", s.to_string()));
    }
    if let Some(t) = tag {
        q = q.bind(("tag", t.to_string()));
    }

    #[derive(serde::Deserialize, surrealdb::types::SurrealValue)]
    struct CountRow {
        count: i64,
    }

    let mut response = q.await?;
    let rows: Vec<CountRow> = response.take(0)?;
    Ok(rows.first().map(|r| r.count as usize).unwrap_or(0))
}
```

Note: verify `CONTAINS` and `string::lowercase` are valid SurrealDB 3.2 functions before trusting this —
run the test and adjust query syntax if SurrealDB rejects it (check error message; 3.2 sometimes wants
different function names — see CLAUDE.md gotchas). If `CONTAINS` on strings is rejected or behaves as an
array-membership operator instead of substring match, fall back to the design doc's originally suggested
`content ~ $search` fuzzy-match operator, or `string::contains(content, $search)` — try both empirically
and pick whichever SurrealDB 3.2 actually accepts before moving on to Task 2.

**Step 4: Run test to verify it passes**

Run: `cargo test -p alexandria-storage test_list_and_count_facts -- --nocapture`
Expected: PASS. If a SurrealQL syntax error appears, read the error text, fix the query, re-run.

**Step 5: Commit**

```bash
git add crates/alexandria-storage/src/repos/memory_repo.rs
git commit -m "feat(storage): add MemoryRepo::list and count for debug UI"
```

---

## Task 2: Storage — `MemoryRepo::cluster_for_fact`

**Files:**

- Modify: `crates/alexandria-storage/src/repos/memory_repo.rs`

**Step 1: Write the failing test**

Add to the same `mod tests` block:

```rust
#[tokio::test]
async fn test_cluster_for_fact() {
    let db = Database::connect_embedded().await.unwrap();
    crate::schema::migrate(db.inner()).await.unwrap();
    let repo = MemoryRepo::new(db.inner());
    let cluster_repo = crate::repos::ClusterRepo::new(db.inner());

    let fact_id = repo.create_fact("clustered content", 0.5, &[0.1, 0.2], &[]).await.unwrap();
    let cluster_id = cluster_repo.create(Some("test cluster"), &[0.1, 0.2]).await.unwrap();
    cluster_repo.add_member(&cluster_id, &fact_id).await.unwrap();

    let cluster_found = repo.cluster_for_fact(&fact_id).await.unwrap();
    assert!(cluster_found.is_some());
    let cluster_found = cluster_found.unwrap();
    assert_eq!(cluster_found.label.as_deref(), Some("test cluster"));

    // Fact with no cluster returns None
    let orphan_id = repo.create_fact("orphan content", 0.5, &[0.9, 0.9], &[]).await.unwrap();
    let none = repo.cluster_for_fact(&orphan_id).await.unwrap();
    assert!(none.is_none());
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p alexandria-storage test_cluster_for_fact`
Expected: FAIL to compile — `cluster_for_fact` not found.

**Step 3: Write minimal implementation**

Add to `impl<'a> MemoryRepo<'a>`:

```rust
/// Find the cluster containing this fact, if any (reverse traversal of contains_memory).
pub async fn cluster_for_fact(&self, fact_id: &str) -> Result<Option<crate::models::Cluster>> {
    let mut response = self
        .db
        .query("SELECT * FROM type::record($id)<-contains_memory<-cluster")
        .bind(("id", fact_id.to_string()))
        .await?;
    let clusters: Vec<crate::models::Cluster> = response.take(0)?;
    Ok(clusters.into_iter().next())
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p alexandria-storage test_cluster_for_fact -- --nocapture`
Expected: PASS. If the graph traversal syntax is rejected, try `<-contains_memory<-(cluster)` or consult
SurrealDB 3.2 docs for reverse edge traversal syntax — the `RELATE cluster->contains_memory->fact` means
`fact<-contains_memory<-cluster` should read backward correctly, but confirm empirically.

**Step 5: Commit**

```bash
git add crates/alexandria-storage/src/repos/memory_repo.rs
git commit -m "feat(storage): add MemoryRepo::cluster_for_fact"
```

---

## Task 3: Storage — `ClusterRepo::list_with_counts`

**Files:**

- Modify: `crates/alexandria-storage/src/repos/cluster_repo.rs`

**Step 1: Write the failing test**

Add a `#[cfg(test)] mod tests` block:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;

    #[tokio::test]
    async fn test_list_with_counts() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let cluster_repo = ClusterRepo::new(db.inner());
        let memory_repo = crate::repos::MemoryRepo::new(db.inner());

        let c1 = cluster_repo.create(Some("cluster one"), &[0.1, 0.1]).await.unwrap();
        let f1 = memory_repo.create_fact("f1", 0.5, &[0.1, 0.1], &[]).await.unwrap();
        let f2 = memory_repo.create_fact("f2", 0.5, &[0.1, 0.1], &[]).await.unwrap();
        cluster_repo.add_member(&c1, &f1).await.unwrap();
        cluster_repo.add_member(&c1, &f2).await.unwrap();

        let c2 = cluster_repo.create(Some("cluster two"), &[0.9, 0.9]).await.unwrap();
        let _ = c2;

        let results = cluster_repo.list_with_counts().await.unwrap();
        assert_eq!(results.len(), 2);
        let (cluster1, count1) = results.iter().find(|(c, _)| c.label.as_deref() == Some("cluster one")).unwrap();
        assert_eq!(*count1, 2);
        let _ = cluster1;
        let (_, count2) = results.iter().find(|(c, _)| c.label.as_deref() == Some("cluster two")).unwrap();
        assert_eq!(*count2, 0);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p alexandria-storage test_list_with_counts`
Expected: FAIL to compile — `list_with_counts` not found.

**Step 3: Write minimal implementation**

Add to `impl<'a> ClusterRepo<'a>`:

```rust
/// List all clusters along with their live member counts.
pub async fn list_with_counts(&self) -> Result<Vec<(Cluster, usize)>> {
    let mut response = self.db.query("SELECT * FROM cluster").await?;
    let clusters: Vec<Cluster> = response.take(0)?;

    let mut result = Vec::with_capacity(clusters.len());
    for cluster in clusters {
        // Use the closure form (matches existing repo convention, e.g. create()/create_fact()),
        // not a bare `ToSql::to_sql` method reference — keeps id formatting consistent with the
        // rest of the codebase's `table:key` string usage.
        let id = cluster
            .id
            .as_ref()
            .map(|r| r.to_sql())
            .unwrap_or_default();
        let count = self.get_members(&id).await.map(|m| m.len()).unwrap_or(0);
        result.push((cluster, count));
    }
    Ok(result)
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p alexandria-storage test_list_with_counts -- --nocapture`
Expected: PASS.

**Step 5: Commit**

```bash
git add crates/alexandria-storage/src/repos/cluster_repo.rs
git commit -m "feat(storage): add ClusterRepo::list_with_counts"
```

---

## Task 4: Storage — stats module

**Files:**

- Create: `crates/alexandria-storage/src/stats.rs`
- Modify: `crates/alexandria-storage/src/lib.rs`

**Step 1: Write the failing test**

Create `crates/alexandria-storage/src/stats.rs` with the implementation directly (this is a thin
aggregation module; a single integration-style test is enough — no separate red step needed for pure
plumbing, but we still verify it compiles and returns sane counts):

```rust
use anyhow::Result;
use serde::Serialize;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

/// Aggregate counts for the debug dashboard.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Stats {
    pub fact_count: usize,
    pub deleted_fact_count: usize,
    pub cluster_count: usize,
    pub edge_count: usize,
    pub raw_count: usize,
}

async fn count_table(db: &Surreal<Any>, table: &str, where_clause: &str) -> Result<usize> {
    #[derive(serde::Deserialize, surrealdb::types::SurrealValue)]
    struct CountRow {
        count: i64,
    }
    let query = format!("SELECT count() FROM {table} {where_clause} GROUP ALL");
    let mut response = db.query(&query).await?;
    let rows: Vec<CountRow> = response.take(0)?;
    Ok(rows.first().map(|r| r.count as usize).unwrap_or(0))
}

/// Gather aggregate counts across all core tables.
pub async fn gather(db: &Surreal<Any>) -> Result<Stats> {
    Ok(Stats {
        fact_count: count_table(db, "fact", "WHERE deleted = false").await?,
        deleted_fact_count: count_table(db, "fact", "WHERE deleted = true").await?,
        cluster_count: count_table(db, "cluster", "").await?,
        edge_count: count_table(db, "memory_edge", "").await?,
        raw_count: count_table(db, "raw", "").await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;

    #[tokio::test]
    async fn test_gather_stats_empty_db() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let stats = gather(db.inner()).await.unwrap();
        assert_eq!(stats.fact_count, 0);
        assert_eq!(stats.cluster_count, 0);
    }

    #[tokio::test]
    async fn test_gather_stats_with_data() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = crate::repos::MemoryRepo::new(db.inner());
        let id = repo.create_fact("x", 0.5, &[0.1], &[]).await.unwrap();
        repo.create_fact("y", 0.5, &[0.2], &[]).await.unwrap();
        repo.soft_delete_fact(&id).await.unwrap();

        let stats = gather(db.inner()).await.unwrap();
        assert_eq!(stats.fact_count, 1);
        assert_eq!(stats.deleted_fact_count, 1);
    }
}
```

**Step 2: Wire the module in**

In `crates/alexandria-storage/src/lib.rs` add `pub mod stats;`.

**Step 3: Run tests**

Run: `cargo test -p alexandria-storage stats::`
Expected: PASS.

**Step 4: Commit**

```bash
git add crates/alexandria-storage/src/stats.rs crates/alexandria-storage/src/lib.rs
git commit -m "feat(storage): add stats module for debug dashboard"
```

---

## Task 5: HTML rendering helpers

**Files:**

- Create: `crates/alexandria-mcp/src/debug/mod.rs`
- Create: `crates/alexandria-mcp/src/debug/html.rs`
- Modify: `crates/alexandria-mcp/src/lib.rs`

**Step 1: Write the failing test**

Create `crates/alexandria-mcp/src/debug/html.rs`:

```rust
/// Escape a string for safe interpolation into HTML text/attribute content.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Wrap a body fragment in the shared page layout (nav + htmx script + minimal CSS).
pub fn layout(title: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{title} — Alexandria Debug</title>
<script src="https://unpkg.com/htmx.org@1.9.12"></script>
<style>
body {{ font-family: system-ui, sans-serif; margin: 0; padding: 0; background: #0d1117; color: #c9d1d9; }}
nav {{ background: #161b22; padding: 12px 24px; border-bottom: 1px solid #30363d; }}
nav a {{ color: #58a6ff; margin-right: 16px; text-decoration: none; }}
main {{ padding: 24px; max-width: 1100px; margin: 0 auto; }}
table {{ width: 100%; border-collapse: collapse; margin-top: 12px; }}
th, td {{ text-align: left; padding: 8px; border-bottom: 1px solid #30363d; }}
th {{ color: #8b949e; font-weight: 600; }}
input, select, textarea, button {{ background: #0d1117; color: #c9d1d9; border: 1px solid #30363d; padding: 6px 8px; border-radius: 4px; }}
button {{ cursor: pointer; }}
.badge {{ display: inline-block; background: #21262d; padding: 2px 8px; border-radius: 12px; font-size: 12px; margin-right: 4px; }}
a.link {{ color: #58a6ff; }}
.error {{ color: #f85149; }}
</style>
</head>
<body>
<nav>
<a href="/debug">Dashboard</a>
<a href="/debug/memories">Memories</a>
<a href="/debug/clusters">Clusters</a>
<a href="/debug/query">Query Tester</a>
</nav>
<main>
{body}
</main>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_esc_escapes_all_special_chars() {
        let input = r#"<script>alert("x")&'y'</script>"#;
        let out = esc(input);
        assert!(!out.contains('<'));
        assert!(!out.contains('>'));
        assert!(out.contains("&lt;script&gt;"));
        assert!(out.contains("&quot;x&quot;"));
        assert!(out.contains("&#39;y&#39;"));
    }

    #[test]
    fn test_layout_includes_title_and_body() {
        let html = layout("Test Page", "<p>hello</p>");
        assert!(html.contains("Test Page"));
        assert!(html.contains("<p>hello</p>"));
        assert!(html.contains("htmx.org"));
    }
}
```

**Step 2: Wire module in**

Create `crates/alexandria-mcp/src/debug/mod.rs`:

```rust
pub mod html;
```

In `crates/alexandria-mcp/src/lib.rs`, add `pub mod debug;`.

**Step 3: Run tests**

Run: `cargo test -p alexandria-mcp debug::html::`
Expected: PASS (tests are self-contained pure functions, should pass immediately — this task establishes
the pattern rather than driving out behavior via a red step).

**Step 4: Commit**

```bash
git add crates/alexandria-mcp/src/debug/ crates/alexandria-mcp/src/lib.rs
git commit -m "feat(debug): add HTML escaping and layout helpers"
```

---

## Task 6: Dashboard route `GET /debug`

**Files:**

- Create: `crates/alexandria-mcp/src/debug/dashboard.rs`
- Modify: `crates/alexandria-mcp/src/debug/mod.rs`

**Step 1: Write the failing test**

Create `crates/alexandria-mcp/src/debug/dashboard.rs`:

```rust
use axum::extract::State;
use axum::response::Html;

use crate::AlexandriaServer;
use super::html::{esc, layout};

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
        Err(e) => format!(r#"<p class="error">Failed to load stats: {}</p>"#, esc(&e.to_string())),
    };
    Html(layout("Dashboard", &body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alexandria_pipeline::embedding::EmbeddingProvider;
    use alexandria_storage::Database;
    use std::sync::Arc;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    // Minimal stub embedding provider for router-level tests (no model download needed).
    struct StubEmbedding;
    #[async_trait::async_trait]
    impl EmbeddingProvider for StubEmbedding {
        async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.1, 0.2]).collect())
        }
        fn dimensions(&self) -> usize { 2 }
        fn model_id(&self) -> &str { "stub" }
    }

    async fn test_server() -> AlexandriaServer {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner()).await.unwrap();
        AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0)
    }

    #[tokio::test]
    async fn test_dashboard_returns_200_with_stats() {
        let server = test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(Request::builder().uri("/debug").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("Alexandria Debug Dashboard"));
        assert!(text.contains("Facts (active)"));
    }
}
```

`EmbeddingProvider` (confirmed at `crates/alexandria-pipeline/src/embedding/provider.rs`) has **three**
required methods: `embed`, `dimensions`, and `model_id(&self) -> &str`. The stub above includes all three
— do not drop `model_id` or the impl won't compile.

**Step 2: Add `router()` to `debug/mod.rs`**

```rust
pub mod dashboard;
pub mod html;

use axum::routing::get;
use axum::Router;

use crate::AlexandriaServer;

pub fn router(server: AlexandriaServer) -> Router {
    Router::new()
        .route("/debug", get(dashboard::handler))
        .with_state(server)
}
```

**Step 3: Run test to verify it fails, then passes**

Run: `cargo test -p alexandria-mcp dashboard::`
Expected: first confirm it fails to compile if types are wrong (adjust stub to match the real trait),
then passes once `router()` and `handler` exist and compile correctly.

**Step 4: Commit**

```bash
git add crates/alexandria-mcp/src/debug/
git commit -m "feat(debug): add dashboard route with live stats"
```

---

## Task 7: Memory browser `GET /debug/memories`

**Files:**

- Create: `crates/alexandria-mcp/src/debug/memories.rs`
- Modify: `crates/alexandria-mcp/src/debug/mod.rs`

**Step 1: Write the test** (add to `memories.rs`, following the same `test_server()` pattern as Task 6 —
extract that helper into a shared `#[cfg(test)]` module, e.g. `debug/test_support.rs`, to avoid duplication)

```rust
#[tokio::test]
async fn test_memories_list_shows_created_fact() {
    let server = super::test_support::test_server().await;
    let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
    repo.create_fact("a searchable memory", 0.5, &[0.1, 0.2], &["demo".to_string()]).await.unwrap();

    let app = crate::debug::router(server);
    let response = app
        .oneshot(Request::builder().uri("/debug/memories").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("a searchable memory"));
    assert!(text.contains("demo"));
}

#[tokio::test]
async fn test_memories_search_query_param_filters() {
    let server = super::test_support::test_server().await;
    let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
    repo.create_fact("apple pie", 0.5, &[0.1, 0.2], &[]).await.unwrap();
    repo.create_fact("banana bread", 0.5, &[0.3, 0.4], &[]).await.unwrap();

    let app = crate::debug::router(server);
    let response = app
        .oneshot(Request::builder().uri("/debug/memories?search=apple").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("apple pie"));
    assert!(!text.contains("banana bread"));
}
```

Refactor: move the `StubEmbedding` + `test_server()` helper from `dashboard.rs` into
`crates/alexandria-mcp/src/debug/test_support.rs` (behind `#[cfg(test)]`, `pub(super)` visibility), have
both test modules `use super::test_support`. Register `#[cfg(test)] mod test_support;` in `debug/mod.rs`.

**Step 2: Run to verify failure, then implement**

```rust
use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::response::Html;

use crate::AlexandriaServer;
use crate::server::record_id_to_string;
use super::html::{esc, layout};

pub async fn list(
    State(server): State<AlexandriaServer>,
    Query(params): Query<HashMap<String, String>>,
) -> Html<String> {
    let search = params.get("search").filter(|s| !s.is_empty());
    let tag = params.get("tag").filter(|s| !s.is_empty());
    let include_deleted = params.get("include_deleted").map(|v| v == "true").unwrap_or(false);
    let limit: usize = params.get("limit").and_then(|v| v.parse().ok()).unwrap_or(50);
    let offset: usize = params.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);

    let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
    let rows = match repo.list(search.map(|s| s.as_str()), tag.map(|s| s.as_str()), include_deleted, limit, offset).await {
        Ok(r) => r,
        Err(e) => return Html(layout("Memories", &format!(r#"<p class="error">{}</p>"#, esc(&e.to_string())))),
    };

    let mut rows_html = String::new();
    for fact in &rows {
        let id = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
        let tags = fact.tags.iter().map(|t| format!(r#"<span class="badge">{}</span>"#, esc(t))).collect::<String>();
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
        r#"<h1>Memories</h1>
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
</div>"#,
        esc(search.map(|s| s.as_str()).unwrap_or("")),
        esc(tag.map(|s| s.as_str()).unwrap_or("")),
        if include_deleted { "checked" } else { "" },
        rows.len(),
    );

    Html(layout("Memories", &body))
}
```

Register in `debug/mod.rs`: `pub mod memories;` and add
`.route("/debug/memories", get(memories::list))` to the router.

**Step 3: Run tests**

Run: `cargo test -p alexandria-mcp memories::`
Expected: PASS.

**Step 4: Commit**

```bash
git add crates/alexandria-mcp/src/debug/
git commit -m "feat(debug): add memory browser route with search/filter"
```

---

## Task 8: Memory detail `GET /debug/memories/:id`

**Files:**

- Modify: `crates/alexandria-mcp/src/debug/memories.rs`
- Modify: `crates/alexandria-mcp/src/debug/mod.rs`

**Step 1: Write the test**

```rust
#[tokio::test]
async fn test_memory_detail_shows_content_and_heat() {
    let server = super::test_support::test_server().await;
    let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
    let heat_repo = alexandria_storage::repos::HeatRepo::new(server.db.inner());
    let id = repo.create_fact("detailed memory content", 0.7, &[0.1, 0.2], &["x".to_string()]).await.unwrap();
    heat_repo.create_for_memory(&id, 1.5).await.unwrap();

    let app = crate::debug::router(server);
    let uri = format!("/debug/memories/{}", id.replace(':', "%3A"));
    let response = app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("detailed memory content"));
}

#[tokio::test]
async fn test_memory_detail_404_for_missing_id() {
    let server = super::test_support::test_server().await;
    let app = crate::debug::router(server);
    let response = app.oneshot(Request::builder().uri("/debug/memories/fact%3Anonexistent").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), 404);
}
```

Note: `RecordId` string form is `table:key`; the `:` needs URL-encoding as `%3A` in the request path since
axum path params don't accept raw `:`. Confirm by running the test — if axum's path extractor already
decodes percent-encoding automatically (it does), this should just work when the id param is captured as
a single path segment.

**Step 2: Implement**

Add to `memories.rs`:

```rust
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;

pub async fn detail(
    State(server): State<AlexandriaServer>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
    let fact = match repo.get_fact(&id).await {
        Ok(Some(f)) => f,
        Ok(None) => return (StatusCode::NOT_FOUND, Html(layout("Not Found", "<p>Memory not found.</p>"))),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Html(layout("Error", &format!(r#"<p class="error">{}</p>"#, esc(&e.to_string()))))),
    };

    let heat_repo = alexandria_storage::repos::HeatRepo::new(server.db.inner());
    let heat = heat_repo.get(&id).await.ok().flatten();
    let heat_html = match &heat {
        Some(h) => format!("heat={:.3} stability={:.3} access_count={}", h.heat, h.stability, h.access_count),
        None => "no heat state".to_string(),
    };

    let cluster = repo.cluster_for_fact(&id).await.ok().flatten();
    let cluster_html = match &cluster {
        Some(c) => format!("{} ({})", esc(c.label.as_deref().unwrap_or("unlabeled")), c.id.as_ref().map(record_id_to_string).unwrap_or_default()),
        None => "none".to_string(),
    };

    let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());
    let edges = edge_repo.get_edges_for(&id).await.unwrap_or_default();
    let edges_html: String = edges.iter().map(|e| {
        format!("<li>{} — {} → {} (strength {:.2})</li>",
            esc(&e.edge_type),
            e.in_node.as_ref().map(record_id_to_string).unwrap_or_default(),
            e.out_node.as_ref().map(record_id_to_string).unwrap_or_default(),
            e.strength)
    }).collect();

    let tags: String = fact.tags.iter().map(|t| format!(r#"<span class="badge">{}</span>"#, esc(t))).collect();

    let body = format!(
        r#"<h1>Memory {}</h1>
<p><a class="link" href="/debug/graph/{}">View graph</a></p>
<pre>{}</pre>
<p>Tags: {tags}</p>
<p>Confidence: {}</p>
<p>Deleted: {}</p>
<p>Heat: {}</p>
<p>Cluster: {}</p>
<h2>Edges</h2>
<ul>{}</ul>"#,
        esc(&id), esc(&id), esc(&fact.content), fact.confidence, fact.deleted, heat_html, cluster_html, edges_html,
    );

    (StatusCode::OK, Html(layout("Memory Detail", &body)))
}
```

Register route in `debug/mod.rs`: `.route("/debug/memories/{id}", get(memories::detail))` (axum 0.8 path
syntax uses `{id}` not `:id` — verify against installed axum version's docs/changelog before writing;
0.7+ switched to this syntax).

**Step 3: Run tests, fix compile/route issues, confirm pass**

Run: `cargo test -p alexandria-mcp memories::`

**Step 4: Commit**

```bash
git add crates/alexandria-mcp/src/debug/
git commit -m "feat(debug): add memory detail route with heat/cluster/edges"
```

---

## Task 9: Cluster explorer `GET /debug/clusters` and `GET /debug/clusters/:id`

**Files:**

- Create: `crates/alexandria-mcp/src/debug/clusters.rs`
- Modify: `crates/alexandria-mcp/src/debug/mod.rs`

**Step 1: Write tests** (mirror Task 7/8 patterns): one test creates two clusters via `ClusterRepo`, hits
`/debug/clusters`, asserts both labels appear with correct member counts; another creates a cluster with
members, hits `/debug/clusters/:id`, asserts member content appears.

**Step 2: Implement `list` and `detail` handlers** analogous to `memories.rs`, using
`ClusterRepo::list_with_counts()` for the list view and `ClusterRepo::get_members()` for detail. Compute
live cohesion in the detail view via `alexandria_engine::clusters::maintenance` — check that module's
public API (`check_cohesion` returns `MaintenanceAction`, not a bare score) and either expose a raw
cosine-avg helper or just display "Healthy"/"Needs split" from `check_cohesion`'s result rather than
inventing a numeric API that doesn't exist yet.

**Step 3: Register routes**, run tests, commit.

```bash
git add crates/alexandria-mcp/src/debug/
git commit -m "feat(debug): add cluster explorer routes"
```

---

## Task 10: Graph view `GET /debug/graph/:id` + `GET /debug/api/graph/:id`

**Files:**

- Create: `crates/alexandria-mcp/src/debug/graph.rs`
- Modify: `crates/alexandria-mcp/src/debug/mod.rs`

**Step 1: Write a test for the JSON API endpoint** — create a fact with a `memory_edge` to another fact via
`EdgeRepo::create_edge`, hit `/debug/api/graph/:id`, assert the JSON body parses and contains both node
ids and one edge.

**Step 2: Implement**

- `api_graph(State, Path<id>) -> Json<serde_json::Value>`: uses `EdgeRepo::get_neighbors(id, 2)` (existing
  method) to build `{ "nodes": [...], "edges": [...] }` in vis-network's expected shape
  (`{id, label}` nodes / `{from, to, label}` edges).
- `page(State, Path<id>) -> Html<String>`: static HTML with a `<div id="graph">` canvas, inline `<script>`
  loading vis-network from CDN pinned to a specific version
(`https://unpkg.com/vis-network@9.1.6/standalone/umd/vis-network.min.js` — check unpkg for the current
latest stable release tag and pin to it explicitly, do not use an unversioned URL), fetching
  `/debug/api/graph/{id}` and rendering it. Keep the fetch/render script small and inline (no separate JS
  file needed).

**Step 3: Register both routes**, run tests, commit.

```bash
git add crates/alexandria-mcp/src/debug/
git commit -m "feat(debug): add graph visualization route"
```

---

## Task 11: Query tester `GET /debug/query` + `POST /debug/query/run`

**Files:**

- Create: `crates/alexandria-mcp/src/debug/query.rs`
- Modify: `crates/alexandria-mcp/src/debug/mod.rs`

**Step 1: Write a test** using the `StubEmbedding` test server: POST to `/debug/query/run` with form data
`mode=retrieve&query=test&limit=5`, assert 200 and that the response HTML fragment renders (even if empty
results, since the stub DB has no facts) without erroring. Add a second test that first stores a fact via
`AlexandriaServer::do_store_memory`, then POSTs the same query and asserts the fact's content appears in
the results fragment. Add a third test with `mode=recall&query=test` (no `limit`) confirming recall mode
works without a limit field.

**Step 2: Implement**

- `form(State) -> Html<String>`: renders a form with `mode` select (`retrieve`/`recall`), `query` text
  input, and a `limit` number input that only applies to `retrieve` mode (note in the UI that it's ignored
  for recall), an htmx `hx-post="/debug/query/run" hx-target="#query-results"`.
- `run(State, Form<QueryForm>) -> Html<String>`: dispatches to `server.do_retrieve_memories(...)` (which
  takes `RetrieveMemoriesParams { query, limit }`) or `server.do_recall(...)` (which takes
  `RecallParams { query, scope_handle }` — **no `limit` field**; do not pass one). Reuse both methods
  directly rather than reimplementing ranking, and render the JSON result as an HTML table/list fragment
  (escaped).

**Step 3: Register routes**, run tests, commit.

```bash
git add crates/alexandria-mcp/src/debug/
git commit -m "feat(debug): add live query tester route"
```

---

## Task 12: Mount `/debug` router in the binary

**Files:**

- Modify: `crates/alexandria/src/main.rs`

**Step 1:** In `serve_http`, after building the MCP `service`, add:

```rust
let debug_router = alexandria_mcp::debug::router(server.clone());
let router = axum::Router::new()
    .nest_service("/mcp", service)
    .merge(debug_router);
```

(Adjust based on how `debug::router` is structured — if it already returns a full `Router` with routes at
absolute paths like `/debug`, `.merge()` is correct; if it expects to be nested at a prefix, switch to
`.nest("/debug", debug_router)` and change route paths in Task 6-11 to be relative, e.g. `""` and `"/memories"`.
**Pick one convention up front in Task 6 and use it consistently** — recommend absolute paths + `.merge()`
since it matches the design doc's literal route table.)

**Ordering pitfall — read before editing:** in the existing `serve_http`, `server` is moved into the MCP
`StreamableHttpService::new(move || Ok(server.clone()), ...)` closure. `alexandria_mcp::debug::router(server.clone())`
MUST be called and stored in a local (e.g. `debug_router`) **before** that `StreamableHttpService::new(...)`
call, the same way `maintenance_db = server.db.clone()` is already cloned out earlier in the function.
Calling `server.clone()` after the `StreamableHttpService::new` line will fail to compile because `server`
was already moved.

**Step 2:** Log the debug URL alongside the existing MCP log line:

```rust
tracing::info!("Alexandria ready, serving HTTP on http://{bind_addr}/mcp (debug UI at http://{bind_addr}/debug)");
```

**Step 3: Manual verification**

Run: `cargo run -p alexandria` with an http-transport config (or `ALEXANDRIA_CONFIG` pointing to a temp
TOML with `[server] transport = "http"`), then in another terminal:

```bash
curl -s http://127.0.0.1:3000/debug | head -20
curl -s http://127.0.0.1:3000/debug/memories | head -20
```

Expected: both return HTML with `200`, dashboard shows zeroed stats on a fresh DB.

**Step 4: Run full workspace test suite**

Run: `cargo test --workspace`
Expected: all tests pass, including the new debug ones and pre-existing suite (no regressions).

**Step 5: Commit**

```bash
git add crates/alexandria/src/main.rs
git commit -m "feat: mount debug web UI at /debug on HTTP transport"
```

---

## Task 13: Update README

**Files:**

- Modify: `README.md`

**Step 1:** Add a short "Debug Web UI" section documenting `http://<host>:<port>/debug`, what it shows,
and that it's unauthenticated/trusted-network-only — matching existing README section style/tone.

**Step 2: Commit**

```bash
git add README.md
git commit -m "docs: document debug web UI"
```

---

## Final check

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: clean. Then use the `requesting-code-review` skill before merging.
