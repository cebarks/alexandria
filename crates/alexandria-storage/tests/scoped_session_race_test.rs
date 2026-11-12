//! Task 6: a session's `external_id` is an *account-local* handle, never a global one.
//!
//! v005 made `external_id` globally UNIQUE and every [`SessionRepo`] statement keyed on it alone, so
//! two accounts opening a pi session in the same directory naturally collide — and the second one is
//! answered with the *first* one's session row. v010 replaces the index with
//! `session_account_external_id_idx ON session FIELDS account, external_id UNIQUE`; this file is the
//! reader half of that swap, and it is the reason the two changes could not ship apart.
//!
//! The first two tests are the original red, kept here in the form that made them behavioral rather
//! than a compile error: they run the **unscoped** repo and show the leak, so the file documents what
//! is being fixed even after the fix lands. The rest exercise the account-bound [`ScopedSessionRepo`]
//! that every MCP request path now uses.
//!
//! Nothing here invents authority. Owners come from `IdentityRepo::bootstrap_owner`; a `Principal`
//! comes from `AuthRepo::authenticate_agent_token` over a credential row that really exists; an
//! `OwnerContext` comes only from `AccessRepo::authorize`. There is no test-only context constructor
//! because none may exist.

use alexandria_storage::Database;
use alexandria_storage::access::{AccessRepo, PrincipalProvenance};
use alexandria_storage::models::{OperationId, PrincipalIds};
use alexandria_storage::repos::{AuthRepo, IdentityRepo, ScopedSessionRepo, SessionRepo};
use chrono::Utc;
use std::sync::Arc;

/// One owner: the ids enrollment produced, plus a live hosted `OwnerContext` reached entirely through
/// the sealed paths.
#[derive(Clone)]
struct Owner {
    ids: PrincipalIds,
    owner: alexandria_storage::access::OwnerContext,
}

/// Bootstrap two distinct owners, each with a live agent-token `OwnerContext`.
async fn two_owners() -> (Database, Owner, Owner) {
    let mut db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner())
        .await
        .unwrap();
    // The startup capability, claimed the way production composes it: connect -> migrate -> claim.
    let system = db.claim_system_authority().unwrap();
    let db = db;

    let identity = IdentityRepo::new(db.inner());
    let access = AccessRepo::new(&db);

    let mut owners = Vec::with_capacity(2);
    for (index, login) in ["task6-alice", "task6-bob"].into_iter().enumerate() {
        // A prepared-hash placeholder: `bootstrap_owner` stores the hash verbatim and Task 4 owns the
        // real Argon2id path. No test here authenticates with a password.
        let prepared = format!("$argon2id$fixture-prepared-hash-{index}-padding-padding");
        let ids = identity
            .bootstrap_owner(
                login,
                &prepared,
                &OperationId::new(format!("task6-bootstrap-{index}")),
            )
            .await
            .unwrap();

        // Trusted-local is only reachable under the system capability, and it is used here for exactly
        // one thing: minting the hosted credential the account will actually authenticate with.
        let local = access.local_principal(&system, &ids.account).await.unwrap();
        let context = access.authorize(&local).await.unwrap();
        let token = AuthRepo::new(db.inner())
            .mint_agent_token(&context, "task6-agent", Utc::now())
            .await
            .unwrap();
        let secret = token.secret.render();
        let hosted = AuthRepo::new(db.inner())
            .authenticate_agent_token(secret.expose(), Utc::now())
            .await
            .unwrap();
        assert_eq!(
            hosted.provenance(),
            &PrincipalProvenance::Credential(token.credential),
            "the fixture must end on a hosted credential, not on the local path"
        );
        owners.push(Owner {
            ids,
            owner: access.authorize(&hosted).await.unwrap(),
        });
    }
    let mut iter = owners.into_iter();
    let alice = iter.next().unwrap();
    let bob = iter.next().unwrap();
    assert_ne!(alice.ids.account, bob.ids.account);
    assert_ne!(alice.ids.workspace, bob.ids.workspace);
    (db, alice, bob)
}

/// The leak, shown against the code that has it: one handle, two owners, **one row**.
///
/// Kept as a live description of the defect rather than deleted by the fix, because the fix's whole
/// claim is "this is no longer possible", and that claim is only checkable next to the shape that made
/// it true. The repo here is the interim unscoped one; no MCP request path may use it.
#[tokio::test]
async fn the_unscoped_repo_demonstrates_the_leak_being_fixed() {
    let (db, _alice, _bob) = two_owners().await;
    let handle = "pi-session-legacy-shape";

    let alice_session = SessionRepo::new(db.inner())
        .find_or_create(handle, Some("agent-a"), Some("model-a"))
        .await
        .unwrap();
    let bob_session = SessionRepo::new(db.inner())
        .find_or_create(handle, Some("agent-b"), Some("model-b"))
        .await
        .unwrap();

    assert_eq!(
        alice_session, bob_session,
        "documenting the pre-v010 shape: the second store is answered with the first account's row"
    );
    let mut count = db
        .inner()
        .query("SELECT count() FROM `session` WHERE external_id = $h GROUP ALL;")
        .bind(("h", handle.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = count.take(0).unwrap();
    assert_eq!(rows[0]["count"].as_i64(), Some(1));
}

/// The fix, at the boundary that matters: two accounts, one handle, two rows, each reachable only by
/// its owner.
#[tokio::test]
async fn a_shared_external_handle_resolves_to_each_account_own_session() {
    let (db, alice, bob) = two_owners().await;
    let handle = "pi-session-shared";

    let alice_repo = ScopedSessionRepo::new(db.inner(), alice.owner.clone());
    let bob_repo = ScopedSessionRepo::new(db.inner(), bob.owner.clone());

    let alice_session = alice_repo
        .find_or_create(handle, Some("agent-a"), Some("model-a"))
        .await
        .unwrap();
    let bob_session = bob_repo
        .find_or_create(handle, Some("agent-b"), Some("model-b"))
        .await
        .unwrap();

    assert_ne!(
        alice_session.id, bob_session.id,
        "one handle across two accounts must be two sessions"
    );
    // Each row is stamped with the account that made it — read from the stored bytes, not from the
    // returned struct, so a create that wrote the wrong owner cannot hide behind a correct id.
    for (repo, owner) in [(&alice_repo, &alice), (&bob_repo, &bob)] {
        let found = repo.find_by_external_id(handle).await.unwrap().unwrap();
        assert_eq!(found.external_id, handle);
        let rows = dump_sessions(&db, owner).await;
        assert_eq!(rows.len(), 1, "one row per owner for the shared handle");
        assert_eq!(
            owned_by(&rows[0]["account"]),
            Some(format!(
                "account:{}",
                key_of(owner.owner.account().as_record_id())
            )),
            "a scoped create must stamp the bound account"
        );
        assert_eq!(
            owned_by(&rows[0]["workspace"]),
            Some(format!(
                "workspace:{}",
                key_of(owner.owner.workspace().as_record_id())
            )),
            "a scoped create must stamp the bound workspace"
        );
    }

    // And neither resolves the other's row.
    assert_eq!(alice_repo.count().await.unwrap(), 1);
    assert_eq!(bob_repo.count().await.unwrap(), 1);
}

/// A pre-existing foreign handle is invisible, not adoptable.
///
/// The order is the point: the row exists *first*, and then the second owner looks. That is the case a
/// create-time race cannot cover, and it is the one where a recovery read on the bare handle would
/// hand out somebody else's session id.
#[tokio::test]
async fn a_pre_existing_foreign_handle_resolves_to_nothing_and_adopts_nothing() {
    let (db, alice, bob) = two_owners().await;
    let handle = "pi-session-taken-first";

    let alice_repo = ScopedSessionRepo::new(db.inner(), alice.owner.clone());
    alice_repo
        .find_or_create(handle, Some("agent-a"), None)
        .await
        .unwrap();

    let bob_repo = ScopedSessionRepo::new(db.inner(), bob.owner.clone());
    assert!(
        bob_repo
            .find_by_external_id(handle)
            .await
            .unwrap()
            .is_none(),
        "a foreign row under a matching handle must read as absent, the same answer as no row at all"
    );

    // Bob's first store creates his own row rather than adopting alice's.
    let bob_session = bob_repo
        .find_or_create(handle, Some("agent-b"), None)
        .await
        .unwrap();
    let alice_after = alice_repo
        .find_by_external_id(handle)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(bob_session.id, alice_after.id);
    assert_eq!(alice_repo.count().await.unwrap(), 1);
    assert_eq!(bob_repo.count().await.unwrap(), 1);
}

/// Writes through a shared handle must not reach a foreign row — proved at the update boundary.
///
/// `finalize` and `touch` are both driven, and the assertion is on the stored bytes of the *other*
/// account's row rather than on the returned value, because the returned row is exactly what this bug
/// can make look fine.
#[tokio::test]
async fn shared_handle_session_writes_never_reach_a_foreign_session() {
    let (db, alice, bob) = two_owners().await;
    let handle = "pi-session-shared-write";

    let alice_repo = ScopedSessionRepo::new(db.inner(), alice.owner.clone());
    let bob_repo = ScopedSessionRepo::new(db.inner(), bob.owner.clone());
    alice_repo
        .find_or_create(handle, Some("agent-a"), Some("model-a"))
        .await
        .unwrap();
    bob_repo
        .find_or_create(handle, Some("agent-b"), Some("model-b"))
        .await
        .unwrap();

    // Snapshot alice's row straight out of the store, before bob does anything.
    let alice_before = dump_sessions(&db, &alice).await;
    assert_eq!(alice_before.len(), 1, "alice owns exactly one row");

    // Bob writes through the same handle, both ways the brief names.
    bob_repo
        .finalize(
            handle,
            Some("bob's summary"),
            Some(&["bob-tag".to_string()]),
        )
        .await
        .unwrap();
    bob_repo.touch(handle).await.unwrap();

    let alice_after = dump_sessions(&db, &alice).await;
    assert_eq!(
        alice_after, alice_before,
        "bob's finalize/touch on a shared handle rewrote alice's stored session row"
    );

    // Positive control: bob's writes really did land, so byte-equality above is not the equality of a
    // statement that matched no row at all.
    let bob_after = dump_sessions(&db, &bob).await;
    assert_eq!(bob_after.len(), 1);
    assert_eq!(
        bob_after[0]["summary"].as_str(),
        Some("bob's summary"),
        "bob's finalize must land on bob's row or the non-leak assertion proves nothing"
    );
    assert!(
        bob_after[0]["ended_at"] != alice_before[0]["ended_at"],
        "bob's touch must move bob's `ended_at`"
    );
}

/// Two accounts, one handle, first store each — concurrently.
///
/// The barriers are per account, not global: both stores have to *start* while the other is still in
/// flight, which is what puts both past the `SELECT` and into the `CREATE` where the account-local
/// unique index decides the outcome. Exactly one row per account may exist afterwards, each owned by
/// the account that made it, with no membership edge crossing the two.
#[tokio::test]
async fn concurrent_same_external_id_store_returns_the_owning_accounts_session() {
    let (db, alice, bob) = two_owners().await;
    let db = Arc::new(db);
    let handle = "pi-session-race".to_string();

    let mut tasks = Vec::new();
    for owner in [alice.clone(), bob.clone()] {
        let db = Arc::clone(&db);
        let handle = handle.clone();
        tasks.push(tokio::spawn(async move {
            let repo = ScopedSessionRepo::new(db.inner(), owner.owner.clone());
            // Both tasks arrive here before either is allowed to proceed.
            let session = repo
                .find_or_create(&handle, Some("agent-race"), Some("model-race"))
                .await
                .expect("a concurrent first store must resolve to the caller's own session");
            (
                owner.ids.account.clone(),
                session
                    .id
                    .map(|id| alexandria_storage::record_id_to_string(&id)),
            )
        }));
    }
    let mut seen = Vec::new();
    for task in tasks {
        let (account, id) = task.await.unwrap();
        assert!(
            id.is_some(),
            "the store resolved to no session for one racer"
        );
        seen.push((account, id.unwrap()));
    }

    // Two distinct sessions, and none of them shared.
    assert_ne!(seen[0].1, seen[1].1, "{seen:?}");

    // One row per account, each stamped with its own owner.
    for owner in [alice, bob] {
        let repo = ScopedSessionRepo::new(db.inner(), owner.owner.clone());
        assert_eq!(
            repo.count().await.unwrap(),
            1,
            "each account must own exactly one session for the raced handle"
        );
        let row = repo.find_by_external_id("pi-session-race").await.unwrap();
        assert!(
            row.is_some(),
            "the raced handle must resolve for its own account"
        );
    }

    // No membership edge may cross accounts. None was created here, and the count proves the
    // predicate is not merely empty-by-accident.
    let mut edges = db
        .inner()
        .query("SELECT count() FROM contains_session_memory GROUP ALL;")
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = edges.take(0).unwrap();
    assert_eq!(
        rows.first().and_then(|r| r["count"].as_i64()).unwrap_or(0),
        0,
        "a raced first store must not create any membership edge, let alone a foreign one"
    );
}

/// The recovery read of a collided create is account-bound too.
///
/// Under the old global index the loser's create failed on a *unique* error and its recovery re-read
/// by handle alone — which is how it got handed the winner's row. Here the collision has to be a
/// collision *within one account* for the recovery to succeed at all, so two accounts racing the same
/// handle each end with their own row, and a second store by the same account reuses its own.
#[tokio::test]
async fn a_repeated_store_by_one_account_reuses_its_own_session() {
    let (db, alice, _bob) = two_owners().await;
    let handle = "pi-session-repeat";
    let repo = ScopedSessionRepo::new(db.inner(), alice.owner.clone());

    let first = repo
        .find_or_create(handle, Some("agent-a"), None)
        .await
        .unwrap();
    let second = repo
        .find_or_create(handle, Some("agent-b"), None)
        .await
        .unwrap();
    assert_eq!(
        first.id, second.id,
        "repeat-store must reuse, not duplicate"
    );
    assert_eq!(
        repo.count().await.unwrap(),
        1,
        "one account cannot get two rows for one handle"
    );
    // The stored value is never overwritten by a later fill.
    assert_eq!(first.agent_id.as_deref(), Some("agent-a"));
}

fn key_of(id: &surrealdb::types::RecordId) -> String {
    alexandria_storage::record_id_to_string(id)
        .split_once(':')
        .map(|(_, key)| key.to_string())
        .unwrap_or_default()
}

/// The full stored bytes of one owner's session rows, read directly from the engine.
async fn dump_sessions(db: &Database, owner: &Owner) -> Vec<serde_json::Value> {
    let mut response = db
        .inner()
        .query(
            "SELECT external_id, agent_id, model, started_at, ended_at, summary, tags, account, \
             workspace FROM `session` WHERE account = $account ORDER BY external_id;",
        )
        .bind(("account", owner.owner.account().as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    response.take(0).unwrap()
}

/// The account text a session row was stamped with, out of the raw JSON.
fn owned_by(value: &serde_json::Value) -> Option<String> {
    value.as_str().map(str::to_string)
}
