#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use alexandria_mcp::auth::AuthService;
use alexandria_storage::access::AccessRepo;
use alexandria_storage::models::embedding::{EmbeddingSpace, MigrationPhase, ReembedJournal};
use alexandria_storage::models::{Fact, OperationId, PrincipalIds, ReadSelection, Reminder};
use alexandria_storage::offline_copy::{OfflineStoreGuard, read_source_manifest};
use alexandria_storage::ownership_migration::{BackupReceipt, OwnershipMigration, Report};
use alexandria_storage::repos::{AuthRepo, IdentityRepo, RetrievalStrategy, ScopedMemoryRepo};
use alexandria_storage::{
    Database, RecordId, embedding_state, parse_record_id, schema, system_config,
};
use serde_json::{Value, json};

const MODEL: &str = "alexandria-test/ownership-cli";
const MODEL_TOKENS: usize = 128;
const OWNER: &str = "nominated";
const PASSWORD: &str = "ownership CLI test fixture passphrase";
const LEGACY_TARGET: &str = " Unmapped Legacy Project ";

fn report(output: &Output) -> Report {
    assert!(
        output.status.success(),
        "bootstrap-owner must complete successfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout must contain the final JSON report only")
}

fn refusal(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "a disallowed command must exit nonzero"
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn model_cache(root: &Path) -> PathBuf {
    let repo = root.join("hub/models--alexandria-test--ownership-cli");
    let model = repo.join("snapshots/fixture");
    std::fs::create_dir_all(repo.join("refs")).unwrap();
    std::fs::write(repo.join("refs/main"), "fixture\n").unwrap();
    std::fs::create_dir_all(model.join("1_Pooling")).unwrap();
    model
}

fn write_unloadable_model(root: &Path) {
    let model = model_cache(root);
    std::fs::write(model.join("config.json"), "{").unwrap();
    for path in [
        "tokenizer.json",
        "model.safetensors",
        "1_Pooling/config.json",
    ] {
        std::fs::write(model.join(path), "{}").unwrap();
    }
}

// An artifact-only fixture: a zero-layer, two-dimensional BERT is sufficient to start the
// unmodified CPU provider. No inference, downloads, cached user artifacts, or binary test hooks.
fn write_model(root: &Path) {
    let model = model_cache(root);
    std::fs::write(
        model.join("config.json"),
        json!({
            "vocab_size": 1, "hidden_size": 2, "num_hidden_layers": 0,
            "num_attention_heads": 1, "intermediate_size": 2, "hidden_act": "gelu",
            "hidden_dropout_prob": 0.0, "max_position_embeddings": MODEL_TOKENS,
            "type_vocab_size": 1, "initializer_range": 0.02, "layer_norm_eps": 1e-12,
            "pad_token_id": 0
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        model.join("tokenizer.json"),
        r#"{"version":"1.0","model":{"type":"WordLevel","vocab":{"[UNK]":0},"unk_token":"[UNK]"}}"#,
    )
    .unwrap();
    std::fs::write(model.join("1_Pooling/config.json"), "{}").unwrap();
    let mut tensors = serde_json::Map::new();
    let mut data = Vec::new();
    for (name, shape) in [
        ("word_embeddings.weight", vec![1, 2]),
        ("position_embeddings.weight", vec![MODEL_TOKENS, 2]),
        ("token_type_embeddings.weight", vec![1, 2]),
        ("LayerNorm.weight", vec![2]),
        ("LayerNorm.bias", vec![2]),
    ] {
        let start = data.len();
        data.resize(start + shape.iter().product::<usize>() * 4, 0u8);
        tensors.insert(
            format!("embeddings.{name}"),
            json!({"dtype": "F32", "shape": shape, "data_offsets": [start, data.len()]}),
        );
    }
    let mut header = serde_json::to_vec(&tensors).unwrap();
    header.resize(header.len().next_multiple_of(8), b' ');
    let mut weights = (header.len() as u64).to_le_bytes().to_vec();
    weights.extend(header);
    weights.extend(data);
    std::fs::write(model.join("model.safetensors"), weights).unwrap();
}

struct Fixture {
    root: tempfile::TempDir,
    store: PathBuf,
    backup: PathBuf,
    passfile: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        let backup = root.path().join("backup");
        let passfile = root.path().join("password");
        write_unloadable_model(root.path());
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&passfile)
            .unwrap()
            .write_all(PASSWORD.as_bytes())
            .unwrap();
        Self {
            root,
            store,
            backup,
            passfile,
        }
    }

    fn legacy() -> Self {
        let fixture = Self::new();
        fixture.child("legacy");
        // Keep the copy's flock out of the parallel test harness, where a sibling's
        // pre-exec subprocess could inherit it across the guard's drop.
        fixture.child("offline-backup");
        fixture
    }

    fn child(&self, mode: &str) {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_child", "--ignored", "--nocapture"])
            .env("OWNERSHIP_CLI_FIXTURE_ROOT", self.root.path())
            .env("OWNERSHIP_CLI_FIXTURE_MODE", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture child {mode} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn command(&self, transport: &str, stdio_account: &str) -> Command {
        let root = self.root.path();
        let config = root.join("config.toml");
        let transport = if transport == "http" {
            // Finite matching loopback origin and lists, even though startup must refuse before bind.
            "[server]\ntransport = 'http'\nhost = '127.0.0.1'\nport = 0\nloopback_development = true\npublic_origin = 'http://127.0.0.1:8080'\nallowed_hosts = ['127.0.0.1']\nallowed_origins = ['http://127.0.0.1:8080']\n"
        } else {
            "[server]\ntransport = 'stdio'\n"
        };
        std::fs::write(
            &config,
            format!("{transport}[auth]\nstdio_account = '{stdio_account}'\n"),
        )
        .unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_alexandria"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ALEXANDRIA_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("ALEXANDRIA_CONFIG", config)
            .env("ALEXANDRIA_DATA_DIR", &self.store)
            .env("ALEXANDRIA_EMBEDDING_MODEL", MODEL)
            .env("ALEXANDRIA_EMBEDDING_DEVICE", "cpu")
            .env("ALEXANDRIA_EMBEDDING_MAX_TOKENS", MODEL_TOKENS.to_string())
            .env("HF_HUB_CACHE", root.join("hub"))
            .env("HOME", root)
            .env("RUST_LOG", "info");
        cmd
    }

    fn bootstrap(&self, backup: bool) -> Output {
        let mut cmd = self.command("stdio", "");
        cmd.args(["bootstrap-owner", "--login", OWNER, "--password-file"])
            .arg(&self.passfile)
            .args(["--max-rows", "1"]);
        if backup {
            cmd.arg("--backup-data-dir").arg(&self.backup);
        }
        cmd.output().unwrap()
    }
}

#[tokio::test]
#[ignore = "parent invokes this in a separate process for each persistent open"]
async fn fixture_child() {
    let root = PathBuf::from(std::env::var_os("OWNERSHIP_CLI_FIXTURE_ROOT").unwrap());
    let mode = std::env::var("OWNERSHIP_CLI_FIXTURE_MODE").unwrap();
    let store = root.join("store");
    if mode == "offline-backup" {
        OfflineStoreGuard::acquire(&store)
            .unwrap()
            .copy_to(&root.join("backup"))
            .unwrap();
        return;
    }
    let mut db = Database::connect_persistent(&store).await.unwrap();
    match mode.as_str() {
        "legacy" => seed_legacy(&db).await,
        "interrupt" => {
            schema::migrate(db.inner()).await.unwrap();
            let system = db.claim_system_authority().unwrap();
            let digest = AuthRepo::create_password_digest(PASSWORD).await.unwrap();
            let owner = seed_account(&db, OWNER, digest.as_str()).await;
            let migration = OwnershipMigration::new(&db);
            let receipt = BackupReceipt::read(&root.join("backup")).unwrap();
            let id = migration.begin(&system, owner, receipt).await.unwrap();
            assert!(!migration.step(&id, 1).await.unwrap().ready_to_verify);
        }
        "interrupt-embedding" => {
            let receipt = read_source_manifest(&root.join("backup")).unwrap();
            let source = embedding_space();
            let journal = ReembedJournal {
                run_id: "ownership-cli-embedding-fixture".into(),
                source_manifest_sha256: receipt.sha256,
                source_space: source,
                target_space:
                    alexandria_pipeline::embedding::granite::artifacts::ArtifactManifest::compiled()
                        .unwrap()
                        .space,
                phase: MigrationPhase::Facts,
                last_fact_id: None,
                reembedded_count: 0,
            };
            embedding_state::begin_target_reembed(db.inner(), &journal)
                .await
                .unwrap();
        }
        "inspect-migrated" => inspect_migrated(&mut db).await,
        "inspect-bootstrapped" => {
            assert_not_armed(&db).await;
            alexandria_storage::ownership_migration::check_ownership_ready(db.inner())
                .await
                .unwrap();
            let db = Arc::new(db);
            AuthService::new(db)
                .login(OWNER, PASSWORD, chrono::Utc::now())
                .await
                .expect("fresh bootstrap must produce a usable account password");
        }
        "stdio-identities" => {
            schema::migrate(db.inner()).await.unwrap();
            for (login, message) in [
                (OWNER, "other account reminder"),
                ("alternate", "configured account reminder"),
            ] {
                // Trusted-local stdio has no password credential to authenticate.
                let ids = seed_account(&db, login, "unused-hash").await;
                db.inner()
                    .query(
                        "CREATE reminder SET account = $account, message = $message, \
                    schedule_kind = 'once', status = 'pending', due_at = d'2090-01-01T09:00:00Z', \
                    next_due_at = d'2090-01-01T09:00:00Z' RETURN NONE",
                    )
                    .bind(("account", ids.account.as_record_id().clone()))
                    .bind(("message", message))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            }
        }
        _ => panic!("unknown fixture mode {mode}"),
    }
    // Release the SurrealKV LOCK by ending the process, not by assuming SDK drop is synchronous.
    std::process::exit(0);
}

fn embedding_space() -> EmbeddingSpace {
    EmbeddingSpace::Legacy {
        model: MODEL.into(),
        dimensions: 2,
        max_tokens: MODEL_TOKENS,
    }
}

async fn seed_legacy(db: &Database) {
    for (_, _, sql) in schema::MIGRATIONS.iter().take_while(|(v, _, _)| *v <= 8) {
        db.inner().query(*sql).await.unwrap().check().unwrap();
    }
    system_config::set_config(db.inner(), "schema_version", "8")
        .await
        .unwrap();
    system_config::check_embedding_space(db.inner(), &embedding_space())
        .await
        .unwrap();
    db.inner()
        .query(
            "CREATE $live SET content = 'legacy text', embedding = [1.0, 0.0];
             CREATE $deleted SET content = 'historical text', embedding = [0.0, 1.0], deleted = true;
             CREATE $reminder SET message = 'recurring legacy reminder', target_project = $target,
                 schedule_kind = 'pattern', freq = 'weekly', time_of_day = '09:00', weekdays = ['mon'],
                 status = 'pending', next_due_at = d'2025-01-13T09:00:00Z',
                 last_delivered_at = d'2025-01-06T09:00:00Z', delivered_count = 7;",
        )
        .bind(("live", parse_record_id("fact:7").unwrap()))
        .bind(("deleted", parse_record_id("fact:⟨old-version⟩").unwrap()))
        .bind(("reminder", RecordId::new("reminder", "weekly")))
        .bind(("target", LEGACY_TARGET))
        .await.unwrap().check().unwrap();
}

async fn seed_account(db: &Database, login: &str, digest: &str) -> PrincipalIds {
    IdentityRepo::new(db.inner())
        .bootstrap_owner(login, digest, &OperationId::new(login))
        .await
        .unwrap()
}

async fn assert_not_armed(db: &Database) {
    assert!(
        system_config::get_config(db.inner(), system_config::ACCESS_RECORDING_ARMED_AT)
            .await
            .unwrap()
            .is_none(),
        "the offline bootstrap must not arm access recording"
    );
}

async fn inspect_migrated(db: &mut Database) {
    assert_not_armed(db).await;
    let system = db.claim_system_authority().unwrap();
    let account = AuthRepo::new(db.inner())
        .account_by_login(OWNER)
        .await
        .unwrap()
        .unwrap();
    let account_id = account.account_id().unwrap();
    let access = AccessRepo::new(db);
    let principal = access.local_principal(&system, &account_id).await.unwrap();
    let owner = access.authorize(&principal).await.unwrap();
    let assigned = access
        .select(&owner, ReadSelection::Assigned)
        .await
        .unwrap();
    assert!(
        ScopedMemoryRepo::new(db.inner(), &owner, &assigned)
            .nearest(&[1.0, 0.0], 10, RetrievalStrategy::Exact)
            .await
            .unwrap()
            .is_empty()
    );
    let review = access
        .select(&owner, ReadSelection::UnassignedReview)
        .await
        .unwrap();
    let reviewed = ScopedMemoryRepo::new(db.inner(), &owner, &review)
        .review_page(None, 10)
        .await
        .unwrap();
    assert_eq!(reviewed.len(), 1);
    assert_eq!(reviewed[0].id, Some(parse_record_id("fact:7").unwrap()));

    for (id, content, embedding, deleted) in [
        ("fact:7", "legacy text", vec![1.0, 0.0], false),
        (
            "fact:⟨old-version⟩",
            "historical text",
            vec![0.0, 1.0],
            true,
        ),
    ] {
        let id = parse_record_id(id).unwrap();
        let fact: Option<Fact> = db
            .inner()
            .query("SELECT * FROM $fact WHERE account = $account AND scope = 'unassigned'")
            .bind(("fact", id.clone()))
            .bind(("account", account_id.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        let fact = fact.expect("original typed fact ID and nominated ownership must be retained");
        assert_eq!(
            (fact.id, fact.content, fact.embedding, fact.deleted),
            (Some(id), content.to_owned(), embedding, deleted)
        );
    }
    let reminder: Option<Reminder> = db
        .inner()
        .select(RecordId::new("reminder", "weekly"))
        .await
        .unwrap();
    let reminder = reminder.unwrap();
    assert_eq!(reminder.account.as_ref(), Some(account_id.as_record_id()));
    assert_eq!(reminder.migration_hold, Some(true));
    assert_eq!(
        reminder.legacy_target_project.as_deref(),
        Some(LEGACY_TARGET)
    );
    assert!(reminder.project.is_none());
    assert_eq!(reminder.schedule_kind, "pattern");
    assert_eq!(reminder.freq.as_deref(), Some("weekly"));
    assert_eq!(reminder.time_of_day.as_deref(), Some("09:00"));
    assert_eq!(reminder.weekdays, ["mon"]);
    assert_eq!(reminder.status, "pending");
    assert_eq!(reminder.delivered_count, 7);
    assert_eq!(
        reminder.next_due_at.unwrap().to_rfc3339(),
        "2025-01-13T09:00:00+00:00"
    );
    assert_eq!(
        reminder.last_delivered_at.unwrap().to_rfc3339(),
        "2025-01-06T09:00:00+00:00"
    );
}

struct StdioMcp(Child);

impl Drop for StdioMcp {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn stdio_exchange(fixture: &Fixture, login: &str, read_reminders: bool) -> Value {
    let mut child = StdioMcp(
        fixture
            .command("stdio", login)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (send, recv) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // Existing binary versions send tracing to stdout as well as MCP JSON.
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && send.send(message).is_err()
            {
                break;
            }
        }
    });
    let response = |id| {
        let message = recv
            .recv_timeout(Duration::from_secs(20))
            .expect("stdio MCP response");
        assert_eq!(message["id"], id, "unexpected response: {message}");
        assert!(
            message.get("error").is_none(),
            "MCP request failed: {message}"
        );
        message["result"].clone()
    };
    writeln!(input, "{}", json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
        "protocolVersion":"2025-03-26", "capabilities":{}, "clientInfo":{"name":"ownership-cli-test","version":"1"}
    }})).unwrap();
    let initialized = response(1);
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
    )
    .unwrap();
    let result = if read_reminders {
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{
                "name":"list_reminders", "arguments":{}
            }})
        )
        .unwrap();
        response(2)
    } else {
        initialized
    };
    drop(input);
    drop(child);
    reader.join().unwrap();
    result
}

#[test]
fn bootstrap_owner_migrates_legacy_corpus_and_preserves_reminders() {
    let fixture = Fixture::legacy();
    let output = fixture.bootstrap(true);
    let result = report(&output);
    assert_eq!(result.rows_migrated, 3);
    assert_eq!(result.held_reminders, 1);
    let progress = String::from_utf8_lossy(&output.stderr);
    assert!(
        progress.contains("page 1") && progress.contains("page 2"),
        "paged progress: {progress}"
    );
    assert!(
        progress.split_whitespace().any(|word| {
            let Some((epoch, pid)) = word
                .strip_prefix("run-cli-")
                .and_then(|id| id.split_once('-'))
            else {
                return false;
            };
            epoch.parse::<u64>().is_ok() && pid.parse::<u32>().is_ok()
        }),
        "progress must identify the run-cli-<epoch>-<pid> audit run: {progress}"
    );
    fixture.child("inspect-migrated");
}

#[test]
fn bootstrap_owner_refuses_a_missing_backup_receipt() {
    let fixture = Fixture::legacy();
    std::fs::rename(
        &fixture.backup,
        fixture.root.path().join("missing-from-cli"),
    )
    .unwrap();
    let err = refusal(&fixture.bootstrap(true));
    assert!(
        err.to_lowercase().contains("backup receipt"),
        "missing backup receipt must be diagnosed: {err}"
    );
}

#[test]
fn bootstrap_owner_refuses_a_held_store_before_opening_the_receipt() {
    let fixture = Fixture::legacy();
    std::fs::rename(
        &fixture.backup,
        fixture.root.path().join("missing-from-cli"),
    )
    .unwrap();
    let _lock = OfflineStoreGuard::acquire(&fixture.store).unwrap();
    let err = refusal(&fixture.bootstrap(true));
    assert!(
        err.contains("single-writer") && err.contains("Stop the alexandria service"),
        "held store must be diagnosed before inspecting the backup receipt: {err}"
    );
}

fn assert_ownership_refusal(fixture: &Fixture, transport: &str, condition: &str) {
    // The default fixture artifacts are malformed and cache-only so even the unguarded HTTP
    // process exits on RED. On GREEN the ownership refusal must beat the model/provider error.
    let output = fixture.command(transport, "").output().unwrap();
    let err = refusal(&output);
    assert!(
        err.contains(condition) && err.contains("alexandria bootstrap-owner"),
        "{transport} must name the ownership condition and recovery command before model I/O: {err}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Loading embedding model"));
}

#[test]
fn legacy_corpus_refuses_to_serve_before_an_ownership_journal_exists() {
    let fixture = Fixture::legacy();
    assert_ownership_refusal(&fixture, "stdio", "Legacy ownership migration required");
}

#[test]
fn interrupted_ownership_refuses_stdio_until_bootstrap_owner_completes() {
    let fixture = Fixture::legacy();
    fixture.child("interrupt");
    assert_ownership_refusal(&fixture, "stdio", "Ownership migration incomplete");
    let result = report(&fixture.bootstrap(true));
    assert_eq!(
        result.rows_migrated, 3,
        "resume must preserve the first page checkpoint"
    );
    fixture.child("inspect-migrated");
    write_model(fixture.root.path());
    let initialized = stdio_exchange(&fixture, OWNER, false);
    assert!(initialized["serverInfo"]["name"].as_str().is_some());
    assert!(initialized["capabilities"]["tools"].is_object());
}

#[test]
fn incomplete_ownership_refuses_http_before_loading_embeddings() {
    let fixture = Fixture::legacy();
    fixture.child("interrupt");
    assert_ownership_refusal(&fixture, "http", "Ownership migration incomplete");
}

#[test]
fn incomplete_embedding_refuses_serve_with_the_embedding_resume_command() {
    let fixture = Fixture::legacy();
    fixture.child("interrupt-embedding");
    let output = fixture.command("stdio", "").output().unwrap();
    let err = refusal(&output);
    assert!(
        err.contains("Embedding migration incomplete")
            && err.contains("alexandria migrate-embeddings --output-data-dir")
            && err.contains("--resume"),
        "embedding refusal must name the owned-target resume command: {err}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Loading embedding model"));
}

#[test]
fn fresh_empty_store_serves_over_stdio_without_an_account() {
    let fixture = Fixture::new();
    write_model(fixture.root.path());
    let initialized = stdio_exchange(&fixture, "", false);
    assert!(initialized["serverInfo"]["name"].as_str().is_some());
    assert!(initialized["capabilities"]["tools"].is_object());
}

#[test]
fn fresh_owner_bootstrap_needs_no_legacy_backup() {
    let fixture = Fixture::new();
    report(&fixture.bootstrap(false));
    fixture.child("inspect-bootstrapped");
}

#[test]
fn stdio_configured_identity_reads_only_that_accounts_reminders() {
    let fixture = Fixture::new();
    fixture.child("stdio-identities");
    write_model(fixture.root.path());
    let result = stdio_exchange(&fixture, "alternate", true);
    assert_eq!(
        result["isError"], false,
        "stdio account was not admitted: {result}"
    );
    let payload = &result["structuredContent"];
    assert_eq!(payload["count"], 1);
    assert_eq!(
        payload["reminders"][0]["message"],
        "configured account reminder"
    );
}
