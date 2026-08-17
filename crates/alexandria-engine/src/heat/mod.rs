mod columns;
mod decay;

pub use columns::HeatColumns;
pub use decay::{on_access, projected_heat, HeatState};
