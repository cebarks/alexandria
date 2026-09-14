use anyhow::Result;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{SurrealValue, ToSql};

use crate::models::HeatState;

/// Band labels for [`HeatRepo::heat_histogram`], in render order.
///
/// Lives next to the repo that produces the counts so the two cannot drift silently; the
/// histogram's meaning is the band edges, which are written once in that query.
pub const HEAT_BANDS: [&str; 4] = ["below 1", "1 to 2", "2 to 3", "3 and above"];

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

    /// Heat-state counts per fixed band, in [`HEAT_BANDS`] order.
    ///
    /// Four bounded aggregates in one round trip rather than `SELECT heat FROM heat_state`
    /// tallied in Rust: the latter pulls a row per memory just to bin a number the store can
    /// bin for free. The first band has no lower edge so a negative heat value still lands in
    /// exactly one band rather than vanishing from the histogram.
    pub async fn heat_histogram(&self) -> Result<Vec<usize>> {
        #[derive(serde::Deserialize, SurrealValue)]
        struct CountRow {
            count: i64,
        }

        // The four statements are positional against HEAT_BANDS; changing one means changing
        // the other, and `test_heat_histogram_bins_are_exclusive` pins the edges.
        let mut response = self
            .db
            .query(
                "SELECT count() FROM heat_state WHERE heat < 1 GROUP ALL; \
                 SELECT count() FROM heat_state WHERE heat >= 1 AND heat < 2 GROUP ALL; \
                 SELECT count() FROM heat_state WHERE heat >= 2 AND heat < 3 GROUP ALL; \
                 SELECT count() FROM heat_state WHERE heat >= 3 GROUP ALL",
            )
            .await?;

        let mut counts = Vec::with_capacity(HEAT_BANDS.len());
        for index in 0..HEAT_BANDS.len() {
            let rows: Vec<CountRow> = response.take(index)?;
            counts.push(rows.first().map(|r| r.count as usize).unwrap_or(0));
        }
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;

    #[tokio::test]
    async fn test_heat_histogram_bins_are_exclusive() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let heat = HeatRepo::new(db.inner());
        let memory = crate::repos::MemoryRepo::new(db.inner());

        // One row per side of every band edge, plus a negative value that must land in the
        // open-ended first band rather than vanishing from the histogram entirely.
        for (i, value) in [-2.0, 0.999, 1.0, 1.5, 2.0, 2.999, 3.0, 7.5]
            .iter()
            .enumerate()
        {
            let fid = memory
                .create_fact(&format!("f{i}"), 0.5, &[0.1], &[])
                .await
                .unwrap();
            heat.create_for_memory(&fid, *value).await.unwrap();
        }

        let counts = heat.heat_histogram().await.unwrap();
        assert_eq!(counts.len(), HEAT_BANDS.len());
        assert_eq!(
            counts,
            vec![2, 2, 2, 2],
            "band edges must not double count or drop a value"
        );
        assert_eq!(counts.iter().sum::<usize>(), 8);
    }

    #[tokio::test]
    async fn test_heat_histogram_on_empty_table() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let heat = HeatRepo::new(db.inner());
        assert_eq!(heat.heat_histogram().await.unwrap(), vec![0, 0, 0, 0]);
    }
}
