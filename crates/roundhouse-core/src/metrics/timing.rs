// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! One response's clock, and the one accumulator both intervals it measures
//! book into.
//!
//! Split out of `fold.rs` for [`Elapsed`]'s reason as much as
//! [`super::cache_evidence`]'s: `fold.rs` is already the fold's busiest file,
//! and a reader chasing what "first output" or "turn elapsed" actually means
//! should not have to find it among the terminal arm's row bookkeeping.

use crate::metrics::fold::Counters;

/// One outcome class's timing, folded down to a total and a count.
///
/// A total and a count rather than a mean: rows merge, sums add exactly, and
/// a mean of means would weight a row that served three turns the same as
/// one that served three hundred. One type for every class this fold times —
/// first output, completed, incomplete — because they were the same three
/// fields written out by hand at each site until now, which is what let the
/// two write sites drift onto two different overflow rules (core-metrics-2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Elapsed {
    pub(super) ms_total: u64,
    pub(super) samples: u64,
    /// Terminals stamped before their own start.
    ///
    /// Counted rather than dropped: a silent drop makes a clock that moved
    /// look identical to a deployment that measured nothing.
    pub(super) rejected: u64,
}

impl Elapsed {
    /// Fold one interval.
    ///
    /// `None` is a stamp that preceded its own start, and it lands in
    /// [`Self::rejected`] rather than as a zero: a zero is a real answer here
    /// — a turn that started and ended inside one millisecond — so spending
    /// it on a clock that moved would make the two indistinguishable.
    fn observe(&mut self, elapsed_ms: Option<u64>) {
        match elapsed_ms {
            Some(elapsed_ms) => {
                // Saturating for the same reason `Self::absorb` is: a wrapped
                // total would report a near-zero mean for the busiest
                // deployment on the fleet, which is the one where the number
                // matters most. One overflow rule for both write paths, so a
                // row that saturated here does not panic or wrap the moment
                // it is merged into a wider scope.
                self.ms_total = self.ms_total.saturating_add(elapsed_ms);
                self.samples += 1;
            }
            None => self.rejected += 1,
        }
    }

    pub(super) fn absorb(&mut self, other: &Elapsed) {
        self.ms_total = self.ms_total.saturating_add(other.ms_total);
        self.samples += other.samples;
        self.rejected += other.rejected;
    }
}

/// How far the first-output interval has got.
///
/// Three states rather than two `Option`s: the first non-empty delta decides
/// the answer once, and a later one must not move it or replace a refusal.
enum FirstOutputState {
    /// Nothing said yet.
    Waiting,
    /// Milliseconds from the start to the first non-empty delta.
    Measured(u64),
    /// That delta was stamped before the start, so there is nothing to fold.
    Rejected,
}

impl FirstOutputState {
    /// Fold this clock's contribution into a row's first-output tally.
    ///
    /// A turn that never spoke books nothing here — not a zero, which would
    /// read as an instant answer — and this is the one place that decision
    /// is made, rather than a tuple the caller has to remember to check.
    fn book(&self, tally: &mut Elapsed) {
        match self {
            FirstOutputState::Measured(elapsed_ms) => tally.observe(Some(*elapsed_ms)),
            FirstOutputState::Rejected => tally.observe(None),
            FirstOutputState::Waiting => {}
        }
    }
}

/// One open response's clock.
///
/// The start stamp outlives the first-output delta: two intervals share one
/// origin, the first text a caller could see and the terminal event, and the
/// second is decided at an event the first has long since passed. One map
/// keyed by response ID, drained once at that event, is what keeps the two
/// from drifting apart.
pub(super) struct TurnClock {
    /// This response's `TurnStarted` append stamp.
    started_at_ms: u64,
    first_output: FirstOutputState,
}

impl TurnClock {
    /// A fresh clock, started at this response's `TurnStarted` stamp.
    pub(super) fn started(at_ms: u64) -> Self {
        Self {
            started_at_ms: at_ms,
            first_output: FirstOutputState::Waiting,
        }
    }

    /// The first non-empty delta closes the interval; a later one must not
    /// move it. Kept idempotent here rather than at the caller, which is
    /// what lets [`super::fold::MetricsFold::apply`] call this on every
    /// non-empty delta without checking `Waiting` itself.
    pub(super) fn spoke_at(&mut self, at_ms: u64) {
        if let FirstOutputState::Waiting = self.first_output {
            self.first_output = match at_ms.checked_sub(self.started_at_ms) {
                Some(elapsed) => FirstOutputState::Measured(elapsed),
                None => FirstOutputState::Rejected,
            };
        }
    }

    /// Book both intervals this clock carries onto one row: first output,
    /// and the terminal span in whichever outcome class the caller names.
    ///
    /// One call rather than two, so a caller cannot book one interval and
    /// forget the other — the two are read off the same clock and always
    /// move together.
    pub(super) fn book(&self, counters: &mut Counters, terminal_at_ms: u64, completed: bool) {
        self.first_output.book(&mut counters.first_output);
        let terminal_ms = terminal_at_ms.checked_sub(self.started_at_ms);
        // Never the same pot. See `Counters::completed_elapsed`.
        match completed {
            true => counters.completed_elapsed.observe(terminal_ms),
            false => counters.incomplete_elapsed.observe(terminal_ms),
        }
    }
}
