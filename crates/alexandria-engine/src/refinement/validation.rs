//! Strict pure validation of model-produced quality proposals.

use std::collections::{BTreeMap, BTreeSet};

use alexandria_storage::models::identity::{OperationId, ToSql};
use alexandria_storage::models::refinement::{
    ApprovedRelationshipEffect, EvidenceBundle, ProposalCategory, QualityOperation,
};
use serde::Deserialize;
use thiserror::Error;

use super::proposal::ValidatedProposal;

pub const MAX_FOCAL_CONTENT_BYTES: usize = 32 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("invalid proposal")]
    Invalid,
    #[error("additional context is required")]
    NeedsContext,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalOutput {
    category: ProposalCategory,
    operation: OperationOutput,
    #[serde(default)]
    relationship_effects: Vec<EffectOutput>,
    rationale: String,
    uncertainty: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum OperationOutput {
    Rewrite {
        source: String,
        content: String,
    },
    Consolidate {
        canonical: String,
        sources: Vec<String>,
    },
    Contradiction {
        sources: Vec<String>,
    },
    Noise {
        source: String,
    },
    Supersede {
        source: String,
        replacement: String,
    },
    Split {
        source: String,
        parts: Vec<String>,
    },
    NoSafeChange,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum EffectOutput {
    LinkSourceToSource { source: String, target: String },
    GeneratedEndpoint { operation: String, slot: u16 },
}

/// Parse exactly one JSON document, optionally enclosed in one `json` code fence.
/// Prose, multiple fences, and trailing material are rejected.
fn parse_response(output: &str) -> Result<ProposalOutput, ValidationError> {
    let trimmed = output.trim();
    let json = if let Some(body) = trimmed.strip_prefix("```json\n") {
        body.strip_suffix("\n```").ok_or(ValidationError::Invalid)?
    } else if trimmed.starts_with("```") {
        return Err(ValidationError::Invalid);
    } else {
        trimmed
    };
    serde_json::from_str(json).map_err(|_| ValidationError::Invalid)
}

/// Validate a complete model response against complete evidence. The returned wrapper is not a
/// persisted authority or approval; this function only proves that the proposed operation is
/// structurally grounded in the supplied bundle.
pub fn validate_proposal(
    input: &EvidenceBundle,
    output: &str,
) -> Result<ValidatedProposal, ValidationError> {
    validate_evidence(input)?;
    if !input.coverage.focal_text_complete
        || input.focal.complete_content.len() > MAX_FOCAL_CONTENT_BYTES
    {
        return Err(ValidationError::NeedsContext);
    }
    let parsed = parse_response(output)?;
    let seen = candidate_ids(input)?;
    let operation = validate_operation(parsed.category, parsed.operation, &seen, input)?;
    let relationship_effects = validate_effects(parsed.relationship_effects, &seen)?;
    if parsed.rationale.trim().is_empty()
        || parsed.rationale.len() > 4096
        || parsed.uncertainty.len() > 4096
    {
        return Err(ValidationError::Invalid);
    }
    Ok(ValidatedProposal {
        category: parsed.category,
        operation,
        relationship_effects,
        rationale: parsed.rationale,
        uncertainty: parsed.uncertainty,
    })
}

fn validate_evidence(input: &EvidenceBundle) -> Result<(), ValidationError> {
    let partition = &input.focal.partition;
    if input
        .related
        .iter()
        .any(|candidate| candidate.partition != *partition)
    {
        return Err(ValidationError::Invalid);
    }
    if matches!(
        &partition.scope,
        alexandria_storage::models::MemoryScope::Unassigned
    ) && input.coverage.selection
        != alexandria_storage::models::refinement::EvidenceSelectionKind::OwnerSelected
    {
        return Err(ValidationError::Invalid);
    }
    let mut ids = BTreeSet::new();
    ids.insert(id_text(&input.focal));
    if input
        .related
        .iter()
        .any(|candidate| !ids.insert(id_text(candidate)))
    {
        return Err(ValidationError::Invalid);
    }
    let mut omitted = BTreeSet::new();
    for neighbor in &input.coverage.omitted_neighbors {
        let id = neighbor.id.as_record_id().to_sql();
        if ids.contains(&id) || !omitted.insert(id) {
            return Err(ValidationError::Invalid);
        }
    }
    Ok(())
}

fn candidate_ids(
    input: &EvidenceBundle,
) -> Result<BTreeMap<String, alexandria_storage::models::FactId>, ValidationError> {
    let mut ids = BTreeMap::new();
    for candidate in std::iter::once(&input.focal).chain(input.related.iter()) {
        let key = id_text(candidate);
        if ids.insert(key, candidate.id.clone()).is_some() {
            return Err(ValidationError::Invalid);
        }
    }
    Ok(ids)
}

fn id_text(candidate: &alexandria_storage::models::Candidate) -> String {
    candidate.id.as_record_id().to_sql()
}

fn require_seen(
    id: &str,
    seen: &BTreeMap<String, alexandria_storage::models::FactId>,
) -> Result<alexandria_storage::models::FactId, ValidationError> {
    seen.get(id).cloned().ok_or(ValidationError::Invalid)
}

fn validate_operation(
    category: ProposalCategory,
    operation: OperationOutput,
    seen: &BTreeMap<String, alexandria_storage::models::FactId>,
    input: &EvidenceBundle,
) -> Result<QualityOperation, ValidationError> {
    let focal = input.focal.id.clone();
    let result = match operation {
        OperationOutput::Rewrite { source, content } if category == ProposalCategory::Rewrite => {
            if require_seen(&source, seen)? != focal || content.trim().is_empty() {
                return Err(ValidationError::Invalid);
            }
            QualityOperation::Rewrite {
                source: focal,
                content,
            }
        }
        OperationOutput::Consolidate { canonical, sources }
            if category == ProposalCategory::Consolidate =>
        {
            let canonical = require_seen(&canonical, seen)?;
            let sources = distinct_seen(sources, seen)?;
            if sources.is_empty() || sources.contains(&canonical) {
                return Err(ValidationError::Invalid);
            }
            QualityOperation::Consolidate { canonical, sources }
        }
        OperationOutput::Contradiction { sources }
            if category == ProposalCategory::Contradiction =>
        {
            let sources = distinct_seen(sources, seen)?;
            if sources.len() < 2 {
                return Err(ValidationError::Invalid);
            }
            QualityOperation::Contradiction { sources }
        }
        OperationOutput::Noise { source } if category == ProposalCategory::Noise => {
            let source = require_seen(&source, seen)?;
            if source != focal {
                return Err(ValidationError::Invalid);
            }
            QualityOperation::Noise { source }
        }
        OperationOutput::Supersede {
            source,
            replacement,
        } if category == ProposalCategory::Supersede => {
            let source = require_seen(&source, seen)?;
            let replacement = require_seen(&replacement, seen)?;
            if source != focal || source == replacement {
                return Err(ValidationError::Invalid);
            }
            QualityOperation::Supersede {
                source,
                replacement,
            }
        }
        OperationOutput::Split { source, parts } if category == ProposalCategory::Split => {
            let source = require_seen(&source, seen)?;
            if source != focal || parts.len() < 2 || parts.iter().any(|part| part.trim().is_empty())
            {
                return Err(ValidationError::Invalid);
            }
            QualityOperation::Split { source, parts }
        }
        OperationOutput::NoSafeChange if category == ProposalCategory::Contradiction => {
            QualityOperation::NoSafeChange
        }
        _ => return Err(ValidationError::Invalid),
    };
    Ok(result)
}

fn distinct_seen(
    values: Vec<String>,
    seen: &BTreeMap<String, alexandria_storage::models::FactId>,
) -> Result<Vec<alexandria_storage::models::FactId>, ValidationError> {
    let facts = values
        .into_iter()
        .map(|id| require_seen(&id, seen))
        .collect::<Result<Vec<_>, _>>()?;
    let mut distinct = facts.clone();
    distinct.sort();
    distinct.dedup();
    if distinct.len() != facts.len() {
        return Err(ValidationError::Invalid);
    }
    Ok(facts)
}

fn validate_effects(
    effects: Vec<EffectOutput>,
    seen: &BTreeMap<String, alexandria_storage::models::FactId>,
) -> Result<Vec<ApprovedRelationshipEffect>, ValidationError> {
    let mut result = Vec::with_capacity(effects.len());
    let mut slots = BTreeSet::new();
    for effect in effects {
        match effect {
            EffectOutput::LinkSourceToSource { source, target } => {
                let source = require_seen(&source, seen)?;
                let target = require_seen(&target, seen)?;
                if source == target {
                    return Err(ValidationError::Invalid);
                }
                result.push(ApprovedRelationshipEffect::LinkSourceToSource { source, target });
            }
            EffectOutput::GeneratedEndpoint { operation, slot } => {
                if operation.trim().is_empty() || !slots.insert((operation.clone(), slot)) {
                    return Err(ValidationError::Invalid);
                }
                result.push(ApprovedRelationshipEffect::GeneratedEndpoint {
                    operation: OperationId::new(operation),
                    slot,
                });
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alexandria_storage::models::Provenance;
    use alexandria_storage::models::identity::{
        FactId, MemoryScope, Partition, RecordId, Version, WorkspaceId,
    };
    use alexandria_storage::models::refinement::{
        Candidate, EvidenceCoverage, EvidenceSelectionKind, OmissionReason, OmittedNeighbor,
    };

    fn fact(key: &str) -> FactId {
        FactId::try_from(RecordId::new("fact", key)).unwrap()
    }

    fn workspace(key: &str) -> WorkspaceId {
        WorkspaceId::try_from(RecordId::new("workspace", key)).unwrap()
    }

    fn candidate(workspace: &WorkspaceId, key: &str, content: &str) -> Candidate {
        Candidate {
            id: fact(key),
            partition: Partition {
                workspace: workspace.clone(),
                scope: MemoryScope::Personal,
            },
            version: Version::new(1),
            complete_content: content.to_owned(),
            tags: Vec::new(),
            provenance: Vec::<Provenance>::new(),
            semantic_hash: format!("hash-{key}"),
        }
    }

    fn evidence() -> EvidenceBundle {
        let workspace = workspace("owner-workspace");
        EvidenceBundle {
            focal: candidate(
                &workspace,
                "focal",
                "The endpoint is not always available in every region.",
            ),
            related: vec![
                candidate(&workspace, "sourcea", "Availability depends on region."),
                candidate(
                    &workspace,
                    "sourceb",
                    "The service is unavailable during maintenance.",
                ),
            ],
            coverage: EvidenceCoverage {
                focal_text_complete: true,
                omitted_neighbors: Vec::new(),
                selection: EvidenceSelectionKind::Scheduled,
            },
            inference_policy: alexandria_storage::models::refinement::InferencePolicyId(
                "policy".into(),
            ),
        }
    }

    fn output(category: &str, operation: &str, effects: &str) -> String {
        format!(
            r#"{{"category":"{category}","operation":{operation},"relationship_effects":{effects},"rationale":"Preserves qualification.","uncertainty":""}}"#
        )
    }

    #[test]
    fn all_six_categories_validate_grounded_shapes() {
        let input = evidence();
        let cases = [
            (
                "rewrite",
                r#"{"kind":"rewrite","source":"fact:focal","content":"The service is not consistently available."}"#,
            ),
            (
                "consolidate",
                r#"{"kind":"consolidate","canonical":"fact:sourcea","sources":["fact:sourceb"]}"#,
            ),
            (
                "contradiction",
                r#"{"kind":"contradiction","sources":["fact:sourcea","fact:sourceb"]}"#,
            ),
            ("noise", r#"{"kind":"noise","source":"fact:focal"}"#),
            (
                "supersede",
                r#"{"kind":"supersede","source":"fact:focal","replacement":"fact:sourcea"}"#,
            ),
            (
                "split",
                r#"{"kind":"split","source":"fact:focal","parts":["Available in region A.","Unavailable in region B."]}"#,
            ),
        ];
        for (category, operation) in cases {
            let response = output(category, operation, "[]");
            assert!(
                validate_proposal(&input, &response).is_ok(),
                "rejected {response}"
            );
        }
        let no_safe_change = output("contradiction", r#"{"kind":"no_safe_change"}"#, "[]");
        assert!(validate_proposal(&input, &no_safe_change).is_ok());
    }

    #[test]
    fn incomplete_focal_cannot_authorize_change() {
        let mut input = evidence();
        input.coverage.focal_text_complete = false;
        let response = output(
            "rewrite",
            r#"{"kind":"rewrite","source":"fact:focal","content":"The endpoint is available."}"#,
            "[]",
        );
        assert_eq!(
            validate_proposal(&input, &response),
            Err(ValidationError::NeedsContext)
        );
    }

    #[test]
    fn fixture_preserves_negation_and_qualification() {
        let input = evidence();
        let response = output(
            "rewrite",
            r#"{"kind":"rewrite","source":"fact:focal","content":"The endpoint is not always available in every region."}"#,
            "[]",
        );
        let validated = validate_proposal(&input, &response).unwrap();
        assert_eq!(
            validated.operation,
            QualityOperation::Rewrite {
                source: input.focal.id.clone(),
                content: "The endpoint is not always available in every region.".into(),
            }
        );
        assert!(input.focal.complete_content.contains("not always"));
        assert!(input.focal.complete_content.contains("every region"));
    }

    #[test]
    fn malformed_json_and_disallowed_code_fence_are_rejected() {
        let input = evidence();
        assert_eq!(
            validate_proposal(&input, "{not json}"),
            Err(ValidationError::Invalid)
        );
        let fenced = format!(
            "```rust\n{}\n```",
            output(
                "rewrite",
                r#"{"kind":"rewrite","source":"fact:focal","content":"A claim."}"#,
                "[]"
            )
        );
        assert_eq!(
            validate_proposal(&input, &fenced),
            Err(ValidationError::Invalid)
        );
    }

    #[test]
    fn relationship_effects_cannot_name_unseen_or_foreign_ids() {
        let input = evidence();
        let unseen = output(
            "rewrite",
            r#"{"kind":"rewrite","source":"fact:focal","content":"A claim."}"#,
            r#"[{"kind":"link_source_to_source","source":"fact:focal","target":"fact:unseen"}]"#,
        );
        assert_eq!(
            validate_proposal(&input, &unseen),
            Err(ValidationError::Invalid)
        );

        let foreign_workspace = workspace("other-workspace");
        let mut foreign = evidence();
        foreign.related.push(candidate(
            &foreign_workspace,
            "foreign",
            "A foreign-partition claim.",
        ));
        let response = output(
            "rewrite",
            r#"{"kind":"rewrite","source":"fact:focal","content":"A claim."}"#,
            "[]",
        );
        assert_eq!(
            validate_proposal(&foreign, &response),
            Err(ValidationError::Invalid)
        );
    }

    #[test]
    fn omitted_neighbors_are_disclosed_not_fabricated() {
        let mut input = evidence();
        input.coverage.omitted_neighbors.push(OmittedNeighbor {
            id: fact("omitted"),
            reason: OmissionReason::NeighborLimit,
        });
        let response = output(
            "supersede",
            r#"{"kind":"supersede","source":"fact:focal","replacement":"fact:omitted"}"#,
            "[]",
        );
        assert_eq!(
            validate_proposal(&input, &response),
            Err(ValidationError::Invalid)
        );
        assert_eq!(input.coverage.omitted_neighbors.len(), 1);
    }

    #[test]
    fn unsupported_action_scope_destination_and_first_pass_authority_are_rejected() {
        let input = evidence();
        let wrong_action = output(
            "rewrite",
            r#"{"kind":"delete","source":"fact:focal"}"#,
            "[]",
        );
        assert_eq!(
            validate_proposal(&input, &wrong_action),
            Err(ValidationError::Invalid)
        );
        for field in ["scope", "destination", "authority"] {
            let response = format!(
                r#"{{"category":"rewrite","operation":{{"kind":"rewrite","source":"fact:focal","content":"A claim."}},"{field}":"untrusted","rationale":"A rationale.","uncertainty":""}}"#
            );
            assert_eq!(
                validate_proposal(&input, &response),
                Err(ValidationError::Invalid),
                "unexpectedly accepted untrusted {field}"
            );
        }
    }
}
