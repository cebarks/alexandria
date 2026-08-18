# Debug Web UI — Design

## Purpose

Alexandria currently has no visibility into its own state short of raw SurrealQL queries. This adds a
read-focused (mostly) debug web UI so a human can inspect memories, clusters, heat, and graph edges, and
sanity-check retrieval/recall quality against the live embedding model — without a separate client.

## Scope (v1)

- Memory browser: search/filter/paginate `fact` rows, view full detail (heat, cluster, edges, lineage)
- Cluster explorer: list clusters with live member counts and cohesion, drill into members
- Graph view: N-hop neighbor visualization for a given memory (`memory_edge` + `contains_memory`)
- Live query tester: run `retrieve_memories` / `recall` against the real embedding model from a form

Out of scope for v1: auth, mutation from the UI (delete/edit stays MCP-tool-only), pagination beyond
simple offset/limit, mobile layout polish.

## Placement & serving

- New `debug` module inside `alexandria-mcp` (the only crate that already knows both `alexandria-engine`
  and `alexandria-storage`; keeps the architecture boundary in CLAUDE.md intact).
- Router assembled as `alexandria_mcp::debug::router(AlexandriaServer) -> axum::Router`.
- Mounted in `crates/alexandria/src/main.rs::serve_http` at `nest_service("/debug", ...)`, alongside the
  existing `/mcp` nest. Only present when `transport = "http"` (stdio mode has no HTTP server to mount on).
- No auth (matches current MCP posture — trusted network boundary). No new config keys needed for v1.

## Rendering approach

- Plain server-rendered HTML via small Rust string-building helpers (a `layout(title, body) -> String`
  wrapper + per-page render functions). No template engine dependency (askama/tera) — keeps the build
  simple and avoids a new dependency for a low-complexity HTML shape.
- [htmx](https://htmx.org) pulled from a CDN `<script>` tag for interactivity: search-as-you-type on the
  memory browser, click-to-expand rows, form submission without full page reloads for the query tester.
- Graph view is the one page needing real client-side rendering: a small inline `vis-network` (CDN) canvas
  fed by a JSON endpoint (`GET /debug/api/graph/:id`). Every other page is plain HTML/htmx swaps.
- All interpolated values HTML-escaped through one small `esc()` helper to avoid XSS from stored memory
  content when rendered back into the page.

## Data access additions

New read-only methods, kept in `alexandria-storage` (repos own all DB access):

- `MemoryRepo::list(search: Option<&str>, tag: Option<&str>, include_deleted: bool, limit, offset) -> Vec<Fact>`
  and a matching `count(...)` for pagination — plain `SELECT`/`WHERE content ~ $search` style filters.
- `MemoryRepo::cluster_for_fact(id) -> Option<Cluster>` — reverse traversal `<-contains_memory<-cluster`.
- `ClusterRepo::list_with_counts() -> Vec<(Cluster, usize)>`.
- A small `stats` module/query (fact/deleted/cluster/edge/raw counts, embedding model + dims from
  `system_config`) for the dashboard landing page.

No changes to existing write paths or MCP tool behavior — this is additive.

## Routes

| Route | Description |
| --- | --- |
| `GET /debug` | Dashboard: counts, embedding model/dims, data dir, cluster maintenance thresholds |
| `GET /debug/memories` | Paginated/searchable table (htmx live search), tag filter, include-deleted toggle |
| `GET /debug/memories/:id` | Detail: content, tags, confidence, heat/stability/access_count, cluster badge, edges, lineage |
| `GET /debug/clusters` | List with member counts, depth, live cohesion |
| `GET /debug/clusters/:id` | Member table |
| `GET /debug/graph/:id` | vis-network canvas, N-hop neighbors |
| `GET /debug/api/graph/:id` | JSON node/edge data backing the graph page |
| `GET /debug/query` | Query tester form |
| `POST /debug/query/run` | Runs `retrieve_memories`/`recall` live, htmx-swaps results table in place |

## Error handling

Every handler returns a rendered error fragment (not a raw 500) on DB/embedding failures, using the same
layout so broken states are still legible in the browser. Missing IDs render a 404 page rather than
panicking.

## Testing

- Storage-layer unit tests for the new `list`/`count`/`cluster_for_fact`/`list_with_counts` methods using
  `Database::connect_embedded()`, following existing repo test conventions.
- A couple of `axum::Router` integration tests (via `tower::ServiceExt::oneshot`) hitting `/debug` and
  `/debug/memories` against an ephemeral DB to confirm 200s and that a stored fact's content is escaped
  and rendered.
