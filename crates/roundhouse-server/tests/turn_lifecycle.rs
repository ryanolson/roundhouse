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
use roundhouse_core::item::{Item, ItemContent, Role};
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
            // Non-zero and distinct from every other count here, so the
            // assertion below is about *this* field arriving rather than about
            // a default that would have matched anyway.
            cache_write_tokens: 12,
            output_tokens: 3,
            reasoning_tokens: 0,
            provider_reported_cost: None,
            stop_reason: None,
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
    // **Every count on the chunk reaches the log, including the newest one.**
    // The cache-write count is the one field nothing prices yet, so a fold that
    // dropped it would go unnoticed by every dollar assertion in this suite —
    // and the correction it exists to enable (the ledger bills all uncached
    // input at the write rate) would never become possible.
    assert_eq!(
        result.usage.cache_write_tokens, 12,
        "a measured cache write must survive the fold from chunk to Usage"
    );
    assert_eq!(
        result.usage.input_tokens, 40,
        "and it is a component of the input total, never an addend beside it"
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

// ---------------------------------------------------------------------------
// What a tool-calling turn leaves behind (M11.2)
// ---------------------------------------------------------------------------

/// **The fold's ordering contract, asserted at the engine seam where it is
/// decided.**
///
/// A turn that speaks, calls, speaks and calls again produces four items, and
/// the order is not a nicety: the client resends the blocks it was handed and
/// prefix admission compares them positionally, so items committed in any other
/// order fork every tool-using session on its second turn while every turn still
/// answers. The text ahead of a call is therefore committed *at the call*, not
/// at the completion — by then it has already gone out as deltas and an item
/// written afterwards would sit behind the call in the log and ahead of it on
/// the wire.
///
/// Asserted over the raw event stream rather than over the projected items,
/// because the interleaving is a fact about *when* each item was committed and a
/// projection would flatten exactly that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_calling_turn_commits_its_items_in_the_order_it_produced_them() {
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
                    vec![Item::user_text("find main")],
                    &Admission::open(),
                )
                .await
        }
    });

    for chunk in [
        FrontierChunk::OutputText("Let me look.".into()),
        FrontierChunk::ToolCall {
            id: "toolu_01".into(),
            name: "Grep".into(),
            // Not in canonical spelling: the engine stores the form the client's
            // resend will canonicalize to, not the model's own bytes.
            arguments: r#"{"pattern": "fn main", "path": "/src"}"#.into(),
        },
        FrontierChunk::OutputText(" And also:".into()),
        FrontierChunk::ToolCall {
            id: "toolu_02".into(),
            name: "Read".into(),
            arguments: r#"{"path": "/src/main.rs"}"#.into(),
        },
        FrontierChunk::Done {
            input_tokens: 12,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 20,
            reasoning_tokens: 0,
            provider_reported_cost: None,
            stop_reason: Some("tool_use".into()),
        },
    ] {
        chunks.send(Ok(chunk)).await.unwrap();
    }
    drop(chunks);

    let result = running.await.unwrap().expect("the turn completes");
    assert_eq!(
        result.text, "Let me look. And also:",
        "the caller is handed the whole spoken answer, calls excluded"
    );

    let probe = Session::open(store, session_id, "probe", 10_000, CacheLedger::new())
        .await
        .unwrap();
    let events = probe.events_since(0, 1000).await.unwrap();
    let emitted: Vec<&Item> = events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::ItemAppended { item } if item.response_id.is_some() => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(
        emitted
            .iter()
            .map(|item| item.content.clone())
            .collect::<Vec<_>>(),
        vec![
            ItemContent::Text {
                text: "Let me look.".into()
            },
            ItemContent::ToolCall {
                call_id: "toolu_01".into(),
                name: "Grep".into(),
                arguments: r#"{"path":"/src","pattern":"fn main"}"#.into(),
            },
            ItemContent::Text {
                text: " And also:".into()
            },
            ItemContent::ToolCall {
                call_id: "toolu_02".into(),
                name: "Read".into(),
                arguments: r#"{"path":"/src/main.rs"}"#.into(),
            },
        ],
        "{emitted:#?}"
    );

    // The text ahead of the first call was committed *before* it, not batched
    // into the completion — the property a projection over the finished items
    // cannot see. Its sequence number is what says so.
    let seq_of = |content: &ItemContent| {
        events
            .iter()
            .find(|event| {
                matches!(&event.kind, SessionEventKind::ItemAppended { item }
                    if &item.content == content && item.response_id.is_some())
            })
            .expect("the item is in the log")
            .seq
    };
    let completed = events
        .iter()
        .find(|event| matches!(event.kind, SessionEventKind::ResponseCompleted { .. }))
        .expect("the turn completed");
    assert!(
        seq_of(&ItemContent::Text {
            text: "Let me look.".into()
        }) < completed.seq - 1,
        "the run ahead of a call must be durable at the call, not at the end"
    );

    // And the provider's own word for why it stopped reached the terminal event,
    // which is the only way a serve surface — a different task entirely — can
    // read it. (M11.1's F1, reporting half.)
    let SessionEventKind::ResponseCompleted { stop_reason, .. } = &completed.kind else {
        unreachable!("filtered above")
    };
    assert_eq!(stop_reason.as_deref(), Some("tool_use"));
}

/// **A turn whose whole answer was a tool call commits no empty text item.**
///
/// The ordinary agent turn: a model that calls a tool usually says nothing
/// first. Completing it with the empty trailing item every prose turn commits
/// would put a block in the log that never went out on the wire, and the
/// client's next resend — which has no empty block in it — would diverge at
/// exactly that item and fork the session.
///
/// The CONTROL is the turn that genuinely produced nothing: *there* the empty
/// item is right, because that is what both serve surfaces emit for an empty
/// answer and because a response with no item at all is one a successor cannot
/// resume from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_only_turn_commits_no_empty_trailing_item() {
    async fn emitted_items(script: Vec<Result<FrontierChunk, FrontierError>>) -> Vec<Item> {
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
                        vec![Item::user_text("go")],
                        &Admission::open(),
                    )
                    .await
            }
        });
        for chunk in script {
            chunks.send(chunk).await.unwrap();
        }
        drop(chunks);
        running.await.unwrap().expect("the turn completes");
        let probe = Session::open(store, session_id, "probe", 10_000, CacheLedger::new())
            .await
            .unwrap();
        probe
            .state()
            .items
            .iter()
            .filter(|item| item.response_id.is_some())
            .cloned()
            .collect()
    }

    let done = || {
        Ok(FrontierChunk::Done {
            input_tokens: 4,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 4,
            reasoning_tokens: 0,
            provider_reported_cost: None,
            stop_reason: None,
        })
    };

    let call_only = emitted_items(vec![
        Ok(FrontierChunk::ToolCall {
            id: "toolu_01".into(),
            name: "Bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        }),
        done(),
    ])
    .await;
    assert_eq!(
        call_only.len(),
        1,
        "the call is the whole answer; an empty text item beside it is a block \
         no client will resend: {call_only:#?}"
    );
    assert!(matches!(call_only[0].content, ItemContent::ToolCall { .. }));

    // CONTROL: nothing at all still commits the empty assistant item, so the
    // assertion above is about tool calls rather than about empty text.
    let silent = emitted_items(vec![done()]).await;
    assert_eq!(
        silent
            .iter()
            .map(|item| item.content.clone())
            .collect::<Vec<_>>(),
        vec![ItemContent::Text {
            text: String::new()
        }],
        "a turn that produced nothing still says so with one empty item"
    );
}

/// **A stream that dies after emitting a call is still evidence the provider
/// held the prompt.**
///
/// The cache ledger reads that evidence to decide whether a target is warm, and
/// before M11.2 it read it off the partial *text* alone. A turn that spoke,
/// committed that run at a tool-call boundary and then died has an empty partial
/// and every reason to count — inferring the evidence from the string would tell
/// the ledger the prompt never arrived, and the next turn would be priced cold
/// against a provider that is holding it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_after_a_committed_call_still_reads_as_a_warm_provider() {
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
                    vec![Item::user_text("go")],
                    &Admission::open(),
                )
                .await
        }
    });

    // Text, then a call — which commits the text as an item and empties the
    // pending run — and then the connection drops with nothing pending.
    chunks
        .send(Ok(FrontierChunk::OutputText("Looking.".into())))
        .await
        .unwrap();
    chunks
        .send(Ok(FrontierChunk::ToolCall {
            id: "toolu_01".into(),
            name: "Bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        }))
        .await
        .unwrap();
    chunks
        .send(Err(FrontierError::Upstream("connection reset".into())))
        .await
        .unwrap();
    drop(chunks);

    running
        .await
        .unwrap()
        .expect_err("a stream that failed must fail its turn");

    let probe = Session::open(store, session_id, "probe", 10_000, CacheLedger::new())
        .await
        .unwrap();
    let target = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    };
    assert!(
        probe.ledger().state_for(&target).is_some(),
        "a committed call is as much proof of a prefill as a delivered delta"
    );

    // And nothing was committed twice: the text item written at the call is the
    // only copy, not one the partial then repeated.
    let assistant_text: Vec<String> = probe
        .state()
        .items
        .iter()
        .filter_map(|item| match &item.content {
            ItemContent::Text { text } if item.role == Role::Assistant => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        assistant_text,
        vec!["Looking.".to_string()],
        "the run committed at the call must not be committed again as the \
         partial: {assistant_text:#?}"
    );
}
