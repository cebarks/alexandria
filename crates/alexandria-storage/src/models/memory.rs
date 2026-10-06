use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue, Value};

/// The columns `Collapse` needs from a live fact, and nothing else.
///
/// A projection rather than `Vec<Fact>`: duplicate grouping compares content strings, so reading
/// every embedding in the corpus to find byte-identical rows would make the embedding the dominant
/// cost of a job that never looks at it.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct CollapseCandidate {
    pub id: RecordId,
    pub content: String,
    pub confidence: f64,
    pub created_at: Option<DateTime<Utc>>,
}

/// The columns `Appraise` needs from a live fact, and nothing else.
///
/// `created_at` is not decoration: it is what tells a memory that was stored under access recording
/// from one whose `access_count == 0` merely means nobody was counting. Confidence is the value
/// demotion writes, so it is read fresh rather than carried in from an earlier query.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct LiveConfidence {
    pub id: RecordId,
    pub confidence: f64,
    pub created_at: Option<DateTime<Utc>>,
}

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
