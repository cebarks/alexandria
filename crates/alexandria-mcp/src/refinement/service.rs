use alexandria_engine::refinement::policy::{
    self as policy, InferencePolicyInputs, RouteDisclosure, RoutePair,
};
use alexandria_engine::refinement::proposal::ValidatedProposal;
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::Database;
use alexandria_storage::access::{OwnerContext, SystemContext};
use alexandria_storage::models::identity::{OperationId, SemanticOperationId};
use alexandria_storage::models::mutation::{AppliedChange, EmbeddingSnapshot, PreparedChangeSet};
use alexandria_storage::models::operation_kind;
use alexandria_storage::models::refinement::{
    ApprovalMode, ApprovalSetting, ConfiguredPolicy, Consent, ConsentChoice, EffectivePolicy,
    InferencePolicyId, PolicyRevision, PolicyVersion, PreparedQualityEmbedding, ProposalDisplay,
    ProposalRevision, Protocol, QualityOperation, RouteDestinationId,
};
use alexandria_storage::repos::RefinementRepo;
use alexandria_storage::repos::refinement_repo::{
    AttemptId, AttemptView, EnqueueRequest, OwnerUsage, current_authority,
};
use alexandria_storage::system_config;
use chrono::Utc;
use surrealdb::types::RecordId;

use super::worker::AutomaticCandidate;
use crate::error::ServiceError;

#[cfg(test)]
mod tests;

/// Operator-owned routes paired with the policy revision published at boot.
pub struct ReconciledRefinement {
    policy: PolicyRevision,
    routes: RoutePair,
}

impl ReconciledRefinement {
    /// The published policy revision this runtime serves under. The worker compares it against
    /// the store's current configuration before any selection, reservation or call.
    pub fn policy(&self) -> &PolicyRevision {
        &self.policy
    }

    pub(super) fn routes(&self) -> &RoutePair {
        &self.routes
    }

    pub async fn reconcile(
        db: &Database,
        system: &SystemContext,
        routes: Option<RoutePair>,
        inputs: &InferencePolicyInputs<'_>,
        ceiling: ApprovalMode,
    ) -> Result<Option<Self>, ServiceError> {
        let configuration = routes.as_ref().map(|routes| ConfiguredPolicy {
            destination_set: routes
                .destination_set()
                .into_iter()
                .map(stored_destination)
                .collect(),
            inference_policy_id: InferencePolicyId(
                policy::InferencePolicyId::fingerprint(routes, inputs).0,
            ),
            allow_automatic: ceiling == ApprovalMode::SecondaryLlm,
        });
        let published = RefinementRepo::new(db)
            .reconcile_policy(system, configuration.as_ref())
            .await?;
        match (routes, published) {
            (Some(routes), Some(policy))
                if Some(&policy.configuration) == configuration.as_ref() =>
            {
                Ok(Some(Self { policy, routes }))
            }
            (None, None) => Ok(None),
            _ => Err(ServiceError::Internal("refinement policy reconciliation")),
        }
    }
}

fn stored_destination(destination: policy::RouteDestinationId) -> RouteDestinationId {
    RouteDestinationId {
        protocol: match destination.protocol {
            policy::Protocol::OpenAiChatCompletions => Protocol::OpenAiChatCompletions,
            policy::Protocol::AnthropicMessages => Protocol::AnthropicMessages,
        },
        scheme: destination.scheme,
        host: destination.host,
        effective_port: destination.effective_port,
        api_prefix: destination.api_prefix,
    }
}

/// The owner snapshot and its disclosure, without credential material or a network call.
#[derive(Debug)]
pub struct EffectiveRefinementPolicy {
    pub policy: EffectivePolicy,
    pub routes: Option<RouteDisclosure>,
    pub approval_mode: ApprovalMode,
}

pub struct RefinementService<'a> {
    db: &'a Database,
    repo: RefinementRepo<'a>,
    configured: Option<&'a ReconciledRefinement>,
    embedding: Option<&'a dyn EmbeddingProvider>,
}

/// Prepare the proposed content's vectors before an application transaction is opened.
pub async fn prepare_change(
    proposal: &ValidatedProposal,
    embedding: &dyn EmbeddingProvider,
) -> Result<PreparedChangeSet, ServiceError> {
    let mut change = PreparedChangeSet::new(operation_kind::QUALITY);
    let texts = match &proposal.operation {
        QualityOperation::Rewrite { content, .. } => vec![content.clone()],
        QualityOperation::Split { parts, .. } => parts.clone(),
        QualityOperation::Contradiction { .. } | QualityOperation::NoSafeChange => {
            return Err(ServiceError::InvalidInput(
                "an unresolved finding cannot apply".into(),
            ));
        }
        _ => return Ok(change),
    };
    if texts.iter().any(|text| embedding.overflow(text).is_some()) {
        return Err(ServiceError::InvalidInput(
            "proposed content exceeds the locked embedding budget".into(),
        ));
    }
    let inputs: Vec<_> = texts.iter().map(String::as_str).collect();
    let vectors = embedding
        .embed(&inputs)
        .await
        .map_err(|_| ServiceError::Internal("refinement embedding preparation"))?;
    let dimensions = embedding.dimensions();
    if dimensions == 0
        || vectors.len() != texts.len()
        || vectors
            .iter()
            .any(|vector| vector.len() != dimensions || !vector.iter().all(|v| v.is_finite()))
    {
        return Err(ServiceError::Internal(
            "invalid refinement embedding output",
        ));
    }
    change.quality_embeddings = texts
        .into_iter()
        .zip(vectors)
        .map(|(content, vector)| PreparedQualityEmbedding {
            content,
            embedding: EmbeddingSnapshot::new(vector, Some(embedding.model_id())),
        })
        .collect();
    Ok(change)
}

impl<'a> RefinementService<'a> {
    pub fn new(db: &'a Database, configured: Option<&'a ReconciledRefinement>) -> Self {
        Self {
            db,
            repo: RefinementRepo::new(db),
            configured,
            embedding: None,
        }
    }

    /// Supply the local provider for applying an already assessed change, not an inference route.
    pub fn with_embedding(mut self, embedding: &'a dyn EmbeddingProvider) -> Self {
        self.embedding = Some(embedding);
        self
    }

    pub async fn approve(
        &self,
        owner: &OwnerContext,
        proposal_revision: &ProposalRevision,
        operation: &OperationId,
    ) -> Result<AppliedChange, ServiceError> {
        if let Some(applied) = self
            .repo
            .replay_human(owner, proposal_revision, operation)
            .await?
        {
            return Ok(applied);
        }
        let prepared = match self.prepare_for(proposal_revision).await {
            Ok(prepared) => prepared,
            Err(error) => {
                self.repo
                    .record_failed_application(owner, proposal_revision, operation)
                    .await?;
                return Err(error);
            }
        };
        Ok(self
            .repo
            .apply_human(owner, proposal_revision, operation, &prepared)
            .await?)
    }

    /// The opaque exact verdict binds its partition, policy and worker generation. A worker
    /// additionally needs instance authority; this entry point does not accept owner approval.
    pub async fn apply_secondary(
        &self,
        system: &SystemContext,
        verdict_revision: &AutomaticCandidate,
        operation: &OperationId,
    ) -> Result<AppliedChange, ServiceError> {
        let generation = self.repo.worker_generation(system).await?;
        let current = current_authority(self.db, &verdict_revision.authority.account).await?;
        if generation != verdict_revision.generation
            || current.as_ref() != Some(&verdict_revision.authority)
            || verdict_revision.authority.mode != ApprovalMode::SecondaryLlm
        {
            return Err(ServiceError::Stale);
        }
        let proposal = &verdict_revision.proposal;
        let prepared = match self.prepare_for(proposal).await {
            Ok(prepared) => prepared,
            Err(error) => {
                self.repo
                    .record_failed_secondary(system, proposal, operation, generation)
                    .await?;
                return Err(error);
            }
        };
        Ok(self
            .repo
            .apply_secondary(system, proposal, generation, operation, &prepared)
            .await?)
    }

    pub async fn revert(
        &self,
        owner: &OwnerContext,
        applied_id: &SemanticOperationId,
        operation: &OperationId,
    ) -> Result<AppliedChange, ServiceError> {
        Ok(self.repo.revert(owner, applied_id, operation).await?)
    }

    async fn prepare_for(
        &self,
        proposal: &ProposalRevision,
    ) -> Result<PreparedChangeSet, ServiceError> {
        let provider = self.embedding.ok_or(ServiceError::Unavailable)?;
        let stored = system_config::stored_embedding_space(self.db.inner())
            .await
            .map_err(|_| ServiceError::Internal("refinement embedding space"))?;
        if stored.as_ref().is_some_and(|space| {
            space.model() != provider.model_id() || space.dimensions() != provider.dimensions()
        }) {
            return Err(ServiceError::Stale);
        }
        let validated = ValidatedProposal {
            category: proposal.category,
            operation: proposal.proposed.clone(),
            relationship_effects: proposal.approved_relationship_effects.clone(),
            rationale: proposal.rationale.clone(),
            uncertainty: proposal.uncertainty.clone(),
        };
        let mut prepared = prepare_change(&validated, provider).await?;
        prepared.embedding_space = stored
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| ServiceError::Internal("refinement embedding space"))?;
        Ok(prepared)
    }

    pub async fn enqueue(
        &self,
        owner: &OwnerContext,
        request: EnqueueRequest,
    ) -> Result<AttemptId, ServiceError> {
        self.repo
            .enqueue(owner, request)
            .await
            .map_err(ServiceError::from)
    }

    pub async fn attempt(
        &self,
        owner: &OwnerContext,
        id: &AttemptId,
    ) -> Result<AttemptView, ServiceError> {
        self.repo
            .attempt(owner, id)
            .await
            .map_err(ServiceError::from)
    }

    /// This owner's real rolling charge counts and queued depth, for the disclosure pages.
    pub async fn owner_usage(&self, owner: &OwnerContext) -> Result<OwnerUsage, ServiceError> {
        self.repo
            .owner_usage(owner, Utc::now())
            .await
            .map_err(ServiceError::from)
    }

    /// List proposal revisions visible to this owner, scoped in storage by account and workspace.
    pub async fn owner_proposals(
        &self,
        owner: &OwnerContext,
        scope: Option<&str>,
    ) -> Result<Vec<ProposalDisplay>, ServiceError> {
        self.repo
            .owner_proposals(owner, scope, alexandria_storage::repos::MAX_OWNER_PROPOSALS)
            .await
            .map_err(ServiceError::from)
    }

    /// Resolve a proposal under the same owner/workspace predicate used by the queue.
    pub async fn owner_proposal(
        &self,
        owner: &OwnerContext,
        id: &RecordId,
    ) -> Result<Option<ProposalDisplay>, ServiceError> {
        self.repo
            .owner_proposal(owner, id)
            .await
            .map_err(ServiceError::from)
    }

    /// Compare the displayed consent revision, plus the displayed operator revision in a grant.
    pub async fn consent(
        &self,
        owner: &OwnerContext,
        disclosed_revision: u64,
        choice: ConsentChoice,
    ) -> Result<Consent, ServiceError> {
        let owner = owner.clone();
        if let ConsentChoice::Grant {
            disclosed_policy_revision,
        } = choice
        {
            let current = self.repo.effective_policy(&owner).await?;
            if !self
                .current_configuration(&current)
                .is_some_and(|config| config.policy.revision == disclosed_policy_revision)
            {
                return Err(ServiceError::Stale);
            }
        }
        // Storage rechecks both revisions and the owner's authority in the write transaction.
        self.repo
            .set_consent(&owner, disclosed_revision, choice, Utc::now())
            .await
            .map_err(ServiceError::from)
    }

    pub async fn set_approval_mode(
        &self,
        owner: &OwnerContext,
        expected_policy: &PolicyVersion,
        mode: ApprovalMode,
    ) -> Result<ApprovalSetting, ServiceError> {
        let owner = owner.clone();
        let expected_policy = *expected_policy;
        if mode == ApprovalMode::SecondaryLlm {
            let current = self.repo.effective_policy(&owner).await?;
            if current.version() != expected_policy
                || (current.configured.is_some() && self.current_configuration(&current).is_none())
            {
                return Err(ServiceError::Stale);
            }
        }
        self.repo
            .set_approval_mode(&owner, &expected_policy, mode)
            .await
            .map_err(ServiceError::from)
    }

    pub async fn effective_policy(
        &self,
        owner: &OwnerContext,
    ) -> Result<EffectiveRefinementPolicy, ServiceError> {
        let owner = owner.clone();
        let mut policy = self.repo.effective_policy(&owner).await?;
        let configured = self.current_configuration(&policy);
        let routes = configured.map(|config| config.routes.disclosure());
        let approval_mode =
            if configured.is_some_and(|config| config.policy.configuration.allow_automatic) {
                policy.approval.mode
            } else {
                ApprovalMode::Human
            };
        // A missing or mismatched runtime cannot admit inference. The owner can still read.
        policy.inference_admitted &= configured.is_some();
        Ok(EffectiveRefinementPolicy {
            policy,
            routes,
            approval_mode,
        })
    }

    fn current_configuration(&self, policy: &EffectivePolicy) -> Option<&ReconciledRefinement> {
        self.configured
            .filter(|config| policy.configured.as_ref() == Some(&config.policy))
    }
}
