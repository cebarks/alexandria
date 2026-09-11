use crate::repos::MemoryRepo;
use anyhow::Result;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::SurrealValue;

#[derive(Debug, serde::Deserialize, SurrealValue)]
struct ConfigRow {
    value: String,
}

/// Read a system config value by key.
pub async fn get_config(db: &Surreal<Any>, key: &str) -> Result<Option<String>> {
    let mut result = db
        .query("SELECT * FROM system_config WHERE key = $key LIMIT 1")
        .bind(("key", key))
        .await?;
    let row: Option<ConfigRow> = result.take(0)?;
    Ok(row.map(|r| r.value))
}

/// Set a system config value (upsert).
pub async fn set_config(db: &Surreal<Any>, key: &str, value: &str) -> Result<()> {
    // Delete then create (upsert pattern)
    db.query("DELETE system_config WHERE key = $key")
        .bind(("key", key))
        .await?
        .check()?;
    db.query("CREATE system_config SET key = $key, value = $value, updated_at = time::now()")
        .bind(("key", key))
        .bind(("value", value))
        .await?
        .check()?;
    Ok(())
}

/// Check if the configured embedding model matches what's stored in the database.
/// On first boot, stores the current model info. On subsequent boots, compares.
///
/// Returns Ok(()) if safe to proceed, Err with a clear message if mismatched.
pub async fn check_embedding_model(
    db: &Surreal<Any>,
    model: &str,
    dimensions: usize,
) -> Result<()> {
    let stored_model = get_config(db, "embedding_model").await?;
    let stored_dims = get_config(db, "embedding_dimensions").await?;

    match (stored_model, stored_dims) {
        (None, _) | (_, None) => {
            // First boot — store the config. A database from before the lock existed
            // has facts but no lock; we can't verify which model produced them, so
            // warn rather than refuse (migrate-embeddings tells the user to boot once
            // to stamp the lock, so refusing here would leave no recovery path).
            let facts = MemoryRepo::new(db).count(None, None, true).await?;
            if facts > 0 {
                tracing::warn!(
                    "{facts} fact(s) exist but no embedding lock; assuming they were embedded                      with {model}. If not, run `alexandria migrate-embeddings` after fixing config."
                );
            }
            set_config(db, "embedding_model", model).await?;
            set_config(db, "embedding_dimensions", &dimensions.to_string()).await?;
            tracing::info!("Stored embedding config: model={model}, dimensions={dimensions}");
            Ok(())
        }
        (Some(stored_m), Some(stored_d)) => {
            if stored_m != model {
                anyhow::bail!(
                    "Embedding model mismatch!\n\
                     Stored: {stored_m}\n\
                     Configured: {model}\n\
                     \n\
                     The database contains embeddings from a different model.\n\
                     Mixing models produces garbage search results.\n\
                     \n\
                     Options:\n\
                     1. Change your config back to: {stored_m} (only if no migration has been attempted)\n\
                     2. Run `alexandria migrate-embeddings` with the server stopped to re-embed everything with {model}\n\
                     3. Delete the database and start fresh"
                );
            }
            let stored_dim: usize = stored_d.parse().unwrap_or(0);
            if stored_dim != dimensions {
                anyhow::bail!(
                    "Embedding dimensions mismatch!\n\
                     Stored: {stored_dim}\n\
                     Current: {dimensions}\n\
                     This likely means the model changed without updating system_config."
                );
            }
            tracing::debug!("Embedding model check passed: {model} ({dimensions} dims)");
            Ok(())
        }
    }
}
