use alexandria_storage::models::embedding::{EmbeddingSpace, GraniteSpace};
use alexandria_storage::{Database, schema, system_config};

fn granite() -> EmbeddingSpace {
    EmbeddingSpace::GraniteV1(Box::new(GraniteSpace {
        model: "ibm-granite/granite-embedding-311m-multilingual-r2".into(),
        revision: "44399559930365213510b1ee2eb15ded83374f0e".into(),
        onnx_sha256: "a".repeat(64),
        burnpack_sha256: "b".repeat(64),
        tokenizer_sha256: "c".repeat(64),
        pooling: "cls".into(),
        normalization: "l2-f64-accum-f32".into(),
        precision: "f32".into(),
        semantics_version: 1,
        dimensions: 768,
        max_tokens: 8192,
    }))
}

#[tokio::test]
async fn fresh_granite_store_stamps_full_and_compatible_identity() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    system_config::check_embedding_space(db.inner(), &granite())
        .await
        .unwrap();
    let stored = system_config::stored_embedding_space(db.inner())
        .await
        .unwrap();
    assert_eq!(stored, Some(granite()));
    for (key, expected) in [
        (
            "embedding_model",
            "ibm-granite/granite-embedding-311m-multilingual-r2",
        ),
        ("embedding_dimensions", "768"),
        ("embedding_max_tokens", "8192"),
    ] {
        assert_eq!(
            system_config::get_config(db.inner(), key)
                .await
                .unwrap()
                .as_deref(),
            Some(expected)
        );
    }
}

#[tokio::test]
async fn granite_identity_binds_all_semantics() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    system_config::check_embedding_space(db.inner(), &granite())
        .await
        .unwrap();
    let original = serde_json::to_value(granite()).unwrap();
    let changes = [
        ("model", serde_json::json!("other-model")),
        ("revision", serde_json::json!("d".repeat(40))),
        ("onnx_sha256", serde_json::json!("d".repeat(64))),
        ("burnpack_sha256", serde_json::json!("d".repeat(64))),
        ("tokenizer_sha256", serde_json::json!("d".repeat(64))),
        ("pooling", serde_json::json!("mean")),
        ("normalization", serde_json::json!("none")),
        ("precision", serde_json::json!("f16")),
        ("semantics_version", serde_json::json!(2)),
        ("dimensions", serde_json::json!(384)),
        ("max_tokens", serde_json::json!(4096)),
    ];
    for (key, replacement) in changes {
        let mut changed = original.clone();
        changed[key] = replacement;
        let changed: EmbeddingSpace = serde_json::from_value(changed).unwrap();
        assert!(
            system_config::check_embedding_space(db.inner(), &changed)
                .await
                .is_err(),
            "{key} must not share the existing embedding space"
        );
        assert_eq!(
            system_config::stored_embedding_space(db.inner())
                .await
                .unwrap(),
            Some(granite()),
            "rejecting {key} must leave identity unchanged"
        );
    }
}

#[tokio::test]
async fn unlabelled_centroids_cannot_be_relabelled_granite() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    alexandria_storage::repos::ClusterRepo::new(db.inner())
        .create(Some("legacy cluster"), &[0.1_f32; 384])
        .await
        .unwrap();
    assert!(
        system_config::check_embedding_space(db.inner(), &granite())
            .await
            .is_err(),
        "a fact-free store can still contain centroids from another model"
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_model")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn legacy_api_cannot_ignore_conflicting_full_identity() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    system_config::check_embedding_space(db.inner(), &granite())
        .await
        .unwrap();
    for (key, value) in [
        ("embedding_model", "legacy-model"),
        ("embedding_dimensions", "384"),
        ("embedding_max_tokens", "256"),
    ] {
        system_config::set_config(db.inner(), key, value)
            .await
            .unwrap();
    }
    assert!(
        system_config::check_embedding_model(db.inner(), "legacy-model", 384, 256)
            .await
            .is_err(),
        "the legacy entry point cannot bypass a versioned identity"
    );
}

#[test]
fn device_not_part_of_space() {
    let cpu = serde_json::to_value(granite()).unwrap();
    assert!(cpu.get("device").is_none());
    let mut with_device = cpu;
    with_device["device"] = serde_json::json!("rocm:0");
    assert!(serde_json::from_value::<EmbeddingSpace>(with_device).is_err());
}

#[tokio::test]
async fn legacy_keys_do_not_invent_revision() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    system_config::check_embedding_model(db.inner(), "legacy-model", 384, 256)
        .await
        .unwrap();
    let stored = system_config::stored_embedding_space(db.inner())
        .await
        .unwrap()
        .unwrap();
    let serialized = serde_json::to_value(stored).unwrap();
    assert_eq!(serialized["kind"], "legacy");
    assert_eq!(serialized["model"], "legacy-model");
    assert_eq!(serialized["max_tokens"], 256);
    assert!(serialized.get("revision").is_none());
    assert!(serialized.get("tokenizer_sha256").is_none());
}

#[tokio::test]
async fn legacy_prelock_limit_stays128() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    system_config::set_config(db.inner(), "embedding_model", "legacy-model")
        .await
        .unwrap();
    system_config::set_config(db.inner(), "embedding_dimensions", "384")
        .await
        .unwrap();
    assert_eq!(
        system_config::stored_embedding_space(db.inner())
            .await
            .unwrap(),
        Some(EmbeddingSpace::Legacy {
            model: "legacy-model".into(),
            dimensions: 384,
            max_tokens: 128,
        })
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_max_tokens")
            .await
            .unwrap(),
        None,
        "reading legacy metadata must not stamp a different token limit"
    );
}

#[tokio::test]
async fn full_identity_cannot_relabel_unlocked_or_partial_corpora() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    let repo = alexandria_storage::repos::MemoryRepo::new(db.inner());
    repo.create_fact("legacy fact", 1.0, &[0.1_f32; 384], &[])
        .await
        .unwrap();
    assert!(
        system_config::check_embedding_space(db.inner(), &granite())
            .await
            .is_err()
    );
    system_config::set_config(db.inner(), "embedding_model", "legacy-model")
        .await
        .unwrap();
    assert!(
        system_config::check_embedding_space(db.inner(), &granite())
            .await
            .is_err()
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_model")
            .await
            .unwrap()
            .as_deref(),
        Some("legacy-model")
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_space")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn malformed_or_inconsistent_stored_identity_refuses_boot() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    system_config::check_embedding_space(db.inner(), &granite())
        .await
        .unwrap();
    system_config::set_config(db.inner(), "embedding_dimensions", "384")
        .await
        .unwrap();
    assert!(
        system_config::check_embedding_space(db.inner(), &granite())
            .await
            .is_err()
    );
    system_config::set_config(db.inner(), "embedding_space", "{broken-json")
        .await
        .unwrap();
    assert!(
        system_config::stored_embedding_space(db.inner())
            .await
            .is_err()
    );
}
