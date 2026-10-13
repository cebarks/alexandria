use crate::models::embedding::{EmbeddingSpace, GRANITE_MODEL_ID};
use crate::repos::{FactListQuery, MemoryRepo};
use anyhow::Result;
use chrono::{DateTime, Utc};
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

/// The instant the retrieve path began recording accesses, stored so it survives restarts.
///
/// `Appraise` reads this before it lowers any confidence. `access_count == 0` does not mean "nobody
/// ever retrieved this memory" for a row written before recording existed — it means nobody was
/// counting — and on a corpus that predates the feature those two states are every row.
pub const ACCESS_RECORDING_ARMED_AT: &str = "access_recording_armed_at";

/// Record the arming instant on first boot, or return the stamp already stored.
///
/// Written at boot rather than at first appraisal so the value names when the build started
/// recording, not when a daily job happened to run.
pub async fn arm_access_recording(db: &Surreal<Any>) -> Result<DateTime<Utc>> {
    if let Some(armed) = access_recording_armed_at(db).await? {
        return Ok(armed);
    }
    let now = Utc::now();
    set_config(db, ACCESS_RECORDING_ARMED_AT, &now.to_rfc3339()).await?;
    Ok(now)
}

/// The arming stamp, or `None` when no boot of this build has armed it.
///
/// A corrupt value is an error rather than a silent `None`: the reader's fail-closed behaviour means
/// a typo in this key would stop appraise for the rest of the store's life with no signal at all.
pub async fn access_recording_armed_at(db: &Surreal<Any>) -> Result<Option<DateTime<Utc>>> {
    let Some(raw) = get_config(db, ACCESS_RECORDING_ARMED_AT).await? else {
        return Ok(None);
    };
    let parsed = DateTime::parse_from_rfc3339(&raw)
        .map_err(|e| anyhow::anyhow!("system_config `{ACCESS_RECORDING_ARMED_AT}` holds `{raw}`, which is not an RFC3339 datetime: {e}"))?;
    Ok(Some(parsed.with_timezone(&Utc)))
}

/// Token limit a corpus was embedded at when its lock predates `embedding_max_tokens`:
/// the tokenizer's shipped default, which was applied unchanged until the key existed.
pub const PRE_LOCK_MAX_TOKENS: usize = 128;

/// Stored `embedding_max_tokens`, or [`PRE_LOCK_MAX_TOKENS`] when the key is absent.
pub async fn stored_max_tokens(db: &Surreal<Any>) -> Result<usize> {
    Ok(get_config(db, "embedding_max_tokens")
        .await?
        .and_then(|t| t.parse().ok())
        .unwrap_or(PRE_LOCK_MAX_TOKENS))
}

/// Read-only startup admission before schema/heat writes or provider I/O. Fresh
/// databases may have no config table; pending/corrupt journals never get there.
/// The ready provider still checks the full identity before index creation.
pub async fn check_boot_embedding_metadata(
    db: &Surreal<Any>,
    model: &str,
    max_tokens: usize,
) -> Result<()> {
    let mut response = db.query("INFO FOR DB STRUCTURE").await?;
    let info: Option<serde_json::Value> = response.take(0)?;
    let info = info.ok_or_else(|| anyhow::anyhow!("Missing database table inventory"))?;
    let tables = info["tables"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Invalid database table inventory"))?;
    let contains = |name: &str| tables.iter().any(|table| table["name"] == name);
    let stored = if contains("system_config") {
        stored_embedding_space(db).await?
    } else {
        None
    };
    if let Some(space) = stored {
        anyhow::ensure!(
            space.model() == model && space.max_tokens() == max_tokens,
            "Database is locked to model={} at {} tokens; requested model={model} at {max_tokens}. Choose the matching model/max_tokens and original directory together, or migrate-embeddings --output-data-dir PATH into a separate offline copy; never relabel this store",
            space.model(),
            space.max_tokens()
        );
    } else if model == GRANITE_MODEL_ID {
        let facts = if contains("fact") {
            MemoryRepo::new(db)
                .count(&FactListQuery {
                    include_deleted: true,
                    include_quarantined: true,
                    ..Default::default()
                })
                .await?
        } else {
            0
        };
        let centroids = if contains("cluster") {
            !crate::repos::ClusterRepo::new(db).list().await?.is_empty()
        } else {
            false
        };
        anyhow::ensure!(
            facts == 0 && !centroids,
            "Existing facts/centroids have no verified embedding identity. Resolve the original model first, then use migrate-embeddings --output-data-dir PATH; do not relabel them Granite"
        );
    }
    Ok(())
}

/// Read the stored vector space without inventing legacy provenance. A fresh store
/// with no identity table returns None without bootstrapping schema.
pub async fn stored_embedding_space(db: &Surreal<Any>) -> Result<Option<EmbeddingSpace>> {
    let mut response = db.query("INFO FOR DB STRUCTURE").await?;
    let info: Option<serde_json::Value> = response.take(0)?;
    let info = info.ok_or_else(|| anyhow::anyhow!("Missing database table inventory"))?;
    let tables = info["tables"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Invalid database table inventory"))?;
    if !tables.iter().any(|table| table["name"] == "system_config") {
        return Ok(None);
    }
    crate::embedding_state::ensure_ready(db).await?;
    if let Some(encoded) = get_config(db, "embedding_space").await? {
        let space: EmbeddingSpace = serde_json::from_str(&encoded)?;
        space.validate()?;
        check_compatible_embedding_keys(db, &space).await?;
        return Ok(Some(space));
    }
    let model = get_config(db, "embedding_model").await?;
    let dimensions = get_config(db, "embedding_dimensions").await?;
    let tokens = get_config(db, "embedding_max_tokens").await?;
    match (model, dimensions) {
        (None, None) => {
            anyhow::ensure!(tokens.is_none(), "Incomplete legacy embedding lock");
            Ok(None)
        }
        (Some(model), Some(dimensions)) => {
            let space = EmbeddingSpace::Legacy {
                model,
                dimensions: dimensions.parse()?,
                max_tokens: match tokens {
                    Some(tokens) => tokens.parse()?,
                    None => PRE_LOCK_MAX_TOKENS,
                },
            };
            space.validate()?;
            Ok(Some(space))
        }
        _ => anyhow::bail!("Incomplete legacy embedding lock; offline migration is required"),
    }
}

async fn check_compatible_embedding_keys(db: &Surreal<Any>, space: &EmbeddingSpace) -> Result<()> {
    for (key, expected) in [
        ("embedding_model", space.model().to_owned()),
        ("embedding_dimensions", space.dimensions().to_string()),
        ("embedding_max_tokens", space.max_tokens().to_string()),
    ] {
        anyhow::ensure!(
            get_config(db, key).await?.as_deref() == Some(expected.as_str()),
            "Stored embedding identity disagrees with compatibility key `{key}`"
        );
    }
    Ok(())
}

/// Check a complete vector-space identity before any serving/index write.
pub async fn check_embedding_space(db: &Surreal<Any>, requested: &EmbeddingSpace) -> Result<()> {
    requested.validate()?;
    if let Some(stored) = stored_embedding_space(db).await? {
        anyhow::ensure!(
            stored == *requested,
            "Embedding space mismatch; use a separate offline migrated copy"
        );
        return Ok(());
    }
    if let EmbeddingSpace::Legacy {
        model,
        dimensions,
        max_tokens,
    } = requested
    {
        return check_embedding_model(db, model, *dimensions, *max_tokens).await;
    }
    let facts = MemoryRepo::new(db)
        .count(&FactListQuery {
            include_deleted: true,
            include_quarantined: true,
            ..Default::default()
        })
        .await?;
    anyhow::ensure!(
        facts == 0,
        "Existing facts have no verified embedding identity; use a separate offline migrated copy"
    );
    let mut clusters = db.query("SELECT * FROM cluster LIMIT 1").await?;
    let clusters: Vec<crate::models::Cluster> = clusters.take(0)?;
    anyhow::ensure!(
        clusters.is_empty(),
        "Existing centroids have no verified embedding identity; use a separate offline migrated copy"
    );
    stamp_embedding_space(db, requested).await
}

/// Admission for the old in-place command. Owned copies use their journal;
/// Granite sources and targets never pass through this legacy path.
#[derive(Debug, PartialEq, Eq)]
pub struct LegacyReembedSource {
    model: Option<String>,
    dimensions: Option<String>,
    max_tokens: Option<String>,
    space: Option<String>,
}

pub async fn legacy_reembed_source(db: &Surreal<Any>) -> Result<LegacyReembedSource> {
    let mut response = db.query("INFO FOR DB STRUCTURE").await?;
    let info: Option<serde_json::Value> = response.take(0)?;
    let info = info.ok_or_else(|| anyhow::anyhow!("Missing database table inventory"))?;
    let tables = info["tables"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Invalid database table inventory"))?;
    if !tables.iter().any(|table| table["name"] == "system_config") {
        return Ok(LegacyReembedSource {
            model: None,
            dimensions: None,
            max_tokens: None,
            space: None,
        });
    }
    anyhow::ensure!(
        crate::embedding_state::read_target_journal(db)
            .await?
            .is_none(),
        "An owned migration target cannot use the legacy in-place command; use --resume or a new --output-data-dir"
    );
    let source = LegacyReembedSource {
        model: get_config(db, "embedding_model").await?,
        dimensions: get_config(db, "embedding_dimensions").await?,
        max_tokens: get_config(db, "embedding_max_tokens").await?,
        space: get_config(db, "embedding_space").await?,
    };
    anyhow::ensure!(
        source.model.as_deref() != Some(GRANITE_MODEL_ID),
        "Granite source requires a separate offline copy with --output-data-dir"
    );
    if source.space.is_some() {
        anyhow::ensure!(
            matches!(
                stored_embedding_space(db).await?,
                Some(EmbeddingSpace::Legacy { .. })
            ),
            "Legacy migration requires a verified legacy identity"
        );
    }
    // Older model-only locks can be repaired by this explicit legacy operation.
    // Preserve their raw keys for the final comparison; never invent dimensions.
    Ok(source)
}

/// The legacy caller must hold exclusive writer access and has already rewritten
/// its vectors. Preserve a coherent full identity as well as compatibility keys.
/// This makes metadata publication atomic, not the preceding legacy vector pass.
pub async fn publish_legacy_reembed_space(
    db: &Surreal<Any>,
    source: &LegacyReembedSource,
    target: &EmbeddingSpace,
) -> Result<()> {
    target.validate()?;
    anyhow::ensure!(
        matches!(target, EmbeddingSpace::Legacy { .. }),
        "Legacy publication cannot label Granite"
    );
    anyhow::ensure!(
        &legacy_reembed_source(db).await? == source,
        "Legacy migration source identity changed"
    );
    stamp_embedding_space(db, target).await
}

async fn stamp_embedding_space(db: &Surreal<Any>, space: &EmbeddingSpace) -> Result<()> {
    db.query(
        "BEGIN TRANSACTION;
         DELETE system_config WHERE key IN [
             'embedding_space', 'embedding_model', 'embedding_dimensions', 'embedding_max_tokens'
         ];
         CREATE system_config SET key = 'embedding_space', value = $space, updated_at = time::now();
         CREATE system_config SET key = 'embedding_model', value = $model, updated_at = time::now();
         CREATE system_config SET key = 'embedding_dimensions', value = $dims, updated_at = time::now();
         CREATE system_config SET key = 'embedding_max_tokens', value = $tokens, updated_at = time::now();
         COMMIT TRANSACTION;",
    )
    .bind(("space", serde_json::to_string(space)?))
    .bind(("model", space.model().to_owned()))
    .bind(("dims", space.dimensions().to_string()))
    .bind(("tokens", space.max_tokens().to_string()))
    .await?
    .check()?;
    Ok(())
}

/// Check a legacy embedding model against the database lock.
/// On first boot, stores the current model info. On subsequent boots, compares.
/// Granite requires `check_embedding_space`; an existing full identity is never bypassed.
///
/// Returns Ok(()) if safe to proceed, Err with a clear message if mismatched.
pub async fn check_embedding_model(
    db: &Surreal<Any>,
    model: &str,
    dimensions: usize,
    max_tokens: usize,
) -> Result<()> {
    crate::embedding_state::ensure_ready(db).await?;
    anyhow::ensure!(
        model != GRANITE_MODEL_ID,
        "Granite requires a complete versioned embedding identity; use check_embedding_space"
    );
    if get_config(db, "embedding_space").await?.is_some() {
        let requested = EmbeddingSpace::Legacy {
            model: model.to_owned(),
            dimensions,
            max_tokens,
        };
        anyhow::ensure!(
            stored_embedding_space(db).await?.as_ref() == Some(&requested),
            "Embedding space mismatch; use a separate offline migrated copy"
        );
        return Ok(());
    }
    let stored_model = get_config(db, "embedding_model").await?;
    let stored_dims = get_config(db, "embedding_dimensions").await?;

    let (stored_m, stored_d) = match (stored_model, stored_dims) {
        (Some(m), Some(d)) => (m, d),
        _ => {
            // First boot: store the config. A database from before the lock existed has facts
            // but no lock. Which model produced them cannot be verified, so that is assumed
            // (migrate-embeddings tells the user to boot once to stamp the lock). The token
            // limit is not assumed: those facts were embedded at the tokenizer's shipped
            // default, so that is what gets stamped, and the comparison below refuses a
            // configured limit that differs.
            // Counts every row, deleted and quarantined included: the question is whether the
            // corpus predates the lock at all, and a soft-deleted or quarantined fact is still a
            // fact whose vectors were made by some model at some token limit.
            let facts = MemoryRepo::new(db)
                .count(&FactListQuery {
                    include_deleted: true,
                    include_quarantined: true,
                    ..Default::default()
                })
                .await?;
            let stamp = if facts > 0 {
                tracing::warn!(
                    "{facts} fact(s) exist but no embedding lock; assuming they were embedded \
                     with {model} at {PRE_LOCK_MAX_TOKENS} tokens. If the model is wrong, run \
                     `alexandria migrate-embeddings` after fixing config."
                );
                PRE_LOCK_MAX_TOKENS
            } else {
                max_tokens
            };
            set_config(db, "embedding_model", model).await?;
            set_config(db, "embedding_dimensions", &dimensions.to_string()).await?;
            set_config(db, "embedding_max_tokens", &stamp.to_string()).await?;
            tracing::info!(
                "Stored embedding config: model={model}, dimensions={dimensions}, max_tokens={stamp}"
            );
            (model.to_string(), dimensions.to_string())
        }
    };

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
    let stored_t = stored_max_tokens(db).await?;
    if max_tokens > stored_t {
        anyhow::bail!(
            "Embedding token limit mismatch!\n\
             Stored: {stored_t}\n\
             Configured: {max_tokens} (embedding.max_tokens)\n\
             \n\
             Facts longer than {stored_t} tokens were embedded on a prefix.\n\
             Run `alexandria migrate-embeddings` with the server stopped to re-embed everything at \
             {max_tokens} tokens, or set embedding.max_tokens = {stored_t} to keep the corpus as it is."
        );
    }
    if max_tokens < stored_t {
        anyhow::bail!(
            "Embedding token limit mismatch!\n\
             Stored: {stored_t}\n\
             Configured: {max_tokens} (embedding.max_tokens)\n\
             \n\
             The corpus was embedded at {stored_t} tokens and `migrate-embeddings` does not lower \
             the limit. Set embedding.max_tokens = {stored_t}."
        );
    }
    tracing::debug!(
        "Embedding model check passed: {model} ({dimensions} dims, {max_tokens} tokens)"
    );
    Ok(())
}
