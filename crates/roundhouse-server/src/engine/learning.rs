// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The online routing learner's two engine seams (draft sections 11.5 and
//! 12.3, milestone M8 of `agent-docs/PLAN-online-routing-learner.md`): the
//! learned choice in `plan`, and delivery in the `run_turn` tail.
//!
//! **An `off` project never reaches the first seam.** [`Engine::learning_for`]
//! branches on [`LearnerMode::active`] before anything reads the store or
//! computes a draw, so a project with no learner block, or one set to `off`,
//! is decided by [`Engine::policy`](super::Engine) exactly as before, with no
//! learner-store call on its path.
//!
//! **Delivery follows the log, not the mode.** A session that has learned
//! history keeps producing entries after its project returns to `off` (draft
//! section 23), so the tail delivers whatever the session's fold holds,
//! whatever the project says now. A session with no learned history holds no
//! entry, and the tail returns before any store call.
//!
//! **The learner store's answer decides the next step, one step per error:**
//!
//! | Answer | Tail |
//! |---|---|
//! | `Applied { watermark }` | append `LearningApplied` through it, then clear the source mark |
//! | `ChainGap { watermark }` | one backfill from it, applied in the same tail |
//! | `ChainDiverged` | stop this session's delivery and report it, never backfill |
//! | `CounterRange`, `Malformed` | stop this session's delivery and report it |
//! | `Unavailable`, `WrongType`, a timeout | leave the entries pending |
//!
//! Pending entries keep their source mark, so the next turn or the recovery
//! task (milestone M9) finishes the work.
//!
//! **Every stop is a session's, never a project's.** The refused entry stays
//! in its session's page under the epoch it was written in, so a new artifact
//! does not take it out of the page: the same page is resent under the new
//! epoch and refused again. A stop keyed by the project and the admission's
//! epoch was therefore met again under every later epoch, and each time it
//! stopped every other session of the project with it. The stops are process
//! memory: a restart tries each stopped session once more, meets the same
//! refusal, and stops it again.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use roundhouse_core::classify::{AvailableClassification, ClassificationWindow};
use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::ProjectId;
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::learn_store::{LearnerError, LearnerStore, LearningBatch, ReadRequest};
use roundhouse_core::metrics::DeliveryOutcome;
use roundhouse_core::routing::learn::{
    ActiveMode, Draw, EpochId, LearnedError, LearnedPolicy, LearnerTerms, LearningTurn,
    ReadFailure, StoreRead,
};
use roundhouse_core::routing::{Decision, RoutingContext, RoutingError};
use roundhouse_core::session::{LearningEntry, Session, SessionState};
use roundhouse_core::store::SessionStore;
use roundhouse_core::validate::Arm;

use crate::control_config::Admission;
use crate::engine::{ClientDeclarations, Engine, EngineError};

/// How long delivery waits for an apply on a session whose project writes no
/// `apply_timeout_ms` any more, in milliseconds.
///
/// Such a session still has entries owed (draft section 23): its project has
/// no learner block, or an `off` block that writes no timeout. The plan's
/// starting value (section 4), named here so it is not a literal in the tail.
/// A block that writes the timeout, in any mode, runs under its own value
/// (`Admission::learner_apply_timeout_ms`).
pub const UNCONFIGURED_APPLY_TIMEOUT_MS: u64 = 250;

/// The learner store and the delivery state one engine keeps.
pub(crate) struct RoutingLearner {
    store: Arc<dyn LearnerStore>,
    /// The deployment's `arm_salt`: the draw's salt, so an exploring turn is
    /// reproducible from the log and the configuration alone.
    salt: String,
    /// Sessions whose delivery stopped on a refusal no retry fixes: a
    /// diverged chain, a counter out of range, or a malformed batch. See the
    /// module doc for why no stop is a project's.
    ///
    /// Keyed by project first so the read every tail makes (`is_stopped`)
    /// looks up by borrowed keys and clones nothing; only `stop`, which runs
    /// once per stopped session rather than once per tail, pays for owned
    /// keys.
    stopped_sessions: Mutex<HashMap<ProjectId, HashSet<SessionId>>>,
    /// Set on the first learner-store read failure since the last success,
    /// cleared on the next one. Same pattern as `Engine`'s
    /// `fair_use_unreachable_warned` field: a `tracing::warn` on every turn of
    /// an outage is the line an operator learns to filter, so only the
    /// transition into the outage warns. Unlike that field, the transition
    /// back out logs no recovery line here -- the reset is silent, and the
    /// next outage simply gets its own warning rather than inheriting this
    /// one's.
    read_unreachable_warned: AtomicBool,
    /// The same pattern for deliveries the store answered `Unavailable` or
    /// `WrongType`.
    apply_unreachable_warned: AtomicBool,
}

impl RoutingLearner {
    fn stopped(&self) -> std::sync::MutexGuard<'_, HashMap<ProjectId, HashSet<SessionId>>> {
        self.stopped_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_stopped(&self, project: &ProjectId, session: &SessionId) -> bool {
        self.stopped()
            .get(project)
            .is_some_and(|sessions| sessions.contains(session))
    }

    fn stop(&self, project: &ProjectId, session: &SessionId) {
        self.stopped()
            .entry(project.clone())
            .or_default()
            .insert(session.clone());
    }
}

/// What the learned choice reads beyond the routing context: the same window,
/// accepted classifications and tool flag the policy encodes its input from,
/// so the store read and the decision name one key.
struct LearnedTurnInputs<'a> {
    pub terms: &'a LearnerTerms,
    pub mode: ActiveMode,
    pub project: &'a ProjectId,
    pub response_id: &'a ResponseId,
    pub arm: Option<Arm>,
    pub tool_turn: bool,
    pub window: Option<&'a ClassificationWindow>,
    pub available: &'a [AvailableClassification],
}

impl From<LearnedError> for EngineError {
    fn from(error: LearnedError) -> Self {
        match error {
            LearnedError::Routing(error) => EngineError::Routing(error),
            // Unreachable through a loaded configuration, which refuses a
            // learner without a recipe; a routing failure rather than a panic
            // if an admission is ever assembled by hand without one.
            LearnedError::NoRecipe => EngineError::Routing(RoutingError::Policy(anyhow::anyhow!(
                "a learner routes by a tier recipe, and this project has none"
            ))),
            LearnedError::Refused { unmet } => EngineError::LearnerRefused { unmet },
        }
    }
}

impl<S: SessionStore, T: Tokenizer + Clone + 'static> Engine<S, T> {
    /// Attach the learner store, so a project whose learner is `shadow` or
    /// `live` is routed by the learned policy and every session's entries are
    /// delivered.
    ///
    /// A builder for [`Self::with_classifier`]'s reason: the default is a real
    /// absence. The draw's salt is [`EngineConfig::arm_salt`](super::EngineConfig),
    /// the salt the validation arm is drawn under, in its own domain.
    pub fn with_learner(mut self, store: Arc<dyn LearnerStore>) -> Self {
        self.learner = Some(Arc::new(RoutingLearner {
            store,
            salt: self.config.arm_salt.clone(),
            stopped_sessions: Mutex::new(HashMap::new()),
            read_unreachable_warned: AtomicBool::new(false),
            apply_unreachable_warned: AtomicBool::new(false),
        }));
        self
    }

    /// The decision for one turn: the learned policy for a `shadow` or `live`
    /// project on an engine with a learner, today's policy for everything
    /// else, both bounded by the turn deadline.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn choose_route(
        &self,
        ctx: &RoutingContext<'_>,
        admission: &Admission,
        session: &Session<S>,
        response_id: &ResponseId,
        declarations: &ClientDeclarations,
        window: Option<&ClassificationWindow>,
        deadline_at: Instant,
    ) -> Result<Decision, EngineError> {
        let Some((learner, terms, mode)) = self.learning_for(admission) else {
            return self.bounded(deadline_at, self.policy.choose(ctx)).await;
        };
        let inputs = LearnedTurnInputs {
            terms,
            mode,
            project: &admission.principal.project,
            response_id,
            arm: session.state().arm(),
            tool_turn: declarations.declares_tools(),
            window,
            available: session.state().classifications(),
        };
        self.bounded(deadline_at, self.choose_learned(learner, ctx, inputs))
            .await
    }

    /// The learner and mode a turn of this admission runs under, or `None`
    /// for today's path: no learner attached, no learner block, or `off`.
    fn learning_for<'a>(
        &'a self,
        admission: &'a Admission,
    ) -> Option<(&'a RoutingLearner, &'a LearnerTerms, ActiveMode)> {
        let learner = self.learner.as_deref()?;
        let terms = admission.learner.as_deref()?;
        let mode = terms.mode.active()?;
        Some((learner, terms, mode))
    }

    /// The learned decision for one turn: read the store under
    /// `read_timeout_ms`, compute the draw, and call the pure policy.
    ///
    /// **A failed or late read is not a failed turn.** It becomes the view's
    /// `Unavailable` reason, and the policy takes the infeasible path with it:
    /// `serve_rules` serves the `rules` decision, `refuse` fails the turn.
    async fn choose_learned(
        &self,
        learner: &RoutingLearner,
        ctx: &RoutingContext<'_>,
        inputs: LearnedTurnInputs<'_>,
    ) -> Result<Decision, EngineError> {
        let LearnedTurnInputs {
            terms,
            mode,
            project,
            response_id,
            arm,
            tool_turn,
            window,
            available,
        } = inputs;
        // The derivation `choose` plans and records under, so the keys read
        // here are the keys the decision records.
        let (_, input) = LearnedPolicy::turn_input(ctx, tool_turn, window, available)?;
        let request = ReadRequest::new(
            project.clone(),
            terms.epoch,
            &input,
            &terms.strategies,
            ctx.candidates.iter().map(|candidate| &candidate.target),
        );
        let view = match tokio::time::timeout(
            Duration::from_millis(terms.read_timeout_ms),
            learner.store.read(&request),
        )
        .await
        {
            Ok(Ok(view)) => {
                // The outage, if there was one, is over: the next one gets
                // its own warning rather than inheriting this success's
                // silence.
                learner
                    .read_unreachable_warned
                    .store(false, Ordering::Relaxed);
                StoreRead::Read(view)
            }
            Ok(Err(error)) => {
                if !learner
                    .read_unreachable_warned
                    .swap(true, Ordering::Relaxed)
                {
                    tracing::warn!(%project, %error, "the learner store read failed; the turn takes the infeasible path");
                } else {
                    tracing::debug!(%project, %error, "the learner store read failed; the turn takes the infeasible path");
                }
                StoreRead::Unavailable {
                    reason: ReadFailure::StoreUnavailable,
                }
            }
            Err(_) => StoreRead::Unavailable {
                reason: ReadFailure::ReadTimedOut,
            },
        };
        let draw = Draw::for_turn(&learner.salt, ctx.session_id, response_id);
        LearnedPolicy::choose(
            ctx,
            mode,
            &LearningTurn {
                terms,
                view: &view,
                draw,
                arm,
                tool_turn,
                window,
                available,
            },
        )
        .map_err(EngineError::from)
    }

    /// Deliver the session's pending learning entries (draft section 11.5).
    ///
    /// Runs in the `run_turn` tail after the settle and the fair-use draw,
    /// while the lease is still held, for steered, failed and dispatched turns
    /// alike: a review can land on any of them. It cannot fail the turn. Every
    /// failure leaves the entries pending under their source mark.
    ///
    /// **At most one refill and one gap backfill per tail.** Each is a full
    /// read-only replay of the log: the refill when the page ran dry, the gap
    /// backfill when the store reported a gap. They are separate budgets
    /// because a refilled page can meet a gap (the store lost what it
    /// acknowledged), and a refill that spent the gap's backfill would leave
    /// every later tail to refill, meet the same gap, and stop there.
    ///
    /// **A gap's backfill is applied in the same tail**, not on the next turn
    /// as draft 11.5 step 6 has it. The live fold's page still starts above
    /// the store's watermark on the next turn, so sending it then would meet
    /// the same gap, forever. The backfilled page is the one that closes it.
    pub(super) async fn deliver_learning(&self, session: &mut Session<S>, admission: &Admission) {
        let Some(learner) = self.learner.as_deref() else {
            return;
        };
        let state = session.state();
        if state.learning_page().is_empty() && state.learning_beyond() == 0 {
            return;
        }
        let Some(project) = state.principal().map(|principal| principal.project.clone()) else {
            return;
        };
        let session_id = session.session_id().clone();
        if learner.is_stopped(&project, &session_id) {
            return;
        }
        let apply_timeout = Duration::from_millis(
            admission
                .learner_apply_timeout_ms
                .unwrap_or(UNCONFIGURED_APPLY_TIMEOUT_MS),
        );
        let delivery = self.metrics.learning_delivery();

        // The page ran dry with entries still owed: refill it from the hint.
        let mut replay: Option<SessionState> = None;
        if state.learning_page().is_empty() {
            let hint = state.learning_hint();
            match self.backfill(&session_id, hint).await {
                Some(backfilled) => replay = Some(backfilled),
                None => return,
            }
            delivery.record(&project, DeliveryOutcome::Backfill);
        }
        let mut gap_backfilled = false;

        let watermark = loop {
            let entries = match &replay {
                Some(replayed) => replayed.learning_page(),
                None => session.state().learning_page(),
            };
            if entries.is_empty() {
                return;
            }
            let batch = LearningBatch {
                project: &project,
                session: &session_id,
                entries,
            };
            match tokio::time::timeout(apply_timeout, learner.store.apply(&batch)).await {
                Ok(Ok(applied)) => {
                    // The outage, if there was one, is over: see the read
                    // side's reset above for why this stays silent.
                    learner
                        .apply_unreachable_warned
                        .store(false, Ordering::Relaxed);
                    delivery.record(
                        &project,
                        DeliveryOutcome::Applied {
                            applied: applied.applied as u64,
                            duplicates: entries.len().saturating_sub(applied.applied) as u64,
                        },
                    );
                    break applied.watermark;
                }
                Ok(Err(LearnerError::ChainGap { store_watermark })) => {
                    delivery.record(&project, DeliveryOutcome::Gap);
                    if gap_backfilled {
                        return;
                    }
                    gap_backfilled = true;
                    match self.backfill(&session_id, store_watermark).await {
                        Some(refilled) => replay = Some(refilled),
                        None => return,
                    }
                    delivery.record(&project, DeliveryOutcome::Backfill);
                }
                Ok(Err(LearnerError::ChainDiverged { store_watermark })) => {
                    // Never a backfill: the store holds an entry this chain
                    // does not, and a backfill from its watermark would send
                    // the same diverged entry again, forever.
                    tracing::error!(
                        %project, session = %session_id, store_watermark,
                        "the learner store refused a diverged chain; this session's delivery \
                         stops until the process restarts, and its entries stay pending"
                    );
                    learner.stop(&project, &session_id);
                    delivery.record(&project, DeliveryOutcome::Diverged);
                    return;
                }
                Ok(Err(
                    error @ (LearnerError::CounterRange { .. } | LearnerError::Malformed { .. }),
                )) => {
                    // Neither goes away on a retry, and a new epoch does not
                    // either: the refused entry stays in this session's page
                    // under its own epoch. See the module doc.
                    let epochs = page_epochs(entries);
                    tracing::error!(
                        %project, session = %session_id, epochs, %error,
                        "the learner store refused a batch that no retry can fix; this session's \
                         delivery stops until the process restarts, and its entries stay pending"
                    );
                    learner.stop(&project, &session_id);
                    delivery.record(&project, DeliveryOutcome::Stopped);
                    return;
                }
                Ok(Err(
                    error @ (LearnerError::Unavailable(_) | LearnerError::WrongType { .. }),
                )) => {
                    if !learner
                        .apply_unreachable_warned
                        .swap(true, Ordering::Relaxed)
                    {
                        tracing::warn!(
                            %project, session = %session_id, %error,
                            "the learner store did not take this session's entries; they stay pending"
                        );
                    } else {
                        tracing::debug!(
                            %project, session = %session_id, %error,
                            "the learner store did not take this session's entries; they stay pending"
                        );
                    }
                    delivery.record(&project, DeliveryOutcome::Unavailable);
                    return;
                }
                Err(_) => {
                    // The result is unknown: the apply may have landed. The
                    // resend skips what did, by the entry identity rule.
                    delivery.record(&project, DeliveryOutcome::TimedOut);
                    return;
                }
            }
        };

        // Acknowledge, then clear. The store already holds these entries
        // durably once it has answered `Applied` above; the mark does not
        // guard against losing them, it only tracks whether this session's
        // own log has recorded that delivery -- the fact the recovery task
        // reads to decide whom to revisit (it never appends the log entry
        // itself, only re-applies and clears, see the module doc). Ordering
        // the append first is a choice, not a fix: keeping the mark set on
        // every failure branch (a failed append, a failed clear, or a crash
        // between the two) costs at most one redundant recovery apply -- the
        // store answers the resent entries as duplicates by watermark -- plus
        // a redundant clear. The other order would instead risk a crash
        // landing between the clear and the append, which drops the mark
        // while the log never records the delivery; that loses no data, but
        // it is a piece of bookkeeping a session that never turns again would
        // carry as drift forever.
        let appended = session
            .record_learning_applied(watermark)
            .await
            .map_err(|error| error.to_string());
        // The clear only runs once the append has landed: see the comment
        // above for why an append failure must short-circuit it.
        let outcome = match appended {
            Ok(()) => self
                .store
                .clear_learning_mark(&session_id, watermark)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            tracing::warn!(
                %project, session = %session_id, watermark, %error,
                "the learner store applied this session's entries, and acknowledging them failed; \
                 the source mark stays for the next turn or the recovery task"
            );
            delivery.record(&project, DeliveryOutcome::AcknowledgementFailed);
        }
    }

    /// A read-only replay whose page holds the entries above `floor`.
    async fn backfill(&self, session_id: &SessionId, floor: u64) -> Option<SessionState> {
        match SessionState::project_learning(self.store.as_ref(), session_id, floor).await {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::warn!(
                    session = %session_id, floor, %error,
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
