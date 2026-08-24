mod connection;
pub mod schema;
pub mod models;
pub mod repos;
pub mod stats;
pub mod system_config;

pub use connection::Database;

use surrealdb::types::{RecordId, RecordIdKey};

/// Helper to convert RecordId to `table:key` string format for SurrealQL.
pub fn record_id_to_string(id: &RecordId) -> String {
    let table = id.table.as_str();
    let key = match &id.key {
        RecordIdKey::String(s) => s.clone(),
        RecordIdKey::Number(n) => n.to_string(),
        RecordIdKey::Uuid(u) => format!("{u}"),
        other => format!("{other:?}"),
    };
    format!("{table}:{key}")
}
