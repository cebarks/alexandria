use alexandria_pipeline::embedding::{CandleProvider, EmbeddingProvider};

#[tokio::test]
async fn test_candle_embed_produces_vectors() {
    let provider = CandleProvider::new("sentence-transformers/all-MiniLM-L6-v2", "cpu")
        .await
        .unwrap();

    assert_eq!(provider.dimensions(), 384);

    let vectors = provider
        .embed(&["hello world", "test memory"])
        .await
        .unwrap();
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

#[tokio::test]
async fn test_candle_cls_pooled_model_loads_and_normalises() {
    // BAAI/bge-small-en-v1.5 ships 1_Pooling/config.json with pooling_mode_cls_token = true.
    let provider = CandleProvider::new("BAAI/bge-small-en-v1.5", "cpu")
        .await
        .unwrap();
    assert_eq!(provider.dimensions(), 384);

    let vectors = provider.embed(&["hello world"]).await.unwrap();
    assert_eq!(vectors[0].len(), 384);
    let norm: f32 = vectors[0].iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "expected unit norm, got {norm}");
}

/// Same model, same text: flipping the pooling flag must change the vector,
/// proving the CLS branch is actually taken rather than falling back to mean.
#[tokio::test]
async fn test_candle_cls_and_mean_pooling_differ() {
    let mut provider = CandleProvider::new("sentence-transformers/all-MiniLM-L6-v2", "cpu")
        .await
        .unwrap();
    let mean = provider.embed(&["hello world"]).await.unwrap().remove(0);
    provider.set_cls_pooling(true);
    let cls = provider.embed(&["hello world"]).await.unwrap().remove(0);
    assert_eq!(mean.len(), cls.len());
    assert_ne!(mean, cls);
}

/// The cached tokenizer.json truncates at 128 tokens. A ~200-token text and the same text
/// with a tail appended must embed differently; under 128-token truncation they are the
/// same prefix and produce identical vectors.
#[tokio::test]
async fn test_candle_embeds_past_128_tokens() {
    let provider = CandleProvider::new("sentence-transformers/all-MiniLM-L6-v2", "cpu")
        .await
        .unwrap();

    // 40 x 5 words = 200 words, one wordpiece each -> ~202 tokens with [CLS]/[SEP].
    let body = "the cat sat down quietly ".repeat(40);
    let tailed = format!("{body} zebra kangaroo volcano");
    let vectors = provider
        .embed(&[body.as_str(), tailed.as_str()])
        .await
        .unwrap();
    assert_ne!(vectors[0], vectors[1], "tail past token 128 was ignored");

    // Past the new limit still embeds (truncated), no error.
    let huge = "word ".repeat(2000);
    let v = provider.embed(&[huge.as_str()]).await.unwrap();
    assert_eq!(v[0].len(), 384);
}
