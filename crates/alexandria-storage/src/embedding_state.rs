//! Atomic lifecycle metadata for an owned, separate migration target.
//! These APIs do not copy/open a source or rewrite vectors. The rewriter validates
//! corpus/centroid/metadata survival before publication; publication additionally
//! proves the target HNSW plan and commits readiness with the exact journal.

use anyhow::{Context, Result, ensure};
use surrealdb::{
    Surreal,
    engine::any::Any,
    types::{SurrealValue, ToSql},
};

use crate::{
    models::embedding::{EmbeddingSpace, MigrationPhase, ReembedJournal},
    system_config::{self, PRE_LOCK_MAX_TOKENS},
};

pub const INCOMPLETE_MODEL: &str = "alexandria:embedding-migration-incomplete";
pub const JOURNAL_KEY: &str = "embedding_migration_journal";

const GUARDED_TRANSACTION: &str = "
BEGIN TRANSACTION;
IF (SELECT * FROM system_config WHERE key = 'embedding_migration_journal' LIMIT 1)[0].value != $seen.journal {
    THROW 'Migration journal changed';
};
IF (SELECT * FROM system_config WHERE key = 'embedding_model' LIMIT 1)[0].value != $seen.model
    OR (SELECT * FROM system_config WHERE key = 'embedding_dimensions' LIMIT 1)[0].value != $seen.dimensions
    OR (SELECT * FROM system_config WHERE key = 'embedding_max_tokens' LIMIT 1)[0].value != $seen.tokens
    OR (SELECT * FROM system_config WHERE key = 'embedding_space' LIMIT 1)[0].value != $seen.space {
    THROW 'Migration embedding lock changed';
};
";

#[derive(SurrealValue)]
struct LockSnapshot {
    model: Option<String>,
    dimensions: Option<String>,
    tokens: Option<String>,
    space: Option<String>,
    journal: Option<String>,
}

impl LockSnapshot {
    async fn load(db: &Surreal<Any>) -> Result<Self> {
        let (model, dimensions, tokens, space, journal) = tokio::try_join!(
            system_config::get_config(db, "embedding_model"),
            system_config::get_config(db, "embedding_dimensions"),
            system_config::get_config(db, "embedding_max_tokens"),
            system_config::get_config(db, "embedding_space"),
            system_config::get_config(db, JOURNAL_KEY),
        )?;
        Ok(Self {
            model,
            dimensions,
            tokens,
            space,
            journal,
        })
    }

    fn source_matches(&self, source: &EmbeddingSpace, blocked: bool) -> Result<()> {
        let model = if blocked {
            INCOMPLETE_MODEL
        } else {
            source.model()
        };
        ensure!(
            self.model.as_deref() == Some(model),
            "Migration source model/marker mismatch"
        );
        ensure!(
            self.dimensions.as_deref() == Some(source.dimensions().to_string().as_str()),
            "Migration source dimensions mismatch"
        );
        let tokens = self
            .tokens
            .as_ref()
            .map(|tokens| tokens.parse::<usize>())
            .transpose()?
            .unwrap_or(PRE_LOCK_MAX_TOKENS);
        ensure!(
            tokens == source.max_tokens(),
            "Migration source token limit mismatch"
        );
        if let Some(encoded) = &self.space {
            let stored: EmbeddingSpace = serde_json::from_str(encoded)?;
            stored.validate()?;
            ensure!(stored == *source, "Migration source full identity mismatch");
        } else {
            ensure!(
                matches!(source, EmbeddingSpace::Legacy { .. }),
                "Granite source has no full identity"
            );
        }
        Ok(())
    }
}

fn decode_journal(encoded: &str) -> Result<ReembedJournal> {
    let journal: ReembedJournal =
        serde_json::from_str(encoded).context("Malformed migration journal")?;
    journal.validate().context("Invalid migration journal")?;
    Ok(journal)
}

/// Reads owned progress even while normal serving is deliberately blocked.
pub async fn read_target_journal(db: &Surreal<Any>) -> Result<Option<ReembedJournal>> {
    system_config::get_config(db, JOURNAL_KEY)
        .await?
        .map(|encoded| decode_journal(&encoded))
        .transpose()
}

/// A corrupt/pending journal is never ignored, even if old compatibility keys were repaired.
pub(crate) async fn ensure_ready(db: &Surreal<Any>) -> Result<()> {
    if let Some(journal) = read_target_journal(db).await? {
        ensure!(
            journal.phase == MigrationPhase::Complete,
            "Embedding migration incomplete; resume the owned separate target, not the source"
        );
        let encoded = system_config::get_config(db, "embedding_space")
            .await?
            .context("Completed migration has no full embedding identity")?;
        let space: EmbeddingSpace = serde_json::from_str(&encoded)?;
        space.validate()?;
        ensure!(
            space == journal.target_space,
            "Completed migration identity disagrees with journal"
        );
    }
    ensure!(
        system_config::get_config(db, "embedding_model")
            .await?
            .as_deref()
            != Some(INCOMPLETE_MODEL),
        "Embedding migration incomplete; resume the owned separate target"
    );
    Ok(())
}

/// Atomically block serving and attach fresh ownership before any vector change.
/// A completed journal copied from a previous source migration may be replaced;
/// an incomplete journal must be resumed, never overwritten by a new run.
pub async fn begin_target_reembed(db: &Surreal<Any>, journal: &ReembedJournal) -> Result<()> {
    journal.validate()?;
    ensure!(
        journal.phase == MigrationPhase::Facts
            && journal.last_fact_id.is_none()
            && journal.reembedded_count == 0,
        "Migration must begin at an empty Facts checkpoint"
    );
    ensure_ready(db).await?;
    let seen = LockSnapshot::load(db).await?;
    seen.source_matches(&journal.source_space, false)?;
    if let Some(encoded) = &seen.journal {
        let prior = decode_journal(encoded)?;
        ensure!(
            prior.phase == MigrationPhase::Complete
                && prior.target_space == journal.source_space
                && prior.run_id != journal.run_id,
            "Cannot replace an existing migration run"
        );
    }
    let sql = format!(
        "{GUARDED_TRANSACTION}
        DELETE system_config WHERE key IN ['embedding_model', 'embedding_migration_journal'];
        CREATE system_config SET key = 'embedding_migration_journal', value = $journal;
        CREATE system_config SET key = 'embedding_model', value = $marker;
        COMMIT TRANSACTION;"
    );
    db.query(sql)
        .bind(("seen", seen))
        .bind(("journal", serde_json::to_string(journal)?))
        .bind(("marker", INCOMPLETE_MODEL))
        .await?
        .check()?;
    Ok(())
}

fn check_progress(previous: &ReembedJournal, next: &ReembedJournal) -> Result<()> {
    next.validate()?;
    ensure!(
        previous.run_id == next.run_id
            && previous.source_manifest_sha256 == next.source_manifest_sha256
            && previous.source_space == next.source_space
            && previous.target_space == next.target_space,
        "Migration checkpoint ownership/space mismatch"
    );
    ensure!(
        next.phase != MigrationPhase::Complete && previous.phase != MigrationPhase::Complete,
        "Only final publication may mark a migration Complete"
    );
    let phase_ok = previous.phase == next.phase
        || matches!(
            (previous.phase, next.phase),
            (MigrationPhase::Facts, MigrationPhase::Centroids)
                | (MigrationPhase::Centroids, MigrationPhase::Verify)
        );
    ensure!(
        phase_ok,
        "Migration phase must advance one step, never regress"
    );
    if previous.phase != MigrationPhase::Facts {
        ensure!(
            previous.last_fact_id == next.last_fact_id
                && previous.reembedded_count == next.reembedded_count,
            "Fact progress cannot change after the Facts phase"
        );
    }
    ensure!(
        next.reembedded_count >= previous.reembedded_count
            && next.last_fact_id >= previous.last_fact_id,
        "Migration fact checkpoint cannot regress"
    );
    ensure!(
        (next.reembedded_count == previous.reembedded_count)
            == (next.last_fact_id == previous.last_fact_id),
        "Migration cursor/count must advance together"
    );
    Ok(())
}

/// Persist only fully written groups. Uncheckpointed partial groups are replayed.
pub async fn checkpoint_target_reembed(db: &Surreal<Any>, journal: &ReembedJournal) -> Result<()> {
    let seen = LockSnapshot::load(db).await?;
    let previous = decode_journal(
        seen.journal
            .as_deref()
            .context("Target has no migration journal")?,
    )?;
    check_progress(&previous, journal)?;
    seen.source_matches(&journal.source_space, true)?;
    let sql = format!(
        "{GUARDED_TRANSACTION}
        DELETE system_config WHERE key = 'embedding_migration_journal';
        CREATE system_config SET key = 'embedding_migration_journal', value = $journal;
        COMMIT TRANSACTION;"
    );
    db.query(sql)
        .bind(("seen", seen))
        .bind(("journal", serde_json::to_string(journal)?))
        .await?
        .check()?;
    Ok(())
}

async fn require_hnsw_plan(db: &Surreal<Any>, dimensions: usize) -> Result<()> {
    let mut result = db.query("INFO FOR TABLE fact STRUCTURE").await?;
    let info: Option<serde_json::Value> = result.take(0)?;
    let info = info.context("Missing fact table information")?;
    let index = info["indexes"]
        .as_array()
        .and_then(|indexes| {
            indexes
                .iter()
                .find(|index| index["name"] == "fact_embedding_hnsw")
        })
        .context("Migration target has no HNSW index definition")?;
    // STRUCTURE returns canonical index parameters as SQL, not a typed HNSW object.
    let definition = index["index"]
        .as_str()
        .context("Invalid HNSW index definition")?;
    let words: Vec<&str> = definition.split_whitespace().collect();
    ensure!(
        words.first() == Some(&"HNSW")
            && words
                .windows(2)
                .any(|pair| pair[0] == "DIMENSION"
                    && pair[1].parse::<usize>().ok() == Some(dimensions))
            && words.windows(2).any(|pair| pair == ["DIST", "COSINE"]),
        "Migration HNSW index has the wrong dimension or distance: {definition}"
    );
    let mut result = db
        .query("EXPLAIN FORMAT JSON SELECT * FROM fact WHERE embedding <|1,150|> $q")
        .bind(("q", vec![0.0_f32; dimensions]))
        .await?;
    let plan: surrealdb::types::Value = result.take(0)?;
    let plan = plan.to_sql();
    ensure!(
        plan.contains("KnnScan") && plan.contains("fact_embedding_hnsw"),
        "Migration target has no verified HNSW index plan"
    );
    Ok(())
}

/// After corpus verification, prove the HNSW path and publish all readiness keys
/// with `Complete` in one transaction. A failure retains the prior blocked state.
pub async fn publish_target_space(
    db: &Surreal<Any>,
    expected: &ReembedJournal,
    space: &EmbeddingSpace,
) -> Result<()> {
    expected.validate()?;
    space.validate()?;
    ensure!(
        expected.phase == MigrationPhase::Verify,
        "Only a Verify checkpoint can be published"
    );
    ensure!(
        *space == expected.target_space,
        "Published space differs from owned target"
    );
    let seen = LockSnapshot::load(db).await?;
    let stored = decode_journal(
        seen.journal
            .as_deref()
            .context("Target has no migration journal")?,
    )?;
    ensure!(
        stored == *expected,
        "Publication journal is stale or belongs to another run"
    );
    seen.source_matches(&expected.source_space, true)?;
    require_hnsw_plan(db, space.dimensions()).await?;
    let mut complete = expected.clone();
    complete.phase = MigrationPhase::Complete;
    let sql = format!("{GUARDED_TRANSACTION}
        DELETE system_config WHERE key IN [
            'embedding_space', 'embedding_model', 'embedding_dimensions', 'embedding_max_tokens', 'embedding_migration_journal'
        ];
        CREATE system_config SET key = 'embedding_space', value = $space;
        CREATE system_config SET key = 'embedding_model', value = $model;
        CREATE system_config SET key = 'embedding_dimensions', value = $dimensions;
        CREATE system_config SET key = 'embedding_max_tokens', value = $tokens;
        CREATE system_config SET key = 'embedding_migration_journal', value = $journal;
        COMMIT TRANSACTION;");
    db.query(sql)
        .bind(("seen", seen))
        .bind(("space", serde_json::to_string(space)?))
        .bind(("model", space.model().to_owned()))
        .bind(("dimensions", space.dimensions().to_string()))
        .bind(("tokens", space.max_tokens().to_string()))
        .bind(("journal", serde_json::to_string(&complete)?))
        .await?
        .check()?;
    Ok(())
}
