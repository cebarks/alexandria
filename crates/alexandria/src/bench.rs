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

/// Question -> the fact that answers it. The first 12 are verbatim from the 2026-09-08 run
/// (`docs/plans/2026-09-08-embedding-model-swap-measurements.md`); their targets are all in
/// the baseline window, so the baseline row still reproduces the table recorded there. The
/// rest (added 2026-09-10) target facts stored after that window, from other projects, so
/// the live row also checks that a recent memory can be found and is not just measuring
/// fixed questions against a growing haystack. They are absent from the baseline corpus and
/// score as `absent` on that row.
const QUESTIONS: [(&str, &str); 20] = [
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
    (
        "pkill -f with an anchored path pattern does not find my process even though it is running",
        "fact:s54d27iol1v5cvr3dfeh",
    ),
    (
        "jj squash prints an error about the editor failing to initialize, did the squash happen",
        "fact:8x2na75v2uyuvtj2y2d8",
    ),
    (
        "passing zero to a seconds flag pegs a core, how should I constrain the argument",
        "fact:m27pl2qci3h2idbz4ku7",
    ),
    (
        "a tiny text change produced a huge diff in the rendered svg, why",
        "fact:ij7g7c4gql4byjq0wsf3",
    ),
    (
        "reading the link speed file under sys class net gives -1 or an error for some interfaces",
        "fact:o3z29nj29curn65rn0e3",
    ),
    (
        "what is the config key for pango markup on a text block",
        "fact:46x5bi68675hryxec20y",
    ),
    (
        "would turning on object lock for the backup bucket break restic",
        "fact:61zqxqcijsi7n847632x",
    ),
    (
        "the lifecycle rule has been on for a day and nothing expired yet, is it broken",
        "fact:i2tf44mnoigxqro4898n",
    ),
];

/// Size of the corpus the 2026-09-08 run measured. The comparison pass takes this many
/// of the oldest active facts, which is an approximation in one direction: facts that
/// were in the original and have been deleted since are gone, so the window reaches
/// forward past where it originally ended. Size rather than a timestamp cutoff because
/// size is what drives rank and the percentile tails, and so what has to match for the
/// metric definitions to be comparable at all.
const BASELINE_SIZE: usize = 143;

/// Client-side auto-recall cutoffs to sweep. `0.45` is the measured default both clients
/// ship; `0.35` was the default until the limit was measured and `0.58` before that, both
/// kept in the sweep so the comparisons that retired them stay reproducible. The rest
/// bracket the three.
const THRESHOLDS: [f32; 6] = [0.30, 0.35, 0.40, 0.45, 0.50, 0.58];

/// How many results the auto-recall hook asks the server for
/// (`ALEXANDRIA_AUTO_RECALL_LIMIT`, default 10 — `contrib/claude/hooks/alexandria-recall.sh`).
/// A target ranked below this never reaches the client, whatever the threshold. Was 5 until
/// 2026-09-09, when the grid below measured it; the threshold tables recorded in
/// `docs/minilm-test-data.md` before that date are the `limit = 5` row and will not reproduce
/// from the single-limit table any more — compare them against the grid's `5` row instead.
const RECALL_LIMIT: usize = 10;

/// The client-side threshold shipped alongside `RECALL_LIMIT` (`ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY`,
/// default 0.45). A target scoring below it is dropped whatever its rank, so the limit can only
/// ever hide a target that scores at or above this — the headroom check counts those alone.
const RECALL_THRESHOLD: f32 = 0.45;

/// Result limits to sweep alongside `THRESHOLDS`. Raising the limit and lowering the
/// threshold trade against each other, so neither is readable from a single row — the
/// grid is the only honest view. `RECALL_LIMIT` is in the set so the recorded
/// single-limit table still reproduces from the same run.
const LIMITS: [usize; 6] = [3, 5, 8, 10, 15, 20];

/// Most facts read from the corpus in one `list` call. `MemoryRepo::list` orders newest-first,
/// so a corpus at or past this size would silently drop the *oldest* facts — the baseline
/// window and every frozen `QUESTIONS` target — and `run()` bails instead.
const CORPUS_CAP: usize = 100_000;

/// One (limit, threshold) cell of the client-filter simulation.
struct Sweep {
    limit: usize,
    threshold: f32,
    /// Targets scoring at or above the threshold, ignoring rank. Limit-independent, so
    /// it repeats down each threshold column.
    hits_kept: usize,
    /// Targets that clear the threshold *and* land in the top `limit`, so a client would
    /// actually see them. The honest recall number.
    hits_delivered: usize,
    /// Mean non-targets per question that survive both the limit and the threshold.
    /// An upper bound on useless injection: a non-target can still be a useful memory.
    noise_per_q: f64,
}

/// One corpus's worth of numbers, in the column order of the measurements table.
struct Metrics {
    corpus: usize,
    scored: usize,
    /// One slot per `QUESTIONS` entry; `None` where the target is absent from this corpus.
    ranks: Vec<Option<usize>>,
    /// Each target's own score, parallel to `ranks`.
    hit_scores: Vec<Option<f32>>,
    mean_rank: f64,
    top1: usize,
    mean_gap: f64,
    hit_min: f32,
    hit_max: f32,
    /// p50, p90, p99 of every question-to-non-target score.
    nonhit: [f32; 3],
    /// p50, p90, p99 of every fact-to-fact pair.
    ff: [f32; 3],
    /// Client-filter simulation, one entry per `LIMITS` x `THRESHOLDS` cell.
    sweep: Vec<Sweep>,
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
        let Some(t) = *target else {
            ranks.push(None);
            hits.push(None);
            continue;
        };
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
        ranks.push(Some(better + 1));
        gaps.push((hit - best_other) as f64);
        hits.push(Some(hit));
    }

    let scored = gaps.len();
    let mean_rank = ranks.iter().flatten().sum::<usize>() as f64 / scored as f64;
    let top1 = ranks.iter().filter(|&&r| r == Some(1)).count();
    let mean_gap = gaps.iter().sum::<f64>() / scored as f64;

    Metrics {
        corpus,
        scored,
        mean_rank,
        top1,
        mean_gap,
        hit_min: hits.iter().flatten().copied().fold(f32::INFINITY, f32::min),
        hit_max: hits
            .iter()
            .flatten()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max),
        nonhit: percentiles(&mut nonhits),
        ff: percentiles(&mut ff),
        sweep: sweep(scores, targets),
        ranks,
        hit_scores: hits,
    }
}

/// Simulate what auto-recall actually does: the server returns the top `limit`, the
/// client drops anything below its threshold, and whatever is left is injected into the
/// prompt. Both levers are swept because they trade against each other. The server floor
/// is not modelled — every swept threshold is far above it, so it cannot change the
/// outcome.
fn sweep(scores: &[Vec<f32>], targets: &[Option<usize>]) -> Vec<Sweep> {
    // Rank once per question; limit and threshold only filter that one ordering.
    let scored: Vec<(&Vec<f32>, usize, Vec<usize>)> = scores
        .iter()
        .zip(targets)
        .filter_map(|(row, target)| {
            let t = (*target)?;
            let mut idx: Vec<usize> = (0..row.len()).collect();
            idx.sort_by(|&a, &b| row[b].total_cmp(&row[a]));
            Some((row, t, idx))
        })
        .collect();

    let mut out = Vec::with_capacity(LIMITS.len() * THRESHOLDS.len());
    for &limit in &LIMITS {
        for &threshold in &THRESHOLDS {
            let mut hits_kept = 0;
            let mut hits_delivered = 0;
            let mut noise = 0usize;
            for (row, t, ranked) in &scored {
                if row[*t] >= threshold {
                    hits_kept += 1;
                }
                for &i in ranked.iter().take(limit) {
                    if row[i] < threshold {
                        continue;
                    }
                    if i == *t {
                        hits_delivered += 1;
                    } else {
                        noise += 1;
                    }
                }
            }
            out.push(Sweep {
                limit,
                threshold,
                hits_kept,
                hits_delivered,
                noise_per_q: noise as f64 / scored.len() as f64,
            });
        }
    }
    out
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
        "corpus: {} facts, {}/{} questions scored",
        m.corpus,
        m.scored,
        QUESTIONS.len()
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
    for (i, (((q, _), rank), score)) in QUESTIONS
        .iter()
        .zip(&m.ranks)
        .zip(&m.hit_scores)
        .enumerate()
    {
        let short: String = q.chars().take(64).collect();
        match rank.zip(*score) {
            Some((r, s)) => println!("  {:>2}. rank {r:<4} score {s:.2}  {short}", i + 1),
            None => println!("  {:>2}. absent                {short}", i + 1),
        }
    }

    // `RECALL_LIMIT` was set where delivery saturates, which is a property of the corpus, not
    // the model: rank inflates as the corpus grows, and a target past the limit that clears
    // the threshold on score is never delivered, with no other signal. Targets under
    // `RECALL_THRESHOLD` are excluded — the client drops those at any limit, so their rank
    // is not the limit's problem (the first version of this check counted them and warned on
    // a target the threshold had already discarded). A rank equal to the limit is delivered.
    let kept: Vec<usize> = m
        .ranks
        .iter()
        .zip(&m.hit_scores)
        .filter_map(|(rank, score)| rank.zip(*score))
        .filter(|(_, score)| *score >= RECALL_THRESHOLD)
        .map(|(rank, _)| rank)
        .collect();
    let worst = kept.iter().copied().max().unwrap_or(0);
    let hidden = kept.iter().filter(|&&r| r > RECALL_LIMIT).count();
    if hidden == 0 {
        println!(
            "\nrecall limit headroom: worst rank {worst} of limit {RECALL_LIMIT} among the {} targets \
             at or above T={RECALL_THRESHOLD:.2} ({} positions)",
            kept.len(),
            RECALL_LIMIT - worst
        );
    } else {
        println!(
            "\nrecall limit headroom WARN: {hidden} of the {} targets at or above T={RECALL_THRESHOLD:.2} \
             rank past limit {RECALL_LIMIT} (worst {worst}) and are never delivered — re-run the grid \
             below and revisit the client default",
            kept.len()
        );
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

    println!("\nclient threshold sweep (server returns top {RECALL_LIMIT}, client drops below T):");
    println!("\n| T | hits_kept | hits_delivered | noise_per_q |");
    println!("|---|---|---|---|");
    for s in m.sweep.iter().filter(|s| s.limit == RECALL_LIMIT) {
        println!(
            "| {:.2} | {}/{} | {}/{} | {:.2} |",
            s.threshold, s.hits_kept, m.scored, s.hits_delivered, m.scored, s.noise_per_q
        );
    }

    // hits_kept is limit-independent, so the grid drops it: the whole point here is that
    // a target can clear every threshold and still never be delivered.
    println!(
        "\nlimit x threshold grid, hits_delivered out of {} (noise_per_q):",
        m.scored
    );
    print!("\n| limit |");
    for t in THRESHOLDS {
        print!(" T={t:.2} |");
    }
    println!("\n|---|{}", "---|".repeat(THRESHOLDS.len()));
    for &limit in &LIMITS {
        let marker = if limit == RECALL_LIMIT { " (now)" } else { "" };
        print!("| {limit}{marker} |");
        for s in m.sweep.iter().filter(|s| s.limit == limit) {
            print!(" {} ({:.2}) |", s.hits_delivered, s.noise_per_q);
        }
        println!();
    }
}

pub async fn run() -> anyhow::Result<()> {
    let config = Config::load()?;
    tracing::info!("Reading corpus from {}", config.database.data_dir.display());
    let db = Database::connect(&config.database.data_dir).await?;

    let all: Vec<(String, Vec<f32>, Option<DateTime<Utc>>)> = MemoryRepo::new(db.inner())
        .list(None, None, false, CORPUS_CAP, 0)
        .await?
        .into_iter()
        .filter_map(|f| {
            f.id.as_ref()
                .map(|id| (record_id_to_string(id), f.embedding, f.created_at))
        })
        .collect();
    anyhow::ensure!(!all.is_empty(), "no active facts in the corpus");
    anyhow::ensure!(
        all.len() < CORPUS_CAP,
        "corpus hit the {CORPUS_CAP}-fact read cap; the read is newest-first, so the oldest facts \
         (the baseline window and the frozen QUESTIONS targets) are missing and every metric would \
         be wrong without saying so. Raise CORPUS_CAP in crates/alexandria/src/bench.rs."
    );
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
    let live_metrics = measure(&live, &qvecs);
    anyhow::ensure!(
        live_metrics.scored > 0,
        "none of the {} benchmark questions' target facts exist in this corpus — the `fact:` \
         record IDs in QUESTIONS (crates/alexandria/src/bench.rs) are frozen from the install the question set was \
         built on, so bench-retrieval only measures that database. Every metric would be NaN or inf.",
        QUESTIONS.len()
    );
    report("Live corpus", &live_metrics, provider.model_id());

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

        assert_eq!(m.ranks, vec![Some(1), Some(3)]);
        assert_eq!(m.hit_scores, vec![Some(0.9), Some(0.4)]);
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
    fn sweep_separates_kept_from_delivered_and_counts_noise() {
        // q0: target scores 0.36, straddling the 0.35 row, and ranks 3rd.
        // q1: target scores 0.50 but ranks 6th, so the limit hides it whatever T is.
        let scores = vec![
            vec![0.36, 0.60, 0.50, 0.34, 0.20, 0.10],
            vec![0.90, 0.80, 0.70, 0.60, 0.55, 0.50],
        ];
        let targets = vec![Some(0), Some(5)];
        let s = sweep(&scores, &targets);
        assert_eq!(s.len(), LIMITS.len() * THRESHOLDS.len());

        let cell = |limit: usize, t: f32| {
            s.iter()
                .find(|r| r.limit == limit && (r.threshold - t).abs() < 1e-6)
                .unwrap()
        };
        // Bound to a literal 5, not `RECALL_LIMIT`: these assertions describe the fixture's
        // limit-5 behaviour, which is the interesting case because q1's target sits at rank 6.
        // Tying them to the shipped default would silently re-point them every time it moves.
        let at = |t: f32| cell(5, t);

        // 0.30 clears both targets; only q0's is inside the top 5.
        assert_eq!(at(0.30).hits_kept, 2);
        assert_eq!(at(0.30).hits_delivered, 1);
        assert!((at(0.30).noise_per_q - 4.0).abs() < 1e-9); // (3 + 5) / 2

        // 0.35 still keeps q0's 0.36; one fewer non-target survives.
        assert_eq!(at(0.35).hits_kept, 2);
        assert_eq!(at(0.35).hits_delivered, 1);
        assert!((at(0.35).noise_per_q - 3.5).abs() < 1e-9); // (2 + 5) / 2

        // 0.40 drops q0's target entirely while admitting the same noise as 0.35 did.
        assert_eq!(at(0.40).hits_kept, 1);
        assert_eq!(at(0.40).hits_delivered, 0);

        // 0.58 drops both targets and still injects 2.5 non-targets per question.
        assert_eq!(at(0.58).hits_kept, 0);
        assert_eq!(at(0.58).hits_delivered, 0);
        assert!((at(0.58).noise_per_q - 2.5).abs() < 1e-9); // (1 + 4) / 2

        // The limit dimension: q1's target ranks 6th, so no threshold reaches it at 5 and
        // every threshold below 0.50 reaches it at 8. 8 exceeds the 6-fact fixture, so it
        // is the whole corpus — and the noise is unchanged from limit 5, because the two
        // entries the wider limit admits are q0's 0.20/0.10, already below every T here.
        assert_eq!(cell(8, 0.30).hits_delivered, 2);
        assert!((cell(8, 0.30).noise_per_q - 4.0).abs() < 1e-9);
        // 0.58 is above q1's 0.50 target, so widening the limit buys nothing there.
        assert_eq!(cell(8, 0.58).hits_delivered, 0);

        // Narrowing to 3 keeps q0's target (rank 3) and sheds two non-targets.
        assert_eq!(cell(3, 0.35).hits_delivered, 1);
        assert!((cell(3, 0.35).noise_per_q - 2.5).abs() < 1e-9); // (2 + 3) / 2

        // hits_kept ignores rank, so it must not move with the limit.
        assert_eq!(cell(3, 0.35).hits_kept, cell(20, 0.35).hits_kept);

        // `report()` prints the single-limit threshold table by filtering the sweep on
        // RECALL_LIMIT, so a RECALL_LIMIT outside LIMITS would print an empty table and say
        // nothing about it. This is the one coupling between the two constants.
        assert!(LIMITS.contains(&RECALL_LIMIT));
        // The headroom line reasons about the shipped pair; keep it on the grid so the two
        // can be read against each other.
        assert!(THRESHOLDS.contains(&RECALL_THRESHOLD));
        // At the shipped limit the whole 6-fact fixture fits, so both targets are delivered.
        assert_eq!(cell(RECALL_LIMIT, 0.30).hits_delivered, 2);
    }

    #[test]
    fn compute_keeps_absent_targets_in_position() {
        // q0's target is not in this corpus; q1's is. The per-question slots stay parallel
        // to QUESTIONS so `report()` prints each rank against its own question.
        let scores = vec![vec![0.9, 0.5], vec![0.2, 0.8]];
        let targets = vec![None, Some(1)];
        let m = compute(2, &scores, &targets, vec![0.5]);
        assert_eq!(m.scored, 1);
        assert_eq!(m.ranks, vec![None, Some(1)]);
        assert_eq!(m.hit_scores, vec![None, Some(0.8)]);
        assert!((m.mean_rank - 1.0).abs() < 1e-9);
        assert_eq!(m.top1, 1);
    }
}
