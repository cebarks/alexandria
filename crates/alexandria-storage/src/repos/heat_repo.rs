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

    /// A caller saw the row: reset heat, store the grown stability, count the access, and stamp
    /// **both** clocks. The only write allowed to move `last_accessed_at`.
    ///
    /// Targets by memory id, like [`HeatRepo::get`] and [`HeatRepo::add_heat`], so no caller has to
    /// hold the `heat_state` record id as well. Like `add_heat`, it matches zero rows when there is
    /// no `heat_state` for the memory — `MemoryRepo::create_fact` does not create one. The caller
    /// has already read the row to compute these values, so that cannot happen on the paths that
    /// use this; the `checked` return is `.check()?`ed rather than swallowed.
    pub async fn record_access(
        &self,
        memory_id: &str,
        heat: f64,
        stability: f64,
        access_count: i64,
    ) -> Result<()> {
        self.db
            .query(
                "UPDATE heat_state SET \
                 heat = $heat, \
                 stability = $stability, \
                 access_count = $access_count, \
                 last_touched = time::now(), \
                 last_accessed_at = time::now() \
                 WHERE memory = type::record($memory_id)",
            )
            .bind(("memory_id", memory_id.to_string()))
            .bind(("heat", heat))
            .bind(("stability", stability))
            .bind(("access_count", access_count))
            .await?
            .check()?;
        Ok(())
    }

    /// Materialise the decayed value: write `heat` and re-anchor `last_touched`, nothing else.
    ///
    /// This must never mention `last_accessed_at`, `stability` or `access_count`. Re-anchoring the
    /// decay clock is a no-op on the projection curve, but if the same field were also the spacing
    /// reference it would cap that ratio at `sweep_interval / spacing_reference` (~0.042 at the
    /// defaults) and under-grow stability ~24x — the reason `last_accessed_at` exists. Use
    /// [`HeatRepo::record_access`] for an actual access.
    pub async fn materialize_heat(&self, memory_id: &str, heat: f64) -> Result<()> {
        self.db
            .query(
                "UPDATE heat_state SET \
                 heat = $heat, \
                 last_touched = time::now() \
                 WHERE memory = type::record($memory_id)",
            )
            .bind(("memory_id", memory_id.to_string()))
            .bind(("heat", heat))
            .await?
            .check()?;
        Ok(())
    }

    /// Add heat to a memory's heat_state (for spreading activation).
    ///
    /// Only increases heat. It must NOT touch `stability`, `access_count` or `last_accessed_at`:
    /// activation warms a neighbour the caller never saw, and moving the access stamp here would
    /// credit the next real access as recent. `last_touched` does move, because this writes `heat`
    /// and the anchor must describe the value that is stored.
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

    // The three tests below pin the two-clock contract at the SQL layer. Every one of them seeds
    // through `create_for_memory`, because `MemoryRepo::create_fact` creates no `heat_state` row
    // and `add_heat`/`update` are `UPDATE ... WHERE memory = ...` that silently match zero rows —
    // a "did not write" assertion on a fixture that cannot write passes vacuously. Each therefore
    // carries a positive control proving the write landed.

    /// `add_heat` is spreading activation, not an access. Its doc comment has always promised it
    /// leaves `stability` and `access_count` alone; the field that actually matters is
    /// `last_accessed_at`, the spacing reference. Warming a neighbour must not make the next real
    /// access look recent, or spaced repetition never earns full stability growth.
    #[tokio::test]
    async fn add_heat_moves_the_decay_anchor_not_the_access_stamp() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let memory = crate::repos::MemoryRepo::new(db.inner());
        let heat = HeatRepo::new(db.inner());
        let fid = memory
            .create_fact("activation target", 0.5, &vec![0.1_f32; 384], &[])
            .await
            .unwrap();
        heat.create_for_memory(&fid, 1.0).await.unwrap();

        heat.add_heat(&fid, 0.5).await.unwrap();
        let after = heat.get(&fid).await.unwrap().expect("row exists");

        assert_eq!(
            after.heat, 1.5,
            "positive control: the warm must actually write"
        );
        assert!(
            after.last_accessed_at.is_none(),
            "activation must not be recorded as an access"
        );
        assert!(
            after.last_touched.is_some(),
            "and must still anchor the value it wrote"
        );
    }

    /// `HeatRepo::update` used to be one method hardcoding `last_touched = time::now()` for every
    /// caller, which cannot serve both the sweep and an access. These are the two intents, kept
    /// separate so a later "simplification" back into one method fails a test rather than
    /// silently re-merging the clocks.
    #[tokio::test]
    async fn materialize_heat_touches_only_the_decay_side() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let memory = crate::repos::MemoryRepo::new(db.inner());
        let heat = HeatRepo::new(db.inner());
        let fid = memory
            .create_fact("swept row", 0.5, &vec![0.1_f32; 384], &[])
            .await
            .unwrap();
        heat.create_for_memory(&fid, 1.0).await.unwrap();
        heat.record_access(&fid, 1.0, 3.0, 2).await.unwrap();

        // Move the access stamp to a fixed instant in the past. Asserting equality against the
        // value `record_access` just wrote would pass by accident if `materialize_heat` also
        // re-stamped it inside the same tick — the two `time::now()` calls can be identical.
        db.inner()
            .query(
                "UPDATE heat_state SET last_accessed_at = type::datetime('2020-01-01T00:00:00Z') \
                 WHERE memory = type::record($mid)",
            )
            .bind(("mid", fid.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();

        let before = heat.get(&fid).await.unwrap().unwrap();
        let epoch: chrono::DateTime<chrono::Utc> = "2020-01-01T00:00:00Z".parse().unwrap();
        assert_eq!(
            before.last_accessed_at,
            Some(epoch),
            "fixture control: the row must carry a known past access stamp to preserve"
        );

        heat.materialize_heat(&fid, 0.25).await.unwrap();
        let after = heat.get(&fid).await.unwrap().unwrap();

        assert_eq!(
            after.heat, 0.25,
            "positive control: the sweep writes the decayed value"
        );
        assert_eq!(
            after.stability, before.stability,
            "the sweep must not grow stability"
        );
        assert_eq!(after.access_count, before.access_count);
        assert_eq!(
            after.last_accessed_at, before.last_accessed_at,
            "the sweep must not re-stamp the access clock"
        );
        // The decay anchor moved while the access stamp did not — that is the whole split, in one
        // pair of assertions. `>` holds here because the stamp is years old.
        assert!(
            after.last_touched.unwrap() > after.last_accessed_at.unwrap(),
            "the decay anchor must have moved past the preserved access stamp"
        );
    }

    #[tokio::test]
    async fn record_access_writes_every_field_it_owns() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let memory = crate::repos::MemoryRepo::new(db.inner());
        let heat = HeatRepo::new(db.inner());
        let fid = memory
            .create_fact("accessed row", 0.5, &vec![0.1_f32; 384], &[])
            .await
            .unwrap();
        heat.create_for_memory(&fid, 1.0).await.unwrap();

        heat.record_access(&fid, 1.0, 2.0, 1).await.unwrap();
        let after = heat.get(&fid).await.unwrap().unwrap();

        assert_eq!(after.stability, 2.0);
        assert_eq!(after.access_count, 1);
        assert!(after.last_accessed_at.is_some());
        assert!(after.last_touched.is_some());
    }
}
