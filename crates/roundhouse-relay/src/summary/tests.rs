// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

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

/// **The price travels in the snapshot, not beside it.** Baselines read
/// from a priced snapshot publish the capacity `actual_cost` with no
/// config in hand, so no caller can pair a snapshot with a config it was
/// not built from and publish a priced deployment's local turn as
/// unpriced.
#[test]
fn baselines_from_a_priced_snapshot_publish_the_capacity_cost_without_a_config() {
    let priced = declared().with_local_capacity_price(CAPACITY);
    let log = local_turn_log(0.004);
    let snapshot = dashboard(&log, &priced);
    let baselines = Baselines::from_snapshot(&snapshot);
    let summary = &from_replay(&SessionReplay::of(log.events()), &baselines)[0];

    let capacity = 10_000.0 * 0.5e-6 + 1_000.0 * 2.0e-6;
    let actual = summary.actual_cost.as_ref().and_then(|cost| cost.total);
    assert!(
        actual.is_some_and(|actual| (actual - capacity).abs() < 1e-12),
        "a priced snapshot must publish its capacity cost {capacity}, got {actual:?}"
    );
}

/// **An unpriced local turn says why it has no `actual_cost`.** Without
/// the limitation, a consumer sees a missing field and a `Partial` it can
/// only attribute to the capability gate, which every local turn carries
/// priced or not, so nothing on the document would say the hardware cost
/// is unknown rather than free.
#[test]
fn an_unpriced_local_turn_names_its_unpriced_capacity_and_a_priced_one_does_not() {
    const UNPRICED: &str = "roundhouse_local_capacity_unpriced";
    let log = local_turn_log(0.0);

    let unpriced = &summaries(&log, &declared())[0];
    assert!(
        unpriced.limitations.iter().any(|note| note == UNPRICED),
        "{:?}",
        unpriced.limitations
    );

    let priced = &summaries(&log, &declared().with_local_capacity_price(CAPACITY))[0];
    assert!(
        !priced.limitations.iter().any(|note| note == UNPRICED),
        "a priced turn's capacity cost is published: {:?}",
        priced.limitations
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
/// byte.** It is the pre-price emitter's document with three deliberate
/// changes: no `actual_cost` (unpriced capacity is unknown, not free), the
/// `roundhouse_local_capacity_unpriced` limitation that says so, and the
/// bumped calculation and payload versions. Any other drift in the
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
/// capacity price, so no `actual_cost` and a limitation naming why.
const UNPRICED_LOCAL_DOCUMENT: &str = r#"[{"schema_version":"1","calculation_version":"2","status":"partial","limitations":["roundhouse_capability_gate:0.1","roundhouse_local_capacity_unpriced"],"baseline_model":{"model":"claude","provider":"anthropic"},"effective_model":{"model":"llama","provider":"dynamo"},"effective_usage":{"prompt_tokens":100000,"completion_tokens":1000,"total_tokens":101000,"cache_read_tokens":90000},"baseline_usage":{"prompt_tokens":100000,"completion_tokens":1000,"total_tokens":101000,"cache_read_tokens":90000},"tokens_saved":{},"baseline_cost":{"total":0.0795,"currency":"USD","source":"model_pricing","pricing_provider":"anthropic","pricing_model":"claude"},"estimated_cost_saved":0.0795,"currency":"USD","contributions":[{"producer":"roundhouse","kind":"model_routing","applied":true,"model_transition":{"baseline":{"model":"claude","provider":"anthropic"},"effective":{"model":"llama","provider":"dynamo"}},"token_impact":{"effective":{"prompt_tokens":100000,"completion_tokens":1000,"cache_read_tokens":90000,"total_tokens":101000},"quality":"observed"},"payload_schema":{"name":"roundhouse/routing","version":"2"},"payload":{"billed_estimated_usd":0.0,"billed_measured_usd":0.0,"capability_band":0.1,"correlary_basis":{"kind":"declared","note":"matched on our eval suite"},"reasoning_tokens":0,"response_id":"r1","routing_savings_at_decision_usd":0.05,"seat_tokens":null,"session_seq":2,"turn_id":"t1"}}]}]"#;

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
