//! Shared refinement ceilings. Storage enforces transactional usage; this module owns defaults.

pub const MAX_FOCALS_PER_PASS: usize = 20;
pub const MAX_NEIGHBORS_PER_FOCAL: usize = 8;
pub const MAX_ATTEMPTS_PER_PASS: usize = 40;
pub const MAX_ATTEMPTS_PER_ACCOUNT: usize = 100;
pub const MAX_INTERACTIVE_ATTEMPTS_PER_ACCOUNT: usize = 40;
pub const MAX_GLOBAL_ATTEMPTS: usize = 500;
pub const MAX_ACCOUNT_INTERACTIVE_QUEUE: usize = 20;
pub const MAX_GLOBAL_QUEUE: usize = 200;
pub const MAX_REQUEST_BYTES: usize = 32 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
pub const MAX_OUTPUT_TOKENS: u32 = 4096;
pub const REQUEST_DEADLINE_SECS: u64 = 120;
pub const PASS_ACTIVE_SECS: u64 = 600;
pub const ROLLING_WINDOW_SECS: u64 = 86_400;
pub const REASSESS_AFTER_SECS: u64 = 30 * 86_400;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refinement_budget_defaults_pin_each_independent_limit() {
        assert_eq!(MAX_FOCALS_PER_PASS, 20);
        assert_eq!(MAX_NEIGHBORS_PER_FOCAL, 8);
        assert_eq!(MAX_ATTEMPTS_PER_PASS, 40);
        assert_eq!(MAX_ATTEMPTS_PER_ACCOUNT, 100);
        assert_eq!(MAX_INTERACTIVE_ATTEMPTS_PER_ACCOUNT, 40);
        assert_eq!(MAX_GLOBAL_ATTEMPTS, 500);
        assert_eq!(MAX_ACCOUNT_INTERACTIVE_QUEUE, 20);
        assert_eq!(MAX_GLOBAL_QUEUE, 200);
        assert_eq!(MAX_REQUEST_BYTES, 32_768);
        assert_eq!(MAX_RESPONSE_BYTES, 65_536);
        assert_eq!(MAX_OUTPUT_TOKENS, 4096);
        assert_eq!(REQUEST_DEADLINE_SECS, 120);
        assert_eq!(PASS_ACTIVE_SECS, 600);
        assert_eq!(ROLLING_WINDOW_SECS, 86400);
        assert_eq!(REASSESS_AFTER_SECS, 2_592_000);
    }
}
