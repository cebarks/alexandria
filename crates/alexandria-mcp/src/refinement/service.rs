use alexandria_engine::refinement::policy::{
    self as policy, InferencePolicyInputs, RouteDisclosure, RoutePair,
};
use alexandria_storage::Database;
use alexandria_storage::access::{OwnerContext, SystemContext};
use alexandria_storage::models::refinement::{
    ApprovalMode, ApprovalSetting, ConfiguredPolicy, Consent, ConsentChoice, EffectivePolicy,
    InferencePolicyId, PolicyRevision, PolicyVersion, ProposalDisplay, Protocol,
    RouteDestinationId,
};
use alexandria_storage::repos::RefinementRepo;
use alexandria_storage::repos::refinement_repo::{AttemptId, AttemptView, EnqueueRequest};
use chrono::Utc;
use surrealdb::types::RecordId;

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
    repo: RefinementRepo<'a>,
    configured: Option<&'a ReconciledRefinement>,
}

impl<'a> RefinementService<'a> {
    pub fn new(db: &'a Database, configured: Option<&'a ReconciledRefinement>) -> Self {
        Self {
            repo: RefinementRepo::new(db),
            configured,
        }
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
