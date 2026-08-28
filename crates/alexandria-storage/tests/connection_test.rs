use alexandria_storage::{schema, Database};

#[tokio::test]
async fn test_connect_embedded() {
    let db = Database::connect_embedded().await.unwrap();
    assert!(db.is_connected());
}

#[tokio::test]
async fn test_schema_bootstrap() {
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();
    // Verify tables exist by attempting a SELECT
    let result: Vec<serde_json::Value> = db
        .inner()
        .query("INFO FOR DB")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert!(!result.is_empty());
}
