//! Task 5's transport gate, end to end: two real fixture accounts, real rmcp sessions and streams, and a
//! refusal at every point a credential stops entitling.
//!
//! Every case runs through the objects production uses — [`AuthMiddleware`](alexandria_mcp::auth::http::AuthMiddleware)
//! in front of rmcp's `StreamableHttpService`, over a `LocalSessionManager` the harness also holds — and
//! every identity is a row written by storage's enrollment and credential paths. Where a claim is about
//! *rmcp*'s behaviour (legacy sessions, stateless requests, reconnects, the standard-header control) it is
//! measured on the wire rather than inferred from a cloned factory.
//!
//! The set answers five questions:
//!
//! * **Is anything admitted without a credential?** [`no_credential_never_reaches_the_handler`] and
//!   [`a_browser_cookie_is_not_an_mcp_bearer_credential`].
//! * **Does a session belong to whoever opened it?** [`a_foreign_session_id_is_refused_on_post_get_and_delete`],
//!   [`deleting_a_session_makes_later_requests_fail`], [`an_open_stream_survives_a_reconnect_by_its_owner`].
//! * **Does revocation reach work that is already running?**
//!   [`revocation_ends_an_open_stream_and_delivers_no_frame_after_it`] and
//!   [`a_revoked_generation_cannot_register_a_live_lease`].
//! * **What does a refusal give away?** [`every_transport_denial_body_carries_no_internals_or_secrets`]
//!   (F4-3), with [`a_malformed_standard_header_is_distinguishable_from_an_auth_failure`] keeping a protocol
//!   refusal apart from an authority one.
//! * **Who decides when, and how much?** [`a_client_supplied_clock_moves_no_deadline`] (F4-11),
//!   [`an_oversized_body_is_refused_before_the_handler_runs],
//!   [`a_saturated_account_is_refused_before_the_handler_runs`], and on the password path
//!   [`an_invalid_login_is_refused_before_the_password_budget_is_touched`] (F4-15).

use std::sync::Arc;

use alexandria_mcp::auth::http::AuthMiddleware;
use alexandria_mcp::auth::{
    Denial, StdioIdentity, principal_from_extensions, principal_from_stdio_or_extensions,
};
use alexandria_mcp::error::ServiceError;
use alexandria_storage::access::PrincipalProvenance;
use axum::http::StatusCode;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::store::{
    SessionState, SessionStore, SessionStoreError,
};
use serde_json::json;
use std::sync::atomic::AtomicUsize;

mod fixtures;
use fixtures::*;

// ---------------------------------------------------------------------------------------------
// Admission: who may reach the handler at all
// ---------------------------------------------------------------------------------------------

/// The handshake and a tool call run under two real fixture accounts, and the principal that reaches the
/// handler is the one the token was verified from.
///
/// Both halves matter: a transport that authenticated correctly but forwarded somebody else's principal
/// would pass every refusal test in this file.
#[tokio::test(flavor = "multi_thread")]
async fn two_real_accounts_are_admitted_and_reach_the_handler_as_themselves() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;

    let alice = harness.handshake(&accounts.alice.bearer).await;
    assert_eq!(
        result_text(
            &harness
                .call_in_session(&accounts.alice.bearer, &alice, "whoami")
                .await
        ),
        vec![accounts.alice.account_id()],
        "alice's tool call did not reach the handler as alice"
    );

    let bob = harness.handshake(&accounts.bob.bearer).await;
    assert_eq!(
        result_text(
            &harness
                .call_in_session(&accounts.bob.bearer, &bob, "whoami")
                .await
        ),
        vec![accounts.bob.account_id()],
        "bob's tool call did not reach the handler as bob"
    );

    assert_ne!(alice, bob, "one session served both accounts");
    assert_eq!(harness.calls.count("whoami"), 2);
    assert_eq!(
        harness.registry.bound(),
        2,
        "the gate bound the wrong number of sessions"
    );
    assert_eq!(
        harness.calls.completed(),
        2,
        "a dispatched call never finished"
    );
}

/// No bearer, no handler.
///
/// The positive control runs first on purpose: without it, "the handler was not entered" would also be
/// satisfied by a fixture that cannot dispatch anything at all.
#[tokio::test(flavor = "multi_thread")]
async fn no_credential_never_reaches_the_handler() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;

    let session = harness.handshake(&accounts.alice.bearer).await;
    harness
        .call_in_session(&accounts.alice.bearer, &session, "whoami")
        .await;
    assert_eq!(
        harness.calls.count("whoami"),
        1,
        "the fixture cannot dispatch"
    );

    let (status, _, body) = harness.send_and_read(harness.post(initialize(41))).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an unauthenticated initialize was not refused: {}",
        String::from_utf8_lossy(&body)
    );

    // A live session id is not a credential.
    let (status, _, _) = harness
        .send_and_read(with_session(
            harness.post(tool_call(42, "whoami")),
            &session,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a session id is not a credential"
    );

    // A wrong scheme and an empty token are the same answer as no header.
    for (label, value) in [
        ("basic", "Basic Zm9vOmJhcg=="),
        ("bare", accounts.alice.bearer.as_str()),
        ("empty", "Bearer "),
    ] {
        let (status, _, _) = harness
            .send_and_read(with_header(
                harness.post(initialize(43)),
                "authorization",
                value,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "`{label}` was treated as a bearer presentation"
        );
    }
    assert_eq!(
        harness.calls.count("whoami"),
        1,
        "a refusal reached the handler"
    );
}

/// A browser session cookie is not an MCP bearer credential, in either direction.
///
/// The control half is the cookie's own path: it must still authenticate a browser session, or this test
/// would pass merely because the session had expired.
#[tokio::test(flavor = "multi_thread")]
async fn a_browser_cookie_is_not_an_mcp_bearer_credential() {
    let harness = Harness::hosted().await;
    harness.enroll("cookie-alice").await;
    let grant = harness
        .service
        .login("cookie-alice", PASSWORD, harness.now())
        .await
        .expect("a valid login mints a session");
    let cookie = grant.expose_cookie().to_string();

    harness
        .service
        .authenticate_browser(&cookie, harness.now())
        .await
        .expect("the cookie must still authenticate a browser session");

    let (status, _, body) = harness
        .send_and_read(with_bearer(harness.post(initialize(51)), &cookie))
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a browser cookie was accepted as an MCP bearer token: {}",
        String::from_utf8_lossy(&body)
    );
    let (status, _, _) = harness
        .send_and_read(with_cookie(harness.post(initialize(52)), &cookie))
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the gate read a cookie as a credential"
    );
    assert_eq!(
        harness.calls.count("whoami"),
        0,
        "a cookie reached the handler"
    );
}

// ---------------------------------------------------------------------------------------------
// Session ownership
// ---------------------------------------------------------------------------------------------

/// A foreign session id is refused on POST, on GET and on DELETE, and an unknown id gets the same answer.
///
/// All three verbs matter: rmcp answers an unknown session 404 on POST and GET and 400 on DELETE, so a
/// check that covered one verb would leave the others to the SDK. Alice's session is asserted to still
/// work afterwards, which is what stops "refused" from meaning "destroyed".
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_session_id_is_refused_on_post_get_and_delete() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.handshake(&accounts.alice.bearer).await;
    harness.handshake(&accounts.bob.bearer).await;

    for (label, request) in [
        (
            "post",
            with_bearer(
                with_session(harness.post(tool_call(61, "whoami")), &alice),
                &accounts.bob.bearer,
            ),
        ),
        (
            "get",
            with_bearer(harness.get_stream(&alice), &accounts.bob.bearer),
        ),
        (
            "delete",
            with_bearer(harness.delete(&alice), &accounts.bob.bearer),
        ),
    ] {
        let (status, _, body) = harness.send_and_read(request).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "bob acted on alice's session over {label}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    assert_eq!(
        result_text(
            &harness
                .call_in_session(&accounts.alice.bearer, &alice, "whoami")
                .await
        ),
        vec![accounts.alice.account_id()],
        "the refusals disturbed alice's session"
    );

    let (unknown, _, _) = harness
        .send_and_read(with_bearer(
            with_session(harness.post(tool_call(62, "whoami")), "not-a-session"),
            &accounts.alice.bearer,
        ))
        .await;
    assert_eq!(
        unknown,
        StatusCode::NOT_FOUND,
        "unknown and foreign differed"
    );
    assert!(
        harness.registry.is_bound(&alice),
        "alice's binding vanished"
    );
}

/// A client that deletes its own session cannot use it again, and the other account is unaffected.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_makes_later_requests_fail() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.handshake(&accounts.alice.bearer).await;

    let (status, _, _) = harness
        .send_and_read(with_bearer(harness.delete(&alice), &accounts.alice.bearer))
        .await;
    assert!(
        status.is_success(),
        "alice could not delete her own session: {status}"
    );
    assert!(!harness.registry.is_bound(&alice), "the binding survived");

    let (status, _, _) = harness
        .send_and_read(with_bearer(
            with_session(harness.post(tool_call(71, "whoami")), &alice),
            &accounts.alice.bearer,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a deleted session was usable"
    );

    let bob = harness.handshake(&accounts.bob.bearer).await;
    assert_eq!(
        result_text(
            &harness
                .call_in_session(&accounts.bob.bearer, &bob, "whoami")
                .await
        ),
        vec![accounts.bob.account_id()],
        "one account's delete disturbed another"
    );
}

/// The owner's SSE reconnect is accepted; a reconnect with somebody else's credential is not.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_stream_survives_a_reconnect_by_its_owner() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.handshake(&accounts.alice.bearer).await;
    assert!(
        harness.sdk_has_session(&alice).await,
        "rmcp has no session to reconnect"
    );

    let owner_reconnect = with_reconnect_marker(harness.get_stream(&alice));
    let response = harness
        .send(with_bearer(owner_reconnect, &accounts.alice.bearer))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the owner's own reconnect was refused"
    );
    // The stream is live, so the read must *not* complete: that is the reconnect actually being served.
    assert!(
        read_body_opt(response.into_body()).await.is_none(),
        "the owner's reconnected stream ended at once"
    );

    let foreign_reconnect = with_reconnect_marker(harness.get_stream(&alice));
    let (status, _, _) = harness
        .send_and_read(with_bearer(foreign_reconnect, &accounts.bob.bearer))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a foreign credential reconnected someone else's stream"
    );
}

/// The header the gate matches on is the one rmcp writes.
///
/// The gate spells `Mcp-Session-Id` as a literal because the pinned SDK does not export the constant; this
/// is what stops the literal from drifting away from the transport.
#[tokio::test(flavor = "multi_thread")]
async fn the_session_header_is_the_one_rmcp_writes() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let (_, headers, _) = harness
        .send_and_read(with_bearer(
            harness.post(initialize(81)),
            &accounts.alice.bearer,
        ))
        .await;
    let value = headers
        .get(alexandria_mcp::auth::SESSION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(
        !value.is_empty(),
        "rmcp did not answer on the header the gate reads: {headers:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Revocation, in flight
// ---------------------------------------------------------------------------------------------

/// The barrier case: a credential verified *before* a revocation cannot register after it.
///
/// The interleaving is driven deterministically at the same two seams the middleware uses —
/// `admit_presentation`, then registration — because that window is the one the design has to close. Two
/// things must hold afterwards: the registration is refused, and it left nothing live behind (no binding, no
/// uncancellable lease), which is what "no fresh uncancelled lease" means in this boundary.
#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_generation_cannot_register_a_live_lease() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;
    let lease = harness
        .lease(&accounts.alice.bearer)
        .await
        .expect("a live agent token admits");
    assert_eq!(harness.registry.live_leases(), 1);

    harness
        .revoke(
            &owner,
            lease
                .credential()
                .expect("a hosted lease names a credential"),
        )
        .await;

    assert!(lease.is_cancelled(), "the live lease was not cancelled");
    // What "nothing live was left behind" actually means here: the *cancellation* reached the lease, and
    // the registration is still counted because this handle is still held. Reading zero at this point
    // would be the double-subtracting counter (P2-1) reporting an empty bus to the one assertion whose
    // whole job is to notice a leak — see
    // `the_live_lease_counter_counts_registrations_not_cancellations`.
    assert_eq!(
        harness.registry.live_leases(),
        1,
        "the held handle stopped being counted, which is what made this oracle vacuous"
    );
    let error = harness
        .gate
        .register_session("late-session", &lease)
        .expect_err("a revoked generation registered a session");
    assert!(
        matches!(error, ServiceError::Stale),
        "registration refused with the wrong class: {error:?}"
    );
    assert!(
        !harness.registry.is_bound("late-session"),
        "the refused bind still left a binding behind"
    );

    // A fresh presentation of the same secret is refused at the door, not registered then cancelled.
    let error = harness
        .lease(&accounts.alice.bearer)
        .await
        .expect_err("a revoked token admitted again");
    assert!(matches!(error, ServiceError::Stale));

    // Released, *then* counted: this is the half that proves nothing uncancellable is registered
    // against a dead credential, and it can only be read honestly once the holder is gone.
    drop(lease);
    for _ in 0..60 {
        if harness.registry.live_leases() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        harness.registry.live_leases(),
        0,
        "a revoked credential still had a registration on the bus"
    );

    // And bob, untouched, still registers a live session.
    let bob = harness
        .lease(&accounts.bob.bearer)
        .await
        .expect("bob's credential must still admit");
    harness
        .gate
        .register_session("bobs-session", &bob)
        .expect("bob may register his own session");
    assert!(harness.registry.is_bound("bobs-session"));
}

/// A revoked token cannot make another request on a session it opened, and the handler is not entered.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_revoked_after_the_handshake_cannot_make_another_request() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.handshake(&accounts.alice.bearer).await;
    harness
        .call_in_session(&accounts.alice.bearer, &alice, "whoami")
        .await;
    let dispatched = harness.calls.count("whoami");
    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;

    harness.revoke(&owner, &accounts.alice.credential).await;

    let (status, _, _) = harness
        .send_and_read(with_bearer(
            with_session(harness.post(tool_call(91, "whoami")), &alice),
            &accounts.alice.bearer,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a revoked token kept being admitted"
    );
    assert_eq!(
        harness.calls.count("whoami"),
        dispatched,
        "the handler ran after revocation"
    );
    assert!(
        !harness.registry.is_bound(&alice),
        "the binding outlived the credential"
    );
    assert!(
        !harness.sdk_has_session(&alice).await,
        "rmcp kept the session open"
    );
}

/// An already-open stream ends when its credential is revoked, and delivers no frame after it.
///
/// A mutex-and-flag registry would satisfy the previous case and prove nothing here, so this drives a real
/// held stream: a tool call is in flight, its response is an event stream, and the revocation lands while
/// the fixture handler is still parked inside its own gate. The positive control — the same call, nothing
/// revoked — is asserted last, so "no frame arrived" cannot pass because the fixture never answers.
#[tokio::test(flavor = "multi_thread")]
async fn revocation_ends_an_open_stream_and_delivers_no_frame_after_it() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.handshake(&accounts.alice.bearer).await;
    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;

    let held = harness.spawn_call(&accounts.alice, &alice, "hold");
    harness.calls.wait_until_entered("hold", 1).await;

    harness.revoke(&owner, &accounts.alice.credential).await;
    let (status, frames) = tokio::time::timeout(STREAM_WAIT * 4, held)
        .await
        .expect("the held stream ended after the revocation")
        .expect("the held request ran");
    assert!(
        status == StatusCode::UNAUTHORIZED || frames.is_empty(),
        "the stream was answered as if nothing had happened: {status} {frames:?}"
    );
    assert!(
        !frames.iter().any(|frame| frame.get("result").is_some()),
        "a result frame arrived after the revocation: {frames:?}"
    );

    // Stopping the delivery is not the same as stopping the work, and once Task 6 puts writes in a
    // handler the difference is the whole property. Synchronise on the handler having *taken* the
    // cancellation branch before touching the gate: once `release` runs, both arms of the handler's
    // `tokio::select!` are ready and the choice is random, so a handler that had not been polled yet
    // could still finish and advance `completed` — which would turn the assertion below into a coin flip
    // instead of a proof. A recorded `hold-cancelled` means the cancel arm already won.
    //
    // Releasing alice's gate afterwards is still the point: a handler still alive finishes and advances
    // `completed`; one that rmcp cancelled cannot.
    harness.calls.wait_until_entered("hold-cancelled", 1).await;
    let completed_after_revocation = harness.calls.completed();
    harness.calls.release(&accounts.alice.account_id());
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        harness.calls.completed(),
        completed_after_revocation,
        "the handler finished after the revocation: cancellation stopped the delivery but not the work \
         it was gating"
    );
    assert!(
        harness
            .calls
            .accounts_for("hold-cancelled")
            .contains(&accounts.alice.account_id()),
        "the handler never noticed the cancellation: {:?}",
        harness.calls.accounts_for("hold-cancelled")
    );

    // Positive control, on a second account: the same held call does deliver its result.
    let carol = harness.agent("carol-the-third", "carol-laptop").await;
    let session = harness.handshake(&carol.bearer).await;
    let ok = harness.spawn_call(&carol, &session, "hold");
    harness.calls.wait_until_entered("hold", 2).await;
    harness.calls.release(&carol.account_id());
    let (_, frames) = ok.await.expect("the control call ran");
    assert!(
        frames.iter().any(|frame| frame.get("result").is_some()),
        "the fixture never delivers a result even when nothing is revoked: {frames:?}"
    );
}

/// A revoked credential that was *queued* is refused at dispatch rather than handed the freed slot.
///
/// The bound is the transport's, so this is the case where the revocation lands while the request waits
/// behind somebody else's work: bob's queued call must still run, and alice's must never enter the handler.
#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_request_is_not_dispatched_from_the_queue() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.handshake(&accounts.alice.bearer).await;
    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;

    let first = harness.spawn_call(&accounts.alice, &alice, "hold");
    let second = harness.spawn_call(&accounts.alice, &alice, "hold");
    harness.calls.wait_until_entered("hold", 2).await;
    let queued = harness.spawn_call(&accounts.alice, &alice, "whoami");
    harness.wait_admission_queued(1).await;

    harness.revoke(&owner, &accounts.alice.credential).await;
    harness.calls.release(&accounts.alice.account_id());
    harness.calls.release(&accounts.alice.account_id());
    let _ = tokio::join!(first, second, queued);

    assert_eq!(
        harness.calls.count("whoami"),
        0,
        "a revoked alice request was dispatched from the queue"
    );
    // Task 5's named carry, discharged. A permit is released on the server-side task when its last
    // handle dies, not inside the handler, so reading the counters synchronously right after the
    // client-side `join!` raced that release and failed intermittently under load. The settle is
    // bounded and the assertion is unchanged in strength: capacity must actually return, and a real
    // leak never settles. What this proves is "the slot comes back", *not* "the slot comes back
    // synchronously with the response" — the latter was never true and must not be claimed.
    harness.wait_admission_idle().await;
    assert_eq!(harness.admission.expensive_active(), 0, "a slot leaked");
    assert_eq!(
        harness.admission.expensive_queued(),
        0,
        "a queue entry leaked"
    );
}

// ---------------------------------------------------------------------------------------------
// The stateless path, and the protocol controls beside it
// ---------------------------------------------------------------------------------------------

/// A stateless request is admitted on its own credential, carries its principal, and creates no session.
#[tokio::test(flavor = "multi_thread")]
async fn a_stateless_request_is_admitted_refused_and_carries_its_principal() {
    let harness = Harness::stateless().await;
    let accounts = harness.pair().await;

    let (status, _, body) = harness
        .send_and_read(with_standard_headers(
            harness.post(stateless_tool_call(101, "whoami")),
            Some("tools/call"),
            Some("whoami"),
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a stateless call with no credential was served: {}",
        String::from_utf8_lossy(&body)
    );

    let (status, _, body) = harness
        .send_and_read(with_standard_headers(
            with_bearer(
                harness.post(stateless_tool_call(102, "whoami")),
                &accounts.alice.bearer,
            ),
            Some("tools/call"),
            Some("whoami"),
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a stateless call with a credential was refused: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        result_text(&frames_of(&body)),
        vec![accounts.alice.account_id()],
        "the stateless handler did not see its own account"
    );
    assert_eq!(
        harness.registry.bound(),
        0,
        "a stateless call created a session"
    );
}

/// A contradictory standard header is a protocol refusal, and it is not the same answer as an auth refusal.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_standard_header_is_distinguishable_from_an_auth_failure() {
    let harness = Harness::stateless().await;
    let accounts = harness.pair().await;

    let (malformed, _, malformed_body) = harness
        .send_and_read(with_standard_headers(
            with_bearer(
                harness.post(stateless_tool_call(111, "whoami")),
                &accounts.alice.bearer,
            ),
            Some("tools/list"),
            Some("whoami"),
        ))
        .await;
    let (unauthenticated, _, unauthenticated_body) = harness
        .send_and_read(with_standard_headers(
            harness.post(stateless_tool_call(112, "whoami")),
            Some("tools/call"),
            Some("whoami"),
        ))
        .await;
    assert_eq!(
        malformed,
        StatusCode::BAD_REQUEST,
        "the SDK did not answer the contradictory header: {}",
        String::from_utf8_lossy(&malformed_body)
    );
    assert_eq!(
        unauthenticated,
        StatusCode::UNAUTHORIZED,
        "an absent credential was not an auth refusal: {}",
        String::from_utf8_lossy(&unauthenticated_body)
    );
    assert_eq!(
        harness.calls.count("whoami"),
        0,
        "either refusal reached the handler"
    );
}

// ---------------------------------------------------------------------------------------------
// What a refusal gives away (F4-3, the transport half)
// ---------------------------------------------------------------------------------------------

/// Every denial this transport can produce, read out of the actual response, carries no internals and no
/// secret.
///
/// Task 4 proved redaction at the service boundary; this is the same scan moved to where a client can see:
/// status plus body bytes from the live handlers, across the auth, session, size and budget paths. The
/// secrets are the real ones from the fixture — tokens, a cookie, a CSRF half, the password, and the digest
/// and ids read back out of the rows — so a leak cannot hide behind a placeholder. The final assertion is
/// the anti-vacuity control: the same account id *does* appear in a body that was admitted.
#[tokio::test(flavor = "multi_thread")]
async fn every_transport_denial_body_carries_no_internals_or_secrets() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.handshake(&accounts.alice.bearer).await;
    let bob_grant = harness
        .service
        .login("task5-bob", PASSWORD, harness.now())
        .await
        .expect("bob can log in again");
    let cookie = bob_grant.expose_cookie().to_string();
    let csrf = bob_grant.expose_csrf().to_string();
    let row = harness.row(&accounts.alice.credential).await;
    let digest = row["secret_digest"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !digest.is_empty(),
        "the fixture row carried no digest to scan for"
    );
    let account_id = accounts.alice.account_id();
    let credential_id = id_of_credential(&accounts.alice.credential);

    let mut denials: Vec<(StatusCode, String)> = vec![
        harness.send_and_read_text(harness.post(initialize(201))).await,
        harness
            .send_and_read_text(with_bearer(harness.post(initialize(202)), "not-a-token"))
            .await,
        harness
            .send_and_read_text(with_bearer(harness.post(initialize(203)), &cookie))
            .await,
        harness
            .send_and_read_text(with_bearer(harness.post(initialize(204)), &csrf))
            .await,
        harness
            .send_and_read_text(with_session(
                with_bearer(harness.post(tool_call(205, "whoami")), &accounts.bob.bearer),
                &alice,
            ))
            .await,
        harness
            .send_and_read_text(with_bearer(
                with_session(harness.post(tool_call(206, "whoami")), "no-such-session"),
                &accounts.alice.bearer,
            ))
            .await,
        harness
            .send_and_read_text(with_bearer(harness.get_stream("no-such-session"), &accounts.alice.bearer))
            .await,
        harness
            .send_and_read_text(with_bearer(harness.delete("no-such-session"), &accounts.alice.bearer))
            .await,
        // A declared body over the limit.
        harness
            .send_and_read_text(with_bearer(
                with_header(
                    harness.post(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"whoami","arguments":{}}}))
                    ,
                    "content-length",
                    &format!("{}", MAX_BODY_BYTES + 1024),
                ),
                &accounts.alice.bearer,
            ))
            .await,
    ];
    // An enrollment denial and a login denial, from the paths that own them.
    denials.push(render(
        enroll_over_http(&harness, "no-such-link", "a-valid-login", PASSWORD).await,
    ));
    denials.push(render(
        harness.gate.login(UNPROXIED_SOURCE, "x", PASSWORD).await,
    ));
    // A budget denial: alice's two active slots are held and her two queue places taken, so the next call
    // is refused rather than parked.
    let first = harness.spawn_call(&accounts.alice, &alice, "hold");
    let second = harness.spawn_call(&accounts.alice, &alice, "hold");
    harness.calls.wait_until_entered("hold", 2).await;
    let parked_first = harness.spawn_call(&accounts.alice, &alice, "hold");
    let parked_second = harness.spawn_call(&accounts.alice, &alice, "hold");
    harness.wait_admission_queued(2).await;
    denials.push(
        harness
            .send_and_read_text(with_bearer(
                with_session(harness.post(tool_call(207, "whoami")), &alice),
                &accounts.alice.bearer,
            ))
            .await,
    );
    for _ in 0..4 {
        harness.calls.release(&accounts.alice.account_id());
    }
    let _ = tokio::join!(first, second, parked_first, parked_second);

    assert!(denials.len() >= 12, "the denial set was not built");
    for (status, body) in &denials {
        assert!(
            status.is_client_error() || status.is_server_error(),
            "a denial that was not a refusal: {status} {body}"
        );
        let secrets = [
            PASSWORD.to_string(),
            ROTATED.to_string(),
            accounts.alice.bearer.clone(),
            accounts.bob.bearer.clone(),
            cookie.clone(),
            csrf.clone(),
            digest.clone(),
        ];
        for secret in &secrets {
            assert!(
                !secret.is_empty() && !body.contains(secret),
                "a {status} body carried credential material: {body}"
            );
        }
        for marker in [
            "SELECT",
            "UPDATE ",
            "CREATE ",
            "DEFINE ",
            "WHERE",
            "secret_digest",
            "password_hash",
            "$argon2id",
            "Caused by",
            "credential:",
            "account:",
            &account_id,
            &credential_id,
        ] {
            assert!(
                !body.contains(marker),
                "a {status} body carried {marker:?}: {body}"
            );
        }
    }

    let admitted = harness
        .call_in_session(&accounts.alice.bearer, &alice, "whoami")
        .await;
    assert!(
        result_text(&admitted).contains(&account_id),
        "the anti-vacuity control did not carry the id the scan looks for: {admitted:?}"
    );
}

/// The status classes are distinct where distinctness is the point.
///
/// A malformed header, an oversized body, a missing credential, a foreign session and a saturated account
/// must not collapse into one code: each is a different instruction to the caller, and one of them (400 vs
/// 401) is the control the brief asks to keep apart.
#[tokio::test(flavor = "multi_thread")]
async fn the_refusal_classes_keep_their_own_status_codes() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.handshake(&accounts.alice.bearer).await;
    harness.handshake(&accounts.bob.bearer).await;
    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;

    let missing = harness.send_and_read(harness.post(initialize(301))).await.0;
    let foreign = harness
        .send_and_read(with_bearer(
            with_session(harness.post(tool_call(302, "whoami")), &alice),
            &accounts.bob.bearer,
        ))
        .await
        .0;
    let oversized = harness
        .send_and_read(with_bearer(
            with_header(
                harness.post(tool_call(303, "whoami")),
                "content-length",
                &format!("{}", MAX_BODY_BYTES + 1),
            ),
            &accounts.alice.bearer,
        ))
        .await
        .0;
    let first = harness.spawn_call(&accounts.alice, &alice, "hold");
    let second = harness.spawn_call(&accounts.alice, &alice, "hold");
    harness.calls.wait_until_entered("hold", 2).await;
    let queued_third = harness.spawn_call(&accounts.alice, &alice, "hold");
    harness.wait_admission_queued(1).await;
    let queued_fourth = harness.spawn_call(&accounts.alice, &alice, "hold");
    harness.wait_admission_queued(2).await;
    let saturated = harness
        .send_and_read(with_bearer(
            with_session(harness.post(tool_call(304, "whoami")), &alice),
            &accounts.alice.bearer,
        ))
        .await
        .0;
    harness.calls.release(&accounts.alice.account_id());
    harness.calls.release(&accounts.alice.account_id());
    harness.calls.release(&accounts.alice.account_id());
    harness.calls.release(&accounts.alice.account_id());
    let _ = tokio::join!(first, second, queued_third, queued_fourth);

    harness.revoke(&owner, &accounts.alice.credential).await;
    let revoked = harness
        .send_and_read(with_bearer(
            with_session(harness.post(tool_call(305, "whoami")), &alice),
            &accounts.alice.bearer,
        ))
        .await
        .0;

    assert_eq!(missing, StatusCode::UNAUTHORIZED);
    assert_eq!(foreign, StatusCode::NOT_FOUND);
    assert_eq!(oversized, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(saturated, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(revoked, StatusCode::UNAUTHORIZED);
    let codes = [missing, foreign, oversized, saturated];
    assert_eq!(
        codes.iter().collect::<std::collections::HashSet<_>>().len(),
        4,
        "refusal classes collapsed onto one status"
    );
}

// ---------------------------------------------------------------------------------------------
// The clock, the body limit, the queue (F4-11, admission)
// ---------------------------------------------------------------------------------------------

/// The server's clock is the only one that moves a deadline.
///
/// Two client-side attempts to move it — a `Date` header and a body field named `now` — both hear the same
/// refusal as the bare request, and the positive control proves the other credential was live at the very
/// same instant. A transport that believed a client date would turn one of the two into a success.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_supplied_clock_moves_no_deadline() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.handshake(&accounts.alice.bearer).await;
    harness
        .set_credential(
            &accounts.alice.credential,
            "expires_at = <datetime>$past",
            vec![(
                "past",
                json!((harness.now() - chrono::Duration::seconds(60)).to_rfc3339()),
            )],
        )
        .await;

    let mut with_date = with_bearer(
        with_session(harness.post(tool_call(301, "whoami")), &alice),
        &accounts.alice.bearer,
    );
    with_date.headers_mut().insert(
        "date",
        "2000-01-01T00:00:00Z".parse().expect("a date header"),
    );
    let (status, _, _) = harness.send_and_read(with_date).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a `Date` header moved the deadline"
    );

    let (status, body) = harness
        .send_and_read_text(with_bearer(
            harness.post(json!({
                "jsonrpc": "2.0", "id": 302, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "task5-fixture-client", "version": "0.0.0"},
                    "now": "2000-01-01T00:00:00Z",
                },
                "now": "2000-01-01T00:00:00Z",
            })),
            &accounts.alice.bearer,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a body field named the clock: {body}"
    );

    let bob = harness.handshake(&accounts.bob.bearer).await;
    assert_eq!(
        result_text(
            &harness
                .call_in_session(&accounts.bob.bearer, &bob, "whoami")
                .await
        ),
        vec![accounts.bob.account_id()],
        "the control credential was not live at the same instant"
    );
}

/// A body over the limit is refused, before the handler sees anything.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_body_is_refused_before_the_handler_runs() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;

    // Control first: an in-limit call is dispatched.
    let session = harness.handshake(&accounts.alice.bearer).await;
    harness
        .call_in_session(&accounts.alice.bearer, &session, "whoami")
        .await;
    assert_eq!(
        harness.calls.count("whoami"),
        1,
        "the in-limit control was not dispatched"
    );

    let big = "a".repeat(MAX_BODY_BYTES + 16);
    let (status, _, _) = harness
        .send_and_read(with_bearer(
            harness.post(json!({
                "jsonrpc": "2.0", "id": 401, "method": "tools/call",
                "params": {"name": "whoami", "arguments": {"blob": big}},
            })),
            &accounts.alice.bearer,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "an oversized body was not refused"
    );
    assert_eq!(
        harness.calls.count("whoami"),
        1,
        "the oversized call reached the handler"
    );
    assert_eq!(harness.gate.max_body_bytes(), MAX_BODY_BYTES);
}

/// A hot account cannot take every slot, and its excess is refused before the work.
///
/// Alice holds both of her expensive slots and queues two more; bob's call still runs and alice's next is
/// answered 429 with no handler entry. Releasing her slots then lets the same call through, which is what
/// shows the bound moved rather than the account being blocked.
#[tokio::test(flavor = "multi_thread")]
async fn a_saturated_account_is_refused_before_the_handler_runs() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.handshake(&accounts.alice.bearer).await;
    let bob = harness.handshake(&accounts.bob.bearer).await;

    let first = harness.spawn_call(&accounts.alice, &alice, "hold");
    let second = harness.spawn_call(&accounts.alice, &alice, "hold");
    harness.calls.wait_until_entered("hold", 2).await;
    let queued_first = harness.spawn_call(&accounts.alice, &alice, "hold");
    let queued_second = harness.spawn_call(&accounts.alice, &alice, "hold");
    harness.wait_admission_queued(2).await;

    // Bob is served while alice is queued: the bound is per account, not a global traffic jam.
    assert_eq!(
        result_text(
            &harness
                .call_in_session(&accounts.bob.bearer, &bob, "whoami")
                .await
        ),
        vec![accounts.bob.account_id()],
        "a second account starved behind a hot one"
    );

    let (status, _, _) = harness
        .send_and_read(with_bearer(
            with_session(harness.post(tool_call(501, "whoami")), &alice),
            &accounts.alice.bearer,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "alice's excess was not refused"
    );
    assert_eq!(
        harness.calls.count("whoami"),
        1,
        "a refused call reached the handler"
    );

    for _ in 0..4 {
        harness.calls.release(&accounts.alice.account_id());
    }
    let _ = tokio::join!(first, second, queued_first, queued_second);
    assert_eq!(
        result_text(
            &harness
                .call_in_session(&accounts.alice.bearer, &alice, "whoami")
                .await
        ),
        vec![accounts.alice.account_id()],
        "alice was still refused after her slots drained"
    );
    assert_eq!(harness.admission.expensive_active(), 0, "slots leaked");
}

/// An expensive slot covers the handler, not the response head.
///
/// Without this, "two active expensive requests per account" would describe how long it takes to write
/// response headers rather than how long work is running.
#[tokio::test(flavor = "multi_thread")]
async fn an_expensive_slot_covers_the_handler_that_is_running() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let alice = harness.handshake(&accounts.alice.bearer).await;
    let first = harness.spawn_call(&accounts.alice, &alice, "hold");
    let second = harness.spawn_call(&accounts.alice, &alice, "hold");
    harness.calls.wait_until_entered("hold", 2).await;
    assert_eq!(
        harness.admission.expensive_active(),
        2,
        "the slot did not outlive the head"
    );
    harness.calls.release(&accounts.alice.account_id());
    harness.calls.release(&accounts.alice.account_id());
    let _ = tokio::join!(first, second);
    for _ in 0..200 {
        if harness.admission.expensive_active() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(harness.admission.expensive_active(), 0, "a slot leaked");
}

// ---------------------------------------------------------------------------------------------
// Enrollment gating (F4-4) and the login gate (F4-15)
// ---------------------------------------------------------------------------------------------

/// A network enrollment needs a link, and no transport path bootstraps an owner directly.
///
/// Three things are asserted together: a made-up secret enrolls nobody; a *live agent token* — a real
/// credential of a real account — is not an enrollment secret either; and the only account that appears is
/// the one a minted invitation named, exactly once. The account count is the measurement, because
/// `bootstrap_owner` is storage's local-startup entry and no route here can reach it.
#[tokio::test(flavor = "multi_thread")]
async fn network_enrollment_requires_a_link_and_never_bootstraps_directly() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let before = harness.account_count().await;

    for (index, (label, secret)) in [
        ("a made-up secret", "0".repeat(64).as_str()),
        ("someone's agent token", accounts.alice.bearer.as_str()),
        ("an empty secret", ""),
        ("an expired-shaped guess", "x".repeat(8).as_str()),
    ]
    .into_iter()
    .enumerate()
    {
        // One peer per probe: this case is about the link, and the attempt budget has its own test.
        // One peer per probe, and a distinct login per probe: the attempt budget has its own test, and
        // this one is about the link.
        let outcome = harness
            .gate
            .enroll(
                &format!("probe-peer-{index}"),
                secret,
                &format!("probe-{index}-login", index = index),
                ROTATED,
            )
            .await;
        assert!(
            matches!(
                outcome,
                Err(ServiceError::Stale) | Err(ServiceError::NotFound)
            ),
            "{label} was accepted as an enrollment credential: {outcome:?}"
        );
    }
    assert_eq!(
        harness.account_count().await,
        before,
        "a refused enrollment wrote an account"
    );

    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;
    let link = harness
        .service
        .create_invite(&owner, harness.now())
        .await
        .expect("an owner may mint an invitation");
    let ids = harness
        .gate
        .enroll(UNPROXIED_SOURCE, link.expose(), "invited-owner", ROTATED)
        .await
        .expect("a valid invitation enrolls");
    assert_eq!(harness.account_count().await, before + 1);
    assert_eq!(
        harness.accounts_named().await,
        sorted(&["invited-owner", "task5-alice", "task5-bob",])
    );
    assert!(
        harness.account_by_login_exists("invited-owner").await,
        "the enrollment receipt named an account that is not in the store: {ids:?}"
    );

    let second = harness
        .gate
        .enroll(
            "second-attempt-peer",
            link.expose(),
            "second-owner",
            ROTATED,
        )
        .await;
    assert!(
        matches!(second, Err(ServiceError::Stale)),
        "one link enrolled two owners: {second:?}"
    );
    assert_eq!(harness.account_count().await, before + 1);
}

/// An invalid login is refused before any password work, and the digest bound is real.
///
/// The two hash permits are taken and held, so anything intending to hash must *block*. A valid, unspent
/// invitation link presented with a login the schema could never store must come back as a refusal
/// immediately — that is F4-15, whose gap was that the digest was paid before the login was checked, and a
/// refused login does not spend the link. The negative control, same exhausted budget with a storable login,
/// must block instead: that is what proves the budget was armed at all rather than the request simply being
/// fast.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_login_is_refused_before_the_password_budget_is_touched() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;
    let link = harness
        .service
        .create_invite(&owner, harness.now())
        .await
        .expect("an owner may mint an invitation");

    let held = vec![
        harness.admission.hash_permit().await.expect("hash slot 1"),
        harness.admission.hash_permit().await.expect("hash slot 2"),
    ];
    assert_eq!(harness.admission.hash_running(), 2);

    for label in ["x", "   ", &"a".repeat(65)] {
        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            harness
                .gate
                .enroll(UNPROXIED_SOURCE, link.expose(), label, ROTATED),
        )
        .await;
        let outcome = outcome.expect("an invalid login must not wait for the password budget");
        assert!(
            matches!(outcome, Err(ServiceError::InvalidInput(_))),
            "the login gate did not refuse `{label}`: {outcome:?}"
        );
    }
    assert_eq!(
        harness.admission.hash_running(),
        2,
        "a refused login consumed the digest budget"
    );

    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        harness
            .gate
            .enroll(UNPROXIED_SOURCE, link.expose(), "a-possible-login", ROTATED),
    )
    .await;
    assert!(
        blocked.is_err(),
        "a request that intended to hash did not wait for the budget"
    );
    drop(held);
    let enrolled = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        harness
            .gate
            .enroll(UNPROXIED_SOURCE, link.expose(), "a-possible-login", ROTATED),
    )
    .await
    .expect("the released request completes")
    .expect("the released enrollment succeeds");
    assert!(harness.account_by_login_exists("a-possible-login").await);
    let _ = enrolled;
}

/// Never more than two digest jobs run at once.
///
/// A monitor samples the in-use count while every link is being claimed in parallel, rather than taking a
/// permit itself: a probe that held a slot while `enroll` took its own would deadlock against the bound it
/// is meant to measure, and a deadlock is not evidence about the bound.
#[tokio::test(flavor = "multi_thread")]
async fn no_more_than_two_digest_jobs_run_at_once() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;
    // The bearer texts are taken out here: a minted secret lives in a `OneTimeSecret`, and the probe needs
    // only the text across a spawn boundary.
    let mut secrets = Vec::new();
    for index in 0..4 {
        let link = harness
            .service
            .create_invite(&owner, harness.now())
            .await
            .unwrap_or_else(|error| panic!("minting invite {index} failed: {error:?}"));
        secrets.push(link.expose().to_string());
    }

    let peak = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&peak);
    let admission = harness.admission.clone();
    let monitor = tokio::spawn(async move {
        for _ in 0..4000 {
            observed.fetch_max(
                admission.hash_running(),
                std::sync::atomic::Ordering::AcqRel,
            );
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    });

    let mut jobs = Vec::new();
    for (index, secret) in secrets.into_iter().enumerate() {
        let gate = harness.gate.clone();
        jobs.push(tokio::spawn(async move {
            gate.enroll(
                UNPROXIED_SOURCE,
                &secret,
                &format!("digest-probe-{index}"),
                ROTATED,
            )
            .await
            .expect("a valid link with a valid login enrolls");
        }));
    }
    for job in jobs {
        job.await.expect("the enrollment task ran");
    }
    monitor.abort();
    let peak = peak.load(std::sync::atomic::Ordering::Acquire);
    assert!(
        peak <= 2,
        "the digest bound was exceeded: {peak} jobs at once"
    );
    assert_eq!(
        harness.admission.hash_running(),
        0,
        "a digest permit leaked"
    );
}

/// The login gate and storage's own validator agree, input by input.
///
/// The bounds are restated in the MCP crate because storage's validator is crate-private, and a restated
/// rule is exactly the kind of thing that drifts. Every input is asked of both, and the answers must match
/// in kind and, where accepted, byte for byte.
#[tokio::test(flavor = "multi_thread")]
async fn the_login_gate_agrees_with_storages_own_validator() {
    let harness = Harness::hosted().await;
    for input in [
        "alice",
        "  Alice  ",
        "ab",
        &"a".repeat(64),
        &"a".repeat(65),
        "",
        "   ",
        "Already-Lower",
        "ünïcode-login",
    ] {
        let gate = gate_login(input);
        let stored = harness.store_accepts_login(input).await;
        match (gate, stored) {
            (Ok(normalized), Ok(what_the_store_would_use)) => assert_eq!(
                normalized, what_the_store_would_use,
                "the two normalised `{input}` differently"
            ),
            (Err(_), Err(_)) => {}
            (gate, stored) => panic!(
                "the login gates disagree on `{input}`: gate {:?}, store {:?}",
                gate.as_deref(),
                stored.as_deref()
            ),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Characterization (F4-12) and the commit-time recheck
// ---------------------------------------------------------------------------------------------

/// Presenting a credential writes its row, every time — the cost of the design, recorded as behaviour.
///
/// Authentication is two reads plus one write, so an idle window cannot be replayed and a use is recorded.
/// The brief lets Task 5 expose that shape but not change it without a measured design that keeps both
/// expiry windows, so the shape is pinned here: an admission rolls the deadline and stamps the use. A later
/// task that caches or elides the write must fail this test and say why it may.
#[tokio::test(flavor = "multi_thread")]
async fn every_presentation_writes_the_credential_row() {
    let harness = Harness::hosted().await;
    harness.enroll("write-on-presentation").await;
    let grant = harness
        .service
        .login("write-on-presentation", PASSWORD, harness.now())
        .await
        .expect("a valid login mints a session");
    let cookie = grant.expose_cookie().to_string();
    let before = harness.row(grant.credential()).await;
    let deadline = before["expires_at"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!deadline.is_empty(), "the row carried no idle deadline");
    assert_eq!(
        before["last_used_at"],
        serde_json::Value::Null,
        "a fresh session was already stamped as used: {before}"
    );

    harness
        .service
        .authenticate_browser(&cookie, harness.now())
        .await
        .expect("the session authenticates");
    let after = harness.row(grant.credential()).await;
    assert_ne!(
        after["expires_at"].as_str().unwrap_or_default(),
        deadline.as_str(),
        "presenting the session did not roll its idle deadline — the F4-12 characterization no longer \
         describes the code"
    );
    assert!(
        after["last_used_at"] != serde_json::Value::Null,
        "a presentation recorded no use: {after}"
    );
}

/// The commit-time recheck re-reads account status, the authentication epoch, and the credential itself.
///
/// Three separate ways for authority to move under a live lease are exercised — revocation, suspension, a
/// recovery's epoch bump — and each must be refused by the recheck, while the same recheck on a live lease
/// succeeds. What the recheck is *not* is atomic with the write: on the paths that exist today the read and
/// the write are separate statements, so a revocation landing between them is felt by the next
/// authorization and by the cancellation gate, not by the row already committed. The transaction-scoped
/// form is Task 6's scoped writers, which are the first callers that hold a transaction at all — that is
/// stated here rather than papered over.
#[tokio::test(flavor = "multi_thread")]
async fn the_commit_gate_feels_revocation_suspension_and_an_epoch_bump() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;

    let live = harness
        .lease(&accounts.alice.bearer)
        .await
        .expect("a live token admits");
    harness
        .revalidate(&live)
        .await
        .expect("a live lease revalidates");

    // 1. Revocation of the credential itself.
    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;
    harness
        .revoke(&owner, live.credential().expect("hosted"))
        .await;
    let error = harness
        .revalidate(&live)
        .await
        .expect_err("a revoked lease revalidated");
    assert!(
        matches!(error, ServiceError::Stale),
        "revocation: {error:?}"
    );

    // 2. Suspension of the account, felt by a different live credential of the same owner.
    let bob = harness
        .lease(&accounts.bob.bearer)
        .await
        .expect("bob admits");
    harness
        .set_account(
            &accounts.bob.ids.account,
            "status = 'suspended'",
            Vec::new(),
        )
        .await;
    let error = harness
        .revalidate(&bob)
        .await
        .expect_err("a suspended account's lease revalidated");
    assert!(
        matches!(error, ServiceError::Stale),
        "suspension: {error:?}"
    );

    // 3. An authentication-epoch bump, which retires every credential without touching any of their rows.
    let carol = harness.agent("carol-the-fourth", "carol-laptop").await;
    let carol_lease = harness.lease(&carol.bearer).await.expect("carol admits");
    harness
        .revalidate(&carol_lease)
        .await
        .expect("carol's lease revalidates before the bump");
    harness
        .set_account(
            &carol.ids.account,
            "auth_epoch = auth_epoch + 1",
            Vec::new(),
        )
        .await;
    let error = harness
        .revalidate(&carol_lease)
        .await
        .expect_err("a superseded epoch still revalidated");
    assert!(
        matches!(error, ServiceError::Stale),
        "epoch bump: {error:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Stdio, and the shape of the identity a handler may see
// ---------------------------------------------------------------------------------------------

/// A request context that carries no admitted principal is refused, with or without a configured stdio
/// identity in the process.
#[tokio::test]
async fn an_absent_identity_is_refused_and_absence_is_never_local() {
    let bare = rmcp::model::Extensions::default();
    let error = principal_from_extensions(&bare).expect_err("a bare context carried an identity");
    assert!(
        matches!(error, ServiceError::Stale),
        "the refusal class: {error:?}"
    );
    let error = principal_from_stdio_or_extensions(None, &bare)
        .expect_err("absence was read as the local operator");
    assert!(matches!(error, ServiceError::Stale));
}

/// A stdio identity a startup issued is available to the transport that was configured with it, and a
/// hosted request never inherits it.
#[tokio::test(flavor = "multi_thread")]
async fn a_configured_stdio_identity_is_carried_by_the_gate_and_not_by_a_hosted_request() {
    let harness = Harness::hosted().await;
    let identity: StdioIdentity = harness.stdio_identity("task5-alice").await;
    assert!(
        matches!(
            identity.principal().provenance(),
            PrincipalProvenance::TrustedLocal
        ),
        "the configured identity is not the trusted-local one"
    );

    // A hosted request that presented nothing is still refused, even with the identity configured.
    let harness = harness.with_stdio_identity(identity.clone());
    let (status, _, _) = harness.send_and_read(harness.post(initialize(601))).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a hosted request inherited the stdio identity"
    );
    assert_eq!(harness.calls.count("whoami"), 0);

    // The gate carries it, and an admitted hosted lease is still the hosted one.
    assert_eq!(
        harness
            .gate
            .stdio_identity()
            .map(|held| held.principal().account().clone()),
        Some(identity.principal().account().clone()),
        "the gate did not carry the configured identity"
    );
    let accounts = harness.pair().await;
    let lease = harness
        .lease(&accounts.alice.bearer)
        .await
        .expect("hosted admits");
    assert!(matches!(
        lease.principal().provenance(),
        PrincipalProvenance::Credential(_)
    ));
}

/// Cross-instance session restore stays off while the ownership binding lives only in this process.
#[test]
fn a_session_store_is_refused_until_the_binding_can_travel_with_it() {
    let plain = StreamableHttpServerConfig::default();
    AuthMiddleware::refuses_unbound_session_restore(&plain)
        .expect("the default config, with no external store, is acceptable");

    let mut configured = StreamableHttpServerConfig::default();
    configured.session_store = Some(Arc::new(InMemorySessionStore::default()));
    let error = AuthMiddleware::refuses_unbound_session_restore(&configured)
        .expect_err("an external store without an ownership binding must be refused");
    assert!(
        matches!(error, ServiceError::InvalidInput(_)),
        "the refusal was not a configuration answer: {error:?}"
    );
}

/// A denial renders the class, the status, and nothing else.
///
/// The unit-level half of F4-3: every variant the transport can reach, rendered the way
/// [`Denial::into_response`] renders it, carries no marker a caller could act on.
#[test]
fn a_denial_renders_only_its_class() {
    for error in [
        ServiceError::NotFound,
        ServiceError::Stale,
        ServiceError::InvalidInput("login must be between 3 and 64 characters".to_string()),
        ServiceError::Quota,
        ServiceError::Unavailable,
        ServiceError::RetryableBookkeepingConflict,
        ServiceError::StoreIntegrity,
        ServiceError::Internal("store"),
    ] {
        let denial = Denial::new(error);
        let body = denial.message();
        for marker in [
            "SELECT",
            "credential:",
            "account:",
            "$argon2id",
            "Caused by",
            "secret_digest",
        ] {
            assert!(
                !body.contains(marker),
                "a denial message carried {marker:?}: {body}"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
/// One revocation cancels every live lease of a credential (P0-1).
///
/// Every lease is a live request or stream, and `admit` issues one *per request*, so two concurrent calls
/// from one token is the ordinary case, not a corner. Cancelling only the most recently registered one
/// means a request admitted under a since-revoked credential keeps running.
#[tokio::test(flavor = "multi_thread")]
async fn one_revocation_cancels_every_live_lease_of_a_credential() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let first = harness
        .lease(&accounts.alice.bearer)
        .await
        .expect("a live token admits");
    let second = harness
        .lease(&accounts.alice.bearer)
        .await
        .expect("a live token admits twice");
    assert!(
        !first.is_cancelled() && !second.is_cancelled(),
        "a fresh lease was born cancelled"
    );
    assert_eq!(harness.registry.live_leases(), 2);

    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;
    harness.revoke(&owner, &accounts.alice.credential).await;

    assert!(
        first.is_cancelled(),
        "the first lease outlived its credential"
    );
    assert!(
        second.is_cancelled(),
        "the second lease outlived its credential"
    );
}

/// A sibling lease dropping must not un-gate a live one (P0-1, second half).
///
/// A registration's `Drop` removes an entry from the very map a revocation cancels out of. If it removes
/// the wrong entry — or the whole subject — the next revocation finds nothing to cancel.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_lease_does_not_uncancel_the_lease_still_in_flight() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let doomed = harness
        .lease(&accounts.alice.bearer)
        .await
        .expect("a live token admits");
    let survivor = harness
        .lease(&accounts.alice.bearer)
        .await
        .expect("a live token admits twice");
    drop(doomed);

    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;
    harness.revoke(&owner, &accounts.alice.credential).await;
    assert!(
        survivor.is_cancelled(),
        "dropping one sibling left the revocation nothing to cancel"
    );
}

/// The live-lease counter counts registrations, not cancellations (P2-1).
///
/// This counter is the oracle the barrier test reads when it asserts that a refusal left nothing
/// registered behind. If a revocation also subtracts for handles that are still alive, it can read zero
/// with live work in flight and the assertion passes because the instrument lied.
#[tokio::test(flavor = "multi_thread")]
async fn the_live_lease_counter_counts_registrations_not_cancellations() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let first = harness
        .lease(&accounts.alice.bearer)
        .await
        .expect("lease 1");
    let second = harness
        .lease(&accounts.alice.bearer)
        .await
        .expect("lease 2");
    assert_eq!(harness.registry.live_leases(), 2);

    let owner = harness
        .authorize(&harness.principal_of(&accounts.alice.bearer).await)
        .await;
    harness.revoke(&owner, &accounts.alice.credential).await;
    assert_eq!(
        harness.registry.live_leases(),
        2,
        "the counter subtracted for handles that are still alive"
    );

    drop(first);
    drop(second);
    for _ in 0..60 {
        if harness.registry.live_leases() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        harness.registry.live_leases(),
        0,
        "a released registration leaked"
    );
}

/// A revocation written by somebody else is still felt at the commit-time recheck (P2-4).
///
/// The other cases of `the_commit_gate_feels_revocation_suspension_and_an_epoch_bump` publish through this
/// process, so the revocation case is answered by the local short-circuit before the store is read. A
/// second instance — or an operator with a console — revokes by writing the row and saying nothing, which
/// is the case the cross-process claim in report §11.3 rests on.
#[tokio::test(flavor = "multi_thread")]
async fn a_revocation_written_elsewhere_is_still_refused_at_the_commit_gate() {
    let harness = Harness::hosted().await;
    let accounts = harness.pair().await;
    let lease = harness
        .lease(&accounts.alice.bearer)
        .await
        .expect("a live token admits");
    harness
        .revalidate(&lease)
        .await
        .expect("a live lease revalidates");

    harness
        .set_credential(&accounts.alice.credential, "revoked = true", Vec::new())
        .await;
    assert!(
        !lease.is_cancelled(),
        "the fixture published through the bus, which is the case already covered"
    );
    let error = harness
        .revalidate(&lease)
        .await
        .expect_err("a credential revoked in storage revalidated");
    assert!(
        matches!(error, ServiceError::Stale),
        "the store-side revocation was answered with the wrong class: {error:?}"
    );

    let bob = harness
        .lease(&accounts.bob.bearer)
        .await
        .expect("bob admits");
    harness
        .revalidate(&bob)
        .await
        .expect("the control lease must still revalidate");
}

/// A stateless request missing the standard headers is a protocol answer, never an auth one (P2-9).
///
/// The brief names both halves: the version-required `Mcp-Method` / `Mcp-Name` / protocol-version headers
/// must be required, and a malformed-request refusal must stay distinguishable from a missing or revoked
/// credential. Only the contradictory-header control existed before, which leaves a client that omits the
/// headers entirely unmeasured.
#[tokio::test(flavor = "multi_thread")]
async fn a_stateless_request_without_the_standard_headers_is_a_protocol_refusal_not_an_auth_one() {
    let harness = Harness::stateless().await;
    let accounts = harness.pair().await;

    // (a) No headers and no credential: the authority question is asked first, so this is 401.
    let (anonymous, _, _) = harness
        .send_and_read(harness.post(stateless_tool_call(701, "whoami")))
        .await;
    assert_eq!(anonymous, StatusCode::UNAUTHORIZED);

    // (b) No headers at all, valid credential: a protocol refusal, and not an auth status.
    let (missing, _, missing_bytes) = harness
        .send_and_read(with_bearer(
            harness.post(stateless_tool_call(702, "whoami")),
            &accounts.alice.bearer,
        ))
        .await;
    let missing_body = String::from_utf8_lossy(&missing_bytes).into_owned();
    assert!(
        missing.is_client_error(),
        "a missing standard header was not refused: {missing} {missing_body}"
    );
    assert_ne!(
        missing,
        StatusCode::UNAUTHORIZED,
        "a missing standard header was reported as an auth failure: {missing_body}"
    );
    assert_ne!(
        missing,
        StatusCode::NOT_FOUND,
        "a missing standard header was reported as an unknown session: {missing_body}"
    );

    // (c) Version and method present, `Mcp-Name` absent for a tools/call: same class.
    let mut partial = with_bearer(
        harness.post(stateless_tool_call(703, "whoami")),
        &accounts.alice.bearer,
    );
    partial.headers_mut().insert(
        "mcp-protocol-version",
        rmcp::model::ProtocolVersion::V_2026_07_28
            .as_str()
            .parse()
            .expect("a version header"),
    );
    partial
        .headers_mut()
        .insert("mcp-method", "tools/call".parse().expect("a method header"));
    let (partial_status, _, partial_bytes) = harness.send_and_read(partial).await;
    let partial_body = String::from_utf8_lossy(&partial_bytes).into_owned();
    assert!(
        partial_status.is_client_error() && partial_status != StatusCode::UNAUTHORIZED,
        "an incomplete standard-header set was not a protocol refusal: {partial_status} {partial_body}"
    );
    assert_eq!(
        harness.calls.count("whoami"),
        0,
        "a refused stateless call reached the handler"
    );

    // Control: fully headed, the same request is served.
    let (served, _, served_bytes) = harness
        .send_and_read(with_standard_headers(
            with_bearer(
                harness.post(stateless_tool_call(704, "whoami")),
                &accounts.alice.bearer,
            ),
            Some("tools/call"),
            Some("whoami"),
        ))
        .await;
    assert_eq!(
        served,
        StatusCode::OK,
        "the headed control did not work: {}",
        String::from_utf8_lossy(&served_bytes)
    );
}

// Local fixtures for this file only
// ---------------------------------------------------------------------------------------------

/// Adds the `Last-Event-Id` a reconnecting SSE client sends.
fn with_reconnect_marker(
    mut request: axum::extract::Request<axum::body::Body>,
) -> axum::extract::Request<axum::body::Body> {
    request
        .headers_mut()
        .insert("last-event-id", "0".parse().expect("a header value"));
    request
}

/// A session store, so the cross-instance refusal has a real object to refuse.
#[derive(Default)]
struct InMemorySessionStore(std::sync::Mutex<std::collections::HashMap<String, SessionState>>);

#[async_trait::async_trait]
impl SessionStore for InMemorySessionStore {
    async fn load(&self, id: &str) -> Result<Option<SessionState>, SessionStoreError> {
        Ok(self.0.lock().expect("store lock poisoned").get(id).cloned())
    }
    async fn store(&self, id: &str, state: &SessionState) -> Result<(), SessionStoreError> {
        self.0
            .lock()
            .expect("store lock poisoned")
            .insert(id.to_string(), state.clone());
        Ok(())
    }
    async fn delete(&self, id: &str) -> Result<(), SessionStoreError> {
        self.0.lock().expect("store lock poisoned").remove(id);
        Ok(())
    }
}

/// Renders an outcome the way a handler would, for the body scan: status plus the only text a caller sees.
fn render<T: std::fmt::Debug>(outcome: Result<T, ServiceError>) -> (StatusCode, String) {
    match outcome {
        Ok(_) => (StatusCode::OK, "ok".to_string()),
        Err(error) => {
            let denial = Denial::new(error);
            (denial.status(), denial.message())
        }
    }
}

/// The logins a test expects to find in the store, in the order `accounts_named` returns them.
fn sorted(logins: &[&str]) -> Vec<String> {
    let mut sorted = logins
        .iter()
        .map(|login| login.to_string())
        .collect::<Vec<_>>();
    sorted.sort();
    sorted
}
