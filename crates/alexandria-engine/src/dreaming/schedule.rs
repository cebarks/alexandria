//! Which housekeeping job runs when, expressed as arithmetic rather than as a loop.
//!
//! This is the half of `crates/alexandria/src/dreaming.rs` that can be tested without a database,
//! and it lives here because the `alexandria` crate is binary-only — a file under
//! `crates/alexandria/tests/` cannot import anything from its `src/`, so anything placed there is
//! reachable only from an inline test module. Due-time behaviour is exactly the kind of thing to
//! pin properly rather than eyeball, since a scheduler that quietly stops running one job is
//! indistinguishable from a healthy one for days.

/// Heat materialisation: hourly. Cheap and bounded, and `Appraise` reads what it wrote.
pub const DEFAULT_SWEEP_INTERVAL_SECS: u64 = 3_600;
/// Cohesion check → split. Preserves today's cluster-maintenance cadence.
pub const DEFAULT_CLUSTER_INTERVAL_SECS: u64 = 300;
/// Centroid similarity → merge. Same cadence, separately named because merge is the expensive
/// half: O(clusters²) comparisons with a member read per cluster.
pub const DEFAULT_MERGE_INTERVAL_SECS: u64 = 300;
/// Byte-identical duplicate collapse. Daily.
pub const DEFAULT_COLLAPSE_INTERVAL_SECS: u64 = 86_400;
/// Cold-row demotion. Daily, and after the sweep at least once so it reads materialised values.
pub const DEFAULT_APPRAISE_INTERVAL_SECS: u64 = 86_400;
/// Per-job bound, so a large corpus drains across ticks instead of stalling one.
pub const DEFAULT_MAX_ROWS_PER_RUN: usize = 500;

/// The five jobs. Two of them (`Cluster`, `Merge`) are today's single maintenance loop split
/// along the phase boundary it already had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Job {
    Sweep,
    Cluster,
    Merge,
    Collapse,
    Appraise,
}

impl Job {
    /// The value written to `maintenance_log.job`. It must match the constants in
    /// `alexandria_storage::models::maintenance::job`; `Sweep` is never written, because a pass
    /// that is a proven no-op on ranking has nothing worth reversing.
    pub fn as_str(self) -> &'static str {
        match self {
            Job::Sweep => "sweep",
            Job::Cluster => "cluster",
            Job::Merge => "merge",
            Job::Collapse => "collapse",
            Job::Appraise => "appraise",
        }
    }

    /// Whether the job runs on the first tick after boot, or waits a full interval.
    ///
    /// `Cluster` and `Merge` run immediately because that is the behaviour being preserved: the
    /// current maintenance loop uses `tokio::time::interval`, which fires on its first tick. The
    /// long-cadence jobs wait instead — a server restarted by its supervisor should not scan the
    /// whole corpus for duplicates on every start, and delaying them costs latency rather than
    /// correctness.
    pub fn run_at_boot(self) -> bool {
        matches!(self, Job::Cluster | Job::Merge)
    }
}

/// The configured cadences, as one value. Mirrors the `[dreaming]` section of the config file but
/// carries no knowledge of TOML, so the engine stays free of the binary's types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Intervals {
    pub sweep_secs: u64,
    pub cluster_secs: u64,
    pub merge_secs: u64,
    pub collapse_secs: u64,
    pub appraise_secs: u64,
}

impl Default for Intervals {
    fn default() -> Self {
        Self {
            sweep_secs: DEFAULT_SWEEP_INTERVAL_SECS,
            cluster_secs: DEFAULT_CLUSTER_INTERVAL_SECS,
            merge_secs: DEFAULT_MERGE_INTERVAL_SECS,
            collapse_secs: DEFAULT_COLLAPSE_INTERVAL_SECS,
            appraise_secs: DEFAULT_APPRAISE_INTERVAL_SECS,
        }
    }
}

impl Intervals {
    /// The soonest any job can come due.
    ///
    /// The loop sleeps until the *soonest* job is due, so nothing legitimately keeps it quiet longer
    /// than this. It is the basis for the dashboard's silence limit rather than a guessed constant,
    /// because an all-hour configuration and a 5-minute cluster cadence are not the same statement
    /// about health.
    #[must_use]
    pub fn shortest_secs(&self) -> u64 {
        [
            self.sweep_secs,
            self.cluster_secs,
            self.merge_secs,
            self.collapse_secs,
            self.appraise_secs,
        ]
        .into_iter()
        .min()
        .unwrap_or(0)
    }

    pub fn interval_for(&self, job: Job) -> u64 {
        match job {
            Job::Sweep => self.sweep_secs,
            Job::Cluster => self.cluster_secs,
            Job::Merge => self.merge_secs,
            Job::Collapse => self.collapse_secs,
            Job::Appraise => self.appraise_secs,
        }
    }

    /// The scheduler's initial state at `now`: jobs that run at boot are already due, the rest are
    /// anchored at `now` so they wait a full interval.
    pub fn initial_schedule(&self, now: u64) -> Schedule {
        Schedule {
            timings: ALL_JOBS
                .iter()
                .map(|job| JobTiming {
                    job: *job,
                    interval_secs: self.interval_for(*job),
                    last_run: if job.run_at_boot() { None } else { Some(now) },
                })
                .collect(),
        }
    }
}

/// Every job, in the order the scheduler runs a due set. Fixed order is deliberate: it keeps one
/// tick's effects reproducible, which is the whole reason the jobs share a loop.
pub const ALL_JOBS: [Job; 5] = [
    Job::Sweep,
    Job::Cluster,
    Job::Merge,
    Job::Collapse,
    Job::Appraise,
];

/// One job's cadence and last run. `last_run` is `None` only before a job's first run.
#[derive(Debug, Clone, Copy)]
pub struct JobTiming {
    pub job: Job,
    pub interval_secs: u64,
    pub last_run: Option<u64>,
}

impl JobTiming {
    /// `now - last_run >= interval`. A zero interval therefore means "due on every tick" — the
    /// binary refuses to start on one rather than silently turning a daily job into a hot loop.
    pub fn is_due(&self, now: u64) -> bool {
        match self.last_run {
            None => true,
            Some(at) => now.saturating_sub(at) >= self.interval_secs,
        }
    }

    /// Seconds until this job is next due, `0` if it is due now. The loop sleeps for the minimum
    /// across all five jobs rather than polling on a fixed tick, so a config change that makes the
    /// shortest interval longer cannot leave the process waking up more often than it needs to.
    pub fn remaining_secs(&self, now: u64) -> u64 {
        match self.last_run {
            None => 0,
            Some(at) => self.interval_secs.saturating_sub(now.saturating_sub(at)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Schedule {
    timings: Vec<JobTiming>,
}

impl Schedule {
    /// Jobs due at `now`, in [`ALL_JOBS`] order. At most one entry per job, so a process that was
    /// down for three intervals catches up with a single run rather than three — the lazy-due
    /// discipline the reminder scheduler already uses: a missed tick costs latency, never
    /// correctness.
    pub fn due(&self, now: u64) -> Vec<Job> {
        ALL_JOBS
            .iter()
            .copied()
            .filter(|job| self.timing(*job).is_some_and(|timing| timing.is_due(now)))
            .collect()
    }

    /// The soonest any job is next due, used as the loop's sleep length.
    pub fn next_wait_secs(&self, now: u64) -> u64 {
        self.timings
            .iter()
            .map(|timing| timing.remaining_secs(now))
            .min()
            .unwrap_or(0)
    }

    pub fn timing(&self, job: Job) -> Option<JobTiming> {
        self.timings.iter().find(|t| t.job == job).copied()
    }

    /// Anchor `job`'s next due-time at `now`. Marking one job must not disturb the others: that is
    /// what makes one job's failure harmless to the rest of the tick.
    pub fn mark_run(&mut self, job: Job, now: u64) {
        if let Some(timing) = self.timings.iter_mut().find(|t| t.job == job) {
            timing.last_run = Some(now);
        }
    }
}

/// What one job did in one run, for the trace log and (later) the debug page.
#[derive(Debug, Clone, Copy)]
pub struct JobReport {
    pub job: Job,
    /// Rows read and decided over.
    pub examined: usize,
    /// Rows whose stored state actually changed.
    pub acted: usize,
    /// Rows read but **not eligible** for a reason the operator could act on: a heat row whose fact
    /// is gone, or a memory that predates access recording. Kept separate from `examined` because
    /// `examined: N, acted: 0` is otherwise read as "the corpus is fine" when it can also mean
    /// "every row on this page was unjudgeable".
    pub skipped: usize,
}

impl JobReport {
    /// A run that has examined nothing yet. There is no `Default`: a report without a `job` is
    /// meaningless, and deriving it would have to invent one.
    pub fn new(job: Job) -> Self {
        Self {
            job,
            examined: 0,
            acted: 0,
            skipped: 0,
        }
    }
}
