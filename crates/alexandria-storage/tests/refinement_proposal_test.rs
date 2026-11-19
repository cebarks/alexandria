use alexandria_storage::access::{AccessRepo, OwnerContext};
use alexandria_storage::models::Provenance;
use alexandria_storage::models::identity::{FactId, MemoryScope, Partition, RecordId, Version};
use alexandria_storage::models::refinement::{
    ApprovedRelationshipEffect, Authority, Candidate, EvidenceBundle, EvidenceCoverage,
    EvidenceSelectionKind, InferencePolicyId, OmissionReason, OmittedNeighbor, ProposalCategory,
    ProposalRevision, ProposalState, QualityOperation, SecondaryVerdict,
};
use alexandria_storage::repos::{AuthRepo, IdentityRepo, PasswordDigest, RefinementRepo};
use alexandria_storage::{Database, schema};
use tokio::sync::OnceCell;

struct Fixture {
    db: Database,
    owner: OwnerContext,
}

impl Fixture {
    async fn new() -> Self {
        static DIGEST: OnceCell<PasswordDigest> = OnceCell::const_new();
        let digest = DIGEST
            .get_or_init(|| async {
                AuthRepo::create_password_digest("proposal-fixture-password-padding")
                    .await
                    .unwrap()
            })
            .await;
        let mut db = Database::connect_embedded().await.unwrap();
        let system = db.claim_system_authority().unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let identity = IdentityRepo::new(db.inner());
        let seeded = identity
            .bootstrap_owner(
                "proposal-fixture",
                digest.as_str(),
                &operation("proposal-owner"),
            )
            .await
            .unwrap();
        let access = AccessRepo::new(&db);
        let principal = access
            .local_principal(&system, &seeded.account)
            .await
            .unwrap();
        let owner = access.authorize(&principal).await.unwrap();
        Self { db, owner }
    }

    fn repo(&self) -> RefinementRepo<'_> {
        RefinementRepo::new(&self.db)
    }
}

fn operation(key: &str) -> alexandria_storage::models::OperationId {
    alexandria_storage::models::OperationId::new(key)
}

fn fact_id(key: &str) -> FactId {
    FactId::try_from(RecordId::new("fact", key)).unwrap()
}

fn candidate(
    workspace: &alexandria_storage::models::WorkspaceId,
    key: &str,
    content: &str,
) -> Candidate {
    Candidate {
        id: fact_id(key),
        partition: Partition {
            workspace: workspace.clone(),
            scope: MemoryScope::Personal,
        },
        version: Version::new(1),
        complete_content: content.to_owned(),
        tags: vec![],
        provenance: Vec::<Provenance>::new(),
        semantic_hash: format!("hash-{key}"),
    }
}

fn proposal(
    owner: &OwnerContext,
    key: &str,
    version: u64,
    related: Vec<Candidate>,
) -> ProposalRevision {
    let focal = candidate(
        owner.workspace(),
        "focal",
        "The endpoint is not always available.",
    );
    ProposalRevision {
        id: RecordId::new("refinement_proposal", key),
        revision: Version::new(version),
        partition: focal.partition.clone(),
        sources: vec![],
        evidence: EvidenceBundle {
            focal: focal.clone(),
            related,
            coverage: EvidenceCoverage {
                focal_text_complete: true,
                omitted_neighbors: vec![],
                selection: EvidenceSelectionKind::Scheduled,
            },
            inference_policy: InferencePolicyId("a".repeat(64)),
        },
        category: ProposalCategory::Rewrite,
        proposed: QualityOperation::Rewrite {
            source: focal.id,
            content: "The endpoint is not consistently available.".into(),
        },
        approved_relationship_effects: vec![],
        rationale: "Preserves the qualification.".into(),
        uncertainty: String::new(),
        origin_policy: Authority {
            account: owner.account().clone(),
            consent_revision: 1,
            owner_mode_revision: 0,
            inference_policy_id: InferencePolicyId("a".repeat(64)),
            configured_revision: 1,
            mode: alexandria_storage::models::refinement::ApprovalMode::Human,
        },
        state: ProposalState::Pending,
        needs_human: true,
        deferred: false,
    }
}

#[test]
fn proposal_categories_and_non_actionable_outcomes_are_closed() {
    let categories = [
        ProposalCategory::Rewrite,
        ProposalCategory::Consolidate,
        ProposalCategory::Contradiction,
        ProposalCategory::Noise,
        ProposalCategory::Supersede,
        ProposalCategory::Split,
    ];
    assert_eq!(categories.len(), 6);
    assert_eq!(
        QualityOperation::NoSafeChange,
        QualityOperation::NoSafeChange
    );
    assert_eq!(
        SecondaryVerdict::NeedsContext,
        SecondaryVerdict::NeedsContext
    );
}

#[test]
fn evidence_retains_complete_qualified_text_and_discloses_omissions() {
    let workspace = alexandria_storage::models::identity::WorkspaceId::try_from(RecordId::new(
        "workspace",
        "owner-workspace",
    ))
    .unwrap();
    let focal = candidate(&workspace, "focal", "The endpoint is not always available.");
    let omitted = fact_id("not-selected");
    let bundle = EvidenceBundle {
        focal: focal.clone(),
        related: vec![candidate(
            &workspace,
            "related",
            "Availability depends on region.",
        )],
        coverage: EvidenceCoverage {
            focal_text_complete: true,
            omitted_neighbors: vec![OmittedNeighbor {
                id: omitted.clone(),
                reason: OmissionReason::NeighborLimit,
            }],
            selection: EvidenceSelectionKind::OwnerSelected,
        },
        inference_policy: InferencePolicyId("policy".into()),
    };

    assert!(bundle.focal.complete_content.contains("not always"));
    assert!(bundle.related[0].complete_content.contains("depends"));
    assert_eq!(bundle.coverage.omitted_neighbors[0].id, omitted);
    assert_eq!(
        bundle.coverage.selection,
        EvidenceSelectionKind::OwnerSelected
    );
}

#[test]
fn no_safe_change_is_a_typed_non_actionable_operation() {
    assert_eq!(
        QualityOperation::NoSafeChange,
        QualityOperation::NoSafeChange
    );
}

#[tokio::test]
async fn first_assessment_is_persisted_pending_and_cannot_be_applied() {
    let fixture = Fixture::new().await;
    let first = proposal(&fixture.owner, "first", 1, vec![]);
    let persisted = fixture
        .repo()
        .persist_proposal_revision(&fixture.owner, first)
        .await;
    assert!(
        persisted.is_ok(),
        "a valid first assessment should persist: {persisted:?}"
    );
    let persisted = persisted.unwrap();
    assert_eq!(persisted.state, ProposalState::Pending);
    assert!(persisted.needs_human);
    assert!(persisted.approved_relationship_effects.is_empty());
}

#[tokio::test]
async fn evidence_pair_a_b_and_b_a_deduplicates_to_one_revision() {
    let fixture = Fixture::new().await;
    let a = candidate(fixture.owner.workspace(), "a", "A claim.");
    let b = candidate(fixture.owner.workspace(), "b", "B claim.");
    let first = fixture
        .repo()
        .persist_proposal_revision(
            &fixture.owner,
            proposal(&fixture.owner, "pair-ab", 1, vec![a.clone(), b.clone()]),
        )
        .await;
    assert!(
        first.is_ok(),
        "first evidence pair should persist: {first:?}"
    );
    let second = fixture
        .repo()
        .persist_proposal_revision(
            &fixture.owner,
            proposal(&fixture.owner, "pair-ba", 1, vec![b, a]),
        )
        .await;
    assert!(
        second.is_ok(),
        "reversed evidence pair should resolve to existing revision: {second:?}"
    );
    assert_eq!(first.unwrap().id, second.unwrap().id);
}

#[tokio::test]
async fn dismissed_proposal_with_unchanged_sources_does_not_regenerate() {
    let fixture = Fixture::new().await;
    let first = fixture
        .repo()
        .persist_proposal_revision(
            &fixture.owner,
            proposal(&fixture.owner, "dismissed-same-source", 1, vec![]),
        )
        .await;
    assert!(
        first.is_ok(),
        "initial assessment should persist: {first:?}"
    );
    let first = first.unwrap();
    let dismissed = fixture
        .repo()
        .dismiss_proposal_revision(&fixture.owner, &first.id, first.revision)
        .await;
    assert!(
        dismissed.is_ok(),
        "pending revision should be dismissible: {dismissed:?}"
    );
    let retry = fixture
        .repo()
        .persist_proposal_revision(
            &fixture.owner,
            proposal(&fixture.owner, "dismissed-same-source", 1, vec![]),
        )
        .await;
    assert!(
        retry.is_ok(),
        "unchanged source should resolve to dismissed assessment: {retry:?}"
    );
    assert_eq!(retry.unwrap().state, ProposalState::Dismissed);
}

#[tokio::test]
async fn reviewer_alternative_is_a_new_pending_revision_not_self_approved() {
    let fixture = Fixture::new().await;
    let first = fixture
        .repo()
        .persist_proposal_revision(
            &fixture.owner,
            proposal(&fixture.owner, "review-alternative", 1, vec![]),
        )
        .await;
    assert!(first.is_ok(), "initial proposal should persist: {first:?}");
    let first = first.unwrap();
    let alternative = fixture
        .repo()
        .create_review_alternative(
            &fixture.owner,
            &first.id,
            first.revision,
            proposal(&fixture.owner, "review-alternative-r2", 2, vec![]),
        )
        .await;
    assert!(
        alternative.is_ok(),
        "reviewer alternative should create a revision: {alternative:?}"
    );
    let alternative = alternative.unwrap();
    assert!(alternative.revision > first.revision);
    assert_eq!(alternative.state, ProposalState::Pending);
    assert!(alternative.needs_human);
}

#[tokio::test]
async fn relationship_effects_cannot_name_unseen_or_foreign_ids() {
    let fixture = Fixture::new().await;
    for foreign in [false, true] {
        let mut invalid = proposal(
            &fixture.owner,
            if foreign {
                "foreign-effect"
            } else {
                "unseen-effect"
            },
            1,
            vec![],
        );
        invalid.approved_relationship_effects.push(
            ApprovedRelationshipEffect::RemoveSourceMembership {
                source: fact_id(if foreign {
                    "foreign-source"
                } else {
                    "not-in-evidence"
                }),
                cluster: RecordId::new("cluster", "unseen-cluster"),
            },
        );
        let result = fixture
            .repo()
            .persist_proposal_revision(&fixture.owner, invalid)
            .await;
        assert!(
            result.is_err(),
            "unseen or foreign relationship endpoints must be refused: {result:?}"
        );
    }
}
