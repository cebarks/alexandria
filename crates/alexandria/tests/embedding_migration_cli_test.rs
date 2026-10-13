use alexandria_storage::models::embedding::GRANITE_MODEL_ID;
use std::path::Path;
use std::process::{Command, Output};

fn command(root: &Path, source: &Path, args: &[&str]) -> Output {
    let config = root.join("config.toml");
    std::fs::write(&config, "").unwrap();
    Command::new(env!("CARGO_BIN_EXE_alexandria"))
        .args(args)
        .env("ALEXANDRIA_CONFIG", config)
        .env("ALEXANDRIA_DATA_DIR", source)
        .env("ALEXANDRIA_EMBEDDING_MODEL", GRANITE_MODEL_ID)
        .env("ALEXANDRIA_EMBEDDING_DEVICE", "cpu")
        .env("ALEXANDRIA_EMBEDDING_MAX_TOKENS", "8192")
        .env("ALEXANDRIA_EMBEDDING_BATCH_SIZE", "2")
        .env(
            "ALEXANDRIA_EMBEDDING_ARTIFACT_DIR",
            root.join("missing-artifacts"),
        )
        .env("XDG_CACHE_HOME", root.join("cache"))
        .output()
        .unwrap()
}

#[tokio::test]
#[ignore = "parent seeds isolated stores and ends the child process without database drop"]
async fn fixture_child() {
    use alexandria_storage::models::embedding::{EmbeddingSpace, MigrationPhase, ReembedJournal};
    use alexandria_storage::{Database, schema, system_config};
    let path = std::env::var_os("ALEXANDRIA_CLI_FIXTURE_PATH").unwrap();
    let mode = std::env::var("ALEXANDRIA_CLI_FIXTURE_MODE").unwrap();
    let db = Database::connect_persistent(Path::new(&path))
        .await
        .unwrap();
    if mode == "seed" {
        schema::migrate(db.inner()).await.unwrap();
        let source = EmbeddingSpace::Legacy {
            model: "public-legacy".into(),
            dimensions: 2,
            max_tokens: 128,
        };
        system_config::check_embedding_space(db.inner(), &source)
            .await
            .unwrap();
        alexandria_storage::repos::MemoryRepo::new(db.inner())
            .create_fact("public one", 0.6, &[1.0, 0.0], &[])
            .await
            .unwrap();
    } else if mode == "duplicate" {
        alexandria_storage::repos::MemoryRepo::new(db.inner())
            .create_fact("public one", 0.6, &[1.0, 0.0], &[])
            .await
            .unwrap();
    } else if mode == "lower-schema-version" {
        system_config::set_config(db.inner(), "schema_version", "0")
            .await
            .unwrap();
    } else if mode == "corrupt-journal" {
        system_config::set_config(
            db.inner(),
            alexandria_storage::embedding_state::JOURNAL_KEY,
            "{broken",
        )
        .await
        .unwrap();
    } else if mode == "repair-marker" {
        system_config::set_config(db.inner(), "embedding_model", "public-legacy")
            .await
            .unwrap();
    } else if mode == "inspect" {
        println!(
            "ARMED={}",
            system_config::get_config(db.inner(), system_config::ACCESS_RECORDING_ARMED_AT)
                .await
                .unwrap()
                .is_some()
        );
        println!(
            "LIVE={}",
            alexandria_storage::repos::MemoryRepo::new(db.inner())
                .count(&alexandria_storage::repos::FactListQuery::default())
                .await
                .unwrap()
        );
        println!(
            "AUDIT={}",
            alexandria_storage::repos::ClusterRepo::new(db.inner())
                .count_maintenance_logs(None)
                .await
                .unwrap()
        );
        println!(
            "SCHEMA={}",
            system_config::get_config(db.inner(), "schema_version")
                .await
                .unwrap()
                .unwrap()
        );
    } else {
        struct Encoder;
        #[async_trait::async_trait]
        impl alexandria_pipeline::embedding::EmbeddingProvider for Encoder {
            async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
                assert_eq!(texts.len(), 1);
                let mut vector = vec![0.0; 768];
                vector[0] = 1.0;
                Ok(vec![vector])
            }
            fn dimensions(&self) -> usize {
                768
            }
            fn model_id(&self) -> &str {
                GRANITE_MODEL_ID
            }
        }
        let receipt =
            alexandria_storage::offline_copy::read_source_manifest(Path::new(&path)).unwrap();
        let target =
            alexandria_pipeline::embedding::granite::artifacts::ArtifactManifest::compiled()
                .unwrap()
                .space;
        let journal = ReembedJournal {
            run_id: "public-cli-fixture".into(),
            source_manifest_sha256: receipt.sha256,
            source_space: system_config::stored_embedding_space(db.inner())
                .await
                .unwrap()
                .unwrap(),
            target_space: target.clone(),
            phase: MigrationPhase::Facts,
            last_fact_id: None,
            reembedded_count: 0,
        };
        alexandria_storage::embedding_state::begin_target_reembed(db.inner(), &journal)
            .await
            .unwrap();
        if mode == "complete" {
            let loaded = alexandria_pipeline::embedding::LoadedEmbedding {
                provider: std::sync::Arc::new(Encoder),
                space: target,
                effective_max_tokens: 8192,
            };
            alexandria_mcp::migrate::reembed_target(&db, &loaded, 2, journal)
                .await
                .unwrap();
        }
    }
    std::process::exit(0);
}

fn fixture_process(mode: &str, path: &Path) -> Output {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fixture_child", "--ignored", "--nocapture"])
        .env("ALEXANDRIA_CLI_FIXTURE_PATH", path)
        .env("ALEXANDRIA_CLI_FIXTURE_MODE", mode)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[cfg(target_os = "linux")]
fn copied_fixture(root: &Path, mode: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let source = root.join("source");
    let target = root.join("target");
    fixture_process("seed", &source);
    let guard = alexandria_storage::offline_copy::OfflineStoreGuard::acquire(&source).unwrap();
    guard.copy_to(&target).unwrap();
    fixture_process(mode, &target);
    (source, target)
}

#[cfg(target_os = "linux")]
#[test]
fn embedding_migration_cli_completed_resume_reports_without_any_artifacts_or_rewrite() {
    let root = tempfile::tempdir().unwrap();
    let (source, target) = copied_fixture(root.path(), "complete");
    let output = command(
        root.path(),
        &source,
        &[
            "migrate-embeddings",
            "--output-data-dir",
            target.to_str().unwrap(),
            "--resume",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["fact_count"], 1);
    assert_eq!(report["index_verified"], true);
    assert_eq!(report["target_space"]["kind"], "granite_v1");
    assert!(String::from_utf8_lossy(&output.stderr).contains("manual"));
    assert!(!root.path().join("cache").exists());
    assert!(
        !root
            .path()
            .join("target.alexandria-migration-report.json")
            .exists()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn embedding_migration_pending_boot_refuses_before_artifacts_and_access_clock_write() {
    let root = tempfile::tempdir().unwrap();
    let (_, target) = copied_fixture(root.path(), "pending");
    let before = fixture_process("inspect", &target);
    assert!(String::from_utf8_lossy(&before.stdout).contains("ARMED=false"));
    let output = command(root.path(), &target, &[]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("incomplete"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = fixture_process("inspect", &target);
    assert!(
        String::from_utf8_lossy(&after.stdout).contains("ARMED=false"),
        "{}",
        String::from_utf8_lossy(&after.stdout)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn embedding_migration_legacy_boot_mismatch_names_source_identity_and_copy_command_before_model_io()
{
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    fixture_process("seed", &source);
    let output = command(root.path(), &source, &[]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("public-legacy")
            && error.contains("128")
            && error.contains("--output-data-dir")
            && error.contains(source.to_str().unwrap()),
        "{error}"
    );
    assert!(
        String::from_utf8_lossy(&fixture_process("inspect", &source).stdout)
            .contains("ARMED=false")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn embedding_migration_resume_rejects_edited_marker_before_provisioning() {
    let root = tempfile::tempdir().unwrap();
    let (source, target) = copied_fixture(root.path(), "pending");
    fixture_process("repair-marker", &target);
    let output = command(
        root.path(),
        &source,
        &[
            "migrate-embeddings",
            "--output-data-dir",
            target.to_str().unwrap(),
            "--resume",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("marker mismatch"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn embedding_migration_resume_rejects_a_different_valid_receipt() {
    let root = tempfile::tempdir().unwrap();
    let (source, target) = copied_fixture(root.path(), "complete");
    std::fs::write(alexandria_storage::offline_copy::source_manifest_path(&target).unwrap(), r#"{"format_version":1,"files":[],"sha256":"1788f993b4772e3516f5409992386cee14668f98ca23773646f1670a8ffaf8d9"}"#).unwrap();
    let output = command(
        root.path(),
        &source,
        &[
            "migrate-embeddings",
            "--output-data-dir",
            target.to_str().unwrap(),
            "--resume",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("manifest mismatch"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn embedding_migration_active_source_refuses_before_artifacts_or_copy() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let target = root.path().join("target");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("LOCK"), "original lock").unwrap();
    let lock = std::fs::File::open(source.join("LOCK")).unwrap();
    lock.try_lock().unwrap();
    let output = command(
        root.path(),
        &source,
        &[
            "migrate-embeddings",
            "--output-data-dir",
            target.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("locked"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!target.exists());
    assert_eq!(
        std::fs::read_to_string(source.join("LOCK")).unwrap(),
        "original lock"
    );
}

#[test]
fn embedding_migration_cli_missing_output_does_not_create_source() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("missing-source");
    for args in [
        &["migrate-embeddings"][..],
        &["migrate-embeddings", "--force"],
        &["migrate-embeddings", "--resume"],
    ] {
        let output = command(root.path(), &source, args);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--output-data-dir"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!source.exists());
    }
}

#[test]
fn embedding_migration_cli_help_describes_copy_resume_and_manual_cutover() {
    let root = tempfile::tempdir().unwrap();
    let output = command(root.path(), &root.path().join("missing"), &["--help"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("--output-data-dir")
            && text.contains("--resume")
            && text.contains("manual")
            && text.contains("dream")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn dream_refuses_pending_or_corrupt_targets_before_schema_or_job_writes() {
    for mode in ["pending", "repair-marker", "corrupt-journal"] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fixture_process("seed", &source);
        fixture_process("duplicate", &source);
        let guard = alexandria_storage::offline_copy::OfflineStoreGuard::acquire(&source).unwrap();
        guard.copy_to(&target).unwrap();
        fixture_process("pending", &target);
        if mode != "pending" {
            fixture_process(mode, &target);
        }
        fixture_process("lower-schema-version", &target);
        let output = command(root.path(), &target, &["dream", "--job", "collapse"]);
        assert!(
            !output.status.success(),
            "{mode}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("job=collapse"));
        let inspected = fixture_process("inspect", &target);
        let text = String::from_utf8(inspected.stdout).unwrap();
        assert!(
            text.contains("LIVE=2")
                && text.contains("AUDIT=0")
                && text.contains("SCHEMA=0")
                && text.contains("ARMED=false"),
            "{mode}: {text}"
        );
        assert!(!root.path().join("cache").exists());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn dream_uses_ready_legacy_or_granite_vectors_without_model_or_config_identity_matching() {
    for mode in ["legacy", "complete"] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fixture_process("seed", &source);
        fixture_process("duplicate", &source);
        let target = root.path().join("target");
        let path = if mode == "complete" {
            let guard =
                alexandria_storage::offline_copy::OfflineStoreGuard::acquire(&source).unwrap();
            guard.copy_to(&target).unwrap();
            fixture_process("complete", &target);
            &target
        } else {
            &source
        };
        let output = command(root.path(), path, &["dream", "--job", "collapse"]);
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("job=collapse examined=2 acted=1 skipped=0")
        );
        let inspected = fixture_process("inspect", path);
        let text = String::from_utf8(inspected.stdout).unwrap();
        assert!(
            text.contains("LIVE=1") && text.contains("AUDIT=1") && text.contains("ARMED=false"),
            "{mode}: {text}"
        );
        assert!(!root.path().join("cache").exists());
    }
}

#[test]
fn dream_accepts_a_fresh_store_without_identity_or_artifact_provisioning() {
    let root = tempfile::tempdir().unwrap();
    let fresh = root.path().join("fresh-source");
    let output = command(root.path(), &fresh, &["dream", "--job", "collapse"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("job=collapse examined=0 acted=0 skipped=0")
    );
    assert!(!root.path().join("cache").exists());
    let inspected = fixture_process("inspect", &fresh);
    assert!(String::from_utf8_lossy(&inspected.stdout).contains("ARMED=false"));
}

#[test]
fn dream_help_routes_without_opening_a_store_or_loading_a_model() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing-source");
    let output = command(root.path(), &missing, &["dream", "--help"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("alexandria dream")
            && text.contains("--max-rows")
            && text.contains("single-writer")
    );
    assert!(!missing.exists());
    assert!(!root.path().join("cache").exists());
}

#[test]
fn embedding_migration_cli_alias_refuses_before_artifact_resolution_or_lock_write() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("LOCK"), "original-lock").unwrap();
    std::fs::write(source.join("public-payload"), "original-data").unwrap();
    let output = command(
        root.path(),
        &source,
        &[
            "migrate-embeddings",
            "--output-data-dir",
            source.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("overlap"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(source.join("LOCK")).unwrap(),
        "original-lock"
    );
    assert_eq!(
        std::fs::read_to_string(source.join("public-payload")).unwrap(),
        "original-data"
    );
    assert!(!root.path().join("cache").exists());
}

#[test]
fn embedding_migration_cli_resume_needs_existing_receipt_before_opening_foreign_store() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let target = root.path().join("foreign");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(source.join("LOCK"), "source-lock").unwrap();
    std::fs::write(target.join("LOCK"), "foreign-lock").unwrap();
    let output = command(
        root.path(),
        &source,
        &[
            "migrate-embeddings",
            "--output-data-dir",
            target.to_str().unwrap(),
            "--resume",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("manifest"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(target.join("LOCK")).unwrap(),
        "foreign-lock"
    );
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 1);
}
