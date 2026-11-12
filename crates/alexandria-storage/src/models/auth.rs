//! Account, workspace, project, credential and enrollment shapes, and the closed string sets v009
//! asserts over them.
//!
//! These are *row* shapes: what a `SELECT` gives back, with the record ids left as the raw
//! [`RecordId`] the engine returned. The typed handles in [`crate::models::identity`] are how a
//! repository hands one of those ids to a caller, and getting from here to there is
//! [`TryFrom<RecordId>`] — which re-checks the table. No struct in this module derives a
//! serialisation path for a typed id, because a derived one would delegate to [`RecordId`] and accept
//! any table, which is the check the newtype exists to make.
//!
//! [`PrincipalIds`] is what enrollment returns: two ids, freshly created and carrying no authority.
//! A caller that wants to *read* with them still has to authenticate a credential and go through
//! [`crate::access::AccessRepo::authorize`], which re-derives the workspace from the database rather
//! than trusting the pair it was handed.
//!
//! The allowlist modules name the same strings the schema asserts, mirroring the
//! [`crate::models::schedule_kind`] pattern. The drift gate is
//! `test_v009_closed_sets_match_the_storage_constants` in `tests/migration_test.rs`: it rebuilds each
//! `ASSERT` list from these constants and looks for that text in the compiled-in v009 file, so a
//! rename on either side fails there rather than leaving a writer producing rows the engine refuses.

use crate::error::StorageError;
use crate::models::identity::{AccountId, CredentialId, ProjectId, WorkspaceId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

/// `account.status`. Suspension stops reads, writes and scheduled work; it never deletes. The value a
/// legacy row reads is `NONE`, not one of these, and readers must treat absent as "not yet an account
/// anyone may act as" rather than as active.
pub mod account_status {
    pub const ACTIVE: &str = "active";
    pub const SUSPENDED: &str = "suspended";
}

/// `account.role`. `admin` is an *account-management* role and nothing more: it grants no read of any
/// other workspace, which is what `AccessRepo::select` demonstrates instead of trusting the name.
pub mod account_role {
    pub const OWNER: &str = "owner";
    pub const ADMIN: &str = "admin";
}

/// `credential.kind` — one identity table with a kind discriminator, because revocation names a
/// credential id without asking which kind it was.
pub mod credential_kind {
    /// A server-side browser session, minted by a successful login. Authenticates.
    ///
    /// Minted, not rotated: `login` writes a new session and retires none, so one account accumulates
    /// concurrent sessions until each is revoked, idles out, or reaches its 24-hour absolute cap.
    pub const BROWSER: &str = "browser";
    /// A named, revocable agent bearer token. Authenticates.
    pub const AGENT: &str = "agent";
    /// A one-use enrollment link. A claim token, never a request credential.
    pub const INVITE: &str = "invite";
    /// A one-use password-recovery link. A claim token, never a request credential.
    pub const RECOVERY: &str = "recovery";

    /// The kinds that may authorise an ordinary read or write. An invitation or a recovery link is a
    /// real credential row that must not open the corpus: a leaked link would otherwise also be an
    /// account key.
    pub const AUTHENTICATING: [&str; 2] = [BROWSER, AGENT];
}

/// The persisted spelling of [`crate::models::MemoryScope`]'s three shapes. The enum stays the only way
/// to *construct* a scope in Rust; these are the strings a `scope`/`*_scope` column accepts, and the
/// resolver maps between them.
pub mod scope_kind {
    pub const PERSONAL: &str = "personal";
    pub const PROJECT: &str = "project";
    pub const UNASSIGNED: &str = "unassigned";
}

/// `semantic_operation.kind`, the closed journal alphabet. An effect set the journal cannot name is an
/// effect set nothing can audit or revert, so a new kind is a migration.
pub mod operation_kind {
    pub const STORE: &str = "store";
    pub const IMPORT: &str = "import";
    pub const UPDATE: &str = "update";
    pub const DELETE: &str = "delete";
    pub const MOVE: &str = "move";
    pub const CLUSTER_CHANGE: &str = "cluster_change";
    pub const COLLAPSE: &str = "collapse";
    pub const APPRAISE: &str = "appraise";
    pub const REMINDER_MAP: &str = "reminder_map";
    pub const QUALITY: &str = "quality";
    pub const REVERT: &str = "revert";
    /// Operator enrollment. Distinct from [`STORE`] because it creates the owner rather than writing a
    /// memory, and it is what makes `bootstrap_owner` auditable.
    pub const BOOTSTRAP_OWNER: &str = "bootstrap_owner";
}

/// `semantic_snapshot.kind`, the four payload variants.
pub mod snapshot_kind {
    pub const FACT: &str = "fact";
    pub const EDGE: &str = "edge";
    pub const CLUSTER: &str = "cluster";
    pub const REMINDER: &str = "reminder";
}

/// `classification_proposal.state`. `approved` is a human decision; nothing in this ladder is
/// automatic, and an agent's own proposal never moves a memory.
pub mod proposal_state {
    pub const PENDING: &str = "pending";
    pub const APPROVED: &str = "approved";
    pub const DISMISSED: &str = "dismissed";
    pub const STALE: &str = "stale";
}

/// `ownership_migration.state`. A store that predates v009 has no row at all, which is why readers
/// fail closed on absence rather than reading "complete" out of a missing record.
pub mod migration_state {
    pub const PENDING: &str = "pending";
    pub const RUNNING: &str = "running";
    pub const PAUSED: &str = "paused";
    pub const VERIFYING: &str = "verifying";
    pub const COMPLETE: &str = "complete";
    pub const FAILED: &str = "failed";
}

/// Normalises a login the way `account.login`'s assertion requires: trimmed, then lowercased.
///
/// One implementation, because Task 4's password lookup has to arrive at the same string the
/// enrollment path stored — and a login stored as-typed is a second owner that authentication can
/// never reach, which the schema's own `ASSERT` would have refused at the write.
pub fn normalize_login(login: &str) -> String {
    login.trim().to_lowercase()
}

/// A login the schema will accept, normalised. Blank and over-long are rejected here rather than at
/// the engine so the caller gets the same message on a fresh store and on a legacy one.
pub(crate) fn normalize_enrollable_login(login: &str) -> Result<String, StorageError> {
    let normalised = normalize_login(login);
    if normalised.is_empty() {
        return Err(StorageError::InvalidInput(
            "login must not be blank".to_string(),
        ));
    }
    // The engine's bound is `string::len(...) > 2`, i.e. at least three characters, and at most 64.
    if normalised.chars().count() < 3 || normalised.chars().count() > 64 {
        return Err(StorageError::InvalidInput(
            "login must be between 3 and 64 characters".to_string(),
        ));
    }
    Ok(normalised)
}

/// The identity of one `bootstrap_owner` request: the canonical normalised login plus the exact
/// prepared `password_hash` string that request carried, length-prefixed and domain-tagged so no
/// two different parts can be re-split into the same pre-image.
///
/// What this is *not*: a plaintext-equivalence test, or a way to recognise the same password across
/// independently salted hashes. The API receives a prepared hash, so hashing it pins the prepared
/// request — retrying an operation means reusing that operation's prepared hash, while a later
/// legitimate rotation changes `password_hash` without rewriting this value.
pub fn bootstrap_request_fingerprint(login: &str, password_hash: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(FINGERPRINT_DOMAIN.as_bytes());
    for part in [login, password_hash] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Tags the pre-image so a fingerprint computed under a different rule can never be mistaken for
/// one this build would accept.
const FINGERPRINT_DOMAIN: &str = "alexandria:bootstrap_owner:v1";

/// The pair [`crate::repos::IdentityRepo::bootstrap_owner`] creates. Data, not authority: see the
/// module comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalIds {
    pub account: AccountId,
    pub workspace: WorkspaceId,
}

/// A row of `account`.
///
/// `Debug` is written by hand rather than derived, and `password_hash` is not serialised: both are the
/// P2-8 rule that no secret material may reach a log line or a JSON render, on a column Task 4 owns the
/// writing of. The value stays a plain `Option<String>` because reading it back is the whole job — a
/// redacting newtype would put the redaction logic between the query and the verification that needs the
/// bytes, and would still have to expose them.
#[derive(Clone, Serialize, Deserialize, SurrealValue)]
pub struct Account {
    pub id: Option<RecordId>,
    pub login: String,
    pub status: Option<String>,
    pub role: Option<String>,
    pub auth_epoch: Option<i64>,
    /// Written by Task 4 (Argon2id), stored by whoever enrols the account. Absent means "no password
    /// authenticates this account", which is representable because the design separates account
    /// identity from credentials for a future SSO link.
    #[serde(skip_serializing)]
    pub password_hash: Option<String>,
    /// The `OperationId` that created this owner — the retry anchor.
    pub bootstrap_operation: Option<String>,
    /// SHA-256 of the request that anchor was used with (see
    /// [`bootstrap_request_fingerprint`]). Absent means the row predates the column, which is
    /// unknown history — not permission to adopt a new request as the original one.
    pub bootstrap_fingerprint: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("id", &self.id)
            .field("login", &self.login)
            .field("status", &self.status)
            .field("role", &self.role)
            .field("auth_epoch", &self.auth_epoch)
            // Present-or-absent is a fact an operator needs; the content is not.
            .field("password_hash", &hide(&self.password_hash))
            .field("bootstrap_operation", &self.bootstrap_operation)
            .field("bootstrap_fingerprint", &self.bootstrap_fingerprint)
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// Renders the presence of a secret-bearing column, never its content, for the hand-written `Debug`
/// impls below. `Some("<redacted>")` keeps a `Debug` line readable as a row shape while making it
/// impossible for a digest or a hash to ride a log line out of the process.
fn hide(value: &Option<String>) -> Option<&'static str> {
    value.as_ref().map(|_| "<redacted>")
}

impl Account {
    /// This row's id as a validated handle. A row read from `account` whose id names another table is
    /// a bug in the query, and it fails here rather than becoming somebody else's account.
    pub fn account_id(&self) -> Result<AccountId, StorageError> {
        AccountId::try_from(self.id.clone().ok_or_else(no_id)?)
    }

    /// Whether this account may currently act: present, and `active`. Absent status is a row this
    /// file's own query did not project, and `NONE` on a legacy row is not a permission.
    pub fn is_active(&self) -> bool {
        self.status.as_deref() == Some(account_status::ACTIVE)
    }
}

/// A row of `workspace`.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Workspace {
    pub id: Option<RecordId>,
    pub owner: Option<RecordId>,
    pub created_at: Option<DateTime<Utc>>,
}

impl Workspace {
    pub fn workspace_id(&self) -> Result<WorkspaceId, StorageError> {
        WorkspaceId::try_from(self.id.clone().ok_or_else(no_id)?)
    }

    /// The account that owns this workspace, re-validated as an `account` handle.
    pub fn owner(&self) -> Result<AccountId, StorageError> {
        AccountId::try_from(self.owner.clone().ok_or_else(no_id)?)
    }
}

/// A row of `project` — one repository identity inside exactly one workspace.
///
/// The alias is the *stored* identity: `normalized_alias` is what `idx_project_alias` is unique on,
/// and v009's own assertion requires it to equal its own lowercase. `display_name` is the
/// human-facing label the caller supplied when the project was established, and it is the only place
/// a case-preserved spelling survives, because `project_alias.alias` is lowercased by its own
/// assertion too. `updated_at` exists in the schema and is deliberately not carried: nothing in this
/// crate reads it, and a row shape that pretends to know a column it never uses is how a later writer
/// forgets to set one.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Project {
    pub id: Option<RecordId>,
    pub workspace: Option<RecordId>,
    pub normalized_alias: String,
    pub display_name: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

impl Project {
    /// This row's handle, re-validated as a `project` id rather than accepted as a string.
    pub fn project_id(&self) -> Result<ProjectId, StorageError> {
        ProjectId::try_from(self.id.clone().ok_or_else(no_id)?)
    }

    /// The workspace this project belongs to, re-validated.
    ///
    /// A `project` row with no workspace is corrupt by schema, so this is an internal refusal rather
    /// than an `Option` a caller could quietly fall back on: an unscoped project is a cross-workspace
    /// read waiting to happen.
    pub fn workspace_id(&self) -> Result<WorkspaceId, StorageError> {
        WorkspaceId::try_from(self.workspace.clone().ok_or_else(no_id)?)
    }
}

/// A row of `classification_proposal`: an agent's recorded judgement about where a memory belongs.
///
/// Carries no authority. `state` stays `pending` until Task 11's human decision moves it, `revision`
/// is that task's compare-and-swap target, and `source_revision` is the memory revision the proposal
/// was formed against — the number that lets a later approval tell "still the same memory" from "the
/// memory moved underneath it".
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct ClassificationProposal {
    pub id: Option<RecordId>,
    pub workspace: Option<RecordId>,
    pub account: Option<RecordId>,
    pub fact: Option<RecordId>,
    /// One of [`proposal_state`]`s` kinds, mirrored by v009's `ASSERT` on `destination_kind`.
    pub destination_kind: String,
    pub destination_project: Option<RecordId>,
    pub source_revision: Option<i64>,
    pub revision: i64,
    pub state: String,
    pub reason: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

impl ClassificationProposal {
    pub fn proposal_id(&self) -> Result<RecordId, StorageError> {
        self.id.clone().ok_or_else(no_id)
    }
}

/// A row of `credential`. The state `AccessRepo::authorize` re-reads on every call, so revocation and
/// recovery are felt by a principal that was already issued.
///
/// Hand-written `Debug` and `skip_serializing` on the two digest columns for the same reason as
/// [`Account`]: a digest of a 256-bit random secret is not itself recoverable, but it *is* the value an
/// attacker who can read a log line replays, and a debug render of a row is exactly where such a line
/// comes from. Task 4 owns writing both columns.
#[derive(Clone, Serialize, Deserialize, SurrealValue)]
pub struct Credential {
    pub id: Option<RecordId>,
    pub account: Option<RecordId>,
    /// One of [`credential_kind`]. Required by the schema, so a row cannot be "some kind".
    pub kind: String,
    pub label: Option<String>,
    /// Digest of the secret, written by Task 4. Never the secret, and never logged.
    #[serde(skip_serializing)]
    pub secret_digest: Option<String>,
    /// The `account.auth_epoch` this credential was minted under.
    pub auth_epoch: Option<i64>,
    pub revoked: Option<bool>,
    pub consumed: Option<bool>,
    /// The deadline past which this credential is inert *without further authenticated use*. For a
    /// browser session it is rolled forward on each use and clamped to [`Credential::absolute_expires_at`];
    /// for an agent token it is absent, which means "no deadline", not "unknown".
    pub expires_at: Option<DateTime<Utc>>,
    /// The absolute cap on a browser session's life: the deadline no amount of use can roll past.
    /// Absent for every other kind.
    pub absolute_expires_at: Option<DateTime<Utc>>,
    /// Digest of the CSRF token paired with a browser session's cookie. Its own column, so neither half
    /// can be presented as the other — and the reason the session-resolution query reads `secret_digest`
    /// and never this one.
    #[serde(skip_serializing)]
    pub csrf_digest: Option<String>,
    /// The last authenticated use, written by Task 4 on a successful presentation. Audit and idle-expiry
    /// bookkeeping; deliberately *not* an access clock for heat or stability, which stay on `heat_state`.
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub consumed_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("id", &self.id)
            .field("account", &self.account)
            .field("kind", &self.kind)
            .field("label", &self.label)
            .field("secret_digest", &hide(&self.secret_digest))
            .field("csrf_digest", &hide(&self.csrf_digest))
            .field("auth_epoch", &self.auth_epoch)
            .field("revoked", &self.revoked)
            .field("consumed", &self.consumed)
            .field("expires_at", &self.expires_at)
            .field("absolute_expires_at", &self.absolute_expires_at)
            .field("last_used_at", &self.last_used_at)
            .field("revoked_at", &self.revoked_at)
            .field("consumed_at", &self.consumed_at)
            .field("created_at", &self.created_at)
            .finish()
    }
}

impl Credential {
    pub fn credential_id(&self) -> Result<CredentialId, StorageError> {
        CredentialId::try_from(self.id.clone().ok_or_else(no_id)?)
    }

    /// The account this credential belongs to. The ownership question `authorize` asks is exactly
    /// `credential_id.account() == principal.account()`, and it is asked of the row, not the caller.
    pub fn account_id(&self) -> Result<AccountId, StorageError> {
        AccountId::try_from(self.account.clone().ok_or_else(no_id)?)
    }
}

fn no_id() -> StorageError {
    StorageError::Internal(anyhow::anyhow!(
        "a row was read without its id; the query has to select it"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::types::RecordId;

    /// Login normalisation is the enrollment path's, not each caller's: two spellings of one login is
    /// two owners the schema cannot tell apart.
    /// The fingerprint identifies one prepared request: stable for identical input, different for any
    /// change to either part, and never a rendering of the secret it was built from.
    #[test]
    fn bootstrap_fingerprint_identifies_the_prepared_request() {
        let base = bootstrap_request_fingerprint("alice", "$argon2id$prepared-one");
        assert_eq!(
            base,
            bootstrap_request_fingerprint("alice", "$argon2id$prepared-one")
        );
        assert_eq!(base.len(), 64, "not a SHA-256 hex digest: {base}");
        assert!(
            base.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "not lowercase hex: {base}"
        );
        assert!(!base.contains("argon2id"), "the digest echoed its input");

        // A different prepared hash is a different request, and so is a different login. Hashing a
        // salted digest pins the prepared request; it cannot promise equivalence across salts.
        assert_ne!(
            base,
            bootstrap_request_fingerprint("alice", "$argon2id$prepared-two")
        );
        assert_ne!(
            base,
            bootstrap_request_fingerprint("bob", "$argon2id$prepared-one")
        );
        // Length prefixes: one string split two ways must not collide with another split.
        assert_ne!(
            bootstrap_request_fingerprint("ab", "c"),
            bootstrap_request_fingerprint("a", "bc")
        );
    }

    #[test]
    fn normalize_login_trims_then_lowercases() {
        assert_eq!(normalize_login("  Alice "), "alice");
        assert_eq!(normalize_login("ALICE"), "alice");
        assert_eq!(normalize_login("already-normal"), "already-normal");
        // Interior characters are data, not decoration: `string::lowercase` of the trimmed form is
        // what the schema's own assertion compares against.
        assert_eq!(normalize_login(" A B "), "a b");
    }

    #[test]
    fn enrollable_login_rejects_what_the_schema_assertion_would() {
        assert_eq!(normalize_enrollable_login(" Alice ").unwrap(), "alice");
        assert!(normalize_enrollable_login("").is_err());
        assert!(normalize_enrollable_login("   ").is_err());
        assert!(
            normalize_enrollable_login("ab").is_err(),
            "the ASSERT is > 2"
        );
        assert!(normalize_enrollable_login(&"a".repeat(65)).is_err());
        // The rejection names the field and quotes no secret-looking value back.
        let message = normalize_enrollable_login("").err().unwrap().to_string();
        assert!(message.contains("login"), "{message}");
        let message = normalize_enrollable_login("a".repeat(70).as_str())
            .err()
            .unwrap()
            .to_string();
        assert!(
            !message.contains(&"a".repeat(20)),
            "message echoed the login: {message}"
        );
    }

    /// Every typed accessor on this module rejects a foreign-table id, and only accepts its own. The
    /// ids come from `RecordId` values, which is the deserialisation boundary the crate's rules put the
    /// table check at.
    #[test]
    fn row_accessors_revalidate_the_table() {
        let account_id = AccountId::try_from(RecordId::new("account", "a_1")).unwrap();
        let other_account = AccountId::try_from(RecordId::new("account", "someone_else")).unwrap();
        let workspace_id = WorkspaceId::try_from(RecordId::new("workspace", "w_1")).unwrap();
        let account = Account {
            id: Some(account_id.as_record_id().clone()),
            login: "alice".into(),
            status: None,
            role: None,
            auth_epoch: None,
            password_hash: None,
            bootstrap_operation: None,
            bootstrap_fingerprint: None,
            created_at: None,
        };
        let workspace = Workspace {
            id: Some(workspace_id.as_record_id().clone()),
            owner: Some(account_id.as_record_id().clone()),
            created_at: None,
        };
        let credential = Credential {
            id: Some(RecordId::new("credential", "c_1")),
            // Somebody else's account on my credential row: the ownership check `authorize` performs.
            account: Some(other_account.as_record_id().clone()),
            kind: credential_kind::AGENT.to_string(),
            label: None,
            secret_digest: None,
            auth_epoch: None,
            revoked: None,
            consumed: None,
            expires_at: None,
            absolute_expires_at: None,
            csrf_digest: None,
            last_used_at: None,
            revoked_at: None,
            consumed_at: None,
            created_at: None,
        };

        // Happy paths first: each getter resolves the row it is meant to resolve.
        assert_eq!(account.account_id().unwrap(), account_id);
        assert_eq!(workspace.workspace_id().unwrap(), workspace_id);
        assert_eq!(workspace.owner().unwrap(), account_id);
        assert_eq!(
            credential.account_id().unwrap(),
            other_account,
            "a credential row lost the account it names"
        );
        assert!(credential.credential_id().is_ok());

        // Then every getter, against a handle from each table it must refuse: a row read from
        // `account` whose id names another table is a query bug, and each getter turns that into an
        // error instead of somebody else's identity. The own table of each getter is skipped, so all
        // five are covered against all four foreign tables.
        for table in ["account", "workspace", "fact", "credential", "project"] {
            let foreign = RecordId::new(table, "no-good");

            if table != "account" {
                let mut row = account.clone();
                row.id = Some(foreign.clone());
                assert!(
                    row.account_id().is_err(),
                    "Account::account_id accepted `{table}`"
                );

                let mut row = workspace.clone();
                row.owner = Some(foreign.clone());
                assert!(row.owner().is_err(), "Workspace::owner accepted `{table}`");

                let mut row = credential.clone();
                row.account = Some(foreign.clone());
                assert!(
                    row.account_id().is_err(),
                    "Credential::account_id accepted `{table}`"
                );
            }
            if table != "workspace" {
                let mut row = workspace.clone();
                row.id = Some(foreign.clone());
                assert!(
                    row.workspace_id().is_err(),
                    "Workspace::workspace_id accepted `{table}`"
                );
            }
            if table != "credential" {
                let mut row = credential.clone();
                row.id = Some(foreign.clone());
                assert!(
                    row.credential_id().is_err(),
                    "Credential::credential_id accepted `{table}`"
                );
            }
        }
    }

    /// A missing id is an internal bug in the query, never a silent `None` that a caller could
    /// mistake for "no such row".
    #[test]
    fn a_row_without_an_id_is_an_error_not_a_none() {
        let workspace = Workspace {
            id: None,
            owner: None,
            created_at: None,
        };
        let error = workspace.workspace_id().unwrap_err();
        assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
    }

    /// P2-8: secret material may not reach a `Debug` render or a JSON serialisation of a row, because both
    /// are ways a log line gets written. The *presence* of each column still renders, so a diagnostic
    /// remains readable as a row shape.
    #[test]
    fn row_debug_and_json_never_render_secret_material() {
        let hash = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0$c2VjcmV0ZGlnZXN0aGVyZQ";
        let digest = "9dCj0Z3nG7m1Hq0pQ2sT4xYz6aBcDeFgHiJkLmNoPqRs";
        let csrf = "Zm91clRoaXJ0eUJ5dGVTZWNyZXREaWdlc3RWYWx1ZUhlcmU";
        let account = Account {
            id: Some(RecordId::new("account", "a_1")),
            login: "alice".to_string(),
            status: Some(account_status::ACTIVE.to_string()),
            role: Some(account_role::OWNER.to_string()),
            auth_epoch: Some(1),
            password_hash: Some(hash.to_string()),
            bootstrap_operation: Some("bootstrap-1".to_string()),
            bootstrap_fingerprint: Some("a".repeat(64)),
            created_at: None,
        };
        let credential = Credential {
            id: Some(RecordId::new("credential", "c_1")),
            account: Some(RecordId::new("account", "a_1")),
            kind: credential_kind::BROWSER.to_string(),
            label: Some("laptop".to_string()),
            secret_digest: Some(digest.to_string()),
            auth_epoch: Some(1),
            revoked: Some(false),
            consumed: Some(false),
            expires_at: None,
            absolute_expires_at: None,
            csrf_digest: Some(csrf.to_string()),
            last_used_at: None,
            revoked_at: None,
            consumed_at: None,
            created_at: None,
        };

        for (label, rendered) in [
            ("Account debug", format!("{account:?}")),
            (
                "Account json",
                serde_json::to_string(&account).expect("an account row must serialise"),
            ),
            ("Credential debug", format!("{credential:?}")),
            (
                "Credential json",
                serde_json::to_string(&credential).expect("a credential row must serialise"),
            ),
        ] {
            for secret in [hash, digest, csrf] {
                assert!(
                    !rendered.contains(secret),
                    "{label} rendered secret material"
                );
            }
            assert!(
                !rendered.contains("19456") && !rendered.contains("argon2id"),
                "{label} rendered the password digest's own policy"
            );
        }
        // Presence survives, which is what makes the render still useful: redaction is not omission.
        assert!(format!("{account:?}").contains("password_hash: Some(\"<redacted>\")"));
        let rendered = format!("{credential:?}");
        assert!(rendered.contains("secret_digest: Some(\"<redacted>\")"));
        assert!(rendered.contains("csrf_digest: Some(\"<redacted>\")"));
        // And a row with no digest yet — every legacy and freshly-enrolled one — renders `None`, so the
        // redaction cannot be mistaken for a value that was dropped.
        let empty = Credential {
            secret_digest: None,
            csrf_digest: None,
            ..credential.clone()
        };
        assert!(format!("{empty:?}").contains("secret_digest: None"));
        // Reading the row back is still possible: the columns are only hidden on the way out.
        let json = serde_json::to_value(&account).unwrap();
        assert!(
            json.get("password_hash").is_none(),
            "the hash was serialised"
        );
        assert_eq!(json["login"], "alice");
    }
}
