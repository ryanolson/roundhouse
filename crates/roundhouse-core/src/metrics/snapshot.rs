// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Counters plus a rate card, out comes the report.
//!
//! Separate from [`super::fold`] on purpose: prices change, and a corrected
//! rate card has to be able to reprice history without replaying it. Every
//! dollar figure in the system is computed here, from token counts the fold
//! has already established and from configuration the fold never sees.
//!
//! The types here are the wire contract of `/v1/metrics`. Two of them are
//! tagged rather than flat — see [`ModelAccounting`] and
//! [`Correlary`](crate::metrics::Correlary) — because a row that could claim
//! to be local *and* to have been billed is a row that can lie about the one
//! number this whole feature exists to report.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::event::Usage;
use crate::metrics::ServingMode;
use crate::metrics::cache_evidence::CacheEvidence;
use crate::metrics::evaluation::EvaluationCounters;
use crate::metrics::fold::{Elapsed, MetricsFold, Scope};
use crate::metrics::pricing::{Correlary, ReferenceModel, ShadowPricing, TokenShape};

/// Token counts for one grouping, split the way a reader asks about them.
///
/// `cached_input` and `reasoning` are components of `input` and `output`, not
/// additions to them — see [`Usage`] — so `total` is `input + output` and the
/// two detail fields are already inside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBreakdown {
    pub input: u64,
    pub cached_input: u64,
    pub uncached_input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub total: u64,
}

impl TokenBreakdown {
    pub fn from_usage(usage: &Usage) -> Self {
        Self {
            input: usage.input_tokens,
            cached_input: usage.cached_input_tokens,
            uncached_input: usage.uncached_input_tokens(),
            output: usage.output_tokens,
            reasoning: usage.reasoning_tokens,
            total: usage.total(),
        }
    }

    pub fn add(&mut self, other: &TokenBreakdown) {
        self.input += other.input;
        self.cached_input += other.cached_input;
        self.uncached_input += other.uncached_input;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.total += other.total;
    }

    /// Cached share of the prompt, 0.0..=1.0.
    pub fn cache_hit_ratio(&self) -> f64 {
        if self.input == 0 {
            0.0
        } else {
            self.cached_input as f64 / self.input as f64
        }
    }
}

/// How much of a grouping's accounting came from the provider.
///
/// A deployment whose clients never ask for usage — or whose gateway strips it
/// — sees this fall below 1.0, and every figure below it becomes partly our own
/// arithmetic rather than a provider's. Surfaced rather than buried because the
/// failure it describes is silent by nature: unreported usage folds in as zero
/// tokens for zero dollars, which on a hosted model is indistinguishable from a
/// saving.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub calls: u64,
    pub reported_calls: u64,
    pub estimated_calls: u64,
    /// Billed tokens the provider counted.
    pub reported_tokens: u64,
    /// Billed tokens Roundhouse counted in its place.
    ///
    /// Present because the call-weighted ratio above is a poor proxy for it:
    /// one unreported 200k-token turn and one reported 2k-token turn is 50%
    /// coverage by calls and 1% by tokens, and it is the token figure that
    /// tracks the money.
    pub estimated_tokens: u64,
}

impl Coverage {
    pub fn reported_fraction(&self) -> f64 {
        if self.calls == 0 {
            1.0
        } else {
            self.reported_calls as f64 / self.calls as f64
        }
    }

    /// Share of billed tokens the provider counted, 0.0..=1.0.
    pub fn reported_token_fraction(&self) -> f64 {
        let total = self.reported_tokens + self.estimated_tokens;
        if total == 0 {
            1.0
        } else {
            self.reported_tokens as f64 / total as f64
        }
    }

    fn add(&mut self, other: &Coverage) {
        self.calls += other.calls;
        self.reported_calls += other.reported_calls;
        self.estimated_calls += other.estimated_calls;
        self.reported_tokens += other.reported_tokens;
        self.estimated_tokens += other.estimated_tokens;
    }
}

/// What a row's money means, which depends on where it ran.
///
/// One tagged value rather than a `mode` field beside two mutually exclusive
/// money fields that were each zero when the other applied. That shape let a
/// row claim to be local and to have been billed, and it made every consumer
/// re-derive which fields were meaningful from `mode` — the repeated
/// conditional the review objected to, which was a symptom rather than the
/// defect.
///
/// Flattened on the wire, so the serialized row still carries `mode` and its
/// money at the top level and consumers are unaffected. The difference is that
/// the fields which do not apply are now absent rather than zero, and a zero
/// that is really zero is no longer indistinguishable from one that is
/// "not applicable".
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ModelAccounting {
    /// Served by our own fleet: bills nothing, priced against a correlary.
    Local {
        /// What this traffic would have cost on its correlary. Zero when the
        /// correlary is [`Correlary::Unpriced`].
        ///
        /// Over the priceable share of the row only — see `seat_tokens` on the
        /// hosted arm. A pass-through project's local turn
        /// displaced a hosted call its caller's seat would have paid for, and
        /// crediting this deployment with having saved that money is the same
        /// invented figure as billing it for the seat's tokens.
        shadow_usd: f64,
        correlary: Correlary,
        /// Tokens this row served for a project whose money is a seat's.
        seat_tokens: TokenBreakdown,
    },
    /// Issued to an external endpoint: bills real money.
    Frontier {
        /// Whether the catalog held a rate for this row's `(provider, model)`.
        ///
        /// A configured zero rate can produce the same amount as missing
        /// pricing. Gap counts and dashboard warnings need the lookup result
        /// to distinguish them.
        priced_by_catalog: bool,
        /// The sum of the two below.
        billed_usd: f64,
        /// Priced from counts the provider reported.
        billed_measured_usd: f64,
        /// Priced from our own tokenizer, because the provider reported
        /// nothing. Not smaller or larger than the truth — unknown, since a
        /// tokenizer mismatch cuts either way.
        billed_estimated_usd: f64,
        /// The discount the provider's own cache applied. Wholly measured: an
        /// unreported call carries no cache reads to guess at.
        ///
        /// Over the priceable share only, for the reason every other dollar
        /// here is: a discount on a seat's bill was applied to a bill this
        /// deployment never saw.
        cache_savings_usd: f64,
        /// Tokens measured under a forwarded subscription seat.
        ///
        /// **A count and never a dollar**, which is the whole of the honesty
        /// rule at this surface: roundhouse holds no rate card for a seat, so
        /// the catalog's per-token price describes what *it* would have paid on
        /// its own key — a counterfactual, not a bill. They are inside this
        /// row's `tokens` and inside its `coverage`, because they were really
        /// served and really counted; they are outside every figure above.
        ///
        /// Zero on a deployment with no pass-through project, which is what
        /// keeps every pre-M7 row reading exactly as it did.
        seat_tokens: TokenBreakdown,
    },
}

/// What [`FirstOutputLatency`] measures, published beside the number.
///
/// On the wire because the figure is unreadable without it. The interval is
/// the `TurnStarted` append stamp to the first non-empty `OutputTextDelta`
/// append stamp, attributed to the target that served; it includes any routing
/// and failover in between, and excludes work before `TurnStarted` and delivery
/// after the append. It is not the provider's service latency.
pub const FIRST_OUTPUT_BASIS: &str = "turn_start_to_first_output";

/// The interval [`FIRST_OUTPUT_BASIS`] names, per target.
#[derive(Debug, Clone, Serialize)]
pub struct FirstOutputLatency {
    /// Mean milliseconds over [`Self::samples`].
    ///
    /// `None` when there are no samples, which is the whole point of the column
    /// being optional twice over: a row that measured nothing publishes no
    /// number rather than a zero that reads as instant.
    pub mean_ms: Option<f64>,
    pub samples: u64,
    /// Timings refused because the first delta's stamp preceded the start's.
    pub rejected: u64,
    pub basis: &'static str,
}

/// What the two [`TurnElapsed`] columns measure, published beside each number.
///
/// The interval is the `TurnStarted` append stamp to the `ResponseCompleted` or
/// `ResponseIncomplete` append stamp, attributed to the turn's last routed
/// target. It includes routing and any failover in between, and excludes
/// admission work before `TurnStarted` and delivery after the terminal append.
///
/// It is not provider latency, not task success, and not time to solution — it
/// is how long this deployment took to finish with a turn, one way or another.
pub const TURN_ELAPSED_BASIS: &str = "turn_start_to_terminal";

/// The interval [`TURN_ELAPSED_BASIS`] names, for one outcome class.
///
/// The basis names the *interval*, which is why both columns carry the same
/// string; the field name names the *class*. Two fields with one basis is the
/// design rather than a copy-paste — what differs between them is how the turn
/// ended, not what was measured.
#[derive(Debug, Clone, Serialize)]
pub struct TurnElapsed {
    /// Mean milliseconds over [`Self::samples`].
    ///
    /// `None` when there are no samples: a class that measured nothing
    /// publishes no number rather than a zero that reads as instant.
    pub mean_ms: Option<f64>,
    pub samples: u64,
    /// Terminals refused because their stamp preceded their turn's start.
    pub rejected: u64,
    pub basis: &'static str,
}

impl TurnElapsed {
    /// One class's column, or `None` when that class measured nothing.
    ///
    /// Published when there is either a timing or a refusal to report, which is
    /// [`FirstOutputLatency`]'s rule and holds for its reason: a class with only
    /// refusals keeps the column and loses the mean, because dropping it would
    /// hide a clock that moved behind "not measured".
    fn publish(elapsed: &Elapsed) -> Option<Self> {
        (elapsed.samples > 0 || elapsed.rejected > 0).then(|| Self {
            mean_ms: (elapsed.samples > 0)
                .then(|| elapsed.ms_total as f64 / elapsed.samples as f64),
            samples: elapsed.samples,
            rejected: elapsed.rejected,
            basis: TURN_ELAPSED_BASIS,
        })
    }
}

/// The denominator [`CacheReuseEvidence::predicted_mean_ratio`] is stated over.
///
/// The decision's own `isl_tokens` minus its `expected_prefill_tokens`, over
/// that same `isl_tokens` — our tokenizer's count of the prompt we were about
/// to send, including a toolbox the client re-declares every turn. It is the
/// router's belief at the moment it chose, read off the persisted
/// `DecisionRecord` and never recomputed from a live quote.
///
/// **The two numbers share that basis only because a tool-declaring turn cannot
/// go local**, and that is worth stating because it is contingent rather than
/// structural. A hosted quote is priced over the same request `isl_tokens` the
/// record carries, so a hosted row subtracts like with like. A local quote is
/// priced over the conversation buffer alone, which has no toolbox in it — and
/// the engine excludes every local candidate from a turn that declares tools,
/// so a local row's `isl_tokens` has no toolbox in it either. A later rung that
/// lets a tool-declaring turn reach a local worker would silently make the
/// whole toolbox render read as predicted cache reuse on that row.
pub const PREDICTED_CACHE_BASIS: &str = "routed_isl_minus_expected_prefill";

/// The denominator [`CacheReuseEvidence::observed_mean_ratio`] is stated over.
///
/// The provider's own `cached_input_tokens` over its own `input_tokens`, and
/// only where `Usage::cache_read_source` says the provider actually stated the
/// read. A stated zero counts; an omitted, null or unparseable field does not,
/// because both decoders fill it in with a zero that would otherwise divide as
/// a measured miss.
pub const OBSERVED_CACHE_BASIS: &str = "stated_cached_input_over_input";

/// What the router expected of a target's cache against what it got.
///
/// **An observation, not a verdict.** A gap in either direction says the
/// router's expectation and the provider's accounting disagree on this row. It
/// is not by itself evidence of cache pressure, of a routing mistake, or of
/// anything about answer quality, and nothing downstream may read it as a
/// reward.
///
/// The two means are over [`Self::samples`] — terminals where a usable
/// prediction met a *stated* cache read. The counts beside them say how much of
/// the row never got that far, which is what keeps a small sample from reading
/// as a whole population. A local row contributes counts and no samples by
/// construction: its cache credit is the router's own quote handed back, so
/// pairing it would check a number against itself.
#[derive(Debug, Clone, Serialize)]
pub struct CacheReuseEvidence {
    /// Mean predicted reuse over [`Self::samples`], on
    /// [`PREDICTED_CACHE_BASIS`].
    pub predicted_mean_ratio: Option<f64>,
    pub predicted_basis: &'static str,
    /// Mean observed reuse over the same samples, on [`OBSERVED_CACHE_BASIS`].
    pub observed_mean_ratio: Option<f64>,
    pub observed_basis: &'static str,
    /// Terminals where a usable prediction met a stated cache read.
    pub samples: u64,
    /// Mean of observed minus predicted. Negative where the router expected
    /// more reuse than the provider reported.
    ///
    /// Derived from the two totals rather than accumulated beside them, so it
    /// cannot come to disagree with the means published next to it.
    pub mean_signed_error: Option<f64>,
    /// Terminals whose decision carried a prediction that can be divided.
    pub predictions: u64,
    /// Terminals whose decision predicted nothing that can be divided.
    pub unusable_prediction: u64,
    /// Terminals where the provider stated its cache read, zero included.
    pub measured_cache_reads: u64,
    /// Terminals where nobody stated one — an omitted, null or unparseable
    /// field, or a locally derived credit.
    pub unverifiable_cache_read: u64,
    /// Terminals reporting more cached input than input.
    pub invalid_usage: u64,
    /// Terminals whose provider count could not be read at all.
    pub unusable_usage: u64,
}

impl CacheReuseEvidence {
    /// One row's column, or `None` when it observed no terminal at all.
    ///
    /// Published as soon as there is anything to report — including a row that
    /// paired nothing — on [`TurnElapsed::publish`]'s rule and for its reason:
    /// dropping the column would hide "nothing about this provider's cache is
    /// checkable" behind the same absence as "nothing ran here".
    fn publish(evidence: &CacheEvidence) -> Option<Self> {
        (evidence.observed_terminals() > 0).then(|| {
            let mean = |total: f64| (evidence.paired > 0).then(|| total / evidence.paired as f64);
            Self {
                predicted_mean_ratio: mean(evidence.predicted_total),
                predicted_basis: PREDICTED_CACHE_BASIS,
                observed_mean_ratio: mean(evidence.observed_total),
                observed_basis: OBSERVED_CACHE_BASIS,
                samples: evidence.paired,
                mean_signed_error: mean(evidence.observed_total - evidence.predicted_total),
                predictions: evidence.predictions,
                unusable_prediction: evidence.unusable_prediction,
                measured_cache_reads: evidence.measured_cache_reads,
                unverifiable_cache_read: evidence.unverifiable_cache_read,
                invalid_usage: evidence.invalid_usage,
                unusable_usage: evidence.unusable_usage,
            }
        })
    }
}

/// One model's row.
#[derive(Debug, Clone, Serialize)]
pub struct ModelMetrics {
    pub provider: String,
    pub model: String,
    pub calls: u64,
    pub tokens: TokenBreakdown,
    pub coverage: Coverage,
    /// Absent when this row has neither a usable timing nor a refused one, so
    /// every row written before this column existed serializes as it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_output: Option<FirstOutputLatency>,
    /// Turn start to terminal, over the turns this row completed.
    ///
    /// Absent on the same rule `first_output` uses, and never added to
    /// [`Self::incomplete_turn_elapsed`]: see `Counters::completed_elapsed`
    /// in the fold module for why one pot would reward failing faster.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_turn_elapsed: Option<TurnElapsed>,
    /// The same interval over the turns this row did not complete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_turn_elapsed: Option<TurnElapsed>,
    /// Predicted against observed cache reuse, absent on the rule above.
    ///
    /// **Never summed across serving modes**, which [`ModelKey`]'s `mode` field
    /// already holds by construction: a local row's prediction and a hosted
    /// row's are stated against prompts assembled differently, so one mean over
    /// both would be a ratio of two incomparable things.
    ///
    /// [`ModelKey`]: crate::metrics::ModelKey
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_reuse_evidence: Option<CacheReuseEvidence>,
    #[serde(flatten)]
    pub accounting: ModelAccounting,
}

impl ModelMetrics {
    pub fn mode(&self) -> ServingMode {
        match self.accounting {
            ModelAccounting::Local { .. } => ServingMode::Local,
            ModelAccounting::Frontier { .. } => ServingMode::Frontier,
        }
    }

    /// Money billed. Structurally zero for a local row.
    pub fn billed_usd(&self) -> f64 {
        match self.accounting {
            ModelAccounting::Local { .. } => 0.0,
            ModelAccounting::Frontier { billed_usd, .. } => billed_usd,
        }
    }

    pub fn billed_measured_usd(&self) -> f64 {
        match self.accounting {
            ModelAccounting::Local { .. } => 0.0,
            ModelAccounting::Frontier {
                billed_measured_usd,
                ..
            } => billed_measured_usd,
        }
    }

    pub fn billed_estimated_usd(&self) -> f64 {
        match self.accounting {
            ModelAccounting::Local { .. } => 0.0,
            ModelAccounting::Frontier {
                billed_estimated_usd,
                ..
            } => billed_estimated_usd,
        }
    }

    /// What this would have cost hosted. Structurally zero for a hosted row,
    /// which is billed rather than shadow-priced.
    pub fn shadow_usd(&self) -> f64 {
        match self.accounting {
            ModelAccounting::Local { shadow_usd, .. } => shadow_usd,
            ModelAccounting::Frontier { .. } => 0.0,
        }
    }

    pub fn cache_savings_usd(&self) -> f64 {
        match self.accounting {
            ModelAccounting::Local { .. } => 0.0,
            ModelAccounting::Frontier {
                cache_savings_usd, ..
            } => cache_savings_usd,
        }
    }

    pub fn correlary(&self) -> Option<&Correlary> {
        match &self.accounting {
            ModelAccounting::Local { correlary, .. } => Some(correlary),
            ModelAccounting::Frontier { .. } => None,
        }
    }

    /// Tokens in this row that no rate card of ours applies to.
    ///
    /// On both arms, unlike every dollar accessor above, because the question
    /// it answers is the same on both: how much of this row is a seat's? A
    /// reader comparing `tokens` with the money beside it needs one number to
    /// explain the gap, not one per serving mode.
    pub fn seat_tokens(&self) -> TokenBreakdown {
        match self.accounting {
            ModelAccounting::Local { seat_tokens, .. }
            | ModelAccounting::Frontier { seat_tokens, .. } => seat_tokens,
        }
    }
}

/// The volume and money of a set of rows.
///
/// One accumulator shared by every aggregate — per provider, per serving mode,
/// and the grand total — because they are the same arithmetic and were three
/// copies of it, plus five more single-field `sum()` passes for the headline.
/// Adding one money field used to mean touching six places, and forgetting one
/// failed silently as a zero on the dashboard, which is the failure this whole
/// feature exists not to produce.
///
/// Flattened on the wire, so every aggregate serializes exactly as it did when
/// these were loose fields.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Rollup {
    pub calls: u64,
    pub tokens: TokenBreakdown,
    /// The share of `tokens` that is a forwarded seat's and carries no price.
    ///
    /// A token field among money fields, deliberately: an aggregate whose
    /// dollars are smaller than its tokens imply is exactly the shape a reader
    /// mistakes for a bargain, and this is the number that explains it.
    pub seat_tokens: TokenBreakdown,
    pub coverage: Coverage,
    pub billed_usd: f64,
    pub billed_measured_usd: f64,
    pub billed_estimated_usd: f64,
    pub shadow_usd: f64,
    pub cache_savings_usd: f64,
}

impl Rollup {
    /// Add one row. The single definition of what aggregation means here.
    fn absorb(&mut self, row: &ModelMetrics) {
        self.calls += row.calls;
        self.tokens.add(&row.tokens);
        self.seat_tokens.add(&row.seat_tokens());
        self.coverage.add(&row.coverage);
        self.billed_usd += row.billed_usd();
        self.billed_measured_usd += row.billed_measured_usd();
        self.billed_estimated_usd += row.billed_estimated_usd();
        self.shadow_usd += row.shadow_usd();
        self.cache_savings_usd += row.cache_savings_usd();
    }
}

/// A provider's rollup across its models.
///
/// Keyed by `(mode, provider)`, so a row is always single-mode and its money
/// could in principle be tagged the way [`ModelAccounting`] is. It is not, and
/// the difference is where the data enters: a [`ModelMetrics`] is *built* from
/// a fold and carries a [`Correlary`], so an invalid combination there would be
/// a claim about the world. An aggregate is a sum over rows whose shape is
/// already enforced, computed in exactly one place and never deserialized.
/// Tagging it would buy a second pair of enums to guard a door with no entrance.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderMetrics {
    pub provider: String,
    pub mode: ServingMode,
    #[serde(flatten)]
    pub totals: Rollup,
    pub models: usize,
}

/// A serving mode's rollup. See [`ProviderMetrics`] on why the money is flat.
#[derive(Debug, Clone, Serialize)]
pub struct ServingModeMetrics {
    pub mode: ServingMode,
    #[serde(flatten)]
    pub totals: Rollup,
}

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
/// in [`Savings::provider_reported_usd`]: this is our own arithmetic over usage
/// a service reported, not a bill anybody issued.
pub const EVALUATION_PRICE_BASIS: &str = "rate_card_recorded_with_each_call";

/// Tokens one set of classifier calls billed, as the services reported them.
///
/// Separate from [`TokenBreakdown`] and never added into it. A classifier call
/// is not a turn: it has no cache accounting, no seat, and no model row, and a
/// figure that mixed the two would put tokens nobody served into the volume the
/// serving rates are computed over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct EvaluationTokens {
    pub input: u64,
    pub output: u64,
    pub total: u64,
}

/// What became of the settles behind one set of evaluation calls.
///
/// **Beside the cost and never instead of it.** A settle nobody acknowledged
/// does not erase the usage that was billed, so `unconfirmed_usd` is money this
/// deployment knows it owes and cannot yet prove it committed —
/// `committed_usd + unconfirmed_usd` is always
/// [`EvaluationMetrics::measured_usd`].
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
/// **Never merged into [`Savings`], and the separation is the point.** Every
/// figure in `Savings` is priced from the current catalog over tokens the fold
/// counted; every figure here was priced once by the call that incurred it. One
/// total over both would be a number with two price bases and no reader able to
/// say which half moved — see [`ObservedCost`], which adds them *and says so*.
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
    fn build(counters: &EvaluationCounters) -> Self {
        Self {
            intents: counters.intents,
            results: counters.results,
            pending: counters.pending(),
            measured_calls: counters.measured_calls,
            measured_usd: counters.measured_usd,
            price_basis: EVALUATION_PRICE_BASIS,
            tokens: EvaluationTokens {
                input: counters.input_tokens,
                output: counters.output_tokens,
                total: counters.input_tokens + counters.output_tokens,
            },
            unknown_usage_calls: counters.unknown_usage_calls,
            refused_calls: counters.refused_calls,
            settlement: EvaluationSettlement {
                acknowledged_calls: counters.acknowledged_calls(),
                committed_usd: counters.committed_usd(),
                unconfirmed_calls: counters.unconfirmed_calls(),
                unconfirmed_usd: counters.unconfirmed_usd(),
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
                    tokens: EvaluationTokens {
                        input: row.input_tokens,
                        output: row.output_tokens,
                        total: row.input_tokens + row.output_tokens,
                    },
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
/// [`MetricsSnapshot::seat_tokens`].
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
    /// `coverage` is handed in rather than re-summed off the rows, because the
    /// document already publishes that sum through [`Rollup::absorb`] and a
    /// second walk would be a second definition of one number — the duplication
    /// `Rollup` exists to have removed.
    ///
    /// Catalog coverage comes from each row's
    /// [`ModelAccounting::Frontier::priced_by_catalog`], because a zero amount
    /// does not establish missing pricing.
    fn of(models: &[ModelMetrics], coverage: &Coverage) -> Self {
        Self {
            estimated_calls: coverage.estimated_calls,
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
/// `serving_usd` is [`Savings::frontier_spend_usd`] — which already carries this
/// deployment's judge side calls, on the model rows that billed them — so an
/// evaluation call must never also be counted as a side call, or the judge would
/// be charged here twice. It excludes traffic on a forwarded seat, which this
/// deployment holds no rate card for.
///
/// **It is not all economic cost, and [`Self::covers`] says which one it is.**
/// A locally served turn bills nobody and costs GPU time; this document prices
/// what left the building, so the fleet's own cost is outside it. Reporting a
/// total that excluded it *silently* would put the most misleading number on the
/// most local deployment — the one whose whole argument is that it serves its
/// own traffic.
///
/// **Not a savings figure and not an invoice.** [`Savings::total_usd`] is what
/// was saved; this is what was observed to be spent, and neither is the other.
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
    fn build(serving_usd: f64, gaps: ServingCostGaps, evaluation: &EvaluationMetrics) -> Self {
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

/// The headline, decomposed by how much each part can be trusted.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Savings {
    /// Money billed by hosted providers: `measured + estimated` below.
    ///
    /// Not labelled "measured" as a whole, which it was and which was wrong.
    /// A provider that reports no usage still bills, and the tokens standing in
    /// for its silence are ours, not its.
    ///
    /// **Over the traffic roundhouse holds a rate card for, and not over a
    /// forwarded seat's.** A pass-through turn is measured in
    /// [`MetricsSnapshot::seat_tokens`] and priced nowhere: the seat is a
    /// subscription, and the catalog's per-token figure describes what this
    /// deployment would have paid on its own key. Reporting that as spend was
    /// this document's version of the bill the ledger has always refused to
    /// issue.
    pub frontier_spend_usd: f64,
    /// The part of `frontier_spend_usd` priced from provider-reported counts.
    pub frontier_spend_measured_usd: f64,
    /// The part priced from our own tokenizer, because the provider was silent.
    pub frontier_spend_estimated_usd: f64,
    /// Measured. The discount hosted caches applied to prompt tokens they had
    /// already seen.
    ///
    /// Wholly measured even when coverage is partial, and not by luck: an
    /// unreported call records `cached_input_tokens: 0` because nothing
    /// observable bears on what a remote cache did, so an estimated call
    /// contributes exactly zero here rather than a guess.
    pub cache_savings_usd: f64,
    /// Estimated. What local traffic would have cost on its correlary — a call
    /// that never happened, priced against a model chosen by [`pricing`].
    ///
    /// A *saving*, so it counts only the local turns whose hosted alternative
    /// would have been this deployment's money. A pass-through project's local
    /// turn passed over a call its caller's seat would have paid for, and there
    /// is no sense in which roundhouse saved that.
    pub routing_savings_usd: f64,
    /// Estimated, independently. The same quantity as `routing_savings_usd`
    /// but taken from the router's own quotes at decision time rather than
    /// from a correlary.
    ///
    /// Kept as a cross-check, not added into the total. Two estimates of one
    /// counterfactual built from different inputs — one from a rate card and a
    /// similarity argument, one from the live cache ledger and the catalog the
    /// router was actually choosing from — should land near each other. When
    /// they do not, one of the two models is wrong, and that disagreement is
    /// worth more than either number alone.
    pub routing_savings_at_decision_usd: f64,
    /// `cache_savings_usd + routing_savings_usd`.
    pub total_usd: f64,
    /// Neither measured by us nor estimated by us: what the providers
    /// themselves billed, summed over the calls that said so.
    ///
    /// **Published beside the figures above and added into none of them**, for
    /// the reason `seat_tokens` sits outside this struct entirely: it is a
    /// different kind of number. Everything else here is priced from this
    /// deployment's catalog, and this is the external bill the admin plane's
    /// reconciliation view checks that pricing against — folded in, the drift
    /// column would be comparing a number with itself and the one disagreement
    /// worth seeing would vanish into agreement.
    ///
    /// `None` when no call in scope reported a price, which is most
    /// deployments: a provider that reports nothing and a provider that reports
    /// zero are both `0.0`, and only the second is a figure a reader may act
    /// on.
    pub provider_reported_usd: Option<f64>,
}

/// Everything the dashboard renders, at one instant.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub generated_at_ms: u64,
    pub first_event_at_ms: Option<u64>,
    pub last_event_at_ms: Option<u64>,
    pub sessions: usize,
    /// Turns *admitted*, which is not the same as turns a client asked for: a
    /// turn abandoned mid-dispatch and retried is admitted twice and appears
    /// here twice, while `calls` counts it once because only one dispatch
    /// reached a provider. The dashboard prints both, and `turns` exceeding
    /// `calls` is the shape of a deployment that has been failing over.
    pub turns: u64,
    /// Terminals with a clock and no model row to carry it.
    ///
    /// Marks what the two [`TurnElapsed`] columns exclude — most often a turn
    /// refused before any dispatch, which stamps `TurnStarted` and a terminal
    /// but never reaches routing — rather than folding a correction back into
    /// either mean. Counts a terminal of either outcome class, whether or not
    /// its interval was itself measurable. Scoped like every other figure
    /// here: a tenant sees its own.
    pub unrouted_terminals: u64,
    /// Dispatches that reached a provider and were accounted for.
    pub calls: u64,
    pub tokens: TokenBreakdown,
    /// The share of `tokens` served under a forwarded subscription seat.
    ///
    /// **Beside `tokens` and outside `savings`, which is the point.** Every
    /// figure in [`Savings`] is dollars, and a seat has none this deployment
    /// may name; reporting these tokens as money would be the invented bill the
    /// whole accounting rule exists to refuse, and reporting them nowhere would
    /// leave a deployment unable to see the traffic it is carrying. So they are
    /// published as what they are: a count, with no price attached.
    ///
    /// Zero for every deployment with no pass-through project.
    pub seat_tokens: TokenBreakdown,
    pub savings: Savings,
    /// What this deployment spent classifying its own turns.
    ///
    /// **Outside [`Savings`] and outside every token figure above.** A
    /// classifier call bills a service this deployment chose to consult, under a
    /// price the call itself recorded; the figures above are serving traffic
    /// priced by the current catalog. They are two economies and the document
    /// keeps them apart, which is what lets [`Self::observed_cost`] add them
    /// while naming what it added.
    pub evaluation: EvaluationMetrics,
    /// Serving plus evaluation, with both price bases named.
    pub observed_cost: ObservedCost,
    pub coverage: Coverage,
    /// Share of *calls* the provider accounted for.
    pub coverage_fraction: f64,
    /// Share of billed *tokens* the provider accounted for.
    ///
    /// The one to quote next to a dollar figure: spend tracks tokens, and a
    /// deployment can have most of its calls reported and most of its tokens
    /// not, or the reverse.
    pub coverage_token_fraction: f64,
    pub models: Vec<ModelMetrics>,
    pub providers: Vec<ProviderMetrics>,
    pub serving_modes: Vec<ServingModeMetrics>,
    /// The capability band the correlary inference was gated on, echoed so a
    /// reader can see how loose the comparison was allowed to be.
    pub capability_band: f64,
    /// The attribution for imported quality priors, or `None` when this
    /// deployment's priors are its own configuration.
    ///
    /// Beside `capability_band` rather than inside [`Savings`], for two
    /// reasons: [`Savings`] is `Copy` and every field on it is dollars, and the
    /// citation qualifies the *gate* — the thing the priors are an input to —
    /// rather than any one figure. The dashboard renders it under the savings
    /// hero, which is where the derived number it attributes is published.
    pub quality_prior_citation: Option<String>,
}

/// What the snapshot needs beyond the fold: rate cards, declared correlaries,
/// and the capability priors the gate compares against.
#[derive(Debug, Clone)]
pub struct MetricsConfig {
    pub pricing: ShadowPricing,
    /// Declared capability of each local model, keyed by model name.
    pub local_quality_priors: HashMap<String, f64>,
    /// Used for a local model with no entry above.
    pub default_local_quality_prior: f64,
    /// Who to credit for the quality priors above, when they were imported from
    /// a published index rather than hand-written.
    ///
    /// **Reporting configuration, not a number**, and it is here for the same
    /// reason the rate card is: this document republishes a figure derived from
    /// those priors — the routing saving is priced through the capability gate
    /// they feed — and the index they came from requires attribution when its
    /// data is republished. `None` on every deployment whose priors are its own
    /// configuration, which is every deployment that never ran
    /// `import-benchmarks`. Set at boot by the catalog loader, which finds the
    /// provenance file beside the catalog; see `catalog_config` in
    /// `roundhouse-server` (M10 review G12).
    pub quality_prior_citation: Option<String>,
}

impl MetricsConfig {
    pub fn new(pricing: ShadowPricing) -> Self {
        Self {
            pricing,
            local_quality_priors: HashMap::new(),
            default_local_quality_prior: 0.5,
            quality_prior_citation: None,
        }
    }

    pub fn with_quality_prior_citation(mut self, citation: impl Into<String>) -> Self {
        self.quality_prior_citation = Some(citation.into());
        self
    }

    pub fn with_local_quality(mut self, model: impl Into<String>, prior: f64) -> Self {
        self.local_quality_priors.insert(model.into(), prior);
        self
    }

    pub fn with_default_local_quality(mut self, prior: f64) -> Self {
        self.default_local_quality_prior = prior;
        self
    }

    fn local_quality(&self, model: &str) -> f64 {
        self.local_quality_priors
            .get(model)
            .copied()
            .unwrap_or(self.default_local_quality_prior)
    }

    fn rate_card(&self, provider: &str, model: &str) -> Option<&ReferenceModel> {
        self.pricing
            .references()
            .iter()
            .find(|r| r.provider == provider && r.model == model)
    }
}

impl MetricsSnapshot {
    /// Apply a rate card to one scope of a fold.
    ///
    /// Separate from the fold on purpose: prices change, and a corrected rate
    /// card has to be able to reprice history without replaying it.
    ///
    /// [`Scope::Deployment`] is the document an admin reads;
    /// [`Scope::Principal`] is what a turn key gets; [`Scope::Project`] is the
    /// measured half of one project's reconciliation. Every field is scoped, not
    /// only the model rows: a document whose money is filtered but whose
    /// session count, turn count and event window still describe the deployment
    /// reads as correct and discloses the size and activity of every other
    /// tenant. Those four are the fields nobody thinks to check, which is why
    /// they arrive together with the rows in a [`ScopeView`] rather than being
    /// fetched separately here.
    ///
    /// One function rather than one per scope, because the alternative is a
    /// second copy of the pricing walk below that agrees with this one until
    /// the day it does not — and the disagreement would be between what a
    /// tenant is billed and what the deployment reports.
    pub fn build(
        fold: &MetricsFold,
        scope: Scope<'_>,
        config: &MetricsConfig,
        generated_at_ms: u64,
    ) -> Self {
        let view = fold.view(scope);
        let rows = &view.rows;
        let frontier_shapes = view.frontier_shapes();

        let mut models = Vec::with_capacity(rows.len());
        for (key, counters) in rows.iter() {
            let total_usage = counters.total_usage();
            let tokens = TokenBreakdown::from_usage(&total_usage);
            let coverage = Coverage {
                calls: counters.calls,
                reported_calls: counters.calls.saturating_sub(counters.estimated_calls),
                estimated_calls: counters.estimated_calls,
                // Across both pots: coverage asks whether the *provider*
                // counted a turn, which is a different axis from whose money
                // paid for it. A seat turn the provider reported is reported.
                reported_tokens: counters.reported_usage().total(),
                estimated_tokens: counters.estimated_usage().total(),
            };

            // Everything below prices this and never `total_usage`: the seat's
            // share of a row is measured and unpriceable, and the one place
            // that rule can be kept for good is the value the rate card is
            // handed. See `Counters::seat`.
            let priceable = counters.billed.total();
            let seat_tokens = TokenBreakdown::from_usage(counters.seat.total().tokens());

            let accounting = match key.mode {
                ServingMode::Frontier => {
                    // A hosted model with no rate card bills an unknown amount,
                    // and zero is the wrong guess. It is reported as zero
                    // dollars against non-zero tokens, which is visible on the
                    // dashboard as a row that used tokens for free — the shape
                    // of a missing rate card rather than of a bargain.
                    let rate = config.rate_card(&key.provider, &key.model);
                    // Priced per provenance, which costs nothing extra because a
                    // pot's price is linear in the axes `PooledUsage`
                    // accumulates, and is the only way the two parts can be
                    // reported apart afterwards.
                    //
                    // Through `price_pooled` and never `price` on a summed
                    // `Usage`: each call's cache-write share was decided at fold
                    // time, so this row's dollars are the sum of its turns'
                    // dollars by construction. Pricing summed tokens instead
                    // understated a row mixing measured and unmeasured writes,
                    // and the understatement reappeared as `drift_usd` in the
                    // reconciliation view (M11.0 review F2).
                    let billed = Billed {
                        measured: rate
                            .map_or(0.0, |r| r.pricing.price_pooled(&counters.billed.reported)),
                        estimated: rate
                            .map_or(0.0, |r| r.pricing.price_pooled(&counters.billed.estimated)),
                    };
                    // Wholly measured: an unreported call carries
                    // `cached_input_tokens: 0`, so it contributes nothing here
                    // rather than a guess.
                    ModelAccounting::Frontier {
                        priced_by_catalog: rate.is_some(),
                        billed_usd: billed.total(),
                        billed_measured_usd: billed.measured,
                        billed_estimated_usd: billed.estimated,
                        // Off the pot's tokens rather than through
                        // `price_pooled`, and it needs no pooling of its own:
                        // the discount is `cached_input_tokens` times a rate
                        // gap, and cache reads are an ordinary additive count
                        // with no per-call branch over them.
                        cache_savings_usd: rate
                            .map_or(0.0, |r| r.pricing.cache_savings(priceable.tokens())),
                        seat_tokens,
                    }
                }
                ServingMode::Local => {
                    // The correlary is inferred from the *whole* row's shape and
                    // priced over the priceable part of it. Two different
                    // questions: which hosted model this traffic resembles is
                    // answered by the traffic, all of it, while what it would
                    // have saved is answered only for the turns whose
                    // alternative would have been this deployment's money.
                    let shape = TokenShape::from_rollup(&total_usage, counters.calls);
                    let correlary = config.pricing.resolve(
                        &key.model,
                        config.local_quality(&key.model),
                        shape,
                        &frontier_shapes,
                        // What this row's turns said they were talking to,
                        // where they agreed. The counterfactual a client named
                        // is a better answer than one inferred from traffic
                        // shape, and a worse one than a procurement decision an
                        // operator wrote down — `resolve` holds that order.
                        counters.declared_baseline.resolved(),
                    );
                    ModelAccounting::Local {
                        // Pooled like a hosted row's, though today no local
                        // dispatch reports a cache write and every one of them
                        // takes the conservative branch. Pricing the pot keeps
                        // the counterfactual additive by construction rather
                        // than by that accident, so a serving plane that starts
                        // reporting one does not quietly move a published
                        // saving.
                        shadow_usd: correlary.shadow_cost_pooled(&priceable),
                        correlary,
                        seat_tokens,
                    }
                }
            };

            // Published when there is either a timing or a refusal to report.
            // A row with only refusals keeps the column and loses the mean:
            // dropping it would hide a skewed clock behind "not measured".
            let first_output = (counters.first_output_samples > 0
                || counters.first_output_rejected > 0)
                .then(|| FirstOutputLatency {
                    mean_ms: (counters.first_output_samples > 0).then(|| {
                        counters.first_output_ms_total as f64 / counters.first_output_samples as f64
                    }),
                    samples: counters.first_output_samples,
                    rejected: counters.first_output_rejected,
                    basis: FIRST_OUTPUT_BASIS,
                });

            models.push(ModelMetrics {
                provider: key.provider.clone(),
                model: key.model.clone(),
                calls: counters.calls,
                tokens,
                coverage,
                first_output,
                completed_turn_elapsed: TurnElapsed::publish(&counters.completed_elapsed),
                incomplete_turn_elapsed: TurnElapsed::publish(&counters.incomplete_elapsed),
                cache_reuse_evidence: CacheReuseEvidence::publish(&counters.cache_reuse),
                accounting,
            });
        }

        // Biggest spend first, then biggest shadow price: the row a reader
        // wants is almost always the expensive one, and a stable secondary key
        // keeps ordering from flickering between polls when spend ties at zero.
        models.sort_by(|a, b| {
            total_dollars(b)
                .partial_cmp(&total_dollars(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.tokens.total.cmp(&a.tokens.total))
                .then_with(|| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)))
        });

        let providers = roll_up_providers(&models);
        let serving_modes = roll_up_modes(&models);

        let mut totals = Rollup::default();
        for model in &models {
            totals.absorb(model);
        }

        let savings = Savings {
            frontier_spend_usd: totals.billed_usd,
            frontier_spend_measured_usd: totals.billed_measured_usd,
            frontier_spend_estimated_usd: totals.billed_estimated_usd,
            cache_savings_usd: totals.cache_savings_usd,
            routing_savings_usd: totals.shadow_usd,
            routing_savings_at_decision_usd: rows.values().map(|c| c.quoted_alternative_usd).sum(),
            total_usd: totals.cache_savings_usd + totals.shadow_usd,
            // Summed straight off the rows rather than through `Rollup`, which
            // is the accumulator every *catalog-priced* figure above goes
            // through. Keeping it out of that pass is the mechanical half of
            // "never merged": there is no line in `Rollup::absorb` for a
            // future edit to accidentally add it to.
            provider_reported_usd: match rows
                .values()
                .map(|c| c.provider_reported_calls)
                .sum::<u64>()
            {
                0 => None,
                _ => Some(rows.values().map(|c| c.provider_reported_usd).sum()),
            },
        };

        // Off the same scope the rows came from, so a tenant's document carries
        // its own evaluation spend and its neighbours' is unreachable from it.
        let evaluation = EvaluationMetrics::build(&fold.evaluation(scope));
        // Added here and in no other place. `frontier_spend_usd` already holds
        // every judge side call, on the model row that billed it, so an
        // evaluation call that had also been folded as a side call would charge
        // this deployment twice — which is why the classifier's own events are
        // the only input to the half beside it.
        let observed_cost = ObservedCost::build(
            savings.frontier_spend_usd,
            ServingCostGaps::of(&models, &totals.coverage),
            &evaluation,
        );

        Self {
            generated_at_ms,
            first_event_at_ms: view.totals.first_at_ms,
            last_event_at_ms: view.totals.last_at_ms,
            sessions: view.totals.sessions,
            turns: view.totals.turns,
            unrouted_terminals: view.totals.unrouted_terminals,
            calls: totals.calls,
            tokens: totals.tokens,
            seat_tokens: totals.seat_tokens,
            savings,
            evaluation,
            observed_cost,
            coverage_fraction: totals.coverage.reported_fraction(),
            coverage_token_fraction: totals.coverage.reported_token_fraction(),
            coverage: totals.coverage,
            models,
            providers,
            serving_modes,
            capability_band: config.pricing.capability_band(),
            quality_prior_citation: config.quality_prior_citation.clone(),
        }
    }
}

/// A billed figure, split by how its tokens were counted.
///
/// A two-field struct rather than a pair, because `(f64, f64)` at a call site
/// is exactly the shape that gets transposed once and never noticed.
#[derive(Debug, Clone, Copy, Default)]
struct Billed {
    measured: f64,
    estimated: f64,
}

impl Billed {
    fn total(&self) -> f64 {
        self.measured + self.estimated
    }
}

/// Ordering key for the model table: everything a row is worth, billed or not.
fn total_dollars(model: &ModelMetrics) -> f64 {
    model.billed_usd() + model.shadow_usd()
}

fn roll_up_providers(models: &[ModelMetrics]) -> Vec<ProviderMetrics> {
    let mut by_provider: BTreeMap<(ServingMode, String), ProviderMetrics> = BTreeMap::new();
    for model in models {
        let entry = by_provider
            .entry((model.mode(), model.provider.clone()))
            .or_insert_with(|| ProviderMetrics {
                provider: model.provider.clone(),
                mode: model.mode(),
                totals: Rollup::default(),
                models: 0,
            });
        entry.totals.absorb(model);
        entry.models += 1;
    }
    let mut providers: Vec<_> = by_provider.into_values().collect();
    providers.sort_by(|a, b| {
        (b.totals.billed_usd + b.totals.shadow_usd)
            .partial_cmp(&(a.totals.billed_usd + a.totals.shadow_usd))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.provider.cmp(&b.provider))
    });
    providers
}

fn roll_up_modes(models: &[ModelMetrics]) -> Vec<ServingModeMetrics> {
    // Both modes are always present, even at zero calls. A dashboard that made
    // the local row vanish when nothing had been routed locally would show its
    // most alarming state — no local serving at all — as an empty space.
    let mut modes: Vec<ServingModeMetrics> = [ServingMode::Local, ServingMode::Frontier]
        .into_iter()
        .map(|mode| ServingModeMetrics {
            mode,
            totals: Rollup::default(),
        })
        .collect();
    for model in models {
        let entry = modes
            .iter_mut()
            .find(|m| m.mode == model.mode())
            .expect("every serving mode has a row");
        entry.totals.absorb(model);
    }
    modes
}
