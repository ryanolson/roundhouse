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
//! task (`crate::learner_recovery`, milestone M9) finishes the work. Both run
//! the one delivery in [`delivery`]; the tail alone appends `LearningApplied`.
//!
//! **A learner block that reaches a process with no learner warns once per
//! project.** The composition root attaches a learner only when a project
//! enables one at boot, so a block the admin plane adds afterwards reaches an
//! engine that cannot run it. The turn routes as before, and the project is
//! named once, until a restart composes the learner.
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

pub(crate) mod delivery;

use tokio::time::Instant;

use roundhouse_core::classify::{AvailableClassification, ClassificationWindow};
use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::ProjectId;
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::learn_store::{LearnerStore, ReadRequest};
use roundhouse_core::routing::learn::{
    ActiveMode, Draw, LearnedError, LearnedPolicy, LearnerTerms, LearningTurn, ReadFailure,
    StoreRead,
};
use roundhouse_core::routing::{Decision, RoutingContext, RoutingError};
use roundhouse_core::session::Session;
use roundhouse_core::store::SessionStore;
use roundhouse_core::validate::Arm;

use crate::control_config::Admission;
use crate::engine::{ClientDeclarations, Engine, EngineError};
use crate::learner_recovery::{LearnerRecovery, RecoveryCadence};

/// How long delivery waits for an apply on a session whose project writes no
/// `apply_timeout_ms` any more, in milliseconds.
///
/// Such a session still has entries owed (draft section 23): its project has
/// no learner block, or an `off` block that writes no timeout. The plan's
/// starting value (section 4), named here so it is not a literal in the tail.
/// A block that writes the timeout, in any mode, runs under its own value
/// (`Admission::learner_apply_timeout_ms`).
pub const UNCONFIGURED_APPLY_TIMEOUT_MS: u64 = 250;

/// The learner store and the delivery state one engine keeps, shared with
/// its recovery task.
pub(crate) struct RoutingLearner {
    pub(crate) store: Arc<dyn LearnerStore>,
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
    /// `WrongType`, from the tail or the recovery task alike.
    pub(crate) apply_unreachable_warned: AtomicBool,
}

impl RoutingLearner {
    fn stopped(&self) -> std::sync::MutexGuard<'_, HashMap<ProjectId, HashSet<SessionId>>> {
        self.stopped_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn is_stopped(&self, project: &ProjectId, session: &SessionId) -> bool {
        self.stopped()
            .get(project)
            .is_some_and(|sessions| sessions.contains(session))
    }

    pub(crate) fn stop(&self, project: &ProjectId, session: &SessionId) {
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
            self.warn_unread_learner(admission);
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

    /// Name, once per project, a `shadow` or `live` block this process has no
    /// learner to run: see the module doc. One `Option` check on the ordinary
    /// path; the lock is taken only for a project that enables a learner the
    /// process did not compose.
    fn warn_unread_learner(&self, admission: &Admission) {
        if self.learner.is_some() {
            return;
        }
        let Some(mode) = admission
            .learner
            .as_deref()
            .and_then(|terms| terms.mode.active())
        else {
            return;
        };
        let project = &admission.principal.project;
        let first = self
            .unread_learner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(project.clone());
        if first {
            tracing::warn!(
                %project,
                mode = mode.label(),
                "this project enables the learner, but this process composed none, so the \
                 block is learning nothing and its turns route as before; it was almost \
                 certainly added through the admin plane after boot, and a restart composes \
                 the learner, its store and its recovery task"
            );
        }
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
    /// The delivery itself is [`delivery::Delivery::deliver`], the code the
    /// recovery task runs too; this tail supplies the live fold and the lease
    /// the acknowledgement is appended under.
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
        let apply_timeout = Duration::from_millis(
            admission
                .learner_apply_timeout_ms
                .unwrap_or(UNCONFIGURED_APPLY_TIMEOUT_MS),
        );
        delivery::Delivery {
            learner,
            sessions: self.store.as_ref(),
            counters: self.metrics.learning_delivery(),
            project: &project,
            session: &session_id,
            apply_timeout,
            source_timeout: None,
            held: None,
        }
        .deliver(delivery::Source::Live(session))
        .await;
    }

    /// The recovery task for this engine's learner, or `None` when no learner
    /// is attached (milestone M9).
    ///
    /// **It shares the engine's learner, not a copy of it**: the same store
    /// handle, the same set of stopped sessions, the same once-per-outage
    /// flags, and the same delivery counters. A session the tail stopped stays
    /// stopped for the task, and a session the task delivered counts where a
    /// tail's delivery would.
    pub fn learner_recovery(&self, cadence: RecoveryCadence) -> Option<LearnerRecovery<S>> {
        let learner = self.learner.as_ref()?;
        Some(LearnerRecovery::new(
            Arc::clone(&self.store),
            Arc::clone(learner),
            self.metrics(),
            cadence,
        ))
    }
}
