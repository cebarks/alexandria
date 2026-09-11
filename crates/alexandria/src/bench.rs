//! `alexandria bench-retrieval`: measure how well the configured embedding model
//! separates a correct answer from the rest of the corpus, and derive the
//! `retrieve.min_similarity` floor from that model's own output rather than by hand.
//!
//! Read-only, but SurrealKV is single-writer: run it with the server stopped, or
//! against a copy of the data dir via `ALEXANDRIA_DATA_DIR`.
//!
//! Corpus vectors are read as stored, so this measures the model the corpus was
//! embedded with — only the questions are embedded here.

use alexandria_engine::search::cosine_similarity;
use alexandria_pipeline::embedding::{CandleProvider, EmbeddingProvider};
use alexandria_storage::repos::MemoryRepo;
use alexandria_storage::{Database, record_id_to_string};
use chrono::{DateTime, Utc};

use crate::config::Config;

/// Question -> the fact that answers it. Verbatim from the 2026-09-08 run
/// (`docs/plans/2026-09-08-embedding-model-swap-measurements.md`) so that new rows
/// stack on the table recorded there.
const QUESTIONS: [(&str, &str); 12] = [
    (
        "how do I make sure the claude hooks do not go stale after a git pull",
        "fact:114neszsc6wf6roti3nh",
    ),
    (
        "is there a maximum width I should wrap at when adding new code",
        "fact:jlzhe9hclr73wrlc3805",
    ),
    (
        "why does uv complain about hardlinks every time it installs packages here",
        "fact:ocga4ch6oj99evo16jcd",
    ),
    (
        "can I run the tests for both storage backends at the same time",
        "fact:18u1ll7xa80k9rd8f1dg",
    ),
    (
        "what does the third column on the cluster list page show",
        "fact:7lc4dgcj8pespq535i1e",
    ),
    (
        "what order do I have to drop a column in a strict surrealdb table",
        "fact:gir01cy2is9gohm25vi0",
    ),
    (
        "why does my shell loop stop consuming input halfway through",
        "fact:1t1ukxhfhwfoftc4p4j5",
    ),
    (
        "how many quadlet units is the gate supposed to find",
        "fact:lff3lvk2lzrgmp7lhe81",
    ),
    (
        "my script reads the wrong values when it queries a systemd service status, what is the flag gotcha",
        "fact:zlt6sp7we2v8sh6d6y67",
    ),
    (
        "the model keeps wrapping its answer in backticks and adding chatter afterwards, how should I read the structured output",
        "fact:306636gbydvykw7lrmr8",
    ),
    (
        "why are very short strings disappearing from what gets saved",
        "fact:ykw2fqnaj9j7q71o3mey",
    ),
    (
        "how fast is memory lookup supposed to be",
        "fact:g8q5rwzz89m4dyidz21h",
    ),
];

/// Size of the corpus the 2026-09-08 run measured. The comparison pass takes this many
/// of the oldest active facts, which is an approximation in one direction: facts that
/// were in the original and have been deleted since are gone, so the window reaches
/// forward past where it originally ended. Size rather than a timestamp cutoff because
/// size is what drives rank and the percentile tails, and so what has to match for the
/// metric definitions to be comparable at all.
const BASELINE_SIZE: usize = 143;

/// One corpus's worth of numbers, in the column order of the measurements table.
struct Metrics {
    corpus: usize,
    scored: usize,
    ranks: Vec<usize>,
    mean_rank: f64,
    top1: usize,
    mean_gap: f64,
    hit_min: f32,
    hit_max: f32,
    /// p50, p90, p99 of every question-to-non-target score.
    nonhit: [f32; 3],
    /// p50, p90, p99 of every fact-to-fact pair.
    ff: [f32; 3],
}

/// Linear-interpolated percentile, matching `numpy.percentile`'s default. The
/// 2026-09-08 second pass ran through numpy, so nearest-rank here would shift the
/// derived floor by a hundredth and quietly break comparability with that table.
fn percentile(sorted: &[f32], p: f64) -> f32 {
    if sorted.is_empty() {
        return f32::NAN;
    }
    let rank = p / 100.0 * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = (rank - lo as f64) as f32;
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

fn percentiles(values: &mut [f32]) -> [f32; 3] {
    values.sort_by(|a, b| a.total_cmp(b));
    [
        percentile(values, 50.0),
        percentile(values, 90.0),
        percentile(values, 99.0),
    ]
}

/// `scores[q][f]` is question `q` against corpus fact `f`; `targets[q]` is the index of
/// the fact that answers it, or `None` when that fact is not in this corpus.
fn compute(
    corpus: usize,
    scores: &[Vec<f32>],
    targets: &[Option<usize>],
    mut ff: Vec<f32>,
) -> Metrics {
    let mut ranks = Vec::new();
    let mut gaps = Vec::new();
    let mut hits = Vec::new();
    let mut nonhits = Vec::new();

    for (row, target) in scores.iter().zip(targets) {
        let Some(t) = *target else { continue };
        let hit = row[t];
        // Rank is 1 + however many facts outscore the target. Exact float ties would
        // resolve optimistically, which on real cosine values does not happen.
        let mut better = 0usize;
        let mut best_other = f32::NEG_INFINITY;
        for (i, &s) in row.iter().enumerate() {
            if i == t {
                continue;
            }
            if s > hit {
                better += 1;
            }
            if s > best_other {
                best_other = s;
            }
            nonhits.push(s);
        }
        ranks.push(better + 1);
        gaps.push((hit - best_other) as f64);
        hits.push(hit);
    }

    let scored = ranks.len();
    let mean_rank = ranks.iter().sum::<usize>() as f64 / scored as f64;
    let top1 = ranks.iter().filter(|&&r| r == 1).count();
    let mean_gap = gaps.iter().sum::<f64>() / scored as f64;

    Metrics {
        corpus,
        scored,
        mean_rank,
        top1,
        mean_gap,
        hit_min: hits.iter().copied().fold(f32::INFINITY, f32::min),
        hit_max: hits.iter().copied().fold(f32::NEG_INFINITY, f32::max),
        nonhit: percentiles(&mut nonhits),
        ff: percentiles(&mut ff),
        ranks,
    }
}

/// Score one corpus against the pre-embedded questions.
fn measure(facts: &[(String, Vec<f32>)], qvecs: &[Vec<f32>]) -> Metrics {
    let scores: Vec<Vec<f32>> = qvecs
        .iter()
        .map(|q| facts.iter().map(|(_, v)| cosine_similarity(q, v)).collect())
        .collect();

    let targets: Vec<Option<usize>> = QUESTIONS
        .iter()
        .map(|(_, id)| facts.iter().position(|(fid, _)| fid == id))
        .collect();

    let mut ff = Vec::with_capacity(facts.len() * facts.len() / 2);
    for (i, (_, a)) in facts.iter().enumerate() {
        for (_, b) in &facts[i + 1..] {
            ff.push(cosine_similarity(a, b));
        }
    }

    compute(facts.len(), &scores, &targets, ff)
}

fn report(label: &str, m: &Metrics, model: &str) {
    println!("\n## {label}");
    println!(
        "corpus: {} facts, {}/12 questions scored",
        m.corpus, m.scored
    );
    println!(
        "\n| model | mean_rank | top1 | mean_gap | hit_min | hit_max | nonhit_p50 | nonhit_p90 | nonhit_p99 | ff_p50 | ff_p90 | ff_p99 |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    println!(
        "| {model} | {:.2} | {}/{} | {:+.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} |",
        m.mean_rank,
        m.top1,
        m.scored,
        m.mean_gap,
        m.hit_min,
        m.hit_max,
        m.nonhit[0],
        m.nonhit[1],
        m.nonhit[2],
        m.ff[0],
        m.ff[1],
        m.ff[2],
    );

    println!("\nper-question rank:");
    for (i, ((q, _), rank)) in QUESTIONS.iter().zip(&m.ranks).enumerate() {
        let short: String = q.chars().take(64).collect();
        println!("  {:>2}. rank {:<4} {short}", i + 1, rank);
    }

    // The rule from docs/plans/2026-09-08-embedding-model-swap-design.md: the floor is
    // the median non-hit score, and it is only usable if it sits below the weakest hit.
    let floor = (m.nonhit[0] * 100.0).round() / 100.0;
    println!("\nretrieve.min_similarity = round(nonhit_p50, 2) = {floor:.2}");
    if floor < m.hit_min {
        println!(
            "  sanity check PASS: floor {floor:.2} < hit_min {:.3}",
            m.hit_min
        );
    } else {
        println!(
            "  sanity check FAIL: floor {floor:.2} >= hit_min {:.3}; this model has no usable \
             noise floor on this corpus",
            m.hit_min
        );
    }
}

pub async fn run() -> anyhow::Result<()> {
    let config = Config::load()?;
    tracing::info!("Reading corpus from {}", config.database.data_dir.display());
    let db = Database::connect(&config.database.data_dir).await?;

    let all: Vec<(String, Vec<f32>, Option<DateTime<Utc>>)> = MemoryRepo::new(db.inner())
        .list(None, None, false, 100_000, 0)
        .await?
        .into_iter()
        .filter_map(|f| {
            f.id.as_ref()
                .map(|id| (record_id_to_string(id), f.embedding, f.created_at))
        })
        .collect();
    anyhow::ensure!(!all.is_empty(), "no active facts in the corpus");
    let oldest = all.iter().filter_map(|(_, _, c)| *c).min();
    let newest = all.iter().filter_map(|(_, _, c)| *c).max();
    if let (Some(o), Some(n)) = (oldest, newest) {
        tracing::info!("Corpus created_at spans {o} .. {n}");
    }

    tracing::info!("Loading embedding model: {}", config.embedding.model);
    let provider = CandleProvider::new(&config.embedding.model, &config.embedding.device).await?;
    let questions: Vec<&str> = QUESTIONS.iter().map(|(q, _)| *q).collect();
    let qvecs = provider.embed(&questions).await?;

    let live: Vec<(String, Vec<f32>)> = all
        .iter()
        .map(|(id, v, _)| (id.clone(), v.clone()))
        .collect();
    report("Live corpus", &measure(&live, &qvecs), provider.model_id());

    let mut by_age = all;
    by_age.sort_by_key(|(_, _, created)| *created);
    by_age.truncate(BASELINE_SIZE);
    let through = by_age.last().and_then(|(_, _, c)| *c);
    let baseline: Vec<(String, Vec<f32>)> = by_age.into_iter().map(|(id, v, _)| (id, v)).collect();
    report(
        &format!(
            "Baseline corpus ({} oldest active facts, through {})",
            baseline.len(),
            through.map_or_else(|| "unknown".into(), |t| t.to_rfc3339())
        ),
        &measure(&baseline, &qvecs),
        provider.model_id(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_interpolates_between_neighbours() {
        let v = [0.1f32, 0.2, 0.3, 0.5, 0.6, 0.8];
        // rank = 0.5 * 5 = 2.5, halfway between 0.3 and 0.5
        assert!((percentile(&v, 50.0) - 0.4).abs() < 1e-6);
        assert!((percentile(&v, 0.0) - 0.1).abs() < 1e-6);
        assert!((percentile(&v, 100.0) - 0.8).abs() < 1e-6);
        assert!((percentile(&[0.7], 50.0) - 0.7).abs() < 1e-6);
    }

    #[test]
    fn compute_ranks_gaps_and_percentiles() {
        // q0 target is the best in the corpus; q1 target is beaten by two others.
        let scores = vec![vec![0.9, 0.5, 0.1, 0.3], vec![0.2, 0.8, 0.4, 0.6]];
        let targets = vec![Some(0), Some(2)];
        let ff = vec![0.1, 0.2, 0.3];
        let m = compute(4, &scores, &targets, ff);

        assert_eq!(m.ranks, vec![1, 3]);
        assert_eq!(m.top1, 1);
        assert_eq!(m.scored, 2);
        assert!((m.mean_rank - 2.0).abs() < 1e-9);
        // +0.4 on the hit, -0.4 on the miss
        assert!(m.mean_gap.abs() < 1e-6);
        assert!((m.hit_min - 0.4).abs() < 1e-6);
        assert!((m.hit_max - 0.9).abs() < 1e-6);
        // non-targets sorted: 0.1 0.2 0.3 0.5 0.6 0.8
        assert!((m.nonhit[0] - 0.4).abs() < 1e-6);
        assert!((m.ff[0] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn compute_skips_questions_whose_target_is_absent() {
        let scores = vec![vec![0.9, 0.5], vec![0.2, 0.8]];
        let targets = vec![Some(0), None];
        let m = compute(2, &scores, &targets, vec![0.5]);
        assert_eq!(m.scored, 1);
        assert_eq!(m.ranks, vec![1]);
    }
}
