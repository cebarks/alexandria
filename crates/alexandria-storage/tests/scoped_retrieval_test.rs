//! Task 7: the scoped top-k read is eligible-first in **both** index states.
//!
//! The one property this suite owns is that `k` truncates *the caller's own population*, never the
//! store. The failure mode it exists to prevent is silent and directional: a candidate read that
//! takes the `k` nearest rows store-wide and filters afterwards answers a Personal-only caller with
//! whatever another account happened to have nearby. It returns *fewer* rows rather than wrong ones,
//! so it reads as "no matches", and how many rows the caller loses is a function of somebody else's
//! corpus — a leak of shape even when no foreign body ever appears.
//!
//! Two shapes are pinned, because `retrieve_memories` picks between them at boot:
//!
//! * `indexed` — the numeric-ef HNSW form, valid only once
//!   [`ensure_vector_index`](alexandria_storage::schema::ensure_vector_index) succeeded;
//! * `exact` — the brute-force cosine form, which is the answer when it did not.
//!
//! Each shape is pinned twice over, because the two proofs are not interchangeable: a plan assertion
//! says the engine took the scan everyone believes it took, and a row assertion says the rows that
//! came back are the caller's. `MemoryRepo::nearest`'s unscoped twin has both halves already; the
//! scoped statement is the one a partition predicate could quietly move out of the scan.
//!
//! Owners and contexts come from real rows through the sealed producers — `bootstrap_owner`,
//! `local_principal` under this instance's claimed capability, `authorize`, `select`. There is no
//! test-only authority constructor here because none may exist, and every seeded fact is written by
//! `ScopedMemoryRepo::create_fact` — the seam the store path uses — so a row this suite reads is a
//! row production could have written.

use alexandria_storage::Database;
use alexandria_storage::access::{AccessRepo, OwnerContext, ScopedContext, SystemContext};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::{OperationId, ProjectId, ReadSelection, normalize_login};
use alexandria_storage::repos::{
    IdentityRepo, MAX_KNN_K, RetrievalStrategy, ScopedHeatWriter, ScopedMemoryRepo,
    ScopedSessionRepo, WarmTarget, WriteTarget, focal_partition_of,
};
use chrono::Utc;
use surrealdb::types::{RecordId, ToSql};

/// The query every case scores against. All fixture vectors are two-dimensional so a cosine is exact
/// arithmetic rather than a statistical claim: `cos([1,0], [c, √(1−c²)]) = c`.
const QUERY: [f32; 2] = [1.0, 0.0];

/// One owner in this store, with two projects so "the owner's *other* project" is a real row rather
/// than an id nobody owns — which would make the exclusion pass for the wrong reason.
struct Owner {
    account: RecordId,
    workspace: RecordId,
    project_a: RecordId,
    project_b: RecordId,
    context: OwnerContext,
}

/// One store holding two owners.
///
/// Two accounts must share a *store* for eligible-first to mean anything: with one owner in the
/// database there is nothing out of scope to be displaced by.
struct Store {
    db: Database,
    alice: Owner,
    bob: Owner,
}

impl Store {
    async fn build() -> Self {
        let mut db = Database::connect_embedded().await.unwrap();
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let system = db.claim_system_authority().unwrap();
        let db = db;

        let mut owners = Vec::with_capacity(2);
        for login in ["ret7-alice", "ret7-bob"] {
            owners.push(enroll(&db, &system, login).await);
        }
        let mut owners = owners.into_iter();
        Self {
            db,
            alice: owners.next().unwrap(),
            bob: owners.next().unwrap(),
        }
    }

    fn db(&self) -> &surrealdb::Surreal<surrealdb::engine::any::Any> {
        self.db.inner()
    }

    /// The Personal-only population — `ReadSelection::Auto(None)`, what automatic recall resolves to
    /// when the caller names no project.
    async fn personal_only(&self, owner: &Owner) -> ScopedContext {
        self.select(owner, ReadSelection::Auto(None)).await
    }

    /// Personal plus one named project: the union automatic recall is allowed to read.
    async fn personal_with(&self, owner: &Owner, project: &RecordId) -> ScopedContext {
        self.select(
            owner,
            ReadSelection::Auto(Some(ProjectId::try_from(project.clone()).unwrap())),
        )
        .await
    }

    async fn select(&self, owner: &Owner, selection: ReadSelection) -> ScopedContext {
        AccessRepo::new(&self.db)
            .select(&owner.context, selection)
            .await
            .unwrap()
    }

    fn repo<'a>(&'a self, owner: &'a Owner, scope: &'a ScopedContext) -> ScopedMemoryRepo<'a> {
        ScopedMemoryRepo::new(self.db.inner(), &owner.context, scope)
    }

    /// Seeds one live fact through the production write seam and returns its handle.
    async fn seed(&self, owner: &Owner, target: Target<'_>, content: &str, cosine: f32) -> String {
        let scope = match target {
            Target::Personal => self.personal_only(owner).await,
            Target::Project(project) => self.personal_with(owner, project).await,
        };
        let repo = self.repo(owner, &scope);
        let fact = repo
            .create_fact(
                content,
                0.5,
                &unit_at(cosine),
                &[],
                match target {
                    Target::Personal => WriteTarget::Personal,
                    Target::Project(project) => {
                        WriteTarget::Project(ProjectId::try_from(project.clone()).unwrap())
                    }
                },
                Utc::now(),
            )
            .await
            .unwrap();
        alexandria_storage::record_id_to_string(fact.id.as_ref().expect("a created fact has an id"))
    }

    /// Moves one of the owner's own rows out of the live population: `deleted` through the scoped
    /// soft-delete (a production writer), `quarantined_at` by the column itself, because no
    /// request-facing writer produces quarantine yet.
    async fn hide(&self, owner: &Owner, id: &str, way: Hide) {
        let handle = RecordId::parse_simple(id).expect("a handle this file produced parses");
        match way {
            Hide::Deleted => {
                let scope = self.personal_only(owner).await;
                self.repo(owner, &scope)
                    .soft_delete(&handle, Utc::now())
                    .await
                    .unwrap();
            }
            Hide::Quarantined => {
                self.db()
                    .query("UPDATE $id SET quarantined_at = time::now();")
                    .bind(("id", handle))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            }
        }
    }

    /// The ids a read returned, sorted, for set comparisons against the ids this file seeded.
    fn ids(facts: &[alexandria_storage::models::Fact]) -> Vec<String> {
        let mut out: Vec<String> = facts
            .iter()
            .map(|f| {
                alexandria_storage::record_id_to_string(
                    f.id.as_ref().expect("a read row carries an id"),
                )
            })
            .collect();
        out.sort();
        out
    }

    /// Sorts a set of seeded handles the same way [`Store::ids`] sorts a read, so the two compare.
    fn sorted(ids: &[String]) -> Vec<String> {
        let mut out = ids.to_vec();
        out.sort();
        out
    }
}

enum Target<'a> {
    Personal,
    Project(&'a RecordId),
}

enum Hide {
    Deleted,
    Quarantined,
}

/// One plan attestation, both directions, on the statement the read itself assembles.
///
/// Rows cannot tell an index scan from a brute-force one, and on 3.3 the difference is invisible in the
/// worst way: `<|k,ef|>` with no index behind it matches no rows at all, so a dropped index reads as an
/// empty population. The Task 6 accepted gate pinned this from inside the crate; the brief asks the
/// durable suite to hold it too, at the statement production runs.
#[tokio::test]
async fn the_two_scoped_statements_plan_as_their_own_scans_and_keep_their_predicates() {
    let store = Store::build().await;
    let scope = store.personal_only(&store.alice).await;
    let repo = store.repo(&store.alice, &scope);

    let brute = repo
        .explain_nearest(&QUERY, 3, RetrievalStrategy::Exact)
        .await
        .unwrap();
    assert!(
        brute.contains("KnnTopK"),
        "the fallback form is the brute-force scan and its plan must say so: {brute}"
    );
    assert!(
        !brute.contains("KnnScan"),
        "the fallback form was planned as an index scan: {brute}"
    );

    define_index(&store).await;
    let indexed = repo
        .explain_nearest(&QUERY, 3, RetrievalStrategy::Indexed)
        .await
        .unwrap();
    assert!(
        indexed.contains("KnnScan"),
        "the scoped kNN dropped off the index: {indexed}"
    );
    assert!(
        indexed.contains("fact_embedding_hnsw"),
        "the scoped kNN's scan is not the HNSW index: {indexed}"
    );

    // The plan belongs to the *scoped* statement: the live predicate and the partition conjunct are in
    // it, so a planner that moved them outside the scan would change this text too.
    let sql = repo.nearest_statement(3, RetrievalStrategy::Indexed);
    assert!(
        sql.contains("quarantined_at = NONE") && sql.contains("project INSIDE $scope_projects"),
        "the statement the plan gate just ran is not the scoped one: {sql}"
    );
}

async fn enroll(db: &Database, system: &SystemContext, login: &str) -> Owner {
    let ids = IdentityRepo::new(db.inner())
        .bootstrap_owner(
            login,
            &format!("$argon2id$fixture-prepared-hash-{login}-padding"),
            &OperationId::new(format!("scoped-retrieval-{login}")),
        )
        .await
        .unwrap();
    let access = AccessRepo::new(db);
    let local = access.local_principal(system, &ids.account).await.unwrap();
    let context = access.authorize(&local).await.unwrap();
    let workspace = ids.workspace.as_record_id().clone();
    Owner {
        account: ids.account.as_record_id().clone(),
        workspace: workspace.clone(),
        project_a: create_project(db, &workspace, &format!("{login}-alpha")).await,
        project_b: create_project(db, &workspace, &format!("{login}-beta")).await,
        context,
    }
}

async fn create_project(db: &Database, workspace: &RecordId, alias: &str) -> RecordId {
    let mut response = db
        .inner()
        .query("CREATE project SET workspace = $workspace, normalized_alias = $alias RETURN AFTER;")
        .bind(("workspace", workspace.clone()))
        .bind(("alias", normalize_login(alias)))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    record_of(&rows.into_iter().next().unwrap())
}

/// A created row's `id` as a typed handle.
fn record_of(row: &serde_json::Value) -> RecordId {
    RecordId::parse_simple(row["id"].as_str().expect("a created row has an id"))
        .expect("a handle the store handed out parses")
}

/// A unit vector whose cosine with `QUERY` is exactly `c`.
fn unit_at(c: f32) -> Vec<f32> {
    vec![c, (1.0 - c * c).max(0.0).sqrt()]
}

/// Defines the HNSW index over the two-dimensional fixture vectors, as boot does when the define
/// succeeds.
async fn define_index(store: &Store) {
    alexandria_storage::schema::ensure_vector_index(store.db(), 2)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// Eligible-first: closer out-of-scope vectors, in both states
// ---------------------------------------------------------------------------------------------

/// The exact fallback must not be "the store's `k` nearest, then drop what is not mine".
///
/// bob holds three rows at cosine 0.99; alice holds three at 0.5. A read of `k = 3` for alice that
/// truncated before filtering returns *nothing* — and "nothing" is what an agent reads as "this
/// account holds nothing near that query".
#[tokio::test]
async fn the_exact_read_answers_every_eligible_row_though_every_foreign_vector_is_closer() {
    let store = Store::build().await;
    for i in 0..3 {
        store
            .seed(
                &store.bob,
                Target::Personal,
                &format!("bobs closer note {i}"),
                0.99,
            )
            .await;
    }
    let mut mine = Vec::new();
    for i in 0..3 {
        mine.push(
            store
                .seed(&store.alice, Target::Personal, &format!("my note {i}"), 0.5)
                .await,
        );
    }

    let scope = store.personal_only(&store.alice).await;
    let hits = store
        .repo(&store.alice, &scope)
        .nearest(&QUERY, 3, RetrievalStrategy::Exact)
        .await
        .unwrap();

    assert_eq!(
        Store::ids(&hits),
        Store::sorted(&mine),
        "the exact fallback truncated a global neighbour list and filtered afterwards"
    );
}

/// The indexed read must satisfy the same contract, not merely the same plan.
///
/// This is the case a plan gate alone cannot see: `KnnScan` is the scan everyone wants, and it can
/// still be a scan of the whole index whose rows are filtered after `k` of them were chosen.
#[tokio::test]
async fn the_indexed_read_answers_every_eligible_row_though_every_foreign_vector_is_closer() {
    let store = Store::build().await;
    define_index(&store).await;
    for i in 0..3 {
        store
            .seed(
                &store.bob,
                Target::Personal,
                &format!("bobs closer note {i}"),
                0.99,
            )
            .await;
    }
    let mut mine = Vec::new();
    for i in 0..3 {
        mine.push(
            store
                .seed(&store.alice, Target::Personal, &format!("my note {i}"), 0.5)
                .await,
        );
    }

    let scope = store.personal_only(&store.alice).await;
    let hits = store
        .repo(&store.alice, &scope)
        .nearest(&QUERY, 3, RetrievalStrategy::Indexed)
        .await
        .unwrap();

    assert_eq!(
        Store::ids(&hits),
        Store::sorted(&mine),
        "the indexed read truncated global neighbours before applying the partition predicates"
    );
}

// ---------------------------------------------------------------------------------------------
// Population shapes: Personal only, Personal+project, index absent, corpus smaller than k
// ---------------------------------------------------------------------------------------------

/// A Personal-only read sees Personal rows and *no* project rows — not even closer ones.
#[tokio::test]
async fn a_personal_only_read_excludes_the_owners_own_project_rows() {
    let store = Store::build().await;
    let personal = store
        .seed(&store.alice, Target::Personal, "my personal note", 0.5)
        .await;
    let project = store
        .seed(
            &store.alice,
            Target::Project(&store.alice.project_a),
            "my project note",
            0.9,
        )
        .await;

    for strategy in [RetrievalStrategy::Exact, RetrievalStrategy::Indexed] {
        if strategy == RetrievalStrategy::Indexed {
            define_index(&store).await;
        }
        let scope = store.personal_only(&store.alice).await;
        let got = Store::ids(
            &store
                .repo(&store.alice, &scope)
                .nearest(&QUERY, 5, strategy)
                .await
                .unwrap(),
        );
        assert_eq!(
            got,
            vec![personal.clone()],
            "a Personal-only read did not answer exactly the Personal population (strategy={strategy:?})"
        );
        assert!(
            !got.contains(&project),
            "the owner's own project row survived the Personal-only filter (strategy={strategy:?})"
        );
    }
}

/// Personal plus *the selected* project reads both partitions and still none of the other project.
///
/// The union is the point: `Auto(Some(p))` is Personal ∪ p, and a conjunct that tested only
/// `scope = 'project'` would read every project in the workspace.
#[tokio::test]
async fn a_personal_plus_project_read_covers_both_partitions_and_nothing_of_the_other_project() {
    let store = Store::build().await;
    let personal = store
        .seed(&store.alice, Target::Personal, "my personal note", 0.4)
        .await;
    let selected = store
        .seed(
            &store.alice,
            Target::Project(&store.alice.project_a),
            "my alpha note",
            0.6,
        )
        .await;
    let other = store
        .seed(
            &store.alice,
            Target::Project(&store.alice.project_b),
            "my beta note",
            0.95,
        )
        .await;

    for strategy in [RetrievalStrategy::Exact, RetrievalStrategy::Indexed] {
        if strategy == RetrievalStrategy::Indexed {
            define_index(&store).await;
        }
        let scope = store
            .personal_with(&store.alice, &store.alice.project_a)
            .await;
        let got = Store::ids(
            &store
                .repo(&store.alice, &scope)
                .nearest(&QUERY, 5, strategy)
                .await
                .unwrap(),
        );
        assert_eq!(
            got,
            Store::sorted(&[personal.clone(), selected.clone()]),
            "the Personal+project union did not read exactly its two partitions (strategy={strategy:?})"
        );
        assert!(
            !got.contains(&other),
            "the owner's unselected project leaked into the union (strategy={strategy:?})"
        );
    }
}

/// With no index defined, only the fallback answers — and it answers *filtered*.
///
/// Both halves matter: 4.0 rejects the numeric-ef form without an eligible index. The server must
/// select the fallback when boot did not define the index; this is not a caller option.
#[tokio::test]
async fn without_a_defined_index_only_the_fallback_answers_and_it_still_filters() {
    let store = Store::build().await;
    for i in 0..3 {
        store
            .seed(
                &store.bob,
                Target::Personal,
                &format!("bobs closer note {i}"),
                0.99,
            )
            .await;
    }
    let mine = store
        .seed(&store.alice, Target::Personal, "my note", 0.5)
        .await;

    let scope = store.personal_only(&store.alice).await;
    let repo = store.repo(&store.alice, &scope);
    let hits = Store::ids(
        &repo
            .nearest(&QUERY, 3, RetrievalStrategy::Exact)
            .await
            .unwrap(),
    );
    assert_eq!(
        hits,
        vec![mine],
        "the no-index fallback did not serve the eligible row"
    );

    let error = repo
        .nearest(&QUERY, 3, RetrievalStrategy::Indexed)
        .await
        .expect_err("numeric-ef kNN requires an index");
    let StorageError::Internal(cause) = error else {
        panic!("expected an internal storage error, got {error}");
    };
    assert!(
        cause.to_string().contains("needs an HNSW or DiskANN index"),
        "indexed retrieval failed for the wrong reason: {cause}"
    );
}

/// An eligible corpus smaller than `k` returns all of it and stops.
#[tokio::test]
async fn an_eligible_corpus_smaller_than_k_returns_every_eligible_row_and_nothing_else() {
    let store = Store::build().await;
    for i in 0..8 {
        store
            .seed(
                &store.bob,
                Target::Personal,
                &format!("bobs note {i}"),
                0.99,
            )
            .await;
    }
    let mut mine = Vec::new();
    for i in 0..2 {
        mine.push(
            store
                .seed(&store.alice, Target::Personal, &format!("my note {i}"), 0.5)
                .await,
        );
    }

    for strategy in [RetrievalStrategy::Exact, RetrievalStrategy::Indexed] {
        if strategy == RetrievalStrategy::Indexed {
            define_index(&store).await;
        }
        let scope = store.personal_only(&store.alice).await;
        let hits = store
            .repo(&store.alice, &scope)
            .nearest(&QUERY, 10, strategy)
            .await
            .unwrap();
        assert_eq!(
            Store::ids(&hits),
            Store::sorted(&mine),
            "a two-row eligible corpus asked for with k=10 did not answer exactly itself (strategy={strategy:?})"
        );
    }
}

/// Hidden rows of the caller's own account are not candidates, in either state.
#[tokio::test]
async fn deleted_and_quarantined_rows_of_the_callers_own_account_are_never_returned() {
    let store = Store::build().await;
    let live = store
        .seed(&store.alice, Target::Personal, "still eligible", 0.5)
        .await;
    let deleted = store
        .seed(&store.alice, Target::Personal, "soft deleted", 0.9)
        .await;
    let quarantined = store
        .seed(&store.alice, Target::Personal, "quarantined", 0.95)
        .await;
    store.hide(&store.alice, &deleted, Hide::Deleted).await;
    store
        .hide(&store.alice, &quarantined, Hide::Quarantined)
        .await;

    for strategy in [RetrievalStrategy::Exact, RetrievalStrategy::Indexed] {
        if strategy == RetrievalStrategy::Indexed {
            define_index(&store).await;
        }
        let scope = store.personal_only(&store.alice).await;
        let got = Store::ids(
            &store
                .repo(&store.alice, &scope)
                .nearest(&QUERY, 5, strategy)
                .await
                .unwrap(),
        );
        assert_eq!(
            got,
            vec![live.clone()],
            "a hidden row of the caller's own account survived the live predicate (strategy={strategy:?})"
        );
    }
}

/// A cross-account candidate never surfaces, whichever scan ran.
#[tokio::test]
async fn another_accounts_rows_never_surface_in_either_state() {
    let store = Store::build().await;
    for i in 0..4 {
        store
            .seed(
                &store.bob,
                Target::Personal,
                &format!("bobs private note {i}"),
                0.999,
            )
            .await;
    }
    let mine = store
        .seed(&store.alice, Target::Personal, "my note", 0.3)
        .await;

    for strategy in [RetrievalStrategy::Exact, RetrievalStrategy::Indexed] {
        if strategy == RetrievalStrategy::Indexed {
            define_index(&store).await;
        }
        let scope = store.personal_only(&store.alice).await;
        let hits = store
            .repo(&store.alice, &scope)
            .nearest(&QUERY, 4, strategy)
            .await
            .unwrap();
        assert_eq!(
            Store::ids(&hits),
            vec![mine.clone()],
            "bob's closer rows displaced or replaced alice's in the read (strategy={strategy:?})"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Bounded k: the number that reaches the statement
// ---------------------------------------------------------------------------------------------

/// `k` is bounded where it is interpolated, not only at the tool boundary.
///
/// `nearest_sql` puts `k` into the KNN operator as a literal, so the ceiling cannot live only in the
/// MCP crate: a storage caller that forgot to clamp would send an unbounded number straight into
/// statement text. Today `usize::MAX` reaches the parser as a twenty-digit literal and the read
/// fails; the contract is that an absurd `k` answers like a large one instead of erroring or
/// reaching the engine unbounded.
#[tokio::test]
async fn an_absurd_k_answers_like_a_large_one_instead_of_reaching_the_engine_unbounded() {
    let store = Store::build().await;
    for i in 0..3 {
        store
            .seed(&store.alice, Target::Personal, &format!("my note {i}"), 0.5)
            .await;
    }
    let scope = store.personal_only(&store.alice).await;
    let repo = store.repo(&store.alice, &scope);

    let absurd = repo
        .nearest(&QUERY, usize::MAX, RetrievalStrategy::Exact)
        .await;
    let large = repo.nearest(&QUERY, 100, RetrievalStrategy::Exact).await;
    // The comparison below is only worth running if the large read actually read: two empty results
    // "answer alike" for any repo at all, including one that cannot return a row.
    let large_ids = large.as_ref().map(|f| Store::ids(f)).unwrap_or_default();
    assert!(
        !large_ids.is_empty(),
        "a k of 100 over three seeded rows returned {large:?}, so the equality below has nothing to \
         discriminate and this fixture proves nothing"
    );
    assert_eq!(
        absurd.as_ref().map(|f| Store::ids(f)).unwrap_or_default(),
        large_ids,
        "a k past the documented ceiling is not the same read as a k at it: {absurd:?} vs {large:?}"
    );

    // The bound lives where the literal is built, so it holds for any caller, not just this one.
    for strategy in [RetrievalStrategy::Exact, RetrievalStrategy::Indexed] {
        let text = repo.nearest_statement(usize::MAX, strategy);
        assert!(
            !text.contains("18446744073709551615"),
            "the caller's unbounded k reached statement text: {text}"
        );
        assert!(
            text.contains(&format!("<|{MAX_KNN_K},")),
            "the clamped k is not the documented ceiling {MAX_KNN_K}: {text}"
        );
    }
}

/// A `k` of zero asks for nothing, and the answer must be nothing — not an error, and not a row.
///
/// `k = 1` is asserted first, on the same repo and the same scope: `hits.is_empty()` alone is also
/// satisfied by a read path that returns nothing for *every* `k`, which is the vacuity `AGENTS.md`
/// tells every "nothing happened" assertion to pair with a positive control proving the fixture can
/// do something at all.
#[tokio::test]
async fn a_zero_k_reads_no_rows_without_breaking_the_statement() {
    let store = Store::build().await;
    let mine = store
        .seed(&store.alice, Target::Personal, "my note", 0.5)
        .await;
    let scope = store.personal_only(&store.alice).await;
    let repo = store.repo(&store.alice, &scope);

    let one = repo
        .nearest(&QUERY, 1, RetrievalStrategy::Exact)
        .await
        .expect("a bounded read of one neighbour is the ordinary case");
    assert_eq!(
        Store::ids(&one),
        vec![mine],
        "the positive control: this fixture can return the seeded row, so the empty answer below is \
         the k = 0 behaviour and not a read path that returns nothing at all"
    );

    let hits = repo
        .nearest(&QUERY, 0, RetrievalStrategy::Exact)
        .await
        .expect("asking for zero neighbours is a bounded read, not a parser accident");
    assert!(
        hits.is_empty(),
        "k = 0 asked for nothing and got {:?}, which is not the answer",
        Store::ids(&hits)
    );
}

// ---------------------------------------------------------------------------------------------
// Session intersection
// ---------------------------------------------------------------------------------------------

/// A lawful session narrows the candidate set to *that session's* live rows.
///
/// `retrieve_memories(session_id: …)` reads the session's facts and ranks them, so the session read
/// **is** the population and eligibility there means "a member of this session, mine, live". A
/// foreign handle must answer exactly as an absent one does, or the handle becomes an existence
/// oracle for other accounts' session ids.
#[tokio::test]
async fn a_lawful_session_read_returns_only_that_sessions_live_rows() {
    let store = Store::build().await;
    let handle = "pi:scoped-retrieval-session";
    let sessions = ScopedSessionRepo::new(store.db(), store.alice.context.clone());
    sessions
        .find_or_create(handle, Some("agent"), None)
        .await
        .unwrap();

    let mine = store
        .seed(&store.alice, Target::Personal, "in the session", 0.5)
        .await;
    let hidden = store
        .seed(
            &store.alice,
            Target::Personal,
            "in the session but deleted",
            0.9,
        )
        .await;
    let outside = store
        .seed(&store.alice, Target::Personal, "never in the session", 0.95)
        .await;
    sessions.add_memory(handle, &mine).await.unwrap();
    sessions.add_memory(handle, &hidden).await.unwrap();
    store.hide(&store.alice, &hidden, Hide::Deleted).await;

    let members = sessions.get_memories(handle).await.unwrap();
    assert_eq!(
        Store::ids(&members),
        vec![mine.clone()],
        "the session read was not exactly the session's own live member set"
    );
    assert!(
        !Store::ids(&members).contains(&outside),
        "the session read returned a fact that is not a member of the session"
    );

    let bob_sessions = ScopedSessionRepo::new(store.db(), store.bob.context.clone());
    let foreign = bob_sessions.get_memories(handle).await.unwrap();
    let absent = bob_sessions
        .get_memories("pi:nobody-holds-this")
        .await
        .unwrap();
    assert!(
        foreign.is_empty() && absent.is_empty(),
        "a foreign session handle and an absent one answered differently, which makes the first an \
         existence oracle: {foreign:?} vs {absent:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Performance measurement — `#[ignore]`d so the suite stays fast, run explicitly for the log
// ---------------------------------------------------------------------------------------------

/// Wall time of the two scoped candidate reads, over the corpus shapes the request path meets, with a
/// second account holding vectors closer than every eligible row.
///
/// Not a `bench-retrieval` run: owner decision D3 keeps the benchmark product out of this lane, and the
/// benchmark measures retrieval *calibration* against a locked model, whereas this measures one property
/// — how an eligible-first read scales with the caller's own population and with index state. Everything
/// is `mem://` and synthetic; no live corpus and no model artifacts are touched.
///
/// Row counts and why:
///
/// * 100 eligible rows for the small corpora — the shape a fresh or lightly-used account meets.
/// * **1,000 eligible rows** for the large synthetic account. The fallback form scores every *eligible*
///   row, so its cost is linear in exactly that number; 1,000 is the smallest count at which the scan
///   dominates the round trip on this engine, and it keeps an explicit measurement run in seconds. The
///   fixture vectors are two-dimensional, so a production 384-dimension model pays more per row than
///   these numbers show: treat them as a floor for the scan, not as production estimates — measuring the
///   real thing is Tasks 12/23's benchmark work.
/// * 500 rows for the foreign account, each closer to the query than any eligible row. If the read
///   truncated globally and filtered afterwards, the caller's row count and timing would move with *this*
///   number. The printed row counts are part of the measurement for that reason.
///
/// Run with:
/// `cargo test --locked -p alexandria-storage --test scoped_retrieval_test -- --ignored --nocapture`
#[tokio::test]
#[ignore = "measures wall time over synthetic corpora; run explicitly and capture the output"]
async fn measure_scoped_retrieval_and_warm_cost() {
    const QUERIES: usize = 50;
    const FOREIGN: usize = 500;

    let mut lines = Vec::new();
    for (label, eligible, project_rows) in [
        ("personal-only small", 100usize, 0usize),
        ("personal+project", 50, 50),
        ("large synthetic account", 1_000, 0),
    ] {
        let store = Store::build().await;
        bulk_seed(&store, &store.alice, "m", eligible, 0.5, None).await;
        bulk_seed(
            &store,
            &store.alice,
            "p",
            project_rows,
            0.55,
            Some(&store.alice.project_a),
        )
        .await;
        bulk_seed(&store, &store.bob, "f", FOREIGN, 0.999, None).await;

        let scope = if project_rows > 0 {
            store
                .personal_with(&store.alice, &store.alice.project_a)
                .await
        } else {
            store.personal_only(&store.alice).await
        };
        let repo = store.repo(&store.alice, &scope);
        let expected = eligible + project_rows;

        for strategy in [RetrievalStrategy::Exact, RetrievalStrategy::Indexed] {
            if strategy == RetrievalStrategy::Indexed {
                define_index(&store).await;
            }
            // Which scan the timing below belongs to, from the plan rather than from the flag.
            let plan = repo.explain_nearest(&QUERY, 10, strategy).await.unwrap();
            match strategy {
                RetrievalStrategy::Exact => assert!(
                    plan.contains("KnnTopK") && !plan.contains("KnnScan"),
                    "the 'exact' measurement was not a brute-force plan: {plan}"
                ),
                RetrievalStrategy::Indexed => assert!(
                    plan.contains("KnnScan") && plan.contains("fact_embedding_hnsw"),
                    "the 'indexed' measurement was not planned on the HNSW index: {plan}"
                ),
            }

            let started = std::time::Instant::now();
            let mut rows = 0usize;
            for _ in 0..QUERIES {
                rows += repo.nearest(&QUERY, 10, strategy).await.unwrap().len();
            }
            let total_us = started.elapsed().as_micros() as f64;
            lines.push(format!(
                "MEASURE corpus={label} eligible={expected} foreign={FOREIGN} strategy={} k=10 queries={QUERIES} total_us={total_us:.0} per_query_us={:.1} rows_per_query={}",
                strategy.as_str(),
                total_us / QUERIES as f64,
                rows / QUERIES,
            ));
            assert_eq!(
                rows / QUERIES,
                10.min(expected),
                "{label}/{strategy:?}: the caller's answer size depended on the foreign corpus"
            );
        }

        // Repeated activation, on the same store: the guarded pass claims once per pass whatever the
        // target count, so its per-target cost falls with the number of neighbours warmed.
        let targets = warm_targets(&store, &store.alice, 20).await;
        assert_eq!(
            targets.len(),
            20,
            "the warm measurement could not assemble 20 targets, so it would time nothing"
        );
        let writer = ScopedHeatWriter::new(store.db(), &store.alice.context);
        // One claim per pass is the structure under test, so the measurement runs the pass at two
        // target counts: the difference is the marginal cost of a neighbour, and the remainder is
        // what every retrieval pays once.
        for count in [1usize, targets.len()] {
            let slice = &targets[..count];
            let started = std::time::Instant::now();
            for _ in 0..QUERIES {
                let pass = writer.warm(slice, Utc::now()).await.unwrap();
                assert_eq!(pass.refused(), 0, "a seeded target was refused: {pass:?}");
            }
            let total_us = started.elapsed().as_micros() as f64;
            lines.push(format!(
                "MEASURE corpus={label} activation_targets={count} passes={QUERIES} total_us={total_us:.0} per_pass_us={:.1} per_target_us={:.1}",
                total_us / QUERIES as f64,
                total_us / (QUERIES * count) as f64,
            ));
        }
    }

    for line in &lines {
        println!("{line}");
    }
}

/// Seeds `count` owned facts in one multi-statement query.
///
/// Bulk on purpose: what is measured is a *read's* cost over a population, so paying a guarded
/// transaction per seeded row would time the writer instead. The columns are the ones
/// `ScopedMemoryRepo::create_fact` writes — `account`, `workspace`, `scope`, `deleted`, `embedding`,
/// `tags`, `confidence` — plus the `heat_state` row `do_store_memory` creates alongside the fact,
/// because an eligible-first read or a warm over rows production could not have written would measure
/// nothing. `project` additionally names one of the owner's own projects, which is what a
/// project-partition row looks like to the reader.
async fn bulk_seed(
    store: &Store,
    owner: &Owner,
    prefix: &str,
    count: usize,
    cosine: f32,
    project: Option<&RecordId>,
) {
    let [x, y] = {
        let v = unit_at(cosine);
        [v[0], v[1]]
    };
    let mut sql = String::new();
    for i in 0..count {
        sql.push_str(&format!(
            "CREATE type::record('fact', $k{i}) SET content = $c{i}, confidence = 0.5, embedding = \
             [{x}, {y}], tags = [], deleted = false, account = $account, workspace = $workspace, \
             scope = {}{}; CREATE heat_state SET memory = type::record('fact', $k{i}), heat = 0.25, \
             stability = 1.0, access_count = 0, last_touched = d'2026-01-01T00:00:00Z', \
             last_accessed_at = d'2026-01-01T00:00:00Z';",
            if project.is_some() { "'project'" } else { "'personal'" },
            if project.is_some() { ", project = $project" } else { "" },
        ));
    }
    let mut query = store
        .db()
        .query(sql)
        .bind(("account", owner.account.clone()))
        .bind(("workspace", owner.workspace.clone()));
    if let Some(project) = project {
        query = query.bind(("project", project.clone()));
    }
    for i in 0..count {
        query = query
            .bind((format!("k{i}"), format!("{prefix}-{i}")))
            .bind((format!("c{i}"), format!("{prefix} content {i}")));
    }
    query.await.unwrap().check().unwrap();
}

/// Live, owned, partition-resolved warm targets: what `activation_targets` hands `warm`.
async fn warm_targets(store: &Store, owner: &Owner, count: usize) -> Vec<WarmTarget> {
    let mut rows = store
        .db()
        .query("SELECT id FROM fact WHERE account = $account ORDER BY id LIMIT 200;")
        .bind(("account", owner.account.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let ids: Vec<serde_json::Value> = rows.take(0).unwrap();
    let mut out = Vec::new();
    for row in ids {
        if out.len() == count {
            break;
        }
        let Some(text) = row["id"].as_str() else {
            continue;
        };
        let Ok(id) = RecordId::parse_simple(text) else {
            continue;
        };
        if let Some(partition) = focal_partition_of(store.db(), &id).await.unwrap() {
            out.push(WarmTarget {
                target: id,
                partition,
                delta: 0.001,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// §Task 8 fix P2-6 — the two reads this lane added for listing/review, pinned by plan and
// by a control that proves the assertions discriminate
// ---------------------------------------------------------------------------------------

/// `EXPLAIN FORMAT JSON` of one shipped statement, bound exactly the way the read binds it.
///
/// The binding is not a detail: this engine's planner decides *at plan time* which index a statement
/// reaches, and an unbound `$scope_projects` makes it choose `TableScan` where the same statement with the
/// caller's real handles chooses `idx_fact_partition`. A plan test that forgot to bind would be measuring a
/// statement no request ever issues — and here it would have reported the exact false negative the read
/// does not have.
async fn planned(
    store: &Store,
    sql: &str,
    filter: &alexandria_storage::repos::ScopeFilter,
    cluster: Option<&RecordId>,
) -> String {
    let mut query = filter.apply(store.db().query(format!("EXPLAIN FORMAT JSON {sql}")));
    if let Some(cluster) = cluster {
        query = query.bind(("cluster", cluster.clone()));
    }
    let mut response = query.await.unwrap().check().unwrap();
    let plan: surrealdb::types::Value = response.take(0).unwrap();
    plan.to_sql()
}

/// The owner-review page reaches `idx_fact_partition` on **both** pages, and no `TableScan` appears.
///
/// `AGENTS.md` flags `WHERE … AND id > $cursor ORDER BY id LIMIT n` as a trap shape that plans a repeated
/// table scan when the *range* is expressed as a `FROM` clause over a bound key range. Here the cursor is
/// a plain filter and the workspace+scope equality in front of it selects the index, which is what this
/// measurement records rather than what the doc warns about in the other spelling. If this ever regresses
/// to a scan, the honest fix is the bound `RecordIdKeyRange` form, not a relaxed assertion.
#[tokio::test(flavor = "multi_thread")]
async fn review_page_plans_the_qualified_partition_index_on_both_pages() {
    let store = Store::build().await;
    let owner = &store.alice;
    let scope = store.personal_only(owner).await;
    let filter = alexandria_storage::repos::ScopeFilter::new(&scope);
    let conjunct = filter.conjunct();

    let first = planned(
        &store,
        &alexandria_storage::repos::review_page_sql(conjunct, ""),
        &filter,
        None,
    )
    .await;
    let resumed = planned(
        &store,
        &alexandria_storage::repos::review_page_sql(conjunct, " AND id > $cursor"),
        &filter,
        None,
    )
    .await;
    for (label, plan) in [("first page", first), ("resumed page", resumed)] {
        assert!(
            plan.contains("idx_fact_partition") && plan.contains("IndexScan"),
            "the {label} of the review page does not reach the qualified partition index: {plan}"
        );
        assert!(
            !plan.contains("TableScan"),
            "the {label} of the review page plans a table scan: {plan}"
        );
    }
}

/// The cluster member plan must keep the bound traversal and its fact-side population filter.
/// 4.0 renders this graph lookup in a `Values` expression, not a separate `GraphEdgeScan` node.
/// The engine's VM lookup reads adjacency by origin; EXPLAIN itself does not count visited keys.
#[tokio::test(flavor = "multi_thread")]
async fn cluster_member_plan_keeps_bound_traversal_and_partition_filter() {
    let store = Store::build().await;
    let owner = &store.alice;
    let scope = store.personal_only(owner).await;
    let filter = alexandria_storage::repos::ScopeFilter::new(&scope);
    let sql = alexandria_storage::repos::cluster_members_sql(filter.conjunct());

    // The statement itself must contain the population predicate: a conjunct that dropped out of the
    // traversal target's parentheses would still return the caller's rows and still pass a results test.
    assert!(
        sql.contains("$scope_projects") && sql.contains("$scope_personal"),
        "the member read no longer carries the partition binds: {sql}"
    );
    let cluster = alexandria_storage::repos::ClusterRepo::new(store.db())
        .create(None, &QUERY)
        .await
        .unwrap();
    let plan = planned(&store, &sql, &filter, Some(&cluster)).await;
    let traversal = format!("({})->contains_memory->(fact WHERE ", cluster.to_sql());
    assert!(
        plan.contains("operator: 'Values'") && plan.contains(&traversal),
        "the member read lost its bound adjacency traversal: {plan}"
    );
    assert!(
        plan.contains("scope = 'personal'") && plan.contains("project INSIDE []"),
        "the personal-only scope did not reach the fact-side filter: {plan}"
    );
    assert!(
        !plan.contains("TableScan"),
        "the member read plans a scan of the whole fact table: {plan}"
    );
    // Positive control on the predicate, not the plan: the same traversal *without* the conjunct is a
    // different statement, which is what proves the conjunct is part of what ships rather than decoration.
    let without = planned(
        &store,
        "SELECT * FROM $cluster->contains_memory->(fact WHERE deleted = false AND quarantined_at = \
         NONE AND workspace = $scope_workspace);",
        &filter,
        Some(&cluster),
    )
    .await;
    assert!(
        without.contains(&traversal)
            && !without.contains("scope = 'personal'")
            && !without.contains("project INSIDE"),
        "removing the partition conjunct should leave only the bound, workspace-filtered traversal: {without}"
    );
}
