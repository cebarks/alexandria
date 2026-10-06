use anyhow::Result;
use std::collections::HashSet;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue, ToSql};
use tracing::warn;

use crate::models::{Cluster, Fact};
use crate::record_id_to_string;
use crate::repos::maintenance_repo::AuditContext;

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

    pub async fn create(&self, label: Option<&str>, centroid: &[f32]) -> Result<String> {
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
        Ok(id.to_sql())
    }

    pub async fn add_member(&self, cluster_id: &str, fact_id: &str) -> Result<()> {
        let from = RecordId::parse_simple(cluster_id)?;
        let to = RecordId::parse_simple(fact_id)?;
        self.db
            .query("RELATE $from->contains_memory->$to")
            .bind(("from", from))
            .bind(("to", to))
            .await?
            .check()?;
        Ok(())
    }

    pub async fn get_members(&self, cluster_id: &str) -> Result<Vec<Fact>> {
        // The filter belongs here and not in the callers: every consumer of this method computes
        // something about the cluster as a whole (`check_cohesion` over member embeddings,
        // `check_merge` over centroids, `execute_merge` moving edges), so a row nobody can retrieve
        // silently changes all three verdicts. See `test_get_members_excludes_deleted_and_quarantined`.
        let mut response = self
            .db
            .query(
                "SELECT * FROM type::record($cluster_id)->contains_memory->\
                 (fact WHERE deleted = false AND quarantined_at = NONE)",
            )
            .bind(("cluster_id", cluster_id.to_string()))
            .await?;
        let members: Vec<Fact> = response.take(0)?;
        Ok(members)
    }

    /// Every fact id with an edge into this cluster, live or not.
    ///
    /// The move steps need exactly what [`Self::get_members`] deliberately hides: a quarantined or
    /// soft-deleted member is still a member, and this repo's `delete` drops
    /// `contains_memory WHERE in = $id` with no fact predicate, so anything not moved here is deleted
    /// with the source cluster. Ids only — a move needs no embeddings.
    pub async fn get_member_ids(&self, cluster_id: &str) -> Result<Vec<String>> {
        #[derive(serde::Deserialize, SurrealValue)]
        struct EdgeRow {
            out: RecordId,
        }
        let mut response = self
            .db
            .query("SELECT out FROM type::record($cluster_id)->contains_memory")
            .bind(("cluster_id", cluster_id.to_string()))
            .await?;
        let rows: Vec<EdgeRow> = response.take(0)?;
        Ok(rows
            .into_iter()
            .map(|row| record_id_to_string(&row.out))
            .collect())
    }

    /// Remove a single fact from this cluster (delete the contains_memory edge).
    pub async fn remove_member(&self, cluster_id: &str, fact_id: &str) -> Result<()> {
        let from = RecordId::parse_simple(cluster_id)?;
        let to = RecordId::parse_simple(fact_id)?;
        self.db
            .query("DELETE contains_memory WHERE in = $from AND out = $to")
            .bind(("from", from))
            .bind(("to", to))
            .await?
            .check()?;
        Ok(())
    }

    /// Delete a cluster record and all its contains_memory edges.
    pub async fn delete(&self, cluster_id: &str) -> Result<()> {
        let id = RecordId::parse_simple(cluster_id)?;
        // Delete edges first, then the cluster itself
        self.db
            .query("DELETE contains_memory WHERE in = $id")
            .bind(("id", id))
            .await?
            .check()?;
        self.db
            .query("DELETE type::record($id)")
            .bind(("id", cluster_id.to_string()))
            .await?
            .check()?;
        Ok(())
    }

    /// Overwrite a cluster's centroid.
    pub async fn update_centroid(&self, cluster_id: &str, centroid: &[f32]) -> Result<()> {
        self.db
            .query("UPDATE type::record($id) SET centroid = $centroid")
            .bind(("id", cluster_id.to_string()))
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
        cluster_id: &str,
        members: &[Fact],
        group_a: &[usize],
        group_b: &[usize],
        centroid_a: &[f32],
        centroid_b: &[f32],
    ) -> Result<(String, String)> {
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
                let fid = fact
                    .id
                    .as_ref()
                    .map(record_id_to_string)
                    .unwrap_or_default();
                if let Err(e) = self.add_member(&cid_a, &fid).await {
                    warn!("Split: failed to add {fid} to new cluster A: {e}");
                    continue;
                }
                if let Err(e) = self.remove_member(cluster_id, &fid).await {
                    warn!("Split: failed to remove {fid} from old cluster: {e}");
                }
            }
        }
        for &idx in group_b {
            if let Some(fact) = members.get(idx) {
                let fid = fact
                    .id
                    .as_ref()
                    .map(record_id_to_string)
                    .unwrap_or_default();
                if let Err(e) = self.add_member(&cid_b, &fid).await {
                    warn!("Split: failed to add {fid} to new cluster B: {e}");
                    continue;
                }
                if let Err(e) = self.remove_member(cluster_id, &fid).await {
                    warn!("Split: failed to remove {fid} from old cluster: {e}");
                }
            }
        }

        // Everyone the live list accounted for, so the pass below can tell a member it moved from a
        // member the caller could not see.
        let moved: HashSet<String> = group_a
            .iter()
            .chain(group_b.iter())
            .filter_map(|&idx| members.get(idx))
            .filter_map(|fact| fact.id.as_ref())
            .map(record_id_to_string)
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
            .get_member_ids(cluster_id)
            .await?
            .iter()
            .filter(|id| !moved.contains(id.as_str()))
        {
            if let Err(e) = self.add_member(hidden_target, fid).await {
                warn!("Split: failed to carry hidden member {fid} into {hidden_target}: {e}");
                continue;
            }
            if let Err(e) = self.remove_member(cluster_id, fid).await {
                warn!("Split: failed to remove hidden member {fid} from the split cluster: {e}");
            }
            hidden_moved += 1;
        }

        // Delete the old cluster (now empty)
        self.delete(cluster_id).await?;

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
            .bind(("source", cluster_id.to_string()))
            .bind(("targets", vec![cid_a.clone(), cid_b.clone()]))
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
        keep_id: &str,
        remove_id: &str,
        merged_centroid: &[f32],
    ) -> Result<()> {
        // Ids, not `get_members`: every edge has to be carried, including the hidden rows the live
        // filter excludes, because `delete` below drops them all. The centroid the caller passes is
        // still a function of live members only — moving a hidden row does not let it decide a
        // verdict.
        let mut members_moved: i64 = 0;
        for fid in self.get_member_ids(remove_id).await? {
            if let Err(e) = self.add_member(keep_id, &fid).await {
                warn!("Merge: failed to add {fid} to kept cluster {keep_id}: {e}");
                continue;
            }
            if let Err(e) = self.remove_member(remove_id, &fid).await {
                warn!("Merge: failed to remove {fid} from removed cluster {remove_id}: {e}");
            }
            members_moved += 1;
        }

        self.update_centroid(keep_id, merged_centroid).await?;
        self.delete(remove_id).await?;

        // Log the merge
        let (run_id, job, actor) = self.audit_binds();
        if let Err(e) = self
            .db
            .query(
                "CREATE maintenance_log SET action = 'merge', source_id = $source, \
                 target_ids = $targets, members_moved = $count, \
                 run_id = $run_id, job = $job, actor = $actor",
            )
            .bind(("source", remove_id.to_string()))
            .bind(("targets", vec![keep_id.to_string()]))
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

    /// List all clusters along with their live member counts.
    /// List maintenance log entries, newest first.
    pub async fn list_maintenance_logs(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::models::MaintenanceLog>> {
        let mut response = self
            .db
            .query(
                "SELECT * FROM maintenance_log ORDER BY created_at DESC LIMIT $limit START $offset",
            )
            .bind(("limit", limit as i64))
            .bind(("offset", offset as i64))
            .await?;
        let logs: Vec<crate::models::MaintenanceLog> = response.take(0)?;
        Ok(logs)
    }

    /// Count total maintenance log entries.
    pub async fn count_maintenance_logs(&self) -> Result<usize> {
        let mut response = self
            .db
            .query("SELECT count() as total FROM maintenance_log GROUP ALL")
            .await?;
        #[derive(serde::Deserialize, SurrealValue)]
        struct CountRow {
            total: i64,
        }
        let row: Option<CountRow> = response.take(0)?;
        Ok(row.map(|r| r.total as usize).unwrap_or(0))
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
    pub async fn get(&self, cluster_id: &str) -> Result<Option<Cluster>> {
        let mut response = self
            .db
            .query("SELECT * FROM type::record($cluster_id)")
            .bind(("cluster_id", cluster_id.to_string()))
            .await?;
        let clusters: Vec<Cluster> = response.take(0)?;
        Ok(clusters.into_iter().next())
    }

    pub async fn list_with_counts(&self) -> Result<Vec<(Cluster, usize)>> {
        let clusters = self.list().await?;

        let mut result = Vec::with_capacity(clusters.len());
        for cluster in clusters {
            let id = cluster.id.as_ref().map(|r| r.to_sql()).unwrap_or_default();
            let count = self.get_members(&id).await?.len();
            result.push((cluster, count));
        }
        Ok(result)
    }
}

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
                .get("cluster:never-created-9f2a")
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
        let f1 = memory_repo
            .create_fact("f1", 0.5, &[0.1, 0.1], &[])
            .await
            .unwrap();
        let f2 = memory_repo
            .create_fact("f2", 0.5, &[0.2, 0.2], &[])
            .await
            .unwrap();
        cluster_repo.add_member(&cid, &f1).await.unwrap();
        cluster_repo.add_member(&cid, &f2).await.unwrap();

        assert_eq!(cluster_repo.get_members(&cid).await.unwrap().len(), 2);

        cluster_repo.remove_member(&cid, &f1).await.unwrap();
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
        let f1 = memory_repo
            .create_fact("f1", 0.5, &[0.1, 0.1], &[])
            .await
            .unwrap();
        cluster_repo.add_member(&cid, &f1).await.unwrap();

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
        let f1 = memory_repo
            .create_fact("f1", 0.5, &[0.1, 0.1], &[])
            .await
            .unwrap();
        let f2 = memory_repo
            .create_fact("f2", 0.5, &[0.1, 0.1], &[])
            .await
            .unwrap();
        cluster_repo.add_member(&c1, &f1).await.unwrap();
        cluster_repo.add_member(&c1, &f2).await.unwrap();

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
    #[tokio::test]
    async fn test_get_members_excludes_deleted_and_quarantined() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let clusters = ClusterRepo::new(db.inner());
        let memory = crate::repos::MemoryRepo::new(db.inner());

        let cid = clusters.create(Some("mixed"), &[0.1, 0.2]).await.unwrap();
        let keep = memory
            .create_fact("kept", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let gone = memory
            .create_fact("deleted", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let secret = memory
            .create_fact("quarantined", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        for id in [&keep, &gone, &secret] {
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
        let live = memory
            .create_fact("live", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let quarantined = memory
            .create_fact("quarantined", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        for id in [&live, &quarantined] {
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
            Some(kept.clone()),
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
            Some(kept),
            "the live member moved to the kept cluster as well"
        );

        // --- split: hidden members follow the larger half, they are not deleted with the source ---
        let splitting = clusters
            .create(Some("splitting"), &[0.3, 0.4])
            .await
            .unwrap();
        let left = memory
            .create_fact("left", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        let hidden = memory
            .create_fact("hidden", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        clusters.add_member(&splitting, &left).await.unwrap();
        clusters.add_member(&splitting, &hidden).await.unwrap();
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
            Some(child_a.clone()),
            "and it went to the larger half (ties to the first), not to a cluster chosen by accident"
        );
        assert_ne!(child_a, child_b, "the two children are distinct clusters");
    }
}
