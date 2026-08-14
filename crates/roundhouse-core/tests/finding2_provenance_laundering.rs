// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Validation test for review finding F2: "Aggregation destroys accounting
//! provenance, then labels estimated spend as measured."
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

#[test]
#[ignore = "F2: validated defect, unfixed — the fold merges reported and estimated tokens before pricing, so spend cannot be decomposed"]
fn measured_and_estimated_spend_cannot_be_separated_in_the_snapshot() {
    // Two deployments. Same model, same number of calls, same total tokens,
    // same accounting coverage (1 of 2 calls unreported). They differ only in
    // WHICH call the provider reported.
    //
    // World A: the provider reported the big call. 95% of the money is real.
    let a_reported = reported(190_000, 0, 19_000);
    let a_estimated = estimated(10_000, 0, 1_000);
    // World B: the provider reported the small call. 5% of the money is real.
    let b_reported = reported(10_000, 0, 1_000);
    let b_estimated = estimated(190_000, 0, 19_000);

    let a = snapshot_of(&[a_reported.clone(), a_estimated.clone()]);
    let b = snapshot_of(&[b_reported.clone(), b_estimated.clone()]);

    // Pricing is linear in tokens, so the split the fold could have kept is
    // exactly computable from the same rate card the snapshot already applies.
    let a_measured = HOSTED.price(&a_reported);
    let a_guessed = HOSTED.price(&a_estimated);
    let b_measured = HOSTED.price(&b_reported);
    let b_guessed = HOSTED.price(&b_estimated);

    let mut violations: Vec<String> = Vec::new();

    // Sanity: the two worlds really do have wildly different measured shares.
    let a_share = a_measured / (a_measured + a_guessed);
    let b_share = b_measured / (b_measured + b_guessed);
    assert!(
        (a_share - 0.95).abs() < 0.01 && (b_share - 0.05).abs() < 0.01,
        "test setup wrong: shares were {a_share} and {b_share}"
    );

    // 1. The headline dollars are identical across the two worlds.
    if (a.savings.frontier_spend_usd - b.savings.frontier_spend_usd).abs() < 1e-12 {
        violations.push(format!(
            "frontier_spend_usd is ${:.6} in BOTH worlds, but the measured part is \
             ${a_measured:.6} in A ({:.0}% of spend) and ${b_measured:.6} in B ({:.0}% of spend). \
             The published \"Measured\" figure is off by ${:.6} between two logs it \
             cannot distinguish.",
            a.savings.frontier_spend_usd,
            a_share * 100.0,
            b_share * 100.0,
            (a_measured - b_measured).abs(),
        ));
    }

    // 2. Coverage is the only provenance that survives, and it is call-counted,
    //    so it is identical too — and therefore a useless proxy for the dollars.
    if a.coverage == b.coverage {
        violations.push(format!(
            "coverage is identical in both worlds: {}/{} calls reported ({:.0}%). \
             A reader applying coverage_fraction to the spend would conclude \
             ${:.6} is measured; the truth is ${a_measured:.6} in A and \
             ${b_measured:.6} in B.",
            a.coverage.reported_calls,
            a.coverage.calls,
            a.coverage_fraction * 100.0,
            a.savings.frontier_spend_usd * a.coverage_fraction,
        ));
    }

    // 3. Nothing else in the whole snapshot separates them either.
    let (ja, jb) = (observable(&a), observable(&b));
    if ja == jb {
        violations.push(format!(
            "the ENTIRE serialized snapshot is byte-identical across the two worlds \
             ({} bytes). No consumer of /v1/metrics can recover which dollars were \
             measured. Snapshot:\n{ja}",
            ja.len(),
        ));
    }

    assert!(
        violations.is_empty(),
        "F2: provenance does not survive the fold:\n\n{}",
        violations.join("\n\n"),
    );
}

#[test]
#[ignore = "F2: validated defect, unfixed — the fold merges reported and estimated tokens before pricing, so spend cannot be decomposed"]
fn a_fully_estimated_hosted_model_is_still_published_as_measured_spend() {
    // Not one call in this log was reported by the provider. Every token is
    // Roundhouse's own tokenizer output.
    let snapshot = snapshot_of(&[estimated(100_000, 0, 10_000), estimated(50_000, 0, 5_000)]);

    let mut violations: Vec<String> = Vec::new();

    if snapshot.coverage.reported_calls == 0 && snapshot.savings.frontier_spend_usd > 0.0 {
        violations.push(format!(
            "0 of {} calls were reported, yet frontier_spend_usd is ${:.6} and the field \
             is documented and labelled \"Measured\" (metrics/mod.rs:418, \
             dashboard.html:296 renders a static <div class=\"kpi-note\">measured</div> \
             that no JS ever rewrites).",
            snapshot.coverage.calls, snapshot.savings.frontier_spend_usd,
        ));
    }

    // Usage::add degrades Usage::accounting to Estimated on contact. Check
    // whether that degraded marker reaches the wire at all.
    let json = serde_json::to_value(&snapshot).unwrap();
    let text = serde_json::to_string(&json).unwrap();
    if !text.contains("\"accounting\"") {
        violations.push(
            "the aggregate Usage::accounting marker — which Usage::add correctly degrades \
             to Estimated (event.rs:114) — appears nowhere in the serialized snapshot. \
             TokenBreakdown::from_usage drops it and ModelMetrics has no field for it, \
             so the degraded marker is computed and then discarded."
                .to_string(),
        );
    }

    // Is there any monetary confidence class at all?
    let money_keys: Vec<&str> = vec![
        "measured_billed_usd",
        "estimated_billed_usd",
        "billed_usd_measured",
        "billed_usd_estimated",
        "frontier_spend_measured_usd",
        "frontier_spend_estimated_usd",
    ];
    if !money_keys.iter().any(|k| text.contains(k)) {
        violations.push(format!(
            "no per-figure monetary confidence class exists anywhere in the snapshot. \
             The only provenance field is Coverage, which counts CALLS, not tokens or \
             dollars. Tokens are likewise merged: tokens.total is {} with no \
             reported/estimated split.",
            snapshot.tokens.total,
        ));
    }

    assert!(
        violations.is_empty(),
        "F2: estimated spend is published as measured:\n\n{}",
        violations.join("\n\n"),
    );
}
