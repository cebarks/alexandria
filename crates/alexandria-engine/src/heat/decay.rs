/// Ebbinghaus-inspired heat + stability model.
///
/// - `heat`: current intensity (resets to 1.0 on access)
/// - `stability`: how slowly heat decays (grows with spaced repetition)
/// - `last_touched`: Unix timestamp (seconds) of last access
/// - `access_count`: total number of accesses

#[derive(Debug, Clone)]
pub struct HeatState {
    pub heat: f64,
    pub stability: f64,
    pub last_touched: u64,
    pub access_count: u64,
}

impl HeatState {
    pub fn new(heat: f64, stability: f64) -> Self {
        Self {
            heat,
            stability,
            last_touched: 0,
            access_count: 0,
        }
    }
}

/// Compute the projected heat at time `now` (Unix seconds) without mutating state.
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
/// `spacing_halflife` controls how much stability grows — spacing measured
/// relative to this value. Typical: 86400.0 (1 day).
pub fn on_access(state: &mut HeatState, now: u64, spacing_halflife: f64) {
    // Compute spacing ratio: how far apart this access is from the last one,
    // relative to the halflife. Clamped to [0, 1].
    let spacing = if now > state.last_touched {
        let elapsed = (now - state.last_touched) as f64;
        (elapsed / spacing_halflife).min(1.0)
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
    state.access_count += 1;
}
