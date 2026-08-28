use alexandria_storage::{schema, Database};

#[tokio::test]
async fn test_fresh_db_runs_all_migrations() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // Verify system_config has schema_version
    let mut result = db
        .inner()
        .query("SELECT * FROM system_config WHERE key = 'schema_version'")
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(rows.len(), 1);
    // Version should be "5" (latest migration)
    let version = rows[0]["value"].as_str().unwrap();
    assert_eq!(version, "5");
}

#[tokio::test]
async fn test_migrate_idempotent() {
    let db = Database::connect_embedded().await.unwrap();

    // Run twice — should be a no-op the second time
    schema::migrate(db.inner()).await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // Still at version 5
    let mut result = db
        .inner()
        .query("SELECT * FROM system_config WHERE key = 'schema_version'")
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(rows[0]["value"].as_str().unwrap(), "5");
}

#[tokio::test]
async fn test_memory_edge_table_exists_after_migration() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // memory_edge table should exist — test by inserting a relation
    // First create two facts
    db.inner()
        .query("CREATE fact:a SET content = 'fact A', embedding = [1.0], tags = []")
        .await
        .unwrap()
        .check()
        .unwrap();
    db.inner()
        .query("CREATE fact:b SET content = 'fact B', embedding = [1.0], tags = []")
        .await
        .unwrap()
        .check()
        .unwrap();

    // Create an edge between them
    db.inner()
        .query("RELATE fact:a->memory_edge->fact:b SET edge_type = 'relates_to', strength = 0.8")
        .await
        .unwrap()
        .check()
        .unwrap();

    // Query the edge
    let mut result = db.inner().query("SELECT * FROM memory_edge").await.unwrap();
    let edges: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["edge_type"].as_str().unwrap(), "relates_to");
}
