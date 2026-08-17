use super::decay::projected_heat;
use super::decay::HeatState;

/// Struct-of-arrays layout for cache-friendly / SIMD-friendly bulk heat computation.
pub struct HeatColumns {
    pub heat: Vec<f64>,
    pub stability: Vec<f64>,
    pub last_touched: Vec<u64>,
}

impl HeatColumns {
    /// Compute projected heat for all entries at the given timestamp.
    /// Returns a Vec of projected heat values in the same order.
    pub fn projected_heat_bulk(&self, now: u64) -> Vec<f64> {
        let len = self.heat.len();
        let mut result = Vec::with_capacity(len);

        for i in 0..len {
            let state = HeatState {
                heat: self.heat[i],
                stability: self.stability[i],
                last_touched: self.last_touched[i],
                access_count: 0, // not needed for projection
            };
            result.push(projected_heat(&state, now));
        }

        result
    }
}
