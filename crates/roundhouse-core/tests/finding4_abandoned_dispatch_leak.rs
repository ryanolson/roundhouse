// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Validation test for review finding F4: "Abandoned dispatches remain in the
//! process-wide fold forever."
//!
//! The claim: `MetricsFold` inserts into `pending` on every `Routed` event and
//! removes only on a matching terminal event, so a routed response that never
//! terminates — the case the event model explicitly permits, since a retry
//! mints a fresh `ResponseId` rather than reusing the abandoned one — leaves an
//! entry that nothing ever reclaims, in a map that lives as long as the
//! process and is rebuilt from the log on restart.
//!
//! One additive change was made to the library to make this observable:
//! `MetricsFold::pending_dispatches()` in
//! `crates/roundhouse-core/src/metrics/mod.rs`, returning `self.pending.len()`.
//! It reads a field and changes no behavior.

use roundhouse_core::event::{Accounting, IncompleteReason, SessionEvent, SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::metrics::MetricsFold;
use roundhouse_core::routing::{Candidate, DecisionRecord, Target};

fn local() -> Target {
    Target::Local {
        worker_id: 7,
        dp_rank: 0,
        model: "qwen3-32b".into(),
    }
}

fn hosted() -> Target {
    Target::Frontier {
        provider: "anthropic".into(),
        model: "claude-sonnet-4".into(),
    }
}

fn decision(chosen: Target) -> DecisionRecord {
    DecisionRecord {
        chosen: chosen.clone(),
        rationale: "cheapest viable".into(),
        policy: "test".into(),
        isl_tokens: 4_000,
        expected_prefill_tokens: 4_000.0,
        expected_cost_usd: 0.0,
        considered: vec![
            Candidate {
                target: chosen,
                expected_prefill_tokens: 4_000.0,
                matched_prefix_tokens: 0,
                expected_ttft_ms: 100.0,
                expected_cost_usd: 0.0,
                quality_prior: 0.6,
                load: None,
            },
            Candidate {
                target: hosted(),
                expected_prefill_tokens: 4_000.0,
                matched_prefix_tokens: 0,
                expected_ttft_ms: 500.0,
                expected_cost_usd: 0.09,
                quality_prior: 0.9,
                load: None,
            },
        ],
        turn_policy_digest: String::new(),
        budget_state: Default::default(),
    }
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        cached_input_tokens: 0,
        output_tokens: output,
        reasoning_tokens: 0,
        accounting: Accounting::Reported,
    }
}

/// Appends events to one session's log with monotonic `seq` and `at_ms`.
struct Log {
    session: SessionId,
    events: Vec<SessionEvent>,
    at_ms: u64,
}

impl Log {
    fn new(session: &str) -> Self {
        Self {
            session: SessionId::new(session),
            events: Vec::new(),
            at_ms: 1_000,
        }
    }

    fn push(&mut self, kind: SessionEventKind) {
        self.at_ms += 10;
        self.events.push(SessionEvent {
            seq: self.events.len() as u64 + 1,
            session_id: self.session.clone(),
            at_ms: self.at_ms,
            kind,
        });
    }

    /// A turn that was admitted, routed, and then abandoned: the owner lost its
    /// lease (or died) mid-dispatch, so the terminal append was fenced and no
    /// `ResponseCompleted` / `ResponseIncomplete` ever reached the log. The
    /// engine's settle seam is best-effort on exactly this path, and the
    /// session doc for `pending_routings` names it: "a response that never
    /// terminates — the process died mid-flight".
    fn abandoned_turn(&mut self, turn: &str, target: Target) -> ResponseId {
        let response = ResponseId::generate();
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new(turn),
            response_id: response.clone(),
        });
        self.push(SessionEventKind::Routed {
            response_id: response.clone(),
            decision: decision(target),
        });
        self.push(SessionEventKind::OutputTextDelta {
            response_id: response.clone(),
            text: "partial ".into(),
        });
        response
    }

    /// The client re-sends the same `turn_id`. `Session::begin_turn` mints a
    /// *new* `ResponseId` for an uncompleted turn (proven by
    /// `an_interrupted_turn_is_retryable_rather_than_deduplicated` in
    /// `session.rs`), so the successor can never retire the abandoned entry.
    fn completed_turn(&mut self, turn: &str, target: Target) -> ResponseId {
        let response = ResponseId::generate();
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new(turn),
            response_id: response.clone(),
        });
        self.push(SessionEventKind::Routed {
            response_id: response.clone(),
            decision: decision(target),
        });
        self.push(SessionEventKind::ResponseCompleted {
            response_id: response.clone(),
            usage: usage(4_000, 300),
        });
        response
    }

    /// A turn that failed but whose settle *did* land.
    fn incomplete_turn(&mut self, turn: &str, target: Target) -> ResponseId {
        let response = ResponseId::generate();
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new(turn),
            response_id: response.clone(),
        });
        self.push(SessionEventKind::Routed {
            response_id: response.clone(),
            decision: decision(target),
        });
        self.push(SessionEventKind::ResponseIncomplete {
            response_id: response.clone(),
            reason: IncompleteReason::UpstreamError,
            usage: usage(4_000, 12),
        });
        response
    }
}

/// The core claim. Every turn here reaches a settled outcome from the client's
/// point of view — each abandoned attempt is retried and the retry completes —
/// so once the log is fully folded there is no dispatch the fold is legitimately
/// still waiting on. Anything left in `pending` is unreclaimable.
#[test]
fn abandoned_dispatches_are_retired_when_their_turn_is_retried() {
    const ABANDONED: usize = 500;

    let mut log = Log::new("sess-leak");
    for i in 0..ABANDONED {
        let turn = format!("turn-{i}");
        // Attempt one: routed, then the owner is fenced. No terminal event.
        log.abandoned_turn(&turn, local());
        // Attempt two: the client retries the same turn id, gets a fresh
        // response id, and this one lands.
        log.completed_turn(&turn, local());
    }

    let mut fold = MetricsFold::new();
    fold.extend(&log.events);

    // Sanity: the retries were all accounted for, so this is not a fold that
    // simply failed to process the log.
    assert_eq!(
        fold.turns(),
        (ABANDONED * 2) as u64,
        "both attempts of every turn should have been counted as started turns"
    );

    assert_eq!(
        fold.pending_dispatches(),
        0,
        "every turn in this log reached a settled outcome, so the fold should be \
         holding no dispatch state; it is holding {} entries, one per abandoned \
         attempt, and nothing in the fold will ever remove them",
        fold.pending_dispatches()
    );
}

/// The persistence half of the claim: restarting the process does not clear the
/// leak, because a fresh fold replaying the same log rebuilds exactly the same
/// pending entries. `Session::open_observed` feeds replayed batches to the
/// metrics observer, so this is the real restart path and not a synthetic one.
#[test]
fn a_restart_reconstructs_the_same_retired_state() {
    const ABANDONED: usize = 50;

    let mut log = Log::new("sess-restart");
    for i in 0..ABANDONED {
        let turn = format!("turn-{i}");
        log.abandoned_turn(&turn, hosted());
        log.completed_turn(&turn, hosted());
    }

    let mut live = MetricsFold::new();
    live.extend(&log.events);
    let leaked_live = live.pending_dispatches();

    // A new process boots and replays the same log.
    let mut rebuilt = MetricsFold::new();
    rebuilt.extend(&log.events);

    assert_eq!(
        leaked_live,
        rebuilt.pending_dispatches(),
        "live and rebuilt folds must agree -- supersession is driven off log \
         contents, so a replay retires exactly what the live fold retired"
    );
    assert_eq!(
        rebuilt.pending_dispatches(),
        0,
        "a restarted process must not reconstruct abandoned dispatches; it held {}",
        rebuilt.pending_dispatches()
    );
}

/// Control: dispatches whose settle *did* land are retired, in both terminal
/// flavours, including the incomplete-with-no-billed-input case that is
/// deliberately not counted as a call. If this failed, the two tests above
/// would be measuring the wrong thing.
#[test]
fn settled_dispatches_are_retired_by_either_terminal_event() {
    let mut log = Log::new("sess-control");
    log.completed_turn("t1", local());
    log.incomplete_turn("t2", hosted());

    // An incomplete carrying no billed input: not counted as a call, but it is
    // still a terminal event and must still retire the pending entry.
    let response = ResponseId::generate();
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t3"),
        response_id: response.clone(),
    });
    log.push(SessionEventKind::Routed {
        response_id: response.clone(),
        decision: decision(local()),
    });
    log.push(SessionEventKind::ResponseIncomplete {
        response_id: response,
        reason: IncompleteReason::UpstreamError,
        usage: Usage::default(),
    });

    let mut fold = MetricsFold::new();
    fold.extend(&log.events);

    assert_eq!(
        fold.pending_dispatches(),
        0,
        "a terminal event of either kind retires the dispatch"
    );
}

/// What supersession does *not* cover, stated rather than glossed.
///
/// A turn abandoned and never retried — the client gave up, or the process died
/// and nobody re-sent — has no second `TurnStarted` to prove it was abandoned,
/// so its entry stays. That residue is bounded by abandoned-and-never-retried
/// turns rather than by all abandoned dispatches, which is the difference
/// between growth on every failover and growth only when a failover is also
/// given up on. Retiring it needs an event-time horizon (the engine's own turn
/// deadline is the natural one) and is deliberately not done here: a wall-clock
/// eviction would make the fold non-deterministic under replay, which is the
/// one property the whole projection rests on.
#[test]
fn a_turn_abandoned_and_never_retried_is_the_documented_residue() {
    let mut log = Log::new("sess-residue");
    log.abandoned_turn("turn-given-up-on", local());
    log.completed_turn("turn-that-finished", local());

    let mut fold = MetricsFold::new();
    fold.extend(&log.events);

    assert_eq!(
        fold.pending_dispatches(),
        1,
        "the abandoned-and-never-retried dispatch is still held"
    );
    assert_eq!(
        fold.open_turns(),
        1,
        "and its turn is still open, since nothing in the log says otherwise"
    );

    // The turn that did settle leaves nothing behind, so the residue really is
    // one entry per given-up-on turn and not one per turn.
    let mut settled_only = MetricsFold::new();
    let mut clean = Log::new("sess-clean");
    clean.completed_turn("turn-that-finished", local());
    settled_only.extend(&clean.events);
    assert_eq!(settled_only.pending_dispatches(), 0);
    assert_eq!(settled_only.open_turns(), 0);
}
