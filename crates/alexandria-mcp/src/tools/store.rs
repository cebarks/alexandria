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
    #[schemars(
        description = "Optional identifier of the agent or harness storing this (e.g. 'claude-code', 'pi'). Only meaningful with session_id; the first non-null value wins whenever it arrives and is never overwritten."
    )]
    pub agent_id: Option<String>,
    #[schemars(
        description = "Optional model name of the agent storing this (e.g. 'claude-sonnet-5'). Only meaningful with session_id; the first non-null value wins whenever it arrives and is never overwritten."
    )]
    pub model: Option<String>,
}
