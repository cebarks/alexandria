# Roadmap

**Current state:** 8 MCP tools, 116 tests, schema `v005`, HTTP + stdio + Docker deployment, one
crate per layer and two client-side pi integrations under `contrib/pi/`.

## Completed

### v0.1 — Core Foundation

- Embedded SurrealDB with in-memory storage
- Candle embedding provider (all-MiniLM-L6-v2, pure Rust)
- Ebbinghaus heat model with decay and stability
- Hierarchical clustering with cosine similarity
- Progressive recall (broad → focused with scope handles)
- 4 MCP tools: `store_memory`, `retrieve_memories`, `recall`, `delete_memory`
- 28 tests (see each milestone below for current counts)

### v0.2 — Production Readiness

- TOML config file with env var overrides
- Persistent storage via SurrealKV on disk
- Version-tracked schema migrations (forward-only `.surql` files)
- Embedding model safety check (refuses start on mismatch)
- Graph edges (`relates_to`, `supports`, `contradicts`, `derived_from`, `extracted_from`)
- Spreading activation (heat propagation along edges on retrieve)
- Cluster split/merge *detection* (execution landed later — see v0.2.5)
- 2 new MCP tools: `update_memory`, `import_document`
- HTTP transport via rmcp StreamableHttpService
- systemd deployment
- 64 tests

### v0.2.1 — Proactive Usage Nudges

A memory server only helps if agents actually reach for it unprompted. This milestone made
Alexandria nudge that behavior at the protocol level rather than relying on client-side prompting
alone:

- `ServerInfo.instructions` populated via `#[tool_handler(instructions = "...")]` — surfaced to
  any MCP-compliant client at `initialize` time with guidance on when to read vs. write memory
- All 6 tool descriptions and their param descriptions rewritten to be directive/trigger-keyed
  ("call this proactively when...") instead of purely mechanical
- 86 tests (was 64 — also reflects the v0.2.2 debug web UI milestone merged alongside)

Client-side companions (outside this repo, not shipped with the server): a pi `SKILL.md`
documenting trigger conditions, and an optional pi extension that auto-calls `retrieve_memories`
on every prompt via `before_agent_start`.

### v0.2.2 — Debug Web UI (2026-08-17)

The gap between "I stored something" and "I can see what the retrieval model actually did":

- Read-only Axum UI mounted alongside the MCP endpoint at `/debug` in HTTP mode
- Dashboard with live fact/cluster/edge/raw-document counts
- Memory list with search, filter, total count, and Prev/Next pagination
- Memory detail: heat, stability, timestamps, cluster membership, navigable edge and cluster links,
  deleted-row styling
- Cluster list and drill-down, plus a per-memory graph neighborhood view
- Query tester that runs `retrieve_memories` / `recall` against the real embedding model, so
  retrieval quality is checkable without writing a client
- All DB-sourced values HTML-escaped at render time (record IDs appear in `href` attributes,
  including percent-encoded ones)

Documented in the README "Debug Web UI" section; design and implementation plans in
`docs/plans/2026-08-17-debug-webui-*.md`.

### v0.2.3 — Proactive Capture / Auto-Store (2026-08-18)

Extended the pi extension from recall-only into a three-layer write funnel, so durable facts get
captured even when the agent doesn't think to store them:

- Heuristic detectors on user prompts — corrections, stated preferences
- Error→resolution pairing from tool results, flushed at `agent_end`
- Dedup buffer that also absorbs agent-initiated `store_memory` / `update_memory` calls
- LLM extraction pass at `session_shutdown` via pi's `ctx.modelRegistry` (cheap model, falls back
  to the session model), tagging output `extracted` and skipping `reload` shutdowns
- Fails open: no path in the extension can block an agent turn

See `docs/plans/2026-08-18-auto-store-{extension-design,implementation}.md` and
`contrib/pi/README.md`.

### v0.2.4 — XDG Config and Client Config File (2026-08-19)

- Server config → `$XDG_CONFIG_HOME/alexandria/config.toml`, data → `$XDG_DATA_HOME/alexandria/data`,
  with legacy `~/.alexandria/` fallback plus a startup migration warning
- New `$XDG_CONFIG_HOME/alexandria/client.toml` for the pi extension (`smol-toml`), so client tuning
  no longer requires env vars
- Added `server.sse_keep_alive_secs`, `cluster.maintenance_interval_secs`, `activation.top_n`
- `serial_test` for env-mutating config tests

### v0.2.5 — Cluster Maintenance Execution and Audit Log (2026-08-24)

Split/merge conditions were being *detected* in v0.2 but the actions were stubbed TODOs. This
finished the loop:

- `ClusterRepo`: `remove_member`, `delete`, `update_centroid`
- Split (k-means k=2) and merge execution wired into the maintenance task
- Maintenance drains **all** eligible merges per tick instead of one, so a backlog clears
- Fixed a bug that passed `member_count = 0` into `check_merge`, corrupting the weighted-centroid
  calculation
- `maintenance_log` table (`v004`) records every split/merge — action, source, targets, members moved
  — with a paginated `/debug/maintenance` view over it
- Integration tests for split/merge mechanics

### v0.2.6 — Session Memory (2026-08-27)

- `session` table and `contains_session_memory` relation (`v005`); sessions are created implicitly on
  first `store_memory(session_id)`
- `session_id` filter on `retrieve_memories` for within-session search
- New tools `get_session` and `finalize_session` (8 tools total), plus session guidance added to the
  MCP `instructions`
- Reference: `docs/session-memory.md` — including the limitations this milestone shipped with

### Infrastructure — Tooling, CI, Containers (2026-08-27 → 08-28)

- `justfile` as the single task runner; CI routes through it so local and CI commands can't drift
- `.githooks/pre-commit` (fmt check + clippy-as-errors), installed via `just install-hooks`
- Multi-stage `Dockerfile`: musl-targeted release binary (dynamically linked — `crt-static` is
  cleared because proc-macro and `cc`-based crates misbehave with musl's default static CRT) on an
  Alpine runtime as a non-root user, with a `/data` volume holding both the SurrealKV data dir and the
  HuggingFace model cache; configured entirely through `ALEXANDRIA_SERVER_*` env overrides
- Server config keys `server.transport` / `host` / `port` became env-overridable, which is what makes
  the image config-file-free

## Planned

### v0.2.x — Known Gaps From Shipped Work

Small, concrete, and already visible in the codebase — worth clearing before the next feature
milestone:

- **Session-scoped search ignores soft-deletes.** `SessionRepo::get_memories()` → `get_fact()` never
  checks `deleted = false`, unlike the unscoped path, so deleted memories surface in `get_session`
  and session-filtered `retrieve_memories`.
- **No session enumeration.** `get_session` needs an id you already know; there is no `list_sessions`,
  and `recall` walks clusters rather than sessions.
- **`session.agent_id` / `session.model` are dead columns.** Present in schema and model, never
  populated — either wire them to a tool parameter or drop them.
- **No tests for the pi extension.** The detector regexes and extraction prompt have no coverage, so
  a pattern edit is unguarded.
- **Extension does not use sessions.** Auto-store writes are ungrouped.
- **README said MIT.** Corrected to AGPL-3.0-or-later to match `LICENSE` and
  `license.workspace`; verified nothing else in-tree still claims MIT (`deny.toml`'s MIT entries are
  third-party license allow-listing, which is unrelated). If the GitHub repo's advertised license
  badge still reads MIT, that is an API-side setting, not a file.

### v0.3 — Self-Organizing Memory

The goal: memories should organize themselves without manual curation.

**Background enrichment pipeline**

- Async task queue with priority levels
- Extract → Relate → Consolidate stages
- Decouple heavy processing from the request path

**LLM-based relation discovery**

- Similarity pre-filter to find candidate pairs
- LLM verification to classify edge type (`supports`, `contradicts`, `relates_to`)
- Auto-create edges between related memories

**Cluster label generation**

- LLM-generated human-readable labels from member content
- Label staleness detection and regeneration as membership shifts
- Makes recall results actionable ("Authentication patterns" vs "cluster:abc123")

**OpenAI embedding provider**

- Feature-gated alternative to Candle
- Higher quality embeddings at API cost
- Model-agnostic — any OpenAI-compatible API

### v0.4+ — Scale & Multi-Tenancy

**Multi-tenancy**

- Namespace (org) → database (user) isolation via SurrealDB's native multi-db
- Per-tenant config overrides

**Performance**

- SurrealDB vector index for DB-side cosine similarity (matters at >10k memories)
- Full-text search index for keyword matching alongside semantic search
- Cluster heat caching with TTL
- Bulk heat maintenance sweep for untouched records

**Operational**

- ~~Embedding model migration CLI (`alexandria migrate-embeddings`)~~ Done 2026-09-08
- Cascade soft-deletes through lineage chains
- Scope handle expiry / TTL
- Metrics endpoint

**Additional embedding providers**

- Ollama (local, any model)
- Cohere, Voyage, Jina (API)
- GPU acceleration via Vulkan when candle ships it

## Ideas / Research

- **Two types of facts:** Known-true (imported, from docs) vs learned-over-time (agent-discovered). Different confidence defaults and decay curves.
- **Agent-centric provenance:** Each agent instance gets its own memory space, tagged with user interaction provenance
- **Cross-org federation:** Shared knowledge bases across organizations
- **SurrealDB fallback:** Postgres migration path if SurrealDB hits production blockers
- **Adaptive thresholds:** Per-org or per-domain cluster/heat tuning instead of global defaults
- **Multi-architecture models:** GTE, E5, and non-BERT models in the candle provider
