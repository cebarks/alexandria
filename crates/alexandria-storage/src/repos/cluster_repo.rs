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
}
