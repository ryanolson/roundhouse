// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the dashboard reports is what actually happened.
//!
//! The metrics fold is unit-tested against hand-built logs next to its own
//! source. What that cannot establish is the wiring: that turns driven through
//! the real engine reach the recorder at all, that a provider which withholds
//! its accounting is *marked* rather than counted as free, and that a node
//! which restarts recovers a session's history instead of reporting only what
//! it served since booting. Those are the claims here, and each one fails
//! silently in production — a dashboard reading zero looks exactly like a fleet
//! that saved nothing.

use std::sync::Arc;

use async_trait::async_trait;
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::Principal;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::metrics::{
    MetricsConfig, MetricsFold, MetricsSnapshot, ServingMode, ShadowPricing,
};
use roundhouse_core::routing::{AffinityPolicy, RoutingPolicy};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierChunk, FrontierClient, FrontierError, FrontierQuote, FrontierStream,
};
use roundhouse_server::{EchoLocalExecutor, Engine, EngineConfig};

mod common;
use common::{config, frontier_catalog};

/// The catalog's own prices, so the dashboard and the router agree by
/// construction rather than by a second copy kept in step by hand.
fn metrics_config() -> MetricsConfig {
    MetricsConfig::new(frontier_catalog().shadow_pricing())
}

fn engine(
    store: Arc<MemoryStore>,
    frontier: Arc<dyn FrontierClient>,
) -> Engine<MemoryStore, ByteTokenizer> {
    Engine::new(
        store,
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        frontier,
        Arc::new(AffinityPolicy::new()) as Arc<dyn RoutingPolicy>,
        config(),
    )
}

async fn run_turns(engine: &Engine<MemoryStore, ByteTokenizer>, session: &SessionId, turns: usize) {
    engine.create_session(session).await.unwrap();
    for turn in 0..turns {
        engine
            .run_turn(
                session,
                TurnId::new(format!("turn-{turn}")),
                vec![Item::user_text(format!("question {turn}"))],
                &Principal::default_open(),
            )
            .await
            .expect("the turn completed");
    }
}

fn snapshot(engine: &Engine<MemoryStore, ByteTokenizer>) -> MetricsSnapshot {
    engine.metrics().snapshot(&metrics_config(), 0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turns_driven_through_the_engine_reach_the_dashboard() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(store, Arc::new(EchoFrontierClient::new("frontier answer")));
    let session = SessionId::new("s-metrics");

    run_turns(&engine, &session, 3).await;
    let snapshot = snapshot(&engine);

    assert_eq!(snapshot.sessions, 1);
    assert_eq!(snapshot.turns, 3);
    assert_eq!(snapshot.calls, 3);
    assert!(snapshot.tokens.input > 0, "prompts were billed");
    assert!(snapshot.tokens.output > 0, "answers were billed");
    assert_eq!(
        snapshot.tokens.total,
        snapshot.tokens.input + snapshot.tokens.output,
        "the total is input plus output, with cached and reasoning inside them"
    );

    // Both rollup axes cover exactly the same calls, because they are two
    // groupings of one set of events rather than two measurements.
    let by_provider: u64 = snapshot.providers.iter().map(|p| p.totals.calls).sum();
    let by_mode: u64 = snapshot.serving_modes.iter().map(|m| m.totals.calls).sum();
    assert_eq!(by_provider, snapshot.calls);
    assert_eq!(by_mode, snapshot.calls);
    assert_eq!(
        snapshot.serving_modes.len(),
        2,
        "both serving modes are always shown, even at zero calls"
    );
}

/// A deduplicated retry must not be billed twice.
///
/// The client already paid for that answer, and the log holds the accounting it
/// was billed under. A retry after a dropped connection replaying as a second
/// call would inflate every figure on the dashboard in exactly the conditions —
/// a flaky network — where someone is most likely to be watching it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_turn_is_not_counted_twice() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(store, Arc::new(EchoFrontierClient::new("frontier answer")));
    let session = SessionId::new("s-retry");
    engine.create_session(&session).await.unwrap();

    let turn = TurnId::new("the-same-turn");
    let input = vec![Item::user_text("only asked once")];
    engine
        .run_turn(
            &session,
            turn.clone(),
            input.clone(),
            &Principal::default_open(),
        )
        .await
        .unwrap();
    let after_first = snapshot(&engine);

    let replay = engine
        .run_turn(&session, turn, input, &Principal::default_open())
        .await
        .unwrap();
    assert!(replay.deduplicated, "the retry was served from the log");

    let after_retry = snapshot(&engine);
    assert_eq!(after_retry.calls, after_first.calls);
    assert_eq!(after_retry.tokens, after_first.tokens);
}

/// A provider that streams an answer and no usage.
///
/// The default behaviour of a streaming OpenAI-compatible endpoint whose
/// request never set `stream_options.include_usage` — which is to say, the
/// behaviour of any client request Roundhouse forwards without rewriting.
struct SilentFrontier;

#[async_trait]
impl FrontierClient for SilentFrontier {
    async fn execute(&self, _quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        use futures::StreamExt;
        Ok(futures::stream::iter([Ok(FrontierChunk::OutputText("an answer".into()))]).boxed())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_that_reports_no_usage_is_marked_not_counted_as_free() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(store, Arc::new(SilentFrontier));
    let session = SessionId::new("s-silent");

    run_turns(&engine, &session, 2).await;
    let snapshot = snapshot(&engine);

    assert_eq!(snapshot.calls, 2);
    assert_eq!(
        snapshot.coverage.estimated_calls, 2,
        "a silent provider must show up as an accounting gap"
    );
    assert_eq!(snapshot.coverage.reported_calls, 0);
    assert!(
        snapshot.coverage_fraction < 1.0,
        "the dashboard must not claim full coverage it does not have"
    );

    // The gap is filled from what we do know rather than left at zero: zero
    // tokens for zero dollars on a hosted model is indistinguishable from a
    // saving, which is the failure this whole path exists to prevent.
    assert!(
        snapshot.tokens.input > 0,
        "the prompt we tokenized and routed on is still a known quantity"
    );
    assert!(
        snapshot.tokens.output > 0,
        "the answer we received is countable"
    );
    assert_eq!(
        snapshot.tokens.cached_input, 0,
        "nothing observable here bears on what a remote cache did"
    );
}

/// A restarted node recovers a session's accounting by replaying its log.
///
/// The engine hands the recorder the replay as well as new commits, and the
/// fold is idempotent by `(session, seq)`, so picking a session back up
/// restores its history exactly once. Without both halves a node that
/// restarted would either under-report — showing only what it served since
/// booting — or double-count every session that takes more than one turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_node_recovers_a_sessions_history_exactly_once() {
    let store = Arc::new(MemoryStore::new());
    let session = SessionId::new("s-restart");

    let first = engine(
        Arc::clone(&store),
        Arc::new(EchoFrontierClient::new("frontier answer")),
    );
    run_turns(&first, &session, 2).await;
    let before = snapshot(&first);
    assert_eq!(before.calls, 2);

    // A new process over the same store: fresh recorder, nothing in memory.
    let successor = engine(
        Arc::clone(&store),
        Arc::new(EchoFrontierClient::new("frontier answer")),
    );
    assert_eq!(snapshot(&successor).calls, 0, "a fresh node starts empty");

    // Serving one more turn opens the session, which replays its log.
    successor
        .run_turn(
            &session,
            TurnId::new("turn-after-restart"),
            vec![Item::user_text("carry on")],
            &Principal::default_open(),
        )
        .await
        .unwrap();

    let after = snapshot(&successor);
    assert_eq!(
        after.calls, 3,
        "two recovered from the log plus one newly served, each counted once"
    );
    assert_eq!(after.turns, 3);
    assert_eq!(after.sessions, 1);
}

/// The live numbers equal a cold rebuild from the log.
///
/// The property the whole projection design rests on: metrics are derived from
/// the log rather than recorded beside it, so a fold of the stored events must
/// reproduce what the running process has been reporting. If these ever differ,
/// the dashboard has become a second source of truth.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_live_numbers_match_a_cold_rebuild_from_the_log() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        Arc::clone(&store),
        Arc::new(EchoFrontierClient::new("frontier answer")),
    );

    let sessions = [SessionId::new("s-a"), SessionId::new("s-b")];
    for session in &sessions {
        run_turns(&engine, session, 2).await;
    }
    let live = snapshot(&engine);

    let mut rebuilt = MetricsFold::new();
    for session in &sessions {
        let mut cursor = 0;
        loop {
            let batch = store.read_events(session, cursor, 256).await.unwrap();
            if batch.is_empty() {
                break;
            }
            cursor = batch.last().unwrap().seq;
            rebuilt.extend(&batch);
        }
    }
    let rebuilt = MetricsSnapshot::build(&rebuilt, &metrics_config(), 0);

    assert_eq!(live.calls, rebuilt.calls);
    assert_eq!(live.turns, rebuilt.turns);
    assert_eq!(live.sessions, rebuilt.sessions);
    assert_eq!(live.tokens, rebuilt.tokens);
    assert_eq!(live.coverage, rebuilt.coverage);
    assert_eq!(
        live.savings.frontier_spend_usd, rebuilt.savings.frontier_spend_usd,
        "the money must fold out of the log too"
    );
}

/// Local traffic with no comparable hosted model contributes no saving.
///
/// The conservative direction, and the one that matters: an unpriced correlary
/// silently defaulting to the nearest rate card would let a 7B model's traffic
/// be billed as a flagship's and turn the headline figure into fiction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_traffic_with_no_correlary_is_reported_unpriced() {
    let store = Arc::new(MemoryStore::new());
    let engine = Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new("frontier answer")),
        Arc::new(AffinityPolicy::new()) as Arc<dyn RoutingPolicy>,
        EngineConfig {
            local_model: "tiny-7b".to_string(),
            ..config()
        },
    );
    let session = SessionId::new("s-unpriced");
    run_turns(&engine, &session, 1).await;

    // The catalog's only hosted model is far above a 7B's capability, so
    // nothing passes the gate.
    let config = MetricsConfig::new(ShadowPricing::new(
        frontier_catalog().shadow_pricing().references().to_vec(),
    ))
    .with_local_quality("tiny-7b", 0.30);
    let snapshot = engine.metrics().snapshot(&config, 0);

    let local: Vec<_> = snapshot
        .models
        .iter()
        .filter(|m| m.mode() == ServingMode::Local)
        .collect();
    for model in local {
        assert_eq!(
            model.shadow_usd(),
            0.0,
            "a model with no defensible stand-in must not be shadow-priced"
        );
    }
}
