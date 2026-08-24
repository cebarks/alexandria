use anyhow::Result;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue, ToSql};
use surrealdb::Surreal;
use tracing::warn;

use crate::models::{Cluster, Fact};
use crate::record_id_to_string;

pub struct ClusterRepo<'a> {
    db: &'a Surreal<Any>,
}

impl<'a> ClusterRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    pub async fn create(
        &self,
        label: Option<&str>,
        centroid: &[f32],
    ) -> Result<String> {
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
        let id = cluster.id.ok_or_else(|| anyhow::anyhow!("Created cluster has no id"))?;
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
        let mut response = self
            .db
            .query(
                "SELECT * FROM type::record($cluster_id)->contains_memory->fact",
            )
            .bind(("cluster_id", cluster_id.to_string()))
            .await?;
        let members: Vec<Fact> = response.take(0)?;
        Ok(members)
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
                let fid = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
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
                let fid = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
                if let Err(e) = self.add_member(&cid_b, &fid).await {
                    warn!("Split: failed to add {fid} to new cluster B: {e}");
                    continue;
                }
                if let Err(e) = self.remove_member(cluster_id, &fid).await {
                    warn!("Split: failed to remove {fid} from old cluster: {e}");
                }
            }
        }

        // Delete the old cluster (now empty)
        self.delete(cluster_id).await?;

        // Log the split
        let members_moved = (group_a.len() + group_b.len()) as i64;
        if let Err(e) = self.db
            .query("CREATE maintenance_log SET action = 'split', source_id = $source, target_ids = $targets, members_moved = $count")
            .bind(("source", cluster_id.to_string()))
            .bind(("targets", vec![cid_a.clone(), cid_b.clone()]))
            .bind(("count", members_moved))
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
        let removed_members = self.get_members(remove_id).await?;

        // Add to target first, then remove from source (avoids orphaning on failure)
        for fact in &removed_members {
            let fid = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
            if let Err(e) = self.add_member(keep_id, &fid).await {
                warn!("Merge: failed to add {fid} to kept cluster {keep_id}: {e}");
                continue;
            }
            if let Err(e) = self.remove_member(remove_id, &fid).await {
                warn!("Merge: failed to remove {fid} from removed cluster {remove_id}: {e}");
            }
        }

        let members_moved = removed_members.len() as i64;

        self.update_centroid(keep_id, merged_centroid).await?;
        self.delete(remove_id).await?;

        // Log the merge
        if let Err(e) = self.db
            .query("CREATE maintenance_log SET action = 'merge', source_id = $source, target_ids = $targets, members_moved = $count")
            .bind(("source", remove_id.to_string()))
            .bind(("targets", vec![keep_id.to_string()]))
            .bind(("count", members_moved))
            .await
        {
            warn!("Failed to log merge: {e}");
        }

        Ok(())
    }

    /// List all clusters along with their live member counts.
    /// List maintenance log entries, newest first.
    pub async fn list_maintenance_logs(&self, limit: usize, offset: usize) -> Result<Vec<crate::models::MaintenanceLog>> {
        let mut response = self.db
            .query("SELECT * FROM maintenance_log ORDER BY created_at DESC LIMIT $limit START $offset")
            .bind(("limit", limit as i64))
            .bind(("offset", offset as i64))
            .await?;
        let logs: Vec<crate::models::MaintenanceLog> = response.take(0)?;
        Ok(logs)
    }

    /// Count total maintenance log entries.
    pub async fn count_maintenance_logs(&self) -> Result<usize> {
        let mut response = self.db
            .query("SELECT count() as total FROM maintenance_log GROUP ALL")
            .await?;
        #[derive(serde::Deserialize, SurrealValue)]
        struct CountRow { total: i64 }
        let row: Option<CountRow> = response.take(0)?;
        Ok(row.map(|r| r.total as usize).unwrap_or(0))
    }

    pub async fn list_with_counts(&self) -> Result<Vec<(Cluster, usize)>> {
        let mut response = self.db.query("SELECT * FROM cluster").await?;
        let clusters: Vec<Cluster> = response.take(0)?;

        let mut result = Vec::with_capacity(clusters.len());
        for cluster in clusters {
            let id = cluster
                .id
                .as_ref()
                .map(|r| r.to_sql())
                .unwrap_or_default();
            let count = self.get_members(&id).await.map(|m| m.len()).unwrap_or(0);
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
    async fn test_remove_member() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let cluster_repo = ClusterRepo::new(db.inner());
        let memory_repo = crate::repos::MemoryRepo::new(db.inner());

        let cid = cluster_repo.create(Some("c1"), &[0.1, 0.1]).await.unwrap();
        let f1 = memory_repo.create_fact("f1", 0.5, &[0.1, 0.1], &[]).await.unwrap();
        let f2 = memory_repo.create_fact("f2", 0.5, &[0.2, 0.2], &[]).await.unwrap();
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

        let cid = cluster_repo.create(Some("doomed"), &[0.1, 0.1]).await.unwrap();
        let f1 = memory_repo.create_fact("f1", 0.5, &[0.1, 0.1], &[]).await.unwrap();
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
        cluster_repo.update_centroid(&cid, &[0.0, 1.0]).await.unwrap();

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

        let c1 = cluster_repo.create(Some("cluster one"), &[0.1, 0.1]).await.unwrap();
        let f1 = memory_repo.create_fact("f1", 0.5, &[0.1, 0.1], &[]).await.unwrap();
        let f2 = memory_repo.create_fact("f2", 0.5, &[0.1, 0.1], &[]).await.unwrap();
        cluster_repo.add_member(&c1, &f1).await.unwrap();
        cluster_repo.add_member(&c1, &f2).await.unwrap();

        let c2 = cluster_repo.create(Some("cluster two"), &[0.9, 0.9]).await.unwrap();
        let _ = c2;

        let results = cluster_repo.list_with_counts().await.unwrap();
        assert_eq!(results.len(), 2);
        let (cluster1, count1) = results.iter().find(|(c, _)| c.label.as_deref() == Some("cluster one")).unwrap();
        assert_eq!(*count1, 2);
        let _ = cluster1;
        let (_, count2) = results.iter().find(|(c, _)| c.label.as_deref() == Some("cluster two")).unwrap();
        assert_eq!(*count2, 0);
    }
}
