#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct StoreMemoryParams {
    #[schemars(description = "The memory text to store")]
    pub content: String,
    #[schemars(description = "Optional tags for categorization")]
    pub tags: Option<Vec<String>>,
}
