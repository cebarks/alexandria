use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
// Re-exported from alexandria_storage where it's now defined.
pub use alexandria_storage::record_id_to_string;

use alexandria_engine::clusters::{assign_to_cluster, update_centroid, ClusterInfo};
use alexandria_engine::heat::{compute_activation_targets, ActivationConfig};
use alexandria_engine::recall::{
    broad_recall, focused_recall, ClusterWithMembers, FactSummary, ScopeHandle,
};
use alexandria_engine::search::rank_by_similarity;
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::repos::{ClusterRepo, EdgeRepo, HeatRepo, MemoryRepo};
use alexandria_storage::Database;

use crate::tools::{
    DeleteMemoryParams, ImportDocumentParams, RecallParams, RetrieveMemoriesParams,
    StoreMemoryParams, UpdateMemoryParams,
};

#[derive(Clone)]
pub struct AlexandriaServer {
    pub db: Arc<Database>,
    pub embedding: Arc<dyn EmbeddingProvider>,
    pub cluster_join_threshold: f32,
    pub heat_spacing_halflife: f64,
    pub activation_config: ActivationConfig,
    pub activation_top_n: usize,
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
}

#[tool_router]
impl AlexandriaServer {
    #[tool(
        description = "Soft-delete a memory by ID. Use when the user explicitly says a stored memory is wrong, outdated, or should be forgotten — prefer update_memory for corrections that should be preserved as lineage."
    )]
    async fn delete_memory(
        &self,
        Parameters(params): Parameters<DeleteMemoryParams>,
    ) -> String {
        let repo = MemoryRepo::new(self.db.inner());
        match repo.soft_delete_fact(&params.id).await {
            Ok(_) => format!("Deleted memory {}", params.id),
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(
        description = "Persist a durable fact, decision, preference, or correction so future sessions/agents can recall it. Call this proactively whenever you learn something worth remembering — a user preference, an architectural decision and its rationale, a resolved bug's root cause, a gotcha you just discovered — not only when explicitly told to 'remember this'. Cheap and idempotent-ish (dedup happens via clustering); prefer storing over losing context. Write content as a standalone statement that makes sense without the current conversation."
    )]
    async fn store_memory(
        &self,
        Parameters(params): Parameters<StoreMemoryParams>,
    ) -> String {
        match self.do_store_memory(params).await {
            Ok(id) => {
                serde_json::json!({ "status": "ok", "id": id }).to_string()
            }
            Err(e) => {
                serde_json::json!({ "status": "error", "message": e.to_string() }).to_string()
            }
        }
    }

    #[tool(
        description = "Search stored memories by semantic similarity before answering questions about past decisions, prior conversations, established preferences, or previously-solved problems. Call this proactively at the start of a task in a known project/domain, or whenever the user references 'earlier', 'last time', 'we decided', or something you don't have in the current context — don't wait to be told to check memory."
    )]
    async fn retrieve_memories(
        &self,
        Parameters(params): Parameters<RetrieveMemoriesParams>,
    ) -> String {
        match self.do_retrieve_memories(params).await {
            Ok(results) => serde_json::to_string(&results).unwrap_or_else(|e| {
                serde_json::json!({ "error": e.to_string() }).to_string()
            }),
            Err(e) => {
                serde_json::json!({ "status": "error", "message": e.to_string() }).to_string()
            }
        }
    }

    #[tool(
        description = "Progressive two-phase recall for open-ended or broad questions ('what do we know about X', 'what's the state of Y'): first call with no scope_handle to get candidate clusters, then call again with the returned scope_handle to narrow into the most relevant one. Prefer this over retrieve_memories when the query is exploratory rather than a specific lookup."
    )]
    async fn recall(
        &self,
        Parameters(params): Parameters<RecallParams>,
    ) -> String {
        match self.do_recall(params).await {
            Ok(result) => result,
            Err(e) => {
                serde_json::json!({ "status": "error", "message": e.to_string() }).to_string()
            }
        }
    }

    #[tool(
        description = "Correct or refine an existing memory in place (content, tags, or confidence) instead of storing a duplicate. Content changes trigger re-embedding and preserve the old version via a derived_from lineage edge. Use this the moment you discover a previously stored memory is stale or wrong."
    )]
    async fn update_memory(
        &self,
        Parameters(params): Parameters<UpdateMemoryParams>,
    ) -> String {
        match self.do_update_memory(params).await {
            Ok(result) => result,
            Err(e) => {
                serde_json::json!({ "status": "error", "message": e.to_string() }).to_string()
            }
        }
    }

    #[tool(
        description = "Bulk-load a document (design doc, README, spec, meeting notes, etc.) into memory as one or many chunked entries with lineage back to the source. Use this whenever the user shares or points at reference material worth retaining long-term, not just when asked to 'import' something."
    )]
    async fn import_document(
        &self,
        Parameters(params): Parameters<ImportDocumentParams>,
    ) -> String {
        match self.do_import_document(params).await {
            Ok(result) => result,
            Err(e) => {
                serde_json::json!({ "status": "error", "message": e.to_string() }).to_string()
            }
        }
    }
}

#[tool_handler(instructions = "Alexandria is a persistent agent memory system — use it proactively, not just when explicitly asked to 'remember' or 'recall' something.\n\n\
When to READ memory (retrieve_memories / recall): at the start of a task in a project or domain you've likely worked in before; whenever the user references past context ('last time', 'we decided', 'like before'); before re-deriving a decision or re-debugging something that may have been solved already. Use retrieve_memories for a specific lookup, recall for open-ended/broad exploration (call it once broad, then again with the returned scope_handle to narrow).\n\n\
When to WRITE memory (store_memory): as soon as you learn a durable fact worth keeping past this conversation — a user preference, an architectural decision and its rationale, a bug's root cause, a non-obvious gotcha, a correction the user gives you. Do this unprompted; don't wait to be told to remember. Write standalone statements that make sense without today's conversation.\n\n\
Use update_memory (not store_memory) when correcting something already stored — it preserves lineage. Use import_document for bulk reference material (specs, READMEs, notes). Use delete_memory only when the user wants something actually forgotten.")]
impl ServerHandler for AlexandriaServer {}

// Implementation details
impl AlexandriaServer {
    pub async fn do_store_memory(&self, params: StoreMemoryParams) -> anyhow::Result<String> {
        let tags = params.tags.unwrap_or_default();

        // 1. Embed
        let embeddings = self.embedding.embed(&[&params.content]).await?;
        let embedding = &embeddings[0];

        // 2. Create fact
        let repo = MemoryRepo::new(self.db.inner());
        let fact_id = repo
            .create_fact(&params.content, 0.5, embedding, &tags)
            .await?;

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
        self.assign_to_cluster_and_update(embedding, &fact_id).await?;

        Ok(fact_id)
    }

    pub async fn do_update_memory(&self, params: UpdateMemoryParams) -> anyhow::Result<String> {
        let repo = MemoryRepo::new(self.db.inner());

        // Verify the memory exists
        let existing = repo.get_fact(&params.id).await?;
        let existing = existing.ok_or_else(|| anyhow::anyhow!("Memory not found: {}", params.id))?;

        // Determine if content changed (triggers re-embedding)
        let new_embedding = if let Some(ref new_content) = params.content {
            if new_content != &existing.content {
                let vecs = self.embedding.embed(&[new_content.as_str()]).await?;
                Some(vecs.into_iter().next()
                    .ok_or_else(|| anyhow::anyhow!("Embedding returned empty result"))?)
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
            edge_repo.create_edge(&params.id, &old_snapshot_id, "derived_from", 1.0).await.ok();
        }

        // Perform the update
        let updated = repo.update_fact(
            &params.id,
            params.content.as_deref(),
            params.tags.as_deref(),
            params.confidence,
            new_embedding.as_deref(),
        ).await?;

        match updated {
            Some(_) => Ok(serde_json::json!({
                "status": "ok",
                "id": params.id,
                "content_changed": new_embedding.is_some(),
            }).to_string()),
            None => Err(anyhow::anyhow!("Update failed for {}", params.id)),
        }
    }

    pub async fn do_import_document(&self, params: ImportDocumentParams) -> anyhow::Result<String> {
        use alexandria_engine::import::{chunk_by_heading, chunk_by_paragraph, chunk_by_fixed_size};

        let mode = params.mode.as_deref().unwrap_or("chunk");
        let tags = params.tags.unwrap_or_default();
        let batch_id = uuid::Uuid::new_v4().to_string();

        let chunks = match mode {
            "whole" => vec![params.content.clone()],
            "chunk" => {
                let strategy = params.chunk_strategy.as_deref().unwrap_or("heading");
                match strategy {
                    "heading" => chunk_by_heading(&params.content)
                        .into_iter().map(|c| c.content).collect(),
                    "paragraph" => chunk_by_paragraph(&params.content)
                        .into_iter().map(|c| c.content).collect(),
                    "fixed_size" => chunk_by_fixed_size(&params.content, 1000, 100)
                        .into_iter().map(|c| c.content).collect(),
                    other => anyhow::bail!("Unknown chunk strategy: {other}"),
                }
            }
            other => anyhow::bail!("Unknown import mode: {other}"),
        };

        // Create a raw record for the full document (source for extracted_from edges)
        let raw_id = self.create_raw_record(&params.content).await?;

        let repo = MemoryRepo::new(self.db.inner());
        let heat_repo = HeatRepo::new(self.db.inner());
        let edge_repo = EdgeRepo::new(self.db.inner());
        let mut created_ids = Vec::new();

        // Add batch_id to tags so chunks can be found together
        let mut import_tags = tags;
        import_tags.push(format!("import_batch:{batch_id}"));

        for chunk in &chunks {
            // Embed
            let embeddings = self.embedding.embed(&[chunk.as_str()]).await?;
            let embedding = &embeddings[0];

            // Create fact with import confidence
            let fact_id = repo.create_fact(chunk, 1.0, embedding, &import_tags).await?;

            // Heat state (imports get higher initial heat)
            heat_repo.create_for_memory(&fact_id, 2.0).await?;

            // Create extracted_from edge: chunk → raw document
            edge_repo.create_edge(&fact_id, &raw_id, "extracted_from", 1.0).await.ok();

            // Cluster assignment
            self.assign_to_cluster_and_update(embedding, &fact_id).await?;

            created_ids.push(fact_id);
        }

        Ok(serde_json::json!({
            "status": "ok",
            "count": created_ids.len(),
            "ids": created_ids,
            "batch_id": batch_id,
            "raw_id": raw_id,
        }).to_string())
    }

    pub async fn do_retrieve_memories(
        &self,
        params: RetrieveMemoriesParams,
    ) -> anyhow::Result<serde_json::Value> {
        let limit = params.limit.unwrap_or(10);

        // 1. Embed query
        let query_vecs = self.embedding.embed(&[&params.query]).await?;
        let query_emb = &query_vecs[0];

        // 2. Load all non-deleted facts
        let mut response = self
            .db
            .inner()
            .query("SELECT * FROM fact WHERE deleted = false")
            .await?;
        let facts: Vec<alexandria_storage::models::Fact> = response.take(0)?;

        if facts.is_empty() {
            return Ok(serde_json::json!({ "results": [] }));
        }

        // 3. Rank by similarity
        let embeddings: Vec<Vec<f32>> = facts.iter().map(|f| f.embedding.clone()).collect();
        let ranked = rank_by_similarity(query_emb, &embeddings, limit);

        // 4. Trigger spreading activation for top results
        for (idx, _) in ranked.iter().take(self.activation_top_n) {
            let fact = &facts[*idx];
            if let Some(ref id) = fact.id {
                let fact_id_str = record_id_to_string(id);
                // Fire-and-forget activation — don't block on it
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
            let result = broad_recall(query_emb, &clusters, 5);

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
                    self.db
                        .inner()
                        .query("UPDATE type::record($id) SET centroid = $centroid")
                        .bind(("id", cid))
                        .bind(("centroid", new_centroid))
                        .await?
                        .check()?;
                }
            }
            alexandria_engine::clusters::ClusterAssignment::NewCluster => {
                let cid = cluster_repo.create(None, embedding).await?;
                cluster_repo.add_member(&cid, fact_id).await?;
            }
        }
        Ok(())
    }

    /// Trigger spreading activation for a memory access.
    async fn trigger_activation(&self, fact_id: &str, bump: f32) -> anyhow::Result<()> {
        let edge_repo = EdgeRepo::new(self.db.inner());
        let neighbors = edge_repo.get_neighbors(fact_id, self.activation_config.max_hops).await?;

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
            heat_repo.add_heat(&target.id, target.heat_delta as f64).await.ok();
        }

        Ok(())
    }

    /// Create a raw record for document import.
    async fn create_raw_record(&self, content: &str) -> anyhow::Result<String> {
        let mut response = self.db.inner()
            .query("CREATE raw SET content = $content, deleted = false")
            .bind(("content", content.to_string()))
            .await?;
        let created: Option<alexandria_storage::models::RawRecord> = response.take(0)?;
        let raw = created.ok_or_else(|| anyhow::anyhow!("Failed to create raw record"))?;
        let id = raw.id.ok_or_else(|| anyhow::anyhow!("Raw record has no id"))?;
        Ok(record_id_to_string(&id))
    }

    async fn load_cluster_infos(&self) -> anyhow::Result<Vec<ClusterInfo>> {
        let mut response = self
            .db
            .inner()
            .query("SELECT * FROM cluster")
            .await?;
        let clusters: Vec<alexandria_storage::models::Cluster> = response.take(0)?;

        let cluster_repo = ClusterRepo::new(self.db.inner());
        let mut infos = Vec::with_capacity(clusters.len());

        for c in clusters {
            let id = c
                .id
                .map(|r| record_id_to_string(&r))
                .unwrap_or_default();
            let member_count = cluster_repo.get_members(&id).await
                .map(|m| m.len())
                .unwrap_or(0);
            infos.push(ClusterInfo {
                id,
                centroid: c.centroid,
                member_count,
            });
        }

        Ok(infos)
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
                let id = f
                    .id
                    .map(|r| record_id_to_string(&r))
                    .unwrap_or_default();
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

    async fn load_all_clusters_with_members(
        &self,
    ) -> anyhow::Result<Vec<ClusterWithMembers>> {
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
        alexandria_storage::schema::migrate(db.inner()).await.unwrap();
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
}
