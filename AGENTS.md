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
- To count filtered records reached by a graph traversal *inline in a SELECT*, the `WHERE` goes **inside** the traversal target's parentheses: `(->contains_session_memory->(fact WHERE deleted = false)).len()`. The obvious `(->edge->fact WHERE ...).len()` fails with `Unexpected token WHERE expected delimiter )`, and so do the `array::len(...)` and `count(...)` spellings — no N+1 fallback is needed once you find this.
- An absent `option<T>` field is `NONE`, and **`IS NOT NULL` and `!= NULL` are both satisfied by `NONE`** — they filter nothing; only `field != NONE` (or `NOT (field = NONE)`, or `type::is_none(field) = false`) excludes it. Verified against the pinned 3.2.4 engine: with one `NONE` and one set row, `WHERE next_due_at IS NOT NULL` returned both and `WHERE next_due_at != NONE` returned one. This bites ordering too, because `NONE` sorts below every datetime: such a row wins the head of an `ORDER BY next_due_at ASC LIMIT n` page.
- `ORDER BY x DESC NULLS LAST` does **not** parse, and neither does `ORDER BY type::coalesce(a, b) DESC`. NULL sorts *smaller* than any datetime, so `ORDER BY ended_at DESC` already parks NULL `ended_at` at the tail — but the ties among those rows are unspecified, so any `LIMIT`/`START` pagination needs an explicit unique secondary key (see `SessionRepo::list`).

## rmcp (MCP SDK) Patterns

- Uses `schemars` 1.x (not 0.8) — `#[schemars(description = "...")]` on tool param fields
- Tool macro: `#[tool(description = "...")]` inside a `#[tool_router]` impl block. `#[tool_router(server_handler)]` auto-generates a bare `get_info()`; use bare `#[tool_router]` plus an explicit `#[tool_handler(instructions = "...")]` block on `impl ServerHandler` instead when the server needs to advertise `instructions` (see Non-Obvious Patterns below — `AlexandriaServer` does this).
- Params: `Parameters(params): Parameters<MyParams>` — the wrapper is required
- HTTP transport: `transport-streamable-http-server` feature, `StreamableHttpService::new(factory, session_mgr, config)`

## Architecture Boundaries

- **storage** owns all DB access — no raw SurrealDB queries outside this crate (the `alexandria-mcp` handlers still issue some inline queries directly; don't add new ones without reason)
- **engine** is pure algorithms — no DB, no async (except test helpers). Takes data in, returns results.
- **pipeline** owns embedding — abstracts over providers via `EmbeddingProvider` trait
- **mcp** wires tools to engine+storage — the only crate that knows about both. Also owns the debug web UI (`alexandria-mcp/src/debug/`), which is Axum handlers over the same repos, plus `crates/alexandria-mcp/templates/` (askama, compiled at build time) and `crates/alexandria-mcp/assets/` (vendored third-party JS, `include_bytes!`). Both are **compile-time contracts**: a missing template or asset file is a build error, not a runtime 404.
- **alexandria** (binary) is config + transport + startup + the background cluster-maintenance task
- **contrib/pi** is client-side only — TypeScript, never compiled into or imported by the Rust server

## Non-Obvious Patterns

- `AlexandriaServer` uses a bare `#[tool_router]` + explicit `#[tool_handler(instructions = "...")]` block — NOT `#[tool_router(server_handler)]` — specifically so `get_info()` carries usage `instructions`. If you add a new tool, add it to the `#[tool_router]` impl block same as the others; the separate `#[tool_handler]` block stays where it is at the bottom of `server.rs` and doesn't need touching unless the overall usage guidance changes.
- Tool descriptions and param field descriptions (`#[tool(description = ...)]`, `#[schemars(description = ...)]`) are written directively ("call this proactively when...") rather than just describing mechanics — this materially affects how often client LLMs choose to call the tool unprompted. Keep new tools consistent with that style.
- `record_id_to_string()` is the canonical way to format SurrealDB `RecordId` for use in queries and JSON responses. It lives in `alexandria-storage/src/lib.rs` and is re-exported from `alexandria-mcp/src/server.rs`.
- There are **13 MCP tools**: `store_memory`, `retrieve_memories`, `recall`, `update_memory`, `import_document`, `delete_memory`, `get_session`, `list_sessions`, `finalize_session`, `set_reminder`, `check_reminders`, `list_reminders`, `cancel_reminder`. Adding one means a params struct in `alexandria-mcp/src/tools/`, a `#[tool]` method, a `do_*` impl, and a row in the README tool table. `do_retrieve_memories_dry` and `do_retrieve_memories_unfiltered` are **not** tools — they are non-`#[tool]` wrappers over `retrieve_core(params, options)` used only by the debug Query Tester. Do not count them as tools, advertise them, or add a `dry_run` field to `RetrieveMemoriesParams`: that struct is the public MCP schema every LLM client sees, and a debug-only knob there would be an affordance agents could set.
- The HNSW index on `fact.embedding` is defined at boot by `schema::ensure_vector_index()`, not in a
  numbered migration, because HNSW needs `DIMENSION` at define time and the dimension comes from the
  locked embedding model. Only the numeric-ef form reaches it: `MemoryRepo::nearest_indexed()` issues
  `embedding <|k,150|> $q` (plan `KnnScan`), while `MemoryRepo::nearest()` issues
  `embedding <|k,COSINE|> $q`, which is a brute-force scan (`KnnTopK`) whether or not the index
  exists. `bench-retrieval` calls `nearest_indexed` too, after defining the index on its snapshot,
  so its overlap line measures the index. `<|k,ef|>` without an index strips to a plain scan,
  so `do_retrieve_memories` picks by
  `AlexandriaServer::vector_index`, which `main.rs` sets only when the define succeeded — a failed
  define logs an error and boots on the brute-force path (most tests never define the index and
  take that path too). `test_nearest_indexed_plan_uses_the_hnsw_index` asserts the plan, because
  results cannot tell the two apart. `migrate-embeddings` drops the index before re-embedding
  because it rejects vectors of any other dimension; the next boot redefines it.
- Cluster `member_count` is queried live (not cached) — `load_cluster_infos()` calls `get_members()` per cluster, so it is one query per cluster. Fine at current scale, the first thing to revisit if cluster counts grow.
- The dashboard's cluster-health rollup costs **~2 queries per cluster per render** (`get_members` + the stored centroid from `list_with_counts`), and is flagged `TODO(debt)` in code. Same revisit trigger as above.
- `update_memory` with content change: creates a soft-deleted snapshot of old content, then links via `derived_from` edge. The old version is hidden from search but preserved for lineage.
- `import_document` creates a `raw` table record for the full document, then `extracted_from` edges from each chunk to it.
- Spreading activation fires on the top N results of `retrieve_memories` (configurable via `activation.top_n`, default 3) — it's a side effect, not part of the ranking.
- `retrieve_memories` drops results below `retrieve.min_similarity` server-side, *after* ranking and *after* the `limit` truncation, and *before* activation is triggered — so a result the Query Tester reports as "dropped" was ranked inside the requested window, not suppressed from the whole database. The default is defined once at `alexandria_engine::search::DEFAULT_MIN_SIMILARITY`; read that constant, not this file, for the number. `RetrieveConfig::default()` and `AlexandriaServer::new`'s fallback both derive from it (they used to diverge — 0.10 vs 0.30 — which made a test-built server filter differently from production). It is a noise cutoff only — the client threshold (`[recall] min_similarity`) does the real filtering, paired with the client `limit` — both judgement calls read off `bench-retrieval`'s grid, and neither readable without the other. Do not restate any of those numbers here; `docs/minilm-test-data.md` records the measurements and `docs/configuration.md` the rationale. `cluster.cohesion_floor` follows the same pattern via `alexandria_engine::clusters::maintenance::DEFAULT_COHESION_FLOOR`.
- Cluster maintenance runs as a background `tokio::spawn` in HTTP mode only (not stdio), at an interval configurable via `cluster.maintenance_interval_secs` (default 300s / 5 minutes). It drains **all** eligible merges per tick, not one.
- Every split/merge is recorded in the `maintenance_log` table (`v004`) and surfaced at `/debug/maintenance`. If cluster behavior looks wrong, that table is the audit trail.
- Sessions are created implicitly by `store_memory(session_id)` — there is no create tool. `SessionRepo::touch()` bumps `ended_at`, so `ended_at` means last-activity; only a non-null `summary` distinguishes a finalized session. There is no `memory_count` column to bump — migration `v006_drop_session_memory_count.surql` dropped it, because `delete_memory` cannot decrement a stored counter; per-session counts are derived from the `contains_session_memory` traversal with the same `deleted = false` filter `get_memories()` applies (see `SessionRepo::list`). See `docs/session-memory.md`.
- Session-scoped retrieval filters soft-deleted memories: `SessionRepo::get_memories()` carries `WHERE deleted = false`, so `get_session` and `retrieve_memories(session_id: ...)` agree with the unscoped path. This was a known gap and is now closed; it is pinned at the storage layer by `test_get_memories_excludes_deleted` and again at the UI layer by `test_session_detail_excludes_soft_deleted_memories` — don't "simplify" either filter away.
- Reminders are evaluated purely at query time — there is NO background timer (`grep -n tokio::spawn` finds exactly the one cluster-maintenance task, HTTP-only). Due-ness is `status = 'pending' AND next_due_at <= now()` (`ReminderRepo::list_due`), so delivery is lazy, works identically in stdio and HTTP mode, and downtime costs nothing but a later delivery. The cost: **no client means no delivery**, and the server itself reaches nobody. `set_reminder`'s description says so explicitly; do not soften it back to 'delivered to the user'.
- `check_reminders` is the only consumer: it claims each delivered row with a conditional UPDATE (`status = 'pending' AND next_due_at = <seen>`, `ReminderRepo::record_delivery`), so a lost race skips the row rather than double-delivering; delivery is best-effort-once (a won claim whose response is lost drops that fire). Recurring rows advance to the first fire after now, coalescing skipped occurrences into `missed_occurrences`, plus `missed_occurrences_saturated` once the walk hits `MAX_ITER` — a capped count published as an exact one is the failure mode that test now pins. The repo-layer claim tests are sequential (stale `seen` → false); the actual concurrency claim is pinned by `concurrent_checks_deliver_a_due_row_exactly_once`, which spawns four real consumers, and by nothing else — do not delete it as 'slow'.
- The fold regression, for the record, because it looks impossible from the doc comment: `cron` applies its strict-after bound to the anchor's *local* fields, and inside a fall-back fold a check anchored in the second pass got back the fold's *earlier* instant — which equals the `next_due_at` just consumed, so the conditional claim succeeded, `delivered_count` bumped, and every prompt re-delivered the same reminder for the rest of the fold. No concurrency required. Pinned by `dst_fold_second_pass_never_returns_a_past_fire`, which anchors inside the fold; the pre-existing DST tests all anchored outside it, which is why 41 green tests said nothing.
- Recurring evaluation is wall-clock, and **one wall-clock reading is one occurrence**. Inside a fall-back fold `cron` yields the same local time twice (`LocalResult::Ambiguous(earlier, later)`, earlier first) and a spring-forward time never appears. `fires_after()` dedupes by `naive_local()` **and** filters the converted instant `> after`; both are needed, because `cron` applies its own strict-after bound to the anchor's *local* fields, so a check anchored in the fold's second pass otherwise hands back the earlier (already consumed, already past) instant — and that value written to `next_due_at` equals the `seen` the claim guards, so the claim succeeds and every check in the fold re-delivers the same reminder with no concurrency involved. `next_fire`, `upcoming` and `occurrences_between` all walk that single stream so the set-time preview, the advance and the coalescing count cannot disagree. Pinned by `dst_fold_second_pass_never_returns_a_past_fire` and `dst_fall_back_repeated_wall_time_delivers_once`.
- This crate's `cron` (0.17) **intersects** day-of-month with day-of-week where Vixie cron unions them, so `0 9 13 * FRI` is Friday-the-13th-only. `cron_dom_dow_conflict()` detects it, `do_set_reminder` reports it through the response `warning`, and the caveat is in the `cron` param description and both SKILL.md copies: an agent writing cron from natural language otherwise produces a schedule that validates, fires, and means something else. (The numeric `1=Sunday` dow numbering is a second, independent trap — `0` is rejected.)
- `concurrent_checks_deliver_a_due_row_exactly_once` (in `crates/alexandria/tests/reminders_test.rs`) spawns four real racing consumers. The repo-layer `record_delivery_claim_*` tests only re-issue a stale `seen` sequentially — they pin the WHERE clause, not the serialization — so don't read them as the concurrency proof, and don't drop the spawned one when the claim shape changes.
- `missed_occurrences` saturates at the engine's `MAX_ITER` (10 000) and ships `missed_occurrences_saturated` beside it. A capped count published as an exact number is wrong in a way no reader can see; the pi companion renders it as `10000+` because of that.
- The `due_reminders` piggyback on `retrieve_memories`/`recall` is read-only and untargeted: up to `DUE_REMINDERS_CAP` (5) due reminders, oldest-due first, regardless of project targeting, each entry carrying its `target` — entries for another project are informational until `check_reminders` delivers/escalates them; targeting, escalation, and coalescing live there alone. The cap is enforced in SQL (`ReminderRepo::list_due_sample`, which also excludes `next_due_at != NONE`) rather than by truncating a full read in Rust, because this runs on every prompt; `DUE_SAMPLE_SLACK` over-reads so one undeliverable row cannot cost a slot.
- `[reminders].escalation_hours` has exactly one default, `alexandria_engine::reminders::DEFAULT_ESCALATION_HOURS`, derived into both the binary's `RemindersConfig::default()` and the MCP `RemindersSettings::default()` and guarded by `test_server_fallback_defaults_match_config_defaults` — the same rule that came out of the `min_similarity` drift. `main.rs` additionally refuses to start on a window too large to represent as a duration, because the delivery path's fallback is to hold project reminders forever.
- Naive datetimes in `set_reminder` are interpreted in `[reminders].timezone` (empty = system-local via iana-time-zone at startup, UTC if detection fails); explicit ISO-8601 offsets always win, and a nonexistent (spring-forward) or ambiguous (fall-back) local time is rejected at *set* time for one-shots, each with its own message naming the fix. Recurring wall times inside a fold are **not** rejected — they deliver once (see the `fires_after` bullet). `RemindersSettings::default().tz` is UTC, not system-local: this crate has no `iana-time-zone` dependency, `main.rs` resolves the operator's zone and injects it for both transports, and the fallback exists so a debug or test-built server stays deterministic.
- Every UTC datetime in reminder tool responses renders via `rfc3339_utc` (`server.rs`): Z-suffixed, seconds precision — `to_rfc3339()` would emit `+00:00`. `next_due_at_local` is the deliberate exception: the same instant spelled in the configured timezone for human confirmation.
- The `schedule_kind` discriminator strings (`once`/`pattern`/`cron`) are constants in `alexandria_storage::models::schedule_kind`, mirrored by the v007 schema `ASSERT`; writers and the engine's `spec_from_reminder` match on the constants so the sides can't drift.
- Schema migrations are forward-only, numbered (`v001`, `v002`, ...), tracked in `system_config` table. Current head is `v007_reminder.surql`; `schema::LATEST_VERSION` is derived from the `MIGRATIONS` table, so it cannot drift from the list, and it is the single source for that number (migration tests assert against it, never a literal). `/debug` renders applied-vs-compiled-in version and calls out a mismatch.
- Every definition statement in a migration is `DEFINE <kind> OVERWRITE …` and every removal is `REMOVE <kind> IF EXISTS …` **because `migrate()` is not atomic across a file**: each migration is one multi-statement query, SurrealDB commits each statement in its own transaction, and `system_config` is stamped only after all pending migrations succeed. A crash mid-file leaves definitions applied with the old stamp, so the next boot re-runs that file from the top — with plain `DEFINE` that is a deterministic `The table 'reminder' already exists` and the server never starts again. `migration_test::test_replaying_a_completed_migration_is_not_an_error` re-applies every compiled-in file and is the gate on the rule; `MIGRATIONS` is `pub` for exactly that test. Note the 3.2 grammar puts the qualifier *after* the kind — `DEFINE OVERWRITE FIELD …` is a parse error.
- Embedding model is locked on first boot — changing `config.toml` model without wiping data will refuse to start.
- The **debug UI is read-only with exactly one sanctioned exception**: the Query Tester's non-dry
  `retrieve` run performs spreading activation, so it writes heat just as a real
  `retrieve_memories` call does. Dry run uses
  `do_retrieve_memories_unfiltered`/`_dry` and writes nothing. That exception is disclosed in the UI
  and load-bearing for its no-auth posture — README's "publish the port only behind a reverse proxy"
  warning rests on "nothing mutates except that one disclosed heat write". Adding any other write
  route breaks the argument and needs that paragraph rewritten first.
- The debug router is guarded by `debug/guard.rs`, applied as a single `middleware::from_fn_with_state`
  layer over the whole router returned by `router_with_context`. **The CSRF half is unconditional** (any
  non-`GET`/`HEAD` request carrying `Sec-Fetch-Site: cross-site`/`cross-origin` gets 403; an absent
  header is allowed, because that is curl and every test) and the **Host half is opt-in** via
  `DebugContext::allowed_hosts`, empty or `"*"` meaning off so the default is unchanged. It is split
  that way on purpose: deriving the CSRF check from `ctx: Some(..)` would give every `router()` caller
  the weaker build silently. **Any new debug route must be added inside `router_with_context`, above
  that `.layer()` call** — a route registered after it, or served by a different router that merges
  this one, escapes both checks. `guard.rs`'s `test_every_debug_route_is_guarded` enumerates every
  guarded path for exactly that reason; extend its list with the route.
- Cluster cohesion in the debug UI comes from the cluster's **stored centroid** (`ClusterRepo::get`), never a recomputed member-average. `main.rs`'s maintenance loop uses the stored centroid, so an approximation made `/debug/clusters/:id` report "Healthy" for a cluster the background task was about to split — a diagnostic surface that disagrees with the mechanism it diagnoses is worse than showing nothing.
- `debug::router(server)` passes `None` for `DebugContext`; only HTTP mode via `router_with_context` populates it. The dashboard's config panel therefore renders an explicit "unavailable outside HTTP mode" state instead of bogus zeros, because `ClusterConfig` lives in the binary crate and structurally cannot reach `alexandria-mcp`. **Do not change `router()`'s signature** — all 61 of its call sites are inside `#[cfg(test)]` modules under `src/debug/`; production is the single `router_with_context` caller (`main.rs`).

## askama Templates & Vendored Assets (Debug UI)

- Every template context struct **must** declare `nav: &'static str`, because `layout.html` reads it in base top-level content. Forgetting it is a compile error, by design — a silently unhighlighted nav is worse.
- **`|safe` is forbidden** in templates. Escaping has to stay structural: askama escapes every `{{ }}` at compile time, so the only remaining decision is *context*. `html::esc()` and `html::layout()` are deleted; there is no helper left to forget to call.
- The `pager` macro in `_pagination.html` is deliberately **presentational**: callers pass complete hrefs (empty string = no link on that side) and their own summary text. It cannot build URLs itself because `maintenance` paginates by `?page=N` while `memories`/`sessions` paginate by offset and must carry their filters across the hop.
- askama 0.16 escapes to **decimal** character references (`&#60;`, `&#62;`, `&#38;`, `&#34;`, `&#39;`), not named entities. Same character set as the deleted `esc()`, different bytes — a test asserting escaped output must expect the decimal form.
- Record ids in an `href` (or any URL) use `|urlencode`; HTML text uses plain `{{ }}`. In `graph.html` the id is interpolated **inside a JavaScript string literal**, where browsers do not decode character references — `|urlencode` is what makes that safe, since its output alphabet is `[A-Za-z0-9_.-~/]` plus `%XX` and so cannot produce a quote, backslash, `<` or newline that would terminate the literal or the `<script>` element.
- Adding a vendored asset means three edits: `assets.rs` (the closed `match` allowlist), `assets/SHA256SUMS`, and `just vendor-assets`. The allowlist is a `match` over literal filenames with `include_bytes!`, so there is no filesystem read or path parsing at request time — traversal is impossible by construction, and the 404 tests exist to keep it that way. Filenames pin the bytes because they are served `max-age=31536000, immutable`; `just verify-assets` is what makes `SHA256SUMS` machine-enforced rather than decorative.

## Testing

- The repo intentionally carries no `rust-toolchain.toml` and no `.cargo/config.toml` — `rust-version = "1.98"` + edition 2024 are the only compiler statement; mold/`target-cpu` live in each dev's `~/.cargo/config.toml` (see TODO-misc "Build / toolchain"). Don't re-add them to the repo.
- Use the `just` recipes (they match CI): `just test` (Rust only), `just ext-test` (pi companion: `npm run typecheck` + `npm test`, needs `just ext-install` once per checkout since `node_modules/` is gitignored), `just lint`, `just fmt`, `just ci` (fmt + lint + test + ext-test + `cargo deny` + `verify-assets`). `.github/workflows/ci.yml` mirrors it with a separate `Pi companion` job — the workflow does not call `just ci`, so a new gate has to be added in both places. `just verify-assets` checks `assets/SHA256SUMS`; `just vendor-assets` re-downloads the assets and re-verifies. `just install-hooks` wires `.githooks/pre-commit`.
- Run tests on **stable**, not nightly: `diskann-wide` (SurrealDB transitive dep) fails trait inference on its NEON intrinsics under recent nightlies on aarch64, and the failure looks like it originates in this workspace. Run the suite with `just test` (Rust) and `just ext-test` (extension); there is also one `#[ignore]`d 130 s wall-clock delivery test that no gate runs (`cargo test -p alexandria --test reminders_test -- --ignored`). Test counts are deliberately not recorded here; they went stale in every doc that carried one.
- All integration tests use `Database::connect_embedded()` (in-memory SurrealDB) — no disk state between tests.
- `CandleProvider` tests download the real model on first run (~80MB) — they're slow the first time.
- Test helpers in `alexandria-storage/src/connection.rs`: `connect_embedded()` for quick in-memory DB.
- Config tests must not touch process env: `Config::load()` delegates to `Config::load_from(&|k| std::env::var(k).ok())`, and tests inject overrides through the closure (`Config::load_from(&env(&[("K", "V")]))`, local helper in `config.rs`). No `serial_test`, no `#[serial]`, no ambient-env races.
- `debug/test_support.rs` has two embedding stubs, and the difference matters. `StubEmbedding` returns a constant vector, so **every similarity is 1.0** and a `min_similarity` floor can never filter anything — useless for floor tests. `BandedEmbedding` (via `banded_server()`) emits `[s, √(1−s²)]`, so cosine against the query's `[1,0]` is exactly `s` — deterministic straddling of a floor. Do not change `StubEmbedding` or `test_server()`; 64 call sites depend on them.
- `HeatRepo::add_heat` is an `UPDATE heat_state ... WHERE memory = ...` that **silently matches zero rows** when the memory has no `heat_state` row, and its caller swallows the result with `.ok()`. `MemoryRepo::create_fact` creates no heat row, so a "did not write heat" assertion built on it passes **vacuously**. Seed with `do_store_memory` (which writes `heat_state` at 1.0) plus `EdgeRepo::create_edge`, and always pair a "no write" assertion with a positive control proving the fixture can write at all.
- The pi extension has its own suite: `cd contrib/pi/extensions/alexandria && npm run typecheck && npm test` (node:test + tsx, hermetic — a controlled `ALEXANDRIA_CLIENT_CONFIG`, restored `process.env`, no server or socket). `just ext-test` runs it and `just ci` includes it, so it is not optional. The detector regexes and the LLM extraction prompt are still unguarded — that is the remaining gap, and the reason the README says so.

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
- Nothing watched the branch while CI was red for 11 days, because no check is required to merge.
  Revisit branch protection if regressions keep landing.
- `.github/workflows/ci.yml` is **four separate jobs** (`fmt`, `clippy`, `test`, `deny`) and does
  **not** call `just ci`. Editing the justfile alone therefore changes nothing on GitHub Actions — a
  new gate has to be added to the workflow too. `just verify-assets` runs as its own step in the
  `test` job, before `just test`, because the year-long `immutable` asset cache header is only safe
  while the filename pins the bytes.

## Docs Map

- `README.md` — feature/tool overview, quick start, deployment (systemd, Docker), debug UI
- `docs/configuration.md` — every server and client config key, env overrides, XDG migration
- `docs/session-memory.md` — session data model, lifecycle, tool semantics, current limitations
- `docs/minilm-test-data.md` — retrieval measurements for the embedding model, how to rerun
  `alexandria bench-retrieval`, metric definitions, and the frozen question set
- `docs/roadmap.md` — shipped milestones and planned work
- `docs/security-findings.md` — 2026-09-10 audit: threat model (memory as a prompt-injection
  persistence layer), the convex-hull paper verdict, ranked findings S1–S6 with file:line refs
- `docs/performance-and-ability-findings.md` — same audit: the 128-token truncation measurement,
  inert heat model, O(N) cluster counting, ranked findings A1–A4 / P1–P5
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
