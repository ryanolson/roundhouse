// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learning index against a real Redis: what the shared contract suite
//! cannot reach because only a Redis key can hold the wrong type.
//!
//! Redis does not roll back a script's earlier writes when a later command in
//! it fails. The append therefore validates the sequence range — and, when
//! marked, every index key and the stored mark — before its first `XADD`;
//! these tests sabotage each of those and check that the log and the index
//! are both unchanged.
//! Every test connects under a fresh namespace, because a sabotaged index key
//! would otherwise break every test sharing it.
//!
//! Gated like the rest of this crate's Redis tests: `#[ignore]`, opted into
//! with `--include-ignored` and `ROUNDHOUSE_TEST_REDIS_URL`.

use std::num::NonZeroUsize;

use roundhouse_core::control::ProjectId;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::SessionId;
use roundhouse_core::store::{LearningMark, Lease, SessionStore, StoreError};
use roundhouse_store_redis::KeyNamespace;
use roundhouse_store_redis::RedisSessionStore;
use roundhouse_store_redis::test_support::{
    connect_in, fresh_namespace, learning_index_keys, log_key_in, url_from_env,
};

/// The largest sequence the append script renders exactly. Redis Lua formats
/// numbers with `%.14g`, so `100_000_000_000_000` would become `1e+14`.
const LAST_EXACT_SEQ: u64 = 99_999_999_999_999;

struct Rig {
    namespace: KeyNamespace,
    store: RedisSessionStore,
    raw: redis::aio::MultiplexedConnection,
}

async fn rig() -> Rig {
    let namespace = fresh_namespace();
    let store = connect_in(namespace.clone()).await;
    let raw = redis::Client::open(url_from_env())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    Rig {
        namespace,
        store,
        raw,
    }
}

impl Rig {
    async fn held(&self) -> (SessionId, Lease) {
        let sid = SessionId::generate();
        assert!(self.store.create_session(&sid, "affinity").await.unwrap());
        let lease = self
            .store
            .acquire_lease(&sid, "node-a", 60_000)
            .await
            .unwrap()
            .expect("nothing contends for a fresh session's lease");
        (sid, lease)
    }

    async fn key_type(&self, key: &str) -> String {
        redis::cmd("TYPE")
            .arg(key)
            .query_async(&mut self.raw.clone())
            .await
            .unwrap()
    }

    async fn set_string(&self, key: &str) {
        let _: () = redis::cmd("SET")
            .arg(key)
            .arg("not an index")
            .query_async(&mut self.raw.clone())
            .await
            .unwrap();
    }

    async fn del(&self, key: &str) {
        let _: i64 = redis::cmd("DEL")
            .arg(key)
            .query_async(&mut self.raw.clone())
            .await
            .unwrap();
    }

    /// Seed a log entry with an explicit sequence, the shape the append
    /// script writes, without going through the lease.
    async fn seed_log(&self, sid: &SessionId, seq: u64) {
        let _: String = redis::cmd("XADD")
            .arg(log_key_in(&self.namespace, sid))
            .arg(format!("{seq}-0"))
            .arg("at_ms")
            .arg(1_700_000_000_000_u64)
            .arg("kind")
            .arg(serde_json::to_string(&event("seed")).unwrap())
            .query_async(&mut self.raw.clone())
            .await
            .unwrap();
    }

    async fn log_len(&self, sid: &SessionId) -> u64 {
        redis::cmd("XLEN")
            .arg(log_key_in(&self.namespace, sid))
            .query_async(&mut self.raw.clone())
            .await
            .unwrap()
    }
}

fn event(message: &str) -> SessionEventKind {
    SessionEventKind::Error {
        message: message.into(),
    }
}

fn mark(event_index: usize) -> Option<LearningMark> {
    Some(LearningMark::new(event_index, ProjectId::new("acme")))
}

/// **A wrong-typed index key refuses the marked append before any write.**
///
/// If the script discovered the wrong type at the write that needs it, the
/// events before that write would already be in the log with no mark — the
/// exact loss source-side discovery exists to prevent. Each of the three keys
/// is sabotaged in turn; the log and the other two keys must be untouched,
/// and removing the sabotage must let the same append through.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_wrong_typed_index_key_refuses_a_marked_append_before_any_write() {
    let rig = rig().await;
    let keys = learning_index_keys(&rig.namespace);
    for sabotaged in 0..keys.len() {
        let (sid, lease) = rig.held().await;
        rig.store
            .append_events(&lease, vec![event("before")], None)
            .await
            .unwrap();
        rig.set_string(&keys[sabotaged]).await;

        let refused = rig
            .store
            .append_events(&lease, vec![event("a"), event("b")], mark(1))
            .await;
        assert!(
            matches!(refused, Err(StoreError::Backend(_))),
            "a wrong-typed `{}` must refuse the marked append, got {refused:?}",
            keys[sabotaged]
        );
        assert_eq!(
            rig.store.last_seq(&sid).await.unwrap(),
            1,
            "no event may be written ahead of a refusal"
        );
        assert_eq!(rig.key_type(&keys[sabotaged]).await, "string");
        for (other, key) in keys.iter().enumerate() {
            if other != sabotaged {
                assert_eq!(
                    rig.key_type(key).await,
                    "none",
                    "no index write may land ahead of a refusal (`{key}`)"
                );
            }
        }

        rig.del(&keys[sabotaged]).await;
        rig.store
            .append_events(&lease, vec![event("a"), event("b")], mark(1))
            .await
            .expect("the same append must pass once the key is the right type");
        for key in &keys {
            rig.del(key).await;
        }
    }
}

/// Control: the unmarked append never touches the index keys, so wrong-typed
/// index keys cannot fail it. This is what keeps the unmarked path on its
/// three hash-tagged, single-slot keys.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn an_unmarked_append_never_touches_the_index_keys() {
    let rig = rig().await;
    let keys = learning_index_keys(&rig.namespace);
    for key in &keys {
        rig.set_string(key).await;
    }
    let (sid, lease) = rig.held().await;
    rig.store
        .append_events(&lease, vec![event("a"), event("b")], None)
        .await
        .expect("an unmarked append must not depend on the index keys");
    assert_eq!(rig.store.last_seq(&sid).await.unwrap(), 2);
    for key in &keys {
        assert_eq!(rig.key_type(key).await, "string");
    }
}

/// **A stored mark the store cannot parse refuses the append before any
/// write**, rather than being overwritten: it may hold the project the
/// session belongs to, and replacing it unread would skip the project check.
///
/// **The refusal is `CorruptLog`, not `Backend`** (M9 round-3, item 3):
/// `ARGV[3]` keys the marks-hash `HGET` by this session's own id, so an
/// unreadable field there is always this session's data, never ambiguous the
/// way a `WRONGTYPE` on one of the six append keys can be. Before, it was
/// `Backend`, the same as `WRONGTYPE`, on a rationale that did not actually
/// apply to it (see `scripts.rs`'s corrected note by `APPEND_BODY`).
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn an_unreadable_stored_mark_refuses_the_append_before_any_write() {
    let rig = rig().await;
    let [marks, marked, pending] = learning_index_keys(&rig.namespace);
    let (sid, lease) = rig.held().await;
    let _: i64 = redis::cmd("HSET")
        .arg(&marks)
        .arg(sid.as_str())
        .arg("not a mark")
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();

    let refused = rig
        .store
        .append_events(&lease, vec![event("a")], mark(0))
        .await;
    assert!(
        matches!(&refused, Err(StoreError::CorruptLog { session_id, .. }) if *session_id == sid),
        "an unreadable stored mark must refuse the append as this session's fault, got {refused:?}"
    );
    assert_eq!(rig.store.last_seq(&sid).await.unwrap(), 0);
    assert_eq!(rig.key_type(&marked).await, "none");
    assert_eq!(rig.key_type(&pending).await, "none");
    let stored: String = redis::cmd("HGET")
        .arg(&marks)
        .arg(sid.as_str())
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();
    assert_eq!(stored, "not a mark");
}

/// **A pending member with no stored mark is named, and the page goes on**
/// (M9 round-3, item 1). Only a foreign `HDEL`/`DEL` of the marks field, or an
/// `allkeys-lru` eviction of the whole hash, produces this: the index only
/// ever adds a pending member together with its mark. Failing the whole page
/// on it would stop every pass at that member for every project, the same
/// reason an unparseable mark is named rather than fatal, so a member with no
/// mark is named in `unreadable` the same way.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_pending_member_without_a_mark_is_named_and_the_page_goes_on() {
    let rig = rig().await;
    let [_, _, pending] = learning_index_keys(&rig.namespace);
    let _: i64 = redis::cmd("ZADD")
        .arg(&pending)
        .arg(0)
        .arg("sess_orphan")
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();
    let page = rig
        .store
        .pending_learning(None, 0, NonZeroUsize::new(8).unwrap())
        .await
        .expect("a member with no mark must not fail the page");
    assert_eq!(
        page.unreadable,
        vec![SessionId::new("sess_orphan")],
        "it is named rather than skipped or fatal"
    );
    assert!(page.sessions.is_empty());
}

/// **A marked append stops at the last sequence the script renders
/// exactly.** Past it, the stream id would be written as `1e+14-0` and the
/// `XADD` would fail after earlier events of the batch were already in the
/// log — events with no mark. The refusal comes first; the last exact
/// sequence itself is still markable.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_marked_append_stops_at_the_last_exact_sequence() {
    let rig = rig().await;
    let keys = learning_index_keys(&rig.namespace);
    let (sid, lease) = rig.held().await;
    rig.seed_log(&sid, LAST_EXACT_SEQ - 1).await;

    let refused = rig
        .store
        .append_events(&lease, vec![event("a"), event("b")], mark(0))
        .await;
    assert!(
        matches!(refused, Err(StoreError::Backend(_))),
        "a batch crossing the exact range must be refused, got {refused:?}"
    );
    assert_eq!(rig.log_len(&sid).await, 1, "no event may be written");
    for key in &keys {
        assert_eq!(rig.key_type(key).await, "none");
    }

    let events = rig
        .store
        .append_events(&lease, vec![event("last")], mark(0))
        .await
        .expect("the last exact sequence is markable");
    assert_eq!(events[0].seq, LAST_EXACT_SEQ);
    let marked = rig
        .store
        .learning_sessions(None, NonZeroUsize::new(8).unwrap())
        .await
        .unwrap();
    assert_eq!(marked.sessions.len(), 1);
    assert_eq!(marked.sessions[0].seq, LAST_EXACT_SEQ);
}

/// **An unmarked batch that would cross the exact range writes nothing.**
///
/// Found while testing the marked range guard: before the shared preflight,
/// the unmarked append wrote seq `99_999_999_999_999`, failed on the next
/// `XADD` (`1e+14-0`), and left that first event in the log behind a refused
/// append. The refusal now comes before the first write whether or not the
/// batch carries a mark, and the last exact sequence is still appendable.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn an_unmarked_batch_crossing_the_exact_range_writes_nothing() {
    let rig = rig().await;
    let keys = learning_index_keys(&rig.namespace);
    let (sid, lease) = rig.held().await;
    rig.seed_log(&sid, LAST_EXACT_SEQ - 1).await;
    let refused = rig
        .store
        .append_events(&lease, vec![event("a"), event("b")], None)
        .await;
    assert!(
        matches!(refused, Err(StoreError::Backend(_))),
        "a batch crossing the exact range must be refused, got {refused:?}"
    );
    assert_eq!(
        rig.log_len(&sid).await,
        1,
        "a refused batch must write none of its events"
    );
    for key in &keys {
        assert_eq!(rig.key_type(key).await, "none");
    }

    let events = rig
        .store
        .append_events(&lease, vec![event("last")], None)
        .await
        .expect("the last exact sequence is appendable");
    assert_eq!(events[0].seq, LAST_EXACT_SEQ);
    assert_eq!(rig.log_len(&sid).await, 2);
}

/// **A stored mark whose seq overflows `u64` is named, and the page goes
/// on** (M9 round-4, item 1). Lua's `parse_mark` accepts any digit run,
/// unbounded; Rust's `u64` is not. Before, `decode_page` failed the whole
/// page as `Backend` on the first entry it could not parse this way, turning
/// one session's overflowed mark into an outage for every project behind it.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_seq_that_overflows_u64_is_named_and_the_page_goes_on() {
    let rig = rig().await;
    let [marks, _, pending] = learning_index_keys(&rig.namespace);
    let _: i64 = redis::cmd("ZADD")
        .arg(&pending)
        .arg(0)
        .arg("sess_x")
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();
    let _: () = redis::cmd("HSET")
        .arg(&marks)
        .arg("sess_x")
        .arg("18446744073709551616:1:acme")
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();
    let page = rig
        .store
        .pending_learning(None, 0, NonZeroUsize::new(8).unwrap())
        .await
        .expect("an overflowed seq must not fail the page");
    assert_eq!(
        page.unreadable,
        vec![SessionId::new("sess_x")],
        "it is named rather than fatal"
    );
    assert!(page.sessions.is_empty());
}

/// `LearningPage::unreadable` is documented in byte order. The Lua page
/// names a member with no mark, and `decode_page` names one whose mark it
/// cannot decode; the two lists are merged, so the order is only right if
/// the merge sorts (M9 round-5).
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn unreadable_members_from_the_script_and_the_decoder_come_back_in_byte_order() {
    let rig = rig().await;
    let [marks, _, pending] = learning_index_keys(&rig.namespace);
    for member in ["sess_a", "sess_b", "sess_c"] {
        let _: i64 = redis::cmd("ZADD")
            .arg(&pending)
            .arg(0)
            .arg(member)
            .query_async(&mut rig.raw.clone())
            .await
            .unwrap();
    }
    // `sess_a` and `sess_c` have no mark (the script names them);
    // `sess_b` has one the script accepts and Rust cannot decode.
    let _: () = redis::cmd("HSET")
        .arg(&marks)
        .arg("sess_b")
        .arg("18446744073709551616:1:acme")
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();
    let page = rig
        .store
        .pending_learning(None, 0, NonZeroUsize::new(8).unwrap())
        .await
        .expect("unreadable members must not fail the page");
    assert_eq!(
        page.unreadable,
        vec![
            SessionId::new("sess_a"),
            SessionId::new("sess_b"),
            SessionId::new("sess_c"),
        ],
        "unreadable is in byte order, whichever side named each member"
    );
}

/// The same overflow, in the marked-at field, against the permanent
/// enumeration rather than pending (M9 round-4, item 1).
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_marked_at_that_overflows_u64_is_named_and_the_page_goes_on() {
    let rig = rig().await;
    let [marks, marked, _] = learning_index_keys(&rig.namespace);
    let _: i64 = redis::cmd("ZADD")
        .arg(&marked)
        .arg(0)
        .arg("sess_y")
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();
    let _: () = redis::cmd("HSET")
        .arg(&marks)
        .arg("sess_y")
        .arg("1:18446744073709551616:acme")
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();
    let page = rig
        .store
        .learning_sessions(None, NonZeroUsize::new(8).unwrap())
        .await
        .expect("an overflowed marked_at must not fail the page");
    assert_eq!(
        page.unreadable,
        vec![SessionId::new("sess_y")],
        "it is named rather than fatal"
    );
    assert!(page.sessions.is_empty());
}

/// A non-UTF-8 project in a stored mark is named the same way (M9 round-4,
/// item 1): `str_at` already answers `None` on invalid UTF-8, but
/// `decode_page` used to fold that into a page-wide `Backend` failure.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_non_utf8_project_is_named_and_the_page_goes_on() {
    let rig = rig().await;
    let [marks, _, pending] = learning_index_keys(&rig.namespace);
    let _: i64 = redis::cmd("ZADD")
        .arg(&pending)
        .arg(0)
        .arg("sess_z")
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();
    let mut mark = b"1:1:".to_vec();
    mark.extend_from_slice(&[0xFF, 0xFE]);
    let _: () = redis::cmd("HSET")
        .arg(&marks)
        .arg("sess_z")
        .arg(mark)
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();
    let page = rig
        .store
        .pending_learning(None, 0, NonZeroUsize::new(8).unwrap())
        .await
        .expect("a non-UTF-8 project must not fail the page");
    assert_eq!(
        page.unreadable,
        vec![SessionId::new("sess_z")],
        "it is named rather than fatal"
    );
    assert!(page.sessions.is_empty());
}

/// **A clear's `NEWER` reply carrying an overflowed seq is this session's
/// fault, not the store's** (M9 round-4, item 1, secondary path). `seq_at`
/// used to fold an unparseable `NEWER`/`MISMATCH` seq into `unexpected`
/// (`Backend`), which the recovery task reads as an outage for every
/// project; `ARGV[1]` keys the `HGET` by this session's own id, so the value
/// is always this session's data.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_clear_whose_newer_reply_carries_an_overflowed_seq_is_corrupt_not_backend() {
    let rig = rig().await;
    let [marks, _, _] = learning_index_keys(&rig.namespace);
    let sid = SessionId::new("sess_w");
    let _: () = redis::cmd("HSET")
        .arg(&marks)
        .arg(sid.as_str())
        .arg("18446744073709551616:1:acme")
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();
    let cleared = rig.store.clear_learning_mark(&sid, 0).await;
    assert!(
        matches!(&cleared, Err(StoreError::CorruptLog { session_id, .. }) if *session_id == sid),
        "an overflowed seq in the NEWER reply must be this session's fault, got {cleared:?}"
    );
}

/// The same fix on the requeue side: a `MISMATCH` reply carrying an
/// overflowed seq is this session's fault, not the store's (M9 round-4,
/// item 1, secondary path). `requeue`'s own `seq_at` call shares the same
/// helper `clear`'s does, and had the identical `unexpected` (`Backend`)
/// fallback.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_requeue_whose_mismatch_reply_carries_an_overflowed_seq_is_corrupt_not_backend() {
    let rig = rig().await;
    let [marks, _, _] = learning_index_keys(&rig.namespace);
    let sid = SessionId::new("sess_v");
    let _: () = redis::cmd("HSET")
        .arg(&marks)
        .arg(sid.as_str())
        .arg("18446744073709551616:1:acme")
        .query_async(&mut rig.raw.clone())
        .await
        .unwrap();
    let requeued = rig.store.requeue_learning(&sid, 1).await;
    assert!(
        matches!(&requeued, Err(StoreError::CorruptLog { session_id, .. }) if *session_id == sid),
        "an overflowed seq in the MISMATCH reply must be this session's fault, got {requeued:?}"
    );
}
