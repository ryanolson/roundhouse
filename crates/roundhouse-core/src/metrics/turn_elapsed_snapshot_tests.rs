// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! B1: what the two turn-elapsed columns publish, and when they publish nothing.
//!
//! The wire half of `fold::turn_elapsed_tests`. Kept beside the snapshot rather
//! than in it for the reason that file gives: `mod.rs` already carries the
//! vocabulary and the recorder, and another block of fixtures buries both.
//!
//! Every state here is reached by folding a log, never by writing a counter
//! through a test-only door. The publish rule is the thing under test, and a
//! fixture that set the counters directly would prove it against a state the
//! fold cannot actually produce.

use super::tests::{config, snapshot};
use super::*;
use crate::control::{Billing, PrincipalKey};
use crate::event::{IncompleteReason, SessionEventKind, Usage};
use crate::ids::{ResponseId, TurnId};
use crate::metrics::fold::tests::{LogBuilder, frontier, principal, usage};

/// The row every test here is about.
fn claude_row(fold: &MetricsFold) -> ModelMetrics {
    snapshot(fold)
        .models
        .into_iter()
        .find(|row| row.model == "claude")
        .expect("the fixture booked a claude row")
}

/// The published shape of one column, flattened for comparison.
fn seen(column: Option<TurnElapsed>) -> Option<(Option<f64>, u64, u64)> {
    column.map(|c| (c.mean_ms, c.samples, c.rejected))
}

/// A turn that ends the way the caller asked. `LogBuilder` stamps ten apart, so
/// a turn with a delta takes thirty milliseconds and one without takes twenty.
fn turn(log: &mut LogBuilder, response: &str, completed: bool, spoke: bool) {
    log.start_and_route(
        response,
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    if spoke {
        log.push(SessionEventKind::OutputTextDelta {
            response_id: ResponseId::new(response),
            text: "hi".into(),
        });
    }
    match completed {
        true => log.push(SessionEventKind::ResponseCompleted {
            response_id: ResponseId::new(response),
            usage: usage(1_000, 0, 100, 0),
            provider_reported_cost_usd: None,
            stop_reason: None,
        }),
        false => log.push(SessionEventKind::ResponseIncomplete {
            response_id: ResponseId::new(response),
            reason: IncompleteReason::UpstreamError,
            usage: usage(1_000, 0, 0, 0),
            terminal_attempt: None,
        }),
    };
}

/// A turn whose terminal is stamped before its own start.
fn backward_turn(log: &mut LogBuilder, response: &str, completed: bool) {
    log.push_at(
        1_000,
        SessionEventKind::TurnStarted {
            turn_id: TurnId::new(format!("turn-{response}")),
            response_id: ResponseId::new(response),
        },
    );
    log.route(
        response,
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    match completed {
        true => log.push_at(
            900,
            SessionEventKind::ResponseCompleted {
                response_id: ResponseId::new(response),
                usage: usage(1_000, 0, 100, 0),
                provider_reported_cost_usd: None,
                stop_reason: None,
            },
        ),
        false => log.push_at(
            900,
            SessionEventKind::ResponseIncomplete {
                response_id: ResponseId::new(response),
                reason: IncompleteReason::UpstreamError,
                usage: usage(1_000, 0, 0, 0),
                terminal_attempt: None,
            },
        ),
    };
}

/// Both columns publish a mean over their own samples, with the basis beside it.
#[test]
fn a_model_row_publishes_each_outcome_class_with_its_basis() {
    let mut log = LogBuilder::new("s1");
    turn(&mut log, "r1", true, true);
    turn(&mut log, "r2", true, true);
    turn(&mut log, "r3", false, true);

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let row = claude_row(&fold);

    let completed = row.completed_turn_elapsed.expect("two turns finished");
    assert_eq!((completed.samples, completed.rejected), (2, 0));
    assert_eq!(completed.mean_ms, Some(30.0));
    let incomplete = row.incomplete_turn_elapsed.expect("one turn did not");
    assert_eq!((incomplete.samples, incomplete.rejected), (1, 0));
    assert_eq!(incomplete.mean_ms, Some(30.0));

    // The literal, not the constant: this string is the wire contract, and
    // asserting it against the value it came from would agree with any rename
    // that silently changed what consumers read.
    assert_eq!(completed.basis, "turn_start_to_terminal");
    assert_eq!(incomplete.basis, "turn_start_to_terminal");
    assert_eq!(TURN_ELAPSED_BASIS, "turn_start_to_terminal");
    assert_ne!(
        TURN_ELAPSED_BASIS, FIRST_OUTPUT_BASIS,
        "the two observations answer different questions and must not share a \
         name a consumer switches on"
    );
}

/// The publish rule, over every combination a fold can reach.
///
/// Table-driven because the claim is the boundary: a class with nothing to say
/// is absent rather than zero, a class with only refusals keeps its column and
/// loses its mean, and one class publishing must not drag the other into view.
#[test]
fn a_class_publishes_only_what_it_measured() {
    struct Case {
        name: &'static str,
        build: fn(&mut LogBuilder),
        expect_completed: Option<(Option<f64>, u64, u64)>,
        expect_incomplete: Option<(Option<f64>, u64, u64)>,
    }

    let cases = [
        Case {
            name: "a row whose turns started before this fold was watching",
            build: |log| {
                log.route(
                    "r1",
                    frontier("anthropic", "claude"),
                    1_000,
                    Billing::Billed,
                );
                log.push(SessionEventKind::ResponseCompleted {
                    response_id: ResponseId::new("r1"),
                    usage: usage(1_000, 0, 100, 0),
                    provider_reported_cost_usd: None,
                    stop_reason: None,
                });
            },
            expect_completed: None,
            expect_incomplete: None,
        },
        Case {
            name: "one class measured, the other silent",
            build: |log| {
                turn(log, "r1", true, false);
                turn(log, "r2", true, false);
            },
            expect_completed: Some((Some(20.0), 2, 0)),
            expect_incomplete: None,
        },
        Case {
            name: "a class whose only timings went backwards",
            build: |log| backward_turn(log, "r1", true),
            expect_completed: Some((None, 0, 1)),
            expect_incomplete: None,
        },
        Case {
            name: "both classes, one of them mixed",
            build: |log| {
                turn(log, "r1", true, false);
                turn(log, "r2", true, false);
                turn(log, "r3", false, false);
                turn(log, "r4", false, false);
                backward_turn(log, "r5", false);
            },
            expect_completed: Some((Some(20.0), 2, 0)),
            expect_incomplete: Some((Some(20.0), 2, 1)),
        },
    ];

    for case in cases {
        let mut log = LogBuilder::new("s1");
        (case.build)(&mut log);
        let mut fold = MetricsFold::new();
        fold.extend(log.events());

        let row = claude_row(&fold);
        assert_eq!(
            seen(row.completed_turn_elapsed),
            case.expect_completed,
            "{}: completed",
            case.name
        );
        assert_eq!(
            seen(row.incomplete_turn_elapsed),
            case.expect_incomplete,
            "{}: incomplete",
            case.name
        );
    }
}

/// The unrouted count is published, and it is scoped like every other figure.
///
/// A tenant reading its neighbour's refusals would be the same disclosure the
/// window and the turn count are already scoped to avoid.
#[test]
fn the_unrouted_terminal_count_is_published_and_scoped() {
    let mut refusing = LogBuilder::new("s1");
    refusing.created(Some(principal("acme", "ada")));
    refusing.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t1"),
        response_id: ResponseId::new("r1"),
    });
    refusing.push(SessionEventKind::ResponseIncomplete {
        response_id: ResponseId::new("r1"),
        reason: IncompleteReason::PolicyRefused,
        usage: Usage::default(),
        terminal_attempt: None,
    });
    let mut serving = LogBuilder::new("s2");
    serving.created(Some(principal("globex", "eve")));
    turn(&mut serving, "r2", true, true);

    let mut fold = MetricsFold::new();
    fold.extend(refusing.events());
    fold.extend(serving.events());

    let scoped = |who| {
        MetricsSnapshot::build(
            &fold,
            Scope::Principal(&PrincipalKey::from(&who)),
            &config(),
            9_999,
        )
        .unrouted_terminals
    };
    assert_eq!(
        snapshot(&fold).unrouted_terminals,
        1,
        "the deployment's own"
    );
    assert_eq!(scoped(principal("acme", "ada")), 1, "ada refused one turn");
    assert_eq!(
        scoped(principal("globex", "eve")),
        0,
        "eve refused none, and must not read ada's"
    );
}

/// A deployment that has refused nothing publishes a zero, not an absence.
///
/// Unlike a mean, zero is a true and complete answer here: it says every
/// terminal this scope measured had a target to book it against.
#[test]
fn a_deployment_that_refused_nothing_publishes_zero_unrouted_terminals() {
    let mut log = LogBuilder::new("s1");
    turn(&mut log, "r1", true, true);
    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    assert_eq!(snapshot(&fold).unrouted_terminals, 0);
}

/// A row that measured neither class serializes exactly as it did before the
/// columns existed.
#[test]
fn a_row_with_no_intervals_omits_both_columns_on_the_wire() {
    let mut log = LogBuilder::new("s1");
    // A dispatch whose start this fold never saw: a row, and no interval.
    log.route(
        "r1",
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r1"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });
    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let row = claude_row(&fold);
    assert!(row.completed_turn_elapsed.is_none());
    assert!(row.incomplete_turn_elapsed.is_none());

    let json = serde_json::to_value(&row).expect("a row serializes");
    assert!(
        json.get("completed_turn_elapsed").is_none()
            && json.get("incomplete_turn_elapsed").is_none(),
        "an unmeasured class is absent from the document, not a null: {json}"
    );
}
