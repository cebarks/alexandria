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
