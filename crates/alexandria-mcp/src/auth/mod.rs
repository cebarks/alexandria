//! Authentication and the credential lifecycle.
//!
//! Task 4's half of the multi-user boundary: password digests, bearer secrets, browser sessions, agent
//! tokens, and the expiring one-use links that enroll an owner or recover a password. The mechanism is in
//! storage (`alexandria_storage::repos::AuthRepo`) because storage owns every query; what lives here is the
//! policy — how long each credential lives, which refusal a caller is allowed to hear, and the fact that a
//! secret is shown exactly once.
//!
//! Two rules hold the whole module together, and both come from the design rather than from taste.
//!
//! **Possession of a name is not authority.** The types re-exported below are data: an
//! [`alexandria_storage::access::Principal`] comes only from a credential this service verified, an
//! [`alexandria_storage::access::OwnerContext`] only from `AccessRepo::authorize` re-reading live rows, and
//! neither has a public or test-only constructor anywhere. A request payload that carries an account id, a
//! workspace id or a `ScopeKey` has therefore no route into a context, which is the property Tasks 5 and 6
//! build their admission and scoped reads on.
//!
//! **A refusal may say its class and nothing else.** Everything a caller learns from a failure is in
//! [`crate::error::ServiceError`], and the credential paths deliberately collapse the cases that would make
//! an unauthenticated endpoint an enumeration oracle: an unknown login, a wrong password and a stored value
//! that is not a digest this build can verify are one answer.

pub mod admission;
pub mod credentials;
pub mod http;
pub mod mcp_transport;
pub mod service;

pub use admission::{
    ATTEMPT_BUCKETS, AUTH_ATTEMPT_WINDOW, AUTH_ATTEMPTS_PER_MINUTE, Admission,
    EXPENSIVE_ACTIVE_PER_ACCOUNT, EXPENSIVE_IN_FLIGHT_GLOBAL, EXPENSIVE_QUEUED_PER_ACCOUNT,
    HASH_JOBS_CONCURRENT, MAX_BODY_BYTES, Permit, RequestClass, enrollable_login,
};
pub use credentials::{
    BROWSER_ABSOLUTE, BROWSER_IDLE, BrowserGrant, INVITE_TTL, OneTimeSecret, RECOVERY_TTL,
};
pub use http::{AuthMiddleware, Denial, SESSION_ID_HEADER, admit, bearer, invocation, session_id};
pub use mcp_transport::{
    AuthEvents, CredentialKind, CredentialLease, McpSessionRegistry, Observation, Registration,
    Revocation, StdioIdentity, lease_from_extensions, lease_from_request,
    principal_from_extensions, principal_from_request, principal_from_stdio_or_extensions,
    principal_from_stdio_or_request, revalidate,
};
pub use service::{AuthService, MAX_ADMIT_ATTEMPTS};
