//! The work bound: what one account may take, how the queue rotates, and what a refusal must not cost.
//!
//! Task 5's other file asks whether a request is *allowed*; this one asks whether the process can *do* it.
//! Both budgets exist because the expensive paths are not proportional to their requests: a retrieval pays
//! an embedding call, an enrollment pays an Argon2 digest at ~19 MiB. The cases below are stated in terms of
//! slots and accounts rather than wall-clock percentages, so raising a tunable means raising it here too.
//!
//! Every principal is a real one. `Principal` has no public constructor by design, so each case admits an
//! agent token through the service and asks for work as the account behind it: a bound that could only be
//! exercised with a fabricated actor would not be the bound the transport uses.

use alexandria_mcp::auth::{
    ATTEMPT_BUCKETS, AUTH_ATTEMPT_WINDOW, AUTH_ATTEMPTS_PER_MINUTE, Admission,
    EXPENSIVE_ACTIVE_PER_ACCOUNT, EXPENSIVE_IN_FLIGHT_GLOBAL, EXPENSIVE_QUEUED_PER_ACCOUNT,
    HASH_JOBS_CONCURRENT, MAX_BODY_BYTES, Permit, RequestClass,
};
use alexandria_mcp::error::ServiceError;
use alexandria_storage::access::Principal;
use axum::http::StatusCode;
use chrono::{TimeDelta, Utc};
use serde_json::json;

mod fixtures;
use fixtures::*;

/// The per-account and global expensive bounds, and the refusal behind them.
///
/// Alice may hold two slots and queue two more; the next thing she asks for is refused rather than parked
/// indefinitely. Both halves matter: the active cap is the fairness mechanism, the queue cap is the memory
/// one, and a refusal that arrived earlier would starve her own legitimate third call.
#[tokio::test(flavor = "multi_thread")]
async fn the_expensive_bound_is_two_active_and_two_queued_per_account() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.principal_of(&accounts.alice.bearer).await;

    let mut held = Vec::new();
    for index in 0..EXPENSIVE_ACTIVE_PER_ACCOUNT {
        let permit = harness
            .admission
            .acquire(&alice, RequestClass::Expensive)
            .await
            .unwrap_or_else(|error| panic!("slot {index} was refused: {error:?}"));
        assert!(permit.is_expensive());
        assert_eq!(permit.account(), Some(alice.account()));
        held.push(permit);
    }
    assert_eq!(harness.admission.expensive_active(), 2);

    let queued = queue_two(&harness.admission, &alice);
    harness.wait_admission_queued(2).await;
    assert_eq!(harness.admission.expensive_in_flight(), 4);

    let excess = harness
        .admission
        .acquire(&alice, RequestClass::Expensive)
        .await;
    assert!(
        matches!(excess, Err(ServiceError::Quota)),
        "a third queued request was accepted: {:?}",
        excess.err()
    );

    // Protocol traffic is not metered by this bound at all: the handshake must not starve on a saturated
    // account.
    let protocol = harness
        .admission
        .acquire(&alice, RequestClass::Protocol)
        .await
        .expect("protocol work is not queued");
    assert!(!protocol.is_expensive());
    assert_eq!(protocol.account(), None);

    // Releasing the active pair must let exactly the two parked waiters through — and the permits have to
    // stay alive while the scheduler is read, or "active" would describe nothing.
    drop(held);
    let granted = tokio::time::timeout(std::time::Duration::from_secs(10), join_all(queued))
        .await
        .expect("the queued work was dispatched");
    assert_eq!(granted.len(), 2, "not every parked waiter ran");
    assert!(
        granted.iter().all(Permit::is_expensive),
        "a grant was not an expensive slot"
    );
    assert_eq!(
        harness.admission.expensive_active(),
        2,
        "a grant did not take its slot"
    );
    assert_eq!(
        harness.admission.expensive_queued(),
        0,
        "a granted waiter stayed queued"
    );

    drop(granted);
    for _ in 0..400 {
        if harness.admission.expensive_in_flight() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // The bound reopened rather than latching.
    let again = harness
        .admission
        .acquire(&alice, RequestClass::Expensive)
        .await;
    assert!(
        again.is_ok(),
        "the account was locked out: {:?}",
        again.err()
    );
}

/// A hot account cannot keep a second account waiting behind its own queue.
///
/// Alice holds her two slots and parks two more; bob then asks for two of his own. The grants must reach bob
/// at once rather than behind alice's parked waiters, and alice's parked waiters must neither displace him
/// nor be forgotten when her own slots free.
///
/// What this case can and cannot show is worth stating, because the queue's rotation is not otherwise
/// observable: `Principal` has no constructor, so waiters can only be parked by a real account hitting its
/// own cap or the global bound, and under those two rules an account is only ever eligible for a slot its
/// own account freed. The strict per-account cap is therefore what carries the fairness property end to
/// here, and the rotation in `Scheduler::dispatch` is what stops one account's parked queue from being
/// served twice in a row the moment two accounts are eligible together.
#[tokio::test(flavor = "multi_thread")]
async fn a_hot_account_cannot_keep_a_second_account_waiting() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.principal_of(&accounts.alice.bearer).await;
    let bob = harness.principal_of(&accounts.bob.bearer).await;

    let alice_held = vec![
        harness
            .admission
            .acquire(&alice, RequestClass::Expensive)
            .await
            .unwrap(),
        harness
            .admission
            .acquire(&alice, RequestClass::Expensive)
            .await
            .unwrap(),
    ];
    let alice_parked = queue_two(&harness.admission, &alice);
    harness
        .wait_admission_queued(EXPENSIVE_QUEUED_PER_ACCOUNT)
        .await;
    assert_eq!(
        harness.admission.expensive_active(),
        EXPENSIVE_ACTIVE_PER_ACCOUNT,
        "somebody other than alice is running"
    );

    // Bob arrives while alice has two waiters ahead of him in a FIFO. Both of his slots must be immediate.
    let granted_to_bob = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        futures(bob.clone(), &harness.admission),
    )
    .await
    .expect("bob was made to wait behind alice's parked queue");
    assert_eq!(granted_to_bob.len(), 2);
    assert!(
        granted_to_bob
            .iter()
            .all(|permit| permit.account() == Some(bob.account())),
        "bob's grants named another account"
    );
    assert_eq!(
        harness.admission.expensive_queued(),
        EXPENSIVE_QUEUED_PER_ACCOUNT,
        "bob's arrival displaced alice's parked waiters"
    );

    // Freeing alice's own slots serves alice's parked waiters — and never takes more than her cap at once.
    drop(alice_held);
    let served = tokio::time::timeout(std::time::Duration::from_secs(10), join_all(alice_parked))
        .await
        .expect("alice's parked waiters were eventually granted");
    assert_eq!(served.len(), 2);
    assert!(
        served
            .iter()
            .all(|permit| permit.account() == Some(alice.account())),
        "alice's parked waiters were not hers"
    );
    assert_eq!(
        harness.admission.expensive_active(),
        2 * EXPENSIVE_ACTIVE_PER_ACCOUNT,
        "the per-account cap did not hold while both accounts were busy"
    );
}

/// Two grants for one principal, taken in parallel.
async fn futures(principal: Principal, admission: &Admission) -> Vec<Permit> {
    let first = admission.acquire(&principal, RequestClass::Expensive);
    let second = admission.acquire(&principal, RequestClass::Expensive);
    let (first, second) = tokio::join!(first, second);
    vec![
        first.expect("bob's first slot"),
        second.expect("bob's second slot"),
    ]
}

/// Work whose credential was revoked while it waited is refused at dispatch, never run.
#[tokio::test(flavor = "multi_thread")]
async fn revoked_work_is_never_dispatched() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.principal_of(&accounts.alice.bearer).await;
    let owner = harness.authorize(&alice).await;

    let held = vec![
        harness
            .admission
            .acquire(&alice, RequestClass::Expensive)
            .await
            .unwrap(),
        harness
            .admission
            .acquire(&alice, RequestClass::Expensive)
            .await
            .unwrap(),
    ];
    let waiting = {
        let admission = harness.admission.clone();
        let principal = alice.clone();
        tokio::spawn(async move {
            admission
                .acquire(&principal, RequestClass::Expensive)
                .await
                .is_ok()
        })
    };
    harness.wait_admission_queued(1).await;

    harness.revoke(&owner, &accounts.alice.credential).await;
    drop(held);

    let dispatched = tokio::time::timeout(std::time::Duration::from_secs(10), waiting)
        .await
        .expect("the waiter settled")
        .expect("the waiter task ran");
    assert!(
        !dispatched,
        "a revoked credential was dispatched from the queue"
    );
    assert_eq!(
        harness.admission.expensive_active(),
        0,
        "a refused waiter took a slot"
    );
    assert_eq!(
        harness.admission.expensive_queued(),
        0,
        "a refused waiter stayed queued"
    );

    // The bus still says revoked, so the account cannot re-queue either.
    let again = harness
        .admission
        .acquire(&alice, RequestClass::Expensive)
        .await;
    assert!(
        matches!(again, Err(ServiceError::Stale)),
        "a revoked principal was admitted to the queue again: {:?}",
        again.err()
    );
}

/// A waiter whose future is dropped gives its queue place back.
///
/// This is the hang-up case. A leaked place is not visible as a wrong answer; it is visible as a budget that
/// shrinks until the account can no longer queue anything, so the assertion is about the *next* request
/// being accepted rather than about a counter.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_waiter_returns_its_queue_place() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.principal_of(&accounts.alice.bearer).await;

    let mut held = Vec::new();
    for _ in 0..EXPENSIVE_ACTIVE_PER_ACCOUNT {
        held.push(
            harness
                .admission
                .acquire(&alice, RequestClass::Expensive)
                .await
                .unwrap(),
        );
    }
    let waiters = queue_two(&harness.admission, &alice);
    harness.wait_admission_queued(2).await;
    for waiter in waiters {
        waiter.abort();
    }
    for _ in 0..400 {
        if harness.admission.expensive_queued() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        harness.admission.expensive_queued(),
        0,
        "a cancelled waiter stayed queued"
    );
    assert_eq!(
        harness.admission.expensive_in_flight(),
        EXPENSIVE_ACTIVE_PER_ACCOUNT,
        "a cancelled waiter kept a slot"
    );

    // The places are back: the same account can queue again instead of being refused.
    let replacement = queue_two(&harness.admission, &alice);
    harness.wait_admission_queued(2).await;
    drop(held);
    let accepted = tokio::time::timeout(std::time::Duration::from_secs(10), join_all(replacement))
        .await
        .expect("the replacements were granted");
    assert_eq!(accepted.len(), 2, "the queue places were not returned");
}

/// The digest budget is two, and a third job waits rather than running.
#[tokio::test(flavor = "multi_thread")]
async fn the_hash_budget_holds_two_jobs_and_waits_for_a_third() {
    let harness = Harness::hosted().await;
    let first = harness.admission.hash_permit().await.expect("hash 1");
    let second = harness.admission.hash_permit().await.expect("hash 2");
    assert_eq!(harness.admission.hash_running(), 2);

    let third = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        harness.admission.hash_permit(),
    )
    .await;
    assert!(
        third.is_err(),
        "a third digest job started alongside two running ones"
    );
    assert_eq!(harness.admission.hash_running(), 2);

    drop(first);
    let freed = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        harness.admission.hash_permit(),
    )
    .await
    .expect("a released slot reaches the next job");
    assert_eq!(harness.admission.hash_running(), 2);
    drop(freed);
    drop(second);
    assert_eq!(
        harness.admission.hash_running(),
        0,
        "a digest permit leaked"
    );
}

/// The attempt budget is five a minute, measured along each axis independently.
///
/// Both axes matter: rotating identifiers defeats a source-only limit, and spraying sources defeats an
/// identifier-only one. The window is counted from an instant the caller passes, so the expiry half is
/// asserted without sleeping a minute — and it is the transport that supplies that instant, never a client.
#[tokio::test]
async fn the_attempt_budget_is_five_a_minute_on_each_axis() {
    let harness = Harness::hosted().await;
    let now = Utc::now();

    for index in 0..AUTH_ATTEMPTS_PER_MINUTE {
        harness
            .admission
            .limit_auth_attempt("attacker-source", &format!("victim-{index}"), now)
            .unwrap_or_else(|error| panic!("source attempt {index} was refused: {error:?}"));
    }
    let sixth = harness
        .admission
        .limit_auth_attempt("attacker-source", "victim-9", now);
    assert!(
        matches!(sixth, Err(ServiceError::Quota)),
        "the source axis did not bite: {sixth:?}"
    );
    harness
        .admission
        .limit_auth_attempt("innocent-source", "victim-0", now)
        .expect("another source keeps its own budget");

    for index in 0..AUTH_ATTEMPTS_PER_MINUTE {
        harness
            .admission
            .limit_auth_attempt(&format!("peer-{index}"), "one-account", now)
            .unwrap_or_else(|error| panic!("identifier attempt {index} was refused: {error:?}"));
    }
    let sprayed = harness
        .admission
        .limit_auth_attempt("peer-99", "one-account", now);
    assert!(
        matches!(sprayed, Err(ServiceError::Quota)),
        "the identifier axis did not bite: {sprayed:?}"
    );

    harness
        .admission
        .limit_auth_attempt("attacker-source", "victim-9", now + TimeDelta::minutes(2))
        .expect("the window never closed");
}

/// The attempt cache is bounded, and a refusal still counts.
#[tokio::test]
async fn the_attempt_cache_is_bounded_and_counts_refusals() {
    let harness = Harness::hosted().await;
    let now = Utc::now();
    for index in 0..(ATTEMPT_BUCKETS + 500) {
        // Distinct source *and* identifier per probe: nothing is refused and every key is new.
        let _ = harness.admission.limit_auth_attempt(
            &format!("source-{index}"),
            &format!("login-{index}"),
            now,
        );
    }
    assert!(
        harness.admission.tracked_attempt_keys() <= ATTEMPT_BUCKETS,
        "the limiter outgrew its bound: {}",
        harness.admission.tracked_attempt_keys()
    );

    for _ in 0..AUTH_ATTEMPTS_PER_MINUTE {
        harness
            .admission
            .limit_auth_attempt("counted", "login", now)
            .expect("inside the budget");
    }
    assert!(
        harness
            .admission
            .limit_auth_attempt("counted", "login", now)
            .is_err(),
        "the sixth attempt was allowed"
    );
    assert_eq!(
        harness.admission.attempts_for_source("counted"),
        AUTH_ATTEMPTS_PER_MINUTE + 1,
        "a refused attempt was not counted"
    );
    assert!(
        harness
            .admission
            .limit_auth_attempt("counted", "login", now)
            .is_err(),
        "an over-budget key fell back into a fresh window"
    );
}

/// The tunables are the numbers the brief sets, and the gate enforces the body one.
#[tokio::test(flavor = "multi_thread")]
async fn the_tunables_are_the_numbers_the_plan_sets_and_the_body_limit_is_enforced() {
    assert_eq!(MAX_BODY_BYTES, 1024 * 1024);
    assert_eq!(EXPENSIVE_ACTIVE_PER_ACCOUNT, 2);
    assert_eq!(EXPENSIVE_IN_FLIGHT_GLOBAL, 8);
    assert_eq!(EXPENSIVE_QUEUED_PER_ACCOUNT, 2);
    assert_eq!(HASH_JOBS_CONCURRENT, 2);
    assert_eq!(AUTH_ATTEMPTS_PER_MINUTE, 5);
    assert_eq!(AUTH_ATTEMPT_WINDOW, TimeDelta::minutes(1));

    let harness = Harness::hosted().await;
    assert_eq!(harness.gate.max_body_bytes(), MAX_BODY_BYTES);
    let accounts = harness.pair().await;
    let session = harness.handshake(&accounts.alice.bearer).await;

    // The in-limit control first: without it, "the oversized body was refused" is also satisfied by a gate
    // that refuses every body.
    let inside = "a".repeat(MAX_BODY_BYTES - 4096);
    let (status, _, _) = harness
        .send_and_read(with_bearer(
            with_session(
                harness.post(json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": "whoami", "arguments": {"blob": inside}},
                })),
                &session,
            ),
            &accounts.alice.bearer,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a body inside the limit was refused"
    );

    let over = "a".repeat(MAX_BODY_BYTES + 64);
    let (status, _, _) = harness
        .send_and_read(with_bearer(
            with_session(
                harness.post(json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": "whoami", "arguments": {"blob": over}},
                })),
                &session,
            ),
            &accounts.alice.bearer,
        ))
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

/// A grant nobody received gives its slot back (P1-1).
///
/// The leak this pins is narrow and permanent: capacity is charged when a grant is *sent*, while the
/// permit that returns it is built when the waiter's future *receives* it. A future cancelled in between —
/// a client that hung up at the wrong instant, a task aborted by its supervisor — leaves the account
/// holding a slot nobody is using, against a global bound of eight, and nothing recovers it but a restart.
///
/// The interleaving is forced rather than raced: on a single-threaded runtime the release, the grant it
/// triggers and the abort all happen inside one poll of this task, so the waiter cannot have resumed
/// between the send and the cancellation.
#[tokio::test]
async fn a_granted_but_unreceived_acquire_gives_its_slot_back() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.principal_of(&accounts.alice.bearer).await;

    let mut held = Vec::new();
    for _ in 0..EXPENSIVE_ACTIVE_PER_ACCOUNT {
        held.push(
            harness
                .admission
                .acquire(&alice, RequestClass::Expensive)
                .await
                .expect("alice's own slots"),
        );
    }
    let waiter = {
        let admission = harness.admission.clone();
        let principal = alice.clone();
        tokio::spawn(async move {
            admission
                .acquire(&principal, RequestClass::Expensive)
                .await
                .expect("the queued waiter is granted")
        })
    };
    harness.wait_admission_queued(1).await;
    assert_eq!(harness.admission.expensive_in_flight(), 3);

    // Release one slot: dispatch grants it, and the grant is sent — but the waiter is cancelled in the
    // same poll, before it can receive anything.
    let released = held.pop().expect("a held permit");
    drop(released);
    waiter.abort();
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(
        harness.admission.expensive_active(),
        EXPENSIVE_ACTIVE_PER_ACCOUNT - 1,
        "an unreceived grant kept charging the account's capacity"
    );
    assert_eq!(
        harness.admission.expensive_in_flight(),
        EXPENSIVE_ACTIVE_PER_ACCOUNT - 1,
        "an unreceived grant leaked a global slot"
    );

    // The capacity is really back: alice can take her second slot again, and a fresh waiter is granted.
    let again = harness
        .admission
        .acquire(&alice, RequestClass::Expensive)
        .await
        .expect("the account's capacity was not returned");
    assert_eq!(
        harness.admission.expensive_active(),
        EXPENSIVE_ACTIVE_PER_ACCOUNT
    );
    drop(again);
    drop(held);
}

/// Spawns parked waiters for one principal, each keeping its permit alive until the test says otherwise.
///
/// Returning the [`Permit`] rather than a boolean is what lets a caller read the scheduler *while* the
/// granted work is still running: a permit that dropped inside the task would make every "how many slots are
/// live" assertion describe nothing.
fn queue_two(admission: &Admission, principal: &Principal) -> Vec<tokio::task::JoinHandle<Permit>> {
    (0..EXPENSIVE_QUEUED_PER_ACCOUNT)
        .map(|_| {
            let admission = admission.clone();
            let principal = principal.clone();
            tokio::spawn(async move {
                admission
                    .acquire(&principal, RequestClass::Expensive)
                    .await
                    .expect("a queued waiter is eventually granted")
            })
        })
        .collect()
}

/// Takes a set of parked waiters in arrival order.
async fn join_all(waiters: Vec<tokio::task::JoinHandle<Permit>>) -> Vec<Permit> {
    let mut permits = Vec::new();
    for waiter in waiters {
        permits.push(waiter.await.expect("the waiter task ran"));
    }
    permits
}
