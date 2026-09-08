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
