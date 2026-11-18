use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, RecordIdKeyRange, SurrealValue};
use tracing::warn;

use crate::Database;
use crate::access::SystemContext;
use crate::models::identity::{OperationId, Partition};
use crate::models::maintenance::job as job_name;
use crate::models::mutation::{
    ClusterChange, ClusterDestination, MaintenanceSummary, PreparedChangeSet, effect,
};
use crate::models::operation_kind;
use crate::models::{Cluster, Fact};
#[cfg(test)]
use crate::record_id_to_string;
use crate::repos::MutationRepo;
use crate::repos::maintenance_repo::AuditContext;
use crate::repos::scope::ScopeFilter;

#[derive(Debug, Clone)]
pub struct PartitionCluster {
    pub cluster: Cluster,
    pub members: Vec<Fact>,
}

/// One prepared split verdict for an owned cluster: how the live members group, and the two
/// centroids those groups imply.
///
/// The grouping is the engine's arithmetic, so it arrives as data — storage ranks nothing. The
/// indices address the source's live member list in the order the partition read returned it,
/// which is the same list [`ClusterRepo::partition_clusters`] handed the caller; the transaction
/// re-verifies that the two groups exactly partition the live population before anything moves.
#[derive(Debug, Clone)]
pub struct ClusterSplit {
    pub group_a: Vec<usize>,
    pub group_b: Vec<usize>,
    pub centroid_a: Vec<f32>,
    pub centroid_b: Vec<f32>,
}

pub struct ClusterRepo<'a> {
    db: &'a Surreal<Any>,
    /// Attribution for the audit rows this repo writes. Carried on the repo rather than passed per
    /// call because it is a property of the writer: one scheduler tick has one `run_id`, and a call
    /// site that forgot to pass it would produce an unattributable row instead of failing to compile.
    audit: Option<AuditContext>,
}

impl<'a> ClusterRepo<'a> {
    /// Unattributed: `maintenance_log` rows get NONE for run_id/job/actor, as every row written
    /// before v008 does.
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db, audit: None }
    }

    /// Attribute the split and merge rows this repo writes to one run of one job.
    pub fn with_audit(db: &'a Surreal<Any>, audit: AuditContext) -> Self {
        Self {
            db,
            audit: Some(audit),
        }
    }

    /// The three attribution binds for a `CREATE maintenance_log`, `None` when the repo is
    /// unattributed. A split moves
    /// members between clusters, which is the most destructive thing any job does, so its row is the
    /// one an operator most needs to be able to tie back to a run.
    fn audit_binds(&self) -> (Option<String>, Option<String>, Option<String>) {
        match &self.audit {
            Some(audit) => (
                Some(audit.run_id.clone()),
                Some(audit.job.clone()),
                Some(audit.actor.clone()),
            ),
            None => (None, None, None),
        }
    }

    pub async fn create(&self, label: Option<&str>, centroid: &[f32]) -> Result<RecordId> {
        let mut response = self
            .db
            .query(
                "CREATE cluster SET \
                 label = $label, \
                 centroid = $centroid, \
                 depth = 0",
            )
            .bind(("label", label.map(|s| s.to_string())))
            .bind(("centroid", centroid.to_vec()))
            .await?;

        let created: Option<Cluster> = response.take(0)?;
        let cluster = created.ok_or_else(|| anyhow::anyhow!("Failed to create cluster"))?;
        let id = cluster
            .id
            .ok_or_else(|| anyhow::anyhow!("Created cluster has no id"))?;
        Ok(id)
    }

    pub async fn add_member(&self, cluster: &RecordId, fact: &RecordId) -> Result<()> {
        let from = cluster.clone();
        let to = fact.clone();
        self.db
            .query("RELATE $from->contains_memory->$to")
            .bind(("from", from))
            .bind(("to", to))
            .await?
            .check()?;
        Ok(())
    }

    pub async fn get_members(&self, cluster: &RecordId) -> Result<Vec<Fact>> {
        // The filter belongs here and not in the callers: every consumer of this method computes
        // something about the cluster as a whole (`check_cohesion` over member embeddings,
        // `check_merge` over centroids, `execute_merge` moving edges), so a row nobody can retrieve
        // silently changes all three verdicts. See `test_get_members_excludes_deleted_and_quarantined`.
        // Typed bind, for the same reason as `delete`: `type::record(<quoted display string>)`
        // matches nothing, so a quoted-key cluster would read as memberless and every verdict
        // computed over its members (cohesion, merge counts) would be silently wrong.
        let mut response = self
            .db
            .query(
                "SELECT * FROM $cluster->contains_memory->\
                 (fact WHERE deleted = false AND quarantined_at = NONE)",
            )
            .bind(("cluster", cluster.clone()))
            .await?;
        let members: Vec<Fact> = response.take(0)?;
        Ok(members)
    }

    /// Read clusters whose complete live workspace population belongs to this partition.
    pub async fn partition_clusters(&self, partition: &Partition) -> Result<Vec<PartitionCluster>> {
        let filter = ScopeFilter::for_partition(partition);
        let mut response = self
            .db
            .query("SELECT VALUE owner FROM workspace WHERE id = $workspace LIMIT 1;")
            .bind(("workspace", partition.workspace.as_record_id().clone()))
            .await?;
        let owners: Vec<RecordId> = response.take(0)?;
        let Some(account) = owners.into_iter().next() else {
            return Ok(Vec::new());
        };
        let mut response = self
            .db
            .query("SELECT * FROM cluster WHERE workspace = $workspace AND account = $account ORDER BY id;")
            .bind(("workspace", partition.workspace.as_record_id().clone()))
            .bind(("account", account.clone()))
            .await?;
        let clusters: Vec<Cluster> = response.take(0)?;
        let mut selected = Vec::new();
        for cluster in clusters {
            let Some(id) = cluster.id.clone() else {
                continue;
            };
            let query = self.db.query(
                "SELECT * FROM $cluster->contains_memory->(fact WHERE deleted = false AND quarantined_at = NONE AND account = $account AND workspace = $scope_workspace AND ((scope = 'personal' AND $scope_personal) OR (scope = 'unassigned' AND $scope_unassigned) OR (scope = 'project' AND project INSIDE $scope_projects)))",
            ).bind(("cluster", id.clone())).bind(("account", account.clone()));
            let mut response = filter.apply(query).await?;
            let members: Vec<Fact> = response.take(0)?;
            if members.is_empty() {
                continue;
            }
            let query = self.db.query(
                "SELECT * FROM $cluster->contains_memory->(fact WHERE account = $account AND workspace = $scope_workspace AND ((scope = 'personal' AND $scope_personal) OR (scope = 'unassigned' AND $scope_unassigned) OR (scope = 'project' AND project INSIDE $scope_projects)))",
            ).bind(("cluster", id.clone())).bind(("account", account.clone()));
            let mut response = filter.apply(query).await?;
            let partition_members: Vec<Fact> = response.take(0)?;
            let mut partition_ids: Vec<RecordId> = partition_members
                .into_iter()
                .filter_map(|fact| fact.id)
                .collect();
            let mut all_ids = self.get_member_ids(&id).await?;
            partition_ids.sort();
            all_ids.sort();
            if partition_ids == all_ids {
                selected.push(PartitionCluster { cluster, members });
            }
        }
        Ok(selected)
    }

    pub async fn execute_partitioned_split(
        &self,
        instance: &Database,
        system: &SystemContext,
        partition: &Partition,
        source: &PartitionCluster,
        split: &ClusterSplit,
    ) -> Result<()> {
        let (run_id, job) = self.audit_for(job_name::CLUSTER)?;
        let source_id = source
            .cluster
            .id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("split source has no id"))?;
        let live: Vec<RecordId> = source
            .members
            .iter()
            .map(|member| {
                member
                    .id
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("split member has no id"))
            })
            .collect::<Result<_>>()?;
        let mut a = split
            .group_a
            .iter()
            .map(|index| {
                live.get(*index)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("split group index is out of range"))
            })
            .collect::<Result<Vec<_>>>()?;
        let b = split
            .group_b
            .iter()
            .map(|index| {
                live.get(*index)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("split group index is out of range"))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut assigned = a.iter().chain(&b).cloned().collect::<Vec<_>>();
        assigned.sort();
        let mut live_sorted = live.clone();
        live_sorted.sort();
        ensure!(
            assigned == live_sorted,
            "split groups do not partition the live members"
        );
        let all_members = self.get_member_ids(source_id).await?;
        let mut hidden: Vec<_> = all_members
            .into_iter()
            .filter(|id| !live.contains(id))
            .collect();
        a.append(&mut hidden);

        let seed = format!("{run_id}:{}", crate::record_id_to_string(source_id));
        let digest = Sha256::digest(seed.as_bytes());
        let key: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let new_id = RecordId::new("cluster", format!("owned_{key}"));
        let mutation = MutationRepo::new(self.db);
        let expectation = mutation
            .cluster_expectation_system(instance, system, partition, source_id)
            .await?;
        let change = PreparedChangeSet::new(operation_kind::CLUSTER_CHANGE)
            .effect(effect::CLUSTER_MEMBERSHIP)
            .effect(effect::CLUSTER_CENTROID)
            .cluster(ClusterChange::Restructure {
                partition: partition.scope.clone(),
                expectations: vec![expectation],
                destinations: vec![
                    ClusterDestination {
                        cluster: Some(source_id.clone()),
                        new_id: None,
                        label: source.cluster.label.clone(),
                        centroid: split.centroid_a.clone(),
                        members: a,
                    },
                    ClusterDestination {
                        cluster: None,
                        new_id: Some(new_id.clone()),
                        label: source.cluster.label.clone(),
                        centroid: split.centroid_b.clone(),
                        members: b,
                    },
                ],
            })
            .summary(MaintenanceSummary {
                action: "split".to_string(),
                source_id: crate::record_id_to_string(source_id),
                target_ids: vec![
                    crate::record_id_to_string(source_id),
                    crate::record_id_to_string(&new_id),
                ],
                members_moved: split.group_b.len() as i64,
                run_id: run_id.clone(),
                job,
                disposition: "split".to_string(),
                previous_value: None,
            });
        mutation
            .apply_system(
                instance,
                system,
                partition,
                &OperationId::new(format!(
                    "cluster-split:{run_id}:{}",
                    crate::record_id_to_string(source_id)
                )),
                &[],
                &change,
            )
            .await?;
        Ok(())
    }

    pub async fn execute_partitioned_merge(
        &self,
        instance: &Database,
        system: &SystemContext,
        partition: &Partition,
        keep: &PartitionCluster,
        remove: &PartitionCluster,
        merged_centroid: &[f32],
    ) -> Result<()> {
        let (run_id, job) = self.audit_for(job_name::MERGE)?;
        let keep_id = keep
            .cluster
            .id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("merge survivor has no id"))?;
        let remove_id = remove
            .cluster
            .id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("merge source has no id"))?;
        ensure!(keep_id != remove_id, "cannot merge a cluster into itself");
        let mut members = self.get_member_ids(keep_id).await?;
        let moved = self.get_member_ids(remove_id).await?;
        members.extend(moved.iter().cloned());
        members.sort();
        members.dedup();
        let mutation = MutationRepo::new(self.db);
        let expectations = vec![
            mutation
                .cluster_expectation_system(instance, system, partition, keep_id)
                .await?,
            mutation
                .cluster_expectation_system(instance, system, partition, remove_id)
                .await?,
        ];
        let change = PreparedChangeSet::new(operation_kind::CLUSTER_CHANGE)
            .effect(effect::CLUSTER_MEMBERSHIP)
            .effect(effect::CLUSTER_CENTROID)
            .cluster(ClusterChange::Restructure {
                partition: partition.scope.clone(),
                expectations,
                destinations: vec![ClusterDestination {
                    cluster: Some(keep_id.clone()),
                    new_id: None,
                    label: keep.cluster.label.clone(),
                    centroid: merged_centroid.to_vec(),
                    members,
                }],
            })
            .summary(MaintenanceSummary {
                action: "merge".to_string(),
                source_id: crate::record_id_to_string(remove_id),
                target_ids: vec![crate::record_id_to_string(keep_id)],
                members_moved: moved.len() as i64,
                run_id: run_id.clone(),
                job,
                disposition: "merge".to_string(),
                previous_value: None,
            });
        mutation
            .apply_system(
                instance,
                system,
                partition,
                &OperationId::new(format!(
                    "cluster-merge:{run_id}:{}:{}",
                    crate::record_id_to_string(keep_id),
                    crate::record_id_to_string(remove_id)
                )),
                &[],
                &change,
            )
            .await?;
        Ok(())
    }

    fn audit_for(&self, expected_job: &str) -> Result<(String, String)> {
        let (run_id, job, actor) = self.audit_binds();
        ensure!(
            actor.as_deref() == Some("system:dreaming") && job.as_deref() == Some(expected_job),
            "partitioned cluster maintenance requires the matching system audit context"
        );
        Ok((
            run_id.ok_or_else(|| anyhow::anyhow!("missing audit run id"))?,
            job.ok_or_else(|| anyhow::anyhow!("missing audit job"))?,
        ))
    }

    /// Owned migration population: live members first, otherwise only retained
    /// deleted members with no quarantine, missing targets or other references.
    /// This is deliberately separate from live retrieval/maintenance semantics.
    pub async fn get_reembed_members(&self, id: &RecordId) -> Result<Vec<Fact>> {
        let live = self.get_members(id).await?;
        if !live.is_empty() {
            return Ok(live);
        }
        ensure!(
            !self.reembed_references(id, true).await?,
            "Cluster {id:?} has zero visible members but retains non-membership references"
        );
        let mut response = self
            .db
            .query("SELECT VALUE out FROM contains_memory WHERE in = $id")
            .bind(("id", id.clone()))
            .await?;
        let mut ids: Vec<RecordId> = response.take(0)?;
        ids.sort();
        ids.dedup();
        ensure!(
            ids.iter().all(|id| id.table.as_str() == "fact"),
            "Migration membership must target a fact"
        );
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut response = self
            .db
            .query("SELECT * FROM $members ORDER BY id ASC")
            .bind(("members", ids.clone()))
            .await?;
        let members: Vec<Fact> = response.take(0)?;
        let mut actual: Vec<RecordId> = members.iter().filter_map(|fact| fact.id.clone()).collect();
        actual.sort();
        ensure!(
            actual == ids,
            "Migration cluster has missing membership targets"
        );
        ensure!(
            members
                .iter()
                .all(|fact| fact.deleted && fact.quarantined_at.is_none()),
            "Migration fallback requires only deleted, non-quarantined members"
        );
        Ok(members)
    }

    /// Migration may drop only genuinely empty, unreferenced clusters. Inspect
    /// every table, not a relation-name allowlist: extension relations count too.
    pub async fn has_reembed_references(&self, id: &RecordId) -> Result<bool> {
        self.reembed_references(id, false).await
    }

    async fn reembed_references(&self, id: &RecordId, allow_memberships: bool) -> Result<bool> {
        let mut response = self.db.query("INFO FOR DB STRUCTURE").await?;
        let info: Option<serde_json::Value> = response.take(0)?;
        let info = info.ok_or_else(|| anyhow::anyhow!("Missing database table inventory"))?;
        let tables = info["tables"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Invalid database table inventory"))?;
        for table in tables {
            let name = table["name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid database table name"))?;
            let records = RecordId::new(
                name,
                RecordIdKeyRange {
                    start: std::ops::Bound::Unbounded,
                    end: std::ops::Bound::Unbounded,
                },
            );
            let sql = if allow_memberships && name == "contains_memory" {
                "SELECT VALUE id FROM $records WHERE out = $id LIMIT 1"
            } else {
                "SELECT VALUE id FROM $records WHERE in = $id OR out = $id LIMIT 1"
            };
            let mut response = self
                .db
                .query(sql)
                .bind(("records", records))
                .bind(("id", id.clone()))
                .await?;
            let references: Vec<RecordId> = response.take(0)?;
            if !references.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Every fact id with an edge into this cluster, live or not.
    ///
    /// The move steps need exactly what [`Self::get_members`] deliberately hides: a quarantined or
    /// soft-deleted member is still a member, and this repo's `delete` drops
    /// `contains_memory WHERE in = $id` with no fact predicate, so anything not moved here is deleted
    /// with the source cluster. Ids only — a move needs no embeddings.
    pub async fn get_member_ids(&self, cluster: &RecordId) -> Result<Vec<RecordId>> {
        #[derive(serde::Deserialize, SurrealValue)]
        struct EdgeRow {
            out: RecordId,
        }
        // Typed bind, for the same reason as `delete`: `type::record(<quoted display string>)`
        // matches nothing, so a merge against a quoted-key cluster would move zero members and
        // still report success.
        let mut response = self
            .db
            .query("SELECT out FROM $cluster->contains_memory")
            .bind(("cluster", cluster.clone()))
            .await?;
        let rows: Vec<EdgeRow> = response.take(0)?;
        Ok(rows.into_iter().map(|row| row.out).collect())
    }

    /// Remove a single fact from this cluster (delete the contains_memory edge).
    pub async fn remove_member(&self, cluster: &RecordId, fact: &RecordId) -> Result<()> {
        let from = cluster.clone();
        let to = fact.clone();
        self.db
            .query("DELETE contains_memory WHERE in = $from AND out = $to")
            .bind(("from", from))
            .bind(("to", to))
            .await?
            .check()?;
        Ok(())
    }

    /// Delete a cluster record and all its contains_memory edges.
    pub async fn delete(&self, id: &RecordId) -> Result<()> {
        // Delete edges first, then the cluster itself. Both statements bind the TYPED id: the
        // record delete used to bind the display string through `type::record($id)`, which
        // matches nothing for a key that needs quoting (a hyphen, a space, any non-alphanumeric),
        // so the delete committed without deleting. The merge drain's termination guarantee is
        // "every successful merge removes a cluster" — a surviving removed cluster made it loop
        // forever. Real corpora never hit this (`create` mints alphanumeric keys); any quoted
        // key did.
        self.db
            .query("DELETE contains_memory WHERE in = $id")
            .bind(("id", id.clone()))
            .await?
            .check()?;
        self.db
            .query("DELETE $id")
            .bind(("id", id.clone()))
            .await?
            .check()?;
        Ok(())
    }

    /// Overwrite a cluster's centroid.
    pub async fn update_centroid(&self, cluster: &RecordId, centroid: &[f32]) -> Result<()> {
        // Typed bind, for the same reason as `delete`: the display string round trip no-ops on
        // keys that need quoting, and a merge that silently kept the old centroid is a wrong
        // cluster centre rather than a loud failure.
        self.db
            .query("UPDATE $id SET centroid = $centroid")
            .bind(("id", cluster.clone()))
            .bind(("centroid", centroid.to_vec()))
            .await?
            .check()?;
        Ok(())
    }

    /// Execute a cluster split: create two new clusters from k-means groups,
    /// reassign all members, and delete the original cluster.
    ///
    /// `cluster_id` — the cluster being split.
    /// `members` — the members of that cluster (order must match the group indices).
    /// `group_a` / `group_b` — indices into `members` for each new cluster.
    /// `centroid_a` / `centroid_b` — centroids for the new clusters.
    pub async fn execute_split(
        &self,
        source: &RecordId,
        members: &[Fact],
        group_a: &[usize],
        group_b: &[usize],
        centroid_a: &[f32],
        centroid_b: &[f32],
    ) -> Result<(RecordId, RecordId)> {
        let supplied = members
            .iter()
            .map(|member| member.id.clone())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| anyhow::anyhow!("Legacy maintenance requires identified members"))?;
        ensure!(
            self.legacy_maintenance_eligible(std::slice::from_ref(source), &supplied)
                .await?,
            "Legacy maintenance is unavailable for owned or unverified data"
        );
        let cid_a = self.create(None, centroid_a).await?;
        let cid_b = match self.create(None, centroid_b).await {
            Ok(id) => id,
            Err(e) => {
                // Clean up the first cluster to avoid orphan
                warn!("Split: failed to create cluster B, cleaning up A: {e}");
                let _ = self.delete(&cid_a).await;
                return Err(e);
            }
        };

        // Reassign members — add to new cluster first (idempotent), then remove from old.
        // This ordering means a failure leaves a duplicate edge rather than an orphan.
        for &idx in group_a {
            if let Some(fact) = members.get(idx) {
                let Some(fid) = fact.id.as_ref() else {
                    continue;
                };
                if let Err(e) = self.add_member(&cid_a, fid).await {
                    warn!(
                        "Split: failed to add {} to new cluster A: {e}",
                        crate::record_id_to_string(fid)
                    );
                    continue;
                }
                if let Err(e) = self.remove_member(source, fid).await {
                    warn!(
                        "Split: failed to remove {} from old cluster: {e}",
                        crate::record_id_to_string(fid)
                    );
                }
            }
        }
        for &idx in group_b {
            if let Some(fact) = members.get(idx) {
                let Some(fid) = fact.id.as_ref() else {
                    continue;
                };
                if let Err(e) = self.add_member(&cid_b, fid).await {
                    warn!(
                        "Split: failed to add {} to new cluster B: {e}",
                        crate::record_id_to_string(fid)
                    );
                    continue;
                }
                if let Err(e) = self.remove_member(source, fid).await {
                    warn!(
                        "Split: failed to remove {} from old cluster: {e}",
                        crate::record_id_to_string(fid)
                    );
                }
            }
        }

        // Everyone the live list accounted for, so the pass below can tell a member it moved from a
        // member the caller could not see.
        // `RecordId`'s key type can carry a range, which trips clippy's interior-mutability
        // heuristic for the key. The set is a transient membership filter over ids already
        // cloned out of the rows, never mutated through the set, so the flagged "risk" does not
        // exist here.
        #[allow(clippy::mutable_key_type)]
        let moved: HashSet<RecordId> = group_a
            .iter()
            .chain(group_b.iter())
            .filter_map(|&idx| members.get(idx))
            .filter_map(|fact| fact.id.clone())
            .collect();

        // Hidden members follow the larger half. They cannot be routed by k-means — the split's
        // verdict is computed over live members precisely so a row nobody can retrieve cannot move it
        // — and dropping their edge is the permanent loss this method used to cause: after a release
        // a quarantined memory has no cluster, `cluster_for_fact` is None, and the cluster-driven
        // paths in `recall` never reach it again. Choosing the larger child is a decision made in the
        // open; ties go to the first.
        let hidden_target = if group_b.len() > group_a.len() {
            &cid_b
        } else {
            &cid_a
        };
        let mut hidden_moved: i64 = 0;
        for fid in self
            .get_member_ids(source)
            .await?
            .iter()
            .filter(|id| !moved.contains(id))
        {
            if let Err(e) = self.add_member(hidden_target, fid).await {
                warn!(
                    "Split: failed to carry hidden member {} into {}: {e}",
                    crate::record_id_to_string(fid),
                    crate::record_id_to_string(hidden_target)
                );
                continue;
            }
            if let Err(e) = self.remove_member(source, fid).await {
                warn!(
                    "Split: failed to remove hidden member {} from the split cluster: {e}",
                    crate::record_id_to_string(fid)
                );
            }
            hidden_moved += 1;
        }

        // Delete the old cluster (now empty)
        self.delete(source).await?;

        // Log the split. Counted from the edge writes, not from the group sizes: the groups only
        // know about the members the caller could see, and under-reporting a move is how a
        // reconciliation against this row comes out wrong.
        let members_moved = (group_a.len() + group_b.len()) as i64 + hidden_moved;
        let (run_id, job, actor) = self.audit_binds();
        if let Err(e) = self
            .db
            .query(
                "CREATE maintenance_log SET action = 'split', source_id = $source, \
                 target_ids = $targets, members_moved = $count, \
                 run_id = $run_id, job = $job, actor = $actor",
            )
            .bind(("source", crate::record_id_to_string(source)))
            // The stored form is the display string (v004: target_ids array<string>) — the same
            // canonical form the merge insert and /debug/maintenance read. Binding the typed ids
            // fails the SCHEMAFULL insert and the warn above swallows it, so the split's audit row
            // silently vanished when `create` became typed.
            .bind((
                "targets",
                vec![
                    crate::record_id_to_string(&cid_a),
                    crate::record_id_to_string(&cid_b),
                ],
            ))
            .bind(("count", members_moved))
            .bind(("run_id", run_id))
            .bind(("job", job))
            .bind(("actor", actor))
            .await
        {
            warn!("Failed to log split: {e}");
        }

        Ok((cid_a, cid_b))
    }

    /// Execute a cluster merge: move all members from `remove_id` to `keep_id`,
    /// update the kept cluster's centroid, and delete the removed cluster.
    pub async fn execute_merge(
        &self,
        keep: &RecordId,
        remove: &RecordId,
        merged_centroid: &[f32],
    ) -> Result<()> {
        let targets = [keep.clone(), remove.clone()];
        ensure!(
            self.legacy_maintenance_eligible(&targets, &[]).await?,
            "Legacy maintenance is unavailable for owned or unverified data"
        );
        // Ids, not `get_members`: every edge has to be carried, including the hidden rows the live
        // filter excludes, because `delete` below drops them all. The centroid the caller passes is
        // still a function of live members only — moving a hidden row does not let it decide a
        // verdict.
        let mut members_moved: i64 = 0;
        for fid in self.get_member_ids(remove).await? {
            if let Err(e) = self.add_member(keep, &fid).await {
                warn!(
                    "Merge: failed to add {} to kept cluster {}: {e}",
                    crate::record_id_to_string(&fid),
                    crate::record_id_to_string(keep)
                );
                continue;
            }
            if let Err(e) = self.remove_member(remove, &fid).await {
                warn!(
                    "Merge: failed to remove {} from removed cluster {}: {e}",
                    crate::record_id_to_string(&fid),
                    crate::record_id_to_string(remove)
                );
            }
            members_moved += 1;
        }

        self.update_centroid(keep, merged_centroid).await?;
        self.delete(remove).await?;

        // Log the merge
        let (run_id, job, actor) = self.audit_binds();
        if let Err(e) = self
            .db
            .query(
                "CREATE maintenance_log SET action = 'merge', source_id = $source, \
                 target_ids = $targets, members_moved = $count, \
                 run_id = $run_id, job = $job, actor = $actor",
            )
            .bind(("source", crate::record_id_to_string(remove)))
            .bind(("targets", vec![crate::record_id_to_string(keep)]))
            .bind(("count", members_moved))
            .bind(("run_id", run_id))
            .bind(("job", job))
            .bind(("actor", actor))
            .await
        {
            warn!("Failed to log merge: {e}");
        }

        Ok(())
    }

    /// The WHERE fragment for the maintenance log pages, shared by the list and the count.
    ///
    /// One builder for both because the bug this prevents is on the record: `MemoryRepo::count` once
    /// duplicated `list`'s clause line-for-line while its doc claimed they matched, and the result was
    /// a debug page whose "N entries" disagreed with the rows under it. A filtered page whose pager
    /// total came from the unfiltered count is the same defect with a parameter in it.
    fn maintenance_filter_clause(run: Option<&str>) -> String {
        match run {
            // `run_id = $run` cannot be reached by a NULL-style trap: the filter is only emitted when
            // the caller asked for a run, and an unattributed row simply never matches a run id.
            Some(_) => "WHERE run_id = $run".to_string(),
            None => String::new(),
        }
    }

    /// List maintenance log entries, newest first, optionally narrowed to one scheduler tick.
    ///
    /// `run` is the unit README describes a pass being selected or reversed by, so the page needs it
    /// to be more than a column you can eyeball: a collapse tick that soft-deleted 40 rows is 40 rows
    /// to review, and on a busy store they are not near the top of an unfiltered list.
    pub async fn list_maintenance_logs(
        &self,
        limit: usize,
        offset: usize,
        run: Option<&str>,
    ) -> Result<Vec<crate::models::MaintenanceLog>> {
        let clause = Self::maintenance_filter_clause(run);
        let sql = format!(
            "SELECT * FROM maintenance_log {clause} ORDER BY created_at DESC LIMIT $limit START $offset"
        );
        let mut query = self
            .db
            .query(sql)
            .bind(("limit", limit as i64))
            .bind(("offset", offset as i64));
        if let Some(run) = run {
            query = query.bind(("run", run.to_string()));
        }
        let mut response = query.await?;
        let logs: Vec<crate::models::MaintenanceLog> = response.take(0)?;
        Ok(logs)
    }

    /// Count maintenance log entries, under the same filter as [`Self::list_maintenance_logs`].
    pub async fn count_maintenance_logs(&self, run: Option<&str>) -> Result<usize> {
        let clause = Self::maintenance_filter_clause(run);
        let sql = format!("SELECT count() as total FROM maintenance_log {clause} GROUP ALL");
        let mut query = self.db.query(sql);
        if let Some(run) = run {
            query = query.bind(("run", run.to_string()));
        }
        let mut response = query.await?;
        #[derive(serde::Deserialize, SurrealValue)]
        struct CountRow {
            total: i64,
        }
        let row: Option<CountRow> = response.take(0)?;
        Ok(row.map(|r| r.total as usize).unwrap_or(0))
    }

    /// Until scoped maintenance replaces the legacy writers, admit only physically unowned
    /// populations. This is an application gate, not a fence against privileged concurrent SQL.
    async fn legacy_maintenance_eligible(
        &self,
        clusters: &[RecordId],
        supplied_members: &[RecordId],
    ) -> Result<bool> {
        if clusters.is_empty() || clusters.iter().any(|id| id.table.as_str() != "cluster") {
            return Ok(false);
        }
        let mut response = self
            .db
            .query("SELECT VALUE out FROM contains_memory WHERE in IN $clusters")
            .bind(("clusters", clusters.to_vec()))
            .await?;
        let mut members: Vec<RecordId> = response.take(0)?;
        members.extend_from_slice(supplied_members);
        if members.iter().any(|id| id.table.as_str() != "fact") {
            return Ok(false);
        }
        let mut records: Vec<RecordId> = clusters.iter().cloned().chain(members).collect();
        records.sort();
        records.dedup();
        let expected = records.len();
        let mut response = self
            .db
            .query(
                "SELECT VALUE id FROM $records WHERE account = NONE AND workspace = NONE \
             AND scope = NONE AND project = NONE",
            )
            .bind(("records", records))
            .await?;
        let admitted: Vec<RecordId> = response.take(0)?;
        Ok(admitted.len() == expected)
    }

    /// Candidates for legacy split/merge only; hidden retained ownership also excludes a cluster.
    /// Entry-point rechecks remain necessary because selection is not write authorization.
    pub async fn legacy_maintenance_clusters(&self) -> Result<Vec<Cluster>> {
        let mut eligible = Vec::new();
        for cluster in self.list().await? {
            if let Some(id) = &cluster.id
                && self
                    .legacy_maintenance_eligible(std::slice::from_ref(id), &[])
                    .await?
            {
                eligible.push(cluster);
            }
        }
        Ok(eligible)
    }

    pub async fn list(&self) -> Result<Vec<Cluster>> {
        let mut response = self.db.query("SELECT * FROM cluster").await?;
        Ok(response.take(0)?)
    }

    /// One cluster by id, or `None` if no such record exists.
    ///
    /// Exists so display code can read the **stored** `centroid` — the value the background
    /// maintenance task actually splits on. Averaging members to approximate it makes two
    /// callers of `check_cohesion` disagree; see `debug::clusters::cohesion_of`.
    pub async fn get(&self, cluster: &RecordId) -> Result<Option<Cluster>> {
        let mut response = self
            .db
            .query("SELECT * FROM $cluster")
            .bind(("cluster", cluster.clone()))
            .await?;
        let clusters: Vec<Cluster> = response.take(0)?;
        Ok(clusters.into_iter().next())
    }

    pub async fn list_with_counts(&self) -> Result<Vec<(Cluster, usize)>> {
        let clusters = self.list().await?;

        let mut result = Vec::with_capacity(clusters.len());
        for cluster in clusters {
            let Some(id) = cluster.id.as_ref() else {
                continue;
            };
            let count = self.get_members(id).await?.len();
            result.push((cluster, count));
        }
        Ok(result)
    }
}

#[cfg(test)]
#[path = "cluster_repo/legacy_maintenance_gate_tests.rs"]
mod legacy_maintenance_gate_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;

    #[tokio::test]
    async fn test_get_returns_the_stored_centroid() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let cluster_repo = ClusterRepo::new(db.inner());

        let cid = cluster_repo
            .create(Some("readable"), &[0.25, -0.75])
            .await
            .unwrap();
        let cluster = cluster_repo
            .get(&cid)
            .await
            .unwrap()
            .expect("created cluster must be readable by its own id");
        assert_eq!(cluster.label.as_deref(), Some("readable"));
        assert_eq!(cluster.centroid, vec![0.25, -0.75]);

        // A different id must not resolve to this record, and a missing one is `None` rather
        // than an error — `get_members` reports an empty set for the same case.
        let other = cluster_repo.create(None, &[1.0, 0.0]).await.unwrap();
        assert_ne!(other, cid);
        assert!(
            cluster_repo
                .get(&RecordId::new("cluster", "never-created-9f2a"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_remove_member() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let cluster_repo = ClusterRepo::new(db.inner());
        let memory_repo = crate::repos::MemoryRepo::new(db.inner());

        let cid = cluster_repo.create(Some("c1"), &[0.1, 0.1]).await.unwrap();
        let (_f1, f1_t) = memory_repo
            .create_fact_with_id("f1", 0.5, &[0.1, 0.1], &[])
            .await
            .unwrap();
        let (_f2, f2_t) = memory_repo
            .create_fact_with_id("f2", 0.5, &[0.2, 0.2], &[])
            .await
            .unwrap();
        cluster_repo.add_member(&cid, &f1_t).await.unwrap();
        cluster_repo.add_member(&cid, &f2_t).await.unwrap();

        assert_eq!(cluster_repo.get_members(&cid).await.unwrap().len(), 2);

        cluster_repo.remove_member(&cid, &f1_t).await.unwrap();
        let remaining = cluster_repo.get_members(&cid).await.unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[tokio::test]
    async fn test_delete_cluster() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let cluster_repo = ClusterRepo::new(db.inner());
        let memory_repo = crate::repos::MemoryRepo::new(db.inner());

        let cid = cluster_repo
            .create(Some("doomed"), &[0.1, 0.1])
            .await
            .unwrap();
        let (f1, f1_t) = memory_repo
            .create_fact_with_id("f1", 0.5, &[0.1, 0.1], &[])
            .await
            .unwrap();
        cluster_repo.add_member(&cid, &f1_t).await.unwrap();

        cluster_repo.delete(&cid).await.unwrap();

        // Cluster gone
        let all = cluster_repo.list_with_counts().await.unwrap();
        assert!(all.is_empty());
        // Edges gone too — fact should have no cluster
        let memory_repo2 = crate::repos::MemoryRepo::new(db.inner());
        assert!(memory_repo2.cluster_for_fact(&f1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_update_centroid() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let cluster_repo = ClusterRepo::new(db.inner());

        let cid = cluster_repo.create(Some("c1"), &[1.0, 0.0]).await.unwrap();
        cluster_repo
            .update_centroid(&cid, &[0.0, 1.0])
            .await
            .unwrap();

        // Re-read and verify
        let clusters = cluster_repo.list_with_counts().await.unwrap();
        let (c, _) = &clusters[0];
        assert!((c.centroid[0] - 0.0).abs() < 0.001);
        assert!((c.centroid[1] - 1.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_list_with_counts() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let cluster_repo = ClusterRepo::new(db.inner());
        let memory_repo = crate::repos::MemoryRepo::new(db.inner());

        let c1 = cluster_repo
            .create(Some("cluster one"), &[0.1, 0.1])
            .await
            .unwrap();
        let (_f1, f1_t) = memory_repo
            .create_fact_with_id("f1", 0.5, &[0.1, 0.1], &[])
            .await
            .unwrap();
        let (_f2, f2_t) = memory_repo
            .create_fact_with_id("f2", 0.5, &[0.1, 0.1], &[])
            .await
            .unwrap();
        cluster_repo.add_member(&c1, &f1_t).await.unwrap();
        cluster_repo.add_member(&c1, &f2_t).await.unwrap();

        let c2 = cluster_repo
            .create(Some("cluster two"), &[0.9, 0.9])
            .await
            .unwrap();
        let _ = c2;

        let results = cluster_repo.list_with_counts().await.unwrap();
        assert_eq!(results.len(), 2);
        let (cluster1, count1) = results
            .iter()
            .find(|(c, _)| c.label.as_deref() == Some("cluster one"))
            .unwrap();
        assert_eq!(*count1, 2);
        let _ = cluster1;
        let (_, count2) = results
            .iter()
            .find(|(c, _)| c.label.as_deref() == Some("cluster two"))
            .unwrap();
        assert_eq!(*count2, 0);
    }

    /// The pin `SessionRepo::get_memories` got in 33e17c9 and the cluster path never did.
    ///
    /// A soft-deleted or quarantined member must not count toward `list_with_counts`, must not feed
    /// `check_cohesion`'s member embeddings, and must not be able to move a merge verdict — which
    /// became actively wrong rather than merely untidy the moment `Collapse` started soft-deleting
    /// duplicates, since a cluster of ten near-identical rows would keep behaving like a ten-member
    /// cluster after nine are collapsed. Quarantined rows are the same defect one rung earlier on the
    /// ladder.
    ///
    /// Filtering the *verdicts* is this method's job; filtering the *moves* is not, and must not
    /// become it — see `test_hidden_members_survive_a_split_and_a_merge`.
    /// The run filter exists because a tick that collapses 40 duplicates writes 40 rows, and on a busy
    /// store they are nowhere near the top of a newest-first list. Asserted against both queries at
    /// once: the page renders `total` from the count and the rows from the list, and the defect this
    /// repo already documents for `MemoryRepo::count` is precisely those two disagreeing.
    #[tokio::test]
    async fn maintenance_logs_can_be_filtered_to_one_run() {
        use crate::models::maintenance::{action, disposition, job};
        use crate::repos::{LogEntry, MaintenanceRepo};

        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let clusters = ClusterRepo::new(db.inner());
        let alpha = MaintenanceRepo::with_audit(
            db.inner(),
            AuditContext::dreaming("run-alpha", job::COLLAPSE),
        );
        let beta = MaintenanceRepo::with_audit(
            db.inner(),
            AuditContext::dreaming("run-beta", job::COLLAPSE),
        );

        for (audit, source) in [(&alpha, "fact:a1"), (&alpha, "fact:a2"), (&beta, "fact:b1")] {
            audit
                .record(&LogEntry {
                    action: action::COLLAPSE.to_string(),
                    source_id: source.to_string(),
                    target_ids: vec!["fact:survivor".to_string()],
                    members_moved: 0,
                    disposition: Some(disposition::SOFT_DELETE.to_string()),
                    previous_value: None,
                })
                .await
                .unwrap();
        }

        // Positive control: unfiltered, everything is present and the count matches the rows.
        assert_eq!(
            clusters
                .list_maintenance_logs(10, 0, None)
                .await
                .unwrap()
                .len(),
            3
        );
        assert_eq!(clusters.count_maintenance_logs(None).await.unwrap(), 3);

        let filtered = clusters
            .list_maintenance_logs(10, 0, Some("run-alpha"))
            .await
            .unwrap();
        assert_eq!(
            filtered.len(),
            2,
            "the filter must select the whole tick, not one row of it"
        );
        assert!(
            filtered
                .iter()
                .all(|row| row.run_id.as_deref() == Some("run-alpha")),
            "every returned row must belong to the run asked for"
        );
        assert_eq!(
            clusters
                .count_maintenance_logs(Some("run-alpha"))
                .await
                .unwrap(),
            filtered.len(),
            "the pager's total must come from the same clause as its rows"
        );

        // A run nobody wrote is empty rather than an error.
        assert!(
            clusters
                .list_maintenance_logs(10, 0, Some("run-missing"))
                .await
                .unwrap()
                .is_empty(),
            "an unknown run must read as no rows, not as every row"
        );
    }

    #[tokio::test]
    async fn test_get_members_excludes_deleted_and_quarantined() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let clusters = ClusterRepo::new(db.inner());
        let memory = crate::repos::MemoryRepo::new(db.inner());

        let cid = clusters.create(Some("mixed"), &[0.1, 0.2]).await.unwrap();
        let (_keep, keep_t) = memory
            .create_fact_with_id("kept", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let (gone, gone_t) = memory
            .create_fact_with_id("deleted", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let (secret, secret_t) = memory
            .create_fact_with_id("quarantined", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        for id in [&keep_t, &gone_t, &secret_t] {
            clusters.add_member(&cid, id).await.unwrap();
        }

        // Positive control first: without this, every assertion below could be satisfied by a
        // method that returns nothing at all.
        let all = clusters.get_members(&cid).await.unwrap();
        assert_eq!(
            all.len(),
            3,
            "fixture control: all three are members to begin with"
        );

        memory.soft_delete_fact(&gone).await.unwrap();
        db.inner()
            .query("UPDATE type::record($id) SET quarantined_at = time::now()")
            .bind(("id", secret.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();

        let members = clusters.get_members(&cid).await.unwrap();
        let contents: Vec<&str> = members.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(
            contents,
            ["kept"],
            "a soft-deleted and a quarantined member must both drop out, leaving the live row"
        );
    }

    /// The other half of the same change, and the half that could silently eat data.
    ///
    /// `get_members` filters, so every caller that iterates it sees only live rows — but `delete`
    /// drops `contains_memory WHERE in = $id` with no fact predicate at all. Before the moves were
    /// switched to `get_member_ids`, that meant a hidden member's edge was destroyed on every split
    /// and merge while its fact stayed live-and-recoverable: after a release, `cluster_for_fact`
    /// returns None and the cluster-driven paths in `recall` can never reach it again. Quarantine is
    /// documented as the reversible rung, so losing its placement is not a cosmetic consequence.
    #[tokio::test]
    async fn test_hidden_members_survive_a_split_and_a_merge() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let clusters = ClusterRepo::new(db.inner());
        let memory = crate::repos::MemoryRepo::new(db.inner());

        // --- merge: the hidden row follows the live one into the kept cluster ---
        let source = clusters.create(Some("source"), &[0.1, 0.2]).await.unwrap();
        let kept = clusters.create(Some("kept"), &[0.1, 0.2]).await.unwrap();
        let (live, live_t) = memory
            .create_fact_with_id("live", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let (quarantined, quarantined_t) = memory
            .create_fact_with_id("quarantined", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        for id in [&live_t, &quarantined_t] {
            clusters.add_member(&source, id).await.unwrap();
        }
        db.inner()
            .query("UPDATE type::record($id) SET quarantined_at = time::now()")
            .bind(("id", quarantined.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();

        clusters
            .execute_merge(&kept, &source, &[0.1, 0.2])
            .await
            .unwrap();

        assert_eq!(
            memory
                .cluster_for_fact(&quarantined)
                .await
                .unwrap()
                .map(|c| c.id.as_ref().map(record_id_to_string).unwrap_or_default()),
            Some(crate::record_id_to_string(&kept)),
            "a quarantined member keeps its cluster through a merge, so releasing it does not orphan it"
        );
        // Positive control on the same code path: the live row moved too, so the assertion above is
        // not being satisfied by a move step that carries nobody.
        assert_eq!(
            memory.cluster_for_fact(&live).await.unwrap().map(|c| c
                .id
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default()),
            Some(crate::record_id_to_string(&kept)),
            "the live member moved to the kept cluster as well"
        );

        // --- split: hidden members follow the larger half, they are not deleted with the source ---
        let splitting = clusters
            .create(Some("splitting"), &[0.3, 0.4])
            .await
            .unwrap();
        let (_left, left_t) = memory
            .create_fact_with_id("left", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        let (hidden, hidden_t) = memory
            .create_fact_with_id("hidden", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        clusters.add_member(&splitting, &left_t).await.unwrap();
        clusters.add_member(&splitting, &hidden_t).await.unwrap();
        db.inner()
            .query("UPDATE type::record($id) SET quarantined_at = time::now()")
            .bind(("id", hidden.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let members = clusters.get_members(&splitting).await.unwrap();
        let (child_a, child_b) = clusters
            .execute_split(&splitting, &members, &[0], &[], &[0.5, 0.6], &[0.7, 0.8])
            .await
            .unwrap();

        assert!(
            memory.cluster_for_fact(&hidden).await.unwrap().is_some(),
            "the hidden member still belongs to a cluster after its old one was split away"
        );
        assert_eq!(
            memory.cluster_for_fact(&hidden).await.unwrap().map(|c| c
                .id
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default()),
            Some(crate::record_id_to_string(&child_a)),
            "and it went to the larger half (ties to the first), not to a cluster chosen by accident"
        );
        assert_ne!(child_a, child_b, "the two children are distinct clusters");
    }
}
