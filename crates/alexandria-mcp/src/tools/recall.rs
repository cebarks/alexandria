#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct RecallParams {
    #[schemars(description = "What you're trying to remember")]
    pub query: String,
    #[schemars(description = "Scope handle from a previous recall to narrow within")]
    pub scope_handle: Option<String>,
}
