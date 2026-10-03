//! The `Appraise` job's decision, as a pure predicate.
//!
//! Phase 1 of GitHub issue #43 is demote-only: the near-identical proposal queue was cut because
//! its only consumer is #38's judge, and a queue nobody reads is a table of stale assertions.
//! What remains is a rule over materialised heat, so it is testable without a database.

/// Projected heat at or below which a memory counts as cold.
///
/// PROVISIONAL, not measured. It is a fraction of the `1.0` a fresh access writes, chosen so that
/// "cold" means roughly an e-folding of `tau` past the last touch rather than any specific corpus
/// property. `docs/minilm-test-data.md` holds the retrieval measurements; nothing there pins this
/// number, and a corpus with different access density will want a different one.
pub const DEFAULT_COLD_HEAT_FLOOR: f64 = 0.05;

/// Stored confidence at or below which a cold, never-accessed memory is demotable.
///
/// PROVISIONAL, and deliberately at the value `store_memory` writes when the caller supplies none:
/// the rule is meant to reach only memories that were never asserted with more than the default
/// confidence AND never used. Anything a caller explicitly raised above the default is exempt.
pub const DEFAULT_DEMOTE_CONFIDENCE_CEILING: f64 = 0.5;

/// The confidence a demoted memory is written at. It sits strictly between nothing and the ceiling,
/// and [`should_demote`] exempts it, which is what makes the job idempotent on its own output.
pub const DEMOTED_CONFIDENCE: f64 = 0.2;

/// Every condition must hold. Each is checked separately so the audit trail can say which one moved.
///
/// - `projected_heat` is the materialised value (what `Sweep` last wrote, or the projection of it),
///   never the raw stored `heat`, which is only as fresh as the last sweep.
/// - `access_count == 0` is the strongest of the three: a memory that has never been returned to a
///   caller is the only kind this rule can honestly call unused. Now that retrieval records
///   accesses, this means "nobody ever retrieved it", not "no heat row exists".
/// - `floor` and `ceiling` come from config, with [`DEFAULT_COLD_HEAT_FLOOR`] and
///   [`DEFAULT_DEMOTE_CONFIDENCE_CEILING`] as the derived defaults. The predicate takes them rather
///   than reading the constants, because a key whose value no code reads is a promise to an operator
///   that the server does not keep.
/// - a row already sitting at [`DEMOTED_CONFIDENCE`] is exempt, so the rule cannot re-arm itself.
///   `Appraise` runs daily and would otherwise rewrite the same confidence, log the same audit row
///   and report the same "acted" count forever — a job that looks busy while doing nothing new.
pub fn should_demote(
    projected_heat: f64,
    access_count: i64,
    confidence: f64,
    floor: f64,
    ceiling: f64,
) -> bool {
    access_count == 0
        && is_cold(projected_heat, floor)
        && confidence > DEMOTED_CONFIDENCE
        && confidence <= ceiling
}

/// Whether one value is cold at the configured floor. Split from [`should_demote`] so the floor can
/// be configured without the predicate having to take every knob at once.
pub fn is_cold(projected_heat: f64, floor: f64) -> bool {
    projected_heat <= floor
}
