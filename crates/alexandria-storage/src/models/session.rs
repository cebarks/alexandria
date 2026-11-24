use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Session {
    pub id: Option<RecordId>,
    pub external_id: String,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub summary: Option<String>,
    pub tags: Vec<String>,
}

/// A `session` row plus its live count of non-deleted memories, as returned by
/// `SessionRepo::list`.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct SessionListItem {
    pub id: Option<RecordId>,
    pub external_id: String,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub summary: Option<String>,
    pub tags: Vec<String>,
    pub memory_count: i64,
    /// Whether the row carries a summary — the one honest "was this session finalized" test, computed
    /// by the statement. `ScopedSessionRepo::list_scoped`'s scoped projection withholds `summary` and
    /// `tags` as prose, and a page that shows a finalized/active badge still needs the discriminator
    /// without the text: `ended_at` will not do, because `touch()` moves it on every attached memory.
    ///
    /// `Option`, not `bool` with a default: a reader that never projected the column (`SessionRepo`'s
    /// own list) must report "this read did not answer that" rather than a `false` that would read as
    /// "not finalized". A caller that has the prose does not need the flag at all.
    #[serde(default)]
    pub finalized: Option<bool>,
}
