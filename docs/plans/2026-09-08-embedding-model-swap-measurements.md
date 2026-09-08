# Embedding model measurements (2026-09-08)

Corpus: 143 active facts from the live database. Questions: 12, listed below.

| model | mean_rank | top1 | mean_gap | hit_min | hit_max | nonhit_p50 | nonhit_p90 | nonhit_p99 | ff_p50 | ff_p90 | ff_p99 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| sentence-transformers/all-MiniLM-L6-v2 | 1.42 | 9/12 | +0.148 | 0.338 | 0.667 | 0.077 | 0.212 | 0.373 | 0.130 | 0.298 | 0.562 |
| sentence-transformers/msmarco-MiniLM-L6-cos-v5 | 12.58 | 6/12 | +0.000 | 0.119 | 0.583 | 0.173 | 0.295 | 0.400 | 0.230 | 0.375 | 0.561 |
| sentence-transformers/multi-qa-MiniLM-L6-cos-v1 | 2.33 | 8/12 | +0.091 | 0.318 | 0.635 | 0.082 | 0.215 | 0.375 | 0.125 | 0.290 | 0.568 |
| BAAI/bge-small-en-v1.5 | 5.42 | 7/12 | +0.037 | 0.620 | 0.799 | 0.564 | 0.628 | 0.709 | 0.592 | 0.670 | 0.778 |

## Chosen

| key | value |
|---|---|
| model | sentence-transformers/all-MiniLM-L6-v2 |
| dimensions | 384 |
| cluster.join_threshold | 0.75 |
| cluster.merge_threshold | 0.88 |
| cluster.cohesion_floor | 0.60 |
| retrieve.min_similarity | 0.36 |
| client auto-recall threshold | 0.22 |

Not applied: the incumbent won, so config defaults are unchanged. The retrieve
floor derived here (0.36) would cut a true hit at 0.338, and nonhit_p99 (0.373)
exceeds hit_min, so the spec's threshold rule has no valid solution on this
corpus. bge-small was measured without its query instruction prefix.

Reasoning: no candidate beat the incumbent on both criteria — MiniLM has the
lowest mean_rank (1.42 vs 2.33 / 5.42 / 12.58) and the largest mean_gap
(+0.148), so the decision rule keeps `all-MiniLM-L6-v2` and Task 6 becomes a
docs-only change. The surprise is bge-small: its absolute scores are
much higher across the board (hit_min 0.620) but so is its noise floor
(nonhit_p50 0.564, fact-fact p50 0.592), so the separation between a hit and
the rest of the corpus is *worse*, not better — high cosine values on a
CLS-pooled model are a compression of the range, not better retrieval.
msmarco-cos-v5 was the clear loser, badly missing two of the low-vocabulary-
overlap questions (rank 69 and rank 56). Note one quirk in the threshold
figures: the current `merge_threshold` of 0.90 sits above the maximum observed
fact-to-fact similarity (0.878) in this corpus, so the "same percentile"
mapping pins it to that maximum and the rule rounds it to 0.88 — the corpus
simply contains no pair similar enough to justify 0.90.

## Questions

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
