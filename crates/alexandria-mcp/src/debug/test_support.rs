//! Shared test helpers for debug route tests. `pub(super)` — only visible within `debug`.

use std::sync::Arc;

use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::Database;

use crate::AlexandriaServer;

/// Minimal stub embedding provider for router-level tests (no model download needed).
pub(super) struct StubEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for StubEmbedding {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2]).collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "stub"
    }
}

pub(super) async fn test_server() -> AlexandriaServer {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner())
        .await
        .unwrap();
    AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0)
}

/// The floor [`banded_server`] runs with.
///
/// `StubEmbedding` returns one constant vector, so every similarity in those tests is 1.0 and the
/// floor can never filter anything — which makes it useless for testing *what the floor drops*.
/// [`BandedEmbedding`] exists for exactly that, and `banded_server` pairs it with a floor its
/// bands straddle.
pub(super) const BANDED_FLOOR: f32 = 0.30;

/// Stub that turns a keyword in the text into an exact cosine similarity to the query.
///
/// Every vector is the 2-D unit vector `[s, sqrt(1 - s²)]` and an unlabelled text — which is what
/// a query is — gets `s = 1`, i.e. `[1, 0]`. Cosine against `[1, 0]` is just `s`, so the
/// similarities are hand-computable and exact: `"strong"` scores [`STRONG_SIMILARITY`], `"weak"`
/// scores [`WEAK_SIMILARITY`], and those two straddle [`BANDED_FLOOR`] by 0.30 either side.
///
/// Deliberately a separate stub from [`StubEmbedding`]/`test_server()`: 40+ existing debug tests
/// assume every similarity is 1.0.
pub(super) struct BandedEmbedding;

/// Cosine similarity a text containing "strong" gets against the query.
pub(super) const STRONG_SIMILARITY: f32 = 0.60;
/// Cosine similarity a text containing "weak" gets against the query — below [`BANDED_FLOOR`].
pub(super) const WEAK_SIMILARITY: f32 = 0.20;

#[async_trait::async_trait]
impl EmbeddingProvider for BandedEmbedding {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let s = if t.contains("strong") {
                    STRONG_SIMILARITY
                } else if t.contains("weak") {
                    WEAK_SIMILARITY
                } else {
                    1.0
                };
                vec![s, (1.0 - s * s).sqrt()]
            })
            .collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "banded-stub"
    }
}

/// A server whose retrieval scores are predictable *and* split by the floor: content naming
/// "strong" lands at 0.60, content naming "weak" at 0.20, against a 0.30 floor.
///
/// The stored memories are written through this same stub, so their embeddings are the band
/// vectors — the ranking is deterministic from the content alone.
pub(super) async fn banded_server() -> AlexandriaServer {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner())
        .await
        .unwrap();
    AlexandriaServer::new(Arc::new(db), Arc::new(BandedEmbedding), 0.75, 86400.0)
        .with_retrieve_min_similarity(BANDED_FLOOR)
}

/// The plain average of a set of embeddings — the centroid the cluster detail page *used* to
/// judge cohesion with, before it was pointed at the stored one.
///
/// Kept available to tests on purpose: it is how a test proves its own fixture is meaningful,
/// by showing the two centroids disagree. Production code must not use it (see
/// [`super::super::clusters::cohesion_of`]).
pub(super) fn average_centroid(members: &[Vec<f32>]) -> Vec<f32> {
    let dims = members.first().map(|m| m.len()).unwrap_or(0);
    let mut centroid = vec![0.0f32; dims];
    for member in members {
        for (index, value) in member.iter().enumerate() {
            centroid[index] += value;
        }
    }
    let n = members.len() as f32;
    for value in centroid.iter_mut() {
        *value /= n;
    }
    centroid
}

/// Creates a four-member cluster whose **stored** centroid and the average of its members
/// reach opposite cohesion verdicts, and returns its id.
///
/// Three members are unit vectors along +x; the fourth is 100× as long and 80° off that axis.
/// The stored centroid `[1, 0]` averages 0.79 cosine across members (Healthy at the default
/// 0.6 floor), while the member average is dragged out to roughly `[0.2, 0.98]`, which
/// averages 0.40 (Needs split). A fixture where the two agree would pass either implementation
/// and prove nothing — that agreement is exactly why the bug survived.
pub(super) async fn disagreeing_cluster(server: &AlexandriaServer) -> String {
    use alexandria_storage::repos::{ClusterRepo, MemoryRepo};

    let cluster_repo = ClusterRepo::new(server.db.inner());
    let memory_repo = MemoryRepo::new(server.db.inner());
    let cid = cluster_repo
        .create(Some("stored centroid disagrees"), &[1.0, 0.0])
        .await
        .unwrap();
    for (index, embedding) in [
        [1.0_f32, 0.0],
        [1.0, 0.0],
        [1.0, 0.0],
        // 100 * [cos 80°, sin 80°]
        [17.364_816, 98.480_78],
    ]
    .iter()
    .enumerate()
    {
        let fact = memory_repo
            .create_fact(&format!("disagreeing member {index}"), 0.5, embedding, &[])
            .await
            .unwrap();
        cluster_repo.add_member(&cid, &fact).await.unwrap();
    }
    cid
}
