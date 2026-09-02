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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use roundhouse_core::control::{Balance, LedgerState, Principal, TurnPolicy};
use roundhouse_core::ids::SessionId;
use roundhouse_core::routing::Target;

use crate::overlay::{ModeNarrowing, OverlayScope, PreferMode, SessionOverlay, TimedOverlay};
use crate::reads::{ControlReads, SessionFacts};
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

/// How many sessions' projections the surface remembers at once.
///
/// A cap and not a policy: see [`ControlPlaneSurface::session_facts`].
const MEMO_CAPACITY: usize = 256;

/// A projection, and the log cursor it was taken at.
struct MemoisedFacts {
    cursor: u64,
    facts: SessionFacts,
}

pub struct ControlPlaneSurface<R: ControlReads> {
    reads: Arc<R>,
    store: Arc<ControlStore>,
    /// The last projection taken of each session, keyed by the cursor it was
    /// taken at. Node-local and owned outright by this surface, unlike
    /// [`ControlStore`], because it is a cache of a *read* and not a fact
    /// anything else in the deployment is entitled to see.
    memo: Mutex<HashMap<SessionId, MemoisedFacts>>,
}

impl<R: ControlReads> ControlPlaneSurface<R> {
    /// The store is shared rather than owned: the engine deposits steer
    /// payloads into it and consumes overlays out of it, so a surface holding
    /// its own copy would be a second control plane that agreed with the first
    /// only by luck.
    pub fn new(reads: Arc<R>, store: Arc<ControlStore>) -> Self {
        Self {
            reads,
            store,
            memo: Mutex::new(HashMap::new()),
        }
    }

    /// What `session`'s log projects to, re-projecting only when it has moved.
    ///
    /// **`status` and `explain_last_route` are called from a model's context,
    /// and a projection is a replay of the whole log.** On the server that is
    /// a store round trip per batch of events plus a clone of every item and of
    /// every routing decision — the cost the neighbouring seam doc refuses on
    /// principle, and the one `fetch_steer` was specifically hardened against.
    /// Nothing stops an agent calling either tool in a loop, so the cost has to
    /// be bounded here rather than trusted not to be paid.
    ///
    /// The bound is a cursor comparison: a log that has not advanced cannot
    /// have changed what it projects to, so the memo is exact rather than
    /// merely fresh-ish, and a turn landing between two calls invalidates it by
    /// construction. A deployment that cannot answer
    /// [`ControlReads::session_cursor`] cheaply says so and pays the projection
    /// every call, which is what every caller paid before the memo existed.
    ///
    /// Eviction is a wholesale drop rather than a recency order. A memo is a
    /// cache: losing it costs one projection per session and nothing else, so
    /// maintaining an LRU here would be machinery whose failure mode is worse
    /// than the thing it optimizes.
    async fn session_facts(&self, session: &SessionId) -> Result<SessionFacts, SurfaceError> {
        let Some(cursor) = self.reads.session_cursor(session).await? else {
            return self.reads.session_facts(session).await;
        };
        if let Some(remembered) = self.remembered(session, cursor) {
            return Ok(remembered);
        }
        let facts = self.reads.session_facts(session).await?;
        self.remember(session, cursor, &facts);
        Ok(facts)
    }

    /// The projection held for `session` at `cursor`, if it is that one.
    fn remembered(&self, session: &SessionId, cursor: u64) -> Option<SessionFacts> {
        self.lock_memo()
            .get(session)
            .filter(|memo| memo.cursor == cursor)
            .map(|memo| memo.facts.clone())
    }

    fn remember(&self, session: &SessionId, cursor: u64, facts: &SessionFacts) {
        let mut memo = self.lock_memo();
        if memo.len() >= MEMO_CAPACITY && !memo.contains_key(session) {
            memo.clear();
        }
        memo.insert(
            session.clone(),
            MemoisedFacts {
                cursor,
                facts: facts.clone(),
            },
        );
    }

    /// The memo's lock. Poisoned means a handler panicked mid-insert; the
    /// recovered guard is a cache that may be one entry stale, which is the
    /// same thing an eviction is.
    fn lock_memo(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, MemoisedFacts>> {
        self.memo
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    /// Move one axis if the result leaves something routable, and say what
    /// happened.
    ///
    /// Two rungs and no third: the ask, else the session exactly as it is. An
    /// earlier version had a third — reset to the empty overlay — against the
    /// case of a catalog an operator edited out from under a live overlay. That
    /// rung could not be entered. Every overlay in the store was written by this
    /// function against the same catalog, `consume_overlay` only ever drops axes
    /// (which widens), and both the ceiling and the catalog are fixed for the
    /// process's lifetime — so an overlay that was routable when it was written
    /// is still routable now, and the second rung always answers.
    ///
    /// **Stated plainly, then: the "leaves something routable" guarantee is
    /// enforced at write time only, and it holds because the catalog outlives
    /// every overlay written against it.** M8 is where that stops being true —
    /// a durable overlay survives the restart that reloads the catalog — and
    /// the place to re-derive is the seam that can see the empty set, which is
    /// the engine's own admission path, not a rung here that fires only for a
    /// session that happens to call an overlay tool again.
    ///
    /// The admissibility question is asked outside the store's lock and the
    /// write is taken inside it, which is safe in one direction only and that
    /// is the direction it runs: a concurrent turn can only *drop* axes, so an
    /// ask judged routable against a sibling axis that has since expired is
    /// judged against a policy at least as strict as the one that lands. The
    /// converse — refusing a write that would now have been fine — is the error
    /// this trade makes, and it errs toward leaving the agent's routing alone.
    async fn install(
        &self,
        principal: &Principal,
        session: &SessionId,
        ceiling: &TurnPolicy,
        current: &SessionOverlay,
        axis: OverlayAxis,
    ) -> Result<(SessionOverlay, bool), SurfaceError> {
        let proposed = axis.applied_to(current.clone());
        if !self.leaves_a_target(principal, ceiling, &proposed).await? {
            // Nothing is written at all — not even the axis's own value — which
            // is what "the session keeps the overlay it had" has to mean once
            // the write is per axis.
            return Ok((self.store.overlay(session).unwrap_or_default(), true));
        }
        let now_ms = self.reads.now_ms();
        let settled = match axis {
            OverlayAxis::Mode(mode) => self.store.set_mode_axis(session, mode, now_ms),
            OverlayAxis::Floor(floor) => self.store.set_floor_axis(session, floor, now_ms),
        };
        Ok((settled, false))
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
    ///
    /// One function for all eight tools, and the only place any part of the
    /// caller's identity is turned into a session: the argument the model wrote
    /// and both correlators the client attached are weighed by
    /// [`ControlReads::resolve_session`], which states the order. Eight copies
    /// of that decision is how two tools in one turn come to disagree about
    /// which conversation they are in.
    async fn session_of(
        &self,
        caller: &Caller,
        conversation: &Conversation,
    ) -> Result<SessionId, SurfaceError> {
        self.reads
            .resolve_session(
                caller.principal(),
                conversation.as_deref(),
                caller.correlators(),
            )
            .await
    }
}

/// Which single axis of an overlay a write moves.
///
/// The unit of an overlay write is one axis and not one snapshot, because the
/// engine is mutating the same entry from another thread — see
/// [`ControlStore::set_mode_axis`]. Spelling that as a type rather than as a
/// convention means an overlay writer cannot accidentally carry a sibling axis
/// it read a moment ago back into the store.
#[derive(Debug, Clone)]
enum OverlayAxis {
    Mode(Option<TimedOverlay<ModeNarrowing>>),
    Floor(Option<TimedOverlay<f64>>),
}

impl OverlayAxis {
    /// `base` with this axis replaced — the overlay the admissibility question
    /// is asked about, never the one that is written.
    fn applied_to(&self, mut base: SessionOverlay) -> SessionOverlay {
        match self {
            OverlayAxis::Mode(mode) => base.mode = mode.clone(),
            OverlayAxis::Floor(floor) => base.floor = floor.clone(),
        }
        base
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
        caller: &Caller,
        request: StatusRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let principal = caller.principal();
        let session = self.session_of(caller, &request.conversation).await?;
        let ceiling = self.reads.ceiling_policy(principal).await?;
        let overlay = self.store.overlay(&session).unwrap_or_default();
        let effective = overlay.apply_to(&ceiling);
        let targets = self.reads.admissible_targets(principal, &effective).await?;
        let balance = self.reads.balance(principal).await?;

        ToolOutcome::ok(&StatusResponse {
            conversation: session.to_string(),
            policy_digest: effective.digest(),
            admissible_targets: names(&targets),
            budget: balance.as_ref().map(budget_view),
            overlay: view(&overlay),
        })
    }

    async fn init_session(
        &self,
        caller: &Caller,
        request: InitSessionRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let principal = caller.principal();
        let session = self.session_of(caller, &request.conversation).await?;
        let id = self
            .store
            .bind_session(principal, &session, self.reads.now_ms());
        ToolOutcome::ok(&InitSessionResponse {
            session_binding_id: id.to_string(),
            conversation: session.to_string(),
            // The sentence that makes the correlation *possible*. It is
            // addressed to the client, it is the reason the id survives into
            // the next turn's resent history, and it is deliberately an
            // instruction rather than a description — a client summarizing its
            // own history keeps what it was told to keep.
            //
            // What it must not say is that the correlation is happening. It is
            // not: nothing in this deployment resolves a session from a binding
            // yet (see `ControlStore::binding_in_log`, whose read side is M7),
            // and a note promising a mechanism the deployment does not run is
            // the one lie an agent has no way to catch.
            note: "This id identifies this conversation to roundhouse, which has recorded it. Keep this tool output in the conversation and do not summarize it away: the id travelling back in the history you resend is what lets a later turn be matched to this conversation.",
        })
    }

    async fn declare_intent(
        &self,
        caller: &Caller,
        request: DeclareIntentRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let goal = require_text("goal", &request.goal)?;
        let done_when = require_text("done_when", &request.done_when)?;
        let session = self.session_of(caller, &request.conversation).await?;
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
        caller: &Caller,
        request: PreferRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let principal = caller.principal();
        let reason = require_text("reason", &request.reason)?;
        let remaining_turns = lifetime(request.scope, request.turns)?;
        let session = self.session_of(caller, &request.conversation).await?;
        let ceiling = self.reads.ceiling_policy(principal).await?;
        let ceiling_targets = self.reads.admissible_targets(principal, &ceiling).await?;
        let current = self.store.overlay(&session).unwrap_or_default();

        // `auto` is a release, not an ask: it drops the mode axis and can never
        // be narrowed, because there is nothing in it to clamp.
        let asked = if request.mode == PreferMode::Auto {
            Some(OverlayAxis::Mode(None))
        } else {
            ModeNarrowing::resolve(request.mode, &ceiling_targets).map(|ask| {
                OverlayAxis::Mode(Some(TimedOverlay {
                    ask,
                    remaining_turns,
                    reason,
                }))
            })
        };

        // An unhonorable ask writes nothing at all, which is the same answer
        // `install` gives an ask that would empty the set — and it has to read
        // the store rather than echo `current`, because a turn may have spent
        // an axis since.
        let (settled, because) = match asked {
            None => (
                self.store.overlay(&session).unwrap_or_default(),
                Some(WOULD_LEAVE_NOTHING),
            ),
            Some(axis) => {
                let (settled, clamped) = self
                    .install(principal, &session, &ceiling, &current, axis)
                    .await?;
                (settled, clamped.then_some(WOULD_LEAVE_NOTHING))
            }
        };
        let response = self
            .overlay_response(principal, &session, &ceiling, &settled, because)
            .await?;
        ToolOutcome::ok(&response)
    }

    async fn set_quality_floor(
        &self,
        caller: &Caller,
        request: SetQualityFloorRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let principal = caller.principal();
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
        let session = self.session_of(caller, &request.conversation).await?;
        let ceiling = self.reads.ceiling_policy(principal).await?;
        let current = self.store.overlay(&session).unwrap_or_default();

        let axis = OverlayAxis::Floor(Some(TimedOverlay {
            ask: request.floor,
            remaining_turns,
            reason,
        }));
        // Asked for below the ceiling's own floor. `narrow` already clamps it,
        // so nothing unsafe happens either way; the report is what stops the
        // agent believing it moved something.
        let widened = !ceiling
            .widenings_of(&axis.applied_to(current.clone()).overrides())
            .is_empty();

        let (settled, clamped) = self
            .install(principal, &session, &ceiling, &current, axis)
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
        caller: &Caller,
        request: FetchSteerRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        // The whole handler. No clock, no fleet, no judge — a fold of the
        // conversation's own log, which is what makes a second call
        // byte-identical and what stops a loop of calls costing anybody
        // anything.
        //
        // The tenancy check is `session_of`, the same door every other
        // session-scoped tool goes through: a conversation outside the caller's
        // namespace is `ForeignConversation` and never a silent fall back to the
        // caller's own. That replaces the principal comparison the steer-id
        // version did for itself — one boundary, kept in one place.
        let session = self.session_of(caller, &request.conversation).await?;
        let facts = self.session_facts(&session).await?;
        let guidance = facts
            .latest_guidance
            .ok_or_else(|| SurfaceError::NoGuidanceYet(session.to_string()))?;
        ToolOutcome::ok(&SteerResponse {
            conversation: session.to_string(),
            guidance,
        })
    }

    async fn report_outcome(
        &self,
        caller: &Caller,
        request: ReportOutcomeRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let principal = caller.principal();
        let session = self.session_of(caller, &request.conversation).await?;
        // Filed, never refused: the descriptor promises that not reporting is
        // never an error, and a store that rejected a report against a
        // conversation nobody steered would make the tool's answer depend on a
        // fact the agent cannot see.
        self.store.record_outcome(
            principal,
            &session,
            request.outcome,
            request.note,
            self.reads.now_ms(),
        );
        ToolOutcome::ok(&OutcomeResponse {
            conversation: session.to_string(),
            outcome: request.outcome,
            recorded: true,
        })
    }

    async fn explain_last_route(
        &self,
        caller: &Caller,
        request: ExplainLastRouteRequest,
    ) -> Result<ToolOutcome, SurfaceError> {
        let session = self.session_of(caller, &request.conversation).await?;
        let facts = self.session_facts(&session).await?;
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
