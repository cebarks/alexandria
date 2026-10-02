//! Ebbinghaus-inspired heat + stability model.
//!
//! - `heat`: current intensity (resets to 1.0 on access)
//! - `stability`: how slowly heat decays (grows with spaced repetition)
//! - `last_touched`: Unix timestamp (seconds) anchoring the stored `heat` value
//! - `last_accessed_at`: Unix timestamp (seconds) of the last real access
//! - `access_count`: total number of accesses
//!
//! The two timestamps are deliberately two clocks, not one, and must not be collapsed.
//! `last_touched` answers "how old is the number in `heat`?" and is written by every
//! materialisation — an access, a spreading-activation warm, or the sweep. `last_accessed_at`
//! answers "when did a caller last see this row?" and is written by accesses only, because it is
//! the sole input to the spacing ratio that grows `stability`. Sharing one field between them
//! makes the sweep's hourly re-anchoring the spacing reference, capping the ratio at
//! `sweep_interval / spacing_reference` (~0.04 at the defaults) and silently under-growing
//! stability ~24x. See GitHub issue #43.

/// Base decay time constant, seconds. `tau = stability * this`; lower cools faster.
///
/// Named a time constant and not a half-life on purpose: `h(t) = heat · e^(-t/tau)` is an
/// e-folding, so at `stability = 1.0` one elapsed day leaves `heat/e`, not `heat/2`. The key
/// this replaced was called `spacing_halflife_secs` while doing neither of those things, which
/// is how its documented direction ended up inverted relative to the code. GitHub issue #36.
pub const DEFAULT_DECAY_TAU_SECS: f64 = 86_400.0;

/// Access gap, seconds, at which one access earns FULL stability growth; shorter gaps earn a
/// proportional fraction. Not a half-life either — it is the denominator of a clamped ratio.
pub const DEFAULT_SPACING_REFERENCE_SECS: f64 = 86_400.0;

/// One memory's heat state, as the decay functions see it.
#[derive(Debug, Clone)]
pub struct HeatState {
    pub heat: f64,
    pub stability: f64,
    pub last_touched: u64,
    pub last_accessed_at: u64,
    pub access_count: u64,
}

impl HeatState {
    pub fn new(heat: f64, stability: f64) -> Self {
        Self {
            heat,
            stability,
            last_touched: 0,
            last_accessed_at: 0,
            access_count: 0,
        }
    }
}

/// Compute the projected heat at time `now` (Unix seconds) without mutating state.
///
/// Reads `last_touched` — the anchor of the stored `heat` — and never `last_accessed_at`, so
/// re-anchoring by the sweep is a no-op on this curve: substituting `h(now)` for `h` and `now`
/// for the anchor leaves every later projection identical, because `exp` composes.
///
/// `decay_tau` is the base time constant ([`DEFAULT_DECAY_TAU_SECS`]); the effective constant
/// scales with `stability`, so a stability of 1.0 cools with `tau == decay_tau`.
pub fn projected_heat(state: &HeatState, now: u64, decay_tau: f64) -> f64 {
    if now <= state.last_touched {
        return state.heat;
    }
    let elapsed = (now - state.last_touched) as f64;
    let tau = state.stability * decay_tau;
    state.heat * (-elapsed / tau).exp()
}

/// Record an access event: reset heat, bump stability based on spacing.
///
/// `spacing_reference` controls how much stability grows — spacing measured relative to this
/// value. Typical: 86400.0 (1 day). Spacing is measured from `last_accessed_at`, **not** from
/// `last_touched`: the decay anchor moves on every sweep and every activation warm, and using it
/// here would credit a month-long gap between real accesses as one hour.
pub fn on_access(state: &mut HeatState, now: u64, spacing_reference: f64) {
    // Compute spacing ratio: how far apart this access is from the previous one, relative to
    // the reference gap. Clamped to [0, 1].
    let spacing = if now > state.last_accessed_at {
        let elapsed = (now - state.last_accessed_at) as f64;
        (elapsed / spacing_reference).min(1.0)
    } else {
        0.0
    };

    // Stability grows proportionally to spacing.
    // Burst access (spacing ≈ 0) → almost no growth.
    // Well-spaced access (spacing ≈ 1) → meaningful growth.
    state.stability += spacing;

    // Reset heat to 1.0 on access
    state.heat = 1.0;
    state.last_touched = now;
    state.last_accessed_at = now;
    state.access_count += 1;
}
