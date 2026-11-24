//! Read-only refinement projections against real scoped facts, policy, and assessment rows.

use std::collections::BTreeSet;

use alexandria_storage::access::{AccessRepo, OwnerContext, SystemContext};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::identity::{
    FactId, MemoryScope, Partition, ProjectId, ReadSelection,
};
use alexandria_storage::models::refinement::{
    ApprovalMode, Authority, ConfiguredPolicy, ConsentChoice, EvidenceBundle,
    EvidenceSelectionKind, InferencePolicyId, OmissionReason, ProposalCategory, ProposalRevision,
    ProposalState, Protocol, QualityOperation, RouteDestinationId,
};
use alexandria_storage::models::{OperationId, Version};
use alexandria_storage::repos::refinement_repo::{
    CandidatePage, EvidenceOutcome, EvidenceSelection,
};
use alexandria_storage::repos::{
    EdgeRepo, HeatRepo, IdentityRepo, RefinementRepo, ScopedMemoryRepo,
};
use alexandria_storage::{Database, parse_record_id, schema};
use chrono::{DateTime, TimeDelta, Utc};
use surrealdb::types::{RecordId, ToSql};

const SEED_TIME: DateTime<Utc> = DateTime::<Utc>::UNIX_EPOCH;

struct Fixture {
    db: Database,
    system: SystemContext,
    owner: OwnerContext,
    other: OwnerContext,
    policy: Authority,
}

impl Fixture {
    async fn new() -> Self {
        let mut db = Database::connect_embedded().await.unwrap();
        let system = db.claim_system_authority().unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let owner = enroll(&db, &system, "candidate-owner").await;
        let other = enroll(&db, &system, "candidate-other").await;
        let repo = RefinementRepo::new(&db);
        let configured = repo
            .reconcile_policy(&system, Some(&configuration('a')))
            .await
            .unwrap()
            .unwrap();
        let mut consent_revision = 0;
        for context in [&owner, &other] {
            let consent = repo
                .set_consent(
                    context,
                    0,
                    ConsentChoice::Grant {
                        disclosed_policy_revision: configured.revision,
                    },
                    SEED_TIME,
                )
                .await
                .unwrap();
            if context.account() == owner.account() {
                consent_revision = consent.revision;
            }
        }
        let policy = Authority {
            account: owner.account().clone(),
            consent_revision,
            owner_mode_revision: 0,
            inference_policy_id: configured.configuration.inference_policy_id,
            configured_revision: configured.revision,
            mode: ApprovalMode::Human,
        };
        Self {
            db,
            system,
            owner,
            other,
            policy,
        }
    }

    fn repo(&self) -> RefinementRepo<'_> {
        RefinementRepo::new(&self.db)
    }

    async fn seed(
        &self,
        key: &str,
        owner: &OwnerContext,
        scope: &MemoryScope,
        content: &str,
        embedding: Vec<f32>,
    ) -> FactId {
        let fact = fact_id(key);
        let (name, project) = match scope {
            MemoryScope::Personal => ("personal", None),
            MemoryScope::Project(project) => ("project", Some(project.as_record_id().clone())),
            MemoryScope::Unassigned => ("unassigned", None),
        };
        self.db
            .inner()
            .query("CREATE $fact SET account = $account, workspace = $workspace, scope = $scope, project = $project, content = $content, embedding = $embedding, created_at = $created, tags = ['candidate'], semantic_revision = 1 RETURN NONE")
            .bind(("fact", fact.as_record_id().clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .bind(("scope", name))
            .bind(("project", project))
            .bind(("content", content.to_owned()))
            .bind(("embedding", embedding))
            .bind(("created", SEED_TIME))
            .await
            .unwrap()
            .check()
            .unwrap();
        fact
    }

    async fn personal(&self, key: &str, content: &str) -> FactId {
        self.seed(
            key,
            &self.owner,
            &MemoryScope::Personal,
            content,
            vec![1.0, 0.0],
        )
        .await
    }

    async fn unassigned(&self, key: &str, content: &str) -> FactId {
        self.seed(
            key,
            &self.owner,
            &MemoryScope::Unassigned,
            content,
            vec![1.0, 0.0],
        )
        .await
    }

    async fn assess(&self, evidence: &EvidenceBundle) {
        let focal = evidence.focal.id.clone();
        let proposal = ProposalRevision {
            id: RecordId::new("refinement_proposal", "candidate-baseline"),
            revision: Version::new(1),
            partition: evidence.focal.partition.clone(),
            sources: vec![],
            evidence: evidence.clone(),
            category: ProposalCategory::Rewrite,
            proposed: QualityOperation::Rewrite {
                source: focal,
                content: "Clarified text.".into(),
            },
            approved_relationship_effects: vec![],
            rationale: "Preserves the meaning of the evidence.".into(),
            uncertainty: String::new(),
            origin_policy: self.policy.clone(),
            state: ProposalState::Pending,
            needs_human: true,
            deferred: false,
        };
        let persisted = self
            .repo()
            .persist_proposal(&self.owner, proposal)
            .await
            .unwrap();
        self.repo()
            .dismiss_proposal_revision(&self.owner, &persisted.id, persisted.revision)
            .await
            .unwrap();
    }
}

async fn enroll(db: &Database, system: &SystemContext, login: &str) -> OwnerContext {
    // The local principal path never verifies a password; this matches the scoped retrieval fixture.
    let seeded = IdentityRepo::new(db.inner())
        .bootstrap_owner(
            login,
            &format!("$argon2id$fixture-prepared-hash-{login}-padding"),
            &OperationId::new(login),
        )
        .await
        .unwrap();
    let access = AccessRepo::new(db);
    let principal = access
        .local_principal(system, &seeded.account)
        .await
        .unwrap();
    access.authorize(&principal).await.unwrap()
}

fn configuration(key: char) -> ConfiguredPolicy {
    ConfiguredPolicy {
        destination_set: BTreeSet::from([RouteDestinationId {
            protocol: Protocol::OpenAiChatCompletions,
            scheme: "https".into(),
            host: "inference.example".into(),
            effective_port: 443,
            api_prefix: "/v1".into(),
        }]),
        inference_policy_id: InferencePolicyId(key.to_string().repeat(64)),
        allow_automatic: false,
    }
}

fn fact_id(key: &str) -> FactId {
    FactId::try_from(parse_record_id(&format!("fact:{key}")).unwrap()).unwrap()
}

fn partition(owner: &OwnerContext, scope: MemoryScope) -> Partition {
    Partition {
        workspace: owner.workspace().clone(),
        scope,
    }
}

fn eligible_before() -> DateTime<Utc> {
    Utc::now() - TimeDelta::days(1)
}

fn bundle(outcome: &EvidenceOutcome) -> &EvidenceBundle {
    match outcome {
        EvidenceOutcome::Ready(bundle) => bundle,
        other => panic!("expected complete evidence, got {other:?}"),
    }
}

fn only_bundle(page: &CandidatePage) -> &EvidenceBundle {
    assert_eq!(page.outcomes.len(), 1, "expected one scheduled assessment");
    bundle(&page.outcomes[0])
}

fn outcome_ids(page: &CandidatePage) -> Vec<FactId> {
    page.outcomes
        .iter()
        .map(|outcome| match outcome {
            EvidenceOutcome::Ready(bundle) => bundle.focal.id.clone(),
            EvidenceOutcome::NeedsContext { focal, .. } => focal.clone(),
        })
        .collect()
}

#[tokio::test]
async fn scheduled_selection_applies_active_assigned_and_live_filters_before_limit() {
    let fx = Fixture::new().await;
    let own = partition(&fx.owner, MemoryScope::Personal);
    let unassigned = partition(&fx.owner, MemoryScope::Unassigned);
    let suspended = partition(&fx.other, MemoryScope::Personal);
    let project = ProjectId::try_from(RecordId::new("project", "candidate-project")).unwrap();
    fx.db.inner()
        .query("CREATE $project SET workspace = $workspace, normalized_alias = 'candidate-project' RETURN NONE")
        .bind(("project", project.as_record_id().clone()))
        .bind(("workspace", fx.owner.workspace().as_record_id().clone()))
        .await.unwrap().check().unwrap();
    let in_project = partition(&fx.owner, MemoryScope::Project(project));
    fx.unassigned("1", "owner review only").await;
    fx.seed(
        "2",
        &fx.other,
        &MemoryScope::Personal,
        "suspended",
        vec![1.0, 0.0],
    )
    .await;
    let deleted = fx.personal("3", "deleted").await;
    let quarantined = fx.personal("4", "quarantined").await;
    let legacy = fact_id("5");
    let project_fact = fx
        .seed(
            "6",
            &fx.owner,
            &in_project.scope,
            "assigned project",
            vec![1.0, 0.0],
        )
        .await;
    let personal = fx.personal("7", "eligible personal").await;
    let eligible_neighbor = fx
        .seed(
            "8",
            &fx.owner,
            &own.scope,
            "lower cosine, but eligible",
            vec![0.95, 0.31225],
        )
        .await;
    // Enough exact but ineligible matches to displace the eligible neighbor in an unscoped top-k.
    for id in 100..112 {
        fx.unassigned(&id.to_string(), "ambient perfect match")
            .await;
    }
    fx.db.inner()
        .query("UPDATE ONLY $other SET status = 'suspended' RETURN NONE; UPDATE ONLY $deleted SET deleted = true RETURN NONE; UPDATE ONLY $quarantined SET quarantined_at = time::now() RETURN NONE; CREATE $legacy SET content = 'legacy with no ownership', embedding = [1.0, 0.0] RETURN NONE;")
        .bind(("other", fx.other.account().as_record_id().clone()))
        .bind(("deleted", deleted.as_record_id().clone()))
        .bind(("quarantined", quarantined.as_record_id().clone()))
        .bind(("legacy", legacy.as_record_id().clone()))
        .await.unwrap().check().unwrap();

    let cut = eligible_before();
    let page = fx.repo().page_candidates(&own, None, 1, cut).await.unwrap();
    let selected = only_bundle(&page);
    assert_eq!(
        selected.focal.id, personal,
        "ineligible lower IDs must not consume the limit"
    );
    assert_eq!(
        selected
            .related
            .iter()
            .map(|neighbor| &neighbor.id)
            .collect::<Vec<_>>(),
        vec![&eligible_neighbor],
        "higher-scoring outsiders must not consume the neighbor limit"
    );
    let page = fx
        .repo()
        .page_candidates(&in_project, None, 1, cut)
        .await
        .unwrap();
    assert_eq!(only_bundle(&page).focal.id, project_fact);
    assert!(matches!(
        fx.repo().page_candidates(&unassigned, None, 1, cut).await,
        Err(StorageError::NotFound)
    ));
    assert!(matches!(
        fx.repo().page_candidates(&suspended, None, 1, cut).await,
        Err(StorageError::Stale)
    ));
}

#[tokio::test]
async fn scheduled_evidence_is_complete_or_explicitly_needs_context() {
    let fx = Fixture::new().await;
    let own = partition(&fx.owner, MemoryScope::Personal);
    let complete = "The endpoint is not always available. The final negation must survive.";
    let eligible = fx.personal("10", complete).await;
    let too_large = fx.personal("20", &"é".repeat(32 * 1024)).await;

    let page = fx
        .repo()
        .page_candidates(&own, None, 10, eligible_before())
        .await
        .unwrap();
    let complete_bundle = page
        .outcomes
        .iter()
        .find_map(|outcome| match outcome {
            EvidenceOutcome::Ready(evidence) if evidence.focal.id == eligible => Some(evidence),
            _ => None,
        })
        .expect("the short focal should remain actionable");
    assert_eq!(complete_bundle.focal.complete_content, complete);
    assert!(complete_bundle.coverage.focal_text_complete);
    assert!(
        complete_bundle.related.is_empty(),
        "oversized neighbor must never be truncated"
    );
    assert!(
        complete_bundle
            .coverage
            .omitted_neighbors
            .iter()
            .any(|entry| { entry.id == too_large && entry.reason == OmissionReason::ByteLimit })
    );
    assert!(
        page.outcomes.iter().any(|outcome| matches!(outcome,
            EvidenceOutcome::NeedsContext { focal, coverage }
                if *focal == too_large && !coverage.focal_text_complete
                    && coverage.selection == EvidenceSelectionKind::Scheduled
        )),
        "oversized focal must be disclosed rather than dropped: {:?}",
        outcome_ids(&page)
    );
    assert!(!page.outcomes.iter().any(|outcome| matches!(outcome,
        EvidenceOutcome::Ready(evidence) if evidence.focal.id == too_large
    )));
}

#[tokio::test]
async fn candidate_and_manual_reads_do_not_create_or_mutate_heat_or_access_state() {
    let fx = Fixture::new().await;
    let own = partition(&fx.owner, MemoryScope::Personal);
    let hot = fx.personal("10", "has a heat row").await;
    let cold = fx.personal("20", "has no heat row").await;
    let linked = fx.personal("25", "a warmable graph neighbor").await;
    let manual = fx.unassigned("30", "explicitly chosen").await;
    let hot = hot.as_record_id().to_sql();
    let cold = cold.as_record_id().to_sql();
    let linked = linked.as_record_id().to_sql();
    let manual_sql = manual.as_record_id().to_sql();
    let heat = HeatRepo::new(fx.db.inner());
    heat.create_for_memory(&hot, 0.25).await.unwrap();
    heat.create_for_memory(&linked, 0.125).await.unwrap();
    heat.create_for_memory(&manual_sql, 0.5).await.unwrap();
    EdgeRepo::new(fx.db.inner())
        .create_edge(&hot, &linked, "related_to", 1.0)
        .await
        .unwrap();
    let snapshot = |row| serde_json::to_value(row).unwrap();
    let hot_before = snapshot(heat.get(&hot).await.unwrap().unwrap());
    let linked_before = snapshot(heat.get(&linked).await.unwrap().unwrap());
    let manual_before = snapshot(heat.get(&manual_sql).await.unwrap().unwrap());
    assert!(heat.get(&cold).await.unwrap().is_none());

    let page = fx
        .repo()
        .page_candidates(&own, None, 10, eligible_before())
        .await
        .unwrap();
    assert!(
        !page.outcomes.is_empty(),
        "a no-op query is not evidence of no writes"
    );
    assert_eq!(snapshot(heat.get(&hot).await.unwrap().unwrap()), hot_before);
    assert_eq!(
        snapshot(heat.get(&linked).await.unwrap().unwrap()),
        linked_before
    );
    assert!(heat.get(&cold).await.unwrap().is_none());
    let access = AccessRepo::new(&fx.db)
        .select(&fx.owner, ReadSelection::UnassignedReview)
        .await
        .unwrap();
    let outcome = fx
        .repo()
        .evidence(
            (&fx.owner, &access),
            &EvidenceSelection {
                focal: manual,
                related: vec![],
            },
            8,
            512,
        )
        .await
        .unwrap();
    assert_eq!(bundle(&outcome).focal.complete_content, "explicitly chosen");
    assert_eq!(
        snapshot(heat.get(&manual_sql).await.unwrap().unwrap()),
        manual_before
    );

    // Positive control: the same snapshots see the columns ordinary access recording changes.
    heat.record_access(&hot, 1.0, 2.0, 1).await.unwrap();
    assert_ne!(snapshot(heat.get(&hot).await.unwrap().unwrap()), hot_before);
}

#[tokio::test]
async fn changed_neighbor_reopens_an_unchanged_focals_assessment() {
    let fx = Fixture::new().await;
    let own = partition(&fx.owner, MemoryScope::Personal);
    fx.personal("10", "original focal text").await;
    fx.personal("20", "original related text").await;
    let cut = eligible_before();
    let first = fx
        .repo()
        .page_candidates(&own, None, 10, cut)
        .await
        .unwrap();
    let baseline = only_bundle(&first);
    assert_eq!(baseline.related.len(), 1);
    let prior = &baseline.related[0];
    fx.assess(baseline).await;
    let unchanged = fx
        .repo()
        .page_candidates(&own, None, 10, cut)
        .await
        .unwrap();
    assert!(
        unchanged.outcomes.is_empty(),
        "recent dismissed evidence is unchanged"
    );
    fx.db.inner().query("UPDATE ONLY $neighbor SET content = 'relationship changed', semantic_revision = 2 RETURN NONE")
        .bind(("neighbor", prior.id.as_record_id().clone())).await.unwrap().check().unwrap();

    let reopened = fx
        .repo()
        .page_candidates(&own, None, 10, cut)
        .await
        .unwrap();
    let next = only_bundle(&reopened);
    assert_eq!(next.focal, baseline.focal);
    assert_eq!(next.related.len(), 1);
    assert_eq!(next.related[0].id, prior.id);
    assert_eq!(next.related[0].complete_content, "relationship changed");
    assert_eq!(next.related[0].version, Version::new(2));
    assert_ne!(next.related[0].semantic_hash, prior.semantic_hash);
}

#[tokio::test]
async fn policy_change_invalidates_the_prior_assessment_and_disable_returns_no_bundle() {
    let fx = Fixture::new().await;
    let own = partition(&fx.owner, MemoryScope::Personal);
    fx.personal("10", "same text under both policies").await;
    let cut = eligible_before();
    let first = fx
        .repo()
        .page_candidates(&own, None, 10, cut)
        .await
        .unwrap();
    let old = only_bundle(&first);
    assert_eq!(old.inference_policy, fx.policy.inference_policy_id);
    fx.assess(old).await;
    assert!(
        fx.repo()
            .page_candidates(&own, None, 10, cut)
            .await
            .unwrap()
            .outcomes
            .is_empty()
    );

    let new = configuration('b');
    fx.repo()
        .reconcile_policy(&fx.system, Some(&new))
        .await
        .unwrap();
    let refreshed = fx
        .repo()
        .page_candidates(&own, None, 10, cut)
        .await
        .unwrap();
    let current = only_bundle(&refreshed);
    assert_eq!(current.focal, old.focal);
    assert_eq!(current.inference_policy, new.inference_policy_id);
    assert_ne!(current.inference_policy, old.inference_policy);

    fx.repo().reconcile_policy(&fx.system, None).await.unwrap();
    let disabled = fx
        .repo()
        .page_candidates(&own, None, 10, cut)
        .await
        .unwrap();
    assert!(
        !disabled
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome, EvidenceOutcome::Ready(_)))
    );
}

#[tokio::test]
async fn manual_projection_returns_bounded_evidence_and_source_attribution() {
    let fx = Fixture::new().await;
    let text = "短い focal with attribution";
    let focal = fx
        .seed(
            "10",
            &fx.owner,
            &MemoryScope::Unassigned,
            text,
            vec![1.0; 8192],
        )
        .await;
    let retained = fx.unassigned("20", "full neighbor").await;
    let omitted = fx.unassigned("30", &"é".repeat(512)).await;
    let access = AccessRepo::new(&fx.db)
        .select(&fx.owner, ReadSelection::UnassignedReview)
        .await
        .unwrap();
    ScopedMemoryRepo::new(fx.db.inner(), &fx.owner, &access)
        .create_provenance(focal.as_record_id(), "user", SEED_TIME)
        .await
        .unwrap();
    let selection = EvidenceSelection {
        focal,
        related: vec![retained.clone(), omitted.clone()],
    };
    let repo = fx.repo();
    let outcome = repo
        .evidence((&fx.owner, &access), &selection, 1, 4096)
        .await
        .unwrap();
    let limited = bundle(&outcome);
    assert_eq!(limited.focal.complete_content, text);
    assert_eq!(limited.focal.tags, vec!["candidate".to_owned()]);
    assert_eq!(limited.focal.provenance.len(), 1);
    assert_eq!(limited.focal.provenance[0].kind, "user");
    assert_eq!(limited.related.len(), 1);
    assert_eq!(limited.related[0].id, retained);
    assert_eq!(limited.related[0].complete_content, "full neighbor");
    assert_eq!(limited.coverage.omitted_neighbors.len(), 1);
    assert_eq!(limited.coverage.omitted_neighbors[0].id, omitted);
    assert_eq!(
        limited.coverage.omitted_neighbors[0].reason,
        OmissionReason::NeighborLimit
    );
    let outcome = repo
        .evidence((&fx.owner, &access), &selection, 2, 768)
        .await
        .unwrap();
    let limited = bundle(&outcome);
    assert_eq!(limited.related.len(), 1);
    assert_eq!(limited.related[0].id, retained);
    assert_eq!(limited.coverage.omitted_neighbors.len(), 1);
    assert_eq!(limited.coverage.omitted_neighbors[0].id, omitted);
    assert_eq!(
        limited.coverage.omitted_neighbors[0].reason,
        OmissionReason::ByteLimit
    );
    let outcome = repo
        .evidence((&fx.owner, &access), &selection, 2, text.chars().count())
        .await
        .unwrap();
    assert!(
        matches!(outcome, EvidenceOutcome::NeedsContext { focal, coverage }
        if focal == selection.focal && !coverage.focal_text_complete)
    );
}

#[tokio::test]
async fn reciprocal_pair_is_emitted_once_even_across_page_boundaries() {
    let fx = Fixture::new().await;
    let own = partition(&fx.owner, MemoryScope::Personal);
    let left = fx.personal("10", "two facts on the same topic").await;
    let right = fx.personal("20", "the symmetric neighbor").await;
    let cut = eligible_before();
    let whole = fx
        .repo()
        .page_candidates(&own, None, 10, cut)
        .await
        .unwrap();
    let paired = only_bundle(&whole);
    assert_eq!(paired.related.len(), 1);
    assert_eq!(
        BTreeSet::from([paired.focal.id.clone(), paired.related[0].id.clone()]),
        BTreeSet::from([left, right])
    );

    let mut cursor = None;
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let page = fx
            .repo()
            .page_candidates(&own, cursor.as_ref(), 1, cut)
            .await
            .unwrap();
        outcomes.extend(page.outcomes);
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert!(
        cursor.is_none(),
        "a suppressed reverse pair must still advance the scan"
    );
    assert_eq!(
        outcomes, whole.outcomes,
        "limit must not change whether A/B or B/A is emitted"
    );
}

#[tokio::test]
async fn partition_cursor_resumes_across_an_insert_with_typed_numeric_and_quoted_ids() {
    let fx = Fixture::new().await;
    let own = partition(&fx.owner, MemoryScope::Personal);
    let oversized = "x".repeat(64 * 1024);
    let first = fx.personal("10", &oversized).await;
    let second = fx.personal("20", &oversized).await;
    let quoted = fx.personal("⟨opaque key with spaces⟩", &oversized).await;
    let cut = eligible_before();
    let baseline = fx
        .repo()
        .page_candidates(&own, None, 10, cut)
        .await
        .unwrap();
    let expected = outcome_ids(&baseline);
    assert_eq!(
        expected.iter().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([first.clone(), second, quoted])
    );
    assert_eq!(
        expected[0], first,
        "fixture: the late insert must precede the initial row"
    );

    let page = fx.repo().page_candidates(&own, None, 1, cut).await.unwrap();
    let mut actual = outcome_ids(&page);
    let mut cursor = page.next;
    let checkpoint = cursor
        .as_ref()
        .expect("bounded first page has a continuation");
    assert_eq!(checkpoint.partition, own);
    assert_eq!(checkpoint.after, first);
    let late = fx.personal("1", &oversized).await;
    let foreign = partition(&fx.other, MemoryScope::Personal);
    assert!(matches!(
        fx.repo()
            .page_candidates(&foreign, Some(checkpoint), 1, cut)
            .await,
        Err(StorageError::InvalidInput(_))
    ));
    for _ in 0..3 {
        let Some(checkpoint) = cursor.as_ref() else {
            break;
        };
        let page = fx
            .repo()
            .page_candidates(&own, Some(checkpoint), 1, cut)
            .await
            .unwrap();
        actual.extend(outcome_ids(&page));
        cursor = page.next;
    }
    assert!(
        cursor.is_none(),
        "an exhausted partition must not keep a continuation"
    );
    assert_eq!(
        actual, expected,
        "no offset drift, repeats, or skipped typed IDs"
    );
    assert!(!actual.contains(&late));
}

#[tokio::test]
async fn manual_unassigned_evidence_reads_only_explicitly_selected_ids() {
    let fx = Fixture::new().await;
    let focal = fx.unassigned("10", "selected focal").await;
    let selected = fx.unassigned("20", "selected related").await;
    fx.unassigned("30", "ambient unassigned with identical embedding")
        .await;
    fx.personal("40", "ambient assigned with identical embedding")
        .await;
    fx.seed(
        "50",
        &fx.other,
        &MemoryScope::Unassigned,
        "another owner",
        vec![1.0, 0.0],
    )
    .await;
    let review = AccessRepo::new(&fx.db)
        .select(&fx.owner, ReadSelection::UnassignedReview)
        .await
        .unwrap();
    let selection = EvidenceSelection {
        focal: focal.clone(),
        related: vec![selected.clone()],
    };
    let outcome = fx
        .repo()
        .evidence((&fx.owner, &review), &selection, 8, 4096)
        .await
        .unwrap();
    let selected_evidence = bundle(&outcome);
    assert_eq!(selected_evidence.focal.id, focal);
    assert_eq!(selected_evidence.related.len(), 1);
    assert_eq!(selected_evidence.related[0].id, selected);
    assert_eq!(
        selected_evidence.related[0].complete_content,
        "selected related"
    );
    assert_eq!(
        selected_evidence.coverage.selection,
        EvidenceSelectionKind::OwnerSelected
    );
    assert!(
        selected_evidence.coverage.omitted_neighbors.is_empty(),
        "ambient IDs are not even omission metadata"
    );
}

#[tokio::test]
async fn manual_unassigned_evidence_refuses_other_contexts_and_out_of_scope_ids() {
    let fx = Fixture::new().await;
    let focal = fx.unassigned("10", "owner's review focal").await;
    let assigned = fx.personal("20", "not part of Unassigned").await;
    let foreign = fx
        .seed(
            "30",
            &fx.other,
            &MemoryScope::Unassigned,
            "foreign Unassigned",
            vec![1.0, 0.0],
        )
        .await;
    let access = AccessRepo::new(&fx.db);
    let review = access
        .select(&fx.owner, ReadSelection::UnassignedReview)
        .await
        .unwrap();
    let assigned_context = access
        .select(&fx.owner, ReadSelection::Assigned)
        .await
        .unwrap();
    let other_review = access
        .select(&fx.other, ReadSelection::UnassignedReview)
        .await
        .unwrap();
    let repo = fx.repo();
    let selection = EvidenceSelection {
        focal: focal.clone(),
        related: vec![],
    };
    for scope in [&assigned_context, &other_review] {
        assert!(matches!(
            repo.evidence((&fx.owner, scope), &selection, 8, 4096).await,
            Err(StorageError::NotFound)
        ));
    }
    for id in [assigned, foreign, fact_id("999")] {
        let selection = EvidenceSelection {
            focal: focal.clone(),
            related: vec![id],
        };
        assert!(
            matches!(
                repo.evidence((&fx.owner, &review), &selection, 8, 4096)
                    .await,
                Err(StorageError::NotFound)
            ),
            "the response must not distinguish foreign, wrong-scope and absent IDs"
        );
    }
}
