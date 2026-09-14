#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ImportDocumentParams {
    #[schemars(description = "The document text to import")]
    pub content: String,
    #[schemars(
        description = "Import mode: 'whole' (single memory) or 'chunk' (split into parts). Default: chunk"
    )]
    pub mode: Option<String>,
    #[schemars(
        description = "Chunking strategy: 'heading', 'paragraph', or 'fixed_size'. Default: heading"
    )]
    pub chunk_strategy: Option<String>,
    #[schemars(description = "Tags applied to all resulting memories")]
    pub tags: Option<Vec<String>>,
    #[schemars(
        description = "Optional session ID to group the imported chunks into a session context. Auto-creates the session on first use."
    )]
    pub session_id: Option<String>,
}
