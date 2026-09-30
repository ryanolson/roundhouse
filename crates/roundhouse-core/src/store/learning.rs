// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Source-side learning discovery: the index a marked append writes.
//!
//! A learner (not built yet — see `agent-docs/DRAFT-online-routing-learner.md`
//! §11.7; only this source-discovery mechanism, L3b, is accepted there, per its
//! §21 ruling and §22 addendum) folds some session events into shared counters
//! in a separate store. Every call to that store can fail, so the record of
//! *which sessions still owe it entries* cannot live there: a session whose
//! every learner-store call failed would never be registered, and nothing
//! would find it once the session went idle. The index lives here instead,
//! written by the same atomic, lease-fenced step that appends the source
//! event, so an entry-producing event that is durable is also discoverable.
//!
//! Two structures per backend:
//!
//! - **The permanent mark**: for every session that was ever marked, the log
//!   sequence of its newest marked event, the project that owns it, and the
//!   store clock at marking. Never removed. It is what an audit or an offline
//!   rebuild enumerates, so a later loss in the learner store can still be
//!   found and repaired from the source.
//! - **Pending membership**: the sessions whose current mark no learner
//!   watermark has yet been confirmed to cover. Removed only by
//!   [`SessionStore::clear_learning_mark`](super::SessionStore::clear_learning_mark) with a watermark at or above the
//!   mark, and restored only by [`SessionStore::requeue_learning`](super::SessionStore::requeue_learning) naming the
//!   mark that is still current.
//!
//! **The mark's log sequence is its identity; the clock is not.** Two marks
//! written in the same millisecond have different sequences, so a delayed
//! clear that confirmed only the older one cannot remove the newer one. The
//! marking time only schedules recovery through the idle filter of
//! [`SessionStore::pending_learning`](super::SessionStore::pending_learning).
//!
//! **Both enumerations are ordered by session id bytes, never by time**, and
//! resume strictly after a [`LearningCursor`]. The draft ordered pending work
//! oldest-first by marking time. A repeated oldest-first page with no cursor
//! returns the same head forever, so one session the consumer can never
//! finish would starve every session behind it; and a time-scored order has
//! no bounded, tie-safe resume point in Redis, whose lexicographic range only
//! works when every score is equal. Session ids are unique, so the byte order
//! is total and the resume point is exact.
//!
//! What a page guarantees, for either enumeration: a *pass* is the sequence
//! of pages from no cursor until a page returns `next: None`. A session that
//! is a member for the whole pass is returned exactly once in it (for pending
//! pages, if it is also idle when the cursor reaches it). A session added or
//! removed during the pass may or may not be returned. No session is returned
//! twice in one pass, because the cursor only moves forward. One page
//! examines at most `limit` members, idle or not, and the cursor moves past
//! every member it examined, so a consumer that never manages to finish a
//! session still reaches the ones after it.
//!
//! **Not wired yet.** `Session::commit` passes no mark, so ordinary session
//! appends stay unmarked until a later slice projects learner entries from the
//! event log and decides which events produce one. Nothing here reads the
//! learner store, and nothing here is runtime recovery: this is the source
//! half of the mechanism and its contract, not a working learner.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::ops::Bound;

use crate::control::ProjectId;
use crate::ids::SessionId;

use super::StoreError;

/// Which event of an append batch is the newest learning-entry-producing one,
/// and the project whose learner owns the session.
///
/// Names a position, never a sequence: the store assigns sequences inside the
/// fenced append, and only the store knows what it assigned. A caller that
/// computed a sequence itself could name one a concurrent writer took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningMark {
    event_index: usize,
    project: ProjectId,
}

impl LearningMark {
    /// Mark the event at `event_index` (zero-based) of the batch it rides
    /// with. Checked against the batch by the append, before anything is
    /// written.
    pub fn new(event_index: usize, project: ProjectId) -> Self {
        Self {
            event_index,
            project,
        }
    }

    pub fn event_index(&self) -> usize {
        self.event_index
    }

    pub fn project(&self) -> &ProjectId {
        &self.project
    }

    /// Refuse a mark that names no event of a `batch_len`-event batch.
    ///
    /// The one spelling of the rule both backends call before they touch
    /// storage. An out-of-range index is a caller defect whatever state the
    /// store is in, so it outranks not-found and lease-lost: a malformed
    /// request is refused without any I/O.
    pub fn check_against(
        &self,
        session_id: &SessionId,
        batch_len: usize,
    ) -> Result<(), StoreError> {
        if self.event_index < batch_len {
            Ok(())
        } else {
            Err(StoreError::InvalidLearningMark {
                session_id: session_id.clone(),
                event_index: self.event_index,
                batch_len,
            })
        }
    }
}

/// A session's latest mark, as the store recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkedSession {
    pub session_id: SessionId,
    pub project: ProjectId,
    /// The log sequence the store assigned to the marked event — the mark's
    /// identity, and what a clear or a requeue is compared against.
    pub seq: u64,
    /// The store clock when the mark was written. Schedules recovery through
    /// the idle filter; never compared as an identity.
    pub marked_at_ms: u64,
}

/// Where the next page of an enumeration starts: strictly after this session
/// id in byte order.
///
/// Public to construct because a position is not a capability: resuming
/// after any id is always a well-defined page. `None` in place of a cursor
/// starts a pass from the beginning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningCursor(SessionId);

impl LearningCursor {
    pub fn after(session_id: SessionId) -> Self {
        Self(session_id)
    }

    pub fn session_id(&self) -> &SessionId {
        &self.0
    }
}

/// One page of an enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningPage {
    /// Returned sessions in byte order of their ids. For a pending page, only
    /// the examined members that were idle.
    pub sessions: Vec<MarkedSession>,
    /// Resume point: the last member this page examined, when it examined a
    /// full `limit` of them. `None` means the page reached the end of the
    /// index and the pass is complete. A full page can be followed by an
    /// empty final one.
    pub next: Option<LearningCursor>,
}

/// What [`SessionStore::clear_learning_mark`](super::SessionStore::clear_learning_mark) found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearOutcome {
    /// The confirmed watermark covers the current mark: the session is not
    /// pending, whether this call removed it or an earlier one had. The
    /// permanent mark is kept.
    Covered,
    /// The current mark is newer than the confirmed watermark. Pending
    /// membership is kept, because entries through `mark_seq` are not all
    /// confirmed delivered.
    Newer { mark_seq: u64 },
    /// The session has never been marked.
    Unmarked,
}

/// What [`SessionStore::requeue_learning`](super::SessionStore::requeue_learning) found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequeueOutcome {
    /// The named mark is still current; the session is pending.
    Requeued,
    /// The current mark is `mark_seq`, not the one named, and nothing
    /// changed. A newer mark was pending when it was written, so the stale
    /// request has nothing to restore.
    Mismatch { mark_seq: u64 },
    /// The session has never been marked.
    Unmarked,
}

// ---------------------------------------------------------------------------
// In-memory index
// ---------------------------------------------------------------------------

/// [`MemoryStore`](super::MemoryStore)'s learning index. It lives under the
/// same lock as the session records, which is what makes a marked append one
/// step here. Ordered maps rather than hash maps because both enumerations
/// are ranges after a cursor: a page costs a seek plus `limit` steps and
/// never walks the whole index.
#[derive(Default)]
pub(super) struct MemoryIndex {
    /// The permanent marks. Never shrinks.
    marks: BTreeMap<SessionId, StoredMark>,
    /// The sessions whose current mark is not yet confirmed delivered. Every
    /// member has an entry in `marks`.
    pending: BTreeSet<SessionId>,
}

struct StoredMark {
    project: ProjectId,
    seq: u64,
    marked_at_ms: u64,
}

impl MemoryIndex {
    /// Refuse a mark for a project other than the one the session is already
    /// marked in. Called before any write of the append it guards.
    pub(super) fn check_project(
        &self,
        session_id: &SessionId,
        project: &ProjectId,
    ) -> Result<(), StoreError> {
        match self.marks.get(session_id) {
            Some(stored) if &stored.project != project => {
                Err(StoreError::LearningProjectMismatch {
                    session_id: session_id.clone(),
                    marked: stored.project.clone(),
                    requested: project.clone(),
                })
            }
            _ => Ok(()),
        }
    }

    /// Record `seq`, the sequence the append just assigned to the marked
    /// event, as the session's latest mark, and make the session pending.
    pub(super) fn record(
        &mut self,
        session_id: &SessionId,
        mark: LearningMark,
        seq: u64,
        marked_at_ms: u64,
    ) {
        self.marks.insert(
            session_id.clone(),
            StoredMark {
                project: mark.project,
                seq,
                marked_at_ms,
            },
        );
        self.pending.insert(session_id.clone());
    }

    pub(super) fn clear(&mut self, session_id: &SessionId, confirmed_through: u64) -> ClearOutcome {
        match self.marks.get(session_id) {
            None => ClearOutcome::Unmarked,
            Some(stored) if stored.seq <= confirmed_through => {
                self.pending.remove(session_id);
                ClearOutcome::Covered
            }
            Some(stored) => ClearOutcome::Newer {
                mark_seq: stored.seq,
            },
        }
    }

    pub(super) fn requeue(&mut self, session_id: &SessionId, mark_seq: u64) -> RequeueOutcome {
        match self.marks.get(session_id) {
            None => RequeueOutcome::Unmarked,
            Some(stored) if stored.seq == mark_seq => {
                self.pending.insert(session_id.clone());
                RequeueOutcome::Requeued
            }
            Some(stored) => RequeueOutcome::Mismatch {
                mark_seq: stored.seq,
            },
        }
    }

    /// A pending page. `cutoff` is the latest marking time that counts as
    /// idle, or `None` when the idle window reaches back before the clock's
    /// epoch and nothing can qualify.
    pub(super) fn pending_page(
        &self,
        after: Option<&LearningCursor>,
        cutoff: Option<u64>,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        let mut sessions = Vec::new();
        let mut examined = 0;
        let mut last = None;
        for session_id in self.pending.range(range_after(after)).take(limit.get()) {
            examined += 1;
            last = Some(session_id);
            let stored = self.marks.get(session_id).ok_or_else(|| {
                StoreError::Backend(anyhow::anyhow!(
                    "pending learning session `{session_id}` has no mark"
                ))
            })?;
            if cutoff.is_some_and(|cutoff| stored.marked_at_ms <= cutoff) {
                sessions.push(stored.marked(session_id));
            }
        }
        Ok(LearningPage {
            sessions,
            next: next_cursor(examined, limit, last),
        })
    }

    pub(super) fn marked_page(
        &self,
        after: Option<&LearningCursor>,
        limit: NonZeroUsize,
    ) -> LearningPage {
        let sessions: Vec<MarkedSession> = self
            .marks
            .range(range_after(after))
            .take(limit.get())
            .map(|(session_id, stored)| stored.marked(session_id))
            .collect();
        let next = next_cursor(
            sessions.len(),
            limit,
            sessions.last().map(|marked| &marked.session_id),
        );
        LearningPage { sessions, next }
    }
}

impl StoredMark {
    fn marked(&self, session_id: &SessionId) -> MarkedSession {
        MarkedSession {
            session_id: session_id.clone(),
            project: self.project.clone(),
            seq: self.seq,
            marked_at_ms: self.marked_at_ms,
        }
    }
}

fn range_after(after: Option<&LearningCursor>) -> (Bound<&SessionId>, Bound<&SessionId>) {
    match after {
        Some(cursor) => (Bound::Excluded(cursor.session_id()), Bound::Unbounded),
        None => (Bound::Unbounded, Bound::Unbounded),
    }
}

/// A full page may have more behind it; a short one reached the end.
fn next_cursor(
    examined: usize,
    limit: NonZeroUsize,
    last: Option<&SessionId>,
) -> Option<LearningCursor> {
    match last {
        Some(session_id) if examined == limit.get() => {
            Some(LearningCursor::after(session_id.clone()))
        }
        _ => None,
    }
}
