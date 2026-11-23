# Alexandria — Agent Context

## SurrealDB Query and Storage Gotchas (Critical)

The current official engine family is **4.0.1** (protocol 0.14.1, backend SurrealKV 1.2.0).
The query traps below were identified on 3.3; historical version references are observations,
not the current pin. Storage tests run on the current engine. Follow
`docs/surrealdb-4-upgrade.md` for the tested Linux copy-first 3.3 conversion: never engine-open
the original/backup with 4.x, explicitly admit conversion only on a disposable copy, rebuild HNSW,
and verify application identity/data/vectors/centroids and the real index plan. Rollback is the
untouched 3.3 backup plus old binary, never a 4.x-touched target, even at Legacy feature level.
`surreal start` forwards `SURREAL_EXPERIMENTAL_UPGRADE_FROM_3X`; embedded `surreal sql` does not
read that datastore environment option. Application startup must not globally opt in or blindly
rebuild indexes. Tests alone use explicit copy-scoped SDK `datastore_option` admission.

Historical 3.3 query observations (retained where the shipped query still relies on them):

- `value` is a **reserved word** — use `SELECT * FROM table` not `SELECT value FROM table`
- `session` is **not** a reserved word, whatever the backticks in the repo's queries suggest: unquoted `SELECT * FROM session`, `CREATE session:1` and `UPDATE session … WHERE` work on 3.2.4 and 3.3.0, and `v006` runs `UPDATE session UNSET …` unquoted. The backticks are harmless. The `session` table is `SCHEMAFULL`, so writing an undefined field is an error rather than being stored.
- `$session` is a **reserved bind parameter name** (SurrealDB's own connection session). Use another name — `session_repo.rs` uses `$sess`.
- `DELETE table WHERE ...` and `DELETE FROM table WHERE ...` both parse and commit on the then-pinned 3.3.0 engine, and the repo uses each (`system_config.rs:set_config` omits `FROM`). A spelling choice, not a constraint
- `RELATE` needs pre-parsed `RecordId` via `.bind()` — inline `type::record()` in RELATE fails
- `type::record()` replaces `type::thing()` (removed in 3.x)
- Query result structs need `#[derive(SurrealValue)]` from `surrealdb::types`
- `RecordId` display/JSON formatting: use `record_id_to_string()`, not `.to_string()`. **It is not a parser-safe SQL round trip**, especially for numeric or quoted opaque keys. Bind typed `RecordId`/`RecordIdKeyRange` for queries; where an existing string API requires a SQL record literal, use `ToSql::to_sql()`.
- Connection: `surrealdb::engine::any::connect("mem://")` with `kv-mem` feature; `surrealkv://path` with `kv-surrealkv`
- To count filtered records reached by a graph traversal *inline in a SELECT*, the `WHERE` goes **inside** the traversal target's parentheses: `(->contains_session_memory->(fact WHERE deleted = false)).len()`. The obvious `(->edge->fact WHERE ...).len()` fails with `Unexpected token WHERE expected delimiter )`, and so do the `array::len(...)` and `count(...)` spellings — no N+1 fallback is needed once you find this.
- An absent `option<T>` field is `NONE`, and **`IS NOT NULL` and `!= NULL` are both satisfied by `NONE`** — they filter nothing; only `field != NONE` (or `NOT (field = NONE)`, or `type::is_none(field) = false`) excludes it. Verified against the then-pinned 3.3.0 engine: with one `NONE` and one set row, `WHERE next_due_at IS NOT NULL` returned both and `WHERE next_due_at != NONE` returned one. This bites ordering too, because `NONE` sorts below every datetime: such a row wins the head of an `ORDER BY next_due_at ASC LIMIT n` page.
- 3.3 rejected `ORDER BY x DESC NULLS LAST` and `ORDER BY type::coalesce(a, b) DESC`. Do not assume all expression sorts are still rejected: 4.0 accepts `ORDER BY array::len(tags)`, and `FactSort::TagCount` now uses it directly without an alias projection. The closed literal allowlist and unique tie-break remain. The shipped date sorts still rely on tested NULL placement, not those historical parse errors: descending parks NULL at the tail; paginated ties need a unique secondary key (see `SessionRepo::list`).
- 4.0's `array::group` deduplicates across grouped rows. Both `top_tags` paths project only visible live rows' tag arrays and count occurrences in Rust, preserving repetitions with one query. Keep the caller and live/quarantine predicates in SQL, the descending count/name tie-break and the cap.
- 4.0 renders the bound cluster member graph lookup inside an EXPLAIN `Values` expression, not a separate `GraphEdgeScan` node. Verify the bound cluster, traversal and fact-side partition conjunct and reject `TableScan`; EXPLAIN is not a count of adjacency keys visited.

- Typed cursor paging uses a bound `RecordIdKeyRange` in `FROM $records`: `WHERE id > $cursor` instead plans as a repeated `TableScan`, even with `ORDER BY id LIMIT n`. `reembed_page_plan_seeks_a_bound_record_range_not_a_repeated_table_scan` pins the `RecordIdScan` plan.
- `SELECT * OMIT embedding FROM $records` is the projection used by metadata attestation; omit vector fields in SQL rather than fetching them and stripping them in Rust.
- `INFO FOR TABLE fact STRUCTURE` returns an indexes array whose canonical HNSW SQL uses `DIST COSINE`, even though definition code may use `DISTANCE COSINE`. Check declared dimension/distance **and** numeric-ef `KnnScan`: a wrong-dimensional empty HNSW index can still produce an index plan.
- The persistent engine truncates root `LOCK` before acquiring its own lock, including on a rejected open. Never engine-open a source as an offline-lock probe; the Linux-tested copy guard opens an existing regular `LOCK` read-only and uses `try_lock()`. Exclude only root `LOCK` from payload attestations, not nested files named `LOCK`.

## rmcp (MCP SDK) Patterns

- Uses `schemars` 1.x (not 0.8) — `#[schemars(description = "...")]` on tool param fields
- Tool macro: `#[tool(description = "...")]` inside a `#[tool_router]` impl block. `#[tool_router(server_handler)]` auto-generates a bare `get_info()`; use bare `#[tool_router]` plus an explicit `#[tool_handler(instructions = "...")]` block on `impl ServerHandler` instead when the server needs to advertise `instructions` (see Non-Obvious Patterns below — `AlexandriaServer` does this).
- Params: `Parameters(params): Parameters<MyParams>` — the wrapper is required
- HTTP transport: `transport-streamable-http-server` feature, `StreamableHttpService::new(factory, session_mgr, config)`

## Architecture Boundaries

- **storage** owns DB access. The one remaining production raw-SQL location outside it is the bare `CREATE provenance` in the unowned compatibility arm of `alexandria-mcp::server::store_body`; scoped writes use the journal instead. The former second exception, `dreaming::Jobs::all_clusters`, now delegates to `ClusterRepo::legacy_maintenance_clusters`. No new runtime exception was found; fixture/test queries are not precedent.
- **engine** is pure algorithms — no DB, no async (except test helpers). Takes data in, returns results.
- **pipeline** owns embedding — unchanged `EmbeddingProvider` trait, legacy CPU Candle and native Granite/Burn behind the shared `load_provider` factory. The binary forwards optional `granite-rocm`/`granite-vulkan` features; a device selector does not compile a backend. The model lives on one named worker thread, not in the MCP server.
- **mcp** wires tools to engine+storage. It is not the only crate that sees both — the binary depends on engine *and* storage, and `main.rs:serve_http`'s maintenance task combines them — but it is the only one whose *purpose* is joining them. Also owns the debug web UI (`alexandria-mcp/src/debug/`), which is Axum handlers over the same repos, plus `crates/alexandria-mcp/templates/` (askama, compiled at build time) and `crates/alexandria-mcp/assets/` (vendored third-party JS, `include_bytes!`). Both are **compile-time contracts**: a missing template or asset file is a build error, not a runtime 404. It also owns the top-level `/health` liveness endpoint (`health.rs`): plain JSON, no templates, 200 only when `Database::ping` round-trips, merged into the binary's router next to the MCP service.
- **alexandria** (binary) is config + transport + startup + the background cluster-maintenance task
- **contrib/pi** is client-side only — TypeScript, never compiled into or imported by the Rust server
- **contrib/hermes** is client-side only — Python, never compiled into or imported by the Rust server. It does not mirror native Hermes memory writes. Alexandria-only curated memory uses Hermes' `memory.memory_enabled = false` and `memory.user_profile_enabled = false` while keeping the `memory` toolset enabled.

## Non-Obvious Patterns

- `AlexandriaServer` uses a bare `#[tool_router]` + explicit `#[tool_handler(instructions = "...")]` block — NOT `#[tool_router(server_handler)]` — specifically so `get_info()` carries usage `instructions`. If you add a new tool, add it to the `#[tool_router]` impl block same as the others; the separate `#[tool_handler]` block stays where it is at the bottom of `server.rs` and doesn't need touching unless the overall usage guidance changes.
- Tool descriptions and param field descriptions (`#[tool(description = ...)]`, `#[schemars(description = ...)]`) are written directively ("call this proactively when...") rather than just describing mechanics — this materially affects how often client LLMs choose to call the tool unprompted. Keep new tools consistent with that style.
- `record_id_to_string()` is the canonical display/JSON formatter for SurrealDB `RecordId`, in `alexandria-storage/src/lib.rs` and re-exported by `alexandria-mcp/src/server.rs`. It is **not** canonical query serialization. Migration binds actual typed IDs/ranges; existing cluster string APIs receive `id.to_sql()` so numeric and quoted opaque keys retain their meaning.
- There are **17 MCP tools**: `store_memory`, `retrieve_memories`, `recall`, `update_memory`, `import_document`, `delete_memory`, `get_session`, `list_sessions`, `finalize_session`, `set_reminder`, `check_reminders`, `list_reminders`, `cancel_reminder`, `resolve_project`, `list_projects`, `list_unassigned_memories`, `propose_memory_scope`. Adding one means a params struct in `alexandria-mcp/src/tools/`, a `#[tool]` method, a `do_*` impl, and a row in the README tool table. `PUBLIC_TOOLS` in `server.rs` is the closed list and `tests/scope_tools_test.rs` asserts it against `tools/list`.

- **Project scope lives at three boundaries and nowhere else.** `alexandria_engine::project_identity` is the pure normalization rule (SSH/HTTPS equivalence, default ports folded, path case preserved, credentials/query/fragment/traversal/absolute-local-path refused); `alexandria_storage::repos::ScopedProjectRepo` is the only workspace-qualified **alias resolver** and the only resolve-or-create write (project *assertions* also exist in `AccessRepo::owned_project`/`owned_projects` and `ClassificationRepo::assert_owned_project`, which validate a handle a caller already holds rather than resolving a name); `alexandria_mcp::scope_service::ScopeService` is where a caller's words become a handle. Storage never imports the engine and never parses a remote; the MCP layer never builds an `OwnerContext`. `recall`'s cluster reads carry the same `ScopeFilter` as the fact reads, so a candidate cluster surfaces only members inside the caller's resolved population and a cluster with none is neither offered nor replayable — owned cluster maintenance also partitions candidates before computing changes. `resolve_project` is the only path that can create a project, and `propose_memory_scope` is a judgement with no application path — the fact's scope columns are asserted unchanged on both layers. `do_retrieve_memories_dry` and `do_retrieve_memories_unfiltered` are **not** tools — they are non-`#[tool]` wrappers over `retrieve_core(params, options)` used only by the debug Query Tester. Do not count them as tools, advertise them, or add a `dry_run` field to `RetrieveMemoriesParams`: that struct is the public MCP schema every LLM client sees, and a debug-only knob there would be an affordance agents could set.
- **Scoped cluster counts have two different meanings and are not interchangeable.** `workspace_member_count` (whole-workspace live membership, reached through `list_with_counts`) is the centroid running average's `n` in `assign_to_cluster_and_update`; `get_members` (partition-filtered) is recall eligibility, and `scoped_clusters_with_members` recounts from the members it loaded. Passing the population count to `update_centroid` corrupts mixed clusters — at `n = 0` it replaces the whole stored centroid with the new embedding — and is pinned by `a_cross_partition_store_updates_the_cluster_centroid_from_workspace_membership`. Do not “tidy” the write-path count into the population count; retrieval eligibility and centroid bookkeeping remain different concerns.
- The HNSW index on `fact.embedding` is defined at boot by `schema::ensure_vector_index()`, not in a
  numbered migration, because HNSW needs `DIMENSION` at define time and the dimension comes from the
  locked embedding model. Only the numeric-ef form reaches it: `MemoryRepo::nearest_indexed()` issues
  `embedding <|k,150|> $q` (plan `KnnScan`), while `MemoryRepo::nearest()` issues
  `embedding <|k,COSINE|> $q`, which is a brute-force scan (`KnnTopK`) whether or not the index
  exists. `bench-retrieval` calls `nearest_indexed` too, after defining the index on its snapshot,
  so its overlap line measures the index. `<|k,ef|>` without an index **errors on 4.0** (3.3
  silently returned no rows; 3.2 stripped it to an unranked plain scan). The positive-control
  regression is `nearest_indexed_without_an_index_errors_and_with_one_returns_rows`.
  `do_retrieve_memories` still picks by
  `AlexandriaServer::vector_index`, which `main.rs` sets only when the define succeeded — a failed
  define logs an error and boots on the brute-force path (most tests never define the index and
  take that path too). `test_nearest_indexed_plan_uses_the_hnsw_index` asserts the plan, because
  results cannot tell the two apart. `migrate-embeddings` drops the index before re-embedding
  because it rejects vectors of any other dimension; the next boot redefines it.
- Cluster `member_count` is queried live (not cached) — `load_cluster_infos()` calls `get_members()` per cluster, so it is one query per cluster. Fine at current scale, the first thing to revisit if cluster counts grow.
- The dashboard's cluster-health rollup costs **~2 queries per cluster per render** (`get_members` + the stored centroid from `list_with_counts`), and is flagged `TODO(debt)` in code. Same revisit trigger as above.
- **Owned migration preserves deleted-only clusters**, via `ClusterRepo::get_reembed_members`: prefer live members; with no live members, allow only existing deleted/non-quarantined fact targets and outgoing `contains_memory` edges, rejecting every other incoming/outgoing reference. Baseline admission, centroid rewrite and final verification share this population; do not change normal `get_members` to include hidden facts. Only truly empty/unreferenced clusters are removed.
- `update_memory` with content change: creates a soft-deleted snapshot of old content, then links via `derived_from` edge. The old version is hidden from search but preserved for lineage.
- `import_document` creates a `raw` table record for the full document, then `extracted_from` edges from each chunk to it.
- Spreading activation fires on the top N results of `retrieve_memories` (configurable via `activation.top_n`, default 3) — it's a side effect, not part of the ranking.
- `retrieve_memories` drops results below `retrieve.min_similarity` server-side, *after* ranking and *after* the `limit` truncation, and *before* activation is triggered — so a result the Query Tester reports as "dropped" was ranked inside the requested window, not suppressed from the whole database. The default is defined once at `alexandria_engine::search::DEFAULT_MIN_SIMILARITY`; read that constant, not this file, for the number. `RetrieveConfig::default()` and `AlexandriaServer::new`'s fallback both derive from it (they used to diverge — 0.10 vs 0.30 — which made a test-built server filter differently from production). It is a noise cutoff only — the client threshold (`[recall] min_similarity`) does the real filtering, paired with the client `limit` — both judgement calls read off `bench-retrieval`'s grid, and neither readable without the other. Do not restate any of those numbers here; `docs/minilm-test-data.md` records the measurements and `docs/configuration.md` the rationale. `cluster.cohesion_floor` follows the same pattern via `alexandria_engine::clusters::maintenance::DEFAULT_COHESION_FLOOR`.
- **Five independently due housekeeping jobs**, HTTP only: `sweep`, `cluster`, `merge`, `collapse`, `appraise`. `dream` runs their same dispatcher on demand; bare `dream` and `--job all` never call inference. Cluster/split and merge have owned partition bodies and a gated legacy path for wholly unowned populations. Mixed ownership is not repaired automatically; Unassigned is outside housekeeping.
- **Global per-job write allowances, not read limits.** Each capped job shares `max_rows_per_run` across active-account assigned partitions. Sweep/appraise charge selected rows, collapse charges retirements; reserve before dispatch, refund verified unused work only. Durable progress rotates workspaces/partitions; appraise also carries a row cursor across passes. Cluster/merge are not capped by this key.
- **Four housekeeping jobs emit audit summaries**: cluster, merge, collapse and appraise. Owned semantic changes pass through the journal kernel, with private snapshots and linked maintenance summaries carrying `system:dreaming` attribution. `/debug/maintenance` is admin-only and aggregate; owners read their own snapshots through `/debug/operations`.
- **Refinement has a separate enqueue cadence**, not a sixth housekeeping job. The HTTP scheduler requests assigned-scope work at `[refinement].interval_secs` when dreaming is enabled and routes are configured. A separate worker processes durable fair queue/cursor state under current account, consent, policy and attempt limits. Manual Unassigned work is human-only; browser enqueue is not exposed. See `docs/ownership-and-refinement.md`.
- **`Sweep` writes no audit rows** and `sweep` is deliberately absent from the `models::maintenance::job` allowlist. Materialising a decayed heat value is a proven no-op on ranking (every reader projects heat itself), so logging it would add up to `max_rows_per_run` rows an hour recording that nothing happened and bury the collapses and demotions that are the reason the table exists. Its observability is the `JobReport` at debug level. Don't "fix" this asymmetry.
- **The access clock and the decay anchor are two columns and must stay two.** `heat_state.last_touched` anchors the stored `heat` value and moves on every materialisation (access, activation warm, sweep); `heat_state.last_accessed_at` is the spacing reference and moves **only** on a real access. `HeatRepo::record_access` is the only writer allowed to touch the second one; `materialize_heat`/`materialize_heat_many` and `add_heat` must not. Sharing one field makes an hourly sweep the spacing reference, capping the ratio at ~0.042 and under-growing stability ~24x.
- **Retrieval records an access for every row it returns to the caller** (audit A2, wired direction), via one bulk read plus one batched write issued *after* the results are ranked and truncated and *before* the response is built — an access resets heat to 1.0 and activation adds to it, so the reverse order loses the access. A returned row with **no** `heat_state` (anything written through `create_fact` directly, or stored before `do_store_memory` began creating rows) is created in a second batch, not per row, so the statement count stays fixed whatever the corpus looks like — `HeatRepo::create_for_memory_many`, and the reason the older "two round trips regardless of limit" wording was only true for corpora whose rows all already had heat. Cost is ~2ms at 1000 rows and flat, because `idx_heat_state_memory` (v008) exists; without it the same batch cost 122ms. Concurrent retrieves can still lose an increment (no transaction) — accepted and recorded in A2, not an oversight to rediscover. What is **not** accepted is the sweep clobbering an access: `materialize_heat_many` claims on the anchor it read (`AND last_touched = $seen`), because that write re-dates the row and makes the loss permanent rather than transient.
- **Quarantine's live predicate is `quarantined_at = NONE`, never `IS NOT NULL` or `!= NULL`** — on this engine both are satisfied by `NONE` and filter nothing (`quarantine_none_predicate_pins_the_trap`). Nine read paths carry it, and `MemoryRepo::list`/`count`/`top_tags`/`collapse_candidates`/`live_confidences` all build their WHERE clause from one `fact_filter_clause` so a tenth cannot be forgotten. Quality quarantine writes the column through the mutation kernel; classification does not clear it. `quarantine_predicate_keeps_the_hnsw_index_plan` exists because results cannot distinguish an index scan from a brute-force one.
- **`all_ids_and_content` includes soft-deleted rows on purpose** (`migrate-embeddings` must re-embed them). It is the wrong read for any live-corpus pass — `collapse_candidates` is the live one.
- **Owned collapse commits lineage, soft deletion and journal atomically.** The legacy compatibility path must still write lineage before retiring a fact.
- **Appraise projects heat itself** rather than reading the stored value, so it has no ordering contract with the sweep. Its rule is conjunctive (cold AND never accessed AND confidence at or below the ceiling) and exempts the value it writes, so a daily job cannot keep lowering — or keep logging — the same row.
- **Session handles are account-qualified** (v010): the same external ID can exist for different owners, and unscoped compatibility repos are not hosted paths. `store_memory(session_id)` creates/links implicitly; `ended_at` means last activity and a non-null summary is the finalized signal. Scoped counts and relevance use the resolved population and withhold summary/tags. Explicit account-private archive exposes summary/tags, not cross-scope fact bodies. No stored `memory_count` (v006). See `docs/session-memory.md`.
- Reminders are evaluated purely at query time — there is NO background timer (`grep -n tokio::spawn` in `crates/` finds the dreaming scheduler's task in `crates/alexandria/src/dreaming.rs`, HTTP-only, plus the spawned consumers inside `reminders_test.rs`'s concurrency test). Due-ness is `status = 'pending' AND next_due_at <= now()` (`ReminderRepo::list_due`), so delivery is lazy, works identically in stdio and HTTP mode, and downtime costs nothing but a later delivery. The cost: **no client means no delivery**, and the server itself reaches nobody. `set_reminder`'s description says so explicitly; do not soften it back to 'delivered to the user'.
- `check_reminders` is the only consumer: it claims each delivered row with a conditional UPDATE (`status = 'pending' AND next_due_at = <seen>`, `ReminderRepo::record_delivery`), so a lost race skips the row rather than double-delivering; delivery is best-effort-once (a won claim whose response is lost drops that fire). Recurring rows advance to the first fire after now, coalescing skipped occurrences into `missed_occurrences`, plus `missed_occurrences_saturated` once the walk hits `MAX_ITER` — a capped count published as an exact one is the failure mode that test now pins. The repo-layer claim tests are sequential (stale `seen` → false); the actual concurrency claim is pinned by `concurrent_checks_deliver_a_due_row_exactly_once`, which spawns four real consumers, and by nothing else — do not delete it as 'slow'.
- The fold regression, for the record, because it looks impossible from the doc comment: `cron` applies its strict-after bound to the anchor's *local* fields, and inside a fall-back fold a check anchored in the second pass got back the fold's *earlier* instant — which equals the `next_due_at` just consumed, so the conditional claim succeeded, `delivered_count` bumped, and every prompt re-delivered the same reminder for the rest of the fold. No concurrency required. Pinned by `dst_fold_second_pass_never_returns_a_past_fire`, which anchors inside the fold; the pre-existing DST tests all anchored outside it, which is why 41 green tests said nothing.
- Recurring evaluation is wall-clock, and **one wall-clock reading is one occurrence**. Inside a fall-back fold `cron` yields the same local time twice (`LocalResult::Ambiguous(earlier, later)`, earlier first) and a spring-forward time never appears. `fires_after()` dedupes by `naive_local()` **and** filters the converted instant `> after`; both are needed, because `cron` applies its own strict-after bound to the anchor's *local* fields, so a check anchored in the fold's second pass otherwise hands back the earlier (already consumed, already past) instant — and that value written to `next_due_at` equals the `seen` the claim guards, so the claim succeeds and every check in the fold re-delivers the same reminder with no concurrency involved. `next_fire`, `upcoming` and `occurrences_between` all walk that single stream so the set-time preview, the advance and the coalescing count cannot disagree. Pinned by `dst_fold_second_pass_never_returns_a_past_fire` and `dst_fall_back_repeated_wall_time_delivers_once`.
- This crate's `cron` (0.17) **intersects** day-of-month with day-of-week where Vixie cron unions them, so `0 9 13 * FRI` is Friday-the-13th-only. `cron_dom_dow_conflict()` detects it, `do_set_reminder` reports it through the response `warning`, and the caveat is in the `cron` param description and both SKILL.md copies: an agent writing cron from natural language otherwise produces a schedule that validates, fires, and means something else. (The numeric `1=Sunday` dow numbering is a second, independent trap — `0` is rejected.)
- `concurrent_checks_deliver_a_due_row_exactly_once` (in `crates/alexandria/tests/reminders_test.rs`) spawns four real racing consumers. The repo-layer `record_delivery_claim_*` tests only re-issue a stale `seen` sequentially — they pin the WHERE clause, not the serialization — so don't read them as the concurrency proof, and don't drop the spawned one when the claim shape changes.
- `missed_occurrences` saturates at the engine's `MAX_ITER` (10 000) and ships `missed_occurrences_saturated` beside it. A capped count published as an exact number is wrong in a way no reader can see; the pi companion renders it as `10000+` because of that.
- The `due_reminders` piggyback on `retrieve_memories`/`recall` is read-only and untargeted: up to `DUE_REMINDERS_CAP` (5) due reminders, oldest-due first, regardless of project targeting, each entry carrying its `target` — entries for another project are informational until `check_reminders` delivers/escalates them; targeting, escalation, and coalescing live there alone. The authenticated path is account-bound through `ScopedReminderRepo::due_sample`; held/unresolved legacy targets are excluded in SQL before LIMIT, as are missing due times (`next_due_at != NONE`). The interim unscoped debug path uses `ReminderRepo::list_due_sample`. The cap is enforced in SQL rather than by truncating a full read in Rust, because this runs on every prompt; `DUE_SAMPLE_SLACK` over-reads so one undeliverable row cannot cost a slot.
- `[reminders].escalation_hours` has exactly one default, `alexandria_engine::reminders::DEFAULT_ESCALATION_HOURS`, derived into both the binary's `RemindersConfig::default()` and the MCP `RemindersSettings::default()` and guarded by `test_server_fallback_defaults_match_config_defaults` — the same rule that came out of the `min_similarity` drift. `main.rs` additionally refuses to start on a window too large to represent as a duration, because the delivery path's fallback is to hold project reminders forever.
- Naive datetimes in `set_reminder` are interpreted in `[reminders].timezone` (empty = system-local via iana-time-zone at startup, UTC if detection fails); explicit ISO-8601 offsets always win, and a nonexistent (spring-forward) or ambiguous (fall-back) local time is rejected at *set* time for one-shots, each with its own message naming the fix. Recurring wall times inside a fold are **not** rejected — they deliver once (see the `fires_after` bullet). `RemindersSettings::default().tz` is UTC, not system-local: this crate has no `iana-time-zone` dependency, `main.rs` resolves the operator's zone and injects it for both transports, and the fallback exists so a debug or test-built server stays deterministic.
- Every UTC datetime in reminder tool responses renders via `rfc3339_utc` (`server.rs`): Z-suffixed, seconds precision — `to_rfc3339()` would emit `+00:00`. `next_due_at_local` is the deliberate exception: the same instant spelled in the configured timezone for human confirmation.
- The `schedule_kind` discriminator strings (`once`/`pattern`/`cron`) are constants in `alexandria_storage::models::schedule_kind`, mirrored by the v007 schema `ASSERT`; writers and the engine's `spec_from_reminder` match on the constants so the sides can't drift.
- `status` has **three** values, not the two the original design specified: `pending | delivered | cancelled`. `'delivered'` exists because a consumed one-shot gets a NULL `next_due_at`, and `NULL <= $now` still matches in SurrealQL — so NULL cannot mark "never fires again" and an explicit terminal value is the only honest option. It is asserted in the v007 schema and filtered on by `list_reminders`.
- `set_reminder` returns `next_fire_preview` whose **first element *is* `next_due_at`**, not a separate list alongside it. The design asked for "next_due_at plus the next 3"; this is the same four instants minus the duplicate, and it puts the value the caller confirms against first.
- `check_reminders` takes a **flat `project`** param, not the design's `context: { project }` wrapper — it matches the other reminder tools and avoids a one-key object.
- **PROJECT wire handles use `project_handle`**, not `record_id_to_string`/`parse_simple`: format with native `ToSql`, parse with `project_handle::parse_record_id` / `parse_project_handle`, then perform the existing owned lookup. The inert parser uses the 4.0.1 native `syn::record_id` EOF check and a single array-key element; SDK `parse::value` still accepts prefixes. The explicit project resolver retains aliases but refuses malformed project-prefixed selectors/ranges and wrong-table record literals instead of establishing them as names — with the same `not found` a foreign handle gets, so refusal shape is not an oracle. Reminder targets keep the loud strict refusal. Preserve typed point lookup with `FROM $id`; converting a numeric key to a string for `type::record` changes its identity. Project descriptors, listing/reminder targets and collection form choices share this codec. The global display formatter, unrelated entity codecs and journal typed-ID storage remain unchanged. Keep the storage-only `surrealdb-core` dependency exact-pinned to the SDK family, defaults off.
- **Authenticated reminder targets are owned typed ProjectIDs**, stored in `reminder.project`. `set_reminder.target_project`, `check_reminders.project` and `list_reminders.target_project` reject foreign/missing handles and aliases/basenames, including checks with no due rows. Omission means global/context-free within the account, never a fallback after an invalid input. `target_project` and `legacy_target_project` are historical text, not routing authority; unresolved text and `migration_hold` remain pending/suppressed before sampling and at the conditional claim, but owned history remains listable/cancellable. Scoped service outputs use one typed handle, not `project:project:…`. Unresolved history has `target: null`, never `global`. Existing recurrence engine/claim semantics remain shared; no new timer or historical mapping. The Pi companion resolves handles via `resolve_project`; Claude recall does not consume reminders.
- Project escalation is **inclusive**: a project-targeted reminder escalates at `>= escalation_hours` overdue. That is what makes `escalation_hours = 0` mean "escalate as soon as overdue"; a strict `>` would make 0 mean never.
- `set_reminder` rejects a blank `message` and v007 asserts non-empty after trim. The client-side `(reminder with no text)` placeholder is defence in depth, not the enforcement point.
- `normalize_cron` collapses whitespace on **every** branch, so a padded expression cannot render two different `schedule` strings from one row.
- **Claude Code has no automatic reminder delivery.** `contrib/claude/hooks/alexandria-recall.sh` injects auto-recall only; a Claude Code user sees the read-only `due_reminders` piggyback but no `check_reminders` consumption unless the agent calls the tool itself. The pi companion extension is the shipped delivery path — implementing it in the hook was deliberately left out of scope.
- The Pi companion dispatches scoped recall and reminders from one prompt handler and merges both into one injection. It has no automatic store or extraction writer. The dispatcher stays registered even when both features start disabled because the UI-only `/alexandria` menu can enable recall live. A prompt captures one effective recall settings snapshot before any await and uses one authorized project lookup and configurable deadline, including a reconnect.
- The companion's `project` hint comes from the session's `ctx.cwd`, **cached per directory** rather than once per process, and the recall/reminder merge lives in `src/injection.ts` so the isolation claim is tested rather than inspected. The whole prompt path carries one `PROMPT_BUDGET_MS` deadline, and `resetClient()` closes what it drops.
- Schema migrations are forward-only, numbered (`v001`, `v002`, ...), tracked in `system_config` table. Current head is `v017_journal_assertion_compat.surql`; `schema::LATEST_VERSION` is derived from the `MIGRATIONS` table. On a new migration, update the named-head assertion in `identity_schema_test` and this note. `/debug` renders applied-vs-compiled-in version and calls out a mismatch.
- SurrealDB 4.0 refuses closures in schema field assertions evaluated under an `AUTH LIMIT`. v017 replaces the two v009 journal closures with equivalent membership checks for nonempty effects and allowed scope kinds. Keep historical migrations and the authority limit intact; apply the new definitions forward without rewriting rows.
- Every definition statement in a migration is `DEFINE <kind> OVERWRITE …` and every removal is `REMOVE <kind> IF EXISTS …` — with one survivor: `v003` opens `DEFINE TABLE IF NOT EXISTS system_config SCHEMAFULL`, which is replay-safe and so does not break the gate. **The rule exists because `migrate()` is not atomic across a file**: each migration is one multi-statement query, SurrealDB commits each statement in its own transaction, and `system_config` is stamped only after all pending migrations succeed. A crash mid-file leaves definitions applied with the old stamp, so the next boot re-runs that file from the top — with plain `DEFINE` that is a deterministic `The table 'reminder' already exists` and the server never starts again. `migration_test::test_replaying_a_completed_migration_is_not_an_error` re-applies every compiled-in file and is the gate on the rule; `MIGRATIONS` is `pub` for exactly that test. Note the 3.2 and 3.3 grammar puts the qualifier *after* the kind — `DEFINE OVERWRITE FIELD …` is a parse error.
- Embedding identity is locked at boot. `check_boot_embedding_metadata` refuses pending/corrupt targets and model/budget mismatches before provider loading, schema/heat writes or index definition; `check_embedding_space(&loaded.space)` checks the actual provider's full identity before access recording/index setup. Omitted budgets resolve after final model/env selection: legacy256, Granite8192; explicit budgets survive. Granite's identity includes revision/checksums/768D/CLS/FP32/FP64 L2/budget/semantics, **not device**. URLs remain null and MiniLM remains default pending licensing/artifact/calibration approval. See `docs/granite-test-data.md` for model-specific score bands and Vulkan8K's tested single-buffer limit.
- **Legacy `DEFAULT_MAX_TOKENS` and `PRE_LOCK_MAX_TOKENS` must stay distinct**: new legacy corpora default256 (above256 unmeasured, issue #26), while historical unstamped corpora genuinely used128. An old corpus re-embedded at256 before the token-lock key existed reads as128 and can silently truncate at128 until the legacy `--force` command repairs it. Never stamp the configured budget as invented historical provenance. Boot rejects budget mismatches in either direction; the legacy in-place CLI only raises limits. Separate offline targets have their own complete identity and verification. `LoadedEmbedding` carries the actual space/effective budget from the shared factory, rather than startup assuming every provider is Candle.
- Offline migration ownership lives in storage's `embedding_state`/`embedding_validation` and `offline_copy` modules: original baseline + journal + sentinel atomic at begin, guarded checkpoints, full data/index verification and atomic Complete publication. Resume never recaptures the baseline, `--force` never adopts a target, and completed report reconstruction needs no model artifacts/GPU/inference. Source receipt attests original physical files; logical baseline is captured after copied schema bootstrap. One-shot CLI tests use separate processes because Surreal SDK drop is asynchronous. Keep failed stages/receipts; no automatic cleanup/cutover.
- **Debug is authenticated and owner-scoped, not read-only.** Besides the Query Tester heat write it supports owner edits/moves, classification, reminder mapping, consent and quality decisions/revert. Admin mutations manage accounts, not corpus. Dry query is non-writing. Production uses `router_secured`; legacy constructors serve allowed assets but fail closed on private routes. Application admin views cannot read other owners' content directly. An admin can issue and use a password-recovery link to reset another account's credentials and sign in as that owner; the v016 `admin_recovery_log` records issuance, and the admin receives the usable link. Host administrators can read disk and backups; this is not end-to-end encryption.
- **Every debug data read goes through a `Scoped*Repo` under the caller's own population.** The pages
  must not construct `MemoryRepo`/`SessionRepo`/`ClusterRepo`/`EdgeRepo`/`HeatRepo`, must not call
  `stats::gather`, and must not re-read ids through an unscoped repo after a scoped walk produced them
  (the graph's node labels were exactly that leak). `crates/alexandria-mcp/tests/scoped_request_tools_test.rs`
  scans `src/` for those constructors including under `src/debug/` — the Task 8 carve-out for the debug
  tree is closed, so a new page that reaches for an unscoped repo fails there rather than in review.
  Foreign and absent ids are **one** `404`; a distinguishable answer is a probe. `debug/test_support.rs`
  and `migrate.rs` are the two named exemptions (a `#[cfg(test)]` fixture, and the offline re-embed pass
  that must see soft-deleted rows).
- **`AdminCapability` is account management only, never a memory reader.** An admin's own corpus is read
  through the ordinary `OwnerContext` path. Every `/debug/admin` handler takes the capability from
  `session.admin()` itself and answers the `ADMIN_ONLY` refusal without it, *in addition* to the guard's
  `admin_route` list — the guard answers first and a router test cannot reach the handler check, so the
  duplication is deliberate defence in depth for a route that escapes the list, not a second authority.
  `/debug/maintenance` is admin-only and **aggregate** (runs ×
  jobs × action counts, no dispositions, no record identities, no content) because the pre-Task 9 per-row
  audit page listed every account's collapses to any browser; owner-facing per-record history belongs to
  the operation journal, not to this table. `MaintenanceRepo::run_rollup`/`count_runs` take the
  capability as a type argument, which is why a page cannot "forget" the gate.
- **No first account exists over HTTP.** Enrollment is invitation-only and an invitation needs an admin,
  so a fresh empty store cannot sign anybody in from the browser; use the offline `bootstrap-owner` command. Nothing about stdio, loopback, `Host`, forwarded headers or an empty account table grants
  authority. Do not add a public "create admin" route to close that gap.
- **Cookies are two, and both are `HttpOnly`.** `alexandria_session` is the credential;
  `alexandria_csrf` is the double-submit token the guard requires on signed-in browser mutations (form field
  `csrf` or header `X-CSRF-Token`). The module docs used to claim CSRF was deliberately not `HttpOnly`
  "because the page has to read it back" — nothing reads it back: templates render the token server-side,
  so both cookies carry `HttpOnly`, and the docs now say that. The guard is the sole CSRF authority; a
  handler must not re-check the field or expose an unread `csrf` param. Login/invite/recover use
  page-scoped, single-use, expiring in-process pre-auth tokens that are **not** sessions and grant no
  authority.
- **Hosted HTTP needs `server.public_origin`; the server will not infer it.** `allowed_hosts` must
  contain the request `Host`, `allowed_origins` must contain that origin exactly, `https` is required
  except under `server.loopback_development = true` on a loopback bind (setting it with a public bind is
  a startup error), and `X-Forwarded-*`/`Forwarded` never establish origin, host or the exception.
  Origin/host checks are pure functions of the request headers in `debug/web.rs` so they can be tested
  without a socket.
- The debug router is guarded by `debug/guard.rs`, applied as a single `middleware::from_fn_with_state`
  layer over the whole router returned by the internal `build`. **The CSRF/authentication half is
  unconditional** and the **Host half is armed by the policy** — an empty `allowed_hosts` no longer means
  "no check" on a secured build. **Any new debug route must be added inside `build`, below that
  `.layer()` call** — a route registered after it, or served by a different router that merges this one,
  escapes every check. `guard.rs`'s `GUARDED_PATHS`/`GUARDED_MUTATIONS` matrices enumerate the surface for
  exactly that reason, and `every_route_the_router_mounts_is_in_the_matrix` parses `mod.rs` to prove a new
  `.route(...)` is in one of them. Extend the lists with the route.

- Cluster cohesion in the debug UI comes from the cluster's **stored centroid** (`ClusterRepo::get`), never a recomputed member-average. `main.rs`'s maintenance loop uses the stored centroid, so an approximation made `/debug/clusters/{id}` report "Healthy" for a cluster the background task was about to split — a diagnostic surface that disagrees with the mechanism it diagnoses is worse than showing nothing.
- `debug::router(server)` passes `None` for `DebugContext`; only HTTP mode via `router_secured` (and the legacy `router_with_context`) populates it. The dashboard's config panel therefore renders an explicit "unavailable outside HTTP mode" state instead of bogus zeros, because `ClusterConfig` lives in the binary crate and structurally cannot reach `alexandria-mcp`. **Do not change `router()`'s signature** — every call site is inside a `#[cfg(test)]` module under `src/debug/`; production is the single `router_secured` caller (`main.rs`), and `router_with_context` survives only as the fail-closed build the no-context tests exercise.

- **pi ≥ 0.99 owns MCP, and it is the naming contract.** pi's built-in MCP registers one real tool per
  MCP tool as `mcp__<server>__<tool>` (`mcp__alexandria__store_memory`), sanitised to `[A-Za-z0-9_-]`,
  with a sha256-8-hex suffix only when the name exceeds 64 chars or collides — so `mcp__alexandria__*`
  is never truncated, and tool arguments are the server's own flat `inputSchema` (`input.content`, not
  nested). The skill and the READMEs use that name. An installed
  extension that registers `/mcp` (pi-mcp-adapter does) **replaces** built-in MCP — pi then never reads
  `mcp.json` nor connects any server. The older adapter published a gateway and proxies rather than
  these flat tool names. Default server `exposure` is `codemode`; `direct` exposes the Alexandria
  tools without a lookup round trip.
- **The npm side sits outside this repo's supply-chain policy.** Rust has `cargo-deny` for advisories
  *and* licences; the companion's npm tree has neither (and `deny.toml`'s allowlist would reject
  `BlueOak-1.0.0` and `0BSD` if it were applied — substantively harmless, but it shows npm is simply
  ungoverned). Counts and per-family numbers are recorded once, in TODO-misc.md, not here. Posture
  today: both install paths pass `--ignore-scripts` (justfile `ext-install`, the CI step, and the
  documented user install), because several packages in the tree declare lifecycle scripts —
  `esbuild` (`postinstall: node install.js`), `protobufjs`, `@google/genai` — and that is the
  only third-party-code execution path in CI. Do NOT substitute `--omit=optional` for it: esbuild's
  `install.js` falls back to `downloadDirectlyFromNPM` when `@esbuild/linux-x64` is missing, so
  omitting optional *with scripts enabled* re-enables an unpinned registry download, and omitting it
  *with* `--ignore-scripts` is what actually breaks `tsx`. `@earendil-works/pi-coding-agent` is
  exact-pinned at `1.1.0` (type-only: every import is `import type`, and
  `npm install --omit=dev` leaves just the runtime closure). Nothing compares that pin to the pi actually loading the extension — CI has no
  host pi, and `just ext-check-pi` was removed rather than left claiming a wiring it never had — so
  the two agree only because a human keeps them in step.
- **pi companion prompt path: a failure is attributed, not guessed.** The MCP SDK reports an *aborted* request as `SdkErrorCode.RequestTimeout` (`REQUEST_TIMEOUT` on the wire), so the error text alone cannot tell an operator's Esc from a dead server. `src/failure.ts` classifies into `cancelled` (pi's `ctx.signal` aborted) / `stalled` (our 10 s path budget expired while the 5 s per-call deadline did not, which means the timers themselves ran late) / `transport` (everything else), and only `transport` drops the connection. `index.ts` supplies the context via `asFailure()` at the throw site and `injection.ts` consumes it via `failureOf()`; both are exported pure functions because the handler is an inline closure that cannot be reached from a test without module mocking, which hangs under tsx. Do not fold them back into the handler. Every `FailureKind` variant must have a producer — a `worker` variant was added for a planned worker-thread client, the plan was dropped, and the variant was removed rather than left speculative.
- **`prewarm()` is a correctness measure, not a startup optimisation.** The SDK arms its handshake deadline as a `setTimeout` on pi's main loop, shared with every other in-process extension, so a stall longer than the remaining budget makes the overdue timer win the race against a handshake that was succeeding. Measured: a 6 s freeze 10 ms into a cold first call gave a false `REQUEST_TIMEOUT` in 6/6 fresh processes, the same freeze against a warm client resolved normally in 15/15, and pre-warming gave 3/3 honoured. Connecting at extension load takes the handshake off the prompt path. It does **not** cover a reconnect after `resetClient()`; if false timeouts ever show up with `cold:false` in the diagnostic log, that residual window is the place to look, and the fix would be moving the client to a worker thread (which needs a plain `.mjs` worker, because pi loads extensions through a bundled jiti that a spawned worker cannot inherit).
- **Anything that shells out to `git` must sanitise `GIT_DIR` and friends.** With `GIT_DIR` set — which git does for hooks, and which a git-driving parent can leak — `git rev-parse --show-toplevel` returns the **cwd** instead of the working tree root. `getProjectHint` probes exactly that, and a wrong hint is not cosmetic: legacy servers could hold that project's reminders until escalation. Authenticated reminder calls now require an owned typed project handle; the unchanged client's basename hint is not one and is refused. `reminders.ts` strips the `GIT_*` overrides per call (`gitProbeEnv()`), `.githooks/pre-commit` unsets them before running any gate, and `tests/reminders.test.ts` pins the behaviour by setting `GIT_DIR` deliberately. The symptom to recognise: git-dependent tests pass on a direct `npm test` but fail when run from a hook.
- **`src/diag.ts` appends one JSON line per prompt** to `$XDG_STATE_HOME/alexandria-companion/prompt-path.jsonl`, rolling at 256 KB. It exists because the intermittent false timeout had never been observed in the field and every lab reproduction was synthetic. `cold` marks a run that paid a handshake; `driftMs` is the worst event-loop lateness seen during the run, and is what separates "the server was slow" from "the server was fine and something else in pi's process held the loop". Nothing in it may be awaited by the prompt path or allowed to throw: writes are async and self-swallowing and the probe timer is `unref`'d, because a diagnostic that can block the loop would be worse than none when a blocked loop is the suspected cause. The probe also measures lateness at stop time, not only on ticks — a stall ending just before `stop()` leaves the overdue callback no chance to run, which is how the first implementation reported zero drift for the exact block it existed to catch.

## askama Templates & Vendored Assets (Debug UI)

- Every template context struct **for a page that extends `layout.html`** must declare `nav: &'static str`, because `layout.html` reads it in base top-level content. Forgetting it is a compile error, by design — a silently unhighlighted nav is worse. Two structs deliberately have no `nav` and must not gain one: the Query Tester's htmx fragment and the pager test scaffold, neither of which extends the layout.
- **`|safe` is forbidden** in templates. Escaping has to stay structural: askama escapes every `{{ }}` at compile time, so the only remaining decision is *context*. `html::esc()` and `html::layout()` are deleted; there is no helper left to forget to call.
- The `pager` macro in `_pagination.html` is deliberately **presentational**: callers pass complete hrefs (empty string = no link on that side) and their own summary text. It cannot build URLs itself because `maintenance` paginates by `?page=N` while `memories`/`sessions` paginate by offset and must carry their filters across the hop.
- askama 0.16 escapes to **decimal** character references (`&#60;`, `&#62;`, `&#38;`, `&#34;`, `&#39;`), not named entities. Same character set as the deleted `esc()`, different bytes — a test asserting escaped output must expect the decimal form.
- Record ids in an `href` (or any URL) use `|urlencode`; HTML text uses plain `{{ }}`. In `graph.html` the id is interpolated **inside a JavaScript string literal**, where browsers do not decode character references — `|urlencode` is what makes that safe, since its output alphabet is `[A-Za-z0-9_.-~/]` plus `%XX` and so cannot produce a quote, backslash, `<` or newline that would terminate the literal or the `<script>` element.
- Adding a vendored asset means three edits: `assets.rs` (the closed `match` allowlist), `assets/SHA256SUMS`, and `just vendor-assets`. The allowlist is a `match` over literal filenames with `include_bytes!`, so there is no filesystem read or path parsing at request time — traversal is impossible by construction, and the 404 tests exist to keep it that way. Filenames pin the bytes because they are served `max-age=31536000, immutable`; `just verify-assets` is what makes `SHA256SUMS` machine-enforced rather than decorative.

## Testing

- The repo intentionally carries no `rust-toolchain.toml` and no `.cargo/config.toml` — `rust-version = "1.99"` + edition 2024 are the only compiler statement; mold/`target-cpu` live in each dev's `~/.cargo/config.toml` (see TODO-misc "Build / toolchain"). Don't re-add them to the repo.
- Use the `just` recipes (they match CI; bare `just` lists every one, including the few not named here): `just test` (all-feature Rust), `just test-cpu` (CPU-only admission), `just cpu-deps` (CPU binary normal/build graph rejects known GPU backend/native packages; permits shared CubeCL utilities), `just artifact-ci` (standalone curator fmt/Clippy/hermetic tests/deny, no official model conversion), `just eval-test` (dependency-free Python scorer), `just hermes-test` (Python 3.12+ with the hash-locked environment in `contrib/hermes/README.md`, no Hermes installation required), `just ext-test` (pi companion: `npm run typecheck` + `npm test`, needs `just ext-install` once per checkout since `node_modules/` is gitignored), `just lint`, `just fmt`, `just ci` (fmt + lint + both Rust feature sets + curator + scorer + ext-test + hermes-test + `cargo deny` + `verify-assets`). `.github/workflows/ci.yml` mirrors it with separate `Pi companion` and `Hermes companion` jobs — the workflow does not call `just ci`, so a new gate has to be added in both places. `just verify-assets` checks `assets/SHA256SUMS`; `just vendor-assets` re-downloads the assets and re-verifies. `just install-hooks` wires `.githooks/pre-commit`.
- `surrealdb` is pinned `=4.0.1` in `[workspace.dependencies]`, and `lint`/`test`/`check`/`run` pass `--locked`. Engine releases are storage-format decisions, not ordinary dependency bumps: the earlier 3.2→3.3 boundary was already one-way, and the 3.3→4.0 physical conversion is experimental, copy-only, and requires an index rebuild (see `docs/surrealdb-4-upgrade.md`). The SDK pin does not freeze internal `surrealdb-*` ranges; preserve the whole resolved family at 4.0.1 and independently versioned protocol/backend at 0.14.1/1.2.0 during ordinary refreshes. Review engine-family changes separately. `just deny` reads the lock and has no `--locked`. `docs/dependency-advisories.md` records the historical main-branch policy and validation, not the merged feature's policy. The feature's dependency policy is unchanged. The documented main-branch crypto and BUSL exceptions are not adopted. A new full-policy scan is required on the combined graph.
- Run tests on **stable**, not nightly: `diskann-wide` (SurrealDB transitive dep) fails trait inference on its NEON intrinsics under recent nightlies on aarch64, and the failure looks like it originates in this workspace. Run the suite with `just test` (Rust) and `just ext-test` (extension); there is also one `#[ignore]`d 130 s wall-clock delivery test that no gate runs (`cargo test -p alexandria --test reminders_test -- --ignored`). Test counts are deliberately not recorded here; they went stale in every doc that carried one.
- Integration tests use `Database::connect_embedded()` (in-memory SurrealDB), so no disk state leaks between them. One deliberate exception opens a persistent SurrealKV store under a `tempfile::tempdir()` and reconnects to it, to prove round-trip survival; the tempdir keeps it isolated all the same.
- `CandleProvider` tests download the real model on first run (~80MB) — they're slow the first time.
- Test helpers in `alexandria-storage/src/connection.rs`: `connect_embedded()` for quick in-memory DB.
- Config tests must not touch process env: `Config::load()` delegates to `Config::load_from(&|k| std::env::var(k).ok())`, and tests inject overrides through the closure (`Config::load_from(&env(&[("K", "V")]))`, local helper in `config.rs`). No `serial_test`, no `#[serial]`, no ambient-env races.
- `debug/test_support.rs` has two embedding stubs, and the difference matters. `StubEmbedding` returns a constant vector, so **every similarity is 1.0** and a `min_similarity` floor can never filter anything — useless for floor tests. `BandedEmbedding` (via `banded_server()`) emits `[s, √(1−s²)]`, so cosine against the query's `[1,0]` is exactly `s` — deterministic straddling of a floor. Do not change `StubEmbedding` or `test_server()`; the whole debug test suite depends on both.
- `HeatRepo::add_heat` is an `UPDATE heat_state ... WHERE memory = ...` that **silently matches zero rows** when the memory has no `heat_state` row, and its caller swallows the result with `.ok()`. `MemoryRepo::create_fact` creates no heat row, so a "did not write heat" assertion built on it passes **vacuously**. Seed with `do_store_memory` (which writes `heat_state` at 1.0) plus `EdgeRepo::create_edge`, and always pair a "no write" assertion with a positive control proving the fixture can write at all.
- The pi extension has its own suite: `cd contrib/pi/extensions/alexandria && npm run typecheck && npm test` (node:test + tsx, hermetic — a controlled `ALEXANDRIA_CLIENT_CONFIG`, restored `process.env`, no server or socket). `just ext-test` runs it and `just ci` includes it, so it is not optional. Bare `just` lists every recipe. The detector regexes **are** covered — `tests/detectors-*.test.ts` exercise them. The remaining gap is the LLM extraction prompt and its truncation, and that is what the README says.
- `tests/diag.test.ts` points `XDG_STATE_HOME` at a per-test `mkdtemp` (restored afterwards), because `logPath()` reads the env at call time rather than import time — that is what makes the rolling-log and unwritable-path cases testable without module mocking. It seeds an oversized file directly instead of appending 256 KB one record at a time. `recordPromptPath` is fire-and-forget with no awaitable handle by design, so those tests poll for the write; do not add a returned promise just to make them tidier, since nothing on the prompt path is allowed to await it.
- The `.githooks/pre-commit` gates are path-scoped: Rust files stage `just fmt` + `just lint`, `contrib/pi/` stages `just ext-test`, and neither stages nothing. That scoping is load-bearing rather than a convenience — running full clippy on a TypeScript-only change in a fresh worktree is slow enough to tempt sharing `CARGO_TARGET_DIR` with the primary checkout, and doing that poisons the primary's incremental fingerprints (its dep-info files record the worktree's absolute paths, so a Rust edit in the primary can silently fail to rebuild). `just install-hooks` copies the hook into `.git/hooks/`, which is the *common* git dir and therefore shared by every worktree, so an edit there takes effect everywhere immediately while the tracked copy keeps it durable.

## CI

- Actions are pinned by full commit SHA with a trailing `# vN` comment, and Dependabot
  (`.github/dependabot.yml`, weekly, 7-day cooldown) proposes bumps.
- Failure signature of a dead pin: a job dies in **"Set up job"** after a few seconds with
  `Unable to resolve action <owner>/<repo>@<sha>, unable to find version`. Everything using that pin
  fails identically, and no recipe ever runs. Fix = repin to the commit the tag names now
  (`gh api repos/<owner>/<repo>/git/ref/tags/v2`), not to a branch head.
- Triage by duration before reading logs: a real `ci.yml` job is ~2–7 min. A run that concludes in
  10–40s failed in setup, which means infrastructure, not code. Caveat: the `Container` job can also
  die in <10s *after* a successful setup — read the step name before blaming the runner.
- The separate `Container` workflow (`.github/workflows/container.yml`) is path-scoped to
  `Containerfile`/`Cargo.*`/`crates/**` and legitimately takes far longer than the rest of CI — it
  does a cold release build of the workspace inside the image with no layer cache. Do not apply the
  10–40s duration heuristic to it, and do not expect it to appear on docs-only pushes.
- The container build file is `Containerfile`, and it must be named: `docker build -f Containerfile`.
  Unlike buildah/podman, Docker only auto-discovers a file literally called `Dockerfile`, so without
  `-f` the job fails in ~8s with `failed to solve: failed to read dockerfile: open Dockerfile: no
  such file or directory`. The same asymmetry is why the ignore file stays `.dockerignore` — Docker
  never reads `.containerignore`, so renaming it would silently widen the build context for Docker
  users while podman kept working. `paths:` filters are never validated against the tree, so renaming
  the build file also silently de-scopes the workflow: both names have to be edited together.
- Do not test whether a pin is reachable with `gh api repos/<owner>/<repo>/commits/<sha>` — it
  returns 422 "No commit found" for pins that Actions resolves fine. Trust the Actions error text, or
  the fact that a job using that pin passed.
- Branch protection here is a **ruleset**, not legacy protection, and it is scoped to every ref:
  `gh api repos/<owner>/<repo>/rulesets` returns one named `main` with
  `conditions.ref_name.include = ["~ALL"]`, required status checks `Formatting`, `Clippy`, `Tests`,
  `cargo-deny` and `Pi companion`, `strict_required_status_checks_policy: false`, squash as the *only*
  allowed merge method, `required_approving_review_count: 0`, `required_review_thread_resolution: true`,
  `dismiss_stale_reviews_on_push: true`, and `require_extra_approval_for_unattributed_changes: true`
  — which the docs say has no effect at zero required approvals, so it is not what blocks anything.
  When a merge is unexpectedly blocked, read the live set with
  `gh api repos/<owner>/<repo>/rulesets/<id>` instead of trusting this paragraph: required checks are
  GitHub config, they drift from the file silently, and there is no revision history endpoint to
  diff against (`/rulesets/<id>/revisions` 404s). The reason it exists is the incident below —
  and its consequence is that **a PR producing no check runs is `mergeStateStatus=BLOCKED` and
  cannot merge at all**, however good the code is. That is what makes workflow trigger scoping
  load-bearing rather than cosmetic: a trigger filtered to `main` combined with checks required on
  `~ALL` means a PR based on another feature branch can never satisfy its own gate.
- Squash merges break **stacked** branches, which is the cost of that only-squash policy. Once a
  base PR is squashed, `main` re-adds files as commits unrelated to the ones the stacked branch
  descends from, so git sees two independent additions and the stacked PR goes
  `CONFLICTING` on add/add. Resolve by merging `origin/main` into the branch and taking the branch
  side (it is the strict descendant), then verify nothing was lost — a rebase needs a force-push,
  which is a different authorisation.
- Neither a merge queue nor auto-merge is available: `merge-queues` 404s and
  `allow_auto_merge` is false, so there is no "queue it" — you wait for checks, or re-trigger them
  deliberately. A PR base change emits only `edited`, which is *not* a default `pull_request`
  activity type, so retargeting alone starts nothing; `gh pr close` then `gh pr reopen` fires
  `reopened` and does.
- `.github/workflows/ci.yml` is **six separate jobs** (`fmt`, `clippy`, `test`, `extension`,
  `hermes-companion`, `deny` — `extension` is the job displayed as `Pi companion`,
  `hermes-companion` as `Hermes companion`) and does
  **not** call `just ci`. Editing the justfile alone therefore changes nothing on GitHub Actions — a
  new gate has to be added to the workflow too. `just verify-assets` runs as its own step in the
  `test` job, before `just test`, because the year-long `immutable` asset cache header is only safe
  while the filename pins the bytes. The `test` job sets `CARGO_PROFILE_DEV_DEBUG=0`,
  `CARGO_PROFILE_TEST_DEBUG=0` and `CARGO_INCREMENTAL=0` to fit both feature sets and
  the curator on the hosted runner; optimization levels, debug assertions, overflow
  checks and coverage stay unchanged, but CI backtraces lose debug-info detail.
  These are job-only overrides, not local profile changes. The pinned rust-cache
  action includes these env values in both exact and fallback cache keys, so old
  debug-info artifacts are not restored under the reduced-footprint settings.
- `Pi companion` **is** in the ruleset's required list, so a red companion job now blocks the merge.
  It was gated by convention only until the ruleset was edited in the UI, which is why no commit
  authorises the change and `git log` cannot show it — this bullet is the only in-repo record of the
  current set. The job runs `npm ci --ignore-scripts`, `npm run typecheck` and `npm test` directly, so
  it is the same suite `just ext-test` wraps but not the recipe, and it runs on every PR including
  Rust-only ones. Practical consequence: a break in `contrib/pi/` can no longer land through a
  change that never touches TypeScript, and `just ci` is not sufficient — the three npm steps have to
  pass locally too.

## Docs Map

- `README.md` — feature/tool overview, quick start, deployment (systemd, Docker), debug UI
- `docs/configuration.md` — every server and client config key, env overrides, XDG migration
- `docs/surrealdb-4-upgrade.md` — tested Linux offline copy-first 3.3→4.0.1 procedure, mandatory HNSW rebuild, preservation checks and backup-only rollback
- `docs/dependency-advisories.md` — patched dependencies and historical main-branch policy/validation; its crypto and BUSL exceptions are not carried into this feature
- `CHANGELOG.md` — user-visible security changes and storage upgrade boundary
- `docs/session-memory.md` — session data model, lifecycle, tool semantics, current limitations
- `docs/minilm-test-data.md` — retrieval measurements for the embedding model, how to rerun
  `alexandria bench-retrieval`, metric definitions, and the frozen question set
- `docs/roadmap.md` — shipped milestones and planned work
- `docs/security-findings.md` — 2026-09-10 audit: threat model (memory as a prompt-injection
  persistence layer), the convex-hull paper verdict, ranked findings S1–S6
- `docs/performance-and-ability-findings.md` — same audit: the 128-token truncation measurement,
  inert heat model, O(N) cluster counting, ranked findings A1–A4 / P1–P5
- `docs/*-findings.md` cite code by symbol (`do_store_memory`, `MemoryRepo::nearest`), never by
  `file.rs:NN`. Line numbers go stale on the next commit; a symbol can be grepped.
- `docs/prompt-path-stall-attribution.md` — 2026-09-22 investigation record for the pi companion's
  prompt path: the measured cold-handshake fault, `prewarm()`, `src/diag.ts`, and why the proposed
  worker isolation was dropped. **A record, not a plan** — its STATUS banner names which tasks were
  never implemented, so paths under those task headings describe work that does not exist.
- `contrib/pi/README.md` — how the pi skill and extension differ and install
- `contrib/pi/extensions/alexandria/README.md` — the pi companion extension: what it does, reminders,
  configuration, known gaps
- `contrib/pi/skills/alexandria-memory/SKILL.md` — the pi client-side skill (`mcp__alexandria__<tool>`
  names, as pi's built-in MCP exposes them since 0.99)
- `contrib/claude/README.md` — the Claude Code skill and hooks
- `contrib/hermes/README.md` — the Hermes Agent memory-provider companion: Alexandria-only setup, config, foreground reminders, hook mapping, tests
- Hermes companion tests must bootstrap the real host contract or shipped stub before any package import; the transport submodule also executes the package initializer. `just hermes-test` and CI use the same clean unittest discovery command. Background/cron/flush/subagent contexts must never consume reminders automatically, and consumed deliveries must never enter the semantic cache. Hermes skips foreground prefetch on trivial prompts, so direct provider tests do not establish end-to-end delivery for those prompts; document that limit rather than consuming in a background hook.
- `contrib/claude/skills/alexandria-memory/SKILL.md` — the Claude Code skill, using the same
  `mcp__alexandria__<tool>` names. Must stay at content parity with the pi copy: since pi 0.99's
  built-in MCP the two clients spell the tools identically, so the remaining differences are
  client-specific prose (hooks vs the companion extension), not tool names — `diff` the two files
  when editing either
- `crates/alexandria-mcp/assets/README.md` — vendored debug-UI assets, checksums, re-vendoring
- `TODO-misc.md` — unprioritised backlog, grouped by area
- `AGENTS.md` — this file. It was named `CLAUDE.md` until the docs sweep that added session memory
  and the pi extension docs, so references to `CLAUDE.md` in commits before that point mean this file.
  `CLAUDE.md` still exists as a one-line `@AGENTS.md` import so Claude Code auto-loads this file.

## Config Precedence

### Server

defaults → `$XDG_CONFIG_HOME/alexandria/config.toml` → `ALEXANDRIA_CONFIG` env var (path to alt TOML) → individual env vars (`ALEXANDRIA_SERVER_TRANSPORT`, etc.)

Legacy path `~/.alexandria/config.toml` is used as fallback if the XDG path doesn't exist.

Data defaults to `$XDG_DATA_HOME/alexandria/data` (was `~/.alexandria/data`).

### Client (Pi extension)

defaults → the JSON file selected by `ALEXANDRIA_CLIENT_CONFIG`, or `$XDG_CONFIG_HOME/alexandria/client.json` by default → individual `ALEXANDRIA_*` env vars. The companion no longer reads `client.toml`; there is no TOML fallback or automatic migration.

The extension mirrors the Rust `dirs::config_dir()` behavior: `~/Library/Application Support/alexandria/client.json` on macOS, and `XDG_CONFIG_HOME` still wins on any platform when set. `/alexandria` edits auto-recall in the current process and saves the selected JSON file; environment overrides still win and are shown in the menu. Its writer follows symlinks, preserves ordinary mode bits, and requires no external metadata tools. The new inode does not preserve ownership, ACLs, xattrs, special mode bits, or inode flags; effective permissions may therefore change. The user accepted that a group-writable temporary file may be modified before rename by a member of its possibly different group; the docs disclose this, so do not claim staging is protected against it. macOS saves have not been tested on a real Mac; Windows menu writes are refused. Companion writers use an exclusive lock, but ordinary editors can still race the final check/rename; never describe that as universal concurrent-write protection. Never replace a symlink with the temporary file or silently overwrite malformed JSON.

## License

AGPL-3.0-or-later (`LICENSE`, `license.workspace` in `Cargo.toml`). Chosen over MIT because Alexandria is a long-running network service — keep new crates on `license.workspace = true`.
