use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

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
    pub schedule_kind: String, // "once" | "pattern" | "cron"
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
