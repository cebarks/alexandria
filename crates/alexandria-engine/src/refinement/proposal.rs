//! Non-persisted validated proposal shape, derived only from complete storage evidence.

use alexandria_storage::models::refinement::{
    ApprovedRelationshipEffect, ProposalCategory, QualityOperation,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedProposal {
    pub category: ProposalCategory,
    pub operation: QualityOperation,
    pub relationship_effects: Vec<ApprovedRelationshipEffect>,
    pub rationale: String,
    pub uncertainty: String,
}
