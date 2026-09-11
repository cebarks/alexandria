#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ListRemindersParams {
    #[schemars(
        description = "Filter by status: 'pending' (default), 'delivered', 'cancelled', or 'all'"
    )]
    pub status: Option<String>,
    #[schemars(description = "Optional: only reminders targeting this project")]
    pub target_project: Option<String>,
}
