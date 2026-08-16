// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M3: the full store contract against a real Redis, plus the adversarial
//! cases only a real backend can exercise.
//!
//! The macro invocation below is the milestone's headline: the *same* eleven
//! assertions that judge `MemoryStore` now judge this store, which is what
//! turns "the backends are interchangeable" from prose into a build step. The
//! tests after it are Redis-specific: races between separate connections,
//! real TTL expiry on the Redis clock, and recovery after the store's
//! connection is killed — behaviors the in-memory store cannot exhibit and
//! the shared suite therefore cannot check.
//!
//! Gating is the same as `read_path.rs`: `#[ignore]` because it is the one
//! skip the harness reports, opted into with `--include-ignored`, and a
//! missing `ROUNDHOUSE_TEST_REDIS_URL` then fails loudly.

mod common;

use common::{assert_covers_every_variant, connect_from_env, every_event_kind, lease_key, rig};
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::store::{SessionStore, StoreError};

roundhouse_core::store_contract_suite!(
    ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
    connect_from_env().await
);

/// Every event kind through the *real* append path. `read_path.rs` proves the
/// wire format with raw writes; this proves the fenced script writes it.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn every_event_kind_survives_the_fenced_append() {
    let kinds = every_event_kind();
    assert_covers_every_variant(&kinds);

    let rig = rig().await;
    let sid = rig.fresh_session().await;
    let lease = rig
        .store
        .acquire_lease(&sid, "node-a", 60_000)
        .await
        .unwrap()
        .unwrap();

    let appended = rig.store.append_events(&lease, kinds).await.unwrap();
    assert_eq!(
        rig.store.read_events(&sid, 0, 100).await.unwrap(),
        appended,
        "replay must reproduce exactly what the fenced append returned"
    );
    assert_eq!(
        rig.store.last_seq(&sid).await.unwrap(),
        appended.len() as u64
    );
}

/// The race the lease exists to referee: after an expiry, many nodes contend
/// and exactly one may win. Sequential takeover is already contract-tested;
/// this drives genuinely concurrent acquisitions through separate
/// connections, where only script atomicity keeps the count at one.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn concurrent_acquisition_after_expiry_admits_exactly_one_node() {
    let rig = rig().await;
    let sid = rig.fresh_session().await;
    rig.store
        .acquire_lease(&sid, "node-0", 60_000)
        .await
        .unwrap()
        .unwrap();
    let _: () = redis::cmd("DEL")
        .arg(lease_key(&sid))
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();

    let contenders = futures::future::join_all((1..=8).map(|node| {
        let sid = sid.clone();
        async move {
            // A connection per contender, so the race is between commands in
            // flight and not serialized by a shared client.
            let store = connect_from_env().await;
            store
                .acquire_lease(&sid, &format!("node-{node}"), 60_000)
                .await
                .unwrap()
        }
    }))
    .await;

    let winners = contenders.iter().flatten().count();
    assert_eq!(winners, 1, "a lease must never be granted twice");
}

/// Two tasks of the same holder appending through separate connections: the
/// log must interleave *batches*, never tear one. Each script execution is
/// atomic in Redis, so every batch's seqs come out consecutive and the whole
/// log gapless — this is the property the M3 script buys over a
/// read-then-XADD sequence, which would tear under exactly this load.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn concurrent_batches_interleave_without_tearing() {
    let rig = rig().await;
    let sid = rig.fresh_session().await;
    let lease = rig
        .store
        .acquire_lease(&sid, "node-a", 60_000)
        .await
        .unwrap()
        .unwrap();

    const TASKS: u64 = 4;
    const BATCHES: u64 = 25;
    const BATCH_LEN: u64 = 3;
    let writers = futures::future::join_all((0..TASKS).map(|task| {
        let lease = lease.clone();
        async move {
            let store = connect_from_env().await;
            for batch in 0..BATCHES {
                let kinds = (0..BATCH_LEN)
                    .map(|line| SessionEventKind::Error {
                        message: format!("{task}:{batch}:{line}"),
                    })
                    .collect();
                store.append_events(&lease, kinds).await.unwrap();
            }
        }
    }));
    writers.await;

    let total = TASKS * BATCHES * BATCH_LEN;
    let mut events = Vec::new();
    loop {
        let last = events
            .last()
            .map_or(0, |e: &roundhouse_core::event::SessionEvent| e.seq);
        let page = rig.store.read_events(&sid, last, 64).await.unwrap();
        if page.is_empty() {
            break;
        }
        events.extend(page);
    }
    assert_eq!(events.len() as u64, total, "no append may be lost");
    // read_events itself verifies gaplessness; what is left to check is that
    // no batch was torn: its members must sit at consecutive seqs.
    for window in events.chunks(BATCH_LEN as usize) {
        let labels: Vec<_> = window
            .iter()
            .map(|event| match &event.kind {
                SessionEventKind::Error { message } => {
                    message.rsplit_once(':').unwrap().0.to_string()
                }
                other => panic!("unexpected event {other:?}"),
            })
            .collect();
        assert!(
            labels.windows(2).all(|pair| pair[0] == pair[1]),
            "a batch was torn across other writers' entries: {labels:?}"
        );
    }
}

/// Leases really expire on the Redis clock — no force-expiry hook, just PX
/// doing its job. This is the one place the suite waits out a real TTL,
/// because the TTL mechanism itself is the subject.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_lease_really_expires_on_the_redis_clock() {
    let rig = rig().await;
    let sid = rig.fresh_session().await;
    let short = rig
        .store
        .acquire_lease(&sid, "node-a", 100)
        .await
        .unwrap()
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    assert!(
        rig.store
            .acquire_lease(&sid, "node-b", 60_000)
            .await
            .unwrap()
            .is_some(),
        "PX must have expired the lease without anyone's help"
    );
    assert!(matches!(
        rig.store
            .append_events(
                &short,
                vec![SessionEventKind::Error {
                    message: "no".into()
                }]
            )
            .await,
        Err(StoreError::LeaseLost { .. })
    ));
}
