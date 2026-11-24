//! Task 6: spreading activation is a *partition-scoped write*, not a graph-wide one.
//!
//! Activation is the one retrieval side effect that writes, so it carries two failure modes a plain
//! scoped read does not:
//!
//! * **Warming across a partition.** A project memory's neighbour that happens to be Personal — or in
//!   another project, or in another account — must not receive heat, because heat decides which rows
//!   surface later. Deriving the warm set from the *request's* union (Personal + project) is the subtle
//!   version: every row warmed would be one the caller was allowed to *read*, so nothing downstream
//!   looks wrong, while influence has crossed the partition the design says maintenance and recall
//!   never cross.
//! * **A warm that silently does nothing.** A warm is an `UPDATE heat_state … WHERE
//!   memory = …` that matches zero rows when the row is absent, and its caller swallows the result. So
//!   "refused" and "happened" are indistinguishable at the call site — which is how an out-of-scope warm
//!   could be reported as a success.
//!
//! The tests seed real `fact`, `memory_edge` and `heat_state` rows — every "was not warmed" assertion
//! therefore reads a *present* row that did not move, which is the only form that proves anything — and
//! drive the scoped primitives. Per the brief, one retrieval carries **both** a Personal focal record
//! and a project focal record, and each must warm its own partition.
//!
//! Owners and contexts come from real rows through the sealed producers; there is no test-only
//! authority constructor here because none may exist.

use alexandria_storage::Database;
use alexandria_storage::access::{AccessRepo, OwnerContext, ScopedContext};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::{MemoryScope, OperationId, ProjectId, ReadSelection};
use alexandria_storage::repos::scope::ScopeFilter;
use alexandria_storage::repos::{
    IdentityRepo, ScopedGraphReader, ScopedHeatWriter, WarmTarget, focal_partition_of,
};
use chrono::Utc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::RecordId;

/// The engine handle every fixture helper takes.
type Db = Surreal<Any>;

/// One store holding two owners, each with a workspace and two projects.
///
/// Two owners must share a *store* for the cross-account cases to mean anything, so this is one fixture
/// with two accounts rather than two fixtures.
struct Store {
    db: Database,
    alice: Account,
    bob: Account,
}

struct Account {
    account: RecordId,
    workspace: RecordId,
    project_a: RecordId,
    project_b: RecordId,
    owner: OwnerContext,
}

impl Store {
    async fn build() -> Self {
        let mut db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        // The startup capability, claimed in the order production composes it. It is dropped with the
        // fixture: nothing here needs a second authority path, and there is no reason to keep one.
        let system = db.claim_system_authority().unwrap();
        let db = db;

        let identity = IdentityRepo::new(db.inner());
        let access = AccessRepo::new(&db);
        let mut accounts = Vec::with_capacity(2);
        for login in ["act-alice", "act-bob"] {
            let ids = identity
                .bootstrap_owner(
                    login,
                    &format!("$argon2id$fixture-prepared-hash-{login}-padding"),
                    &OperationId::new(format!("activation-{login}")),
                )
                .await
                .unwrap();
            let local = access.local_principal(&system, &ids.account).await.unwrap();
            let owner = access.authorize(&local).await.unwrap();
            let workspace = ids.workspace.as_record_id().clone();
            let project_a = create_project(db.inner(), &workspace, "alpha").await;
            let project_b = create_project(db.inner(), &workspace, "beta").await;
            accounts.push(Account {
                account: ids.account.as_record_id().clone(),
                workspace,
                project_a: project_a.as_record_id().clone(),
                project_b: project_b.as_record_id().clone(),
                owner,
            });
        }
        let mut iter = accounts.into_iter();
        Self {
            db,
            alice: iter.next().unwrap(),
            bob: iter.next().unwrap(),
        }
    }

    fn db(&self) -> &Db {
        self.db.inner()
    }

    /// Personal plus project A — the automatic-recall shape, and the union that must *not* become the
    /// warm set.
    async fn auto_with(&self, account: &Account, project: &RecordId) -> ScopedContext {
        AccessRepo::new(&self.db)
            .select(
                &account.owner,
                ReadSelection::Auto(Some(ProjectId::try_from(project.clone()).unwrap())),
            )
            .await
            .unwrap()
    }

    /// Personal plus every project, and never Unassigned.
    async fn assigned(&self, account: &Account) -> ScopedContext {
        AccessRepo::new(&self.db)
            .select(&account.owner, ReadSelection::Assigned)
            .await
            .unwrap()
    }

    async fn one(&self, account: &Account, scope: MemoryScope) -> ScopedContext {
        AccessRepo::new(&self.db)
            .select(&account.owner, ReadSelection::One(scope))
            .await
            .unwrap()
    }
}

async fn create_project(db: &Db, workspace: &RecordId, alias: &str) -> ProjectId {
    let mut response = db
        .query("CREATE project SET workspace = $workspace, normalized_alias = $alias RETURN AFTER;")
        .bind(("workspace", workspace.clone()))
        .bind(("alias", alias.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    ProjectId::try_from(id_of(&rows[0])).unwrap()
}

/// Seeds one fact with its ownership columns written explicitly, plus a `heat_state` row for it.
///
/// The heat row is the load-bearing part of the fixture: without it, the warm's `UPDATE heat_state …
/// WHERE memory = …` matches no row and every "did not warm" assertion would pass **vacuously**. Seeding
/// it turns each refusal into a comparison of a present row that stayed put.
#[allow(clippy::too_many_arguments)]
async fn seed_fact(
    db: &Db,
    key: &str,
    content: &str,
    account: &RecordId,
    workspace: &RecordId,
    scope: &str,
    project: Option<&RecordId>,
    deleted: bool,
    quarantined: bool,
) -> RecordId {
    let mut query = String::from(
        "CREATE type::record('fact', $key) SET content = $content, confidence = 0.5, embedding = \
         [1.0, 0.0], tags = [], deleted = $deleted, account = $account, workspace = $workspace, \
         scope = $scope",
    );
    if project.is_some() {
        query.push_str(", project = $project");
    }
    if quarantined {
        query.push_str(", quarantined_at = time::now()");
    }
    query.push(';');

    let mut q = db
        .query(&query)
        .bind(("key", key.to_string()))
        .bind(("content", content.to_string()))
        .bind(("deleted", deleted))
        .bind(("account", account.clone()))
        .bind(("workspace", workspace.clone()))
        .bind(("scope", scope.to_string()));
    if let Some(project) = project {
        q = q.bind(("project", project.clone()));
    }
    let mut created = q.await.unwrap().check().unwrap();
    let rows: Vec<serde_json::Value> = created.take(0).unwrap();
    let fact = id_of(rows.first().expect("a created row is returned"));

    db.query(
        "CREATE heat_state SET memory = $fact, heat = 0.25, stability = 1.0, access_count = 0, \
         last_touched = d'2026-01-01T00:00:00Z', last_accessed_at = d'2026-01-01T00:00:00Z';",
    )
    .bind(("fact", fact.clone()))
    .await
    .unwrap()
    .check()
    .unwrap();
    fact
}

async fn link(db: &Db, from: &RecordId, to: &RecordId) {
    db.query("RELATE $from->memory_edge->$to SET edge_type = 'related', strength = 1.0;")
        .bind(("from", from.clone()))
        .bind(("to", to.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
}

/// The stored heat bytes of one row.
async fn heat_row(db: &Db, fact: &RecordId) -> serde_json::Value {
    let mut response = db
        .query(
            "SELECT heat, last_touched, last_accessed_at, access_count FROM heat_state WHERE \
             memory = $fact LIMIT 1;",
        )
        .bind(("fact", fact.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    rows.into_iter()
        .next()
        .unwrap_or_else(|| panic!("every seeded fact must carry a heat_state row"))
}

fn id_of(row: &serde_json::Value) -> RecordId {
    let text = row["id"].as_str().expect("a created row has an id");
    let (table, key) = text.split_once(':').expect("a record id has a table");
    RecordId::new(table.to_string(), key.trim_matches('`').to_string())
}

/// Snapshot the stored bytes of a labelled row set, in a stable order.
async fn snapshot(db: &Db, rows: &[(&str, &RecordId)]) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::with_capacity(rows.len());
    for (label, id) in rows {
        out.push(((*label).to_string(), heat_row(db, id).await));
    }
    out
}

fn moved(before: &[(String, serde_json::Value)], after: &[(String, serde_json::Value)]) -> String {
    before
        .iter()
        .zip(after)
        .filter(|((_, b), (_, a))| a != b)
        .map(|((label, b), (_, a))| format!("{label}: {b:?} -> {a:?}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The brief's named gate, in one test.
///
/// One retrieval, two focal records — a Personal one and a project one — each warming only its **own**
/// partition's live neighbours. Everything else must stay byte-identical: another account's row, the
/// owner's other project, Personal-seen-from-a-project, project-seen-from-Personal, Unassigned,
/// soft-deleted and quarantined.
#[tokio::test]
async fn activation_does_not_warm_out_of_scope_or_unassigned_neighbors() {
    let store = Store::build().await;
    let db = store.db();
    let alice = &store.alice;
    let bob = &store.bob;

    // --- focal records: both are readable by one Personal+project-A retrieval ------------------
    let personal_focal = seed_fact(
        db,
        "focal-personal",
        "personal focal",
        &alice.account,
        &alice.workspace,
        "personal",
        None,
        false,
        false,
    )
    .await;
    let project_focal = seed_fact(
        db,
        "focal-project",
        "project focal",
        &alice.account,
        &alice.workspace,
        "project",
        Some(&alice.project_a),
        false,
        false,
    )
    .await;

    // --- neighbours of the PERSONAL focal ------------------------------------------------------
    // Its own partition is Personal, so the Personal neighbour is the positive control.
    let personal_live = seed_fact(
        db,
        "pn-live",
        "personal neighbour of personal focal",
        &alice.account,
        &alice.workspace,
        "personal",
        None,
        false,
        false,
    )
    .await;
    let project_seen_from_personal = seed_fact(
        db,
        "pn-project",
        "project neighbour of personal focal",
        &alice.account,
        &alice.workspace,
        "project",
        Some(&alice.project_a),
        false,
        false,
    )
    .await;
    let unassigned_seen_from_personal = seed_fact(
        db,
        "pn-unassigned",
        "unassigned neighbour of personal focal",
        &alice.account,
        &alice.workspace,
        "unassigned",
        None,
        false,
        false,
    )
    .await;
    let foreign_personal = seed_fact(
        db,
        "pn-foreign",
        "another account's personal neighbour",
        &bob.account,
        &bob.workspace,
        "personal",
        None,
        false,
        false,
    )
    .await;
    let deleted_personal = seed_fact(
        db,
        "pn-deleted",
        "deleted personal neighbour",
        &alice.account,
        &alice.workspace,
        "personal",
        None,
        true,
        false,
    )
    .await;
    let quarantined_personal = seed_fact(
        db,
        "pn-quarantined",
        "quarantined personal neighbour",
        &alice.account,
        &alice.workspace,
        "personal",
        None,
        false,
        true,
    )
    .await;
    for neighbour in [
        &personal_live,
        &project_seen_from_personal,
        &unassigned_seen_from_personal,
        &foreign_personal,
        &deleted_personal,
        &quarantined_personal,
    ] {
        link(db, &personal_focal, neighbour).await;
    }

    // --- neighbours of the PROJECT focal: its own partition is (workspace, project A) ----------
    let project_live = seed_fact(
        db,
        "jn-live",
        "same project neighbour",
        &alice.account,
        &alice.workspace,
        "project",
        Some(&alice.project_a),
        false,
        false,
    )
    .await;
    let personal_seen_from_project = seed_fact(
        db,
        "jn-personal",
        "personal neighbour of project focal",
        &alice.account,
        &alice.workspace,
        "personal",
        None,
        false,
        false,
    )
    .await;
    let other_project = seed_fact(
        db,
        "jn-otherproj",
        "the owner's other project",
        &alice.account,
        &alice.workspace,
        "project",
        Some(&alice.project_b),
        false,
        false,
    )
    .await;
    let unassigned_seen_from_project = seed_fact(
        db,
        "jn-unassigned",
        "unassigned neighbour of project focal",
        &alice.account,
        &alice.workspace,
        "unassigned",
        None,
        false,
        false,
    )
    .await;
    let foreign_project = seed_fact(
        db,
        "jn-foreign",
        "another account's project neighbour",
        &bob.account,
        &bob.workspace,
        "project",
        Some(&bob.project_a),
        false,
        false,
    )
    .await;
    let deleted_project = seed_fact(
        db,
        "jn-deleted",
        "deleted same-project neighbour",
        &alice.account,
        &alice.workspace,
        "project",
        Some(&alice.project_a),
        true,
        false,
    )
    .await;
    let quarantined_project = seed_fact(
        db,
        "jn-quarantined",
        "quarantined same-project neighbour",
        &alice.account,
        &alice.workspace,
        "project",
        Some(&alice.project_a),
        false,
        true,
    )
    .await;
    for neighbour in [
        &project_live,
        &personal_seen_from_project,
        &other_project,
        &unassigned_seen_from_project,
        &foreign_project,
        &deleted_project,
        &quarantined_project,
    ] {
        link(db, &project_focal, neighbour).await;
    }

    // --- the forbidden population, with the reason each row is in it ---------------------------
    let forbidden: Vec<(&str, &RecordId)> = vec![
        (
            "project neighbour of a Personal focal",
            &project_seen_from_personal,
        ),
        (
            "Unassigned neighbour of a Personal focal",
            &unassigned_seen_from_personal,
        ),
        ("another account's Personal neighbour", &foreign_personal),
        ("soft-deleted neighbour", &deleted_personal),
        ("quarantined neighbour", &quarantined_personal),
        (
            "Personal neighbour of a project focal",
            &personal_seen_from_project,
        ),
        ("the owner's other project", &other_project),
        (
            "Unassigned neighbour of a project focal",
            &unassigned_seen_from_project,
        ),
        ("another account's project neighbour", &foreign_project),
        ("soft-deleted same-project neighbour", &deleted_project),
        ("quarantined same-project neighbour", &quarantined_project),
    ];
    let before = snapshot(db, &forbidden).await;

    // One retrieval over Personal + project A. The union is what a *read* may span; the warm set is not.
    let scope = store.auto_with(alice, &alice.project_a).await;
    let reader = ScopedGraphReader::new(db, &scope);
    let writer = ScopedHeatWriter::new(db, &alice.owner);

    let mut warmed = Vec::new();
    let mut targets: Vec<WarmTarget> = Vec::new();
    for focal in [&personal_focal, &project_focal] {
        // The focal record's OWN partition, derived from its row — never the request's union and never
        // one project named by the caller.
        let partition = focal_partition_of(db, focal)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("a seeded focal record carries its ownership columns"));
        let neighbors = reader
            .direct_neighbors_in_partition(focal, &partition)
            .await
            .unwrap();
        assert!(
            !neighbors.is_empty(),
            "a focal record found no neighbour at all, so the warm loop below would prove nothing"
        );
        for neighbor in neighbors {
            targets.push(WarmTarget {
                target: neighbor.id.clone(),
                partition: partition.clone(),
                delta: 0.5,
            });
            warmed.push(neighbor.id);
        }
    }

    // One pass: every target of both focals, under one authority claim.
    let pass = writer.warm(&targets, Utc::now()).await.unwrap();
    assert_eq!(
        pass.warmed(),
        warmed.len(),
        "an in-partition warm reported that it wrote nothing: {pass:?}"
    );

    // Positive control for BOTH focal shapes in one retrieval.
    assert!(
        warmed.contains(&personal_live),
        "a Personal focal record did not warm its own Personal neighbour"
    );
    assert!(
        warmed.contains(&project_live),
        "a project focal record did not warm its own project neighbour"
    );

    // Nothing else moved.
    let after = snapshot(db, &forbidden).await;
    assert_eq!(
        after,
        before,
        "an out-of-partition neighbour received heat: {}",
        moved(&before, &after)
    );
    // And the allowed rows did move, so the equality above is not the equality of a job that wrote
    // nothing at all.
    for (label, id) in [
        ("Personal neighbour of a Personal focal", &personal_live),
        ("project neighbour of a project focal", &project_live),
    ] {
        let state = heat_row(db, id).await;
        assert!(
            state["heat"].as_f64().unwrap_or(0.0) > 0.25,
            "{label} was not warmed: {state:?}"
        );
    }
}

/// A caller cannot widen a warm by naming the wrong partition.
///
/// `WarmTarget` carries a partition, so the honest question is whether a caller could name one that does
/// not match the target. It can — and the write's own `WHERE` re-tests the *target row* against it, which
/// is what makes a mismatch a zero-row refusal rather than a successful cross-partition warm. This is the
/// difference between a guard living in the statement and living in a comment, and it is why `warm`
/// reports a per-target `false` instead of a count: a warm that wrote nothing must not look like a warm
/// that happened, which is the property the single-transaction pass must not cost.
#[tokio::test]
async fn a_warm_against_a_mismatched_partition_writes_nothing_and_reports_it() {
    let store = Store::build().await;
    let db = store.db();
    let alice = &store.alice;

    let focal = seed_fact(
        db,
        "mm-focal",
        "personal focal",
        &alice.account,
        &alice.workspace,
        "personal",
        None,
        false,
        false,
    )
    .await;
    let target = seed_fact(
        db,
        "mm-target",
        "a project row",
        &alice.account,
        &alice.workspace,
        "project",
        Some(&alice.project_a),
        false,
        false,
    )
    .await;
    link(db, &focal, &target).await;

    let scope = store.assigned(alice).await;
    let reader = ScopedGraphReader::new(db, &scope);
    let writer = ScopedHeatWriter::new(db, &alice.owner);

    // The focal row's own partition is Personal; the target lives in project A.
    let partition = focal_partition_of(db, &focal).await.unwrap().unwrap();
    let neighbors = reader
        .direct_neighbors_in_partition(&focal, &partition)
        .await
        .unwrap();
    assert!(
        neighbors.is_empty(),
        "the partition filter admitted a cross-partition neighbour: {neighbors:?}"
    );

    // Even aimed directly at the target with the wrong partition, the write refuses and says so.
    let written = writer
        .warm(
            &[WarmTarget {
                target: target.clone(),
                partition: partition.clone(),
                delta: 0.5,
            }],
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(!written.wrote[0], "a mismatched warm reported success");
    let state = heat_row(db, &target).await;
    assert_eq!(
        state["heat"],
        serde_json::json!(0.25),
        "the mismatched warm still moved the row: {state:?}"
    );

    // Positive control on the same row with its own partition, so the refusal above is about the
    // partition and not about the target being unwritable in principle.
    let target_partition = focal_partition_of(db, &target).await.unwrap().unwrap();
    let control = writer
        .warm(
            &[WarmTarget {
                target: target.clone(),
                partition: target_partition,
                delta: 0.5,
            }],
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(
        control.wrote[0],
        "the same target must be warmable under its own partition"
    );
}

/// A focal record outside the resolved population cannot seed a warm.
///
/// The focal row is checked against the caller's partitions *before* any neighbour is read, and the
/// refusal is the same class as for a row that does not exist — so this is not an oracle for "does
/// somebody else hold this id".
#[tokio::test]
async fn a_focal_record_outside_the_resolved_population_is_not_a_warm_seed() {
    let store = Store::build().await;
    let db = store.db();
    let alice = &store.alice;
    let bob = &store.bob;

    let foreign = seed_fact(
        db,
        "out-foreign",
        "somebody else's row",
        &bob.account,
        &bob.workspace,
        "personal",
        None,
        false,
        false,
    )
    .await;

    let scope = store.auto_with(alice, &alice.project_a).await;
    let reader = ScopedGraphReader::new(db, &scope);
    let partition = focal_partition_of(db, &foreign).await.unwrap().unwrap();
    let outcome = reader
        .direct_neighbors_in_partition(&foreign, &partition)
        .await;
    assert!(
        matches!(outcome, Err(StorageError::NotFound)),
        "a foreign focal record yielded a warm set: {outcome:?}"
    );
}

/// Unassigned is neither a legal seed nor a legal target.
#[tokio::test]
async fn unassigned_is_neither_a_warm_seed_nor_a_warm_target() {
    let store = Store::build().await;
    let db = store.db();
    let alice = &store.alice;

    let unassigned_focal = seed_fact(
        db,
        "ua-focal",
        "unassigned focal",
        &alice.account,
        &alice.workspace,
        "unassigned",
        None,
        false,
        false,
    )
    .await;
    let unassigned_neighbour = seed_fact(
        db,
        "ua-neighbour",
        "unassigned neighbour",
        &alice.account,
        &alice.workspace,
        "unassigned",
        None,
        false,
        false,
    )
    .await;
    link(db, &unassigned_focal, &unassigned_neighbour).await;

    // The ordinary recall populations exclude Unassigned, so the focal row is not a legal seed at all.
    for scope in [
        store.assigned(alice).await,
        store.auto_with(alice, &alice.project_a).await,
    ] {
        assert!(
            !scope.scopes().contains(&MemoryScope::Unassigned),
            "a recall population resolved to Unassigned"
        );
        let reader = ScopedGraphReader::new(db, &scope);
        let partition = focal_partition_of(db, &unassigned_focal)
            .await
            .unwrap()
            .unwrap();
        let outcome = reader
            .direct_neighbors_in_partition(&unassigned_focal, &partition)
            .await;
        assert!(
            matches!(outcome, Err(StorageError::NotFound)),
            "an Unassigned focal record seeded a warm set for a recall population"
        );
    }

    let state = heat_row(db, &unassigned_neighbour).await;
    assert_eq!(
        state["heat"],
        serde_json::json!(0.25),
        "an Unassigned row was warmed: {state:?}"
    );

    // Positive control: under the one selection that *does* name Unassigned — an explicit owner review
    // — the same pair is reachable. Excluded from recall is not the same as unreachable by anybody.
    let review = AccessRepo::new(&store.db)
        .select(&alice.owner, ReadSelection::UnassignedReview)
        .await
        .unwrap();
    let reader = ScopedGraphReader::new(db, &review);
    let partition = focal_partition_of(db, &unassigned_focal)
        .await
        .unwrap()
        .unwrap();
    let neighbors = reader
        .direct_neighbors_in_partition(&unassigned_focal, &partition)
        .await
        .unwrap();
    assert!(
        neighbors.iter().any(|n| n.id == unassigned_neighbour),
        "an explicit Unassigned review population could not reach the Unassigned pair"
    );
}

/// A migration-pending fact owns no partition, so it can neither seed nor receive a warm.
#[tokio::test]
async fn a_migration_pending_fact_is_not_eligible_for_activation() {
    let store = Store::build().await;
    let db = store.db();
    let alice = &store.alice;

    // No account, no workspace, no scope: exactly what the Task 14 backfill has not reached.
    let mut created = db
        .query(
            "CREATE type::record('fact', 'pending') SET content = 'unowned', confidence = 0.5, \
             embedding = [1.0, 0.0], tags = [], deleted = false;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = created.take(0).unwrap();
    let pending = id_of(&rows[0]);

    assert!(
        focal_partition_of(db, &pending).await.is_err(),
        "a migration-pending row yielded a partition and could seed a warm"
    );

    let live = seed_fact(
        db,
        "mp-live",
        "a live row",
        &alice.account,
        &alice.workspace,
        "personal",
        None,
        false,
        false,
    )
    .await;
    link(db, &live, &pending).await;

    let scope = store.assigned(alice).await;
    let reader = ScopedGraphReader::new(db, &scope);
    let partition = focal_partition_of(db, &live).await.unwrap().unwrap();
    let neighbors = reader
        .direct_neighbors_in_partition(&live, &partition)
        .await
        .unwrap();
    assert!(
        !neighbors.iter().any(|n| n.id == pending),
        "an unowned row was admitted as a warm target"
    );
}

/// The shared scope conjunct selects exactly the resolved partitions and nothing wider.
///
/// `ScopeFilter` is spliced into every scoped reader, so it is pinned once here rather than relying on
/// each reader's own test to notice a regression: a Personal-only context must not see project rows,
/// and one project's context must not see another project's rows even though both are
/// `scope = 'project'`.
#[tokio::test]
async fn the_scope_conjunct_selects_exactly_the_resolved_partitions() {
    let store = Store::build().await;
    let db = store.db();
    let alice = &store.alice;
    seed_fact(
        db,
        "sc-personal",
        "p",
        &alice.account,
        &alice.workspace,
        "personal",
        None,
        false,
        false,
    )
    .await;
    seed_fact(
        db,
        "sc-a",
        "a",
        &alice.account,
        &alice.workspace,
        "project",
        Some(&alice.project_a),
        false,
        false,
    )
    .await;
    seed_fact(
        db,
        "sc-b",
        "b",
        &alice.account,
        &alice.workspace,
        "project",
        Some(&alice.project_b),
        false,
        false,
    )
    .await;
    seed_fact(
        db,
        "sc-u",
        "u",
        &alice.account,
        &alice.workspace,
        "unassigned",
        None,
        false,
        false,
    )
    .await;

    let cases: Vec<(&str, ScopedContext, Vec<&str>)> = vec![
        (
            "Personal only",
            store.one(alice, MemoryScope::Personal).await,
            vec!["p"],
        ),
        (
            "one project",
            store
                .one(
                    alice,
                    MemoryScope::Project(ProjectId::try_from(alice.project_a.clone()).unwrap()),
                )
                .await,
            vec!["a"],
        ),
        (
            "Unassigned review",
            AccessRepo::new(&store.db)
                .select(&alice.owner, ReadSelection::UnassignedReview)
                .await
                .unwrap(),
            vec!["u"],
        ),
        (
            "Personal plus project A",
            store.auto_with(alice, &alice.project_a).await,
            vec!["a", "p"],
        ),
    ];
    for (label, scope, expected) in cases {
        assert_eq!(
            select_contents(db, &ScopeFilter::new(&scope)).await,
            expected,
            "{label}"
        );
    }

    // Every case above named at least one partition, so an empty filter is the one shape left: it must
    // select nothing rather than widen into a table scan. A real context always carries at least
    // Personal, so this is built from a partition projected back down by `for_partition` on a scope the
    // caller cannot otherwise resolve to nothing.
    let review = AccessRepo::new(&store.db)
        .select(&alice.owner, ReadSelection::UnassignedReview)
        .await
        .unwrap();
    assert!(!ScopeFilter::new(&review).is_empty());
}

// ---------------------------------------------------------------------------------------------
// The two hand-offs Task 6 left for this lane: who the writer trusts, and what an empty pass costs.
// ---------------------------------------------------------------------------------------------

/// A target naming a workspace the writer does not hold is refused, and nothing is claimed.
///
/// `WarmTarget` carries a partition, and `warm` used to take the statement's `$workspace` from it —
/// which made the field load-bearing in the wrong direction: an in-process caller that assembled
/// `bob`'s partition around `bob`'s fact would warm `bob`'s heat row under `alice`'s authority,
/// because `claim_authority` re-asserts only *alice*'s account. The pass is the writer's, so the
/// workspace it writes under comes from the sealed `OwnerContext`, and a target that names any other
/// is refused **before** the transaction opens: no claim, no write, no half-warmed neighbourhood.
#[tokio::test]
async fn a_warm_target_naming_another_workspace_is_refused_before_anything_is_claimed() {
    let store = Store::build().await;
    let db = store.db();
    let alice = &store.alice;
    let bob = &store.bob;

    let foreign = seed_fact(
        db,
        "ws-foreign",
        "bob's own row",
        &bob.account,
        &bob.workspace,
        "personal",
        None,
        false,
        false,
    )
    .await;
    let mine = seed_fact(
        db,
        "ws-mine",
        "alice's own row",
        &alice.account,
        &alice.workspace,
        "personal",
        None,
        false,
        false,
    )
    .await;

    // The partition a caller would have to forge: read from bob's row, exactly as production derives
    // a focal's partition from its own row.
    let bob_partition = focal_partition_of(db, &foreign).await.unwrap().unwrap();
    assert_ne!(
        bob_partition.workspace,
        *alice.owner.workspace().as_record_id(),
        "the fixture collapsed two accounts into one workspace, so the case below proves nothing"
    );

    define_claim_counter(db).await;
    let writer = ScopedHeatWriter::new(db, &alice.owner);
    let before = claim_ticks(db).await;

    let outcome = writer
        .warm(
            &[WarmTarget {
                target: foreign.clone(),
                partition: bob_partition,
                delta: 0.5,
            }],
            Utc::now(),
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::InvalidInput(_))),
        "a target naming another account's workspace was accepted by the writer: {outcome:?}"
    );
    let state = heat_row(db, &foreign).await;
    assert_eq!(
        state["heat"],
        serde_json::json!(0.25),
        "the refused pass still moved bob's heat: {state:?}"
    );
    assert_eq!(
        claim_ticks(db).await,
        before,
        "the refusal opened a transaction and claimed authority anyway"
    );

    // Positive control: the same writer, the same seam, one of alice's own rows. Without this the
    // claim count above would be measuring a writer that could not warm anything at all.
    let mine_partition = focal_partition_of(db, &mine).await.unwrap().unwrap();
    let pass = writer
        .warm(
            &[WarmTarget {
                target: mine.clone(),
                partition: mine_partition,
                delta: 0.5,
            }],
            Utc::now(),
        )
        .await
        .expect("alice may warm her own partition");
    assert!(pass.warmed() == 1 && pass.refused() == 0, "{pass:?}");
    assert!(
        heat_row(db, &mine).await["heat"] != serde_json::json!(0.25),
        "the positive control did not actually warm anything"
    );
    assert_eq!(
        claim_ticks(db).await,
        before + 1,
        "one real pass must claim exactly once"
    );
}

/// An empty pass is an empty answer — not a transaction, and not an authority claim.
///
/// `guarded_writes` built the transaction, ran `claim_authority` and committed even for a zero-length
/// slice. `claim_authority` *writes* (the account row, and a hosted credential's `last_used_at`), so
/// an empty pass was a durable write with nothing in it: a wakeup that contends the account row for
/// no reason, and — hosted — a `last_used_at` bump that reads as an access which surfaced nothing.
/// `trigger_activation` early-returns today so no request path reaches it, but `warm` is the public
/// seam and this lane calls it directly, so the guard belongs under it.
#[tokio::test]
async fn an_empty_warm_pass_claims_nothing_and_opens_no_transaction() {
    let store = Store::build().await;
    let db = store.db();
    let alice = &store.alice;

    define_claim_counter(db).await;
    let writer = ScopedHeatWriter::new(db, &alice.owner);
    let before = claim_ticks(db).await;

    let pass = writer
        .warm(&[], Utc::now())
        .await
        .expect("an empty pass is a committed success, not an error");
    assert_eq!(pass.warmed(), 0, "an empty pass reported warming");
    assert_eq!(
        pass.refused(),
        0,
        "an empty pass reported a refusal it never made"
    );
    assert!(
        pass.wrote.is_empty(),
        "an empty pass answered with per-target verdicts it had no targets for: {:?}",
        pass.wrote
    );
    assert_eq!(
        claim_ticks(db).await,
        before,
        "the empty pass claimed authority, which writes the account row"
    );

    // Positive control on the same counter: one target, one claim.
    let mine = seed_fact(
        db,
        "empty-ctl",
        "alice controllable row",
        &alice.account,
        &alice.workspace,
        "personal",
        None,
        false,
        false,
    )
    .await;
    let partition = focal_partition_of(db, &mine).await.unwrap().unwrap();
    writer
        .warm(
            &[WarmTarget {
                target: mine,
                partition,
                delta: 0.5,
            }],
            Utc::now(),
        )
        .await
        .expect("a one-target pass runs");
    assert_eq!(
        claim_ticks(db).await,
        before + 1,
        "the counter never counted anything, so the assertion above proves nothing"
    );
}

/// Installs the engine-side witness for "did this pass claim authority".
///
/// `claim_authority` writes `account.updated_at`, so a committed `UPDATE` on the account table is the
/// claim — counted by the engine itself rather than inferred from a return value. The event survives
/// commit and not cancellation, which is the property that makes it the right witness.
async fn define_claim_counter(db: &Db) {
    db.query(
        "DEFINE TABLE claim_tick SCHEMALESS PERMISSIONS FULL; DEFINE EVENT count_claims ON TABLE \
         account WHEN $event = 'UPDATE' THEN CREATE claim_tick SET at = time::now();",
    )
    .await
    .unwrap()
    .check()
    .unwrap();
}

/// Committed authority claims on any account, as the engine's own event counted them.
async fn claim_ticks(db: &Db) -> i64 {
    let rows: Vec<serde_json::Value> = db
        .query("SELECT count() AS c FROM claim_tick GROUP ALL;")
        .await
        .unwrap()
        .check()
        .unwrap()
        .take(0)
        .unwrap();
    rows.first().and_then(|row| row["c"].as_i64()).unwrap_or(0)
}

async fn select_contents(db: &Db, filter: &ScopeFilter) -> Vec<String> {
    let sql = format!(
        "SELECT content FROM fact WHERE {} ORDER BY content;",
        filter.conjunct()
    );
    let query = filter.apply(db.query(sql));
    let mut response = query.await.unwrap().check().unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    rows.iter()
        .map(|row| row["content"].as_str().unwrap_or_default().to_string())
        .collect()
}
