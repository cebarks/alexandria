//! Manual fixture generator: copy into a 3.3 checkout as an integration test.
use alexandria_storage::models::embedding::EmbeddingSpace;
use alexandria_storage::{Database, schema, system_config};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use surrealdb::types::RecordId;

async fn snapshot(db: &Database) -> BTreeMap<String, Value> {
    let info: Option<Value> = db
        .inner()
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
            .inner()
            .query(format!("SELECT * FROM `{name}` ORDER BY id"))
            .await
            .unwrap()
            .take(0)
            .unwrap();
        result.insert(name.to_owned(), json!(rows));
    }
    result
}

/// Build and execute with 3.3; never open this source store with 4.x.
#[tokio::test]
#[ignore = "manual 3.3-only fixture writer; see README.md"]
async fn write_legacy_fixture() {
    assert!(
        include_str!("../../../Cargo.lock").contains("name = \"surrealdb\"\nversion = \"3.3.0\"")
    );
    let root = std::env::var_os("ALEXANDRIA_UPGRADE_FIXTURE")
        .map(PathBuf::from)
        .unwrap();
    assert!(!root.exists(), "writer requires a new disposable directory");
    std::fs::create_dir_all(&root).unwrap();
    let db = Database::connect_persistent(&root.join("store"))
        .await
        .unwrap();
    schema::migrate(db.inner()).await.unwrap();
    system_config::check_embedding_space(
        db.inner(),
        &EmbeddingSpace::Legacy {
            model: "upgrade-fixture".into(),
            dimensions: 2,
            max_tokens: 256,
        },
    )
    .await
    .unwrap();
    schema::ensure_vector_index(db.inner(), 2).await.unwrap();
    db.inner().query("DEFINE FIELD OVERWRITE metadata.sentinel ON fact TYPE option<string>; DEFINE FIELD OVERWRITE metadata.sentinel ON raw TYPE option<string>;")
        .await.unwrap().check().unwrap();
    for n in 0..16 {
        db.inner().query("CREATE $id SET content = $content, embedding = [1.0f, 0.0f], tags = ['upgrade'], metadata = { sentinel: 'preserve' }")
            .bind(("id", RecordId::new("fact", n as i64)))
            .bind(("content", format!("identical vector {n}")))
            .await.unwrap().check().unwrap();
    }
    db.inner().query("CREATE fact:deleted SET content = 'hidden deleted', embedding = [0.6f, 0.8f], deleted = true;
        CREATE fact:quarantined SET content = 'hidden quarantined', embedding = [0.6f, 0.8f], quarantined_at = d'2026-01-01T00:00:00Z';
        CREATE cluster:fixture SET label = 'preserved centroid', centroid = [0.6f, 0.8f];
        CREATE raw:fixture SET content = 'original document', metadata = { sentinel: 'raw' };
        CREATE provenance:fixture SET kind = 'test', agent_id = 'legacy-writer', model = 'fixture';
        CREATE session:fixture SET external_id = 'upgrade-session', summary = 'keep summary', tags = ['upgrade'];
        CREATE reminder:fixture SET message = 'keep reminder', schedule_kind = 'once', due_at = d'2030-01-01T00:00:00Z', next_due_at = d'2030-01-01T00:00:00Z';
        CREATE heat_state:fixture SET memory = fact:deleted, heat = 0.75f, stability = 2.0f, access_count = 3;
        RELATE cluster:fixture->contains_memory:fixture->fact:deleted;
        RELATE session:fixture->contains_session_memory:fixture->fact:deleted;
        RELATE fact:deleted->extracted_from:fixture->raw:fixture;
        RELATE fact:deleted->has_provenance:fixture->provenance:fixture;")
        .await.unwrap().check().unwrap();
    let before = snapshot(&db).await;
    assert_eq!(before["fact"].as_array().unwrap().len(), 18);
    assert_eq!(before["reminder"].as_array().unwrap().len(), 1);
    assert_eq!(before["fact"][0]["metadata"]["sentinel"], "preserve");
    assert_eq!(before["raw"][0]["metadata"]["sentinel"], "raw");
    std::fs::write(
        root.join("logical.json"),
        serde_json::to_vec_pretty(&before).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("writer.txt"),
        "surrealdb=3.3.0\nsurrealkv=0.21.4\n",
    )
    .unwrap();
}
