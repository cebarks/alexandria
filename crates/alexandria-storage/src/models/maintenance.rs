use crate::models::identity::{MemoryScope, Version, WorkspaceId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct MaintenanceLog {
    pub id: Option<RecordId>,
    pub action: String,
    pub source_id: String,
    pub target_ids: Vec<String>,
    pub members_moved: i64,
    pub created_at: Option<DateTime<Utc>>,
    /// One tick's unit of reversal: every row a run touched shares this id, so a pass that
    /// soft-deleted 40 memories can be undone (or later gated) wholesale. `NONE` on rows written
    /// before v008.
    pub run_id: Option<String>,
    /// Which scheduler job wrote the row — [`job`] constants below. `NONE` on pre-v008 rows.
    pub job: Option<String>,
    /// `NONE` on pre-v008 rows.
    pub actor: Option<String>,
    /// Which rung of the demote -> quarantine -> soft-delete ladder the row landed on, or `NONE`
    /// for the cluster moves, which are not a disposition.
    pub disposition: Option<String>,
    /// The value a value-changing job overwrote — for a demotion, the confidence before the write.
    /// `NONE` for the cluster moves, which overwrite no scalar. Without it `run_id` names a pass but
    /// does not make it reversible.
    pub previous_value: Option<f64>,
}

/// The writer identity for every dreaming job. Dreaming is the one writer that can never produce
/// a user identity, which is why the column exists at all rather than being backfilled later.
pub const ACTOR_DREAMING: &str = "system:dreaming";

/// Which of the five jobs wrote the row. `Sweep` is absent on purpose: materialising a decayed
/// value is a proven no-op on ranking, so it is not recorded — a sweep would add up to
/// `max_rows_per_run` rows per hour and bury the collapse and demote entries that are the reason
/// this table exists. Observability for the sweep is its `JobReport` at trace level.
pub mod job {
    pub const CLUSTER: &str = "cluster";
    pub const MERGE: &str = "merge";
    pub const COLLAPSE: &str = "collapse";
    pub const APPRAISE: &str = "appraise";
}

/// The three rungs of the ladder. Mirrors the schema's intent; the allowlist lives here because
/// asserting on an optional column would have to accept NONE plus every value, which is a rule no
/// reader can check at a glance.
pub mod disposition {
    /// Confidence lowered; the row stays retrievable.
    pub const DEMOTE: &str = "demote";
    /// Hidden from every live read path, still inspectable. The state #29 secret-scanning needs.
    pub const QUARANTINE: &str = "quarantine";
    /// Certain noise, or a collapsed duplicate with a `derived_from` edge to its survivor.
    pub const SOFT_DELETE: &str = "soft_delete";
}

/// The row-capped dreaming jobs, and the persisted cursor state that lets a partial pass resume.
///
/// Deliberately *not* the same list as [`job`]: that one is the audit namespace, where `sweep` has no
/// row because materialising a decayed value records nothing worth reversing. A budget job needs a
/// ledger; an audit job needs a log line. `Sweep` appears here and nowhere in [`job`] for exactly
/// that reason, and the `maintenance_progress.job` assertion in v009 accepts this list and only this
/// list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MaintenanceBudgetJob {
    /// Materialises decayed heat. Row-capped by selected page.
    Sweep,
    /// Retires byte-identical duplicates. Row-capped by successfully retired fact.
    Collapse,
    /// Demotes cold, never-accessed rows. Row-capped by selected page.
    Appraise,
}

impl MaintenanceBudgetJob {
    /// The value persisted in `maintenance_progress.job`, and the spelling the schema's `ASSERT`
    /// accepts. `cluster` and `merge` are absent because they are not row-budgeted.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sweep => budget_job::SWEEP,
            Self::Collapse => budget_job::COLLAPSE,
            Self::Appraise => budget_job::APPRAISE,
        }
    }

    /// Every capped job. The order is this declaration's order, which is deterministic on purpose:
    /// a pass that enumerated jobs in hash order would visit the same ledger twice before another.
    pub fn all() -> [Self; 3] {
        [Self::Sweep, Self::Collapse, Self::Appraise]
    }

    /// Parses the persisted spelling. Anything else — including an audit-only job name — is refused,
    /// because a ledger row for a job with no row budget would be a budget that no code honours.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            budget_job::SWEEP => Some(Self::Sweep),
            budget_job::COLLAPSE => Some(Self::Collapse),
            budget_job::APPRAISE => Some(Self::Appraise),
            _ => None,
        }
    }
}

/// The `maintenance_progress.job` values. Mirrors the schema's `ASSERT` the same way [`job`] and
/// [`action`] mirror theirs; [`MaintenanceBudgetJob`] is the type callers pass.
pub mod budget_job {
    pub const SWEEP: &str = "sweep";
    pub const COLLAPSE: &str = "collapse";
    pub const APPRAISE: &str = "appraise";
}

/// One job's persisted fair-share state: where the last pass stopped, and which partitions have
/// already been visited or exhausted in the current one.
///
/// Storage-owned coordination data — the engine consumes it and never the reverse, and nothing here
/// imports an engine, MCP or chrono type. It is *not* an audit row: the ledger records position, the
/// audit table records what happened, and a pass that made no semantic change still advances this and
/// writes nothing there.
///
/// [`Version`] is the compare-and-swap guard Task 12's save path passes as `expected`: a lost race
/// must fail rather than overwrite another writer's cursor. A record that does not exist yet loads as
/// [`MaintenanceProgress::untouched`] — version zero, no cursors — which is the only value a fresh
/// store can honestly report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceProgress {
    pub job: MaintenanceBudgetJob,
    pub version: Version,
    /// The workspace the rotation last visited, so the next pass continues the round-robin instead
    /// of always starting at the same account.
    pub last_workspace: Option<WorkspaceId>,
    /// One cursor per `(workspace, scope)` partition the job has touched.
    pub cursors: Vec<WorkspaceProgress>,
}

/// One partition's cursor within a [`MaintenanceProgress`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceProgress {
    pub workspace: WorkspaceId,
    /// Which population within the workspace this cursor is for. A project scope names its project
    /// handle, so two workspaces with the same alias cannot share a cursor by accident.
    pub scope: MemoryScope,
    /// The last row the pass processed, so it resumes after it. `None` means the partition has not
    /// been entered yet in this pass.
    pub row_cursor: Option<RecordId>,
    /// Chargeable units the pass already spent here — selected page rows for `sweep`/`appraise`,
    /// retired facts for `collapse`. Not a candidate count: the budget is over work, not reads.
    pub units_done: i64,
    /// The partition terminated (empty or fully processed) in this pass, so a later pass skips it
    /// rather than spinning on it.
    pub exhausted: bool,
}

impl MaintenanceProgress {
    /// The state of a job that has never run: version zero and no cursors, which is what a first
    /// save guards against rather than assuming a row exists.
    pub fn untouched(job: MaintenanceBudgetJob) -> Self {
        Self {
            job,
            version: Version::ZERO,
            last_workspace: None,
            cursors: Vec::new(),
        }
    }
}

/// The verbs written to `action`. `split` and `merge` predate v008.
pub mod action {
    pub const SPLIT: &str = "split";
    pub const MERGE: &str = "merge";
    pub const COLLAPSE: &str = "collapse";
    pub const DEMOTE: &str = "demote";
}
