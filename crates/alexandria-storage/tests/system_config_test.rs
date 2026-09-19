use alexandria_storage::{Database, schema, system_config};

#[tokio::test]
async fn test_first_boot_stores_embedding_config() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // First call should store the config
    system_config::check_embedding_model(db.inner(), "test-model", 384, 256)
        .await
        .unwrap();

    // Verify it was stored
    let model = system_config::get_config(db.inner(), "embedding_model")
        .await
        .unwrap();
    assert_eq!(model.unwrap(), "test-model");

    let dims = system_config::get_config(db.inner(), "embedding_dimensions")
        .await
        .unwrap();
    assert_eq!(dims.unwrap(), "384");
}

#[tokio::test]
async fn test_same_model_on_restart_passes() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // First boot
    system_config::check_embedding_model(db.inner(), "test-model", 384, 256)
        .await
        .unwrap();

    // Second boot with same model — should pass
    system_config::check_embedding_model(db.inner(), "test-model", 384, 256)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_different_model_on_restart_fails() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // First boot
    system_config::check_embedding_model(db.inner(), "model-a", 384, 256)
        .await
        .unwrap();

    // Second boot with different model — should fail
    let result = system_config::check_embedding_model(db.inner(), "model-b", 384, 256).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Embedding model mismatch"));
    assert!(err.contains("model-a"));
    assert!(err.contains("model-b"));
}

#[tokio::test]
async fn test_different_dimensions_fails() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    system_config::check_embedding_model(db.inner(), "test-model", 384, 256)
        .await
        .unwrap();

    let result = system_config::check_embedding_model(db.inner(), "test-model", 768, 256).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("dimensions mismatch"));
}

/// Pre-lock corpus: facts exist, no lock. Boot stamps the lock so the migrate-embeddings
/// recovery path ("start the server once") keeps working, but it stamps the limit those facts
/// were embedded at, not the configured one. Stamping the configured 256 would make every later
/// boot pass and make `migrate-embeddings` a no-op over a corpus that is really at 128.
#[tokio::test]
async fn test_facts_without_lock_stamps_the_pre_lock_limit() {
    use alexandria_storage::repos::MemoryRepo;

    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    MemoryRepo::new(db.inner())
        .create_fact("pre-lock fact", 1.0, &[0.1_f32; 384], &[])
        .await
        .unwrap();

    let err = system_config::check_embedding_model(db.inner(), "test-model", 384, 256)
        .await
        .expect_err("a 128-token corpus must not boot at 256")
        .to_string();
    assert!(err.contains("migrate-embeddings"), "{err}");

    let get = async |key| system_config::get_config(db.inner(), key).await.unwrap();
    assert_eq!(get("embedding_model").await.unwrap(), "test-model");
    assert_eq!(get("embedding_max_tokens").await.unwrap(), "128");

    // At the default limit the same corpus boots.
    system_config::check_embedding_model(db.inner(), "test-model", 384, 128)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_lower_token_limit_points_at_config_not_at_the_migration() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    system_config::check_embedding_model(db.inner(), "test-model", 384, 256)
        .await
        .unwrap();

    let err = system_config::check_embedding_model(db.inner(), "test-model", 384, 128)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not lower"), "{err}");
    assert!(err.contains("embedding.max_tokens = 256"), "{err}");
}

#[tokio::test]
async fn test_first_boot_stores_token_lock() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    system_config::check_embedding_model(db.inner(), "test-model", 384, 256)
        .await
        .unwrap();

    let tokens = system_config::get_config(db.inner(), "embedding_max_tokens")
        .await
        .unwrap();
    assert_eq!(tokens.unwrap(), "256");
}

#[tokio::test]
async fn test_different_token_limit_fails() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    system_config::check_embedding_model(db.inner(), "test-model", 384, 256)
        .await
        .unwrap();

    let result = system_config::check_embedding_model(db.inner(), "test-model", 384, 512).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("token limit mismatch"), "{err}");
    assert!(err.contains("256"), "{err}");
    assert!(err.contains("512"), "{err}");
    assert!(err.contains("migrate-embeddings"), "{err}");
}

/// A model lock without a token lock predates the token lock; that corpus was embedded
/// at the tokenizer's shipped 128, so booting at any other limit must refuse.
#[tokio::test]
async fn test_model_lock_without_token_lock_means_128() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    system_config::set_config(db.inner(), "embedding_model", "test-model")
        .await
        .unwrap();
    system_config::set_config(db.inner(), "embedding_dimensions", "384")
        .await
        .unwrap();

    system_config::check_embedding_model(db.inner(), "test-model", 384, 128)
        .await
        .unwrap();

    let err = system_config::check_embedding_model(db.inner(), "test-model", 384, 256)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("token limit mismatch"), "{err}");
    assert!(err.contains("128"), "{err}");
}
