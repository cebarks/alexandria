use alexandria_storage::{Database, schema};

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
    let version = rows[0]["value"].as_str().unwrap();
    assert_eq!(version, schema::LATEST_VERSION.to_string());
}

#[tokio::test]
async fn test_migrate_idempotent() {
    let db = Database::connect_embedded().await.unwrap();

    // Run twice — should be a no-op the second time
    schema::migrate(db.inner()).await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    let mut result = db
        .inner()
        .query("SELECT * FROM system_config WHERE key = 'schema_version'")
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(
        rows[0]["value"].as_str().unwrap(),
        schema::LATEST_VERSION.to_string()
    );
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

// CREATE alone doesn't prove the migration ran — SurrealDB implicitly creates
// undefined tables as SCHEMALESS; the field assertions below do.
#[tokio::test]
async fn test_reminder_table_exists_after_migration() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    let result = db
        .inner()
        .query("CREATE reminder SET message = 'probe', schedule_kind = 'once', next_due_at = time::now()")
        .await
        .unwrap();
    result.check().unwrap();

    // SCHEMAFULL defaults from v007 must be present on the created row.
    let mut result = db.inner().query("SELECT * FROM reminder").await.unwrap();
    let rows: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"].as_str().unwrap(), "pending");
    assert_eq!(rows[0]["delivered_count"].as_i64().unwrap(), 0);
}
