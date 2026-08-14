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
| Temperature | Heat float + spreading activation + hierarchical clusters |
| Recall model | Progressive scoping-in via cluster hierarchy + scope handles |
| Provenance | Structured records with agent/session/model context |
| Lineage | Full chain via SurrealDB relations across tiers |
| Multi-tenancy | SurrealDB namespace (org) → database (user) + shared db |
| Enrichment | Async pipeline: extract → embed → cluster → relate → consolidate |
| Document import | Direct-to-fact with chunk/whole modes |
| Deletes | Soft-delete only via MCP |
| MCP tools | store, retrieve, recall, update, delete, import_document |

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
DEFINE FIELD heat         ON raw TYPE float DEFAULT 1.0;
DEFINE FIELD last_touched ON raw TYPE datetime DEFAULT time::now();
DEFINE FIELD metadata     ON raw TYPE option<object>;

-- Tier 1: Extracted facts — discrete claims with embeddings
DEFINE TABLE fact SCHEMAFULL;
DEFINE FIELD content      ON fact TYPE string;
DEFINE FIELD confidence   ON fact TYPE float DEFAULT 0.5;
DEFINE FIELD embedding    ON fact TYPE array<float>;
DEFINE FIELD tags         ON fact TYPE array<string> DEFAULT [];
DEFINE FIELD provenance   ON fact TYPE record<provenance>;
DEFINE FIELD created_at   ON fact TYPE datetime DEFAULT time::now();
DEFINE FIELD heat         ON fact TYPE float DEFAULT 1.0;
DEFINE FIELD last_touched ON fact TYPE datetime DEFAULT time::now();
DEFINE FIELD metadata     ON fact TYPE option<object>;

-- Tier 2: Consolidated — merged/deduped facts, higher confidence
DEFINE TABLE consolidated SCHEMAFULL;
DEFINE FIELD content      ON consolidated TYPE string;
DEFINE FIELD confidence   ON consolidated TYPE float;
DEFINE FIELD embedding    ON consolidated TYPE array<float>;
DEFINE FIELD tags         ON consolidated TYPE array<string> DEFAULT [];
DEFINE FIELD provenance   ON consolidated TYPE record<provenance>;
DEFINE FIELD created_at   ON consolidated TYPE datetime DEFAULT time::now();
DEFINE FIELD heat         ON consolidated TYPE float DEFAULT 1.0;
DEFINE FIELD last_touched ON consolidated TYPE datetime DEFAULT time::now();
```

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
DEFINE FIELD heat       ON cluster TYPE float DEFAULT 0.0;
DEFINE FIELD depth      ON cluster TYPE int DEFAULT 0;    -- nesting level
DEFINE FIELD created_at ON cluster TYPE datetime DEFAULT time::now();

-- Hierarchy: clusters contain sub-clusters
DEFINE TABLE contains_cluster SCHEMAFULL TYPE RELATION IN cluster OUT cluster;

-- Membership: clusters contain memories
DEFINE TABLE contains_memory SCHEMAFULL TYPE RELATION IN cluster OUT fact | consolidated;
```

---

## Heat & Temperature Model

Temperature is driven by a `heat` float on every memory and cluster, combined with spreading activation through graph edges. No arbitrary thresholds — temperature is relative rank within scope.

### Hierarchical Heat

```
Cluster: "Authentication"          ← heat: 8.2
├── Cluster: "OAuth"               ← heat: 6.1
│   ├── fact: "tokens expire 7d"        heat: 4.0
│   ├── fact: "refresh via /token"      heat: 3.2
│   └── fact: "PKCE for public clients" heat: 1.5
├── Cluster: "Session mgmt"        ← heat: 2.3
│   └── ...
```

- Clusters carry their own heat — a memory can be cold individually but its cluster stays warm because siblings are active.
- "Hot" is relative rank within a scope, not an absolute number.

### Spreading Activation

When a memory is accessed:

1. It gets a heat bump.
2. A fraction propagates along graph edges (`relates_to`, `supports`, `derived_from`), diminishing per hop.
3. Parent clusters absorb heat from their members.

Accessing "refresh tokens expire in 7 days" warms up "PKCE for public clients" (sibling) and the "Authentication" cluster as a whole — like how recalling one detail makes related details more accessible.

```rust
struct ActivationConfig {
    direct_bump: f32,         // heat added to accessed memory (default 1.0)
    propagation_factor: f32,  // fraction passed per hop (default 0.3)
    max_hops: u32,            // activation radius (default 2)
    cluster_absorb: f32,      // fraction parent cluster absorbs (default 0.5)
}
```

Activation writes are **batched and async** — the recall query returns immediately, activation propagates in the background so reads aren't blocked by graph walks.

### Open Questions

- Exact decay model (half-life vs. other curves) — needs experimentation
- Whether decay should be lazy (computed on access) or periodic (background sweep)
- Tuning activation parameters per org or globally

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

- **Heat decay model:** Exact decay curve and whether lazy vs. periodic — needs experimentation
- **Activation tuning:** Per-org or global activation parameters
- **Cascade deletes:** Soft-delete cascade through lineage chain — needs careful design
- **Agent-centric provenance:** Each agent instance with its own memory, tagged with user interaction provenance
- **Cross-org federation:** Shared knowledge bases across organizations
- **SurrealDB fallback:** Postgres migration path if SurrealDB hits production blockers
