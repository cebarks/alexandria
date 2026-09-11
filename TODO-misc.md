# TODO (misc)

Open code items. Rationale for settled decisions lives in the docs and commit history, not here.

## Server

- [-] **`raw` record carries no session.** `import_document` links the chunks to the session; the
  `raw` document is reachable only via `extracted_from`. `contains_session_memory` is `IN session OUT
  fact`, so linking `raw` needs a new edge table plus a migration, and nothing reads it. Add one if a
  session view ever needs the source document directly.
- [-] **No debug UI page for sessions.** `/debug` covers memories, clusters, graph, maintenance, and
  query; sessions are reachable only through the MCP tools. `SessionRepo::list` already returns
  everything a list view would show.
- [-] **`list_sessions` cannot search summaries.** Filters are `agent_id` / `tag` / `finalized`
  only. Substring `CONTAINS` on `summary` is one clause; semantic search would mean embedding
  summaries on finalize, which is a schema change.
- [-] **`CandleProvider::set_cls_pooling` is public API that exists only for one test.**
  Integration tests cannot see `cfg(test)` items, so it is `pub` behind `#[doc(hidden)]`. A
  `test-util` cargo feature would hide it properly; add one if a second such hook appears.

- [-] **`alexandria-mcp` still issues one inline SurrealDB query.** The `provenance` create in
  `do_store_memory` (`server.rs`) bypasses the storage crate. It is maintained by someone else, so
  leave it; do not add new ones.
- [-] **Cluster `member_count` is one query per cluster.** `load_cluster_infos()` calls
  `get_members()` for each cluster. Batch it when cluster counts grow.
- [-] **`recall` walks clusters, not sessions.** Sessions are reachable only through the session
  tools.
- [-] **The model fetcher (`alexandria-pipeline/src/embedding/hub.rs`) is snapshot-only.** No
  `blobs/` symlinks, `.no_exist` markers, or locks in the cache layout; revision `main` only; no
  `HF_TOKEN` / `HF_ENDPOINT`, so gated or private models cannot be fetched and a cached revision is
  served until its dir is deleted. Add whichever one bites.

### `bench-retrieval`

- [-] **The HNSW overlap check covers `RECALL_LIMIT` only.** `report_hnsw_overlap()` asks the index
  for the top 10; the `limit x threshold` grid's other rows (3, 5, 8, 15, 20) are still exact-scan
  numbers with no index counterpart. Sweep `LIMITS` through `nearest()` if a wider limit ever
  becomes a candidate default.
- [-] **The baseline is reconstructed by size, not recorded.** `BASELINE_SIZE = 143` takes the 143
  oldest active facts. Deleting a fact inside that window lets it reach forward, and `update_memory`
  keeps the record ID while rewriting content, so a frozen `QUESTIONS` target can silently start
  measuring different text with every metric still looking comparable. If the baseline row stops
  reproducing, suspect this before the metrics.
- [-] **A restated target scores as a miss.** Rank is by record ID, so when a duplicate memory
  outranks the target the bench records the target's rank even though the user got the answer at
  rank 1. MiniLM cannot separate duplicates from adjacent memories by score, so this is not
  detectable automatically. Treat rank as a lower bound on delivery, and read the results above
  the target before acting on a headroom WARN.
- [-] **The recall defaults are literals in three places.** `contrib/claude/hooks/alexandria-recall.sh`
  and `contrib/pi/extensions/alexandria-auto-recall/src/config.ts` are authoritative; `crates/alexandria/src/bench.rs`
  restates them as `RECALL_LIMIT` / `RECALL_THRESHOLD` for the headroom check. Nothing ties them
  together. When changing one, grep the tree for the old value.
- [-] **`config.ts` recall defaults differ from upstream and merge without a conflict.**
  `contrib/pi/extensions/alexandria-auto-recall/src/config.ts` ships `0.45` / `10` against
  upstream's `0.58` / `5`. A one-sided edit only conflicts if upstream touches the same lines, so a
  sync can revert both silently. Grep the file for both values after every upstream sync.

## Build / toolchain

- [-] **`just install-hooks` copies the hook instead of symlinking it.** `.git/hooks/pre-commit` is
  a snapshot, so every edit to `.githooks/pre-commit` needs a re-run and nothing warns that the
  installed copy is stale. A symlink breaks on Windows checkouts without developer mode; leave the
  copy unless staleness bites.
- [ ] **`.cargo/config.toml` and `rust-toolchain.toml` were dropped when this branch was integrated
  into main (2026-09-09).** Decision: linker/CPU flags are per-machine developer preference, not
  repo policy — the file only ever affected local x86-64 Linux gnu builds (CI's ubuntu jobs would
  have broken on the missing `mold`, and the Docker build already overrides `rustflags` via
  `RUSTFLAGS`, so neither `target-cpu` nor mold applied there). Dev boxes wanting it keep
  `-C target-cpu=native` + mold in `~/.cargo/config.toml`. Windows `target-cpu` verification is
  moot. Repo keeps `rust-version = "1.98"` + edition 2024 as the only compiler floor statement.

## Dependencies

- [-] **`tokenizers` is pinned to 0.22 by candle-core 0.11.** 0.23 builds a second copy, and
  candle-core enables the `onig` feature itself, so our `default-features = false` cannot drop the C
  build. Both move together on the next candle bump.
- [-] **`RUSTSEC-2023-0071` (Marvin attack in `rsa`) is ignored in `deny.toml`.** `rsa` 0.9.10
  reaches us via `surrealdb-core -> jsonwebtoken`; nothing here uses RSA. No patched release exists
  (0.10 is still a release candidate). Drop the ignore once `cargo deny` stops needing it, i.e.
  when surrealdb picks up a `jsonwebtoken` built on `rsa` 0.10.

## Pi extension

- [-] **An assistant reply to a text-less user message shares the previous turn number.**
  `serializeEntries` only advances `turnNum` on user text, so an image-only user message and the
  assistant's answer to it are both labelled with the prior turn. Label the assistant line by its own
  counter if the extraction prompt ever starts misattributing answers.
- [-] **Store failures notify once per call.** `notifyStoreFailed` in `index.ts` fires for every
  rejected `storeMemory`, so a server that rejects everything at shutdown produces one toast per
  extracted memory. Collapse the shutdown loop to a single count if that gets noisy.
- [-] **Auto-recall is not session-scoped.** `retrieveMemories` never passes `session_id`, so a
  resumed pi session recalls across everything. Pass `ctx.sessionManager.getSessionId()` if
  same-session recall ever matters more than cross-session recall.
- [-] **`typecheck-pi` checks against whatever `pi-coding-agent` the lockfile holds.**
  `package.json` says `latest` but `npm ci` installs the locked `0.84.2`, so the types only move
  when someone runs `npm install` or Dependabot bumps the lock. A failing typecheck after a lock
  bump means upstream changed `ExtensionAPI`, not that our code regressed; pin the version if that
  starts happening.

## Claude Code integration

- [-] **The Pi `error-resolution` tag is not ported.** `alexandria-extract.sh` serializes `is_error`
  tool results as `[Tool error]: <tool> ...` and leaves pairing and root-cause judgement to haiku.
  Permission denials and user rejections go in too. Add a tag filter only if junk memories of that
  shape appear. `contrib/claude/README.md` records the same non-port; keep the two in step.
- [-] **Stop-hook extraction makes one haiku call per turn; the retry on an empty result was
  dropped.** It rested on one observation and doubled the cost of every turn. If `extracted` volume
  drops noticeably, restore the loop gated on transcript size, not unconditionally.
- [-] **The entrypoint gate is a denylist read off the 2.1.263 bundle.** Off: `sdk-*`, `mcp`,
  `bench`, `claude-code-github-action`, `claude-security`, `*_trigger`, and Cowork (`local-agent`,
  `claude-coworker*`, `remote_cowork`). An allowlist on `cli` was rejected because it would silently
  turn memory off in `claude-desktop` and `claude-vscode`. A new headless entrypoint lands on by
  default until someone re-reads the validator table; re-grep `$Yt={cli:!0,...}` in
  `/opt/claude-code/bin/claude` after upgrades that add surfaces.
- [-] **The entrypoint `case` and the marker-prune `find` are duplicated by hand in both hooks.**
  `alexandria-recall.sh` and `alexandria-extract.sh` carry identical lines and nothing checks they
  match. Factor a sourced helper out if a third hook needs them or either expression changes.
- [-] **Cross-session extraction dedup covers only what the prompts recalled.**
  `alexandria-extract.sh` feeds the recall hook's `hook_additional_context` hits into
  `<already_stored>`; a post-hoc similarity filter does not work on MiniLM (duplicates and distinct
  neighbours both score 0.63-0.76), so haiku has to judge. Not covered: gotchas that surface only
  from tool output, sessions with `ALEXANDRIA_AUTO_RECALL=off`, and hits recalled in an earlier
  chunk of the same session. If duplicates of that shape keep appearing, the next step is one
  `retrieve_memories` per candidate with the top hits fed to a second, smaller haiku call.
