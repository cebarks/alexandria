//! Legacy-only re-embedding of every fact and centroid with a new model/limit, then move
//! the lock.
//! Not transactional: a failure mid-way leaves the lock on the old model and limit. While
//! config still names the new ones the server refuses to boot; rerun the migration to
//! finish. Reverting config (`embedding.model`, `embedding.max_tokens`) goes back only if no
//! fact has been rewritten yet. The limit only goes up: a corpus embedded at 256 tokens is
//! not re-embedded at 128. The HNSW index is
//! dropped first (it rejects vectors of another dimension); the next server boot
//! redefines it.

use alexandria_pipeline::embedding::{EmbeddingProvider, LoadedEmbedding};
use alexandria_storage::embedding_state::{
    checkpoint_target_reembed, publish_target_space, read_target_journal, validate_target_ownership,
};
use alexandria_storage::embedding_validation::{
    checked_target_centroid, read_target_baseline, validate_target_data, validate_target_facts,
    validate_target_metadata, validate_unit_embedding,
};
use alexandria_storage::models::embedding::{MigrationPhase, ReembedJournal};
use alexandria_storage::repos::{ClusterRepo, FactListQuery, MemoryRepo};
use alexandria_storage::{Database, record_id_to_string, system_config};
use anyhow::ensure;
use surrealdb::types::ToSql;

#[derive(Debug)]
pub enum ReembedOutcome {
    /// Nothing to do; the string is a human-readable reason.
    Skipped(String),
    Done {
        facts: usize,
        clusters: usize,
    },
}

/// Rewrite only an already-owned target. Inference is microbatch1; `progress_batch_size`
/// bounds content pages and committed write/log groups, not model concurrency.
/// Replay an uncheckpointed group, rerun centroid work idempotently, then validate
/// the original metadata, every vector, and the index before atomic publication.
pub async fn reembed_target(
    db: &Database,
    loaded: &LoadedEmbedding,
    progress_batch_size: usize,
    mut journal: ReembedJournal,
) -> anyhow::Result<ReembedOutcome> {
    journal.validate()?;
    loaded.space.validate()?;
    ensure!(
        progress_batch_size > 0,
        "Migration progress batch size must be positive"
    );
    ensure!(
        loaded.space == journal.target_space
            && loaded.provider.model_id() == loaded.space.model()
            && loaded.provider.dimensions() == loaded.space.dimensions()
            && loaded.effective_max_tokens == loaded.space.max_tokens(),
        "Migration loaded provider/owned target identity mismatch"
    );
    ensure!(
        read_target_journal(db.inner()).await?.as_ref() == Some(&journal),
        "Migration journal is stale or owned by another run"
    );
    validate_target_ownership(db.inner(), &journal).await?;
    let baseline = read_target_baseline(db.inner(), &journal).await?;
    if journal.phase == MigrationPhase::Complete {
        system_config::check_embedding_space(db.inner(), &loaded.space).await?;
        return Ok(ReembedOutcome::Skipped(
            "owned target is already complete".into(),
        ));
    }
    validate_target_metadata(db.inner(), &journal).await?;
    ensure!(
        journal.reembedded_count <= baseline.fact_count,
        "Checkpoint exceeds original fact count"
    );
    if journal.phase == MigrationPhase::Facts {
        alexandria_storage::schema::drop_vector_index(db.inner()).await?;
        rewrite_target_facts(
            db,
            loaded,
            progress_batch_size,
            &mut journal,
            baseline.fact_count,
        )
        .await?;
        journal.phase = MigrationPhase::Centroids;
        checkpoint_target_reembed(db.inner(), &journal).await?;
    }
    if journal.phase == MigrationPhase::Centroids {
        validate_target_facts(db.inner(), &journal).await?;
        rewrite_target_centroids(db, &journal).await?;
        journal.phase = MigrationPhase::Verify;
        checkpoint_target_reembed(db.inner(), &journal).await?;
    }
    validate_target_data(db.inner(), &journal).await?;
    alexandria_storage::schema::ensure_vector_index(db.inner(), loaded.space.dimensions()).await?;
    publish_target_space(db.inner(), &journal, &loaded.space).await?;
    Ok(ReembedOutcome::Done {
        facts: baseline.fact_count,
        clusters: baseline.cluster_count,
    })
}

async fn rewrite_target_facts(
    db: &Database,
    loaded: &LoadedEmbedding,
    batch_size: usize,
    journal: &mut ReembedJournal,
    total: usize,
) -> anyhow::Result<()> {
    let memories = MemoryRepo::new(db.inner());
    loop {
        let page = memories
            .reembed_page(journal.last_fact_id.clone(), batch_size)
            .await?;
        if page.is_empty() {
            break;
        }
        for fact in &page {
            let vectors = loaded.provider.embed(&[fact.content.as_str()]).await?;
            ensure!(
                vectors.len() == 1,
                "Migration provider must return one embedding for one text"
            );
            validate_unit_embedding(&vectors[0], loaded.space.dimensions())?;
            memories
                .write_reembed_embedding(&fact.id, &vectors[0])
                .await?;
        }
        let mut next = journal.clone();
        next.last_fact_id = page.last().map(|fact| fact.id.clone());
        next.reembedded_count = next
            .reembedded_count
            .checked_add(page.len())
            .ok_or_else(|| anyhow::anyhow!("Migration count overflow"))?;
        ensure!(
            next.reembedded_count <= total,
            "Migration corpus grew beyond original baseline"
        );
        checkpoint_target_reembed(db.inner(), &next).await?;
        *journal = next;
        tracing::info!(
            "Re-embedded {}/{total} target facts",
            journal.reembedded_count
        );
    }
    ensure!(
        journal.reembedded_count == total,
        "Migration did not cover the original fact count"
    );
    Ok(())
}

async fn rewrite_target_centroids(db: &Database, journal: &ReembedJournal) -> anyhow::Result<()> {
    let baseline = read_target_baseline(db.inner(), journal).await?;
    let clusters = ClusterRepo::new(db.inner());
    let mut rows = clusters.list().await?;
    rows.sort_by(|left, right| left.id.cmp(&right.id));
    for cluster in rows {
        let id = cluster
            .id
            .ok_or_else(|| anyhow::anyhow!("Cluster has no ID"))?;
        let text_id = id.to_sql();
        let members = clusters.get_reembed_members(&id).await?;
        if baseline.empty_cluster_ids.binary_search(&id).is_ok() {
            ensure!(
                members.is_empty() && !clusters.has_reembed_references(&id).await?,
                "Approved empty cluster acquired references"
            );
            clusters.delete(&text_id).await?;
        } else {
            let centroid = checked_target_centroid(&members, journal.target_space.dimensions())?;
            clusters.update_centroid(&text_id, &centroid).await?;
        }
    }
    Ok(())
}

/// Granite is refused before any read/write; it requires a separate offline target.
/// `batch_size` is facts per `embed()` call and the progress-log granularity. It does not
/// bound memory: the corpus is preloaded and Candle embeds one text per forward pass.
/// Must be at least 1; `Config::load` rejects 0 before this is reached.
///
/// `max_tokens` is the limit `provider` truncates at, which becomes the new lock. `force`
/// re-embeds even when the lock already matches: the escape for a corpus whose lock is right
/// but whose vectors are not (a binary rolled back after migrating writes old-limit vectors
/// under the new lock, and nothing can detect that afterwards).
pub async fn reembed(
    db: &Database,
    provider: &dyn EmbeddingProvider,
    batch_size: usize,
    max_tokens: usize,
    force: bool,
) -> anyhow::Result<ReembedOutcome> {
    let new_model = provider.model_id();
    anyhow::ensure!(
        new_model != alexandria_storage::models::embedding::GRANITE_MODEL_ID,
        "Granite requires a separate offline migrated copy; the legacy in-place helper is not supported"
    );
    let source_space = system_config::legacy_reembed_source(db.inner()).await?;
    let memories = MemoryRepo::new(db.inner());
    match system_config::get_config(db.inner(), "embedding_model").await? {
        None => {
            // A database from before the lock existed has facts but no lock; stamping
            // the new model over them would silently mix vector spaces.
            // Every row counts here, deleted and quarantined included: a soft-deleted or
            // quarantined fact still holds vectors made by some model at some token limit, and the
            // dimension invariant covers those rows too.
            let facts = memories
                .count(&FactListQuery {
                    include_deleted: true,
                    include_quarantined: true,
                    ..Default::default()
                })
                .await?;
            ensure!(
                facts == 0,
                "no embedding lock but {facts} fact(s) exist; the database predates the lock. \
                 Start the server once with config naming the model that produced them, \
                 then rerun"
            );
            return Ok(ReembedOutcome::Skipped(
                "no embedding lock found (fresh database); just start the server".into(),
            ));
        }
        Some(stored) => {
            let stored_tokens = system_config::stored_max_tokens(db.inner()).await?;
            // Same model only: a different model re-embeds everything in its own space, and
            // its position table may be smaller than the old limit.
            ensure!(
                stored != new_model || max_tokens >= stored_tokens,
                "the corpus is embedded at {stored_tokens} tokens and embedding.max_tokens is \
                 {max_tokens}; the limit is not lowered. Set embedding.max_tokens = {stored_tokens}"
            );
            if stored == new_model && stored_tokens == max_tokens && !force {
                return Ok(ReembedOutcome::Skipped(format!(
                    "already on {new_model} at {max_tokens} tokens (pass --force to re-embed anyway)"
                )));
            }
            tracing::info!(
                "Re-embedding {stored} ({stored_tokens} tokens) -> {new_model} ({max_tokens} tokens)"
            );
        }
    }

    alexandria_storage::schema::drop_vector_index(db.inner()).await?;

    // 1. Facts, deleted ones included.
    let rows = memories.all_ids_and_content().await?;
    let total = rows.len();
    let mut done = 0;
    for batch in rows.chunks(batch_size) {
        let texts: Vec<&str> = batch.iter().map(|(_, c)| c.as_str()).collect();
        let vecs = provider.embed(&texts).await?;
        ensure!(
            vecs.len() == batch.len(),
            "provider returned {} embeddings for {} texts",
            vecs.len(),
            batch.len()
        );
        for ((id, _), vec) in batch.iter().zip(&vecs) {
            memories
                .update_fact(id, None, None, None, Some(vec))
                .await?;
        }
        done += batch.len();
        tracing::info!("Re-embedded {done}/{total} facts");
    }

    // 2. Centroids: plain mean of live members, matching get_members/maintenance.
    // Legacy empty-cluster deletion remains unchanged; owned targets use the
    // stricter reference/baseline checks in reembed_target above.
    let clusters = ClusterRepo::new(db.inner());
    let dims = provider.dimensions();
    let mut updated = 0;
    let mut dropped = 0;
    for cluster in clusters.list().await? {
        let Some(id) = cluster.id.as_ref().map(record_id_to_string) else {
            continue;
        };
        let members = clusters.get_members(&id).await?;
        if members.is_empty() {
            clusters.delete(&id).await?;
            dropped += 1;
            continue;
        }
        let mut centroid = vec![0.0f32; dims];
        for m in &members {
            for (c, e) in centroid.iter_mut().zip(&m.embedding) {
                *c += e;
            }
        }
        let n = members.len() as f32;
        for c in &mut centroid {
            *c /= n;
        }
        clusters.update_centroid(&id, &centroid).await?;
        updated += 1;
    }

    if dropped > 0 {
        tracing::info!("Dropped {dropped} empty clusters");
    }

    // 3. Lock last.
    let target = alexandria_storage::models::embedding::EmbeddingSpace::Legacy {
        model: new_model.into(),
        dimensions: dims,
        max_tokens,
    };
    system_config::publish_legacy_reembed_space(db.inner(), &source_space, &target).await?;

    Ok(ReembedOutcome::Done {
        facts: total,
        clusters: updated,
    })
}
