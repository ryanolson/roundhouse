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

mod columns;
mod cost;
mod local;

pub use columns::{
    CacheReuseEvidence, FIRST_OUTPUT_BASIS, IntervalMetric, OBSERVED_CACHE_BASIS,
    PREDICTED_CACHE_BASIS, TURN_ELAPSED_BASIS,
};
pub use cost::{
    EVALUATION_PRICE_BASIS, EvaluationMetrics, EvaluationModelMetrics, EvaluationSettlement,
    EvaluationTokens, EvaluationUnbooked, OBSERVED_COST_SCOPE,
    OBSERVED_COST_SCOPE_WITH_LOCAL_CAPACITY, ObservedCost, SERVING_PRICE_BASIS, ServingCostGaps,
};

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::event::Usage;
use crate::metrics::fold::{Counters, MetricsFold, Scope};
use crate::metrics::pricing::{Correlary, ReferenceModel, ShadowPricing};
use crate::metrics::{ModelKey, ServingMode};
use crate::routing::LocalCapacityPrice;

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
        /// Of this row's [`Coverage::estimated_calls`], how many were a
        /// seat's. See the hosted arm's own field for why this is a count
        /// beside `seat_tokens` rather than folded into it.
        seat_estimated_calls: u64,
        /// What this row's GPU time cost at the catalog's
        /// `local_capacity_price`, or `null` when the catalog sets none.
        ///
        /// **`null` and never `0.0` when unpriced**: a zero here would read as
        /// traffic that cost nothing, which is the claim ruling 6 of the
        /// 2026-09-28 addendum exists to stop. Over the whole row, a seat's
        /// share included, because the hardware is this deployment's whoever
        /// the caller's hosted alternative would have billed. Charged on
        /// uncached prompt plus output, the router's own rule; a serving plane
        /// that reports no cache reads is therefore charged its whole prompt.
        capacity_usd: Option<f64>,
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
        /// Of this row's [`Coverage::estimated_calls`], how many were a
        /// seat's — priced nowhere and exact either way, so
        /// [`ServingCostGaps`] has to subtract these back out rather than
        /// publish the coverage figure whole.
        seat_estimated_calls: u64,
    },
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
    pub first_output: Option<IntervalMetric>,
    /// Turn start to terminal, over the turns this row completed.
    ///
    /// Absent on the same rule `first_output` uses, and never added to
    /// [`Self::incomplete_turn_elapsed`]: see `TurnTimings::completed_elapsed`
    /// in the timing module for why one pot would reward failing faster.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_turn_elapsed: Option<IntervalMetric>,
    /// The same interval over the turns this row did not complete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_turn_elapsed: Option<IntervalMetric>,
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

    /// Local capacity spend, or `None` for a hosted row or an unpriced local
    /// one. See [`ModelAccounting::Local::capacity_usd`].
    pub fn capacity_usd(&self) -> Option<f64> {
        match self.accounting {
            ModelAccounting::Local { capacity_usd, .. } => capacity_usd,
            ModelAccounting::Frontier { .. } => None,
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

    /// Of this row's [`Coverage::estimated_calls`], how many were a seat's.
    pub fn seat_estimated_calls(&self) -> u64 {
        match self.accounting {
            ModelAccounting::Local {
                seat_estimated_calls,
                ..
            }
            | ModelAccounting::Frontier {
                seat_estimated_calls,
                ..
            } => seat_estimated_calls,
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
    /// Local capacity spend over the rows that are priced, or `null` when no
    /// row in this aggregate carries a capacity price — a hosted aggregate, or
    /// local traffic the catalog does not price. Never a `0.0` that reads as
    /// free.
    ///
    /// The local serving mode's row is seeded `0.0` on a priced deployment,
    /// so there `null` means unpriced and only that: with no local calls yet
    /// it agrees with `savings.local_capacity_usd`, which is `0.0` for the
    /// same reason.
    pub capacity_usd: Option<f64>,
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
        if let Some(capacity) = row.capacity_usd() {
            self.capacity_usd = Some(self.capacity_usd.unwrap_or(0.0) + capacity);
        }
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
    /// Local capacity spend: our own fleet's GPU time at the catalog's
    /// `local_capacity_price`, over every local row. `None` when the catalog
    /// sets no price, so an unpriced fleet never reads as a free one.
    ///
    /// A spend beside `frontier_spend_usd`, not part of it: that figure is
    /// money hosted providers billed, and this is a configured approximation
    /// of hardware this deployment already owns.
    pub local_capacity_usd: Option<f64>,
    /// Estimated. What local traffic would have cost on its correlary — a call
    /// that never happened, priced against a model chosen by [`pricing`] —
    /// **less what it cost in local capacity** when the catalog prices it.
    ///
    /// A *saving*, so it counts only the local turns whose hosted alternative
    /// would have been this deployment's money. A pass-through project's local
    /// turn passed over a call its caller's seat would have paid for, and there
    /// is no sense in which roundhouse saved that.
    ///
    /// **Net, and allowed to go negative.** Serving locally is a saving only by
    /// the amount the hosted call would have cost *more* than the GPU time, so
    /// the capacity cost of the same priceable turns is subtracted. It is
    /// subtracted only on rows whose correlary is priced: a row with no hosted
    /// counterpart makes no saving claim to net against, and its capacity cost
    /// is still in `local_capacity_usd`. A negative figure is the honest signal
    /// that local cost more than the hosted alternative, and is not clamped.
    pub routing_savings_usd: f64,
    /// Estimated, independently. The same quantity as `routing_savings_usd`
    /// but taken from the router's own quotes at decision time rather than
    /// from a correlary: the cheapest hosted quote less the local quote the
    /// turn was served on, which carries the capacity price the router used.
    ///
    /// Kept as a cross-check, not added into the total. Two estimates of one
    /// counterfactual built from different inputs — one from a rate card and a
    /// similarity argument, one from the live cache ledger and the catalog the
    /// router was actually choosing from — should land near each other. When
    /// they do not, one of the two models is wrong, and that disagreement is
    /// worth more than either number alone.
    ///
    /// **One disagreement is not a wrong model.** A decision quoted local at a
    /// different capacity price, or at none — a $0 quote, so its figure here
    /// stays gross — while `routing_savings_usd` nets the same turn at the
    /// current price. Over such history the two differ by the difference in
    /// capacity cost.
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
    /// Marks what the two `*_turn_elapsed` [`IntervalMetric`] columns
    /// exclude — most often a turn refused before any dispatch, which stamps
    /// `TurnStarted` and a terminal but never reaches routing — rather than
    /// folding a correction back into either mean. Counts a terminal of
    /// either outcome class, whether or not its interval was itself
    /// measurable. Scoped like every other figure here: a tenant sees its
    /// own.
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
    /// The local capacity price every local dollar here was priced at, or
    /// `null` when the catalog sets none.
    ///
    /// The price itself rather than a flag, so the document is self-describing:
    /// the dashboard shows the rate beside the spend, and a reader of this
    /// snapshot — the Relay summary among them — needs no second handle on the
    /// config it was built from. `null` means every local figure on this
    /// document that could be a dollar is unpriced — `savings.local_capacity_usd`
    /// and each local row's `capacity_usd` are `null`, and `routing_savings_usd`
    /// is the gross counterfactual — so a reader cannot take local serving as
    /// free.
    pub local_capacity_price: Option<LocalCapacityPrice>,
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
    /// What our own fleet's capacity costs, or `None` when the catalog sets no
    /// price.
    ///
    /// The same value the router quotes local turns at (`catalog_config` in
    /// `roundhouse-server` hands one figure to both), so local capacity spend
    /// and the net routing saving are priced at the rate the decision was made
    /// on. `None` publishes local cost as unpriced, never as zero.
    pub local_capacity_price: Option<LocalCapacityPrice>,
}

impl MetricsConfig {
    pub fn new(pricing: ShadowPricing) -> Self {
        Self {
            pricing,
            local_quality_priors: HashMap::new(),
            default_local_quality_prior: 0.5,
            quality_prior_citation: None,
            local_capacity_price: None,
        }
    }

    pub fn with_local_capacity_price(mut self, price: LocalCapacityPrice) -> Self {
        self.local_capacity_price = Some(price);
        self
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

        let capacity_price = config.local_capacity_price;
        // Each row with the capacity cost of the priceable local turns whose
        // saving is counted, taken off `routing_savings_usd` below. Summed
        // here rather than through `Rollup`, because it is not a figure any
        // row publishes: see `LocalRow::routing_capacity_offset_usd`.
        let (mut models, offsets): (Vec<ModelMetrics>, Vec<f64>) = rows
            .iter()
            .map(|(key, counters)| {
                let total_usage = counters.total_usage();
                let coverage = Coverage {
                    calls: counters.calls,
                    reported_calls: counters.calls.saturating_sub(counters.estimated_calls),
                    estimated_calls: counters.estimated_calls,
                    // Across both pots: coverage asks whether the *provider*
                    // counted a turn, which is a different axis from whose
                    // money paid for it. A seat turn the provider reported is
                    // reported.
                    reported_tokens: counters.reported_usage().total(),
                    estimated_tokens: counters.estimated_usage().total(),
                };
                let (accounting, offset) = match key.mode {
                    ServingMode::Frontier => (frontier_row(key, counters, config), 0.0),
                    ServingMode::Local => {
                        let row = local::local_row(
                            &key.model,
                            counters,
                            &total_usage,
                            config,
                            &frontier_shapes,
                        );
                        (row.accounting, row.routing_capacity_offset_usd)
                    }
                };
                let row = ModelMetrics {
                    provider: key.provider.clone(),
                    model: key.model.clone(),
                    calls: counters.calls,
                    tokens: TokenBreakdown::from_usage(&total_usage),
                    coverage,
                    first_output: IntervalMetric::publish(
                        &counters.timing.first_output,
                        FIRST_OUTPUT_BASIS,
                    ),
                    completed_turn_elapsed: IntervalMetric::publish(
                        &counters.timing.completed_elapsed,
                        TURN_ELAPSED_BASIS,
                    ),
                    incomplete_turn_elapsed: IntervalMetric::publish(
                        &counters.timing.incomplete_elapsed,
                        TURN_ELAPSED_BASIS,
                    ),
                    cache_reuse_evidence: CacheReuseEvidence::publish(&counters.cache_reuse),
                    accounting,
                };
                (row, offset)
            })
            .unzip();
        let routing_capacity_offset_usd: f64 = offsets.iter().sum();

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
        let serving_modes = roll_up_modes(&models, capacity_price.is_some());

        let mut totals = Rollup::default();
        for model in &models {
            totals.absorb(model);
        }

        let routing_savings_usd = totals.shadow_usd - routing_capacity_offset_usd;
        let savings = Savings {
            frontier_spend_usd: totals.billed_usd,
            frontier_spend_measured_usd: totals.billed_measured_usd,
            frontier_spend_estimated_usd: totals.billed_estimated_usd,
            cache_savings_usd: totals.cache_savings_usd,
            // `Some(0.0)` on a priced deployment with no local traffic: the
            // price exists and nothing was spent at it.
            local_capacity_usd: capacity_price.map(|_| totals.capacity_usd.unwrap_or(0.0)),
            routing_savings_usd,
            routing_savings_at_decision_usd: rows.values().map(|c| c.quoted_saving_usd).sum(),
            total_usd: totals.cache_savings_usd + routing_savings_usd,
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
            savings.local_capacity_usd,
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
            local_capacity_price: capacity_price,
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

/// Price one hosted row.
///
/// Everything here prices the row's priceable pot and never its total usage:
/// the seat's share of a row is measured and unpriceable, and the one place
/// that rule can be kept for good is the value the rate card is handed. See
/// `Counters::seat`.
fn frontier_row(key: &ModelKey, counters: &Counters, config: &MetricsConfig) -> ModelAccounting {
    // A hosted model with no rate card bills an unknown amount, and zero is
    // the wrong guess. It is reported as zero dollars against non-zero tokens,
    // which is visible on the dashboard as a row that used tokens for free —
    // the shape of a missing rate card rather than of a bargain.
    let rate = config.rate_card(&key.provider, &key.model);
    // Priced per provenance, which costs nothing extra because a pot's price
    // is linear in the axes `PooledUsage` accumulates, and is the only way the
    // two parts can be reported apart afterwards.
    //
    // Through `price_pooled` and never `price` on a summed `Usage`: each
    // call's cache-write share was decided at fold time, so this row's dollars
    // are the sum of its turns' dollars by construction. Pricing summed tokens
    // instead understated a row mixing measured and unmeasured writes, and the
    // understatement reappeared as `drift_usd` in the reconciliation view
    // (M11.0 review F2).
    let billed = Billed {
        measured: rate.map_or(0.0, |r| r.pricing.price_pooled(&counters.billed.reported)),
        estimated: rate.map_or(0.0, |r| r.pricing.price_pooled(&counters.billed.estimated)),
    };
    // Wholly measured: an unreported call carries `cached_input_tokens: 0`, so
    // it contributes nothing here rather than a guess.
    ModelAccounting::Frontier {
        priced_by_catalog: rate.is_some(),
        billed_usd: billed.total(),
        billed_measured_usd: billed.measured,
        billed_estimated_usd: billed.estimated,
        // Off the pot's tokens rather than through `price_pooled`, and it needs
        // no pooling of its own: the discount is `cached_input_tokens` times a
        // rate gap, and cache reads are an ordinary additive count with no
        // per-call branch over them.
        cache_savings_usd: rate.map_or(0.0, |r| {
            r.pricing.cache_savings(counters.billed.total().tokens())
        }),
        seat_tokens: TokenBreakdown::from_usage(counters.seat.total().tokens()),
        seat_estimated_calls: counters.seat_estimated_calls,
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

fn roll_up_modes(models: &[ModelMetrics], capacity_priced: bool) -> Vec<ServingModeMetrics> {
    // Both modes are always present, even at zero calls. A dashboard that made
    // the local row vanish when nothing had been routed locally would show its
    // most alarming state — no local serving at all — as an empty space.
    let mut modes: Vec<ServingModeMetrics> = [ServingMode::Local, ServingMode::Frontier]
        .into_iter()
        .map(|mode| ServingModeMetrics {
            mode,
            totals: Rollup {
                // See `Rollup::capacity_usd`: priced and idle is `0.0`.
                capacity_usd: (capacity_priced && mode == ServingMode::Local).then_some(0.0),
                ..Rollup::default()
            },
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
