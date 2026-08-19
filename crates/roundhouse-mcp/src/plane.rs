// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The one [`ControlSurface`] implementation: reads through [`ControlReads`],
//! writes to [`ControlStore`], and nothing else.
//!
//! # The narrowing rule, in one place
//!
//! Both halves of the rule stated in [`crate::overlay`] are enforced by
//! [`ControlPlaneSurface::install`] and by nothing else:
//!
//! - a request that would *widen* is clamped by
//!   [`TurnPolicy::narrow`](roundhouse_core::control::TurnPolicy::narrow) and
//!   reported through [`TurnPolicy::widenings_of`](roundhouse_core::control::TurnPolicy::widenings_of);
//! - a request that would leave the admissible set *empty* is not applied at
//!   all, and the session keeps the overlay it had.
//!
//! Neither is an error. An agent that gets an error for asking has to guess
//! what would have been allowed, and an agent that guesses asks again — which
//! is a retry loop bolted to the one surface that must stay cheap. `narrowed:
//! true` plus the resulting admissible list tells it exactly what it got.

use std::sync::Arc;

use async_trait::async_trait;

use roundhouse_core::control::{Balance, LedgerState, Principal, TurnPolicy};
use roundhouse_core::ids::SessionId;
use roundhouse_core::routing::Target;

use crate::overlay::{ModeNarrowing, OverlayScope, PreferMode, SessionOverlay, TimedOverlay};
use crate::reads::ControlReads;
use crate::store::{ControlStore, IntentRecord};
use crate::surface::*;

/// Why an ask was not honored in full.
///
/// Two sentences and no more: an agent reading them has to be able to decide
/// what to do next, and "your ask was too wide" and "your ask left nothing"
/// lead to different next moves.
const CLAMPED_TO_CEILING: &str =
    "this key's policy is already at least this narrow, so the ask changed nothing";
const WOULD_LEAVE_NOTHING: &str =
    "no model this key may use satisfies that request, so your routing was left as it was";

pub struct ControlPlaneSurface<R: ControlReads> {
    reads: Arc<R>,
    store: Arc<ControlStore>,
}

impl<R: ControlReads> ControlPlaneSurface<R> {
    /// The store is shared rather than owned: the engine deposits steer
    /// payloads into it and consumes overlays out of it, so a surface holding
    /// its own copy would be a second control plane that agreed with the first
    /// only by luck.
    pub fn new(reads: Arc<R>, store: Arc<ControlStore>) -> Self {
        Self { reads, store }
    }

    /// Whether `overlay` leaves this principal anything to be routed to.
    ///
    /// An empty overlay is `true` without asking: what a ceiling alone admits
    /// is checked once at startup — the composition root refuses to serve a key
    /// that can route nowhere — so re-asking here would be a second opinion
    /// about a question already settled, and would fail every MCP call in a
    /// deployment that was already broken in a louder way.
    async fn leaves_a_target(
        &self,
        principal: &Principal,
        ceiling: &TurnPolicy,
        overlay: &SessionOverlay,
    ) -> Result<bool, SurfaceError> {
        if overlay.is_empty() {
            return Ok(true);
        }
        Ok(!self
            .reads
            .admissible_targets(principal, &overlay.apply_to(ceiling))
            .await?
            .is_empty())
    }

    /// Store `proposed` if it leaves something routable, and say what happened.
    ///
    /// The fallback chain is total and self-healing: proposed, else what the
    /// session already had, else nothing at all. The last rung exists because a
    /// catalog is a file an operator edits — an overlay installed against
    /// yesterday's models can be left admitting none of today's, and a session
    /// pinned to an overlay it can no longer satisfy would fail every remaining
    /// turn at a seam the agent cannot reach.
    async fn install(
        &self,
        principal: &Principal,
        session: &SessionId,
        ceiling: &TurnPolicy,
        current: SessionOverlay,
        proposed: SessionOverlay,
    ) -> Result<(SessionOverlay, bool), SurfaceError> {
        let (settled, clamped) = if self.leaves_a_target(principal, ceiling, &proposed).await? {
            (proposed, false)
        } else if self.leaves_a_target(principal, ceiling, &current).await? {
            (current, true)
        } else {
            (SessionOverlay::default(), true)
        };
        self.store.set_overlay(session, settled.clone());
        Ok((settled, clamped))
    }

    /// The answer both overlay writers give, built from the settled overlay.
    async fn overlay_response(
        &self,
        principal: &Principal,
        session: &SessionId,
        ceiling: &TurnPolicy,
        settled: &SessionOverlay,
        narrowed_because: Option<&'static str>,
    ) -> Result<OverlayResponse, SurfaceError> {
        let effective = settled.apply_to(ceiling);
        let targets = self.reads.admissible_targets(principal, &effective).await?;
        Ok(OverlayResponse {
            conversation: session.to_string(),
            narrowed: narrowed_because.is_some(),
            narrowed_because,
            policy_digest: effective.digest(),
            admissible_targets: names(&targets),
            overlay: view(settled),
        })
    }

    /// Resolve the conversation a session-scoped tool concerns.
    async fn session_of(
        &self,
        principal: &Principal,
        conversation: &Conversation,
    ) -> Result<SessionId, SurfaceError> {
        self.reads
            .resolve_session(principal, conversation.as_deref())
            .await
    }
}

/// Targets by the one name a policy knows them by.
fn names(targets: &[Target]) -> Vec<String> {
    targets.iter().map(Target::policy_identity).collect()
}

/// The overlay as an agent reads it back.
fn view(overlay: &SessionOverlay) -> Option<OverlayView> {
    if overlay.is_empty() {
        return None;
    }
    Some(OverlayView {
        mode: overlay.mode.as_ref().map(|axis| axis.ask.mode),
        mode_reason: overlay.mode.as_ref().map(|axis| axis.reason.clone()),
        mode_turns_remaining: overlay.mode.as_ref().and_then(|axis| axis.remaining_turns),
        quality_floor: overlay.floor.as_ref().map(|axis| axis.ask),
        floor_reason: overlay.floor.as_ref().map(|axis| axis.reason.clone()),
        floor_turns_remaining: overlay.floor.as_ref().and_then(|axis| axis.remaining_turns),
    })
}

/// The ledger's position, in the tool's vocabulary.
fn budget_view(balance: &Balance) -> BudgetView {
    BudgetView {
        // The one basis this surface can report. Measured usage arrives with
        // M6's validate loop; until it has a producer there is no field for it.
        basis: "committed",
        project_remaining_usd: balance.project_remaining_usd,
        member_remaining_usd: balance.member_remaining_usd,
        state: match balance.state {
            LedgerState::Unconstrained => "unconstrained",
            LedgerState::Warned => "warned",
            LedgerState::Exhausted => "exhausted",
        },
    }
}

/// Refuse a justification that is not one.
fn require_text(field: &'static str, value: &str) -> Result<String, SurfaceError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(SurfaceError::InvalidField {
            field,
            requirement: "not be empty",
        });
    }
    Ok(trimmed.to_string())
}

/// How many turns an ask lasts, refusing a scope and a count that disagree.
///
/// A contradiction is refused rather than resolved because both resolutions are
/// wrong in the same way: honoring the scope silently drops a number the agent
/// wrote down, and honoring the count silently ignores the word it chose. Either
/// leaves an agent believing a preference it does not have.
fn lifetime(scope: OverlayScope, turns: Option<u32>) -> Result<Option<u32>, SurfaceError> {
    match (scope, turns) {
        (OverlayScope::Turn, None) | (OverlayScope::Turn, Some(1)) => Ok(Some(1)),
        (OverlayScope::Turn, Some(_)) => Err(SurfaceError::InvalidField {
            field: "turns",
            requirement: "be 1 or absent when `scope` is `turn`",
        }),
        (OverlayScope::Session, None) => Ok(None),
        (OverlayScope::Session, Some(0)) => Err(SurfaceError::InvalidField {
            field: "turns",
            requirement: "be at least 1",
        }),
        (OverlayScope::Session, Some(count)) => Ok(Some(count)),
    }
}

#[async_trait]
impl<R: ControlReads> ControlSurface for ControlPlaneSurface<R> {
    async fn status(
        &self,
        principal: &Principal,
        request: StatusRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let session = self.session_of(principal, &request.conversation).await?;
        let ceiling = self.reads.ceiling_policy(principal).await?;
        let overlay = self.store.overlay(&session).unwrap_or_default();
        let effective = overlay.apply_to(&ceiling);
        let targets = self.reads.admissible_targets(principal, &effective).await?;
        let balance = self.reads.balance(principal).await?;
        let facts = self.reads.session_facts(&session).await?;

        ToolOutcome::ok(&StatusResponse {
            conversation: session.to_string(),
            policy_digest: effective.digest(),
            admissible_targets: names(&targets),
            budget: balance.as_ref().map(budget_view),
            open_steers: facts.open_steers,
            overlay: view(&overlay),
        })
    }

    async fn init_session(
        &self,
        principal: &Principal,
        request: InitSessionRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let session = self.session_of(principal, &request.conversation).await?;
        let id = self
            .store
            .bind_session(principal, &session, self.reads.now_ms());
        ToolOutcome::ok(&InitSessionResponse {
            session_binding_id: id.to_string(),
            conversation: session.to_string(),
            // The sentence that makes the correlation work. It is addressed to
            // the client, is the reason the id survives into the next turn's
            // resent history, and is deliberately an instruction rather than a
            // description — a client summarizing its own history keeps what it
            // was told to keep.
            note: "This id identifies this conversation to roundhouse. Keep this tool output in the conversation and do not summarize it away; roundhouse recognizes this conversation by seeing the id in the history you resend.",
        })
    }

    async fn declare_intent(
        &self,
        principal: &Principal,
        request: DeclareIntentRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let goal = require_text("goal", &request.goal)?;
        let done_when = require_text("done_when", &request.done_when)?;
        let session = self.session_of(principal, &request.conversation).await?;
        self.store.set_intent(
            &session,
            IntentRecord {
                goal,
                plan_steps: request.plan_steps,
                done_when,
                declared_at_ms: self.reads.now_ms(),
            },
        );
        // Rendered from the store rather than echoed from the request. What an
        // agent is told was recorded has to be what a later reader finds, and
        // the two are the same thing only if one of them is read back — which
        // is exactly the property a durable store swapped in at M8 could break
        // without any other test noticing.
        let stored = self
            .store
            .intent(&session)
            .ok_or_else(|| SurfaceError::Internal("the intent store dropped a write".into()))?;
        ToolOutcome::ok(&IntentResponse {
            conversation: session.to_string(),
            goal: stored.goal,
            plan_steps: stored.plan_steps,
            done_when: stored.done_when,
            routing_effect: "none: an intent is read when your work is reviewed, and never when it is routed",
        })
    }

    async fn prefer(
        &self,
        principal: &Principal,
        request: PreferRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let reason = require_text("reason", &request.reason)?;
        let remaining_turns = lifetime(request.scope, request.turns)?;
        let session = self.session_of(principal, &request.conversation).await?;
        let ceiling = self.reads.ceiling_policy(principal).await?;
        let ceiling_targets = self.reads.admissible_targets(principal, &ceiling).await?;
        let current = self.store.overlay(&session).unwrap_or_default();

        // `auto` is a release, not an ask: it drops the mode axis and can never
        // be narrowed, because there is nothing in it to clamp.
        let mut proposed = current.clone();
        let mut unhonorable = None;
        if request.mode == PreferMode::Auto {
            proposed.mode = None;
        } else {
            match ModeNarrowing::resolve(request.mode, &ceiling_targets) {
                Some(ask) => {
                    proposed.mode = Some(TimedOverlay {
                        ask,
                        remaining_turns,
                        reason,
                    });
                }
                None => unhonorable = Some(WOULD_LEAVE_NOTHING),
            }
        }

        let (settled, clamped) = self
            .install(principal, &session, &ceiling, current, proposed)
            .await?;
        let because = unhonorable.or(if clamped {
            Some(WOULD_LEAVE_NOTHING)
        } else {
            None
        });
        let response = self
            .overlay_response(principal, &session, &ceiling, &settled, because)
            .await?;
        ToolOutcome::ok(&response)
    }

    async fn set_quality_floor(
        &self,
        principal: &Principal,
        request: SetQualityFloorRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        // Refused rather than clamped: a `NaN` floor loses every comparison it
        // is part of, so a floor that silently became 0.0 would read in the
        // audit trail as an agent that asked for nothing.
        if !request.floor.is_finite() || !(0.0..=1.0).contains(&request.floor) {
            return Err(SurfaceError::InvalidField {
                field: "floor",
                requirement: "be a number between 0.0 and 1.0",
            });
        }
        let reason = require_text("reason", &request.reason)?;
        let remaining_turns = lifetime(OverlayScope::Session, Some(request.turns))?;
        let session = self.session_of(principal, &request.conversation).await?;
        let ceiling = self.reads.ceiling_policy(principal).await?;
        let current = self.store.overlay(&session).unwrap_or_default();

        let mut proposed = current.clone();
        proposed.floor = Some(TimedOverlay {
            ask: request.floor,
            remaining_turns,
            reason,
        });
        // Asked for below the ceiling's own floor. `narrow` already clamps it,
        // so nothing unsafe happens either way; the report is what stops the
        // agent believing it moved something.
        let widened = !ceiling.widenings_of(&proposed.overrides()).is_empty();

        let (settled, clamped) = self
            .install(principal, &session, &ceiling, current, proposed)
            .await?;
        let because = if clamped {
            Some(WOULD_LEAVE_NOTHING)
        } else if widened {
            Some(CLAMPED_TO_CEILING)
        } else {
            None
        };
        let response = self
            .overlay_response(principal, &session, &ceiling, &settled, because)
            .await?;
        ToolOutcome::ok(&response)
    }

    async fn fetch_steer(
        &self,
        principal: &Principal,
        request: FetchSteerRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        // The whole handler. No clock, no fleet, no judge — a read of a record
        // committed when the steer was emitted, which is what makes a second
        // call byte-identical and what stops a loop of calls costing anybody
        // anything.
        let record = self.store.steer_for(principal, &request.steer_id)?;
        ToolOutcome::ok(&SteerResponse {
            steer_id: record.steer_id,
            guidance: record.guidance,
            emitted_at_ms: record.emitted_at_ms,
        })
    }

    async fn report_outcome(
        &self,
        principal: &Principal,
        request: ReportOutcomeRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let record = self.store.record_outcome(
            principal,
            &request.steer_id,
            request.outcome,
            request.note,
        )?;
        ToolOutcome::ok(&OutcomeResponse {
            steer_id: record.steer_id,
            outcome: request.outcome,
            recorded: true,
        })
    }

    async fn explain_last_route(
        &self,
        principal: &Principal,
        request: ExplainLastRouteRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let session = self.session_of(principal, &request.conversation).await?;
        let facts = self.reads.session_facts(&session).await?;
        let decision = facts
            .last_decision
            .ok_or_else(|| SurfaceError::NotRoutedYet(session.to_string()))?;
        ToolOutcome::ok(&RouteExplanation {
            conversation: session.to_string(),
            chosen: decision.chosen.policy_identity(),
            rationale: decision.rationale,
            routing_policy: decision.policy,
            budget_state: decision.budget_state,
            turn_policy_digest: decision.turn_policy_digest,
            considered: decision
                .considered
                .iter()
                .map(|candidate| candidate.target.policy_identity())
                .collect(),
        })
    }
}
