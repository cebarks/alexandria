#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ListRemindersParams {
    #[schemars(
        description = "Filter by status: 'pending' (default), 'delivered', 'cancelled', or 'all'. Ordered oldest-due first"
    )]
    pub status: Option<String>,
    #[schemars(description = "Optional: only reminders targeting this project")]
    pub target_project: Option<String>,
    #[schemars(
        description = "Optional page size, 1-500 (default 50). Ordered oldest-due first; the response carries 'truncated' and 'next_offset' when more rows exist."
    )]
    pub limit: Option<i64>,
    #[schemars(
        description = "Optional rows to skip, for paging with 'limit' (default 0). Pass back the 'next_offset' the previous page returned."
    )]
    pub offset: Option<i64>,
}
