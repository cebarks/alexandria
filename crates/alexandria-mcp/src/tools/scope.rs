//! Task 8 — the project-identity and scope-review tool surface.
//!
//! Four tools, and each exists to answer a question an agent cannot answer for itself: *which project
//! is this*, *what projects do I have*, *what did I fail to classify*, and *may this memory belong
//! somewhere else*. None of them takes an account, an owner or a workspace: the admitted principal the
//! transport resolved is the only authority any of them act under, and every one of them answers
//! through the caller's own workspace-qualified rows.
//!
//! The shapes here are wire shapes only. Normalization lives in
//! [`alexandria_engine::project_identity`], lookup and creation in storage's `ScopedProjectRepo`, and
//! the policy that joins them — including the one rule that a *proposal* never moves a memory — in
//! [`crate::scope_service`].

/// The sanitized descriptor a client sends when it wants the server to identify a repository.
///
/// Flat, like every other tool parameter, with `transport` as a closed string rather than an enum a
/// client has to guess the spelling of. There is deliberately no credential, query or fragment field:
/// the engine refuses those *inside* `host` or `path`, so a client cannot smuggle one by naming a field
/// for it, and a client that has a token in its origin remote strips it before sending (the design puts
/// that stripping client-side precisely so no secret reaches a workspace-unique index).
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ResolveProjectParams {
    #[schemars(
        description = "An opaque project ID this server issued ('project:...') or a workspace-qualified alias you have used before. Mutually exclusive with `remote` — supply exactly one of the two. A name that does not resolve in your workspace is created by this call; a project ID that is not yours is refused without saying whether it exists."
    )]
    pub project: Option<String>,
    #[schemars(
        description = "The repository's sanitized origin remote. Supply exactly one of this or `project`."
    )]
    pub remote: Option<RemoteDescriptor>,
    #[schemars(
        description = "Optional human label to store with the project the first time it is established (e.g. 'Alexandria'). Never used to look anything up, and never overwrites a name an earlier call set."
    )]
    pub display_name: Option<String>,
}

/// One sanitized repository remote, as the client observed it.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct RemoteDescriptor {
    #[schemars(
        description = "Bare hostname, lowercase or not (the server folds it): 'github.com'. No scheme, no user, no port, no path."
    )]
    pub host: String,
    #[schemars(
        description = "Port, only when it is not the transport's own default. 443 over https and 22 over ssh are treated as 'no port'; any other port is part of the identity, so host:8443 is a different server than host."
    )]
    pub port: Option<u16>,
    #[schemars(
        description = "Repository path within the host, leading slash optional, one trailing '.git' ignored: 'anten/alexandria' from either git@github.com:anten/alexandria.git or https://github.com/anten/alexandria. Case in this path is significant."
    )]
    pub path: String,
    #[schemars(
        description = "How the remote is reached: 'https' or 'ssh'. A label only — it is not part of the identity, so switching a working clone between transports does not fork your project."
    )]
    pub transport: String,
}

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ListProjectsParams {
    #[schemars(
        description = "Opaque continuation from the previous page's `next_cursor`. Omit for the first page. Never interpret or build one: it is a keyset position in your own alias ordering, so a value you invented is answered as a legitimate position past the end (an empty last page), not as a malformed one — nothing here validates a cursor's shape, because an alias is free text and any string can be where a walk resumes."
    )]
    pub cursor: Option<String>,
    #[schemars(
        description = "Projects to return, in alphabetical alias order (default 20, capped at 100 — a larger request is clamped, not refused). The response carries `next_cursor` and `truncated` when more exist."
    )]
    pub limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ListUnassignedMemoriesParams {
    #[schemars(
        description = "Opaque continuation from the previous page's `next_cursor`. Omit for the first page."
    )]
    pub cursor: Option<String>,
    #[schemars(
        description = "Memories to return, 1-100 (default 20). This is owner-review state: these memories are in no recall path and no background maintenance, and reading them here does not change that."
    )]
    pub limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ProposeMemoryScopeParams {
    #[schemars(
        description = "1-20 proposals, each naming one memory, the revision you read it at, and where you think it belongs. Nothing is moved by this call: a proposal is a durable, pending judgement that the owner (or a human reviewing in the UI) decides on, and a memory that has moved since you read it is refused as stale."
    )]
    pub proposals: Vec<ScopeProposalInput>,
}

/// One entry of [`ProposeMemoryScopeParams`].
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ScopeProposalInput {
    #[schemars(
        description = "The memory's ID, e.g. 'fact:abc123' (from retrieve_memories/store output)"
    )]
    pub id: String,
    #[schemars(
        description = "The `revision` you read that memory at. A proposal whose expectation no longer matches the stored memory is refused rather than recorded against the wrong state."
    )]
    pub expected_revision: u64,
    #[schemars(
        description = "Where you believe the memory belongs: 'personal', 'project' (with `project` supplied), or 'unassigned'. A label of intent — the owner decides."
    )]
    pub destination: String,
    #[schemars(
        description = "Destination project: an opaque project ID or a workspace-qualified alias you have established. Required for destination 'project', refused for any other destination."
    )]
    pub project: Option<String>,
    #[schemars(
        description = "Optional short reason shown to whoever reviews the proposal — say why, in one clause, not 'it seems wrong'"
    )]
    pub reason: Option<String>,
}
