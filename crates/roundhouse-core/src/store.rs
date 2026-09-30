// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Durable session state.
//!
//! The trait is deliberately small. Conversation items and the routing ledger
//! are *projections* of the event log rather than separately stored
//! collections, so there is exactly one write path and no way for the log and
//! the materialized state to disagree after a crash. A backend therefore only
//! has to provide an append-only log plus a lease.
//!
//! Two implementations are expected: the [`MemoryStore`] here (tests, single
//! process) and a Redis Streams backend in `roundhouse-store-redis`. What the
//! two must agree on is executable rather than prose: `store::contract` holds
//! the trait's guarantees as a generic test suite, and every backend —
//! including the memory one — is judged by that identical suite.
//!
//! `store::doubles` is the other side of the same coin: not a backend the
//! contract suite judges, but the shared shapes every test-only
//! [`SessionStore`] fixture across the workspace is built from — a read-only
//! replay over a fixed log, and a `Delegating` trait a sabotaging double
//! implements so that forwarding the methods it does not sabotage is
//! inherited rather than retyped.
//!
//! The store also keeps the source half of learning discovery (`learning`):
//! an append may carry a [`LearningMark`], and the same atomic, fenced step
//! that writes the events records it. That index is deliberately unwired —
//! `Session::commit` passes no mark until a later slice projects learner
//! entries from the log (`agent-docs/DRAFT-online-routing-learner.md` §11.7;
//! of that draft only this mechanism, L3b, is accepted, per §21). The contract
//! suite pins its guarantees now so the learner can be built on them.

#[cfg(any(test, feature = "test-support"))]
pub mod contract;
#[cfg(any(test, feature = "test-support"))]
pub mod doubles;
mod learning;

pub use learning::{
    ClearOutcome, LearningCursor, LearningMark, LearningPage, MarkedSession, RequeueOutcome,
};

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::control::ProjectId;
use crate::event::{SessionEvent, SessionEventKind};
use crate::ids::SessionId;
use crate::now_ms;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("session `{0}` not found")]
    SessionNotFound(SessionId),
    #[error("lease for session `{session_id}` is not held by `{node_id}`")]
    LeaseLost {
        session_id: SessionId,
        node_id: String,
    },
    /// The mark names no event of its batch. Refused before any write.
    #[error(
        "learning mark for session `{session_id}` names event {event_index} of a \
         {batch_len}-event batch"
    )]
    InvalidLearningMark {
        session_id: SessionId,
        event_index: usize,
        batch_len: usize,
    },
    /// The session is already marked for another project. Refused before any
    /// write: a learner session belongs to one project for life, and moving
    /// its mark would let one project's recovery deliver another's entries.
    #[error(
        "session `{session_id}` is marked for learning in project `{marked}`; \
         refusing a mark for project `{requested}`"
    )]
    LearningProjectMismatch {
        session_id: SessionId,
        marked: ProjectId,
        requested: ProjectId,
    },
    #[error("backend failure: {0}")]
    Backend(#[from] anyhow::Error),
}

/// Proof that one node tenure is the single writer for a session.
///
/// Every mutating call takes one. A node whose lease has expired — because it
/// stalled, or was partitioned, or died and came back — fails its next append
/// rather than writing behind the successor that took over. This is the only
/// thing standing between a failover and a split-brain log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub session_id: SessionId,
    pub node_id: String,
    /// Uniquely identifies this ownership tenure.
    ///
    /// When a node re-acquires a lease, the store mints a new token. This token
    /// fences every handle from the previous tenure. The node id identifies
    /// the owner. This token identifies the current acquisition.
    pub fencing_token: Uuid,
    pub expires_at_ms: u64,
}

impl Lease {
    pub fn is_expired_at(&self, now: u64) -> bool {
        now >= self.expires_at_ms
    }
}

#[async_trait]
pub trait SessionStore: Send + Sync + 'static {
    /// Create a session. Returns `false` if it already existed.
    async fn create_session(
        &self,
        session_id: &SessionId,
        model_policy: &str,
    ) -> Result<bool, StoreError>;

    /// Claim single-writer ownership, or `None` if another live node holds it.
    /// Every successful acquisition mints a fresh fencing token, including a
    /// re-acquisition by the current node, so older handles cannot revive.
    async fn acquire_lease(
        &self,
        session_id: &SessionId,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<Option<Lease>, StoreError>;

    /// Extend a held lease. `None` means it was lost and must be re-acquired.
    ///
    /// Validated the same way as [`SessionStore::append_events`]: against the
    /// stored record, so the caller may renew from a handle it has not
    /// refreshed. That is what lets a background heartbeat renew a lease the
    /// session handle is still using, and it is also why `None` here is
    /// final — the record now belongs to someone else, and the only correct
    /// response is to stop rather than to re-acquire behind the successor.
    async fn renew_lease(&self, lease: &Lease, ttl_ms: u64) -> Result<Option<Lease>, StoreError>;

    async fn release_lease(&self, lease: &Lease) -> Result<(), StoreError>;

    /// Whether some live tenure currently holds this session's lease.
    ///
    /// **The one lease question a reader may ask**, and it exists because a
    /// projection of the log cannot answer "is a turn writing this session
    /// right now" from the log alone: a turn mid-flight and a turn whose writer
    /// died leave byte-identical traces — items with no terminal event yet —
    /// and only the lease distinguishes them. `prefix_admission`'s
    /// `stored_conversation` reads it for exactly that (M11.2a's F3): items
    /// stamped by a response that never terminated may be superseded, but only
    /// once nobody is still producing them.
    ///
    /// Deliberately *not* an acquisition: the caller wants to know who is
    /// writing, not to become the writer, and `acquire_lease` on a free session
    /// would evict the very turn it is about to start.
    ///
    /// The default answers `true` — "this backend cannot prove the session is
    /// idle" — because the caller's conservative direction is to leave history
    /// alone: a backend that guessed `false` would let a reader supersede items
    /// a live turn is still committing. Every real backend overrides it; the
    /// default is for doubles that wrap one, and it costs them only the
    /// supersession.
    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        let _ = session_id;
        Ok(true)
    }

    /// Append events, assigning contiguous sequence numbers.
    ///
    /// Fails with [`StoreError::LeaseLost`] if `lease` is no longer valid, so a
    /// stalled writer cannot interleave with its successor.
    ///
    /// A [`Lease`] is a tenure identity — node plus fencing token — and not an
    /// expiry snapshot: validity is decided against the store's *current*
    /// record, so a handle whose own `expires_at_ms` has passed still appends
    /// while the tenure it names is live. Renewal is therefore free to happen
    /// on a separate task from the writes, and the session layer relies on it:
    /// its heartbeat renews the record while every append continues to go
    /// through the original handle. An implementation that instead rejected a
    /// stale-looking handle would fail every append made during a long turn.
    ///
    /// With a `mark`, the same atomic step also records the sequence it
    /// assigned to the marked event as the session's latest learning mark and
    /// makes the session pending. A fenced or refused append writes neither
    /// events nor mark. An out-of-range mark fails with
    /// [`StoreError::InvalidLearningMark`] before the store is consulted; a
    /// session already marked for another project fails with
    /// [`StoreError::LearningProjectMismatch`] after the fence check. The
    /// store keeps a project fixed once set, but it cannot check the project
    /// against the session's principal — that is the caller's word.
    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
        mark: Option<LearningMark>,
    ) -> Result<Vec<SessionEvent>, StoreError>;

    /// Read events with `seq > after_seq`, oldest first.
    async fn read_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError>;

    /// Highest assigned sequence number, or 0 for an empty session.
    async fn last_seq(&self, session_id: &SessionId) -> Result<u64, StoreError>;

    /// Drop the session's pending membership if its current mark is at or
    /// below `confirmed_through`, a watermark the learner store confirmed.
    /// The permanent mark stays.
    ///
    /// The predicate, not a token, is what makes a late clear safe: a mark
    /// written after the delivery being confirmed has a higher sequence, so a
    /// delayed clear, a retried one, or one from a node that lost its lease
    /// cannot remove it. Takes no lease and appends nothing. Consults only
    /// the index, so a session that was never created reads as
    /// [`ClearOutcome::Unmarked`].
    async fn clear_learning_mark(
        &self,
        session_id: &SessionId,
        confirmed_through: u64,
    ) -> Result<ClearOutcome, StoreError>;

    /// Make the session pending again if its current mark is still
    /// `mark_seq` — the audit's repair after the learner store lost what an
    /// earlier clear confirmed. A mismatch changes nothing: any newer mark was
    /// made pending by the append that wrote it.
    async fn requeue_learning(
        &self,
        session_id: &SessionId,
        mark_seq: u64,
    ) -> Result<RequeueOutcome, StoreError>;

    /// One page of pending sessions, in session id byte order after `after`.
    ///
    /// Examines at most `limit` pending members and returns those marked at
    /// least `idle_for_ms` ago by the store's own clock — the clock that
    /// stamped the marks, so a node clock that runs behind cannot hide every
    /// session. The cursor moves past every examined member, idle or not; see
    /// the `learning` module doc for what a pass guarantees.
    async fn pending_learning(
        &self,
        after: Option<&LearningCursor>,
        idle_for_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError>;

    /// One page of every session ever marked, in session id byte order after
    /// `after`, whether pending or not. For the audit and offline
    /// enumeration: the permanent marks outlive every clear.
    async fn learning_sessions(
        &self,
        after: Option<&LearningCursor>,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError>;
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SessionRecord {
    _model_policy: String,
    events: Vec<SessionEvent>,
    lease: Option<Lease>,
}

impl SessionRecord {
    fn is_current_tenure(&self, lease: &Lease) -> bool {
        self.lease.as_ref().is_some_and(|current| {
            current.node_id == lease.node_id && current.fencing_token == lease.fencing_token
        })
    }

    fn is_held_by(&self, lease: &Lease, now: u64) -> bool {
        self.is_current_tenure(lease)
            && self
                .lease
                .as_ref()
                .is_some_and(|current| !current.is_expired_at(now))
    }
}

/// Everything a [`MemoryStore`] holds, under one lock: that one lock is what
/// makes a marked append write its events and its mark in one step.
#[derive(Default)]
struct MemoryState {
    sessions: HashMap<SessionId, SessionRecord>,
    learning: learning::MemoryIndex,
}

/// Non-durable [`SessionStore`] for tests and single-process runs.
///
/// Lease semantics are modelled faithfully — including expiry and takeover —
/// so failover logic can be tested without standing up Redis.
#[derive(Default, Clone)]
pub struct MemoryStore {
    state: Arc<RwLock<MemoryState>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Force-expire a session's lease. Test hook for simulating a dead owner
    /// without waiting out a TTL.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn expire_lease_now(&self, session_id: &SessionId) {
        if let Some(record) = self.state.write().await.sessions.get_mut(session_id)
            && let Some(lease) = record.lease.as_mut()
        {
            lease.expires_at_ms = 0;
        }
    }
}

#[async_trait]
impl SessionStore for MemoryStore {
    async fn create_session(
        &self,
        session_id: &SessionId,
        model_policy: &str,
    ) -> Result<bool, StoreError> {
        let mut state = self.state.write().await;
        if state.sessions.contains_key(session_id) {
            return Ok(false);
        }
        state.sessions.insert(
            session_id.clone(),
            SessionRecord {
                _model_policy: model_policy.to_string(),
                ..Default::default()
            },
        );
        Ok(true)
    }

    async fn acquire_lease(
        &self,
        session_id: &SessionId,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<Option<Lease>, StoreError> {
        let mut state = self.state.write().await;
        let record = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| StoreError::SessionNotFound(session_id.clone()))?;

        let now = now_ms();
        // A live lease held by someone else blocks the claim; our own lease is
        // simply re-taken, which makes acquisition idempotent for a node that
        // is recovering rather than competing.
        if let Some(existing) = &record.lease
            && !existing.is_expired_at(now)
            && existing.node_id != node_id
        {
            return Ok(None);
        }

        let lease = Lease {
            session_id: session_id.clone(),
            node_id: node_id.to_string(),
            fencing_token: Uuid::new_v4(),
            expires_at_ms: now.saturating_add(ttl_ms),
        };
        record.lease = Some(lease.clone());
        Ok(Some(lease))
    }

    async fn renew_lease(&self, lease: &Lease, ttl_ms: u64) -> Result<Option<Lease>, StoreError> {
        let mut state = self.state.write().await;
        let record = state
            .sessions
            .get_mut(&lease.session_id)
            .ok_or_else(|| StoreError::SessionNotFound(lease.session_id.clone()))?;

        let now = now_ms();
        match record.is_held_by(lease, now) {
            true => {
                let renewed = Lease {
                    expires_at_ms: now.saturating_add(ttl_ms),
                    ..lease.clone()
                };
                record.lease = Some(renewed.clone());
                Ok(Some(renewed))
            }
            false => Ok(None),
        }
    }

    async fn release_lease(&self, lease: &Lease) -> Result<(), StoreError> {
        let mut state = self.state.write().await;
        if let Some(record) = state.sessions.get_mut(&lease.session_id)
            && record.is_current_tenure(lease)
        {
            record.lease = None;
        }
        Ok(())
    }

    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        let state = self.state.read().await;
        let record = state
            .sessions
            .get(session_id)
            .ok_or_else(|| StoreError::SessionNotFound(session_id.clone()))?;
        // Expiry, not just presence: a record left behind by a node that died
        // names a tenure nobody is writing under, which is precisely the state
        // the caller is asking about. `expire_lease_now` above is the test hook
        // that reaches it without waiting out a TTL.
        Ok(record
            .lease
            .as_ref()
            .is_some_and(|lease| !lease.is_expired_at(now_ms())))
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
        mark: Option<LearningMark>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        if let Some(mark) = &mark {
            mark.check_against(&lease.session_id, kinds.len())?;
        }
        let mut state = self.state.write().await;
        let MemoryState { sessions, learning } = &mut *state;
        let record = sessions
            .get_mut(&lease.session_id)
            .ok_or_else(|| StoreError::SessionNotFound(lease.session_id.clone()))?;

        let held = record.is_held_by(lease, now_ms());
        if !held {
            return Err(StoreError::LeaseLost {
                session_id: lease.session_id.clone(),
                node_id: lease.node_id.clone(),
            });
        }
        if let Some(mark) = &mark {
            learning.check_project(&lease.session_id, mark.project())?;
        }

        let at_ms = now_ms();
        let first_seq = record.events.len() as u64 + 1;
        let mut appended = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let seq = record.events.len() as u64 + 1;
            let event = SessionEvent {
                seq,
                session_id: lease.session_id.clone(),
                at_ms,
                kind,
            };
            record.events.push(event.clone());
            appended.push(event);
        }
        if let Some(mark) = mark {
            let seq = first_seq + mark.event_index() as u64;
            learning.record(&lease.session_id, mark, seq, at_ms);
        }
        Ok(appended)
    }

    async fn read_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let state = self.state.read().await;
        let record = state
            .sessions
            .get(session_id)
            .ok_or_else(|| StoreError::SessionNotFound(session_id.clone()))?;
        Ok(record
            .events
            .iter()
            .filter(|event| event.seq > after_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn last_seq(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        let state = self.state.read().await;
        let record = state
            .sessions
            .get(session_id)
            .ok_or_else(|| StoreError::SessionNotFound(session_id.clone()))?;
        Ok(record.events.last().map_or(0, |event| event.seq))
    }

    async fn clear_learning_mark(
        &self,
        session_id: &SessionId,
        confirmed_through: u64,
    ) -> Result<ClearOutcome, StoreError> {
        Ok(self
            .state
            .write()
            .await
            .learning
            .clear(session_id, confirmed_through))
    }

    async fn requeue_learning(
        &self,
        session_id: &SessionId,
        mark_seq: u64,
    ) -> Result<RequeueOutcome, StoreError> {
        Ok(self
            .state
            .write()
            .await
            .learning
            .requeue(session_id, mark_seq))
    }

    async fn pending_learning(
        &self,
        after: Option<&LearningCursor>,
        idle_for_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        // `now_ms` is the clock this store stamped the marks with.
        let cutoff = now_ms().checked_sub(idle_for_ms);
        self.state
            .read()
            .await
            .learning
            .pending_page(after, cutoff, limit)
    }

    async fn learning_sessions(
        &self,
        after: Option<&LearningCursor>,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        Ok(self.state.read().await.learning.marked_page(after, limit))
    }
}

#[cfg(test)]
mod tests {
    //! The memory store's conformance run.
    //!
    //! The assertions live in [`contract`](super::contract) and the macro is
    //! the list; this module only points both at [`MemoryStore`]. Each
    //! contract test still gets its own `#[tokio::test]`, so a failure names
    //! the violated invariant.

    use super::MemoryStore;

    crate::store_contract_suite!(MemoryStore::new());
}
