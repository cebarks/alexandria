use alexandria_storage::repos::{MemoryRepo, ReembedFact};
use alexandria_storage::{Database, schema};
use surrealdb::types::{RecordId, SurrealValue};

async fn fixture() -> Database {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    for (key, deleted, quarantined) in [
        ("a", false, false),
        ("b", true, false),
        ("é:quote'", false, true),
        ("漢字", false, false),
    ] {
        db.inner().query("CREATE $id SET content = $content, confidence = 0.6, embedding = [1.0, 0.0], tags = ['fixture'], deleted = $deleted, quarantined_at = $quarantine")
            .bind(("id", RecordId::new("fact", key)))
            .bind(("content", format!("public {key}")))
            .bind(("deleted", deleted))
            .bind(("quarantine", quarantined.then(chrono::Utc::now)))
            .await.unwrap().check().unwrap();
    }
    db.inner().query("CREATE $id SET content = 'public numeric key', confidence = 0.6, embedding = [1.0, 0.0], tags = ['fixture'], deleted = false")
        .bind(("id", RecordId::new("fact", 7i64))).await.unwrap().check().unwrap();
    db
}

#[tokio::test]
async fn pages_cover_all_facts_in_unique_key_order_including_hidden_unicode_rows() {
    let db = fixture().await;
    let repo = MemoryRepo::new(db.inner());
    let mut response = db
        .inner()
        .query("SELECT id, content FROM fact ORDER BY id ASC")
        .await
        .unwrap();
    let expected: Vec<ReembedFact> = response.take(0).unwrap();
    let mut actual = Vec::new();
    let mut cursor = None;
    loop {
        let page = repo.reembed_page(cursor.clone(), 1).await.unwrap();
        if page.is_empty() {
            break;
        }
        assert_eq!(page.len(), 1);
        cursor = Some(page[0].id.clone());
        actual.extend(page);
    }
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 5);
    assert!(repo.reembed_page(cursor, 2).await.unwrap().is_empty());
}

#[tokio::test]
async fn uncheckpointed_group_is_replayed_and_embedding_write_preserves_nonvector_metadata() {
    #[derive(Debug, PartialEq, serde::Deserialize, SurrealValue)]
    struct Metadata {
        id: RecordId,
        content: String,
        confidence: f64,
        tags: Vec<String>,
        created_at: chrono::DateTime<chrono::Utc>,
        deleted: bool,
        quarantined_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let db = fixture().await;
    let repo = MemoryRepo::new(db.inner());
    let first = repo.reembed_page(None, 2).await.unwrap();
    assert_eq!(first.len(), 2);
    let mut response = db
        .inner()
        .query("SELECT * OMIT embedding FROM fact ORDER BY id ASC")
        .await
        .unwrap();
    let before: Vec<Metadata> = response.take(0).unwrap();
    repo.update_fact(
        &alexandria_storage::record_id_to_string(&first[0].id),
        None,
        None,
        None,
        Some(&[0.0, 1.0, 0.0]),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(repo.reembed_page(None, 2).await.unwrap(), first);
    assert_eq!(
        repo.reembed_page(Some(first[1].id.clone()), 2)
            .await
            .unwrap()
            .len(),
        2
    );
    let mut response = db
        .inner()
        .query("SELECT * OMIT embedding FROM fact ORDER BY id ASC")
        .await
        .unwrap();
    assert_eq!(response.take::<Vec<Metadata>>(0).unwrap(), before);
}

#[tokio::test]
async fn invalid_page_limit_and_wrong_table_cursor_refuse() {
    let db = fixture().await;
    let repo = MemoryRepo::new(db.inner());
    assert!(repo.reembed_page(None, 0).await.is_err());
    assert!(repo.reembed_page(None, usize::MAX).await.is_err());
    let range = RecordId::new(
        "fact",
        surrealdb::types::RecordIdKeyRange {
            start: std::ops::Bound::Unbounded,
            end: std::ops::Bound::Unbounded,
        },
    );
    assert!(repo.reembed_page(Some(range), 1).await.is_err());
    assert!(
        repo.reembed_page(Some(RecordId::new("cluster", "a")), 1)
            .await
            .is_err()
    );
}
