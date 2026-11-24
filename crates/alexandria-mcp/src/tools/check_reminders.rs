#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct CheckRemindersParams {
    #[schemars(
        description = "Optional current owned project ID (project:<opaque-id>), copy the canonical record text returned by the project tools exactly, including quoting/escaping, never a project name or checkout basename. Unknown or foreign handles are refused even when no reminders are due. Matching typed targets, global reminders, and escalated valid targets are delivered only within your account. Omit when no owned project context is known; held or unresolved legacy targets are never delivered."
    )]
    pub project: Option<String>,
}
