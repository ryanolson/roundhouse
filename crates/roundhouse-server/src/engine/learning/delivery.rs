// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The one delivery of a session's learning entries, for both of its callers
//! (draft sections 11.5 and 11.7, milestone M9).
//!
//! **The engine tail and the recovery task run this same code**: the page,
//! the apply under a timeout, the refill and the gap backfill, the stop
//! classes, the mark clear, and the [`LearningDelivery`] counters. A second
//! copy in the recovery task would be a second answer to "what does
//! `ChainDiverged` mean", and the two would drift the first time one of them
//! was fixed. They differ in exactly one step, and [`Source`] names it:
//!
//! - [`Source::Live`]: the engine's tail. The entries are the live fold's
//!   page, and a confirmed watermark is recorded in the session's own log as
//!   `LearningApplied` under the turn's lease before the mark is cleared.
//! - [`Source::Replayed`]: the recovery task. The entries are a lease-free
//!   replay above the learner store's own watermark, and nothing is appended:
//!   the task never takes a lease. The mark is cleared with the watermark the
//!   store confirmed, even when the replay held nothing to send (a lost
//!   acknowledgement), because the clear predicate (`mark <= watermark`) is
//!   exactly the proof that every entry through the mark is delivered.
//!
//! The refill of an empty page from the log's hint is reachable only from
//! [`Source::Live`]: a replay above a floor holds every entry above it up to
//! the page size, so its page is empty only when nothing is owed.

use std::sync::atomic::Ordering;
use std::time::Duration;

use roundhouse_core::control::ProjectId;
use roundhouse_core::ids::SessionId;
use roundhouse_core::learn_store::{LearnerError, LearningBatch};
use roundhouse_core::metrics::{DeliveryOutcome, LearningDelivery};
use roundhouse_core::routing::learn::EpochId;
use roundhouse_core::session::{LearningEntry, Session, SessionState};
use roundhouse_core::store::{ClearOutcome, SessionStore};

use super::RoutingLearner;

/// Where one delivery's entries come from, and whether the session's own log
/// records the confirmation. See the module doc.
pub(crate) enum Source<'a, S: SessionStore> {
    /// The engine's tail: the live fold, under the turn's lease.
    Live(&'a mut Session<S>),
    /// The recovery task: a replay whose page holds the entries above
    /// `confirmed`, the watermark the learner store reported before it.
    ///
    /// Boxed: a folded state is large, and the live variant is a reference.
    Replayed {
        state: Box<SessionState>,
        confirmed: u64,
    },
}

/// What one delivery did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivered {
    /// Nothing is owed, or this session's delivery is stopped.
    Nothing,
    /// The learner store confirmed `watermark`, and the mark clear answered
    /// `cleared`. `more` is whether the page left entries behind it.
    Confirmed {
        watermark: u64,
        cleared: ClearOutcome,
        more: bool,
    },
    /// The entries stay pending under their mark.
    Held(Hold),
}

/// Why a delivery left its entries pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hold {
    /// A refusal no retry fixes: the session's delivery stopped.
    Stopped,
    /// The learner store did not answer, or ran past the apply timeout.
    LearnerUnavailable,
    /// A key this session's batch names holds foreign data (`WrongType`):
    /// the store answers, and this one session cannot be delivered until an
    /// operator removes the key.
    ForeignKey,
    /// The session store could not replay this session's log.
    SourceUnavailable,
    /// A second gap in one delivery: the backfill did not close it.
    GapPersisted,
    /// The store applied, and the acknowledgement or the mark clear failed.
    AcknowledgementFailed,
}

impl Hold {
    /// Whether a store is out, so a sweep should end rather than visit the
    /// next session only to meet the same outage.
    ///
    /// **Only what says the store itself is down.** A hold that belongs to
    /// one session -- a foreign key, a replay of its log that failed, a stop
    /// -- must not end the sweep: the sweep would end with its cursor on that
    /// session, the next would start there, and one session nobody can
    /// finish would starve every session behind it in every project, which
    /// is the failure the index's byte-order cursor exists to prevent.
    pub(crate) fn is_outage(self) -> bool {
        matches!(self, Hold::LearnerUnavailable | Hold::AcknowledgementFailed)
    }
}

/// One session's delivery: everything [`deliver`] needs besides its source.
pub(crate) struct Delivery<'a, S: SessionStore> {
    pub(crate) learner: &'a RoutingLearner,
    pub(crate) sessions: &'a S,
    pub(crate) counters: &'a LearningDelivery,
    pub(crate) project: &'a ProjectId,
    pub(crate) session: &'a SessionId,
    pub(crate) apply_timeout: Duration,
}

impl<S: SessionStore> Delivery<'_, S> {
    /// Deliver one page of `source`'s entries, then record and clear.
    ///
    /// **At most one refill and one gap backfill.** Each is a full read-only
    /// replay of the log: the refill when the live page ran dry, the gap
    /// backfill when the store reported a gap. They are separate budgets
    /// because a refilled page can meet a gap (the store lost what it
    /// acknowledged), and a refill that spent the gap's backfill would leave
    /// every later tail to refill, meet the same gap, and stop there.
    ///
    /// **A gap's backfill is applied at once**, not on the next turn as draft
    /// 11.5 step 6 has it. The live fold's page still starts above the store's
    /// watermark on the next turn, so sending it then would meet the same gap,
    /// forever. The backfilled page is the one that closes it.
    pub(crate) async fn deliver(&self, mut source: Source<'_, S>) -> Delivered {
        let Delivery {
            learner,
            counters,
            project,
            session,
            apply_timeout,
            ..
        } = *self;
        if learner.is_stopped(project, session) {
            return Delivered::Nothing;
        }
        let (mut replay, confirmed) = match &mut source {
            Source::Live(live) => {
                let state = live.state();
                if state.learning_page().is_empty() && state.learning_beyond() == 0 {
                    return Delivered::Nothing;
                }
                (None, None)
            }
            Source::Replayed { state, confirmed } => {
                (Some(std::mem::take(&mut **state)), Some(*confirmed))
            }
        };

        // The live page ran dry with entries still owed: refill it from the
        // hint. Unreachable for a replay; see the module doc.
        if let Source::Live(live) = &source
            && live.state().learning_page().is_empty()
        {
            let hint = live.state().learning_hint();
            match self.backfill(hint).await {
                Some(backfilled) => replay = Some(backfilled),
                None => return Delivered::Held(Hold::SourceUnavailable),
            }
            counters.record(project, DeliveryOutcome::Backfill);
        }
        let mut gap_backfilled = false;

        let (watermark, more) = loop {
            let state = match (&replay, &source) {
                (Some(replayed), _) => replayed,
                (None, Source::Live(live)) => live.state(),
                (None, Source::Replayed { .. }) => {
                    unreachable!("a replayed source moved its state into `replay`")
                }
            };
            let entries = state.learning_page();
            if entries.is_empty() {
                match confirmed {
                    // Nothing above the store's watermark: every entry
                    // through it is delivered, and the clear below proves
                    // the mark is too.
                    Some(watermark) => break (watermark, false),
                    None => return Delivered::Nothing,
                }
            }
            let batch = LearningBatch {
                project,
                session,
                entries,
            };
            match tokio::time::timeout(apply_timeout, learner.store.apply(&batch)).await {
                Ok(Ok(applied)) => {
                    // The outage, if there was one, is over: see the read
                    // side's reset in `choose_learned` for why this stays
                    // silent.
                    learner
                        .apply_unreachable_warned
                        .store(false, Ordering::Relaxed);
                    counters.record(
                        project,
                        DeliveryOutcome::Applied {
                            applied: applied.applied as u64,
                            duplicates: entries.len().saturating_sub(applied.applied) as u64,
                        },
                    );
                    break (applied.watermark, state.learning_beyond() > 0);
                }
                Ok(Err(LearnerError::ChainGap { store_watermark })) => {
                    counters.record(project, DeliveryOutcome::Gap);
                    if gap_backfilled {
                        return Delivered::Held(Hold::GapPersisted);
                    }
                    gap_backfilled = true;
                    match self.backfill(store_watermark).await {
                        Some(refilled) => replay = Some(refilled),
                        None => return Delivered::Held(Hold::SourceUnavailable),
                    }
                    counters.record(project, DeliveryOutcome::Backfill);
                }
                Ok(Err(LearnerError::ChainDiverged { store_watermark })) => {
                    // Never a backfill: the store holds an entry this chain
                    // does not, and a backfill from its watermark would send
                    // the same diverged entry again, forever.
                    tracing::error!(
                        %project, %session, store_watermark,
                        "the learner store refused a diverged chain; this session's delivery \
                         stops until the process restarts, and its entries stay pending"
                    );
                    learner.stop(project, session);
                    counters.record(project, DeliveryOutcome::Diverged);
                    return Delivered::Held(Hold::Stopped);
                }
                Ok(Err(
                    error @ (LearnerError::CounterRange { .. } | LearnerError::Malformed { .. }),
                )) => {
                    // Neither goes away on a retry, and a new epoch does not
                    // either: the refused entry stays in this session's page
                    // under its own epoch. See `engine::learning`'s module doc.
                    let epochs = page_epochs(entries);
                    tracing::error!(
                        %project, %session, epochs, %error,
                        "the learner store refused a batch that no retry can fix; this session's \
                         delivery stops until the process restarts, and its entries stay pending"
                    );
                    learner.stop(project, session);
                    counters.record(project, DeliveryOutcome::Stopped);
                    return Delivered::Held(Hold::Stopped);
                }
                Ok(Err(
                    error @ (LearnerError::Unavailable(_) | LearnerError::WrongType { .. }),
                )) => {
                    if !learner
                        .apply_unreachable_warned
                        .swap(true, Ordering::Relaxed)
                    {
                        tracing::warn!(
                            %project, %session, %error,
                            "the learner store did not take this session's entries; they stay pending"
                        );
                    } else {
                        tracing::debug!(
                            %project, %session, %error,
                            "the learner store did not take this session's entries; they stay pending"
                        );
                    }
                    counters.record(project, DeliveryOutcome::Unavailable);
                    return Delivered::Held(match error {
                        LearnerError::WrongType { .. } => Hold::ForeignKey,
                        _ => Hold::LearnerUnavailable,
                    });
                }
                Err(_) => {
                    // The result is unknown: the apply may have landed. The
                    // resend skips what did, by the entry identity rule.
                    counters.record(project, DeliveryOutcome::TimedOut);
                    return Delivered::Held(Hold::LearnerUnavailable);
                }
            }
        };

        // Acknowledge, then clear. The store already holds these entries
        // durably once it has answered `Applied` above; the mark does not
        // guard against losing them, it only tracks whether this session's
        // own log has recorded that delivery. Ordering the append first is a
        // choice, not a fix: keeping the mark set on every failure branch (a
        // failed append, a failed clear, or a crash between the two) costs at
        // most one redundant recovery apply -- the store answers the resent
        // entries as duplicates by watermark -- plus a redundant clear. The
        // other order would instead risk a crash landing between the clear
        // and the append, which drops the mark while the log never records
        // the delivery; that loses no data, but it is a piece of bookkeeping
        // a session that never turns again would carry as drift forever.
        //
        // A replayed source appends nothing: the recovery task holds no lease,
        // and the clear predicate alone proves the delivery to the index.
        if let Source::Live(live) = &mut source
            && let Err(error) = live.record_learning_applied(watermark).await
        {
            self.acknowledgement_failed(watermark, &error.to_string());
            return Delivered::Held(Hold::AcknowledgementFailed);
        }
        match self.sessions.clear_learning_mark(session, watermark).await {
            Ok(cleared) => Delivered::Confirmed {
                watermark,
                cleared,
                more,
            },
            Err(error) => {
                self.acknowledgement_failed(watermark, &error.to_string());
                Delivered::Held(Hold::AcknowledgementFailed)
            }
        }
    }

    fn acknowledgement_failed(&self, watermark: u64, error: &str) {
        tracing::warn!(
            project = %self.project, session = %self.session, watermark, %error,
            "the learner store applied this session's entries, and acknowledging them failed; \
             the source mark stays for the next turn or the recovery task"
        );
        self.counters
            .record(self.project, DeliveryOutcome::AcknowledgementFailed);
    }

    /// A read-only replay whose page holds the entries above `floor`.
    async fn backfill(&self, floor: u64) -> Option<SessionState> {
        match SessionState::project_learning(self.sessions, self.session, floor).await {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::warn!(
                    project = %self.project, session = %self.session, floor, %error,
                    "the learning backfill could not replay the log; the entries stay pending"
                );
                None
            }
        }
    }
}

/// The epochs a refused page's entries were written under, in page order and
/// without repeats, for the stop's log line.
fn page_epochs(entries: &[LearningEntry]) -> String {
    let mut epochs: Vec<EpochId> = Vec::new();
    for epoch in entries
        .iter()
        .filter_map(|entry| entry.deltas.as_ref().map(|deltas| deltas.epoch))
    {
        if !epochs.contains(&epoch) {
            epochs.push(epoch);
        }
    }
    epochs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod page_epochs_tests {
    //! Mutation survivor 8b: `page_epochs` had no test of its own, so the
    //! `if !epochs.contains(&epoch)` de-duplication could be deleted without
    //! turning any suite red. A refused batch's log line would then repeat
    //! the same epoch once per entry instead of naming it once.

    use super::page_epochs;
    use roundhouse_core::routing::learn::{EpochId, LEARNING_CREDIT_REVISION};
    use roundhouse_core::session::{Deltas, LearningEntry};
    use roundhouse_core::validate::REVIEW_RULE_REVISION;

    fn entry(seq: u64, epoch: EpochId) -> LearningEntry {
        LearningEntry {
            seq,
            prev_seq: seq.saturating_sub(1),
            credit_revision: LEARNING_CREDIT_REVISION,
            review_rule_revision: REVIEW_RULE_REVISION,
            deltas: Some(Deltas {
                epoch,
                quality: Vec::new(),
                targets: Vec::new(),
                overhead: Default::default(),
                jev: Vec::new(),
            }),
        }
    }

    #[test]
    fn repeated_epochs_collapse_to_one_entry() {
        let epoch = EpochId::new([0x11; 16]);
        let entries = vec![entry(1, epoch), entry(2, epoch), entry(3, epoch)];
        assert_eq!(page_epochs(&entries), epoch.to_string());
    }

    #[test]
    fn distinct_epochs_are_named_once_each_in_page_order() {
        let first = EpochId::new([0x22; 16]);
        let second = EpochId::new([0x33; 16]);
        let entries = vec![entry(1, first), entry(2, second), entry(3, first)];
        assert_eq!(
            page_epochs(&entries),
            format!("{first},{second}"),
            "a later repeat of the first epoch does not add a second entry"
        );
    }
}
