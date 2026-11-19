//! Storage-owned refinement shapes. The bridge translates them to engine policy types; the
//! dependency must never point from storage to engine. Row codecs keep record IDs typed when binding
//! and validate the table when decoding; account IDs are not freely deserializable strings.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use super::identity::{AccountId, FactId, OperationId, Partition, Version};
use super::mutation::ExpectedState;
use super::provenance::Provenance;
use surrealdb::types::RecordId;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, SurrealValue,
)]
#[serde(rename_all = "snake_case")]
#[surreal(untagged)]
pub enum Protocol {
    #[surreal(rename = "open_ai_chat_completions")]
    OpenAiChatCompletions,
    #[surreal(rename = "anthropic_messages")]
    AnthropicMessages,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, SurrealValue)]
pub struct RouteDestinationId {
    pub protocol: Protocol,
    pub scheme: String,
    pub host: String,
    pub effective_port: u16,
    pub api_prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InferencePolicyId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredPolicy {
    pub destination_set: BTreeSet<RouteDestinationId>,
    pub inference_policy_id: InferencePolicyId,
    pub allow_automatic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRevision {
    pub revision: u64,
    pub configuration: ConfiguredPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consent {
    pub account: AccountId,
    pub destination_set: BTreeSet<RouteDestinationId>,
    pub revision: u64,
    pub granted_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentChoice {
    Grant { disclosed_policy_revision: u64 },
    Revoke,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SurrealValue)]
#[serde(rename_all = "snake_case")]
#[surreal(untagged, rename_all = "snake_case")]
pub enum ApprovalMode {
    Human,
    SecondaryLlm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSetting {
    pub mode: ApprovalMode,
    pub revision: u64,
}

/// Owner mode writes compare the full snapshot shown to the owner, not just the mode row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyVersion {
    pub configured_revision: Option<u64>,
    pub consent_revision: u64,
    pub owner_mode_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectivePolicy {
    pub configured: Option<PolicyRevision>,
    pub consent: Option<Consent>,
    /// The owner's saved preference. The policy engine clamps automatic mode to the server ceiling.
    pub approval: ApprovalSetting,
    pub inference_admitted: bool,
}

impl EffectivePolicy {
    pub fn version(&self) -> PolicyVersion {
        PolicyVersion {
            configured_revision: self.configured.as_ref().map(|policy| policy.revision),
            consent_revision: self.consent.as_ref().map_or(0, |consent| consent.revision),
            owner_mode_revision: self.approval.revision,
        }
    }

    /// Pass only this grant to the pure authority binder. A historical consent row
    /// remains visible to the owner even after a destination change invalidates it.
    pub fn admitted_consent(&self) -> Option<&Consent> {
        self.consent.as_ref().filter(|_| self.inference_admitted)
    }
}

/// Storage counterpart to the immutable generation binding used by the pure policy engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    pub account: AccountId,
    pub consent_revision: u64,
    pub owner_mode_revision: u64,
    pub inference_policy_id: InferencePolicyId,
    pub configured_revision: u64,
    pub mode: ApprovalMode,
}

/// Needs-human and deferred are not extra proposal states; neither can authorize a mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SurrealValue)]
#[serde(rename_all = "snake_case")]
#[surreal(untagged, rename_all = "snake_case")]
pub enum ProposalState {
    Pending,
    Applied,
    Dismissed,
    Stale,
    Reverted,
}

/// Complete, partition-bound input record. Content is never truncated in an actionable bundle.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: FactId,
    pub partition: Partition,
    pub version: Version,
    pub complete_content: String,
    pub tags: Vec<String>,
    pub provenance: Vec<Provenance>,
    pub semantic_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSelectionKind {
    Scheduled,
    OwnerSelected,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OmittedNeighbor {
    pub id: FactId,
    pub reason: OmissionReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmissionReason {
    OutsideSelection,
    NeighborLimit,
    ByteLimit,
    Ineligible,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceCoverage {
    pub focal_text_complete: bool,
    pub omitted_neighbors: Vec<OmittedNeighbor>,
    pub selection: EvidenceSelectionKind,
}

/// All members are captured as complete text in one authorized partition.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceBundle {
    pub focal: Candidate,
    pub related: Vec<Candidate>,
    pub coverage: EvidenceCoverage,
    pub inference_policy: InferencePolicyId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalCategory {
    Rewrite,
    Consolidate,
    Contradiction,
    Noise,
    Supersede,
    Split,
}

/// Closed operation vocabulary; there is no caller-provided patch, SQL, or selector field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualityOperation {
    Rewrite {
        source: FactId,
        content: String,
    },
    Consolidate {
        canonical: FactId,
        sources: Vec<FactId>,
    },
    Contradiction {
        sources: Vec<FactId>,
    },
    Noise {
        source: FactId,
    },
    Supersede {
        source: FactId,
        replacement: FactId,
    },
    Split {
        source: FactId,
        parts: Vec<String>,
    },
    NoSafeChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecondaryVerdict {
    ApproveExact,
    Reject,
    Uncertain,
    NeedsContext,
}

/// Relationship effects are enumerated and name only typed evidence IDs or operation-local slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovedRelationshipEffect {
    RemoveSourceMembership { source: FactId, cluster: RecordId },
    AddSourceMembership { source: FactId, cluster: RecordId },
    LinkSourceToSource { source: FactId, target: FactId },
    GeneratedEndpoint { operation: OperationId, slot: u16 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProposalRevision {
    pub id: RecordId,
    pub revision: Version,
    pub partition: Partition,
    pub sources: Vec<ExpectedState>,
    pub evidence: EvidenceBundle,
    pub category: ProposalCategory,
    pub proposed: QualityOperation,
    pub approved_relationship_effects: Vec<ApprovedRelationshipEffect>,
    pub rationale: String,
    pub uncertainty: String,
    pub origin_policy: Authority,
    pub state: ProposalState,
    pub needs_human: bool,
    pub deferred: bool,
}

/// Owner-authorized, read-only projection for the refinement queue and detail page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalDisplay {
    pub id: RecordId,
    pub revision: u64,
    pub state: ProposalState,
    pub needs_human: bool,
    pub deferred: bool,
    pub scope: String,
    pub project: Option<RecordId>,
    pub proposal_payload: String,
    pub origin_inference_policy_id: String,
    pub operation: Option<RecordId>,
    pub created_at: DateTime<Utc>,
}
