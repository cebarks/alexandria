use anyhow::Result;
use surrealdb::engine::any::Any;
use surrealdb::types::ToSql;
use surrealdb::Surreal;

use crate::models::HeatState;

pub struct HeatRepo<'a> {
    db: &'a Surreal<Any>,
}

impl<'a> HeatRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    pub async fn create_for_memory(&self, memory_id: &str, initial_heat: f64) -> Result<String> {
        let mut response = self
            .db
            .query(
                "CREATE heat_state SET \
                 memory = type::record($memory_id), \
                 heat = $heat, \
                 stability = 1.0, \
                 access_count = 0",
            )
            .bind(("memory_id", memory_id.to_string()))
            .bind(("heat", initial_heat))
            .await?;

        let created: Option<HeatState> = response.take(0)?;
        let state = created.ok_or_else(|| anyhow::anyhow!("Failed to create heat_state"))?;
        let id = state
            .id
            .ok_or_else(|| anyhow::anyhow!("Created heat_state has no id"))?;
        Ok(id.to_sql())
    }

    pub async fn get(&self, memory_id: &str) -> Result<Option<HeatState>> {
        let mut response = self
            .db
            .query("SELECT * FROM heat_state WHERE memory = type::record($memory_id)")
            .bind(("memory_id", memory_id.to_string()))
            .await?;
        let state: Option<HeatState> = response.take(0)?;
        Ok(state)
    }

    pub async fn update(
        &self,
        id: &str,
        heat: f64,
        stability: f64,
        access_count: i64,
    ) -> Result<()> {
        self.db
            .query(
                "UPDATE type::record($id) SET \
                 heat = $heat, \
                 stability = $stability, \
                 access_count = $access_count, \
                 last_touched = time::now()",
            )
            .bind(("id", id.to_string()))
            .bind(("heat", heat))
            .bind(("stability", stability))
            .bind(("access_count", access_count))
            .await?
            .check()?;
        Ok(())
    }

    /// Add heat to a memory's heat_state (for spreading activation).
    /// Only increases heat — does NOT touch stability or access_count.
    pub async fn add_heat(&self, memory_id: &str, heat_delta: f64) -> Result<()> {
        self.db
            .query(
                "UPDATE heat_state SET \
                 heat = heat + $delta, \
                 last_touched = time::now() \
                 WHERE memory = type::record($memory_id)",
            )
            .bind(("memory_id", memory_id.to_string()))
            .bind(("delta", heat_delta))
            .await?
            .check()?;
        Ok(())
    }
}
