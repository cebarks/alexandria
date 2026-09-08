# TODO (misc)

Open items noticed while getting Alexandria running under Claude Code (2026-09-08).

## Retrieval quality

- [ ] **Embedding model is the real ceiling.** `all-MiniLM-L6-v2` is a symmetric similarity
  model. A natural-language question against a stored statement scores only ~0.1–0.2
  cosine ("which database does the project use" vs "the project uses SurrealDB" = 0.19),
  while a keyword hit scores ~0.6. The floor was lowered to 0.10 to compensate, but an
  asymmetric retrieval model (e.g. an msmarco/bge/e5 family model) would separate real
  matches from noise far better. Blocked on: model is locked on first boot, so switching
  the default needs a migration/re-embed story.
  Measured 2026-09-08 on the live corpus (143 facts, 12 questions; see
  `docs/plans/2026-09-08-embedding-model-swap-measurements.md`): none of msmarco-MiniLM-L6-cos-v5,
  multi-qa-MiniLM-L6-cos-v1, or bge-small-en-v1.5 beat MiniLM (mean rank 1.42 vs 2.33 for the best
  challenger, mean gap +0.148 vs +0.091). Default unchanged. bge-small scores higher in absolute
  terms (hit_min 0.620 vs 0.338) but its noise floor rises just as much (nonhit_p50 0.564 vs 0.077),
  so separation is worse. The blocker is gone: `alexandria migrate-embeddings` re-embeds an existing
  database and CLS-pooled models load, so a future candidate is a config change plus one command.
  Note bge was measured without its query instruction prefix.

## Server

- [ ] **`tests/migration_test.rs` hardcodes the latest schema version.** Noticed 2026-09-08 while adding
  v006: two asserts compare `schema_version` to a literal string and must be bumped with every new
  migration. `MIGRATIONS` is private to `schema/mod.rs`; exposing a `LATEST_VERSION` const would
  remove the churn. Cosmetic, do it the next time a migration lands.
- [ ] **Session find-or-create is duplicated.** `do_store_memory` and `do_import_document` (2026-09-08)
  each carry the same find-by-external-id, create-if-missing, re-find, unwrap-record-id block. Two
  copies is tolerable; on a third caller move it into `SessionRepo::find_or_create` returning the
  record id string.
- [ ] **`raw` record carries no session.** The 2026-09-08 `import_document` session linkage attaches
  the chunks only; the `raw` document record is reachable from them via `extracted_from` but has no
  session edge of its own. Add one if a session view ever needs the source document directly.

### Embedding migration follow-ups (deferred from the 2026-09-08 branch review)

- [ ] **`migrate-embeddings` has never run against a real SurrealKV database.** Only the fake-provider
  test on `kv-mem` and an empty scratch dir exercised it. Before first real use: stop the service,
  `cp -a` the data dir to `/tmp`, run it there with `ALEXANDRIA_DATA_DIR` pointed at the copy and
  `ALEXANDRIA_EMBEDDING_MODEL=sentence-transformers/multi-qa-MiniLM-L6-cos-v1`, then migrate the copy
  back to MiniLM. Throw the copy away.
- [ ] **`migrate.rs` rough edges.** `&vecs[0]` is an unchecked index on provider output
  (`vecs.first().context(..)?`); `ReembedOutcome` derives no `Debug`; one `embed()` call per fact, no
  batching (wall-clock only); `list_with_counts()` is called and the counts discarded, then
  `get_members` re-fetched (2N queries). Module comment at lines 2-3 says a partial failure "keeps
  refusing to boot", which is only true while config still names the new model.
- [ ] **Empty clusters keep a stale-dimension centroid** after a dimension-changing migration.
  `engine::search::cosine_similarity` only `debug_assert`s equal lengths, so release builds would
  silently score a truncated dot product. Near-unreachable (split/merge delete their originals).
- [ ] **"Fresh database" is inferred purely from a missing lock.** A database predating the v003
  `system_config` table has facts and no lock; `migrate-embeddings` would say "just start the
  server" and the next boot stamps the current model over old vectors. Guard: if there are facts
  and no lock, say so instead. Almost certainly nonexistent in the wild.
- [ ] **CLI ignores trailing arguments.** `alexandria migrate-embeddings --dry-run` runs a real
  migration (`args().nth(1)`). Reject `args().count() > 2`. `--help` errors rather than prints usage.
- [ ] **Pooling-config warn text** in `candle.rs` says "no 1_Pooling/config.json" even when the cause
  was a network failure; the real cause is only in the interpolated `{e}`.
- [ ] **Spec defect: threshold-derivation rule has no valid solution when `nonhit_p99 > hit_min`.**
  The design spec's `retrieve.min_similarity` rule (and its midpoint fallback) lands above `hit_min`
  on this corpus (0.373 vs 0.338), so any derived floor cuts a true hit. Rewrite the rule before the
  next model bench (see `docs/plans/2026-09-08-embedding-model-swap-measurements.md`).
- [ ] **`CLAUDE.md` says `record_id_to_string()` lives in `alexandria-mcp/src/server.rs`.** It lives
  in `alexandria-storage/src/lib.rs` and `server.rs` only re-exports it. Fix the note.
- [ ] **Test gaps, low priority.** No test asserts CLS output differs from mean output for the same
  model; the `reembed` centroid test uses a constant fake vector so it cannot distinguish mean from
  copy-first-member; no test for lock-present-over-empty-corpus (`Done { 0, 0 }`);
  `all_ids_and_content` test assertions are positional and could flip on same-tick `created_at`.

## Build / toolchain

- [ ] **Windows `rustflags` (msvc/gnu/gnullvm targets) added 2026-09-08 but unverified.** `.cargo/config.toml`
  sets `target-cpu=x86-64-v2` for the three Windows targets alongside the Linux `mold` target; there's
  no Windows toolchain in this environment to cross-compile and confirm they take effect.
- [ ] **`[profile.release]` (lto = "thin", codegen-units = 1, strip) added 2026-09-08, never built.**
  Only `cargo build --workspace` (dev profile) has been run since the toolchain/profile changes;
  do a `cargo build --release` smoke test before shipping a release artifact.

- [ ] **Startup memory doubled during model load (2026-09-08).** `candle.rs` now reads the safetensors
  file into a `Vec<u8>` (`from_buffered_safetensors`) instead of mmap, so the workspace can carry
  `unsafe_code = "forbid"`. For MiniLM (~90 MB) the buffer plus the built tensors coexist briefly at
  boot, then the buffer drops. Revisit only if a much larger model is adopted; the escape hatch is a
  `#[allow(unsafe_code)]` on that one call plus `from_mmaped_safetensors`.
- [ ] **`ALEXANDRIA_EMBEDDING_DEVICE` env override has no test.** Noticed 2026-09-08 while moving the
  config tests off process env; the other five overrides are covered by `test_env_overrides` /
  `test_server_env_overrides`, this one is not. One-line addition to `test_env_overrides`.

## Dependencies

- [ ] **Blocked `cargo update` targets (semver-incompatible, need Cargo.toml bump).** `cargo update
  --verbose` (2026-09-08) only had compatible updates to apply; these are "Unchanged" because the
  available version is a breaking bump past what `Cargo.toml` allows. Direct deps: `base64` 0.22.1 →
  0.23.1, `dirs` 6.0.0 → 7.0.0, `toml` 0.8.23 → 1.1.5, `tokenizers` 0.22.2 → 0.23.2, `hf-hub` 0.5.0 →
  1.0.0, `serial_test` 3.5.0 → 4.0.1 (dev-dep). Transitive (bump the pulling crate, not these
  directly): `generic-array` 0.14.7 → 0.14.9, `i_float`/`i_overlay`/`i_shape` (geometry stack, likely
  via a shared dep) 1.15.0/4.0.7/1.14.0 → 1.16.0/4.5.2/1.18.0, `matchit` 0.8.4 → 0.8.6, `pdqselect`
  0.1.0 → 0.1.1. Each needs its own bump + changelog check before touching `Cargo.toml`; expect
  breaking changes in the major-version jumps (`toml` 0.8→1.x, `hf-hub` 0.5→1.x, `dirs` 6→7,
  `serial_test` 3→4) — deconflict one at a time, not as a batch.

## Claude Code integration

- [ ] **Error-resolution tracker not ported.** `contrib/claude/hooks/` now has auto-recall,
  session_id injection, correction/preference detectors, and Stop-hook LLM extraction
  (plan: `docs/plans/2026-09-08-todo-misc-plan.md`). The Pi error-resolution detector was
  deliberately skipped: it needs PostToolUse state across a turn and yields low-signal
  "Error with X / Resolution: <200 chars>" memories; the extraction pass captures root causes
  once resolved. Revisit only if extracted memories turn out to miss resolved errors.
- [ ] **Retry doubles cost on tactical turns.** Every turn where haiku correctly finds nothing now pays a
  second call (up to ~80 s wall, hidden by async). Watch the `extracted` volume; if it is mostly
  noise or the cost matters, drop the retry or gate it on transcript size.
- [ ] **Headless experiments write to the live server.** Any `claude -p` run on this machine fires the
  installed hooks, so a stub extractor's output (or haiku's) lands in the real database under a
  throwaway session id; 2026-09-08 testing left six `stub` memories that had to be deleted by hand.
  Prefix experiments with `ALEXANDRIA_AUTO_STORE=off` or point `ALEXANDRIA_URL` at a scratch server.
- [ ] **Hook development in a live interactive session pollutes the real database.** Companion to the
  headless item above: the installed Stop hook extracts from this session's transcript too, so stub
  payloads and probe strings from tests pasted into the conversation become `extracted` memories (a
  "Detach debug probe" memory from 2026-09-08 surfaced in auto-recall today). `test.sh` itself is
  clean (own session id, deletes on exit); the leak is the interactive session around it. Mitigation
  is the same: `ALEXANDRIA_AUTO_STORE=off` in the developing session's env, or delete by hand.
- [ ] **A queued follow-up prompt lands in the previous turn's chunk.** If the user types the next
  prompt while a turn is still generating, Claude Code dispatches it as soon as the turn ends, inside
  the 1 s flush wait, so the extract hook sees it with the previous turn. Harmless (it is extracted
  once, just one turn early); noted so it is not mistaken for a marker bug.
