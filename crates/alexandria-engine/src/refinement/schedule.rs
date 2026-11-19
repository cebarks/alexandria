//! Refinement enqueue time is independent of the five housekeeping jobs.

pub const DEFAULT_INTERVAL_SECS: u64 = 86_400;

/// Return the second at which the next refinement enqueue request is due.
///
/// A missing stamp means the first request is due immediately. A stamp older than one interval
/// (downtime, a stopped clock) also means "due now": missed requests coalesce into exactly one,
/// the same discipline the reminder scheduler uses. A stamp in the future — a clock moved
/// backwards — defers to the stamped deadline rather than re-requesting early.
#[must_use]
pub fn next_enqueue_due(last_request: Option<u64>, interval: u64, now: u64) -> u64 {
    let Some(last) = last_request else {
        return now;
    };
    // An interval that would overflow the clock space means "never due again", not "due now":
    // saturating to u64::MAX keeps the deadline in the future for every representable `now`.
    last.saturating_add(interval).max(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refinement_enqueue_deadline_coalesces_downtime_and_handles_clock_edges() {
        assert_eq!(next_enqueue_due(Some(100), 60, 130), 160);
        assert_eq!(next_enqueue_due(Some(100), 60, 1_000), 1_000);
        assert_eq!(next_enqueue_due(Some(1_000), 60, 1_000), 1_060);
        assert_eq!(next_enqueue_due(None, 60, 1_000), 1_000);
        assert_eq!(next_enqueue_due(Some(100), 60, 90), 160);
        assert_eq!(
            next_enqueue_due(Some(u64::MAX - 1), 60, u64::MAX - 1),
            u64::MAX
        );
    }
}
