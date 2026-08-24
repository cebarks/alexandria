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
}
