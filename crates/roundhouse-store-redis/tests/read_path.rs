// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The M2 read path against a real Redis.
//!
//! These tests write through `common::Rig::raw_append`, which produces
//! byte-for-byte the entries the fenced append script produces (explicit
//! `<seq>-0` ids, `at_ms` and `kind` fields). That was the point of M2:
//! prove the read side of the wire format before the write side existed —
//! and it stays the corruption harness now that M3 is real, because raw
//! writes are how a foreign writer's damage is simulated. The full contract
//! suite runs in `contract.rs`.
//!
//! Gated with `#[ignore]` because that is the one skip the test harness
//! *reports*: a plain `cargo test` prints the ignored count with the reason
//! beside each name, which is the truth. The tempting alternative — an
//! env-var check that returns early — reports "passed" for tests that
//! verified nothing. Opting in is `--include-ignored`, and a missing
//! `ROUNDHOUSE_TEST_REDIS_URL` then fails loudly rather than skipping again.

mod common;

use common::{assert_covers_every_variant, every_event_kind, rig};
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::SessionId;
use roundhouse_core::store::{SessionStore, StoreError, contract};

#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn create_is_idempotent_and_reports_existing() {
    contract::create_is_idempotent_and_reports_existing(&rig().await.store).await;
}

#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_read_path_reports_unknown_sessions_as_not_found() {
    let store = rig().await.store;
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
    let store = rig().await.store;
    let sid = SessionId::generate();
    assert!(store.create_session(&sid, "affinity").await.unwrap());
    assert_eq!(store.last_seq(&sid).await.unwrap(), 0);
    assert!(store.read_events(&sid, 0, 16).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn every_event_kind_reads_back_identically_and_paging_is_gapless() {
    let kinds = every_event_kind();
    assert_covers_every_variant(&kinds);

    let mut rig = rig().await;
    let sid = SessionId::generate();
    assert!(rig.store.create_session(&sid, "affinity").await.unwrap());
    let written = rig.raw_append(&sid, 1, kinds).await;

    let store = &rig.store;
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
    let mut rig = rig().await;

    // An entry missing the fields the wire format requires.
    let sid = SessionId::generate();
    assert!(rig.store.create_session(&sid, "affinity").await.unwrap());
    let _: String = redis::cmd("XADD")
        .arg(rig.config.log_key(&sid))
        .arg("1-0")
        .arg("garbage")
        .arg("x")
        .query_async(&mut rig.raw)
        .await
        .unwrap();
    assert!(
        matches!(
            rig.store.read_events(&sid, 0, 16).await,
            Err(StoreError::Backend(_))
        ),
        "an unreadable entry is corruption to report, not an event to skip"
    );

    // An entry some foreign writer added with an auto-generated id: it breaks
    // the seq==id invariant every read relies on, so both read paths refuse.
    let sid = SessionId::generate();
    assert!(rig.store.create_session(&sid, "affinity").await.unwrap());
    let _: String = redis::cmd("XADD")
        .arg(rig.config.log_key(&sid))
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
        .query_async(&mut rig.raw)
        .await
        .unwrap();
    assert!(matches!(
        rig.store.read_events(&sid, 0, 16).await,
        Err(StoreError::Backend(_))
    ));
    assert!(matches!(
        rig.store.last_seq(&sid).await,
        Err(StoreError::Backend(_))
    ));
}
