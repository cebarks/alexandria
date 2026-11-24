#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ListSessionsParams {
    #[schemars(
        description = "'scoped' (default) omits the account-private summary/tag prose and counts only the memories your selected scope can read; 'archive' is the deliberate whole-session read that includes that prose across your owned scopes. Tag filtering is archive-only, because it searches that prose."
    )]
    pub view: Option<String>,
    #[schemars(
        description = "The project context to enumerate sessions for (opaque ID or established alias). With it, the scoped view filters membership and counts to that project."
    )]
    pub project: Option<String>,
    #[schemars(description = "Only sessions recorded by this agent (e.g. 'claude-code', 'pi')")]
    pub agent_id: Option<String>,
    #[schemars(
        description = "Only sessions carrying this tag. Requires `view: \"archive\"`: the tag set is account-private prose that the default scoped page withholds, and a filter the caller cannot see the values for would silently return nothing."
    )]
    pub tag: Option<String>,
    #[schemars(
        description = "true: only sessions closed with finalize_session (have a summary); false: only still-open sessions; omit for both"
    )]
    pub finalized: Option<bool>,
    #[schemars(description = "Maximum sessions to return, newest first (default 20)")]
    pub limit: Option<u32>,
    #[schemars(description = "Number of sessions to skip, for paging (default 0)")]
    pub offset: Option<u32>,
}
