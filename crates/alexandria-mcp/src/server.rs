use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
// Re-exported from alexandria_storage where it's now defined.
pub use alexandria_storage::record_id_to_string;

/// Wrap a `do_*` result (a JSON string by construction) into a structured MCP
/// tool result. `CallToolResult::structured()` mirrors the same JSON into a
/// text block, so clients that only read `content[].text` (Pi extension,
/// Claude hooks) are unaffected. A payload that fails to parse means a `do_*`
/// returned non-JSON — a bug — so it is kept as a plain text result rather
/// than fabricated JSON and logged loudly.
fn tool_json(result: anyhow::Result<String>) -> CallToolResult {
    match result {
        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => CallToolResult::structured(v),
            Err(e) => {
                tracing::error!("tool returned non-JSON payload ({e}); passing through as text");
                CallToolResult::success(vec![rmcp::model::ContentBlock::text(s)])
            }
        },
        Err(e) => CallToolResult::structured_error(serde_json::json!({
            "status": "error",
            "message": e.to_string()
        })),
    }
}

use alexandria_engine::clusters::{ClusterInfo, assign_to_cluster, update_centroid};
use alexandria_engine::heat::{ActivationConfig, compute_activation_targets, on_access};
use alexandria_engine::recall::{
    ClusterWithMembers, FactSummary, ScopeHandle, broad_recall, focused_recall,
};
use alexandria_engine::search::rank_by_similarity;
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::Database;
use alexandria_storage::repos::{ClusterRepo, EdgeRepo, HeatRepo, MemoryRepo, SessionRepo};

use crate::tools::{
    DeleteMemoryParams, FinalizeSessionParams, GetSessionParams, ImportDocumentParams,
    ListSessionsParams, RecallParams, RetrieveMemoriesParams, StoreMemoryParams,
    UpdateMemoryParams,
};

#[derive(Clone)]
pub struct AlexandriaServer {
    pub db: Arc<Database>,
    pub embedding: Arc<dyn EmbeddingProvider>,
    pub cluster_join_threshold: f32,
    pub heat_spacing_halflife: f64,
    pub activation_config: ActivationConfig,
    pub activation_top_n: usize,
    /// Hard floor on cosine similarity for retrieve_memories results. The
    /// builder default matches `RetrieveConfig`; production overrides it from config.
    pub retrieve_min_similarity: f32,
}

impl AlexandriaServer {
    pub fn new(
        db: Arc<Database>,
        embedding: Arc<dyn EmbeddingProvider>,
        cluster_join_threshold: f32,
        heat_spacing_halflife: f64,
    ) -> Self {
        Self {
            db,
            embedding,
            cluster_join_threshold,
            heat_spacing_halflife,
            activation_config: ActivationConfig::default(),
            activation_top_n: 3,
            retrieve_min_similarity: 0.10,
        }
    }

    pub fn with_activation_config(mut self, config: ActivationConfig) -> Self {
        self.activation_config = config;
        self
    }

    pub fn with_activation_top_n(mut self, n: usize) -> Self {
        self.activation_top_n = n;
        self
    }

    pub fn with_retrieve_min_similarity(mut self, min_similarity: f32) -> Self {
        self.retrieve_min_similarity = min_similarity;
        self
    }
}

/// Result of `do_store_memory`. `duplicate` means an identical live fact already
/// existed and `id` is that fact's id; nothing was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreOutcome {
    pub id: String,
    pub duplicate: bool,
}

#[tool_router]
impl AlexandriaServer {
    #[tool(
        description = "Soft-delete a memory by ID. Use when the user explicitly says a stored memory is wrong, outdated, or should be forgotten — prefer update_memory for corrections that should be preserved as lineage."
    )]
    async fn delete_memory(
        &self,
        Parameters(params): Parameters<DeleteMemoryParams>,
    ) -> CallToolResult {
        let repo = MemoryRepo::new(self.db.inner());
        match repo.soft_delete_fact(&params.id).await {
            Ok(_) => CallToolResult::structured(serde_json::json!({
                "status": "ok",
                "id": params.id
            })),
            Err(e) => CallToolResult::structured_error(serde_json::json!({
                "status": "error",
                "message": e.to_string()
            })),
        }
    }

    #[tool(
        description = "Persist a durable fact, decision, preference, or correction so future sessions/agents can recall it. Call this proactively whenever you learn something worth remembering — a user preference, an architectural decision and its rationale, a resolved bug's root cause, a gotcha you just discovered — not only when explicitly told to 'remember this'. Cheap and idempotent: storing byte-identical content returns the existing memory's id with status 'duplicate' instead of a second copy, so prefer storing over losing context. A reworded restatement is still a new memory; use update_memory to revise an existing one. Write content as a standalone statement that makes sense without the current conversation."
    )]
    async fn store_memory(
        &self,
        Parameters(params): Parameters<StoreMemoryParams>,
    ) -> CallToolResult {
        match self.do_store_memory(params).await {
            Ok(StoreOutcome { id, duplicate }) => {
                let status = if duplicate { "duplicate" } else { "ok" };
                CallToolResult::structured(serde_json::json!({ "status": status, "id": id }))
            }
            Err(e) => CallToolResult::structured_error(serde_json::json!({
                "status": "error",
                "message": e.to_string()
            })),
        }
    }

    #[tool(
        description = "Search stored memories by semantic similarity before answering questions about past decisions, prior conversations, established preferences, or previously-solved problems. Call this proactively at the start of a task in a known project/domain, or whenever the user references 'earlier', 'last time', 'we decided', or something you don't have in the current context — don't wait to be told to check memory."
    )]
    async fn retrieve_memories(
        &self,
        Parameters(params): Parameters<RetrieveMemoriesParams>,
    ) -> CallToolResult {
        // `structured()` also puts the same JSON in a text block, so clients that
        // only read `content[].text` (Pi extension, Claude hook) are unaffected.
        match self.do_retrieve_memories(params).await {
            Ok(results) => CallToolResult::structured(results),
            Err(e) => CallToolResult::structured_error(
                serde_json::json!({ "status": "error", "message": e.to_string() }),
            ),
        }
    }

    #[tool(
        description = "Progressive two-phase recall for open-ended or broad questions ('what do we know about X', 'what's the state of Y'): first call with no scope_handle to get candidate clusters, then call again with the returned scope_handle to narrow into the most relevant one. Prefer this over retrieve_memories when the query is exploratory rather than a specific lookup."
    )]
    async fn recall(&self, Parameters(params): Parameters<RecallParams>) -> CallToolResult {
        tool_json(self.do_recall(params).await)
    }

    #[tool(
        description = "Correct or refine an existing memory in place (content, tags, or confidence) instead of storing a duplicate. Content changes trigger re-embedding and preserve the old version via a derived_from lineage edge. Use this the moment you discover a previously stored memory is stale or wrong."
    )]
    async fn update_memory(
        &self,
        Parameters(params): Parameters<UpdateMemoryParams>,
    ) -> CallToolResult {
        tool_json(self.do_update_memory(params).await)
    }

    #[tool(
        description = "Bulk-load a document (design doc, README, spec, meeting notes, etc.) into memory as one or many chunked entries with lineage back to the source. Use this whenever the user shares or points at reference material worth retaining long-term, not just when asked to 'import' something."
    )]
    async fn import_document(
        &self,
        Parameters(params): Parameters<ImportDocumentParams>,
    ) -> CallToolResult {
        tool_json(self.do_import_document(params).await)
    }

    #[tool(
        description = "Retrieve a session and all its memories. Use this to review what happened in a specific session — returns the session metadata (summary, tags, memory count, timestamps) plus every memory stored during that session."
    )]
    async fn get_session(
        &self,
        Parameters(params): Parameters<GetSessionParams>,
    ) -> CallToolResult {
        tool_json(self.do_get_session(params).await)
    }

    #[tool(
        description = "List sessions newest-first with their metadata and live memory count. Call this when you need to find a session whose id you don't have — 'the session from yesterday', 'what did the pi agent work on' — then pass its external_id to get_session. Filter by agent_id, tag, or finalized (true = has a summary)."
    )]
    async fn list_sessions(
        &self,
        Parameters(params): Parameters<ListSessionsParams>,
    ) -> CallToolResult {
        tool_json(self.do_list_sessions(params).await)
    }

    #[tool(
        description = "Finalize a session by setting its summary, tags, and ended_at timestamp. Call this when a session wraps up to capture a summary of what was accomplished."
    )]
    async fn finalize_session(
        &self,
        Parameters(params): Parameters<FinalizeSessionParams>,
    ) -> CallToolResult {
        tool_json(self.do_finalize_session(params).await)
    }
}

#[tool_handler(
    instructions = "Alexandria is a persistent agent memory system — use it proactively, not just when explicitly asked to 'remember' or 'recall' something.\n\n\
When to READ memory (retrieve_memories / recall): at the start of a task in a project or domain you've likely worked in before; whenever the user references past context ('last time', 'we decided', 'like before'); before re-deriving a decision or re-debugging something that may have been solved already. Use retrieve_memories for a specific lookup, recall for open-ended/broad exploration (call it once broad, then again with the returned scope_handle to narrow).\n\n\
When to WRITE memory (store_memory): as soon as you learn a durable fact worth keeping past this conversation — a user preference, an architectural decision and its rationale, a bug's root cause, a non-obvious gotcha, a correction the user gives you. Do this unprompted; don't wait to be told to remember. Write standalone statements that make sense without today's conversation.\n\n\
Session memory: pass session_id to store_memory or import_document to group memories by session. Use get_session to review all memories from a session, and list_sessions to find a session id you don't have. Use finalize_session at the end of a session to attach a summary and tags.\n\n\
Use update_memory (not store_memory) when correcting something already stored — it preserves lineage. Use import_document for bulk reference material (specs, READMEs, notes). Use delete_memory only when the user wants something actually forgotten."
)]
impl ServerHandler for AlexandriaServer {}

// Implementation details
impl AlexandriaServer {
    pub async fn do_store_memory(&self, params: StoreMemoryParams) -> anyhow::Result<StoreOutcome> {
        let tags = params.tags.unwrap_or_default();
        let content = params.content.trim();

        // 0. Exact-content duplicate check. Measured 2026-09-10 on the live corpus
        // (docs/minilm-test-data.md, "Duplicate bar"): no cosine bar separates
        // restatements from adjacent distinct facts, and 0.98 caught only the
        // byte-identical set, so equality is the whole of what the data supports.
        let repo = MemoryRepo::new(self.db.inner());
        if let Some(id) = repo.find_by_content(content).await? {
            return Ok(StoreOutcome {
                id,
                duplicate: true,
            });
        }

        // 1. Embed
        let embeddings = self.embedding.embed(&[content]).await?;
        let embedding = &embeddings[0];

        // 2. Create fact
        let fact_id = repo.create_fact(content, 0.5, embedding, &tags).await?;

        // 3. Create heat state
        let heat_repo = HeatRepo::new(self.db.inner());
        heat_repo.create_for_memory(&fact_id, 1.0).await?;

        // 4. Create provenance
        self.db
            .inner()
            .query("CREATE provenance SET kind = 'user', timestamp = time::now()")
            .await?
            .check()?;

        // 5. Cluster assignment
        self.assign_to_cluster_and_update(embedding, &fact_id)
            .await?;

        // 6. Session linkage (implicit create on first use)
        if let Some(ref session_id) = params.session_id {
            let session_repo = SessionRepo::new(self.db.inner());
            let session_rid = session_repo
                .find_or_create(
                    session_id,
                    params.agent_id.as_deref(),
                    params.model.as_deref(),
                )
                .await?;
            session_repo.add_memory(&session_rid, &fact_id).await?;
            session_repo.touch(session_id).await?;
        }

        Ok(StoreOutcome {
            id: fact_id,
            duplicate: false,
        })
    }

    pub async fn do_update_memory(&self, params: UpdateMemoryParams) -> anyhow::Result<String> {
        let repo = MemoryRepo::new(self.db.inner());

        // Verify the memory exists
        let existing = repo.get_fact(&params.id).await?;
        let existing =
            existing.ok_or_else(|| anyhow::anyhow!("Memory not found: {}", params.id))?;

        // Determine if content changed (triggers re-embedding)
        let new_embedding = if let Some(ref new_content) = params.content {
            if new_content != &existing.content {
                let vecs = self.embedding.embed(&[new_content.as_str()]).await?;
                Some(
                    vecs.into_iter()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("Embedding returned empty result"))?,
                )
            } else {
                None
            }
        } else {
            None
        };

        // If content changed, store old content hash as lineage marker
        if new_embedding.is_some() {
            let edge_repo = EdgeRepo::new(self.db.inner());
            // Store a snapshot of the old content as a new fact, link via derived_from
            let old_snapshot_id = MemoryRepo::new(self.db.inner())
                .create_fact(
                    &existing.content,
                    existing.confidence,
                    &existing.embedding,
                    &existing.tags,
                )
                .await?;
            // Mark snapshot as superseded (soft-delete so it doesn't appear in search)
            MemoryRepo::new(self.db.inner())
                .soft_delete_fact(&old_snapshot_id)
                .await?;
            // Create lineage edge: current → old snapshot
            edge_repo
                .create_edge(&params.id, &old_snapshot_id, "derived_from", 1.0)
                .await
                .ok();
        }

        // Perform the update
        let updated = repo
            .update_fact(
                &params.id,
                params.content.as_deref(),
                params.tags.as_deref(),
                params.confidence,
                new_embedding.as_deref(),
            )
            .await?;

        match updated {
            Some(_) => Ok(serde_json::json!({
                "status": "ok",
                "id": params.id,
                "content_changed": new_embedding.is_some(),
            })
            .to_string()),
            None => Err(anyhow::anyhow!("Update failed for {}", params.id)),
        }
    }

    pub async fn do_import_document(&self, params: ImportDocumentParams) -> anyhow::Result<String> {
        use alexandria_engine::import::{
            chunk_by_fixed_size, chunk_by_heading, chunk_by_paragraph,
        };

        let mode = params.mode.as_deref().unwrap_or("chunk");
        let tags = params.tags.unwrap_or_default();
        let batch_id = uuid::Uuid::new_v4().to_string();

        let chunks = match mode {
            "whole" => vec![params.content.clone()],
            "chunk" => {
                let strategy = params.chunk_strategy.as_deref().unwrap_or("heading");
                match strategy {
                    "heading" => chunk_by_heading(&params.content)
                        .into_iter()
                        .map(|c| c.content)
                        .collect(),
                    "paragraph" => chunk_by_paragraph(&params.content)
                        .into_iter()
                        .map(|c| c.content)
                        .collect(),
                    // 800 chars stays under the 256-token embedding limit even for code-dense text.
                    "fixed_size" => chunk_by_fixed_size(&params.content, 800, 100)
                        .into_iter()
                        .map(|c| c.content)
                        .collect(),
                    other => anyhow::bail!("Unknown chunk strategy: {other}"),
                }
            }
            other => anyhow::bail!("Unknown import mode: {other}"),
        };

        // Create a raw record for the full document (source for extracted_from edges)
        let repo = MemoryRepo::new(self.db.inner());
        let raw_id = repo.create_raw(&params.content).await?;
        let heat_repo = HeatRepo::new(self.db.inner());
        let edge_repo = EdgeRepo::new(self.db.inner());
        let mut created_ids = Vec::new();

        // Add batch_id to tags so chunks can be found together
        let mut import_tags = tags;
        import_tags.push(format!("import_batch:{batch_id}"));

        // Session linkage (implicit create on first use), resolved once for all chunks
        let session_repo = SessionRepo::new(self.db.inner());
        let session_rid = match params.session_id {
            Some(ref session_id) => Some(
                session_repo
                    .find_or_create(
                        session_id,
                        params.agent_id.as_deref(),
                        params.model.as_deref(),
                    )
                    .await?,
            ),
            None => None,
        };

        for chunk in &chunks {
            // Embed
            let embeddings = self.embedding.embed(&[chunk.as_str()]).await?;
            let embedding = &embeddings[0];

            // Create fact with import confidence
            let fact_id = repo
                .create_fact(chunk, 1.0, embedding, &import_tags)
                .await?;

            // Heat state (imports get higher initial heat)
            heat_repo.create_for_memory(&fact_id, 2.0).await?;

            // Create extracted_from edge: chunk → raw document
            edge_repo
                .create_edge(&fact_id, &raw_id, "extracted_from", 1.0)
                .await
                .ok();

            // Cluster assignment
            self.assign_to_cluster_and_update(embedding, &fact_id)
                .await?;

            if let Some(ref session_rid) = session_rid {
                session_repo.add_memory(session_rid, &fact_id).await?;
            }

            created_ids.push(fact_id);
        }

        if let Some(ref session_id) = params.session_id {
            session_repo.touch(session_id).await?;
        }

        Ok(serde_json::json!({
            "status": "ok",
            "count": created_ids.len(),
            "ids": created_ids,
            "batch_id": batch_id,
            "raw_id": raw_id,
        })
        .to_string())
    }

    pub async fn do_retrieve_memories(
        &self,
        params: RetrieveMemoriesParams,
    ) -> anyhow::Result<serde_json::Value> {
        let limit = params.limit.unwrap_or(10);

        // 1. Embed query
        let query_vecs = self.embedding.embed(&[&params.query]).await?;
        let query_emb = &query_vecs[0];

        // 2. Load candidates: the session's facts if scoped, otherwise the `limit`
        // nearest live facts (HNSW when the index is defined).
        let facts: Vec<alexandria_storage::models::Fact> =
            if let Some(ref session_id) = params.session_id {
                let session_repo = SessionRepo::new(self.db.inner());
                session_repo.get_memories(session_id).await?
            } else {
                MemoryRepo::new(self.db.inner())
                    .nearest(query_emb, limit)
                    .await?
            };

        if facts.is_empty() {
            return Ok(serde_json::json!({ "results": [] }));
        }

        // 3. Rank by similarity, then drop results below the server-side floor.
        // This is a conservative defense-in-depth cutoff: it removes pure noise
        // even if a client sets a lax threshold, without changing semantics for
        // deliberate agent lookups (the floor sits well below plausible matches).
        let embeddings: Vec<Vec<f32>> = facts.iter().map(|f| f.embedding.clone()).collect();
        let ranked: Vec<(usize, f32)> = rank_by_similarity(query_emb, &embeddings, limit)
            .into_iter()
            .filter(|(_, sim)| *sim >= self.retrieve_min_similarity)
            .collect();

        // 4. Record the access and trigger spreading activation for top results
        for (idx, _) in ranked.iter().take(self.activation_top_n) {
            let fact = &facts[*idx];
            if let Some(ref id) = fact.id {
                let fact_id_str = record_id_to_string(id);
                // Best-effort side effects — never fail a retrieval over them
                let _ = self.record_access(&fact_id_str).await;
                let _ = self.trigger_activation(&fact_id_str, 1.0).await;
            }
        }

        // 5. Build results
        let results: Vec<serde_json::Value> = ranked
            .iter()
            .map(|(idx, sim)| {
                let fact = &facts[*idx];
                let id = fact
                    .id
                    .as_ref()
                    .map(record_id_to_string)
                    .unwrap_or_default();
                serde_json::json!({
                    "id": id,
                    "content": fact.content,
                    "similarity": sim,
                    "tags": fact.tags,
                })
            })
            .collect();

        Ok(serde_json::json!({ "results": results }))
    }

    pub async fn do_recall(&self, params: RecallParams) -> anyhow::Result<String> {
        // Embed query
        let query_vecs = self.embedding.embed(&[&params.query]).await?;
        let query_emb = &query_vecs[0];

        if let Some(ref handle_str) = params.scope_handle {
            // Focused recall
            let scope = ScopeHandle::decode(handle_str)?;
            let cluster_data = self.load_cluster_with_members(&scope.cluster_id).await?;
            let result = focused_recall(query_emb, &scope, &cluster_data);

            let memories: Vec<serde_json::Value> = result
                .memories
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "content": m.content,
                        "similarity": m.similarity,
                        "heat": m.heat,
                    })
                })
                .collect();

            Ok(serde_json::json!({
                "mode": "focused",
                "memories": memories,
            })
            .to_string())
        } else {
            // Broad recall
            let clusters = self.load_all_clusters_with_members().await?;
            let result = broad_recall(query_emb, &clusters, 5, self.retrieve_min_similarity);

            let cluster_results: Vec<serde_json::Value> = result
                .clusters
                .iter()
                .map(|cm| {
                    let mems: Vec<serde_json::Value> = cm
                        .representative_memories
                        .iter()
                        .map(|m| {
                            serde_json::json!({
                                "id": m.id,
                                "content": m.content,
                                "similarity": m.similarity,
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "cluster_id": cm.cluster_id,
                        "similarity": cm.similarity,
                        "scope_handle": cm.scope_handle,
                        "representative_memories": mems,
                    })
                })
                .collect();

            Ok(serde_json::json!({
                "mode": "broad",
                "clusters": cluster_results,
            })
            .to_string())
        }
    }

    pub async fn do_list_sessions(&self, params: ListSessionsParams) -> anyhow::Result<String> {
        let sessions = SessionRepo::new(self.db.inner())
            .list(
                params.agent_id.as_deref(),
                params.tag.as_deref(),
                params.finalized,
                params.limit.unwrap_or(20),
                params.offset.unwrap_or(0),
            )
            .await?;
        let list: Vec<serde_json::Value> = sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "external_id": s.external_id,
                    "agent_id": s.agent_id,
                    "model": s.model,
                    "started_at": s.started_at,
                    "ended_at": s.ended_at,
                    "summary": s.summary,
                    "memory_count": s.memory_count,
                    "tags": s.tags,
                })
            })
            .collect();
        Ok(serde_json::json!({ "sessions": list, "count": list.len() }).to_string())
    }

    pub async fn do_get_session(&self, params: GetSessionParams) -> anyhow::Result<String> {
        let session_repo = SessionRepo::new(self.db.inner());

        let session = session_repo
            .find_by_external_id(&params.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", params.session_id))?;

        let memories = session_repo.get_memories(&params.session_id).await?;
        let memory_list: Vec<serde_json::Value> = memories
            .iter()
            .map(|f| {
                let id = f.id.as_ref().map(record_id_to_string).unwrap_or_default();
                serde_json::json!({
                    "id": id,
                    "content": f.content,
                    "tags": f.tags,
                    "confidence": f.confidence,
                    "created_at": f.created_at,
                })
            })
            .collect();

        Ok(serde_json::json!({
            "session": {
                "external_id": session.external_id,
                "agent_id": session.agent_id,
                "model": session.model,
                "started_at": session.started_at,
                "ended_at": session.ended_at,
                "summary": session.summary,
                "memory_count": memories.len(),
                "tags": session.tags,
            },
            "memories": memory_list,
        })
        .to_string())
    }

    pub async fn do_finalize_session(
        &self,
        params: FinalizeSessionParams,
    ) -> anyhow::Result<String> {
        let session_repo = SessionRepo::new(self.db.inner());

        let updated = session_repo
            .finalize(
                &params.session_id,
                params.summary.as_deref(),
                params.tags.as_deref(),
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", params.session_id))?;

        Ok(serde_json::json!({
            "status": "ok",
            "external_id": updated.external_id,
            "ended_at": updated.ended_at,
            "summary": updated.summary,
            "tags": updated.tags,
        })
        .to_string())
    }

    // --- Internal helpers ---

    /// Assign a fact to a cluster, creating a new one if needed. Updates centroids.
    async fn assign_to_cluster_and_update(
        &self,
        embedding: &[f32],
        fact_id: &str,
    ) -> anyhow::Result<()> {
        let cluster_repo = ClusterRepo::new(self.db.inner());
        let clusters = self.load_cluster_infos().await?;
        let assignment = assign_to_cluster(embedding, &clusters, self.cluster_join_threshold);

        match assignment {
            alexandria_engine::clusters::ClusterAssignment::Existing(cid) => {
                cluster_repo.add_member(&cid, fact_id).await?;
                if let Some(old) = clusters.iter().find(|c| c.id == cid) {
                    let new_centroid = update_centroid(&old.centroid, embedding, old.member_count);
                    cluster_repo.update_centroid(&cid, &new_centroid).await?;
                }
            }
            alexandria_engine::clusters::ClusterAssignment::NewCluster => {
                let cid = cluster_repo.create(None, embedding).await?;
                cluster_repo.add_member(&cid, fact_id).await?;
            }
        }
        Ok(())
    }

    /// Record an access on a memory's heat row: reset heat, grow stability by
    /// spacing, bump access_count. Heat is recorded here but not yet used in
    /// ranking; see A2 in docs/performance-and-ability-findings.md.
    async fn record_access(&self, fact_id: &str) -> anyhow::Result<()> {
        let heat_repo = HeatRepo::new(self.db.inner());
        let Some(row) = heat_repo.get(fact_id).await? else {
            return Ok(());
        };
        let Some(ref row_id) = row.id else {
            return Ok(());
        };
        let mut state = alexandria_engine::heat::HeatState {
            heat: row.heat,
            stability: row.stability,
            last_touched: row
                .last_touched
                .map(|dt| dt.timestamp().max(0) as u64)
                .unwrap_or(0),
            access_count: row.access_count.max(0) as u64,
        };
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        on_access(&mut state, now, self.heat_spacing_halflife);
        heat_repo
            .update(
                &record_id_to_string(row_id),
                state.heat,
                state.stability,
                state.access_count as i64,
            )
            .await
    }

    /// Trigger spreading activation for a memory access.
    async fn trigger_activation(&self, fact_id: &str, bump: f32) -> anyhow::Result<()> {
        let edge_repo = EdgeRepo::new(self.db.inner());
        let neighbors = edge_repo
            .get_neighbors(fact_id, self.activation_config.max_hops)
            .await?;

        if neighbors.is_empty() {
            return Ok(());
        }

        let neighbor_data: Vec<(String, u32, f64)> = neighbors
            .iter()
            .map(|n| {
                let id_str = record_id_to_string(&n.id);
                (id_str, n.hop, n.strength)
            })
            .collect();

        let targets = compute_activation_targets(&neighbor_data, bump, &self.activation_config);

        // Batch-update heat for all activation targets
        let heat_repo = HeatRepo::new(self.db.inner());
        for target in &targets {
            heat_repo
                .add_heat(&target.id, target.heat_delta as f64)
                .await
                .ok();
        }

        Ok(())
    }

    async fn load_cluster_infos(&self) -> anyhow::Result<Vec<ClusterInfo>> {
        let clusters = ClusterRepo::new(self.db.inner()).list_with_counts().await?;
        Ok(clusters
            .into_iter()
            .map(|(c, member_count)| ClusterInfo {
                id: c.id.map(|r| record_id_to_string(&r)).unwrap_or_default(),
                centroid: c.centroid,
                member_count,
            })
            .collect())
    }

    async fn load_cluster_with_members(
        &self,
        cluster_id: &str,
    ) -> anyhow::Result<ClusterWithMembers> {
        let cluster_repo = ClusterRepo::new(self.db.inner());
        let members = cluster_repo.get_members(cluster_id).await?;

        let fact_summaries: Vec<FactSummary> = members
            .into_iter()
            .map(|f| {
                let id = f.id.map(|r| record_id_to_string(&r)).unwrap_or_default();
                FactSummary {
                    id,
                    content: f.content,
                    embedding: f.embedding,
                    heat: 1.0,
                }
            })
            .collect();

        Ok(ClusterWithMembers {
            info: ClusterInfo {
                id: cluster_id.to_string(),
                centroid: vec![],
                member_count: fact_summaries.len(),
            },
            members: fact_summaries,
        })
    }

    async fn load_all_clusters_with_members(&self) -> anyhow::Result<Vec<ClusterWithMembers>> {
        let infos = self.load_cluster_infos().await?;
        let mut result = Vec::with_capacity(infos.len());
        for info in infos {
            let cwm = self.load_cluster_with_members(&info.id).await?;
            result.push(ClusterWithMembers {
                info: ClusterInfo {
                    id: cwm.info.id,
                    centroid: info.centroid,
                    member_count: cwm.members.len(),
                },
                members: cwm.members,
            });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod get_info_tests {
    use super::*;
    use rmcp::ServerHandler;

    struct StubEmbedding;

    #[async_trait::async_trait]
    impl EmbeddingProvider for StubEmbedding {
        async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.1, 0.2]).collect())
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn model_id(&self) -> &str {
            "stub"
        }
    }

    #[tokio::test]
    async fn get_info_carries_usage_instructions_and_tools_capability() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0);

        let info = server.get_info();

        let instructions = info
            .instructions
            .expect("server must advertise usage instructions to MCP clients");
        assert!(instructions.contains("proactively"));
        assert!(instructions.contains("store_memory"));
        assert!(instructions.contains("retrieve_memories"));
        assert!(info.capabilities.tools.is_some());
    }

    #[tokio::test]
    async fn store_memory_returns_existing_id_for_identical_content() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0);
        let store = |content: &str| StoreMemoryParams {
            content: content.to_string(),
            tags: None,
            session_id: None,
            agent_id: None,
            model: None,
        };

        let first = server.do_store_memory(store("same fact")).await.unwrap();
        assert!(!first.duplicate);

        // Byte-identical content, and the same content with surrounding whitespace.
        let again = server.do_store_memory(store("same fact")).await.unwrap();
        assert_eq!(
            again,
            StoreOutcome {
                id: first.id.clone(),
                duplicate: true
            }
        );
        let padded = server
            .do_store_memory(store("  same fact\n"))
            .await
            .unwrap();
        assert_eq!(padded.id, first.id);
        assert!(padded.duplicate);

        // Different content stores normally even though the stub embeds it identically.
        let other = server.do_store_memory(store("other fact")).await.unwrap();
        assert!(!other.duplicate);
        assert_ne!(other.id, first.id);

        // A soft-deleted fact does not count as a live duplicate.
        MemoryRepo::new(server.db.inner())
            .soft_delete_fact(&first.id)
            .await
            .unwrap();
        let revived = server.do_store_memory(store("same fact")).await.unwrap();
        assert!(!revived.duplicate);
        assert_ne!(revived.id, first.id);
    }

    /// Stub that maps content/query text to fixed embeddings so we can assert
    /// the retrieve floor deterministically: text containing "far" -> [0, 1]
    /// (orthogonal to the query, cosine 0), everything else -> [1, 0] (aligned
    /// with the query, cosine 1).
    struct DirectionalEmbedding;

    #[async_trait::async_trait]
    impl EmbeddingProvider for DirectionalEmbedding {
        async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    if t.contains("far") {
                        vec![0.0, 1.0]
                    } else {
                        vec![1.0, 0.0]
                    }
                })
                .collect())
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn model_id(&self) -> &str {
            "directional-stub"
        }
    }

    #[tokio::test]
    async fn retrieve_memories_drops_results_below_floor() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server =
            AlexandriaServer::new(Arc::new(db), Arc::new(DirectionalEmbedding), 0.75, 86400.0)
                .with_retrieve_min_similarity(0.30);

        server
            .do_store_memory(StoreMemoryParams {
                content: "a near match memory".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();
        server
            .do_store_memory(StoreMemoryParams {
                content: "a far away memory".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();

        let result = server
            .do_retrieve_memories(RetrieveMemoriesParams {
                query: "looking for something".to_string(),
                limit: Some(10),
                session_id: None,
            })
            .await
            .unwrap();

        let results = result["results"].as_array().unwrap();
        // The orthogonal "far" memory (cosine 0.0) is below the 0.30 floor and
        // must be dropped; only the aligned "near" memory survives.
        assert_eq!(results.len(), 1, "floor should drop the orthogonal memory");
        assert!(results[0]["content"].as_str().unwrap().contains("near"));
        assert!(results[0]["similarity"].as_f64().unwrap() >= 0.30);
    }

    /// Same retrieval through the HNSW index: the query asks the database for
    /// `limit` neighbours, so the count is bounded before the engine ranks.
    #[tokio::test]
    async fn retrieve_memories_serves_from_vector_index() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        alexandria_storage::schema::ensure_vector_index(db.inner(), 2)
            .await
            .unwrap();
        let server =
            AlexandriaServer::new(Arc::new(db), Arc::new(DirectionalEmbedding), 0.75, 86400.0);
        for content in ["near one", "near two", "far away"] {
            server
                .do_store_memory(StoreMemoryParams {
                    content: content.to_string(),
                    tags: None,
                    session_id: None,
                    agent_id: None,
                    model: None,
                })
                .await
                .unwrap();
        }

        let retrieve = |limit| {
            server.do_retrieve_memories(RetrieveMemoriesParams {
                query: "anything".to_string(),
                limit: Some(limit),
                session_id: None,
            })
        };
        let results = retrieve(2).await.unwrap();
        let results = results["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        for r in results {
            assert!(r["content"].as_str().unwrap().contains("near"));
        }
        let results = retrieve(0).await.unwrap();
        assert_eq!(results["results"].as_array().unwrap().len(), 0);
    }

    /// Stub producing vectors with exact cosine similarity to the query [1, 0]:
    /// text containing "below" -> cosine 0.29, "above" -> cosine 0.31. A unit
    /// vector [s, sqrt(1 - s^2)] has cosine s with [1, 0], so these straddle a
    /// 0.30 floor and catch `<` vs `<=` / off-by-epsilon regressions.
    struct BoundaryEmbedding;

    #[async_trait::async_trait]
    impl EmbeddingProvider for BoundaryEmbedding {
        async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let s: f32 = if t.contains("below") {
                        0.29
                    } else if t.contains("above") {
                        0.31
                    } else {
                        1.0
                    };
                    vec![s, (1.0 - s * s).sqrt()]
                })
                .collect())
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn model_id(&self) -> &str {
            "boundary-stub"
        }
    }

    #[tokio::test]
    async fn retrieve_memories_floor_is_inclusive_at_boundary() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server =
            AlexandriaServer::new(Arc::new(db), Arc::new(BoundaryEmbedding), 0.75, 86400.0)
                .with_retrieve_min_similarity(0.30);

        server
            .do_store_memory(StoreMemoryParams {
                content: "just below the floor".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();
        server
            .do_store_memory(StoreMemoryParams {
                content: "just above the floor".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();

        let result = server
            .do_retrieve_memories(RetrieveMemoriesParams {
                query: "query".to_string(),
                limit: Some(10),
                session_id: None,
            })
            .await
            .unwrap();

        let results = result["results"].as_array().unwrap();
        // 0.31 >= 0.30 survives; 0.29 < 0.30 is dropped.
        assert_eq!(
            results.len(),
            1,
            "only the above-floor memory should survive"
        );
        assert!(results[0]["content"].as_str().unwrap().contains("above"));
    }

    #[tokio::test]
    async fn list_sessions_filters_and_counts() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0);

        for (sess, agent) in [("sess-l1", "pi"), ("sess-l2", "claude-code")] {
            server
                .do_store_memory(StoreMemoryParams {
                    content: format!("fact in {sess}"),
                    tags: None,
                    session_id: Some(sess.to_string()),
                    agent_id: Some(agent.to_string()),
                    model: None,
                })
                .await
                .unwrap();
        }
        server
            .do_finalize_session(FinalizeSessionParams {
                session_id: "sess-l2".to_string(),
                summary: Some("wrapped".to_string()),
                tags: None,
            })
            .await
            .unwrap();

        let all: serde_json::Value = serde_json::from_str(
            &server
                .do_list_sessions(ListSessionsParams {
                    agent_id: None,
                    tag: None,
                    finalized: None,
                    limit: None,
                    offset: None,
                })
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(all["count"], 2);
        assert_eq!(all["sessions"][0]["external_id"], "sess-l2");
        assert_eq!(all["sessions"][0]["memory_count"], 1);
        assert_eq!(all["sessions"][0]["summary"], "wrapped");

        let open: serde_json::Value = serde_json::from_str(
            &server
                .do_list_sessions(ListSessionsParams {
                    agent_id: Some("pi".to_string()),
                    tag: None,
                    finalized: Some(false),
                    limit: None,
                    offset: None,
                })
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(open["count"], 1);
        assert_eq!(open["sessions"][0]["external_id"], "sess-l1");
    }

    #[tokio::test]
    async fn get_session_hides_deleted_and_reports_live_count() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0);

        let _kept = server
            .do_store_memory(StoreMemoryParams {
                content: "kept fact".to_string(),
                tags: None,
                session_id: Some("sess-del".to_string()),
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();
        let gone = server
            .do_store_memory(StoreMemoryParams {
                content: "deleted fact".to_string(),
                tags: None,
                session_id: Some("sess-del".to_string()),
                agent_id: None,
                model: None,
            })
            .await
            .unwrap()
            .id;
        MemoryRepo::new(server.db.inner())
            .soft_delete_fact(&gone)
            .await
            .unwrap();

        let session_json = server
            .do_get_session(GetSessionParams {
                session_id: "sess-del".to_string(),
            })
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&session_json).unwrap();
        let memories = parsed["memories"].as_array().unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0]["content"], "kept fact");
        assert_eq!(parsed["session"]["memory_count"], 1);

        // Session-scoped search shares the same path and must hide it too.
        let result = server
            .do_retrieve_memories(RetrieveMemoriesParams {
                query: "fact".to_string(),
                limit: Some(10),
                session_id: Some("sess-del".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn import_document_links_chunks_to_session() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0);

        server
            .do_store_memory(StoreMemoryParams {
                content: "unrelated fact outside the session".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();
        server
            .do_import_document(ImportDocumentParams {
                content: "first paragraph\n\nsecond paragraph".to_string(),
                mode: None,
                chunk_strategy: Some("paragraph".to_string()),
                tags: None,
                session_id: Some("sess-import".to_string()),
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();

        let session_json = server
            .do_get_session(GetSessionParams {
                session_id: "sess-import".to_string(),
            })
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&session_json).unwrap();
        assert_eq!(parsed["memories"].as_array().unwrap().len(), 2);

        let result = server
            .do_retrieve_memories(RetrieveMemoriesParams {
                query: "paragraph".to_string(),
                limit: Some(10),
                session_id: Some("sess-import".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(result["results"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn session_memory_store_retrieve_finalize() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0);

        // Store memories with a session_id — session auto-creates
        let _id1 = server
            .do_store_memory(StoreMemoryParams {
                content: "first session fact".to_string(),
                tags: None,
                session_id: Some("sess-abc".to_string()),
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();
        let _id2 = server
            .do_store_memory(StoreMemoryParams {
                content: "second session fact".to_string(),
                tags: Some(vec!["important".to_string()]),
                session_id: Some("sess-abc".to_string()),
                agent_id: Some("claude-code".to_string()),
                model: Some("claude-sonnet-5".to_string()),
            })
            .await
            .unwrap();

        // Also store a memory outside the session
        let _id3 = server
            .do_store_memory(StoreMemoryParams {
                content: "unrelated fact".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();

        // Retrieve scoped to session — should only get the 2 session facts
        let result = server
            .do_retrieve_memories(RetrieveMemoriesParams {
                query: "session fact".to_string(),
                limit: Some(10),
                session_id: Some("sess-abc".to_string()),
            })
            .await
            .unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            2,
            "session-scoped retrieve should return only session memories"
        );

        // Get session details
        let session_json = server
            .do_get_session(GetSessionParams {
                session_id: "sess-abc".to_string(),
            })
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&session_json).unwrap();
        assert_eq!(parsed["session"]["external_id"], "sess-abc");
        assert_eq!(parsed["session"]["memory_count"], 2);
        // Acquired from the second store_memory; the session was created without them.
        assert_eq!(parsed["session"]["agent_id"], "claude-code");
        assert_eq!(parsed["session"]["model"], "claude-sonnet-5");
        assert!(parsed["session"]["summary"].is_null());
        assert_eq!(parsed["memories"].as_array().unwrap().len(), 2);

        // Finalize the session
        let finalize_json = server
            .do_finalize_session(FinalizeSessionParams {
                session_id: "sess-abc".to_string(),
                summary: Some("debugging session".to_string()),
                tags: Some(vec!["debug".to_string()]),
            })
            .await
            .unwrap();
        let finalized: serde_json::Value = serde_json::from_str(&finalize_json).unwrap();
        assert_eq!(finalized["status"], "ok");
        assert_eq!(finalized["summary"], "debugging session");
        assert!(finalized["ended_at"].is_string());

        // Verify session is finalized
        let session_json = server
            .do_get_session(GetSessionParams {
                session_id: "sess-abc".to_string(),
            })
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&session_json).unwrap();
        assert_eq!(parsed["session"]["summary"], "debugging session");
        assert_eq!(parsed["session"]["tags"].as_array().unwrap().len(), 1);

        // Non-existent session should error
        let err = server
            .do_get_session(GetSessionParams {
                session_id: "nonexistent".to_string(),
            })
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn retrieve_memories_tool_returns_structured_content() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server =
            AlexandriaServer::new(Arc::new(db), Arc::new(DirectionalEmbedding), 0.75, 86400.0);
        server
            .do_store_memory(StoreMemoryParams {
                content: "a near match memory".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();

        let result = server
            .retrieve_memories(Parameters(RetrieveMemoriesParams {
                query: "anything".to_string(),
                limit: Some(10),
                session_id: None,
            }))
            .await;

        let structured = result.structured_content.expect("structuredContent set");
        assert_eq!(structured["results"].as_array().unwrap().len(), 1);
        // Text block carries the same JSON so clients reading content[].text keep working.
        let rmcp::model::ContentBlock::Text(t) = &result.content[0] else {
            panic!("expected text block");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&t.text).unwrap(),
            structured
        );
        assert_eq!(result.is_error, Some(false));
    }
    /// Each retrieve records an access on its top results: the heat row's
    /// access_count climbs and last_touched moves. Ranking is unaffected.
    #[tokio::test]
    async fn retrieve_memories_records_access_on_top_results() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server =
            AlexandriaServer::new(Arc::new(db), Arc::new(DirectionalEmbedding), 0.75, 86400.0);
        let id = server
            .do_store_memory(StoreMemoryParams {
                content: "a near memory".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap()
            .id;
        let heat_repo = HeatRepo::new(server.db.inner());
        let before = heat_repo.get(&id).await.unwrap().unwrap();
        assert_eq!(before.access_count, 0);

        let retrieve = || {
            server.do_retrieve_memories(RetrieveMemoriesParams {
                query: "anything".to_string(),
                limit: Some(10),
                session_id: None,
            })
        };
        retrieve().await.unwrap();
        let after_one = heat_repo.get(&id).await.unwrap().unwrap();
        assert_eq!(after_one.access_count, 1);
        assert!(after_one.last_touched >= before.last_touched);

        retrieve().await.unwrap();
        let after_two = heat_repo.get(&id).await.unwrap().unwrap();
        assert_eq!(after_two.access_count, 2);
        assert_eq!(after_two.heat, 1.0);
    }

    fn check_structured(label: &str, result: &CallToolResult) {
        assert_eq!(
            result.is_error,
            Some(false),
            "{label}: unexpected error result"
        );
        let structured = result
            .structured_content
            .as_ref()
            .unwrap_or_else(|| panic!("{label}: structuredContent missing"));
        // The same JSON must stay in the text block for text-only consumers.
        let rmcp::model::ContentBlock::Text(t) = result
            .content
            .first()
            .unwrap_or_else(|| panic!("{label}: no content block"))
        else {
            panic!("{label}: expected text block");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&t.text).unwrap(),
            *structured,
            "{label}: text block and structuredContent diverged"
        );
    }

    #[tokio::test]
    async fn every_tool_returns_structured_content_matching_text() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0);

        let stored = server
            .store_memory(Parameters(StoreMemoryParams {
                content: "the project uses SurrealDB".to_string(),
                tags: Some(vec!["db".to_string()]),
                session_id: Some("sess-struct".to_string()),
                agent_id: None,
                model: None,
            }))
            .await;
        check_structured("store_memory", &stored);
        let id = stored.structured_content.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        check_structured(
            "recall",
            &server
                .recall(Parameters(RecallParams {
                    query: "database".to_string(),
                    scope_handle: None,
                }))
                .await,
        );
        check_structured(
            "update_memory",
            &server
                .update_memory(Parameters(UpdateMemoryParams {
                    id: id.clone(),
                    content: Some("the project uses SurrealDB 3.2".to_string()),
                    tags: None,
                    confidence: None,
                }))
                .await,
        );
        check_structured(
            "import_document",
            &server
                .import_document(Parameters(ImportDocumentParams {
                    content: "first paragraph\n\nsecond paragraph".to_string(),
                    mode: None,
                    chunk_strategy: Some("paragraph".to_string()),
                    tags: None,
                    session_id: Some("sess-struct".to_string()),
                    agent_id: None,
                    model: None,
                }))
                .await,
        );
        check_structured(
            "get_session",
            &server
                .get_session(Parameters(GetSessionParams {
                    session_id: "sess-struct".to_string(),
                }))
                .await,
        );
        check_structured(
            "list_sessions",
            &server
                .list_sessions(Parameters(ListSessionsParams {
                    agent_id: None,
                    tag: None,
                    finalized: None,
                    limit: None,
                    offset: None,
                }))
                .await,
        );
        check_structured(
            "finalize_session",
            &server
                .finalize_session(Parameters(FinalizeSessionParams {
                    session_id: "sess-struct".to_string(),
                    summary: Some("structured-content exercise".to_string()),
                    tags: None,
                }))
                .await,
        );
        check_structured(
            "delete_memory",
            &server
                .delete_memory(Parameters(DeleteMemoryParams { id }))
                .await,
        );
    }

    #[tokio::test]
    async fn tool_errors_are_flagged_structured() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0);

        let result = server
            .update_memory(Parameters(UpdateMemoryParams {
                id: "fact:does-not-exist".to_string(),
                content: Some("x".to_string()),
                tags: None,
                confidence: None,
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structuredContent set");
        assert_eq!(structured["status"], "error");
        assert!(
            structured["message"]
                .as_str()
                .unwrap()
                .contains("does-not-exist")
        );
    }
}
