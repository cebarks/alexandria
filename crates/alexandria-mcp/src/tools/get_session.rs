#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct GetSessionParams {
    #[schemars(description = "The session ID to retrieve")]
    pub session_id: String,
}
