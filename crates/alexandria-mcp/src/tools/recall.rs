#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct RecallParams {
    #[schemars(description = "What you're trying to remember or explore, in natural language")]
    pub query: String,
    #[schemars(
        description = "Omit for a broad first pass across clusters; pass the scope_handle returned by that broad call to narrow into one cluster"
    )]
    pub scope_handle: Option<String>,
}
