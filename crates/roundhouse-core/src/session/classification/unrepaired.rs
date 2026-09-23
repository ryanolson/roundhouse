// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Arrival-ordered settlements with indexed removal by call identity.
//!
//! The index avoids scanning an outage backlog for each acknowledgement.
//! The ordered map preserves oldest-first repair scheduling.

use std::collections::{BTreeMap, HashMap};

use crate::classify::UnconfirmedSettlement;
use crate::ids::ResponseId;

/// Settlements the log says nobody has confirmed, oldest arrival first.
#[derive(Debug, Default)]
pub(super) struct UnrepairedSettlements {
    /// Arrival order determines which pending settlements are offered first.
    entries: BTreeMap<u64, UnconfirmedSettlement>,
    /// The arrival key for each pending call, removed with its settlement.
    index: HashMap<ResponseId, u64>,
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
    /// Record a settlement whose result said nobody acknowledged the charge.
    pub(super) fn push(&mut self, settlement: UnconfirmedSettlement) {
        let arrival = self.next;
        self.next += 1;
        // Keep both maps paired even if a caller replaces a pending identity.
        if let Some(previous) = self.index.insert(settlement.call_id.clone(), arrival) {
            self.entries.remove(&previous);
        }
        self.entries.insert(arrival, settlement);
    }

    /// Remove the named settlement. Missing and repeated acknowledgements change nothing.
    pub(super) fn remove(&mut self, call_id: &ResponseId) {
        let Some(arrival) = self.index.remove(call_id) else {
            return;
        };
        #[cfg(test)]
        {
            self.examined += 1;
        }
        self.entries.remove(&arrival);
    }

    /// Borrowed arrival order lets scheduling select a prefix without copying the backlog.
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
    use crate::control::BudgetWindow;

    fn settlement(call_id: &str, usd: f64) -> UnconfirmedSettlement {
        UnconfirmedSettlement {
            call_id: ResponseId::new(call_id),
            usd,
            window: BudgetWindow::Total,
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
