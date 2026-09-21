//! Re-embed every fact and cluster centroid with a new model or token limit, then move
//! the lock.
//! Not transactional: a failure mid-way leaves the lock on the old model and limit. While
//! config still names the new ones the server refuses to boot; rerun the migration to
//! finish. Reverting config (`embedding.model`, `embedding.max_tokens`) goes back only if no
//! fact has been rewritten yet. The limit only goes up: a corpus embedded at 256 tokens is
//! not re-embedded at 128. The HNSW index is
//! dropped first (it rejects vectors of another dimension); the next server boot
//! redefines it.

use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::repos::{ClusterRepo, MemoryRepo};
use alexandria_storage::{Database, record_id_to_string, system_config};
use anyhow::ensure;

#[derive(Debug)]
pub enum ReembedOutcome {
    /// Nothing to do; the string is a human-readable reason.
    Skipped(String),
    Done {
        facts: usize,
        clusters: usize,
    },
}

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
    let memories = MemoryRepo::new(db.inner());
    match system_config::get_config(db.inner(), "embedding_model").await? {
        None => {
            // A database from before the lock existed has facts but no lock; stamping
            // the new model over them would silently mix vector spaces.
            let facts = memories.count(None, None, true).await?;
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

    // 2. Centroids: plain mean of all members, deleted included, matching how the
    //    maintenance loop reads members via get_members. Empty clusters are dropped:
    //    their centroid would keep the old dimension and nothing references them.
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
    system_config::set_config(db.inner(), "embedding_model", new_model).await?;
    system_config::set_config(db.inner(), "embedding_dimensions", &dims.to_string()).await?;
    system_config::set_config(db.inner(), "embedding_max_tokens", &max_tokens.to_string()).await?;

    Ok(ReembedOutcome::Done {
        facts: total,
        clusters: updated,
    })
}
