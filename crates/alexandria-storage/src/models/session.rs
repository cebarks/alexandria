use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Session {
    pub id: Option<RecordId>,
    pub external_id: String,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub summary: Option<String>,
    pub memory_count: i64,
    pub tags: Vec<String>,
}
