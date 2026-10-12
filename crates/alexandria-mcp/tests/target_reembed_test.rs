use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use alexandria_mcp::migrate::{ReembedOutcome, reembed_target};
use alexandria_pipeline::embedding::{EmbeddingProvider, LoadedEmbedding};
use alexandria_storage::embedding_state::{begin_target_reembed, read_target_journal};
use alexandria_storage::embedding_validation::{BASELINE_KEY, validate_target_metadata};
use alexandria_storage::models::embedding::{
    EmbeddingSpace, GRANITE_MODEL_ID, GraniteSpace, MigrationPhase, ReembedJournal,
};
use alexandria_storage::repos::{ClusterRepo, MemoryRepo};
use alexandria_storage::{Database, schema, system_config};
use surrealdb::types::{Array, Object, RecordId, ToSql, Uuid};

fn space() -> EmbeddingSpace {
    EmbeddingSpace::GraniteV1(Box::new(GraniteSpace {
        model: GRANITE_MODEL_ID.into(),
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
    }))
}

fn journal() -> ReembedJournal {
    ReembedJournal {
        run_id: "target-fixture".into(),
        source_manifest_sha256: "a".repeat(64),
        source_space: EmbeddingSpace::Legacy {
            model: "source".into(),
            dimensions: 2,
            max_tokens: 128,
        },
        target_space: space(),
        phase: MigrationPhase::Facts,
        last_fact_id: None,
        reembedded_count: 0,
    }
}

#[derive(Default)]
struct Probe {
    fail_on: Option<String>,
    invalid: Option<&'static str>,
    calls: Mutex<Vec<String>>,
}

fn vector(text: &str) -> Vec<f32> {
    let index = match text {
        "one" => 0,
        "two" => 1,
        "gone" => 2,
        "quarantine" => 3,
        _ => 4,
    };
    let mut vector = vec![0.0; 768];
    vector[index] = 1.0;
    vector
}

#[async_trait::async_trait]
impl EmbeddingProvider for Probe {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        assert_eq!(texts.len(), 1, "migration inference stays microbatch1");
        self.calls.lock().unwrap().push(texts[0].into());
        anyhow::ensure!(
            self.fail_on.as_deref() != Some(texts[0]),
            "injected encoder failure"
        );
        let mut output = vector(texts[0]);
        match self.invalid {
            Some("dimension") => output.truncate(2),
            Some("finite") => output[0] = f32::NAN,
            Some("unit") => output[0] = 10.0,
            Some("count") => return Ok(vec![]),
            _ => {}
        }
        Ok(vec![output])
    }
    fn dimensions(&self) -> usize {
        768
    }
    fn model_id(&self) -> &str {
        GRANITE_MODEL_ID
    }
}

fn loaded(probe: Arc<Probe>) -> LoadedEmbedding {
    LoadedEmbedding {
        provider: probe,
        space: space(),
        effective_max_tokens: 8192,
    }
}

async fn seed(db: &Database) {
    schema::migrate(db.inner()).await.unwrap();
    db.inner()
        .query("DEFINE FIELD metadata.untouched ON fact TYPE int")
        .await
        .unwrap()
        .check()
        .unwrap();
    system_config::check_embedding_space(db.inner(), &journal().source_space)
        .await
        .unwrap();
    for (key, text, deleted, quarantine) in [
        ("a", "one", false, false),
        ("b", "two", false, false),
        ("c", "gone", true, false),
        ("é:quote'", "quarantine", false, true),
    ] {
        db.inner().query("CREATE $id SET content = $text, confidence = 0.6, embedding = [1.0, 0.0], tags = ['public'], metadata = { untouched: 42 }, deleted = $deleted, quarantined_at = $quarantine")
            .bind(("id", RecordId::new("fact", key))).bind(("text", text)).bind(("deleted", deleted))
            .bind(("quarantine", quarantine.then(chrono::Utc::now))).await.unwrap().check().unwrap();
    }
    db.inner().query("CREATE cluster:a SET label = 'first', centroid = [1.0, 0.0]; CREATE cluster:b SET label = 'second', centroid = [1.0, 0.0]; CREATE cluster:empty SET label = 'empty', centroid = [1.0, 0.0]")
        .await.unwrap().check().unwrap();
    let clusters = ClusterRepo::new(db.inner());
    for key in ["a", "b", "é:quote'"] {
        clusters
            .add_member(
                "cluster:a",
                &alexandria_storage::record_id_to_string(&RecordId::new("fact", key)),
            )
            .await
            .unwrap();
    }
    clusters.add_member("cluster:b", "fact:a").await.unwrap();
    db.inner().query("CREATE raw:r SET content = 'public original'; CREATE session:s SET external_id = 'public-session', summary = 'preserved'; CREATE reminder:r SET message = 'public reminder', schedule_kind = 'once', next_due_at = d'2030-01-01T00:00:00Z'; CREATE heat_state:h SET memory = fact:a, heat = 0.2, stability = 7.0, access_count = 3; RELATE fact:c->extracted_from->raw:r; RELATE session:s->contains_session_memory->fact:c; RELATE fact:c->memory_edge->fact:a SET edge_type = 'derived_from'; CREATE provenance:p SET kind = 'test'; RELATE fact:a->has_provenance->provenance:p")
        .await.unwrap().check().unwrap();
    schema::ensure_vector_index(db.inner(), 2).await.unwrap();
}

async fn fixture() -> (Database, ReembedJournal) {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    let current = journal();
    begin_target_reembed(db.inner(), &current).await.unwrap();
    (db, current)
}

async fn blocked(db: &Database) {
    assert!(
        system_config::check_embedding_space(db.inner(), &space())
            .await
            .is_err()
    );
    assert!(
        system_config::check_embedding_model(db.inner(), "source", 2, 128)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rewrites_every_fact_preserves_metadata_recomputes_live_centroids_and_builds_index() {
    let (db, current) = fixture().await;
    let probe = Arc::new(Probe::default());
    let original_baseline = system_config::get_config(db.inner(), BASELINE_KEY)
        .await
        .unwrap();
    let outcome = reembed_target(&db, &loaded(probe.clone()), 2, current.clone())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        ReembedOutcome::Done {
            facts: 4,
            clusters: 2
        }
    ));
    assert_eq!(
        probe.calls.lock().unwrap().as_slice(),
        ["one", "two", "gone", "quarantine"]
    );
    for fact in MemoryRepo::new(db.inner())
        .reembed_page(None, 10)
        .await
        .unwrap()
    {
        let record = MemoryRepo::new(db.inner())
            .get_fact(&fact.id.to_sql())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.embedding, vector(&fact.content));
    }
    let clusters = ClusterRepo::new(db.inner());
    assert!(clusters.get("cluster:empty").await.unwrap().is_none());
    let centroid = clusters.get("cluster:a").await.unwrap().unwrap().centroid;
    assert_eq!(
        &centroid[..4],
        &[0.5, 0.5, 0.0, 0.0],
        "hidden members do not change live centroid semantics"
    );
    validate_target_metadata(db.inner(), &current)
        .await
        .unwrap();
    assert_eq!(
        system_config::get_config(db.inner(), BASELINE_KEY)
            .await
            .unwrap(),
        original_baseline
    );
    assert_eq!(
        read_target_journal(db.inner())
            .await
            .unwrap()
            .unwrap()
            .phase,
        MigrationPhase::Complete
    );
    system_config::check_embedding_space(db.inner(), &space())
        .await
        .unwrap();
    let indexed = MemoryRepo::new(db.inner())
        .nearest_indexed(&vector("one"), 1)
        .await
        .unwrap();
    let brute = MemoryRepo::new(db.inner())
        .nearest(&vector("one"), 1)
        .await
        .unwrap();
    assert_eq!(indexed[0].id, brute[0].id);
    assert_eq!(indexed[0].content, "one");
}

#[tokio::test]
async fn partial_group_replays_from_last_committed_cursor_before_resume() {
    let (db, current) = fixture().await;
    let failing = Arc::new(Probe {
        fail_on: Some("two".into()),
        ..Default::default()
    });
    assert!(
        reembed_target(&db, &loaded(failing), 2, current.clone())
            .await
            .is_err()
    );
    let stored = read_target_journal(db.inner()).await.unwrap().unwrap();
    assert_eq!(stored, current);
    assert_eq!(
        MemoryRepo::new(db.inner())
            .get_fact("fact:a")
            .await
            .unwrap()
            .unwrap()
            .embedding
            .len(),
        768
    );
    assert_eq!(
        MemoryRepo::new(db.inner())
            .get_fact("fact:b")
            .await
            .unwrap()
            .unwrap()
            .embedding
            .len(),
        2
    );
    blocked(&db).await;
    let probe = Arc::new(Probe::default());
    reembed_target(&db, &loaded(probe.clone()), 2, stored)
        .await
        .unwrap();
    assert_eq!(
        probe.calls.lock().unwrap()[0],
        "one",
        "uncheckpointed first write is replayed"
    );
}

#[tokio::test]
async fn later_partial_group_preserves_committed_progress_and_skips_prior_facts() {
    let (db, current) = fixture().await;
    let failing = Arc::new(Probe {
        fail_on: Some("quarantine".into()),
        ..Default::default()
    });
    assert!(
        reembed_target(&db, &loaded(failing), 2, current)
            .await
            .is_err()
    );
    let pending = read_target_journal(db.inner()).await.unwrap().unwrap();
    assert_eq!(pending.phase, MigrationPhase::Facts);
    assert_eq!(pending.reembedded_count, 2);
    assert_eq!(pending.last_fact_id, Some(RecordId::new("fact", "b")));
    let probe = Arc::new(Probe::default());
    reembed_target(&db, &loaded(probe.clone()), 2, pending)
        .await
        .unwrap();
    assert_eq!(
        probe.calls.lock().unwrap().as_slice(),
        ["gone", "quarantine"]
    );
}

#[tokio::test]
async fn numeric_ids_survive_typed_vector_writes_and_completed_retry_is_noop() {
    let db = Database::connect_embedded().await.unwrap();
    seed(&db).await;
    db.inner()
        .query("CREATE $id SET content = 'number', embedding = [1.0, 0.0]")
        .bind(("id", RecordId::new("fact", 7)))
        .await
        .unwrap()
        .check()
        .unwrap();
    let current = journal();
    begin_target_reembed(db.inner(), &current).await.unwrap();
    let provider = Arc::new(Probe::default());
    reembed_target(&db, &loaded(provider.clone()), 2, current)
        .await
        .unwrap();
    assert_eq!(
        MemoryRepo::new(db.inner())
            .get_fact("fact:7")
            .await
            .unwrap()
            .unwrap()
            .embedding,
        vector("number")
    );
    let count = provider.calls.lock().unwrap().len();
    let complete = read_target_journal(db.inner()).await.unwrap().unwrap();
    assert!(matches!(
        reembed_target(&db, &loaded(provider.clone()), 2, complete)
            .await
            .unwrap(),
        ReembedOutcome::Skipped(_)
    ));
    assert_eq!(provider.calls.lock().unwrap().len(), count);
}

#[tokio::test]
async fn mixed_key_checkpoints_resume_without_skipping_or_replaying_committed_facts() {
    let records = [
        (RecordId::new("fact", 7), "one"),
        (RecordId::new("fact", "é:quote'"), "two"),
        (
            RecordId::new(
                "fact",
                "00000000-0000-4000-8000-000000000001"
                    .parse::<Uuid>()
                    .unwrap(),
            ),
            "gone",
        ),
        (
            RecordId::new("fact", Array::from(vec![1_i64, 2])),
            "quarantine",
        ),
        (
            RecordId::new("fact", Object::from(BTreeMap::from([("key", 7_i64)]))),
            "object",
        ),
    ];
    for failure_index in 2..records.len() {
        let db = Database::connect_embedded().await.unwrap();
        schema::migrate(db.inner()).await.unwrap();
        system_config::check_embedding_space(db.inner(), &journal().source_space)
            .await
            .unwrap();
        for (id, content) in &records {
            db.inner().query("CREATE $id SET content = $content, embedding = [1.0, 0.0], tags = ['mixed-key'], deleted = $deleted")
                .bind(("id", id.clone())).bind(("content", *content))
                .bind(("deleted", *content == "quarantine"))
                .await.unwrap().check().unwrap();
        }
        let current = journal();
        begin_target_reembed(db.inner(), &current).await.unwrap();
        let failing = Arc::new(Probe {
            fail_on: Some(records[failure_index].1.into()),
            ..Probe::default()
        });
        let error = reembed_target(&db, &loaded(failing.clone()), 1, current)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("injected encoder failure"),
            "{error:#}"
        );
        let pending = read_target_journal(db.inner()).await.unwrap().unwrap();
        assert_eq!(pending.reembedded_count, failure_index);
        assert_eq!(
            pending.last_fact_id,
            Some(records[failure_index - 1].0.clone())
        );
        assert_eq!(
            *failing.calls.lock().unwrap(),
            records[..=failure_index]
                .iter()
                .map(|(_, text)| text.to_string())
                .collect::<Vec<_>>()
        );
        let resumed = Arc::new(Probe::default());
        assert!(matches!(
            reembed_target(&db, &loaded(resumed.clone()), 1, pending)
                .await
                .unwrap(),
            ReembedOutcome::Done {
                facts: 5,
                clusters: 0
            }
        ));
        assert_eq!(
            *resumed.calls.lock().unwrap(),
            records[failure_index..]
                .iter()
                .map(|(_, text)| text.to_string())
                .collect::<Vec<_>>()
        );
        for (id, content) in &records {
            let fact: Option<alexandria_storage::models::Fact> =
                db.inner().select(id.clone()).await.unwrap();
            let fact = fact.unwrap();
            assert_eq!(fact.embedding, vector(content));
            assert_eq!(fact.tags, ["mixed-key"]);
            assert_eq!(fact.deleted, *content == "quarantine");
        }
        let complete = read_target_journal(db.inner()).await.unwrap().unwrap();
        assert_eq!(
            complete.last_fact_id,
            Some(records.last().unwrap().0.clone())
        );
        let retry = Arc::new(Probe::default());
        assert!(matches!(
            reembed_target(&db, &loaded(retry.clone()), 1, complete)
                .await
                .unwrap(),
            ReembedOutcome::Skipped(_)
        ));
        assert!(retry.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn invalid_provider_outputs_fail_before_any_fact_write() {
    for invalid in ["dimension", "finite", "unit", "count"] {
        let (db, current) = fixture().await;
        let probe = Arc::new(Probe {
            invalid: Some(invalid),
            ..Default::default()
        });
        assert!(
            reembed_target(&db, &loaded(probe), 2, current.clone())
                .await
                .is_err()
        );
        assert_eq!(
            MemoryRepo::new(db.inner())
                .get_fact("fact:a")
                .await
                .unwrap()
                .unwrap()
                .embedding
                .len(),
            2,
            "{invalid}"
        );
        assert_eq!(
            read_target_journal(db.inner()).await.unwrap(),
            Some(current)
        );
        blocked(&db).await;
    }
}

#[tokio::test]
async fn centroid_write_failure_leaves_resumable_centroids_not_ready() {
    let (db, current) = fixture().await;
    db.inner().query("DEFINE FIELD OVERWRITE centroid ON cluster TYPE array<float> ASSERT $this.label != 'second' OR array::len($value) = 2").await.unwrap().check().unwrap();
    assert!(
        reembed_target(&db, &loaded(Arc::new(Probe::default())), 2, current)
            .await
            .is_err()
    );
    let stored = read_target_journal(db.inner()).await.unwrap().unwrap();
    assert_eq!(stored.phase, MigrationPhase::Centroids);
    assert_eq!(
        ClusterRepo::new(db.inner())
            .get("cluster:a")
            .await
            .unwrap()
            .unwrap()
            .centroid
            .len(),
        768
    );
    assert_eq!(
        ClusterRepo::new(db.inner())
            .get("cluster:b")
            .await
            .unwrap()
            .unwrap()
            .centroid
            .len(),
        2
    );
    blocked(&db).await;
    db.inner()
        .query("DEFINE FIELD OVERWRITE centroid ON cluster TYPE array<float>")
        .await
        .unwrap()
        .check()
        .unwrap();
    let probe = Arc::new(Probe::default());
    reembed_target(&db, &loaded(probe.clone()), 2, stored)
        .await
        .unwrap();
    assert!(
        probe.calls.lock().unwrap().is_empty(),
        "completed facts are not inferred again"
    );
}

#[tokio::test]
async fn publication_failure_after_index_rebuild_keeps_verify_checkpoint_and_atomic_old_identity() {
    let (db, current) = fixture().await;
    db.inner().query("DEFINE FIELD OVERWRITE value ON system_config TYPE string ASSERT $this.key != 'embedding_space' OR !string::contains($value, 'granite_v1')").await.unwrap().check().unwrap();
    assert!(
        reembed_target(&db, &loaded(Arc::new(Probe::default())), 2, current)
            .await
            .is_err()
    );
    let stored = read_target_journal(db.inner()).await.unwrap().unwrap();
    assert_eq!(stored.phase, MigrationPhase::Verify);
    assert_eq!(stored.reembedded_count, 4);
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_dimensions")
            .await
            .unwrap()
            .as_deref(),
        Some("2")
    );
    assert_eq!(
        MemoryRepo::new(db.inner())
            .nearest_indexed(&vector("one"), 1)
            .await
            .unwrap()[0]
            .content,
        "one"
    );
    blocked(&db).await;
    db.inner()
        .query("DEFINE FIELD OVERWRITE value ON system_config TYPE string")
        .await
        .unwrap()
        .check()
        .unwrap();
    reembed_target(&db, &loaded(Arc::new(Probe::default())), 2, stored)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "parent runs isolated persistent stores and exits without database drop"]
async fn abrupt_target_writer_child() {
    let path = std::env::var_os("ALEXANDRIA_TARGET_TEST_PATH").expect("parent-owned target path");
    let mode = std::env::var("ALEXANDRIA_TARGET_TEST_MODE").expect("child mode");
    let db = Database::connect_persistent(std::path::Path::new(&path))
        .await
        .unwrap();
    if mode == "seed" {
        seed(&db).await;
    } else {
        let mut current = journal();
        let encoded = std::fs::read(
            alexandria_storage::offline_copy::source_manifest_path(std::path::Path::new(&path))
                .unwrap(),
        )
        .unwrap();
        let manifest: alexandria_storage::offline_copy::SourceManifest =
            serde_json::from_slice(&encoded).unwrap();
        manifest.validate().unwrap();
        current.source_manifest_sha256 = manifest.sha256;
        begin_target_reembed(db.inner(), &current).await.unwrap();
        let failing = Arc::new(Probe {
            fail_on: Some("two".into()),
            ..Default::default()
        });
        assert!(
            reembed_target(&db, &loaded(failing), 2, current)
                .await
                .is_err()
        );
    }
    std::process::exit(0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn persistent_copy_recovers_partial_group_and_preserves_every_original_source_byte() {
    fn run(mode: &str, path: &std::path::Path) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "abrupt_target_writer_child",
                "--ignored",
                "--nocapture",
            ])
            .env("ALEXANDRIA_TARGET_TEST_PATH", path)
            .env("ALEXANDRIA_TARGET_TEST_MODE", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn payload(root: &std::path::Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn walk(
            root: &std::path::Path,
            directory: &std::path::Path,
            files: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path == root.join("LOCK") {
                    continue;
                }
                if path.is_dir() {
                    walk(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root).unwrap().into(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut files = Default::default();
        walk(root, root, &mut files);
        files
    }
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let target = directory.path().join("target");
    run("seed", &source);
    let original = payload(&source);
    let guard = alexandria_storage::offline_copy::OfflineStoreGuard::acquire(&source).unwrap();
    let receipt = guard.copy_to(&target).unwrap();
    run("interrupt", &target);
    let db = Database::connect_persistent(&target).await.unwrap();
    let current = read_target_journal(db.inner()).await.unwrap().unwrap();
    assert_eq!(current.source_manifest_sha256, receipt.sha256);
    assert_eq!(current.reembedded_count, 0);
    let original_baseline = system_config::get_config(db.inner(), BASELINE_KEY)
        .await
        .unwrap();
    assert!(original_baseline.is_some());
    assert_eq!(
        MemoryRepo::new(db.inner())
            .get_fact("fact:a")
            .await
            .unwrap()
            .unwrap()
            .embedding
            .len(),
        768
    );
    blocked(&db).await;
    reembed_target(&db, &loaded(Arc::new(Probe::default())), 2, current.clone())
        .await
        .unwrap();
    validate_target_metadata(db.inner(), &current)
        .await
        .unwrap();
    assert_eq!(
        system_config::get_config(db.inner(), BASELINE_KEY)
            .await
            .unwrap(),
        original_baseline
    );
    assert_eq!(payload(&source), original);
    system_config::check_embedding_space(db.inner(), &space())
        .await
        .unwrap();
}

#[tokio::test]
async fn short_member_vector_refuses_centroid_resume_instead_of_zip_truncation() {
    let (db, current) = fixture().await;
    db.inner().query("DEFINE FIELD OVERWRITE centroid ON cluster TYPE array<float> ASSERT $this.label != 'second' OR array::len($value) = 2").await.unwrap().check().unwrap();
    assert!(
        reembed_target(&db, &loaded(Arc::new(Probe::default())), 2, current)
            .await
            .is_err()
    );
    let pending = read_target_journal(db.inner()).await.unwrap().unwrap();
    assert_eq!(pending.phase, MigrationPhase::Centroids);
    MemoryRepo::new(db.inner())
        .write_reembed_embedding(&RecordId::new("fact", "a"), &[1.0, 0.0])
        .await
        .unwrap();
    assert!(
        reembed_target(&db, &loaded(Arc::new(Probe::default())), 2, pending)
            .await
            .is_err()
    );
    blocked(&db).await;
}

#[tokio::test]
async fn a_wrong_distance_index_cannot_mark_a_resumed_target_ready() {
    let (db, current) = fixture().await;
    db.inner().query("DEFINE FIELD OVERWRITE value ON system_config TYPE string ASSERT $this.key != 'embedding_space' OR !string::contains($value, 'granite_v1')").await.unwrap().check().unwrap();
    assert!(
        reembed_target(&db, &loaded(Arc::new(Probe::default())), 2, current)
            .await
            .is_err()
    );
    let pending = read_target_journal(db.inner()).await.unwrap().unwrap();
    assert_eq!(pending.phase, MigrationPhase::Verify);
    db.inner().query("DEFINE FIELD OVERWRITE value ON system_config TYPE string; REMOVE INDEX fact_embedding_hnsw ON fact; DEFINE INDEX fact_embedding_hnsw ON fact FIELDS embedding HNSW DIMENSION 768 DISTANCE EUCLIDEAN").await.unwrap().check().unwrap();
    let error = reembed_target(&db, &loaded(Arc::new(Probe::default())), 2, pending.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("distance"));
    blocked(&db).await;
    schema::drop_vector_index(db.inner()).await.unwrap();
    reembed_target(&db, &loaded(Arc::new(Probe::default())), 2, pending)
        .await
        .unwrap();
}

#[tokio::test]
async fn completed_target_query_tester_and_real_retrieval_agree() {
    use alexandria_mcp::server::{AlexandriaServer, HeatSettings};
    use alexandria_mcp::tools::RetrieveMemoriesParams;
    let (db, current) = fixture().await;
    let provider = Arc::new(Probe::default());
    reembed_target(&db, &loaded(provider.clone()), 2, current)
        .await
        .unwrap();
    let server = AlexandriaServer::new(Arc::new(db), provider, 0.8, HeatSettings::default())
        .with_vector_index(true);
    let params = || RetrieveMemoriesParams {
        query: "one".into(),
        limit: Some(1),
        session_id: None,
    };
    let dry = server.do_retrieve_memories_dry(params()).await.unwrap();
    let real = server.do_retrieve_memories(params()).await.unwrap();
    assert_eq!(dry["results"][0]["id"], real["results"][0]["id"]);
    assert_eq!(real["results"][0]["content"], "one");
}

#[tokio::test]
async fn changed_metadata_and_owned_identity_refuse_resume() {
    let (db, current) = fixture().await;
    db.inner()
        .query("UPDATE reminder:r SET message = 'tampered'")
        .await
        .unwrap()
        .check()
        .unwrap();
    let probe = Arc::new(Probe::default());
    assert!(
        reembed_target(&db, &loaded(probe.clone()), 2, current.clone())
            .await
            .is_err()
    );
    assert!(probe.calls.lock().unwrap().is_empty());
    let mut wrong = current;
    wrong.run_id = "other-owner".into();
    assert!(
        reembed_target(&db, &loaded(probe.clone()), 2, wrong)
            .await
            .is_err()
    );
    blocked(&db).await;
}
