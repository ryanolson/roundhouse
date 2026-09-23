// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! B1: turn start to terminal event, per outcome class.
//!
//! A sibling file rather than another block in `fold.rs`, which is already long
//! enough that a reader loses the fold itself among its fixtures. The fixtures
//! stay where they are and are imported: one `LogBuilder` means one clock, and
//! a second copy here would make every stamp in these tests agree with the ones
//! next door only by coincidence.

use super::tests::{LogBuilder, claude, decision_for, frontier, local, principal, row, usage};
use super::*;
use crate::control::{Billing, PrincipalKey, ProjectId};
use crate::event::{IncompleteReason, SessionEventKind, Usage};
use crate::ids::{ResponseId, TurnId};
use crate::routing::{AttemptClass, DecisionRecord, DispatchAttempt};

/// The whole B1 interval in one fixture: start, dispatch, text, terminal.
///
/// `LogBuilder` stamps ten milliseconds apart, so a turn that speaks once
/// terminates thirty after its start and one that never speaks, twenty.
fn completed_turn(log: &mut LogBuilder, response: &str, spoke: bool) {
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
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new(response),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });
}

/// The same shape, ending the other way.
fn incomplete_turn(log: &mut LogBuilder, response: &str, reason: IncompleteReason, usage: Usage) {
    log.start_and_route(
        response,
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    log.push(SessionEventKind::ResponseIncomplete {
        response_id: ResponseId::new(response),
        reason,
        usage,
        terminal_attempt: None,
    });
}

/// The two classes are measured, and neither reaches the other's pot.
///
/// One assertion pair per class rather than two tests, because the claim is
/// about the pair: a merge shows up here as the wrong total in *both*.
#[test]
fn a_terminal_books_its_interval_in_its_own_outcome_class() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    // Start, Routed, delta, ResponseCompleted: terminal is thirty after start.
    completed_turn(&mut log, "r1", true);
    // Start, Routed, ResponseIncomplete: terminal is twenty after start.
    incomplete_turn(
        &mut log,
        "r2",
        IncompleteReason::UpstreamError,
        usage(1_000, 0, 0, 0),
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let claude = row(&fold, &claude());
    assert_eq!(
        (
            claude.completed_elapsed.samples,
            claude.completed_elapsed.ms_total
        ),
        (1, 30),
        "the completed turn's own interval, and only it"
    );
    assert_eq!(
        (
            claude.incomplete_elapsed.samples,
            claude.incomplete_elapsed.ms_total
        ),
        (1, 20),
        "the incomplete turn's own interval, and only it"
    );
}

/// A turn that never spoke is still a terminal with two stamps.
///
/// The half `first_output` cannot see: no delta means no first-output sample,
/// and the terminal interval is unaffected by that.
#[test]
fn a_turn_that_never_spoke_still_has_a_terminal_interval() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    completed_turn(&mut log, "r1", false);

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let claude = row(&fold, &claude());
    assert_eq!(claude.first_output.samples, 0, "nothing was ever said");
    assert_eq!(
        (
            claude.completed_elapsed.samples,
            claude.completed_elapsed.ms_total
        ),
        (1, 20),
        "but the turn still ended, and when it ended is measured"
    );
}

/// Failover books the whole interval on the target that transmitted.
///
/// The engine writes one `Routed` per attempt and the pending map is last-wins,
/// so the interval spans the dead attempt and lands on the survivor. The dead
/// target's own row carries `failed_attempts` and no interval — booking a
/// timing there would report the fallback as the slow one.
#[test]
fn a_failover_books_the_whole_interval_on_the_target_that_transmitted() {
    let kimi = ModelKey {
        mode: ServingMode::Frontier,
        provider: "moonshot".into(),
        model: "kimi".into(),
    };
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t1"),
        response_id: ResponseId::new("r1"),
    });
    // The first dispatch, to a target that never opened a stream.
    log.route("r1", frontier("moonshot", "kimi"), 1_000, Billing::Billed);
    // The second, to the one that did — carrying the attempt it fell forward
    // from, because that is the record the engine writes: the dead dispatch
    // rides the `Routed` of the dispatch its failure caused.
    log.push(SessionEventKind::Routed {
        response_id: ResponseId::new("r1"),
        decision: DecisionRecord {
            attempts: vec![DispatchAttempt {
                target: frontier("moonshot", "kimi"),
                class: AttemptClass::Transport,
                elapsed_ms: 10,
            }],
            ..decision_for(frontier("anthropic", "claude"), 1_000)
        },
    });
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r1"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let served = row(&fold, &claude());
    assert_eq!(
        (
            served.completed_elapsed.samples,
            served.completed_elapsed.ms_total
        ),
        (1, 30),
        "measured from the turn's start, not from the surviving dispatch, so \
         the time the dead attempt burned is inside the interval"
    );
    let abandoned = fold
        .summed_rows(Scope::Deployment)
        .get(&kimi)
        .cloned()
        .expect("the dead attempt rides the next dispatch's record, so kimi has a row");
    assert_eq!(
        abandoned.failed_attempts, 1,
        "and what that row says about kimi is that one dispatch to it died"
    );
    assert_eq!(
        (
            abandoned.completed_elapsed,
            abandoned.incomplete_elapsed,
            abandoned.first_output.samples
        ),
        (Elapsed::default(), Elapsed::default(), 0),
        "the target that never transmitted gets no interval, in either outcome \
         class -- booking one there would report the fallback as the slow one"
    );
}

/// A retried turn's abandoned response contributes no interval, and its late
/// terminal contributes nothing either.
///
/// Both the clock and the pending entry drain at supersession, which is what
/// makes the late terminal a no-op rather than an unrouted one.
#[test]
fn a_superseded_response_books_no_interval_and_its_late_terminal_books_none() {
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
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r2"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });
    // The fenced owner's terminal finally lands, after the retry already
    // settled. It has neither a clock nor a dispatch.
    log.push(SessionEventKind::ResponseIncomplete {
        response_id: ResponseId::new("r1"),
        reason: IncompleteReason::OwnerLost,
        usage: Usage::default(),
        terminal_attempt: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let claude = row(&fold, &claude());
    assert_eq!(
        (
            claude.completed_elapsed.samples,
            claude.completed_elapsed.ms_total
        ),
        (1, 20),
        "only the retry, measured from the retry's own start"
    );
    assert_eq!(
        claude.incomplete_elapsed.samples, 0,
        "the abandoned response's late terminal is not a second observation"
    );
    assert_eq!(
        fold.unrouted_terminals_of_principal.values().sum::<u64>(),
        0,
        "and it is not an unrouted one either: it had no clock, which is the \
         half of the gate that tells it apart from a refusal"
    );
}

/// Two sessions may hold one `TurnId`, and neither supersedes the other.
///
/// The accounting half of this is already covered next door; this is the same
/// claim for the new counters, which would otherwise lose one session's
/// interval to the other session's retry.
#[test]
fn one_turn_id_in_two_sessions_keeps_both_intervals() {
    let ada = principal("acme", "ada");
    let bo = principal("globex", "bo");
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
    second.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r2"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });

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

    assert_eq!(
        row(&fold, &claude()).completed_elapsed.samples,
        2,
        "both sessions finished a turn, so both intervals are real"
    );
    for who in [&ada, &bo] {
        let scoped =
            fold.summed_rows(Scope::Principal(&PrincipalKey::from(who)))[&claude()].clone();
        assert_eq!(
            scoped.completed_elapsed.samples, 1,
            "{who:?} keeps its own interval"
        );
    }
}

/// Every way a stamp can be unusable, and what each one contributes.
///
/// Table-driven because the four cases differ only in two numbers and the claim
/// is about the boundary between them: a forward stamp is a sample, an equal
/// one is a zero-millisecond sample and not a refusal, a backward one is a
/// refusal in its own class and nothing else, and a terminal whose start this
/// fold never saw is neither.
#[test]
fn unusable_stamps_are_refused_or_ignored_but_never_folded_as_zero() {
    struct Case {
        name: &'static str,
        /// `None` writes no `TurnStarted` at all.
        start_at_ms: Option<u64>,
        terminal_at_ms: u64,
        expect: Elapsed,
        expect_unrouted: u64,
    }

    let cases = [
        Case {
            name: "an ordinary forward interval",
            start_at_ms: Some(1_000),
            terminal_at_ms: 1_400,
            expect: Elapsed {
                ms_total: 400,
                samples: 1,
                rejected: 0,
            },
            expect_unrouted: 0,
        },
        Case {
            name: "a terminal in the same millisecond as its start",
            start_at_ms: Some(1_000),
            terminal_at_ms: 1_000,
            expect: Elapsed {
                ms_total: 0,
                samples: 1,
                rejected: 0,
            },
            expect_unrouted: 0,
        },
        Case {
            name: "a terminal stamped before its own start",
            start_at_ms: Some(1_000),
            terminal_at_ms: 900,
            expect: Elapsed {
                ms_total: 0,
                samples: 0,
                rejected: 1,
            },
            expect_unrouted: 0,
        },
        Case {
            name: "a terminal whose start this fold never saw",
            start_at_ms: None,
            terminal_at_ms: 1_400,
            expect: Elapsed::default(),
            expect_unrouted: 0,
        },
    ];

    for case in cases {
        let mut log = LogBuilder::new("s1");
        log.created(Some(principal("acme", "ada")));
        if let Some(at_ms) = case.start_at_ms {
            log.push_at(
                at_ms,
                SessionEventKind::TurnStarted {
                    turn_id: TurnId::new("t1"),
                    response_id: ResponseId::new("r1"),
                },
            );
        }
        log.route(
            "r1",
            frontier("anthropic", "claude"),
            1_000,
            Billing::Billed,
        );
        log.push_at(
            case.terminal_at_ms,
            SessionEventKind::ResponseCompleted {
                response_id: ResponseId::new("r1"),
                usage: usage(1_000, 0, 100, 0),
                provider_reported_cost_usd: None,
                stop_reason: None,
            },
        );

        let mut fold = MetricsFold::new();
        fold.extend(log.events());

        assert_eq!(
            row(&fold, &claude()).completed_elapsed,
            case.expect,
            "{}",
            case.name
        );
        // The discriminating half: a missing start is pending-present and
        // clock-absent, which is the mirror of the refusal case below. Without
        // this, a gate written on either half alone would still pass.
        assert_eq!(
            fold.unrouted_terminals_of_principal.values().sum::<u64>(),
            case.expect_unrouted,
            "{}: unrouted terminals",
            case.name
        );
    }
}

/// A backward stamp is refused only in the class it terminated in.
#[test]
fn a_backward_terminal_is_refused_only_in_its_own_class() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.push_at(
        1_000,
        SessionEventKind::TurnStarted {
            turn_id: TurnId::new("t1"),
            response_id: ResponseId::new("r1"),
        },
    );
    log.route(
        "r1",
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    log.push_at(
        900,
        SessionEventKind::ResponseIncomplete {
            response_id: ResponseId::new("r1"),
            reason: IncompleteReason::UpstreamError,
            usage: usage(1_000, 0, 0, 0),
            terminal_attempt: None,
        },
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let claude = row(&fold, &claude());
    assert_eq!(claude.incomplete_elapsed.rejected, 1);
    assert_eq!(
        claude.completed_elapsed,
        Elapsed::default(),
        "a refusal must not appear in the class the turn did not end in"
    );
}

// The interval of a dispatch that billed nothing is covered next door, by
// `a_started_turn_that_never_spoke_books_its_interval_without_booking_a_call`:
// that test already owns the fixture and the `calls: 0` claim, and B1 changed
// the rule it asserts rather than adding a second one beside it.

/// A turn refused before any dispatch is counted, not booked and not dropped.
///
/// `RoutingError::PolicyRefused` terminates a turn that wrote `TurnStarted` and
/// never reached `record_routing`, so both stamps exist and no target does.
/// Hand-built rather than taken from the relay fixtures, whose `refused_turn`
/// writes a `Routed` — that shape has a pending target and would book an
/// ordinary incomplete sample, which is the opposite of what this is about.
#[test]
fn a_refusal_with_no_dispatch_is_counted_as_unrouted_and_makes_no_row() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t1"),
        response_id: ResponseId::new("r1"),
    });
    log.push(SessionEventKind::ResponseIncomplete {
        response_id: ResponseId::new("r1"),
        reason: IncompleteReason::PolicyRefused,
        usage: Usage::default(),
        terminal_attempt: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    assert!(
        fold.summed_rows(Scope::Deployment).is_empty(),
        "no target was chosen, so no row may be invented to carry the timing: \
         a zero-token row reads as a free call: {:?}",
        fold.summed_rows(Scope::Deployment)
            .keys()
            .collect::<Vec<_>>()
    );
    assert_eq!(
        fold.view(Scope::Principal(&PrincipalKey::from(&principal(
            "acme", "ada"
        ))))
        .totals
        .unrouted_terminals,
        1,
        "the interval the two means do not cover is counted where the refusing \
         principal can see it"
    );
    assert!(fold.clocks.is_empty(), "and the clock drains anyway");
}

/// A dispatch with no clock and no consumption mints no row either — the
/// other half of core-metrics-3's unified guard from the test above, which
/// covers a clock with no dispatch.
///
/// `Routed` with no preceding `TurnStarted` is not a shape the engine writes
/// today, but the fold must not invent a zero-call row for it: the guard
/// reads `clock` and `consumed` together, and this is the one combination
/// where both are absent while `pending` is not. `provider_cost` needs no
/// third term in the guard: it is `Some` only on `ResponseCompleted`, which
/// is exactly what makes `consumed` true, so a dispatch this guard drops
/// never had a provider cost to lose either.
#[test]
fn a_dispatch_with_no_clock_and_no_consumption_makes_no_row() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.route(
        "r1",
        frontier("anthropic", "claude"),
        1_000,
        Billing::Billed,
    );
    log.push(SessionEventKind::ResponseIncomplete {
        response_id: ResponseId::new("r1"),
        reason: IncompleteReason::PolicyRefused,
        usage: Usage::default(),
        terminal_attempt: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    assert!(
        fold.summed_rows(Scope::Deployment).is_empty(),
        "no clock and no consumed usage: nothing to book, so no row: {:?}",
        fold.summed_rows(Scope::Deployment)
            .keys()
            .collect::<Vec<_>>()
    );
}

/// A completion with no dispatch counts too.
///
/// The counter is about the shape of the log, not about the reason: a
/// `ResponseCompleted` that no `Routed` precedes is a fact worth surfacing, and
/// filing it under a refusal would name a reason nobody observed.
#[test]
fn an_unrouted_completion_counts_as_well_as_an_unrouted_refusal() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t1"),
        response_id: ResponseId::new("r1"),
    });
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r1"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    assert_eq!(fold.view(Scope::Deployment).totals.unrouted_terminals, 1);
}

/// Every terminal drains every map it opened, whichever way it ended.
///
/// The booking branches on the outcome class inside one arm, so a drain proved
/// only for a completion leaves the incomplete path unguarded — and the residue
/// this checks for is unbounded in a long-lived process.
#[test]
fn a_terminal_of_either_kind_drains_the_state_it_opened() {
    for completed in [true, false] {
        let mut log = LogBuilder::new("s1");
        log.created(Some(principal("acme", "ada")));
        match completed {
            true => completed_turn(&mut log, "r1", true),
            false => incomplete_turn(
                &mut log,
                "r1",
                IncompleteReason::UpstreamError,
                usage(1_000, 0, 0, 0),
            ),
        }

        let mut fold = MetricsFold::new();
        fold.extend(log.events());

        assert!(
            fold.clocks.is_empty(),
            "completed={completed}: the clock must drain: {:?}",
            fold.clocks.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            fold.pending_dispatches(),
            0,
            "completed={completed}: the dispatch must drain with it"
        );
        assert_eq!(
            fold.open_turns(),
            0,
            "completed={completed}: and so must the turn"
        );
    }
}

/// Merging two rows adds both classes, in every term.
///
/// The one definition of what a deployment-wide row means. A class missing from
/// `absorb` under-reports silently and only at the scope nobody tests directly.
#[test]
fn absorbing_a_row_adds_both_classes_in_every_term() {
    let mut left = Counters {
        completed_elapsed: Elapsed {
            ms_total: 30,
            samples: 1,
            rejected: 2,
        },
        incomplete_elapsed: Elapsed {
            ms_total: 5,
            samples: 1,
            rejected: 0,
        },
        ..Counters::default()
    };
    let right = Counters {
        completed_elapsed: Elapsed {
            ms_total: 70,
            samples: 3,
            rejected: 1,
        },
        incomplete_elapsed: Elapsed {
            ms_total: 15,
            samples: 2,
            rejected: 4,
        },
        ..Counters::default()
    };

    left.absorb(&right);

    assert_eq!(
        left.completed_elapsed,
        Elapsed {
            ms_total: 100,
            samples: 4,
            rejected: 3,
        }
    );
    assert_eq!(
        left.incomplete_elapsed,
        Elapsed {
            ms_total: 20,
            samples: 3,
            rejected: 4,
        }
    );
}

/// A project's intervals are the sum of its members', and only its members'.
#[test]
fn a_project_view_sums_its_members_intervals_and_its_own_unrouted_terminals() {
    let mut ada = LogBuilder::new("s1");
    ada.created(Some(principal("acme", "ada")));
    completed_turn(&mut ada, "r1", true);
    let mut bo = LogBuilder::new("s2");
    bo.created(Some(principal("acme", "bo")));
    // Refused before any dispatch: acme's unrouted terminal, not globex's.
    bo.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t2"),
        response_id: ResponseId::new("r2"),
    });
    bo.push(SessionEventKind::ResponseIncomplete {
        response_id: ResponseId::new("r2"),
        reason: IncompleteReason::PolicyRefused,
        usage: Usage::default(),
        terminal_attempt: None,
    });
    let mut eve = LogBuilder::new("s3");
    eve.created(Some(principal("globex", "eve")));
    completed_turn(&mut eve, "r3", true);

    let mut fold = MetricsFold::new();
    fold.extend(ada.events());
    fold.extend(bo.events());
    fold.extend(eve.events());

    let acme = ProjectId::from("acme");
    assert_eq!(
        fold.summed_rows(Scope::Project(&acme))[&claude()]
            .completed_elapsed
            .samples,
        1,
        "ada finished one; bo never dispatched and eve is another project"
    );
    assert_eq!(
        fold.view(Scope::Project(&acme)).totals.unrouted_terminals,
        1,
        "bo's refusal belongs to acme"
    );
    assert_eq!(
        fold.view(Scope::Project(&ProjectId::from("globex")))
            .totals
            .unrouted_terminals,
        0,
        "and to nobody else"
    );
    assert_eq!(
        row(&fold, &claude()).completed_elapsed.samples,
        2,
        "the deployment sums every principal's"
    );
    assert_eq!(
        fold.view(Scope::Deployment).totals.unrouted_terminals,
        1,
        "and so does its unrouted count"
    );
}

/// Re-folding a log changes nothing, and a second terminal adds no second
/// observation.
///
/// Two different guarantees that would each look like the other if only one
/// were tested: the first is the `(session, seq)` watermark, and the second is
/// the clock having drained, which is what stops a duplicate *delivery* under a
/// fresh sequence number from booking twice.
#[test]
fn a_replayed_log_and_a_repeated_terminal_both_add_nothing() {
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    completed_turn(&mut log, "r1", true);

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let once = row(&fold, &claude());

    fold.extend(log.events());
    assert_eq!(
        row(&fold, &claude()).completed_elapsed,
        once.completed_elapsed,
        "the watermark makes a replay free"
    );

    // The same response terminating a second time, at a sequence the fold has
    // not seen. Nothing about the watermark stops this one.
    log.push(SessionEventKind::ResponseIncomplete {
        response_id: ResponseId::new("r1"),
        reason: IncompleteReason::UpstreamError,
        usage: Usage::default(),
        terminal_attempt: None,
    });
    fold.extend(&log.events()[log.events().len() - 1..]);

    let after = row(&fold, &claude());
    assert_eq!(
        after.completed_elapsed, once.completed_elapsed,
        "a second terminal cannot revive a drained clock"
    );
    assert_eq!(
        after.incomplete_elapsed.samples, 0,
        "nor book the same turn again in the other class"
    );
    assert_eq!(
        fold.view(Scope::Deployment).totals.unrouted_terminals,
        0,
        "and a drained clock is not an unrouted terminal either"
    );
}

/// A local target's turns are measured like a hosted one's.
///
/// The interval is about two log stamps, so it has nothing to do with who
/// billed — but every fixture above is hosted, and a booking keyed off the
/// serving mode would pass all of them.
#[test]
fn a_local_target_books_its_interval_too() {
    let llama = ModelKey {
        mode: ServingMode::Local,
        provider: crate::metrics::LOCAL_PROVIDER.into(),
        model: "llama".into(),
    };
    let mut log = LogBuilder::new("s1");
    log.created(Some(principal("acme", "ada")));
    log.start_and_route("r1", local("llama"), 1_000, Billing::Billed);
    log.push(SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new("r1"),
        usage: usage(1_000, 0, 100, 0),
        provider_reported_cost_usd: None,
        stop_reason: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    assert_eq!(
        row(&fold, &llama).completed_elapsed.samples,
        1,
        "a local turn ends at a stamp like any other"
    );
}
