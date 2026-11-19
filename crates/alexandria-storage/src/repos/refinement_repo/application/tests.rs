use super::*;
use crate::access::AccessRepo;
use crate::models::identity::ReadSelection;
use crate::models::refinement::{Authority, EvidenceCoverage, PreparedQualityEmbedding, Protocol};
use crate::repos::{IdentityRepo, ScopedClusterRepo, ScopedMemoryRepo, WriteTarget};

#[tokio::test]
async fn secondary_revoked_after_bookkeeping_conflict_cannot_apply_on_repreparation() {
    let mut db = Database::connect_embedded().await.unwrap();
    let system = db.claim_system_authority().unwrap();
    crate::schema::migrate(db.inner()).await.unwrap();
    let account = IdentityRepo::new(db.inner())
        .bootstrap_owner(
            "secondary-retry",
            "test-only",
            &OperationId::new("bootstrap"),
        )
        .await
        .unwrap()
        .account;
    let access = AccessRepo::new(&db);
    let principal = access.local_principal(&system, &account).await.unwrap();
    let owner = access.authorize(&principal).await.unwrap();
    let repo = RefinementRepo::new(&db);
    let policy = repo
        .reconcile_policy(
            &system,
            Some(&ConfiguredPolicy {
                destination_set: BTreeSet::from([RouteDestinationId {
                    protocol: Protocol::OpenAiChatCompletions,
                    scheme: "https".into(),
                    host: "inference.example".into(),
                    effective_port: 443,
                    api_prefix: "/v1".into(),
                }]),
                inference_policy_id: InferencePolicyId("a".repeat(64)),
                allow_automatic: true,
            }),
        )
        .await
        .unwrap()
        .unwrap();
    let consent = repo
        .set_consent(
            &owner,
            0,
            ConsentChoice::Grant {
                disclosed_policy_revision: policy.revision,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    let version = repo.effective_policy(&owner).await.unwrap().version();
    repo.set_approval_mode(&owner, &version, ApprovalMode::SecondaryLlm)
        .await
        .unwrap();
    let authority = work::current_authority(&db, &account)
        .await
        .unwrap()
        .unwrap();
    let generation = repo.worker_generation(&system).await.unwrap();
    let selected = access
        .select(&owner, ReadSelection::Assigned)
        .await
        .unwrap();
    let fact = ScopedMemoryRepo::new(db.inner(), &owner, &selected)
        .create_fact(
            "original quality text",
            0.5,
            &[0.1, 0.2],
            &[],
            WriteTarget::Personal,
            Utc::now(),
        )
        .await
        .unwrap()
        .id
        .unwrap();
    let cluster = ScopedClusterRepo::new(db.inner(), &owner, &selected)
        .create(None, &[0.1, 0.2], Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    db.inner()
        .query("RELATE $cluster->contains_memory->$fact;")
        .bind(("cluster", cluster.clone()))
        .bind(("fact", fact.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let fact = FactId::try_from(fact).unwrap();
    let mutation = MutationRepo::new(db.inner());
    let before = mutation.state(&owner, &fact).await.unwrap().unwrap();
    let proposal = rewrite_proposal(&before, authority);
    let operation = OperationId::new("secondary-bookkeeping-retry");
    let mut prepared = PreparedChangeSet::new(operation_kind::QUALITY);
    prepared.quality_embeddings.push(PreparedQualityEmbedding {
        content: "corrected quality text".into(),
        embedding: EmbeddingSnapshot::new(vec![0.6, 0.8], None),
    });
    let mut stale = quality_envelope(&proposal, Some(generation)).unwrap();
    plan_quality(
        &mutation, &owner, &proposal, &operation, &prepared, &mut stale,
    )
    .await
    .unwrap();
    db.inner()
        .query("UPDATE $cluster SET centroid_revision = 1;")
        .bind(("cluster", cluster))
        .await
        .unwrap()
        .check()
        .unwrap();
    let system_access =
        MutationContext::issued_by_system(&system, account, owner.workspace().clone());
    let first = mutation
        .apply(&system_access, &operation, &proposal.sources, &stale)
        .await;
    assert!(
        matches!(first, Err(StorageError::RetryableBookkeepingConflict)),
        "{first:?}"
    );

    repo.set_consent(&owner, consent.revision, ConsentChoice::Revoke, Utc::now())
        .await
        .unwrap();
    let retry = repo
        .apply_secondary(&system, &proposal, generation, &operation, &prepared)
        .await;
    assert!(matches!(retry, Err(StorageError::Stale)), "{retry:?}");
    assert_eq!(
        before,
        mutation.state(&owner, &fact).await.unwrap().unwrap()
    );
    assert!(
        mutation
            .operation(&owner, operation.as_str())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repo.owner_proposal(&owner, &proposal.id)
            .await
            .unwrap()
            .is_none()
    );
}

fn rewrite_proposal(before: &FactSnapshot, authority: Authority) -> ProposalRevision {
    let source = before.id.clone();
    let partition = Partition {
        workspace: before.workspace.clone(),
        scope: before.scope.clone(),
    };
    ProposalRevision {
        id: RecordId::new("refinement_proposal", "secondary-retry"),
        revision: Version::new(1),
        partition: partition.clone(),
        sources: vec![ExpectedState::of(before)],
        evidence: EvidenceBundle {
            focal: Candidate {
                id: source.clone(),
                partition,
                version: before.version,
                complete_content: before.content.clone(),
                tags: before.tags.clone(),
                provenance: vec![],
                semantic_hash: before.semantic_hash(),
            },
            related: vec![],
            coverage: EvidenceCoverage {
                focal_text_complete: true,
                omitted_neighbors: vec![],
                selection: EvidenceSelectionKind::Scheduled,
            },
            inference_policy: authority.inference_policy_id.clone(),
        },
        category: ProposalCategory::Rewrite,
        proposed: QualityOperation::Rewrite {
            source,
            content: "corrected quality text".into(),
        },
        approved_relationship_effects: vec![],
        rationale: "Rewrite the cited active fact.".into(),
        uncertainty: "none".into(),
        origin_policy: authority,
        state: ProposalState::Pending,
        needs_human: true,
        deferred: false,
    }
}
