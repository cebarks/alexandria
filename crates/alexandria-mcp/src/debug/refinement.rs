//! Owner-facing refinement queue, proposal review, and destination-consent settings.
//!
//! Reads and decisions are account- and workspace-qualified; GET never applies a change.

use askama::Template;
use axum::Json;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::Value;

use super::html::{self, page};
use super::web::{self, Boundary, BrowserSession};
use crate::AlexandriaServer;
use crate::error::ServiceError;
use crate::refinement::service::RefinementService;
use alexandria_storage::access::OwnerContext;
use alexandria_storage::models::identity::{
    FactId, MemoryScope, OperationId, Partition, ProjectId, RefinementProposalId,
    SemanticOperationId, Version,
};
use alexandria_storage::models::mutation::OperationDetail;
use alexandria_storage::models::refinement::{
    ApprovalMode, ApprovalSetting, Consent, ConsentChoice, PolicyVersion, ProposalDisplay,
    ProposalState,
};
use alexandria_storage::repos::refinement_repo::{
    AttemptId, AttemptState, EnqueueRequest, EvidenceSelection, WorkKind, WorkSelection,
};
use alexandria_storage::repos::{MutationRepo, RefinementRepo};
use surrealdb::types::ToSql;

const NAV: &str = "refinement";

#[derive(Deserialize, Default)]
pub(crate) struct QueueFilter {
    scope: Option<String>,
    status: Option<String>,
}

pub(crate) async fn queue(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Query(filter): Query<QueueFilter>,
) -> Response {
    let Ok((owner, read)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    let projects = read
        .partitions()
        .iter()
        .filter_map(|partition| match &partition.scope {
            MemoryScope::Project(id) => Some(id.as_record_id().to_sql()),
            _ => None,
        })
        .collect();
    let filter_scope = filter.scope.unwrap_or_else(|| "all".to_string());
    let scope = match filter_scope.as_str() {
        "all" => None,
        "personal" | "project" | "unassigned" => Some(filter_scope.as_str()),
        _ => return bad_request("unknown refinement scope filter"),
    };
    let deferred = match filter.status.as_deref() {
        None | Some("regular") => false,
        Some("deferred") => true,
        _ => return bad_request("unknown refinement status filter"),
    };
    let service = RefinementService::new(server.db.as_ref(), server.refinement.as_deref());
    let proposals = match service.owner_proposals(&owner, scope, deferred).await {
        Ok(proposals) => proposals,
        Err(_) => return unavailable(),
    };
    let effective = match service.effective_policy(&owner).await {
        Ok(policy) => policy,
        Err(_) => return unavailable(),
    };
    let usage = service.owner_usage(&owner).await;
    let attempt_status = match usage {
        Ok(usage) => format!(
            "{} queued refinement attempt(s) for this account; {} provider call(s) charged in the \
             rolling window",
            usage.queued, usage.account
        ),
        Err(_) => "attempt counts are unavailable".to_string(),
    };
    let current_policy = effective
        .policy
        .configured
        .as_ref()
        .map(|policy| policy.configuration.inference_policy_id.0.as_str());
    let rows = proposals
        .into_iter()
        .map(|proposal| queue_row(proposal, current_policy))
        .collect::<Vec<_>>();
    private_page(QueueTemplate {
        nav: NAV,
        message: format!(
            "{} owner-scoped proposal revision(s) match this filter.",
            rows.len()
        ),
        approval_mode: approval_mode_label(effective.approval_mode).to_string(),
        attempt_status,
        filter_scope,
        deferred,
        rows,
        projects,
        enqueue: EnqueueControls::new(&session, &effective),
    })
}

pub(crate) async fn detail(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Path(id): Path<String>,
) -> Response {
    let Ok((owner, _)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    let Ok(id) = alexandria_storage::parse_record_id(&id) else {
        return not_found();
    };
    if id.table.as_str() != "refinement_proposal" {
        return not_found();
    }
    let service = RefinementService::new(server.db.as_ref(), server.refinement.as_deref());
    let proposal = match service.owner_proposal(&owner, &id).await {
        Ok(Some(proposal)) => proposal,
        Ok(None) => return not_found(),
        Err(_) => return unavailable(),
    };
    let effective = match service.effective_policy(&owner).await {
        Ok(policy) => policy,
        Err(_) => return unavailable(),
    };
    let current_policy = effective
        .policy
        .configured
        .as_ref()
        .map(|policy| policy.configuration.inference_policy_id.0.as_str());
    let (same_model, different_model) =
        model_comparison(&proposal.origin_inference_policy_id, current_policy);
    let model_disclosure = model_disclosure(&effective, same_model, different_model);
    let operation = match proposal.operation.as_ref() {
        Some(operation_id) => {
            if operation_id.table.as_str() != "semantic_operation" {
                return not_found();
            }
            let repo = MutationRepo::new(server.db.inner());
            match repo.operation_by_id(&owner, operation_id).await {
                Ok(operation) => operation,
                Err(_) => return unavailable(),
            }
        }
        None => None,
    };
    let (preimage, postimage, operation_id, actor, revert_history) =
        application_history(operation.as_ref());
    let payload = parse_payload(&proposal.proposal_payload);
    private_page(DetailTemplate {
        nav: NAV,
        proposal_id: alexandria_storage::record_id_to_string(&proposal.id),
        revision: proposal.revision,
        state: format!("{:?}", proposal.state).to_lowercase(),
        proposal_path: id.to_sql(),
        csrf: session.csrf().to_string(),
        decision_key: uuid::Uuid::new_v4().to_string(),
        can_decide: proposal.state == ProposalState::Pending,
        enqueue: EnqueueControls::new(&session, &effective),
        can_revert: proposal.state == ProposalState::Applied && proposal.operation.is_some(),
        source: source_summary(&payload),
        selected_records: selected_records(&payload),
        changes: change_summary(&payload),
        rationale: payload_text(&payload, "rationale"),
        uncertainty: payload_text(&payload, "uncertainty"),
        model_disclosure,
        same_model: if same_model { "yes" } else { "no" }.to_string(),
        different_model: if different_model { "yes" } else { "no" }.to_string(),
        operation_id,
        actor,
        preimage,
        postimage,
        revert_history,
    })
}

pub(crate) async fn settings(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
) -> Response {
    let Ok((owner, _)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    match RefinementService::new(&server.db, server.refinement.as_deref())
        .effective_policy(&owner)
        .await
    {
        Ok(effective) => {
            let same_model = effective.routes.as_ref().map_or_else(
                || "not configured".to_string(),
                |routes| {
                    if routes.same_model {
                        "yes".into()
                    } else {
                        "no".into()
                    }
                },
            );
            let usage = RefinementService::new(&server.db, server.refinement.as_deref())
                .owner_usage(&owner)
                .await;
            let (quota_status, attempt_status) = match usage {
                Ok(usage) => (
                    format!(
                        "{} account-window and {} interactive-window provider call(s) in the \
                         rolling 24-hour window",
                        usage.account, usage.interactive
                    ),
                    format!("{} queued refinement attempt(s)", usage.queued),
                ),
                Err(_) => (
                    "quota counts are unavailable".to_string(),
                    "attempt counts are unavailable".to_string(),
                ),
            };
            private_page(SettingsTemplate {
                nav: NAV,
                csrf: session.csrf().to_string(),
                consent: consent_label(&effective),
                destination: destination_disclosure(&effective),
                approval_mode: approval_mode_label(effective.approval_mode).to_string(),
                quota_status,
                attempt_status,
                selected_mode: match effective.approval_mode {
                    ApprovalMode::Human => "human",
                    ApprovalMode::SecondaryLlm => "secondary_llm",
                }
                .to_string(),
                same_model,
                consent_revision: effective.policy.version().consent_revision,
                policy_revision: effective
                    .policy
                    .version()
                    .configured_revision
                    .unwrap_or_default(),
                configured_revision: effective
                    .policy
                    .version()
                    .configured_revision
                    .map_or_else(String::new, |value| value.to_string()),
                mode_revision: effective.policy.version().owner_mode_revision,
            })
        }
        Err(_) => unavailable(),
    }
}

#[derive(Deserialize)]
pub(crate) struct ConsentForm {
    /// The router guard verifies this raw field before the handler runs.
    #[allow(dead_code)]
    pub csrf: String,
    pub revision: u64,
    pub policy_revision: u64,
    pub choice: String,
}

pub(crate) async fn consent(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<ConsentForm>,
) -> Response {
    let Ok((owner, _)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    let choice = match form.choice.as_str() {
        "grant" => ConsentChoice::Grant {
            disclosed_policy_revision: form.policy_revision,
        },
        "revoke" => ConsentChoice::Revoke,
        _ => return bad_request("unknown consent choice"),
    };
    settings_result(set_consent(server, owner, form.revision, choice).await)
}

#[derive(Deserialize)]
pub(crate) struct ModeForm {
    /// The router guard verifies this raw field before the handler runs.
    #[allow(dead_code)]
    pub csrf: String,
    pub configured_revision: Option<u64>,
    pub consent_revision: u64,
    pub mode_revision: u64,
    pub mode: String,
}

pub(crate) async fn mode(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<ModeForm>,
) -> Response {
    let owner = session.owner().clone();
    drop(boundary);
    drop(session);
    let mode = match form.mode.as_str() {
        "human" => ApprovalMode::Human,
        "secondary_llm" => ApprovalMode::SecondaryLlm,
        _ => return bad_request("unknown approval mode"),
    };
    let expected = PolicyVersion {
        configured_revision: form.configured_revision,
        consent_revision: form.consent_revision,
        owner_mode_revision: form.mode_revision,
    };
    settings_result(set_mode(server, owner, expected, mode).await)
}

fn settings_result<T>(result: Result<T, ServiceError>) -> Response {
    let mut response = match result {
        Ok(_) => Redirect::to("/debug/refinement/settings").into_response(),
        Err(ServiceError::Stale | ServiceError::RetryableBookkeepingConflict) => {
            let mut response = html::error_page(
                NAV,
                "refinement settings changed; reload the settings before submitting again",
            );
            *response.status_mut() = StatusCode::CONFLICT;
            response
        }
        Err(_) => unavailable(),
    };
    web::no_store(&mut response);
    response
}

async fn set_consent(
    server: AlexandriaServer,
    owner: OwnerContext,
    revision: u64,
    choice: ConsentChoice,
) -> Result<Consent, ServiceError> {
    let service = RefinementService::new(&server.db, server.refinement.as_deref());
    service.consent(&owner, revision, choice).await
}

async fn set_mode(
    server: AlexandriaServer,
    owner: OwnerContext,
    expected: PolicyVersion,
    mode: ApprovalMode,
) -> Result<ApprovalSetting, ServiceError> {
    let service = RefinementService::new(&server.db, server.refinement.as_deref());
    service.set_approval_mode(&owner, &expected, mode).await
}

#[derive(Deserialize)]
pub(crate) struct EnqueueForm {
    action: String,
    scope: Option<String>,
    target: Option<String>,
    revision: Option<u64>,
    request_key: uuid::Uuid,
    configured_revision: Option<u64>,
    consent_revision: u64,
    mode_revision: u64,
}

pub(crate) async fn enqueue(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<EnqueueForm>,
) -> Response {
    // The protected router has already verified the origin and CSRF token in this form body.
    let Ok((owner, read)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    let mut response = match enqueue_owner(&server, &owner, &read, form).await {
        Ok(id) => Redirect::to(&format!(
            "/debug/refinement/attempts/{}",
            url_component(&id.record().to_sql())
        ))
        .into_response(),
        Err(ServiceError::Stale) => {
            let mut response = html::error_page(
                NAV,
                "refinement settings or source changed; reload before submitting again",
            );
            *response.status_mut() = StatusCode::CONFLICT;
            response
        }
        Err(ServiceError::NotFound) => not_found(),
        Err(ServiceError::InvalidInput(_)) => bad_request("invalid refinement request"),
        Err(ServiceError::Quota) => {
            let mut response =
                html::error_page(NAV, "the refinement queue is full; try again later");
            *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
            response
        }
        Err(_) => unavailable(),
    };
    web::no_store(&mut response);
    response
}

fn url_component(value: &str) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => write!(out, "%{byte:02X}").expect("writing to String"),
        }
    }
    out
}

async fn enqueue_owner(
    server: &AlexandriaServer,
    owner: &OwnerContext,
    read: &alexandria_storage::access::ScopedContext,
    form: EnqueueForm,
) -> Result<AttemptId, ServiceError> {
    let service = RefinementService::new(&server.db, server.refinement.as_deref());
    let expected = PolicyVersion {
        configured_revision: form.configured_revision,
        consent_revision: form.consent_revision,
        owner_mode_revision: form.mode_revision,
    };
    let (kind, scope, selection) = match form.action.as_str() {
        "run" => {
            let scope = match form.scope.as_deref() {
                Some("personal") => MemoryScope::Personal,
                Some(handle) => MemoryScope::Project(ProjectId::try_from(
                    alexandria_storage::parse_record_id(handle)?,
                )?),
                None => {
                    return Err(ServiceError::InvalidInput(
                        "missing assessment scope".into(),
                    ));
                }
            };
            if !read
                .partitions()
                .iter()
                .any(|partition| partition.scope == scope)
            {
                return Err(ServiceError::NotFound);
            }
            (WorkKind::Assess, scope, WorkSelection::Scheduled)
        }
        "opinion" | "regenerate" => {
            let id = form_target(&form)
                .and_then(|id| RefinementProposalId::try_from(id).map_err(ServiceError::from))?;
            let proposal = service
                .owner_proposal(owner, id.as_record_id())
                .await?
                .ok_or(ServiceError::NotFound)?;
            if Some(proposal.revision) != form.revision {
                return Err(ServiceError::Stale);
            }
            let scope = proposal_scope(proposal.scope.as_str(), proposal.project)?;
            let kind = if form.action == "opinion" {
                WorkKind::Opinion
            } else {
                WorkKind::Regenerate
            };
            (
                kind,
                scope,
                WorkSelection::Proposal(id.as_record_id().clone()),
            )
        }
        "assess_unassigned" => {
            let id = FactId::try_from(form_target(&form)?)?;
            let snapshot = MutationRepo::new(server.db.inner())
                .state(owner, &id)
                .await?
                .filter(|fact| {
                    fact.scope == MemoryScope::Unassigned
                        && !fact.visibility.deleted
                        && fact.visibility.quarantined_at.is_some()
                })
                .ok_or(ServiceError::NotFound)?;
            if Some(snapshot.version.get()) != form.revision {
                return Err(ServiceError::Stale);
            }
            (
                WorkKind::AssessUnassigned,
                MemoryScope::Unassigned,
                WorkSelection::Evidence(EvidenceSelection {
                    focal: id,
                    related: Vec::new(),
                }),
            )
        }
        _ => {
            return Err(ServiceError::InvalidInput(
                "unknown refinement action".into(),
            ));
        }
    };
    let effective = service.effective_policy(owner).await?;
    if effective.policy.version() != expected {
        return Err(ServiceError::Stale);
    }
    if !effective.policy.inference_admitted {
        return Err(ServiceError::Unavailable);
    }
    service
        .enqueue_checked(
            owner,
            EnqueueRequest {
                owner: owner.account().clone(),
                partition: Partition {
                    workspace: owner.workspace().clone(),
                    scope,
                },
                kind,
                source_revision: form.revision.map(Version::new),
                selection,
                idempotency_key: format!("browser-{}-{}", kind.as_str(), form.request_key),
            },
            expected,
        )
        .await
}

fn form_target(form: &EnqueueForm) -> Result<surrealdb::types::RecordId, ServiceError> {
    let target = form
        .target
        .as_deref()
        .ok_or_else(|| ServiceError::InvalidInput("missing refinement target".into()))?;
    Ok(alexandria_storage::parse_record_id(target)?)
}

fn proposal_scope(
    scope: &str,
    project: Option<surrealdb::types::RecordId>,
) -> Result<MemoryScope, ServiceError> {
    match (scope, project) {
        ("personal", None) => Ok(MemoryScope::Personal),
        ("unassigned", None) => Ok(MemoryScope::Unassigned),
        ("project", Some(id)) => Ok(MemoryScope::Project(ProjectId::try_from(id)?)),
        _ => Err(ServiceError::NotFound),
    }
}

#[derive(Deserialize)]
pub(crate) struct DecisionForm {
    revision: u64,
    decision_key: uuid::Uuid,
}

#[derive(Clone, Copy)]
enum Decision {
    Approve,
    Dismiss,
    Defer,
    Revert,
}

pub(crate) async fn approve(
    State(server): State<AlexandriaServer>,
    session: BrowserSession,
    Path(id): Path<String>,
    Form(form): Form<DecisionForm>,
) -> Response {
    decide(server, session.owner().clone(), id, form, Decision::Approve).await
}

pub(crate) async fn dismiss(
    State(server): State<AlexandriaServer>,
    session: BrowserSession,
    Path(id): Path<String>,
    Form(form): Form<DecisionForm>,
) -> Response {
    decide(server, session.owner().clone(), id, form, Decision::Dismiss).await
}

pub(crate) async fn defer(
    State(server): State<AlexandriaServer>,
    session: BrowserSession,
    Path(id): Path<String>,
    Form(form): Form<DecisionForm>,
) -> Response {
    decide(server, session.owner().clone(), id, form, Decision::Defer).await
}

pub(crate) async fn revert(
    State(server): State<AlexandriaServer>,
    session: BrowserSession,
    Path(id): Path<String>,
    Form(form): Form<DecisionForm>,
) -> Response {
    decide(server, session.owner().clone(), id, form, Decision::Revert).await
}

async fn decide(
    server: AlexandriaServer,
    owner: OwnerContext,
    id: String,
    form: DecisionForm,
    decision: Decision,
) -> Response {
    let Ok(id) = alexandria_storage::parse_record_id(&id).and_then(RefinementProposalId::try_from)
    else {
        return not_found();
    };
    let result = review(&server, &owner, &id, form, decision).await;
    let mut response = match result {
        Ok(()) => Redirect::to("/debug/refinement").into_response(),
        Err(ServiceError::NotFound) => not_found(),
        Err(ServiceError::Stale) => {
            // The live refusal keeps the placeholder's guidance: the decision was refused because
            // the proposal is stale or moved, so the owner must reload before acting again.
            let mut response = html::error_page(
                NAV,
                "this proposal is stale or was moved; reload the proposal before deciding",
            );
            *response.status_mut() = StatusCode::CONFLICT;
            response
        }
        Err(ServiceError::InvalidInput(_)) => {
            bad_request("This proposal cannot be applied as reviewed.")
        }
        Err(_) => unavailable(),
    };
    web::no_store(&mut response);
    response
}

async fn review(
    server: &AlexandriaServer,
    owner: &OwnerContext,
    id: &RefinementProposalId,
    form: DecisionForm,
    decision: Decision,
) -> Result<(), ServiceError> {
    let repo = RefinementRepo::new(&server.db);
    let service =
        RefinementService::new(&server.db, None).with_embedding(server.embedding.as_ref());
    let revision = Version::new(form.revision);
    match decision {
        Decision::Dismiss => {
            repo.dismiss_proposal_revision(owner, id.as_record_id(), revision)
                .await?
        }
        Decision::Defer => repo.defer_proposal_revision(owner, id, revision).await?,
        Decision::Approve => {
            let proposal = repo.owner_proposal_revision(owner, id).await?;
            if proposal.revision != revision {
                return Err(ServiceError::Stale);
            }
            let operation = OperationId::new(format!("quality-review-{}", form.decision_key));
            service.approve(owner, &proposal, &operation).await?;
        }
        Decision::Revert => {
            let proposal = service
                .owner_proposal(owner, id.as_record_id())
                .await?
                .ok_or(ServiceError::NotFound)?;
            if proposal.revision != form.revision {
                return Err(ServiceError::Stale);
            }
            let applied =
                SemanticOperationId::try_from(proposal.operation.ok_or(ServiceError::Stale)?)?;
            let operation = OperationId::new(format!("quality-revert-{}", form.decision_key));
            service.revert(owner, &applied, &operation).await?;
        }
    }
    Ok(())
}
pub(crate) async fn attempt(
    State(server): State<AlexandriaServer>,
    session: BrowserSession,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(id) = alexandria_storage::parse_record_id(&id).and_then(AttemptId::try_from) else {
        return not_found();
    };
    let owner = session.owner().clone();
    let service = RefinementService::new(&server.db, server.refinement.as_deref());
    let mut response = match service.attempt(&owner, &id).await {
        Ok(attempt) => {
            let view = AttemptStatus {
                id: attempt.id.record().to_sql(),
                kind: attempt.kind.as_str(),
                state: attempt.state.as_str(),
                poll: matches!(attempt.state, AttemptState::Queued | AttemptState::Running),
            };
            if headers
                .get("HX-Request")
                .is_some_and(|value| value == "true")
            {
                private_page(AttemptStatusTemplate { status: view })
            } else if headers
                .get(header::ACCEPT)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|accept| {
                    accept
                        .split(',')
                        .any(|value| value.trim().starts_with("text/html"))
                })
            {
                private_page(AttemptTemplate {
                    nav: NAV,
                    status: view,
                })
            } else {
                Json(serde_json::json!({
                    "id": view.id, "kind": view.kind, "state": view.state,
                }))
                .into_response()
            }
        }
        Err(ServiceError::NotFound) => not_found(),
        Err(_) => unavailable(),
    };
    web::no_store(&mut response);
    response
}

fn queue_row(proposal: ProposalDisplay, current_policy: Option<&str>) -> QueueRow {
    let payload = parse_payload(&proposal.proposal_payload);
    let (_, different) = model_comparison(&proposal.origin_inference_policy_id, current_policy);
    let model_disclosure = if current_policy.is_none() {
        "origin policy recorded; current model unavailable".to_string()
    } else if different {
        "different model or policy from the one that assessed this proposal".to_string()
    } else {
        "same model and policy generation as the assessment".to_string()
    };
    QueueRow {
        id: alexandria_storage::record_id_to_string(&proposal.id),
        proposal_path: proposal.id.to_sql(),
        revision: proposal.revision,
        state: format!("{:?}", proposal.state).to_lowercase(),
        scope: proposal.scope,
        sources: source_summary(&payload),
        changes: change_summary(&payload),
        rationale: payload_text(&payload, "rationale"),
        uncertainty: payload_text(&payload, "uncertainty"),
        model_disclosure,
        needs_human: proposal.needs_human,
        deferred: proposal.deferred,
    }
}

fn model_comparison(origin: &str, current: Option<&str>) -> (bool, bool) {
    match current {
        Some(current) if current == origin => (true, false),
        Some(_) => (false, true),
        None => (false, false),
    }
}

fn model_disclosure(
    effective: &crate::refinement::service::EffectiveRefinementPolicy,
    same_model: bool,
    different_model: bool,
) -> String {
    let comparison = if same_model {
        "same model and inference policy as the proposal origin"
    } else if different_model {
        "different model or inference policy; this proposal's assessment may be stale"
    } else {
        "current model configuration is unavailable for comparison"
    };
    let routes = effective.routes.as_ref().map_or_else(
        || "No active endpoint is configured.".to_string(),
        |routes| {
            format!(
                "Proposer endpoint/model: {}://{}:{}{} / {}; reviewer endpoint/model: {}://{}:{}{} / {}.",
                routes.proposer.destination.scheme,
                routes.proposer.destination.host,
                routes.proposer.destination.effective_port,
                routes.proposer.destination.api_prefix,
                routes.proposer.model,
                routes.reviewer.destination.scheme,
                routes.reviewer.destination.host,
                routes.reviewer.destination.effective_port,
                routes.reviewer.destination.api_prefix,
                routes.reviewer.model,
            )
        },
    );
    format!("{comparison}. {routes} Credentials are not displayed.")
}

fn consent_label(effective: &crate::refinement::service::EffectiveRefinementPolicy) -> String {
    match (
        &effective.policy.consent,
        effective.policy.inference_admitted,
    ) {
        (None, _) => "Not granted".to_string(),
        (Some(_), true) => "Granted for the current destinations".to_string(),
        (Some(_), false) => "Previously granted; not admitted for the current policy".to_string(),
    }
}

fn destination_disclosure(
    effective: &crate::refinement::service::EffectiveRefinementPolicy,
) -> String {
    effective.routes.as_ref().map_or_else(
        || "No endpoint configured".to_string(),
        |routes| {
            format!(
                "Proposer endpoint {}://{}:{}{} (model {}); reviewer endpoint {}://{}:{}{} (model {}).",
                routes.proposer.destination.scheme,
                routes.proposer.destination.host,
                routes.proposer.destination.effective_port,
                routes.proposer.destination.api_prefix,
                routes.proposer.model,
                routes.reviewer.destination.scheme,
                routes.reviewer.destination.host,
                routes.reviewer.destination.effective_port,
                routes.reviewer.destination.api_prefix,
                routes.reviewer.model,
            )
        },
    )
}

fn approval_mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Human => "Human approval required by default",
        ApprovalMode::SecondaryLlm => "Automatic secondary review is enabled by owner choice",
    }
}

fn parse_payload(payload: &str) -> Value {
    serde_json::from_str(payload).unwrap_or(Value::Null)
}

fn payload_text(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .unwrap_or("(not provided)")
        .to_string()
}

fn source_summary(payload: &Value) -> String {
    let ids = payload
        .get("sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|source| source.get("fact").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if ids.is_empty() {
        payload
            .pointer("/evidence/focal/id")
            .and_then(Value::as_str)
            .map(|id| format!("source {id}"))
            .unwrap_or_else(|| "source unavailable".to_string())
    } else {
        format!(
            "{} selected source record(s): {}",
            ids.len(),
            ids.join(", ")
        )
    }
}

fn change_summary(payload: &Value) -> String {
    payload
        .get("operation")
        .map(|operation| operation.to_string())
        .unwrap_or_else(|| "proposed change unavailable".to_string())
}

fn selected_records(payload: &Value) -> String {
    let mut records = Vec::new();
    if let Some(focal) = payload.pointer("/evidence/focal") {
        records.push(candidate_line(focal));
    }
    if let Some(related) = payload
        .pointer("/evidence/related")
        .and_then(Value::as_array)
    {
        records.extend(related.iter().map(candidate_line));
    }
    if records.is_empty() {
        source_summary(payload)
    } else {
        records.join("\n\n")
    }
}

fn candidate_line(candidate: &Value) -> String {
    let id = candidate
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown record");
    let version = candidate
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let content = candidate
        .get("complete_content")
        .and_then(Value::as_str)
        .unwrap_or("(content unavailable)");
    format!("{id} · revision {version}\n{content}")
}

fn application_history(
    operation: Option<&OperationDetail>,
) -> (String, String, String, String, String) {
    let Some(operation) = operation else {
        return (
            String::new(),
            String::new(),
            String::new(),
            "—".to_string(),
            "No linked application or revert history is recorded.".to_string(),
        );
    };
    let preimage = operation
        .preimage_records
        .iter()
        .map(|record| format!("{record:#?}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let postimage = operation
        .postimage_records
        .iter()
        .map(|record| format!("{record:#?}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let reverts = if operation.reverts.is_some() {
        "This operation is recorded as a revert of an earlier owner operation."
    } else {
        "This operation does not revert an earlier operation."
    };
    let reverted_by = if operation.reverted_by.is_some() {
        " A later owner operation is recorded as reverting this application."
    } else {
        " No later revert is recorded."
    };
    (
        preimage,
        postimage,
        operation.id.clone(),
        if operation.actor.is_empty() {
            "owner".into()
        } else {
            operation.actor.clone()
        },
        format!("{reverts}{reverted_by}"),
    )
}

fn private_page<T: Template>(template: T) -> Response {
    let mut response = page(template);
    web::no_store(&mut response);
    response
}

fn forbidden() -> Response {
    let mut response = html::error_page(NAV, "this page requires an authenticated owner session");
    *response.status_mut() = StatusCode::FORBIDDEN;
    response
}

fn unavailable() -> Response {
    let mut response = html::error_page(NAV, "refinement proposal service is not available");
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response
}

fn bad_request(message: &str) -> Response {
    let mut response = html::error_page(NAV, message);
    *response.status_mut() = StatusCode::BAD_REQUEST;
    response
}

fn not_found() -> Response {
    let mut response = html::error_page(NAV, "proposal not found");
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

#[derive(Template)]
#[template(path = "refinement_queue.html")]
struct QueueTemplate {
    nav: &'static str,
    message: String,
    approval_mode: String,
    attempt_status: String,
    filter_scope: String,
    deferred: bool,
    rows: Vec<QueueRow>,
    projects: Vec<String>,
    enqueue: EnqueueControls,
}

struct EnqueueControls {
    enabled: bool,
    csrf: String,
    key: uuid::Uuid,
    configured_revision: u64,
    consent_revision: u64,
    mode_revision: u64,
}

impl EnqueueControls {
    fn new(
        session: &BrowserSession,
        effective: &crate::refinement::service::EffectiveRefinementPolicy,
    ) -> Self {
        let version = effective.policy.version();
        Self {
            enabled: effective.policy.inference_admitted,
            csrf: session.csrf().to_string(),
            key: uuid::Uuid::new_v4(),
            configured_revision: version.configured_revision.unwrap_or_default(),
            consent_revision: version.consent_revision,
            mode_revision: version.owner_mode_revision,
        }
    }
}

struct QueueRow {
    id: String,
    proposal_path: String,
    revision: u64,
    state: String,
    scope: String,
    sources: String,
    changes: String,
    rationale: String,
    uncertainty: String,
    model_disclosure: String,
    needs_human: bool,
    deferred: bool,
}

#[derive(Template)]
#[template(path = "refinement_detail.html")]
struct DetailTemplate {
    nav: &'static str,
    proposal_id: String,
    proposal_path: String,
    revision: u64,
    state: String,
    csrf: String,
    decision_key: String,
    can_decide: bool,
    can_revert: bool,
    enqueue: EnqueueControls,
    source: String,
    selected_records: String,
    changes: String,
    rationale: String,
    uncertainty: String,
    model_disclosure: String,
    same_model: String,
    different_model: String,
    operation_id: String,
    actor: String,
    preimage: String,
    postimage: String,
    revert_history: String,
}

#[derive(Template)]
#[template(path = "refinement_attempt.html")]
struct AttemptTemplate {
    nav: &'static str,
    status: AttemptStatus,
}

#[derive(Template)]
#[template(path = "_refinement_attempt_status.html")]
struct AttemptStatusTemplate {
    status: AttemptStatus,
}

struct AttemptStatus {
    id: String,
    kind: &'static str,
    state: &'static str,
    poll: bool,
}

#[derive(Template)]
#[template(path = "refinement_settings.html")]
struct SettingsTemplate {
    nav: &'static str,
    csrf: String,
    consent: String,
    destination: String,
    approval_mode: String,
    quota_status: String,
    attempt_status: String,
    selected_mode: String,
    same_model: String,
    consent_revision: u64,
    policy_revision: u64,
    configured_revision: String,
    mode_revision: u64,
}
