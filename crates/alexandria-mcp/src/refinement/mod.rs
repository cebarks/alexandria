//! Refinement policy and inference boundaries; no corpus mutation.

mod anthropic;
pub mod inference;
mod openai;
pub mod service;

pub use service::{EffectiveRefinementPolicy, ReconciledRefinement, RefinementService};
