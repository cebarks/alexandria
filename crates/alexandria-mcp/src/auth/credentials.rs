//! The outward shapes that carry a bearer secret, and the initial lifetimes the plan sets for them.
//!
//! [`OneTimeSecret`] and [`BrowserGrant`] exist because a secret has exactly one moment of visibility. The
//! store holds only a SHA-256 digest of these values (written by
//! [`alexandria_storage::repos::AuthRepo`]), so once a mint returns, nothing in this process — no read
//! path, no repository, no debug page — can produce the text again. The types make that property visible
//! rather than merely true: their fields are private, there is no public constructor and no `Serialize`, and
//! both `Debug` and `Display` render `<redacted>`. The plaintext is reachable only through an accessor whose
//! name says what it is for, so a call site that puts a secret on the wire is greppable.
//!
//! The lifetimes are constants, not configuration, for the same reason owner decision A2 gives for the
//! Argon2 costs: a knob is a way to weaken a control by accident. Changing one is an edit here, and rows
//! already written keep the deadlines stored on them — only credentials minted afterwards move.

use alexandria_storage::models::identity::CredentialId;
use alexandria_storage::repos::SecretText;
use chrono::{DateTime, TimeDelta, Utc};

/// A browser session's idle window: an authenticated use extends its life by this much.
pub const BROWSER_IDLE: TimeDelta = TimeDelta::minutes(30);
/// A browser session's absolute lifetime: no amount of use takes it past this.
pub const BROWSER_ABSOLUTE: TimeDelta = TimeDelta::hours(24);
/// How long an invitation link stays claimable.
pub const INVITE_TTL: TimeDelta = TimeDelta::hours(48);
/// How long a recovery link stays claimable. The shortest window of the policy, because consuming one
/// rotates an account's password and invalidates every credential it held.
pub const RECOVERY_TTL: TimeDelta = TimeDelta::hours(1);

/// A named agent token, shown once.
///
/// Held by whoever asked for it and by nobody else: revocation and a recovery's epoch bump are the ways it
/// ends, and `expires_at` is absent on its row because there is no date to check.
#[derive(Clone)]
pub struct OneTimeSecret {
    credential: CredentialId,
    secret: SecretText,
}

impl OneTimeSecret {
    /// Built only by [`crate::auth::service::AuthService`], out of a credential row that was just written.
    pub(crate) fn issue(credential: CredentialId, secret: SecretText) -> Self {
        Self { credential, secret }
    }

    /// The row this secret authenticates: the handle `revoke_credential` takes.
    pub fn credential(&self) -> &CredentialId {
        &self.credential
    }

    /// The bearer text, for the single response that may carry it. Every call site of this is a place a
    /// secret leaves the process, which is why there is no `as_str`.
    pub fn expose(&self) -> &str {
        self.secret.expose()
    }
}

impl std::fmt::Debug for OneTimeSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneTimeSecret")
            .field("credential", &self.credential)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Display for OneTimeSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A receipt that prints itself is how a secret ends up in a log line or an error message one
        // refactor away from here.
        f.write_str("a one-time agent token (secret not recoverable)")
    }
}

/// A browser session: the cookie that authenticates it, the CSRF token paired with that session, and the
/// idle deadline it lives under.
///
/// Two secrets, not one. A session cookie that also served as the CSRF token would let any request that can
/// carry the cookie carry the proof too, which is the property CSRF protection exists to establish; the
/// store keeps the two halves in separate columns precisely so neither can stand in for the other.
#[derive(Clone)]
pub struct BrowserGrant {
    credential: CredentialId,
    cookie: SecretText,
    csrf: SecretText,
    expires_at: DateTime<Utc>,
}

impl BrowserGrant {
    /// Built only by [`crate::auth::service::AuthService`], out of a session row that was just written.
    pub(crate) fn grant(
        credential: CredentialId,
        cookie: SecretText,
        csrf: SecretText,
        expires_at: DateTime<Utc>,
    ) -> Self {
        Self {
            credential,
            cookie,
            csrf,
            expires_at,
        }
    }

    /// The session's credential row: what a revocation names and what a CSRF check is answered against.
    pub fn credential(&self) -> &CredentialId {
        &self.credential
    }

    /// The cookie bearer text.
    pub fn expose_cookie(&self) -> &str {
        self.cookie.expose()
    }

    /// The CSRF bearer text.
    pub fn expose_csrf(&self) -> &str {
        self.csrf.expose()
    }

    /// When the session stops being usable without another authenticated request. The absolute cap it can
    /// never roll past is stored on the row and enforced in storage, not returned here: a client that could
    /// report — let alone extend — the cap would be holding the one value the roll is bounded by.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

impl std::fmt::Debug for BrowserGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserGrant")
            .field("credential", &self.credential)
            .field("cookie", &"<redacted>")
            .field("csrf", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl std::fmt::Display for BrowserGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a browser session expiring {}", self.expires_at)
    }
}
