//! Witnesses for the fair refinement worker and the route provider reused by its supervisor.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use alexandria_engine::refinement::policy::{InferencePolicyInputs, Protocol, Route, RoutePair};
use alexandria_mcp::refinement::inference::{FinishReason, InferenceError, InferenceReply};
use alexandria_mcp::refinement::worker::{
    Call, Dispatch, Stage, Worker, WorkerOutput, WorkerProvider, WorkerQuota, WorkerSettings,
};
use alexandria_mcp::refinement::{ReconciledRefinement, RefinementService};
use alexandria_storage::access::{AccessRepo, OwnerContext, SystemContext};
use alexandria_storage::models::OperationId;
use alexandria_storage::models::identity::{
    FactId, MemoryScope, Partition, ProjectId, RefinementProposalId, Version,
};
use alexandria_storage::models::refinement::{
    ApprovalMode, Authority, ConsentChoice, EvidenceBundle, ProposalCategory, ProposalRevision,
    ProposalState, QualityOperation, SecondaryVerdict,
};
use alexandria_storage::repos::refinement_repo::{
    AttemptId, AttemptState, BudgetDenial, BudgetRepo, CallLimits, EnqueueRequest, EvidenceOutcome,
    EvidenceSelection, PassId, Reservation, WorkKind, WorkSelection, current_authority,
};
use alexandria_storage::repos::{IdentityRepo, RefinementRepo};
use alexandria_storage::{Database, schema};
use chrono::{DateTime, TimeDelta, Utc};
use surrealdb::types::{RecordId, ToSql};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn now() -> DateTime<Utc> {
    // Epoch-relative and deliberately far from any moving wall-clock test boundary.
    DateTime::<Utc>::UNIX_EPOCH + TimeDelta::days(90)
}

struct Fixture {
    db: Database,
    system: SystemContext,
    owner: OwnerContext,
    other: OwnerContext,
    config: ReconciledRefinement,
}

impl Fixture {
    async fn new() -> Self {
        let mut db = Database::connect_embedded().await.unwrap();
        let system = db.claim_system_authority().unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let owner = enroll(&db, &system, "worker-owner").await;
        let other = enroll(&db, &system, "worker-other").await;
        let config = configure(&db, &system, "first-model").await;
        let service = RefinementService::new(&db, Some(&config));
        for owner in [&owner, &other] {
            let policy = service.effective_policy(owner).await.unwrap().policy;
            RefinementRepo::new(&db)
                .set_consent(
                    owner,
                    0,
                    ConsentChoice::Grant {
                        disclosed_policy_revision: policy.configured.unwrap().revision,
                    },
                    now() - TimeDelta::days(61),
                )
                .await
                .unwrap();
        }
        Self {
            db,
            system,
            owner,
            other,
            config,
        }
    }

    fn service(&self) -> RefinementService<'_> {
        RefinementService::new(&self.db, Some(&self.config))
    }

    fn worker(&self, settings: WorkerSettings) -> Worker<'_> {
        Worker::new(&self.db, &self.system, &self.config, settings)
    }

    async fn automatic(&self) {
        let service = self.service();
        let policy = service.effective_policy(&self.owner).await.unwrap().policy;
        service
            .set_approval_mode(&self.owner, &policy.version(), ApprovalMode::SecondaryLlm)
            .await
            .unwrap();
    }

    async fn seed(
        &self,
        owner: &OwnerContext,
        scope: MemoryScope,
        key: &str,
        axis: usize,
    ) -> FactId {
        let record = RecordId::new("fact", key);
        let (scope, project) = match scope {
            MemoryScope::Personal => ("personal", None),
            MemoryScope::Project(project) => ("project", Some(project.as_record_id().clone())),
            MemoryScope::Unassigned => ("unassigned", None),
        };
        let mut embedding = vec![0.0_f32; 8];
        if let Some(component) = embedding.get_mut(axis) {
            *component = 1.0;
        }
        self.db.inner()
            .query("CREATE $id SET account = $account, workspace = $workspace, scope = $scope, project = $project, content = $text, embedding = $vector, created_at = $created, tags = ['refinement-test'], semantic_revision = 1 RETURN NONE")
            .bind(("id", record.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .bind(("scope", scope))
            .bind(("project", project))
            .bind(("text", format!("Task 18 must project this complete content: {key}.")))
            .bind(("vector", embedding))
            .bind(("created", now() - TimeDelta::days(60)))
            .await.unwrap().check().unwrap();
        FactId::try_from(record).unwrap()
    }

    async fn project(&self, owner: &OwnerContext, key: &str) -> MemoryScope {
        let record = RecordId::new("project", key);
        self.db
            .inner()
            .query(
                "CREATE $project SET workspace = $workspace, normalized_alias = $alias RETURN NONE",
            )
            .bind(("project", record.clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .bind(("alias", key.to_owned()))
            .await
            .unwrap()
            .check()
            .unwrap();
        MemoryScope::Project(ProjectId::try_from(record).unwrap())
    }

    async fn enqueue(&self, owner: &OwnerContext, scope: MemoryScope, key: &str) -> AttemptId {
        self.service()
            .enqueue(owner, scheduled(owner, scope, key))
            .await
            .unwrap()
    }

    async fn record_expired_assessment(&self, evidence: EvidenceBundle, key: &str) -> RecordId {
        let policy = self
            .service()
            .effective_policy(&self.owner)
            .await
            .unwrap()
            .policy;
        let version = policy.version();
        let proposal = ProposalRevision {
            id: RecordId::new("refinement_proposal", key),
            revision: Version::new(1),
            partition: evidence.focal.partition.clone(),
            sources: vec![],
            category: ProposalCategory::Rewrite,
            proposed: QualityOperation::Rewrite {
                source: evidence.focal.id.clone(),
                content: "Earlier assessment.".into(),
            },
            approved_relationship_effects: vec![],
            rationale: "Previously reviewed.".into(),
            uncertainty: String::new(),
            origin_policy: Authority {
                account: self.owner.account().clone(),
                consent_revision: version.consent_revision,
                owner_mode_revision: version.owner_mode_revision,
                configured_revision: version.configured_revision.unwrap(),
                inference_policy_id: evidence.inference_policy.clone(),
                mode: ApprovalMode::Human,
            },
            evidence,
            state: ProposalState::Pending,
            needs_human: true,
            deferred: false,
        };
        let old = RefinementRepo::new(&self.db)
            .persist_proposal(&self.owner, proposal)
            .await
            .unwrap();
        self.db
            .inner()
            .query("UPDATE ONLY $id SET created_at = $created RETURN NONE")
            .bind(("id", old.id.clone()))
            .bind(("created", now() - TimeDelta::days(45)))
            .await
            .unwrap()
            .check()
            .unwrap();
        old.id
    }
}

async fn configure(db: &Database, system: &SystemContext, model: &str) -> ReconciledRefinement {
    let routes = RoutePair::resolve(
        Route {
            protocol: Protocol::OpenAiChatCompletions,
            api_prefix: "https://inference.example/v1".into(),
            model: model.into(),
            credential_ref: None,
            headers: BTreeMap::new(),
            output_limit_field: None,
        },
        None,
    )
    .unwrap();
    ReconciledRefinement::reconcile(
        db,
        system,
        Some(routes),
        &InferencePolicyInputs {
            prompt_template: b"refinement worker fixture",
            prompt_template_version: "1",
            operation_schema_version: "1",
            settings: &BTreeMap::new(),
        },
        ApprovalMode::SecondaryLlm,
    )
    .await
    .unwrap()
    .unwrap()
}

async fn enroll(db: &Database, system: &SystemContext, login: &str) -> OwnerContext {
    let seeded = IdentityRepo::new(db.inner())
        .bootstrap_owner(
            login,
            &format!("$argon2id$fixture-prepared-hash-{login}-padding"),
            &OperationId::new(login),
        )
        .await
        .unwrap();
    let access = AccessRepo::new(db);
    let principal = access
        .local_principal(system, &seeded.account)
        .await
        .unwrap();
    access.authorize(&principal).await.unwrap()
}

fn partition(owner: &OwnerContext, scope: MemoryScope) -> Partition {
    Partition {
        workspace: owner.workspace().clone(),
        scope,
    }
}

fn scheduled(owner: &OwnerContext, scope: MemoryScope, key: &str) -> EnqueueRequest {
    EnqueueRequest {
        owner: owner.account().clone(),
        partition: partition(owner, scope),
        kind: WorkKind::Assess,
        source_revision: None,
        selection: WorkSelection::Scheduled,
        idempotency_key: key.into(),
    }
}

fn unassigned(owner: &OwnerContext, focal: &FactId, key: &str) -> EnqueueRequest {
    EnqueueRequest {
        owner: owner.account().clone(),
        partition: partition(owner, MemoryScope::Unassigned),
        kind: WorkKind::AssessUnassigned,
        source_revision: Some(Version::new(1)),
        selection: WorkSelection::Evidence(EvidenceSelection {
            focal: focal.clone(),
            related: Vec::new(),
        }),
        idempotency_key: key.into(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Sent {
    stage: Stage,
    partition: Partition,
    focal: FactId,
    text: String,
}

#[derive(Clone, Copy)]
enum SecondaryChange {
    SwitchToHuman,
    RegrantAfterReservation,
}

#[derive(Default)]
struct FakeProvider<'a> {
    sent: Mutex<Vec<Sent>>,
    entered: Notify,
    release: Notify,
    hold: bool,
    primary_attempts: usize,
    after_reservation: Option<(&'a Database, &'a OwnerContext)>,
    secondary_change: Option<(&'a Fixture, SecondaryChange)>,
    reservations: AtomicUsize,
}

#[async_trait::async_trait]
impl WorkerProvider for FakeProvider<'_> {
    async fn complete(
        &self,
        call: Call<'_>,
        quota: &dyn WorkerQuota,
        cancel: CancellationToken,
    ) -> Result<InferenceReply, InferenceError> {
        let attempts = if call.stage == Stage::Primary {
            self.primary_attempts.max(1)
        } else {
            1
        };
        let change = (call.stage == Stage::Secondary)
            .then_some(self.secondary_change)
            .flatten();
        if let Some((fx, SecondaryChange::SwitchToHuman)) = change {
            let service = fx.service();
            let policy = service.effective_policy(&fx.owner).await.unwrap().policy;
            service
                .set_approval_mode(&fx.owner, &policy.version(), ApprovalMode::Human)
                .await
                .unwrap();
        }
        for _ in 0..attempts {
            quota
                .reserve_attempt()
                .await
                .map_err(|_| InferenceError::QuotaDenied)?;
            self.reservations.fetch_add(1, Ordering::SeqCst);
            if let Some((db, owner)) = self.after_reservation {
                set_status(db, owner, "suspended").await;
            }
            if let Some((fx, SecondaryChange::RegrantAfterReservation)) = change {
                let service = fx.service();
                let policy = service.effective_policy(&fx.owner).await.unwrap().policy;
                let revoked = service
                    .consent(
                        &fx.owner,
                        policy.version().consent_revision,
                        ConsentChoice::Revoke,
                    )
                    .await
                    .unwrap();
                service
                    .consent(
                        &fx.owner,
                        revoked.revision,
                        ConsentChoice::Grant {
                            disclosed_policy_revision: policy.configured.unwrap().revision,
                        },
                    )
                    .await
                    .unwrap();
            }
            let authorized = quota.authorize_outbound().await;
            if matches!(change, Some((_, SecondaryChange::RegrantAfterReservation))) {
                assert!(
                    authorized.is_err(),
                    "regrant must not restore a dispatched grant"
                );
            }
            authorized.map_err(|_| InferenceError::QuotaDenied)?;
            if cancel.is_cancelled() {
                return Err(InferenceError::Cancelled);
            }
            self.sent.lock().unwrap().push(Sent {
                stage: call.stage,
                partition: call.evidence.focal.partition.clone(),
                focal: call.evidence.focal.id.clone(),
                text: call.evidence.focal.complete_content.clone(),
            });
            self.entered.notify_one();
            if self.hold {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(InferenceError::Cancelled),
                    _ = self.release.notified() => {},
                }
            }
        }
        let text = match call.stage {
            Stage::Primary => serde_json::json!({
                "category": "rewrite",
                "operation": {"kind": "rewrite", "source": call.evidence.focal.id.as_record_id().to_sql(), "content": "Clarified text."},
                "rationale": "Proposes a grounded rewrite.",
                "uncertainty": ""
            }).to_string(),
            Stage::Secondary => {
                assert!(call.proposal.is_some(), "review must cite the exact proposal revision");
                r#"{"verdict":"approve_exact","rationale":"No facts added."}"#.into()
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

impl FakeProvider<'_> {
    fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }
}

async fn set_status(db: &Database, owner: &OwnerContext, status: &str) {
    db.inner()
        .query("UPDATE ONLY $account SET status = $status RETURN NONE")
        .bind(("account", owner.account().as_record_id().clone()))
        .bind(("status", status.to_owned()))
        .await
        .unwrap()
        .check()
        .unwrap();
}

#[tokio::test]
async fn empty_queue_never_invokes_the_provider() {
    let fx = Fixture::new().await;
    let provider = FakeProvider::default();
    let worker = fx.worker(WorkerSettings::default());
    assert!(matches!(
        worker.step(now(), &provider).await.unwrap(),
        Dispatch::Idle
    ));
    assert!(provider.sent().is_empty());
    assert_eq!(provider.reservations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn one_in_flight_and_real_task18_projection_produces_typed_proposal() {
    let fx = Fixture::new().await;
    let focal = fx
        .seed(&fx.owner, MemoryScope::Personal, "one-focal", 0)
        .await;
    fx.seed(&fx.other, MemoryScope::Personal, "other-focal", 0)
        .await;
    let attempt = fx
        .enqueue(&fx.owner, MemoryScope::Personal, "one-first")
        .await;
    let worker = fx.worker(WorkerSettings::default());
    let provider = FakeProvider {
        hold: true,
        ..Default::default()
    };
    let first = worker.step(now(), &provider);
    let concurrent = async {
        provider.entered.notified().await;
        fx.enqueue(&fx.other, MemoryScope::Personal, "one-second")
            .await;
        let result = worker.step(now(), &provider).await.unwrap();
        assert!(
            matches!(result, Dispatch::Busy),
            "second step did not refuse concurrent inference: {result:?}"
        );
        assert_eq!(provider.sent().len(), 1);
        provider.release.notify_one();
    };
    let (first, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(first, concurrent)
    })
    .await
    .expect("first call and concurrent refusal must not hang");
    let Dispatch::Completed {
        output: WorkerOutput::Proposal(proposal),
        ..
    } = first.unwrap()
    else {
        panic!("first primary did not produce a typed proposal");
    };
    assert_eq!(proposal.evidence.focal.id, focal);
    assert_eq!(
        provider.sent()[0].text,
        "Task 18 must project this complete content: one-focal."
    );
    assert_eq!(
        fx.service()
            .attempt(&fx.owner, &attempt)
            .await
            .unwrap()
            .state,
        AttemptState::Completed
    );
    assert!(
        fx.service().attempt(&fx.other, &attempt).await.is_err(),
        "attempt views are owner-scoped"
    );

    let service = fx.service();
    let repo = RefinementRepo::new(&fx.db);
    let persisted = repo.persist_proposal(&fx.owner, proposal).await.unwrap();
    let visible = service
        .owner_proposals(&fx.owner, None, false)
        .await
        .unwrap();
    assert_eq!(visible.len(), 1, "new proposal is in the regular queue");
    assert_eq!(visible[0].id, persisted.id);
    let id = RefinementProposalId::try_from(persisted.id).unwrap();
    repo.defer_proposal_revision(&fx.owner, &id, persisted.revision)
        .await
        .unwrap();
    assert!(
        service
            .owner_proposals(&fx.owner, None, false)
            .await
            .unwrap()
            .is_empty()
    );
    let deferred = service
        .owner_proposals(&fx.owner, Some("personal"), true)
        .await
        .unwrap();
    assert_eq!(deferred.len(), 1);
    assert_eq!(&deferred[0].id, id.as_record_id());
    assert!(
        service
            .owner_proposals(&fx.other, None, true)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn account_partition_and_interactive_class_get_turns_under_backlog() {
    let fx = Fixture::new().await;
    let project = fx.project(&fx.owner, "worker-project").await;
    for i in 0..6 {
        fx.seed(
            &fx.owner,
            MemoryScope::Personal,
            &format!("fair-personal-{i}"),
            i,
        )
        .await;
    }
    fx.seed(&fx.owner, project.clone(), "fair-project", 6).await;
    fx.seed(&fx.other, MemoryScope::Personal, "fair-other", 6)
        .await;
    let review = fx
        .seed(&fx.owner, MemoryScope::Unassigned, "fair-review", 7)
        .await;
    fx.enqueue(&fx.owner, MemoryScope::Personal, "fair-personal-pass")
        .await;
    fx.enqueue(&fx.owner, project.clone(), "fair-project").await;
    fx.enqueue(&fx.other, MemoryScope::Personal, "fair-other")
        .await;
    fx.service()
        .enqueue(&fx.owner, unassigned(&fx.owner, &review, "fair-review"))
        .await
        .unwrap();
    let worker = fx.worker(WorkerSettings {
        max_focals_per_pass: 1,
        ..Default::default()
    });
    let provider = FakeProvider::default();
    for i in 0..4 {
        worker.step(now(), &provider).await.unwrap();
        if provider
            .sent()
            .last()
            .is_some_and(|sent| sent.partition == partition(&fx.owner, MemoryScope::Personal))
        {
            fx.enqueue(
                &fx.owner,
                MemoryScope::Personal,
                &format!("fair-personal-pass-{i}"),
            )
            .await;
        }
    }
    let sends = provider.sent();
    assert_eq!(sends.len(), 4);
    let seen: Vec<_> = sends.into_iter().map(|sent| sent.partition).collect();
    for expected in [
        partition(&fx.owner, MemoryScope::Personal),
        partition(&fx.owner, project),
        partition(&fx.other, MemoryScope::Personal),
        partition(&fx.owner, MemoryScope::Unassigned),
    ] {
        assert!(
            seen.contains(&expected),
            "turn lost to the Personal backlog: {expected:?}"
        );
    }
}

#[tokio::test]
async fn run107b_rotation_survives_restart_with_two_older_requeued_partitions() {
    let fx = Fixture::new().await;
    let personal = MemoryScope::Personal;
    let project = fx.project(&fx.owner, "rotation-project").await;
    fx.seed(&fx.owner, personal.clone(), "rotation-a1", 8).await;
    fx.seed(&fx.owner, project.clone(), "rotation-a2", 8).await;
    fx.seed(&fx.other, personal.clone(), "rotation-b", 8).await;
    let a1 = fx.enqueue(&fx.owner, personal.clone(), "rotation-a1").await;
    let a2 = fx.enqueue(&fx.owner, project, "rotation-a2").await;
    let provider = FakeProvider::default();
    assert!(matches!(
        fx.worker(WorkerSettings::default())
            .step(now(), &provider)
            .await
            .unwrap(),
        Dispatch::Completed { .. }
    ));
    let b = fx.enqueue(&fx.other, personal, "rotation-b").await;
    // Model two interrupted requeues at the unchanged head of the FIFO.
    for (id, age) in [(&a1, 3), (&a2, 2), (&b, 1)] {
        fx.db
            .inner()
            .query("UPDATE ONLY $id SET state = 'queued', created_at = $created RETURN NONE")
            .bind(("id", id.record().clone()))
            .bind(("created", now() - TimeDelta::hours(age)))
            .await
            .unwrap()
            .check()
            .unwrap();
    }
    let dispatched = fx
        .worker(WorkerSettings::default())
        .step(now(), &provider)
        .await
        .unwrap();
    assert!(
        matches!(&dispatched, Dispatch::Completed { attempt, .. } if attempt == &b),
        "the other eligible account must go first after reconstruction: {dispatched:?}"
    );
}

#[tokio::test]
async fn run107b_route_provider_survives_elapsed_pass_deadline() {
    use alexandria_mcp::refinement::inference::{AttemptQuota, QuotaDenied};
    use alexandria_mcp::refinement::provider::RouteProvider;

    struct Admit;
    #[async_trait::async_trait]
    impl AttemptQuota for Admit {
        async fn reserve_attempt(&self) -> Result<(), QuotaDenied> {
            Ok(())
        }
    }
    #[async_trait::async_trait]
    impl WorkerQuota for Admit {
        async fn authorize_outbound(&self) -> Result<(), QuotaDenied> {
            Ok(())
        }
    }

    let fx = Fixture::new().await;
    fx.seed(&fx.owner, MemoryScope::Personal, "deadline-focal", 8)
        .await;
    let mut page = RefinementRepo::new(&fx.db)
        .page_candidates(&partition(&fx.owner, MemoryScope::Personal), None, 1, now())
        .await
        .unwrap();
    let EvidenceOutcome::Ready(evidence) = page.outcomes.pop().unwrap() else {
        panic!("fixture must produce provider evidence");
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prefix = format!("http://{}/v1", listener.local_addr().unwrap());
    let host = tokio::spawn(async move {
        let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(|| async {
            axum::Json(serde_json::json!({
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "ready"}, "finish_reason": "stop"}]
            }))
        }));
        axum::serve(listener, app).await.unwrap();
    });
    let routes = RoutePair::resolve(
        Route {
            protocol: Protocol::OpenAiChatCompletions,
            api_prefix: prefix,
            model: "fixture".into(),
            credential_ref: None,
            headers: BTreeMap::new(),
            output_limit_field: None,
        },
        None,
    )
    .unwrap();
    let config = ReconciledRefinement::reconcile(
        &fx.db,
        &fx.system,
        Some(routes),
        &InferencePolicyInputs {
            prompt_template: b"deadline fixture",
            prompt_template_version: "1",
            operation_schema_version: "1",
            settings: &BTreeMap::new(),
        },
        ApprovalMode::Human,
    )
    .await
    .unwrap()
    .unwrap();
    let settings = WorkerSettings::default();
    let provider = RouteProvider::new(&config, settings);
    let complete = || {
        provider.complete(
            Call {
                kind: WorkKind::Assess,
                stage: Stage::Primary,
                evidence: &evidence,
                proposal: None,
            },
            &Admit,
            CancellationToken::new(),
        )
    };
    assert_eq!(complete().await.unwrap().text, "ready");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(settings.pass_active_secs + 1)).await;
    tokio::time::resume();
    let second = complete().await;
    host.abort();
    assert_eq!(
        second.expect("independent calls get a fresh deadline").text,
        "ready"
    );
}

#[tokio::test]
async fn new_old_cursor_carries_across_partial_passes_and_worker_reconstruction() {
    let fx = Fixture::new().await;
    let scope = MemoryScope::Personal;
    let partition = partition(&fx.owner, scope.clone());
    // Zero vectors make the evidence independent: adding a new focal cannot
    // invalidate the older fixtures just by changing their related set.
    let old_a = fx.seed(&fx.owner, scope.clone(), "old-a", 8).await;
    let old_b = fx.seed(&fx.owner, scope.clone(), "old-b", 8).await;
    let page = RefinementRepo::new(&fx.db)
        .page_candidates(&partition, None, 10, now())
        .await
        .unwrap();
    assert_eq!(
        page.outcomes.len(),
        2,
        "Task 18 fixture must yield both old projections"
    );
    for (i, outcome) in page.outcomes.into_iter().enumerate() {
        let EvidenceOutcome::Ready(bundle) = outcome else {
            panic!("old fixture isn't projectable")
        };
        fx.record_expired_assessment(bundle, &format!("prior-{i}"))
            .await;
    }
    let fresh = fx.seed(&fx.owner, scope.clone(), "new-z", 2).await;
    let provider = FakeProvider::default();
    for i in 0..4 {
        fx.enqueue(&fx.owner, scope.clone(), &format!("partial-{i}"))
            .await;
        let settings = WorkerSettings {
            max_focals_per_pass: 1,
            ..Default::default()
        };
        fx.worker(settings)
            .step(now() + TimeDelta::seconds(i), &provider)
            .await
            .unwrap();
        fx.seed(
            &fx.owner,
            scope.clone(),
            &format!("new-{i}"),
            3 + usize::try_from(i).unwrap(),
        )
        .await;
    }
    let sent: Vec<_> = provider.sent().into_iter().map(|sent| sent.focal).collect();
    assert_eq!(
        sent.first(),
        Some(&fresh),
        "never-assessed work gets the first turn"
    );
    let old: Vec<_> = sent
        .into_iter()
        .filter(|id| id == &old_a || id == &old_b)
        .collect();
    assert_eq!(
        old,
        vec![old_a, old_b],
        "new lower-key arrivals cannot reset the old cursor"
    );
}

#[tokio::test]
async fn retries_and_secondary_charge_the_origin_pass_account_and_global() {
    let fx = Fixture::new().await;
    fx.automatic().await;
    fx.seed(&fx.owner, MemoryScope::Personal, "charged-focal", 0)
        .await;
    fx.enqueue(&fx.owner, MemoryScope::Personal, "charged")
        .await;
    let settings = WorkerSettings::default();
    let provider = FakeProvider {
        primary_attempts: 3,
        ..Default::default()
    };
    let done = fx.worker(settings).step(now(), &provider).await.unwrap();
    let Dispatch::Completed {
        pass,
        output: WorkerOutput::Automatic(_),
        ..
    } = done
    else {
        panic!("exact reviewer approval must produce a handoff candidate, not apply it");
    };
    let charges = BudgetRepo::new(&fx.db, settings.calls)
        .usage(fx.owner.account(), &pass, now())
        .await
        .unwrap();
    assert_eq!(
        (
            charges.pass,
            charges.account,
            charges.global,
            charges.interactive
        ),
        (4, 4, 4, 0)
    );
    assert_eq!(
        provider
            .sent()
            .iter()
            .map(|sent| sent.stage)
            .collect::<Vec<_>>(),
        [
            Stage::Primary,
            Stage::Primary,
            Stage::Primary,
            Stage::Secondary
        ]
    );
}

#[tokio::test]
async fn budget_denial_prevents_http_and_does_not_reset_on_a_new_request() {
    let fx = Fixture::new().await;
    let settings = WorkerSettings {
        calls: CallLimits {
            pass: 1,
            account: 1,
            interactive: 1,
            global: 10,
        },
        ..Default::default()
    };
    let repo = BudgetRepo::new(&fx.db, settings.calls);
    let authority = current_authority(&fx.db, fx.owner.account())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        repo.reserve_attempt(&authority, &PassId("prior-call".into()), false, now())
            .await
            .unwrap(),
        Reservation::Granted
    );
    fx.seed(&fx.owner, MemoryScope::Personal, "denied-focal", 0)
        .await;
    fx.enqueue(&fx.owner, MemoryScope::Personal, "denied-new-click")
        .await;
    let provider = FakeProvider::default();
    let done = fx.worker(settings).step(now(), &provider).await.unwrap();
    assert!(
        matches!(done, Dispatch::Denied),
        "expected budget refusal, got {done:?}"
    );
    assert!(
        provider.sent().is_empty(),
        "denied work tried to send bytes"
    );
}

#[tokio::test]
async fn call_caps_are_independent_atomic_and_rolling() {
    let fx = Fixture::new().await;
    let a = current_authority(&fx.db, fx.owner.account())
        .await
        .unwrap()
        .unwrap();
    let b = current_authority(&fx.db, fx.other.account())
        .await
        .unwrap()
        .unwrap();
    let repo = BudgetRepo::new(
        &fx.db,
        CallLimits {
            pass: 2,
            account: 3,
            interactive: 1,
            global: 4,
        },
    );
    let first = PassId("first".into());
    let second = PassId("second".into());
    let other = PassId("other".into());
    for (pass, interactive, expected) in [
        (&first, true, Reservation::Granted),
        (
            &second,
            true,
            Reservation::Denied(BudgetDenial::Interactive),
        ),
        (&first, false, Reservation::Granted),
        (&first, false, Reservation::Denied(BudgetDenial::Pass)),
        (&second, false, Reservation::Granted),
        (&second, false, Reservation::Denied(BudgetDenial::Account)),
    ] {
        assert_eq!(
            repo.reserve_attempt(&a, pass, interactive, now())
                .await
                .unwrap(),
            expected
        );
    }
    let latest = now() + TimeDelta::hours(23);
    let left = repo.reserve_attempt(&b, &other, false, latest);
    let right = repo.reserve_attempt(&b, &other, false, latest);
    let (left, right) = tokio::join!(left, right);
    let results = [left.unwrap(), right.unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == Reservation::Granted)
            .count(),
        1
    );
    assert!(results.contains(&Reservation::Denied(BudgetDenial::Global)));
    assert_eq!(
        repo.reserve_attempt(
            &a,
            &PassId("next-day".into()),
            true,
            now() + TimeDelta::hours(24) + TimeDelta::seconds(1)
        )
        .await
        .unwrap(),
        Reservation::Granted
    );
    let usage = repo
        .usage(fx.other.account(), &other, latest + TimeDelta::hours(2))
        .await
        .unwrap();
    assert_eq!(
        (usage.account, usage.global),
        (1, 2),
        "later traffic survives earlier charge expiry"
    );
}

#[tokio::test]
async fn reservation_rejects_stale_policy_revoked_consent_and_suspended_accounts() {
    let fx = Fixture::new().await;
    let service = fx.service();
    let old = service.effective_policy(&fx.owner).await.unwrap().policy;
    let a = current_authority(&fx.db, fx.owner.account())
        .await
        .unwrap()
        .unwrap();
    let b = current_authority(&fx.db, fx.other.account())
        .await
        .unwrap()
        .unwrap();
    let pass = PassId("authority-test".into());
    let repo = BudgetRepo::new(&fx.db, WorkerSettings::default().calls);
    assert_eq!(
        repo.reserve_attempt(&a, &pass, false, now()).await.unwrap(),
        Reservation::Granted
    );
    service
        .consent(
            &fx.owner,
            old.version().consent_revision,
            ConsentChoice::Revoke,
        )
        .await
        .unwrap();
    assert!(repo.reserve_attempt(&a, &pass, false, now()).await.is_err());
    set_status(&fx.db, &fx.other, "suspended").await;
    assert!(repo.reserve_attempt(&b, &pass, false, now()).await.is_err());
    set_status(&fx.db, &fx.other, "active").await;
    configure(&fx.db, &fx.system, "same-destination-new-policy").await;
    assert!(repo.reserve_attempt(&b, &pass, false, now()).await.is_err());
    let fresh = current_authority(&fx.db, fx.other.account())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        repo.reserve_attempt(&fresh, &pass, false, now())
            .await
            .unwrap(),
        Reservation::Granted
    );
    let usage = repo.usage(fx.other.account(), &pass, now()).await.unwrap();
    assert_eq!(
        (usage.account, usage.global),
        (1, 2),
        "rejected authority consumed no call budget"
    );
}

#[tokio::test]
async fn returning_to_a_previous_destination_requires_renewed_consent() {
    let fx = Fixture::new().await;
    let repo = RefinementRepo::new(&fx.db);
    let initial = repo.effective_policy(&fx.owner).await.unwrap();
    assert!(initial.inference_admitted);
    let route_a = initial.configured.unwrap().configuration;
    let mut route_b = route_a.clone();
    let mut destination = route_a.destination_set.iter().next().unwrap().clone();
    destination.host = "other-inference.example".into();
    route_b.destination_set = [destination].into_iter().collect();

    repo.reconcile_policy(&fx.system, None).await.unwrap();
    assert!(
        !repo
            .effective_policy(&fx.owner)
            .await
            .unwrap()
            .inference_admitted
    );
    repo.reconcile_policy(&fx.system, Some(&route_b))
        .await
        .unwrap();
    assert!(
        !repo
            .effective_policy(&fx.owner)
            .await
            .unwrap()
            .inference_admitted
    );
    let restored = repo
        .reconcile_policy(&fx.system, Some(&route_a))
        .await
        .unwrap()
        .unwrap();
    let stale = repo.effective_policy(&fx.owner).await.unwrap();
    assert!(!stale.inference_admitted);
    let historical = stale.consent.unwrap();
    assert_eq!(historical.destination_set, route_a.destination_set);
    assert!(historical.revoked_at.is_none());

    let request = scheduled(&fx.owner, MemoryScope::Personal, "renewed-destination");
    let refused = repo.enqueue(&fx.owner, request.clone()).await;
    assert!(
        matches!(
            refused,
            Err(alexandria_storage::error::StorageError::Unavailable)
        ),
        "queue admission must agree with the effective policy: {refused:?}"
    );

    repo.set_consent(
        &fx.owner,
        historical.revision,
        ConsentChoice::Grant {
            disclosed_policy_revision: restored.revision,
        },
        now(),
    )
    .await
    .unwrap();
    repo.enqueue(&fx.owner, request).await.unwrap();
}

#[tokio::test]
async fn enqueue_refuses_policy_changed_after_browser_check() {
    let fx = Fixture::new().await;
    let service = fx.service();
    let displayed = service.effective_policy(&fx.owner).await.unwrap().policy;
    assert!(displayed.inference_admitted);
    let expected = displayed.version();
    let request = scheduled(&fx.owner, MemoryScope::Personal, "stale-browser-form");

    // The browser has already verified the displayed version. Change only the mode before
    // enqueue: inference remains admitted, so checking admission alone would accept the form.
    fx.automatic().await;
    let current = service.effective_policy(&fx.owner).await.unwrap().policy;
    assert!(current.inference_admitted);
    assert_ne!(current.version(), expected);
    assert!(matches!(
        service
            .enqueue_checked(&fx.owner, request.clone(), expected)
            .await,
        Err(alexandria_mcp::error::ServiceError::Stale)
    ));
    assert_eq!(service.owner_usage(&fx.owner).await.unwrap().queued, 0);
    service
        .enqueue_checked(&fx.owner, request, current.version())
        .await
        .unwrap();
    assert_eq!(service.owner_usage(&fx.owner).await.unwrap().queued, 1);
}

#[tokio::test]
async fn queue_coalesces_idempotency_and_enforces_interactive_and_global_caps() {
    let fx = Fixture::new().await;
    let focal = fx
        .seed(&fx.owner, MemoryScope::Unassigned, "queue-review", 0)
        .await;
    let service = fx.service();
    let request = unassigned(&fx.owner, &focal, "same-click");
    let (a, b) = tokio::join!(
        service.enqueue(&fx.owner, request.clone()),
        service.enqueue(&fx.owner, request)
    );
    let id = a.unwrap();
    assert_eq!(
        id,
        b.unwrap(),
        "same owner request consumed two queue entries"
    );
    for i in 1..20 {
        let key = format!("queue-click-{i}");
        let selected = fx.seed(&fx.owner, MemoryScope::Unassigned, &key, 0).await;
        service
            .enqueue(&fx.owner, unassigned(&fx.owner, &selected, &key))
            .await
            .unwrap();
    }
    let overflow = fx
        .seed(&fx.owner, MemoryScope::Unassigned, "queue-overflow", 0)
        .await;
    assert!(
        service
            .enqueue(
                &fx.owner,
                unassigned(&fx.owner, &overflow, "interactive-overflow")
            )
            .await
            .is_err()
    );
    assert_eq!(
        service
            .enqueue(&fx.owner, unassigned(&fx.owner, &focal, "same-click"))
            .await
            .unwrap(),
        id
    );
    for i in 20..200 {
        let key = format!("global-{i}");
        fx.enqueue(&fx.other, fx.project(&fx.other, &key).await, &key)
            .await;
    }
    assert!(
        service
            .enqueue(
                &fx.other,
                scheduled(&fx.other, MemoryScope::Personal, "global-overflow")
            )
            .await
            .is_err()
    );
    assert_eq!(
        service.attempt(&fx.owner, &id).await.unwrap().state,
        AttemptState::Queued
    );
}

#[tokio::test]
async fn opinion_coalesces_by_proposal_revision_and_charges_interactive_allowance() {
    let fx = Fixture::new().await;
    let partition = partition(&fx.owner, MemoryScope::Personal);
    fx.seed(&fx.owner, MemoryScope::Personal, "opinion-focal", 0)
        .await;
    let mut page = RefinementRepo::new(&fx.db)
        .page_candidates(&partition, None, 1, now())
        .await
        .unwrap();
    let EvidenceOutcome::Ready(evidence) = page.outcomes.pop().expect("real evidence fixture")
    else {
        panic!("projection unexpectedly needs context");
    };
    let proposal = fx
        .record_expired_assessment(evidence, "opinion-source")
        .await;
    let request = |key: &str| EnqueueRequest {
        owner: fx.owner.account().clone(),
        partition: partition.clone(),
        kind: WorkKind::Opinion,
        source_revision: Some(Version::new(1)),
        selection: WorkSelection::Proposal(proposal.clone()),
        idempotency_key: key.into(),
    };
    let service = fx.service();
    let first = service
        .enqueue(&fx.owner, request("first-opinion-click"))
        .await
        .unwrap();
    assert_eq!(
        service
            .enqueue(&fx.owner, request("another-opinion-click"))
            .await
            .unwrap(),
        first
    );
    let settings = WorkerSettings::default();
    let provider = FakeProvider::default();
    let result = fx.worker(settings).step(now(), &provider).await.unwrap();
    let Dispatch::Completed {
        pass,
        output: WorkerOutput::Opinion(SecondaryVerdict::ApproveExact),
        ..
    } = result
    else {
        panic!("owner opinion must return a typed opinion, never automatic authority");
    };
    assert_eq!(
        provider
            .sent()
            .iter()
            .map(|sent| sent.stage)
            .collect::<Vec<_>>(),
        [Stage::Secondary]
    );
    let usage = BudgetRepo::new(&fx.db, settings.calls)
        .usage(fx.owner.account(), &pass, now())
        .await
        .unwrap();
    assert_eq!(
        (usage.pass, usage.account, usage.global, usage.interactive),
        (1, 1, 1, 1)
    );
}

#[tokio::test]
async fn suspended_account_cannot_enqueue_or_dispatch_queued_work() {
    let fx = Fixture::new().await;
    fx.seed(&fx.owner, MemoryScope::Personal, "suspend-focal", 0)
        .await;
    fx.seed(&fx.other, MemoryScope::Personal, "suspend-positive", 0)
        .await;
    fx.enqueue(&fx.owner, MemoryScope::Personal, "queued-before-suspension")
        .await;
    set_status(&fx.db, &fx.owner, "suspended").await;
    let service = fx.service();
    assert!(
        service
            .enqueue(
                &fx.owner,
                scheduled(&fx.owner, MemoryScope::Personal, "clicked-while-suspended")
            )
            .await
            .is_err()
    );
    let worker = fx.worker(WorkerSettings::default());
    let blocked = FakeProvider::default();
    worker.step(now(), &blocked).await.unwrap();
    assert_eq!(
        blocked.reservations.load(Ordering::SeqCst),
        0,
        "suspended queued work reached selection/charge"
    );
    assert!(blocked.sent().is_empty());

    set_status(&fx.db, &fx.owner, "active").await;
    fx.enqueue(
        &fx.owner,
        MemoryScope::Personal,
        "suspend-between-reserve-and-send",
    )
    .await;
    let between = FakeProvider {
        after_reservation: Some((&fx.db, &fx.owner)),
        ..Default::default()
    };
    assert!(matches!(
        worker.step(now(), &between).await.unwrap(),
        Dispatch::Denied
    ));
    assert_eq!(
        between.reservations.load(Ordering::SeqCst),
        1,
        "did not exercise the post-reservation race"
    );
    assert!(
        between.sent().is_empty(),
        "suspension after reservation did not bar send"
    );

    fx.enqueue(&fx.other, MemoryScope::Personal, "active-positive")
        .await;
    let active = FakeProvider::default();
    assert!(matches!(
        worker.step(now(), &active).await.unwrap(),
        Dispatch::Completed { .. }
    ));
    let sent = active.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].partition.workspace, *fx.other.workspace());
}

#[tokio::test]
async fn revocation_and_policy_replacement_bar_queued_egress() {
    let fx = Fixture::new().await;
    fx.seed(&fx.owner, MemoryScope::Personal, "revoked-focal", 0)
        .await;
    fx.seed(&fx.other, MemoryScope::Personal, "replaced-focal", 0)
        .await;
    fx.enqueue(&fx.owner, MemoryScope::Personal, "revoked-queued")
        .await;
    let service = fx.service();
    let policy = service.effective_policy(&fx.owner).await.unwrap().policy;
    service
        .consent(
            &fx.owner,
            policy.version().consent_revision,
            ConsentChoice::Revoke,
        )
        .await
        .unwrap();
    let worker = fx.worker(WorkerSettings::default());
    let provider = FakeProvider::default();
    worker.step(now(), &provider).await.unwrap();
    fx.enqueue(&fx.other, MemoryScope::Personal, "replaced-queued")
        .await;
    let _replacement = configure(&fx.db, &fx.system, "replacement-model").await;
    worker.step(now(), &provider).await.unwrap();
    assert!(provider.sent().is_empty());
    assert_eq!(provider.reservations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn revocation_cancels_a_supported_in_flight_call() {
    let fx = Fixture::new().await;
    fx.seed(&fx.owner, MemoryScope::Personal, "cancel-focal", 0)
        .await;
    let attempt = fx
        .enqueue(&fx.owner, MemoryScope::Personal, "cancel-in-flight")
        .await;
    let worker = fx.worker(WorkerSettings::default());
    let provider = FakeProvider {
        hold: true,
        ..Default::default()
    };
    let service = fx.service();
    let policy = service.effective_policy(&fx.owner).await.unwrap().policy;
    let revoke = async {
        provider.entered.notified().await;
        service
            .consent(
                &fx.owner,
                policy.version().consent_revision,
                ConsentChoice::Revoke,
            )
            .await
            .unwrap();
        // Never release the provider's wait; only cancellation can unblock it.
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(worker.step(now(), &provider), revoke)
    })
    .await
    .expect("revocation must cancel the provider, not wait for its deadline");
    assert!(matches!(result.unwrap(), Dispatch::Denied));
    assert_eq!(
        service.attempt(&fx.owner, &attempt).await.unwrap().state,
        AttemptState::Cancelled
    );
    assert_eq!(
        provider.sent().len(),
        1,
        "no retry or secondary after revocation"
    );
}

#[tokio::test]
async fn secondary_disclosure_uses_dispatched_mode_and_consent_revisions() {
    for (change, reservations) in [
        (SecondaryChange::SwitchToHuman, 1),
        (SecondaryChange::RegrantAfterReservation, 2),
    ] {
        let fx = Fixture::new().await;
        fx.automatic().await;
        fx.seed(&fx.owner, MemoryScope::Personal, "secondary-focal", 0)
            .await;
        let attempt = fx
            .enqueue(&fx.owner, MemoryScope::Personal, "secondary-fence")
            .await;
        let provider = FakeProvider {
            secondary_change: Some((&fx, change)),
            ..Default::default()
        };
        let result = fx
            .worker(WorkerSettings::default())
            .step(now(), &provider)
            .await
            .unwrap();
        assert!(matches!(result, Dispatch::Denied));
        let sent = provider.sent();
        assert_eq!(sent.len(), 1, "only the primary may send");
        assert_eq!(sent[0].stage, Stage::Primary);
        assert_eq!(provider.reservations.load(Ordering::SeqCst), reservations);
        assert_eq!(
            fx.service()
                .attempt(&fx.owner, &attempt)
                .await
                .unwrap()
                .state,
            AttemptState::Cancelled
        );
    }
}

#[tokio::test]
async fn obsolete_policy_or_worker_generation_cannot_authorize_application() {
    for replace_policy in [false, true] {
        let fx = Fixture::new().await;
        fx.automatic().await;
        fx.seed(&fx.owner, MemoryScope::Personal, "automatic-focal", 0)
            .await;
        fx.enqueue(&fx.owner, MemoryScope::Personal, "automatic")
            .await;
        let worker = fx.worker(WorkerSettings::default());
        let Dispatch::Completed {
            output: WorkerOutput::Automatic(candidate),
            ..
        } = worker.step(now(), &FakeProvider::default()).await.unwrap()
        else {
            panic!("missing automatic handoff candidate");
        };
        assert!(
            worker.authorize_application(&candidate).await.unwrap(),
            "current authority positive control"
        );
        if replace_policy {
            let _replacement = configure(&fx.db, &fx.system, "replacement-model").await;
            assert!(!worker.authorize_application(&candidate).await.unwrap());
            // Returning to the original configuration cannot restore the old generation.
            let _restored = configure(&fx.db, &fx.system, "first-model").await;
        } else {
            worker.recover_interrupted(now()).await.unwrap();
        }
        assert!(
            !worker.authorize_application(&candidate).await.unwrap(),
            "obsolete authority could apply: policy_replaced={replace_policy}"
        );
    }
}
