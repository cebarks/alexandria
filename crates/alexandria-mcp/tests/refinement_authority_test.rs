//! Authority tests use the real worker and storage; only inference and local embeddings are fakes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use alexandria_engine::refinement::policy::{InferencePolicyInputs, Protocol, Route, RoutePair};
use alexandria_engine::refinement::validation::validate_proposal;
use alexandria_mcp::error::ServiceError;
use alexandria_mcp::refinement::inference::{FinishReason, InferenceError, InferenceReply};
use alexandria_mcp::refinement::worker::{
    Call, Dispatch, Stage, Worker, WorkerOutput, WorkerProvider, WorkerQuota, WorkerSettings,
};
use alexandria_mcp::refinement::{ReconciledRefinement, RefinementService, prepare_change};
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::access::{AccessRepo, OwnerContext, SystemContext};
use alexandria_storage::models::identity::{
    FactId, MemoryScope, OperationId, Partition, RecordId, ToSql,
};
use alexandria_storage::models::refinement::{
    ApprovalMode, ConsentChoice, ProposalRevision, ProposalState,
};
use alexandria_storage::repos::refinement_repo::{EnqueueRequest, WorkKind, WorkSelection};
use alexandria_storage::repos::{IdentityRepo, MutationRepo, RefinementRepo};
use alexandria_storage::{Database, schema};
use chrono::{DateTime, TimeDelta, Utc};
use surrealdb::types::Value;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const BEFORE: &str = "The server sometimes waits for its index.";
const AFTER: &str = "The server may wait for its index.";

fn now() -> DateTime<Utc> {
    DateTime::<Utc>::UNIX_EPOCH + TimeDelta::days(90)
}

struct Fixture {
    db: Database,
    system: SystemContext,
    owner: OwnerContext,
    config: ReconciledRefinement,
    fact: FactId,
}

impl Fixture {
    async fn new(automatic: bool) -> Self {
        let mut db = Database::connect_embedded().await.unwrap();
        let system = db.claim_system_authority().unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let seeded = IdentityRepo::new(db.inner())
            .bootstrap_owner(
                "quality-authority-owner",
                "$argon2id$fixture-prepared-hash-quality-authority-owner-padding",
                &OperationId::new("enroll-quality-authority"),
            )
            .await
            .unwrap();
        let access = AccessRepo::new(&db);
        let principal = access
            .local_principal(&system, &seeded.account)
            .await
            .unwrap();
        let owner = access.authorize(&principal).await.unwrap();
        let config = ReconciledRefinement::reconcile(
            &db,
            &system,
            Some(
                RoutePair::resolve(
                    Route {
                        protocol: Protocol::OpenAiChatCompletions,
                        api_prefix: "https://inference.example/v1".into(),
                        model: "fixture-model".into(),
                        credential_ref: None,
                        headers: BTreeMap::new(),
                        output_limit_field: None,
                    },
                    None,
                )
                .unwrap(),
            ),
            &InferencePolicyInputs {
                prompt_template: b"quality authority fixture",
                prompt_template_version: "1",
                operation_schema_version: "1",
                settings: &BTreeMap::new(),
            },
            ApprovalMode::SecondaryLlm,
        )
        .await
        .unwrap()
        .unwrap();
        let repo = RefinementRepo::new(&db);
        repo.set_consent(
            &owner,
            0,
            ConsentChoice::Grant {
                disclosed_policy_revision: config.policy().revision,
            },
            now() - TimeDelta::days(61),
        )
        .await
        .unwrap();
        if automatic {
            let service = RefinementService::new(&db, Some(&config));
            let version = service
                .effective_policy(&owner)
                .await
                .unwrap()
                .policy
                .version();
            service
                .set_approval_mode(&owner, &version, ApprovalMode::SecondaryLlm)
                .await
                .unwrap();
        }
        let record = RecordId::new("fact", "quality-authority-focal");
        db.inner()
            .query(
                "CREATE $id SET account = $account, workspace = $workspace, scope = 'personal', \
            content = $text, embedding = $vector, created_at = $created, \
            tags = ['quality-authority'], semantic_revision = 1 RETURN NONE;",
            )
            .bind(("id", record.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .bind(("text", BEFORE))
            .bind(("vector", vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))
            .bind((
                "created",
                surrealdb::types::Datetime::from(now() - TimeDelta::days(60)),
            ))
            .await
            .unwrap()
            .check()
            .unwrap();
        Self {
            db,
            system,
            owner,
            config,
            fact: FactId::try_from(record).unwrap(),
        }
    }

    fn service(&self) -> RefinementService<'_> {
        RefinementService::new(&self.db, Some(&self.config))
    }

    fn worker(&self) -> Worker<'_> {
        Worker::new(
            &self.db,
            &self.system,
            &self.config,
            WorkerSettings::default(),
        )
    }

    async fn assess(&self, inference: &Inference) -> WorkerOutput {
        self.service()
            .enqueue(
                &self.owner,
                EnqueueRequest {
                    owner: self.owner.account().clone(),
                    partition: Partition {
                        workspace: self.owner.workspace().clone(),
                        scope: MemoryScope::Personal,
                    },
                    kind: WorkKind::Assess,
                    source_revision: None,
                    selection: WorkSelection::Scheduled,
                    idempotency_key: "fixture-assessment".into(),
                },
            )
            .await
            .unwrap();
        let result = self.worker().step(now(), inference).await.unwrap();
        let Dispatch::Completed { output, .. } = result else {
            panic!("fixture assessment did not complete: {result:?}")
        };
        output
    }

    async fn human(&self, inference: &Inference) -> ProposalRevision {
        let WorkerOutput::Proposal(pending) = self.assess(inference).await else {
            panic!("human assessment did not produce a pending proposal")
        };
        RefinementRepo::new(&self.db)
            .persist_proposal(&self.owner, pending)
            .await
            .unwrap()
    }

    async fn revoke(&self) {
        let service = self.service();
        let version = service
            .effective_policy(&self.owner)
            .await
            .unwrap()
            .policy
            .version();
        service
            .consent(&self.owner, version.consent_revision, ConsentChoice::Revoke)
            .await
            .unwrap();
    }

    async fn assert_pending(&self, proposal: &ProposalRevision) {
        let row = self
            .service()
            .owner_proposal(&self.owner, &proposal.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, ProposalState::Pending);
        assert!(row.operation.is_none());
        assert_eq!(row.revision, proposal.revision.get());
    }

    async fn assert_content(&self, expected: &str) {
        let fact = MutationRepo::new(self.db.inner())
            .state(&self.owner, &self.fact)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fact.content, expected);
    }

    async fn semantic_state(&self) -> Vec<Vec<Value>> {
        let mut response = self.db.inner().query("SELECT * FROM fact ORDER BY id; \
            SELECT * FROM memory_edge ORDER BY id; SELECT * FROM semantic_operation ORDER BY id; \
            SELECT * FROM semantic_snapshot ORDER BY id; SELECT * FROM refinement_proposal ORDER BY id;")
            .await.unwrap().check().unwrap();
        (0..5).map(|index| response.take(index).unwrap()).collect()
    }

    async fn failed_attempts(&self) -> usize {
        let rows: Vec<Value> = self
            .db
            .inner()
            .query(
                "SELECT id FROM refinement_attempt WHERE account = $account AND state = 'failed';",
            )
            .bind(("account", self.owner.account().as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        rows.len()
    }
}

#[derive(Default)]
struct Inference(AtomicUsize);

fn rewrite(fact: &FactId) -> String {
    serde_json::json!({
        "category": "rewrite",
        "operation": { "kind": "rewrite", "source": fact.as_record_id().to_sql(), "content": AFTER },
        "rationale": "Retains the original qualification.",
        "uncertainty": ""
    }).to_string()
}

#[async_trait::async_trait]
impl WorkerProvider for Inference {
    async fn complete(
        &self,
        call: Call<'_>,
        quota: &dyn WorkerQuota,
        _: CancellationToken,
    ) -> Result<InferenceReply, InferenceError> {
        quota
            .reserve_attempt()
            .await
            .map_err(|_| InferenceError::QuotaDenied)?;
        quota
            .authorize_outbound()
            .await
            .map_err(|_| InferenceError::QuotaDenied)?;
        self.0.fetch_add(1, Ordering::SeqCst);
        let text = match call.stage {
            Stage::Primary => rewrite(&call.evidence.focal.id),
            Stage::Secondary => {
                assert!(call.proposal.is_some());
                r#"{"verdict":"approve_exact","rationale":"Equivalent claim."}"#.into()
            }
        };
        Ok(InferenceReply {
            text,
            finish_reason: FinishReason::Stop,
            usage: None,
            configured_model: "fixture-model".into(),
            reported_model: None,
        })
    }
}

#[derive(Default)]
struct Embedding {
    calls: AtomicUsize,
    fail: bool,
    overflow: bool,
    gate: Option<(Notify, Notify)>,
}

#[async_trait::async_trait]
impl EmbeddingProvider for Embedding {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            texts,
            [AFTER],
            "apply tried to embed text outside the approved change"
        );
        if let Some((entered, release)) = self.gate.as_ref() {
            entered.notify_one();
            release.notified().await;
        }
        if self.fail {
            anyhow::bail!("fixture embedding failure");
        }
        Ok(vec![
            vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
            texts.len()
        ])
    }

    fn dimensions(&self) -> usize {
        8
    }
    fn model_id(&self) -> &str {
        "fixture-model"
    }
    fn overflow(&self, _: &str) -> Option<usize> {
        self.overflow.then_some(8193)
    }
}

#[tokio::test]
async fn prepare_change_rejects_locked_budget_overflow_before_embedding() {
    let fx = Fixture::new(false).await;
    let proposal = fx.human(&Inference::default()).await;
    let validated = validate_proposal(&proposal.evidence, &rewrite(&fx.fact)).unwrap();
    let allowed = Embedding::default();
    let prep = prepare_change(&validated, &allowed)
        .await
        .expect("within-budget preparation must succeed");
    assert_eq!(allowed.calls.load(Ordering::SeqCst), 1);
    assert_eq!(prep.quality_embeddings.len(), 1);
    let oversized = Embedding {
        overflow: true,
        ..Default::default()
    };
    let result = prepare_change(&validated, &oversized).await;
    assert!(
        matches!(result, Err(ServiceError::InvalidInput(_))),
        "overflow must be reported distinctly: {result:?}"
    );
    assert_eq!(oversized.calls.load(Ordering::SeqCst), 0);
    fx.assert_pending(&proposal).await;
    fx.assert_content(BEFORE).await;
}

#[tokio::test]
async fn existing_human_proposal_applies_after_egress_revocation_without_new_inference() {
    let fx = Fixture::new(false).await;
    let inference = Inference::default();
    let proposal = fx.human(&inference).await;
    fx.revoke().await;
    let embedding = Embedding::default();
    let applied = fx
        .service()
        .with_embedding(&embedding)
        .approve(
            &fx.owner,
            &proposal,
            &OperationId::new("approve-after-revoke"),
        )
        .await
        .expect("existing human proposal remains applicable after egress revocation");
    fx.assert_content(AFTER).await;
    assert!(!applied.replayed);
    assert_eq!(inference.0.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn current_secondary_authority_applies_exactly_the_candidate() {
    let fx = Fixture::new(true).await;
    let inference = Inference::default();
    let WorkerOutput::Automatic(verdict) = fx.assess(&inference).await else {
        panic!("missing exact automatic verdict")
    };
    assert!(fx.worker().authorize_application(&verdict).await.unwrap());
    let embedding = Embedding::default();
    fx.service()
        .with_embedding(&embedding)
        .apply_secondary(&fx.system, &verdict, &OperationId::new("automatic"))
        .await
        .expect("current exact secondary approval must apply");
    fx.assert_content(AFTER).await;
    assert_eq!(inference.0.load(Ordering::SeqCst), 2);
    assert_eq!(embedding.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn secondary_revocation_during_embedding_wins_at_the_transaction_boundary() {
    let fx = Fixture::new(true).await;
    let inference = Inference::default();
    let WorkerOutput::Automatic(verdict) = fx.assess(&inference).await else {
        panic!("missing exact automatic verdict")
    };
    let before = fx.semantic_state().await;
    let embedding = Embedding {
        gate: Some((Notify::new(), Notify::new())),
        ..Default::default()
    };
    let (entered, release) = embedding.gate.as_ref().unwrap();
    let service = fx.service().with_embedding(&embedding);
    let operation = OperationId::new("automatic-revoked-in-flight");
    let mut applying = Box::pin(service.apply_secondary(&fx.system, &verdict, &operation));
    tokio::select! {
        premature = &mut applying => panic!("application resolved before guarded preparation: {premature:?}"),
        _ = entered.notified() => {},
    }
    fx.revoke().await;
    release.notify_one();
    let result = applying.await;
    assert!(
        matches!(result, Err(ServiceError::Stale)),
        "consent changed during preparation: {result:?}"
    );
    assert_eq!(fx.semantic_state().await, before);
    assert!(!fx.worker().authorize_application(&verdict).await.unwrap());
    assert_eq!(inference.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn crash_rediscovery_does_not_reuse_a_stale_automatic_verdict() {
    let fx = Fixture::new(true).await;
    let inference = Inference::default();
    let WorkerOutput::Automatic(verdict) = fx.assess(&inference).await else {
        panic!("missing exact automatic verdict")
    };
    assert!(fx.worker().authorize_application(&verdict).await.unwrap());
    fx.worker().recover_interrupted(now()).await.unwrap();
    assert!(!fx.worker().authorize_application(&verdict).await.unwrap());
    let before = fx.semantic_state().await;
    let embedding = Embedding::default();
    let result = fx
        .service()
        .with_embedding(&embedding)
        .apply_secondary(
            &fx.system,
            &verdict,
            &OperationId::new("stale-after-recovery"),
        )
        .await;
    assert!(
        matches!(result, Err(ServiceError::Stale)),
        "obsolete generation must refuse: {result:?}"
    );
    assert_eq!(fx.semantic_state().await, before);
    assert_eq!(embedding.calls.load(Ordering::SeqCst), 0);
    assert_eq!(inference.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failed_embedding_leaves_pending_and_requires_explicit_new_approval() {
    let fx = Fixture::new(false).await;
    let inference = Inference::default();
    let proposal = fx.human(&inference).await;
    let before = fx.semantic_state().await;
    let failed = fx.failed_attempts().await;
    let embedding = Embedding {
        fail: true,
        ..Default::default()
    };
    assert!(
        fx.service()
            .with_embedding(&embedding)
            .approve(
                &fx.owner,
                &proposal,
                &OperationId::new("failed-preparation")
            )
            .await
            .is_err()
    );
    assert_eq!(
        fx.failed_attempts().await,
        failed + 1,
        "record the failed attempt, not an approval"
    );
    assert_eq!(embedding.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fx.semantic_state().await, before);
    fx.assert_pending(&proposal).await;
    fx.worker().recover_interrupted(now()).await.unwrap();
    assert!(matches!(
        fx.worker().step(now(), &inference).await.unwrap(),
        Dispatch::Idle
    ));
    assert_eq!(fx.semantic_state().await, before);
    let working = Embedding::default();
    fx.service()
        .with_embedding(&working)
        .approve(&fx.owner, &proposal, &OperationId::new("explicit-retry"))
        .await
        .expect("a different explicit approval can restart preparation");
    fx.assert_content(AFTER).await;
    assert_eq!(fx.failed_attempts().await, failed + 1);
    assert_eq!(inference.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn duplicate_approval_submissions_commit_one_journal_and_archive() {
    let fx = Fixture::new(false).await;
    let proposal = fx.human(&Inference::default()).await;
    let embedding = Embedding::default();
    let service = fx.service().with_embedding(&embedding);
    let same_click = OperationId::new("same-browser-submission");
    let (first, second) = tokio::join!(
        service.approve(&fx.owner, &proposal, &same_click),
        service.approve(&fx.owner, &proposal, &same_click),
    );
    let first = first.expect("concurrent duplicate submissions must share the committed result");
    let second = second.expect("same-key submission must resolve as a replay");
    assert_eq!(first.operation, second.operation);
    assert_ne!(first.replayed, second.replayed);
    assert_eq!(first.created.len() + second.created.len(), 1);
    fx.assert_content(AFTER).await;
    let journal = MutationRepo::new(fx.db.inner())
        .operation(&fx.owner, same_click.as_str())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.id, first.operation);
    assert!(
        service
            .approve(&fx.owner, &proposal, &OperationId::new("different-click"))
            .await
            .is_err()
    );
    let all: Vec<Value> = fx
        .db
        .inner()
        .query("SELECT id FROM semantic_operation WHERE kind = 'quality';")
        .await
        .unwrap()
        .check()
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(all.len(), 1);
}
