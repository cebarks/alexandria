# Roadmap

## Completed

### v0.1 — Core Foundation

- Embedded SurrealDB with in-memory storage
- Candle embedding provider (all-MiniLM-L6-v2, pure Rust)
- Ebbinghaus heat model with decay and stability
- Hierarchical clustering with cosine similarity
- Progressive recall (broad → focused with scope handles)
- 4 MCP tools: `store_memory`, `retrieve_memories`, `recall`, `delete_memory`
- 28 tests

### v0.2 — Production Readiness

- TOML config file with env var overrides
- Persistent storage via SurrealKV on disk
- Version-tracked schema migrations (forward-only `.surql` files)
- Embedding model safety check (refuses start on mismatch)
- Graph edges (`relates_to`, `supports`, `contradicts`, `derived_from`, `extracted_from`)
- Spreading activation (heat propagation along edges on retrieve)
- Cluster split/merge maintenance (background task every 5min)
- 2 new MCP tools: `update_memory`, `import_document`
- HTTP transport via rmcp StreamableHttpService
- systemd deployment
- 64 tests

## Planned

### v0.3 — Self-Organizing Memory

The goal: memories should organize themselves without manual curation.

**Background enrichment pipeline**

- Async task queue with priority levels
- Extract → Relate → Consolidate stages
- Decouple heavy processing from the request path

**LLM-based relation discovery**

- Similarity pre-filter to find candidate pairs
- LLM verification to classify edge type (`supports`, `contradicts`, `relates_to`)
- Auto-create edges between related memories

**Cluster label generation**

- LLM-generated human-readable labels from member content
- Label staleness detection and regeneration as membership shifts
- Makes recall results actionable ("Authentication patterns" vs "cluster:abc123")

**OpenAI embedding provider**

- Feature-gated alternative to Candle
- Higher quality embeddings at API cost
- Model-agnostic — any OpenAI-compatible API

### v0.4+ — Scale & Multi-Tenancy

**Multi-tenancy**

- Namespace (org) → database (user) isolation via SurrealDB's native multi-db
- Per-tenant config overrides

**Performance**

- SurrealDB vector index for DB-side cosine similarity (matters at >10k memories)
- Full-text search index for keyword matching alongside semantic search
- Cluster heat caching with TTL
- Bulk heat maintenance sweep for untouched records

**Operational**

- Embedding model migration CLI (`alexandria migrate-embeddings`)
- Cascade soft-deletes through lineage chains
- Scope handle expiry / TTL
- Metrics endpoint

**Additional embedding providers**

- Ollama (local, any model)
- Cohere, Voyage, Jina (API)
- GPU acceleration via Vulkan when candle ships it

## Ideas / Research

- **Two types of facts:** Known-true (imported, from docs) vs learned-over-time (agent-discovered). Different confidence defaults and decay curves.
- **Agent-centric provenance:** Each agent instance gets its own memory space, tagged with user interaction provenance
- **Cross-org federation:** Shared knowledge bases across organizations
- **SurrealDB fallback:** Postgres migration path if SurrealDB hits production blockers
- **Adaptive thresholds:** Per-org or per-domain cluster/heat tuning instead of global defaults
- **Multi-architecture models:** GTE, E5, and non-BERT models in the candle provider
