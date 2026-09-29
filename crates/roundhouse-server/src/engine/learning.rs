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
//! | `Applied { watermark }` | clear the source mark through it, then append `LearningApplied` |
//! | `ChainGap { watermark }` | one backfill from it, applied in the same tail |
//! | `ChainDiverged` | stop this session's delivery and report it, never backfill |
//! | `CounterRange`, `Malformed` | stop the project epoch's delivery and report it |
//! | `Unavailable`, `WrongType`, a timeout | leave the entries pending |
//!
//! Pending entries keep their source mark, so the next turn or the recovery
//! task (milestone M9) finishes the work. The stops are process memory: a
//! restart tries once more, meets the same refusal, and stops again, and a new
//! epoch (a new artifact) resumes a stopped project.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use roundhouse_core::classify::{AvailableClassification, ClassificationWindow};
use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::ProjectId;
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::learn_store::{LearnerError, LearnerStore, LearningBatch, ReadRequest};
use roundhouse_core::metrics::DeliveryOutcome;
use roundhouse_core::routing::learn::{
    ActiveMode, Draw, EpochId, LearnedError, LearnedInput, LearnedPolicy, LearnerTerms,
    LearningTurn, ReadFailure, StoreRead,
};
use roundhouse_core::routing::stage::pick_tier;
use roundhouse_core::routing::{Decision, RoutingContext, RoutingError, TurnSignals};
use roundhouse_core::session::{Session, SessionState};
use roundhouse_core::store::SessionStore;
use roundhouse_core::validate::Arm;

use crate::control_config::Admission;
use crate::engine::{Engine, EngineError};

/// How long delivery waits for an apply on a session whose project no longer
/// configures a learner, in milliseconds.
///
/// Such a session still has entries owed (draft section 23), and its project
/// has no `apply_timeout_ms` any more. The plan's starting value (section 4),
/// named here so it is not a literal in the tail. A project that configures a
/// learner always runs under its own value.
pub const UNCONFIGURED_APPLY_TIMEOUT_MS: u64 = 250;

/// The learner store and the delivery state one engine keeps.
pub(crate) struct RoutingLearner {
    store: Arc<dyn LearnerStore>,
    /// The deployment's `arm_salt`: the draw's salt, so an exploring turn is
    /// reproducible from the log and the configuration alone.
    salt: String,
    /// Sessions whose delivery stopped on a diverged chain.
    diverged: Mutex<HashSet<(ProjectId, SessionId)>>,
    /// Project epochs whose delivery stopped on a counter out of range or a
    /// malformed batch. `None` is a project that no longer configures a
    /// learner.
    stopped: Mutex<HashSet<(ProjectId, Option<EpochId>)>>,
}

impl RoutingLearner {
    fn is_stopped(&self, project: &ProjectId, session: &SessionId, epoch: Option<EpochId>) -> bool {
        let diverged = self
            .diverged
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&(project.clone(), session.clone()));
        diverged
            || self
                .stopped
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&(project.clone(), epoch))
    }
}

/// What the learned choice reads beyond the routing context: the same window,
/// accepted classifications and tool flag the policy encodes its input from,
/// so the store read and the decision name one key.
pub(super) struct LearnedTurnInputs<'a> {
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
            diverged: Mutex::new(HashSet::new()),
            stopped: Mutex::new(HashSet::new()),
        }));
        self
    }

    /// The learner and mode a turn of this admission runs under, or `None`
    /// for today's path: no learner attached, no learner block, or `off`.
    pub(super) fn learning_for<'a>(
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
    pub(super) async fn choose_learned(
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
        let recipe = ctx.tiers.ok_or(LearnedError::NoRecipe)?;
        // The pick `choose` computes from the same signals, so the keys read
        // here are the keys the decision records.
        let rules_pick = match ctx.signals {
            Some(signals) => pick_tier(signals, recipe.picker(), recipe.confidence_threshold()),
            None => pick_tier(
                &TurnSignals::default(),
                recipe.picker(),
                recipe.confidence_threshold(),
            ),
        };
        let input = LearnedInput::encode(rules_pick.tier, tool_turn, window, available);
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
            Ok(Ok(view)) => StoreRead::Read(view),
            Ok(Err(error)) => {
                tracing::warn!(%project, %error, "the learner store read failed; the turn takes the infeasible path");
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
    /// **At most one backfill per tail.** A backfill is a full read-only
    /// replay of the log, run only when the page ran dry or the store reported
    /// a gap.
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
        let terms = admission.learner.as_deref();
        let epoch = terms.map(|terms| terms.epoch);
        if learner.is_stopped(&project, &session_id, epoch) {
            return;
        }
        let apply_timeout =
            Duration::from_millis(terms.map_or(UNCONFIGURED_APPLY_TIMEOUT_MS, |terms| {
                terms.apply_timeout_ms
            }));
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
        let mut backfilled = replay.is_some();

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
                    delivery.record(
                        &project,
                        DeliveryOutcome::Applied {
                            applied: applied.applied as u64,
                            duplicates: (entries.len() - applied.applied) as u64,
                        },
                    );
                    break applied.watermark;
                }
                Ok(Err(LearnerError::ChainGap { store_watermark })) => {
                    delivery.record(&project, DeliveryOutcome::Gap);
                    if backfilled {
                        return;
                    }
                    backfilled = true;
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
                    learner
                        .diverged
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert((project.clone(), session_id.clone()));
                    delivery.record(&project, DeliveryOutcome::Diverged);
                    return;
                }
                Ok(Err(
                    error @ (LearnerError::CounterRange { .. } | LearnerError::Malformed { .. }),
                )) => {
                    // Neither goes away on a retry; a new epoch is the recovery
                    // (draft section 11.3).
                    tracing::error!(
                        %project, session = %session_id, %error,
                        "the learner store refused a batch that no retry can fix; delivery for \
                         this project epoch stops, and a new artifact starts a new one"
                    );
                    learner
                        .stopped
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert((project.clone(), epoch));
                    delivery.record(&project, DeliveryOutcome::Stopped);
                    return;
                }
                Ok(Err(
                    error @ (LearnerError::Unavailable(_) | LearnerError::WrongType { .. }),
                )) => {
                    tracing::warn!(
                        %project, session = %session_id, %error,
                        "the learner store did not take this session's entries; they stay pending"
                    );
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

        // Clear, then acknowledge. Either failing leaves the mark, and the
        // next turn or the recovery task confirms it: the store skips every
        // entry it already holds.
        let cleared = self
            .store
            .clear_learning_mark(&session_id, watermark)
            .await
            .map_err(|error| error.to_string());
        let appended = session
            .record_learning_applied(watermark)
            .await
            .map_err(|error| error.to_string());
        if let Err(error) = cleared.and(appended) {
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
