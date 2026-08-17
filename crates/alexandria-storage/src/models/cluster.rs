use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Cluster {
    pub id: Option<RecordId>,
    pub label: Option<String>,
    pub centroid: Vec<f32>,
    pub depth: i64,
    pub created_at: Option<DateTime<Utc>>,
}
