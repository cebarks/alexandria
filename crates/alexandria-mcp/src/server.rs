use std::collections::HashMap;
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

use alexandria_engine::clusters::maintenance::DEFAULT_COHESION_FLOOR;
use alexandria_engine::clusters::{
    ClusterAssignment, ClusterInfo, assign_to_cluster, update_centroid,
};
use alexandria_engine::heat::{
    ActivationConfig, DEFAULT_DECAY_TAU_SECS, DEFAULT_SPACING_REFERENCE_SECS,
    HeatState as EngineHeatState, compute_activation_targets, on_access,
};
use alexandria_engine::recall::{
    ClusterWithMembers, FactSummary, RecallHandle, broad_recall, focused_recall,
};
use alexandria_engine::search::{DEFAULT_MIN_SIMILARITY, rank_by_similarity};
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::Database;
use alexandria_storage::access::{AccessRepo, OwnerContext, ScopedContext};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::identity::{FactId, OperationId};
use alexandria_storage::models::mutation::{
    DestinationMembership, EdgeKind, ExpectedState, FactChange, MoveReconciliation, RelationChange,
    RelationTarget, Visibility, effect,
};
use alexandria_storage::models::{
    Cluster, ClusterVersion, MemoryScope, ReadSelection, Version, operation_kind,
};
use alexandria_storage::project_handle::format_project_handle;
use alexandria_storage::repos::{
    ClusterRepo, EdgeRepo, HeatRepo, HeatUpdate, MemoryRepo, MutationRepo, RetrievalStrategy,
    ScopeFilter, ScopedClusterRepo, ScopedGraphReader, ScopedHeatWriter, ScopedMemoryRepo,
    ScopedReminderRepo, ScopedSessionRepo, SessionPageQuery, SessionProjection, SessionRepo,
};
use chrono::{DateTime, SecondsFormat, Utc, Weekday};
use rmcp::RoleServer;
use rmcp::service::RequestContext;
use surrealdb::types::RecordId;

use crate::mutation::{ApplyService, ApprovedCluster, Plan};
use crate::scope_service::{ReadScope, ScopeService};
use crate::tools::{
    CancelReminderParams, CheckRemindersParams, DeleteMemoryParams, FinalizeSessionParams,
    GetSessionParams, ImportDocumentParams, ListProjectsParams, ListRemindersParams,
    ListSessionsParams, ListUnassignedMemoriesParams, ProposeMemoryScopeParams, RecallParams,
    ResolveProjectParams, RetrieveMemoriesParams, SetReminderParams, StoreMemoryParams,
    UpdateMemoryParams,
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

/// `[reminders]` defaults the MCP layer falls back to when no config was
/// injected. Named once and shared with the binary's `Default for
/// RemindersConfig` so the two cannot drift the way `min_similarity` once did
/// (see AGENTS.md); `tz` is deliberately UTC rather than system-local because
/// this crate has no `iana-time-zone` dependency — `main.rs` resolves the
/// operator's zone and injects it for both transports, and the fallback here is
/// the *test/debug* default, which must be deterministic.
pub const DEFAULT_REMINDER_ESCALATION_HOURS: u64 =
    alexandria_engine::reminders::DEFAULT_ESCALATION_HOURS;

/// Rows `list_reminders` returns when the caller asks for no specific page size.
/// Bounded because the history is append-only (delivered and cancelled rows are
/// kept deliberately), and every row that comes back is context the caller pays
/// for.
pub const DEFAULT_LIST_REMINDERS_LIMIT: i64 = 50;

/// Upper bound on a `list_reminders` page, so a caller cannot ask for the whole
/// history by passing an enormous `limit`.
pub const MAX_LIST_REMINDERS_LIMIT: i64 = 500;

/// Rows the read-only `due_reminders` piggyback shows per retrieve/recall
/// response. One constant because four response shapes carry the field.
pub const DUE_REMINDERS_CAP: i64 = 5;

/// Extra rows read beyond the cap, so that undeliverable rows cannot starve the
/// sample. Whether a row is renderable is only knowable in Rust (the schedule has
/// to be reconstructed), while the `LIMIT` has to be in SQL to bound the read — so
/// the two disagree by construction, and a corrupt row would otherwise spend one of
/// five informational slots for as long as an operator leaves it pending. The slack
/// is a bounded read of `cap + slack`, not a full scan.
const DUE_SAMPLE_SLACK: i64 = 10;

/// Ceiling on `retrieve_memories`' `limit`. With `KNN_SLACK` it keeps `k` under the HNSW
/// search breadth (`HNSW_EF` in `memory_repo.rs`).
pub const MAX_RETRIEVE_LIMIT: usize = 100;

/// Extra neighbours requested beyond `limit`, so an approximate index result still holds the
/// exact top `limit` after the in-process re-rank.
const KNN_SLACK: usize = 10;

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
            escalation_hours: DEFAULT_REMINDER_ESCALATION_HOURS,
        }
    }
}

/// Storage row -> engine state. The engine counts seconds since an epoch and storage keeps UTC
/// datetimes, so the conversion lives where the two crates meet — `alexandria-storage` does not
/// depend on `alexandria-engine` (by design: storage is DB access, engine is pure algorithms), and
/// a `From` impl is impossible from here anyway under the orphan rules.
///
/// `last_accessed_at` is `NONE` only on rows that predate v008; the decay anchor is then the best
/// available bound on the true last access. Stated once so no call site re-implements the fallback
/// differently.
fn heat_engine_state(row: &alexandria_storage::models::HeatState) -> EngineHeatState {
    let seconds = |value: Option<DateTime<Utc>>| value.map(|t| t.timestamp().max(0) as u64);
    let last_touched = seconds(row.last_touched).unwrap_or(0);
    EngineHeatState {
        heat: row.heat,
        stability: row.stability,
        last_touched,
        last_accessed_at: seconds(row.last_accessed_at).unwrap_or(last_touched),
        access_count: row.access_count.max(0) as u64,
    }
}

/// The two heat time constants, as configured.
///
/// Defaults come from the engine's own constants, so a server built without a config file (debug,
/// tests) decays exactly as production does at default — the same rule `RemindersSettings` and
/// `retrieve_min_similarity` follow, and the `min_similarity` 0.10-vs-0.30 drift is what it
/// prevents. See also `HeatConfig` in the binary crate, whose parity test keeps the two in step.
#[derive(Debug, Clone, Copy)]
pub struct HeatSettings {
    /// Base decay time constant, seconds. `tau = stability * this`; lower cools faster.
    pub decay_tau_secs: f64,
    /// Access gap, seconds, at which an access earns full stability growth.
    pub spacing_reference_secs: f64,
}

impl Default for HeatSettings {
    fn default() -> Self {
        Self {
            decay_tau_secs: DEFAULT_DECAY_TAU_SECS,
            spacing_reference_secs: DEFAULT_SPACING_REFERENCE_SECS,
        }
    }
}

/// Request-local ranking labels never double as serialized database identities.
struct DestinationCandidates {
    infos: Vec<ClusterInfo>,
    identities: Vec<RecordId>,
}

impl DestinationCandidates {
    fn from_clusters(clusters: Vec<(Cluster, usize)>) -> Result<Self, StorageError> {
        let mut infos = Vec::with_capacity(clusters.len());
        let mut identities = Vec::with_capacity(clusters.len());
        for (index, (cluster, member_count)) in clusters.into_iter().enumerate() {
            identities.push(cluster.id.ok_or_else(|| {
                StorageError::InvalidInput(
                    "a destination candidate has no typed identity".to_string(),
                )
            })?);
            infos.push(ClusterInfo {
                id: index.to_string(),
                centroid: cluster.centroid,
                member_count,
            });
        }
        Ok(Self { infos, identities })
    }

    fn choose(&self, embedding: &[f32], threshold: f32) -> Result<Option<&RecordId>, StorageError> {
        match assign_to_cluster(embedding, &self.infos, threshold) {
            ClusterAssignment::NewCluster => Ok(None),
            ClusterAssignment::Existing(token) => self
                .infos
                .iter()
                .position(|info| info.id == token)
                .and_then(|index| self.identities.get(index))
                .map(Some)
                .ok_or_else(|| {
                    StorageError::InvalidInput(
                        "ranked cluster was not among the authorized candidates".to_string(),
                    )
                }),
        }
    }
}

#[cfg(test)]
#[path = "server/destination_candidates_tests.rs"]
mod destination_candidates_tests;

#[derive(Clone)]
pub struct AlexandriaServer {
    pub db: Arc<Database>,
    pub embedding: Arc<dyn EmbeddingProvider>,
    pub cluster_join_threshold: f32,
    /// Both heat knobs, as one value. Passed as a struct rather than two adjacent positional
    /// `f64`s: a transposed pair of same-typed floats is compiler-invisible and would silently
    /// change how fast memories cool.
    pub heat: HeatSettings,
    pub activation_config: ActivationConfig,
    pub activation_top_n: usize,
    /// Hard floor on cosine similarity for retrieve_memories results. The
    /// builder default matches `RetrieveConfig`; production overrides it from config.
    pub retrieve_min_similarity: f32,
    /// Avg member-to-centroid similarity below which a cluster is reported as needing a
    /// split. Carried here so the debug UI shows the same verdict the maintenance task acts on.
    pub cohesion_floor: f32,
    pub reminders: RemindersSettings,
    /// Whether `schema::ensure_vector_index` succeeded at boot. Off, retrieval stays on the
    /// brute-force KNN form, which returns the same rows without the index.
    pub vector_index: bool,
    /// The identity a stdio transport acts as, when the operator configured one.
    ///
    /// Task 5's transport seam, and narrow on purpose: the value can only have been issued by
    /// `AccessRepo::local_principal`, which needs this database instance's startup capability plus an
    /// account that exists and is active in this store, so holding the field is not a way to mint an
    /// identity and a hosted request never consults it. It is carried here because the stdio path has no
    /// HTTP `Parts` to read an admitted principal from — and it stays unused until Task 6's `do_*`
    /// services take a context, which is where an absent identity starts meaning a refusal rather than a
    /// legacy passthrough.
    pub stdio_identity: Option<crate::auth::StdioIdentity>,
}

impl AlexandriaServer {
    pub fn new(
        db: Arc<Database>,
        embedding: Arc<dyn EmbeddingProvider>,
        cluster_join_threshold: f32,
        heat: HeatSettings,
    ) -> Self {
        Self {
            db,
            embedding,
            cluster_join_threshold,
            heat,
            activation_config: ActivationConfig::default(),
            activation_top_n: 3,
            retrieve_min_similarity: DEFAULT_MIN_SIMILARITY,
            cohesion_floor: DEFAULT_COHESION_FLOOR,
            reminders: RemindersSettings::default(),
            vector_index: false,
            stdio_identity: None,
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

    pub fn with_cohesion_floor(mut self, cohesion_floor: f32) -> Self {
        self.cohesion_floor = cohesion_floor;
        self
    }

    pub fn with_reminders_config(mut self, settings: RemindersSettings) -> Self {
        self.reminders = settings;
        self
    }

    pub fn with_vector_index(mut self, defined: bool) -> Self {
        self.vector_index = defined;
        self
    }

    /// Attaches the trusted-local identity this process serves as, on a stdio transport.
    ///
    /// See the field: the only producer of the value is storage's local-principal path.
    pub fn with_stdio_identity(mut self, identity: crate::auth::StdioIdentity) -> Self {
        self.stdio_identity = Some(identity);
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
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        delete_result(self.delete_core(&ctx, &params.id).await)
    }

    #[tool(
        description = "Persist a durable fact, decision, preference, or correction so future sessions/agents can recall it. Call this proactively whenever you learn something worth remembering — a user preference, an architectural decision and its rationale, a resolved bug's root cause, a gotcha you just discovered — not only when explicitly told to 'remember this'. Cheap and idempotent-ish (dedup happens via clustering); prefer storing over losing context. Write content as a standalone statement that makes sense without the current conversation. If the response carries `truncated: true`, the content was longer than the embedding sees and only its head is searchable: split it into shorter memories."
    )]
    async fn store_memory(
        &self,
        Parameters(params): Parameters<StoreMemoryParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        // Task 8 replaced D4's blanket hosted refusal with a resolved target and kept the one property D4
        // was established for: an untargeted hosted store is refused **before** the overflow count (which
        // is provider work on the embedded text), before `embed`, and before any row exists. A session
        // handle is still not a target and is not consulted. Resolution is a workspace-qualified read of
        // the caller's own project rows, so what a refusal pays for is the lookup that proves it was
        // unanswerable — and a trusted-local principal remains the account every row below is stamped
        // with, which is the difference between storing a memory and storing one somebody can retrieve.
        let (scope, partition) = match self
            .admit_write_target(
                &ctx,
                params.scope.as_deref(),
                params.project.as_deref(),
                params.context_project.as_deref(),
            )
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => return tool_json(Err(anyhow::Error::new(error))),
        };
        self.store_response(&scope, partition, params).await
    }

    /// The store path's response assembly: overflow counting, the store, and the truncation warning that
    /// only makes sense beside the id of the row it describes.
    ///
    /// Split from the `#[tool]` handler because the handler also resolves the request's authority from a
    /// `RequestContext<RoleServer>`, whose `Peer` rmcp builds only for a live service. The truncation,
    /// off-thread-offload and counting-panic cases are about this code rather than about admission, so they
    /// are tested here instead of being reconstructed through a transport a unit test cannot build.
    ///
    /// The scope is required, not optional, and so is the partition: `admit_write_target` resolved both
    /// from the request, and there is no arm here that infers a target from whatever happened to be
    /// granted — which is the inference D4 and Task 8 both exist to remove.
    pub(crate) async fn store_response(
        &self,
        scope: &RequestScope,
        partition: MemoryScope,
        params: StoreMemoryParams,
    ) -> CallToolResult {
        self.store_response_body(Some(scope), Some(partition), params)
            .await
    }

    /// The interim store wrapped the way the handler wraps it. Test-only: the accepted response-shape
    /// tests (truncation, off-thread overflow counting, the counting panic) are its only caller, and
    /// `store_memory` itself uses [`Self::store_response`].
    #[cfg(test)]
    pub(crate) async fn store_response_interim(&self, params: StoreMemoryParams) -> CallToolResult {
        self.store_response_body(None, None, params).await
    }

    async fn store_response_body(
        &self,
        scope: Option<&RequestScope>,
        partition: Option<MemoryScope>,
        params: StoreMemoryParams,
    ) -> CallToolResult {
        let reported = partition.as_ref().map(scope_label);
        // Asked before the store because the id does not exist until after it, and the id is
        // what makes the warning actionable.
        let embedding = Arc::clone(&self.embedding);
        let content = params.content.clone();
        let overflow = match tokio::task::spawn_blocking(move || embedding.overflow(&content)).await
        {
            Ok(overflow) => overflow,
            Err(error) => {
                return CallToolResult::structured_error(serde_json::json!({
                    "status": "error",
                    "message": format!("Embedding overflow counting failed: {error}")
                }));
            }
        };
        match self.store_body(scope, partition, params).await {
            Ok(id) => {
                // Where the row actually landed, as the server's answer rather than an echo of the
                // request: a client can confirm a partition without reading the fact back.
                let mut body = serde_json::json!({
                    "status": "ok",
                    "id": id,
                    "scope": reported.unwrap_or_else(|| "unscoped".to_string()),
                });
                if let Some(tokens) = overflow {
                    tracing::warn!(
                        fact_id = %id,
                        tokens,
                        "stored memory exceeds the embedding token limit; only its head is searchable"
                    );
                    body["truncated"] = true.into();
                    body["tokens"] = tokens.into();
                }
                CallToolResult::structured(body)
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
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        // The scope is resolved *before* the population is admitted, because "which partitions" is a
        // question about the caller's own project rows. `auto` with no project is the shape automatic
        // recall has always had; anything the caller names that is not theirs refuses here, in the same
        // indistinguishable way a later read would.
        let owner = match self.admit_owner(&ctx).await {
            Ok(owner) => owner,
            Err(error) => return refusal(error),
        };
        let (selection, selection_label) = match ScopeService::new(&self.db)
            .resolve_read_selection(
                &owner,
                named(params.scope.as_deref()),
                named(params.project.as_deref()),
            )
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => return refusal(error),
        };
        let scope = match self.admit_read(&owner, selection).await {
            Ok(scope) => scope,
            Err(error) => return refusal(error),
        };
        // `structured()` also puts the same JSON in a text block, so clients that
        // only read `content[].text` (Pi extension, Claude hook) are unaffected.
        match self.retrieve_scoped(&scope, params, RETRIEVE_TOOL).await {
            Ok(mut results) => {
                // The selection that actually ran, reported on the tool's own response only: the debug
                // wrappers share `retrieve_core` and have no admitted selection to name.
                report_selection(&mut results, selection_label);
                CallToolResult::structured(results)
            }
            Err(e) => CallToolResult::structured_error(
                serde_json::json!({ "status": "error", "message": e.to_string() }),
            ),
        }
    }

    #[tool(
        description = "Progressive two-phase recall for open-ended or broad questions ('what do we know about X', 'what's the state of Y'): first call with no scope_handle to get candidate clusters, then call again with the returned scope_handle to narrow into the most relevant one. Prefer this over retrieve_memories when the query is exploratory rather than a specific lookup."
    )]
    async fn recall(
        &self,
        Parameters(params): Parameters<RecallParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        // Recall has two leak surfaces the unscoped path did not close: the candidate list, whose
        // cluster **labels** and member counts describe what another account stored, and the
        // `scope_handle` the client echoes back, which is an unauthenticated string and was replayed
        // against whichever cluster it named. Both are resolved through the caller's own account here.
        let owner = match self.admit_owner(&ctx).await {
            Ok(owner) => owner,
            Err(error) => return refusal(error),
        };
        let (selection, selection_label) = match ScopeService::new(&self.db)
            .resolve_read_selection(
                &owner,
                named(params.scope.as_deref()),
                named(params.project.as_deref()),
            )
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => return refusal(error),
        };
        let scope = match self.admit_read(&owner, selection).await {
            Ok(scope) => scope,
            Err(error) => return refusal(error),
        };
        match self.recall_scoped(&scope, selection_label, &params).await {
            Ok(body) => tool_json(Ok(body)),
            Err(error) => tool_json(Err(error)),
        }
    }

    #[tool(
        description = "Correct or refine an existing memory in place (content, tags, or confidence) instead of storing a duplicate. Content changes trigger re-embedding and preserve the old version via a derived_from lineage edge. Use this the moment you discover a previously stored memory is stale or wrong."
    )]
    async fn update_memory(
        &self,
        Parameters(params): Parameters<UpdateMemoryParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        tool_json(self.do_update_memory(&ctx, params).await)
    }

    #[tool(
        description = "Bulk-load a document (design doc, README, spec, meeting notes, etc.) into memory as one or many chunked entries with lineage back to the source. Use this whenever the user shares or points at reference material worth retaining long-term, not just when asked to 'import' something."
    )]
    async fn import_document(
        &self,
        Parameters(params): Parameters<ImportDocumentParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        // D4's ordering, now with a resolved target. Import is the expensive case: untargeted it would
        // embed a whole batch, write a `raw` document, chunk rows and `extracted_from` edges, so
        // refusing after the first chunk would be the costlier mistake, not the smaller one.
        let (scope, partition) = match self
            .admit_write_target(
                &ctx,
                params.scope.as_deref(),
                params.project.as_deref(),
                params.context_project.as_deref(),
            )
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => return tool_json(Err(anyhow::Error::new(error))),
        };
        tool_json(self.import_scoped(&scope, partition, params).await)
    }

    /// The interim import wrapped the way the handler wraps it. Test-only: the accepted response-shape
    /// tests are its only caller, and `import_document` itself uses [`Self::import_scoped`].
    #[cfg(test)]
    pub(crate) async fn import_response(&self, params: ImportDocumentParams) -> CallToolResult {
        tool_json(self.do_import_document_interim(params).await)
    }

    #[tool(
        description = "Retrieve a session and the memories stored during it. Use this to review what happened in a specific session — 'the session from yesterday', 'what did we learn during the auth refactor'. Which partitions the memories are read from is derived from `project` (one of your projects) or, when you name none, your personal partition; the session's own `summary` and `tags` are withheld because they are account-private prose, and `memory_count` counts only memories inside that population. Pass `view: \"archive\"` for the deliberate whole-session read of your own history, prose included. Naming a session id is not by itself archive access."
    )]
    async fn get_session(
        &self,
        Parameters(params): Parameters<GetSessionParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        // Admission first: the principal comes from the transport, and the bound account is what every
        // statement below carries. There is no default population and no store-wide fallback — and the
        // *view* decides which population the caller is entitled to, which is why it is resolved before
        // any read rather than applied to the rows afterwards.
        let (scope, view) = match self
            .admit_session_view(&ctx, params.view.as_deref(), params.project.as_deref())
            .await
        {
            Ok(admitted) => admitted,
            Err(error) => return refusal(error),
        };
        tool_json(self.session_detail(&scope, &params.session_id, view).await)
    }

    #[tool(
        description = "List your sessions newest-first with live memory counts. Call this when you need to find a session whose id you don't have — 'the session from yesterday', 'what did the pi agent work on' — then pass its external_id to get_session. Filters: `agent_id` and `finalized` (true = closed with a summary) always available; `tag` only with `view: \"archive\"`, because the tag set is account-private prose the default scoped page does not expose. Membership and counts are limited to the population `project` selects (your personal partition when you name none)."
    )]
    async fn list_sessions(
        &self,
        Parameters(params): Parameters<ListSessionsParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (scope, view) = match self
            .admit_session_view(&ctx, params.view.as_deref(), params.project.as_deref())
            .await
        {
            Ok(admitted) => admitted,
            Err(error) => return refusal(error),
        };
        tool_json(self.session_list(&scope, &params, view.archive()).await)
    }

    #[tool(
        description = "Finalize a session by setting its summary, tags, and ended_at timestamp. Call this when a session wraps up to capture a summary of what was accomplished."
    )]
    async fn finalize_session(
        &self,
        Parameters(params): Parameters<FinalizeSessionParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let scope = match self.admit(&ctx, ReadSelection::Auto(None)).await {
            Ok(scope) => scope,
            Err(error) => return refusal(error),
        };
        tool_json(self.session_finalize(&scope, &params).await)
    }

    #[tool(
        description = "Schedule a reminder to be delivered on a future interaction — one-shot (due_at) or recurring (pattern/cron). Use this when the user asks to be reminded of something later, when they say 'remind me', 'don't let me forget', or when a follow-up action will be needed at a specific time ('check the deploy at 3pm', 'ping me about this tomorrow morning'). Delivery happens on the next check_reminders call after the row comes due — the server runs no timer and reaches no one until some client asks. That call injects the reminder into the agent's context; clients that also surface it to the human do so themselves (the pi companion notifies, a client with no UI does not), so never tell the user a notification was sent. Targets and check contexts must be owned project IDs, never names or checkout basenames. Valid project-targeted reminders escalate within your account once at least escalation_hours overdue; held or unresolved legacy targets stay pending and suppressed. The response includes the next fire times — confirm them with the user when the schedule was parsed from natural language, and read the response's `warning` field before you end the turn."
    )]
    async fn set_reminder(
        &self,
        Parameters(params): Parameters<SetReminderParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        // Reminder ownership does not require a corpus-write grant. An explicit target must still
        // resolve to this owner's typed project; a name/basename must not become a global fallback.
        let scope = match self.admit(&ctx, ReadSelection::Auto(None)).await {
            Ok(scope) => scope,
            Err(error) => return refusal(error),
        };
        tool_json(self.reminder_create(&scope, params).await)
    }

    #[tool(
        description = "Check for reminders that have come due and consume them. Client integrations call this automatically at the start of each interaction; you generally don't need to call it manually unless the user asks 'any reminders?'. Delivery is best-effort-once and one-way: what this returns is injected into your context and the row is advanced or retired, so a reminder you fail to act on is gone until its next fire. Recurring rows move to their next occurrence, coalescing elapsed fires into missed_occurrences (missed_occurrences_saturated means that number is a floor, not a count). Supply only an owned project ID as context, not a name or basename; invalid contexts are refused even if nothing is due. Held/unresolved legacy targets are never consumed."
    )]
    async fn check_reminders(
        &self,
        Parameters(params): Parameters<CheckRemindersParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        // The destructive reminder surface: an unscoped claim advanced another account's `next_due_at`
        // and incremented their `delivered_count`, so a reminder they were never shown was consumed or
        // rescheduled by somebody else. Admission comes first, and every read and claim below carries the
        // bound account.
        let scope = match self.admit(&ctx, ReadSelection::Auto(None)).await {
            Ok(scope) => scope,
            Err(error) => return refusal(error),
        };
        tool_json(self.reminder_check(&scope, &params).await)
    }

    #[tool(
        description = "List reminders (pending by default; filter by status 'pending'|'delivered'|'cancelled'|'all' and/or target project), oldest-due first, 50 to a page via limit/offset — the response carries truncated and next_offset. Use when the user asks what reminders exist, or to find an ID before cancelling; page it rather than asking for the whole history, since delivered and cancelled rows are kept."
    )]
    async fn list_reminders(
        &self,
        Parameters(params): Parameters<ListRemindersParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let scope = match self.admit(&ctx, ReadSelection::Auto(None)).await {
            Ok(scope) => scope,
            Err(error) => return refusal(error),
        };
        match self.reminder_list(&scope, &params).await {
            Ok(body) => CallToolResult::structured(body),
            Err(error) => CallToolResult::structured_error(serde_json::json!({
                "status": "error",
                "message": error_message(&error)
            })),
        }
    }

    #[tool(
        description = "Cancel a reminder by ID (soft-cancel; it stays listed under status=cancelled). Use when the user says a reminder is no longer needed, or after a reminder was delivered and the follow-up is done — for recurring reminders the user no longer wants."
    )]
    async fn cancel_reminder(
        &self,
        Parameters(params): Parameters<CancelReminderParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let scope = match self.admit(&ctx, ReadSelection::Auto(None)).await {
            Ok(scope) => scope,
            Err(error) => return refusal(error),
        };
        tool_json(self.reminder_cancel(&scope, &params).await)
    }

    #[tool(
        description = "Establish which project the current work belongs to, and get the opaque project ID to pass as `context_project` on later store/retrieve calls. Call this once per repository at the start of project-scoped work — with a sanitized origin remote (`remote`) or a project name/ID (`project`) — BEFORE storing anything with scope 'project'. This is the only tool that can create a project, so an agent never forks a partition by guessing a name: a remote or alias that does not resolve in your workspace is established here, and a project ID that is not yours is refused without saying whether it exists. The answer is stable across machines and across ssh/https spellings of the same repository. Never call it to satisfy a read that could simply use `scope: 'personal'`."
    )]
    async fn resolve_project(
        &self,
        Parameters(params): Parameters<ResolveProjectParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let owner = match self.admit_owner(&ctx).await {
            Ok(owner) => owner,
            Err(error) => return refusal(error),
        };
        let remote = match params.remote.as_ref() {
            None => None,
            Some(remote) => {
                match crate::scope_service::sanitized_remote(
                    &remote.host,
                    remote.port,
                    &remote.path,
                    &remote.transport,
                ) {
                    Ok(parsed) => Some(parsed),
                    Err(error) => return refusal(error),
                }
            }
        };
        let outcome = match ScopeService::new(&self.db)
            .establish_project(
                &owner,
                named(params.project.as_deref()),
                remote.as_ref(),
                named(params.display_name.as_deref()),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => return refusal(error),
        };
        CallToolResult::structured(serde_json::json!({
            "status": "ok",
            "established": outcome.established,
            "project": outcome.descriptor,
        }))
    }

    #[tool(
        description = "List the projects you own, with live memory counts, 20 to a page via cursor/limit (max 100) — page it rather than asking for all of them. Use it to find a project ID before storing into one, to check whether an alias is already taken, and to see what is actually classified. It answers from your workspace only: another account's project with the same name is neither listed nor resolvable here, and a name you did not establish is not created by this call."
    )]
    async fn list_projects(
        &self,
        Parameters(params): Parameters<ListProjectsParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        // Listing projects is a read over the caller's own workspace, so the owner context is the whole
        // authority it needs; no memory partition is resolved, and none is read.
        let owner = match self.admit_owner(&ctx).await {
            Ok(owner) => owner,
            Err(error) => return refusal(error),
        };
        match ScopeService::new(&self.db)
            .list_projects(
                &owner,
                named(params.cursor.as_deref()),
                params.limit.unwrap_or(DEFAULT_SCOPE_PAGE_LIMIT),
            )
            .await
        {
            Ok(page) => CallToolResult::structured(serde_json::json!({
                "projects": page
                    .entries
                    .iter()
                    .map(|entry| serde_json::json!({
                        "id": format_project_handle(entry.project.id.as_record_id()),
                        "display_name": entry.project.display_name,
                        "canonical_identity": entry.project.normalized_alias,
                        "memory_count": entry.live_memory_count,
                    }))
                    .collect::<Vec<_>>(),
                "count": page.entries.len(),
                "limit": page.limit,
                "next_cursor": page.next_cursor,
                "truncated": page.next_cursor.is_some(),
                "oversize": page.oversize,
            })),
            Err(error) => refusal(error),
        }
    }

    #[tool(
        description = "Enumerate the memories sitting in your Unassigned partition, for owner review only — use it when the user asks what was never classified, or to report how much of their corpus is unreadable by recall. Reading here changes nothing and grants nothing: these rows are in no recall path and no background maintenance, and the response says so (`review`, `eligible_for_recall`). Never call it to answer a memory question — `retrieve_memories`/`recall` deliberately cannot see this partition, which is the point of it."
    )]
    async fn list_unassigned_memories(
        &self,
        Parameters(params): Parameters<ListUnassignedMemoriesParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        // `UnassignedReview` is the only selection that reaches the partition at all, and asking for it
        // by name is what makes a review read distinguishable from a recall read in the storage layer.
        let scope = match self.admit(&ctx, ReadSelection::UnassignedReview).await {
            Ok(scope) => scope,
            Err(error) => return refusal(error),
        };
        let owner_clone = scope.owner().clone();
        match ScopeService::new(&self.db)
            .list_unassigned(
                &owner_clone,
                scope.read(),
                named(params.cursor.as_deref()),
                params.limit.unwrap_or(DEFAULT_SCOPE_PAGE_LIMIT),
            )
            .await
        {
            Ok(page) => CallToolResult::structured(serde_json::json!({
                "memories": page
                    .rows
                    .iter()
                    .map(|fact| serde_json::json!({
                        "id": fact.id.as_ref().map(record_id_to_string).unwrap_or_default(),
                        "content": fact.content,
                        "tags": fact.tags,
                        "confidence": fact.confidence,
                        "created_at": fact.created_at,
                        "quarantined": fact.quarantined_at.is_some(),
                    }))
                    .collect::<Vec<_>>(),
                "count": page.rows.len(),
                "limit": page.limit,
                "next_cursor": page.next_cursor,
                "truncated": page.next_cursor.is_some(),
                "oversize": page.oversize,
                "review": true,
                "eligible_for_recall": false,
                "disclosure": UNASSIGNED_REVIEW_DISCLOSURE,
            })),
            Err(error) => refusal(error),
        }
    }

    #[tool(
        description = "Propose where a memory ought to belong — a judgement the owner decides, never an edit. Use it when you notice a project-specific fact sitting in Personal (or a general one inside a project), especially right after storing. Supply the memory's ID, the `revision` you read it at (so a memory that moved since is refused as stale rather than judged against a row you never saw), the destination, and a one-clause reason. Nothing is moved by this call, by this server, or by you: a proposal lands as a pending row for the owner (or a human reviewing in the UI) to approve or dismiss, so never tell the user you reclassified anything. At most 20 entries per call; batch related proposals and propose again later rather than flooding the review queue."
    )]
    async fn propose_memory_scope(
        &self,
        Parameters(params): Parameters<ProposeMemoryScopeParams>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let owner = match self.admit_owner(&ctx).await {
            Ok(owner) => owner,
            Err(error) => return refusal(error),
        };
        tool_json(self.propose_scopes(&owner, &params).await)
    }
}

#[tool_handler(
    instructions = "Alexandria is a persistent agent memory system — use it proactively, not just when explicitly asked to 'remember' or 'recall' something.\n\n\
When to READ memory (retrieve_memories / recall): at the start of a task in a project or domain you've likely worked in before; whenever the user references past context ('last time', 'we decided', 'like before'); before re-deriving a decision or re-debugging something that may have been solved already. Use retrieve_memories for a specific lookup, recall for open-ended/broad exploration (call it once broad, then again with the returned scope_handle to narrow).\n\n\
When to WRITE memory (store_memory): as soon as you learn a durable fact worth keeping past this conversation — a user preference, an architectural decision and its rationale, a bug's root cause, a non-obvious gotcha, a correction the user gives you. Do this unprompted; don't wait to be told to remember. Write standalone statements that make sense without today's conversation.\n\n\
Session memory: pass session_id to store_memory or import_document to group memories by session. Use get_session to review a session's memories, and list_sessions to find a session id you don't have. Both answer in the population your `project` selects (personal when you name none) and withhold the session's own summary and tags by default; pass view: \"archive\" when you want the whole-session read of your own history. A session id narrows a read, it never widens the selected population. Use finalize_session at the end of a session to attach a summary and tags.
\
Project scope: call resolve_project once per repository before storing anything project-bound — pass `remote` (the sanitized origin remote, never a credential-bearing URL) or an established `project` name/alias, and use the opaque id it returns as `context_project` for the rest of the work. It is the only tool that can create a project, so a partition is never guessed into existence by a write. store_memory/import_document take `scope` (\"personal\"|\"project\") with `project`/`context_project`; retrieve_memories/recall take `scope` (\"auto\"|\"personal\"|\"project\"|\"assigned\") with `project`, and both report the selection that actually ran back as `scope`. A hosted write that names no target is refused before anything is embedded — there is no silent personal fallback; a trusted local single-owner install defaults to personal. list_projects shows your projects with live memory counts (paged by opaque cursor); list_unassigned_memories is owner review of rows no recall path can reach, and its response says so. propose_memory_scope records where you think a memory belongs, naming the revision you read it at: it is a judgement only — it moves nothing, and nothing applies it in this release. Never invent a project id, and never write another project's memories by guessing an alias: an unresolvable selector is refused as one \"not found\" answer.

\
Use update_memory (not store_memory) when correcting something already stored — it preserves lineage. Use import_document for bulk reference material (specs, READMEs, notes). Use delete_memory only when the user wants something actually forgotten.

\
Reminders: use set_reminder when the user asks to be reminded of something later or a follow-up will be needed at a specific time. Omit target_project for anything the user should see anywhere; set it to an existing owned project ID for project-bound follow-ups, never a name or checkout basename. Supply that same typed handle as check_reminders.project; omit the context when no handle is known. Unknown/foreign handles are refused even if no rows are due. Held or unresolved legacy text targets remain pending and suppressed, not global. The response carries next_fire_preview and the timezone the schedule was parsed in — confirm those with the user whenever the schedule came from natural language. Due reminders are delivered when a client calls check_reminders at the start of an interaction — the server runs no timer, so nothing fires on its own and a client that never calls it never receives one. check_reminders is the only tool that consumes them (recurring ones advance; missed fires coalesce rather than trickle) and it injects them into your context only: telling the user is your job. Call it yourself when the user asks whether anything is due, or when a reminder they expected has not shown up.

\
Reminder delivery: retrieve_memories and recall responses also carry a due_reminders array: a read-only, untargeted view of what is currently due (a short oldest-due sample, each entry has a target of global or an owned project:<opaque-id>) that consumes nothing. An entry targeting another project is informational there; it reaches the user through the next check_reminders, which also delivers project reminders once they are at least the server's escalation window overdue. Use list_reminders to review what is scheduled and cancel_reminder when a reminder is no longer needed."
)]
impl ServerHandler for AlexandriaServer {}

/// The tool's own options, named once so the request path and any wrapper cannot drift apart.
const RETRIEVE_TOOL: RetrieveOptions = RetrieveOptions {
    activate: true,
    record_access: true,
    apply_floor: true,
};

/// Which optional stages a retrieval run performs.
///
/// A Rust-side parameter rather than fields on [`RetrieveMemoriesParams`], because that struct is
/// the public MCP tool schema advertised to every LLM client — a debug-only knob there would be
/// something agents could set.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetrieveOptions {
    /// Fire spreading activation for the kept top-`top_n` results, i.e. warm neighbours' heat.
    activate: bool,
    /// Record this retrieval as an access on every row actually returned: reset its heat, grow
    /// stability by the spacing ratio, and count it. This is the signal `Appraise`'s demote rule
    /// reads (`access_count = 0` means "no retrieval has ever surfaced this row"), so it is a
    /// separate flag from `activate` rather than the same one under two names — a caller that
    /// legitimately suppresses activation must not silently stop counting accesses.
    record_access: bool,
    /// Drop ranked results below `retrieve_min_similarity`.
    apply_floor: bool,
}

/// The refusal for a handle that is not a well-formed `fact` id.
///
/// One text for an unparseable string and for a string naming another table, and it says nothing about
/// whether a row exists: a malformed request is a different conclusion from "not yours", and letting the
/// two share a class would make a typo look like an ownership answer.
const MALFORMED_ID: &str = "malformed memory id";

/// Put the selection that actually ran on a response body, under the owner-safe `scope` key.
///
/// A label, never a handle: the value is the caller's own word for a population (`personal`,
/// `personal+project`, `project`, `assigned`), not a project record id, so it discloses nothing the
/// request did not already carry. The store path already confirms a write's partition the same way;
/// a read had no equivalent, which left a caller unable to tell `auto`-with-no-project from
/// `personal` after the fact.
fn report_selection(body: &mut serde_json::Value, selection: ReadScope) {
    if let Some(map) = body.as_object_mut() {
        map.insert(
            "scope".to_string(),
            serde_json::Value::String(selection.as_str().to_string()),
        );
    }
}

/// The refusal for a `scope_handle` the server cannot read as a handle.
///
/// One text for every way that can happen — not base64, not JSON, not a missing field, not a field of
/// the wrong type, and not a clean decode whose `cluster_id` names something other than a `cluster`
/// record — because which layer gave up is a fact about this server's decoder and the caller's own
/// string, and the answer is the same for the caller either way: re-run the broad pass and use the
/// handle it returns. Distinguishing them would make a refused handle a probe for how close a guess
/// got.
const MALFORMED_HANDLE: &str = "malformed recall handle";

/// Turns a cluster-id the store refuses as *input* into the one handle refusal, and leaves every other
/// error alone.
///
/// A handle that decodes cleanly can still name `fact:1`, or a string that is not a record id at all,
/// and the scoped cluster read answers that as `StorageError::InvalidInput` — correct for a repository,
/// but a *second* refusal text for the same caller mistake, and one that quotes the storage layer.
/// [`AlexandriaServer::cluster_for_replay`] routes the replay's reads through here so the outward
/// answer stays exactly two-valued: a handle it can resolve (whose cluster may be foreign, deleted or
/// empty, all of which answer as no members), or a handle it cannot.
fn malformed_handle_or_through(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<alexandria_storage::error::StorageError>() {
        Some(alexandria_storage::error::StorageError::InvalidInput(_)) => {
            anyhow::anyhow!(MALFORMED_HANDLE)
        }
        _ => error,
    }
}

/// Which session enumeration a request asked for, once its population is resolved.
///
/// Two arms, named for the tool parameter rather than for a boolean, because `archive` is a widening the
/// caller has to ask for by name: a session id, a project, or a recall call never implies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionView {
    Scoped,
    Archive,
}

impl SessionView {
    pub fn archive(self) -> bool {
        self == Self::Archive
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scoped => "scoped",
            Self::Archive => "archive",
        }
    }
}

/// Which partition population one delete call is authorized to reach.
///
/// A closed private enum, and that shape is the point: `ReviewPartition` cannot be constructed by a request,
/// a tool argument or a deserializer (there is no `Deserialize`, and the variants are not `pub`), so the
/// only way a call carries it is by being made from the one handler that the owner's authenticated browser
/// session reaches. A boolean or a string parameter would have made human authority a field somebody could
/// set, which is exactly the shape the owner decision rules out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeletionReach {
    /// Every partition the caller's corpus read grants — but not the review partition.
    Corpus,
    /// The review partition too: the owner's own human cleanup of a row under review.
    ReviewPartition,
}

/// What an Unassigned listing must tell a caller about what it just read.
///
/// Stated in the response rather than only in the tool description, because the reading that matters is
/// that these rows are *not* missing from recall by accident: the partition is a review state, and an
/// agent that treated a hit here as a recallable memory would start citing what the owner has not
/// classified yet.
pub const UNASSIGNED_REVIEW_DISCLOSURE: &str = "owner-review state only: unassigned memories are in no recall path and no background maintenance, and reading them here changes nothing";

/// The pages `list_projects` and `list_unassigned_memories` return when the caller names no size.
pub const DEFAULT_SCOPE_PAGE_LIMIT: u32 = crate::scope_service::DEFAULT_SCOPE_PAGE_LIMIT;

/// The largest page either listing will return.
pub const MAX_SCOPE_PAGE_LIMIT: u32 = crate::scope_service::MAX_SCOPE_PAGE_LIMIT;

/// The refusal a partition-bearing write hears when the request names no lawful target.
///
/// A named constant because the wording is the contract: the pi companion and the Claude hook surface a
/// refusal to an agent, and "nothing has been embedded, written or queued" is the part that lets the
/// agent tell the user the store is unchanged instead of guessing. Task 8 kept the text and moved the
/// decision into [`AlexandriaServer::admit_write_target`], which now asks whether the request named a
/// lawful target rather than whether the caller is hosted.
pub const SCOPE_REQUIRED: &str = "scope required: this request carries no partition target for a new memory; nothing has been \
     embedded, written or queued";

/// An optional tool parameter as a trimmed, non-empty word.
///
/// `""` and `"   "` mean the same thing to a client that serialized a missing field, and treating one as
/// a named target would make an empty string a way to look authorized.
fn named(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

/// The scope label a response reports for a resolved partition.
///
/// A project is named by its opaque handle, which is the only form a caller may hand back to
/// `store_memory`/`retrieve_memories`; nothing here leaks a workspace id or an internal row layout.
fn scope_label(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::Personal => "personal".to_string(),
        MemoryScope::Unassigned => "unassigned".to_string(),
        // This label is handed back as a selector: preserve the typed key and quote it only once.
        MemoryScope::Project(project) => format_project_handle(project.as_record_id()),
    }
}

/// Turns a delete outcome into the tool's response payload.
///
/// One function because the structured-content contract is the observable thing, and it must not be able to
/// drift between the handler and any other caller of the same delete.
fn delete_result(outcome: Result<String, crate::error::ServiceError>) -> CallToolResult {
    match outcome {
        Ok(id) => CallToolResult::structured(serde_json::json!({ "status": "ok", "id": id })),
        Err(error) => CallToolResult::structured_error(serde_json::json!({
            "status": "error",
            "message": error.to_string()
        })),
    }
}

/// The one of two answers a proposal refusal may report.
///
/// `NotFound` and `Stale` are kept distinct because the caller's next action differs (re-read the id /
/// re-read the memory); anything else collapses to the caller-facing text of an invalid request. No SQL,
/// no id, and no account is named.
fn scope_refusal_reason(error: &crate::error::ServiceError) -> String {
    match error {
        crate::error::ServiceError::NotFound => "not found".to_string(),
        crate::error::ServiceError::Stale => "stale".to_string(),
        crate::error::ServiceError::InvalidInput(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A refusal every handler renders identically, so "not admitted" and "not yours" cannot be told apart
/// by which wrapper happened to be in scope at the call site.
fn refusal(error: crate::error::ServiceError) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "status": "error",
        "message": error.to_string()
    }))
}

/// The authority one admitted request acts under, for the duration of that request.
///
/// Two values, because a request needs both and neither implies the other: the [`OwnerContext`] is what a
/// write is stamped with and guarded on, and the [`ScopedContext`] is the population a read may see. A
/// handler holding only the first could read the whole workspace; one holding only the second could not
/// write at all.
///
/// The only producer is [`AlexandriaServer::admit`], which takes a principal the transport actually
/// admitted. There is no `Default`, no constructor from an account id, and nothing a request payload can
/// reach: naming an account, a workspace or a partition still has to authenticate to become one.
#[derive(Clone)]
pub struct RequestScope {
    owner: OwnerContext,
    read: ScopedContext,
}

impl RequestScope {
    /// The pair an authenticated browser page acts under, from the two contexts its session already
    /// resolved.
    ///
    /// A constructor on the scope rather than a helper on the server: the value is the same one a
    /// hosted MCP request carries, and saying so here is what makes "the debug UI and the tool share
    /// one authority type" a fact of the code rather than a claim in a comment.
    pub(crate) fn issued_for_debug(owner: OwnerContext, read: ScopedContext) -> Self {
        Self::issued(owner, read)
    }
    /// Bundles two contexts that [`AccessRepo`] already issued. Adds no authority of its own, because both
    /// arguments are already sealed capabilities.
    pub(crate) fn issued(owner: OwnerContext, read: ScopedContext) -> Self {
        Self { owner, read }
    }

    pub(crate) fn owner(&self) -> &OwnerContext {
        &self.owner
    }

    pub(crate) fn read(&self) -> &ScopedContext {
        &self.read
    }
}

// Implementation details
impl AlexandriaServer {
    /// The seams a *stdio* handler reaches: the principal is already resolved, so no `RequestContext`
    /// is involved. Test-only, and narrow — `Peer::new` is `pub(crate)` in rmcp, so an integration test
    /// cannot build the context the HTTP handlers take, and stdio never had one. These call the same
    /// bodies the handlers call; they add no authority and no public surface.
    #[cfg(test)]
    pub(crate) async fn import_document_scoped(
        &self,
        scope: &RequestScope,
        content: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let body: String = self
            .import_body(
                Some(scope),
                // The test harness stores into Personal, which is the partition its scope grants.
                Some(MemoryScope::Personal),
                ImportDocumentParams {
                    scope: None,
                    project: None,
                    context_project: None,
                    content: content.to_string(),
                    mode: Some("whole".to_string()),
                    chunk_strategy: None,
                    tags: None,
                    session_id: None,
                    agent_id: None,
                    model: None,
                },
            )
            .await?;
        serde_json::from_str(&body).map_err(anyhow::Error::from)
    }

    #[cfg(test)]
    pub(crate) async fn scoped_retrieve_text(
        &self,
        scope: &RequestScope,
        query: &str,
    ) -> anyhow::Result<String> {
        let value = self
            .retrieve_scoped(
                scope,
                RetrieveMemoriesParams {
                    scope: None,
                    project: None,
                    query: query.to_string(),
                    limit: Some(10),
                    session_id: None,
                },
                RETRIEVE_TOOL,
            )
            .await?;
        Ok(value.to_string())
    }

    /// Reads the caller's own session and its memories.
    ///
    /// `SessionRepo::find_by_external_id` keyed on the handle alone, so a handle another account also
    /// used returned *that* account's session — its metadata and, through `get_memories`, its memory
    /// bodies. The bound account is in every statement here, so a foreign handle and an absent one are
    /// the same refusal, and the refusal names no handle.
    pub(crate) async fn session_detail(
        &self,
        scope: &RequestScope,
        session_id: &str,
        view: SessionView,
    ) -> anyhow::Result<String> {
        let repo = ScopedSessionRepo::new(self.db.inner(), scope.owner().clone());
        let session = repo
            .find_by_external_id(session_id)
            .await
            .map_err(anyhow::Error::from)?
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        // The archive arm reads the session's live rows across every owned scope; the scoped arm reads
        // only the rows inside the population this request resolved. Both keep the soft-delete and
        // quarantine predicate — the archive is a wider *scope*, not a bypass of visibility.
        let memories = match view {
            SessionView::Archive => repo.get_memories(session_id).await,
            SessionView::Scoped => repo.memories_in(session_id, scope.read()).await,
        }
        .map_err(anyhow::Error::from)?;
        let list: Vec<serde_json::Value> = memories
            .iter()
            .map(|f| {
                serde_json::json!({
                    "id": f.id.as_ref().map(record_id_to_string).unwrap_or_default(),
                    "content": f.content,
                    "tags": f.tags,
                    "confidence": f.confidence,
                    "created_at": f.created_at,
                })
            })
            .collect();
        let mut body = serde_json::json!({
            "session": {
                "external_id": session.external_id,
                "agent_id": session.agent_id,
                "model": session.model,
                "started_at": session.started_at,
                "ended_at": session.ended_at,
                "memory_count": list.len(),
            },
            "memories": list,
            "view": view.as_str(),
        });
        // The prose is added by the archive arm only, and the keys are *absent* rather than null in the
        // scoped one: `summary: null` would read as "this session has no summary" to a client, which is
        // a different claim from "this view does not answer that".
        if view.archive() {
            body["session"]["summary"] = serde_json::json!(session.summary);
            body["session"]["tags"] = serde_json::json!(session.tags);
        }
        Ok(body.to_string())
    }

    /// Creates a reminder owned by the resolved caller.
    ///
    /// Creates the caller's own reminder — one statement, stamped as it is created.
    ///
    /// Schedule parsing and validation run through `set_reminder_body`, one implementation shared with the
    /// interim `do_set_reminder`, so the set-time refusals (past one-shot, never-fires pattern, the
    /// day-of-month/day-of-week intersection warning) cannot drift between the two arms. What the scoped
    /// arm then does differently is the write: `ScopedReminderRepo::create` carries `account` in its own
    /// `CREATE`, so there is no second statement to interrupt. The old shape was `ReminderRepo::create`
    /// (no owner) followed by a stamp, and the row left behind by an interruption was invisible to every
    /// scoped reader *including its author*, who had already been told it was scheduled.
    ///
    /// The create is guarded, so a revocation landing after `admit` costs the commit rather than creating
    /// a row the caller no longer has authority over.
    pub(crate) async fn reminder_create(
        &self,
        scope: &RequestScope,
        params: SetReminderParams,
    ) -> anyhow::Result<String> {
        self.set_reminder_body(Some(scope), params).await
    }

    /// Delivers the caller's own due reminders, and consumes only those rows.
    ///
    /// Three predicates are in play on each claim and all three are load-bearing: `account = $account`
    /// says whose rows may be consumed; `status = 'pending' AND next_due_at = $seen` says the row still
    /// looks exactly as this call read it, which is what stops two overlapping checks from both delivering
    /// one row and what stops a `cancel` landing in between from being clobbered; and the in-transaction
    /// authority claim (`ScopedReminderRepo::record_delivery` → `guarded_run`) says the caller is still
    /// who `admit` said they were when the row advances. Dropping any one reopens a different race.
    ///
    /// A refused claim is an error rather than a skipped row, and the effect is deliberate: if the
    /// authority dies after the third of five rows was delivered, the call fails and the remaining rows
    /// stay pending. The three consumed rows stay consumed — each committed on its own — so the response
    /// an agent never got describes deliveries that did happen. That is the same loss the accepted
    /// best-effort-once contract already allows for a claim whose response is lost in flight; what the
    /// guard removes is consuming rows for a caller that can no longer deliver them.
    ///
    /// A migration-pending row (`account` unset) is unreachable here, which is the fail-closed answer;
    /// Task 14 maps legacy reminders onto real owners and projects.
    pub(crate) async fn reminder_check(
        &self,
        scope: &RequestScope,
        params: &CheckRemindersParams,
    ) -> anyhow::Result<String> {
        use alexandria_engine::reminders as sched;
        use alexandria_storage::repos::ScopedReminderRepo;

        let tz = self.reminders.tz;
        let now = Utc::now();
        let escalation = match i64::try_from(self.reminders.escalation_hours)
            .ok()
            .and_then(chrono::Duration::try_hours)
        {
            Some(window) => window,
            None => chrono::Duration::MAX,
        };
        let repo = ScopedReminderRepo::new(self.db.inner(), scope.owner().clone());
        // Validate even when no rows are due: a context is an owned handle, never a best-effort hint.
        let context = match params.project.as_deref() {
            Some(project) => Some(repo.resolve_target(project).await?),
            None => None,
        };
        let due = repo.list_due(now).await.map_err(anyhow::Error::from)?;
        let mut delivered = Vec::new();
        for reminder in &due {
            let spec = match sched::spec_from_reminder(reminder) {
                Ok(spec) => spec,
                Err(error) => {
                    tracing::debug!(
                        "check_reminders: skipping unreadable row: {}",
                        error_message(&error)
                    );
                    continue;
                }
            };
            let (Some(id), Some(due_at)) = (reminder.id.as_ref(), reminder.next_due_at) else {
                continue;
            };
            let matches = context
                .as_ref()
                .is_some_and(|project| reminder.project.as_ref() == Some(project.as_record_id()));
            let escalated = reminder.project.is_some() && !matches;
            if escalated && now - due_at < escalation {
                continue;
            }
            let recurring = !matches!(spec, sched::ScheduleSpec::Once { .. });
            let missed = if recurring {
                match sched::occurrences_between(&spec, due_at, now, tz) {
                    Ok(missed) => missed,
                    Err(error) => {
                        tracing::debug!(
                            "check_reminders: cannot coalesce: {}",
                            error_message(&error)
                        );
                        continue;
                    }
                }
            } else {
                sched::OccurrenceCount {
                    count: 0,
                    saturated: false,
                }
            };
            let next = if recurring {
                match sched::next_fire(&spec, now, tz) {
                    Ok(next) => next,
                    Err(error) => {
                        tracing::debug!("check_reminders: no next fire: {}", error_message(&error));
                        continue;
                    }
                }
            } else {
                None
            };
            let taken = repo
                .record_delivery(&record_id_to_string(id), next, Some(due_at), now)
                .await
                .map_err(anyhow::Error::from)?;
            if !taken {
                continue;
            }
            delivered.push(serde_json::json!({
                "id": reminder.id.as_ref().map(record_id_to_string),
                "message": reminder.message,
                "target": reminder.project.as_ref().map(format_project_handle)
                    .unwrap_or_else(|| "global".into()),
                "escalated": escalated,
                "recurring": recurring,
                "missed_occurrences": missed.count,
                "missed_occurrences_saturated": missed.saturated,
                "due_at": rfc3339_utc(due_at),
                "schedule": sched::human_readable(&spec),
                "note": reminder.note,
                "provenance": {
                    "project": reminder.prov_project,
                    "session_id": reminder.prov_session_id,
                },
                "next_due_at": next.map(rfc3339_utc),
                "timezone": tz.name(),
                "due_at_local": due_at.with_timezone(&tz).format("%Y-%m-%d %H:%M %Z").to_string(),
                "next_due_at_local": next.map(|at| at.with_timezone(&tz).format("%Y-%m-%d %H:%M %Z").to_string()),
            }));
        }
        Ok(serde_json::json!({
            "count": delivered.len(),
            "delivered": delivered,
            "timezone": self.reminders.tz.name().to_string(),
        })
        .to_string())
    }

    /// Cancels the caller's own reminder, and reports success only if a row was actually written.
    ///
    /// The unscoped `ReminderRepo::cancel` was `UPDATE type::record($id) SET status = 'cancelled'` with
    /// no owner in the clause, so any admitted caller could retire any reminder by id and the tool
    /// answered `status:"ok"` for a row it did not own — destructive rather than merely visible. Both the
    /// lookup and the write now carry the bound account, and the two refusals (foreign id, absent id)
    /// render identically.
    pub(crate) async fn reminder_cancel(
        &self,
        scope: &RequestScope,
        params: &CancelReminderParams,
    ) -> anyhow::Result<String> {
        use alexandria_storage::repos::ScopedReminderRepo;

        let repo = ScopedReminderRepo::new(self.db.inner(), scope.owner().clone());
        let id = surrealdb::types::RecordId::parse_simple(&params.id)
            .map_err(|_| anyhow::anyhow!("Reminder not found"))?;
        if id.table.as_str() != "reminder" {
            anyhow::bail!("Reminder not found");
        }
        if repo
            .get(&params.id)
            .await
            .map_err(anyhow::Error::from)?
            .is_none()
        {
            anyhow::bail!("Reminder not found");
        }
        if !repo
            .cancel(&params.id, Utc::now())
            .await
            .map_err(anyhow::Error::from)?
        {
            anyhow::bail!("Reminder not found");
        }
        Ok(serde_json::json!({ "status": "ok", "id": params.id }).to_string())
    }

    /// Lists the caller's own reminders. The account predicate precedes `LIMIT`/`START`, so the page and
    /// the `truncated` flag describe this owner's rows rather than a global page filtered afterwards.
    pub(crate) async fn reminder_list(
        &self,
        scope: &RequestScope,
        params: &ListRemindersParams,
    ) -> anyhow::Result<serde_json::Value> {
        use alexandria_storage::repos::ScopedReminderRepo;

        let repo = ScopedReminderRepo::new(self.db.inner(), scope.owner().clone());
        let status = match params.status.as_deref() {
            None | Some("pending") => Some("pending"),
            Some("all") => None,
            Some(other) => Some(other),
        };
        let limit = params
            .limit
            .unwrap_or(DEFAULT_LIST_REMINDERS_LIMIT)
            .clamp(1, MAX_LIST_REMINDERS_LIMIT);
        let offset = params.offset.unwrap_or(0).max(0);
        let (rows, truncated) = repo
            .list(status, params.target_project.as_deref(), limit, offset)
            .await
            .map_err(anyhow::Error::from)?;
        Ok(serde_json::json!({
            "count": rows.len(),
            "limit": limit,
            "offset": offset,
            "truncated": truncated,
            "next_offset": if truncated { Some(offset + limit) } else { None },
            "reminders": rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id.as_ref().map(record_id_to_string),
                        "message": r.message,
                        "status": r.status,
                        "target": r.project.as_ref().map(format_project_handle).or_else(|| {
                            (r.target_project.is_none() && r.legacy_target_project.is_none())
                                .then(|| "global".to_string())
                        }),
                        "migration_hold": r.migration_hold.unwrap_or(false),
                        "legacy_target_project": r.legacy_target_project,
                        "unresolved_target_project": r.target_project,
                        "schedule_kind": r.schedule_kind,
                        "next_due_at": r.next_due_at.map(rfc3339_utc),
                        "delivered_count": r.delivered_count,
                        "note": r.note,
                        "created_at": r.created_at.map(rfc3339_utc),
                    })
                })
                .collect::<Vec<_>>(),
        }))
    }

    /// The population and view a session read is entitled to.
    ///
    /// `archive` is the one deliberate widening in the session surface: the whole-session read of the
    /// caller's own history, across every scope they own, resolving `Assigned` (never Unassigned) because
    /// that is what "every scope I own" means. `scoped` is Personal, or exactly one named project — and a
    /// `project` on an archive read is refused rather than ignored, because a silently-dropped selector
    /// turns a filter into a lie about the rows it did not apply.
    pub(crate) async fn admit_session_view(
        &self,
        ctx: &RequestContext<RoleServer>,
        view: Option<&str>,
        project: Option<&str>,
    ) -> Result<(RequestScope, SessionView), crate::error::ServiceError> {
        match named(view) {
            None | Some("scoped") | Some("archive") => {}
            Some(other) => {
                return Err(crate::error::ServiceError::InvalidInput(format!(
                    "unknown view '{other}': use 'scoped' or 'archive'"
                )));
            }
        }
        if named(view) == Some("archive") {
            if named(project).is_some() {
                return Err(crate::error::ServiceError::InvalidInput(
                    "view 'archive' already spans every scope you own; it takes no `project`"
                        .to_string(),
                ));
            }
            let scope = self.admit(ctx, ReadSelection::Assigned).await?;
            return Ok((scope, SessionView::Archive));
        }
        let owner = self.admit_owner(ctx).await?;
        let project = match named(project) {
            None => ReadSelection::Auto(None),
            Some(text) => {
                let target = ScopeService::new(&self.db)
                    .resolve_selector(&owner, text)
                    .await?;
                ReadSelection::One(MemoryScope::Project(target.id))
            }
        };
        Ok((self.admit_read(&owner, project).await?, SessionView::Scoped))
    }

    /// Lists the caller's own sessions, newest activity first.
    ///
    /// The account predicate is in the `WHERE` before `LIMIT`/`START`, so a page holds this owner's
    /// sessions rather than a global page filtered afterwards — the latter would let how many sessions
    /// other accounts hold change what this caller sees on page two.
    pub(crate) async fn session_list(
        &self,
        scope: &RequestScope,
        params: &ListSessionsParams,
        archive: bool,
    ) -> anyhow::Result<String> {
        // A `tag` filter searches the session's tag set, which is account-private prose. Asking for it
        // on a scoped page is refused rather than quietly widened into an archive read: silently
        // ignoring a filter the caller named would return more rows than they asked for.
        if !archive && named(params.tag.as_deref()).is_some() {
            anyhow::bail!(
                "a scoped session list cannot filter by session tag: the tag set is account-private \
                 prose. Re-read with view: \"archive\" to search it"
            );
        }
        let sessions = ScopedSessionRepo::new(self.db.inner(), scope.owner().clone())
            .list_scoped(&SessionPageQuery {
                agent_id: params.agent_id.clone(),
                // A `tag` filter searches the prose the scoped view withholds, which the check above has
                // already refused; this is the archive arm only.
                tag: if archive { params.tag.clone() } else { None },
                finalized: params.finalized,
                // The membership filter is the caller's own resolved population, so a session's count is
                // exactly the memories that caller can read under the scope they asked for.
                membership: ScopeFilter::new(scope.read()),
                projection: if archive {
                    SessionProjection::Archive
                } else {
                    SessionProjection::Scoped
                },
                limit: params.limit.unwrap_or(20) as usize,
                offset: params.offset.unwrap_or(0) as usize,
            })
            .await
            .map_err(anyhow::Error::from)?;
        let list: Vec<serde_json::Value> = sessions
            .iter()
            .map(|s| {
                let mut row = serde_json::json!({
                    "external_id": s.external_id,
                    "agent_id": s.agent_id,
                    "model": s.model,
                    "started_at": s.started_at,
                    "ended_at": s.ended_at,
                    "memory_count": s.memory_count,
                });
                // The two prose keys exist on the archive arm only, exactly as `session_detail` does it.
                // Emitting `"summary": null, "tags": []` on a scoped page is not a withheld field, it is a
                // claim — that the session has no summary — which is a different statement from "this
                // view does not answer that", and the two session tools must not disagree about what
                // withholding looks like.
                if archive {
                    // `json!` above produced an object, so the mutable view cannot be `None` here; the
                    // unwrap is about the local invariant, not about a value from outside.
                    let map = row
                        .as_object_mut()
                        .expect("the row builder produced an object");
                    map.insert("summary".to_string(), serde_json::json!(s.summary));
                    map.insert("tags".to_string(), serde_json::json!(s.tags));
                }
                row
            })
            .collect();
        Ok(serde_json::json!({
            "sessions": list,
            "count": list.len(),
            "view": if archive { "archive" } else { "scoped" },
        })
        .to_string())
    }

    /// Records up to [`MAX_PROPOSALS_PER_REQUEST`] judgements and moves nothing.
    ///
    /// Every entry is answered for on its own — one bad id in a batch of five does not lose the other
    /// four — because an agent that has to re-send a whole batch to fix one entry will eventually stop
    /// proposing. `refused` carries the same two words a caller can act on (`not found`, `stale`) and
    /// nothing about whether the id exists elsewhere.
    pub(crate) async fn propose_scopes(
        &self,
        owner: &OwnerContext,
        params: &ProposeMemoryScopeParams,
    ) -> anyhow::Result<String> {
        use crate::scope_service::MAX_PROPOSALS_PER_REQUEST;

        if params.proposals.is_empty() {
            anyhow::bail!("propose at least one entry, or send nothing and change nothing");
        }
        if params.proposals.len() > MAX_PROPOSALS_PER_REQUEST {
            anyhow::bail!(
                "at most {MAX_PROPOSALS_PER_REQUEST} proposals per request (this call carried {}); \
                 propose in passes so the review queue stays readable",
                params.proposals.len()
            );
        }
        let service = ScopeService::new(&self.db);
        let mut accepted = Vec::new();
        let mut refused = Vec::new();
        for entry in &params.proposals {
            let refusal = |reason: String| serde_json::json!({ "id": entry.id, "reason": reason });
            let Some(fact) = crate::scope_service::parse_fact_handle(&entry.id) else {
                refused.push(refusal("not found".to_string()));
                continue;
            };
            let (destination, descriptor) = match service
                .resolve_destination(owner, &entry.destination, entry.project.as_deref())
                .await
            {
                Ok(resolved) => resolved,
                Err(error) => {
                    refused.push(refusal(error.to_string()));
                    continue;
                }
            };
            match service
                .propose(
                    owner,
                    &fact,
                    Version::new(entry.expected_revision),
                    destination,
                    named(entry.reason.as_deref()),
                )
                .await
            {
                Ok(proposal) => accepted.push(serde_json::json!({
                    "id": record_id_to_string(fact.as_record_id()),
                    "proposal_id": proposal.proposal_id,
                    "revision": proposal.revision,
                    "destination": proposal.destination,
                    "project": descriptor.map(|p| p.id),
                    "state": "pending",
                    "applied": false,
                })),
                Err(error) => refused.push(refusal(scope_refusal_reason(&error))),
            }
        }
        Ok(serde_json::json!({
            "status": "ok",
            "applied": false,
            "count": accepted.len(),
            "proposals": accepted,
            "refused": refused,
            "disclosure": "a proposal records a judgement and moves nothing; the owner decides",
        })
        .to_string())
    }

    /// Finalizes the caller's own session, guarded inside its write transaction.
    ///
    /// The worst of the session holes, and the reason v010 could not ship alone: `SessionRepo::finalize`
    /// was `UPDATE session SET … WHERE external_id = $handle` with no owner, so two accounts that
    /// independently used the same handle shared one row and the second to finalize rewrote the first
    /// one's summary and tags — while the tool answered `status:"ok"` with the *victim's* `external_id`.
    /// The bound account is now part of the statement (a foreign handle is `Session not found`, the
    /// identical answer to an absent one), and the in-transaction authorization claim means a revocation
    /// landing between resolving the row and committing costs the commit a write conflict instead of
    /// letting the write land.
    pub(crate) async fn session_finalize(
        &self,
        scope: &RequestScope,
        params: &FinalizeSessionParams,
    ) -> anyhow::Result<String> {
        let repo = ScopedSessionRepo::new(self.db.inner(), scope.owner().clone());
        let updated = repo
            .finalize(
                &params.session_id,
                params.summary.as_deref(),
                params.tags.as_deref(),
            )
            .await
            .map_err(anyhow::Error::from)?
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        Ok(serde_json::json!({
            "status": "ok",
            "external_id": updated.external_id,
            "ended_at": updated.ended_at,
            "summary": updated.summary,
            "tags": updated.tags,
        })
        .to_string())
    }

    /// Resolves the authority an admitted request acts under, re-reading it every time.
    ///
    /// The principal comes from the transport: an admitted hosted credential wins, otherwise the configured
    /// stdio identity, otherwise nothing — and "nothing" is a refusal, never a default. `authorize` and
    /// `select` then re-read the live rows, so a context earned by an earlier request cannot carry a
    /// suspension, a revocation or an epoch bump forward into this one.
    ///
    /// A hosted principal is additionally gated on its **credential lease**, not on rmcp's `ctx.ct`: a
    /// session closed under a revocation cancels the lease and does not cancel the per-request token, so a
    /// handler parked on `ctx.ct` would finish and commit after its credential was revoked. Task 5 measured
    /// that difference in `revocation_ends_an_open_stream_and_delivers_no_frame_after_it` and named the
    /// lease as the handle Task 6's writes ride on.
    pub(crate) async fn admit(
        &self,
        ctx: &RequestContext<RoleServer>,
        selection: ReadSelection,
    ) -> Result<RequestScope, crate::error::ServiceError> {
        let owner = self.admit_owner(ctx).await?;
        self.admit_read(&owner, selection).await
    }

    /// The first half of [`Self::admit`]: the principal, the lease gate, and the owner context.
    ///
    /// Split out because Task 8's store, import and proposal paths have to resolve a *partition* between
    /// the two halves, and resolving one is a workspace-qualified read that needs the owner. Nothing about
    /// the order changed: an untargeted hosted write is still refused before `select`, before any
    /// embedding, and before any row.
    pub(crate) async fn admit_owner(
        &self,
        ctx: &RequestContext<RoleServer>,
    ) -> Result<OwnerContext, crate::error::ServiceError> {
        let principal =
            crate::auth::principal_from_stdio_or_request(self.stdio_identity.as_ref(), ctx)?;
        if matches!(
            principal.provenance(),
            alexandria_storage::access::PrincipalProvenance::Credential(_)
        ) {
            let lease = crate::auth::lease_from_request(ctx)?;
            if lease.is_cancelled() {
                return Err(crate::error::ServiceError::Stale);
            }
        }
        Ok(AccessRepo::new(&self.db).authorize(&principal).await?)
    }

    /// The second half of [`Self::admit`]: resolve a read population for an already-authorized owner.
    pub(crate) async fn admit_read(
        &self,
        owner: &OwnerContext,
        selection: ReadSelection,
    ) -> Result<RequestScope, crate::error::ServiceError> {
        let read = AccessRepo::new(&self.db).select(owner, selection).await?;
        Ok(RequestScope::issued(owner.clone(), read))
    }

    /// Whether this request's principal is the operator's own configured identity rather than a bearer.
    ///
    /// The only question D4's "may this caller default to Personal?" turns on, and it is a fact the
    /// transport established: a hosted request that presents no credential does not become the local
    /// operator by omission, which is why this reads the sealed provenance and not a parameter.
    pub(crate) fn principal_is_trusted_local(
        &self,
        ctx: &RequestContext<RoleServer>,
    ) -> Result<bool, crate::error::ServiceError> {
        let principal =
            crate::auth::principal_from_stdio_or_request(self.stdio_identity.as_ref(), ctx)?;
        Ok(matches!(
            principal.provenance(),
            alexandria_storage::access::PrincipalProvenance::TrustedLocal
        ))
    }

    /// The partition a store or import may write, and the scope that grants it.
    ///
    /// Task 8 replaced D4's blanket hosted refusal with this and kept the one property D4 existed for:
    /// **the refusal precedes the first `embed`**. Resolution is a workspace-qualified read of the
    /// caller's own project rows, so what a refusal pays for is the lookup that proves the request was
    /// unanswerable; `scoped_request_path_test`'s three pre-side-effect cases pin that ordering against a
    /// counting provider and fail if it is reversed.
    ///
    /// The returned scope is admitted with `One(scope)`, so the caller's read population is exactly the
    /// partition the row lands in — which is also what makes the store-time cluster assignment consider
    /// only that partition's clusters, the design's own rule for a store.
    pub(crate) async fn admit_write_target(
        &self,
        ctx: &RequestContext<RoleServer>,
        scope: Option<&str>,
        project: Option<&str>,
        context_project: Option<&str>,
    ) -> Result<(RequestScope, MemoryScope), crate::error::ServiceError> {
        let trusted_local = self.principal_is_trusted_local(ctx)?;
        // Refuse a hosted request that names no partition *before* authorizing anything: an untargeted
        // hosted store is not a question about the store, and it must not cost a read.
        if !trusted_local
            && named(scope).is_none()
            && named(project).is_none()
            && named(context_project).is_none()
        {
            return Err(crate::error::ServiceError::InvalidInput(
                SCOPE_REQUIRED.to_string(),
            ));
        }
        let owner = self.admit_owner(ctx).await?;
        let target = ScopeService::new(&self.db)
            .resolve_write_target(
                &owner,
                named(scope),
                named(project),
                named(context_project),
                trusted_local,
            )
            .await?;
        let partition = target.memory_scope();
        let scope = self
            .admit_read(&owner, ReadSelection::One(partition.clone()))
            .await?;
        Ok((scope, partition))
    }

    /// Recall over the caller's own clusters.
    ///
    /// Two leaks closed. `load_cluster_infos`/`load_cluster_with_members` were unscoped, so broad
    /// recall's candidate list carried another account's cluster **labels** and member counts — content,
    /// without returning a single memory body. And a `scope_handle` is an unauthenticated string the
    /// client echoes back; the focused arm replayed it against whichever cluster it named, so a handle
    /// that was logged, guessed, or handed over read somebody else's memories. Both now resolve through
    /// the caller's own workspace. The engine call and the response shape are unchanged.
    pub(crate) async fn recall_scoped(
        &self,
        scope: &RequestScope,
        selection: ReadScope,
        params: &RecallParams,
    ) -> anyhow::Result<String> {
        let query_vecs = self.embedding.embed(&[&params.query]).await?;
        let query_emb = &query_vecs[0];
        if let Some(handle_str) = params.scope_handle.as_ref() {
            // One refusal text for anything the server cannot read as a handle, so a base64 mistake and
            // a JSON mistake do not answer differently.
            let handle =
                RecallHandle::decode(handle_str).map_err(|_| anyhow::anyhow!(MALFORMED_HANDLE))?;
            let (version, members) = self
                .cluster_for_replay(scope, handle.cluster_id.as_str())
                .await?;
            let cluster_data = ClusterWithMembers {
                info: ClusterInfo {
                    id: handle.cluster_id.as_str().to_string(),
                    centroid: vec![],
                    member_count: members.len(),
                },
                version,
                members,
            };
            let result = focused_recall(query_emb, &handle, &cluster_data);
            if result.snapshot_moved {
                // The disclosure, in the log rather than the response: the rows below are the caller's
                // cluster as it is now, not as it was when this handle was issued. The id here is the
                // cluster the handle named — which the caller submitted, so it is not necessarily the
                // caller's own — but nothing reaches the client from this line, and the two versions it
                // compares are the caller's own read, so naming one id and two integers stays
                // owner-safe whatever the reader.
                tracing::debug!(
                    "recall: scope_handle for {} was issued at {:?} and is being answered from {:?}",
                    handle.cluster_id,
                    handle.version,
                    version,
                );
            }
            return Ok(serde_json::json!({
                "mode": "focused",
                "scope": selection.as_str(),
                "memories": result
                    .memories
                    .iter()
                    .map(|m| serde_json::json!({
                        "id": m.id,
                        "content": m.content,
                        "similarity": m.similarity,
                        "heat": m.heat,
                    }))
                    .collect::<Vec<_>>(),
                "due_reminders": self
                    .due_reminders_summary_for(scope, DUE_REMINDERS_CAP as usize)
                    .await,
            })
            .to_string());
        }
        let clusters = self.scoped_clusters_with_members(scope).await?;
        let result = broad_recall(query_emb, &clusters, 5, self.retrieve_min_similarity);
        let cluster_results: Vec<serde_json::Value> = result
            .clusters
            .iter()
            .map(|cm| {
                serde_json::json!({
                    "cluster_id": cm.cluster_id,
                    "similarity": cm.similarity,
                    "scope_handle": cm.scope_handle,
                    "representative_memories": cm
                        .representative_memories
                        .iter()
                        .map(|m| serde_json::json!({
                            "id": m.id,
                            "content": m.content,
                            "similarity": m.similarity,
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(serde_json::json!({
            "mode": "broad",
            "scope": selection.as_str(),
            "clusters": cluster_results,
            "due_reminders": self
                .due_reminders_summary_for(scope, DUE_REMINDERS_CAP as usize)
                .await,
        })
        .to_string())
    }

    /// Broad recall's candidate clusters, each with the caller's own members and the version the handle
    /// it issues will carry.
    ///
    /// The guard comes from one `revisions()` read for the whole population, not one per cluster: this
    /// pass lists every candidate and keeps the top few, and the member count it needs anyway falls out
    /// of the members it is about to load.
    async fn scoped_clusters_with_members(
        &self,
        scope: &RequestScope,
    ) -> anyhow::Result<Vec<ClusterWithMembers>> {
        let repo = ScopedClusterRepo::new(self.db.inner(), scope.owner(), scope.read());
        let revisions = repo.revisions().await.map_err(anyhow::Error::from)?;
        let mut out = Vec::new();
        for info in self.cluster_infos_for(scope).await? {
            let members = self.cluster_members_for(scope, &info.id).await?;
            // A cluster with no member inside the caller's resolved population is not a candidate, and
            // gets no handle: focused replay of that cluster would answer with no members, and offering
            // it would make an empty selection look like a populated snapshot.
            if members.is_empty() {
                continue;
            }
            let membership_revision = revisions
                .iter()
                .find(|(id, _)| record_id_to_string(id) == info.id)
                .and_then(|(_, revision)| *revision);
            out.push(ClusterWithMembers {
                info: ClusterInfo {
                    member_count: members.len(),
                    ..info
                },
                version: ClusterVersion {
                    membership_revision,
                    members: members.len(),
                },
                members,
            });
        }
        Ok(out)
    }

    /// One cluster, re-read under the caller's current context, with the version it now carries.
    ///
    /// The replay path's reauthorization, and the reason a `scope_handle` can be an unsigned string:
    /// nothing is taken from the handle but the cluster it names, and that name is resolved through the
    /// caller's own workspace before a member is touched. A cluster that has been deleted, moved, or
    /// never belonged to this caller answers with no members, so the handle is never an oracle for which
    /// of the two it was. A name the store cannot even parse is not a third answer: see
    /// [`malformed_handle_or_through`], which folds it back onto `MALFORMED_HANDLE`.
    async fn cluster_for_replay(
        &self,
        scope: &RequestScope,
        cluster_id: &str,
    ) -> anyhow::Result<(ClusterVersion, Vec<FactSummary>)> {
        let members = self
            .cluster_members_for(scope, cluster_id)
            .await
            .map_err(malformed_handle_or_through)?;
        let membership_revision =
            ScopedClusterRepo::new(self.db.inner(), scope.owner(), scope.read())
                .revision(cluster_id)
                .await
                .map_err(anyhow::Error::from)?;
        Ok((
            ClusterVersion {
                membership_revision,
                members: members.len(),
            },
            members,
        ))
    }

    /// One cluster's members, re-resolved through the caller's own account.
    ///
    /// A `scope_handle` is an **unauthenticated string** the client echoes back, so the cluster it names
    /// must be checked against the caller before any member is read. Without that, replaying a handle
    /// that was guessed, logged, or handed over reads whoever's cluster it happens to be — contents
    /// included.
    pub(crate) async fn cluster_members_for(
        &self,
        scope: &RequestScope,
        cluster_id: &str,
    ) -> anyhow::Result<Vec<FactSummary>> {
        let members = ScopedClusterRepo::new(self.db.inner(), scope.owner(), scope.read())
            .get_members(cluster_id)
            .await
            .map_err(anyhow::Error::from)?;
        Ok(members
            .into_iter()
            .map(|f| FactSummary {
                id: f.id.as_ref().map(record_id_to_string).unwrap_or_default(),
                content: f.content,
                embedding: f.embedding,
                heat: 1.0,
            })
            .collect())
    }

    /// The clusters a memory may lawfully **join** when it is filed into `partition`: the partition's own,
    /// non-empty and unmixed.
    ///
    /// This is the writer's ranking population, and it exists beside [`Self::cluster_infos_for`] rather
    /// than replacing it because the two questions are different and both are real. `cluster_infos_for`
    /// answers "what clusters does this account have in this workspace", which is what broad recall needs
    /// — and which is *wrong* for a destination, because a workspace holds several partitions and the
    /// frozen multi-user design keeps request-path assignment inside one. Ranking a destination against the
    /// workspace list is how a `Personal` cluster became the destination of an `A`-to-`B` move and then
    /// conflicted with its own detach; the defect was invisible to a vector-separated corpus and is obvious
    /// under a constant one, which is why the fix's fixture uses constant vectors.
    ///
    /// Empty clusters and legacy clusters with no owner are not adopted here; see
    /// [`ScopedClusterRepo::partition_candidates`]. A `partition` that is not one of the caller's granted
    /// partitions cannot be reached by any caller of this method — every one resolves its write target
    /// through `ScopeService` first, and admission is what makes the scope real.
    async fn destination_cluster_candidates(
        &self,
        scope: &RequestScope,
        partition: &MemoryScope,
    ) -> anyhow::Result<DestinationCandidates> {
        let repo = ScopedClusterRepo::new(self.db.inner(), scope.owner(), scope.read());
        let clusters = repo.candidates_for_partition(partition).await?;
        Ok(DestinationCandidates::from_clusters(clusters)?)
    }

    /// The caller's own clusters, with stored centroids and workspace live-member counts.
    ///
    /// Filtered by workspace, so broad recall's candidate list carries no other account's **labels** or
    /// sizes. A cluster name describes what its memories are about, so listing foreign clusters leaks
    /// content while returning no memory body at all. The count is deliberately the whole-workspace
    /// membership because the store path consumes it as the centroid running average; recall separately
    /// loads the partition-eligible members and drops a cluster with none.
    ///
    /// **Read-only by contract.** This is the recall candidate list, and no writer may rank a destination
    /// against it — `destination_cluster_candidates` is the writer's seam, and a call site that reaches for this
    /// one has reintroduced the cross-partition join B2 closes.
    pub(crate) async fn cluster_infos_for(
        &self,
        scope: &RequestScope,
    ) -> anyhow::Result<Vec<ClusterInfo>> {
        let clusters = ScopedClusterRepo::new(self.db.inner(), scope.owner(), scope.read())
            .list_with_counts()
            .await
            .map_err(anyhow::Error::from)?;
        Ok(clusters
            .into_iter()
            .map(|(c, member_count)| ClusterInfo {
                id: c.id.as_ref().map(record_id_to_string).unwrap_or_default(),
                centroid: c.centroid,
                member_count,
            })
            .collect())
    }

    ///
    /// Two things the unscoped path got wrong, both pinned by
    /// `scoped_request_path_test::a_foreign_id_and_a_missing_id_are_indistinguishable`:
    ///
    /// * **The write was not owner-bound.** `MemoryRepo::soft_delete_fact` was
    ///   `UPDATE type::record($id) SET deleted = true` with no owner in its `WHERE`, so any admitted caller
    ///   could destroy any row by id and the tool answered `{"status":"ok"}` — a cross-account write that
    ///   did not even look like a refusal.
    /// * **A foreign id and an absent id gave different answers.** Both are `NotFound` here, so the response
    ///   cannot be used to probe whether some other account holds a given row.
    ///
    /// `Assigned` rather than `Auto(None)`: forgetting one's own row is authorized by the owner and the
    /// row's partition, not by whatever a *read* happened to select, and a caller must be able to delete a
    /// project memory from a Personal-scoped session. `Assigned` still excludes Unassigned, so quarantined
    /// rows stay out of reach of this path.
    async fn delete_core(
        &self,
        ctx: &RequestContext<RoleServer>,
        id: &str,
    ) -> Result<String, crate::error::ServiceError> {
        let scope = self.admit(ctx, ReadSelection::Assigned).await?;
        self.delete_within(&scope, id).await
    }

    /// The delete, given an already-resolved scope.
    ///
    /// Split from [`Self::delete_core`] because a `RequestContext<RoleServer>` carries a `Peer` that only a
    /// live rmcp service builds: admission is exercised through the transport suite, while the owner
    /// predicate, the malformed-handle refusal and the response shape are exercised directly.
    ///
    /// Soft deletion is a **semantic** change — it moves `visibility`, which is one of the halves
    /// `FactSnapshot::semantic_hash` digests — so it is journaled like any other rather than issued as a bare
    /// `UPDATE`. A hidden row whose hiding was never recorded is a memory the owner cannot account for on their
    /// own history page.
    pub(crate) async fn delete_within(
        &self,
        scope: &RequestScope,
        id: &str,
    ) -> Result<String, crate::error::ServiceError> {
        self.delete_reaching(scope, id, DeletionReach::Corpus).await
    }

    /// The human review path's delete: the owner's own soft-delete of a row in the review partition.
    ///
    /// **Nothing here is a client-supplied authority.** The reach is chosen by which handler calls it, and
    /// only a browser request that passed the session cookie, the `Origin`/Fetch-Metadata pair and the
    /// CSRF token in `guard` reaches a handler that can name `ReviewPartition`; no MCP tool, tool
    /// parameter, JSON field or stored value can produce it. Inside the write the authority is the same
    /// `OwnerContext` the tool path carries, and `claim_authority` still re-checks the live account, the
    /// auth epoch and the presenting credential in the mutating transaction — so a revocation landing
    /// between the page load and the click costs the delete rather than committing over it.
    ///
    /// The owner decision this encodes is D-review's: cleaning up the review partition is a human act. An
    /// agent may propose where a quarantined memory belongs and a human may approve the move, but the row
    /// leaving the corpus is the owner's decision alone, which is why [`Self::delete_within`] refuses what
    /// this one permits.
    pub(crate) async fn delete_reviewed_within(
        &self,
        scope: &RequestScope,
        id: &str,
    ) -> Result<String, crate::error::ServiceError> {
        self.delete_reaching(scope, id, DeletionReach::ReviewPartition)
            .await
    }

    /// The delete body both reaches share, so the two cannot drift apart in guards, journal or shape.
    async fn delete_reaching(
        &self,
        scope: &RequestScope,
        id: &str,
        reach: DeletionReach,
    ) -> Result<String, crate::error::ServiceError> {
        let handle = surrealdb::types::RecordId::parse_simple(id)
            .map_err(|_| crate::error::ServiceError::InvalidInput(MALFORMED_ID.to_string()))?;
        if handle.table.as_str() != "fact" {
            return Err(crate::error::ServiceError::InvalidInput(
                MALFORMED_ID.to_string(),
            ));
        }
        let service = ApplyService::new(self.db.as_ref(), scope.owner(), scope.read());
        let fact = FactId::try_from(handle.clone())?;
        // Read as the caller's own, so a foreign handle and an absent one are the same `NotFound` and neither
        // of them embeds, writes or journals anything.
        let existing = service
            .state(&fact)
            .await?
            .ok_or(alexandria_storage::error::StorageError::NotFound)?;
        // `MutationRepo::state` qualifies by account and workspace, not by partition — a journaled read has
        // to be able to describe a quarantined row it is asked to move — so the review partition's reach is
        // the *writer's* refusal, exactly as `update_within` states it. Answered as `NotFound`, the same
        // bytes a foreign or absent id gives: refusing the corpus path while telling the caller the row is
        // in the review partition would turn `delete_memory` into a probe for what an owner has hidden,
        // and `list_unassigned_memories` hands those ids to agents.
        if reach == DeletionReach::Corpus
            && matches!(
                existing.scope,
                alexandria_storage::models::MemoryScope::Unassigned
            )
        {
            return Err(crate::error::ServiceError::NotFound);
        }
        let expected = [ExpectedState::of(&existing)];
        let contribution = service
            .contribution_removal(fact.as_record_id())
            .await
            .map_err(crate::error::ServiceError::from)?;
        let mut plan = Plan::new(operation_kind::DELETE)
            .effect(effect::DELETE)
            .in_space(Some(self.embedding.model_id()))
            .fact(FactChange::Update {
                fact,
                expected: existing.version,
                content: None,
                tags: None,
                confidence: None,
                embedding: None,
                // Hiding a row must not also *un-quarantine* it. `Visibility::deleted()` carries
                // `quarantined_at: None`, and `update_fact` writes both halves whenever visibility is
                // present, so the shape this replaces turned every delete of a review row into a silent
                // revocation of a state the owner never withdrew — and journaled that revocation as the
                // visibility change. The deletion is the decision; the access state survives it.
                visibility: Some(Visibility {
                    deleted: true,
                    quarantined_at: existing.visibility.quarantined_at,
                }),
                scope: None,
            });
        if let Some(contribution) = contribution {
            plan = plan.cluster(contribution);
        }
        let operation = OperationId::new(uuid::Uuid::new_v4().to_string());
        service
            .commit(&operation, &expected, &plan)
            .await
            .map_err(crate::error::ServiceError::from)?;
        Ok(id.to_string())
    }
    /// The move, given an already-resolved scope — **the owner's own act, and the only journaled move body.**
    ///
    /// `delete_memory` has no move sibling among the tools by design: an agent proposes where a memory
    /// belongs and a human applies it. This is what that human press reaches, and the page and any later
    /// owner-facing surface share it, so there is one notion of what a move writes.
    ///
    /// Three things a page must not get to decide on its own, and does not:
    ///
    /// * **The destination.** A project is named by the caller's *own* inventory — resolved through
    ///   [`ScopeService::resolve_destination`], the same resolver the tools use, never a second
    ///   normalizer, and an unresolvable name is `NotFound` rather than a created partition. Implicit
    ///   project creation lives in `establish_project` alone.
    /// * **The revision it expected.** `expected` is the number the page rendered, not one re-read at
    ///   commit time, so a memory that moved since the owner looked at it conflicts instead of being
    ///   overwritten by an opinion formed against a row nobody is looking at. The pre-read below is a
    ///   *existence and ownership* check; the guard is the caller's number.
    /// * **The destination partition's cluster.** Joining a cluster is a similarity decision against the
    ///   configured `cluster_join_threshold`, which is this server's policy and the engine's arithmetic —
    ///   never storage's. The leaving half needs no policy: `MutationRepo::plan_move` re-derives the
    ///   survivors' centre inside the move's own transaction, under the cluster's guards.
    ///
    /// The initial semantic expectation and named cluster effects are fixed before the shared commit
    /// loop. A derived conflict refreshes only their guards and centroids, at most
    /// [`crate::mutation::MAX_PREPARATIONS`] times, and never selects a different destination.
    pub(crate) async fn move_within(
        &self,
        scope: &RequestScope,
        id: &str,
        destination: &str,
        project: Option<&str>,
        expected: Version,
    ) -> Result<serde_json::Value, crate::error::ServiceError> {
        let handle = surrealdb::types::RecordId::parse_simple(id)
            .map_err(|_| crate::error::ServiceError::InvalidInput(MALFORMED_ID.to_string()))?;
        if handle.table.as_str() != "fact" {
            return Err(crate::error::ServiceError::InvalidInput(
                MALFORMED_ID.to_string(),
            ));
        }
        let fact = FactId::try_from(handle.clone())
            .map_err(|_| crate::error::ServiceError::InvalidInput(MALFORMED_ID.to_string()))?;
        let (destination, descriptor) = ScopeService::new(self.db.as_ref())
            .resolve_destination(scope.owner(), destination, project)
            .await?;

        let service = ApplyService::new(self.db.as_ref(), scope.owner(), scope.read());
        // Foreign or absent answers `NotFound` before anything is ranked, so the response cannot be used
        // to ask whether an id is stored — the same outward equivalence the update and delete paths give.
        let existing = service
            .state(&fact)
            .await?
            .ok_or(alexandria_storage::error::StorageError::NotFound)?;
        if existing.version != expected {
            return Err(crate::error::ServiceError::Stale);
        }
        if existing.scope == destination {
            // Not a no-op journal row: the page asked for a move that is already the state of the world,
            // and a `semantic_operation` claiming a change that changed nothing is exactly the audit
            // noise the journal exists to avoid.
            return Ok(serde_json::json!({
                "status": "already_there",
                "id": id,
                "scope": descriptor
                    .as_ref()
                    .map(|p| p.display_name.clone().unwrap_or(p.canonical_identity.clone()))
                    .unwrap_or_else(|| crate::scope_service::destination_label(&destination)),
            }));
        }

        let plan = self
            .prepare_move_plan(scope, &existing, &destination)
            .await
            .map_err(crate::error::ServiceError::from)?;
        let operation = OperationId::new(uuid::Uuid::new_v4().to_string());
        let applied = service
            .commit(&operation, &[ExpectedState::of(&existing)], &plan)
            .await?;
        Ok(serde_json::json!({
            "status": "ok",
            "id": id,
            "scope": crate::scope_service::destination_label(&destination),
            "project": descriptor.map(|p| p.id),
            "operation": applied.operation,
        }))
    }

    /// Pin both membership effects once; later preparation may refresh derived data, not rank again.
    pub(crate) async fn prepare_move_plan(
        &self,
        scope: &RequestScope,
        existing: &alexandria_storage::models::FactSnapshot,
        destination: &MemoryScope,
    ) -> anyhow::Result<Plan> {
        let service = ApplyService::new(self.db.as_ref(), scope.owner(), scope.read());
        let reconciliation = self.prepare_arrival(scope, existing, destination).await?;
        let leaving = service.cluster_of(existing.id.as_record_id()).await?;
        if let Some((source, _)) = &leaving
            && matches!(&reconciliation.arriving, DestinationMembership::Join { expectation, .. }
                if &expectation.cluster == source)
        {
            return Err(alexandria_storage::error::StorageError::InvalidInput(
                "a scope move cannot leave and join the same cluster".to_string(),
            )
            .into());
        }
        let mut plan =
            Plan::new(operation_kind::MOVE)
                .effect(effect::SCOPE)
                .fact(FactChange::Update {
                    fact: existing.id.clone(),
                    expected: existing.version,
                    content: None,
                    tags: None,
                    confidence: None,
                    embedding: None,
                    visibility: None,
                    scope: Some(destination.clone()),
                });
        if let Some(embedding) = &existing.embedding {
            plan = plan.embedding(&embedding.vector);
        }
        if let Some((cluster, _)) = leaving {
            plan = plan.cluster(
                service
                    .departure(&cluster, existing.id.as_record_id())
                    .await?,
            );
        }
        let cluster = match reconciliation.arriving {
            DestinationMembership::Join { expectation, .. } => Some(expectation.cluster),
            DestinationMembership::Create { .. } => None,
            DestinationMembership::Unclustered => return Ok(plan),
        };
        Ok(plan.cluster(ApprovedCluster::Attach {
            cluster,
            member: existing.id.as_record_id().clone(),
            contributes: existing.visibility == Visibility::live(),
        }))
    }

    /// The destination partition's cluster this moved memory belongs to, ranked by the server's policy.
    ///
    /// Ranked against the **destination partition's own lawful candidates**, not the caller's current read
    /// and not the workspace's cluster list: a memory moving into a project joins that project's cluster or
    /// forms one there, and joining against the partition it is *leaving* would re-attach it to the
    /// membership the same transaction detaches. That was the defect this call's ranking once had — it read
    /// `list_with_counts`, which is workspace-wide by design and truthful about it — and under a constant
    /// vector the wrong partition's cluster ranked *first*, so the move named one cluster as both sides of
    /// itself. See [`ScopedClusterRepo::partition_candidates`] for what a candidate has to be.
    ///
    /// `Unclustered` is a real answer with **three** producers, and each leaves the journal stating no
    /// membership effect it did not write: a memory with no vector to rank; a ranking that named no centre
    /// above the join threshold — which is what [`ClusterAssignment::NewCluster`] means, and it is stated
    /// as `Create` rather than as ungrouped; and a destination cluster whose stored membership has no
    /// dimension in common with the arriving vector, which is a corrupt cluster and is *ungrouped* rather
    /// than given the arriving vector as its centre. An `Unassigned` destination is a fourth, deliberate
    /// producer decided before any read: the accepted access policy refuses `ReadSelection::One(Unassigned)`
    /// (`UnassignedReview` is the only authorized read of that partition), and a classification may not
    /// grant recall a move never earned. Everything else — a destination read that will not resolve, a
    /// population the engine refuses — propagates as the typed failure it is instead of being flattened
    /// into "grouped by nothing", because a ranking that failed is not a ranking that chose nothing.
    pub(crate) async fn prepare_arrival(
        &self,
        scope: &RequestScope,
        existing: &alexandria_storage::models::FactSnapshot,
        destination: &MemoryScope,
    ) -> Result<MoveReconciliation, alexandria_storage::error::StorageError> {
        // Explicit Ungrouped before the destination read, for the access boundary named above — not an
        // error swallowed into a ranking answer.
        if matches!(destination, MemoryScope::Unassigned) {
            return Ok(MoveReconciliation::ungrouped());
        }
        let Some(embedding) = existing.embedding.as_ref() else {
            return Ok(MoveReconciliation::ungrouped());
        };
        let vector = embedding.vector.clone();
        // The destination partition's own read, re-authorized for *this* request: `AccessRepo::select`
        // re-reads the account and the workspace link, so a destination that stopped being the caller's
        // between the resolve above and this call fails here rather than ranking somebody else's
        // clusters. A genuine failure propagates — it is not a ranking outcome.
        let read = AccessRepo::new(&self.db)
            .select(scope.owner(), ReadSelection::One(destination.clone()))
            .await?;
        let clusters = ScopedClusterRepo::new(self.db.inner(), scope.owner(), &read)
            .partition_candidates()
            .await?;
        let candidates = DestinationCandidates::from_clusters(clusters)?;
        let repo = MutationRepo::new(self.db.inner());
        let reconciliation = match candidates.choose(&vector, self.cluster_join_threshold)? {
            None => MoveReconciliation {
                leaving: None,
                arriving: DestinationMembership::Create { centroid: vector },
            },
            Some(cluster) => {
                let expectation = repo.cluster_expectation(scope.owner(), cluster).await?;
                // One sample, the same one the transaction validates: the expectation's population is
                // both the mean's input and the write's guard, so a second member query here could
                // disagree with the guard and rank against numbers the commit never sees.
                let held: Vec<&[f32]> = expectation
                    .population
                    .iter()
                    .map(|member| member.embedding.as_slice())
                    .collect();
                // Filing preserves hidden status; only an eligible arrival enters the live mean.
                let mean = if existing.visibility == Visibility::live() {
                    alexandria_engine::clusters::centroid_with(&held, &vector)
                } else {
                    alexandria_engine::clusters::centroid(&held)
                };
                let Some(centroid) = mean else {
                    tracing::warn!(
                        cluster = %alexandria_storage::record_id_to_string(cluster),
                        members = expectation.population.len(),
                        "destination population has no valid mean; filing the arrival ungrouped",
                    );
                    return Ok(MoveReconciliation::ungrouped());
                };
                MoveReconciliation {
                    leaving: None,
                    arriving: DestinationMembership::Join {
                        expectation,
                        centroid,
                    },
                }
            }
        };
        Ok(reconciliation)
    }

    /// The interim **unscoped** store.
    ///
    /// The debug UI's Query Tester seeds through it and the binary's end-to-end engine tests store through
    /// it, because neither has an admitted principal (D1 defers the debug population to Task 9). It is *not*
    /// what `store_memory` does: the handler goes through [`Self::store_response`], which carries an
    /// admitted scope, and `request_facing_modules_never_reach_an_interim_or_unscoped_surface` fails if a
    /// `#[tool]` handler ever calls this again. The rows it writes carry no owner, so no scoped reader can
    /// see them — which is the disclosed interim cost, not a hidden one.
    pub async fn do_store_memory(&self, params: StoreMemoryParams) -> anyhow::Result<String> {
        self.store_body(None, None, params).await
    }

    /// Stores one memory, stamped with the admitted caller when there is one.
    ///
    /// The arms differ in exactly one thing — whether an owner exists to stamp — and that difference is now
    /// bigger than a stamp: the scoped arm commits **one journaled transaction**, while the interim arm keeps
    /// the six separate round trips it always had, because there is no authority to record the change under.
    ///
    /// The scoped order is: embed (outside any transaction — a provider round trip inside one would hold a
    /// write lock open for as long as the model takes), then rank the caller's own clusters, then commit fact,
    /// heat seed, provenance row *and* its link, cluster membership, derived centroid and session link as one
    /// `semantic_operation`. On refusal none of them happened, which is the property the old shape could not
    /// offer: it could leave a live row with a vector and no provenance, or a cluster whose centroid averaged a
    /// member set nobody had approved.
    ///
    /// The heat seed moved *inside* the transaction for the scoped arm (`PreparedChangeSet::heat`), which closes
    /// the carry the interim arm still has: a journaled store cannot leave a live fact with no heat row, because
    /// the row and its heat are the same commit.
    async fn store_body(
        &self,
        scope: Option<&RequestScope>,
        partition: Option<MemoryScope>,
        params: StoreMemoryParams,
    ) -> anyhow::Result<String> {
        let tags = params.tags.clone().unwrap_or_default();
        let now = Utc::now();

        // 1. Embed, outside the transaction.
        let embeddings = self.embedding.embed(&[&params.content]).await?;
        let embedding = &embeddings[0];

        // 2. The scoped arm journals; the interim arm keeps the legacy round trips.
        if let Some(scope) = scope {
            let partition = partition.ok_or_else(|| {
                anyhow::anyhow!("a store arm carried a scope without its write target")
            })?;
            return self
                .store_journaled(scope, partition, params, tags, embedding, now)
                .await;
        }

        // 3. The interim path: no owner to record the change under, so nothing is journaled. Every row this
        //    block writes carries no `account`, which is why no scoped reader can see it — the disclosed
        //    interim cost, not a hidden one.
        let memory_repo = MemoryRepo::new(self.db.inner());
        let (fact_id, fact_typed) = memory_repo
            .create_fact_with_id(&params.content, 0.5, embedding, &tags)
            .await?;

        // Heat state, then a bare provenance create with no link (v009 stamped the link to a row this path
        // never made, so it stays as it was).
        let heat_repo = HeatRepo::new(self.db.inner());
        heat_repo.create_for_memory(&fact_id, 1.0).await?;
        self.db
            .inner()
            .query("CREATE provenance SET kind = 'user', timestamp = time::now()")
            .await?
            .check()?;

        // Cluster assignment from the store-wide list, which is what makes this arm unsafe for a request.
        self.assign_to_cluster_and_update(None, None, embedding, &fact_typed)
            .await?;

        // Session linkage (implicit create on first use)
        if let Some(ref session_id) = params.session_id {
            let session_repo = SessionRepo::new(self.db.inner());
            // The interim repo links by typed record id (it is the unscoped arm, and its handle lookup is what
            // v010 made unsafe); the scoped arm links by external handle, which is the account-qualified form.
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

        Ok(fact_id)
    }

    /// The scoped store: one plan, one journal row, one commit.
    ///
    /// Split from [`Self::store_body`] because the six-hop interim arm still exists for callers with no
    /// admitted principal, and keeping the two shapes in one function with `match (scope, partition)` arms was
    /// how the journal and the round trips ended up interleaved.
    async fn store_journaled(
        &self,
        scope: &RequestScope,
        partition: MemoryScope,
        params: StoreMemoryParams,
        tags: Vec<String>,
        embedding: &[f32],
        now: DateTime<Utc>,
    ) -> anyhow::Result<String> {
        let target = Self::write_target(partition.clone())?;
        let service = ApplyService::new(self.db.as_ref(), scope.owner(), scope.read());
        let model = self.embedding.model_id().to_string();
        // Rank the **write partition's own** candidates. Nothing in this call may consult a store-wide
        // cluster list — joining on one would attach another account's row to this account's cluster and
        // rewrite that centroid — and within the account it may not consult the workspace either, because a
        // row filed into Personal must not join the project's cluster just because both sit in one
        // workspace. `cluster_infos_for` is the recall read's workspace-wide view and is deliberately not
        // used here.
        let candidates = self
            .destination_cluster_candidates(scope, &partition)
            .await?;
        let assignment = candidates
            .choose(embedding, self.cluster_join_threshold)?
            .cloned();

        // The session row is resolved before the transaction, because the link inside it needs a handle to
        // point at, and the external id is kept alongside it for the activity stamp that follows. An implicit
        // create that lands and is then refused by the mutation leaves an empty session — which is what the old
        // path left on any failure after its first write too, and is better than the alternative of journaling a
        // link to a row the transaction minted itself.
        let session = match params.session_id.as_deref() {
            Some(session_id) => {
                let resolved = ScopedSessionRepo::new(self.db.inner(), scope.owner().clone())
                    .find_or_create(
                        session_id,
                        params.agent_id.as_deref(),
                        params.model.as_deref(),
                    )
                    .await
                    .map_err(anyhow::Error::from)?;
                Some((
                    session_id.to_string(),
                    resolved
                        .id
                        .ok_or_else(|| anyhow::anyhow!("session row has no id"))?,
                ))
            }
            None => None,
        };

        let mut plan = Plan::new(operation_kind::STORE)
            .effect(effect::STORE)
            .effect(effect::VECTOR)
            .effect(effect::PROVENANCE)
            .fact(FactChange::Create {
                scope: partition,
                content: params.content.clone(),
                tags,
                confidence: 0.5,
                embedding: crate::mutation::vector(embedding, Some(&model)),
                visibility: Visibility::live(),
                metadata: None,
            })
            // The description of where this memory came from, linked in the same transaction as the row it
            // describes. The default target is slot zero, which is this change's only create.
            .provenance("user", now)
            // Seeded here rather than after the commit: a stored memory is never a memory with no heat row.
            .heat(1.0)
            .in_space(Some(&model));

        plan = match assignment {
            Some(cluster) => plan.cluster(ApprovedCluster::Join { cluster, member: 0 }),
            None => plan.cluster(ApprovedCluster::Create { member: 0 }),
        };
        plan = plan.embedding(embedding);

        // The session link is part of the change, so the effect that names it is stated with it rather than
        // written afterwards.
        if let Some((_external, handle)) = &session {
            plan = plan
                .link_session(handle.clone(), RelationTarget::NewFact(0))
                .effect(EdgeKind::SessionMembership.effect());
        }

        let operation = OperationId::new(uuid::Uuid::new_v4().to_string());
        let applied = service
            .commit(&operation, &[], &plan)
            .await
            .map_err(anyhow::Error::from)?;
        let created = applied
            .created
            .first()
            .ok_or_else(|| anyhow::anyhow!("a journaled store minted no fact"))?
            .clone();

        // The session's activity stamp is not semantic state — it is the same bookkeeping `touch` has
        // always done, and it belongs after the commit because a refused store must not leave a session
        // looking busy.
        //
        // Because it is after the commit, it also cannot be allowed to *report* like the commit: the memory
        // is written, journaled and readable, and returning `Err` here told the caller nothing was stored.
        // A client that believed that message would store the same content again and get a second memory —
        // the duplicate is the failure, not the stale timestamp. A failed touch is logged; the returned
        // committed id stays successful. No warning field is returned by this id-only helper.
        if let Some((external, _handle)) = &session
            && let Err(error) = ScopedSessionRepo::new(self.db.inner(), scope.owner().clone())
                .touch(external)
                .await
        {
            tracing::warn!(
                session_id = external.as_str(),
                error = %error,
                "stored a memory and journaled it, but could not stamp its session's activity",
            );
        }
        // The write target was resolved above and is not re-stated here: `FactChange::Create` carries the
        // partition, and `apply` refuses a scope this caller may not write.
        drop(target);
        let id = record_id_to_string(created.as_record_id());
        Ok(id)
    }

    /// The storage write target one resolved [`MemoryScope`] spells.
    ///
    /// A translation, not a decision: the decision was [`Self::admit_write_target`]'s, which resolved the
    /// caller's words against the caller's own workspace and admitted a read population granting exactly
    /// that partition. `Unassigned` has no arm here on purpose — no public store or import path may stage
    /// a row into the review partition (owner decision D4, rule 3), so reaching it is a refusal rather
    /// than a fallback.
    fn write_target(
        partition: MemoryScope,
    ) -> anyhow::Result<alexandria_storage::repos::WriteTarget> {
        Ok(match partition {
            MemoryScope::Personal => alexandria_storage::repos::WriteTarget::Personal,
            MemoryScope::Project(project) => {
                alexandria_storage::repos::WriteTarget::Project(project)
            }
            MemoryScope::Unassigned => anyhow::bail!(
                "unassigned is a review partition: no store or import path may write it"
            ),
        })
    }

    pub async fn do_update_memory(
        &self,
        ctx: &RequestContext<RoleServer>,
        params: UpdateMemoryParams,
    ) -> anyhow::Result<String> {
        self.update_core(ctx, params).await
    }

    /// The interim **unscoped** update, kept for callers that have no admitted principal yet.
    ///
    /// Like the debug UI's retrieval arm (D1, deferred to Task 9) and the binary's end-to-end engine tests,
    /// which assert re-embedding and tag-only behaviour on rows written by the interim store path — rows
    /// that carry no owner, and so are correctly invisible to [`Self::do_update_memory`]. This is not a
    /// request-facing entry point: no `#[tool]` handler calls it, and the row it writes is matched by id
    /// with no owner predicate, which is the hole the scoped arm exists to close.
    pub async fn do_update_memory_interim(
        &self,
        params: UpdateMemoryParams,
    ) -> anyhow::Result<String> {
        let repo = MemoryRepo::new(self.db.inner());
        let existing = repo
            .get_fact(&params.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Memory not found: {}", params.id))?;
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
        if new_embedding.is_some() {
            let edge_repo = EdgeRepo::new(self.db.inner());
            let old_snapshot_id = MemoryRepo::new(self.db.inner())
                .create_fact(
                    &existing.content,
                    existing.confidence,
                    &existing.embedding,
                    &existing.tags,
                )
                .await?;
            MemoryRepo::new(self.db.inner())
                .soft_delete_fact(&old_snapshot_id)
                .await?;
            edge_repo
                .create_edge(&params.id, &old_snapshot_id, "derived_from", 1.0)
                .await
                .ok();
        }
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

    /// The update, given an already-resolved scope.
    ///
    /// Two defects were closed here in sequence, and both are still the reason this shape exists:
    ///
    /// * **The owner predicate.** `MemoryRepo::get_fact` was `SELECT * FROM type::record($id)` with no account
    ///   in it, so any admitted caller could read *and then rewrite* another account's memory. The row is
    ///   resolved through the caller's own population first, so a foreign id and an absent id are the same
    ///   `NotFound` before anything is embedded.
    /// * **Atomicity.** The content change, its vector, the hidden lineage snapshot, the `derived_from` edge and
    ///   the supersede were four round trips. `failed_lineage_is_atomic` in `tests/mutation_test.rs` proves the
    ///   engine rolls all of them back; this is the request path that has to *use* it, because the same four
    ///   calls here could still leave a live row whose text described one thing and whose vector another.
    ///
    /// `Assigned`, for the reason documented on [`Self::delete_core`]: correcting one's own row is authorized by
    /// the owner and the row's partition, not by whatever a read happened to select.
    async fn update_core(
        &self,
        ctx: &RequestContext<RoleServer>,
        params: UpdateMemoryParams,
    ) -> anyhow::Result<String> {
        let scope = self.admit(ctx, ReadSelection::Assigned).await?;
        self.update_within(&scope, params).await
    }

    /// The update against an already-resolved scope.
    ///
    /// Split out because a `RequestContext<RoleServer>` carries a `Peer` only a live rmcp service builds:
    /// admission is exercised through the transport suite, while the owner predicate and the response shapes
    /// are exercised directly.
    pub(crate) async fn update_within(
        &self,
        scope: &RequestScope,
        params: UpdateMemoryParams,
    ) -> anyhow::Result<String> {
        self.update_within_at(scope, params, None).await
    }

    /// The same edit with the **revision the page rendered** bound to it.
    ///
    /// `update_memory` has no expected-revision parameter and gains none: an agent's edit is authorized by
    /// its owner-scoped admission, and the tool schema is a public contract. A human's edit is different
    /// in kind — the owner looked at a row and corrected *that*, and a page that advertises "reload if it
    /// changed" cannot honour the promise if the handler re-reads the current version at POST time and
    /// then compares it with itself. So the browser passes the number it displayed, and this checks it
    /// against the row the journal is about to guard. `None` is the tool path's shape, unchanged.
    pub(crate) async fn update_within_at(
        &self,
        scope: &RequestScope,
        params: UpdateMemoryParams,
        expected: Option<Version>,
    ) -> anyhow::Result<String> {
        let handle = surrealdb::types::RecordId::parse_simple(&params.id)
            .map_err(|_| crate::error::ServiceError::InvalidInput(MALFORMED_ID.to_string()))?;
        if handle.table.as_str() != "fact" {
            return Err(crate::error::ServiceError::InvalidInput(MALFORMED_ID.to_string()).into());
        }
        let service = ApplyService::new(self.db.as_ref(), scope.owner(), scope.read());
        // The state the journal will compare against, read as *the caller's own*. A foreign id and an absent
        // one are one answer here, before anything is embedded.
        let existing =
            service
                .state(&FactId::try_from(handle.clone()).map_err(|_| {
                    crate::error::ServiceError::InvalidInput(MALFORMED_ID.to_string())
                })?)
                .await?
                .ok_or(alexandria_storage::error::StorageError::NotFound)?;
        // The revision the page rendered, checked before a single vector is produced. Answered as `Stale` —
        // the same class the journaled commit gives for a row that moved under the caller — so a browser
        // cannot tell "your revision lost the race" from "the store refused the write", and a UI that
        // advertises reload-on-stale is bound to a number that can actually lose.
        if let Some(expected) = expected
            && existing.version != expected
        {
            return Err(alexandria_storage::error::StorageError::Stale.into());
        }
        let model = self.embedding.model_id().to_string();

        // A content change re-embeds; the vector and the text move together or not at all, which is the whole
        // reason this path is one transaction now.
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

        // A superseded row keeps its own partition — but `Unassigned` has no such thing as a superseded row,
        // because staging a *new* row into the review partition is what owner decision D4 rule 3 forbids, and a
        // lineage snapshot is a new row. `MutationRepo::state` qualifies by account and workspace rather than by
        // partition (a journaled read must be able to describe a quarantined memory it is asked to move), so
        // this refusal is the writer's job and not the read's: reaching it is a `NotFound`, the same answer a
        // foreign or absent id gives, so hiding the review partition's contents is not a way to learn they exist.
        if matches!(
            existing.scope,
            alexandria_storage::models::MemoryScope::Unassigned
        ) {
            return Err(alexandria_storage::error::StorageError::NotFound.into());
        }

        let mut plan = Plan::new(operation_kind::UPDATE).in_space(Some(&model));
        if params.content.is_some() {
            plan = plan.effect(effect::CONTENT);
        }
        if params.tags.is_some() {
            plan = plan.effect(effect::TAGS);
        }
        if params.confidence.is_some() {
            plan = plan.effect(effect::CONFIDENCE);
        }

        // The lineage snapshot is a *new row*, so it needs a partition that is not an inference: the partition
        // of the row being superseded, read back from that row. Writing it unowned would hide the lineage from
        // the very account that owns the memory it describes, and writing it into Personal because the caller
        // happened to be reading Personal would move a project memory's history into another partition. The
        // snapshot carries the *old* vector, which is what makes it a legible before-state rather than a second
        // copy of the new one.
        if new_embedding.is_some() {
            let prior = existing.embedding.as_ref().ok_or(
                alexandria_storage::error::StorageError::InvalidInput(
                    "a journaled row carries no vector to snapshot".to_string(),
                ),
            )?;
            let snapshot = alexandria_storage::models::EmbeddingSnapshot::new(
                prior.vector.clone(),
                prior.model.as_deref(),
            );
            plan = plan
                .effect(effect::VECTOR)
                .effect(effect::LINEAGE)
                .fact(FactChange::Create {
                    scope: existing.scope.clone(),
                    content: existing.content.clone(),
                    tags: existing.tags.clone(),
                    confidence: existing.confidence,
                    embedding: snapshot,
                    // Hidden on creation: a snapshot enters search only if something un-hides it.
                    visibility: Visibility::deleted(),
                    metadata: None,
                })
                // The edge is written by the same transaction as the row it points at, in the direction the
                // superseded copy is reachable *from* the live row: a snapshot nothing links to is a hidden row
                // with no path back, which is the half-state this replaces.
                .relate(RelationChange::memory(
                    "derived_from",
                    RelationTarget::Existing(handle.clone()),
                    RelationTarget::NewFact(0),
                    1.0,
                ))
                .effect(EdgeKind::Memory("derived_from".to_string()).effect());
        }

        plan = plan.fact(FactChange::Update {
            fact: FactId::try_from(handle.clone())
                .map_err(|_| crate::error::ServiceError::InvalidInput(MALFORMED_ID.to_string()))?,
            expected: existing.version,
            content: params.content.clone(),
            tags: params.tags.clone(),
            confidence: params.confidence,
            embedding: new_embedding
                .as_ref()
                .map(|vector| crate::mutation::vector(vector, Some(&model))),
            visibility: None,
            scope: None,
        });

        // Only an eligible member's changed vector affects the live centre. Hidden edits remain allowed.
        // Find the existing cluster by membership, not by re-ranking.
        if let Some(ref embedding) = new_embedding
            && existing.visibility == Visibility::live()
            && let Some((cluster, _expectation)) = service.cluster_of(&handle).await?
        {
            plan = plan
                .cluster(ApprovedCluster::Revector {
                    cluster,
                    member: handle.clone(),
                })
                .embedding(embedding);
        }

        let expected = [ExpectedState::of(&existing)];
        let operation = OperationId::new(uuid::Uuid::new_v4().to_string());
        let applied = service
            .commit(&operation, &expected, &plan)
            .await
            .map_err(anyhow::Error::from)?;

        Ok(serde_json::json!({
            "status": "ok",
            "id": params.id,
            "content_changed": new_embedding.is_some(),
            // The journal row this change wrote, so the caller can name the operation it just performed.
            "operation": applied.operation,
        })
        .to_string())
    }

    /// Imports a document **as the caller**, so every row it creates carries their account, workspace
    /// and partition.
    ///
    /// This is the defect the fourth lane found. The path previously created its `raw` document, its
    /// chunk facts and its session through the unscoped repositories, so a *trusted-local* write — the
    /// one D4 lets through, because its partition is lawfully its own Personal one — produced rows with
    /// `account`, `workspace` and `scope` all `NONE`. Those rows are invisible to `ScopeFilter`, which
    /// selects a partition, so the author could never retrieve what they had just imported and nothing
    /// along the way reported an error: a silent data-loss shape on the legitimate write path, invisible
    /// to every hosted test because hosted imports are refused before any write.
    pub(crate) async fn import_scoped(
        &self,
        scope: &RequestScope,
        partition: MemoryScope,
        params: ImportDocumentParams,
    ) -> anyhow::Result<String> {
        self.import_body(Some(scope), Some(partition), params).await
    }

    /// The interim **unscoped** import.
    ///
    /// The debug surface has no import path and no admitted principal (D1, Task 9), and the binary's
    /// end-to-end engine tests drive imports the same way they drive `do_store_memory`. Every
    /// `#[tool]` handler goes through [`AlexandriaServer::import_scoped`] instead, which carries an
    /// admitted scope and stamps the rows it creates.
    pub async fn do_import_document_interim(
        &self,
        params: ImportDocumentParams,
    ) -> anyhow::Result<String> {
        self.import_body(None, None, params).await
    }

    async fn import_body(
        &self,
        scope: Option<&RequestScope>,
        partition: Option<MemoryScope>,
        params: ImportDocumentParams,
    ) -> anyhow::Result<String> {
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
                    // 800 chars is about 180-200 tokens of Latin prose, inside a 256-token limit
                    // but over the default 128. Not a bound either way: CJK tokenizes near one
                    // token per character. A chunk past `embedding.max_tokens` embeds on its head
                    // and logs the overflow.
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
        // The partition every chunk lands in, resolved once for the whole batch from the request rather
        // than per chunk and rather than inferred from the granted population.
        let reported = partition.as_ref().map(scope_label);
        // The partition is kept as well as translated: `WriteTarget` is storage's write shape, while the
        // journaled `FactChange::Create` states the `MemoryScope` the row belongs to, and both come from the
        // one decision `admit_write_target` made for this request.
        let target = match (scope, partition.clone()) {
            (Some(_), Some(partition)) => Some(Self::write_target(partition)?),
            (None, None) => None,
            _ => anyhow::bail!("an import arm carried a scope without its write target"),
        };
        // The raw row's typed handle, kept beside the display string the response reports. The journal's
        // `extracted_from` edge binds a `RecordId`, and a display form is not a parser-safe SQL form
        // (`AGENTS.md`), so the handle is taken from the create rather than re-parsed from its rendering.
        // Where the source document is written is a decision about the journal, not about the loop.
        //
        // The scoped arm must **not** create the `raw` row up front: an import whose every chunk then failed
        // left a source document that no operation recorded and no chunk links to — an unjournaled orphan
        // describing a document the owner's corpus never received, invisible to owner history and reachable
        // by nothing except a listing of the `raw` table. So the scoped path carries the content with the
        // **first chunk that actually lands** and takes the handle back from that commit; the interim path,
        // which has no admitted principal and therefore no journal to be coherent with, keeps the single
        // pre-loop create it always had.
        let mut raw_handle = match scope {
            Some(_) => None,
            None => {
                let text = repo.create_raw(&params.content).await?;
                Some(
                    surrealdb::types::RecordId::parse_simple(&text)
                        .map_err(|_| anyhow::anyhow!("raw row has no usable id"))?,
                )
            }
        };

        let heat_repo = HeatRepo::new(self.db.inner());
        let edge_repo = EdgeRepo::new(self.db.inner());
        let mut created_ids = Vec::new();

        // Add batch_id to tags so chunks can be found together
        let mut import_tags = tags;
        import_tags.push(format!("import_batch:{batch_id}"));

        // Session linkage (implicit create on first use), resolved once for all chunks
        let scoped_session =
            scope.map(|scope| ScopedSessionRepo::new(self.db.inner(), scope.owner().clone()));
        let interim_session = SessionRepo::new(self.db.inner());
        // The session row's typed handle, for the journal's link; the text form stays what `touch` and the
        // interim linker take.
        let session_handle = match params.session_id {
            Some(ref session_id) => Some(match &scoped_session {
                Some(repo) => repo
                    .find_or_create(
                        session_id,
                        params.agent_id.as_deref(),
                        params.model.as_deref(),
                    )
                    .await
                    .map_err(anyhow::Error::from)?
                    .id
                    .ok_or_else(|| anyhow::anyhow!("session row has no id"))?,
                None => {
                    let text = interim_session
                        .find_or_create(
                            session_id,
                            params.agent_id.as_deref(),
                            params.model.as_deref(),
                        )
                        .await?;
                    surrealdb::types::RecordId::parse_simple(&text)
                        .map_err(|_| anyhow::anyhow!("session row has no usable id"))?
                }
            }),
            None => None,
        };
        let session_rid = session_handle.as_ref().map(record_id_to_string);

        for chunk in &chunks {
            // Embed
            let embeddings = self.embedding.embed(&[chunk.as_str()]).await?;
            let embedding = &embeddings[0];

            // Create fact with import confidence, in the caller's partition when there is a caller.
            //
            // The scoped arm journals **one operation per chunk**, which is the shape classification approval already
            // uses for a classification batch: the approved effect set of one entry is that one memory, so a
            // transaction spanning a whole document could not report which entries a reviewer approved or let
            // one stale chunk conflict without unwinding the chunks that were lawful. What changes is that a
            // chunk is now whole or absent: its row, vector, heat, source link, cluster membership and centroid
            // commit together, where the loop below used to write them as five calls with `.ok()` on the
            // `extracted_from` edge — so a chunk could live with no path back to the document it came from.
            if let Some(scope) = scope {
                let partition = partition.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("an import arm carried a scope without its write target")
                })?;
                let service = ApplyService::new(self.db.as_ref(), scope.owner(), scope.read());
                let model = self.embedding.model_id().to_string();
                // Whether *this* chunk's transaction mints the source document. The first chunk that lands
                // does; every later one links to the row that commit created, so one document has one `raw`
                // row and a failed import has none.
                let mints_raw = raw_handle.is_none();
                let mut plan = Plan::new(operation_kind::IMPORT)
                    .effect(effect::CHUNK)
                    .effect(effect::VECTOR)
                    .effect(EdgeKind::ExtractedFrom.effect())
                    .in_space(Some(&model))
                    // Imports seed higher heat than a store: a document's chunks are deliberately bulk-loaded.
                    .heat(2.0)
                    .fact(FactChange::Create {
                        scope: partition.clone(),
                        content: chunk.clone(),
                        tags: import_tags.clone(),
                        confidence: 1.0,
                        embedding: crate::mutation::vector(embedding, Some(&model)),
                        visibility: Visibility::live(),
                        metadata: None,
                    });
                // The source document rides in the transaction of the first chunk that lands, so a
                // document that produced no chunk leaves no `raw` row for anybody to wonder about.
                if mints_raw {
                    plan = plan.raw(params.content.clone());
                }
                // The link to the source document, written by the same transaction as the chunk that needs
                // it: a chunk cannot live with no path back to its document, and the row cannot exist
                // without at least one chunk that points at it.
                plan = plan.relate(RelationChange::extracted_from(
                    RelationTarget::NewFact(0),
                    match &raw_handle {
                        Some(handle) => RelationTarget::Existing(handle.clone()),
                        None => RelationTarget::NewRaw(0),
                    },
                ));
                if let Some(handle) = session_handle.as_ref() {
                    // The journal links the session row to this chunk's fact in the same transaction, which is
                    // what replaces `add_memory` as a sixth round trip that could fail after the fact landed.
                    plan = plan
                        .link_session(handle.clone(), RelationTarget::NewFact(0))
                        .effect(EdgeKind::SessionMembership.effect());
                }
                let candidates = self
                    .destination_cluster_candidates(scope, partition)
                    .await?;
                plan = match candidates.choose(embedding, self.cluster_join_threshold)? {
                    Some(cluster) => plan.cluster(ApprovedCluster::Join {
                        cluster: cluster.clone(),
                        member: 0,
                    }),
                    None => plan.cluster(ApprovedCluster::Create { member: 0 }),
                };
                plan = plan.embedding(embedding);
                let operation = OperationId::new(uuid::Uuid::new_v4().to_string());
                let applied = service
                    .commit(&operation, &[], &plan)
                    .await
                    .map_err(anyhow::Error::from)?;
                let created = applied
                    .created
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("an imported chunk minted no fact"))?;
                created_ids.push(record_id_to_string(created.as_record_id()));
                if mints_raw {
                    let raw = applied.created_raw.first().cloned().ok_or_else(|| {
                        anyhow::anyhow!("a chunk that minted its source reported none")
                    })?;
                    raw_handle = Some(raw);
                }
                continue;
            }

            // Both arms carry the TYPED id alongside the display string: the cluster assignment
            // below binds typed, and the display string feeds the string-shaped callers (edges,
            // session links) unchanged.
            let (fact_id, fact_typed) = match (&scope, &target) {
                (Some(scope), Some(target)) => {
                    let created =
                        ScopedMemoryRepo::new(self.db.inner(), scope.owner(), scope.read())
                            .create_fact(
                                chunk,
                                1.0,
                                embedding,
                                &import_tags,
                                // The caller's resolved partition, not a literal: chunks that landed in
                                // Personal because Personal happened to be granted is exactly the
                                // silent-misfiling failure Task 8 exists to remove.
                                target.clone(),
                                Utc::now(),
                            )
                            .await
                            .map_err(anyhow::Error::from)?;
                    let id = created
                        .id
                        .ok_or_else(|| anyhow::anyhow!("imported fact has no id"))?;
                    (record_id_to_string(&id), id)
                }
                (None, None) => {
                    repo.create_fact_with_id(chunk, 1.0, embedding, &import_tags)
                        .await?
                }
                // Unreachable: `target` is derived from `scope` above. Reached only if a future edit
                // gives one arm without the other, which is the mistake this match refuses to hide.
                _ => anyhow::bail!("an import arm carried a scope without its write target"),
            };

            // Heat state (imports get higher initial heat)
            heat_repo.create_for_memory(&fact_id, 2.0).await?;

            // Create extracted_from edge: chunk → raw document. The interim arm always holds its handle
            // from before the loop — it has no journal to be coherent with, so nothing deferred it.
            let raw_id = raw_handle
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default();
            edge_repo
                .create_edge(&fact_id, &raw_id, "extracted_from", 1.0)
                .await
                .ok();

            // Cluster assignment, into the partition this chunk was just written into. The journaled arm
            // above never reaches here; this is the interim loop, and it passes its own `target` so the
            // ranking is destination-scoped whenever an owner exists at all.
            self.assign_to_cluster_and_update(scope, partition.as_ref(), embedding, &fact_typed)
                .await?;

            if let (Some(repo), Some(session_id)) = (&scoped_session, params.session_id.as_ref()) {
                repo.add_memory(session_id, &fact_id)
                    .await
                    .map_err(anyhow::Error::from)?;
            } else if session_rid.is_some() {
                interim_session
                    .add_memory(&session_rid.clone().unwrap(), &fact_id)
                    .await?;
            }

            created_ids.push(fact_id);
        }

        if let Some(ref session_id) = params.session_id {
            match &scoped_session {
                Some(repo) => {
                    // Activity bookkeeping cannot report already committed chunks as an import failure.
                    if let Err(error) = repo.touch(session_id).await {
                        tracing::warn!(
                            session_id = session_id.as_str(),
                            committed_chunks = created_ids.len(),
                            error = %error,
                            "imported chunks and journaled them, but could not stamp session activity",
                        );
                    }
                }
                None => {
                    interim_session.touch(session_id).await?;
                }
            }
        }

        Ok(serde_json::json!({
            "status": "ok",
            "count": created_ids.len(),
            "ids": created_ids,
            "batch_id": batch_id,
            "raw_id": raw_handle.as_ref().map(record_id_to_string),
            // Where the chunks landed, for the same reason `store_memory` reports it: a batch id cannot
            // tell a caller which partition to read back from.
            "scope": reported.unwrap_or_else(|| "unscoped".to_string()),
        })
        .to_string())
    }

    pub async fn do_retrieve_memories(
        &self,
        params: RetrieveMemoriesParams,
    ) -> anyhow::Result<serde_json::Value> {
        self.retrieve_core(
            params,
            RetrieveOptions {
                activate: true,
                record_access: true,
                apply_floor: true,
            },
            // The interim unscoped diagnostic arm; D1 leaves the debug population to Task 9.
            None,
        )
        .await
    }

    /// Non-mutating retrieval for the debug UI's dry-run mode.
    ///
    /// Identical results to [`Self::do_retrieve_memories`] — the only difference is that both heat
    /// writes are skipped: the spreading-activation warm and the access recording. A diagnostic
    /// query must not perturb the heat it is trying to explain, and an access counted by a dry run
    /// would make a memory look surfaced-but-never-seen.
    pub async fn do_retrieve_memories_dry(
        &self,
        params: RetrieveMemoriesParams,
    ) -> anyhow::Result<serde_json::Value> {
        self.retrieve_core(
            params,
            RetrieveOptions {
                activate: false,
                record_access: false,
                apply_floor: true,
            },
            // The interim unscoped diagnostic arm; D1 leaves the debug population to Task 9.
            None,
        )
        .await
    }

    /// The ranked window *before* the similarity floor, for the debug Query Tester.
    ///
    /// Non-mutating (`activate: false`, `record_access: false`), so unlike
    /// [`Self::do_retrieve_memories`] it never writes heat at all — the honest default for a surface
    /// that exists to explain a ranking rather than change it. Returning the unfiltered window is
    /// what lets the tester name the results the floor suppressed. Note the window is still only
    /// the top `limit` rows: the tool truncates to `limit` *before* filtering, so neither path can
    /// see anything ranked below that.
    pub async fn do_retrieve_memories_unfiltered(
        &self,
        params: RetrieveMemoriesParams,
    ) -> anyhow::Result<serde_json::Value> {
        self.retrieve_core(
            params,
            RetrieveOptions {
                activate: false,
                record_access: false,
                apply_floor: false,
            },
            // The interim unscoped diagnostic arm; D1 leaves the debug population to Task 9.
            None,
        )
        .await
    }

    /// Shared retrieval implementation. `options` selects the two optional stages — the
    /// spreading-activation write and the similarity floor — and nothing else; every other step
    /// (embedding, loading, ranking, result building) is the same code on every path, which is
    /// what makes dry results equal wet results and the unfiltered window a superset of both.
    ///
    /// `do_retrieve_memories` must keep its exact signature: `#[tool] retrieve_memories` calls
    /// it, and the rmcp derives are sensitive to the shape of tool-adjacent methods. The knob is
    /// deliberately a Rust parameter rather than a field on [`RetrieveMemoriesParams`] — that
    /// struct is the public MCP tool schema advertised to every LLM client, and a `dry_run`
    /// field there would be a debug-only affordance that agents could set.
    /// Retrieval under a resolved population.
    ///
    /// The candidate read is scoped *inside* the KNN statement rather than filtered after it. Filtering
    /// afterwards is not a filter: the engine returns the `k` nearest rows in the whole store, so a closer
    /// out-of-scope vector displaces an eligible one and the caller silently gets fewer results — and the
    /// rows it does get were selected by a ranking that included other accounts' content.
    pub(crate) async fn retrieve_scoped(
        &self,
        scope: &RequestScope,
        params: RetrieveMemoriesParams,
        options: RetrieveOptions,
    ) -> anyhow::Result<serde_json::Value> {
        self.retrieve_core(params, options, Some(scope)).await
    }

    /// The debug Query Tester's three retrieval arms, each over an admitted population.
    ///
    /// Three named methods rather than a bool parameter because the three differ in *writes*, and a
    /// caller that transposed two flags in a struct literal would silently turn a dry diagnostic into
    /// a heat write. The tool arm is the one that matches `retrieve_memories` exactly, which is what
    /// makes the tester's "what the tool would return" claim worth anything.
    pub(crate) fn debug_scoped(dry: bool, unfiltered: bool) -> RetrieveOptions {
        RetrieveOptions {
            activate: !dry && !unfiltered,
            record_access: !dry && !unfiltered,
            apply_floor: !unfiltered,
        }
    }

    /// The shared retrieval body.
    ///
    /// `scope` is `Some` for every request-facing path and `None` only for the debug UI's interim
    /// diagnostics, which D1 leaves unscoped until Task 9 and which write no heat on their read-only arms.
    /// The parameter is an Option because the two paths genuinely differ in *population*, not in ranking:
    /// everything after the candidate read — ranking, floor, activation, response shape — is one code path,
    /// which is what keeps a dry run equal to the tool run it mirrors.
    async fn retrieve_core(
        &self,
        params: RetrieveMemoriesParams,
        options: RetrieveOptions,
        scope: Option<&RequestScope>,
    ) -> anyhow::Result<serde_json::Value> {
        // `limit` is client input and ends up interpolated into the KNN operator, which parses
        // `k` as a u32: clamp it here rather than fail the whole retrieve on a large value. Storage
        // clamps again where the literal is built (`MAX_KNN_K`), because `nearest` is not the only way
        // to reach that statement and the ceiling is a property of the index, not of this tool.
        let limit = params.limit.unwrap_or(10).clamp(1, MAX_RETRIEVE_LIMIT);

        // Which candidate read this request is about to run, chosen from the server's own knowledge of
        // whether `ensure_vector_index` succeeded. Nothing in `params` can move it — the tool's input
        // schema carries no such field, which `no_client_input_can_ask_for_the_indexed_form` pins — and
        // the same value both selects the statement inside `nearest` and labels the response, so the
        // label cannot be a claim about a scan that did not happen. Getting this wrong in the optimistic
        // direction is not a slow query but an empty answer: on 3.3 `<|k,ef|>` with no index behind it
        // matches no rows at all.
        let strategy = RetrievalStrategy::for_index(self.vector_index);

        // 1. Embed query
        let query_vecs = self.embedding.embed(&[&params.query]).await?;
        let query_emb = &query_vecs[0];

        // 2. Load candidates: the session's facts if scoped, otherwise the nearest live facts,
        // from the HNSW index when boot defined it. The index is approximate, so ask for more
        // than `limit`; step 3 re-ranks by exact cosine and truncates, which makes the slack
        // free and keeps a near-miss from displacing a row that was never fetched.
        let (facts, strategy) = match (scope, params.session_id.as_ref()) {
            // A session read under a scope resolves the handle *within that account* first, so a
            // foreign handle yields no memories rather than the other account's.
            //
            // It also consults no vector index at all: the session's own live rows are the candidate
            // set and step 3 scores them exactly, so `exact` is the honest label even on a store that
            // has an index. Reporting `indexed` here would describe a scan that never ran, which is
            // the failure this diagnostic exists to make visible.
            // A session narrows the candidate set; it does not widen the scope. The rows are read
            // through the caller's *resolved partitions*, so an `auto` read with no project over a
            // session that also holds project rows answers with the personal ones — the intersection
            // the design states and the pre-Task 8 shape (workspace-wide `get_memories`) did not.
            (Some(scope), Some(session_id)) => {
                let session_repo = ScopedSessionRepo::new(self.db.inner(), scope.owner().clone());
                (
                    session_repo
                        .memories_in(session_id, scope.read())
                        .await
                        .map_err(anyhow::Error::from)?,
                    RetrievalStrategy::Exact,
                )
            }
            (None, Some(session_id)) => (
                SessionRepo::new(self.db.inner())
                    .get_memories(session_id)
                    .await?,
                RetrievalStrategy::Exact,
            ),
            (Some(scope), None) => {
                let repo = ScopedMemoryRepo::new(self.db.inner(), scope.owner(), scope.read());
                (
                    repo.nearest(query_emb, limit + KNN_SLACK, strategy).await?,
                    strategy,
                )
            }
            (None, None) => {
                // The interim unscoped arm, kept compiling for the debug UI until Task 9 replaces its
                // population. The label describes it the same way: label and operator come from one
                // `vector_index` flag, and neither from a caller.
                let repo = MemoryRepo::new(self.db.inner());
                let k = limit + KNN_SLACK;
                let facts = match strategy {
                    RetrievalStrategy::Indexed => repo.nearest_indexed(query_emb, k).await?,
                    RetrievalStrategy::Exact => repo.nearest(query_emb, k).await?,
                };
                (facts, strategy)
            }
        };

        if facts.is_empty() {
            return Ok(serde_json::json!({
                "results": [],
                "retrieval_strategy": strategy.as_str(),
                "due_reminders": self.due_reminders_summary_scoped(scope, DUE_REMINDERS_CAP as usize).await,
            }));
        }

        // 3. Rank by similarity, then — for the tool and the dry run — drop results below the
        // server-side floor.
        // This is a conservative defense-in-depth cutoff: it removes pure noise
        // even if a client sets a lax threshold, without changing semantics for
        // deliberate agent lookups (the floor sits well below plausible matches).
        // It is skipped only by the unfiltered debug path, which needs the suppressed rows back.
        let embeddings: Vec<Vec<f32>> = facts.iter().map(|f| f.embedding.clone()).collect();
        let mut ranked: Vec<(usize, f32)> = rank_by_similarity(query_emb, &embeddings, limit);
        if options.apply_floor {
            // `retain` keeps the rank order, so the activation loop below sees exactly the rows the
            // old `into_iter().filter()` chain produced.
            ranked.retain(|(_, sim)| *sim >= self.retrieve_min_similarity);
        }

        // 4. Record the access for every row actually returned.
        //
        // Boundary: the rows the caller sees, not the `activation_top_n` seeds and not the wider
        // candidate window. That is what makes `access_count = 0` mean "no retrieval has ever
        // surfaced this row" — the strongest available evidence of dead weight, and the field
        // `Appraise` demotes on. It mirrors the reasoning step 5 documents for activation: touch
        // what a caller actually saw, never a row they did not.
        //
        // This runs BEFORE activation and the order is load-bearing: an access resets heat to 1.0
        // and activation adds to it, so recording after warming erases the warm on every row that is
        // both surfaced and an activation seed. `test_wet_run_does_write_heat` and
        // `test_query_run_retrieve_writes_heat_without_dry_run` caught exactly that on the first
        // attempt at this code. Reset, then propagate.
        //
        // Two round trips regardless of `limit` (one bulk read, one batched write) rather than two
        // per result, because this path runs on every agent prompt. Failures are logged, never
        // propagated: housekeeping must not turn a working retrieve into a tool error, but it is not
        // swallowed the way `add_heat`'s `.ok()` swallows either — a persistent breakage here
        // silently stops accesses being counted, which is the failure mode this branch exists to fix.
        if options.record_access {
            let ids: Vec<String> = ranked
                .iter()
                .filter_map(|(idx, _)| facts[*idx].id.as_ref())
                .map(record_id_to_string)
                .collect();
            if let Err(e) = self.record_accesses(&ids).await {
                tracing::warn!(
                    "retrieve: could not record accesses for {} results: {e}",
                    ids.len()
                );
            }
        }

        // 5. Trigger spreading activation for top results.
        //
        // Order matters and is load-bearing (AGENTS.md documents it): this runs after ranking
        // and after the `retrieve_min_similarity` filter dropped the noise, so it warms the
        // results a caller actually sees — never a row they did not. It also runs after the access
        // reset, so a seeded row's warm lands on top of 1.0 instead of being erased by it.
        //
        // Every warm it issues is a real database write — one guarded transaction for the whole pass on
        // the scoped arm, one per neighbour on the interim arm — which is why the debug UI's dry-run mode
        // passes `activate: false` rather than calling this unconditionally.
        if options.activate {
            // Fire-and-forget activation — don't block on it, and never let it turn a working retrieve
            // into a tool error. Activation is a *write*, so the interim unscoped arm uses
            // `trigger_activation_interim`: a store-wide warm is the cross-partition leak in its most
            // damaging form. The debug dry run and unfiltered window were already non-mutating, and the
            // wet debug arm keeps its disclosed heat write in `record_accesses`, not here.
            //
            // INTERIM (Task 6; see also the store-wide due sample below): the debug arm activates through
            // the unscoped path, because no lawful scope exists there until Task 9.
            // `request_facing_modules_never_reach_an_interim_or_unscoped_surface` keeps the interim and
            // unscoped surfaces out of every `#[tool]` handler; the debug arms below are outside that
            // guarantee by construction, since only `scope: None` reaches them.
            let focals: Vec<String> = ranked
                .iter()
                .take(self.activation_top_n)
                .filter_map(|(idx, _)| facts[*idx].id.as_ref())
                .map(record_id_to_string)
                .collect();
            match scope {
                // One pass, one authority claim, for the whole set of focals.
                Some(scope) => {
                    let _ = self.trigger_activation(scope, &focals, 1.0).await;
                }
                None => {
                    for fact_id in &focals {
                        let _ = self.trigger_activation_interim(fact_id, 1.0).await;
                    }
                }
            }
        }

        // 6. Build results
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
            "retrieval_strategy": strategy.as_str(),
            "due_reminders": self.due_reminders_summary_scoped(scope, DUE_REMINDERS_CAP as usize).await,
        }))
    }

    /// Materialise "a caller saw these rows" for the retrieve path: read the heat state of every
    /// returned row in one query, advance it through the engine, and write all of it back in one
    /// batched query.
    ///
    /// A returned row with no `heat_state` yet (an imported chunk, or anything written through
    /// `MemoryRepo::create_fact`) gets one created at the access values rather than being skipped:
    /// skipping would leave `access_count` at 0 forever for precisely those rows, and `Appraise`
    /// reads 0 as "never surfaced".
    async fn record_accesses(&self, memory_ids: &[String]) -> anyhow::Result<()> {
        if memory_ids.is_empty() {
            return Ok(());
        }
        let heat_repo = HeatRepo::new(self.db.inner());
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let rows = heat_repo.get_many(memory_ids).await?;
        let by_memory: HashMap<String, alexandria_storage::models::HeatState> = rows
            .into_iter()
            .map(|row| (record_id_to_string(&row.memory), row))
            .collect();

        // One round trip for the rows that have no heat_state, then one for the accesses. Doing this
        // per row was correct and read as "two round trips" in the docs, which was true only for a
        // corpus where every returned row already has a heat row — and memories written through
        // `create_fact` directly, or predating `do_store_memory`'s heat write, have none. So the
        // count of statements here is a property of the corpus rather than of the code.
        let rowless: Vec<&String> = memory_ids
            .iter()
            .filter(|id| !by_memory.contains_key(id.as_str()))
            .collect();
        heat_repo
            .create_for_memory_many(
                rowless
                    .iter()
                    .map(|id| id.as_str())
                    .collect::<Vec<&str>>()
                    .as_slice(),
            )
            .await?;

        let mut updates = Vec::with_capacity(memory_ids.len());
        for id in memory_ids {
            let mut state = match by_memory.get(id) {
                Some(row) => heat_engine_state(row),
                None => {
                    // Created above, in the batch. Its anchor is this instant: the first sight of a
                    // brand-new row earns no spacing credit, which is the correct reading rather than
                    // the accident of `HeatState::new` starting at the epoch.
                    EngineHeatState {
                        heat: 1.0,
                        stability: 1.0,
                        last_touched: now,
                        last_accessed_at: now,
                        access_count: 0,
                    }
                }
            };
            on_access(&mut state, now, self.heat.spacing_reference_secs);
            updates.push(HeatUpdate {
                memory_id: id.clone(),
                heat: state.heat,
                stability: state.stability,
                access_count: state.access_count as i64,
            });
        }

        let written = heat_repo.record_access_many(&updates).await?;
        if written != updates.len() {
            tracing::warn!(
                "retrieve: recorded access for {written} of {} rows; the rest lost their \
                 heat_state between the read and the write",
                updates.len()
            );
        }
        Ok(())
    }

    pub async fn do_recall(&self, params: RecallParams) -> anyhow::Result<String> {
        // Embed query
        let query_vecs = self.embedding.embed(&[&params.query]).await?;
        let query_emb = &query_vecs[0];

        if let Some(ref handle_str) = params.scope_handle {
            // Focused recall
            let scope =
                RecallHandle::decode(handle_str).map_err(|_| anyhow::anyhow!(MALFORMED_HANDLE))?;
            // The handle's cluster id is the display form this server minted from its own typed
            // listing (the engine's `RecallHandle` is the wire type and carries text). Converting
            // HERE — at the holder that produced the text — keeps the repo boundary typed; the
            // handle re-typing is named follow-up alongside it.
            let cluster = RecordId::parse_simple(scope.cluster_id.as_str())
                .map_err(|_| anyhow::anyhow!(MALFORMED_HANDLE))?;
            let cluster_data = self.load_cluster_with_members(&cluster).await?;
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
                "due_reminders": self.due_reminders_summary(DUE_REMINDERS_CAP as usize).await,
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
                "due_reminders": self.due_reminders_summary(DUE_REMINDERS_CAP as usize).await,
            })
            .to_string())
        }
    }

    pub async fn do_list_sessions(&self, params: ListSessionsParams) -> anyhow::Result<String> {
        let sessions = SessionRepo::new(self.db.inner())
            .list_filtered(
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

    /// The interim **unscoped** reminder create. Test-only callers and any non-request path; the
    /// `set_reminder` tool goes through [`Self::reminder_create`].
    pub async fn do_set_reminder(&self, params: SetReminderParams) -> anyhow::Result<String> {
        self.set_reminder_body(None, params).await
    }

    /// Create a reminder row from validated tool params. All schedule parsing and
    /// validation happens here (set-time, not delivery-time) so a bad cron or an
    /// impossible local time is rejected while the user is still in the loop.
    ///
    /// `scope` decides only *who the row belongs to* — nothing about validation depends on it, which is
    /// why the two arms cannot disagree about whether a schedule is lawful.
    async fn set_reminder_body(
        &self,
        scope: Option<&RequestScope>,
        params: SetReminderParams,
    ) -> anyhow::Result<String> {
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

        // The one field whose entire purpose is to be read. A blank message stores
        // fine, reports `status: ok` with a `next_fire_preview`, and then delivers
        // an empty bullet — the client has to invent a placeholder for it, which is
        // the wrong place to discover that nothing was ever written. The blank
        // `target_project` check below already refuses on the same logic.
        if params.message.trim().is_empty() {
            anyhow::bail!(
                "message must not be empty — it is the text delivered to the user when the reminder fires"
            );
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
                "target_project must be a non-empty owned project ID, or omit it for a global reminder"
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
        let mut warning = match (&spec, next) {
            (sched::ScheduleSpec::Once { due_at }, None) => Some(format!(
                "due_at {} is in the past; this reminder will fire on the very next check",
                rfc3339_utc(*due_at)
            )),
            _ => None,
        };
        // The other thing set-time validation cannot fix but can name: this crate
        // intersects day-of-month with day-of-week where Vixie cron unions them, so
        // an expression written from habit fires on a far narrower set of days than
        // the caller meant. It validates and it fires, so the only honest place to
        // say it is here — the caller is still in the conversation to reword it.
        if let sched::ScheduleSpec::Cron { expr } = &spec
            && sched::cron_dom_dow_conflict(expr)
        {
            warning = Some(match warning {
                Some(existing) => format!(
                    "{existing} Also: this expression restricts both day-of-month and day-of-week, which are ANDed (so '0 9 13 * FRI' is Friday the 13th only, not the 13th plus every Friday) — use two reminders for a union."
                ),
                None => "this expression restricts both day-of-month and day-of-week, which are ANDed (so '0 9 13 * FRI' is Friday the 13th only, not the 13th plus every Friday) — use two reminders for a union.".to_string(),
            });
        }
        // A past one-shot still stores next_due_at = due_at so list_due catches it.
        let next_due_at = match &spec {
            sched::ScheduleSpec::Once { due_at } => Some(*due_at),
            _ => next,
        };

        let reminder = NewReminder {
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
        };
        let id = match scope {
            Some(scope) => {
                let created = ScopedReminderRepo::new(self.db.inner(), scope.owner().clone())
                    .create(&reminder, now)
                    .await
                    .map_err(anyhow::Error::from)?;
                created
                    .id
                    .as_ref()
                    .map(record_id_to_string)
                    .ok_or_else(|| anyhow::anyhow!("created reminder has no id"))?
            }
            None => ReminderRepo::new(self.db.inner()).create(&reminder).await?,
        };

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
            // The walk stops at the engine's `MAX_ITER` bound (10 000) and says so
            // via `saturated`, because a capped count published as an exact one is
            // wrong in a way no reader can see — see `OccurrenceCount`.
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
                sched::OccurrenceCount {
                    count: 0,
                    saturated: false,
                }
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
                // `saturated` travels with the count: at the cap the true number
                // is larger, and "missed 10000 fires" read as fact is worse than
                // "at least 10000".
                "missed_occurrences": missed.count,
                "missed_occurrences_saturated": missed.saturated,
                "due_at": rfc3339_utc(due_at),
                "schedule": sched::human_readable(&spec),
                "note": r.note,
                "provenance": {
                    "project": r.prov_project,
                    "session_id": r.prov_session_id,
                },
                "next_due_at": new_next.map(rfc3339_utc),
                // The schedule is wall-clock in `[reminders].timezone` while
                // `due_at`/`next_due_at` are UTC instants. Without the zone name
                // and a local rendering, a client cannot tell the user *when* a
                // fire was — `set_reminder` and `list_reminders` already give both.
                "timezone": tz.name().to_string(),
                "due_at_local": due_at
                    .with_timezone(&tz)
                    .format("%Y-%m-%d %H:%M %Z")
                    .to_string(),
                "next_due_at_local": new_next
                    .map(|n| n.with_timezone(&tz).format("%Y-%m-%d %H:%M %Z").to_string()),
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
        let limit = params
            .limit
            .unwrap_or(DEFAULT_LIST_REMINDERS_LIMIT)
            .clamp(1, MAX_LIST_REMINDERS_LIMIT);
        let offset = params.offset.unwrap_or(0).max(0);
        let (rows, truncated) = repo
            .list(status, params.target_project.as_deref(), limit, offset)
            .await?;

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
            // Paginated because this history only grows: delivered and cancelled
            // rows are kept on purpose, so an unbounded "show me everything" slowly
            // turns into the whole reminder history in an agent context.
            "limit": limit,
            "offset": offset,
            "truncated": truncated,
            "next_offset": if truncated { Some(offset + limit) } else { None },
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
    /// Dispatches the piggyback: the caller's own rows when a request scope exists, the store-wide sample
    /// when it does not.
    ///
    /// The `None` arm is the **disclosed interim exposure**, not an empty array: it calls
    /// [`Self::due_reminders_summary`], which reads every account's due rows, so a debug dry run answers
    /// with other accounts' reminder text. That is the same interim population D1 defers to Task 9 (which
    /// replaces the debug surface with an authenticated one), and the reason it is tolerable for now is
    /// that no `#[tool]` handler reaches this arm — the seam scan in `scoped_request_tools_test` fails if
    /// one does. The wording here used to claim the arm was empty, which was the opposite of the code; if
    /// this sentence and the `None` arm ever disagree again, fix the arm and this comment in the same
    /// change.
    async fn due_reminders_summary_scoped(
        &self,
        scope: Option<&RequestScope>,
        cap: usize,
    ) -> Vec<serde_json::Value> {
        match scope {
            Some(scope) => self.due_reminders_summary_for(scope, cap).await,
            // The interim arm keeps the store-wide sample it has always produced. That is a disclosed
            // limitation of the unscoped path, not the request behaviour: no `#[tool]` handler reaches
            // this arm, and Task 9 replaces the debug surface that does.
            None => self.due_reminders_summary(cap).await,
        }
    }

    /// The caller's own due sample for the read-response piggyback.
    ///
    /// The unscoped version below puts every account's due reminder text into whoever asked. That is a
    /// leak with no memory body in it, on the highest-frequency path in the server, so the request-facing
    /// arms use this one and the interim debug arm keeps the store-wide read it was written for.
    async fn due_reminders_summary_for(
        &self,
        scope: &RequestScope,
        cap: usize,
    ) -> Vec<serde_json::Value> {
        use alexandria_storage::repos::ScopedReminderRepo;

        let repo = ScopedReminderRepo::new(self.db.inner(), scope.owner().clone());
        match repo.due_sample(Utc::now(), cap as i64 + DUE_SAMPLE_SLACK).await {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|r| {
                    let spec = match alexandria_engine::reminders::spec_from_reminder(&r) {
                        Ok(spec) => spec,
                        Err(error) => {
                            tracing::debug!(
                                "due_reminders: skipping unreadable row: {}",
                                error_message(&error)
                            );
                            return None;
                        }
                    };
                    Some(serde_json::json!({
                        "id": r.id.as_ref().map(record_id_to_string),
                        "message": r.message,
                        "due_at": r.next_due_at.map(rfc3339_utc),
                        "due_at_local": r.next_due_at.map(|at| at.with_timezone(&self.reminders.tz).format("%Y-%m-%d %H:%M").to_string()),
                        "target": r.project.as_ref().map(format_project_handle)
                            .unwrap_or_else(|| "global".into()),
                        "recurring": !matches!(
                            spec,
                            alexandria_engine::reminders::ScheduleSpec::Once { .. }
                        ),
                    }))
                })
                .take(cap)
                .collect(),
            Err(error) => {
                tracing::warn!(
                    "due_reminders piggyback failed: {}",
                    error_message(&anyhow::Error::from(error))
                );
                Vec::new()
            }
        }
    }

    /// The interim store-wide due sample, for the debug UI's arms only. See
    /// [`Self::due_reminders_summary_for`].
    async fn due_reminders_summary(&self, cap: usize) -> Vec<serde_json::Value> {
        use alexandria_storage::repos::ReminderRepo;

        let repo = ReminderRepo::new(self.db.inner());
        // The bound is pushed into the query, so an unrelated memory lookup never
        // deserializes the whole overdue set to show a slice of it.
        match repo
            .list_due_sample(Utc::now(), cap as i64 + DUE_SAMPLE_SLACK)
            .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|r| {
                    // A row whose schedule cannot be reconstructed is undeliverable
                    // and would render as `due_at: null` noise; an operator still
                    // sees it in `list_reminders` (which reports it as invalid) and
                    // can `cancel_reminder` it. Until then it costs one of these
                    // slots, which is the cheaper failure than reading every
                    // pending row on every prompt to find out.
                    let spec = match alexandria_engine::reminders::spec_from_reminder(&r) {
                        Ok(spec) => spec,
                        Err(e) => {
                            tracing::debug!(
                                "due_reminders: skipping unreadable row: {}",
                                error_message(&e)
                            );
                            return None;
                        }
                    };
                    let _ = spec;
                    Some(serde_json::json!({
                        "id": r.id.as_ref().map(record_id_to_string).unwrap_or_default(),
                        "message": r.message,
                        "target": match &r.target_project {
                            Some(p) => format!("project:{p}"),
                            None => "global".to_string(),
                        },
                        "due_at": r.next_due_at.map(rfc3339_utc),
                    }))
                })
                // After the filter, not before: capping the raw rows first would put
                // the corrupt-row problem right back, since the row dropped by the
                // filter was already counted against `cap`.
                .take(cap)
                .collect(),
            Err(e) => {
                tracing::warn!("due_reminders piggyback failed: {}", error_message(&e));
                Vec::new() // fail-open: never break retrieval over reminders
            }
        }
    }

    /// Assign a fact to a cluster, creating a new one if needed. Updates centroids.
    ///
    /// `destination` is the partition the fact was just written into, and it is required alongside
    /// `scope`: an arm that has an owner but no destination could only rank against the workspace, which is
    /// the population a destination must never be chosen from. Both `None` is the interim arm; a `scope`
    /// without a `destination` is the same "arm carried a scope without its write target" refusal the store
    /// and import bodies already state, rather than a silent fall back to a wider list.
    async fn assign_to_cluster_and_update(
        &self,
        scope: Option<&RequestScope>,
        destination: Option<&MemoryScope>,
        embedding: &[f32],
        fact_id: &RecordId,
    ) -> anyhow::Result<()> {
        let cluster_repo = ClusterRepo::new(self.db.inner());
        // A new fact may only join a cluster its owner can see, and only one its *partition* owns; joining
        // on a store-wide list would let a near centroid attach another account's row to this one's cluster
        // and rewrite that centroid, and joining on the owner's workspace-wide list would let it attach a
        // row across a scope line. `None` is the interim arm's view and no `#[tool]` handler passes it.
        let candidates = match (scope, destination) {
            (Some(scope), Some(destination)) => {
                self.destination_cluster_candidates(scope, destination)
                    .await?
            }
            (None, None) => {
                DestinationCandidates::from_clusters(cluster_repo.list_with_counts().await?)?
            }
            _ => anyhow::bail!(
                "a cluster assignment carried a scope without its destination partition"
            ),
        };
        let assignment = candidates.choose(embedding, self.cluster_join_threshold)?;

        match assignment {
            Some(cluster) => {
                let cid = cluster;
                // `cid` came out of the caller's own destination candidates above, so this edge and this
                // centroid refresh aim at a cluster the caller can see *and that lives in the partition the
                // fact was just filed into*; they stay `ClusterRepo` calls because the membership and
                // centroid *restructuring* surface is Task 12's (D3).
                let old = candidates
                    .identities
                    .iter()
                    .position(|id| id == cluster)
                    .and_then(|index| candidates.infos.get(index))
                    .ok_or_else(|| anyhow::anyhow!("ranked cluster has no captured population"))?;
                let new_centroid = update_centroid(&old.centroid, embedding, old.member_count);
                cluster_repo.add_member(cid, fact_id).await?;
                cluster_repo.update_centroid(cid, &new_centroid).await?;
            }
            None => {
                // A new cluster is an ownership-bearing row, so it is stamped where it is created. The
                // unscoped `ClusterRepo::create` writes `label`/`centroid`/`depth` and nothing else, and
                // `ScopedClusterRepo::list` filters on `workspace` — so every cluster that call made was
                // invisible to the account that paid for it, `recall` was permanently empty for every
                // owner, and each imported chunk spawned another unreachable cluster.
                let cid = match scope {
                    Some(scope) => {
                        let cluster =
                            ScopedClusterRepo::new(self.db.inner(), scope.owner(), scope.read())
                                .create(None, embedding, Utc::now())
                                .await
                                .map_err(anyhow::Error::from)?;
                        cluster
                            .id
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("created cluster has no id"))?
                            .clone()
                    }
                    None => cluster_repo.create(None, embedding).await?,
                };
                cluster_repo.add_member(&cid, fact_id).await?;
            }
        }
        Ok(())
    }

    /// The interim unscoped activation, reachable only from the debug arm. See
    /// [`Self::trigger_activation`] for the scoped behaviour that replaced it.
    async fn trigger_activation_interim(&self, fact_id: &str, bump: f32) -> anyhow::Result<()> {
        let neighbors = EdgeRepo::new(self.db.inner())
            .get_neighbors(fact_id, self.activation_config.max_hops)
            .await?;
        if neighbors.is_empty() {
            return Ok(());
        }
        let neighbor_data: Vec<(String, u32, f64)> = neighbors
            .iter()
            .map(|n| (record_id_to_string(&n.id), n.hop, n.strength))
            .collect();
        let targets = compute_activation_targets(&neighbor_data, bump, &self.activation_config);
        let heat_repo = HeatRepo::new(self.db.inner());
        for target in &targets {
            heat_repo
                .add_heat(&target.id, target.heat_delta as f64)
                .await
                .ok();
        }
        Ok(())
    }

    /// Runs **one** spreading-activation pass for a retrieval: the warm set of every focal record,
    /// claimed and committed once.
    ///
    /// Three properties the unscoped path could not express, and each was a live defect:
    ///
    /// * **The partition came from the request, not the row.** Activation used the retrieval's union, so
    ///   a Personal focal record could warm a project neighbour and the reverse — cross-partition
    ///   influence expressed as a heat number rather than as a leaked row, invisible to every read test.
    ///   Each focal record's own `(workspace, scope)` is read back from its row (`focal_partition_of`),
    ///   travels with its own warm targets, and a row with no ownership columns seeds nothing.
    /// * **Neighbours were filtered after traversal.** `get_neighbors` walked every edge from the focal
    ///   row, so an edge to another account's, a deleted, a quarantined or an Unassigned fact yielded a
    ///   candidate the writer then had to remember to skip. `ScopedGraphReader` filters every candidate
    ///   target by workspace, partition and the live predicate **in SQL**.
    /// * **A warm that wrote nothing looked like a warm that happened.** `HeatRepo::add_heat` is an
    ///   `UPDATE heat_state … WHERE memory = …` that matches zero rows when there is no such row, and its
    ///   caller wrapped it in `.ok()`. `ScopedHeatWriter::warm` reports a `false` per target, so a refusal
    ///   is a reported answer and never a silent success.
    ///
    /// And the fourth, which is why this is one call rather than one per target: each authority claim is a
    /// conditional write on the `account` row (and, hosted, on the `credential` row). Warmed per target,
    /// one retrieval over `activation_top_n` focals with several neighbours each opened a dozen
    /// transactions contending on the same account row, so two concurrent retrieves by one account were
    /// guaranteed conflict opponents and the dropped warms surfaced only in a `debug!` line. Targets are
    /// collected first and written in one transaction; `one_activation_pass_claims_authority_once`
    /// measures it. The reference is named exactly as the test is named: this sentence used to send a
    /// reader looking for a gate that does not exist.
    ///
    /// Hop distances come from the same engine function as before; only the population feeding it is
    /// bounded. Eligible **unreturned** same-partition neighbours still warm, which is the design's
    /// intent — the focal record's neighbourhood, not the caller's result page.
    async fn trigger_activation(
        &self,
        scope: &RequestScope,
        focal_ids: &[String],
        bump: f32,
    ) -> anyhow::Result<()> {
        let mut targets: Vec<alexandria_storage::repos::WarmTarget> = Vec::new();
        for fact_id in focal_ids {
            targets.extend(self.activation_targets(scope, fact_id, bump).await?);
        }
        if targets.is_empty() {
            return Ok(());
        }

        let writer = ScopedHeatWriter::new(self.db.inner(), scope.owner());
        match writer.warm(&targets, Utc::now()).await {
            Ok(pass) => {
                for (target, wrote) in targets.iter().zip(pass.wrote.iter()) {
                    if !*wrote {
                        // `false` is a refusal by the partition/live predicate inside the statement, not
                        // an error; it is logged rather than retried, and never reported as a warm.
                        tracing::debug!(
                            "activation: warm refused for {}/out-of-partition",
                            record_id_to_string(&target.target)
                        );
                    }
                }
                tracing::debug!(
                    "activation: {} warmed, {} refused across {} focal record(s)",
                    pass.warmed(),
                    pass.refused(),
                    focal_ids.len()
                );
            }
            // One pass failing is now all-or-nothing, so this is a dropped activation rather than a
            // dropped neighbour — worth more than a `debug!` line nobody reads.
            Err(error) => {
                tracing::warn!(
                    "activation: warm pass dropped ({} target(s)): {error}",
                    targets.len()
                );
            }
        }
        Ok(())
    }

    /// One focal record's warm set: its own partition, its own bounded neighbourhood, its own deltas.
    ///
    /// Reads only. The writes belong to [`Self::trigger_activation`], so a retrieval with several focals
    /// claims authority once instead of once per neighbour.
    async fn activation_targets(
        &self,
        scope: &RequestScope,
        fact_id: &str,
        bump: f32,
    ) -> anyhow::Result<Vec<alexandria_storage::repos::WarmTarget>> {
        let Some(focal) = surrealdb::types::RecordId::parse_simple(fact_id).ok() else {
            return Ok(Vec::new());
        };
        // The focal record's own partition, read from its row rather than passed in by the caller.
        let Some(partition) =
            alexandria_storage::repos::focal_partition_of(self.db.inner(), &focal)
                .await
                .map_err(anyhow::Error::from)?
        else {
            return Ok(Vec::new());
        };

        let reader = ScopedGraphReader::new(self.db.inner(), scope.read());
        let mut hop_of: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        let mut strengths: std::collections::HashMap<String, f64> =
            std::collections::HashMap::new();
        // Breadth over the partition-bounded direct-neighbour set, to `max_hops`. Each hop is re-read
        // through the reader, so a neighbour that is only reachable by leaving the partition is never
        // discovered in the first place. The frontier advances per hop, exactly as the accepted traversal
        // did: dropping `frontier = next` would silently cap the warm set at one hop.
        let mut frontier = vec![focal.clone()];
        let mut hop = 0u32;
        let mut visited: std::collections::HashSet<String> =
            std::collections::HashSet::from([record_id_to_string(&focal)]);
        while hop < self.activation_config.max_hops && !frontier.is_empty() {
            hop += 1;
            let mut next = Vec::new();
            for node in &frontier {
                for neighbor in reader
                    .direct_neighbors_in_partition(node, &partition)
                    .await
                    .map_err(anyhow::Error::from)?
                {
                    let key = record_id_to_string(&neighbor.id);
                    if visited.insert(key.clone()) {
                        hop_of.insert(key.clone(), hop);
                        strengths.insert(key, neighbor.strength);
                        next.push(neighbor.id);
                    }
                }
            }
            frontier = next;
        }

        if hop_of.is_empty() {
            return Ok(Vec::new());
        }

        let neighbor_data: Vec<(String, u32, f64)> = hop_of
            .iter()
            .map(|(id, at_hop)| (id.clone(), *at_hop, *strengths.get(id).unwrap_or(&1.0)))
            .collect();
        Ok(
            compute_activation_targets(&neighbor_data, bump, &self.activation_config)
                .into_iter()
                .filter_map(|target| {
                    let handle = surrealdb::types::RecordId::parse_simple(&target.id).ok()?;
                    Some(alexandria_storage::repos::WarmTarget {
                        target: handle,
                        partition: partition.clone(),
                        delta: target.heat_delta as f64,
                    })
                })
                .collect(),
        )
    }

    async fn load_cluster_with_members(
        &self,
        cluster: &RecordId,
    ) -> anyhow::Result<ClusterWithMembers> {
        let cluster_repo = ClusterRepo::new(self.db.inner());
        let members = cluster_repo.get_members(cluster).await?;

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
                id: record_id_to_string(cluster),
                centroid: vec![],
                member_count: fact_summaries.len(),
            },
            // The interim unscoped arm resolves no owned cluster row, so it has no stored guard to
            // stamp: `None` says "this handle claims a member count and nothing else", which is what
            // Task 9's authenticated debug fixture will replace once there is a lawful context to read
            // the row under.
            version: ClusterVersion {
                membership_revision: None,
                members: fact_summaries.len(),
            },
            members: fact_summaries,
        })
    }

    async fn load_all_clusters_with_members(&self) -> anyhow::Result<Vec<ClusterWithMembers>> {
        // The typed rows are the authority; their display forms are built once, at the response.
        let clusters = ClusterRepo::new(self.db.inner()).list().await?;
        let mut result = Vec::with_capacity(clusters.len());
        for cluster in clusters {
            let Some(id) = cluster.id.clone() else {
                continue;
            };
            let info = ClusterInfo {
                id: record_id_to_string(&id),
                centroid: cluster.centroid.clone(),
                member_count: 0,
            };
            let cwm = self.load_cluster_with_members(&id).await?;
            result.push(ClusterWithMembers {
                info: ClusterInfo {
                    id: cwm.info.id,
                    centroid: info.centroid.clone(),
                    member_count: cwm.members.len(),
                },
                version: ClusterVersion {
                    membership_revision: None,
                    members: cwm.members.len(),
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
    use alexandria_storage::repos::FactListQuery;
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
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );

        let info = server.get_info();

        let instructions = info
            .instructions
            .expect("server must advertise usage instructions to MCP clients");
        assert!(instructions.contains("proactively"));
        assert!(instructions.contains("store_memory"));
        assert!(instructions.contains("retrieve_memories"));
        assert!(info.capabilities.tools.is_some());
    }

    /// `new()` is the construction path every test harness and any non-`main.rs` embedder
    /// takes, so its fallbacks must be the engine's tuned constants rather than local
    /// literals that can drift from what production actually runs.
    #[tokio::test]
    async fn test_new_uses_engine_default_thresholds() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );

        assert_eq!(
            server.retrieve_min_similarity,
            alexandria_engine::search::DEFAULT_MIN_SIMILARITY
        );
        assert_eq!(
            server.cohesion_floor,
            alexandria_engine::clusters::maintenance::DEFAULT_COHESION_FLOOR
        );
    }

    /// The seventeen public MCP tools, by name — the exact set `AGENTS.md` and `README.md` document.
    ///
    /// Deliberately a closed list rather than a count. `do_retrieve_memories_dry` and
    /// `do_retrieve_memories_unfiltered` are `#[tool]`-less wrappers over `retrieve_core` used only
    /// by the debug Query Tester, and a *count* check would keep passing if someone renamed a real
    /// tool and registered one of those as the fourteenth: the advertised surface would change while
    /// the total stayed seventeen. Names are what an MCP client actually calls.
    const PUBLIC_TOOLS: [&str; 17] = [
        "store_memory",
        "retrieve_memories",
        "recall",
        "update_memory",
        "import_document",
        "delete_memory",
        "get_session",
        "list_sessions",
        "finalize_session",
        "set_reminder",
        "check_reminders",
        "list_reminders",
        "cancel_reminder",
        "resolve_project",
        "list_projects",
        "list_unassigned_memories",
        "propose_memory_scope",
    ];

    /// Guards the "exactly 17 MCP tools" invariant in `AGENTS.md`/`README.md`.
    ///
    /// Until now that claim was verified by a human counting `#[tool]` attributes, so a future
    /// `#[tool]` on a debug-only wrapper — or a rename — would silently widen or shift the
    /// advertised surface and CI would pass. This asks the server what it advertises and compares
    /// the sorted name set against [`PUBLIC_TOOLS`] exactly.
    ///
    /// The advertised list is `ToolRouter::list_all()` because that is literally what the generated
    /// `ServerHandler::list_tools` returns (`tools: Self::tool_router().list_all()`); the trait
    /// method itself takes a `RequestContext<RoleServer>`, which carries a live `Peer<RoleServer>`
    /// and so cannot be built in a unit test. `get_tool` is the other half of the handler surface
    /// and takes only a `&str`, so it is checked here too — a name the router lists but the handler
    /// cannot resolve would be advertised and then rejected at call time.
    #[tokio::test]
    async fn test_advertised_tool_set_is_exactly_the_seventeen_public_tools() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );

        let mut advertised: Vec<String> = AlexandriaServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        advertised.sort();
        let mut expected: Vec<String> = PUBLIC_TOOLS.iter().map(|s| (*s).to_string()).collect();
        expected.sort();
        assert_eq!(
            advertised, expected,
            "the advertised MCP tool set must be exactly the seventeen documented public tools"
        );
        // The count is asserted as well as the set, not instead of it: a duplicate name would leave the
        // sorted lists equal while hiding an eighteenth tool behind a renamed one.
        assert_eq!(
            advertised.len(),
            17,
            "asserting the exact set is the point; a duplicate name would hide an eighteenth tool"
        );

        for name in PUBLIC_TOOLS {
            assert!(
                server.get_tool(name).is_some(),
                "{name} must resolve through ServerHandler::get_tool, not just appear in the list"
            );
        }
        // The closed set really is closed: the debug-only wrappers are not reachable this way.
        for not_a_tool in [
            "retrieve_memories_dry",
            "retrieve_memories_unfiltered",
            "do_retrieve_memories_dry",
        ] {
            assert!(
                server.get_tool(not_a_tool).is_none(),
                "{not_a_tool} must not be advertised or resolvable as an MCP tool"
            );
        }
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
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(DirectionalEmbedding),
            0.75,
            HeatSettings::default(),
        )
        .with_retrieve_min_similarity(0.30);

        server
            .do_store_memory(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
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
                scope: None,
                project: None,
                context_project: None,
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
                scope: None,
                project: None,
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

    /// Same retrieval through the HNSW index, and `limit` is clamped to
    /// `1..=MAX_RETRIEVE_LIMIT` instead of reaching the KNN operator as sent.
    #[tokio::test]
    async fn retrieve_memories_serves_from_vector_index() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        alexandria_storage::schema::ensure_vector_index(db.inner(), 2)
            .await
            .unwrap();
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(DirectionalEmbedding),
            0.75,
            HeatSettings::default(),
        )
        .with_vector_index(true);
        for content in ["near one", "near two", "far away"] {
            server
                .do_store_memory(StoreMemoryParams {
                    scope: None,
                    project: None,
                    context_project: None,
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
                scope: None,
                project: None,
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
        assert_eq!(results["results"].as_array().unwrap().len(), 1);
        let results = retrieve(usize::MAX).await.unwrap();
        assert!(results["results"].as_array().unwrap().len() >= 2);
    }

    /// One store, one server, and the owners a case can hand a scope to.
    ///
    /// `index` decides two things at once, which is the whole point of the pair: the HNSW index is
    /// defined on the store *and* the server is told, the way `main.rs` tells it only after
    /// `ensure_vector_index` succeeds. A `with_vector_index(true)` server over an undefined index (or
    /// the reverse) is not a state production can be in, and building the two together here is what stops
    /// a strategy assertion from testing a contradiction.
    struct Owners {
        server: AlexandriaServer,
        system: alexandria_storage::access::SystemContext,
        alice: RequestScope,
        bob: RequestScope,
    }

    impl Owners {
        async fn built(index: bool) -> Self {
            let mut db = Database::connect_embedded().await.unwrap();
            alexandria_storage::schema::migrate(db.inner())
                .await
                .unwrap();
            if index {
                alexandria_storage::schema::ensure_vector_index(db.inner(), 2)
                    .await
                    .expect("the stub model is two-dimensional, so the index is too");
            }
            let system = db.claim_system_authority().unwrap();
            let server = AlexandriaServer::new(
                Arc::new(db),
                Arc::new(StubEmbedding),
                0.75,
                HeatSettings::default(),
            )
            .with_vector_index(index);
            let alice = trusted_local_scope(&server, &system, "owners-alice").await;
            let bob = trusted_local_scope(&server, &system, "owners-bob").await;
            Self {
                server,
                system,
                alice,
                bob,
            }
        }

        /// A third owner over the same store, who has stored nothing.
        async fn stranger(&self, login: &str) -> RequestScope {
            trusted_local_scope(&self.server, &self.system, login).await
        }
    }

    fn retrieve_params(
        query: &str,
        limit: Option<usize>,
        session_id: Option<&str>,
    ) -> RetrieveMemoriesParams {
        RetrieveMemoriesParams {
            scope: None,
            project: None,
            query: query.to_string(),
            limit,
            session_id: session_id.map(str::to_string),
        }
    }

    /// The retrieve response names the strategy that actually ran — and nothing else.
    ///
    /// Two claims, because the diagnostic is the part of the eligible-first contract a client can act on
    /// and the part that can lie. The first is honesty in both directions: a server whose index define
    /// succeeded reports `indexed`, one whose did not reports `exact`, and the empty-answer path carries
    /// the label too, because "nothing matched" from a server that silently fell off its index is what
    /// the field exists to make visible. The second is the privacy control: the value is one of two
    /// words, it is the same word for two owners of one store, and the response carries no statement
    /// text, no workspace handle, no account handle.
    #[tokio::test]
    async fn the_retrieve_response_names_the_strategy_that_actually_ran() {
        let fallback = Owners::built(false).await;
        for content in ["a memory the owners can find", "another"] {
            fallback
                .server
                .store_response(
                    &fallback.alice,
                    MemoryScope::Personal,
                    store_params(content),
                )
                .await;
        }

        let off = fallback
            .server
            .retrieve_scoped(
                &fallback.alice,
                retrieve_params("a memory the owners can find", Some(5), None),
                RETRIEVE_TOOL,
            )
            .await
            .expect("the fallback retrieve runs");
        assert_eq!(
            off["retrieval_strategy"].as_str(),
            Some("exact"),
            "a server with no index defined claimed one: {off}"
        );
        assert!(
            !off["results"]
                .as_array()
                .map(|r| r.is_empty())
                .unwrap_or(true),
            "the labelled read answered with nothing, so the label is describing an empty result: {off}"
        );

        let indexed = Owners::built(true).await;
        indexed
            .server
            .store_response(
                &indexed.alice,
                MemoryScope::Personal,
                store_params("an indexed store note"),
            )
            .await;
        let on = indexed
            .server
            .retrieve_scoped(
                &indexed.alice,
                retrieve_params("an indexed store note", Some(5), None),
                RETRIEVE_TOOL,
            )
            .await
            .expect("the indexed retrieve runs");
        assert_eq!(
            on["retrieval_strategy"].as_str(),
            Some("indexed"),
            "a server whose index define succeeded reported the fallback: {on}"
        );

        // The empty-answer path is labelled by the same rule, not dropped.
        let stranger = indexed.stranger("strategy-stranger").await;
        let empty = indexed
            .server
            .retrieve_scoped(
                &stranger,
                retrieve_params("nothing under this owner", Some(5), None),
                RETRIEVE_TOOL,
            )
            .await
            .expect("an empty retrieve runs");
        assert_eq!(
            empty["retrieval_strategy"].as_str(),
            Some("indexed"),
            "the empty-answer path dropped the label it shares with the full one: {empty}"
        );
        assert_eq!(
            empty["results"].as_array().map(|r| r.len()),
            Some(0),
            "the control owner was supposed to hold nothing: {empty}"
        );

        // Privacy control: neither answer carries a statement, an owner, or a partition.
        for arm in [&off, &on, &empty] {
            let rendered = arm.to_string();
            for forbidden in [
                "SELECT",
                "quarantined_at",
                "scope_projects",
                "workspace",
                "account",
                "<|",
                "KnnScan",
                "fact_embedding_hnsw",
            ] {
                assert!(
                    !rendered.contains(forbidden),
                    "a retrieve response names {forbidden:?}, which is a statement, a partition or an \
                     owner and not a memory: {rendered}"
                );
            }
        }
        let bob_view = fallback
            .server
            .retrieve_scoped(
                &fallback.bob,
                retrieve_params("a memory the owners can find", Some(5), None),
                RETRIEVE_TOOL,
            )
            .await
            .expect("the foreign retrieve runs");
        assert_eq!(
            bob_view["retrieval_strategy"].as_str(),
            off["retrieval_strategy"].as_str(),
            "the label differed between two owners of one store, which makes it a fact about somebody \
             else rather than about this read"
        );
    }

    /// A session-narrowed read says `exact`, because no vector index took part in it.
    ///
    /// The session arm's candidates are that session's own live rows and the ranking is exact cosine, so
    /// this is the one place the label is deliberately not a copy of `vector_index`: reporting `indexed`
    /// while serving a session traversal would be the diagnostic lying in the optimistic direction — the
    /// direction that costs a caller an answer rather than a slow query.
    #[tokio::test]
    async fn a_session_scoped_read_reports_the_read_it_ran_and_not_the_index_it_has() {
        let indexed = Owners::built(true).await;
        let session = "pi:strategy-session";
        let inside = indexed
            .server
            .store_response(
                &indexed.alice,
                MemoryScope::Personal,
                StoreMemoryParams {
                    scope: None,
                    project: None,
                    context_project: None,
                    session_id: Some(session.to_string()),
                    ..store_params("a memory inside the session")
                },
            )
            .await;
        assert_eq!(
            inside.structured_content.expect("stored")["status"],
            "ok",
            "the session store failed, so the narrowing below reads an empty session"
        );
        let outside = indexed
            .server
            .store_response(
                &indexed.alice,
                MemoryScope::Personal,
                store_params("a memory outside the session"),
            )
            .await;
        let outside_id = outside.structured_content.expect("stored")["id"]
            .as_str()
            .unwrap()
            .to_string();

        let narrowed = indexed
            .server
            .retrieve_scoped(
                &indexed.alice,
                retrieve_params("a memory inside the session", Some(5), Some(session)),
                RETRIEVE_TOOL,
            )
            .await
            .expect("the session-narrowed retrieve runs");
        assert_eq!(
            narrowed["retrieval_strategy"].as_str(),
            Some("exact"),
            "a read that consulted no index reported one: {narrowed}"
        );
        let ids: Vec<String> = narrowed["results"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !ids.contains(&outside_id),
            "the session-narrowed read returned a row that is not a member of the session: {ids:?}"
        );
        assert_eq!(
            ids.len(),
            1,
            "the session read did not answer exactly its own member set: {ids:?}"
        );

        // A session handle held by another account narrows to nothing, not to their rows.
        let stranger = indexed.stranger("session-stranger").await;
        let foreign = indexed
            .server
            .retrieve_scoped(
                &stranger,
                retrieve_params("a memory inside the session", Some(5), Some(session)),
                RETRIEVE_TOOL,
            )
            .await
            .expect("the foreign session read runs");
        assert_eq!(
            foreign["results"].as_array().map(|r| r.len()),
            Some(0),
            "another account narrowed into somebody else's session: {foreign}"
        );
    }

    /// The ceiling on `k` belongs to the index, and the tool's ceiling has to sit under it.
    ///
    /// Two bounds exist on purpose — the tool clamps what a caller asks for, storage clamps what reaches
    /// statement text — and what makes that pair safe rather than confusing is that the request path's
    /// largest ask can never reach the storage clamp. Pin the arithmetic, so the day either moves, the
    /// test says which one is wrong.
    #[test]
    fn the_k_ceiling_sits_above_what_the_request_path_can_ask() {
        // Read through locals rather than comparing the two constants inline: `assertions_on_constants`
        // rejects a constant-condition assert as dead weight, and what makes this guard worth having is
        // that its *message* names which ceiling moved. A `const { assert!(..) }` would fail the build
        // with no such sentence.
        let request_path_ask = MAX_RETRIEVE_LIMIT + KNN_SLACK;
        let storage_ceiling = alexandria_storage::repos::MAX_KNN_K;
        assert_eq!(
            request_path_ask, 110,
            "the request path's largest candidate ask moved; the storage ceiling has to be re-checked \
             against the new number"
        );
        assert!(
            storage_ceiling >= request_path_ask,
            "the tool can ask for {request_path_ask} candidates but storage caps the operator at \
             {storage_ceiling}: the clamp would silently truncate a lawful request",
        );
        assert_eq!(
            storage_ceiling, 150,
            "the storage ceiling is documented as the HNSW search breadth; if it moved, the reasoning in \
             `MAX_KNN_K`'s own documentation no longer holds"
        );
    }

    /// No client input can select the indexed candidate read, nor move the label it reports.
    ///
    /// The brief states the requirement twice — index-vs-fallback comes *solely* from a successful HNSW
    /// definition, and the tool schema must stay unchanged — and this is the single gate that holds both,
    /// because the second is how the first is enforced in practice. Three claims:
    ///
    /// * **Schema.** The advertised `inputSchema` for `retrieve_memories` names exactly `query`, `limit`,
    ///   `session_id`, and the two Task 8 scope parameters — and nothing else. A `strategy`/`indexed`/
    ///   `use_index` knob there would be a public contract change *and* a caller-supplied claim about the
    ///   server's index, which is why this list is exact rather than an absence check: `scope` and
    ///   `project` say *which partition to read*, never *how to read it.
    /// * **Dispatch.** One and the same payload, carrying invented strategy keys, is run against a server
    ///   whose index define failed and one whose succeeded. The labels differ; the request does not. That
    ///   is the discrimination control and the positive control at once — a label frozen to either word
    ///   fails one of the two arms, and a reader who doubts the extra keys are ignored should note that
    ///   the `indexed` arm is reached by *server* state alone while sending the same `"exact"` request.
    /// * **No new response surface.** `retrieval_strategy` is the only key this lane adds to a retrieve
    ///   response, and it carries one of two words.
    #[tokio::test]
    async fn no_client_input_can_ask_for_the_indexed_form() {
        let schema = serde_json::to_value(schemars::schema_for!(RetrieveMemoriesParams))
            .expect("the retrieve params produce a schema");
        let mut advertised: Vec<String> = schema["properties"]
            .as_object()
            .expect("an object schema")
            .keys()
            .cloned()
            .collect();
        advertised.sort();
        assert_eq!(
            advertised,
            vec![
                "limit".to_string(),
                "project".to_string(),
                "query".to_string(),
                "scope".to_string(),
                "session_id".to_string(),
            ],
            "the advertised retrieve parameter set moved — a strategy knob here would let a caller pick \
             the scan the server is supposed to choose: {schema}"
        );
        // The two scope parameters are the whole of what Task 8 added, and neither names an index.
        for forbidden in ["strategy", "indexed", "use_index", "index"] {
            assert!(
                !schema["properties"]
                    .as_object()
                    .expect("an object schema")
                    .contains_key(forbidden),
                "a strategy knob reached the public schema: {schema}"
            );
        }

        // One client payload, naming three keys that do not exist. Built twice from the same JSON
        // because the params struct is not `Clone` — the payload, not the struct, is what a client sends.
        let smuggled_json = || {
            serde_json::from_value::<RetrieveMemoriesParams>(serde_json::json!({
                "query": "an indexed store note",
                "limit": 5,
                "retrieval_strategy": "indexed",
                "indexed": true,
                "use_index": true,
            }))
            .expect("unknown fields are dropped, not rejected")
        };

        let fallback = Owners::built(false).await;
        fallback
            .server
            .store_response(
                &fallback.alice,
                MemoryScope::Personal,
                store_params("an indexed store note"),
            )
            .await;
        let off = fallback
            .server
            .retrieve_scoped(&fallback.alice, smuggled_json(), RETRIEVE_TOOL)
            .await
            .expect("the fallback retrieve runs");
        assert_eq!(
            off["retrieval_strategy"].as_str(),
            Some("exact"),
            "a caller-supplied strategy key reached the statement selection: {off}"
        );

        let indexed = Owners::built(true).await;
        indexed
            .server
            .store_response(
                &indexed.alice,
                MemoryScope::Personal,
                store_params("an indexed store note"),
            )
            .await;
        let on = indexed
            .server
            .retrieve_scoped(&indexed.alice, smuggled_json(), RETRIEVE_TOOL)
            .await
            .expect("the indexed retrieve runs");
        assert_eq!(
            on["retrieval_strategy"].as_str(),
            Some("indexed"),
            "the same payload that answered `exact` did not answer `indexed` once the server knew about \
             its index, so the label is not driven by the request: {on}"
        );
        assert_eq!(
            off["retrieval_strategy"].as_str(),
            Some("exact"),
            "control arm moved: {off}"
        );
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
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(BoundaryEmbedding),
            0.75,
            HeatSettings::default(),
        )
        .with_retrieve_min_similarity(0.30);

        server
            .do_store_memory(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
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
                scope: None,
                project: None,
                context_project: None,
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
                scope: None,
                project: None,
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

    // --- Dry-run vs wet retrieval: the heat side effect -------------------------
    //
    // Spreading activation only writes anything if three things all line up, so these tests
    // share one fixture built to satisfy every one of them:
    //   1. the retrieved fact must have a `memory_edge` neighbour (`trigger_activation` returns
    //      early when `get_neighbors` comes back empty);
    //   2. that neighbour must be within `max_hops` (2 by default) and its heat delta must clear
    //      `compute_activation_targets`' 0.001 negligible floor (edge strength 1.0 at hop 1 is
    //      1.0 * 0.3 * 1.0 = 0.3);
    //   3. that neighbour must already have a `heat_state` row, because `HeatRepo::add_heat` is
    //      an `UPDATE heat_state ... WHERE memory = ...` that silently matches nothing — and the
    //      caller discards the result with `.ok()` — when there is no row to warm.
    // Point 3 is why the fixture stores through `do_store_memory` (which creates the row at 1.0)
    // rather than `MemoryRepo::create_fact`.

    /// Two memories joined by a real `memory_edge` — the graph activation actually walks.
    async fn seed_edge_pair(server: &AlexandriaServer) -> (String, String) {
        let a = server
            .do_store_memory(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
                content: "seed alpha".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();
        let b = server
            .do_store_memory(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
                content: "seed beta".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();
        EdgeRepo::new(server.db.inner())
            .create_edge(&a, &b, "relates_to", 1.0)
            .await
            .unwrap();
        (a, b)
    }

    /// Heat of both seeded memories, read through `HeatRepo` rather than a query of the test's
    /// own. `None` would mean the row vanished, which is also a change worth failing on.
    async fn seeded_heat(
        server: &AlexandriaServer,
        ids: &(String, String),
    ) -> (Option<f64>, Option<f64>) {
        let heat_repo = HeatRepo::new(server.db.inner());
        (
            heat_repo.get(&ids.0).await.unwrap().map(|h| h.heat),
            heat_repo.get(&ids.1).await.unwrap().map(|h| h.heat),
        )
    }

    /// One `RetrieveMemoriesParams` shared by every call in these three tests, so a difference
    /// in what they assert can only come from the code path, never from the arguments.
    fn seeded_retrieve_params() -> RetrieveMemoriesParams {
        RetrieveMemoriesParams {
            scope: None,
            project: None,
            query: "anything at all".to_string(),
            limit: Some(10),
            session_id: None,
        }
    }

    /// A server with the seeded pair already in it, plus the seeded ids.
    /// A trusted-local write must produce rows its own scoped reader can retrieve.
    ///
    /// Why in-crate rather than in the transport suite: over HTTP the gate always attaches an admitted
    /// hosted principal, and a hosted principal wins over the configured stdio identity, so a request can
    /// never *become* trusted-local — which is correct, and is why D4 refuses hosted writes. On the real
    /// stdio arm there are no HTTP parts at all and the configured identity is what resolves, so the
    /// trusted-local write path is only reachable through the service seam directly. `#[cfg(test)]` items
    /// are invisible to an integration test, and widening them to `pub` would be a new public authority
    /// constructor, which is what the seal exists to prevent.
    ///
    /// The load-bearing assertion is the second one on each row: a row written without an owner or a
    /// partition is invisible to `ScopeFilter::conjunct()`, so the author could never retrieve what they
    /// stored, and nothing along the way would report an error.
    #[tokio::test]
    async fn a_trusted_local_write_is_retrievable_by_its_own_reader() {
        let (server, ids, scope) = server_with_seeded_edge().await;
        // Fresh rows so the assertions below are about *this* write, not the edge fixture's.
        for table in ["fact", "raw", "memory_edge"] {
            server
                .db
                .inner()
                .query(format!("DELETE {table};"))
                .await
                .unwrap()
                .check()
                .unwrap();
        }
        let body = server
            .import_document_scoped(&scope, "# Section\n\nunique import body phrase")
            .await
            .expect(
                "a trusted-local import with a lawful Personal target must be allowed to write",
            );
        assert_eq!(
            body["status"].as_str(),
            Some("ok"),
            "import reported {body}"
        );
        let _ = &ids;

        let facts = server
            .db
            .inner()
            .query("SELECT id, account, workspace, scope FROM fact;")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take::<Vec<serde_json::Value>>(0)
            .unwrap();
        assert_eq!(facts.len(), 1, "the import wrote no chunk row: {facts:?}");
        assert!(
            !facts[0]["account"].is_null(),
            "a trusted-local import wrote a fact with no owner, so its own scoped reader can never \
             retrieve it: {:?}",
            facts[0]
        );
        assert_eq!(
            facts[0]["scope"].as_str(),
            Some("personal"),
            "a trusted-local import wrote a fact with no partition: {:?}",
            facts[0]
        );

        let raws = server
            .db
            .inner()
            .query("SELECT id, account, workspace FROM raw;")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take::<Vec<serde_json::Value>>(0)
            .unwrap();
        assert_eq!(raws.len(), 1, "the import wrote no `raw` document row");
        assert!(
            !raws[0]["account"].is_null(),
            "the `raw` document was written with no owner, so the provenance of a chunk imported under \
             it is unreachable: {:?}",
            raws[0]
        );

        // The decisive end-to-end check: the author reads back what they stored.
        let surfaced = server
            .scoped_retrieve_text(&scope, "unique import body")
            .await
            .expect("the scoped retrieval runs");
        assert!(
            surfaced.contains("unique import body"),
            "a trusted-local write produced rows its own scoped reader cannot retrieve: {surfaced}"
        );
    }

    /// Two trusted-local owners over one fresh store, plus a server on it.
    ///
    /// The capability is claimed before the store is shared (the order production uses —
    /// `AccessRepo::local_principal` needs this instance's capability and an `Arc<Database>` cannot
    /// produce it), and both owners are issued their contexts through the sealed chain: `bootstrap_owner`
    /// writes the rows, `local_principal` + `authorize` + `select` produce what a request would get. There
    /// is no test-only authority constructor here because none may exist.
    ///
    /// Two owners in one store is the point: every case below is about what one account's write does to
    /// another account's rows.
    async fn two_trusted_local_owners() -> (AlexandriaServer, RequestScope, RequestScope) {
        let mut db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let system = db.claim_system_authority().unwrap();
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );
        let alice = trusted_local_scope(&server, &system, "store-alice").await;
        let bob = trusted_local_scope(&server, &system, "store-bob").await;
        (server, alice, bob)
    }

    /// Issues the pair of contexts a trusted-local request of this login would be handed.
    async fn trusted_local_scope(
        server: &AlexandriaServer,
        system: &alexandria_storage::access::SystemContext,
        login: &str,
    ) -> RequestScope {
        let ids = alexandria_storage::repos::IdentityRepo::new(server.db.inner())
            .bootstrap_owner(
                login,
                &format!("$argon2id$fixture-prepared-hash-{login}-padding"),
                &alexandria_storage::models::OperationId::new(login),
            )
            .await
            .unwrap();
        let access = AccessRepo::new(&server.db);
        let principal = access.local_principal(system, &ids.account).await.unwrap();
        let owner = access.authorize(&principal).await.unwrap();
        let read = access
            .select(&owner, ReadSelection::Auto(None))
            .await
            .unwrap();
        RequestScope::issued(owner, read)
    }

    type Task12ProjectWire = tokio::io::BufReader<tokio::io::DuplexStream>;

    async fn task12_project_wire_request(
        wire: &mut Task12ProjectWire,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let request =
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        wire.write_all(format!("{request}\n").as_bytes())
            .await
            .expect("fixture MCP request writes");
        wire.flush().await.expect("fixture MCP request flushes");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mut line = String::new();
            let size = tokio::time::timeout_at(deadline, wire.read_line(&mut line))
                .await
                .expect("fixture MCP response is bounded")
                .expect("fixture MCP response reads");
            assert!(size > 0, "fixture MCP transport closed during {method}");
            let response: serde_json::Value =
                serde_json::from_str(&line).expect("fixture MCP JSON response");
            if response["id"] != 1 {
                continue;
            }
            assert!(
                response.get("error").is_none(),
                "actual MCP {method} refused: {response}"
            );
            return response["result"].clone();
        }
    }

    async fn task12_project_wire_tool(
        wire: &mut Task12ProjectWire,
        name: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        let result = task12_project_wire_request(
            wire,
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        )
        .await;
        assert_ne!(
            result["isError"], true,
            "actual tool {name} refused: {result}"
        );
        assert!(
            result["structuredContent"].is_object(),
            "actual tool {name} must return structured JSON: {result}"
        );
        result["structuredContent"].clone()
    }

    async fn task12_project_wire_fixture() -> (
        AlexandriaServer,
        RequestScope,
        RequestScope,
        Task12ProjectWire,
        rmcp::service::RunningService<RoleServer, AlexandriaServer>,
    ) {
        use rmcp::ServiceExt;
        use tokio::io::AsyncWriteExt;
        // Same sealed two-owner chain as two_trusted_local_owners; retain startup authority only
        // long enough to issue the real stdio principal needed by the actual MCP tool router.
        let mut db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let system = db.claim_system_authority().unwrap();
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );
        let alice = trusted_local_scope(&server, &system, "handle-alice").await;
        let bob = trusted_local_scope(&server, &system, "handle-bob").await;
        let principal = AccessRepo::new(&server.db)
            .local_principal(&system, alice.owner().account())
            .await
            .unwrap();
        let server = server.with_stdio_identity(crate::auth::StdioIdentity::issued(principal));
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let mut wire = tokio::io::BufReader::new(client_io);
        let initialize = async {
            let result = task12_project_wire_request(
                &mut wire,
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05", "capabilities": {},
                    "clientInfo": {"name": "task12-project-handle-fixture", "version": "1"}
                }),
            )
            .await;
            assert!(
                result["serverInfo"].is_object(),
                "actual MCP initialized: {result}"
            );
            wire.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
                .await
                .unwrap();
            wire.flush().await.unwrap();
        };
        let (running, ()) = tokio::join!(server.clone().serve(server_io), initialize);
        (
            server,
            alice,
            bob,
            wire,
            running.expect("fixture MCP service initializes"),
        )
    }

    async fn task12_seed_project_key(
        server: &AlexandriaServer,
        owner: &OwnerContext,
        id: &surrealdb::types::RecordId,
        alias: &str,
    ) {
        let mut response = server
            .db
            .inner()
            .query("CREATE $id SET workspace = $workspace, normalized_alias = $alias RETURN AFTER;")
            .bind(("id", id.clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .bind(("alias", alias.to_string()))
            .await
            .expect("typed project creation executes")
            .check()
            .expect("typed project creation commits");
        let created: Vec<alexandria_storage::models::Project> =
            response.take(0).expect("typed project creation decodes");
        assert_eq!(
            created.len(),
            1,
            "exactly one typed project created for {alias}"
        );
        assert_eq!(
            created[0].id.as_ref(),
            Some(id),
            "creation preserves key type/content for {alias}"
        );
        let mut response = server
            .db
            .inner()
            .query("SELECT * FROM $id WHERE workspace = $workspace;")
            .bind(("id", id.clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .expect("typed project fixture selection executes")
            .check()
            .expect("typed project fixture selection succeeds");
        let read: Vec<alexandria_storage::models::Project> =
            response.take(0).expect("typed fixture selection decodes");
        assert_eq!(read.len(), 1, "typed fixture read retains {alias}");
        assert_eq!(read[0].id.as_ref(), Some(id));
        assert_eq!(read[0].normalized_alias, alias);
    }

    async fn task12_project_handle_roundtrip(keys: [surrealdb::types::RecordId; 2]) {
        let (server, alice, bob, mut wire, running) = task12_project_wire_fixture().await;
        let aliases = ["handle-first", "handle-second"];
        for (id, alias) in keys.iter().zip(aliases) {
            task12_seed_project_key(&server, alice.owner(), id, alias).await;
        }
        let foreign = surrealdb::types::RecordId::new("project", "foreign-handle-control");
        task12_seed_project_key(&server, bob.owner(), &foreign, "foreign-control").await;
        let catalog =
            alexandria_storage::repos::ScopedProjectRepo::new(server.db.inner(), alice.owner())
                .list(None, 10)
                .await
                .expect("owned typed catalog positive control");
        assert_eq!(
            catalog.entries.len(),
            2,
            "owned catalog contains both typed twins, not foreign row"
        );
        for id in &keys {
            assert!(
                catalog
                    .entries
                    .iter()
                    .any(|entry| entry.project.id.as_record_id() == id),
                "actual typed catalog contains {id:?} before wire assertions"
            );
        }
        let listed =
            task12_project_wire_tool(&mut wire, "list_projects", serde_json::json!({"limit": 10}))
                .await;
        let entries = listed["projects"]
            .as_array()
            .expect("actual list_projects JSON entries");
        assert_eq!(
            entries.len(),
            2,
            "actual project listing remains owner-bound"
        );
        let handles: Vec<String> = aliases
            .iter()
            .map(|alias| {
                entries
                    .iter()
                    .find(|entry| entry["canonical_identity"] == *alias)
                    .expect("actual list contains owned fixture alias")["id"]
                    .as_str()
                    .expect("actual list ID is wire text")
                    .to_string()
            })
            .collect();
        assert_ne!(
            handles[0], handles[1],
            "distinct actual project keys require distinct public JSON handles"
        );
        let repo = ScopedReminderRepo::new(server.db.inner(), alice.owner().clone());
        for (index, alias) in aliases.into_iter().enumerate() {
            let resolved = task12_project_wire_tool(
                &mut wire,
                "resolve_project",
                serde_json::json!({"project": alias}),
            )
            .await;
            assert_eq!(
                resolved["project"]["id"], handles[index],
                "actual resolve/list producers agree"
            );
            let created = task12_project_wire_tool(
                &mut wire,
                "set_reminder",
                serde_json::json!({
                    "message": format!("roundtrip {alias}"),
                    "due_at": (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339(),
                    "target_project": handles[index],
                }),
            )
            .await;
            let reminder_id = created["id"]
                .as_str()
                .expect("actual reminder creation returns its ID");
            let stored = repo
                .get(reminder_id)
                .await
                .unwrap()
                .expect("actual created reminder exists");
            assert_eq!(
                stored.project.as_ref(),
                Some(&keys[index]),
                "copying the actual project handle must preserve the intended typed DB target"
            );
            let delivered = task12_project_wire_tool(
                &mut wire,
                "check_reminders",
                serde_json::json!({"project": handles[index]}),
            )
            .await;
            assert_eq!(delivered["count"], 1);
            assert_eq!(delivered["delivered"][0]["id"], reminder_id);
            assert_eq!(delivered["delivered"][0]["target"], handles[index]);
            let history = task12_project_wire_tool(
                &mut wire,
                "list_reminders",
                serde_json::json!({"status": "all", "target_project": handles[index]}),
            )
            .await;
            assert_eq!(
                history["count"], 1,
                "typed filter must distinguish the two project keys"
            );
            assert_eq!(history["reminders"][0]["id"], reminder_id);
            assert_eq!(history["reminders"][0]["target"], handles[index]);
            let stored = repo.get(reminder_id).await.unwrap().unwrap();
            assert_eq!(stored.project.as_ref(), Some(&keys[index]));
            assert_eq!(stored.delivered_count, 1);
        }
        drop(running);
    }

    #[tokio::test]
    async fn task12_project_handle_numeric_and_string_twin_roundtrip() {
        task12_project_handle_roundtrip([
            surrealdb::types::RecordId::new("project", 7_i64),
            surrealdb::types::RecordId::new("project", "7"),
        ])
        .await;
    }

    #[tokio::test]
    async fn task12_project_handle_trailing_input_refuses_owned_prefix() {
        let (server, alice, bob, mut wire, running) = task12_project_wire_fixture().await;
        let keys = [
            surrealdb::types::RecordId::new("project", "inside;semicolon"),
            surrealdb::types::RecordId::new("project", "tail-control"),
        ];
        let aliases = ["quoted-semicolon-control", "tail-control"];
        for (id, alias) in keys.iter().zip(aliases) {
            task12_seed_project_key(&server, alice.owner(), id, alias).await;
        }
        let foreign = surrealdb::types::RecordId::new("project", "foreign-tail-control");
        task12_seed_project_key(&server, bob.owner(), &foreign, "foreign-tail-control").await;
        let catalog =
            alexandria_storage::repos::ScopedProjectRepo::new(server.db.inner(), alice.owner())
                .list(None, 10)
                .await
                .unwrap();
        assert_eq!(catalog.entries.len(), 2);
        for id in &keys {
            assert!(
                catalog
                    .entries
                    .iter()
                    .any(|entry| entry.project.id.as_record_id() == id)
            );
        }
        let listed =
            task12_project_wire_tool(&mut wire, "list_projects", serde_json::json!({"limit": 10}))
                .await;
        let entries = listed["projects"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "actual producer excludes foreign project");
        let handles: Vec<String> = aliases
            .iter()
            .map(|alias| {
                entries
                    .iter()
                    .find(|entry| entry["canonical_identity"] == *alias)
                    .unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let resolved = task12_project_wire_tool(
            &mut wire,
            "resolve_project",
            serde_json::json!({"project": handles[0]}),
        )
        .await;
        assert_eq!(
            resolved["project"]["id"], handles[0],
            "semicolon INSIDE a quoted key remains a valid point"
        );
        let created = task12_project_wire_tool(
            &mut wire,
            "set_reminder",
            serde_json::json!({
                "message": "owned quoted-semicolon control", "target_project": handles[0],
                "due_at": (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339(),
            }),
        )
        .await;
        let repo = ScopedReminderRepo::new(server.db.inner(), alice.owner().clone());
        let saved = repo
            .get(created["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.project.as_ref(), Some(&keys[0]));
        assert_eq!(saved.delivered_count, 0);
        let before = serde_json::to_value(repo.list(None, None, 100, 0).await.unwrap().0).unwrap();
        let trailing = format!("{}; {}", handles[0], handles[1]);
        // Check FIRST: the prefix is genuinely owned and due, so lookup cannot mask parser acceptance.
        for (name, arguments) in [
            ("check_reminders", serde_json::json!({"project": trailing})),
            (
                "set_reminder",
                serde_json::json!({"message": "must not create", "target_project": trailing,
                "due_at": (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339()}),
            ),
            (
                "list_reminders",
                serde_json::json!({"status": "all", "target_project": trailing}),
            ),
        ] {
            let result = task12_project_wire_request(
                &mut wire,
                "tools/call",
                serde_json::json!({"name": name, "arguments": arguments}),
            )
            .await;
            assert_eq!(
                result["isError"], true,
                "actual {name} must reject non-full-input selector {trailing:?}: {result}"
            );
            assert_eq!(
                serde_json::to_value(repo.list(None, None, 100, 0).await.unwrap().0).unwrap(),
                before,
                "malformed selectors cannot create/consume/mutate reminders"
            );
        }
        let projects_after =
            task12_project_wire_tool(&mut wire, "list_projects", serde_json::json!({"limit": 10}))
                .await;
        assert_eq!(
            projects_after, listed,
            "strict reminder selectors never establish aliases"
        );
        let delivered = task12_project_wire_tool(
            &mut wire,
            "check_reminders",
            serde_json::json!({"project": handles[0]}),
        )
        .await;
        assert_eq!(
            delivered["count"], 1,
            "valid quoted-semicolon point still delivers after refusals"
        );
        assert_eq!(delivered["delivered"][0]["id"], created["id"]);
        assert_eq!(delivered["delivered"][0]["target"], handles[0]);
        let repeated = task12_project_wire_tool(
            &mut wire,
            "check_reminders",
            serde_json::json!({"project": handles[0]}),
        )
        .await;
        assert_eq!(repeated["count"], 0);
        drop(running);
    }

    #[tokio::test]
    async fn task12_project_handle_uuid_and_string_twin_roundtrip() {
        const UUID_TEXT: &str = "123e4567-e89b-42d3-a456-426614174000";
        let uuid = UUID_TEXT.parse::<surrealdb::types::Uuid>().unwrap();
        task12_project_handle_roundtrip([
            surrealdb::types::RecordId::new("project", uuid),
            surrealdb::types::RecordId::new("project", UUID_TEXT),
        ])
        .await;
    }

    #[tokio::test]
    async fn task12_project_handle_literal_backticks_and_plain_twin_roundtrip() {
        task12_project_handle_roundtrip([
            surrealdb::types::RecordId::new("project", "`ticktwin`"),
            surrealdb::types::RecordId::new("project", "ticktwin"),
        ])
        .await;
    }

    #[tokio::test]
    async fn task12_project_handle_reminder_selectors_refuse_nonliteral_range_wrong_table_and_foreign()
     {
        let (server, alice, bob, mut wire, running) = task12_project_wire_fixture().await;
        let foreign = surrealdb::types::RecordId::new("project", "foreign_control");
        task12_seed_project_key(&server, bob.owner(), &foreign, "foreign-control").await;
        let owned = task12_project_wire_tool(
            &mut wire,
            "resolve_project",
            serde_json::json!({"project": "owned-control"}),
        )
        .await;
        let own_handle = owned["project"]["id"].as_str().unwrap();
        for selector in ["project:", "project:1..9", "workspace:7", "fact:7"] {
            let result = task12_project_wire_request(
                &mut wire,
                "tools/call",
                serde_json::json!({
                    "name": "resolve_project", "arguments": {"project": selector}
                }),
            )
            .await;
            assert_eq!(
                result["isError"], true,
                "explicit malformed/wrong-table selector cannot establish an alias: {result}"
            );
        }
        let invalid = [
            "",
            "owned-control",
            "workspace:7",
            "fact:7",
            "project:missing",
            "project:",
            "project:1..9",
            "[project:7]",
            "type::record('project', 7)",
            "project:7; project:8",
            "7",
        ];
        let mut invalid: Vec<String> = invalid.into_iter().map(str::to_string).collect();
        invalid.push(format_project_handle(&foreign));
        for selector in invalid {
            for (name, arguments) in [
                (
                    "set_reminder",
                    serde_json::json!({"message": "must not become global",
                    "due_at": (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339(), "target_project": selector}),
                ),
                ("check_reminders", serde_json::json!({"project": selector})),
                (
                    "list_reminders",
                    serde_json::json!({"status": "all", "target_project": selector}),
                ),
            ] {
                let result = task12_project_wire_request(
                    &mut wire,
                    "tools/call",
                    serde_json::json!({"name": name, "arguments": arguments}),
                )
                .await;
                assert_eq!(
                    result["isError"], true,
                    "actual {name} must refuse selector {selector:?}: {result}"
                );
            }
        }
        let repo = ScopedReminderRepo::new(server.db.inner(), alice.owner().clone());
        assert!(
            repo.list(None, None, 100, 0).await.unwrap().0.is_empty(),
            "invalid targets cannot create global reminders"
        );
        let projects =
            task12_project_wire_tool(&mut wire, "list_projects", serde_json::json!({})).await;
        assert_eq!(
            projects["count"], 1,
            "strict reminder selectors never establish aliases or reveal foreign rows"
        );
        let created = task12_project_wire_tool(&mut wire, "set_reminder", serde_json::json!({
            "message": "owned positive control", "due_at": (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339(),
            "target_project": own_handle,
        })).await;
        let delivered = task12_project_wire_tool(
            &mut wire,
            "check_reminders",
            serde_json::json!({"project": own_handle}),
        )
        .await;
        assert_eq!(delivered["count"], 1);
        assert_eq!(delivered["delivered"][0]["id"], created["id"]);
        assert_eq!(delivered["delivered"][0]["target"], own_handle);
        drop(running);
    }

    fn typed_reminder_request(target: &str) -> SetReminderParams {
        SetReminderParams {
            message: "typed service delivery witness".into(),
            due_at: Some((Utc::now() - chrono::Duration::minutes(1)).to_rfc3339()),
            pattern: None,
            cron: None,
            target_project: Some(target.into()),
            prov_project: Some("display-only-not-routing".into()),
            session_id: None,
            note: None,
        }
    }

    #[tokio::test]
    async fn task12_reminder_api_checks_owned_typed_context_even_with_no_due_rows() {
        let (server, alice, bob) = two_trusted_local_owners().await;
        let foreign =
            alexandria_storage::repos::ScopedProjectRepo::new(server.db.inner(), bob.owner())
                .establish("same-alias", None)
                .await
                .unwrap();
        let foreign = record_id_to_string(foreign.project.id.as_record_id());
        for project in [foreign, "same-alias".into(), "project:missing".into()] {
            assert!(
                server
                    .reminder_check(
                        &alice,
                        &CheckRemindersParams {
                            project: Some(project.clone())
                        }
                    )
                    .await
                    .is_err(),
                "typed delivery context {project:?} must be owned, not a hint or fallback"
            );
            assert!(
                server
                    .reminder_list(
                        &alice,
                        &ListRemindersParams {
                            status: Some("all".into()),
                            target_project: Some(project.clone()),
                            limit: None,
                            offset: None,
                        }
                    )
                    .await
                    .is_err(),
                "typed list filter {project:?} must be owned too"
            );
        }
    }

    #[tokio::test]
    async fn task12_reminder_api_holds_a_different_owned_project_then_delivers_exact_target() {
        let (server, alice, _) = two_trusted_local_owners().await;
        let projects =
            alexandria_storage::repos::ScopedProjectRepo::new(server.db.inner(), alice.owner());
        let own = projects.establish("target", None).await.unwrap();
        let other = projects.establish("other", None).await.unwrap();
        let target = record_id_to_string(own.project.id.as_record_id());
        let created: serde_json::Value = serde_json::from_str(
            &server
                .reminder_create(&alice, typed_reminder_request(&target))
                .await
                .unwrap(),
        )
        .unwrap();
        let held: serde_json::Value = serde_json::from_str(
            &server
                .reminder_check(
                    &alice,
                    &CheckRemindersParams {
                        project: Some(record_id_to_string(other.project.id.as_record_id())),
                    },
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            held["count"], 0,
            "a typed target is not global merely because legacy text is NONE"
        );
        let delivered: serde_json::Value = serde_json::from_str(
            &server
                .reminder_check(
                    &alice,
                    &CheckRemindersParams {
                        project: Some(target.clone()),
                    },
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(delivered["count"], 1);
        assert_eq!(delivered["delivered"][0]["id"], created["id"]);
        assert_eq!(
            delivered["delivered"][0]["target"], target,
            "never prefix an already typed handle twice"
        );
        assert_eq!(delivered["delivered"][0]["escalated"], false);
        assert_eq!(delivered["delivered"][0]["missed_occurrences"], 0);
        assert_eq!(
            delivered["delivered"][0]["missed_occurrences_saturated"],
            false
        );
        let again: serde_json::Value = serde_json::from_str(
            &server
                .reminder_check(&alice, &CheckRemindersParams { project: None })
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(again["count"], 0, "the one-shot is consumed exactly once");
    }

    #[tokio::test]
    async fn task12_reminder_api_recurring_claim_reports_coalescing_and_advances_once() {
        let (server, alice, _) = two_trusted_local_owners().await;
        let own =
            alexandria_storage::repos::ScopedProjectRepo::new(server.db.inner(), alice.owner())
                .establish("recurring-target", None)
                .await
                .unwrap();
        let target = record_id_to_string(own.project.id.as_record_id());
        let mut params = typed_reminder_request(&target);
        params.due_at = None;
        params.cron = Some("* * * * *".into());
        let created: serde_json::Value =
            serde_json::from_str(&server.reminder_create(&alice, params).await.unwrap()).unwrap();
        let repo = ScopedReminderRepo::new(server.db.inner(), alice.owner().clone());
        let id = created["id"].as_str().unwrap();
        let row = repo.get(id).await.unwrap().unwrap();
        let due = Utc::now() - chrono::Duration::minutes(3);
        server
            .db
            .inner()
            .query("UPDATE $id SET next_due_at = $due;")
            .bind(("id", row.id.unwrap()))
            .bind(("due", due))
            .await
            .unwrap()
            .check()
            .unwrap();
        let delivered: serde_json::Value = serde_json::from_str(
            &server
                .reminder_check(
                    &alice,
                    &CheckRemindersParams {
                        project: Some(target.clone()),
                    },
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(delivered["count"], 1);
        let fire = &delivered["delivered"][0];
        assert_eq!(fire["target"], target);
        assert_eq!(fire["recurring"], true);
        assert!(
            fire["missed_occurrences"]
                .as_u64()
                .is_some_and(|count| count >= 2)
        );
        assert_eq!(fire["missed_occurrences_saturated"], false);
        assert_eq!(fire["due_at"], rfc3339_utc(due));
        let stored = repo.get(id).await.unwrap().unwrap();
        assert_eq!(stored.status, "pending");
        assert_eq!(stored.delivered_count, 1);
        assert!(stored.next_due_at.unwrap() > Utc::now());
        let again: serde_json::Value = serde_json::from_str(
            &server
                .reminder_check(&alice, &CheckRemindersParams { project: None })
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(again["count"], 0);
        assert_eq!(repo.get(id).await.unwrap().unwrap().delivered_count, 1);
    }

    #[tokio::test]
    async fn task12_reminder_api_keeps_held_history_and_typed_piggyback_without_consuming() {
        let (server, alice, _) = two_trusted_local_owners().await;
        let project =
            alexandria_storage::repos::ScopedProjectRepo::new(server.db.inner(), alice.owner())
                .establish("history-target", None)
                .await
                .unwrap();
        let target = record_id_to_string(project.project.id.as_record_id());
        let repo = ScopedReminderRepo::new(server.db.inner(), alice.owner().clone());
        let mut ids = Vec::new();
        for message in ["typed eligible", "typed held", "unresolved history"] {
            let mut params = typed_reminder_request(&target);
            params.message = message.into();
            let created: serde_json::Value =
                serde_json::from_str(&server.reminder_create(&alice, params).await.unwrap())
                    .unwrap();
            ids.push(created["id"].as_str().unwrap().to_string());
        }
        let held = repo.get(&ids[1]).await.unwrap().unwrap();
        server
            .db
            .inner()
            .query("UPDATE $id SET migration_hold = true;")
            .bind(("id", held.id.unwrap()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let legacy = repo.get(&ids[2]).await.unwrap().unwrap();
        server.db.inner().query("UPDATE $id SET project = NONE, target_project = 'old-name', legacy_target_project = 'old-name';")
            .bind(("id", legacy.id.unwrap())).await.unwrap().check().unwrap();
        let sample = server.due_reminders_summary_for(&alice, 5).await;
        assert_eq!(sample.len(), 1);
        assert_eq!(sample[0]["id"], ids[0]);
        assert_eq!(sample[0]["target"], target);
        assert_eq!(
            repo.get(&ids[0]).await.unwrap().unwrap().delivered_count,
            0,
            "piggyback is informational, not the consuming check"
        );
        let history = server
            .reminder_list(
                &alice,
                &ListRemindersParams {
                    status: Some("all".into()),
                    target_project: None,
                    limit: Some(100),
                    offset: None,
                },
            )
            .await
            .unwrap();
        let rows = history["reminders"].as_array().unwrap();
        assert_eq!(rows.len(), 3, "suppression must not erase owned history");
        let targeted = server
            .reminder_list(
                &alice,
                &ListRemindersParams {
                    status: Some("all".into()),
                    target_project: Some(target.clone()),
                    limit: Some(100),
                    offset: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            targeted["count"], 2,
            "typed history filter includes the held typed row, not unresolved text"
        );
        assert!(
            targeted["reminders"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["target"] == target)
        );
        let held = rows.iter().find(|row| row["id"] == ids[1]).unwrap();
        assert_eq!(held["migration_hold"], true);
        assert_eq!(held["target"], target);
        let legacy = rows.iter().find(|row| row["id"] == ids[2]).unwrap();
        assert!(
            legacy["target"].is_null(),
            "unresolved history must not be labelled global"
        );
        assert_eq!(legacy["legacy_target_project"], "old-name");
        let delivered: serde_json::Value = serde_json::from_str(
            &server
                .reminder_check(
                    &alice,
                    &CheckRemindersParams {
                        project: Some(target),
                    },
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(delivered["count"], 1);
        for id in &ids[1..] {
            let row = repo.get(id).await.unwrap().unwrap();
            assert_eq!(row.status, "pending");
            assert_eq!(row.delivered_count, 0);
        }
        server
            .reminder_cancel(&alice, &CancelReminderParams { id: ids[1].clone() })
            .await
            .unwrap();
        assert_eq!(
            repo.get(&ids[1]).await.unwrap().unwrap().status,
            "cancelled"
        );
    }

    #[tokio::test]
    async fn task12_reminder_api_global_and_escalation_stay_within_the_owner_account() {
        let (mut server, alice, bob) = two_trusted_local_owners().await;
        server.reminders.escalation_hours = 1;
        let project =
            alexandria_storage::repos::ScopedProjectRepo::new(server.db.inner(), alice.owner())
                .establish("escalation-target", None)
                .await
                .unwrap();
        let target = record_id_to_string(project.project.id.as_record_id());
        let mut targeted = typed_reminder_request(&target);
        targeted.due_at = Some((Utc::now() - chrono::Duration::hours(2)).to_rfc3339());
        server.reminder_create(&alice, targeted).await.unwrap();
        let mut global = typed_reminder_request(&target);
        global.target_project = None;
        server.reminder_create(&alice, global).await.unwrap();
        let mut foreign = typed_reminder_request(&target);
        foreign.target_project = None;
        let foreign: serde_json::Value =
            serde_json::from_str(&server.reminder_create(&bob, foreign).await.unwrap()).unwrap();
        let delivered: serde_json::Value = serde_json::from_str(
            &server
                .reminder_check(&alice, &CheckRemindersParams { project: None })
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(delivered["count"], 2);
        let rows = delivered["delivered"].as_array().unwrap();
        assert!(
            rows.iter()
                .any(|row| row["target"] == "global" && row["escalated"] == false)
        );
        assert!(
            rows.iter()
                .any(|row| row["target"] == target && row["escalated"] == true)
        );
        let bobs = ScopedReminderRepo::new(server.db.inner(), bob.owner().clone());
        let retained = bobs
            .get(foreign["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.status, "pending");
        assert_eq!(retained.delivered_count, 0);
    }

    /// One `StoreMemoryParams` with nothing but content, so each case varies only what it names.
    fn store_params(content: &str) -> StoreMemoryParams {
        StoreMemoryParams {
            scope: None,
            project: None,
            context_project: None,
            content: content.to_string(),
            tags: None,
            session_id: None,
            agent_id: None,
            model: None,
        }
    }

    /// `store_memory` is the one partition-bearing write D4 admits, so it is also the one write whose
    /// rows a hosted test could never reach: hosted stores are refused before anything is written. This
    /// drives the handler's own body against the trusted-local arm — the shape `main.rs` wires for
    /// stdio — and asserts the round trip the four earlier lanes never made: the tool that stores, and
    /// the tool that reads, over the rows that store produced.
    ///
    /// Before this test existed, every scoped transport test seeded its rows with raw SQL, so the store
    /// path could write rows its own server could not retrieve and 294 green tests said nothing.
    #[tokio::test]
    async fn a_trusted_local_store_is_retrievable_by_its_own_reader() {
        let (server, alice, bob) = two_trusted_local_owners().await;
        let result = server
            .store_response(
                &alice,
                MemoryScope::Personal,
                StoreMemoryParams {
                    scope: None,
                    project: None,
                    context_project: None,
                    session_id: Some("pi:store-round-trip".to_string()),
                    agent_id: Some("pi".to_string()),
                    ..store_params("the deploy runs from the api gateway host")
                },
            )
            .await;
        assert_eq!(
            result.structured_content.expect("structured content")["status"],
            "ok",
            "a trusted-local store with a lawful Personal target must be allowed to write"
        );

        let facts = server
            .db
            .inner()
            .query("SELECT id, account, workspace, scope FROM fact;")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take::<Vec<serde_json::Value>>(0)
            .unwrap();
        assert_eq!(facts.len(), 1, "the store wrote no fact row: {facts:?}");
        assert!(
            !facts[0]["account"].is_null(),
            "the store path wrote a fact with no owner, so its own scoped reader can never retrieve \
             it: {:?}",
            facts[0]
        );
        assert_eq!(
            facts[0]["scope"].as_str(),
            Some("personal"),
            "the store path wrote a fact with no partition: {:?}",
            facts[0]
        );

        // The session the same call created: v010 keyed the handle index on (account, external_id), so
        // an unowned session is a session nobody's reader can resolve.
        let sessions = server
            .db
            .inner()
            .query("SELECT id, account, workspace FROM `session`;")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take::<Vec<serde_json::Value>>(0)
            .unwrap();
        assert_eq!(
            sessions.len(),
            1,
            "the store wrote no session row: {sessions:?}"
        );
        assert!(
            !sessions[0]["account"].is_null(),
            "the store path created a session with no owner, so `get_session` and `list_sessions` \
             cannot see the session they are supposed to describe: {:?}",
            sessions[0]
        );

        // The decisive pair: the author reads back what the author stored.
        let surfaced = server
            .scoped_retrieve_text(&alice, "api gateway host deploy")
            .await
            .expect("the scoped retrieval runs");
        assert!(
            surfaced.contains("api gateway host"),
            "a trusted-local store produced a row its own scoped reader cannot retrieve: {surfaced}"
        );

        // …and the other account cannot see it, which is the same row set proving the stamp is *alice's*.
        let foreign = server
            .scoped_retrieve_text(&bob, "api gateway host deploy")
            .await
            .expect("the foreign scoped retrieval runs");
        assert!(
            !foreign.contains("api gateway host"),
            "bob retrieved alice's stored memory: {foreign}"
        );
        let session_detail = server
            .session_detail(&alice, "pi:store-round-trip", SessionView::Scoped)
            .await
            .expect("the owner reads their own session");
        assert!(
            session_detail.contains("api gateway host"),
            "the session linkage did not reach the owner's own read: {session_detail}"
        );
        assert!(
            server
                .session_detail(&bob, "pi:store-round-trip", SessionView::Scoped)
                .await
                .is_err(),
            "a foreign account resolved the same session handle to a row"
        );
    }

    /// The provenance row a store writes describes *that* fact and belongs to *that* account.
    ///
    /// The request path used to issue `CREATE provenance SET kind = 'user', timestamp = time::now()` and
    /// nothing else: no owner (though v009 gave `provenance` `account`/`workspace`), and no
    /// `has_provenance` edge, so the row described no memory and no reader could attribute it.
    #[tokio::test]
    async fn a_stored_memory_stamps_and_links_its_provenance() {
        let (server, alice, _bob) = two_trusted_local_owners().await;
        server
            .store_response(
                &alice,
                MemoryScope::Personal,
                store_params("the nightly index rebuild starts at two"),
            )
            .await;

        let rows = server
            .db
            .inner()
            .query("SELECT id, kind, account, workspace, (->has_provenance) AS links FROM fact;")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take::<Vec<serde_json::Value>>(0)
            .unwrap();
        assert_eq!(rows.len(), 1, "the store wrote no fact: {rows:?}");
        let links = rows[0]["links"]
            .as_array()
            .expect("a has_provenance traversal");
        assert_eq!(
            links.len(),
            1,
            "the stored fact has no provenance link, so the row describing it belongs to no memory: {:?}",
            rows[0]
        );

        let provenance = server
            .db
            .inner()
            .query("SELECT id, kind, account, workspace FROM provenance;")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take::<Vec<serde_json::Value>>(0)
            .unwrap();
        assert_eq!(provenance.len(), 1, "the store wrote no provenance row");
        assert_eq!(provenance[0]["kind"].as_str(), Some("user"));
        assert!(
            !provenance[0]["account"].is_null() && !provenance[0]["workspace"].is_null(),
            "the request path wrote an ownership-bearing row unstamped: {:?}",
            provenance[0]
        );
    }

    /// A cluster the store path makes belongs to the account that stored the memory.
    ///
    /// The unscoped `ClusterRepo::create` writes `label`/`centroid`/`depth` and no owner, while
    /// `ScopedClusterRepo::list` filters on `workspace` — so every cluster the request path created was
    /// invisible to the account that paid for it, `recall` had no candidates for anybody, and each imported
    /// chunk spawned another unreachable cluster rather than joining an existing one.
    #[tokio::test]
    async fn a_stored_cluster_belongs_to_the_account_that_stored() {
        let (server, alice, bob) = two_trusted_local_owners().await;
        let first = server
            .store_response(
                &alice,
                MemoryScope::Personal,
                store_params("alice's gateway rotation note"),
            )
            .await;
        assert_eq!(
            first.structured_content.expect("structured content")["status"],
            "ok",
            "the first store did not happen, so every cluster assertion below describes an empty store"
        );

        let alice_cluster = ScopedClusterRepo::new(server.db.inner(), alice.owner(), alice.read())
            .list_with_counts()
            .await
            .unwrap();
        assert_eq!(
            alice_cluster.len(),
            1,
            "alice's own stored memory created no cluster alice can see — broad `recall` would return \
             nothing for every account in the store"
        );
        assert_eq!(
            alice_cluster[0].1, 1,
            "alice's cluster reports no member of hers"
        );
        let owned = server
            .db
            .inner()
            .query("SELECT account, workspace FROM cluster WHERE id = $cluster;")
            .bind((
                "cluster",
                alice_cluster[0]
                    .0
                    .id
                    .clone()
                    .expect("a listed cluster carries an id"),
            ))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take::<Vec<serde_json::Value>>(0)
            .unwrap();
        assert_eq!(
            owned.first().and_then(|row| row["account"].as_str()),
            Some(record_id_to_string(alice.owner().account().as_record_id()).as_str()),
            "the cluster the request path created is not owned by the account that created it: {owned:?}"
        );

        // The other account sees nothing of it — and a second store by alice joins her own cluster
        // instead of spawning a second unreachable one.
        let foreign = ScopedClusterRepo::new(server.db.inner(), bob.owner(), bob.read())
            .list()
            .await
            .unwrap();
        assert!(
            foreign.is_empty(),
            "bob's cluster list returned alice's cluster: {foreign:?}"
        );
        // The half that was only ever asserted by a count. A cluster list of one looks the same whether
        // the second store joined it or failed inside `add_member`, so this asserts the store succeeded
        // *and* that the cluster it was supposed to join now holds two members.
        let second = server
            .store_response(
                &alice,
                MemoryScope::Personal,
                store_params("a second alice note"),
            )
            .await;
        assert_eq!(
            second.structured_content.expect("structured content")["status"],
            "ok",
            "a store into the owner's own existing cluster failed; the member count below cannot see \
             that it never happened"
        );
        let after = ScopedClusterRepo::new(server.db.inner(), alice.owner(), alice.read())
            .list_with_counts()
            .await
            .unwrap();
        assert_eq!(
            after.len(),
            1,
            "a second store by the same owner spawned a second cluster instead of joining their own"
        );
        assert_eq!(
            after[0].1, 2,
            "the cluster the second store claims to have joined holds {} member, so the join never \
             happened",
            after[0].1
        );
    }

    /// A stored memory may not be attached to another account's cluster.
    ///
    /// The store path passed `None` to the cluster pass, which took the store-wide cluster list: a
    /// centroid near the new vector related the caller's fact into **another account's** cluster and
    /// rewrote that cluster's centroid. The membership edge is what is counted here rather than a
    /// `get_members` result, because `get_members` filters members by the *reader's* workspace — it would
    /// hide the foreign row instead of proving the write never happened.
    #[tokio::test]
    async fn a_stored_memory_never_reaches_a_foreign_cluster() {
        let (server, alice, bob) = two_trusted_local_owners().await;
        // Bob owns a cluster, made by bob's own store, holding one of bob's own memories.
        server
            .store_response(
                &bob,
                MemoryScope::Personal,
                store_params("bob's private deployment history note"),
            )
            .await;
        let bobs_cluster = ScopedClusterRepo::new(server.db.inner(), bob.owner(), bob.read())
            .list()
            .await
            .unwrap()
            .remove(0)
            .id
            .expect("bob's store created a cluster he can see");
        let members_before = cluster_members(&server, &bobs_cluster).await;
        assert_eq!(
            members_before, 1,
            "fixture: bob's own cluster holds bob's own fact, so the count below can only fall by never \
             growing"
        );

        server
            .store_response(
                &alice,
                MemoryScope::Personal,
                store_params("alice's unrelated gateway note"),
            )
            .await;

        assert_eq!(
            cluster_members(&server, &bobs_cluster).await,
            members_before,
            "alice's store attached her fact to bob's cluster: the store path joined a cluster list it \
             was never granted"
        );
    }

    /// Membership edges of one cluster, as stored.
    async fn cluster_members(
        server: &AlexandriaServer,
        cluster: &surrealdb::types::RecordId,
    ) -> i64 {
        server
            .db
            .inner()
            .query("SELECT count() AS c FROM contains_memory WHERE in = $cluster GROUP ALL;")
            .bind(("cluster", cluster.clone()))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take::<Vec<serde_json::Value>>(0)
            .unwrap()
            .first()
            .and_then(|row| row["c"].as_i64())
            .unwrap_or(0)
    }

    /// One activation pass claims the writer's authority **once**, not once per warm target.
    ///
    /// This is the measurement the transaction-cost finding asked for, and the counter is the engine's
    /// own: a `DEFINE EVENT` on `account` ticks one row per committed change to that row, and claiming
    /// authority *is* a conditional write on it (`updated_at = time::now()`, which always differs). So the
    /// tick count for one pass is the number of guarded transactions that pass opened — no instrumentation
    /// in production code, and a rolled-back transaction contributes nothing, which is what makes the
    /// number mean *committed* claims.
    ///
    /// Per-target claims were the defect: one retrieval over `activation_top_n` focals with several
    /// neighbours each opened a dozen transactions all contending on the same account row, so two
    /// concurrent retrieves by one account were guaranteed conflict opponents whose dropped warms surfaced
    /// only in a `debug!` line.
    #[tokio::test]
    async fn one_activation_pass_claims_authority_once() {
        let (server, alice, _bob) = two_trusted_local_owners().await;
        let focal = server
            .store_response(
                &alice,
                MemoryScope::Personal,
                store_params("the focal record of the pass"),
            )
            .await
            .structured_content
            .expect("stored")["id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut neighbours = Vec::new();
        for i in 0..3 {
            let id = server
                .store_response(
                    &alice,
                    MemoryScope::Personal,
                    store_params(&format!("neighbour {i} of the focal")),
                )
                .await
                .structured_content
                .expect("stored")["id"]
                .as_str()
                .unwrap()
                .to_string();
            EdgeRepo::new(server.db.inner())
                .create_edge(&focal, &id, "relates_to", 1.0)
                .await
                .unwrap();
            neighbours.push(id);
        }
        // One row reachable only from a neighbour, not from the focal record: the pass is two hops deep,
        // so a traversal that forgot to advance its frontier would warm fewer rows and still claim once,
        // and the count below would be measuring a smaller pass than production runs.
        let second_hop = server
            .store_response(
                &alice,
                MemoryScope::Personal,
                store_params("two hops from the focal record"),
            )
            .await
            .structured_content
            .expect("stored")["id"]
            .as_str()
            .unwrap()
            .to_string();
        EdgeRepo::new(server.db.inner())
            .create_edge(&neighbours[0], &second_hop, "relates_to", 1.0)
            .await
            .unwrap();
        neighbours.push(second_hop);

        server
            .db
            .inner()
            .query(
                "DEFINE TABLE claim_tick SCHEMALESS PERMISSIONS FULL; DEFINE EVENT count_claims ON \
                 TABLE account WHEN $event = 'UPDATE' THEN CREATE claim_tick SET at = time::now();",
            )
            .await
            .unwrap()
            .check()
            .unwrap();

        let before = claim_ticks(&server).await;
        server
            .trigger_activation(&alice, std::slice::from_ref(&focal), 1.0)
            .await
            .expect("the activation pass runs");
        let after = claim_ticks(&server).await;

        // The pass must have actually warmed: three neighbours, each with its own `UPDATE`, in one
        // transaction. Without this the claim count could be one because nothing ran.
        for id in &neighbours {
            let heat = HeatRepo::new(server.db.inner())
                .get(id)
                .await
                .unwrap()
                .expect("a stored memory has heat state");
            assert!(
                heat.heat > 1.0,
                "neighbour {id} was not warmed, so the claim count below measures a pass that did nothing"
            );
        }
        assert_eq!(
            after - before,
            1,
            "one activation pass over {} warm targets opened {claimed} guarded transactions",
            neighbours.len(),
            claimed = after - before
        );
        assert_eq!(
            neighbours.len(),
            4,
            "the pass must reach the second hop for the claim count to describe the accepted traversal"
        );

        // The counter is live rather than decorative: a plain committed write to the same row moves it.
        server
            .db
            .inner()
            .query("UPDATE ONLY account SET updated_at = time::now() WHERE id = $id;")
            .bind(("id", alice.owner().account().as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(
            claim_ticks(&server).await,
            after + 1,
            "the event counter never counted anything, so the assertion above proves nothing"
        );
    }

    /// Committed authority claims on the fixture account, as counted by the engine's own event.
    async fn claim_ticks(server: &AlexandriaServer) -> i64 {
        let rows = server
            .db
            .inner()
            .query("SELECT count() AS c FROM claim_tick GROUP ALL;")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take::<Vec<serde_json::Value>>(0)
            .unwrap();
        rows.first().and_then(|row| row["c"].as_i64()).unwrap_or(0)
    }

    /// A server over a store with one seeded edge pair, plus a lawful scope over that same store.
    ///
    /// The capability is claimed **before** the store is shared, which is the order production uses and
    /// the only order that works: `AccessRepo::local_principal` needs this instance's capability, and a
    /// request-facing holder has the database behind an `Arc` that cannot produce it. The seeded rows are
    /// then stamped with the fixture account, because activation is partition-scoped and an
    /// migration-pending row with no owner is correctly neither a warm seed nor a warm target.
    async fn server_with_seeded_edge() -> (AlexandriaServer, (String, String), RequestScope) {
        let mut db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let system = db.claim_system_authority().unwrap();
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );
        let ids = seed_edge_pair(&server).await;
        let scope = edge_fixture_scope(&server, &system).await;
        (server, ids, scope)
    }

    /// Issues a scope over `server`'s own store through the sealed chain, and stamps the rows the
    /// fixtures seeded through the interim store path so the scoped readers can see them.
    async fn edge_fixture_scope(
        server: &AlexandriaServer,
        system: &alexandria_storage::access::SystemContext,
    ) -> RequestScope {
        let ids = alexandria_storage::repos::IdentityRepo::new(server.db.inner())
            .bootstrap_owner(
                "edge-fixture-owner",
                "$argon2id$fixture-prepared-hash-edge-fixture-owner-pad",
                &alexandria_storage::models::OperationId::new("edge-fixture-owner"),
            )
            .await
            .unwrap();
        let access = AccessRepo::new(&server.db);
        let principal = access.local_principal(system, &ids.account).await.unwrap();
        let owner = access.authorize(&principal).await.unwrap();
        let read = access
            .select(&owner, ReadSelection::Auto(None))
            .await
            .unwrap();
        // Only `fact` carries ownership columns; `heat_state` is reached through its `memory` link, so
        // stamping it would invent a second ownership model in the schema and the scoped writer filters
        // on the fact row instead.
        //
        // `scope` is the part that matters, and its absence is what made lane three believe the value
        // would not persist. `ScopeFilter::conjunct()` selects a *partition*, so a row stamped with
        // account and workspace but left at `scope = NONE` is invisible to the scoped reader that supplies
        // retrieval candidates — `trigger_activation` is then never reached at all. Probing confirmed the
        // reader is failing closed correctly (identical account+workspace: scope NONE -> not visible,
        // scope 'personal' -> visible) and that `scope` writes persist in every shape a fixture might use,
        // so this is a fixture technique fix, not a product one. See
        // `task-6-fourth-logs/01-activation-scope-diagnosis.log`.
        server
            .db
            .inner()
            .query(
                "UPDATE fact SET account = $account, workspace = $workspace, scope = 'personal' \n                 WHERE account = NONE;",
            )
            .bind(("account", ids.account.as_record_id().clone()))
            .bind(("workspace", ids.workspace.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        RequestScope::issued(owner, read)
    }

    /// The control for `test_dry_run_does_not_write_heat`: this fixture really can produce a
    /// heat write. Without it the dry assertion could pass vacuously on a fixture where nothing
    /// ever activates.
    #[tokio::test]
    async fn test_wet_run_does_write_heat() {
        let (server, ids, _scope) = server_with_seeded_edge().await;
        let before = seeded_heat(&server, &ids).await;
        assert_eq!(
            before,
            (Some(1.0), Some(1.0)),
            "each `do_store_memory` seeds heat at 1.0, so the fixture starts from a known place"
        );

        server
            .do_retrieve_memories(seeded_retrieve_params())
            .await
            .unwrap();

        let after = seeded_heat(&server, &ids).await;
        // One hop of propagation at the default factor: 1.0 * 0.3^1 * strength 1.0. Compared with
        // a tolerance because that delta is computed in f32 and widened to f64 on the way in.
        let warmed = |h: Option<f64>| h.is_some_and(|h| (h - 1.3).abs() < 1e-3);
        assert!(
            warmed(after.0) && warmed(after.1),
            "a wet retrieve must warm both endpoints of the seeded edge; before {before:?}, after {after:?}"
        );
    }

    /// The load-bearing one: the same query through the dry path leaves heat exactly where it
    /// was, even though the control test above proves it moves.
    #[tokio::test]
    async fn test_dry_run_does_not_write_heat() {
        let (server, ids, _scope) = server_with_seeded_edge().await;
        let before = seeded_heat(&server, &ids).await;

        let result = server
            .do_retrieve_memories_dry(seeded_retrieve_params())
            .await
            .unwrap();
        assert_eq!(
            result["results"].as_array().unwrap().len(),
            2,
            "the dry path must still run the retrieval, not short-circuit it"
        );
        assert_eq!(
            seeded_heat(&server, &ids).await,
            before,
            "a dry retrieve must not write heat anywhere it touches"
        );
    }

    /// Pinning the equivalence property the split depends on: dry and wet differ *only* in the
    /// side effect. Whole-`Value` comparison, so ids, contents, similarities, tags and row order
    /// are all in scope.
    #[tokio::test]
    async fn test_dry_and_wet_return_identical_results() {
        let (server, _ids, _scope) = server_with_seeded_edge().await;

        let dry = server
            .do_retrieve_memories_dry(seeded_retrieve_params())
            .await
            .unwrap();
        let wet = server
            .do_retrieve_memories(seeded_retrieve_params())
            .await
            .unwrap();
        // A second dry run *after* the wet one, to show the heat the wet run wrote does not leak
        // into what the dry path reports.
        let dry_again = server
            .do_retrieve_memories_dry(seeded_retrieve_params())
            .await
            .unwrap();

        assert_eq!(
            dry["results"].as_array().unwrap().len(),
            2,
            "an empty result set would make the comparisons below tautological"
        );
        assert_eq!(
            dry, wet,
            "dry and wet retrieval must return identical results"
        );
        assert_eq!(
            dry_again, wet,
            "a dry run after a wet one must return the same results as before it"
        );
        // The tool-facing `scope` diagnostic must stay out of every interim/debug arm. Adding it inside
        // `retrieve_core` would silently widen the debug surface's published response.
        assert!(
            dry.get("scope").is_none(),
            "the dry interim arm leaked the tool-only scope key: {dry}"
        );
        assert!(
            wet.get("scope").is_none(),
            "the wet interim arm leaked the tool-only scope key: {wet}"
        );
    }

    #[tokio::test]
    async fn list_sessions_filters_and_counts() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );

        for (sess, agent) in [("sess-l1", "pi"), ("sess-l2", "claude-code")] {
            server
                .do_store_memory(StoreMemoryParams {
                    scope: None,
                    project: None,
                    context_project: None,
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
                    view: None,
                    project: None,
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
                    view: None,
                    project: None,
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
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );

        let _kept = server
            .do_store_memory(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
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
                scope: None,
                project: None,
                context_project: None,
                content: "deleted fact".to_string(),
                tags: None,
                session_id: Some("sess-del".to_string()),
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();
        MemoryRepo::new(server.db.inner())
            .soft_delete_fact(&gone)
            .await
            .unwrap();

        let session_json = server
            .do_get_session(GetSessionParams {
                view: None,
                project: None,
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
                scope: None,
                project: None,
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
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );

        server
            .do_store_memory(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
                content: "unrelated fact outside the session".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();
        server
            .do_import_document_interim(ImportDocumentParams {
                scope: None,
                project: None,
                context_project: None,
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
                view: None,
                project: None,
                session_id: "sess-import".to_string(),
            })
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&session_json).unwrap();
        assert_eq!(parsed["memories"].as_array().unwrap().len(), 2);

        let result = server
            .do_retrieve_memories(RetrieveMemoriesParams {
                scope: None,
                project: None,
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
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );

        // Store memories with a session_id — session auto-creates
        let _id1 = server
            .do_store_memory(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
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
                scope: None,
                project: None,
                context_project: None,
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
                scope: None,
                project: None,
                context_project: None,
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
                scope: None,
                project: None,
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
                view: None,
                project: None,
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
                view: None,
                project: None,
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
                view: None,
                project: None,
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
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(DirectionalEmbedding),
            0.75,
            HeatSettings::default(),
        );
        server
            .do_store_memory(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
                content: "a near match memory".to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap();

        let result = tool_json(
            server
                .do_retrieve_memories(RetrieveMemoriesParams {
                    scope: None,
                    project: None,
                    query: "anything".to_string(),
                    limit: Some(10),
                    session_id: None,
                })
                .await
                .map(|value| value.to_string()),
        );

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

    /// A server and a resolved scope over **one** store, for the response-shape assertions.
    ///
    /// Not a test-only authority constructor: the scope is produced by the same sealed chain production
    /// uses — `bootstrap_owner`, then `local_principal` under the capability this store issued at
    /// construction, then `authorize` and `select`. The server and the scope have to come from one store
    /// because a capability is per-instance and the account rows live in exactly one of them.
    async fn shape_server_and_scope() -> (AlexandriaServer, RequestScope) {
        let mut db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let system = db.claim_system_authority().unwrap();
        let ids = alexandria_storage::repos::IdentityRepo::new(db.inner())
            .bootstrap_owner(
                "shape-owner",
                "$argon2id$fixture-prepared-hash-shape-owner-pad",
                &alexandria_storage::models::OperationId::new("shape-owner"),
            )
            .await
            .unwrap();
        let access = AccessRepo::new(&db);
        let principal = access.local_principal(&system, &ids.account).await.unwrap();
        let owner = access.authorize(&principal).await.unwrap();
        let read = access
            .select(&owner, ReadSelection::Assigned)
            .await
            .unwrap();
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );
        (server, RequestScope::issued(owner, read))
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

    /// Stub whose limit is ten characters, standing in for a tokenizer.
    struct TinyLimit;

    #[async_trait::async_trait]
    impl EmbeddingProvider for TinyLimit {
        async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            StubEmbedding.embed(texts).await
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn model_id(&self) -> &str {
            "stub"
        }
        fn overflow(&self, text: &str) -> Option<usize> {
            (text.len() > 10).then_some(text.len())
        }
    }

    /// The store response is the only machine-readable truncation signal the storing agent gets.
    #[tokio::test]
    async fn store_memory_flags_truncated_content() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(TinyLimit),
            0.75,
            HeatSettings::default(),
        );
        let store = async |content: &str| {
            let result = server
                .store_response_interim(StoreMemoryParams {
                    scope: None,
                    project: None,
                    context_project: None,
                    content: content.to_string(),
                    tags: None,
                    session_id: None,
                    agent_id: None,
                    model: None,
                })
                .await;
            result.structured_content.expect("structuredContent set")
        };

        let long = store("well past ten characters").await;
        assert_eq!(long["status"], "ok");
        assert_eq!(long["truncated"], true);
        assert_eq!(long["tokens"], 24);
        assert!(store("short").await.get("truncated").is_none());
    }

    struct OverflowProbe {
        caller: std::thread::ThreadId,
        off_thread: Arc<std::sync::atomic::AtomicBool>,
        panic_counting: bool,
        fail_embedding: bool,
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for OverflowProbe {
        async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            if self.fail_embedding {
                anyhow::bail!("Tokenization failed: invalid input");
            }
            StubEmbedding.embed(texts).await
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn model_id(&self) -> &str {
            "overflow-probe"
        }
        fn overflow(&self, text: &str) -> Option<usize> {
            self.off_thread.store(
                std::thread::current().id() != self.caller,
                std::sync::atomic::Ordering::SeqCst,
            );
            assert!(!self.panic_counting, "overflow tokenizer failed");
            (text.len() > 10).then_some(text.len())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn store_memory_counts_overflow_off_the_async_executor() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let off_thread = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(OverflowProbe {
                caller: std::thread::current().id(),
                off_thread: off_thread.clone(),
                panic_counting: false,
                fail_embedding: false,
            }),
            0.75,
            HeatSettings::default(),
        );
        let result = server
            .store_response_interim(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
                content: "well past ten characters".into(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await;
        check_structured("overflow offload", &result);
        assert!(off_thread.load(std::sync::atomic::Ordering::SeqCst));
        let body = result.structured_content.unwrap();
        assert_eq!(body["truncated"], true);
        assert_eq!(body["tokens"], 24);
    }

    #[tokio::test]
    async fn store_memory_counting_panic_is_a_structured_error_without_a_fact() {
        let db = Arc::new(Database::connect_embedded().await.unwrap());
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(
            db.clone(),
            Arc::new(OverflowProbe {
                caller: std::thread::current().id(),
                off_thread: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                panic_counting: true,
                fail_embedding: false,
            }),
            0.75,
            HeatSettings::default(),
        );
        let result = server
            .store_response_interim(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
                content: "must not be written".into(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await;
        assert_eq!(result.is_error, Some(true));
        let body = result.structured_content.unwrap();
        assert_eq!(body["status"], "error");
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("overflow counting")
        );
        assert!(body.get("truncated").is_none());
        assert_eq!(
            MemoryRepo::new(db.inner())
                .count(&FactListQuery::default())
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn store_memory_tokenization_error_is_not_reported_as_successful_truncation() {
        let db = Arc::new(Database::connect_embedded().await.unwrap());
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(
            db.clone(),
            Arc::new(OverflowProbe {
                caller: std::thread::current().id(),
                off_thread: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                panic_counting: false,
                fail_embedding: true,
            }),
            0.75,
            HeatSettings::default(),
        );
        let result = server
            .store_response_interim(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
                content: "will fail tokenization".into(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await;
        assert_eq!(result.is_error, Some(true));
        let body = result.structured_content.unwrap();
        assert_eq!(body["status"], "error");
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("Tokenization failed")
        );
        assert!(body.get("truncated").is_none());
        assert_eq!(
            MemoryRepo::new(db.inner())
                .count(&FactListQuery::default())
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn every_tool_returns_structured_content_matching_text() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );

        let stored = server
            .store_response_interim(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
                content: "the project uses SurrealDB".to_string(),
                tags: Some(vec!["db".to_string()]),
                session_id: Some("sess-struct".to_string()),
                agent_id: None,
                model: None,
            })
            .await;
        check_structured("store_memory", &stored);

        check_structured(
            "recall",
            &tool_json(
                server
                    .do_recall(RecallParams {
                        scope: None,
                        project: None,
                        query: "database".to_string(),
                        scope_handle: None,
                    })
                    .await,
            ),
        );
        // The update arm needs a row the caller actually owns, because `update_within` refuses anything
        // outside the resolved population — that refusal is the point of the scoped path. The store above
        // still goes through the interim unscoped service (it has no partition target), so this arm creates
        // its own owned row through the scoped repo and updates that.
        let (update_server, update_scope) = shape_server_and_scope().await;
        let owned = alexandria_storage::repos::ScopedMemoryRepo::new(
            update_server.db.inner(),
            update_scope.owner(),
            update_scope.read(),
        )
        .create_fact(
            "the project uses SurrealDB",
            0.5,
            &[1.0, 0.0],
            &["db".to_string()],
            alexandria_storage::repos::WriteTarget::Personal,
            Utc::now(),
        )
        .await
        .unwrap();
        let owned_id = record_id_to_string(owned.id.as_ref().expect("a created fact has an id"));
        check_structured(
            "update_memory",
            &tool_json(
                update_server
                    .update_within(
                        &update_scope,
                        UpdateMemoryParams {
                            id: owned_id,
                            content: Some("the project uses SurrealDB 3.2".to_string()),
                            tags: None,
                            confidence: None,
                        },
                    )
                    .await,
            ),
        );
        check_structured(
            "import_document",
            &server
                .import_response(ImportDocumentParams {
                    scope: None,
                    project: None,
                    context_project: None,
                    content: "first paragraph\n\nsecond paragraph".to_string(),
                    mode: None,
                    chunk_strategy: Some("paragraph".to_string()),
                    tags: None,
                    session_id: Some("sess-struct".to_string()),
                    agent_id: None,
                    model: None,
                })
                .await,
        );
        // The three session tools now resolve an admitted principal in the handler, and a unit test
        // cannot build a `RequestContext` (rmcp's `Peer::new` is `pub(crate)`), so their **payload shape**
        // is asserted over the real transport in `scoped_request_tools_test.rs`. What stays here is the
        // service method the handler delegates to below admission, which is the code that produces the
        // structuredContent/text pairing this test exists to pin.
        check_structured(
            "get_session",
            &tool_json(
                server
                    .do_get_session(GetSessionParams {
                        view: None,
                        project: None,
                        session_id: "sess-struct".to_string(),
                    })
                    .await,
            ),
        );
        check_structured(
            "list_sessions",
            &tool_json(
                server
                    .do_list_sessions(ListSessionsParams {
                        view: None,
                        project: None,
                        agent_id: None,
                        tag: None,
                        finalized: None,
                        limit: None,
                        offset: None,
                    })
                    .await,
            ),
        );
        check_structured(
            "finalize_session",
            &tool_json(
                server
                    .do_finalize_session(FinalizeSessionParams {
                        session_id: "sess-struct".to_string(),
                        summary: Some("structured-content exercise".to_string()),
                        tags: None,
                    })
                    .await,
            ),
        );
        // The delete tool's response shape, asserted without a live `Peer` (only a running rmcp service
        // builds one): admission is covered by the transport suite. The server and scope come from one
        // store, because a scope issued against a different instance names rows that store does not have.
        // Both arms are checked — a delete that succeeds, and the same id refused once the row is gone —
        // because the contract is that *either* mirrors its JSON into the text block. The row has to be one
        // this scope owns; `id` above came from the interim unscoped store path and carries no owner, which
        // the scoped delete correctly refuses.
        let (shape_server, shape_scope) = shape_server_and_scope().await;
        let forgettable = alexandria_storage::repos::ScopedMemoryRepo::new(
            shape_server.db.inner(),
            shape_scope.owner(),
            shape_scope.read(),
        )
        .create_fact(
            "a row this caller owns",
            0.5,
            &[1.0, 0.0],
            &[],
            alexandria_storage::repos::WriteTarget::Personal,
            Utc::now(),
        )
        .await
        .unwrap();
        let forgettable_id =
            record_id_to_string(forgettable.id.as_ref().expect("a created fact has an id"));
        check_structured(
            "delete_memory",
            &delete_result(
                shape_server
                    .delete_within(&shape_scope, &forgettable_id)
                    .await,
            ),
        );
        // A second delete of one's own row is idempotent rather than an error: the row is already
        // superseded and the caller's intent is satisfied. What must not be distinguishable is the *other*
        // axis — a row that is somebody else's and a row that never existed both answer `NotFound`, which
        // replaces the old unscoped `{"status":"ok"}` for a foreign id.
        let repeated = delete_result(
            shape_server
                .delete_within(&shape_scope, &forgettable_id)
                .await,
        );
        assert_ne!(
            repeated.is_error,
            Some(true),
            "re-delete must stay a success"
        );
        let absent = delete_result(
            shape_server
                .delete_within(&shape_scope, "fact:never-existed-at-all")
                .await,
        );
        assert_eq!(
            absent.is_error,
            Some(true),
            "an absent row must not report success"
        );
        // The four reminder tools were converted to `tool_json` after this test
        // was written, so they are the ones most likely to drift back — every
        // `do_*` here returns a JSON string by construction and must keep
        // mirroring it into structuredContent.
        let set = tool_json(
            server
                .do_set_reminder(SetReminderParams {
                    message: "water the plants".to_string(),
                    due_at: Some("2030-01-01T12:00:00Z".to_string()),
                    pattern: None,
                    cron: None,
                    target_project: Some("struct".to_string()),
                    prov_project: None,
                    session_id: None,
                    note: None,
                })
                .await,
        );
        check_structured("set_reminder", &set);
        let reminder_id = set.structured_content.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        check_structured(
            "check_reminders",
            &tool_json(
                server
                    .do_check_reminders(CheckRemindersParams {
                        project: Some("struct".to_string()),
                    })
                    .await,
            ),
        );
        check_structured(
            "list_reminders",
            &tool_json(
                server
                    .do_list_reminders(ListRemindersParams {
                        status: None,
                        target_project: None,
                        limit: None,
                        offset: None,
                    })
                    .await,
            ),
        );
        check_structured(
            "cancel_reminder",
            &tool_json(
                server
                    .do_cancel_reminder(CancelReminderParams { id: reminder_id })
                    .await,
            ),
        );
    }

    #[tokio::test]
    async fn tool_errors_are_flagged_structured() {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let server = AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        );

        // An update against a row that is not the caller's must be a structured error. The id is
        // deliberately **not** echoed any more: `update_within` answers `NotFound`, the same bytes a
        // missing id produces, so a tool response cannot be used to ask whether some other account holds
        // the row. This case used to assert the id appeared in the message, and that older expectation was
        // the leak's own shape — the reconciliation is to the class, not to a weaker check. Chain
        // flattening is still proven below by the bad-cron arm, which carries a real cause.
        let (shape_server, shape_scope) = shape_server_and_scope().await;
        let result = tool_json(
            shape_server
                .update_within(
                    &shape_scope,
                    UpdateMemoryParams {
                        id: "fact:does-not-exist".to_string(),
                        content: Some("x".to_string()),
                        tags: None,
                        confidence: None,
                    },
                )
                .await,
        );
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structuredContent set");
        assert_eq!(structured["status"], "error");
        assert_eq!(
            structured["message"].as_str().unwrap(),
            "not found",
            "a foreign or absent id must answer with the sanitized class"
        );

        // The error arm flattens the whole anyhow chain, not just the top-level
        // context: reminder validation wraps the parser's reason, and
        // `to_string()` would drop it.
        let bad_cron = tool_json(
            server
                .do_set_reminder(SetReminderParams {
                    message: "x".to_string(),
                    due_at: None,
                    pattern: None,
                    cron: Some("61 99 * * *".to_string()),
                    target_project: None,
                    prov_project: None,
                    session_id: None,
                    note: None,
                })
                .await,
        );
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

    // --- retrieve-path access recording (GitHub issue #43) --------------------------------

    /// A migrated in-memory server with the stub embedding. Retrieval tests need the store and the
    /// read side, not a particular provider.
    async fn heat_server() -> AlexandriaServer {
        let db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        AlexandriaServer::new(
            Arc::new(db),
            Arc::new(StubEmbedding),
            0.75,
            HeatSettings::default(),
        )
    }

    async fn store_one(server: &AlexandriaServer, content: &str) -> String {
        server
            .do_store_memory(StoreMemoryParams {
                scope: None,
                project: None,
                context_project: None,
                content: content.to_string(),
                tags: None,
                session_id: None,
                agent_id: None,
                model: None,
            })
            .await
            .unwrap()
    }

    /// Every heat row in the store. Reading the whole table rather than the row under test is what
    /// makes the boundary assertions mean something: a leak onto a row nobody saw would otherwise
    /// be invisible. Ids are formatted with `record_id_to_string` on both sides of every
    /// comparison, because the storage layer returns `to_sql()` strings and the tool response
    /// returns `record_id_to_string` ones.
    async fn all_heat(server: &AlexandriaServer) -> Vec<alexandria_storage::models::HeatState> {
        let mut response = server
            .db
            .inner()
            .query("SELECT * FROM heat_state")
            .await
            .unwrap();
        response.take(0).unwrap()
    }

    /// `StubEmbedding` gives every fact the same vector, so which rows survive `limit` is the
    /// engine's business. The assertions therefore read the returned ids out of the response
    /// instead of assuming an order.
    #[tokio::test]
    async fn retrieve_records_an_access_on_every_returned_row_and_only_those() {
        let server = heat_server().await;
        for n in 0..3 {
            store_one(&server, &format!("candidate row {n}")).await;
        }

        let out = server
            .do_retrieve_memories(RetrieveMemoriesParams {
                scope: None,
                project: None,
                query: "candidate row".to_string(),
                limit: Some(1),
                session_id: None,
            })
            .await
            .unwrap();
        let ids: Vec<String> = out["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), 1, "limit 1 must return exactly one row");

        let counted: Vec<String> = all_heat(&server)
            .await
            .into_iter()
            .filter(|row| row.access_count == 1)
            .map(|row| record_id_to_string(&row.memory))
            .collect();
        assert_eq!(
            counted, ids,
            "exactly the rows the caller saw must be counted, and no others — access_count = 0 is \
             what Appraise demotes on"
        );
        let surfaced = all_heat(&server)
            .await
            .into_iter()
            .find(|row| record_id_to_string(&row.memory) == ids[0])
            .expect("the returned row has a heat_state");
        assert_eq!(surfaced.heat, 1.0, "an access resets heat");
        assert!(surfaced.last_accessed_at.is_some());
    }

    /// The end-to-end half of the clock split. The decay anchor is left at now (what a swept row
    /// looks like) while the access stamp is years old, so full spacing credit is only reachable if
    /// `on_access` reads the stamp. Measuring growth from the anchor instead would leave stability
    /// at ~1.04 and still pass every other test in this file.
    #[tokio::test]
    async fn access_growth_reads_the_access_stamp_not_the_decay_anchor() {
        let server = heat_server().await;
        let id = store_one(&server, "spaced repetition growth").await;

        server
            .db
            .inner()
            .query(
                "UPDATE heat_state SET last_accessed_at = type::datetime('2020-01-01T00:00:00Z') \
                 WHERE memory = type::record($mid)",
            )
            .bind(("mid", id.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();

        let result = server
            .do_retrieve_memories(RetrieveMemoriesParams {
                scope: None,
                project: None,
                query: "spaced repetition".to_string(),
                limit: Some(5),
                session_id: None,
            })
            .await
            .unwrap();
        // The retrieve must actually have surfaced the row it is about to be credited with, or the heat
        // assertion below would be satisfied by an access that never happened.
        assert!(
            result["results"]
                .as_array()
                .is_some_and(|hits| !hits.is_empty()),
            "the wet retrieve returned no results, so it proves nothing about heat"
        );

        let row = HeatRepo::new(server.db.inner())
            .get(&id)
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(
            row.stability, 2.0,
            "a six-year gap must saturate the spacing clamp and add a full 1.0"
        );
        assert_eq!(row.access_count, 1);
    }

    /// The debug UI's read-only claim depends on this, so the dry assertions are paired with a
    /// positive control on the same fixture: without the second half, "nothing was written" would
    /// also be satisfied by a server that could not write at all.
    #[tokio::test]
    async fn dry_run_writes_no_access_and_the_tool_path_is_the_positive_control() {
        let server = heat_server().await;
        let id = store_one(&server, "dry run boundary").await;
        let heat = HeatRepo::new(server.db.inner());
        let params = || RetrieveMemoriesParams {
            scope: None,
            project: None,
            query: "dry run".to_string(),
            limit: Some(5),
            session_id: None,
        };

        server.do_retrieve_memories_dry(params()).await.unwrap();
        let after_dry = heat.get(&id).await.unwrap().expect("row exists");
        assert_eq!(
            after_dry.access_count, 0,
            "a diagnostic query must not count as an access"
        );
        assert!(after_dry.last_accessed_at.is_none());

        server.do_retrieve_memories(params()).await.unwrap();
        let after_tool = heat.get(&id).await.unwrap().expect("row exists");
        assert_eq!(
            after_tool.access_count, 1,
            "positive control: the same row and the same fixture do change on the tool path"
        );
        assert!(after_tool.last_accessed_at.is_some());
    }

    /// Imported chunks and anything written through `MemoryRepo::create_fact` have no `heat_state`
    /// row. Skipping those would leave `access_count` at 0 forever for exactly the rows that are
    /// already poorly covered, and `Appraise` reads 0 as "never surfaced".
    /// A quarantined fact must be unreachable through the tools an agent actually calls, not just
    /// through the storage queries behind them — hiding it in `knn_sql` while `do_recall` still
    /// walked cluster members would leak it back. Both retrieval tools are asserted, and the
    /// positive control is the same fixture before quarantine is set.
    #[tokio::test]
    async fn quarantined_facts_are_unreachable_through_retrieve_and_recall() {
        let server = heat_server().await;
        let id = store_one(&server, "the api token rotates nightly").await;

        let params = || RetrieveMemoriesParams {
            scope: None,
            project: None,
            query: "api token".to_string(),
            limit: Some(5),
            session_id: None,
        };
        let before = server.do_retrieve_memories(params()).await.unwrap();
        assert!(
            before.to_string().contains(&id),
            "positive control: the fact is retrievable before quarantine: {before}"
        );

        server
            .db
            .inner()
            .query("UPDATE type::record($id) SET quarantined_at = time::now()")
            .bind(("id", id.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();

        // Checked as serialized text rather than a parsed shape: `do_retrieve_memories` returns a
        // `serde_json::Value` and `do_recall` returns a `String`, and the property under test is
        // "this id does not appear", which does not depend on either envelope.
        let checks: Vec<(&str, String)> = vec![
            (
                "retrieve",
                server
                    .do_retrieve_memories(params())
                    .await
                    .unwrap()
                    .to_string(),
            ),
            (
                "dry retrieve",
                server
                    .do_retrieve_memories_dry(params())
                    .await
                    .unwrap()
                    .to_string(),
            ),
            (
                "unfiltered retrieve",
                server
                    .do_retrieve_memories_unfiltered(params())
                    .await
                    .unwrap()
                    .to_string(),
            ),
            (
                "recall",
                server
                    .do_recall(RecallParams {
                        scope: None,
                        project: None,
                        query: "api token".to_string(),
                        scope_handle: None,
                    })
                    .await
                    .unwrap(),
            ),
        ];
        for (label, text) in checks {
            assert!(
                !text.contains(&id),
                "{label} must not surface a quarantined fact: {text}"
            );
        }
    }

    #[tokio::test]
    async fn a_returned_row_with_no_heat_state_is_created_and_counted() {
        let server = heat_server().await;
        let id = MemoryRepo::new(server.db.inner())
            .create_fact("chunk with no heat row", 0.5, &[0.1_f32, 0.2], &[])
            .await
            .unwrap();
        assert!(
            HeatRepo::new(server.db.inner())
                .get(&id)
                .await
                .unwrap()
                .is_none(),
            "fixture control: this row must start with no heat_state"
        );

        server
            .do_retrieve_memories(RetrieveMemoriesParams {
                scope: None,
                project: None,
                query: "chunk".to_string(),
                limit: Some(5),
                session_id: None,
            })
            .await
            .unwrap();

        let row = HeatRepo::new(server.db.inner())
            .get(&id)
            .await
            .unwrap()
            .expect("surfacing the row must create and count it");
        assert_eq!(row.access_count, 1);
        assert!(row.last_accessed_at.is_some());
        assert_eq!(
            row.stability, 1.0,
            "a row created at this instant earns no spacing credit"
        );
    }
}
