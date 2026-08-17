use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct MemoryEdge {
    pub id: Option<RecordId>,
    #[serde(rename = "in")]
    pub in_node: Option<RecordId>,
    #[serde(rename = "out")]
    pub out_node: Option<RecordId>,
    pub edge_type: String,
    pub strength: f64,
    pub created_at: Option<DateTime<Utc>>,
}
