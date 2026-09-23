// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learning-index half of the [`SessionStore`] contract.
//!
//! Listed in [`store_contract_suite!`](crate::store_contract_suite), so every
//! backend runs these beside the lease and log cases. Like the rest of the
//! suite, each test tolerates a shared backend: it filters every enumeration
//! to the sessions it created, so another test's marks in the same index can
//! lengthen a pass but cannot change what it asserts.
//!
//! Nothing here calls a learner store. That is the point of the source-side
//! index: everything a recovery task needs to find undelivered work is in the
//! session store, so these tests prove discovery with the learner absent.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use crate::control::ProjectId;
use crate::event::SessionEvent;
use crate::ids::SessionId;
use crate::store::{
    ClearOutcome, LearningCursor, LearningMark, LearningPage, Lease, MarkedSession, RequeueOutcome,
    SessionStore, StoreError,
};

use super::{LeaseControl, TTL_MS, held, text_event};

/// Pages one pass may take before the suite calls the cursor broken. A
/// backend whose cursor stopped moving would otherwise spin the test forever
/// instead of failing it.
const MAX_PAGES_PER_PASS: usize = 100_000;

/// How many consecutive marked appends
/// [`a_delayed_clear_keeps_a_newer_mark`] makes while looking for two in the
/// same millisecond. A bound, not a requirement: the property is asserted on
/// the last pair whether or not the clock tied.
const MAX_TIE_ATTEMPTS: usize = 50;

/// Large enough that no session marked during a test is that idle.
const AN_HOUR_MS: u64 = 3_600_000;

fn limit(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("page limits in this suite are positive")
}

fn acme() -> ProjectId {
    ProjectId::new("acme")
}

fn globex() -> ProjectId {
    ProjectId::new("globex")
}

#[derive(Clone, Copy)]
enum Index {
    Pending { idle_for_ms: u64 },
    Permanent,
}

/// Every pending member, whatever its age.
const PENDING: Index = Index::Pending { idle_for_ms: 0 };

async fn page<S: SessionStore>(
    store: &S,
    index: Index,
    after: Option<&LearningCursor>,
    n: usize,
) -> LearningPage {
    match index {
        Index::Pending { idle_for_ms } => store
            .pending_learning(after, idle_for_ms, limit(n))
            .await
            .unwrap(),
        Index::Permanent => store.learning_sessions(after, limit(n)).await.unwrap(),
    }
}

/// Page from `start` until `next` is `None`, checking on the way that every
/// page respects its limit, returns sessions in strictly increasing id order,
/// and moves the cursor strictly forward.
async fn pass_from<S: SessionStore>(
    store: &S,
    index: Index,
    start: Option<LearningCursor>,
    n: usize,
) -> Vec<MarkedSession> {
    let mut seen: Vec<MarkedSession> = Vec::new();
    let mut cursor = start;
    for _ in 0..MAX_PAGES_PER_PASS {
        let page = page(store, index, cursor.as_ref(), n).await;
        assert!(
            page.sessions.len() <= n,
            "a page returned {} sessions for a limit of {n}",
            page.sessions.len()
        );
        for session in &page.sessions {
            if let Some(cursor) = &cursor {
                assert!(
                    cursor.session_id() < &session.session_id,
                    "a page must start strictly after its cursor"
                );
            }
            if let Some(previous) = seen.last() {
                assert!(
                    previous.session_id < session.session_id,
                    "a pass must return sessions in strictly increasing id order, once each"
                );
            }
        }
        seen.extend(page.sessions);
        match page.next {
            None => return seen,
            Some(next) => {
                if let Some(returned) = seen.last() {
                    assert!(
                        &returned.session_id <= next.session_id(),
                        "the next cursor must not fall behind a session already returned"
                    );
                }
                if let Some(cursor) = &cursor {
                    assert!(
                        cursor.session_id() < next.session_id(),
                        "the cursor must move strictly forward, or a pass never ends"
                    );
                }
                cursor = Some(next);
            }
        }
    }
    panic!("a pass did not finish within {MAX_PAGES_PER_PASS} pages; the cursor is not advancing")
}

async fn pass<S: SessionStore>(store: &S, index: Index, n: usize) -> Vec<MarkedSession> {
    pass_from(store, index, None, n).await
}

fn only(sessions: Vec<MarkedSession>, own: &BTreeSet<SessionId>) -> Vec<MarkedSession> {
    sessions
        .into_iter()
        .filter(|session| own.contains(&session.session_id))
        .collect()
}

fn ids(sessions: &[MarkedSession]) -> Vec<SessionId> {
    sessions
        .iter()
        .map(|session| session.session_id.clone())
        .collect()
}

async fn find<S: SessionStore>(store: &S, index: Index, sid: &SessionId) -> Option<MarkedSession> {
    pass(store, index, 64)
        .await
        .into_iter()
        .find(|session| &session.session_id == sid)
}

/// The session's permanent mark, if it has one.
async fn permanent_mark<S: SessionStore>(store: &S, sid: &SessionId) -> Option<MarkedSession> {
    find(store, Index::Permanent, sid).await
}

/// The session's mark as a pending page reports it, if it is pending.
async fn pending_mark<S: SessionStore>(store: &S, sid: &SessionId) -> Option<MarkedSession> {
    find(store, PENDING, sid).await
}

async fn marked_append<S: SessionStore>(
    store: &S,
    lease: &Lease,
    batch_len: usize,
    event_index: usize,
    project: ProjectId,
) -> Vec<SessionEvent> {
    let kinds = (0..batch_len)
        .map(|offset| text_event(&format!("entry {offset}")))
        .collect();
    store
        .append_events(lease, kinds, Some(LearningMark::new(event_index, project)))
        .await
        .expect("a valid mark on a held lease must append")
}

/// The expected record for the mark an append just wrote.
fn mark_for(sid: &SessionId, project: ProjectId, event: &SessionEvent) -> MarkedSession {
    MarkedSession {
        session_id: sid.clone(),
        project,
        seq: event.seq,
        marked_at_ms: event.at_ms,
    }
}

/// **The mark is the sequence the store assigned, not the batch tail.**
///
/// Marks the middle event of a batch that lands after earlier events, so a
/// backend that recorded the last sequence of the batch, the first, or a
/// batch-relative offset would each name the wrong event.
pub async fn the_mark_is_the_sequence_assigned_to_the_marked_event<S: SessionStore>(store: &S) {
    let (sid, lease) = held(store).await;
    store
        .append_events(&lease, vec![text_event("one"), text_event("two")], None)
        .await
        .unwrap();
    let events = marked_append(store, &lease, 3, 1, acme()).await;
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![3, 4, 5]
    );

    let expected = mark_for(&sid, acme(), &events[1]);
    assert_eq!(expected.seq, 4);
    assert_eq!(
        permanent_mark(store, &sid).await,
        Some(expected.clone()),
        "the permanent mark must name the marked event's own sequence"
    );
    assert_eq!(
        pending_mark(store, &sid).await,
        Some(expected),
        "the marking append must also make the session pending"
    );
}

/// **An unmarked append writes nothing to the index** — and does not disturb
/// a mark an earlier append wrote.
pub async fn an_unmarked_append_leaves_no_learning_entry<S: SessionStore>(store: &S) {
    let (plain, plain_lease) = held(store).await;
    store
        .append_events(&plain_lease, vec![text_event("plain")], None)
        .await
        .unwrap();
    assert_eq!(permanent_mark(store, &plain).await, None);
    assert_eq!(pending_mark(store, &plain).await, None);
    assert_eq!(
        store.clear_learning_mark(&plain, u64::MAX).await.unwrap(),
        ClearOutcome::Unmarked
    );
    assert_eq!(
        store.requeue_learning(&plain, 1).await.unwrap(),
        RequeueOutcome::Unmarked
    );
    assert_eq!(
        pending_mark(store, &plain).await,
        None,
        "requeue must not invent pending membership for an unmarked session"
    );

    let (sid, lease) = held(store).await;
    let events = marked_append(store, &lease, 1, 0, acme()).await;
    store
        .append_events(&lease, vec![text_event("after the mark")], None)
        .await
        .unwrap();
    let expected = mark_for(&sid, acme(), &events[0]);
    assert_eq!(
        permanent_mark(store, &sid).await,
        Some(expected.clone()),
        "an unmarked append must leave the latest mark where it was"
    );
    assert_eq!(pending_mark(store, &sid).await, Some(expected));
}

/// **A mark that names no event refuses the whole append, before any write.**
///
/// Also pins the precedence both backends share: an out-of-range index is a
/// malformed request whatever the store holds, so it is reported before
/// lease-lost and before not-found — while a valid mark on an unknown session
/// is still not-found.
pub async fn an_invalid_mark_refuses_the_append_before_any_write<S: LeaseControl>(store: &S) {
    let (sid, lease) = held(store).await;
    store
        .append_events(&lease, vec![text_event("before")], None)
        .await
        .unwrap();

    let past_end = store
        .append_events(
            &lease,
            vec![text_event("a"), text_event("b")],
            Some(LearningMark::new(2, acme())),
        )
        .await;
    assert!(
        matches!(
            past_end,
            Err(StoreError::InvalidLearningMark {
                event_index: 2,
                batch_len: 2,
                ..
            })
        ),
        "a mark one past the batch must be refused, got {past_end:?}"
    );
    let empty = store
        .append_events(&lease, Vec::new(), Some(LearningMark::new(0, acme())))
        .await;
    assert!(
        matches!(
            empty,
            Err(StoreError::InvalidLearningMark {
                event_index: 0,
                batch_len: 0,
                ..
            })
        ),
        "an empty batch has no event to mark, got {empty:?}"
    );
    assert_eq!(
        store.last_seq(&sid).await.unwrap(),
        1,
        "a refused mark must refuse its events too"
    );
    assert_eq!(permanent_mark(store, &sid).await, None);
    assert_eq!(pending_mark(store, &sid).await, None);

    store.force_expire_lease(&sid).await;
    store
        .acquire_lease(&sid, "node-b", TTL_MS)
        .await
        .unwrap()
        .expect("an expired lease must be takeable");
    let fenced = store
        .append_events(
            &lease,
            vec![text_event("stale")],
            Some(LearningMark::new(1, acme())),
        )
        .await;
    assert!(
        matches!(fenced, Err(StoreError::InvalidLearningMark { .. })),
        "a malformed mark outranks lease-lost, got {fenced:?}"
    );

    let ghost = Lease {
        session_id: SessionId::generate(),
        node_id: "node-a".into(),
        fencing_token: uuid::Uuid::new_v4(),
        expires_at_ms: u64::MAX,
    };
    let unknown = store
        .append_events(
            &ghost,
            vec![text_event("x")],
            Some(LearningMark::new(1, acme())),
        )
        .await;
    assert!(
        matches!(unknown, Err(StoreError::InvalidLearningMark { .. })),
        "a malformed mark outranks not-found, got {unknown:?}"
    );
    let unknown_valid = store
        .append_events(
            &ghost,
            vec![text_event("x")],
            Some(LearningMark::new(0, acme())),
        )
        .await;
    assert!(
        matches!(unknown_valid, Err(StoreError::SessionNotFound(_))),
        "a valid mark on an unknown session is still not-found, got {unknown_valid:?}"
    );
}

/// **A fenced marked append changes neither the log nor the index.**
///
/// A displaced writer that could still move a mark would register work the
/// log does not hold, or overwrite the successor's newer mark with a lower
/// sequence. Also pins that the fence outranks a project mismatch: a fenced
/// writer is refused as fenced and learns nothing about the index.
pub async fn a_fenced_append_writes_neither_events_nor_mark<S: LeaseControl>(store: &S) {
    let (sid, stale) = held(store).await;
    store.force_expire_lease(&sid).await;
    let successor = store
        .acquire_lease(&sid, "node-b", TTL_MS)
        .await
        .unwrap()
        .expect("an expired lease must be takeable");
    let events = marked_append(store, &successor, 1, 0, acme()).await;
    let expected = mark_for(&sid, acme(), &events[0]);
    assert_eq!(
        permanent_mark(store, &sid).await,
        Some(expected.clone()),
        "the successor's marked append must record its mark"
    );

    let fenced = store
        .append_events(
            &stale,
            vec![text_event("stale a"), text_event("stale b")],
            Some(LearningMark::new(1, acme())),
        )
        .await;
    assert!(matches!(fenced, Err(StoreError::LeaseLost { .. })));
    let fenced_elsewhere = store
        .append_events(
            &stale,
            vec![text_event("stale")],
            Some(LearningMark::new(0, globex())),
        )
        .await;
    assert!(
        matches!(fenced_elsewhere, Err(StoreError::LeaseLost { .. })),
        "the fence outranks a project mismatch, got {fenced_elsewhere:?}"
    );
    assert_eq!(store.last_seq(&sid).await.unwrap(), events[0].seq);
    assert_eq!(permanent_mark(store, &sid).await, Some(expected.clone()));
    assert_eq!(pending_mark(store, &sid).await, Some(expected));

    // A session nobody has marked stays unmarked when its only marked
    // append is fenced.
    let (never, never_stale) = held(store).await;
    store.force_expire_lease(&never).await;
    store
        .acquire_lease(&never, "node-b", TTL_MS)
        .await
        .unwrap()
        .expect("an expired lease must be takeable");
    let refused = store
        .append_events(
            &never_stale,
            vec![text_event("stale")],
            Some(LearningMark::new(0, acme())),
        )
        .await;
    assert!(matches!(refused, Err(StoreError::LeaseLost { .. })));
    assert_eq!(store.last_seq(&never).await.unwrap(), 0);
    assert_eq!(permanent_mark(store, &never).await, None);
    assert_eq!(pending_mark(store, &never).await, None);
}

/// **A delayed clear cannot remove a newer mark, even one written in the
/// same millisecond.**
///
/// The parent counterexample: a turn's clear is delayed until after the next
/// turn marked the session, which is also what a recovery clear racing a live
/// owner looks like. The newer mark's sequence is above the confirmed
/// watermark, so it stays. Consecutive marks are made until two share a
/// store-clock millisecond, which a fast store almost always gives within a
/// few tries; the assertion does not depend on getting one, because the
/// clear compares sequences and never time.
pub async fn a_delayed_clear_keeps_a_newer_mark<S: SessionStore>(store: &S) {
    let (sid, lease) = held(store).await;
    let mut marks: Vec<SessionEvent> = Vec::new();
    for _ in 0..MAX_TIE_ATTEMPTS {
        let events = marked_append(store, &lease, 1, 0, acme()).await;
        marks.extend(events);
        if let [.., older, newer] = marks.as_slice()
            && older.at_ms == newer.at_ms
        {
            break;
        }
    }
    let [.., older, newer] = marks.as_slice() else {
        panic!("at least two marks were made");
    };
    assert_ne!(
        older.seq, newer.seq,
        "two marks never share a sequence, whatever the clock says"
    );

    assert_eq!(
        store.clear_learning_mark(&sid, older.seq).await.unwrap(),
        ClearOutcome::Newer {
            mark_seq: newer.seq
        },
        "a clear that confirmed only the older mark must keep the newer one"
    );
    assert_eq!(
        pending_mark(store, &sid).await,
        Some(mark_for(&sid, acme(), newer)),
        "the newer mark must still be pending after the delayed clear"
    );

    assert_eq!(
        store.clear_learning_mark(&sid, newer.seq).await.unwrap(),
        ClearOutcome::Covered
    );
    assert_eq!(pending_mark(store, &sid).await, None);
}

/// **A covering clear ends pending membership only; the permanent mark
/// stays.** A watermark below the mark keeps the session pending, one at or
/// above it clears it, and clearing is idempotent — including at the top of
/// the `u64` range, where a backend comparing through a lossy number could
/// get the order wrong.
pub async fn a_covering_clear_keeps_the_permanent_mark<S: SessionStore>(store: &S) {
    let (sid, lease) = held(store).await;
    store
        .append_events(&lease, vec![text_event("unmarked")], None)
        .await
        .unwrap();
    let events = marked_append(store, &lease, 1, 0, acme()).await;
    let mark = mark_for(&sid, acme(), &events[0]);

    assert_eq!(
        store.clear_learning_mark(&sid, mark.seq - 1).await.unwrap(),
        ClearOutcome::Newer { mark_seq: mark.seq },
        "a watermark below the mark does not cover it"
    );
    assert_eq!(pending_mark(store, &sid).await, Some(mark.clone()));

    assert_eq!(
        store.clear_learning_mark(&sid, mark.seq).await.unwrap(),
        ClearOutcome::Covered
    );
    assert_eq!(pending_mark(store, &sid).await, None);
    assert_eq!(
        permanent_mark(store, &sid).await,
        Some(mark.clone()),
        "the permanent mark must survive the clear, or an audit could never find a loss"
    );

    assert_eq!(
        store.clear_learning_mark(&sid, u64::MAX).await.unwrap(),
        ClearOutcome::Covered
    );
    assert_eq!(pending_mark(store, &sid).await, None);
    assert_eq!(permanent_mark(store, &sid).await, Some(mark));
}

/// **Requeue restores only the mark that is still current.**
///
/// The audit's repair after the learner store lost delivered entries. A
/// requeue naming a mark that has since been replaced changes nothing — the
/// replacement was made pending by its own append, and whether it is still
/// pending is the replacement's business.
pub async fn requeue_restores_only_the_current_mark<S: SessionStore>(store: &S) {
    let (sid, lease) = held(store).await;
    let older = marked_append(store, &lease, 1, 0, acme()).await;
    let newer = marked_append(store, &lease, 1, 0, acme()).await;
    let (older, newer) = (older[0].seq, newer[0].seq);
    assert_eq!(
        store.clear_learning_mark(&sid, newer).await.unwrap(),
        ClearOutcome::Covered
    );

    assert_eq!(
        store.requeue_learning(&sid, older).await.unwrap(),
        RequeueOutcome::Mismatch { mark_seq: newer },
        "a stale requeue must be ignored"
    );
    assert_eq!(
        store.requeue_learning(&sid, newer + 1).await.unwrap(),
        RequeueOutcome::Mismatch { mark_seq: newer },
        "a requeue naming a mark that never existed must be ignored"
    );
    assert_eq!(pending_mark(store, &sid).await, None);

    assert_eq!(
        store.requeue_learning(&sid, newer).await.unwrap(),
        RequeueOutcome::Requeued
    );
    assert_eq!(
        store.requeue_learning(&sid, newer).await.unwrap(),
        RequeueOutcome::Requeued,
        "requeue is idempotent"
    );
    let pending = pending_mark(store, &sid)
        .await
        .expect("the current mark must be pending again");
    assert_eq!(pending.seq, newer);
    assert_eq!(permanent_mark(store, &sid).await, Some(pending));
}

/// **Paging reaches every session, including past ones the consumer never
/// finishes.**
///
/// The failure this rules out: a repeated oldest-N query with no cursor
/// returns the same head forever, so one session whose delivery always fails
/// (or whose owner holds it) starves every session behind it. Here every
/// session stays leased and nothing is ever cleared, yet one pass with any
/// page size visits each exactly once, a second pass visits them again, and
/// the idle filter never stalls the cursor. The sessions are marked in quick
/// succession, so many share a store-clock millisecond; the order is by id
/// and never by time, so ties cannot reorder or hide anything.
///
/// Also pins the honest mutation guarantee: a session added behind the
/// cursor mid-pass is not returned until the next pass, and one cleared
/// ahead of the cursor is not returned at all.
pub async fn pages_reach_every_session_past_unfinished_ones<S: SessionStore>(store: &S) {
    // One unique prefix keeps this test's sessions contiguous in id order,
    // whatever else a shared index holds.
    let prefix = SessionId::generate().into_string();
    let mut own = BTreeSet::new();
    let mut leases = Vec::new();
    for i in 0..7 {
        let sid = SessionId::new(format!("{prefix}-{i:02}"));
        let lease = create_and_hold(store, &sid).await;
        marked_append(store, &lease, 1, 0, acme()).await;
        own.insert(sid);
        leases.push(lease);
    }
    let expected: Vec<SessionId> = own.iter().cloned().collect();
    // Just before this test's sessions, so the pages below start on them.
    let start = LearningCursor::after(SessionId::new(prefix.clone()));

    // Control: a consumer that never advances its cursor sees one head,
    // forever — the starvation the cursor exists to prevent.
    for _ in 0..3 {
        assert_eq!(
            ids(&page(store, PENDING, Some(&start), 1).await.sessions),
            expected[..1].to_vec(),
            "an unadvanced page is a fixed point; only the cursor makes progress"
        );
    }

    for n in [1, 2, 3, 7, 64] {
        assert_eq!(
            ids(&only(pass(store, PENDING, n).await, &own)),
            expected,
            "a pass with pages of {n} must visit every pending session exactly once"
        );
        assert_eq!(
            ids(&only(pass(store, Index::Permanent, n).await, &own)),
            expected,
            "a permanent pass with pages of {n} must visit every marked session exactly once"
        );
    }
    assert_eq!(
        ids(&only(pass(store, PENDING, 2).await, &own)),
        expected,
        "nothing was cleared, so the next pass visits every session again"
    );
    assert!(
        only(
            pass(
                store,
                Index::Pending {
                    idle_for_ms: AN_HOUR_MS
                },
                2
            )
            .await,
            &own
        )
        .is_empty(),
        "no session marked just now has been idle for an hour"
    );
    assert!(
        only(
            pass(
                store,
                Index::Pending {
                    idle_for_ms: u64::MAX
                },
                2
            )
            .await,
            &own
        )
        .is_empty(),
        "an idle cutoff before the epoch admits nothing and must not wrap"
    );
    // An empty filtered page is not the end: each page still examines its
    // `limit` members and moves past them. A backend that scanned the whole
    // index per page, or stopped at the first page with nothing to return,
    // would pass the two full-pass checks above but not this.
    let not_idle = Index::Pending {
        idle_for_ms: AN_HOUR_MS,
    };
    let filtered = page(store, not_idle, Some(&start), 2).await;
    assert!(filtered.sessions.is_empty());
    assert_eq!(
        filtered.next,
        Some(LearningCursor::after(expected[1].clone())),
        "a page that returns nothing must still move past the members it examined"
    );
    let filtered = page(store, not_idle, filtered.next.as_ref(), 2).await;
    assert!(filtered.sessions.is_empty());
    assert_eq!(
        filtered.next,
        Some(LearningCursor::after(expected[3].clone())),
        "each filtered page advances by exactly its limit"
    );

    // Mid-pass mutation.
    let first = page(store, PENDING, Some(&start), 2).await;
    assert_eq!(ids(&first.sessions), expected[..2].to_vec());
    let resume = first.next.expect("more pending sessions remain");

    let behind = SessionId::new(format!("{prefix}-00b"));
    let behind_lease = create_and_hold(store, &behind).await;
    marked_append(store, &behind_lease, 1, 0, acme()).await;
    let ahead = &expected[3];
    let ahead_mark = pending_mark(store, ahead).await.expect("still pending");
    assert_eq!(
        store
            .clear_learning_mark(ahead, ahead_mark.seq)
            .await
            .unwrap(),
        ClearOutcome::Covered
    );
    own.insert(behind.clone());

    let rest = ids(&only(
        pass_from(store, PENDING, Some(resume), 2).await,
        &own,
    ));
    let mut remaining = expected[2..].to_vec();
    remaining.retain(|sid| sid != ahead);
    assert_eq!(
        rest, remaining,
        "the rest of the pass skips a session added behind the cursor and one cleared ahead of it"
    );

    let mut next_pass = expected.clone();
    next_pass.retain(|sid| sid != ahead);
    next_pass.insert(1, behind);
    assert_eq!(
        ids(&only(pass(store, PENDING, 2).await, &own)),
        next_pass,
        "the next pass picks up the session added behind the old cursor"
    );
}

/// **Marks are isolated by session and project, and a session's project is
/// fixed once marked.** A cross-project mark is refused before any write, so
/// one project's recovery can never be handed another's session.
pub async fn marks_are_isolated_by_session_and_project<S: SessionStore>(store: &S) {
    let (a, lease_a) = held(store).await;
    let (b, lease_b) = held(store).await;
    let a_events = marked_append(store, &lease_a, 1, 0, acme()).await;
    let b_events = marked_append(store, &lease_b, 1, 0, globex()).await;
    let a_mark = mark_for(&a, acme(), &a_events[0]);
    let b_mark = mark_for(&b, globex(), &b_events[0]);
    assert_eq!(permanent_mark(store, &a).await, Some(a_mark.clone()));
    assert_eq!(permanent_mark(store, &b).await, Some(b_mark.clone()));

    assert_eq!(
        store.clear_learning_mark(&a, a_mark.seq).await.unwrap(),
        ClearOutcome::Covered
    );
    assert_eq!(
        pending_mark(store, &b).await,
        Some(b_mark.clone()),
        "clearing one session must not touch another"
    );

    let moved = store
        .append_events(
            &lease_a,
            vec![text_event("moved")],
            Some(LearningMark::new(0, globex())),
        )
        .await;
    match moved {
        Err(StoreError::LearningProjectMismatch {
            session_id,
            marked,
            requested,
        }) => {
            assert_eq!(session_id, a);
            assert_eq!(marked, acme());
            assert_eq!(requested, globex());
        }
        other => panic!("a cross-project mark must be refused, got {other:?}"),
    }
    assert_eq!(
        store.last_seq(&a).await.unwrap(),
        a_mark.seq,
        "the refused mark must refuse its events too"
    );
    assert_eq!(permanent_mark(store, &a).await, Some(a_mark.clone()));
    assert_eq!(
        pending_mark(store, &a).await,
        None,
        "a refused mark must not make the session pending"
    );

    let again = marked_append(store, &lease_a, 1, 0, acme()).await;
    assert_eq!(
        pending_mark(store, &a).await,
        Some(mark_for(&a, acme(), &again[0])),
        "the owning project may keep marking"
    );
    assert_eq!(permanent_mark(store, &b).await, Some(b_mark.clone()));
    assert_eq!(pending_mark(store, &b).await, Some(b_mark));
}

/// **Discovery needs nothing but the session store.**
///
/// The revision-2 gap: a session whose every learner-store call failed was
/// never registered, and once it went idle nothing found it. Here the turn
/// marks its event and ends, no learner store exists at all, and a recovery
/// task that knows nothing about the session still finds it, with the mark
/// and every source event through it, from this store alone.
pub async fn a_marked_session_is_discoverable_without_a_learner_store<S: SessionStore>(store: &S) {
    let (sid, lease) = held(store).await;
    let events = marked_append(store, &lease, 3, 2, acme()).await;
    store.release_lease(&lease).await.unwrap();

    let found = pending_mark(store, &sid)
        .await
        .expect("an idle marked session must be discoverable from the source store");
    assert_eq!(found, mark_for(&sid, acme(), &events[2]));
    let replay = store
        .read_events(&sid, 0, usize::try_from(found.seq).unwrap())
        .await
        .unwrap();
    assert_eq!(
        replay, events,
        "the source store alone must replay every event through the mark"
    );
}

async fn create_and_hold<S: SessionStore>(store: &S, sid: &SessionId) -> Lease {
    assert!(store.create_session(sid, "affinity").await.unwrap());
    store
        .acquire_lease(sid, "node-a", TTL_MS)
        .await
        .unwrap()
        .expect("nothing contends for a fresh session's lease")
}
