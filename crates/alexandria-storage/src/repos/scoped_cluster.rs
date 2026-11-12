//! The account-bound cluster reads a request-facing handler needs.
//!
//! Reads are bound to a **population**, not only to a workspace: [`ScopedClusterRepo::get_members`]
//! filters candidate facts through the same [`ScopeFilter`] the fact reads use, so `recall(scope:
//! "personal")` serves personal rows and nothing else. A cluster row itself carries only
//! `account`/`workspace` (v009), so cluster *candidates* stay workspace-scoped; what this pass controls
//! is which members a candidate may surface, and a candidate with no eligible member surfaces nothing.
//! Partitioned cluster *maintenance* — re-clustering rows so a cluster's membership is itself
//! partition-faithful — remains Task 12.
//!
//! Two leaks live in `ClusterRepo` as the request path uses it:
//!
//! * **A `scope_handle` was a bearer capability.** `do_recall` decoded a cluster id straight out of the
//!   handle string and called `get_members` on it, with nothing checking that the caller owned that
//!   cluster. Anyone who obtained or guessed another account's cluster id read that account's memory
//!   contents — the handle is not signed, so "it came from a previous response" is not an authority
//!   check.
//! * **Cluster labels and member counts are content.** A cluster name is a phrase about what its
//!   memories are about, and a member count is a fact about how much another account stored. Both leak
//!   through `list`/`list_with_counts` even when no memory body is returned, which is why the label case
//!   is asserted in `scoped_request_tools_test::recall_surfaces_no_foreign_cluster_label_or_membership`
//!   rather than folded into a memory-content assertion.
//!
//! Reads are therefore owner-stamped: a cluster row carries `account`/`workspace` (v009), and both the
//! cluster and each member fact are matched against the bound account and workspace before anything is
//! returned. A foreign cluster id and a nonexistent one both resolve to `None`, so a replayed handle
//! cannot be used to probe which of the two it was.
//!
//! One write *is* here: [`ScopedClusterRepo::create`]. A store or import whose vector matches no existing
//! cluster has to make one, and the unscoped `ClusterRepo::create` it used to call stamps
//! `label`/`centroid`/`depth` and nothing else — so every cluster the request path created was invisible
//! to the account that created it, and the reads above could never return it. An ownership-bearing row is
//! stamped where it is created, which is this seam.
//!
//! Two reads are here for the recall handle rather than for a row: [`ScopedClusterRepo::revisions`] and
//! [`ScopedClusterRepo::revision`] surface v009's `membership_revision` guard, which is the number a
//! `scope_handle` stamps so a replay can tell whether the cluster moved since it was named. They are
//! reads only: the writer that *increments* the guard is a membership-edge writer, and those stay with
//! Task 12's partitioned maintenance; request-path journaled writers already increment these guards.
//!
//! Splits, merges and maintenance restructuring remain Task 12's surface. Request-path destinations use
//! [`ScopedClusterRepo::partition_candidates`] or [`ScopedClusterRepo::candidates_for_partition`], not
//! the workspace-wide compatibility list. Their journaled membership and centroid writes are guarded
//! inside `MutationRepo`; the interim unscoped arm retains its legacy `ClusterRepo` writes.

use crate::access::{OwnerContext, ScopedContext};
use crate::error::StorageError;
use crate::models::{Cluster, Fact, MemoryScope, Partition};
use crate::repos::scope::{Bind, GuardedWrite, ScopeFilter, guarded_run};
use chrono::{DateTime, Utc};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue, ToSql};

/// The live-member predicate, spelled once.
///
/// `quarantined_at = NONE` and never `IS NOT NULL`/`!= NULL`: on this engine both of those are satisfied
/// by `NONE`, so they would filter nothing and quarantine would stop meaning anything.
const LIVE_MEMBER: &str = "deleted = false AND quarantined_at = NONE";

pub struct ScopedClusterRepo<'a> {
    db: &'a Surreal<Any>,
    /// The account a created cluster is stamped with, and the authority its guarded create re-asserts.
    ///
    /// Held alongside the read filter rather than as an `Option` some methods consult: a cluster this repo
    /// creates and a cluster it lists must not disagree about who owns it, and there is no way to build
    /// the one without the other.
    owner: &'a OwnerContext,
    /// The population this caller resolved to: its workspace *and* its partitions.
    ///
    /// The shared [`ScopeFilter`] rather than a cluster-local struct holding a workspace, because a
    /// member read carrying a different population predicate from the fact reads would let `recall` and
    /// `retrieve_memories` disagree while both advertise one `scope` word.
    read: ScopeFilter,
}

impl<'a> ScopedClusterRepo<'a> {
    /// Binds the reads to the population a request resolved to and the write to the owner that population
    /// was resolved from.
    ///
    /// The same shape [`ScopedMemoryRepo::new`](super::ScopedMemoryRepo::new) uses: reads select a
    /// partition, writes stamp an account, and both come from one admitted request rather than from two
    /// arguments a caller could mismatch.
    pub fn new(db: &'a Surreal<Any>, owner: &'a OwnerContext, scope: &'a ScopedContext) -> Self {
        Self {
            db,
            owner,
            read: ScopeFilter::new(scope),
        }
    }

    /// Creates the caller's own cluster, stamped with account and workspace in the statement that creates
    /// it, inside the transaction that re-asserts the caller's authority.
    ///
    /// The three revision columns v009 gives a cluster (`membership_revision`,
    /// `member_embedding_revision`, `centroid_revision`) take their schema `DEFAULT 0`, which is what
    /// every accepted scoped create does: a brand-new row has no earlier version to compare against, so
    /// the first *change* to it is what increments it, and that change is Task 12's writer.
    ///
    /// A credential revoked between the caller's `admit` and this commit does not get to leave a cluster
    /// behind — `Err(StorageError::Stale)`, the same answer the other scoped creates give.
    pub async fn create(
        &self,
        label: Option<&str>,
        centroid: &[f32],
        now: DateTime<Utc>,
    ) -> Result<Cluster, StorageError> {
        let write = GuardedWrite::new(
            "CREATE cluster SET label = $label, centroid = $centroid, depth = 0, account = $account, \
             workspace = $workspace RETURN AFTER;",
        )
        .value("label", Bind::OptText(label.map(str::to_string)))
        .value("centroid", Bind::Floats(centroid.to_vec()));
        let rows = guarded_run::<Cluster>(self.db, self.owner, &write, now)
            .await?
            .ok_or(StorageError::Stale)?;
        rows.into_iter()
            .next()
            .ok_or_else(|| StorageError::Internal(anyhow::anyhow!("cluster row was not returned")))
    }

    /// One of the caller's own clusters, by handle.
    ///
    /// The `workspace` stamp is what turns "is this id yours" into a single indexed read; a cluster with
    /// no owner (a migration-pending row) is unreachable, which is the fail-closed answer rather than a
    /// gap to heal by falling back to an unscoped read.
    pub async fn get(&self, cluster_id: &str) -> Result<Option<Cluster>, StorageError> {
        let id = RecordId::parse_simple(cluster_id)
            .map_err(|_| StorageError::InvalidInput("malformed cluster id".to_string()))?;
        if id.table.as_str() != "cluster" {
            return Err(StorageError::InvalidInput(
                "malformed cluster id".to_string(),
            ));
        }
        self.get_by_id(&id).await
    }

    /// The same read by the **typed handle**, for a caller that resolved the cluster from its own listing.
    ///
    /// This exists rather than a round trip through [`record_id_to_string`](crate::record_id_to_string) because
    /// that formatter is a display form and is **not** a parser-safe SQL round trip (`AGENTS.md`, for numeric
    /// and opaque keys). A read that re-parsed a display form could resolve to a different row or to none, and
    /// a centroid derived from "no members" is not a refusal but a silent replacement of the cluster's centre.
    pub async fn get_by_id(&self, cluster: &RecordId) -> Result<Option<Cluster>, StorageError> {
        let mut response = self
            .db
            .query(
                "SELECT * FROM type::record('cluster', $key) WHERE id = $id AND workspace = \
                 $workspace LIMIT 1;",
            )
            .bind(("key", key(cluster)?))
            .bind(("id", cluster.clone()))
            .bind(("workspace", self.read.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<Cluster> = response.take(0).map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    /// The caller's clusters.
    pub async fn list(&self) -> Result<Vec<Cluster>, StorageError> {
        let mut response = self
            .db
            .query("SELECT * FROM cluster WHERE workspace = $workspace ORDER BY id;")
            .bind(("workspace", self.read.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// The members of one of the caller's clusters that its **resolved population** may read: live,
    /// inside the same workspace, and inside the partitions the caller named.
    ///
    /// The member filter is applied to the traversal target's own parentheses, which is the only
    /// spelling this engine parses for a filtered graph edge, and the live, workspace and partition
    /// predicates all belong **in the query**. Counting members from a global traversal and dropping
    /// ineligible rows in Rust would report a member count derived from another account's facts, and
    /// would let `recall(scope: "personal")` answer with a project memory — the P1 this read exists to
    /// close, because `retrieve_memories` and `recall` share one `scope` word and must therefore share
    /// one population predicate. Same conjunct [`ScopedSessionRepo::memories_in`] uses for the session
    /// arm of retrieval.
    pub async fn get_members(&self, cluster_id: &str) -> Result<Vec<Fact>, StorageError> {
        // The cluster itself must be the caller's first; an id that resolves to no readable cluster
        // returns no members rather than the row set its edges happen to point at.
        if self.get(cluster_id).await?.is_none() {
            return Ok(vec![]);
        }
        let id = RecordId::parse_simple(cluster_id)
            .map_err(|_| StorageError::InvalidInput("malformed cluster id".to_string()))?;
        let query = self
            .db
            .query(cluster_members_sql(self.read.conjunct()))
            .bind(("cluster", id));
        let mut response = self
            .read
            .apply(query)
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// The members of one of the caller's clusters across **every partition in the workspace**, with their
    /// vectors — the population a centroid is derived from.
    ///
    /// This exists because the other two reads cannot answer the question. [`Self::get_members`] is filtered
    /// to the partitions the caller resolved, so a mixed cluster's members in the *other* partition are
    /// missing and their vectors are silently absent from any average taken over it; at the degenerate case
    /// of the caller selecting a partition the cluster has no member in, it returns nothing and an average
    /// over it would replace a live centroid with one new embedding. [`Self::workspace_member_count`] is
    /// the right *population* but answers only how many, discarding the rows it read, so it cannot support
    /// a recomputation at all — a caller holding only a count has nothing to derive from but the stale
    /// centroid it is trying to replace.
    ///
    /// Same qualification as the count: the cluster must be the caller's own first (so a foreign handle has
    /// no observable membership), and the traversal filters live members by workspace. `deleted = false` and
    /// `quarantined_at = NONE` both matter: a hidden or quarantined member is not part of the centre of what
    /// is retrievable, and the second predicate is `= NONE` rather than `IS NOT NULL` for the reason
    /// `AGENTS.md` records — on this engine `IS NOT NULL` is satisfied by `NONE` and would filter nothing.
    pub async fn workspace_members(&self, cluster_id: &str) -> Result<Vec<Fact>, StorageError> {
        if self.get(cluster_id).await?.is_none() {
            return Ok(vec![]);
        }
        let id = RecordId::parse_simple(cluster_id)
            .map_err(|_| StorageError::InvalidInput("malformed cluster id".to_string()))?;
        let mut response = self
            .db
            .query(workspace_members_sql())
            .bind(("cluster", id))
            .bind(("workspace", self.read.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// The members of one of the caller's clusters across **every partition in the workspace**, with their
    /// vectors — the population a centroid is derived from.
    ///
    /// Takes the typed handle, because a display form is **not** a parser-safe round trip
    /// (`AGENTS.md`: numeric and opaque keys do not survive re-parsing), and a centroid derived from the
    /// wrong cluster because an id failed to re-parse is a silent corruption rather than a loud failure.
    /// [`Self::workspace_members`] is the display-keyed convenience for callers that already hold text.
    ///
    /// This is deliberately not [`Self::get_members`]. That read is filtered to the partitions the caller
    /// resolved, so a mixed cluster's members in the *other* partition are missing and their vectors are
    /// silently absent from any average taken over it; at the degenerate case of the caller selecting a
    /// partition the cluster has no member in, it returns nothing, and an average over it would replace a live
    /// centroid with the one vector the change carries. The population count is a read/eligibility property;
    /// a derived centroid needs the stored cluster's own membership.
    ///
    /// Same qualification as the count: the cluster must be the caller's own first (so a foreign handle has no
    /// observable membership), and the traversal filters live members by workspace. `quarantined_at = NONE`
    /// rather than `IS NOT NULL` for the reason `AGENTS.md` records — on this engine the latter is satisfied by
    /// `NONE` and would filter nothing, which would put a quarantined member's vector back into the centre.
    pub async fn workspace_members_of(
        &self,
        cluster: &RecordId,
    ) -> Result<Vec<Fact>, StorageError> {
        // The same fail-closed check the text-keyed read makes: a cluster outside the caller's workspace has no
        // observable membership, so this cannot probe a foreign cluster's size or vectors.
        if self.get_by_id(cluster).await?.is_none() {
            return Ok(vec![]);
        }
        let mut response = self
            .db
            .query(workspace_members_sql())
            .bind(("cluster", cluster.clone()))
            .bind(("workspace", self.read.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// The members of one of the caller's clusters in its **workspace**, regardless of partition.
    ///
    /// This is deliberately not [`Self::get_members`]. A cluster's centroid is an average over every live
    /// member the cluster has, and the store path uses this count as the running average's `n`. Feeding it
    /// the population-filtered count would silently rewrite a mixed cluster's centroid using the number of
    /// members in one partition — at the degenerate case of zero, replacing the whole centroid with the new
    /// embedding. The population count is a read/eligibility property; the centroid needs the stored
    /// cluster's own membership size.
    pub(crate) async fn workspace_member_count(
        &self,
        cluster_id: &str,
    ) -> Result<usize, StorageError> {
        Ok(self.workspace_members(cluster_id).await?.len())
    }

    /// The caller's clusters with their **workspace** live-member counts.
    ///
    /// The count is the cluster's size, not the size of the caller's current partition: it is the value
    /// the store path must pass to `update_centroid` to avoid corrupting mixed clusters. Recall callers
    /// load the partition-eligible members separately (`get_members`) and drop any candidate whose
    /// resolved population has none.
    ///
    /// One query per cluster, which is the same cost the unscoped path already paid and is unchanged
    /// here; collapsing it is Task 12's partitioned-maintenance work, not this checkpoint's.
    ///
    /// **This is a workspace-wide read and must stay one.** Its name says `workspace`, its predicate says
    /// `workspace`, and the recall path depends on exactly that (see `cluster_infos_for`). A writer must
    /// not reach for it: ranking a destination against it can name a cluster of another partition, which
    /// is the defect [`Self::partition_candidates`] exists to refuse. Task 8's partitioned recall read
    /// filter members, not candidates, and that asymmetry is documented, not accidental.
    pub async fn list_with_counts(&self) -> Result<Vec<(Cluster, usize)>, StorageError> {
        let mut out = Vec::new();
        for cluster in self.list().await? {
            let id = cluster
                .id
                .as_ref()
                .map(|id| id.to_sql())
                .unwrap_or_default();
            let count = self.workspace_member_count(&id).await?;
            out.push((cluster, count));
        }
        Ok(out)
    }

    /// The clusters this caller may lawfully **join** to land a memory in one partition.
    ///
    /// This is the destination-candidate read the request path's writers are required to use, and it is
    /// deliberately *not* [`Self::list_with_counts`] relabelled. A candidate here is a cluster of the
    /// caller's own workspace whose live membership is (a) **not empty** inside the partition the memory is
    /// being filed into and (b) **wholly inside it** — one live member outside that partition and the cluster
    /// is not a candidate at all. Three states that `list_with_counts` accepts and this refuses:
    ///
    /// * a cluster whose members are all in *another* partition (the `Personal` cluster a `project` move
    ///   must not join, and the `project A` cluster a `project B` move must not join);
    /// * a cluster with no live member at all, which is not implicitly adopted into a new partition;
    /// * a legacy **mixed** cluster, which is something to detach from under guard and to report on, never
    ///   something to select as a destination — its mixed membership is Task 12's to repair, and joining it
    ///   would add a cross-partition edge rather than remove one.
    ///
    /// Both counts are computed **by the store over the cluster's whole live population** and the selection
    /// happens before any ranking: `assign_to_cluster` is never handed a cluster the predicate rejected, so
    /// there is no "rank the workspace, then drop the wrong ones after choosing" path to drift back into.
    /// There is no `LIMIT` in the statement, so the Rust filter runs over a complete population, not a
    /// trimmed page — which is the distinction `repos::scope` documents for filtering in Rust.
    ///
    /// A row whose counts the engine did not project is refused as a candidate rather than read as zero
    /// members: "the store did not answer" and "the cluster is empty" must not collide.
    pub async fn partition_candidates(&self) -> Result<Vec<(Cluster, usize)>, StorageError> {
        candidates_matching(self.db, &self.read).await
    }

    /// [`Self::partition_candidates`] bound to **one named partition** of the caller's own workspace,
    /// rather than to the population a request resolved (which may be a union: `Auto` is personal plus a
    /// project, and `Assigned` is everything).
    ///
    /// A writer needs this because its destination is one partition, always: a store files into the
    /// partition it admitted, and a move files into the partition it resolved. Using the request's union
    /// filter would treat a cluster straddling two granted partitions as lawful, and the frozen design
    /// says a cluster's membership is one scope, not a set of scopes the caller happens to hold.
    pub async fn candidates_for_partition(
        &self,
        partition: &MemoryScope,
    ) -> Result<Vec<(Cluster, usize)>, StorageError> {
        let partition = Partition {
            workspace: self.owner.workspace().clone(),
            scope: partition.clone(),
        };
        candidates_matching(self.db, &ScopeFilter::for_partition(&partition)).await
    }

    /// The stored membership guard on each of the caller's clusters, keyed by the cluster's own
    /// [`RecordId`] — the identity `list` returns, so a caller matches rows without round-tripping an id
    /// through a string spelling that `AGENTS.md` warns is not parser-safe.
    ///
    /// One read for the whole population, deliberately: a broad recall pass needs a version per
    /// *candidate* cluster and keeps the top few, so asking per cluster would cost a round trip for
    /// every cluster the caller has ever had. The member count is not included because that pass is about
    /// to read the members themselves — counting the rows it already holds is one query cheaper and
    /// cannot disagree with what it serves.
    pub async fn revisions(&self) -> Result<Vec<(RecordId, Option<i64>)>, StorageError> {
        let mut response = self
            .db
            .query(
                "SELECT id, membership_revision FROM cluster WHERE workspace = $workspace ORDER \
                 BY id;",
            )
            .bind(("workspace", self.read.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<RevisionRow> = response.take(0).map_err(internal)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| Some((row.id?, row.membership_revision)))
            .collect())
    }

    /// The stored membership guard on one of the caller's own clusters, or `None` when this caller
    /// cannot read that row at all.
    ///
    /// This is the reauthorization a replayed `scope_handle` is checked against, and it is a read of the
    /// row rather than a parse of the handle. `None` therefore says "this handle may claim no guard",
    /// whether because the row is not the caller's to read at all or because no membership writer has
    /// ever stamped it — the two are deliberately not distinguished, since telling them apart would make
    /// the handle a probe for which of them it was. The member count is not read here: the caller is
    /// about to read the members themselves, and a handle's version is assembled from what that read
    /// actually returned.
    pub async fn revision(&self, cluster_id: &str) -> Result<Option<i64>, StorageError> {
        // A string that is not a cluster record is the same answer as a cluster the caller cannot read,
        // so this cannot be used to tell "not yours" from "not a cluster" either.
        let Ok(id) = RecordId::parse_simple(cluster_id) else {
            return Ok(None);
        };
        if id.table.as_str() != "cluster" {
            return Ok(None);
        }
        let mut response = self
            .db
            .query(
                "SELECT membership_revision FROM type::record('cluster', $key) WHERE id = $id AND \
                 workspace = $workspace LIMIT 1;",
            )
            .bind(("key", key(&id)?))
            .bind(("id", id))
            .bind(("workspace", self.read.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<RevisionRow> = response.take(0).map_err(internal)?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|row| row.membership_revision))
    }
}

/// One cluster as the destination-candidate read sees it: its row, plus the two counts that decide it.
#[derive(SurrealValue)]
struct CandidateRow {
    id: Option<RecordId>,
    label: Option<String>,
    centroid: Vec<f32>,
    depth: i64,
    created_at: Option<DateTime<Utc>>,
    inside_population: Option<i64>,
    outside_population: Option<i64>,
}

/// The destination-candidate read over the caller's own workspace, bound to one population predicate.
async fn candidates_matching(
    db: &Surreal<Any>,
    population: &ScopeFilter,
) -> Result<Vec<(Cluster, usize)>, StorageError> {
    // Refused before the statement runs, not filtered after it: a union population would report a cluster
    // that straddles two of its partitions as pure, and every ranking downstream would then be lawful
    // against a destination that is not one destination.
    if !population.is_single_partition() {
        return Err(StorageError::InvalidInput(
            "a destination candidate population must name exactly one partition".to_string(),
        ));
    }
    let sql = partition_candidates_sql(population.conjunct());
    let query = db.query(&sql);
    let mut response = population
        .apply(query)
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?;
    let rows: Vec<CandidateRow> = response.take(0).map_err(internal)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let (Some(id), Some(inside), Some(outside)) =
                (row.id, row.inside_population, row.outside_population)
            else {
                return None;
            };
            if inside <= 0 || outside != 0 {
                return None;
            }
            let cluster = Cluster {
                id: Some(id),
                label: row.label,
                centroid: row.centroid,
                depth: row.depth,
                created_at: row.created_at,
            };
            Some((cluster, inside as usize))
        })
        .collect())
}

/// The destination-candidate statement, built from the caller's population conjunct.
///
/// A function rather than an inline literal for the reason [`cluster_members_sql`] gives: the shipped
/// statement and the test that pins it must have one source of truth, and here the shape is the whole
/// claim — two counts over the cluster's complete live membership, with the non-emptiness and purity
/// predicates expressed as those counts and nothing else.
///
/// The `NOT (...)` arm is what makes a mixed cluster fail: it counts live members the partition does not
/// grant, which is where a member of another partition, a member of another workspace and a member with no
/// partition stamp at all all land. `deleted = false AND quarantined_at = NONE` is spelled on both sides
/// because a hidden or quarantined row is not part of the population a centroid describes, so it can neither
/// make a cluster eligible nor disqualify it.
pub fn partition_candidates_sql(membership: &str) -> String {
    format!(
        "SELECT id, label, centroid, depth, created_at, \
         (->contains_memory->(fact WHERE {LIVE_MEMBER} AND {membership})).len() AS inside_population, \
         (->contains_memory->(fact WHERE {LIVE_MEMBER} AND NOT ({membership}))).len() AS \
         outside_population FROM cluster WHERE workspace = $scope_workspace ORDER BY id;"
    )
}

/// A cluster's member read, built from the caller's population conjunct.
///
/// A function rather than an inline literal so the shipped statement and its plan test are one source of
/// truth: the conjunct is what makes `recall(scope: "personal")` answer differently from
/// `recall(scope: "assigned")`, and a test that re-typed the SQL could stay green after the conjunct
/// drifted out of the query.
pub fn cluster_members_sql(membership: &str) -> String {
    format!("SELECT * FROM $cluster->contains_memory->(fact WHERE {LIVE_MEMBER} AND {membership});")
}

/// The statement that counts a cluster's live workspace membership for centroid bookkeeping.
///
/// It omits the partition conjunct intentionally; the caller's own account/workspace isolation remains
/// because the cluster is first checked under the caller's workspace and the traversal filters by workspace.
fn workspace_members_sql() -> String {
    format!(
        "SELECT * FROM $cluster->contains_memory->(fact WHERE {LIVE_MEMBER} AND workspace = $workspace);"
    )
}

/// An id and the guard, and nothing else: the projection `revisions` and `revision` share so neither
/// pulls a centroid back to discard it.
#[derive(SurrealValue)]
struct RevisionRow {
    id: Option<RecordId>,
    membership_revision: Option<i64>,
}

fn key(id: &RecordId) -> Result<String, StorageError> {
    use surrealdb::types::RecordIdKey;
    let text = match &id.key {
        RecordIdKey::String(text) => text.clone(),
        RecordIdKey::Number(number) => number.to_string(),
        RecordIdKey::Uuid(uuid) => uuid.to_string(),
        other => format!("{other:?}"),
    };
    if text.is_empty() {
        return Err(StorageError::InvalidInput(
            "malformed cluster id".to_string(),
        ));
    }
    Ok(text)
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}
