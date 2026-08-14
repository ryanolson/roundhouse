// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The cache thesis, measured rather than assumed.
//!
//! Every other test in this workspace takes the central premise on faith. The
//! end-to-end suite runs its selection service with `use_kv_events: false`
//! against a worker that publishes nothing, so the indexer's radix tree is
//! permanently empty, overlap is structurally zero, and every local turn is
//! priced at its full prompt length. The one "a warmed target wins" proof there
//! is about the *frontier* ledger — our own model of a provider's cache TTL,
//! not a measurement of any engine's KV cache. The local half of the thesis,
//! the half the whole design exists for, has never been observed to happen.
//!
//! This test observes it. A real mock vLLM scheduler (`dynamo-mocker`) executes
//! the turns with prefix caching enabled and publishes BlockStored events over
//! ZMQ in the engine-native wire format; the embedded selection service
//! subscribes to that socket. Nothing between the two is stubbed, and the
//! tokenizer is a real 32k BPE, so the chain actually under test is
//!
//! ```text
//! text -> BPE ids -> block hashes -> the dispatched prompt -> the engine's
//! block pool -> KV events -> the indexer -> overlap -> effective prefill ->
//! Usage::cached_input_tokens
//! ```
//!
//! A break anywhere along it — a re-tokenization, a block-size mismatch, a
//! hash-space divergence between what we quote on and what the engine caches —
//! shows up here as a repeat turn priced at its full length, which is exactly
//! the failure the rest of the suite cannot see.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dynamo_mocker::common::protocols::{
    DirectRequest, KvEventPublishers, MockEngineArgs, RawKvEventSink,
};
use dynamo_mocker::live::{LiveEngine, LiveEngineConfig};
use dynamo_mocker::services::zmq_events::ZmqKvEventSink;
use roundhouse_core::context::ContextAssembler;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{AffinityPolicy, CacheLedger, DecisionRecord, Target};
use roundhouse_core::session::Session;
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::{
    EchoFrontierClient, EmbeddedFleet, FleetError, FleetQuery, KvRouterConfig, LocalFleet,
    SelectionServiceBuilder, StaticFrontierCatalog, WorkerRegistration,
};
use roundhouse_server::{Engine, EngineConfig, HfTokenizer, LocalExecution, LocalExecutor};

/// One block size for the whole stack. The engine's block pool, the worker
/// registration, the indexer, and the token buffer must agree on it: the
/// indexer drops stored blocks whose event-side `block_size` differs from its
/// own, and a buffer hashed at a different stride produces hashes that can
/// never match.
const BLOCK_SIZE: u32 = 16;
const LOCAL_MODEL: &str = "local";
const ROUTING_GROUP: &str = "default";
const WORKER_ID: u64 = 1;
const REPLY: &str = "mocker answer";

/// Generated tokens per turn. Small on purpose: decode tokens are random ids
/// inside the mock engine, so every block they complete is one the next turn
/// cannot match. Keeping the tail short keeps the unmatched region to the one
/// block that straddles the prompt/reply boundary.
const MAX_OUTPUT_TOKENS: u32 = 8;

/// A [`LocalExecutor`] backed by a real mock vLLM scheduler.
///
/// The point of routing through the mocker rather than [`EchoLocalExecutor`] is
/// the side effect: submitting the prompt makes the engine admit it into a
/// simulated block pool, and the pool publishes the resulting BlockStored
/// events — carrying the very token ids it was handed — to the selection
/// service. The reply text is synthetic because none of the assertions depend
/// on it; what has to be real is the token stream and the cache state it leaves
/// behind.
struct MockerExecutor {
    engine: LiveEngine,
}

#[async_trait]
impl LocalExecutor for MockerExecutor {
    async fn execute(
        &self,
        _endpoint: &str,
        prompt_tokens: &[u32],
        expected_output_tokens: Option<u32>,
    ) -> Result<LocalExecution, FleetError> {
        // `prompt_tokens` is passed through verbatim. Re-deriving it from text
        // here would be the exact defect this file exists to rule out: the
        // engine would cache blocks of a different token stream from the one
        // the turn was quoted on, and the second turn would match nothing.
        let mut request = self
            .engine
            .submit(DirectRequest {
                tokens: prompt_tokens.to_vec(),
                max_output_tokens: expected_output_tokens.unwrap_or(MAX_OUTPUT_TOKENS).max(1)
                    as usize,
                uuid: Some(uuid::Uuid::new_v4()),
                dp_rank: 0,
                ..Default::default()
            })
            .await
            .map_err(FleetError::Other)?;

        let mut output_tokens = 0u64;
        loop {
            let Some(signal) = request.recv().await else {
                return Err(FleetError::Rejected(
                    "mock engine closed the output stream before completing the request".into(),
                ));
            };
            if signal.token_id.is_some() {
                output_tokens += 1;
            }
            if signal.completed {
                // A rejection means the request never ran, so it also never
                // warmed anything. Failing loudly beats a later assertion
                // failing for a reason that looks like a cache miss.
                if signal.rejected {
                    return Err(FleetError::Rejected(
                        "mock engine rejected the request: its footprint exceeds the KV pool"
                            .into(),
                    ));
                }
                break;
            }
        }

        Ok(LocalExecution {
            text: REPLY.to_string(),
            output_tokens,
            reasoning_tokens: 0,
        })
    }
}

/// A localhost port nothing else holds.
///
/// Binding and immediately dropping leaves a window, but it is the only way to
/// let the OS pick: the ZMQ publisher binds by port number, and a hard-coded
/// one would make concurrent test binaries collide.
fn reserve_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("a localhost port must be available");
    let port = listener
        .local_addr()
        .expect("a bound listener has a local address")
        .port();
    drop(listener);
    port
}

/// Start the mock engine with its KV events published on `port`.
///
/// `block_size` has no default worth relying on — [`MockEngineArgs`] leaves it
/// at 0, which the validator rejects — and it has to equal [`BLOCK_SIZE`] on
/// both the sink and the engine, since the sink asserts that each event's token
/// ids divide evenly into its blocks.
async fn mocker_engine(port: u16) -> LiveEngine {
    let sink = ZmqKvEventSink::new(port, None, 0, BLOCK_SIZE)
        .await
        .expect("the ZMQ KV event sink must bind its PUB socket");
    let publishers = KvEventPublishers::new(None, Some(Arc::new(sink) as Arc<dyn RawKvEventSink>));

    let args = MockEngineArgs::builder()
        .block_size(BLOCK_SIZE as usize)
        // Far more than these turns need: an eviction would retract the blocks
        // the second turn is supposed to match, turning a cache proof into a
        // capacity test.
        .num_gpu_blocks(4096)
        .enable_prefix_caching(true)
        // The mocker models real timings; this collapses them so the test
        // spends its time on the KV path rather than on simulated decode.
        .speedup_ratio(100.0)
        .build()
        .expect("mock engine args must validate");

    LiveEngine::start_with_config(
        args,
        0,
        LiveEngineConfig {
            kv_event_publishers: publishers,
            ..Default::default()
        },
    )
    .expect("the mock engine must start")
}

/// An embedded selection service subscribed to the mock engine's KV events.
///
/// `use_kv_events: true` is the whole difference from the fleet the other
/// end-to-end tests build. It is what makes `upsert_worker` spawn the SUB
/// listener for each registered dp rank; without an endpoint for every rank in
/// the worker's range the worker would register as incomplete and never be
/// schedulable at all.
async fn kv_event_fleet(port: u16) -> Arc<EmbeddedFleet> {
    let service = SelectionServiceBuilder::new(KvRouterConfig {
        use_kv_events: true,
        router_queue_threshold: None,
        ..Default::default()
    })
    .indexer_threads(1)
    .build()
    .await
    .expect("selection service should start");

    let fleet = Arc::new(EmbeddedFleet::new(Arc::new(service)));
    fleet
        .register_worker(WorkerRegistration {
            worker_id: WORKER_ID,
            model_name: LOCAL_MODEL.to_string(),
            routing_group: ROUTING_GROUP.to_string(),
            endpoint: "http://mocker-1:8000".to_string(),
            block_size: BLOCK_SIZE,
            kv_events_endpoints: HashMap::from([(0u32, format!("tcp://127.0.0.1:{port}"))]),
        })
        .await
        .expect("the mock worker must register");
    fleet
}

/// The TinyLlama BPE, the same fixture the tokenizer unit tests use.
///
/// A real vocabulary is load-bearing here rather than incidental: under a byte
/// tokenizer, hashing and dispatch agree trivially, and the interesting failure
/// — merges that differ depending on what a string is encoded alongside —
/// cannot occur.
fn tokenizer() -> HfTokenizer {
    HfTokenizer::from_file(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/tinyllama-tokenizer.json"),
    )
    .expect("fixture tokenizer must load")
}

fn config() -> EngineConfig {
    EngineConfig {
        block_size: BLOCK_SIZE,
        local_model: LOCAL_MODEL.to_string(),
        routing_group: ROUTING_GROUP.to_string(),
        expected_output_tokens: MAX_OUTPUT_TOKENS,
        ..Default::default()
    }
}

/// A prompt long enough that a whole-prompt prefill is unmistakably different
/// from a delta-sized one.
///
/// Each sentence carries different numbers so no two blocks of it are
/// identical: a prompt built from a repeated phrase would produce repeating
/// block hashes, and a match against it would prove only that the same block
/// was stored once, not that a prefix chain was reconstructed.
fn long_opening_message() -> String {
    (0..24)
        .map(|section: u32| {
            format!(
                "Section {section}: the {section} regional deployment reported {} anomalies, \
                 {} retries, and a p99 latency of {} milliseconds over the last {} hours. \
                 Summarize the trend and state whether it stays within the error budget. ",
                section * 7 + 3,
                section * 11 + 5,
                section * 13 + 41,
                section + 2,
            )
        })
        .collect()
}

/// Poll the fleet until the engine's KV events for `items` have been indexed.
///
/// Sleeping a fixed interval instead would trade a flaky test for a slow one.
/// This asks the only question that matters — does the router now believe this
/// prefix is resident? — using a query-only `price` call, which books no load
/// and whose quote simply expires unclaimed.
///
/// The bar is `minimum` matched tokens rather than merely nonzero, because a
/// partially delivered batch would satisfy "nonzero" while leaving the next
/// turn to match fewer blocks than it should. Waiting for the count the next
/// turn's assertions assume removes that race instead of losing to it.
async fn wait_for_indexed_prefix(
    fleet: &Arc<EmbeddedFleet>,
    items: Vec<Item>,
    minimum: u32,
) -> u32 {
    let assembler = ContextAssembler::rehydrate(tokenizer(), BLOCK_SIZE, items);
    // Built exactly as the engine builds it, so the hashes probed here are the
    // hashes the next turn will be priced on.
    let query = FleetQuery::for_buffer(
        assembler.buffer(),
        LOCAL_MODEL,
        ROUTING_GROUP,
        Some(MAX_OUTPUT_TOKENS),
        None,
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut best = 0;
    loop {
        if let Some(quote) = fleet
            .price(&query)
            .await
            .expect("pricing a warm prefix must not fail")
        {
            best = best.max(quote.longest_matched_tokens);
            if quote.longest_matched_tokens >= minimum {
                return quote.longest_matched_tokens;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the mock engine's KV events never reached the indexer: after 15s the router matched \
             {best} tokens of the {}-token prefix it had just prefilled, expected at least \
             {minimum}",
            query.isl_tokens
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Every routing decision the session recorded, in order.
async fn routed_records(store: Arc<MemoryStore>, session_id: &SessionId) -> Vec<DecisionRecord> {
    let session = Session::open(
        store,
        session_id.clone(),
        "probe",
        10_000,
        CacheLedger::new(),
    )
    .await
    .expect("the session must be readable after both turns");
    let records = session
        .events_since(0, 1000)
        .await
        .expect("the log must replay")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .collect();
    session.release().await.expect("the probe must release");
    records
}

/// The measurement the whole design is for: a repeat turn against a worker that
/// already holds its prefix is priced far below its own prompt length.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_warmed_worker_prices_a_repeat_turn_far_below_its_prompt_length() {
    let port = reserve_port();
    let engine = mocker_engine(port).await;
    let fleet = kv_event_fleet(port).await;

    // ZMQ's slow joiner: a PUB socket drops everything published before a
    // subscriber has finished connecting, and those events are gone for good
    // without a replay endpoint. The listener is spawned by `register_worker`,
    // so this waits for its SUB to land before any block is ever stored.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let store = Arc::new(MemoryStore::new());
    let turn_engine = Engine::new(
        Arc::clone(&store),
        tokenizer(),
        Arc::new(MockerExecutor { engine }) as Arc<dyn LocalExecutor>,
        // No frontier models at all: local is the only candidate, so the
        // decisions below are about cache state and nothing else.
        StaticFrontierCatalog::new(vec![]),
        Arc::new(EchoFrontierClient::new("frontier answer")),
        Arc::new(AffinityPolicy::new()),
        config(),
    )
    .with_fleet(Arc::clone(&fleet) as Arc<dyn LocalFleet>);

    let session_id = SessionId::generate();
    turn_engine
        .create_session(&session_id)
        .await
        .expect("session creation must succeed");

    // --- turn 1: a cold worker, so the whole prompt is prefill --------------
    let first = turn_engine
        .run_turn(
            &session_id,
            TurnId::new("t0"),
            vec![Item::user_text(long_opening_message())],
        )
        .await
        .expect("the first turn must run");
    assert_eq!(first.text, REPLY, "the mock engine must have answered");
    assert!(
        first
            .decision
            .as_ref()
            .expect("the first turn must route")
            .target
            .is_local()
    );

    // --- between turns: wait for the engine's events to be indexed ----------
    let committed = {
        let session = Session::open(
            Arc::clone(&store),
            session_id.clone(),
            "probe",
            10_000,
            CacheLedger::new(),
        )
        .await
        .expect("the session must be readable between turns");
        let items = session.state().items.clone();
        session.release().await.expect("the probe must release");
        items
    };
    // Every complete block of the first turn's prompt should become resident;
    // allow the one that the engine finished with its own generated tokens.
    let warm_blocks = first.usage.input_tokens as usize / BLOCK_SIZE as usize;
    let matched_between_turns = wait_for_indexed_prefix(
        &fleet,
        committed,
        ((warm_blocks - 1) * BLOCK_SIZE as usize) as u32,
    )
    .await;

    // --- turn 2: a short follow-up on a warm worker -------------------------
    let second = turn_engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            vec![Item::user_text(
                "Given all of that, which region should we look at first?",
            )],
        )
        .await
        .expect("the second turn must run");

    let routed = routed_records(Arc::clone(&store), &session_id).await;
    assert_eq!(routed.len(), 2, "each turn records exactly one decision");
    let (cold, warm) = (&routed[0], &routed[1]);

    // A cold engine prices the whole prompt. This is the baseline the headline
    // claim is measured against, and it is what every other test in this
    // workspace is stuck at.
    assert_eq!(
        cold.expected_prefill_tokens, cold.isl_tokens as f64,
        "an unwarmed worker must be priced at its full prompt length"
    );
    assert!(
        cold.isl_tokens >= 400,
        "the opening message must be long enough for the saving to be unambiguous, got {} tokens",
        cold.isl_tokens
    );

    assert!(
        matches!(
            warm.chosen,
            Target::Local {
                worker_id: WORKER_ID,
                ..
            }
        ),
        "the warm turn must land on the same local worker, got {:?}",
        warm.chosen
    );

    // The headline: the second turn's prompt is *longer* than the first's, and
    // it is priced at less than half of itself.
    assert!(
        warm.expected_prefill_tokens < 0.5 * warm.isl_tokens as f64,
        "a warmed worker must price a repeat turn far below its prompt length: \
         {} prefill tokens against an {}-token prompt (cold turn was {} of {})",
        warm.expected_prefill_tokens,
        warm.isl_tokens,
        cold.expected_prefill_tokens,
        cold.isl_tokens,
    );

    // Sharper, and the reason the number is what it is: a matched device block
    // removes exactly `block_size` tokens from the effective prefill. Every
    // complete block of the first turn's context should match, so the only
    // slack allowed is the single block straddling the prompt/reply boundary,
    // which the engine completed with its own generated tokens.
    assert_eq!(warm_blocks, cold.isl_tokens as usize / BLOCK_SIZE as usize);
    let ceiling = warm
        .isl_tokens
        .saturating_sub(((warm_blocks - 1) * BLOCK_SIZE as usize) as u64) as f64;
    assert!(
        warm.expected_prefill_tokens <= ceiling,
        "matched blocks must each cut {BLOCK_SIZE} tokens of prefill: expected at most {ceiling} \
         from {} complete warm blocks, got {}",
        warm_blocks,
        warm.expected_prefill_tokens,
    );

    // The same fact from the candidate's own view, before any weighting.
    let local_candidate = warm
        .considered
        .iter()
        .find(|candidate| candidate.target.is_local())
        .expect("the local worker must have been considered");
    assert!(
        local_candidate.matched_prefix_tokens > 0,
        "the considered local candidate must report a matched prefix"
    );

    // And the metric the design exists to maximize, nonzero for the first time.
    assert!(
        second.usage.cached_input_tokens > 0,
        "a warm turn must bill part of its input as cached"
    );
    assert_eq!(
        second.usage.input_tokens, warm.isl_tokens,
        "usage must be reported against the prompt that was actually priced"
    );
    assert!(second.usage.cached_input_tokens < second.usage.input_tokens);
    assert!(
        matched_between_turns > 0,
        "the between-turns probe is what proved the events landed"
    );

    // Both reservations settled. A leak here would inflate this worker's load
    // permanently, and every later decision would be made against a fiction.
    let residual: usize = fleet
        .service()
        .loads(Some(LOCAL_MODEL), Some(ROUTING_GROUP))
        .into_iter()
        .flat_map(|model| model.loads)
        .map(|load| load.potential_prefill_tokens)
        .sum();
    assert_eq!(
        residual, 0,
        "every reservation must be released once its turn ends"
    );
}
