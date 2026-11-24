//! Quality operations must commit the corpus, private journal and proposal decision together.

use alexandria_storage::access::{AccessRepo, MutationContext, OwnerContext};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::identity::{
    FactId, MemoryScope, OperationId, Partition, ReadSelection, RecordId, RefinementProposalId,
    SemanticOperationId, Version,
};
use alexandria_storage::models::mutation::{
    ClusterChange, ClusterWriteExpectation, EdgeKind, EmbeddingSnapshot, ExpectedState, FactChange,
    FactSnapshot, PreparedChangeSet, RelationChange, RelationTarget, Visibility, effect,
};
use alexandria_storage::models::operation_kind;
use alexandria_storage::models::refinement::{
    ApprovalMode, ApprovedRelationshipEffect, Authority, Candidate, EvidenceBundle,
    EvidenceCoverage, EvidenceSelectionKind, InferencePolicyId, PreparedQualityEmbedding,
    ProposalCategory, ProposalRevision, ProposalState, QualityOperation, QualityProposalTransition,
};
use alexandria_storage::repos::{
    AuthRepo, IdentityRepo, MAX_PREPARATIONS, MutationRepo, PasswordDigest, RefinementRepo,
    ScopedClusterRepo, ScopedMemoryRepo, ScopedSessionRepo, WriteTarget, commit_prepared,
};
use alexandria_storage::{Database, schema};
use chrono::Utc;
use std::sync::atomic::{AtomicUsize, Ordering};
use surrealdb::{
    Surreal,
    engine::any::Any,
    types::{SurrealValue, Value},
};
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
                AuthRepo::create_password_digest("refinement-apply-fixture-password")
                    .await
                    .unwrap()
            })
            .await;
        let mut db = Database::connect_embedded().await.unwrap();
        let system = db.claim_system_authority().unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let account = IdentityRepo::new(db.inner())
            .bootstrap_owner("quality-fixture", digest.as_str(), &op("quality-owner"))
            .await
            .unwrap()
            .account;
        let access = AccessRepo::new(&db);
        let principal = access.local_principal(&system, &account).await.unwrap();
        let owner = access.authorize(&principal).await.unwrap();
        Self { db, owner }
    }

    fn mutation(&self) -> MutationRepo<'_> {
        MutationRepo::new(self.db.inner())
    }

    fn refinement(&self) -> RefinementRepo<'_> {
        RefinementRepo::new(&self.db)
    }

    async fn seed(&self, content: &str, tag: &str) -> FactId {
        let selected = AccessRepo::new(&self.db)
            .select(&self.owner, ReadSelection::Assigned)
            .await
            .unwrap();
        let row = ScopedMemoryRepo::new(self.db.inner(), &self.owner, &selected)
            .create_fact(
                content,
                0.5,
                &[0.1, 0.2],
                &[tag.into()],
                WriteTarget::Personal,
                Utc::now(),
            )
            .await
            .unwrap();
        FactId::try_from(row.id.unwrap()).unwrap()
    }

    async fn state(&self, fact: &FactId) -> FactSnapshot {
        self.mutation()
            .state(&self.owner, fact)
            .await
            .unwrap()
            .unwrap()
    }

    async fn proposal(
        &self,
        facts: &[&FactId],
        category: ProposalCategory,
        proposed: QualityOperation,
        approved_relationship_effects: Vec<ApprovedRelationshipEffect>,
    ) -> ProposalRevision {
        let mut candidates = Vec::new();
        let mut sources = Vec::new();
        for fact in facts {
            let row = self.state(fact).await;
            sources.push(ExpectedState::of(&row));
            candidates.push(Candidate {
                id: row.id.clone(),
                partition: Partition {
                    workspace: row.workspace.clone(),
                    scope: row.scope.clone(),
                },
                version: row.version,
                complete_content: row.content.clone(),
                tags: row.tags.clone(),
                provenance: vec![],
                semantic_hash: row.semantic_hash(),
            });
        }
        let focal = candidates.remove(0);
        let policy_id = InferencePolicyId("e".repeat(64));
        let proposal = ProposalRevision {
            id: RecordId::new("refinement_proposal", "quality-fixture"),
            revision: Version::new(1),
            partition: focal.partition.clone(),
            sources,
            evidence: EvidenceBundle {
                focal,
                related: candidates,
                coverage: EvidenceCoverage {
                    focal_text_complete: true,
                    omitted_neighbors: vec![],
                    selection: EvidenceSelectionKind::Scheduled,
                },
                inference_policy: policy_id.clone(),
            },
            category,
            proposed,
            approved_relationship_effects,
            rationale: "The evidence is complete for this operation.".into(),
            uncertainty: String::new(),
            origin_policy: Authority {
                account: self.owner.account().clone(),
                consent_revision: 1,
                owner_mode_revision: 0,
                inference_policy_id: policy_id,
                configured_revision: 1,
                mode: ApprovalMode::Human,
            },
            state: ProposalState::Pending,
            needs_human: true,
            deferred: false,
        };
        self.refinement()
            .persist_proposal(&self.owner, proposal)
            .await
            .unwrap()
    }

    async fn rewrite(&self, fact: &FactId) -> ProposalRevision {
        self.proposal(
            &[fact],
            ProposalCategory::Rewrite,
            QualityOperation::Rewrite {
                source: fact.clone(),
                content: "corrected text".into(),
            },
            vec![],
        )
        .await
    }

    async fn applied_id(&self, proposal: &ProposalRevision) -> SemanticOperationId {
        let display = self
            .refinement()
            .owner_proposal(&self.owner, &proposal.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(display.state, ProposalState::Applied);
        SemanticOperationId::try_from(
            display
                .operation
                .expect("applied proposal links its journal"),
        )
        .unwrap()
    }

    async fn execute(&self, sql: &str) {
        self.db.inner().query(sql).await.unwrap().check().unwrap();
    }

    async fn semantic_state(&self) -> Vec<Vec<Value>> {
        let mut response = self.db.inner().query(
            "SELECT * FROM fact ORDER BY id; SELECT * FROM memory_edge ORDER BY id; \
            SELECT * FROM contains_session_memory ORDER BY id; \
            SELECT * FROM contains_memory ORDER BY id; SELECT * FROM cluster ORDER BY id; \
            SELECT * FROM semantic_operation ORDER BY id; SELECT * FROM semantic_snapshot ORDER BY id; \
            SELECT * FROM refinement_proposal ORDER BY id;",
        ).await.unwrap().check().unwrap();
        (0..8).map(|i| response.take(i).unwrap()).collect()
    }

    async fn ordinary_change(&self, fact: &FactId, change: &PreparedChangeSet) {
        let expected = ExpectedState::of(&self.state(fact).await);
        self.mutation()
            .apply(
                &MutationContext::issued_to(&self.owner),
                &op("ordinary-change"),
                &[expected],
                change,
            )
            .await
            .unwrap();
    }
}

fn op(key: &str) -> OperationId {
    OperationId::new(key)
}

fn prepared(texts: &[&str]) -> PreparedChangeSet {
    let mut change = PreparedChangeSet::new(operation_kind::QUALITY);
    change.quality_embeddings = texts
        .iter()
        .map(|text| PreparedQualityEmbedding {
            content: (*text).into(),
            embedding: EmbeddingSnapshot::new(vec![0.6, 0.8], Some("fixture-model")),
        })
        .collect();
    change
}

fn live(row: &FactSnapshot) -> bool {
    !row.visibility.deleted && row.visibility.quarantined_at.is_none()
}

fn has_lineage(row: &FactSnapshot, from: &FactId, to: &FactId) -> bool {
    row.relation_versions.iter().any(|edge| {
        edge.kind == EdgeKind::Memory("derived_from".into())
            && &edge.from == from.as_record_id()
            && &edge.to == to.as_record_id()
    })
}

#[tokio::test]
async fn rewrite_keeps_fact_identity_hides_old_version_and_journals_both() {
    let fx = Fixture::new().await;
    let source = fx.seed("imprecise text", "preserve-me").await;
    let proposal = fx.rewrite(&source).await;
    let applied = fx
        .refinement()
        .apply_human(
            &fx.owner,
            &proposal,
            &op("rewrite"),
            &prepared(&["corrected text"]),
        )
        .await
        .expect("quality rewrite must apply");
    let current = fx.state(&source).await;
    assert_eq!(current.content, "corrected text");
    assert_eq!(current.tags, ["preserve-me"]);
    assert!(live(&current));
    let [archive] = applied.created.as_slice() else {
        panic!("rewrite must archive one old version")
    };
    let old = fx.state(archive).await;
    assert_eq!(old.content, "imprecise text");
    assert!(!live(&old));
    assert!(has_lineage(&current, &source, archive));
    fx.applied_id(&proposal).await;
    assert!(!applied.preimages.is_empty() && !applied.postimages.is_empty());
}

#[tokio::test]
async fn consolidation_keeps_named_canonical_and_complementary_fields_and_lineage() {
    let fx = Fixture::new().await;
    let canonical = fx.seed("the canonical description", "canonical-tag").await;
    let one = fx.seed("equivalent description", "first-source-tag").await;
    let two = fx
        .seed("equivalent description again", "second-source-tag")
        .await;
    let historic = fx.seed("historic evidence", "history").await;
    let hide_history = PreparedChangeSet::new(operation_kind::DELETE)
        .effect(effect::DELETE)
        .fact(FactChange::Update {
            fact: historic.clone(),
            expected: fx.state(&historic).await.version,
            content: None,
            tags: None,
            confidence: None,
            embedding: None,
            visibility: Some(Visibility::deleted()),
            scope: None,
        });
    fx.mutation()
        .apply(
            &MutationContext::issued_to(&fx.owner),
            &op("hide-history"),
            &[ExpectedState::of(&fx.state(&historic).await)],
            &hide_history,
        )
        .await
        .unwrap();
    fx.db
        .inner()
        .query(
            "RELATE $one->memory_edge->$historic SET edge_type = 'derived_from', strength = 1.0;",
        )
        .bind(("one", one.as_record_id().clone()))
        .bind(("historic", historic.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let proposal = fx
        .proposal(
            &[&one, &canonical, &two],
            ProposalCategory::Consolidate,
            QualityOperation::Consolidate {
                canonical: canonical.clone(),
                sources: vec![one.clone(), two.clone()],
            },
            [&one, &two]
                .into_iter()
                .map(|source| ApprovedRelationshipEffect::LinkSourceToSource {
                    source: source.clone(),
                    target: canonical.clone(),
                })
                .collect(),
        )
        .await;
    let applied = fx
        .refinement()
        .apply_human(&fx.owner, &proposal, &op("consolidate"), &prepared(&[]))
        .await
        .expect("quality consolidation must apply");
    let kept = fx.state(&canonical).await;
    assert!(live(&kept));
    assert_eq!(kept.content, "the canonical description");
    assert_eq!(
        kept.tags
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        ["canonical-tag", "first-source-tag", "second-source-tag"]
            .into_iter()
            .collect()
    );
    assert!(applied.created.is_empty());
    for retired in [&one, &two] {
        let row = fx.state(retired).await;
        assert!(!live(&row));
        assert!(has_lineage(&row, retired, &canonical));
    }
    assert!(has_lineage(&fx.state(&one).await, &one, &historic));
    assert!(!live(&fx.state(&historic).await));
}

#[derive(SurrealValue)]
struct GeneratedSessionLink {
    generated_from: Option<RecordId>,
}

#[tokio::test]
async fn split_descendants_keep_scope_session_attribution_and_historical_lineage() {
    let fx = Fixture::new().await;
    let source = fx.seed("two independent claims", "shared-tag").await;
    let sessions = ScopedSessionRepo::new(fx.db.inner(), fx.owner.clone());
    let session = sessions
        .find_or_create("source-session", Some("source-agent"), None)
        .await
        .unwrap();
    use alexandria_storage::models::identity::ToSql;
    sessions
        .add_memory("source-session", &source.as_record_id().to_sql())
        .await
        .unwrap();
    let operation = op("split");
    let proposal = fx
        .proposal(
            &[&source],
            ProposalCategory::Split,
            QualityOperation::Split {
                source: source.clone(),
                parts: vec!["claim one".into(), "claim two".into()],
            },
            (0..2)
                .map(|slot| ApprovedRelationshipEffect::GeneratedEndpoint {
                    operation: operation.clone(),
                    slot,
                })
                .collect(),
        )
        .await;
    let prep = prepared(&["claim one", "claim two"]);
    let result = fx
        .refinement()
        .apply_human(&fx.owner, &proposal, &operation, &prep)
        .await
        .expect("quality split must apply");
    assert_eq!(result.created.len(), 2);
    assert_ne!(result.created[0], result.created[1]);
    assert!(!live(&fx.state(&source).await));
    for (id, claim) in result.created.iter().zip(["claim one", "claim two"]) {
        let row = fx.state(id).await;
        assert_eq!(row.scope, proposal.partition.scope);
        assert_eq!(row.workspace, proposal.partition.workspace);
        assert_eq!(row.content, claim);
        assert!(has_lineage(&row, id, &source));
        let rows: Vec<GeneratedSessionLink> = fx.db.inner()
            .query("SELECT generated_from FROM contains_session_memory WHERE in = $sess AND out = $fact;")
            .bind(("sess", session.id.clone().unwrap())).bind(("fact", id.as_record_id().clone()))
            .await.unwrap().check().unwrap().take(0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].generated_from.as_ref(), Some(source.as_record_id()));
    }
    let before = fx.semantic_state().await;
    let duplicate = fx
        .refinement()
        .apply_human(&fx.owner, &proposal, &operation, &prep)
        .await
        .unwrap();
    assert_eq!(duplicate.operation, result.operation);
    assert!(duplicate.replayed);
    assert_eq!(fx.semantic_state().await, before);
}

#[derive(Debug, PartialEq, SurrealValue)]
struct HeatAccess {
    access_count: i64,
    heat: f64,
}

async fn heat(fx: &Fixture, fact: &FactId) -> HeatAccess {
    let rows: Vec<HeatAccess> = fx
        .db
        .inner()
        .query("SELECT access_count, heat FROM heat_state WHERE memory = $fact;")
        .bind(("fact", fact.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(rows.len(), 1);
    rows.into_iter().next().unwrap()
}

#[derive(SurrealValue)]
struct Centroid {
    centroid: Vec<f32>,
}

async fn centroid(fx: &Fixture, cluster: &RecordId) -> Vec<f32> {
    let rows: Vec<Centroid> = fx
        .db
        .inner()
        .query("SELECT centroid FROM $cluster;")
        .bind(("cluster", cluster.clone()))
        .await
        .unwrap()
        .check()
        .unwrap()
        .take(0)
        .unwrap();
    rows.into_iter()
        .next()
        .expect("cluster with a surviving member remains")
        .centroid
}

#[tokio::test]
async fn quarantine_is_reversible_without_rewinding_later_heat() {
    let fx = Fixture::new().await;
    let source = fx.seed("ambiguous note", "noise").await;
    let survivor = fx.seed("separate useful memory", "useful").await;
    let selected = AccessRepo::new(&fx.db)
        .select(&fx.owner, ReadSelection::Assigned)
        .await
        .unwrap();
    let cluster = ScopedClusterRepo::new(fx.db.inner(), &fx.owner, &selected)
        .create(None, &[0.4, 0.5], Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    fx.db.inner().query("UPDATE $survivor SET embedding = $vector; RELATE $cluster->contains_memory->$source; RELATE $cluster->contains_memory->$survivor;")
        .bind(("source", source.as_record_id().clone())).bind(("survivor", survivor.as_record_id().clone()))
        .bind(("cluster", cluster.clone())).bind(("vector", vec![0.7_f32, 0.8]))
        .await.unwrap().check().unwrap();
    let retained = fx.state(&survivor).await;
    let before_centroid = centroid(&fx, &cluster).await;
    fx.db
        .inner()
        .query(
            "CREATE heat_state SET memory = $fact, heat = 0.3, stability = 1.0, access_count = 0;",
        )
        .bind(("fact", source.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let proposal = fx
        .proposal(
            &[&source],
            ProposalCategory::Noise,
            QualityOperation::Noise {
                source: source.clone(),
            },
            vec![ApprovedRelationshipEffect::RemoveSourceMembership {
                source: source.clone(),
                cluster: cluster.clone(),
            }],
        )
        .await;
    let before = fx.state(&source).await;
    fx.refinement()
        .apply_human(&fx.owner, &proposal, &op("quarantine"), &prepared(&[]))
        .await
        .expect("quality quarantine must apply");
    let hidden = fx.state(&source).await;
    assert!(hidden.visibility.quarantined_at.is_some());
    assert!(!hidden.visibility.deleted);
    assert!(
        !hidden
            .relation_versions
            .iter()
            .any(|edge| edge.kind == EdgeKind::ClusterMembership)
    );
    assert_eq!(centroid(&fx, &cluster).await, vec![0.7, 0.8]);
    assert_eq!(fx.state(&survivor).await, retained);
    let journal = fx.applied_id(&proposal).await;
    fx.db.inner().query("UPDATE heat_state SET access_count = 2, heat = 1.0, last_accessed_at = time::now() WHERE memory = $fact;")
        .bind(("fact", source.as_record_id().clone())).await.unwrap().check().unwrap();
    let recent = heat(&fx, &source).await;
    assert_eq!(recent.access_count, 2);
    fx.refinement()
        .revert(&fx.owner, &journal, &op("undo-quarantine"))
        .await
        .unwrap();
    let restored = fx.state(&source).await;
    assert_eq!(restored.visibility, before.visibility);
    assert_eq!(restored.content, before.content);
    assert_eq!(restored.embedding, before.embedding);
    assert!(restored.version > hidden.version);
    assert!(
        restored
            .relation_versions
            .iter()
            .any(|edge| edge.kind == EdgeKind::ClusterMembership && edge.from == cluster)
    );
    assert_eq!(centroid(&fx, &cluster).await, before_centroid);
    assert_eq!(fx.state(&survivor).await, retained);
    assert_eq!(heat(&fx, &source).await, recent);
    let detail = fx
        .mutation()
        .operation(&fx.owner, "undo-quarantine")
        .await
        .unwrap()
        .unwrap();
    assert!(detail.reverts.is_some());
}

#[tokio::test]
async fn unresolved_contradiction_cannot_apply() {
    let fx = Fixture::new().await;
    let a = fx.seed("client enables redirects", "config").await;
    let b = fx.seed("client disables redirects", "config").await;
    let p = fx
        .proposal(
            &[&a, &b],
            ProposalCategory::Contradiction,
            QualityOperation::Contradiction {
                sources: vec![a.clone(), b.clone()],
            },
            vec![],
        )
        .await;
    let before = fx.semantic_state().await;
    let result = fx
        .refinement()
        .apply_human(&fx.owner, &p, &op("contradiction"), &prepared(&[]))
        .await;
    assert_eq!(fx.semantic_state().await, before);
    assert!(
        matches!(result, Err(StorageError::InvalidInput(_))),
        "unresolved contradiction must be refused: {result:?}"
    );
}

#[tokio::test]
async fn supersession_needs_explicit_evidence() {
    for evidenced in [false, true] {
        let fx = Fixture::new().await;
        let old = fx.seed("Policy v1: write limit is two", "policy").await;
        let replacement = fx
            .seed(
                if evidenced {
                    "Policy v2 explicitly supersedes Policy v1; write limit is four"
                } else {
                    "One workload ran with a write limit of four"
                },
                "policy",
            )
            .await;
        let relation = ApprovedRelationshipEffect::LinkSourceToSource {
            source: old.clone(),
            target: replacement.clone(),
        };
        let p = fx
            .proposal(
                &[&old, &replacement],
                ProposalCategory::Supersede,
                QualityOperation::Supersede {
                    source: old.clone(),
                    replacement: replacement.clone(),
                },
                if evidenced { vec![relation] } else { vec![] },
            )
            .await;
        let before = fx.semantic_state().await;
        let result = fx
            .refinement()
            .apply_human(&fx.owner, &p, &op("supersede"), &prepared(&[]))
            .await;
        if evidenced {
            result.expect("evidenced supersession must apply");
            let retired = fx.state(&old).await;
            assert!(!live(&retired));
            assert!(has_lineage(&retired, &old, &replacement));
            assert!(live(&fx.state(&replacement).await));
        } else {
            assert_eq!(fx.semantic_state().await, before);
            assert!(
                matches!(result, Err(StorageError::InvalidInput(_))),
                "related text is not explicit supersession evidence: {result:?}"
            );
        }
    }
}

#[tokio::test]
async fn fact_lineage_snapshot_audit_and_decision_failures_leave_identical_semantic_state() {
    for (table, condition) in [
        ("fact", "$after.content = 'corrected text'"),
        ("memory_edge", "$after.edge_type = 'derived_from'"),
        ("semantic_snapshot", "$event = 'CREATE'"),
        ("semantic_operation", "$after.kind = 'quality'"),
        ("refinement_proposal", "$after.state = 'applied'"),
    ] {
        let fx = Fixture::new().await;
        let fact = fx.seed("original text", "label").await;
        let proposal = fx.rewrite(&fact).await;
        fx.execute(&format!("DEFINE EVENT OVERWRITE refuse_quality_stage ON TABLE {table} WHEN {condition} THEN {{ THROW 'injected quality stage refusal'; }};")).await;
        let before = fx.semantic_state().await;
        let prep = prepared(&["corrected text"]);
        let result = fx
            .refinement()
            .apply_human(&fx.owner, &proposal, &op("stage-injection"), &prep)
            .await;
        assert_eq!(
            fx.semantic_state().await,
            before,
            "partial write at {table}"
        );
        assert!(
            matches!(result, Err(StorageError::Internal(_))),
            "expected the engine refusal at {table}: {result:?}"
        );
        fx.execute(&format!(
            "REMOVE EVENT IF EXISTS refuse_quality_stage ON TABLE {table};"
        ))
        .await;
        fx.refinement()
            .apply_human(&fx.owner, &proposal, &op("stage-control"), &prep)
            .await
            .expect("same quality write succeeds when the injected failure is removed");
    }
}

#[tokio::test]
async fn ordinary_edit_delete_move_and_graph_change_invalidate_approval_without_partial_writes() {
    for kind in [
        operation_kind::UPDATE,
        operation_kind::DELETE,
        operation_kind::MOVE,
        operation_kind::CLUSTER_CHANGE,
    ] {
        let fx = Fixture::new().await;
        let fact = fx.seed("before review", "reviewed").await;
        let neighbor = fx.seed("another claim", "other").await;
        let p = fx.rewrite(&fact).await;
        let state = fx.state(&fact).await;
        if kind == operation_kind::CLUSTER_CHANGE {
            // Cover an independently inserted relation, not merely a bumped fact revision.
            fx.db.inner().query("RELATE $from_fact->memory_edge->$to_fact SET edge_type = 'derived_from', strength = 1.0;")
                .bind(("from_fact", fact.as_record_id().clone())).bind(("to_fact", neighbor.as_record_id().clone()))
                .await.unwrap().check().unwrap();
        } else {
            let label = match kind {
                operation_kind::UPDATE => effect::TAGS,
                operation_kind::DELETE => effect::DELETE,
                _ => effect::SCOPE,
            };
            let change = PreparedChangeSet::new(kind)
                .effect(label)
                .fact(FactChange::Update {
                    fact: fact.clone(),
                    expected: state.version,
                    content: None,
                    tags: (kind == operation_kind::UPDATE).then(|| vec!["edited".into()]),
                    confidence: None,
                    embedding: None,
                    visibility: (kind == operation_kind::DELETE).then(Visibility::deleted),
                    scope: (kind == operation_kind::MOVE).then_some(MemoryScope::Unassigned),
                });
            fx.ordinary_change(&fact, &change).await;
        }
        assert_ne!(
            fx.state(&fact).await.semantic_hash(),
            state.semantic_hash(),
            "race fixture did not move: {kind}"
        );
        let before = fx.semantic_state().await;
        let result = fx
            .refinement()
            .apply_human(
                &fx.owner,
                &p,
                &op("stale-review"),
                &prepared(&["corrected text"]),
            )
            .await;
        assert_eq!(
            fx.semantic_state().await,
            before,
            "partial write after {kind}"
        );
        assert!(
            matches!(result, Err(StorageError::Stale)),
            "ordinary {kind} did not stale the approval: {result:?}"
        );
    }
}

struct ClusterRetryFixture {
    fx: Fixture,
    input: FactSnapshot,
    unrelated: FactId,
    proposal: ProposalRevision,
    stale: ClusterWriteExpectation,
    fresh: ClusterWriteExpectation,
}

impl ClusterRetryFixture {
    async fn new() -> Self {
        let fx = Fixture::new().await;
        let source = fx.seed("original text", "quality").await;
        let unrelated = fx.seed("an unrelated member", "different").await;
        let selected = AccessRepo::new(&fx.db)
            .select(&fx.owner, ReadSelection::Assigned)
            .await
            .unwrap();
        let cluster = ScopedClusterRepo::new(fx.db.inner(), &fx.owner, &selected)
            .create(None, &[0.1, 0.2], Utc::now())
            .await
            .unwrap()
            .id
            .unwrap();
        for fact in [&source, &unrelated] {
            fx.db
                .inner()
                .query("RELATE $cluster->contains_memory->$fact;")
                .bind(("cluster", cluster.clone()))
                .bind(("fact", fact.as_record_id().clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
        }
        let input = fx.state(&source).await;
        let proposal = fx.rewrite(&source).await;
        let stale = fx
            .mutation()
            .cluster_expectation(&fx.owner, &cluster)
            .await
            .unwrap();
        // Independently refresh the centroid revision without changing the approved fact or any membership.
        fx.db
            .inner()
            .query("UPDATE $cluster SET centroid_revision = 1;")
            .bind(("cluster", cluster.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let fresh = fx
            .mutation()
            .cluster_expectation(&fx.owner, &cluster)
            .await
            .unwrap();
        assert_ne!(stale.centroid_revision, fresh.centroid_revision);
        Self {
            fx,
            input,
            unrelated,
            proposal,
            stale,
            fresh,
        }
    }

    fn change(&self, expectation: ClusterWriteExpectation) -> PreparedChangeSet {
        let source = &self.input;
        let mut change = PreparedChangeSet::new(operation_kind::QUALITY)
            .effect(effect::CONTENT)
            .effect(effect::VECTOR)
            .effect(effect::LINEAGE)
            .effect(effect::CLUSTER_CENTROID)
            .fact(FactChange::Create {
                scope: source.scope.clone(),
                content: source.content.clone(),
                tags: source.tags.clone(),
                confidence: source.confidence,
                embedding: source.embedding.clone().unwrap(),
                visibility: Visibility::deleted(),
                metadata: None,
            })
            .fact(FactChange::Update {
                fact: source.id.clone(),
                expected: source.version,
                content: Some("corrected text".into()),
                tags: None,
                confidence: None,
                embedding: Some(EmbeddingSnapshot::new(
                    vec![0.6, 0.8],
                    Some("fixture-model"),
                )),
                visibility: None,
                scope: None,
            })
            .relate(RelationChange::memory(
                "derived_from",
                RelationTarget::Existing(source.id.as_record_id().clone()),
                RelationTarget::NewFact(0),
                1.0,
            ))
            .cluster(ClusterChange::Effect {
                expectation,
                add_member: None,
                remove_member: None,
                centroid: Some(vec![0.35, 0.5]),
            });
        change.quality_transition = Some(QualityProposalTransition {
            proposal: RefinementProposalId::try_from(self.proposal.id.clone()).unwrap(),
            revision: self.proposal.revision,
        });
        change
    }
}

#[tokio::test]
async fn derived_cluster_conflict_retries_without_changing_approved_effects() {
    let case = ClusterRetryFixture::new().await;
    let stale = case.change(case.stale.clone());
    let fresh = case.change(case.fresh.clone());
    let before_unrelated = case.fx.state(&case.unrelated).await;
    let attempts = AtomicUsize::new(0);
    let applied = commit_prepared(
        &case.fx.mutation(),
        &MutationContext::issued_to(&case.fx.owner),
        &op("cluster-retry"),
        &[ExpectedState::of(&case.input)],
        &fresh.effects,
        || {
            std::future::ready(Ok(if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                stale.clone()
            } else {
                fresh.clone()
            }))
        },
    )
    .await
    .expect("refreshing the derived revision should permit the same quality operation");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(case.fx.state(&case.unrelated).await, before_unrelated);
    assert_eq!(
        case.fx.state(&case.input.id).await.content,
        "corrected text"
    );
    assert_eq!(applied.created.len(), 1);
    case.fx.applied_id(&case.proposal).await;
}

#[tokio::test]
async fn semantic_or_membership_effect_change_never_retries_as_bookkeeping() {
    let case = ClusterRetryFixture::new().await;
    let stale = case.change(case.stale.clone());
    let mut widened = case.change(case.fresh.clone());
    widened.effects.push(EdgeKind::ClusterMembership.effect());
    widened.edge_removals.push(RelationChange::membership(
        case.fresh.cluster.clone(),
        RelationTarget::Existing(case.unrelated.as_record_id().clone()),
    ));
    let before = case.fx.semantic_state().await;
    let attempts = AtomicUsize::new(0);
    let result = commit_prepared(
        &case.fx.mutation(),
        &MutationContext::issued_to(&case.fx.owner),
        &op("widened-retry"),
        &[ExpectedState::of(&case.input)],
        &stale.effects,
        || {
            std::future::ready(Ok(if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                stale.clone()
            } else {
                widened.clone()
            }))
        },
    )
    .await;
    assert!(
        matches!(result, Err(StorageError::Stale)),
        "a different effect is a new decision: {result:?}"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(case.fx.semantic_state().await, before);
}

#[tokio::test]
async fn bookkeeping_retry_exhaustion_retains_no_approval() {
    let case = ClusterRetryFixture::new().await;
    let stale = case.change(case.stale.clone());
    let before = case.fx.semantic_state().await;
    let attempts = AtomicUsize::new(0);
    let result = commit_prepared(
        &case.fx.mutation(),
        &MutationContext::issued_to(&case.fx.owner),
        &op("exhausted-retry"),
        &[ExpectedState::of(&case.input)],
        &stale.effects,
        || {
            attempts.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(stale.clone()))
        },
    )
    .await;
    assert!(
        matches!(result, Err(StorageError::RetryableBookkeepingConflict)),
        "wrong exhaustion result: {result:?}"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), MAX_PREPARATIONS);
    assert_eq!(case.fx.semantic_state().await, before);
    let row = case
        .fx
        .refinement()
        .owner_proposal(&case.fx.owner, &case.proposal.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ProposalState::Pending);
    assert!(row.operation.is_none());
}

async fn prepare_with_owner_suspended(
    db: Surreal<Any>,
    account: RecordId,
    change: PreparedChangeSet,
    suspend: bool,
) -> Result<PreparedChangeSet, StorageError> {
    if suspend {
        db.query("UPDATE $account SET status = 'suspended' RETURN NONE;")
            .bind(("account", account))
            .await
            .unwrap()
            .check()
            .unwrap();
    }
    Ok(change)
}

#[tokio::test]
async fn owner_revoked_during_bookkeeping_retry_cannot_spend_previous_authority() {
    let case = ClusterRetryFixture::new().await;
    let stale = case.change(case.stale.clone());
    let fresh = case.change(case.fresh.clone());
    let before = case.fx.semantic_state().await;
    let attempts = AtomicUsize::new(0);
    let result = commit_prepared(
        &case.fx.mutation(),
        &MutationContext::issued_to(&case.fx.owner),
        &op("revoke-during-retry"),
        &[ExpectedState::of(&case.input)],
        &stale.effects,
        || {
            let suspend = attempts.fetch_add(1, Ordering::SeqCst) != 0;
            prepare_with_owner_suspended(
                case.fx.db.inner().clone(),
                case.fx.owner.account().as_record_id().clone(),
                if suspend {
                    fresh.clone()
                } else {
                    stale.clone()
                },
                suspend,
            )
        },
    )
    .await;
    assert!(
        matches!(result, Err(StorageError::Stale)),
        "retry did not reclaim owner authority: {result:?}"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(case.fx.semantic_state().await, before);
}

#[tokio::test]
async fn rollback_refuses_a_later_semantic_or_reference_change() {
    for graph in [false, true] {
        let fx = Fixture::new().await;
        let fact = fx.seed("original text", "original").await;
        let p = fx.rewrite(&fact).await;
        let applied = fx
            .refinement()
            .apply_human(
                &fx.owner,
                &p,
                &op("apply-before-undo"),
                &prepared(&["corrected text"]),
            )
            .await
            .expect("quality rewrite must apply before rollback conflict can be tested");
        let id = fx.applied_id(&p).await;
        if graph {
            let new_reference = fx.seed("later claim", "later").await;
            fx.db.inner().query("RELATE $claim->memory_edge->$old SET edge_type = 'derived_from', strength = 1.0;")
                .bind(("claim", new_reference.as_record_id().clone()))
                .bind(("old", applied.created[0].as_record_id().clone())).await.unwrap().check().unwrap();
        } else {
            fx.ordinary_change(
                &fact,
                &PreparedChangeSet::new(operation_kind::UPDATE)
                    .effect(effect::TAGS)
                    .fact(FactChange::Update {
                        fact: fact.clone(),
                        expected: fx.state(&fact).await.version,
                        content: None,
                        tags: Some(vec!["later".into()]),
                        confidence: None,
                        embedding: None,
                        visibility: None,
                        scope: None,
                    }),
            )
            .await;
        }
        let before = fx.semantic_state().await;
        let result = fx
            .refinement()
            .revert(&fx.owner, &id, &op("stale-undo"))
            .await;
        assert!(
            matches!(result, Err(StorageError::Stale)),
            "rollback ignored a later change: {result:?}"
        );
        assert_eq!(fx.semantic_state().await, before);
    }
}
