# TODO (misc)

Open code items. Rationale for settled decisions lives in the docs and commit history, not here.

## Server

- [-] **`raw` record carries no session.** `import_document` links the chunks to the session; the
  `raw` document is reachable only via `extracted_from`. `contains_session_memory` is `IN session OUT
  fact`, so linking `raw` needs a new edge table plus a migration, and nothing reads it. Add one if a
  session view ever needs the source document directly.
- [-] **`list_sessions` cannot search summaries.** Filters are `agent_id` / `tag` / `finalized`
  only. Substring `CONTAINS` on `summary` is one clause; semantic search would mean embedding
  summaries on finalize, which is a schema change.
- [-] **`CandleProvider::set_cls_pooling` is public API that exists only for one test.**
  Integration tests cannot see `cfg(test)` items, so it is `pub` behind `#[doc(hidden)]`. A
  `test-util` cargo feature would hide it properly; add one if a second such hook appears.

- [-] **`alexandria-mcp` still issues one inline SurrealDB query.** The `provenance` create in
  `do_store_memory` (`server.rs`) bypasses the storage crate. Do not add new ones.
- [-] **Cluster `member_count` is one query per cluster.** `load_cluster_infos()` calls
  `get_members()` for each cluster. Batch it when cluster counts grow.
- [-] **`recall` walks clusters, not sessions.** Sessions are reachable only through the session
  tools and `/debug/sessions`.
- [-] **The model fetcher (`alexandria-pipeline/src/embedding/hub.rs`) is snapshot-only.** No
  `blobs/` symlinks or `.no_exist` markers in the cache layout (locks *are* handled: a cache miss
  takes an advisory lock on `<repo>/.lock` and re-checks); revision `main` only; no
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
- [-] **A restated target scores as a miss.** Rank comes from exact cosine — `better` counts only
  strictly-higher scores, so ties resolve optimistically — and `sweep` sorts by score, not by record
  ID. When a duplicate memory outscores the target the bench still records the target's rank, even
  though the user got the answer at rank 1. MiniLM cannot separate duplicates from adjacent memories
  by score, so this is not detectable automatically. Treat rank as a lower bound on delivery, and read
  the results above the target before acting on a headroom WARN.
- [-] **The recall defaults are literals in three places.**
  `contrib/claude/hooks/alexandria-recall.sh`,
  `contrib/pi/extensions/alexandria/src/config.ts` and `crates/alexandria/src/bench.rs`
  (`RECALL_LIMIT` / `RECALL_THRESHOLD`) all ship `10` / `0.45` now that #17 and #18 have landed, so
  they currently agree. Nothing *ties* them together, though: when changing one, grep the tree for the
  old value.

## Build / toolchain

- [-] **`just install-hooks` copies the hook instead of symlinking it.** `.git/hooks/pre-commit` is
  a snapshot, so every edit to `.githooks/pre-commit` needs a re-run and nothing warns that the
  installed copy is stale. A symlink breaks on Windows checkouts without developer mode; leave the
  copy unless staleness bites.
- [-] **`.cargo/config.toml` and `rust-toolchain.toml` are deliberately absent (decided
  2026-09-09).** Decision: linker/CPU flags are per-machine developer preference, not
  repo policy — the file only ever affected local x86-64 Linux gnu builds (CI's ubuntu jobs would
  have broken on the missing `mold`, and the Docker build already overrides `rustflags` via
  `RUSTFLAGS`, so neither `target-cpu` nor mold applied there). Dev boxes wanting it keep
  `-C target-cpu=native` + mold in `~/.cargo/config.toml`. Windows `target-cpu` verification is
  moot. Repo keeps `rust-version = "1.98"` + edition 2024 as the only compiler floor statement.

## Dependencies

- [-] **`tokenizers` is pinned to 0.22 by candle-core 0.11.** The lockfile carries exactly one
  `tokenizers` (0.22.2) with `onig` among its dependencies, and candle-core enables that feature
  itself, so our `default-features = false` does not drop the C build. A 0.23 alongside it would build
  a second copy; the two move together on the next candle bump.
- [-] **`RUSTSEC-2023-0071` (Marvin attack in `rsa`) is ignored in `deny.toml`.** `rsa` 0.9.10
  reaches us via `surrealdb-core -> jsonwebtoken`; nothing here uses RSA. No patched release exists
  (0.10 is still a release candidate). Drop the ignore once `cargo deny` stops needing it, i.e.
  when surrealdb picks up a `jsonwebtoken` built on `rsa` 0.10.

## Pi extension

- [-] **`tool-tracker.ts` matches the tool name, not the server.** After the exact-match fix, a tool
  called `store_memory` on a *different* MCP server (`mcp__agentmemory__store_memory`) still feeds the
  dedup buffer, and the single-`_` prefix fallback mis-strips names like `bulk_store_memory`. Both were
  accepted knowingly during the PR #41 review rather than fixed, because scoping to `alexandria` needs
  the server's registered name, which differs per client (`mcp__alexandria__*` on pi ≥0.99,
  `alexandria_*` on adapter 2.x) and the companion does not know which key an operator chose. If a
  second memory server ever enters the fleet, pin the server segment and add the fixture. The asymmetry
  with `isTransient()` is then visible, and it is deliberate-but-imperfect: that function *does* key on
  the server name (`toolName.includes("alexandria")`), so it is right only when the operator registered
  the server under a name containing it. pi ≥0.99 does emit nested MCP calls under their own name (see
  `toolName` in pi's `dist/core/nested-tool-calls.js`), so `codemode` exposure does not break it — but a
  server keyed `mem` loses its own API evidence again, and `mcp__alexandria-indexer__*` is wrongly
  exempted. Both accepted; both die if the fleet ever standardises the server key.
- [-] **Nothing gates pi's *behavioural* drift, only its types.** `@earendil-works/pi-coding-agent` is
  now exact-pinned, so a bump is a deliberate lockfile edit rather than background float, and CI's
  `npm run typecheck` does catch a bump that breaks the type boundary. What nothing catches is a change
  that compiles and still behaves differently: the `0.87` provider-contract change and the `0.99` MCP
  rebuild were both type-compatible and both changed what the companion sees. `just ext-check-pi` was
  added mid-review and removed again in the same round — it compared the *installed* copy rather than
  the declared pin, and claimed a place in `just ci` it never had, so it could print OK while the
  committed pin was stale. Replacing it needs both fixed: read the pin from `package.json`, treat a
  pin-vs-lock disagreement as its own failure, and make "no host pi" a skip rather than an error so it
  can actually live in `ci`. The real gap is a smoke job that loads the extension under the host pi and
  asserts what it registers — see the next-but-one entry.
- [-] **Seven lockfile entries have no `integrity`.** The `@earendil-works/*` packages inside pi's
  subtree (`chord`, `pi-agent-core`, `pi-ai`, `pi-codemode`, `pi-mcp`, `pi-telemetry`, `pi-tui`) are
  pinned by URL and version only, because pi ships `npm-shrinkwrap.json` and its generator drops
  digests for same-version workspace siblings — `npm` copies that omission forward. These are exactly
  the `.d.ts` files the `Pick<ExtensionContext, …>` boundary resolves through. Guard: a check that
  fails if any `package-lock.json` entry lacks `integrity` (same shape as `just verify-assets`), plus
  the upstream ask. Note the asymmetry: Rust has `cargo-deny` for advisories *and* licences; the npm
  tree has neither, and `deny.toml`'s licence allowlist would reject `BlueOak-1.0.0` and `0BSD` today
  if it were applied — substantively harmless, but it shows npm sits outside the policy.

- [-] **An assistant reply to a text-less user message shares the previous turn number.**
  `serializeEntries` only advances `turnNum` on user text, so an image-only user message and the
  assistant's answer to it are both labelled with the prior turn. Label the assistant line by its own
  counter if the extraction prompt ever starts misattributing answers.
- [-] **Auto-recall is not session-scoped.** `retrieveMemories` never passes `session_id`, so a
  resumed pi session recalls across everything. Pass `ctx.sessionManager.getSessionId()` if
  same-session recall ever matters more than cross-session recall.
- [-] **`just ext-test` type-checks against whatever `@earendil-works/pi-coding-agent` the lockfile
  holds.** The lock now pins `0.99.1`, matching the running pi, and the extension's context boundary
  is declared as `Pick<ExtensionContext, …>` with no casts at the production call sites — so a pi
  rename or removal fails `npm run typecheck` instead of failing at runtime. A typecheck that starts
  failing after a lockfile bump still means upstream changed `ExtensionAPI`, not that our code
  regressed. The remaining gap is pi's *behaviour*, which no typecheck covers: the `0.87` provider
  contract change and the `0.99` MCP rebuild were both type-compatible and both changed what the
  companion sees.
- [-] **ErrorTracker's transient gate is bounded by what it can prove is noise.** Per-family counts
  live *only in this entry* — code comments and AGENTS.md point here rather than repeating them,
  because a number copied into three files is a number nobody re-derives. Measured 2026-09-30 on the
  live store: this path had written 526 of 1440 live memories (36.5%), collapsing to 396 classes of
  which 351 are singletons. Dominant families: tool-schema rejections ×85, `edit PARTIAL APPLY` ×47,
  pi-lens `RETRYABLE —` ×44, gateway argument errors ×26, `fatal: not a git repository` ×8. Tier-2
  candidates counted but not shipped, since each is a judgement about human data rather than a tool
  contract: `Command exited with code N` ×43, `No such file or directory` ×22, `unexpected EOF` ×12,
  `Permission denied` ×4, `command not found` ×3, `unrecognized option` ×3, `HTTP 4xx` ×1,
  `invalid argument:` ×1.
  Carry this caveat with the numbers: they come from replaying **already-stored** rows through
  `ErrorTracker`, so the input was already normalised and truncated, and two populations were used
  (124 session-attached rows with full content; 517 parseable dashboard previews out of the same 526).
  Hence a range, ~51–61% would not have been written, not a constant — and no committed script
  reproduces it. The earlier 43% was worse than imprecise: it came from a survey script that
  re-implemented the patterns *beside* the code, which disagreed with the shipped gate for two reasons
  (raw vs normalised text, and elision deleting the error signal). If this number matters again,
  commit the replay script, run it over raw `tool_execution_end` text, and state the population.
- [-] **ErrorTracker's premise fails for general executors, and no pattern list fixes that.** Of the
  235 rows surviving both tiers, 149 are `bash` (plus `grep` 19, `ctx_execute` 10). `event.isError`
  for bash means *the command exited non-zero*, so `extractResultText` stores whatever stdout came
  back, and `ERROR_SIGNAL_PATTERN` matches ordinary output containing an error-shaped word. Real
  rows currently in the store that are not errors at all: a passing `cargo test … 0 failed` captured
  because the compound command exited non-zero; a `git log --oneline` dump; `movie units 21 missing
  598.6 GB have 546.3 GB`; a successful `lsar` listing. The fix is structural, not another regex:
  track only tools that return an error *message* (skip bash/grep/ls/find and the ctx_* sandbox
  tools), or require message-shaped text (single leading line, no multi-line payload). Worth pairing
  with the consolidation/"dreaming" pass, which needs the same class keys the normalizer already
  produces.

## Claude Code integration

- [-] **The Pi `error-resolution` tag is not ported.** `alexandria-extract.sh` serializes `is_error`
  tool results as `[Tool error]: <tool> ...` and leaves pairing and root-cause judgement to haiku.
  Permission denials and user rejections go in too. **The junk this warned about has now been
  measured on the pi side** — 526 rows, 36.5% of the store, dominated by tool-protocol noise
  (`Validation failed for tool`, `PARTIAL APPLY`, `Path not found`, `not a git repository`) — and pi
  filters it with `isTransient()` in `src/detectors/error-tracker.ts` — two lists, not one:
  `TRANSIENT_PATTERNS` (unconditional) and `CALL_SHAPE_PATTERNS` (applied only when the tool name does
  NOT reference this server, because a validation rejection from alexandria's own tools is a durable API
  contract). Two of the unconditional families are pi-lens's wording, so a porter cannot assume the
  same strings appear in Claude's transcript. The Claude hook has the same problem and no gate: port
  `isTransient()`'s shape or make the haiku prompt reject it, and keep the two clients' noise floors
  comparable. `contrib/claude/README.md` records the divergence; keep them in step.
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
  `<already_stored>`; a post-hoc similarity filter does not work on MiniLM (`docs/minilm-test-data.md`,
  "Duplicate bar"), so haiku has to judge. Not covered: gotchas that surface only
  from tool output, sessions with `ALEXANDRIA_AUTO_RECALL=off`, and hits recalled in an earlier
  chunk of the same session. If duplicates of that shape keep appearing, the next step is one
  `retrieve_memories` per candidate with the top hits fed to a second, smaller haiku call.
