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
use alexandria_engine::dreaming::{Intervals, Job, JobReport};
use alexandria_storage::repos::ClusterRepo;
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
}

impl Jobs {
    pub(crate) fn new(db: Arc<Database>, config: &Config) -> Self {
        Self {
            db,
            intervals: config.dreaming.intervals(),
            cohesion_floor: config.cluster.cohesion_floor,
            merge_threshold: config.cluster.merge_threshold,
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

    async fn run_job(&self, job: Job) -> anyhow::Result<JobReport> {
        match job {
            Job::Cluster => self.run_cluster().await,
            Job::Merge => self.run_merge().await,
            // Implemented in the commits that follow this one; the dispatch is total so a new job
            // cannot silently fall through a `match` arm.
            job @ (Job::Sweep | Job::Collapse | Job::Appraise) => {
                tracing::debug!(job = job.as_str(), "dreaming job not yet implemented");
                Ok(JobReport::new(job))
            }
        }
    }

    /// Cohesion check → split. Moved out of `main.rs` verbatim: same ordering, same per-cluster
    /// member read, same logged outcomes.
    async fn run_cluster(&self) -> anyhow::Result<JobReport> {
        let cluster_repo = ClusterRepo::new(self.db.inner());
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
    async fn run_merge(&self) -> anyhow::Result<JobReport> {
        let cluster_repo = ClusterRepo::new(self.db.inner());
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
        for job in schedule.due(now) {
            match jobs.run_job(job).await {
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

    #[tokio::test]
    async fn the_cluster_and_merge_jobs_survive_an_empty_database() {
        let db = migrated_db().await;
        let jobs = Jobs::new(db, &Config::default());

        // Both jobs used to live inline in `main.rs`, where an early `continue` on a query error was
        // the only failure handling. They now return `Result`, so "no clusters at all" has to be the
        // success case rather than an error — a fresh database is the common case on first boot.
        let cluster = jobs
            .run_job(Job::Cluster)
            .await
            .expect("empty is not an error");
        let merge = jobs
            .run_job(Job::Merge)
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
}
