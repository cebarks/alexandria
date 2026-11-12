//! Task 4 credential persistence and crypto, against the pinned engine.
//!
//! Every case runs on a real migrated `mem://` store: owners are enrolled through
//! `IdentityRepo::bootstrap_owner`, credentials are written by `AuthRepo`, and a refusal is the engine's or
//! the repo's answer rather than a mock. Each negative is paired with a positive control from the same
//! fixture, because an implementation that refused everything would otherwise pass.
//!
//! The Argon2 policy costs ~19 MiB per hash, so the digest of the shared test password is produced **once
//! per process** ([`policy_digest`]) and reused; a test that needs a second password pays one more hash.
//! Nothing here hashes in a loop, and the two concurrency tests are the only places where four jobs can
//! overlap.

use alexandria_storage::access::{AccessRepo, OwnerContext, Principal, PrincipalProvenance};
use alexandria_storage::error::{OwnerWorkspaceLost, StorageError};
use alexandria_storage::models::auth::{Account, Credential, account_status, credential_kind};
use alexandria_storage::models::identity::{
    AccountId, CredentialId, OperationId, ProjectId, WorkspaceId,
};
use alexandria_storage::models::{MemoryScope, ReadSelection};
use alexandria_storage::repos::{AuthRepo, IdentityRepo, PasswordDigest};
use alexandria_storage::{Database, schema};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value as Json;
use std::sync::Arc;
use surrealdb::types::RecordId;
use tokio::sync::OnceCell;

/// The password every fixture owner shares, deliberately not a real secret: what is under test is the
/// shape of the digest and the state machine around it.
const PASSWORD: &str = "correct-horse-battery-staple";

// The service-side initial policy, mirrored here so the tests read as the policy is written: browser
// 30-minute idle / 24-hour absolute, invitation 48 hours, recovery 1 hour.
const IDLE: TimeDelta = TimeDelta::minutes(30);
const ABSOLUTE: TimeDelta = TimeDelta::hours(24);
const INVITE_TTL: TimeDelta = TimeDelta::hours(48);
const RECOVERY_TTL: TimeDelta = TimeDelta::hours(1);

/// Renders a freshly minted secret into an owned string.
///
/// The tests hold the bearer text across `await` points, which a borrow of a temporary cannot do, and the
/// rendering itself consumes the secret — that is the "shown once" property, so a test that wants the text
/// twice takes it once and clones the resulting `String`.
fn shown(secret: &alexandria_storage::repos::BearerSecret) -> String {
    secret.clone().render().expose().to_string()
}

/// The Argon2 digest of [`PASSWORD`], hashed once per process. `OnceCell` rather than a `Lazy` so an
/// unrelated test that never needs a password never pays for one.
async fn policy_digest() -> &'static PasswordDigest {
    static DIGEST: OnceCell<PasswordDigest> = OnceCell::const_new();
    DIGEST
        .get_or_init(|| async {
            AuthRepo::create_password_digest(PASSWORD)
                .await
                .expect("the Argon2id policy must produce a digest")
        })
        .await
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

/// A migrated store with one enrolled owner and its private workspace, created through the public
/// enrollment path with a policy digest — so no case below is testing against a row v009 would refuse.
struct Fixture {
    db: Database,
    alice: AccountId,
    alice_workspace: WorkspaceId,
}

impl Fixture {
    async fn enrolled() -> Self {
        let db = Database::connect_embedded().await.unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let digest = policy_digest().await;
        let ids = IdentityRepo::new(db.inner())
            .bootstrap_owner(
                "alice",
                digest.as_str(),
                &OperationId::new("bootstrap-alice"),
            )
            .await
            .expect("a policy digest must be acceptable to enrollment");
        // The F3 precondition, asserted at the fixture rather than only in its own case: what reached
        // `bootstrap_owner` is a digest this build would produce, and it is still what is stored.
        assert!(AuthRepo::password_digest_is_policy(digest.as_str()));
        Self {
            alice: ids.account,
            alice_workspace: ids.workspace,
            db,
        }
    }

    fn auth(&self) -> AuthRepo<'_> {
        AuthRepo::new(self.db.inner())
    }

    fn access(&self) -> AccessRepo<'_> {
        AccessRepo::new(&self.db)
    }

    async fn account_row(&self, account: &AccountId) -> Account {
        self.auth()
            .account_of(account)
            .await
            .unwrap()
            .expect("the fixture account should still exist")
    }

    async fn credential_row(&self, credential: &CredentialId) -> Credential {
        self.auth()
            .credential_of(credential)
            .await
            .unwrap()
            .expect("the fixture credential should still exist")
    }

    /// The row as the *store* holds it, read straight out of the engine rather than through the repo, so a
    /// claim about what is persisted is a claim about the persisted bytes.
    async fn stored_row(&self, id: &RecordId) -> Json {
        let rows = json(&self.db, "SELECT * FROM ONLY $target;", id).await;
        assert_eq!(rows.len(), 1, "no readable row for {id:?}");
        rows.into_iter().next().unwrap()
    }

    async fn credential_json(&self, credential: &CredentialId) -> Json {
        self.stored_row(credential.as_record_id()).await
    }

    async fn set(&self, id: &RecordId, sets: &str) {
        self.db
            .inner()
            .query(format!("UPDATE ONLY $target SET {sets} RETURN NONE;"))
            .bind(("target", id.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    async fn count(&self, table: &str) -> i64 {
        let mut response = self
            .db
            .inner()
            .query(format!("SELECT count() AS total FROM {table} GROUP ALL;"))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Json> = response.take(0).unwrap();
        rows.first()
            .and_then(|row| row["total"].as_i64())
            .unwrap_or(0)
    }

    /// A second enrolled owner, for the cross-owner cases.
    async fn enroll_bob(&self) -> AccountId {
        IdentityRepo::new(self.db.inner())
            .bootstrap_owner(
                "bob",
                policy_digest().await.as_str(),
                &OperationId::new("bootstrap-bob"),
            )
            .await
            .expect("a second owner should enroll beside the first")
            .account
    }

    /// Alice's owner context, issued by the authorization seam for a principal this module authenticated.
    /// There is no public or test-only authority constructor to reach for instead, which is the point.
    async fn alice_owner(&self) -> OwnerContext {
        self.owner_of(&self.alice).await
    }

    async fn owner_of(&self, account: &AccountId) -> OwnerContext {
        let principal = self.principal_for(account).await;
        self.access().authorize(&principal).await.unwrap()
    }

    /// A real principal: a browser session minted by `AuthRepo` and then presented to it.
    async fn principal_for(&self, account: &AccountId) -> Principal {
        let session = self
            .auth()
            .mint_browser_session(account, IDLE, ABSOLUTE, now())
            .await
            .unwrap();
        self.auth()
            .authenticate_browser_session(shown(&session.cookie).as_str(), IDLE, now())
            .await
            .unwrap()
    }
}

/// `SELECT` rows through a bound id, so no test interpolates a record id into SQL text.
async fn json(db: &Database, sql: &str, target: &RecordId) -> Vec<Json> {
    let mut response = db
        .inner()
        .query(sql)
        .bind(("target", target.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    response.take(0).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Digests (A2, A3, F3)
// ---------------------------------------------------------------------------------------------

/// The digest this build writes is the digest this build verifies, at the policy the owner chose (A2): a
/// standard PHC Argon2id string, `v=19` as the spec's own version field, no injected scheme marker, a fresh
/// salt per call, and a wrong password that does not verify.
#[tokio::test]
async fn digest_round_trip_is_argon2id_at_policy() {
    let digest = AuthRepo::create_password_digest(PASSWORD).await.unwrap();
    let twin = AuthRepo::create_password_digest(PASSWORD).await.unwrap();
    let text = digest.as_str();

    assert!(
        text.starts_with("$argon2id$v=19$"),
        "not an Argon2id PHC string at the spec's version 19"
    );
    assert!(
        text.contains("m=19456,t=2,p=1"),
        "the stored digest does not carry the policy parameters"
    );
    assert_eq!(
        text.matches('$').count(),
        5,
        "expected exactly ident / version / params / salt / hash segments"
    );
    assert!(
        !text.contains("alexandria") && !text.contains("$v1$"),
        "a scheme marker was injected into the PHC string"
    );
    assert!(AuthRepo::password_digest_is_policy(text));
    assert_ne!(
        text,
        twin.as_str(),
        "two digests of one password shared a salt"
    );

    assert!(
        AuthRepo::password_matches(Some(text), PASSWORD)
            .await
            .unwrap(),
        "the password that produced a digest stopped verifying"
    );
    assert!(
        !AuthRepo::password_matches(Some(text), "wrong-horse")
            .await
            .unwrap(),
        "a wrong password verified"
    );
    // The gate is a precondition, not a courtesy: a stored value that is not a policy digest never reaches
    // a verification, even when the presented password is literally that stored text.
    assert!(
        !AuthRepo::password_matches(Some("hunter2"), "hunter2")
            .await
            .unwrap(),
        "a stored value that is not an Argon2id digest authenticated its own text"
    );
    assert!(!AuthRepo::password_matches(None, PASSWORD).await.unwrap());
}

/// F3, the case verification alone cannot catch: a **well-formed Argon2id digest at the wrong cost**
/// verifies happily inside `argon2` (the crate recomputes at the parameters the string carries), so without
/// the policy gate a downgraded or historical digest would keep authenticating forever. Weaker here means
/// 1 MiB and one pass instead of ~19 MiB and two, which is exactly the shape an attacker with the row would
/// have preferred us to be storing.
#[tokio::test]
async fn a_well_formed_digest_at_the_wrong_cost_does_not_authenticate() {
    use argon2::password_hash::rand_core::OsRng;
    use argon2::password_hash::{PasswordHasher, SaltString};
    use argon2::{Algorithm, Argon2, Params, Version};

    let password = "a-weak-but-legal-digest";
    // 1 MiB, one pass: the cheap end of the same algorithm, which is what a downgraded or pre-A2 digest
    // looks like to a verifier that only checks the shape.
    let weak = {
        let params = Params::new(1_024, 1, 1, Some(32)).unwrap();
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        argon2
            .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
            .unwrap()
            .to_string()
    };
    // A real Argon2id PHC string that parses, at parameters this build did not choose.
    assert!(
        !AuthRepo::password_digest_is_policy(&weak),
        "a below-policy digest passed the F3 gate"
    );
    assert!(
        !AuthRepo::password_matches(Some(&weak), password)
            .await
            .unwrap(),
        "a digest below the policy cost authenticated its own password"
    );

    // Positive control, same call site: the policy digest of the same password does authenticate. The
    // difference between the two is the gate, not the verifier.
    let strong = AuthRepo::create_password_digest(password).await.unwrap();
    assert!(AuthRepo::password_digest_is_policy(strong.as_str()));
    assert!(
        AuthRepo::password_matches(Some(strong.as_str()), password)
            .await
            .unwrap(),
        "the policy digest of a password stopped verifying"
    );

    // And the rotation path refuses to write the weak one, so the column cannot drift down.
    let fixture = Fixture::enrolled().await;
    let refused = fixture
        .auth()
        .rotate_password(&fixture.alice, &weak, now())
        .await;
    assert!(
        matches!(refused, Err(StorageError::InvalidInput(_))),
        "a below-policy digest was written into the password column: {refused:?}"
    );
    assert_ne!(
        fixture.account_row(&fixture.alice).await.password_hash,
        Some(weak),
        "the refused rotation still changed the row"
    );
}

/// F3 at the storage boundary: a stored value that is not a policy digest authenticates nothing and is
/// never written by this module. The positive control is the same fixture's owner with the real digest.
#[tokio::test]
async fn a_stored_value_that_is_not_a_policy_digest_never_authenticates() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();

    let enrolled = auth.account_by_login("alice").await.unwrap().unwrap();
    let real = enrolled.password_hash.clone().unwrap();
    assert!(
        AuthRepo::password_digest_is_policy(&real),
        "enrollment stored something other than a policy digest"
    );

    // Somebody hand-wrote a low-entropy value into the column, in the shape a pre-Argon2 build might have
    // left behind. v009 permits the string; this module refuses to act as if it were a digest.
    fixture
        .set(
            fixture.alice.as_record_id(),
            "password_hash = 'shadows-revenge'",
        )
        .await;
    let legacy = auth.account_by_login("alice").await.unwrap().unwrap();
    let legacy_hash = legacy.password_hash.clone().unwrap();
    assert!(!AuthRepo::password_digest_is_policy(&legacy_hash));
    for presented in ["shadows-revenge", PASSWORD, ""] {
        assert!(
            !AuthRepo::password_matches(Some(&legacy_hash), presented)
                .await
                .unwrap(),
            "a non-digest stored value authenticated `{presented}`"
        );
    }

    // And it cannot be written back either: the rotation path refuses anything the gate refuses, so no
    // caller can route a weak value into the column that outlives the fingerprint.
    // `rotate_password` must classify a non-digest as a wrong request rather than store it.
    let refused = auth
        .rotate_password(&fixture.alice, "shadows-revenge", now())
        .await;
    assert!(
        matches!(refused, Err(StorageError::InvalidInput(_))),
        "a non-digest was written into the password column: {refused:?}"
    );
    assert_eq!(
        fixture.account_row(&fixture.alice).await.password_hash,
        Some(legacy_hash),
        "the refused rotation still changed the row"
    );

    // Positive control: a policy digest rotates the same row.
    auth.rotate_password(&fixture.alice, policy_digest().await.as_str(), now())
        .await
        .unwrap();
    assert!(
        AuthRepo::password_digest_is_policy(
            &fixture
                .account_row(&fixture.alice)
                .await
                .password_hash
                .unwrap()
        ),
        "a policy rotation did not store a policy digest"
    );
}

// ---------------------------------------------------------------------------------------------
// Browser sessions
// ---------------------------------------------------------------------------------------------

/// A minted session is two independent random secrets, stored only as digests, and neither half can stand
/// in for the other.
#[tokio::test]
async fn a_minted_session_shows_two_independent_secrets_and_stores_only_digests() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();

    let session = auth
        .mint_browser_session(&fixture.alice, IDLE, ABSOLUTE, at)
        .await
        .unwrap();
    let cookie = session.cookie.render();
    let csrf = session.csrf.render();
    let (cookie_text, csrf_text) = (cookie.expose().to_string(), csrf.expose().to_string());

    assert_eq!(
        cookie_text.len(),
        64,
        "a bearer secret is not 32 bytes of hex"
    );
    assert_eq!(csrf_text.len(), 64);
    assert_ne!(cookie_text, csrf_text, "one secret was minted twice");

    let row = fixture.credential_json(&session.credential).await;
    assert_eq!(row["kind"], "browser");
    assert_eq!(
        row["secret_digest"],
        Json::String(cookie.digest().unwrap()),
        "the session index does not hold the cookie's digest"
    );
    assert_eq!(
        row["csrf_digest"],
        Json::String(csrf.digest().unwrap()),
        "the CSRF half is not paired with this session"
    );
    let rendered = row.to_string();
    assert!(
        !rendered.contains(&cookie_text) && !rendered.contains(&csrf_text),
        "a plaintext secret is readable in the stored row"
    );
    assert_ne!(row["secret_digest"], Json::String(cookie_text.clone()));
    assert!(
        row.get("expires_at").is_some(),
        "no rolling deadline written"
    );
    assert!(
        row.get("absolute_expires_at").is_some(),
        "no absolute cap written"
    );
    assert_eq!(row["revoked"], Json::Bool(false));
    assert_eq!(row["consumed"], Json::Bool(false));

    // Each half resolves only its own way.
    let principal = auth
        .authenticate_browser_session(&cookie_text, IDLE, at)
        .await
        .unwrap();
    assert_eq!(principal.account(), &fixture.alice);
    assert_eq!(
        principal.provenance(),
        &PrincipalProvenance::Credential(session.credential.clone())
    );
    let csrf_as_cookie = auth
        .authenticate_browser_session(&csrf_text, IDLE, at)
        .await;
    assert!(
        matches!(csrf_as_cookie, Err(StorageError::NotFound)),
        "a CSRF token was accepted as a session cookie: {csrf_as_cookie:?}"
    );
    assert!(
        auth.browser_session_matches_csrf(&session.credential, &csrf_text, at)
            .await
            .unwrap(),
        "the paired CSRF token did not verify"
    );
    assert!(
        !auth
            .browser_session_matches_csrf(&session.credential, &cookie_text, at)
            .await
            .unwrap(),
        "the session cookie doubled as a CSRF token"
    );
    assert!(
        !auth
            .browser_session_matches_csrf(&session.credential, "not-hex-at-all", at)
            .await
            .unwrap(),
        "a malformed presentation verified"
    );
    // Presenting the *stored digest* as a secret cannot work: the digest is one-way and not a spelling.
    let stored_digest = row["secret_digest"].as_str().unwrap().to_string();
    assert!(matches!(
        auth.authenticate_browser_session(&stored_digest, IDLE, at)
            .await,
        Err(StorageError::NotFound)
    ));

    // A retired session matches nothing, CSRF included: "the digest is right" about a credential the store
    // has already spent is not an answer a caller may act on.
    let owner = fixture.alice_owner().await;
    auth.revoke_credential(&owner, &session.credential, at)
        .await
        .unwrap();
    assert!(
        !auth
            .browser_session_matches_csrf(&session.credential, &csrf_text, at)
            .await
            .unwrap(),
        "a revoked session's CSRF token still verified"
    );
}

/// 30 minutes idle and 24 hours absolute are two different clocks, and use extends only the first.
#[tokio::test]
async fn browser_sessions_idle_out_and_stop_at_the_absolute_cap() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();
    let session = auth
        .mint_browser_session(&fixture.alice, IDLE, ABSOLUTE, at)
        .await
        .unwrap();
    let cookie = session.cookie.render().expose().to_string();

    // Used inside the window: the deadline moves out and the row records the use.
    let soon = at + TimeDelta::minutes(29);
    auth.authenticate_browser_session(&cookie, IDLE, soon)
        .await
        .unwrap();
    let row = fixture.credential_row(&session.credential).await;
    assert_eq!(
        row.expires_at,
        Some(soon + IDLE),
        "an authenticated use did not extend the idle deadline"
    );
    assert_eq!(row.last_used_at, Some(soon));
    assert_eq!(
        row.absolute_expires_at,
        Some(at + ABSOLUTE),
        "use moved the absolute cap, which is what a rolling deadline must not do"
    );

    // Idle out without a use: the rolled deadline passes, and nothing wrote to the row to make that happen.
    let lapsed = auth
        .authenticate_browser_session(&cookie, IDLE, soon + TimeDelta::minutes(31))
        .await;
    assert!(
        matches!(lapsed, Err(StorageError::Stale)),
        "a session idle past its window still authenticated: {lapsed:?}"
    );

    // A session that is *used* the whole time still stops at the cap. Wide windows here are deliberate:
    // the point is that the roll, not the idle clock, is what the cap binds.
    let wide_idle = TimeDelta::hours(23) + TimeDelta::minutes(59);
    let rolling = auth
        .mint_browser_session(&fixture.alice, wide_idle, ABSOLUTE, at)
        .await
        .unwrap();
    let rolling_cookie = rolling.cookie.render().expose().to_string();
    // Strictly inside the rolling deadline: `expires_at <= now` is what refuses a session, so a use at the
    // exact deadline would be testing the deadline check rather than the clamp.
    let near_cap = at + ABSOLUTE - TimeDelta::minutes(30);
    auth.authenticate_browser_session(&rolling_cookie, wide_idle, near_cap)
        .await
        .unwrap();
    let rolled = fixture.credential_row(&rolling.credential).await;
    assert_eq!(
        rolled.expires_at,
        Some(at + ABSOLUTE),
        "the idle roll outlived the absolute cap: {:?}",
        rolled.expires_at
    );
    assert_eq!(
        rolled.absolute_expires_at,
        Some(at + ABSOLUTE),
        "the cap itself moved"
    );
    let past_cap = auth
        .authenticate_browser_session(&rolling_cookie, wide_idle, at + ABSOLUTE)
        .await;
    assert!(
        matches!(past_cap, Err(StorageError::Stale)),
        "a session was usable at its absolute deadline: {past_cap:?}"
    );
}

/// Minting needs an account the store says may act, and a deadline policy it can honour.
#[tokio::test]
async fn minting_refuses_an_account_that_cannot_act() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();

    let nobody = AccountId::try_from(RecordId::new("account", "nobody")).unwrap();
    let absent = auth.mint_browser_session(&nobody, IDLE, ABSOLUTE, at).await;
    assert!(
        matches!(absent, Err(StorageError::NotFound)),
        "a credential was minted for an account that does not exist: {absent:?}"
    );
    assert_eq!(
        fixture.count("credential").await,
        0,
        "the refused mint still wrote a row"
    );

    fixture
        .set(
            fixture.alice.as_record_id(),
            &format!("status = '{}'", account_status::SUSPENDED),
        )
        .await;
    let frozen = auth
        .mint_browser_session(&fixture.alice, IDLE, ABSOLUTE, at)
        .await;
    assert!(
        matches!(frozen, Err(StorageError::Stale)),
        "a suspended account could mint a session: {frozen:?}"
    );

    // Positive control, same store: lifting the suspension makes minting work again.
    fixture
        .set(
            fixture.alice.as_record_id(),
            &format!("status = '{}'", account_status::ACTIVE),
        )
        .await;
    assert!(
        auth.mint_browser_session(&fixture.alice, IDLE, ABSOLUTE, at)
            .await
            .is_ok()
    );

    // A policy pair that cannot be honoured is a wrong request, not a session that dies at first use.
    let inverted = auth
        .mint_browser_session(&fixture.alice, ABSOLUTE, IDLE, at)
        .await;
    assert!(
        matches!(inverted, Err(StorageError::InvalidInput(_))),
        "an idle window longer than the absolute cap was accepted: {inverted:?}"
    );
    let zero = auth
        .mint_browser_session(&fixture.alice, TimeDelta::zero(), ABSOLUTE, at)
        .await;
    assert!(
        matches!(zero, Err(StorageError::InvalidInput(_))),
        "a zero idle window was accepted: {zero:?}"
    );
}

/// F6 at the storage boundary: an owner whose private workspace row is gone is an internal failure with a
/// *typed* cause, never a silent mint into nothing — and the typing is by type, not by wording.
#[tokio::test]
async fn a_lost_owner_workspace_is_the_typed_internal_case() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();
    // A live owner context and a live session, both produced before anything goes missing: that is what a
    // caller holding an issued context actually looks like.
    let owner = fixture.alice_owner().await;
    let session = auth
        .mint_browser_session(&fixture.alice, IDLE, ABSOLUTE, at)
        .await
        .unwrap();
    let cookie = session.cookie.render().expose().to_string();

    fixture
        .db
        .inner()
        .query("DELETE workspace;")
        .await
        .unwrap()
        .check()
        .unwrap();

    let lost = auth
        .mint_browser_session(&fixture.alice, IDLE, ABSOLUTE, at)
        .await
        .expect_err("a credential was minted for an owner with no workspace");
    assert!(
        matches!(lost, StorageError::Internal(_)),
        "a lost workspace was not kept internal: {lost:?}"
    );
    assert!(
        lost.is_owner_workspace_lost(),
        "the typed cause was lost on the mint path"
    );
    match &lost {
        StorageError::Internal(cause) => assert_eq!(
            cause.downcast_ref::<OwnerWorkspaceLost>(),
            Some(&OwnerWorkspaceLost),
            "the cause is not the marker type the service boundary classifies on"
        ),
        other => panic!("expected an internal failure, got {other:?}"),
    }
    // `Display` stays the fixed string, so nothing about *which* owner is affected rides a client path.
    assert_eq!(lost.to_string(), "internal storage error");

    // The same classification on the revocation path, which re-reads the same owner state.
    let revoke = auth
        .revoke_credential(
            &owner,
            &CredentialId::try_from(RecordId::new("credential", "whatever")).unwrap(),
            at,
        )
        .await
        .expect_err("a revocation went through with no workspace behind the owner");
    assert!(
        revoke.is_owner_workspace_lost(),
        "revocation under a lost workspace was not the typed internal case: {revoke:?}"
    );

    // A failure that merely *sounds* like this one is not reclassified: classification is by type.
    let lookalike = StorageError::Internal(anyhow::anyhow!(
        "the private state an enrolled owner is entitled to is missing from the store"
    ));
    assert!(
        !lookalike.is_owner_workspace_lost(),
        "an error was classified by its message text"
    );

    // Positive control: a credential minted *before* the workspace went missing still authenticates — a
    // lost owner workspace stops new authority from being minted, it does not silently erase the store, and
    // the row is not the thing that was lost.
    let still_live = auth.authenticate_browser_session(&cookie, IDLE, at).await;
    assert!(
        still_live.is_ok(),
        "the pre-existing session stopped authenticating: {still_live:?}"
    );
    // ...while the authorization seam that needs the workspace refuses it, indistinguishably from absence.
    assert!(matches!(
        fixture.access().authorize(&still_live.unwrap()).await,
        Err(StorageError::NotFound)
    ));
}

/// F6 across the crate, not only across `AuthRepo`: the enrollment path Task 3 wrote reports a lost owner
/// workspace as `Internal` too, and it now carries the same typed cause, so one operator-facing class covers
/// every way this crate can discover that an enrolled owner's private state is gone. Task 3's own
/// `bootstrap_retry_refuses_a_workspace_that_no_longer_exists` pins the fail-closed half (no repair, no
/// replacement workspace); this pins the classification half.
#[tokio::test]
async fn enrollment_reports_a_lost_owner_workspace_as_the_same_typed_case() {
    let fixture = Fixture::enrolled().await;
    let ids = IdentityRepo::new(fixture.db.inner())
        .bootstrap_owner(
            "dave",
            policy_digest().await.as_str(),
            &OperationId::new("bootstrap-dave"),
        )
        .await
        .unwrap();
    fixture
        .db
        .inner()
        .query("DELETE workspace WHERE owner = $account;")
        .bind(("account", ids.account.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();

    let retry = IdentityRepo::new(fixture.db.inner())
        .bootstrap_owner(
            "dave",
            policy_digest().await.as_str(),
            &OperationId::new("bootstrap-dave"),
        )
        .await
        .expect_err("a retry healed a missing workspace instead of refusing it");
    assert!(
        matches!(retry, StorageError::Internal(_)),
        "enrollment did not keep this internal: {retry:?}"
    );
    assert!(
        retry.is_owner_workspace_lost(),
        "enrollment's lost-workspace failure is not the typed case: {retry:?}"
    );
    // The credential minting path agrees, which is what lets the service draw one line in the sand.
    let mint = fixture
        .auth()
        .mint_browser_session(&ids.account, IDLE, ABSOLUTE, now())
        .await
        .expect_err("a session was minted for an owner with no workspace");
    assert!(mint.is_owner_workspace_lost(), "{mint:?}");
}

// ---------------------------------------------------------------------------------------------
// Agent tokens
// ---------------------------------------------------------------------------------------------

/// An agent token has no expiry and no cap, records its use, and mints a principal the authorization seam
/// accepts — which is what makes this the production mint path rather than a lookup.
#[tokio::test]
async fn an_agent_token_authenticates_records_and_authorizes() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();
    let owner = fixture.alice_owner().await;

    let minted = auth
        .mint_agent_token(&owner, "  work laptop  ", at)
        .await
        .unwrap();
    let token = minted.secret.render();
    let text = token.expose().to_string();
    assert!(
        minted.expires_at.is_none(),
        "an agent token gained an expiry"
    );

    let row = fixture.credential_row(&minted.credential).await;
    assert_eq!(row.kind, credential_kind::AGENT);
    assert_eq!(
        row.label.as_deref(),
        Some("work laptop"),
        "the token name was stored untrimmed"
    );
    assert_eq!(row.auth_epoch, Some(1));
    assert!(row.expires_at.is_none() && row.absolute_expires_at.is_none());
    assert!(row.last_used_at.is_none());

    let principal = auth.authenticate_agent_token(&text, at).await.unwrap();
    assert_eq!(principal.account(), &fixture.alice);
    assert_eq!(
        principal.credential(),
        Some(&minted.credential),
        "the principal does not name the credential it was verified from"
    );
    let used = fixture.credential_row(&minted.credential).await;
    assert_eq!(used.last_used_at, Some(at));
    assert!(
        used.expires_at.is_none(),
        "recording a use invented a deadline"
    );

    // The minted principal is real authority: it resolves through the authorization seam to this owner's
    // own population, and to nothing else's.
    let context = fixture.access().authorize(&principal).await.unwrap();
    assert_eq!(context.account(), &fixture.alice);
    assert_eq!(context.workspace(), &fixture.alice_workspace);
    assert_eq!(
        fixture
            .access()
            .select(&context, ReadSelection::Auto(None))
            .await
            .unwrap()
            .scopes(),
        vec![MemoryScope::Personal]
    );

    // A blank name is a wrong request, not an unnamed credential — v009's own label assertion would refuse
    // the row, and the caller deserves the classified answer instead of a coercion failure.
    assert!(matches!(
        auth.mint_agent_token(&owner, "   ", at).await,
        Err(StorageError::InvalidInput(_))
    ));

    // A second token on the same account is independent: both work, and neither shares a secret.
    let second = auth.mint_agent_token(&owner, "phone", at).await.unwrap();
    let second_text = second.secret.render().expose().to_string();
    assert_ne!(second_text, text);
    assert!(
        auth.authenticate_agent_token(&second_text, at)
            .await
            .is_ok()
    );
    assert!(auth.authenticate_agent_token(&text, at).await.is_ok());
}

// ---------------------------------------------------------------------------------------------
// Inertness and revocation
// ---------------------------------------------------------------------------------------------

/// Revoked, spent and expired are all inert, each with the answer Task 3's contract gives, and a live
/// sibling on the same account is untouched.
#[tokio::test]
async fn inert_credentials_are_refused_while_a_live_sibling_works() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();
    let owner = fixture.alice_owner().await;

    let live = auth.mint_agent_token(&owner, "live", at).await.unwrap();
    let live_text = live.secret.render().expose().to_string();
    let doomed = auth.mint_agent_token(&owner, "doomed", at).await.unwrap();
    let doomed_text = doomed.secret.render().expose().to_string();

    auth.revoke_credential(&owner, &doomed.credential, at)
        .await
        .unwrap();
    let revoked = auth.authenticate_agent_token(&doomed_text, at).await;
    assert!(
        matches!(revoked, Err(StorageError::Stale)),
        "a revoked token still authenticated: {revoked:?}"
    );
    assert!(auth.authenticate_agent_token(&live_text, at).await.is_ok());

    // Expired by a deadline that has passed.
    fixture
        .set(
            live.credential.as_record_id(),
            "expires_at = d'2000-01-01T00:00:00Z'",
        )
        .await;
    let expired = auth.authenticate_agent_token(&live_text, at).await;
    assert!(
        matches!(expired, Err(StorageError::Stale)),
        "an expired token still authenticated: {expired:?}"
    );

    // Consumed — the state a one-use claim leaves behind.
    fixture
        .set(
            live.credential.as_record_id(),
            "consumed = true, expires_at = NONE",
        )
        .await;
    assert!(matches!(
        auth.authenticate_agent_token(&live_text, at).await,
        Err(StorageError::Stale)
    ));

    // An invitation or a recovery secret is a claim token, never a request credential: presenting it finds
    // nothing at all, so a leaked one-use link is not also an account key.
    let invite = auth
        .create_invitation(&fixture.alice, INVITE_TTL, at)
        .await
        .unwrap();
    let invite_text = invite.secret.render().expose().to_string();
    for presented in [
        auth.authenticate_browser_session(&invite_text, IDLE, at)
            .await,
        auth.authenticate_agent_token(&invite_text, at).await,
    ] {
        assert!(
            matches!(presented, Err(StorageError::NotFound)),
            "a one-use link authenticated a request: {presented:?}"
        );
    }
    let recovery = auth
        .create_recovery_link(&fixture.alice, RECOVERY_TTL, at)
        .await
        .unwrap();
    let recovery_text = shown(&recovery.secret);
    assert!(matches!(
        auth.authenticate_agent_token(&recovery_text, at).await,
        Err(StorageError::NotFound)
    ));

    // Positive control: a credential minted after all that, on the same account, works.
    let fresh = auth.mint_agent_token(&owner, "fresh", at).await.unwrap();
    let fresh_text = shown(&fresh.secret);
    assert!(auth.authenticate_agent_token(&fresh_text, at).await.is_ok());
}

/// An owner cannot revoke somebody else's credential, and the answer is the one a typo gets — in the
/// variant, in the rendered text, and in what the row looks like afterwards.
#[tokio::test]
async fn another_owner_cannot_revoke_and_gets_the_missing_answer() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();
    let alice_owner = fixture.alice_owner().await;
    let bob = fixture.enroll_bob().await;
    let bob_owner = fixture.owner_of(&bob).await;

    let alice_token = auth
        .mint_agent_token(&alice_owner, "laptop", at)
        .await
        .unwrap();
    let alice_text = alice_token.secret.render().expose().to_string();
    let made_up = CredentialId::try_from(RecordId::new("credential", "never_minted")).unwrap();

    let foreign = auth
        .revoke_credential(&bob_owner, &alice_token.credential, at)
        .await;
    let missing = auth
        .revoke_credential(&bob_owner, &made_up, at)
        .await
        .expect_err("a credential that never existed was revoked");
    assert!(
        matches!(foreign, Err(StorageError::NotFound)),
        "a foreign credential was revoked, or its ownership leaked: {foreign:?}"
    );
    assert_eq!(
        foreign.as_ref().err().unwrap().to_string(),
        missing.to_string(),
        "a probe can tell a foreign id from an absent one"
    );
    assert_eq!(
        format!("{:?}", foreign.as_ref().err().unwrap()),
        format!("{missing:?}"),
        "the two refusals differ even in their debug rendering"
    );

    // The row is untouched, and still authenticates for Alice.
    let row = fixture.credential_row(&alice_token.credential).await;
    assert!(!row.revoked.unwrap_or(true));
    assert!(row.revoked_at.is_none());
    assert!(auth.authenticate_agent_token(&alice_text, at).await.is_ok());

    // Alice revokes her own: felt by the presentation path, and idempotent without a second stamp.
    auth.revoke_credential(&alice_owner, &alice_token.credential, at)
        .await
        .unwrap();
    let after = fixture.credential_row(&alice_token.credential).await;
    assert!(after.revoked.unwrap_or(false));
    assert_eq!(after.revoked_at, Some(at));
    assert!(matches!(
        auth.authenticate_agent_token(&alice_text, at).await,
        Err(StorageError::Stale)
    ));
    auth.revoke_credential(
        &alice_owner,
        &alice_token.credential,
        at + TimeDelta::hours(1),
    )
    .await
    .expect("re-revoking your own credential should be idempotent");
    assert_eq!(
        fixture
            .credential_row(&alice_token.credential)
            .await
            .revoked_at,
        Some(at),
        "the idempotent retry rewrote the first revocation stamp"
    );

    // Bob's own credential is unaffected by any of this.
    let bob_token = auth
        .mint_agent_token(&bob_owner, "bob laptop", at)
        .await
        .unwrap();
    assert!(
        auth.authenticate_agent_token(shown(&bob_token.secret).as_str(), at)
            .await
            .is_ok()
    );
}

// ---------------------------------------------------------------------------------------------
// One-use links
// ---------------------------------------------------------------------------------------------

/// A claim is a race, and the race is real: eight spawned consumers of one invitation secret, exactly one of
/// which wins. The losers get the spent-authority answer rather than an internal error, which is what the
/// bounded conflict retry is for — the engine raises an optimistic write conflict on this statement under
/// contention (measured: three of four racers on the identical statement), so a classification that stopped
/// recognising it would surface `Internal` here instead of `Stale`. Eight racers, not two, so the
/// contention is not a coincidence of scheduling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_invitation_claims_have_exactly_one_winner() {
    let fixture = Arc::new(Fixture::enrolled().await);
    let at = now();
    let invite = fixture
        .auth()
        .create_invitation(&fixture.alice, INVITE_TTL, at)
        .await
        .unwrap();
    let secret = invite.secret.render().expose().to_string();

    let racers_live = 8usize;
    let barrier = Arc::new(tokio::sync::Barrier::new(racers_live));
    let mut racers = Vec::new();
    for _ in 0..racers_live {
        let fixture = Arc::clone(&fixture);
        let secret = secret.clone();
        let barrier = Arc::clone(&barrier);
        racers.push(tokio::spawn(async move {
            barrier.wait().await;
            match fixture
                .auth()
                .claim_link(credential_kind::INVITE, &secret, at)
                .await
            {
                Ok(_) => "won".to_string(),
                Err(StorageError::Stale) => "stale".to_string(),
                Err(other) => format!("error {other:?}"),
            }
        }));
    }
    let mut outcomes = Vec::new();
    for racer in racers {
        outcomes.push(racer.await.unwrap());
    }
    let winners = outcomes.iter().filter(|outcome| *outcome == "won").count();
    assert_eq!(
        winners, 1,
        "one link produced {winners} winners: {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome == "won" || outcome == "stale"),
        "a loser did not get the spent-authority answer: {outcomes:?}"
    );

    let row = fixture.credential_row(&invite.credential).await;
    assert!(
        row.consumed.unwrap_or(false),
        "the winner did not consume the link"
    );
    assert_eq!(row.consumed_at, Some(at));
    // A fifth attempt after the race is inert and changes nothing.
    assert!(matches!(
        fixture
            .auth()
            .claim_link(credential_kind::INVITE, &secret, at)
            .await,
        Err(StorageError::Stale)
    ));
    assert_eq!(
        fixture.credential_row(&invite.credential).await.consumed_at,
        Some(at)
    );
}

/// Accepting an invitation enrolls exactly one owner, the second accept is inert, and the enrollment is
/// auditable back to the link that produced it.
#[tokio::test]
async fn accepting_an_invitation_enrolls_one_owner_and_is_inert_afterwards() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();
    let invite = auth
        .create_invitation(&fixture.alice, INVITE_TTL, at)
        .await
        .unwrap();
    let secret = invite.secret.render();
    let secret_text = secret.expose().to_string();
    assert_eq!(secret_text.len(), 64);

    // The link belongs to the issuer, because `credential.account` is required and the invited account does
    // not exist yet. Pinned so the shape reads as a documented choice, not an accident.
    let link_row = fixture.credential_row(&invite.credential).await;
    assert_eq!(link_row.kind, credential_kind::INVITE);
    assert_eq!(&link_row.account_id().unwrap(), &fixture.alice);
    assert_eq!(link_row.expires_at, Some(at + INVITE_TTL));
    assert_eq!(link_row.auth_epoch, Some(1));

    let enrolled = auth
        .accept_invitation(&secret_text, "  Carol ", PASSWORD, at)
        .await
        .unwrap();
    assert_ne!(
        enrolled.account, fixture.alice,
        "the invite became the issuer"
    );
    assert_ne!(enrolled.workspace, fixture.alice_workspace);

    let carol = auth.account_of(&enrolled.account).await.unwrap().unwrap();
    assert_eq!(
        carol.login, "carol",
        "the enrolled login was not normalised"
    );
    assert_eq!(carol.status.as_deref(), Some(account_status::ACTIVE));
    assert_eq!(carol.auth_epoch, Some(1));
    assert!(
        AuthRepo::password_digest_is_policy(carol.password_hash.as_deref().unwrap()),
        "an invitation enrolled an owner without an Argon2id digest"
    );
    assert!(
        AuthRepo::password_matches(carol.password_hash.as_deref(), PASSWORD)
            .await
            .unwrap(),
        "the enrolled password does not verify against the stored digest"
    );
    // The retry anchor names the link that produced it.
    assert!(
        carol
            .bootstrap_operation
            .as_deref()
            .unwrap_or_default()
            .starts_with("invite:credential:"),
        "the enrollment is not auditable back to the invitation"
    );
    assert_eq!(
        fixture.count("workspace").await,
        2,
        "the invited owner did not get its own private workspace"
    );

    let replay = auth
        .accept_invitation(&secret_text, "dave", PASSWORD, at)
        .await;
    assert!(
        matches!(replay, Err(StorageError::Stale)),
        "a consumed invitation was usable a second time: {replay:?}"
    );
    assert_eq!(
        fixture.count("account").await,
        2,
        "the refused replay enrolled somebody anyway"
    );

    // Carol is an owner in her own right, and cannot reach a population that is not hers.
    let carol_session = auth
        .mint_browser_session(&enrolled.account, IDLE, ABSOLUTE, at)
        .await
        .unwrap();
    let carol_cookie = shown(&carol_session.cookie);
    let principal = auth
        .authenticate_browser_session(&carol_cookie, IDLE, at)
        .await
        .unwrap();
    let carol_owner = fixture.access().authorize(&principal).await.unwrap();
    let nope = ProjectId::try_from(RecordId::new("project", "not_carols")).unwrap();
    assert!(matches!(
        fixture
            .access()
            .select(&carol_owner, ReadSelection::Auto(Some(nope)))
            .await,
        Err(StorageError::NotFound)
    ));
}

/// An expired link is inert, and — unlike a *refused request* — an attempt does not consume it, so an
/// operator can still see what was issued.
#[tokio::test]
async fn expired_and_revoked_links_are_inert_and_consume_nothing() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();
    let invite = auth
        .create_invitation(&fixture.alice, RECOVERY_TTL, at)
        .await
        .unwrap();
    let secret = invite.secret.render().expose().to_string();

    let lapsed = auth
        .accept_invitation(&secret, "later", PASSWORD, at + TimeDelta::hours(2))
        .await;
    assert!(
        matches!(lapsed, Err(StorageError::Stale)),
        "an expired invitation was accepted: {lapsed:?}"
    );
    let row = fixture.credential_row(&invite.credential).await;
    assert!(
        !row.consumed.unwrap_or(true),
        "a refused claim consumed the link"
    );
    assert!(row.consumed_at.is_none());

    // Revoking the link — the issuer's own credential — makes it inert the same way.
    let owner = fixture.alice_owner().await;
    auth.revoke_credential(&owner, &invite.credential, at)
        .await
        .unwrap();
    let revoked = auth.accept_invitation(&secret, "later", PASSWORD, at).await;
    assert!(
        matches!(revoked, Err(StorageError::Stale)),
        "a revoked invitation was accepted: {revoked:?}"
    );
    assert!(
        !fixture
            .credential_row(&invite.credential)
            .await
            .consumed
            .unwrap_or(true)
    );

    // Positive control: a fresh link from the same issuer still works.
    let fresh = auth
        .create_invitation(&fixture.alice, INVITE_TTL, at)
        .await
        .unwrap();
    let fresh_text = shown(&fresh.secret);
    assert!(
        auth.accept_invitation(&fresh_text, "fresh-owner", PASSWORD, at)
            .await
            .is_ok()
    );
}

/// Recovery is the whole "invalidate every previous session and agent token" story, and it rides on one
/// integer: the password rotates, the epoch rises, every credential minted before it is inert to both the
/// presentation path and the authorization seam, and a credential minted after it works.
#[tokio::test]
async fn recovery_rotates_the_password_and_invalidates_every_earlier_credential() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();
    let owner = fixture.alice_owner().await;

    let session = auth
        .mint_browser_session(&fixture.alice, IDLE, ABSOLUTE, at)
        .await
        .unwrap();
    let session_cookie = session.cookie.render().expose().to_string();
    let token = auth.mint_agent_token(&owner, "laptop", at).await.unwrap();
    let token_text = token.secret.render().expose().to_string();
    let before = fixture.account_row(&fixture.alice).await;

    // The principal whose authority is about to be revoked, minted through the production path: there is no
    // test-only authority constructor to build one with, which is what makes invalidating *this* value the
    // meaningful assertion.
    let pre_recovery = auth
        .authenticate_agent_token(&token_text, at)
        .await
        .unwrap();
    assert!(
        fixture.access().authorize(&pre_recovery).await.is_ok(),
        "precondition: the credential minted before recovery should authorize"
    );

    let recovery = auth
        .create_recovery_link(&fixture.alice, RECOVERY_TTL, at)
        .await
        .unwrap();
    let secret = recovery.secret.render().expose().to_string();
    let new_password = "a-rotated-secret-password";
    auth.recover(&secret, new_password, at).await.unwrap();

    let after = fixture.account_row(&fixture.alice).await;
    assert_eq!(
        after.auth_epoch,
        Some(before.auth_epoch.unwrap() + 1),
        "recovery did not raise the authentication epoch"
    );
    assert_ne!(
        after.password_hash, before.password_hash,
        "recovery did not rotate the stored digest"
    );
    let rotated = after.password_hash.clone().unwrap();
    assert!(AuthRepo::password_digest_is_policy(&rotated));
    assert!(
        AuthRepo::password_matches(Some(&rotated), new_password)
            .await
            .unwrap(),
        "the new password does not verify"
    );
    assert!(
        !AuthRepo::password_matches(Some(&rotated), PASSWORD)
            .await
            .unwrap(),
        "the old password still verifies after a recovery"
    );

    // Every credential minted before the recovery is inert — to authentication *and* to authorization —
    // without anything touching its row.
    let stale_session = auth
        .authenticate_browser_session(&session_cookie, IDLE, at)
        .await;
    assert!(
        matches!(stale_session, Err(StorageError::Stale)),
        "a pre-recovery session still authenticated: {stale_session:?}"
    );
    let stale_token = auth.authenticate_agent_token(&token_text, at).await;
    assert!(
        matches!(stale_token, Err(StorageError::Stale)),
        "a pre-recovery agent token still authenticated: {stale_token:?}"
    );
    assert!(matches!(
        fixture.access().authorize(&pre_recovery).await,
        Err(StorageError::Stale)
    ));
    for row in [
        fixture.credential_row(&session.credential).await,
        fixture.credential_row(&token.credential).await,
    ] {
        assert!(
            !row.revoked.unwrap_or(false) && !row.consumed.unwrap_or(false),
            "recovery swept credential rows instead of relying on the epoch"
        );
        assert_eq!(
            row.auth_epoch,
            Some(1),
            "recovery rewrote a credential's minted epoch"
        );
    }

    // Positive control: a credential minted under the new epoch works on both paths.
    let fresh = auth
        .mint_browser_session(&fixture.alice, IDLE, ABSOLUTE, at)
        .await
        .unwrap();
    let fresh_cookie = shown(&fresh.cookie);
    let fresh_principal = auth
        .authenticate_browser_session(&fresh_cookie, IDLE, at)
        .await
        .unwrap();
    assert_eq!(
        fixture.credential_row(&fresh.credential).await.auth_epoch,
        Some(2),
        "a credential minted after recovery used the superseded epoch"
    );
    assert!(fixture.access().authorize(&fresh_principal).await.is_ok());

    // The link is spent: a second recovery, with any password, is refused.
    let replay = auth.recover(&secret, "third-password", at).await;
    assert!(
        matches!(replay, Err(StorageError::Stale)),
        "a consumed recovery link was used twice: {replay:?}"
    );
    assert_eq!(
        fixture.account_row(&fixture.alice).await.auth_epoch,
        Some(2),
        "the refused replay moved the epoch again"
    );
}

/// Four spawned consumers of one recovery secret: exactly one rotates the password, and the epoch rises by
/// exactly one. The failure mode this pins is two concurrent recoveries writing the same epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_recovery_claims_rotate_the_password_exactly_once() {
    let fixture = Arc::new(Fixture::enrolled().await);
    let at = now();
    let recovery = fixture
        .auth()
        .create_recovery_link(&fixture.alice, RECOVERY_TTL, at)
        .await
        .unwrap();
    let secret = recovery.secret.render().expose().to_string();

    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let mut racers = Vec::new();
    for index in 0..4 {
        let fixture = Arc::clone(&fixture);
        let secret = secret.clone();
        let barrier = Arc::clone(&barrier);
        racers.push(tokio::spawn(async move {
            barrier.wait().await;
            match fixture
                .auth()
                .recover(&secret, &format!("rotated-{index}"), at)
                .await
            {
                Ok(()) => "won".to_string(),
                Err(StorageError::Stale) => "stale".to_string(),
                Err(other) => format!("error {other:?}"),
            }
        }));
    }
    let mut outcomes = Vec::new();
    for racer in racers {
        outcomes.push(racer.await.unwrap());
    }
    let winners = outcomes.iter().filter(|outcome| *outcome == "won").count();
    assert_eq!(
        winners, 1,
        "one recovery link rotated the password {winners} times: {outcomes:?}"
    );
    assert_eq!(
        fixture.account_row(&fixture.alice).await.auth_epoch,
        Some(2),
        "the epoch did not move exactly once"
    );
}

// ---------------------------------------------------------------------------------------------
// Login-shaped refusals
// ---------------------------------------------------------------------------------------------

/// The building blocks of a login answer identically for an unknown account and a wrong password, so the
/// composition in the service layer cannot be an account-enumeration oracle: an unknown login is *no row*,
/// and no row and a non-verifying digest both route to the same `NotFound`.
#[tokio::test]
async fn an_unknown_login_and_a_wrong_password_are_the_same_answer() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();

    assert!(
        auth.account_by_login("nobody-here")
            .await
            .unwrap()
            .is_none(),
        "a login nobody enrolled was found"
    );
    // The two shapes a login path has to treat alike.
    assert!(!AuthRepo::password_matches(None, PASSWORD).await.unwrap());
    let alice = auth.account_by_login("ALICE ").await.unwrap().unwrap();
    assert_eq!(
        alice.login, "alice",
        "the lookup did not normalise the login"
    );
    assert!(
        !AuthRepo::password_matches(alice.password_hash.as_deref(), "not-the-password")
            .await
            .unwrap(),
        "control broken: a wrong password verified"
    );
    assert!(
        AuthRepo::password_matches(alice.password_hash.as_deref(), PASSWORD)
            .await
            .unwrap(),
        "control: the right password must verify"
    );

    // A malformed login is a wrong request, refused before any read, and the message quotes nothing back.
    let long = "a".repeat(70);
    for bad in ["", "   ", "ab", long.as_str()] {
        let refusal = auth.account_by_login(bad).await.unwrap_err();
        assert!(
            matches!(refusal, StorageError::InvalidInput(_)),
            "`{bad}` reached the store instead of being refused: {refusal:?}"
        );
        assert!(
            !refusal.to_string().contains(&"a".repeat(20)),
            "the refusal echoed the login: {refusal}"
        );
    }
}

/// P2-8: a row read out of the store cannot put secret material in a log line, and neither can any refusal
/// this module produces — in `Display` or in `Debug`.
#[tokio::test]
async fn no_refusal_or_row_render_carries_secret_material() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();
    let owner = fixture.alice_owner().await;

    let session = auth
        .mint_browser_session(&fixture.alice, IDLE, ABSOLUTE, at)
        .await
        .unwrap();
    let cookie = session.cookie.render().expose().to_string();
    let csrf = session.csrf.render().expose().to_string();
    let account = fixture.account_row(&fixture.alice).await;
    let credential = fixture.credential_row(&session.credential).await;
    let hash = account.password_hash.clone().unwrap();
    let digest = credential.secret_digest.clone().unwrap();
    let csrf_digest = credential.csrf_digest.clone().unwrap();

    // The row really does carry them, or the assertions below would pass vacuously.
    let raw = fixture
        .credential_json(&session.credential)
        .await
        .to_string();
    assert!(raw.contains(&digest) && raw.contains(&csrf_digest));
    assert!(
        fixture
            .stored_row(fixture.alice.as_record_id())
            .await
            .to_string()
            .contains(&hash),
        "the stored account row does not hold the digest at all"
    );

    for (label, rendered) in [
        ("Account debug", format!("{account:?}")),
        ("Credential debug", format!("{credential:?}")),
    ] {
        for secret in [&hash, &digest, &csrf_digest, &cookie, &csrf] {
            assert!(
                !rendered.contains(secret),
                "{label} rendered secret material"
            );
        }
        assert!(
            rendered.contains("<redacted>"),
            "{label} does not even say what it hid: {rendered}"
        );
    }

    let refusals: Vec<StorageError> = vec![
        auth.authenticate_agent_token(&cookie, at)
            .await
            .unwrap_err(),
        auth.authenticate_browser_session("zz", IDLE, at)
            .await
            .unwrap_err(),
        auth.claim_link(credential_kind::INVITE, &cookie, at)
            .await
            .unwrap_err(),
        auth.revoke_credential(
            &owner,
            &CredentialId::try_from(RecordId::new("credential", "gone")).unwrap(),
            at,
        )
        .await
        .unwrap_err(),
        auth.account_by_login("").await.unwrap_err(),
        auth.mint_agent_token(&owner, "  ", at).await.unwrap_err(),
    ];
    for refusal in refusals {
        let shown = refusal.to_string();
        let debug = format!("{refusal:?}");
        for secret in [&hash, &digest, &csrf_digest, &cookie, &csrf] {
            assert!(
                !shown.contains(secret.as_str()) && !debug.contains(secret.as_str()),
                "a refusal carried secret material"
            );
        }
        for marker in [
            "SELECT",
            "UPDATE",
            "CREATE",
            "credential",
            "account:",
            "workspace",
        ] {
            assert!(
                !shown.contains(marker),
                "a refusal rendered {marker}: {shown}"
            );
        }
    }
}

/// A link that is not a link, a lifetime that is not positive, and a claim of an authenticating kind are
/// wrong requests rather than storage errors — and none of them writes a row.
#[tokio::test]
async fn malformed_auth_requests_are_refused_before_any_write() {
    let fixture = Fixture::enrolled().await;
    let auth = fixture.auth();
    let at = now();

    let zero = auth
        .create_invitation(&fixture.alice, TimeDelta::zero(), at)
        .await;
    assert!(
        matches!(zero, Err(StorageError::InvalidInput(_))),
        "a zero-lifetime invitation was written: {zero:?}"
    );
    let negative = auth
        .create_recovery_link(&fixture.alice, TimeDelta::seconds(-60), at)
        .await;
    assert!(
        matches!(negative, Err(StorageError::InvalidInput(_))),
        "a negative-lifetime recovery link was written: {negative:?}"
    );
    assert_eq!(
        fixture.count("credential").await,
        0,
        "a refused request still wrote a credential"
    );

    let claimed = auth
        .claim_link(credential_kind::AGENT, &"f".repeat(64), at)
        .await;
    assert!(
        matches!(claimed, Err(StorageError::InvalidInput(_))),
        "an agent token was treated as a claimable link: {claimed:?}"
    );

    // Nothing was written, and the owner is still mintable: these refusals are not a blanket denial.
    assert!(
        auth.create_invitation(&fixture.alice, INVITE_TTL, at)
            .await
            .is_ok()
    );
    assert_eq!(fixture.count("credential").await, 1);
}

// ---------------------------------------------------------------------------------------------
// Refusal cost: which inputs pay for the Argon2 job
// ---------------------------------------------------------------------------------------------

/// The probe's wait budget: slack on a millisecond read, not a discriminator.
///
/// Being exact about what this constant is and is not. It is a constant, and every gated call's wall time
/// is compared against it by `tokio::time::timeout`, so the probe is not threshold-free. What it does not
/// do is use a threshold to tell a digest from a refusal: under the parked gate the digest side can *never*
/// answer, whatever the number, so there is no value of `ANSWER` that separates the two — the gate does
/// that structurally. `ANSWER` only says how long to wait for the things that must answer (a `mem://`
/// read), and if a machine were ever too contended for that, the case fails loudly at the named control
/// instead of passing falsely.
const ANSWER: std::time::Duration = std::time::Duration::from_millis(4_000);

/// Holds the runtime's single blocking thread for as long as the guard is alive.
///
/// [`AuthRepo::create_password_digest`] is the only thing an invitation or recovery claim sends to
/// `spawn_blocking`, so a parked pool is a hard gate on password work rather than a measurement of it:
/// a claim that hashes cannot finish while the guard is held, and a claim that is refused on the read
/// never asks for the thread. That is the F4-1 ordering property observed as capacity: no threshold
/// compares a digest against a refusal, and no mock in sight.
async fn park_the_hashing_pool() -> HashPoolGate {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        let _ = entered_tx.send(());
        let _ = release_rx.recv();
    });
    // Budgeted like every other wait here. Unbounded, this would be the one way the case could hang CI
    // instead of failing: the parked task is the first `spawn_blocking` on a private runtime, so it is
    // essentially certain to arrive, but "certain" is not the same as "cannot stall the suite".
    tokio::time::timeout(ANSWER, entered_rx)
        .await
        .expect("the parked task never took the runtime's only blocking thread")
        .expect("the parked task's entry signal was dropped without being sent");
    HashPoolGate { release_tx }
}

struct HashPoolGate {
    /// Dropping it releases the parked thread, so a test that fails cannot strand the runtime.
    release_tx: std::sync::mpsc::Sender<()>,
}

impl Drop for HashPoolGate {
    fn drop(&mut self) {
        let _ = self.release_tx.send(());
    }
}

/// One claim under the gate, labelled by what it did: refused with this answer, accepted, or unable to
/// answer at all because it queued a digest behind the parked thread.
async fn claim_under_gate<T, F>(budget: std::time::Duration, work: F) -> String
where
    F: std::future::Future<Output = Result<T, StorageError>>,
{
    match tokio::time::timeout(budget, work).await {
        Err(_) => "waited for the hashing pool".to_string(),
        Ok(Ok(_)) => "accepted".to_string(),
        Ok(Err(StorageError::NotFound)) => "not found".to_string(),
        Ok(Err(StorageError::Stale)) => "stale".to_string(),
        Ok(Err(other)) => format!("other error {other:?}"),
    }
}

/// F4-1: the secret is validated before the password is hashed, so the cheapest input to forge is the
/// cheapest to refuse.
///
/// `accept_invitation` and `recover` both used to call `create_password_digest` as their first
/// statement, which put ~19 MiB of Argon2 in front of every refusal on paths Task 5 admits without
/// authentication. The order is now read, hash, spend, and the invariant that ordering was originally
/// written to protect survives it: `presented_link` writes nothing, so a request that cannot produce a
/// digest still cannot burn a one-use link — the guarded UPDATE in `spend_link` is the only statement
/// that spends one. Every case below answers inside the refusal budget while the hashing pool is parked;
/// reintroducing the hash-first order leaves each of them waiting for a thread that is deliberately
/// never released until the test ends, which is what makes this a sequencing probe rather than a
/// stopwatch: no value of the budget tells a digest from a refusal, because under the gate the digest
/// side cannot answer at all — the budget is slack on a millisecond read, and if a machine were ever too
/// loaded for even that slack, the named control fails instead of the case passing falsely.
/// The live claim in the middle is the control that the gate is really gating hashing and not simply
/// blocking the store: it must *not* answer while the guard is held, and must answer after it drops.
#[test]
fn an_unusable_link_is_refused_before_any_password_work() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("a two-worker runtime with exactly one blocking thread");
    runtime.block_on(async {
        // Everything that legitimately hashes is set up before the gate exists.
        let fixture = Fixture::enrolled().await;
        let auth = fixture.auth();
        let at = now();
        let owner = fixture.alice_owner().await;

        let expired = auth
            .create_invitation(&fixture.alice, RECOVERY_TTL, at)
            .await
            .unwrap();
        let expired_text = shown(&expired.secret);
        let revoked = auth
            .create_invitation(&fixture.alice, INVITE_TTL, at)
            .await
            .unwrap();
        let revoked_text = shown(&revoked.secret);
        auth.revoke_credential(&owner, &revoked.credential, at)
            .await
            .unwrap();
        let lapsed_recovery = auth
            .create_recovery_link(&fixture.alice, RECOVERY_TTL, at)
            .await
            .unwrap();
        let lapsed_recovery_text = shown(&lapsed_recovery.secret);
        let live = auth
            .create_invitation(&fixture.alice, INVITE_TTL, at)
            .await
            .unwrap();
        let live_text = shown(&live.secret);
        let live_recovery = auth
            .create_recovery_link(&fixture.alice, RECOVERY_TTL, at)
            .await
            .unwrap();
        let live_recovery_text = shown(&live_recovery.secret);

        let gate = park_the_hashing_pool().await;
        let unknown = "f".repeat(64);

        // Control: the store itself answers while the pool is parked. Without this, a refusal that
        // answered would say nothing about ordering — the gate might simply have stranged the engine too.
        // It is asserted with its own budget rather than trusted, so an engine that ever starts routing
        // reads through `spawn_blocking` fails here by name instead of hanging the suite later.
        let read = tokio::time::timeout(ANSWER, auth.account_of(&fixture.alice)).await;
        assert!(
            matches!(read, Ok(Ok(Some(_)))),
            "control: a plain indexed read could not answer while the blocking pool was parked, so this \n             probe cannot tell a read from a digest"
        );

        let refusals = [
            (
                "accepting an unknown invitation secret",
                claim_under_gate(ANSWER, auth.accept_invitation(&unknown, "someone", PASSWORD, at)).await,
                "not found",
            ),
            (
                "accepting an invitation past its deadline",
                claim_under_gate(
                    ANSWER,
                    auth.accept_invitation(&expired_text, "someone", PASSWORD, at + TimeDelta::hours(2)),
                )
                .await,
                "stale",
            ),
            (
                "accepting a revoked invitation",
                claim_under_gate(ANSWER, auth.accept_invitation(&revoked_text, "someone", PASSWORD, at)).await,
                "stale",
            ),
            (
                "recovering on an unknown secret",
                claim_under_gate(ANSWER, auth.recover(&unknown, PASSWORD, at)).await,
                "not found",
            ),
            (
                "recovering on a link past its deadline",
                claim_under_gate(
                    ANSWER,
                    auth.recover(&lapsed_recovery_text, PASSWORD, at + TimeDelta::hours(2)),
                )
                .await,
                "stale",
            ),
        ];
        for (label, answer, expected) in refusals {
            assert_eq!(
                answer, expected,
                "{label} did not answer as a refusal while the hashing pool was parked"
            );
        }

        // Control: the gate gates hashing, not the store. A claim that legitimately needs a policy
        // digest cannot answer here, so the five refusals above were answered by reads, not by a pool
        // that was never busy in the first place.
        assert_eq!(
            claim_under_gate(ANSWER, auth.accept_invitation(&live_text, "someone", PASSWORD, at)).await,
            "waited for the hashing pool",
            "control: an accepted invitation answered without the hashing pool, so this probe would prove nothing"
        );

        // None of the refusals spent anything: the read-only pre-check burns no link, and the hung
        // claim never reached the guarded write.
        for (label, credential) in [
            ("an expired invitation", &expired.credential),
            ("a revoked invitation", &revoked.credential),
            ("an expired recovery link", &lapsed_recovery.credential),
            ("a live invitation whose claim hung", &live.credential),
        ] {
            let row = fixture.credential_row(credential).await;
            assert!(
                !row.consumed.unwrap_or(true),
                "{label} was spent by a refusal or an abandoned claim"
            );
        }
        assert_eq!(
            fixture.count("account").await,
            1,
            "a refused claim enrolled an owner anyway"
        );

        drop(gate);

        // And the same paths still work when the digest can be produced: the password reaches
        // enrollment, and a recovery still rotates the stored digest.
        assert_eq!(
            claim_under_gate(ANSWER * 4, auth.accept_invitation(&live_text, "someone", PASSWORD, at)).await,
            "accepted",
            "the live invitation stopped working after the pool was released"
        );
        assert_eq!(
            claim_under_gate(ANSWER * 4, auth.recover(&live_recovery_text, PASSWORD, at)).await,
            "accepted",
            "the live recovery link stopped working after the pool was released"
        );
        assert_eq!(
            fixture.count("account").await,
            2,
            "the accepted invitation did not enroll exactly one owner"
        );
    });
}
