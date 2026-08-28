use alexandria_engine::heat::{on_access, projected_heat, HeatColumns, HeatState};

#[test]
fn test_new_memory_decays_fast() {
    let state = HeatState::new(1.0, 1.0);
    let heat_after_1_day = projected_heat(&state, 86400);
    let heat_after_7_days = projected_heat(&state, 86400 * 7);
    assert!(heat_after_1_day < state.heat);
    assert!(heat_after_7_days < heat_after_1_day);
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
        access_count: 20,
    };
    let unstable = HeatState {
        heat: 1.0,
        stability: 1.0,
        last_touched: 0,
        access_count: 1,
    };
    let day = 86400;
    assert!(projected_heat(&stable, day * 7) > projected_heat(&unstable, day * 7));
}

#[test]
fn test_bulk_projected_heat() {
    let columns = HeatColumns {
        heat: vec![1.0, 5.0, 2.0],
        stability: vec![1.0, 4.0, 1.0],
        last_touched: vec![0, 0, 0],
    };
    let projected = columns.projected_heat_bulk(86400);
    assert_eq!(projected.len(), 3);
    assert!(projected[1] > projected[0]);
}

#[test]
fn test_bulk_matches_scalar() {
    let states = [
        HeatState {
            heat: 3.0,
            stability: 2.0,
            last_touched: 0,
            access_count: 5,
        },
        HeatState {
            heat: 1.0,
            stability: 6.0,
            last_touched: 1000,
            access_count: 15,
        },
    ];
    let columns = HeatColumns {
        heat: states.iter().map(|s| s.heat).collect(),
        stability: states.iter().map(|s| s.stability).collect(),
        last_touched: states.iter().map(|s| s.last_touched).collect(),
    };
    let now = 86400_u64;
    let bulk = columns.projected_heat_bulk(now);
    for (i, state) in states.iter().enumerate() {
        let scalar = projected_heat(state, now);
        assert!(
            (bulk[i] - scalar).abs() < 1e-6,
            "bulk[{i}] ({}) != scalar ({scalar})",
            bulk[i]
        );
    }
}
