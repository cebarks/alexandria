//! The one prepare→apply retry loop, shared by every caller that commits derived bookkeeping.
//!
//! MCP's apply-service wrapper and the classification batch driver both need the same
//! policy: a preparation re-reads the derived data (cluster guards, populations, means) the transaction
//! will defend, `apply` commits it, and only [`StorageError::RetryableBookkeepingConflict`] — the class
//! that means "the bookkeeping moved under a lawful change, nothing was written" — earns another
//! preparation. `Stale`, `NotFound`, an invalid request and every engine refusal return on the first
//! attempt, because a second try at a wrong change is not a retry but a stall.
//!
//! The effect-set comparison is the loop's other half. The *approved* effects are fixed when the caller
//! pins its intent; a preparation may refresh the numbers behind those labels, and nothing else. A retry
//! that would widen or change the approved effect set fails loudly here instead of quietly becoming a
//! different operation, so the bound below is a bound on refreshing the *same* intent.
//!
//! This is deliberately not a framework: one constant, one function, no trait surface. Each caller
//! supplies the preparation for its already-approved intent; the retry policy stays here.

use crate::access::MutationContext;
use crate::error::StorageError;
use crate::models::identity::OperationId;
use crate::models::mutation::{AppliedChange, ExpectedState, PreparedChangeSet};
use crate::repos::mutation_repo::MutationRepo;

/// The approved per-entry bound: one initial preparation and at most two further preparations.
/// Exhaustion returns the bookkeeping conflict without starting a fourth preparation.
pub const MAX_PREPARATIONS: usize = 3;

/// Prepare and commit one change, retrying only the derived-bookkeeping conflict.
///
/// `prepare` is called once per attempt and must rebuild the change from live state; its errors
/// — including a preparation-side [`StorageError::RetryableBookkeepingConflict`] — return immediately,
/// because a preparation that cannot even read consistent derived data has nothing to retry against.
/// After [`MAX_PREPARATIONS`] attempts the conflict is returned to the caller, who decides whether it is
/// a refusal to report or a stop to record.
pub async fn commit_prepared<P, F>(
    mutation: &MutationRepo<'_>,
    access: &MutationContext,
    operation: &OperationId,
    expected: &[ExpectedState],
    approved_effects: &[String],
    mut prepare: P,
) -> Result<AppliedChange, StorageError>
where
    P: FnMut() -> F + Send,
    F: std::future::Future<Output = Result<PreparedChangeSet, StorageError>> + Send,
{
    let mut preparations = 0;
    while preparations < MAX_PREPARATIONS {
        preparations += 1;
        let change = prepare().await?;
        if change.effects != approved_effects {
            // Reached only by an edit that let a preparation re-decide the approved shape; it fails
            // closed, because the honest answer to a change that no longer matches its own approved
            // intent is a refusal, not a quieter variant of the operation.
            return Err(StorageError::Stale);
        }
        match mutation.apply(access, operation, expected, &change).await {
            // Only the derived class continues. See the module docs for why nothing else may.
            Ok(applied) => return Ok(applied),
            Err(StorageError::RetryableBookkeepingConflict) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(StorageError::RetryableBookkeepingConflict)
}
