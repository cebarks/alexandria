#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use alexandria_engine::refinement::policy::{InferencePolicyInputs, Protocol, Route, RoutePair};
use alexandria_mcp::refinement::ReconciledRefinement;
use alexandria_storage::access::{AccessRepo, OwnerContext};
use alexandria_storage::models::embedding::{EmbeddingSpace, MigrationPhase, ReembedJournal};
use alexandria_storage::models::identity::ToSql;
use alexandria_storage::models::refinement::{ApprovalMode, ConsentChoice};
use alexandria_storage::models::{OperationId, PrincipalIds};
use alexandria_storage::offline_copy::OfflineStoreGuard;
use alexandria_storage::ownership_migration::{BackupReceipt, OwnershipMigration};
use alexandria_storage::repos::{AuthRepo, IdentityRepo, RefinementRepo};
use alexandria_storage::{Database, embedding_state, schema, system_config};
use serde_json::{Value, json};

const LOCAL: &str = "local-owner";
const OTHER: &str = "not-the-local-owner";
const MODEL: &str = "alexandria-test/refinement-cli";
const TOKENS: usize = 128;

struct Fixture {
    root: tempfile::TempDir,
    inference: TcpListener,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        write_poisoned_model_cache(root.path());
        let inference = TcpListener::bind("127.0.0.1:0").unwrap();
        inference.set_nonblocking(true).unwrap();
        Self { root, inference }
    }

    fn ready() -> Self {
        let fixture = Self::new();
        fixture.child("ready");
        fixture
    }

    fn path(&self, path: &str) -> PathBuf {
        self.root.path().join(path)
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.inference.local_addr().unwrap())
    }

    fn workspace(&self, login: &str) -> String {
        std::fs::read_to_string(self.path(login)).unwrap()
    }

    fn child(&self, mode: &str) {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_child", "--ignored", "--nocapture"])
            .env("REFINEMENT_CLI_ROOT", self.root.path())
            .env("REFINEMENT_CLI_MODE", mode)
            .env("REFINEMENT_CLI_URL", self.base_url())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture {mode}: {}",
            transcript(&output)
        );
    }

    fn command(&self, login: &str, prefix: &str) -> Command {
        let config = self.path("config.toml");
        std::fs::write(
            &config,
            format!(
                "[server]\ntransport = 'stdio'\n\
                 [auth]\nstdio_account = '{login}'\n\
                 [refinement]\napproval_mode_ceiling = 'secondary_llm'\n\
                 [refinement.proposer]\nprotocol = 'open_ai_chat_completions'\n\
                 api_prefix = '{prefix}'\nmodel = 'fixture-proposer'\n\
                 credential_ref = 'REFINEMENT_CLI_SECRET'\n"
            ),
        )
        .unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_alexandria"));
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("ALEXANDRIA_") || name.to_ascii_uppercase().ends_with("_PROXY") {
                cmd.env_remove(key);
            }
        }
        cmd.env("ALEXANDRIA_CONFIG", config)
            .env("ALEXANDRIA_DATA_DIR", self.path("store"))
            .env("ALEXANDRIA_EMBEDDING_MODEL", MODEL)
            .env("ALEXANDRIA_EMBEDDING_DEVICE", "cpu")
            .env("ALEXANDRIA_EMBEDDING_MAX_TOKENS", TOKENS.to_string())
            .env("HF_HUB_CACHE", self.path("hub"))
            .env("HOME", self.root.path())
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("REFINEMENT_CLI_SECRET", "fixture-secret-never-disclose")
            .env("RUST_LOG", "info");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(LOCAL, &self.base_url())
            .args(args)
            .output()
            .unwrap()
    }

    fn refine(&self, login: &str, workspace: &str, prefix: &str) -> Output {
        self.command(login, prefix)
            .args([
                "dream",
                "--job",
                "refine",
                "--workspace",
                workspace,
                "--scope",
                "personal",
            ])
            .output()
            .unwrap()
    }

    fn grant(&self, disclosure: &Value) -> Output {
        self.run(&[
            "refinement",
            "consent",
            "grant",
            "--policy-revision",
            &revision(disclosure, "policy_revision"),
            "--consent-revision",
            &revision(disclosure, "consent_revision"),
        ])
    }

    fn assert_no_inference(&self) {
        match self.inference.accept() {
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            _ => panic!("a command opened an inference connection"),
        }
    }
}

fn write_poisoned_model_cache(root: &Path) {
    // All artifacts are cached but invalid: merely loading this provider fails offline. A
    // legacy dream which loads it cannot pass the success and exact-five-job assertions.
    let repo = root.join("hub/models--alexandria-test--refinement-cli");
    let snapshot = repo.join("snapshots/fixture");
    std::fs::create_dir_all(repo.join("refs")).unwrap();
    std::fs::write(repo.join("refs/main"), "fixture\n").unwrap();
    std::fs::create_dir_all(snapshot.join("1_Pooling")).unwrap();
    for file in [
        "config.json",
        "tokenizer.json",
        "model.safetensors",
        "1_Pooling/config.json",
    ] {
        std::fs::write(snapshot.join(file), "{").unwrap();
    }
}

fn transcript(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn json_report(output: &Output, label: &str) -> Value {
    assert!(
        output.status.success(),
        "{label} must succeed: {}",
        transcript(output)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{label} must print one JSON report: {error}: {}",
            transcript(output)
        )
    })
}

fn refused_for(output: &Output, label: &str, needles: &[&str]) {
    let text = transcript(output);
    assert!(!output.status.success(), "{label} must refuse: {text}");
    let text = text.to_ascii_lowercase();
    for needle in needles {
        assert!(
            text.contains(needle),
            "{label} must diagnose `{needle}`: {text}"
        );
    }
}

fn revision(report: &Value, field: &str) -> String {
    report[field]
        .as_u64()
        .unwrap_or_else(|| panic!("missing {field}: {report}"))
        .to_string()
}

fn embedding_space() -> EmbeddingSpace {
    EmbeddingSpace::Legacy {
        model: MODEL.into(),
        dimensions: 2,
        max_tokens: TOKENS,
    }
}

async fn enroll(db: &Database, root: &Path, login: &str, administer: bool) -> PrincipalIds {
    let ids = IdentityRepo::new(db.inner())
        .bootstrap_owner(
            login,
            "refinement-cli-fixture-unused-password-digest",
            &OperationId::new(login),
        )
        .await
        .unwrap();
    if administer {
        db.inner()
            .query("UPDATE $id SET role = 'admin' RETURN NONE")
            .bind(("id", ids.account.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
    }
    std::fs::write(root.join(login), ids.workspace.as_record_id().to_sql()).unwrap();
    ids
}

async fn inspect(db: &mut Database, expected_consent: Option<(u64, bool)>, configured: bool) {
    let system = db.claim_system_authority().unwrap();
    let access = AccessRepo::new(db);
    for login in [LOCAL, OTHER] {
        let account = AuthRepo::new(db.inner())
            .account_by_login(login)
            .await
            .unwrap()
            .unwrap()
            .account_id()
            .unwrap();
        let principal = access.local_principal(&system, &account).await.unwrap();
        let owner = access.authorize(&principal).await.unwrap();
        let effective = RefinementRepo::new(db)
            .effective_policy(&owner)
            .await
            .unwrap();
        assert_eq!(effective.configured.is_some(), configured);
        let expected = if login == LOCAL {
            expected_consent
        } else {
            None
        };
        assert_eq!(
            effective
                .consent
                .map(|c| (c.revision, c.revoked_at.is_some())),
            expected,
            "persisted consent for {login}"
        );
        assert_eq!(
            effective.inference_admitted,
            expected == Some((1, false)),
            "inference admission for {login}"
        );
    }
    assert!(
        system_config::access_recording_armed_at(db.inner())
            .await
            .unwrap()
            .is_none()
    );
}

async fn seed_consent(
    db: &Database,
    owner: &OwnerContext,
    system: &alexandria_storage::access::SystemContext,
    prefix: String,
    revoked: bool,
) {
    let routes = RoutePair::resolve(
        Route {
            protocol: Protocol::OpenAiChatCompletions,
            api_prefix: prefix,
            model: "fixture-proposer".into(),
            credential_ref: Some("REFINEMENT_CLI_SECRET".into()),
            headers: BTreeMap::new(),
            output_limit_field: None,
        },
        None,
    )
    .unwrap();
    let configured = ReconciledRefinement::reconcile(
        db,
        system,
        Some(routes),
        &InferencePolicyInputs {
            prompt_template: b"refinement cli fixture policy",
            prompt_template_version: "1",
            operation_schema_version: "1",
            settings: &BTreeMap::new(),
        },
        ApprovalMode::SecondaryLlm,
    )
    .await
    .unwrap()
    .unwrap();
    let repo = RefinementRepo::new(db);
    let grant = repo
        .set_consent(
            owner,
            0,
            ConsentChoice::Grant {
                disclosed_policy_revision: configured.policy().revision,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(grant.revision, 1);
    assert!(
        repo.effective_policy(owner)
            .await
            .unwrap()
            .inference_admitted,
        "the seeded grant must really be admitted before invalidation"
    );
    if revoked {
        repo.set_consent(
            owner,
            grant.revision,
            ConsentChoice::Revoke,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        assert!(
            !repo
                .effective_policy(owner)
                .await
                .unwrap()
                .inference_admitted
        );
    }
}

#[tokio::test]
#[ignore = "each persistent store open runs in its own process"]
async fn fixture_child() {
    let root = PathBuf::from(std::env::var_os("REFINEMENT_CLI_ROOT").unwrap());
    let mode = std::env::var("REFINEMENT_CLI_MODE").unwrap();
    let mut db = Database::connect_persistent(&root.join("store"))
        .await
        .unwrap();
    match mode.as_str() {
        "ready" => {
            schema::migrate(db.inner()).await.unwrap();
            system_config::check_embedding_space(db.inner(), &embedding_space())
                .await
                .unwrap();
            enroll(&db, &root, LOCAL, true).await;
            enroll(&db, &root, OTHER, false).await;
        }
        "legacy" => {
            for (_, _, sql) in schema::MIGRATIONS
                .iter()
                .take_while(|(version, _, _)| *version <= 8)
            {
                db.inner().query(*sql).await.unwrap().check().unwrap();
            }
            system_config::set_config(db.inner(), "schema_version", "8")
                .await
                .unwrap();
            system_config::check_embedding_space(db.inner(), &embedding_space())
                .await
                .unwrap();
            db.inner().query("CREATE fact SET content = 'legacy refinement fixture', embedding = [1.0, 0.0] RETURN NONE")
                .await.unwrap().check().unwrap();
        }
        "interrupt-ownership" => {
            schema::migrate(db.inner()).await.unwrap();
            let system = db.claim_system_authority().unwrap();
            let ids = enroll(&db, &root, LOCAL, true).await;
            let receipt = BackupReceipt::read(&root.join("backup")).unwrap();
            OwnershipMigration::new(&db)
                .begin(&system, ids, receipt)
                .await
                .unwrap();
        }
        "interrupt-embedding" => {
            let journal = ReembedJournal {
                run_id: "refinement-cli-pending-embedding".into(),
                source_manifest_sha256: std::fs::read_to_string(root.join("manifest-sha256"))
                    .unwrap(),
                source_space: embedding_space(),
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
        "seed-grant" | "seed-revoked-grant" | "suspend" => {
            let system = db.claim_system_authority().unwrap();
            let account = AuthRepo::new(db.inner())
                .account_by_login(LOCAL)
                .await
                .unwrap()
                .unwrap()
                .account_id()
                .unwrap();
            let access = AccessRepo::new(&db);
            let principal = access.local_principal(&system, &account).await.unwrap();
            let owner = access.authorize(&principal).await.unwrap();
            seed_consent(
                &db,
                &owner,
                &system,
                std::env::var("REFINEMENT_CLI_URL").unwrap(),
                mode == "seed-revoked-grant",
            )
            .await;
            if mode == "suspend" {
                db.inner()
                    .query("UPDATE $id SET status = 'suspended' RETURN NONE")
                    .bind(("id", account.as_record_id().clone()))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            }
        }
        "inspect-unconfigured" => inspect(&mut db, None, false).await,
        "inspect-no-consent" => inspect(&mut db, None, true).await,
        "inspect-granted" => inspect(&mut db, Some((1, false)), true).await,
        "inspect-revoked" => inspect(&mut db, Some((2, true)), true).await,
        _ => panic!("unknown fixture mode {mode}"),
    }
    // Surreal SDK drop is asynchronous; the next opener waits for this whole process to exit.
    std::process::exit(0);
}

#[test]
fn configured_routes_disclose_destinations_but_do_not_grant_consent() {
    let fixture = Fixture::ready();
    let output = fixture.run(&["refinement", "routes"]);
    let report = json_report(&output, "route disclosure");
    assert_eq!(report["consent_revision"], 0);
    assert_eq!(report["inference_admitted"], false);
    assert!(report["policy_revision"].as_u64().unwrap() > 0);
    let routes = &report["routes"];
    assert_eq!(routes["proposer"]["destination"]["host"], "127.0.0.1");
    assert_eq!(routes["proposer"]["destination"]["api_prefix"], "/v1");
    assert_eq!(routes["proposer"]["model"], "fixture-proposer");
    assert_eq!(routes["reviewer"], routes["proposer"]);
    assert_eq!(routes["reviewer_inherited"], true);
    let printed = transcript(&output);
    assert!(
        !printed.contains("fixture-secret-never-disclose")
            && !printed.contains("REFINEMENT_CLI_SECRET"),
        "disclosure must omit credentials and their references"
    );
    fixture.child("inspect-no-consent");
    refused_for(
        &fixture.refine(LOCAL, &fixture.workspace(LOCAL), &fixture.base_url()),
        "configured without consent",
        &["consent"],
    );
    fixture.assert_no_inference();
}

#[test]
fn explicit_disclosed_grant_writes_the_consent_revision_and_only_for_local_owner() {
    let fixture = Fixture::ready();
    let disclosure = json_report(
        &fixture.run(&["refinement", "routes"]),
        "route disclosure before grant",
    );
    refused_for(
        &fixture.run(&["refinement", "consent", "grant"]),
        "undisclosed grant",
        &["--policy-revision", "--consent-revision"],
    );
    fixture.child("inspect-no-consent");
    let grant = json_report(&fixture.grant(&disclosure), "explicit consent grant");
    assert_eq!(grant["consent_revision"], 1);
    assert_eq!(grant["inference_admitted"], true);
    fixture.child("inspect-granted");
    refused_for(
        &fixture.grant(&disclosure),
        "stale disclosed consent revision",
        &["stale"],
    );
    fixture.child("inspect-granted");
    fixture.assert_no_inference();
}

#[test]
fn another_account_cannot_be_selected_for_a_consent_grant() {
    let fixture = Fixture::ready();
    let disclosure = json_report(
        &fixture.run(&["refinement", "routes"]),
        "route disclosure before account-selection refusal",
    );
    let resolved_policy = revision(&disclosure, "policy_revision");
    refused_for(
        &fixture.run(&[
            "refinement",
            "consent",
            "grant",
            "--policy-revision",
            &resolved_policy,
            "--consent-revision",
            "0",
            "--account",
            OTHER,
        ]),
        "account selection",
        &["--account", "usage"],
    );
    fixture.child("inspect-no-consent");
    fixture.assert_no_inference();
}

#[test]
fn local_approval_and_revocation_are_persisted_without_inferring() {
    let fixture = Fixture::ready();
    let disclosure = json_report(
        &fixture.run(&["refinement", "routes"]),
        "route disclosure before approval and revoke",
    );
    let grant = json_report(&fixture.grant(&disclosure), "consent grant before approval");
    let secondary = json_report(
        &fixture.run(&["refinement", "approval", "secondary-llm"]),
        "secondary approval preference",
    );
    assert_eq!(secondary["approval_mode"], "secondary_llm");
    let human = json_report(
        &fixture.run(&["refinement", "approval", "human"]),
        "return to human approval",
    );
    assert_eq!(human["approval_mode"], "human");
    let revoked = json_report(
        &fixture.run(&[
            "refinement",
            "consent",
            "revoke",
            "--consent-revision",
            &revision(&grant, "consent_revision"),
        ]),
        "explicit consent revocation",
    );
    assert_eq!(revoked["consent_revision"], 2);
    assert_eq!(revoked["inference_admitted"], false);
    fixture.child("inspect-revoked");
    fixture.assert_no_inference();
}

#[test]
fn held_store_refuses_before_writes_without_truncating_lock() {
    let fixture = Fixture::ready();
    let store = fixture.path("store");
    let lock_path = store.join("LOCK");
    let original_lock = b"refinement CLI must not truncate this held LOCK\n";
    std::fs::write(&lock_path, original_lock).unwrap();
    let hold = OfflineStoreGuard::acquire(&store).unwrap();
    let grant = fixture.run(&[
        "refinement",
        "consent",
        "grant",
        "--policy-revision",
        "1",
        "--consent-revision",
        "0",
    ]);
    let run = fixture.refine(LOCAL, &fixture.workspace(LOCAL), &fixture.base_url());
    assert_eq!(std::fs::read(lock_path).unwrap().as_slice(), original_lock);
    refused_for(&grant, "grant while held", &["single-writer", "stop"]);
    refused_for(&run, "refine while held", &["single-writer", "stop"]);
    drop(hold);
    fixture.child("inspect-unconfigured");
    fixture.assert_no_inference();
}

#[test]
fn explicit_refine_refuses_incomplete_ownership_before_provider_loading() {
    let fixture = Fixture::new();
    fixture.child("legacy");
    OfflineStoreGuard::acquire(&fixture.path("store"))
        .unwrap()
        .copy_to(&fixture.path("backup"))
        .unwrap();
    fixture.child("interrupt-ownership");
    refused_for(
        &fixture.refine(LOCAL, &fixture.workspace(LOCAL), &fixture.base_url()),
        "incomplete ownership",
        &["ownership migration incomplete"],
    );
    fixture.assert_no_inference();
}

#[test]
fn explicit_refine_refuses_incomplete_embedding_before_provider_loading() {
    let fixture = Fixture::ready();
    let receipt = OfflineStoreGuard::acquire(&fixture.path("store"))
        .unwrap()
        .copy_to(&fixture.path("backup"))
        .unwrap();
    std::fs::write(fixture.path("manifest-sha256"), receipt.sha256).unwrap();
    fixture.child("interrupt-embedding");
    refused_for(
        &fixture.refine(LOCAL, &fixture.workspace(LOCAL), &fixture.base_url()),
        "incomplete embedding",
        &["embedding migration incomplete", "--resume"],
    );
    fixture.assert_no_inference();
}

#[test]
fn revoked_grant_cannot_be_spent_by_explicit_refine() {
    let fixture = Fixture::ready();
    fixture.child("seed-revoked-grant");
    refused_for(
        &fixture.refine(LOCAL, &fixture.workspace(LOCAL), &fixture.base_url()),
        "revoked grant",
        &["consent"],
    );
    fixture.child("inspect-revoked");
    fixture.assert_no_inference();
}

#[test]
fn changed_destination_refuses_old_consent_and_stale_disclosure() {
    let fixture = Fixture::ready();
    fixture.child("seed-grant");
    let changed = format!("{}/changed", fixture.base_url());
    refused_for(
        &fixture.refine(LOCAL, &fixture.workspace(LOCAL), &changed),
        "changed destination",
        &["consent"],
    );
    let report = json_report(
        &fixture
            .command(LOCAL, &changed)
            .args(["refinement", "routes"])
            .output()
            .unwrap(),
        "changed destination disclosure",
    );
    assert_eq!(report["consent_revision"], 1, "history must remain visible");
    assert_eq!(report["inference_admitted"], false);
    let outdated = json!({"policy_revision": report["policy_revision"].as_u64().unwrap() - 1, "consent_revision": 1});
    let stale = fixture
        .command(LOCAL, &changed)
        .args([
            "refinement",
            "consent",
            "grant",
            "--policy-revision",
            &revision(&outdated, "policy_revision"),
            "--consent-revision",
            "1",
        ])
        .output()
        .unwrap();
    refused_for(&stale, "stale route disclosure", &["stale"]);
    refused_for(
        &fixture.refine(LOCAL, &fixture.workspace(LOCAL), &fixture.base_url()),
        "superseded destination consent must not revive when the old route returns",
        &["consent"],
    );
    fixture.assert_no_inference();
}

#[test]
fn explicit_refine_checks_administrator_and_never_adopts_another_workspace() {
    let fixture = Fixture::ready();
    fixture.child("seed-grant");
    refused_for(
        &fixture.refine(LOCAL, &fixture.workspace(OTHER), &fixture.base_url()),
        "foreign workspace",
        &["workspace", "trusted-local"],
    );
    refused_for(
        &fixture.refine(OTHER, &fixture.workspace(OTHER), &fixture.base_url()),
        "owner without admin role",
        &["admin"],
    );
    fixture.assert_no_inference();
}

#[test]
fn explicit_refine_refuses_a_suspended_local_account_before_inference() {
    let fixture = Fixture::ready();
    fixture.child("suspend");
    refused_for(
        &fixture.refine(LOCAL, &fixture.workspace(LOCAL), &fixture.base_url()),
        "suspended local account",
        &["trusted-local", "active"],
    );
    fixture.assert_no_inference();
}

#[test]
fn bare_dream_and_all_never_load_refinement() {
    let fixture = Fixture::ready();
    fixture.child("seed-grant");
    for args in [&["dream"][..], &["dream", "--job", "all"][..]] {
        let output = fixture.run(args);
        assert!(
            output.status.success(),
            "{args:?} must succeed with a poisoned embedding cache: {}",
            transcript(&output)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let jobs: Vec<_> = stdout
            .lines()
            .filter_map(|line| line.strip_prefix("job="))
            .map(|line| line.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(
            jobs,
            ["sweep", "cluster", "merge", "collapse", "appraise"],
            "{stdout}"
        );
        fixture.assert_no_inference();
    }
    fixture.child("inspect-granted");
}
