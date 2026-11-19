//! The fair refinement worker: one in-flight provider call, round-robin across accounts,
//! partitions and work classes, every charge and authority check at the edge it protects.
//!
//! Primary calls yield typed proposals; owner-requested opinions yield typed verdicts.
//! With the local embedding provider attached, the worker publishes human-review proposals
//! and sends exact automatic verdicts to the transactional refinement service. Without it,
//! callers can examine an opaque candidate, but dispatch never commits it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alexandria_engine::refinement::budget;
use alexandria_engine::refinement::proposal::ValidatedProposal;
use alexandria_engine::refinement::validation::validate_proposal;
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::Database;
use alexandria_storage::access::{AccessRepo, OwnerContext, SystemContext};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::identity::{
    AccountId, OperationId, Partition, ReadSelection, Version,
};
use alexandria_storage::models::refinement::{
    ApprovalMode, Authority, EvidenceBundle, ProposalRevision, ProposalState, QualityOperation,
    SecondaryVerdict,
};
use alexandria_storage::record_id_to_string;
use alexandria_storage::repos::RefinementRepo;
use alexandria_storage::repos::refinement_repo::{
    AttemptId, AttemptState, BudgetRepo, CallLimits, CandidateCheckpoint, CandidateCursor,
    EvidenceOutcome, PassId, QueuedWork, Reservation, WorkKind, WorkSelection, admission,
    current_authority,
};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;
use surrealdb::types::RecordId;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use super::inference::{AttemptQuota, InferenceError, InferenceReply, QuotaDenied};
use super::service::{ReconciledRefinement, RefinementService};
use crate::error::ServiceError;

/// How often an in-flight call's authority is re-checked while the provider works. Bounded by
/// the request deadline; a revocation or suspension must stop a supported call, not wait for
/// the provider to finish on its own.
const AUTHORITY_POLL_MS: u64 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerSettings {
    pub calls: CallLimits,
    pub max_focals_per_pass: usize,
    pub max_neighbors_per_focal: usize,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_output_tokens: u32,
    pub request_deadline_secs: u64,
    pub pass_active_secs: u64,
}

impl Default for WorkerSettings {
    fn default() -> Self {
        Self {
            calls: CallLimits {
                pass: budget::MAX_ATTEMPTS_PER_PASS,
                account: budget::MAX_ATTEMPTS_PER_ACCOUNT,
                interactive: budget::MAX_INTERACTIVE_ATTEMPTS_PER_ACCOUNT,
                global: budget::MAX_GLOBAL_ATTEMPTS,
            },
            max_focals_per_pass: budget::MAX_FOCALS_PER_PASS,
            max_neighbors_per_focal: budget::MAX_NEIGHBORS_PER_FOCAL,
            max_request_bytes: budget::MAX_REQUEST_BYTES,
            max_response_bytes: budget::MAX_RESPONSE_BYTES,
            max_output_tokens: budget::MAX_OUTPUT_TOKENS,
            request_deadline_secs: budget::REQUEST_DEADLINE_SECS,
            pass_active_secs: budget::PASS_ACTIVE_SECS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Primary,
    Secondary,
}

pub struct Call<'a> {
    pub kind: WorkKind,
    pub stage: Stage,
    pub evidence: &'a EvidenceBundle,
    pub proposal: Option<&'a ProposalRevision>,
}

/// A reservation charges the origin budgets but does not entitle a provider to send data.
/// Authority is checked again at the egress edge, on every physical call, including retries.
#[async_trait::async_trait]
pub trait WorkerQuota: AttemptQuota {
    async fn authorize_outbound(&self) -> Result<(), QuotaDenied>;
}

#[async_trait::async_trait]
pub trait WorkerProvider: Send + Sync {
    async fn complete(
        &self,
        call: Call<'_>,
        quota: &dyn WorkerQuota,
        cancel: CancellationToken,
    ) -> Result<InferenceReply, InferenceError>;
}

/// An exact secondary approval under an opted-in automatic mode, bound to the worker
/// generation and the authority that produced it. Only the transactional consumer may
/// spend it, and only while both bindings are current.
#[derive(Debug)]
pub struct AutomaticCandidate {
    pub(super) generation: u64,
    pub(super) authority: Authority,
    pub(super) proposal: ProposalRevision,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // A typed proposal is the common completion.
pub enum WorkerOutput {
    Proposal(ProposalRevision),
    Opinion(SecondaryVerdict),
    Automatic(AutomaticCandidate),
    Applied(RecordId),
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Dispatch {
    Idle,
    Busy,
    Denied,
    Completed {
        attempt: AttemptId,
        pass: PassId,
        output: WorkerOutput,
    },
}

/// The outcome of one dispatched work item. The completed payload is boxed: one typed
/// proposal dwarfs the refusal it sits beside, and `Turn` moves through every stage of a
/// dispatch on its way to the caller.
enum Turn {
    /// A typed result ready for review or transactional application.
    Completed(Box<WorkerOutput>),
    /// No call produced a result. The attempt is either still queued (a spent budget, with
    /// progress preserved) or already settled (withdrawn authority, a failed call).
    Denied,
}

/// One dispatched work item's threaded state: everything the work kinds, the assessment
/// stages and the provider calls below [`Worker::dispatch`] need. Grouped so the chain
/// carries one reference instead of five separate arguments, the same grouping the cluster
/// and reminder lanes settled on for their thread-through state.
struct DispatchCtx<'a> {
    repo: &'a RefinementRepo<'a>,
    work: &'a QueuedWork,
    expected: &'a alexandria_storage::models::refinement::InferencePolicyId,
    now: DateTime<Utc>,
    provider: &'a dyn WorkerProvider,
}

pub struct Worker<'a> {
    db: &'a Database,
    system: &'a SystemContext,
    configured: &'a ReconciledRefinement,
    settings: WorkerSettings,
    embedding: Option<&'a dyn EmbeddingProvider>,
    /// One provider call at a time, process-wide. The claim on the attempt row is the durable
    /// half; this is the in-memory half that answers a concurrent dispatch with `Busy`.
    in_flight: AtomicBool,
    /// The last served (account, partition, class). The next dispatch picks the first queued
    /// unit that is not this one, which is the whole fairness discipline.
    last_served: Mutex<Option<(AccountId, Partition, bool)>>,
}

impl<'a> Worker<'a> {
    pub fn new(
        db: &'a Database,
        system: &'a SystemContext,
        configured: &'a ReconciledRefinement,
        settings: WorkerSettings,
    ) -> Self {
        Self {
            db,
            system,
            configured,
            settings,
            embedding: None,
            in_flight: AtomicBool::new(false),
            last_served: Mutex::new(None),
        }
    }

    /// Enable durable publication and transactional application for this runtime.
    pub fn with_embedding(mut self, embedding: &'a dyn EmbeddingProvider) -> Self {
        self.embedding = Some(embedding);
        self
    }

    /// Run at most one fair turn: select the next eligible queued unit, lease it, and make at
    /// most one provider call (plus its retries and secondary, all charged to the origin
    /// pass). Returns `Busy` while a call is in flight.
    pub async fn step(
        &self,
        now: DateTime<Utc>,
        provider: &dyn WorkerProvider,
    ) -> Result<Dispatch, ServiceError> {
        if self.in_flight.swap(true, Ordering::SeqCst) {
            return Ok(Dispatch::Busy);
        }
        let dispatch = self.dispatch(now, provider).await;
        self.in_flight.store(false, Ordering::SeqCst);
        dispatch
    }

    /// Run one explicit, assigned assessment turn. No other account, partition or work class
    /// can be selected, even if this attempt is no longer runnable.
    pub async fn step_assigned_attempt(
        &self,
        owner: OwnerContext,
        partition: Partition,
        attempt: AttemptId,
        now: DateTime<Utc>,
        provider: &dyn WorkerProvider,
    ) -> Result<Dispatch, ServiceError> {
        if self.in_flight.swap(true, Ordering::SeqCst) {
            return Ok(Dispatch::Busy);
        }
        let result = self
            .dispatch_assigned(&owner, &partition, &attempt, now, provider)
            .await;
        self.in_flight.store(false, Ordering::SeqCst);
        result
    }

    async fn dispatch_assigned(
        &self,
        owner: &OwnerContext,
        partition: &Partition,
        attempt: &AttemptId,
        now: DateTime<Utc>,
        provider: &dyn WorkerProvider,
    ) -> Result<Dispatch, ServiceError> {
        let repo = RefinementRepo::new(self.db);
        let Some(work) = repo
            .queued_assessment_at(self.system, owner, partition, attempt)
            .await?
        else {
            return Ok(Dispatch::Idle);
        };
        if !matches!(&work.selection, WorkSelection::Scheduled) {
            return Err(ServiceError::InvalidInput(
                "expected an assigned scheduled assessment".into(),
            ));
        }
        let expected = self.expected_policy();
        let ctx = DispatchCtx {
            repo: &repo,
            work: &work,
            expected: &expected,
            now,
            provider,
        };
        if !self.claim_dispatch(&ctx).await? {
            return Ok(Dispatch::Denied);
        }
        let result = run_assigned_turn(self, &ctx, owner.clone()).await;
        if result.is_err() {
            // No supervisor is waiting to settle this claim. Preserve the original error and
            // never overwrite a state the work already settled.
            let _ = repo
                .finish_work(self.system, &work.id, AttemptState::Failed)
                .await;
        }
        result
    }

    /// Whether an automatic-application candidate may still be spent: its worker generation
    /// is the current one and the authority that produced it is still the account's binding
    /// authority. A replaced policy (even an A→B→A return) or a rolled generation refuses.
    pub async fn authorize_application(
        &self,
        candidate: &AutomaticCandidate,
    ) -> Result<bool, ServiceError> {
        let repo = RefinementRepo::new(self.db);
        if repo.worker_generation(self.system).await? != candidate.generation {
            return Ok(false);
        }
        let current = current_authority(self.db, &candidate.authority.account).await?;
        Ok(current.is_some_and(|current| {
            current.account == candidate.authority.account
                && current.consent_revision == candidate.authority.consent_revision
                && current.owner_mode_revision == candidate.authority.owner_mode_revision
                && current.configured_revision == candidate.authority.configured_revision
                && current.inference_policy_id == candidate.authority.inference_policy_id
        }))
    }

    /// Invalidate the current worker generation and return interrupted attempts to the queue.
    /// Late responses from the superseded generation can no longer authorize application.
    pub async fn recover_interrupted(&self, _now: DateTime<Utc>) -> Result<usize, ServiceError> {
        let repo = RefinementRepo::new(self.db);
        repo.bump_worker_generation(self.system).await?;
        repo.requeue_interrupted(self.system)
            .await
            .map_err(ServiceError::from)
    }

    fn expected_policy(&self) -> alexandria_storage::models::refinement::InferencePolicyId {
        self.configured
            .policy()
            .configuration
            .inference_policy_id
            .clone()
    }

    async fn dispatch(
        &self,
        now: DateTime<Utc>,
        provider: &dyn WorkerProvider,
    ) -> Result<Dispatch, ServiceError> {
        let repo = RefinementRepo::new(self.db);
        let expected = self.expected_policy();
        let mut queue = repo.queued_work(self.system).await?;
        if queue.is_empty() {
            return Ok(Dispatch::Idle);
        }
        let last = self
            .last_served
            .lock()
            .expect("refinement fairness cursor")
            .clone();
        let start = queue
            .iter()
            .position(|work| Some(work.fairness_key()) != last)
            .unwrap_or(0);
        queue.rotate_left(start);
        for work in queue {
            let ctx = DispatchCtx {
                repo: &repo,
                work: &work,
                expected: &expected,
                now,
                provider,
            };
            if self.claim_dispatch(&ctx).await? {
                let result = self.run_work(&ctx).await;
                if result.is_err() {
                    let _ = repo
                        .finish_work(self.system, &work.id, AttemptState::Failed)
                        .await;
                }
                self.last_served
                    .lock()
                    .expect("refinement fairness cursor")
                    .replace(work.fairness_key());
                return result;
            }
        }
        Ok(Dispatch::Idle)
    }

    async fn claim_dispatch(&self, ctx: &DispatchCtx<'_>) -> Result<bool, ServiceError> {
        // The targeted offline entry and the fair supervisor share the same admission and claim.
        let work = ctx.work;
        if admission(self.db, &work.account, ctx.expected)
            .await
            .is_err()
            || !authority_is_current(self.db, &work.authority).await?
        {
            ctx.repo.cancel_queued(self.system, &work.id).await?;
            return Ok(false);
        }
        Ok(ctx.repo.claim_work(self.system, &work.id).await?)
    }

    async fn run_work(&self, ctx: &DispatchCtx<'_>) -> Result<Dispatch, ServiceError> {
        match ctx.work.kind {
            WorkKind::Assess => self.run_scheduled(ctx).await,
            WorkKind::AssessUnassigned => self.run_unassigned(ctx).await,
            WorkKind::Apply => Err(ServiceError::Internal(
                "application attempt cannot dispatch",
            )),
            WorkKind::Opinion | WorkKind::Regenerate => self.run_review(ctx).await,
        }
    }

    /// Scheduled assessment of an assigned partition: one real Task 18 projection per turn,
    /// with the durable candidate cursor carried across partial passes, restarts and budget
    /// exhaustion. The attempt completes when the walk is exhausted or yields nothing.
    async fn run_scheduled(&self, ctx: &DispatchCtx<'_>) -> Result<Dispatch, ServiceError> {
        let repo = ctx.repo;
        let work = ctx.work;
        let (checkpoint, next, turn) = self
            .scheduled_turn(
                ctx,
                if self.embedding.is_some() {
                    1
                } else {
                    self.settings.max_focals_per_pass
                },
            )
            .await?;
        let turn = match turn {
            Some(Turn::Completed(output)) if self.embedding.is_some() => {
                let owner = self.owner_context(&work.account).await?;
                match self.publish_assessment(ctx, owner, *output).await {
                    Ok(output) => Some(Turn::Completed(Box::new(output))),
                    Err(ServiceError::Stale) => {
                        repo.finish_work(self.system, &work.id, AttemptState::Cancelled)
                            .await?;
                        return Ok(Dispatch::Denied);
                    }
                    Err(error) => return Err(error),
                }
            }
            turn => turn,
        };
        if self.embedding.is_some() && matches!(&turn, Some(Turn::Denied)) {
            return Ok(Dispatch::Denied);
        }
        // Progress is preserved whatever happened: quota exhaustion leaves the walk where it
        // stopped, and an exhausted walk closes the checkpoint for the next cycle. A `Stale`
        // from this save is not a cursor conflict — with one in-flight worker there is no
        // other writer — but the partition's admission being withdrawn mid-pass: the account
        // was suspended between the projection and this write, and `assigned_account` refuses
        // it. The dispatch's own outcome already carries the answer (the attempt was settled
        // by the refusal path), so the walk simply restarts from the last acknowledged
        // position once the account is admitted again, rather than the suspension being
        // reported as a stale-bookkeeping error.
        if let Err(error) = repo
            .save_candidate_checkpoint(
                self.system,
                &work.partition,
                checkpoint.revision,
                next.as_ref(),
            )
            .await
        {
            if matches!(error, StorageError::Stale) {
                tracing::warn!(
                    "refinement checkpoint save refused: partition admission withdrawn mid-pass"
                );
            } else {
                return Err(error.into());
            }
        }
        match turn {
            Some(Turn::Denied) => Ok(Dispatch::Denied),
            Some(Turn::Completed(output)) => {
                if next.is_none() {
                    repo.finish_work(self.system, &work.id, AttemptState::Completed)
                        .await?;
                } else if self.embedding.is_some() {
                    repo.requeue_work(self.system, &work.id).await?;
                }
                Ok(Dispatch::Completed {
                    attempt: work.id.clone(),
                    pass: pass_id(work),
                    output: *output,
                })
            }
            None => {
                if self.embedding.is_some() && next.is_some() {
                    repo.requeue_work(self.system, &work.id).await?;
                } else {
                    repo.finish_work(self.system, &work.id, AttemptState::Completed)
                        .await?;
                }
                Ok(Dispatch::Idle)
            }
        }
    }

    async fn scheduled_turn(
        &self,
        ctx: &DispatchCtx<'_>,
        focal_limit: usize,
    ) -> Result<(CandidateCheckpoint, Option<CandidateCursor>, Option<Turn>), ServiceError> {
        let partition = &ctx.work.partition;
        let checkpoint = ctx
            .repo
            .candidate_checkpoint(self.system, partition)
            .await?;
        let eligible_before = ctx.now - TimeDelta::seconds(budget::REASSESS_AFTER_SECS as i64);
        let page = ctx
            .repo
            .page_candidates(
                partition,
                checkpoint.position.as_ref(),
                focal_limit,
                eligible_before,
            )
            .await?;
        let turn = match page
            .outcomes
            .iter()
            .find(|outcome| matches!(outcome, EvidenceOutcome::Ready(_)))
        {
            Some(EvidenceOutcome::Ready(bundle)) => {
                Some(self.assess_bundle(ctx, bundle, true).await?)
            }
            _ => None,
        };
        Ok((checkpoint, page.next, turn))
    }

    async fn publish_assessment(
        &self,
        ctx: &DispatchCtx<'_>,
        owner: OwnerContext,
        output: WorkerOutput,
    ) -> Result<WorkerOutput, ServiceError> {
        if !authority_is_current(self.db, &ctx.work.authority).await? {
            return Err(ServiceError::Stale);
        }
        let proposal = match output {
            WorkerOutput::Proposal(proposal) => proposal,
            WorkerOutput::Automatic(candidate) => {
                let embedding = self.embedding.ok_or(ServiceError::Unavailable)?;
                let operation = OperationId::new(format!(
                    "secondary-{}",
                    record_id_to_string(&candidate.proposal.id)
                ));
                let service = RefinementService::new(self.db, Some(self.configured))
                    .with_embedding(embedding);
                service
                    .apply_secondary(self.system, &candidate, &operation)
                    .await?;
                return Ok(WorkerOutput::Applied(candidate.proposal.id));
            }
            WorkerOutput::Opinion(_) | WorkerOutput::Applied(_) => {
                return Err(ServiceError::Internal("offline assessment output"));
            }
        };
        // Durably publish before acknowledging the cursor. Persistence coalesces an identical
        // evidence replay if the process dies between these two steps.
        Ok(WorkerOutput::Proposal(
            ctx.repo.persist_proposal(&owner, proposal).await?,
        ))
    }

    /// Owner-selected Unassigned evidence, read through the same owner-review door a human
    /// review uses, then assessed once. Unassigned work never enters the automatic path.
    async fn run_unassigned(&self, ctx: &DispatchCtx<'_>) -> Result<Dispatch, ServiceError> {
        let repo = ctx.repo;
        let work = ctx.work;
        let selection = match &work.selection {
            WorkSelection::Evidence(selection) => selection.clone(),
            _ => {
                repo.finish_work(self.system, &work.id, AttemptState::Failed)
                    .await?;
                return Ok(Dispatch::Idle);
            }
        };
        let owner = self.owner_context(&work.account).await?;
        let access = AccessRepo::new(self.db);
        let review = access
            .select(&owner, ReadSelection::UnassignedReview)
            .await?;
        let outcome = repo
            .evidence(
                (&owner, &review),
                &selection,
                self.settings.max_neighbors_per_focal,
                self.settings.max_request_bytes,
            )
            .await?;
        match outcome {
            EvidenceOutcome::Ready(bundle) => {
                let turn = self.assess_bundle(ctx, &bundle, false).await?;
                match turn {
                    Turn::Completed(output) => {
                        let output = if self.embedding.is_some() {
                            self.publish_assessment(ctx, owner, *output).await?
                        } else {
                            *output
                        };
                        repo.finish_work(self.system, &work.id, AttemptState::Completed)
                            .await?;
                        Ok(Dispatch::Completed {
                            attempt: work.id.clone(),
                            pass: pass_id(work),
                            output,
                        })
                    }
                    Turn::Denied => Ok(Dispatch::Denied),
                }
            }
            EvidenceOutcome::NeedsContext { .. } => {
                repo.finish_work(self.system, &work.id, AttemptState::Completed)
                    .await?;
                Ok(Dispatch::Idle)
            }
        }
    }

    /// An owner-requested opinion or regeneration of one stored proposal revision. Opinions
    /// review; regenerations re-propose. Neither gains automatic authority by itself.
    async fn run_review(&self, ctx: &DispatchCtx<'_>) -> Result<Dispatch, ServiceError> {
        let repo = ctx.repo;
        let work = ctx.work;
        let proposal_id = match &work.selection {
            WorkSelection::Proposal(proposal) => proposal.clone(),
            _ => {
                repo.finish_work(self.system, &work.id, AttemptState::Failed)
                    .await?;
                return Ok(Dispatch::Idle);
            }
        };
        let (proposal, evidence) = repo
            .proposal_with_evidence(self.system, &proposal_id)
            .await?;
        let sources = if work.kind == WorkKind::Regenerate {
            repo.capture_sources(self.system, &evidence).await?
        } else {
            Vec::new()
        };
        let stage = if work.kind == WorkKind::Opinion {
            Stage::Secondary
        } else {
            Stage::Primary
        };
        let call = Call {
            kind: work.kind,
            stage,
            evidence: &evidence,
            proposal: Some(&proposal),
        };
        let reply = self.provider_call(ctx, call).await?;
        match reply {
            Ok(reply) => {
                let output = if work.kind == WorkKind::Opinion {
                    match parse_verdict(&reply.text) {
                        Ok(verdict) => WorkerOutput::Opinion(verdict),
                        Err(_) => {
                            repo.finish_work(self.system, &work.id, AttemptState::Failed)
                                .await?;
                            return Ok(Dispatch::Denied);
                        }
                    }
                } else {
                    match validate_proposal(&evidence, &reply.text) {
                        Ok(validated) => WorkerOutput::Proposal(proposal_from(
                            work, &evidence, sources, validated,
                        )),
                        Err(_) => {
                            repo.finish_work(self.system, &work.id, AttemptState::Failed)
                                .await?;
                            return Ok(Dispatch::Denied);
                        }
                    }
                };
                repo.finish_work(self.system, &work.id, AttemptState::Completed)
                    .await?;
                Ok(Dispatch::Completed {
                    attempt: work.id.clone(),
                    pass: pass_id(work),
                    output,
                })
            }
            Err(error) => {
                self.provider_failed(ctx, error).await?;
                Ok(Dispatch::Denied)
            }
        }
    }

    /// One assessment of one real evidence bundle: a primary call producing a strictly
    /// validated typed proposal, and — only for scheduled assigned work under an opted-in
    /// automatic mode — a secondary reviewer whose exact approval becomes an application
    /// candidate rather than an application.
    async fn assess_bundle(
        &self,
        ctx: &DispatchCtx<'_>,
        evidence: &EvidenceBundle,
        automatic_allowed: bool,
    ) -> Result<Turn, ServiceError> {
        let repo = ctx.repo;
        let work = ctx.work;
        let generation = repo.worker_generation(self.system).await?;
        let sources = repo.capture_sources(self.system, evidence).await?;
        let reply = self
            .provider_call(
                ctx,
                Call {
                    kind: work.kind,
                    stage: Stage::Primary,
                    evidence,
                    proposal: None,
                },
            )
            .await?;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => return self.provider_failed(ctx, error).await,
        };
        let validated = match validate_proposal(evidence, &reply.text) {
            Ok(validated) => validated,
            Err(_) => {
                repo.finish_work(self.system, &work.id, AttemptState::Failed)
                    .await?;
                return Ok(Turn::Denied);
            }
        };
        let proposal = proposal_from(work, evidence, sources, validated);
        if automatic_allowed
            && work.authority.mode == ApprovalMode::SecondaryLlm
            && !matches!(
                &proposal.proposed,
                QualityOperation::Contradiction { .. } | QualityOperation::NoSafeChange
            )
        {
            let reply = self
                .provider_call(
                    ctx,
                    Call {
                        kind: work.kind,
                        stage: Stage::Secondary,
                        evidence,
                        proposal: Some(&proposal),
                    },
                )
                .await?;
            match reply {
                Ok(reply) => {
                    let verdict = match parse_verdict(&reply.text) {
                        Ok(verdict) => verdict,
                        Err(_) => {
                            repo.finish_work(self.system, &work.id, AttemptState::Failed)
                                .await?;
                            return Ok(Turn::Denied);
                        }
                    };
                    if verdict == SecondaryVerdict::ApproveExact {
                        if repo.worker_generation(self.system).await? != generation {
                            return Err(ServiceError::Stale);
                        }
                        return Ok(Turn::Completed(Box::new(WorkerOutput::Automatic(
                            AutomaticCandidate {
                                generation,
                                authority: work.authority.clone(),
                                proposal,
                            },
                        ))));
                    }
                    Ok(Turn::Completed(Box::new(WorkerOutput::Proposal(proposal))))
                }
                Err(error) => self.provider_failed(ctx, error).await,
            }
        } else {
            Ok(Turn::Completed(Box::new(WorkerOutput::Proposal(proposal))))
        }
    }

    /// Run one provider call with the authority watcher beside it. A revocation, suspension
    /// or policy replacement that lands mid-call cancels the call instead of waiting for the
    /// provider's own deadline.
    async fn provider_call(
        &self,
        ctx: &DispatchCtx<'_>,
        call: Call<'_>,
    ) -> Result<Result<InferenceReply, InferenceError>, ServiceError> {
        let quota = StepQuota::new(
            self.db,
            self.settings.calls,
            ctx.expected.clone(),
            ctx.work,
            pass_id(ctx.work),
            ctx.now,
        );
        let cancel = CancellationToken::new();
        let account = ctx.work.account.clone();
        let policy = ctx.expected.clone();
        let db = self.db;
        let done = tokio::sync::Notify::new();
        let watch = async {
            loop {
                tokio::select! {
                    _ = done.notified() => break,
                    _ = sleep(Duration::from_millis(AUTHORITY_POLL_MS)) => {},
                }
                if admission(db, &account, &policy).await.is_err() {
                    cancel.cancel();
                    break;
                }
            }
        };
        let reply = tokio::join!(
            async {
                let reply = ctx.provider.complete(call, &quota, cancel.clone()).await;
                done.notify_one();
                reply
            },
            watch
        )
        .0;
        Ok(reply)
    }

    /// Settle a failed call. A quota refusal with intact authority is a spent budget: the
    /// work returns to the queue with its progress, for a later eligible pass. Withdrawn
    /// authority cancels the attempt; anything else fails it.
    async fn provider_failed(
        &self,
        ctx: &DispatchCtx<'_>,
        error: InferenceError,
    ) -> Result<Turn, ServiceError> {
        let repo = ctx.repo;
        let work = ctx.work;
        match error {
            InferenceError::QuotaDenied | InferenceError::Cancelled => {
                if admission(self.db, &work.account, ctx.expected)
                    .await
                    .is_err()
                {
                    repo.finish_work(self.system, &work.id, AttemptState::Cancelled)
                        .await?;
                } else {
                    repo.requeue_work(self.system, &work.id).await?;
                }
            }
            _ => {
                repo.finish_work(self.system, &work.id, AttemptState::Failed)
                    .await?;
            }
        }
        Ok(Turn::Denied)
    }

    /// The owner context the worker acts under for owner-scoped reads. Minted through the
    /// same local-principal door the trusted-local path uses, after the active check.
    async fn owner_context(&self, account: &AccountId) -> Result<OwnerContext, ServiceError> {
        let access = AccessRepo::new(self.db);
        let principal = access
            .local_principal(self.system, account)
            .await
            .map_err(ServiceError::from)?;
        access
            .authorize(&principal)
            .await
            .map_err(ServiceError::from)
    }
}

// Keep the one-shot publication future outside the future used by the fair worker.
async fn run_assigned_turn(
    worker: &Worker<'_>,
    ctx: &DispatchCtx<'_>,
    owner: OwnerContext,
) -> Result<Dispatch, ServiceError> {
    // A one-focal page cannot acknowledge a record this turn never examined.
    let (checkpoint, next, turn) = worker.scheduled_turn(ctx, 1).await?;
    let work = ctx.work;
    let output = match turn {
        Some(Turn::Completed(reply)) => Some(worker.publish_assessment(ctx, owner, *reply).await?),
        Some(Turn::Denied) => return Ok(Dispatch::Denied),
        None => None,
    };
    ctx.repo
        .save_candidate_checkpoint(
            worker.system,
            &work.partition,
            checkpoint.revision,
            next.as_ref(),
        )
        .await?;
    ctx.repo
        .finish_work(worker.system, &work.id, AttemptState::Completed)
        .await?;
    Ok(match output {
        Some(output) => Dispatch::Completed {
            attempt: work.id.clone(),
            pass: pass_id(work),
            output,
        },
        None => Dispatch::Idle,
    })
}

fn pass_id(work: &QueuedWork) -> PassId {
    PassId(format!("attempt:{}", record_id_to_string(work.id.record())))
}

async fn authority_is_current(db: &Database, authority: &Authority) -> Result<bool, ServiceError> {
    let current = current_authority(db, &authority.account).await?;
    Ok(current.is_some_and(|current| {
        current.account == authority.account
            && current.consent_revision == authority.consent_revision
            && current.owner_mode_revision == authority.owner_mode_revision
            && current.configured_revision == authority.configured_revision
            && current.inference_policy_id == authority.inference_policy_id
    }))
}

fn proposal_from(
    work: &QueuedWork,
    evidence: &EvidenceBundle,
    sources: Vec<alexandria_storage::models::mutation::ExpectedState>,
    validated: ValidatedProposal,
) -> ProposalRevision {
    ProposalRevision {
        id: RecordId::new(
            "refinement_proposal",
            format!("worker-{}", uuid::Uuid::new_v4()),
        ),
        revision: Version::new(1),
        partition: evidence.focal.partition.clone(),
        sources,
        evidence: evidence.clone(),
        category: validated.category,
        proposed: validated.operation,
        approved_relationship_effects: validated.relationship_effects,
        rationale: validated.rationale,
        uncertainty: validated.uncertainty,
        origin_policy: work.authority.clone(),
        state: ProposalState::Pending,
        needs_human: true,
        deferred: false,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerdictOutput {
    verdict: SecondaryVerdict,
    /// Required present by `deny_unknown_fields` so a reviewer reply without its reasoning is
    /// refused; the worker's contract is the verdict, not the text.
    #[allow(dead_code)]
    rationale: String,
}

/// Parse exactly one JSON verdict document, optionally in one `json` fence, mirroring the
/// strictness of the primary proposal parser.
fn parse_verdict(text: &str) -> Result<SecondaryVerdict, ServiceError> {
    let trimmed = text.trim();
    let json = if let Some(body) = trimmed.strip_prefix("```json\n") {
        body.strip_suffix("\n```").ok_or_else(|| {
            ServiceError::InvalidInput("reviewer verdict fence is unterminated".into())
        })?
    } else if trimmed.starts_with("```") {
        return Err(ServiceError::InvalidInput(
            "reviewer verdict is not one JSON document".into(),
        ));
    } else {
        trimmed
    };
    let parsed: VerdictOutput = serde_json::from_str(json)
        .map_err(|_| ServiceError::InvalidInput("reviewer verdict is not valid".into()))?;
    Ok(parsed.verdict)
}

/// The quota one dispatched work item offers its provider: every physical attempt reserves
/// against the origin pass, account and global budgets, and every send re-authorizes the
/// account, consent and policy at the egress edge.
struct StepQuota<'a> {
    db: &'a Database,
    repo: BudgetRepo<'a>,
    policy: alexandria_storage::models::refinement::InferencePolicyId,
    account: AccountId,
    partition: Partition,
    pass: PassId,
    interactive: bool,
    now: DateTime<Utc>,
}

impl<'a> StepQuota<'a> {
    fn new(
        db: &'a Database,
        limits: CallLimits,
        policy: alexandria_storage::models::refinement::InferencePolicyId,
        work: &QueuedWork,
        pass: PassId,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            db,
            repo: BudgetRepo::new(db, limits),
            policy,
            account: work.account.clone(),
            partition: work.partition.clone(),
            pass,
            interactive: work.interactive,
            now,
        }
    }
}

#[async_trait::async_trait]
impl AttemptQuota for StepQuota<'_> {
    async fn reserve_attempt(&self) -> Result<(), QuotaDenied> {
        match self
            .repo
            .reserve_attempt(
                &self.policy,
                &self.account,
                &self.partition,
                &self.pass,
                self.interactive,
                self.now,
            )
            .await
        {
            Ok(Reservation::Granted) => Ok(()),
            _ => Err(QuotaDenied),
        }
    }
}

#[async_trait::async_trait]
impl WorkerQuota for StepQuota<'_> {
    async fn authorize_outbound(&self) -> Result<(), QuotaDenied> {
        admission(self.db, &self.account, &self.policy)
            .await
            .map_err(|_| QuotaDenied)
    }
}
