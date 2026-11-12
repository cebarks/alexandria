//! Bounded work per principal: what the transport may run at all, and how much of it one account may
//! take.
//!
//! Authentication decides *who*; this module decides *how much*. It exists because the expensive paths
//! in this server are not proportional to the request that triggers them: one `retrieve_memories` pays
//! an embedding call, and one `accept_invitation` pays an Argon2 digest at ~19 MiB. Without a bound, a
//! single account — or a single held invitation link, which is the cheapest thing an attacker can
//! present repeatedly — can occupy the worker every other account is waiting on.
//!
//! **The tunables are constants, not configuration.** Owner decision A2 made the same argument for the
//! Argon2 costs that applies here: a knob is a way to weaken a control by accident. Changing one of
//! these numbers is an edit here plus a measurement, not a deployment choice, and the brief's "measure
//! before raising limits" is a later documentation job rather than a runtime affordance.
//!
//! **A refusal costs nothing that a success would pay.** [`Admission::acquire`] answers
//! [`ServiceError::Quota`] — which the transport renders as `429` — when the global queue is full or
//! when the account has already queued the most it may, so the request never reaches the embedding
//! provider. [`enrollable_login`] is the same rule on the password path: a login the store would reject
//! is rejected here, *before* a digest is computed, because a refused enrollment does not spend the link
//! that carried it, and one held secret would otherwise buy one Argon2 run per attempt for the rest of
//! that link's lifetime.
//!
//! **Slots are released by dropping, never by remembering to.** A [`Permit`] releases on `Drop`, and a
//! waiter whose future is cancelled (a client that hung up) removes itself from the queue. Both are
//! tested, because a leaked slot is an outage that arrives slowly and looks like load.
//!
//! **A charge is released outside the scheduler lock, never inside it.** Releasing one is a `Drop` that
//! takes that lock, and `std::sync::Mutex` is not reentrant: a permit that dies while a guard on it is
//! live relocks the mutex its own caller holds, the guard is never released, and every later admission
//! operation in the process blocks forever. The one place a permit can die under the lock is a grant
//! whose receiver vanished *before* the send — `oneshot` hands the undelivered value back to the sender,
//! and the sender is running under the lock — so [`Scheduler::dispatch`] returns those permits as
//! [`Stranded`] and each caller drops its own after its guard is gone. `scheduler_tests` pins that this
//! is the release site, not a theory: the regression hangs rather than miscounts.
//!
//! **Dispatch is round-robin across accounts, and it re-asks who it is serving.** A granted slot goes
//! to the account that has been served least recently among those waiting, and every grant checks the
//! invalidation bus first: work whose credential was revoked while it waited is answered `Stale`
//! instead of being handed a slot.

use crate::auth::mcp_transport::AuthEvents;
use crate::error::ServiceError;
use alexandria_storage::access::Principal;
use alexandria_storage::models::auth::normalize_login;
use alexandria_storage::models::identity::AccountId;
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

/// Largest request body the transport will read: 1 MiB.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// How many expensive requests one account may have running at once.
pub const EXPENSIVE_ACTIVE_PER_ACCOUNT: usize = 2;

/// How many expensive requests this process may hold at once, dispatched or queued. Beyond it a request
/// is refused with 429 before any embedding work starts.
pub const EXPENSIVE_IN_FLIGHT_GLOBAL: usize = 8;

/// How many expensive requests one account may have *queued*. Without this a hot account could fill
/// [`EXPENSIVE_IN_FLIGHT_GLOBAL`] with its own waiters and leave nothing for anybody else's arrival.
pub const EXPENSIVE_QUEUED_PER_ACCOUNT: usize = 2;

/// How many Argon2 jobs may run at once. The policy costs ~19 MiB per job, so this bounds memory as
/// much as it bounds CPU.
pub const HASH_JOBS_CONCURRENT: usize = 2;

/// Login, invitation and recovery attempts allowed per minute, measured along each axis. See
/// [`Admission::limit_auth_attempt`].
pub const AUTH_ATTEMPTS_PER_MINUTE: usize = 5;

/// The window [`AUTH_ATTEMPTS_PER_MINUTE`] is counted over.
pub const AUTH_ATTEMPT_WINDOW: TimeDelta = TimeDelta::minutes(1);

/// Buckets the attempt limiter may hold at once. Beyond it the oldest window is evicted: bounded memory
/// is the requirement, and an evicted key starts a fresh window — weaker than the policy wants, and the
/// only failure mode that does not make the limiter itself the thing under attack.
pub const ATTEMPT_BUCKETS: usize = 4096;

/// What a request is asking for, as far as the queue is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// Protocol traffic: the handshake, notifications, discovery. Bounded by the body limit and the
    /// session registry, not by the work queue.
    Protocol,
    /// A tool call. Expensive because every tool in this server touches the store and most of them
    /// embed; classifying only the tool names known to be embedding-backed would let a tool added
    /// later — or a name this classifier does not know — bypass the budget by being unrecognized.
    Expensive,
}

/// What happened to one waiter. Informational only.
///
/// The account charge is committed **inside** [`Scheduler::grant`], by the same `Permit` the receiver is
/// handed, so nothing here may be read as a cue to touch `active`: the base review's P1-1 came from
/// exactly that pattern (`Granted => active += 1`), and restoring it double-charges the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The waiter was given the slot; the permit it produces releases it again.
    Granted,
    /// The waiter was refused — revoked while it waited — and takes nothing.
    Refused,
    /// The waiter's future was already gone before the send.
    Gone,
}

/// Charges a grant could not deliver, to be dropped once the scheduler lock is out of hand.
///
/// `Scheduler::grant` charges an account and hands the permit to the waiter in one step, and a
/// `oneshot::Sender::send` that fails returns the value it could not deliver. That value is a live
/// charge: dropping it releases the slot, which means releasing it takes the scheduler lock — so it must
/// not die inside [`Scheduler::dispatch`], where the caller's guard is held. See the module doc.
///
/// The newtype is load-bearing rather than decorative: `#[must_use]` does not reach a collection's
/// element type, so returning `Vec<Permit>` would let a future caller ignore the strand and put the
/// hang back. Ignoring a value of *this* type is a lint.
#[derive(Default)]
#[must_use = "each of these permits holds a charge; dropping one while the scheduler lock is held relocks it and wedges admission"]
struct Stranded(Vec<Permit>);

impl Stranded {
    fn push(&mut self, permit: Option<Permit>) {
        self.0.extend(permit);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

/// A login the store will accept, checked *before* any password work is paid for.
///
/// The same normalisation and the same bounds as storage's enrollment gate: trimmed, lowercased, at
/// least three characters and at most 64. This is the F4-15 half the transport can enforce — a valid,
/// unspent invitation link presented with a login nobody could ever store must cost one read and no
/// Argon2 job.
///
/// The bounds are restated here rather than imported because storage's own validator is crate-private;
/// `mcp_auth_test::the_login_gate_agrees_with_storages_own_validator` pins the two against each other
/// input by input, so a restatement cannot drift quietly. A change to either side is a migration
/// question (owner decision F3: a stored `bootstrap_fingerprint` carries no rule marker), not an edit.
pub fn enrollable_login(login: &str) -> Result<String, ServiceError> {
    let normalized = normalize_login(login);
    if normalized.is_empty() {
        return Err(ServiceError::InvalidInput(
            "login must not be blank".to_string(),
        ));
    }
    let length = normalized.chars().count();
    if !(3..=64).contains(&length) {
        return Err(ServiceError::InvalidInput(
            "login must be between 3 and 64 characters".to_string(),
        ));
    }
    Ok(normalized)
}

/// One account's place in the expensive queue.
struct Waiter {
    id: u64,
    account: AccountId,
    /// The principal that asked for this slot, so dispatch can re-ask liveness *before* handing work
    /// out: revoked work is refused, never dispatched.
    principal: Principal,
    /// Shared with the scheduler so a grant can hand over a `Permit` that is already *charged* rather
    /// than a signal the receiver turns into one. See [`Scheduler::grant`].
    inner: Arc<Inner>,
    tx: oneshot::Sender<Grant>,
}

/// What a woken waiter receives: its permit, or the reason it was refused.
type Grant = Result<Permit, ServiceError>;

/// Bounded work per principal, and the Argon2 and attempt budgets beside it.
///
/// Cheap to share: the scheduler, the hash semaphore and the attempt limiter all live behind one `Arc`,
/// so every holder drives a single budget rather than a copy of the truth.
#[derive(Clone)]
pub struct Admission {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheduler = self.inner.scheduler();
        f.debug_struct("Admission")
            .field("expensive_active", &scheduler.active_total())
            .field("expensive_queued", &scheduler.waiters.len())
            .field("hash_running", &self.hash_running())
            .finish()
    }
}

struct Inner {
    /// The invalidation bus, consulted at dispatch.
    events: AuthEvents,
    scheduler: Mutex<Scheduler>,
    hash: Arc<Semaphore>,
    /// Held hash permits. Counted rather than inferred from the semaphore's interior, so a test can
    /// assert the bound without depending on tokio's private accounting.
    hash_running: AtomicUsize,
    attempts: Mutex<Attempts>,
}

impl Inner {
    fn scheduler(&self) -> std::sync::MutexGuard<'_, Scheduler> {
        self.scheduler
            .lock()
            .expect("admission scheduler lock poisoned")
    }

    fn attempts(&self) -> std::sync::MutexGuard<'_, Attempts> {
        self.attempts
            .lock()
            .expect("admission attempt limiter lock poisoned")
    }
}

/// The expensive queue. `in_flight` is derived, not stored: a counter that can disagree with the queue
/// it is supposed to describe is the bug that turns a bound into an outage.
#[derive(Default)]
struct Scheduler {
    /// Expensive work dispatched per account, capped by [`EXPENSIVE_ACTIVE_PER_ACCOUNT`].
    active: HashMap<AccountId, usize>,
    /// Waiting, in rotation order. Length is the queued count; per-account counts are derived from it.
    waiters: VecDeque<Waiter>,
    next_id: u64,
}

impl std::fmt::Debug for Scheduler {
    /// Renders the queue's *shape*, not its contents: a waiter holds a `Principal`, which is a
    /// capability, and a oneshot sender that has nothing worth printing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("active", &self.active)
            .field("queued", &self.waiters.len())
            .finish()
    }
}

impl Scheduler {
    fn account_active(&self, account: &AccountId) -> usize {
        self.active.get(account).copied().unwrap_or_default()
    }

    fn account_queued(&self, account: &AccountId) -> usize {
        self.waiters
            .iter()
            .filter(|waiter| &waiter.account == account)
            .count()
    }

    fn active_total(&self) -> usize {
        self.active.values().sum()
    }

    /// Dispatched plus waiting: the number [`EXPENSIVE_IN_FLIGHT_GLOBAL`] bounds.
    fn in_flight(&self) -> usize {
        self.active_total() + self.waiters.len()
    }

    /// Charges a waiter's account and hands the permit itself to the receiver.
    ///
    /// The permit is built *before* the send, and the failure path gives it back: `oneshot` returns the
    /// message it could not deliver, so a receiver that was already gone destroys the permit it never
    /// received and `Held::drop` uncharges the account. That is the whole of P1-1 — charging and
    /// releasing are one object's birth and death, so they cannot diverge, and no drop timing on the
    /// receiver's side can strand a charge.
    ///
    /// A *refused* waiter (revoked while it queued) is answered `Stale` and charged nothing: refused
    /// work is never dispatched.
    ///
    /// The second half of the return is the charge that a failed send handed back, if any. It is handed
    /// *out* rather than dropped here because `&mut self` means the caller's guard on this scheduler is
    /// live, and a charged permit's `Drop` would have to take that same lock. See [`Stranded`].
    fn grant(&mut self, waiter: Waiter, events: &AuthEvents) -> (Outcome, Option<Permit>) {
        let account = waiter.account.clone();
        if !events.entitles(&waiter.principal) {
            // A refusal carries a `ServiceError`, never a permit, so nothing can strand on this path.
            return match waiter.tx.send(Err(ServiceError::Stale)) {
                Ok(()) => (Outcome::Refused, None),
                Err(_) => (Outcome::Gone, None),
            };
        }
        *self.active.entry(account.clone()).or_default() += 1;
        let permit = Permit::held(Arc::clone(&waiter.inner), account);
        match waiter.tx.send(Ok(permit)) {
            // `send` consumed the value, so the receiver owns the charge from here.
            Ok(()) => (Outcome::Granted, None),
            // The receiver was gone before the send, so the permit came back with the message. The
            // caller owns it now and releases it outside the lock.
            Err(lost) => (Outcome::Gone, lost.ok()),
        }
    }

    /// Hands out freed capacity until none can be filled, rotating accounts as it goes.
    ///
    /// Round-robin is the point: a hot account with several waiters must not drain the freed slots one
    /// after another while a second account's single waiter sits behind them. After an account is
    /// served its remaining waiters move to the back, behind every other account's.
    ///
    /// Returns whatever a grant could not deliver. The caller must drop that **after** its guard on this
    /// scheduler is gone; ignoring it is a lint, and dropping it here is the hang.
    fn dispatch(&mut self, events: &AuthEvents) -> Stranded {
        let mut stranded = Stranded::default();
        loop {
            let Some(index) = self.waiters.iter().position(|waiter| {
                self.account_active(&waiter.account) < EXPENSIVE_ACTIVE_PER_ACCOUNT
            }) else {
                return stranded;
            };
            // `index` was chosen against the current `active` map, so a grant here always fits.
            let Some(waiter) = self.waiters.remove(index) else {
                return stranded;
            };
            let account = waiter.account.clone();
            // Rotate first, then grant: everything this account still has waiting goes behind every
            // other account's, and a receiver that wakes cannot observe its own account both served and
            // still at the head.
            let (later_same, others): (VecDeque<Waiter>, VecDeque<Waiter>) =
                std::mem::take(&mut self.waiters)
                    .into_iter()
                    .partition(|pending| pending.account == account);
            self.waiters = others;
            // The charge is already committed inside `grant`; the outcome says nothing about `active`.
            let (_outcome, permit) = self.grant(waiter, events);
            stranded.push(permit);
            self.waiters.extend(later_same);
        }
    }

    /// A cancelled waiter leaves the queue. Absent from the queue means it was already granted, and
    /// then the [`Permit`] it produced is what releases the slot — so this never double-counts.
    fn abandon(&mut self, id: u64) -> bool {
        let Some(index) = self.waiters.iter().position(|waiter| waiter.id == id) else {
            return false;
        };
        self.waiters.remove(index);
        true
    }

    /// A permit's release: the slot it held is free, so dispatch again. See [`Scheduler::dispatch`] for
    /// the returned charges and where they may be dropped.
    fn release(&mut self, account: &AccountId, events: &AuthEvents) -> Stranded {
        let Some(count) = self.active.get_mut(account) else {
            return Stranded::default();
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.active.remove(account);
        }
        self.dispatch(events)
    }

    /// Grants a slot without queueing: the account has room and the global bound has slack.
    fn grant_now(&mut self, account: &AccountId) {
        *self.active.entry(account.clone()).or_default() += 1;
    }
}

/// Cleanup for a waiter whose future was cancelled before it was served.
///
/// It only ever touches the queue. Once a grant has been sent there is nothing here to release, because
/// the object that charges the account is the permit the receiver holds — and if nobody receives it, it
/// dies with the message.
struct QueueGuard {
    inner: Arc<Inner>,
    id: u64,
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        // The block is the whole safety argument: the guard leaves scope at its end, so a permit the
        // dispatch could not deliver is released with nothing holding the scheduler lock.
        let stranded = {
            let mut scheduler = self.inner.scheduler();
            if scheduler.abandon(self.id) {
                scheduler.dispatch(&self.inner.events)
            } else {
                // Still queued, already granted, or never here: nothing was charged, so nothing stranded.
                Stranded::default()
            }
        };
        drop(stranded);
    }
}

/// An acquired slot.
///
/// Cloning is the point: the transport keeps one handle for the request it is admitting and hands another
/// into the request extensions, which rmcp carries into the handler context. The slot is released when
/// the *last* handle goes — so an expensive tool call holds its account's slot for as long as the handler
/// is running, not merely until the response head is written. Dropping the original handle (a refused or
/// finished request) is the other half of the contract: that is the whole of it.
#[derive(Clone)]
#[must_use = "a permit bounds the work it was acquired for; dropping it releases the slot"]
pub struct Permit {
    state: Option<Arc<Held>>,
}

/// What the last handle of a [`Permit`] releases.
struct Held {
    inner: Arc<Inner>,
    account: AccountId,
}

impl Drop for Held {
    fn drop(&mut self) {
        // Releasing this slot dispatches, and a dispatch whose grant lands on a vanished receiver hands
        // back a charged permit. That permit has to die *here*, not inside `release`, whose scheduler
        // guard is still live — so the guard ends at the block and the strand is dropped after it.
        let stranded = {
            let mut scheduler = self.inner.scheduler();
            scheduler.release(&self.account, &self.inner.events)
        };
        drop(stranded);
    }
}

impl std::fmt::Debug for Permit {
    /// Renders the account and whether a slot is held. The scheduler handle is deliberately not shown: it
    /// is a lock, and a `Debug` reachable from inside a dispatch loop would deadlock on it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.state.as_ref() {
            Some(held) => f
                .debug_struct("Permit")
                .field("account", &held.account)
                .finish_non_exhaustive(),
            None => f.debug_struct("Permit").finish_non_exhaustive(),
        }
    }
}

impl Permit {
    /// A permit that holds nothing: the protocol class is bounded by the body limit and the session
    /// registry, not by the work queue.
    fn protocol() -> Self {
        Self { state: None }
    }

    fn held(inner: Arc<Inner>, account: AccountId) -> Self {
        Self {
            state: Some(Arc::new(Held { inner, account })),
        }
    }

    /// The account this permit was acquired for, or `None` for a class that queues nothing.
    pub fn account(&self) -> Option<&AccountId> {
        self.state.as_ref().map(|held| &held.account)
    }

    /// Whether this permit holds an expensive slot. For assertions, so a test can say which class it is
    /// measuring.
    pub fn is_expensive(&self) -> bool {
        self.state.is_some()
    }
}

/// A held Argon2 budget slot.
#[must_use = "the digest bound is held by this permit; dropping it releases the budget"]
pub struct HashPermit {
    _permit: OwnedSemaphorePermit,
    inner: Arc<Inner>,
}

impl Drop for HashPermit {
    fn drop(&mut self) {
        self.inner.hash_running.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Which axis an attempt is counted against.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Axis {
    /// Where the request came from: the peer address, or a forwarded value the operator opted into.
    Source(String),
    /// What it was attempted against: the normalised login.
    Identifier(String),
}

/// One fixed window of attempts.
#[derive(Debug)]
struct Window {
    started: DateTime<Utc>,
    attempts: usize,
}

/// The attempt budget: bounded buckets, counted along both axes.
#[derive(Debug)]
struct Attempts {
    windows: HashMap<Axis, Window>,
    /// Insertion order for eviction, oldest first.
    order: VecDeque<Axis>,
}

impl Attempts {
    fn new() -> Self {
        Self {
            windows: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Records one attempt against one axis and answers whether it was inside the budget.
    ///
    /// A refused attempt still counts. Not counting refusals would let a caller keep the window open
    /// by staying over the limit, which is the opposite of what a budget is for.
    fn count(&mut self, axis: Axis, now: DateTime<Utc>) -> bool {
        if let Some(window) = self.windows.get_mut(&axis) {
            if window.started + AUTH_ATTEMPT_WINDOW > now {
                window.attempts += 1;
                return window.attempts <= AUTH_ATTEMPTS_PER_MINUTE;
            }
            window.started = now;
            window.attempts = 1;
            return true;
        }
        self.insert(axis, now);
        true
    }

    /// Opens a window for `axis`, keeping the map inside [`ATTEMPT_BUCKETS`].
    fn insert(&mut self, axis: Axis, now: DateTime<Utc>) {
        if self.windows.len() >= ATTEMPT_BUCKETS {
            // Expired windows first: they cost nothing to give up.
            self.windows
                .retain(|_, window| window.started + AUTH_ATTEMPT_WINDOW > now);
            self.order.retain(|known| self.windows.contains_key(known));
        }
        while self.windows.len() >= ATTEMPT_BUCKETS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.windows.remove(&oldest);
        }
        self.order.push_back(axis.clone());
        self.windows.insert(
            axis,
            Window {
                started: now,
                attempts: 1,
            },
        );
    }

    fn attempts_for(&self, axis: &Axis) -> usize {
        self.windows
            .get(axis)
            .map(|window| window.attempts)
            .unwrap_or(0)
    }
}

impl Admission {
    /// An admission control whose dispatch-time liveness question is answered by `events`.
    pub fn bound_to(events: AuthEvents) -> Self {
        Self {
            inner: Arc::new(Inner {
                events,
                scheduler: Mutex::new(Scheduler::default()),
                hash: Arc::new(Semaphore::new(HASH_JOBS_CONCURRENT)),
                hash_running: AtomicUsize::new(0),
                attempts: Mutex::new(Attempts::new()),
            }),
        }
    }

    /// Takes a slot for `class`, or refuses before the work.
    ///
    /// `Expensive` is granted immediately when the account has room and the global bound has slack;
    /// otherwise the caller waits its turn in an account-rotated queue, and is refused with
    /// [`ServiceError::Quota`] when the global bound or its own account's queue is full — that refusal
    /// is the transport's `429`, and it happens before any embedding call. A grant hands back a
    /// [`Permit`] whose drop frees the slot.
    pub async fn acquire(
        &self,
        principal: &Principal,
        class: RequestClass,
    ) -> Result<Permit, ServiceError> {
        if class == RequestClass::Protocol {
            return Ok(Permit::protocol());
        }
        let account = principal.account().clone();
        let (rx, id) = {
            let mut scheduler = self.inner.scheduler();
            if !self.inner.events.entitles(principal) {
                return Err(ServiceError::Stale);
            }
            if scheduler.account_active(&account) < EXPENSIVE_ACTIVE_PER_ACCOUNT
                && scheduler.in_flight() < EXPENSIVE_IN_FLIGHT_GLOBAL
            {
                scheduler.grant_now(&account);
                return Ok(Permit::held(Arc::clone(&self.inner), account));
            }
            if scheduler.account_queued(&account) >= EXPENSIVE_QUEUED_PER_ACCOUNT
                || scheduler.in_flight() >= EXPENSIVE_IN_FLIGHT_GLOBAL
            {
                // 429 before the embedding call, not after it.
                return Err(ServiceError::Quota);
            }
            let id = scheduler.next_id;
            scheduler.next_id += 1;
            let (tx, response) = oneshot::channel();
            scheduler.waiters.push_back(Waiter {
                id,
                account: account.clone(),
                principal: principal.clone(),
                inner: Arc::clone(&self.inner),
                tx,
            });
            (response, id)
        };
        // Armed until the grant lands: a client that hangs up while queued must take its place out of
        // the queue, or the bound slowly becomes a permanent reservation for callers who are gone.
        let guard = QueueGuard {
            inner: Arc::clone(&self.inner),
            id,
        };
        match rx.await {
            // The permit arrives already charged, so there is no window in which the account is charged
            // and nothing owns the release: cancelling this future anywhere after the send destroys the
            // permit with it, and the guard above has nothing left to do either way.
            Ok(Ok(permit)) => {
                drop(guard);
                Ok(permit)
            }
            // Refused at dispatch (revoked while queued) — the reason is already in the error.
            Ok(Err(error)) => Err(error),
            // The sender was dropped without granting: the queue moved on without this waiter. Say
            // unavailable rather than pretending it was ever permitted.
            Err(_) => Err(ServiceError::Unavailable),
        }
    }

    /// Waits for one of the [`HASH_JOBS_CONCURRENT`] Argon2 slots.
    ///
    /// This is the bound owner decision A2 asked Task 5 to add: a digest is ~19 MiB of blocking work, so
    /// an unbounded number of concurrent password attempts is an outage waiting for one script. Callers
    /// take this *before* any password work, and after [`enrollable_login`] — so a login that could
    /// never be stored does not consume a slot.
    pub async fn hash_permit(&self) -> Result<HashPermit, ServiceError> {
        let permit = Arc::clone(&self.inner.hash)
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::Unavailable)?;
        self.inner.hash_running.fetch_add(1, Ordering::AcqRel);
        Ok(HashPermit {
            _permit: permit,
            inner: Arc::clone(&self.inner),
        })
    }

    /// How many hash permits are held right now.
    pub fn hash_running(&self) -> usize {
        self.inner.hash_running.load(Ordering::Acquire)
    }

    /// Records one login / invitation / recovery attempt against both its source and its identifier, and
    /// refuses when either axis is past [`AUTH_ATTEMPTS_PER_MINUTE`] in the current window.
    ///
    /// Two axes deliberately: keyed on the pair alone, one source could spray identifiers; keyed on the
    /// identifier alone, many sources could attack one account. Counting both is the version that
    /// resists either, at the cost of being the stricter of the two — the right direction on a path that
    /// pays Argon2.
    ///
    /// `source` must be a value the server derived, not one the client asserted: the peer address from
    /// the listener, or a forwarded header only where the operator has said a proxy is in front of this
    /// process. Behind no proxy, `X-Forwarded-For` is attacker-controlled, and a limiter keyed on it is
    /// a limiter the attacker sets.
    pub fn limit_auth_attempt(
        &self,
        source: &str,
        identifier: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ServiceError> {
        let mut attempts = self.inner.attempts();
        let source_ok = attempts.count(Axis::Source(source.to_string()), now);
        let identifier_ok = attempts.count(Axis::Identifier(identifier.to_string()), now);
        if source_ok && identifier_ok {
            return Ok(());
        }
        Err(ServiceError::Quota)
    }

    /// Attempts counted against one source in its current window. For the assertions that the bound is
    /// real and per-axis.
    pub fn attempts_for_source(&self, source: &str) -> usize {
        self.inner
            .attempts()
            .attempts_for(&Axis::Source(source.to_string()))
    }

    /// How many attempt windows the limiter is holding. Bounded by [`ATTEMPT_BUCKETS`]; the accessor exists
    /// so the bound is asserted by a test rather than believed.
    pub fn tracked_attempt_keys(&self) -> usize {
        self.inner.attempts().windows.len()
    }

    /// Attempts counted against one identifier in its current window.
    pub fn attempts_for_identifier(&self, identifier: &str) -> usize {
        self.inner
            .attempts()
            .attempts_for(&Axis::Identifier(identifier.to_string()))
    }

    /// Expensive work dispatched at once, across every account.
    pub fn expensive_active(&self) -> usize {
        self.inner.scheduler().active_total()
    }

    /// Expensive work waiting at once, across every account.
    pub fn expensive_queued(&self) -> usize {
        self.inner.scheduler().waiters.len()
    }

    /// In-flight expensive work of both kinds: the number [`EXPENSIVE_IN_FLIGHT_GLOBAL`] bounds.
    pub fn expensive_in_flight(&self) -> usize {
        self.inner.scheduler().in_flight()
    }

    /// The invalidation bus this admission asks its liveness question against.
    pub fn events(&self) -> &AuthEvents {
        &self.inner.events
    }
}

#[cfg(test)]
mod scheduler_tests {
    //! The strand: a charge whose grant never reached a receiver.
    //!
    //! `Scheduler::grant` charges an account and hands the permit to that account's waiter in one step.
    //! If the receiver vanished first — the ordinary end of a cancelled `acquire`, because `admit` awaits
    //! `acquire` bare and a client that hangs up drops the future — tokio's `send` fails and returns the
    //! value, and the value is a live charge. Where it dies is the whole test: releasing a charge takes
    //! the scheduler lock, and `std::sync::Mutex` is not reentrant, so a permit that dies while a guard
    //! is live relocks the mutex its own caller holds. That is not a wrong number, it is every later
    //! admission operation in the process blocking forever, silently and unrecoverably.
    //!
    //! These are in-crate because the window is between two private steps of `dispatch`, and an
    //! integration test cannot reach it deterministically: the receiver has to be gone *before* the send,
    //! while the dispatcher is already inside its guard. `a_granted_but_unreceived_acquire_gives_its_slot_back`
    //! in `tests/admission_test.rs` is the other half — a grant that *was* delivered and never received —
    //! and it can neither hang nor strand, because the receiver's own teardown drops the permit outside
    //! any lock. Green there says nothing about the strand here.
    //!
    //! Each case runs its scheduler work on a plain thread behind a channel deadline. A regression is a
    //! hang, and a hang cannot be asserted; the deadline converts it into a failing test instead of a
    //! wedged suite. Every case builds its own `Admission`, so a wedged lock cannot leak into another
    //! one.

    use super::*;
    use crate::auth::mcp_transport::Revocation;
    use alexandria_storage::access::AccessRepo;
    use alexandria_storage::models::identity::OperationId;
    use alexandria_storage::repos::{AuthRepo, IdentityRepo};
    use alexandria_storage::{Database, schema};
    use std::time::Duration;

    /// How long a scheduler call may take before it is called a deadlock. The work itself is a handful of
    /// map operations; anything still running after this is blocked on a lock, not thinking.
    const STRAND_DEADLINE: Duration = Duration::from_secs(30);

    /// A store, an enrolled account, an admission over a fresh bus, and a real principal for it.
    ///
    /// `Principal` has no public constructor by design, so the fixture issues one the way startup does —
    /// the same sequence `bus_tests` in `mcp_transport.rs` runs. Nothing here fabricates authority.
    async fn fixture(login: &str) -> (Admission, Principal, Database) {
        static DIGEST: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
        let digest = DIGEST
            .get_or_init(|| async {
                AuthRepo::create_password_digest("scheduler-test-password")
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
        let account = IdentityRepo::new(db.inner())
            .bootstrap_owner(login, &digest, &OperationId::new(format!("sched-{login}")))
            .await
            .expect("the fixture owner must enrol")
            .account;
        let principal = AccessRepo::new(&db)
            .local_principal(&system, &account)
            .await
            .expect("the configured local account issues a trusted-local principal");
        let admission = Admission::bound_to(AuthEvents::without_sessions());
        (admission, principal, db)
    }

    /// Runs `work` on a plain thread and fails the test if it has not returned by the deadline.
    fn bounded<T: Send + 'static>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, receipt) = std::sync::mpsc::channel::<T>();
        std::thread::spawn(move || {
            let _ = done.send(work());
        });
        match receipt.recv_timeout(STRAND_DEADLINE) {
            Ok(value) => value,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
                "`{what}` did not return within {}s: a charged permit was dropped while a guard on the \
                 non-reentrant scheduler lock was live, so its release re-entered the lock its own caller \
                 holds and admission wedged",
                STRAND_DEADLINE.as_secs()
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!(
                "`{what}`'s worker thread panicked instead of returning; see the panic above"
            ),
        }
    }

    /// The shape of `Held::drop`: releasing one slot dispatches, and the grant it dispatches lands on a
    /// receiver that is already gone. The charge must come back out of the lock and be dropped there.
    #[tokio::test]
    async fn a_release_that_grants_to_a_vanished_receiver_hands_its_charge_back() {
        let (admission, principal, _store) = fixture("strand-release").await;
        let inner = Arc::clone(&admission.inner);
        let account = principal.account().clone();
        let asker = principal.clone();

        let (stranded, charged_while_held, charged_after_drop) = bounded("release", move || {
            let (tx, rx) = oneshot::channel::<Grant>();
            // Gone *before* the send, which is the only closure `send` can fail on.
            drop(rx);
            let permits = {
                let mut scheduler = inner.scheduler();
                // One slot already dispatched, so there is something for this drop to release, and one
                // waiter whose future will never wake to receive it.
                scheduler.grant_now(&account);
                scheduler.waiters.push_back(Waiter {
                    id: 1,
                    account: account.clone(),
                    principal,
                    inner: Arc::clone(&inner),
                    tx,
                });
                scheduler.release(&account, &inner.events)
            };
            // The strand is still a live charge here: the slot it holds has not been given back yet.
            let while_held = inner.scheduler().active_total();
            let stranded = permits.len();
            // The guard above left scope with the block, so *this* is where the charge may die.
            drop(permits);
            (stranded, while_held, inner.scheduler().active_total())
        });

        assert_eq!(
            stranded, 1,
            "a grant to a vanished receiver did not hand its charge back"
        );
        assert_eq!(
            charged_while_held, 1,
            "the strand was not a live charge: a permit that carries nothing cannot be the thing that \
             releases the slot"
        );
        assert_eq!(
            charged_after_drop, 0,
            "the stranded charge was not released when it was dropped outside the lock"
        );

        // And the capacity is really back where a caller can use it: the same account can acquire again.
        let again = admission
            .acquire(&asker, RequestClass::Expensive)
            .await
            .expect("a stranded grant took the account's capacity for good");
        assert_eq!(admission.expensive_active(), 1);
        drop(again);
        assert_eq!(admission.expensive_in_flight(), 0);
    }

    /// The release site as production actually reaches it: a real [`Permit`] is dropped, `Held::drop`
    /// releases its slot, that release dispatches, and the grant lands on a sibling whose receiver is gone.
    ///
    /// This is the case that pins *where* `Held::drop` drops the strand it is handed. The synthetic
    /// `release` call above asserts the return value; only a permit produced by `acquire` goes through the
    /// drop that owns the scheduler guard, so a mutation of `Held::drop` alone is invisible to it.
    ///
    /// The permit is moved into the worker thread and never held on the test thread: an unwinding test
    /// thread that still owns a `Permit` would try the same wedged lock and turn a caught regression into a
    /// second hang with no verdict.
    #[tokio::test]
    async fn dropping_a_real_permit_that_dispatches_to_a_vanished_receiver_releases_its_charge() {
        let (admission, principal, _store) = fixture("strand-held").await;
        let inner = Arc::clone(&admission.inner);
        let account = principal.account().clone();
        let sibling_principal = principal.clone();

        let permit = admission
            .acquire(&principal, RequestClass::Expensive)
            .await
            .expect("an account with room is granted at once");
        assert_eq!(admission.expensive_active(), 1);

        // A sibling is already queued with nothing on the other end of it.
        let (sibling_tx, sibling_rx) = oneshot::channel::<Grant>();
        drop(sibling_rx);
        {
            let mut scheduler = inner.scheduler();
            scheduler.waiters.push_back(Waiter {
                id: 12,
                account: account.clone(),
                principal: sibling_principal,
                inner: Arc::clone(&inner),
                tx: sibling_tx,
            });
        }

        let (charged_after, queued_after) = bounded("Permit::drop", move || {
            drop(permit);
            let scheduler = inner.scheduler();
            (scheduler.active_total(), scheduler.waiters.len())
        });

        assert_eq!(
            charged_after, 0,
            "dropping a permit left the account charged for it, or for the grant it dispatched"
        );
        assert_eq!(
            queued_after, 0,
            "the dispatched-to waiter stayed in the queue"
        );
    }

    /// The other caller of `dispatch`: a cancelled waiter's cleanup. Same window, different guard, and a
    /// *sibling* is the waiter the grant lands on — abandoning a queue entry takes that very entry out, so
    /// a guard's own dispatch can only strand somebody else's charge, and a one-waiter version of this test
    /// would pass by never reaching `grant` at all.
    ///
    /// Nothing here keeps a `Permit` alive on the test thread: if the strand is dropped under the lock, the
    /// worker is the thread that wedges, and the test thread has to stay free to report it. A `Permit` on
    /// the unwinding thread would try the same locked scheduler and turn a caught regression into a second
    /// hang with no verdict at all.
    #[tokio::test]
    async fn a_queue_guard_that_dispatches_to_a_vanished_receiver_leaves_nothing_charged() {
        let (admission, principal, _store) = fixture("strand-guard").await;
        let inner = Arc::clone(&admission.inner);
        let account = principal.account().clone();
        let sibling = principal.clone();

        let (charged_after_guard, queued_after_guard) = bounded("QueueGuard::drop", move || {
            // Two waiters for one account, both receivers already gone: id 11 is the future being torn
            // down, and id 10 is the sibling still sitting in the queue behind it.
            let (sibling_tx, sibling_rx) = oneshot::channel::<Grant>();
            drop(sibling_rx);
            let (leaving_tx, leaving_rx) = oneshot::channel::<Grant>();
            drop(leaving_rx);
            let guard = QueueGuard {
                inner: Arc::clone(&inner),
                id: 11,
            };
            {
                let mut scheduler = inner.scheduler();
                scheduler.waiters.push_back(Waiter {
                    id: 10,
                    account: account.clone(),
                    principal,
                    inner: Arc::clone(&inner),
                    tx: sibling_tx,
                });
                scheduler.waiters.push_back(Waiter {
                    id: 11,
                    account,
                    principal: sibling,
                    inner: Arc::clone(&inner),
                    tx: leaving_tx,
                });
            }
            // Dropping the guard takes id 11 out of the queue and dispatches what is left — id 10, whose
            // receiver is gone — inside the same lock hold. The strand dies before this thread reads the
            // counts, which is the half of the remedy this case exists for.
            drop(guard);
            let scheduler = inner.scheduler();
            (scheduler.active_total(), scheduler.waiters.len())
        });

        assert_eq!(
            charged_after_guard, 0,
            "the guard's dispatch left a grant charged that nobody ever received"
        );
        assert_eq!(
            queued_after_guard, 0,
            "a vanished waiter stayed in the queue after its grant"
        );
    }

    /// The positive control, and the reason the two cases above are not vacuous: with a receiver that is
    /// still there, a grant is delivered, the account *is* charged, and nothing strands.
    #[tokio::test]
    async fn a_receiver_that_is_still_there_is_charged_and_strands_nothing() {
        let (admission, principal, _store) = fixture("strand-control").await;
        let inner = Arc::clone(&admission.inner);
        let waiter_account = principal.account().clone();
        let account = waiter_account.clone();

        let (mut rx, stranded, charged) = bounded("dispatch (live receiver)", move || {
            let (tx, rx) = oneshot::channel::<Grant>();
            let stranded = {
                let mut scheduler = inner.scheduler();
                scheduler.waiters.push_back(Waiter {
                    id: 2,
                    account: waiter_account,
                    principal,
                    inner: Arc::clone(&inner),
                    tx,
                });
                scheduler.dispatch(&inner.events)
            };
            let charged = inner.scheduler().active_total();
            (rx, stranded.len(), charged)
        });

        assert_eq!(stranded, 0, "a delivered grant stranded a charge");
        assert_eq!(charged, 1, "a delivered grant did not charge the account");

        let permit = rx
            .try_recv()
            .expect("the grant was not in the channel")
            .expect("a live waiter was answered with a refusal");
        assert_eq!(permit.account(), Some(&account));
        assert!(permit.is_expensive());
        drop(permit);
        assert_eq!(
            admission.expensive_in_flight(),
            0,
            "the delivered permit's drop did not release the slot"
        );
    }

    /// A waiter revoked while it queued is refused, not dispatched: it strands nothing and is never
    /// charged. The invariant the base review's P2-1 needed, asserted against the returned strand so a
    /// future signature change cannot lose it.
    #[tokio::test]
    async fn a_refused_waiter_is_never_charged_and_strands_nothing() {
        let (admission, principal, _store) = fixture("strand-refused").await;
        let inner = Arc::clone(&admission.inner);
        let account = principal.account().clone();
        admission
            .events()
            .revoke(&Revocation::Account(account.clone()));

        let (tx, mut rx) = oneshot::channel::<Grant>();
        let stranded = {
            let mut scheduler = inner.scheduler();
            scheduler.waiters.push_back(Waiter {
                id: 3,
                account: account.clone(),
                principal,
                inner: Arc::clone(&inner),
                tx,
            });
            scheduler.dispatch(&inner.events)
        };

        assert_eq!(stranded.len(), 0, "a refusal produced a charge to strand");
        assert_eq!(
            inner.scheduler().active_total(),
            0,
            "refused work was dispatched, and charged"
        );
        assert_eq!(
            inner.scheduler().waiters.len(),
            0,
            "a refused waiter stayed in the queue"
        );
        let grant = rx.try_recv().expect("the refusal never reached the waiter");
        assert!(
            matches!(grant, Err(ServiceError::Stale)),
            "a revoked waiter was answered with something other than Stale: {grant:?}"
        );
    }
}
