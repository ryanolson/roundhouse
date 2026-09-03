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

#[cfg(any(test, feature = "test-support"))]
pub mod contract;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

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
    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
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

/// Non-durable [`SessionStore`] for tests and single-process runs.
///
/// Lease semantics are modelled faithfully — including expiry and takeover —
/// so failover logic can be tested without standing up Redis.
#[derive(Default, Clone)]
pub struct MemoryStore {
    sessions: Arc<RwLock<HashMap<SessionId, SessionRecord>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Force-expire a session's lease. Test hook for simulating a dead owner
    /// without waiting out a TTL.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn expire_lease_now(&self, session_id: &SessionId) {
        if let Some(record) = self.sessions.write().await.get_mut(session_id)
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
        let mut sessions = self.sessions.write().await;
        if sessions.contains_key(session_id) {
            return Ok(false);
        }
        sessions.insert(
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
        let mut sessions = self.sessions.write().await;
        let record = sessions
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
        let mut sessions = self.sessions.write().await;
        let record = sessions
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
        let mut sessions = self.sessions.write().await;
        if let Some(record) = sessions.get_mut(&lease.session_id)
            && record.is_current_tenure(lease)
        {
            record.lease = None;
        }
        Ok(())
    }

    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        let sessions = self.sessions.read().await;
        let record = sessions
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
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let mut sessions = self.sessions.write().await;
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

        let at_ms = now_ms();
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
        Ok(appended)
    }

    async fn read_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let sessions = self.sessions.read().await;
        let record = sessions
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
        let sessions = self.sessions.read().await;
        let record = sessions
            .get(session_id)
            .ok_or_else(|| StoreError::SessionNotFound(session_id.clone()))?;
        Ok(record.events.last().map_or(0, |event| event.seq))
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
