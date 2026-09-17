#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct CheckRemindersParams {
    #[schemars(
        description = "Current project name (e.g. git repo dir name). Project-targeted reminders matching this are delivered; global and escalated reminders are delivered regardless."
    )]
    pub project: Option<String>,
}
