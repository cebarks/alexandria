use alexandria_storage::embedding_state::{
    INCOMPLETE_MODEL, begin_target_reembed, checkpoint_target_reembed,
};
use alexandria_storage::embedding_validation::{
    BASELINE_KEY, read_target_baseline, validate_target_metadata,
};
use alexandria_storage::models::embedding::{EmbeddingSpace, MigrationPhase, ReembedJournal};
use alexandria_storage::repos::{ClusterRepo, MemoryRepo};
use alexandria_storage::{Database, schema, system_config};

fn journal() -> ReembedJournal {
    ReembedJournal {
        run_id: "baseline-fixture".into(),
        source_manifest_sha256: "a".repeat(64),
        source_space: EmbeddingSpace::Legacy {
            model: "before".into(),
            dimensions: 2,
            max_tokens: 128,
        },
        target_space: EmbeddingSpace::Legacy {
            model: "after".into(),
            dimensions: 3,
            max_tokens: 256,
        },
        phase: MigrationPhase::Facts,
        last_fact_id: None,
        reembedded_count: 0,
    }
}

async fn fixture() -> (Database, String, String) {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    system_config::check_embedding_space(db.inner(), &journal().source_space)
        .await
        .unwrap();
    let facts = MemoryRepo::new(db.inner());
    let id = facts
        .create_fact("public one", 0.6, &[1.0, 0.0], &["fixture".into()])
        .await
        .unwrap();
    let clusters = ClusterRepo::new(db.inner());
    let populated = clusters
        .create(Some("populated"), &[1.0, 0.0])
        .await
        .unwrap();
    clusters.add_member(&populated, &id).await.unwrap();
    let empty = clusters
        .create(Some("truly empty"), &[0.0, 1.0])
        .await
        .unwrap();
    db.inner().query("DEFINE TABLE extension_data SCHEMALESS; CREATE extension_data:fixture SET opaque = { nested: ['public', 5] }").await.unwrap().check().unwrap();
    (db, id, empty)
}

#[tokio::test]
async fn begin_persists_owned_baseline_and_counts_atomically_with_sentinel() {
    let (db, _, empty) = fixture().await;
    let current = journal();
    begin_target_reembed(db.inner(), &current).await.unwrap();
    let baseline = read_target_baseline(db.inner(), &current).await.unwrap();
    assert_eq!(baseline.fact_count, 1);
    assert_eq!(baseline.cluster_count, 1);
    assert_eq!(baseline.empty_cluster_ids.len(), 1);
    assert_eq!(
        alexandria_storage::record_id_to_string(&baseline.empty_cluster_ids[0]),
        empty
    );
    assert_eq!(baseline.run_id, current.run_id);
    assert_eq!(
        baseline.source_manifest_sha256,
        current.source_manifest_sha256
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_model")
            .await
            .unwrap()
            .as_deref(),
        Some(INCOMPLETE_MODEL)
    );
    validate_target_metadata(db.inner(), &current)
        .await
        .unwrap();
}

#[tokio::test]
async fn original_baseline_survives_embedding_writes_and_empty_cluster_removal() {
    let (db, id, empty) = fixture().await;
    let current = journal();
    begin_target_reembed(db.inner(), &current).await.unwrap();
    let original = system_config::get_config(db.inner(), BASELINE_KEY)
        .await
        .unwrap();
    MemoryRepo::new(db.inner())
        .update_fact(&id, None, None, None, Some(&[0.0, 1.0, 0.0]))
        .await
        .unwrap();
    ClusterRepo::new(db.inner()).delete(&empty).await.unwrap();
    validate_target_metadata(db.inner(), &current)
        .await
        .unwrap();
    assert!(original.is_some());
    assert_eq!(
        system_config::get_config(db.inner(), BASELINE_KEY)
            .await
            .unwrap(),
        original
    );
}

#[tokio::test]
async fn metadata_mutation_is_detected_on_resume_not_recaptured_as_original() {
    let (db, _, _) = fixture().await;
    let current = journal();
    begin_target_reembed(db.inner(), &current).await.unwrap();
    let original = system_config::get_config(db.inner(), BASELINE_KEY)
        .await
        .unwrap();
    db.inner()
        .query("UPDATE extension_data:fixture SET opaque.nested = ['changed']")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(
        validate_target_metadata(db.inner(), &current)
            .await
            .is_err()
    );
    assert_eq!(
        system_config::get_config(db.inner(), BASELINE_KEY)
            .await
            .unwrap(),
        original
    );
}

#[tokio::test]
async fn malformed_or_missing_baseline_refuses_checkpoints() {
    for payload in ["malformed", ""] {
        let (db, _, _) = fixture().await;
        let mut current = journal();
        begin_target_reembed(db.inner(), &current).await.unwrap();
        if payload.is_empty() {
            db.inner()
                .query("DELETE system_config WHERE key = $key")
                .bind(("key", BASELINE_KEY))
                .await
                .unwrap()
                .check()
                .unwrap();
        } else {
            system_config::set_config(db.inner(), BASELINE_KEY, payload)
                .await
                .unwrap();
        }
        current.phase = MigrationPhase::Centroids;
        assert!(
            checkpoint_target_reembed(db.inner(), &current)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn hidden_only_cluster_refuses_begin_without_deleting_memberships() {
    let (db, id, _) = fixture().await;
    MemoryRepo::new(db.inner())
        .soft_delete_fact(&id)
        .await
        .unwrap();
    let before = ClusterRepo::new(db.inner()).list().await.unwrap().len();
    assert!(begin_target_reembed(db.inner(), &journal()).await.is_err());
    assert_eq!(
        ClusterRepo::new(db.inner()).list().await.unwrap().len(),
        before
    );
    assert!(
        system_config::get_config(db.inner(), BASELINE_KEY)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_model")
            .await
            .unwrap()
            .as_deref(),
        Some("before")
    );
}

#[tokio::test]
async fn hierarchy_reference_refuses_empty_cluster_removal() {
    let (db, _, empty) = fixture().await;
    let parent = ClusterRepo::new(db.inner())
        .create(Some("parent"), &[1.0, 0.0])
        .await
        .unwrap();
    db.inner()
        .query("RELATE $parent->contains_cluster->$child")
        .bind((
            "parent",
            surrealdb::types::RecordId::parse_simple(&parent).unwrap(),
        ))
        .bind((
            "child",
            surrealdb::types::RecordId::parse_simple(&empty).unwrap(),
        ))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(begin_target_reembed(db.inner(), &journal()).await.is_err());
    assert!(
        system_config::get_config(db.inner(), BASELINE_KEY)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn other_relation_references_are_not_silently_deleted_with_empty_clusters() {
    let (db, _, empty) = fixture().await;
    db.inner()
        .query(
            "CREATE provenance:p SET kind = 'public'; RELATE $empty->has_provenance->provenance:p",
        )
        .bind((
            "empty",
            surrealdb::types::RecordId::parse_simple(&empty).unwrap(),
        ))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(begin_target_reembed(db.inner(), &journal()).await.is_err());
    assert!(
        ClusterRepo::new(db.inner())
            .get(&empty)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        system_config::get_config(db.inner(), BASELINE_KEY)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn baseline_write_failure_rolls_back_journal_and_sentinel() {
    let (db, _, _) = fixture().await;
    db.inner().query("DEFINE FIELD OVERWRITE value ON system_config TYPE string ASSERT $this.key != 'embedding_migration_baseline'").await.unwrap().check().unwrap();
    assert!(begin_target_reembed(db.inner(), &journal()).await.is_err());
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_model")
            .await
            .unwrap()
            .as_deref(),
        Some("before")
    );
    assert!(
        system_config::get_config(db.inner(), BASELINE_KEY)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        system_config::get_config(db.inner(), "embedding_migration_journal")
            .await
            .unwrap()
            .is_none()
    );
}
