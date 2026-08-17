use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue, Value};

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Provenance {
    pub id: Option<RecordId>,
    pub kind: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub metadata: Option<Value>,
}
