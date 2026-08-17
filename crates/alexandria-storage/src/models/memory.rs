use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue, Value};

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Fact {
    pub id: Option<RecordId>,
    pub content: String,
    pub confidence: f64,
    pub embedding: Vec<f32>,
    pub tags: Vec<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub metadata: Option<Value>,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct RawRecord {
    pub id: Option<RecordId>,
    pub content: String,
    pub created_at: Option<DateTime<Utc>>,
    pub metadata: Option<Value>,
    pub deleted: bool,
}
