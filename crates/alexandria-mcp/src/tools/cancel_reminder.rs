#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct CancelReminderParams {
    #[schemars(description = "Reminder ID, e.g. 'reminder:abc123' (from set/list output)")]
    pub id: String,
}
