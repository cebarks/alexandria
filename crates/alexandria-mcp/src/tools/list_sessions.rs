#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ListSessionsParams {
    #[schemars(description = "Only sessions recorded by this agent (e.g. 'claude-code', 'pi')")]
    pub agent_id: Option<String>,
    #[schemars(description = "Only sessions carrying this tag")]
    pub tag: Option<String>,
    #[schemars(
        description = "true: only sessions closed with finalize_session (have a summary); false: only still-open sessions; omit for both"
    )]
    pub finalized: Option<bool>,
    #[schemars(description = "Maximum sessions to return, newest first (default 20)")]
    pub limit: Option<u32>,
    #[schemars(description = "Number of sessions to skip, for paging (default 0)")]
    pub offset: Option<u32>,
}
