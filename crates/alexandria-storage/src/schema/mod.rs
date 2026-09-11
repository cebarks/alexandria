use anyhow::Result;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::SurrealValue;

/// All migrations in version order. Each is (version, name, SQL).
const MIGRATIONS: &[(u32, &str, &str)] = &[
    (1, "initial", include_str!("v001_initial.surql")),
    (2, "memory_edge", include_str!("v002_memory_edge.surql")),
    (3, "system_config", include_str!("v003_system_config.surql")),
    (
        4,
        "maintenance_log",
        include_str!("v004_maintenance_log.surql"),
    ),
    (5, "session", include_str!("v005_session.surql")),
    (
        6,
        "drop_session_memory_count",
        include_str!("v006_drop_session_memory_count.surql"),
    ),
];

/// Version a fully migrated database reports in `system_config.schema_version`.
pub const LATEST_VERSION: u32 = MIGRATIONS[MIGRATIONS.len() - 1].0;

/// Run all pending migrations. Safe to call on every startup.
///
/// On a fresh database (no system_config table), runs all migrations from v001.
/// On an existing database, reads the current version and runs only newer migrations.
pub async fn migrate(db: &Surreal<Any>) -> Result<()> {
    let current_version = get_current_version(db).await;

    let pending: Vec<_> = MIGRATIONS
        .iter()
        .filter(|(v, _, _)| *v > current_version)
        .collect();

    if pending.is_empty() {
        tracing::debug!("Schema up to date at v{current_version}");
        return Ok(());
    }

    for (version, name, sql) in &pending {
        tracing::info!("Running migration v{version:03}: {name}");
        db.query(*sql).await?.check()?;
    }

    // After v003 runs, system_config table exists. Store the version.
    let latest = pending.last().unwrap().0;
    set_version(db, latest).await?;

    tracing::info!("Schema migrated to v{latest:03}");
    Ok(())
}

/// Define the HNSW index over `fact.embedding` if it is missing. Not a numbered
/// migration: HNSW needs `DIMENSION` at define time, and the dimension is a
/// property of the embedding model locked on first boot, so this runs at boot
/// after the model check with the verified dimension.
pub async fn ensure_vector_index(db: &Surreal<Any>, dimensions: usize) -> Result<()> {
    db.query(format!(
        "DEFINE INDEX IF NOT EXISTS fact_embedding_hnsw ON fact FIELDS embedding \
         HNSW DIMENSION {dimensions} DISTANCE COSINE"
    ))
    .await?
    .check()?;
    Ok(())
}

/// Drop the HNSW index. Re-embedding to a model with a different dimension has
/// to remove it first, since the index rejects vectors of any other size.
pub async fn drop_vector_index(db: &Surreal<Any>) -> Result<()> {
    db.query("REMOVE INDEX IF EXISTS fact_embedding_hnsw ON fact")
        .await?
        .check()?;
    Ok(())
}

/// Backwards-compatible bootstrap that runs all migrations.
/// Existing tests and code that call `schema::bootstrap()` still work.
pub async fn bootstrap(db: &Surreal<Any>) -> Result<()> {
    migrate(db).await
}

/// Get the current schema version from system_config, or 0 if not yet tracked.
async fn get_current_version(db: &Surreal<Any>) -> u32 {
    // Try to read the schema_version key. If the table doesn't exist yet, this
    // returns an error or empty result — either way, version is 0.
    let result: Result<Option<SystemConfigRow>, _> = db
        .query("SELECT * FROM system_config WHERE key = 'schema_version' LIMIT 1")
        .await
        .and_then(|mut r| r.take(0));

    match result {
        Ok(Some(row)) => row.value.parse().unwrap_or(0),
        _ => 0,
    }
}

/// Store the current schema version in system_config.
async fn set_version(db: &Surreal<Any>, version: u32) -> Result<()> {
    db.query(
        "DELETE FROM system_config WHERE key = 'schema_version'; \
         CREATE system_config SET key = 'schema_version', value = $version, updated_at = time::now();"
    )
    .bind(("version", version.to_string()))
    .await?
    .check()?;
    Ok(())
}

#[derive(Debug, serde::Deserialize, surrealdb::types::SurrealValue)]
struct SystemConfigRow {
    value: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Session;

    /// Rows written before v006 carry a memory_count value; the migration must
    /// clear it and the field-less Session struct must still read them back.
    #[tokio::test]
    async fn v006_clears_legacy_memory_count() {
        let db = crate::connection::Database::connect_embedded()
            .await
            .unwrap();
        let db = db.inner();

        for (_, _, sql) in MIGRATIONS.iter().filter(|(v, _, _)| *v <= 5) {
            db.query(*sql).await.unwrap().check().unwrap();
        }
        set_version(db, 5).await.unwrap();
        db.query("CREATE `session` SET external_id = 'legacy', memory_count = 3, tags = []")
            .await
            .unwrap()
            .check()
            .unwrap();

        migrate(db).await.unwrap();

        let rows: Vec<Session> = db
            .query("SELECT * FROM `session`")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].external_id, "legacy");
        let leftover: Option<serde_json::Value> = db
            .query("SELECT VALUE memory_count FROM ONLY `session` WHERE external_id = 'legacy' LIMIT 1")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert!(leftover.is_none() || leftover == Some(serde_json::Value::Null));
    }

    /// The index is defined at boot (dimensions are per-deployment), so defining
    /// it twice must be a no-op and the KNN query must go through it.
    #[tokio::test]
    async fn vector_index_is_idempotent_and_serves_knn() {
        let db = crate::connection::Database::connect_embedded()
            .await
            .unwrap();
        let db = db.inner();
        migrate(db).await.unwrap();
        let memories = crate::repos::MemoryRepo::new(db);
        let near = memories
            .create_fact("near", 0.5, &[1.0, 0.0], &[])
            .await
            .unwrap();
        memories
            .create_fact("far", 0.5, &[0.0, 1.0], &[])
            .await
            .unwrap();

        ensure_vector_index(db, 2).await.unwrap();
        ensure_vector_index(db, 2).await.unwrap();

        let rows: Vec<crate::models::Fact> = db
            .query("SELECT * FROM fact WHERE deleted = false AND embedding <|1,COSINE|> $q")
            .bind(("q", vec![0.9f32, 0.1]))
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            crate::record_id_to_string(rows[0].id.as_ref().unwrap()),
            near
        );

        drop_vector_index(db).await.unwrap();
        drop_vector_index(db).await.unwrap();
        // A 3-dim vector is only writable once the 2-dim index is gone.
        memories
            .create_fact("wider", 0.5, &[0.0, 0.0, 1.0], &[])
            .await
            .unwrap();
    }
}
