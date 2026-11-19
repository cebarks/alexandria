//! Owner-facing refinement queue, proposal review, and destination-consent settings.
//!
//! Queue/detail reads are account- and workspace-qualified in storage. Worker actions remain
//! fail-closed until Task 19 supplies their durable attempt and application services.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::Value;

use super::html::{self, page};
use super::web::{self, Boundary, BrowserSession};
use crate::AlexandriaServer;
use crate::error::ServiceError;
use crate::refinement::service::RefinementService;
use alexandria_storage::Database;
use alexandria_storage::access::OwnerContext;
use alexandria_storage::models::mutation::OperationDetail;
use alexandria_storage::models::refinement::{
    ApprovalMode, ApprovalSetting, Consent, ConsentChoice, PolicyVersion, ProposalDisplay,
};
use alexandria_storage::repos::MutationRepo;

const NAV: &str = "refinement";

#[derive(Deserialize, Default)]
pub(crate) struct QueueFilter {
    scope: Option<String>,
}

pub(crate) async fn queue(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Query(filter): Query<QueueFilter>,
) -> Response {
    let Ok((owner, _)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    let filter_scope = filter.scope.unwrap_or_else(|| "all".to_string());
    let scope = match filter_scope.as_str() {
        "all" => None,
        "personal" | "project" | "unassigned" => Some(filter_scope.as_str()),
        _ => return bad_request("unknown refinement scope filter"),
    };
    let service = RefinementService::new(server.db.as_ref(), None);
    let proposals = match service.owner_proposals(&owner, scope).await {
        Ok(proposals) => proposals,
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
        attempt_status: "attempt and quota data unavailable until the worker service is integrated"
            .to_string(),
        filter_scope,
        rows,
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
    let service = RefinementService::new(server.db.as_ref(), None);
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
    match effective_policy(Arc::clone(&server.db), owner).await {
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
            private_page(SettingsTemplate {
                nav: NAV,
                csrf: session.csrf().to_string(),
                consent: consent_label(&effective),
                destination: destination_disclosure(&effective),
                approval_mode: approval_mode_label(effective.approval_mode).to_string(),
                quota_status: "Unavailable: worker quota integration is pending".to_string(),
                attempt_status: "Unavailable: attempt polling is pending".to_string(),
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
    match set_consent(Arc::clone(&server.db), owner, form.revision, choice).await {
        Ok(_) => axum::response::Redirect::to("/debug/refinement/settings").into_response(),
        Err(_) => unavailable(),
    }
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
    match set_mode(Arc::clone(&server.db), owner, expected, mode).await {
        Ok(_) => axum::response::Redirect::to("/debug/refinement/settings").into_response(),
        Err(_) => unavailable(),
    }
}

async fn effective_policy(
    db: Arc<Database>,
    owner: OwnerContext,
) -> Result<crate::refinement::service::EffectiveRefinementPolicy, ServiceError> {
    let service = RefinementService::new(db.as_ref(), None);
    service.effective_policy(&owner).await
}

async fn set_consent(
    db: Arc<Database>,
    owner: OwnerContext,
    revision: u64,
    choice: ConsentChoice,
) -> Result<Consent, ServiceError> {
    let service = RefinementService::new(db.as_ref(), None);
    service.consent(&owner, revision, choice).await
}

async fn set_mode(
    db: Arc<Database>,
    owner: OwnerContext,
    expected: PolicyVersion,
    mode: ApprovalMode,
) -> Result<ApprovalSetting, ServiceError> {
    let service = RefinementService::new(db.as_ref(), None);
    service.set_approval_mode(&owner, &expected, mode).await
}

// Task 19 supplies durable enqueue/attempt/proposal-action services. These routes remain
// deliberately unavailable rather than implying work or mutating a proposal without those guards.
pub(crate) async fn enqueue() -> Response {
    unavailable()
}
pub(crate) async fn approve() -> Response {
    unavailable()
}
pub(crate) async fn dismiss() -> Response {
    unavailable()
}
pub(crate) async fn defer() -> Response {
    unavailable()
}
pub(crate) async fn revert() -> Response {
    unavailable()
}
pub(crate) async fn attempt() -> Response {
    unavailable()
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
    rows: Vec<QueueRow>,
}

struct QueueRow {
    id: String,
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
    revision: u64,
    state: String,
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
