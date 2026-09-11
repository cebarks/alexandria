# Self-Organizing Memory — Design

## Purpose

Memories should organize themselves without manual curation. Today two parts of the data model
exist but are never populated in production:

- `memory_edge` rows of type `relates_to` / `supports` / `contradicts` are only created in tests.
  Production writes `derived_from` (update snapshots) and `extracted_from` (import chunks). Spreading
  activation therefore only propagates along lineage chains.
- `cluster.label` exists since `v001`, every production cluster is created with `None`, and `recall`
  hardcodes `label: None`. Broad recall returns `cluster:abc123`, which an agent cannot act on.

This milestone adds a server-side LLM pass that fills both in, runs in the background, and surfaces
contradictions in `retrieve_memories`.

## Decisions Already Made

- **LLM host:** the server calls an OpenAI-compatible `chat/completions` endpoint (OpenAI, Ollama,
  llama.cpp, etc.). Not MCP sampling (Claude Code doesn't support it, and background work has no
  peer), not a local candle instruct model, not the pi extension.
- **Scheduling:** the enrichment pass is appended to the existing cluster-maintenance tick in HTTP
  mode. No queue table, no priorities. State lives in columns.
- **Relation calls:** one LLM call per newly stored fact, with its top-k similar candidates batched
  into that one prompt. Not one call per pair, not many facts per call.
- **Contradictions:** create the edge and expose it in `retrieve_memories` results. No automatic
  demotion of either side.

## Scope

In:

- `LlmProvider` trait and `OpenAiCompatible` implementation in `alexandria-pipeline`.
- Pure candidate-selection, prompt-building, response-parsing, and staleness functions in
  `alexandria-engine`.
- Migration `v007` and the repo methods listed below.
- `[llm]` config section with env overrides.
- Enrichment pass in the maintenance task; the task moves from `crates/alexandria/src/main.rs` to
  `src/maintenance.rs`.
- `retrieve_memories` result field `contradicts`, `recall` reading the real label, `update_memory`
  resetting relation state on content change.
- Debug UI: cluster list already renders `label`; no new pages.
- Docs: README tool table (retrieve output), `docs/configuration.md` (`[llm]`), `AGENTS.md`
  (schema head, non-obvious patterns), roadmap.

Out:

- OpenAI embedding provider (separate bounded task; shares nothing but reqwest).
- Queue table, priorities, backpressure.
- Enrichment in stdio mode. Maintenance is HTTP-only today; this follows that rule.
- Relation discovery between two *old* facts. Only facts stored or updated after this ships are
  checked, plus anything with a null `relations_checked_at` (which the migration leaves null for
  existing rows, so the backlog drains at `max_calls_per_tick` per tick).
- Hierarchical cluster labels (`depth > 0` clusters are not created by anything today).

## Components

### `alexandria-pipeline::llm`

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, system: &str, user: &str) -> Result<String>;
    fn model_id(&self) -> &str;
}

pub struct OpenAiCompatible { base_url, api_key: Option<String>, model, client: reqwest::Client }
```

`complete` POSTs `{model, messages:[{system},{user}], temperature: 0}` to
`{base_url}/chat/completions`, sends `Authorization: Bearer` only when `api_key` is set (Ollama
needs none), and returns `choices[0].message.content`. Request timeout from config. No streaming, no
retries — the tick retries naturally.

The trait exists so tests can inject a scripted fake, not for provider variety.

### `alexandria-engine::enrich`

Pure, no I/O, no async.

```rust
pub struct Candidate { pub id: String, pub content: String, pub similarity: f32 }

/// Cosine-rank `others` against `fact_embedding`; keep the top `k` at or above `min_similarity`,
/// skipping ids in `exclude` (siblings from the same imported document and the fact itself).
pub fn select_candidates(fact_embedding, others: &[(id, content, embedding)], exclude: &HashSet<String>, k, min_similarity) -> Vec<Candidate>

pub fn relation_prompt(fact_content, candidates) -> (system: String, user: String)

pub enum Relation { RelatesTo, Supports, Contradicts }
pub struct RelationVerdict { pub candidate_index: usize, pub relation: Relation, pub strength: f64 }

/// Parses the model's JSON array. Unknown relation names and out-of-range indexes are dropped,
/// `none` is dropped, strength is clamped to [0, 1]. Returns Err only if the text is not a JSON
/// array at all (after stripping a ```json fence if present).
pub fn parse_relations(text, candidate_count) -> Result<Vec<RelationVerdict>>

pub fn label_prompt(member_contents: &[&str]) -> (String, String)
/// First line, trimmed, quotes stripped, truncated to 80 chars. Err if empty.
pub fn parse_label(text) -> Result<String>

/// No label, or |current - labeled| >= max(1, ceil(0.2 * labeled)).
pub fn label_is_stale(label: Option<&str>, labeled_member_count: Option<usize>, current: usize) -> bool
```

Relation prompt shape (user message):

```
NEW MEMORY:
<content>

EXISTING MEMORIES:
[0] <content>
[1] <content>
...

For each existing memory, decide how it relates to the new one. Reply with only a JSON array:
[{"index": 0, "relation": "relates_to" | "supports" | "contradicts" | "none", "strength": 0.0-1.0}]
```

The system message defines the three relations: `supports` = same claim or evidence for it,
`contradicts` = cannot both be true, `relates_to` = same topic but neither of the above, `none` =
unrelated despite surface similarity.

Label prompt: up to 20 member contents (highest heat first), "Reply with a 2–5 word topic label,
nothing else."

### `alexandria-storage`

Migration `v007_enrichment.surql`:

```sql
DEFINE FIELD relations_checked_at   ON fact    TYPE option<datetime>;
DEFINE FIELD labeled_member_count   ON cluster TYPE option<int>;
```

Both nullable so existing rows need no backfill. `Fact` and `Cluster` models gain the matching
`Option` fields.

New repo methods:

- `MemoryRepo::unchecked_facts(limit) -> Vec<Fact>` — `deleted = false AND relations_checked_at IS
  NONE`, oldest first.
- `MemoryRepo::mark_relations_checked(id)` — sets `time::now()`.
- `MemoryRepo::clear_relations_checked(id)` — sets `NONE` (used by `update_memory`).
- `MemoryRepo::raw_sources() -> HashMap<fact_id, raw_id>` — one query over all `extracted_from`
  edges, for sibling exclusion. Fine at current scale; scope it to the unchecked ids' sources if
  imports grow large.
- `EdgeRepo::delete_relation_edges(id)` — deletes `memory_edge` rows touching `id` whose
  `edge_type` is one of the three relation types. Lineage edges stay.
- `EdgeRepo::contradictions_for(ids) -> HashMap<fact_id, Vec<fact_id>>` — one query, both
  directions, `edge_type = 'contradicts'`, excluding soft-deleted counterparts.
- `ClusterRepo::set_label(id, label, member_count)`.

`ClusterRepo::list_with_counts()` already returns `(Cluster, usize)`; the enrichment pass reuses it.

### `alexandria` binary

Config:

```toml
[llm]
# Unset = enrichment disabled. Example: "http://localhost:11434/v1" for Ollama.
base_url = ""
api_key = ""                     # optional; ALEXANDRIA_LLM_API_KEY
model = "gpt-4o-mini"
timeout_secs = 30
max_calls_per_tick = 20          # relation calls + label calls, combined
relation_candidates = 5          # k
relation_min_similarity = 0.5    # candidate floor; passage-vs-passage cosine on the current model, tune against the debug query tester
```

Env overrides: `ALEXANDRIA_LLM_BASE_URL`, `ALEXANDRIA_LLM_API_KEY`, `ALEXANDRIA_LLM_MODEL`.
`api_key` is never logged.

`src/maintenance.rs` receives the existing split/merge loop verbatim, plus `enrich_pass` called
after it each tick when an `Arc<dyn LlmProvider>` is present.

### `alexandria-mcp`

- `retrieve_memories`: after ranking and the similarity floor, one `contradictions_for` call over
  the result ids; each result gains `"contradicts": ["fact:…", …]` (empty array when none). Tool
  description gains a sentence telling the agent to reconcile listed contradictions before relying
  on the memory.
- `recall`: `ClusterMatch.label` is populated from `cluster.label`.
- `update_memory` with a content change: after the snapshot and `derived_from` edge are written,
  call `delete_relation_edges(id)` and `clear_relations_checked(id)`.
- `AlexandriaServer` gains nothing LLM-related. The enrichment pass lives in the binary and talks
  to repos directly, like split/merge does.

## Data Flow: One Enrichment Tick

Budget `remaining = max_calls_per_tick`.

**Relations**

1. `unchecked_facts(remaining)`.
2. If any: load all live facts once (`SELECT id, content, embedding FROM fact WHERE deleted =
   false`) and `raw_sources()` (one query each).
3. For each unchecked fact:
   - `exclude` = itself ∪ facts sharing its raw source.
   - `select_candidates(k, relation_min_similarity)`. If empty: `mark_relations_checked`, no LLM
     call, continue.
   - `complete(relation_prompt)`. On transport error: log warn, stop the relations phase for this
     tick (the endpoint is down; don't burn the budget). On success: `parse_relations`. On parse
     error: log warn with the first 200 chars of the reply, then mark checked anyway (a model that
     returns garbage for this input will do so again; don't loop forever).
   - Create one `memory_edge` per verdict, `new_fact -> candidate`, with the verdict's type and
     strength. Skip if an edge of that type already exists between the pair in either direction
     (`create_edge` is not idempotent today; the check is one query on the pair).
   - `mark_relations_checked`. `remaining -= 1`.

**Labels**

4. `list_with_counts()`. For each cluster where `label_is_stale`, while `remaining > 0`:
   - `get_members`, sort by heat descending, take 20 contents.
   - `complete(label_prompt)`. Transport error: log, stop the labels phase. Parse error: log, skip
     this cluster (it will be retried next tick; there is no per-cluster failure counter, and the
     `max_calls_per_tick` cap bounds the cost).
   - `set_label(id, label, current_count)`. `remaining -= 1`.

**Ordering:** relations before labels, so a tick with a large backlog labels nothing until the
backlog drains. That is intended: edges are the more valuable output and labels re-derive cheaply.

**Logging:** one `info!` per tick summarizing `facts_checked`, `edges_created`, `clusters_labeled`,
`calls_used`. Per-item logging at `debug!`. Failures at `warn!` with the fact or cluster id.

Nothing is written to `maintenance_log`; that table is the split/merge audit trail and its schema
(`action`, `source`, `targets`, `members_moved`) doesn't fit. Edges carry `created_at`, and labels are
inspectable in the cluster list, which is enough audit for now.

## Error Handling

- **LLM unconfigured:** no client built, `enrich_pass` never called, zero log noise.
- **LLM unreachable or 5xx:** the phase stops for this tick; state is untouched; next tick retries.
  A single `warn!` per tick, not per fact.
- **4xx (bad key, unknown model):** same as unreachable. The operator sees the warn with the status
  and the response body's first 200 chars.
- **Unparseable reply:** relation phase marks the fact checked and moves on; label phase skips the
  cluster. Both log the truncated reply.
- **Server shutdown mid-tick:** each fact's edges and stamp are separate writes, so at worst one
  fact has some edges but no stamp and gets re-checked next start. The duplicate-edge check makes
  that harmless.
- **Concurrent split/merge:** the enrichment pass runs in the same task after split/merge, so no
  overlap within a tick. `store_memory` can add members during the pass; the label was computed on a
  snapshot and `labeled_member_count` records that snapshot's size, so the next tick's staleness
  check is still correct.

## Testing

Engine (unit, no I/O):

- `select_candidates`: respects k, floor, exclusion set, ordering.
- `parse_relations`: valid array, fenced array, `none` dropped, unknown relation dropped, bad index
  dropped, strength clamped, non-JSON → Err.
- `parse_label`: trims, strips quotes, truncates, empty → Err.
- `label_is_stale`: null label, null count, exactly-20%, below-20%, small clusters (1 → 2 is stale).

Storage (integration, in-memory DB):

- `v007` applies on a fresh DB and on a DB migrated through `v006`; existing rows read back with
  `None` in the new fields.
- `unchecked_facts` excludes stamped and deleted facts, oldest first.
- `delete_relation_edges` removes the three relation types and keeps `derived_from`.
- `contradictions_for` finds both directions and drops deleted counterparts.
- `raw_sources` maps chunk → raw and omits non-imported facts.

Binary (integration, in-memory DB, `FakeLlm` returning scripted replies):

- A tick with two similar facts and a fake returning `contradicts` creates the edge, stamps both,
  and `retrieve_memories` shows the id in `contradicts`.
- Same-document chunks are never sent as candidates for each other.
- A cluster with no label gets one; adding one member to a five-member labeled cluster (20%) marks
  it stale; adding one to a ten-member cluster does not.
- Fake returning `Err` leaves stamps and labels untouched.
- Fake returning `"not json"` stamps the fact and creates no edges.
- `max_calls_per_tick = 1` with two unchecked facts checks exactly one.

MCP:

- `update_memory` content change clears the stamp and relation edges, keeps `derived_from`.
- `recall` returns the stored label.

Config:

- `[llm]` parses, env overrides win, unset `base_url` yields `None`. `#[serial]` on env tests.

No test calls a real LLM.

## Follow-ups (not this milestone)

- Relation discovery over the pre-existing corpus pairs (old × old), if the backlog drain via null
  stamps proves insufficient.
- Debug UI page for enrichment stats.
- Re-labeling on `update_memory` content change for the fact's cluster.
- Using `contradicts` edges to drive confidence, once there is data on how often the model is right.
