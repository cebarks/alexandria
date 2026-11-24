//! The credential lifecycle service: the only path from a presented secret to a [`Principal`].
//!
//! This is the seam Task 5's HTTP and MCP transports admit through, and the surface Tasks 8–9 build tools
//! and pages on top of. It is deliberately thin: it decides *policy* (how long a session lives, which
//! answer a caller is allowed to hear, that a secret leaves the process once) and delegates every mechanism —
//! hashing, digesting, comparing, claiming, revoking — to
//! [`alexandria_storage::repos::AuthRepo`], which owns the rows.
//!
//! The service holds a database handle, not an authority. It mints no [`OwnerContext`] and no
//! [`SystemContext`](alexandria_storage::access::SystemContext): an authenticated [`Principal`] comes out of
//! here, and [`AccessRepo::authorize`](alexandria_storage::access::AccessRepo::authorize) decides
//! separately, from live rows, what that principal may act on. There is no public or test-only constructor
//! for either, in this crate or in storage, so a request payload cannot become an actor by naming an id —
//! and a caller that wants to *act* as the owner still has to authorize, per request, against the rows.
//!
//! ## What each method refuses, and why that is the answer
//!
//! The full table is in [`crate::error::ServiceError`]'s module documentation. Three choices are specific to
//! authentication rather than to the general mapping:
//!
//! * a login where the account is unknown, the password is wrong, or the stored value is not a digest this
//!   build can verify are all [`ServiceError::NotFound`], in the same words. Anything else is an
//!   account-enumeration oracle on an unauthenticated path.
//! * a credential that exists, belongs to the caller, and no longer entitles — revoked, consumed, past a
//!   deadline, minted under a superseded epoch — is [`ServiceError::Stale`]: re-authenticate, and do not
//!   retry the same presentation.
//! * an enrolled owner whose private workspace row is gone is [`ServiceError::StoreIntegrity`]. That is the
//!   same underlying [`alexandria_storage::error::StorageError::Internal`] the enrollment path reports — it
//!   stays internal because the caller did nothing wrong and no client may learn whose state is missing —
//!   and it is distinguishable to whoever operates the server, by variant, because "restore from backup or
//!   re-enroll" is a different decision from "the driver hiccuped".

use super::credentials::{
    BROWSER_ABSOLUTE, BROWSER_IDLE, BrowserGrant, INVITE_TTL, OneTimeSecret, RECOVERY_TTL,
};
use crate::auth::admission::enrollable_login;
use crate::auth::mcp_transport::{AuthEvents, CredentialKind, CredentialLease, Registration};
use crate::error::ServiceError;
use alexandria_storage::Database;
use alexandria_storage::access::OwnerContext;
use alexandria_storage::access::Principal;
use alexandria_storage::models::auth::PrincipalIds;
use alexandria_storage::models::identity::CredentialId;
use alexandria_storage::repos::AuthRepo;
use chrono::{DateTime, Utc};
use std::sync::Arc;

/// How many times [`AuthService::admit`] re-verifies a credential before it gives up.
///
/// Bounded on purpose: a re-verification is a live read, so an unbounded loop would let a caller that
/// keeps revoking something make this process spin. Two retries covers the realistic interleaving — a
/// revocation that races one or two admissions — and after that the answer is `Stale`, which is the same
/// thing the loop was doing, asked of a fresher observation.
pub const MAX_ADMIT_ATTEMPTS: u32 = 3;

pub struct AuthService {
    db: Arc<Database>,
    /// The boundary this service mints leases into and publishes revocations through.
    ///
    /// Held by the service rather than by a transport because the revocation that matters is the one that
    /// just committed: whoever writes `revoked = true` is the only party that knows it happened, and a bus
    /// it does not reach is a bus whose live leases can never be cancelled.
    events: AuthEvents,
}

impl AuthService {
    /// Bound to the store every other repo in this process uses. The service takes the database, not a
    /// capability: it needs no `SystemContext`, and holding one would be a way to widen what authentication
    /// can reach.
    ///
    /// The bus this constructor builds terminates no SDK sessions, because there is no MCP transport
    /// attached to it. A hosted server builds one over its own session manager and passes it to
    /// [`AuthService::admitting`].
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            events: AuthEvents::without_sessions(),
        }
    }

    /// The same service over a bus the caller shares with its transport and its admission control.
    ///
    /// One bus per store handle, deliberately: two buses are two answers to "who has been revoked?", and
    /// the one the streams are gated by would be the one that never learns.
    pub fn admitting(db: Arc<Database>, events: AuthEvents) -> Self {
        Self { db, events }
    }

    /// The bus this service mints leases into and publishes revocations through.
    pub fn events(&self) -> AuthEvents {
        self.events.clone()
    }

    /// Verifies a presented secret and registers the lifetime it entitles.
    ///
    /// This is the transport's entry point, and the only producer of a [`CredentialLease`]. It differs from
    /// [`AuthService::authenticate_agent`] in what it hands back: a lease carries the generation this
    /// credential was admitted at and the token revocation fires, so a stream or a queued request can be
    /// *ended* rather than merely refused the next time it is asked.
    ///
    /// `kind` is the caller's statement of where the secret came from, and storage requires the row to match
    /// it: a browser session cookie presented as an agent token is refused because the agent path resolves
    /// only agent rows. That is why this transport needs no cookie sniffing to keep the two kinds apart —
    /// the boundary is the credential table, not a header-name heuristic.
    ///
    /// `now` is a parameter because the credential windows must be testable without sleeping, and every
    /// transport path passes the server's own clock; a client that could name the instant could roll an idle
    /// window forward. The loop below is what makes that observation honest: an invalidation published while
    /// the credential was being verified costs a re-verification, and a lookup that succeeded against a
    /// since-revoked row never becomes a live lease.
    pub async fn admit(
        &self,
        credential: &str,
        kind: CredentialKind,
        now: DateTime<Utc>,
    ) -> Result<CredentialLease, ServiceError> {
        for _attempt in 0..MAX_ADMIT_ATTEMPTS {
            // Observed *before* the verification, so a revocation published during it is strictly newer.
            let observation = self.events.observed();
            let principal = match kind {
                CredentialKind::Browser => self.authenticate_browser(credential, now).await?,
                CredentialKind::Agent => self.authenticate_agent(credential, now).await?,
            };
            match self.events.register(&principal, &observation) {
                Registration::Issued(lease) => return Ok(lease),
                // This credential, or the account behind it, moved while it was being verified.
                Registration::Stale => return Err(ServiceError::Stale),
                // Somebody else moved, so this verification can no longer be vouched for. Re-read.
                Registration::Contended => continue,
            }
        }
        Err(ServiceError::Stale)
    }

    /// Verifies a login and mints a browser session for it.
    ///
    /// Three of the refusals below are the same answer on purpose: an unknown login, a wrong password, and
    /// an account whose stored `password_hash` is not an Argon2id digest this build recognises all render
    /// identically as `NotFound`. The third case is the one worth naming, because it is a *state* the store
    /// can be in — an account enrolled before Task 4, or a row written by hand — and the temptation is to
    /// report it as "this account needs a password reset". Doing that would turn the login form into an
    /// oracle for which accounts are enrollable, so the condition is refused silently to the client and left
    /// to the storage layer's own gate ([`AuthRepo::password_digest_is_policy`]) to enforce on every write.
    ///
    /// The session's two deadlines come from [`super::credentials`]: the idle window rolls on each use, the
    /// absolute cap never moves.
    pub async fn login(
        &self,
        login: &str,
        password: &str,
        now: DateTime<Utc>,
    ) -> Result<BrowserGrant, ServiceError> {
        let auth = AuthRepo::new(self.db.inner());
        let account = match auth.account_by_login(login).await? {
            Some(account) => account,
            None => {
                // No row, so nothing to verify against — and the same words as a wrong password, so the
                // difference is not observable from outside.
                return Err(ServiceError::NotFound);
            }
        };
        if !AuthRepo::password_matches(account.password_hash.as_deref(), password).await? {
            return Err(ServiceError::NotFound);
        }
        let session = auth
            .mint_browser_session(&account.account_id()?, BROWSER_IDLE, BROWSER_ABSOLUTE, now)
            .await?;
        Ok(BrowserGrant::grant(
            session.credential,
            session.cookie.render(),
            session.csrf.render(),
            session.expires_at,
        ))
    }

    /// Verifies a browser session cookie, extends its idle deadline, and mints the principal it entitles.
    pub async fn authenticate_browser(
        &self,
        cookie: &str,
        now: DateTime<Utc>,
    ) -> Result<Principal, ServiceError> {
        Ok(AuthRepo::new(self.db.inner())
            .authenticate_browser_session(cookie, BROWSER_IDLE, now)
            .await?)
    }

    /// Verifies an agent bearer token and mints the principal it entitles.
    pub async fn authenticate_agent(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<Principal, ServiceError> {
        Ok(AuthRepo::new(self.db.inner())
            .authenticate_agent_token(token, now)
            .await?)
    }

    /// Whether `csrf` is the token paired with the session behind `credential`.
    ///
    /// The check Task 5's mutation admission needs and Task 4 has to make possible: the half-secret was
    /// minted and digested here, so it is verified here too. It answers a question and takes no action — a
    /// `true` here is not authority, and the caller still holds a `Principal` it authenticated with.
    ///
    /// A session the store has already retired — revoked, spent, past a deadline, or superseded by a
    /// recovery's epoch bump — matches nothing, so a mutation cannot be admitted on the proof of a session
    /// that can no longer be used to read. That is the same liveness predicate `AccessRepo::authorize`
    /// applies, which is why the two calls cannot disagree.
    pub async fn verify_csrf(
        &self,
        credential: &CredentialId,
        csrf: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, ServiceError> {
        Ok(AuthRepo::new(self.db.inner())
            .browser_session_matches_csrf(credential, csrf, now)
            .await?)
    }

    /// Issues a named, revocable agent token into the owner's account.
    ///
    /// The [`OwnerContext`] is re-read from live rows before anything is written, so a context that was
    /// issued before a suspension, a workspace loss or a recovery cannot mint a new credential — holding an
    /// old receipt is not a grant, which is the same rule `AccessRepo::authorize` applies to reads.
    ///
    /// The returned secret is the only time this process will show that token. There is no redisplay: the
    /// row keeps a digest, and losing the receipt means issuing a new token and revoking the old one.
    pub async fn create_agent_token(
        &self,
        owner: &OwnerContext,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<OneTimeSecret, ServiceError> {
        let minted = AuthRepo::new(self.db.inner())
            .mint_agent_token(owner, name, now)
            .await?;
        Ok(OneTimeSecret::issue(
            minted.credential,
            minted.secret.render(),
        ))
    }

    /// Claims an invitation link and enrols the account it names.
    ///
    /// Returns the new owner's ids, not a principal and not a session: an invitation is a way to *create* an
    /// account, and the person who used it still has to log in. Exactly one concurrent claim can win, so one
    /// link enrolls one owner; the loser finds the link consumed and gets `Stale`, and a login that is
    /// already taken refuses without writing anything (the enrollment rules are Task 3's and are unchanged
    /// here — see [`IdentityRepo::bootstrap_owner`]).
    ///
    /// The password is hashed *before* the link is claimed, so a request that cannot produce a policy digest
    /// never burns a one-use link. The digest that reaches `bootstrap_owner` is produced here at the policy,
    /// which is what keeps the recorded `bootstrap_fingerprint` meaningful for the life of the account
    /// (owner decision F3).
    pub async fn accept_invite(
        &self,
        secret: &str,
        login: &str,
        password: &str,
        now: DateTime<Utc>,
    ) -> Result<PrincipalIds, ServiceError> {
        // The F4-15 gate, and the order is the whole of it: storage claims the link last, so a login the
        // schema could never store is refused here, before `create_password_digest` is reached, and a
        // request that cannot produce a digest burns nothing. Invert the two and one held invitation link
        // buys one ~19 MiB Argon2 run per attempt for the rest of its 48 hours — which is a denial of
        // service purchased with a secret that was already leaked.
        let login = enrollable_login(login)?;
        Ok(AuthRepo::new(self.db.inner())
            .accept_invitation(secret, &login, password, now)
            .await?)
    }

    /// Claims a recovery link, rotates the account's password, and invalidates every credential that account
    /// held.
    ///
    /// Nothing is returned — no session, no principal, no token — because a recovery that also handed back a
    /// live credential would let whoever consumed the link keep using the account without ever proving the
    /// new password. The account's `auth_epoch` rises, and every browser session and agent token minted
    /// before it is inert to both this crate's authentication and `AccessRepo`'s authorization without a
    /// sweep of their rows.
    pub async fn recover(
        &self,
        secret: &str,
        new_password: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ServiceError> {
        // The password work is bounded the same way an invitation's is: an unusable link is refused before
        // a digest, and the digest itself runs inside the hash budget the transport holds.
        Ok(AuthRepo::new(self.db.inner())
            .recover(secret, new_password, now)
            .await?)
    }

    /// Invalidates every lease and session of an account that this process has just changed out from under.
    ///
    /// A recovery raises `account.auth_epoch`, which makes every credential the account held inert *in the
    /// store* — the next authorization refuses it, and the next admission with it. What a store-side epoch
    /// bump cannot do is reach a stream that is already open, because `AuthRepo::recover` returns `()` and
    /// the account it rotated is not in the answer. So the caller that knows the account — the admin action
    /// that suspended it, the recovery flow that looked the link's owner up first — hands it here, and this
    /// is the one call that turns "inert in the rows" into "cancelled in this process".
    ///
    /// Leaving it uncalled is a real, bounded limit and not a silent one: an open stream from before the
    /// recovery keeps serving until its next request, which is then refused.
    pub async fn invalidate_account(
        &self,
        account: &alexandria_storage::models::identity::AccountId,
    ) -> usize {
        self.events.revoke_account(account).await
    }

    /// Revokes one of the owner's own credentials.
    ///
    /// A credential id belonging to another owner and one that does not exist are the same answer, in the
    /// same words, so ids cannot be probed for ownership. Revoking your own already-revoked credential is
    /// idempotent and does not rewrite the first revocation's stamp.
    ///
    /// `authorize` re-reads the row on every call, so a request already in flight is refused at its next
    /// authorization rather than at this write; cancelling an open stream is Task 5's `CredentialLease`, not
    /// something a revocation here can do.
    pub async fn revoke_credential(
        &self,
        owner: &OwnerContext,
        id: &CredentialId,
        now: DateTime<Utc>,
    ) -> Result<(), ServiceError> {
        AuthRepo::new(self.db.inner())
            .revoke_credential(owner, id, now)
            .await?;
        // Publish after the write, never before: the invariant the lease boundary rests on is that a
        // published invalidation has a committed row behind it. Reversed, an admission could verify against
        // a row that is about to be revoked, see a generation that has already moved, and be refused for a
        // credential that is still live — and a revocation whose stamp can land before its own write is a
        // revocation that leaks a lease the bus will never reach.
        self.events.revoke_credential(id).await;
        Ok(())
    }

    /// Creates an invitation link for `owner` to hand over out of band.
    ///
    /// Minting lives here as well as claiming, because the two halves share the policy that makes a link
    /// safe: the same secret shape, the same `INVITE_TTL` window, and a caller that cannot pick either. A
    /// later admin surface (Task 9) may want its own issuance path, but it should not be able to mint a link
    /// whose lifetime is a parameter of the request.
    ///
    /// The link belongs to the issuing account in `credential.account`, which v009 requires and which the
    /// invited account cannot be: it does not exist until the link is claimed. The consequence is the useful
    /// one — an owner can see and revoke the invitations it granted.
    pub async fn create_invite(
        &self,
        owner: &OwnerContext,
        now: DateTime<Utc>,
    ) -> Result<OneTimeSecret, ServiceError> {
        let minted = AuthRepo::new(self.db.inner())
            .create_invitation(owner.account(), INVITE_TTL, now)
            .await?;
        Ok(OneTimeSecret::issue(
            minted.credential,
            minted.secret.render(),
        ))
    }

    /// Creates a password-recovery link for `owner`'s account, with the policy's shortest lifetime.
    ///
    /// The pairing to [`AuthService::recover`], and the reason that operation invalidates rather than merely
    /// replaces: a link that can rotate a password is the one credential whose consumption must end every
    /// other session the account holds.
    pub async fn create_recovery_link(
        &self,
        owner: &OwnerContext,
        now: DateTime<Utc>,
    ) -> Result<OneTimeSecret, ServiceError> {
        let minted = AuthRepo::new(self.db.inner())
            .create_recovery_link(owner.account(), RECOVERY_TTL, now)
            .await?;
        Ok(OneTimeSecret::issue(
            minted.credential,
            minted.secret.render(),
        ))
    }
}
