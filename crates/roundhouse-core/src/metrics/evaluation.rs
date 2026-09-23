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
//! the call was requested under. After one: the settlement state and, while it
//! is open, the amount a repair would resolve. No projection, no prompt, no
//! classification and no principal — the payer is resolved from the session's
//! own `SessionCreated` at every event, so a second copy here could not drift
//! from it.

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

/// One principal's evaluation spend.
///
/// **Every accumulated field is add-only, and stays that way even for
/// settlement.** The obvious design decrements an "unconfirmed" pot when a
/// repair closes it; this does not, because subtracting a float from an
/// accumulated sum does not return the exact remainder any more than
/// subtracting one accumulated sum from another does -- see
/// [`Self::committed_usd`] and [`EvaluationFold::tally`] for where that
/// float-order defect (core-metrics-5) actually got fixed. Add-only is also
/// what makes [`Self::absorb`] a straight field-wise sum, which is what the
/// deployment and project views are.
///
/// [`Self::unconfirmed_calls`] and [`Self::unconfirmed_usd`] are the one
/// exception to "accumulated": they are not folded field by field at all.
/// [`EvaluationFold::tally`] fills them in, once, from the calls still open
/// at query time, and [`Self::absorb`] leaves them alone -- see their doc.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct EvaluationCounters {
    /// Calls this deployment committed to, by durable identity.
    pub(super) intents: u64,
    /// Every accepted result: measured, unknown-usage and refused alike.
    pub(super) all: EvaluationCallTally,
    /// Measured dollars whose settle this deployment has an answer for,
    /// added exactly once each: at the record, when the settle already
    /// arrived committed, or at the repair that later closes it (see
    /// [`EvaluationFold::repaired`]). Never derived by subtracting
    /// `unconfirmed_usd` from `measured_usd` -- that subtraction is exactly
    /// the mechanism core-metrics-5 removed from `unconfirmed_usd`, and
    /// reusing it here would put the same residue on this figure instead.
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
    /// Calls whose settle nobody has answered for, right now. Set only by
    /// [`EvaluationFold::tally`]; every other `EvaluationCounters` in this
    /// module (including the per-principal rows [`Self::absorb`] folds
    /// together) leaves this at its default. See
    /// [`Self::unconfirmed_usd`].
    pub(super) unconfirmed_calls: u64,
    /// The dollars behind [`Self::unconfirmed_calls`], summed once, directly,
    /// over the calls that are open right now -- never by subtracting one
    /// accumulated sum from another. That is what lands on exactly `0.0`
    /// once the open set is empty, rather than on the few-bits-wide residue
    /// two independently-ordered sums leave behind (core-metrics-5).
    pub(super) unconfirmed_usd: f64,
}

impl EvaluationCounters {
    /// Add another principal's row into this one, field by field.
    ///
    /// The one definition of what merging means here, for
    /// [`Counters::absorb`](super::fold::Counters)'s reason: a field omitted is
    /// a figure the deployment view quietly under-reports.
    ///
    /// Never touches `unconfirmed_calls` or `unconfirmed_usd` -- those are
    /// not per-principal accumulator state, they are [`EvaluationFold::tally`]'s
    /// own answer, filled in after every `absorb` call has already run.
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

    /// Results that reached the service and whose settle nobody has confirmed.
    pub(super) fn unconfirmed_calls(&self) -> u64 {
        self.unconfirmed_calls
    }

    /// The measured dollars behind [`Self::unconfirmed_calls`].
    pub(super) fn unconfirmed_usd(&self) -> f64 {
        self.unconfirmed_usd
    }

    /// Results that reached the service with an acknowledged settle.
    ///
    /// Derived rather than accumulated, so it cannot come to disagree with the
    /// unconfirmed count published beside it. A refusal has no settlement at
    /// all and is excluded here rather than counted as acknowledged.
    pub(super) fn acknowledged_calls(&self) -> u64 {
        self.all.calls - self.all.refused_calls - self.unconfirmed_calls()
    }

    /// Measured dollars whose settle this deployment has an answer for.
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
    /// A result was accepted and its settle is unacknowledged. Carries the
    /// amount a repair resolves — the record's own, never one derived here.
    Unacknowledged(f64),
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
    /// delivered by the session's own writer (`Engine::deliver_classifications`
    /// reads the runtime by session id), so the join never has to cross one.
    calls: HashMap<(SessionId, ResponseId), CallState>,
    by_principal: BTreeMap<PrincipalKey, EvaluationCounters>,
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
        self.row(payer).intents += 1;
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
                self.row(payer).unattributed_results += 1;
                return;
            }
            // Already settled. One answer, one cost, however often it is
            // delivered.
            Some(state) => {
                self.calls.insert(key, state);
                self.row(payer).duplicate_results += 1;
                return;
            }
            // No intent of this session names this call.
            None => {
                self.row(payer).unattributed_results += 1;
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
        let counters = self.by_principal.entry(payer.clone()).or_default();
        counters.all.book(spend);
        counters.by_model.entry(model).or_default().book(spend);
        // The amount a repair would re-drive, which is the record's own: a zero
        // on the unknown-usage arm is a *release* and not a price.
        let state = match spend.filter(|spend| spend.settled() == SettlementAck::Unconfirmed) {
            Some(spend) => {
                let usd = spend.unconfirmed_settlement_usd().unwrap_or_default();
                CallState::Unacknowledged(usd)
            }
            // Already answered for at record time -- a settle that arrived
            // committed needs no later repair, so its dollars join
            // `committed_usd` right here rather than waiting on an event
            // that will never come. `Unknown` spend has no dollars to book:
            // `committed_usd` is Some(_) only off `Measured`.
            None => {
                if let Some(usd) = spend.and_then(EvaluationSpend::committed_usd) {
                    counters.committed_usd += usd;
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
        // `f64` is `Copy`, so the amount can be read out of the borrow and
        // the state closed in the same match arm -- no `mem::replace`, no
        // re-match to prove what was already just matched.
        if let Some(state) = self.calls.get_mut(&key)
            && let CallState::Unacknowledged(usd) = *state
        {
            *state = CallState::Closed;
            let counters = self.row(payer);
            counters.repaired_calls += 1;
            counters.committed_usd += usd;
            return;
        }
        self.row(payer).unmatched_repairs += 1;
    }

    /// Every collected principal's row added together, plus the settlement
    /// question `absorb` cannot answer: how much is open *right now*.
    ///
    /// Summed on the way out through the same predicate the money view uses, so
    /// a tenant's report cannot read its neighbours' evaluation spend by
    /// forgetting to narrow, and a project's total covers the members who spent
    /// rather than the members the config still lists.
    ///
    /// `principal_of` is [`MetricsFold::principal_for`](super::fold::MetricsFold::principal_for),
    /// threaded in rather than duplicated: [`Self::calls`] is keyed by
    /// session and deliberately carries no principal of its own (see the
    /// module doc), so scoping the open set needs the one resolver that
    /// already exists for this instead of a second copy that could drift
    /// from it.
    pub(super) fn tally(
        &self,
        scope: Scope<'_>,
        principal_of: impl Fn(&SessionId) -> PrincipalKey,
    ) -> EvaluationCounters {
        let mut total = EvaluationCounters::default();
        for (owner, counters) in &self.by_principal {
            if scope.collects(owner) {
                total.absorb(counters);
            }
        }
        // The open amount is queried fresh every time, summed once, directly,
        // over the calls still `Unacknowledged` right now -- never by
        // subtracting one accumulated sum from another (see
        // `EvaluationCounters::unconfirmed_usd`'s doc). Sorted by key first
        // so the sum, and this method's answer, does not depend on a
        // `HashMap`'s iteration order.
        let mut open: Vec<(&(SessionId, ResponseId), f64)> = self
            .calls
            .iter()
            .filter_map(|(key, state)| match state {
                CallState::Unacknowledged(usd) if scope.collects(&principal_of(&key.0)) => {
                    Some((key, *usd))
                }
                _ => None,
            })
            .collect();
        open.sort_by_key(|(key, _)| *key);
        total.unconfirmed_calls = open.len() as u64;
        // Not `Iterator::sum`: its `f64` identity is `-0.0`, so an empty open
        // set would publish a signed zero that formats as `-$0.00` on the
        // dashboard -- the exact defect core-metrics-5 removed, reintroduced
        // by the standard library's own fold seed.
        total.unconfirmed_usd = open.iter().fold(0.0, |sum, (_, usd)| sum + usd);
        total
    }

    fn row(&mut self, payer: &PrincipalKey) -> &mut EvaluationCounters {
        self.by_principal.entry(payer.clone()).or_default()
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
