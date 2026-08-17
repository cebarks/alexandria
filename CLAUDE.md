# Alexandria — Agent Context

## SurrealDB 3.2 Gotchas (Critical)

These will bite you. SurrealDB 3.2 differs from docs and prior versions:

- `value` is a **reserved word** — use `SELECT * FROM table` not `SELECT value FROM table`
- `DELETE table WHERE ...` — no `FROM` keyword
- `RELATE` needs pre-parsed `RecordId` via `.bind()` — inline `type::record()` in RELATE fails
- `type::record()` replaces `type::thing()` (removed in 3.x)
- Query result structs need `#[derive(SurrealValue)]` from `surrealdb::types`
- `RecordId` formatting: use `record_id_to_string()` helper (in `server.rs`), not `.to_string()`
- Connection: `surrealdb::engine::any::connect("mem://")` with `kv-mem` feature; `surrealkv://path` with `kv-surrealkv`

## rmcp (MCP SDK) Patterns

- Uses `schemars` 1.x (not 0.8) — `#[schemars(description = "...")]` on tool param fields
- Tool macro: `#[tool(description = "...")]` inside `#[tool_router(server_handler)]` impl block
- Params: `Parameters(params): Parameters<MyParams>` — the wrapper is required
- HTTP transport: `transport-streamable-http-server` feature, `StreamableHttpService::new(factory, session_mgr, config)`

## Architecture Boundaries

- **storage** owns all DB access — no raw SurrealDB queries outside this crate
- **engine** is pure algorithms — no DB, no async (except test helpers). Takes data in, returns results.
- **pipeline** owns embedding — abstracts over providers via `EmbeddingProvider` trait
- **mcp** wires tools to engine+storage — the only crate that knows about both
- **alexandria** (binary) is config + transport + startup only

## Non-Obvious Patterns

- `record_id_to_string()` is the canonical way to format SurrealDB `RecordId` for use in queries and JSON responses. It's in `alexandria-mcp/src/server.rs` and is `pub`.
- Cluster `member_count` is queried live (not cached) — `load_cluster_infos()` calls `get_members()` per cluster.
- `update_memory` with content change: creates a soft-deleted snapshot of old content, then links via `derived_from` edge. The old version is hidden from search but preserved for lineage.
- `import_document` creates a `raw` table record for the full document, then `extracted_from` edges from each chunk to it.
- Spreading activation fires on the top 3 results of `retrieve_memories` — it's a side effect, not part of the ranking.
- Cluster maintenance runs as a background `tokio::spawn` every 5 minutes in HTTP mode only (not stdio).
- Schema migrations are forward-only, numbered (`v001`, `v002`, ...), tracked in `system_config` table.
- Embedding model is locked on first boot — changing `config.toml` model without wiping data will refuse to start.

## Testing

- All integration tests use `Database::connect_ephemeral()` (in-memory SurrealDB) — no disk state between tests.
- `CandleProvider` tests download the real model on first run (~80MB) — they're slow the first time.
- Test helpers in `alexandria-storage/src/connection.rs`: `connect_ephemeral()` for quick in-memory DB.

## Config Precedence

defaults → `~/.alexandria/config.toml` → `ALEXANDRIA_CONFIG` env var (path to alt TOML) → individual env vars (`ALEXANDRIA_SERVER_TRANSPORT`, etc.)
