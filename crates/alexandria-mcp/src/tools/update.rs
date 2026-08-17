#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct UpdateMemoryParams {
    #[schemars(description = "The memory record ID to update")]
    pub id: String,
    #[schemars(description = "New content text (triggers re-embedding if changed)")]
    pub content: Option<String>,
    #[schemars(description = "Replace tags")]
    pub tags: Option<Vec<String>>,
    #[schemars(description = "Override confidence score")]
    pub confidence: Option<f64>,
}
