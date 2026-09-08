pub mod activation;
mod columns;
mod decay;

pub use activation::{ActivationConfig, ActivationTarget, compute_activation_targets};
pub use columns::HeatColumns;
pub use decay::{HeatState, on_access, projected_heat};
