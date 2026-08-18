// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The durability claim, end to end: the engine over the Redis store.
//!
//! Everything below the trait is already conformance-tested in
//! `roundhouse-store-redis`; what this binary proves is the composition M4
//! ships — the real engine driving `RedisSessionStore` — and the one property
//! no `MemoryStore` test can state: a session outliving the process that
//! created it. Two separate store connections stand in for two processes,
//! and the second continues — and deduplicates against — a conversation it
//! never saw created.
//!
//! Gated like the store's own integration tests: `#[ignore]` because it is
//! the one skip the harness reports, opted into with `--include-ignored`, and
//! a missing `ROUNDHOUSE_TEST_REDIS_URL` then fails loudly.

use std::sync::Arc;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::Principal;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::{Item, Role};
use roundhouse_core::routing::{AffinityPolicy, CacheLedger};
use roundhouse_core::session::Session;
use roundhouse_fleet::EchoFrontierClient;
use roundhouse_server::{EchoLocalExecutor, Engine, EngineConfig};
use roundhouse_store_redis::RedisSessionStore;
use roundhouse_store_redis::test_support::connect_from_env;

mod common;
use common::{config, frontier_catalog};

async fn store_from_env() -> Arc<RedisSessionStore> {
    Arc::new(connect_from_env().await)
}

/// The offline-demo engine shape from `main.rs`, over Redis instead.
fn engine_over(
    store: Arc<RedisSessionStore>,
    node_id: &str,
) -> Engine<RedisSessionStore, ByteTokenizer> {
    Engine::new(
        store,
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new("frontier answer")),
        Arc::new(AffinityPolicy::new()),
        EngineConfig {
            node_id: node_id.to_string(),
            ..config()
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_session_survives_the_process_that_created_it() {
    let session_id = SessionId::generate();

    // The first "process" serves three turns and dies (drops).
    {
        let engine = engine_over(store_from_env().await, "node-first");
        engine.create_session(&session_id).await.unwrap();
        for turn in 0..3 {
            let result = engine
                .run_turn(
                    &session_id,
                    TurnId::new(format!("turn-{turn}")),
                    vec![Item::user_text(format!("Step {turn}"))],
                    &Principal::default_open(),
                )
                .await
                .unwrap();
            assert!(!result.deduplicated);
        }
    }

    // The successor connects fresh: nothing in memory, everything in Redis.
    let store = store_from_env().await;
    let successor = engine_over(Arc::clone(&store), "node-second");

    // It can extend the conversation…
    let fourth = successor
        .run_turn(
            &session_id,
            TurnId::new("turn-3"),
            vec![Item::user_text("Step 3")],
            &Principal::default_open(),
        )
        .await
        .unwrap();
    assert!(!fourth.deduplicated);

    // …and a retry of a turn the *dead* process completed is recognized and
    // served from the log rather than generated twice. Idempotency across a
    // process boundary is the durability claim in one line.
    let replayed = successor
        .run_turn(
            &session_id,
            TurnId::new("turn-1"),
            vec![Item::user_text("Step 1")],
            &Principal::default_open(),
        )
        .await
        .unwrap();
    assert!(
        replayed.deduplicated,
        "a completed turn must not be generated twice, even by a successor"
    );

    // The projection the successor rebuilt is the full conversation: four
    // user items and four assistant answers, in order.
    let session = Session::open(
        store,
        session_id.clone(),
        "probe",
        10_000,
        CacheLedger::new(),
    )
    .await
    .unwrap();
    let items = &session.state().items;
    let users: Vec<_> = items
        .iter()
        .filter(|item| item.role == Role::User)
        .map(|item| item.content.render())
        .collect();
    assert_eq!(users, ["Step 0", "Step 1", "Step 2", "Step 3"]);
    assert_eq!(
        items
            .iter()
            .filter(|item| item.role == Role::Assistant)
            .count(),
        4,
        "each turn's answer must have survived, including the dead process's"
    );
    session.release().await.unwrap();
}
