use alexandria_storage::{schema, system_config, Database};

#[tokio::test]
async fn test_first_boot_stores_embedding_config() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // First call should store the config
    system_config::check_embedding_model(db.inner(), "test-model", 384)
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
    system_config::check_embedding_model(db.inner(), "test-model", 384)
        .await
        .unwrap();

    // Second boot with same model — should pass
    system_config::check_embedding_model(db.inner(), "test-model", 384)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_different_model_on_restart_fails() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // First boot
    system_config::check_embedding_model(db.inner(), "model-a", 384)
        .await
        .unwrap();

    // Second boot with different model — should fail
    let result = system_config::check_embedding_model(db.inner(), "model-b", 384).await;
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

    system_config::check_embedding_model(db.inner(), "test-model", 384)
        .await
        .unwrap();

    let result = system_config::check_embedding_model(db.inner(), "test-model", 768).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("dimensions mismatch"));
}
