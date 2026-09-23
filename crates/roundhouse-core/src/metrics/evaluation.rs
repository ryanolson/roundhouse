// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What classifier evaluation calls cost, folded on its own axis.
//!
//! Separate from [`super::fold`]'s token counters because the two are priced by
//! different authorities and must never merge. A serving row holds tokens and
//! [`super::snapshot`] applies the *current* rate card to them; an evaluation
//! call carries the amount it was priced at, under the rate card its own
//! reservation recorded, and nothing here may reprice it. Folding the two into
//! one row would put a number no serving rate card produced into the column the
//! savings claim is computed from.
//!
//! ## Three facts, kept apart
//!
//! - **What the service billed** — [`EvaluationSpend::Measured`] and its `usd`.
//!   Usage the service reported, priced by the call's recorded rate card. It is
//!   an observation, not an invoice: no provider stated that dollar figure.
//! - **What this deployment's ledger did about it** — [`SettlementAck`]. A
//!   settle nobody acknowledged does not erase the usage and does not make the
//!   call free, and a later
//!   [`ClassificationSettlementRepair`] resolves the acknowledgement without
//!   adding a second call or a second dollar.
//! - **What nobody can say** — an intent whose result never landed, and a
//!   result whose usage was absent. Both are counted and neither is filled in
//!   with a zero, because a zero is indistinguishable from a free call.
//!
//! ## Retention, and what it buys
//!
//! [`EvaluationFold::calls`] gains one entry per durable call identity per
//! session and never loses one. That is the same shape — and the same
//! trade — as the fold's sequence watermarks: the entry *is* the once-only
//! guarantee, so pruning it would let a replayed result, a redelivered one or a
//! re-driven repair book a second time. It is not bounded by the classifier's
//! worker capacity, which governs calls in flight and says nothing about calls
//! already made.
//!
//! What each entry holds is the smallest join the contract needs. Before a
//! result: the source turn and response the result must match, and the model
//! the call was requested under. After one: the settlement state alone --
//! while it is open, the amount a repair would resolve lives on the payer's
//! own [`EvaluationAccumulator::open`] entry, not here. No projection, no
//! prompt, no classification and no principal — the payer is resolved from
//! the session's own `SessionCreated` at every event, so a second copy here
//! could not drift from it.

use std::collections::{BTreeMap, HashMap};

use crate::classify::{
    ClassificationIntent, ClassificationOutcome, ClassificationRecord,
    ClassificationSettlementRepair, EvaluationSpend, SettlementAck,
};
use crate::control::PrincipalKey;
use crate::ids::{ResponseId, SessionId};
use crate::metrics::fold::Scope;

/// Which classifier was asked, and which one answered.
///
/// Both halves are the key, because they are two different identities and the
/// interesting deployment is the one where they disagree: a configured alias
/// that resolves to a model nobody expected is visible as its own row rather
/// than absorbed into the row of the name that was asked for.
///
/// `reported` is `None` for a service that named nothing, for one that named an
/// empty string, and for a call that reached no service at all. All three are
/// "no usable identity", and the row's own `refused_calls` is what keeps the
/// third from being mistaken for the first two.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct EvaluationModelKey {
    pub(super) requested: String,
    pub(super) reported: Option<String>,
}

/// One accepted result's booking, on whatever grouping holds it -- the
/// deployment-wide tally and each `(requested, reported)` row alike.
///
/// A single type rather than two, because the two were the same six fields
/// written out by hand in every arm of [`EvaluationFold::recorded`]: adding a
/// seventh meant five edits that had to agree, and [`Self::book`] is now the
/// one place that can disagree with itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct EvaluationCallTally {
    pub(super) calls: u64,
    pub(super) measured_calls: u64,
    pub(super) measured_usd: f64,
    pub(super) unknown_usage_calls: u64,
    pub(super) refused_calls: u64,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
}

impl EvaluationCallTally {
    /// Book one accepted result, by what it spent and nothing else -- the
    /// settlement state is a separate axis, folded in
    /// [`EvaluationFold::recorded`] and [`EvaluationFold::repaired`].
    fn book(&mut self, spend: Option<&EvaluationSpend>) {
        self.calls += 1;
        match spend {
            Some(EvaluationSpend::Measured { usage, usd, .. }) => {
                self.measured_calls += 1;
                self.measured_usd += usd;
                self.input_tokens += usage.input_tokens;
                self.output_tokens += usage.output_tokens;
            }
            Some(EvaluationSpend::Unknown { .. }) => {
                self.unknown_usage_calls += 1;
            }
            // Nothing was sent, so nothing was billed -- the one class that
            // is free rather than unknown, and the one with no settlement.
            None => {
                self.refused_calls += 1;
            }
        }
    }

    fn absorb(&mut self, other: &Self) {
        self.calls += other.calls;
        self.measured_calls += other.measured_calls;
        self.measured_usd += other.measured_usd;
        self.unknown_usage_calls += other.unknown_usage_calls;
        self.refused_calls += other.refused_calls;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// One scope's evaluation spend, add-only over every field.
///
/// **Every accumulated field is add-only, and stays that way even for
/// settlement.** The obvious design decrements an "unconfirmed" pot when a
/// repair closes it; this does not, because subtracting a float from an
/// accumulated sum does not return the exact remainder any more than
/// subtracting one accumulated sum from another does -- see
/// [`Self::committed_usd`] and [`EvaluationFold::tally`] for how that
/// float-order residue is avoided instead. Add-only is also
/// what makes [`Self::absorb`] a straight field-wise sum, which is what the
/// deployment and project views are.
///
/// **Carries no answer to "how much is open right now"**: that question
/// needs the open set, which lives beside this type rather than inside it --
/// [`EvaluationAccumulator::open`] on a raw per-principal row, `open_calls`
/// and `open_usd` on [`EvaluationView`] once `tally` has summed every
/// collected principal's. One `acknowledged_calls` formula on this type
/// takes the open count as an argument, so both callers derive the same
/// answer from the same subtraction rather than each carrying a copy that
/// could drift from the other.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct EvaluationCounters {
    /// Calls this scope committed to, by durable identity.
    pub(super) intents: u64,
    /// Every accepted result: measured, unknown-usage and refused alike.
    pub(super) all: EvaluationCallTally,
    /// Measured dollars whose settle this scope has an answer for, added
    /// exactly once each: at the record, when the settle already arrived
    /// committed, or at the repair that later closes it (see
    /// [`EvaluationFold::repaired`]). Never derived by subtracting an open
    /// amount from `measured_usd` -- that subtraction would leave a float
    /// residue behind once the open set empties, so committed dollars are
    /// added directly at each of the three points above instead.
    pub(super) committed_usd: f64,
    /// Acknowledgements that arrived as a repair rather than with the
    /// result. A subset of `acknowledged_calls`, not an addition to it.
    pub(super) repaired_calls: u64,
    /// A result delivered again for an identity already settled.
    pub(super) duplicate_results: u64,
    /// A result naming no outstanding intent of this session, or naming one it
    /// does not answer.
    pub(super) unattributed_results: u64,
    /// A repair resolving no open settlement: no accepted result, or one whose
    /// settlement was already acknowledged.
    pub(super) unmatched_repairs: u64,
    pub(super) by_model: BTreeMap<EvaluationModelKey, EvaluationCallTally>,
}

impl EvaluationCounters {
    /// Add another scope's counters into this one, field by field.
    ///
    /// The one definition of what merging means here, for
    /// [`Counters::absorb`](super::fold::Counters)'s reason: a field omitted is
    /// a figure the deployment view quietly under-reports.
    fn absorb(&mut self, other: &Self) {
        self.intents += other.intents;
        self.all.absorb(&other.all);
        self.committed_usd += other.committed_usd;
        self.repaired_calls += other.repaired_calls;
        self.duplicate_results += other.duplicate_results;
        self.unattributed_results += other.unattributed_results;
        self.unmatched_repairs += other.unmatched_repairs;
        for (key, row) in &other.by_model {
            self.by_model.entry(key.clone()).or_default().absorb(row);
        }
    }

    /// Intents with no accepted result. Their cost is unknown, never zero.
    pub(super) fn pending(&self) -> u64 {
        self.intents - self.all.calls
    }

    /// Results that reached the service with an acknowledged settle, given
    /// how many of this scope's calls are open right now.
    ///
    /// Derived rather than accumulated, so it cannot come to disagree with
    /// the open count it is computed against. A refusal has no settlement at
    /// all and is excluded here rather than counted as acknowledged.
    pub(super) fn acknowledged_calls(&self, open_calls: u64) -> u64 {
        self.all.calls - self.all.refused_calls - open_calls
    }

    /// Measured dollars whose settle this scope has an answer for.
    pub(super) fn committed_usd(&self) -> f64 {
        self.committed_usd
    }

    /// Whether some in-scope evaluation cost cannot be stated.
    ///
    /// A pending intent and an unreported usage are both "somebody billed an
    /// amount nobody here can name". A refusal is not: nothing was sent.
    pub(super) fn cost_incomplete(&self) -> bool {
        self.pending() > 0 || self.all.unknown_usage_calls > 0
    }
}

/// One principal's evaluation spend, folded directly off the log.
///
/// The counters and the open set live beside each other rather than in one
/// type: `counters` is add-only and can be merged into a wider scope by
/// straight field addition, while `open` is a set this principal's own
/// [`EvaluationFold::recorded`] and [`EvaluationFold::repaired`] insert into
/// and remove from by identity -- the two would need different merge rules
/// under one `absorb`, so they get one each.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct EvaluationAccumulator {
    pub(super) counters: EvaluationCounters,
    /// Calls whose settle is open right now, keyed by call identity and
    /// valued at the amount a repair would resolve.
    ///
    /// A `BTreeMap` so [`EvaluationFold::tally`]'s sum over it runs in key
    /// order without a separate sort.
    pub(super) open: BTreeMap<(SessionId, ResponseId), f64>,
}

impl EvaluationAccumulator {
    /// Results that reached the service and whose settle nobody has
    /// confirmed, on this principal's own row.
    pub(super) fn unconfirmed_calls(&self) -> u64 {
        self.open.len() as u64
    }

    /// The dollars behind [`Self::unconfirmed_calls`], summed once, directly,
    /// over this principal's open calls -- never by subtracting one
    /// accumulated sum from another, which would leave a float residue
    /// behind once the open set is empty instead of landing on exact `0.0`.
    pub(super) fn unconfirmed_usd(&self) -> f64 {
        self.open.values().fold(0.0, |sum, usd| sum + usd)
    }

    /// This principal's own answer to "acknowledged", from its own open set.
    ///
    /// No production caller: every real reader goes through
    /// [`EvaluationFold::tally`] and [`EvaluationView`], because a report is
    /// never scoped to "whatever this one row happens to hold" alone. Kept as
    /// a real method rather than dropped, so a test can prove the row itself
    /// -- not only the view built over it -- excludes an open call correctly.
    #[cfg(test)]
    pub(super) fn acknowledged_calls(&self) -> u64 {
        self.counters.acknowledged_calls(self.unconfirmed_calls())
    }
}

/// What [`EvaluationFold::tally`] answers for one scope: every collected
/// principal's counters, plus the open-call totals summed from each
/// collected principal's own open set.
pub(super) struct EvaluationView {
    pub(super) counters: EvaluationCounters,
    open_calls: u64,
    open_usd: f64,
}

impl EvaluationView {
    /// Results that reached the service and whose settle nobody has
    /// confirmed, across every principal this scope collects.
    pub(super) fn unconfirmed_calls(&self) -> u64 {
        self.open_calls
    }

    /// The dollars behind [`Self::unconfirmed_calls`].
    pub(super) fn unconfirmed_usd(&self) -> f64 {
        self.open_usd
    }

    /// This scope's answer to "acknowledged", from the open count `tally`
    /// summed across every collected principal.
    pub(super) fn acknowledged_calls(&self) -> u64 {
        self.counters.acknowledged_calls(self.open_calls)
    }
}

/// What a result must match before it may consume an intent.
///
/// The same three-part check the session projection makes, held here for the
/// same reason: a mismatched result must neither book a cost nor consume the
/// intent the valid answer still needs.
#[derive(Debug)]
struct Outstanding {
    source_turn_index: u64,
    source_response_id: ResponseId,
    /// The model the call was requested under, which is the only place a
    /// refusal's identity can come from: a call that reached no service
    /// reported nothing.
    requested_model: String,
}

/// How far one durable call identity has got.
#[derive(Debug)]
enum CallState {
    /// Committed to, with no accepted result yet.
    Intended(Outstanding),
    /// A result was accepted and its settle is unacknowledged. The amount a
    /// repair resolves lives on the payer's [`EvaluationAccumulator::open`]
    /// entry for this call -- one home for the amount, not a second copy
    /// here that [`EvaluationFold::repaired`] would have to keep in sync.
    Unacknowledged,
    /// Nothing further can move: the settle is acknowledged, or there was none.
    Closed,
}

/// The classifier-evaluation half of [`MetricsFold`](super::fold::MetricsFold).
///
/// Holds both the per-call join state and the per-principal counters, because
/// they are one mechanism: the join decides what may be booked, and booking it
/// anywhere else would need the same table under a second name.
#[derive(Default)]
pub(super) struct EvaluationFold {
    /// Keyed by `(session, call)` and not by the call alone.
    ///
    /// A [`ClassificationIntent::call_id`] is fresh per external attempt, so one
    /// id appearing in two sessions is two real calls — deduplicating across
    /// them would drop a second tenant's spend. Results and repairs are
    /// delivered by the session's own writer (`Engine::deliver_classifier_output`
    /// reads the runtime by session id), so the join never has to cross one.
    calls: HashMap<(SessionId, ResponseId), CallState>,
    by_principal: BTreeMap<PrincipalKey, EvaluationAccumulator>,
}

impl EvaluationFold {
    /// A call this deployment committed to.
    ///
    /// Idempotent by identity: a re-appended intent for a call already known is
    /// not a second call. The fold's sequence watermark already refuses the
    /// *same* event twice; this refuses the same *call* twice.
    pub(super) fn requested(
        &mut self,
        session: &SessionId,
        payer: &PrincipalKey,
        record: &ClassificationIntent,
    ) {
        let key = (session.clone(), record.call_id.clone());
        if self.calls.contains_key(&key) {
            return;
        }
        self.calls.insert(
            key,
            CallState::Intended(Outstanding {
                source_turn_index: record.source_turn_index,
                source_response_id: record.source_response_id.clone(),
                requested_model: record.identity.model.clone(),
            }),
        );
        self.row(payer).counters.intents += 1;
    }

    /// What a call produced, booked once or not at all.
    pub(super) fn recorded(
        &mut self,
        session: &SessionId,
        payer: &PrincipalKey,
        record: &ClassificationRecord,
    ) {
        let key = (session.clone(), record.call_id.clone());
        // The join is resolved before anything is booked, because the counters
        // are a second borrow of this struct. Taking the entry out also frees
        // the intent's strings the moment the call stops needing them.
        let outstanding = match self.calls.remove(&key) {
            Some(CallState::Intended(outstanding))
                if outstanding.source_turn_index == record.source_turn_index
                    && outstanding.source_response_id == record.source_response_id =>
            {
                outstanding
            }
            // Attribution failed. The intent is put back untouched: a
            // mismatched delivery must not stand in the way of the valid answer
            // that arrives later, including after a replay.
            Some(state @ CallState::Intended(_)) => {
                self.calls.insert(key, state);
                self.row(payer).counters.unattributed_results += 1;
                return;
            }
            // Already settled. One answer, one cost, however often it is
            // delivered.
            Some(state) => {
                self.calls.insert(key, state);
                self.row(payer).counters.duplicate_results += 1;
                return;
            }
            // No intent of this session names this call.
            None => {
                self.row(payer).counters.unattributed_results += 1;
                return;
            }
        };

        let spend = record.outcome.spend();
        let model = EvaluationModelKey {
            requested: outstanding.requested_model,
            // Empty is the same statement as absent — the service named nothing
            // usable — and collapsing them keeps a row from claiming an
            // identity of `""`. The refusal count on the row is what keeps
            // "nobody was asked" distinguishable from "nobody answered by name".
            reported: reported_model(&record.outcome)
                .filter(|name| !name.trim().is_empty())
                .map(str::to_string),
        };
        let row = self.by_principal.entry(payer.clone()).or_default();
        row.counters.all.book(spend);
        row.counters.by_model.entry(model).or_default().book(spend);
        // The amount a repair would re-drive, which is the record's own: a zero
        // on the unknown-usage arm is a *release* and not a price. Keyed into
        // `open` under the same key `calls` uses, so `repaired` can find it
        // by identity without a second lookup path.
        let state = match spend.filter(|spend| spend.settled() == SettlementAck::Unconfirmed) {
            Some(spend) => {
                let usd = spend.unconfirmed_settlement_usd().unwrap_or_default();
                row.open.insert(key.clone(), usd);
                CallState::Unacknowledged
            }
            // Already answered for at record time -- a settle that arrived
            // committed needs no later repair, so its dollars join
            // `committed_usd` right here rather than waiting on an event
            // that will never come. `Unknown` spend has no dollars to book:
            // `committed_usd` is Some(_) only off `Measured`.
            None => {
                if let Some(usd) = spend.and_then(EvaluationSpend::committed_usd) {
                    row.counters.committed_usd += usd;
                }
                CallState::Closed
            }
        };
        self.calls.insert(key, state);
    }

    /// One unconfirmed settlement, resolved.
    ///
    /// **An acknowledgement, never a cost.** `applied: false` is a success —
    /// the ledger already held the settle — so both answers close the question
    /// and neither adds a call or a dollar. A repair naming a call with no
    /// accepted result behind it books nothing at all: a settlement cannot
    /// create the spend it settles.
    pub(super) fn repaired(
        &mut self,
        session: &SessionId,
        payer: &PrincipalKey,
        record: &ClassificationSettlementRepair,
    ) {
        let key = (session.clone(), record.call_id.clone());
        if let Some(state) = self.calls.get_mut(&key)
            && matches!(state, CallState::Unacknowledged)
        {
            *state = CallState::Closed;
            let row = self.row(payer);
            row.counters.repaired_calls += 1;
            // The amount lives on this principal's `open` entry, inserted by
            // `recorded` under the same key -- removing it here is what
            // closes the call's one home for the amount, rather than a
            // second copy `repaired` would have to keep in sync by hand.
            let usd = row.open.remove(&key);
            debug_assert!(
                usd.is_some(),
                "an Unacknowledged call state has no matching entry on its \
                 payer's open set -- recorded and repaired disagree about \
                 which row this call belongs to"
            );
            row.counters.committed_usd += usd.unwrap_or_default();
            return;
        }
        self.row(payer).counters.unmatched_repairs += 1;
    }

    /// Every collected principal's counters added together, plus the
    /// settlement question `absorb` cannot answer: how much is open *right
    /// now*.
    ///
    /// Summed on the way out through the same predicate the money view uses, so
    /// a tenant's report cannot read its neighbours' evaluation spend by
    /// forgetting to narrow, and a project's total covers the members who spent
    /// rather than the members the config still lists.
    ///
    /// Each collected principal's own [`EvaluationAccumulator::open`] is the
    /// open set: this needs no resolver back to a principal and no scan of
    /// [`Self::calls`], because the amount already lives where the payer was
    /// known when it was booked.
    pub(super) fn tally(&self, scope: Scope<'_>) -> EvaluationView {
        let mut counters = EvaluationCounters::default();
        let mut open_calls = 0u64;
        // Not `+= .sum()`: `Iterator::sum`'s `f64` identity is `-0.0`, so an
        // empty open set would publish a signed zero that formats as
        // `-$0.00` on the dashboard. `EvaluationAccumulator::unconfirmed_usd`
        // already seeds each principal's own sum at `0.0`, so summing those
        // across principals here with an explicit `0.0` seed cannot
        // reintroduce the standard library's signed-zero fold seed.
        let mut open_usd = 0.0_f64;
        for (owner, row) in &self.by_principal {
            if !scope.collects(owner) {
                continue;
            }
            counters.absorb(&row.counters);
            open_calls += row.unconfirmed_calls();
            open_usd += row.unconfirmed_usd();
        }
        EvaluationView {
            counters,
            open_calls,
            open_usd,
        }
    }

    fn row(&mut self, payer: &PrincipalKey) -> &mut EvaluationAccumulator {
        self.by_principal.entry(payer.clone()).or_default()
    }

    /// A raw per-principal row, exactly as [`Self::by_principal`] holds it --
    /// never as [`Self::tally`] would answer it. Test-only: production code
    /// has exactly one way to read this fold, and this accessor exists so a
    /// test can inspect what a row nobody has tallied yet actually carries.
    #[cfg(test)]
    pub(super) fn principal_row(&self, payer: &PrincipalKey) -> Option<&EvaluationAccumulator> {
        self.by_principal.get(payer)
    }
}

/// The identity the service reported, where the outcome can carry one.
///
/// A failed call received no envelope and a refused one opened no socket, so
/// neither has anything to report — which is a different statement from a
/// service that answered and named nothing.
fn reported_model(outcome: &ClassificationOutcome) -> Option<&str> {
    match outcome {
        ClassificationOutcome::Classified { reported_model, .. }
        | ClassificationOutcome::Unusable { reported_model, .. } => reported_model.as_deref(),
        ClassificationOutcome::Failed { .. } | ClassificationOutcome::Unfunded { .. } => None,
    }
}
