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
use roundhouse_core::routing::{ProviderPricing, Target};
use serde::Serialize;

use crate::PRODUCER;
use crate::replay::{SessionReplay, TurnRecord};

/// The currency every figure in this module is denominated in.
///
/// A constant rather than a parameter: `ROUNDHOUSE_CATALOG` is a USD rate card
/// and the spend ledger is a USD ledger, so a configurable currency here would
/// be a label over unconverted numbers.
const CURRENCY: &str = "USD";

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
}

impl Baselines {
    /// Resolve correlaries from a session's own events.
    pub fn for_session(events: &[SessionEvent], config: &MetricsConfig) -> Self {
        let mut fold = MetricsFold::new();
        fold.extend(events);
        // `generated_at_ms` is discarded: only the rows are read. It is the one
        // field of a snapshot that would make this function impure, so it is
        // passed as a constant rather than as a clock.
        Self::from_snapshot(&MetricsSnapshot::build(&fold, Scope::Deployment, config, 0))
    }

    /// Read the correlaries out of a snapshot somebody else already built.
    pub fn from_snapshot(snapshot: &MetricsSnapshot) -> Self {
        let mut by_local_model = BTreeMap::new();
        for row in &snapshot.models {
            if let Some(correlary) = row.correlary() {
                by_local_model.insert(row.model.clone(), correlary.clone());
            }
        }
        Self {
            by_local_model,
            capability_band: snapshot.capability_band,
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
    /// What the router itself quoted for the best hosted alternative at the
    /// moment it chose local — an independent estimate of the same
    /// counterfactual `estimated_cost_saved` carries, deliberately not added to
    /// it. Two estimates built from different inputs should land near each
    /// other; when they do not, one of the two models is wrong.
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
    const SCHEMA_VERSION: &'static str = "1";
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
    let hosted_cost = hosted_cost(local, billed, card, usage);
    let shadow_cost = shadow_cost(local, billed, baseline.correlary, usage);

    // A hosted turn has no counterfactual model, so the only saving that exists
    // on it is the discount the provider's own cache applied — measured, from
    // the cache-read tokens it reported and the gap between its two published
    // rates. A local turn's saving is the counterfactual itself.
    let saved = match local {
        true => shadow_cost,
        false => card
            .filter(|_| billed)
            .map(|card| card.cache_savings(usage)),
    };

    Some(LlmOptimizationSummary {
        schema_version: "1".to_string(),
        calculation_version: "1".to_string(),
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
        effective_usage: Some(relay_usage(&tokens)),
        // The counterfactual is deliberately like-for-like — the *same* token
        // counts including the same cached fraction, at the reference model's
        // rates. It is not "what if we had sent this cold", which would assume
        // the hosted provider's cache never warmed and would roughly double the
        // figure on a long session. See `Correlary::shadow_cost_usd`.
        baseline_usage: shadow_cost.map(|_| relay_usage(&tokens)),
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
        actual_cost: hosted_cost.map(|total| CostEstimate {
            total: Some(total),
            currency: CURRENCY.to_string(),
            input: None,
            output: None,
            cache_read: None,
            cache_write: None,
            source: CostSource::ModelPricing,
            pricing_provider: Some(key.provider.clone()),
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

/// What this turn actually billed, where roundhouse may say.
///
/// A local dispatch bills nothing and says so with a measured zero — which is
/// what makes the summary's own arithmetic close, `baseline - actual` being the
/// routing saving. A forwarded seat and a turn with no recorded rate card both
/// answer `None`: the first because there is no bill of ours to name, the second
/// because a log written before the card travelled in it can no longer be priced
/// from the log alone.
fn hosted_cost(
    local: bool,
    billed: bool,
    card: Option<ProviderPricing>,
    usage: &Usage,
) -> Option<f64> {
    if !billed {
        return None;
    }
    match local {
        true => Some(0.0),
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
            .and_then(|decision| decision.quoted_frontier_alternative_usd())
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
            effective: Some(relay_tokens(tokens)),
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
/// `cache_write_tokens` is deliberately absent. Roundhouse prices uncached
/// prompt tokens at the provider's cache-*write* rate, because that is what a
/// provider charges for them — but it does not *measure* a cache write, and
/// putting the uncached count in a field named for one would publish a pricing
/// convention as an observation.
///
/// `cost` is absent for the same reason on the other axis: this crate's costs
/// live in `baseline_cost` and `actual_cost`, where their provenance travels
/// with them, and a second copy here would be a number with no `CostSource`.
fn relay_usage(tokens: &TokenBreakdown) -> RelayUsage {
    RelayUsage {
        prompt_tokens: Some(tokens.input),
        completion_tokens: Some(tokens.output),
        total_tokens: Some(tokens.total),
        cache_read_tokens: Some(tokens.cached_input),
        cache_write_tokens: None,
        cost: None,
    }
}

fn relay_tokens(tokens: &TokenBreakdown) -> LlmOptimizationTokens {
    LlmOptimizationTokens {
        prompt_tokens: Some(tokens.input),
        completion_tokens: Some(tokens.output),
        cache_read_tokens: Some(tokens.cached_input),
        cache_write_tokens: None,
        total_tokens: Some(tokens.total),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, HOSTED, Log};
    use roundhouse_core::control::Billing;
    use roundhouse_core::metrics::{ReferenceModel, ShadowPricing};
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
        let summary = &summaries(&log, &declared())[0];
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
        assert_eq!(json["calculation_version"], "1");

        let contribution = &json["contributions"][0];
        assert_eq!(contribution["producer"], "roundhouse");
        assert_eq!(contribution["kind"], "model_routing");
        assert_eq!(contribution["applied"], true);
        assert_eq!(contribution["payload_schema"]["name"], "roundhouse/routing");
        assert_eq!(contribution["payload_schema"]["version"], "1");
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
            summary.actual_cost.as_ref().unwrap().total,
            Some(0.0),
            "our own fleet bills nothing, and that is a measured zero"
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
