// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `Session` and `SessionState` under test.
//!
//! Split from `session.rs` when the tests outgrew the code they pin, on the
//! `control_config/config/tests.rs` precedent: a module split would be wrong
//! — the session and its state are one concept — but a thousand test lines
//! standing between a reader and the settle seam were not earning their
//! position.

use super::*;
use crate::control::Principal;
use crate::item::{ItemContent, Role};
use crate::routing::{Candidate, Target};
use crate::store::MemoryStore;
use async_trait::async_trait;
use std::sync::Mutex;

const TTL: u64 = 30_000;

/// The steer a test emits: one call id, one name, one argument string,
/// minted once and reused by every party the way the real one is.
const STEER_CALL_ID: &str = "rhsteer_resp_1";
const STEER_NAME: &str = "fetch_steer";
const STEER_ARGS: &str = r#"{"steer_id":"rhsteer_resp_1"}"#;

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
/// dollars.
fn decision_for(target: Target, isl: u64) -> DecisionRecord {
    DecisionRecord {
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
        withheld_providers: Vec::new(),
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
        .complete(&response_id, "hi there", Usage::default())
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
        .complete(&first_id, "hi", billed.clone())
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
        .complete(&response_id, "part one and two", Usage::default())
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
                .complete(&response_id, "a", Usage::default())
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
            Item::tool_call(STEER_CALL_ID, STEER_NAME, STEER_ARGS),
            Usage::default(),
            ControlRecord::default(),
        )
        .await
        .unwrap();

    let projected = SessionState::project(store.as_ref(), &sid, CacheLedger::new(), None)
        .await
        .expect("a projection needs no lease");
    assert_eq!(projected.open_steer_ids(), vec![STEER_CALL_ID.to_string()]);
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
        reopened.state().open_steer_ids(),
        reprojected.open_steer_ids()
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

#[async_trait]
impl SessionStore for BatchRecordingStore {
    async fn create_session(
        &self,
        session_id: &SessionId,
        model_policy: &str,
    ) -> Result<bool, StoreError> {
        self.inner.create_session(session_id, model_policy).await
    }

    async fn acquire_lease(
        &self,
        session_id: &SessionId,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<Option<Lease>, StoreError> {
        self.inner.acquire_lease(session_id, node_id, ttl_ms).await
    }

    async fn renew_lease(&self, lease: &Lease, ttl_ms: u64) -> Result<Option<Lease>, StoreError> {
        self.inner.renew_lease(lease, ttl_ms).await
    }

    async fn release_lease(&self, lease: &Lease) -> Result<(), StoreError> {
        self.inner.release_lease(lease).await
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let appended = self.inner.append_events(lease, kinds).await?;
        // Recorded only on success: a rejected append wrote nothing, and
        // counting it would let a fenced writer look like a second batch.
        self.batches
            .lock()
            .expect("batch record poisoned")
            .push(appended.iter().map(|event| event.kind.clone()).collect());
        Ok(appended)
    }

    async fn read_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        self.inner.read_events(session_id, after_seq, limit).await
    }

    async fn last_seq(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        self.inner.last_seq(session_id).await
    }
}

/// The item a steered turn completes with, built the way the emission
/// builds it: bare name, no namespace, arguments minted once.
fn steer_call() -> Item {
    Item::tool_call(STEER_CALL_ID, STEER_NAME, STEER_ARGS)
}

/// The client's half of the round trip: the output it appends after
/// running the call, canonicalized exactly as `canonical_item` produces it
/// — role `tool`, no response id.
fn tool_result(call_id: &str, output: &str) -> Item {
    Item {
        role: Role::Tool,
        content: ItemContent::ToolResult {
            call_id: call_id.into(),
            output: output.into(),
        },
        response_id: None,
    }
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
            steer_call(),
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
         leaves a session holding a tool call whose turn never completed, \
         so the client's retry does not deduplicate and the same call is \
         emitted twice"
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
                if matches!(item.content, ItemContent::ToolCall { .. }))
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

#[tokio::test]
async fn the_appended_item_carries_the_response_stamp_and_renders_as_a_tool_call() {
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
            steer_call(),
            Usage::default(),
            ControlRecord::default(),
        )
        .await
        .unwrap();

    let emitted = session
        .state()
        .items
        .last()
        .expect("the item was committed");
    assert_eq!(
        emitted.response_id,
        Some(response_id),
        "the stamp is the provenance marker the whole design rests on: an \
         item bearing this response's id is one this response emitted, and \
         every item on the input path carries none"
    );
    assert_eq!(
        emitted.role,
        Role::Assistant,
        "the role `canonical_item` gives an incoming `function_call`, so \
         the client's resend of this very call compares equal to it under \
         prefix admission, which compares role and content only"
    );
    assert_eq!(
        emitted.content,
        ItemContent::ToolCall {
            call_id: STEER_CALL_ID.into(),
            name: STEER_NAME.into(),
            arguments: STEER_ARGS.into(),
        },
        "arguments are stored as minted, never re-serialized: the client \
         echoes the string back verbatim and it has to match by \
         construction"
    );
    assert!(
        !STEER_NAME.contains("__"),
        "the log stores the bare tool name; the namespace lives only in \
         the wire projection, so a namespaced resend and a flat one \
         canonicalize to this same stored item"
    );
    assert!(
        emitted.render().contains(STEER_NAME),
        "an emitted call is an ordinary item on the ordinary path: it \
         renders into the prompt like every other one"
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
            steer_call(),
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
            .filter(
                |item| matches!(&item.content, ItemContent::ToolCall { call_id, .. }
                if call_id == STEER_CALL_ID)
            )
            .count(),
        1,
        "and the call is emitted once, not once per retry"
    );
}

#[tokio::test]
async fn a_server_emitted_tool_call_opens_a_steer_and_the_matching_result_closes_it() {
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
            steer_call(),
            Usage::default(),
            ControlRecord::default(),
        )
        .await
        .unwrap();

    let opened_at = *session
        .state()
        .open_steers
        .get(STEER_CALL_ID)
        .expect("an emitted tool call opens a steer");

    // The client answers: it resends the call verbatim -- canonicalized
    // with no response id, exactly as `canonical_item` produces it -- and
    // appends the output it got.
    session
        .begin_turn(
            TurnId::new("t2"),
            vec![
                Item::user_text("q"),
                steer_call(),
                tool_result(STEER_CALL_ID, "{\"directive\":\"narrow the search\"}"),
            ],
        )
        .await
        .unwrap();

    assert!(
        !session.state().open_steers.contains_key(STEER_CALL_ID),
        "the client's output for this call id closes it -- one writer, one \
         event kind, no second source of truth"
    );

    // Fulfilment latency is derivable from the projection and the log
    // alone, which is the reason the opening timestamp is what the
    // projection holds.
    let closed_at = session
        .events_since(0, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|event| {
            matches!(&event.kind, SessionEventKind::ItemAppended { item }
                if matches!(&item.content, ItemContent::ToolResult { call_id, .. }
                    if call_id == STEER_CALL_ID))
        })
        .expect("the closing item is in the log")
        .at_ms;
    assert!(
        closed_at >= opened_at,
        "latency is closed_at - opened_at, and it cannot be negative"
    );
}

#[tokio::test]
async fn an_unrelated_tool_result_leaves_the_steer_open() {
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
            steer_call(),
            Usage::default(),
            ControlRecord::default(),
        )
        .await
        .unwrap();

    // The agent's own tooling, running its own call, in the same turn the
    // steer is still waiting on. Closing on any result rather than on the
    // matching one would report a steer fulfilled that nobody answered.
    session
        .begin_turn(
            TurnId::new("t2"),
            vec![tool_result("call_the_agent_made", "42")],
        )
        .await
        .unwrap();

    assert!(
        session.state().open_steers.contains_key(STEER_CALL_ID),
        "only the matching call id closes a steer"
    );
}

#[tokio::test]
async fn a_client_sent_tool_call_in_input_opens_no_steer() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    // The agent telling us about a call it made itself. Identical in shape
    // to an emitted one and different in provenance: an input item carries
    // no response id, because `canonical_item` sets none.
    session
        .begin_turn(
            TurnId::new("t1"),
            vec![Item::tool_call("call_the_agent_made", "grep", "{}")],
        )
        .await
        .unwrap();

    assert!(
        session.state().open_steers.is_empty(),
        "a steer is opened by provenance, not by shape -- otherwise a \
         client could open one by sending a tool call, and M6's \
         open-steer exclusion would be a knob the client turns"
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
    session.complete(&response_id, "done", usage).await.unwrap();
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
            "done",
            Usage {
                input_tokens: 100,
                ..Usage::default()
            },
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

/// The hysteresis rule's evidence, folded from the turn that carries it.
#[tokio::test]
async fn the_turn_whose_input_answers_a_steer_is_the_one_that_says_so() {
    let store = Arc::new(MemoryStore::new());
    let (_, mut session) = new_session(store, "node-a").await;

    // A steered turn: the call is emitted and the steer is open.
    let admitted = session
        .begin_turn(TurnId::new("t1"), vec![Item::user_text("go")])
        .await
        .unwrap();
    let response_id = admitted.response_id().clone();
    session
        .complete_with_item(
            &response_id,
            steer_call(),
            Usage::default(),
            ControlRecord::default(),
        )
        .await
        .unwrap();
    assert!(!session.state().this_turn_fulfilled_a_steer());

    // The next turn resends the call and appends its output. By the time the
    // interjection seam runs, the input is already committed and `open_steers`
    // is already empty — which is exactly why the question is about a turn
    // index and not about whether a steer is open.
    session
        .begin_turn(
            TurnId::new("t2"),
            vec![steer_call(), tool_result(STEER_CALL_ID, "re-read the task")],
        )
        .await
        .unwrap();
    assert!(session.state().open_steer_ids().is_empty());
    assert!(
        session.state().this_turn_fulfilled_a_steer(),
        "the turn that answers a correction looks, to every signal, exactly \
         like the turn that provoked it; without this the steer re-triggers \
         the validation that emitted it, forever"
    );

    // The control: the turn after it does not.
    let admitted = session
        .begin_turn(TurnId::new("t3"), vec![Item::user_text("carry on")])
        .await
        .unwrap();
    assert!(
        !session.state().this_turn_fulfilled_a_steer(),
        "a steer fulfilled two turns ago must not disable validation for the \
         rest of the session"
    );
    let response_id = admitted.response_id().clone();
    session
        .complete(&response_id, "done", Usage::default())
        .await
        .unwrap();
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
        .complete_with_item(&response_id, steer_call(), judge_usage, record)
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
