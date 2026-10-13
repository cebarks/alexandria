//! Separate offline target orchestration; final reports are reconstructed from
//! the journal, original baseline and receipt, never a second authoritative file.

use crate::config::Config;
use alexandria_pipeline::embedding::{
    LoadedEmbedding, ProviderOptions, parse_granite_device, validate_max_tokens,
};
use alexandria_storage::embedding_state::{
    begin_target_reembed, read_target_journal, validate_target_ownership, verify_target_index,
};
use alexandria_storage::embedding_validation::read_target_baseline;
use alexandria_storage::models::embedding::{
    EmbeddingSpace, GRANITE_MODEL_ID, MigrationPhase, ReembedJournal,
};
use alexandria_storage::offline_copy::{
    OfflineStoreGuard, SourceManifest, preflight_migration_paths, read_source_manifest,
};
use alexandria_storage::{Database, schema, system_config};
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub struct MigrationArgs {
    pub output_data_dir: PathBuf,
    pub resume: bool,
    pub force: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct MigrationReport {
    pub output_path: PathBuf,
    pub source_space: EmbeddingSpace,
    pub target_space: EmbeddingSpace,
    pub fact_count: usize,
    pub cluster_count: usize,
    pub source_manifest_sha256: String,
    pub index_verified: bool,
}

/// Strict separate-target grammar; legacy no-output dispatch stays in main.
pub fn parse_migration_args(args: &[String]) -> Result<MigrationArgs> {
    let mut output = None;
    let mut resume = false;
    let mut force = false;
    let mut flags = args.iter();
    while let Some(flag) = flags.next() {
        match flag.as_str() {
            "--output-data-dir" => {
                ensure!(output.is_none(), "Repeated --output-data-dir");
                let path = flags.next().context("--output-data-dir requires a path")?;
                ensure!(
                    !path.is_empty() && !path.starts_with('-'),
                    "--output-data-dir requires a path, not another flag"
                );
                output = Some(PathBuf::from(path));
            }
            "--resume" => {
                ensure!(!resume, "Repeated --resume");
                resume = true;
            }
            "--force" => {
                ensure!(!force, "Repeated --force");
                force = true;
            }
            other => anyhow::bail!("Unknown migration argument: {other}"),
        }
    }
    Ok(MigrationArgs { output_data_dir: output.context("Separate migration requires --output-data-dir; --force alone is not an in-place escape")?, resume, force })
}

pub async fn migrate_to_copy(config: &Config, args: MigrationArgs) -> Result<MigrationReport> {
    migrate_with_loader(config, args, alexandria_pipeline::embedding::load_provider).await
}

async fn migrate_with_loader<F, Fut>(
    config: &Config,
    args: MigrationArgs,
    loader: F,
) -> Result<MigrationReport>
where
    F: FnOnce(ProviderOptions) -> Fut,
    Fut: std::future::Future<Output = Result<LoadedEmbedding>>,
{
    validate_request(config)?;
    let source = config.database.data_dir.clone();
    let output = args.output_data_dir;
    let resume = args.resume;
    let target =
        tokio::task::spawn_blocking(move || preflight_migration_paths(&source, &output, resume))
            .await??;
    if args.force {
        tracing::info!("--force does not override output ownership or replay a completed target");
    }
    if resume {
        let path = target.clone();
        let receipt = tokio::task::spawn_blocking(move || read_source_manifest(&path)).await??;
        let db = Database::connect_persistent(&target).await?;
        let journal = read_target_journal(db.inner())
            .await?
            .context("Resume target has no owned migration journal")?;
        ensure!(
            journal.source_manifest_sha256 == receipt.sha256,
            "Resume journal/source manifest mismatch"
        );
        configured_target_matches(config, &journal.target_space)?;
        validate_target_ownership(db.inner(), &journal).await?;
        if journal.phase != MigrationPhase::Complete {
            let loaded = loader((&config.embedding).into()).await?;
            alexandria_mcp::migrate::reembed_target(
                &db,
                &loaded,
                config.embedding.batch_size,
                journal,
            )
            .await?;
        }
        return reconstruct_report(&db, target, &receipt).await;
    }
    let source = config.database.data_dir.clone();
    let guard = tokio::task::spawn_blocking(move || OfflineStoreGuard::acquire(&source)).await??;
    // Provision first so a download/device failure does not publish a copy without
    // a journal. Keep the read-only source guard through the subsequent copy.
    let loaded = loader((&config.embedding).into()).await?;
    configured_target_matches(config, &loaded.space)?;
    let path = target.clone();
    let receipt = tokio::task::spawn_blocking(move || guard.copy_to(&path)).await??;
    let db = Database::connect_persistent(&target).await?;
    schema::migrate(db.inner()).await?;
    let source_space = system_config::stored_embedding_space(db.inner()).await?.context("Copied source has no verified embedding identity; preserve it and resolve its original model before migration")?;
    let journal = ReembedJournal {
        run_id: uuid::Uuid::new_v4().to_string(),
        source_manifest_sha256: receipt.sha256.clone(),
        source_space,
        target_space: loaded.space.clone(),
        phase: MigrationPhase::Facts,
        last_fact_id: None,
        reembedded_count: 0,
    };
    begin_target_reembed(db.inner(), &journal).await?;
    tracing::info!(source_model = %journal.source_space.model(), target_model = %journal.target_space.model(), device = %config.embedding.device, "Rewriting owned separate target");
    alexandria_mcp::migrate::reembed_target(&db, &loaded, config.embedding.batch_size, journal)
        .await?;
    reconstruct_report(&db, target, &receipt).await
}

fn validate_request(config: &Config) -> Result<()> {
    ensure!(
        !config.embedding.model.trim().is_empty(),
        "embedding.model must not be blank"
    );
    validate_max_tokens(&config.embedding.model, config.embedding.max_tokens)?;
    ensure!(
        (1..=4096).contains(&config.embedding.batch_size),
        "Migration batch_size must be between 1 and 4096"
    );
    if config.embedding.model == GRANITE_MODEL_ID {
        parse_granite_device(&config.embedding.device)?;
        // The loader checks compiled/available backends when inference is needed.
        // Completed reporting is device-independent and never initializes one.
    } else {
        ensure!(
            config.embedding.device == "cpu",
            "Legacy Candle supports only cpu"
        );
    }
    Ok(())
}

fn configured_target_matches(config: &Config, space: &EmbeddingSpace) -> Result<()> {
    space.validate()?;
    ensure!(
        space.model() == config.embedding.model
            && space.max_tokens() == config.embedding.max_tokens,
        "Resume/configured target embedding identity mismatch"
    );
    if config.embedding.model == GRANITE_MODEL_ID {
        let mut expected =
            alexandria_pipeline::embedding::granite::artifacts::ArtifactManifest::compiled()?.space;
        let EmbeddingSpace::GraniteV1(identity) = &mut expected else {
            anyhow::bail!("Invalid compiled Granite identity")
        };
        identity.max_tokens = config.embedding.max_tokens;
        ensure!(
            space == &expected,
            "Resume/configured target full embedding identity mismatch"
        );
    }
    Ok(())
}

async fn reconstruct_report(
    db: &Database,
    output_path: PathBuf,
    receipt: &SourceManifest,
) -> Result<MigrationReport> {
    let journal = read_target_journal(db.inner())
        .await?
        .context("Target has no migration journal")?;
    ensure!(
        journal.phase == MigrationPhase::Complete
            && journal.source_manifest_sha256 == receipt.sha256,
        "Target is not a completed owned copy of this manifest"
    );
    system_config::check_embedding_space(db.inner(), &journal.target_space).await?;
    let baseline = read_target_baseline(db.inner(), &journal).await?;
    verify_target_index(db.inner(), journal.target_space.dimensions()).await?;
    Ok(MigrationReport {
        output_path,
        source_space: journal.source_space,
        target_space: journal.target_space,
        fact_count: baseline.fact_count,
        cluster_count: baseline.cluster_count,
        source_manifest_sha256: receipt.sha256.clone(),
        index_verified: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alexandria_pipeline::embedding::{EmbeddingProvider, LoadedEmbedding};
    use alexandria_storage::embedding_state::read_target_journal;
    use alexandria_storage::models::embedding::{GRANITE_MODEL_ID, MigrationPhase};
    use alexandria_storage::{Database, schema, system_config};
    use std::sync::Arc;

    fn target_space() -> EmbeddingSpace {
        alexandria_pipeline::embedding::granite::artifacts::ArtifactManifest::parse_json(
            include_str!("../../alexandria-pipeline/src/embedding/granite/artifact-manifest.json"),
        )
        .unwrap()
        .space
    }
    struct PublicEncoder;
    #[async_trait::async_trait]
    impl EmbeddingProvider for PublicEncoder {
        async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
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
    fn loaded() -> LoadedEmbedding {
        LoadedEmbedding {
            provider: Arc::new(PublicEncoder),
            space: target_space(),
            effective_max_tokens: 8192,
        }
    }

    fn config_and_args(
        source: &std::path::Path,
        target: &std::path::Path,
        resume: bool,
    ) -> (Config, MigrationArgs) {
        let mut config = Config::default();
        config.database.data_dir = source.into();
        config.embedding.model = GRANITE_MODEL_ID.into();
        config.embedding.max_tokens = 8192;
        config.embedding.batch_size = 2;
        (
            config,
            MigrationArgs {
                output_data_dir: target.into(),
                resume,
                force: false,
            },
        )
    }
    fn seed_source(path: &std::path::Path) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "embedding_migration::tests::fixture_child",
                "--ignored",
                "--nocapture",
            ])
            .env("ALEXANDRIA_MIGRATION_FIXTURE_PATH", path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[tokio::test]
    #[ignore = "parent seeds an isolated public store then exits without database drop"]
    async fn fixture_child() {
        let path = std::env::var_os("ALEXANDRIA_MIGRATION_FIXTURE_PATH").unwrap();
        let db = Database::connect_persistent(std::path::Path::new(&path))
            .await
            .unwrap();
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
        schema::ensure_vector_index(db.inner(), 2).await.unwrap();
        std::process::exit(0);
    }
    fn operation(mode: &str, source: &std::path::Path, target: &std::path::Path) -> String {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "embedding_migration::tests::migration_operation_child",
                "--ignored",
                "--nocapture",
            ])
            .env("ALEXANDRIA_MIGRATION_FIXTURE_PATH", source)
            .env("ALEXANDRIA_MIGRATION_FIXTURE_TARGET", target)
            .env("ALEXANDRIA_MIGRATION_FIXTURE_MODE", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    fn result_line(output: &str, prefix: &str) -> serde_json::Value {
        serde_json::from_str(
            output
                .lines()
                .find_map(|line| line.strip_prefix(prefix))
                .unwrap(),
        )
        .unwrap()
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn embedding_migration_copy_report_is_durable_and_completed_resume_never_loads_encoder() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        seed_source(&source);
        let report = result_line(&operation("fresh", &source, &target), "REPORT=");
        assert_eq!(
            report["output_path"],
            target.canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(report["fact_count"], 1);
        assert_eq!(report["cluster_count"], 0);
        assert_eq!(report["index_verified"], true);
        assert_eq!(
            report["target_space"],
            serde_json::to_value(target_space()).unwrap()
        );
        let resumed = result_line(&operation("report", &source, &target), "REPORT=");
        assert_eq!(report, resumed);
        assert!(
            !root
                .path()
                .join("target.alexandria-migration-report.json")
                .exists()
        );
        operation("source", &source, &target);
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn embedding_migration_provider_failure_does_not_publish_an_unresumable_copy() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        seed_source(&source);
        let original_lock = std::fs::read(source.join("LOCK")).unwrap();
        let (config, arguments) = config_and_args(&source, &target, false);
        assert!(
            migrate_with_loader(&config, arguments, |_| async {
                anyhow::bail!("injected provisioning failure")
            })
            .await
            .is_err()
        );
        assert!(!target.exists());
        assert_eq!(std::fs::read(source.join("LOCK")).unwrap(), original_lock);
    }
    #[tokio::test]
    #[ignore = "parent runs each command in its own process, matching one-shot CLI lifetime"]
    async fn migration_operation_child() {
        let source = PathBuf::from(std::env::var_os("ALEXANDRIA_MIGRATION_FIXTURE_PATH").unwrap());
        let target =
            PathBuf::from(std::env::var_os("ALEXANDRIA_MIGRATION_FIXTURE_TARGET").unwrap());
        let mode = std::env::var("ALEXANDRIA_MIGRATION_FIXTURE_MODE").unwrap();
        let (mut config, arguments) =
            config_and_args(&source, &target, mode != "fresh" && mode != "fail");
        match mode.as_str() {
            "source" => {
                let db = Database::connect_persistent(&source).await.unwrap();
                assert_eq!(
                    system_config::stored_embedding_space(db.inner())
                        .await
                        .unwrap(),
                    Some(EmbeddingSpace::Legacy {
                        model: "public-legacy".into(),
                        dimensions: 2,
                        max_tokens: 128
                    })
                );
                assert_eq!(
                    alexandria_storage::repos::MemoryRepo::new(db.inner())
                        .list(&Default::default())
                        .await
                        .unwrap()[0]
                        .embedding
                        .len(),
                    2
                );
            }
            "inspect" => {
                let db = Database::connect_persistent(&target).await.unwrap();
                let journal = read_target_journal(db.inner()).await.unwrap().unwrap();
                let ready = system_config::check_embedding_space(db.inner(), &target_space()).await;
                assert_eq!(ready.is_ok(), journal.phase == MigrationPhase::Complete);
                println!("PHASE={}", serde_json::to_string(&journal.phase).unwrap());
                println!(
                    "BASELINE={}",
                    serde_json::to_string(
                        &read_target_baseline(db.inner(), &journal).await.unwrap()
                    )
                    .unwrap()
                );
            }
            "fail" => {
                struct Fail;
                #[async_trait::async_trait]
                impl EmbeddingProvider for Fail {
                    async fn embed(&self, _: &[&str]) -> Result<Vec<Vec<f32>>> {
                        anyhow::bail!("interrupted forward")
                    }
                    fn dimensions(&self) -> usize {
                        768
                    }
                    fn model_id(&self) -> &str {
                        GRANITE_MODEL_ID
                    }
                }
                let error = migrate_with_loader(&config, arguments, |_| async {
                    let mut embedding = loaded();
                    embedding.provider = Arc::new(Fail);
                    Ok(embedding)
                })
                .await
                .unwrap_err();
                assert!(error.to_string().contains("interrupted forward"));
                assert!(target.is_dir());
            }
            "wrong" => {
                config.embedding.max_tokens = 16;
                let error = migrate_with_loader(&config, arguments, |_| async {
                    panic!("identity refusal precedes provider load")
                })
                .await
                .unwrap_err();
                assert!(error.to_string().contains("identity mismatch"));
            }
            "report" => {
                let report = migrate_with_loader(&config, arguments, |_| async {
                    panic!("completed report must not load weights/infer")
                })
                .await
                .unwrap();
                println!("REPORT={}", serde_json::to_string(&report).unwrap());
            }
            "fresh" | "resume" => {
                let report = migrate_with_loader(&config, arguments, |_| async { Ok(loaded()) })
                    .await
                    .unwrap();
                println!("REPORT={}", serde_json::to_string(&report).unwrap());
            }
            other => panic!("unknown fixture mode {other}"),
        }
        std::process::exit(0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn embedding_migration_pending_resume_keeps_original_baseline_and_checks_target_identity() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        seed_source(&source);
        operation("fail", &source, &target);
        let before = operation("inspect", &source, &target);
        assert_eq!(result_line(&before, "PHASE="), "facts");
        operation("wrong", &source, &target);
        let report = result_line(&operation("resume", &source, &target), "REPORT=");
        assert_eq!(report["index_verified"], true);
        let after = operation("inspect", &source, &target);
        assert_eq!(result_line(&after, "PHASE="), "complete");
        assert_eq!(
            result_line(&before, "BASELINE="),
            result_line(&after, "BASELINE=")
        );
    }
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }
    #[test]
    fn embedding_migration_parser_accepts_explicit_output_and_order_independent_flags() {
        assert_eq!(
            parse_migration_args(&args(&["--output-data-dir", "target"])).unwrap(),
            MigrationArgs {
                output_data_dir: "target".into(),
                resume: false,
                force: false,
            }
        );
        assert_eq!(
            parse_migration_args(&args(&[
                "--resume",
                "--output-data-dir",
                "target",
                "--force"
            ]))
            .unwrap(),
            MigrationArgs {
                output_data_dir: "target".into(),
                resume: true,
                force: true,
            }
        );
    }
    #[test]
    fn embedding_migration_parser_requires_output_even_with_force_or_resume() {
        for values in [
            &[][..],
            &["--force"],
            &["--resume"],
            &["--output-data-dir"],
            &["--output-data-dir", ""],
        ] {
            assert!(
                parse_migration_args(&args(values))
                    .unwrap_err()
                    .to_string()
                    .contains("--output-data-dir")
            );
        }
    }
    #[test]
    fn embedding_migration_parser_rejects_unknown_repeated_and_flag_as_path() {
        for values in [
            vec!["--output-data-dir", "t", "--unknown"],
            vec!["--output-data-dir", "t", "--resume", "--resume"],
            vec!["--output-data-dir", "t", "--force", "--force"],
            vec!["--output-data-dir", "t", "--output-data-dir", "u"],
            vec!["--output-data-dir", "--resume"],
        ] {
            assert!(parse_migration_args(&args(&values)).is_err());
        }
    }
}
