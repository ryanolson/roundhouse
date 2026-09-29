// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A read-only, in-memory session source over a dump of marked logs: what a
//! fixture run of the calibrator reads.
//!
//! **Why a dump and not the memory store.** Both session stores stamp `at_ms`
//! with their own clock at append, and every latency the report prints is a
//! difference of those stamps. A fixture replayed into a store gets new
//! stamps, so it could never give the same report twice, nor the report a
//! Redis run over the same logs gives. A dump keeps each event exactly as the
//! store it was read from wrote it: [`LogDump::capture`] reads a store the way
//! the calibrator does, and [`DumpStore`] serves those bytes back through the
//! `SessionStore` read methods the calibrator calls.
//!
//! Every write method is refused, and so is the pending enumeration, which no
//! reader of a dump has a use for.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::ops::Bound;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::source::ENUMERATION_PAGE;
use crate::control::ProjectId;
use crate::event::{SessionEvent, SessionEventKind};
use crate::ids::SessionId;
use crate::store::{
    ClearOutcome, LearningCursor, LearningMark, LearningPage, Lease, MarkedSession, RequeueOutcome,
    SessionStore, StoreError,
};

/// A session's permanent mark, as the store recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DumpedMark {
    pub project: ProjectId,
    pub seq: u64,
    pub marked_at_ms: u64,
}

/// One marked session and its whole log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DumpedSession {
    pub session: SessionId,
    pub mark: DumpedMark,
    pub events: Vec<SessionEvent>,
}

/// Marked sessions and their logs, in byte order of their ids.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogDump {
    pub sessions: Vec<DumpedSession>,
}

impl LogDump {
    /// Read every marked session of `store` and its log. Reads only.
    ///
    /// A member whose mark the store cannot read has no mark to dump and is
    /// left out; the calibrator run against the store itself names it.
    pub async fn capture<S: SessionStore + ?Sized>(store: &S) -> Result<Self, StoreError> {
        let page_size = NonZeroUsize::new(ENUMERATION_PAGE).expect("a positive page");
        let mut marked = Vec::new();
        let mut cursor = None;
        loop {
            let page = store.learning_sessions(cursor.as_ref(), page_size).await?;
            marked.extend(page.sessions);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        marked.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        let mut sessions = Vec::with_capacity(marked.len());
        for mark in marked {
            let mut events = Vec::new();
            loop {
                let after = events.last().map_or(0, |event: &SessionEvent| event.seq);
                let batch = store.read_events(&mark.session_id, after, 1024).await?;
                if batch.is_empty() {
                    break;
                }
                events.extend(batch);
            }
            sessions.push(DumpedSession {
                session: mark.session_id,
                mark: DumpedMark {
                    project: mark.project,
                    seq: mark.seq,
                    marked_at_ms: mark.marked_at_ms,
                },
                events,
            });
        }
        Ok(Self { sessions })
    }
}

/// Why a dump was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DumpError {
    #[error("session `{0}` appears twice in the dump")]
    Repeated(SessionId),
    #[error("session `{session}`: {reason}")]
    Log { session: SessionId, reason: String },
}

/// A [`SessionStore`] that serves a [`LogDump`] and refuses every write.
#[derive(Debug, Clone)]
pub struct DumpStore {
    sessions: BTreeMap<SessionId, DumpedSession>,
}

impl DumpStore {
    /// Check the dump: each session once, each log contiguous from seq 1 and
    /// all its events its own, and each mark on an event of its log.
    pub fn new(dump: LogDump) -> Result<Self, DumpError> {
        let mut sessions = BTreeMap::new();
        for session in dump.sessions {
            let log = |reason: String| DumpError::Log {
                session: session.session.clone(),
                reason,
            };
            for (at, event) in session.events.iter().enumerate() {
                if event.seq != at as u64 + 1 {
                    return Err(log(format!("event {at} has seq {}", event.seq)));
                }
                if event.session_id != session.session {
                    return Err(log(format!("event {} names another session", event.seq)));
                }
            }
            if session.mark.seq == 0 || session.mark.seq > session.events.len() as u64 {
                return Err(log(format!(
                    "the mark names seq {}, outside the log",
                    session.mark.seq
                )));
            }
            let id = session.session.clone();
            if sessions.insert(id.clone(), session).is_some() {
                return Err(DumpError::Repeated(id));
            }
        }
        Ok(Self { sessions })
    }

    fn session(&self, session_id: &SessionId) -> Result<&DumpedSession, StoreError> {
        self.sessions
            .get(session_id)
            .ok_or_else(|| StoreError::SessionNotFound(session_id.clone()))
    }
}

fn read_only() -> StoreError {
    StoreError::Backend(anyhow::anyhow!("a log dump is read-only"))
}

#[async_trait]
impl SessionStore for DumpStore {
    async fn create_session(&self, _: &SessionId, _: &str) -> Result<bool, StoreError> {
        Err(read_only())
    }

    async fn acquire_lease(
        &self,
        _: &SessionId,
        _: &str,
        _: u64,
    ) -> Result<Option<Lease>, StoreError> {
        Err(read_only())
    }

    async fn renew_lease(&self, _: &Lease, _: u64) -> Result<Option<Lease>, StoreError> {
        Err(read_only())
    }

    async fn release_lease(&self, _: &Lease) -> Result<(), StoreError> {
        Err(read_only())
    }

    async fn append_events(
        &self,
        _: &Lease,
        _: Vec<SessionEventKind>,
        _: Option<LearningMark>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        Err(read_only())
    }

    async fn read_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        Ok(self
            .session(session_id)?
            .events
            .iter()
            .filter(|event| event.seq > after_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn last_seq(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        Ok(self
            .session(session_id)?
            .events
            .last()
            .map_or(0, |event| event.seq))
    }

    async fn clear_learning_mark(&self, _: &SessionId, _: u64) -> Result<ClearOutcome, StoreError> {
        Err(read_only())
    }

    async fn requeue_learning(&self, _: &SessionId, _: u64) -> Result<RequeueOutcome, StoreError> {
        Err(read_only())
    }

    async fn pending_learning(
        &self,
        _: Option<&LearningCursor>,
        _: u64,
        _: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        Err(StoreError::Backend(anyhow::anyhow!(
            "a log dump has no pending set"
        )))
    }

    /// The index's paging contract: byte order, strictly after the cursor,
    /// and a resume point only after a full page.
    async fn learning_sessions(
        &self,
        after: Option<&LearningCursor>,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        let lower = match after {
            Some(cursor) => Bound::Excluded(cursor.session_id().clone()),
            None => Bound::Unbounded,
        };
        let sessions: Vec<MarkedSession> = self
            .sessions
            .range((lower, Bound::Unbounded))
            .take(limit.get())
            .map(|(id, session)| MarkedSession {
                session_id: id.clone(),
                project: session.mark.project.clone(),
                seq: session.mark.seq,
                marked_at_ms: session.mark.marked_at_ms,
            })
            .collect();
        let next = (sessions.len() == limit.get())
            .then(|| {
                sessions
                    .last()
                    .map(|last| LearningCursor::after(last.session_id.clone()))
            })
            .flatten();
        Ok(LearningPage {
            sessions,
            next,
            unreadable: Vec::new(),
        })
    }
}
