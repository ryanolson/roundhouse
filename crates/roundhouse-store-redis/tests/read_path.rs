// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The wire format proven from outside the crate, and the corruption harness.
//!
//! These tests write through `common::Rig::raw_append`, which produces
//! byte-for-byte the entries the fenced append script produces (explicit
//! `<seq>-0` ids, `at_ms` and `kind` fields). Together with the real-append
//! round-trip in `contract.rs`, that pins the on-disk format as the contract:
//! script-written and externally written entries are interchangeable, so the
//! format cannot drift silently inside the script. Raw writes are also how a
//! foreign writer's damage is simulated. Everything the shared contract suite
//! covers lives in `contract.rs` alone — nothing here repeats it.
//!
//! Gated with `#[ignore]` because that is the one skip the test harness
//! *reports*: a plain `cargo test` prints the ignored count with the reason
//! beside each name, which is the truth. The tempting alternative — an
//! env-var check that returns early — reports "passed" for tests that
//! verified nothing. Opting in is `--include-ignored`, and a missing
//! `ROUNDHOUSE_TEST_REDIS_URL` then fails loudly rather than skipping again.

mod common;

use common::{assert_covers_every_variant, every_event_kind, log_key, rig};
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::SessionId;
use roundhouse_core::store::{SessionStore, StoreError};

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
        .arg(log_key(&sid))
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
        .arg(log_key(&sid))
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

/// **F12 (M14.0 review).** The finding: `create_session`'s Redis-side
/// coverage in this crate only ever calls it once per session id (this
/// file's other tests, `common::Rig::fresh_session`), so the `SET ... NX`
/// reply is never checked in the direction that matters for R13 -- a
/// second `create_session` on a name the store already holds must report
/// `false`, not `true`. A mutation that drops the NX check (`Ok(true)`
/// unconditionally at `lib.rs:234`) would then read every re-creation as
/// fresh, which is the pre-R13 duplicated-prefix bug reintroduced under
/// exactly the backend the ruling names as its cause.
///
/// Proven false: this crate already carries a false-on-second-create
/// assertion against real Redis --
/// `roundhouse_core::store::contract::create_is_idempotent_and_reports_existing`,
/// run here through `store_contract_suite!` in `contract.rs` -- and it
/// fails under the `Ok(true)` mutation (`cargo test -p roundhouse-store-redis
/// -- --include-ignored` goes red, not green, contradicting the finding's
/// own proof instructions). This second, file-local assertion closes the
/// specific gap the finding points at in *this* file, so the guard is not
/// resting on `contract.rs` alone.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn f12_recreating_an_existing_session_reports_false_not_fresh() {
    let rig = rig().await;
    let sid = SessionId::generate();

    assert!(
        rig.store.create_session(&sid, "affinity").await.unwrap(),
        "F12: the first create on a never-seen id must report fresh"
    );
    assert!(
        !rig.store.create_session(&sid, "affinity").await.unwrap(),
        "F12: re-creating the same id must report `false` -- the NX reply \
         prefix admission (R13) depends on to tell a generation nothing \
         has ever held -- one that takes a claim whole -- from one the \
         store remembers and the claim must be checked against"
    );
}
