use anyhow::Result;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::SurrealValue;

/// All migrations in version order. Each is (version, name, SQL).
const MIGRATIONS: &[(u32, &str, &str)] = &[
    (1, "initial", include_str!("v001_initial.surql")),
    (2, "memory_edge", include_str!("v002_memory_edge.surql")),
    (3, "system_config", include_str!("v003_system_config.surql")),
];

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
