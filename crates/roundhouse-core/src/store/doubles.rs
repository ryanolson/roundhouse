// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared [`SessionStore`] test doubles.
//!
//! Every trait method [`SessionStore`] gains (the learning-index slice added
//! four of them) used to multiply across every "wrap a backend and sabotage
//! one method" double in the workspace, because each one hand-wrote all
//! eleven forwarders next to the one it actually cared about. Three of those
//! doubles were not even sabotaging anything — [`ReplayLog`] here — and were
//! copy-pasted byte-for-byte across `roundhouse-core` and `roundhouse-server`
//! test files. This module is the one place both patterns live now: a
//! finished [`ReplayLog`] for read-only replay, and [`Delegating`], a trait a
//! sabotaging double implements instead of [`SessionStore`] so that
//! forwarding the other ten or eleven methods is a default it inherits
//! rather than a body it repeats.
//!
//! Gated the same as [`super::contract`] — compiled for this crate's own
//! tests and, under `test-support`, for dependent crates' integration tests —
//! since both are test-only by construction and nothing in production code
//! may depend on either existing.

use std::num::NonZeroUsize;

use async_trait::async_trait;

use crate::event::{SessionEvent, SessionEventKind};
use crate::ids::SessionId;
use crate::store::{
    ClearOutcome, LearningCursor, LearningMark, LearningPage, Lease, RequeueOutcome, SessionStore,
    StoreError,
};

/// A read-only [`SessionStore`] over a fixed log.
///
/// [`crate::session::Session::project`] and its callers read events through
/// the store rather than a slice, so replaying a log that has already been
/// through the wire — or a *prefix* of one, which is what a successor
/// picking a session up mid-history sees — needs a reader over exactly those
/// events. The writing half is unreachable by construction: nothing here
/// ever acquires a lease, so no caller under test can observe anything but
/// [`Self::read_events`] and [`Self::last_seq`].
pub struct ReplayLog(pub Vec<SessionEvent>);

impl ReplayLog {
    pub fn new(events: Vec<SessionEvent>) -> Self {
        Self(events)
    }
}

#[async_trait]
impl SessionStore for ReplayLog {
    async fn create_session(&self, _: &SessionId, _: &str) -> Result<bool, StoreError> {
        unreachable!("a replay log is never written to")
    }

    async fn acquire_lease(
        &self,
        _: &SessionId,
        _: &str,
        _: u64,
    ) -> Result<Option<Lease>, StoreError> {
        unreachable!("a replay log is never written to")
    }

    async fn renew_lease(&self, _: &Lease, _: u64) -> Result<Option<Lease>, StoreError> {
        unreachable!("a replay log is never written to")
    }

    async fn release_lease(&self, _: &Lease) -> Result<(), StoreError> {
        unreachable!("a replay log is never written to")
    }

    async fn append_events(
        &self,
        _: &Lease,
        _: Vec<SessionEventKind>,
        _: Option<LearningMark>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        unreachable!("a replay log is never written to")
    }

    async fn read_events(
        &self,
        _: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        Ok(self
            .0
            .iter()
            .filter(|event| event.seq > after_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn last_seq(&self, _: &SessionId) -> Result<u64, StoreError> {
        Ok(self.0.last().map_or(0, |event| event.seq))
    }

    async fn clear_learning_mark(&self, _: &SessionId, _: u64) -> Result<ClearOutcome, StoreError> {
        unreachable!("a replay log has no learning index")
    }

    async fn requeue_learning(&self, _: &SessionId, _: u64) -> Result<RequeueOutcome, StoreError> {
        unreachable!("a replay log has no learning index")
    }

    async fn pending_learning(
        &self,
        _: Option<&LearningCursor>,
        _: u64,
        _: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        unreachable!("a replay log has no learning index")
    }

    async fn learning_sessions(
        &self,
        _: Option<&LearningCursor>,
        _: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        unreachable!("a replay log has no learning index")
    }
}

/// A [`SessionStore`] double built by wrapping one real backend and
/// sabotaging exactly the methods a test cares about.
///
/// Implement this instead of [`SessionStore`] directly. Every method here has
/// a default that forwards to [`Self::backend`], so a double that fails
/// `append_events` while it is armed writes only that one method — the other
/// ten (eleven, once a sabotaging double also needs `is_leased`) stay
/// inherited rather than re-typed per double, which is what let nine
/// near-identical forwarder blocks accumulate across the workspace before
/// this existed.
///
/// **`is_leased`'s default here matches [`SessionStore`]'s own, not a
/// forward.** Every double built directly against [`SessionStore`] before
/// this trait existed either hand-wrote a forward to
/// `self.inner.is_leased(..)` or omitted the method and took
/// [`SessionStore`]'s own default (`Ok(true)`, "cannot prove idle")
/// unchanged. Defaulting `Delegating::is_leased` to a forward would have
/// flipped the omitting doubles onto their backend's real lease state — a
/// behavior change this consolidation must not make. A double that wants the
/// forward overrides `is_leased` with the same one-line body it always
/// wrote; a double that wants the old default writes nothing, exactly as
/// before.
#[async_trait]
pub trait Delegating: Send + Sync {
    type Backend: SessionStore;

    fn backend(&self) -> &Self::Backend;

    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        let _ = session_id;
        Ok(true)
    }

    async fn create_session(
        &self,
        session_id: &SessionId,
        model_policy: &str,
    ) -> Result<bool, StoreError> {
        self.backend()
            .create_session(session_id, model_policy)
            .await
    }

    async fn acquire_lease(
        &self,
        session_id: &SessionId,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<Option<Lease>, StoreError> {
        self.backend()
            .acquire_lease(session_id, node_id, ttl_ms)
            .await
    }

    async fn renew_lease(&self, lease: &Lease, ttl_ms: u64) -> Result<Option<Lease>, StoreError> {
        self.backend().renew_lease(lease, ttl_ms).await
    }

    async fn release_lease(&self, lease: &Lease) -> Result<(), StoreError> {
        self.backend().release_lease(lease).await
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
        mark: Option<LearningMark>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        self.backend().append_events(lease, kinds, mark).await
    }

    async fn read_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        self.backend()
            .read_events(session_id, after_seq, limit)
            .await
    }

    async fn last_seq(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        self.backend().last_seq(session_id).await
    }

    async fn clear_learning_mark(
        &self,
        session_id: &SessionId,
        confirmed_through: u64,
    ) -> Result<ClearOutcome, StoreError> {
        self.backend()
            .clear_learning_mark(session_id, confirmed_through)
            .await
    }

    async fn requeue_learning(
        &self,
        session_id: &SessionId,
        mark_seq: u64,
    ) -> Result<RequeueOutcome, StoreError> {
        self.backend().requeue_learning(session_id, mark_seq).await
    }

    async fn pending_learning(
        &self,
        after: Option<&LearningCursor>,
        idle_for_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        self.backend()
            .pending_learning(after, idle_for_ms, limit)
            .await
    }

    async fn learning_sessions(
        &self,
        after: Option<&LearningCursor>,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        self.backend().learning_sessions(after, limit).await
    }
}

#[async_trait]
impl<T: Delegating + 'static> SessionStore for T {
    async fn create_session(
        &self,
        session_id: &SessionId,
        model_policy: &str,
    ) -> Result<bool, StoreError> {
        Delegating::create_session(self, session_id, model_policy).await
    }

    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        Delegating::is_leased(self, session_id).await
    }

    async fn acquire_lease(
        &self,
        session_id: &SessionId,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<Option<Lease>, StoreError> {
        Delegating::acquire_lease(self, session_id, node_id, ttl_ms).await
    }

    async fn renew_lease(&self, lease: &Lease, ttl_ms: u64) -> Result<Option<Lease>, StoreError> {
        Delegating::renew_lease(self, lease, ttl_ms).await
    }

    async fn release_lease(&self, lease: &Lease) -> Result<(), StoreError> {
        Delegating::release_lease(self, lease).await
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
        mark: Option<LearningMark>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        Delegating::append_events(self, lease, kinds, mark).await
    }

    async fn read_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        Delegating::read_events(self, session_id, after_seq, limit).await
    }

    async fn last_seq(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        Delegating::last_seq(self, session_id).await
    }

    async fn clear_learning_mark(
        &self,
        session_id: &SessionId,
        confirmed_through: u64,
    ) -> Result<ClearOutcome, StoreError> {
        Delegating::clear_learning_mark(self, session_id, confirmed_through).await
    }

    async fn requeue_learning(
        &self,
        session_id: &SessionId,
        mark_seq: u64,
    ) -> Result<RequeueOutcome, StoreError> {
        Delegating::requeue_learning(self, session_id, mark_seq).await
    }

    async fn pending_learning(
        &self,
        after: Option<&LearningCursor>,
        idle_for_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        Delegating::pending_learning(self, after, idle_for_ms, limit).await
    }

    async fn learning_sessions(
        &self,
        after: Option<&LearningCursor>,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        Delegating::learning_sessions(self, after, limit).await
    }
}
