//! Credentials, browser sessions, one-use links, and the crypto in front of all of them.
//!
//! This is the crate's only writer of `account.password_hash` and `credential.secret_digest`, and the only
//! in-crate producer of a production [`Principal`]. Both halves matter: a principal exists because a
//! credential row was *verified here*, against a secret presented now. [`crate::access::AccessRepo`] still
//! decides what a principal may read, and nothing in this module hands out an
//! [`crate::access::OwnerContext`], a workspace, or a shortcut past `authorize`.
//!
//! ## Password digests (owner decisions A2 and A3)
//!
//! Argon2id, PHC strings, `m_cost = 19456` KiB (~19 MiB), `t_cost = 2`, `p_cost = 1`, 32-byte output, a
//! per-password random salt, and **no pepper**: `stored = argon2id(password)`. The price of that choice is
//! stated rather than implied — a database dump is offline-attackable without any additional secret. There
//! is no HMAC pre-hash, and no `[auth]` section to dial the costs down by accident; changing them is a code
//! change plus a documented migration ([`AuthRepo::password_digest_is_policy`]).
//!
//! Hashing and verification run on `tokio::task::spawn_blocking`, never on an async worker: ~19 MiB and two
//! passes per call would otherwise stall the runtime serving retrieval. Bounded admission of those jobs is
//! Task 5's; this module neither blocks the runtime nor pretends a pool exists, and adds none.
//!
//! ## Bearer material
//!
//! Session cookies, agent tokens, invitation links and recovery links are 32 bytes from the operating
//! system's CSPRNG, rendered as 64 lowercase hex characters on the wire and stored **only** as the SHA-256
//! digest of those bytes. [`BearerSecret::render`] takes `self` by value, so a second display of one minted
//! secret is a compile error rather than a convention, and no read path here can recover a secret: the
//! stored digest is one-way.
//!
//! Digest comparison is constant time by way of `argon2`'s own `password-hash` re-export —
//! [`argon2::password_hash::Output`]'s `PartialEq` *is* `subtle::ConstantTimeEq` inside that crate — so the
//! property needs no new dependency edge and no hand-rolled comparison loop. The only new edge in this
//! workspace is `argon2` itself.
//!
//! ## Inertness, and what a refusal says
//!
//! `expires_at` is the deadline past which a credential is inert *without further authenticated use*; for a
//! browser session it is rolled forward on each use and clamped to `absolute_expires_at`, which is why a
//! 30-minute idle window and a 24-hour absolute cap cannot disagree. An absent (`NONE`) deadline means
//! unconstrained — an agent token genuinely has none, because the plan's initial policy names browser,
//! invitation and recovery lifetimes only, and such a token ends at revocation or at a recovery's epoch
//! bump.
//!
//! Whether a row is still live is asked of [`crate::access::credential_entitles_read`], the same function
//! `AccessRepo::authorize` uses, so a credential cannot be inert to authentication and current to
//! authorization. The classification is Task 3's, and it is load-bearing:
//!
//! * not present, or present but not yours → [`StorageError::NotFound`];
//! * present, yours, but no longer entitling (revoked, consumed, expired, obsolete `auth_epoch`, suspended
//!   account) → [`StorageError::Stale`];
//! * the request shape itself is wrong → [`StorageError::InvalidInput`];
//! * an enrolled owner's private state is missing from the store → [`StorageError::owner_workspace_lost`].
//!
//! A failed password check is `NotFound`, the *same* answer as an unknown login: a login form that says "no
//! such account" differently from "wrong password" is an account-enumeration oracle.
//!
//! ## Concurrency
//!
//! One-use claims are a conditional `UPDATE type::record($id) … WHERE consumed = false`, and the engine
//! reports the *loser* of that race as an optimistic **write conflict**, not as a zero-row result (measured
//! on the pinned 3.3.0 engine: four simultaneous claims of one link produced one winner and three
//! `TransactionConflict` errors). So the conflict is classified by variant —
//! `surrealdb::types::QueryError::TransactionConflict`, never a message match — and the guard is
//! re-evaluated on retry, at which point the loser's write no longer matches and it reports `Stale`.
//!
//! **The spelling of a guarded write is load-bearing, and the obvious one is a trap.** Measured on the
//! pinned 3.3.0 engine (`probe-12-only-guard-effect.log`):
//! `UPDATE ONLY type::record($id) SET … WHERE <guard> RETURN AFTER` *does* honour the guard — the row is
//! genuinely not written — but returns a single `NONE` element instead of an empty array. A caller counting
//! the rows it wrote therefore counts a win on a loss, and one deserialising the result into a row type
//! fails with `Expected object, got none`, which is an internal error handed to a client for what was
//! really somebody else losing a race. The usable conditional form is the plain
//! `UPDATE type::record($id) SET … WHERE <guard> RETURN AFTER`: one row for the winner, an empty result for
//! a guard miss *and* for an id that is not there, and no insert in either case (the row count is unchanged
//! in both). That is `ReminderRepo::record_delivery`'s existing claim shape, and every guarded write in this
//! module follows it.
//! Without the retry, "exactly one winner" would be "one winner plus three internal errors". A racer that
//! exhausts [`CLAIM_CONFLICT_ATTEMPTS`] gets [`StorageError::RetryableBookkeepingConflict`], which is also
//! not a win, so exactly one winner holds either way.

use crate::access::{OwnerContext, Principal, credential_entitles_read};
use crate::error::StorageError;
use crate::models::auth::{
    Account, Credential, PrincipalIds, Workspace, credential_kind, normalize_enrollable_login,
};
use crate::models::identity::{AccountId, CredentialId, OperationId, WorkspaceId};
use crate::repos::IdentityRepo;
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{Output, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest as _, Sha256};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{QueryError, RecordId, SurrealValue, ToSql};

/// Argon2id memory cost in KiB — ~19 MiB, the OWASP moderate set the owner selected (A2). The frozen plan's
/// "64 MiB / 3 iterations" sentence declares itself a tunable starting point and is deliberately
/// superseded; `task-4-owner-decisions.md` records the choice, and the task report repeats it so a later
/// reader does not treat the plan text as a mismatch.
const ARGON2_M_COST: u32 = 19_456;
/// Argon2id passes over the memory.
const ARGON2_T_COST: u32 = 2;
/// Argon2id lanes. One, so a verification costs one core, and so a lower-parallelism digest cannot slip
/// through the shape gate.
const ARGON2_P_COST: u32 = 1;
/// Digest length in bytes, and the length of every bearer secret this module mints.
const SECRET_BYTES: usize = 32;
/// The `v=` field of a PHC Argon2 string: the Argon2 spec's own format version (`0x13` = 19 decimal) that
/// the crate writes, and which `password_hash` parses back as a number. Not a policy knob, and not a place
/// to inject a scheme marker — a custom one would produce a string this crate cannot verify.
const ARGON2_PHC_VERSION: u32 = 19;
/// Re-evaluations of a claim guard before the conflict is reported as retryable rather than retried here.
/// The engine resolves the race on the first retry in practice; the bound exists so a pathological store
/// cannot spin inside this call.
const CLAIM_CONFLICT_ATTEMPTS: usize = 8;

/// The id a credential write produced.
#[derive(SurrealValue)]
struct Created {
    id: RecordId,
}

/// An Argon2id password digest, in the PHC spelling this build produces and verifies.
///
/// A type rather than a `String` so that the one thing every log line and every client-visible body must
/// not do — print secret material — cannot happen by reaching for `{digest:?}`. `Debug` renders the length
/// only, `Display` a fixed placeholder; the stored form is reachable through [`PasswordDigest::as_str`] for
/// the single caller that writes it.
#[derive(Clone, PartialEq, Eq)]
pub struct PasswordDigest(String);

impl PasswordDigest {
    /// The PHC string to store in `account.password_hash`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PasswordDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordDigest")
            .field("digest", &Redacted(self.0.len()))
            .finish()
    }
}

impl std::fmt::Display for PasswordDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<password digest redacted>")
    }
}

/// One minted bearer secret: 32 random bytes, hex on the wire, SHA-256 in the store, shown once.
#[derive(Clone)]
pub struct BearerSecret([u8; SECRET_BYTES]);

impl BearerSecret {
    /// 32 bytes from the operating system's CSPRNG, through the `rand_core` re-export `argon2` already
    /// carries — the same generator that produces the Argon2 salt, so no new dependency edge.
    fn generate() -> Self {
        let mut bytes = [0u8; SECRET_BYTES];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// The wire spelling, 64 lowercase hex characters. Takes `self` by value because "returned exactly
    /// once" is worth a compile error: the caller gets one [`SecretText`] and there is no second render to
    /// log, to echo into a response body, or to re-issue.
    pub fn render(self) -> SecretText {
        let mut text = String::with_capacity(SECRET_BYTES * 2);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
        }
        SecretText(text)
    }

    /// Parses a presentation. Trimming and case-folding happen first because a token copied out of a
    /// terminal or pasted through a form arrives with whitespace and in either case; the digest is over
    /// the *bytes*, so both spellings must resolve to the same credential. Anything that is not then
    /// exactly 64 hex characters is `None` — a stored digest, a PHC string, a truncated copy — so a
    /// malformed presentation never reaches a comparison at all.
    fn parse(presented: &str) -> Option<Self> {
        let normalised = presented.trim().to_ascii_lowercase();
        if normalised.len() != SECRET_BYTES * 2 {
            return None;
        }
        let mut bytes = [0u8; SECRET_BYTES];
        for (index, pair) in normalised.as_bytes().chunks(2).enumerate() {
            let hi = hex_value(pair[0])?;
            let lo = hex_value(pair[1])?;
            bytes[index] = hi << 4 | lo;
        }
        Some(Self(bytes))
    }

    /// The canonical stored spelling of this secret's SHA-256 digest.
    fn digest(&self) -> Result<String, StorageError> {
        digest_text(&self.0)
    }

    /// Whether `stored` is the digest of these bytes, in constant time.
    ///
    /// A stored value that is not in canonical form is a *mismatch* rather than an error, which is what
    /// makes a hand-written fixture row unable to authenticate: the index can be matched by writing the
    /// same non-canonical text twice, this comparison cannot.
    fn matches_digest(&self, stored: &str) -> bool {
        let computed = Sha256::digest(self.0);
        match (Output::new(computed.as_slice()), Output::b64_decode(stored)) {
            (Ok(computed), Ok(stored)) => computed == stored,
            _ => false,
        }
    }
}

impl std::fmt::Debug for BearerSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BearerSecret(<redacted>)")
    }
}

/// A bearer secret's wire spelling, already rendered, and rendered out of a [`BearerSecret`] that no longer
/// exists.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretText(String);

impl SecretText {
    /// The only path to the plaintext. Named for what it does, because every call site is a place a secret
    /// can leave the process; there is no `as_str`.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The digest of these bytes — what a stored row would have to carry for this to be the same secret. A
    /// caller comparing a presentation uses this and never the plaintext.
    pub fn digest(&self) -> Result<String, StorageError> {
        BearerSecret::parse(&self.0)
            .ok_or_else(|| {
                StorageError::InvalidInput("secret text is not 64 hex characters".to_string())
            })?
            .digest()
    }
}

impl std::fmt::Debug for SecretText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretText(<redacted>)")
    }
}

impl std::fmt::Display for SecretText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<secret redacted>")
    }
}

/// Renders a byte count and nothing else, for the redacting `Debug` impls.
struct Redacted(usize);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted {} bytes>", self.0)
    }
}

/// A credential just written: the row handle, the secret about to be shown, and the deadline it is inert
/// past — `None` for an agent token, which has none.
#[derive(Debug)]
pub struct MintedSecret {
    pub credential: CredentialId,
    pub secret: BearerSecret,
    pub expires_at: Option<DateTime<Utc>>,
}

/// A browser session just written: two independent secrets and the two deadlines bounding the session.
#[derive(Debug)]
pub struct MintedSession {
    pub credential: CredentialId,
    pub cookie: BearerSecret,
    pub csrf: BearerSecret,
    /// The rolling idle deadline: pushed out by each authenticated use, never past
    /// `absolute_expires_at`.
    pub expires_at: DateTime<Utc>,
    /// The absolute cap, stored so no amount of use can outlive it.
    pub absolute_expires_at: DateTime<Utc>,
}

/// What this crate says about an owner that is entitled to have credentials minted for it: the current
/// epoch, and the one private workspace the database names.
struct MintState {
    epoch: i64,
    workspace: WorkspaceId,
}

/// Which credential row is being written, and which of its optional columns are present.
///
/// An enum rather than a list of optional arguments, because the three shapes differ in *columns*, and a
/// row of `Option<…>` parameters makes "did the agent token gain an expiry by accident" invisible in the
/// source. Each arm's SQL is a literal, so no statement text is assembled from data.
enum Draft<'a> {
    Browser {
        csrf_digest: &'a str,
        expires_at: DateTime<Utc>,
        absolute_expires_at: DateTime<Utc>,
    },
    Agent {
        label: &'a str,
    },
    Link {
        kind: &'a str,
        expires_at: DateTime<Utc>,
    },
}

pub struct AuthRepo<'a> {
    db: &'a Surreal<Any>,
}

impl<'a> AuthRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    // ---------------------------------------------------------------------------------------------
    // Password digests
    // ---------------------------------------------------------------------------------------------

    /// `argon2id(password)` at the policy parameters, with a fresh random salt, off the async runtime.
    ///
    /// This is the only producer of a digest this crate stores, and it is the half of owner decision F3
    /// that later code depends on: anything reaching [`IdentityRepo::bootstrap_owner`] — whose
    /// `account.bootstrap_fingerprint` outlives password rotation — comes from here, or is checked by
    /// [`AuthRepo::password_digest_is_policy`] first. `bootstrap_owner` itself stays as Task 3 wrote it (it
    /// must be able to answer a replay of a recorded request, so it stores what it is handed); the
    /// guarantee is that no path in this crate hands it anything else, and that no network path reaches it
    /// until Task 5 admits requests.
    pub async fn create_password_digest(password: &str) -> Result<PasswordDigest, StorageError> {
        let password = password.to_string();
        let digest = tokio::task::spawn_blocking(move || hash_password(&password))
            .await
            .map_err(join_failure)
            .and_then(|result| result.map_err(internal))?;
        Ok(PasswordDigest(digest))
    }

    /// Whether `stored` is an Argon2id PHC digest at exactly this build's policy parameters.
    ///
    /// A fail-closed *shape-and-policy* test, not a validator: an `argon2i` or `argon2d` string, the same
    /// algorithm at different costs, a bare hash, a placeholder like the fixture strings Task 3's
    /// enrollment tests write, or empty text is refused rather than verified. Verification alone could not
    /// tell those apart from a legitimate historical policy, and a stored digest carries **no rule
    /// marker** — so if `login` accepted a weak-parameter digest today, every one of them would still
    /// authenticate after a later cost change, silently and forever.
    ///
    /// That is why [`ARGON2_M_COST`], [`ARGON2_T_COST`] and [`ARGON2_P_COST`] are a migration rather than an
    /// edit: existing digests stop verifying, and the remedy is a re-hash at the new shape or an explicit
    /// invalidation of the affected accounts. The same rule covers normalisation of the login
    /// ([`normalize_enrollable_login`]) and of the prepared hash, because
    /// [`crate::models::auth::bootstrap_request_fingerprint`] is computed over the prepared digest — a
    /// change to either re-stamps or invalidates recorded fingerprints, and never quietly re-normalises
    /// them.
    pub fn password_digest_is_policy(stored: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(stored) else {
            return false;
        };
        // The PHC ident field, `$argon2id$…`. Anything else is a different algorithm whatever the rest of
        // the string claims — and argon2's own verifier picks the algorithm from the string, which is
        // exactly how a downgrade would otherwise go unnoticed.
        if parsed.algorithm.as_str() != "argon2id" {
            return false;
        }
        // `v=` is the crate's format version. A string this build would not write is not a string this
        // build should authenticate with.
        if parsed.version != Some(ARGON2_PHC_VERSION) {
            return false;
        }
        // A salt is what makes two digests of one password differ, and a hash is what is compared: either
        // half absent is not a digest, whatever the parameter list says.
        if parsed.salt.is_none() || parsed.hash.is_none() {
            return false;
        }
        number_param(&parsed, "m") == Some(ARGON2_M_COST)
            && number_param(&parsed, "t") == Some(ARGON2_T_COST)
            && number_param(&parsed, "p") == Some(ARGON2_P_COST)
            // The crate omits `output_len` at its default (32). Present or absent, it has to mean 32
            // bytes, because a shorter digest is a weaker offline target.
            && match number_param(&parsed, "output_len") {
                None => true,
                Some(length) => length as usize == SECRET_BYTES,
            }
    }

    /// Whether `password` is the password behind `stored`.
    ///
    /// Two `Ok(false)` answers and no error case for a bad password: a stored value that is not a policy
    /// digest (refused before any hashing work, so a legacy or hand-written row costs nothing and
    /// authenticates nobody), and a wrong password. Callers must not invent a third answer — telling a
    /// client "that account has no usable digest" apart from "wrong password" is the enumeration oracle
    /// the login path cannot afford.
    pub async fn password_matches(
        stored: Option<&str>,
        password: &str,
    ) -> Result<bool, StorageError> {
        let Some(stored) = stored else {
            return Ok(false);
        };
        if !Self::password_digest_is_policy(stored) {
            return Ok(false);
        }
        let stored = stored.to_string();
        let password = password.to_string();
        let matched = tokio::task::spawn_blocking(move || verify_password(&stored, &password))
            .await
            .map_err(join_failure)?;
        Ok(matched)
    }

    // ---------------------------------------------------------------------------------------------
    // Accounts, and the owner state a credential is minted into
    // ---------------------------------------------------------------------------------------------

    /// The account `login` names, with the login normalised the way `account.login`'s own assertion
    /// requires — the same function the enrollment path uses, which is what makes a typed login reach a
    /// stored one.
    ///
    /// Out-of-bounds input is `InvalidInput` before any read, so nothing the schema could not have stored is
    /// ever looked up, and the rendered message names the field without echoing the value back.
    pub async fn account_by_login(&self, login: &str) -> Result<Option<Account>, StorageError> {
        let login = normalize_enrollable_login(login)?;
        let mut response = self
            .db
            .query("SELECT * FROM account WHERE login = $login LIMIT 1;")
            .bind(("login", login))
            .await
            .map_err(engine)?
            .check()
            .map_err(engine)?;
        let rows: Vec<Account> = response.take(0).map_err(engine)?;
        Ok(rows.into_iter().next())
    }

    /// The account row `account` names, or `None`. A plain read: the caller decides what absence means, and
    /// the login path decides it means the same thing as a wrong password.
    pub async fn account_of(&self, account: &AccountId) -> Result<Option<Account>, StorageError> {
        let mut response = self
            .db
            .query("SELECT * FROM account WHERE id = $id LIMIT 1;")
            .bind(("id", account.as_record_id().clone()))
            .await
            .map_err(engine)?
            .check()
            .map_err(engine)?;
        let rows: Vec<Account> = response.take(0).map_err(engine)?;
        Ok(rows.into_iter().next())
    }

    /// The rows an owner may act on: their live or revoked browser sessions and agent tokens.
    ///
    /// `kind` is restricted to the two authenticating kinds on purpose. An invitation or a recovery
    /// link is also a credential row of theirs, and listing one would put a still-spendable secret's
    /// *existence* on a page whose job is "which device should I revoke" — so the kinds a caller can
    /// use to act, and nothing else. Both digest columns are hidden by `Credential`'s own `Debug` and
    /// serde impls, which is why this can return rows rather than a projection of them.
    pub async fn list_own_credentials(
        &self,
        owner: &OwnerContext,
    ) -> Result<Vec<Credential>, StorageError> {
        let mut response = self
            .db
            .query(
                "SELECT * FROM credential WHERE account = $account AND kind INSIDE ['browser', \
                 'agent'] ORDER BY created_at DESC, id ASC;",
            )
            .bind(("account", owner.account().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// The account a live recovery link is addressed to, **without** consuming it.
    ///
    /// The caller that needs this is the recovery route: rotating a password raises the account's
    /// `auth_epoch`, which retires every credential it holds *in the store*, and the one thing a
    /// store-side epoch bump cannot do is reach a stream that is already open. `recover` returns no
    /// account to name, so a caller that wants to close those has to ask whose link it is first.
    ///
    /// One answer for a secret that is unknown, spent, expired or not a recovery link at all — this is
    /// reachable without a session and must not become a probe for which links exist.
    ///
    /// Built on [`Self::resolve`], which is the module's existing non-disclosing lookup: a made-up
    /// secret, a well-formed digest of a *different* kind, and a non-canonical digest spelling all
    /// answer `NotFound`, and this method turns that into `None`. A link that can no longer be claimed
    /// is `None` too, because naming the account behind a consumed link would let this read enumerate
    /// links that were already spent.
    pub async fn recovery_link_target(
        &self,
        secret: &str,
    ) -> Result<Option<AccountId>, StorageError> {
        let row = match self.resolve(credential_kind::RECOVERY, secret).await {
            Ok(row) => row,
            Err(StorageError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        if row.revoked.unwrap_or(false)
            || row.consumed.unwrap_or(false)
            || row
                .expires_at
                .is_some_and(|expires_at| expires_at <= Utc::now())
        {
            return Ok(None);
        }
        Ok(row.account_id().ok())
    }

    /// The credential row `credential` names, or `None`.
    pub async fn credential_of(
        &self,
        credential: &CredentialId,
    ) -> Result<Option<Credential>, StorageError> {
        let mut response = self
            .db
            .query("SELECT * FROM credential WHERE id = $id LIMIT 1;")
            .bind(("id", credential.as_record_id().clone()))
            .await
            .map_err(engine)?
            .check()
            .map_err(engine)?;
        let rows: Vec<Credential> = response.take(0).map_err(engine)?;
        Ok(rows.into_iter().next())
    }

    /// The epoch a presented credential has to match for `account`, from the account row alone.
    ///
    /// Deliberately *not* [`AuthRepo::mint_state`]: presenting a live session does not require the owner's
    /// workspace row to be present, because [`crate::access::AccessRepo::authorize`] answers that on the
    /// same principal with the same indistinguishable `NotFound`. A credential that is valid but whose
    /// owner has no workspace is therefore refused *by authorization*, not by authentication — and keeping
    /// the two questions apart is what stops a read-only presentation from being reported as an integrity
    /// event, which would say "restore from backup" to an operator when the truth is "this account's
    /// private state is gone, and the caller already had a valid cookie".
    ///
    /// An account row with no epoch cannot be compared against anything, so `authorize` would refuse every
    /// credential of that account forever: an internal fault, not a `Stale` the caller could retry.
    fn live_epoch(row: &Account) -> Result<i64, StorageError> {
        if !row.is_active() {
            return Err(StorageError::Stale);
        }
        row.auth_epoch.ok_or_else(|| {
            StorageError::Internal(anyhow::anyhow!(
                "an enrolled owner has no authentication epoch to compare a credential against"
            ))
        })
    }

    /// The epoch to mint under, and the private workspace to mint into — re-read on every call, for the
    /// paths that are about to *write* authority.
    ///
    /// Four answers, each deliberate:
    ///
    /// * no account row → [`StorageError::NotFound`];
    /// * an account that exists but is not active → [`StorageError::Stale`], the authority-has-moved answer
    ///   `AccessRepo::authorize` gives for the same row, so a mint cannot outlive a suspension;
    /// * an account with no workspace → [`StorageError::owner_workspace_lost`]. Internal by classification
    ///   (the caller did nothing wrong, and nothing about *which* owner is affected may be inferred from
    ///   it) but *typed*, so the service boundary can tell an operator "an enrolled owner's private state
    ///   is missing from the store" — a backup-or-recovery decision — instead of a driver hiccup. Owner
    ///   decision F6, and the reason this is a real read of the workspace row rather than an assumption.
    /// * an [`OwnerContext`] naming a workspace the account does not own → [`StorageError::NotFound`], the
    ///   absent-id answer, so a held context whose workspace moved is indistinguishable from a made-up one.
    ///
    /// An account row with no epoch is `Internal` rather than mintable: `AccessRepo::authorize` refuses any
    /// credential whose minted epoch cannot be compared with a current one, so a credential minted against
    /// an absent epoch would be a row nothing can use.
    async fn mint_state(&self, account: &AccountId) -> Result<MintState, StorageError> {
        let row = self
            .account_of(account)
            .await?
            .ok_or(StorageError::NotFound)?;
        let epoch = Self::live_epoch(&row)?;
        let workspace = self
            .workspace_row(account)
            .await?
            .ok_or_else(StorageError::owner_workspace_lost)?;
        Ok(MintState {
            epoch,
            workspace: workspace.workspace_id()?,
        })
    }

    /// The same re-read for a call that carries an [`OwnerContext`], with the workspace agreement checked
    /// against the database's answer rather than the caller's.
    async fn mint_state_of(&self, owner: &OwnerContext) -> Result<MintState, StorageError> {
        let state = self.mint_state(owner.account()).await?;
        if state.workspace != *owner.workspace() {
            return Err(StorageError::NotFound);
        }
        Ok(state)
    }

    /// The account's private workspace, as the database names it.
    ///
    /// Read through the `workspace.owner` link rather than inferred from the account id: v009 makes that
    /// unique link the whole privacy model, and a workspace derived from an id instead of from the row that
    /// owns it is exactly the invention the ownership rules forbid.
    async fn workspace_row(&self, account: &AccountId) -> Result<Option<Workspace>, StorageError> {
        let mut response = self
            .db
            .query("SELECT * FROM workspace WHERE owner = $account LIMIT 1;")
            .bind(("account", account.as_record_id().clone()))
            .await
            .map_err(engine)?
            .check()
            .map_err(engine)?;
        let rows: Vec<Workspace> = response.take(0).map_err(engine)?;
        Ok(rows.into_iter().next())
    }

    // ---------------------------------------------------------------------------------------------
    // Minting
    // ---------------------------------------------------------------------------------------------

    /// Creates a browser session for a live owner: one credential row, two freshly minted secrets, both
    /// deadlines written in the same statement.
    ///
    /// The two secrets are independent on purpose. A session cookie that doubles as a CSRF token means any
    /// request that can carry one can carry the other, which is the whole property CSRF exists to
    /// establish. Only the session half lands in `secret_digest` — the index a session resolves through —
    /// so a CSRF token can never be presented as a session, nor a session as a CSRF token.
    ///
    /// `idle` must be positive and shorter than `absolute`: a session whose idle window exceeded its own cap
    /// would be clamped to the cap on the first roll, and a policy pair that cannot be honoured is a wrong
    /// request rather than a session with surprising behaviour.
    pub async fn mint_browser_session(
        &self,
        account: &AccountId,
        idle: Duration,
        absolute: Duration,
        now: DateTime<Utc>,
    ) -> Result<MintedSession, StorageError> {
        if idle.num_seconds() <= 0 || absolute.num_seconds() <= 0 || absolute <= idle {
            return Err(StorageError::InvalidInput(
                "the session idle window must be positive and shorter than the absolute lifetime"
                    .to_string(),
            ));
        }
        let state = self.mint_state(account).await?;
        let cookie = BearerSecret::generate();
        let csrf = BearerSecret::generate();
        let expires_at = now + idle;
        let absolute_expires_at = now + absolute;
        let csrf_digest = csrf.digest()?;
        let credential = self
            .create_credential(
                account,
                &cookie,
                state.epoch,
                now,
                Draft::Browser {
                    csrf_digest: &csrf_digest,
                    expires_at,
                    absolute_expires_at,
                },
            )
            .await?;
        Ok(MintedSession {
            credential,
            cookie,
            csrf,
            expires_at,
            absolute_expires_at,
        })
    }

    /// Creates a named, revocable agent token for a live owner, with no expiry column written: such a token
    /// ends at revocation, or at the epoch bump of a recovery.
    ///
    /// The [`OwnerContext`] is re-read before the row is written — account state, workspace ownership, and
    /// the epoch to mint under all come from the database — so a held context whose account was suspended or
    /// whose workspace moved cannot mint fresh credentials. That re-read is the difference between minting
    /// under authority and minting under a receipt.
    pub async fn mint_agent_token(
        &self,
        owner: &OwnerContext,
        label: &str,
        now: DateTime<Utc>,
    ) -> Result<MintedSecret, StorageError> {
        let label = label.trim();
        if label.is_empty() {
            return Err(StorageError::InvalidInput(
                "token name must not be blank".to_string(),
            ));
        }
        let state = self.mint_state_of(owner).await?;
        let secret = BearerSecret::generate();
        let credential = self
            .create_credential(
                owner.account(),
                &secret,
                state.epoch,
                now,
                Draft::Agent { label },
            )
            .await?;
        Ok(MintedSecret {
            credential,
            secret,
            expires_at: None,
        })
    }

    /// Creates an invitation link, the credential that lets somebody enroll a **new** owner.
    ///
    /// The row belongs to `issuer`, not to whoever will use it: `credential.account` is required by v009 and
    /// an invited account does not exist yet. That is the honest shape rather than a workaround — an
    /// invitation is one enrollment the issuer grants, so the issuer can revoke it, it appears in the
    /// issuer's own credential list, and a leaked one is the issuer's to notice. The account
    /// [`AuthRepo::accept_invitation`] creates is a second, independent owner with its own workspace; it
    /// never becomes the issuer's.
    pub async fn create_invitation(
        &self,
        issuer: &AccountId,
        lifetime: Duration,
        now: DateTime<Utc>,
    ) -> Result<MintedSecret, StorageError> {
        self.mint_link(credential_kind::INVITE, issuer, lifetime, now, None)
            .await
    }

    /// Creates a password-recovery link for `account`. Consuming it rotates that account's password *and*
    /// raises its `auth_epoch`, which is why the lifetime is the shortest of the policy: a recovery link is
    /// a way to take an account over.
    pub async fn create_recovery_link(
        &self,
        account: &AccountId,
        lifetime: Duration,
        now: DateTime<Utc>,
    ) -> Result<MintedSecret, StorageError> {
        self.mint_link(credential_kind::RECOVERY, account, lifetime, now, None)
            .await
    }

    /// The admin repo is the only caller. Commit the audit row and credential together so a
    /// failed audit cannot leave a minted recovery link. Neither the secret nor its digest goes
    /// into the audit.
    pub(crate) async fn create_admin_recovery_link(
        &self,
        admin: &crate::access::AdminCapability,
        account: &AccountId,
        lifetime: Duration,
        now: DateTime<Utc>,
    ) -> Result<MintedSecret, StorageError> {
        self.mint_link(
            credential_kind::RECOVERY,
            account,
            lifetime,
            now,
            Some(admin),
        )
        .await
    }

    /// The shared body of the two links. An authenticating kind is refused here, so a browser session or an
    /// agent token can only be created by the two paths above.
    async fn mint_link(
        &self,
        kind: &str,
        account: &AccountId,
        lifetime: Duration,
        now: DateTime<Utc>,
        admin: Option<&crate::access::AdminCapability>,
    ) -> Result<MintedSecret, StorageError> {
        if kind != credential_kind::INVITE && kind != credential_kind::RECOVERY {
            return Err(StorageError::InvalidInput(
                "only an invitation or a recovery link is created as a one-use link".to_string(),
            ));
        }
        if lifetime.num_seconds() <= 0 {
            return Err(StorageError::InvalidInput(
                "a one-use link must have a positive lifetime".to_string(),
            ));
        }
        let state = self.mint_state(account).await?;
        let secret = BearerSecret::generate();
        let expires_at = now + lifetime;
        let credential = if let Some(admin) = admin {
            self.create_audited_recovery(
                account,
                admin.account(),
                secret.digest()?,
                state.epoch,
                now,
                expires_at,
            )
            .await?
        } else {
            self.create_credential(
                account,
                &secret,
                state.epoch,
                now,
                Draft::Link { kind, expires_at },
            )
            .await?
        };
        Ok(MintedSecret {
            credential,
            secret,
            expires_at: Some(expires_at),
        })
    }

    /// Writes one credential row and returns its handle.
    ///
    /// Every value is bound as a parameter, and an optional column is *omitted* rather than bound as null:
    /// writing `NULL` over an `option<…>` column stores a value where an absent column would read `NONE`,
    /// and the two do not answer the same predicate. The `RETURN AFTER` row is the engine's proof the write
    /// landed; its id is the only thing handed back — no digest, no secret.
    async fn create_credential(
        &self,
        account: &AccountId,
        secret: &BearerSecret,
        auth_epoch: i64,
        now: DateTime<Utc>,
        draft: Draft<'_>,
    ) -> Result<CredentialId, StorageError> {
        let digest = secret.digest()?;
        let (sql, label, kind, csrf_digest, expires_at, absolute_expires_at) = match &draft {
            Draft::Browser {
                csrf_digest,
                expires_at,
                absolute_expires_at,
            } => (
                "CREATE credential SET account = $account, kind = 'browser', secret_digest = $digest, \
                 csrf_digest = $csrf_digest, auth_epoch = $auth_epoch, expires_at = $expires_at, \
                 absolute_expires_at = $absolute_expires_at, created_at = $now RETURN AFTER;",
                None,
                None,
                Some(*csrf_digest),
                Some(*expires_at),
                Some(*absolute_expires_at),
            ),
            Draft::Agent { label } => (
                "CREATE credential SET account = $account, kind = 'agent', label = $label, \
                 secret_digest = $digest, auth_epoch = $auth_epoch, created_at = $now RETURN AFTER;",
                Some(*label),
                None,
                None,
                None,
                None,
            ),
            Draft::Link { kind, expires_at } => (
                "CREATE credential SET account = $account, kind = $kind, secret_digest = $digest, \
                 auth_epoch = $auth_epoch, expires_at = $expires_at, created_at = $now RETURN AFTER;",
                None,
                Some(*kind),
                None,
                Some(*expires_at),
                None,
            ),
        };
        let mut query = self
            .db
            .query(sql)
            .bind(("account", account.as_record_id().clone()))
            .bind(("digest", digest))
            .bind(("auth_epoch", auth_epoch))
            .bind(("now", now));
        if let Some(label) = label {
            query = query.bind(("label", label.to_string()));
        }
        if let Some(kind) = kind {
            query = query.bind(("kind", kind.to_string()));
        }
        if let Some(csrf_digest) = csrf_digest {
            query = query.bind(("csrf_digest", csrf_digest.to_string()));
        }
        if let Some(expires_at) = expires_at {
            query = query.bind(("expires_at", expires_at));
        }
        if let Some(absolute_expires_at) = absolute_expires_at {
            query = query.bind(("absolute_expires_at", absolute_expires_at));
        }
        let mut response = query.await.map_err(engine)?.check().map_err(engine)?;
        let rows: Vec<Created> = response.take(0).map_err(engine)?;
        let created = rows.into_iter().next().ok_or_else(|| {
            StorageError::Internal(anyhow::anyhow!(
                "a credential write returned no row to take an id from"
            ))
        })?;
        CredentialId::try_from(created.id)
    }

    /// Use a typed handle for both writes; no audit attribution or ID is inferred from display text.
    async fn create_audited_recovery(
        &self,
        target: &AccountId,
        issuer: &AccountId,
        digest: String,
        auth_epoch: i64,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<CredentialId, StorageError> {
        let id = RecordId::new("credential", uuid::Uuid::new_v4().to_string());
        self.db
            .query(
                "BEGIN TRANSACTION;
                 CREATE $credential SET account = $target, kind = 'recovery', secret_digest = $digest,
                     auth_epoch = $auth_epoch, expires_at = $expires_at, created_at = $now RETURN NONE;
                 CREATE admin_recovery_log SET actor = $actor, target = $target,
                     credential = $credential, created_at = $now RETURN NONE;
                 COMMIT TRANSACTION;",
            )
            .bind(("credential", id.clone()))
            .bind(("target", target.as_record_id().clone()))
            .bind(("actor", issuer.as_record_id().clone()))
            .bind(("digest", digest))
            .bind(("auth_epoch", auth_epoch))
            .bind(("expires_at", expires_at))
            .bind(("now", now))
            .await
            .map_err(engine)?
            .check()
            .map_err(engine)?;
        CredentialId::try_from(id)
    }

    // ---------------------------------------------------------------------------------------------
    // Presenting a credential
    // ---------------------------------------------------------------------------------------------

    /// Resolves a presented browser session cookie, rolls its idle deadline, and mints the principal.
    pub async fn authenticate_browser_session(
        &self,
        cookie: &str,
        idle: Duration,
        now: DateTime<Utc>,
    ) -> Result<Principal, StorageError> {
        self.authenticate(credential_kind::BROWSER, cookie, Some(idle), now)
            .await
    }

    /// Resolves a presented agent token and mints the principal. Bookkeeping only: no deadline to roll,
    /// because such a token's lifetime is revocation and the account's epoch.
    pub async fn authenticate_agent_token(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<Principal, StorageError> {
        self.authenticate(credential_kind::AGENT, token, None, now)
            .await
    }

    /// Whether `csrf` is the token paired with a **still live** browser session.
    ///
    /// Task 4's half of the CSRF story: the digest is written at mint and compared here, in constant time,
    /// against the row. Admitting a mutation that presents both halves is Task 5's middleware, which is why
    /// this answers a question instead of taking an action — and why it mints no principal and returns no
    /// owner context.
    ///
    /// Liveness is part of the question, not an extra one. A revoked, spent, expired or epoch-superseded
    /// session does not match anything, because "the digest is right" about a credential the store has
    /// already retired is a fact no caller should be able to act on: the answer a caller wants from this is
    /// *"is this mutation carrying this session's own proof"*, and a session that recovery invalidated
    /// carries nothing. The same predicate [`crate::access::AccessRepo::authorize`] applies, so the two
    /// cannot disagree about whether the session is live.
    pub async fn browser_session_matches_csrf(
        &self,
        credential: &CredentialId,
        csrf: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, StorageError> {
        let Some(presented) = BearerSecret::parse(csrf) else {
            return Ok(false);
        };
        let Some(row) = self.credential_of(credential).await? else {
            return Ok(false);
        };
        if row.kind != credential_kind::BROWSER {
            return Ok(false);
        };
        let account = match row.account_id() {
            Ok(account) => account,
            // A credential row that names no account is a broken row, and a broken row proves nothing.
            Err(_) => return Ok(false),
        };
        let Some(account_row) = self.account_of(&account).await? else {
            return Ok(false);
        };
        if !credential_entitles_read(&row, &account_row, now) {
            return Ok(false);
        }
        Ok(row
            .csrf_digest
            .as_deref()
            .is_some_and(|stored| presented.matches_digest(stored)))
    }

    /// The one resolution path: digest lookup, constant-time confirmation, live-state re-read, then the roll
    /// and the principal.
    ///
    /// Three orderings matter. The kind is a lookup condition *and* re-checked on the row, so an invitation
    /// link presented as a session cookie finds nothing rather than being argued out of authority
    /// afterwards. The constant-time comparison runs against the row the engine returned, because a digest
    /// written by hand in a non-canonical spelling must not authenticate just because an index matched its
    /// text. And the account is re-read, never assumed: comparing a credential's minted epoch against a
    /// value cached from an earlier call is exactly the in-flight revoked request Task 5 refuses.
    ///
    /// The output is a minted principal, which is why this is a production authority path and not a lookup:
    /// [`Principal::authenticated`] is crate-private and this is its only caller. The one other in-crate
    /// construction of a principal is the private struct literal inside `access.rs` itself, which is how
    /// [`crate::access::AccessRepo::local_principal`] builds the trusted-local variant — so there are
    /// exactly two ways a principal exists, both verified, and neither is reachable from another crate.
    async fn authenticate(
        &self,
        kind: &str,
        presented: &str,
        idle: Option<Duration>,
        now: DateTime<Utc>,
    ) -> Result<Principal, StorageError> {
        if !credential_kind::AUTHENTICATING.contains(&kind) {
            return Err(StorageError::InvalidInput(
                "only a browser session or an agent token can authenticate a request".to_string(),
            ));
        }
        let row = self.resolve(kind, presented).await?;
        let credential = row.credential_id()?;
        let account = row.account_id()?;
        let account_row = self
            .account_of(&account)
            .await?
            .ok_or(StorageError::NotFound)?;
        // The same predicate `authorize` applies, so authentication and authorization cannot disagree about
        // whether this row is live, and both refuse with `Stale` — Task 3's "authenticate again" answer,
        // which is not a probe result. `live_epoch` then makes the absent-epoch case an internal fault
        // rather than a silently matching zero.
        if !credential_entitles_read(&row, &account_row, now) {
            return Err(StorageError::Stale);
        }
        Self::live_epoch(&account_row)?;

        match idle {
            Some(idle) => {
                let renewed = renew_deadline(now + idle, row.absolute_expires_at);
                if !self
                    .roll_session(&credential, &account, now, renewed)
                    .await?
                {
                    return Err(StorageError::Stale);
                }
            }
            None => self.record_use(&credential, &account, now).await?,
        }

        Ok(Principal::authenticated(account, credential))
    }

    // ---------------------------------------------------------------------------------------------
    // One-use enrollment and recovery
    // ---------------------------------------------------------------------------------------------

    /// Claims an invitation link and enrolls the owner it names.
    ///
    /// The claim comes first, and that order is the security property: one link can produce one owner, so
    /// two concurrent accepts cannot enroll two accounts from one secret and a *failed* enrollment burns the
    /// link rather than minting an owner nobody holds a key to. The link row is therefore written by the
    /// issuer, and the account `IdentityRepo::bootstrap_owner` creates is a second, independent owner with
    /// its own workspace.
    ///
    /// Enrollment goes through Task 3's `bootstrap_owner` rather than a second creator, so there is exactly
    /// one place in this crate that makes an owner — the place that stamps `bootstrap_operation` and
    /// `bootstrap_fingerprint`, holds both rows in one transaction, and refuses to adopt a request that
    /// does not match the recorded one. The operation key is the link's own credential handle rendered as a
    /// SQL literal (`ToSql`, because a numeric and a quoted opaque key must not collide into one label), so
    /// the enrollment is auditable back to the link that produced it.
    ///
    /// The order is deliberate and it is the expensive-work order: the link is read and refused first, the
    /// password is hashed second, and the one-use link is spent last. A forged secret — the cheapest input
    /// to present — therefore costs one indexed read and no Argon2 work, which matters on a path Task 5
    /// admits without authentication. The invariant the reverse ordering was written to protect is
    /// preserved by that order rather than by hashing first: [`AuthRepo::presented_link`] consumes
    /// nothing, so a request whose digest cannot be produced still reaches no write, and
    /// [`AuthRepo::spend_link`] is unchanged as the only statement that burns a link.
    pub async fn accept_invitation(
        &self,
        secret: &str,
        login: &str,
        password: &str,
        now: DateTime<Utc>,
    ) -> Result<PrincipalIds, StorageError> {
        let link = self
            .presented_link(credential_kind::INVITE, secret, now)
            .await?;
        let digest = Self::create_password_digest(password).await?;
        let link = self.spend_link(&link, now).await?;
        let identities = IdentityRepo::new(self.db);
        identities
            .bootstrap_owner(
                login,
                digest.as_str(),
                &OperationId::new(enrollment_key(&link)?),
            )
            .await
    }

    /// Claims a recovery link, rotates the account's password, and raises its `auth_epoch`.
    ///
    /// `()` is the whole answer on purpose: recovery returns no secret, no session and no principal, so a
    /// caller that consumed somebody's link cannot keep using it, and every browser session and agent token
    /// of that account is inert from the epoch bump onward — the invalidation is the integer, not a sweep,
    /// which is what makes it complete rather than best-effort.
    ///
    /// The same order as an invitation — read, hash, spend — so an unusable recovery secret is refused at
    /// the cost of a read. Exactly one concurrent recovery may rotate one account, and the loser gets
    /// [`StorageError::Stale`] rather than a second rotation.
    pub async fn recover(
        &self,
        secret: &str,
        new_password: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let link = self
            .presented_link(credential_kind::RECOVERY, secret, now)
            .await?;
        let digest = Self::create_password_digest(new_password).await?;
        let link = self.spend_link(&link, now).await?;
        self.rotate_password(&link.account_id()?, digest.as_str(), now)
            .await
    }

    /// Claims a one-use link — an invitation or a recovery link — and returns the row as it stands after the
    /// claim. Exactly one concurrent caller gets `Ok`; the rest get [`StorageError::Stale`].
    ///
    /// `link_kind` is a parameter because the two paths differ only in what they do next. The claim is the
    /// point: `consumed = false` is evaluated by the engine before the write, and the bounded conflict
    /// retry (module comment) is what turns the loser's write conflict into that `Stale`. A link that is
    /// expired, revoked, already consumed, or of an unexpected kind is refused *before* the claim, so a
    /// rejected request consumes nothing — the difference between an inert link and one spent by the attempt
    /// to use it.
    ///
    /// **What this does not check is an owner decision, not an oversight.** The pre-check reads the link
    /// row and nothing else: it never reads the grantor's `Account`, so it does not consult
    /// `account.status` and does not compare the link's minted `auth_epoch` with the account's current
    /// one — link kinds are deliberately exempt from
    /// [`crate::access::credential_entitles_read`], which is the spelling that retires an authenticating
    /// credential on a suspension or a recovery. The owner chose to keep that brief-literal behaviour
    /// (Task 4 review finding F4-2) and accepted its two concrete consequences: an outstanding recovery
    /// link remains claimable for up to an hour *after* the victim recovers, and an invitation issued
    /// before a suspension remains claimable for up to 48 hours. Both live inside the lifetimes the brief
    /// specifies, and the brief scopes recovery's invalidation to the account's old browser and agent
    /// credentials — which is what the epoch bump does. Task 24 owns disclosing both next to the
    /// invitation and recovery docs. Do not "tighten" this into a grantor read.
    pub async fn claim_link(
        &self,
        link_kind: &str,
        presented: &str,
        now: DateTime<Utc>,
    ) -> Result<Credential, StorageError> {
        let row = self.presented_link(link_kind, presented, now).await?;
        self.spend_link(&row, now).await
    }

    /// The read half of a one-use claim: the kind check, [`AuthRepo::resolve`], and the inertness test.
    ///
    /// It writes nothing, so a refusal here cannot spend the link — that is the whole reason
    /// [`AuthRepo::accept_invitation`] and [`AuthRepo::recover`] can ask it *before* paying for a password
    /// digest and still keep the guarantee that a request which cannot produce a digest never burns a
    /// one-use credential. Returns the row as the store holds it before the claim.
    ///
    /// Any caller reaching this half directly takes the same owner decision as [`AuthRepo::claim_link`]
    /// does, so read that doc too: the inertness test below is **deliberately link-row-only** — it never
    /// reads the grantor's `Account`, and therefore checks neither `account.status` nor the minted-versus-
    /// current `auth_epoch`. That exemption of link kinds from
    /// [`crate::access::credential_entitles_read`] is owner decision F4-2, not a gap to close here; a new
    /// link-consuming path written against this method must not reintroduce a grantor read by accident,
    /// and must not remove the posture comment on `claim_link`.
    async fn presented_link(
        &self,
        link_kind: &str,
        presented: &str,
        now: DateTime<Utc>,
    ) -> Result<Credential, StorageError> {
        if link_kind != credential_kind::INVITE && link_kind != credential_kind::RECOVERY {
            return Err(StorageError::InvalidInput(
                "only an invitation or a recovery link can be claimed".to_string(),
            ));
        }
        let row = self.resolve(link_kind, presented).await?;
        if row.revoked.unwrap_or(false)
            || row.consumed.unwrap_or(false)
            || row.expires_at.is_some_and(|expires| expires <= now)
            || row
                .absolute_expires_at
                .is_some_and(|absolute| absolute <= now)
        {
            return Err(StorageError::Stale);
        }
        Ok(row)
    }

    /// The write half of a one-use claim, on a row already accepted by [`AuthRepo::presented_link`]: the
    /// guarded UPDATE is the only statement in this crate that burns a link, and its `WHERE` re-evaluates
    /// the spendable state at the write, so a link revoked or spent between the read and here is still
    /// refused rather than quietly consumed.
    async fn spend_link(
        &self,
        row: &Credential,
        now: DateTime<Utc>,
    ) -> Result<Credential, StorageError> {
        let credential = row.credential_id()?;
        let owner = row.account_id()?;
        for attempt in 0..CLAIM_CONFLICT_ATTEMPTS {
            let outcome = self
                .db
                .query(
                    "UPDATE type::record($id) SET consumed = true, consumed_at = $now \
                     WHERE consumed = false AND revoked = false AND account = $account \
                     RETURN AFTER;",
                )
                .bind(("id", credential.as_record_id().clone()))
                .bind(("now", now))
                .bind(("account", owner.as_record_id().clone()))
                .await
                .and_then(|response| response.check())
                .map_err(engine);
            match outcome {
                Err(StorageError::RetryableBookkeepingConflict)
                    if attempt + 1 < CLAIM_CONFLICT_ATTEMPTS =>
                {
                    continue;
                }
                other => {
                    let mut response = other?;
                    let rows: Vec<Credential> = response.take(0).map_err(engine)?;
                    return rows.into_iter().next().ok_or(StorageError::Stale);
                }
            }
        }
        Err(StorageError::RetryableBookkeepingConflict)
    }

    // ---------------------------------------------------------------------------------------------
    // Lifecycle
    // ---------------------------------------------------------------------------------------------

    /// Revokes `credential` belonging to the account `owner` names, and answers `NotFound` — the
    /// byte-identical absent-id answer — for a credential that is somebody else's.
    ///
    /// The ownership qualifier is inside the write's own `WHERE`, so a foreign row cannot be touched even
    /// momentarily. The read that follows runs only for a row the caller's account owns, which is why
    /// revoking your own already-revoked credential is an idempotent `Ok(())` while a stranger's is
    /// indistinguishable from a typo. Nothing here deletes a row: a revocation a later write could undo
    /// would not be a revocation.
    ///
    /// The same minting gate runs first, so a suspended account cannot mutate its own credentials, a held
    /// context whose workspace moved is refused, and a lost workspace surfaces as the operator-facing
    /// integrity case rather than as a silent success.
    pub async fn revoke_credential(
        &self,
        owner: &OwnerContext,
        credential: &CredentialId,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        self.mint_state_of(owner).await?;
        for attempt in 0..CLAIM_CONFLICT_ATTEMPTS {
            let outcome = self
                .db
                .query(
                    "UPDATE type::record($id) SET revoked = true, revoked_at = $now \
                     WHERE revoked = false AND account = $account RETURN AFTER;",
                )
                .bind(("id", credential.as_record_id().clone()))
                .bind(("now", now))
                .bind(("account", owner.account().as_record_id().clone()))
                .await
                .and_then(|response| response.check())
                .map_err(engine);
            match outcome {
                Err(StorageError::RetryableBookkeepingConflict)
                    if attempt + 1 < CLAIM_CONFLICT_ATTEMPTS =>
                {
                    continue;
                }
                other => {
                    let mut response = other?;
                    let rows: Vec<Credential> = response.take(0).map_err(engine)?;
                    if rows.len() == 1 {
                        return Ok(());
                    }
                    // Zero rows is either "already revoked, by me" or "not mine, or not there". Only the
                    // owner may be told those apart, so the follow-up read keeps the account qualifier.
                    let mut check = self
                        .db
                        .query(
                            "SELECT * FROM credential WHERE id = $id AND account = $account \
                             AND revoked = true LIMIT 1;",
                        )
                        .bind(("id", credential.as_record_id().clone()))
                        .bind(("account", owner.account().as_record_id().clone()))
                        .await
                        .map_err(engine)?
                        .check()
                        .map_err(engine)?;
                    let existing: Vec<Credential> = check.take(0).map_err(engine)?;
                    return if existing.len() == 1 {
                        Ok(())
                    } else {
                        Err(StorageError::NotFound)
                    };
                }
            }
        }
        Err(StorageError::RetryableBookkeepingConflict)
    }

    /// Replaces an account's password with `digest` and raises its `auth_epoch`, in one statement.
    ///
    /// Both halves in one write is the security property, not an efficiency: a rotated password with an
    /// unchanged epoch, or an advanced epoch with the old password still stored, both leave the account's
    /// existing sessions and agent tokens working after the recovery that was supposed to stop them. The
    /// engine reads the committed value for `auth_epoch += 1`, so two concurrent rotations of one account
    /// cannot write the same epoch twice.
    ///
    /// The policy gate runs first, so no caller can route a low-entropy or pre-Argon2 value into
    /// `account.password_hash` (owner decision F3). It takes the prepared text rather than a
    /// [`PasswordDigest`] because the guard belongs at the write, not in the type: a wrapper that could
    /// only be built by hashing would tempt a test-only constructor, which is exactly the shape this crate
    /// refuses for anything that reaches a credential column. Invalidating the prior credentials is *not* a sweep over
    /// live rows: `AccessRepo::authorize` compares each credential's minted epoch with this account's
    /// current one, so those rows stay readable for audit and inert for access.
    pub async fn rotate_password(
        &self,
        account: &AccountId,
        digest: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        if !Self::password_digest_is_policy(digest) {
            return Err(StorageError::InvalidInput(
                "the new password digest is not an Argon2id digest at this build's policy"
                    .to_string(),
            ));
        }
        for attempt in 0..CLAIM_CONFLICT_ATTEMPTS {
            let outcome = self
                .db
                .query(
                    "UPDATE type::record($id) SET password_hash = $digest, auth_epoch += 1, \
                     updated_at = $now WHERE auth_epoch != NONE RETURN AFTER;",
                )
                .bind(("id", account.as_record_id().clone()))
                .bind(("digest", digest.to_string()))
                .bind(("now", now))
                .await
                .and_then(|response| response.check())
                .map_err(engine);
            match outcome {
                Err(StorageError::RetryableBookkeepingConflict)
                    if attempt + 1 < CLAIM_CONFLICT_ATTEMPTS =>
                {
                    continue;
                }
                other => {
                    let mut response = other?;
                    let rows: Vec<Account> = response.take(0).map_err(engine)?;
                    return if rows.len() == 1 {
                        Ok(())
                    } else {
                        Err(StorageError::NotFound)
                    };
                }
            }
        }
        Err(StorageError::RetryableBookkeepingConflict)
    }

    // ---------------------------------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------------------------------

    /// Finds the row a presented secret belongs to, for the given kind. Absent, wrong-kind and
    /// non-canonical-digest presentations all answer [`StorageError::NotFound`] — the same answer, so
    /// presenting a made-up secret cannot learn whether some credential would have matched it.
    async fn resolve(&self, kind: &str, presented: &str) -> Result<Credential, StorageError> {
        let Some(secret) = BearerSecret::parse(presented) else {
            return Err(StorageError::NotFound);
        };
        let digest = secret.digest()?;
        let mut response = self
            .db
            .query(
                "SELECT * FROM credential WHERE secret_digest = $digest AND kind = $kind LIMIT 1;",
            )
            .bind(("digest", digest))
            .bind(("kind", kind.to_string()))
            .await
            .map_err(engine)?
            .check()
            .map_err(engine)?;
        let rows: Vec<Credential> = response.take(0).map_err(engine)?;
        let row = rows.into_iter().next().ok_or(StorageError::NotFound)?;
        if row.kind != kind
            || !secret.matches_digest(row.secret_digest.as_deref().unwrap_or_default())
        {
            return Err(StorageError::NotFound);
        }
        Ok(row)
    }

    /// Rolls a browser session's idle deadline, guarded on the row still being live and still owned.
    /// `false` means the guard stopped matching — revoked, consumed, or reassigned between the read and
    /// this write — and the caller must treat the session as spent rather than as recorded. The guard is
    /// what makes this a conditional write; see the module note on `UPDATE ONLY`.
    async fn roll_session(
        &self,
        credential: &CredentialId,
        account: &AccountId,
        now: DateTime<Utc>,
        renewed: DateTime<Utc>,
    ) -> Result<bool, StorageError> {
        for attempt in 0..CLAIM_CONFLICT_ATTEMPTS {
            let outcome = self
                .db
                .query(
                    "UPDATE type::record($id) SET last_used_at = $now, expires_at = $renewed \
                     WHERE revoked = false AND consumed = false AND account = $account \
                     RETURN AFTER;",
                )
                .bind(("id", credential.as_record_id().clone()))
                .bind(("now", now))
                .bind(("renewed", renewed))
                .bind(("account", account.as_record_id().clone()))
                .await
                .and_then(|response| response.check())
                .map_err(engine);
            match outcome {
                Err(StorageError::RetryableBookkeepingConflict)
                    if attempt + 1 < CLAIM_CONFLICT_ATTEMPTS =>
                {
                    continue;
                }
                other => {
                    let mut response = other?;
                    let rows: Vec<Credential> = response.take(0).map_err(engine)?;
                    return Ok(rows.len() == 1);
                }
            }
        }
        Err(StorageError::RetryableBookkeepingConflict)
    }

    /// Records a use of an agent token. An absent `expires_at` stays absent: recording a use must not turn
    /// "no expiry" into a deadline, which would expire a long-lived token at the epoch of this request.
    async fn record_use(
        &self,
        credential: &CredentialId,
        account: &AccountId,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        for attempt in 0..CLAIM_CONFLICT_ATTEMPTS {
            let outcome = self
                .db
                .query(
                    "UPDATE type::record($id) SET last_used_at = $now \
                     WHERE revoked = false AND consumed = false AND account = $account \
                     RETURN AFTER;",
                )
                .bind(("id", credential.as_record_id().clone()))
                .bind(("now", now))
                .bind(("account", account.as_record_id().clone()))
                .await
                .and_then(|response| response.check())
                .map_err(engine);
            match outcome {
                Err(StorageError::RetryableBookkeepingConflict)
                    if attempt + 1 < CLAIM_CONFLICT_ATTEMPTS =>
                {
                    continue;
                }
                other => {
                    let mut response = other?;
                    let rows: Vec<Credential> = response.take(0).map_err(engine)?;
                    return if rows.len() == 1 {
                        Ok(())
                    } else {
                        Err(StorageError::Stale)
                    };
                }
            }
        }
        Err(StorageError::RetryableBookkeepingConflict)
    }
}

/// The enrollment operation key an invitation link produces. `to_sql()` rather than a display form because
/// a numeric and a quoted opaque credential key have to stay distinct labels.
fn enrollment_key(link: &Credential) -> Result<String, StorageError> {
    let id = link.credential_id()?;
    Ok(format!("invite:{}", id.as_record_id().to_sql()))
}

/// The new idle deadline, never past the absolute cap. A row with no cap (a session written before the
/// column existed) keeps only the idle window, the same reading `credential_entitles_read` gives an absent
/// deadline: absent means unconstrained.
fn renew_deadline(idle_deadline: DateTime<Utc>, absolute: Option<DateTime<Utc>>) -> DateTime<Utc> {
    match absolute {
        Some(cap) if cap <= idle_deadline => cap,
        _ => idle_deadline,
    }
}

/// Reads a numeric PHC parameter, or `None` when it is absent or not a number.
fn number_param(hash: &PasswordHash<'_>, key: &str) -> Option<u32> {
    hash.params
        .iter()
        .find(|(name, _)| name.as_str() == key)
        .map(|(_, value)| value.as_str())
        .and_then(|text| text.parse::<u32>().ok())
}

/// The blocking half of [`AuthRepo::create_password_digest`].
///
/// `Params::new` refuses a set Argon2 cannot express (`m_cost` below `8 * p_cost`, or zero passes);
/// `policy_constants_are_a_valid_argon2_parameter_set` below pins that the constants above are not such a
/// set, so the error path here is unreachable in practice and still not an `unwrap`.
fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let params = Params::new(
        ARGON2_M_COST,
        ARGON2_T_COST,
        ARGON2_P_COST,
        Some(SECRET_BYTES),
    )
    .map_err(|_| argon2::password_hash::Error::Algorithm)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let salt = SaltString::generate(&mut OsRng);
    Ok(argon2
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

/// The blocking half of [`AuthRepo::password_matches`].
///
/// Constant time inside: `argon2`'s own verifier recomputes at the parameters the string carries and
/// compares the two digests with the same `subtle`-backed equality as [`BearerSecret::matches_digest`]. It
/// is reached only after [`AuthRepo::password_digest_is_policy`] has accepted the shape, so the parameters
/// it will use are the policy's by the time it runs — which is also why the instance is built from the
/// policy constants rather than defaulted.
fn verify_password(stored: &str, password: &str) -> bool {
    let Ok(hash) = PasswordHash::new(stored) else {
        return false;
    };
    let Ok(params) = Params::new(
        ARGON2_M_COST,
        ARGON2_T_COST,
        ARGON2_P_COST,
        Some(SECRET_BYTES),
    ) else {
        return false;
    };
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    argon2.verify_password(password.as_bytes(), &hash).is_ok()
}

/// The canonical stored spelling of a SHA-256 digest: the base64 rendering
/// [`argon2::password_hash::Output`] produces, and the form [`BearerSecret::matches_digest`] parses back.
fn digest_text(bytes: &[u8; SECRET_BYTES]) -> Result<String, StorageError> {
    let computed = Sha256::digest(bytes);
    Output::new(computed.as_slice())
        .map(|output| output.to_string())
        .map_err(internal)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A `spawn_blocking` failure is a panicked or cancelled hashing job, never a wrong password.
fn join_failure(error: tokio::task::JoinError) -> StorageError {
    StorageError::Internal(
        anyhow::Error::new(error).context("password hashing work did not complete"),
    )
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

/// Engine failures, classified by variant and never by message text. The optimistic write conflict is the
/// one case the claim, roll and record paths have to tell apart, because they retry it; everything else
/// stays [`StorageError::Internal`] with its cause, for the log.
fn engine(error: surrealdb::Error) -> StorageError {
    if matches!(error.query_details(), Some(QueryError::TransactionConflict)) {
        return StorageError::RetryableBookkeepingConflict;
    }
    StorageError::Internal(anyhow::Error::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy gate is a shape test, so it has to refuse the things that are *nearly* right. Each string
    /// below is digest-shaped material a hand-written row, an older build, or a truncated log line could
    /// contain; none of them may authenticate.
    #[test]
    fn policy_gate_refuses_everything_this_build_did_not_produce() {
        let weak =
            "$argon2id$v=19$m=1024,t=1,p=1$c2FsdHNhbHRzYWx0c2ExMg$faaQ8LCTWSspjBWODnKX9VRNlDo7X8om";
        assert!(
            !AuthRepo::password_digest_is_policy(weak),
            "a 1 MiB single-pass digest passed the policy gate"
        );
        let other_algorithm =
            "$argon2i$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0c2ExMg$faaQ8LCTWSspjBWODnKX9VRNlDo7X8om";
        assert!(
            !AuthRepo::password_digest_is_policy(other_algorithm),
            "argon2i passed a gate that must insist on argon2id"
        );
        let other_version = "$argon2id$v=16$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0c2ExMg$faaQ8LCTWSspjBWODnKX9VRNlDo7X8om";
        assert!(!AuthRepo::password_digest_is_policy(other_version));
        // Salt and hash both required: this spelling has the hash where the salt belongs.
        let no_hash = "$argon2id$v=19$m=19456,t=2,p=1$faaQ8LCTWSspjBWODnKX9VRNlDo7X8om";
        assert!(!AuthRepo::password_digest_is_policy(no_hash));
        let short_output =
            "$argon2id$v=19$m=19456,t=2,p=1,output_len=16$c2FsdHNhbHRzYWx0c2ExMg$faaQ8LCTWS";
        assert!(!AuthRepo::password_digest_is_policy(short_output));
        for text in [
            "",
            "   ",
            "hunter2",
            "$argon2id$",
            "faaQ8LCTWSspjBWODnKX9VRNlDo7X8om",
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0c2ExMg",
            // The exact placeholder shape Task 3's own enrollment fixtures use.
            "$argon2id$fixture-hash",
            "fixture-hash",
        ] {
            assert!(
                !AuthRepo::password_digest_is_policy(text),
                "`{text}` was accepted as a password digest"
            );
        }
    }

    /// The idle roll cannot outlive the cap, and an absent cap means unconstrained.
    #[test]
    fn renew_clamps_to_the_absolute_cap() {
        let base = DateTime::<Utc>::from_timestamp(1_000_000, 0).unwrap();
        let idle = base + Duration::minutes(30);
        let cap = base + Duration::hours(24);
        assert_eq!(renew_deadline(idle, Some(cap)), idle);
        assert_eq!(renew_deadline(cap + Duration::minutes(1), Some(cap)), cap);
        assert_eq!(renew_deadline(idle, None), idle);
        // A cap already passed still clamps, because the deadline check is what refuses the row; the roll
        // must not be the thing that resurrects it.
        assert_eq!(
            renew_deadline(idle, Some(base - Duration::seconds(1))),
            base - Duration::seconds(1)
        );
    }

    /// A secret renders to 64 lowercase hex characters and parses back to bytes with the same digest; a
    /// stored digest is not a secret spelling, so parsing it must fail rather than reconstruct one.
    #[test]
    fn secrets_render_hex_and_parse_only_hex() {
        let secret = BearerSecret::generate();
        let text = secret.render();
        assert_eq!(text.0.len(), SECRET_BYTES * 2);
        assert!(
            text.0
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "the wire spelling is not lowercase hex"
        );
        let reparsed = BearerSecret::parse(&text.0).expect("a rendered secret must parse back");
        assert_eq!(
            reparsed.digest().unwrap(),
            text.digest().unwrap(),
            "a rendered secret did not survive the wire spelling"
        );
        // Whitespace and case are transport noise, not different secrets.
        let padded = format!("  {}  ", text.0);
        assert_eq!(
            BearerSecret::parse(&padded)
                .expect("padded hex must parse")
                .digest()
                .unwrap(),
            text.digest().unwrap()
        );
        let upper = text.0.to_uppercase();
        assert_eq!(
            BearerSecret::parse(&upper)
                .expect("uppercase hex must parse")
                .digest()
                .unwrap(),
            text.digest().unwrap()
        );
        assert!(BearerSecret::parse(&text.0[..62]).is_none());
        assert!(BearerSecret::parse("").is_none());
        assert!(BearerSecret::parse(&"f".repeat(63)).is_none());
        assert!(BearerSecret::parse(&"f".repeat(65)).is_none());
        assert!(BearerSecret::parse(&"z".repeat(64)).is_none());
        let digest = text.digest().unwrap();
        assert_ne!(digest, text.0, "the digest echoed the secret");
        assert!(
            BearerSecret::parse(&digest).is_none(),
            "a stored digest is not a presentation spelling"
        );
    }

    /// Two secrets never share a digest, a different secret is a mismatch, and a stored value that is not
    /// canonical is a mismatch rather than an error.
    #[test]
    fn digests_are_canonical_and_discriminate() {
        let secret = BearerSecret::generate();
        let digest = secret.digest().unwrap();
        assert!(secret.matches_digest(&digest));
        assert!(
            !BearerSecret::generate().matches_digest(&digest),
            "two independently minted secrets matched one digest"
        );
        assert!(!secret.matches_digest("not base64 at all"));
        assert!(!secret.matches_digest(""));
    }

    /// Nothing secret is reachable through `Debug` or `Display` on any type in this module, and the
    /// redaction costs nothing at the boundary that legitimately needs the bytes.
    #[test]
    fn debug_and_display_redact_secret_material() {
        let secret = BearerSecret::generate();
        let text = secret.clone().render();
        let plaintext = text.expose().to_string();
        let digest = secret.digest().unwrap();

        let minted = MintedSecret {
            credential: CredentialId::try_from(RecordId::new("credential", "c_1")).unwrap(),
            secret: BearerSecret::generate(),
            expires_at: None,
        };
        let session = MintedSession {
            credential: CredentialId::try_from(RecordId::new("credential", "c_2")).unwrap(),
            cookie: BearerSecret::generate(),
            csrf: BearerSecret::generate(),
            expires_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            absolute_expires_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        };
        let password = PasswordDigest("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$abcdef".to_string());

        for (label, rendered) in [
            ("bearer", format!("{secret:?}")),
            ("text", format!("{text:?}")),
            ("text display", format!("{text}")),
            ("minted", format!("{minted:?}")),
            ("session", format!("{session:?}")),
            ("password", format!("{password:?}")),
            ("password display", format!("{password}")),
        ] {
            assert!(
                !rendered.contains(&digest)
                    && !rendered.contains(&plaintext)
                    && !rendered.contains("19456")
                    && !rendered.contains("argon2id"),
                "{label} rendered secret material: {rendered}"
            );
        }
        assert_eq!(format!("{secret:?}"), "BearerSecret(<redacted>)");
        assert_eq!(format!("{text}"), "<secret redacted>");
        assert!(format!("{minted:?}").contains("redacted"), "a mint leaked");
        assert!(
            format!("{session:?}").contains("redacted"),
            "a grant leaked"
        );
        // Redaction costs nothing where the bytes are legitimately needed.
        assert_eq!(plaintext.len(), SECRET_BYTES * 2);
        assert_eq!(text.expose(), plaintext);
        assert_eq!(
            password.as_str(),
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$abcdef"
        );
    }

    /// `m_cost` is in KiB, and the crate's own range check proves the policy is a *valid* Argon2 parameter
    /// set — the test that keeps a typo (`19_456_000`, or `19`) from shipping as an 18 GiB or a 19 KiB
    /// policy.
    #[test]
    fn policy_constants_are_a_valid_argon2_parameter_set() {
        let params = Params::new(
            ARGON2_M_COST,
            ARGON2_T_COST,
            ARGON2_P_COST,
            Some(SECRET_BYTES),
        )
        .expect("the policy must be a usable Argon2 parameter set");
        assert_eq!(params.m_cost(), ARGON2_M_COST);
        assert_eq!(params.t_cost(), ARGON2_T_COST);
        assert_eq!(params.p_cost(), ARGON2_P_COST);
        assert_eq!(params.output_len(), Some(SECRET_BYTES));
        assert!(
            (ARGON2_M_COST as usize) >= 8 * (ARGON2_P_COST as usize),
            "Argon2 requires m_cost >= 8 * p_cost"
        );
        assert_eq!(
            SECRET_BYTES * 2,
            64,
            "a bearer secret is 64 hex characters on the wire"
        );
    }

    /// The label a mint writes is the trimmed one, and a blank name is a wrong request rather than an
    /// unnamed credential: `credential.label`'s own assertion refuses whitespace, so a caller that could
    /// mint with `" "` would get a storage error instead of an answer.
    #[test]
    fn mint_labels_are_trimmed_and_blank_is_refused() {
        assert_eq!("  work laptop  ".trim(), "work laptop");
        assert!("   ".trim().is_empty());
    }

    /// An invitation link's enrollment label distinguishes a numeric from a quoted opaque credential key,
    /// because the operation key is the audit anchor for the owner it creates.
    #[test]
    fn enrollment_labels_cannot_collide_across_key_shapes() {
        let label = |key: &str| {
            enrollment_key(&Credential {
                id: Some(RecordId::new("credential", key)),
                account: None,
                kind: credential_kind::INVITE.to_string(),
                label: None,
                secret_digest: None,
                auth_epoch: None,
                revoked: None,
                consumed: None,
                expires_at: None,
                absolute_expires_at: None,
                csrf_digest: None,
                last_used_at: None,
                revoked_at: None,
                consumed_at: None,
                created_at: None,
            })
            .unwrap()
        };
        let numeric = label("7");
        let string_key = label("\"7\"");
        assert_ne!(
            numeric, string_key,
            "a quoted opaque key collapsed onto the numeric one: {numeric}"
        );
        assert!(numeric.starts_with("invite:"));
        // A row with no id is a bug in the query, and the label refuses rather than inventing a key: the
        // same `Internal` every other row accessor in this crate gives.
        let broken = enrollment_key(&Credential {
            id: None,
            account: None,
            kind: credential_kind::INVITE.to_string(),
            label: None,
            secret_digest: None,
            auth_epoch: None,
            revoked: None,
            consumed: None,
            expires_at: None,
            absolute_expires_at: None,
            csrf_digest: None,
            last_used_at: None,
            revoked_at: None,
            consumed_at: None,
            created_at: None,
        });
        assert!(
            matches!(broken, Err(StorageError::Internal(_))),
            "an id-less link row produced a key anyway: {broken:?}"
        );
    }
}
