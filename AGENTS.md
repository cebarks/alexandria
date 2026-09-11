# Alexandria — Agent Context

## SurrealDB 3.2 Gotchas (Critical)

These will bite you. SurrealDB 3.2 differs from docs and prior versions:

- `value` is a **reserved word** — use `SELECT * FROM table` not `SELECT value FROM table`
- `session` is a **reserved word** too — every session query needs backticks: ``SELECT * FROM `session` ``. The `session` table is `SCHEMAFULL`, so an undefined field fails rather than being stored.
- `$session` is a **reserved bind parameter name** (SurrealDB's own connection session). Use another name — `session_repo.rs` uses `$sess`.
- `DELETE table WHERE ...` — no `FROM` keyword
- `RELATE` needs pre-parsed `RecordId` via `.bind()` — inline `type::record()` in RELATE fails
- `type::record()` replaces `type::thing()` (removed in 3.x)
- Query result structs need `#[derive(SurrealValue)]` from `surrealdb::types`
- `RecordId` formatting: use `record_id_to_string()` helper, not `.to_string()`
- Connection: `surrealdb::engine::any::connect("mem://")` with `kv-mem` feature; `surrealkv://path` with `kv-surrealkv`

## rmcp (MCP SDK) Patterns

- Uses `schemars` 1.x (not 0.8) — `#[schemars(description = "...")]` on tool param fields
- Tool macro: `#[tool(description = "...")]` inside a `#[tool_router]` impl block. `#[tool_router(server_handler)]` auto-generates a bare `get_info()`; use bare `#[tool_router]` plus an explicit `#[tool_handler(instructions = "...")]` block on `impl ServerHandler` instead when the server needs to advertise `instructions` (see Non-Obvious Patterns below — `AlexandriaServer` does this).
- Params: `Parameters(params): Parameters<MyParams>` — the wrapper is required
- HTTP transport: `transport-streamable-http-server` feature, `StreamableHttpService::new(factory, session_mgr, config)`

## Architecture Boundaries

- **storage** owns all DB access — no raw SurrealDB queries outside this crate (the `alexandria-mcp` handlers still issue some inline queries directly; don't add new ones without reason)
- **engine** is pure algorithms — no DB, no async (except test helpers). Takes data in, returns results.
- **pipeline** owns embedding — abstracts over providers via `EmbeddingProvider` trait
- **mcp** wires tools to engine+storage — the only crate that knows about both. Also owns the debug web UI (`alexandria-mcp/src/debug/`), which is Axum handlers over the same repos.
- **alexandria** (binary) is config + transport + startup + the background cluster-maintenance task
- **contrib/pi** is client-side only — TypeScript, never compiled into or imported by the Rust server

## Non-Obvious Patterns

- `AlexandriaServer` uses a bare `#[tool_router]` + explicit `#[tool_handler(instructions = "...")]` block — NOT `#[tool_router(server_handler)]` — specifically so `get_info()` carries usage `instructions`. If you add a new tool, add it to the `#[tool_router]` impl block same as the others; the separate `#[tool_handler]` block stays where it is at the bottom of `server.rs` and doesn't need touching unless the overall usage guidance changes.
- Tool descriptions and param field descriptions (`#[tool(description = ...)]`, `#[schemars(description = ...)]`) are written directively ("call this proactively when...") rather than just describing mechanics — this materially affects how often client LLMs choose to call the tool unprompted. Keep new tools consistent with that style.
- `record_id_to_string()` is the canonical way to format SurrealDB `RecordId` for use in queries and JSON responses. It lives in `alexandria-storage/src/lib.rs` and is re-exported from `alexandria-mcp/src/server.rs`.
- There are **9 MCP tools**: `store_memory`, `retrieve_memories`, `recall`, `update_memory`, `import_document`, `delete_memory`, `get_session`, `list_sessions`, `finalize_session`. Adding one means a params struct in `alexandria-mcp/src/tools/`, a `#[tool]` method, a `do_*` impl, and a row in the README tool table.
- The HNSW index on `fact.embedding` is defined at boot by `schema::ensure_vector_index()`, not in a
  numbered migration, because HNSW needs `DIMENSION` at define time and the dimension comes from the
  locked embedding model. `MemoryRepo::nearest()` issues `embedding <|k,COSINE|> $q`, which goes
  through the index when present and falls back to a brute-force scan inside SurrealDB when it is
  not (most tests never define it). `do_retrieve_memories` and `bench-retrieval` both call it;
  the bench defines the index on its snapshot so its overlap line measures the index, not the
  fallback. `migrate-embeddings` drops the index before re-embedding because it rejects vectors of
  any other dimension; the next boot redefines it.
- Cluster `member_count` is queried live (not cached) — `load_cluster_infos()` calls `get_members()` per cluster, so it is one query per cluster. Fine at current scale, the first thing to revisit if cluster counts grow.
- `update_memory` with content change: creates a soft-deleted snapshot of old content, then links via `derived_from` edge. The old version is hidden from search but preserved for lineage.
- `import_document` creates a `raw` table record for the full document, then `extracted_from` edges from each chunk to it.
- Spreading activation fires on the top N results of `retrieve_memories` (configurable via `activation.top_n`, default 3) — it's a side effect, not part of the ranking.
- `retrieve_memories` drops results below `retrieve.min_similarity` (default 0.10) server-side, *after* ranking and *before* activation is triggered. It is a noise cutoff only — the client threshold (`[recall] min_similarity`) does the real filtering, paired with the client `limit` — both measured, not chosen, and neither readable without the other. Do not restate any of those numbers here; `docs/minilm-test-data.md` records the measurements and `docs/configuration.md` the rationale.
- Cluster maintenance runs as a background `tokio::spawn` in HTTP mode only (not stdio), at an interval configurable via `cluster.maintenance_interval_secs` (default 300s / 5 minutes). It drains **all** eligible merges per tick, not one.
- Every split/merge is recorded in the `maintenance_log` table (`v004`) and surfaced at `/debug/maintenance`. If cluster behavior looks wrong, that table is the audit trail.
- Sessions are created implicitly by `store_memory(session_id)` — there is no create tool. `SessionRepo::touch()` bumps `memory_count` **and** `ended_at`, so `ended_at` means last-activity; only a non-null `summary` distinguishes a finalized session. See `docs/session-memory.md`.
- Known inconsistency: session-scoped retrieval walks edges through `SessionRepo::get_memories()` → `MemoryRepo::get_fact()`, which does **not** filter `deleted = false` the way the unscoped path does. Soft-deleted memories therefore still surface in `get_session` and `retrieve_memories(session_id: ...)`. Not yet fixed — don't document it as intended behavior.
- Schema migrations are forward-only, numbered (`v001`, `v002`, ...), tracked in `system_config` table. Current head is `v005_session.surql`.
- Embedding model is locked on first boot — changing `config.toml` model without wiping data will refuse to start.

## Testing

- The repo intentionally carries no `rust-toolchain.toml` and no `.cargo/config.toml` — `rust-version = "1.98"` + edition 2024 are the only compiler statement; mold/`target-cpu` live in each dev's `~/.cargo/config.toml` (see TODO-misc "Build / toolchain"). Don't re-add them to the repo.
- Use the `just` recipes (they match CI): `just test`, `just lint`, `just fmt`, `just ci` (fmt + lint + test + `cargo deny`). `just install-hooks` wires `.githooks/pre-commit`.
- Run tests on **stable**, not nightly: `diskann-wide` (SurrealDB transitive dep) fails trait inference on its NEON intrinsics under recent nightlies on aarch64, and the failure looks like it originates in this workspace. Current suite: 151 tests, all green.
- All integration tests use `Database::connect_embedded()` (in-memory SurrealDB) — no disk state between tests.
- `CandleProvider` tests download the real model on first run (~80MB) — they're slow the first time.
- Test helpers in `alexandria-storage/src/connection.rs`: `connect_embedded()` for quick in-memory DB.
- Env-mutating config tests must carry `#[serial]` (`serial_test`) — `cargo test` runs them in parallel within a binary and they otherwise race.
- The pi extension under `contrib/pi/` has **no** test suite; detector regexes are unguarded.

## CI

- Actions are pinned by full commit SHA with a trailing `# vN` comment, and Dependabot
  (`.github/dependabot.yml`, weekly, 7-day cooldown) proposes bumps.
- Failure signature of a dead pin: a job dies in **"Set up job"** after a few seconds with
  `Unable to resolve action <owner>/<repo>@<sha>, unable to find version`. Everything using that pin
  fails identically, and no recipe ever runs. Fix = repin to the commit the tag names now
  (`gh api repos/<owner>/<repo>/git/ref/tags/v2`), not to a branch head.
- Triage by duration before reading logs: a real `ci.yml` job is ~2–7 min. A run that concludes in
  10–40s failed in setup, which means infrastructure, not code.
- The separate `Container` workflow (`.github/workflows/container.yml`) is path-scoped to
  `Dockerfile`/`Cargo.*`/`crates/**` and legitimately takes far longer than the rest of CI — it does a
  cold release build of the workspace inside the image with no layer cache. Do not apply the 10–40s
  duration heuristic to it, and do not expect it to appear on docs-only pushes.
- Do not test whether a pin is reachable with `gh api repos/<owner>/<repo>/commits/<sha>` — it
  returns 422 "No commit found" for pins that Actions resolves fine. Trust the Actions error text, or
  the fact that a job using that pin passed.
- Nothing watched the branch while CI was red for 11 days, because no check is required to merge.
  Revisit branch protection if regressions keep landing.

## Docs Map

- `README.md` — feature/tool overview, quick start, deployment (systemd, Docker), debug UI
- `docs/configuration.md` — every server and client config key, env overrides, XDG migration
- `docs/session-memory.md` — session data model, lifecycle, tool semantics, current limitations
- `docs/minilm-test-data.md` — retrieval measurements for the embedding model, how to rerun
  `alexandria bench-retrieval`, metric definitions, and the frozen question set
- `docs/roadmap.md` — shipped milestones and planned work
- `docs/plans/` — dated design/implementation plans for completed work (historical, not maintained)
- `contrib/pi/README.md` — how the pi skill and extension differ and install
- `AGENTS.md` — this file. It was named `CLAUDE.md` until the docs sweep that added session memory
  and the pi extension docs, so older `docs/plans/*` references to `CLAUDE.md` point here.

## Config Precedence

### Server

defaults → `$XDG_CONFIG_HOME/alexandria/config.toml` → `ALEXANDRIA_CONFIG` env var (path to alt TOML) → individual env vars (`ALEXANDRIA_SERVER_TRANSPORT`, etc.)

Legacy path `~/.alexandria/config.toml` is used as fallback if the XDG path doesn't exist.

Data defaults to `$XDG_DATA_HOME/alexandria/data` (was `~/.alexandria/data`).

### Client (Pi extension)

defaults → `$XDG_CONFIG_HOME/alexandria/client.toml` → `ALEXANDRIA_CLIENT_CONFIG` env var → individual `ALEXANDRIA_*` env vars

The extension mirrors the Rust `dirs::config_dir()` behavior: `~/Library/Application Support/alexandria/client.toml` on macOS, and `XDG_CONFIG_HOME` still wins on any platform when set.

## License

AGPL-3.0-or-later (`LICENSE`, `license.workspace` in `Cargo.toml`). Chosen over MIT because Alexandria is a long-running network service — keep new crates on `license.workspace = true`.
