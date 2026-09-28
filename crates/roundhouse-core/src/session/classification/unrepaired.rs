// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Priority-ordered settlements with indexed removal by call identity.
//!
//! The index avoids scanning an outage backlog for each acknowledgement. The
//! ordered map preserves positive-amount-first, then-oldest-arrival repair
//! scheduling — see [`UnrepairedSettlements::priority`] for why the amount
//! is part of the sort key rather than a filter applied after it.

use std::collections::{BTreeMap, HashMap};

use crate::classify::UnconfirmedSettlement;
use crate::ids::ResponseId;

/// `(owes_nothing, arrival)`: `false` sorts before `true`, so every positive
/// amount precedes every zero-dollar one, and arrival order still breaks
/// ties within each group.
type PriorityKey = (bool, u64);

/// Settlements the log says nobody has confirmed, positive amounts first and
/// oldest arrival first within each group.
#[derive(Debug, Default)]
pub(super) struct UnrepairedSettlements {
    /// Priority order determines which pending settlements are offered
    /// first: every positive amount before every zero-dollar one, oldest
    /// arrival first within each group.
    entries: BTreeMap<PriorityKey, UnconfirmedSettlement>,
    /// The priority key for each pending call, removed with its settlement.
    index: HashMap<ResponseId, PriorityKey>,
    /// Monotonic arrival keys, reconstructed in the same order during replay.
    next: u64,
    /// Settlements visited during removal, excluding map lookup comparisons.
    ///
    /// **Test-only.** By construction it equals the number of successful
    /// removals — `remove` increments it exactly once per indexed hit, so a
    /// regression to a linear scan would only move it if the scan's own
    /// author chose to count each visit. It costs a production field and a
    /// public accessor for a guard that is self-reported rather than
    /// observed, so it is gated out of a production build entirely rather
    /// than kept as dead state nothing reads.
    #[cfg(test)]
    examined: u64,
}

impl UnrepairedSettlements {
    /// The sort key one settlement occupies: positive amounts group ahead of
    /// zero-dollar ones, so a real debt is never scheduled behind a
    /// zero-dollar release that merely arrived first.
    ///
    /// **The same predicate `owes_settlement` uses** (`usd > 0.0`), so the
    /// question "does this session owe a repair" and the question "which
    /// entry does the repair loop offer first" can never disagree about a
    /// single settlement.
    fn priority(usd: f64, arrival: u64) -> PriorityKey {
        (usd <= 0.0, arrival)
    }

    /// Record a settlement whose result said nobody acknowledged the charge.
    pub(super) fn push(&mut self, settlement: UnconfirmedSettlement) {
        let arrival = self.next;
        self.next += 1;
        let key = Self::priority(settlement.usd, arrival);
        // Keep both maps paired even if a caller replaces a pending identity.
        if let Some(previous) = self.index.insert(settlement.call_id.clone(), key) {
            self.entries.remove(&previous);
        }
        self.entries.insert(key, settlement);
    }

    /// Remove the named settlement. Missing and repeated acknowledgements change nothing.
    pub(super) fn remove(&mut self, call_id: &ResponseId) {
        let Some(key) = self.index.remove(call_id) else {
            return;
        };
        #[cfg(test)]
        {
            self.examined += 1;
        }
        self.entries.remove(&key);
    }

    /// Borrowed priority order lets scheduling select a prefix without
    /// copying the backlog: every positive amount before any zero-dollar
    /// entry, oldest first within each group, in one `O(max_in_flight)`
    /// walk of an already-sorted map rather than a scan of the backlog
    /// looking for one.
    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = &UnconfirmedSettlement> {
        self.entries.values()
    }

    /// Whether `call_id` is still recorded as unrepaired. The same index
    /// `remove` uses, so a delivery path checking one call before appending
    /// its repair costs a lookup and not a scan of the backlog.
    pub(super) fn contains(&self, call_id: &ResponseId) -> bool {
        self.index.contains_key(call_id)
    }

    /// See [`Self::examined`].
    #[cfg(test)]
    pub(super) fn examined(&self) -> u64 {
        self.examined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settlement(call_id: &str, usd: f64) -> UnconfirmedSettlement {
        UnconfirmedSettlement {
            call_id: ResponseId::new(call_id),
            usd,
        }
    }

    /// Every pending settlement must have exactly one matching index entry.
    fn paired(backlog: &UnrepairedSettlements, len: usize, whose: &str) {
        assert_eq!(backlog.entries.len(), len, "{whose}: the backlog itself");
        assert_eq!(
            backlog.index.len(),
            len,
            "{whose}: and exactly one index entry for each of them"
        );
        for (arrival, entry) in &backlog.entries {
            assert_eq!(
                backlog.index.get(&entry.call_id),
                Some(arrival),
                "{whose}: every entry must be reachable by the call id an \
                 acknowledgement names it with"
            );
        }
    }

    #[test]
    fn every_mutation_leaves_one_index_entry_per_backlog_entry() {
        let mut backlog = UnrepairedSettlements::default();
        paired(&backlog, 0, "empty");

        for index in 1..=4 {
            backlog.push(settlement(&format!("eval_{index}"), index as f64));
        }
        paired(&backlog, 4, "after four arrivals");

        backlog.remove(&ResponseId::new("eval_2"));
        paired(&backlog, 3, "after an interior removal");

        backlog.remove(&ResponseId::new("eval_2"));
        paired(&backlog, 3, "after acknowledging the same call twice");

        backlog.remove(&ResponseId::new("eval_never_seen"));
        paired(&backlog, 3, "after acknowledging a call it never held");

        for index in [1, 3, 4] {
            backlog.remove(&ResponseId::new(format!("eval_{index}")));
        }
        paired(&backlog, 0, "drained");
    }

    /// Replacing an existing call must not leave an unreachable settlement.
    #[test]
    fn a_second_arrival_under_one_call_id_leaves_no_orphan() {
        let mut backlog = UnrepairedSettlements::default();
        backlog.push(settlement("eval_1", 1.0));
        backlog.push(settlement("eval_1", 2.0));
        paired(&backlog, 1, "one call id, one entry");

        assert_eq!(
            backlog.iter().map(|entry| entry.usd).collect::<Vec<_>>(),
            vec![2.0],
            "and it is the newer record, not the displaced one"
        );

        backlog.remove(&ResponseId::new("eval_1"));
        paired(&backlog, 0, "and one acknowledgement drains it");
    }

    /// **A positive amount is never scheduled behind a zero-dollar entry
    /// that merely arrived first.** A zero-dollar release is the routine
    /// result of a call whose own deadline fired before an answer came
    /// back, not a rare failure — oldest-arrival-only order would let a
    /// wall of these delay the one entry that is an actual debt, one
    /// `max_in_flight`-sized turn at a time.
    #[test]
    fn a_positive_amount_precedes_every_zero_dollar_entry_that_arrived_before_it() {
        let mut backlog = UnrepairedSettlements::default();
        backlog.push(settlement("eval_zero_1", 0.0));
        backlog.push(settlement("eval_zero_2", 0.0));
        backlog.push(settlement("eval_zero_3", 0.0));
        backlog.push(settlement("eval_positive", 0.02));

        assert_eq!(
            backlog
                .iter()
                .map(|entry| entry.call_id.to_string())
                .collect::<Vec<_>>(),
            vec![
                "eval_positive".to_string(),
                "eval_zero_1".to_string(),
                "eval_zero_2".to_string(),
                "eval_zero_3".to_string(),
            ],
            "the positive entry must be offered first even though every \
             zero-dollar entry arrived before it"
        );
    }

    /// Within each group -- positive and zero-dollar -- arrival order is
    /// still the tiebreaker: two positive entries keep their own oldest-first
    /// order, and the zero-dollar entries that follow them keep theirs.
    #[test]
    fn each_priority_group_keeps_its_own_oldest_first_order() {
        let mut backlog = UnrepairedSettlements::default();
        backlog.push(settlement("eval_zero_1", 0.0));
        backlog.push(settlement("eval_positive_1", 0.02));
        backlog.push(settlement("eval_zero_2", 0.0));
        backlog.push(settlement("eval_positive_2", 0.05));
        backlog.push(settlement("eval_zero_3", 0.0));

        assert_eq!(
            backlog
                .iter()
                .map(|entry| entry.call_id.to_string())
                .collect::<Vec<_>>(),
            vec![
                "eval_positive_1".to_string(),
                "eval_positive_2".to_string(),
                "eval_zero_1".to_string(),
                "eval_zero_2".to_string(),
                "eval_zero_3".to_string(),
            ]
        );
    }

    /// Interior removal must preserve oldest-first scheduling.
    #[test]
    fn the_backlog_reads_oldest_first_after_an_interior_removal() {
        let mut backlog = UnrepairedSettlements::default();
        for index in 1..=5 {
            backlog.push(settlement(&format!("eval_{index}"), index as f64));
        }
        backlog.remove(&ResponseId::new("eval_3"));

        assert_eq!(
            backlog.iter().map(|entry| entry.usd).collect::<Vec<_>>(),
            vec![1.0, 2.0, 4.0, 5.0]
        );
        assert_eq!(backlog.iter().len(), 4, "and it counts without walking");
    }
}
