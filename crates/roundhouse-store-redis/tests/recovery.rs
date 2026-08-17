// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Connection-loss recovery, quarantined into its own test binary.
//!
//! The test below runs `CLIENT KILL … SKIPME yes`, which severs every other
//! connection to the shared Redis — including those of any test running
//! beside it in the same binary, whose in-flight commands then fail for
//! reasons that have nothing to do with what they assert. Cargo runs test
//! binaries sequentially, so a file of its own is the boring, reliable
//! isolation. Gated the same way as the rest: `#[ignore]`, opted into with
//! `--include-ignored`.

mod common;

use common::rig;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::store::{SessionStore, StoreError};

/// Kill the store's connection out from under it mid-session: the manager
/// must reconnect, and the log must come back gapless with nothing repeated.
/// The first calls after the kill may fail — that is the contract of a broken
/// pipe — but the store must not need replacing.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_store_survives_its_connection_being_killed() {
    let rig = rig().await;
    let sid = rig.fresh_session().await;
    let lease = rig
        .store
        .acquire_lease(&sid, "node-a", 60_000)
        .await
        .unwrap()
        .unwrap();
    rig.store
        .append_events(
            &lease,
            vec![SessionEventKind::Error {
                message: "one".into(),
            }],
        )
        .await
        .unwrap();

    // Sever every connection except the raw one doing the killing.
    let _killed: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg("normal")
        .arg("SKIPME")
        .arg("yes")
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();

    // The manager reconnects behind the scenes; give it a few attempts.
    let mut appended = None;
    for _ in 0..10 {
        match rig
            .store
            .append_events(
                &lease,
                vec![SessionEventKind::Error {
                    message: "two".into(),
                }],
            )
            .await
        {
            Ok(events) => {
                appended = Some(events);
                break;
            }
            Err(StoreError::Backend(_)) => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(other) => panic!("only transport errors are acceptable here: {other}"),
        }
    }
    let appended = appended.expect("the store must recover after a killed connection");
    assert_eq!(appended[0].seq, 2, "no gap, no repeat across the reconnect");

    let replay = rig.store.read_events(&sid, 0, 16).await.unwrap();
    assert_eq!(
        replay.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2],
        "the reconnect must not have duplicated or dropped an append"
    );
}
