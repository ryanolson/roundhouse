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
//! **An outage ends the sweep, backs off, and logs once.** A store that does
//! not answer is met by the first session of a sweep and would be met by
//! every other, so the sweep stops there with the marks in place, the next
//! sweep waits twice as long (up to [`MAX_BACKOFF_FACTOR`] intervals), and one
//! warning covers the outage however many sweeps and sessions it spans. The
//! first clean sweep resets both.
//!
//! **One session's trouble is not an outage.** A key holding foreign data
//! (`WrongType`), a replay that fails or times out, or a stopped session holds
//! that session only, and the sweep goes on: ending it would leave the cursor
//! on the same session, and one session nobody can finish would starve every
//! project behind it.
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
use roundhouse_core::session::SessionState;
use roundhouse_core::store::{
    ClearOutcome, LearningCursor, LearningPage, MarkedSession, RequeueOutcome, SessionStore,
    StoreError,
};

use crate::engine::learning::RoutingLearner;
use crate::engine::learning::delivery::{Delivered, Delivery, Source};

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
    /// One session-store call: an index page, a replay, a clear or a requeue.
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
        for marked in &page.sessions {
            if self
                .deliver_session(&marked.project, &marked.session_id)
                .await?
            {
                report.cleared += 1;
            }
        }
        // Moved only once the whole page is done, so an outage retries the
        // session it could not reach first.
        self.pending_after = page.next;
        Ok(())
    }

    /// Deliver one session's owed entries, up to the page budget, and clear
    /// its mark with the watermark the store confirmed. `Ok(true)` when the
    /// clear covered the mark.
    async fn deliver_session(
        &self,
        project: &ProjectId,
        session: &SessionId,
    ) -> Result<bool, Outage> {
        if self.learner.is_stopped(project, session) {
            return Ok(false);
        }
        let Watermark::At(mut confirmed) = self.watermark(project, session).await? else {
            return Ok(false);
        };
        let delivery = Delivery {
            learner: &self.learner,
            sessions: self.sessions.as_ref(),
            counters: self.metrics.learning_delivery(),
            project,
            session,
            apply_timeout: self.cadence.apply_timeout,
        };
        for _ in 0..self.cadence.pages_per_session_per_sweep.get() {
            let Some(state) = self.replay(project, session, confirmed).await? else {
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
                    confirmed = watermark;
                    if !more {
                        return Ok(cleared == ClearOutcome::Covered);
                    }
                }
                Delivered::Held(hold) if hold.is_outage() => return Err(Outage),
                // Stopped, or a gap the backfill did not close: the entries
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
        for marked in &page.sessions {
            if self.audit_session(marked).await? {
                report.requeued += 1;
            }
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
        match self.watermark(project, session_id).await? {
            Watermark::At(watermark) if watermark < *seq => {}
            Watermark::At(_) | Watermark::Foreign => return Ok(false),
        }
        let requeued = self
            .source(self.sessions.requeue_learning(session_id, *seq))
            .await?;
        if requeued == RequeueOutcome::Requeued {
            tracing::debug!(
                %project, session = %session_id, mark = seq,
                "the learner store's watermark is below this session's mark; requeued for delivery"
            );
        }
        Ok(requeued == RequeueOutcome::Requeued)
    }

    async fn watermark(
        &self,
        project: &ProjectId,
        session: &SessionId,
    ) -> Result<Watermark, Outage> {
        match tokio::time::timeout(
            self.cadence.read_timeout,
            self.learner.store.watermark(project, session),
        )
        .await
        {
            Ok(Ok(watermark)) => Ok(Watermark::At(watermark)),
            Ok(Err(error @ LearnerError::WrongType { .. })) => {
                tracing::warn!(
                    %project, %session, %error,
                    "the learner store holds foreign data for this session; the recovery task \
                     holds it and goes on with the rest"
                );
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
    /// not an outage: skipping it lets the sweep go on.
    async fn replay(
        &self,
        project: &ProjectId,
        session: &SessionId,
        floor: u64,
    ) -> Result<Option<SessionState>, Outage> {
        let replayed = tokio::time::timeout(
            self.cadence.source_timeout,
            SessionState::project_learning(self.sessions.as_ref(), session, floor),
        )
        .await;
        match replayed {
            Ok(Ok(state)) => Ok(Some(state)),
            Ok(Err(roundhouse_core::session::SessionError::Store(
                StoreError::SessionNotFound(_),
            ))) => {
                tracing::warn!(
                    %project, %session,
                    "a session marked for learning is not in the log; the recovery task skips it"
                );
                Ok(None)
            }
            // One session's replay, after the index already answered: held,
            // not an outage, so a log too long for `source_timeout` cannot
            // stall the sessions behind it. A store that is really down
            // fails the next index call, and that ends the sweep.
            Ok(Err(error)) => {
                tracing::warn!(%project, %session, %error, "the log replay failed; the session is held");
                Ok(None)
            }
            Err(_) => {
                tracing::warn!(%project, %session, "the log replay timed out; the session is held");
                Ok(None)
            }
        }
    }

    async fn index(
        &self,
        call: impl Future<Output = Result<LearningPage, StoreError>>,
    ) -> Result<LearningPage, Outage> {
        self.source(call).await
    }

    /// One session-store call under the source timeout.
    async fn source<T>(
        &self,
        call: impl Future<Output = Result<T, StoreError>>,
    ) -> Result<T, Outage> {
        match tokio::time::timeout(self.cadence.source_timeout, call).await {
            Ok(Ok(value)) => Ok(value),
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
