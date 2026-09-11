# Embedding Model Swap — Design

## Purpose

`all-MiniLM-L6-v2` is a symmetric similarity model. A natural-language question against a stored
statement scores ~0.1–0.2 cosine while a keyword hit scores ~0.6, so the retrieve floor had to drop
to 0.10 and real matches sit next to noise. An asymmetric retrieval model separates them far better.
The model is locked on first boot (`system_config` rows `embedding_model` and
`embedding_dimensions`), so switching needs a re-embed path. The boot-time mismatch error and the
roadmap already promise `alexandria migrate-embeddings`.

Deliverable: the re-embed mechanism, plus a new shipped default model and retuned thresholds,
chosen by measuring candidates against the real corpus.

## Scope

In:

- Measurement script (throwaway) that scores candidate models on the live memories.
- `CandleProvider` reads the model's pooling mode from its repo so CLS-pooled models (bge) work.
- `alexandria migrate-embeddings` subcommand that rewrites every fact embedding and cluster
  centroid in place and updates the lock.
- New default model and threshold defaults, with docs updated.

Out:

- Query/passage prefixes (e5 family). The trait stays `embed(&[&str])`. Add only if a prefix model
  is adopted later.
- Non-BERT architectures, GPU devices, other providers.
- Crash-safe shadow columns or resumable migration. At the current corpus size (hundreds of facts)
  a rerun is the recovery.
- Automatic re-embed at boot.

## Current state (facts the design relies on)

- Vectors live in `fact.embedding` and `cluster.centroid`, both `array<float>`. There is no vector
  index in SurrealDB; ranking is cosine in Rust over all facts. A dimension change needs no schema
  migration.
- Queries and documents both go through `EmbeddingProvider::embed`.
- `CandleProvider` is BERT + mean pooling + L2 normalise, hardwired.
- `ClusterRepo::get_members` returns deleted members too; the maintenance loop uses it as-is for
  cohesion and merge decisions.
- Thresholds tuned to MiniLM's score distribution: cluster `join_threshold` 0.75,
  `merge_threshold` 0.9, `cohesion_floor` 0.6, retrieve `min_similarity` 0.10.
- Live install: ~270 facts (active + deleted), 123 clusters, HTTP mode under a user systemd unit,
  config pins the model explicitly.

## 1. Measurement (throwaway)

A binary under `crates/alexandria-pipeline/examples/`, deleted once the model is chosen.

- Fetches fact contents from the running server's `/debug/memories` page over HTTP (the service
  holds the SurrealKV lock, so the data dir cannot be opened directly).
- Loads each candidate through `CandleProvider`: `sentence-transformers/all-MiniLM-L6-v2`
  (baseline), `sentence-transformers/msmarco-MiniLM-L6-cos-v5`,
  `sentence-transformers/multi-qa-MiniLM-L6-cos-v1`, `BAAI/bge-small-en-v1.5`.
- Embeds every fact plus ~10 hand-written questions, each tagged with the id of its correct memory.
- Reports per model:
  - rank of the correct memory for each question;
  - mean gap between the hit score and the best non-hit score;
  - percentiles (p50, p90, p95, p99) of the pairwise fact-to-fact similarity distribution.

Decision rule: best mean rank, then largest gap. If no candidate beats MiniLM clearly on both,
stop after this step and report; ship the mechanism only, default unchanged.

Threshold rule for the winner:

- Cluster thresholds: find the percentile each current value sits at under MiniLM's fact-to-fact
  distribution, take the same percentile under the winner, round to two decimals.
- Retrieve floor: the median non-hit score (`nonhit_p50`), rounded to two decimals. The floor is a
  noise cutoff only, so it is derived from the non-hit distribution and never from the hits; the
  client auto-recall threshold does the discrimination. Sanity check: the value must be below the
  lowest correct-hit score. If it is not, the model has no usable noise floor; report that instead
  of deriving a threshold. (Rewritten 2026-09-08: the original rule, "between the lowest hit and
  the highest non-hit, midpoint on overlap", has no valid solution once any non-hit outscores the
  weakest hit, which every real corpus produces, and the midpoint cuts a true hit by construction.)

## 2. Provider pooling

`CandleProvider::load_model` additionally fetches `1_Pooling/config.json` from the model repo
(every sentence-transformers model ships it) and reads `pooling_mode_cls_token`. Missing file or
`false` means mean pooling, so MiniLM and the two drop-in candidates are unchanged. `embed_sync`
gains one branch: CLS pooling takes hidden state at position 0 instead of the masked mean. L2
normalisation stays.

Pooling is derived from the model id, so the existing two-row lock still fully identifies how the
vectors were produced. No config knob, no trait change.

## 3. `alexandria migrate-embeddings`

`main.rs` checks `std::env::args().nth(1)` for `migrate-embeddings` before loading anything else.
No argument parser dependency. Any other argument is an error; no argument starts the server as
today.

The logic lives in `alexandria_mcp::migrate::reembed(db, provider)` because mcp is the one crate
that may depend on both storage and pipeline, and a function there is testable with a fake
provider. The binary only wires config, database, and model loading around it.

Flow:

1. Load config, connect to the configured data dir, run `schema::migrate`.
2. Load the configured model via `CandleProvider`. (Loading before the lock check costs a few
   seconds in the no-op case and keeps the check in one place.)
3. `reembed` reads the lock. No lock: return `Skipped` with "fresh database, just start the
   server". Lock equal to `provider.model_id()`: return `Skipped` with "already on `<model>`".
   Otherwise continue. `main` prints the message and exits 0 on `Skipped`.
4. `MemoryRepo` gains two methods: `list_all_for_reembed() -> Vec<(id, content)>` over every fact
   including soft-deleted ones (lineage snapshots must stay comparable), and
   `set_embedding(id, &[f32])`. Loop over the list: embed, write, log progress every 50 facts.
5. For every cluster, fetch members with the existing `get_members` (deleted included, matching
   the maintenance loop), write the plain element-wise mean via the existing
   `ClusterRepo::update_centroid`. Clusters with zero members are skipped.
6. `system_config::set_config` for `embedding_model` and `embedding_dimensions`, in that order,
   last.
7. Print "done, restart the service".

Failure mid-way leaves the lock on the old model, so the server keeps refusing to boot with the
existing mismatch error. Rerunning the command re-embeds everything again and is the recovery.

`check_embedding_model`'s mismatch message drops the "(Future)" wording and names the command:
"Run `alexandria migrate-embeddings` with the new model in config to re-embed everything."

## 4. Default swap and thresholds

- `EmbeddingConfig::default().model` becomes the measured winner.
- `ClusterConfig` and `RetrieveConfig` defaults become the measured values from section 1.
- The doc comment on `min_similarity` in `config.rs` is rewritten for the new model.
- `docs/configuration.md`: example and defaults tables updated; a note next to `[embedding]`
  listing the previous MiniLM threshold values, for installs that keep MiniLM pinned but do not pin
  thresholds. Threshold defaults are global, so such an install would otherwise silently pick up
  values tuned for the new model.
- `docs/roadmap.md`: the migration CLI item is marked done.
- `TODO-misc.md`: the retrieval-quality entry gets a "Done 2026-09-08" note with the chosen model
  and the measured numbers.
- Upgrade path for an install with no `[embedding]` section: first boot after upgrade fails with
  the mismatch error, which now names the command. That is intended; no automatic re-embed.

## 5. Testing

- Pooling: unit test of the `1_Pooling/config.json` parse on a JSON string (CLS true, CLS false,
  missing key). One slow candle test that `BAAI/bge-small-en-v1.5` loads, reports 384 dimensions,
  and returns a unit-norm vector, alongside the existing model-download tests.
- Storage: in-memory tests for `list_all_for_reembed` (includes a soft-deleted fact) and
  `set_embedding`.
- Reembed: integration test in `alexandria-mcp` with a fake `EmbeddingProvider` returning
  fixed-size vectors. Seed an in-memory database with clustered facts at one dimension under lock
  model "a", run `reembed` with a fake provider "b" at another dimension, assert every fact and
  centroid has the new dimension and the lock rows read "b" and the new dimension. Second test:
  lock already equal to the provider's model is a no-op (the early exit lives in `reembed`, not
  only in `main`).
- Config: `test_defaults` updated to the new model and thresholds.
- Manual: run the command against a copy of the live data dir with the service stopped, then
  against the real one. Confirm the service boots and `retrieve_memories` returns sensible scores
  for the measurement questions.

## Files

- `crates/alexandria-pipeline/examples/model_bench.rs` (throwaway, deleted before merge)
- `crates/alexandria-pipeline/src/embedding/candle.rs`
- `crates/alexandria-storage/src/repos/memory_repo.rs`
- `crates/alexandria-storage/src/system_config.rs`
- `crates/alexandria-mcp/src/migrate.rs`, `crates/alexandria-mcp/src/lib.rs`
- `crates/alexandria-mcp/tests/reembed_test.rs`
- `crates/alexandria/src/main.rs`, `crates/alexandria/src/config.rs`
- `docs/configuration.md`, `docs/roadmap.md`, `TODO-misc.md`
