//! What the scheduler publishes about its own running state.
//!
//! This exists because a scheduler that stopped is otherwise indistinguishable from one that is
//! idle. `summary()` renders the *configured* cadences, so before this the debug dashboard said the
//! same thing about a healthy loop and about one that had panicked itself out three times — the
//! failure `schedule.rs`'s header warns about ("a scheduler that quietly stops running one job is
//! indistinguishable from a healthy one for days"), arrived at by the supervisor's give-up path.
//!
//! Deliberately **clock-free**, like the rest of the engine: every method takes `now` from the
//! caller. That is what makes the transitions testable without sleeping, and it is why the type
//! lives here rather than in the binary — `alexandria-mcp` cannot name a type that lives in a
//! binary crate, the same constraint that forces `ClusterConfig` and the `[dreaming]` cadences to
//! arrive at the dashboard pre-flattened.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The states a reader can distinguish, named so the dashboard cannot flatten them back into one
/// sentence by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerState {
    /// The loop never started. Either `[dreaming] enabled = false`, or the process is still booting.
    NotStarted,
    /// The loop is running and last woke within the silence limit.
    Running {
        /// Seconds since the last completed tick.
        since_last_tick: u64,
        restarts: u32,
    },
    /// The loop is running but has not woken within `silence_limit_secs`. A scheduler whose shortest
    /// interval is 1 h is legitimately silent for an hour, so the limit is derived from the
    /// configured intervals rather than a constant — this is the state that means the sleep loop died.
    Quiet {
        since_last_tick: u64,
        restarts: u32,
        silence_limit_secs: u64,
    },
    /// The supervisor gave up restarting. Housekeeping is stopped for the life of the process.
    GivenUp { restarts: u32, panics: u32 },
}

/// Shared by the loop that writes it and the dashboard that reads it.
///
/// `Send + Sync` by construction (atomics, plus one `Mutex` for the panic payload, which is not
/// `Copy`), and cheap to hand to both `Jobs::spawn` and `DebugContext` as an `Arc`.
#[derive(Debug, Default)]
pub struct Liveness {
    started: AtomicBool,
    last_tick_unix: AtomicU64,
    restarts: AtomicU32,
    gave_up: AtomicU32,
    silence_limit_secs: AtomicU64,
    last_panic: Mutex<Option<String>>,
}

impl Liveness {
    /// A loop that has never started and would be reported quiet after `silence_limit_secs`.
    ///
    /// The limit is passed in rather than derived here because the engine's `Intervals` type owns
    /// the cadences and the binary owns the config; the caller knows both.
    #[must_use]
    pub fn new(silence_limit_secs: u64) -> Self {
        Self {
            silence_limit_secs: AtomicU64::new(silence_limit_secs),
            ..Self::default()
        }
    }

    /// Record that the loop entered its `loop`. Distinct from the first tick: a loop that starts and
    /// then panics before finishing a tick has started, and saying so is the point.
    pub fn note_start(&self, now: u64) {
        self.started.store(true, Ordering::Relaxed);
        self.last_tick_unix.store(now, Ordering::Relaxed);
    }

    /// Record a completed wake of the loop, whether or not any job was due.
    pub fn note_tick(&self, now: u64) {
        self.started.store(true, Ordering::Relaxed);
        self.last_tick_unix.store(now, Ordering::Relaxed);
    }

    /// The supervisor restarted the loop after a panic, carrying `panic`'s payload for display.
    pub fn note_restart(&self, panic: Option<String>) {
        self.restarts.fetch_add(1, Ordering::Relaxed);
        if let Some(message) = panic
            && let Ok(mut slot) = self.last_panic.lock()
        {
            *slot = Some(message);
        }
    }

    /// The supervisor stopped trying. `panics` is how many attempts failed before it gave up.
    pub fn note_give_up(&self, panics: u32) {
        self.gave_up.store(panics, Ordering::Relaxed);
    }

    /// `Some(payload)` if a panic message was captured.
    #[must_use]
    pub fn last_panic(&self) -> Option<String> {
        self.last_panic.lock().ok().and_then(|guard| guard.clone())
    }

    /// Where the scheduler stands as of `now`.
    #[must_use]
    pub fn state(&self, now: u64) -> SchedulerState {
        let restarts = self.restarts.load(Ordering::Relaxed);
        let panics = self.gave_up.load(Ordering::Relaxed);
        if panics > 0 {
            return SchedulerState::GivenUp { restarts, panics };
        }
        if !self.started.load(Ordering::Relaxed) {
            return SchedulerState::NotStarted;
        }
        let last = self.last_tick_unix.load(Ordering::Relaxed);
        let since_last_tick = now.saturating_sub(last);
        let silence_limit_secs = self.silence_limit_secs.load(Ordering::Relaxed);
        if since_last_tick > silence_limit_secs {
            return SchedulerState::Quiet {
                since_last_tick,
                restarts,
                silence_limit_secs,
            };
        }
        SchedulerState::Running {
            since_last_tick,
            restarts,
        }
    }
}

/// One `Liveness` behind the `Arc` both the loop and the dashboard need.
pub type SharedLiveness = Arc<Liveness>;

/// Wall clock in whole seconds, supplied by the caller rather than read here, so the type stays
/// clock-free and its transitions are testable without sleeping.
#[cfg(test)]
mod tests {
    use super::*;

    /// The three states an operator must not be able to confuse, asserted on the transitions rather
    /// than on the rendering: `NotStarted` and `GivenUp` both mean "nothing is running" and say
    /// completely different things about why.
    #[test]
    fn never_started_is_not_the_same_as_stopped() {
        let live = Liveness::new(600);
        assert_eq!(live.state(1_000), SchedulerState::NotStarted);

        live.note_start(1_000);
        assert_eq!(
            live.state(1_100),
            SchedulerState::Running {
                since_last_tick: 100,
                restarts: 0
            }
        );

        let stopped = Liveness::new(600);
        stopped.note_start(1_000);
        stopped.note_restart(Some("boom".to_string()));
        stopped.note_give_up(4);
        assert_eq!(
            stopped.state(1_100),
            SchedulerState::GivenUp {
                panics: 4,
                restarts: 1
            },
            "giving up outranks every other state, whatever the last tick was"
        );
        assert_eq!(
            stopped.last_panic().as_deref(),
            Some("boom"),
            "the payload is kept for the log and the panel"
        );
    }

    /// The limit exists so a long-cadence configuration is not reported as broken. A sweep-only loop
    /// legitimately sleeps for an hour; the same wall clock on a 5-minute loop is a real problem.
    #[test]
    fn quiet_is_measured_against_the_configured_limit_not_a_constant() {
        let hourly = Liveness::new(7_200);
        hourly.note_tick(0);
        assert_eq!(
            hourly.state(3_600),
            SchedulerState::Running {
                since_last_tick: 3_600,
                restarts: 0
            },
            "one hour of silence is normal when the shortest cadence is an hour"
        );

        let fivemin = Liveness::new(600);
        fivemin.note_tick(0);
        assert_eq!(
            fivemin.state(3_600),
            SchedulerState::Quiet {
                since_last_tick: 3_600,
                restarts: 0,
                silence_limit_secs: 600
            },
            "the same hour on a 5-minute loop is the state that means the sleep loop died"
        );
    }

    /// Boundary, because the difference between `Running` and `Quiet` at exactly the limit is what an
    /// operator will read as either "fine" or "broken".
    #[test]
    fn the_limit_is_exclusive_at_the_boundary() {
        let live = Liveness::new(600);
        live.note_tick(0);
        assert!(matches!(live.state(600), SchedulerState::Running { .. }));
        assert!(matches!(live.state(601), SchedulerState::Quiet { .. }));
    }

    /// A restart must not look like a fresh start: the count is the only signal that the loop has
    /// been dying quietly in the background, and `note_start` re-stamps the tick like any wake.
    #[test]
    fn restarts_accumulate_across_a_re_start() {
        let live = Liveness::new(600);
        live.note_start(0);
        live.note_restart(None);
        live.note_start(100);
        live.note_restart(None);
        live.note_tick(200);
        assert_eq!(
            live.state(260),
            SchedulerState::Running {
                since_last_tick: 60,
                restarts: 2
            },
            "two restarts must still be visible after the loop recovered"
        );
    }
}
