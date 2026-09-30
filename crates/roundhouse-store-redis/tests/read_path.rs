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

use common::{assert_covers_every_variant, every_event_kind, lease_key, log_key, rig};
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
    // `CorruptLog`, not `Backend`: the store answered, and the fault is in
    // this one log's content (M9 review, L1 ruling).
    assert!(
        matches!(
            rig.store.read_events(&sid, 0, 16).await,
            Err(StoreError::CorruptLog { ref session_id, .. }) if *session_id == sid
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
        Err(StoreError::CorruptLog { ref session_id, .. }) if *session_id == sid
    ));
    assert!(matches!(
        rig.store.last_seq(&sid).await,
        Err(StoreError::CorruptLog { ref session_id, .. }) if *session_id == sid
    ));

    // An entry with the right id and a `kind` no build writes.
    let sid = SessionId::generate();
    assert!(rig.store.create_session(&sid, "affinity").await.unwrap());
    let _: String = redis::cmd("XADD")
        .arg(log_key(&sid))
        .arg("1-0")
        .arg("at_ms")
        .arg(1u64)
        .arg("kind")
        .arg("{\"not\":\"an event\"}")
        .query_async(&mut rig.raw)
        .await
        .unwrap();
    assert!(matches!(
        rig.store.read_events(&sid, 0, 16).await,
        Err(StoreError::CorruptLog { ref session_id, .. }) if *session_id == sid
    ));
}

/// **A wrong-typed key of one session is that session's fault** (M9
/// round-2, item 1). A foreign writer replaced the session's log with a
/// string, or its lease with a string, so Redis answers `WRONGTYPE`. The
/// store answered, and the fault is in one session's keys: `CorruptLog`,
/// never `Backend`, which the recovery task reads as an outage for every
/// project.
///
/// **The log assertions hold on every supported Redis; the lease ones need
/// Redis 7** (M9 round-3, item 2, low). `read_events` and `last_seq` are
/// plain pipelined commands, so their `WRONGTYPE` classifies on any Redis
/// this crate supports. `acquire_lease`, `renew_lease` and `release_lease`
/// run a Lua script, and only Redis 7 and later propagates a script's raised
/// `WRONGTYPE` with its code intact for `is_wrong_type` to read; Redis 6.x
/// wraps it as a generic `ERR Error running script ...`, so the lease
/// assertions below need a Redis 7-or-later target even though the crate's
/// own floor stays ≥ 6.2. Harmless in production either way: nothing here
/// branches on `CorruptLog` versus `Backend` for a lease call.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_wrong_typed_session_key_is_corrupt_not_a_backend_failure() {
    fn is_corrupt<T>(result: &Result<T, StoreError>, sid: &SessionId) -> bool {
        matches!(result, Err(StoreError::CorruptLog { session_id, .. }) if session_id == sid)
    }
    let mut rig = rig().await;

    // The log: every read path of one session's log.
    let sid = rig.fresh_session().await;
    rig.raw_append(
        &sid,
        1,
        vec![SessionEventKind::Error {
            message: "x".into(),
        }],
    )
    .await;
    let _: () = redis::cmd("DEL")
        .arg(log_key(&sid))
        .query_async(&mut rig.raw)
        .await
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg(log_key(&sid))
        .arg("x")
        .query_async(&mut rig.raw)
        .await
        .unwrap();
    let read = rig.store.read_events(&sid, 0, 16).await;
    assert!(is_corrupt(&read, &sid), "read_events: {read:?}");
    let last = rig.store.last_seq(&sid).await;
    assert!(is_corrupt(&last, &sid), "last_seq: {last:?}");

    // The lease: acquire, renew and release read only this session's meta
    // and lease keys.
    let sid = rig.fresh_session().await;
    let lease = rig
        .store
        .acquire_lease(&sid, "node-a", 60_000)
        .await
        .unwrap()
        .expect("a fresh session's lease is free");
    let _: () = redis::cmd("SET")
        .arg(lease_key(&sid))
        .arg("x")
        .query_async(&mut rig.raw)
        .await
        .unwrap();
    let acquired = rig.store.acquire_lease(&sid, "node-b", 60_000).await;
    assert!(is_corrupt(&acquired, &sid), "acquire_lease: {acquired:?}");
    let renewed = rig.store.renew_lease(&lease, 60_000).await;
    assert!(is_corrupt(&renewed, &sid), "renew_lease: {renewed:?}");
    let released = rig.store.release_lease(&lease).await;
    assert!(is_corrupt(&released, &sid), "release_lease: {released:?}");
}

/// **A foreign stream entry with a non-UTF-8 field name is corrupt, not a
/// backend failure** (M9 round-4, item 3, low). Decoding the pipeline's
/// reply into a `StreamId` fails inside redis-rs itself -- a client-side
/// parse error with no server error code for `is_wrong_type` to read, so the
/// old classification fell through to `Backend`. The fault is still this
/// one session's stored bytes, never ambiguous the way a `WRONGTYPE` on a
/// call touching more than one session's keys can be, so it must answer
/// `CorruptLog`.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_non_utf8_stream_field_name_is_corrupt_not_a_backend_failure() {
    let mut rig = rig().await;
    let sid = rig.fresh_session().await;
    let _: String = redis::cmd("XADD")
        .arg(log_key(&sid))
        .arg("1-0")
        .arg(vec![0xFFu8, 0xFE])
        .arg("x")
        .query_async(&mut rig.raw)
        .await
        .unwrap();
    let read = rig.store.read_events(&sid, 0, 16).await;
    assert!(
        matches!(read, Err(StoreError::CorruptLog { ref session_id, .. }) if *session_id == sid),
        "a non-UTF-8 field name must be this session's fault, got {read:?}"
    );
    // `last_seq` decodes the same reply shape (`StreamRangeReply`) through
    // its own, separate local `Value` conversion -- not exercised by
    // `read_events` above, so it needs its own assertion.
    let last = rig.store.last_seq(&sid).await;
    assert!(
        matches!(last, Err(StoreError::CorruptLog { ref session_id, .. }) if *session_id == sid),
        "last_seq must answer the same way: {last:?}"
    );
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
