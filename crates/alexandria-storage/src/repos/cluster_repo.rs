use anyhow::Result;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, ToSql};
use surrealdb::Surreal;

use crate::models::{Cluster, Fact};

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

    /// List all clusters along with their live member counts.
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
