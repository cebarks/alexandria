use alexandria_pipeline::embedding::{CandleProvider, EmbeddingProvider};

#[tokio::test]
async fn test_candle_embed_produces_vectors() {
    let provider = CandleProvider::new("sentence-transformers/all-MiniLM-L6-v2", "cpu")
        .await
        .unwrap();

    assert_eq!(provider.dimensions(), 384);

    let vectors = provider.embed(&["hello world", "test memory"]).await.unwrap();
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0].len(), 384);
    assert_eq!(vectors[1].len(), 384);
}

#[tokio::test]
async fn test_candle_similar_texts_have_high_similarity() {
    let provider = CandleProvider::new("sentence-transformers/all-MiniLM-L6-v2", "cpu")
        .await
        .unwrap();

    let vectors = provider
        .embed(&[
            "OAuth tokens expire after 7 days",
            "authentication tokens have a 7 day expiry",
            "the weather is sunny today",
        ])
        .await
        .unwrap();

    let sim_related = cosine_similarity(&vectors[0], &vectors[1]);
    let sim_unrelated = cosine_similarity(&vectors[0], &vectors[2]);
    assert!(
        sim_related > sim_unrelated,
        "related similarity ({sim_related}) should be > unrelated ({sim_unrelated})"
    );
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm_a * norm_b)
}
