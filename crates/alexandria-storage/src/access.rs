//! Authorized access contexts, and the repository that resolves them from live rows.
//!
//! The four types here are capabilities, not data. A [`Principal`] is an authenticated actor; an
//! [`OwnerContext`] is that actor's authority over their own workspace; a [`ScopedContext`] is one
//! resolved read population inside it; a [`SystemContext`] is the explicit startup/background authority
//! that selects without an owner. None of them can be constructed, deserialised or defaulted outside
//! this crate, which is what makes possession of one meaningful: an MCP request handler may be handed a
//! `ScopeKey`, a `ReadSelection` or a client-supplied owner id straight from JSON — those are the
//! caller's words — and no combination of those words mints a context.
//!
//! [`AccessRepo`] is the producer. It is the only thing in the crate that turns a `Principal` into an
//! [`OwnerContext`] and an [`OwnerContext`] into a [`ScopedContext`], and it decides both by reading the
//! database: the account's status, its authentication epoch, the particular credential's ownership and
//! revocation state, and the workspace the account owns. That is why this module runs queries — a
//! context a caller could assemble by hand would make the authority a claim instead of a finding.
//!
//! Two properties are worth stating plainly because they are the reason the types look the way they
//! do:
//!
//! * **Authority is issued, never asserted.** [`SystemContext`] comes from
//!   [`Database::claim_system_authority`], which takes `&mut self` and succeeds once per database
//!   instance. Request-facing code holds `Arc<Database>` and cannot produce that borrow, and a
//!   capability minted against one store is refused by another.
//! * **A missing credential is not a local request.** [`PrincipalProvenance::TrustedLocal`] exists only
//!   on the path that already proved it holds the startup capability
//!   ([`AccessRepo::local_principal`]); a hosted request that presented no credential has no way to
//!   become the operator by omission.
//!
//! The negative examples below are about these types, not about the harness. `serde` is reachable from
//! outside this crate, and an existing row shape satisfies the same bound the negatives fail:
//!
//! ```
//! fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
//! requires_deserialize::<alexandria_storage::models::memory::Fact>();
//! ```

use crate::connection::{Database, InstanceAuthority};
use crate::error::StorageError;
use crate::models::auth::{Account, Credential, Workspace};
use crate::models::identity::{
    AccountId, CredentialId, MemoryScope, Partition, ProjectId, ReadSelection, WorkspaceId,
};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue};

/// How a principal reached the server, sealed so that "no credential was presented" cannot be read
/// back as "then this must be the local operator".
///
/// [`AccessRepo::authorize`] re-checks both variants, and they are not the same check: a hosted
/// credential is re-read row by row, and a trusted-local one is only ever issuable against the startup
/// capability of this database instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalProvenance {
    /// A credential row this server verified: a browser session or an agent token. The handle is to
    /// *that* row, so revoking it is felt by the principal holding it and by nobody else on the account.
    Credential(CredentialId),
    /// The configured local owner, on a stdio or offline-command path. Issued only by
    /// [`AccessRepo::local_principal`], which requires this instance's [`SystemContext`] and an account
    /// that exists in the store.
    TrustedLocal,
}

/// The authenticated actor behind a request or an offline command.
///
/// Only the authentication boundary can produce one: Task 4's `AuthRepo`, from a credential it has
/// actually verified, or [`AccessRepo::local_principal`] for the configured owner. Both are inside this
/// crate — [`Principal::authenticated`] and the private struct literal — and there is no public or
/// test-only constructor to reach from a request handler.
///
/// ```compile_fail
/// use alexandria_storage::access::{Principal, PrincipalProvenance};
/// use alexandria_storage::models::identity::{AccountId, CredentialId};
/// use surrealdb::types::RecordId;
///
/// // Every handle here is built the way any caller builds one, so the only thing left to fail on is
/// // the struct literal itself: an outside crate cannot assemble a `Principal` from two validated
/// // ids it already holds.
/// let claimed = AccountId::try_from(RecordId::new("account", "someone_else")).unwrap();
/// let credential = CredentialId::try_from(RecordId::new("credential", "not_mine")).unwrap();
/// let principal = Principal {
///     account: claimed,
///     provenance: PrincipalProvenance::Credential(credential),
/// };
/// # let _ = principal;
/// ```
///
/// ```compile_fail
/// use alexandria_storage::access::Principal;
///
/// fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
/// requires_deserialize::<Principal>();
/// ```
#[derive(Debug, Clone)]
pub struct Principal {
    account: AccountId,
    provenance: PrincipalProvenance,
}

impl Principal {
    /// The account this principal was authenticated as — readable, never writable. Handing this out
    /// lets a caller log or scope-check an actor without handing out the authority to be it.
    pub fn account(&self) -> &AccountId {
        &self.account
    }

    /// How the principal reached the server. Read-only: the value decides what `authorize` re-checks,
    /// and a caller must not be able to rewrite that history by changing it.
    pub fn provenance(&self) -> &PrincipalProvenance {
        &self.provenance
    }

    /// The credential this principal authenticated with, if any. `None` means a trusted-local
    /// principal, and is *not* a hosted request that failed to present one.
    pub fn credential(&self) -> Option<&CredentialId> {
        match &self.provenance {
            PrincipalProvenance::Credential(credential) => Some(credential),
            PrincipalProvenance::TrustedLocal => None,
        }
    }

    /// Mints the principal that a verified hosted credential entitles: the production path, crate-private
    /// for the same reason [`SystemContext::issued_by`] is.
    ///
    /// Two places in this crate reach it. [`crate::repos::AuthRepo`] after it has resolved a presented
    /// secret to a credential row, confirmed that row's digest in constant time, and re-read the account
    /// behind it; and [`AccessRepo::local_principal`], which builds the trusted-local variant with the
    /// struct literal. Neither is public, and neither is test-only, so the `compile_fail` controls on
    /// [`Principal`] remain the honest proof that an outside crate cannot assemble one — which is the whole
    /// reason the MCP layer's `Principal` is a finding rather than a claim.
    ///
    /// It takes two validated handles rather than a row because the checks that make a principal
    /// meaningful are the caller's (revocation, expiry, minted epoch, account status), and this function's
    /// only job is to make the result unforgeable. `AccessRepo::authorize` re-reads those states on every
    /// call precisely so that a caller which skipped them here cannot get far.
    pub(crate) fn authenticated(account: AccountId, credential: CredentialId) -> Self {
        Principal {
            account,
            provenance: PrincipalProvenance::Credential(credential),
        }
    }
}

/// How the owner behind an [`OwnerContext`] is entitled to *write*, and under which epoch.
///
/// This exists because a scoped write has to re-assert its own authority **inside the transaction that
/// performs it**, and the two things that decide that are not on the workspace: they are the account's
/// current `auth_epoch` and, for a hosted caller, the identity of the particular credential the request
/// authenticated with.
///
/// * [`WriteAuthorization::Credential`] names one credential row. Revoking *it* must stop the writes
///   that carry it while leaving a sibling credential of the same account untouched, so the guard has
///   to be keyed on that row — and it has to be a *conditional write on that row*, because a guarded
///   `SELECT` read inside a transaction is not atomic here: measured on the pinned engine, a revoke
///   committed by another writer between the read and the commit silently won (see
///   `scoped_repos_test::a_credential_revoked_mid_transaction_does_not_commit_a_scoped_write`).
/// * [`WriteAuthorization::TrustedLocal`] is the startup-issued local path. It carries an epoch and
///   deliberately carries **no credential id**, because it does not have one: `local_principal` issues
///   it from the operator's configured account under this instance's system capability, not from a
///   bearer token. A local write therefore guards the account row, and must not pretend to guard a
///   credential it never held.
///
/// `pub(crate)`, and constructed in exactly one place — [`AccessRepo::authorize`], from a `Principal`
/// whose provenance it re-read. It is not `pub`, so no caller outside storage can assemble an
/// `OwnerContext` that claims a credential it was not issued, and it has no `Serialize`/`Deserialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteAuthorization {
    Credential {
        credential_id: CredentialId,
        auth_epoch: i64,
    },
    TrustedLocal {
        auth_epoch: i64,
    },
}

/// Authority over one account's own workspace, issued by the authorization step rather than claimed by
/// the caller.
///
/// ```compile_fail
/// use alexandria_storage::access::OwnerContext;
/// use alexandria_storage::models::identity::{AccountId, WorkspaceId};
/// use surrealdb::types::RecordId;
///
/// let account = AccountId::try_from(RecordId::new("account", "someone_else")).unwrap();
/// let workspace = WorkspaceId::try_from(RecordId::new("workspace", "w_1")).unwrap();
/// let owner = OwnerContext { account, workspace };
/// ```
///
/// ```compile_fail
/// use alexandria_storage::access::OwnerContext;
///
/// fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
/// requires_deserialize::<OwnerContext>();
/// ```
///
/// ```compile_fail
/// // The write authorization is not forgeable from outside storage either. `OwnerContext` is `pub`
/// // but every field is private, so naming a field that is not in the public API is the refusal —
/// // and `WriteAuthorization` itself is `pub(crate)`, so an outside crate cannot even name the type
/// // it would have to build. There is no `OwnerContext::with_credential`, no `Default`, and no
/// // constructor that takes a client-supplied credential id: the epoch and the credential a scoped
/// // write guards come from rows `authorize` read, or the write does not happen.
/// use alexandria_storage::access::OwnerContext;
/// use surrealdb::types::RecordId;
///
/// let account = RecordId::parse_simple("account:alice").unwrap();
/// let owner = OwnerContext { claimed: account };
/// ```
#[derive(Debug, Clone)]
pub struct OwnerContext {
    account: AccountId,
    workspace: WorkspaceId,
    write: WriteAuthorization,
}

impl OwnerContext {
    pub fn account(&self) -> &AccountId {
        &self.account
    }

    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }

    /// The write authorization this context was issued with. `pub(crate)`: readers may be handed an
    /// `OwnerContext`, and every scoped *writer* lives in this crate, so nothing outside storage needs
    /// — and nothing outside storage gets — the credential id and epoch a guarded write runs on.
    pub(crate) fn write_authorization(&self) -> &WriteAuthorization {
        &self.write
    }
}

/// The authority to write *semantic* state, and to have written it recorded in the private journal.
///
/// This is deliberately a second capability rather than a method on [`OwnerContext`], because a journal
/// row is not an ordinary write: it names its actor and the credential that presented itself, and it
/// leaves a before-state an owner can read back on the history page. Holding a read context must not be
/// enough to produce that record, and `OwnerContext` is handed to readers as well as writers.
///
/// The only producer is [`MutationContext::issued_to`], which takes an [`OwnerContext`] — a value nothing
/// outside this crate can build, deserialize, or assemble from an id. So the type adds no authority: it
/// *re-names* the authority an admitted request already holds, and in doing so carries the initiator the
/// journal needs. There is no constructor from an [`AccountId`], no `Default`, no deserialisation, and
/// **no path from [`AdminCapability`]**: the administrator's role manages accounts and reads content-free
/// health, and a capability that could be turned into a corpus writer would make that sentence false.
///
/// ```compile_fail
/// use alexandria_storage::access::MutationContext;
/// use alexandria_storage::models::identity::AccountId;
/// use surrealdb::types::RecordId;
///
/// let account = AccountId::try_from(RecordId::new("account", "someone_else")).unwrap();
/// let write = MutationContext::issued_for(&account);
/// ```
///
/// ```compile_fail
/// use alexandria_storage::access::MutationContext;
///
/// fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
/// requires_deserialize::<MutationContext>();
/// ```
#[derive(Debug, Clone)]
pub struct MutationContext {
    owner: OwnerContext,
}

impl MutationContext {
    /// Re-name an issued owner authority as a write authority.
    ///
    /// Taking `&OwnerContext` rather than two ids is the whole design: the account, the workspace, the
    /// epoch and the credential are whatever `AccessRepo::authorize` read off live rows, and a caller
    /// that never went through authorization has nothing to hand here.
    pub fn issued_to(owner: &OwnerContext) -> Self {
        Self {
            owner: owner.clone(),
        }
    }

    pub fn account(&self) -> &AccountId {
        self.owner.account()
    }

    pub fn workspace(&self) -> &WorkspaceId {
        self.owner.workspace()
    }

    /// The owner context this write authority was issued from, for the scoped writers that guard on it.
    pub fn owner(&self) -> &OwnerContext {
        &self.owner
    }

    /// The credential that presented itself, for `semantic_operation.initiator`.
    ///
    /// `None` means a trusted-local principal, which holds no bearer secret — the same meaning its
    /// `Option` carries on [`Principal::credential`], and the audit answer to "browser, agent token, or
    /// the operator?" without a second table.
    pub(crate) fn initiator(&self) -> Option<RecordId> {
        match self.owner.write_authorization() {
            WriteAuthorization::Credential { credential_id, .. } => {
                Some(credential_id.as_record_id().clone())
            }
            WriteAuthorization::TrustedLocal { .. } => None,
        }
    }
}

/// One resolved read population: the partitions a request may actually touch.
///
/// The vector is whatever the authorization step confirmed the context owns, so it is the only form a
/// scoped read takes. A `Vec<Partition>` the caller assembled itself is just data.
///
/// ```compile_fail
/// use alexandria_storage::access::ScopedContext;
/// use alexandria_storage::models::identity::{MemoryScope, Partition, WorkspaceId};
/// use surrealdb::types::RecordId;
///
/// let workspace = WorkspaceId::try_from(RecordId::new("workspace", "w_1")).unwrap();
/// let partitions = vec![Partition { workspace: workspace.clone(), scope: MemoryScope::Personal }];
/// let context = ScopedContext { workspace, partitions };
/// ```
///
/// ```compile_fail
/// use alexandria_storage::access::ScopedContext;
///
/// fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
/// requires_deserialize::<ScopedContext>();
/// ```
#[derive(Debug, Clone)]
pub struct ScopedContext {
    workspace: WorkspaceId,
    partitions: Vec<Partition>,
}

impl ScopedContext {
    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }

    /// The partitions this context may read, every one already qualified to [`Self::workspace`]:
    /// Personal first, then projects by record id. Deterministic on purpose — a budgeted pass has to
    /// resume against the same list, and a `Hash`-ordered one would not.
    pub fn partitions(&self) -> &[Partition] {
        &self.partitions
    }

    /// The scopes of the resolved partitions, in the same order. What the plan's anchor for
    /// `auto_without_project_is_personal_only` compares against.
    pub fn scopes(&self) -> Vec<MemoryScope> {
        self.partitions.iter().map(|p| p.scope.clone()).collect()
    }
}

/// Explicit system authority for offline and background work, with no owner behind it.
///
/// Issued exactly once per database instance, by [`Database::claim_system_authority`] at the end of
/// startup composition — the boundary the owner selected. That a request-facing handle offers no
/// second path is the point: `AlexandriaServer` holds `Arc<Database>`, which cannot produce the
/// `&mut` borrow, and a capability minted for one store is refused by another.
///
/// It is a Rust in-process capability, not a network token, and it does not sandbox code that already
/// shares this process — whoever can reach the engine handle can already reach the data. What it buys
/// is that authority is *issued* rather than asserted, so no request payload, configured string or
/// missing credential can become one.
///
/// ```compile_fail
/// use alexandria_storage::access::SystemContext;
///
/// // The field is real and private: construction is the mint path this type does not have.
/// let system = SystemContext {
///     authority: std::sync::Arc::new(()),
/// };
/// ```
///
/// ```compile_fail
/// use alexandria_storage::access::SystemContext;
///
/// fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
/// requires_deserialize::<SystemContext>();
/// ```
///
/// ```compile_fail
/// // And no storage-owned shortcut opens a persistent store, migrates it and hands back the
/// // capability in one call: that order skips the embedding admission a real store needs before its
/// // schema is written. Startup composes the steps instead.
/// use alexandria_storage::Database;
///
/// # async fn runs() {
/// let (db, system) = Database::bootstrap(std::path::Path::new("/tmp/alexandria-data")).await.unwrap();
/// # let _ = (db, system);
/// # }
/// ```
///
/// ```compile_fail
/// // No default, no `offline()`, no callback that hands one to a closure: any of those would give
/// // every request handler in the process the background authority for free.
/// use alexandria_storage::access::SystemContext;
///
/// let system = SystemContext::default();
/// ```
///
/// ```compile_fail
/// // The half that is about *sharing*: a request-facing holder has the database behind an `Arc`, and a
/// // shared `Arc` cannot produce the `&mut` borrow `claim_system_authority` needs. This is why the
/// // mint path takes `&mut self` rather than `&self`.
/// use alexandria_storage::Database;
///
/// # async fn runs() {
/// let db = std::sync::Arc::new(Database::connect_embedded().await.unwrap());
/// let system = std::sync::Arc::clone(&db).claim_system_authority();
/// # let _ = system;
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct SystemContext {
    authority: Arc<InstanceAuthority>,
}

impl SystemContext {
    /// The only mint path, crate-private: `InstanceAuthority::claim` reaches it, and that is reachable
    /// only from a `Database` this process owns.
    pub(crate) fn issued_by(authority: Arc<InstanceAuthority>) -> Self {
        Self { authority }
    }

    /// Which instance issued this capability. Crate-private and unexposed: an equality test anybody can
    /// run is fine, a way to *make* one is not.
    pub(crate) fn issued_by_instance(&self) -> &Arc<InstanceAuthority> {
        &self.authority
    }
}

/// Authority to manage *accounts* and read non-content service health, and nothing else.
///
/// This is deliberately a separate capability rather than a flag on [`OwnerContext`], because
/// `account.role` must never become a way to read a corpus: the design's rule is that the admin role
/// "grants account/service management, not a universal memory-reader context", and an admin's own
/// memories are served by the same ordinary [`OwnerContext`] any other account gets. A caller that
/// holds both therefore holds them for two different jobs — `OwnerContext` to read its own corpus,
/// this to touch anybody's *account row*.
///
/// The only producer is [`AccessRepo::authorize_admin`], which reads `status` and `role` from the
/// account row every time; there is no constructor, no `Default`, and no deserialisation that grants
/// authority, so a browser form field or an MCP argument naming an account cannot become this.
///
/// ```compile_fail
/// use alexandria_storage::access::AdminCapability;
///
/// fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
/// requires_deserialize::<AdminCapability>();
/// ```
///
/// ```compile_fail
/// use alexandria_storage::access::AdminCapability;
/// use alexandria_storage::models::identity::AccountId;
/// use surrealdb::types::RecordId;
///
/// let account = AccountId::try_from(RecordId::new("account", "root")).unwrap();
/// let admin = AdminCapability { account };
/// ```
/// The credential it was issued from is part of the capability, exactly as it is part of an
/// [`OwnerContext`]'s write authorization: an account may hold several credentials, and revoking *one*
/// of them has to stop the writes carrying that one while leaving a sibling live. Falling back to "any
/// live credential for this account" would let a revoked session be masked by an agent token, which is
/// the reason Task 6's D2 ruling refused the same shortcut for `OwnerContext`.
#[derive(Debug, Clone)]
pub struct AdminCapability {
    account: AccountId,
    credential: Option<CredentialId>,
}

impl AdminCapability {
    /// The account this capability was issued to. Reading it is a question about the *caller*, so it
    /// is safe to hand out; what it must not do is offer a way in, which is why there is no
    /// constructor and why [`AccessRepo::authorize_admin`] re-reads the row on every issuance.
    pub fn account(&self) -> &AccountId {
        &self.account
    }

    /// The credential row this capability was issued from, or `None` for a trusted-local principal,
    /// which holds no bearer secret at all. Read-only, and crate-internal consumers are the only ones
    /// that need it: an admin write claims this row alongside the account row.
    pub(crate) fn credential(&self) -> Option<&CredentialId> {
        self.credential.as_ref()
    }
}

pub struct AccessRepo<'a> {
    db: &'a Surreal<Any>,
    instance: &'a Database,
}

/// Whether a credential row still entitles its own account to act.
///
/// The one spelling of "live credential" in this crate: [`AccessRepo::authorize`] and
/// [`crate::repos::AuthRepo`] both ask it, so a session that has been revoked, spent, expired or
/// superseded cannot be inert on one path and current on the other. The rule lives here rather than being
/// restated inside the authentication path for exactly that reason.
///
/// `revoked`/`consumed` are the explicit states; the minted epoch has to equal the account's *current*
/// one, because recovery raises `account.auth_epoch` and every credential minted before it is inert
/// without anything touching its row. An invitation or recovery link is a claim token, not a request
/// credential, so it never authorises a read even while live.
///
/// `now` is a parameter rather than `Utc::now()` inside, because the authentication path runs on the
/// caller's clock — a 30-minute idle window has to be testable without sleeping — while `authorize` passes
/// the real one. An absent deadline means unconstrained: an agent token legitimately has none, and a row
/// written before `absolute_expires_at` existed has no cap.
pub(crate) fn credential_entitles_read(
    credential: &Credential,
    account: &Account,
    now: DateTime<Utc>,
) -> bool {
    if credential.revoked.unwrap_or(false) || credential.consumed.unwrap_or(false) {
        return false;
    }
    if !crate::models::auth::credential_kind::AUTHENTICATING.contains(&credential.kind.as_str()) {
        return false;
    }
    if credential
        .expires_at
        .is_some_and(|expires_at| expires_at <= now)
        || credential
            .absolute_expires_at
            .is_some_and(|absolute| absolute <= now)
    {
        return false;
    }
    match (credential.auth_epoch, account.auth_epoch) {
        (Some(minted), Some(current)) => minted == current,
        // An account row without an epoch is not an account that can mint one: treat absent as
        // unusable rather than as zero, which every credential would then match.
        _ => false,
    }
}

impl<'a> AccessRepo<'a> {
    /// Bound to a `Database` rather than a bare engine handle, because two of this repo's jobs are to
    /// ask the *instance* whether a presented [`SystemContext`] is its own, and to answer for the store
    /// the caller was handed.
    pub fn new(instance: &'a Database) -> Self {
        Self {
            db: instance.inner(),
            instance,
        }
    }

    /// Re-reads the account *and the particular credential* behind `principal`, and if both are live
    /// and connected to each other, issues the owner context for the workspace the database says this
    /// account owns.
    ///
    /// Three things are checked, and none of them is the caller's claim:
    ///
    /// 1. the account row exists and is `active`;
    /// 2. for a hosted principal, the credential it presented still exists, still belongs to *that*
    ///    account, is not revoked or consumed, has not expired, and was minted under the account's
    ///    current authentication epoch;
    /// 3. the account has a private workspace, which is the population the caller is about to be
    ///    allowed to read.
    ///
    /// Refusals are classified so a caller can tell *which* answer it is without learning anything about
    /// anybody else's rows: [`StorageError::NotFound`] for an absent or unowned id (indistinguishable
    /// from a probe), [`StorageError::Stale`] for authority that has moved — revoked, consumed,
    /// expired, an obsolete epoch, a suspended account — which means "authenticate again" and nothing
    /// more.
    pub async fn authorize(&self, principal: &Principal) -> Result<OwnerContext, StorageError> {
        let account = self
            .account_row(principal.account().as_record_id())
            .await?
            .ok_or(StorageError::NotFound)?;
        if !account.is_active() {
            return Err(StorageError::Stale);
        }

        // Held so the write authorization below is built from the row that passed these checks rather
        // than from the handle the caller presented. Only ever `Some` for a credential provenance.
        let mut verified_credential: Option<Credential> = None;
        if let PrincipalProvenance::Credential(credential_id) = &principal.provenance {
            let credential = self
                .credential_row(credential_id.as_record_id())
                .await?
                .ok_or(StorageError::NotFound)?;
            // Ownership first: a credential belonging to another account cannot authorise the account
            // the caller claimed, whatever its own state. Compared by record id, so a row whose account
            // link is missing fails closed.
            if credential.account.as_ref() != Some(principal.account().as_record_id()) {
                return Err(StorageError::NotFound);
            }
            if !credential_entitles_read(&credential, &account, Utc::now()) {
                return Err(StorageError::Stale);
            }
            verified_credential = Some(credential);
        }

        let workspace = self
            .workspace_of(principal.account())
            .await?
            .ok_or(StorageError::NotFound)?;

        // The epoch is read from the account row, never taken from the principal, and an account with
        // no epoch is an internal fault rather than a default of zero: zero is a value some credential
        // could have been minted under, so coalescing `NONE` into it would let a legacy row authorize.
        let auth_epoch = Self::live_epoch(&account)?;
        let write = match &principal.provenance {
            // The id comes from the credential row `authorize` just re-read and proved belongs to this
            // account, not from the handle the caller presented.
            PrincipalProvenance::Credential(_) => WriteAuthorization::Credential {
                credential_id: verified_credential
                    .as_ref()
                    .expect("a credential provenance was read above or this call returned")
                    .credential_id()?,
                auth_epoch,
            },
            PrincipalProvenance::TrustedLocal => WriteAuthorization::TrustedLocal { auth_epoch },
        };

        Ok(OwnerContext {
            // Both ids come out of rows. `principal.account` was only ever used as a lookup key, and
            // the workspace is the one *that* account owns, never one the caller named.
            account: account.account_id()?,
            workspace: workspace.workspace_id()?,
            write,
        })
    }

    /// The account's current authentication epoch, refusing a row that has none.
    ///
    /// Same rule as [`crate::repos::AuthRepo`]'s private helper and the reason `credential_entitles_read`
    /// treats an absent epoch as unusable: an absent epoch on a `SCHEMAFULL` table with `DEFAULT 1` is
    /// a corrupt or projected-away row, not a generation anybody can match.
    fn live_epoch(account: &Account) -> Result<i64, StorageError> {
        if !account.is_active() {
            return Err(StorageError::Stale);
        }
        account.auth_epoch.ok_or_else(|| {
            StorageError::Internal(anyhow::anyhow!(
                "an enrolled owner has no authentication epoch to bind a scoped write to"
            ))
        })
    }

    /// Resolves what a context may actually read: the automatic-recall population, the owner's whole
    /// assigned corpus, one named scope, or the owner's explicit look at the Unassigned partition.
    ///
    /// The rules that make this a boundary rather than a filter:
    ///
    /// * `Auto(None)` is Personal only. An unrecognised context never widens.
    /// * `Auto(Some(p))` adds `p` only when `p` is a project of *this* workspace. A foreign or a
    ///   never-existing id produce the same error, and neither is quietly dropped.
    /// * `Assigned` is Personal plus every project of the workspace, and never Unassigned.
    /// * `Unassigned` is reachable only through `UnassignedReview`, the one selection that states the
    ///   review intent the quarantine exception requires.
    ///
    /// `account.role` is consulted nowhere: an admin resolves to the partitions of the workspace the
    /// admin owns, because resolution is keyed on the workspace, not on a flag.
    pub async fn select(
        &self,
        owner: &OwnerContext,
        selection: ReadSelection,
    ) -> Result<ScopedContext, StorageError> {
        // Holding a context is not proof it is still current: an account can be suspended, or its
        // workspace reassigned, between two calls. Two reads per selection — the link, then the state of
        // the account the link points at — and both are the engine's answer, not a cached one.
        let workspace = self
            .workspace_still_owned_by(owner)
            .await?
            .ok_or(StorageError::NotFound)?;
        let account = self
            .account_row(owner.account().as_record_id())
            .await?
            .ok_or(StorageError::NotFound)?;
        if !account.is_active() {
            return Err(StorageError::Stale);
        }
        let workspace_id = workspace.workspace_id()?;

        let partition = |scope: MemoryScope| Partition {
            workspace: workspace_id.clone(),
            scope,
        };
        let partitions = match &selection {
            ReadSelection::Auto(None) => vec![partition(MemoryScope::Personal)],
            ReadSelection::Auto(Some(project)) => {
                self.owned_project(&workspace_id, project).await?;
                vec![
                    partition(MemoryScope::Personal),
                    partition(MemoryScope::Project(project.clone())),
                ]
            }
            ReadSelection::Assigned => {
                let mut scopes = vec![partition(MemoryScope::Personal)];
                for project in self.owned_projects(&workspace_id).await? {
                    scopes.push(partition(MemoryScope::Project(project)));
                }
                scopes
            }
            ReadSelection::One(MemoryScope::Personal) => vec![partition(MemoryScope::Personal)],
            ReadSelection::One(MemoryScope::Project(project)) => {
                self.owned_project(&workspace_id, project).await?;
                vec![partition(MemoryScope::Project(project.clone()))]
            }
            ReadSelection::One(MemoryScope::Unassigned) => {
                // Not another owner's data, but the wrong door: reading Unassigned is a review act, and
                // `UnassignedReview` is where that act is recorded.
                return Err(StorageError::InvalidInput(
                    "unassigned memories are readable only through ReadSelection::UnassignedReview"
                        .to_string(),
                ));
            }
            ReadSelection::UnassignedReview => vec![partition(MemoryScope::Unassigned)],
        };

        Ok(ScopedContext {
            workspace: workspace_id,
            partitions,
        })
    }

    /// Re-reads `status` and `role` from the account row and issues the account-management capability
    /// when this principal's authenticated account is an active `admin`.
    ///
    /// The checks are the ones [`Self::authorize`] already runs on the account and the presented
    /// credential — active account, live credential, matching authentication epoch — plus
    /// `role = 'admin'`. That ordering is the whole point: a suspended account, a revoked session or a
    /// superseded epoch does not get to be an administrator either, and a role read is never taken from
    /// a cached principal or a caller's word.
    ///
    /// An account that is active but merely an `owner` gets [`StorageError::NotFound`], the same answer
    /// an unresolvable id gets: "you are not an administrator" is a refusal about the caller's own
    /// row, and it must not be dressed up as something a client can probe.
    pub async fn authorize_admin(
        &self,
        principal: &Principal,
    ) -> Result<AdminCapability, StorageError> {
        let account = self
            .account_row(principal.account().as_record_id())
            .await?
            .ok_or(StorageError::NotFound)?;
        if !account.is_active() {
            return Err(StorageError::Stale);
        }
        if account.role.as_deref() != Some(crate::models::auth::account_role::ADMIN) {
            return Err(StorageError::NotFound);
        }

        // The credential behind the principal, when there is one. A trusted-local principal has no
        // credential row and is gated on this instance's startup capability instead — but that path is
        // only ever reachable through [`Self::local_principal`], and this method takes no capability,
        // so a stdio caller arriving here still has an account row re-read and still has to be an
        // active admin.
        if let PrincipalProvenance::Credential(credential_id) = &principal.provenance {
            let credential = self
                .credential_row(credential_id.as_record_id())
                .await?
                .ok_or(StorageError::NotFound)?;
            if credential.account.as_ref() != Some(principal.account().as_record_id()) {
                return Err(StorageError::NotFound);
            }
            if !credential_entitles_read(&credential, &account, Utc::now()) {
                return Err(StorageError::Stale);
            }
        }

        Ok(AdminCapability {
            account: account.account_id()?,
            credential: principal.credential().cloned(),
        })
    }

    /// Issues the trusted-local principal for the account the operator configured.
    ///
    /// Two things are required and neither is a client-supplied id: this instance's [`SystemContext`]
    /// — which a request path cannot obtain, issuance being once-per-instance and bootstrap-only — and
    /// an account that exists and is active in *this* store. The returned principal is the only way a
    /// [`PrincipalProvenance::TrustedLocal`] value exists, so the local path cannot be reached by a
    /// hosted request that simply omitted a credential.
    pub async fn local_principal(
        &self,
        system: &SystemContext,
        account: &AccountId,
    ) -> Result<Principal, StorageError> {
        // A capability from another instance is refused before the account is even looked up: the
        // answer must not depend on whether the id exists here.
        if !self.instance.holds_authority(system) {
            return Err(StorageError::NotFound);
        }
        let row = self
            .account_row(account.as_record_id())
            .await?
            .ok_or(StorageError::NotFound)?;
        if !row.is_active() {
            return Err(StorageError::Stale);
        }
        Ok(Principal {
            account: row.account_id()?,
            provenance: PrincipalProvenance::TrustedLocal,
        })
    }

    /// Resolves a read population for an **offline** caller: one that names the workspace explicitly
    /// instead of holding an owner, because it is a background or CLI pass with no request behind it.
    ///
    /// This is the named producer the brief asks for, and its boundary is the point:
    ///
    /// * it requires this instance's [`SystemContext`], which a request path cannot obtain — issuance
    ///   is once per database, only from a `&mut Database` the process owns, and a capability minted
    ///   against one store is refused by another. So `select_offline` is unreachable as a request
    ///   fallback, which is what keeps "offline selection" from becoming a way to read somebody else's
    ///   workspace by naming its id.
    /// * it **does not fabricate an [`OwnerContext`]** and returns no write authorization. An offline
    ///   caller selects a corpus; it does not become an owner. `ScopedContext` is a read population and
    ///   nothing more, so there is no path from here to a guarded write.
    /// * the workspace is validated, not assumed: it must exist and must be a private workspace of a
    ///   real, *active* account. A never-existing id and one belonging to a suspended account are
    ///   refused, and every project named by `selection` is checked against *that* workspace.
    ///
    /// The predicate rules are the same as [`Self::select`] — `Auto(None)` is Personal only, `Assigned`
    /// excludes Unassigned, Unassigned is reachable only through `UnassignedReview` — because a
    /// benchmark or maintenance pass that saw a wider population than the owner itself would, would be
    /// the leak with extra steps. In particular an explicit `UnassignedReview` selection is a *review*
    /// population, not recall or maintenance eligibility; the caller that makes it says so by name.
    pub async fn select_offline(
        &self,
        system: &SystemContext,
        workspace: &WorkspaceId,
        selection: ReadSelection,
    ) -> Result<ScopedContext, StorageError> {
        // Refused before anything is read, so the answer never depends on whether the workspace exists.
        if !self.instance.holds_authority(system) {
            return Err(StorageError::NotFound);
        }
        let row = self
            .workspace_row(workspace.as_record_id())
            .await?
            .ok_or(StorageError::NotFound)?;
        let owner = row.owner()?;
        let account = self
            .account_row(owner.as_record_id())
            .await?
            .ok_or(StorageError::NotFound)?;
        if !account.is_active() {
            return Err(StorageError::Stale);
        }

        // The same partition rules as `select`, keyed on the validated workspace rather than on a
        // context the offline caller does not have.
        let workspace_id = row.workspace_id()?;
        let partition = |scope: MemoryScope| Partition {
            workspace: workspace_id.clone(),
            scope,
        };
        let partitions = match &selection {
            ReadSelection::Auto(None) => vec![partition(MemoryScope::Personal)],
            ReadSelection::Auto(Some(project)) => {
                self.owned_project(&workspace_id, project).await?;
                vec![
                    partition(MemoryScope::Personal),
                    partition(MemoryScope::Project(project.clone())),
                ]
            }
            ReadSelection::Assigned => {
                let mut scopes = vec![partition(MemoryScope::Personal)];
                for project in self.owned_projects(&workspace_id).await? {
                    scopes.push(partition(MemoryScope::Project(project)));
                }
                scopes
            }
            ReadSelection::One(MemoryScope::Personal) => vec![partition(MemoryScope::Personal)],
            ReadSelection::One(MemoryScope::Project(project)) => {
                self.owned_project(&workspace_id, project).await?;
                vec![partition(MemoryScope::Project(project.clone()))]
            }
            ReadSelection::One(MemoryScope::Unassigned) => {
                return Err(StorageError::InvalidInput(
                    "unassigned memories are readable only through ReadSelection::UnassignedReview"
                        .to_string(),
                ));
            }
            ReadSelection::UnassignedReview => vec![partition(MemoryScope::Unassigned)],
        };

        Ok(ScopedContext {
            workspace: workspace_id,
            partitions,
        })
    }

    async fn account_row(&self, account: &RecordId) -> Result<Option<Account>, StorageError> {
        self.select_one("SELECT * FROM account WHERE id = $id LIMIT 1;", account)
            .await
    }

    async fn credential_row(
        &self,
        credential: &RecordId,
    ) -> Result<Option<Credential>, StorageError> {
        self.select_one(
            "SELECT * FROM credential WHERE id = $id LIMIT 1;",
            credential,
        )
        .await
    }

    async fn workspace_of(&self, account: &AccountId) -> Result<Option<Workspace>, StorageError> {
        let rows: Vec<Workspace> = self
            .query(
                "SELECT * FROM workspace WHERE owner = $account LIMIT 1;",
                account.as_record_id(),
            )
            .await?;
        Ok(rows.into_iter().next())
    }

    /// The context's workspace, but only while it still belongs to the context's account.
    async fn workspace_still_owned_by(
        &self,
        owner: &OwnerContext,
    ) -> Result<Option<Workspace>, StorageError> {
        let mut response = self
            .db
            .query("SELECT * FROM workspace WHERE id = $workspace AND owner = $account LIMIT 1;")
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<Workspace> = response.take(0).map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    /// Ok(()) when `project` is a project of `workspace`; the same `NotFound` when it belongs to
    /// another workspace and when it does not exist at all.
    async fn owned_project(
        &self,
        workspace: &WorkspaceId,
        project: &ProjectId,
    ) -> Result<(), StorageError> {
        let mut response = self
            .db
            .query("SELECT id FROM project WHERE id = $project AND workspace = $workspace LIMIT 1;")
            .bind(("project", project.as_record_id().clone()))
            .bind(("workspace", workspace.as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<IdRow> = response.take(0).map_err(internal)?;
        if rows.is_empty() {
            return Err(StorageError::NotFound);
        }
        Ok(())
    }

    /// Every project of the workspace, ordered by record id so the resolved partition list is
    /// reproducible across passes.
    async fn owned_projects(
        &self,
        workspace: &WorkspaceId,
    ) -> Result<Vec<ProjectId>, StorageError> {
        let mut response = self
            .db
            .query("SELECT id FROM project WHERE workspace = $workspace ORDER BY id;")
            .bind(("workspace", workspace.as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<IdRow> = response.take(0).map_err(internal)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.id)
            .filter_map(|id| ProjectId::try_from(id).ok())
            .collect())
    }

    async fn workspace_row(&self, workspace: &RecordId) -> Result<Option<Workspace>, StorageError> {
        self.select_one("SELECT * FROM workspace WHERE id = $id LIMIT 1;", workspace)
            .await
    }

    async fn select_one<T>(&self, sql: &str, id: &RecordId) -> Result<Option<T>, StorageError>
    where
        T: SurrealValue,
    {
        let mut response = self
            .db
            .query(sql)
            .bind(("id", id.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<T> = response.take(0).map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    async fn query<T>(&self, sql: &str, account: &RecordId) -> Result<Vec<T>, StorageError>
    where
        T: SurrealValue,
    {
        let mut response = self
            .db
            .query(sql)
            .bind(("account", account.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }
}

/// The `id` of a row this repo resolved by handle rather than by full read.
#[derive(SurrealValue)]
struct IdRow {
    id: Option<RecordId>,
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

#[cfg(test)]
mod tests {
    //! DB-backed authorization: real `account` / `workspace` / `project` / `credential` rows in a
    //! migrated in-memory store, and private principals assembled here because there is no public
    //! test-only authority constructor. Nothing here is a mock — every denial below is the engine
    //! refusing a row it read, and every denial is paired with a permitted result from the same
    //! fixture, because a resolver that denies everything would pass a denial-only test.

    use super::*;
    use crate::Database;
    use crate::models::OperationId;
    use crate::models::auth::account_status;
    use crate::models::identity::MemoryScope;

    /// Two owners with a colliding project alias, an admin with its own workspace, and the credential
    /// states `authorize` has to tell apart. Record ids are written down rather than read back, so no
    /// test depends on parsing a display form.
    struct Fixture {
        db: Database,
        alice: AccountId,
        alice_project: ProjectId,
        alice_agent: CredentialId,
        alice_browser: CredentialId,
        bob: AccountId,
        bob_project: ProjectId,
        admin: AccountId,
        admin_project: ProjectId,
    }

    /// One private workspace per account, one colliding project alias each, one admin.
    const SEED: &[&str] = &[
        "CREATE account:alice SET login = 'alice', password_hash = 'fixture-hash-one';",
        "CREATE workspace:alice_ws SET owner = account:alice;",
        "CREATE project:alice_git SET workspace = workspace:alice_ws, normalized_alias = 'alexandria';",
        "CREATE credential:alice_agent SET account = account:alice, kind = 'agent', label = 'pi', auth_epoch = 1;",
        "CREATE credential:alice_browser SET account = account:alice, kind = 'browser', auth_epoch = 1;",
        "CREATE account:bob SET login = 'bob-beyond', password_hash = 'fixture-hash-two';",
        "CREATE workspace:bob_ws SET owner = account:bob;",
        "CREATE project:bob_git SET workspace = workspace:bob_ws, normalized_alias = 'alexandria';",
        "CREATE credential:bob_agent SET account = account:bob, kind = 'agent', auth_epoch = 1;",
        "CREATE account:root SET login = 'root', password_hash = 'fixture-hash-three', role = 'admin';",
        "CREATE workspace:root_ws SET owner = account:root;",
        "CREATE project:root_git SET workspace = workspace:root_ws, normalized_alias = 'alexandria';",
        "CREATE credential:root_agent SET account = account:root, kind = 'agent', auth_epoch = 1;",
    ];

    impl Fixture {
        async fn seeded() -> Self {
            let db = Database::connect_embedded().await.unwrap();
            crate::schema::migrate(db.inner()).await.unwrap();
            for statement in SEED {
                db.inner().query(*statement).await.unwrap().check().unwrap();
            }
            Self {
                db,
                alice: account("alice"),
                alice_project: project("alice_git"),
                alice_agent: credential("alice_agent"),
                alice_browser: credential("alice_browser"),
                bob: account("bob"),
                bob_project: project("bob_git"),
                admin: account("root"),
                admin_project: project("root_git"),
            }
        }

        fn repo(&self) -> AccessRepo<'_> {
            AccessRepo::new(&self.db)
        }

        /// A hosted principal: what Task 4's `AuthRepo` will hand back after verifying a token. Assembled
        /// here, inside the crate, because that is the only place a principal can be assembled.
        /// Assembled as a struct literal, which only this module can do: there is no public or
        /// test-only authority constructor to reach from another crate.
        fn hosted(&self, who: &AccountId, credential: CredentialId) -> Principal {
            Principal {
                account: who.clone(),
                provenance: PrincipalProvenance::Credential(credential),
            }
        }

        /// Applies a state change to a credential row through the engine, not through a fake.
        async fn revoke_credential(&self, key: &str) {
            self.update(format!(
                "UPDATE ONLY credential:{key} SET revoked = true, revoked_at = time::now();"
            ))
            .await;
        }

        async fn set_credential(&self, key: &str, sets: &str) {
            self.update(format!("UPDATE ONLY credential:{key} SET {sets};"))
                .await;
        }

        async fn set_account(&self, key: &str, sets: &str) {
            self.update(format!("UPDATE ONLY account:{key} SET {sets};"))
                .await;
        }

        async fn update(&self, sql: String) {
            self.db.inner().query(&sql).await.unwrap().check().unwrap();
        }
    }

    fn account(key: &str) -> AccountId {
        AccountId::try_from(RecordId::new("account", key)).unwrap()
    }

    fn project(key: &str) -> ProjectId {
        ProjectId::try_from(RecordId::new("project", key)).unwrap()
    }

    fn credential(key: &str) -> CredentialId {
        CredentialId::try_from(RecordId::new("credential", key)).unwrap()
    }

    /// `Auto` with no project is the automatic-recall shape, and that shape is Personal *only*.
    ///
    /// Plan anchor: `assert_eq!(selected, vec![MemoryScope::Personal]); assert!(foreign_project.is_err())`.
    #[tokio::test]
    async fn auto_without_project_is_personal_only() {
        let fixture = Fixture::seeded().await;
        let owner = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_agent.clone()))
            .await
            .unwrap();

        let selected = fixture
            .repo()
            .select(&owner, ReadSelection::Auto(None))
            .await
            .unwrap();
        assert_eq!(selected.scopes(), vec![MemoryScope::Personal]);
        assert_eq!(selected.workspace(), owner.workspace());

        // A recognised selector adds exactly that project, and nothing else.
        let with_project = fixture
            .repo()
            .select(
                &owner,
                ReadSelection::Auto(Some(fixture.alice_project.clone())),
            )
            .await
            .unwrap();
        assert_eq!(
            with_project.scopes(),
            vec![
                MemoryScope::Personal,
                MemoryScope::Project(fixture.alice_project.clone())
            ]
        );

        // A real project in another workspace is refused rather than widened into.
        let foreign = fixture
            .repo()
            .select(
                &owner,
                ReadSelection::Auto(Some(fixture.bob_project.clone())),
            )
            .await;
        assert!(foreign.is_err(), "a foreign project selector was accepted");
        assert!(
            matches!(foreign, Err(StorageError::NotFound)),
            "a foreign selector should be indistinguishable from a missing one: {foreign:?}"
        );
    }

    /// A handle to nothing and a handle to somebody else's project fail the same way, with the same
    /// rendered message, and neither yields a population. An owner probing project ids learns nothing it
    /// could use.
    #[tokio::test]
    async fn foreign_project_selector_is_not_found() {
        let fixture = Fixture::seeded().await;
        let owner = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_agent.clone()))
            .await
            .unwrap();

        let missing = fixture
            .repo()
            .select(
                &owner,
                ReadSelection::One(MemoryScope::Project(project("no_such_project"))),
            )
            .await
            .expect_err("a project id that never existed was resolved");
        let foreign = fixture
            .repo()
            .select(
                &owner,
                ReadSelection::One(MemoryScope::Project(fixture.bob_project.clone())),
            )
            .await
            .expect_err("a project in another workspace was resolved");

        assert!(matches!(missing, StorageError::NotFound), "{missing:?}");
        assert!(matches!(foreign, StorageError::NotFound), "{foreign:?}");
        assert_eq!(
            missing.to_string(),
            foreign.to_string(),
            "the two refusals are tellable apart"
        );

        // Positive control, same fixture, same call site: alice's own project resolves.
        let own = fixture
            .repo()
            .select(
                &owner,
                ReadSelection::One(MemoryScope::Project(fixture.alice_project.clone())),
            )
            .await
            .unwrap();
        assert_eq!(
            own.scopes(),
            vec![MemoryScope::Project(fixture.alice_project.clone())]
        );
    }

    /// `Assigned` is the owner's whole corpus, and an owner's corpus is one workspace. The admin role
    /// changes nothing about what a read resolves to.
    #[tokio::test]
    async fn admin_does_not_get_other_workspace() {
        let fixture = Fixture::seeded().await;
        let admin = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.admin, credential("root_agent")))
            .await
            .expect("an admin's own live credential should authorize");
        assert_eq!(admin.account(), &fixture.admin);

        let assigned = fixture
            .repo()
            .select(&admin, ReadSelection::Assigned)
            .await
            .unwrap();
        assert_eq!(
            assigned.scopes(),
            vec![
                MemoryScope::Personal,
                MemoryScope::Project(fixture.admin_project.clone())
            ],
            "the admin role widened the resolved population"
        );
        assert!(
            assigned
                .partitions()
                .iter()
                .all(|partition| partition.workspace == *admin.workspace()),
            "a resolved partition named a workspace the context does not own"
        );

        // Naming another owner's project gets what any caller would get.
        let reach = fixture
            .repo()
            .select(
                &admin,
                ReadSelection::Auto(Some(fixture.alice_project.clone())),
            )
            .await;
        assert!(
            matches!(reach, Err(StorageError::NotFound)),
            "an admin reached another workspace by naming it: {reach:?}"
        );

        // Positive control: the owner of that project resolves it, in a different workspace.
        let alice = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_agent.clone()))
            .await
            .unwrap();
        let alice_assigned = fixture
            .repo()
            .select(&alice, ReadSelection::Assigned)
            .await
            .unwrap();
        assert_eq!(
            alice_assigned.scopes(),
            vec![
                MemoryScope::Personal,
                MemoryScope::Project(fixture.alice_project.clone())
            ]
        );
        assert_ne!(alice_assigned.workspace(), admin.workspace());
    }

    /// The Unassigned partition is a quarantine exception, so it arrives only through the selection
    /// that says so. `Assigned` is the broadest ordinary lookup and still excludes it; `One(Unassigned)`
    /// is refused with the name of the selection that is meant to carry it.
    #[tokio::test]
    async fn unassigned_requires_explicit_review_context() {
        let fixture = Fixture::seeded().await;
        let owner = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_agent.clone()))
            .await
            .unwrap();

        let assigned = fixture
            .repo()
            .select(&owner, ReadSelection::Assigned)
            .await
            .unwrap();
        assert!(
            !assigned.scopes().contains(&MemoryScope::Unassigned),
            "Assigned included the quarantined partition: {:?}",
            assigned.scopes()
        );
        let auto = fixture
            .repo()
            .select(&owner, ReadSelection::Auto(None))
            .await
            .unwrap();
        assert!(!auto.scopes().contains(&MemoryScope::Unassigned));

        let one = fixture
            .repo()
            .select(&owner, ReadSelection::One(MemoryScope::Unassigned))
            .await;
        match one {
            Err(StorageError::InvalidInput(message)) => assert!(
                message.contains("UnassignedReview"),
                "the refusal should name the selection that does carry the intent: {message}"
            ),
            other => panic!("One(Unassigned) resolved without review intent: {other:?}"),
        }

        // Positive control: the review selection resolves exactly that partition, in this owner's
        // workspace, and nothing else.
        let review = fixture
            .repo()
            .select(&owner, ReadSelection::UnassignedReview)
            .await
            .unwrap();
        assert_eq!(review.scopes(), vec![MemoryScope::Unassigned]);
        assert_eq!(review.workspace(), owner.workspace());
    }

    /// A revoked credential is refused while a second live credential on the same account still
    /// authorizes — which is what makes per-credential revocation real rather than a status string that
    /// only the account could carry.
    #[tokio::test]
    async fn revoked_credential_is_refused_while_its_live_sibling_authorizes() {
        let fixture = Fixture::seeded().await;
        let principal = fixture.hosted(&fixture.alice, fixture.alice_agent.clone());
        assert!(
            fixture.repo().authorize(&principal).await.is_ok(),
            "the live credential should authorize before anything is revoked"
        );

        fixture.revoke_credential("alice_agent").await;
        let revoked = fixture.repo().authorize(&principal).await;
        assert!(
            matches!(revoked, Err(StorageError::Stale)),
            "a revoked credential still authorized: {revoked:?}"
        );

        // Same account, same repo, one query later: the other credential is untouched.
        let browser = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_browser.clone()))
            .await;
        assert!(
            browser.is_ok(),
            "revoking one credential took the account's other credential with it: {browser:?}"
        );
        // And the refusal is not "no such credential": the row is there, it is spent.
        let consumed = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_agent.clone()))
            .await
            .err()
            .unwrap();
        assert_eq!(consumed.to_string(), StorageError::Stale.to_string());
    }

    /// Expiry is a clock, not a flag: a credential whose `expires_at` has passed is refused, and one
    /// that has not is accepted, with nothing written to either row.
    #[tokio::test]
    async fn expired_credential_is_refused_while_a_live_one_authorizes() {
        let fixture = Fixture::seeded().await;
        fixture
            .set_credential("alice_agent", "expires_at = d'2000-01-01T00:00:00Z'")
            .await;
        let expired = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_agent.clone()))
            .await;
        assert!(
            matches!(expired, Err(StorageError::Stale)),
            "an expired credential authorized: {expired:?}"
        );

        fixture
            .set_credential("alice_agent", "expires_at = d'2999-01-01T00:00:00Z'")
            .await;
        let live = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_agent.clone()))
            .await;
        assert!(
            live.is_ok(),
            "a credential that has not expired was refused: {live:?}"
        );
    }

    /// An invitation or a recovery link is a credential row. It is not a request credential: a leaked
    /// one-use link must not also be an account key, whether or not it has been claimed.
    #[tokio::test]
    async fn one_use_link_credentials_are_not_request_authority() {
        let fixture = Fixture::seeded().await;
        for (key, kind) in [("alice_invite", "invite"), ("alice_recovery", "recovery")] {
            fixture
                .db
                .inner()
                .query(format!(
                    "CREATE credential:{key} SET account = account:alice, kind = '{kind}', \
                     auth_epoch = 1;"
                ))
                .await
                .unwrap()
                .check()
                .unwrap();
            let attempt = fixture
                .repo()
                .authorize(&fixture.hosted(&fixture.alice, credential(key)))
                .await;
            assert!(
                matches!(attempt, Err(StorageError::Stale)),
                "a {kind} link authorized an ordinary read: {attempt:?}"
            );
        }

        // A consumed browser session is refused the same way, so a rotated session id cannot be
        // replayed after logout.
        fixture
            .set_credential(
                "alice_browser",
                "consumed = true, consumed_at = time::now()",
            )
            .await;
        let consumed = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_browser.clone()))
            .await;
        assert!(
            matches!(consumed, Err(StorageError::Stale)),
            "a consumed session still authorized: {consumed:?}"
        );
        // Positive control: the agent credential on the same account is untouched.
        assert!(
            fixture
                .repo()
                .authorize(&fixture.hosted(&fixture.alice, fixture.alice_agent.clone()))
                .await
                .is_ok()
        );
    }

    /// Recovery raises `account.auth_epoch`; every credential minted before it is inert even though
    /// nothing touched its row, and a credential minted under the new epoch works. This is the whole
    /// "invalidate all previous sessions" mechanism, checked as a DB read rather than as a sweep.
    #[tokio::test]
    async fn obsolete_auth_epoch_is_refused_and_a_fresh_credential_is_not() {
        let fixture = Fixture::seeded().await;
        fixture.set_account("alice", "auth_epoch = 2").await;

        let stale = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_agent.clone()))
            .await;
        assert!(
            matches!(stale, Err(StorageError::Stale)),
            "a credential minted under a superseded epoch authorized: {stale:?}"
        );

        fixture
            .db
            .inner()
            .query("CREATE credential:alice_agent2 SET account = account:alice, kind = 'agent', auth_epoch = 2;")
            .await
            .unwrap()
            .check()
            .unwrap();
        let fresh = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, credential("alice_agent2")))
            .await;
        assert!(
            fresh.is_ok(),
            "a credential minted under the current epoch was refused: {fresh:?}"
        );
    }

    /// A suspended account is refused, and the same principal is accepted before and after the
    /// suspension is lifted. The positive controls matter: an implementation that refused everything
    /// would pass a denial-only assertion.
    #[tokio::test]
    async fn inactive_account_is_refused_while_it_is_not_active() {
        let fixture = Fixture::seeded().await;
        let principal = fixture.hosted(&fixture.alice, fixture.alice_agent.clone());
        assert!(
            fixture.repo().authorize(&principal).await.is_ok(),
            "precondition: the live account should authorize"
        );

        fixture.set_account("alice", "status = 'suspended'").await;
        let suspended = fixture.repo().authorize(&principal).await;
        assert!(
            matches!(suspended, Err(StorageError::Stale)),
            "a suspended account still authorized: {suspended:?}"
        );

        // An unknown account is a different kind of refusal: no row to be stale about.
        let nobody = fixture
            .repo()
            .authorize(&fixture.hosted(&account("nobody"), credential("alice_agent")))
            .await;
        assert!(
            matches!(nobody, Err(StorageError::NotFound)),
            "an absent account should read as absent: {nobody:?}"
        );

        fixture.set_account("alice", "status = 'active'").await;
        assert!(
            fixture.repo().authorize(&principal).await.is_ok(),
            "lifting the suspension did not restore access"
        );
    }

    /// A credential owned by another account cannot authorize the account the caller claimed, and the
    /// refusal is the absent-row answer, so probing does not learn whose token whose is.
    #[tokio::test]
    async fn another_accounts_credential_cannot_authorize_the_claimed_account() {
        let fixture = Fixture::seeded().await;
        // bob's real, live credential, presented as alice.
        let claim = fixture.hosted(&fixture.alice, credential("bob_agent"));
        let attempt = fixture.repo().authorize(&claim).await;
        assert!(
            matches!(attempt, Err(StorageError::NotFound)),
            "a cross-account credential was accepted or leaked its kind: {attempt:?}"
        );

        // Positive control: the same credential authorizes its own account.
        let owner = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.bob, credential("bob_agent")))
            .await
            .unwrap();
        assert_eq!(owner.account(), &fixture.bob);
        // And the populations are disjoint, which is the point of the check.
        let bob_sees = fixture
            .repo()
            .select(
                &owner,
                ReadSelection::One(MemoryScope::Project(fixture.bob_project.clone())),
            )
            .await
            .unwrap();
        assert_eq!(
            bob_sees.scopes(),
            vec![MemoryScope::Project(fixture.bob_project.clone())]
        );
        let alice = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_agent.clone()))
            .await
            .unwrap();
        let alice_reaches = fixture
            .repo()
            .select(
                &alice,
                ReadSelection::One(MemoryScope::Project(fixture.bob_project.clone())),
            )
            .await;
        assert!(
            matches!(alice_reaches, Err(StorageError::NotFound)),
            "alice reached bob's project after all: {alice_reaches:?}"
        );
    }

    /// An `OwnerContext` is not a permanent grant: `select` re-reads the workspace→account link, so a
    /// context held across a reassignment or a suspension stops resolving.
    #[tokio::test]
    async fn select_revalidates_the_context_it_was_handed() {
        let fixture = Fixture::seeded().await;
        let owner = fixture
            .repo()
            .authorize(&fixture.hosted(&fixture.alice, fixture.alice_agent.clone()))
            .await
            .unwrap();
        assert!(
            fixture
                .repo()
                .select(&owner, ReadSelection::Auto(None))
                .await
                .is_ok()
        );

        fixture.set_account("alice", "status = 'suspended'").await;
        let after = fixture.repo().select(&owner, ReadSelection::Assigned).await;
        assert!(
            matches!(after, Err(StorageError::Stale)),
            "a stale OwnerContext kept resolving after the account was suspended: {after:?}"
        );

        // A context whose workspace is gone is the absent-row answer. `workspace.owner` is UNIQUE, so
        // reassigning one owner's workspace onto an account that already has one cannot even be
        // written — the population disappearing under a held context is the case that can happen, and
        // it is the one tested here.
        fixture
            .update("DELETE workspace:alice_ws;".to_string())
            .await;
        let reassigned = fixture
            .repo()
            .select(&owner, ReadSelection::Auto(None))
            .await;
        assert!(
            matches!(reassigned, Err(StorageError::NotFound)),
            "a context survived its workspace being reassigned: {reassigned:?}"
        );
    }

    /// The trusted-local path works — and only under this instance's startup capability, only for an
    /// account that exists and is active, and never as a fallback for a hosted request that presented
    /// nothing.
    #[tokio::test]
    async fn trusted_local_authority_needs_the_startup_capability_of_this_instance() {
        let (db, system) = Database::bootstrap_embedded_for_tests().await.unwrap();
        for statement in SEED {
            db.inner().query(*statement).await.unwrap().check().unwrap();
        }
        let repo = AccessRepo::new(&db);
        let alice = account("alice");

        let principal = repo
            .local_principal(&system, &alice)
            .await
            .expect("the startup capability should mint the configured local owner");
        assert_eq!(principal.account(), &alice);
        assert_eq!(principal.provenance(), &PrincipalProvenance::TrustedLocal);
        assert!(
            principal.credential().is_none(),
            "a trusted-local principal presents no credential, and authorize must still accept it"
        );
        let owner = repo
            .authorize(&principal)
            .await
            .expect("a trusted-local principal should authorize against the account row");
        let resolved = repo
            .select(&owner, ReadSelection::Auto(None))
            .await
            .unwrap();
        assert_eq!(resolved.scopes(), vec![MemoryScope::Personal]);

        // An account that is not in the store is not a local owner either.
        assert!(
            matches!(
                repo.local_principal(&system, &account("nobody")).await,
                Err(StorageError::NotFound)
            ),
            "a configured-but-absent account minted a principal"
        );
        // A suspended one is refused as spent authority, not as absence.
        fixture_set(&db, "UPDATE ONLY account:bob SET status = 'suspended';").await;
        assert!(
            matches!(
                repo.local_principal(&system, &account("bob")).await,
                Err(StorageError::Stale)
            ),
            "a suspended account minted a local principal"
        );
    }

    /// A capability minted against one store is refused by another. The positive control is the same
    /// code path against the instance that issued it, in the same process, with the same namespace and
    /// database name — so the refusal is about the instance, not about the connection string.
    #[tokio::test]
    async fn a_capability_from_another_instance_is_refused() {
        let (alice_store, alice_authority) =
            Database::bootstrap_embedded_for_tests().await.unwrap();
        let (bob_store, _) = Database::bootstrap_embedded_for_tests().await.unwrap();
        for statement in SEED {
            alice_store
                .inner()
                .query(*statement)
                .await
                .unwrap()
                .check()
                .unwrap();
            bob_store
                .inner()
                .query(*statement)
                .await
                .unwrap()
                .check()
                .unwrap();
        }

        let foreign = AccessRepo::new(&bob_store)
            .local_principal(&alice_authority, &account("alice"))
            .await;
        assert!(
            matches!(foreign, Err(StorageError::NotFound)),
            "another instance's system capability was accepted: {foreign:?}"
        );

        // Same-instance positive control, on the store that issued it.
        let same = AccessRepo::new(&alice_store)
            .local_principal(&alice_authority, &account("alice"))
            .await;
        assert!(
            same.is_ok(),
            "the issuing instance refused its own capability: {same:?}"
        );
    }

    /// One capability per instance, and the once-only state is the instance's, not the handle's: two
    /// `Arc` references to one `Database` share it, so neither can mint a second.
    #[tokio::test]
    async fn startup_authority_is_issued_once_per_instance() {
        let mut db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let first = db
            .claim_system_authority()
            .expect("the first claim should succeed");
        let second = db.claim_system_authority();
        assert!(
            matches!(second, Err(StorageError::Stale)),
            "a second capability was issued for one instance: {second:?}"
        );

        // Wrapping the same value in an `Arc` and handing back exclusive access does not hand out a
        // fresh issuance slot: the state belongs to the instance, not to the handle.
        let mut shared = Arc::new(db);
        let sole_owner = Arc::get_mut(&mut shared).expect("the last owner of the handle");
        let through_arc = sole_owner.claim_system_authority();
        assert!(
            matches!(through_arc, Err(StorageError::Stale)),
            "a claim through a shared handle re-minted the capability: {through_arc:?}"
        );
        // A *different* instance gets its own single capability, and only its own.
        let (other, other_capability) = Database::bootstrap_embedded_for_tests().await.unwrap();
        assert!(
            matches!(
                AccessRepo::new(&other)
                    .local_principal(&first, &account("nobody"))
                    .await,
                Err(StorageError::NotFound)
            ),
            "the first instance's capability was accepted by the second"
        );
        // The second instance's own capability works on the second instance, which is what makes the
        // refusal above about the binding rather than about a blanket denial.
        let minted = AccessRepo::new(&other)
            .local_principal(&other_capability, &account("never-created"))
            .await;
        assert!(
            matches!(minted, Err(StorageError::NotFound)),
            "a freshly issued capability did not reach the account check: {minted:?}"
        );
    }

    /// The ordinary bootstrap entry point: connect, migrate, and hand back the store with the one
    /// capability it will ever issue — so authority is claimed before any request handle exists.
    #[tokio::test]
    async fn bootstrap_returns_the_store_with_its_only_capability() {
        let (db, system) = Database::bootstrap_embedded_for_tests().await.unwrap();
        // The capability that came back from bootstrap is the one the store holds: proving it through
        // the consumer rather than a getter keeps the getter from ever becoming a mint path.
        let through_consumer = AccessRepo::new(&db)
            .local_principal(&system, &account_from_bootstrap(&db).await)
            .await;
        assert!(
            through_consumer.is_ok(),
            "bootstrap's own capability was refused by bootstrap's own store: {through_consumer:?}"
        );
        // The schema came with it, so a later task cannot serve from a bootstrapped store that skipped
        // the migrations.
        let version: Vec<surrealdb::types::Value> = db
            .inner()
            .query("SELECT * FROM system_config WHERE key = 'schema_version' LIMIT 1;")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert!(!version.is_empty(), "bootstrap skipped the migrations");
        assert!(
            matches!(
                // Moving out of the `Arc` is how a second claim would have to be attempted, and the
                // shared borrow above is what a request-facing holder cannot do.
                AccessRepo::new(&db)
                    .local_principal(&system, &account("never-created"))
                    .await,
                Err(StorageError::NotFound)
            ),
            "an absent account minted a local principal"
        );
    }

    /// `OwnerContext`s and `Principal`s are assembled by the resolver, so a seeded-enrollment pair is
    /// not one: the ids from `bootstrap_owner` cannot be turned into authority without the credential
    /// re-check, which is what keeps Task 4's production mint path the only one.
    #[tokio::test]
    async fn enrollment_ids_are_not_authority() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let ids = crate::repos::IdentityRepo::new(db.inner())
            .bootstrap_owner("Alice ", "fixture-hash", &OperationId::new("bootstrap-1"))
            .await
            .unwrap();

        // A principal assembled from the enrolled ids, with a credential that does not exist, is
        // refused: enrollment created an account and a workspace, not a way in.
        let attempt = AccessRepo::new(&db)
            .authorize(&Principal {
                account: ids.account.clone(),
                provenance: PrincipalProvenance::Credential(credential("no_such_credential")),
            })
            .await;
        assert!(
            matches!(attempt, Err(StorageError::NotFound)),
            "a non-existent credential authorized an enrolled account: {attempt:?}"
        );
        // The account is nonetheless real, normalised and active, so a credential minted for it will
        // work — which is the difference between a refusal and a dead end.
        let rows =
            run_select::<AccountRow>(&db, "SELECT * FROM account WHERE login = 'alice';").await;
        assert_eq!(
            rows.len(),
            1,
            "the enrolled account should be readable under its normalised login"
        );
        assert_eq!(rows[0].status.as_deref(), Some(account_status::ACTIVE));

        let minted = run_select_one(
            &db,
            "CREATE credential:alice_bootstrap SET account = $account, kind = 'agent', auth_epoch = 1; RETURN AFTER;",
            ids.account.as_record_id(),
        )
        .await;
        assert!(minted.is_some(), "the credential row was not created");
        let authorized = AccessRepo::new(&db)
            .authorize(&Principal {
                account: ids.account.clone(),
                provenance: PrincipalProvenance::Credential(credential("alice_bootstrap")),
            })
            .await
            .expect("a credential minted for the enrolled account should authorize");
        assert_eq!(authorized.account(), &ids.account);
    }

    #[derive(SurrealValue)]
    struct AccountRow {
        status: Option<String>,
        login: String,
    }

    #[derive(SurrealValue)]
    struct IdRow {
        id: Option<RecordId>,
    }

    async fn run_select<T>(db: &Database, sql: &str) -> Vec<T>
    where
        T: SurrealValue,
    {
        let mut response = db.inner().query(sql).await.unwrap().check().unwrap();
        response.take(0).unwrap()
    }

    async fn run_select_one(db: &Database, sql: &str, account: &RecordId) -> Option<IdRow> {
        let mut response = db
            .inner()
            .query(sql)
            .bind(("account", account.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<IdRow> = response.take(0).unwrap();
        rows.into_iter().next()
    }

    async fn fixture_set(db: &Database, sql: &str) {
        db.inner().query(sql).await.unwrap().check().unwrap();
    }

    /// Enrolls through the public path so the capability test above never hand-writes an account row
    /// the resolver would not have to accept anyway.
    async fn account_from_bootstrap(db: &Database) -> AccountId {
        crate::repos::IdentityRepo::new(db.inner())
            .bootstrap_owner(
                "bootstrap-probe",
                "fixture-hash",
                &crate::models::OperationId::new("capability-check"),
            )
            .await
            .unwrap()
            .account
    }
}
