// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Review finding F2: "Aggregation destroys accounting provenance, then labels
//! estimated spend as measured."
//!
//! It did. The fold merged reported and estimated tokens into one `Usage`
//! before pricing, so two logs whose measured share of spend differed 19x
//! serialized byte-identically under a figure documented as "Measured". The
//! fold now keeps the two provenances in separate accumulators and prices each
//! — free, because pricing is linear in tokens — so these tests assert the
//! split is present and correct rather than absent.
//!
//! Uses only the public API of `roundhouse-core`. No library source was
//! modified to make this test possible.

use roundhouse_core::event::{Accounting, SessionEvent, SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::metrics::pricing::{ReferenceModel, ShadowPricing};
use roundhouse_core::metrics::{MetricsConfig, MetricsFold, MetricsSnapshot};
use roundhouse_core::routing::{DecisionRecord, ProviderPricing, Target};

const HOSTED: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 3.0,
    cached_input_per_mtok_usd: 0.3,
    cache_write_per_mtok_usd: 3.75,
    output_per_mtok_usd: 15.0,
};

fn frontier() -> Target {
    Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    }
}

fn reported(input: u64, cached: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        cached_input_tokens: cached,
        output_tokens: output,
        reasoning_tokens: 0,
        accounting: Accounting::Reported,
    }
}

fn estimated(input: u64, cached: u64, output: u64) -> Usage {
    Usage {
        accounting: Accounting::Estimated,
        ..reported(input, cached, output)
    }
}

fn config() -> MetricsConfig {
    MetricsConfig::new(ShadowPricing::new(vec![ReferenceModel {
        provider: "anthropic".into(),
        model: "claude".into(),
        pricing: HOSTED,
        quality_prior: 0.6,
    }]))
}

/// A log of hosted calls, each routed and completed, all to the same model.
fn log(calls: &[Usage]) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    let session = SessionId::new("s1");
    let mut at_ms = 1_000u64;
    let mut push = |events: &mut Vec<SessionEvent>, kind: SessionEventKind| {
        at_ms += 10;
        let seq = events.len() as u64 + 1;
        events.push(SessionEvent {
            seq,
            session_id: session.clone(),
            at_ms,
            kind,
        });
    };
    for (i, usage) in calls.iter().enumerate() {
        let response_id = ResponseId::new(format!("r{i}"));
        push(
            &mut events,
            SessionEventKind::TurnStarted {
                turn_id: TurnId::new(format!("t{i}")),
                response_id: response_id.clone(),
            },
        );
        push(
            &mut events,
            SessionEventKind::Routed {
                response_id: response_id.clone(),
                decision: DecisionRecord {
                    chosen: frontier(),
                    rationale: "test".into(),
                    policy: "test".into(),
                    isl_tokens: usage.input_tokens,
                    expected_prefill_tokens: 0.0,
                    expected_cost_usd: 0.0,
                    considered: vec![],
                },
            },
        );
        push(
            &mut events,
            SessionEventKind::ResponseCompleted {
                response_id,
                usage: usage.clone(),
            },
        );
    }
    events
}

fn snapshot_of(calls: &[Usage]) -> MetricsSnapshot {
    let mut fold = MetricsFold::new();
    fold.extend(log(calls).iter());
    MetricsSnapshot::build(&fold, &config(), 9_999)
}

/// Everything a consumer of the snapshot can observe about the hosted row and
/// the headline, as JSON. This is exactly what `/v1/metrics` hands the
/// dashboard, so if two worlds serialize identically the dashboard cannot tell
/// them apart.
fn observable(snapshot: &MetricsSnapshot) -> String {
    let mut value = serde_json::to_value(snapshot).expect("snapshot serializes");
    // Drop the wall-clock fields; they are not part of the accounting claim.
    let obj = value.as_object_mut().unwrap();
    obj.remove("generated_at_ms");
    obj.remove("first_event_at_ms");
    obj.remove("last_event_at_ms");
    serde_json::to_string_pretty(&value).unwrap()
}

/// Two deployments, same model, same number of calls, same total tokens, same
/// call-level coverage. They differ only in WHICH call the provider reported.
///
/// The published total is legitimately identical — the same tokens were billed
/// either way, and that was never the defect. What must differ is how much of
/// that total anyone can stand behind.
#[test]
fn measured_and_estimated_spend_are_separable_in_the_snapshot() {
    // World A: the provider reported the big call. 95% of the money is real.
    let a_reported = reported(190_000, 0, 19_000);
    let a_estimated = estimated(10_000, 0, 1_000);
    // World B: the provider reported the small call. 5% of the money is real.
    let b_reported = reported(10_000, 0, 1_000);
    let b_estimated = estimated(190_000, 0, 19_000);

    let a = snapshot_of(&[a_reported.clone(), a_estimated.clone()]);
    let b = snapshot_of(&[b_reported.clone(), b_estimated.clone()]);

    // Pricing is linear in tokens, so the true split is exactly computable
    // from the same rate card the snapshot applies.
    let a_measured = HOSTED.price(&a_reported);
    let a_guessed = HOSTED.price(&a_estimated);
    let b_measured = HOSTED.price(&b_reported);
    let b_guessed = HOSTED.price(&b_estimated);

    let a_share = a_measured / (a_measured + a_guessed);
    let b_share = b_measured / (b_measured + b_guessed);
    assert!(
        (a_share - 0.95).abs() < 0.01 && (b_share - 0.05).abs() < 0.01,
        "test setup wrong: shares were {a_share} and {b_share}"
    );

    assert!(
        (a.savings.frontier_spend_usd - b.savings.frontier_spend_usd).abs() < 1e-12,
        "same tokens, same total spend"
    );

    assert!(
        (a.savings.frontier_spend_measured_usd - a_measured).abs() < 1e-12,
        "world A: measured spend should be ${a_measured:.6}, got ${:.6}",
        a.savings.frontier_spend_measured_usd,
    );
    assert!(
        (b.savings.frontier_spend_measured_usd - b_measured).abs() < 1e-12,
        "world B: measured spend should be ${b_measured:.6}, got ${:.6}",
        b.savings.frontier_spend_measured_usd,
    );
    assert!(
        (a.savings.frontier_spend_estimated_usd - a_guessed).abs() < 1e-12
            && (b.savings.frontier_spend_estimated_usd - b_guessed).abs() < 1e-12,
        "the estimated halves must be exact too"
    );

    // The two halves must reconstitute the published total, or the split is
    // decoration rather than a decomposition.
    for (world, snapshot) in [("A", &a), ("B", &b)] {
        let parts = snapshot.savings.frontier_spend_measured_usd
            + snapshot.savings.frontier_spend_estimated_usd;
        assert!(
            (parts - snapshot.savings.frontier_spend_usd).abs() < 1e-12,
            "world {world}: measured + estimated must equal the total"
        );
    }

    // Call-weighted coverage still cannot tell the two apart. That is why it
    // was never a sufficient proxy, and why the token-weighted one exists.
    assert_eq!(
        a.coverage.reported_calls, b.coverage.reported_calls,
        "call coverage is identical in both worlds, as it always was"
    );
    assert!(
        a.coverage_token_fraction > 0.9 && b.coverage_token_fraction < 0.1,
        "token coverage separates them: {} vs {}",
        a.coverage_token_fraction,
        b.coverage_token_fraction,
    );

    // The whole point: a consumer of /v1/metrics can now tell the two apart.
    assert_ne!(
        observable(&a),
        observable(&b),
        "the two worlds must not serialize identically"
    );
}

#[test]
fn a_fully_estimated_hosted_model_is_not_published_as_measured_spend() {
    // Not one call in this log was reported by the provider. Every token is
    // Roundhouse's own tokenizer output.
    let snapshot = snapshot_of(&[estimated(100_000, 0, 10_000), estimated(50_000, 0, 5_000)]);

    assert_eq!(snapshot.coverage.reported_calls, 0);
    assert!(
        snapshot.savings.frontier_spend_usd > 0.0,
        "the provider still billed for these calls"
    );
    assert_eq!(
        snapshot.savings.frontier_spend_measured_usd, 0.0,
        "none of it was measured, so none of it may be reported as measured"
    );
    assert!(
        (snapshot.savings.frontier_spend_estimated_usd - snapshot.savings.frontier_spend_usd).abs()
            < 1e-12,
        "all of it is estimated"
    );
    assert_eq!(
        snapshot.coverage_token_fraction, 0.0,
        "and no token behind it was counted by anyone but us"
    );

    // The per-model row carries the same split, so a reader does not have to
    // infer it from the headline.
    let row = &snapshot.models[0];
    assert_eq!(row.billed_measured_usd(), 0.0);
    assert!((row.billed_estimated_usd() - row.billed_usd()).abs() < 1e-12);
    assert_eq!(row.coverage.estimated_tokens, snapshot.tokens.total);
}

/// The cache discount is wholly measured even at zero coverage, and not by
/// luck: an unreported call records `cached_input_tokens: 0`, because nothing
/// observable bears on what a remote cache did. So it contributes zero here
/// rather than a guess, and the figure keeps its "measured" label honestly.
#[test]
fn the_cache_discount_stays_measured_when_nothing_else_is() {
    let snapshot = snapshot_of(&[
        reported(100_000, 80_000, 5_000),
        estimated(100_000, 0, 5_000),
    ]);

    let measured_only = snapshot_of(&[reported(100_000, 80_000, 5_000)]);
    assert!(
        (snapshot.savings.cache_savings_usd - measured_only.savings.cache_savings_usd).abs()
            < 1e-12,
        "the unreported call must contribute nothing to the cache discount"
    );
    assert!(snapshot.savings.cache_savings_usd > 0.0);
}
