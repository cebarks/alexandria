//! Pure pass planning for the partitioned dreaming dispatcher.
//!
//! The dispatcher in the binary composes these decisions with the store's persisted progress; the
//! decisions themselves live here because they are the part worth testing without a database:
//! which partitions a turn visits, in what order, and how the pass's visits are shared out.
//! Nothing here reads a clock, opens a store, or invents authority — the contracts are rotation,
//! bounded shares and once-per-pass visits.
//!
//! Two properties this module exists to keep true:
//!
//! * **Fair rotation.** A pass continues after the workspace the last pass ended on, so two
//!   accounts with equal work alternate rather than the enumeration order deciding a winner every
//!   time. Rotation is by *workspace* because that is the account's whole corpus; scopes within a
//!   workspace keep their enumeration order (workspace id, then scope), which is deterministic.
//! * **Bounded shares.** One account with many projects cannot take every slot of a turn: visits
//!   are interleaved round-robin across workspaces and the turn is cut at `per_turn` visits. A
//!   partition is visited at most once per pass — the cut leaves the rest for a later pass, which
//!   is what the persisted cursor is for.

use std::collections::VecDeque;

/// Rotate `groups` so the pass continues after `last`: the group following `last` moves to the
/// front, the group equal to `last` goes to the back, and an absent or unknown `last` leaves the
/// enumeration order alone.
///
/// `groups` is the workspace list in enumeration order (already deterministic from storage); the
/// return is the order this pass should walk them in.
pub fn rotate_after<T>(mut groups: Vec<T>, last: Option<&T>) -> Vec<T>
where
    T: PartialEq,
{
    let Some(last) = last else {
        return groups;
    };
    let Some(position) = groups.iter().position(|group| group == last) else {
        // A workspace that vanished since the last pass (deleted, or its account suspended) is not
        // an error: the rotation simply has nothing to continue after, so enumeration order stands.
        return groups;
    };
    let mut rotated = groups.split_off(position + 1);
    rotated.extend(groups);
    rotated
}

/// Interleave partitions round-robin across their workspaces, cutting the turn at `per_turn`
/// visits.
///
/// Round one takes each workspace's first pending partition, round two its second, and so on — so
/// a workspace with ten projects and a turn of three slots gets its second slot only after every
/// other workspace has had one. A `per_turn` of zero is an empty turn; the caller's budget, not
/// this function, decides whether even that is legal.
pub fn interleave_rounds<P>(
    partitions: Vec<P>,
    workspace_of: impl Fn(&P) -> usize,
    per_turn: usize,
) -> Vec<P> {
    if per_turn == 0 {
        return Vec::new();
    }
    // One queue per workspace, in first-seen order — first-seen keeps the enumeration's
    // deterministic workspace ordering rather than adopting a hash map's.
    let mut order: Vec<usize> = Vec::new();
    let mut queues: Vec<VecDeque<P>> = Vec::new();
    for partition in partitions {
        let workspace = workspace_of(&partition);
        match order.iter().position(|seen| *seen == workspace) {
            Some(index) => queues[index].push_back(partition),
            None => {
                order.push(workspace);
                queues.push(VecDeque::from([partition]));
            }
        }
    }
    let mut out = Vec::new();
    'rounds: loop {
        let mut served_this_round = 0;
        for queue in &mut queues {
            if let Some(partition) = queue.pop_front() {
                out.push(partition);
                served_this_round += 1;
                if out.len() == per_turn {
                    break 'rounds;
                }
            }
        }
        if served_this_round == 0 {
            // Every queue is empty: the population is exhausted, and the turn ends rather than
            // spinning on empty partitions.
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups() -> Vec<&'static str> {
        vec!["w-a", "w-b", "w-c"]
    }

    #[test]
    fn rotation_continues_after_the_last_visited_workspace() {
        assert_eq!(
            rotate_after(groups(), Some(&"w-a")),
            vec!["w-b", "w-c", "w-a"],
            "the pass after w-a starts at w-b, not at the enumeration front"
        );
        assert_eq!(
            rotate_after(groups(), Some(&"w-b")),
            vec!["w-c", "w-a", "w-b"]
        );
        assert_eq!(
            rotate_after(groups(), Some(&"w-c")),
            vec!["w-a", "w-b", "w-c"],
            "wrapping puts the just-visited workspace last, not first"
        );
    }

    #[test]
    fn rotation_without_or_with_a_vanished_anchor_keeps_enumeration_order() {
        assert_eq!(rotate_after(groups(), None), groups());
        assert_eq!(
            rotate_after(groups(), Some(&"w-gone")),
            groups(),
            "a suspended or deleted workspace is not an error; the order simply stands"
        );
    }

    #[test]
    fn one_accounts_many_projects_cannot_take_every_slot_of_a_turn() {
        // Workspace 0 has three partitions; workspace 1 has one. A turn of two slots must give
        // workspace 1 one of them rather than letting workspace 0's enumeration order eat both.
        let turn = interleave_rounds(vec![0, 1, 2, 9], |p| if *p < 9 { 0 } else { 1 }, 2);
        assert_eq!(turn.len(), 2);
        assert!(
            turn.contains(&9),
            "the lone workspace's only partition is visited in the first round: {turn:?}"
        );
    }

    #[test]
    fn a_turn_cut_leaves_the_rest_for_a_later_pass_and_visits_once() {
        let turn = interleave_rounds(vec![0, 1, 2, 3, 4, 5], |p| *p / 2, 4);
        assert_eq!(turn.len(), 4, "the turn is cut at its share");
        let mut seen = std::collections::HashSet::new();
        for partition in &turn {
            assert!(
                seen.insert(partition),
                "a partition visited twice in one pass"
            );
        }
    }

    #[test]
    fn a_zero_share_is_an_empty_turn_and_exhaustion_ends_the_loop() {
        assert!(interleave_rounds(vec![0, 1], |_| 0, 0).is_empty());
        // More slots than partitions: the turn ends at exhaustion, not at the cut.
        assert_eq!(interleave_rounds(vec![0, 1], |_| 0, 10), vec![0, 1]);
    }
}
