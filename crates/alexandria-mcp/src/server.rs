use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use surrealdb::types::{RecordId, RecordIdKey};

/// Helper to convert RecordId to `table:key` string format for SurrealQL.
fn record_id_to_string(id: &RecordId) -> String {
    let table = id.table.as_str();
    let key = match &id.key {
        RecordIdKey::String(s) => s.clone(),
        RecordIdKey::Number(n) => n.to_string(),
        RecordIdKey::Uuid(u) => format!("{u}"),
        other => format!("{other:?}"),
    };
    format!("{table}:{key}")
}

use alexandria_engine::clusters::{assign_to_cluster, update_centroid, ClusterInfo};
use alexandria_engine::recall::{
    broad_recall, focused_recall, ClusterWithMembers, FactSummary, ScopeHandle,
};
use alexandria_engine::search::rank_by_similarity;
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::repos::{ClusterRepo, HeatRepo, MemoryRepo};
use alexandria_storage::Database;

use crate::tools::{
    DeleteMemoryParams, RecallParams, RetrieveMemoriesParams, StoreMemoryParams,
};

#[derive(Clone)]
pub struct AlexandriaServer {
    pub db: Arc<Database>,
    pub embedding: Arc<dyn EmbeddingProvider>,
    pub cluster_join_threshold: f32,
    pub heat_spacing_halflife: f64,
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
        }
    }
}

#[tool_router(server_handler)]
impl AlexandriaServer {
    #[tool(description = "Soft-delete a memory by ID")]
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

    #[tool(description = "Store a new memory with automatic embedding and clustering")]
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

    #[tool(description = "Search memories by semantic similarity with heat-based ranking")]
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

    #[tool(description = "Progressive recall: broad cluster matching or narrowing within a scope")]
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
}

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
        let cluster_repo = ClusterRepo::new(self.db.inner());
        let clusters = self.load_cluster_infos().await?;
        let assignment =
            assign_to_cluster(embedding, &clusters, self.cluster_join_threshold);

        match assignment {
            alexandria_engine::clusters::ClusterAssignment::Existing(cid) => {
                cluster_repo.add_member(&cid, &fact_id).await?;
                // Update centroid
                let old = clusters.iter().find(|c| c.id == cid).unwrap();
                let new_centroid =
                    update_centroid(&old.centroid, embedding, old.member_count);
                self.db
                    .inner()
                    .query("UPDATE type::record($id) SET centroid = $centroid")
                    .bind(("id", cid.clone()))
                    .bind(("centroid", new_centroid))
                    .await?
                    .check()?;
            }
            alexandria_engine::clusters::ClusterAssignment::NewCluster => {
                let cid = cluster_repo.create(None, embedding).await?;
                cluster_repo.add_member(&cid, &fact_id).await?;
            }
        }

        Ok(fact_id)
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

        // 4. Load heat states for ranked facts
        let _now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let results: Vec<serde_json::Value> = ranked
            .iter()
            .map(|(idx, sim)| {
                let fact = &facts[*idx];
                let id = fact
                    .id
                    .as_ref()
                    .map(|r| {
                        record_id_to_string(r)
                    })
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

    async fn load_cluster_infos(&self) -> anyhow::Result<Vec<ClusterInfo>> {
        let mut response = self
            .db
            .inner()
            .query("SELECT * FROM cluster")
            .await?;
        let clusters: Vec<alexandria_storage::models::Cluster> = response.take(0)?;

        Ok(clusters
            .into_iter()
            .map(|c| {
                let id = c
                    .id
                    .map(|r| {
                        record_id_to_string(&r)
                    })
                    .unwrap_or_default();
                ClusterInfo {
                    id,
                    centroid: c.centroid,
                    member_count: 0, // TODO: query count
                }
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
                let id = f
                    .id
                    .map(|r| {
                        record_id_to_string(&r)
                    })
                    .unwrap_or_default();
                FactSummary {
                    id,
                    content: f.content,
                    embedding: f.embedding,
                    heat: 1.0, // simplified for v0.1
                }
            })
            .collect();

        Ok(ClusterWithMembers {
            info: ClusterInfo {
                id: cluster_id.to_string(),
                centroid: vec![], // not needed for focused recall
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
            // Re-create with the proper centroid from info
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
