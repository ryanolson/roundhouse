// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! B1: time to first output (R21).
//!
//! A sibling file rather than another block in `fold.rs`, which is already long
//! enough that a reader loses the fold itself among its fixtures. The fixtures
//! stay where they are and are imported: one `LogBuilder` means one clock, and
//! a second copy here would make every stamp in these tests agree with the ones
//! next door only by coincidence.

use super::tests::{LogBuilder, claude, decision_for, frontier, principal, row, usage};
use super::*;
use crate::control::{Billing, PrincipalKey, ProjectId};
use crate::event::{IncompleteReason, SessionEventKind, Usage};
use crate::ids::{ResponseId, TurnId};
use crate::routing::{AttemptClass, DecisionRecord, DispatchAttempt};

/// The measurement R21 asks for: the `TurnStarted` stamp against the first
/// non-empty delta's, on the row of the target that served it.
#[test]
fn a_turns_first_text_is_measured_from_its_start_on_the_row_that_served_it() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    // TurnStarted, Routed, delta: three pushes ten apart, so the first text
    // is twenty milliseconds after the start.
    log.turn_speaking(
        "r1",
        frontier("anthropic", "claude"),
        usage(1_000, 0, 100, 0),
        &["hello"],
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let claude = row(&fold, &claude());
    assert_eq!(claude.first_output.samples, 1);
    assert_eq!(claude.first_output.ms_total, 20);
    assert_eq!(claude.first_output.rejected, 0);
}

/// An empty delta carries no text, so it does not close the interval.
///
/// The provider opens a block before it says anything, and closing on the
/// first delta of any kind would measure to that stamp instead.
#[test]
fn an_empty_delta_does_not_stop_the_clock_and_a_later_one_does_not_move_it() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.turn_speaking(
        "r1",
        frontier("anthropic", "claude"),
        usage(1_000, 0, 100, 0),
        &["", "first", "second"],
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let claude = row(&fold, &claude());
    assert_eq!(claude.first_output.samples, 1, "one turn, one sample");
    assert_eq!(
        claude.first_output.ms_total, 30,
        "the empty delta at +20 is skipped and the first real text at +30 \
         is the measurement; a later delta must not move it"
    );
}

/// A turn that answers with no text at all reports nothing, not zero.
#[test]
fn a_turn_that_never_speaks_leaves_no_sample_and_no_zero() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.turn(
        "r1",
        frontier("anthropic", "claude"),
        Vec::new(),
        usage(1_000, 0, 100, 0),
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let claude = row(&fold, &claude());
    assert_eq!(claude.calls, 1, "the turn itself still booked");
    assert_eq!(
        (claude.first_output.samples, claude.first_output.ms_total),
        (0, 0),
        "a turn with nothing to time contributes no sample, and a zero \
         here would read as an instant answer"
    );
}

/// Text whose turn never started in this fold's view is not a sample.
///
/// There is no start to subtract, and the event's own stamp is not one: a
/// turn that began before this process did would otherwise report the whole
/// interval since the epoch.
#[test]
fn a_delta_with_no_turn_start_in_view_is_not_a_sample() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.route(
        "r1",
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    log.push(SessionEventKind::OutputTextDelta {
        response_id: ResponseId::new("r1"),
        text: "hello".into(),
    });
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r1"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let claude = row(&fold, &claude());
    assert_eq!(
        (claude.first_output.samples, claude.first_output.rejected),
        (0, 0),
        "no start is not a rejection either; there was nothing to measure"
    );
}

/// A first text stamped before its own turn started is refused and counted.
#[test]
fn a_delta_before_its_own_turn_start_is_rejected_rather_than_folded() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.start_and_route(
        "r1",
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    // Behind the start, which is a clock that moved and not a fast answer.
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

    let claude = row(&fold, &claude());
    assert_eq!(claude.first_output.rejected, 1);
    assert_eq!(
        (claude.first_output.samples, claude.first_output.ms_total),
        (0, 0),
        "a refused timing must not reach the mean in either term"
    );
}

/// A turn that billed nothing still reports its interval.
///
/// Both stamps exist on a response that spoke and then failed with no
/// reported input, so the evidence rule that keeps phantom calls out of
/// token denominators does not decide whether the interval is real.
#[test]
fn a_turn_that_billed_nothing_still_reports_the_wait_it_delivered() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.start_and_route("r1", frontier("anthropic", "claude"), 0, Billing::Billed);
    log.push(SessionEventKind::OutputTextDelta {
        response_id: ResponseId::new("r1"),
        text: "half an ans".into(),
    });
    log.push(SessionEventKind::ResponseIncomplete {
        response_id: ResponseId::new("r1"),
        reason: IncompleteReason::UpstreamError,
        usage: Usage::default(),
        terminal_attempt: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let claude = row(&fold, &claude());
    assert_eq!(
        claude.calls, 0,
        "nothing billed, so nothing booked as a call"
    );
    assert_eq!(claude.first_output.samples, 1);
    assert_eq!(claude.first_output.ms_total, 20);
}

/// A retried turn's abandoned response contributes nothing.
///
/// The first response never terminates — its owner was fenced — so its
/// timing is drained by the supersession rule rather than left to attach
/// itself to whatever terminates next.
#[test]
fn a_superseded_response_contributes_no_latency_sample() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    let turn_id = TurnId::new("t1");
    log.push(SessionEventKind::TurnStarted {
        turn_id: turn_id.clone(),
        response_id: ResponseId::new("r1"),
    });
    log.route(
        "r1",
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    log.push(SessionEventKind::OutputTextDelta {
        response_id: ResponseId::new("r1"),
        text: "abandoned".into(),
    });
    // The client retries the same turn under a fresh response.
    log.push(SessionEventKind::TurnStarted {
        turn_id,
        response_id: ResponseId::new("r2"),
    });
    log.route(
        "r2",
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    log.push(SessionEventKind::OutputTextDelta {
        response_id: ResponseId::new("r2"),
        text: "served".into(),
    });
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r2"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let claude = row(&fold, &claude());
    assert_eq!(
        claude.first_output.samples, 1,
        "only the response that finished is a sample"
    );
    assert_eq!(
        claude.first_output.ms_total, 20,
        "measured from the retry's own start, not from the abandoned one"
    );
    // The abandoned response never terminates. Its clock must drain at
    // supersession, or each retry retains another unused map entry.
    assert!(
        fold.clocks.is_empty(),
        "the fenced response's clock must be drained at supersession, not \
         left to leak: {:?}",
        fold.clocks.keys().collect::<Vec<_>>()
    );
}

/// A turn that fell forward books its interval on the target that answered.
///
/// The failover sits inside the interval, which is what the basis says;
/// what it must not do is land on the dead provider's row, which never
/// wrote a delta.
#[test]
fn a_fell_forward_turn_books_its_wait_on_the_target_that_answered() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.start_and_route("r1", frontier("anthropic", "kimi"), 1_000, Billing::Billed);
    // The second dispatch of the same response: the engine writes one
    // `Routed` per attempt, and the dead one rides the decision that
    // replaced it — which is what gives kimi a row at all.
    log.push(SessionEventKind::Routed {
        response_id: ResponseId::new("r1"),
        decision: DecisionRecord {
            attempts: vec![DispatchAttempt {
                target: frontier("anthropic", "kimi"),
                class: AttemptClass::Transport,
                elapsed_ms: 10,
            }],
            ..decision_for(frontier("anthropic", "claude"), 1_000)
        },
    });
    log.push(SessionEventKind::OutputTextDelta {
        response_id: ResponseId::new("r1"),
        text: "hello".into(),
    });
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r1"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let kimi = ModelKey {
        mode: ServingMode::Frontier,
        provider: "anthropic".into(),
        model: "kimi".into(),
    };
    assert_eq!(
        row(&fold, &claude()).first_output.samples,
        1,
        "the target that spoke owns the sample"
    );
    assert_eq!(
        row(&fold, &claude()).first_output.ms_total,
        30,
        "measured from the turn's start, so the failed attempt's time is \
         inside it -- which is what the basis says"
    );
    assert_eq!(
        row(&fold, &kimi).first_output.samples,
        0,
        "the target that never spoke has no latency to report"
    );
}

/// Two sessions may hold one `TurnId`, and neither supersedes the other.
///
/// A `TurnId` is a client's idempotency key *within a session* — the type's
/// own contract — and the server derives it by hashing the turn's rendered
/// items, so two sessions sending the same content hold the same key by
/// construction rather than by collision. Superseding across sessions drops
/// the first session's open dispatch: its tokens, its provider-reported
/// cost and its timing all leave with the `pending` entry, and the two
/// sessions can belong to different projects.
#[test]
fn one_turn_id_in_two_sessions_supersedes_neither() {
    let ada = principal("acme", "ada");
    let bo = principal("globex", "bo");
    // One key, because both clients said the same thing.
    let shared = TurnId::new("turn_same_content");

    let mut first = LogBuilder::new("s1");
    first.created(Some(ada.clone()));
    first.push(SessionEventKind::TurnStarted {
        turn_id: shared.clone(),
        response_id: ResponseId::new("r1"),
    });
    first.route(
        "r1",
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    first.push(SessionEventKind::OutputTextDelta {
        response_id: ResponseId::new("r1"),
        text: "from s1".into(),
    });

    let mut second = LogBuilder::new("s2");
    second.created(Some(bo.clone()));
    second.push(SessionEventKind::TurnStarted {
        turn_id: shared,
        response_id: ResponseId::new("r2"),
    });
    second.route(
        "r2",
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    second.push(SessionEventKind::OutputTextDelta {
        response_id: ResponseId::new("r2"),
        text: "from s2".into(),
    });
    second.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r2"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });

    // s1's terminal event arrives last, which is what makes this a real
    // interleaving rather than two logs folded end to end.
    let mut fold = MetricsFold::new();
    fold.extend(first.events());
    fold.extend(second.events());
    first.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r1"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });
    fold.extend(&first.events()[first.events().len() - 1..]);

    let both = row(&fold, &claude());
    assert_eq!(
        both.first_output.samples, 2,
        "both sessions spoke, so both timings are real"
    );
    assert_eq!(
        both.calls, 2,
        "and the same supersession drops the first session's usage, which \
         is the accounting half of the same defect"
    );

    // Each principal keeps its own, or one project's traffic has been
    // silently folded into nobody.
    for who in [&ada, &bo] {
        let scoped =
            fold.summed_rows(Scope::Principal(&PrincipalKey::from(who)))[&claude()].clone();
        assert_eq!(
            (scoped.first_output.samples, scoped.calls),
            (1, 1),
            "{who:?} served exactly one turn"
        );
    }
}

/// A turn that started, dispatched and died without speaking books its
/// interval, and still books no call.
///
/// The timing block runs above the evidence gate, so it is the one place
/// that can create a serving row for a response the gate would have
/// dropped. A zero-token row reads as a free call — the failure
/// `failed_attempts` and `abandoned_side_calls` are both shaped to avoid.
#[test]
fn a_started_turn_that_never_spoke_books_its_interval_without_booking_a_call() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.start_and_route(
        "r1",
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    // The stream opened and nothing came: empty usage, no attempt to book.
    log.push(SessionEventKind::ResponseIncomplete {
        response_id: ResponseId::new("r1"),
        reason: IncompleteReason::UpstreamError,
        usage: Usage::default(),
        terminal_attempt: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let claude = row(&fold, &claude());
    assert_eq!(
        claude.calls, 0,
        "nothing reached the provider, so the row counts no call: a \
         zero-token row that counted one would read as a free call"
    );
    assert_eq!(
        claude.total_usage().total(),
        0,
        "and it carries no tokens to be priced"
    );
    assert_eq!(
        (
            claude.incomplete_elapsed.samples,
            claude.incomplete_elapsed.ms_total
        ),
        (1, 20),
        "the row exists because the turn ended, and when it ended is the \
         whole of what it reports"
    );
    assert_eq!(
        claude.first_output.samples, 0,
        "the response never spoke, so no first-output sample rides along"
    );
    assert!(
        fold.clocks.is_empty(),
        "the clock drains at the terminal event whether or not it decided"
    );
}

/// A turn split across two replay batches measures the same as a turn
/// folded whole.
///
/// `SessionState::project` feeds the fold in chunks, so a turn's start and
/// its first text routinely arrive in different calls — the one way this
/// clock differs from every counter beside it, which settle from a single
/// event.
#[test]
fn a_turn_folded_in_two_batches_measures_what_it_measures_whole() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.turn_speaking(
        "r1",
        frontier("anthropic", "claude"),
        usage(1_000, 0, 100, 0),
        &["hi"],
    );
    let events = log.events().to_vec();
    // Between the start and the text, which is the boundary that matters.
    let (head, tail) = events.split_at(3);

    let mut whole = MetricsFold::new();
    whole.extend(&events);
    let mut batched = MetricsFold::new();
    batched.extend(head);
    batched.extend(tail);

    assert_eq!(
        row(&batched, &claude()).first_output.samples,
        row(&whole, &claude()).first_output.samples,
    );
    assert_eq!(
        row(&batched, &claude()).first_output.ms_total,
        row(&whole, &claude()).first_output.ms_total,
    );
    assert_eq!(
        row(&batched, &claude()).first_output.ms_total,
        20,
        "and the shared answer is the real one, not two matching zeros"
    );
}

/// A project's latency counters are the sum of its members', like its
/// tokens.
#[test]
fn a_project_view_sums_its_members_latency_samples() {
    let mut ada = LogBuilder::new("s1");
    ada.created(Some(principal("acme", "ada")));
    ada.turn_speaking(
        "r1",
        frontier("anthropic", "claude"),
        usage(1_000, 0, 100, 0),
        &["hi"],
    );
    let mut bo = LogBuilder::new("s2");
    bo.created(Some(principal("acme", "bo")));
    bo.turn_speaking(
        "r2",
        frontier("anthropic", "claude"),
        usage(1_000, 0, 100, 0),
        &["", "hi"],
    );
    let mut eve = LogBuilder::new("s3");
    eve.created(Some(principal("globex", "eve")));
    eve.turn_speaking(
        "r3",
        frontier("anthropic", "claude"),
        usage(1_000, 0, 100, 0),
        &["hi"],
    );

    let mut fold = MetricsFold::new();
    fold.extend(ada.events());
    fold.extend(bo.events());
    fold.extend(eve.events());

    let acme = fold.summed_rows(Scope::Project(&ProjectId::from("acme")))[&claude()].clone();
    assert_eq!(acme.first_output.samples, 2, "ada and bo, never eve");
    assert_eq!(acme.first_output.ms_total, 20 + 30);
    assert_eq!(
        row(&fold, &claude()).first_output.samples,
        3,
        "the deployment sums every principal's"
    );
}
