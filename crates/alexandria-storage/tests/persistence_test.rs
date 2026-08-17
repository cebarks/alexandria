use std::path::Path;

use alexandria_storage::{Database, schema};

#[tokio::test]
async fn test_persistent_storage_round_trip() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let db_path = tmp_dir.path().join("test-db");

    // 1. Write a fact to persistent storage
    let db = Database::connect_persistent(db_path.as_path()).await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    db.inner()
        .query("CREATE fact:persist_test SET content = 'I persist across restarts', embedding = [0.1, 0.2], tags = ['test']")
        .await
        .unwrap()
        .check()
        .unwrap();

    // Drop the connection to release the lock
    drop(db);

    // Small delay for lock release
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 2. Reconnect and read it back
    let db2 = Database::connect_persistent(db_path.as_path()).await.unwrap();
    schema::migrate(db2.inner()).await.unwrap();

    let mut result = db2.inner()
        .query("SELECT * FROM fact:persist_test")
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["content"].as_str().unwrap(), "I persist across restarts");
}

#[tokio::test]
async fn test_memory_mode_via_connect() {
    let memory_path = Path::new(":memory:");
    let db = Database::connect(memory_path).await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    assert!(db.is_connected());
}
