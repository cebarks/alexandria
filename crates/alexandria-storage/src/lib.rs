pub mod access;
mod connection;
pub mod embedding_state;
pub mod embedding_validation;
pub mod error;
pub mod models;
pub mod offline_copy;
pub mod project_handle;
pub mod repos;
pub mod schema;
pub mod stats;
pub mod system_config;

pub use connection::Database;
/// The typed record id, re-exported so downstream crates can carry storage's typed values
/// without depending on the engine crate directly — the boundary rule is that the binary names
/// ids through storage, not through `surrealdb`.
pub use surrealdb::types::RecordId;

use surrealdb::types::RecordIdKey;

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
