use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

// NOTE: `#[serde(rename = "...")]` alone is not enough here — the `SurrealValue` derive macro
// (which performs the actual DB row deserialization) reads its own `#[surreal(rename = "...")]`
// attribute, not serde's. Without it, `in_node`/`out_node` silently deserialize as `None` even
// though the underlying `memory_edge` row has valid `in`/`out` RecordId fields.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct MemoryEdge {
    pub id: Option<RecordId>,
    #[serde(rename = "in")]
    #[surreal(rename = "in")]
    pub in_node: Option<RecordId>,
    #[serde(rename = "out")]
    #[surreal(rename = "out")]
    pub out_node: Option<RecordId>,
    pub edge_type: String,
    pub strength: f64,
    pub created_at: Option<DateTime<Utc>>,
}
