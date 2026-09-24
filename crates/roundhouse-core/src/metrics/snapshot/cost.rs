// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The second economy: classifier evaluation spend, and what the serving and
//! evaluation halves add up to together.
//!
//! Split out of `snapshot.rs` for [`super::columns`]'s reason: `build` touches
//! this module in two lines — [`EvaluationMetrics::build`] and
//! [`ObservedCost::build`] — and sitting those two lines beside 550 lines of
//! types they barely call into is exactly what makes a reader chasing what
//! `build` does scroll past all of them first. `metrics/evaluation.rs` holds
//! the same split on the fold side, for the same reason.

use serde::Serialize;

use crate::metrics::ServingMode;
use crate::metrics::evaluation::{EvaluationCallTally, EvaluationView};
use crate::metrics::snapshot::{Coverage, ModelAccounting, ModelMetrics};

/// What every serving dollar on this document was priced by.
///
/// The catalog as loaded *now*, applied to token counts the fold established
/// when the turn ran. Correcting a rate card reprices this history, which is the
/// whole reason the fold holds no serving dollars.
pub const SERVING_PRICE_BASIS: &str = "current_catalog_rate_card";

/// What every evaluation dollar on this document was priced by.
///
/// The rate card each classifier call recorded on its own reservation, applied
/// once, at the call. Nothing here is repriced by a later catalog edit — the
/// record *is* the pricing authority for a settled call, and a second one would
/// let the ledger and the log disagree about a finished turn with no reader able
/// to say which was right.
///
/// **Not a provider-reported dollar figure**, which is a different claim living
/// in [`Savings::provider_reported_usd`](super::Savings::provider_reported_usd):
/// this is our own arithmetic over usage a service reported, not a bill anybody
/// issued.
pub const EVALUATION_PRICE_BASIS: &str = "rate_card_recorded_with_each_call";

/// Tokens one set of classifier calls billed, as the services reported them.
///
/// Separate from [`TokenBreakdown`](super::TokenBreakdown) and never added into
/// it. A classifier call is not a turn: it has no cache accounting, no seat,
/// and no model row, and a figure that mixed the two would put tokens nobody
/// served into the volume the serving rates are computed over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct EvaluationTokens {
    pub input: u64,
    pub output: u64,
    pub total: u64,
}

impl From<&EvaluationCallTally> for EvaluationTokens {
    fn from(tally: &EvaluationCallTally) -> Self {
        Self {
            input: tally.input_tokens,
            output: tally.output_tokens,
            total: tally.input_tokens + tally.output_tokens,
        }
    }
}

/// What became of the settles behind one set of evaluation calls.
///
/// **Beside the cost and never instead of it.** A settle nobody acknowledged
/// does not erase the usage that was billed, so `unconfirmed_usd` is money this
/// deployment knows it owes and cannot yet prove it committed —
/// `committed_usd + unconfirmed_usd` agrees with
/// [`EvaluationMetrics::measured_usd`] up to the rounding of two independently
/// ordered float sums over the same addends. `unconfirmed_usd` is exactly zero
/// once nothing is open; that identity is bit-exact and pinned by
/// `evaluation_tests.rs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct EvaluationSettlement {
    /// Calls that reached a service and whose settle the ledger answered for,
    /// at the call or through a later repair.
    pub acknowledged_calls: u64,
    /// The measured share of those calls, in dollars.
    pub committed_usd: f64,
    /// Calls whose settle nobody has answered for. See
    /// [`SettlementAck::Unconfirmed`](crate::classify::SettlementAck::Unconfirmed):
    /// absence of an acknowledgement, never proof the charge is not there.
    pub unconfirmed_calls: u64,
    pub unconfirmed_usd: f64,
    /// Acknowledgements that arrived as a repair rather than with the result.
    ///
    /// A subset of `acknowledged_calls`, not an addition to it, and never a
    /// second cost: a repair re-drives the amount the record already holds.
    pub repaired_calls: u64,
}

/// Classification events this projection refused to book.
///
/// Published rather than dropped, because each one is a silent failure
/// otherwise: a redelivery loop, a worker answering about the wrong turn, or a
/// repair driving a settlement no result stands behind all look identical to a
/// quiet deployment from the outside.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct EvaluationUnbooked {
    /// A result delivered again for an identity already settled.
    pub duplicate_results: u64,
    /// A result naming no outstanding intent of its session, or naming one it
    /// does not answer.
    pub unattributed_results: u64,
    /// A repair resolving no open settlement.
    pub unmatched_repairs: u64,
}

/// One `(requested, reported)` classifier identity's calls.
///
/// Two names and not one, because they answer different questions: `model` on
/// the intent is what this deployment asked for, and `reported_model` is what
/// the service said answered. `reported_model` is `null` where nothing usable
/// was reported — an absent field, an empty one, or a call that reached no
/// service at all — and `refused_calls` is what keeps that last case from
/// reading as a service that answered anonymously.
#[derive(Debug, Clone, Serialize)]
pub struct EvaluationModelMetrics {
    pub requested_model: String,
    pub reported_model: Option<String>,
    /// Every accepted result on this identity: measured, unknown and refused.
    pub calls: u64,
    pub measured_calls: u64,
    /// On [`EVALUATION_PRICE_BASIS`], like every evaluation dollar here.
    pub measured_usd: f64,
    pub unknown_usage_calls: u64,
    pub refused_calls: u64,
    pub tokens: EvaluationTokens,
}

/// What this deployment spent classifying turns, on its own axis.
///
/// **Never merged into [`Savings`](super::Savings), and the separation is the
/// point.** Every figure in `Savings` is priced from the current catalog over
/// tokens the fold counted; every figure here was priced once by the call that
/// incurred it. One total over both would be a number with two price bases and
/// no reader able to say which half moved — see [`ObservedCost`], which adds
/// them *and says so*.
///
/// The four call classes are a partition of what the log knows:
/// `results == measured_calls + unknown_usage_calls + refused_calls`, and
/// `intents == results + pending`. A pending intent and an unreported usage are
/// both cost this deployment cannot state, which is what `cost_incomplete`
/// says; a refusal is genuinely free, because nothing was sent.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EvaluationMetrics {
    /// Calls committed to, by durable call identity within a session.
    pub intents: u64,
    /// Intents whose result landed and answered them.
    pub results: u64,
    /// Intents with no accepted result. Their cost is unknown, not zero.
    pub pending: u64,
    pub measured_calls: u64,
    /// Summed over `measured_calls`, each at the rate card it recorded.
    ///
    /// **An observation, not an invoice.** Usage a service reported, priced by
    /// this deployment; no provider stated this figure.
    pub measured_usd: f64,
    pub price_basis: &'static str,
    pub tokens: EvaluationTokens,
    /// Results whose usage nobody reported. The call billed an amount that
    /// cannot be named, and zero is the wrong guess.
    pub unknown_usage_calls: u64,
    /// Results established before any HTTP — a refused budget, an unreachable
    /// ledger, an expired call. Nothing was sent, so nothing was billed.
    pub refused_calls: u64,
    pub settlement: EvaluationSettlement,
    /// Whether some in-scope evaluation cost cannot be stated, which makes
    /// `measured_usd` a floor rather than a total.
    pub cost_incomplete: bool,
    pub unbooked: EvaluationUnbooked,
    /// One row per identity pair, ordered by requested then reported name.
    pub models: Vec<EvaluationModelMetrics>,
}

impl EvaluationMetrics {
    pub(super) fn build(view: &EvaluationView) -> Self {
        let counters = &view.counters;
        Self {
            intents: counters.intents,
            results: counters.all.calls,
            pending: counters.pending(),
            measured_calls: counters.all.measured_calls,
            measured_usd: counters.all.measured_usd,
            price_basis: EVALUATION_PRICE_BASIS,
            tokens: EvaluationTokens::from(&counters.all),
            unknown_usage_calls: counters.all.unknown_usage_calls,
            refused_calls: counters.all.refused_calls,
            settlement: EvaluationSettlement {
                acknowledged_calls: view.acknowledged_calls(),
                committed_usd: counters.committed_usd(),
                unconfirmed_calls: view.unconfirmed_calls(),
                unconfirmed_usd: view.unconfirmed_usd(),
                repaired_calls: counters.repaired_calls,
            },
            cost_incomplete: counters.cost_incomplete(),
            unbooked: EvaluationUnbooked {
                duplicate_results: counters.duplicate_results,
                unattributed_results: counters.unattributed_results,
                unmatched_repairs: counters.unmatched_repairs,
            },
            models: counters
                .by_model
                .iter()
                .map(|(key, row)| EvaluationModelMetrics {
                    requested_model: key.requested.clone(),
                    reported_model: key.reported.clone(),
                    calls: row.calls,
                    measured_calls: row.measured_calls,
                    measured_usd: row.measured_usd,
                    unknown_usage_calls: row.unknown_usage_calls,
                    refused_calls: row.refused_calls,
                    tokens: EvaluationTokens::from(row),
                })
                .collect(),
        }
    }
}

/// Why the serving half of a combined total is not the whole of what serving
/// cost.
///
/// **Three different facts, counted apart, because the reader does different
/// things about them.** An estimated call is *priced* and uncertain — the
/// dollars are there, computed over token counts a silent provider left us to
/// make — and the error cuts either way, because a tokenizer mismatch is not a
/// bias. An unpriced model is *not priced at all*: real tokens billed by a real
/// provider, published as zero dollars because the catalog holds no rate for
/// that row, so the total understates by an amount nobody can name, and the
/// remedy is a rate card. A local call is *not an invoice at all*: our own fleet
/// bills nobody, and what it cost is GPU time, which a catalog of per-token
/// prices has no basis to put a number on and this projection will not invent
/// one for.
///
/// The three overlap and are deliberately not additive — a local call whose
/// usage nobody reported is in two of them — because each answers a different
/// question about the same traffic.
///
/// **A forwarded seat is not one of these, and the contrast with the local
/// count is the reason rather than a placement preference.** Both are traffic
/// this deployment served and priced nowhere, so the two look alike. They are
/// not: GPU time is *this deployment's* cost, paid in hardware instead of in
/// invoices, so a total that omits it is short — while a seat's tokens were
/// charged to the caller's own subscription, so they are not this deployment's
/// cost at all and a total that omits them is exact. A gap counter for a seat
/// would report an incomplete total that is in fact complete.
///
/// The exclusion is still stated, on [`OBSERVED_COST_SCOPE`], because it is a
/// statement about what the total is *of*. The volume is published as
/// [`MetricsSnapshot::seat_tokens`](super::MetricsSnapshot::seat_tokens).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ServingCostGaps {
    /// Calls no provider accounted for, priced from our own tokenizer.
    pub estimated_calls: u64,
    /// Hosted rows that served priceable tokens and that the catalog holds no
    /// rate for.
    ///
    /// Configured zero rates, including zero cache-read rates, are priced.
    pub unpriced_models: u64,
    /// Calls our own fleet served, whose hardware cost this document does not
    /// price.
    ///
    /// **Not a defect and not a missing rate card**, which is why it is counted
    /// rather than warned about: a local row correctly bills nothing, and the
    /// GPU time behind it is a capital cost no per-token catalog can state. It
    /// is here because without it a deployment that routed everything to its own
    /// workers would publish a near-zero total and call it complete — the one
    /// deployment this whole projection exists to describe, reporting its
    /// strongest claim as a fact about money it never spent.
    pub local_calls: u64,
}

impl ServingCostGaps {
    /// Whether anything about the serving half is unmeasured, unpriced, or
    /// priced in hardware this document cannot value.
    pub fn any(&self) -> bool {
        self.estimated_calls > 0 || self.unpriced_models > 0 || self.local_calls > 0
    }

    /// The gaps in one scope's already-priced rows.
    ///
    /// `coverage.estimated_calls` is handed in rather than re-summed off the
    /// rows — the document already publishes that sum through
    /// [`Rollup::absorb`](super::Rollup::absorb) — but it counts both pots
    /// [`Counters::estimated_calls`](crate::metrics::fold::Counters) always
    /// has, and a seat's tokens are priced nowhere and exact either way, so a
    /// seat call the provider never reported is not a gap in *this*
    /// deployment's total. [`ModelMetrics::seat_estimated_calls`] is what each
    /// row knows about its own share of that, summed back out below.
    ///
    /// Catalog coverage comes from each row's
    /// [`ModelAccounting::Frontier::priced_by_catalog`], because a zero amount
    /// does not establish missing pricing.
    pub(super) fn of(models: &[ModelMetrics], coverage: &Coverage) -> Self {
        Self {
            estimated_calls: coverage.estimated_calls.saturating_sub(
                models
                    .iter()
                    .map(ModelMetrics::seat_estimated_calls)
                    .sum::<u64>(),
            ),
            unpriced_models: models
                .iter()
                .filter(|row| {
                    matches!(
                        row.accounting,
                        ModelAccounting::Frontier {
                            priced_by_catalog: false,
                            ..
                        }
                    )
                })
                .filter(|row| row.tokens.total > row.seat_tokens().total)
                .count() as u64,
            // Calls rather than rows, unlike the count above: "which rate card
            // is missing" is a question about a model, and "how much of this
            // deployment's work is not in the total" is a question about
            // traffic.
            local_calls: models
                .iter()
                .filter(|row| row.mode() == ServingMode::Local)
                .map(|row| row.calls)
                .sum(),
        }
    }
}

/// What [`ObservedCost::total_usd`] is a total *of*.
///
/// On the wire beside the number, because the number cannot say it: hosted
/// serving this deployment paid for, plus the classifier calls it made. Two
/// kinds of traffic are counted everywhere else on this document and priced
/// nowhere, so the total passes over both — a turn our own fleet answered, whose
/// cost is GPU time, and a turn on a forwarded subscription seat, whose cost was
/// somebody else's. Neither has a per-token price this projection could state
/// without inventing one. Only the first makes the total *incomplete*; see
/// [`ServingCostGaps`].
pub const OBSERVED_COST_SCOPE: &str = "hosted_serving_and_classifier_calls";

/// Serving and evaluation added up, with both price bases named.
///
/// **The one field on this document that mixes two pricing authorities, and it
/// says so in its own payload.** A reader wants to know what the deployment
/// spent; the honest answer has a catalog-priced half and a log-priced half, and
/// publishing the sum without the two labels beside it would make a corrected
/// rate card look like a changed bill.
///
/// `serving_usd` is [`Savings::frontier_spend_usd`](super::Savings::frontier_spend_usd)
/// — which already carries this deployment's judge side calls, on the model
/// rows that billed them — so an evaluation call must never also be counted as
/// a side call, or the judge would be charged here twice. It excludes traffic
/// on a forwarded seat, which this deployment holds no rate card for.
///
/// **It is not all economic cost, and [`Self::covers`] says which one it is.**
/// A locally served turn bills nobody and costs GPU time; this document prices
/// what left the building, so the fleet's own cost is outside it. Reporting a
/// total that excluded it *silently* would put the most misleading number on the
/// most local deployment — the one whose whole argument is that it serves its
/// own traffic.
///
/// **Not a savings figure and not an invoice.**
/// [`Savings::total_usd`](super::Savings::total_usd) is what was saved; this is
/// what was observed to be spent, and neither is the other.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ObservedCost {
    pub serving_usd: f64,
    pub serving_basis: &'static str,
    pub evaluation_usd: f64,
    pub evaluation_basis: &'static str,
    /// `serving_usd + evaluation_usd`.
    pub total_usd: f64,
    /// What this total is a total of. See [`OBSERVED_COST_SCOPE`].
    pub covers: &'static str,
    /// What the serving half does not know. See [`ServingCostGaps`].
    pub serving_gaps: ServingCostGaps,
    /// Whether the evaluation half could not state some of its cost — a pending
    /// intent, or usage nobody reported. The same value as
    /// [`EvaluationMetrics::cost_incomplete`], read from it rather than
    /// recomputed.
    pub evaluation_incomplete: bool,
    /// Whether *either* half is incomplete, so `total_usd` is not a complete
    /// statement of what was spent.
    ///
    /// **Not the classifier's answer on its own**, which is the trap: a
    /// deployment whose every classifier call is measured and settled can still
    /// be serving traffic through a model the catalog has no rate for, or on its
    /// own GPUs, and a combined figure that called itself complete on the
    /// strength of the evaluation half would be at its most confident exactly
    /// where the serving side is least knowable.
    ///
    /// `serving_gaps` and `evaluation_incomplete` say which half, so a reader
    /// who has to act on it knows whether the answer is a rate card to write or
    /// a classifier to go and look at.
    pub incomplete: bool,
}

impl ObservedCost {
    pub(super) fn build(
        serving_usd: f64,
        gaps: ServingCostGaps,
        evaluation: &EvaluationMetrics,
    ) -> Self {
        Self {
            serving_usd,
            serving_basis: SERVING_PRICE_BASIS,
            evaluation_usd: evaluation.measured_usd,
            evaluation_basis: EVALUATION_PRICE_BASIS,
            total_usd: serving_usd + evaluation.measured_usd,
            covers: OBSERVED_COST_SCOPE,
            serving_gaps: gaps,
            evaluation_incomplete: evaluation.cost_incomplete,
            incomplete: gaps.any() || evaluation.cost_incomplete,
        }
    }
}
