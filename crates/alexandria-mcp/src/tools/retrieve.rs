#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct RetrieveMemoriesParams {
    #[schemars(description = "Natural language query")]
    pub query: String,
    #[schemars(description = "Maximum results to return")]
    pub limit: Option<usize>,
}
