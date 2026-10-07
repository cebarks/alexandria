//! Scheduled housekeeping ("dreaming") — the pure half of GitHub issue #43.
//!
//! Due-time arithmetic and the job vocabulary live here rather than in the binary crate because
//! `alexandria` is a binary-only crate: a file under `crates/alexandria/tests/` cannot import
//! anything from its `src/`, so logic placed there is reachable only from an inline `#[cfg(test)]`
//! module. Putting the decisions here makes them testable against the whole engine suite, and keeps
//! the binary's job to exactly one thing — talking to the database.

pub mod appraise;
pub mod collapse;
pub mod liveness;
pub mod schedule;

pub use collapse::{Candidate, DuplicateGroup, UNKNOWN_CREATED_AT, duplicate_groups};

pub use appraise::{
    DEFAULT_COLD_HEAT_FLOOR, DEFAULT_DEMOTE_CONFIDENCE_CEILING, DEMOTED_CONFIDENCE, is_cold,
    should_demote,
};

pub use liveness::{Liveness, SchedulerState};

pub use schedule::{
    ALL_JOBS, DEFAULT_APPRAISE_INTERVAL_SECS, DEFAULT_CLUSTER_INTERVAL_SECS,
    DEFAULT_COLLAPSE_INTERVAL_SECS, DEFAULT_MAX_ROWS_PER_RUN, DEFAULT_MERGE_INTERVAL_SECS,
    DEFAULT_SWEEP_INTERVAL_SECS, Intervals, Job, JobReport, JobTiming, Schedule,
};
