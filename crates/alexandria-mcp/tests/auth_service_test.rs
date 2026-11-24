//! The credential lifecycle service, end to end: password in, verified principal out, and nothing secret
//! left in a render.
//!
//! This is the surface Task 5's transports will call and Tasks 6+ will consume errors from, so the cases
//! below are about the *boundary*: what a caller learns, what leaves the process, and which of two failures
//! it can tell apart. The credential state machine itself is pinned in storage's `auth_repo_test`; the
//! split is deliberate, and it is why nothing here needs a mock.
//!
//! Owners are enrolled through `IdentityRepo::bootstrap_owner` with a digest produced by the same Argon2id
//! policy the service uses, and every owner context comes out of `AccessRepo::authorize` — there is no
//! test-only authority constructor to reach for, in this crate or in storage, which is the property the
//! recovery and revocation cases below are measured against.
//!
//! Cost note: the policy is ~19 MiB per hash, so the shared digest is built once per process and every
//! `login` case that needs a real password reuses it. The concurrency tests are the only places where
//! several hashing jobs can overlap.

use alexandria_mcp::auth::{
    AuthService, BROWSER_ABSOLUTE, BROWSER_IDLE, BrowserGrant, INVITE_TTL, RECOVERY_TTL,
};
use alexandria_mcp::error::ServiceError;
use alexandria_storage::access::{AccessRepo, PrincipalProvenance};
use alexandria_storage::error::{OwnerWorkspaceLost, StorageError};
use alexandria_storage::models::auth::{PrincipalIds, account_status, credential_kind};
use alexandria_storage::models::identity::{AccountId, CredentialId, OperationId};
use alexandria_storage::models::{MemoryScope, ProjectId, ReadSelection};
use alexandria_storage::repos::{AuthRepo, IdentityRepo, PasswordDigest};
use alexandria_storage::{Database, schema};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value as Json;
use std::sync::Arc;
use surrealdb::types::RecordId;
use tokio::sync::OnceCell;

const PASSWORD: &str = "horse-battery-correct-staple";
const ROTATED: &str = "rotated-horse-battery-staple";

/// The Argon2id digest of [`PASSWORD`], for enrolling fixture owners without paying for a hash per test.
async fn owner_digest() -> &'static PasswordDigest {
    static DIGEST: OnceCell<PasswordDigest> = OnceCell::const_new();
    DIGEST
        .get_or_init(|| async {
            AuthRepo::create_password_digest(PASSWORD)
                .await
                .expect("the policy must produce a digest")
        })
        .await
}

/// A store, a service over it, and the handles the tests need to authorize and to read rows back.
struct Harness {
    db: Arc<Database>,
    service: AuthService,
}

impl Harness {
    async fn started() -> Self {
        let db = Arc::new(
            Database::connect_embedded()
                .await
                .expect("an in-memory store"),
        );
        schema::migrate(db.inner()).await.unwrap();
        let service = AuthService::new(Arc::clone(&db));
        Self { db, service }
    }

    fn auth(&self) -> AuthRepo<'_> {
        AuthRepo::new(self.db.inner())
    }

    fn access(&self) -> AccessRepo<'_> {
        AccessRepo::new(&self.db)
    }

    /// Enrols an owner through the public enrollment path with a policy digest.
    async fn enroll(&self, login: &str) -> PrincipalIds {
        IdentityRepo::new(self.db.inner())
            .bootstrap_owner(
                login,
                owner_digest().await.as_str(),
                &OperationId::new(login),
            )
            .await
            .unwrap_or_else(|error| panic!("enrolling `{login}` failed: {error:?}"))
    }

    /// Signs `login` in and authorizes the result: the pair of calls a transport makes per request.
    async fn owner_of(&self, login: &str, at: DateTime<Utc>) -> (PrincipalIds, BrowserGrant) {
        let ids = self.enroll(login).await;
        let grant = self
            .service
            .login(login, PASSWORD, at)
            .await
            .unwrap_or_else(|error| panic!("a valid login for `{login}` failed: {error:?}"));
        (ids, grant)
    }

    async fn authorize(
        &self,
        grant: &BrowserGrant,
        at: DateTime<Utc>,
    ) -> alexandria_storage::access::OwnerContext {
        let principal = self
            .service
            .authenticate_browser(grant.expose_cookie(), at)
            .await
            .expect("the session just granted should authenticate");
        self.access()
            .authorize(&principal)
            .await
            .expect("the authenticated principal should authorize")
    }

    async fn stored_row(&self, id: &RecordId) -> Json {
        let mut response = self
            .db
            .inner()
            .query("SELECT * FROM ONLY $target;")
            .bind(("target", id.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Json> = response.take(0).unwrap();
        assert_eq!(rows.len(), 1, "no readable row for {id:?}");
        rows.into_iter().next().unwrap()
    }

    async fn account_json(&self, account: &AccountId) -> Json {
        self.stored_row(account.as_record_id()).await
    }

    async fn set_account(&self, account: &AccountId, sets: &str) {
        self.db
            .inner()
            .query(format!("UPDATE ONLY $target SET {sets} RETURN NONE;"))
            .bind(("target", account.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// Login and browser sessions
// ---------------------------------------------------------------------------------------------

/// A successful login yields a session whose two secrets are independent, shown once, and present in the
/// store only as digests — and each half does only its own job.
#[tokio::test]
async fn login_grants_a_session_whose_halves_are_independent_and_unrecoverable() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let (ids, grant) = harness.owner_of("alice", at).await;

    let cookie = grant.expose_cookie().to_string();
    let csrf = grant.expose_csrf().to_string();
    assert_eq!(cookie.len(), 64, "a session cookie is not 32 bytes of hex");
    assert_eq!(csrf.len(), 64);
    assert_ne!(cookie, csrf, "one secret was minted for both halves");
    assert_eq!(
        grant.expires_at(),
        at + TimeDelta::minutes(30),
        "the grant reports the policy idle deadline, not a made-up lifetime"
    );

    // The cookie authenticates and names the account it belongs to; the CSRF half cannot be presented as a
    // session, and the session cookie cannot satisfy a CSRF check.
    let principal = harness
        .service
        .authenticate_browser(&cookie, at + TimeDelta::minutes(5))
        .await
        .unwrap();
    assert_eq!(principal.account(), &ids.account);
    assert_eq!(
        principal.credential(),
        Some(grant.credential()),
        "the principal does not name the session it was verified from"
    );
    let csrf_as_session = harness
        .service
        .authenticate_browser(&csrf, at)
        .await
        .expect_err("the CSRF half authenticated a session");
    assert_eq!(
        csrf_as_session.to_string(),
        ServiceError::NotFound.to_string()
    );

    assert!(
        harness
            .service
            .verify_csrf(grant.credential(), &csrf, at)
            .await
            .unwrap(),
        "the paired CSRF token did not verify"
    );
    assert!(
        !harness
            .service
            .verify_csrf(grant.credential(), &cookie, at)
            .await
            .unwrap(),
        "the session cookie doubled as a CSRF token"
    );
    assert!(
        !harness
            .service
            .verify_csrf(grant.credential(), "not-hex", at)
            .await
            .unwrap(),
        "a malformed CSRF presentation verified"
    );

    // What the store holds is the digest, never either secret.
    let row = harness.stored_row(grant.credential().as_record_id()).await;
    assert_eq!(row["kind"], "browser");
    let rendered = row.to_string();
    for secret in [&cookie, &csrf] {
        assert!(
            !rendered.contains(secret),
            "a plaintext secret is readable in the stored row"
        );
    }
    assert_ne!(row["secret_digest"], Json::String(cookie.clone()));
    assert_ne!(row["csrf_digest"], Json::String(csrf.clone()));
    // The account row carries a password digest, not a password.
    let account = harness.account_json(&ids.account).await;
    assert!(
        !account.to_string().contains(PASSWORD),
        "the stored account row carried the password"
    );
    assert!(
        account["password_hash"]
            .as_str()
            .unwrap_or_default()
            .starts_with("$argon2id$"),
        "a login-stored digest was not Argon2id PHC"
    );
}

/// The three ways a login fails are one answer, in the variant and in the words, so the endpoint is not an
/// enumeration oracle. The third case is the one a reviewer should look for: an account whose stored value
/// is not an Argon2id digest this build can verify (owner decision F3).
#[tokio::test]
async fn every_login_failure_is_the_same_answer() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let ids = harness.enroll("alice").await;

    let unknown = harness
        .service
        .login("nobody-enrolled", PASSWORD, at)
        .await
        .expect_err("an unknown login succeeded");
    let wrong_password = harness
        .service
        .login("alice", "not-the-password", at)
        .await
        .expect_err("a wrong password succeeded");

    // A value that is not a policy digest — a pre-Argon2 row, or one written by hand. The presented
    // password is *literally the stored text*, which is the strongest thing an attacker who had read the
    // row could try, and it still must not authenticate.
    harness
        .set_account(&ids.account, "password_hash = 'shadows-revenge'")
        .await;
    let legacy = harness
        .service
        .login("alice", "shadows-revenge", at)
        .await
        .expect_err("a stored value that is not an Argon2id digest authenticated");
    // And an account with no password at all is inert, not open: identity and credentials are separate
    // rows, which is what leaves room for a future SSO link.
    harness
        .set_account(&ids.account, "password_hash = NONE")
        .await;
    let no_password = harness
        .service
        .login("alice", "", at)
        .await
        .expect_err("an account with no password logged in");

    for refusal in [&unknown, &wrong_password, &legacy, &no_password] {
        assert!(
            matches!(refusal, ServiceError::NotFound),
            "a login failure was classified {refusal:?}"
        );
        assert_eq!(refusal.to_string(), "not found");
        assert_eq!(format!("{refusal:?}"), "NotFound");
    }

    // Positive control on the same store: with a policy digest stored, the same login works.
    harness
        .set_account(
            &ids.account,
            &format!("password_hash = '{}'", owner_digest().await.as_str()),
        )
        .await;
    assert!(harness.service.login("alice", PASSWORD, at).await.is_ok());
}

/// The policy is stated in literals here, not in terms of the constants under test: a widened window must
/// fail this suite rather than move its own expectations along with it. 31 minutes of silence ends a
/// session whose idle window is 30, and 25 hours ends one that was used the whole time.
#[tokio::test]
async fn a_session_ends_on_the_literally_stated_windows() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let (_ids, grant) = harness.owner_of("alice", at).await;
    let cookie = grant.expose_cookie().to_string();

    // 29 minutes: inside the 30-minute window.
    assert!(
        harness
            .service
            .authenticate_browser(&cookie, at + TimeDelta::minutes(29))
            .await
            .is_ok()
    );
    // 31 minutes after the last use: outside it.
    let lapsed = harness
        .service
        .authenticate_browser(
            &cookie,
            at + TimeDelta::minutes(29) + TimeDelta::minutes(31),
        )
        .await
        .expect_err("a session survived past 30 minutes of silence");
    assert!(matches!(lapsed, ServiceError::Stale), "{lapsed:?}");

    // A second session, used continuously and then met at 25 hours.
    let (_ids, second) = harness.owner_of("bobth", at).await;
    let second_cookie = second.expose_cookie().to_string();
    // Steps of 25 minutes, i.e. strictly inside the 30-minute window: each use lands before the deadline
    // the previous use wrote, so the only thing that can stop the loop is the absolute cap.
    let mut clock = at;
    let mut uses = 0;
    for _ in 0..64 {
        clock += TimeDelta::minutes(25);
        if harness
            .service
            .authenticate_browser(&second_cookie, clock)
            .await
            .is_err()
        {
            break;
        }
        uses += 1;
    }
    let at_25_hours = harness
        .service
        .authenticate_browser(&second_cookie, at + TimeDelta::hours(25))
        .await
        .expect_err("a session was usable past its 24-hour absolute lifetime");
    assert!(
        matches!(at_25_hours, ServiceError::Stale),
        "{at_25_hours:?}"
    );
    // The loop has to have been cut off by the cap, not by a window it should have cleared: that is what
    // makes the 25-hour refusal above a statement about the absolute lifetime rather than a stale fixture.
    assert!(
        (23..30).contains(&(clock - at).num_hours()),
        "the roll stopped at {clock}, after {uses} uses — not at the 24-hour cap"
    );

    // The minting path reports the same windows the constants name, in the same literals.
    let (_ids, third) = harness.owner_of("carolth", at).await;
    assert_eq!(third.expires_at(), at + TimeDelta::minutes(30));
    let invite = {
        let owner = harness.authorize(&third, at).await;
        harness.service.create_invite(&owner, at).await.unwrap()
    };
    let invite_row = harness
        .auth()
        .credential_of(invite.credential())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        invite_row.expires_at,
        Some(at + TimeDelta::hours(48)),
        "an invitation's window is not the policy's 48 hours"
    );
    let owner = harness.authorize(&third, at).await;
    let recovery = harness
        .service
        .create_recovery_link(&owner, at)
        .await
        .unwrap();
    let recovery_row = harness
        .auth()
        .credential_of(recovery.credential())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        recovery_row.expires_at,
        Some(at + TimeDelta::hours(1)),
        "a recovery link's window is not the policy's hour"
    );
    // The windows are not the same number wearing different names: a link that idles out like a session
    // would make `recover` unreachable in practice.
    assert!(recovery_row.expires_at < invite_row.expires_at);
}

/// The policy numbers themselves are requirements, not inputs to the assertions below: 30 minutes of no use
/// ends a browser session, 24 hours ends it however much it was used, an invitation lives 48 hours and a
/// recovery link one hour. Stated as literals so widening a constant cannot make its own test agree.
#[tokio::test]
async fn the_credential_policy_is_the_numbers_the_plan_sets() {
    assert_eq!(BROWSER_IDLE, TimeDelta::minutes(30));
    assert_eq!(BROWSER_ABSOLUTE, TimeDelta::hours(24));
    assert_eq!(INVITE_TTL, TimeDelta::hours(48));
    assert_eq!(RECOVERY_TTL, TimeDelta::hours(1));

    // The idle window must fit inside the cap it is clamped by, or the cap is unreachable; and a recovery
    // link — the credential that can take an account over — must be the shortest window in the policy.
    assert!(BROWSER_IDLE < BROWSER_ABSOLUTE);
    assert!(RECOVERY_TTL < INVITE_TTL);
}

/// The policy the service applies is the plan's: 30 minutes of no use ends a session, and 24 hours ends it
/// however much it was used.
#[tokio::test]
async fn the_service_applies_the_idle_and_absolute_policy_it_advertises() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let (_ids, grant) = harness.owner_of("alice", at).await;
    let cookie = grant.expose_cookie().to_string();

    // Idle out: no use inside the window. The literal 31 minutes is the point — a widened policy constant
    // must fail this, not quietly move the expectation with it.
    let lapsed = harness
        .service
        .authenticate_browser(&cookie, at + TimeDelta::minutes(31))
        .await
        .expect_err("a session idle past its window authenticated");
    assert!(matches!(lapsed, ServiceError::Stale), "{lapsed:?}");

    // Used inside the window, then idle out from the *newer* deadline.
    let (_ids, second) = harness.owner_of("bobth", at).await;
    let second_cookie = second.expose_cookie().to_string();
    let used_at = at + TimeDelta::minutes(29);
    harness
        .service
        .authenticate_browser(&second_cookie, used_at)
        .await
        .expect("a use inside the window must authenticate");
    let rolled = harness
        .auth()
        .credential_of(second.credential())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rolled.expires_at,
        Some(used_at + BROWSER_IDLE),
        "an authenticated use did not push the idle deadline out"
    );
    assert!(
        harness
            .service
            .authenticate_browser(
                &second_cookie,
                used_at + BROWSER_IDLE - TimeDelta::seconds(30)
            )
            .await
            .is_ok(),
        "the rolled deadline is not the one the next use is measured against"
    );
    // Past the *rolled* deadline (not the mint-time one) the session is idle out.
    assert!(matches!(
        harness
            .service
            .authenticate_browser(
                &second_cookie,
                used_at + TimeDelta::minutes(30) + TimeDelta::minutes(30)
            )
            .await,
        Err(ServiceError::Stale)
    ));

    // The absolute cap stops a session that was used the whole time. A window that nearly equals the cap
    // makes the clamp observable in one use rather than forty-eight of them.
    let wide_idle = BROWSER_ABSOLUTE - TimeDelta::minutes(5);
    let rolling = harness
        .auth()
        .mint_browser_session(
            &harness.enroll("carol").await.account,
            wide_idle,
            BROWSER_ABSOLUTE,
            at,
        )
        .await
        .unwrap();
    let rolling_cookie = rolling.cookie.render().expose().to_string();
    // A use one minute before the idle deadline, six minutes before the cap: the roll would reach 29h50m
    // without the clamp, which is what the assertion below can then see.
    harness
        .auth()
        .authenticate_browser_session(
            &rolling_cookie,
            wide_idle,
            at + BROWSER_ABSOLUTE - TimeDelta::minutes(6),
        )
        .await
        .expect("a use inside both windows must work");
    // Typed, not string-compared: the engine renders a nanosecond-precision Z form and chrono's
    // `to_rfc3339()` renders an offset, so a text comparison here would test formatting rather than the
    // clamp.
    let rolled = harness
        .auth()
        .credential_of(&rolling.credential)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rolled.expires_at,
        Some(at + BROWSER_ABSOLUTE),
        "the roll pushed the idle deadline past the absolute cap"
    );
    assert_eq!(
        rolled.absolute_expires_at,
        Some(at + BROWSER_ABSOLUTE),
        "the cap itself moved"
    );
    assert!(matches!(
        harness
            .auth()
            .authenticate_browser_session(
                &rolling_cookie,
                BROWSER_ABSOLUTE - TimeDelta::minutes(2),
                at + BROWSER_ABSOLUTE
            )
            .await,
        Err(StorageError::Stale)
    ));
}

/// A suspended account is refused, and the refusal is the moved-authority answer rather than the
/// enumeration-shaped one: whoever holds a valid password is entitled to learn their account is frozen.
#[tokio::test]
async fn a_suspended_account_cannot_sign_in_or_mint() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let ids = harness.enroll("alice").await;
    assert!(harness.service.login("alice", PASSWORD, at).await.is_ok());

    harness
        .set_account(
            &ids.account,
            &format!("status = '{}'", account_status::SUSPENDED),
        )
        .await;
    let frozen = harness
        .service
        .login("alice", PASSWORD, at)
        .await
        .expect_err("a suspended account signed in");
    assert!(matches!(frozen, ServiceError::Stale), "{frozen:?}");

    // Positive control: the same password on an active account works again.
    harness
        .set_account(
            &ids.account,
            &format!("status = '{}'", account_status::ACTIVE),
        )
        .await;
    assert!(harness.service.login("alice", PASSWORD, at).await.is_ok());
}

// ---------------------------------------------------------------------------------------------
// Agent tokens
// ---------------------------------------------------------------------------------------------

/// A named token is minted once, authenticates, and is revoked by name — with the owner's other tokens
/// untouched, which is what makes per-credential revocation real rather than an account flag.
#[tokio::test]
async fn agent_tokens_are_named_independent_and_revocable() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let (ids, grant) = harness.owner_of("alice", at).await;
    let owner = harness.authorize(&grant, at).await;

    let laptop = harness
        .service
        .create_agent_token(&owner, "work laptop", at)
        .await
        .unwrap();
    let phone = harness
        .service
        .create_agent_token(&owner, "phone", at)
        .await
        .unwrap();
    let laptop_text = laptop.expose().to_string();
    let phone_text = phone.expose().to_string();
    assert_ne!(laptop_text, phone_text);

    let principal = harness
        .service
        .authenticate_agent(&laptop_text, at)
        .await
        .unwrap();
    assert_eq!(principal.account(), &ids.account);
    assert_eq!(
        principal.provenance(),
        &PrincipalProvenance::Credential(laptop.credential().clone())
    );
    assert!(
        harness
            .service
            .authenticate_agent(&phone_text, at)
            .await
            .is_ok()
    );

    // Revoke one. The other keeps working, and the revoked one is inert to both this service and the
    // authorization seam.
    harness
        .service
        .revoke_credential(&owner, laptop.credential(), at)
        .await
        .unwrap();
    let revoked = harness
        .service
        .authenticate_agent(&laptop_text, at)
        .await
        .expect_err("a revoked token authenticated");
    assert!(matches!(revoked, ServiceError::Stale), "{revoked:?}");
    assert!(
        harness
            .service
            .authenticate_agent(&phone_text, at)
            .await
            .is_ok()
    );
    // Revoking again is idempotent and does not move the first stamp.
    harness
        .service
        .revoke_credential(&owner, laptop.credential(), at + TimeDelta::hours(3))
        .await
        .unwrap();
    let row = harness.stored_row(laptop.credential().as_record_id()).await;
    assert_eq!(row["revoked"], Json::Bool(true));
    assert_ne!(
        row["revoked_at"].as_str(),
        Some((at + TimeDelta::hours(3)).to_rfc3339().as_str()),
        "the idempotent retry rewrote the revocation stamp"
    );

    // A blank name is a wrong request, refused before any row exists.
    let blank = harness
        .service
        .create_agent_token(&owner, "   ", at)
        .await
        .expect_err("a blank token name was accepted");
    assert!(matches!(blank, ServiceError::InvalidInput(_)), "{blank:?}");
    assert_eq!(
        harness
            .auth()
            .credential_of(&CredentialId::try_from(RecordId::new("credential", "none")).unwrap())
            .await
            .unwrap()
            .map(|_| ()),
        None,
        "the refused request wrote a credential anyway"
    );

    // A token has no deadline: what ends it is revocation and the account's epoch.
    let row = harness.stored_row(laptop.credential().as_record_id()).await;
    assert!(
        row.get("expires_at").is_none() || row["expires_at"].is_null(),
        "an agent token gained an expiry: {row}"
    );
    assert_eq!(row["kind"], credential_kind::AGENT);
}

/// One owner cannot revoke another's credential, and the answer is the one a made-up id gets — the same
/// variant, the same words, the same debug rendering.
#[tokio::test]
async fn another_owners_credential_is_indistinguishable_from_a_missing_id() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let (_alice_ids, alice_grant) = harness.owner_of("alice", at).await;
    let alice_owner = harness.authorize(&alice_grant, at).await;
    let (_bob_ids, bob_grant) = harness.owner_of("bobsaccount", at).await;
    let bob_owner = harness.authorize(&bob_grant, at).await;

    let alice_token = harness
        .service
        .create_agent_token(&alice_owner, "alice laptop", at)
        .await
        .unwrap();
    let alice_text = alice_token.expose().to_string();
    let made_up = CredentialId::try_from(RecordId::new("credential", "never-issued")).unwrap();

    let foreign = harness
        .service
        .revoke_credential(&bob_owner, alice_token.credential(), at)
        .await
        .expect_err("Bob revoked Alice's token");
    let missing = harness
        .service
        .revoke_credential(&bob_owner, &made_up, at)
        .await
        .expect_err("a credential that never existed was revoked");
    assert!(matches!(foreign, ServiceError::NotFound), "{foreign:?}");
    assert_eq!(foreign.to_string(), missing.to_string());
    assert_eq!(format!("{foreign:?}"), format!("{missing:?}"));
    // Alice's token is untouched and still authenticates.
    assert!(
        harness
            .service
            .authenticate_agent(&alice_text, at)
            .await
            .is_ok()
    );

    // And the two owners' populations stay disjoint, which is the reason the refusal is worded this way.
    let alice_principal = harness
        .service
        .authenticate_agent(&alice_text, at)
        .await
        .unwrap();
    let context = harness.access().authorize(&alice_principal).await.unwrap();
    let bobs_project = ProjectId::try_from(RecordId::new("project", "bobs_project")).unwrap();
    assert!(matches!(
        harness
            .access()
            .select(&context, ReadSelection::Auto(Some(bobs_project)))
            .await,
        Err(StorageError::NotFound)
    ));
    let own = harness
        .access()
        .select(&context, ReadSelection::Auto(None))
        .await
        .unwrap();
    assert_eq!(own.scopes(), vec![MemoryScope::Personal]);
}

// ---------------------------------------------------------------------------------------------
// Invitations and recovery
// ---------------------------------------------------------------------------------------------

/// An invitation enrolls exactly one owner. Four real concurrent accepts of the same secret produce one
/// winner, and the loser's answer is the spent-authority class rather than an internal error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_accepts_of_one_invite_enroll_one_owner() {
    let harness = Arc::new(Harness::started().await);
    let at = Utc::now();
    let (_ids, grant) = harness.owner_of("alice", at).await;
    let issuer = harness.authorize(&grant, at).await;
    let invite = harness.service.create_invite(&issuer, at).await.unwrap();
    let secret = invite.expose().to_string();

    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let mut racers = Vec::new();
    for index in 0..4 {
        let harness = Arc::clone(&harness);
        let secret = secret.clone();
        let barrier = Arc::clone(&barrier);
        racers.push(tokio::spawn(async move {
            barrier.wait().await;
            match harness
                .service
                .accept_invite(&secret, &format!("invited-{index}"), ROTATED, at)
                .await
            {
                Ok(ids) => format!("won invited-{index} {:?}", ids.account),
                Err(ServiceError::Stale) => "stale".to_string(),
                Err(other) => format!("error {other:?}"),
            }
        }));
    }
    let mut outcomes = Vec::new();
    for racer in racers {
        outcomes.push(racer.await.unwrap());
    }
    let winners: Vec<&String> = outcomes
        .iter()
        .filter(|outcome| outcome.starts_with("won"))
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "one invitation enrolled {} owners: {outcomes:?}",
        winners.len()
    );
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.starts_with("won") || outcome == "stale"),
        "a loser was not told the link was spent: {outcomes:?}"
    );

    // The winner's owner exists, is normalised, and can sign in with the password it chose.
    let winner_login = winners[0]
        .split_whitespace()
        .nth(1)
        .expect("a winner reported its login")
        .to_string();
    let enrolled = harness
        .auth()
        .account_by_login(&winner_login)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("the winner's enrollment of `{winner_login}` did not persist"));
    assert!(enrolled.login.starts_with("invited-"));
    assert!(
        AuthRepo::password_matches(enrolled.password_hash.as_deref(), ROTATED)
            .await
            .unwrap(),
        "the invited owner cannot authenticate with the password it set"
    );
    assert_eq!(
        harness
            .db
            .inner()
            .query("SELECT count() AS total FROM account GROUP ALL;")
            .await
            .unwrap()
            .take::<Vec<Json>>(0)
            .unwrap()[0]["total"]
            .as_i64(),
        Some(2),
        "more than one owner was enrolled by one invitation"
    );

    // The link is spent for a fifth attempt too, and a fresh link from the same issuer still works.
    let replay = harness
        .service
        .accept_invite(&secret, "someone-else", ROTATED, at)
        .await
        .expect_err("a consumed invitation was accepted again");
    assert!(matches!(replay, ServiceError::Stale), "{replay:?}");
    let second_link = harness.service.create_invite(&issuer, at).await.unwrap();
    assert!(
        harness
            .service
            .accept_invite(second_link.expose(), "second-owner", ROTATED, at)
            .await
            .is_ok()
    );
}

/// A recovery rotates the password, invalidates every credential minted before it, and hands back nothing
/// the caller could use without signing in again.
#[tokio::test]
async fn recovery_invalidates_every_earlier_credential_and_returns_nothing() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let (ids, grant) = harness.owner_of("alice", at).await;
    let owner = harness.authorize(&grant, at).await;
    let token = harness
        .service
        .create_agent_token(&owner, "laptop", at)
        .await
        .unwrap();
    let token_text = token.expose().to_string();
    let cookie = grant.expose_cookie().to_string();
    let csrf = grant.expose_csrf().to_string();

    let link = harness
        .service
        .create_recovery_link(&owner, at)
        .await
        .unwrap();
    let secret = link.expose().to_string();
    let unit: () = harness
        .service
        .recover(&secret, ROTATED, at)
        .await
        .expect("a valid recovery link failed");
    assert_eq!(unit, (), "recovery returned something other than nothing");

    // Old password out, new password in.
    let stale_login = harness
        .service
        .login("alice", PASSWORD, at)
        .await
        .expect_err("the pre-recovery password still works");
    assert!(
        matches!(stale_login, ServiceError::NotFound),
        "{stale_login:?}"
    );
    let signed_in_again = harness
        .service
        .login("alice", ROTATED, at)
        .await
        .expect("the rotated password does not work");

    // Every credential minted before the recovery is inert — sessions, tokens, and the CSRF half of the
    // session they were paired with.
    let old_session = harness
        .service
        .authenticate_browser(&cookie, at)
        .await
        .expect_err("a pre-recovery session still authenticated");
    assert!(
        matches!(old_session, ServiceError::Stale),
        "{old_session:?}"
    );
    let old_token = harness
        .service
        .authenticate_agent(&token_text, at)
        .await
        .expect_err("a pre-recovery agent token still authenticated");
    assert!(matches!(old_token, ServiceError::Stale), "{old_token:?}");
    assert!(
        !harness
            .service
            .verify_csrf(grant.credential(), &csrf, at)
            .await
            .unwrap_or(true),
        "the CSRF half of an invalidated session still verified"
    );

    // The new session works, so the invalidation was the epoch and not the account.
    let fresh_cookie = signed_in_again.expose_cookie().to_string();
    let principal = harness
        .service
        .authenticate_browser(&fresh_cookie, at)
        .await
        .unwrap();
    assert_eq!(principal.account(), &ids.account);
    assert!(harness.access().authorize(&principal).await.is_ok());

    // The link cannot be used twice, and a second use does not move the epoch again.
    let replay = harness
        .service
        .recover(&secret, "a-third-password", at)
        .await
        .expect_err("a consumed recovery link was used again");
    assert!(matches!(replay, ServiceError::Stale), "{replay:?}");
    assert_eq!(
        harness.account_json(&ids.account).await["auth_epoch"].as_i64(),
        Some(2),
        "the refused replay advanced the authentication epoch"
    );
}

/// Four real concurrent recoveries of one link: exactly one rotation, and the epoch moves once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_recoveries_of_one_link_rotate_exactly_once() {
    let harness = Arc::new(Harness::started().await);
    let at = Utc::now();
    let (ids, grant) = harness.owner_of("alice", at).await;
    let owner = harness.authorize(&grant, at).await;
    let link = harness
        .service
        .create_recovery_link(&owner, at)
        .await
        .unwrap();
    let secret = link.expose().to_string();

    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let mut racers = Vec::new();
    for index in 0..4 {
        let harness = Arc::clone(&harness);
        let secret = secret.clone();
        let barrier = Arc::clone(&barrier);
        racers.push(tokio::spawn(async move {
            barrier.wait().await;
            match harness
                .service
                .recover(&secret, &format!("rotated-password-{index}"), at)
                .await
            {
                Ok(()) => "won".to_string(),
                Err(ServiceError::Stale) => "stale".to_string(),
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
        harness.account_json(&ids.account).await["auth_epoch"].as_i64(),
        Some(2),
        "the authentication epoch did not move exactly once"
    );
    // Exactly one of the four passwords is the stored one — the winner's, whichever raced first.
    let stored = harness.account_json(&ids.account).await;
    let hash = stored["password_hash"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut accepted = 0;
    for index in 0..4 {
        if AuthRepo::password_matches(Some(&hash), &format!("rotated-password-{index}"))
            .await
            .unwrap()
        {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 1, "more than one rotated password verifies");
}

/// An expired link is inert and consumes nothing, and a link is never a request credential.
#[tokio::test]
async fn expired_links_are_inert_and_links_never_authenticate() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let (_ids, grant) = harness.owner_of("alice", at).await;
    let owner = harness.authorize(&grant, at).await;

    let invite = harness.service.create_invite(&owner, at).await.unwrap();
    let secret = invite.expose().to_string();
    let lapsed = harness
        .service
        .accept_invite(&secret, "much-later", ROTATED, at + INVITE_TTL)
        .await
        .expect_err("an expired invitation was accepted");
    assert!(matches!(lapsed, ServiceError::Stale), "{lapsed:?}");
    let row = harness.stored_row(invite.credential().as_record_id()).await;
    assert_eq!(
        row["consumed"],
        Json::Bool(false),
        "a refused claim consumed the invitation"
    );
    // The same link, inside its window, still works: the refusal did not spend it.
    assert!(
        harness
            .service
            .accept_invite(&secret, "still-in-time", ROTATED, at + TimeDelta::hours(2))
            .await
            .is_ok()
    );

    // Neither kind of link is a request credential, however it is presented.
    for text in [
        secret.clone(),
        harness
            .service
            .create_recovery_link(&owner, at)
            .await
            .unwrap()
            .expose()
            .to_string(),
    ] {
        assert!(matches!(
            harness.service.authenticate_browser(&text, at).await,
            Err(ServiceError::NotFound)
        ));
        assert!(matches!(
            harness.service.authenticate_agent(&text, at).await,
            Err(ServiceError::NotFound)
        ));
    }
}

// ---------------------------------------------------------------------------------------------
// The error boundary (P2-8, F6)
// ---------------------------------------------------------------------------------------------

/// F6: a lost owner workspace stays `StorageError::Internal` in storage and becomes a distinguishable,
/// operator-actionable refusal here — classified by variant, never by error text.
#[tokio::test]
async fn a_lost_owner_workspace_becomes_an_operator_actionable_refusal() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let ids = harness.enroll("alice").await;

    // The workspace row is gone: an integrity finding for whoever runs the server, not a bad request.
    harness
        .db
        .inner()
        .query("DELETE workspace;")
        .await
        .unwrap()
        .check()
        .unwrap();

    let refusal = harness
        .service
        .login("alice", PASSWORD, at)
        .await
        .expect_err("a login succeeded with no workspace behind the owner");
    assert!(
        matches!(refusal, ServiceError::StoreIntegrity),
        "the lost workspace was classified {refusal:?}"
    );
    // The storage class is unchanged: internal, with a typed cause.
    let storage = StorageError::owner_workspace_lost();
    assert_eq!(storage.to_string(), "internal storage error");
    assert!(storage.is_owner_workspace_lost());
    match &storage {
        StorageError::Internal(cause) => {
            assert_eq!(cause.downcast_ref(), Some(&OwnerWorkspaceLost))
        }
        other => panic!("expected an internal failure, got {other:?}"),
    }
    assert!(matches!(
        ServiceError::from(storage),
        ServiceError::StoreIntegrity
    ));
    // A wording lookalike is *not* reclassified: the mapping is by type.
    let lookalike = ServiceError::from(StorageError::Internal(anyhow::anyhow!(
        "the private state an enrolled owner is entitled to is missing from the store"
    )));
    assert!(
        matches!(lookalike, ServiceError::Internal(_)),
        "an error was classified by its message: {lookalike:?}"
    );
    // And the operator message says what to decide, without naming the owner or the table.
    let words = refusal.to_string();
    assert!(
        words.contains("backup") && words.contains("recovery"),
        "the operator message is not actionable: {words}"
    );
    assert_ne!(
        ids.account,
        AccountId::try_from(RecordId::new("account", "whoever")).unwrap()
    );

    // Every other storage class reaches its own service class, unflattened.
    for (storage, expected) in [
        (StorageError::NotFound, "not found"),
        (StorageError::Stale, "stale"),
        (
            StorageError::RetryableBookkeepingConflict,
            "retryable bookkeeping conflict",
        ),
        (StorageError::Quota, "quota exceeded"),
        (StorageError::Unavailable, "unavailable"),
        (
            StorageError::InvalidInput("login must not be blank".to_string()),
            "invalid input: login must not be blank",
        ),
    ] {
        let class = std::mem::discriminant(&storage);
        let mapped = ServiceError::from(storage);
        assert_eq!(mapped.to_string(), expected, "{class:?} was flattened");
    }
}

/// P2-8, the client-visible half: every denial this service can produce, rendered as a body would be,
/// carries no SQL, no cause chain, no table name, no account id and no credential material.
#[tokio::test]
async fn every_denial_body_carries_no_internals_or_secrets() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let ids = harness.enroll("alice").await;
    let (_ids, grant) = harness.owner_of("bobth", at).await;
    let owner = harness.authorize(&grant, at).await;
    let token = harness
        .service
        .create_agent_token(&owner, "laptop", at)
        .await
        .unwrap();
    let token_text = token.expose().to_string();
    let cookie = grant.expose_cookie().to_string();
    let csrf = grant.expose_csrf().to_string();
    let account_row = harness.account_json(&ids.account).await;
    let password_hash = account_row["password_hash"].as_str().unwrap().to_string();
    let credential_row = harness.stored_row(token.credential().as_record_id()).await;
    let secret_digest = credential_row["secret_digest"]
        .as_str()
        .unwrap()
        .to_string();

    harness
        .service
        .revoke_credential(&owner, token.credential(), at)
        .await
        .unwrap();
    harness
        .set_account(&ids.account, "password_hash = 'not-an-argon2-digest'")
        .await;
    harness
        .db
        .inner()
        .query("DELETE workspace;")
        .await
        .unwrap()
        .check()
        .unwrap();

    let denials: Vec<ServiceError> = vec![
        harness
            .service
            .login("alice", PASSWORD, at)
            .await
            .unwrap_err(),
        harness
            .service
            .login("who-is-this", PASSWORD, at)
            .await
            .unwrap_err(),
        harness
            .service
            .authenticate_agent(&token_text, at)
            .await
            .unwrap_err(),
        harness
            .service
            .revoke_credential(&owner, grant.credential(), at)
            .await
            .expect_err("revoking under a deleted workspace should fail"),
        harness
            .service
            .authenticate_browser(&cookie, at + BROWSER_IDLE)
            .await
            .unwrap_err(),
        harness
            .service
            .create_agent_token(&owner, "", at)
            .await
            .unwrap_err(),
        harness
            .service
            .accept_invite("no-such-link", "newowner", PASSWORD, at)
            .await
            .unwrap_err(),
        harness
            .service
            .recover("no-such-link", PASSWORD, at)
            .await
            .unwrap_err(),
        ServiceError::from(StorageError::owner_workspace_lost()),
        ServiceError::from(StorageError::Internal(anyhow::anyhow!(
            "SELECT * FROM credential WHERE secret_digest = 'x'"
        ))),
    ];
    assert!(!denials.is_empty());

    // The session's own CSRF digest, so the loop below checks the value a *revoked* session's row still
    // carries and not only the ones captured earlier.
    let session_csrf_digest =
        harness.stored_row(grant.credential().as_record_id()).await["csrf_digest"]
            .as_str()
            .unwrap_or_default()
            .to_string();

    for denial in denials {
        // What a transport renders: the sanitized message, and a body built out of it.
        let message = denial.to_string();
        let body = serde_json::to_string(&serde_json::json!({ "error": message })).unwrap();
        let debug = format!("{denial:?}");
        for secret in [
            password_hash.as_str(),
            secret_digest.as_str(),
            cookie.as_str(),
            csrf.as_str(),
            token_text.as_str(),
            session_csrf_digest.as_str(),
            PASSWORD,
        ] {
            assert!(
                !secret.is_empty()
                    && !message.contains(secret)
                    && !body.contains(secret)
                    && !debug.contains(secret),
                "a denial render carried credential material"
            );
        }
        for marker in [
            "SELECT",
            "UPDATE",
            "CREATE ",
            "DEFINE",
            "WHERE",
            "credential",
            "workspace",
            "account:",
            "secret_digest",
            "password_hash",
            "Caused by",
            "argon2",
        ] {
            assert!(
                !message.contains(marker) && !body.contains(marker),
                "a denial body carried {marker:?}: {body}"
            );
            assert!(
                !debug.contains(marker),
                "a denial's Debug carried {marker:?}: {debug}"
            );
        }
    }
}

/// The grant and token types themselves render redacted, so a `{:?}` in a log line or a panic message
/// cannot leak the one value that has no second copy.
#[tokio::test]
async fn grants_and_secrets_render_redacted() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let (_ids, grant) = harness.owner_of("alice", at).await;
    let owner = harness.authorize(&grant, at).await;
    let token = harness
        .service
        .create_agent_token(&owner, "laptop", at)
        .await
        .unwrap();

    let cookie = grant.expose_cookie().to_string();
    let csrf = grant.expose_csrf().to_string();
    let secret = token.expose().to_string();
    let grant_debug = format!("{grant:?}");
    let token_debug = format!("{token:?}");
    let row = harness.stored_row(token.credential().as_record_id()).await;
    let secret_digest = row["secret_digest"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    for (label, rendered) in [
        ("BrowserGrant debug", grant_debug.clone()),
        ("BrowserGrant display", format!("{grant}")),
        ("OneTimeSecret debug", token_debug.clone()),
        ("OneTimeSecret display", format!("{token}")),
    ] {
        for value in [&cookie, &csrf, &secret, &secret_digest] {
            assert!(
                !value.is_empty() && !rendered.contains(value),
                "{label} rendered a secret"
            );
        }
    }
    assert!(grant_debug.contains("<redacted>") && token_debug.contains("<redacted>"));
    // Redaction is not loss: the plaintext is still reachable where it is legitimately needed.
    assert_eq!(grant.expose_cookie(), cookie);
    assert_eq!(token.expose(), secret);
    assert!(format!("{grant}").contains("expiring"));
}

/// Storage's row shapes, as this crate reads and logs them, keep their secret columns hidden — the P2-8
/// half that lives in storage is asserted here too, because it is this crate's log lines that would carry
/// them.
#[tokio::test]
async fn storage_rows_render_without_secret_columns() {
    let harness = Harness::started().await;
    let at = Utc::now();
    let ids = harness.enroll("alice").await;
    let auth = harness.auth();
    let account = auth.account_of(&ids.account).await.unwrap().unwrap();
    let grant = harness.service.login("alice", PASSWORD, at).await.unwrap();
    let credential = auth
        .credential_of(grant.credential())
        .await
        .unwrap()
        .unwrap();

    let row = harness.account_json(&ids.account).await;
    let hash = row["password_hash"].as_str().unwrap().to_string();
    let session_row = harness.stored_row(grant.credential().as_record_id()).await;
    let digest = session_row["secret_digest"].as_str().unwrap().to_string();
    let csrf_digest = session_row["csrf_digest"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    for (label, rendered) in [
        ("Account", format!("{account:?}")),
        ("Credential", format!("{credential:?}")),
    ] {
        for secret in [&hash, &digest, &csrf_digest] {
            assert!(
                !secret.is_empty() && !rendered.contains(secret),
                "{label} rendered secret material"
            );
        }
        assert!(rendered.contains("<redacted>"), "{label} hid nothing");
    }
    // And the columns really are in the store, or the above proves nothing.
    assert!(row.to_string().contains(&hash));
    assert!(session_row.to_string().contains(&digest));
    // A row handed to a serializer must not carry them either.
    assert!(
        !serde_json::to_string(&account).unwrap().contains(&hash),
        "serializing an account row shipped its password digest"
    );
}
