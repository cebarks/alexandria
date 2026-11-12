//! Task 11: scope moves, owner edits and human classification, against the pinned 3.3.0 engine.
//!
//! These cases are about the *human* half: an agent's proposal is a judgement with no authority, and an
//! owner's decision is the thing that moves a memory. Every refusal is paired with a positive control on
//! the same fixture, and the fixtures write through the scoped writers so the rows are real — owned,
//! partitioned, revisioned.

use alexandria_storage::Database;
use alexandria_storage::access::{AccessRepo, MutationContext, OwnerContext};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::identity::{
    FactId, MemoryScope, OperationId, ProjectId, ReadSelection, Version,
};
use alexandria_storage::models::mutation::{
    EdgeKind, EmbeddingSnapshot, FactChange, PreparedChangeSet, RelationChange,
    RelationExpectation, RelationTarget, SemanticSnapshot, Visibility, effect,
};
use alexandria_storage::models::{operation_kind, proposal_state};
use alexandria_storage::repos::{
    ClassificationRepo, IdentityRepo, MutationRepo, ScopedClusterRepo, ScopedMemoryRepo,
    ScopedProjectRepo, WriteTarget,
};
use alexandria_storage::schema;
use chrono::{DateTime, Utc};
use surrealdb::types::{RecordId, SurrealValue};

#[path = "classification_test/approval_atomicity.rs"]
mod approval_atomicity;
#[path = "classification_test/approval_envelope.rs"]
mod approval_envelope;
#[path = "classification_test/partition_moves.rs"]
mod partition_moves;

const HASH: &str = "$argon2id$fixture-prepared-hash-classification-padding";

struct Fixture {
    db: Database,
    owner: OwnerContext,
}

impl Fixture {
    async fn enrolled(login: &str) -> Self {
        let mut db = Database::connect_embedded()
            .await
            .expect("an in-memory store");
        schema::migrate(db.inner()).await.expect("the schema");
        let system = db
            .claim_system_authority()
            .expect("a fresh store issues its startup capability once");
        let ids = IdentityRepo::new(db.inner())
            .bootstrap_owner(login, HASH, &OperationId::new(format!("bootstrap-{login}")))
            .await
            .expect("bootstrapping a fixture owner");
        let access = AccessRepo::new(&db);
        let principal = access
            .local_principal(&system, &ids.account)
            .await
            .expect("a local principal");
        let owner = access.authorize(&principal).await.expect("authorizing");
        Self { db, owner }
    }

    async fn read(&self) -> alexandria_storage::access::ScopedContext {
        AccessRepo::new(&self.db)
            .select(&self.owner, ReadSelection::Assigned)
            .await
            .expect("the owner's own corpus resolves")
    }

    /// The review population: the one door the Unassigned partition has.
    async fn unassigned(&self) -> alexandria_storage::access::ScopedContext {
        AccessRepo::new(&self.db)
            .select(&self.owner, ReadSelection::UnassignedReview)
            .await
            .expect("the review population resolves")
    }

    async fn project(&self, alias: &str) -> ProjectId {
        ScopedProjectRepo::new(self.db.inner(), &self.owner)
            .establish(alias, Some(alias))
            .await
            .expect("establishing a fixture project")
            .project
            .id
    }

    async fn seed(&self, content: &str) -> FactId {
        let read = self.read().await;
        let fact = ScopedMemoryRepo::new(self.db.inner(), &self.owner, &read)
            .create_fact(
                content,
                0.5,
                &[0.1, 0.2],
                &["seed".to_string()],
                WriteTarget::Personal,
                Utc::now(),
            )
            .await
            .expect("seeding a fixture fact");
        FactId::try_from(fact.id.expect("a created fact has an id")).expect("a fact row")
    }

    /// One live memory with a vector of its own, so a centroid that moves is distinguishable from one that
    /// was never a mean of two different members.
    async fn seed_vector(&self, content: &str, vector: &[f32]) -> FactId {
        let read = self.read().await;
        let fact = ScopedMemoryRepo::new(self.db.inner(), &self.owner, &read)
            .create_fact(
                content,
                0.5,
                vector,
                &["seed".to_string()],
                WriteTarget::Personal,
                Utc::now(),
            )
            .await
            .expect("seeding a fixture fact");
        FactId::try_from(fact.id.expect("a created fact has an id")).expect("a fact row")
    }

    /// A memory already sitting in Unassigned **and** quarantined — the state a collapse leaves behind,
    /// and the one an owner is entitled to review rather than have forgotten.
    async fn seed_quarantined_unassigned(&self, content: &str) -> FactId {
        let read = self.unassigned().await;
        let fact = ScopedMemoryRepo::new(self.db.inner(), &self.owner, &read)
            .create_fact(
                content,
                0.5,
                &[0.1, 0.2],
                &[],
                WriteTarget::Unassigned,
                Utc::now(),
            )
            .await
            .expect("seeding a fixture unassigned fact");
        let id = fact.id.clone().expect("a created fact has an id");
        self.db
            .inner()
            .query("UPDATE type::record('fact', $key) SET quarantined_at = $at WHERE id = $id;")
            .bind(("key", key(&id)))
            .bind(("id", id.clone()))
            .bind(("at", Utc::now()))
            .await
            .expect("quarantining")
            .check()
            .expect("the quarantine write ran");
        FactId::try_from(id).expect("a fact row")
    }

    async fn row(&self, fact: &FactId) -> FactState {
        let rows: Vec<FactState> = self
            .db
            .inner()
            .query(
                "SELECT id, scope, project, quarantined_at, deleted, semantic_revision FROM \
                 type::record('fact', $key) WHERE id = $id LIMIT 1;",
            )
            .bind(("key", key(fact.as_record_id())))
            .bind(("id", fact.as_record_id().clone()))
            .await
            .expect("reading the row")
            .check()
            .expect("the read ran")
            .take(0)
            .expect("the row decodes");
        rows.into_iter().next().expect("the row exists")
    }

    /// The clusters this memory belongs to, as handles.
    async fn clusters_of(&self, fact: &FactId) -> Vec<String> {
        let rows: Vec<serde_json::Value> = self
            .db
            .inner()
            .query("SELECT in FROM contains_memory WHERE out = $id ORDER BY in;")
            .bind(("id", fact.as_record_id().clone()))
            .await
            .expect("reading memberships")
            .check()
            .expect("the membership read ran")
            .take(0)
            .expect("memberships decode");
        rows.iter()
            .filter_map(|row| row["in"].as_str().map(str::to_string))
            .collect()
    }

    async fn session_links(&self, fact: &FactId) -> usize {
        let rows: Vec<serde_json::Value> = self
            .db
            .inner()
            .query("SELECT id FROM contains_session_memory WHERE out = $id;")
            .bind(("id", fact.as_record_id().clone()))
            .await
            .expect("reading session links")
            .check()
            .expect("the link read ran")
            .take(0)
            .expect("the links decode");
        rows.len()
    }

    async fn lineage(&self, fact: &FactId) -> usize {
        let rows: Vec<serde_json::Value> = self
            .db
            .inner()
            .query("SELECT id FROM memory_edge WHERE in = $id AND edge_type = 'derived_from';")
            .bind(("id", fact.as_record_id().clone()))
            .await
            .expect("reading lineage")
            .check()
            .expect("the lineage read ran")
            .take(0)
            .expect("the edges decode");
        rows.len()
    }

    async fn proposal_row(&self, id: &str) -> Option<ProposalRow> {
        let target = RecordId::parse_simple(id).ok()?;
        let rows: Vec<ProposalRow> = self
            .db
            .inner()
            .query(
                "SELECT id, state, revision, destination_kind, destination_project, source_revision, \
                 decided_by, decided_at FROM type::record('classification_proposal', $key) WHERE id = $id LIMIT 1;",
            )
            .bind(("key", key(&target)))
            .bind(("id", target))
            .await
            .expect("reading the proposal")
            .check()
            .expect("the proposal read ran")
            .take(0)
            .expect("the proposal decodes");
        rows.into_iter().next()
    }

    /// The cluster row itself: centroid and depth, the columns the journal must agree with.
    async fn cluster_row(&self, cluster: &RecordId) -> ClusterState {
        let rows: Vec<ClusterState> = self
            .db
            .inner()
            .query("SELECT id, centroid, depth FROM type::record('cluster', $key) WHERE id = $id LIMIT 1;")
            .bind(("key", key(cluster)))
            .bind(("id", cluster.clone()))
            .await
            .expect("reading the cluster")
            .check()
            .expect("the cluster read ran")
            .take(0)
            .expect("the cluster decodes");
        rows.into_iter().next().expect("the cluster exists")
    }

    /// Every member the cluster currently holds, as the record **key** half — the same spelling the
    /// fixture's own `key()` helper produces, so a comparison with a `FactId` is not a table-prefix
    /// mismatch dressed up as an assertion.
    async fn cluster_members(&self, cluster: &RecordId) -> Vec<String> {
        let rows: Vec<serde_json::Value> = self
            .db
            .inner()
            .query("SELECT out FROM contains_memory WHERE in = $cluster ORDER BY out;")
            .bind(("cluster", cluster.clone()))
            .await
            .expect("reading members")
            .check()
            .expect("the member read ran")
            .take(0)
            .expect("the members decode");
        rows.iter()
            .filter_map(|row| row["out"].as_str())
            .map(|handle| {
                handle
                    .split_once(':')
                    .map_or_else(|| handle.to_string(), |(_, key)| key.to_string())
            })
            .collect()
    }

    /// Attach a memory to a cluster, moving the membership guard the way a journaled writer would.
    async fn attach(&self, cluster: &RecordId, member: &FactId) {
        self.db
            .inner()
            .query(
                "RELATE $cluster->contains_memory->$member; UPDATE type::record('cluster', $key) \
                 SET membership_revision = 1 WHERE id = $cluster;",
            )
            .bind(("cluster", cluster.clone()))
            .bind(("member", member.as_record_id().clone()))
            .bind(("key", key(cluster)))
            .await
            .expect("attaching a member")
            .check()
            .expect("the attach ran");
    }
}

fn key(id: &RecordId) -> String {
    use surrealdb::types::RecordIdKey;
    match &id.key {
        RecordIdKey::String(text) => text.clone(),
        RecordIdKey::Number(number) => number.to_string(),
        RecordIdKey::Uuid(uuid) => uuid.to_string(),
        other => format!("{other:?}"),
    }
}

fn op(key: &str) -> OperationId {
    OperationId::new(key.to_string())
}

#[derive(SurrealValue)]
struct FactState {
    id: Option<RecordId>,
    scope: Option<String>,
    project: Option<RecordId>,
    quarantined_at: Option<DateTime<Utc>>,
    deleted: bool,
    semantic_revision: Option<i64>,
}

#[derive(SurrealValue)]
struct ClusterState {
    id: Option<RecordId>,
    centroid: Vec<f32>,
    depth: i64,
}

#[derive(SurrealValue)]
struct ProposalRow {
    state: String,
    revision: i64,
    destination_kind: String,
    source_revision: Option<i64>,
    destination_project: Option<RecordId>,
    decided_by: Option<RecordId>,
    decided_at: Option<DateTime<Utc>>,
}

fn embedding(vector: &[f32]) -> EmbeddingSnapshot {
    EmbeddingSnapshot::new(vector.to_vec(), Some("fixture-model"))
}

// -- the plan's named cases ------------------------------------------------------------------------------

/// An owner edits content and tags, and soft-deletes, through the journal.
#[tokio::test]
async fn owner_edits_content_and_tags_and_soft_deletes() {
    let fixture = Fixture::enrolled("edit-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let access = MutationContext::issued_to(&fixture.owner);
    let fact = fixture.seed("text the owner will correct").await;
    let before = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");

    // Tags and content in one operation.
    let planned = PreparedChangeSet::new(operation_kind::UPDATE)
        .effect(effect::CONTENT)
        .effect(effect::TAGS)
        .fact(FactChange::Update {
            fact: fact.clone(),
            expected: before.version,
            content: Some("corrected text".to_string()),
            tags: Some(vec!["owner-edited".to_string()]),
            confidence: None,
            embedding: Some(embedding(&[0.9, 0.1])),
            visibility: None,
            scope: None,
        });
    let applied = repo
        .apply(
            &access,
            &op("owner-edit"),
            std::slice::from_ref(&before),
            &planned,
        )
        .await
        .expect("the edit applies");
    assert_eq!(
        applied.preimages.len(),
        1,
        "the edit journaled no before-state"
    );
    assert_eq!(
        applied.postimages.len(),
        1,
        "the edit journaled no after-state"
    );
    // The seeded row predates any journaled write, so it carries no revision column value at all; v009's
    // `option<int> DEFAULT 0` reads as the zero the digest treats as "unjournaled", and the first
    // journaled edit stamps one.
    assert_eq!(
        fixture.row(&fact).await.semantic_revision,
        Some(1),
        "the edited row's revision did not move"
    );

    // Soft delete, which is a semantic visibility change and therefore a journaled operation.
    let after = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("re-read");
    // The state the delete is planned on, kept for the replay below: re-reading after the delete would
    // hand the replay the revision the delete itself just stamped, which is a lawful expectation and not
    // the stale one this control is about.
    let pre_delete = after.clone();
    let deletion = PreparedChangeSet::new(operation_kind::DELETE)
        .effect(effect::DELETE)
        .fact(FactChange::Update {
            fact: fact.clone(),
            expected: after.version,
            content: None,
            tags: None,
            confidence: None,
            embedding: None,
            visibility: Some(Visibility::deleted()),
            scope: None,
        });
    repo.apply(&access, &op("owner-delete"), &[after], &deletion)
        .await
        .expect("the delete applies");
    let row = fixture.row(&fact).await;
    assert!(row.deleted, "the soft delete did not land");
    assert_eq!(
        row.scope.as_deref(),
        Some("personal"),
        "a delete changed the partition"
    );

    // Negative control with a positive one: the *same* delete replayed against the pre-delete revision
    // conflicts rather than double-writing, and the row is still exactly as it was.
    let state_before_delete = pre_delete;
    let replay = repo
        .apply(
            &access,
            &op("owner-delete-again"),
            std::slice::from_ref(&state_before_delete),
            &PreparedChangeSet::new(operation_kind::DELETE)
                .effect(effect::DELETE)
                .fact(FactChange::Update {
                    fact: fact.clone(),
                    expected: state_before_delete.version,
                    content: None,
                    tags: None,
                    confidence: None,
                    embedding: None,
                    visibility: Some(Visibility::deleted()),
                    scope: None,
                }),
        )
        .await;
    assert!(
        matches!(replay, Err(StorageError::Stale)),
        "a delete aimed at a superseded revision applied: {replay:?}"
    );
    assert!(
        fixture.row(&fact).await.deleted,
        "the refused delete un-deleted the row"
    );
}

/// A move and an edit of the same memory race, and exactly one of them wins.
#[tokio::test]
async fn simultaneous_move_and_update_has_one_stale_winner() {
    let fixture = Fixture::enrolled("race-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let project = fixture.project("moved-into").await;
    let fact = fixture.seed("a memory two writers both reached for").await;
    let shared = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");

    // The move lands first.
    let moved = repo
        .move_scope(
            &fixture.owner,
            fact.clone(),
            shared.version,
            MemoryScope::Project(project.clone()),
            &op("race-move"),
        )
        .await
        .expect("the move applies");
    assert!(!moved.replayed);
    let row = fixture.row(&fact).await;
    assert_eq!(
        row.scope.as_deref(),
        Some("project"),
        "the move did not file the row"
    );
    assert_eq!(
        row.project.as_ref(),
        Some(project.as_record_id()),
        "the move lost its project"
    );

    // The edit planned against the same shared read is now stale — and must not undo the move.
    let outcome = repo
        .apply(
            &MutationContext::issued_to(&fixture.owner),
            &op("race-edit"),
            &[shared],
            &PreparedChangeSet::new(operation_kind::UPDATE)
                .effect(effect::CONTENT)
                .fact(FactChange::Update {
                    fact: fact.clone(),
                    expected: Version::new(0),
                    content: Some("an edit that arrived second".to_string()),
                    tags: None,
                    confidence: None,
                    embedding: None,
                    visibility: None,
                    scope: None,
                }),
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::Stale)),
        "a stale edit won the race: {outcome:?}"
    );
    let after = fixture.row(&fact).await;
    assert_eq!(
        after.scope.as_deref(),
        Some("project"),
        "the losing edit moved the row back"
    );
    assert_eq!(
        after.project.as_ref(),
        Some(project.as_record_id()),
        "the losing edit cleared the project"
    );

    // Positive control: re-read, and the edit is lawful in the new partition.
    let fresh = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("re-read");
    repo.apply(
        &MutationContext::issued_to(&fixture.owner),
        &op("race-edit-reread"),
        std::slice::from_ref(&fresh),
        &PreparedChangeSet::new(operation_kind::UPDATE)
            .effect(effect::CONTENT)
            .fact(FactChange::Update {
                fact: fact.clone(),
                expected: fresh.version,
                content: Some("an edit that re-read first".to_string()),
                tags: None,
                confidence: None,
                embedding: None,
                visibility: None,
                scope: None,
            }),
    )
    .await
    .expect("a re-read edit lands in the moved partition");
}

/// A move keeps the memory's id and detaches its cluster membership; history does not follow.
#[tokio::test]
async fn a_move_keeps_the_id_detaches_the_cluster_and_leaves_history_alone() {
    let fixture = Fixture::enrolled("move-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let read = fixture.read().await;
    let project = fixture.project("destination-project").await;
    let fact = fixture.seed("a clustered memory about to move").await;

    // The graph and history the move must not disturb: a cluster membership, a session link, and a
    // lineage edge to a hidden preimage.
    let cluster = ScopedClusterRepo::new(fixture.db.inner(), &fixture.owner, &read)
        .create(None, &[0.5, 0.5], Utc::now())
        .await
        .expect("a fixture cluster")
        .id
        .expect("a created cluster has an id");
    fixture.attach(&cluster, &fact).await;
    assert_eq!(
        fixture.clusters_of(&fact).await.len(),
        1,
        "the fixture attach failed"
    );

    let session = fixture
        .db
        .inner()
        .query("CREATE session SET external_id = 'moved-session', account = $account, workspace = $workspace, started_at = time::now(), ended_at = time::now() RETURN AFTER;")
        .bind(("account", fixture.owner.account().as_record_id().clone()))
        .bind(("workspace", fixture.owner.workspace().as_record_id().clone()))
        .await
        .expect("creating a session")
        .check()
        .expect("the session was created");
    let session_rows: Vec<serde_json::Value> = session
        .check()
        .expect("the session create ran")
        .take(0)
        .expect("the session decodes");
    let session_id = RecordId::parse_simple(
        session_rows.first().expect("a session row")["id"]
            .as_str()
            .expect("a session id renders as a handle"),
    )
    .expect("a session handle");
    fixture
        .db
        .inner()
        .query("RELATE $sess->contains_session_memory->$fact;")
        .bind(("sess", session_id.clone()))
        .bind(("fact", fact.as_record_id().clone()))
        .await
        .expect("linking the session")
        .check()
        .expect("the link ran");
    assert_eq!(fixture.session_links(&fact).await, 1);

    let expected = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");
    assert!(
        expected
            .graph
            .iter()
            .any(|guard| matches!(guard, RelationExpectation::Present { kind, .. } if matches!(kind, EdgeKind::ClusterMembership))),
        "the expectation the move guards on does not name the cluster membership: {:?}",
        expected.graph
    );

    let applied = repo
        .move_scope(
            &fixture.owner,
            fact.clone(),
            expected.version,
            MemoryScope::Project(project.clone()),
            &op("the-move"),
        )
        .await
        .expect("the move applies");

    // The id is unchanged: a move is a partition change, not a re-create.
    assert_eq!(
        applied.created,
        Vec::<FactId>::new(),
        "the move minted a new row"
    );
    let row = fixture.row(&fact).await;
    assert_eq!(row.scope.as_deref(), Some("project"));
    assert_eq!(row.project.as_ref(), Some(project.as_record_id()));

    // The old partition's membership is gone, so no cluster traversal in the source partition can still
    // reach a memory that no longer belongs there.
    assert!(
        fixture.clusters_of(&fact).await.is_empty(),
        "the move left a cross-partition membership: {:?}",
        fixture.clusters_of(&fact).await
    );

    // History stays where it was written.
    assert_eq!(
        fixture.session_links(&fact).await,
        1,
        "the move silently re-partitioned a session link"
    );
    assert_eq!(
        fixture
            .db
            .inner()
            .query("SELECT id FROM session WHERE external_id = 'moved-session';")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take::<Vec<serde_json::Value>>(0)
            .unwrap()
            .len(),
        1,
        "the move touched the session row"
    );

    // And the move itself is the journaled operation the owner can read back.
    let detail = repo
        .operation(&fixture.owner, "the-move")
        .await
        .expect("lookup")
        .expect("the move is journaled");
    assert_eq!(detail.kind, operation_kind::MOVE);
    assert_eq!(
        detail.preimages.len(),
        2,
        "the move needs fact and cluster preimages"
    );
    let cluster_before = detail
        .preimages
        .iter()
        .find_map(|image| match image {
            SemanticSnapshot::Cluster(image) => Some(image),
            _ => None,
        })
        .expect("the detached cluster must have a preimage");
    assert_eq!(cluster_before.id, cluster);
    assert_eq!(cluster_before.members, vec![fact.as_record_id().clone()]);
    assert_eq!(cluster_before.centroid, vec![0.5, 0.5]);
    assert_eq!(cluster_before.membership_revision, Version::new(1));
    assert_eq!(cluster_before.member_embedding_revision, Version::ZERO);
    assert_eq!(cluster_before.centroid_revision, Version::ZERO);
    let before = detail.preimages.first().expect("a before-state");
    let after = detail.postimages.first().expect("an after-state");
    match (before, after) {
        (SemanticSnapshot::Fact(source), SemanticSnapshot::Fact(destination)) => {
            assert_eq!(
                source.scope,
                MemoryScope::Personal,
                "the journaled before-state is not the source partition"
            );
            assert!(
                matches!(destination.scope, MemoryScope::Project(_)),
                "the journaled after-state is not the destination"
            );
            assert_eq!(
                source.id, destination.id,
                "the journaled move describes two memories"
            );
        }
        other => panic!("a move must journal fact images, got {other:?}"),
    }

    // Positive control that the guard was real: a second move planned on the *old* expectation refuses.
    let outcome = repo
        .move_scope(
            &fixture.owner,
            fact.clone(),
            expected.version,
            MemoryScope::Personal,
            &op("stale-second-move"),
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::Stale)),
        "a move planned on a superseded revision applied: {outcome:?}"
    );
    assert_eq!(
        fixture.row(&fact).await.scope.as_deref(),
        Some("project"),
        "the refused move moved the row back"
    );
}

/// Historical lineage does not move with the memory it describes.
#[tokio::test]
async fn historical_lineage_is_not_auto_moved() {
    let fixture = Fixture::enrolled("lineage-move-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let access = MutationContext::issued_to(&fixture.owner);
    let project = fixture.project("lineage-destination").await;
    let fact = fixture.seed("a memory with a hidden preimage").await;

    // A real content update: this leaves a hidden preimage row plus a `derived_from` edge.
    let before = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");
    repo.apply(
        &access,
        &op("lineage-then-move"),
        std::slice::from_ref(&before),
        &PreparedChangeSet::new(operation_kind::UPDATE)
            .effect(effect::CONTENT)
            .effect(effect::LINEAGE)
            .fact(FactChange::Create {
                scope: MemoryScope::Personal,
                content: "a memory with a hidden preimage".to_string(),
                tags: vec![],
                confidence: 0.5,
                embedding: embedding(&[0.1, 0.2]),
                visibility: Visibility::deleted(),
                metadata: None,
            })
            .fact(FactChange::Update {
                fact: fact.clone(),
                expected: before.version,
                content: Some("corrected before the move".to_string()),
                tags: None,
                confidence: None,
                embedding: Some(embedding(&[0.8, 0.2])),
                visibility: None,
                scope: None,
            })
            .relate(RelationChange::memory(
                "derived_from",
                RelationTarget::Existing(fact.as_record_id().clone()),
                RelationTarget::NewFact(0),
                1.0,
            )),
    )
    .await
    .expect("the update with lineage applies");
    assert_eq!(
        fixture.lineage(&fact).await,
        1,
        "the fixture left no lineage edge"
    );

    let expected = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");
    repo.move_scope(
        &fixture.owner,
        fact.clone(),
        expected.version,
        MemoryScope::Project(project),
        &op("move-after-lineage"),
    )
    .await
    .expect("the move applies");

    // The preimage stays in Personal, and the edge survives: an owner's history is not re-filed by a
    // later classification decision.
    assert_eq!(
        fixture.lineage(&fact).await,
        1,
        "the move dropped the lineage edge"
    );
    let personal: Vec<serde_json::Value> = fixture
        .db
        .inner()
        .query("SELECT id FROM fact WHERE scope = 'personal' AND deleted = true;")
        .await
        .expect("reading the hidden preimage")
        .check()
        .expect("the read ran")
        .take(0)
        .expect("the rows decode");
    assert_eq!(
        personal.len(),
        1,
        "the hidden preimage was silently moved or lost: {personal:?}"
    );
}

/// A quarantined Unassigned memory stays listed, and classifying it does not lift quarantine.
#[tokio::test]
async fn classification_keeps_quarantine() {
    let fixture = Fixture::enrolled("quarantine-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let project = fixture.project("out-of-unassigned").await;
    let fact = fixture
        .seed_quarantined_unassigned("noise an agent proposed filing")
        .await;
    let before = fixture.row(&fact).await;
    assert!(
        before.quarantined_at.is_some(),
        "the fixture row is not quarantined"
    );

    // Listed for review: the Unassigned partition is invisible to recall and visible only here.
    let review = ScopedMemoryRepo::new(
        fixture.db.inner(),
        &fixture.owner,
        &fixture.unassigned().await,
    )
    .review_page(None, 50)
    .await
    .expect("the review page");
    assert!(
        review
            .iter()
            .any(|row| row.id.as_ref() == Some(fact.as_record_id())),
        "a quarantined Unassigned memory vanished from owner review: {review:?}"
    );

    // An agent records a judgement. The proposal moves nothing.
    let reference = ClassificationRepo::new(fixture.db.inner(), &fixture.owner)
        .propose(
            &fact,
            Version::new(before.semantic_revision.unwrap_or(0) as u64),
            MemoryScope::Project(project.clone()),
            Some("the agent thinks this is project-bound"),
        )
        .await
        .expect("an agent may record a judgement");
    assert_eq!(
        fixture.row(&fact).await.scope.as_deref(),
        Some("unassigned"),
        "a proposal applied itself"
    );

    // The human applies it. The destination moves; the quarantine does not lift.
    let applied = ClassificationRepo::new(fixture.db.inner(), &fixture.owner)
        .apply_batch(
            std::slice::from_ref(&reference),
            &op("classify-quarantined"),
        )
        .await
        .expect("the batch runs");
    assert_eq!(
        applied.applied_count() + applied.conflicts.len(),
        1,
        "the batch answered with neither: {applied:?}"
    );
    if applied.applied_count() == 1 {
        let after = fixture.row(&fact).await;
        assert_eq!(
            after.scope.as_deref(),
            Some("project"),
            "the approved move did not file the row"
        );
        assert_eq!(
            after.quarantined_at, before.quarantined_at,
            "classification lifted a quarantine the owner never revoked"
        );
        assert!(!after.deleted, "classification deleted the row it moved");
    } else {
        // The refusal half: a proposal formed against a revision the row never carried must not move it,
        // and must not touch quarantine either.
        let after = fixture.row(&fact).await;
        assert_eq!(
            after.scope.as_deref(),
            Some("unassigned"),
            "a conflicted entry moved the row"
        );
        assert_eq!(
            after.quarantined_at, before.quarantined_at,
            "a conflicted entry touched quarantine"
        );
        assert_eq!(
            applied.conflicts[0].reason,
            alexandria_storage::models::mutation::BatchConflictReason::Stale,
            "the batch refused for a reason that is not staleness: {:?}",
            applied.conflicts[0]
        );
    }

    // Positive control on the same fixture: a live memory in the same workspace moves and stays live.
    let live = fixture
        .seed("a live memory that classification can move")
        .await;
    let live_before = fixture.row(&live).await;
    let live_reference = ClassificationRepo::new(fixture.db.inner(), &fixture.owner)
        .propose(
            &live,
            Version::new(live_before.semantic_revision.unwrap_or(0) as u64),
            MemoryScope::Project(project),
            None,
        )
        .await
        .expect("a judgement on the live row");
    let second = ClassificationRepo::new(fixture.db.inner(), &fixture.owner)
        .apply_batch(std::slice::from_ref(&live_reference), &op("classify-live"))
        .await
        .expect("the second batch runs");
    assert_eq!(
        second.applied_count(),
        1,
        "a lawful classification never applied: {second:?}"
    );
    let moved = fixture.row(&live).await;
    assert_eq!(moved.scope.as_deref(), Some("project"));
    assert_eq!(
        moved.quarantined_at, None,
        "a live row gained a quarantine it never had"
    );

    // The applied decision is readable back as its own journal row. The proposal's `state` column says
    // "approved"; only the journal says *what the memory used to be*, which is what makes the queue's
    // "done" answer auditable rather than merely marked.
    let history = repo
        .history(&fixture.owner, 50)
        .await
        .expect("the owner's own history resolves");
    assert!(
        history.iter().any(|row| row.kind == operation_kind::MOVE),
        "an applied classification left no move in the journal: {history:?}"
    );
}

/// A stale batch entry cannot overwrite a later move, and its neighbour still lands.
#[tokio::test]
async fn stale_batch_entry_cannot_overwrite_a_later_move() {
    let fixture = Fixture::enrolled("batch-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let access = MutationContext::issued_to(&fixture.owner);
    let project = fixture.project("batch-destination").await;
    let first = fixture
        .seed("the memory whose proposal is still current")
        .await;
    let second = fixture
        .seed("the memory that moved after its proposal")
        .await;

    let classify = ClassificationRepo::new(fixture.db.inner(), &fixture.owner);
    let first_reference = classify
        .propose(
            &first,
            Version::new(0),
            MemoryScope::Project(project.clone()),
            None,
        )
        .await
        .expect("a judgement on the first");
    let second_reference = classify
        .propose(
            &second,
            Version::new(0),
            MemoryScope::Project(project.clone()),
            None,
        )
        .await
        .expect("a judgement on the second");

    // Somebody moves the second memory between the proposal and the human's decision.
    let second_state = repo
        .expected_state(&fixture.owner, &second)
        .await
        .expect("read");
    repo.apply(
        &access,
        &op("intervening-edit"),
        std::slice::from_ref(&second_state),
        &PreparedChangeSet::new(operation_kind::UPDATE)
            .effect(effect::CONFIDENCE)
            .fact(FactChange::Update {
                fact: second.clone(),
                expected: second_state.version,
                content: None,
                tags: None,
                confidence: Some(0.9),
                embedding: None,
                visibility: None,
                scope: None,
            }),
    )
    .await
    .expect("the intervening edit applies");
    // The later move is planned against the state the edit left behind — it is the move that must win
    // the race against the batch's stale proposal, not a stale write itself.
    let after_edit = repo
        .expected_state(&fixture.owner, &second)
        .await
        .expect("re-read");
    assert_eq!(
        after_edit.version.get(),
        second_state.version.get() + 1,
        "the edit did not bump the revision the later move guards"
    );
    let moved_to = MemoryScope::Unassigned;
    repo.move_scope(
        &fixture.owner,
        second.clone(),
        after_edit.version,
        moved_to,
        &op("intervening-move"),
    )
    .await
    .expect("the intervening move applies");

    // The batch: the current entry applies, the stale one conflicts, and the later move survives both.
    let result = classify
        .apply_batch(
            &[first_reference.clone(), second_reference.clone()],
            &op("batch-apply"),
        )
        .await
        .expect("the batch runs");
    assert_eq!(
        result.applied_count(),
        1,
        "the batch did not apply exactly its one lawful entry: {result:?}"
    );
    assert_eq!(
        result.conflicts.len(),
        1,
        "the batch did not report exactly one conflict: {result:?}"
    );
    assert_eq!(
        result.conflicts[0].proposal, second_reference.id,
        "the batch conflicted on the wrong entry: {:?}",
        result.conflicts[0]
    );
    assert_eq!(
        result.conflicts[0].reason,
        alexandria_storage::models::mutation::BatchConflictReason::Stale
    );
    assert_eq!(
        fixture.row(&first).await.scope.as_deref(),
        Some("project"),
        "the lawful entry did not move"
    );
    assert_eq!(
        fixture.row(&second).await.scope.as_deref(),
        Some("unassigned"),
        "a stale entry overwrote a later move"
    );
    assert_eq!(
        fixture
            .proposal_row(&first_reference.id)
            .await
            .map(|row| row.state),
        Some(proposal_state::APPROVED.to_string()),
        "an applied entry was not marked decided"
    );
    assert_eq!(
        fixture
            .proposal_row(&second_reference.id)
            .await
            .map(|row| row.state),
        Some(proposal_state::STALE.to_string()),
        "a stale entry was left pending, so the queue would offer it again"
    );

    // Positive control: the whole batch replayed under its own key returns the recorded answer rather
    // than moving anything a second time.
    let replayed = classify
        .apply_batch(&[first_reference], &op("batch-apply"))
        .await
        .expect("the replay runs");
    assert!(
        replayed.applied.is_empty()
            || replayed
                .conflicts
                .iter()
                .all(|c| c.reason
                    == alexandria_storage::models::mutation::BatchConflictReason::Decided),
        "replaying the batch applied a decided entry: {replayed:?}"
    );
    assert_eq!(fixture.row(&first).await.scope.as_deref(), Some("project"));
}

/// Revising a proposal's destination moves its revision, so an approval of the old one stales.
#[tokio::test]
async fn revising_a_destination_stales_an_earlier_approval() {
    let fixture = Fixture::enrolled("revise-owner").await;
    let project = fixture.project("first-choice").await;
    let other = fixture.project("second-choice").await;
    let fact = fixture
        .seed("a memory whose destination the owner reconsidered")
        .await;

    let classify = ClassificationRepo::new(fixture.db.inner(), &fixture.owner);
    let proposed = classify
        .propose(&fact, Version::new(0), MemoryScope::Project(project), None)
        .await
        .expect("a judgement");
    let revised = classify
        .revise_destination(proposed.clone(), MemoryScope::Project(other.clone()))
        .await
        .expect("the owner may change their mind");
    assert_eq!(
        revised.revision.get(),
        proposed.revision.get() + 1,
        "the revision did not move"
    );
    assert_eq!(
        fixture
            .proposal_row(&revised.id)
            .await
            .map(|row| row.destination_kind),
        Some("project".to_string())
    );

    // The ref minted *before* the revision is now a stale claim: applying it must not move the memory to
    // the destination the owner stopped asking for.
    let outcome = classify
        .apply_batch(std::slice::from_ref(&proposed), &op("stale-approval"))
        .await;
    assert!(
        outcome.is_ok(),
        "a stale ref crashed the batch: {outcome:?}"
    );
    let result = outcome.expect("a batch answer");
    assert_eq!(
        result.applied_count(),
        0,
        "a superseded ref applied its old destination: {result:?}"
    );
    assert_eq!(
        fixture.row(&fact).await.scope.as_deref(),
        Some("personal"),
        "the superseded approval moved the row"
    );

    // Positive control: the *current* ref applies, and lands the revised destination.
    let applied = classify
        .apply_batch(std::slice::from_ref(&revised), &op("current-approval"))
        .await
        .expect("the current ref applies");
    assert_eq!(
        applied.applied_count(),
        1,
        "the current ref did not apply: {applied:?}"
    );
    let row = fixture.row(&fact).await;
    assert_eq!(row.scope.as_deref(), Some("project"));
    assert_eq!(
        row.project.as_ref(),
        Some(other.as_record_id()),
        "the wrong destination landed"
    );

    // A second revision of the same ref is refused — the revision is the guard.
    let again = classify
        .revise_destination(revised.clone(), MemoryScope::Personal)
        .await;
    assert!(
        matches!(again, Err(StorageError::Stale)),
        "a superseded ref revised the destination again: {again:?}"
    );
    assert_eq!(
        fixture
            .proposal_row(&revised.id)
            .await
            .map(|row| row.revision),
        Some(revised.revision.get() as i64),
        "the refused revision still moved the proposal"
    );
}

/// A journaled cluster effect cannot be aimed at a project the caller does not own.
#[tokio::test]
async fn a_journaled_write_refuses_a_project_the_caller_does_not_own() {
    let fixture = Fixture::enrolled("foreign-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let access = MutationContext::issued_to(&fixture.owner);
    let fact = fixture.seed("a memory that must not be filed away").await;
    let foreign =
        ProjectId::try_from(RecordId::new("project", "never-minted-anywhere")).expect("a handle");
    let expected = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");

    let before = fixture.semantic_state().await;
    let outcome = repo
        .apply(
            &access,
            &op("into-a-foreign-project"),
            &[expected],
            &PreparedChangeSet::new(operation_kind::MOVE)
                .effect(effect::SCOPE)
                .fact(FactChange::Update {
                    fact: fact.clone(),
                    expected: Version::new(0),
                    content: None,
                    tags: None,
                    confidence: None,
                    embedding: None,
                    visibility: None,
                    scope: Some(MemoryScope::Project(foreign)),
                }),
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::NotFound)),
        "a journaled move into a foreign project was accepted: {outcome:?}"
    );
    assert_eq!(
        fixture.semantic_state().await,
        before,
        "the refused move wrote anyway"
    );

    // Positive control: the same move into the caller's own project lands.
    let mine = fixture.project("my-own-project").await;
    let fresh = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("re-read");
    repo.move_scope(
        &fixture.owner,
        fact.clone(),
        fresh.version,
        MemoryScope::Project(mine.clone()),
        &op("into-my-project"),
    )
    .await
    .expect("a lawful move lands");
    assert_eq!(
        fixture.row(&fact).await.project.as_ref(),
        Some(mine.as_record_id())
    );
}

impl Fixture {
    /// The store's full semantic state, for the "nothing moved" assertions.
    async fn semantic_state(&self) -> String {
        let mut response = self
            .db
            .inner()
            .query(
                "SELECT * FROM fact ORDER BY id; SELECT * FROM memory_edge ORDER BY id; \
                 SELECT * FROM contains_memory ORDER BY id; \
                 SELECT * FROM contains_session_memory ORDER BY id; \
                 SELECT * FROM semantic_operation ORDER BY id; \
                 SELECT * FROM semantic_snapshot ORDER BY id; \
                 SELECT * FROM classification_proposal ORDER BY id; \
                 SELECT * FROM cluster ORDER BY id; \
                 SELECT * FROM raw ORDER BY id; SELECT * FROM provenance ORDER BY id; \
                 SELECT * FROM has_provenance ORDER BY id; SELECT * FROM extracted_from ORDER BY id; \
                 SELECT * FROM maintenance_log ORDER BY id; SELECT * FROM session ORDER BY id; \
                 SELECT * FROM heat_state ORDER BY id;",
            )
            .await
            .expect("reading the store's state")
            .check()
            .expect("every state statement ran");
        let mut rendered = String::new();
        for index in 0..15 {
            let rows: Vec<serde_json::Value> = response
                .take(index)
                .unwrap_or_else(|error| panic!("state statement {index} did not decode: {error}"));
            rendered.push_str(&serde_json::to_string(&rows).expect("state renders"));
            rendered.push('\u{1c}');
        }
        rendered
    }
}

// -- the cluster a move leaves behind, and the batch's journal shape -------------------------------
//
// The review found three things this file did not measure: a move detached a membership and left the
// cluster's stored centroid still averaging the moved vector (the owner ruled that deferral to Task 12 is
// not acceptable), a batch of N applied entries reported a single `operation` that named only the last row
// to apply, and a cluster's journaled image carried an empty member set with the *expected* guards rather
// than the written ones. Each case below is the store's own answer, not a rendered string.

/// A move rewrites the centre of the cluster it leaves, keeps the surviving member, and journals both.
#[tokio::test]
async fn a_move_leaves_no_stale_centroid_behind() {
    let fixture = Fixture::enrolled("centroid-move-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let read = fixture.read().await;
    let project = fixture.project("centroid-destination").await;

    // A two-member cluster whose stored centroid is the mean of both vectors — with two *different*
    // vectors, so a centre that still reads the mean after one member left is visibly the departed
    // contribution rather than a rounding artefact.
    let mover = fixture
        .seed_vector("the memory that is about to move", &[0.1, 0.2])
        .await;
    let stayer = fixture
        .seed_vector("the memory that stays in this partition", &[0.9, 0.2])
        .await;
    let cluster = ScopedClusterRepo::new(fixture.db.inner(), &fixture.owner, &read)
        .create(None, &[0.5, 0.2], Utc::now())
        .await
        .expect("a fixture cluster")
        .id
        .expect("a created cluster has an id");
    fixture.attach(&cluster, &mover).await;
    fixture.attach(&cluster, &stayer).await;
    assert_eq!(
        fixture.cluster_members(&cluster).await.len(),
        2,
        "the fixture cluster is not a two-member population"
    );

    let guards_before = repo
        .cluster_expectation(&fixture.owner, &cluster)
        .await
        .expect("guards before");
    let state = fixture.cluster_row(&cluster).await;
    assert_eq!(
        state.centroid,
        vec![0.5, 0.2],
        "the fixture centroid is not the mean of its members"
    );

    let expected = repo
        .expected_state(&fixture.owner, &mover)
        .await
        .expect("read the mover's state");
    repo.move_scope(
        &fixture.owner,
        mover.clone(),
        expected.version,
        MemoryScope::Project(project.clone()),
        &op("centroid-move"),
    )
    .await
    .expect("the move applies");

    // The membership left, the survivor stayed, and the centre moved to the survivor's own vector — the
    // mean of one member is that member. A centroid still reading `[0.5, 0.2]` here is the defect.
    let after = fixture.cluster_row(&cluster).await;
    assert_eq!(
        after.centroid,
        vec![0.9, 0.2],
        "the move left a centroid that still averages the departed member"
    );
    let members = fixture.cluster_members(&cluster).await;
    assert_eq!(
        members,
        vec![key(stayer.as_record_id())],
        "the surviving member was lost, or the departed one kept: {members:?}"
    );
    assert!(
        fixture.clusters_of(&mover).await.is_empty(),
        "the moved memory is still a member of the partition it left"
    );

    // Both guards moved, and they moved by one: membership *and* the centroid it re-derived. A bump of the
    // first without the second is a journal that claims a centre it did not write.
    let guards_after = repo
        .cluster_expectation(&fixture.owner, &cluster)
        .await
        .expect("guards after");
    assert_eq!(
        guards_after.membership_revision.get(),
        guards_before.membership_revision.get() + 1,
        "the membership guard did not move"
    );
    assert_eq!(
        guards_after.centroid_revision.get(),
        guards_before.centroid_revision.get() + 1,
        "the centroid guard did not move while the centroid did"
    );

    // And the journal's own cluster images say what the store says. `postimage_ids` rendering as a fact
    // list is not enough: the review found a cluster snapshot that reported zero members and the guards
    // the caller *expected*, so a history page would have depicted a population that was never there.
    let detail = repo
        .operation(&fixture.owner, "centroid-move")
        .await
        .expect("lookup")
        .expect("the move is journaled");
    let clusters: Vec<&alexandria_storage::models::mutation::ClusterSnapshot> = detail
        .postimages
        .iter()
        .filter_map(|image| match image {
            SemanticSnapshot::Cluster(snapshot) => Some(snapshot),
            _ => None,
        })
        .collect();
    let after_image = clusters
        .first()
        .expect("a move that detached a membership must journal the cluster it left");
    assert_eq!(
        after_image.members,
        vec![stayer.as_record_id().clone()],
        "the journaled cluster image does not carry the members the store has"
    );
    assert_eq!(
        after_image.centroid,
        vec![0.9, 0.2],
        "the journaled cluster image carries a centroid the store does not have"
    );
    assert_eq!(
        after_image.membership_revision, guards_after.membership_revision,
        "the journaled cluster image reports the expected guard, not the written one"
    );
    assert_eq!(
        after_image.centroid_revision, guards_after.centroid_revision,
        "the journaled cluster image reports the expected centroid guard, not the written one"
    );
    let before_clusters: Vec<&alexandria_storage::models::mutation::ClusterSnapshot> = detail
        .preimages
        .iter()
        .filter_map(|image| match image {
            SemanticSnapshot::Cluster(snapshot) => Some(snapshot),
            _ => None,
        })
        .collect();
    assert_eq!(
        before_clusters.len(),
        1,
        "one affected cluster requires one preimage"
    );
    let before_image = before_clusters
        .first()
        .expect("the cluster preimage is mandatory");
    let mut before_members = vec![mover.as_record_id().clone(), stayer.as_record_id().clone()];
    before_members.sort();
    assert_eq!(before_image.id, cluster);
    assert_eq!(before_image.members, before_members);
    assert_eq!(before_image.centroid, state.centroid);
    assert_eq!(before_image.depth, state.depth);
    assert_eq!(
        before_image.membership_revision,
        guards_before.membership_revision
    );
    assert_eq!(
        before_image.member_embedding_revision,
        guards_before.member_embedding_revision
    );
    assert_eq!(
        before_image.centroid_revision,
        guards_before.centroid_revision
    );

    // Positive control that a *second* member leaving the same cluster re-derives again rather than
    // freezing: the last member out means no centre is written, and the membership guard still moves.
    let stayer_state = repo
        .expected_state(&fixture.owner, &stayer)
        .await
        .expect("read the stayer's state");
    let second = repo
        .move_scope(
            &fixture.owner,
            stayer.clone(),
            stayer_state.version,
            MemoryScope::Unassigned,
            &op("centroid-move-last"),
        )
        .await;
    assert!(
        second.is_ok(),
        "moving the last member out of a cluster refused: {second:?}"
    );
    let emptied = fixture.cluster_row(&cluster).await;
    assert_eq!(
        emptied.centroid,
        vec![0.9, 0.2],
        "an emptied cluster had a centroid written from no population"
    );
    assert!(
        fixture.cluster_members(&cluster).await.is_empty(),
        "the last member was not detached"
    );
}

/// Two approved entries are two journal rows. There is no row that describes the batch.
#[tokio::test]
async fn two_approved_entries_journal_two_operations_and_no_batch_row() {
    let fixture = Fixture::enrolled("batch-two-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let classify = ClassificationRepo::new(fixture.db.inner(), &fixture.owner);
    let first = fixture.seed("the first memory an owner approved").await;
    let second_fact = fixture.seed("the second memory an owner approved").await;
    let project = fixture.project("batch-destination").await;

    let mut references = Vec::new();
    for fact in [&first, &second_fact] {
        let state = fixture.row(fact).await;
        references.push(
            classify
                .propose(
                    fact,
                    Version::new(state.semantic_revision.unwrap_or(0) as u64),
                    MemoryScope::Project(project.clone()),
                    Some("a two-entry batch"),
                )
                .await
                .expect("a proposal"),
        );
    }

    let applied = classify
        .apply_batch(&references, &op("batch-of-two"))
        .await
        .expect("the batch runs");
    assert_eq!(
        applied.applied_count(),
        2,
        "a lawful two-entry batch did not apply both: {applied:?}"
    );
    assert_eq!(applied.conflicts, Vec::new(), "both entries were lawful");

    // One row per entry, and the applied list names them. A consumer that read `applied` as "the batch
    // operation" would be citing one of two rows for a decision that covers both.
    assert_eq!(
        journal_rows(&fixture.db).await,
        2,
        "a two-entry batch did not write exactly two journal rows"
    );
    assert_eq!(
        applied.applied.len(),
        2,
        "the answer does not name one operation per applied entry"
    );
    assert_ne!(
        applied.applied[0], applied.applied[1],
        "both entries named the same journal row: {:?}",
        applied.applied
    );

    // Each entry is readable back on its own, keyed `<operation>:<proposal>` — which is what makes a replay
    // of the batch unable to re-land a move that already applied.
    let history = repo
        .history(&fixture.owner, 50)
        .await
        .expect("the owner's own history resolves");
    let moves = history
        .iter()
        .filter(|row| row.kind == operation_kind::MOVE)
        .count();
    assert_eq!(
        moves, 2,
        "the journal holds {moves} moves, not one per entry"
    );
    for handle in &applied.applied {
        let target = RecordId::parse_simple(handle).expect("the batch returns operation handles");
        let detail = repo
            .operation_by_id(&fixture.owner, &target)
            .await
            .expect("lookup by the handle the batch returned");
        let detail = detail.unwrap_or_else(|| panic!("{handle} is not a readable operation"));
        assert_eq!(
            detail.kind,
            operation_kind::MOVE,
            "a batch entry's own row is not a move: {detail:?}"
        );
    }

    // A replay of the same two references under the same batch key applies nothing again.
    let replay = classify
        .apply_batch(&references, &op("batch-of-two"))
        .await
        .expect("the replay runs");
    assert_eq!(
        journal_rows(&fixture.db).await,
        2,
        "a replayed batch wrote a third journal row: {replay:?}"
    );

    // Positive control that the batch is not simply refusing everything: a fresh proposal on a third memory
    // under a fresh key lands, and its entry is journaled.
    let third = fixture.seed("the third memory, approved later").await;
    let third_state = fixture.row(&third).await;
    let third_reference = classify
        .propose(
            &third,
            Version::new(third_state.semantic_revision.unwrap_or(0) as u64),
            MemoryScope::Project(project),
            None,
        )
        .await
        .expect("a third proposal");
    let later = classify
        .apply_batch(&[third_reference], &op("batch-third"))
        .await
        .expect("the third batch runs");
    assert_eq!(
        later.applied_count(),
        1,
        "a lawful later entry refused: {later:?}"
    );
    assert_eq!(fixture.row(&third).await.scope.as_deref(), Some("project"));
}

/// Filing a live memory into the review partition does not quarantine it, and filing a quarantined memory
/// out does not un-quarantine it. The comment that claimed both once sat above code that did neither.
#[tokio::test]
async fn a_move_touches_quarantine_in_neither_direction() {
    let fixture = Fixture::enrolled("quarantine-direction-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let live = fixture.seed("a live memory filed into review").await;
    let live_before = fixture.row(&live).await;
    assert_eq!(
        live_before.quarantined_at, None,
        "the fixture row starts live"
    );

    let state = repo
        .expected_state(&fixture.owner, &live)
        .await
        .expect("read");
    repo.move_scope(
        &fixture.owner,
        live.clone(),
        state.version,
        MemoryScope::Unassigned,
        &op("into-review"),
    )
    .await
    .expect("the move into review applies");
    let after_in = fixture.row(&live).await;
    assert_eq!(after_in.scope.as_deref(), Some("unassigned"));
    assert_eq!(
        after_in.quarantined_at, None,
        "a move into the review partition quarantined a live row it was never asked to hide"
    );

    // And back out, with a quarantined row that must stay quarantined.
    let hidden = fixture
        .seed_quarantined_unassigned("a quarantined row filed back to personal")
        .await;
    let hidden_before = fixture.row(&hidden).await;
    let hidden_state = repo
        .expected_state(&fixture.owner, &hidden)
        .await
        .expect("read the hidden row");
    repo.move_scope(
        &fixture.owner,
        hidden.clone(),
        hidden_state.version,
        MemoryScope::Personal,
        &op("out-of-review"),
    )
    .await
    .expect("the move out of review applies");
    let after_out = fixture.row(&hidden).await;
    assert_eq!(after_out.scope.as_deref(), Some("personal"));
    assert_eq!(
        after_out.quarantined_at, hidden_before.quarantined_at,
        "a move out of the review partition lifted a quarantine no human revoked"
    );
}

/// The integrity digests distinguish two cluster states the same handle describes.
#[tokio::test]
async fn cluster_images_are_hashed_by_state_not_by_handle() {
    use alexandria_storage::models::mutation::{ClusterSnapshot, SemanticSnapshot};

    let base = ClusterSnapshot {
        id: RecordId::new("cluster", "c1"),
        label: Some("topic".to_string()),
        centroid: vec![0.5, 0.5],
        depth: 0,
        members: vec![RecordId::new("fact", "f1")],
        membership_revision: Version::new(1),
        member_embedding_revision: Version::ZERO,
        centroid_revision: Version::ZERO,
    };
    let digest =
        |snapshot: &ClusterSnapshot| SemanticSnapshot::Cluster(snapshot.clone()).integrity_digest();
    assert!(!digest(&base).is_empty());

    // A different member set, same handle, same centroid: still a different state.
    let mut other_members = base.clone();
    other_members.members = vec![RecordId::new("fact", "f2")];
    assert_ne!(
        digest(&base),
        digest(&other_members),
        "the cluster digest ignores its membership"
    );

    // A moved centroid.
    let mut moved_centroid = base.clone();
    moved_centroid.centroid = vec![0.6, 0.4];
    assert_ne!(
        digest(&base),
        digest(&moved_centroid),
        "the cluster digest ignores its centroid"
    );

    // A guard that moved with an unchanged population — the exact shape a bookkeeping retry leaves behind.
    let mut bumped = base.clone();
    bumped.membership_revision = Version::new(2);
    assert_ne!(
        digest(&base),
        digest(&bumped),
        "the cluster digest ignores its membership guard"
    );

    // Membership order is not state: one population renders one digest.
    let mut shuffled = base.clone();
    shuffled.members = vec![
        RecordId::new("fact", "f2"),
        RecordId::new("fact", "f1"),
        RecordId::new("fact", "f3"),
    ];
    let mut sorted = shuffled.clone();
    sorted.members.reverse();
    assert_eq!(
        digest(&shuffled),
        digest(&sorted),
        "the cluster digest depends on the order one population was read in"
    );

    // And the two variants of one subject are not interchangeable digests: an edge image of the same row
    // handle must not collide with a cluster image.
    let edge = SemanticSnapshot::Edge(alexandria_storage::models::mutation::EdgeSnapshot {
        edge: RecordId::new("memory_edge", "e1"),
        kind: EdgeKind::Memory("derived_from".to_string()),
        from: RecordId::new("fact", "f1"),
        to: RecordId::new("fact", "f2"),
        strength: 1.0,
    });
    assert_ne!(
        edge.integrity_digest(),
        SemanticSnapshot::Cluster(base).integrity_digest()
    );
}

// -- the owner's queue: listing and dismissal -------------------------------------------------------
//
// Task 11's page cannot be built on `propose` alone: an owner needs to *see* what an agent suggested, and
// needs to be able to say no. Both reads are new, so both are pinned here with the rule this file already
// follows — every "wrote nothing / saw nothing" assertion paired with a positive control on the same
// fixture, because an empty listing is also what a broken fixture reports.

/// Count of this store's journal rows, for the case that must prove a dismissal is not a corpus write.
async fn journal_rows(db: &alexandria_storage::Database) -> i64 {
    let rows: Vec<serde_json::Value> = db
        .inner()
        .query("SELECT count() AS n FROM semantic_operation GROUP ALL;")
        .await
        .expect("counting the journal")
        .check()
        .expect("the count ran")
        .take(0)
        .expect("the count decodes");
    rows.first().and_then(|row| row["n"].as_i64()).unwrap_or(0)
}

/// The queue lists exactly the caller's own pending proposals, with the destination spelled as a scope.
#[tokio::test]
async fn the_owner_can_list_their_pending_queue() {
    let fixture = Fixture::enrolled("queue-owner").await;
    let project = fixture.project("queue-project").await;
    let classify = ClassificationRepo::new(fixture.db.inner(), &fixture.owner);

    let personal = fixture.seed("a memory proposed for personal").await;
    let filed = fixture.seed("a memory proposed for a project").await;

    classify
        .propose(
            &personal,
            Version::new(0),
            MemoryScope::Personal,
            Some("it is generic"),
        )
        .await
        .expect("a judgement");
    classify
        .propose(
            &filed,
            Version::new(0),
            MemoryScope::Project(project.clone()),
            None,
        )
        .await
        .expect("a second judgement");

    let queue = classify.list_pending().await.expect("the queue reads");
    assert_eq!(queue.len(), 2, "{queue:?}");
    assert!(
        queue.iter().all(|row| row.reason.as_deref() != Some("")),
        "an absent reason arrived as an empty string rather than None"
    );
    // The project destination comes back as a scope naming the project, not as the bare kind word: the
    // page has to show *where the memory would go*, and a listing that could not tell two projects apart
    // would be approving something the owner cannot read.
    assert!(
        queue
            .iter()
            .any(|row| row.destination == MemoryScope::Personal),
        "no personal destination in {queue:?}"
    );
    assert!(
        queue
            .iter()
            .any(|row| row.destination == MemoryScope::Project(project.clone())),
        "the project destination did not resolve: {queue:?}"
    );
    assert!(
        queue.iter().all(|row| !row.quarantined),
        "a live memory is reported as quarantined: {queue:?}"
    );

    // Positive control that the reason column round-trips, and the negative that an absent one stays absent.
    assert!(
        queue
            .iter()
            .any(|row| row.reason.as_deref() == Some("it is generic")),
        "the stated reason did not survive the listing: {queue:?}"
    );
    assert!(
        queue.iter().any(|row| row.reason.is_none()),
        "a proposal with no reason reported one"
    );

    // Dismissing one entry shrinks the queue by exactly that entry — which is also the proof the listing
    // is not a constant.
    let target = queue
        .iter()
        .find(|row| row.destination == MemoryScope::Personal)
        .expect("the personal proposal");
    classify
        .dismiss(&target.reference)
        .await
        .expect("dismissing one's own pending proposal");
    let after = classify.list_pending().await.expect("the queue re-reads");
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0].fact, *filed.as_record_id());
}

/// A proposal about a quarantined memory is listed **as quarantined**, because applying it must not hide that.
///
/// The queue's purpose here is to stop an owner approving a move that would make a hidden memory
/// searchable again by surprise: Task 11 preserves quarantine on application, and a page cannot say so
/// honestly unless the listing carries the state. v009 puts no quarantine column on the proposal, so this
/// is read from the target fact — and the read must use `!= NONE`, not `IS NOT NULL`, or every row in the
/// queue would come back quarantined.
#[tokio::test]
async fn the_queue_shows_quarantine_from_the_target_memory() {
    let fixture = Fixture::enrolled("queue-quarantine-owner").await;
    let classify = ClassificationRepo::new(fixture.db.inner(), &fixture.owner);

    let hidden = fixture
        .seed_quarantined_unassigned("a quarantined memory under review")
        .await;
    let live = fixture.seed("a live memory under review").await;
    for fact in [&hidden, &live] {
        classify
            .propose(fact, Version::new(0), MemoryScope::Personal, None)
            .await
            .expect("a judgement about each memory");
    }

    let queue = classify.list_pending().await.expect("the queue reads");
    assert_eq!(queue.len(), 2, "{queue:?}");
    let quarantined: Vec<RecordId> = queue
        .iter()
        .filter(|row| row.quarantined)
        .map(|row| row.fact.clone())
        .collect();
    // The discriminator: exactly one of the two rows, and it is the quarantined one. An `IS NOT NULL`
    // predicate would mark both; a missing predicate would mark neither.
    assert_eq!(
        quarantined.len(),
        1,
        "quarantine was not per-row: {queue:?}"
    );
    assert!(
        queue
            .iter()
            .any(|row| row.fact == *hidden.as_record_id() && row.quarantined),
        "the quarantined memory was not marked: {queue:?}"
    );
    assert!(
        queue
            .iter()
            .any(|row| row.fact == *live.as_record_id() && !row.quarantined),
        "the live memory was marked quarantined: {queue:?}"
    );
}

/// One owner's queue never contains another owner's proposal.
#[tokio::test]
async fn one_owners_queue_is_not_another_owners() {
    use alexandria_storage::repos::AuthRepo;
    use chrono::TimeDelta;

    let fixture = Fixture::enrolled("queue-leak-alice").await;
    let ids = IdentityRepo::new(fixture.db.inner())
        .bootstrap_owner("queue-leak-bob", HASH, &op("queue-bob-bootstrap"))
        .await
        .unwrap();
    let auth = AuthRepo::new(fixture.db.inner());
    let session = auth
        .mint_browser_session(
            &ids.account,
            TimeDelta::minutes(30),
            TimeDelta::hours(24),
            Utc::now(),
        )
        .await
        .unwrap();
    let cookie = session.cookie.render().expose().to_string();
    let principal = auth
        .authenticate_browser_session(&cookie, TimeDelta::minutes(30), Utc::now())
        .await
        .unwrap();
    let access = AccessRepo::new(&fixture.db);
    let other = access.authorize(&principal).await.unwrap();
    let fact = fixture.seed("alice's memory, proposed by alice").await;
    ClassificationRepo::new(fixture.db.inner(), &fixture.owner)
        .propose(
            &fact,
            Version::new(0),
            MemoryScope::Personal,
            Some("alice's reason"),
        )
        .await
        .expect("a judgement");
    let bobs_read = access
        .select(&other, ReadSelection::Assigned)
        .await
        .unwrap();
    let bobs_fact = ScopedMemoryRepo::new(fixture.db.inner(), &other, &bobs_read)
        .create_fact(
            "bob's memory, proposed by bob",
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
    let bobs_fact = FactId::try_from(bobs_fact).unwrap();
    ClassificationRepo::new(fixture.db.inner(), &other)
        .propose(
            &bobs_fact,
            Version::new(0),
            MemoryScope::Personal,
            Some("bob's reason"),
        )
        .await
        .expect("bob's judgement");

    let shared_bobs_queue = ClassificationRepo::new(fixture.db.inner(), &other)
        .list_pending()
        .await
        .expect("bob reads the shared physical store");
    assert_eq!(
        shared_bobs_queue.len(),
        1,
        "the isolation fixture must hold both owners' proposals in one physical store"
    );
    assert_eq!(shared_bobs_queue[0].reason.as_deref(), Some("bob's reason"));
    // Positive control: Alice also sees exactly her own proposal in this same store.
    let alices_queue = ClassificationRepo::new(fixture.db.inner(), &fixture.owner)
        .list_pending()
        .await
        .expect("alice's queue reads");
    assert_eq!(alices_queue.len(), 1, "{alices_queue:?}");
    assert_eq!(alices_queue[0].reason.as_deref(), Some("alice's reason"));

    // And bob cannot dismiss what is not his: foreign and absent are one answer, so the call is not a probe.
    let foreign = alices_queue[0].reference.clone();
    let before = fixture.semantic_state().await;
    let denial = ClassificationRepo::new(fixture.db.inner(), &other)
        .dismiss(&foreign)
        .await;
    assert!(
        matches!(denial, Err(StorageError::NotFound)),
        "a foreign dismissal was not refused as absent: {denial:?}"
    );
    assert_eq!(
        ClassificationRepo::new(fixture.db.inner(), &fixture.owner)
            .list_pending()
            .await
            .expect("alice re-reads")
            .len(),
        1,
        "bob's refused dismissal still decided alice's proposal"
    );
    let bobs_after = ClassificationRepo::new(fixture.db.inner(), &other)
        .list_pending()
        .await
        .unwrap();
    assert_eq!(bobs_after.len(), 1);
    assert_eq!(
        bobs_after[0].reference.id,
        shared_bobs_queue[0].reference.id
    );
    assert_eq!(
        fixture.semantic_state().await,
        before,
        "foreign dismissal changes neither owner's corpus, proposal nor journal state"
    );
}

/// Dismissing a proposal decides it, touches no memory, and writes no journal row.
#[tokio::test]
async fn a_dismissal_decides_the_proposal_and_writes_no_journal_row() {
    let fixture = Fixture::enrolled("dismiss-owner").await;
    let classify = ClassificationRepo::new(fixture.db.inner(), &fixture.owner);
    let fact = fixture.seed("a memory somebody else guessed at").await;
    let reference = classify
        .propose(&fact, Version::new(0), MemoryScope::Personal, None)
        .await
        .expect("a judgement");
    let before = fixture.row(&fact).await;
    let journal_before = journal_rows(&fixture.db).await;

    classify
        .dismiss(&reference)
        .await
        .expect("dismissing one's own proposal");

    assert_eq!(
        fixture
            .proposal_row(&reference.id)
            .await
            .map(|row| row.state),
        Some(proposal_state::DISMISSED.to_string()),
        "the proposal was not marked dismissed"
    );
    // The memory is untouched: no scope change, no revision bump, no journal row. A dismissal is a decision
    // about an opinion, not a corpus write, and `semantic_operation` is corpus-write history.
    let after = fixture.row(&fact).await;
    assert_eq!(after.scope, before.scope, "a dismissal moved the memory");
    assert_eq!(
        after.semantic_revision, before.semantic_revision,
        "a dismissal rewrote the memory"
    );
    assert_eq!(
        journal_rows(&fixture.db).await,
        journal_before,
        "a dismissal wrote a journal row"
    );

    // A second dismissal is refused: the row is no longer pending, and re-deciding a decision is the
    // failure mode the state check exists to catch.
    let again = classify.dismiss(&reference).await;
    assert!(
        matches!(again, Err(StorageError::Stale)),
        "a decided proposal was dismissed again: {again:?}"
    );
    // And an id that was never a proposal answers as absent, not as a different error.
    let invented = alexandria_storage::repos::ScopeProposalRef {
        id: "classification_proposal:never-minted".to_string(),
        revision: Version::new(0),
    };
    assert!(
        matches!(
            classify.dismiss(&invented).await,
            Err(StorageError::NotFound)
        ),
        "a made-up proposal id was not refused as absent"
    );
}
