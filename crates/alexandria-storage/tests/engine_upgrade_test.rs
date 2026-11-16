//! Frozen 3.3 input is only ever opened through a private physical copy.
#![cfg(target_os = "linux")]

use alexandria_storage::offline_copy::OfflineStoreGuard;
use alexandria_storage::{Database, schema};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::opt::Config;

async fn snapshot(db: &Surreal<Any>) -> BTreeMap<String, Value> {
    let info: Option<Value> = db
        .query("INFO FOR DB STRUCTURE")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    let mut result = BTreeMap::new();
    for table in info.unwrap()["tables"].as_array().unwrap() {
        let name = table["name"].as_str().unwrap();
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        let rows: Vec<Value> = db
            .query(format!("SELECT * FROM `{name}` ORDER BY id"))
            .await
            .unwrap()
            .take(0)
            .unwrap();
        result.insert(name.to_owned(), json!(rows));
    }
    result
}

fn vector_bits(rows: &Value, field: &str) -> Vec<Vec<u64>> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|row| {
            row[field]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_f64().unwrap().to_bits())
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn approximate_knn_without_index_is_refused() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    let result = alexandria_storage::repos::MemoryRepo::new(db.inner())
        .nearest_indexed(&[1.0, 0.0], 1)
        .await;
    assert!(
        result.is_err(),
        "approximate KNN silently accepted a missing index: {result:?}"
    );
}

#[tokio::test]
async fn offline_engine_upgrade_preserves_corpus_and_rebuilds_old_index() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/engine-3.3");
    let expected: BTreeMap<String, Value> =
        serde_json::from_slice(&std::fs::read(root.join("logical.json")).unwrap()).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("writer.txt")).unwrap(),
        "surrealdb=3.3.0\nsurrealkv=0.21.4\n"
    );
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("upgraded");
    let lock_bytes = std::fs::read(root.join("store/LOCK")).unwrap();
    let guard = OfflineStoreGuard::acquire(&root.join("store")).unwrap();
    let receipt = guard.copy_to(&target).unwrap();
    let url = format!("surrealkv://{}", target.display());
    assert!(
        Database::connect_persistent(&target).await.is_err(),
        "ordinary application connections must refuse a 3.x physical store"
    );
    // Explicit opt-in is test-copy-only; ordinary application connections refuse 3.x stores.
    let config = Config::new().datastore_option("experimental_upgrade_from_3x", true);
    let db = surrealdb::engine::any::connect((url.as_str(), config))
        .await
        .unwrap();
    db.use_ns("alexandria").use_db("default").await.unwrap();
    let before_rebuild = snapshot(&db).await;
    assert_eq!(
        before_rebuild, expected,
        "engine conversion changed application data or identity"
    );
    for (table, field) in [("fact", "embedding"), ("cluster", "centroid")] {
        assert_eq!(
            vector_bits(&before_rebuild[table], field),
            vector_bits(&expected[table], field)
        );
    }
    db.query("REBUILD INDEX fact_embedding_hnsw ON fact")
        .await
        .unwrap()
        .check()
        .unwrap();
    alexandria_storage::embedding_state::verify_target_index(&db, 2)
        .await
        .unwrap();
    let rows: Vec<Value> = db.query("SELECT * FROM fact WHERE deleted = false AND quarantined_at = NONE AND embedding <|18,150|> [1.0f, 0.0f]")
        .await.unwrap().take(0).unwrap();
    let actual_ids: BTreeSet<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
    let expected_ids: BTreeSet<&str> = expected["fact"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["deleted"] == false && r.get("quarantined_at").is_none_or(Value::is_null))
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(rows.len(), 16);
    assert_eq!(
        actual_ids, expected_ids,
        "rebuilt index lost identical vectors or returned hidden rows"
    );
    assert_eq!(
        snapshot(&db).await,
        expected,
        "index rebuild changed application data"
    );
    let untouched = temp.path().join("untouched-check");
    assert_eq!(
        guard.copy_to(&untouched).unwrap(),
        receipt,
        "source payload changed"
    );
    assert_eq!(
        std::fs::read(root.join("store/LOCK")).unwrap(),
        lock_bytes,
        "source LOCK changed"
    );
}
