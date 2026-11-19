//! Refinement policy and inference boundaries; no corpus mutation.

mod anthropic;
pub mod inference;
mod openai;
pub mod provider;
pub mod service;
pub mod worker;

pub use alexandria_storage::repos::refinement_repo::{
    AttemptId, AttemptView, EnqueueRequest, WorkKind, WorkSelection,
};
pub use service::{EffectiveRefinementPolicy, ReconciledRefinement, RefinementService};
