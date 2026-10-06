use anyhow::Result;
use chrono::{DateTime, Utc};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{SurrealValue, ToSql};

use crate::models::HeatState;

/// Band labels for [`HeatRepo::heat_histogram`], in render order.
///
/// Lives next to the repo that produces the counts so the two cannot drift silently; the
/// histogram's meaning is the band edges, which are written once in that query.
pub const HEAT_BANDS: [&str; 4] = ["below 1", "1 to 2", "2 to 3", "3 and above"];

/// One row's new heat values, as computed by the caller — the engine owns the maths and storage
/// never decays anything.
pub struct HeatUpdate {
    pub memory_id: String,
    pub heat: f64,
    pub stability: f64,
    pub access_count: i64,
}

/// One row the sweep wants to materialise: the value to write, and the decay anchor the caller read
/// to compute it.
///
/// The anchor turns the write into a claim instead of a clobber. `Sweep` projects heat from
/// `(heat, last_touched)`; an access landing between the page read and the batched write sets
/// `heat = 1.0` **and** re-stamps the anchor, so an unconditional absolute write would put the stale
/// projection back and date it today — and every reader projects from the anchor, so the row would
/// then be durably cold rather than briefly wrong. That corrupts the one field #43 is wiring toward
/// ranking, through a wider window than the accepted lost-increment on the retrieve side. Same
/// discipline as `ReminderRepo::record_delivery`, which claims on the value it read.
///
/// `expected_anchor` is `None` for a row nobody has ever dated, and `last_touched = NONE` is the
/// test that matches it — `IS NULL` would match a present field too.
pub struct HeatMaterialise {
    pub memory_id: String,
    pub heat: f64,
    pub expected_anchor: Option<DateTime<Utc>>,
}

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

    /// Read the heat rows for a set of memories in **one** round trip.
    ///
    /// Retrieval needs every returned row's heat state to record the access, and `get` would cost
    /// one query per result. `limit` bounds this (default retrieval returns ~10), so the
    /// OR-expansion of `type::record($mN)` predicates is deliberately preferred over a gamble on
    /// which of `IN` / `INSIDE` the pinned 3.2.4 engine accepts for a record array — the whole
    /// point is one round trip, not a cleverer one.
    ///
    /// Returns only the rows that exist. A memory with no `heat_state` (imported chunks, or
    /// anything created through `MemoryRepo::create_fact`) is simply absent; callers decide.
    pub async fn get_many(&self, memory_ids: &[String]) -> Result<Vec<HeatState>> {
        if memory_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut prepared = self.db.query(Self::get_many_sql(memory_ids.len()));
        for (i, id) in memory_ids.iter().enumerate() {
            prepared = prepared.bind((format!("m{i}"), id.clone()));
        }
        let mut response = prepared.await?;
        Ok(response.take(0)?)
    }

    /// Write several accesses in **one** round trip, as one multi-statement query.
    ///
    /// The per-row statements cannot be merged into one `UPDATE ... WHERE memory IN $ids` because
    /// each row gets its own `heat`/`stability` values, so the batching is in the round trip, not
    /// the statement count. Named parameters are per-row (`$h0`, `$s0`, …) because bindings are
    /// shared across the statements of a single `query()`.
    ///
    /// Unlike `add_heat`, whose caller discards the result with `.ok()`, this reports how many rows
    /// were actually written: a zero-row `UPDATE` is not a query error, so `.check()?` alone would
    /// not notice a row that vanished between the read and the write. `access_count = 0` is the
    /// field `Appraise` demotes on, so a silently-skipped access has to be visible to the caller.
    pub async fn record_access_many(&self, updates: &[HeatUpdate]) -> Result<usize> {
        if updates.is_empty() {
            return Ok(0);
        }
        let mut query = String::new();
        for (i, _) in updates.iter().enumerate() {
            query.push_str(&format!(
                "UPDATE heat_state SET heat = $h{i}, stability = $s{i}, access_count = $a{i}, \
                 last_touched = time::now(), last_accessed_at = time::now() \
                 WHERE memory = type::record($m{i}); "
            ));
        }
        let mut prepared = self.db.query(query);
        for (i, u) in updates.iter().enumerate() {
            prepared = prepared
                .bind((format!("m{i}"), u.memory_id.clone()))
                .bind((format!("h{i}"), u.heat))
                .bind((format!("s{i}"), u.stability))
                .bind((format!("a{i}"), u.access_count));
        }
        let mut response = prepared.await?;
        // Positional per-statement results, the same shape `heat_histogram` relies on. A statement
        // that matched nothing yields an empty Vec rather than an error; a statement that errored
        // surfaces through `take`. `check` consumes the response, so it runs last.
        let mut written = 0usize;
        for i in 0..updates.len() {
            let rows: Vec<HeatState> = response.take(i)?;
            written += rows.len();
        }
        response.check()?;
        Ok(written)
    }

    /// The statement behind [`Self::get_many`], as a function of the id count.
    ///
    /// Split out because an index plan is a property of the *shipped string*, and a test that
    /// EXPLAINs a hand-written approximation of it can stay green while the real query degrades —
    /// which is exactly how this method's OR-chain came to be pinned by a single-equality query.
    /// Same reasoning that made `MemoryRepo::list_query` and `knn_sql` builders.
    pub fn get_many_sql(count: usize) -> String {
        let mut query = String::from("SELECT * FROM heat_state WHERE ");
        for i in 0..count {
            if i > 0 {
                query.push_str(" OR ");
            }
            query.push_str(&format!("memory = type::record($m{i})"));
        }
        query
    }

    /// The statement behind [`Self::page_oldest`], extracted for the same reason as
    /// [`Self::get_many_sql`]: the pin has to name the query the sweep issues, including the compound
    /// `ORDER BY` a single-field index cannot necessarily satisfy.
    pub fn page_oldest_sql() -> &'static str {
        "SELECT * FROM heat_state ORDER BY last_touched ASC, id ASC LIMIT $limit"
    }

    /// Page the `limit` rows with the oldest decay anchor, oldest first.
    ///
    /// `Sweep` walks the corpus in this order and re-anchors what it touches, so the rows it writes
    /// move to the back of the ordering and the next run picks up where this one stopped: a corpus
    /// larger than `max_rows_per_run` drains across ticks instead of the same first page being
    /// swept forever.
    ///
    /// The `id` tiebreak is not decoration. `ORDER BY last_touched ASC LIMIT n` alone leaves the
    /// order among equal timestamps unspecified, so a page boundary landing inside a tie could skip
    /// rows or re-read the same ones indefinitely — the same reason `SessionRepo::list` needs a
    /// unique secondary key. Rows whose `last_touched` is `NONE` sort first, which is what a sweep
    /// wants: an unanchored row is the one whose stored heat is least trustworthy.
    pub async fn page_oldest(&self, limit: usize) -> Result<Vec<HeatState>> {
        let mut response = self
            .db
            .query(Self::page_oldest_sql())
            .bind(("limit", limit as i64))
            .await?;
        Ok(response.take(0)?)
    }

    /// Materialise heat for many rows in **one** round trip.
    ///
    /// The batched twin of [`Self::materialize_heat`], under the same restriction: it writes `heat`
    /// and the decay anchor and nothing else. `Sweep` examines up to `max_rows_per_run` rows an
    /// hour, so per-row round trips would be most of the job's cost — and `heat_state.memory` is
    /// indexed as of v008, which is what makes the batched form cheap rather than merely fewer
    /// trips.
    ///
    /// Returns the number of rows actually written, which is also the number of claims won: a row
    /// whose anchor moved under us matches nothing, so the caller sees it as not-written and the
    /// access that beat it survives. As with [`Self::record_access_many`], a zero-row `UPDATE` is
    /// not a query error, so `.check()?` alone would not notice either case.
    pub async fn materialize_heat_many(&self, rows: &[HeatMaterialise]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut query = String::new();
        for i in 0..rows.len() {
            query.push_str(&format!(
                "UPDATE heat_state SET heat = $h{i}, last_touched = time::now() \
                 WHERE memory = type::record($m{i}) AND last_touched = $t{i}; "
            ));
        }
        let mut prepared = self.db.query(query);
        for (i, row) in rows.iter().enumerate() {
            prepared = prepared
                .bind((format!("m{i}"), row.memory_id.clone()))
                .bind((format!("h{i}"), row.heat))
                .bind((format!("t{i}"), row.expected_anchor));
        }
        let mut response = prepared.await?;
        let mut written = 0usize;
        for i in 0..rows.len() {
            let updated: Vec<HeatState> = response.take(i)?;
            written += updated.len();
        }
        response.check()?;
        Ok(written)
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

    /// The batch read is what keeps retrieval from costing one round trip per result. It must
    /// return every row that exists and invent none.
    #[tokio::test]
    async fn get_many_returns_only_existing_rows() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let memory = crate::repos::MemoryRepo::new(db.inner());
        let heat = HeatRepo::new(db.inner());

        let mut seeded = Vec::new();
        for n in 0..3 {
            let fid = memory
                .create_fact(&format!("row {n}"), 0.5, &vec![0.1_f32; 384], &[])
                .await
                .unwrap();
            heat.create_for_memory(&fid, 1.0).await.unwrap();
            seeded.push(fid);
        }
        // A fact with no heat_state row: exactly what an imported chunk or a create_fact-only row
        // looks like, and the case a caller has to decide about.
        let rowless = memory
            .create_fact("no heat row", 0.5, &vec![0.1_f32; 384], &[])
            .await
            .unwrap();

        let mut ids = seeded;
        ids.push(rowless);
        let rows = heat.get_many(&ids).await.unwrap();
        assert_eq!(
            rows.len(),
            3,
            "the row-less memory must be absent, not fabricated"
        );
        assert!(heat.get_many(&[]).await.unwrap().is_empty());
    }

    /// Distinct values per row are the proof that the parameters are per-statement. Reusing `$h`
    /// for every statement would write row 0's values to all of them and still report success.
    #[tokio::test]
    async fn record_access_many_writes_each_row_its_own_values() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let memory = crate::repos::MemoryRepo::new(db.inner());
        let heat = HeatRepo::new(db.inner());

        let mut ids = Vec::new();
        for n in 0..2 {
            let fid = memory
                .create_fact(&format!("batched {n}"), 0.5, &vec![0.1_f32; 384], &[])
                .await
                .unwrap();
            heat.create_for_memory(&fid, 1.0).await.unwrap();
            ids.push(fid);
        }

        let written = heat
            .record_access_many(&[
                HeatUpdate {
                    memory_id: ids[0].clone(),
                    heat: 0.25,
                    stability: 2.0,
                    access_count: 1,
                },
                HeatUpdate {
                    memory_id: ids[1].clone(),
                    heat: 0.75,
                    stability: 9.0,
                    access_count: 7,
                },
            ])
            .await
            .unwrap();
        assert_eq!(written, 2);

        let first = heat.get(&ids[0]).await.unwrap().unwrap();
        let second = heat.get(&ids[1]).await.unwrap().unwrap();
        assert_eq!(
            (first.heat, first.stability, first.access_count),
            (0.25, 2.0, 1)
        );
        assert_eq!(
            (second.heat, second.stability, second.access_count),
            (0.75, 9.0, 7),
            "each statement must carry its own values"
        );
        assert!(second.last_accessed_at.is_some());

        // The zero-row case is reported rather than swallowed, which is the whole reason this
        // method returns a count and `add_heat` does not.
        let rowless = memory
            .create_fact("still no heat row", 0.5, &vec![0.1_f32; 384], &[])
            .await
            .unwrap();
        let written = heat
            .record_access_many(&[HeatUpdate {
                memory_id: rowless,
                heat: 1.0,
                stability: 1.0,
                access_count: 1,
            }])
            .await
            .unwrap();
        assert_eq!(
            written, 0,
            "an UPDATE matching no row must be visible to the caller"
        );
    }

    /// `idx_heat_state_memory` is what makes the per-prompt heat write cheap, and a duration is not
    /// a reproducible CI guard. Measured with a throwaway probe on 2026-10-02: ten batched access
    /// writes cost 11.7ms against a 100-row corpus and 122ms against 1000 rows (a full scan per
    /// statement); with the index, 1.2ms at both sizes, flat in corpus size.
    ///
    /// So the pin is the plan, not the clock — the same reasoning behind
    /// `test_nearest_indexed_plan_uses_the_hnsw_index`: results cannot tell an index scan from a
    /// table scan. It asserts the `SELECT` form because this engine rejects `EXPLAIN UPDATE` outright
    /// (`"EXPLAIN is only supported with the new execution model"`), and the update path resolves
    /// its rows through the identical predicate, so it is the same lookup.
    #[tokio::test]
    async fn heat_lookups_by_memory_reach_the_index() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let memory = crate::repos::MemoryRepo::new(db.inner());
        let fid = memory
            .create_fact("indexed lookups", 0.5, &vec![0.1_f32; 384], &[])
            .await
            .unwrap();
        HeatRepo::new(db.inner())
            .create_for_memory(&fid, 1.0)
            .await
            .unwrap();

        let plan = |statement: String| {
            let db = db.inner().clone();
            let fid = fid.clone();
            async move {
                let mut response = db
                    .query(format!("EXPLAIN FORMAT JSON {statement}"))
                    .bind(("mid", fid.clone()))
                    .bind(("m0", fid.clone()))
                    .bind(("m1", fid.clone()))
                    .bind(("m2", fid.clone()))
                    .bind(("limit", 5i64))
                    .await
                    .unwrap();
                let plan: surrealdb::types::Value = response.take(0).unwrap();
                plan.to_sql().replace(' ', "")
            }
        };

        let lookup = plan(HeatRepo::get_many_sql(3)).await;
        assert!(
            lookup.contains("idx_heat_state_memory") && lookup.contains("IndexScan"),
            "the per-prompt heat lookup must be an index scan, not a table scan: {lookup}"
        );

        // The sweep's page, pinned here rather than in its own task so the index it depends on
        // cannot be dropped without failing something before the job exists. Asserted against
        // `page_oldest_sql()` — the compound `ORDER BY last_touched ASC, id ASC` is precisely the
        // detail a hand-written proxy would have missed, since a single-field index does not
        // obviously satisfy a two-column sort.
        let sweep = plan(HeatRepo::page_oldest_sql().to_string()).await;
        assert!(
            sweep.contains("idx_heat_state_last_touched"),
            "the sweep page must be served by an index scan on the decay anchor, not a table scan \
             plus a sort of the whole `heat_state` table: {sweep}"
        );
    }

    /// `page_oldest` is the sweep's cursor, so both its ordering and its bound are load-bearing:
    /// the ordering decides which rows decay first, and the bound is what keeps an hourly job from
    /// reading the whole table.
    #[tokio::test]
    async fn page_oldest_walks_from_the_stalest_anchor_and_stops_at_the_limit() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let heat = HeatRepo::new(db.inner());
        let memory = crate::repos::MemoryRepo::new(db.inner());

        let mut ids = Vec::new();
        for (i, day) in ["2020-01-01", "2021-01-01", "2022-01-01"]
            .iter()
            .enumerate()
        {
            let fid = memory
                .create_fact(&format!("f{i}"), 0.5, &[0.1], &[])
                .await
                .unwrap();
            heat.create_for_memory(&fid, 1.0).await.unwrap();
            db.inner()
                .query(format!(
                    "UPDATE heat_state SET last_touched = d'{day}T00:00:00Z' \
                     WHERE memory = type::record($m)"
                ))
                .bind(("m", fid.clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
            ids.push(fid);
        }

        let page = heat.page_oldest(2).await.unwrap();
        assert_eq!(page.len(), 2, "the limit bounds the read");
        let anchors: Vec<i64> = page
            .iter()
            .map(|row| row.last_touched.unwrap().timestamp())
            .collect();
        assert!(anchors[0] < anchors[1], "stalest anchor first: {anchors:?}");
        assert_eq!(
            page.iter()
                .map(|row| crate::record_id_to_string(&row.memory))
                .collect::<Vec<_>>(),
            vec![ids[0].clone(), ids[1].clone()],
            "the two stalest rows are the first page, so the tail of a large corpus is reached by \
             later runs rather than never"
        );
    }

    /// Each row in a batch gets its own value, and the count is the number of rows actually written.
    /// The per-row `$h{i}` naming is what makes the first half true: bindings are shared across the
    /// statements of one `query()`, so a single `$heat` would write the last bound value to every
    /// row and the sweep would flatten the corpus to one number.
    #[tokio::test]
    async fn materialize_heat_many_writes_each_rows_own_value() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let heat = HeatRepo::new(db.inner());
        let memory = crate::repos::MemoryRepo::new(db.inner());

        let mut ids = Vec::new();
        for i in 0..2 {
            let fid = memory
                .create_fact(&format!("m{i}"), 0.5, &[0.1], &[])
                .await
                .unwrap();
            heat.create_for_memory(&fid, 1.0).await.unwrap();
            ids.push(fid);
        }

        let written = heat
            .materialize_heat_many(&[
                HeatMaterialise {
                    memory_id: ids[0].clone(),
                    heat: 0.25,
                    expected_anchor: heat.get(&ids[0]).await.unwrap().unwrap().last_touched,
                },
                HeatMaterialise {
                    memory_id: ids[1].clone(),
                    heat: 0.75,
                    expected_anchor: heat.get(&ids[1]).await.unwrap().unwrap().last_touched,
                },
            ])
            .await
            .unwrap();
        assert_eq!(written, 2);

        let first = heat.get(&ids[0]).await.unwrap().unwrap();
        let second = heat.get(&ids[1]).await.unwrap().unwrap();
        assert_eq!((first.heat, second.heat), (0.25, 0.75), "per-row values");
        assert_eq!(
            (first.access_count, second.access_count),
            (0, 0),
            "materialisation is not an access, so the count Appraise demotes on must not move"
        );

        // A row that vanished between the read and the write is reported, not swallowed: a zero-row
        // UPDATE is not a query error, so `.check()?` alone would call this a success.
        let written = heat
            .materialize_heat_many(&[HeatMaterialise {
                memory_id: "fact:no_such_row".to_string(),
                heat: 0.5,
                expected_anchor: None,
            }])
            .await
            .unwrap();
        assert_eq!(written, 0);
    }

    /// The write is a claim, not a clobber. A row accessed after the sweep read it must keep that
    /// access, and the sweep's stale projection must be the thing that loses.
    ///
    /// Without the anchor in the WHERE clause the same write restores the pre-access heat **and**
    /// re-dates the anchor — so the corruption is not a transient every later projection fixes, it is
    /// durable: readers project from `(heat, last_touched)`, and the row now reads as cold at
    /// today's date for the rest of its life. That is the field #43 is wiring toward ranking.
    #[tokio::test]
    async fn materialize_heat_many_leaves_a_row_accessed_after_the_read_alone() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let heat = HeatRepo::new(db.inner());
        let memory = crate::repos::MemoryRepo::new(db.inner());

        let fid = memory
            .create_fact("claimed", 0.5, &[0.1], &[])
            .await
            .unwrap();
        heat.create_for_memory(&fid, 1.0).await.unwrap();
        let stale_anchor = heat.get(&fid).await.unwrap().unwrap().last_touched;

        // Stand in for an access beating the sweep to the write. Spelled as a fixed far-future date
        // rather than through `record_access_many`, because that stamps `time::now()` and two calls
        // this close together can land on the same instant — which would make the claim below match,
        // and the test would then be asserting whichever way the clock happened to fall.
        db.inner()
            .query(
                "UPDATE heat_state SET heat = 1.0, access_count = 1, \
                 last_touched = d'2099-01-01T00:00:00Z' WHERE memory = type::record($m)",
            )
            .bind(("m", fid.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();

        let written = heat
            .materialize_heat_many(&[HeatMaterialise {
                memory_id: fid.clone(),
                heat: 0.01,
                expected_anchor: stale_anchor,
            }])
            .await
            .unwrap();
        assert_eq!(
            written, 0,
            "a claim against an anchor that moved matches nothing"
        );

        let row = heat.get(&fid).await.unwrap().unwrap();
        assert_eq!(
            row.heat, 1.0,
            "the access wins, not the sweep's stale projection"
        );
        assert_eq!(
            row.access_count, 1,
            "and the count `Appraise` demotes on survived the lost write"
        );

        // Positive control: the identical write carrying the anchor the reader actually holds does
        // land. Without this, every assertion above would be satisfied by a method that never writes.
        let written = heat
            .materialize_heat_many(&[HeatMaterialise {
                memory_id: fid.clone(),
                heat: 0.01,
                expected_anchor: row.last_touched,
            }])
            .await
            .unwrap();
        assert_eq!(written, 1, "the same claim with a current anchor wins");
        assert_eq!(
            heat.get(&fid).await.unwrap().unwrap().heat,
            0.01,
            "and the value is written"
        );
    }
}
