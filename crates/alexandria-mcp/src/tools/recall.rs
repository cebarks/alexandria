#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct RecallParams {
    #[schemars(description = "What you're trying to remember or explore, in natural language")]
    pub query: String,
    #[schemars(
        description = "Which of your partitions to explore: 'auto' (default), 'personal', 'project' or 'assigned' — same meaning as in retrieve_memories, and the same population predicate: a cluster contributes only members inside it, and a cluster with none is not offered or replayable. Cluster *structure* is still maintained per workspace (Task 12 partitions it), so a mixed cluster can answer a personal read with its personal members; the response's `scope` says which selection ran."
    )]
    pub scope: Option<String>,
    #[schemars(
        description = "The project you are working in (opaque ID or established alias); with 'auto' the search covers personal plus that project."
    )]
    pub project: Option<String>,
    #[schemars(
        description = "Omit for a broad first pass across clusters; pass the scope_handle returned by that broad call to narrow into one cluster"
    )]
    pub scope_handle: Option<String>,
}
