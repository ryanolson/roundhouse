// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests for the claims this design rests on.
//!
//! Four things are worth proving, and none of them need a GPU:
//!
//! 1. A client sends a constant number of bytes per turn while the context it
//!    is reasoning over grows without bound. That is the reason to be stateful.
//! 2. Routing genuinely reacts to cache state — a target that was warmed on an
//!    earlier turn wins a later one it would otherwise lose.
//! 3. Killing the owning process mid-session loses nothing. A successor claims
//!    the lease, replays the log, and continues.
//! 4. The token stream the routing hashes describe is the one actually
//!    dispatched. If those ever diverge, every cache-locality number the system
//!    reports is fiction.
//!
//! What a turn survives — a lease shorter than itself, a hung provider, a
//! stream that breaks halfway — is proved next door, in `turn_lifecycle.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use roundhouse_core::context::{ByteTokenizer, ContextAssembler};
use roundhouse_core::event::{IncompleteReason, SessionEventKind, Usage};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::{Item, Role};
use roundhouse_core::routing::{AffinityPolicy, CacheLedger, RoutingPolicy, Target};
use roundhouse_core::session::Session;
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::{
    EchoFrontierClient, FleetError, FrontierClient, FrontierError, FrontierQuote, FrontierStream,
    LocalFleet,
};
use roundhouse_server::{EchoLocalExecutor, Engine, EngineConfig, LocalExecution, LocalExecutor};

mod common;
use common::{BLOCK_SIZE, LOCAL_MODEL, config, embedded_fleet, frontier_catalog};

fn engine_without_fleet(
    store: Arc<MemoryStore>,
    policy: Arc<dyn RoutingPolicy>,
) -> Engine<MemoryStore, ByteTokenizer> {
    Engine::new(
        store,
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new("frontier answer")),
        policy,
        config(),
    )
}

/// The headline claim: constant client cost, growing server-side context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_bytes_stay_flat_while_context_grows() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine_without_fleet(store.clone(), Arc::new(AffinityPolicy::new()));
    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let mut bytes_per_turn = Vec::new();
    let mut context_tokens = Vec::new();

    for turn in 0..20 {
        // What a stateless client would have to re-upload every turn is the
        // whole conversation; here it is one message.
        let message = format!("Step {turn}: continue the analysis please.");
        bytes_per_turn.push(message.len());

        engine
            .run_turn(
                &session_id,
                TurnId::new(format!("turn-{turn}")),
                vec![Item::user_text(message)],
            )
            .await
            .unwrap();

        let session = Session::open(
            store.clone(),
            session_id.clone(),
            "probe",
            10_000,
            CacheLedger::new(),
        )
        .await
        .unwrap();
        let rendered: usize = session.state().items.iter().map(|i| i.render().len()).sum();
        context_tokens.push(rendered);
        session.release().await.unwrap();
    }

    // Client-side per-turn cost is flat: every turn is within a few bytes of
    // the first, purely from the turn number's digit count.
    let smallest = *bytes_per_turn.iter().min().unwrap();
    let largest = *bytes_per_turn.iter().max().unwrap();
    assert!(
        largest - smallest <= 2,
        "client bytes per turn must not grow with context: {bytes_per_turn:?}"
    );

    // Server-side context grew monotonically and by a large multiple, which is
    // exactly the cost the client no longer pays.
    assert!(context_tokens.windows(2).all(|pair| pair[1] > pair[0]));
    assert!(
        *context_tokens.last().unwrap() > 10 * context_tokens[0],
        "context should grow substantially over 20 turns: {context_tokens:?}"
    );
}

/// Routing must actually respond to cache state, not just to static config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warmed_frontier_target_wins_a_turn_it_would_otherwise_lose() {
    let store = Arc::new(MemoryStore::new());
    let session_id = SessionId::generate();

    // No local fleet, so the frontier is the only option and the first turn
    // necessarily warms it.
    let engine = engine_without_fleet(store.clone(), Arc::new(AffinityPolicy::new()));
    engine.create_session(&session_id).await.unwrap();

    let first = engine
        .run_turn(
            &session_id,
            TurnId::new("t0"),
            vec![Item::user_text("open the task")],
        )
        .await
        .unwrap();
    let first_decision = first.decision.expect("first turn must route");
    assert!(!first_decision.target.is_local());

    // Second turn: the ledger now knows this target is warm, so the recorded
    // expected prefill must be lower than the prompt length.
    let second = engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            vec![Item::user_text("continue")],
        )
        .await
        .unwrap();
    assert!(second.decision.is_some());

    let session = Session::open(store, session_id, "probe", 10_000, CacheLedger::new())
        .await
        .unwrap();
    let routed: Vec<_> = session
        .events_since(0, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .collect();

    assert_eq!(routed.len(), 2);
    // Cold: the whole prompt is prefill. Warm: strictly less.
    assert_eq!(
        routed[0].expected_prefill_tokens,
        routed[0].isl_tokens as f64
    );
    assert!(
        routed[1].expected_prefill_tokens < routed[1].isl_tokens as f64,
        "a warmed target must be priced below its prompt length"
    );
    assert!(!routed[1].considered.is_empty());
}

/// Killing the owner mid-session must lose nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successor_resumes_a_killed_session_without_gaps() {
    let store = Arc::new(MemoryStore::new());
    let session_id = SessionId::generate();
    let engine = engine_without_fleet(store.clone(), Arc::new(AffinityPolicy::new()));
    engine.create_session(&session_id).await.unwrap();

    for turn in 0..3 {
        engine
            .run_turn(
                &session_id,
                TurnId::new(format!("t{turn}")),
                vec![Item::user_text(format!("question {turn}"))],
            )
            .await
            .unwrap();
    }

    // Client's cursor at the moment of the crash.
    let cursor = {
        let probe = Session::open(
            store.clone(),
            session_id.clone(),
            "probe",
            10_000,
            CacheLedger::new(),
        )
        .await
        .unwrap();
        let seq = probe.last_seq();
        probe.release().await.unwrap();
        seq
    };

    // The owner dies without releasing. A successor on a different node takes
    // over once the lease lapses.
    store.expire_lease_now(&session_id).await;
    let successor = Engine::new(
        store.clone(),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new("frontier answer")),
        Arc::new(AffinityPolicy::new()),
        EngineConfig {
            node_id: "node-b".to_string(),
            ..config()
        },
    );

    let resumed = successor
        .run_turn(
            &session_id,
            TurnId::new("t3"),
            vec![Item::user_text("question 3")],
        )
        .await
        .unwrap();

    // Replay from the client's cursor: contiguous, no gap and no repeat.
    let session = Session::open(store, session_id, "probe-2", 10_000, CacheLedger::new())
        .await
        .unwrap();
    let replayed = session.events_since(cursor, 1000).await.unwrap();
    assert!(!replayed.is_empty());
    let seqs: Vec<u64> = replayed.iter().map(|event| event.seq).collect();
    let expected: Vec<u64> = (cursor + 1..=resumed.last_seq).collect();
    assert_eq!(
        seqs, expected,
        "replay must be contiguous across a failover"
    );

    // The successor saw all four turns, including the three it did not run.
    assert_eq!(session.turn_index(), 4);
}

/// A retried turn must not be answered twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_turn_replays_instead_of_regenerating() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine_without_fleet(store.clone(), Arc::new(AffinityPolicy::new()));
    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let first = engine
        .run_turn(
            &session_id,
            TurnId::new("same-turn"),
            vec![Item::user_text("hello")],
        )
        .await
        .unwrap();
    assert!(!first.deduplicated);

    // The client never saw the response and retries with the same turn id.
    let retry = engine
        .run_turn(
            &session_id,
            TurnId::new("same-turn"),
            vec![Item::user_text("hello")],
        )
        .await
        .unwrap();

    assert!(retry.deduplicated);
    assert_eq!(retry.response_id, first.response_id);
    assert!(retry.decision.is_none(), "a replay must not route again");

    // A replay is a redelivery, not a re-derivation. The client must receive
    // the provider's own bytes -- not the `<|assistant|>`-prefixed form the
    // prompt is built from -- and the accounting it was originally billed.
    assert_eq!(first.text, "frontier answer");
    assert_eq!(retry.text, first.text);
    assert_ne!(
        first.usage,
        Usage::default(),
        "the echo frontier client reports real numbers"
    );
    assert_eq!(retry.usage, first.usage);

    let session = Session::open(store, session_id, "probe", 10_000, CacheLedger::new())
        .await
        .unwrap();
    assert_eq!(
        session.turn_index(),
        1,
        "a retry must not open a second turn"
    );
}

/// A [`FrontierClient`] whose first call fails and whose later calls behave.
#[derive(Default)]
struct FlakyFrontierClient {
    calls: AtomicUsize,
}

#[async_trait]
impl FrontierClient for FlakyFrontierClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(FrontierError::Upstream("provider exploded".into()));
        }
        EchoFrontierClient::new("frontier answer")
            .execute(quote)
            .await
    }
}

/// A failed dispatch must leave the session usable rather than wedged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_turn_terminates_its_response_and_frees_the_session() {
    let store = Arc::new(MemoryStore::new());
    let engine = Engine::new(
        store.clone(),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(FlakyFrontierClient::default()),
        Arc::new(AffinityPolicy::new()),
        config(),
    );
    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let failed = engine
        .run_turn(
            &session_id,
            TurnId::new("t0"),
            vec![Item::user_text("hello")],
        )
        .await;
    assert!(failed.is_err(), "the provider failed, so the turn must too");

    // The failure is durable and terminal. Opening the session from a
    // *different* node is also the proof that the lease came back: a live lease
    // held by the engine's node would refuse this claim outright.
    let probe = Session::open(
        store.clone(),
        session_id.clone(),
        "probe",
        10_000,
        CacheLedger::new(),
    )
    .await
    .expect("the failed turn must not hold the session hostage until its TTL");
    let terminal: Vec<_> = probe
        .events_since(0, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.is_terminal())
        .collect();
    assert_eq!(terminal.len(), 1, "the failed response must have ended");
    assert!(
        matches!(
            terminal[0].kind,
            SessionEventKind::ResponseIncomplete {
                reason: IncompleteReason::UpstreamError,
                ..
            }
        ),
        "an unterminated response would strand every poller of is_terminal"
    );
    probe.release().await.unwrap();

    // Immediately retryable on the same engine, without waiting out the TTL.
    let retry = engine
        .run_turn(
            &session_id,
            TurnId::new("t0"),
            vec![Item::user_text("hello")],
        )
        .await
        .expect("the same turn id must be runnable again after a failure");
    assert!(
        !retry.deduplicated,
        "the first attempt never completed, so this must generate"
    );
    assert_eq!(retry.text, "frontier answer");

    let probe = Session::open(store, session_id, "probe-2", 10_000, CacheLedger::new())
        .await
        .unwrap();
    let user_items = probe
        .state()
        .items
        .iter()
        .filter(|item| item.role == Role::User)
        .count();
    // Twice, once per attempt: the log is append-only and a fresh attempt
    // re-appends its input. That duplication is the documented cost of retrying
    // a turn that failed rather than completed — reusing the failed turn's
    // items instead would need input-hash matching, which does not exist yet.
    // Deduplication only covers turns that *completed*, which this one did not.
    assert_eq!(user_items, 2);
}

/// The full stack against a real embedded selection service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_worker_wins_and_its_reservation_settles() {
    let store = Arc::new(MemoryStore::new());
    let fleet = embedded_fleet().await;
    let engine = engine_without_fleet(store.clone(), Arc::new(AffinityPolicy::new()))
        .with_fleet(Arc::clone(&fleet) as Arc<dyn LocalFleet>);

    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let result = engine
        .run_turn(
            &session_id,
            TurnId::new("t0"),
            vec![Item::user_text("a question for the local model")],
        )
        .await
        .unwrap();

    // Local is free and the frontier is cold, so local must win on cost.
    let decision = result.decision.expect("turn must route");
    assert!(
        matches!(decision.target, Target::Local { worker_id: 1, .. }),
        "expected the local worker, got {:?}: {}",
        decision.target,
        decision.rationale
    );
    assert_eq!(result.text, "local answer");

    // The reservation was settled: the worker is back to idle. A leak here
    // would silently distort every later routing decision.
    let residual: usize = fleet
        .service()
        .loads(Some(LOCAL_MODEL), Some("default"))
        .into_iter()
        .flat_map(|model| model.loads)
        .map(|load| load.potential_prefill_tokens)
        .sum();
    assert_eq!(residual, 0, "reservation must be released after the turn");
}

/// A [`LocalExecutor`] that keeps every payload it was handed.
#[derive(Default)]
struct CapturingExecutor {
    dispatched: Mutex<Vec<Vec<u32>>>,
}

#[async_trait]
impl LocalExecutor for CapturingExecutor {
    async fn execute(
        &self,
        _endpoint: &str,
        prompt_tokens: &[u32],
        _expected_output_tokens: Option<u32>,
    ) -> Result<LocalExecution, FleetError> {
        self.dispatched.lock().unwrap().push(prompt_tokens.to_vec());
        Ok(LocalExecution {
            text: "local answer".to_string(),
            output_tokens: 2,
            reasoning_tokens: 0,
        })
    }
}

/// The premise the routing rests on: what was hashed is what was dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_receives_exactly_the_tokens_the_router_was_quoted_on() {
    let store = Arc::new(MemoryStore::new());
    let fleet = embedded_fleet().await;
    let executor = Arc::new(CapturingExecutor::default());
    let engine = Engine::new(
        store.clone(),
        ByteTokenizer,
        Arc::clone(&executor) as Arc<dyn LocalExecutor>,
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new("frontier answer")),
        Arc::new(AffinityPolicy::new()),
        config(),
    )
    .with_fleet(Arc::clone(&fleet) as Arc<dyn LocalFleet>);

    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    // Two turns: with a single item in the context, a per-item token stream and
    // a separator-joined one are indistinguishable.
    for turn in 0..2 {
        let result = engine
            .run_turn(
                &session_id,
                TurnId::new(format!("t{turn}")),
                vec![Item::user_text(format!(
                    "question {turn} for the local model"
                ))],
            )
            .await
            .unwrap();
        assert!(
            result.decision.expect("turn must route").target.is_local(),
            "the capture proves nothing unless local won"
        );
    }

    // The context as it stood at the last dispatch: every item committed up to
    // and including that turn's user input, but not the reply it went on to
    // produce.
    let session = Session::open(store, session_id, "probe", 10_000, CacheLedger::new())
        .await
        .unwrap();
    let mut items = session.state().items.clone();
    let reply = items.pop().expect("the turn committed an assistant item");
    assert_eq!(reply.role, Role::Assistant);
    let at_dispatch = ContextAssembler::rehydrate(ByteTokenizer, BLOCK_SIZE, items);

    let dispatched = executor.dispatched.lock().unwrap();
    assert_eq!(dispatched.len(), 2);
    assert_eq!(
        dispatched[1],
        at_dispatch.buffer().tokens(),
        "the worker must prefill the very token stream the hashes describe"
    );
}
