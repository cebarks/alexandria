//! Re-embed every fact and cluster centroid with a new model, then move the lock.
//! Not transactional: a failure mid-way leaves the lock on the old model. While
//! config still names the new model the server refuses to boot; rerun the migration
//! to finish, or revert config to go back to the old model.

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

/// `batch_size` is facts per `embed()` call; it bounds peak memory for large corpora.
pub async fn reembed(
    db: &Database,
    provider: &dyn EmbeddingProvider,
    batch_size: usize,
) -> anyhow::Result<ReembedOutcome> {
    ensure!(batch_size > 0, "embedding.batch_size must be at least 1");
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
        Some(stored) if stored == new_model => {
            return Ok(ReembedOutcome::Skipped(format!("already on {new_model}")));
        }
        Some(stored) => tracing::info!("Re-embedding {stored} -> {new_model}"),
    }

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

    Ok(ReembedOutcome::Done {
        facts: total,
        clusters: updated,
    })
}
