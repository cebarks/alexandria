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

## Metric definitions

- **rank** — position of the target fact when the whole corpus is sorted by cosine descending
  against the question, 1-based. Computed as `1 + count(facts scoring above the target)`.
- **top1** — questions whose target ranked 1.
- **mean_gap** — mean over questions of (target score − best non-target score). Negative on a
  miss, so a corpus that crowds the target drags it toward zero.
- **hit_min / hit_max** — min and max of the 12 target scores. Independent of corpus size.
- **nonhit_pN** — percentiles over every question-to-non-target score (12 × corpus).
- **ff_pN** — percentiles over every fact-to-fact pair.

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
