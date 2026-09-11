# Performance and Ability Findings

Audit of 2026-09-10 against commit `858dd82`. Scope: the request path (`crates/alexandria-mcp/src/server.rs`),
the storage repositories, the embedding provider (`crates/alexandria-pipeline`), the engine algorithms,
and the background maintenance task in `crates/alexandria/src/main.rs`. "Performance" findings are about work done per
request; "ability" findings are about retrieval quality and features the code claims but does not
deliver.

No latency benchmarks were run. Every performance claim below is structural (query counts, bytes moved,
tokens processed) and can be checked by reading the cited lines. The one measured item is the token
length of the live corpus, which drives finding A1.

| ID | Finding | Severity | Effort |
|---|---|---|---|
| [A1](#a1-every-embedding-is-truncated-to-128-tokens) | Every embedding is truncated to 128 tokens | High | small, plus a re-embed |
| [P1](#p1-every-embedding-pays-a-128-token-forward-pass) | Every embedding pays a 128-token forward pass | Medium | trivial |
| [P2](#p2-cluster-member-counting-is-on-per-store-and-per-broad-recall) | Cluster member counting is O(N) per store and per broad recall | Medium | small |
| [A2](#a2-the-heat-model-is-inert) | The heat model is inert | Medium, decision needed | small either way |
| [P3](#p3-inference-runs-inline-on-the-async-runtime-and-never-batches) | Inference runs inline on the async runtime and never batches | Medium | medium |
| [A3](#a3-no-near-duplicate-check-at-store-time) | No near-duplicate check at store time | Medium | small |
| [A4](#a4-no-lexical-search) | No lexical search | Medium | medium |
| [P4](#p4-spreading-activation-is-awaited-on-the-retrieve-path) | Spreading activation is awaited on the retrieve path | Low | trivial |
| [P5](#p5-maintenance-re-reads-every-cluster-after-every-merge) | Maintenance re-reads every cluster after every merge | Low | none yet |

## Measurements

Token length of every live fact, tokenized with the model's own wordpiece tokenizer with truncation
and padding disabled, counting `[CLS]` and `[SEP]`:

| Live facts | p50 | p90 | p99 | max | over 128 | over 256 | over 512 |
|---|---|---|---|---|---|---|---|
| 1122 | 73 | 122 | 284 | 443 | 100 (8.9%) | 19 (1.7%) | 0 |

The longest truncated facts were multi-sentence gotchas and dated corrections, which is the content
type memory exists for.

How it was measured, so it can be rerun after a fix:

1. `GET /debug/memories?limit=5000` and collect the `/debug/memories/<id>` links. The list page
   shows a truncated preview, so lengths must not be taken from it.
2. Fetch each detail page and take the text inside `<pre class="content-block">`, skipping pages that
   carry the deleted badge outside the `<style>` block.
3. Tokenize with the `tokenizers` Python package against
   `~/.cache/huggingface/hub/models--sentence-transformers--all-MiniLM-L6-v2/snapshots/*/tokenizer.json`
   after calling `no_truncation()` and `no_padding()`. `uv run --with tokenizers` avoids installing
   anything.

## Ability

### A1. Every embedding is truncated to 128 tokens

**Severity:** High. **Effort:** small code change, then a forced re-embed.

**Where.**

- `crates/alexandria-pipeline/src/embedding/candle.rs:104` loads `tokenizer.json` with
  `Tokenizer::from_file` and never changes its truncation or padding settings; `:126` calls
  `encode(text, true)`, which applies whatever the file specifies.
- The cached snapshot's `tokenizer.json` specifies `truncation.max_length = 128` and
  `padding.strategy = Fixed(128)`. The model's `config.json` has `max_position_embeddings = 512`;
  sentence-transformers runs this model at `max_seq_length = 256`.

**Impact.** 100 of 1122 live facts are searchable only by their first 128 tokens. The rest of each
fact is invisible to `retrieve_memories`, `recall`, cluster assignment, and the duplicate detection
that A3 would add. `import_document` is hit harder: the `fixed_size` strategy cuts 1000-character
chunks (`server.rs:340`), about 220 to 250 tokens, so nearly every fixed-size chunk is truncated;
`heading` chunks are unbounded; `whole` mode embeds only the opening of the document.

The client thresholds in `docs/minilm-test-data.md` (`limit = 10`, `min_similarity = 0.45`) were
measured under this truncation. They may move once long facts embed on their full text.

**Fix.**

1. After loading, set truncation to 256 (`TruncationParams { max_length: 256, .. }`) and padding to
   none for single inputs (see P1). Going to 512 is possible but the model was trained at 128 and is
   served upstream at 256; 256 covers the p99 of the live corpus.
2. Detect truncation per call: after `encode`, `get_overflowing()` is non-empty when truncation
   happened. Count it in a `tracing::warn!` and return a `truncated: true` flag from `store_memory`
   so agents can split long content.
3. Make the chunkers token-aware or cap them so that `fixed_size` stays under the limit
   (roughly 900 characters for 256 tokens).
4. **Re-embed.** `alexandria migrate-embeddings` skips when the stored model id equals the configured
   one (`crates/alexandria-mcp/src/migrate.rs:47-49`), so the fix needs either a `--force` flag or,
   better, a second lock value such as `embedding_max_tokens` in `system_config` that
   `check_embedding_model` and `reembed` treat like a model change. The lock approach also stops a
   future truncation edit from silently mixing vector spaces.
5. Rerun `alexandria bench-retrieval` afterwards and re-read the threshold grid.

### A2. The heat model is inert

**Severity:** Medium. Needs a decision. **Effort:** small either way.

**Where.**

- `crates/alexandria-engine/src/heat/decay.rs:31` (`projected_heat`) and `:46` (`on_access`) have
  no callers outside the engine's own tests.
- `server.rs` never calls `HeatRepo::update` (`crates/alexandria-storage/src/repos/heat_repo.rs:49`).
  The only writes are the initial row (`server.rs:219`, `:387`) and `add_heat` from spreading
  activation (`server.rs:696-702`), which warms *neighbours* of a retrieved fact but not the fact.
- Ranking ignores heat: `do_retrieve_memories` ranks by cosine only (`server.rs:451`);
  `broad_recall` sorts by similarity (`crates/alexandria-engine/src/recall/algorithm.rs:126`) even
  though its doc comment at `:57` says "rank by best_member_sim × cluster_heat"; the members it is
  given have `heat` hardcoded to `1.0` (`server.rs:734`).

**Impact.** `access_count` and `stability` never change. `heat` only ever rises via activation and
is never read, so the Ebbinghaus model described in the README and roadmap (v0.1, "Ebbinghaus heat
model with decay and stability") does nothing for retrieval. The debug UI displays the numbers, which
is misleading. The `heat` and `activation` config sections tune a no-op.

**Options.**

- **Wire it.** In `do_retrieve_memories`, for the top `activation_top_n` results read the heat row,
  call `on_access`, write it back (`HeatRepo::update`), and fold `projected_heat` into ranking as a
  mild tiebreak (for example `similarity * (1 + a * heat)` with a small `a`). Do the same in
  `broad_recall` by loading real heat instead of `1.0`. Measure with `bench-retrieval` before and
  after; if `mean_rank` and `top1` do not improve, do not ship it.
- **Delete it.** Remove `heat_state`, the activation path, the `[heat]` and `[activation]` config
  sections, and the debug fields. Less code, no misleading numbers, and nothing observable changes
  because nothing reads heat today.

Recommendation: delete unless the measured gain from wiring is real. Half-alive is the worst state.

### A3. No near-duplicate check at store time

**Severity:** Medium. **Effort:** small.

**Where.** The `store_memory` tool description promises "dedup happens via clustering"
(`server.rs:86`). Clustering groups facts; it never rejects or merges one. `TODO-misc.md`
("A restated target scores as a miss") already records that duplicates outrank the original in the
bench.

**Impact.** Auto-store paths (heuristic detectors, extraction, the Claude hook) restate facts across
sessions. Each copy takes a result slot, so `limit = 10` delivers fewer distinct facts, and
`update_memory` cannot find the canonical record to correct.

**Fix.** With the HNSW index now defined at boot, one `MemoryRepo::nearest(embedding, 1)` before
`create_fact` costs one indexed query. On a hit at or above a high bar, return
`{"status": "duplicate", "id": <existing>}` or link the new text to the existing fact with
`derived_from`. The bar must be high: `TODO-misc.md` notes that MiniLM scores true duplicates and
adjacent distinct memories in the same 0.63 to 0.76 band, so only near-verbatim restatements (0.95
and up) are safe to collapse automatically. Measure the threshold on the live corpus before choosing.

### A4. No lexical search

**Severity:** Medium. **Effort:** medium.

**Where.** `do_retrieve_memories` is KNN only (`server.rs:432-454`). The `docs/roadmap.md` v0.4 list
already names "Full-text search index for keyword matching alongside semantic search".

**Impact.** A symmetric sentence model is weak on identifiers, flag names, error strings, and config
keys, and several `bench-retrieval` questions are exactly that shape ("what is the config key for
pango markup on a text block"). Those are the cases where an exact token match should win outright.

**Fix.** SurrealDB, already the dependency, ships BM25 full-text search: `DEFINE ANALYZER` plus
`DEFINE INDEX ... SEARCH ANALYZER ... BM25` in a `v007` migration, a `content @@ $q` query with
`search::score()` in `MemoryRepo`, and reciprocal-rank fusion of the two ranked lists in
`do_retrieve_memories`. `bench-retrieval` measures the effect directly; ship only if `top1` or
`mean_rank` move.

## Performance

### P1. Every embedding pays a 128-token forward pass

**Severity:** Medium. **Effort:** trivial. Same root cause as A1.

**Where.** `padding.strategy = Fixed(128)` in the loaded `tokenizer.json`; the attention mask keeps
mean pooling correct (`candle.rs:148-158`), so the result is right, but the compute is not.

**Impact.** A typical query is 10 to 25 tokens and a typical fact is 73 (p50). BERT attention cost
grows with sequence length, so single-text embeds do several times the work they need. This sits on
every `retrieve_memories`, which the auto-recall hooks call on every user prompt under a 5-second
client timeout.

**Fix.** `tokenizer.with_padding(None)` for the single-input path. For batched calls (P3) use
`PaddingStrategy::BatchLongest`.

### P2. Cluster member counting is O(N) per store and per broad recall

**Severity:** Medium. **Effort:** small.

**Where.**

- `server.rs:707-717` `load_cluster_infos` calls `ClusterRepo::list_with_counts`
  (`crates/alexandria-storage/src/repos/cluster_repo.rs:266`), which calls `get_members` (`:52`) once
  per cluster and takes `.len()`. `get_members` selects full `Fact` rows, embeddings included.
- Callers: `assign_to_cluster_and_update` on every `store_memory` and on every `import_document`
  chunk (`server.rs:655`), and `load_all_clusters_with_members` (`server.rs:749-764`) on every broad
  `recall` (`:520`), which then keeps every embedding in memory to score clusters.

**Impact.** Each store moves every live embedding out of the database to count them. At 384 floats
per fact that is about 1.5 KB per fact, roughly 1.7 MB per store on today's corpus, and it grows
linearly. Broad recall does the same and then does an in-process cosine over the whole corpus,
bypassing the HNSW index that `retrieve_memories` already uses. `TODO-misc.md` records the
per-cluster query but not that the rows carry embeddings.

**Fix.**

1. Counting: one query, the pattern `SessionRepo::list` already uses at
   `crates/alexandria-storage/src/repos/session_repo.rs:163`:
   `SELECT *, count(->contains_memory->fact) AS member_count FROM cluster`. Replace
   `list_with_counts` with it.
2. Broad recall: take the top-k facts from `MemoryRepo::nearest`, group them by cluster via
   `cluster_for_fact` or a single graph query, and rank clusters from that. That makes recall O(k)
   instead of O(N) and keeps it on the index. If the full-member design is kept for now, at least
   project only `id, content` in `get_members` when embeddings are not needed.

### P3. Inference runs inline on the async runtime and never batches

**Severity:** Medium. **Effort:** medium.

**Where.**

- `candle.rs:184`: "Run inline for now", because `&self` cannot move into `spawn_blocking`.
- `candle.rs:120-123`: `embed_sync` loops over texts one at a time, building tensors and running a
  `(1, L)` forward per text. The `batch_size` config for `migrate-embeddings` bounds memory but does
  not batch inference.

**Impact.** A BERT forward on a tokio worker thread blocks that worker for its duration. With the
multi-thread runtime other workers keep serving, but a large `import_document` monopolises one for
the whole loop and any request that lands on it waits. `migrate-embeddings` runs the whole corpus
one text at a time.

**Fix.** Hold `BertModel` and `Tokenizer` in an `Arc` inside the provider, clone the handles into
`tokio::task::spawn_blocking`, and for slices of more than one text use `encode_batch` with
longest-padding and a single `(B, L)` forward. Keep single-text calls unpadded (P1). Measure
`migrate-embeddings` wall time before and after; that is the one path where batching should show a
multiple.

### P4. Spreading activation is awaited on the retrieve path

**Severity:** Low. **Effort:** trivial.

**Where.** `server.rs:461-462`: the comment says "Fire-and-forget activation, don't block on it" and
the next line awaits it. `trigger_activation` (`:675`) runs `EdgeRepo::get_neighbors`
(`crates/alexandria-storage/src/repos/edge_repo.rs:109`), a BFS that issues two queries per visited
node (`:64`), then one `UPDATE` per target.

**Impact.** At least two queries per top result on every retrieve, more with edges. Cheap today
because the only edges are `derived_from` and `extracted_from`, so most facts have none. It becomes
real once relation discovery (roadmap v0.3) creates edges, and it is on the auto-recall latency
budget. Note that if A2 is resolved by deletion, this finding goes with it.

**Fix.** `tokio::spawn` the activation with cloned `Arc`s, or skip it entirely when the fact has no
edges (the early return at `:681` already does that after the first query).

### P5. Maintenance re-reads every cluster after every merge

**Severity:** Low. **Effort:** none yet.

**Where.** `crates/alexandria/src/main.rs:228-293`: after each executed merge the loop re-queries all clusters and
re-counts members (through `get_members`, so with embeddings, see P2) before scanning for the next
pair. Merges per tick times clusters times facts.

**Impact.** Runs every 5 minutes off the request path; at the current scale it is invisible. Worth a
note only because P2's fix (count in one query) removes most of the cost for free, and because the
merge scan is O(C²) cosine comparisons which is fine until cluster counts reach the thousands.

**Fix.** None now. Reuse the P2 count query here when it lands.

## Recommended order

1. A1 and P1 together: two lines in `candle.rs`, the truncation flag, the lock or `--force` for
   re-embed, then re-embed and rerun the bench. Largest measured defect and a speed win in the same
   change.
2. P2 step 1: the one-query cluster count. Removes an O(N) transfer from every write.
3. A2: decide wire-or-delete. If delete, P4 disappears too.
4. A3: duplicate check on store, threshold measured first.
5. P3: move inference off the runtime and batch it.
6. A4 and P2 step 2: lexical search and index-backed recall, both measured with `bench-retrieval`.
