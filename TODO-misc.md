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

- [ ] **Session find-or-create is duplicated.** `do_store_memory` and `do_import_document` (2026-09-08)
  each carry the same find-by-external-id, create-if-missing, re-find, unwrap-record-id block. Two
  copies is tolerable; on a third caller move it into `SessionRepo::find_or_create` returning the
  record id string.
- [-] **`raw` record carries no session.** The 2026-09-08 `import_document` session linkage attaches
  the chunks only; the `raw` document record is reachable from them via `extracted_from` but has no
  session edge of its own. Parked 2026-09-08: `contains_session_memory` is declared `IN session OUT fact`,
  so linking `raw` needs a new edge table plus a schema migration, and nothing reads it. Add one if a
  session view ever needs the source document directly.

### Embedding migration follow-ups (deferred from the 2026-09-08 branch review)

- [ ] **`migrate-embeddings` has never run against a real SurrealKV database.** Only the fake-provider
  test on `kv-mem` and an empty scratch dir exercised it. Before first real use: stop the service,
  `cp -a` the data dir to `/tmp`, run it there with `ALEXANDRIA_DATA_DIR` pointed at the copy and
  `ALEXANDRIA_EMBEDDING_MODEL=sentence-transformers/multi-qa-MiniLM-L6-cos-v1`, then migrate the copy
  back to MiniLM. Throw the copy away.
- [ ] **`migrate.rs` batch size is a hardcoded 32** (2026-09-08, added with fact batching). Fine for
  MiniLM on CPU; make it a config knob only if a larger model or GPU makes a different size matter.
- [x] **`ClusterRepo::list_with_counts` swallows `get_members` errors** (`unwrap_or(0)`), so a failing
  membership query reads as an empty cluster. Done 2026-09-08 (bfaa52a): the error now propagates; the
  debug clusters page already rendered it.
- [x] **Empty clusters keep a stale-dimension centroid** after a dimension-changing migration.
  Done 2026-09-08 (5326677): `migrate-embeddings` deletes empty clusters instead of skipping them.
  `engine::search::cosine_similarity` still only `debug_assert`s equal lengths; no other path leaves
  a stale centroid behind (split/merge delete their originals).
- [x] **"Fresh database" is inferred purely from a missing lock.** Done 2026-09-08 (df7db27):
  `migrate-embeddings` now errors when facts exist without a lock and tells the user to boot once with
  config naming the model that produced them. The server boot path itself still stamps whatever the
  config names over an unlocked corpus; see the open item below.
- [x] **`--help` output is preceded by a tracing INFO line.** Done 2026-09-08 (ed923ee). `tracing_subscriber::fmt::init()` and the
  "Alexandria v0.2 starting..." log run before argument parsing (2026-09-08), so `alexandria --help`
  prints a log line to stderr before the usage. Cosmetic; move the subscriber init below the arg
  match if it bothers anyone.
- [-] **`migrate-embeddings` no longer logs "Alexandria v0.2 starting..."** (2026-09-08, side effect of
  ed923ee): the subcommand returns from inside the argument match, before the startup log line. It
  still logs its own progress. Accepted; add a line at the top of `migrate_embeddings()` if it matters.
- [x] **Pooling-config warn text** (done 2026-09-08, 843f325) in `candle.rs` says "no 1_Pooling/config.json" even when the cause
  was a network failure; the real cause is only in the interpolated `{e}`.
- [ ] **Spec defect: threshold-derivation rule has no valid solution when `nonhit_p99 > hit_min`.**
  The design spec's `retrieve.min_similarity` rule (and its midpoint fallback) lands above `hit_min`
  on this corpus (0.373 vs 0.338), so any derived floor cuts a true hit. Rewrite the rule before the
  next model bench (see `docs/plans/2026-09-08-embedding-model-swap-measurements.md`).
- [x] **`CLAUDE.md` says `record_id_to_string()` lives in `alexandria-mcp/src/server.rs`.** Fixed 2026-09-08 (4f2d961). It lives
  in `alexandria-storage/src/lib.rs` and `server.rs` only re-exports it. Fix the note.
- [x] **Test gaps, low priority.** Done 2026-09-08 (d4bc3eb): CLS-vs-mean unit test in `candle.rs`
  (loads the real MiniLM, slow like the other provider tests), per-text fake vectors in the `reembed`
  centroid test, a `Done { 0, 0 }` test, and an order-independent `all_ids_and_content` assertion.
- [-] **`alexandria-pipeline` unit tests now need the real model** (2026-09-08, d4bc3eb). The CLS-vs-mean
  test sits in `candle.rs` under `#[cfg(test)]` because it flips the private pooling flag, so
  `cargo test -p alexandria-pipeline --lib` downloads MiniLM on a cold cache where before only the
  `tests/` integration tests did. Accepted; if it bothers anyone, expose a test-only constructor and
  move the test to `tests/embedding_test.rs` with the other slow ones.
- [ ] **Server boot stamps the lock over an unlocked corpus.** Companion to the guard above (2026-09-08):
  `migrate-embeddings` refuses, but a normal start with facts present and no lock still writes the
  configured model as the lock without checking that the stored vectors came from it. Same
  pre-v003 population, so almost certainly nonexistent in the wild; add the same facts-without-lock
  check to the boot path if it ever matters.

## Build / toolchain

- [ ] **`.cargo/config.toml` and `rust-toolchain.toml` were dropped when this branch was integrated
  into main (2026-09-09).** Decision: linker/CPU flags are per-machine developer preference, not
  repo policy — the file only ever affected local x86-64 Linux gnu builds (CI's ubuntu jobs would
  have broken on the missing `mold`, and the Docker build already overrides `rustflags` via
  `RUSTFLAGS`, so neither `target-cpu` nor mold applied there). Dev boxes wanting it keep
  `-C target-cpu=native` + mold in `~/.cargo/config.toml`. Windows `target-cpu` verification is
  moot. Repo keeps `rust-version = "1.98"` + edition 2024 as the only compiler floor statement.
- [ ] **`[profile.release]` (lto = "thin", codegen-units = 1, strip) added 2026-09-08, never built.**
  Only `cargo build --workspace` (dev profile) has been run since the toolchain/profile changes,
  and the 2026-09-08 dependency major bumps (notably `hf-hub` 1.0 / `hf-xet`) were likewise only
  dev-built and tested; do a `cargo build --release` smoke test before shipping a release artifact.
- [ ] **Dockerfile base bumped to `rust:1.98.1-alpine3.22` and OpenSSL stripped, container build
  not yet exercised.** After the hf-hub 1.0 port, `cargo tree` shows no `openssl`/`openssl-sys`
  left (network is rustls + `aws-lc-sys`); the builder's `openssl-dev`/`pkgconfig` and the
  runtime's `libssl3`/`libcrypto3` were removed on 2026-09-09 based on that, not on a real
  `docker build`. `aws-lc-sys`'s build script normally needs only a C compiler on x86_64-musl,
  but if the container build starts failing, `cmake` is the first suspect to add to the builder
  apk line.

- [ ] **Startup memory doubled during model load (2026-09-08).** `candle.rs` now reads the safetensors
  file into a `Vec<u8>` (`from_buffered_safetensors`) instead of mmap, so the workspace can carry
  `unsafe_code = "forbid"`. For MiniLM (~90 MB) the buffer plus the built tensors coexist briefly at
  boot, then the buffer drops. Revisit only if a much larger model is adopted; the escape hatch is a
  `#[allow(unsafe_code)]` on that one call plus `from_mmaped_safetensors`.
- [x] **`ALEXANDRIA_EMBEDDING_DEVICE` env override has no test.** Done 2026-09-08 (fc582e0). Noticed 2026-09-08 while moving the
  config tests off process env; the other five overrides are covered by `test_env_overrides` /
  `test_server_env_overrides`, this one is not. One-line addition to `test_env_overrides`.

## Dependencies

- [x] **Direct-dep major bumps done 2026-09-08**, one commit each: `dirs` 7, `base64` 0.23, `toml` 1.1,
  `tokenizers` 0.23, `hf-hub` 1.0. `serial_test` was already gone from the workspace. Only `hf-hub`
  needed code: 1.0 is a rewrite (reqwest client, `blocking` feature runs its own runtime thread) and
  by default revalidates every cached file against the Hub on each boot, retrying on transient
  errors — a cached offline start went from 0.5 s to 12 s. `candle.rs` now resolves
  `local_files_only` first and only downloads on a miss, restoring 0.5 behaviour (~0.4 s cached start
  with the Hub blackholed). Net tree: openssl/native-tls/ureq gone, `hf-xet` (mandatory in 1.0) in;
  629 → 662 crates.
- [ ] **`hf-hub` 1.0 is heavy for what we use.** We call one thing (download four files into the
  standard HF cache) and pay for the whole client plus mandatory `hf-xet` (redb, sysinfo, statrs,
  xet-*): +33 crates net. `reqwest` 0.13 is already in the tree via rmcp/surrealdb, so a ~40-line
  downloader writing the same `models--owner--name/snapshots/<sha>/` layout would let us drop
  `hf-hub` entirely. Do it if build time or binary size becomes a complaint; until then the crate
  is one isolated commit (98295e5) to revert.
- [-] **Cache-first model loading never refreshes a cached revision** (2026-09-08, hf-hub 1.0 port).
  Same as 0.5 behaved: once `main` is on disk it is served forever; delete
  `~/.cache/huggingface/hub/models--<owner>--<name>` to re-fetch. Also, a model that lacks
  `1_Pooling/config.json` needs one online boot to write hf-hub's `.no_exist` marker; after that the
  cache-first path honours it without a network call. Accepted as-is.
- [ ] **Two `tokenizers` versions compile** until candle bumps: candle-core 0.11 still pins 0.22, we're
  on 0.23. Behaviourally harmless; revert ours to 0.22 if the duplicate build cost bothers anyone.
- [ ] **Transitive "Unchanged" `cargo update` entries are upstream pins, not ours.** `generic-array`
  0.14.7, `i_float`/`i_overlay`/`i_shape`, `matchit` 0.8.4, `pdqselect` 0.1.0 stay put even after
  the direct bumps above; they move when the pulling crate (surrealdb stack) does.

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
