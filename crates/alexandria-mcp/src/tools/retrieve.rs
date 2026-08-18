#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct RetrieveMemoriesParams {
    #[schemars(description = "Natural language description of what you're looking for — phrase it as the fact/decision/preference itself, not a question")]
    pub query: String,
    #[schemars(description = "Maximum results to return (default 10)")]
    pub limit: Option<usize>,
}
