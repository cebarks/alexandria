use alexandria_storage::embedding_state::{
    INCOMPLETE_MODEL, JOURNAL_KEY, begin_target_reembed, checkpoint_target_reembed,
    publish_target_space, read_target_journal,
};
use alexandria_storage::models::embedding::{
    EmbeddingSpace, GraniteSpace, MigrationPhase, ReembedJournal,
};
use alexandria_storage::{Database, schema, system_config};
use surrealdb::types::RecordId;

fn journal() -> ReembedJournal {
    ReembedJournal {
        run_id: "isolated-test-run".into(),
        source_manifest_sha256: "a".repeat(64),
        source_space: EmbeddingSpace::Legacy {
            model: "source-model".into(),
            dimensions: 2,
            max_tokens: 128,
        },
        target_space: EmbeddingSpace::GraniteV1(Box::new(GraniteSpace {
            model: "ibm-granite/granite-embedding-311m-multilingual-r2".into(),
            revision: "4".repeat(40),
            onnx_sha256: "b".repeat(64),
            burnpack_sha256: "c".repeat(64),
            tokenizer_sha256: "d".repeat(64),
            pooling: "cls".into(),
            normalization: "l2-f64-accum-f32".into(),
            precision: "f32".into(),
            semantics_version: 1,
            dimensions: 768,
            max_tokens: 8192,
        })),
        phase: MigrationPhase::Facts,
        last_fact_id: None,
        reembedded_count: 0,
    }
}

async fn seed(db: &Database) {
    schema::migrate(db.inner()).await.unwrap();
    system_config::check_embedding_model(db.inner(), "source-model", 2, 128)
        .await
        .unwrap();
}

async fn snapshot(db: &Database) -> Vec<Option<String>> {
    let mut values = Vec::new();
    for key in [
        "embedding_model",
        "embedding_dimensions",
        "embedding_max_tokens",
        "embedding_space",
        JOURNAL_KEY,
    ] {
        values.push(system_config::get_config(db.inner(), key).await.unwrap());
    }
    values
}

async fn verify_phase(db: &Database) -> ReembedJournal {
    let mut current = journal();
    begin_target_reembed(db.inner(), &current).await.unwrap();
    current.phase = MigrationPhase::Centroids;
    checkpoint_target_reembed(db.inner(), &current)
        .await
        .unwrap();
    current.phase = MigrationPhase::Verify;
    checkpoint_target_reembed(db.inner(), &current)
        .await
        .unwrap();
    current
}

#[tokio::test]
async fn beginning_blocks_both_serving_apis_and_preserves_source_keys() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    let current = journal();
    begin_target_reembed(db.inner(), &current).await.unwrap();
    assert_eq!(
        read_target_journal(db.inner()).await.unwrap(),
        Some(current.clone())
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_model")
            .await
            .unwrap()
            .as_deref(),
        Some(INCOMPLETE_MODEL)
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_dimensions")
            .await
            .unwrap()
            .as_deref(),
        Some("2")
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_max_tokens")
            .await
            .unwrap()
            .as_deref(),
        Some("128")
    );
    assert!(
        system_config::check_embedding_model(db.inner(), "source-model", 2, 128)
            .await
            .is_err()
    );
    assert!(
        system_config::check_embedding_space(db.inner(), &current.target_space)
            .await
            .is_err()
    );
    assert!(
        system_config::stored_embedding_space(db.inner())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn begin_transaction_failure_cannot_leave_a_journal_without_its_sentinel() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    let before = snapshot(&db).await;
    db.inner().query("DEFINE FIELD OVERWRITE value ON system_config TYPE string ASSERT $value != 'alexandria:embedding-migration-incomplete'")
        .await.unwrap().check().unwrap();
    assert!(begin_target_reembed(db.inner(), &journal()).await.is_err());
    assert_eq!(snapshot(&db).await, before);
    db.inner()
        .query("DEFINE FIELD OVERWRITE value ON system_config TYPE string")
        .await
        .unwrap()
        .check()
        .unwrap();
    begin_target_reembed(db.inner(), &journal()).await.unwrap();
    assert_eq!(
        read_target_journal(db.inner()).await.unwrap(),
        Some(journal())
    );
}

#[tokio::test]
async fn checkpoints_reject_changed_ownership_and_regressing_progress() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    let original = journal();
    begin_target_reembed(db.inner(), &original).await.unwrap();
    for changed in [
        ReembedJournal {
            run_id: "other-run".into(),
            ..original.clone()
        },
        ReembedJournal {
            source_manifest_sha256: "e".repeat(64),
            ..original.clone()
        },
        ReembedJournal {
            source_space: EmbeddingSpace::Legacy {
                model: "other-source".into(),
                dimensions: 2,
                max_tokens: 128,
            },
            ..original.clone()
        },
        ReembedJournal {
            target_space: EmbeddingSpace::Legacy {
                model: "other-target".into(),
                dimensions: 2,
                max_tokens: 256,
            },
            ..original.clone()
        },
        ReembedJournal {
            phase: MigrationPhase::Complete,
            ..original.clone()
        },
        ReembedJournal {
            phase: MigrationPhase::Verify,
            ..original.clone()
        },
    ] {
        assert!(
            checkpoint_target_reembed(db.inner(), &changed)
                .await
                .is_err()
        );
        assert_eq!(
            read_target_journal(db.inner()).await.unwrap(),
            Some(original.clone())
        );
    }
    let progressed = ReembedJournal {
        reembedded_count: 2,
        last_fact_id: Some(RecordId::new("fact", "β:memory")),
        ..original.clone()
    };
    checkpoint_target_reembed(db.inner(), &progressed)
        .await
        .unwrap();
    assert_eq!(
        read_target_journal(db.inner()).await.unwrap(),
        Some(progressed.clone())
    );
    assert!(
        checkpoint_target_reembed(db.inner(), &original)
            .await
            .is_err()
    );
    let earlier_cursor = ReembedJournal {
        reembedded_count: 3,
        last_fact_id: Some(RecordId::new("fact", "a")),
        ..progressed.clone()
    };
    assert!(
        checkpoint_target_reembed(db.inner(), &earlier_cursor)
            .await
            .is_err()
    );
    let mut centroids = progressed.clone();
    centroids.phase = MigrationPhase::Centroids;
    checkpoint_target_reembed(db.inner(), &centroids)
        .await
        .unwrap();
    assert!(
        checkpoint_target_reembed(db.inner(), &progressed)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn malformed_or_pending_journal_refuses_even_when_compatibility_keys_look_ready() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    for encoded in [
        "not-json".to_string(),
        "{}".to_string(),
        serde_json::json!({"run_id":"fake", "phase":"complete"}).to_string(),
    ] {
        system_config::set_config(db.inner(), JOURNAL_KEY, &encoded)
            .await
            .unwrap();
        assert!(read_target_journal(db.inner()).await.is_err());
        assert!(
            system_config::check_embedding_model(db.inner(), "source-model", 2, 128)
                .await
                .is_err()
        );
    }
    system_config::set_config(
        db.inner(),
        JOURNAL_KEY,
        &serde_json::to_string(&journal()).unwrap(),
    )
    .await
    .unwrap();
    assert!(
        system_config::check_embedding_model(db.inner(), "source-model", 2, 128)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_fact_checkpoints_cannot_both_replace_the_same_progress() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    begin_target_reembed(db.inner(), &journal()).await.unwrap();
    let first = ReembedJournal {
        reembedded_count: 1,
        last_fact_id: Some(RecordId::new("fact", "a")),
        ..journal()
    };
    let second = ReembedJournal {
        reembedded_count: 1,
        last_fact_id: Some(RecordId::new("fact", "b")),
        ..journal()
    };
    let (a, b) = tokio::join!(
        checkpoint_target_reembed(db.inner(), &first),
        checkpoint_target_reembed(db.inner(), &second)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let stored = read_target_journal(db.inner()).await.unwrap().unwrap();
    assert!(stored == first || stored == second);
}

#[tokio::test]
async fn completed_journal_cannot_be_served_under_a_different_full_identity() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    let current = verify_phase(&db).await;
    schema::ensure_vector_index(db.inner(), 768).await.unwrap();
    publish_target_space(db.inner(), &current, &current.target_space)
        .await
        .unwrap();
    for (key, value) in [
        (
            "embedding_space",
            serde_json::to_string(&current.source_space).unwrap(),
        ),
        ("embedding_model", "source-model".into()),
        ("embedding_dimensions", "2".into()),
        ("embedding_max_tokens", "128".into()),
    ] {
        system_config::set_config(db.inner(), key, &value)
            .await
            .unwrap();
    }
    assert!(
        system_config::check_embedding_model(db.inner(), "source-model", 2, 128)
            .await
            .is_err()
    );
    assert!(
        system_config::check_embedding_space(db.inner(), &current.source_space)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn invalid_initial_state_and_wrong_source_do_not_modify_any_key() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    let before = snapshot(&db).await;
    for changed in [
        ReembedJournal {
            phase: MigrationPhase::Complete,
            ..journal()
        },
        ReembedJournal {
            run_id: " ".into(),
            ..journal()
        },
        ReembedJournal {
            source_manifest_sha256: "bad".into(),
            ..journal()
        },
        ReembedJournal {
            source_space: EmbeddingSpace::Legacy {
                model: "wrong-source".into(),
                dimensions: 2,
                max_tokens: 128,
            },
            ..journal()
        },
        ReembedJournal {
            last_fact_id: Some(RecordId::new("session", "wrong-table")),
            reembedded_count: 1,
            ..journal()
        },
    ] {
        assert!(begin_target_reembed(db.inner(), &changed).await.is_err());
        assert_eq!(snapshot(&db).await, before);
    }
}

#[tokio::test]
async fn publish_requires_verify_matching_target_and_an_actual_hnsw_plan() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    let facts = journal();
    begin_target_reembed(db.inner(), &facts).await.unwrap();
    assert!(
        publish_target_space(db.inner(), &facts, &facts.target_space)
            .await
            .is_err()
    );
    let mut current = facts;
    current.phase = MigrationPhase::Centroids;
    checkpoint_target_reembed(db.inner(), &current)
        .await
        .unwrap();
    current.phase = MigrationPhase::Verify;
    checkpoint_target_reembed(db.inner(), &current)
        .await
        .unwrap();
    let before = snapshot(&db).await;
    assert!(
        publish_target_space(db.inner(), &current, &current.target_space)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&db).await, before);
    schema::ensure_vector_index(db.inner(), 768).await.unwrap();
    assert!(
        publish_target_space(db.inner(), &current, &current.source_space)
            .await
            .is_err()
    );
    let stale = ReembedJournal {
        run_id: "not-owner".into(),
        ..current.clone()
    };
    assert!(
        publish_target_space(db.inner(), &stale, &stale.target_space)
            .await
            .is_err()
    );
    publish_target_space(db.inner(), &current, &current.target_space)
        .await
        .unwrap();
    let mut complete = current.clone();
    complete.phase = MigrationPhase::Complete;
    assert_eq!(
        read_target_journal(db.inner()).await.unwrap(),
        Some(complete.clone())
    );
    assert_eq!(
        system_config::stored_embedding_space(db.inner())
            .await
            .unwrap(),
        Some(current.target_space.clone())
    );
    system_config::check_embedding_space(db.inner(), &current.target_space)
        .await
        .unwrap();
    assert!(
        checkpoint_target_reembed(db.inner(), &complete)
            .await
            .is_err()
    );
    assert!(
        system_config::check_embedding_model(db.inner(), "source-model", 2, 128)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_stale_hnsw_dimension_cannot_publish_an_empty_target_as_ready() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    let current = verify_phase(&db).await;
    schema::ensure_vector_index(db.inner(), 2).await.unwrap();
    let before = snapshot(&db).await;
    assert!(
        publish_target_space(db.inner(), &current, &current.target_space)
            .await
            .is_err(),
        "an empty index may have a KnnScan plan despite its old dimension"
    );
    assert_eq!(snapshot(&db).await, before);
    schema::drop_vector_index(db.inner()).await.unwrap();
    db.inner().query("DEFINE INDEX fact_embedding_hnsw ON fact FIELDS embedding HNSW DIMENSION 768 DISTANCE EUCLIDEAN")
        .await.unwrap().check().unwrap();
    assert!(
        publish_target_space(db.inner(), &current, &current.target_space)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&db).await, before);
    schema::drop_vector_index(db.inner()).await.unwrap();
    schema::ensure_vector_index(db.inner(), 768).await.unwrap();
    publish_target_space(db.inner(), &current, &current.target_space)
        .await
        .unwrap();
}

#[tokio::test]
async fn failed_final_transaction_preserves_all_incomplete_metadata() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    let current = verify_phase(&db).await;
    schema::ensure_vector_index(db.inner(), 768).await.unwrap();
    let before = snapshot(&db).await;
    db.inner().query("DEFINE FIELD OVERWRITE value ON system_config TYPE string ASSERT $value != 'ibm-granite/granite-embedding-311m-multilingual-r2'")
        .await.unwrap().check().unwrap();
    assert!(
        publish_target_space(db.inner(), &current, &current.target_space)
            .await
            .is_err()
    );
    assert_eq!(
        snapshot(&db).await,
        before,
        "the full identity and every compatibility key must roll back together"
    );
    assert!(
        system_config::check_embedding_model(db.inner(), "source-model", 2, 128)
            .await
            .is_err()
    );
    db.inner()
        .query("DEFINE FIELD OVERWRITE value ON system_config TYPE string")
        .await
        .unwrap()
        .check()
        .unwrap();
    publish_target_space(db.inner(), &current, &current.target_space)
        .await
        .unwrap();
    assert_eq!(
        system_config::stored_embedding_space(db.inner())
            .await
            .unwrap(),
        Some(current.target_space)
    );
}

#[tokio::test]
async fn a_completed_copy_can_begin_a_new_owned_run_but_an_incomplete_one_cannot() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    let current = verify_phase(&db).await;
    assert!(begin_target_reembed(db.inner(), &journal()).await.is_err());
    schema::ensure_vector_index(db.inner(), 768).await.unwrap();
    publish_target_space(db.inner(), &current, &current.target_space)
        .await
        .unwrap();
    let next = ReembedJournal {
        run_id: "new-copy-run".into(),
        source_manifest_sha256: "f".repeat(64),
        source_space: current.target_space,
        ..journal()
    };
    begin_target_reembed(db.inner(), &next).await.unwrap();
    assert_eq!(read_target_journal(db.inner()).await.unwrap(), Some(next));
}

#[tokio::test]
#[ignore = "parent test runs this in a child which exits without dropping its database"]
async fn abrupt_exit_writer_child() {
    let path =
        std::env::var_os("ALEXANDRIA_TEST_CRASH_TARGET").expect("parent-owned temporary target");
    let db = Database::connect_persistent(std::path::Path::new(&path))
        .await
        .unwrap();
    seed(&db).await;
    if std::env::var_os("ALEXANDRIA_TEST_CRASH_PUBLISH").is_some() {
        let current = verify_phase(&db).await;
        schema::ensure_vector_index(db.inner(), 768).await.unwrap();
        publish_target_space(db.inner(), &current, &current.target_space)
            .await
            .unwrap();
    } else {
        begin_target_reembed(db.inner(), &journal()).await.unwrap();
    }
    std::process::exit(0);
}

#[tokio::test]
async fn complete_identity_and_journal_survive_abrupt_process_exit_and_reconnect() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("isolated-ready-target");
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "abrupt_exit_writer_child",
            "--ignored",
            "--nocapture",
        ])
        .env("ALEXANDRIA_TEST_CRASH_TARGET", &target)
        .env("ALEXANDRIA_TEST_CRASH_PUBLISH", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let db = Database::connect_persistent(&target).await.unwrap();
    let mut complete = journal();
    complete.phase = MigrationPhase::Complete;
    assert_eq!(
        read_target_journal(db.inner()).await.unwrap(),
        Some(complete.clone())
    );
    assert_eq!(
        system_config::stored_embedding_space(db.inner())
            .await
            .unwrap(),
        Some(complete.target_space.clone())
    );
    system_config::check_embedding_space(db.inner(), &complete.target_space)
        .await
        .unwrap();
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_dimensions")
            .await
            .unwrap()
            .as_deref(),
        Some("768")
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_max_tokens")
            .await
            .unwrap()
            .as_deref(),
        Some("8192")
    );
}

#[tokio::test]
async fn committed_sentinel_and_journal_survive_abrupt_process_exit_and_reconnect() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("isolated-target");
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "abrupt_exit_writer_child",
            "--ignored",
            "--nocapture",
        ])
        .env("ALEXANDRIA_TEST_CRASH_TARGET", &target)
        .env_remove("ALEXANDRIA_TEST_CRASH_PUBLISH")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let db = Database::connect_persistent(&target).await.unwrap();
    assert_eq!(
        read_target_journal(db.inner()).await.unwrap(),
        Some(journal())
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_model")
            .await
            .unwrap()
            .as_deref(),
        Some(INCOMPLETE_MODEL)
    );
    assert!(
        system_config::check_embedding_model(db.inner(), "source-model", 2, 128)
            .await
            .is_err()
    );
    let mut resumed = journal();
    resumed.phase = MigrationPhase::Centroids;
    checkpoint_target_reembed(db.inner(), &resumed)
        .await
        .unwrap();
    assert_eq!(
        read_target_journal(db.inner()).await.unwrap(),
        Some(resumed)
    );
}
