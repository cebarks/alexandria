#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct RetrieveMemoriesParams {
    #[schemars(
        description = "Natural language description of what you're looking for — phrase it as the fact/decision/preference itself, not a question"
    )]
    pub query: String,
    #[schemars(
        description = "Which of your partitions to search: 'auto' (default — personal, plus `project` when you name one), 'personal' (yours only), 'project' (exactly the named project), 'assigned' (personal plus every one of your projects). Never 'unassigned': those memories are owner-review state and out of every recall path by design."
    )]
    pub scope: Option<String>,
    #[schemars(
        description = "The project you are working in, as an opaque project ID or a workspace-qualified alias. With the default 'auto' this widens the search to personal plus that project; a name that is not yours is refused without saying whether it exists."
    )]
    pub project: Option<String>,
    #[schemars(description = "Maximum results to return (default 10, clamped to 1-100)")]
    pub limit: Option<usize>,
    #[schemars(
        description = "Optional session ID to restrict search to memories from a specific session"
    )]
    pub session_id: Option<String>,
}
