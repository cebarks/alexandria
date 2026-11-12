#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct GetSessionParams {
    #[schemars(description = "The session ID to retrieve")]
    pub session_id: String,
    #[schemars(
        description = "'scoped' (default) answers with the session's memories inside the scope you asked for and withholds the account-private summary and tags; 'archive' is the deliberate whole-session read of your own history, prose included. Passing a session id is not archive access."
    )]
    pub view: Option<String>,
    #[schemars(
        description = "Restrict the scoped view to one project (opaque ID or established alias). Ignored by 'archive', which spans your owned scopes by design."
    )]
    pub project: Option<String>,
}
