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

/// The verbs written to `action`. `split` and `merge` predate v008.
pub mod action {
    pub const SPLIT: &str = "split";
    pub const MERGE: &str = "merge";
    pub const COLLAPSE: &str = "collapse";
    pub const DEMOTE: &str = "demote";
}
