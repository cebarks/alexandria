#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct StoreMemoryParams {
    #[schemars(
        description = "The fact/decision/preference to store, written as a standalone statement that still makes sense without today's conversation"
    )]
    pub content: String,
    #[schemars(
        description = "Where this memory belongs: 'personal' (yours, recallable in every project) or 'project' (visible to this project only). Required for a hosted request unless `context_project` names the project you are working in; there is no silent default, and a refused store embeds and writes nothing."
    )]
    pub scope: Option<String>,
    #[schemars(
        description = "Destination project for scope 'project': an opaque project ID ('project:...') or a workspace-qualified alias this server already established for you. Overrides `context_project`. A name that does not resolve in your workspace is an error, never a fallback to personal and never a project created behind your back."
    )]
    pub project: Option<String>,
    #[schemars(
        description = "The project context you resolved for this session (the ID `resolve_project` returned). Used when `scope` is omitted, so a project-scoped agent can store without naming a destination every time; ignored when `scope` is 'personal'."
    )]
    pub context_project: Option<String>,
    #[schemars(description = "Optional tags for categorization/filtering later")]
    pub tags: Option<Vec<String>>,
    #[schemars(
        description = "Optional session ID to group this memory into a session context. Auto-creates the session on first use."
    )]
    pub session_id: Option<String>,
    #[schemars(
        description = "Optional identifier of the agent or harness storing this (e.g. 'claude-code', 'pi'). Only meaningful with session_id; the first non-null value wins whenever it arrives and is never overwritten."
    )]
    pub agent_id: Option<String>,
    #[schemars(
        description = "Optional model name of the agent storing this (e.g. 'claude-sonnet-5'). Only meaningful with session_id; the first non-null value wins whenever it arrives and is never overwritten."
    )]
    pub model: Option<String>,
}
