#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct FinalizeSessionParams {
    #[schemars(description = "The session ID to finalize")]
    pub session_id: String,
    #[schemars(description = "Optional summary of what happened in the session")]
    pub summary: Option<String>,
    #[schemars(description = "Optional tags for the session")]
    pub tags: Option<Vec<String>>,
}
