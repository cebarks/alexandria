use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

/// `Reminder::schedule_kind` discriminators. Single source of truth for the
/// string set that is also asserted by `DEFINE FIELD schedule_kind ... ASSERT
/// $value IN [...]` in `schema/v006_reminder.surql`; writers and
/// `alexandria_engine::reminders::spec_from_reminder` match on these constants
/// so the two sides can't drift silently.
pub mod schedule_kind {
    pub const ONCE: &str = "once";
    pub const PATTERN: &str = "pattern";
    pub const CRON: &str = "cron";
}

/// A scheduled message. Flat storage shape; see
/// `alexandria_engine::reminders::spec_from_reminder` for reconstruction into a
/// validated schedule.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Reminder {
    pub id: Option<RecordId>,
    pub message: String,
    /// None = global target; Some(name) = deliver only in that project (until escalation).
    pub target_project: Option<String>,
    // Provenance (display-only metadata)
    pub prov_project: Option<String>,
    pub prov_session_id: Option<String>,
    pub note: Option<String>,
    // Schedule: kind discriminator + kind-specific fields
    /// One of [`schedule_kind::ONCE`], [`schedule_kind::PATTERN`],
    /// [`schedule_kind::CRON`].
    pub schedule_kind: String,
    pub due_at: Option<DateTime<Utc>>,
    pub freq: Option<String>,
    pub time_of_day: Option<String>,
    pub weekdays: Vec<String>,
    pub day_of_month: Option<i64>,
    pub cron_expr: Option<String>,
    // State
    pub next_due_at: Option<DateTime<Utc>>,
    pub status: String, // "pending" | "delivered" | "cancelled"
    pub created_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub last_delivered_at: Option<DateTime<Utc>>,
    pub delivered_count: i64,
}
