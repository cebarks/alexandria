#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct DeleteMemoryParams {
    #[schemars(description = "The memory record ID to soft-delete")]
    pub id: String,
}
