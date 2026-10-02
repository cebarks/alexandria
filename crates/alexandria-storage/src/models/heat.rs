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
    /// Last time a caller actually saw the row — the spacing reference the heat model grows
    /// stability from, and deliberately not the same field as `last_touched`, which every
    /// materialisation moves. `NONE` means the row predates the column.
    pub last_accessed_at: Option<DateTime<Utc>>,
    pub access_count: i64,
}
