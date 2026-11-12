//! Identity, scope and partition data. Pure types — no queries, no schema, no authority.
//!
//! Two families live here, and the difference between them is the point.
//!
//! The typed record IDs ([`AccountId`], [`WorkspaceId`], [`ProjectId`], [`FactId`],
//! [`CredentialId`]) are *validated handles*: `TryFrom<RecordId>` is the only way to make one, so a
//! value of type [`ProjectId`] is proof that the record id came from a `project` row. They keep the
//! [`RecordId`] — and therefore the key — untouched, because a numeric, UUID or quoted opaque key
//! all mean different things and none of them survive a trip through a display string.
//!
//! [`ScopeKey`], [`ReadSelection`] and [`Partition`] are *untrusted request data*: the caller's
//! words about what it wants to read. Nothing converts one into a [`MemoryScope`] implicitly,
//! because possession of a selector is not possession of the rows it names — resolution happens
//! against an authorized context, in the repo layer that owns the lookup.
//!
//! The typed IDs implement no `Serialize`/`Deserialize`, and that is a constraint rather than an
//! omission: a derived `Deserialize` would delegate to [`RecordId`] and accept *any* table, which
//! is exactly the check the newtype exists to make. A later task that needs one at a JSON boundary
//! has to write that conversion, and it has to keep the table check. The untrusted shapes carry no
//! serialization yet either, but for the ordinary reason — nothing consumes it before the task that
//! resolves a request, and adding it there cannot weaken a typed ID.

use crate::error::StorageError;
use surrealdb::types::RecordId;

/// Defines one opaque typed record ID: a wrapper that can only be built from a `RecordId` whose
/// table it recognises. All five ID types in this module come from here, which is why one
/// `compile_fail` example on [`AccountId`] is enough to pin the serde property for the family —
/// there is no per-type derive list to leak.
macro_rules! typed_record_id {
    ($(#[$doc:meta])* $name:ident, $table:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(RecordId);

        impl $name {
            /// The validated handle to bind into a query. Binding the typed newtype itself would
            /// serialize it, and the table check — the thing this type exists to carry — is a
            /// property of the record id, not of its rendering.
            pub fn as_record_id(&self) -> &RecordId {
                &self.0
            }
        }

        impl TryFrom<RecordId> for $name {
            type Error = StorageError;

            fn try_from(id: RecordId) -> Result<Self, Self::Error> {
                // The comparison is against the parsed record id's own table field. Building a
                // `table:key` string (or any SQL rendering) to compare — or to interpolate into a
                // query — would let a table name that only *looks* right in string form through.
                if id.table.as_str() != $table {
                    return Err(StorageError::InvalidInput(format!(
                        "expected a record of table `{}`",
                        $table
                    )));
                }
                Ok(Self(id))
            }
        }
    };
}

typed_record_id! {
    /// A row of the `account` table.
    ///
    /// ```
    /// use alexandria_storage::models::identity::AccountId;
    /// use surrealdb::types::RecordId;
    ///
    /// let account = AccountId::try_from(RecordId::new("account", "a_1")).unwrap();
    /// assert_eq!(account.as_record_id().table.as_str(), "account");
    /// ```
    ///
    /// Rejecting a foreign table is runtime behaviour, so `tests/identity_types_test.rs` pins it.
    /// What the type system has to pin is that no deserialisation path skips that check:
    ///
    /// ```compile_fail
    /// use alexandria_storage::models::identity::AccountId;
    ///
    /// fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
    /// requires_deserialize::<AccountId>();
    /// ```
    AccountId,
    "account"
}

typed_record_id! {
    /// A row of the `workspace` table — one account's private workspace.
    WorkspaceId,
    "workspace"
}

typed_record_id! {
    /// A row of the `project` table, inside exactly one workspace.
    ///
    /// A project alias is not an id: two workspaces may have projects with the same name, so only
    /// this handle distinguishes them.
    ProjectId,
    "project"
}

typed_record_id! {
    /// A row of the `fact` table — one stored memory.
    FactId,
    "fact"
}

typed_record_id! {
    /// A row of the `credential` table: browser, agent, invitation or recovery.
    CredentialId,
    "credential"
}

/// A semantic revision, used as the compare-and-swap guard on writes that carry an expectation
/// (`expected == actual`, otherwise [`StorageError::Stale`]).
///
/// Ordering matters as much as equality: a revision guard has to say which side moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Version(u64);

impl Version {
    /// The revision a record without one starts at.
    pub const ZERO: Version = Version(0);

    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// The idempotency key of one semantic operation, unique within a workspace rather than globally —
/// so it is data the caller supplies, validated where it is written.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperationId(String);

impl OperationId {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a caller *says* it wants to read. Untrusted text, with no validation and no authority.
///
/// ```
/// use alexandria_storage::models::identity::ScopeKey;
///
/// let requested = ScopeKey::Project("alexandria".to_string());
/// assert_eq!(requested, ScopeKey::Project("alexandria".to_string()));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScopeKey {
    Personal,
    Project(String),
    Unassigned,
}

/// The resolved scope of a memory: the same three shapes, with the project named by a validated
/// handle rather than by text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MemoryScope {
    Personal,
    Project(ProjectId),
    Unassigned,
}

/// How a read request asks its population to be chosen. `Auto` is the automatic-recall shape
/// (personal plus the current project, project only when the caller recognised one); `Assigned` is
/// everything the context owns; `One` is an explicit single scope; `UnassignedReview` is the
/// owner's explicit look at quarantined unassigned rows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReadSelection {
    Auto(Option<ProjectId>),
    Assigned,
    One(MemoryScope),
    UnassignedReview,
}

/// One `(workspace, scope)` population — the unit maintenance, candidate selection and row budgets
/// never cross.
///
/// ```
/// use alexandria_storage::models::identity::{MemoryScope, Partition, WorkspaceId};
/// use surrealdb::types::RecordId;
///
/// let workspace = WorkspaceId::try_from(RecordId::new("workspace", "w_1")).unwrap();
/// let partition = Partition { workspace: workspace.clone(), scope: MemoryScope::Personal };
/// assert_eq!(partition.workspace, workspace);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Partition {
    pub workspace: WorkspaceId,
    pub scope: MemoryScope,
}
