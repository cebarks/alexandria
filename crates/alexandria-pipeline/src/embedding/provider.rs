use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn dimensions(&self) -> usize;
    fn model_id(&self) -> &str;
    /// `Some(token count)` when `embed` would truncate `text`, so a caller that knows which
    /// record the text belongs to can say so. Providers that never truncate keep the default.
    fn overflow(&self, _text: &str) -> Option<usize> {
        None
    }
}
