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

/// The failure mode this pins: `migrate()` applies a file statement-by-statement
/// (each top-level statement commits in its own transaction) and stamps
/// `system_config.schema_version` only once *all* pending migrations succeed. A
/// crash or timeout part-way through the head migration therefore leaves
/// definitions in place with an old stamp, and the next boot re-runs that file
/// from the top. Plain `DEFINE`/`REMOVE` made that retry fatal —
/// "The table 'reminder' already exists" — so the server refused to start until
/// an operator manually removed the partial definitions, contradicting
/// `migrate()`'s "Safe to call on every startup". Every definition statement is
/// `OVERWRITE` (and removals `IF EXISTS`) so a retry finishes instead of jamming.
#[tokio::test]
async fn test_migrations_retry_after_a_lost_version_stamp() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // Rewind the stamp while every definition from v001..v007 is still present:
    // exactly what the next boot sees after a mid-migration failure.
    db.inner()
        .query("UPDATE system_config SET value = '5' WHERE key = 'schema_version'")
        .await
        .unwrap()
        .check()
        .unwrap();

    schema::migrate(db.inner()).await.unwrap();

    let mut result = db
        .inner()
        .query("SELECT * FROM system_config WHERE key = 'schema_version'")
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(
        rows[0]["value"].as_str().unwrap(),
        schema::LATEST_VERSION.to_string(),
        "the retry must re-stamp the head, not just stop erroring"
    );

    // And the schema still behaves: the head migration's defaults survived the
    // second application of its own statements.
    let mut result = db
        .inner()
        .query("SELECT * FROM reminder LIMIT 1")
        .await
        .unwrap();
    assert!(
        result
            .take::<Vec<serde_json::Value>>(0)
            .is_ok_and(|v| v.is_empty()),
        "reminder must remain selectable after the retry"
    );
}

/// The stricter form of the same contract: re-applying the *earliest* migration
/// over a fully built schema must not error either, so a stamp lost at any
/// version self-heals rather than bricking at v001.
#[tokio::test]
async fn test_replaying_a_completed_migration_is_not_an_error() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    for (version, name, sql) in schema::MIGRATIONS {
        let response = db.inner().query(*sql).await.unwrap_or_else(|e| {
            panic!("re-running v{version:03} ({name}) failed at query time: {e}")
        });
        response
            .check()
            .unwrap_or_else(|e| panic!("re-running v{version:03} ({name}) reported an error: {e}"));
    }
}
