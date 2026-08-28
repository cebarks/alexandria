#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct StoreMemoryParams {
    #[schemars(
        description = "The fact/decision/preference to store, written as a standalone statement that still makes sense without today's conversation"
    )]
    pub content: String,
    #[schemars(description = "Optional tags for categorization/filtering later")]
    pub tags: Option<Vec<String>>,
    #[schemars(
        description = "Optional session ID to group this memory into a session context. Auto-creates the session on first use."
    )]
    pub session_id: Option<String>,
}
