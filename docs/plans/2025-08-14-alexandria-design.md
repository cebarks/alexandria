# Alexandria: Agent Memory System — Design Document

**Date:** 2025-08-14
**Status:** Draft — core architecture decided, implementation details to be fleshed out

## Overview

Alexandria is a multi-user agent memory MCP server written in Rust, backed by SurrealDB 3.0. It provides persistent, evolving memory for AI agents across sessions, with a human-like progressive recall model.

### Key Design Decisions

| Decision | Choice |
| --- | --- |
| Language | Rust (tokio, async) |
| Database | SurrealDB 3.0 (embedded SurrealKV to start) |
| Protocol | MCP server |
| Memory tiers | raw → fact → consolidated → graph edges |
| Temperature | Ebbinghaus-inspired heat + stability, separate heat_state table |
| Heat storage | Dedicated heat_state table, on-demand columnar layout for SIMD |
| Recall model | Progressive scoping-in via cluster hierarchy + scope handles |
| Provenance | Structured records with agent/session/model context |
| Lineage | Full chain via SurrealDB relations across tiers |
| Multi-tenancy | SurrealDB namespace (org) → database (user) + shared db |
| Enrichment | Async pipeline: extract → embed → cluster → relate → consolidate |
| Document import | Direct-to-fact with chunk/whole modes |
| Deletes | Soft-delete only via MCP |
| MCP tools | store, retrieve, recall, update, delete, import_document |
| Cluster matching | Hybrid: centroid filter → member verification |
| Cluster splitting | Graph-weighted hierarchical agglomerative |
| Scope handles | Stateless signed tokens encoding cluster path |

---

## Architecture

Three-layer architecture:

```
┌─────────────────────────────────────┐
│         MCP Tool Interface          │  ← CRUD + semantic search + recall
├─────────────────────────────────────┤
│          Memory Engine              │  ← retrieval, ranking, scoping
├──────────┬──────────────────────────┤
│ SurrealDB│  Enrichment Pipeline     │  ← multi-model store + async workers
│ (embed)  │  (tokio background tasks)│
└──────────┴──────────────────────────┘
```

**Why SurrealDB:**

- **Raw tier** → SurrealDB documents (schema-flexible records)
- **Extracted facts** → structured records with vector embeddings for semantic search
- **Graph tier** → native graph edges (`->relates_to->`, `->derived_from->`) with SurrealQL traversal
- **Temperature** → record-level heat float with spreading activation through graph edges

**Deployment model:** Start embedded (SurrealKV) for single-node simplicity. The same code scales to client/server or distributed (TiKV) without application changes — just a connection string swap.

**Mitigating SurrealDB risks:** Pin a specific 3.x release, integration-test on every upgrade, and keep the storage interface behind a Rust trait so a Postgres fallback remains viable if SurrealDB hits a blocker.

---

## Data Model

Each tier has its own SurrealDB table with only the fields it needs. Graph edges connect across tables.

### Memory Tables

```surql
-- Tier 0: Raw session logs — minimal structure, no embedding
DEFINE TABLE raw SCHEMAFULL;
DEFINE FIELD content      ON raw TYPE string;
DEFINE FIELD provenance   ON raw TYPE record<provenance>;
DEFINE FIELD created_at   ON raw TYPE datetime DEFAULT time::now();
DEFINE FIELD metadata     ON raw TYPE option<object>;

-- Tier 1: Extracted facts — discrete claims with embeddings
DEFINE TABLE fact SCHEMAFULL;
DEFINE FIELD content      ON fact TYPE string;
DEFINE FIELD confidence   ON fact TYPE float DEFAULT 0.5;
DEFINE FIELD embedding    ON fact TYPE array<float>;
DEFINE FIELD tags         ON fact TYPE array<string> DEFAULT [];
DEFINE FIELD provenance   ON fact TYPE record<provenance>;
DEFINE FIELD created_at   ON fact TYPE datetime DEFAULT time::now();
DEFINE FIELD metadata     ON fact TYPE option<object>;

-- Tier 2: Consolidated — merged/deduped facts, higher confidence
DEFINE TABLE consolidated SCHEMAFULL;
DEFINE FIELD content      ON consolidated TYPE string;
DEFINE FIELD confidence   ON consolidated TYPE float;
DEFINE FIELD embedding    ON consolidated TYPE array<float>;
DEFINE FIELD tags         ON consolidated TYPE array<string> DEFAULT [];
DEFINE FIELD provenance   ON consolidated TYPE record<provenance>;
DEFINE FIELD created_at   ON consolidated TYPE datetime DEFAULT time::now();

-- Heat state — separate table, linked to any memory record
-- See "Heat & Temperature Model" section for full design
DEFINE TABLE heat_state SCHEMAFULL;
DEFINE FIELD memory       ON heat_state TYPE record;  -- fact | consolidated | raw
DEFINE FIELD heat         ON heat_state TYPE float DEFAULT 1.0;
DEFINE FIELD stability    ON heat_state TYPE float DEFAULT 1.0;
DEFINE FIELD last_touched ON heat_state TYPE datetime DEFAULT time::now();
DEFINE FIELD access_count ON heat_state TYPE int DEFAULT 0;
```

Heat and temperature fields are **not stored on memory records**. They live in the dedicated `heat_state` table for performance — see the Heat & Temperature Model section for rationale.

### Provenance

Rich source tracking — not just a type string, but full context about where a memory came from.

```surql
DEFINE TABLE provenance SCHEMAFULL;
DEFINE FIELD kind       ON provenance TYPE string;  -- "conversation" | "import" | "enrichment"
DEFINE FIELD agent_id   ON provenance TYPE option<string>;
DEFINE FIELD session_id ON provenance TYPE option<string>;
DEFINE FIELD user_id    ON provenance TYPE option<string>;
DEFINE FIELD model      ON provenance TYPE option<string>;  -- which LLM generated/extracted this
DEFINE FIELD timestamp  ON provenance TYPE datetime DEFAULT time::now();
DEFINE FIELD metadata   ON provenance TYPE option<object>;  -- extensible (import filename, etc.)
```

### Lineage Relations

Every higher-tier memory links back to what it was derived from, enabling full traceability: `consolidated:xyz -> merged_from -> fact:abc -> extracted_from -> raw:123`.

```surql
DEFINE TABLE extracted_from SCHEMAFULL TYPE RELATION IN fact OUT raw;
DEFINE TABLE merged_from    SCHEMAFULL TYPE RELATION IN consolidated OUT fact;
```

### Semantic Relations (Graph Tier)

```surql
DEFINE TABLE relates_to  SCHEMAFULL TYPE RELATION;
DEFINE TABLE contradicts SCHEMAFULL TYPE RELATION;
DEFINE TABLE supports    SCHEMAFULL TYPE RELATION;
DEFINE TABLE derived_from SCHEMAFULL TYPE RELATION;
```

### Clusters

Memories self-organize into hierarchical clusters based on embedding similarity. Clusters carry their own heat and serve as the foundation for progressive scoping-in recall.

```surql
DEFINE TABLE cluster SCHEMAFULL;
DEFINE FIELD label      ON cluster TYPE option<string>;  -- auto-generated summary label
DEFINE FIELD centroid   ON cluster TYPE array<float>;     -- average embedding of members
DEFINE FIELD depth      ON cluster TYPE int DEFAULT 0;    -- nesting level
DEFINE FIELD created_at ON cluster TYPE datetime DEFAULT time::now();
-- No heat field — cluster heat is derived from members at query time

-- Hierarchy: clusters contain sub-clusters
DEFINE TABLE contains_cluster SCHEMAFULL TYPE RELATION IN cluster OUT cluster;

-- Membership: clusters contain memories
DEFINE TABLE contains_memory SCHEMAFULL TYPE RELATION IN cluster OUT fact | consolidated;
```

---

## Recall Algorithm

The `recall` tool implements progressive scoping-in — multi-turn narrowing that mirrors human memory retrieval.

### Cluster Matching: Hybrid Centroid → Member Verification

Two-phase matching balances speed and accuracy:

1. **Centroid filter (fast):** Embed the query, compare against all cluster centroids at the target level. Take top ~10 candidates by cosine similarity.
2. **Member verification (precise):** Within each candidate cluster, find the 3–5 best-matching individual memories by vector search. Rank clusters by `best_member_similarity × cluster_heat`.

This avoids the "mushy centroid" problem — where a cluster's average embedding doesn't match any specific query well — while keeping the search space manageable.

### First Call: Broad Recall (no scope_handle)

```text
Agent calls: recall("something about auth tokens")

1. Embed the query → query_vec
2. Centroid filter: top ~10 clusters by cosine similarity
3. Member verification: best 3-5 memories per candidate cluster
4. Rank clusters by: best_member_similarity × cluster_heat
5. Return top clusters with:
   - label ("Authentication & OAuth")
   - heat score
   - 2-3 representative memories (best-matching ones)
   - scope_handle: signed token encoding { cluster_id, depth: 0 }
   - sub-cluster previews if they exist (labels + heat)
```

### Subsequent Calls: Narrowing (with scope_handle)

```text
Agent calls: recall("refresh token expiry", scope_handle: "...")

1. Decode handle → { cluster_id: cluster:auth, depth: 0 }
2. Search WITHIN cluster:auth only:
   - If sub-clusters exist: repeat centroid filter → member
     verify flow against sub-clusters of cluster:auth
   - If no sub-clusters (leaf): vector search against individual
     memories within this cluster
3. Return:
   - Sub-clusters with previews + new scope_handles (depth: 1), OR
   - Individual memories ranked by similarity × heat
   - Each memory includes its lineage chain for drill-down
```

Each subsequent call goes deeper. Eventually you hit leaf clusters and get individual memories.

### Scope Handle Structure

Scope handles are **stateless** — signed, base64-encoded tokens. The server holds no session state.

```rust
struct ScopeHandle {
    cluster_id: Thing,         // which cluster we're scoped into
    depth: u32,                // how many levels deep
    query_embedding: Vec<f32>, // original query for continuity
    issued_at: u64,            // timestamp for optional expiry
}
```

The `query_embedding` is carried forward so subsequent calls can blend the original context with the new query — the agent's recall builds on itself rather than starting fresh each pass.

---

## Cluster Lifecycle

Clusters are living structures — they form, split, merge, and re-label as the memory space evolves. All lifecycle operations are **background maintenance** and never block MCP requests.

### Formation

When a new fact arrives with an embedding:

```text
New fact embedded
       │
       ▼
  Compare against all cluster centroids
  at the deepest applicable level
       │
       ├── Similarity > JOIN_THRESHOLD (e.g. 0.75)
       │   → Add to best-matching cluster
       │   → Recompute centroid (running average)
       │   → Create contains_memory edge
       │
       └── No cluster above threshold
           → Create new single-member cluster
           → Centroid = the fact's embedding
           → Label auto-generated (LLM summarizes content)
```

Centroid update is O(1) — no need to re-embed all members:

```rust
// O(1) centroid update
new_centroid = (old_centroid * member_count + new_embedding) / (member_count + 1)
```

### Splitting

Triggered by **cohesion monitoring**: when the average similarity of members to the centroid drops below a threshold, the cluster is too diffuse and should split.

**Method: Graph-weighted hierarchical agglomerative clustering.**

Run hierarchical agglomerative clustering on member embeddings, but adjust the distance metric with graph edges:

- Members with `relates_to` or `supports` edges get a **similarity bonus** (pull together)
- Members with `contradicts` edges get a **similarity penalty** (push apart)

This works gracefully across the system's maturity:

- **Sparse graph (early):** Falls back to pure embedding-based clustering
- **Dense graph (mature):** Splits along semantic relationship boundaries, producing more meaningful sub-clusters

### Merging

Two sibling clusters (same parent) should merge when they've converged:

```text
Periodic maintenance check:
  For each pair of sibling clusters:
    If centroid_similarity > MERGE_THRESHOLD (e.g. 0.9):
      → Merge into one cluster
      → Recompute centroid from all members
      → Re-generate label
      → Update contains_memory / contains_cluster edges
      → Check cohesion of merged result (split again if too diffuse)
```

Merging only considers siblings — never across the hierarchy.

### Label Generation

Every cluster gets an auto-generated human-readable label. Labels are critical because:

- The `recall` response shows labels to agents for scoping decisions
- Labels make the system inspectable and debuggable

Generated by sending the top 5–10 representative members to an LLM: *"Summarize what these memories have in common in 2–5 words."*

Labels regenerate on split, merge, or when membership changes significantly (>30% new members since last label).

### Lifecycle Summary

```text
                    ┌──────────┐
    new fact ──────►│  Assign  │ join existing or create new
                    └────┬─────┘
                         │
              ┌──────────▼──────────┐
              │  Cohesion Monitor   │ periodic background check
              └──┬──────────────┬───┘
                 │              │
          low cohesion    high similarity
                 │         between siblings
                 ▼              ▼
           ┌─────────┐   ┌─────────┐
           │  Split   │   │  Merge  │
           └─────────┘   └─────────┘
                 │              │
                 └──────┬───────┘
                        ▼
                  Re-label affected clusters
```

---

## Heat & Temperature Model

Inspired by the Ebbinghaus forgetting curve and spaced repetition research. Two core values per memory: **heat** (current temperature, decays over time) and **stability** (durability, grows with well-spaced access).

### Design Principle: Separate Storage for Performance

Heat state lives in its own `heat_state` SurrealDB table, **not on memory records**. This solves three scaling problems:

- **Cache locality:** Heat records are tiny and uniform — no variable-length content or embeddings interleaved. SurrealDB can scan them efficiently.
- **SIMD-friendly computation:** At query time, heat values are fetched and loaded into a transient struct-of-arrays (contiguous `f32` arrays) for bulk computation. The compiler can auto-vectorize these loops.
- **Write isolation:** Heat updates don't touch content records. Spreading activation writes are batched into single DB round-trips.

### Heat State Schema

```surql
DEFINE TABLE heat_state SCHEMAFULL;
DEFINE FIELD memory       ON heat_state TYPE record;  -- links to fact | consolidated | raw
DEFINE FIELD heat         ON heat_state TYPE float DEFAULT 1.0;
DEFINE FIELD stability    ON heat_state TYPE float DEFAULT 1.0;
DEFINE FIELD last_touched ON heat_state TYPE datetime DEFAULT time::now();
DEFINE FIELD access_count ON heat_state TYPE int DEFAULT 0;
```

### Ebbinghaus Decay Model

Each memory decays exponentially, but the rate is governed by its stability. Higher stability = slower decay. Stability grows with well-spaced access — mimicking how human memories strengthen through spaced repetition.

```rust
fn on_access(state: &mut HeatState, now: u64, spacing_halflife: f32) {
    let elapsed = (now - state.last_touched) as f32;

    // 1. Apply decay since last touch
    //    Higher stability → slower decay rate
    let decay_rate = 1.0 / state.stability;
    state.heat *= (-decay_rate * elapsed).exp();

    // 2. Bump heat
    state.heat += 1.0;

    // 3. Strengthen stability — spacing-aware
    //    spacing_factor: 0 for instant re-access, approaches 1.0 for well-spaced access
    let spacing_factor = 1.0 - (-elapsed / spacing_halflife).exp();
    //    count_diminish: sub-linear, first accesses matter most
    let count_diminish = 1.0 / (1.0 + (state.access_count as f32).ln());
    state.stability += spacing_factor * count_diminish;

    state.access_count += 1;
    state.last_touched = now;
}
```

**How it behaves:**

| Scenario | heat | stability | behavior |
| --- | --- | --- | --- |
| Brand new memory | 1.0 | 1.0 | Decays fast if never touched again |
| Burst: 10 accesses in 1 hour | ~10.0 | ~1.05 | Hot now, but stability barely moved. Fades fast once burst stops |
| Steady: accessed weekly for 2 months | ~3.0 | ~4.5 | Moderate heat, high stability. Fades very slowly |
| Foundational: accessed regularly over 6+ months | ~2.0 | ~8.0 | May not be "hot" right now, but nearly permanent |
| Imported fact, never accessed after | 1.0 | 1.0 | Starts warm (import heat boost), fades like any other if unused |

### Initial Heat by Source

```rust
fn initial_heat(source: &Source) -> f32 {
    match source {
        Source::Import       => 2.0,  // user explicitly provided — starts warm
        Source::Conversation => 1.0,  // normal starting point
        Source::Enrichment   => 0.5,  // system-generated, prove your value
    }
}
```

Stability always starts at 1.0 regardless of source.

### Lazy Computation

Decay is **never applied in the background.** It is computed lazily:

- **On access:** The `on_access` function applies decay and writes the update.
- **On query:** A read-only projection computes current heat without writing:

```rust
/// Read-only: what would the heat be right now? (no side effects)
fn projected_heat(state: &HeatState, now: u64) -> f32 {
    let elapsed = (now - state.last_touched) as f32;
    let decay_rate = 1.0 / state.stability;
    state.heat * (-decay_rate * elapsed).exp()
}
```

Queries use `projected_heat` for sorting. Only actual accesses trigger writes.

### On-Demand Columnar Layout for Queries

When a query needs heat-based ranking, heat values are fetched from the `heat_state` table and loaded into a transient struct-of-arrays:

```rust
/// Transient columnar layout — allocated per query, dropped after
struct HeatColumns {
    heat: Vec<f32>,
    stability: Vec<f32>,
    last_touched: Vec<u64>,
}

impl HeatColumns {
    /// SIMD-friendly: contiguous same-type arrays, same operation per element
    fn projected_heat_bulk(&self, now: u64) -> Vec<f32> {
        self.heat.iter()
            .zip(&self.stability)
            .zip(&self.last_touched)
            .map(|((h, s), t)| {
                let elapsed = (now - t) as f32;
                h * (-elapsed / s).exp()
            })
            .collect()
    }
}
```

Vector search narrows candidates first (typically 30–100 records), so columnar math runs on small N. For larger bulk operations (admin dashboards, cold maintenance), the same pattern works in batches.

### Spreading Activation

When a memory is accessed, a fraction of the heat bump propagates along graph edges (`relates_to`, `supports`, `derived_from`), diminishing per hop. Parent clusters are warmed because cluster heat is derived from members.

Critically: **activation adds heat but does NOT increase stability.** Only direct access strengthens durability — second-hand activation warms you up temporarily but doesn't make you more durable.

```rust
struct ActivationConfig {
    direct_bump: f32,         // heat added to accessed memory (default 1.0)
    propagation_factor: f32,  // fraction passed per hop (default 0.3)
    max_hops: u32,            // activation radius (default 2)
}
```

Activation writes are **batched into a single SurrealDB round-trip** per propagation event:

```rust
async fn propagate_activation(
    db: &Surreal,
    neighbors: &[(Thing, f32)], // (memory_id, propagated_heat)
    now: u64,
) {
    // Single batched update — one DB round-trip for all neighbors
    db.query("
        FOR $item IN $neighbors {
            UPDATE heat_state SET
                heat = (heat * math::exp(-((($now - last_touched) / stability))))
                       + $item.propagated_heat,
                last_touched = $now
                -- NO stability or access_count change
            WHERE memory = $item.id
        }
    ").await;
}
```

### Cluster Heat

Cluster heat is **derived from members at query time**, not stored independently. This avoids stale cluster heat values and eliminates cluster-level write amplification:

```rust
fn cluster_heat(member_heats: &[f32]) -> f32 {
    let max = member_heats.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let avg = member_heats.iter().sum::<f32>() / member_heats.len() as f32;
    // Max ensures a single hot memory keeps the cluster findable
    // Average reflects overall cluster activity
    0.6 * max + 0.4 * avg
}
```

### No Thresholds in the API

MCP tools return **raw heat scores**. There are no hot/warm/cold labels in API responses — agents sort and rank by heat as a continuous value. The admin/observability layer computes percentile-based labels for dashboards and metrics only

---

## MCP Tool Interface

Six tools: CRUD, semantic search, document import, and scoping-in recall.

### `store_memory`

```json
{
  "name": "store_memory",
  "params": {
    "content": "string — the memory text",
    "tags": "string[] — optional tags",
    "tier": "raw | fact — default raw, facts skip enrichment extraction",
    "confidence": "float — optional, default 0.5 for facts",
    "metadata": "object — optional freeform metadata"
  }
}
```

Provenance is auto-captured from the MCP connection context (agent ID, session ID, user ID).

### `retrieve_memories`

Direct search across tiers — the familiar CRUD pattern.

```json
{
  "name": "retrieve_memories",
  "params": {
    "query": "string — natural language or keywords",
    "mode": "semantic | keyword | hybrid — default hybrid",
    "tiers": "string[] — which tiers to search, default all",
    "limit": "int — max results, default 10",
    "min_confidence": "float — optional floor",
    "tags": "string[] — optional tag filter"
  }
}
```

Returns results ranked by a blend of similarity score and heat. Each result includes its lineage chain (links to source records) so the agent can drill down.

### `recall`

The **scoping-in** tool — progressive narrowing that mirrors human recall.

```json
{
  "name": "recall",
  "params": {
    "query": "string — what you're trying to remember",
    "scope_handle": "string? — from a previous recall result to narrow within",
    "depth": "broad | focused | specific — how tight to search"
  }
}
```

**First call** (no `scope_handle`): matches at the cluster level, returns top cluster matches with labels and representative memories, plus a `scope_handle`.

**Subsequent calls** (with `scope_handle`): narrows within that cluster, returns sub-clusters or individual memories with a new `scope_handle` to keep narrowing.

### `update_memory`

```json
{
  "name": "update_memory",
  "params": {
    "id": "string — memory record ID",
    "content": "string? — updated text",
    "tags": "string[]? — replace tags",
    "confidence": "float? — override confidence",
    "metadata": "object? — merge into existing metadata"
  }
}
```

Updates re-embed if content changes. Creates a lineage edge to the previous version so history is preserved.

### `delete_memory`

```json
{
  "name": "delete_memory",
  "params": {
    "id": "string — memory record ID"
  }
}
```

Soft-delete only — marks the record inactive and excludes it from search. Underlying data and lineage chain are preserved. Hard deletes and cascade behavior are admin-level operations for a future iteration.

### `import_document`

```json
{
  "name": "import_document",
  "params": {
    "content": "string — document text",
    "mode": "chunk | whole — default chunk",
    "chunk_strategy": "heading | paragraph | fixed_size — default heading",
    "tags": "string[] — applied to all resulting facts",
    "metadata": "object — e.g. { filename, source_url }"
  }
}
```

Imported chunks land as `fact` records with `confidence: 1.0` and `source: "import"`. In `chunk` mode, all records share an `import_batch_id` in provenance metadata. A `raw` record is also created with the full original document, and each fact links back to it via `extracted_from`.

---

## Enrichment Pipeline

Background `tokio` tasks, decoupled from the MCP request path. Writes return immediately; enrichment happens async.

### Pipeline Stages

```
Raw Record Arrives
       │
       ▼
┌──────────────┐
│  1. Extract  │  LLM extracts discrete facts from raw content
│              │  raw:123 → fact:abc, fact:def
└──────┬───────┘
       │  extracted_from edges created
       ▼
┌──────────────┐
│  2. Embed    │  Generate vector embeddings for each fact
│              │  Batched for efficiency
└──────┬───────┘
       │
       ▼
┌──────────────┐
│  3. Cluster  │  Assign to nearest cluster or create new one
│              │  Update cluster centroid
└──────┬───────┘
       │  contains_memory edge created
       ▼
┌──────────────┐
│  4. Relate   │  Discover graph edges to existing facts
│              │  (similarity threshold + LLM verification)
└──────┬───────┘
       │  relates_to / supports / contradicts edges
       ▼
┌──────────────┐
│  5. Consolidate │  Periodic: merge duplicate/overlapping facts
│                 │  into consolidated records
└─────────────────┘
       │  merged_from edges created
```

### Stage Details

**1. Extract** — LLM breaks raw content into atomic claims. Prompt: *"Extract discrete factual claims from this text. Return each as a separate item. Preserve specifics — names, numbers, decisions, dates."* Idempotent — raw record is flagged as processed.

**2. Embed** — Facts batched and sent to configurable embedding model (OpenAI, local, etc.). Stored on the fact record.

**3. Cluster** — Compare against existing cluster centroids. Join if similarity exceeds threshold, otherwise create new cluster. Sub-clusters form naturally when clusters grow large.

**4. Relate** — Two-phase: fast pass (vector similarity within neighboring clusters) → verification pass (LLM determines relationship type and filters junk edges).

**5. Consolidate** — Periodic, not per-write. Finds duplicate/overlapping facts, merges into consolidated records with higher confidence. Originals preserved, deprioritized in retrieval.

### Prioritization

```rust
enum Priority {
    Immediate,  // imported documents (user explicitly provided)
    High,       // facts from active conversations
    Normal,     // background re-processing, re-clustering
    Low,        // cold record maintenance
}
```

Imported documents enter at stages 2–4 (already facts, skip extraction).

### Failure Handling

Each stage independently retryable with exponential backoff. Failed records enter a `dead_letter` table. No data loss — pipeline failure means the memory stays at a lower tier, not that it disappears.

---

## Multi-Tenancy & Isolation

SurrealDB's namespace/database hierarchy maps directly to org/user isolation.

```
SurrealDB Instance
└── Namespace: org_acme              ← one per organization
    ├── Database: user_alice         ← Alice's private memories
    │   ├── table: raw
    │   ├── table: fact
    │   ├── table: consolidated
    │   ├── table: cluster
    │   └── ...
    │
    ├── Database: user_bob           ← Bob's private memories
    │   └── ...
    │
    └── Database: shared             ← org-wide shared memories
        └── ...
```

**Isolation is structural, not query-level.** Each user's database is a completely separate keyspace — no row-level security filters, no risk of cross-tenant leakage.

**Shared org memories** live in a dedicated `shared` database within the namespace. Writes carry provenance. When an agent queries, the MCP server searches both private and shared databases, merging results by heat and relevance. Results are tagged with their scope (`private` | `shared`).

**Authentication:** MCP connection carries identity claims (user ID, org ID). Scoping is derived from auth context, never caller-controlled.

---

## Error Handling & Observability

### Request-Path Errors

- SurrealDB connection failures → MCP error with retry hint
- Invalid queries → structured validation error
- Auth failures → reject before DB access, log attempt
- Embedding service down → `retrieve_memories` falls back to keyword-only; `store_memory` still succeeds (embedding is async)

### Pipeline Errors

- Each stage independently retryable with exponential backoff
- Failed records enter `dead_letter` table with error context, stage, attempt count
- No data loss — failure means lower tier, not disappearance

### Observability

- **Logging:** `tracing` crate with structured spans per MCP call and pipeline stage
- **Metrics:** Prometheus endpoint (`/metrics`)
  - `alexandria_memories_total` — by tier, org, scope
  - `alexandria_recall_latency_seconds` — by depth
  - `alexandria_pipeline_stage_duration_seconds` — by stage
  - `alexandria_pipeline_dead_letters` — by stage
  - `alexandria_heat_distribution` — heat value histogram
  - `alexandria_cluster_count` — by depth level
- **Health:** `/health` endpoint verifying SurrealDB connectivity and pipeline liveness

### Admin Tooling (not MCP-exposed)

- Pipeline status dashboard
- Re-trigger failed enrichment
- Hard delete (only path to actual data removal)
- Cluster health monitoring
- Per-org/user storage usage

---

## Open Questions & Future Work

- **Activation tuning:** Per-org or global activation parameters — needs experimentation
- **Spacing halflife tuning:** The `spacing_halflife` parameter controls how quickly spaced access strengthens stability — likely needs per-org configuration
- **Cascade deletes:** Soft-delete cascade through lineage chain — needs careful design
- **Agent-centric provenance:** Each agent instance with its own memory, tagged with user interaction provenance
- **Cross-org federation:** Shared knowledge bases across organizations
- **SurrealDB fallback:** Postgres migration path if SurrealDB hits production blockers
- **Cluster thresholds:** Tuning JOIN_THRESHOLD, MERGE_THRESHOLD, and cohesion floor — likely needs to be adaptive or per-org
- **Label staleness:** How aggressively to regenerate cluster labels as membership shifts
- **Scope handle expiry:** Whether to enforce TTL on scope handles or let them be indefinitely valid
- **Cluster heat caching:** Whether to cache derived cluster heat values with a TTL to avoid recomputation on every query
- **Heat bulk maintenance:** Strategy for periodic bulk-sweep of very old records that haven't been touched (and therefore never had decay applied lazily)
