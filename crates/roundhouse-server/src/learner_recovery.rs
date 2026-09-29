// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learner recovery task: idle sessions with entries owed are found in
//! the session store's index and delivered without the engine (draft section
//! 11.7, milestone M9 of `agent-docs/PLAN-online-routing-learner.md`).
//!
//! **Why it exists.** The engine delivers a session's entries in the tail of
//! its next turn. A session that never turns again, whose every learner-store
//! call failed, or whose node died between the source append and the apply,
//! would hold its entries forever. The session store's index finds them: the
//! append that wrote an entry-producing event marked the session in the same
//! atomic step, so every durable entry is discoverable whatever the learner
//! store did.
//!
//! **One sweep**, every `sweep_interval`:
//!
//! 1. A page of pending sessions idle for at least `idle_after_ms`, by the
//!    session store's clock (`SessionStore::pending_learning`). For each: read
//!    the learner store's watermark, replay the log above it lease-free, and
//!    deliver through [`Delivery`](crate::engine::learning::delivery) -- the
//!    same code the engine's tail runs -- up to `pages_per_session_per_sweep`
//!    pages. The mark is cleared with the watermark the store confirmed, so
//!    it goes only when the store holds every entry through it.
//! 2. A page of the audit (`SessionStore::learning_sessions`): a session whose
//!    learner watermark is below its permanent mark lost state after a clear,
//!    and `requeue_learning` makes it pending again. The audit never clears.
//!
//! **It never appends and never takes a lease**, and it does not ask
//! `is_leased`: the entry identity rule makes an apply safe while the owner
//! runs, the clear predicate keeps any newer mark, and a backend that
//! inherits the trait's `true` default would otherwise stall recovery for
//! every session. The idle window keeps most live sessions out of the page.
//!
//! **Sequences, not threads.** A session is one append-only history
//! (`label#g{n}`); a compaction starts a new generation, which is a new
//! session here. The body for one session is a function of its project and
//! id, so a superseded generation is delivered and cleared like any other.
//!
//! **An outage ends the sweep, backs off, and logs once.** A store that says
//! it is down -- a learner-store watermark read or apply that answers
//! `Unavailable`, a watermark read that times out, a session-store call that
//! fails with a backend error, or an index call (a page or a requeue) that
//! times out -- is met by the first session of a sweep and would be met by
//! every other, so the sweep stops there with the marks in place, the next
//! sweep waits twice as long (up to [`MAX_BACKOFF_FACTOR`] intervals), and
//! one warning covers the outage however many sweeps and sessions it spans.
//! The first clean sweep resets both.
//!
//! **One session's trouble is not an outage.** A key or field holding
//! foreign data (`WrongType`), a stop, a gap the backfill cannot close, a log
//! that is gone or corrupt (`CorruptLog`, which covers a log key of another
//! type), a stored mark the index cannot read, and an apply, replay,
//! backfill or clear of this session that runs past its timeout each hold
//! that session only, and the sweep goes on: ending it would leave the
//! cursor on the same session, and one session nobody can finish would
//! starve every project behind it. A timeout is one session's because the
//! next session's watermark read is what probes the store.
//!
//! **Which holds warn, and how often.** A foreign watermark, a replay or
//! backfill that cannot replay the log, an apply that meets a foreign key,
//! and a clear that fails, times out or cannot read the mark each warn once
//! per session and mark: again only after the session is delivered or marked
//! anew, or after a full pass that did not hold it again this way. A member
//! with no stored mark at all and one whose stored mark cannot be parsed are
//! both named as unreadable, and each warns once per session until it is
//! readable again. A stop logs one error when it stops the session, and the
//! task does not visit the session again until a restart. An apply that
//! times out and a gap the backfill cannot close are counted in
//! `learning.delivery` and not logged.
//!
//! **The pass cursors are this task's memory.** Each index is walked in
//! session id order and resumes after the last session a sweep finished; a
//! sweep that stops on an outage keeps its cursor, so the session it could
//! not reach is the first one the next sweep tries.

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use roundhouse_core::control::ProjectId;
use roundhouse_core::ids::SessionId;
use roundhouse_core::learn_store::LearnerError;
use roundhouse_core::metrics::{DeliveryOutcome, MetricsRecorder};
use roundhouse_core::session::{SessionError, SessionState};
use roundhouse_core::store::{
    ClearOutcome, LearningCursor, LearningPage, MarkedSession, RequeueOutcome, SessionStore,
    StoreError,
};

use crate::engine::learning::RoutingLearner;
use crate::engine::learning::delivery::{Delivered, Delivery, HeldMark, HeldSessions, Source};

/// The most intervals one backoff waits: 8, reached after three outages in a
/// row. Long enough that an outage costs a store a handful of probes per
/// minute at the section 4 cadence, short enough that recovery resumes within
/// a few minutes of the store returning.
pub const MAX_BACKOFF_FACTOR: u32 = 8;

/// The resolved `learner_recovery` block (see
/// `control_config::learner_recovery`). Every count and duration is non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryCadence {
    /// The wait between two sweeps when the stores answer.
    pub sweep_interval: Duration,
    /// How long a session's newest mark must have stood before a sweep
    /// delivers it, by the session store's clock.
    pub idle_after_ms: u64,
    /// Pending sessions one sweep examines.
    pub max_sessions_per_sweep: NonZeroUsize,
    /// Pages of entries one sweep applies for one session.
    pub pages_per_session_per_sweep: NonZeroUsize,
    /// Marked sessions one sweep audits.
    pub audit_sessions_per_sweep: NonZeroUsize,
    /// One learner-store watermark read.
    pub read_timeout: Duration,
    /// One learner-store apply.
    pub apply_timeout: Duration,
    /// One session-store call: an index page, a replay, a gap backfill, a
    /// clear or a requeue.
    pub source_timeout: Duration,
}

/// What one sweep did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Sessions whose mark a clear covered this sweep.
    pub cleared: usize,
    /// Sessions the audit made pending again.
    pub requeued: usize,
    /// Whether a store outage ended the sweep early.
    pub outage: bool,
}

/// The recovery task over one engine's learner. Built by
/// [`Engine::learner_recovery`](crate::Engine::learner_recovery), so it
/// shares the engine's learner store, stopped sessions, once-per-outage
/// flags and delivery counters.
pub struct LearnerRecovery<S: SessionStore> {
    sessions: Arc<S>,
    learner: Arc<RoutingLearner>,
    metrics: Arc<MetricsRecorder>,
    cadence: RecoveryCadence,
    /// Where the pending pass resumes.
    pending_after: Option<LearningCursor>,
    /// Where the audit pass resumes.
    audit_after: Option<LearningCursor>,
    /// Sweeps in a row that met an outage: the backoff exponent, and the
    /// once-per-outage flag (the warning fires on the first).
    outages: u32,
    /// The pending pass's sessions already warned about as held, each under
    /// the mark its page named, so one held on every sweep warns once per
    /// mark, here and in the delivery. Pruned at the end of each full pass
    /// to the sessions that pass held again (see [`HeldSessions`]).
    held: HeldSessions,
    /// The audit pass's own held set, pruned at the end of each audit pass.
    /// Separate because the audit walks every marked session, pending or
    /// not: pruned with the pending pass, a foreign watermark the audit meets
    /// on a delivered session would warn again on every pending pass. An
    /// unreadable mark is held here for both passes, under no mark, because
    /// the audit is the pass that meets it whether it is pending or not: one
    /// warning for it, forgotten at the end of the first audit pass that no
    /// longer meets it.
    audit_held: HeldSessions,
}

/// Why a sweep stopped early: a store did not answer.
struct Outage;

/// What one learner-store watermark read found.
enum Watermark {
    /// The store answered.
    At(u64),
    /// This session's watermark key holds foreign data: the store is up, and
    /// this one session is held until an operator removes the key.
    Foreign,
}

impl<S: SessionStore> LearnerRecovery<S> {
    pub(crate) fn new(
        sessions: Arc<S>,
        learner: Arc<RoutingLearner>,
        metrics: Arc<MetricsRecorder>,
        cadence: RecoveryCadence,
    ) -> Self {
        Self {
            sessions,
            learner,
            metrics,
            cadence,
            pending_after: None,
            audit_after: None,
            outages: 0,
            held: HeldSessions::default(),
            audit_held: HeldSessions::default(),
        }
    }

    /// One sweep: a page of pending sessions, then a page of the audit. See
    /// the module doc.
    pub async fn sweep(&mut self) -> SweepReport {
        let mut report = SweepReport::default();
        let outcome = match self.deliver_page(&mut report).await {
            Ok(()) => self.audit_page(&mut report).await,
            Err(outage) => Err(outage),
        };
        match outcome {
            Ok(()) => self.outages = 0,
            Err(Outage) => {
                report.outage = true;
                if self.outages == 0 {
                    tracing::warn!(
                        "the learner recovery task cannot reach a store; its sweeps back off, and \
                         pending sessions wait with their marks until it answers"
                    );
                }
                self.outages = self.outages.saturating_add(1);
            }
        }
        report
    }

    /// How many sessions the task holds as already warned about. Test-only:
    /// the set must stay bounded by the sessions still pending.
    #[cfg(feature = "test-support")]
    pub fn held_len(&self) -> usize {
        self.held.len()
    }

    /// The wait before the next sweep: the interval, doubled for each sweep
    /// in a row that met an outage, up to [`MAX_BACKOFF_FACTOR`] intervals.
    pub fn next_delay(&self) -> Duration {
        backoff(self.cadence.sweep_interval, self.outages)
    }

    async fn deliver_page(&mut self, report: &mut SweepReport) -> Result<(), Outage> {
        let page = self
            .index(self.sessions.pending_learning(
                self.pending_after.as_ref(),
                self.cadence.idle_after_ms,
                self.cadence.max_sessions_per_sweep,
            ))
            .await?;
        self.unreadable_marks(&page.unreadable);
        for marked in &page.sessions {
            if self.deliver_session(marked).await? {
                report.cleared += 1;
            }
        }
        if page.next.is_none() {
            self.held.end_pass();
        }
        // Moved only once the whole page is done, so an outage retries the
        // session it could not reach first.
        self.pending_after = page.next;
        Ok(())
    }

    /// Deliver one session's owed entries, up to the page budget, and clear
    /// its mark with the watermark the store confirmed. `Ok(true)` when the
    /// clear covered the mark.
    async fn deliver_session(&self, marked: &MarkedSession) -> Result<bool, Outage> {
        let MarkedSession {
            session_id: session,
            project,
            seq: mark,
            ..
        } = marked;
        if self.learner.is_stopped(project, session) {
            return Ok(false);
        }
        let Watermark::At(mut confirmed) = self
            .watermark(project, session, (&self.held, *mark))
            .await?
        else {
            return Ok(false);
        };
        let delivery = Delivery {
            learner: &self.learner,
            sessions: self.sessions.as_ref(),
            counters: self.metrics.learning_delivery(),
            project,
            session,
            apply_timeout: self.cadence.apply_timeout,
            source_timeout: Some(self.cadence.source_timeout),
            held: Some(HeldMark {
                sessions: &self.held,
                mark: *mark,
            }),
        };
        for _ in 0..self.cadence.pages_per_session_per_sweep.get() {
            let Some(state) = self.replay(project, session, confirmed, *mark).await? else {
                // No log to deliver from: nothing more this task can do.
                return Ok(false);
            };
            match delivery
                .deliver(Source::Replayed {
                    state: Box::new(state),
                    confirmed,
                })
                .await
            {
                Delivered::Confirmed {
                    watermark,
                    cleared,
                    more,
                } => {
                    self.held.delivered(session, Some(*mark));
                    confirmed = watermark;
                    if !more {
                        return Ok(cleared == ClearOutcome::Covered);
                    }
                }
                Delivered::Held(hold) if hold.is_outage() => return Err(Outage),
                // One session's hold (see `Hold::is_outage`): the entries
                // stay pending and the next sweep tries again.
                Delivered::Nothing | Delivered::Held(_) => return Ok(false),
            }
        }
        // The page budget ran out with entries still owed: the last clear
        // confirmed a watermark below the mark, so the session stays pending
        // for the next sweep.
        Ok(false)
    }

    async fn audit_page(&mut self, report: &mut SweepReport) -> Result<(), Outage> {
        let page = self
            .index(self.sessions.learning_sessions(
                self.audit_after.as_ref(),
                self.cadence.audit_sessions_per_sweep,
            ))
            .await?;
        self.unreadable_marks(&page.unreadable);
        for marked in &page.sessions {
            if self.audit_session(marked).await? {
                report.requeued += 1;
            }
        }
        if page.next.is_none() {
            self.audit_held.end_pass();
        }
        self.audit_after = page.next;
        Ok(())
    }

    /// Make one session pending again if the learner store lost what an
    /// earlier clear confirmed: its watermark is below the permanent mark.
    /// `requeue_learning` changes nothing when the mark moved on, and a
    /// session that is already pending stays pending, so the audit does not
    /// ask which one it is. It never clears.
    async fn audit_session(&self, marked: &MarkedSession) -> Result<bool, Outage> {
        let MarkedSession {
            session_id,
            project,
            seq,
            ..
        } = marked;
        match self
            .watermark(project, session_id, (&self.audit_held, *seq))
            .await?
        {
            Watermark::At(watermark) if watermark < *seq => {}
            Watermark::At(_) | Watermark::Foreign => return Ok(false),
        }
        let requeued = match self
            .source(self.sessions.requeue_learning(session_id, *seq))
            .await?
        {
            Ok(requeued) => requeued,
            // The mark became unreadable after the page read it: that one
            // session's data, not the index being down.
            Err(_) => {
                self.unreadable_marks(std::slice::from_ref(session_id));
                return Ok(false);
            }
        };
        if requeued == RequeueOutcome::Requeued {
            tracing::debug!(
                %project, session = %session_id, mark = seq,
                "the learner store's watermark is below this session's mark; requeued for delivery"
            );
        }
        Ok(requeued == RequeueOutcome::Requeued)
    }

    /// The learner store's watermark for `session`. A foreign one is held
    /// under `held`'s mark, in the set of the pass that asked.
    async fn watermark(
        &self,
        project: &ProjectId,
        session: &SessionId,
        (held, mark): (&HeldSessions, u64),
    ) -> Result<Watermark, Outage> {
        match tokio::time::timeout(
            self.cadence.read_timeout,
            self.learner.store.watermark(project, session),
        )
        .await
        {
            Ok(Ok(watermark)) => Ok(Watermark::At(watermark)),
            Ok(Err(error @ LearnerError::WrongType { .. })) => {
                if held.first_hold(session, Some(mark)) {
                    tracing::warn!(
                        %project, %session, %error,
                        "the learner store holds foreign data for this session; the recovery \
                         task holds it and goes on with the rest"
                    );
                }
                self.metrics
                    .learning_delivery()
                    .record(project, DeliveryOutcome::Unavailable);
                Ok(Watermark::Foreign)
            }
            Ok(Err(error)) => {
                tracing::debug!(%project, %session, %error, "the learner store did not answer a watermark read");
                Err(Outage)
            }
            Err(_) => {
                tracing::debug!(%project, %session, "a learner-store watermark read timed out");
                Err(Outage)
            }
        }
    }

    /// The log above `floor`, lease-free. `None` for a session whose log is
    /// gone or could not be replayed in time, which holds that session and is
    /// not an outage: skipping it lets the sweep go on. A backend failure is
    /// the session store's, and an outage.
    async fn replay(
        &self,
        project: &ProjectId,
        session: &SessionId,
        floor: u64,
        mark: u64,
    ) -> Result<Option<SessionState>, Outage> {
        let first_hold = || self.held.first_hold(session, Some(mark));
        let replayed = tokio::time::timeout(
            self.cadence.source_timeout,
            SessionState::project_learning(self.sessions.as_ref(), session, floor),
        )
        .await;
        match replayed {
            Ok(Ok(state)) => Ok(Some(state)),
            // The session store failed after its index page answered: it is
            // down, and every session behind this one would meet the same.
            Ok(Err(SessionError::Store(error @ StoreError::Backend(_)))) => {
                tracing::debug!(%project, %session, %error, "the log replay met a backend failure");
                Err(Outage)
            }
            // The store read the log, and its content is foreign: this one
            // session's fault, which an outage would stop the sweep on at
            // every attempt.
            Ok(Err(SessionError::Store(error @ StoreError::CorruptLog { .. }))) => {
                if first_hold() {
                    tracing::warn!(
                        %project, %session, %error,
                        "the log of this session is corrupt; the recovery task holds it and goes \
                         on with the rest"
                    );
                }
                Ok(None)
            }
            Ok(Err(SessionError::Store(StoreError::SessionNotFound(_)))) => {
                if first_hold() {
                    tracing::warn!(
                        %project, %session,
                        "a session marked for learning is not in the log; the recovery task skips it"
                    );
                }
                Ok(None)
            }
            Ok(Err(error)) => {
                if first_hold() {
                    tracing::warn!(%project, %session, %error, "the log replay failed; the session is held");
                }
                Ok(None)
            }
            // One session's replay, after the index already answered: held,
            // not an outage, so a log too long for `source_timeout` cannot
            // stall the sessions behind it.
            Err(_) => {
                if first_hold() {
                    tracing::warn!(%project, %session, "the log replay timed out; the session is held");
                }
                Ok(None)
            }
        }
    }

    async fn index(
        &self,
        call: impl Future<Output = Result<LearningPage, StoreError>>,
    ) -> Result<LearningPage, Outage> {
        // A page names an unreadable mark rather than failing on it, so no
        // page error is one session's.
        self.source(call).await?.map_err(|error| {
            tracing::debug!(%error, "a learning index page failed");
            Outage
        })
    }

    /// One session-store call under the source timeout. A `CorruptLog` is
    /// one session's data, and comes back as `Ok(Err(..))` for the caller to
    /// hold; any other failure, or the timeout, is the store, and an outage.
    async fn source<T>(
        &self,
        call: impl Future<Output = Result<T, StoreError>>,
    ) -> Result<Result<T, StoreError>, Outage> {
        match tokio::time::timeout(self.cadence.source_timeout, call).await {
            Ok(Ok(value)) => Ok(Ok(value)),
            Ok(Err(error @ StoreError::CorruptLog { .. })) => Ok(Err(error)),
            Ok(Err(error)) => {
                tracing::debug!(%error, "a learning index call failed");
                Err(Outage)
            }
            Err(_) => {
                tracing::debug!("a learning index call timed out");
                Err(Outage)
            }
        }
    }

    /// Sessions whose stored mark the store cannot read: each is held, and
    /// warned about once, in the audit's set (see `audit_held`).
    fn unreadable_marks(&self, sessions: &[SessionId]) {
        for session in sessions {
            if self.audit_held.first_hold(session, None) {
                tracing::warn!(
                    %session,
                    "the stored learning mark of this session is unreadable; the recovery task \
                     cannot deliver or audit it, and goes on with the rest"
                );
            }
        }
    }
}

/// The wait after `outages` sweeps in a row met an outage: `interval` times
/// `2^outages`, at most [`MAX_BACKOFF_FACTOR`] intervals. A pure function so
/// the arithmetic is tested without a clock.
pub fn backoff(interval: Duration, outages: u32) -> Duration {
    let factor = 1u32
        .checked_shl(outages)
        .unwrap_or(MAX_BACKOFF_FACTOR)
        .min(MAX_BACKOFF_FACTOR);
    interval.saturating_mul(factor)
}

/// The spawned task. Dropping it stops the sweeps, so the composition root
/// holds it for as long as it serves, under a real name: a `_` binding drops
/// it at once.
pub struct RecoveryTask {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for RecoveryTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl<S: SessionStore + 'static> LearnerRecovery<S> {
    /// Sweep now, then after every [`Self::next_delay`], until the returned
    /// handle is dropped. The first sweep runs at once, so a restart delivers
    /// what the last process left owed without waiting an interval.
    pub fn spawn(mut self) -> RecoveryTask {
        tracing::info!(
            sweep_interval_ms = self.cadence.sweep_interval.as_millis() as u64,
            idle_after_ms = self.cadence.idle_after_ms,
            "the learner recovery task is running: it delivers sessions idle with entries owed, \
             and audits delivered ones against the learner store"
        );
        let handle = tokio::spawn(async move {
            loop {
                self.sweep().await;
                tokio::time::sleep(self.next_delay()).await;
            }
        });
        RecoveryTask { handle }
    }
}
