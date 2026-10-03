//! Duplicate grouping and survivor choice for the `Collapse` job.
//!
//! Byte-identical content only. No fuzzy matching, no cosine threshold: near-duplicates are #38's
//! judge's call, and a similarity bar in a background job that soft-deletes memories is a way to
//! lose data on a judgement nobody asked for. Two rows collapse together only when their content is
//! the same bytes.

use std::collections::HashMap;

/// A live fact as `Collapse` sees it. Deliberately not the storage row: the engine must not depend
/// on storage types, and the job does not need the embedding to group by content.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: String,
    pub content: String,
    pub confidence: f64,
    /// Creation time in seconds since the epoch, or [`UNKNOWN_CREATED_AT`].
    pub created_at: i64,
}

/// Stands in for a row with no `created_at`. It sorts *last* among ties, so an undateable row never
/// wins survivorship over one that can be dated — promoting a row nobody can place in time would be
/// the worse error, since the survivor is the one that stays retrievable.
pub const UNKNOWN_CREATED_AT: i64 = i64::MAX;

/// One group of byte-identical facts: which row survives, and which are collapsed into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub survivor: String,
    /// Sorted by id, so a run's audit rows come out in a stable order.
    pub collapsed: Vec<String>,
}

/// Group `candidates` by content and pick a survivor per group.
///
/// Survivor rules, in order: highest `confidence`; then oldest `created_at`; then lowest id. The
/// last one is not a preference, it is what makes the function total — without it, two rows with
/// equal confidence and equal timestamps would be ordered by `HashMap` iteration, so the same
/// corpus could collapse in a different direction on the next run.
///
/// Singletons produce no group: a fact with no byte-identical twin has nothing to collapse into, and
/// returning a group of one would make the caller soft-delete nothing while logging that it did.
///
/// Output order is by survivor id, and the whole result is a pure function of the input *set* —
/// permuting the input produces the same groups, which `grouping_is_independent_of_input_order`
/// pins. A job that runs hourly over a re-ordered read must not produce different audit rows.
pub fn duplicate_groups(candidates: &[Candidate]) -> Vec<DuplicateGroup> {
    let mut by_content: HashMap<&str, Vec<&Candidate>> = HashMap::new();
    for candidate in candidates {
        by_content
            .entry(candidate.content.as_str())
            .or_default()
            .push(candidate);
    }

    let mut groups: Vec<DuplicateGroup> = by_content
        .into_values()
        .filter(|members| members.len() > 1)
        .map(|mut members| {
            members.sort_by(|a, b| {
                b.confidence
                    .total_cmp(&a.confidence)
                    .then(a.created_at.cmp(&b.created_at))
                    .then(a.id.cmp(&b.id))
            });
            let survivor = members[0].id.clone();
            let mut collapsed: Vec<String> = members[1..].iter().map(|m| m.id.clone()).collect();
            collapsed.sort();
            DuplicateGroup {
                survivor,
                collapsed,
            }
        })
        .collect();

    groups.sort_by(|a, b| a.survivor.cmp(&b.survivor));
    groups
}
