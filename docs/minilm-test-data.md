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
the corpus was actually embedded with. Only the questions are embedded at run time.

## Results (2026-09-09)

Corpus `created_at` spans 2026-09-08 12:30:01 UTC .. 2026-09-10 00:44:21 UTC. The baseline
subset runs through 2026-09-08 16:26:33 UTC.

| corpus | facts | mean_rank | top1 | mean_gap | hit_min | hit_max | nonhit_p50 | nonhit_p90 | nonhit_p99 | ff_p50 | ff_p90 | ff_p99 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (143 oldest active) | 143 | 1.42 | 9/12 | +0.148 | 0.338 | 0.667 | 0.078 | 0.212 | 0.372 | 0.130 | 0.298 | 0.560 |
| live | 743 | 2.75 | 7/12 | +0.077 | 0.338 | 0.667 | 0.074 | 0.198 | 0.341 | 0.128 | 0.287 | 0.507 |
| live (threshold sweep run) | 807 | 2.83 | 7/12 | +0.077 | 0.338 | 0.667 | 0.074 | 0.197 | 0.339 | 0.128 | 0.287 | 0.506 |
| live (limit sweep run) | 880 | 3.25 | 7/12 | +0.065 | 0.338 | 0.667 | 0.075 | 0.197 | 0.341 | 0.129 | 0.286 | 0.501 |
| live (headroom guard run) | 927 | 3.42 | 7/12 | +0.064 | 0.338 | 0.667 | 0.075 | 0.198 | 0.341 | 0.129 | 0.286 | 0.497 |

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

The fifth row (927 facts, 2026-09-10, taken with the server stopped) is the first pass after
`bench-retrieval` started printing its own recall-limit headroom. Same shape as before —
`hit_min`/`hit_max` unchanged, `mean_rank` 3.25 -> 3.42 — but the worst target rank is now 10
(q12, 6 -> 7 -> 9 -> 10 across the live passes), which is exactly `RECALL_LIMIT`. That is not
the headroom that matters: q12's target scores 0.379, under the shipped `T = 0.45`, so the
client drops it at any limit. Among the eight targets that clear 0.45 the worst rank is q1 at 8,
so the shipped pair has two positions of headroom — which is what `bench-retrieval`'s headroom
line reports, after its first version counted every target and warned on q12. The `limit = 10`
grid row at 927 matches the 880 grid on every hit count; only `noise_per_q` moved, by at most
0.2. Rows 10, 15 and 20 are identical at `T = 0.45`, so nothing above the threshold sits past
rank 10 and a wider limit would buy nothing today.

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
to what `retrieve_memories(limit=N)` returned. The sweep simulates that filter: for each
candidate threshold, how many of the 12 targets survive, how many of those the limit would
have delivered anyway, and how many non-targets ride along.

> **These two tables hold `limit` at 5**, the value shipped when they were measured. The
> default is now `10` and the shipped threshold is `0.45`, decided in [Result
> limit](#result-limit) below — the reasoning in this subsection is the argument as it stood
> at `limit = 5`, kept because it is what the grid had to overturn. Do not read a
> recommendation out of it; the bolded `0.35` rows mark the then-default, not the current one.

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

**`0.40` is strictly dominated on both corpora in this pass** — the same `hits_delivered` as
`0.45` at roughly double the noise. That held for the original 12 questions only: with the
recent targets in, `0.40` buys two more hits on the live corpus and becomes a trade rather than
a dominated cell — see [Grid with the recent targets in](#grid-with-the-recent-targets-in-965-facts).

**At `limit = 5` the remaining choice was `0.35` against `0.50`:** 9 hits at 3.00 injected
non-targets, or 7 hits at 0.33. Nine times the injection for two more hits out of twelve.
`0.35` was taken, because `noise_per_q` is an upper bound in a way `hits_delivered` is not —
see the limits below — and because a memory that never surfaces is the failure auto-recall
exists to prevent, while an extra adjacent memory costs a few hundred prompt tokens. That
choice was superseded once the limit was measured: it is a bad exchange rate, and the grid
below finds a better one rather than picking a side of it. Note what it forced — with the
limit fixed at 5, `0.40` and `0.45` both delivered 7, the same as `0.50` at more noise, so the
whole middle of the range was dominated and the decision really was `0.35`-or-`0.50`.

**`0.30` shows the threshold is not always the binding constraint.** Even admitting every
target by score, the live row delivers 10/12: two targets rank 8th and 7th, outside
`limit=5`, so no threshold reaches them. For those questions the lever is
`ALEXANDRIA_AUTO_RECALL_LIMIT` — measured in the next section.

Limits on how far to read this table:

- Twelve questions. A six-row table computed off twelve samples reads far more precise than
  it is, and single-hit differences between adjacent rows are noise.
- `noise_per_q` counts every non-target, so it is an **upper bound on useless injection**: a
  memory that is not the designated target may still be exactly what the prompt needed. The
  true cost of a low threshold is therefore somewhere below these numbers, by an unmeasured
  amount. `hits_delivered` has no such slack — a dropped target is a real miss.
- Every target predates 2026-09-08 16:26 UTC, so the 664 facts added since can only ever be
  noise here. That inflates `noise_per_q` and cannot deflate it.

### Result limit

The threshold tables above are one row of a grid: they hold `limit` at 5 — the then-default —
and vary `T`. Both levers gate the same delivery, so neither is readable alone. This pass (880
facts, a superset of the 807 above — the threshold numbers here are the same measurement at a
larger corpus, not a revision of it) sweeps both. Cells are `hits_delivered` out of 12, with
`noise_per_q` in parentheses. **`limit = 10, T = 0.45` is the pair now shipped**, set in both
clients on 2026-09-09 off this table.

Live corpus, 880 facts:

| limit | T=0.30 | T=0.35 | T=0.40 | T=0.45 | T=0.50 | T=0.58 |
|---|---|---|---|---|---|---|
| 3 | 8 (2.3) | 8 (2.1) | 7 (1.2) | 7 (0.7) | 7 (0.4) | 4 (0.2) |
| 5 (was) | 8 (4.2) | 8 (3.2) | 7 (1.7) | 7 (0.8) | 7 (0.5) | 4 (0.2) |
| 8 | 11 (5.8) | 10 (4.2) | 8 (2.0) | 8 (1.0) | 7 (0.5) | 4 (0.2) |
| **10** (shipped) | 12 (6.7) | 11 (4.7) | 8 (2.2) | **8 (1.0)** | 7 (0.5) | 4 (0.2) |
| 15 | 12 (8.9) | 11 (5.8) | 8 (2.6) | 8 (1.0) | 7 (0.5) | 4 (0.2) |
| 20 | 12 (10.5) | 11 (6.6) | 8 (2.7) | 8 (1.0) | 7 (0.5) | 4 (0.2) |

Baseline corpus, 143 facts:

| limit | T=0.30 | T=0.35 | T=0.40 | T=0.45 | T=0.50 | T=0.58 |
|---|---|---|---|---|---|---|
| 3 | 11 (1.6) | 10 (0.9) | 7 (0.7) | 7 (0.3) | 7 (0.0) | 4 (0.0) |
| 5 (was) | 12 (2.3) | 11 (1.4) | 8 (0.8) | 8 (0.3) | 7 (0.0) | 4 (0.0) |
| 8 | 12 (2.8) | 11 (1.8) | 8 (0.9) | 8 (0.3) | 7 (0.0) | 4 (0.0) |
| **10** (shipped) | 12 (3.2) | 11 (2.0) | 8 (0.9) | **8 (0.3)** | 7 (0.0) | 4 (0.0) |
| 15 | 12 (3.6) | 11 (2.0) | 8 (0.9) | 8 (0.3) | 7 (0.0) | 4 (0.0) |
| 20 | 12 (3.6) | 11 (2.0) | 8 (0.9) | 8 (0.3) | 7 (0.0) | 4 (0.0) |

**The previous pair was strictly dominated, which is why it changed.** From `limit=5, T=0.35`
(8 delivered, 3.2 noise), `limit=10, T=0.45` delivers the same 8 at 1.0 — a third of the
injection for identical recall. There was no trade to weigh in that move: the old setting was
simply off the frontier, so taking it needed no view on how recall and noise should be priced.
That is the whole reason this pair was picked over `limit=10, T=0.35` (11 delivered at 4.7),
which is a genuine trade and would have needed one.

**The limit is the stronger lever, and by a wide margin.** From the same starting cell,
lowering `T` to 0.30 buys **zero** hits for +1.05 noise, because the targets it admits by score
are the ones rank is hiding. Raising the limit to 10 buys **three** hits for +1.5 noise. The
threshold discussion above settled `0.35` against `0.50` as "nine times the injection for two
more hits"; the limit's exchange rate is better than that by an order of magnitude.

**Delivery saturates at `limit=10`, and the mechanism is visible.** The worst target rank in
this pass is 9 (`how fast is memory lookup supposed to be`), with 8 and 7 behind it — so a
window of 10 contains every target, and `hits_delivered` reaches `hits_kept` in every column.
Rows 15 and 20 are pure cost: +1.9 noise at `T=0.35` for no additional hit. This is not a
property of the model, it is the rank distribution of *this* corpus, so it moves as the corpus
grows — which is exactly how rank inflation reaches a user.

**The baseline corpus could not have shown any of this.** At 143 facts the worst rank is 4, so
`limit=5` already saturates and every row below it is flat. The limit only became the binding
constraint as the corpus grew 143 -> 880; measuring it on the original install would have
returned "5 is fine" correctly and uselessly.

Limits on how far to read the grid, beyond the three that apply to the threshold tables:

- `noise_per_q` is the only column that keeps rising past saturation, and it is an upper bound
  (a non-target can still be the memory the prompt needed). So the real cost of `limit=10` over
  `limit=5` is somewhere below +1.5 memories per prompt, by an unmeasured amount — while the +3
  hits have no such slack.
- The `limit=3` row is not a recommendation without a second check: `activation.top_n` defaults
  to 3 and fires on an already-limit-truncated list, so at `limit=3` the two couple and below it
  spreading activation silently narrows. Every row at 3 or above leaves activation untouched.
- Nothing here says what happens between 10 and 15, or whether 10 still saturates at 2000 facts.
  The grid is six points chosen to bracket the shipped value, not a curve.

### Recent targets (2026-09-10, 20 questions)

Every target above predates the baseline window, so those rows measure fixed questions
against a growing haystack and never check that a *recent* memory can be found. On 2026-09-10
eight questions were added (13–20 in [Test data](#test-data)), each targeting a fact stored on
2026-09-09 from another project — restic/S3, oatbar, jj, clap — so that they do not sit in
the cluster of memories about Alexandria itself that crowds q12. The live row is now over 20
questions and **does not stack on the table above**; this pass starts a new one. The baseline
row still scores only the original 12 (the new targets are absent from that corpus) and is
the comparability check as before.

| corpus | facts | scored | mean_rank | top1 | mean_gap | hit_min | hit_max | nonhit_p50 | nonhit_p90 | nonhit_p99 | ff_p50 | ff_p90 | ff_p99 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (143 oldest active) | 143 | 12/20 | 1.42 | 9/12 | +0.148 | 0.338 | 0.667 | 0.078 | 0.212 | 0.372 | 0.130 | 0.298 | 0.560 |
| live | 957 | 20/20 | 2.75 | 12/20 | +0.082 | 0.338 | 0.828 | 0.079 | 0.197 | 0.340 | 0.129 | 0.286 | 0.497 |

The baseline row reproduces the 2026-09-09 row on every column with the eight absent
questions skipped, so the absent-target path changes nothing for the questions it does score.

Per-question rank, live. Against the per-question table above (743 facts) the originals moved
on three questions — q4 5 -> 7, q11 5 -> 7, q12 6 -> 11 — and are unchanged on the rest:

| # | question | live |
|---|---|---|
| 13 | pkill -f with an anchored path pattern does not find my process even though it is running | 1 |
| 14 | jj squash prints an error about the editor failing to initialize, did the squash happen | 1 |
| 15 | passing zero to a seconds flag pegs a core, how should I constrain the argument | 2 |
| 16 | a tiny text change produced a huge diff in the rendered svg, why | 1 |
| 17 | reading the link speed file under sys class net gives -1 or an error for some interfaces | 2 |
| 18 | what is the config key for pango markup on a text block | 1 |
| 19 | would turning on object lock for the backup bucket break restic | 1 |
| 20 | the lifecycle rule has been on for a day and nothing expired yet, is it broken | 4 |

**Recent memories are found, and more easily than the old ones.** Five of the eight rank 1
and the worst is 4, against a worst of 11 among the originals. That is the expected shape,
not evidence the model improved: a fact stored yesterday has had one day to accumulate
neighbours, where the originals have had two days and a 6x corpus. `hit_max` rose from 0.667
to 0.828, so the strongest hit in the set is now one of the new targets. Six of the eight
clear the shipped `T = 0.45` and all six are delivered at `limit = 10`; the other two score
between 0.40 and 0.45 (they appear in the 0.40 row's `hits_kept` and not the 0.45 row's).

**q12 has crossed the limit.** Its target now ranks 11, past `RECALL_LIMIT = 10`, but it
scores under `T = 0.45`, so the client would have dropped it anyway and the headroom line is
unchanged: worst rank 8 (q1) among the 14 targets that clear the threshold, two positions
left. It is the only target past rank 10, which is why the `limit = 15` row picks up exactly
one more hit than `limit = 10` at `T = 0.30` and `0.35` (its score is 0.379) and none above.

Live corpus, 957 facts, `limit = 10`:

| T | hits_kept | hits_delivered | noise_per_q |
|---|---|---|---|
| 0.30 | 20/20 | 19/20 | 7.20 |
| 0.35 | 19/20 | 18/20 | 5.45 |
| 0.40 | 16/20 | 16/20 | 2.95 |
| **0.45** | 14/20 | **14/20** | 1.65 |
| 0.50 | 13/20 | 13/20 | 0.70 |
| 0.58 | 9/20 | 9/20 | 0.15 |

`noise_per_q` at `0.45` is 1.65 against 1.0 in the 880-fact grid. Part of that is the corpus
(957 vs 880) and part is that the new questions, being about other projects' memories, sit
in denser neighbourhoods of the corpus than the Alexandria-specific originals; the two are
not separable from one pass.

### Grid with the recent targets in (965 facts)

The 20-question threshold table above is the `limit = 10` row of a grid, read the same way
as the 880-fact one. Re-run 2026-09-10 on 965 facts (eight added since the 957 pass; every
rank and every column reproduces except `nonhit_p99` 0.340 -> 0.339 and `ff_p90` 0.286 ->
0.287). The baseline grid is identical to the 143-fact one recorded under [Result
limit](#result-limit) and is not repeated. Cells are `hits_delivered` out of 20 with
`noise_per_q` in parentheses.

Live corpus, 965 facts:

| limit | T=0.30 | T=0.35 | T=0.40 | T=0.45 | T=0.50 | T=0.58 |
|---|---|---|---|---|---|---|
| 3 | 15 (2.2) | 15 (2.0) | 14 (1.5) | 13 (1.1) | 13 (0.6) | 9 (0.1) |
| 5 | 16 (4.1) | 16 (3.4) | 15 (2.2) | 13 (1.4) | 13 (0.7) | 9 (0.1) |
| 8 | 19 (6.0) | 18 (4.8) | 16 (2.8) | 14 (1.6) | 13 (0.7) | 9 (0.1) |
| **10** (shipped) | 19 (7.2) | 18 (5.5) | 16 (3.0) | **14 (1.6)** | 13 (0.7) | 9 (0.1) |
| 15 | 20 (9.8) | 19 (6.5) | 16 (3.2) | 14 (1.6) | 13 (0.7) | 9 (0.1) |
| 20 | 20 (11.7) | 19 (7.2) | 16 (3.2) | 14 (1.6) | 13 (0.7) | 9 (0.1) |

**The shipped pair stays, by the rule that picked it.** The pair moves only when another cell
strictly dominates it — at least as many hits at no more noise — on both corpora. No cell does.
`limit=8, T=0.45` ties it exactly: the 14 targets that clear 0.45 all rank 8 or better, so the
two cells deliver the same set. `limit=3, T=0.40` delivers 14 at 1.5 against 1.65, but it is a
different 14 — it trades q1 (rank 8, 0.47) for q15 (rank 2, 0.41) — it is 7 (0.7) against
8 (0.3) on the baseline, and the `limit=3` caveat above (coupling with `activation.top_n`)
still applies.

**`0.40` is no longer dominated, and the docs that said so were read off the 12-question
grid.** At `limit=10` it delivers 16 at 2.95 against `0.45`'s 14 at 1.65: the two targets it
adds are q15 (0.41) and q20 (0.43), and the price is +1.3 non-targets per prompt. That is a
genuine trade in the same shape as the limit's — +2 hits for +1.3 noise, against the limit's
+3 for +1.5 — and the dominance rule does not take trades. On the baseline `0.40` is still
dominated (8 at 0.9 against 8 at 0.3). Anyone pricing recall above injection should read
this cell, not the 12-question claim.

**Saturation still holds at 10 for what the threshold admits.** Every column is flat from
`limit=8` to `10`; only `T=0.30` and `0.35` pick up a hit at 15, and it is q12 (rank 11,
0.38), which the shipped threshold drops anyway. The headroom line is unchanged: worst rank 8
(q1) among the 14 targets at or above 0.45, two positions left.

## Metric definitions

- **rank** — position of the target fact when the whole corpus is sorted by cosine descending
  against the question, 1-based. Computed as `1 + count(facts scoring above the target)`.
- **top1** — questions whose target ranked 1.
- **mean_gap** — mean over questions of (target score − best non-target score). Negative on a
  miss, so a corpus that crowds the target drags it toward zero.
- **hit_min / hit_max** — min and max of the scored targets' scores. Independent of corpus size.
- **nonhit_pN** — percentiles over every question-to-non-target score (scored questions × corpus).
- **ff_pN** — percentiles over every fact-to-fact pair.
- **hits_kept** — targets scoring at or above the client threshold, ignoring rank.
- **hits_delivered** — targets that clear the threshold *and* rank within the row's `limit`, so
  a client would actually be shown them. The threshold tables hold this at `RECALL_LIMIT` (5,
  the auto-recall hook's default `limit`); the grid varies it. The honest recall number;
  `hits_kept` alone only restates whether the threshold sits below a target's score, and is
  therefore the ceiling every `limit` column converges on.
- **noise_per_q** — mean non-targets per question that survive both the limit and the
  threshold. The server floor is not modelled: every swept threshold is far above it.

Percentiles use linear interpolation, matching `numpy.percentile`'s default. The 2026-09-08
second pass ran through numpy; nearest-rank here would shift the derived floor by a hundredth
and silently break comparability with the recorded tables.

## Test data

Question → the fact that answers it; the list is `QUESTIONS` in `crates/alexandria/src/bench.rs`. Questions
1–12 are frozen from the 2026-09-08 run and every target is inside the baseline window.
Questions 13–20 were added 2026-09-10 and target facts stored 2026-09-09 from other projects;
they are absent from the baseline corpus and print as `absent` on that row.

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
13. "pkill -f with an anchored path pattern does not find my process even though it is running" -> `fact:s54d27iol1v5cvr3dfeh`
14. "jj squash prints an error about the editor failing to initialize, did the squash happen" -> `fact:8x2na75v2uyuvtj2y2d8`
15. "passing zero to a seconds flag pegs a core, how should I constrain the argument" -> `fact:m27pl2qci3h2idbz4ku7`
16. "a tiny text change produced a huge diff in the rendered svg, why" -> `fact:ij7g7c4gql4byjq0wsf3`
17. "reading the link speed file under sys class net gives -1 or an error for some interfaces" -> `fact:o3z29nj29curn65rn0e3`
18. "what is the config key for pango markup on a text block" -> `fact:46x5bi68675hryxec20y`
19. "would turning on object lock for the backup bucket break restic" -> `fact:61zqxqcijsi7n847632x`
20. "the lifecycle rule has been on for a day and nothing expired yet, is it broken" -> `fact:i2tf44mnoigxqro4898n`

## Limitations

- **The recent targets are one day old.** Questions 13–20 check that a memory stored after
  the baseline window can be found, but all eight targets date from a single day, and their
  ranks will inflate the way the originals' did as the corpus grows around them. Read them as
  the near-term signal, not as a different model.
- **The baseline is reconstructed by size, not identity.** `BASELINE_SIZE = 143` takes the
  143 oldest *active* facts, which is not the same set that was active on 2026-09-08: any of
  those deleted since drops out and the window reaches forward to replace it. Drift is
  currently negligible — the row reproduces exactly — but it grows with every deletion and
  the tool cannot detect it.
- **A timestamp cutoff cannot be substituted for the size-based baseline.** No active fact
  predates 2026-09-08 12:30 UTC. The `08:08` in the measurements doc is local time (UTC-4)
  and is when the data dir was created, not when the measurement ran, so a cutoff built from
  it selects nothing.
- **20 questions is a small sample** (12 on the baseline row). A single question changing rank
  moves `mean_rank` by up to a twentieth of the change. Treat differences of a tenth as noise.
