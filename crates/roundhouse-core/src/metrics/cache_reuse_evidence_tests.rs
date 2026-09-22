// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! B1: what the router predicted about cache reuse, and what evidence the log
//! holds about the answer.
//!
//! Beside the snapshot rather than inside it, for the reason
//! `turn_elapsed_snapshot_tests` gives: `mod.rs` already carries the vocabulary
//! and the recorder, and a third block of fixtures buries both.
//!
//! The observed half is gated on `Usage::cache_read_source`, which is what
//! makes it derivable at all: both wire decoders fill `cached_input_tokens`
//! with a zero when the provider omits the field, so before that marker existed
//! a silent upstream and a cold prefix were one value. A **stated** zero is
//! ordinary evidence and pairs like any other count; silence does not, and a
//! local dispatch does not, because its credit is the router's own quote handed
//! back.
//!
//! Every assertion reads the **serialized** row rather than the struct. The
//! column is a wire contract — a dashboard and a later learner both read it as
//! JSON — and asserting against the field a `skip_serializing_if` may have
//! dropped is the only way an absence control is about the document instead of
//! about the accessor.
//!
//! Every state here is reached by folding a log. There is no test-only door
//! into the counters, because the publish rule is the thing under test and a
//! fixture that wrote counters directly would prove it against a state the fold
//! cannot produce.

use super::tests::{config, snapshot};
use super::*;
use crate::control::{Billing, PrincipalKey};
use crate::event::{Accounting, CacheReadSource, IncompleteReason, SessionEventKind, Usage};
use crate::ids::{ResponseId, TurnId};
use crate::metrics::fold::tests::{LogBuilder, decision_for, frontier, local, principal, usage};
use crate::routing::{DecisionRecord, Target};

/// The published evidence for one row, as the document carries it.
///
/// Panics naming the row's whole JSON when the column is absent, so a red run
/// says which field went missing rather than comparing a `null` with a number.
fn published(fold: &MetricsFold, model: &str) -> serde_json::Value {
    published_in(&snapshot(fold), model)
}

fn published_in(snapshot: &MetricsSnapshot, model: &str) -> serde_json::Value {
    let row = snapshot
        .models
        .iter()
        .find(|row| row.model == model)
        .unwrap_or_else(|| panic!("the fixture booked no {model} row"));
    let json = serde_json::to_value(row).expect("a row serializes");
    json.get("cache_reuse_evidence")
        .cloned()
        .unwrap_or_else(|| panic!("the {model} row publishes no evidence column: {json}"))
}

/// One optional ratio off the published evidence.
fn ratio(evidence: &serde_json::Value, field: &str) -> Option<f64> {
    evidence
        .get(field)
        .unwrap_or_else(|| panic!("no {field} in {evidence}"))
        .as_f64()
}

/// One count off the published evidence.
fn count(evidence: &serde_json::Value, field: &str) -> u64 {
    ratio(evidence, field).unwrap_or_else(|| panic!("{field} is not a number in {evidence}")) as u64
}

fn close(seen: Option<f64>, want: f64, what: &str, evidence: &serde_json::Value) {
    let seen = seen.unwrap_or_else(|| panic!("{what} is absent in {evidence}"));
    assert!(
        (seen - want).abs() < 1e-12,
        "{what} is {seen}, want {want}: {evidence}"
    );
}

/// A provider that counted its own tokens and stated its cache read.
fn reported(input: u64, cached: u64) -> Usage {
    Usage {
        cache_read_source: CacheReadSource::Provider,
        ..usage(input, cached, 100, 0)
    }
}

/// A provider that counted its tokens and said nothing about its cache, which
/// is what an omitted `input_tokens_details` decodes to.
fn silent_about_cache(input: u64) -> Usage {
    usage(input, 0, 100, 0)
}

/// The local path's synthesized credit: a real count, computed by us.
fn derived(input: u64, cached: u64) -> Usage {
    Usage {
        cache_read_source: CacheReadSource::Derived,
        ..usage(input, cached, 100, 0)
    }
}

/// The same counts, from our tokenizer because the provider reported none.
fn our_own(input: u64, cached: u64) -> Usage {
    Usage {
        accounting: Accounting::Estimated,
        ..reported(input, cached)
    }
}

/// One dispatch of an already-started turn, carrying its own prediction.
fn predict(
    log: &mut LogBuilder,
    response: &str,
    target: Target,
    isl_tokens: u64,
    expected_prefill: f64,
    billing: Billing,
) {
    log.push(SessionEventKind::Routed {
        response_id: ResponseId::new(response),
        decision: DecisionRecord {
            expected_prefill_tokens: expected_prefill,
            billing,
            ..decision_for(target, isl_tokens)
        },
    });
}

/// One whole turn: a decision that predicted `expected_prefill` over
/// `isl_tokens`, and a completion carrying whatever the provider said.
fn turn_predicting(
    log: &mut LogBuilder,
    response: &str,
    target: Target,
    isl_tokens: u64,
    expected_prefill: f64,
    usage: Usage,
) {
    turn_predicting_billed(
        log,
        response,
        target,
        isl_tokens,
        expected_prefill,
        usage,
        Billing::Billed,
    );
}

fn turn_predicting_billed(
    log: &mut LogBuilder,
    response: &str,
    target: Target,
    isl_tokens: u64,
    expected_prefill: f64,
    usage: Usage,
    billing: Billing,
) {
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new(format!("turn-{response}")),
        response_id: ResponseId::new(response),
    });
    predict(log, response, target, isl_tokens, expected_prefill, billing);
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new(response),
        usage,
        provider_reported_cost_usd: None,
        stop_reason: None,
    });
}

fn claude() -> Target {
    frontier("anthropic", "claude")
}

// --- the two findings, now carried by the vocabulary ----------------------

/// **Finding 1, fixed.** A stated zero is evidence; silence is not.
///
/// Before `Usage::cache_read_source` the two were the same value — both
/// decoders fill the count with a zero when the field is absent — so dividing
/// it scored a silent upstream as a measured miss. The distinction now survives
/// into the log, and the fold splits on it rather than on the number.
///
/// The stated zero pairs, which is the half that must not be lost to a rule
/// that simply skipped every zero: a cold prefix a provider reported is exactly
/// the observation a router most needs.
#[test]
fn a_stated_cache_zero_pairs_and_a_silent_provider_does_not() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    // Stated zero: predicted 0.8, observed 0.0.
    turn_predicting(&mut log, "r1", claude(), 1_000, 200.0, reported(1_000, 0));
    // Silent: the same shape on the wire, and no evidence at all.
    turn_predicting(
        &mut log,
        "r2",
        claude(),
        1_000,
        200.0,
        silent_about_cache(1_000),
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let seen = published(&fold, "claude");
    assert_eq!(
        count(&seen, "samples"),
        1,
        "only the stated zero pairs: {seen}"
    );
    assert_eq!(count(&seen, "measured_cache_reads"), 1, "{seen}");
    assert_eq!(count(&seen, "unverifiable_cache_read"), 1, "{seen}");
    close(
        ratio(&seen, "observed_mean_ratio"),
        0.0,
        "a stated cold prefix is a real zero",
        &seen,
    );
    close(
        ratio(&seen, "mean_signed_error"),
        -0.8,
        "the router expected 0.8 reuse and the provider reported none",
        &seen,
    );
    // Both turns still predicted, whatever their provider said.
    assert_eq!(count(&seen, "predictions"), 2, "{seen}");
}

/// **Finding 2, fixed.** A local dispatch's credit is the router's own quote,
/// and is excluded by provenance rather than by a special case.
///
/// `Engine::local_stream` synthesizes `cached = isl - effective_prefill`, and
/// the decision recorded `expected_prefill_tokens = effective_prefill`. The two
/// ratios are one number with two names, so pairing them would publish a
/// structurally zero error as a result. The engine marks that count
/// `CacheReadSource::Derived`, which keeps it priceable and out of the sample.
#[test]
fn a_local_derived_cache_credit_prices_but_never_pairs() {
    let isl: u64 = 4_096;
    let effective_prefill: u64 = 512;
    // Exactly `engine.rs`'s `isl_tokens.saturating_sub(effective_prefill_tokens)`.
    let synthesized = isl - effective_prefill;
    assert_eq!(
        (isl as f64 - effective_prefill as f64) / isl as f64,
        synthesized as f64 / isl as f64,
        "the identity this exclusion exists for"
    );

    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    turn_predicting(
        &mut log,
        "r1",
        local("llama"),
        isl,
        effective_prefill as f64,
        derived(isl, synthesized),
    );
    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let seen = published(&fold, "llama");
    assert_eq!(
        count(&seen, "samples"),
        0,
        "a local row never pairs: {seen}"
    );
    assert_eq!(count(&seen, "unverifiable_cache_read"), 1, "{seen}");
    assert_eq!(ratio(&seen, "mean_signed_error"), None, "{seen}");
    // And the count still reached the token breakdown, so pricing is untouched.
    let row = snapshot(&fold)
        .models
        .into_iter()
        .find(|row| row.model == "llama")
        .expect("the turn booked a row");
    assert_eq!(
        row.tokens.cached_input, synthesized,
        "excluding the credit from the sample must not remove it from pricing"
    );
}

/// A log written before the provenance marker existed is unknown, not a miss.
///
/// `#[serde(default)]` lands every historical `Usage` on
/// `CacheReadSource::Unreported`. That loses real measurements, which is the
/// conservative direction: absent evidence rather than invented evidence.
#[test]
fn a_log_written_before_the_marker_is_unknown_rather_than_a_measured_miss() {
    let historical: Usage = serde_json::from_value(serde_json::json!({
        "input_tokens": 1_000,
        "cached_input_tokens": 0,
        "output_tokens": 100,
        "reasoning_tokens": 0,
        "accounting": "reported",
    }))
    .expect("a pre-marker usage still deserializes");
    assert_eq!(historical.cache_read_source, CacheReadSource::Unreported);
    assert_eq!(historical.accounting, Accounting::Reported);

    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    turn_predicting(&mut log, "r1", claude(), 1_000, 200.0, historical);
    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let seen = published(&fold, "claude");
    assert_eq!(count(&seen, "samples"), 0, "{seen}");
    assert_eq!(count(&seen, "unverifiable_cache_read"), 1, "{seen}");
}

/// A total that mixes a stated read with a silent one is not a measurement.
#[test]
fn an_aggregate_usage_keeps_the_weakest_provenance_it_absorbed() {
    let mut total = Usage::default();
    total.add(&reported(1_000, 400));
    assert_eq!(
        total.cache_read_source,
        CacheReadSource::Provider,
        "a fresh accumulator adopts rather than degrades, or every total would \
         read as unreported"
    );
    total.add(&silent_about_cache(1_000));
    assert_eq!(
        total.cache_read_source,
        CacheReadSource::Unreported,
        "half a cache count is not a cache count"
    );

    let mut with_local = Usage::default();
    with_local.add(&reported(1_000, 400));
    with_local.add(&derived(1_000, 300));
    assert_eq!(
        with_local.cache_read_source,
        CacheReadSource::Derived,
        "a real count computed by us is weaker than a provider's and stronger \
         than silence"
    );
}

// --- what the log does support ---------------------------------------------

/// The prediction replays from the decision alone, with its basis beside it.
///
/// The literals are hand-computed from the fixture and deliberately asymmetric,
/// so a mean that weighted the two turns differently goes red.
#[test]
fn a_row_publishes_its_predicted_cache_ratio_with_its_basis() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    // Predicted 800/1000 = 0.8.
    turn_predicting(&mut log, "r1", claude(), 1_000, 200.0, reported(1_000, 500));
    // Predicted 100/1000 = 0.1.
    turn_predicting(&mut log, "r2", claude(), 1_000, 900.0, reported(1_000, 300));

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let seen = published(&fold, "claude");
    assert_eq!(count(&seen, "predictions"), 2, "{seen}");
    close(
        ratio(&seen, "predicted_mean_ratio"),
        0.45,
        "predicted_mean_ratio",
        &seen,
    );

    // The literal, not the constant: this string is what a consumer reads to
    // know which denominator the ratio is over, and asserting it against the
    // value it came from would agree with any rename.
    assert_eq!(
        seen.get("predicted_basis").and_then(|b| b.as_str()),
        Some("routed_isl_minus_expected_prefill"),
        "{seen}"
    );
    assert_eq!(
        seen.get("observed_basis").and_then(|b| b.as_str()),
        Some("stated_cached_input_over_input"),
        "{seen}"
    );
    assert_eq!(PREDICTED_CACHE_BASIS, "routed_isl_minus_expected_prefill");
    assert_eq!(OBSERVED_CACHE_BASIS, "stated_cached_input_over_input");
}

/// The prediction is stated over the router's own token count, never the
/// provider's.
///
/// Not a hypothetical: the recorded `isl_tokens` is our tokenizer over the
/// conversation *plus the re-declared toolbox*, while `input_tokens` is the
/// provider's count of its own prompt. The fixture makes them disagree by 250
/// tokens, so a ratio that reached for the provider's denominator reads 0.8
/// rather than 0.75.
#[test]
fn the_prediction_keeps_its_own_denominator_when_the_bases_disagree() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    turn_predicting(&mut log, "r1", claude(), 1_000, 250.0, reported(1_250, 500));

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let seen = published(&fold, "claude");
    close(
        ratio(&seen, "predicted_mean_ratio"),
        0.75,
        "predicted over the router's own 1000",
        &seen,
    );
}

/// The four usage buckets separate the one unambiguous case from the three that
/// are not.
#[test]
fn the_usage_census_separates_real_evidence_from_silence() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    // A nonzero reported read: the only unambiguous evidence there is.
    turn_predicting(&mut log, "r1", claude(), 1_000, 200.0, reported(1_000, 600));
    // Silence: no cache detail reached the decoder.
    turn_predicting(
        &mut log,
        "r2",
        claude(),
        1_000,
        200.0,
        silent_about_cache(1_000),
    );
    // No provider accounting at all.
    turn_predicting(&mut log, "r3", claude(), 1_000, 200.0, our_own(1_000, 500));
    // A count that cannot be true.
    turn_predicting(&mut log, "r4", claude(), 1_000, 200.0, reported(100, 900));

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let seen = published(&fold, "claude");
    assert_eq!(
        (
            count(&seen, "measured_cache_reads"),
            count(&seen, "unverifiable_cache_read"),
            count(&seen, "unusable_usage"),
            count(&seen, "invalid_usage"),
        ),
        (1, 1, 1, 1),
        "{seen}"
    );
    // Every terminal made a prediction, whatever its provider did.
    assert_eq!(count(&seen, "predictions"), 4, "{seen}");
    assert_eq!(count(&seen, "unusable_prediction"), 0, "{seen}");
}

/// Three decisions that predict nothing usable, none of which may read as a
/// prediction of no reuse.
///
/// Each fails differently under the obvious arithmetic and that is why all
/// three are here. A zero ISL divides by zero. A `NaN` expected prefill
/// produces `NaN.max(0.0) == 0.0` in Rust — so the naive numerator collapses to
/// zero and the turn reads as a confident prediction of no reuse. A negative
/// expected prefill clamps the other way and reads as a predicted 1.0.
#[test]
fn a_decision_with_no_usable_prediction_is_counted_and_never_read_as_zero_reuse() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    turn_predicting(&mut log, "r1", claude(), 0, 0.0, reported(1_000, 500));
    turn_predicting(
        &mut log,
        "r2",
        claude(),
        1_000,
        f64::NAN,
        reported(1_000, 0),
    );
    turn_predicting(&mut log, "r3", claude(), 1_000, -50.0, reported(1_000, 900));

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let seen = published(&fold, "claude");
    assert_eq!(count(&seen, "predictions"), 0, "{seen}");
    assert_eq!(count(&seen, "unusable_prediction"), 3, "{seen}");
    assert_eq!(ratio(&seen, "predicted_mean_ratio"), None, "{seen}");
}

/// A turn that failed over carries the prediction of the target that served it.
///
/// The abandoned dispatch's prediction was made about a cache this turn never
/// touched. Booking it on the first target would attribute a belief to a
/// dispatch that never happened.
#[test]
fn a_failover_books_the_prediction_of_the_last_routed_target() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t1"),
        response_id: ResponseId::new("r1"),
    });
    // Warm on claude, or so the router thought: predicted 0.9.
    predict(&mut log, "r1", claude(), 1_000, 100.0, Billing::Billed);
    // Fell forward to gpt, cold-ish: predicted 0.4.
    predict(
        &mut log,
        "r1",
        frontier("openai", "gpt"),
        1_000,
        600.0,
        Billing::Billed,
    );
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r1"),
        usage: reported(1_000, 200),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let seen = published(&fold, "gpt");
    assert_eq!(count(&seen, "predictions"), 1, "{seen}");
    close(
        ratio(&seen, "predicted_mean_ratio"),
        0.4,
        "the served target's own prediction",
        &seen,
    );

    // The abandoned target carries no evidence at all: it was never asked.
    let claude_json = snapshot(&fold)
        .models
        .into_iter()
        .find(|row| row.model == "claude")
        .map(|row| serde_json::to_value(&row).expect("a row serializes"))
        .unwrap_or(serde_json::Value::Null);
    assert!(
        claude_json.get("cache_reuse_evidence").is_none(),
        "the target that never served must publish no evidence: {claude_json}"
    );
}

/// A rebuild over a log already folded adds nothing.
#[test]
fn a_replayed_log_observes_each_terminal_exactly_once() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    turn_predicting(&mut log, "r1", claude(), 1_000, 200.0, reported(1_000, 500));

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let once = published(&fold, "claude");
    assert_eq!(fold.extend(log.events()), 0, "no event is new twice");
    let twice = published(&fold, "claude");

    assert_eq!(once, twice, "a replay must not double an observation");
    assert_eq!(count(&twice, "predictions"), 1, "{twice}");
}

/// A dispatch a retry superseded contributes nothing, and leaves nothing
/// behind.
///
/// Both halves matter. The abandoned response's prediction must not be booked
/// against the retry's terminal, and its state must drain — a prediction held
/// for a response that will never terminate is unbounded memory keyed by a
/// response nobody will mention again.
#[test]
fn a_superseded_dispatch_contributes_nothing_and_leaves_no_pending_state() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    // The abandoned attempt, predicting near-total reuse.
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t1"),
        response_id: ResponseId::new("r1"),
    });
    predict(&mut log, "r1", claude(), 1_000, 50.0, Billing::Billed);
    // The retry of the same turn, predicting much less.
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t1"),
        response_id: ResponseId::new("r2"),
    });
    predict(&mut log, "r2", claude(), 1_000, 700.0, Billing::Billed);
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r2"),
        usage: reported(1_000, 250),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });
    // The abandoned response's terminal, arriving late.
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r1"),
        usage: reported(1_000, 990),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let seen = published(&fold, "claude");
    assert_eq!(count(&seen, "predictions"), 1, "one live dispatch: {seen}");
    close(
        ratio(&seen, "predicted_mean_ratio"),
        0.3,
        "the retry's prediction, not the abandoned attempt's 0.95",
        &seen,
    );
    assert_eq!(
        fold.pending_dispatches(),
        0,
        "every prediction this fold is holding must have drained"
    );
}

/// A scoped document carries its own evidence and nobody else's.
#[test]
fn a_scoped_report_carries_only_its_own_cache_evidence() {
    let acme = principal("acme", "ada");
    let mut mine = LogBuilder::new("acme/ada/main");
    mine.created(Some(acme.clone()));
    turn_predicting(
        &mut mine,
        "r1",
        claude(),
        1_000,
        200.0,
        reported(1_000, 800),
    );

    let mut theirs = LogBuilder::new("globex/bob/main");
    theirs.created(Some(principal("globex", "bob")));
    turn_predicting(
        &mut theirs,
        "r2",
        claude(),
        1_000,
        900.0,
        reported(1_000, 100),
    );

    let mut fold = MetricsFold::new();
    fold.extend(mine.events());
    fold.extend(theirs.events());

    let deployment = published(&fold, "claude");
    assert_eq!(count(&deployment, "predictions"), 2, "{deployment}");
    close(
        ratio(&deployment, "predicted_mean_ratio"),
        0.45,
        "both tenants' predictions",
        &deployment,
    );

    let scoped = MetricsSnapshot::build(
        &fold,
        Scope::Principal(&PrincipalKey::from(&acme)),
        &config(),
        9_999,
    );
    let mine = published_in(&scoped, "claude");
    assert_eq!(count(&mine, "predictions"), 1, "{mine}");
    close(
        ratio(&mine, "predicted_mean_ratio"),
        0.8,
        "one tenant's own prediction",
        &mine,
    );
    assert_eq!(count(&mine, "measured_cache_reads"), 1, "{mine}");
}

/// Cache evidence is not money, so a forwarded seat is observed like any other
/// turn.
///
/// The row's *dollars* refuse a seat's tokens, and every other field on the row
/// follows that split. This one deliberately does not: what a prompt's cache
/// did is a fact about the prompt, not about whose card paid, and a router that
/// learns from these observations would otherwise go blind on every
/// pass-through project.
#[test]
fn a_seat_forwarded_turn_is_observed_like_any_other() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    turn_predicting_billed(
        &mut log,
        "r1",
        claude(),
        1_000,
        400.0,
        reported(1_000, 700),
        Billing::AccountedNotBilled,
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let seen = published(&fold, "claude");
    assert_eq!(count(&seen, "predictions"), 1, "{seen}");
    close(
        ratio(&seen, "predicted_mean_ratio"),
        0.6,
        "predicted_mean_ratio",
        &seen,
    );
    assert_eq!(count(&seen, "measured_cache_reads"), 1, "{seen}");
}

/// A turn that failed after the provider had already prefilled is observed.
///
/// The other side of the evidence gate, and the pair matters more than either
/// alone. Both this turn and the one below end in `ResponseIncomplete`; what
/// separates them is whether a provider counted any input, which is the same
/// rule the call counter uses. An upstream that died mid-stream still read the
/// prompt and still reported what its cache returned.
#[test]
fn an_incomplete_that_still_prefilled_is_observed() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t1"),
        response_id: ResponseId::new("r1"),
    });
    predict(&mut log, "r1", claude(), 1_000, 300.0, Billing::Billed);
    log.push(SessionEventKind::ResponseIncomplete {
        response_id: ResponseId::new("r1"),
        reason: IncompleteReason::UpstreamError,
        usage: reported(1_000, 550),
        terminal_attempt: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let seen = published(&fold, "claude");
    assert_eq!(count(&seen, "predictions"), 1, "{seen}");
    close(
        ratio(&seen, "predicted_mean_ratio"),
        0.7,
        "predicted_mean_ratio",
        &seen,
    );
    assert_eq!(count(&seen, "measured_cache_reads"), 1, "{seen}");
}

/// A dispatch that died before the provider saw the prompt is not evidence of
/// anything, and not even bad evidence.
///
/// Counting it among the unusable would make an outage look like a
/// cache-accounting problem.
#[test]
fn a_dispatch_that_never_reached_the_provider_is_not_an_observation() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t1"),
        response_id: ResponseId::new("r1"),
    });
    predict(&mut log, "r1", claude(), 1_000, 200.0, Billing::Billed);
    log.push(SessionEventKind::ResponseIncomplete {
        response_id: ResponseId::new("r1"),
        reason: IncompleteReason::UpstreamError,
        usage: Usage::default(),
        terminal_attempt: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    // The row exists — the terminal interval books there — and carries no
    // evidence column at all.
    let row = snapshot(&fold)
        .models
        .into_iter()
        .find(|row| row.model == "claude")
        .expect("the terminal interval booked a row");
    let json = serde_json::to_value(&row).expect("a row serializes");
    assert!(
        json.get("cache_reuse_evidence").is_none(),
        "a turn that reached nobody is absent from the document, not a zeroed \
         observation: {json}"
    );
}

/// A row built only out of calls that carry no routing decision publishes no
/// column.
///
/// The absence control, and a real case rather than a contrived one: a side
/// call is dispatched before `plan` and has no `DecisionRecord`, so there is no
/// prediction for its usage to be checked against. The assertion is that the
/// key is *absent* from the serialized row — the `skip_serializing_if` contract
/// its sibling columns document — rather than present and null.
#[test]
fn a_row_with_no_routed_observation_publishes_no_evidence_column() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.side_call(frontier("anthropic", "haiku"), Some(reported(1_000, 400)));

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let row = snapshot(&fold)
        .models
        .into_iter()
        .find(|row| row.model == "haiku")
        .expect("the side call booked a row");
    assert_eq!(row.calls, 1, "the call itself is still counted");
    let json = serde_json::to_value(&row).expect("a row serializes");
    assert!(
        json.get("cache_reuse_evidence").is_none(),
        "a call with no prediction to check publishes nothing: {json}"
    );
}
