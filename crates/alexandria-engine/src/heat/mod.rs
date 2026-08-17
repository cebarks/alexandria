pub mod activation;
mod columns;
mod decay;

pub use activation::{compute_activation_targets, ActivationConfig, ActivationTarget};
pub use columns::HeatColumns;
pub use decay::{on_access, projected_heat, HeatState};
