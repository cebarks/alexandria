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
        // `error_message` (not `to_string`) so the whole anyhow chain reaches the
        // caller: reminder validation wraps the underlying parser's reason, and
        // `to_string()` would surface only the top-level context.
        Err(e) => CallToolResult::structured_error(serde_json::json!({
            "status": "error",
            "message": error_message(&e)
        })),
    }
}

use alexandria_engine::clusters::{ClusterInfo, assign_to_cluster, update_centroid};
use alexandria_engine::heat::{ActivationConfig, compute_activation_targets};
use alexandria_engine::recall::{
    ClusterWithMembers, FactSummary, ScopeHandle, broad_recall, focused_recall,
};
use alexandria_engine::search::rank_by_similarity;
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::Database;
use alexandria_storage::repos::{ClusterRepo, EdgeRepo, HeatRepo, MemoryRepo, SessionRepo};
use chrono::{DateTime, SecondsFormat, Utc, Weekday};

use crate::tools::{
    CancelReminderParams, CheckRemindersParams, DeleteMemoryParams, FinalizeSessionParams,
    GetSessionParams, ImportDocumentParams, ListRemindersParams, RecallParams,
    RetrieveMemoriesParams, SetReminderParams, StoreMemoryParams, UpdateMemoryParams,
};

/// Z-suffixed UTC RFC 3339 spelling — single source of truth for every datetime
/// in reminder tool responses (`to_rfc3339()` would emit `+00:00` instead).
/// Seconds precision is fixed (`Secs`, not `AutoSi`) so the shape never varies
/// with sub-second content: reminder wall-clock granularity is the minute.
fn rfc3339_utc(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// One-line rendering of a whole error chain for tool responses. `{e:#}` keeps
/// the underlying cause (e.g. the cron crate's "Minutes must be less than 59"
/// rather than a bare "invalid cron expression"); whitespace is collapsed
/// because a cause may render across lines and the JSON response should not.
fn error_message(e: &anyhow::Error) -> String {
    format!("{e:#}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reminder delivery settings resolved from server config at startup.
#[derive(Debug, Clone)]
pub struct RemindersSettings {
    pub tz: chrono_tz::Tz,
    pub escalation_hours: u64,
}

impl Default for RemindersSettings {
    fn default() -> Self {
        Self {
            tz: chrono_tz::Tz::UTC,
            escalation_hours: 48,
        }
    }
}

#[derive(Clone)]
pub struct AlexandriaServer {
    pub db: Arc<Database>,
    pub embedding: Arc<dyn EmbeddingProvider>,
    pub cluster_join_threshold: f32,
    pub heat_spacing_halflife: f64,
    pub activation_config: ActivationConfig,
    pub activation_top_n: usize,
    /// Hard floor on cosine similarity for retrieve_memories results.
    pub retrieve_min_similarity: f32,
    pub reminders: RemindersSettings,
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
            retrieve_min_similarity: 0.30,
            reminders: RemindersSettings::default(),
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

    pub fn with_reminders_config(mut self, settings: RemindersSettings) -> Self {
        self.reminders = settings;
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
        description = "Persist a durable fact, decision, preference, or correction so future sessions/agents can recall it. Call this proactively whenever you learn something worth remembering — a user preference, an architectural decision and its rationale, a resolved bug's root cause, a gotcha you just discovered — not only when explicitly told to 'remember this'. Cheap and idempotent-ish (dedup happens via clustering); prefer storing over losing context. Write content as a standalone statement that makes sense without the current conversation."
    )]
    async fn store_memory(
        &self,
        Parameters(params): Parameters<StoreMemoryParams>,
    ) -> CallToolResult {
        match self.do_store_memory(params).await {
            Ok(id) => CallToolResult::structured(serde_json::json!({ "status": "ok", "id": id })),
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
        description = "Finalize a session by setting its summary, tags, and ended_at timestamp. Call this when a session wraps up to capture a summary of what was accomplished."
    )]
    async fn finalize_session(
        &self,
        Parameters(params): Parameters<FinalizeSessionParams>,
    ) -> CallToolResult {
        tool_json(self.do_finalize_session(params).await)
    }

    #[tool(
        description = "Schedule a reminder to be delivered on a future interaction — one-shot (due_at) or recurring (pattern/cron). Use this when the user asks to be reminded of something later, when they say 'remind me', 'don't let me forget', or when a follow-up action will be needed at a specific time ('check the deploy at 3pm', 'ping me about this tomorrow morning'). Reminders are delivered to both the agent context and the user on the next interaction after they come due; project-targeted reminders escalate to global delivery if they stay overdue, so they are never silently lost. The response includes the next fire times — confirm them with the user when the schedule was parsed from natural language."
    )]
    async fn set_reminder(
        &self,
        Parameters(params): Parameters<SetReminderParams>,
    ) -> CallToolResult {
        tool_json(self.do_set_reminder(params).await)
    }

    #[tool(
        description = "Check for reminders that have come due and consume them. Client integrations call this automatically at the start of each interaction; you generally don't need to call it manually unless the user asks 'any reminders?'. Delivery is best-effort-once: calling this marks returned reminders as delivered (recurring ones advance to their next occurrence, coalescing any missed fires into a missed_occurrences count)."
    )]
    async fn check_reminders(
        &self,
        Parameters(params): Parameters<CheckRemindersParams>,
    ) -> CallToolResult {
        tool_json(self.do_check_reminders(params).await)
    }

    #[tool(
        description = "List reminders (pending by default; filter by status 'pending'|'delivered'|'cancelled'|'all' and/or target project). Use when the user asks what reminders exist, or to find an ID before cancelling."
    )]
    async fn list_reminders(
        &self,
        Parameters(params): Parameters<ListRemindersParams>,
    ) -> CallToolResult {
        tool_json(self.do_list_reminders(params).await)
    }

    #[tool(
        description = "Cancel a reminder by ID (soft-cancel; it stays listed under status=cancelled). Use when the user says a reminder is no longer needed, or after a reminder was delivered and the follow-up is done — for recurring reminders the user no longer wants."
    )]
    async fn cancel_reminder(
        &self,
        Parameters(params): Parameters<CancelReminderParams>,
    ) -> CallToolResult {
        tool_json(self.do_cancel_reminder(params).await)
    }
}

#[tool_handler(
    instructions = "Alexandria is a persistent agent memory system — use it proactively, not just when explicitly asked to 'remember' or 'recall' something.\n\n\
When to READ memory (retrieve_memories / recall): at the start of a task in a project or domain you've likely worked in before; whenever the user references past context ('last time', 'we decided', 'like before'); before re-deriving a decision or re-debugging something that may have been solved already. Use retrieve_memories for a specific lookup, recall for open-ended/broad exploration (call it once broad, then again with the returned scope_handle to narrow).\n\n\
When to WRITE memory (store_memory): as soon as you learn a durable fact worth keeping past this conversation — a user preference, an architectural decision and its rationale, a bug's root cause, a non-obvious gotcha, a correction the user gives you. Do this unprompted; don't wait to be told to remember. Write standalone statements that make sense without today's conversation.\n\n\
Session memory: pass session_id to store_memory or import_document to group memories by session. Use get_session to review all memories from a session. Use finalize_session at the end of a session to attach a summary and tags.\n\n\
Use update_memory (not store_memory) when correcting something already stored — it preserves lineage. Use import_document for bulk reference material (specs, READMEs, notes). Use delete_memory only when the user wants something actually forgotten.\n\n\
Reminders: use set_reminder when the user asks to be reminded of something later or a follow-up will be needed at a specific time. Omit target_project for anything the user should see anywhere; set it for repo-bound follow-ups (exact, case-sensitive match). The response carries next_fire_preview and the timezone the schedule was parsed in — confirm those with the user whenever the schedule came from natural language. Due reminders are delivered when a client calls check_reminders at the start of an interaction — the server runs no timer, so nothing fires on its own. check_reminders is the only tool that consumes them (recurring ones advance; missed fires coalesce rather than trickle); call it yourself when the user asks whether anything is due, or when a reminder they expected has not shown up.\n\n\
Reminder delivery: retrieve_memories and recall responses also carry a due_reminders array: a read-only, untargeted view of what is currently due (a short oldest-due sample, each entry has a target of global or project:<name>) that consumes nothing. An entry targeting another project is informational there; it reaches the user through the next check_reminders, which also delivers project reminders once they are overdue by more than the server's escalation window. Use list_reminders to review what is scheduled and cancel_reminder when a reminder is no longer needed."
)]
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
        self.assign_to_cluster_and_update(embedding, &fact_id)
            .await?;

        // 6. Session linkage (implicit create on first use)
        if let Some(ref session_id) = params.session_id {
            let session_repo = SessionRepo::new(self.db.inner());
            if session_repo
                .find_by_external_id(session_id)
                .await?
                .is_none()
            {
                session_repo.create(session_id, None, None).await?;
            }
            let session = session_repo.find_by_external_id(session_id).await?.unwrap();
            let session_rid = session.id.map(|r| record_id_to_string(&r)).unwrap();
            session_repo.add_memory(&session_rid, &fact_id).await?;
            session_repo.touch(session_id).await?;
        }

        Ok(fact_id)
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
                    "fixed_size" => chunk_by_fixed_size(&params.content, 1000, 100)
                        .into_iter()
                        .map(|c| c.content)
                        .collect(),
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

        // Session linkage (implicit create on first use), resolved once for all chunks
        let session_repo = SessionRepo::new(self.db.inner());
        let session_rid = match params.session_id {
            Some(ref session_id) => {
                if session_repo
                    .find_by_external_id(session_id)
                    .await?
                    .is_none()
                {
                    session_repo.create(session_id, None, None).await?;
                }
                let session = session_repo.find_by_external_id(session_id).await?.unwrap();
                Some(session.id.map(|r| record_id_to_string(&r)).unwrap())
            }
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

        // 2. Load facts (scoped to session if provided, otherwise all non-deleted)
        let facts: Vec<alexandria_storage::models::Fact> =
            if let Some(ref session_id) = params.session_id {
                let session_repo = SessionRepo::new(self.db.inner());
                session_repo.get_memories(session_id).await?
            } else {
                let mut response = self
                    .db
                    .inner()
                    .query("SELECT * FROM fact WHERE deleted = false")
                    .await?;
                response.take(0)?
            };

        if facts.is_empty() {
            return Ok(serde_json::json!({
                "results": [],
                "due_reminders": self.due_reminders_summary(5).await,
            }));
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

        Ok(serde_json::json!({
            "results": results,
            "due_reminders": self.due_reminders_summary(5).await,
        }))
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
                "due_reminders": self.due_reminders_summary(5).await,
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
                "due_reminders": self.due_reminders_summary(5).await,
            })
            .to_string())
        }
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

    /// Create a reminder row from validated tool params. All schedule parsing and
    /// validation happens here (set-time, not delivery-time) so a bad cron or an
    /// impossible local time is rejected while the user is still in the loop.
    pub async fn do_set_reminder(&self, params: SetReminderParams) -> anyhow::Result<String> {
        use alexandria_engine::reminders as sched;
        use alexandria_storage::models::schedule_kind;
        use alexandria_storage::repos::{NewReminder, ReminderRepo};
        use anyhow::Context;
        use chrono::Timelike;

        /// Flat storage columns for one schedule kind. A named struct instead of
        /// the positional 6-tuple keeps the branches readable and
        /// `clippy::type_complexity` quiet.
        struct RowFields {
            due_at: Option<DateTime<Utc>>,
            freq: Option<String>,
            time_of_day: Option<String>,
            weekdays: Vec<String>,
            day_of_month: Option<i64>,
            cron_expr: Option<String>,
        }

        let tz = self.reminders.tz;
        let now = Utc::now();

        // Exactly one schedule kind must be given. The failure names the
        // offending fields so a caller can fix the request without guessing.
        let given: Vec<&str> = [
            ("due_at", params.due_at.is_some()),
            ("pattern", params.pattern.is_some()),
            ("cron", params.cron.is_some()),
        ]
        .into_iter()
        .filter(|(_, is_some)| *is_some)
        .map(|(name, _)| name)
        .collect();
        if given.len() != 1 {
            let got = if given.is_empty() {
                "none".to_string()
            } else {
                given.join(" + ")
            };
            anyhow::bail!("provide exactly one of due_at, pattern, or cron (got {got})");
        }

        // A blank target is not "no target": `None` is global, while an empty or
        // all-whitespace `target_project` matches no project hint (matching is
        // byte-exact) and can therefore only ever be held and then escalated —
        // noise at the worst possible time. Refuse it while the user is still
        // asking, and name the alternative.
        if params
            .target_project
            .as_deref()
            .is_some_and(|project| project.trim().is_empty())
        {
            anyhow::bail!(
                "target_project must be a non-empty project name, or omit it for a global reminder"
            );
        }

        let (spec, kind, row): (sched::ScheduleSpec, &'static str, RowFields) =
            if let Some(due) = params.due_at.as_deref() {
                let due_at = sched::parse_datetime(due, tz)?;
                (
                    sched::ScheduleSpec::Once { due_at },
                    schedule_kind::ONCE,
                    RowFields {
                        due_at: Some(due_at),
                        freq: None,
                        time_of_day: None,
                        weekdays: Vec::new(),
                        day_of_month: None,
                        cron_expr: None,
                    },
                )
            } else if let Some(pat) = params.pattern.as_ref() {
                let freq = sched::parse_freq(&pat.freq)?;
                let time = sched::parse_time_of_day(&pat.time)?;
                // Parse once, dedupe, and derive both the stored strings and the
                // cron expression from the parsed values: `['fri','FRI','friday']`
                // would otherwise leak "every Friday, Friday, Friday" into the
                // user-facing schedule string and mixed spellings into the row.
                let mut weekdays: Vec<Weekday> = Vec::new();
                if let Some(named) = &pat.weekdays {
                    for w in named {
                        let day = sched::parse_weekday(w)?;
                        if !weekdays.contains(&day) {
                            weekdays.push(day);
                        }
                    }
                }
                let weekday_names: Vec<String> = weekdays
                    .iter()
                    .map(|w| w.to_string().to_ascii_lowercase())
                    .collect();
                // `ScheduleSpec::Pattern` requires a concrete u32, so 1 is a
                // type-level filler, not a semantic default: the validation below
                // bails for Monthly without day_of_month (the engine refuses to
                // guess "the 1st"), so the filler is only reachable for
                // Daily/Weekly, where the field is unused.
                let dom = pat.day_of_month.unwrap_or(1);
                // Validate the field combination before storing anything. The
                // asymmetry is deliberate: validation takes the RAW user Option
                // (so monthly-without-day and weekly/daily-with-day both fail via
                // the engine's messages), while the stored column is masked to
                // Monthly only — the other freqs store NULL, the reader's contract.
                let dom_for_cron = pat.day_of_month;
                let stored_dom = (freq == sched::Freq::Monthly).then_some(i64::from(dom));
                // Result discarded on purpose: this call is validation-only
                // (next_fire/upcoming recompile the expression themselves).
                let _validated_cron = sched::pattern_to_cron(freq, time, &weekdays, dom_for_cron)?;
                (
                    sched::ScheduleSpec::Pattern {
                        freq,
                        time,
                        weekdays,
                        day_of_month: dom,
                    },
                    schedule_kind::PATTERN,
                    RowFields {
                        due_at: None,
                        freq: Some(pat.freq.to_ascii_lowercase()),
                        time_of_day: Some(format!("{:02}:{:02}", time.hour(), time.minute())),
                        weekdays: weekday_names,
                        day_of_month: stored_dom,
                        cron_expr: None,
                    },
                )
            } else {
                // `params.cron` is Some here: the exactly-one check above ruled
                // out the other two. The context names that invariant instead of
                // an `unwrap()` (repo convention: no panics in production paths).
                let raw = params
                    .cron
                    .as_deref()
                    .context("cron schedule missing despite exactly-one check")?;
                let expr = sched::normalize_cron(raw)?;
                (
                    sched::ScheduleSpec::Cron { expr: expr.clone() },
                    schedule_kind::CRON,
                    RowFields {
                        due_at: None,
                        freq: None,
                        time_of_day: None,
                        weekdays: Vec::new(),
                        day_of_month: None,
                        cron_expr: Some(expr),
                    },
                )
            };

        // Next fire + preview from one walk of the schedule: `upcoming` returns
        // the fires strictly after `now`, oldest first, so its first element *is*
        // what `next_fire` would compute — calling both would compile and iterate
        // the same cron expression twice for one request. Recurring specs are
        // evaluated in the configured timezone (wall-clock semantics —
        // cron/chrono-tz handle DST), and `Once` yields `[due_at]` only while it
        // is still in the future, which keeps the past-one-shot warning below on
        // the same condition `next_fire` used to test.
        let preview = sched::upcoming(&spec, now, tz, 3)?;
        let next = preview.first().copied();
        // A recurring schedule with no future fire would be stored with
        // next_due_at = NULL, and `list_due` filters on `next_due_at <= $now`, so
        // such a row can never be delivered or escalated — a silent loss.
        // Reject at set time instead. (A past one-shot is different: it stores
        // next_due_at = due_at and fires on the very next check.)
        if let (sched::ScheduleSpec::Pattern { .. } | sched::ScheduleSpec::Cron { .. }, None) =
            (&spec, next)
        {
            anyhow::bail!(
                "schedule {} never fires again (check year fields or impossible dates like Feb 30); no reminder created",
                sched::human_readable(&spec)
            );
        }
        let warning = match (&spec, next) {
            (sched::ScheduleSpec::Once { due_at }, None) => Some(format!(
                "due_at {} is in the past; this reminder will fire on the very next check",
                rfc3339_utc(*due_at)
            )),
            _ => None,
        };
        // A past one-shot still stores next_due_at = due_at so list_due catches it.
        let next_due_at = match &spec {
            sched::ScheduleSpec::Once { due_at } => Some(*due_at),
            _ => next,
        };

        let repo = ReminderRepo::new(self.db.inner());
        let id = repo
            .create(&NewReminder {
                message: params.message.clone(),
                target_project: params.target_project.clone(),
                prov_project: params.prov_project.clone(),
                prov_session_id: params.session_id.clone(),
                note: params.note.clone(),
                schedule_kind: kind.to_string(),
                due_at: row.due_at,
                freq: row.freq,
                time_of_day: row.time_of_day,
                weekdays: row.weekdays,
                day_of_month: row.day_of_month,
                cron_expr: row.cron_expr,
                next_due_at,
            })
            .await?;

        // `schedule` renders a one-shot in UTC but a pattern in local wall-clock,
        // so also spell out next_due_at in the configured timezone: the tool
        // description asks the LLM to confirm these times with the user, who is
        // thinking in local time.
        let next_due_at_local =
            next_due_at.map(|d| d.with_timezone(&tz).format("%Y-%m-%d %H:%M %Z").to_string());

        let mut out = serde_json::json!({
            "status": "ok",
            "id": id,
            "schedule": sched::human_readable(&spec),
            "next_due_at": next_due_at.map(rfc3339_utc),
            "next_due_at_local": next_due_at_local,
            "next_fire_preview": preview.iter().map(|d| rfc3339_utc(*d)).collect::<Vec<_>>(),
            "timezone": tz.name().to_string(),
        });
        if let Some(w) = warning {
            out["warning"] = serde_json::Value::String(w);
        }
        Ok(out.to_string())
    }

    /// Deliver the reminders that have come due, consuming each one.
    ///
    /// Due-ness is a query-time predicate — `status = 'pending' AND
    /// next_due_at <= now` (`ReminderRepo::list_due`) — because delivery rides on
    /// interactions and never on a background timer. Targeting is decided here:
    /// global reminders always deliver, a project reminder delivers on an exact
    /// match with the caller's project, and one that has been overdue for
    /// `escalation_hours` escalates to global delivery so a project that stops
    /// being visited can't swallow it silently. The escalation boundary is
    /// inclusive on the escalate side — a row escalates once it is *at least*
    /// `escalation_hours` overdue, which is what makes `escalation_hours: 0`
    /// escalate every overdue project reminder.
    ///
    /// Consumption is best-effort-once, including under concurrent consumers: a
    /// one-shot becomes `delivered`, while a recurring schedule advances to the
    /// first fire after `now` and the occurrences skipped in between are reported
    /// as `missed_occurrences` instead of trickling out one per check. Each row is
    /// claimed with a conditional write
    /// (`ReminderRepo::record_delivery` — `status = 'pending' AND next_due_at =
    /// <the value this call read>`), so two overlapping checks can never both
    /// deliver the same row and a `cancel` landing after `list_due` can never be
    /// clobbered into `delivered`: a lost claim skips the row, and the consumer
    /// that won owns it. Recording a delivery is still a separate write per row
    /// and every per-row outcome is handled in place — the row is skipped (staying
    /// pending, so the next check retries it) and the rows already gathered are
    /// still reported, because dropping the response would discard the consumption
    /// of rows that can never come due again. The window that remains is a claim
    /// that succeeds and then loses its response (the request future being
    /// cancelled): that row is consumed and unreported, so the guarantee can lose
    /// a delivery — it can no longer duplicate one.
    pub async fn do_check_reminders(&self, params: CheckRemindersParams) -> anyhow::Result<String> {
        use alexandria_engine::reminders as sched;
        use alexandria_storage::repos::ReminderRepo;

        let tz = self.reminders.tz;
        let now = Utc::now();
        // Saturate without wrapping *or* panicking: `escalation_hours` is a bare
        // u64 in config, `Duration::hours` takes an i64 count of hours *and*
        // panics above i64::MAX/3600 of them, so saturating the `u64`→`i64` step
        // alone still lands on "TimeDelta::hours out of bounds" — which would kill
        // the serve loop (stdio) or the request task (HTTP) on every check.
        // `try_hours` reports the bound instead, and a window too large to
        // represent is a window nothing can be overdue by: project reminders are
        // held rather than escalated.
        let escalation = match i64::try_from(self.reminders.escalation_hours)
            .ok()
            .and_then(chrono::Duration::try_hours)
        {
            Some(window) => window,
            None => {
                tracing::warn!(
                    "reminders escalation_hours {} cannot be represented as a duration; \
                     project reminders will be held rather than escalated",
                    self.reminders.escalation_hours
                );
                chrono::Duration::MAX
            }
        };

        let repo = ReminderRepo::new(self.db.inner());
        let due = repo.list_due(now).await?;
        let due_count = due.len();

        let mut delivered = Vec::new();
        for r in due {
            // Without an id there is nothing to record the consumption against;
            // treat it like any other unreadable row rather than sending an empty
            // key to the repo.
            let Some(id) = r.id.as_ref().map(record_id_to_string) else {
                tracing::warn!("skipping reminder row with no id: {}", r.message);
                continue;
            };

            // Reconstructing the spec is the row's own integrity check, done
            // before any targeting decision so a corrupt row is reported once per
            // shape rather than only in the contexts that would have delivered
            // it. One bad row must never break the others, and must not be
            // cancelled behind the user's back: log it (the engine's message
            // already names the row) and leave it pending.
            let spec = match sched::spec_from_reminder(&r) {
                Ok(spec) => spec,
                Err(e) => {
                    tracing::warn!("skipping unreadable reminder: {}", error_message(&e));
                    continue;
                }
            };
            // A row with no `next_due_at` has no measurable age, so neither
            // "held" nor "escalated" can be answered honestly. The writer cannot
            // produce one (Task 9 rejects a NULL next_due_at), but `list_due`'s
            // `next_due_at <= $now` does select a NULL in SurrealQL, so an admin
            // path or migration can put one here: name it and skip it rather than
            // reporting `due_at: null` or — at `escalation_hours: 0` — escalating
            // a reminder that was never due. This also makes `r.next_due_at` a
            // real datetime for the rest of the loop, which is what the delivery
            // claim below compares against.
            let Some(due_at) = r.next_due_at else {
                tracing::warn!("skipping reminder {id} with no next_due_at: {}", r.message);
                continue;
            };
            let recurring = !matches!(spec, sched::ScheduleSpec::Once { .. });

            let escalated = match &r.target_project {
                None => false,
                Some(target) => {
                    // Byte-exact and case-sensitive on purpose: nothing
                    // normalizes either side, and the other side is Task 14's pi
                    // companion, whose hint is `basename(git rev-parse
                    // --show-toplevel)` (overridable with
                    // ALEXANDRIA_REMINDERS_PROJECT). So "Alexandria" vs
                    // "alexandria", a worktree directory name, or a trailing
                    // space is not an error — the symptom is a targeted reminder
                    // that arrives late with `escalated: true` instead of
                    // surfacing in its own project.
                    if params.project.as_deref() == Some(target.as_str()) {
                        false
                    } else if now - due_at < escalation {
                        continue; // held for a matching context
                    } else {
                        true
                    }
                }
            };

            // Occurrences strictly after the stored due time and up to now: the
            // occurrence being delivered *is* the stored one, so it is excluded.
            // The count saturates at the engine's `MAX_ITER` iteration bound
            // (10 000, `alexandria_engine::reminders::occurrences_between`): a
            // schedule abandoned long enough to exceed it reports the cap, not a
            // wrong-but-larger number.
            let missed = if recurring {
                match sched::occurrences_between(&spec, due_at, now, tz) {
                    Ok(missed) => missed,
                    Err(e) => {
                        tracing::warn!(
                            "skipping reminder {id} while coalescing missed fires: {}",
                            error_message(&e)
                        );
                        continue;
                    }
                }
            } else {
                0
            };
            // None on a recurring spec means it has no future fire left; it is
            // delivered once more and consumed rather than left due forever.
            let new_next = if recurring {
                match sched::next_fire(&spec, now, tz) {
                    Ok(next) => next,
                    Err(e) => {
                        tracing::warn!(
                            "skipping reminder {id} while advancing its schedule: {}",
                            error_message(&e)
                        );
                        continue;
                    }
                }
            } else {
                None
            };
            // The one step that cannot be redone, which is why it is a claim
            // rather than an update: the write only lands if the row is still
            // pending with the exact `next_due_at` this call read from `list_due`
            // (`r.next_due_at`, non-NULL here because of the skip above). A failed
            // write leaves the row pending, so reporting it anyway would
            // double-deliver while propagating the error would throw away the rows
            // already consumed above; a lost claim means another check consumed it
            // first or the user cancelled it, which is an expected outcome and
            // worth a debug line at most. Either way: skip the row, keep the rest.
            match repo.record_delivery(&id, new_next, r.next_due_at).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(
                        "reminder {id} was consumed or cancelled elsewhere; skipping it"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!("failed to consume reminder {id}: {}", error_message(&e));
                    continue;
                }
            }

            delivered.push(serde_json::json!({
                "id": id,
                "message": r.message,
                "target": match &r.target_project {
                    Some(p) => format!("project:{p}"),
                    None => "global".to_string(),
                },
                "escalated": escalated,
                "recurring": recurring,
                // Saturated count: see the `MAX_ITER` bound noted where it is computed.
                "missed_occurrences": missed,
                "due_at": rfc3339_utc(due_at),
                "schedule": sched::human_readable(&spec),
                "note": r.note,
                "provenance": {
                    "project": r.prov_project,
                    "session_id": r.prov_session_id,
                },
                "next_due_at": new_next.map(rfc3339_utc),
            }));
        }

        // There is no background timer, so this line is the only record that the
        // server saw a reminder at all. Checks run per interaction and mostly
        // find nothing, which stays at debug; an actual delivery is worth an
        // info-level entry (matching the startup info in main.rs).
        let delivered_count = delivered.len();
        if delivered_count > 0 {
            tracing::info!(
                due = due_count,
                delivered = delivered_count,
                project = ?params.project,
                "check_reminders"
            );
        } else {
            tracing::debug!(
                due = due_count,
                delivered = 0,
                project = ?params.project,
                "check_reminders"
            );
        }

        Ok(serde_json::json!({
            "count": delivered_count,
            "delivered": delivered,
        })
        .to_string())
    }

    /// Report the reminders matching a status/project filter, without consuming
    /// or changing anything. This is the management view — what is scheduled, and
    /// the place an ID is found before cancelling it.
    ///
    /// Unlike delivery, nothing here is filtered out for being unreadable: a row
    /// whose schedule cannot be reconstructed is still owed to the user (or to an
    /// operator cleaning up after a migration), so it is listed with the failure
    /// named in its `schedule` field. Only that one row degrades — one corrupt
    /// row must not hide every healthy reminder behind it.
    ///
    /// Rows carry the fields the set and deliver responses established — the
    /// local rendering of `next_due_at` and the envelope's `timezone` from set,
    /// `recurring` and `provenance` from deliver — so the management view answers
    /// "what is scheduled, and when does it fire in my time?" on its own terms
    /// rather than a narrower subset of them.
    pub async fn do_list_reminders(&self, params: ListRemindersParams) -> anyhow::Result<String> {
        use alexandria_engine::reminders as sched;
        use alexandria_storage::models::schedule_kind;
        use alexandria_storage::repos::ReminderRepo;

        // `pending` is the default because it is what "what reminders do I have?"
        // means; `all` is the one spelling of "no filter". Any other value is
        // passed through to the repo, whose `status = $status` simply matches no
        // row for a status that does not exist.
        let status = match params.status.as_deref() {
            None | Some("pending") => Some("pending"),
            Some("all") => None,
            s @ Some(_) => s,
        };
        let tz = self.reminders.tz;
        let repo = ReminderRepo::new(self.db.inner());
        let rows = repo.list(status, params.target_project.as_deref()).await?;

        let reminders: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let spec = sched::spec_from_reminder(r);
                serde_json::json!({
                    "id": r.id.as_ref().map(record_id_to_string).unwrap_or_default(),
                    "message": r.message,
                    "target": match &r.target_project {
                        Some(p) => format!("project:{p}"),
                        None => "global".to_string(),
                    },
                    "status": r.status,
                    "schedule": spec
                        .as_ref()
                        .map(sched::human_readable)
                        .unwrap_or_else(|e| format!("<invalid schedule: {e}>")),
                    "next_due_at": r.next_due_at.map(rfc3339_utc),
                    // The bridge `do_set_reminder` gives: `schedule` renders a
                    // recurring reminder in local wall clock while `next_due_at`
                    // is UTC, so a reader needs the local spelling of that instant
                    // to recognise its own schedule.
                    "next_due_at_local": r
                        .next_due_at
                        .map(|d| d.with_timezone(&tz).format("%Y-%m-%d %H:%M %Z").to_string()),
                    // From the stored discriminator, not the reconstructed spec: a
                    // corrupt row has no spec to ask, and it is still owed an
                    // answer. For every readable row this is what
                    // `do_check_reminders` reports.
                    "recurring": matches!(
                        r.schedule_kind.as_str(),
                        schedule_kind::PATTERN | schedule_kind::CRON
                    ),
                    "delivered_count": r.delivered_count,
                    "last_delivered_at": r.last_delivered_at.map(rfc3339_utc),
                    "note": r.note,
                    "provenance": {
                        "project": r.prov_project,
                        "session_id": r.prov_session_id,
                    },
                    "created_at": r.created_at.map(rfc3339_utc),
                })
            })
            .collect();

        Ok(serde_json::json!({
            "count": reminders.len(),
            // The zone every `next_due_at_local` above is rendered in: without it
            // a reader cannot tell whether the local times are theirs.
            "timezone": tz.name().to_string(),
            "reminders": reminders,
        })
        .to_string())
    }

    /// Retire a reminder by ID.
    ///
    /// A soft cancel: the row keeps existing (under `status = 'cancelled'`,
    /// visible in `do_list_reminders`) because it is the only record that the
    /// reminder was ever set. Every other status is cancellable — a delivered
    /// reminder can be retired, which is a success rather than an error — but a
    /// row that is already cancelled is a true no-op, because
    /// `ReminderRepo::cancel` stamps `cancelled_at` unconditionally and
    /// re-issuing that write would move the record of *when* the reminder was
    /// retired while still reporting the success of the first cancel. Only an ID
    /// that matches no row is a failure — that is a typo or a stale reference,
    /// and silently "cancelling" nothing would tell the user their reminder was
    /// taken care of when it was not.
    pub async fn do_cancel_reminder(&self, params: CancelReminderParams) -> anyhow::Result<String> {
        use alexandria_storage::repos::ReminderRepo;

        let repo = ReminderRepo::new(self.db.inner());
        let Some(existing) = repo.get(&params.id).await? else {
            anyhow::bail!("Reminder not found: {}", params.id);
        };
        // Read-only exit for the already-cancelled row: the response is the same
        // success a first cancel returned, because from the caller's side nothing
        // is left to do.
        if existing.status == "cancelled" {
            return Ok(serde_json::json!({ "status": "ok", "id": params.id }).to_string());
        }
        repo.cancel(&params.id).await?;
        Ok(serde_json::json!({ "status": "ok", "id": params.id }).to_string())
    }

    // --- Internal helpers ---

    /// Read-only due-reminder summary for piggybacking onto memory responses
    /// (`retrieve_memories`, `recall`), so an agent that never calls
    /// `check_reminders` still sees that something came due.
    ///
    /// Never consumes: due-ness is a query-time predicate, so listing leaves the
    /// row pending and `check_reminders` keeps sole ownership of every delivery
    /// decision (targeting, escalation, occurrence coalescing, the claim write).
    /// Capped, oldest-due first (`list_due` orders by `next_due_at ASC`), to
    /// bound how much noise an unrelated memory lookup carries.
    ///
    /// Fail-open: a reminder-layer error degrades to an empty array rather than
    /// breaking retrieval, because a broken reminder table must not take the
    /// memory tools down with it.
    async fn due_reminders_summary(&self, cap: usize) -> Vec<serde_json::Value> {
        use alexandria_storage::repos::ReminderRepo;

        let repo = ReminderRepo::new(self.db.inner());
        match repo.list_due(Utc::now()).await {
            Ok(rows) => rows
                .into_iter()
                .take(cap)
                .map(|r| {
                    serde_json::json!({
                        "id": r.id.as_ref().map(record_id_to_string).unwrap_or_default(),
                        "message": r.message,
                        "target": match &r.target_project {
                            Some(p) => format!("project:{p}"),
                            None => "global".to_string(),
                        },
                        // A NULL `next_due_at` renders as JSON null: the only
                        // writer cannot produce one (Task 9 refuses a schedule
                        // with no future fire), but `list_due`'s `next_due_at <=
                        // $now` does select such a row, so an admin/migration
                        // path can. It is reported rather than hidden — like the
                        // corrupt rows in `do_list_reminders`.
                        "due_at": r.next_due_at.map(rfc3339_utc),
                    })
                })
                .collect(),
            Err(e) => {
                tracing::warn!("due_reminders piggyback failed: {}", error_message(&e));
                Vec::new() // fail-open: never break retrieval over reminders
            }
        }
    }

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

    /// Create a raw record for document import.
    async fn create_raw_record(&self, content: &str) -> anyhow::Result<String> {
        let mut response = self
            .db
            .inner()
            .query("CREATE raw SET content = $content, deleted = false")
            .bind(("content", content.to_string()))
            .await?;
        let created: Option<alexandria_storage::models::RawRecord> = response.take(0)?;
        let raw = created.ok_or_else(|| anyhow::anyhow!("Failed to create raw record"))?;
        let id = raw
            .id
            .ok_or_else(|| anyhow::anyhow!("Raw record has no id"))?;
        Ok(record_id_to_string(&id))
    }

    async fn load_cluster_infos(&self) -> anyhow::Result<Vec<ClusterInfo>> {
        let mut response = self.db.inner().query("SELECT * FROM cluster").await?;
        let clusters: Vec<alexandria_storage::models::Cluster> = response.take(0)?;

        let cluster_repo = ClusterRepo::new(self.db.inner());
        let mut infos = Vec::with_capacity(clusters.len());

        for c in clusters {
            let id = c.id.map(|r| record_id_to_string(&r)).unwrap_or_default();
            let member_count = cluster_repo
                .get_members(&id)
                .await
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
mod error_message_tests {
    use super::error_message;

    /// `{e:#}` keeps the underlying cause — here the cron crate's field-range
    /// complaint, which `e.to_string()` drops — and the whitespace collapse
    /// keeps the JSON tool response on one line (the cause renders a caret
    /// diagram across several lines).
    #[test]
    fn flattens_chain_and_collapses_whitespace() {
        let err = alexandria_engine::reminders::normalize_cron("61 99 * * *").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid cron expression \"61 99 * * *\"",
            "top-level context alone must not be what the user sees"
        );

        let msg = error_message(&err);
        assert!(
            msg.contains("invalid cron expression \"61 99 * * *\""),
            "message lost its context: {msg}"
        );
        assert!(
            msg.contains("Minutes must be less than 59"),
            "message lost the underlying cause: {msg}"
        );
        assert!(
            !msg.contains('\n') && !msg.contains("  "),
            "message must stay on one line: {msg:?}"
        );
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
            })
            .await
            .unwrap();
        server
            .do_store_memory(StoreMemoryParams {
                content: "a far away memory".to_string(),
                tags: None,
                session_id: None,
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
            })
            .await
            .unwrap();
        server
            .do_store_memory(StoreMemoryParams {
                content: "just above the floor".to_string(),
                tags: None,
                session_id: None,
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
            })
            .await
            .unwrap();
        let gone = server
            .do_store_memory(StoreMemoryParams {
                content: "deleted fact".to_string(),
                tags: None,
                session_id: Some("sess-del".to_string()),
            })
            .await
            .unwrap();
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
            })
            .await
            .unwrap();
        let _id2 = server
            .do_store_memory(StoreMemoryParams {
                content: "second session fact".to_string(),
                tags: Some(vec!["important".to_string()]),
                session_id: Some("sess-abc".to_string()),
            })
            .await
            .unwrap();

        // Also store a memory outside the session
        let _id3 = server
            .do_store_memory(StoreMemoryParams {
                content: "unrelated fact".to_string(),
                tags: None,
                session_id: None,
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
        // The four reminder tools were converted to `tool_json` after this test
        // was written, so they are the ones most likely to drift back — every
        // `do_*` here returns a JSON string by construction and must keep
        // mirroring it into structuredContent.
        let set = server
            .set_reminder(Parameters(SetReminderParams {
                message: "water the plants".to_string(),
                due_at: Some("2030-01-01T12:00:00Z".to_string()),
                pattern: None,
                cron: None,
                target_project: Some("struct".to_string()),
                prov_project: None,
                session_id: None,
                note: None,
            }))
            .await;
        check_structured("set_reminder", &set);
        let reminder_id = set.structured_content.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        check_structured(
            "check_reminders",
            &server
                .check_reminders(Parameters(CheckRemindersParams {
                    project: Some("struct".to_string()),
                }))
                .await,
        );
        check_structured(
            "list_reminders",
            &server
                .list_reminders(Parameters(ListRemindersParams {
                    status: None,
                    target_project: None,
                }))
                .await,
        );
        check_structured(
            "cancel_reminder",
            &server
                .cancel_reminder(Parameters(CancelReminderParams { id: reminder_id }))
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

        // The error arm flattens the whole anyhow chain, not just the top-level
        // context: reminder validation wraps the parser's reason, and
        // `to_string()` would drop it.
        let bad_cron = server
            .set_reminder(Parameters(SetReminderParams {
                message: "x".to_string(),
                due_at: None,
                pattern: None,
                cron: Some("61 99 * * *".to_string()),
                target_project: None,
                prov_project: None,
                session_id: None,
                note: None,
            }))
            .await;
        assert_eq!(bad_cron.is_error, Some(true));
        let msg = bad_cron.structured_content.expect("structuredContent set")["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(msg.contains("61 99 * * *"), "must echo the input: {msg}");
        assert!(
            msg.contains("Minutes must be less than 59"),
            "must carry the cron crate's reason: {msg}"
        );
    }
}
