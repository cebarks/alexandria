/// Ebbinghaus-inspired heat + stability model.
///
/// - `heat`: current intensity (resets to 1.0 on access)
/// - `stability`: how slowly heat decays (grows with spaced repetition)
/// - `last_touched`: Unix timestamp (seconds) anchoring the stored `heat` value
/// - `last_accessed_at`: Unix timestamp (seconds) of the last real access
/// - `access_count`: total number of accesses
///
/// The two timestamps are deliberately two clocks, not one, and must not be collapsed.
/// `last_touched` answers "how old is the number in `heat`?" and is written by every
/// materialisation — an access, a spreading-activation warm, or the sweep. `last_accessed_at`
/// answers "when did a caller last see this row?" and is written by accesses only, because it is
/// the sole input to the spacing ratio that grows `stability`. Sharing one field between them
/// makes the sweep's hourly re-anchoring the spacing reference, capping the ratio at
/// `sweep_interval / spacing_reference` (~0.04 at the defaults) and silently under-growing
/// stability ~24x. See GitHub issue #43.
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
/// Uses Ebbinghaus forgetting curve: h(t) = heat * exp(-elapsed / (stability * halflife_base))
/// where halflife_base normalizes the time constant.
pub fn projected_heat(state: &HeatState, now: u64) -> f64 {
    if now <= state.last_touched {
        return state.heat;
    }
    let elapsed = (now - state.last_touched) as f64;
    // Time constant scales with stability. A stability of 1.0 means
    // heat halves roughly every day (86400 seconds).
    let tau = state.stability * 86400.0;
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
