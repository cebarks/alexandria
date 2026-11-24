use std::collections::BTreeMap;

use alexandria_engine::refinement::policy::{InferencePolicyInputs, Protocol, Route, RoutePair};
use alexandria_mcp::refinement::inference::{FinishReason, InferenceError, InferenceReply};
use alexandria_mcp::refinement::provider::prompt_template;
use alexandria_mcp::refinement::worker::{
    Call, Dispatch, Stage, Worker, WorkerOutput, WorkerProvider, WorkerQuota, WorkerSettings,
};
use alexandria_mcp::refinement::{ReconciledRefinement, RefinementService};
use alexandria_mcp::scope_service::ScopeService;
use alexandria_storage::models::identity::{
    FactId, MemoryScope, Partition, ProjectId, RecordId, ToSql, Version,
};
use alexandria_storage::models::refinement::{ApprovalMode, ConsentChoice, ProposalRevision};
use alexandria_storage::project_handle::parse_project_handle;
use alexandria_storage::repos::RefinementRepo;
use alexandria_storage::repos::classification_repo::ClassificationRepo;
use alexandria_storage::repos::refinement_repo::{EnqueueRequest, WorkKind, WorkSelection};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{Database, OwnerContext, SystemContext};

pub(super) struct Seeded {
    pub config: ReconciledRefinement,
    pub opinion_attempt: String,
    pub regenerate_attempt: String,
}

pub(super) async fn review_scenarios(
    db: &Database,
    system: &SystemContext,
    owner: &OwnerContext,
    origin: &str,
) -> Result<Seeded> {
    let config = configure(db, system, owner, origin).await?;
    let settings = WorkerSettings::default();
    let worker = Worker::new(db, system, &config, settings);
    let service = RefinementService::new(db, Some(&config));
    let provider = LocalFixtureProvider;
    let repo = RefinementRepo::new(db);

    for (key, label) in [
        ("approve-rollback", "approve and rollback"),
        ("dismiss", "dismiss"),
        ("defer", "defer"),
        ("move-conflict", "move while review is open"),
    ] {
        let project = add_project(db, owner, key, label).await?;
        add_fact(db, owner, key, label, Some(&project)).await?;
        let pending = assess(owner, &service, &worker, &provider, project, key).await?;
        repo.persist_proposal(owner, pending).await?;
    }

    // Opinion/regenerate use the real worker. The browser has no live enqueue handler;
    // linked attempt pages let an owner inspect their outcomes without a fixture-only UI.
    let project = add_project(db, owner, "second-opinion", "opinion and regenerate").await?;
    add_fact(
        db,
        owner,
        "second-opinion",
        "opinion and regenerate",
        Some(&project),
    )
    .await?;
    let pending = assess(
        owner,
        &service,
        &worker,
        &provider,
        project.clone(),
        "second-opinion",
    )
    .await?;
    let pending = repo.persist_proposal(owner, pending).await?;
    let mut attempts = Vec::new();
    for kind in [WorkKind::Opinion, WorkKind::Regenerate] {
        let request = EnqueueRequest {
            owner: owner.account().clone(),
            partition: Partition {
                workspace: owner.workspace().clone(),
                scope: MemoryScope::Project(project.clone()),
            },
            kind,
            source_revision: Some(pending.revision),
            selection: WorkSelection::Proposal(pending.id.clone()),
            idempotency_key: format!("review-fixture-{}", kind.as_str()),
        };
        let attempt = service.enqueue(owner, request).await?;
        let dispatch = worker.step(Utc::now(), &provider).await?;
        match (kind, dispatch) {
            (
                WorkKind::Opinion,
                Dispatch::Completed {
                    output: WorkerOutput::Opinion(_),
                    ..
                },
            ) => {}
            (
                WorkKind::Regenerate,
                Dispatch::Completed {
                    output: WorkerOutput::Proposal(regenerated),
                    ..
                },
            ) => {
                repo.persist_proposal_revision(owner, regenerated).await?;
            }
            (_, result) => anyhow::bail!("fixture review attempt did not complete: {result:?}"),
        }
        attempts.push(alexandria_storage::record_id_to_string(attempt.record()));
    }
    let destination =
        add_project(db, owner, "classification", "classification destination").await?;
    let hidden = add_fact(db, owner, "unassigned", "quarantined classification", None).await?;
    ClassificationRepo::new(db.inner(), owner)
        .propose(
            &hidden,
            Version::new(1),
            MemoryScope::Project(destination),
            Some("Keep quarantine while filing this synthetic note."),
        )
        .await?;

    let [opinion_attempt, regenerate_attempt] = attempts
        .try_into()
        .map_err(|_| anyhow::anyhow!("fixture review attempt count"))?;
    Ok(Seeded {
        config,
        opinion_attempt,
        regenerate_attempt,
    })
}

async fn configure(
    db: &Database,
    system: &SystemContext,
    owner: &OwnerContext,
    origin: &str,
) -> Result<ReconciledRefinement> {
    let routes = RoutePair::resolve(
        Route {
            protocol: Protocol::OpenAiChatCompletions,
            api_prefix: format!("{origin}/fixture-no-egress"),
            model: "in-process-review-stub".into(),
            credential_ref: None,
            headers: BTreeMap::new(),
            output_limit_field: None,
        },
        None,
    )?;
    let config = ReconciledRefinement::reconcile(
        db,
        system,
        Some(routes),
        &InferencePolicyInputs {
            prompt_template: prompt_template().as_bytes(),
            prompt_template_version: "1",
            operation_schema_version: "1",
            settings: &BTreeMap::new(),
        },
        ApprovalMode::Human,
    )
    .await?
    .context("fixture policy")?;
    RefinementService::new(db, Some(&config))
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

async fn add_project(
    db: &Database,
    owner: &OwnerContext,
    key: &str,
    label: &str,
) -> Result<ProjectId> {
    let project = ScopeService::new(db)
        .establish_project(owner, Some(key), None, Some(label))
        .await?;
    Ok(parse_project_handle(&project.descriptor.id)?)
}

async fn add_fact(
    db: &Database,
    owner: &OwnerContext,
    key: &str,
    label: &str,
    project: Option<&ProjectId>,
) -> Result<FactId> {
    let id = FactId::try_from(RecordId::new("fact", format!("review-fixture-{key}")))?;
    db.inner().query("CREATE $fact SET account = $owner, workspace = $workspace, scope = $scope, project = $project, \
        content = $text, tags = [], created_at = time::now(), semantic_revision = 1, embedding = $embedding, \
        quarantined_at = IF $hidden THEN time::now() ELSE NONE END RETURN NONE;")
        .bind(("fact", id.as_record_id().clone())).bind(("owner", owner.account().as_record_id().clone()))
        .bind(("workspace", owner.workspace().as_record_id().clone()))
        .bind(("scope", if project.is_some() { "project" } else { "unassigned" }))
        .bind(("project", project.map(|project| project.as_record_id().clone())))
        .bind(("text", format!("{label}: the synthetic local index can take a moment on startup.")))
        .bind(("embedding", vec![1.0_f32, 0.001])).bind(("hidden", project.is_none())).await?.check()?;
    Ok(id)
}

async fn assess(
    owner: &OwnerContext,
    service: &RefinementService<'_>,
    worker: &Worker<'_>,
    provider: &LocalFixtureProvider,
    project: ProjectId,
    key: &str,
) -> Result<ProposalRevision> {
    let attempt = service
        .enqueue(
            owner,
            EnqueueRequest {
                owner: owner.account().clone(),
                partition: Partition {
                    workspace: owner.workspace().clone(),
                    scope: MemoryScope::Project(project),
                },
                kind: WorkKind::Assess,
                source_revision: None,
                selection: WorkSelection::Scheduled,
                idempotency_key: format!("seed-{key}"),
            },
        )
        .await?;
    let dispatch = worker.step(Utc::now(), provider).await?;
    let Dispatch::Completed {
        attempt: selected,
        output: WorkerOutput::Proposal(proposal),
        ..
    } = dispatch
    else {
        anyhow::bail!("fixture proposal {key} did not complete: {dispatch:?}");
    };
    ensure!(
        selected == attempt,
        "seeded assessment selected a different attempt"
    );
    Ok(proposal)
}

struct LocalFixtureProvider;

#[async_trait::async_trait]
impl WorkerProvider for LocalFixtureProvider {
    async fn complete(
        &self,
        call: Call<'_>,
        quota: &dyn WorkerQuota,
        _cancel: CancellationToken,
    ) -> Result<InferenceReply, InferenceError> {
        quota
            .reserve_attempt()
            .await
            .map_err(|_| InferenceError::QuotaDenied)?;
        quota
            .authorize_outbound()
            .await
            .map_err(|_| InferenceError::QuotaDenied)?;
        let answer = match call.stage {
            Stage::Secondary => {
                json!({"verdict":"approve_exact","rationale":"Only tighter wording; review remains manual."})
            }
            Stage::Primary => {
                let source = &call.evidence.focal;
                let (label, _) = source
                    .complete_content
                    .split_once(':')
                    .ok_or(InferenceError::InvalidRequest)?;
                let regenerated = call.kind == WorkKind::Regenerate;
                let verb = if regenerated {
                    "may briefly wait"
                } else {
                    "can wait briefly"
                };
                let suffix = if regenerated { " (regenerated)" } else { "" };
                json!({"category":"rewrite", "operation":{"kind":"rewrite", "source":source.id.as_record_id().to_sql(),
                       "content":format!("{label}: the synthetic local index {verb} on startup.")},
                       "rationale":format!("Fixture review: {label}{suffix}"), "uncertainty":""})
            }
        };
        Ok(InferenceReply {
            text: answer.to_string(),
            finish_reason: FinishReason::Stop,
            usage: None,
            configured_model: "in-process-review-stub".into(),
            reported_model: None,
        })
    }
}
