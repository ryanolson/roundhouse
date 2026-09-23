// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `Session` and `SessionState` under test.
//!
//! Split from `session.rs` when the tests outgrew the code they pin, on the
//! `control_config/config/tests.rs` precedent: a module split would be wrong
//! — the session and its state are one concept — but a thousand test lines
//! standing between a reader and the settle seam were not earning their
//! position.

use super::classification::landed_through;
use super::*;
use crate::control::Principal;
use crate::item::{ItemContent, Role};
use crate::routing::{Candidate, Target};
use crate::store::MemoryStore;
use async_trait::async_trait;
use std::sync::Mutex;

const TTL: u64 = 30_000;

/// The correction a steered turn answers with, as M10.0 renders it: the
/// directive, then the pending request quoted line by line.
///
/// A literal rather than a call into `render_steer_answer`, so a test about the
/// *fold* fails for the fold's reasons; the rendering has its own golden pin in
/// `validate::verdict`.
const GUIDANCE: &str = "A review of this session's recent steps found it is not making progress \
toward the stated task.\n\nThe request you are working on is restated below.\n\n> go";

async fn new_session(store: Arc<MemoryStore>, node: &str) -> (SessionId, Session<MemoryStore>) {
    let sid = SessionId::generate();
    store.create_session(&sid, "affinity").await.unwrap();
    let session = Session::open(store, sid.clone(), node, TTL, CacheLedger::new())
        .await
        .unwrap();
    (sid, session)
}

/// A hosted rate card, so a decision under test carries the price its
/// settle will be driven from.
fn card() -> ProviderPricing {
    ProviderPricing {
        input_per_mtok_usd: 3.0,
        cached_input_per_mtok_usd: 0.3,
        cache_write_per_mtok_usd: 3.75,
        output_per_mtok_usd: 15.0,
    }
}

/// A decision priced the way the engine prices one: a card for a hosted
/// target, none for a local worker, which bills capacity rather than
/// dollars. Budgeted, so the projection below has a basis to carry rather
/// than an absence that would be true whatever it did.
fn decision_for(target: Target, isl: u64) -> DecisionRecord {
    DecisionRecord {
        selection: None,
        local_quote_skipped: None,
        rate_card: (!target.is_local()).then(card),
        chosen: target,
        rationale: "test".into(),
        policy: "affinity".into(),
        isl_tokens: isl,
        expected_prefill_tokens: isl as f64,
        expected_cost_usd: 0.0,
        considered: Vec::<Candidate>::new(),
        turn_policy_digest: String::new(),
        budget_state: Default::default(),
        payer: Default::default(),
        billing: Default::default(),
        budget_draw: Some(BudgetCounts::AllFrontierSpend),
        withheld_providers: Vec::new(),
        declared_baseline: None,
        attempts: Vec::new(),
    }
}

#[tokio::test]
async fn record_created_commits_session_created_with_the_principal() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;
    let principal = Principal::new("acme", "ada");

    session
        .record_created("affinity", &principal, None)
        .await
        .unwrap();

    let events = session.events_since(0, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].seq, 1,
        "identity is the first fact in the log, so a replay learns it before any spend"
    );
    assert_eq!(
        events[0].kind,
        SessionEventKind::SessionCreated {
            arm: None,
            model_policy: "affinity".into(),
            principal: Some(principal),
        }
    );
}

/// **The recorded principal survives a restart, read off the log alone.**
///
/// A repair reads `SessionState::principal` rather than a live turn's
/// admission — see `Engine::repair_classification_settlements`. That is only a
/// correct source if the fold actually reconstructs it from the durable
/// `SessionCreated` event through a fresh replay, not merely within the
/// process that wrote it. This is the cheap companion to that claim: no
/// classification, no ledger, just the fold surviving a process boundary.
#[tokio::test]
async fn principal_survives_a_restart_from_the_durable_log() {
    let store = Arc::new(MemoryStore::new());
    let (sid, mut session) = new_session(store.clone(), "node-a").await;
    let principal = Principal::new("acme", "ada");
    session
        .record_created("affinity", &principal, None)
        .await
        .unwrap();
    drop(session);
    store.expire_lease_now(&sid).await;

    let successor = Session::open(store, sid, "node-b", TTL, CacheLedger::new())
        .await
        .unwrap();
    assert_eq!(
        successor.state().principal(),
        Some(&principal),
        "a fresh replay reconstructs the payer from `SessionCreated` alone"
    );
}

/// **A log with no recorded principal folds to `None`, not to a guess.**
///
/// `Session::record_created` always writes `Some`, so this state is
/// unreachable through it — but the type it writes is `Option<Principal>`,
/// and `Engine::repair_classification_settlements` explicitly branches on
/// finding `None` there: a log predating tenancy. This pins the fold arm
/// directly against a raw event a real deployment's own legacy log could
/// still hand a replay, without going through the engine to construct it.
#[tokio::test]
async fn a_session_created_with_no_principal_folds_to_none() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    session
        .commit(vec![SessionEventKind::SessionCreated {
            model_policy: "affinity".into(),
            principal: None,
            arm: None,
        }])
        .await
        .unwrap();

    assert_eq!(
        session.state().principal(),
        None,
        "the fold does not invent a payer for a log that never named one"
    );
}

#[tokio::test]
async fn a_turn_appends_items_and_advances_the_turn_index() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;
    assert_eq!(session.turn_index(), 0);

    let admission = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .unwrap();
    assert!(matches!(admission, TurnAdmission::Started(_)));
    assert_eq!(session.turn_index(), 1);
    assert_eq!(session.state().items.len(), 1);
}

#[tokio::test]
async fn replaying_a_completed_turn_id_does_not_generate_twice() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let first = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .unwrap();
    let response_id = first.response_id().clone();
    session
        .complete(&response_id, Some("hi there"), Usage::default(), None, None)
        .await
        .unwrap();

    let retry = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .unwrap();
    assert_eq!(retry, TurnAdmission::Deduplicated(response_id));
    // The retry must not have appended the user item a second time.
    assert_eq!(
        session
            .state()
            .items
            .iter()
            .filter(|item| item.render().contains("hello"))
            .count(),
        1
    );
}

#[tokio::test]
async fn an_interrupted_turn_is_retryable_rather_than_deduplicated() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let admission = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .unwrap();
    let response_id = admission.response_id().clone();
    session
        .mark_incomplete(
            &response_id,
            "partial",
            IncompleteReason::OwnerLost,
            Usage::default(),
            None,
        )
        .await
        .unwrap();

    // The turn never completed, so re-sending it must start fresh.
    let retry = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .unwrap();
    assert!(matches!(retry, TurnAdmission::Started(_)));
    assert_ne!(retry.response_id(), &response_id);
}

#[tokio::test]
async fn the_settlement_projection_names_the_last_terminal_event_and_where_it_went() {
    // The spend ledger's whole input, and the property that makes one
    // entry enough: a session's turns are serialized, so the only spend
    // that can still be unapplied when a successor opens the log is the
    // last one's.
    let store = Arc::new(MemoryStore::new());
    let (session_id, mut session) = new_session(Arc::clone(&store), "node-a").await;
    assert!(
        session.state().last_settlement().is_none(),
        "a session with no terminated response owes nobody anything"
    );

    let target = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    };
    let first = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .unwrap();
    let first_id = first.response_id().clone();
    session
        .record_routing(&first_id, decision_for(target.clone(), 100))
        .await
        .unwrap();
    let billed = Usage {
        input_tokens: 100,
        output_tokens: 20,
        ..Usage::default()
    };
    session
        .complete(&first_id, Some("hi"), billed.clone(), None, None)
        .await
        .unwrap();

    let settlement = session
        .state()
        .last_settlement()
        .expect("a completed response is a settlement")
        .clone();
    assert_eq!(settlement.response_id, first_id);
    assert_eq!(settlement.target, Some(target));
    assert_eq!(settlement.usage, billed);
    assert_eq!(
        settlement.rate_card,
        Some(card()),
        "the price travels with the target it prices: a settle that had to \
         look the card up somewhere else would be reading a file, and a \
         file is not the same thing twice"
    );
    assert_eq!(
        settlement.seq,
        session.last_seq(),
        "the seq is the terminal event's own, which is what makes the \
         settle idempotent across a replay that assigns the same numbers"
    );
    assert_eq!(
        settlement.budget_draw,
        Some(BudgetCounts::AllFrontierSpend),
        "and the basis the turn draws its project's budget on travels with \
         it too: read from the live plane instead, a settle would apply \
         whichever basis an admin had switched to since"
    );

    // A response that terminated without ever routing carries no target,
    // and that is what prices it at zero: it reached no provider.
    let second = session
        .begin_turn(TurnId::new("t2"), vec![Item::user_text("again")])
        .await
        .unwrap();
    let second_id = second.response_id().clone();
    session
        .mark_incomplete(
            &second_id,
            "",
            IncompleteReason::BudgetExhausted,
            Usage::default(),
            None,
        )
        .await
        .unwrap();
    let settlement = session
        .state()
        .last_settlement()
        .expect("a refused response terminates too")
        .clone();
    assert_eq!(settlement.response_id, second_id);
    assert_eq!(
        settlement.target, None,
        "a turn that routed nowhere owes nothing, and the absence is what \
         says so"
    );
    assert_eq!(
        settlement.rate_card, None,
        "and there is no card, because there was nothing to price -- which \
         is a different absence from a hosted turn whose card the log never \
         recorded"
    );
    assert_eq!(
        settlement.budget_draw, None,
        "nor any basis to draw a budget on, because no decision was \
         recorded to carry one -- which is why the settle that releases \
         this turn's hold draws zero rather than skipping"
    );

    // And a successor that replays this log arrives at the same answer,
    // which is the whole basis of the repair.
    session.release().await.unwrap();
    let successor = Session::open(store, session_id, "node-b", TTL, CacheLedger::new())
        .await
        .unwrap();
    assert_eq!(
        successor.state().last_settlement().cloned(),
        Some(settlement),
        "a replay has to reconstruct the settlement identically, or the \
         repair would charge a different number than the settle it replaces"
    );
}

/// The count the wire needs is the count at *dispatch*, and the log's order
/// within a turn is what makes those two different numbers.
///
/// `TurnStarted`, this turn's input items, `Routed`, then the output items,
/// then the terminal event. A ledger filled from `items.len()` at the terminal
/// fold would count the answer the provider produced as blocks the provider was
/// sent, and the next request's lookback marker would be placed past the end of
/// what was actually cached.
#[tokio::test]
async fn the_ledger_records_the_item_count_the_turn_was_dispatched_with() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let admission = session
        .begin_turn(
            TurnId::new("t1"),
            vec![Item::user_text("one"), Item::user_text("two")],
        )
        .await
        .unwrap();
    let response_id = admission.response_id().clone();
    let target = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    };
    session
        .record_routing(&response_id, decision_for(target.clone(), 8_192))
        .await
        .unwrap();
    let at_dispatch = session.state().items.len();
    assert_eq!(at_dispatch, 2, "the two input items and nothing else");

    session
        .complete(
            &response_id,
            Some("an answer"),
            Usage::default(),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        session.state().items.len(),
        3,
        "the answer is an item too, which is exactly the trap"
    );

    let state = session
        .ledger()
        .state_for(&target)
        .expect("a completed dispatch is ledger evidence");
    assert_eq!(
        state.last_segment_count, at_dispatch as u64,
        "the prompt the provider cached had {at_dispatch} blocks; recording \
         the terminal count instead would place the next request's lookback \
         marker past the end of that entry"
    );
}

/// The ledger is a projection of the log, so a successor that replays it has to
/// reach the same block count without being told.
///
/// This is what makes the new field derive rather than migrate: no event
/// changed, and a node that took over mid-session would otherwise place a
/// lookback marker the original node would not have.
#[tokio::test]
async fn a_replayed_log_reconstructs_the_same_last_segment_count() {
    let store = Arc::new(MemoryStore::new());
    let (sid, mut session) = new_session(store.clone(), "node-a").await;

    let admission = session
        .begin_turn(
            TurnId::new("t1"),
            vec![Item::user_text("one"), Item::user_text("two")],
        )
        .await
        .unwrap();
    let response_id = admission.response_id().clone();
    let target = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    };
    session
        .record_routing(&response_id, decision_for(target.clone(), 8_192))
        .await
        .unwrap();
    session
        .complete(
            &response_id,
            Some("an answer"),
            Usage::default(),
            None,
            None,
        )
        .await
        .unwrap();
    let recorded = session
        .ledger()
        .state_for(&target)
        .expect("a completed dispatch is ledger evidence");

    drop(session);
    store.expire_lease_now(&sid).await;
    let successor = Session::open(store, sid, "node-b", TTL, CacheLedger::new())
        .await
        .unwrap();

    assert_eq!(
        successor
            .ledger()
            .state_for(&target)
            .expect("the replay folds the same dispatch")
            .last_segment_count,
        recorded.last_segment_count
    );
}

#[tokio::test]
async fn a_dispatch_that_never_terminates_leaves_the_target_cold() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let admission = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .unwrap();
    let target = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    };
    session
        .record_routing(admission.response_id(), decision_for(target.clone(), 8_192))
        .await
        .unwrap();

    // The process died before the response terminated, so nothing is known
    // about what the provider saw. Claiming a warm prefix here would price
    // the retry against a cache that may not exist.
    assert!(session.ledger().state_for(&target).is_none());
}

#[tokio::test]
async fn an_incomplete_response_records_its_dispatch_at_the_terminal_event() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let admission = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .unwrap();
    let response_id = admission.response_id().clone();
    let target = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    };
    let checkpoint = session.last_seq();
    session
        .record_routing(&response_id, decision_for(target.clone(), 8_192))
        .await
        .unwrap();
    session
        .mark_incomplete(
            &response_id,
            "partial",
            IncompleteReason::UpstreamError,
            Usage {
                input_tokens: 8_192,
                cached_input_tokens: 0,
                output_tokens: 3,
                reasoning_tokens: 0,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();

    // Billed input is the proof the prompt was prefilled: the prefix is
    // warm even though the response never completed.
    let state = session
        .ledger()
        .state_for(&target)
        .expect("an incomplete with billed input is ledger evidence");
    assert_eq!(state.last_prefix_tokens, 8_192);

    let terminal = session
        .events_since(checkpoint, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|event| event.is_terminal())
        .expect("the response terminated");
    assert_eq!(
        state.last_call_at_ms, terminal.at_ms,
        "the TTL runs from when the provider stopped holding the prompt"
    );
}

#[tokio::test]
async fn an_incomplete_response_with_no_billed_input_leaves_the_target_cold() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let admission = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .unwrap();
    let response_id = admission.response_id().clone();
    let target = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    };
    session
        .record_routing(&response_id, decision_for(target.clone(), 8_192))
        .await
        .unwrap();
    // The engine terminates a dispatch that failed before anything was
    // sent with exactly this shape: incomplete, empty usage.
    session
        .mark_incomplete(
            &response_id,
            "",
            IncompleteReason::UpstreamError,
            Usage::default(),
            None,
        )
        .await
        .unwrap();

    // No billed input, no evidence the provider ever saw the prompt --
    // claiming a warm prefix here is precisely the phantom the ledger fold
    // must not produce.
    assert!(session.ledger().state_for(&target).is_none());
}

#[tokio::test]
async fn a_successor_node_reconstructs_identical_state_from_the_log() {
    let store = Arc::new(MemoryStore::new());
    let (sid, mut session) = new_session(store.clone(), "node-a").await;

    let admission = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("first question")])
        .await
        .unwrap();
    let response_id = admission.response_id().clone();
    let target = Target::Local {
        worker_id: 7,
        dp_rank: 0,
        model: "llama".into(),
    };
    session
        .record_routing(&response_id, decision_for(target.clone(), 4_096))
        .await
        .unwrap();
    session
        .append_output(&response_id, "part one ")
        .await
        .unwrap();
    session
        .complete(
            &response_id,
            Some("part one and two"),
            Usage::default(),
            None,
            None,
        )
        .await
        .unwrap();

    // Owner dies; a second node takes over.
    let seq_before = session.last_seq();
    let items_before = session.state().items.clone();
    drop(session);
    store.expire_lease_now(&sid).await;

    let successor = Session::open(store, sid, "node-b", TTL, CacheLedger::new())
        .await
        .unwrap();
    assert_eq!(successor.last_seq(), seq_before);
    assert_eq!(successor.state().items, items_before);
    assert_eq!(successor.turn_index(), 1);
    // The routing ledger is part of the projection, so the successor knows
    // worker 7 is warm without being told.
    assert!(successor.ledger().state_for(&target).is_some());
}

#[tokio::test]
async fn the_frontier_window_is_a_projection_a_successor_reconstructs() {
    let store = Arc::new(MemoryStore::new());
    let (sid, mut session) = new_session(store.clone(), "node-a").await;
    let hosted = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    };
    let own = Target::Local {
        worker_id: 7,
        dp_rank: 0,
        model: "llama".into(),
    };

    for (turn, target) in [(1, &hosted), (2, &own), (3, &hosted)] {
        let admission = session
            .begin_turn(TurnId::new(format!("t{turn}")), vec![Item::user_text("q")])
            .await
            .unwrap();
        let response_id = admission.response_id().clone();
        session
            .record_routing(&response_id, decision_for(target.clone(), 1_000))
            .await
            .unwrap();
        // Turn 3's dispatch dies before the provider ever answers. It has
        // still spent its ration: the window is folded from `Routed`.
        if turn != 3 {
            session
                .complete(&response_id, Some("a"), Usage::default(), None, None)
                .await
                .unwrap();
        }
    }

    let window = session.state().frontier_history.clone();
    assert_eq!(window.frontier_in_last(3), 2);
    assert_eq!(
        window.frontier_in_last(1),
        1,
        "the last routed turn was the abandoned frontier dispatch, and it counts"
    );
    assert_eq!(
        window.frontier_in_last(100),
        2,
        "a window longer than the session sees the whole session"
    );

    // Ownership moves. A cadence that a successor could not reconstruct
    // would reset every time a node died, which is exactly when a session
    // is retrying hardest.
    drop(session);
    store.expire_lease_now(&sid).await;
    let successor = Session::open(store, sid, "node-b", TTL, CacheLedger::new())
        .await
        .unwrap();
    assert_eq!(
        successor.state().frontier_history,
        window,
        "the window is derived from the log, so the successor derives the same one"
    );
}

#[tokio::test]
async fn a_reader_projects_a_session_without_taking_the_lease_the_writer_holds() {
    // The rule the MCP control surface rests on: a reader that took the lease
    // would evict the engine it is reporting on. `project` is that read, and it
    // is the *same* fold `open` runs — one replay loop, so a question answered
    // for a reader and the same question answered for the engine cannot come
    // back with two answers.
    let store = Arc::new(MemoryStore::new());
    let (sid, mut session) = new_session(Arc::clone(&store), "writer").await;
    session
        .record_created("affinity", &Principal::new("acme", "ada"), None)
        .await
        .unwrap();
    let started = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .unwrap();
    let response_id = started.response_id().clone();
    session
        .record_routing(&response_id, decision_for(local_target(), 100))
        .await
        .unwrap();
    // An emitted call, so the projection has a steer to report as open.
    session
        .complete_with_item(
            &response_id,
            steer_item(&response_id),
            Usage::default(),
            ControlRecord::default(),
        )
        .await
        .unwrap();

    let projected = SessionState::project(store.as_ref(), &sid, CacheLedger::new(), None)
        .await
        .expect("a projection needs no lease");
    assert_eq!(
        projected.items.last().map(|item| item.spoken_text()),
        Some(GUIDANCE)
    );
    assert_eq!(
        projected
            .last_decision()
            .expect("the turn was routed")
            .chosen,
        local_target(),
        "and the last decision survives its response terminating -- which is \
         the whole difference between this and `pending_routings`"
    );

    // The control that makes the claim about the *lease*: the writer still
    // holds it and can still write, which a reader that had taken it would
    // have made impossible.
    session
        .begin_turn(TurnId::new("t2"), vec![Item::user_text("again")])
        .await
        .expect("the reader must not have displaced the writer");

    // And the projection agrees with what the engine's own replay reconstructs.
    let reopened = Session::open(
        Arc::clone(&store),
        sid.clone(),
        "writer",
        TTL,
        CacheLedger::new(),
    )
    .await
    .unwrap();
    let reprojected = SessionState::project(store.as_ref(), &sid, CacheLedger::new(), None)
        .await
        .unwrap();
    assert_eq!(reopened.state().items.len(), reprojected.items.len());
    assert_eq!(
        reopened.state().last_guidance(),
        reprojected.last_guidance()
    );
    assert_eq!(
        reopened.state().last_decision().map(|d| d.chosen.clone()),
        reprojected.last_decision().map(|d| d.chosen.clone())
    );
}

/// The worker a routed test turn lands on.
fn local_target() -> Target {
    Target::Local {
        worker_id: 1,
        dp_rank: 0,
        model: "llama".into(),
    }
}

#[tokio::test]
async fn a_displaced_owner_cannot_keep_writing() {
    let store = Arc::new(MemoryStore::new());
    let (sid, mut displaced) = new_session(store.clone(), "node-a").await;

    store.expire_lease_now(&sid).await;
    let _successor = Session::open(store, sid, "node-b", TTL, CacheLedger::new())
        .await
        .unwrap();

    let result = displaced
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await;
    assert!(matches!(
        result,
        Err(SessionError::Store(StoreError::LeaseLost { .. }))
    ));
}

#[tokio::test]
async fn a_heartbeat_keeps_a_writer_alive_past_its_lease_ttl() {
    let store = Arc::new(MemoryStore::new());
    let sid = SessionId::generate();
    store.create_session(&sid, "affinity").await.unwrap();
    let mut session = Session::open(store, sid, "node-a", 200, CacheLedger::new())
        .await
        .unwrap();

    let _heartbeat = session.heartbeat(60, 200);
    // Longer than the TTL. Unrenewed, the append below is fenced and
    // whatever produced it is thrown away.
    tokio::time::sleep(Duration::from_millis(500)).await;

    session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .expect("a renewed lease is still the single writer");
}

#[tokio::test]
async fn a_heartbeat_stops_at_takeover_instead_of_stealing_the_session_back() {
    let store = Arc::new(MemoryStore::new());
    let sid = SessionId::generate();
    store.create_session(&sid, "affinity").await.unwrap();
    let mut displaced = Session::open(
        store.clone(),
        sid.clone(),
        "node-a",
        200,
        CacheLedger::new(),
    )
    .await
    .unwrap();
    let _heartbeat = displaced.heartbeat(60, 200);

    store.expire_lease_now(&sid).await;
    let mut successor = Session::open(store, sid, "node-b", TTL, CacheLedger::new())
        .await
        .unwrap();

    // Several renewal ticks. A heartbeat that treated a lost lease as
    // something to re-acquire would put two writers on one log here.
    tokio::time::sleep(Duration::from_millis(300)).await;

    successor
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("hello")])
        .await
        .expect("the successor is the owner and stays the owner");
    assert!(matches!(
        displaced
            .begin_turn(TurnId::new("t2"), vec![Item::user_text("hello")])
            .await,
        Err(SessionError::Store(StoreError::LeaseLost { .. }))
    ));
}

#[tokio::test]
async fn resumption_from_a_sequence_number_is_gapless() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let admission = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("q")])
        .await
        .unwrap();
    let response_id = admission.response_id().clone();
    let checkpoint = session.last_seq();

    for chunk in ["a", "b", "c"] {
        session.append_output(&response_id, chunk).await.unwrap();
    }

    let replayed = session.events_since(checkpoint, 100).await.unwrap();
    assert_eq!(replayed.len(), 3);
    assert_eq!(
        replayed.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![checkpoint + 1, checkpoint + 2, checkpoint + 3]
    );

    // Projecting to one response drops the session-level events.
    let scoped = session.response_events(&response_id, 0, 100).await.unwrap();
    assert!(scoped.iter().all(|e| e.response_id() == Some(&response_id)));
}

// -----------------------------------------------------------------------
// The steered turn: a turn that completes carrying an emitted tool call.
// -----------------------------------------------------------------------

/// A store that remembers the shape of every append batch.
///
/// Contiguous sequence numbers are *not* evidence of atomicity — two
/// separate appends produce contiguous seqs too — so this double exists to
/// make the one assertion seq inspection cannot: that the item and the
/// completion reached the log in a single call, leaving no window for a
/// crash to land between a decision and its realization.
struct BatchRecordingStore {
    inner: MemoryStore,
    batches: Mutex<Vec<Vec<SessionEventKind>>>,
}

impl BatchRecordingStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            batches: Mutex::new(Vec::new()),
        }
    }

    /// Drop the record so far, so an assertion is about the batches the
    /// method under test produced rather than about the setup's.
    fn forget_batches(&self) {
        self.batches.lock().expect("batch record poisoned").clear();
    }

    fn batches(&self) -> Vec<Vec<SessionEventKind>> {
        self.batches.lock().expect("batch record poisoned").clone()
    }
}

// `Delegating`, not `use`d in this module: `store.create_session(..)` below
// calls through `SessionStore` on the concrete double, and importing
// `Delegating` alongside it would make that call ambiguous between the two
// traits' same-named methods (E0034). Fully qualifying the trait here avoids
// that without needing every call site in this file to disambiguate instead.
#[async_trait]
impl crate::store::doubles::Delegating for BatchRecordingStore {
    type Backend = MemoryStore;

    fn backend(&self) -> &MemoryStore {
        &self.inner
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
        mark: Option<crate::store::LearningMark>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let appended = self.inner.append_events(lease, kinds, mark).await?;
        // Recorded only on success: a rejected append wrote nothing, and
        // counting it would let a fenced writer look like a second batch.
        self.batches
            .lock()
            .expect("batch record poisoned")
            .push(appended.iter().map(|event| event.kind.clone()).collect());
        Ok(appended)
    }
}

/// The item a steered turn completes with, built the way the seam builds it:
/// assistant text, with the response stamp applied by `complete_with_item`
/// rather than by the caller.
fn steer_item(response_id: &ResponseId) -> Item {
    Item::assistant_text(GUIDANCE, response_id.clone())
}

#[tokio::test]
async fn complete_with_item_appends_the_item_and_completes_in_one_batch() {
    let store = Arc::new(BatchRecordingStore::new());
    let session_id = SessionId::generate();
    store.create_session(&session_id, "affinity").await.unwrap();
    let mut session = Session::open(
        Arc::clone(&store),
        session_id,
        "node-a",
        TTL,
        CacheLedger::new(),
    )
    .await
    .unwrap();

    let admission = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("q")])
        .await
        .unwrap();
    let response_id = admission.response_id().clone();
    store.forget_batches();

    session
        .complete_with_item(
            &response_id,
            steer_item(&response_id),
            Usage::default(),
            ControlRecord::default(),
        )
        .await
        .unwrap();

    let batches = store.batches();
    assert_eq!(
        batches.len(),
        1,
        "the emitted item and the completion are a decision and its \
         realization: committed in two appends, a crash between them \
         leaves a session holding a correction whose turn never completed, \
         so the client's retry does not deduplicate and the same guidance is \
         answered twice"
    );
    assert!(
        matches!(
            batches[0].as_slice(),
            [
                SessionEventKind::ItemAppended { .. },
                SessionEventKind::ResponseCompleted { .. }
            ]
        ),
        "the batch is the item then the completion, in that order: {:?}",
        batches[0]
    );

    // And the store's own numbering makes the pair contiguous, which is
    // what a replay reads them back as.
    let events = session.events_since(0, 100).await.unwrap();
    let item_seq = events
        .iter()
        .find(|event| {
            matches!(&event.kind, SessionEventKind::ItemAppended { item }
                if item.spoken_text() == GUIDANCE)
        })
        .expect("the emitted item is in the log")
        .seq;
    let completed_seq = events
        .iter()
        .find(|event| matches!(event.kind, SessionEventKind::ResponseCompleted { .. }))
        .expect("the response completed")
        .seq;
    assert_eq!(completed_seq, item_seq + 1);
}

/// The provenance stamp, and the reason M10.0 could not keep using it.
///
/// The stamp is still the marker that says which items this deployment
/// produced, and `complete_with_item` is still the only thing that applies one.
/// What changed is that the stamp no longer *identifies a steer*: a steered turn
/// completes with assistant text now, which is byte-for-byte the shape every
/// dispatched turn completes with, so "stamped" answers "we wrote it" and
/// nothing finer. The control below is the whole of that argument — the two
/// items are indistinguishable — and it is why `steered_on_turn` is folded from
/// `ValidationDecided` instead.
#[tokio::test]
async fn a_completed_item_carries_the_response_stamp_and_a_steer_looks_like_any_answer() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let admission = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("q")])
        .await
        .unwrap();
    let response_id = admission.response_id().clone();
    session
        .complete_with_item(
            &response_id,
            steer_item(&response_id),
            Usage::default(),
            ControlRecord::default(),
        )
        .await
        .unwrap();

    let emitted = session
        .state()
        .items
        .last()
        .expect("the item was committed")
        .clone();
    assert_eq!(
        emitted.response_id,
        Some(response_id.clone()),
        "the stamp is applied here and nowhere else: every item on the input \
         path carries none, so a stamped item is one this deployment wrote"
    );
    assert_eq!(emitted.role, Role::Assistant);
    assert_eq!(
        emitted.content,
        ItemContent::Text {
            text: GUIDANCE.into()
        },
        "the correction is the turn's answer, in the conversation, where the \
         agent reads every other answer"
    );

    // The control, and M10.0's discriminator problem as an assertion: an
    // ordinary dispatched answer produces an item that differs from the steer
    // in its *text* and in nothing else. Anything in the fold that tried to
    // recognise a steer by shape or by provenance would recognise this too.
    let admission = session
        .begin_turn(TurnId::new("t2"), vec![Item::user_text("q2")])
        .await
        .unwrap();
    let ordinary_id = admission.response_id().clone();
    session
        .complete(
            &ordinary_id,
            Some("an ordinary answer"),
            Usage::default(),
            None,
            None,
        )
        .await
        .unwrap();
    let ordinary = session.state().items.last().expect("committed").clone();
    assert_eq!(ordinary.role, emitted.role);
    assert!(ordinary.response_id.is_some());
    assert!(
        matches!(
            (&ordinary.content, &emitted.content),
            (ItemContent::Text { .. }, ItemContent::Text { .. })
        ),
        "same role, same stamp, same content variant: there is nothing here \
         to tell a steer from an answer, which is why the fold does not try"
    );
}

#[tokio::test]
async fn complete_with_item_registers_the_turn_for_dedup_like_complete_does() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let admission = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("q")])
        .await
        .unwrap();
    let response_id = admission.response_id().clone();
    let billed = Usage {
        input_tokens: 40,
        output_tokens: 7,
        ..Usage::default()
    };
    session
        .complete_with_item(
            &response_id,
            steer_item(&response_id),
            billed.clone(),
            ControlRecord::default(),
        )
        .await
        .unwrap();

    let retry = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("q")])
        .await
        .unwrap();
    assert_eq!(
        retry,
        TurnAdmission::Deduplicated(response_id),
        "a steered turn completes, so its retry replays -- an incomplete \
         one would re-enter the interjection on every retry and never settle"
    );
    assert_eq!(
        session.state().completed_usage_for(&TurnId::new("t1")),
        Some(&billed),
        "the retry is answered with the accounting the interjection \
         supplied, the same as any completed turn's"
    );
    assert_eq!(
        session
            .state()
            .items
            .iter()
            .filter(|item| item.spoken_text() == GUIDANCE)
            .count(),
        1,
        "and the correction is committed once, not once per retry"
    );
}

// ---------------------------------------------------------------------------
// The validate loop's projections, folded from a real log
// ---------------------------------------------------------------------------

/// One ordinary dispatched turn, start to finish, billing `tokens`.
///
/// Every step the engine takes for a turn that reaches a provider, including
/// the `Routed` — which is what separates a conversation turn from a completing
/// interjection in every projection that pairs a dispatch with its terminal
/// event. Returns what the turn billed, so a caller asserts against the number
/// it asked for rather than restating it.
async fn routed_turn(session: &mut Session<MemoryStore>, turn: &str, tokens: u64) -> u64 {
    let admitted = session
        .begin_turn(TurnId::new(turn), vec![Item::user_text("go")])
        .await
        .unwrap();
    let response_id = admitted.response_id().clone();
    session
        .record_routing(
            &response_id,
            decision_for(
                Target::Local {
                    worker_id: 7,
                    dp_rank: 0,
                    model: "llama".into(),
                },
                tokens,
            ),
        )
        .await
        .unwrap();
    let usage = Usage {
        input_tokens: tokens,
        ..Usage::default()
    };
    let billed = usage.total();
    session
        .complete(&response_id, Some("done"), usage, None, None)
        .await
        .unwrap();
    billed
}

/// A judged validation that acted on nothing — the cheapest outcome that still
/// reached a judge, so a test about *being charged* is not also a test about
/// what the action did.
fn judged_continue() -> crate::event::ValidationOutcome {
    crate::event::ValidationOutcome::Judged {
        side_call_id: crate::ids::SideCallId::new("sc_judged"),
        verdict: crate::validate::Verdict {
            on_track: true,
            confidence: 0.9,
            divergence: None,
            missing_context: None,
        },
        action: SteerAction::Continue,
        interval: None,
    }
}

/// Everything the trigger reads, driven through a session rather than set by
/// hand.
///
/// The trigger's own tests fabricate a [`SessionState`] on purpose — a test
/// about the gate should fail for the gate's reasons — but that leaves the fold
/// that *produces* those fields untested, which is where the whole design's one
/// rule lives: every one of them is a projection of the log and never a counter
/// kept beside it. This is that fold's test.
#[tokio::test]
async fn the_trigger_reads_projections_of_the_log_and_not_counters_beside_it() {
    let store = Arc::new(MemoryStore::new());
    let (sid, mut session) = new_session(Arc::clone(&store), "node-a").await;
    session
        .record_created("affinity", &Principal::new("acme", "ada"), Some(Arm::Live))
        .await
        .unwrap();
    assert_eq!(session.state().arm(), Some(Arm::Live));

    // Two ordinary turns, so there is spend to measure and a trailing
    // distribution to compare against. Dispatched, `Routed` and all: what these
    // two projections measure is conversation spend, and a turn with no routing
    // is a turn that reached no provider.
    for (n, tokens) in [(1u64, 1_000u64), (2, 3_000)] {
        routed_turn(&mut session, &format!("t{n}"), tokens).await;
    }
    assert_eq!(session.state().tokens_since_last_validation(), 4_000);
    assert_eq!(session.state().recent_turn_tokens(), &[1_000, 3_000]);
    assert_eq!(session.state().validations_run(), 0);
    assert_eq!(session.state().last_validation_at_ms(), None);
    assert_eq!(session.state().consecutive_interventions(), 0);
    assert_eq!(session.state().active_escalation(), None);

    // A third turn, this one escalated: the decision is committed before
    // dispatch, exactly as the interjection seam commits it.
    let admitted = session
        .begin_turn(TurnId::new("t3"), vec![Item::user_text("still going")])
        .await
        .unwrap();
    let response_id = admitted.response_id().clone();
    let mut record = ControlRecord::default();
    record.side_call_completed(
        crate::ids::SideCallId::new("sc_1"),
        Target::Frontier {
            provider: "anthropic".into(),
            model: "claude".into(),
        },
        Usage {
            input_tokens: 4_000,
            output_tokens: 40,
            ..Usage::default()
        },
    );
    record.validation_decided(
        crate::ids::ValidationId::new("val_1"),
        crate::validate::TriggerRecord::new(3, 4_000, Vec::new()),
        Arm::Live,
        crate::event::ValidationOutcome::Judged {
            side_call_id: crate::ids::SideCallId::new("sc_1"),
            verdict: crate::validate::Verdict {
                on_track: false,
                confidence: 0.8,
                divergence: Some(crate::validate::Divergence {
                    at_step: 1,
                    description: "never opened the failing import".into(),
                }),
                missing_context: None,
            },
            action: SteerAction::Escalate {
                turns: 2,
                overrides: EscalationOverrides { min_quality: 0.9 },
            },
            interval: None,
        },
    );
    let ledger_before = format!("{:?}", session.ledger());
    session.record_control(record).await.unwrap();

    assert_eq!(session.state().validations_run(), 1);
    assert!(session.state().last_validation_at_ms().is_some());
    assert_eq!(
        session.state().tokens_since_last_validation(),
        0,
        "the gate's budget resets at the decision, not at the next turn"
    );
    assert_eq!(
        session.state().active_escalation(),
        Some(EscalationOverrides { min_quality: 0.9 }),
        "the narrowing outlives the turn that decided it, and it does so as a \
         fold of the log rather than as a value handed across the seam"
    );
    assert_eq!(
        format!("{:?}", session.ledger()),
        ledger_before,
        "and a side call reaches the cache ledger not at all: a judge prompt is \
         not a prefix of the conversation, and warming that target would \
         mis-price the next real turn"
    );

    // The turn then terminates. Intervening turns are counted at the terminal
    // event, which is the one place every turn passes through exactly once.
    session
        .record_routing(
            &response_id,
            decision_for(
                Target::Local {
                    worker_id: 7,
                    dp_rank: 0,
                    model: "llama".into(),
                },
                100,
            ),
        )
        .await
        .unwrap();
    session
        .complete(
            &response_id,
            Some("done"),
            Usage {
                input_tokens: 100,
                ..Usage::default()
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(session.state().consecutive_interventions(), 1);
    assert_eq!(
        session.state().active_escalation(),
        Some(EscalationOverrides { min_quality: 0.9 }),
        "two turns were asked for, and one has been served"
    );
    assert_eq!(session.state().tokens_since_last_validation(), 100);

    // A fourth turn, uninterrupted: the count resets and the escalation runs
    // out. A count that only ever grew would disable validation for the rest of
    // any long session that was interrupted twice.
    routed_turn(&mut session, "t4", 0).await;
    assert_eq!(session.state().consecutive_interventions(), 0);
    assert_eq!(session.state().active_escalation(), None);

    // And the whole projection is reproduced by a replay, which is the property
    // every one of these fields exists in the fold to have.
    let replayed = SessionState::project(store.as_ref(), &sid, CacheLedger::new(), None)
        .await
        .unwrap();
    assert_eq!(replayed.arm(), Some(Arm::Live));
    assert_eq!(replayed.validations_run(), 1);
    assert_eq!(replayed.consecutive_interventions(), 0);
    assert_eq!(replayed.active_escalation(), None);
    assert_eq!(
        replayed.recent_turn_tokens(),
        session.state().recent_turn_tokens()
    );
    assert_eq!(
        replayed.tokens_since_last_validation(),
        session.state().tokens_since_last_validation()
    );
    assert_eq!(
        replayed.last_event_at_ms(),
        session.state().last_event_at_ms()
    );
}

/// The placebo arm's synthetic halt is not a correction, and must not be
/// re-readable as one.
///
/// **A defect M10.0 introduced and this test is what found it.** The placebo arm
/// interrupts without consulting the judge, and the fold represents that as
/// `SteerAction::Halt { reason: String::new() }` — a marker, deliberately empty,
/// because there is no judge and therefore no directive. Before M10.0 nothing
/// read a halt's reason out of the fold, so the empty string went nowhere. The
/// text steer gave it somewhere to go: `last_guidance` is what `fetch_steer`
/// serves, and folding the marker into it makes the tool answer
/// `{"guidance": ""}` — which is precisely the payload
/// [`SurfaceError::NoGuidanceYet`] exists to refuse, because an agent reads an
/// empty correction as "there was nothing to correct".
///
/// So the fold takes guidance from text that was actually written. The control
/// below is what keeps that from being a special case for the empty string
/// alone: a real halt's guidance *is* re-readable, because a halt does put its
/// reason in the conversation.
#[tokio::test]
async fn a_placebo_intervention_leaves_no_guidance_to_re_read() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let admitted = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("go")])
        .await
        .unwrap();
    let response_id = admitted.response_id().clone();
    let mut record = ControlRecord::default();
    record.validation_decided(
        crate::ids::ValidationId::generate(),
        crate::validate::TriggerRecord::new(session.state().turn_index, 4_000, Vec::new()),
        Arm::Placebo,
        crate::event::ValidationOutcome::NotRun {
            reason: crate::event::NotRunReason::PlaceboArm {
                timing: crate::event::PlaceboTiming::Intervened,
            },
        },
    );
    session
        .complete_with_item(
            &response_id,
            Item::assistant_text("", response_id.clone()),
            Usage::default(),
            record,
        )
        .await
        .unwrap();

    // The premise: the placebo really did act, so this is not a test of an arm
    // that decided nothing.
    assert_eq!(
        session.state().consecutive_interventions(),
        1,
        "the premise: the placebo arm acts, which is the whole of what it is \
         for -- so this is not a test of an arm that decided nothing"
    );
    assert_eq!(
        session.state().last_guidance(),
        None,
        "an empty marker is not a correction: serving it to `fetch_steer` would \
         hand an agent `{{\"guidance\": \"\"}}`, which reads as `there was \
         nothing to correct` -- the one thing a steer must never be mistaken for"
    );
    assert_eq!(
        session.state().steered_on_turn,
        None,
        "and it suppresses no validation: a placebo halt restates no task, so \
         no turn is coming that fulfils it"
    );

    // The control, and the reason the assertion above is about *emptiness* and
    // not about halts: a real halt writes its reason into the conversation, and
    // that reason is re-readable exactly as a steer's directive is.
    let admitted = session
        .begin_turn(TurnId::new("t2"), vec![Item::user_text("again")])
        .await
        .unwrap();
    let response_id = admitted.response_id().clone();
    let mut record = ControlRecord::default();
    record.validation_decided(
        crate::ids::ValidationId::generate(),
        crate::validate::TriggerRecord::new(session.state().turn_index, 4_000, Vec::new()),
        Arm::Live,
        crate::event::ValidationOutcome::Judged {
            side_call_id: crate::ids::SideCallId::new("sc_halt"),
            verdict: crate::validate::Verdict {
                on_track: false,
                confidence: 0.8,
                divergence: Some(crate::validate::Divergence {
                    at_step: 1,
                    description: "the judge's prose, which never travels".into(),
                }),
                missing_context: None,
            },
            action: SteerAction::Halt {
                reason: "stopping here; the last four steps repeated".to_string(),
            },
            interval: None,
        },
    );
    session
        .complete_with_item(
            &response_id,
            Item::assistant_text(
                "stopping here; the last four steps repeated",
                response_id.clone(),
            ),
            Usage::default(),
            record,
        )
        .await
        .unwrap();
    assert_eq!(
        session.state().last_guidance(),
        Some("stopping here; the last four steps repeated"),
    );
}

/// Commit a steered turn: the decision, the guidance item, and the completion,
/// exactly as `Validator::decide` and `complete_with_item` produce them.
///
/// A helper rather than three copies, because the *ordering* inside the batch is
/// what the fold depends on — the `ValidationDecided` is what says a steer
/// happened, and it has to be committed on the turn it interrupts.
async fn steered_turn(session: &mut Session<MemoryStore>, turn: &str, directive: &str) {
    let admitted = session
        .begin_turn(TurnId::new(turn), vec![Item::user_text("go")])
        .await
        .unwrap();
    let response_id = admitted.response_id().clone();
    let mut record = ControlRecord::default();
    record.validation_decided(
        crate::ids::ValidationId::generate(),
        crate::validate::TriggerRecord::new(session.state().turn_index, 4_000, Vec::new()),
        Arm::Live,
        crate::event::ValidationOutcome::Judged {
            side_call_id: crate::ids::SideCallId::new("sc_steer"),
            verdict: crate::validate::Verdict {
                on_track: false,
                confidence: 0.8,
                divergence: Some(crate::validate::Divergence {
                    at_step: 1,
                    description: "never opened the failing import".into(),
                }),
                missing_context: None,
            },
            action: SteerAction::Steer {
                directive: directive.to_string(),
            },
            interval: None,
        },
    );
    session
        .complete_with_item(
            &response_id,
            Item::assistant_text(GUIDANCE, response_id.clone()),
            Usage::default(),
            record,
        )
        .await
        .unwrap();
}

/// **R2/T6.** The first turn under an escalation is a fact the fold answers,
/// and only the first turn.
///
/// The gate the handoff note rides. It has to come off the log rather than off a
/// flag the engine keeps, and the reason is the failure it would otherwise have:
/// a successor picking a session up mid-escalation — after a failover, a lease
/// takeover, or simply a second process serving the next turn — would decorate
/// again, and the client's own history would then carry two copies of a note
/// that narrates one switch. That is the accumulation Switchyard's statelessness
/// rule exists to prevent, arriving through the back door.
///
/// Three positions, and each one is a different way to get the arithmetic wrong:
///
/// - the turn the escalation was decided on says **yes**. The seam runs after
///   `TurnStarted`, so this is also the first turn the narrowing reaches `plan`
///   on — a gate keyed on the *next* turn would decorate a request served under
///   an escalation the model was never told about, one turn late;
/// - the turn after it says **no**, while `active_escalation` still says the
///   narrowing is in force. That pair is the whole point: "escalated" and "just
///   escalated" are different questions, and a gate that asked the first would
///   decorate every turn of the escalation's life;
/// - a replay of the same log answers identically, which is what makes the
///   successor safe.
#[tokio::test]
async fn only_the_turn_an_escalation_was_decided_on_reads_as_its_first() {
    let store = Arc::new(MemoryStore::new());
    let (sid, mut session) = new_session(Arc::clone(&store), "node-a").await;

    // An ordinary turn first, so "no escalation at all" is distinguishable from
    // "an escalation that began earlier" — both answer `false`, and a test that
    // never saw the first would not know which one it was reading.
    routed_turn(&mut session, "t1", 1_000).await;
    assert!(!session.state().this_turn_opened_an_escalation());
    assert_eq!(session.state().active_escalation(), None);

    // The escalated turn: decided mid-turn, applied to this same turn.
    let admitted = session
        .begin_turn(TurnId::new("t2"), vec![Item::user_text("still going")])
        .await
        .unwrap();
    let response_id = admitted.response_id().clone();
    let mut record = ControlRecord::default();
    record.validation_decided(
        crate::ids::ValidationId::new("val_1"),
        crate::validate::TriggerRecord::new(session.state().turn_index, 4_000, Vec::new()),
        Arm::Live,
        crate::event::ValidationOutcome::Judged {
            side_call_id: crate::ids::SideCallId::new("sc_1"),
            verdict: crate::validate::Verdict {
                on_track: false,
                confidence: 0.8,
                divergence: Some(crate::validate::Divergence {
                    at_step: 1,
                    description: "never opened the failing import".into(),
                }),
                missing_context: None,
            },
            action: SteerAction::Escalate {
                turns: 3,
                overrides: EscalationOverrides { min_quality: 0.9 },
            },
            interval: None,
        },
    );
    session.record_control(record).await.unwrap();
    assert!(
        session.state().this_turn_opened_an_escalation(),
        "the turn the decision was committed on is the turn it first applies \
         to, and the only one a switch can be narrated on"
    );

    session
        .record_routing(&response_id, decision_for(local_target(), 100))
        .await
        .unwrap();
    session
        .complete(
            &response_id,
            Some("done"),
            Usage {
                input_tokens: 100,
                ..Usage::default()
            },
            None,
            None,
        )
        .await
        .unwrap();

    // The next turn: still escalated, and no longer the first.
    session
        .begin_turn(TurnId::new("t3"), vec![Item::user_text("carrying on")])
        .await
        .unwrap();
    assert_eq!(
        session.state().active_escalation(),
        Some(EscalationOverrides { min_quality: 0.9 }),
        "three turns were asked for and one has been served, so the narrowing \
         is still in force — which is what makes the assertion below about the \
         gate rather than about the escalation lapsing"
    );
    assert!(
        !session.state().this_turn_opened_an_escalation(),
        "a second turn under one escalation opened nothing; narrating it again \
         would put two copies of one switch's note in the client's history"
    );

    // And a successor rebuilding the same log agrees, mid-escalation and
    // mid-turn — the property the gate exists in the fold to have.
    let replayed = SessionState::project(store.as_ref(), &sid, CacheLedger::new(), None)
        .await
        .unwrap();
    assert_eq!(
        replayed.active_escalation(),
        session.state().active_escalation()
    );
    assert_eq!(
        replayed.this_turn_opened_an_escalation(),
        session.state().this_turn_opened_an_escalation(),
        "a process that took this session over must decorate exactly the turns \
         this one would have, and no others"
    );
}

/// **T3.** The turn *after* a steer is the one the hysteresis suppresses, and
/// exactly one turn is suppressed.
///
/// The off-by-one is the whole test and it is where the M10.0 pivot could
/// silently go wrong in either direction. Under the tool-call steer the
/// correction arrived as a *result*, on the fulfilling turn's own input, so the
/// question was about `turn_index`. Under the text steer the correction is the
/// *previous* turn's answer, so the question is about `turn_index - 1` — and a
/// fold that kept comparing against `turn_index` would suppress the turn that
/// emitted the steer (already past the gate, so no symptom) and judge the turn
/// that acts on it, which is the exact re-trigger loop the rule exists to stop.
///
/// The turn-after-next assertion is the other half: a rule written as "a steer
/// ever happened" passes the first two assertions and disables validation for
/// the rest of the session.
#[tokio::test]
async fn the_turn_after_a_steer_is_the_one_that_fulfils_it_and_only_that_turn() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    steered_turn(&mut session, "t1", "narrow the search").await;
    assert!(
        !session.state().this_turn_fulfils_a_steer(),
        "the turn that *emits* the correction is not the turn that acts on it; \
         the agent has not seen it yet"
    );
    assert_eq!(
        session.state().last_guidance(),
        Some("narrow the search"),
        "and the correction is re-readable off the fold, which is what lets \
         `fetch_steer` be a pure read of the session log"
    );

    // The next turn: the agent has read the guidance in its own conversation
    // and is acting on it. Judging it would re-trigger the validation that
    // produced it, before compliance is observable.
    session
        .begin_turn(TurnId::new("t2"), vec![Item::user_text("re-reading now")])
        .await
        .unwrap();
    assert!(
        session.state().this_turn_fulfils_a_steer(),
        "the turn that answers a correction looks, to every signal, exactly \
         like the turn that provoked it; without this the steer re-triggers \
         the validation that emitted it, forever"
    );
    let response_id = session
        .state()
        .open_turns
        .values()
        .next()
        .expect("t2 is open")
        .clone();
    session
        .complete(&response_id, Some("done"), Usage::default(), None, None)
        .await
        .unwrap();

    // The control, and the assertion that a permanent-suppression bug fails:
    // one turn later the gate is open again.
    session
        .begin_turn(TurnId::new("t3"), vec![Item::user_text("carry on")])
        .await
        .unwrap();
    assert!(
        !session.state().this_turn_fulfils_a_steer(),
        "a steer fulfilled two turns ago must not disable validation for the \
         rest of the session"
    );
}

/// A halt is not a steer, and the fold must not treat it as one.
///
/// A halt restates nothing and ends the agent's loop, so there is no turn coming
/// that fulfils it — the next turn is one a *human* started, and suppressing its
/// validation would suppress a turn nobody corrected. The guidance is still
/// re-readable, which is the difference between "not a steer" and "not
/// recorded".
#[tokio::test]
async fn a_halt_leaves_the_next_turn_judgeable() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let admitted = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("go")])
        .await
        .unwrap();
    let response_id = admitted.response_id().clone();
    let mut record = ControlRecord::default();
    record.validation_decided(
        crate::ids::ValidationId::generate(),
        crate::validate::TriggerRecord::new(1, 4_000, Vec::new()),
        Arm::Live,
        crate::event::ValidationOutcome::Judged {
            side_call_id: crate::ids::SideCallId::new("sc_halt"),
            verdict: crate::validate::Verdict {
                on_track: false,
                confidence: 0.8,
                divergence: Some(crate::validate::Divergence {
                    at_step: 1,
                    description: "drifted".into(),
                }),
                missing_context: None,
            },
            action: SteerAction::Halt {
                reason: "stopping here".into(),
            },
            interval: None,
        },
    );
    session
        .complete_with_item(
            &response_id,
            Item::assistant_text("stopping here", response_id.clone()),
            Usage::default(),
            record,
        )
        .await
        .unwrap();

    session
        .begin_turn(TurnId::new("t2"), vec![Item::user_text("here is more")])
        .await
        .unwrap();
    assert!(
        !session.state().this_turn_fulfils_a_steer(),
        "a halt invites nothing, so no turn fulfils it"
    );
    assert_eq!(
        session.state().last_guidance(),
        Some("stopping here"),
        "control: the halt *is* recorded as the last correction, so the \
         assertion above is about the hysteresis and not about a fold that \
         stopped looking at halts"
    );
}

/// A Shadow arm's action is computed and discarded, so it must not suppress the
/// next turn either.
///
/// The observe-only arm is the control the whole experiment leans on. An arm
/// that suppressed its own next observation would quietly destroy it — the same
/// argument `turn_intervened` is guarded by, applied to the projection M10.0
/// added.
#[tokio::test]
async fn a_shadow_arms_steer_suppresses_nothing() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    let admitted = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("go")])
        .await
        .unwrap();
    let response_id = admitted.response_id().clone();
    let mut record = ControlRecord::default();
    record.validation_decided(
        crate::ids::ValidationId::generate(),
        crate::validate::TriggerRecord::new(1, 4_000, Vec::new()),
        Arm::Shadow,
        crate::event::ValidationOutcome::Judged {
            side_call_id: crate::ids::SideCallId::new("sc_shadow"),
            verdict: crate::validate::Verdict {
                on_track: false,
                confidence: 0.8,
                divergence: Some(crate::validate::Divergence {
                    at_step: 1,
                    description: "drifted".into(),
                }),
                missing_context: None,
            },
            action: SteerAction::Steer {
                directive: "would have said this".into(),
            },
            interval: None,
        },
    );
    session.record_control(record).await.unwrap();
    session
        .complete(
            &response_id,
            Some("the turn ran unchanged"),
            Usage::default(),
            None,
            None,
        )
        .await
        .unwrap();

    session
        .begin_turn(TurnId::new("t2"), vec![Item::user_text("carry on")])
        .await
        .unwrap();
    assert!(
        !session.state().this_turn_fulfils_a_steer(),
        "the Shadow arm did nothing, so there is nothing for the next turn to \
         be fulfilling -- an arm that suppressed its own next observation \
         would destroy the control it exists to be"
    );
    assert_eq!(
        session.state().last_guidance(),
        None,
        "and nothing it would have said is readable back: a Shadow run that \
         served its guidance through `fetch_steer` would be a live arm"
    );
}

/// A judge outage must not spend the session's lifetime allowance.
///
/// `validations_run` is documented as "validations this session has bought",
/// and the trigger closes its gate for good once it reaches
/// `max_validations_per_session`. A validation that reached no judge bought
/// nothing: it produced no verdict, took no action, and cost no side call. What
/// it legitimately spends is the *cooldown* — that is what stops a failing
/// judge being re-dialled on every turn — and nothing else.
#[tokio::test]
async fn a_validation_that_reached_no_judge_spends_the_cooldown_and_nothing_else() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;
    session
        .record_created("affinity", &Principal::new("acme", "ada"), Some(Arm::Live))
        .await
        .unwrap();
    let evidence = routed_turn(&mut session, "t1", 5_000).await;
    assert_eq!(session.state().tokens_since_last_validation(), evidence);

    // The judge could not be dialled at all — the exact shape `Validator`
    // records when a consult finds no admissible judge.
    let mut record = ControlRecord::default();
    record.validation_decided(
        crate::ids::ValidationId::new("val_1"),
        crate::validate::TriggerRecord::new(2, evidence, Vec::new()),
        Arm::Live,
        crate::event::ValidationOutcome::NotRun {
            reason: NotRunReason::JudgeUnavailable,
        },
    );
    session.record_control(record).await.unwrap();

    assert_eq!(
        session.state().validations_run(),
        0,
        "a check that never happened is not a check this session bought; a \
         judge outage would otherwise burn the whole lifetime allowance of \
         every session that fired a trigger while it lasted"
    );
    assert_eq!(
        session.state().tokens_since_last_validation(),
        evidence,
        "and the evidence that would have funded a check survives the outage, \
         so the session validates as soon as the judge is back rather than \
         having to earn the budget again"
    );
    assert!(
        session.state().last_validation_at_ms().is_some(),
        "the cooldown *is* spent, and it is the only thing that is: without it \
         a session with an open gate re-dials a down judge every single turn"
    );

    // The control: the same session, the same shape of event, and an outcome
    // that did reach a judge. This is what the session bought, and it is
    // charged for it.
    let more = routed_turn(&mut session, "t2", 7_000).await;
    let evidence = evidence + more;
    assert_eq!(
        session.state().tokens_since_last_validation(),
        evidence,
        "the outage did not reset the gate, so the next turn's spend adds to \
         what was already there"
    );
    let mut record = ControlRecord::default();
    record.validation_decided(
        crate::ids::ValidationId::new("val_2"),
        crate::validate::TriggerRecord::new(3, evidence, Vec::new()),
        Arm::Live,
        judged_continue(),
    );
    session.record_control(record).await.unwrap();
    assert_eq!(session.state().validations_run(), 1);
    assert_eq!(
        session.state().tokens_since_last_validation(),
        0,
        "the gate's budget resets for a check that happened"
    );
}

/// The tokens a check cost must not bring the next check forward.
///
/// A steered or halted turn completes with the *side call's* billing — the
/// judge's own prompt and answer — because that is genuinely what the turn cost
/// and the client is told so. What it is not is conversation spend, and both
/// projections it would otherwise land in are about conversation spend:
/// `tokens_since_last_validation` is the budget the trigger's gate opens on,
/// and `recent_turn_tokens` is the trailing distribution the cost-anomaly
/// signal compares each turn against. A check that fed either would be a
/// validator triggering on the cost of validating.
#[tokio::test]
async fn a_completing_interjections_own_tokens_reach_neither_trigger_projection() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;
    session
        .record_created("affinity", &Principal::new("acme", "ada"), Some(Arm::Live))
        .await
        .unwrap();

    // One ordinary dispatched turn, so both projections have something in them
    // that a check could later be confused with.
    let conversation = routed_turn(&mut session, "t1", 1_000).await;
    assert_eq!(session.state().tokens_since_last_validation(), conversation);
    assert_eq!(session.state().recent_turn_tokens(), &[conversation]);

    // A steered turn: no `Routed`, and the usage on the terminal event is the
    // judge's side call, exactly as `Validator::decide` hands it over.
    let admitted = session
        .begin_turn(TurnId::new("t2"), vec![Item::user_text("still going")])
        .await
        .unwrap();
    let response_id = admitted.response_id().clone();
    let judge_usage = Usage {
        input_tokens: 4_000,
        output_tokens: 40,
        ..Usage::default()
    };
    let mut record = ControlRecord::default();
    record.side_call_completed(
        crate::ids::SideCallId::new("sc_1"),
        Target::Frontier {
            provider: "anthropic".into(),
            model: "claude".into(),
        },
        judge_usage.clone(),
    );
    record.validation_decided(
        crate::ids::ValidationId::new("val_1"),
        crate::validate::TriggerRecord::new(2, conversation, Vec::new()),
        Arm::Live,
        judged_continue(),
    );
    session
        .complete_with_item(
            &response_id,
            Item::assistant_text(GUIDANCE, response_id.clone()),
            judge_usage,
            record,
        )
        .await
        .unwrap();

    assert_eq!(
        session.state().tokens_since_last_validation(),
        0,
        "counting the check's own tokens here would let the act of checking \
         bring the next check forward, which is the one thing this field's own \
         doc says it must not do"
    );
    assert_eq!(
        session.state().recent_turn_tokens(),
        &[conversation],
        "and the trailing distribution the cost-anomaly signal compares against \
         is a distribution of *conversation* turns: a side call in it makes the \
         next ordinary turn look cheap and the next check less likely"
    );

    // The control: an ordinary dispatched turn on the same session does feed
    // both, so the assertions above are about the interjection and not about a
    // fold that has stopped counting.
    let conversation_again = routed_turn(&mut session, "t3", 2_000).await;
    assert_eq!(
        session.state().tokens_since_last_validation(),
        conversation_again
    );
    assert_eq!(
        session.state().recent_turn_tokens(),
        &[conversation, conversation_again]
    );
}

/// **A turn's leading configuration run replaces the session's, at the head.**
///
/// The M11.1 review's F7 ruling as the fold sees it. A client that rebuilt one
/// line of its system prompt — the date rolled over, the branch changed, a beta
/// dropped out of the header — resends a different leading run, and what the
/// session must then hold is *one* system prompt, the new one, in front of a
/// conversation that is otherwise untouched. Two copies with the stale one
/// first is a prompt nobody wrote, and it is what a plain append gives.
///
/// The second turn is the one that matters. It appends the replacement *after*
/// two turns of history, so a fold that pushed would leave the new run in the
/// middle of the conversation; the assertion is that it is at the front and the
/// old one is gone.
#[test]
fn a_turns_leading_configuration_run_replaces_the_sessions_at_the_head() {
    fn configuration(text: &str) -> Item {
        Item {
            role: Role::Developer,
            content: ItemContent::Text { text: text.into() },
            response_id: None,
        }
    }

    let mut items = Vec::new();
    let mut cursor = ConfigurationCursor::default();

    cursor.turn_started();
    for item in [
        configuration("header"),
        configuration("prompt v1"),
        Item::user_text("hello"),
    ] {
        cursor.append(&mut items, item);
    }
    cursor.append(
        &mut items,
        Item::assistant_text("hi", ResponseId::new("r1")),
    );
    assert_eq!(cursor.len(), 2);

    cursor.turn_started();
    for item in [
        configuration("header"),
        configuration("prompt v2"),
        Item::user_text("again"),
    ] {
        cursor.append(&mut items, item);
    }

    assert_eq!(
        items
            .iter()
            .map(|item| (item.role, item.content.render()))
            .collect::<Vec<_>>(),
        vec![
            (Role::Developer, "header".to_string()),
            (Role::Developer, "prompt v2".to_string()),
            (Role::User, "hello".to_string()),
            (Role::Assistant, "hi".to_string()),
            (Role::User, "again".to_string()),
        ],
        "one system prompt, the current one, ahead of an untouched conversation"
    );
    assert_eq!(cursor.len(), 2);

    // Two consecutive configuration-only turns — a client retrying with a
    // rewritten prompt and nothing new to say — must replace rather than
    // accumulate. Without the per-turn reset the second run has no way to know
    // it is a new one, and the session ends up holding both.
    cursor.turn_started();
    cursor.append(&mut items, configuration("header"));
    cursor.append(&mut items, configuration("prompt v3"));
    cursor.turn_started();
    cursor.append(&mut items, configuration("header"));
    cursor.append(&mut items, configuration("prompt v4"));
    assert_eq!(
        items
            .iter()
            .map(|item| (item.role, item.content.render()))
            .collect::<Vec<_>>(),
        vec![
            (Role::Developer, "header".to_string()),
            (Role::Developer, "prompt v4".to_string()),
            (Role::User, "hello".to_string()),
            (Role::Assistant, "hi".to_string()),
            (Role::User, "again".to_string()),
        ],
    );

    // An interior system item is history: it is not `Developer`, so it never
    // enters the run, and a configuration item that follows it opens a fresh
    // one rather than extending the old.
    cursor.turn_started();
    cursor.append(&mut items, Item::system_text("a reminder"));
    assert_eq!(
        items.last().map(|item| (item.role, item.content.render())),
        Some((Role::System, "a reminder".to_string())),
        "appended as history, at the end, not lifted to the head"
    );
    assert_eq!(cursor.len(), 2, "and it did not join the configuration run");
}

// -------------------------------------------- ClassificationRecorded fold
//
// Contract: a delivered result is this session's answer to a call only when an
// outstanding intent under the same `call_id` names the same
// `source_turn_index` and the same `source_response_id`. Attribution is
// decided before anything is consumed, so a result that fails it closes no
// intent, claims no settlement, and files no feature -- which is exactly what
// leaves the call's true answer still able to land.
//
// Every case is driven through the session's own commit methods
// (`record_classification_intent`, `record_classification`) and then re-read
// through `SessionState::project`, because the invariant belongs to the fold:
// a writer that attributed one way and a successor replaying the same log the
// other would disagree about which turn a feature describes.

use crate::classify::{
    ClassificationOutcome, ClassifierIdentity, ContextDependence, EvaluationSpend, EvaluationUsage,
    Graded, ReservationRecord, SettlementAck, TAXONOMY_VERSION, TurnClassification, TurnComplexity,
    TurnIntent,
};
use crate::control::BudgetWindow;

fn classify_identity() -> ClassifierIdentity {
    ClassifierIdentity {
        model: "jev-1.12".to_string(),
        schema: "typesafe.systemone.choice.v1".to_string(),
        taxonomy_version: TAXONOMY_VERSION,
        projection_revision: 1,
        config_revision: 1,
    }
}

fn classify_reservation() -> ReservationRecord {
    ReservationRecord {
        rate_card: card(),
        estimated_input_tokens: 100,
        expected_output_tokens: 16,
        requested_usd: 0.0002,
        hold_ttl_ms: 30_000,
        budget_limit_usd: 100.0,
        budget_window: BudgetWindow::Total,
        member_ceiling_usd: None,
        warn_at: 0.8,
    }
}

/// The intent a session actually opened: `call_id`, about `source_turn_index`
/// / `source_response_id`.
fn classify_intent(
    call_id: &str,
    source_turn_index: u64,
    source_response_id: &str,
) -> ClassificationIntent {
    ClassificationIntent {
        call_id: ResponseId::new(call_id),
        source_turn_index,
        source_response_id: ResponseId::new(source_response_id),
        requested_at_ms: 0,
        expires_at_ms: 30_000,
        identity: classify_identity(),
        reservation: classify_reservation(),
    }
}

/// A usable answer, delivered under whatever `(call_id, source_turn_index,
/// source_response_id)` the caller names, so a test can deliver one whose
/// source disagrees with the intent it claims to settle.
///
/// The spend it carries is `SettlementAck::Committed` on purpose: the money
/// half of a call settles at the evaluation ledger and is already final by the
/// time this record exists. Whether the fold attributes the answer is a
/// separate question, and the tests below pin that the two do not move
/// together -- declining a feature does not un-charge anything.
fn classify_result(
    call_id: &str,
    source_turn_index: u64,
    source_response_id: &str,
) -> ClassificationRecord {
    ClassificationRecord {
        call_id: ResponseId::new(call_id),
        source_turn_index,
        source_response_id: ResponseId::new(source_response_id),
        completed_at_ms: 1_000,
        outcome: ClassificationOutcome::Classified {
            classification: TurnClassification {
                taxonomy_version: TAXONOMY_VERSION,
                intent: Graded {
                    value: TurnIntent::Implement,
                    confidence: 0.8,
                },
                complexity: Graded {
                    value: TurnComplexity::Involved,
                    confidence: 0.6,
                },
                context_dependence: Graded {
                    value: ContextDependence::Recent,
                    confidence: 0.5,
                },
            },
            spend: EvaluationSpend::Unknown {
                granted_usd: 0.0,
                settled: SettlementAck::Committed,
            },
            reported_model: None,
        },
    }
}

/// The projection a successor would rebuild from this session's own log.
///
/// Every attribution case below is asserted against this as well as against
/// the state the writer folded live. One fold serves both, so a case that held
/// for only one of them would mean a restart disagreed with the process that
/// wrote the log about which turns have answers.
async fn replayed(session: &Session<MemoryStore>) -> SessionState {
    SessionState::project(
        session.store.as_ref(),
        session.session_id(),
        CacheLedger::new(),
        None,
    )
    .await
    .expect("an in-memory log replays")
}

/// Outstanding intents as `(call_id, source_turn_index, source_response_id)`,
/// ordered.
///
/// The count alone would pass on a fold that dropped an intent and put a
/// different one back; the source fields are what the invariant is about, so
/// they are what gets compared.
fn outstanding_sources(state: &SessionState) -> Vec<(ResponseId, u64, ResponseId)> {
    let mut sources: Vec<_> = state
        .outstanding_classifications()
        .map(|intent| {
            (
                intent.call_id.clone(),
                intent.source_turn_index,
                intent.source_response_id.clone(),
            )
        })
        .collect();
    sources.sort();
    sources
}

/// Filed features as `(call_id, source_turn_index, available_seq)`, in the
/// order they became available.
fn filed_features(state: &SessionState) -> Vec<(ResponseId, u64, u64)> {
    state
        .classifications()
        .iter()
        .map(|available| {
            (
                available.reference.call_id.clone(),
                available.reference.source_turn_index,
                available.reference.available_seq,
            )
        })
        .collect()
}

/// **A result whose `source_turn_index` and `source_response_id` both disagree
/// with the intent it claims to settle answers a different question, and must
/// leave that intent open.**
///
/// Closing it would strand the call's true answer forever: the intent is gone,
/// so nothing records that the turn is still unanswered, and the settlement
/// slot the identity gets exactly once has been spent on a record about some
/// other turn. The second half of this test is that stranding, stated as a
/// requirement -- the correctly-sourced result still lands, exactly once, and
/// is available only from its own sequence onward.
#[tokio::test]
async fn a_mismatched_source_result_must_not_close_the_intent_or_block_the_true_one() {
    let store = Arc::new(MemoryStore::new());
    let (_sid, mut session) = new_session(store, "node-mismatch").await;

    session
        .record_classification_intent(classify_intent("call_1", 3, "resp_3"))
        .await
        .unwrap();
    assert_eq!(session.state.outstanding_classifications().count(), 1);

    // The delivered record names call_1, but a different turn and a
    // different source response than the intent it claims to settle.
    session
        .record_classification(classify_result("call_1", 9, "resp_9"))
        .await
        .unwrap();
    let mismatch_seq = session.last_seq();

    let intact = vec![(ResponseId::new("call_1"), 3, ResponseId::new("resp_3"))];
    assert_eq!(
        outstanding_sources(&session.state),
        intact,
        "a source-mismatched result must not close the intent it does not \
         actually answer, and must leave it naming its own source"
    );
    assert!(
        !session
            .state
            .classification_settled(&ResponseId::new("call_1")),
        "and must not consume the one settlement slot this call_id has -- \
         the ledger already committed this record's spend, which is a \
         separate fact from whether the log attributes its answer"
    );
    assert!(
        session.state.classifications().is_empty(),
        "no feature may be filed under a source the intent never claimed"
    );
    assert_eq!(
        outstanding_sources(&replayed(&session).await),
        intact,
        "and a successor replaying the log reads the same open intent"
    );
    assert!(replayed(&session).await.classifications().is_empty());

    // The true, correctly-sourced result for the same call must still be
    // able to land after the mismatched delivery above.
    session
        .record_classification(classify_result("call_1", 3, "resp_3"))
        .await
        .unwrap();
    let true_seq = session.last_seq();

    assert!(
        outstanding_sources(&session.state).is_empty(),
        "the correctly-sourced result closes the intent"
    );
    let expected = vec![(ResponseId::new("call_1"), 3, true_seq)];
    assert_eq!(
        filed_features(&session.state),
        expected,
        "and becomes exactly one feature, filed under the intent's own \
         source and available from its own sequence -- not the mismatch's"
    );
    assert_eq!(
        session.state.classifications_through(mismatch_seq).count(),
        0,
        "a decision whose cutoff predates the true result cannot name it"
    );

    let replayed = replayed(&session).await;
    assert!(outstanding_sources(&replayed).is_empty());
    assert_eq!(filed_features(&replayed), expected);
    assert!(replayed.classification_settled(&ResponseId::new("call_1")));
}

/// **The same requirement, isolated to `source_turn_index`** --
/// `source_response_id` matches the intent exactly, so a guard that compared
/// only the response id would wrongly accept this.
#[tokio::test]
async fn a_result_with_only_the_turn_index_wrong_must_not_close_the_intent() {
    let store = Arc::new(MemoryStore::new());
    let (_sid, mut session) = new_session(store, "node-mismatch-turn").await;

    session
        .record_classification_intent(classify_intent("call_1", 3, "resp_3"))
        .await
        .unwrap();

    // source_response_id ("resp_3") matches; source_turn_index (7, not 3)
    // does not.
    session
        .record_classification(classify_result("call_1", 7, "resp_3"))
        .await
        .unwrap();

    let intact = vec![(ResponseId::new("call_1"), 3, ResponseId::new("resp_3"))];
    assert_eq!(
        outstanding_sources(&session.state),
        intact,
        "a turn-index-only mismatch must not close the intent either"
    );
    assert!(
        !session
            .state
            .classification_settled(&ResponseId::new("call_1"))
    );
    assert!(session.state.classifications().is_empty());

    let replayed = replayed(&session).await;
    assert_eq!(outstanding_sources(&replayed), intact);
    assert!(replayed.classifications().is_empty());

    // And the intent is still answerable, which is the point of leaving it.
    session
        .record_classification(classify_result("call_1", 3, "resp_3"))
        .await
        .unwrap();
    assert_eq!(
        filed_features(&session.state),
        vec![(ResponseId::new("call_1"), 3, session.last_seq())],
        "the correctly-sourced result contributes exactly once"
    );
}

/// **The same requirement, isolated to `source_response_id`** --
/// `source_turn_index` matches the intent exactly, so a guard that compared
/// only the turn index would wrongly accept this.
#[tokio::test]
async fn a_result_with_only_the_response_id_wrong_must_not_close_the_intent() {
    let store = Arc::new(MemoryStore::new());
    let (_sid, mut session) = new_session(store, "node-mismatch-response").await;

    session
        .record_classification_intent(classify_intent("call_1", 3, "resp_3"))
        .await
        .unwrap();

    // source_turn_index (3) matches; source_response_id ("resp_9", not
    // "resp_3") does not.
    session
        .record_classification(classify_result("call_1", 3, "resp_9"))
        .await
        .unwrap();

    let intact = vec![(ResponseId::new("call_1"), 3, ResponseId::new("resp_3"))];
    assert_eq!(
        outstanding_sources(&session.state),
        intact,
        "a response-id-only mismatch must not close the intent either"
    );
    assert!(
        !session
            .state
            .classification_settled(&ResponseId::new("call_1"))
    );
    assert!(session.state.classifications().is_empty());

    let replayed = replayed(&session).await;
    assert_eq!(outstanding_sources(&replayed), intact);
    assert!(replayed.classifications().is_empty());

    // And the intent is still answerable, which is the point of leaving it.
    session
        .record_classification(classify_result("call_1", 3, "resp_3"))
        .await
        .unwrap();
    assert_eq!(
        filed_features(&session.state),
        vec![(ResponseId::new("call_1"), 3, session.last_seq())],
        "the correctly-sourced result contributes exactly once"
    );
}

/// **A result naming a `call_id` this session holds no intent for is nothing
/// this session asked for: not settled, not a feature.**
///
/// A hand-repaired log, a successor delivering into the wrong session, or an
/// intent lost to a bug elsewhere all arrive here, and reading any of them as
/// an ordinary answer would put a label on a turn nobody paid to have read.
/// Rejecting it must also not poison the identity: if the intent shows up
/// afterwards, its own answer still lands.
#[tokio::test]
async fn a_result_with_no_matching_intent_must_not_be_settled_or_filed() {
    let store = Arc::new(MemoryStore::new());
    let (_sid, mut session) = new_session(store, "node-orphan").await;

    assert_eq!(session.state.outstanding_classifications().count(), 0);

    session
        .record_classification(classify_result("call_orphan", 2, "resp_2"))
        .await
        .unwrap();

    assert!(
        !session
            .state
            .classification_settled(&ResponseId::new("call_orphan")),
        "a result this session never requested must not read as settled"
    );
    assert!(
        session.state.classifications().is_empty(),
        "and must not be folded into a feature"
    );
    let replayed = replayed(&session).await;
    assert!(!replayed.classification_settled(&ResponseId::new("call_orphan")));
    assert!(replayed.classifications().is_empty());

    // The intent this session really did open for that call arrives late --
    // the durable intent is written before dispatch, but a log assembled out
    // of order is exactly the case this arm has to survive.
    session
        .record_classification_intent(classify_intent("call_orphan", 2, "resp_2"))
        .await
        .unwrap();
    session
        .record_classification(classify_result("call_orphan", 2, "resp_2"))
        .await
        .unwrap();

    assert_eq!(
        filed_features(&session.state),
        vec![(ResponseId::new("call_orphan"), 2, session.last_seq())],
        "the rejected delivery did not spend the identity's one answer"
    );
    assert!(
        session
            .state
            .classification_settled(&ResponseId::new("call_orphan"))
    );
}

/// Controls: an ordinary delayed delivery (other events land between the
/// intent and its result) and an ordinary duplicate delivery (the same record
/// folded twice) both behave as documented -- the delayed result lands once,
/// and the duplicate does not double it. Kept beside the mismatch and orphan
/// cases so the attribution guard is visibly not a rejection of ordinary
/// out-of-order delivery.
#[tokio::test]
async fn a_delayed_and_then_duplicated_delivery_lands_exactly_once() {
    let store = Arc::new(MemoryStore::new());
    let (_sid, mut session) = new_session(store, "node-control").await;

    session
        .record_classification_intent(classify_intent("call_a", 1, "resp_1"))
        .await
        .unwrap();

    // Time passes: an unrelated call is requested and answered before
    // `call_a`'s own result arrives -- an ordinary delayed delivery, not a
    // same-turn round trip.
    session
        .record_classification_intent(classify_intent("call_b", 2, "resp_2"))
        .await
        .unwrap();
    session
        .record_classification(classify_result("call_b", 2, "resp_2"))
        .await
        .unwrap();
    let b_seq = session.last_seq();
    assert_eq!(session.state.classifications().len(), 1);

    // `call_a`'s correctly-sourced result finally lands.
    session
        .record_classification(classify_result("call_a", 1, "resp_1"))
        .await
        .unwrap();
    let a_seq = session.last_seq();
    let expected = vec![
        (ResponseId::new("call_b"), 2, b_seq),
        (ResponseId::new("call_a"), 1, a_seq),
    ];
    assert_eq!(
        filed_features(&session.state),
        expected,
        "both land, in availability order rather than source order"
    );
    assert!(session.state.outstanding_classifications().next().is_none());

    // A duplicate delivery of the same call -- a successor re-driving a
    // result this session already folded.
    session
        .record_classification(classify_result("call_a", 1, "resp_1"))
        .await
        .unwrap();
    assert_eq!(
        filed_features(&session.state),
        expected,
        "a duplicate delivery must not double the feature"
    );
    assert_eq!(
        session.state.classifications_through(b_seq).count(),
        1,
        "and the availability cutoff still separates them"
    );

    assert_eq!(filed_features(&replayed(&session).await), expected);
}

/// **Attribution survives the wire, not just the process that wrote it.**
///
/// The events are serialized and deserialized before a fresh `SessionState`
/// folds them, which is what a store that persists JSON actually hands a
/// successor. A guard that compared something the encoding dropped would pass
/// every test above and fail here.
#[tokio::test]
async fn attribution_holds_across_a_serialized_replay() {
    let store = Arc::new(MemoryStore::new());
    let (sid, mut session) = new_session(store.clone(), "node-serde").await;

    session
        .record_classification_intent(classify_intent("call_1", 3, "resp_3"))
        .await
        .unwrap();
    session
        .record_classification(classify_result("call_1", 9, "resp_9"))
        .await
        .unwrap();
    session
        .record_classification_intent(classify_intent("call_2", 4, "resp_4"))
        .await
        .unwrap();
    session
        .record_classification(classify_result("call_2", 4, "resp_4"))
        .await
        .unwrap();

    let logged = store.read_events(&sid, 0, 1024).await.unwrap();
    let round_tripped: Vec<SessionEventKind> = logged
        .iter()
        .map(|event| {
            let encoded = serde_json::to_string(&event.kind).expect("an event encodes");
            serde_json::from_str(&encoded).expect("and decodes to the same kind")
        })
        .collect();

    let successor_store = MemoryStore::new();
    let successor = SessionId::generate();
    successor_store
        .create_session(&successor, "affinity")
        .await
        .unwrap();
    let lease = successor_store
        .acquire_lease(&successor, "node-serde-successor", TTL)
        .await
        .unwrap()
        .expect("an unheld session");
    successor_store
        .append_events(&lease, round_tripped, None)
        .await
        .unwrap();

    let state = SessionState::project(&successor_store, &successor, CacheLedger::new(), None)
        .await
        .expect("a replay of the decoded log");

    assert_eq!(
        outstanding_sources(&state),
        vec![(ResponseId::new("call_1"), 3, ResponseId::new("resp_3"))],
        "the mismatched delivery left call_1 open through the encoding"
    );
    assert!(!state.classification_settled(&ResponseId::new("call_1")));
    assert!(state.classification_settled(&ResponseId::new("call_2")));

    let filed = filed_features(&state);
    assert_eq!(filed.len(), 1, "only the correctly-sourced answer is filed");
    assert_eq!(filed[0].0, ResponseId::new("call_2"));
    assert_eq!(filed[0].1, 4);
    assert_eq!(
        state.classifications_through(filed[0].2 - 1).count(),
        0,
        "and it is nameable only from the sequence it landed at"
    );
}

// ------------------------------------------------- the availability cutoff

/// **Finding the cutoff must not read the history behind it.**
///
/// `classifications_through` runs on every routed turn of every session, and a
/// long session is exactly where it matters: a scan that reads all of a
/// thousand entries to answer "how many landed by sequence N" is work that
/// grows with the session on a path that returns four references however long
/// the session is.
///
/// Counted key reads rather than a clock: [`landed_through`] takes the key
/// reader, so this can say exactly how many entries were touched. A timing
/// assertion would pass on a quiet machine and fail on a loaded one while
/// saying nothing about the shape of the work.
#[test]
fn finding_the_cutoff_reads_a_logarithmic_number_of_entries() {
    let seqs: Vec<u64> = (0..4_096u64).map(|index| index * 2).collect();
    let probes = std::cell::Cell::new(0usize);
    let counted = |seq: &u64| {
        probes.set(probes.get() + 1);
        *seq
    };

    let landed = landed_through(&seqs, 5_000, counted);

    assert_eq!(
        landed, 2_501,
        "every even sequence up to and including 5,000"
    );
    let ceiling = seqs.len().ilog2() as usize + 2;
    assert!(
        probes.get() <= ceiling,
        "the cutoff must be found without reading the history behind it: \
         {} entries read of {}, and a logarithmic search reads at most {ceiling}",
        probes.get(),
        seqs.len()
    );
}

/// The control for the search above: the answer it gives is the one a reader
/// of every entry would give, at every boundary where an off-by-one hides.
#[test]
fn the_cutoff_admits_exactly_what_landed_at_or_before_it() {
    let seqs: Vec<u64> = vec![2, 4, 6, 8];
    let counted = |seq: &u64| *seq;

    assert_eq!(landed_through(&[] as &[u64], 10, counted), 0, "empty");
    assert_eq!(landed_through(&seqs, 1, counted), 0, "below every entry");
    assert_eq!(landed_through(&seqs, 2, counted), 1, "the first, exactly");
    assert_eq!(landed_through(&seqs, 3, counted), 1, "between two entries");
    assert_eq!(landed_through(&seqs, 8, counted), 4, "the last, exactly");
    assert_eq!(landed_through(&seqs, 9, counted), 4, "above every entry");

    // And the same answers the linear reading gives, over every cutoff the
    // slice can be asked about.
    for cutoff in 0..12u64 {
        assert_eq!(
            landed_through(&seqs, cutoff, counted),
            seqs.iter().filter(|seq| **seq <= cutoff).count(),
            "at cutoff {cutoff}"
        );
    }
}

// --------------------------------------- the acknowledgement removal bound
//
// Contract: an *outage* sizes the unrepaired backlog, and an acknowledgement
// must not walk it. `ClassificationRuntime::repair_batch` bounds how many
// settlements one turn *schedules*; that is a different quantity from how much
// work the fold does when their answers come back, and only the second is
// pinned here. The engine writes one `ClassificationSettlementRepaired` per
// answered repair, so a turn that acknowledges a batch of K against a backlog
// of N decides whether a recovering deployment pays K or K*N.
//
// Counted entries rather than a clock, on the `landed_through` precedent
// above: a wall-clock threshold passes on a quiet box and fails on a loaded
// one, and a constant nobody observed passes over any implementation at all.

/// The call id the `index`-th entry of a synthetic backlog was committed
/// under.
fn backlog_call(index: usize) -> String {
    format!("eval_backlog_{index}")
}

/// A result that says the evaluation ledger never acknowledged this call's
/// charge, which is the one outcome the fold holds for a later turn to
/// re-drive.
///
/// `Unusable` rather than `Classified`: an envelope arrived and its answers
/// could not be used, so the accounting survives with nothing to say about the
/// turn — which keeps these entries out of `classifications()` and makes the
/// feature-count controls below unambiguous.
fn unconfirmed_result(
    call_id: &str,
    source_turn_index: u64,
    source_response_id: &str,
    usd: f64,
) -> ClassificationRecord {
    ClassificationRecord {
        call_id: ResponseId::new(call_id),
        source_turn_index,
        source_response_id: ResponseId::new(source_response_id),
        completed_at_ms: 1_000,
        outcome: ClassificationOutcome::Unusable {
            reason: "schema".to_string(),
            spend: EvaluationSpend::Measured {
                granted_usd: 0.0002,
                usage: EvaluationUsage {
                    input_tokens: 100,
                    output_tokens: 16,
                },
                usd,
                settled: SettlementAck::Unconfirmed,
            },
            reported_model: None,
        },
    }
}

/// Build a backlog through the intent/result join; distinct costs expose reordering.
async fn session_with_backlog(node: &str, size: usize) -> Session<MemoryStore> {
    let store = Arc::new(MemoryStore::new());
    let (_sid, mut session) = new_session(store, node).await;
    for index in 1..=size {
        let call = backlog_call(index);
        let turn = index as u64;
        let response = format!("resp_{index}");
        session
            .record_classification_intent(classify_intent(&call, turn, &response))
            .await
            .unwrap();
        session
            .record_classification(unconfirmed_result(&call, turn, &response, index as f64))
            .await
            .unwrap();
    }
    assert_eq!(
        session.state.unrepaired_settlements().len(),
        size,
        "the fixture must actually build a backlog: an attribution mismatch \
         here would leave nothing unrepaired and every bound below would hold \
         vacuously"
    );
    session
}

/// The backlog as `(call_id, usd)`, in the order the fold offers it.
fn backlog_order(state: &SessionState) -> Vec<(String, f64)> {
    state
        .unrepaired_settlements()
        .map(|settlement| (settlement.call_id.to_string(), settlement.usd))
        .collect()
}

/// One acknowledgement, as the engine commits it after a repair worker answers.
fn acknowledgement(call_id: &str, applied: bool) -> ClassificationSettlementRepair {
    ClassificationSettlementRepair {
        call_id: ResponseId::new(call_id),
        applied,
        repaired_at_ms: 9_000,
    }
}

/// Count settlement visits for a fixed acknowledgement batch at two backlog sizes.
/// Map lookup comparisons are excluded; this guards against a full scan, not logarithmic lookup.
#[tokio::test]
async fn acknowledging_a_batch_examines_entries_for_the_batch_and_not_the_backlog() {
    const ACKNOWLEDGED: usize = 4;
    const SIZES: [usize; 2] = [16, 1_024];

    let mut examined = Vec::new();
    for size in SIZES {
        let mut session = session_with_backlog(&format!("node-bound-{size}"), size).await;
        let before = session.state.unrepaired_settlements_examined();
        for index in 1..=ACKNOWLEDGED {
            session
                .record_classification_settlement_repair(acknowledgement(
                    &backlog_call(index),
                    true,
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            session.state.unrepaired_settlements().len(),
            size - ACKNOWLEDGED,
            "and each acknowledgement must actually drain its entry, or the \
             count below would be cheap because it did nothing, backlog={size}"
        );
        examined.push(session.state.unrepaired_settlements_examined() - before);
    }

    assert_eq!(
        examined[0], examined[1],
        "acknowledging {ACKNOWLEDGED} settlements examined {} backlog entries \
         under a backlog of {} and {} under a backlog of {} -- work that grows \
         with the outage is what a recovering deployment cannot afford",
        examined[0], SIZES[0], examined[1], SIZES[1]
    );
    assert_eq!(
        examined[0], ACKNOWLEDGED as u64,
        "one entry per acknowledgement and no others: an equal-but-large count \
         would mean both backlogs were walked equally badly"
    );
}

/// Missing acknowledgements must leave the backlog unchanged without scanning its entries.
#[tokio::test]
async fn an_acknowledgement_naming_nothing_in_the_backlog_examines_nothing() {
    let mut session = session_with_backlog("node-missing", 32).await;
    let before = backlog_order(&session.state);
    let examined_before = session.state.unrepaired_settlements_examined();

    session
        .record_classification_settlement_repair(acknowledgement("eval_never_seen", true))
        .await
        .unwrap();

    assert_eq!(
        backlog_order(&session.state),
        before,
        "an acknowledgement for a call this session never settled must leave \
         the backlog exactly as it was"
    );
    assert_eq!(
        session.state.unrepaired_settlements_examined(),
        examined_before,
        "and must not pay to discover that"
    );
}

/// A repeated acknowledgement must not scan or remove another settlement.
#[tokio::test]
async fn a_repeated_acknowledgement_examines_nothing_and_drains_nothing_twice() {
    let mut session = session_with_backlog("node-duplicate", 32).await;

    session
        .record_classification_settlement_repair(acknowledgement(&backlog_call(7), true))
        .await
        .unwrap();
    let after_first = backlog_order(&session.state);
    let examined_after_first = session.state.unrepaired_settlements_examined();
    assert_eq!(
        after_first.len(),
        31,
        "the first acknowledgement drains exactly its own entry"
    );

    session
        .record_classification_settlement_repair(acknowledgement(&backlog_call(7), false))
        .await
        .unwrap();

    assert_eq!(
        backlog_order(&session.state),
        after_first,
        "a repeated acknowledgement must not drain a second entry -- the \
         identity it names is already gone"
    );
    assert_eq!(
        session.state.unrepaired_settlements_examined(),
        examined_after_first,
        "and must not scan the backlog to find that out"
    );
}

/// Interior removals must preserve arrival order in both live state and replay.
#[tokio::test]
async fn interior_removal_keeps_the_backlog_oldest_first_through_replay() {
    let mut session = session_with_backlog("node-order", 8).await;

    for (index, applied) in [(3usize, true), (6, false)] {
        session
            .record_classification_settlement_repair(acknowledgement(&backlog_call(index), applied))
            .await
            .unwrap();
    }

    let expected: Vec<(String, f64)> = [1usize, 2, 4, 5, 7, 8]
        .into_iter()
        .map(|index| (backlog_call(index), index as f64))
        .collect();
    assert_eq!(
        backlog_order(&session.state),
        expected,
        "the survivors stay in arrival order, and `applied: false` drains its \
         entry exactly as `applied: true` does -- both are the ledger ending \
         the question"
    );
    assert_eq!(
        backlog_order(&replayed(&session).await),
        expected,
        "and a successor replaying this log repairs them in the same order"
    );
}
