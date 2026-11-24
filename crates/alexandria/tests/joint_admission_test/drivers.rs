use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use alexandria_engine::refinement::policy::{InferencePolicyInputs, Protocol, Route, RoutePair};
use alexandria_mcp::error::ServiceError;
use alexandria_mcp::refinement::provider::{RouteProvider, prompt_template};
use alexandria_mcp::refinement::worker::{Dispatch, Worker, WorkerOutput, WorkerSettings};
use alexandria_mcp::refinement::{ReconciledRefinement, RefinementService};
use alexandria_storage::models::identity::{
    AccountId, FactId, MemoryScope, Partition, RecordId, SemanticOperationId,
};
use alexandria_storage::models::mutation::{FactSnapshot, SemanticSnapshot};
use alexandria_storage::models::refinement::{
    ApprovalMode, ConsentChoice, ProposalRevision, ProposalState,
};
use alexandria_storage::offline_copy::OfflineStoreGuard;
use alexandria_storage::ownership_migration::{
    BackupReceipt, OwnershipMigration, check_ownership_ready,
};
use alexandria_storage::project_handle::parse_project_handle;
use alexandria_storage::repos::classification_repo::ClassificationRepo;
use alexandria_storage::repos::refinement_repo::{
    EnqueueRequest, EvidenceSelection, WorkKind, WorkSelection,
};
use alexandria_storage::repos::{MutationRepo, RefinementRepo};
use axum::{Json, extract::State, http::StatusCode as HttpStatus, routing::post};
use tokio::sync::Notify;

use super::{
    Account, Adapter, Arc, AuthRepo, CleanupTrace, Context, Database, Host, IdentityRepo,
    MigrationTrace, NearEmbedding, OperationId, OwnerContext, Result, RollbackTrace, Router,
    SecondaryTrace, Utc, Value, ensure, json, schema,
};

fn collision_id(reversed: bool, other_owner: bool) -> FactId {
    // Swapping runs exercises both numeric and quoted/opaque ids on both ownership sides.
    let key = if reversed ^ other_owner {
        RecordId::new("fact", 10023_i64)
    } else {
        RecordId::new("fact", "same label:quoted/opaque")
    };
    FactId::try_from(key).expect("fixture fact id")
}

fn project(account: &Account) -> Result<MemoryScope> {
    Ok(MemoryScope::Project(parse_project_handle(
        &account.project,
    )?))
}

async fn fact_state(host: &Host, account: &Account, fact: &FactId) -> Result<FactSnapshot> {
    MutationRepo::new(host.db.inner())
        .state(&account.owner, fact)
        .await?
        .context("owner-visible fact state")
}

async fn classify(host: &Host, account: &Account, fact: &FactId) -> Result<()> {
    let state = fact_state(host, account, fact).await?;
    let repo = ClassificationRepo::new(host.db.inner(), &account.owner);
    let proposal = repo
        .propose(
            fact,
            state.version,
            project(account)?,
            Some("Owner checked this fixture scope."),
        )
        .await?;
    let result = repo
        .apply_batch(&[proposal], &OperationId::new("joint-classify"))
        .await?;
    ensure!(
        result.applied.len() == 1 && result.conflicts.is_empty() && result.stopped.is_none(),
        "classification did not commit: {result:?}"
    );
    Ok(())
}

async fn recall(host: &Host, account: &Account, in_project: bool, query: &str) -> Result<Value> {
    let mut args = json!({"query":query,"scope":"personal"});
    if in_project {
        args["scope"] = json!("project");
        args["project"] = json!(account.project);
    }
    host.call(account, "retrieve_memories", args).await
}

pub(super) async fn migrate_classify_restart(
    path: &Path,
    reversed: bool,
) -> Result<MigrationTrace> {
    // Each store open runs in a separate process: Surreal SDK drop does not synchronously release LOCK.
    migration_child(path, reversed, "seed")?;
    OfflineStoreGuard::acquire(&path.join("store"))?.copy_to(&path.join("backup"))?;
    let before = migration_child(path, reversed, "migrate")?;
    let after = migration_child(path, reversed, "restart")?;
    Ok(MigrationTrace {
        before_classification: before["recall"].clone(),
        unassigned: before["unassigned"].clone(),
        after_restart: after["owner"].clone(),
        other_owner: after["other"].clone(),
    })
}

fn migration_child(path: &Path, reversed: bool, mode: &str) -> Result<Value> {
    let output = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "drivers::migration_process",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .env("JOINT_ADMISSION_ROOT", path)
        .env("JOINT_ADMISSION_MODE", mode)
        .env(
            "JOINT_ADMISSION_REVERSED",
            if reversed { "yes" } else { "no" },
        )
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "migration child {mode} (reversed={reversed}, {}):\n--- stderr ---\n{stderr}\n--- stdout ---\n{stdout}",
            output.status
        );
    }
    let stdout = String::from_utf8(output.stdout)?;
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("JOINT_ADMISSION_JSON="))
        .context("child report")?;
    Ok(serde_json::from_str(line)?)
}

#[tokio::test]
#[ignore = "parent runs separate processes for persistent database opens"]
async fn migration_process() -> Result<()> {
    let root =
        std::path::PathBuf::from(std::env::var_os("JOINT_ADMISSION_ROOT").context("fixture root")?);
    let mode = std::env::var("JOINT_ADMISSION_MODE")?;
    let reversed = std::env::var("JOINT_ADMISSION_REVERSED")? == "yes";
    let fact = collision_id(reversed, false);
    let mut db = Database::connect_persistent(&root.join("store")).await?;
    schema::migrate(db.inner()).await?;
    let system = db.claim_system_authority()?;
    let payload = match mode.as_str() {
        "seed" => {
            db.inner().query("CREATE $fact SET content = 'legacy review marker', embedding = $vector, tags = [], created_at = time::now() RETURN NONE;")
                .bind(("fact", fact.as_record_id().clone())).bind(("vector", vec![1.0_f32, 0.001])).await?.check()?;
            Value::Null
        }
        "migrate" => migrate_and_file(&root, db, system, &fact, reversed).await?,
        "restart" => {
            check_ownership_ready(db.inner()).await?;
            let host = Host::with_db(db, system).await?;
            let a = reconnect(&host, "joint-a", Adapter::Pi).await?;
            let b = reconnect(&host, "joint-b", Adapter::Claude).await?;
            let (owner, other) = tokio::try_join!(
                recall(&host, &a, true, "legacy review marker"),
                recall(&host, &b, true, "legacy review marker")
            )?;
            json!({"owner":owner,"other":other})
        }
        _ => anyhow::bail!("unknown fixture process mode"),
    };
    println!("JOINT_ADMISSION_JSON={payload}");
    Ok(())
}

async fn reconnect(host: &Host, login: &str, adapter: Adapter) -> Result<Account> {
    let row = AuthRepo::new(host.db.inner())
        .account_by_login(login)
        .await?
        .context("persisted account")?;
    let account = AccountId::try_from(row.id.context("persisted account ID")?)?;
    host.connect(host.owner_by_id(&account).await?, adapter)
        .await
}

async fn migrate_and_file(
    root: &Path,
    db: Database,
    system: super::SystemContext,
    fact: &FactId,
    reversed: bool,
) -> Result<Value> {
    ensure!(
        check_ownership_ready(db.inner()).await.is_err(),
        "legacy source must not be admitted before adoption"
    );
    let password = format!("migration-fixture-{}", uuid::Uuid::new_v4());
    let digest = AuthRepo::create_password_digest(&password).await?;
    let owner = IdentityRepo::new(db.inner())
        .bootstrap_owner(
            "joint-a",
            digest.as_str(),
            &OperationId::new("migrate-joint-owner"),
        )
        .await?;
    let migration = OwnershipMigration::new(&db);
    let run = migration
        .begin(
            &system,
            owner.clone(),
            BackupReceipt::read(&root.join("backup"))?,
        )
        .await?;
    let mut ready = false;
    for _ in 0..32 {
        if migration.step(&run, 1).await?.ready_to_verify {
            ready = true;
            break;
        }
    }
    ensure!(ready, "migration did not finish bounded single-row pages");
    migration.verify_and_complete(&run).await?;
    check_ownership_ready(db.inner()).await?;
    let host = Host::with_db(db, system).await?;
    let (a, b) = host.invited_pair(&owner, &password, reversed).await?;
    seed(
        &host,
        &b,
        &collision_id(reversed, true),
        project(&b)?,
        "PRIVATE-B equally relevant legacy counterpart",
        false,
    )
    .await?;
    let unassigned = host.call(&a, "list_unassigned_memories", json!({})).await?;
    let before = recall(&host, &a, true, "legacy review marker").await?;
    classify(&host, &a, fact).await.with_context(|| {
        use alexandria_storage::models::identity::ToSql;
        format!(
            "classify migrated fixture fact {}",
            fact.as_record_id().to_sql()
        )
    })?;
    Ok(json!({"unassigned":unassigned,"recall":before}))
}

async fn seed(
    host: &Host,
    account: &Account,
    id: &FactId,
    scope: MemoryScope,
    text: &str,
    hidden: bool,
) -> Result<()> {
    let (scope, project) = match scope {
        MemoryScope::Personal => ("personal", None),
        MemoryScope::Project(project) => ("project", Some(project.as_record_id().clone())),
        MemoryScope::Unassigned => ("unassigned", None),
    };
    host.db.inner().query("CREATE $fact SET account = $owner, workspace = $workspace, scope = $scope, project = $project, \
        content = $text, embedding = $vector, tags = [], semantic_revision = 1, created_at = time::now(),\
        quarantined_at = IF $hidden THEN time::now() ELSE NONE END RETURN NONE;")
        .bind(("fact", id.as_record_id().clone())).bind(("owner", account.owner.account().as_record_id().clone()))
        .bind(("workspace", account.owner.workspace().as_record_id().clone())).bind(("scope", scope)).bind(("project", project))
        .bind(("text", text.to_owned())).bind(("vector", vec![1.0_f32, if text.contains("PRIVATE-B") { 0.002 } else { 0.001 }]))
        .bind(("hidden", hidden)).await?.check()?;
    Ok(())
}

#[derive(Default)]
struct InferenceState(Mutex<Vec<Value>>);

struct InferenceMock {
    prefix: String,
    state: Arc<InferenceState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for InferenceMock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl InferenceMock {
    async fn open() -> Result<Self> {
        let state = Arc::new(InferenceState::default());
        let app = Router::new()
            .route("/v1/chat/completions", post(infer))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let prefix = format!("http://{}/v1", listener.local_addr()?);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("fixture inference listener");
        });
        Ok(Self {
            prefix,
            state,
            task,
        })
    }

    async fn grant(&self, host: &Host, owner: &OwnerContext) -> Result<ReconciledRefinement> {
        let route = Route {
            protocol: Protocol::OpenAiChatCompletions,
            api_prefix: self.prefix.clone(),
            model: "joint-review".into(),
            credential_ref: None,
            headers: BTreeMap::new(),
            output_limit_field: None,
        };
        let config = ReconciledRefinement::reconcile(
            &host.db,
            &host.system,
            Some(RoutePair::resolve(route, None)?),
            &InferencePolicyInputs {
                prompt_template: prompt_template().as_bytes(),
                prompt_template_version: "1",
                operation_schema_version: "1",
                settings: &BTreeMap::new(),
            },
            ApprovalMode::SecondaryLlm,
        )
        .await?
        .context("configured fixture route")?;
        RefinementService::new(&host.db, Some(&config))
            .consent(
                owner,
                0,
                ConsentChoice::Grant {
                    disclosed_policy_revision: config.policy().revision,
                },
            )
            .await?;
        Ok(config)
    }

    fn requests(&self) -> Vec<Value> {
        self.state.0.lock().expect("inference capture").clone()
    }
}

async fn infer(
    State(state): State<Arc<InferenceState>>,
    Json(wire): Json<Value>,
) -> Result<Json<Value>, HttpStatus> {
    let body = wire["messages"][0]["content"]
        .as_str()
        .ok_or(HttpStatus::BAD_REQUEST)?;
    let data: Value = serde_json::from_str(body).map_err(|_| HttpStatus::BAD_REQUEST)?;
    let focal = &data["evidence"]["focal"];
    let id = focal["id"].as_str().ok_or(HttpStatus::BAD_REQUEST)?;
    let content = focal["content"].as_str().ok_or(HttpStatus::BAD_REQUEST)?;
    let answer = if data["proposal"].is_object() {
        json!({"verdict":"approve_exact","rationale":"Retains the supplied qualification."})
    } else {
        let after = if content.contains("quarantine marker") {
            "PRIVATE-A quarantine marker."
        } else {
            "PRIVATE-A rewrite"
        };
        json!({"category":"rewrite","operation":{"kind":"rewrite","source":id,"content":after},
               "rationale":"Keep the supported wording only.","uncertainty":""})
    };
    state.0.lock().expect("inference capture").push(wire);
    Ok(Json(
        json!({"model":"joint-review","choices":[{"finish_reason":"stop","message":{"role":"assistant","content":answer.to_string()}}]}),
    ))
}

async fn assessment(
    host: &Host,
    owner: &OwnerContext,
    config: &ReconciledRefinement,
    scope: MemoryScope,
    selection: WorkSelection,
    key: &str,
) -> Result<WorkerOutput> {
    let (kind, source_revision) = match &selection {
        WorkSelection::Evidence(selected) => {
            let state = MutationRepo::new(host.db.inner())
                .state(owner, &selected.focal)
                .await?
                .context("selected evidence no longer belongs to the owner")?;
            (WorkKind::AssessUnassigned, Some(state.version))
        }
        _ => (WorkKind::Assess, None),
    };
    RefinementService::new(&host.db, Some(config))
        .enqueue(
            owner,
            EnqueueRequest {
                owner: owner.account().clone(),
                partition: Partition {
                    workspace: owner.workspace().clone(),
                    scope,
                },
                kind,
                source_revision,
                selection,
                idempotency_key: key.into(),
            },
        )
        .await?;
    let settings = WorkerSettings::default();
    let provider = RouteProvider::new(config, settings);
    let dispatch = Worker::new(&host.db, &host.system, config, settings)
        .step(Utc::now(), &provider)
        .await
        .with_context(|| format!("dispatch assessment {key}"))?;
    let Dispatch::Completed { output, .. } = dispatch else {
        anyhow::bail!("fixture assessment did not finish: {dispatch:?}")
    };
    Ok(output)
}

async fn pending(
    host: &Host,
    owner: &OwnerContext,
    output: WorkerOutput,
) -> Result<ProposalRevision> {
    let WorkerOutput::Proposal(proposal) = output else {
        anyhow::bail!("expected a human proposal: {output:?}")
    };
    Ok(RefinementRepo::new(&host.db)
        .persist_proposal(owner, proposal)
        .await?)
}

pub(super) async fn cleanup_classify(
    host: &Host,
    a: &Account,
    b: &Account,
    content: &str,
) -> Result<CleanupTrace> {
    let focal = collision_id(false, false);
    tokio::try_join!(
        seed(host, a, &focal, MemoryScope::Unassigned, content, true),
        async {
            seed(
                host,
                b,
                &collision_id(false, true),
                MemoryScope::Unassigned,
                "PRIVATE-B quarantine marker",
                true,
            )
            .await
        }
    )?;
    let before = fact_state(host, a, &focal)
        .await?
        .visibility
        .quarantined_at
        .context("initial quarantine")?;
    let mock = InferenceMock::open().await?;
    let config = mock.grant(host, &a.owner).await?;
    let output = assessment(
        host,
        &a.owner,
        &config,
        MemoryScope::Unassigned,
        WorkSelection::Evidence(EvidenceSelection {
            focal: focal.clone(),
            related: Vec::new(),
        }),
        "unassigned-lesson",
    )
    .await?;
    let proposal = pending(host, &a.owner, output).await?;
    RefinementService::new(&host.db, Some(&config))
        .with_embedding(&NearEmbedding)
        .approve(&a.owner, &proposal, &OperationId::new("clean-unassigned"))
        .await
        .context("apply human-approved Unassigned rewrite")?;
    let cleaned = fact_state(host, a, &focal).await?;
    ensure!(
        cleaned.content == "PRIVATE-A quarantine marker.",
        "manual cleanup did not rewrite the selected memory"
    );
    classify(host, a, &focal).await?;
    let after = fact_state(host, a, &focal).await?;
    let model_request = mock
        .requests()
        .into_iter()
        .next()
        .context("observed local model request")?;
    let message = &model_request["messages"][0];
    let data: Value = serde_json::from_str(message["content"].as_str().context("model input")?)?;
    ensure!(
        message["role"] == "user" && data["evidence"]["focal"]["content"] == content,
        "fixture text was not isolated as evidence data"
    );
    ensure!(
        !data["instruction"]
            .as_str()
            .context("trusted instruction")?
            .contains(super::INJECTION_MARKER),
        "fixture text entered trusted instructions"
    );
    Ok(CleanupTrace {
        model_request,
        scoped_recall: recall(host, a, true, "quarantine marker").await?,
        classified_scope: if after.scope == project(a)? {
            "project".into()
        } else {
            format!("{:?}", after.scope)
        },
        quarantined_before: before.to_rfc3339(),
        quarantined_after: after.visibility.quarantined_at.map(|at| at.to_rfc3339()),
        deleted: after.visibility.deleted,
    })
}

#[derive(Default)]
struct EmbeddingGate {
    entered: Notify,
    release: Notify,
}

#[async_trait::async_trait]
impl alexandria_pipeline::embedding::EmbeddingProvider for EmbeddingGate {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.entered.notify_one();
        self.release.notified().await;
        NearEmbedding.embed(texts).await
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "task23-near"
    }
}

pub(super) async fn revoke_during_secondary(
    host: &Host,
    a: &Account,
    b: &Account,
) -> Result<SecondaryTrace> {
    let focal = collision_id(true, false);
    let assigned = project(a)?;
    seed(
        host,
        a,
        &focal,
        assigned.clone(),
        "PRIVATE-A original",
        false,
    )
    .await?;
    seed(
        host,
        a,
        &FactId::try_from(RecordId::new("fact", "manual-review"))?,
        MemoryScope::Personal,
        "PRIVATE-A pending human review",
        false,
    )
    .await?;
    seed(
        host,
        b,
        &collision_id(true, true),
        project(b)?,
        "PRIVATE-B original",
        false,
    )
    .await?;
    let mock = InferenceMock::open().await?;
    let config = mock.grant(host, &a.owner).await?;
    let embedding = EmbeddingGate::default();
    let service = RefinementService::new(&host.db, Some(&config)).with_embedding(&embedding);
    let draft = pending(
        host,
        &a.owner,
        assessment(
            host,
            &a.owner,
            &config,
            MemoryScope::Personal,
            WorkSelection::Scheduled,
            "human-first",
        )
        .await?,
    )
    .await?;
    let version = service.effective_policy(&a.owner).await?.policy.version();
    service
        .set_approval_mode(&a.owner, &version, ApprovalMode::SecondaryLlm)
        .await?;
    let verdict = assessment(
        host,
        &a.owner,
        &config,
        assigned,
        WorkSelection::Scheduled,
        "automatic-next",
    )
    .await?;
    let WorkerOutput::Automatic(verdict) = verdict else {
        anyhow::bail!("expected exact secondary approval: {verdict:?}")
    };
    let operation = OperationId::new("revoke-before-commit");
    let mut applying = Box::pin(service.apply_secondary(&host.system, &verdict, &operation));
    tokio::select! {
        result = &mut applying => anyhow::bail!("secondary application returned before embedding: {result:?}"),
        entered = tokio::time::timeout(super::DEADLINE, embedding.entered.notified()) => entered?,
    }
    let version = service.effective_policy(&a.owner).await?.policy.version();
    service
        .consent(&a.owner, version.consent_revision, ConsentChoice::Revoke)
        .await?;
    embedding.release.notify_one();
    ensure!(
        matches!(applying.await, Err(ServiceError::Stale)),
        "secondary authority was not fenced at commit"
    );
    let row = service
        .owner_proposal(&a.owner, &draft.id)
        .await?
        .context("existing owner review")?;
    let charged_calls = service.owner_usage(&a.owner).await?.account;
    ensure!(
        charged_calls == mock.requests().len() && charged_calls == 3,
        "expected one draft call, one automatic propose, and one review"
    );
    Ok(SecondaryTrace {
        charged_calls,
        status: if row.state == ProposalState::Pending && row.operation.is_none() {
            "pending".into()
        } else {
            format!("{:?}", row.state)
        },
        committed_applications: usize::from(
            MutationRepo::new(host.db.inner())
                .operation(&a.owner, operation.as_str())
                .await?
                .is_some(),
        ),
        live_memory: recall(host, a, true, "original").await?,
    })
}

pub(super) async fn apply_move_rollback(
    host: &Host,
    a: &Account,
    b: &Account,
) -> Result<RollbackTrace> {
    let fact = collision_id(false, false);
    seed(host, a, &fact, project(a)?, "PRIVATE-A original", false).await?;
    seed(
        host,
        b,
        &collision_id(false, true),
        project(b)?,
        "PRIVATE-B original",
        false,
    )
    .await?;
    let mock = InferenceMock::open().await?;
    let config = mock.grant(host, &a.owner).await?;
    let output = assessment(
        host,
        &a.owner,
        &config,
        project(a)?,
        WorkSelection::Scheduled,
        "rewrite-then-move",
    )
    .await?;
    let proposal = pending(host, &a.owner, output).await?;
    let service = RefinementService::new(&host.db, Some(&config)).with_embedding(&NearEmbedding);
    let apply = OperationId::new("apply-before-move");
    service.approve(&a.owner, &proposal, &apply).await?;
    let row = service
        .owner_proposal(&a.owner, &proposal.id)
        .await?
        .context("applied proposal")?;
    let journal = SemanticOperationId::try_from(row.operation.context("applied journal")?)?;
    let mutations = MutationRepo::new(host.db.inner());
    let premature_history = usize::from(
        mutations
            .operation_by_id(&b.owner, journal.as_record_id())
            .await?
            .is_some(),
    );
    let state = fact_state(host, a, &fact).await?;
    mutations
        .move_scope(
            &a.owner,
            fact,
            state.version,
            MemoryScope::Personal,
            &OperationId::new("later-move"),
        )
        .await?;
    let undo = OperationId::new("stale-rollback");
    ensure!(
        matches!(
            service.revert(&a.owner, &journal, &undo).await,
            Err(ServiceError::Stale)
        ),
        "rollback must conflict with the later move"
    );
    ensure!(
        mutations
            .operation(&a.owner, undo.as_str())
            .await?
            .is_none(),
        "stale rollback journaled a mutation"
    );
    let history = mutations
        .operation_by_id(&a.owner, journal.as_record_id())
        .await?
        .context("private owner audit")?;
    let bodies: Vec<_> = history
        .preimages
        .iter()
        .chain(&history.postimages)
        .filter_map(|snapshot| match snapshot {
            SemanticSnapshot::Fact(fact) => Some(fact.content.as_str()),
            _ => None,
        })
        .collect();
    Ok(RollbackTrace {
        premature_history,
        former_scope: recall(host, a, true, "rewrite").await?,
        current_scope: recall(host, a, false, "rewrite").await?,
        owner_history: json!(bodies),
    })
}
