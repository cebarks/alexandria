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
    /// The middle rung of the dreaming ladder: hidden from every live read path, still
    /// inspectable by an operator. `NONE` means not quarantined, and the live predicate is
    /// `quarantined_at = NONE` — `IS NOT NULL` and `!= NULL` are both satisfied by `NONE` on this
    /// engine and would filter nothing.
    pub quarantined_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct RawRecord {
    pub id: Option<RecordId>,
    pub content: String,
    pub created_at: Option<DateTime<Utc>>,
    pub metadata: Option<Value>,
    pub deleted: bool,
}
