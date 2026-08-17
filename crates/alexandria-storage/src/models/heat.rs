use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct HeatState {
    pub id: Option<RecordId>,
    pub memory: RecordId,
    pub heat: f64,
    pub stability: f64,
    pub last_touched: Option<DateTime<Utc>>,
    pub access_count: i64,
}
