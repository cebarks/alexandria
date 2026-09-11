# MiniLM retrieval test data

Measurements for `sentence-transformers/all-MiniLM-L6-v2`, the embedding model Alexandria
runs on. This is the living record; the model-selection passes that chose it are historical
and live in `docs/plans/2026-09-08-embedding-model-swap-measurements.md`.

Produced by `alexandria bench-retrieval` (`crates/alexandria/src/bench.rs`).

## Running it

SurrealKV is single-writer, so the bench cannot share the data dir with a running server.
Either stop the server for the run, or copy the data dir and point the bench at the copy —
the copy keeps the server down only for the duration of a `cp`:

```sh
systemctl --user stop alexandria
cp -r ~/.local/share/alexandria/data /tmp/alexandria-bench-snap
systemctl --user start alexandria
rm -f /tmp/alexandria-bench-snap/LOCK
ALEXANDRIA_DATA_DIR=/tmp/alexandria-bench-snap alexandria bench-retrieval
```

It prints two rows: the live corpus, and a baseline of the `BASELINE_SIZE` (143) oldest
active facts. The baseline exists to prove the metric definitions still match the ones
behind the recorded tables — if it stops reproducing, distrust the live row.

Corpus vectors are read as stored rather than recomputed, so the bench measures the model
the corpus was actually embedded with. Only the 12 questions are embedded at run time.

## Results (2026-09-09)

Corpus `created_at` spans 2026-09-08 12:30:01 UTC .. 2026-09-09 20:33:50 UTC. The baseline
subset runs through 2026-09-08 16:26:33 UTC.

| corpus | facts | mean_rank | top1 | mean_gap | hit_min | hit_max | nonhit_p50 | nonhit_p90 | nonhit_p99 | ff_p50 | ff_p90 | ff_p99 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (143 oldest active) | 143 | 1.42 | 9/12 | +0.148 | 0.338 | 0.667 | 0.078 | 0.212 | 0.372 | 0.130 | 0.298 | 0.560 |
| live | 743 | 2.75 | 7/12 | +0.077 | 0.338 | 0.667 | 0.074 | 0.198 | 0.341 | 0.128 | 0.287 | 0.507 |
| live (threshold sweep run) | 807 | 2.83 | 7/12 | +0.077 | 0.338 | 0.667 | 0.074 | 0.197 | 0.339 | 0.128 | 0.287 | 0.506 |

Per-question rank:

| # | question | baseline | live |
|---|---|---|---|
| 1 | how do I make sure the claude hooks do not go stale after a git pull | 4 | 8 |
| 2 | is there a maximum width I should wrap at when adding new code | 1 | 1 |
| 3 | why does uv complain about hardlinks every time it installs packages here | 1 | 1 |
| 4 | can I run the tests for both storage backends at the same time | 1 | 5 |
| 5 | what does the third column on the cluster list page show | 1 | 1 |
| 6 | what order do I have to drop a column in a strict surrealdb table | 1 | 1 |
| 7 | why does my shell loop stop consuming input halfway through | 1 | 1 |
| 8 | how many quadlet units is the gate supposed to find | 1 | 1 |
| 9 | my script reads the wrong values when it queries a systemd service status, what is the flag gotcha | 1 | 1 |
| 10 | the model keeps wrapping its answer in backticks and adding chatter afterwards, how should I read the structured output | 1 | 2 |
| 11 | why are very short strings disappearing from what gets saved | 2 | 5 |
| 12 | how fast is memory lookup supposed to be | 2 | 6 |

### Baseline check

The baseline row reproduces the 2026-09-08 candle pass on every column — recorded 1.42,
9/12, +0.148, 0.338, 0.667, 0.077, 0.212, 0.373, 0.130, 0.298, 0.562 — and every
per-question rank. The metric definitions therefore agree with the recorded tables, and the
live row can be compared against them.

### Reading the live row

Retrieval got worse, and only because the haystack grew. `hit_min` and `hit_max` are
identical across the two rows: the same targets, scored by the same question vectors, get
the same cosine values. What moved is how many facts sit above them — `mean_rank` 1.42 to
2.75, `top1` 9/12 to 7/12, `mean_gap` halved from +0.148 to +0.077.

The noise tail moved the other way: `nonhit_p99` 0.373 to 0.341, `ff_p99` 0.562 to 0.507.
The 590 facts added since are on average *less* similar to these questions than the original
corpus, which tightens the distribution while still crowding the target on rank. A floor
derived from the non-hit distribution therefore gets slightly looser as the corpus grows, at
the same time as ranking gets harder. **The floor is not a proxy for retrieval quality.**

The third row is the same measurement re-run later the same day to collect the threshold
sweep below, by which point the corpus had grown 743 -> 807. Everything moved in the
direction the second row predicts and by very little: `mean_rank` 2.75 -> 2.83, one
per-question rank change (q12, 6 -> 7), and the noise tail one thousandth tighter. `top1`,
`mean_gap`, `hit_min` and `hit_max` are unchanged. Two live rows 64 facts apart are not a
trend, but they do bound the short-term jitter: it is smaller than the 143 -> 743 move by
more than an order of magnitude.

### Floor

The rule from `docs/plans/2026-09-08-embedding-model-swap-design.md` — `round(nonhit_p50, 2)`,
valid only if it sits below `hit_min` — gives:

| corpus | floor | sanity check |
|---|---|---|
| baseline | 0.08 | pass (`hit_min` 0.338) |
| live | 0.07 | pass (`hit_min` 0.338) |

The configured default is `0.10` and is left unchanged: both values sit far below the weakest
true hit, so the difference is immaterial. Note that the rule's output is a property of the
model *and the corpus*, not of the model alone — it drifts down as the corpus grows.

### Client threshold

The floor above is the server's noise cutoff. The number that decides what a user actually
sees is the *client* threshold — `[recall] min_similarity`, applied by the auto-recall hook
to what `retrieve_memories(limit=5)` returned. The sweep simulates that filter: for each
candidate threshold, how many of the 12 targets survive, how many of those the limit would
have delivered anyway, and how many non-targets ride along.

Live corpus, 807 facts:

| T | hits_kept | hits_delivered | noise_per_q |
|---|---|---|---|
| 0.30 | 12/12 | 10/12 | 4.08 |
| **0.35** | 11/12 | **9/12** | 3.00 |
| 0.40 | 8/12 | 7/12 | 1.42 |
| 0.45 | 8/12 | 7/12 | 0.67 |
| 0.50 | 7/12 | 7/12 | 0.33 |
| 0.58 | 4/12 | **4/12** | 0.17 |

Baseline corpus, 143 facts:

| T | hits_kept | hits_delivered | noise_per_q |
|---|---|---|---|
| 0.30 | 12/12 | 12/12 | 2.33 |
| **0.35** | 11/12 | **11/12** | 1.42 |
| 0.40 | 8/12 | 8/12 | 0.83 |
| 0.45 | 8/12 | 8/12 | 0.33 |
| 0.50 | 7/12 | 7/12 | 0.00 |
| 0.58 | 4/12 | **4/12** | 0.00 |

**`0.58` is wrong, and this is the first corpus measurement that says so.** It delivers 4/12
— it drops two thirds of the true hits. The claim was already in `docs/configuration.md`, but
it rested on the 2026-09-08 synthetic-pair ranges rather than on the corpus.

**`0.35` is a recall-favouring choice, and it is not free.** It delivers 9/12 live, 11/12 on
the baseline. The one target it drops outright is the weakest, at `hit_min` 0.338 — so it is
not accurate to say `0.35` keeps every real hit; it keeps 11 of 12 by score and delivers 9.

**`0.40` is strictly dominated on both corpora** — the same `hits_delivered` as `0.45` at
roughly double the noise. It is never the right pick, whatever else is being traded off.

**The remaining choice is `0.35` against `0.50`:** 9 hits at 3.00 injected non-targets, or 7
hits at 0.33. Nine times the injection for two more hits out of twelve. `0.35` is kept
because `noise_per_q` is an upper bound in a way `hits_delivered` is not — see the limits
below — and because a memory that never surfaces is the failure auto-recall exists to
prevent, while an extra adjacent memory costs a few hundred prompt tokens.

**`0.30` shows the threshold is not always the binding constraint.** Even admitting every
target by score, the live row delivers 10/12: two targets rank 8th and 7th, outside
`limit=5`, so no threshold reaches them. For those questions the lever is
`ALEXANDRIA_AUTO_RECALL_LIMIT`, which no document currently discusses as one.

Limits on how far to read this table:

- Twelve questions. A six-row table computed off twelve samples reads far more precise than
  it is, and single-hit differences between adjacent rows are noise.
- `noise_per_q` counts every non-target, so it is an **upper bound on useless injection**: a
  memory that is not the designated target may still be exactly what the prompt needed. The
  true cost of a low threshold is therefore somewhere below these numbers, by an unmeasured
  amount. `hits_delivered` has no such slack — a dropped target is a real miss.
- Every target predates 2026-09-08 16:26 UTC, so the 664 facts added since can only ever be
  noise here. That inflates `noise_per_q` and cannot deflate it.

## Metric definitions

- **rank** — position of the target fact when the whole corpus is sorted by cosine descending
  against the question, 1-based. Computed as `1 + count(facts scoring above the target)`.
- **top1** — questions whose target ranked 1.
- **mean_gap** — mean over questions of (target score − best non-target score). Negative on a
  miss, so a corpus that crowds the target drags it toward zero.
- **hit_min / hit_max** — min and max of the 12 target scores. Independent of corpus size.
- **nonhit_pN** — percentiles over every question-to-non-target score (12 × corpus).
- **ff_pN** — percentiles over every fact-to-fact pair.
- **hits_kept** — targets scoring at or above the client threshold, ignoring rank.
- **hits_delivered** — targets that clear the threshold *and* rank within `RECALL_LIMIT` (5,
  the auto-recall hook's default `limit`), so a client would actually be shown them. The
  honest recall number; `hits_kept` alone only restates whether the threshold sits below a
  target's score.
- **noise_per_q** — mean non-targets per question that survive both the limit and the
  threshold. The server floor is not modelled: every swept threshold is far above it.

Percentiles use linear interpolation, matching `numpy.percentile`'s default. The 2026-09-08
second pass ran through numpy; nearest-rank here would shift the derived floor by a hundredth
and silently break comparability with the recorded tables.

## Test data

Question → the fact that answers it. Frozen from the 2026-09-08 run so new rows stack on the
recorded tables; the list is `QUESTIONS` in `crates/alexandria/src/bench.rs`.

1. "how do I make sure the claude hooks do not go stale after a git pull" -> `fact:114neszsc6wf6roti3nh`
2. "is there a maximum width I should wrap at when adding new code" -> `fact:jlzhe9hclr73wrlc3805`
3. "why does uv complain about hardlinks every time it installs packages here" -> `fact:ocga4ch6oj99evo16jcd`
4. "can I run the tests for both storage backends at the same time" -> `fact:18u1ll7xa80k9rd8f1dg`
5. "what does the third column on the cluster list page show" -> `fact:7lc4dgcj8pespq535i1e`
6. "what order do I have to drop a column in a strict surrealdb table" -> `fact:gir01cy2is9gohm25vi0`
7. "why does my shell loop stop consuming input halfway through" -> `fact:1t1ukxhfhwfoftc4p4j5`
8. "how many quadlet units is the gate supposed to find" -> `fact:lff3lvk2lzrgmp7lhe81`
9. "my script reads the wrong values when it queries a systemd service status, what is the flag gotcha" -> `fact:zlt6sp7we2v8sh6d6y67`
10. "the model keeps wrapping its answer in backticks and adding chatter afterwards, how should I read the structured output" -> `fact:306636gbydvykw7lrmr8`
11. "why are very short strings disappearing from what gets saved" -> `fact:ykw2fqnaj9j7q71o3mey`
12. "how fast is memory lookup supposed to be" -> `fact:g8q5rwzz89m4dyidz21h`

## Limitations

- **Every target predates 2026-09-08 16:26 UTC.** The 590 facts added since are never a
  correct answer, only distractors. This makes the live row a clean measurement of fixed
  questions against a growing haystack, but it does not check that a recently stored memory
  can be found at all. Add questions targeting recent facts before reading the bench as a
  general retrieval-quality signal.
- **The baseline is reconstructed by size, not identity.** `BASELINE_SIZE = 143` takes the
  143 oldest *active* facts, which is not the same set that was active on 2026-09-08: any of
  those deleted since drops out and the window reaches forward to replace it. Drift is
  currently negligible — the row reproduces exactly — but it grows with every deletion and
  the tool cannot detect it.
- **A timestamp cutoff cannot be substituted for the size-based baseline.** No active fact
  predates 2026-09-08 12:30 UTC. The `08:08` in the measurements doc is local time (UTC-4)
  and is when the data dir was created, not when the measurement ran, so a cutoff built from
  it selects nothing.
- **12 questions is a small sample.** A single question changing rank moves `mean_rank` by up
  to a twelfth of the change. Treat differences of a tenth as noise.
