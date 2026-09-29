// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The savings story, in Relay's close-time accounting shape.
//!
//! `LlmOptimizationSummary` is per *call* in Relay's model — it hangs off one
//! normalized LLM response — so the natural emitter here is per turn. What a
//! deployment saved in aggregate is `/v1/metrics`, and it stays there: this
//! surface answers "what did *this* turn's routing decision do", which is the
//! question a Relay consumer aggregating across producers is asking.
//!
//! # Every dollar comes from core
//!
//! Nothing here computes money. The correlary is
//! [`Correlary`](roundhouse_core::metrics::Correlary), resolved through
//! [`MetricsSnapshot`](roundhouse_core::metrics::MetricsSnapshot) exactly as the
//! dashboard resolves it; the counterfactual is `Correlary::shadow_cost_usd`;
//! the hosted price and the cache discount are `ProviderPricing`'s own methods
//! against the rate card *the decision recorded*, not against whatever catalog
//! this process booted with. A second pricing walk here would be a second answer
//! to what a turn cost, and the day a rate card was corrected the two would
//! disagree about the same turn.
//!
//! **Local capacity is the one price read from the booted catalog**, because
//! that is where the dashboard reads it. When the catalog sets a
//! `local_capacity_price`, a local turn's `actual_cost` is
//! `LocalCapacityPrice::price` over its usage, published under
//! `pricing_provider: "roundhouse_local_capacity_price"`, and its
//! `estimated_cost_saved` is the counterfactual less that cost — the
//! dashboard's `local_capacity_usd` and net `routing_savings_usd` for the one
//! turn, unclamped like them. The price arrives on the same [`MetricsConfig`]
//! the binary hands `/v1/metrics`. Without a price, a local turn publishes no
//! `actual_cost` at all, the way the dashboard publishes `null` for unpriced
//! capacity: a zero here would read as free hardware, which is the one claim
//! the owner's rule forbids. Its saving is still the whole counterfactual,
//! and the capability-gate limitation already marks the summary partial. The payload's
//! `routing_savings_at_decision_usd` is the router's hosted quote less its own
//! local quote, the dashboard's figure of that name.
//!
//! **The capability gate's outcome is carried, never recomputed.** Which hosted
//! model a local one may be priced against was decided in
//! `metrics::pricing`; this module publishes the band it used and the basis it
//! reached, and has no opinion of its own.
//!
//! # `status` is derived, and most turns are `Partial`
//!
//! Relay's own producer writes `Complete` if and only if `limitations` is empty,
//! and a summary claiming `Complete` while listing a limitation would be
//! incoherent, so this one does the same. Two consequences the ruling states
//! rather than discovers:
//!
//! - a turn whose correlary is [`Correlary::Unpriced`] publishes as `Partial`,
//!   carrying `roundhouse_correlary_unpriced:<reason>`;
//! - **every locally-served turn publishes as `Partial`**, because every one of
//!   them carries `roundhouse_capability_gate:<band>`. That is deliberate: a
//!   routing saving is a counterfactual gated on configured quality priors, and
//!   the round-2 ruling asks that our number never sit indistinguishable beside
//!   ungated ones. `Complete` is reachable — a hosted turn, on this
//!   deployment's own key, whose usage the provider reported, against a recorded
//!   rate card, is completely accounted for — and it is the only shape that is.
//!
//! # Seat tokens are never priced into any field
//!
//! A turn a pass-through project forwarded is measured under somebody's
//! subscription. Roundhouse holds no rate card for a seat, so the catalog's
//! per-token figure would describe what *this deployment* would have paid on its
//! own key — a counterfactual, not a bill. Such a turn therefore publishes no
//! `baseline_cost`, no `actual_cost` and no `estimated_cost_saved` at all, and
//! its tokens ride [`RoutingEvidence::seat_tokens`] as a bare count. The ledger
//! has refused to draw against a seat since budgets existed; this is the same
//! refusal at the one surface that would otherwise invent the bill.
//!
//! # What the correlary is resolved against
//!
//! [`Baselines::for_session`] folds *this session's* events, so the document is
//! a pure function of the log the caller handed it and two nodes replaying one
//! session agree. The cost is stated rather than hidden: inference needs an
//! observed traffic shape for the hosted candidate, and a session that never
//! called a hosted model has none — so its local turns come back
//! `Unpriced { reason: "no capability-comparable hosted model has been called" }`
//! and publish as `Partial` with no baseline cost. A **declared** correlary is
//! unaffected, which is the same conclusion `metrics::pricing` reaches from the
//! other direction: where a real evaluation exists, declare it.

use std::collections::BTreeMap;

use nemo_relay_types::codec::optimization::{
    LlmOptimizationContribution, LlmOptimizationEvidenceQuality, LlmOptimizationKind,
    LlmOptimizationModel, LlmOptimizationModelTransition, LlmOptimizationPayload,
    LlmOptimizationSummary, LlmOptimizationSummaryStatus, LlmOptimizationTokenImpact,
    LlmOptimizationTokens,
};
use nemo_relay_types::codec::response::{CostEstimate, CostSource, Usage as RelayUsage};
use roundhouse_core::event::{Accounting, SessionEvent, Usage};
use roundhouse_core::metrics::{
    Correlary, MetricsConfig, MetricsFold, MetricsSnapshot, ModelKey, PricedBasis, Scope,
    TokenBreakdown,
};
use roundhouse_core::routing::{LocalCapacityPrice, ProviderPricing, Target};
use serde::Serialize;

use crate::PRODUCER;
use crate::replay::{SessionReplay, TurnRecord};

/// The currency every figure in this module is denominated in.
///
/// A constant rather than a parameter: `ROUNDHOUSE_CATALOG` is a USD rate card
/// and the spend ledger is a USD ledger, so a configurable currency here would
/// be a label over unconverted numbers.
const CURRENCY: &str = "USD";

/// The `pricing_provider` a local turn's capacity cost is published under.
///
/// Not the serving provider's name (`dynamo`): nobody quoted this price. It is
/// the deployment's own `local_capacity_price` from its catalog, and a
/// consumer reading `pricing_provider: "dynamo"` beside `ModelPricing` would
/// take it for a vendor's rate card. Named for the catalog field so an operator
/// can find the number it came from.
const LOCAL_CAPACITY_PRICING_PROVIDER: &str = "roundhouse_local_capacity_price";

/// What the deployment's pricing configuration says about one turn's
/// counterfactual.
///
/// Two fields rather than one because they answer different questions and come
/// from different places: the correlary is *this model's* stand-in, and the band
/// is the gate's setting for the whole deployment. Carrying the band beside the
/// correlary is what lets a summary say how loose the comparison was allowed to
/// be even when the gate refused.
#[derive(Debug, Clone, Copy)]
pub struct Baseline<'a> {
    /// `None` for a hosted turn: there is no counterfactual to a call that
    /// actually happened.
    pub correlary: Option<&'a Correlary>,
    pub capability_band: f64,
    /// What a local turn's capacity costs, or `None` when the catalog sets no
    /// price. The same value the dashboard prices local capacity at.
    pub local_capacity_price: Option<LocalCapacityPrice>,
}

/// Every local model's correlary, resolved once for a session.
///
/// Built through core's own snapshot rather than by calling
/// `ShadowPricing::resolve` directly, and the difference matters: `resolve`
/// needs the observed traffic shape of every hosted candidate, which is a fold
/// over the log — so a second call site assembling that argument by hand would
/// be a second, quietly different answer to which model this one stands in for.
#[derive(Debug, Clone)]
pub struct Baselines {
    by_local_model: BTreeMap<String, Correlary>,
    capability_band: f64,
    local_capacity_price: Option<LocalCapacityPrice>,
}

impl Baselines {
    /// Resolve correlaries from a session's own events.
    pub fn for_session(events: &[SessionEvent], config: &MetricsConfig) -> Self {
        let mut fold = MetricsFold::new();
        fold.extend(events);
        // `generated_at_ms` is discarded: only the rows are read. It is the one
        // field of a snapshot that would make this function impure, so it is
        // passed as a constant rather than as a clock.
        Self::from_snapshot(
            &MetricsSnapshot::build(&fold, Scope::Deployment, config, 0),
            config,
        )
    }

    /// Read the correlaries out of a snapshot somebody else already built.
    ///
    /// `config` is the one the snapshot was built from. The snapshot publishes
    /// only whether local capacity is priced, not the price, so the price is
    /// read from the config; defaulting it to `None` would publish a priced
    /// deployment's local turns as free.
    pub fn from_snapshot(snapshot: &MetricsSnapshot, config: &MetricsConfig) -> Self {
        let mut by_local_model = BTreeMap::new();
        for row in &snapshot.models {
            if let Some(correlary) = row.correlary() {
                by_local_model.insert(row.model.clone(), correlary.clone());
            }
        }
        Self {
            by_local_model,
            capability_band: snapshot.capability_band,
            local_capacity_price: config.local_capacity_price,
        }
    }

    /// The baseline for one turn's target.
    pub fn of(&self, target: &Target) -> Baseline<'_> {
        Baseline {
            correlary: target
                .is_local()
                .then(|| self.by_local_model.get(target.model()))
                .flatten(),
            capability_band: self.capability_band,
            local_capacity_price: self.local_capacity_price,
        }
    }
}

/// Roundhouse-specific evidence that `LlmOptimizationSummary` has no field for.
///
/// The sanctioned extension point: Relay's `LlmOptimizationPayload` puts a typed,
/// schema-tagged object on a contribution precisely so a producer can carry what
/// the shared shape cannot, and it is in the types crate rather than in Relay
/// core — so none of this requires the heavy dependency.
///
/// Every field here is one the field map found no home for. In particular the
/// **measured/estimated split**: `CostSource` is per-`CostEstimate`, so one
/// summary cannot say "60% of this was priced from counts the provider
/// reported", and that distinction is the whole of what separates our spend
/// figure from a guess.
#[derive(Debug, Clone, Serialize)]
pub struct RoutingEvidence {
    /// How far apart two models' quality priors may be and still be compared.
    pub capability_band: f64,
    /// How the correlary was arrived at — a procurement decision or a similarity
    /// argument, which are not the same claim and should not be quoted the same
    /// way. `None` on a hosted turn or where the gate refused.
    pub correlary_basis: Option<PricedBasis>,
    /// The part of this turn's spend priced from counts the provider reported.
    pub billed_measured_usd: f64,
    /// The part priced from our own tokenizer, because the provider was silent.
    /// Not smaller or larger than the truth — unknown, since a tokenizer
    /// mismatch cuts either way.
    pub billed_estimated_usd: f64,
    /// What choosing local saved by the router's own quotes at the moment it
    /// chose: the cheapest hosted quote less the local quote the turn was
    /// served on ([`DecisionRecord::quoted_routing_saving_usd`]), the figure
    /// the dashboard sums under the same name. An independent estimate of the
    /// same saving `estimated_cost_saved` carries, deliberately not added to
    /// it. Two estimates built from different inputs should land near each
    /// other; when they do not, one of the two models is wrong.
    ///
    /// [`DecisionRecord::quoted_routing_saving_usd`]: roundhouse_core::routing::DecisionRecord::quoted_routing_saving_usd
    pub routing_savings_at_decision_usd: Option<f64>,
    /// Thinking tokens, which are a component of `completion_tokens` rather than
    /// an addition to them and have no Relay field of their own.
    pub reasoning_tokens: u64,
    /// Tokens served under a forwarded subscription seat.
    ///
    /// **A count and never a dollar.** Present only on a turn that forwarded
    /// one, and priced into no field of the summary above, because roundhouse
    /// holds no rate card for a seat and the catalog's per-token figure would
    /// describe what this deployment would have paid on its own key.
    pub seat_tokens: Option<TokenBreakdown>,
    /// Where this turn sits in its session's log, so a consumer holding both
    /// documents can join them.
    pub session_seq: u64,
    pub turn_id: String,
    pub response_id: String,
}

impl LlmOptimizationPayload for RoutingEvidence {
    const SCHEMA_NAME: &'static str = "roundhouse/routing";
    /// `2` since the local capacity price: `routing_savings_at_decision_usd`
    /// is the hosted quote less the router's own quote (net), where `1`
    /// published the hosted quote alone (gross). Relay's consumers keep
    /// unknown keys, so only a change of meaning bumps this.
    const SCHEMA_VERSION: &'static str = "2";
}

/// Every turn's summary, for one session.
pub fn for_session(events: &[SessionEvent], config: &MetricsConfig) -> Vec<LlmOptimizationSummary> {
    let replay = SessionReplay::of(events);
    let baselines = Baselines::for_session(events, config);
    from_replay(&replay, &baselines)
}

/// The same, from a replay and baselines a caller already has.
pub fn from_replay(replay: &SessionReplay, baselines: &Baselines) -> Vec<LlmOptimizationSummary> {
    replay
        .turns
        .iter()
        .filter_map(|turn| {
            let decision = turn.decision()?;
            for_decision(turn, &baselines.of(&decision.chosen))
        })
        .collect()
}

/// One turn's close-time accounting.
///
/// `None` for a turn there is nothing to account for — never routed, or routed
/// and refused before its prompt reached a provider. A summary for one of those
/// would publish a zero-dollar saving on a call that never happened, which is
/// the shape a reader mistakes for a bargain.
pub fn for_decision(turn: &TurnRecord, baseline: &Baseline<'_>) -> Option<LlmOptimizationSummary> {
    if !turn.is_publishable() {
        return None;
    }
    let decision = turn.decision()?;
    let usage = &turn.usage;
    let tokens = TokenBreakdown::from_usage(usage);
    let key = ModelKey::from_target(&decision.chosen);
    let local = decision.chosen.is_local();
    // Whether roundhouse may put a price on this dispatch at all. Read off the
    // decision, which is where it was decided, rather than asked of a live
    // admission an operator may have edited since.
    let billed = decision.billing.is_billable();
    let card = decision.rate_card;

    let limitations = limitations(turn, baseline);
    let priced_reference = baseline.correlary.and_then(Correlary::reference);
    let capacity_cost = capacity_cost(local, billed, baseline.local_capacity_price, usage);
    let actual_cost = actual_cost(local, billed, card, capacity_cost, usage);
    let shadow_cost = shadow_cost(local, billed, baseline.correlary, usage);

    // A hosted turn has no counterfactual model, so the only saving that exists
    // on it is the discount the provider's own cache applied — measured, from
    // the cache-read tokens it reported and the gap between its two published
    // rates. A local turn's saving is the counterfactual less what the turn
    // cost our own fleet at the catalog's capacity price, the dashboard's
    // `routing_savings_usd` for this one turn. Not clamped at zero, the same as
    // the dashboard: a local turn dearer than its stand-in is a loss, and
    // publishing it as nothing saved would make the route look cheaper than it
    // is. With no stand-in there is no saving to net, so none is published.
    let saved = match local {
        true => shadow_cost.map(|shadow| match capacity_cost {
            Some(capacity) => shadow - capacity,
            None => shadow,
        }),
        false => card
            .filter(|_| billed)
            .map(|card| card.cache_savings(usage)),
    };

    Some(LlmOptimizationSummary {
        schema_version: "1".to_string(),
        // `2` since the local capacity price: a priced local turn's saving is
        // net of its capacity cost, and an unpriced one publishes no
        // `actual_cost`. The document's shape did not change, so
        // `schema_version` stays `1`.
        calculation_version: "2".to_string(),
        // Derived, never chosen: see the module documentation.
        status: status(&limitations),
        limitations,
        baseline_model: priced_reference.map(|reference| LlmOptimizationModel {
            model: reference.model.clone(),
            provider: Some(reference.provider.clone()),
        }),
        effective_model: Some(LlmOptimizationModel {
            model: key.model.clone(),
            provider: Some(key.provider.clone()),
        }),
        effective_usage: Some(relay_usage(&tokens, usage)),
        // The counterfactual is deliberately like-for-like — the *same* token
        // counts including the same cached fraction, at the reference model's
        // rates. It is not "what if we had sent this cold", which would assume
        // the hosted provider's cache never warmed and would roughly double the
        // figure on a long session. See `Correlary::shadow_cost_usd`.
        baseline_usage: shadow_cost.map(|_| relay_usage(&tokens, usage)),
        tokens_saved: tokens_saved(local, billed, usage),
        baseline_cost: shadow_cost.map(|total| CostEstimate {
            total: Some(total),
            currency: CURRENCY.to_string(),
            input: None,
            output: None,
            cache_read: None,
            cache_write: None,
            // Our own arithmetic against a rate card, which is exactly what
            // `ModelPricing` means. `ProviderReported` would claim a provider
            // had quoted us for a call nobody made.
            source: CostSource::ModelPricing,
            pricing_provider: priced_reference.map(|reference| reference.provider.clone()),
            pricing_model: priced_reference.map(|reference| reference.model.clone()),
            // The catalog records neither yet — S1's provenance item. Absent
            // rather than invented: an undated price is a price, and a wrongly
            // dated one is a claim.
            pricing_as_of: None,
            pricing_source: None,
        }),
        actual_cost: actual_cost.map(|total| CostEstimate {
            total: Some(total),
            currency: CURRENCY.to_string(),
            input: None,
            output: None,
            cache_read: None,
            cache_write: None,
            // Our arithmetic in every arm: a hosted rate card, a configured
            // capacity price, or the unpriced local zero.
            source: CostSource::ModelPricing,
            pricing_provider: Some(match capacity_cost {
                Some(_) => LOCAL_CAPACITY_PRICING_PROVIDER.to_string(),
                None => key.provider.clone(),
            }),
            pricing_model: Some(key.model.clone()),
            pricing_as_of: None,
            pricing_source: None,
        }),
        estimated_cost_saved: saved,
        currency: saved.map(|_| CURRENCY.to_string()),
        contributions: vec![contribution(turn, baseline, &tokens)],
    })
}

/// Why this summary is not a complete calculation.
///
/// A closed vocabulary, because a consumer greps it. Each entry names one input
/// that was unavailable or one gate that was applied, and the presence of any of
/// them is what makes [`status`] `Partial`.
fn limitations(turn: &TurnRecord, baseline: &Baseline<'_>) -> Vec<String> {
    let mut limitations = Vec::new();
    if let Some(Correlary::Unpriced { reason, .. }) = baseline.correlary {
        limitations.push(format!("roundhouse_correlary_unpriced:{reason}"));
    }
    if turn.usage.accounting == Accounting::Estimated {
        limitations.push("roundhouse_usage_estimated".to_string());
    }
    // On every turn that sought a counterfactual, priced or not: the round-2
    // ruling asks that a gated number never sit indistinguishable beside an
    // ungated one, and a reader cannot tell the difference from a band that is
    // only published when the gate refused.
    //
    // The band is rendered by `f64`'s own shortest round-trip form, which is
    // safe to grep because of where it comes from: `capability_band` is a JSON
    // literal in `ROUNDHOUSE_CATALOG` (`catalog_config.rs`, validated onto the
    // unit interval) or the shipped default, so the digits an operator wrote
    // are the digits in the string. A band computed at a call site could spell
    // itself `0.30000000000000004`; nothing in the tree computes one, and the
    // day something does this wants a fixed precision instead.
    if baseline.correlary.is_some() {
        limitations.push(format!(
            "roundhouse_capability_gate:{}",
            baseline.capability_band
        ));
    }
    // Not in the ruling's three, and added because omitting it would make the
    // status field lie: a forwarded seat publishes no cost of any kind, and a
    // summary with no money in it and `status: Complete` claims every requested
    // calculation was available.
    if let Some(decision) = turn.decision()
        && !decision.billing.is_billable()
    {
        limitations.push("roundhouse_seat_forwarded".to_string());
    }
    limitations
}

/// `Complete` if and only if nothing was missing.
///
/// One function, deliberately, and it is the only place the two states are
/// decided. Relay's own builder does exactly this; a producer that chose a
/// status independently could publish `Complete` beside a listed limitation,
/// which describes nothing.
fn status(limitations: &[String]) -> LlmOptimizationSummaryStatus {
    match limitations.is_empty() {
        true => LlmOptimizationSummaryStatus::Complete,
        false => LlmOptimizationSummaryStatus::Partial,
    }
}

/// What this turn would have cost on its stand-in, where there is one.
///
/// **An `Unpriced` correlary answers `None` and never `0.0`.** Its
/// `shadow_cost_usd` returns a structural zero — there is no rate card in that
/// arm to price against, which is a different thing from a counterfactual that
/// came out free — and publishing it as a `baseline_cost` of $0.00 would tell a
/// consumer that routing locally saved nothing, when what happened is that no
/// comparable model could be justified.
fn shadow_cost(
    local: bool,
    billed: bool,
    correlary: Option<&Correlary>,
    usage: &Usage,
) -> Option<f64> {
    if !local || !billed {
        return None;
    }
    match correlary? {
        priced @ Correlary::Priced { .. } => Some(priced.shadow_cost_usd(usage)),
        Correlary::Unpriced { .. } => None,
    }
}

/// What this local turn cost our own fleet at the catalog's
/// `local_capacity_price`, or `None` when the catalog sets no price or the turn
/// is hosted or a forwarded seat's.
///
/// `LocalCapacityPrice::price`, the arithmetic the dashboard's capacity spend
/// is, over this turn's usage. The price is the catalog's current one, as the
/// dashboard's is, and not a figure the decision recorded: the decision's own
/// local quote is an estimate over expected tokens, and this is the measured
/// turn.
fn capacity_cost(
    local: bool,
    billed: bool,
    price: Option<LocalCapacityPrice>,
    usage: &Usage,
) -> Option<f64> {
    (local && billed)
        .then_some(price)
        .flatten()
        .map(|price| price.price(usage))
}

/// What this turn actually cost, where roundhouse may say.
///
/// A hosted turn costs its recorded rate card's price. A local turn costs its
/// capacity cost when the catalog prices capacity, and otherwise a zero — our
/// own fleet bills nobody, and that zero is what makes the summary's own
/// arithmetic close, `baseline - actual` being the routing saving. Either way
/// `baseline - actual = saved` holds. A forwarded seat and a hosted turn with no
/// recorded rate card both answer `None`: the first because there is no bill of
/// ours to name, the second because a log written before the card travelled in
/// it can no longer be priced from the log alone.
fn actual_cost(
    local: bool,
    billed: bool,
    card: Option<ProviderPricing>,
    capacity_cost: Option<f64>,
    usage: &Usage,
) -> Option<f64> {
    if !billed {
        return None;
    }
    match local {
        // Unpriced capacity is unknown, not free. See the module doc.
        true => capacity_cost,
        false => card.map(|card| card.price(usage)),
    }
}

/// The token side of the saving.
///
/// Routing saves *money* and not tokens — the counterfactual is the same tokens
/// at another model's rates — so a local turn's saved counts are all absent and
/// the field serializes as `{}`. It is non-optional in Relay's shape, so `{}` is
/// what a turn with no token reduction looks like rather than a bug.
///
/// A hosted turn does have one measured token reduction: the share of its prompt
/// the provider served from its own cache.
fn tokens_saved(local: bool, billed: bool, usage: &Usage) -> LlmOptimizationTokens {
    LlmOptimizationTokens {
        cache_read_tokens: (!local && billed).then_some(usage.cached_input_tokens),
        ..LlmOptimizationTokens::default()
    }
}

/// One routing decision, as the evidence Relay aggregates.
fn contribution(
    turn: &TurnRecord,
    baseline: &Baseline<'_>,
    tokens: &TokenBreakdown,
) -> LlmOptimizationContribution {
    let decision = turn.decision();
    let local = decision.is_some_and(|decision| decision.chosen.is_local());
    let billed = decision.is_some_and(|decision| decision.billing.is_billable());
    let card = decision.and_then(|decision| decision.rate_card);
    let price = card
        .filter(|_| billed && !local)
        .map_or(0.0, |card| card.price(&turn.usage));
    let measured = matches!(turn.usage.accounting, Accounting::Reported);

    let evidence = RoutingEvidence {
        capability_band: baseline.capability_band,
        correlary_basis: match baseline.correlary {
            Some(Correlary::Priced { basis, .. }) => Some(basis.clone()),
            _ => None,
        },
        billed_measured_usd: if measured { price } else { 0.0 },
        billed_estimated_usd: if measured { 0.0 } else { price },
        routing_savings_at_decision_usd: decision
            .and_then(|decision| decision.quoted_routing_saving_usd())
            .filter(|_| billed),
        reasoning_tokens: turn.usage.reasoning_tokens,
        seat_tokens: (!billed).then_some(*tokens),
        session_seq: turn.started_seq,
        turn_id: turn.turn_id.as_str().to_string(),
        response_id: turn.response_id.as_str().to_string(),
    };

    let contribution = LlmOptimizationContribution {
        // Relay assigns both on ingestion and replaces whatever a producer sent,
        // so sending one would be noise a consumer has to know to discard.
        id: None,
        sequence: None,
        producer: PRODUCER.to_string(),
        kind: LlmOptimizationKind::model_routing(),
        // The decision was made and the turn ran under it. Roundhouse has no
        // shadow mode at this seam: a recorded decision is an executed one.
        applied: true,
        model_transition: Some(LlmOptimizationModelTransition {
            baseline: baseline
                .correlary
                .and_then(Correlary::reference)
                .map(|reference| LlmOptimizationModel {
                    model: reference.model.clone(),
                    provider: Some(reference.provider.clone()),
                }),
            effective: decision.map(|decision| {
                let key = ModelKey::from_target(&decision.chosen);
                LlmOptimizationModel {
                    model: key.model,
                    provider: Some(key.provider),
                }
            }),
        }),
        token_impact: Some(LlmOptimizationTokenImpact {
            baseline: None,
            effective: Some(relay_tokens(tokens, &turn.usage)),
            saved: None,
            // Straight off the log's own provenance marker, which is the point
            // of that marker existing: an unreported call folded in as zero
            // tokens for zero dollars is indistinguishable from a saving.
            quality: Some(match measured {
                true => LlmOptimizationEvidenceQuality::Observed,
                false => LlmOptimizationEvidenceQuality::Estimated,
            }),
            estimation_method: (!measured).then(|| "roundhouse-tokenizer".to_string()),
        }),
        payload_schema: None,
        payload: None,
        extra: BTreeMap::new(),
    };
    // Serializing a plain struct of scalars cannot fail; falling back to the
    // unpayloaded contribution rather than unwrapping keeps a report about the
    // past from panicking in a route.
    contribution
        .clone()
        .with_payload(&evidence)
        .unwrap_or(contribution)
}

/// A roundhouse token breakdown in Relay's `Usage` shape.
///
/// `cache_write_tokens` was deliberately absent until M11.0, because roundhouse
/// *priced* uncached prompt tokens at the provider's cache-write rate without
/// *measuring* a cache write, and putting the uncached count in a field named
/// for one would publish a pricing convention as an observation. The rule is
/// unchanged; what changed is that a measurement now exists —
/// `Usage::cache_write_tokens`, folded by the `anthropic_messages` client out of
/// the `cache_creation_input_tokens` its upstream reports — so the field is
/// emitted from that and from nothing else. See [`measured_cache_write`] for the
/// one condition it is emitted under.
///
/// `cost` is absent for the same reason on the other axis: this crate's costs
/// live in `baseline_cost` and `actual_cost`, where their provenance travels
/// with them, and a second copy here would be a number with no `CostSource`.
fn relay_usage(tokens: &TokenBreakdown, usage: &Usage) -> RelayUsage {
    RelayUsage {
        prompt_tokens: Some(tokens.input),
        completion_tokens: Some(tokens.output),
        total_tokens: Some(tokens.total),
        cache_read_tokens: Some(tokens.cached_input),
        cache_write_tokens: measured_cache_write(usage),
        cost: None,
    }
}

fn relay_tokens(tokens: &TokenBreakdown, usage: &Usage) -> LlmOptimizationTokens {
    LlmOptimizationTokens {
        prompt_tokens: Some(tokens.input),
        completion_tokens: Some(tokens.output),
        cache_read_tokens: Some(tokens.cached_input),
        cache_write_tokens: measured_cache_write(usage),
        total_tokens: Some(tokens.total),
    }
}

/// The provider's own cache-write count, or `None` when nobody measured one.
///
/// **Two conditions, and the second is the one a reader will want to argue
/// with.** The counts must be `Accounting::Reported` — our own tokenizer knows
/// nothing about a remote cache, so an estimated turn has no business publishing
/// a write count at all — *and* the count must be positive.
///
/// The positivity test is not a tidy-up. The log stores `0` for two different
/// facts: "the provider reported zero cache writes on this turn" and "this
/// dialect has no such counter, so nothing was ever asked". A Responses turn is
/// the second and is `Reported` all the same, so emitting `Some(0)` for it would
/// publish an observation nobody made — the exact failure this field spent three
/// releases absent to avoid. The cost is that a genuine measured zero is
/// published as "unknown" rather than as zero, which understates what we know
/// and never overstates it; the alternative errs the other way, on the axis the
/// savings figure is computed from.
fn measured_cache_write(usage: &Usage) -> Option<u64> {
    (matches!(usage.accounting, Accounting::Reported) && usage.cache_write_tokens > 0)
        .then_some(usage.cache_write_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, HOSTED, Log};
    use roundhouse_core::control::Billing;
    use roundhouse_core::metrics::{ReferenceModel, ShadowPricing};
    use roundhouse_core::routing::LocalCapacityPrice;
    use serde_json::Value;

    /// A deployment that has declared what its local model stands in for.
    ///
    /// Declared rather than inferred, because inference needs an observed shape
    /// for the hosted candidate and the point of most of these fixtures is a
    /// session that never called one.
    fn declared() -> MetricsConfig {
        MetricsConfig::new(
            ShadowPricing::new(vec![ReferenceModel {
                provider: "anthropic".into(),
                model: "claude".into(),
                pricing: HOSTED,
                quality_prior: 0.6,
            }])
            .declare("llama", "anthropic", "claude", "matched on our eval suite"),
        )
        .with_default_local_quality(0.6)
    }

    /// The same deployment with nothing declared, so the gate has to decide.
    fn undeclared() -> MetricsConfig {
        MetricsConfig::new(ShadowPricing::new(vec![ReferenceModel {
            provider: "anthropic".into(),
            model: "claude".into(),
            pricing: HOSTED,
            quality_prior: 0.95,
        }]))
        .with_default_local_quality(0.35)
    }

    fn summaries(log: &Log, config: &MetricsConfig) -> Vec<LlmOptimizationSummary> {
        for_session(log.events(), config)
    }

    /// Every field name of `LlmOptimizationSummary`, so a pin exists on our side
    /// of a crate we do not control.
    #[test]
    fn the_summary_carries_relays_field_names() {
        let mut log = Log::new("s1");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::local("llama"),
            fixtures::usage(1_000, 0, 100),
        );
        // Priced, so every field Relay defines is present: an unpriced local
        // turn deliberately publishes no `actual_cost`.
        let summary = &summaries(&log, &declared().with_local_capacity_price(CAPACITY))[0];
        let json: Value = serde_json::to_value(summary).unwrap();

        let mut got: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        got.sort_unstable();
        let mut want = vec![
            "schema_version",
            "calculation_version",
            "status",
            "limitations",
            "baseline_model",
            "effective_model",
            "effective_usage",
            "baseline_usage",
            "tokens_saved",
            "baseline_cost",
            "actual_cost",
            "estimated_cost_saved",
            "currency",
            "contributions",
        ];
        want.sort_unstable();
        assert_eq!(got, want);
        assert_eq!(json["schema_version"], "1");
        assert_eq!(json["calculation_version"], "2");

        let contribution = &json["contributions"][0];
        assert_eq!(contribution["producer"], "roundhouse");
        assert_eq!(contribution["kind"], "model_routing");
        assert_eq!(contribution["applied"], true);
        assert_eq!(contribution["payload_schema"]["name"], "roundhouse/routing");
        assert_eq!(contribution["payload_schema"]["version"], "2");
        assert!(
            contribution.get("id").is_none() && contribution.get("sequence").is_none(),
            "Relay assigns both on ingestion and replaces what a producer sent"
        );
    }

    /// Both directions of the derivation, from one fixture pair.
    #[test]
    fn status_is_complete_exactly_when_nothing_was_missing() {
        // A hosted turn on this deployment's key, usage reported, rate card
        // recorded: completely accounted for, and the only shape that is.
        let mut hosted = Log::new("s1");
        hosted.created(None);
        hosted.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(10_000, 8_000, 500),
        );
        let complete = &summaries(&hosted, &declared())[0];
        assert_eq!(complete.status, LlmOptimizationSummaryStatus::Complete);
        assert!(complete.limitations.is_empty());

        // The same turn with the provider silent: one limitation, and the status
        // follows it rather than being chosen.
        let mut estimated = Log::new("s2");
        estimated.created(None);
        estimated.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::estimated(fixtures::usage(10_000, 8_000, 500)),
        );
        let partial = &summaries(&estimated, &declared())[0];
        assert_eq!(partial.status, LlmOptimizationSummaryStatus::Partial);
        assert_eq!(partial.limitations, vec!["roundhouse_usage_estimated"]);
    }

    #[test]
    fn a_local_turn_is_always_partial_and_names_the_gate() {
        let mut log = Log::new("s1");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::local("llama"),
            fixtures::usage(100_000, 90_000, 1_000),
        );
        let summary = &summaries(&log, &declared())[0];

        assert_eq!(summary.status, LlmOptimizationSummaryStatus::Partial);
        assert!(
            summary
                .limitations
                .iter()
                .any(|note| note == "roundhouse_capability_gate:0.1"),
            "a counterfactual gated on configured priors must never sit \
             indistinguishable beside an ungated number: {:?}",
            summary.limitations
        );
        assert_eq!(
            summary.baseline_model.as_ref().map(|m| m.model.as_str()),
            Some("claude")
        );
        assert_eq!(
            summary
                .baseline_model
                .as_ref()
                .and_then(|m| m.provider.as_deref()),
            Some("anthropic"),
            "the provider travels with the model, or the baseline names a \
             string two vendors both use"
        );

        // Core's arithmetic, not ours: same tokens including the same cached
        // fraction, at the reference model's rates.
        let expected = 10_000.0 * 3.75e-6 + 90_000.0 * 0.3e-6 + 1_000.0 * 15.0e-6;
        let baseline = summary.baseline_cost.as_ref().unwrap().total.unwrap();
        assert!(
            (baseline - expected).abs() < 1e-12,
            "{baseline} != {expected}"
        );
        assert_eq!(
            summary.actual_cost, None,
            "unpriced capacity is unknown, not a free zero"
        );
        assert!((summary.estimated_cost_saved.unwrap() - expected).abs() < 1e-12);
    }

    #[test]
    fn an_unpriced_correlary_publishes_as_partial_with_its_reason() {
        let mut log = Log::new("s1");
        log.created(None);
        // A hosted call, so there is an observed shape to infer against, and a
        // local one the gate will refuse to compare with it.
        log.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(10_000, 5_000, 500),
        );
        log.turn(
            "t2",
            "r2",
            fixtures::local("tiny"),
            fixtures::usage(10_000, 5_000, 500),
        );

        let summaries = summaries(&log, &undeclared());
        let local = summaries
            .iter()
            .find(|summary| {
                summary
                    .effective_model
                    .as_ref()
                    .is_some_and(|model| model.model == "tiny")
            })
            .expect("the local turn");
        assert_eq!(local.status, LlmOptimizationSummaryStatus::Partial);
        assert!(
            local
                .limitations
                .iter()
                .any(|note| note.starts_with("roundhouse_correlary_unpriced:")),
            "{:?}",
            local.limitations
        );
        assert!(local.baseline_model.is_none());
        assert!(
            local.baseline_cost.is_none(),
            "no stand-in could be justified, so no shadow price is charged"
        );
        assert_eq!(local.estimated_cost_saved, None);
    }

    /// The rule read off the wire, because `skip_serializing_if` hides an
    /// absent field: a `None` cost is invisible in JSON and present in the type,
    /// so a struct-level assertion would pass on a document that carried one.
    #[test]
    fn a_forwarded_seat_is_priced_into_no_field_at_all() {
        let mut log = Log::new("s1");
        log.created(None);
        let mut seat = fixtures::decision(fixtures::frontier("anthropic", "claude"), Vec::new());
        seat.billing = Billing::AccountedNotBilled;
        log.routed_turn("t1", "r1", seat, fixtures::usage(20_000, 0, 2_000));

        let summary = &summaries(&log, &declared())[0];
        let json = serde_json::to_string(summary).unwrap();
        for field in [
            "baseline_cost",
            "actual_cost",
            "estimated_cost_saved",
            "currency",
        ] {
            assert!(
                !json.contains(field),
                "a seat's tokens must reach no money field, and `{field}` is on \
                 the wire: {json}"
            );
        }
        assert!(
            !json.contains("\"cost\""),
            "not even inside a usage: {json}"
        );
        assert_eq!(summary.status, LlmOptimizationSummaryStatus::Partial);
        assert!(
            summary
                .limitations
                .iter()
                .any(|note| note == "roundhouse_seat_forwarded")
        );

        // The tokens are still real and still reported — as a count, in the
        // payload, with no price beside them.
        let payload: Value = serde_json::from_str(&json).unwrap();
        let seat_tokens = &payload["contributions"][0]["payload"]["seat_tokens"];
        assert_eq!(seat_tokens["total"], 22_000);
        assert_eq!(
            payload["contributions"][0]["payload"]["billed_measured_usd"],
            0.0
        );
        assert_eq!(
            payload["contributions"][0]["payload"]["billed_estimated_usd"],
            0.0
        );

        // CONTROL: the identical turn on this deployment's own key does carry
        // money, so the assertions above are about the seat and not about a
        // rate card having gone missing.
        let mut keyed = Log::new("s2");
        keyed.created(None);
        keyed.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(20_000, 0, 2_000),
        );
        let billed = serde_json::to_string(&summaries(&keyed, &declared())[0]).unwrap();
        assert!(billed.contains("actual_cost"), "{billed}");
    }

    /// **`cache_write_tokens` is published from a measurement and from nothing
    /// else.**
    ///
    /// The field was hardcoded `None` for three releases with a doc saying why:
    /// roundhouse priced uncached tokens at the write rate without measuring a
    /// write, and a field named for an observation must not carry a pricing
    /// convention. M11.0's Anthropic client supplies the measurement, so the
    /// field is now emitted — under two conditions, and each has its own arm
    /// here because dropping either one re-opens the hole the `None` was
    /// protecting.
    #[test]
    fn a_cache_write_is_published_only_when_a_provider_actually_measured_one() {
        let anthropic_turn = |usage: Usage| {
            let mut log = Log::new("s1");
            log.created(None);
            log.turn("t1", "r1", fixtures::frontier("anthropic", "claude"), usage);
            summaries(&log, &declared())[0]
                .effective_usage
                .clone()
                .expect("a dispatched turn publishes its usage")
        };

        // PROBE: a warm Anthropic turn — 10k prompt, 8k read from the provider's
        // cache, 500 newly written. The write count is the provider's own.
        let measured = anthropic_turn(Usage {
            cache_write_tokens: 500,
            ..fixtures::usage(10_000, 8_000, 100)
        });
        assert_eq!(measured.cache_write_tokens, Some(500));
        assert_eq!(
            measured.cache_read_tokens,
            Some(8_000),
            "and the read count is untouched: the two are separate observations"
        );

        // CONTROL 1: the same turn over a dialect with no such counter. `0` in
        // the log means "nobody asked", not "the provider measured zero", so
        // `Some(0)` here would publish an observation nobody made -- and every
        // Responses turn roundhouse has ever served is this case.
        assert_eq!(
            anthropic_turn(fixtures::usage(10_000, 8_000, 100)).cache_write_tokens,
            None
        );

        // CONTROL 2: a measured-looking count on a turn our own tokenizer
        // counted. A local tokenizer knows nothing about a remote cache, so a
        // write count on an estimated turn is arithmetic wearing a
        // measurement's name -- and `estimation_method` beside it would then
        // claim `roundhouse-tokenizer` produced a provider's counter.
        assert_eq!(
            anthropic_turn(fixtures::estimated(Usage {
                cache_write_tokens: 500,
                ..fixtures::usage(10_000, 8_000, 100)
            }))
            .cache_write_tokens,
            None
        );
    }

    #[test]
    fn tokens_saved_is_present_even_when_nothing_was_saved() {
        let mut log = Log::new("s1");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::local("llama"),
            fixtures::usage(1_000, 0, 100),
        );
        let json = serde_json::to_string(&summaries(&log, &declared())[0]).unwrap();
        assert!(
            json.contains(r#""tokens_saved":{}"#),
            "the field is non-optional in Relay's shape, so an empty object is \
             what a turn with no token reduction looks like: {json}"
        );

        // A hosted turn does have one measured reduction: the share of its
        // prompt the provider served from its own cache.
        let mut hosted = Log::new("s2");
        hosted.created(None);
        hosted.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(10_000, 8_000, 100),
        );
        let summary = &summaries(&hosted, &declared())[0];
        assert_eq!(summary.tokens_saved.cache_read_tokens, Some(8_000));
    }

    #[test]
    fn the_payload_carries_what_the_summary_cannot() {
        let mut log = Log::new("s1");
        log.created(None);
        log.routed_turn(
            "t1",
            "r1",
            fixtures::decision(
                fixtures::local("llama"),
                vec![fixtures::candidate(
                    fixtures::frontier("anthropic", "claude"),
                    0.05,
                )],
            ),
            Usage {
                reasoning_tokens: 300,
                ..fixtures::usage(1_000, 0, 900)
            },
        );

        let json: Value = serde_json::to_value(&summaries(&log, &declared())[0]).unwrap();
        let payload = &json["contributions"][0]["payload"];
        assert_eq!(payload["capability_band"], 0.1);
        assert_eq!(payload["correlary_basis"]["kind"], "declared");
        assert_eq!(payload["routing_savings_at_decision_usd"], 0.05);
        assert_eq!(payload["reasoning_tokens"], 300);
        assert_eq!(payload["response_id"], "r1");
        assert!(
            payload.get("seat_tokens").is_some(),
            "the field is present and null on a keyed turn"
        );
    }

    #[test]
    fn two_runs_over_one_log_are_byte_identical() {
        let mut log = Log::new("acme/ada/main");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::local("llama"),
            fixtures::usage(1_000, 0, 100),
        );
        log.turn(
            "t2",
            "r2",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(1_000, 0, 100),
        );

        let first = serde_json::to_string(&summaries(&log, &declared())).unwrap();
        let second = serde_json::to_string(&summaries(&log, &declared())).unwrap();
        assert_eq!(first, second);
    }

    /// A local capacity price at 0.5 / 2.0 per Mtok, distinct from every hosted
    /// rate so a term billed at the wrong rate cannot cancel out.
    const CAPACITY: LocalCapacityPrice = LocalCapacityPrice {
        input_per_mtok_usd: 0.5,
        output_per_mtok_usd: 2.0,
    };

    /// The pricing wire string a local capacity cost is published under.
    const CAPACITY_PROVIDER: &str = "roundhouse_local_capacity_price";

    /// One billed local turn the router chose over a hosted quote of $0.05,
    /// with `local_quote` as its own quote for the local target.
    fn local_turn_log(local_quote: f64) -> Log {
        let mut decision = fixtures::decision(
            fixtures::local("llama"),
            vec![fixtures::candidate(
                fixtures::frontier("anthropic", "claude"),
                0.05,
            )],
        );
        decision.expected_cost_usd = local_quote;
        let mut log = Log::new("s1");
        log.created(None);
        log.routed_turn(
            "t1",
            "r1",
            decision,
            fixtures::usage(100_000, 90_000, 1_000),
        );
        log
    }

    /// The dashboard's own snapshot of the same log, under the same config.
    fn dashboard(log: &Log, config: &MetricsConfig) -> MetricsSnapshot {
        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        MetricsSnapshot::build(&fold, Scope::Deployment, config, 0)
    }

    /// **The router's own saving is the figure the dashboard publishes under
    /// the same name**: the hosted quote less the local quote the turn was
    /// served on, not the gross hosted quote.
    #[test]
    fn a_priced_local_turns_saving_at_decision_is_the_dashboards_net_figure() {
        let priced = declared().with_local_capacity_price(CAPACITY);
        // The router quoted the local target at $0.004 under that price.
        let log = local_turn_log(0.004);

        let json: Value = serde_json::to_value(&summaries(&log, &priced)[0]).unwrap();
        let emitted = json["contributions"][0]["payload"]["routing_savings_at_decision_usd"]
            .as_f64()
            .expect("a billed local turn with a hosted quote publishes one");
        let dashboard = dashboard(&log, &priced)
            .savings
            .routing_savings_at_decision_usd;
        assert!(
            (dashboard - (0.05 - 0.004)).abs() < 1e-12,
            "the dashboard nets the local quote: {dashboard}"
        );
        assert!(
            (emitted - dashboard).abs() < 1e-12,
            "Relay's routing_savings_at_decision_usd {emitted} disagrees with the \
             dashboard's {dashboard} for the same turn"
        );
    }

    /// **With a price, a local turn's actual cost is its capacity cost and its
    /// saving is net of it**, each the same figure the dashboard reports.
    #[test]
    fn a_priced_local_turn_publishes_its_capacity_cost_and_a_net_saving() {
        let priced = declared().with_local_capacity_price(CAPACITY);
        let log = local_turn_log(0.004);
        let summary = &summaries(&log, &priced)[0];
        let snapshot = dashboard(&log, &priced);

        // 10k uncached at 0.5 plus 1k output at 2.0; the 90k cached are free.
        let capacity = 10_000.0 * 0.5e-6 + 1_000.0 * 2.0e-6;
        let shadow = 10_000.0 * 3.75e-6 + 90_000.0 * 0.3e-6 + 1_000.0 * 15.0e-6;

        let actual = summary
            .actual_cost
            .as_ref()
            .expect("a priced local turn has an actual cost");
        let actual_total = actual.total.unwrap();
        assert!(
            (actual_total - capacity).abs() < 1e-12,
            "actual_cost {actual_total} is not the capacity cost {capacity}"
        );
        assert!(
            (actual_total - snapshot.savings.local_capacity_usd.unwrap()).abs() < 1e-12,
            "and it is the dashboard's local capacity spend"
        );
        assert_eq!(actual.source, CostSource::ModelPricing);
        assert_eq!(actual.pricing_model.as_deref(), Some("llama"));
        assert_eq!(
            actual.pricing_provider.as_deref(),
            Some(CAPACITY_PROVIDER),
            "the price is the deployment's configured capacity price, not a \
             vendor's quote"
        );

        let saved = summary.estimated_cost_saved.unwrap();
        assert!(
            (saved - (shadow - capacity)).abs() < 1e-12,
            "estimated_cost_saved {saved} must be net of capacity"
        );
        assert!(
            (saved - snapshot.savings.routing_savings_usd).abs() < 1e-12,
            "and it is the dashboard's routing saving: {} vs {saved}",
            snapshot.savings.routing_savings_usd
        );
        let baseline = summary.baseline_cost.as_ref().unwrap().total.unwrap();
        assert!(
            (baseline - actual_total - saved).abs() < 1e-12,
            "the summary's own arithmetic closes: baseline - actual = saved"
        );
    }

    /// **A price does not manufacture a saving where no stand-in could be
    /// justified.** The dashboard nets capacity only on a row with a priced
    /// correlary; netting it here would publish a loss against nothing.
    #[test]
    fn a_priced_local_turn_with_no_stand_in_has_a_cost_and_no_saving() {
        let mut log = Log::new("s1");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(10_000, 5_000, 500),
        );
        log.turn(
            "t2",
            "r2",
            fixtures::local("tiny"),
            fixtures::usage(10_000, 5_000, 500),
        );
        let summaries = summaries(&log, &undeclared().with_local_capacity_price(CAPACITY));
        let local = summaries
            .iter()
            .find(|summary| {
                summary
                    .effective_model
                    .as_ref()
                    .is_some_and(|model| model.model == "tiny")
            })
            .expect("the local turn");

        assert!(local.baseline_cost.is_none());
        assert_eq!(local.estimated_cost_saved, None);
        let capacity = 5_000.0 * 0.5e-6 + 500.0 * 2.0e-6;
        let actual = local.actual_cost.as_ref().unwrap().total.unwrap();
        assert!((actual - capacity).abs() < 1e-12, "{actual} != {capacity}");
    }

    /// **A seat stays priced into no field when local is priced.** The rule is
    /// that roundhouse puts no money on a forwarded seat's turn, and a capacity
    /// price is money.
    #[test]
    fn a_priced_local_seat_turn_still_publishes_no_money() {
        let mut log = Log::new("s1");
        log.created(None);
        let mut seat = fixtures::decision(fixtures::local("llama"), Vec::new());
        seat.billing = Billing::AccountedNotBilled;
        log.routed_turn("t1", "r1", seat, fixtures::usage(20_000, 0, 2_000));

        let summary = &summaries(&log, &declared().with_local_capacity_price(CAPACITY))[0];
        let json = serde_json::to_string(summary).unwrap();
        for field in [
            "baseline_cost",
            "actual_cost",
            "estimated_cost_saved",
            "currency",
        ] {
            assert!(!json.contains(field), "`{field}` is on the wire: {json}");
        }
    }

    /// **Without a price, a local session publishes this document, byte for
    /// byte.** It is the pre-price emitter's document with two deliberate
    /// changes: no `actual_cost` (unpriced capacity is unknown, not free) and
    /// the bumped calculation and payload versions. Any other drift in the
    /// unpriced path fails here.
    #[test]
    fn an_unpriced_local_turn_publishes_exactly_what_it_did_before() {
        // Unpriced, the router quotes local at zero dollars.
        let log = local_turn_log(0.0);
        let json = serde_json::to_string(&summaries(&log, &declared())).unwrap();
        assert_eq!(json, UNPRICED_LOCAL_DOCUMENT);
    }

    /// CONTROL: **a hosted turn does not move when local is priced.** Its cost
    /// is its own rate card's, and it has no local quote to net.
    #[test]
    fn a_hosted_turn_is_the_same_with_or_without_a_local_price() {
        let mut log = Log::new("s1");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(10_000, 8_000, 500),
        );
        let unpriced = serde_json::to_string(&summaries(&log, &declared())).unwrap();
        let priced = serde_json::to_string(&summaries(
            &log,
            &declared().with_local_capacity_price(CAPACITY),
        ))
        .unwrap();
        assert_eq!(priced, unpriced);
        assert!(unpriced.contains("actual_cost"), "{unpriced}");
    }

    /// What `local_turn_log(0.0)` publishes under `declared()`: no local
    /// capacity price, so no `actual_cost`.
    const UNPRICED_LOCAL_DOCUMENT: &str = r#"[{"schema_version":"1","calculation_version":"2","status":"partial","limitations":["roundhouse_capability_gate:0.1"],"baseline_model":{"model":"claude","provider":"anthropic"},"effective_model":{"model":"llama","provider":"dynamo"},"effective_usage":{"prompt_tokens":100000,"completion_tokens":1000,"total_tokens":101000,"cache_read_tokens":90000},"baseline_usage":{"prompt_tokens":100000,"completion_tokens":1000,"total_tokens":101000,"cache_read_tokens":90000},"tokens_saved":{},"baseline_cost":{"total":0.0795,"currency":"USD","source":"model_pricing","pricing_provider":"anthropic","pricing_model":"claude"},"estimated_cost_saved":0.0795,"currency":"USD","contributions":[{"producer":"roundhouse","kind":"model_routing","applied":true,"model_transition":{"baseline":{"model":"claude","provider":"anthropic"},"effective":{"model":"llama","provider":"dynamo"}},"token_impact":{"effective":{"prompt_tokens":100000,"completion_tokens":1000,"cache_read_tokens":90000,"total_tokens":101000},"quality":"observed"},"payload_schema":{"name":"roundhouse/routing","version":"2"},"payload":{"billed_estimated_usd":0.0,"billed_measured_usd":0.0,"capability_band":0.1,"correlary_basis":{"kind":"declared","note":"matched on our eval suite"},"reasoning_tokens":0,"response_id":"r1","routing_savings_at_decision_usd":0.05,"seat_tokens":null,"session_seq":2,"turn_id":"t1"}}]}]"#;

    #[test]
    fn a_turn_that_never_reached_a_provider_publishes_nothing() {
        let mut log = Log::new("s1");
        log.created(None);
        log.refused_turn(
            "t1",
            "r1",
            roundhouse_core::event::IncompleteReason::PolicyRefused,
        );
        assert!(
            summaries(&log, &declared()).is_empty(),
            "a zero-dollar saving on a call that never happened is the shape a \
             reader mistakes for a bargain"
        );
    }
}
