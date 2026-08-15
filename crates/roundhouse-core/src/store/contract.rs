// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The [`SessionStore`] contract as executable assertions.
//!
//! Every guarantee the trait documents lives here as a test any backend must
//! pass unchanged. The suite that judges [`MemoryStore`] is the same suite a
//! Redis (or any future) backend runs, which is what makes "the backends are
//! interchangeable" a checked property rather than a claim. Compiled for this
//! crate's own tests and, under the `test-support` feature, for dependent
//! crates' integration tests.
//!
//! Every test mints fresh [`SessionId`]s instead of assuming an empty store,
//! so one shared backend instance — one real Redis — can host the whole suite
//! without cross-test interference.
//!
//! Every public test here must also be called from [`run_all`]. That function
//! is the conformance entry point for backend crates, and a test missing from
//! it silently exempts every backend except the memory one.

use async_trait::async_trait;

use crate::event::{Accounting, SessionEvent, SessionEventKind, Usage};
use crate::ids::{ResponseId, SessionId, TurnId};
use crate::store::{Lease, MemoryStore, SessionStore, StoreError};

/// Lease TTL used throughout the suite: long enough that a slow CI machine
/// cannot expire a lease mid-test against a backend that enforces TTLs on the
/// wall clock. Expiry is never waited out — it is forced through
/// [`LeaseControl`].
const TTL_MS: u64 = 60_000;

/// Store-side lever for expiring a lease without waiting out its TTL.
///
/// The contracted effect is only that the current holder, if any, stops being
/// live: acquisition by another node must now succeed, and the displaced
/// holder's next fenced call must fail. How that happens is the backend's
/// business — [`MemoryStore`] backdates its record, a Redis backend deletes
/// the lease key. Test-only by construction: nothing in production code may
/// depend on this trait existing.
#[async_trait]
pub trait LeaseControl: SessionStore {
    async fn force_expire_lease(&self, session_id: &SessionId);
}

#[async_trait]
impl LeaseControl for MemoryStore {
    async fn force_expire_lease(&self, session_id: &SessionId) {
        self.expire_lease_now(session_id).await;
    }
}

async fn fresh_session<S: SessionStore>(store: &S) -> SessionId {
    let sid = SessionId::generate();
    assert!(
        store.create_session(&sid, "affinity").await.unwrap(),
        "a freshly generated id must not already exist"
    );
    sid
}

/// Filler payload for tests that care about sequencing, not content.
fn text_event(message: &str) -> SessionEventKind {
    SessionEventKind::Error {
        message: message.into(),
    }
}

pub async fn create_is_idempotent_and_reports_existing<S: SessionStore>(store: &S) {
    let sid = fresh_session(store).await;
    assert!(
        !store.create_session(&sid, "affinity").await.unwrap(),
        "re-creating must report the session already existed"
    );
}

pub async fn unknown_sessions_are_not_found<S: SessionStore>(store: &S) {
    let sid = SessionId::generate(); // minted but never created
    let ghost = Lease {
        session_id: sid.clone(),
        node_id: "node-a".into(),
        expires_at_ms: u64::MAX,
    };

    assert!(matches!(
        store.acquire_lease(&sid, "node-a", TTL_MS).await,
        Err(StoreError::SessionNotFound(_))
    ));
    assert!(matches!(
        store.renew_lease(&ghost, TTL_MS).await,
        Err(StoreError::SessionNotFound(_))
    ));
    // Not-found outranks lease-lost: an unknown session cannot have a lease,
    // and reporting the lease as the problem would send an operator chasing
    // failover where the actual defect is a session that was never created.
    assert!(matches!(
        store.append_events(&ghost, vec![text_event("x")]).await,
        Err(StoreError::SessionNotFound(_))
    ));
    assert!(matches!(
        store.read_events(&sid, 0, 16).await,
        Err(StoreError::SessionNotFound(_))
    ));
    assert!(matches!(
        store.last_seq(&sid).await,
        Err(StoreError::SessionNotFound(_))
    ));
    // Release alone is lenient. It is the cleanup path, and a shutdown racing
    // a session's disappearance should not turn into an error report.
    store.release_lease(&ghost).await.unwrap();
}

pub async fn a_live_lease_blocks_others_and_retakes_for_its_holder<S: SessionStore>(store: &S) {
    let sid = fresh_session(store).await;
    assert!(
        store
            .acquire_lease(&sid, "node-a", TTL_MS)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .acquire_lease(&sid, "node-b", TTL_MS)
            .await
            .unwrap()
            .is_none(),
        "a live lease held elsewhere must block acquisition"
    );
    assert!(
        store
            .acquire_lease(&sid, "node-a", TTL_MS)
            .await
            .unwrap()
            .is_some(),
        "the holder re-acquiring is recovery, not competition, and must succeed"
    );
}

pub async fn an_expired_lease_is_takeable_and_the_loser_cannot_append<S: LeaseControl>(store: &S) {
    let sid = fresh_session(store).await;
    let stale = store
        .acquire_lease(&sid, "node-a", TTL_MS)
        .await
        .unwrap()
        .unwrap();

    store.force_expire_lease(&sid).await;
    let fresh = store
        .acquire_lease(&sid, "node-b", TTL_MS)
        .await
        .unwrap()
        .expect("an expired lease must be takeable by another node");

    // The successor writes; the displaced owner is fenced out.
    store
        .append_events(&fresh, vec![text_event("ok")])
        .await
        .unwrap();
    let err = store
        .append_events(&stale, vec![text_event("no")])
        .await
        .expect_err("a displaced writer must not interleave with its successor");
    assert!(matches!(err, StoreError::LeaseLost { .. }));
}

pub async fn a_released_lease_is_gone_not_renewable<S: SessionStore>(store: &S) {
    let sid = fresh_session(store).await;
    let lease = store
        .acquire_lease(&sid, "node-a", TTL_MS)
        .await
        .unwrap()
        .unwrap();
    store.release_lease(&lease).await.unwrap();

    assert!(
        store.renew_lease(&lease, TTL_MS).await.unwrap().is_none(),
        "release ends the tenure; ownership restarts at acquire, never at renew"
    );
    assert!(
        store
            .acquire_lease(&sid, "node-b", TTL_MS)
            .await
            .unwrap()
            .is_some(),
        "a released session must be immediately takeable"
    );
}

pub async fn release_by_a_non_holder_leaves_the_lease_standing<S: SessionStore>(store: &S) {
    let sid = fresh_session(store).await;
    let held = store
        .acquire_lease(&sid, "node-a", TTL_MS)
        .await
        .unwrap()
        .unwrap();

    // A handle the store never granted. Release is compare-and-delete, so it
    // must not evict the holder on a stranger's say-so.
    let never_granted = Lease {
        session_id: sid.clone(),
        node_id: "node-b".into(),
        expires_at_ms: u64::MAX,
    };
    store.release_lease(&never_granted).await.unwrap();

    assert!(
        store
            .acquire_lease(&sid, "node-b", TTL_MS)
            .await
            .unwrap()
            .is_none(),
        "the holder must still hold after a non-holder's release"
    );
    store
        .append_events(&held, vec![text_event("still mine")])
        .await
        .unwrap();
}

/// The heartbeat invariant. A [`Lease`] is an identity, not a snapshot of
/// ownership: validity is decided against the store's *current* record, so a
/// handle whose own `expires_at_ms` has passed keeps working while the record
/// it names is live. The session layer depends on this — its heartbeat renews
/// the record on a separate task while every append continues through the
/// original handle.
pub async fn a_stale_handle_works_while_the_record_is_live<S: SessionStore>(store: &S) {
    let sid = fresh_session(store).await;
    let lease = store
        .acquire_lease(&sid, "node-a", TTL_MS)
        .await
        .unwrap()
        .unwrap();
    let stale_handle = Lease {
        expires_at_ms: 0,
        ..lease.clone()
    };

    store
        .append_events(&stale_handle, vec![text_event("append via stale handle")])
        .await
        .expect("a backend rejecting a stale-looking handle would fail every append made during a long turn");
    assert!(
        store
            .renew_lease(&stale_handle, TTL_MS)
            .await
            .unwrap()
            .is_some(),
        "renewal must also judge the record, not the handle"
    );
}

pub async fn appends_assign_contiguous_seqs_and_replay_is_gapless<S: SessionStore>(store: &S) {
    let sid = fresh_session(store).await;
    let lease = store
        .acquire_lease(&sid, "node-a", TTL_MS)
        .await
        .unwrap()
        .unwrap();

    let first = store
        .append_events(
            &lease,
            vec![
                SessionEventKind::SessionCreated {
                    model_policy: "affinity".into(),
                },
                text_event("one"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        first.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2],
        "a batch must be numbered contiguously from 1 within one call"
    );

    let second = store
        .append_events(&lease, vec![text_event("two")])
        .await
        .unwrap();
    assert_eq!(second[0].seq, 3, "numbering must continue across calls");
    assert_eq!(store.last_seq(&sid).await.unwrap(), 3);

    // Resume from seq 1 yields exactly the tail, with no gap or repeat.
    let tail = store.read_events(&sid, 1, 100).await.unwrap();
    assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
}

pub async fn read_events_pages_oldest_first_and_reproduces_the_append<S: SessionStore>(store: &S) {
    let sid = fresh_session(store).await;
    let lease = store
        .acquire_lease(&sid, "node-a", TTL_MS)
        .await
        .unwrap()
        .unwrap();

    // One of each easily built kind, so a backend that persists events by
    // taking them apart (fields in a Redis stream entry, say) proves it can
    // put them back together.
    let response_id = ResponseId::generate();
    let kinds = [
        SessionEventKind::SessionCreated {
            model_policy: "affinity".into(),
        },
        SessionEventKind::TurnStarted {
            turn_id: TurnId::generate(),
            response_id: response_id.clone(),
        },
        SessionEventKind::OutputTextDelta {
            response_id: response_id.clone(),
            text: "hel".into(),
        },
        SessionEventKind::ResponseCompleted {
            response_id,
            usage: Usage {
                input_tokens: 10,
                cached_input_tokens: 4,
                output_tokens: 3,
                reasoning_tokens: 1,
                accounting: Accounting::Estimated,
            },
        },
        text_event("tail"),
    ];
    let mut appended = store
        .append_events(&lease, kinds[..2].to_vec())
        .await
        .unwrap();
    appended.extend(
        store
            .append_events(&lease, kinds[2..].to_vec())
            .await
            .unwrap(),
    );

    // Page through with a limit smaller than the log, driving the cursor the
    // way the SSE follower does.
    let mut replayed: Vec<SessionEvent> = Vec::new();
    loop {
        let cursor = replayed.last().map_or(0, |event| event.seq);
        let batch = store.read_events(&sid, cursor, 2).await.unwrap();
        assert!(batch.len() <= 2, "limit must bound the batch");
        if batch.is_empty() {
            break;
        }
        replayed.extend(batch);
    }
    assert_eq!(
        replayed, appended,
        "replay must reproduce exactly what append returned — seqs, timestamps, and payloads"
    );
}

pub async fn last_seq_is_zero_when_empty_and_tracks_the_tail<S: SessionStore>(store: &S) {
    let sid = fresh_session(store).await;
    assert_eq!(
        store.last_seq(&sid).await.unwrap(),
        0,
        "an empty session reads as seq 0"
    );

    let lease = store
        .acquire_lease(&sid, "node-a", TTL_MS)
        .await
        .unwrap()
        .unwrap();
    store
        .append_events(&lease, vec![text_event("one"), text_event("two")])
        .await
        .unwrap();
    assert_eq!(store.last_seq(&sid).await.unwrap(), 2);
}

pub async fn renew_fails_once_the_lease_was_taken_over<S: LeaseControl>(store: &S) {
    let sid = fresh_session(store).await;
    let lease = store
        .acquire_lease(&sid, "node-a", TTL_MS)
        .await
        .unwrap()
        .unwrap();
    assert!(store.renew_lease(&lease, TTL_MS).await.unwrap().is_some());

    store.force_expire_lease(&sid).await;
    store
        .acquire_lease(&sid, "node-b", TTL_MS)
        .await
        .unwrap()
        .unwrap();
    assert!(
        store.renew_lease(&lease, TTL_MS).await.unwrap().is_none(),
        "the record belongs to the successor; renewal must report the loss as final"
    );
}

/// The whole contract, against one store instance.
///
/// This is the conformance entry point for backend crates: one integration
/// test calling this against a real backend runs every assertion in the
/// module. The individual functions stay public so a failing invariant can be
/// re-run alone while debugging.
pub async fn run_all<S: LeaseControl>(store: &S) {
    create_is_idempotent_and_reports_existing(store).await;
    unknown_sessions_are_not_found(store).await;
    a_live_lease_blocks_others_and_retakes_for_its_holder(store).await;
    an_expired_lease_is_takeable_and_the_loser_cannot_append(store).await;
    a_released_lease_is_gone_not_renewable(store).await;
    release_by_a_non_holder_leaves_the_lease_standing(store).await;
    a_stale_handle_works_while_the_record_is_live(store).await;
    appends_assign_contiguous_seqs_and_replay_is_gapless(store).await;
    read_events_pages_oldest_first_and_reproduces_the_append(store).await;
    last_seq_is_zero_when_empty_and_tracks_the_tail(store).await;
    renew_fails_once_the_lease_was_taken_over(store).await;
}
