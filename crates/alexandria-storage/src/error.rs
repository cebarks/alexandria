//! The error type new storage boundaries return.
//!
//! Repositories that still hand back `anyhow::Result` keep doing so until the task that converts
//! their boundary; `StorageError` implements `std::error::Error` and crosses into `anyhow` as
//! itself, so nothing upstream has to classify a message to recover the kind.

use thiserror::Error;

/// A storage failure, classified by what the caller may conclude rather than by the database text
/// behind it.
///
/// Three of the classes carry no payload on purpose:
///
/// - `NotFound` is what both a missing row and a row the caller has no claim to turn into, so the
///   outward response cannot be used to probe which of the two it was. Carrying the id would make
///   the two distinguishable again, which is exactly what the scoped read paths promise not to do.
/// - `Stale` is a semantic conflict — the world moved away from the revision the caller expected —
///   while `RetryableBookkeepingConflict` is a transient race over derived bookkeeping that the
///   same request may retry. Merging them would tell a caller to retry an edit whose approved
///   effects no longer exist, or to give up on a lock that clears by itself.
/// - `Internal` keeps the underlying failure for the log but renders a fixed message, so driver
///   text, SQL and private payloads stay out of anything a client can be shown.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("not found")]
    NotFound,
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("stale")]
    Stale,
    #[error("retryable bookkeeping conflict")]
    RetryableBookkeepingConflict,
    #[error("quota exceeded")]
    Quota,
    #[error("unavailable")]
    Unavailable,
    #[error("internal storage error")]
    Internal(#[from] anyhow::Error),
}

/// The one classified cause `Internal` is allowed to carry a decision about.
///
/// A lost owner workspace is *not* a client-facing class: the caller did nothing wrong, the row it
/// named is not merely absent, and nothing may be inferred about which owner is affected. It stays
/// [`StorageError::Internal`] for that reason, and `Internal`'s own `Display` is the fixed
/// `"internal storage error"` — which is precisely the problem this type solves: an operator reading a
/// log needs to tell "the store lost an owner's private state" (a backup/recovery decision) apart
/// from a driver hiccup, and a string comparison of a rendered error is not a classification.
///
/// So this is a *typed* marker: wrapped in `anyhow`, and recovered by the service boundary with
/// `anyhow::Error::downcast_ref::<OwnerWorkspaceLost>()` — that is what [`StorageError::is_owner_workspace_lost`]
/// does. Nothing on either side inspects an error message to decide which class a failure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerWorkspaceLost;

impl std::fmt::Display for OwnerWorkspaceLost {
    /// Operator-facing log text. Deliberately names no table, no id and no column: this string reaches
    /// the log, never the client, and the client-visible wording is the service boundary's own.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the private state an enrolled owner is entitled to is missing from the store; \
             this is a backup-or-recovery decision, not a request to retry",
        )
    }
}

impl std::error::Error for OwnerWorkspaceLost {}

impl StorageError {
    /// `Internal` carrying [`OwnerWorkspaceLost`] as its cause — the one internal failure the service
    /// boundary is allowed to distinguish, and only by downcasting to that type.
    pub fn owner_workspace_lost() -> Self {
        StorageError::Internal(anyhow::Error::new(OwnerWorkspaceLost))
    }

    /// Whether this internal failure *is* the lost-owner-workspace case, by type.
    ///
    /// The check is `anyhow`'s typed downcast over the cause chain, never a rendered-message match:
    /// a driver that phrases a write conflict like this one must not be reclassified as a backup
    /// decision, and a future rewording of either string must not silently change the answer.
    pub fn is_owner_workspace_lost(&self) -> bool {
        match self {
            StorageError::Internal(cause) => cause.downcast_ref::<OwnerWorkspaceLost>().is_some(),
            _ => false,
        }
    }
}
