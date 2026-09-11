# Embedding Model Swap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the embedding model switchable on an existing database via `alexandria migrate-embeddings`, and ship a better default retrieval model with thresholds retuned from measurements on the live corpus.

**Architecture:** `CandleProvider` learns its pooling mode from the model repo (CLS or mean). A new `alexandria_mcp::migrate::reembed` rewrites every `fact.embedding` and `cluster.centroid` in place and updates the `system_config` lock last; the binary wires it behind a bare `migrate-embeddings` argument. A throwaway bench example picks the winning model and threshold values, which are then written into config defaults and docs.

**Tech Stack:** Rust 1.88 workspace, SurrealDB 3.2 (embedded, `kv-mem` in tests), candle 0.11 + hf-hub 0.5 for BERT inference, tokio.

**Spec:** `docs/plans/2026-09-08-embedding-model-swap-design.md`

## Global Constraints

- All DB access stays in `alexandria-storage` (no raw SurrealQL outside it; the throwaway bench example uses `MemoryRepo` only).
- `alexandria-mcp` is the only crate that may depend on both storage and pipeline; `reembed` lives there.
- No new dependencies. No argument-parser crate; `std::env::args` only.
- No schema migration is needed (vectors are `array<float>`, no vector index). Do not add one; `tests/migration_test.rs` stays at version 6.
- `EmbeddingProvider` trait is unchanged: `embed(&[&str])`, `dimensions()`, `model_id()`.
- SurrealDB 3.2 gotchas: `DELETE table WHERE` (no `FROM`), `type::record()` not `type::thing()`, query result structs derive `SurrealValue`, format ids with `record_id_to_string`.
- Tests use `Database::connect_embedded()` and `schema::migrate(db.inner())`.
- Commit after each task. Commit messages end with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.
- The live server (`systemctl --user status alexandria`) holds the SurrealKV lock on `~/.local/share/alexandria/data`. Never open that directory while the service runs. Task 7 touches it only with explicit user go-ahead.

---

### Task 1: Pooling mode from the model repo

**Files:**
- Modify: `crates/alexandria-pipeline/src/embedding/candle.rs`
- Test (unit, in the same file): pooling-config parse
- Test (slow, downloads model): `crates/alexandria-pipeline/tests/embedding_test.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `CandleProvider` loads any BERT sentence-transformers model that uses CLS or mean pooling. Internal `fn cls_pooling_from_json(s: &str) -> bool` (private, unit-tested). `CandleProvider` gains a private field `cls_pooling: bool`.

- [ ] **Step 1: Write the failing unit tests for the parse helper**

Append to the bottom of `crates/alexandria-pipeline/src/embedding/candle.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::cls_pooling_from_json;

    #[test]
    fn cls_true_when_flag_set() {
        let json = r#"{"word_embedding_dimension": 384, "pooling_mode_cls_token": true, "pooling_mode_mean_tokens": false}"#;
        assert!(cls_pooling_from_json(json));
    }

    #[test]
    fn mean_when_flag_false() {
        let json = r#"{"word_embedding_dimension": 384, "pooling_mode_cls_token": false, "pooling_mode_mean_tokens": true}"#;
        assert!(!cls_pooling_from_json(json));
    }

    #[test]
    fn mean_when_key_missing_or_unparseable() {
        assert!(!cls_pooling_from_json(r#"{"pooling_mode_mean_tokens": true}"#));
        assert!(!cls_pooling_from_json("not json"));
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p alexandria-pipeline --lib cls_ 2>&1 | tail -20`
Expected: compile error, `cls_pooling_from_json` not found.

- [ ] **Step 3: Implement the parse helper, the repo fetch, and the CLS branch**

In `candle.rs`:

Add the helper (free function, above `impl CandleProvider`):

```rust
/// sentence-transformers models ship `1_Pooling/config.json`. Only the CLS flag
/// matters to us; anything else (missing file, missing key, bad JSON) means mean pooling.
fn cls_pooling_from_json(s: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| v.get("pooling_mode_cls_token")?.as_bool())
        .unwrap_or(false)
}
```

Add the field to the struct:

```rust
pub struct CandleProvider {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    model_id: String,
    dimensions: usize,
    cls_pooling: bool,
}
```

Change `load_model` to return the flag. Its signature becomes
`fn load_model(model_id: &str, device_str: &str) -> Result<(BertModel, Tokenizer, Device, usize, bool)>`.
After the `weights_path` fetch add:

```rust
        // Optional: pooling config. Not every repo has it; absence means mean pooling.
        let cls_pooling = repo
            .get("1_Pooling/config.json")
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| cls_pooling_from_json(&s))
            .unwrap_or(false);
```

and return `Ok((model, tokenizer, device, dimensions, cls_pooling))`. Update `new` to destructure five values and set `cls_pooling`.

In `embed_sync`, replace the block from `// Mean pooling over sequence length` through `let mean_pooled = (summed / counts)?;` with:

```rust
            let pooled = if self.cls_pooling {
                // CLS pooling: hidden state of the first token. Shape (1, hidden).
                output.narrow(1, 0, 1)?.squeeze(1)?
            } else {
                // Mean pooling over sequence length (dim 1), respecting attention mask
                let mask = attention_mask
                    .unsqueeze(2)?
                    .to_dtype(candle_core::DType::F32)?
                    .broadcast_as(output.shape())?;
                let masked = (output * mask)?;
                let summed = masked.sum(1)?;
                let counts = attention_mask
                    .to_dtype(candle_core::DType::F32)?
                    .sum(1)?
                    .unsqueeze(1)?
                    .broadcast_as(summed.shape())?;
                (summed / counts)?
            };
```

and rename the following `mean_pooled` uses to `pooled` (the L2 normalise block and `normalized`).

Add a log line in `new` after loading: `tracing::info!("Pooling: {}", if cls_pooling { "cls" } else { "mean" });`

- [ ] **Step 4: Run unit tests**

Run: `cargo test -p alexandria-pipeline --lib cls_ 2>&1 | tail -20`
Expected: 3 passed.

- [ ] **Step 5: Add the slow bge integration test**

Append to `crates/alexandria-pipeline/tests/embedding_test.rs`:

```rust
#[tokio::test]
async fn test_candle_cls_pooled_model_loads_and_normalises() {
    // BAAI/bge-small-en-v1.5 ships 1_Pooling/config.json with pooling_mode_cls_token = true.
    let provider = CandleProvider::new("BAAI/bge-small-en-v1.5", "cpu")
        .await
        .unwrap();
    assert_eq!(provider.dimensions(), 384);

    let vectors = provider.embed(&["hello world"]).await.unwrap();
    assert_eq!(vectors[0].len(), 384);
    let norm: f32 = vectors[0].iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "expected unit norm, got {norm}");
}
```

- [ ] **Step 6: Run the full pipeline tests (downloads ~130 MB for bge on first run)**

Run: `cargo test -p alexandria-pipeline 2>&1 | tail -20`
Expected: all pass, including the two existing MiniLM tests (mean path unchanged).

- [ ] **Step 7: Commit**

```bash
git add crates/alexandria-pipeline
git commit -m "feat(pipeline): read pooling mode from the model repo, support CLS pooling

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: Measurement bench (throwaway) and model decision

**Files:**
- Create: `crates/alexandria-pipeline/examples/model_bench.rs` (deleted in Task 7)
- Create: `/tmp/alexandria-bench/questions.json` (not committed)
- Create: `docs/plans/2026-09-08-embedding-model-swap-measurements.md` (committed; Task 6 reads it)

**Interfaces:**
- Consumes: Task 1 (`CandleProvider` handles bge). `MemoryRepo::list(search, tag, include_deleted, limit, offset) -> Vec<Fact>`, `alexandria_engine::search::cosine_similarity(&[f32], &[f32]) -> f32`, `alexandria_storage::record_id_to_string`.
- Produces: the measurements file with a fixed table (format below) naming the chosen model and the four threshold values, plus the recommended client-side auto-recall threshold.

- [ ] **Step 1: Snapshot the live data dir (service keeps running)**

```bash
mkdir -p /tmp/alexandria-bench
rm -rf /tmp/alexandria-bench/data
cp -a ~/.local/share/alexandria/data /tmp/alexandria-bench/data
```

The copy is read-only for us. If opening it fails in Step 4 with a SurrealKV consistency error, repeat the copy and retry once; if it still fails, fall back to scraping `http://127.0.0.1:3000/debug/memories/{id}` pages for content (note this in the measurements file).

- [ ] **Step 2: Write the bench example**

`crates/alexandria-pipeline/examples/model_bench.rs`:

```rust
//! THROWAWAY. Scores candidate embedding models against a copy of the live corpus.
//! Usage: cargo run -p alexandria-pipeline --release --example model_bench -- <data_dir_copy> <questions.json>
//! questions.json: [{"q": "natural language question", "id": "fact:xxxx"}, ...]
//! Delete this file once the model is chosen.

use std::path::Path;

use alexandria_engine::search::cosine_similarity;
use alexandria_pipeline::embedding::{CandleProvider, EmbeddingProvider};
use alexandria_storage::repos::MemoryRepo;
use alexandria_storage::{record_id_to_string, Database};

const MODELS: &[&str] = &[
    "sentence-transformers/all-MiniLM-L6-v2",
    "sentence-transformers/msmarco-MiniLM-L6-cos-v5",
    "sentence-transformers/multi-qa-MiniLM-L6-cos-v1",
    "BAAI/bge-small-en-v1.5",
];

// Current defaults, tuned on MiniLM. We report where they sit in the baseline's
// fact-to-fact distribution and the same-percentile value for each candidate.
const CURRENT: &[(&str, f32)] = &[
    ("cluster.join_threshold", 0.75),
    ("cluster.merge_threshold", 0.90),
    ("cluster.cohesion_floor", 0.60),
];

fn percentile(sorted: &[f32], p: f32) -> f32 {
    let idx = ((sorted.len() - 1) as f32 * p).round() as usize;
    sorted[idx]
}

fn percentile_of(sorted: &[f32], value: f32) -> f32 {
    let below = sorted.iter().filter(|&&s| s < value).count();
    below as f32 / sorted.len() as f32
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = Path::new(&args[1]);
    let questions: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&args[2])?)?;

    let db = Database::connect(data_dir).await?;
    let facts = MemoryRepo::new(db.inner())
        .list(None, None, false, 100_000, 0)
        .await?;
    let ids: Vec<String> = facts
        .iter()
        .map(|f| f.id.as_ref().map(record_id_to_string).unwrap_or_default())
        .collect();
    let texts: Vec<&str> = facts.iter().map(|f| f.content.as_str()).collect();
    println!("{} active facts, {} questions", facts.len(), questions.len());

    // Baseline distribution, filled on the first model.
    let mut baseline_pairwise: Vec<f32> = Vec::new();

    for model in MODELS {
        println!("\n=== {model} ===");
        let provider = match CandleProvider::new(model, "cpu").await {
            Ok(p) => p,
            Err(e) => {
                println!("SKIPPED (load failed): {e:#}");
                continue;
            }
        };
        let doc_vecs = provider.embed(&texts).await?;

        // Pairwise fact-to-fact distribution.
        let mut pairwise = Vec::new();
        for i in 0..doc_vecs.len() {
            for j in (i + 1)..doc_vecs.len() {
                pairwise.push(cosine_similarity(&doc_vecs[i], &doc_vecs[j]));
            }
        }
        pairwise.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "fact-fact p50={:.3} p90={:.3} p95={:.3} p99={:.3} max={:.3}",
            percentile(&pairwise, 0.50),
            percentile(&pairwise, 0.90),
            percentile(&pairwise, 0.95),
            percentile(&pairwise, 0.99),
            pairwise.last().unwrap()
        );
        if baseline_pairwise.is_empty() {
            baseline_pairwise = pairwise.clone();
        }
        for (name, value) in CURRENT {
            let p = percentile_of(&baseline_pairwise, *value);
            println!(
                "  {name}: baseline {value:.2} sits at p{:.1}; same percentile here = {:.3}",
                p * 100.0,
                percentile(&pairwise, p)
            );
        }

        // Question set.
        let mut ranks = Vec::new();
        let mut gaps = Vec::new();
        let mut hit_scores = Vec::new();
        let mut nonhit_scores = Vec::new();
        for q in &questions {
            let text = q["q"].as_str().unwrap();
            let target = q["id"].as_str().unwrap();
            let qv = &provider.embed(&[text]).await?[0];
            let mut scored: Vec<(usize, f32)> = doc_vecs
                .iter()
                .enumerate()
                .map(|(i, d)| (i, cosine_similarity(qv, d)))
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            let rank = scored.iter().position(|(i, _)| ids[*i] == target);
            let Some(rank) = rank else {
                println!("  question target {target} not found in corpus, skipping");
                continue;
            };
            let hit = scored[rank].1;
            let best_other = scored
                .iter()
                .find(|(i, _)| ids[*i] != target)
                .map(|(_, s)| *s)
                .unwrap_or(0.0);
            ranks.push(rank + 1);
            gaps.push(hit - best_other);
            hit_scores.push(hit);
            nonhit_scores.extend(scored.iter().filter(|(i, _)| ids[*i] != target).map(|(_, s)| *s));
            println!(
                "  rank {:>3}  hit {:.3}  best-other {:.3}  gap {:+.3}  | {}",
                rank + 1,
                hit,
                best_other,
                hit - best_other,
                text
            );
        }
        nonhit_scores.sort_by(|a, b| a.partial_cmp(b).unwrap());
        hit_scores.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean_rank = ranks.iter().sum::<usize>() as f32 / ranks.len() as f32;
        let mean_gap = gaps.iter().sum::<f32>() / gaps.len() as f32;
        println!(
            "SUMMARY mean_rank={mean_rank:.2} top1={}/{} mean_gap={mean_gap:+.3} \
             hit_min={:.3} hit_max={:.3} nonhit_p50={:.3} nonhit_p90={:.3} nonhit_p99={:.3}",
            ranks.iter().filter(|&&r| r == 1).count(),
            ranks.len(),
            hit_scores[0],
            hit_scores[hit_scores.len() - 1],
            percentile(&nonhit_scores, 0.50),
            percentile(&nonhit_scores, 0.90),
            percentile(&nonhit_scores, 0.99),
        );
    }
    Ok(())
}
```

- [ ] **Step 3: Write the question set**

Dump the corpus to pick targets:

```bash
cargo run -p alexandria-pipeline --release --example model_bench -- /tmp/alexandria-bench/data /dev/null 2>&1 | head -5
```

(That only prints the fact count; to see contents open `http://127.0.0.1:3000/debug/memories?limit=200` in a browser or `curl -s 'http://127.0.0.1:3000/debug/memories?limit=200' | sed 's/<[^>]*>/ /g' | grep -v '^\s*$'`.)

Write `/tmp/alexandria-bench/questions.json` with 10 to 12 entries. Rules for a useful set:
- Phrase each as a question a coding agent would actually ask at task start ("which database does this project use", "how are hooks installed", "what did we decide about X").
- Cover different topics across the corpus (not ten questions about one memory).
- At least four questions must share little or no vocabulary with the target memory's wording (these are the cases MiniLM fails on).
- The `id` is the full `fact:xxxx` id shown in the debug UI.

Example shape:

```json
[
  {"q": "which database does alexandria store memories in", "id": "fact:abc123"},
  {"q": "how do I make sure the hook scripts do not go stale after a git pull", "id": "fact:def456"}
]
```

- [ ] **Step 4: Run the bench**

```bash
cargo run -p alexandria-pipeline --release --example model_bench -- \
  /tmp/alexandria-bench/data /tmp/alexandria-bench/questions.json 2>&1 | tee /tmp/alexandria-bench/results.txt
```

Expected: four `===` sections (or a `SKIPPED` line for a model whose repo lacks `model.safetensors`; record that and move on). First run downloads models (~80 to 130 MB each).

- [ ] **Step 5: Decide, and write the measurements file**

Decision rule (from the spec): lowest `mean_rank` wins; tie broken by largest `mean_gap`. If no candidate beats MiniLM on both, the chosen model is `sentence-transformers/all-MiniLM-L6-v2` and Task 6 only edits docs.

Threshold rules:
- `join_threshold`, `merge_threshold`, `cohesion_floor`: the "same percentile here" value printed for the chosen model, rounded to two decimals.
- `retrieve.min_similarity`: `nonhit_p50` rounded to two decimals. It must be below `hit_min`; if it is not, the model has no usable noise floor, so record that in the measurements file and do not derive a value. (Rewritten 2026-09-08; the earlier hit-anchored rule had no solution when `nonhit_p99 > hit_min`.)
- Client auto-recall threshold (hooks' `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY`): `nonhit_p90` rounded up to two decimals, but not above `hit_min`; if `hit_min` is lower, use the midpoint of `hit_min` and `nonhit_p90`.

Write `docs/plans/2026-09-08-embedding-model-swap-measurements.md` in exactly this shape (values filled in from `results.txt`):

```markdown
# Embedding model measurements (2026-09-08)

Corpus: N active facts from the live database. Questions: M, listed below.

| model | mean_rank | top1 | mean_gap | hit_min | hit_max | nonhit_p50 | nonhit_p90 | nonhit_p99 | ff_p50 | ff_p90 | ff_p99 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| sentence-transformers/all-MiniLM-L6-v2 | ... |
| sentence-transformers/msmarco-MiniLM-L6-cos-v5 | ... |
| sentence-transformers/multi-qa-MiniLM-L6-cos-v1 | ... |
| BAAI/bge-small-en-v1.5 | ... |

## Chosen

| key | value |
|---|---|
| model | <model id> |
| dimensions | 384 |
| cluster.join_threshold | 0.xx |
| cluster.merge_threshold | 0.xx |
| cluster.cohesion_floor | 0.xx |
| retrieve.min_similarity | 0.xx |
| client auto-recall threshold | 0.xx |

Reasoning: two or three sentences on why this model won and anything surprising.

## Questions

1. "<q>" -> `<id>`
...
```

- [ ] **Step 6: Commit the bench and measurements**

```bash
git add crates/alexandria-pipeline/examples/model_bench.rs docs/plans/2026-09-08-embedding-model-swap-measurements.md
git commit -m "chore(pipeline): throwaway model bench and measurements for the embedding swap

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: Storage: list every fact for re-embedding

**Files:**
- Modify: `crates/alexandria-storage/src/repos/memory_repo.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `MemoryRepo::all_ids_and_content(&self) -> Result<Vec<(String, String)>>` returning `(fact id as "fact:xxx", content)` for every fact including soft-deleted ones, ordered by `created_at`. Embedding writes reuse the existing `MemoryRepo::update_fact(id, None, None, None, Some(&emb))`; no new setter (the spec's `set_embedding` is already covered by `update_fact`).

- [ ] **Step 1: Write the failing test**

Add inside the existing `mod tests` in `memory_repo.rs`:

```rust
    #[tokio::test]
    async fn test_all_ids_and_content_includes_deleted() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());

        let a = repo.create_fact("first", 0.5, &[0.1, 0.2], &[]).await.unwrap();
        let b = repo.create_fact("second", 0.5, &[0.3, 0.4], &[]).await.unwrap();
        repo.soft_delete_fact(&b).await.unwrap();

        let rows = repo.all_ids_and_content().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], (a.clone(), "first".to_string()));
        assert_eq!(rows[1], (b.clone(), "second".to_string()));

        // update_fact with only an embedding is the write path reembed uses
        let updated = repo
            .update_fact(&a, None, None, None, Some(&[9.0, 8.0, 7.0]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.embedding, vec![9.0, 8.0, 7.0]);
        assert_eq!(updated.content, "first");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p alexandria-storage --lib all_ids_and_content 2>&1 | tail -20`
Expected: compile error, no method `all_ids_and_content`.

- [ ] **Step 3: Implement**

In `memory_repo.rs`, add the import `use crate::record_id_to_string;` and `use surrealdb::types::RecordId;` (extend the existing `surrealdb::types` import line), then add to `impl MemoryRepo`:

```rust
    /// Every fact, deleted ones included, as (id, content). Used by the embedding
    /// migration, which must re-embed lineage snapshots too so they stay comparable.
    pub async fn all_ids_and_content(&self) -> Result<Vec<(String, String)>> {
        #[derive(serde::Deserialize, SurrealValue)]
        struct Row {
            id: RecordId,
            content: String,
            // Projected only so ORDER BY has it; not returned.
            #[allow(dead_code)]
            created_at: Option<chrono::DateTime<chrono::Utc>>,
        }
        let mut response = self
            .db
            .query("SELECT id, content, created_at FROM fact ORDER BY created_at")
            .await?;
        let rows: Vec<Row> = response.take(0)?;
        Ok(rows
            .into_iter()
            .map(|r| (record_id_to_string(&r.id), r.content))
            .collect())
    }
```

If `create_fact`'s returned id (`id.to_sql()`) differs in format from `record_id_to_string` (for example angle brackets around the key), the test's equality on `a` will fail; in that case convert with `record_id_to_string` consistently and keep the test.

- [ ] **Step 4: Run tests**

Run: `cargo test -p alexandria-storage --lib memory_repo 2>&1 | tail -20`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/alexandria-storage/src/repos/memory_repo.rs
git commit -m "feat(storage): MemoryRepo::all_ids_and_content for re-embedding

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 4: `alexandria_mcp::migrate::reembed`

**Files:**
- Create: `crates/alexandria-mcp/src/migrate.rs`
- Modify: `crates/alexandria-mcp/src/lib.rs`
- Create: `crates/alexandria-mcp/tests/reembed_test.rs`

**Interfaces:**
- Consumes: Task 3 `MemoryRepo::all_ids_and_content`, `MemoryRepo::update_fact`; existing `ClusterRepo::list_with_counts() -> Vec<(Cluster, usize)>`, `ClusterRepo::get_members(id) -> Vec<Fact>`, `ClusterRepo::update_centroid(id, &[f32])`; `alexandria_storage::system_config::{get_config, set_config}`; `EmbeddingProvider`.
- Produces:

```rust
pub enum ReembedOutcome {
    /// Nothing to do; the string is a human-readable reason.
    Skipped(String),
    Done { facts: usize, clusters: usize },
}
pub async fn reembed(db: &Database, provider: &dyn EmbeddingProvider) -> anyhow::Result<ReembedOutcome>;
```

- [ ] **Step 1: Write the failing integration tests**

`crates/alexandria-mcp/tests/reembed_test.rs`:

```rust
use alexandria_mcp::migrate::{reembed, ReembedOutcome};
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::repos::{ClusterRepo, MemoryRepo};
use alexandria_storage::{system_config, Database};

/// Fake model "b": every text embeds to the same unit vector in 3 dims.
struct ModelB;

#[async_trait::async_trait]
impl EmbeddingProvider for ModelB {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
    }
    fn dimensions(&self) -> usize {
        3
    }
    fn model_id(&self) -> &str {
        "b"
    }
}

/// Seed: two live facts and one deleted fact, all 2-dim under lock model "a";
/// one cluster holding both live facts with a 2-dim centroid.
async fn seed() -> (Database, String, String, String, String) {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner()).await.unwrap();
    let memories = MemoryRepo::new(db.inner());
    let live1 = memories.create_fact("one", 0.5, &[0.6, 0.8], &[]).await.unwrap();
    let live2 = memories.create_fact("two", 0.5, &[0.8, 0.6], &[]).await.unwrap();
    let gone = memories.create_fact("three", 0.5, &[0.0, 1.0], &[]).await.unwrap();
    memories.soft_delete_fact(&gone).await.unwrap();

    let clusters = ClusterRepo::new(db.inner());
    let cid = clusters.create(None, &[0.7, 0.7]).await.unwrap();
    clusters.add_member(&cid, &live1).await.unwrap();
    clusters.add_member(&cid, &live2).await.unwrap();

    system_config::set_config(db.inner(), "embedding_model", "a").await.unwrap();
    system_config::set_config(db.inner(), "embedding_dimensions", "2").await.unwrap();
    (db, live1, live2, gone, cid)
}

#[tokio::test]
async fn reembed_rewrites_facts_centroids_and_lock() {
    let (db, live1, live2, gone, cid) = seed().await;

    let outcome = reembed(&db, &ModelB).await.unwrap();
    match outcome {
        ReembedOutcome::Done { facts, clusters } => {
            assert_eq!(facts, 3, "deleted facts are re-embedded too");
            assert_eq!(clusters, 1);
        }
        ReembedOutcome::Skipped(why) => panic!("unexpected skip: {why}"),
    }

    let memories = MemoryRepo::new(db.inner());
    for id in [&live1, &live2, &gone] {
        let fact = memories.get_fact(id).await.unwrap().unwrap();
        assert_eq!(fact.embedding, vec![1.0, 0.0, 0.0], "fact {id}");
    }
    assert!(memories.get_fact(&gone).await.unwrap().unwrap().deleted);

    let clusters = ClusterRepo::new(db.inner());
    let (cluster, _) = clusters
        .list_with_counts()
        .await
        .unwrap()
        .into_iter()
        .find(|(c, _)| c.id.as_ref().map(alexandria_storage::record_id_to_string) == Some(cid.clone()))
        .expect("cluster still exists");
    assert_eq!(cluster.centroid, vec![1.0, 0.0, 0.0]);

    assert_eq!(
        system_config::get_config(db.inner(), "embedding_model").await.unwrap().as_deref(),
        Some("b")
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_dimensions").await.unwrap().as_deref(),
        Some("3")
    );
}

#[tokio::test]
async fn reembed_is_noop_when_lock_matches() {
    let (db, live1, _, _, _) = seed().await;
    system_config::set_config(db.inner(), "embedding_model", "b").await.unwrap();

    let outcome = reembed(&db, &ModelB).await.unwrap();
    assert!(matches!(outcome, ReembedOutcome::Skipped(_)));

    let fact = MemoryRepo::new(db.inner()).get_fact(&live1).await.unwrap().unwrap();
    assert_eq!(fact.embedding, vec![0.6, 0.8], "untouched");
}

#[tokio::test]
async fn reembed_is_noop_on_fresh_database() {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner()).await.unwrap();

    let outcome = reembed(&db, &ModelB).await.unwrap();
    assert!(matches!(outcome, ReembedOutcome::Skipped(_)));
    assert!(system_config::get_config(db.inner(), "embedding_model").await.unwrap().is_none());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p alexandria-mcp --test reembed_test 2>&1 | tail -20`
Expected: compile error, `alexandria_mcp::migrate` not found.

- [ ] **Step 3: Implement**

`crates/alexandria-mcp/src/migrate.rs`:

```rust
//! Re-embed every fact and cluster centroid with a new model, then move the lock.
//! Not transactional: a failure mid-way leaves the lock on the old model, so the
//! server keeps refusing to boot and rerunning the migration is the recovery.

use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::repos::{ClusterRepo, MemoryRepo};
use alexandria_storage::{record_id_to_string, system_config, Database};

pub enum ReembedOutcome {
    /// Nothing to do; the string is a human-readable reason.
    Skipped(String),
    Done { facts: usize, clusters: usize },
}

pub async fn reembed(
    db: &Database,
    provider: &dyn EmbeddingProvider,
) -> anyhow::Result<ReembedOutcome> {
    let new_model = provider.model_id();
    match system_config::get_config(db.inner(), "embedding_model").await? {
        None => {
            return Ok(ReembedOutcome::Skipped(
                "no embedding lock found (fresh database); just start the server".into(),
            ))
        }
        Some(stored) if stored == new_model => {
            return Ok(ReembedOutcome::Skipped(format!("already on {new_model}")))
        }
        Some(stored) => tracing::info!("Re-embedding {stored} -> {new_model}"),
    }

    // 1. Facts, deleted ones included.
    let memories = MemoryRepo::new(db.inner());
    let rows = memories.all_ids_and_content().await?;
    let total = rows.len();
    for (i, (id, content)) in rows.iter().enumerate() {
        let vecs = provider.embed(&[content.as_str()]).await?;
        memories
            .update_fact(id, None, None, None, Some(&vecs[0]))
            .await?;
        if (i + 1) % 50 == 0 || i + 1 == total {
            tracing::info!("Re-embedded {}/{total} facts", i + 1);
        }
    }

    // 2. Centroids: plain mean of all members, deleted included, matching how the
    //    maintenance loop reads members via get_members.
    let clusters = ClusterRepo::new(db.inner());
    let dims = provider.dimensions();
    let mut updated = 0;
    for (cluster, _) in clusters.list_with_counts().await? {
        let Some(id) = cluster.id.as_ref().map(record_id_to_string) else {
            continue;
        };
        let members = clusters.get_members(&id).await?;
        if members.is_empty() {
            continue;
        }
        let mut centroid = vec![0.0f32; dims];
        for m in &members {
            for (c, e) in centroid.iter_mut().zip(&m.embedding) {
                *c += e;
            }
        }
        let n = members.len() as f32;
        for c in &mut centroid {
            *c /= n;
        }
        clusters.update_centroid(&id, &centroid).await?;
        updated += 1;
    }

    // 3. Lock last.
    system_config::set_config(db.inner(), "embedding_model", new_model).await?;
    system_config::set_config(db.inner(), "embedding_dimensions", &dims.to_string()).await?;

    Ok(ReembedOutcome::Done {
        facts: total,
        clusters: updated,
    })
}
```

`crates/alexandria-mcp/src/lib.rs`: add `pub mod migrate;` after `pub mod debug;`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p alexandria-mcp --test reembed_test 2>&1 | tail -20`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/alexandria-mcp/src/migrate.rs crates/alexandria-mcp/src/lib.rs crates/alexandria-mcp/tests/reembed_test.rs
git commit -m "feat(mcp): reembed() rewrites fact embeddings and centroids, moves the model lock

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 5: `alexandria migrate-embeddings` subcommand and error text

**Files:**
- Modify: `crates/alexandria/src/main.rs` (top of `main`, plus a new fn)
- Modify: `crates/alexandria-storage/src/system_config.rs` (mismatch message)

**Interfaces:**
- Consumes: Task 4 `alexandria_mcp::migrate::{reembed, ReembedOutcome}`.
- Produces: `alexandria migrate-embeddings` exits 0 with a message on skip or completion, non-zero on error. Any other argument is an error.

- [ ] **Step 1: Wire the argument**

In `main.rs`, right after `tracing_subscriber::fmt::init();` and the startup log line, before `// 1. Load configuration`, insert:

```rust
    if let Some(arg) = std::env::args().nth(1) {
        return match arg.as_str() {
            "migrate-embeddings" => migrate_embeddings().await,
            other => anyhow::bail!("unknown argument `{other}`. Usage: alexandria [migrate-embeddings]"),
        };
    }
```

Add the function after `main`:

```rust
/// `alexandria migrate-embeddings`: re-embed everything with the model in config and
/// move the lock. Run with the server stopped; the data dir is single-writer.
async fn migrate_embeddings() -> anyhow::Result<()> {
    use alexandria_mcp::migrate::{reembed, ReembedOutcome};

    let config = Config::load()?;
    let db = Database::connect(&config.database.data_dir).await?;
    schema::migrate(db.inner()).await?;

    tracing::info!("Loading embedding model: {}", config.embedding.model);
    let embedding = CandleProvider::new(&config.embedding.model, &config.embedding.device).await?;

    match reembed(&db, &embedding).await? {
        ReembedOutcome::Skipped(why) => println!("Nothing to do: {why}"),
        ReembedOutcome::Done { facts, clusters } => println!(
            "Re-embedded {facts} facts and {clusters} cluster centroids with {} ({} dims). Restart the service.",
            embedding.model_id(),
            embedding.dimensions()
        ),
    }
    Ok(())
}
```

- [ ] **Step 2: Update the mismatch message**

In `system_config.rs`, replace the three `Options:` lines in the `Embedding model mismatch!` bail with:

```rust
                     Options:\n\
                     1. Change your config back to: {stored_m}\n\
                     2. Run `alexandria migrate-embeddings` with the server stopped to re-embed everything with {model}\n\
                     3. Delete the database and start fresh"
```

- [ ] **Step 3: Build and check the unknown-argument path**

Run: `cargo build -p alexandria 2>&1 | tail -5 && ./target/debug/alexandria bogus; echo "exit=$?"`
Expected: builds; prints `Error: unknown argument \`bogus\`...` and `exit=1`.

- [ ] **Step 4: Check the subcommand end to end on a scratch database**

```bash
export ALEXANDRIA_DATA_DIR=/tmp/alexandria-bench/scratch
rm -rf "$ALEXANDRIA_DATA_DIR"
# fresh DB: no lock
ALEXANDRIA_EMBEDDING_MODEL=sentence-transformers/all-MiniLM-L6-v2 ./target/debug/alexandria migrate-embeddings
# expected: "Nothing to do: no embedding lock found ..."
# boot once to set the lock, then stop it
ALEXANDRIA_SERVER_TRANSPORT=http ALEXANDRIA_SERVER_PORT=3999 \
  ALEXANDRIA_EMBEDDING_MODEL=sentence-transformers/all-MiniLM-L6-v2 timeout 20 ./target/debug/alexandria || true
# now switch the model
ALEXANDRIA_EMBEDDING_MODEL=BAAI/bge-small-en-v1.5 ./target/debug/alexandria migrate-embeddings
# expected: "Re-embedded 0 facts and 0 cluster centroids with BAAI/bge-small-en-v1.5 (384 dims)."
ALEXANDRIA_EMBEDDING_MODEL=BAAI/bge-small-en-v1.5 ./target/debug/alexandria migrate-embeddings
# expected: "Nothing to do: already on BAAI/bge-small-en-v1.5"
ALEXANDRIA_SERVER_TRANSPORT=http ALEXANDRIA_SERVER_PORT=3999 \
  ALEXANDRIA_EMBEDDING_MODEL=sentence-transformers/all-MiniLM-L6-v2 timeout 20 ./target/debug/alexandria; echo "exit=$?"
# expected: mismatch error naming `alexandria migrate-embeddings`, non-zero exit
unset ALEXANDRIA_DATA_DIR
```

Note: the config file at `~/.config/alexandria/config.toml` pins `model`; the env var overrides it, so these runs never touch the live install.

- [ ] **Step 5: Run the workspace tests**

Run: `cargo test --workspace 2>&1 | grep -E '^test result|FAILED|panicked' | head -20`
Expected: every `test result:` line shows `0 failed`.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria/src/main.rs crates/alexandria-storage/src/system_config.rs
git commit -m "feat(cli): alexandria migrate-embeddings subcommand

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 6: New defaults, thresholds, and docs

**Files:**
- Modify: `crates/alexandria/src/config.rs` (`EmbeddingConfig`, `RetrieveConfig`, `ClusterConfig` defaults; `min_similarity` doc comment; `test_defaults`)
- Modify: `docs/configuration.md`, `docs/roadmap.md`, `README.md`
- Modify: `contrib/claude/hooks/alexandria-recall.sh`, `contrib/claude/README.md`
- Modify: `TODO-misc.md`

**Interfaces:**
- Consumes: the `## Chosen` table in `docs/plans/2026-09-08-embedding-model-swap-measurements.md`. Every value below written as `<chosen ...>` is copied from that table verbatim.
- Produces: shipped defaults match the chosen model.

If the chosen model is `all-MiniLM-L6-v2` (no candidate won), skip Steps 1 to 3 and 5, and in Step 4 only mark the roadmap item done and record the measurement outcome in `TODO-misc.md`.

- [ ] **Step 1: Update the config defaults and the test**

In `config.rs`:

- `impl Default for EmbeddingConfig`: `model: "<chosen model>".to_string()`.
- `impl Default for RetrieveConfig`: `min_similarity: <chosen retrieve.min_similarity>`.
- `impl Default for ClusterConfig`: `join_threshold`, `merge_threshold`, `cohesion_floor` to the chosen values; `maintenance_interval_secs` unchanged.
- Replace the doc comment on `RetrieveConfig::min_similarity` with:

```rust
    /// Server-side hard floor on cosine similarity for `retrieve_memories`
    /// results. A noise cutoff only, tuned to the default model: on
    /// <chosen model> (measured 2026-09-08 on the live corpus) a question against
    /// its matching memory scores at least <hit_min> and unrelated memories at most
    /// <nonhit_p99> at p99. Default <chosen retrieve.min_similarity>. If you pin a
    /// different model, retune; the old all-MiniLM-L6-v2 value was 0.10.
    pub min_similarity: f32,
```

- In `test_defaults`, update the four asserted values (`embedding.model`, `cluster.join_threshold`, `retrieve.min_similarity`, and add `assert_eq!(config.cluster.merge_threshold, <chosen>); assert_eq!(config.cluster.cohesion_floor, <chosen>);`). In `test_new_config_from_toml`, the line `assert_eq!(config.retrieve.min_similarity, 0.10);` becomes the chosen value.

Run: `cargo test -p alexandria config:: 2>&1 | tail -15`
Expected: all pass.

- [ ] **Step 2: Update `docs/configuration.md`**

- Line ~27 example: `model = "<chosen model>"`. Lines ~39 and ~45 example comments and defaults: chosen values. Also `merge_threshold` and `cohesion_floor` example lines.
- `[embedding]` table row for `model`: default becomes the chosen model. Keep "Must be a BERT-family model compatible with candle." and append: "Pooling mode (CLS or mean) is read from the model repo's `1_Pooling/config.json`; models without it use mean pooling."
- Directly under the `[embedding]` table, add:

```markdown
**Switching models on an existing database:** stop the server, set the new `model`, run `alexandria migrate-embeddings` (re-embeds every memory and cluster centroid, then updates the lock), and start the server again. Thresholds are tuned to the default model. If you keep `sentence-transformers/all-MiniLM-L6-v2` pinned, also pin its old defaults: `[cluster] join_threshold = 0.75, merge_threshold = 0.9, cohesion_floor = 0.6` and `[retrieve] min_similarity = 0.10`.
```

- The "**Model locking:**" paragraph: replace "or run a migration" with "or run `alexandria migrate-embeddings`".
- "**First run:**" paragraph: model size and name for the chosen model (bge-small is ~130 MB; the MiniLM variants ~80 MB).
- `[cluster]` table defaults: chosen values.
- `[retrieve]` `min_similarity` row: default and the measured sentence rewritten for the chosen model using `hit_min`, `hit_max`, `nonhit_p50`, `nonhit_p99` from the measurements table. Keep the MiniLM numbers as a trailing "Previous default on all-MiniLM-L6-v2: 0.10 (…)" sentence.
- `[recall]` `min_similarity` row (~line 165): recommended value becomes `<chosen client auto-recall threshold>`; keep the note that the Pi default `0.58` is pending an extension change.

- [ ] **Step 3: Update the hook default and its README**

`contrib/claude/hooks/alexandria-recall.sh`: line 18 comment default, the comment block at ~line 26 (rewrite for the chosen model: "measured 2026-09-08 on <chosen model>: question-vs-matching-memory >= <hit_min>, unrelated p90 <nonhit_p90>"), and line 28 `MIN_SIM="${ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY:-<chosen client auto-recall threshold>}"`.

`contrib/claude/README.md` line ~112: default column to the chosen client threshold.

Run: `bash contrib/claude/hooks/test.sh 2>&1 | tail -5`
Expected: passes (it sets the threshold to 0.0 itself, so only syntax matters).

- [ ] **Step 4: Roadmap, README, TODO**

- `docs/roadmap.md`: change `- Embedding model migration CLI (\`alexandria migrate-embeddings\`)` to `- ~~Embedding model migration CLI (\`alexandria migrate-embeddings\`)~~ Done 2026-09-08`. Leave "Multi-architecture models" as is (CLS pooling is still BERT).
- `README.md` line 7: `(all-MiniLM-L6-v2 via candle, pure Rust)` becomes `(<chosen model short name> via candle, pure Rust)`; line 31 example `model = "<chosen model>"`.
- `TODO-misc.md`, append to the "Embedding model is the real ceiling" entry:

```markdown
  Done 2026-09-08: default is now `<chosen model>` (measured on the live corpus, see
  `docs/plans/2026-09-08-embedding-model-swap-measurements.md`: mean rank <x> vs <y> for MiniLM,
  mean gap <a> vs <b>). Thresholds retuned by percentile matching. `alexandria migrate-embeddings`
  re-embeds an existing database; pooling mode is read from the model repo so CLS models work.
```

- [ ] **Step 5: Full test run and commit**

Run: `cargo test --workspace 2>&1 | grep -E '^test result|FAILED|panicked' | head -20`
Expected: all `0 failed`.

```bash
git add crates/alexandria/src/config.rs docs/configuration.md docs/roadmap.md README.md contrib/claude/hooks/alexandria-recall.sh contrib/claude/README.md TODO-misc.md
git commit -m "feat(config): default to <chosen model>, retune thresholds from measurements

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 7: Migrate the live database, remove the bench

**Files:**
- Delete: `crates/alexandria-pipeline/examples/model_bench.rs`
- Modify (outside repo): `~/.config/alexandria/config.toml`

**Interfaces:**
- Consumes: Task 5 subcommand, Task 6 chosen model.
- Produces: the live install runs on the chosen model.

**STOP: this task rewrites the user's live database. Do not start it without the user saying so in this session.** Skip Steps 2 to 6 entirely if the chosen model is MiniLM.

- [ ] **Step 1: Delete the bench and commit**

```bash
git rm crates/alexandria-pipeline/examples/model_bench.rs
git commit -m "chore(pipeline): remove throwaway model bench

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

- [ ] **Step 2: Build release and back up**

```bash
cargo build --release -p alexandria 2>&1 | tail -3
systemctl --user stop alexandria
cp -a ~/.local/share/alexandria/data ~/.local/share/alexandria/data.bak-minilm-2026-09-08
```

- [ ] **Step 3: Rehearse on the backup copy**

```bash
ALEXANDRIA_DATA_DIR=/tmp/alexandria-bench/rehearsal bash -c '
  rm -rf "$ALEXANDRIA_DATA_DIR" && cp -a ~/.local/share/alexandria/data.bak-minilm-2026-09-08 "$ALEXANDRIA_DATA_DIR" &&
  ALEXANDRIA_EMBEDDING_MODEL=<chosen model> ./target/release/alexandria migrate-embeddings'
```

Expected: `Re-embedded N facts and M cluster centroids with <chosen model> (384 dims).` with N equal to active plus deleted facts. Any error: do not proceed, report it.

- [ ] **Step 4: Migrate the real data dir**

Edit `~/.config/alexandria/config.toml`: `model = "<chosen model>"`. Then:

```bash
./target/release/alexandria migrate-embeddings
```

Expected: same `Re-embedded ...` line. Check the installed binary the service uses (`systemctl --user cat alexandria | grep ExecStart`); if it is not `./target/release/alexandria`, copy or reinstall it so the service runs the new build (`cargo install --path crates/alexandria` if that is how it was installed).

- [ ] **Step 5: Start and verify**

```bash
systemctl --user start alexandria
sleep 5
systemctl --user status alexandria --no-pager | head -5
journalctl --user -u alexandria -n 20 --no-pager | grep -E 'Pooling|Embedding model|ready'
```

Expected: active, log shows the chosen model and `Pooling: cls` (bge) or `Pooling: mean`.

Then run the hook harness and one real query from the question set:

```bash
bash contrib/claude/hooks/test.sh 2>&1 | tail -3
```

and in a Claude Code session (or via the debug UI at `http://127.0.0.1:3000/debug/query`) run `retrieve_memories` with two questions from the measurements file. Expected: the target memory ranks first with a similarity at or above `hit_min` from the measurements.

- [ ] **Step 6: Report**

Tell the user: the migration ran, counts, the backup path (`data.bak-minilm-2026-09-08`, delete when satisfied), and the two verification scores.
