use alexandria_engine::heat::{
    DEFAULT_DECAY_TAU_SECS, HeatColumns, HeatState, on_access, projected_heat,
};

#[test]
fn test_new_memory_decays_fast() {
    let state = HeatState::new(1.0, 1.0);
    let heat_after_1_day = projected_heat(&state, 86400, DEFAULT_DECAY_TAU_SECS);
    let heat_after_7_days = projected_heat(&state, 86400 * 7, DEFAULT_DECAY_TAU_SECS);
    assert!(heat_after_1_day < state.heat);
    assert!(heat_after_7_days < heat_after_1_day);
}

/// `decay_tau` is a real knob now, and the curve is an e-folding rather than a halving. Pinned
/// because the key this replaces was named `spacing_halflife_secs`, and the name is the reason the
/// documented direction and the code disagreed (GitHub issue #36).
#[test]
fn test_decay_tau_scales_the_curve() {
    let state = HeatState {
        heat: 1.0,
        stability: 1.0,
        last_touched: 0,
        last_accessed_at: 0,
        access_count: 0,
    };
    let day = 86_400_u64;

    let base = projected_heat(&state, day, DEFAULT_DECAY_TAU_SECS);
    let slow = projected_heat(&state, day, DEFAULT_DECAY_TAU_SECS * 2.0);
    assert!(slow > base, "a longer time constant must decay slower");
    assert!(
        (base - (-1.0_f64).exp()).abs() < 1e-12,
        "stability 1.0 at tau = one day must leave heat/e after a day, got {base}"
    );
}

#[test]
fn test_stability_increases_with_spaced_access() {
    let mut state = HeatState::new(1.0, 1.0);
    on_access(&mut state, 86400, 86400.0);
    let stability_after_1 = state.stability;
    on_access(&mut state, 86400 * 2, 86400.0);
    assert!(state.stability > stability_after_1);
}

#[test]
fn test_burst_access_barely_increases_stability() {
    let mut state = HeatState::new(1.0, 1.0);
    on_access(&mut state, 1, 86400.0);
    on_access(&mut state, 2, 86400.0);
    on_access(&mut state, 3, 86400.0);
    assert!(state.stability < 1.1);
}

#[test]
fn test_high_stability_memory_decays_slowly() {
    let stable = HeatState {
        heat: 1.0,
        stability: 8.0,
        last_touched: 0,
        last_accessed_at: 0,
        access_count: 20,
    };
    let unstable = HeatState {
        heat: 1.0,
        stability: 1.0,
        last_touched: 0,
        last_accessed_at: 0,
        access_count: 1,
    };
    let day = 86400;
    assert!(
        projected_heat(&stable, day * 7, DEFAULT_DECAY_TAU_SECS)
            > projected_heat(&unstable, day * 7, DEFAULT_DECAY_TAU_SECS)
    );
}

#[test]
fn test_bulk_projected_heat() {
    let columns = HeatColumns {
        heat: vec![1.0, 5.0, 2.0],
        stability: vec![1.0, 4.0, 1.0],
        last_touched: vec![0, 0, 0],
    };
    let projected = columns.projected_heat_bulk(86400, DEFAULT_DECAY_TAU_SECS);
    assert_eq!(projected.len(), 3);
    assert!(projected[1] > projected[0]);
}

/// The sweep re-anchors the decay clock every hour. That must not become the spacing
/// reference, or a month between genuine accesses is credited as an hour and `stability`
/// — the only thing that scales `tau` — grows ~24x slower than designed, which is exactly
/// what happens if spacing keeps reading `last_touched`.
#[test]
fn test_sweep_reanchoring_does_not_attenuate_spacing() {
    let day = 86_400_u64;
    let mut state = HeatState {
        heat: 1.0,
        stability: 1.0,
        // Swept an hour ago; never actually accessed.
        last_touched: 30 * day,
        last_accessed_at: 0,
        access_count: 0,
    };

    // 720 hourly sweeps, the same shape the Sweep job applies: materialise the decayed
    // value, then re-anchor the decay clock.
    for hour in 1..=720_u64 {
        let now = hour * 3600;
        state.heat = projected_heat(&state, now, DEFAULT_DECAY_TAU_SECS);
        state.last_touched = now;
    }

    on_access(&mut state, 30 * day + 3600, 86_400.0);

    assert_eq!(state.access_count, 1);
    assert!(
        (state.stability - 2.0).abs() < f64::EPSILON,
        "expected full stability growth from a 30-day real gap, got {}",
        state.stability
    );
}

/// An access stamps both clocks; they coincide at the moment of access and only diverge
/// afterwards, when the sweep moves the decay anchor alone.
#[test]
fn test_on_access_stamps_both_clocks() {
    let mut state = HeatState::new(1.0, 1.0);
    on_access(&mut state, 500_000, 86_400.0);
    assert_eq!(state.last_touched, 500_000);
    assert_eq!(state.last_accessed_at, 500_000);
}

/// Projection is about the stored value's age, not the last time a caller saw the row: a
/// memory accessed long ago but swept recently must project its swept heat, not have decayed
/// for the whole access gap on top.
#[test]
fn test_projection_reads_the_decay_anchor_not_the_access_stamp() {
    let day = 86_400_u64;
    let swept = HeatState {
        heat: 0.5,
        stability: 1.0,
        last_touched: day,
        last_accessed_at: 0,
        access_count: 1,
    };
    let just_accessed = HeatState {
        heat: 0.5,
        stability: 1.0,
        last_touched: day,
        last_accessed_at: day,
        access_count: 1,
    };
    let now = day * 2;
    assert!(
        (projected_heat(&swept, now, DEFAULT_DECAY_TAU_SECS)
            - projected_heat(&just_accessed, now, DEFAULT_DECAY_TAU_SECS))
        .abs()
            < f64::EPSILON,
        "projection must ignore last_accessed_at entirely"
    );
}

#[test]
fn test_bulk_matches_scalar() {
    let states = [
        HeatState {
            heat: 3.0,
            stability: 2.0,
            last_touched: 0,
            last_accessed_at: 0,
            access_count: 5,
        },
        HeatState {
            heat: 1.0,
            stability: 6.0,
            last_touched: 1000,
            last_accessed_at: 0,
            access_count: 15,
        },
    ];
    let columns = HeatColumns {
        heat: states.iter().map(|s| s.heat).collect(),
        stability: states.iter().map(|s| s.stability).collect(),
        last_touched: states.iter().map(|s| s.last_touched).collect(),
    };
    let now = 86400_u64;
    let bulk = columns.projected_heat_bulk(now, DEFAULT_DECAY_TAU_SECS);
    for (i, state) in states.iter().enumerate() {
        let scalar = projected_heat(state, now, DEFAULT_DECAY_TAU_SECS);
        assert!(
            (bulk[i] - scalar).abs() < 1e-6,
            "bulk[{i}] ({}) != scalar ({scalar})",
            bulk[i]
        );
    }
}
