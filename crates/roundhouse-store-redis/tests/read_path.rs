// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The M2 read path against a real Redis.
//!
//! These tests need infrastructure, and the gate is `#[ignore]` because that
//! is the one skip the test harness *reports*: a plain `cargo test` prints
//! `5 ignored` with the reason beside each name, which is the truth. The
//! tempting alternative — an env-var check that returns early — reports
//! "passed" for tests that verified nothing, and a suite that overstates
//! what ran is worse than one that visibly did not run.
//!
//! To run them: `ROUNDHOUSE_TEST_REDIS_URL=redis://… cargo test -p
//! roundhouse-store-redis -- --include-ignored`. Asking for them without the
//! variable fails loudly rather than skipping again — `--include-ignored` is
//! an explicit request for the real backend. Every test mints fresh session
//! ids, so pointing at a shared or long-lived Redis instance is safe.
//!
//! Writes here go through [`raw_append`], which produces byte-for-byte the
//! entries the M3 fenced append script will produce (explicit `<seq>-0` ids,
//! `at_ms` and `kind` fields). That is the point of M2: prove the read side of
//! the wire format before the write side exists. The full store contract suite
//! takes over in M3 when appends are real.

use roundhouse_core::event::{Accounting, IncompleteReason, SessionEvent, SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{Candidate, DecisionRecord, Target};
use roundhouse_core::store::{SessionStore, StoreError, contract};
use roundhouse_store_redis::{RedisSessionStore, RedisStoreConfig};

const URL_VAR: &str = "ROUNDHOUSE_TEST_REDIS_URL";

/// The store under test. Reaching this at all means `--include-ignored` asked
/// for the real backend, so a missing variable is a runner error to report,
/// not a skip.
async fn store() -> (RedisSessionStore, RedisStoreConfig, String) {
    let url = std::env::var(URL_VAR).unwrap_or_else(|_| {
        panic!("--include-ignored asks for the real backend; set {URL_VAR} to a reachable Redis")
    });
    let config = RedisStoreConfig::new(&url);
    let store = RedisSessionStore::connect(config.clone())
        .await
        .expect("Redis named by the env var must be reachable");
    (store, config, url)
}

/// Append entries exactly as the M3 fenced script will write them.
async fn raw_append(
    url: &str,
    config: &RedisStoreConfig,
    session_id: &SessionId,
    first_seq: u64,
    kinds: Vec<SessionEventKind>,
) -> Vec<SessionEvent> {
    let client = redis::Client::open(url).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();

    let mut events = Vec::with_capacity(kinds.len());
    for (offset, kind) in kinds.into_iter().enumerate() {
        let seq = first_seq + offset as u64;
        // Deterministic timestamps so equality below asserts the store read
        // back what was written, not something it re-stamped.
        let at_ms = 1_700_000_000_000 + seq;
        let _: String = redis::cmd("XADD")
            .arg(config.log_key(session_id))
            .arg(format!("{seq}-0"))
            .arg("at_ms")
            .arg(at_ms)
            .arg("kind")
            .arg(serde_json::to_string(&kind).unwrap())
            .query_async(&mut conn)
            .await
            .unwrap();
        events.push(SessionEvent {
            seq,
            session_id: session_id.clone(),
            at_ms,
            kind,
        });
    }
    events
}

/// One of every event kind, so the field-wise persistence proves it can take
/// apart and reassemble the whole vocabulary, not just the easy variants.
fn every_event_kind() -> Vec<SessionEventKind> {
    let response_id = ResponseId::generate();
    let local = Target::Local {
        worker_id: 7,
        dp_rank: 1,
        model: "llama".into(),
    };
    let frontier = Target::Frontier {
        provider: "anthropic".into(),
        model: "claude-sonnet-5".into(),
    };
    vec![
        SessionEventKind::SessionCreated {
            model_policy: "affinity".into(),
        },
        SessionEventKind::TurnStarted {
            turn_id: TurnId::generate(),
            response_id: response_id.clone(),
        },
        SessionEventKind::ItemAppended {
            item: Item::user_text("hello"),
        },
        SessionEventKind::Routed {
            response_id: response_id.clone(),
            decision: DecisionRecord {
                chosen: local.clone(),
                rationale: "warm prefix".into(),
                policy: "affinity".into(),
                isl_tokens: 128,
                expected_prefill_tokens: 16.5,
                expected_cost_usd: 0.0,
                considered: vec![
                    Candidate {
                        target: local,
                        expected_prefill_tokens: 16.5,
                        matched_prefix_tokens: 112,
                        expected_ttft_ms: 40.0,
                        expected_cost_usd: 0.0,
                        quality_prior: 0.6,
                        load: Some(2048.0),
                    },
                    Candidate {
                        target: frontier,
                        expected_prefill_tokens: 128.0,
                        matched_prefix_tokens: 0,
                        expected_ttft_ms: 900.0,
                        expected_cost_usd: 0.0021,
                        quality_prior: 0.9,
                        load: None,
                    },
                ],
            },
        },
        SessionEventKind::OutputTextDelta {
            response_id: response_id.clone(),
            text: "hel".into(),
        },
        SessionEventKind::ResponseCompleted {
            response_id: response_id.clone(),
            usage: Usage {
                input_tokens: 128,
                cached_input_tokens: 112,
                output_tokens: 5,
                reasoning_tokens: 2,
                accounting: Accounting::Reported,
            },
        },
        SessionEventKind::ResponseIncomplete {
            response_id: response_id.clone(),
            reason: IncompleteReason::OwnerLost,
            usage: Usage {
                accounting: Accounting::Estimated,
                ..Usage::default()
            },
        },
        SessionEventKind::TurnDeduplicated {
            turn_id: TurnId::generate(),
            response_id,
        },
        SessionEventKind::Error {
            message: "boom".into(),
        },
    ]
}

#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn create_is_idempotent_and_reports_existing() {
    let (store, _, _) = store().await;
    contract::create_is_idempotent_and_reports_existing(&store).await;
}

#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_read_path_reports_unknown_sessions_as_not_found() {
    let (store, _, _) = store().await;
    let sid = SessionId::generate(); // minted but never created
    assert!(matches!(
        store.read_events(&sid, 0, 16).await,
        Err(StoreError::SessionNotFound(_))
    ));
    assert!(matches!(
        store.last_seq(&sid).await,
        Err(StoreError::SessionNotFound(_))
    ));
}

#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn an_empty_session_reads_as_seq_zero_with_no_events() {
    let (store, _, _) = store().await;
    let sid = SessionId::generate();
    assert!(store.create_session(&sid, "affinity").await.unwrap());
    assert_eq!(store.last_seq(&sid).await.unwrap(), 0);
    assert!(store.read_events(&sid, 0, 16).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn every_event_kind_reads_back_identically_and_paging_is_gapless() {
    let (store, config, url) = store().await;
    let sid = SessionId::generate();
    assert!(store.create_session(&sid, "affinity").await.unwrap());
    let written = raw_append(&url, &config, &sid, 1, every_event_kind()).await;

    assert_eq!(store.last_seq(&sid).await.unwrap(), written.len() as u64);
    assert_eq!(
        store.read_events(&sid, 0, 100).await.unwrap(),
        written,
        "a full replay must reproduce seqs, timestamps, and payloads exactly"
    );

    // A mid-log cursor with a limit, the shape every follower read takes.
    let page = store.read_events(&sid, 4, 3).await.unwrap();
    assert_eq!(
        page.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![5, 6, 7],
        "reads start strictly after the cursor and honor the limit"
    );
    assert_eq!(page, written[4..7].to_vec());
}

#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_corrupted_log_fails_loudly_rather_than_dropping_events() {
    let (store, config, url) = store().await;
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();

    // An entry missing the fields the wire format requires.
    let sid = SessionId::generate();
    assert!(store.create_session(&sid, "affinity").await.unwrap());
    let _: String = redis::cmd("XADD")
        .arg(config.log_key(&sid))
        .arg("1-0")
        .arg("garbage")
        .arg("x")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(
        matches!(
            store.read_events(&sid, 0, 16).await,
            Err(StoreError::Backend(_))
        ),
        "an unreadable entry is corruption to report, not an event to skip"
    );

    // An entry some foreign writer added with an auto-generated id: it breaks
    // the seq==id invariant every read relies on, so both read paths refuse.
    let sid = SessionId::generate();
    assert!(store.create_session(&sid, "affinity").await.unwrap());
    let _: String = redis::cmd("XADD")
        .arg(config.log_key(&sid))
        .arg("*")
        .arg("at_ms")
        .arg(1u64)
        .arg("kind")
        .arg(
            serde_json::to_string(&SessionEventKind::Error {
                message: "x".into(),
            })
            .unwrap(),
        )
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(matches!(
        store.read_events(&sid, 0, 16).await,
        Err(StoreError::Backend(_))
    ));
    assert!(matches!(
        store.last_seq(&sid).await,
        Err(StoreError::Backend(_))
    ));
}
