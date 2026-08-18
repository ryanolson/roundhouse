// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a turn survives.
//!
//! `end_to_end.rs` proves the design's claims about statefulness and routing.
//! This file proves the turn itself holds together for as long as a real model
//! call takes:
//!
//! 1. The lease is renewed while the turn runs, so a call longer than the TTL
//!    is not fenced at its own commit and thrown away.
//! 2. Renewal never outranks fencing: an owner displaced mid-call still loses.
//! 3. A provider that goes silent settles at the turn deadline, which is what
//!    keeps a live-but-stuck owner from renewing forever.
//! 4. Deltas are durable *before* the response completes, so a death mid-answer
//!    leaves the partial in the log and TTFT is derivable from it.
//! 5. A stream that breaks halfway commits what it produced, and that partial
//!    is evidence the provider held the prompt.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::{IncompleteReason, SessionEvent, SessionEventKind};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::{Item, Role};
use roundhouse_core::routing::{AffinityPolicy, CacheLedger, Target};
use roundhouse_core::session::{Session, SessionError};
use roundhouse_core::store::{MemoryStore, SessionStore, StoreError};
use roundhouse_fleet::{
    EchoFrontierClient, EmbeddedFleet, FleetError, FrontierChunk, FrontierClient, FrontierError,
    FrontierQuote, FrontierStream, LocalFleet,
};
use roundhouse_server::{
    Admission, EchoLocalExecutor, Engine, EngineConfig, EngineError, LocalExecution, LocalExecutor,
};
use tokio::sync::mpsc;

mod common;
use common::{config, embedded_fleet, frontier_catalog};

/// An engine with a live local option, so a slow executor is on the hot path.
fn engine_with_fleet(
    store: Arc<MemoryStore>,
    fleet: Arc<EmbeddedFleet>,
    executor: Arc<dyn LocalExecutor>,
    config: EngineConfig,
) -> Engine<MemoryStore, ByteTokenizer> {
    Engine::new(
        store,
        ByteTokenizer,
        executor,
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new("frontier answer")),
        Arc::new(AffinityPolicy::new()),
        config,
    )
    .with_fleet(fleet as Arc<dyn LocalFleet>)
}

/// A [`LocalExecutor`] that takes as long as a real model call.
struct SlowLocalExecutor {
    delay: Duration,
}

#[async_trait]
impl LocalExecutor for SlowLocalExecutor {
    async fn execute(
        &self,
        _endpoint: &str,
        _prompt_tokens: &[u32],
        _expected_output_tokens: Option<u32>,
    ) -> Result<LocalExecution, FleetError> {
        tokio::time::sleep(self.delay).await;
        Ok(LocalExecution {
            text: "local answer".to_string(),
            output_tokens: 2,
            reasoning_tokens: 0,
        })
    }
}

/// Poll the log until an event matches, or give up.
///
/// Polling the store rather than awaiting the turn is the point: what has to be
/// observed is the log *during* a response, which nothing that resolves when the
/// turn ends can show.
async fn await_event(
    store: &MemoryStore,
    session_id: &SessionId,
    predicate: impl Fn(&SessionEvent) -> bool,
) -> SessionEvent {
    for _ in 0..300 {
        let found = store
            .read_events(session_id, 0, 1000)
            .await
            .unwrap()
            .into_iter()
            .find(&predicate);
        if let Some(event) = found {
            return event;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the awaited event never reached the log");
}

/// A model call routinely outlasts the lease TTL; the turn must survive it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_longer_than_the_lease_ttl_still_commits() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine_with_fleet(
        store.clone(),
        embedded_fleet().await,
        Arc::new(SlowLocalExecutor {
            delay: Duration::from_millis(3_000),
        }),
        EngineConfig {
            // Three times shorter than the call it has to cover. Unrenewed,
            // this lease is gone long before the first delta is appended. The
            // absolute numbers are deliberately generous: the renewal tick is a
            // third of the TTL, and a margin in the hundreds of milliseconds is
            // what keeps a loaded CI scheduler from lapsing a healthy lease.
            lease_ttl_ms: 1_000,
            ..config()
        },
    );
    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let result = engine
        .run_turn(
            &session_id,
            TurnId::new("t0"),
            vec![Item::user_text("a question worth waiting for")],
            &Admission::open(),
        )
        .await
        .expect("a turn longer than the TTL must not be fenced at its own commit");
    assert!(
        result.decision.expect("turn must route").target.is_local(),
        "the delay proves nothing unless the slow executor is the one that ran"
    );
    assert_eq!(result.text, "local answer");

    let probe = Session::open(store, session_id, "probe", 10_000, CacheLedger::new())
        .await
        .unwrap();
    assert!(
        probe
            .events_since(0, 1000)
            .await
            .unwrap()
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::ResponseCompleted { .. })),
        "the answer the client paid for must be in the log"
    );
}

/// Renewal must not outrank fencing: a displaced owner still loses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fenced_owner_cannot_commit_after_takeover() {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(engine_with_fleet(
        store.clone(),
        embedded_fleet().await,
        Arc::new(SlowLocalExecutor {
            delay: Duration::from_millis(1_000),
        }),
        EngineConfig {
            // Far longer than this test runs, so the heartbeat never ticks and
            // what fences this owner is the takeover itself.
            lease_ttl_ms: 30_000,
            ..config()
        },
    ));
    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let running = tokio::spawn({
        let engine = Arc::clone(&engine);
        let session_id = session_id.clone();
        async move {
            engine
                .run_turn(
                    &session_id,
                    TurnId::new("t0"),
                    vec![Item::user_text("a question the owner will not finish")],
                    &Admission::open(),
                )
                .await
        }
    });

    // Mid-execution: sequenced on the log rather than timed. Waiting for the
    // Routed event proves the owner is already inside its model call when the
    // session is declared dead — a fixed sleep would race the dispatch on a
    // loaded scheduler and fence the owner before it had routed at all.
    await_event(&store, &session_id, |event| {
        matches!(event.kind, SessionEventKind::Routed { .. })
    })
    .await;
    store.expire_lease_now(&session_id).await;
    let successor = Session::open(store, session_id, "node-b", 30_000, CacheLedger::new())
        .await
        .expect("an expired lease is claimable");

    let error = running
        .await
        .unwrap()
        .expect_err("a displaced owner must not be able to commit its answer");
    assert!(
        matches!(
            error,
            EngineError::Session(SessionError::Store(StoreError::LeaseLost { .. }))
        ),
        "expected the fence to fail the append, got {error}"
    );
    let events = successor.events_since(0, 1000).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::Routed { .. })),
        "the fence proves nothing unless the displaced owner had already dispatched"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::ResponseCompleted { .. })),
        "the displaced attempt must not have completed a response behind its successor"
    );
}

/// A [`LocalExecutor`] whose first call never returns and whose later ones do.
#[derive(Default)]
struct HangingLocalExecutor {
    calls: AtomicUsize,
}

#[async_trait]
impl LocalExecutor for HangingLocalExecutor {
    async fn execute(
        &self,
        _endpoint: &str,
        _prompt_tokens: &[u32],
        _expected_output_tokens: Option<u32>,
    ) -> Result<LocalExecution, FleetError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        Ok(LocalExecution {
            text: "local answer".to_string(),
            output_tokens: 2,
            reasoning_tokens: 0,
        })
    }
}

/// The bound that makes the heartbeat safe: a hung turn settles rather than
/// renewing forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_dispatch_settles_at_the_deadline() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine_with_fleet(
        store.clone(),
        embedded_fleet().await,
        Arc::new(HangingLocalExecutor::default()),
        EngineConfig {
            turn_deadline_ms: 300,
            ..config()
        },
    );
    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let started = std::time::Instant::now();
    let error = engine
        .run_turn(
            &session_id,
            TurnId::new("t0"),
            vec![Item::user_text("a question nothing answers")],
            &Admission::open(),
        )
        .await
        .expect_err("a turn that produces nothing must not succeed");
    assert!(
        matches!(error, EngineError::TurnDeadline(300)),
        "expected the deadline, got {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline must cut the call short rather than wait it out"
    );

    let probe = Session::open(
        store,
        session_id.clone(),
        "probe",
        10_000,
        CacheLedger::new(),
    )
    .await
    .expect("a settled turn holds nothing hostage");
    let terminal: Vec<_> = probe
        .events_since(0, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.is_terminal())
        .collect();
    assert_eq!(terminal.len(), 1, "the hung response must have ended");
    assert!(matches!(
        terminal[0].kind,
        SessionEventKind::ResponseIncomplete { .. }
    ));
    probe.release().await.unwrap();

    // Immediately retryable: the deadline released the session as well as the
    // response, so the client does not wait out a TTL to try again.
    let retry = engine
        .run_turn(
            &session_id,
            TurnId::new("t0"),
            vec![Item::user_text("a question nothing answers")],
            &Admission::open(),
        )
        .await
        .expect("the same turn id must be runnable again after a deadline");
    assert!(!retry.deduplicated, "the first attempt never completed");
    assert_eq!(retry.text, "local answer");
}

/// Turns on one session must serialize within a node, not interleave.
///
/// The lease deliberately re-grants to its own node so a recovering process is
/// not locked out by its previous life — which means the lease alone cannot
/// stop two concurrent turns in one process from co-owning the log. The
/// engine's per-session gate is what does; this pins it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_turns_on_one_session_serialize_rather_than_interleave() {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(engine_with_fleet(
        store.clone(),
        embedded_fleet().await,
        Arc::new(SlowLocalExecutor {
            delay: Duration::from_millis(100),
        }),
        config(),
    ));
    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let turns: Vec<_> = (0..2)
        .map(|turn| {
            tokio::spawn({
                let engine = Arc::clone(&engine);
                let session_id = session_id.clone();
                async move {
                    engine
                        .run_turn(
                            &session_id,
                            TurnId::new(format!("t{turn}")),
                            vec![Item::user_text(format!("question {turn}"))],
                            &Admission::open(),
                        )
                        .await
                }
            })
        })
        .collect();
    for turn in turns {
        turn.await
            .unwrap()
            .expect("both concurrent turns must succeed");
    }

    let probe = Session::open(store, session_id, "probe", 10_000, CacheLedger::new())
        .await
        .unwrap();
    assert_eq!(probe.turn_index(), 2);
    let events = probe.events_since(0, 1000).await.unwrap();

    // Serialized means the first turn's events all precede the second's: the
    // second admission must come after the first response terminated. An
    // interleaving would put a `turn_started` inside the other turn's window.
    let starts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event.kind, SessionEventKind::TurnStarted { .. }))
        .map(|(index, _)| index)
        .collect();
    let first_terminal = events
        .iter()
        .position(SessionEvent::is_terminal)
        .expect("the first turn terminated");
    assert_eq!(starts.len(), 2);
    assert!(
        starts[1] > first_terminal,
        "the second turn was admitted while the first was still open"
    );
    // And nothing was lost to the contention: contiguous sequence numbers,
    // both answers committed.
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>()
    );
    assert_eq!(
        events.iter().filter(|event| event.is_terminal()).count(),
        2,
        "both responses must have terminated"
    );
}

/// A [`FrontierClient`] whose stream the test feeds one chunk at a time.
struct PacedFrontierClient {
    chunks: Mutex<Option<mpsc::Receiver<Result<FrontierChunk, FrontierError>>>>,
}

impl PacedFrontierClient {
    fn new() -> (Self, mpsc::Sender<Result<FrontierChunk, FrontierError>>) {
        let (sender, receiver) = mpsc::channel(8);
        (
            Self {
                chunks: Mutex::new(Some(receiver)),
            },
            sender,
        )
    }
}

#[async_trait]
impl FrontierClient for PacedFrontierClient {
    async fn execute(&self, _quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        let receiver =
            self.chunks.lock().unwrap().take().ok_or_else(|| {
                FrontierError::Upstream("the paced client answers one turn".into())
            })?;
        Ok(
            futures::stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|chunk| (chunk, receiver))
            })
            .boxed(),
        )
    }
}

/// Genuine streaming: the log leads the response rather than following it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deltas_are_durable_before_the_response_completes() {
    let store = Arc::new(MemoryStore::new());
    let (client, chunks) = PacedFrontierClient::new();
    // No local fleet, so the frontier is the only target and the test controls
    // the pace of the only response.
    let engine = Arc::new(Engine::new(
        store.clone(),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(client),
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let running = tokio::spawn({
        let engine = Arc::clone(&engine);
        let session_id = session_id.clone();
        async move {
            engine
                .run_turn(
                    &session_id,
                    TurnId::new("t0"),
                    vec![Item::user_text("stream it")],
                    &Admission::open(),
                )
                .await
        }
    });

    chunks
        .send(Ok(FrontierChunk::OutputText("first ".into())))
        .await
        .unwrap();

    // Readable from the log while the provider is still mid-answer. This is the
    // whole claim: a process that dies here leaves the partial behind.
    let delta = await_event(&store, &session_id, |event| {
        matches!(
            &event.kind,
            SessionEventKind::OutputTextDelta { text, .. } if text == "first "
        )
    })
    .await;
    assert!(
        !running.is_finished(),
        "the delta must be durable while the stream is still open"
    );

    for part in ["second ", "third"] {
        chunks
            .send(Ok(FrontierChunk::OutputText(part.into())))
            .await
            .unwrap();
    }
    chunks
        .send(Ok(FrontierChunk::Done {
            input_tokens: 40,
            cached_input_tokens: 0,
            output_tokens: 3,
            reasoning_tokens: 0,
        }))
        .await
        .unwrap();
    drop(chunks);

    let result = running.await.unwrap().expect("the stream completed");
    assert_eq!(result.text, "first second third");
    assert_eq!(
        result.usage.output_tokens, 3,
        "the terminating chunk carries the accounting"
    );

    // TTFT is derivable from the log alone: first delta minus the routing that
    // preceded it.
    let routed_at = await_event(&store, &session_id, |event| {
        matches!(event.kind, SessionEventKind::Routed { .. })
    })
    .await
    .at_ms;
    assert!(
        delta.at_ms >= routed_at,
        "a delta cannot predate the decision that produced it"
    );
}

/// What a stream that breaks halfway leaves behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mid_stream_failure_commits_the_partial() {
    let store = Arc::new(MemoryStore::new());
    let (client, chunks) = PacedFrontierClient::new();
    let engine = Arc::new(Engine::new(
        store.clone(),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(client),
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let running = tokio::spawn({
        let engine = Arc::clone(&engine);
        let session_id = session_id.clone();
        async move {
            engine
                .run_turn(
                    &session_id,
                    TurnId::new("t0"),
                    vec![Item::user_text("stream it")],
                    &Admission::open(),
                )
                .await
        }
    });

    chunks
        .send(Ok(FrontierChunk::OutputText("half an ".into())))
        .await
        .unwrap();
    chunks
        .send(Err(FrontierError::Upstream(
            "the provider dropped the connection".into(),
        )))
        .await
        .unwrap();
    drop(chunks);

    let error = running
        .await
        .unwrap()
        .expect_err("a stream that failed must fail its turn");
    assert!(
        matches!(error, EngineError::Frontier(FrontierError::Upstream(_))),
        "expected the provider's own error, got {error}"
    );

    let probe = Session::open(store, session_id, "probe", 10_000, CacheLedger::new())
        .await
        .unwrap();
    let terminal: Vec<_> = probe
        .events_since(0, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.is_terminal())
        .collect();
    assert_eq!(terminal.len(), 1);
    assert!(matches!(
        terminal[0].kind,
        SessionEventKind::ResponseIncomplete {
            reason: IncompleteReason::UpstreamError,
            ..
        }
    ));

    // The partial is committed as an assistant item, which is what a successor
    // resumes from -- and, on the target that produced it, a guaranteed hit.
    let partial: String = probe
        .state()
        .items
        .iter()
        .filter(|item| item.role == Role::Assistant)
        .map(|item| item.content.render())
        .collect();
    assert_eq!(partial, "half an ");

    // A delta cannot exist without a prefill, so the ledger reads the target as
    // warm even though the turn failed.
    let target = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    };
    assert!(
        probe.ledger().state_for(&target).is_some(),
        "a delivered delta is evidence the provider held the prompt"
    );
}
