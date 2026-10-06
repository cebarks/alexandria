use std::path::Path;

use alexandria_storage::{Database, schema};

#[tokio::test]
async fn test_persistent_storage_round_trip() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let db_path = tmp_dir.path().join("test-db");

    // 1. Write a fact to persistent storage
    let db = Database::connect_persistent(db_path.as_path())
        .await
        .unwrap();
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
    let db2 = Database::connect_persistent(db_path.as_path())
        .await
        .unwrap();
    schema::migrate(db2.inner()).await.unwrap();

    // 3. Replay every compiled-in migration against the on-disk engine.
    //
    // The PR claimed v008 was covered "for both apply and replay on disk", and only apply was true:
    // the second `migrate` above short-circuits (`current_version == LATEST_VERSION` leaves nothing
    // pending), so no v008 statement re-ran here at all. Statement-level replay was pinned only
    // in-memory. That distinction is the entire reason this test opens a disk store rather than
    // `mem://` — SurrealKV is the engine users actually run, and a `DEFINE ... OVERWRITE` that
    // replays cleanly in memory is not proven to replay cleanly on disk.
    //
    // Rewinding the stamp makes `migrate` treat all eight files as pending again, over a schema that
    // already exists — the crash-mid-file state the `OVERWRITE`/`IF EXISTS` rule exists for.
    db2.inner()
        .query("UPDATE system_config SET value = '1' WHERE key = 'schema_version'")
        .await
        .unwrap()
        .check()
        .unwrap();
    schema::migrate(db2.inner())
        .await
        .expect("re-applying every migration, v008 included, must succeed against a disk store");

    // The replay must have left the schema at the compiled-in head, not somewhere below it.
    let applied = alexandria_storage::system_config::get_config(db2.inner(), "schema_version")
        .await
        .unwrap()
        .expect("the stamp is written by every migrate");
    assert_eq!(
        applied,
        schema::LATEST_VERSION.to_string(),
        "after a full replay the disk store must report the compiled-in head"
    );

    let mut result = db2
        .inner()
        .query("SELECT * FROM fact:persist_test")
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["content"].as_str().unwrap(),
        "I persist across restarts"
    );
}

#[tokio::test]
async fn test_memory_mode_via_connect() {
    let memory_path = Path::new(":memory:");
    let db = Database::connect(memory_path).await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    assert!(db.is_connected());
}
