// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Background-classification state, folded from the log.
//!
//! What a turn's own classifier call produced, what is still outstanding, and
//! this deployment's own extracted read of recent turns — three related but
//! distinct things a classifier's projection draws on (see
//! [`crate::classify::projection`]) — folded here the way [`super::review`]
//! folds the review interval: one type owns the state, `apply` calls one
//! method per event kind, and [`super::SessionState`]'s public accessors
//! delegate.

mod unrepaired;

use std::collections::{HashMap, HashSet};

use crate::classify::{
    AvailableClassification, ClassificationIntent, ClassificationRecord, ClassificationRef,
    EvaluationSpend, PriorTurnMetadata, UnconfirmedSettlement,
};
use crate::ids::ResponseId;
use crate::routing::SelectionSnapshot;

use unrepaired::UnrepairedSettlements;

/// How many turns of local metadata the projection may draw on.
///
/// Small on purpose and for the same reason `session::TURN_TOKEN_WINDOW` is:
/// what a classifier can use is a description of *recent* work, and an
/// unbounded window would make the fold grow with the session while adding
/// nothing a reader of the newest few entries did not already have.
const PRIOR_METADATA_WINDOW: usize = 8;

/// How many of an availability-ordered slice had landed by `cutoff_seq`.
///
/// The key accessor lets tests count search work without timing assertions.
/// Entries must be ordered by availability sequence, as the session fold appends them.
pub(crate) fn landed_through<T>(
    available: &[T],
    cutoff_seq: u64,
    seq_of: impl Fn(&T) -> u64,
) -> usize {
    available.partition_point(|entry| seq_of(entry) <= cutoff_seq)
}

/// Background-classification state for one session.
#[derive(Debug, Default)]
pub(crate) struct ClassificationFold {
    /// Calls this session committed to and has no result for.
    ///
    /// An unanswered intent remains across replay because its request could
    /// already have reached the provider. A result naming another source does
    /// not answer this intent or authorize another request.
    outstanding: HashMap<ResponseId, ClassificationIntent>,
    /// Every call whose result this session attributed to its own intent.
    ///
    /// Failed and unfunded outcomes also complete an intent. A mismatched
    /// result does not, even if its event remains in the log. This set prevents
    /// duplicate delivery from contributing another feature.
    settled: HashSet<ResponseId>,
    /// Usable classifications, in the order their results landed.
    available: Vec<AvailableClassification>,
    /// Settlements the log records as unconfirmed and unresolved.
    ///
    /// **Drained by repair, so it is bounded by the outage rather than by the
    /// session.** An entry appears when a result lands saying nobody
    /// acknowledged its settle, and leaves when a
    /// `SessionEventKind::ClassificationSettlementRepaired` says the ledger
    /// answered. A deployment whose evaluation ledger is healthy never holds
    /// one; a deployment whose ledger is down accumulates one per call, which
    /// is the same posture the serving ledger's own repair already accepts.
    ///
    /// Folded here rather than re-derived at the repair site because the join
    /// it needs is not available later: the amount is on the *result* and the
    /// window is on the *intent*, and the intent is consumed the moment its
    /// result arrives.
    unrepaired: UnrepairedSettlements,
    /// What this deployment's own extractor made of recent turns.
    ///
    /// **Bounded to [`PRIOR_METADATA_WINDOW`], oldest dropped first.** The
    /// classifier's projection is allowed prior *metadata* as well as prior
    /// classifications, and this is the metadata: counts and one heuristic flag,
    /// folded out of the selection snapshot each `Routed` already carries. Held
    /// rather than re-derived because the extractor that produced them is
    /// versioned, and re-running today's extractor over an old turn would answer
    /// a different question than the record does.
    prior_turns: Vec<PriorTurnMetadata>,
}

impl ClassificationFold {
    /// A `ClassificationRequested` event folded in.
    ///
    /// **Folded, never re-dispatched.** A replay reaches this and learns that
    /// a call was committed to; it does not make one. Holding the intent is
    /// what lets the engine refuse to buy a second answer for a turn that
    /// already has one outstanding, and what lets a reader say "this turn's
    /// classification, and its cost, are unknown" instead of saying nothing.
    pub(crate) fn requested(&mut self, record: &ClassificationIntent) {
        self.outstanding
            .insert(record.call_id.clone(), record.clone());
    }

    /// A `ClassificationRecorded` event folded in, at `seq`.
    pub(crate) fn recorded(&mut self, seq: u64, record: &ClassificationRecord) {
        // Duplicate delivery must not give one answer extra weight.
        if self.settled.contains(&record.call_id) {
            return;
        }
        // Check attribution before consuming the intent or identity.
        // Otherwise a mismatched result would block the valid answer
        // that arrives later, including after replay.
        let answers_the_intent = self.outstanding.get(&record.call_id).is_some_and(|intent| {
            intent.source_turn_index == record.source_turn_index
                && intent.source_response_id == record.source_response_id
        });
        if !answers_the_intent {
            return;
        }
        // Joined here because this is the last moment both halves are
        // in hand: the amount is on the result and the window is on the
        // intent, and the next line consumes the intent. A repair site
        // that tried to re-derive this would find the intent gone.
        let intent = self.outstanding.remove(&record.call_id);
        if let (Some(intent), Some(usd)) = (
            intent,
            record
                .outcome
                .spend()
                .and_then(EvaluationSpend::unconfirmed_settlement_usd),
        ) {
            self.unrepaired.push(UnconfirmedSettlement {
                call_id: record.call_id.clone(),
                usd,
                window: intent.reservation.budget_window,
            });
        }
        self.settled.insert(record.call_id.clone());
        // Only a usable answer becomes a feature. An unusable, failed or
        // unfunded call is retained above as *settled* — so it is never
        // redelivered and never re-bought — and contributes no label,
        // because nobody answered.
        if let Some(classification) = record.outcome.classification() {
            self.available.push(AvailableClassification {
                reference: ClassificationRef {
                    call_id: record.call_id.clone(),
                    source_turn_index: record.source_turn_index,
                    // This event's own sequence. A later decision may
                    // name it; an earlier one cannot, which is the whole
                    // no-backdating rule expressed as a number.
                    available_seq: seq,
                },
                classification: *classification,
            });
        }
    }

    /// A `ClassificationSettlementRepaired` event folded in.
    ///
    /// The ledger answered, whichever way. **Both answers end the
    /// question**: `applied` means the charge is now committed, and
    /// `!applied` means it already was and only the acknowledgement
    /// had been lost. Retaining an entry on `!applied` would re-drive
    /// the same settle on every later turn forever, for a call the
    /// ledger has told us twice it already has.
    pub(crate) fn repaired(&mut self, call_id: &ResponseId) {
        self.unrepaired.remove(call_id);
    }

    /// A `Routed` event folded in: this deployment's own read of the turn it
    /// selected on, when one is attached.
    ///
    /// One entry per *turn*, not per dispatch: a failover writes several
    /// `Routed` carrying one selection, and three copies of one turn's counts
    /// would read as three turns of work.
    pub(crate) fn routed(&mut self, selection: Option<&SelectionSnapshot>) {
        let Some(selection) = selection else {
            return;
        };
        if self
            .prior_turns
            .last()
            .is_some_and(|last| last.turn_index == selection.features.turn_index)
        {
            return;
        }
        let signals = &selection.features.signals;
        self.prior_turns.push(PriorTurnMetadata {
            turn_index: selection.features.turn_index,
            extractor_revision: selection.features.extractor_revision,
            turn_depth: signals.turn_depth,
            edit_count: signals.tools.edit_count,
            read_count: signals.tools.read_count,
            severity: signals.tools.severity,
            tests_passed_heuristic: signals.tools.tests_passed,
        });
        if self.prior_turns.len() > PRIOR_METADATA_WINDOW {
            self.prior_turns.remove(0);
        }
    }

    /// Classifications usable as features, oldest availability first.
    pub(crate) fn available(&self) -> &[AvailableClassification] {
        &self.available
    }

    /// References to the classifications that had landed by `seq`.
    ///
    /// **Takes a cutoff rather than answering "everything now", because a
    /// routing decision must be explainable from what existed when it was
    /// taken.** The engine captures its cutoff before selection; a result that
    /// lands during the same turn therefore cannot enter that turn's evidence,
    /// however quickly it arrives.
    ///
    /// The ordered prefix supports an exact count and reverse traversal without
    /// scanning the older history to select the newest references.
    pub(crate) fn through(
        &self,
        seq: u64,
    ) -> impl DoubleEndedIterator<Item = &ClassificationRef> + ExactSizeIterator {
        let landed = landed_through(&self.available, seq, |available| {
            available.reference.available_seq
        });
        self.available[..landed]
            .iter()
            .map(|available| &available.reference)
    }

    /// Whether this session has accepted `call_id`'s answer.
    ///
    /// The delivery path uses this to avoid duplicate appends. Acceptance is
    /// independent of ledger settlement: an unmatched result can remain in the
    /// log without completing the intent or contributing a feature.
    pub(crate) fn settled(&self, call_id: &ResponseId) -> bool {
        self.settled.contains(call_id)
    }

    /// Evaluation settlements the log says nobody has confirmed, in arrival
    /// order.
    ///
    /// Everything a repair needs and nothing it does not: the call's identity,
    /// the amount the record holds, and the window the intent recorded. The
    /// payer is the session's own principal and is deliberately not repeated
    /// here.
    ///
    /// Borrowed and in arrival order, so a repair path that takes a bounded
    /// prefix copies nothing it did not select and touches nothing it did not
    /// take.
    pub(crate) fn unrepaired(&self) -> impl ExactSizeIterator<Item = &UnconfirmedSettlement> {
        self.unrepaired.iter()
    }

    /// Settlements visited during acknowledgement removal, excluding map
    /// lookup comparisons. Test-only; see
    /// [`UnrepairedSettlements`]'s own field doc.
    #[cfg(test)]
    pub(crate) fn unrepaired_examined(&self) -> u64 {
        self.unrepaired.examined()
    }

    /// Calls with no result, which is the same thing as unknown answers.
    ///
    /// **The durable half of "a crash costs knowledge, not money twice".** A
    /// successor folding this log finds the intent here and learns that a third
    /// party may have been paid and what it answered is unrecoverable. Nothing
    /// iterates this to dispatch anything — that is what makes "replay never
    /// redispatches" a property of the code's shape rather than of a check.
    pub(crate) fn outstanding(&self) -> impl Iterator<Item = &ClassificationIntent> {
        self.outstanding.values()
    }

    /// This deployment's own read of recent turns, oldest first.
    ///
    /// The other half of the projection's permitted prior context — see
    /// [`PriorTurnMetadata`]. Bounded by the fold, so a caller cannot ask for
    /// more of a session's history than the window holds.
    pub(crate) fn prior_turns(&self) -> &[PriorTurnMetadata] {
        &self.prior_turns
    }
}
