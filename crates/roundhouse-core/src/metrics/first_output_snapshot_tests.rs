// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the first-output-latency column publishes, and when it publishes
//! nothing.
//!
//! Split from `mod.rs`'s own `tests` on the `turn_elapsed_snapshot_tests`
//! precedent beside it: `mod.rs` already carries the vocabulary and the
//! recorder, and a second block of fixtures buries both.

use super::tests::snapshot;
use super::*;
use crate::control::Billing;
use crate::event::SessionEventKind;
use crate::ids::ResponseId;
use crate::metrics::fold::tests::{LogBuilder, frontier, usage};

/// The published column: a mean over its samples, with the basis beside it.
#[test]
fn a_model_row_publishes_its_first_output_latency_with_its_basis() {
    let mut log = LogBuilder::new("s1");
    log.turn_speaking(
        "r1",
        frontier("anthropic", "claude"),
        usage(1_000, 0, 100, 0),
        &["hi"],
    );
    log.turn_speaking(
        "r2",
        frontier("anthropic", "claude"),
        usage(1_000, 0, 100, 0),
        &["", "hi"],
    );
    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let row = snapshot(&fold)
        .models
        .into_iter()
        .find(|row| row.model == "claude")
        .expect("the turns booked a row");
    let latency = row.first_output.expect("two turns spoke");
    assert_eq!(latency.samples, 2);
    assert_eq!(latency.rejected, 0);
    assert_eq!(latency.mean_ms, Some(25.0), "the mean of 20 and 30");
    // The literal, not the constant: this string is the wire contract, and
    // asserting it against the value it came from would agree with any
    // rename that silently changed what consumers read.
    assert_eq!(latency.basis, "turn_start_to_first_output");
    assert_eq!(FIRST_OUTPUT_BASIS, "turn_start_to_first_output");
}

/// A row nobody could time publishes no column at all.
#[test]
fn a_row_with_no_usable_timing_publishes_no_latency_column() {
    let mut log = LogBuilder::new("s1");
    log.turn(
        "r1",
        frontier("anthropic", "claude"),
        Vec::new(),
        usage(1_000, 0, 100, 0),
    );
    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let row = snapshot(&fold)
        .models
        .into_iter()
        .find(|row| row.model == "claude")
        .expect("the turn booked a row");
    assert!(
        row.first_output.is_none(),
        "no sample is an absent column, never a zero millisecond answer"
    );
}

/// A row whose only timings went backwards keeps the count and loses the
/// mean, so a skewed clock is visible instead of reading as unmeasured.
#[test]
fn a_row_whose_timings_went_backwards_reports_the_refusals_and_no_mean() {
    let mut log = LogBuilder::new("s1");
    log.start_and_route(
        "r1",
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    log.push_at(
        5,
        SessionEventKind::OutputTextDelta {
            response_id: ResponseId::new("r1"),
            text: "hello".into(),
        },
    );
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r1"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });
    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let row = snapshot(&fold)
        .models
        .into_iter()
        .find(|row| row.model == "claude")
        .expect("the turn booked a row");
    let latency = row.first_output.expect("a refusal is still a report");
    assert_eq!((latency.samples, latency.rejected), (0, 1));
    assert_eq!(latency.mean_ms, None, "no mean is fabricated from nothing");
}
