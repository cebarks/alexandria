//! The live lifetime of an authenticated request: leases, session ownership, and the gates that
//! revoke them.
//!
//! Task 4 proved a credential can be verified; Task 3 proved a verified [`Principal`] cannot be
//! forged. Neither says anything about the moment *after* verification, which is what this module
//! owns: a request admitted minutes ago, or an SSE stream that is still open, acts on a credential
//! that may since have been revoked, suspended, or superseded by a password recovery.
//!
//! **One boundary, one generation.** [`AuthEvents`] holds the generations, the live leases and the
//! session bindings under one lock, and [`AuthEvents::observed`] is the only way an admission learns
//! what it is admitting against. An admission reads the epoch *before* verifying the credential,
//! then registers under that observation. If an invalidation for this subject was published in the
//! meantime, registration refuses; if an invalidation for some *other* subject moved the epoch, the
//! caller re-verifies. Either way there is no path from a credential lookup that succeeded against a
//! since-revoked row to a fresh, uncancellable token — the token is handed out by the same call that
//! checks the generation, inside the same lock hold. That is the property
//! `mcp_auth_test.rs::a_revoked_generation_cannot_register_a_live_lease` pins.
//!
//! **A session id is data; the binding is authority.** [`McpSessionRegistry`] maps an rmcp session
//! id to the *account and the originating credential* that opened it, so a second, equally valid
//! token of the same account cannot take over a session it did not open, and a foreign token cannot
//! use the id at all. Unknown and foreign are the same `NotFound`, because a session id is a private
//! handle on a network surface. Revocation cancels the binding's token and closes the SDK session, so
//! an already-open stream terminates instead of draining whatever the session worker still has queued.
//!
//! **Absence is never local.** [`principal_from_request`] reads the admitted [`Principal`] out of the
//! HTTP `Parts` that rmcp hands every handler context, and refuses when it finds none. The only way a
//! request with no presented credential gets an identity is a transport *explicitly configured* for it
//! — [`StdioIdentity`], whose value can only come from [`AccessRepo::local_principal`], which requires
//! this database instance's startup capability and an account that exists and is active.
//!
//! [`AccessRepo::local_principal`]: alexandria_storage::access::AccessRepo::local_principal

use crate::error::ServiceError;
use alexandria_storage::access::{AccessRepo, OwnerContext, Principal, PrincipalProvenance};
use alexandria_storage::models::auth::credential_kind;
use alexandria_storage::models::identity::{AccountId, CredentialId};
use rmcp::RoleServer;
use rmcp::model::Extensions;
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::SessionId;
use rmcp::transport::streamable_http_server::session::SessionManager;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use tokio_util::sync::CancellationToken;

/// Which half of the credential table a presented secret belongs to.
///
/// The transport decides this from *where the secret arrived* — an `Authorization: Bearer` value on
/// `/mcp` is an agent token, a browser session cookie is a browser session — and storage then
/// requires the row to be of that kind. It is a parameter rather than a try-every-kind lookup because
/// a browser cookie presented as an MCP bearer token must fail, and trying both kinds is how the two
/// halves would become interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// A server-side browser session, minted by a login.
    Browser,
    /// A named agent bearer token. The only kind the MCP HTTP transport admits.
    Agent,
}

impl CredentialKind {
    /// The persisted spelling, so no transport re-types the constant and drifts.
    pub fn storage_kind(self) -> &'static str {
        match self {
            CredentialKind::Browser => credential_kind::BROWSER,
            CredentialKind::Agent => credential_kind::AGENT,
        }
    }
}

/// What an invalidation names: one credential, or an account and everything minted against it.
///
/// A credential revocation must not disturb the account's other live sessions — D2's "a revoked
/// credential is refused while another live credential still authorizes" — while a suspension or a
/// recovery's `auth_epoch` bump must reach all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revocation {
    /// One browser session or agent token.
    Credential(CredentialId),
    /// An account: every lease and every session of every credential it holds.
    Account(AccountId),
}

/// What a credential is tracked against: its own row, or — for a trusted-local principal, which has
/// no row — the account it was issued to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Subject {
    Credential(CredentialId),
    Account(AccountId),
}

impl Subject {
    fn of(principal: &Principal) -> Self {
        match principal.provenance() {
            PrincipalProvenance::Credential(credential) => Self::Credential(credential.clone()),
            PrincipalProvenance::TrustedLocal => Self::Account(principal.account().clone()),
        }
    }
}

/// One subject's mutable admission state.
#[derive(Debug)]
struct SubjectState {
    /// Bumped by every published invalidation. A lease records the generation it was admitted at and
    /// a session bind refuses a lease whose generation has since moved.
    generation: u64,
    /// The invalidation counter when this subject was last revoked, or `None` once no admission can
    /// predate it any more. Registration refuses a lease whose observation is older than it.
    invalidated_at: Option<u64>,
    /// Live leases, held weakly so the map is bounded by work in flight rather than by everything
    /// that ever authenticated.
    ///
    /// Keyed by a unique registration id, **not** by generation. Two leases admitted at one generation
    /// are two pieces of live work — `admit` runs per request — and a generation-keyed map let the
    /// second registration overwrite the first, so a revocation could reach one token and leave the
    /// other running forever. The generation is still what `bind` / `check` compare against: it is the
    /// *authority* stamp; the id is only an ownership handle for cleanup.
    leases: HashMap<u64, LiveLease>,
}

/// One live lease's entry: a weak handle on its token, plus the generation it was admitted at.
///
/// The generation travels with the handle rather than being looked up from the subject, so a debug dump
/// of the bus can say which of two concurrent leases a bind is refusing — and so nothing has to assume
/// every live lease of a subject shares one generation, which is exactly the assumption P0-1 broke.
#[derive(Debug, Clone)]
struct LiveLease {
    generation: u64,
    token: Weak<CancellationToken>,
}

impl SubjectState {
    fn new() -> Self {
        Self {
            generation: 0,
            invalidated_at: None,
            leases: HashMap::new(),
        }
    }

    fn prune_dead(&mut self) {
        self.leases
            .retain(|_, lease| lease.token.strong_count() > 0);
    }

    /// Cancels every live lease, then drops the dead handles.
    ///
    /// This is the whole of P0-1's remedy: one pass over the registrations, each reached by its own id.
    fn cancel_live_leases(&mut self) {
        for lease in self.leases.values() {
            if let Some(token) = lease.token.upgrade() {
                token.cancel();
            }
        }
        self.prune_dead();
    }
}

/// The ownership a session id was bound to when it was created.
#[derive(Debug, Clone)]
struct SessionBinding {
    account: AccountId,
    /// The credential that opened the session, or `None` for a trusted-local lease — which no HTTP
    /// transport can present, so a bind from a hosted token always carries one.
    credential: Option<CredentialId>,
    /// A child of the admitting lease's token: revoking the credential ends the session's streams
    /// even after the request that opened them has finished.
    cancel: CancellationToken,
}

impl SessionBinding {
    /// Whether `other` is the same actor: the same account *and* the same originating credential. Two
    /// different tokens of one account are different actors here, which is the whole point of binding
    /// both halves.
    fn owned_by(&self, account: &AccountId, credential: Option<&CredentialId>) -> bool {
        &self.account == account && self.credential.as_ref() == credential
    }
}

/// The one synchronization boundary of the transport's authority state.
#[derive(Debug)]
struct Events {
    /// Monotonic count of published invalidations. An observation older than the current value means
    /// something moved; `invalidated_at` says whether it was this subject.
    epoch: u64,
    subjects: HashMap<Subject, SubjectState>,
    /// Subjects per account, so an account-level invalidation reaches its credentials without
    /// scanning everything this process has ever seen.
    accounts: HashMap<AccountId, HashSet<Subject>>,
    /// How many admissions are currently between "observed" and "registered" at each epoch, smallest
    /// epoch first. A *count*, not a set: two admissions that observed the same epoch are two windows
    /// that both have to close before a stamp may be forgotten, and a set let the first one's drop
    /// clear every stamp while the second was still verifying.
    ///
    /// This is what bounds how long an invalidation stamp is kept: once nothing is observing, no future
    /// observation can predate a stamp that is already published.
    observing: BTreeMap<u64, usize>,
    bound: HashMap<SessionId, SessionBinding>,
    /// Bind order, so the limit below evicts the oldest rather than an arbitrary entry.
    bound_order: VecDeque<SessionId>,
    /// Live leases, counted rather than scanned. Changed only by a registration coming and going.
    live_leases: usize,
    /// Identity for the next lease registration, so two concurrent leases of one credential cannot
    /// share a map entry. Monotonic and never reused while the bus lives.
    next_lease_id: u64,
}

impl Events {
    /// Drops the bindings `matches` accepts and returns the session ids that must be closed.
    fn drop_matching(
        &mut self,
        mut matches: impl FnMut(&SessionBinding) -> bool,
    ) -> Vec<SessionId> {
        let doomed: Vec<SessionId> = self
            .bound
            .iter()
            .filter(|(_, binding)| matches(binding))
            .map(|(session, _)| session.clone())
            .collect();
        for session in &doomed {
            self.bound_order.retain(|known| known != session);
            if let Some(binding) = self.bound.remove(session) {
                binding.cancel.cancel();
            }
        }
        doomed
    }

    /// Whether any binding still references `subject`. A subject may only be forgotten when nothing
    /// live can still be gated by it.
    fn referenced(&self, subject: &Subject) -> bool {
        self.bound.values().any(|binding| match subject {
            Subject::Credential(credential) => binding.credential.as_ref() == Some(credential),
            Subject::Account(account) => &binding.account == account,
        })
    }

    /// Forgets subjects that can hold nothing live any more — and drops their reverse index entries,
    /// which is the other half of the bound: an index that outlives its subject would grow with every
    /// credential this process ever admitted and would be re-stamped into a fresh `SubjectState` on
    /// every account-level revocation.
    fn sweep_subjects(&mut self) {
        if self.subjects.len() <= SUBJECT_BUDGET {
            return;
        }
        let doomed: Vec<Subject> = self
            .subjects
            .iter()
            .filter(|(subject, state)| Self::prunable(state) && !self.referenced(subject))
            .map(|(subject, _)| subject.clone())
            .collect();
        for subject in doomed {
            self.forget_subject(&subject, None);
        }
    }

    fn prunable(state: &SubjectState) -> bool {
        state.generation == 0
            && state.invalidated_at.is_none()
            && state
                .leases
                .values()
                .all(|lease| lease.token.strong_count() == 0)
    }

    /// Removes a subject and every trace of it, including the account's reverse index entry.
    ///
    /// `owner` is the account whose set holds `subject`. A lease registration knows it, and carrying it is
    /// what keeps an ordinary request off the `accounts` scan: this runs from `LeaseRegistration::drop`
    /// whenever the subject is prunable, which for a stateless request on a never-revoked credential is
    /// every request. `None` is the budget sweep's cold path, where there is no registration to ask and
    /// the owner is resolved by lookup.
    fn forget_subject(&mut self, subject: &Subject, owner: Option<&AccountId>) {
        self.subjects.remove(subject);
        let account = owner.cloned().or_else(|| match subject {
            Subject::Credential(_) => self
                .accounts
                .iter()
                .find(|(_, set)| set.contains(subject))
                .map(|(account, _)| account.clone()),
            Subject::Account(account) => Some(account.clone()),
        });
        let Some(account) = account else {
            return;
        };
        if let Some(set) = self.accounts.get_mut(&account) {
            set.remove(subject);
            if set.is_empty() {
                self.accounts.remove(&account);
            }
        }
    }
}

/// How many subject entries the bus keeps before sweeping the ones that can hold nothing live. A
/// browser login mints a credential, so without a bound this map would grow with every session this
/// process ever admitted; the sweep only forgets entries that provably gate nothing.
const SUBJECT_BUDGET: usize = 8192;

/// Session bindings this process may hold at once. A binding nobody releases (a client that opened a
/// session and never sent `Mcp-Session-Id` again, or one the SDK reaped on its own timer) is a slow
/// leak with no other way to reclaim it, so the limit is enforced by dropping the oldest — which fails
/// *closed*: an evicted session's next request is refused as unknown and the client must re-initialize.
const BOUND_SESSION_LIMIT: usize = 1024;

/// Removes a lease from the bus when its last clone goes, so the live-lease map is bounded by work in
/// flight. Cancelling is *not* done here: a lease outliving its admitting request is the normal case —
/// it is what a session binding is made of — and revocation reaches the stream through the binding's
/// own token.
struct LeaseRegistration {
    subject: Subject,
    /// The account this lease was registered under — the same value `register` put into the reverse
    /// index, so cleanup reaches its entry directly instead of scanning every account's set for it.
    account: AccountId,
    /// This lease's own entry. Removing anything else is P0-1: an entry a sibling registered under the
    /// same generation belongs to work that is still running.
    id: u64,
    events: Arc<Mutex<Events>>,
}

impl Drop for LeaseRegistration {
    fn drop(&mut self) {
        let Ok(mut events) = self.events.lock() else {
            return;
        };
        // The counter is *this* registration's, and only ever changed here: a revocation cancels and
        // clears without subtracting, because the handles it refused are still alive and still counted.
        events.live_leases = events.live_leases.saturating_sub(1);
        if let Some(state) = events.subjects.get_mut(&self.subject) {
            state.leases.remove(&self.id);
            if Events::prunable(state) && !events.referenced(&self.subject) {
                let subject = self.subject.clone();
                events.forget_subject(&subject, Some(&self.account));
            }
        }
    }
}

/// The lifetime of one admitted credential.
///
/// Cloning is cheap and means "this work is still in flight": every clone shares one token, and the
/// bus registration is released with the last of them.
#[derive(Clone)]
pub struct CredentialLease {
    principal: Principal,
    generation: u64,
    cancel: Arc<CancellationToken>,
    _registration: Arc<LeaseRegistration>,
}

impl std::fmt::Debug for CredentialLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The account and the generation are what an operator needs; the principal is a capability
        // and the token is protocol state, so neither is rendered.
        f.debug_struct("CredentialLease")
            .field("account", self.principal.account())
            .field("generation", &self.generation)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

impl CredentialLease {
    /// The authenticated actor this lease was issued to.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The invalidation generation this lease was admitted at.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The credential row behind the lease, or `None` for a trusted-local lease.
    pub fn credential(&self) -> Option<&CredentialId> {
        self.principal.credential()
    }

    /// The account this lease acts for.
    pub fn account(&self) -> &AccountId {
        self.principal.account()
    }

    /// Fires when the credential or the account behind this lease is revoked, suspended, or
    /// superseded by a recovery. Held by every gate that must stop work: the response-body pump, the
    /// admission queue, and — through the session binding — the SDK session itself.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Whether this lease has already been revoked.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// A token for one piece of work that must end with this lease.
    pub fn child_token(&self) -> CancellationToken {
        self.cancel.child_token()
    }
}

/// The outcome of trying to register a lease against an [`Observation`].
#[derive(Debug)]
pub enum Registration {
    /// The lease is live and registered.
    Issued(CredentialLease),
    /// An invalidation for *this* subject was published after the observation. There is nothing to
    /// retry: the credential this lease would have carried is gone.
    Stale,
    /// The epoch moved, but not for this subject — some other credential or account was revoked while
    /// this verification was running. The observation no longer proves anything, so the caller
    /// re-verifies before it registers again.
    Contended,
}

/// An admission's read of the invalidation epoch, taken *before* it verified a credential.
///
/// A guard: dropping it — including on an early return or a cancelled future — takes this observation
/// out of the set that keeps invalidation stamps alive. Holding one across the verification is what
/// makes [`AuthEvents::register`] able to tell "nothing moved" from "something moved that I could not
/// have seen".
pub struct Observation {
    epoch: u64,
    events: Arc<Mutex<Events>>,
}

impl Observation {
    /// The epoch this observation saw. For an assertion or a log line, never for a decision the
    /// caller makes on its own.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        let Ok(mut events) = self.events.lock() else {
            return;
        };
        // Only the last observation at this epoch retires it; a sibling still verifying keeps every
        // stamp it could need to be refused by.
        if let Some(count) = events.observing.get_mut(&self.epoch) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                events.observing.remove(&self.epoch);
            }
        }
        if events.observing.is_empty() {
            // No admission can now predate an already-published stamp, so no stamp has to outlive
            // this moment. The generations stay: they are what a late bind is refused against.
            for state in events.subjects.values_mut() {
                state.invalidated_at = None;
            }
        }
    }
}

/// Live leases, session ownership, and the invalidation bus between them.
///
/// Cloneable and cheap: a request path, the admission queue and the session registry all hold the
/// same boundary rather than a copy of the truth.
#[derive(Clone, Debug)]
pub struct AuthEvents {
    events: Arc<Mutex<Events>>,
    /// The SDK's own sessions, so a revocation reaches rmcp's session machinery instead of merely
    /// marking an in-process map entry dead.
    sessions: Arc<LocalSessionManager>,
}

impl AuthEvents {
    /// A bus whose revocations close sessions in `sessions` — the same `Arc<LocalSessionManager>` the
    /// MCP service is built on.
    pub fn closing_sessions_in(sessions: Arc<LocalSessionManager>) -> Self {
        Self {
            events: Arc::new(Mutex::new(Events {
                epoch: 0,
                subjects: HashMap::new(),
                accounts: HashMap::new(),
                observing: BTreeMap::new(),
                bound: HashMap::new(),
                bound_order: VecDeque::new(),
                live_leases: 0,
                next_lease_id: 1,
            })),
            sessions,
        }
    }

    /// A bus with no SDK sessions: the offline and stdio shapes, where the same generation rules
    /// govern registration but there is no stream to terminate.
    pub fn without_sessions() -> Self {
        Self::closing_sessions_in(Arc::new(LocalSessionManager::default()))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Events>, ServiceError> {
        self.events
            .lock()
            .map_err(|_| ServiceError::Internal("store"))
    }

    /// Read the epoch an admission must register against.
    ///
    /// Call this *before* verifying the credential: an observation taken afterwards would match any
    /// revocation that landed during the verification, which is the whole window this exists to close.
    pub fn observed(&self) -> Observation {
        let epoch = match self.lock() {
            Ok(mut events) => {
                let epoch = events.epoch;
                *events.observing.entry(epoch).or_insert(0) += 1;
                epoch
            }
            Err(_) => {
                // A poisoned bus means an earlier registration panicked while holding the lock. The
                // honest answer is an observation that can never register: `u64::MAX` is above every
                // epoch a publish can produce, so `register` refuses and the caller denies.
                u64::MAX
            }
        };
        Observation {
            epoch,
            events: Arc::clone(&self.events),
        }
    }

    /// Register a lease for a principal verified while `observation` was current.
    ///
    /// The generation check and the token allocation are one lock hold, so there is no window in
    /// which a successful lookup produces a lease that a later revocation cannot reach.
    pub fn register(&self, principal: &Principal, observation: &Observation) -> Registration {
        let Ok(mut events) = self.lock() else {
            return Registration::Stale;
        };
        if observation.epoch == u64::MAX {
            return Registration::Stale;
        }
        let subject = Subject::of(principal);
        let owner = principal.account().clone();
        let account_subject = Subject::Account(owner.clone());
        // Both stamps are asked, and asked the same way: an account-level invalidation has to be visible
        // to an admission whose *credential* subject was already swept out of the map, because that
        // sweep is exactly what bounds the reverse index.
        let stamped_after_observation = |candidate: &Subject| {
            events
                .subjects
                .get(candidate)
                .and_then(|state| state.invalidated_at)
                .is_some_and(|stamp| stamp > observation.epoch)
        };
        if stamped_after_observation(&subject) || stamped_after_observation(&account_subject) {
            return Registration::Stale;
        }
        if events.epoch != observation.epoch {
            return Registration::Contended;
        }
        // The id is taken before the subject entry is borrowed: `entry` holds the map borrowed, and the
        // counter lives beside it.
        let id = events.next_lease_id;
        events.next_lease_id = events.next_lease_id.wrapping_add(1);
        let cancel = Arc::new(CancellationToken::new());
        let state = events
            .subjects
            .entry(subject.clone())
            .or_insert_with(SubjectState::new);
        state.prune_dead();
        let generation = state.generation;
        state.leases.insert(
            id,
            LiveLease {
                generation,
                token: Arc::downgrade(&cancel),
            },
        );
        debug_assert_eq!(
            state.leases.get(&id).map(|lease| lease.generation),
            Some(generation),
            "a lease registration must carry its own generation into the map"
        );
        events
            .accounts
            .entry(owner.clone())
            .or_default()
            .insert(subject.clone());
        events.live_leases += 1;
        Registration::Issued(CredentialLease {
            principal: principal.clone(),
            generation,
            cancel,
            _registration: Arc::new(LeaseRegistration {
                subject,
                account: owner,
                id,
                events: Arc::clone(&self.events),
            }),
        })
    }

    /// Publish an invalidation and return the sessions that must be closed for it.
    ///
    /// Cancelling happens with the lock held (cheap, and it cannot re-enter this bus); the SDK close is
    /// the caller's, through [`AuthEvents::close_sessions`], because it awaits.
    pub fn revoke(&self, target: &Revocation) -> Vec<SessionId> {
        let Ok(mut events) = self.lock() else {
            return Vec::new();
        };
        events.epoch += 1;
        let epoch = events.epoch;
        let subjects: Vec<Subject> = match target {
            Revocation::Credential(credential) => vec![Subject::Credential(credential.clone())],
            Revocation::Account(account) => {
                let mut subjects = vec![Subject::Account(account.clone())];
                if let Some(known) = events.accounts.get(account) {
                    subjects.extend(
                        known
                            .iter()
                            .filter(|subject| {
                                matches!(subject, Subject::Credential(_))
                                    // Only subjects that are actually still here: the index is pruned
                                    // with its subject, and re-stamping a forgotten one would rebuild
                                    // the map this pruning exists to bound.
                                    && events.subjects.contains_key(*subject)
                            })
                            .cloned(),
                    );
                }
                subjects
            }
        };
        for subject in &subjects {
            // Record the stamp even for a subject this bus has never seen: the point is that an
            // admission already in flight must not be able to register against it afterwards.
            let state = events
                .subjects
                .entry(subject.clone())
                .or_insert_with(SubjectState::new);
            state.invalidated_at = Some(epoch);
            state.generation += 1;
            // Cancelled, not forgotten: each handle still alive is still a registration, and its own
            // drop is what decrements the counter and removes the entry. Clearing here would leave a
            // live lease uncounted *and* uncancellable-by-id on the next pass.
            state.cancel_live_leases();
        }
        events.drop_matching(|binding| match target {
            Revocation::Credential(credential) => binding.credential.as_ref() == Some(credential),
            Revocation::Account(account) => &binding.account == account,
        })
    }

    /// Revoke one credential, closing the sessions it opened. Returns the number of sessions closed.
    pub async fn revoke_credential(&self, credential: &CredentialId) -> usize {
        let doomed = self.revoke(&Revocation::Credential(credential.clone()));
        self.close_sessions(doomed).await
    }

    /// Revoke every credential and session of one account: what a suspension or a recovery's
    /// `auth_epoch` bump costs.
    pub async fn revoke_account(&self, account: &AccountId) -> usize {
        let doomed = self.revoke(&Revocation::Account(account.clone()));
        self.close_sessions(doomed).await
    }

    /// Terminate SDK sessions. A session the SDK has already reaped still counts: its binding is gone
    /// either way, and that is the outcome a revocation wanted.
    pub async fn close_sessions(&self, sessions: Vec<SessionId>) -> usize {
        for session in &sessions {
            self.forget(session);
        }
        for session in &sessions {
            // `close_session` is the SDK's own termination path: it ends the session worker, so an
            // open SSE stream stops instead of draining queued frames.
            let _ = self.sessions.close_session(session).await;
        }
        sessions.len()
    }

    /// How many leases are live. Observability for the assertions that a revoked lease leaves nothing
    /// registered behind.
    pub fn live_leases(&self) -> usize {
        self.lock()
            .map(|events| events.live_leases)
            .unwrap_or(usize::MAX)
    }

    /// The current generation of a principal's subject, or `None` if this bus has never seen it.
    pub fn generation(&self, principal: &Principal) -> Option<u64> {
        let Ok(events) = self.lock() else {
            return None;
        };
        events
            .subjects
            .get(&Subject::of(principal))
            .map(|state| state.generation)
    }

    /// Whether `generation` is still current for `principal`. A subject this bus has forgotten is not
    /// "at generation zero": sweeping only reaches subjects that gate nothing.
    pub fn is_current(&self, principal: &Principal, generation: u64) -> bool {
        self.generation(principal) == Some(generation)
    }

    /// Whether this principal still entitles the work it is asking for.
    ///
    /// `true` for a subject this bus has never seen: absence is not revocation, and the per-request
    /// re-verification is what refuses a credential the store no longer entitles. `false` once an
    /// invalidation has been published against the credential or its account — which is what stops a
    /// queued request from being dispatched after its credential was revoked while it waited.
    pub fn entitles(&self, principal: &Principal) -> bool {
        let Ok(events) = self.lock() else {
            return false;
        };
        let revoked = |subject: &Subject| {
            events
                .subjects
                .get(subject)
                .is_some_and(|state| state.invalidated_at.is_some() || state.generation > 0)
        };
        !revoked(&Subject::of(principal))
            && !revoked(&Subject::Account(principal.account().clone()))
    }

    /// Bind a session id to the account *and* originating credential of `lease`.
    ///
    /// This is [`McpSessionRegistry::bind`]; the registry is the named view, the binding lives here so
    /// registration and invalidation stay one boundary.
    pub fn bind(&self, session: &str, lease: &CredentialLease) -> Result<(), ServiceError> {
        if lease.is_cancelled() {
            return Err(ServiceError::Stale);
        }
        let mut events = self.lock()?;
        let subject = Subject::of(lease.principal());
        let current = events
            .subjects
            .get(&subject)
            .map(|state| state.generation)
            .ok_or(ServiceError::Stale)?;
        if current != lease.generation() {
            return Err(ServiceError::Stale);
        }
        let session: SessionId = SessionId::from(session);
        if let Some(bound) = events.bound.get(&session) {
            // Reconnect of a session this lease owns keeps the original token, so a stream that is
            // still open is not orphaned from the gate that ends it.
            return if bound.owned_by(lease.account(), lease.credential()) {
                Ok(())
            } else {
                Err(ServiceError::NotFound)
            };
        }
        while events.bound.len() >= BOUND_SESSION_LIMIT {
            let Some(oldest) = events.bound_order.pop_front() else {
                break;
            };
            if let Some(binding) = events.bound.remove(&oldest) {
                binding.cancel.cancel();
            }
        }
        events.bound_order.push_back(session.clone());
        events.bound.insert(
            session,
            SessionBinding {
                account: lease.account().clone(),
                credential: lease.credential().cloned(),
                cancel: lease.child_token(),
            },
        );
        events.sweep_subjects();
        Ok(())
    }

    /// Whether `lease` may act on `session`: same account, same originating credential, same
    /// generation, and nothing in between has revoked them.
    pub fn check(&self, session: &str, lease: &CredentialLease) -> Result<(), ServiceError> {
        if lease.is_cancelled() {
            return Err(ServiceError::Stale);
        }
        let events = self.lock()?;
        let session: SessionId = SessionId::from(session);
        let Some(bound) = events.bound.get(&session) else {
            return Err(ServiceError::NotFound);
        };
        if !bound.owned_by(lease.account(), lease.credential()) || bound.cancel.is_cancelled() {
            return Err(ServiceError::NotFound);
        }
        let current = events
            .subjects
            .get(&Subject::of(lease.principal()))
            .map(|state| state.generation);
        if current != Some(lease.generation()) {
            return Err(ServiceError::Stale);
        }
        Ok(())
    }

    /// Drop a binding without revoking the credential — an `Mcp-Session-Id` DELETE, where the client
    /// ends its own session.
    pub fn unbind(&self, session: &str) {
        self.forget(&SessionId::from(session));
    }

    fn forget(&self, session: &SessionId) {
        let Ok(mut events) = self.lock() else {
            return;
        };
        events.bound_order.retain(|known| known != session);
        events.bound.remove(session);
    }

    /// How many sessions are bound.
    pub fn bound(&self) -> usize {
        self.lock().map(|events| events.bound.len()).unwrap_or(0)
    }

    /// Whether `session` has a binding at all.
    pub fn is_bound(&self, session: &str) -> bool {
        let Ok(events) = self.lock() else {
            return false;
        };
        events.bound.contains_key(&SessionId::from(session))
    }
}

/// Session-id ownership: the bindings [`AuthEvents`] carries, named for the transport.
///
/// Bindings live in the same lock as the leases and the invalidations, because "may this session be
/// registered, or used?" has one answer only if it is asked at one boundary.
#[derive(Clone, Debug)]
pub struct McpSessionRegistry {
    events: AuthEvents,
}

impl McpSessionRegistry {
    /// A registry over `events`.
    pub fn over(events: &AuthEvents) -> Self {
        Self {
            events: events.clone(),
        }
    }

    /// The boundary this registry registers and revokes through.
    pub fn events(&self) -> &AuthEvents {
        &self.events
    }

    /// Bind a session id returned by rmcp to the account *and* the originating credential of `lease`.
    ///
    /// Refuses a lease that is already cancelled, and one whose generation has moved — this is the
    /// registration boundary, so a stale-but-successful credential lookup cannot hand the SDK a live,
    /// uncancellable session. Re-binding the same session with the same lease is idempotent (a
    /// reconnect does exactly that), and a session id already bound to someone else is refused as
    /// `NotFound`: the same answer as an unknown id, so a session id is not probeable.
    pub fn bind(&self, session: &str, lease: &CredentialLease) -> Result<(), ServiceError> {
        self.events.bind(session, lease)
    }

    /// Whether `lease` may act on `session` right now.
    pub fn check(&self, session: &str, lease: &CredentialLease) -> Result<(), ServiceError> {
        self.events.check(session, lease)
    }

    /// Drop a binding without revoking the credential.
    pub fn unbind(&self, session: &str) {
        self.events.unbind(session);
    }

    /// Revoke a credential or an account: cancels its leases, drops its bindings, and closes the SDK
    /// sessions they had. Returns the number of sessions closed.
    pub async fn revoke(&self, target: &Revocation) -> usize {
        let doomed = self.events.revoke(target);
        self.events.close_sessions(doomed).await
    }

    /// How many sessions are bound.
    pub fn bound(&self) -> usize {
        self.events.bound()
    }

    /// Whether `session` has a binding at all.
    pub fn is_bound(&self, session: &str) -> bool {
        self.events.is_bound(session)
    }

    /// How many leases are live.
    pub fn live_leases(&self) -> usize {
        self.events.live_leases()
    }
}

/// The identity a stdio transport acts as, when the operator configured one.
///
/// It wraps a `Principal` that *startup* was issued: `AccessRepo::local_principal` requires this
/// database instance's `SystemContext` and an account that exists and is active, neither of which a
/// request path can produce. Holding one is therefore not a way to mint another, and nothing here
/// converts an absent credential into a local principal — that conversion is exactly what D2 forbids,
/// and the reason the hosted path refuses.
#[derive(Clone, Debug)]
pub struct StdioIdentity {
    principal: Principal,
}

impl StdioIdentity {
    /// Wrap a principal startup was issued. It takes a `Principal` because the only producers of one
    /// are storage's authentication path and [`AccessRepo::local_principal`].
    pub fn issued(principal: Principal) -> Self {
        Self { principal }
    }

    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// This transport's own identity, for the call sites that need it without a request context.
    pub fn resolve(&self) -> Principal {
        self.principal.clone()
    }
}

/// The principal an rmcp request context was admitted as.
///
/// rmcp copies the inbound `http::request::Parts` into every handler context (see the pinned
/// `streamable_http_server::tower` documentation), and the auth middleware puts the sealed
/// [`Principal`] in those parts' extensions. A context with no `Parts` never came through the HTTP
/// admission path — stdio, or an in-process service — and a `Parts` with no `Principal` was forwarded
/// without being admitted. Both are refusals, never defaults.
pub fn principal_from_request(ctx: &RequestContext<RoleServer>) -> Result<Principal, ServiceError> {
    principal_from_extensions(&ctx.extensions)
}

/// The half of [`principal_from_request`] that needs no service, so the stdio rule and the hosted rule
/// read the same extension the same way.
pub fn principal_from_extensions(extensions: &Extensions) -> Result<Principal, ServiceError> {
    let parts = extensions
        .get::<axum::http::request::Parts>()
        .ok_or(ServiceError::Stale)?;
    parts
        .extensions
        .get::<Principal>()
        .cloned()
        .ok_or(ServiceError::Stale)
}

/// The principal a request acts as on a transport that may also have a configured stdio identity.
///
/// An admitted hosted principal always wins. With none, the configured stdio identity is used — and
/// with neither, `Stale`: authenticate, which is the only answer a surface that must not guess local
/// authority is allowed to give.
pub fn principal_from_stdio_or_request(
    stdio: Option<&StdioIdentity>,
    ctx: &RequestContext<RoleServer>,
) -> Result<Principal, ServiceError> {
    principal_from_stdio_or_extensions(stdio, &ctx.extensions)
}

/// The extensions-level half of [`principal_from_stdio_or_request`], so the rule can be read — and
/// tested — without a live service.
pub fn principal_from_stdio_or_extensions(
    stdio: Option<&StdioIdentity>,
    extensions: &Extensions,
) -> Result<Principal, ServiceError> {
    match principal_from_extensions(extensions) {
        Ok(principal) => Ok(principal),
        Err(_) => match stdio {
            Some(identity) => Ok(identity.resolve()),
            None => Err(ServiceError::Stale),
        },
    }
}

/// The lease an admitted request was issued under.
///
/// A handler's half of [`super::http`]'s cancellation story, and the reason it exists: rmcp cancels a
/// request context's own `ct` when *that request's response* is sent or when the client sends a
/// cancellation notification — **not** when a session is closed, which is what a revocation does. So
/// `ctx.ct` cannot be the handle a writer aborts on: a handler parked on a revoked credential finishes
/// and commits. `mcp_auth_test::revocation_ends_an_open_stream_and_delivers_no_frame_after_it` measures
/// exactly that difference, and this is the handle that survives it.
///
/// Refused with the same `Stale` as a missing principal: a request that was never admitted has no lease
/// to gate on either.
pub fn lease_from_request(
    ctx: &RequestContext<RoleServer>,
) -> Result<CredentialLease, ServiceError> {
    lease_from_extensions(&ctx.extensions)
}

/// The extensions-level half of [`lease_from_request`], so the rule reads the same with or without a
/// live service.
pub fn lease_from_extensions(extensions: &Extensions) -> Result<CredentialLease, ServiceError> {
    let parts = extensions
        .get::<axum::http::request::Parts>()
        .ok_or(ServiceError::Stale)?;
    parts
        .extensions
        .get::<CredentialLease>()
        .cloned()
        .ok_or(ServiceError::Stale)
}

/// Re-reads the live rows behind a lease: the account's status and `auth_epoch`, and the specific
/// credential's ownership, revocation, consumption, expiry, and minted epoch.
///
/// This is the commit-time question a write must answer, and the honest statement of what Task 5 can
/// do about it. Every predicate is re-read from the store — but on the paths that exist today the read
/// and the write are separate statements, so a revocation landing between them is felt by the
/// cancellation gate and by the next authorization, not by the row already being committed. The atomic
/// form, the same recheck *inside* the write transaction, is storage's work and lands with Task 6's
/// scoped writers, which are the first callers that hold a transaction at all. Until then a writer that
/// must not commit after revocation holds this lease's [`CancellationToken`] across the write and
/// treats a cancellation as an abort — which is what the admission queue does with queued work, and
/// what `revoked_work_is_never_dispatched` exercises.
pub async fn revalidate(
    lease: &CredentialLease,
    access: &AccessRepo<'_>,
) -> Result<OwnerContext, ServiceError> {
    // The local short-circuit first, but it is *not* the guarantee: an invalidation published by another
    // process never touches this token, and only the read below catches it. The test that matters for
    // that case writes the row without publishing
    // (`mcp_auth_test::a_revocation_written_elsewhere_is_still_refused_at_the_commit_gate`).
    if lease.is_cancelled() {
        return Err(ServiceError::Stale);
    }
    Ok(access.authorize(lease.principal()).await?)
}

#[cfg(test)]
mod bus_tests {
    //! The two bookkeeping bounds the transport cannot show from the outside: how long an
    //! invalidation stamp is kept (P2-3), and whether the account's reverse index dies with the subject
    //! it points at (P2-2).
    //!
    //! These run in-crate because both are properties of the bus's own maps, and a `Principal` is
    //! obtainable without a test-only constructor: the startup path is public — enrol an account,
    //! claim this instance's authority, ask for its local principal. That is the same sequence
    //! `main.rs` runs, which is the point: nothing here fabricates authority.

    use super::*;
    use alexandria_storage::models::identity::OperationId;
    use alexandria_storage::repos::{AuthRepo, IdentityRepo};
    use surrealdb::types::RecordId;

    /// A store, an enrolled account, and this instance's system capability.
    struct Fixture {
        db: Arc<Database>,
        system: alexandria_storage::access::SystemContext,
        account: AccountId,
    }

    async fn fixture(login: &str) -> Fixture {
        static DIGEST: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
        let digest = DIGEST
            .get_or_init(|| async {
                AuthRepo::create_password_digest("bus-test-password")
                    .await
                    .expect("the policy must produce a digest")
                    .as_str()
                    .to_string()
            })
            .await
            .clone();
        let mut db = Database::connect_embedded()
            .await
            .expect("an in-memory store");
        schema::migrate(db.inner()).await.expect("v009 must apply");
        let system = db.claim_system_authority().expect("one claim per instance");
        let ids = IdentityRepo::new(db.inner())
            .bootstrap_owner(login, &digest, &OperationId::new(format!("bus-{login}")))
            .await
            .expect("the fixture owner must enrol");
        Fixture {
            db: Arc::new(db),
            system,
            account: ids.account,
        }
    }

    impl Fixture {
        async fn principal(&self) -> Principal {
            AccessRepo::new(&self.db)
                .local_principal(&self.system, &self.account)
                .await
                .expect("the configured local account issues a trusted-local principal")
        }
    }

    /// A stamp is kept while *any* admission is still observing the epoch it was published after.
    ///
    /// Two admissions at one epoch are two windows. A set let the first one's drop clear every stamp in
    /// the process, so the second registered as though nothing had moved — the design's first line of
    /// defence intermittently absent, and the reason this is asserted on the bus rather than inferred.
    #[tokio::test]
    async fn a_stamp_outlives_every_observation_it_was_published_after() {
        let fixture = fixture("bus-stamp-race").await;
        let events = AuthEvents::without_sessions();
        let principal = fixture.principal().await;

        let first = events.observed();
        let second = events.observed();
        assert_eq!(first.epoch(), second.epoch());

        // Revoked while both are verifying.
        events.revoke(&Revocation::Account(principal.account().clone()));
        drop(first);

        // The sibling is still inside its window, so the stamp must still bite.
        match events.register(&principal, &second) {
            Registration::Stale => {}
            Registration::Issued(_) => panic!(
                "a lease registered against a revocation published during its own verification window"
            ),
            Registration::Contended => panic!(
                "the subject's own stamp was not consulted; contention is the *unrelated* answer"
            ),
        }
        assert_eq!(
            events.live_leases(),
            0,
            "a refused registration was counted"
        );

        // Once every observation is gone the stamps may go, and the same principal registers cleanly.
        let fresh = events.observed();
        drop(second);
        drop(fresh);
        let again = events.observed();
        assert!(
            matches!(events.register(&principal, &again), Registration::Issued(_)),
            "the bus never let go of a stamp with nothing observing"
        );
    }

    /// The account's reverse index never outlives the subject it points at — on either path into
    /// `forget_subject`.
    ///
    /// Without that, the index grows with every credential the process ever admitted, and an
    /// account-level revocation re-stamps a `SubjectState` per remembered credential — rebuilding, one
    /// revoke at a time, the very map the sweep exists to bound. Asserted as the invariant rather than as
    /// a size, because the size only crosses its budget in production.
    ///
    /// Both arms are pinned because they are reached from different places at different costs: a lease
    /// registration names its own account, which is what keeps an ordinary stateless request off the
    /// index scan, while the budget sweep has no registration to ask and resolves the owner by lookup.
    #[test]
    fn the_account_index_never_outlives_its_subject() {
        for owner_is_named in [true, false] {
            let events = Arc::new(Mutex::new(Events::new_for_test()));
            let account = AccountId::try_from(RecordId::new("account", "indexed"))
                .expect("a fixture account id");
            let subject = Subject::Credential(
                CredentialId::try_from(RecordId::new("credential", "indexed"))
                    .expect("a fixture credential id"),
            );

            {
                let mut guard = events.lock().expect("bus lock");
                guard.subjects.insert(subject.clone(), SubjectState::new());
                guard
                    .accounts
                    .entry(account.clone())
                    .or_default()
                    .insert(subject.clone());
                assert!(guard.referenced_test(&subject));
            }

            // The subject is forgettable — nothing live, nothing bound — and the index has to follow it.
            {
                let mut guard = events.lock().expect("bus lock");
                let owner = if owner_is_named { Some(&account) } else { None };
                guard.forget_subject(&subject, owner);
            }

            let guard = events.lock().expect("bus lock");
            assert!(
                !guard.subjects.contains_key(&subject),
                "the subject survived a forget (owner named: {owner_is_named})"
            );
            assert!(
                !guard
                    .accounts
                    .get(&account)
                    .is_some_and(|set| set.contains(&subject)),
                "the account kept an index entry for a subject that is gone (owner named: \
                 {owner_is_named}): {:?}",
                guard.accounts
            );
            assert!(
                !guard.accounts.contains_key(&account),
                "an account whose every subject is gone is still indexed (owner named: \
                 {owner_is_named}): {:?}",
                guard.accounts
            );
        }
    }
    impl Events {
        fn new_for_test() -> Self {
            Self {
                epoch: 0,
                subjects: HashMap::new(),
                accounts: HashMap::new(),
                observing: BTreeMap::new(),
                bound: HashMap::new(),
                bound_order: VecDeque::new(),
                live_leases: 0,
                next_lease_id: 1,
            }
        }

        fn referenced_test(&self, subject: &Subject) -> bool {
            self.accounts.values().any(|set| set.contains(subject))
        }
    }

    use alexandria_storage::Database;
    use alexandria_storage::schema;
}
