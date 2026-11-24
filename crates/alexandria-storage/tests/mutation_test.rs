//! The shared mutation contract, exercised against the pinned 3.3.0 engine.
//!
//! Every case drives [`MutationRepo::apply`] through a real store with a real authorized owner context. The
//! failure injections are engine refusals rather than mocks, and every denial is paired with a positive
//! control on the same fixture, because "nothing was written" is also what a broken fixture reports.

use alexandria_storage::Database;
use alexandria_storage::access::{AccessRepo, MutationContext, OwnerContext, ScopedContext};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::identity::{
    FactId, MemoryScope, OperationId, ReadSelection, Version,
};
use alexandria_storage::models::mutation::{
    ClusterChange, ClusterWriteExpectation, EdgeKind, EmbeddingSnapshot, FactChange,
    MaintenanceSummary, PreparedChangeSet, RelationChange, RelationTarget, SemanticSnapshot,
    Visibility, effect,
};
use alexandria_storage::models::operation_kind;
use alexandria_storage::repos::{
    IdentityRepo, MutationRepo, ScopedClusterRepo, ScopedMemoryRepo, WriteTarget,
};
use alexandria_storage::schema;
use chrono::Utc;
use std::sync::Arc;
use surrealdb::types::{RecordId, SurrealValue};

use alexandria_storage::repos::AuthRepo;
use chrono::TimeDelta;

/// The fixture session's two lifetimes: `mint_browser_session` refuses an idle window not strictly
/// shorter than the absolute one, and the guard re-reads both on every call.
const IDLE: TimeDelta = TimeDelta::minutes(30);
const ABSOLUTE: TimeDelta = TimeDelta::hours(24);

const PASSWORD_HASH: &str = "$argon2id$fixture-prepared-hash-mutation-test-padding-padding";

#[tokio::test]
async fn guard_only_cluster_preserves_rows_and_does_not_fabricate_images() {
    let fixture = Fixture::enrolled("guard-only").await;
    let member = fixture.seed("guarded member").await;
    let read = fixture.read().await;
    let cluster = ScopedClusterRepo::new(fixture.db.inner(), &fixture.owner, &read)
        .create(Some("guarded"), &[0.1, 0.2], Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    fixture
        .db
        .inner()
        .query("RELATE $cluster->contains_memory->$member;")
        .bind(("cluster", cluster.clone()))
        .bind(("member", member.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let repo = MutationRepo::new(fixture.db.inner());
    let subject = repo.state(&fixture.owner, &member).await.unwrap().unwrap();
    let expectation = repo
        .cluster_expectation(&fixture.owner, &cluster)
        .await
        .unwrap();
    let sql =
        "SELECT * FROM $cluster; SELECT * FROM contains_memory WHERE in = $cluster ORDER BY id;";
    let mut response = fixture
        .db
        .inner()
        .query(sql)
        .bind(("cluster", cluster.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let before: [Vec<serde_json::Value>; 2] =
        [response.take(0).unwrap(), response.take(1).unwrap()];
    let change = PreparedChangeSet::new(operation_kind::UPDATE)
        .effect(effect::TAGS)
        .fact(FactChange::Update {
            fact: member.clone(),
            expected: subject.version,
            content: None,
            tags: Some(vec!["checked".to_string()]),
            confidence: None,
            embedding: None,
            visibility: None,
            scope: None,
        })
        .cluster(ClusterChange::Guard { expectation });
    let access = MutationContext::issued_to(&fixture.owner);
    let outcome = repo
        .apply(
            &access,
            &OperationId::new("guard-only"),
            &[alexandria_storage::models::ExpectedState::of(&subject)],
            &change,
        )
        .await;
    assert!(
        outcome.is_ok(),
        "a nonmutating cluster condition must be usable: {outcome:?}"
    );
    let mut response = fixture
        .db
        .inner()
        .query(sql)
        .bind(("cluster", cluster))
        .await
        .unwrap()
        .check()
        .unwrap();
    let after: [Vec<serde_json::Value>; 2] = [response.take(0).unwrap(), response.take(1).unwrap()];
    assert_eq!(
        after, before,
        "a guard must change neither cluster nor retained membership"
    );
    let detail = repo
        .operation(&fixture.owner, "guard-only")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.preimages.len(), 1);
    assert_eq!(detail.postimages.len(), 1);
    assert!(matches!(&detail.preimages[0], SemanticSnapshot::Fact(_)));
    assert!(matches!(&detail.postimages[0], SemanticSnapshot::Fact(_)));
    assert_eq!(
        repo.state(&fixture.owner, &member)
            .await
            .unwrap()
            .unwrap()
            .tags,
        vec!["checked"]
    );
}

#[tokio::test]
async fn guard_only_cluster_rejects_each_revision_and_population_drift() {
    for (index, field) in [
        "membership_revision",
        "member_embedding_revision",
        "centroid_revision",
        "population",
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::enrolled(&format!("guard-drift-{index}")).await;
        let member = fixture.seed("guarded member").await;
        let read = fixture.read().await;
        let cluster = ScopedClusterRepo::new(fixture.db.inner(), &fixture.owner, &read)
            .create(Some("guarded"), &[0.1, 0.2], Utc::now())
            .await
            .unwrap()
            .id
            .unwrap();
        fixture
            .db
            .inner()
            .query("RELATE $cluster->contains_memory->$member;")
            .bind(("cluster", cluster.clone()))
            .bind(("member", member.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let repo = MutationRepo::new(fixture.db.inner());
        let subject = repo.state(&fixture.owner, &member).await.unwrap().unwrap();
        let expectation = repo
            .cluster_expectation(&fixture.owner, &cluster)
            .await
            .unwrap();
        let mut change = PreparedChangeSet::new(operation_kind::UPDATE)
            .effect(effect::TAGS)
            .fact(FactChange::Update {
                fact: member.clone(),
                expected: subject.version,
                content: None,
                tags: Some(vec!["checked".to_string()]),
                confidence: None,
                embedding: None,
                visibility: None,
                scope: None,
            })
            .cluster(ClusterChange::Guard { expectation });
        if field == "population" {
            // Deliberately bypass the bookkeeping counter, as a compatibility writer can.
            let arrival = fixture.seed("unversioned arrival").await;
            fixture
                .db
                .inner()
                .query("RELATE $cluster->contains_memory->$member;")
                .bind(("cluster", cluster.clone()))
                .bind(("member", arrival.as_record_id().clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
        } else {
            fixture
                .db
                .inner()
                .query(format!("UPDATE $cluster SET {field} = 1;"))
                .bind(("cluster", cluster.clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
        }
        let before = fixture.semantic_state().await;
        let access = MutationContext::issued_to(&fixture.owner);
        let expected = [alexandria_storage::models::ExpectedState::of(&subject)];
        let result = repo
            .apply(
                &access,
                &OperationId::new("guard-stale"),
                &expected,
                &change,
            )
            .await;
        assert!(
            matches!(result, Err(StorageError::RetryableBookkeepingConflict)),
            "{field} must invalidate the guard: {result:?}"
        );
        assert_eq!(
            fixture.semantic_state().await,
            before,
            "{field} refusal must write nothing"
        );
        assert!(
            repo.operation(&fixture.owner, "guard-stale")
                .await
                .unwrap()
                .is_none()
        );
        let fingerprint = change.request_fingerprint();
        change.cluster_changes = vec![ClusterChange::Guard {
            expectation: repo
                .cluster_expectation(&fixture.owner, &cluster)
                .await
                .unwrap(),
        }];
        assert_eq!(
            change.request_fingerprint(),
            fingerprint,
            "derived guards are not new intent"
        );
        repo.apply(
            &access,
            &OperationId::new("guard-refreshed"),
            &expected,
            &change,
        )
        .await
        .expect("refreshed guard is the positive control on the same fixture");
        assert_eq!(
            repo.state(&fixture.owner, &member)
                .await
                .unwrap()
                .unwrap()
                .tags,
            vec!["checked"]
        );
    }
}

struct Fixture {
    db: Arc<Database>,
    owner: OwnerContext,
}

impl Fixture {
    /// One store, one enrolled owner, and the authority that owner's requests carry.
    async fn enrolled(login: &str) -> Self {
        let db = Database::connect_embedded()
            .await
            .expect("an in-memory store");
        schema::migrate(db.inner()).await.expect("the schema");
        let db = Arc::new(db);
        let owner = Self::authorize(&db, login).await;
        Self { db, owner }
    }

    /// The one way this fixture reaches an `OwnerContext`: enroll, mint a real browser session, present
    /// it, and let `AccessRepo` re-read the rows behind it.
    ///
    /// A hosted credential rather than a trusted-local principal, because the journal records the
    /// credential that actually presented itself (`semantic_operation.initiator`), and a test that took
    /// the local path would never exercise the column the owner history page renders.
    async fn authorize(db: &Arc<Database>, login: &str) -> OwnerContext {
        let ids = IdentityRepo::new(db.inner())
            .bootstrap_owner(
                login,
                PASSWORD_HASH,
                &OperationId::new(format!("bootstrap-{login}")),
            )
            .await
            .expect("bootstrapping a fixture owner");
        let session = AuthRepo::new(db.inner())
            .mint_browser_session(&ids.account, IDLE, ABSOLUTE, Utc::now())
            .await
            .expect("minting a fixture session");
        let cookie = session.cookie.render().expose().to_string();
        let principal = AuthRepo::new(db.inner())
            .authenticate_browser_session(&cookie, IDLE, Utc::now())
            .await
            .expect("presenting the fixture session");
        AccessRepo::new(db)
            .authorize(&principal)
            .await
            .expect("authorizing the fixture principal")
    }

    /// A second owner over the **same** store, with their own workspace.
    ///
    /// Privacy assertions are vacuous without this: "the other owner sees nothing" is a tautology when
    /// there is no other owner, and the whole point of a workspace-scoped operation key is that two real
    /// workspaces both hold `retry-1`.
    /// A second owner over the **same** store, with their own workspace.
    ///
    /// Privacy assertions are vacuous without this: "the other owner reads nothing" is a tautology when
    /// there is no other owner, and the whole point of a workspace-scoped operation key is that two real
    /// workspaces both hold `retry-1`.
    async fn second(&self, login: &str) -> Self {
        Self {
            db: Arc::clone(&self.db),
            owner: Self::authorize(&self.db, login).await,
        }
    }

    async fn read(&self) -> ScopedContext {
        AccessRepo::new(&self.db)
            .select(&self.owner, ReadSelection::Assigned)
            .await
            .expect("the owner's own corpus resolves")
    }

    /// One live memory, written through the scoped writer — so it carries an owner and a revision.
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
        let id = fact.id.expect("a created fact carries an id");
        // A real heat row, through the same shape the store path writes: `AGENTS.md` warns that a "no
        // heat was written" assertion built on `create_fact` alone passes **vacuously**, because
        // `HeatRepo::add_heat` matches zero rows silently. A test that asserts heat is *untouched* has
        // to be able to see it touched at all.
        self.db
            .inner()
            .query("CREATE heat_state SET memory = $fact, heat = 1.0, stability = 1.0, access_count = 0;")
            .bind(("fact", id.clone()))
            .await
            .expect("seeding a fixture heat row")
            .check()
            .expect("the heat row was created");
        FactId::try_from(id).expect("a fact row")
    }

    /// The store's full semantic state, as one comparable string.
    ///
    /// Not a row count: `failed_lineage_is_atomic` must prove the corpus is *identical*, and a count is
    /// satisfied by one row swapped for another. Heat and `last_used_at` are excluded on purpose — those
    /// are the columns a legitimate access moves, and including them would make every atomicity assertion
    /// red for a reason that is not a defect.
    async fn semantic_state(&self) -> String {
        let mut response = self
            .db
            .inner()
            .query(STATE_QUERIES)
            .await
            .expect("reading the store's state")
            .check()
            .expect("every state statement ran");
        let mut rendered = String::new();
        for index in 0..11 {
            let rows: Vec<serde_json::Value> = response
                .take(index)
                .unwrap_or_else(|error| panic!("state statement {index} did not decode: {error}"));
            rendered.push_str(&serde_json::to_string(&rows).expect("state renders"));
            rendered.push('\u{1c}');
        }
        rendered
    }

    async fn counts(&self) -> Counts {
        let mut response = self
            .db
            .inner()
            .query(COUNT_QUERIES)
            .await
            .expect("counting the store")
            .check()
            .expect("every count statement ran");
        let mut values = [0i64; 8];
        for (index, slot) in values.iter_mut().enumerate() {
            // Decoded as a list, not an `Option<CountRow>`: `take::<Option<T>>` answers only for the
            // first statement of a multi-statement response, so every later count came back `None` and
            // the fixture's "nothing was written" assertion passed for the wrong reason.
            let rows: Vec<CountRow> = response
                .take(index)
                .unwrap_or_else(|error| panic!("count statement {index} did not decode: {error}"));
            *slot = rows.first().and_then(|row| row.total).unwrap_or(0);
        }
        Counts(values)
    }

    /// A single row's semantic revision, read the way the journal reads it.
    async fn revision(&self, fact: &FactId) -> Option<Version> {
        let rows: Vec<RevisionRow> = self
            .db
            .inner()
            .query("SELECT semantic_revision FROM type::record('fact', $key) WHERE id = $id;")
            .bind(("key", key(fact.as_record_id())))
            .bind(("id", fact.as_record_id().clone()))
            .await
            .expect("reading the revision")
            .check()
            .expect("the revision read ran")
            .take(0)
            .expect("the revision decodes");
        rows.into_iter()
            .next()
            .map(|row| Version::new(row.semantic_revision.unwrap_or(0).max(0) as u64))
    }

    async fn memberships(&self, fact: &FactId) -> Vec<String> {
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
            .map(|row| row["in"].as_str().unwrap_or_default().to_string())
            .collect()
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

const STATE_QUERIES: &str = "SELECT id, content, embedding, confidence, tags, deleted, quarantined_at, scope, \
     project, semantic_revision FROM fact ORDER BY id; SELECT id, in, out, edge_type, strength FROM \
     memory_edge ORDER BY id; SELECT id, in, out FROM contains_memory ORDER BY id; SELECT id, in, \
     out FROM contains_session_memory ORDER BY id; SELECT id, in, out FROM extracted_from ORDER BY \
     id; SELECT id, in, out FROM has_provenance ORDER BY id; SELECT id, content, deleted FROM raw \
     ORDER BY id; SELECT id, centroid, membership_revision, centroid_revision, \
     member_embedding_revision FROM cluster ORDER BY id; SELECT * FROM semantic_operation ORDER BY \
     id; SELECT * FROM semantic_snapshot ORDER BY id; SELECT id FROM maintenance_log \
     ORDER BY id;";

/// The populations a journaled change writes, counted in one round trip.
///
/// Every statement aliases its aggregate to the *same* column name, because `take::<Vec<CountRow>>`
/// decodes by field and one `AS count` among eight `AS total`s reads as a missing row — which is how a
/// fixture can report "nothing was written" while eight rows sit there.
const COUNT_QUERIES: &str = "SELECT count() AS total FROM fact GROUP ALL; \
     SELECT count() AS total FROM memory_edge GROUP ALL; \
     SELECT count() AS total FROM contains_memory GROUP ALL; \
     SELECT count() AS total FROM semantic_operation GROUP ALL; \
     SELECT count() AS total FROM semantic_snapshot GROUP ALL; \
     SELECT count() AS total FROM maintenance_log GROUP ALL; \
     SELECT count() AS total FROM heat_state GROUP ALL; \
     SELECT count() AS total FROM provenance GROUP ALL;";

#[derive(Debug, Clone, Copy)]
struct Counts([i64; 8]);
impl Counts {
    fn facts(&self) -> i64 {
        self.0[0]
    }
    fn operations(&self) -> i64 {
        self.0[3]
    }
    fn snapshots(&self) -> i64 {
        self.0[4]
    }
    fn maintenance(&self) -> i64 {
        self.0[5]
    }
    fn heat(&self) -> i64 {
        self.0[6]
    }
    fn provenance(&self) -> i64 {
        self.0[7]
    }
}

#[derive(SurrealValue)]
struct CountRow {
    total: Option<i64>,
}

#[derive(SurrealValue)]
struct RevisionRow {
    semantic_revision: Option<i64>,
}

fn embedding(vector: &[f32]) -> EmbeddingSnapshot {
    EmbeddingSnapshot::new(vector.to_vec(), Some("fixture-model"))
}

fn op(key: &str) -> OperationId {
    OperationId::new(key.to_string())
}

/// One lawful content update, with the vector written in the same statement.
fn edit(fact: &FactId, version: Version, content: &str) -> PreparedChangeSet {
    PreparedChangeSet::new(operation_kind::UPDATE)
        .effect(effect::CONTENT)
        .effect(effect::VECTOR)
        .fact(FactChange::Update {
            fact: fact.clone(),
            expected: version,
            content: Some(content.to_string()),
            tags: None,
            confidence: None,
            embedding: Some(embedding(&[0.9, 0.1])),
            visibility: None,
            scope: None,
        })
}

// -- the plan's named cases ------------------------------------------------------------------------------

#[tokio::test]
async fn semantic_state_observes_vector_only_drift() {
    let fixture = Fixture::enrolled("vector-observation-owner").await;
    let fact = fixture.seed("unchanged text with a drifting vector").await;
    let before = fixture.semantic_state().await;
    let counts = fixture.counts().await;
    let mut changed = fixture
        .db
        .inner()
        .query("UPDATE $fact SET embedding = [0.0, 1.0] RETURN AFTER;")
        .bind(("fact", fact.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = changed.take(0).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the positive control actually updated its fixture"
    );
    assert_eq!(rows[0]["embedding"], serde_json::json!([0.0, 1.0]));
    assert_eq!(
        fixture.counts().await.0,
        counts.0,
        "a vector-only change alters no row counts"
    );
    assert_ne!(
        fixture.semantic_state().await,
        before,
        "rollback observation must detect a vector-only change, not merely unchanged text and row counts"
    );
}

/// A content update whose lineage edge cannot be written leaves the entire before-state behind.
///
/// Task 10's headline case, with a real injected failure: `memory_edge` is v002's `TYPE RELATION FROM fact
/// TO fact`, so relating it to a `raw` row is refused by the engine mid-transaction. The assertion is not
/// "an error came back" but "nothing moved" — the hidden snapshot row, the edge, the content, the vector,
/// the revision and the journal.
#[tokio::test]
async fn failed_lineage_is_atomic() {
    let fixture = Fixture::enrolled("lineage-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let fact = fixture.seed("the original text").await;
    let access = MutationContext::issued_to(&fixture.owner);
    let expected = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");

    let before = fixture.semantic_state().await;
    let before_counts = fixture.counts().await;

    let mut planned = PreparedChangeSet::new(operation_kind::UPDATE)
        .effect(effect::CONTENT)
        .effect(effect::LINEAGE)
        .fact(FactChange::Create {
            scope: MemoryScope::Personal,
            content: "the original text".to_string(),
            tags: vec!["seed".to_string()],
            confidence: 0.5,
            embedding: embedding(&[0.1, 0.2]),
            visibility: Visibility::deleted(),
            metadata: None,
        })
        .fact(FactChange::Update {
            fact: fact.clone(),
            expected: expected.version,
            content: Some("the corrected text".to_string()),
            tags: None,
            confidence: None,
            embedding: Some(embedding(&[0.9, 0.1])),
            visibility: None,
            scope: None,
        });
    planned.edge_additions.push(RelationChange::memory(
        "derived_from",
        RelationTarget::Existing(fact.as_record_id().clone()),
        // Injected failure: the far end is a `raw` row, which the relation's own declaration forbids.
        RelationTarget::Existing(RecordId::new("raw", "not-a-fact")),
        1.0,
    ));

    let apply_result = repo
        .apply(
            &access,
            &op("lineage-fails"),
            std::slice::from_ref(&expected),
            &planned,
        )
        .await;
    assert!(
        apply_result.is_err(),
        "a refused edge committed anyway: {apply_result:?}"
    );
    assert_eq!(
        fixture.semantic_state().await,
        before,
        "an injected edge failure left part of the change behind"
    );
    let after = fixture.counts().await;
    assert_eq!(
        after.facts(),
        before_counts.facts(),
        "the hidden snapshot survived"
    );
    assert_eq!(
        after.operations(),
        before_counts.operations(),
        "a rolled-back attempt journaled"
    );
    assert_eq!(after.snapshots(), before_counts.snapshots());

    // Positive control: the same change with the unlawful edge removed applies, so the assertion above is
    // measuring a rollback rather than a fixture that cannot write at all.
    let mut lawful = planned.clone();
    lawful.edge_additions.clear();
    let applied = repo
        .apply(&access, &op("lineage-ok"), &[expected], &lawful)
        .await
        .expect("a lawful version of the same change applies");
    assert!(!applied.replayed);
    assert_eq!(
        fixture.counts().await.operations(),
        before_counts.operations() + 1
    );
    assert_eq!(fixture.counts().await.facts(), before_counts.facts() + 1);
    assert_ne!(fixture.semantic_state().await, before);
}

/// A stale ordinary update and a stale ordinary delete both conflict and write nothing.
#[tokio::test]
async fn stale_ordinary_update_delete_conflicts() {
    let fixture = Fixture::enrolled("stale-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let fact = fixture.seed("text before anyone edited it").await;
    let access = MutationContext::issued_to(&fixture.owner);
    let current = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");

    // A belief about a row that has since moved: same handle, stale digest, stale counter.
    let mut moved_on = current.clone();
    moved_on.semantic_hash = "f".repeat(64);
    moved_on.version = Version::new(current.version.get() + 5);

    let before = fixture.semantic_state().await;

    let outcome = repo
        .apply(
            &access,
            &op("stale-update"),
            &[moved_on],
            &edit(&fact, Version::new(9), "text after the edit"),
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::Stale)),
        "a stale update did not conflict: {outcome:?}"
    );
    assert_eq!(
        fixture.semantic_state().await,
        before,
        "a stale update wrote anyway"
    );

    // A delete aimed at a revision the row does not carry is refused by the same guard.
    let delete = PreparedChangeSet::new(operation_kind::DELETE)
        .effect(effect::DELETE)
        .fact(FactChange::Update {
            fact: fact.clone(),
            expected: Version::new(current.version.get() + 9),
            content: None,
            tags: None,
            confidence: None,
            embedding: None,
            visibility: Some(Visibility::deleted()),
            scope: None,
        });
    let outcome = repo
        .apply(
            &access,
            &op("stale-delete"),
            std::slice::from_ref(&current),
            &delete,
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::Stale)),
        "a delete aimed at a foreign revision succeeded: {outcome:?}"
    );
    assert_eq!(
        fixture.semantic_state().await,
        before,
        "a refused delete moved visibility"
    );

    // Positive control on the same fixture: the current expectation applies, and the revision moves.
    let applied = repo
        .apply(
            &access,
            &op("live-update"),
            std::slice::from_ref(&current),
            &edit(&fact, current.version, "corrected"),
        )
        .await
        .expect("a current expectation applies");
    assert!(!applied.replayed);
    assert_ne!(fixture.semantic_state().await, before);
    assert_eq!(
        fixture.revision(&fact).await,
        Some(Version::new(current.version.get() + 1)),
        "the applied update did not bump the revision it guards"
    );

    // …which is why re-issuing the *same* expectation now conflicts instead of silently re-editing.
    let again = repo
        .apply(
            &access,
            &op("after-the-edit"),
            std::slice::from_ref(&current),
            &edit(&fact, current.version, "corrected"),
        )
        .await;
    assert!(
        matches!(again, Err(StorageError::Stale)),
        "a stale expectation still applies after the row moved: {again:?}"
    );
}

/// A duplicate same-key request returns the recorded result; a different request under that key conflicts.
#[tokio::test]
async fn duplicate_operation_returns_recorded_result() {
    let fixture = Fixture::enrolled("duplicate-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let fact = fixture.seed("text to be corrected once").await;
    let access = MutationContext::issued_to(&fixture.owner);
    let expected = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");
    let planned = edit(&fact, expected.version, "corrected once");

    let first = repo
        .apply(
            &access,
            &op("retry-1"),
            std::slice::from_ref(&expected),
            &planned,
        )
        .await
        .expect("the first request applies");
    let state_after = fixture.semantic_state().await;
    let counts_after = fixture.counts().await;
    assert_eq!(
        counts_after.operations(),
        1,
        "one request left one journal row"
    );

    // The same key and the same request, re-issued *after* the row moved. Re-executing would be a second
    // edit; the replay path answers from the journal instead.
    let replay = repo
        .apply(&access, &op("retry-1"), &[], &planned)
        .await
        .expect("a duplicate request replays");
    assert!(replay.replayed, "the second call re-executed the change");
    assert_eq!(replay.operation, first.operation);
    assert_eq!(replay.postimages, first.postimages);
    assert_eq!(
        fixture.semantic_state().await,
        state_after,
        "a replay wrote to the corpus"
    );
    assert_eq!(
        fixture.counts().await.operations(),
        counts_after.operations(),
        "a replay wrote a second journal row"
    );
    assert_eq!(fixture.counts().await.snapshots(), counts_after.snapshots());

    // The same key, a different request: a conflict, never somebody else's recorded success.
    let different = PreparedChangeSet::new(operation_kind::UPDATE)
        .effect(effect::TAGS)
        .fact(FactChange::Update {
            fact: fact.clone(),
            expected: expected.version,
            content: None,
            tags: Some(vec!["changed".to_string()]),
            confidence: None,
            embedding: None,
            visibility: None,
            scope: None,
        });
    let outcome = repo.apply(&access, &op("retry-1"), &[], &different).await;
    assert!(
        matches!(outcome, Err(StorageError::InvalidInput(_))),
        "a key reused across requests was accepted: {outcome:?}"
    );
    assert_eq!(
        fixture.semantic_state().await,
        state_after,
        "the conflicting request wrote"
    );
}

/// The operation key is scoped to a workspace, and one owner never resolves another's operation.
#[tokio::test]
async fn operation_key_is_workspace_scoped() {
    let fixture = Fixture::enrolled("scoped-key-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let mine = fixture.seed("my memory").await;
    let access = MutationContext::issued_to(&fixture.owner);
    let expected = repo
        .expected_state(&fixture.owner, &mine)
        .await
        .expect("my read");
    let first = repo
        .apply(
            &access,
            &op("retry-1"),
            std::slice::from_ref(&expected),
            &edit(&mine, expected.version, "my memory, edited"),
        )
        .await
        .expect("my operation journals");

    let other = fixture.second("scoped-key-other").await;
    let theirs = other.seed("their memory").await;
    let other_repo = MutationRepo::new(other.db.inner());
    let other_access = MutationContext::issued_to(&other.owner);
    let other_expected = other_repo
        .expected_state(&other.owner, &theirs)
        .await
        .expect("their read");
    let second = other_repo
        .apply(
            &other_access,
            &op("retry-1"),
            std::slice::from_ref(&other_expected),
            &edit(&theirs, other_expected.version, "their memory, edited"),
        )
        .await
        .expect("the same key under a different workspace is a different operation");
    assert!(!second.replayed);
    assert_ne!(first.operation, second.operation);
    assert_eq!(
        fixture.counts().await.operations(),
        2,
        "two workspaces using one key produced one journal row"
    );

    // Each owner's history page lists exactly their own operation, and neither resolves the other's.
    let mine_history = repo.history(&fixture.owner, 50).await.expect("my history");
    let theirs_history = other_repo
        .history(&other.owner, 50)
        .await
        .expect("their history");
    assert_eq!(
        mine_history.len(),
        1,
        "a history page crossed workspaces: {mine_history:?}"
    );
    assert_eq!(
        theirs_history.len(),
        1,
        "a history page crossed workspaces: {theirs_history:?}"
    );
    assert_ne!(mine_history[0].id, theirs_history[0].id);
    assert!(
        repo.operation_by_id(
            &fixture.owner,
            &RecordId::parse_simple(&theirs_history[0].id).unwrap()
        )
        .await
        .expect("a foreign handle is not a probe")
        .is_none(),
        "one owner read another's operation by handle"
    );
}

/// A journal, snapshot, or summary failure rolls every semantic write back.
///
/// Two real injections, one per half of the contract: an effect set v009's `ASSERT array::len($value) > 0`
/// refuses, and a `maintenance_log` row whose required `source_id` is empty. In both cases the fact writes
/// that ran earlier in the same transaction must not survive.
#[tokio::test]
async fn journal_snapshot_or_summary_failure_rolls_back_everything() {
    let fixture = Fixture::enrolled("rollback-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let fact = fixture
        .seed("text that must survive a failed attempt")
        .await;
    let access = MutationContext::issued_to(&fixture.owner);
    let expected = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");

    let before = fixture.semantic_state().await;
    let before_counts = fixture.counts().await;

    // Half one: the journal row itself is refused, after a real corpus write.
    let doomed = PreparedChangeSet {
        effects: Vec::new(),
        ..edit(&fact, expected.version, "rewritten by a doomed operation")
    };
    let outcome = repo
        .apply(
            &access,
            &op("no-effects"),
            std::slice::from_ref(&expected),
            &doomed,
        )
        .await;
    assert!(
        outcome.is_err(),
        "an operation the journal refuses was accepted: {outcome:?}"
    );
    assert_eq!(
        fixture.semantic_state().await,
        before,
        "a refused journal row left a corpus write"
    );
    assert_eq!(
        fixture.counts().await.operations(),
        before_counts.operations()
    );

    // Half two: the linked summary cannot be attributed, and the corpus change goes with it.
    let mut with_summary = edit(
        &fact,
        expected.version,
        "rewritten by a summary that cannot land",
    );
    with_summary.maintenance_summary = Some(MaintenanceSummary {
        action: "collapse".to_string(),
        // A job outside the closed audit namespace: `sweep` exists as a maintenance job and deliberately
        // writes no audit row, so attributing a corpus change to it is the refusal this case needs.
        source_id: "cluster:rolled-back".to_string(),
        target_ids: vec![],
        members_moved: 0,
        run_id: "run-test".to_string(),
        job: "sweep".to_string(),
        disposition: "demote".to_string(),
        previous_value: None,
    });
    let outcome = repo
        .apply(&access, &op("bad-summary"), &[expected], &with_summary)
        .await;
    assert!(
        outcome.is_err(),
        "a summary the audit schema refuses did not roll the change back: {outcome:?}"
    );
    assert_eq!(
        fixture.semantic_state().await,
        before,
        "the corpus kept a change whose audit row failed"
    );
    let after = fixture.counts().await;
    assert_eq!(
        after.operations(),
        before_counts.operations(),
        "the journal row survived its rollback"
    );
    assert_eq!(
        after.snapshots(),
        before_counts.snapshots(),
        "snapshots survived their rollback"
    );
    assert_eq!(after.maintenance(), before_counts.maintenance());
    assert_eq!(after.facts(), before_counts.facts());

    // Positive control: the same change with a lawful summary writes both halves, atomically.
    let mut lawful = with_summary.clone();
    let summary = lawful.maintenance_summary.as_mut().expect("a summary");
    summary.job = "collapse".to_string();
    let applied = repo
        .apply(
            &access,
            &op("lawful-summary"),
            &[repo
                .expected_state(&fixture.owner, &fact)
                .await
                .expect("re-read")],
            &lawful,
        )
        .await
        .expect("a lawful change with a lawful summary applies");
    assert_eq!(
        fixture.counts().await.maintenance(),
        before_counts.maintenance() + 1
    );
    assert_eq!(
        fixture.counts().await.operations(),
        before_counts.operations() + 1
    );
    assert_eq!(
        fixture.counts().await.snapshots(),
        before_counts.snapshots() + 2
    );
    assert!(!applied.preimages.is_empty() && !applied.postimages.is_empty());
}

/// A heat-only access does not change the semantic version, and cannot stale a planned edit.
///
/// The retrieval path resets heat to 1.0 and bumps `access_count` on every row it returns. If that moved a
/// memory's semantic revision, every edit a caller planned before a retrieval would conflict on something
/// that was never a semantic change — so this pins the two-column separation from the mutation side.
#[tokio::test]
async fn heat_only_version_stability() {
    let fixture = Fixture::enrolled("heat-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let fact = fixture.seed("text a retrieval is about to surface").await;
    let access = MutationContext::issued_to(&fixture.owner);
    let expected = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");

    // A real heat row, written through the store's own recorder. AGENTS.md warns that a "no heat was
    // written" assertion built on `create_fact` passes vacuously, so the positive control comes first.
    let before_counts = fixture.counts().await;
    crate::heat_access::record(&fixture, &fact).await;
    // The positive control is that the access *landed on a row*, not that a row was added: an access
    // updates `heat_state`, so a count is the wrong instrument and a fixture with no heat row would read
    // as a passing assertion.
    let touched: Vec<serde_json::Value> = fixture
        .db
        .inner()
        .query("SELECT access_count FROM heat_state WHERE memory = $fact;")
        .bind(("fact", fact.as_record_id().clone()))
        .await
        .expect("reading the access count")
        .check()
        .expect("the access read ran")
        .take(0)
        .expect("the access count decodes");
    assert!(
        touched
            .first()
            .and_then(|row| row["access_count"].as_i64())
            .unwrap_or(0)
            >= 1,
        "the fixture could not record an access at all, so this test would prove nothing: {touched:?}"
    );
    // And the access added and removed no rows: it is an update to one heat row, so a fixture whose
    // counts moved would mean the recorder wrote something semantic.
    let after_counts = fixture.counts().await;
    assert_eq!(
        after_counts.facts(),
        before_counts.facts(),
        "recording an access changed the corpus's row count"
    );
    assert_eq!(
        after_counts.heat(),
        before_counts.heat(),
        "recording an access created or destroyed a heat row"
    );

    let after_access = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("re-read");
    assert_eq!(
        after_access.version, expected.version,
        "an access moved the semantic revision a mutation guards"
    );
    assert_eq!(
        after_access.semantic_hash, expected.semantic_hash,
        "an access moved the semantic digest"
    );

    // The expectation read *before* the access still applies.
    let applied = repo
        .apply(
            &access,
            &op("after-access"),
            std::slice::from_ref(&expected),
            &edit(&fact, expected.version, "edited after a retrieval"),
        )
        .await
        .expect("a heat-only change must not stale a semantic expectation");
    assert!(!applied.replayed);

    // …and the journal did not restore over the access: the heat row is still the one the access left.
    let heat: Vec<serde_json::Value> = fixture
        .db
        .inner()
        .query("SELECT access_count, heat FROM heat_state WHERE memory = $fact;")
        .bind(("fact", fact.as_record_id().clone()))
        .await
        .expect("reading heat")
        .check()
        .expect("the heat read ran")
        .take(0)
        .expect("heat decodes");
    assert_eq!(heat.len(), 1, "the access's heat row is gone: {heat:?}");
    assert!(
        heat[0]["access_count"].as_i64().unwrap_or(0) >= 1,
        "the journal reset an access count it must not touch: {:?}",
        heat[0]
    );
}

/// A newly stored memory's provenance is linked, owned, and atomic with the row.
#[tokio::test]
async fn linked_provenance_is_written_with_the_row() {
    let fixture = Fixture::enrolled("provenance-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let access = MutationContext::issued_to(&fixture.owner);
    let before = fixture.counts().await;

    let planned = PreparedChangeSet::new(operation_kind::STORE)
        .effect(effect::STORE)
        .effect(effect::PROVENANCE)
        .heat(1.0)
        .provenance("user", Utc::now())
        .fact(FactChange::Create {
            scope: MemoryScope::Personal,
            content: "a memory stored under the journal".to_string(),
            tags: vec!["stored".to_string()],
            confidence: 0.5,
            embedding: embedding(&[0.3, 0.7]),
            visibility: Visibility::live(),
            metadata: None,
        });
    let applied = repo
        .apply(&access, &op("store-provenance"), &[], &planned)
        .await
        .expect("the store applies");
    assert_eq!(applied.created.len(), 1, "the stored row was not reported");
    assert_eq!(fixture.counts().await.provenance(), before.provenance() + 1);
    assert_eq!(fixture.counts().await.facts(), before.facts() + 1);

    // Linked, and owned: `has_provenance` has no owner column, so ownership travels with the rows.
    let mut response = fixture
        .db
        .inner()
        .query("SELECT in, out FROM has_provenance; SELECT kind, account FROM provenance;")
        .await
        .expect("reading provenance")
        .check()
        .expect("the provenance reads ran");
    let links: Vec<serde_json::Value> = response.take(0).expect("links decode");
    let rows: Vec<serde_json::Value> = response.take(1).expect("rows decode");
    assert_eq!(
        links.len(),
        1,
        "the provenance row was never linked: {links:?}"
    );
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0]["account"].is_string() || rows[0]["account"].is_object(),
        "the provenance row carries no owner stamp: {:?}",
        rows[0]
    );
    assert_eq!(
        fixture.counts().await.heat(),
        before.heat() + 1,
        "the stored row has no heat row"
    );

    // Negative control with a positive one attached: a store whose fact write is refused writes no
    // provenance either — which is the whole point of one transaction.
    let refused = PreparedChangeSet::new(operation_kind::STORE)
        .effect(effect::STORE)
        .effect(effect::PROVENANCE)
        .provenance("user", Utc::now())
        .fact(FactChange::Create {
            scope: MemoryScope::Project(
                alexandria_storage::models::identity::ProjectId::try_from(RecordId::new(
                    "project",
                    "never-minted",
                ))
                .expect("a project handle"),
            ),
            content: "a store that must not half-land".to_string(),
            tags: vec![],
            confidence: 0.5,
            embedding: embedding(&[0.3, 0.7]),
            visibility: Visibility::live(),
            metadata: None,
        });
    let outcome = repo
        .apply(&access, &op("store-refused"), &[], &refused)
        .await;
    assert!(
        outcome.is_err(),
        "a store aimed at a foreign project landed: {outcome:?}"
    );
    let after = fixture.counts().await;
    assert_eq!(
        after.facts(),
        before.facts() + 1,
        "the refused store added a fact row"
    );
    assert_eq!(
        after.provenance(),
        before.provenance() + 1,
        "the refused store added a provenance row"
    );
}

/// An unrelated membership change is preserved across a derived-bookkeeping conflict and its lawful retry.
#[tokio::test]
async fn derived_cluster_conflict_preserves_unrelated_members() {
    let fixture = Fixture::enrolled("cluster-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let access = MutationContext::issued_to(&fixture.owner);
    let read = fixture.read().await;
    let cluster = ScopedClusterRepo::new(fixture.db.inner(), &fixture.owner, &read)
        .create(None, &[0.5, 0.5], Utc::now())
        .await
        .expect("a fixture cluster")
        .id
        .expect("a created cluster has an id");

    // An unrelated writer lands a member first, moving the cluster's derived guards.
    let outsider = fixture.seed("a member somebody else added").await;
    fixture.attach(&cluster, &outsider).await;
    let members_before = fixture.memberships(&outsider).await;
    assert_eq!(
        members_before.len(),
        1,
        "the fixture could not attach a member"
    );

    // This change approves a *different* member and recomputes the centroid, against the guards as they
    // were before that unrelated write.
    let stale = ClusterWriteExpectation {
        cluster: cluster.clone(),
        membership_revision: Version::ZERO,
        member_embedding_revision: Version::ZERO,
        centroid_revision: Version::ZERO,
        // Current population keeps this a revision-CAS test, not a population-refusal test.
        population: repo
            .cluster_expectation(&fixture.owner, &cluster)
            .await
            .unwrap()
            .population,
    };
    let planned = {
        let mut change = PreparedChangeSet::new(operation_kind::CLUSTER_CHANGE)
            .effect(EdgeKind::ClusterMembership.effect())
            .effect(effect::CLUSTER_CENTROID)
            .fact(FactChange::Create {
                scope: MemoryScope::Personal,
                content: "the member this change approves".to_string(),
                tags: vec![],
                confidence: 0.5,
                embedding: embedding(&[0.2, 0.2]),
                visibility: Visibility::live(),
                metadata: None,
            });
        change.cluster_changes.push(ClusterChange::Effect {
            expectation: stale.clone(),
            add_member: Some(RelationTarget::NewFact(0)),
            remove_member: None,
            centroid: Some(vec![0.4, 0.4]),
        });
        change
    };

    let outcome = repo
        .apply(&access, &op("cluster-stale"), &[], &planned)
        .await;
    assert!(
        matches!(outcome, Err(StorageError::RetryableBookkeepingConflict)),
        "a derived conflict was not classified as retryable: {outcome:?}"
    );
    assert_eq!(
        fixture.memberships(&outsider).await,
        members_before,
        "the refused attempt dropped the unrelated writer's member"
    );
    assert_eq!(
        fixture.counts().await.operations(),
        0,
        "a rolled-back conflict still journaled an operation"
    );

    // Re-authorized against the cluster's current guards, with the same approved effect set, the member
    // lands and the unrelated one is still there — no silent loss, and no overwrite of the centroid.
    let fresh = repo
        .cluster_expectation(&fixture.owner, &cluster)
        .await
        .expect("current guards");
    assert_ne!(fresh.centroid_revision, stale.centroid_revision);
    let mut retry = planned.clone();
    retry.cluster_changes = vec![ClusterChange::Effect {
        expectation: fresh.clone(),
        add_member: Some(RelationTarget::NewFact(0)),
        remove_member: None,
        centroid: Some(vec![0.4, 0.4]),
    }];
    let applied = repo
        .apply(&access, &op("cluster-retry"), &[], &retry)
        .await
        .expect("a lawful retry lands");
    assert!(!applied.replayed);
    let members = fixture.members(&cluster).await;
    assert_eq!(
        members.len(),
        2,
        "a member was lost across the conflict: {members:?}"
    );
    assert!(
        members.contains(&key(outsider.as_record_id())),
        "the unrelated writer's membership was silently removed: {members:?}"
    );
    let guards = repo
        .cluster_expectation(&fixture.owner, &cluster)
        .await
        .expect("guards after retry");
    assert_eq!(
        guards.centroid_revision.get(),
        fresh.centroid_revision.get() + 1
    );
    assert_eq!(
        guards.membership_revision.get(),
        fresh.membership_revision.get() + 1
    );

    // And an exhausted bound leaves the operation unapplied rather than overwriting the loser.
    let mut third = retry.clone();
    third.cluster_changes = vec![ClusterChange::Effect {
        expectation: fresh,
        add_member: Some(RelationTarget::NewFact(0)),
        remove_member: None,
        centroid: Some(vec![0.3, 0.3]),
    }];
    let exhausted = repo.apply(&access, &op("cluster-again"), &[], &third).await;
    assert!(
        matches!(exhausted, Err(StorageError::RetryableBookkeepingConflict)),
        "an expectation the cluster no longer carries was applied anyway: {exhausted:?}"
    );
    assert_eq!(
        fixture.members(&cluster).await.len(),
        2,
        "the exhausted attempt added a member it refused to commit"
    );
}

/// The owner's own history is private and complete; another owner cannot read any of it.
#[tokio::test]
async fn owner_history_is_private_and_complete() {
    let fixture = Fixture::enrolled("history-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let access = MutationContext::issued_to(&fixture.owner);
    let fact = fixture.seed("a memory with a real history").await;

    let mut keys = Vec::new();
    for step in 0..3 {
        let expected = repo
            .expected_state(&fixture.owner, &fact)
            .await
            .expect("read");
        let key = format!("history-{step}");
        repo.apply(
            &access,
            &op(&key),
            std::slice::from_ref(&expected),
            &edit(&fact, expected.version, &format!("edited at step {step}")),
        )
        .await
        .expect("each step applies");
        keys.push(key);
    }

    // Complete: every operation the owner performed, each with both images.
    let history = repo
        .history(&fixture.owner, 50)
        .await
        .expect("the history page");
    assert_eq!(
        history.len(),
        keys.len(),
        "the page is missing operations: {history:?}"
    );
    // The **same** rows by handle, which is how a history page's link actually reaches a detail page.
    //
    // This assertion did not exist, and its absence is how a real defect shipped: `operation_by_id` binds
    // `$id` only for its own statement, and the shared `detail` helper never bound it, so
    // `WHERE id = $id` compared against an unbound parameter — `NONE` on this engine — and the handle read
    // answered `None` for every operation, the caller's own included. The only existing case that calls it
    // asks whether a *foreign* handle is refused, and a read that always returns nothing passes that test
    // for the wrong reason. A negative control with no positive twin is not a control.
    for row in &history {
        let handle = RecordId::parse_simple(&row.id).expect("a listed operation's own handle");
        let by_handle = repo
            .operation_by_id(&fixture.owner, &handle)
            .await
            .expect("the owner's own handle resolves")
            .unwrap_or_else(|| {
                panic!(
                    "operation_by_id returned None for the caller's own {}",
                    row.id
                )
            });
        assert_eq!(
            by_handle.kind,
            operation_kind::UPDATE,
            "the handle read resolved a different kind"
        );
        assert_eq!(
            by_handle.preimages.len(),
            1,
            "the handle read lost the before-state for {}",
            row.id
        );
        assert_eq!(
            by_handle.postimages.len(),
            1,
            "the handle read lost the after-state for {}",
            row.id
        );
    }

    for key in &keys {
        let detail = repo
            .operation(&fixture.owner, key)
            .await
            .expect("the owner's own key resolves")
            .expect("a listed operation is readable");
        assert_eq!(detail.kind, operation_kind::UPDATE);
        assert_eq!(detail.preimages.len(), 1, "{key} journaled no before-state");
        assert_eq!(detail.postimages.len(), 1, "{key} journaled no after-state");
        let before = match &detail.preimages[0] {
            SemanticSnapshot::Fact(snapshot) => snapshot,
            other => panic!(
                "a fact operation must journal a fact image, got {:?}",
                other.kind()
            ),
        };
        let after = match &detail.postimages[0] {
            SemanticSnapshot::Fact(snapshot) => snapshot,
            other => panic!(
                "a fact operation must journal a fact image, got {:?}",
                other.kind()
            ),
        };
        assert_ne!(
            before.content, after.content,
            "{key} reports identical images"
        );
        assert_eq!(
            after.version.get(),
            before.version.get() + 1,
            "{key} did not move the revision"
        );
        assert_eq!(
            before.id, after.id,
            "{key} describes two different memories"
        );
        assert!(!detail.actor.is_empty(), "{key} names no actor");
        assert!(
            !detail.expected_effects.is_empty(),
            "{key} names no effects"
        );
    }

    // Private: a second owner reads none of it.
    let other = fixture.second("history-other").await;
    let other_repo = MutationRepo::new(other.db.inner());
    assert!(
        other_repo
            .history(&other.owner, 50)
            .await
            .expect("their page")
            .is_empty(),
        "one owner's history page listed another's operations"
    );
    for key in &keys {
        assert!(
            other_repo
                .operation(&other.owner, key)
                .await
                .expect("a foreign key is not a probe")
                .is_none(),
            "the second owner resolved the first's operation {key}"
        );
    }

    // The same key under the second workspace resolves *their* row — the journal answers about the
    // caller's own workspace and nothing else.
    let theirs = other.seed("their own memory").await;
    let expected = other_repo
        .expected_state(&other.owner, &theirs)
        .await
        .expect("their read");
    let other_access = MutationContext::issued_to(&other.owner);
    other_repo
        .apply(
            &other_access,
            &op("history-0"),
            std::slice::from_ref(&expected),
            &edit(&theirs, expected.version, "their own edit"),
        )
        .await
        .expect("the same key is lawful under their workspace");
    let shared = other_repo
        .operation(&other.owner, "history-0")
        .await
        .expect("their lookup")
        .expect("their operation resolves for them");
    let SemanticSnapshot::Fact(before) = &shared.preimages[0] else {
        panic!("their operation must journal a fact image");
    };
    assert_eq!(
        before.content, "their own memory",
        "the workspace-scoped key resolved the *first* owner's before-state"
    );
}

/// The request fingerprint covers *intent*, not derived bookkeeping, so a replay means the same request.
///
/// The distinction a fingerprint exists to draw is "the same thing again" versus "a different thing under a
/// key that already recorded one". A cluster's three guard revisions and the centroid they imply are
/// re-derived on every preparation inside one live request, so folding them into the digest would give one
/// request two fingerprints — and a genuine replay, whose population has since moved under it, would be
/// refused as a different request instead of returning the recorded answer. That is the source-vs-derived
/// line, and this is the control that holds it: same requested effect after the guards moved ⇒ replay;
/// different requested effect under the same key ⇒ conflict.
#[tokio::test]
async fn a_replayed_request_replays_after_the_bookkeeping_moved() {
    let fixture = Fixture::enrolled("fingerprint-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let access = MutationContext::issued_to(&fixture.owner);
    let read = fixture.read().await;
    let cluster = ScopedClusterRepo::new(fixture.db.inner(), &fixture.owner, &read)
        .create(None, &[0.4, 0.4], Utc::now())
        .await
        .expect("a fixture cluster")
        .id
        .expect("a created cluster has an id");
    // A join needs an eligible pre-existing witness; an empty cluster has no lawful partition.
    let witness = fixture.seed("the destination's existing member").await;
    fixture.attach(&cluster, &witness).await;
    let member = fixture.seed("the memory a join is approved for").await;

    let request = |expectation: ClusterWriteExpectation| {
        PreparedChangeSet::new(operation_kind::CLUSTER_CHANGE)
            .effect(EdgeKind::ClusterMembership.effect())
            .effect(effect::CLUSTER_CENTROID)
            .cluster(ClusterChange::Effect {
                expectation,
                add_member: Some(RelationTarget::Existing(member.as_record_id().clone())),
                remove_member: None,
                centroid: Some(vec![0.5, 0.5]),
            })
    };

    let guards = repo
        .cluster_expectation(&fixture.owner, &cluster)
        .await
        .expect("current guards");
    let fingerprint = request(guards.clone()).request_fingerprint();
    repo.apply(
        &access,
        &op("fingerprint-replay"),
        &[],
        &request(guards.clone()),
    )
    .await
    .expect("the first join applies");

    // The population moved, so the guards the next caller would read are different numbers.
    let moved = repo
        .cluster_expectation(&fixture.owner, &cluster)
        .await
        .expect("guards after the join");
    assert_ne!(
        moved.membership_revision, guards.membership_revision,
        "the fixture did not move the guard the derived half rides on"
    );
    let rebuilt = request(moved.clone());
    assert_eq!(
        rebuilt.request_fingerprint(),
        fingerprint,
        "the same requested effect hashed differently because its derived guards moved"
    );
    let replay = repo
        .apply(&access, &op("fingerprint-replay"), &[], &rebuilt)
        .await
        .expect("a replay of the same request must replay, not conflict");
    assert!(
        replay.replayed,
        "the second call applied instead of replaying"
    );
    assert_eq!(
        fixture.counts().await.operations(),
        1,
        "a replay wrote a second journal row"
    );

    // Positive control that the key still guards: a *different* requested effect — this member leaving the
    // cluster instead of joining it — is refused under the same key, not adopted.
    let different = PreparedChangeSet::new(operation_kind::CLUSTER_CHANGE)
        .effect(EdgeKind::ClusterMembership.effect())
        .cluster(ClusterChange::Detach {
            expectation: moved,
            member: member.as_record_id().clone(),
            surviving_centroid: None,
        });
    let outcome = repo
        .apply(&access, &op("fingerprint-replay"), &[], &different)
        .await;
    assert!(
        matches!(outcome, Err(StorageError::InvalidInput(_))),
        "a different effect set reused the key: {outcome:?}"
    );
    assert_eq!(
        fixture.counts().await.operations(),
        1,
        "the refused different request journalled a second row"
    );
}

/// The incident graph rides with the fact's preimage: a moved edge conflicts even at the same revision.
#[tokio::test]
async fn a_graph_move_stales_an_edit_at_the_same_revision() {
    let fixture = Fixture::enrolled("graph-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let access = MutationContext::issued_to(&fixture.owner);
    let fact = fixture.seed("a memory whose graph just moved").await;
    let expected = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");
    let related = fixture.seed("a memory it is related to").await;
    fixture
        .db
        .inner()
        .query("RELATE $a->memory_edge->$b SET edge_type = 'related', strength = 1.0;")
        .bind(("a", fact.as_record_id().clone()))
        .bind(("b", related.as_record_id().clone()))
        .await
        .expect("relating")
        .check()
        .expect("the relate ran");

    // The edge appeared after the caller read the row, at a revision that did not move.
    let outcome = repo
        .apply(
            &access,
            &op("graph-moved"),
            std::slice::from_ref(&expected),
            &edit(&fact, expected.version, "edited anyway"),
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::Stale)),
        "a fact revision alone certified an unchanged graph: {outcome:?}"
    );
    assert_eq!(
        fixture.revision(&fact).await,
        Some(expected.version),
        "the refused edit wrote"
    );

    // Positive control: the caller re-reads, and the same edit is now lawful.
    let fresh = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("re-read");
    assert_eq!(
        fresh.version, expected.version,
        "a graph move must not bump the revision"
    );
    assert_ne!(fresh.semantic_hash, expected.semantic_hash);
    repo.apply(
        &access,
        &op("graph-reread"),
        &[fresh],
        &edit(&fact, expected.version, "edited after re-reading"),
    )
    .await
    .expect("a re-read expectation applies");
}

/// A compat writer that ignores the revision counter is still caught by the semantic digest.
#[tokio::test]
async fn an_unjournaled_writer_is_caught_by_the_preimage_not_the_counter() {
    let fixture = Fixture::enrolled("compat-owner").await;
    let repo = MutationRepo::new(fixture.db.inner());
    let access = MutationContext::issued_to(&fixture.owner);
    let fact = fixture
        .seed("text another writer will change without a bump")
        .await;
    let expected = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("read");

    // An unguarded write of the kind that still exists in this crate: content moved, revision untouched.
    fixture
        .db
        .inner()
        .query(
            "UPDATE type::record('fact', $key) SET content = 'silently rewritten' WHERE id = $id;",
        )
        .bind(("key", key(fact.as_record_id())))
        .bind(("id", fact.as_record_id().clone()))
        .await
        .expect("the unjournaled write")
        .check()
        .expect("the write ran");
    assert_eq!(
        fixture.revision(&fact).await,
        Some(expected.version),
        "the unjournaled writer moved the revision, so this test would prove nothing"
    );

    let outcome = repo
        .apply(
            &access,
            &op("over-a-blind-write"),
            std::slice::from_ref(&expected),
            &edit(&fact, expected.version, "my edit"),
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::Stale)),
        "a compatibility writer was trusted past its counter: {outcome:?}"
    );
    assert!(
        fixture
            .semantic_state()
            .await
            .contains("silently rewritten"),
        "the refused edit overwrote the unjournaled write"
    );

    // Positive control: the caller sees the new state and their edit lands on top of it.
    let fresh = repo
        .expected_state(&fixture.owner, &fact)
        .await
        .expect("re-read");
    repo.apply(
        &access,
        &op("after-the-blind-write"),
        &[fresh],
        &edit(&fact, expected.version, "my edit"),
    )
    .await
    .expect("a re-read expectation applies");
}

// -- helpers the cluster case needs ----------------------------------------------------------------------

impl Fixture {
    async fn attach(&self, cluster: &RecordId, member: &FactId) {
        // A membership *and* its guard, in one statement list: this is the shape Task 12's writers will
        // follow, and a test that attached a member without moving the guard could not produce the
        // derived conflict this case is about.
        self.db
            .inner()
            .query(
                "RELATE $cluster->contains_memory->$member; UPDATE type::record('cluster', $key) \
                 SET membership_revision = 1, centroid_revision = 1 WHERE id = $cluster;",
            )
            .bind(("cluster", cluster.clone()))
            .bind(("member", member.as_record_id().clone()))
            .bind(("key", key(cluster)))
            .await
            .expect("attaching a member")
            .check()
            .expect("the attach ran");
    }

    async fn members(&self, cluster: &RecordId) -> Vec<String> {
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
            .expect("members decode");
        rows.iter()
            .filter_map(|row| row["out"].as_str().map(str::to_string))
            .map(|id| {
                RecordId::parse_simple(&id)
                    .ok()
                    .map(|id| key(&id))
                    .unwrap_or(id)
            })
            .collect()
    }
}

/// Recording an access the way retrieval does: one bulk read, one batched write, heat reset to 1.0.
mod heat_access {
    use super::*;

    pub(super) async fn record(fixture: &Fixture, fact: &FactId) {
        fixture
            .db
            .inner()
            .query(
                "UPDATE heat_state SET heat = 1.0, access_count = access_count + 1, last_accessed_at \
                 = time::now(), last_touched = time::now() WHERE memory = $fact RETURN AFTER;",
            )
            .bind(("fact", fact.as_record_id().clone()))
            .await
            .expect("recording an access")
            .check()
            .expect("the access write ran");
    }
}
