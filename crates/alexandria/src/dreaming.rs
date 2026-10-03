//! The dreaming scheduler: one loop, five independently-due jobs (GitHub issue #43).
//!
//! Before this, cluster maintenance was a `tokio::spawn` in `main.rs` with a single
//! `tokio::time::interval`: one cadence for two distinct phases, no way to add a third job without
//! changing the cadence of the first two, and a failure anywhere in the loop body skipped the rest
//! of that tick. The jobs now share a loop but not a clock — each carries its own interval and its
//! own last-run stamp, so one failing or one being slow affects only itself.
//!
//! HTTP mode only, same as before: stdio has no long-lived process to run a clock in.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alexandria_engine::clusters::maintenance::{
    MaintenanceAction, MergeCheck, check_cohesion, check_merge,
};
use alexandria_engine::dreaming::collapse::{Candidate, UNKNOWN_CREATED_AT, duplicate_groups};
use alexandria_engine::dreaming::{Intervals, Job, JobReport};
use alexandria_engine::heat::HeatColumns;
use alexandria_storage::models::maintenance::{action, disposition, job as job_name};
use alexandria_storage::repos::{
    AuditContext, ClusterRepo, EdgeRepo, HeatRepo, LogEntry, MaintenanceRepo, MemoryRepo,
};
use alexandria_storage::{Database, record_id_to_string};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Config;

/// Wall clock in whole seconds. The engine stays clock-free so its due-time arithmetic is testable;
/// this is the one place the process supplies a clock.
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Everything the jobs need, captured once. Nothing here is read from config at call time, so a
/// job cannot observe a different setting than the one the scheduler was started with.
pub(crate) struct Jobs {
    db: Arc<Database>,
    intervals: Intervals,
    cohesion_floor: f32,
    merge_threshold: f32,
    /// `[heat] decay_tau_secs`. The sweep is the only job that projects heat, so this is the only
    /// place outside retrieval that needs the constant.
    decay_tau_secs: f64,
    max_rows_per_run: usize,
}

impl Jobs {
    pub(crate) fn new(db: Arc<Database>, config: &Config) -> Self {
        Self {
            db,
            intervals: config.dreaming.intervals(),
            cohesion_floor: config.cluster.cohesion_floor,
            merge_threshold: config.cluster.merge_threshold,
            decay_tau_secs: config.heat.decay_tau_secs,
            max_rows_per_run: config.dreaming.max_rows_per_run,
        }
    }

    /// Spawn the loop. Returns the handle so a caller can await it on shutdown; the loop exits on
    /// `cancel`, which `serve_http` already fires when the HTTP service stops.
    pub(crate) fn spawn(
        db: Arc<Database>,
        config: &Config,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        let jobs = Jobs::new(db, config);
        tokio::spawn(async move { run(jobs, cancel).await })
    }

    async fn run_job(&self, job: Job, run_id: &str) -> anyhow::Result<JobReport> {
        match job {
            Job::Sweep => self.run_sweep().await,
            Job::Cluster => self.run_cluster(run_id).await,
            Job::Merge => self.run_merge(run_id).await,
            Job::Collapse => self.run_collapse(run_id).await,
            // Implemented in the commit that follows this one; the dispatch is total so a new job
            // cannot silently fall through a `match` arm.
            job @ Job::Appraise => {
                tracing::debug!(job = job.as_str(), "dreaming job not yet implemented");
                Ok(JobReport::new(job))
            }
        }
    }

    /// Heat materialisation: project every row's decay to now and store the result, oldest anchor
    /// first, at most `max_rows_per_run` rows.
    ///
    /// Two properties are load-bearing and both are pinned by tests below. It writes `heat` and the
    /// decay anchor and **nothing else** — touching `last_accessed_at` here is the bug that made an
    /// hourly sweep cap the spacing ratio at ~0.042 and under-grow stability ~24x. And it writes no
    /// `maintenance_log` rows: a sweep changes ranking by nothing, because every reader projects
    /// heat itself, so an audit trail for it would be hundreds of rows an hour recording that
    /// nothing happened. That is also why `sweep` is absent from the v008 `job` allowlist.
    async fn run_sweep(&self) -> anyhow::Result<JobReport> {
        let heat_repo = HeatRepo::new(self.db.inner());
        let rows = heat_repo.page_oldest(self.max_rows_per_run).await?;

        let mut report = JobReport::new(Job::Sweep);
        report.examined = rows.len();
        if rows.is_empty() {
            return Ok(report);
        }

        let now = now_secs();
        let columns = HeatColumns {
            heat: rows.iter().map(|row| row.heat).collect(),
            stability: rows.iter().map(|row| row.stability).collect(),
            // A row with no anchor has nothing to decay from. Epoch reads as "ancient", which
            // projects it to ~0 — the honest interpretation of a heat value nobody can date.
            last_touched: rows
                .iter()
                .map(|row| {
                    row.last_touched
                        .map(|at| at.timestamp().max(0) as u64)
                        .unwrap_or(0)
                })
                .collect(),
        };
        let projected = columns.projected_heat_bulk(now, self.decay_tau_secs);

        // Every row in the page is written, including ones whose projected heat barely moved. A
        // change threshold would skip most writes and save almost nothing: the page is bounded by
        // `max_rows_per_run` and runs hourly, and skipping rows would leave their anchors stale,
        // which is the thing materialisation exists to fix.
        let writes: Vec<(String, f64)> = rows
            .iter()
            .zip(projected)
            .map(|(row, heat)| (record_id_to_string(&row.memory), heat))
            .collect();
        report.acted = heat_repo.materialize_heat_many(&writes).await?;
        Ok(report)
    }

    /// Cohesion check → split. Moved out of `main.rs` verbatim: same ordering, same per-cluster
    /// member read, same logged outcomes.
    async fn run_cluster(&self, run_id: &str) -> anyhow::Result<JobReport> {
        let cluster_repo = ClusterRepo::with_audit(
            self.db.inner(),
            AuditContext::dreaming(run_id, job_name::CLUSTER),
        );
        let clusters = self.all_clusters().await?;
        let mut report = JobReport::new(Job::Cluster);

        for cluster in &clusters {
            let cid = cluster
                .id
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default();
            let members = match cluster_repo.get_members(&cid).await {
                Ok(m) => m,
                // A single unreadable cluster must not abort the pass: the others are still
                // checkable, and this job runs again in five minutes.
                Err(e) => {
                    tracing::warn!("Cluster job: cannot read members of {cid}: {e}");
                    continue;
                }
            };
            report.examined += 1;
            let member_embeddings: Vec<Vec<f32>> =
                members.iter().map(|f| f.embedding.clone()).collect();

            let action = check_cohesion(
                &cid,
                &cluster.centroid,
                &member_embeddings,
                self.cohesion_floor,
            );
            if let MaintenanceAction::Split {
                cluster_id,
                group_a,
                group_b,
                centroid_a,
                centroid_b,
            } = action
            {
                tracing::info!(
                    "Splitting cluster {cluster_id} ({} / {} members)",
                    group_a.len(),
                    group_b.len()
                );
                match cluster_repo
                    .execute_split(&cid, &members, &group_a, &group_b, &centroid_a, &centroid_b)
                    .await
                {
                    Ok((cid_a, cid_b)) => {
                        tracing::info!("Split complete: {cluster_id} -> {cid_a}, {cid_b}");
                        report.acted += 1;
                    }
                    Err(e) => tracing::warn!("Split failed for {cluster_id}: {e}"),
                }
            }
        }
        Ok(report)
    }

    /// Centroid similarity → merge. Also moved verbatim, including the labelled break: after each
    /// merge the cluster set is re-read, because a merge changes the centroids the next comparison
    /// would otherwise use.
    async fn run_merge(&self, run_id: &str) -> anyhow::Result<JobReport> {
        let cluster_repo = ClusterRepo::with_audit(
            self.db.inner(),
            AuditContext::dreaming(run_id, job_name::MERGE),
        );
        let mut report = JobReport::new(Job::Merge);

        loop {
            let merge_clusters = self.all_clusters().await?;
            report.examined += merge_clusters.len();

            let mut infos: Vec<(String, Vec<f32>, usize)> = Vec::new();
            for c in &merge_clusters {
                let id = c.id.as_ref().map(record_id_to_string).unwrap_or_default();
                let count = cluster_repo
                    .get_members(&id)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                infos.push((id, c.centroid.clone(), count));
            }

            let mut merged_one = false;
            'scan: for i in 0..infos.len() {
                for j in (i + 1)..infos.len() {
                    let result = check_merge(
                        &infos[i].0,
                        &infos[i].1,
                        infos[i].2,
                        &infos[j].0,
                        &infos[j].1,
                        infos[j].2,
                        self.merge_threshold,
                    );
                    if let MergeCheck::Merge {
                        keep_id,
                        remove_id,
                        merged_centroid,
                    } = result
                    {
                        tracing::info!("Merging cluster {remove_id} into {keep_id}");
                        match cluster_repo
                            .execute_merge(&keep_id, &remove_id, &merged_centroid)
                            .await
                        {
                            Ok(()) => {
                                tracing::info!("Merge complete: {remove_id} -> {keep_id}");
                                merged_one = true;
                                report.acted += 1;
                            }
                            Err(e) => {
                                tracing::warn!("Merge failed ({remove_id} -> {keep_id}): {e}")
                            }
                        }
                        // Re-query fresh data before looking for more merges.
                        break 'scan;
                    }
                }
            }
            if !merged_one {
                break;
            }
        }
        Ok(report)
    }

    /// Byte-identical duplicate collapse: keep the strongest copy, soft-delete the rest, and leave a
    /// `derived_from` edge from each dead row to the survivor.
    ///
    /// The grouping and the survivor rule live in `alexandria_engine::dreaming::collapse`, so the
    /// decisions are tested without a database. What is here is the write order and the audit trail.
    ///
    /// The edge is written **before** the delete. Reversed, a failure between the two would leave a
    /// soft-deleted memory with no path back to the copy that replaced it — the lineage is the only
    /// thing that makes the deletion reversible, so it has to exist first.
    async fn run_collapse(&self, run_id: &str) -> anyhow::Result<JobReport> {
        let memories = MemoryRepo::new(self.db.inner());
        let edges = EdgeRepo::new(self.db.inner());
        let audit = MaintenanceRepo::with_audit(
            self.db.inner(),
            AuditContext::dreaming(run_id, job_name::COLLAPSE),
        );

        let rows = memories.collapse_candidates().await?;
        let mut report = JobReport::new(Job::Collapse);
        report.examined = rows.len();

        let candidates: Vec<Candidate> = rows
            .iter()
            .map(|row| Candidate {
                id: record_id_to_string(&row.id),
                content: row.content.clone(),
                confidence: row.confidence,
                created_at: row
                    .created_at
                    .map(|at| at.timestamp())
                    .unwrap_or(UNKNOWN_CREATED_AT),
            })
            .collect();

        'groups: for group in duplicate_groups(&candidates) {
            for collapsed in &group.collapsed {
                // The bound is on writes, not on the read. Stopping mid-group is safe: each collapsed
                // row is independent, and the next run groups the same survivors against whatever is
                // left, so nothing is skipped forever.
                if report.acted >= self.max_rows_per_run {
                    break 'groups;
                }

                if let Err(e) = edges
                    .create_edge(collapsed, &group.survivor, "derived_from", 1.0)
                    .await
                {
                    tracing::warn!(
                        "Collapse: cannot link {collapsed} -> {}: {e}",
                        group.survivor
                    );
                    continue;
                }
                if let Err(e) = memories.soft_delete_fact(collapsed).await {
                    tracing::warn!("Collapse: cannot soft-delete {collapsed}: {e}");
                    continue;
                }
                // A row that acted but failed to log is worse than the reverse, so the log write is
                // last and its failure does not unwind the delete — it is warned about instead.
                if let Err(e) = audit
                    .record(&LogEntry {
                        action: action::COLLAPSE.to_string(),
                        source_id: collapsed.clone(),
                        target_ids: vec![group.survivor.clone()],
                        members_moved: 0,
                        disposition: Some(disposition::SOFT_DELETE.to_string()),
                    })
                    .await
                {
                    tracing::warn!("Collapse: cannot log the collapse of {collapsed}: {e}");
                }
                report.acted += 1;
            }
        }
        Ok(report)
    }

    async fn all_clusters(&self) -> anyhow::Result<Vec<alexandria_storage::models::Cluster>> {
        let clusters = self
            .db
            .inner()
            .query("SELECT * FROM cluster")
            .await?
            .take(0)?;
        Ok(clusters)
    }
}

/// One-line rendering of the `[dreaming]` cadence, for the debug dashboard's config panel. Built
/// here because `DreamingConfig` lives in this crate and cannot reach `alexandria-mcp` — the same
/// constraint that makes `ClusterConfig` values arrive at `DebugContext` pre-flattened.
pub(crate) fn summary(config: &crate::config::DreamingConfig) -> String {
    if !config.enabled {
        return "off".to_string();
    }
    format!(
        "on - sweep {}s, cluster {}s, merge {}s, collapse {}s, appraise {}s, {} rows/run",
        config.sweep_interval_secs,
        config.cluster_interval_secs,
        config.merge_interval_secs,
        config.collapse_interval_secs,
        config.appraise_interval_secs,
        config.max_rows_per_run
    )
}

/// The loop. Sleeps until the soonest job is due rather than waking on a fixed tick, runs the whole
/// due set in declared order, and marks each job as run whether it succeeded or not.
///
/// Marking on failure matters: a job that errors every run would otherwise be re-queued immediately,
/// and the loop would spin on it instead of sleeping until its interval. The cost is that a
/// transient failure waits a full interval for its retry — acceptable for housekeeping, and the
/// alternative (retry immediately) is what turns one poisoned row into a hot loop.
pub(crate) async fn run(jobs: Jobs, cancel: CancellationToken) {
    let mut schedule = jobs.intervals.initial_schedule(now_secs());
    let mut tick: u64 = 0;
    tracing::info!(
        "dreaming scheduler started: sweep {}s, cluster {}s, merge {}s, collapse {}s, appraise {}s",
        jobs.intervals.sweep_secs,
        jobs.intervals.cluster_secs,
        jobs.intervals.merge_secs,
        jobs.intervals.collapse_secs,
        jobs.intervals.appraise_secs
    );

    loop {
        let wait = schedule.next_wait_secs(now_secs());
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
            _ = cancel.cancelled() => break,
        }

        let now = now_secs();
        let due = schedule.due(now);
        if due.is_empty() {
            continue;
        }
        tick += 1;
        // One id for the whole tick, shared by every job that runs in it. `run_id` is the unit an
        // operator would reverse, and a tick that split a cluster and then collapsed duplicates into
        // it is one event, not two.
        let run_id = format!("run-{now}-{tick}");
        for job in due {
            match jobs.run_job(job, &run_id).await {
                Ok(report) => tracing::debug!(
                    job = report.job.as_str(),
                    examined = report.examined,
                    acted = report.acted,
                    "dreaming job finished"
                ),
                Err(e) => tracing::warn!(job = job.as_str(), "dreaming job failed: {e}"),
            }
            schedule.mark_run(job, now);
        }
    }
    tracing::debug!("dreaming scheduler stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use alexandria_storage::Database;

    /// An embedded database with the schema applied and nothing in it: the state every first boot
    /// starts from, and the one the jobs must treat as success rather than as an error.
    async fn migrated_db() -> Arc<Database> {
        let db = Arc::new(Database::connect_embedded().await.unwrap());
        alexandria_storage::schema::migrate(db.inner())
            .await
            .expect("schema applies to a fresh database");
        db
    }

    /// A heat row with an old decay anchor and a known access history, so the sweep has something to
    /// decay and three fields it must leave alone.
    async fn seed_cold(db: &Database, content: &str) -> String {
        let memories = alexandria_storage::repos::MemoryRepo::new(db.inner());
        let heat = HeatRepo::new(db.inner());
        let id = memories
            .create_fact(content, 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        heat.create_for_memory(&id, 1.0).await.unwrap();
        db.inner()
            .query(
                "UPDATE heat_state SET last_touched = d'2020-01-01T00:00:00Z', \
                 last_accessed_at = d'2020-01-01T00:00:00Z', stability = 3.0, access_count = 7 \
                 WHERE memory = type::record($m)",
            )
            .bind(("m", id.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        id
    }

    /// Rows whose decay anchor is recent, i.e. rows a sweep has already materialised.
    async fn recently_swept(db: &Database) -> usize {
        let mut response = db
            .inner()
            .query(
                "SELECT * FROM heat_state WHERE last_touched > d'2021-01-01T00:00:00Z' \
                 ORDER BY id ASC",
            )
            .await
            .unwrap();
        let rows: Vec<alexandria_storage::models::HeatState> = response.take(0).unwrap();
        rows.len()
    }

    /// The sweep's contract in one test: heat is materialised, the anchor moves, and the three
    /// fields that belong to accesses do not. `last_accessed_at` is the one that matters — a sweep
    /// that stamped it would cap the spacing ratio at `sweep_interval / spacing_reference` (~0.042
    /// at the defaults) and under-grow stability roughly 24x, which is why the column exists.
    #[tokio::test]
    async fn the_sweep_materialises_heat_without_touching_the_access_clock() {
        let db = migrated_db().await;
        let id = seed_cold(&db, "a memory nobody has looked at in years").await;

        let jobs = Jobs::new(db.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Sweep, "run-test")
            .await
            .expect("sweep succeeds");
        assert_eq!((report.examined, report.acted), (1, 1));

        let state = HeatRepo::new(db.inner())
            .get(&id)
            .await
            .unwrap()
            .expect("row exists");
        assert!(
            state.heat < 0.01,
            "years past a one-day tau must decay to nothing, not stay at the stored 1.0: {}",
            state.heat
        );
        assert!(
            state.last_touched.expect("re-anchored").timestamp() > 1_700_000_000,
            "the anchor moves, because it has to describe the value now stored"
        );
        assert_eq!(state.stability, 3.0, "stability is earned by accesses");
        assert_eq!(state.access_count, 7, "a sweep is not an access");
        assert_eq!(
            state.last_accessed_at.expect("untouched").timestamp(),
            1_577_836_800,
            "the spacing reference must not move"
        );
    }

    /// Bounded per run, and the bound does not strand the tail of the corpus. Sweeping re-anchors
    /// what it touched, which moves those rows to the back of the oldest-first ordering, so the next
    /// run picks up the rows this one could not reach. Without that, one page would be swept forever
    /// and everything past `max_rows_per_run` would never decay at all.
    #[tokio::test]
    async fn the_sweep_is_bounded_and_drains_a_larger_corpus_across_runs() {
        let db = migrated_db().await;
        for i in 0..3 {
            seed_cold(&db, &format!("memory {i}")).await;
        }

        let mut config = Config::default();
        config.dreaming.max_rows_per_run = 2;
        let jobs = Jobs::new(db.clone(), &config);

        let first = jobs
            .run_job(Job::Sweep, "run-test")
            .await
            .expect("sweep succeeds");
        assert_eq!(
            (first.examined, first.acted),
            (2, 2),
            "one run examines at most max_rows_per_run rows"
        );
        assert_eq!(
            recently_swept(&db).await,
            2,
            "only the page was materialised"
        );

        let second = jobs
            .run_job(Job::Sweep, "run-test")
            .await
            .expect("sweep succeeds");
        assert_eq!(second.examined, 2);
        assert_eq!(
            recently_swept(&db).await,
            3,
            "the third row is reached on the next run"
        );
    }

    /// An empty corpus is the common case on first boot, and must read as a successful run that did
    /// nothing rather than as an error the scheduler logs every hour.
    #[tokio::test]
    async fn the_sweep_on_an_empty_corpus_is_a_quiet_success() {
        let db = migrated_db().await;
        let jobs = Jobs::new(db.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Sweep, "run-test")
            .await
            .expect("no rows is not an error");
        assert_eq!(
            (report.job, report.examined, report.acted),
            (Job::Sweep, 0, 0)
        );
    }

    #[tokio::test]
    async fn the_cluster_and_merge_jobs_survive_an_empty_database() {
        let db = migrated_db().await;
        let jobs = Jobs::new(db, &Config::default());

        // Both jobs used to live inline in `main.rs`, where an early `continue` on a query error was
        // the only failure handling. They now return `Result`, so "no clusters at all" has to be the
        // success case rather than an error — a fresh database is the common case on first boot.
        let cluster = jobs
            .run_job(Job::Cluster, "run-test")
            .await
            .expect("empty is not an error");
        let merge = jobs
            .run_job(Job::Merge, "run-test")
            .await
            .expect("empty is not an error");
        assert_eq!(
            (cluster.job, cluster.examined, cluster.acted),
            (Job::Cluster, 0, 0)
        );
        assert_eq!((merge.job, merge.examined, merge.acted), (Job::Merge, 0, 0));
    }

    /// The scheduler must stop when the HTTP service does, or a restarted process leaves the old
    /// loop running against the same data directory. The timeout *is* the assertion: `run` loops
    /// forever, so a loop that ignored the token would hang the suite rather than fail cleanly.
    ///
    /// Cancelled after a short delay rather than before the spawn, because `Cluster` and `Merge` are
    /// due on the first tick — cancelling first would race the sleep against the token and exercise
    /// whichever branch `select!` happened to pick.
    #[tokio::test]
    async fn the_loop_exits_when_cancelled() {
        let db = migrated_db().await;
        let jobs = Jobs::new(db, &Config::default());
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(run(jobs, cancel.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("a cancelled token must end the loop")
            .expect("the loop must not panic on the way out");
    }

    /// The dashboard row has to say something true about all five jobs, or an operator reading it
    /// concludes the scheduler is configured when it is not.
    #[test]
    fn the_summary_names_every_job_or_says_the_scheduler_is_off() {
        let config = Config::default();
        let line = summary(&config.dreaming);
        for needle in [
            "on", "sweep", "cluster", "merge", "collapse", "appraise", "rows/run",
        ] {
            assert!(
                line.contains(needle),
                "summary must mention {needle}: {line}"
            );
        }

        let mut disabled = config.dreaming.clone();
        disabled.enabled = false;
        assert_eq!(summary(&disabled), "off");
    }

    /// Two byte-identical memories collapse into the more confident one: the survivor stays
    /// retrievable, the duplicate is soft-deleted, and a `derived_from` edge points from the dead
    /// row to the copy that replaced it. That edge is what makes the deletion reversible rather than
    /// merely hidden, so it is asserted and not assumed.
    #[tokio::test]
    async fn collapse_keeps_the_stronger_copy_and_links_the_duplicate_to_it() {
        let db = migrated_db().await;
        let memories = MemoryRepo::new(db.inner());
        let weak = memories
            .create_fact("the token rotates nightly", 0.3, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let strong = memories
            .create_fact("the token rotates nightly", 0.9, &[0.1, 0.2], &[])
            .await
            .unwrap();

        let jobs = Jobs::new(db.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Collapse, "run-collapse-1")
            .await
            .expect("collapse succeeds");
        assert_eq!((report.examined, report.acted), (2, 1));

        let survivor = memories
            .get_fact(&strong)
            .await
            .unwrap()
            .expect("survivor still exists");
        assert!(!survivor.deleted, "the stronger copy stays retrievable");

        let dead = memories
            .get_fact(&weak)
            .await
            .unwrap()
            .expect("the duplicate is soft-deleted, not removed");
        assert!(dead.deleted);

        let edges = EdgeRepo::new(db.inner())
            .get_edges_for(&weak)
            .await
            .unwrap();
        let link = edges
            .iter()
            .find(|edge| edge.edge_type == "derived_from")
            .expect("the lineage edge is written before the delete");
        assert_eq!(
            link.out_node.as_ref().map(record_id_to_string),
            Some(strong.clone()),
            "the edge points from the collapsed row to the survivor"
        );
    }

    /// A daily job must not re-delete and re-log the same pair forever. Once collapsed, the duplicate
    /// is soft-deleted and therefore not a candidate, so the second run acts on nothing.
    #[tokio::test]
    async fn a_second_collapse_run_finds_nothing_to_do() {
        let db = migrated_db().await;
        let memories = MemoryRepo::new(db.inner());
        for confidence in [0.4, 0.8] {
            memories
                .create_fact("duplicate content", confidence, &[0.1, 0.2], &[])
                .await
                .unwrap();
        }

        let jobs = Jobs::new(db.clone(), &Config::default());
        let first = jobs
            .run_job(Job::Collapse, "run-1")
            .await
            .expect("collapse succeeds");
        assert_eq!(first.acted, 1);

        let second = jobs
            .run_job(Job::Collapse, "run-2")
            .await
            .expect("collapse succeeds");
        assert_eq!(
            (second.examined, second.acted),
            (1, 0),
            "the collapsed row is no longer a candidate, so the second run has nothing to do"
        );

        let logs = ClusterRepo::new(db.inner())
            .list_maintenance_logs(10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "and it wrote no second audit row");
    }

    /// `run_id` is the unit of reversal, so every row one run writes has to carry the same one,
    /// along with the job and the writer. Without that, "undo the pass that ate my memories" is a
    /// timestamp range and a guess.
    #[tokio::test]
    async fn collapse_attributes_every_row_to_the_run_that_wrote_it() {
        let db = migrated_db().await;
        let memories = MemoryRepo::new(db.inner());
        for _ in 0..3 {
            memories
                .create_fact("three identical rows", 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
        }

        let jobs = Jobs::new(db.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Collapse, "run-77")
            .await
            .expect("collapse succeeds");
        assert_eq!(
            report.acted, 2,
            "two of the three collapse into the survivor"
        );

        let logs = ClusterRepo::new(db.inner())
            .list_maintenance_logs(10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 2, "one audit row per collapsed fact");
        for log in &logs {
            assert_eq!(log.run_id.as_deref(), Some("run-77"));
            assert_eq!(log.job.as_deref(), Some("collapse"));
            assert_eq!(log.actor.as_deref(), Some("system:dreaming"));
            assert_eq!(log.disposition.as_deref(), Some("soft_delete"));
            assert_eq!(log.action, "collapse");
        }
    }

    /// The bound is on writes, and stopping mid-group must not strand the rest: the next run picks up
    /// where this one stopped, because the survivor stays live and the remaining duplicates are
    /// still candidates.
    #[tokio::test]
    async fn collapse_honours_max_rows_per_run_and_finishes_on_the_next_run() {
        let db = migrated_db().await;
        let memories = MemoryRepo::new(db.inner());
        for _ in 0..3 {
            memories
                .create_fact("bounded collapse", 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
        }

        let mut config = Config::default();
        config.dreaming.max_rows_per_run = 1;
        let jobs = Jobs::new(db.clone(), &config);

        let first = jobs
            .run_job(Job::Collapse, "run-a")
            .await
            .expect("collapse succeeds");
        assert_eq!(first.acted, 1, "one write per run");

        let second = jobs
            .run_job(Job::Collapse, "run-b")
            .await
            .expect("collapse succeeds");
        assert_eq!(second.acted, 1, "the last duplicate goes on the next run");

        let live = memories.collapse_candidates().await.unwrap();
        assert_eq!(live.len(), 1, "only the survivor is left live");
    }
}
