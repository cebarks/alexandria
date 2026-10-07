pub mod activation;
mod columns;
mod decay;

pub use activation::{ActivationConfig, ActivationTarget, compute_activation_targets};
pub use columns::HeatColumns;
pub use decay::{
    DEFAULT_DECAY_TAU_SECS, DEFAULT_SPACING_REFERENCE_SECS, HeatState, on_access, projected_heat,
};
