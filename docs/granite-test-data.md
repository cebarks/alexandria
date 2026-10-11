# Granite evaluation and release gates

**Status: opt-in implementation, not a calibrated default-model release.** MiniLM remains the
installation default. Artifact publication, tokenizer licensing review, representative cutoff
calibration and operator approval are separate gates. Neither model selection nor migration
changes an explicitly configured similarity threshold.

## What this pilot measures

[`retrieval-pilot.json`](../crates/alexandria-pipeline/tests/fixtures/retrieval-pilot.json) is a
complete, public synthetic fixture: eight memory-style passages, eight Rust snippets, and eight
queries per group. Each group has four answerable queries and four near-miss unanswerables.
Answerable queries enumerate every relevant passage in that group; several have multiple
relevant passages. Labels were author-reviewed against the complete fixture before evaluation.
They are **not independently adjudicated production ground truth**.

Unanswerables request a specific identifier, measurement or implementation absent from the
fixture. Shared vocabulary is intentional: a semantically related passage need not answer the
question. Candidates are restricted to the query's memory/code group, not mixed with a larger
unlabelled corpus.

The native evaluation uses the actual shared provider factory: legacy Candle MiniLM at its
256-token default, or checksum-pinned Burn Granite at 8192 tokens. Inference is Rust only,
serial and one text per call. Python may prepare fixtures or analyze saved vectors; it does not
perform inference. Private corpus snapshots, vectors and failure evidence stay local/ignored.

### Metrics and cutoff order

For each query, rank all group documents by cosine similarity, truncate to the requested limit,
then apply the server floor and client floor inclusively. This models the server's **post-limit**
noise cutoff, followed by client filtering; it is not a claim about HNSW candidate recall or
memory side effects. The pilot does not evaluate two-phase cluster recall.

- **Relevant recall:** returned relevant passages / all relevant passages, macro-averaged over
  answerable queries. Finding one of two valid answers is not full recall.
- **Micro precision:** relevant returned passages / all returned passages, including results
  returned for unanswerable queries. An empty result set has undefined precision, not 100%.
- **Unanswerable false acceptance:** number of unanswerable queries returning any passage.
- **Answerable abstention:** number of answerable queries returning nothing.
- **MRR:** reciprocal rank of the first relevant result, averaged over answerable queries.

Exact ties at a truncated boundary produce pessimistic/optimistic bounds for relevance and
reciprocal rank. Input order or an arbitrary ID tie-break must not manufacture a unique top-one
success. Scores below use FP64 accumulation after renormalizing the saved FP32 vectors; the
production engine accumulates cosine in FP32, so last-bit boundary differences are possible.

## Synthetic pilot observations

CPU, ROCm and Vulkan Granite runs gave the same ranking and cutoff outcomes in this fixture.
This is a limited check, not proof of universal cross-device rank equality.

All rows below use **server floor 0.10 and requested limit 3**. Client floors are experimental
comparison points, **not recommended defaults**. Recall counts all relevant passages; each
unanswerable denominator is only four queries.

| Group / model | Client floor | Relevant recall | Micro precision | Unanswerable false accepts | Answerable abstentions |
| --- | ---: | ---: | ---: | ---: | ---: |
| Memory / MiniLM | 0.50 | 0.500 | 1.000 | 0/4 | 1/4 |
| Memory / Granite | 0.50 | 1.000 | 0.333 | 4/4 | 0/4 |
| Memory / Granite | 0.87 | 0.625 | 1.000 | 0/4 | 1/4 |
| Code / MiniLM | 0.50 | 0.625 | 0.800 | 0/4 | 1/4 |
| Code / Granite | 0.50 | 1.000 | 0.250 | 4/4 | 0/4 |
| Code / Granite | 0.80 | 1.000 | 0.545 | 0/4 | 0/4 |
| Code / Granite | 0.87 | 0.750 | 0.833 | 0/4 | 1/4 |

Granite's score scale is materially different here. Its highest relevant memory scores span
approximately 0.867–0.928, while the maximum scores for unanswerable memory queries span
0.833–0.869. Those bands **overlap**: no single floor can preserve every answerable memory
query while rejecting every unanswerable in even this small fixture. Code's corresponding
bands are approximately 0.855–0.903 and 0.727–0.787, but eliminating unanswerable results still
leaves some irrelevant passages in answerable result windows.

**Do not carry MiniLM's cutoff calibration over to Granite.** Better labelled ranks do not imply
better abstention at the same numeric threshold. Raising a floor can trade false acceptance for
lost valid results; an answerability/reranking mechanism would be a separate design decision,
not something this migration silently introduces.

The score-scale caveat also applies to cluster join/merge/cohesion thresholds: the old model's
absolute cosine settings are not established Granite settings. Preserving existing memberships
during migration does not calibrate future clustering or background maintenance.

The recommendation from this pilot is to **keep the current defaults unchanged** and obtain a
larger, independently reviewed, representative relevance/unanswerable set before proposing
paired cutoff changes. With only four negatives, even observing zero false accepts has a
one-sided 95% binomial upper bound of about 53%; it is not a production safety claim. The
synthetic 50% unanswerable mix also differs from an unknown deployment base rate.

## Reproduce native vectors without a database

The ignored integration acceptance test reads a strict JSON roster and creates a new output
file exclusively (0600 on Unix). Existing success or failure files are never overwritten. A
completed output carries input SHA256, actual embedding space/device, per-row vectors and
elapsed embedding time, and a final count. Missing final records are failed evidence, not a
successful partial benchmark. Each vector is checked for cardinality, dimensions, finiteness
and unit norm without repair.

Prepare an input outside tracked files:

```sh
python3 - <<'PY'
import json
from pathlib import Path
p = json.loads(Path('crates/alexandria-pipeline/tests/fixtures/retrieval-pilot.json').read_text())
with Path('/tmp/granite-pilot-input.json').open('x') as f:
    json.dump({'format_version': 1,
               'samples': [{'id': r['id'], 'text': r['text']}
                           for r in p['documents'] + p['queries']]}, f)
PY
```

With a verified local artifact directory (licensing/release caveats still apply):

```sh
ALEXANDRIA_EVAL_INPUT=/tmp/granite-pilot-input.json \
ALEXANDRIA_EVAL_OUTPUT=/tmp/granite-pilot-cpu-vectors.jsonl \
ALEXANDRIA_EVAL_MODEL=ibm-granite/granite-embedding-311m-multilingual-r2 \
ALEXANDRIA_GRANITE_TEST_DEVICE=cpu \
ALEXANDRIA_GRANITE_TEST_ARTIFACT_DIR=/path/to/verified-artifacts \
RAYON_NUM_THREADS=8 \
cargo test --locked --workspace --test fixture_evaluation_test \
  native_fixture_evaluation -- --ignored --exact --nocapture
```

For GPU, build with the corresponding binary/workspace feature and select `rocm:0` or
`vulkan:0`; `--all-features` compiles both. A CPU default build cannot test a GPU by merely
changing the environment. Run one GPU encoder at a time, bound host RAM and deadlines, and
remember that a host-RAM limit is not a VRAM limit. An explicit GPU failure does not fall back.
For MiniLM, select `sentence-transformers/all-MiniLM-L6-v2` and `cpu`; pre-populate/verify its
cache if downloads are unwanted. The optional `ALEXANDRIA_EVAL_MAX_TOKENS` changes the
recorded space and must be reported with comparisons. Ordinary CI skips native model runs.

Score completed evidence using the dependency-free Python tool (no inference):

```sh
python3 tools/granite-evaluation/score_pilot.py \
  --input /tmp/granite-pilot-input.json \
  --vectors /tmp/granite-pilot-cpu-vectors.jsonl \
  --labels crates/alexandria-pipeline/tests/fixtures/retrieval-pilot.json \
  --output /tmp/granite-pilot-comparison.json \
  --limit 3 --server-floor 0.10 --client-floor 0.87
python3 -m unittest discover -s tools/granite-evaluation
```

All numeric settings are explicit. The scorer checks the native input SHA256, full roster and
labelled text identity before scoring; incomplete native outputs cannot become a successful
report. Output creation is exclusive and private on Unix. Use a new filename for each paired
floor/limit point. These tools do not edit production configuration or assert release approval.

Evaluation timings are optimized-debug integration-test measurements: initialization includes
artifact verification and readiness, and per-row embedding time includes the worker path.
They are not release-build throughput, isolated forward-pass timings, or end-to-end MCP
latency. Cold shapes, persistent caches, feature sets and concurrent desktop load matter.

## Hardware acceptance limits

The integrated provider passed 8192-token CPU and ROCm checks on the Ryzen 7950X3D / RX 7900 XTX
workstation, including empty, whitespace and Unicode inputs. This is hardware-specific
acceptance, not a portable resource bound or a long-document retrieval-quality measurement.

**Vulkan 8192 is not accepted on the tested RADV stack.** The same long input failed requesting
a 3,221,225,472-byte buffer. A read-only Burn/CubeCL device probe reported a maximum page size
of 2,147,483,644 bytes. Raw Vulkan's larger storage-range/VRAM figures do not remove the WGPU
binding-size limit used by this backend. The frozen shorter corpus passed Vulkan, but that does
not establish maximum-budget capacity. Failure evidence is retained; there is no silent CPU
fallback, budget reduction or modified attention graph. A different Vulkan implementation or
attention strategy requires separate investigation and approval. Use the tested CPU/ROCm path
for the 8K acceptance claim, not a generic assertion that every compiled GPU supports 8K.

## Release checklist

- Representative independently reviewed memory/code relevance sets, with multiple valid
  answers and adjudicated near-miss unanswerables; multilingual/TypeScript/graph coverage is
  not established by this fixture.
- Owner approval of paired model-specific server/client cutoffs, judged together with limits.
- Tokenizer licensing review: IBM weights are Apache-2.0; the Gemma3-derived tokenizer also
  carries [Gemma Terms](https://ai.google.dev/gemma/terms). Do not label the entire bundle
  Apache-only or claim AGPL incompatibility has been settled.
- Approved checksum/provenance/notice package and artifact publication. The compiled URLs
  remain null until real approved assets exist; a semantic-equivalent regenerated pack is not
  necessarily byte-identical because importer parameter IDs are randomized.
- CPU/GPU acceptance with exact pinned artifacts and declared resource limits; no inference
  or official model conversion during ordinary Cargo builds or hermetic CI.
- Manual verified target cutover and paired original-model/directory rollback, each separately
  authorized. No automatic deployment, path swap, source deletion or cutoff retuning.
