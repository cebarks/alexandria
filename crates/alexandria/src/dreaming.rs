//! The dreaming scheduler: one loop, five independently-due jobs (GitHub issue #43).
//!
//! Before this, cluster maintenance was a `tokio::spawn` in `main.rs` with a single
//! `tokio::time::interval`: one cadence for two distinct phases, no way to add a third job without
//! changing the cadence of the first two, and a failure anywhere in the loop body skipped the rest
//! of that tick. The jobs now share a loop but not a clock — each carries its own interval and its
//! own last-run stamp, so one failing or one being slow affects only itself.
//!
//! HTTP mode only, same as before: stdio has no long-lived process to run a clock in.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alexandria_engine::clusters::maintenance::{
    MaintenanceAction, MergeCheck, check_cohesion, check_merge,
};
use alexandria_engine::dreaming::budget::RowBudget;
use alexandria_engine::dreaming::collapse::{Candidate, UNKNOWN_CREATED_AT, duplicate_groups};
use alexandria_engine::dreaming::dispatch::{interleave_rounds, rotate_after};
use alexandria_engine::dreaming::liveness::{Liveness, SharedLiveness};
use alexandria_engine::dreaming::{DEMOTED_CONFIDENCE, Intervals, Job, JobReport, should_demote};
use alexandria_engine::heat::{HeatColumns, HeatState as EngineHeatState, projected_heat};
use alexandria_storage::access::SystemContext;
use alexandria_storage::models::LiveConfidence;
use alexandria_storage::models::identity::Partition;
use alexandria_storage::models::maintenance::MaintenanceBudgetJob;
use alexandria_storage::models::maintenance::{action, disposition, job as job_name};
use alexandria_storage::repos::{
    AuditContext, ClusterRepo, EdgeRepo, HeatMaterialise, HeatRepo, IdentityRepo, LogEntry,
    MaintenanceRepo, MemoryRepo,
};
use alexandria_storage::{Database, RecordId, record_id_to_string, system_config};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Config;

/// The id one scheduler tick writes its audit rows under: `run-<epoch>-<tick>`.
///
/// The second component disambiguates ticks *within* one long-lived process, which is what makes it
/// meaningful there; see [`cli_run_id`] for why the CLI cannot use the same shape.
pub(crate) fn tick_run_id(now: u64, tick: u64) -> String {
    format!("run-{now}-{tick}")
}

/// The id one `alexandria dream` pass writes its audit rows under: `run-cli-<epoch>-<pid>`.
///
/// v008 puts **no `ASSERT`** on `maintenance_log.run_id` — it is a plain `option<string>`, so the
/// only constraints are uniqueness per pass and readability (the pinned engine accepts a
/// `run-cli-…` row and returns it through the same `?run=` filter `/debug/maintenance` applies —
/// `a_pass_prints_one_line_per_job_and_ends_with_its_run_id`). The `cli` component is the
/// provenance an operator needs; `actor` stays `system:dreaming`, because the code path genuinely
/// is the dreaming jobs — the difference is who pulled the trigger, and that is what the run id is
/// for.
///
/// The pid, not a tick counter: a CLI process runs exactly one pass, so a per-process counter would
/// always read `1` and say nothing, while two invocations landing in the same epoch second are
/// ordinary (`for j in sweep collapse; do alexandria dream --job $j; done`). The single-writer lock
/// does not separate those, because it excludes *concurrent* opens, not back-to-back ones.
pub(crate) fn cli_run_id(now: u64) -> String {
    format!("run-cli-{now}-{}", std::process::id())
}

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
    /// The instance's system authority, claimed once at startup and shared with the scheduler: the
    /// dispatcher enumerates partitions and persists progress under it, and every maintenance write
    /// re-asserts the partition's account is active through the M1 barrier.
    system: SystemContext,
    intervals: Intervals,
    /// Published so the dashboard can tell a stopped loop from an idle one. Owned by `Jobs` because
    /// the silence limit is derived from the intervals, and shared with `DebugContext` by `Arc`.
    liveness: SharedLiveness,
    cohesion_floor: f32,
    merge_threshold: f32,
    /// `[heat] decay_tau_secs`. The time constant both heat projections use: `sweep` materialises
    /// with it and `appraise` judges coldness with it, so it is needed by two jobs and by nothing
    /// outside retrieval. (The comment here used to claim `sweep` was the only job that projects
    /// heat, which `run_appraise`'s own projection contradicted.)
    decay_tau_secs: f64,
    max_rows_per_run: usize,
    cold_heat_floor: f64,
    demote_confidence_ceiling: f64,
}

impl Jobs {
    pub(crate) fn new(db: Arc<Database>, system: SystemContext, config: &Config) -> Self {
        let intervals = config.dreaming.intervals();
        Self {
            db,
            system,
            // Derived from the cadences rather than fixed: a loop that wakes late is not a dead one,
            // and nothing is legitimately due sooner than the shortest interval. Doubled, so one
            // overrun is not reported as a stalled scheduler.
            liveness: Arc::new(Liveness::new(silence_limit_secs(intervals))),
            intervals,
            cohesion_floor: config.cluster.cohesion_floor,
            merge_threshold: config.cluster.merge_threshold,
            decay_tau_secs: config.heat.decay_tau_secs,
            max_rows_per_run: config.dreaming.max_rows_per_run,
            cold_heat_floor: config.dreaming.cold_heat_floor,
            demote_confidence_ceiling: config.dreaming.demote_confidence_ceiling,
        }
    }

    /// Spawn the loop under a supervisor.
    ///
    /// `run` handles job *errors* — each returns `Result`, a failure is warned about and the job is
    /// still marked, so one bad job cannot abort the rest of the tick. It cannot handle a *panic*: a
    /// panic anywhere in the five bodies unwinds the single task that also hosts cluster maintenance,
    /// and until now the returned handle was dropped with nothing watching. The process then kept
    /// serving — and `/debug` kept printing the configured cadences from `summary()` — while every
    /// background write stopped for the rest of its life. That is the failure `schedule.rs`'s own
    /// header calls out: a scheduler that quietly stopped is indistinguishable from a healthy one for
    /// days, and there are now five jobs and more code in the panic path than there were before.
    ///
    /// Restart with a backoff, and give up loudly after a few *consecutive* fast failures: a loop that
    /// panics on startup will panic again, and restarting it forever converts one bug into permanent
    /// load against the store with a log line every few seconds as its only trace. A run that survived
    /// longer than `STABLE_RUN` resets the counter, so a process that panics once in a month restarts
    /// once rather than being told it has used up its allowance.
    pub(crate) fn spawn(
        db: Arc<Database>,
        system: SystemContext,
        config: &Config,
        cancel: CancellationToken,
        liveness: SharedLiveness,
    ) -> JoinHandle<()> {
        let mut jobs = Jobs::new(db, system, config);
        // The caller's handle replaces the one `new` derived, so the dashboard and the loop observe
        // the same counters. Replacing rather than borrowing keeps `Jobs::new` unchanged for the job
        // tests, which have no supervisor and no dashboard.
        jobs.liveness = liveness;
        let jobs = Arc::new(jobs);
        // The closure needs its own handle to the token for each attempt, and `supervise` needs one
        // to watch — `CancellationToken` is cloneable precisely for this, each clone observing the
        // same cancellation without the loop having to own it.
        let watch = cancel.clone();
        let reported = Arc::clone(&jobs.liveness);
        tokio::spawn(supervise(
            move || run(Arc::clone(&jobs), cancel.clone()),
            watch,
            Duration::from_secs(30),
            reported,
        ))
    }
}

/// The shared heartbeat for one process: created by `main.rs`, written by the loop and its
/// supervisor, read by the dashboard.
///
/// The silence limit comes from the same [`silence_limit_secs`] that `Jobs::new` derives, so the
/// writer and the reader cannot disagree about what "quiet" means while looking at the same struct.
pub(crate) fn shared_liveness(dreaming: &crate::config::DreamingConfig) -> SharedLiveness {
    Arc::new(Liveness::new(silence_limit_secs(dreaming.intervals())))
}

/// How long the loop may legitimately stay asleep before the dashboard calls it quiet.
///
/// Twice the shortest interval: the loop sleeps until the soonest job is due, so nothing is
/// legitimately silent longer than that, and the doubling tolerates one overrun without reporting a
/// healthy loop as stalled.
fn silence_limit_secs(intervals: Intervals) -> u64 {
    intervals.shortest_secs().saturating_mul(2)
}

/// The panic payload, if the aborted task left one worth showing an operator.
///
/// A `JoinError` carries `Box<dyn Any>`, and the two shapes that reach here from application code are
/// `&str` (a literal like `panic!("boom")`) and `String` (a formatted panic). Anything else is not
/// worth guessing at, so it reports `None` — the log line still carries the `JoinError`'s own text.
fn panic_message(error: tokio::task::JoinError) -> Option<String> {
    let payload = error.into_panic();
    if let Some(text) = payload.downcast_ref::<&str>() {
        return Some((*text).to_string());
    }
    payload.downcast_ref::<String>().cloned()
}

/// The restart policy, kept separate from the work so the give-up branch is testable without a job
/// that panics on purpose.
///
/// `start` is called once per attempt; each attempt runs in its own task, because a panic is only
/// observable as a `JoinError` — awaiting it in this task would take the supervisor down with the
/// work. A clean return, a cancellation, or a non-panic join failure all mean "stop, no restart".
///
/// `base_backoff` scales with the consecutive-failure count. It is a parameter rather than a
/// constant so the restart policy can be tested at millisecond scale: `start_paused` would need
/// tokio's `test-util` feature, and threading a test-only runtime flag through the crate's dev
/// dependencies to save six seconds of wall clock is the worse trade.
pub(crate) async fn supervise<F, Fut>(
    mut start: F,
    cancel: CancellationToken,
    base_backoff: Duration,
    liveness: SharedLiveness,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    const MAX_CONSECUTIVE_FAILURES: u32 = 3;
    const STABLE_RUN: Duration = Duration::from_secs(600);
    let mut failures: u32 = 0;

    loop {
        let started = tokio::time::Instant::now();
        let error = match tokio::spawn(start()).await {
            Ok(()) => return,
            Err(e) if cancel.is_cancelled() || !e.is_panic() => return,
            Err(e) => e,
        };

        if started.elapsed() >= STABLE_RUN {
            failures = 0;
        }
        failures += 1;

        if failures > MAX_CONSECUTIVE_FAILURES {
            liveness.note_give_up(failures);
            tracing::error!(
                "dreaming scheduler has panicked {failures} times in a row; giving up. Background \
                 housekeeping (split, merge, sweep, collapse, appraise) is STOPPED for the life of \
                 this process: {error}"
            );
            return;
        }

        let backoff = base_backoff * failures;
        // Captured before `error` is consumed for its payload: the `JoinError`'s own text names the
        // task that died, which is the part that survives a payload this code cannot downcast.
        let died = error.to_string();
        liveness.note_restart(panic_message(error));
        tracing::error!(
            "dreaming scheduler panicked ({died}); restarting ({failures}/{MAX_CONSECUTIVE_FAILURES}) \
             in {backoff:?}"
        );
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = cancel.cancelled() => return,
        }
    }
}

/// The rest of the job bodies. `spawn` above closes the first `impl Jobs` block because
/// `supervise` is a module-level function: it is generic over the attempt, and putting it inside the
/// impl would tie a testable policy function to the type it restarts.
impl Jobs {
    /// Run exactly one job, once, under `run_id`. This is the only entry to the five bodies: the
    /// scheduler's loop and `alexandria dream` both come through here, so there is no second
    /// implementation of "run a job now" to keep in step with the first.
    pub(crate) async fn run_job(&self, job: Job, run_id: &str) -> anyhow::Result<JobReport> {
        match job {
            Job::Sweep => self.run_sweep().await,
            Job::Cluster => self.run_cluster(run_id).await,
            Job::Merge => self.run_merge(run_id).await,
            Job::Collapse => self.run_collapse(run_id).await,
            Job::Appraise => self.run_appraise(run_id).await,
        }
    }

    /// One capped job's pass over every eligible partition, under one pass-wide row allowance.
    ///
    /// The dispatcher is real and the job *bodies* are not yet partitioned (see
    /// [`Self::run_partitioned`]): enumeration, fair planning, budget accounting and progress
    /// persistence are the contracts this method owns, and the witnesses below pin them against
    /// whatever the body currently does.
    ///
    /// The pass shape, in order: enumerate the partitions the store says are eligible (active
    /// accounts only — a suspended account's workspace is absent before any write is attempted);
    /// load the job's persisted progress; rotate the workspaces so the pass continues after the
    /// last one visited and interleave the turn's visits round-robin so one account's many
    /// projects cannot take every slot; reserve the pass allowance **before** dispatch; run the
    /// body once per visited partition, charging `budget_used` and refunding only the verified
    /// unused amount; advance past failures and refusals rather than aborting the pass; and
    /// persist the advanced progress under the M1 CAS.
    pub(crate) async fn dispatch_job(&self, job: Job, run_id: &str) -> anyhow::Result<PassReport> {
        let Some(budget_job) = capped(job) else {
            // Cluster and merge are not row-budgeted; their bodies stay global until M3 gives
            // them partitioned reads, and the old entry keeps working in the meantime.
            let report = self.run_job(job, run_id).await?;
            return Ok(PassReport {
                report,
                visited: 0,
                refused: 0,
                failed: 0,
                budget_used: 0,
                last_error: None,
            });
        };

        let partitions =
            IdentityRepo::maintenance_partitions(self.db.inner(), &self.db, &self.system).await?;
        let mut progress =
            MaintenanceRepo::load_progress(&self.db, &self.system, budget_job).await?;
        // An exhausted cursor only counts against a partition the store still enumerates. A
        // partition whose account suspended (or whose workspace vanished) since it was marked
        // exhausted is stale coordination data: counting it against this pass would underflow
        // `partitions.len() - exhausted.len()` when the enumerated set shrank, and — short of
        // that — would claim visits the pass cannot make. Matching is on the cursor's own
        // workspace/scope pair, never a parse of any display string.
        //
        // Cycle wrap: when every partition the store still enumerates is exhausted, the walk has
        // covered the corpus and the cursors reset so the next pass starts a fresh cycle — heat
        // has decayed and rows have arrived in the meantime, so re-appraisal is the job's
        // purpose, not a spin. A store with no partitions at all has nothing to wrap.
        if !partitions.is_empty()
            && partitions
                .iter()
                .all(|partition| cursor_exhausted(&progress, partition))
        {
            for cursor in &mut progress.cursors {
                cursor.exhausted = false;
                cursor.row_cursor = None;
                cursor.units_done = 0;
            }
        }
        let pending: Vec<Partition> = partitions
            .iter()
            .filter(|partition| !cursor_exhausted(&progress, partition))
            .cloned()
            .collect();
        let pending_count = pending.len();

        // Fair rotation and bounded shares, as pure decisions: rotate the *workspace* order after
        // the last pass's anchor, then take one partition per workspace per round up to the turn's
        // slot count. The slot count is the pass's whole allowance: a capped job visits at most as
        // many partitions as it could afford rows for, and the budget below — not this count — is
        // what stops dispatch when the allowance runs out.
        let mut workspace_order: Vec<alexandria_storage::models::identity::WorkspaceId> =
            pending.iter().map(|p| p.workspace.clone()).collect();
        workspace_order.dedup();
        let rotated = rotate_after(workspace_order, progress.last_workspace.as_ref());
        let rank = |partition: &Partition| {
            rotated
                .iter()
                .position(|workspace| *workspace == partition.workspace)
                .unwrap_or(usize::MAX)
        };
        let mut ordered: Vec<Partition> = pending;
        ordered.sort_by_key(|partition| rank(partition));
        // Every pending partition is visited at most once per pass — the interleave still orders
        // the visits round-robin across workspaces, but the pass is not CUT at a visit count: an
        // empty or failed partition consumes no budget and must not end the pass while rows
        // remain, which a cut at the row cap would do (the first visit can be an empty personal
        // partition, and other accounts' projects must not strand behind it). The ROW budget
        // below is the bound that matters.
        let visits = interleave_rounds(
            ordered,
            |partition: &Partition| rank(partition),
            pending_count,
        );

        let mut budget = RowBudget::new(self.max_rows_per_run);
        let mut report = JobReport::new(job);
        let (mut visited, mut refused, mut failed) = (0usize, 0usize, 0usize);
        let mut last_error: Option<String> = None;
        // Per-partition paging outcomes to persist: the advancing row cursor and the exhaustion
        // verdict, keyed by the partition that produced them.
        let mut paged: Vec<(Partition, Option<RecordId>, bool, usize)> = Vec::new();
        // The rotation anchor is the last partition the pass actually RAN, not the last one it
        // planned: with no visit cut, the plan can name partitions the budget never reached, and
        // anchoring on an unvisited workspace would rotate the next pass back to the one that
        // just ran instead of forward.
        let mut anchor: Option<alexandria_storage::models::identity::WorkspaceId> = None;
        // A bounded per-visit share, not the whole remaining allowance: dividing the pass evenly
        // over the turn's visits is what stops the FIRST partition from eating the entire cap and
        // leaving every other account unvisited — the budget half of the fairness contract, with
        // the interleave above as its slot half. At least one row per visit, rounded up so a cap
        // smaller than the visit count still serves the earlier rotations before it runs out.
        let share = self.max_rows_per_run.div_ceil(visits.len().max(1)).max(1);
        for partition in &visits {
            let Some(reservation) = budget.reserve(share) else {
                // Zero remaining allowance means no further capped partition dispatch — the whole
                // pass's reserve was consumed by earlier partitions' verified work.
                break;
            };
            let allowance = Some(reservation.rows());
            // The partition's persisted row cursor, if any: where the last pass's page stopped, so
            // this pass resumes after it instead of re-reading the same oldest rows.
            let row_cursor = progress
                .cursors
                .iter()
                .find(|cursor| {
                    cursor.workspace == partition.workspace && cursor.scope == partition.scope
                })
                .and_then(|cursor| cursor.row_cursor.clone());
            match self
                .run_partitioned(job, partition, run_id, allowance, row_cursor.as_ref())
                .await
            {
                Ok(outcome) => {
                    let used = outcome.report.budget_used;
                    // Refund only the verified unused reservation: an outcome that reports more
                    // usage than it reserved is refused by the budget rather than clamped.
                    if let Err(error) = budget.refund_verified(reservation, used) {
                        anyhow::bail!("budget accounting refused a partition report: {error}");
                    }
                    report.examined += outcome.report.report.examined;
                    report.acted += outcome.report.report.acted;
                    report.skipped += outcome.report.report.skipped;
                    visited += 1;
                    anchor = Some(partition.workspace.clone());
                    paged.push((
                        partition.clone(),
                        outcome.row_cursor,
                        outcome.exhausted,
                        used,
                    ));
                }
                // A refused writer (the M1 barrier found the partition's account no longer
                // active) advances the pass rather than aborting it: the partition is simply done
                // for this pass, and the progress below records that it was passed over.
                Err(error)
                    if matches!(
                        error.downcast_ref::<alexandria_storage::error::StorageError>(),
                        Some(alexandria_storage::error::StorageError::Stale)
                    ) =>
                {
                    refused += 1;
                }
                // A failed partition is that partition's news: recorded, not propagated, because
                // the rest of the pass is still owed its turn. The charge stands — work of an
                // unknown amount may have landed before the failure, so no refund is attempted.
                Err(error) => {
                    tracing::warn!(
                        job = job.as_str(),
                        "partition dispatch failed, advancing: {error}"
                    );
                    failed += 1;
                    last_error = Some(error.to_string());
                }
            }
        }

        // Persist the advanced rotation anchor and the visited partitions' paging outcomes. The
        // row cursors land here because the dispatcher — not the body — owns the pass ledger:
        // the body reports where its page stopped, the ledger decides what the next pass resumes
        // after. A refused or failed visit leaves the loaded cursor untouched (position unknown,
        // the safe direction is to re-examine), and its charge stands.
        let mut next = progress.clone();
        next.version =
            alexandria_storage::models::identity::Version::new(progress.version.get() + 1);
        next.last_workspace = anchor.or(progress.last_workspace.clone());
        for partition in &visits {
            let cursor = next.cursors.iter_mut().find(|cursor| {
                cursor.workspace == partition.workspace && cursor.scope == partition.scope
            });
            if cursor.is_none() {
                next.cursors
                    .push(alexandria_storage::models::maintenance::WorkspaceProgress {
                        workspace: partition.workspace.clone(),
                        scope: partition.scope.clone(),
                        row_cursor: None,
                        units_done: 0,
                        exhausted: false,
                    });
            }
        }
        for (partition, row_cursor, exhausted, used) in &paged {
            let Some(cursor) = next.cursors.iter_mut().find(|cursor| {
                cursor.workspace == partition.workspace && cursor.scope == partition.scope
            }) else {
                continue;
            };
            cursor.row_cursor = row_cursor.clone();
            cursor.exhausted = *exhausted;
            cursor.units_done += *used as i64;
        }
        if let Err(error) =
            MaintenanceRepo::save_progress(&self.db, &self.system, progress.version, &next).await
        {
            // Progress is coordination metadata, never the pass's result: a lost CAS race means
            // another writer advanced the ledger, and the work this pass did stands.
            tracing::warn!("maintenance progress save lost its race: {error}");
        }

        Ok(PassReport {
            report,
            visited,
            refused,
            failed,
            // The reservation arithmetic nets to the actual chargeable units when bodies
            // succeed (each Ok refunds its unused share), and conservatively keeps a failed
            // visit's whole share — work of an unknown amount may have landed.
            budget_used: self.max_rows_per_run - budget.remaining(),
            last_error,
        })
    }

    /// One job over one partition, under the slice of a pass's row allowance.
    ///
    /// SCAFFOLD: the bodies are not partitioned yet. This runs the job's existing whole-store body
    /// and ignores `partition` and `row_allowance` — the honest intermediate state, because a fake
    /// partition filter here would make the witnesses below pass for the wrong reason. M3 replaces
    /// the body: per-partition reads through scoped repos and writes through the M1 guarded system
    /// barrier, reporting the partition's own `budget_used`.
    ///
    // TODO(debt): partitioned body — M3 of the Task-12 plan.
    async fn run_partitioned(
        &self,
        job: Job,
        partition: &Partition,
        run_id: &str,
        row_allowance: Option<usize>,
        row_cursor: Option<&RecordId>,
    ) -> anyhow::Result<PartitionOutcome> {
        match job {
            // The sweep's partitioned body: read this partition's oldest anchors, project each to
            // now, and materialise them through the M1 guarded system barrier — one transaction,
            // one active-account claim, every statement the same per-row anchor CAS the global
            // body uses. The two heat clocks stay two: the write names `heat` and `last_touched`
            // only.
            Job::Sweep => {
                let limit = row_allowance.unwrap_or(self.max_rows_per_run);
                let heat_repo = HeatRepo::new(self.db.inner());
                let rows = heat_repo.page_oldest_in_partition(partition, limit).await?;

                let mut report = JobReport::new(Job::Sweep);
                report.examined = rows.len();
                if rows.is_empty() {
                    return Ok(PartitionOutcome {
                        report: alexandria_engine::dreaming::PartitionJobReport {
                            report,
                            budget_used: 0,
                        },
                        row_cursor: None,
                        exhausted: false,
                    });
                }

                let now = now_secs();
                let columns = HeatColumns {
                    heat: rows.iter().map(|row| row.heat).collect(),
                    stability: rows.iter().map(|row| row.stability).collect(),
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

                let writes: Vec<alexandria_storage::repos::scope::GuardedWrite> = rows
                    .iter()
                    .zip(projected)
                    .map(|(row, heat)| {
                        alexandria_storage::repos::scope::GuardedWrite::new(
                            "UPDATE heat_state SET heat = $h, last_touched = time::now() \
                             WHERE memory = $m AND last_touched = $t RETURN AFTER;",
                        )
                        .id("m", row.memory.clone())
                        .value("h", alexandria_storage::repos::scope::Bind::Float(heat))
                        .value(
                            "t",
                            alexandria_storage::repos::scope::Bind::OptTime(row.last_touched),
                        )
                    })
                    .collect();
                // The turbofish pins the statement's row type: `RETURN AFTER` on `heat_state`
                // yields `HeatState` rows, and the sweep only counts them — without the pin the
                // count alone leaves the type parameter unconstrained.
                let written = alexandria_storage::repos::scope::guarded_system_writes::<
                    alexandria_storage::models::HeatState,
                >(
                    self.db.inner(), &self.db, &self.system, partition, &writes
                )
                .await?;
                let Some(per_statement) = written else {
                    // The barrier refused the partition: its account stopped being active between
                    // enumeration and the write. The dispatcher owes this a Stale, which it
                    // reports as a refusal rather than a failure.
                    return Err(alexandria_storage::error::StorageError::Stale.into());
                };
                report.acted = per_statement.iter().map(|statement| statement.len()).sum();
                report.skipped = rows.len().saturating_sub(report.acted);
                Ok(PartitionOutcome {
                    report: alexandria_engine::dreaming::PartitionJobReport {
                        report,
                        // Selected page rows are charged, including ones a lost anchor race
                        // skipped.
                        budget_used: rows.len(),
                    },
                    // Sweep self-advances: materialisation moves `last_touched` to now, so the
                    // rows it processed go to the tail of the oldest-first order and the next
                    // page is new rows. No cursor to persist.
                    row_cursor: None,
                    exhausted: false,
                })
            }
            // Cold-row demotion, partition-scoped. Same rule as the whole-store body — heat is
            // *projected* here (no ordering contract with the sweep), the precondition is
            // conjunctive (cold AND never accessed AND confidence at or below the ceiling), rows
            // created before access recording was armed are skipped not demoted, and the value
            // this job writes exempts itself from the rule so a daily pass cannot keep lowering
            // the same row. The demotion and its audit row go through the M1 barrier.
            //
            // Paging: appraise does NOT move `last_touched`, so the page is resumed after the
            // persisted cursor — without it, a page of ineligible rows (the armed-at
            // precondition failing) would be re-selected every pass and later eligible rows would
            // starve. Examined-but-ineligible rows still advance the cursor: they were looked at,
            // and the walk owes the rows behind them.
            Job::Appraise => {
                let limit = row_allowance.unwrap_or(self.max_rows_per_run);
                let Some(armed_at) =
                    system_config::access_recording_armed_at(self.db.inner()).await?
                else {
                    tracing::warn!(
                        "Appraise: access recording has never been armed on this store, so \
                         demoting nothing."
                    );
                    // Not exhausted: the store may arm access recording later, and this partition
                    // is owed a real walk when it does. No budget was consumed either way.
                    return Ok(PartitionOutcome {
                        report: alexandria_engine::dreaming::PartitionJobReport {
                            report: JobReport::new(Job::Appraise),
                            budget_used: 0,
                        },
                        row_cursor: None,
                        exhausted: false,
                    });
                };
                let heat_repo = HeatRepo::new(self.db.inner());
                let memories = MemoryRepo::new(self.db.inner());
                let rows = heat_repo
                    .page_oldest_in_partition_after(partition, limit, row_cursor)
                    .await?;
                let confidences: HashMap<String, LiveConfidence> = memories
                    .live_confidences_in_partition(partition)
                    .await?
                    .into_iter()
                    .map(|row| (record_id_to_string(&row.id), row))
                    .collect();

                let mut report = JobReport::new(Job::Appraise);
                let now = now_secs();
                let audit = MaintenanceRepo::with_audit(
                    self.db.inner(),
                    AuditContext::dreaming(run_id, job_name::APPRAISE),
                );
                let mut demotions: Vec<(RecordId, f64)> = Vec::new();
                for row in &rows {
                    let id = record_id_to_string(&row.memory);
                    let Some(live) = confidences.get(&id) else {
                        report.skipped += 1;
                        continue;
                    };
                    if live.created_at.is_none_or(|at| at < armed_at) {
                        report.skipped += 1;
                        continue;
                    }
                    report.examined += 1;
                    let state = EngineHeatState {
                        heat: row.heat,
                        stability: row.stability,
                        last_touched: row
                            .last_touched
                            .map(|at| at.timestamp().max(0) as u64)
                            .unwrap_or(0),
                        last_accessed_at: row
                            .last_accessed_at
                            .map(|at| at.timestamp().max(0) as u64)
                            .unwrap_or(0),
                        access_count: row.access_count.max(0) as u64,
                    };
                    let projected = projected_heat(&state, now, self.decay_tau_secs);
                    if !should_demote(
                        projected,
                        row.access_count,
                        live.confidence,
                        self.cold_heat_floor,
                        self.demote_confidence_ceiling,
                    ) {
                        continue;
                    }
                    demotions.push((row.memory.clone(), live.confidence));
                }
                if demotions.is_empty() {
                    return Ok(PartitionOutcome {
                        report: alexandria_engine::dreaming::PartitionJobReport {
                            report,
                            budget_used: rows.len(),
                        },
                        row_cursor: Self::page_cursor(&rows),
                        exhausted: rows.len() < limit,
                    });
                }
                // One transaction for every demotion in this partition: one active-account claim,
                // every statement the same single-column update the whole-store body issues.
                let writes: Vec<alexandria_storage::repos::scope::GuardedWrite> = demotions
                    .iter()
                    .map(|(fact, _)| {
                        alexandria_storage::repos::scope::GuardedWrite::new(
                            "UPDATE $f SET confidence = $c WHERE workspace = $workspace AND \
                             deleted = false AND quarantined_at = NONE RETURN AFTER;",
                        )
                        .id("f", fact.clone())
                        .value(
                            "c",
                            alexandria_storage::repos::scope::Bind::Float(DEMOTED_CONFIDENCE),
                        )
                    })
                    .collect();
                let written = alexandria_storage::repos::scope::guarded_system_writes::<
                    alexandria_storage::models::Fact,
                >(
                    self.db.inner(), &self.db, &self.system, partition, &writes
                )
                .await?;
                let Some(per_statement) = written else {
                    return Err(alexandria_storage::error::StorageError::Stale.into());
                };
                for (index, (fact, previous)) in demotions.iter().enumerate() {
                    // A row that acted but failed to log is worse than the reverse: the log write
                    // is outside the barrier transaction and its failure is warned, not unwound —
                    // the same discipline the whole-store body keeps.
                    if per_statement[index].is_empty() {
                        continue;
                    }
                    if let Err(e) = audit
                        .record(&LogEntry {
                            action: action::DEMOTE.to_string(),
                            source_id: record_id_to_string(fact),
                            target_ids: Vec::new(),
                            members_moved: 0,
                            disposition: Some(disposition::DEMOTE.to_string()),
                            previous_value: Some(*previous),
                        })
                        .await
                    {
                        tracing::warn!(
                            "Appraise: cannot log the demotion of {}: {e}",
                            record_id_to_string(fact)
                        );
                    }
                    report.acted += 1;
                }
                Ok(PartitionOutcome {
                    report: alexandria_engine::dreaming::PartitionJobReport {
                        report,
                        budget_used: rows.len(),
                    },
                    row_cursor: Self::page_cursor(&rows),
                    exhausted: rows.len() < limit,
                })
            }
            // Duplicate retirement, partition-scoped. The grouping sees only this partition's
            // live rows, so a byte-identical row in another account's corpus is never a collapse
            // partner. The `derived_from` edge is written BEFORE the soft-delete inside ONE
            // barrier transaction per retirement — reversed, a failure between the two would
            // leave a dead row with no path back to its survivor. Budget counts retirements.
            Job::Collapse => {
                let limit = row_allowance.unwrap_or(self.max_rows_per_run);
                let memories = MemoryRepo::new(self.db.inner());
                let rows = memories.collapse_candidates_in_partition(partition).await?;
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

                let mut report = JobReport::new(Job::Collapse);
                report.examined = rows.len();
                let audit = MaintenanceRepo::with_audit(
                    self.db.inner(),
                    AuditContext::dreaming(run_id, job_name::COLLAPSE),
                );
                // The engine groups by display string; the typed ids the scoped read returned are
                // the authority, so the strings are only keys back into this map. No id is ever
                // *parsed* from text - that is the type-erasing trap the reminder work removed.
                let typed: HashMap<String, RecordId> = rows
                    .iter()
                    .map(|row| (record_id_to_string(&row.id), row.id.clone()))
                    .collect();
                let mut retired: Vec<(RecordId, RecordId)> = Vec::new();
                'groups: for group in duplicate_groups(&candidates) {
                    for collapsed in &group.collapsed {
                        if retired.len() >= limit {
                            break 'groups;
                        }
                        let Some(from) = typed.get(collapsed) else {
                            continue;
                        };
                        let Some(to) = typed.get(&group.survivor) else {
                            continue;
                        };
                        let (from, to) = (from.clone(), to.clone());
                        // Edge first, delete second, one claim for both.
                        let writes = vec![
                            alexandria_storage::repos::scope::GuardedWrite::new(
                                "RELATE $from->memory_edge->$to SET edge_type = $edge_type, \
                                 strength = $strength;",
                            )
                            .id("from", from.clone())
                            .id("to", to.clone())
                            .value(
                                "edge_type",
                                alexandria_storage::repos::scope::Bind::Text(
                                    "derived_from".to_string(),
                                ),
                            )
                            .value(
                                "strength",
                                alexandria_storage::repos::scope::Bind::Float(1.0),
                            ),
                            alexandria_storage::repos::scope::GuardedWrite::new(
                                "UPDATE $f SET deleted = true WHERE workspace = $workspace AND \
                                 deleted = false AND quarantined_at = NONE RETURN AFTER;",
                            )
                            .id("f", from.clone()),
                        ];
                        // The rows decode as JSON values because the two statements return
                        // different row shapes: the RELATE yields `memory_edge` records and the
                        // UPDATE yields `fact` rows, and one type parameter cannot name both.
                        // Landing is judged by the UPDATE's rows (index 1) alone — the delete is
                        // the retirement; the edge's own result shape is not a contract.
                        let written = alexandria_storage::repos::scope::guarded_system_writes::<
                            serde_json::Value,
                        >(
                            self.db.inner(), &self.db, &self.system, partition, &writes
                        )
                        .await?;
                        let Some(per_statement) = written else {
                            return Err(alexandria_storage::error::StorageError::Stale.into());
                        };
                        if per_statement.get(1).is_none_or(|rows| rows.is_empty()) {
                            // The retirement did not land (a raced row state, most likely):
                            // counted as skipped, the next pass re-groups whatever is left.
                            report.skipped += 1;
                            continue;
                        }
                        retired.push((from, to));
                    }
                }
                for (collapsed, survivor) in &retired {
                    if let Err(e) = audit
                        .record(&LogEntry {
                            action: action::COLLAPSE.to_string(),
                            source_id: record_id_to_string(collapsed),
                            target_ids: vec![record_id_to_string(survivor)],
                            members_moved: 0,
                            disposition: Some(disposition::SOFT_DELETE.to_string()),
                            previous_value: None,
                        })
                        .await
                    {
                        tracing::warn!(
                            "Collapse: cannot log the collapse of {}: {e}",
                            record_id_to_string(collapsed)
                        );
                    }
                    report.acted += 1;
                }
                Ok(PartitionOutcome {
                    report: alexandria_engine::dreaming::PartitionJobReport {
                        report,
                        // Collapse charges successful retirements, not candidate reads.
                        budget_used: retired.len(),
                    },
                    // Collapse self-advances: retired rows are soft-deleted and leave the live
                    // candidate set, so the next pass groups only what is left.
                    row_cursor: None,
                    exhausted: false,
                })
            }
            // SCAFFOLD, still: cluster and merge (the uncapped pair) keep their whole-store bodies
            // until their partition-scoped cluster reads land (M3c); they take no row allowance.
            //
            // TODO(debt): partitioned cluster/merge bodies — M3c.
            Job::Cluster | Job::Merge => {
                let report = self.run_job(job, run_id).await?;
                Ok(PartitionOutcome {
                    report: alexandria_engine::dreaming::PartitionJobReport {
                        report,
                        budget_used: 0,
                    },
                    row_cursor: None,
                    exhausted: false,
                })
            }
        }
    }

    /// The paging cursor for an appraise page: the last row the body examined, in the page's own
    /// `(last_touched, id)` order. An empty page keeps `None` — nothing was examined, so there is
    /// nothing to resume after.
    fn page_cursor(rows: &[alexandria_storage::models::HeatState]) -> Option<RecordId> {
        rows.last().and_then(|row| row.id.clone())
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
        let writes: Vec<HeatMaterialise> = rows
            .iter()
            .zip(projected)
            .map(|(row, heat)| HeatMaterialise {
                memory_id: record_id_to_string(&row.memory),
                heat,
                expected_anchor: row.last_touched,
            })
            .collect();
        report.acted = heat_repo.materialize_heat_many(&writes).await?;
        // Rows whose anchor moved between the page read and the write were accessed mid-flight: the
        // access wins and the sweep simply did not get to them this tick. Counted, not swallowed —
        // a page where every claim is lost is a signal, and `acted: 0` alone cannot say so.
        report.skipped = rows.len().saturating_sub(report.acted);
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
        // The engine's verdicts are labelled by display string; the typed ids the listing
        // returned are the authority, so the strings are only keys back into this map. No id is
        // ever parsed from text.
        let typed: HashMap<String, RecordId> = clusters
            .iter()
            .filter_map(|cluster| {
                cluster
                    .id
                    .as_ref()
                    .map(|id| (record_id_to_string(id), id.clone()))
            })
            .collect();

        for cluster in &clusters {
            let cid = cluster
                .id
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default();
            let Some(cluster_id) = typed.get(&cid) else {
                continue;
            };
            let members = match cluster_repo.get_members(cluster_id).await {
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
                let source = typed
                    .get(&cluster_id)
                    .expect("the split verdict names a listed cluster");
                match cluster_repo
                    .execute_split(
                        source,
                        &members,
                        &group_a,
                        &group_b,
                        &centroid_a,
                        &centroid_b,
                    )
                    .await
                {
                    Ok((cid_a, cid_b)) => {
                        tracing::info!(
                            "Split complete: {cluster_id} -> {}, {}",
                            record_id_to_string(&cid_a),
                            record_id_to_string(&cid_b)
                        );
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
            // Typed ids from the listing are the authority; the engine's verdict labels are
            // display strings keyed back into this map, never parsed.
            let typed: HashMap<String, RecordId> = merge_clusters
                .iter()
                .filter_map(|c| {
                    c.id.as_ref()
                        .map(|id| (record_id_to_string(id), id.clone()))
                })
                .collect();

            let mut infos: Vec<(String, Vec<f32>, usize)> = Vec::new();
            for c in &merge_clusters {
                let id = c.id.as_ref().map(record_id_to_string).unwrap_or_default();
                let count = match typed.get(&id) {
                    Some(cluster_id) => cluster_repo
                        .get_members(cluster_id)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(0),
                    None => 0,
                };
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
                        let (Some(keep), Some(remove)) =
                            (typed.get(&keep_id), typed.get(&remove_id))
                        else {
                            continue;
                        };
                        match cluster_repo
                            .execute_merge(keep, remove, &merged_centroid)
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
                        // A collapse overwrites no scalar: the survivor keeps its own confidence and
                        // the loser stays recoverable through its `derived_from` edge.
                        previous_value: None,
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

    /// Cold-row demotion: lower the confidence of memories that are cold, have never been retrieved,
    /// and were never asserted more strongly than the default.
    ///
    /// Demote is the first rung of the ladder and the only one this job can reach. It changes a number
    /// the ranking already reads, so a demoted memory sinks instead of disappearing, and being wrong
    /// costs a confidence value rather than a memory. Quarantine (#29) and delete stay out of scope:
    /// a background job that removes memories on a threshold nobody measured is how a corpus loses
    /// data quietly.
    ///
    /// The near-identical proposal queue from the original design is deliberately absent. Its only
    /// consumer was #38's judge, and a queue nobody reads is a table of stale assertions.
    ///
    /// Heat is **projected** here rather than read as stored, so the job does not depend on the sweep
    /// having run first: two jobs with independent clocks must not have a hidden ordering contract.
    async fn run_appraise(&self, run_id: &str) -> anyhow::Result<JobReport> {
        let heat_repo = HeatRepo::new(self.db.inner());
        let memories = MemoryRepo::new(self.db.inner());
        let audit = MaintenanceRepo::with_audit(
            self.db.inner(),
            AuditContext::dreaming(run_id, job_name::APPRAISE),
        );

        // The precondition of the whole rule. `access_count` was not written by anything before this
        // release, so on a store that has never been armed every row would satisfy "never retrieved"
        // and the first pass would walk the corpus down to `DEMOTED_CONFIDENCE`. No stamp, no
        // demotions — the fail-closed reading of a missing key, the same posture as refusing to start
        // on a zero interval rather than hot-looping.
        let Some(armed_at) = system_config::access_recording_armed_at(self.db.inner()).await?
        else {
            tracing::warn!(
                "Appraise: access recording has never been armed on this store, so demoting nothing. \
                 Boot arms it via `system_config::arm_access_recording`."
            );
            return Ok(JobReport::new(Job::Appraise));
        };

        // Coldest anchors first — the same paging read the sweep uses, so this job examines the rows
        // most likely to be cold rather than an arbitrary page of the table.
        let rows = heat_repo.page_oldest(self.max_rows_per_run).await?;
        let confidences: HashMap<String, LiveConfidence> = memories
            .live_confidences()
            .await?
            .into_iter()
            .map(|row| (record_id_to_string(&row.id), row))
            .collect();

        let mut report = JobReport::new(Job::Appraise);
        let now = now_secs();

        for row in &rows {
            let id = record_id_to_string(&row.memory);
            // No live fact behind this heat row: the memory was deleted, is quarantined, or the row
            // is orphaned. Nothing to demote, and counted as skipped rather than examined so a
            // report of "acted: 0" is not read as "the corpus is healthy".
            let Some(live) = confidences.get(&id) else {
                report.skipped += 1;
                continue;
            };
            // `access_count == 0` only means "never retrieved" for a memory that was stored after the
            // retrieve path began recording accesses. For an older one the count is a non-event: nobody
            // was writing it, so cold-and-never-retrieved is indistinguishable from never-observed, and
            // every legacy row would satisfy the rule on the first pass. Left alone, permanently.
            if live.created_at.is_none_or(|at| at < armed_at) {
                report.skipped += 1;
                continue;
            }
            report.examined += 1;

            let state = EngineHeatState {
                heat: row.heat,
                stability: row.stability,
                last_touched: row
                    .last_touched
                    .map(|at| at.timestamp().max(0) as u64)
                    .unwrap_or(0),
                // Projection reads the decay anchor only, so the access stamp cannot change the
                // answer. Carried through rather than zeroed so the state is a faithful copy.
                last_accessed_at: row
                    .last_accessed_at
                    .map(|at| at.timestamp().max(0) as u64)
                    .unwrap_or(0),
                access_count: row.access_count.max(0) as u64,
            };
            let projected = projected_heat(&state, now, self.decay_tau_secs);
            if !should_demote(
                projected,
                row.access_count,
                live.confidence,
                self.cold_heat_floor,
                self.demote_confidence_ceiling,
            ) {
                continue;
            }

            // `update_fact` with only `confidence` set writes that one column, so a demotion cannot
            // disturb content, tags or the embedding.
            if let Err(e) = memories
                .update_fact(&id, None, None, Some(DEMOTED_CONFIDENCE), None)
                .await
            {
                tracing::warn!("Appraise: cannot demote {id}: {e}");
                continue;
            }
            if let Err(e) = audit
                .record(&LogEntry {
                    action: action::DEMOTE.to_string(),
                    source_id: id.clone(),
                    // A demotion moves nothing, so there is no target.
                    target_ids: Vec::new(),
                    members_moved: 0,
                    disposition: Some(disposition::DEMOTE.to_string()),
                    // The whole point of writing it: without the prior value, `run_id` names a pass
                    // but does not make it reversible.
                    previous_value: Some(live.confidence),
                })
                .await
            {
                tracing::warn!("Appraise: cannot log the demotion of {id}: {e}");
            }
            report.acted += 1;
        }
        Ok(report)
    }

    async fn all_clusters(&self) -> anyhow::Result<Vec<alexandria_storage::models::Cluster>> {
        ClusterRepo::new(self.db.inner())
            .legacy_maintenance_clusters()
            .await
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

/// Whether one partition's persisted cursor marks it exhausted. A free function rather than a
/// closure so the pass can mutate the progress it reads between calls — the cycle wrap resets the
/// flags this predicate tests.
fn cursor_exhausted(
    progress: &alexandria_storage::models::maintenance::MaintenanceProgress,
    partition: &Partition,
) -> bool {
    progress.cursors.iter().any(|cursor| {
        cursor.exhausted
            && cursor.workspace == partition.workspace
            && cursor.scope == partition.scope
    })
}

/// The capped jobs, in the engine's budget vocabulary. `None` for cluster and merge, which are not
/// row-budgeted and keep their existing whole-store behaviour until M3.
fn capped(job: Job) -> Option<MaintenanceBudgetJob> {
    match job {
        Job::Sweep => Some(MaintenanceBudgetJob::Sweep),
        Job::Collapse => Some(MaintenanceBudgetJob::Collapse),
        Job::Appraise => Some(MaintenanceBudgetJob::Appraise),
        Job::Cluster | Job::Merge => None,
    }
}

/// One partition's dispatch outcome: the engine's chargeable report plus the paging facts the
/// dispatcher persists. The row cursor and exhaustion verdict are appraise's alone — sweep
/// self-advances (materialisation moves `last_touched` to now, sending processed rows to the
/// tail of the oldest-first order) and collapse self-advances (retired rows leave the live
/// candidate set) — but the shape is shared so the dispatcher persists one kind of outcome.
#[derive(Debug)]
pub(crate) struct PartitionOutcome {
    pub report: alexandria_engine::dreaming::PartitionJobReport,
    /// The last row the body examined, so the next pass resumes after it. `None` leaves the
    /// persisted cursor where it was.
    pub row_cursor: Option<RecordId>,
    /// The body's page came back short of its share, so the partition is fully processed this
    /// cycle and later passes skip it until the cycle wraps.
    pub exhausted: bool,
}

/// One capped job's whole pass, as the scheduler and CLI report it.
#[derive(Debug)]
pub(crate) struct PassReport {
    pub report: JobReport,
    /// Partitions whose body ran to completion this pass.
    pub visited: usize,
    /// Partitions the M1 barrier refused (the account stopped being active mid-pass).
    pub refused: usize,
    /// Partitions whose body failed; the pass advanced past them.
    pub failed: usize,
    /// Chargeable units the pass consumed against its allowance.
    pub budget_used: usize,
    /// The last per-partition failure, carried for the operator and the witnesses: a pass that
    /// advances past failures is silent by design, and "failed: 1" without the error is a
    /// diagnostic dead end.
    pub last_error: Option<String>,
}

/// The loop. Sleeps until the soonest job is due rather than waking on a fixed tick, runs the whole
/// due set in declared order, and marks each job as run whether it succeeded or not.
///
/// Marking on failure matters: a job that errors every run would otherwise be re-queued immediately,
/// and the loop would spin on it instead of sleeping until its interval. The cost is that a
/// transient failure waits a full interval for its retry — acceptable for housekeeping, and the
/// alternative (retry immediately) is what turns one poisoned row into a hot loop.
pub(crate) async fn run(jobs: Arc<Jobs>, cancel: CancellationToken) {
    let mut schedule = jobs.intervals.initial_schedule(now_secs());
    let mut tick: u64 = 0;
    // Claim the loop's existence before the first sleep. Without this a loop that starts, sleeps for
    // an hour (sweep's cadence) and is read during that sleep would report `NotStarted`, which is a
    // different statement than the truth.
    jobs.liveness.note_start(now_secs());
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
        // Stamp **every** wake, including one where nothing was due. `due.is_empty()` continues
        // below, and a heartbeat written only after a real tick would go quiet for exactly the
        // periods where the scheduler is healthy but idle — which is the failure this field exists
        // to distinguish from a dead loop.
        jobs.liveness.note_tick(now);
        let due = schedule.due(now);
        if due.is_empty() {
            continue;
        }
        tick += 1;
        // One id for the whole tick, shared by every job that runs in it. `run_id` is the unit an
        // operator would reverse, and a tick that split a cluster and then collapsed duplicates into
        // it is one event, not two.
        let run_id = tick_run_id(now, tick);
        for job in due {
            // The capped jobs run as partitioned passes under the dispatcher (enumeration, fair
            // shares, pass-wide budget, persisted rotation); the uncapped cluster/merge keep their
            // whole-store bodies until their partitioned reads land. This is the loop half of the
            // Task-12 wiring; `alexandria dream` joins it when its CLI witness turns green.
            let ran = if capped(job).is_some() {
                jobs.dispatch_job(job, &run_id).await.map(|pass| {
                    // The pass's own observability, at the same debug level a single
                    // JobReport gets: which partitions ran, which were refused by the
                    // barrier or failed and advanced, and what the pass charged against
                    // its global allowance.
                    tracing::debug!(
                        job = pass.report.job.as_str(),
                        visited = pass.visited,
                        refused = pass.refused,
                        failed = pass.failed,
                        budget_used = pass.budget_used,
                        last_error = ?pass.last_error,
                        "dreaming pass finished"
                    );
                    pass.report
                })
            } else {
                jobs.run_job(job, &run_id).await
            };
            match ran {
                Ok(report) => tracing::debug!(
                    job = report.job.as_str(),
                    examined = report.examined,
                    acted = report.acted,
                    "dreaming job finished"
                ),
                Err(e) => tracing::warn!(job = job.as_str(), "dreaming job failed: {e}"),
            }
            // Stamp with a clock read *after* the job, not the tick's start. `is_due` compares against
            // this value, so a stamp taken before a job that outlived its own interval leaves it due
            // again the moment the loop recomputes — `next_wait_secs` returns 0, `sleep(0)` returns
            // immediately, and the job runs back-to-back with no rest for as long as the overrun
            // lasts. Every job in the due set shared that one stale stamp, so one slow job also
            // cancelled the wait of every shorter interval in the tick: precisely the hot loop the
            // loader refuses to start on for a zero interval.
            schedule.mark_run(job, now_secs());
        }
    }
    tracing::debug!("dreaming scheduler stopped");
}

#[cfg(test)]
#[path = "dreaming/legacy_gate_tests.rs"]
mod legacy_gate_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use alexandria_engine::dreaming::SchedulerState;
    use alexandria_storage::Database;

    /// An embedded database with the schema applied and nothing in it: the state every first boot
    /// starts from, and the one the jobs must treat as success rather than as an error.
    async fn migrated_db() -> (Arc<Database>, SystemContext) {
        let mut db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .expect("schema applies to a fresh database");
        let system = db
            .claim_system_authority()
            .expect("a fresh embedded store has an unclaimed system authority");
        (Arc::new(db), system)
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
        let (db, system) = migrated_db().await;
        let id = seed_cold(&db, "a memory nobody has looked at in years").await;

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
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
        let (db, system) = migrated_db().await;
        for i in 0..3 {
            seed_cold(&db, &format!("memory {i}")).await;
        }

        let mut config = Config::default();
        config.dreaming.max_rows_per_run = 2;
        let jobs = Jobs::new(db.clone(), system.clone(), &config);

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
        let (db, system) = migrated_db().await;
        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Sweep, "run-test")
            .await
            .expect("no rows is not an error");
        assert_eq!(
            (report.job, report.examined, report.acted),
            (Job::Sweep, 0, 0)
        );
    }

    /// Split and merge are the most destructive things any job does, and this branch gave them a run
    /// id so an operator could answer "why did my corpus change" — advertised in AGENTS.md and
    /// README. Nothing tested it: `ClusterRepo::with_audit` was constructed only by `run_cluster` and
    /// `run_merge`, while `cluster_maintenance_test.rs` still drives split/merge through the
    /// unattributed `ClusterRepo::new`. A typo in the new `run_id`/`job`/`actor` binds would have
    /// produced unattributed rows with only a warn to show for it — which is precisely the silent
    /// breakage the audit trail exists to catch.
    /// The heartbeat is only useful if the loop actually writes it, and it must be written on a wake
    /// where nothing was due — otherwise the field goes quiet precisely during the idle periods that
    /// are normal for a 1-hour sweep cadence, and "quiet" stops meaning anything.
    #[tokio::test]
    async fn the_loop_stamps_the_heartbeat_even_when_nothing_is_due() {
        let (db, system) = migrated_db().await;
        let jobs = Jobs::new(db, system.clone(), &Config::default());
        let liveness = Arc::new(Liveness::new(600));
        // Same field `spawn` sets, and the same reason: a dashboard reading a different `Liveness`
        // than the loop writes would report `NotStarted` forever.
        let jobs = Jobs {
            liveness: Arc::clone(&liveness),
            ..jobs
        };

        assert_eq!(liveness.state(now_secs()), SchedulerState::NotStarted);

        let cancel = CancellationToken::new();
        let loop_handle = tokio::spawn(run(Arc::new(jobs), cancel.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(10), loop_handle)
            .await
            .expect("cancelled loop must end")
            .expect("must not panic on the way out");

        assert!(
            !matches!(liveness.state(now_secs()), SchedulerState::NotStarted),
            "entering the loop must be observable, not just the completion of a tick"
        );
        if let SchedulerState::GivenUp { .. } = liveness.state(now_secs()) {
            panic!("a clean cancelled run must not report itself as stopped");
        }
    }

    #[tokio::test]
    async fn a_split_attributes_its_audit_row_to_the_tick_that_wrote_it() {
        let (db, system) = migrated_db().await;
        let memories = MemoryRepo::new(db.inner());
        let clusters = ClusterRepo::new(db.inner());

        // Cohesion is average cosine similarity to the centroid, and the floor is 0.6. With this
        // centroid the two members pointing along it score 1.0 and the two orthogonal to it score
        // 0.0, so the average is 0.5 — under the floor, and a real split rather than an empty tick.
        let wide = clusters
            .create(Some("wide"), &[1.0_f32, 0.0])
            .await
            .unwrap();
        for (i, point) in [[1.0_f32, 0.0], [1.0, 0.0], [0.0, 1.0], [0.0, 1.0]]
            .iter()
            .enumerate()
        {
            let (_, fid) = memories
                .create_fact_with_id(&format!("member {i}"), 0.5, point, &[])
                .await
                .unwrap();
            clusters.add_member(&wide, &fid).await.unwrap();
        }

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Cluster, "run-split-attrib")
            .await
            .expect("cluster job succeeds");
        assert_eq!(
            report.acted, 1,
            "the fixture must actually provoke a split, or the assertions below measure nothing"
        );

        let logs = ClusterRepo::new(db.inner())
            .list_maintenance_logs(10, 0, None)
            .await
            .unwrap();
        let split = logs
            .iter()
            .find(|row| row.action == "split")
            .expect("a split was logged");
        assert_eq!(
            split.run_id.as_deref(),
            Some("run-split-attrib"),
            "the destructive write carries the tick that made it"
        );
        assert_eq!(split.job.as_deref(), Some("cluster"));
        assert_eq!(split.actor.as_deref(), Some("system:dreaming"));
        assert!(split.members_moved > 0, "and says how many members moved");
    }

    /// Merge gets its own database: the split children left behind by the test above have centroids
    /// of their own, and a shared fixture would make "exactly one merge" depend on which pair the
    /// scan happened to reach first.
    #[tokio::test]
    async fn a_merge_attributes_its_audit_row_to_the_tick_that_wrote_it() {
        let (db, system) = migrated_db().await;
        let memories = MemoryRepo::new(db.inner());
        let clusters = ClusterRepo::new(db.inner());

        // Centroid similarity well over the 0.75 join threshold, so these two merge.
        let a = clusters.create(Some("a"), &[1.0_f32, 0.0]).await.unwrap();
        let b = clusters.create(Some("b"), &[0.99_f32, 0.01]).await.unwrap();
        let (_, fid) = memories
            .create_fact_with_id("shared", 0.5, &[0.99_f32, 0.01], &[])
            .await
            .unwrap();
        clusters.add_member(&b, &fid).await.unwrap();

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Merge, "run-merge-attrib")
            .await
            .expect("merge job succeeds");
        assert_eq!(
            report.acted, 1,
            "two near-identical centroids must merge, or the assertion measures an empty tick"
        );

        let logs = ClusterRepo::new(db.inner())
            .list_maintenance_logs(10, 0, None)
            .await
            .unwrap();
        let merge = logs
            .iter()
            .find(|row| row.action == "merge")
            .expect("a merge was logged");
        assert_eq!(merge.run_id.as_deref(), Some("run-merge-attrib"));
        assert_eq!(merge.job.as_deref(), Some("merge"));
        assert_eq!(merge.actor.as_deref(), Some("system:dreaming"));
        // And the direction is the documented one: the kept cluster is the one with more members,
        // so `b` (one member) survives and `a` (none) is the source. Asserted rather than incidental,
        // because a merge that deleted the populated cluster would still log an attributed row.
        // `create` returns the typed id now; the audit row carries the canonical display form.
        assert_eq!(
            merge.source_id,
            record_id_to_string(&a),
            "the empty cluster is the one removed"
        );
        assert_eq!(
            merge.target_ids,
            vec![record_id_to_string(&b)],
            "the cluster holding the member is the one kept"
        );
    }

    /// REGRESSION WITNESS (run 50): the merge drain over clusters whose keys need quoting
    /// (`cluster:`quoted key``) or are numeric (`cluster:7`) must terminate and land its audit rows.
    ///
    /// Before the typed-id boundary, `ClusterRepo::delete` bound the cluster's *display string* into
    /// `type::record($id)`, which matches nothing for a quoted key and binds a string key for a numeric
    /// one — so `execute_merge` moved the members, "succeeded", and deleted nothing. The drain loop
    /// re-read the cluster set, found the same near-identical pair again, and merged forever: the job
    /// hung with an empty audit trail. The timeout below IS the assertion — the pre-fix code fails it
    /// by hanging, not by returning.
    ///
    /// Determinism: `check_merge` keeps the cluster with more members, so the numeric-keyed cluster
    /// (two members) is the survivor and the quoted-keyed one (one member) is the source, whatever
    /// order `all_clusters` returns them in. Both are legacy-gate-admissible by construction (no
    /// owner fields set), which is the population the merge job reads.
    #[tokio::test]
    async fn the_merge_drain_terminates_on_quoted_and_numeric_cluster_keys() {
        let (db, system) = migrated_db().await;
        let memories = MemoryRepo::new(db.inner());
        let clusters = ClusterRepo::new(db.inner());

        let numeric = RecordId::new("cluster", 7_i64);
        let quoted = RecordId::new("cluster", "quoted key with spaces");
        for (id, label) in [(numeric.clone(), "numeric"), (quoted.clone(), "quoted")] {
            db.inner()
                .query("CREATE $id SET label = $label, centroid = $centroid, depth = 0")
                .bind(("id", id.clone()))
                .bind(("label", label.to_string()))
                .bind(("centroid", vec![1.0_f32, 0.0]))
                .await
                .unwrap()
                .check()
                .unwrap();
        }
        // Two members for the numeric cluster, one for the quoted: the keep/remove direction is
        // pinned by member count, not by scan order.
        for (cluster, content) in [
            (numeric.clone(), "drain member one"),
            (numeric.clone(), "drain member two"),
            (quoted.clone(), "drain member three"),
        ] {
            let (_, fact) = memories
                .create_fact_with_id(content, 0.5, &[1.0_f32, 0.0], &[])
                .await
                .unwrap();
            clusters.add_member(&cluster, &fact).await.unwrap();
        }

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
        let report = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            jobs.run_job(Job::Merge, "run-quoted-numeric-drain"),
        )
        .await
        .expect("the merge drain terminated")
        .expect("merge job succeeds");
        assert_eq!(
            report.acted, 1,
            "the quoted/numeric pair must merge, or the witness measures an empty tick"
        );

        // The drain finished AND finished its work: the quoted-keyed cluster is gone, the
        // numeric-keyed survivor holds every member, and the audit row names both keys in their
        // canonical display forms.
        assert!(
            clusters.get(&quoted).await.unwrap().is_none(),
            "the quoted-keyed cluster survived its own merge"
        );
        assert_eq!(
            clusters.get_members(&numeric).await.unwrap().len(),
            3,
            "the surviving numeric-keyed cluster did not inherit every member"
        );
        let logs = ClusterRepo::new(db.inner())
            .list_maintenance_logs(10, 0, None)
            .await
            .unwrap();
        let merge = logs
            .iter()
            .find(|row| row.action == "merge")
            .expect("a merge was logged for the quoted/numeric pair");
        assert_eq!(merge.run_id.as_deref(), Some("run-quoted-numeric-drain"));
        assert_eq!(
            merge.source_id,
            record_id_to_string(&quoted),
            "the audit row did not carry the quoted key in canonical display form"
        );
        assert_eq!(
            merge.target_ids,
            vec![record_id_to_string(&numeric)],
            "the audit row did not carry the numeric key in canonical display form"
        );
    }

    #[tokio::test]
    async fn the_cluster_and_merge_jobs_survive_an_empty_database() {
        let (db, system) = migrated_db().await;
        let jobs = Jobs::new(db, system.clone(), &Config::default());

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
    /// The supervisor must restart a panicked loop and must eventually stop trying.
    ///
    /// Both halves matter and neither is reachable through the real `run`: a supervisor that restarts
    /// forever turns one panic into permanent load against the store with a log line every backoff as
    /// its only trace, and one that never restarts is the silent stop this code exists to prevent.
    /// `supervise` is generic over the attempt precisely so the policy can be tested without a job
    /// that panics on purpose.
    #[tokio::test]
    async fn the_supervisor_restarts_a_panicking_loop_and_then_gives_up() {
        let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cancel = CancellationToken::new();
        let counter = Arc::clone(&attempts);
        let liveness = Arc::new(Liveness::new(600));

        supervise(
            move || {
                let counter = Arc::clone(&counter);
                async move {
                    // Never returns normally: every attempt panics, which is the worst case.
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    panic!("attempt panicked on purpose");
                }
            },
            cancel,
            Duration::from_millis(1),
            Arc::clone(&liveness),
        )
        .await;

        let seen = attempts.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            seen, 4,
            "three restarts after the first failure, then it stops trying — not forever"
        );
        // The give-up has to be visible to a reader who never looks at a log file, because the
        // dashboard's cadence row says the same thing either way. `restarts` counts the restarts it
        // did (3), `panics` the failures it saw (4) — the last one is the one it stopped on.
        assert_eq!(
            liveness.state(0),
            SchedulerState::GivenUp {
                panics: 4,
                restarts: 3
            },
            "a scheduler that gave up must report itself as stopped"
        );
        assert!(
            liveness
                .last_panic()
                .is_some_and(|m| m.contains("on purpose")),
            "the payload should survive for display"
        );
    }

    /// A loop that exits cleanly must not be restarted, and neither must one that ends because the
    /// service is shutting down: restarting either would spin the scheduler past the point of its
    /// own cancellation.
    #[tokio::test]
    async fn the_supervisor_does_not_restart_a_clean_exit() {
        let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cancel = CancellationToken::new();
        let counter = Arc::clone(&attempts);

        supervise(
            move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            },
            cancel,
            Duration::from_millis(1),
            // A heartbeat the loop never got to write: a clean exit must not be reported as a
            // stopped scheduler, which is the difference between `NotStarted` and `GivenUp`.
            Arc::new(Liveness::new(600)),
        )
        .await;

        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a clean return is the end of the scheduler, not a failure to retry"
        );
    }

    #[tokio::test]
    async fn the_loop_exits_when_cancelled() {
        let (db, system) = migrated_db().await;
        let jobs = Jobs::new(db, system.clone(), &Config::default());
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(run(Arc::new(jobs), cancel.clone()));
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
        let (db, system) = migrated_db().await;
        let memories = MemoryRepo::new(db.inner());
        let weak = memories
            .create_fact("the token rotates nightly", 0.3, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let strong = memories
            .create_fact("the token rotates nightly", 0.9, &[0.1, 0.2], &[])
            .await
            .unwrap();

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
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
        let (db, system) = migrated_db().await;
        let memories = MemoryRepo::new(db.inner());
        for confidence in [0.4, 0.8] {
            memories
                .create_fact("duplicate content", confidence, &[0.1, 0.2], &[])
                .await
                .unwrap();
        }

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
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
            .list_maintenance_logs(10, 0, None)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "and it wrote no second audit row");
    }

    /// `run_id` is the unit of reversal, so every row one run writes has to carry the same one,
    /// along with the job and the writer. Without that, "undo the pass that ate my memories" is a
    /// timestamp range and a guess.
    #[tokio::test]
    async fn collapse_attributes_every_row_to_the_run_that_wrote_it() {
        let (db, system) = migrated_db().await;
        let memories = MemoryRepo::new(db.inner());
        for _ in 0..3 {
            memories
                .create_fact("three identical rows", 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
        }

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Collapse, "run-77")
            .await
            .expect("collapse succeeds");
        assert_eq!(
            report.acted, 2,
            "two of the three collapse into the survivor"
        );

        let logs = ClusterRepo::new(db.inner())
            .list_maintenance_logs(10, 0, None)
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
        let (db, system) = migrated_db().await;
        let memories = MemoryRepo::new(db.inner());
        for _ in 0..3 {
            memories
                .create_fact("bounded collapse", 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
        }

        let mut config = Config::default();
        config.dreaming.max_rows_per_run = 1;
        let jobs = Jobs::new(db.clone(), system.clone(), &config);

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

    /// Seed one memory plus its heat row. `cold` puts the decay anchor six years back, which at the
    /// default one-day tau projects to essentially zero; `access_count` is the field the demote rule
    /// reads, so it is set directly rather than through a retrieval.
    ///
    /// Does not arm access recording — a fixture that silently satisfies a rule's precondition is the
    /// kind of fixture that stops testing it. Call [`armed_long_ago`] when the row is meant to be
    /// judgeable, and call neither when it is meant to be the fail-closed case.
    async fn seed_for_appraise(
        db: &Database,
        content: &str,
        confidence: f64,
        access_count: i64,
        cold: bool,
    ) -> String {
        let memories = MemoryRepo::new(db.inner());
        let heat = HeatRepo::new(db.inner());
        let id = memories
            .create_fact(content, confidence, &[0.1, 0.2], &[])
            .await
            .unwrap();
        heat.create_for_memory(&id, 1.0).await.unwrap();
        let anchor = if cold {
            "d'2020-01-01T00:00:00Z'"
        } else {
            "time::now()"
        };
        db.inner()
            .query(format!(
                "UPDATE heat_state SET last_touched = {anchor}, last_accessed_at = {anchor}, \
                 access_count = $count WHERE memory = type::record($m)"
            ))
            .bind(("count", access_count))
            .bind(("m", id.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        id
    }

    /// Arm recording at the epoch, so every memory this suite can create was stored after it and the
    /// liveness gate cannot depend on statement ordering inside a test. Written directly rather than
    /// through `arm_access_recording` because the value, not the stamping, is what these tests are
    /// about; `arm_access_recording`'s own behaviour is pinned by the boot path test.
    async fn armed_long_ago(db: &Database) {
        system_config::set_config(
            db.inner(),
            system_config::ACCESS_RECORDING_ARMED_AT,
            "1970-01-01T00:00:00Z",
        )
        .await
        .unwrap();
    }

    /// The first rung of the ladder: confidence lowered, memory still live and retrievable, one
    /// attributed audit row. Nothing is deleted or hidden, so being wrong costs a number.
    #[tokio::test]
    async fn appraise_demotes_a_cold_never_retrieved_memory_and_logs_it() {
        let (db, system) = migrated_db().await;
        armed_long_ago(&db).await;
        let id = seed_for_appraise(&db, "nobody ever asked for this", 0.5, 0, true).await;

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Appraise, "run-demote-1")
            .await
            .expect("appraise succeeds");
        assert_eq!((report.examined, report.acted), (1, 1));

        let fact = MemoryRepo::new(db.inner())
            .get_fact(&id)
            .await
            .unwrap()
            .expect("row still exists");
        assert_eq!(
            fact.confidence, DEMOTED_CONFIDENCE,
            "confidence is the only thing demotion writes"
        );
        assert!(!fact.deleted, "demotion is not deletion");
        assert!(
            fact.quarantined_at.is_none(),
            "and not quarantine either: the row stays retrievable, just ranked lower"
        );

        let logs = ClusterRepo::new(db.inner())
            .list_maintenance_logs(10, 0, None)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].action, "demote");
        assert_eq!(logs[0].source_id, id);
        assert!(logs[0].target_ids.is_empty(), "a demotion moves nothing");
        assert_eq!(logs[0].disposition.as_deref(), Some("demote"));
        assert_eq!(logs[0].job.as_deref(), Some("appraise"));
        assert_eq!(logs[0].run_id.as_deref(), Some("run-demote-1"));
        assert_eq!(logs[0].actor.as_deref(), Some("system:dreaming"));
        assert_eq!(
            logs[0].previous_value,
            Some(0.5),
            "the demotion records what it overwrote, or `run_id` names a pass that cannot be undone"
        );
    }

    /// The precondition that keeps this release from rewriting the corpus it upgrades. Every memory
    /// stored before access recording existed has `access_count == 0` because nothing ever wrote the
    /// field, and the default confidence equals the ceiling, so without this gate the first daily
    /// pass would walk the legacy corpus down to `DEMOTED_CONFIDENCE` at 500 rows a tick — the oldest
    /// anchors first, which is precisely the set most likely to have been used a lot before recording
    /// started. The positive control is the row stored under arming: it must still be demoted, or this
    /// test would pass for a job that does nothing at all.
    #[tokio::test]
    async fn a_memory_stored_before_access_recording_was_armed_is_never_demoted() {
        let (db, system) = migrated_db().await;
        // Armed now, which is what the first boot of this build does.
        system_config::arm_access_recording(db.inner())
            .await
            .unwrap();
        let legacy =
            seed_for_appraise(&db, "stored years before anyone was counting", 0.5, 0, true).await;
        let current = seed_for_appraise(&db, "stored under recording", 0.5, 0, true).await;
        // Put the legacy row's birth back before the stamp; `create_fact` cannot make an old row.
        db.inner()
            .query("UPDATE type::record($id) SET created_at = d'2020-01-01T00:00:00Z'")
            .bind(("id", legacy.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Appraise, "run-arming")
            .await
            .expect("appraise succeeds");
        assert_eq!(
            (report.examined, report.acted, report.skipped),
            (1, 1, 1),
            "the pre-arming row is skipped and said so, not quietly examined and left alone"
        );

        let memories = MemoryRepo::new(db.inner());
        assert_eq!(
            memories
                .get_fact(&legacy)
                .await
                .unwrap()
                .unwrap()
                .confidence,
            0.5,
            "a memory nobody ever counted is untouched"
        );
        assert_eq!(
            memories
                .get_fact(&current)
                .await
                .unwrap()
                .unwrap()
                .confidence,
            DEMOTED_CONFIDENCE,
            "positive control: the same rule still fires on a row it can honestly judge"
        );
    }

    /// Fail closed, not open: with no arming stamp the job has no basis for demoting anything, so it
    /// writes nothing rather than treating every `access_count == 0` as evidence of disuse.
    #[tokio::test]
    async fn appraise_demotes_nothing_until_access_recording_has_been_armed() {
        let (db, system) = migrated_db().await;
        let id = seed_for_appraise(&db, "cold, unused, unarmed", 0.5, 0, true).await;

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Appraise, "run-unarmed")
            .await
            .expect("a missing stamp is not an error");
        assert_eq!((report.examined, report.acted, report.skipped), (0, 0, 0));
        assert_eq!(
            MemoryRepo::new(db.inner())
                .get_fact(&id)
                .await
                .unwrap()
                .unwrap()
                .confidence,
            0.5,
            "nothing was lowered"
        );
        let logs = ClusterRepo::new(db.inner())
            .list_maintenance_logs(10, 0, None)
            .await
            .unwrap();
        assert!(logs.is_empty(), "and nothing was logged");
    }

    /// Every condition in the rule is necessary, so each is relaxed on its own row and none of them
    /// may be demoted. A rule that fired on any of these would be pruning memories that are either
    /// used, current, or explicitly trusted — which is the complaint that would end this feature.
    #[tokio::test]
    async fn appraise_leaves_accessed_warm_and_confident_memories_alone() {
        let (db, system) = migrated_db().await;
        armed_long_ago(&db).await;
        let accessed = seed_for_appraise(&db, "retrieved three times", 0.5, 3, true).await;
        let warm = seed_for_appraise(&db, "touched today", 0.5, 0, false).await;
        let confident = seed_for_appraise(&db, "asserted with confidence", 0.95, 0, true).await;
        let demotable = seed_for_appraise(&db, "cold and unused", 0.5, 0, true).await;

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Appraise, "run-selective")
            .await
            .expect("appraise succeeds");
        assert_eq!(report.examined, 4);
        assert_eq!(
            report.acted, 1,
            "only the cold, unused, default-confidence row"
        );

        let memories = MemoryRepo::new(db.inner());
        for (id, expected, why) in [
            (accessed, 0.5, "an access exempts it"),
            (warm, 0.5, "a warm row is not cold"),
            (confident, 0.95, "above the confidence ceiling"),
            (demotable, DEMOTED_CONFIDENCE, "the one row that matches"),
        ] {
            let fact = memories.get_fact(&id).await.unwrap().unwrap();
            assert_eq!(fact.confidence, expected, "{why}");
        }
    }

    /// The rule exempts the value it writes, so a daily job cannot keep lowering the same row or
    /// keep logging that it did. Both halves matter: the second is what would make the audit trail
    /// unreadable, since every run would report the same demotions forever.
    #[tokio::test]
    async fn a_second_appraise_run_does_not_demote_the_same_row_twice() {
        let (db, system) = migrated_db().await;
        armed_long_ago(&db).await;
        seed_for_appraise(&db, "cold and unused", 0.5, 0, true).await;

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
        let first = jobs
            .run_job(Job::Appraise, "run-1")
            .await
            .expect("appraise succeeds");
        assert_eq!(first.acted, 1);

        let second = jobs
            .run_job(Job::Appraise, "run-2")
            .await
            .expect("appraise succeeds");
        assert_eq!(
            second.acted, 0,
            "already demoted, so the second run leaves it alone"
        );

        let logs = ClusterRepo::new(db.inner())
            .list_maintenance_logs(10, 0, None)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "and wrote no second audit row");
    }

    /// A memory whose heat row exists but whose fact was deleted or quarantined must not be examined
    /// or demoted. `page_oldest` reads `heat_state`, which has no `deleted` column of its own, so
    /// the join against the live-fact read is what keeps dead rows out.
    /// A memory whose heat row exists but whose fact was deleted or quarantined must not be examined
    /// or demoted. `page_oldest` reads `heat_state`, which has no `deleted` column of its own, so
    /// the join against the live-fact read is what keeps dead rows out.
    #[tokio::test]
    async fn appraise_skips_heat_rows_whose_fact_is_not_live() {
        let (db, system) = migrated_db().await;
        armed_long_ago(&db).await;
        let deleted = seed_for_appraise(&db, "already deleted", 0.5, 0, true).await;
        let quarantined = seed_for_appraise(&db, "quarantined", 0.5, 0, true).await;
        let live = seed_for_appraise(&db, "still live", 0.5, 0, true).await;

        MemoryRepo::new(db.inner())
            .soft_delete_fact(&deleted)
            .await
            .unwrap();
        db.inner()
            .query("UPDATE type::record($id) SET quarantined_at = time::now()")
            .bind(("id", quarantined.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();

        let jobs = Jobs::new(db.clone(), system.clone(), &Config::default());
        let report = jobs
            .run_job(Job::Appraise, "run-liveness")
            .await
            .expect("appraise succeeds");
        assert_eq!(
            (report.examined, report.acted),
            (1, 1),
            "only the live fact is examined"
        );

        let memories = MemoryRepo::new(db.inner());
        assert_eq!(
            memories.get_fact(&live).await.unwrap().unwrap().confidence,
            DEMOTED_CONFIDENCE
        );
        assert_eq!(
            memories
                .get_fact(&quarantined)
                .await
                .unwrap()
                .unwrap()
                .confidence,
            0.5,
            "a quarantined row is hidden from retrieval already; demoting it would be noise in the              audit log for a row nobody can see"
        );
    }
    // ---------------------------------------------------------------------------------------
    // Task 12 M2: the partitioned dispatcher's outward witnesses. The dispatcher is real
    // (enumeration, fair planning, budget, progress CAS); `run_partitioned` is the disclosed
    // scaffold that still runs the whole-store body, so these are the behavioral RED for the
    // partition-scoping contracts M3's bodies must deliver.
    // ---------------------------------------------------------------------------------------

    /// One enrolled owner with a cold heat row in their workspace, `age_secs` in the past: the
    /// fixture every dispatcher witness seeds accounts with. Returns the fact's typed id.
    async fn seed_owner_cold(
        db: &Database,
        login: &str,
        content: &str,
        age_secs: i64,
    ) -> alexandria_storage::models::identity::WorkspaceId {
        let ids = IdentityRepo::new(db.inner())
            .bootstrap_owner(
                login,
                "digest-dispatch-padding-padding",
                &alexandria_storage::models::OperationId::new(format!("op-{login}")),
            )
            .await
            .unwrap();
        let memories = MemoryRepo::new(db.inner());
        let heat = HeatRepo::new(db.inner());
        let fact = memories
            .create_fact(content, 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        heat.create_for_memory(&fact, 1.0).await.unwrap();
        let touched = chrono::Utc::now() - chrono::Duration::seconds(age_secs);
        db.inner()
            .query("UPDATE heat_state SET last_touched = $touched WHERE memory = type::record($m);")
            .bind(("touched", touched))
            .bind(("m", fact.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        // Stamp the fact into the owner's workspace and the personal scope, so the partition's
        // conjunct (`workspace = $w AND scope = 'personal'`) finds it.
        db.inner()
            .query(
                "UPDATE type::record($f) SET workspace = $workspace, scope = 'personal', \
                 deleted = false;",
            )
            .bind(("f", fact.clone()))
            .bind(("workspace", ids.workspace.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        ids.workspace
    }

    /// Another cold row in an *existing* owner's workspace: the second seed for witnesses that
    /// need more eligible rows than the cap in one account.
    async fn seed_cold_in_workspace(
        db: &Database,
        workspace: &alexandria_storage::models::identity::WorkspaceId,
        content: &str,
        age_secs: i64,
    ) {
        let memories = MemoryRepo::new(db.inner());
        let heat = HeatRepo::new(db.inner());
        let fact = memories
            .create_fact(content, 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        heat.create_for_memory(&fact, 1.0).await.unwrap();
        let touched = chrono::Utc::now() - chrono::Duration::seconds(age_secs);
        db.inner()
            .query("UPDATE heat_state SET last_touched = $touched WHERE memory = type::record($m);")
            .bind(("touched", touched))
            .bind(("m", fact.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        db.inner()
            .query(
                "UPDATE type::record($f) SET workspace = $workspace, scope = 'personal', \
                 deleted = false;",
            )
            .bind(("f", fact.clone()))
            .bind(("workspace", workspace.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    /// A workspace's heat rows, by content, for asserting who was and was not touched.
    ///
    /// Typed reads, not JSON: `last_touched` lives on `heat_state`, not on `fact`, so a single-table
    /// select silently reads NONE for it — the exact defect that made the M2 RED and the 37 run fail
    /// identically whatever the body did. The join is by record id in Rust, and the rows come back
    /// as `HeatState`/`Fact` models so the anchor is a real `DateTime` or a real `None`.
    async fn workspace_heats(
        db: &Database,
        workspace: &alexandria_storage::models::identity::WorkspaceId,
    ) -> Vec<(String, f64, Option<chrono::DateTime<chrono::Utc>>)> {
        let heats: Vec<alexandria_storage::models::HeatState> = db
            .inner()
            .query(
                "SELECT * FROM heat_state WHERE memory INSIDE (SELECT VALUE id FROM fact WHERE \
                 workspace = $workspace);",
            )
            .bind(("workspace", workspace.as_record_id().clone()))
            .await
            .unwrap()
            .take(0)
            .unwrap();
        let facts: Vec<alexandria_storage::models::Fact> = db
            .inner()
            .query("SELECT * FROM fact WHERE workspace = $workspace;")
            .bind(("workspace", workspace.as_record_id().clone()))
            .await
            .unwrap()
            .take(0)
            .unwrap();
        let contents: std::collections::HashMap<String, String> = facts
            .iter()
            .map(|fact| {
                (
                    record_id_to_string(fact.id.as_ref().expect("a seeded fact has an id")),
                    fact.content.clone(),
                )
            })
            .collect();
        let mut rows: Vec<(String, f64, Option<chrono::DateTime<chrono::Utc>>)> = heats
            .iter()
            .map(|state| {
                (
                    contents
                        .get(&record_id_to_string(&state.memory))
                        .cloned()
                        .unwrap_or_default(),
                    state.heat,
                    state.last_touched,
                )
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    /// WITNESS 1 — suspension: a suspended account has no maintenance partition, and nothing the
    /// pass does writes one of its rows. First-reached failure at the scaffold: the whole-store
    /// body sweeps the suspended account's rows too, because nothing scopes it yet.
    #[tokio::test]
    async fn suspended_account_has_no_maintenance_partition_or_write() {
        let (db, system) = migrated_db().await;
        let suspended = seed_owner_cold(
            &db,
            "disp-suspended",
            "suspended account memory",
            86_400 * 365,
        )
        .await;
        let active =
            seed_owner_cold(&db, "disp-active", "active account memory", 86_400 * 30).await;
        db.inner()
            .query("UPDATE account SET status = 'suspended' WHERE login = 'disp-suspended';")
            .await
            .unwrap()
            .check()
            .unwrap();

        // Enumeration excludes the suspended workspace before any write is attempted.
        let partitions = IdentityRepo::maintenance_partitions(db.inner(), &db, &system)
            .await
            .unwrap();
        assert!(
            partitions
                .iter()
                .all(|partition| partition.workspace != suspended),
            "a suspended account's workspace is not a maintenance partition: {partitions:?}"
        );

        let jobs = Jobs::new(db.clone(), system, &Config::default());
        let pass = jobs
            .dispatch_job(Job::Sweep, "run-dispatch-suspended")
            .await
            .unwrap();
        assert_eq!(
            pass.visited, 1,
            "exactly the active account's partition ran"
        );

        let suspended_rows = workspace_heats(&db, &suspended).await;
        assert_eq!(
            suspended_rows.len(),
            1,
            "the suspended account's row count is unchanged"
        );
        assert!(
            suspended_rows[0]
                .2
                .is_some_and(|touched| touched < chrono::Utc::now() - chrono::Duration::hours(1)),
            "a suspended account's decay anchor was moved by a maintenance pass: \
             {suspended_rows:?}"
        );
        // Later branch, reached only if the refusal above held: the active account was served.
        let active_rows = workspace_heats(&db, &active).await;
        assert!(
            active_rows[0]
                .2
                .is_some_and(|touched| touched > chrono::Utc::now() - chrono::Duration::hours(1)),
            "the active account's row was swept: {active_rows:?}"
        );
    }

    /// WITNESS 2 — the allowance is one per capped job per pass across all partitions, and a
    /// second account still gets its turn. First-reached failure at the scaffold: the whole-store
    /// body serves the globally-oldest rows, so one account can consume the entire pass.
    #[tokio::test]
    async fn row_allowance_is_global_per_job_not_per_partition() {
        let (db, system) = migrated_db().await;
        // The first account holds MORE eligible rows than the cap; the second holds one. The
        // property under test is that the pass's two units are shared across the accounts rather
        // than both taken by the first account's older rows — which needs a cap of two to be
        // expressible at all (a cap of one can only ever serve one account, by the owner's
        // one-allowance-per-job-per-pass contract).
        let first =
            seed_owner_cold(&db, "disp-first", "first account memory a", 86_400 * 365).await;
        seed_cold_in_workspace(&db, &first, "first account memory b", 86_400 * 364).await;
        let second =
            seed_owner_cold(&db, "disp-second", "second account memory", 86_400 * 363).await;

        let mut config = Config::default();
        config.dreaming.max_rows_per_run = 2;
        let jobs = Jobs::new(db.clone(), system, &config);
        let pass = jobs
            .dispatch_job(Job::Sweep, "run-dispatch-global-budget")
            .await
            .unwrap();

        assert!(
            pass.budget_used <= 2,
            "the pass's chargeable units stayed within the single global allowance: {pass:?}"
        );
        let second_rows = workspace_heats(&db, &second).await;
        assert!(
            second_rows[0]
                .2
                .is_some_and(|touched| touched > chrono::Utc::now() - chrono::Duration::hours(1)),
            "a second account with eligible rows got no turn: the allowance was not shared: \
             {second_rows:?}"
        );
        // Later branch: the first account was served exactly one row — its second-oldest stays
        // anchored in the past, which is also the proof the cap is a *global* two rather than a
        // per-partition two.
        let first_rows = workspace_heats(&db, &first).await;
        assert_eq!(
            first_rows.len(),
            2,
            "both seeded rows remain: {first_rows:?}"
        );
        assert!(
            first_rows[0]
                .2
                .is_some_and(|touched| touched > chrono::Utc::now() - chrono::Duration::hours(1)),
            "the first account's oldest row was swept: {first_rows:?}"
        );
        assert!(
            first_rows[1]
                .2
                .is_some_and(|touched| touched < chrono::Utc::now() - chrono::Duration::hours(1)),
            "the first account's second row was also swept: the cap was not global: {first_rows:?}"
        );
    }

    /// WITNESS 3 — progress survives a restart: a second pass, over a freshly constructed
    /// dispatcher, continues the rotation instead of replaying the first account again.
    ///
    /// The store's workspace enumeration is deterministic *per run* but its keys are
    /// engine-minted random ids, so the witness names the accounts by their TURN (first and
    /// second enumerated) rather than by their seed order - the rotation property is symmetric,
    /// and a seed-order assumption was a per-run coin flip. Both accounts hold two rows to a cap
    /// of one, so whichever account pass one serves, its second row must wait for a later pass
    /// and pass two must rotate to the other account.
    #[tokio::test]
    async fn maintenance_progress_survives_restart_and_many_projects() {
        let (db, system) = migrated_db().await;
        // "rotation alpha" is each account's OLDEST row and sorts before "rotation beta".
        let a = seed_owner_cold(&db, "disp-rot-a", "rotation alpha", 86_400 * 365).await;
        seed_cold_in_workspace(&db, &a, "rotation beta", 86_400 * 364).await;
        let b = seed_owner_cold(&db, "disp-rot-b", "rotation alpha", 86_400 * 363).await;
        seed_cold_in_workspace(&db, &b, "rotation beta", 86_400 * 362).await;

        // Name the accounts by their turn: the enumeration order is the rotation's starting point.
        let partitions = IdentityRepo::maintenance_partitions(db.inner(), &db, &system)
            .await
            .unwrap();
        assert_eq!(partitions.len(), 2, "two accounts, one partition each");
        let (first, second) = (
            partitions[0].workspace.clone(),
            partitions[1].workspace.clone(),
        );

        let mut config = Config::default();
        config.dreaming.max_rows_per_run = 1;
        let jobs = Jobs::new(db.clone(), system.clone(), &config);
        let first_pass = jobs
            .dispatch_job(Job::Sweep, "run-dispatch-restart-1")
            .await
            .unwrap();
        assert_eq!(
            first_pass.visited, 1,
            "pass one ran one partition: {first_pass:?} (last partition error: {:?})",
            first_pass.last_error
        );

        // Pass one served the FIRST-enumerated account's oldest row only: its beta row and the
        // whole second account keep their seeded anchors.
        let after_first_first = workspace_heats(&db, &first).await;
        let after_first_second = workspace_heats(&db, &second).await;
        assert_eq!(
            after_first_first.len(),
            2,
            "both of the first turn's rows remain: {after_first_first:?}"
        );
        assert!(
            after_first_first[0]
                .2
                .is_some_and(|touched| touched > chrono::Utc::now() - chrono::Duration::hours(1)),
            "pass one served the first turn's oldest row: {after_first_first:?} \
             (pass one: {first_pass:?})"
        );
        assert!(
            after_first_first[1]
                .2
                .is_some_and(|touched| touched < chrono::Utc::now() - chrono::Duration::hours(1)),
            "pass one's single-row allowance did not reach the first turn's second row: \
             {after_first_first:?}"
        );
        assert!(
            after_first_second.iter().all(|row| row
                .2
                .is_some_and(|touched| touched < chrono::Utc::now() - chrono::Duration::hours(1))),
            "pass one served only the first turn's account: {after_first_second:?}"
        );
        let first_turn_anchor = after_first_first[0].2;

        // A second Jobs over the same store is the inline stand-in for a restart: nothing but the
        // persisted ledger can tell it where the first pass stopped.
        let jobs = Jobs::new(db.clone(), system, &config);
        let second_pass = jobs
            .dispatch_job(Job::Sweep, "run-dispatch-restart-2")
            .await
            .unwrap();
        assert_eq!(
            second_pass.visited, 1,
            "the second pass ran one partition: {second_pass:?} (last error: {:?})",
            second_pass.last_error
        );

        // The rotation advanced: pass two served the OTHER account's oldest row.
        let after_second_second = workspace_heats(&db, &second).await;
        assert_ne!(
            after_second_second[0].2, after_first_second[0].2,
            "the second pass did not continue the rotation onto the second turn's account: \
             {after_second_second:?} (after pass one: {after_first_second:?})"
        );
        assert!(
            after_second_second[1]
                .2
                .is_some_and(|touched| touched < chrono::Utc::now() - chrono::Duration::hours(1)),
            "the second pass's single-row allowance did not reach the second turn's beta row: \
             {after_second_second:?}"
        );
        let after_second_first = workspace_heats(&db, &first).await;
        assert_eq!(
            after_second_first[0].2, first_turn_anchor,
            "the first turn's already-swept row was swept a second time: {after_second_first:?} \
             (its anchor after pass one was {first_turn_anchor:?})"
        );
        assert!(
            after_second_first[1]
                .2
                .is_some_and(|touched| touched < chrono::Utc::now() - chrono::Duration::hours(1)),
            "the first turn's second row was swept by the second pass - the rotation skipped a \
             turn: {after_second_first:?}"
        );
    }

    /// WITNESS 4 — a partition the writer refuses (the account stopped being active mid-pass)
    /// advances the pass, is reported as refused rather than failed or visited, and does not give
    /// its reservation back to the budget.
    #[tokio::test]
    async fn partial_partition_failure_cannot_reset_the_budget() {
        let (db, system) = migrated_db().await;
        let refused =
            seed_owner_cold(&db, "disp-refused", "refused account memory", 86_400 * 365).await;
        let active = seed_owner_cold(
            &db,
            "disp-refused-active",
            "active peer memory",
            86_400 * 364,
        )
        .await;
        // Suspend after seeding: the partition exists in the corpus, and the pass must refuse its
        // writes through the barrier rather than serve or crash on them.
        db.inner()
            .query("UPDATE account SET status = 'suspended' WHERE login = 'disp-refused';")
            .await
            .unwrap()
            .check()
            .unwrap();

        let mut config = Config::default();
        config.dreaming.max_rows_per_run = 2;
        let jobs = Jobs::new(db.clone(), system, &config);
        let pass = jobs
            .dispatch_job(Job::Sweep, "run-dispatch-partial-failure")
            .await
            .unwrap();

        assert_eq!(
            (pass.visited, pass.refused, pass.failed),
            (1, 0, 0),
            "the suspended partition was neither visited nor reported as a refusal: {pass:?}"
        );
        assert!(
            pass.budget_used <= 2,
            "a refused or failed partition did not return its reservation to the budget: {pass:?}"
        );
        // Later branch: the active peer still got its turn.
        let active_rows = workspace_heats(&db, &active).await;
        assert!(
            active_rows[0]
                .2
                .is_some_and(|touched| touched > chrono::Utc::now() - chrono::Duration::hours(1)),
            "the active peer lost its turn to a refused partition: {active_rows:?}"
        );
        let refused_rows = workspace_heats(&db, &refused).await;
        assert!(
            refused_rows[0]
                .2
                .is_some_and(|touched| touched < chrono::Utc::now() - chrono::Duration::hours(1)),
            "the suspended account's row was written: {refused_rows:?}"
        );
    }

    /// WITNESS 5a — appraise is partition-scoped: a cold, never-accessed, post-arm row in the
    /// active account is demoted, and a byte-identical eligible row in a suspended account is not
    /// (no partition, no write), with the audit rows attributed to this run.
    #[tokio::test]
    async fn appraise_is_partition_scoped_and_audited() {
        let (db, system) = migrated_db().await;
        // Arm access recording first: appraise's precondition. Rows created after the arm with
        // zero accesses are the demotion population.
        alexandria_storage::system_config::arm_access_recording(db.inner())
            .await
            .unwrap();
        let active =
            seed_owner_cold(&db, "disp-appraise-active", "cold memory", 86_400 * 400).await;
        let suspended =
            seed_owner_cold(&db, "disp-appraise-suspended", "cold memory", 86_400 * 400).await;
        db.inner()
            .query(
                "UPDATE account SET status = 'suspended' WHERE login = 'disp-appraise-suspended';",
            )
            .await
            .unwrap()
            .check()
            .unwrap();

        let jobs = Jobs::new(db.clone(), system, &Config::default());
        let pass = jobs
            .dispatch_job(Job::Appraise, "run-dispatch-appraise")
            .await
            .unwrap();
        assert_eq!(
            pass.visited, 1,
            "only the active account's partition ran: {pass:?}"
        );
        assert_eq!(
            pass.report.acted, 1,
            "the active account's cold row was demoted: {pass:?}"
        );

        let rows: Vec<serde_json::Value> = db
            .inner()
            .query("SELECT content, confidence FROM fact WHERE workspace INSIDE $workspaces ORDER BY content;")
            .bind(("workspaces", vec![
                active.as_record_id().clone(),
                suspended.as_record_id().clone(),
            ]))
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(rows.len(), 2, "both rows remain: {rows:?}");
        assert_eq!(
            rows[0]["confidence"].as_f64(),
            Some(alexandria_engine::dreaming::DEMOTED_CONFIDENCE),
            "the active account's row was demoted: {rows:?}"
        );
        assert_eq!(
            rows[1]["confidence"].as_f64(),
            Some(0.5),
            "the suspended account's row kept its confidence: {rows:?}"
        );
        let logs: Vec<serde_json::Value> = db
            .inner()
            .query("SELECT source_id, job, actor FROM maintenance_log WHERE run_id = 'run-dispatch-appraise';")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(
            logs.len(),
            1,
            "exactly one audit row for the demotion: {logs:?}"
        );
        assert_eq!(logs[0]["job"], "appraise");
        assert_eq!(logs[0]["actor"], "system:dreaming");
    }

    /// WITNESS 5c (run 84, finding 1) — appraise's page must ADVANCE past examined-but-ineligible
    /// rows: a partition whose oldest rows fail the armed-at precondition still reaches its newer
    /// eligible rows within bounded passes. Without the persisted row cursor the same ineligible
    /// page is re-selected every pass (appraise does not move `last_touched`, so the oldest-first
    /// order never changes under it) and the eligible rows behind it starve forever.
    #[tokio::test]
    async fn appraise_pages_past_an_ineligible_head_within_bounded_passes() {
        let (db, system) = migrated_db().await;
        // The ineligible head FIRST, so its facts are created before the arm and the appraise
        // rule must skip them (created_at < armed_at): three cold rows, oldest first.
        let workspace = seed_owner_cold(
            &db,
            "disp-appraise-page",
            "ineligible head one",
            86_400 * 400,
        )
        .await;
        seed_cold_in_workspace(&db, &workspace, "ineligible head two", 86_400 * 399).await;
        seed_cold_in_workspace(&db, &workspace, "ineligible head three", 86_400 * 398).await;
        // Arm access recording now: everything seeded above predates it.
        alexandria_storage::system_config::arm_access_recording(db.inner())
            .await
            .unwrap();
        // The eligible tail: created after the arm, cold, never accessed — the demotion
        // population, sitting BEHIND the ineligible head in the oldest-first order.
        seed_cold_in_workspace(&db, &workspace, "eligible tail one", 86_400 * 397).await;
        seed_cold_in_workspace(&db, &workspace, "eligible tail two", 86_400 * 396).await;

        // A two-row page: pass 1 examines the first two ineligible rows and advances past them,
        // pass 2 the third plus the first eligible row, pass 3 the last. Bounded at three passes
        // for two eligible rows behind a three-row ineligible head — the unfixable shape re-read
        // the same head forever.
        let mut config = Config::default();
        config.dreaming.max_rows_per_run = 2;
        let jobs = Jobs::new(db.clone(), system, &config);

        let mut demotions = 0usize;
        for pass in 1..=3 {
            let report = jobs
                .dispatch_job(Job::Appraise, &format!("run-appraise-page-{pass}"))
                .await
                .unwrap();
            assert_eq!(report.visited, 1, "the one partition ran: {report:?}");
            assert!(report.last_error.is_none(), "the pass failed: {report:?}");
            demotions += report.report.acted;
        }
        assert_eq!(
            demotions, 2,
            "both eligible tail rows were demoted within three bounded passes"
        );

        // And the head was skipped, not demoted: the armed-at precondition held.
        let rows: Vec<serde_json::Value> = db
            .inner()
            .query("SELECT content, confidence FROM fact WHERE workspace = $workspace ORDER BY content;")
            .bind(("workspace", workspace.as_record_id().clone()))
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(rows.len(), 5, "every seeded row remains: {rows:?}");
        for row in &rows {
            let expected = if row["content"]
                .as_str()
                .unwrap_or_default()
                .starts_with("ineligible")
            {
                0.5
            } else {
                alexandria_engine::dreaming::DEMOTED_CONFIDENCE
            };
            assert_eq!(
                row["confidence"].as_f64(),
                Some(expected),
                "wrong confidence for {}: {rows:?}",
                row["content"].as_str().unwrap_or_default()
            );
        }
    }

    /// WITNESS 5d (run 84, finding 2) — a partition exhausted in one pass whose account suspends
    /// before the next must not underflow the pass arithmetic: the stale exhausted cursor is
    /// coordination data for a partition the store no longer enumerates, and it must not affect
    /// the pass at all. Pre-fix, `partitions.len() - exhausted.len()` panicked on the unsigned
    /// underflow the moment the enumerated set shrank below the exhausted set.
    #[tokio::test]
    async fn a_suspended_exhausted_partition_does_not_underflow_the_pass() {
        let (db, system) = migrated_db().await;
        alexandria_storage::system_config::arm_access_recording(db.inner())
            .await
            .unwrap();
        seed_owner_cold(&db, "disp-underflow-a", "cold memory a", 86_400 * 400).await;
        seed_owner_cold(&db, "disp-underflow-b", "cold memory b", 86_400 * 400).await;

        let jobs = Jobs::new(db.clone(), system, &Config::default());
        // Pass 1: both partitions visited, both cold rows demoted, both pages short of the
        // share (one row against hundreds), so both cursors are marked exhausted.
        let pass1 = jobs
            .dispatch_job(Job::Appraise, "run-underflow-1")
            .await
            .unwrap();
        assert_eq!(
            (pass1.visited, pass1.report.acted),
            (2, 2),
            "both partitions ran and demoted: {pass1:?}"
        );

        // Suspend one account between passes: its partition leaves the enumeration while its
        // exhausted cursor stays in the ledger.
        db.inner()
            .query("UPDATE account SET status = 'suspended' WHERE login = 'disp-underflow-a';")
            .await
            .unwrap()
            .check()
            .unwrap();

        // Pass 2: enumerated = one partition; stale exhausted = two. The pass must complete
        // (this is the line that underflowed), wrap the cycle — the one enumerated partition is
        // exhausted — and visit that partition fresh.
        let pass2 = jobs
            .dispatch_job(Job::Appraise, "run-underflow-2")
            .await
            .unwrap();
        assert_eq!(
            pass2.visited, 1,
            "the remaining account's partition ran: {pass2:?}"
        );
        // Its row already carries the demoted confidence, which exempts itself from the rule, so
        // the fresh walk demotes nothing — the pass is correct, not merely non-crashing.
        assert_eq!(pass2.report.acted, 0, "no second demotion: {pass2:?}");
    }

    /// WITNESS 5b — collapse is partition-scoped: byte-identical rows within EACH account's
    /// partition collapse (edge before delete, one audit row per retirement), while the same
    /// content in the *other* account's corpus is never a collapse partner, and the pass charges
    /// retirements against the budget. Both accounts hold a duplicate pair, so the outcome is
    /// independent of the store's per-run workspace enumeration order.
    #[tokio::test]
    async fn collapse_is_partition_scoped_and_counts_retirements() {
        let (db, system) = migrated_db().await;
        let first = seed_owner_cold(&db, "disp-collapse-a", "duplicate memory", 86_400 * 30).await;
        seed_cold_in_workspace(&db, &first, "duplicate memory", 86_400 * 29).await;
        let second = seed_owner_cold(&db, "disp-collapse-b", "duplicate memory", 86_400 * 28).await;
        seed_cold_in_workspace(&db, &second, "duplicate memory", 86_400 * 27).await;

        let jobs = Jobs::new(db.clone(), system, &Config::default());
        let pass = jobs
            .dispatch_job(Job::Collapse, "run-dispatch-collapse")
            .await
            .unwrap();
        assert_eq!(
            pass.visited, 2,
            "both accounts' partitions ran: {pass:?} (last partition error: {:?})",
            pass.last_error
        );
        // One retirement in each partition: each groups its own pair. The charge is retirements,
        // not candidate reads (4 read, 2 retired).
        assert_eq!(
            pass.report.examined, 4,
            "each partition read its own pair: {pass:?}"
        );
        assert_eq!(
            pass.report.acted, 2,
            "one retirement per partition: {pass:?}"
        );
        assert_eq!(
            pass.budget_used, 2,
            "the charge counts retirements: {pass:?}"
        );

        // The edge endpoints decode as typed record ids - no display string is ever parsed.
        let endpoints: Vec<RecordId> = db
            .inner()
            .query("SELECT VALUE in FROM memory_edge WHERE edge_type = 'derived_from';")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(endpoints.len(), 2, "one lineage edge per retirement");
        for source in &endpoints {
            let rows: Vec<serde_json::Value> = db
                .inner()
                .query("SELECT workspace, deleted FROM $id;")
                .bind(("id", source.clone()))
                .await
                .unwrap()
                .take(0)
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0]["deleted"],
                true,
                "every edge source is soft-deleted (edge before delete): {}",
                record_id_to_string(source)
            );
        }
        // Cross-partition isolation: no edge crosses the two workspaces.
        let cross: Vec<serde_json::Value> = db
            .inner()
            .query(
                "SELECT VALUE in FROM memory_edge WHERE edge_type = 'derived_from' AND \
                 (in.workspace != out.workspace);",
            )
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert!(
            cross.is_empty(),
            "no cross-workspace collapse edge: {cross:?}"
        );
    }

    /// WITNESS 5 — a store with no eligible rows terminates its pass cleanly rather than spinning.
    #[tokio::test]
    async fn empty_partitions_terminate_without_spinning() {
        let (db, system) = migrated_db().await;
        // An enrolled account with no memories at all: its personal partition is eligible and empty.
        IdentityRepo::new(db.inner())
            .bootstrap_owner(
                "disp-empty",
                "digest-empty-padding-padding-pad",
                &alexandria_storage::models::OperationId::new("op-disp-empty"),
            )
            .await
            .unwrap();

        let jobs = Jobs::new(db.clone(), system, &Config::default());
        let pass = jobs
            .dispatch_job(Job::Sweep, "run-dispatch-empty")
            .await
            .unwrap();
        assert_eq!(
            (pass.report.examined, pass.report.acted),
            (0, 0),
            "an empty population did nothing: {pass:?}"
        );
        assert_eq!(pass.visited, 1, "the empty partition was visited once");
    }
}
