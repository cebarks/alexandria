//! Owner-facing refinement policy service; no inference or corpus mutation.

pub mod service;

pub use service::{EffectiveRefinementPolicy, ReconciledRefinement, RefinementService};
