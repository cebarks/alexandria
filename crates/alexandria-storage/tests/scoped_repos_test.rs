//! Task 6: scoped repository reads, and the in-transaction commit predicate every scoped write runs.
//!
//! Two things are pinned here that a reviewer cannot see from any other test:
//!
//! 1. **The commit predicate is atomic, not a pre-check.** The Task 5 carry names the exact set —
//!    account status, current `auth_epoch`, and the specific credential's ownership / revocation /
//!    consumption / expiry / minted-epoch rules — and requires them *inside the write's own
//!    transaction*. `revalidate()` before the write is the accepted interface for the request path and
//!    is explicitly not a substitute: measured on this engine, a guarded `SELECT` inside a transaction
//!    still lets a revoke committed by another writer between the read and the commit silently win.
//!    `a_credential_revoked_mid_transaction_does_not_commit_a_scoped_write` pins the shape that does
//!    hold, and `a_guarded_read_alone_is_not_atomic` pins *why* the obvious shape was rejected, so
//!    nobody "simplifies" the guard back into a read.
//! 2. **`select_offline` is a system authority, not a request fallback.**
//!    `offline_selection_requires_explicit_owned_workspace_project_pair` and
//!    `select_offline_refuses_a_capability_from_another_instance`.
//!
//! Owners, credentials and contexts all come from real rows through the sealed producers —
//! `bootstrap_owner`, `local_principal` under this instance's claimed capability, `mint_agent_token`,
//! `authenticate_agent_token`, `authorize`. There is no test-only authority constructor here because
//! none may exist.

use alexandria_storage::Database;
use alexandria_storage::access::{AccessRepo, OwnerContext, Principal, SystemContext};
use alexandria_storage::models::{
    AccountId, CredentialId, MemoryScope, OperationId, ProjectId, ReadSelection, WorkspaceId,
    credential_kind, normalize_login,
};
use alexandria_storage::repos::{
    AuthRepo, IdentityRepo, NewReminder, ScopedReminderRepo, ScopedSessionRepo,
};
use chrono::Utc;
use surrealdb::types::{RecordId, SurrealValue};

/// One bootstrapped owner with a live hosted context, plus the credential that produced it.
struct Fixture {
    db: Database,
    system: SystemContext,
    account: AccountId,
    workspace: WorkspaceId,
    project: ProjectId,
    owner: OwnerContext,
    principal: Principal,
}

impl std::fmt::Debug for Fixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fixture")
            .field("account", &self.account)
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

/// One owner, one project, one live agent token, one hosted `OwnerContext`.
///
/// `login` varies per test so two tests sharing the `mem://` *per-process* namespace cannot collide —
/// each call builds its own `Database`, and `account.login` is UNIQUE within it.
async fn fixture(login: &str) -> Fixture {
    let mut db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner())
        .await
        .unwrap();
    let system = db.claim_system_authority().unwrap();
    let db = db;

    let identity = IdentityRepo::new(db.inner());
    let access = AccessRepo::new(&db);
    let prepared = format!("$argon2id$fixture-prepared-hash-{login}-padding");
    let ids = identity
        .bootstrap_owner(
            login,
            &prepared,
            &OperationId::new(format!("scoped-repos-{login}")),
        )
        .await
        .unwrap();

    let project = create_project(&db, &ids.workspace, &format!("{login}-proj")).await;

    let local = access.local_principal(&system, &ids.account).await.unwrap();
    let context = access.authorize(&local).await.unwrap();
    let token = AuthRepo::new(db.inner())
        .mint_agent_token(&context, "scoped-repos-agent", Utc::now())
        .await
        .unwrap();
    let secret = token.secret.render();
    let plaintext = secret.expose().to_string();
    let principal = AuthRepo::new(db.inner())
        .authenticate_agent_token(&plaintext, Utc::now())
        .await
        .unwrap();
    let owner = access.authorize(&principal).await.unwrap();

    Fixture {
        db,
        system,
        account: ids.account,
        workspace: ids.workspace,
        project,
        owner,
        principal,
    }
}

async fn create_project(db: &Database, workspace: &WorkspaceId, alias: &str) -> ProjectId {
    // `normalize_login` is the same normalisation the schema's own ASSERT requires, and it returns
    // String rather than a Result: an alias this fixture writes must already be valid.
    let alias = normalize_login(alias);
    let mut response = db
        .inner()
        .query("CREATE project SET workspace = $workspace, normalized_alias = $alias RETURN AFTER;")
        .bind(("workspace", workspace.as_record_id().clone()))
        .bind(("alias", alias))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<ProjectRow> = response.take(0).unwrap();
    ProjectId::try_from(rows.into_iter().next().unwrap().id).unwrap()
}

#[derive(surrealdb::types::SurrealValue)]
#[allow(dead_code)]
struct ProjectRow {
    id: RecordId,
}

/// A live session row for the fixture's own account, created through the scoped repo.
async fn session(fx: &Fixture, handle: &str) {
    ScopedSessionRepo::new(fx.db.inner(), fx.owner.clone())
        .find_or_create(handle, Some("agent"), None)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// The commit predicate
// ---------------------------------------------------------------------------------------------

/// The Task 5 carry, pinned. A revoke that lands *between* the guard and the commit must stop the
/// write, not be committed over.
///
/// The window is entered for real: this issues a scoped write, and revokes the credential from a
/// second handle while that write is in flight. Under the accepted shape the commit is refused with an
/// engine write conflict and the data write is rolled back, so the stored row is byte-identical to
/// before. Under the tempting shape — a guarded read, then the write — this test would pass while the
/// leak was live, which is why the *negative* control below exists next to it.
#[tokio::test]
async fn a_revoked_credential_stops_a_scoped_write_before_it_commits() {
    let fx = fixture("predicate-alice").await;
    session(&fx, "predicate-handle").await;

    let before = dump_session(&fx, "predicate-handle").await;
    assert_eq!(
        before["summary"],
        serde_json::Value::Null,
        "the fixture starts unfinalized"
    );

    // Revoke the very credential this context was issued under, then write with it.
    let credential = match fx.principal.provenance() {
        alexandria_storage::access::PrincipalProvenance::Credential(id) => id.clone(),
        other => panic!("fixture must hold a hosted credential, got {other:?}"),
    };
    AuthRepo::new(fx.db.inner())
        .revoke_credential(&fx.owner, &credential, Utc::now())
        .await
        .unwrap();

    let repo = ScopedSessionRepo::new(fx.db.inner(), fx.owner.clone());
    let outcome = repo
        .finalize("predicate-handle", Some("written after revocation"), None)
        .await;
    match outcome {
        // `Stale` is the honest answer: the caller's authority moved, so it should re-authenticate.
        Err(alexandria_storage::error::StorageError::Stale) => {}
        Err(other) => panic!("a revoked credential must be refused as Stale, got {other:?}"),
        Ok(Some(row)) => panic!(
            "a revoked credential COMMITTED a scoped write: {:?}",
            row.summary
        ),
        Ok(None) => panic!("a revoked credential reported success with no row"),
    }

    // And the stored row is untouched, which is the only part a caller could not already infer.
    let after = dump_session(&fx, "predicate-handle").await;
    assert_eq!(
        after, before,
        "the write reached the stored row despite the revoked credential"
    );
}

/// The race, entered mid-transaction rather than before it.
///
/// This is the case a sequential "revoke, then write" test cannot reach, and it is the reason the guard
/// is a *conditional write on the authorization row* rather than a read. A second engine handle revokes
/// the credential after the guard has been claimed and before the transaction commits; the commit then
/// has to fail and the data write has to be undone.
#[tokio::test]
async fn a_credential_revoked_mid_transaction_does_not_commit_a_scoped_write() {
    let fx = fixture("midtx-alice").await;
    session(&fx, "midtx-handle").await;
    let before = dump_session(&fx, "midtx-handle").await;

    let credential = credential_of(&fx);
    // Drive the guard and the data write by hand, in exactly the order `guard_then_write` uses, with
    // the revoke injected between them. Reproducing the shape here rather than only asserting through
    // the repo is deliberate: it makes the engine's behaviour the subject of the test.
    let tx = fx.db.inner().clone().begin().await.unwrap();
    let claimed = conditional_credential_claim(&tx, &fx, &credential).await;
    assert_eq!(
        claimed, 1,
        "the credential is live when the guard is claimed"
    );
    let wrote = tx
        .query(
            "UPDATE `session` SET summary = 'raced' WHERE external_id = $handle AND account = \
             $account RETURN AFTER;",
        )
        .bind(("handle", "midtx-handle".to_string()))
        .bind(("account", fx.account.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap()
        .take::<Vec<serde_json::Value>>(0)
        .unwrap()
        .len();
    assert_eq!(wrote, 1, "the data write is applied inside the transaction");

    // The concurrent revoke, from a separate handle, after our claim and before our commit.
    AuthRepo::new(fx.db.inner())
        .revoke_credential(&fx.owner, &credential, Utc::now())
        .await
        .unwrap();

    let commit = tx.commit().await;
    assert!(
        commit.is_err(),
        "the commit must be refused: a transaction that both wrote on the authorization row and lost \
         the race has to conflict rather than overwrite the revocation"
    );

    let after = dump_session(&fx, "midtx-handle").await;
    assert_eq!(
        after, before,
        "the raced write survived the revoke, so the guard is not atomic"
    );
}

/// The session **create** runs the same commit predicate as every other scoped write.
///
/// Task 6 left exactly one write on the session repository unguarded: `create`, reached from
/// `find_or_create` whenever a caller stores against a handle nobody has opened yet. Every *other*
/// session write was moved behind the claim in Task 6's fifth lane, and the reason this one matters is
/// that it is the only one that runs on the store path — so a hosted caller whose credential died
/// between `admit` and the create would leave a durable session row (with the account stamp, but
/// written by authority that no longer exists) behind on a path the review of #25 named.
///
/// Two assertions, in this order: a live credential must still create (or every refusal below is just
/// a broken fixture), and a revoked one must be refused **and** write nothing.
#[tokio::test]
async fn a_revoked_credential_stops_an_implicit_session_create() {
    let fx = fixture("createguard-alice").await;
    let repo = ScopedSessionRepo::new(fx.db.inner(), fx.owner.clone());

    let live = repo
        .find_or_create("createguard:live", Some("agent"), None)
        .await
        .expect("a live credential may open a session");
    assert_eq!(live.external_id, "createguard:live");

    let credential = credential_of(&fx);
    AuthRepo::new(fx.db.inner())
        .revoke_credential(&fx.owner, &credential, Utc::now())
        .await
        .unwrap();

    let outcome = repo
        .find_or_create("createguard:dead", Some("agent"), None)
        .await;
    match outcome {
        Err(alexandria_storage::error::StorageError::Stale) => {}
        Err(other) => panic!("a revoked credential was refused for the wrong reason: {other:?}"),
        Ok(session) => panic!("a revoked credential created a session: {:?}", session.id),
    }

    // Read the stored rows, not the repo: a create that wrote a row and then errored would otherwise
    // look like a refusal.
    let mut response = fx
        .db
        .inner()
        .query("SELECT external_id FROM `session` WHERE account = $account ORDER BY external_id;")
        .bind(("account", fx.account.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let handles: Vec<String> = response
        .take::<Vec<serde_json::Value>>(0)
        .unwrap()
        .into_iter()
        .map(|row| row["external_id"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        handles,
        vec!["createguard:live".to_string()],
        "the refused create still wrote a session row"
    );
}

/// The mid-transaction shape, for the create specifically.
///
/// Same engine behaviour the update path was pinned on: a revoke committed by another handle between
/// our claim and our commit costs the transaction a write conflict, and the `CREATE` is rolled back
/// with it. A guarded *read* before the create would commit over it —
/// `a_guarded_read_alone_is_not_atomic` is that negative control, and this is the positive one for the
/// row-creating arm. The control half runs first, on a second live credential and the same statement
/// shape, so the refusal below cannot be an unrelated commit failure.
#[tokio::test]
async fn a_credential_revoked_mid_transaction_does_not_commit_a_session_create() {
    let fx = fixture("createtx-alice").await;
    let credential = credential_of(&fx);

    // Control: claim + create + commit, with nothing racing, lands.
    let control = fx.db.inner().clone().begin().await.unwrap();
    assert_eq!(
        conditional_credential_claim(&control, &fx, &credential).await,
        1,
        "the credential is live when the control claims it"
    );
    session_create_statement(&control, "createtx:control", &fx)
        .await
        .expect("the control create runs");
    control
        .commit()
        .await
        .expect("live authority commits a session create");

    // The race: same two statements, and a revoke committed by another handle in between.
    let tx = fx.db.inner().clone().begin().await.unwrap();
    assert_eq!(
        conditional_credential_claim(&tx, &fx, &credential).await,
        1,
        "the credential is live when the raced transaction claims it"
    );
    session_create_statement(&tx, "createtx:raced", &fx)
        .await
        .expect("the raced create runs inside the transaction");
    AuthRepo::new(fx.db.inner())
        .revoke_credential(&fx.owner, &credential, Utc::now())
        .await
        .unwrap();
    let commit = tx.commit().await;
    assert!(
        commit.is_err(),
        "the commit was not refused, so the create would outlive the authority that claimed it"
    );

    assert_eq!(
        session_handle_rows(&fx, "createtx:raced").await,
        0,
        "the rolled-back create left a row behind"
    );
    assert_eq!(
        session_handle_rows(&fx, "createtx:control").await,
        1,
        "the control create did not land either, so the two arms cannot be told apart"
    );
}

/// Why the guard is not a read. Pinned as a negative control so nobody replaces the conditional write
/// with the `SELECT` that looks equivalent.
///
/// A guarded read inside a transaction, then a revoke from another handle, then the write and commit:
/// the commit SUCCEEDS and the data write survives over a revoked credential. That is a real TOCTOU on
/// this engine — not a hypothesis — and the only reason the repository does not use this shape.
#[tokio::test]
async fn a_guarded_read_alone_is_not_atomic() {
    let fx = fixture("readguard-alice").await;
    session(&fx, "readguard-handle").await;
    let credential = credential_of(&fx);

    let tx = fx.db.inner().clone().begin().await.unwrap();
    let rows = guarded_credential_read(&tx, &fx, &credential).await;
    assert_eq!(rows, 1, "the credential is live when the guard reads it");

    AuthRepo::new(fx.db.inner())
        .revoke_credential(&fx.owner, &credential, Utc::now())
        .await
        .unwrap();

    tx.query(
        "UPDATE `session` SET summary = 'over-a-dead-credential' WHERE external_id = $handle AND \
         account = $account;",
    )
    .bind(("handle", "readguard-handle".to_string()))
    .bind(("account", fx.account.as_record_id().clone()))
    .await
    .unwrap()
    .check()
    .unwrap();
    let committed = tx.commit().await;
    assert!(
        committed.is_ok(),
        "if the engine ever starts refusing this, the read shape becomes acceptable and this test \
         should be deleted along with the comment that forbids it"
    );
    let stored = dump_session(&fx, "readguard-handle").await;
    assert_eq!(
        stored["summary"].as_str(),
        Some("over-a-dead-credential"),
        "documented behaviour is that the read-guarded write commits over a revoked credential"
    );
}

/// A trusted-local context has no credential and must not claim one: it guards the account row, and a
/// suspension — not a revocation — is what stops it.
#[tokio::test]
async fn a_suspended_account_stops_both_local_and_hosted_writes() {
    let fx = fixture("suspend-alice").await;
    session(&fx, "suspend-handle").await;
    let access = AccessRepo::new(&fx.db);
    let local = access
        .local_principal(&fx.system, &fx.account)
        .await
        .unwrap();
    let local_owner = access.authorize(&local).await.unwrap();

    // Suspend the account out from under both live contexts.
    fx.db
        .inner()
        .query("UPDATE account SET status = 'suspended' WHERE id = $id;")
        .bind(("id", fx.account.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();

    for (label, owner) in [("hosted", &fx.owner), ("trusted-local", &local_owner)] {
        let repo = ScopedSessionRepo::new(fx.db.inner(), owner.clone());
        let outcome = repo
            .finalize("suspend-handle", Some("after suspension"), None)
            .await;
        assert!(
            matches!(outcome, Err(alexandria_storage::error::StorageError::Stale)),
            "a suspended account's {label} context must be refused as Stale, got {outcome:?}"
        );
    }
    let stored = dump_session(&fx, "suspend-handle").await;
    assert_eq!(
        stored["summary"],
        serde_json::Value::Null,
        "neither path may write for a suspended account"
    );
}

/// A recovery raises `auth_epoch`, and every credential minted before it is inert without its own row
/// changing. The scoped write has to feel that through the epoch comparison alone.
#[tokio::test]
async fn an_epoch_bump_stops_a_scoped_write_that_a_row_change_would_not() {
    let fx = fixture("epoch-alice").await;
    session(&fx, "epoch-handle").await;
    let before = dump_session(&fx, "epoch-handle").await;

    // Nothing touches the credential row: only the account's epoch moves, which is exactly what
    // `credential_entitles_read` compares and what a per-row revocation check cannot see.
    fx.db
        .inner()
        .query("UPDATE account SET auth_epoch = auth_epoch + 1 WHERE id = $id;")
        .bind(("id", fx.account.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();

    let repo = ScopedSessionRepo::new(fx.db.inner(), fx.owner.clone());
    let outcome = repo
        .finalize("epoch-handle", Some("after the epoch moved"), None)
        .await;
    assert!(
        matches!(outcome, Err(alexandria_storage::error::StorageError::Stale)),
        "a context issued under an obsolete epoch must be refused, got {outcome:?}"
    );
    let after = dump_session(&fx, "epoch-handle").await;
    assert_eq!(after, before, "the obsolete-epoch write must not land");
}

/// One live credential of an account must not vouch for a revoked sibling.
///
/// This is the reason the guard names a credential id rather than asking "does this account have a
/// live credential": a subquery over `credential WHERE account = $account AND revoked = false` would
/// return a row here, and the revoked bearer would keep writing.
#[tokio::test]
async fn a_sibling_live_credential_does_not_vouch_for_a_revoked_one() {
    let fx = fixture("sibling-alice").await;
    session(&fx, "sibling-handle").await;
    let revoked = credential_of(&fx);

    // A second, live agent token for the same account.
    let _sibling = AuthRepo::new(fx.db.inner())
        .mint_agent_token(&fx.owner, "sibling-token", Utc::now())
        .await
        .unwrap();
    AuthRepo::new(fx.db.inner())
        .revoke_credential(&fx.owner, &revoked, Utc::now())
        .await
        .unwrap();

    // Positive control that the account really does still hold a live credential.
    let live = fx
        .db
        .inner()
        .query(
            "SELECT count() FROM credential WHERE account = $account AND revoked = false AND \
             consumed = false AND kind = $kind GROUP ALL;",
        )
        .bind(("account", fx.account.as_record_id().clone()))
        .bind(("kind", credential_kind::AGENT.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap()
        .take::<Vec<serde_json::Value>>(0)
        .unwrap();
    assert!(
        live[0]["count"].as_i64().unwrap_or(0) >= 1,
        "the account must still hold a live credential, or this test proves nothing about masking"
    );

    let repo = ScopedSessionRepo::new(fx.db.inner(), fx.owner.clone());
    let outcome = repo.finalize("sibling-handle", Some("masked"), None).await;
    assert!(
        matches!(outcome, Err(alexandria_storage::error::StorageError::Stale)),
        "a revoked credential must not be vouched for by a sibling, got {outcome:?}"
    );
    let stored = dump_session(&fx, "sibling-handle").await;
    assert_eq!(stored["summary"], serde_json::Value::Null);
}

// ---------------------------------------------------------------------------------------------
// select_offline
// ---------------------------------------------------------------------------------------------

/// The named offline selection producer: explicit workspace, explicit project, validated pair.
#[tokio::test]
async fn offline_selection_requires_explicit_owned_workspace_project_pair() {
    let fx = fixture("offline-alice").await;
    let other = fixture("offline-bob").await;
    let access = AccessRepo::new(&fx.db);

    // Correct pair resolves.
    let scoped = access
        .select_offline(
            &fx.system,
            &fx.workspace,
            ReadSelection::Auto(Some(fx.project.clone())),
        )
        .await
        .unwrap();
    assert_eq!(scoped.workspace(), &fx.workspace);
    assert_eq!(
        scoped.scopes(),
        vec![
            MemoryScope::Personal,
            MemoryScope::Project(fx.project.clone())
        ],
        "`Auto(Some(project))` is Personal plus exactly that project"
    );

    // A project from another workspace fails, and fails the same way a nonexistent one does.
    let foreign = access
        .select_offline(
            &fx.system,
            &fx.workspace,
            ReadSelection::Auto(Some(other.project.clone())),
        )
        .await;
    let missing = access
        .select_offline(
            &fx.system,
            &fx.workspace,
            ReadSelection::Auto(Some(
                ProjectId::try_from(RecordId::parse_simple("project:never-existed").unwrap())
                    .unwrap(),
            )),
        )
        .await;
    assert!(
        matches!(
            foreign,
            Err(alexandria_storage::error::StorageError::NotFound)
        ),
        "a foreign project must be NotFound, got {foreign:?}"
    );
    assert_eq!(
        format!("{:?}", foreign.as_ref().err()),
        format!("{:?}", missing.as_ref().err()),
        "a foreign project and a nonexistent one must be indistinguishable"
    );

    // A workspace that does not exist is refused, not provisioned.
    let ghost = access
        .select_offline(
            &fx.system,
            &WorkspaceId::try_from(RecordId::parse_simple("workspace:ghost").unwrap()).unwrap(),
            ReadSelection::Assigned,
        )
        .await;
    assert!(matches!(
        ghost,
        Err(alexandria_storage::error::StorageError::NotFound)
    ));

    // Unassigned stays a review act even offline.
    let review = access
        .select_offline(
            &fx.system,
            &fx.workspace,
            ReadSelection::One(MemoryScope::Unassigned),
        )
        .await;
    assert!(matches!(
        review,
        Err(alexandria_storage::error::StorageError::InvalidInput(_))
    ));
    let explicit = access
        .select_offline(&fx.system, &fx.workspace, ReadSelection::UnassignedReview)
        .await
        .unwrap();
    assert_eq!(explicit.scopes(), vec![MemoryScope::Unassigned]);
}

/// `select_offline` is not reachable as a request fallback.
///
/// The authority is per-database: a capability minted against one store is refused by another, even
/// though both are `mem://` with the same namespace and database name. That pointer-equality refusal is
/// what stops a value obtained on one startup handle from being replayed somewhere else, and it is
/// checked before any row is read, so the answer cannot leak whether the workspace exists.
#[tokio::test]
async fn select_offline_refuses_a_capability_from_another_instance() {
    let fx = fixture("crossdb-alice").await;
    let mut other = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(other.inner())
        .await
        .unwrap();
    let foreign_system = other.claim_system_authority().unwrap();

    // Refused on `fx`'s store, naming a workspace that really does exist there.
    let outcome = AccessRepo::new(&fx.db)
        .select_offline(
            &foreign_system,
            &fx.workspace,
            ReadSelection::Auto(Some(fx.project.clone())),
        )
        .await;
    assert!(
        matches!(
            outcome,
            Err(alexandria_storage::error::StorageError::NotFound)
        ),
        "a capability issued by another instance must be refused outright, got {outcome:?}"
    );

    // Positive control on the store that *did* issue it: the same shape of call succeeds there, so the
    // refusal above is about the capability and not about the arguments.
    let ids = IdentityRepo::new(other.inner())
        .bootstrap_owner(
            "crossdb-owner",
            "$argon2id$fixture-prepared-hash-crossdb-owner-padding",
            &OperationId::new("crossdb-owner-bootstrap"),
        )
        .await
        .unwrap();
    let own = AccessRepo::new(&other)
        .select_offline(&foreign_system, &ids.workspace, ReadSelection::Auto(None))
        .await
        .unwrap();
    assert_eq!(own.scopes(), vec![MemoryScope::Personal]);

    // And the reverse direction: `fx`'s capability is refused by `other`.
    let reverse = AccessRepo::new(&other)
        .select_offline(&fx.system, &ids.workspace, ReadSelection::Auto(None))
        .await;
    assert!(matches!(
        reverse,
        Err(alexandria_storage::error::StorageError::NotFound)
    ));
}

/// An offline selection produces no write authorization at all: it selects a population, it does not
/// make the caller an owner.
#[tokio::test]
async fn select_offline_yields_a_read_population_not_an_owner_context() {
    let fx = fixture("offline-shape-alice").await;
    let access = AccessRepo::new(&fx.db);
    let scoped = access
        .select_offline(&fx.system, &fx.workspace, ReadSelection::Assigned)
        .await
        .unwrap();
    // The resolved partitions are exactly Personal plus the owner's own projects, and never
    // Unassigned — the same rule `select` applies to a hosted request.
    assert!(
        !scoped.scopes().contains(&MemoryScope::Unassigned),
        "`Assigned` excludes Unassigned offline too: {:?}",
        scoped.scopes()
    );
    assert_eq!(
        scoped.partitions().len(),
        2,
        "Personal plus the fixture's one project"
    );
    for partition in scoped.partitions() {
        assert_eq!(&partition.workspace, &fx.workspace);
    }
}

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

fn credential_of(fx: &Fixture) -> CredentialId {
    match fx.principal.provenance() {
        alexandria_storage::access::PrincipalProvenance::Credential(id) => id.clone(),
        other => panic!("fixture must hold a hosted credential, got {other:?}"),
    }
}

/// The create `ScopedSessionRepo::create` issues, run against a caller's transaction.
///
/// Written out rather than called through the repo because this test needs the statement *inside* the
/// same transaction as the claim, with a revoke injected between the two — which is the one ordering
/// the repository's own API cannot express.
async fn session_create_statement(
    tx: &surrealdb::method::Transaction<surrealdb::engine::any::Any>,
    handle: &str,
    fx: &Fixture,
) -> Result<usize, surrealdb::Error> {
    let response = tx
        .query(
            "CREATE `session` SET external_id = $handle, account = $account, workspace = $workspace, \
             tags = [] RETURN AFTER;",
        )
        .bind(("handle", handle.to_string()))
        .bind(("account", fx.account.as_record_id().clone()))
        .bind(("workspace", fx.workspace.as_record_id().clone()))
        .await?;
    let mut checked = response.check()?;
    Ok(checked.take::<Vec<serde_json::Value>>(0)?.len())
}

/// How many rows in `session` carry one handle, for the bound account.
async fn session_handle_rows(fx: &Fixture, handle: &str) -> i64 {
    let rows = fx
        .db
        .inner()
        .query("SELECT count() AS c FROM `session` WHERE external_id = $handle GROUP ALL;")
        .bind(("handle", handle.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap()
        .take::<Vec<serde_json::Value>>(0)
        .unwrap();
    rows.first().and_then(|r| r["c"].as_i64()).unwrap_or(0)
}

/// The exact conditional claim `guard_then_write` issues on the credential row.
async fn conditional_credential_claim(
    tx: &surrealdb::method::Transaction<surrealdb::engine::any::Any>,
    fx: &Fixture,
    credential: &CredentialId,
) -> usize {
    let mut response = tx
        .query(
            "UPDATE type::record('credential', $key) SET last_used_at = time::now() WHERE id = $id \
             AND account = $account AND revoked = false AND consumed = false AND auth_epoch = $epoch \
             AND (expires_at = NONE OR expires_at > time::now()) RETURN AFTER;",
        )
        .bind(("key", record_key(credential.as_record_id())))
        .bind(("id", credential.as_record_id().clone()))
        .bind(("account", fx.account.as_record_id().clone()))
        .bind(("epoch", 1i64))
        .await
        .unwrap()
        .check()
        .unwrap();
    response.take::<Vec<serde_json::Value>>(0).unwrap().len()
}

/// The rejected shape: the same predicate as a read.
async fn guarded_credential_read(
    tx: &surrealdb::method::Transaction<surrealdb::engine::any::Any>,
    fx: &Fixture,
    credential: &CredentialId,
) -> usize {
    let mut response = tx
        .query(
            "SELECT id FROM credential WHERE id = $id AND account = $account AND revoked = false \
             AND consumed = false AND auth_epoch = $epoch LIMIT 1;",
        )
        .bind(("id", credential.as_record_id().clone()))
        .bind(("account", fx.account.as_record_id().clone()))
        .bind(("epoch", 1i64))
        .await
        .unwrap()
        .check()
        .unwrap();
    response.take::<Vec<serde_json::Value>>(0).unwrap().len()
}

// ---------------------------------------------------------------------------------------------
// The reminder writes run the same commit predicate as every other scoped write
// ---------------------------------------------------------------------------------------------

/// A due-in-one-minute one-shot, as the scoped create takes it.
fn reminder(message: &str) -> NewReminder {
    let due = Utc::now() + chrono::Duration::minutes(1);
    NewReminder {
        message: message.to_string(),
        target_project: None,
        prov_project: None,
        prov_session_id: None,
        note: None,
        schedule_kind: "once".to_string(),
        due_at: Some(due),
        freq: None,
        time_of_day: None,
        weekdays: Vec::new(),
        day_of_month: None,
        cron_expr: None,
        next_due_at: Some(due),
    }
}

/// Rows of `reminder`, as stored, with their owner.
async fn reminder_rows(fx: &Fixture) -> Vec<serde_json::Value> {
    let mut response = fx
        .db
        .inner()
        .query(
            "SELECT id, status, delivered_count, next_due_at, account FROM reminder ORDER BY id;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    response.take(0).unwrap()
}

/// Every scoped reminder write refuses a revoked credential — **and** the same context writes while the
/// credential is live.
///
/// `set_reminder` is admitted for hosted bearers without a corpus-write grant (an explicit target
/// must still be an owned project handle), so these writes were the last request-path places where a
/// revocation landing after `admit` still got a committed row. The positive control runs first, on the
/// same context and the same store, so the refusals below cannot be the quiet success of a fixture that
/// could not write at all.
#[tokio::test]
async fn a_revoked_credential_stops_every_scoped_reminder_write() {
    let fx = fixture("reminder-revoke-alice").await;
    let credential = credential_of(&fx);
    let repo = ScopedReminderRepo::new(fx.db.inner(), fx.owner.clone());

    // Positive control: live authority creates.
    let created = repo
        .create(&reminder("standup notes"), Utc::now())
        .await
        .expect("a live credential may schedule a reminder");
    let id = alexandria_storage::record_id_to_string(
        created
            .id
            .as_ref()
            .expect("a created reminder carries an id"),
    );
    assert_eq!(
        reminder_rows(&fx).await[0]["account"].as_str(),
        Some(alexandria_storage::record_id_to_string(fx.account.as_record_id()).as_str()),
        "the create statement did not stamp the owner it was bound to"
    );

    AuthRepo::new(fx.db.inner())
        .revoke_credential(&fx.owner, &credential, Utc::now())
        .await
        .unwrap();

    let refusal = repo
        .create(&reminder("created over a dead credential"), Utc::now())
        .await;
    assert!(
        matches!(refusal, Err(alexandria_storage::error::StorageError::Stale)),
        "a revoked credential created a reminder: {refusal:?}"
    );
    assert_eq!(
        reminder_rows(&fx).await.len(),
        1,
        "the revoked create reached the stored rows"
    );

    let cancel = repo.cancel(&id, Utc::now()).await;
    assert!(
        matches!(cancel, Err(alexandria_storage::error::StorageError::Stale)),
        "a revoked credential cancelled a reminder: {cancel:?}"
    );
    let rows = reminder_rows(&fx).await;
    assert_eq!(
        rows[0]["status"].as_str(),
        Some("pending"),
        "the cancel wrote: {rows:?}"
    );

    // `seen` is the value the row carries: a refusal here can therefore only come from the authority
    // predicate, not from the conditional claim's own guard.
    let seen = chrono::DateTime::parse_from_rfc3339(rows[0]["next_due_at"].as_str().unwrap())
        .expect("a stored due time reads back")
        .with_timezone(&Utc);
    let claim = repo
        .record_delivery(&id, None, Some(seen), Utc::now())
        .await;
    assert!(
        matches!(claim, Err(alexandria_storage::error::StorageError::Stale)),
        "a revoked credential consumed a delivery: {claim:?}"
    );
    let after = reminder_rows(&fx).await;
    assert_eq!(
        after[0]["delivered_count"].as_i64(),
        Some(0),
        "the refused claim still counted a delivery: {after:?}"
    );
}

/// Suspension and an epoch bump stop the reminder writes too, on both local and hosted contexts.
///
/// The epoch case is the one a per-row revocation check cannot see: nothing touches the credential's row,
/// only the account's `auth_epoch` moves, and the guard compares it again inside the transaction.
#[tokio::test]
async fn a_suspended_or_reordered_account_stops_the_scoped_reminder_writes() {
    for (label, prepare) in [("suspension", true), ("epoch bump", false)] {
        let fx = fixture(&format!("reminder-{label}-alice")).await;
        let access = AccessRepo::new(&fx.db);
        let local = access
            .local_principal(&fx.system, &fx.account)
            .await
            .unwrap();
        let local_owner = access.authorize(&local).await.unwrap();

        if prepare {
            fx.db
                .inner()
                .query("UPDATE account SET status = 'suspended' WHERE id = $id;")
                .bind(("id", fx.account.as_record_id().clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
        } else {
            fx.db
                .inner()
                .query("UPDATE account SET auth_epoch = auth_epoch + 1 WHERE id = $id;")
                .bind(("id", fx.account.as_record_id().clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
        }

        for (who, owner) in [("hosted", &fx.owner), ("trusted-local", &local_owner)] {
            let repo = ScopedReminderRepo::new(fx.db.inner(), owner.clone());
            let outcome = repo
                .create(&reminder(&format!("{label}/{who}")), Utc::now())
                .await;
            assert!(
                matches!(outcome, Err(alexandria_storage::error::StorageError::Stale)),
                "a {label} account's {who} context created a reminder: {outcome:?}"
            );
        }
        assert!(
            reminder_rows(&fx).await.is_empty(),
            "{label} did not stop the reminder create after all"
        );
    }
}

/// A revocation landing between the claim and the commit costs a reminder **create** its commit.
///
/// The accepted session case pins this for an `UPDATE`; a `CREATE` has no `WHERE` to hang a guard on, so
/// its atomicity comes from the other direction — the conditional write on the credential row and the
/// `CREATE` share one transaction. This drives the two statements by hand, in exactly the order
/// `guarded_run` issues them, because `create()` opens its own transaction and a test cannot park a
/// revoke inside it. The row it must not leave behind is the one that would be visible to the account's
/// *later* requests: a reminder scheduled by a credential that no longer exists.
#[tokio::test]
async fn a_credential_revoked_mid_transaction_does_not_commit_a_scoped_reminder_create() {
    let fx = fixture("midtx-reminder-alice").await;
    let credential = credential_of(&fx);
    let due = Utc::now() + chrono::Duration::minutes(1);

    let tx = fx.db.inner().clone().begin().await.unwrap();
    assert_eq!(
        conditional_credential_claim(&tx, &fx, &credential).await,
        1,
        "the credential is live when the guard is claimed"
    );
    tx.query(
        "CREATE reminder SET message = 'raced', schedule_kind = 'once', due_at = $due, next_due_at \
         = $due, status = 'pending', delivered_count = 0, account = $account;",
    )
    .bind(("due", due))
    .bind(("account", fx.account.as_record_id().clone()))
    .await
    .unwrap()
    .check()
    .unwrap();

    AuthRepo::new(fx.db.inner())
        .revoke_credential(&fx.owner, &credential, Utc::now())
        .await
        .unwrap();

    assert!(
        tx.commit().await.is_err(),
        "the create committed over a revocation that landed after its own claim"
    );
    assert!(
        reminder_rows(&fx).await.is_empty(),
        "the raced create survived the revoked credential"
    );
}

/// The conditional delivery claim is unchanged by the guard around it.
///
/// Two guards on one statement, both necessary: the account predicate says *whose* row may be claimed and
/// `next_due_at = $seen` says the row still looks exactly as this caller read it. Adding the commit
/// predicate must not have quietly replaced the second — a `check_reminders` racing another consumer, or a
/// `cancel` landing between the read and the claim, still has to lose the claim rather than clobber it.
#[tokio::test]
async fn the_scoped_delivery_claim_still_refuses_a_row_that_moved_under_it() {
    let fx = fixture("claim-cas-alice").await;
    let repo = ScopedReminderRepo::new(fx.db.inner(), fx.owner.clone());
    let created = repo
        .create(&reminder("the standup is at nine"), Utc::now())
        .await
        .expect("live authority creates");
    let id = alexandria_storage::record_id_to_string(created.id.as_ref().expect("row has id"));

    // A stale `seen`: somebody else advanced the row first.
    let stale = repo
        .record_delivery(
            &id,
            None,
            Some(Utc::now() - chrono::Duration::hours(1)),
            Utc::now(),
        )
        .await
        .expect("the claim runs");
    assert!(!stale, "a claim with a stale `next_due_at` was taken");
    let rows = reminder_rows(&fx).await;
    assert_eq!(
        rows[0]["delivered_count"].as_i64(),
        Some(0),
        "the refused claim counted a delivery anyway: {rows:?}"
    );

    // The honest claim, on the value the row actually holds.
    let seen = created.next_due_at;
    let taken = repo
        .record_delivery(&id, None, seen, Utc::now())
        .await
        .expect("the claim runs");
    assert!(taken, "the matching claim was refused");
    let after = reminder_rows(&fx).await;
    assert_eq!(after[0]["status"].as_str(), Some("delivered"), "{after:?}");
    assert_eq!(after[0]["delivered_count"].as_i64(), Some(1));
}

fn record_key(id: &RecordId) -> String {
    alexandria_storage::record_id_to_string(id)
        .split_once(':')
        .map(|(_, key)| key.to_string())
        .unwrap_or_default()
}

async fn dump_session(fx: &Fixture, handle: &str) -> serde_json::Value {
    let mut response = fx
        .db
        .inner()
        .query(
            "SELECT external_id, agent_id, model, started_at, ended_at, summary, tags, account, \
             workspace FROM `session` WHERE external_id = $handle;",
        )
        .bind(("handle", handle.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    assert_eq!(rows.len(), 1, "exactly one row for {handle}");
    rows.into_iter().next().unwrap()
}
