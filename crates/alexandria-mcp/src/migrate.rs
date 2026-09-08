//! Re-embed every fact and cluster centroid with a new model, then move the lock.
//! Not transactional: a failure mid-way leaves the lock on the old model, so the
//! server keeps refusing to boot and rerunning the migration is the recovery.

use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::repos::{ClusterRepo, MemoryRepo};
use alexandria_storage::{Database, record_id_to_string, system_config};

pub enum ReembedOutcome {
    /// Nothing to do; the string is a human-readable reason.
    Skipped(String),
    Done {
        facts: usize,
        clusters: usize,
    },
}

pub async fn reembed(
    db: &Database,
    provider: &dyn EmbeddingProvider,
) -> anyhow::Result<ReembedOutcome> {
    let new_model = provider.model_id();
    match system_config::get_config(db.inner(), "embedding_model").await? {
        None => {
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
    let memories = MemoryRepo::new(db.inner());
    let rows = memories.all_ids_and_content().await?;
    let total = rows.len();
    for (i, (id, content)) in rows.iter().enumerate() {
        let vecs = provider.embed(&[content.as_str()]).await?;
        memories
            .update_fact(id, None, None, None, Some(&vecs[0]))
            .await?;
        if (i + 1) % 50 == 0 || i + 1 == total {
            tracing::info!("Re-embedded {}/{total} facts", i + 1);
        }
    }

    // 2. Centroids: plain mean of all members, deleted included, matching how the
    //    maintenance loop reads members via get_members.
    let clusters = ClusterRepo::new(db.inner());
    let dims = provider.dimensions();
    let mut updated = 0;
    for (cluster, _) in clusters.list_with_counts().await? {
        let Some(id) = cluster.id.as_ref().map(record_id_to_string) else {
            continue;
        };
        let members = clusters.get_members(&id).await?;
        if members.is_empty() {
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

    // 3. Lock last.
    system_config::set_config(db.inner(), "embedding_model", new_model).await?;
    system_config::set_config(db.inner(), "embedding_dimensions", &dims.to_string()).await?;

    Ok(ReembedOutcome::Done {
        facts: total,
        clusters: updated,
    })
}
