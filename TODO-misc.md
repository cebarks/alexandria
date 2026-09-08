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
