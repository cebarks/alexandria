use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Cluster {
    pub id: Option<RecordId>,
    pub label: Option<String>,
    pub centroid: Vec<f32>,
    pub depth: i64,
    pub created_at: Option<DateTime<Utc>>,
}

/// Where one of the caller's clusters stands, for the version a recall handle records.
///
/// A value type, not a repository: it names no DB access, which is why it lives with the other
/// row-shaped models rather than beside the scoped cluster reads that produce it.
///
/// Two parts, because they answer the same question on two different clocks.
///
/// `membership_revision` is v009's own guard on the member set — the column the schema says every
/// membership-edge writer must increment inside its own transaction — so it is the honest thing to stamp
/// a snapshot claim with, and the half that will still mean something once Task 12's cluster writers
/// start moving it. `None` means the row carries no guard at all, which is reported rather than read as
/// zero: "no membership write has touched this row" and "that writer has run to zero" are the same
/// today and different the moment it lands.
///
/// `members` is the live count the caller's own traversal saw, and it is the half that moves on the paths
/// that exist today, because the store path links a member with `ClusterRepo::add_member` and does not
/// touch the guard. A handle carrying neither would be a locator pretending to be a snapshot.
///
/// `Default` is the version a handle carries when its issuer read nothing: no guard, no members. It is
/// reachable from a real path — a handle minted before the field existed decodes to it — and it is a
/// claim that nothing was read, never a claim of version zero.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterVersion {
    pub membership_revision: Option<i64>,
    pub members: usize,
}

/// The mean a population of member vectors implies: the one arithmetic behind every derived centroid.
///
/// One home, in the crate that owns the writes, because two implementations could disagree on the case
/// where agreement actually matters — the dimension mismatch, where a plausible-but-truncated centroid
/// is worse than a refusal. The ranking path's [`alexandria_engine::clusters::centroid`] is now a call
/// here, so the engine's mean and the storage writer's mean are the same function; the dependency arrow
/// (engine -> storage) already permits it, and storage's scope-move writer needs the number inside its
/// own transaction, where no engine call can reach.
///
/// Dimension handling: every embedding in a cluster was produced by one locked model, so a member of
/// another length is not a smaller cluster but a corrupt population, and averaging a short member against
/// a long one would silently truncate the tail of the centroid. The caller sees the refusal instead of a
/// plausible number. An empty population has no centre (`None`), not a zero vector to be mistaken for one.
pub fn mean_centroid(population: &[&[f32]]) -> Option<Vec<f32>> {
    let first = population.first()?;
    let dimensions = first.len();
    if dimensions == 0 {
        return None;
    }
    let mut sums = vec![0.0f64; dimensions];
    for embedding in population {
        if embedding.len() != dimensions {
            return None;
        }
        for (sum, value) in sums.iter_mut().zip(*embedding) {
            *sum += f64::from(*value);
        }
    }
    let count = population.len() as f64;
    Some(sums.into_iter().map(|sum| (sum / count) as f32).collect())
}
