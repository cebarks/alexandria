//! Task 8 — the storage-owned project resolver and the classification proposal write.
//!
//! Two boundaries are under test here, and they are different boundaries.
//!
//! **Resolution is workspace-qualified, end to end.** A project *alias* is not a global name: two
//! accounts working on the same repository legitimately own two projects that both normalize to
//! `github.com/anten/alexandria`. So every statement in `ScopedProjectRepo` carries the caller's
//! workspace on the same side of the `WHERE` as the alias, and each pair is a keyset read on
//! `idx_project_alias` / `idx_project_alias_lookup` rather than a store-wide filter — which is why the
//! count and listing assertions below read the stored rows rather than trusting a returned struct. A
//! foreign project and an absent one give the same refusal, so a selector cannot be probed for
//! existence in another workspace.
//!
//! **A proposal records a judgement and changes nothing.** `classification_proposal` is the row an
//! agent writes about a memory it cannot move: the fact's stored scope bytes must be identical after
//! the call, and the row must carry the revision it was formed against so Task 11's approval can tell
//! "still the same memory" from "the memory moved underneath the proposal".
//!
//! Owners come from the sealed producers only — `bootstrap_owner`, a minted agent token,
//! `AccessRepo::authorize`. There is no test-only authority constructor, because none may exist. Where a
//! case needs a row the request path does not create yet (an explicit `project_alias` association, which
//! belongs to a later owner-facing surface) it is written as a storage-owned fixture and says so.

use alexandria_storage::Database;
use alexandria_storage::access::{AccessRepo, OwnerContext, Principal, PrincipalProvenance};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::{
    AccountId, FactId, MemoryScope, OperationId, ProjectId, ReadSelection, Version, WorkspaceId,
};
use alexandria_storage::repos::{
    AuthRepo, ClassificationRepo, IdentityRepo, ProjectEntry, ScopedProjectRepo, ScopedSessionRepo,
};
use chrono::Utc;
use surrealdb::types::RecordId;

/// One owner: the handles enrollment produced, a live hosted `OwnerContext`, and the credential that
/// issued it (so a case can revoke exactly the authority the context carries).
struct Owner {
    account: AccountId,
    workspace: WorkspaceId,
    owner: OwnerContext,
    credential: alexandria_storage::models::CredentialId,
}

struct Fixture {
    db: Database,
    alice: Owner,
    bob: Owner,
}

/// Two owners, each with a live hosted context reached only through rows that really exist.
async fn fixture(login: &str) -> Fixture {
    let mut db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner())
        .await
        .unwrap();
    let system = db.claim_system_authority().unwrap();
    let db = db;

    let identity = IdentityRepo::new(db.inner());
    let access = AccessRepo::new(&db);
    let mut owners: Vec<Owner> = Vec::with_capacity(2);
    for who in [format!("{login}-alice"), format!("{login}-bob")] {
        let prepared = format!("$argon2id$fixture-prepared-hash-{who}-padding-padding");
        let ids = identity
            .bootstrap_owner(&who, &prepared, &OperationId::new(format!("task8-{who}")))
            .await
            .unwrap();
        // Trusted-local is reachable only under this instance's capability, and only to mint the hosted
        // credential the account will actually authenticate with.
        let local = access.local_principal(&system, &ids.account).await.unwrap();
        let context = access.authorize(&local).await.unwrap();
        let token = AuthRepo::new(db.inner())
            .mint_agent_token(&context, "task8-agent", Utc::now())
            .await
            .unwrap();
        let plaintext = token.secret.render().expose().to_string();
        let principal: Principal = AuthRepo::new(db.inner())
            .authenticate_agent_token(&plaintext, Utc::now())
            .await
            .unwrap();
        let credential = match principal.provenance() {
            PrincipalProvenance::Credential(id) => id.clone(),
            other => panic!("a minted agent token must carry a credential, got {other:?}"),
        };
        let owner = access.authorize(&principal).await.unwrap();
        owners.push(Owner {
            account: ids.account,
            workspace: ids.workspace,
            owner,
            credential,
        });
    }
    let mut owners = owners;
    let bob = owners.pop().expect("two owners were built");
    let alice = owners.pop().expect("two owners were built");
    Fixture { db, alice, bob }
}

fn resolver<'a>(fx: &'a Fixture, owner: &'a Owner) -> ScopedProjectRepo<'a> {
    ScopedProjectRepo::new(fx.db.inner(), &owner.owner)
}

fn classifier<'a>(fx: &'a Fixture, owner: &'a Owner) -> ClassificationRepo<'a> {
    ClassificationRepo::new(fx.db.inner(), &owner.owner)
}

/// A live, owned Personal-partition fact at semantic revision 4.
async fn fact(fx: &Fixture, owner: &Owner, key: &str, content: &str) -> RecordId {
    let mut response = fx
        .db
        .inner()
        .query(
            "CREATE type::record('fact', $key) SET content = $content, confidence = 0.5, embedding \
             = [1.0, 0.0], tags = [], deleted = false, account = $account, workspace = $workspace, \
             scope = 'personal', semantic_revision = $revision RETURN AFTER;",
        )
        .bind(("key", key.to_string()))
        .bind(("content", content.to_string()))
        .bind(("account", owner.account.as_record_id().clone()))
        .bind(("workspace", owner.workspace.as_record_id().clone()))
        .bind(("revision", 4i64))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    RecordId::parse_simple(rows[0]["id"].as_str().unwrap()).unwrap()
}

/// The stored bytes of one fact — what a proposal must not disturb.
async fn stored_fact(db: &Database, id: &RecordId) -> serde_json::Value {
    let mut response = db
        .inner()
        .query(
            "SELECT scope, project, account, workspace, content, deleted, semantic_revision FROM \
             type::record('fact', $key) WHERE id = $id;",
        )
        .bind(("key", record_key_text(id)))
        .bind(("id", id.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    rows.into_iter().next().expect("the fixture fact exists")
}

async fn count(db: &Database, table: &str, alias: &str) -> i64 {
    let mut response = db
        .inner()
        .query(format!("SELECT count() AS c FROM {table} GROUP ALL;"))
        .await
        .unwrap()
        .check()
        .unwrap();
    let _ = alias;
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    rows.first().and_then(|r| r["c"].as_i64()).unwrap_or(0)
}

/// The key part of a record id, for `type::record('table', $key)`.
fn record_key_text(id: &RecordId) -> String {
    alexandria_storage::record_id_to_string(id)
        .split_once(':')
        .map(|(_, key)| key.to_string())
        .unwrap_or_default()
}

fn project_id(key: &str) -> ProjectId {
    ProjectId::try_from(RecordId::new("project", key)).expect("a project handle")
}

/// Two owners, one canonical alias: each resolves to *their own* project, and neither can name the
/// other's by handle.
///
/// This is the whole privacy model for projects, so it is asserted three ways: the alias resolves
/// inside each workspace, a foreign handle refuses identically to one that never existed, and the
/// store is shown to hold two rows so the refusals are about the workspace and not about emptiness.
#[tokio::test]
async fn project_lookup_is_workspace_qualified_end_to_end() {
    let fx = fixture("qualify").await;
    let alias = "github.com/anten/alexandria";

    let alice = resolver(&fx, &fx.alice)
        .establish(alias, Some("Alexandria"))
        .await
        .expect("alice may establish her own project");
    let bob = resolver(&fx, &fx.bob)
        .establish(alias, None)
        .await
        .expect("the same alias establishes separately for bob");
    assert!(alice.created && bob.created, "both were firsts");
    assert_ne!(
        alice.project.id, bob.project.id,
        "one alias collapsed two workspaces' projects into one row"
    );

    // Each resolves the alias to their own row, and a repeat call is a replay, not a second project.
    for (owner, expected) in [(&fx.alice, &alice.project), (&fx.bob, &bob.project)] {
        let again = resolver(&fx, owner)
            .establish(alias, None)
            .await
            .expect("establish is resolve-or-create");
        assert!(!again.created, "a replay created a second project row");
        assert_eq!(again.project.id, expected.id);
        let resolved = resolver(&fx, owner)
            .resolve_alias(alias)
            .await
            .expect("a resolution read does not fail")
            .expect("the alias resolves for its own owner");
        assert_eq!(resolved.id, expected.id);
        assert_eq!(resolved.normalized_alias, alias);
    }
    assert_eq!(
        resolver(&fx, &fx.alice)
            .resolve_alias(alias)
            .await
            .unwrap()
            .unwrap()
            .display_name
            .as_deref(),
        Some("Alexandria"),
        "the display name established with the row must read back"
    );

    // A foreign handle is refused exactly as an absent one.
    let foreign = resolver(&fx, &fx.alice)
        .resolve_id(&bob.project.id)
        .await
        .unwrap_err();
    let absent = resolver(&fx, &fx.alice)
        .resolve_id(&project_id("never-made"))
        .await
        .unwrap_err();
    assert_eq!(
        foreign.to_string(),
        absent.to_string(),
        "a foreign project id and an absent one answered differently: {foreign:?} vs {absent:?}"
    );
    assert!(matches!(foreign, StorageError::NotFound));
    assert_eq!(
        count(&fx.db, "project", alias).await,
        2,
        "the store does not hold one project per workspace, so the refusals prove nothing"
    );
}

/// The explicit alias table is a second door to the same row.
///
/// A `project_alias` row is an *explicit* association — a repository's second remote, or a renamed one
/// that must still resolve — and no Task 8 request path creates one, so this fixture writes it the way
/// the schema intends. The assertion that matters is that `establish` never mints a second project for
/// an alias that already names one through the explicit table, and that another workspace cannot reach
/// it.
#[tokio::test]
async fn an_explicit_alias_resolves_to_the_project_it_names_and_establish_does_not_duplicate_it() {
    let fx = fixture("alias-table").await;
    let primary = resolver(&fx, &fx.alice)
        .establish("github.com/anten/alexandria", None)
        .await
        .unwrap();
    let extra = "gitlab.com/anten/alexandria";

    fx.db
        .inner()
        .query(
            "CREATE project_alias SET workspace = $workspace, project = $project, alias = $alias;",
        )
        .bind(("workspace", fx.alice.workspace.as_record_id().clone()))
        .bind(("project", primary.project.id.as_record_id().clone()))
        .bind(("alias", extra.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();

    let through_alias = resolver(&fx, &fx.alice)
        .resolve_alias(extra)
        .await
        .unwrap()
        .expect("the explicit alias resolves");
    assert_eq!(
        through_alias.id, primary.project.id,
        "the alias table did not reach the project it names"
    );

    let established = resolver(&fx, &fx.alice)
        .establish(extra, None)
        .await
        .unwrap();
    assert!(
        !established.created,
        "establish minted a second project for an alias that already names one"
    );
    assert_eq!(established.project.id, primary.project.id);

    assert_eq!(
        resolver(&fx, &fx.bob)
            .resolve_alias(extra)
            .await
            .unwrap()
            .as_ref()
            .map(|project| project.id.clone()),
        None,
        "a foreign workspace's explicit alias resolved here"
    );
}

/// Concurrent establishes of one alias in one workspace produce one project and one handle for all.
#[tokio::test]
async fn establish_is_idempotent_under_a_concurrent_same_alias_race() {
    use std::sync::Arc;

    let fx = fixture("race").await;
    let alias = "github.com/anten/raced";
    let db = Arc::new(fx.db);
    let owner = fx.alice.owner.clone();

    let mut tasks = Vec::new();
    for _ in 0..4 {
        let db = Arc::clone(&db);
        let owner = owner.clone();
        let alias = alias.to_string();
        tasks.push(tokio::spawn(async move {
            ScopedProjectRepo::new(db.inner(), &owner)
                .establish(&alias, None)
                .await
                .map(|established| (established.created, established.project.id))
        }));
    }
    let mut ids = Vec::new();
    let mut created_any = false;
    for task in tasks {
        let (created, id) = task
            .await
            .unwrap()
            .expect("a raced establish must succeed or refuse loudly");
        created_any |= created;
        ids.push(id);
    }
    assert!(created_any, "no racer believed it created the project");
    assert!(
        ids.iter().all(|id| id == &ids[0]),
        "the racers were handed different projects for one alias: {ids:?}"
    );
    assert_eq!(
        count(&db, "project", alias).await,
        1,
        "the raced establish left more than one project row"
    );
}

/// The listing is the caller's own, paged once through every row, with counts that do not cross.
///
/// Paging is asserted by *coverage*: pages of two over five of the caller's projects must visit each
/// exactly once, in index order, and terminate — which a global page filtered afterwards fails the
/// moment another account holds more rows. The counts are the leak that returns no body at all, so
/// each project's live count is checked against what the store holds, including the rows that must not
/// be counted.
#[tokio::test]
async fn listing_projects_pages_the_callers_own_rows_and_counts_only_theirs() {
    let fx = fixture("listing").await;
    let alias = |i: usize| format!("github.com/anten/p{i}");
    for i in 0..5 {
        resolver(&fx, &fx.alice)
            .establish(&alias(i), None)
            .await
            .unwrap();
    }

    // Bob owns three projects with the same aliases plus four rows in one of them: one live, one
    // deleted, one quarantined, one in another of his projects.
    let mut bob_projects = Vec::new();
    for i in 0..3 {
        bob_projects.push(
            resolver(&fx, &fx.bob)
                .establish(&alias(i), None)
                .await
                .unwrap()
                .project,
        );
    }
    for (key, scope, project, set) in [
        ("live-row", "project", Some(&bob_projects[0]), ""),
        (
            "dead-row",
            "project",
            Some(&bob_projects[0]),
            ", deleted = true",
        ),
        (
            "held-row",
            "project",
            Some(&bob_projects[0]),
            ", quarantined_at = time::now()",
        ),
        ("elsewhere", "project", Some(&bob_projects[1]), ""),
        ("mine", "personal", None, ""),
    ] {
        let mut sql = format!(
            "CREATE type::record('fact', '{key}') SET content = 'x', confidence = 0.5, embedding = \
             [1.0, 0.0], tags = [], deleted = false{set}, account = $account, workspace = \
             $workspace, scope = '{scope}'"
        );
        if project.is_some() {
            sql.push_str(", project = $project");
        }
        let mut query = fx
            .db
            .inner()
            .query(format!("{sql};"))
            .bind(("account", fx.bob.account.as_record_id().clone()))
            .bind(("workspace", fx.bob.workspace.as_record_id().clone()));
        if let Some(project) = project {
            query = query.bind(("project", project.id.as_record_id().clone()));
        }
        query.await.unwrap().check().unwrap();
    }

    // Alice's own listing, paged by two, must visit each of her five exactly once.
    let mut seen: Vec<ProjectEntry> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = resolver(&fx, &fx.alice)
            .list(cursor.as_deref(), 2)
            .await
            .expect("a page read runs");
        pages += 1;
        assert!(
            page.entries.len() <= 2,
            "a page carried more than it was asked for: {:?}",
            page.entries.len()
        );
        for entry in &page.entries {
            assert_eq!(
                entry.live_memory_count, 0,
                "alice's project counted another account's row: {entry:?}"
            );
        }
        seen.extend(page.entries.iter().cloned());
        match page.next_cursor {
            None => break,
            Some(next) => cursor = Some(next),
        }
        assert!(pages < 10, "the pages never terminated: {seen:?}");
    }
    let aliases: Vec<String> = seen
        .iter()
        .map(|entry| entry.project.normalized_alias.clone())
        .collect();
    assert_eq!(
        aliases.len(),
        5,
        "the pages did not visit every project exactly once: {aliases:?}"
    );
    let mut sorted = aliases.clone();
    sorted.sort();
    assert_eq!(aliases, sorted, "the pages were not in the index order");
    assert_eq!(
        aliases
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        5,
        "one project appeared on two pages: {aliases:?}"
    );

    // Bob's counts are his own and partition-local: exactly the one live row in `p0`.
    let bob_page = resolver(&fx, &fx.bob)
        .list(None, 100)
        .await
        .expect("bob's page reads");
    let counts: Vec<(String, i64)> = bob_page
        .entries
        .iter()
        .map(|entry| {
            (
                entry.project.normalized_alias.clone(),
                entry.live_memory_count,
            )
        })
        .collect();
    assert_eq!(
        counts,
        vec![(alias(0), 1), (alias(1), 1), (alias(2), 0)],
        "the per-project live count is not partition- or liveness-local: {counts:?}"
    );
    assert!(
        bob_page.next_cursor.is_none(),
        "a short page claimed a continuation: {:?}",
        bob_page.next_cursor
    );
}

/// A proposal persists as pending, names the revision it was formed against, and leaves the fact alone.
///
/// The `moves nothing` half is the reason this lives at the storage boundary rather than only in a tool:
/// an agent's judgement must be durable without being an edit, so the fact's stored bytes are compared
/// before and after the call.
#[tokio::test]
async fn a_proposal_records_its_revision_and_moves_nothing() {
    let fx = fixture("propose").await;
    let project = resolver(&fx, &fx.alice)
        .establish("github.com/anten/proposed", None)
        .await
        .unwrap()
        .project;
    let id = fact(&fx, &fx.alice, "propose-fact", "durable fact").await;
    let before = stored_fact(&fx.db, &id).await;
    assert_eq!(before["scope"].as_str(), Some("personal"));

    let repo = classifier(&fx, &fx.alice);
    let fact_id = FactId::try_from(id.clone()).unwrap();
    let reference = repo
        .propose(
            &fact_id,
            Version::new(4),
            MemoryScope::Project(project.id.clone()),
            Some("repo-specific"),
        )
        .await
        .expect("a lawful proposal is accepted");
    assert_eq!(reference.revision, Version::ZERO);
    assert_eq!(
        stored_fact(&fx.db, &id).await,
        before,
        "the proposal changed the fact it was written about"
    );

    let mut response = fx
        .db
        .inner()
        .query(
            "SELECT id, fact, workspace, account, destination_kind, destination_project, \
             source_revision, revision, state, reason FROM classification_proposal;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["state"].as_str(), Some("pending"));
    assert_eq!(rows[0]["revision"].as_i64(), Some(0));
    assert_eq!(rows[0]["source_revision"].as_i64(), Some(4));
    assert_eq!(rows[0]["destination_kind"].as_str(), Some("project"));
    assert_eq!(rows[0]["reason"].as_str(), Some("repo-specific"));
    assert_eq!(
        RecordId::parse_simple(rows[0]["fact"].as_str().unwrap_or_default())
            .ok()
            .as_ref(),
        Some(&id),
        "the proposal names a different fact than it was given"
    );
    for (column, expected) in [
        (
            "account",
            alexandria_storage::record_id_to_string(fx.alice.account.as_record_id()),
        ),
        (
            "workspace",
            alexandria_storage::record_id_to_string(fx.alice.workspace.as_record_id()),
        ),
    ] {
        assert_eq!(
            rows[0][column].as_str(),
            Some(expected.as_str()),
            "the proposal's {column} did not come from the bound context"
        );
    }
    assert_eq!(
        RecordId::parse_simple(rows[0]["destination_project"].as_str().unwrap_or_default())
            .ok()
            .as_ref(),
        Some(project.id.as_record_id()),
        "the destination project was not stored as given"
    );
    // The reference the caller gets back names the row that was written: same handle, no second row.
    assert_eq!(
        reference.id,
        alexandria_storage::record_id_to_string(
            &RecordId::parse_simple(rows[0]["id"].as_str().unwrap_or_default())
                .expect("the stored proposal carries a parseable id")
        ),
        "the returned reference is not the stored row's handle"
    );

    // The same judgement twice is the same proposal, not a second row.
    let again = repo
        .propose(
            &fact_id,
            Version::new(4),
            MemoryScope::Project(project.id.clone()),
            Some("repo-specific"),
        )
        .await
        .expect("a replay is idempotent");
    assert_eq!(again.id, reference.id, "a replay created a second proposal");
    assert_eq!(
        count(&fx.db, "classification_proposal", "").await,
        1,
        "a replay duplicated the row"
    );
}

/// Every way a proposal can be unlawful refuses, and none of them writes.
///
/// A stale revision, an absent fact, a foreign destination project and a foreign fact are four
/// different conclusions; only the first is a `Stale`, and the two ownership-shaped ones answer
/// identically so a refusal cannot be probed for what exists elsewhere.
#[tokio::test]
async fn a_proposal_refuses_a_stale_revision_a_foreign_destination_and_a_foreign_fact() {
    let fx = fixture("propose-refuse").await;
    let mine = resolver(&fx, &fx.alice)
        .establish("github.com/anten/mine", None)
        .await
        .unwrap()
        .project;
    let theirs = resolver(&fx, &fx.bob)
        .establish("github.com/anten/theirs", None)
        .await
        .unwrap()
        .project;
    let id = fact(&fx, &fx.alice, "propose-refuse-fact", "durable fact").await;
    let repo = classifier(&fx, &fx.alice);
    let fact_id = FactId::try_from(id.clone()).unwrap();

    let stale = repo
        .propose(&fact_id, Version::new(9), MemoryScope::Personal, None)
        .await;
    assert!(
        matches!(stale, Err(StorageError::Stale)),
        "a stale expected revision was accepted: {stale:?}"
    );
    assert_eq!(
        stored_fact(&fx.db, &id).await["semantic_revision"].as_i64(),
        Some(4),
        "a refused proposal still moved the fact it was about"
    );

    let absent = repo
        .propose(
            &FactId::try_from(RecordId::new("fact", "never-stored")).unwrap(),
            Version::new(0),
            MemoryScope::Personal,
            None,
        )
        .await;
    assert!(
        matches!(absent, Err(StorageError::NotFound)),
        "an absent fact answered something else: {absent:?}"
    );

    let foreign_project = repo
        .propose(
            &fact_id,
            Version::new(4),
            MemoryScope::Project(theirs.id.clone()),
            None,
        )
        .await;
    let absent_project = repo
        .propose(
            &fact_id,
            Version::new(4),
            MemoryScope::Project(project_id("never-minted")),
            None,
        )
        .await;
    assert!(
        matches!(foreign_project, Err(StorageError::NotFound)),
        "a foreign destination project was accepted: {foreign_project:?}"
    );
    assert_eq!(
        foreign_project.unwrap_err().to_string(),
        absent_project.unwrap_err().to_string(),
        "a foreign project and an absent one answered differently"
    );

    let bobs_view = classifier(&fx, &fx.bob);
    let refused = bobs_view
        .propose(&fact_id, Version::new(4), MemoryScope::Personal, None)
        .await;
    assert!(
        matches!(refused, Err(StorageError::NotFound)),
        "a foreign fact was proposal-able: {refused:?}"
    );

    assert_eq!(
        count(&fx.db, "classification_proposal", "").await,
        0,
        "a refused proposal still wrote a row"
    );

    // Positive control on the same repo and the same fact: the lawful write does land.
    repo.propose(&fact_id, Version::new(4), MemoryScope::Personal, None)
        .await
        .expect("the lawful proposal is accepted");
    let mut response = fx
        .db
        .inner()
        .query(
            "SELECT count() AS c FROM classification_proposal WHERE destination_kind = 'personal' \
             AND destination_project = NONE GROUP ALL;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    assert_eq!(rows[0]["c"].as_i64(), Some(1), "{rows:?}");
    let _ = &mine;
}

/// A proposal cannot be written over dead authority, and a live one can.
///
/// The row is durable corpus state about a memory, so it runs through the same claim predicate every
/// other scoped write runs through — otherwise a revoked bearer could leave judgements behind that a
/// human would then be asked to approve.
#[tokio::test]
async fn a_revoked_credential_stops_a_proposal_write() {
    let fx = fixture("propose-guard").await;
    let id = fact(&fx, &fx.alice, "propose-guard-fact", "durable fact").await;
    let fact_id = FactId::try_from(id).unwrap();
    let repo = classifier(&fx, &fx.alice);

    repo.propose(&fact_id, Version::new(4), MemoryScope::Personal, None)
        .await
        .expect("live authority may propose");

    AuthRepo::new(fx.db.inner())
        .revoke_credential(&fx.alice.owner, &fx.alice.credential, Utc::now())
        .await
        .unwrap();

    let refused = repo
        .propose(
            &fact_id,
            Version::new(4),
            MemoryScope::Personal,
            Some("after revocation"),
        )
        .await;
    assert!(
        matches!(refused, Err(StorageError::Stale)),
        "a revoked credential was not refused: {refused:?}"
    );
    let mut response = fx
        .db
        .inner()
        .query("SELECT count() AS c FROM classification_proposal GROUP ALL;")
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    assert_eq!(
        rows[0]["c"].as_i64(),
        Some(1),
        "the revoked write added a row: {rows:?}"
    );
    // …and the row that exists is the one the live credential wrote.
    assert_eq!(
        rows.len(),
        1,
        "the duplicate-guard let a second pending row through"
    );
    let mut probe = fx
        .db
        .inner()
        .query("SELECT reason FROM classification_proposal;")
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = probe.take(0).unwrap();
    assert_eq!(
        rows[0]["reason"].as_str(),
        None,
        "the refused write replaced the live row's reason"
    );
}

/// A session's scoped membership is the caller's partition; the assigned view is the whole corpus.
///
/// The same handle, the same memories, and the only difference is which scopes the request named — which
/// is the claim the `view` parameter makes to a client, so it is pinned at the repository rather than
/// inferred from a response body.
#[tokio::test]
async fn a_scoped_session_view_filters_membership_and_the_assigned_view_does_not() {
    let fx = fixture("session-views").await;
    let project = resolver(&fx, &fx.alice)
        .establish("github.com/anten/views", None)
        .await
        .unwrap()
        .project;
    let access = AccessRepo::new(&fx.db);

    let project_fact = {
        let mut response = fx
            .db
            .inner()
            .query(
                "CREATE type::record('fact', 'in-project') SET content = 'project row', confidence \
                 = 0.5, embedding = [1.0, 0.0], tags = [], deleted = false, account = $account, \
                 workspace = $workspace, scope = 'project', project = $project RETURN AFTER;",
            )
            .bind(("account", fx.alice.account.as_record_id().clone()))
            .bind(("workspace", fx.alice.workspace.as_record_id().clone()))
            .bind(("project", project.id.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<serde_json::Value> = response.take(0).unwrap();
        RecordId::parse_simple(rows[0]["id"].as_str().unwrap()).unwrap()
    };
    let personal_fact = fact(&fx, &fx.alice, "in-personal", "personal row").await;

    let sessions = ScopedSessionRepo::new(fx.db.inner(), fx.alice.owner.clone());
    sessions
        .find_or_create("views:handle", Some("agent"), None)
        .await
        .unwrap();
    for id in [&personal_fact, &project_fact] {
        sessions
            .add_memory("views:handle", &alexandria_storage::record_id_to_string(id))
            .await
            .unwrap();
    }

    let names = |rows: Vec<alexandria_storage::models::Fact>| -> Vec<String> {
        rows.iter()
            .map(|row| {
                alexandria_storage::record_id_to_string(
                    row.id.as_ref().expect("a stored fact carries an id"),
                )
            })
            .collect()
    };

    let assigned = sessions
        .memories_in(
            "views:handle",
            &access
                .select(&fx.alice.owner, ReadSelection::Assigned)
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        names(assigned).len(),
        2,
        "the assigned view lost one of the caller's own rows"
    );

    let personal = sessions
        .memories_in(
            "views:handle",
            &access
                .select(&fx.alice.owner, ReadSelection::One(MemoryScope::Personal))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        names(personal),
        vec![alexandria_storage::record_id_to_string(&personal_fact)],
        "the personal view did not filter to the personal partition"
    );

    let one_project = sessions
        .memories_in(
            "views:handle",
            &access
                .select(
                    &fx.alice.owner,
                    ReadSelection::One(MemoryScope::Project(project.id.clone())),
                )
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        names(one_project),
        vec![alexandria_storage::record_id_to_string(&project_fact)],
        "the project view did not filter to the named project"
    );
}
