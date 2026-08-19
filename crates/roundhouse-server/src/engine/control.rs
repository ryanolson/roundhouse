// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The agent's half of a turn: the overlay it asked for, and the steer it is
//! offered.
//!
//! Split from the turn's execution for the reason [`super::spend`] is, and it is
//! the same reason stated about a different store. Everything in [`super`]
//! answers to the session log — one writer, a lease, a replay that has to be
//! byte-identical. Everything here answers to [`ControlStore`], which is a
//! `HashMap` in this process: node-local, lost on restart, and shared with the
//! MCP surface that mounts beside this engine.
//!
//! **That the store is not durable is what shapes both operations.** An overlay
//! is a *narrowing*, so an overlay lost to a restart widens routing back to the
//! deployment's ceiling and never past it — the failure is a turn that was not
//! as cheap as an agent asked for, which is visible in the audit trail either
//! way. A steer payload lost to a restart is the one real hole, and it is
//! bounded the same way: the log holds the emitted call, so `fetch_steer`
//! refuses rather than inventing, and the turn continues. Neither loss can
//! produce a turn routed somewhere its key does not allow, which is the only
//! failure that would not be survivable.

use std::sync::Arc;

use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::{PolicyOverrides, Principal};
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::{Item, ItemContent};
use roundhouse_core::now_ms;
use roundhouse_core::session::SessionState;
use roundhouse_core::store::SessionStore;
use roundhouse_core::validate::{Arm, Objective};
use roundhouse_mcp::{ControlStore, SteerRecord};

use crate::control_config::Admission;
use crate::engine::Engine;

impl<S: SessionStore, T: Tokenizer + Clone> Engine<S, T> {
    /// Share the control surface's node-local store, so overlays reach this
    /// engine's routing and steer payloads reach that surface's readers.
    ///
    /// A builder for the reason the three above are: it is a deployment's
    /// choice — a deployment that mounts no `/mcp` router has neither half of
    /// the conversation to hold — and the composition root passes the *same*
    /// `Arc` here and to [`mcp_router`](crate::mcp_api::mcp_router).
    pub fn with_control_store(mut self, control: Arc<ControlStore>) -> Self {
        self.control = Some(control);
        self
    }

    /// This turn's admission, narrowed by whatever the agent asked for.
    ///
    /// **The one place an overlay reaches routing**, and it is called from one
    /// place: the `Interjection::Proceed` arm of `run_turn`, after the
    /// interjection seam has answered and before `plan`. Not inside `plan`,
    /// because an overlay is *spent* as well as applied — one turn, one ration —
    /// and consuming it where the policy for the turn is fixed makes "the turn
    /// routed under the overlay" and "the turn that spent it" the same turn by
    /// construction, with the digest on that turn's [`DecisionRecord`] as the
    /// observable an operator checks it against.
    ///
    /// And not *above* the seam, which is where it used to be. A steered turn
    /// is answered by [`Interjection::Complete`] and never reaches `plan`, so it
    /// writes no `Routed` event, no [`DecisionRecord`] and no
    /// `turn_policy_digest`: consuming before the seam charged the agent a
    /// ration for a turn with nothing in the audit trail to check the charge
    /// against, and made `status`'s promise — that the digest it reports is the
    /// string the next `DecisionRecord` will carry — false for exactly those
    /// turns. Moving the call into the arm costs nothing else, because a
    /// narrowing changes only the policy: the principal and the budget the
    /// settle reads are copied through untouched.
    ///
    /// [`Interjection::Complete`]: roundhouse_core::interject::Interjection::Complete
    /// [`DecisionRecord`]: roundhouse_core::routing::DecisionRecord
    ///
    /// Composed the only way an overlay is ever composed:
    /// [`TurnPolicy::narrow`], which is total and can only shrink the admissible
    /// set. There is no path by which an agent's ask widens what its key may do,
    /// and it is this call — not the surface that stored the overlay — that
    /// makes that true, because this is the call the router's answer depends on.
    ///
    /// A turn that consumes nothing gets its admission back unchanged, which is
    /// every turn of a deployment with no control surface and every turn of a
    /// session whose agent has asked for nothing.
    ///
    /// Called after the dedup short-circuit, so a client's retry of an answered
    /// turn replays the log and spends none of the agent's ration — the same
    /// once-per-turn-id rule the interjection seam is placed under. A retry of a
    /// turn that *failed* does spend one, and that is the honest reading rather
    /// than an oversight: nothing was answered, so the turn is re-admitted and
    /// re-decided, and the ration is counted in turns an agent took rather than
    /// in turns that happened to succeed. An overlay is a narrowing, so the cost
    /// of spending one early is that routing widens back to the deployment's
    /// ceiling and never past it.
    /// **Two narrowings, one composition, and they arrive from opposite
    /// directions.** The overlay is what the *agent* asked for and is spent as
    /// it is applied; the escalation is what a *validation* decided and lasts
    /// for the turns it named. Both are [`PolicyOverrides`] and both compose
    /// through [`TurnPolicy::narrow`], which is total and can only shrink — so
    /// "neither an agent nor a judge can widen what a key may do" is a property
    /// of the operator rather than a rule two callers are trusted to remember,
    /// and the order they compose in cannot change the result.
    ///
    /// The escalation is read from [`SessionState::active_escalation`], which
    /// is folded from the `ValidationDecided` in the log. It crosses no side
    /// channel: the turns it applies to read it from the same projection a
    /// replay builds, so a successor picking this session up mid-escalation
    /// narrows exactly as this process would have.
    ///
    /// [`PolicyOverrides`]: roundhouse_core::control::PolicyOverrides
    /// [`TurnPolicy::narrow`]: roundhouse_core::control::TurnPolicy::narrow
    /// [`SessionState::active_escalation`]: roundhouse_core::session::SessionState::active_escalation
    pub(super) fn narrowed_admission(
        &self,
        session_id: &SessionId,
        state: &SessionState,
        admission: &Admission,
    ) -> Admission {
        let overlay = self
            .control
            .as_ref()
            .and_then(|control| control.consume_overlay(session_id));
        let escalation = state.active_escalation().map(PolicyOverrides::from);
        if overlay.is_none() && escalation.is_none() {
            return admission.clone();
        }
        let mut policy = admission.policy.as_ref().clone();
        for overrides in [escalation, overlay].into_iter().flatten() {
            policy = policy.narrow(&overrides);
        }
        Admission {
            principal: admission.principal.clone(),
            policy: Arc::new(policy),
            // Untouched, and deliberately: the two axes an agent may move are
            // the two the overlay carries, and a budget is not one of them.
            // Narrowing a ceiling an admin wrote would be an agent editing its
            // own project's money, and widening it needs no comment. An
            // escalation is the same: it raises a quality floor, which is a
            // routing question, and says nothing about what may be spent.
            budget: admission.budget.clone(),
            // Copied through for the same reason: which experiment a session is
            // in was decided when the session was created, and no per-turn
            // narrowing may move it.
            validation: admission.validation.clone(),
        }
    }

    /// The arm this session is enrolled in, if its membership enrolled it.
    ///
    /// Called once per session, where the log is empty, and its answer is
    /// *stamped* rather than recomputed — see
    /// [`Arm::for_session`](roundhouse_core::validate::Arm::for_session). A
    /// deployment that edits its salt therefore re-buckets the sessions it has
    /// not yet created and none of the ones it has, which is what keeps an arm
    /// comparison from being computed across a boundary nobody recorded.
    ///
    /// `None` for a membership with no `validate` block, and that is the
    /// shipped answer: an unstamped session is not enrolled, the validator
    /// declines to be asked about it, and the turn costs no trigger and no
    /// judge. Guessing an arm here instead would enrol every deployment that
    /// merely upgraded.
    pub(super) fn arm_for(&self, session_id: &SessionId, admission: &Admission) -> Option<Arm> {
        admission
            .validation
            .as_ref()
            .map(|terms| Arm::for_session(session_id, &self.config.arm_salt, terms.shares))
    }

    /// What the agent says it is trying to do, best answer first.
    ///
    /// **The declared goal is the better one and the log cannot hold it.** An
    /// agent states its objective through the MCP surface, which writes into
    /// the node-local control store; a stated goal turns the judge's question
    /// from "infer the goal, then judge drift against your inference" into
    /// "here is the goal, name the divergence", and the difference is the
    /// whole reason `declare_intent` has a write half.
    ///
    /// The fallback is [`Objective::from_items`], which every session has. A
    /// declaration lost to a restart therefore degrades to the last user
    /// message rather than to nothing — the same bounded loss every other read
    /// of this store takes, and for the same reason it is acceptable: what is
    /// lost is precision in a brief, never a routing decision.
    pub(super) fn objective(&self, session_id: &SessionId, state: &SessionState) -> Objective {
        self.control
            .as_ref()
            .and_then(|control| control.intent(session_id))
            .map(|intent| Objective::Declared {
                goal: intent.goal,
                plan_steps: intent.plan_steps,
                done_when: intent.done_when,
            })
            .unwrap_or_else(|| Objective::from_items(&state.items))
    }

    /// Commit the corrective payload behind a steer this turn just emitted.
    ///
    /// **After the log commit, always.** The log is the truth and this store is
    /// a projection of it, so the ordering decides which way a crash between the
    /// two fails: this way leaves a call in the log with no payload here and
    /// `fetch_steer` refuses an id it cannot resolve, which the agent reports and
    /// the turn survives. The other way would leave a payload for a call that
    /// was never emitted — a steer an agent can fetch and answer against a
    /// session that never asked for it.
    ///
    /// What to deposit is [`steer_for_completion`]'s question; this only writes
    /// it, and only where there is a store to write to.
    pub(super) fn deposit_steer(
        &self,
        session_id: &SessionId,
        principal: &Principal,
        item: &Item,
        guidance: String,
    ) {
        let Some(control) = &self.control else { return };
        if let Some(record) = steer_for_completion(session_id, principal, item, guidance, now_ms())
        {
            control.deposit_steer(record);
        }
    }
}

/// The steer record a completing interjection leaves behind, if it left one.
///
/// A free function with no `&self`, for the reason
/// [`settled_cost_usd`](spend::settled_cost_usd) is one: the decision about
/// *what* is deposited is separable from the store it is deposited into, and
/// separating them is what makes the degenerate case checkable without an
/// engine to build.
///
/// **The id comes off the item and from nowhere else.** The item is what the log
/// committed and what the client will resend, so reading the id from it makes
/// "the call an agent fetches by" and "the call its client answers" one string
/// written once — a `steer_id` supplied beside the item would be a second place
/// for them to disagree, and a disagreement there is a steer nobody can fetch.
///
/// `None` for a completion carrying anything but a tool call. That is not an
/// error: such a completion emitted no call, so there is nothing for an agent to
/// fetch and nothing to key a record by. Depositing one anyway would need an
/// invented id, which is a payload an agent could reach only by guessing.
fn steer_for_completion(
    session_id: &SessionId,
    principal: &Principal,
    item: &Item,
    guidance: String,
    now_ms: u64,
) -> Option<SteerRecord> {
    let ItemContent::ToolCall { call_id, .. } = &item.content else {
        return None;
    };
    Some(SteerRecord {
        steer_id: call_id.clone(),
        session: session_id.clone(),
        principal: principal.clone(),
        guidance,
        emitted_at_ms: now_ms,
        outcome: None,
        outcome_note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_steer_is_keyed_by_the_call_the_committed_item_names_and_by_nothing_else() {
        let session = SessionId::new("acme/ada/main");
        let principal = roundhouse_core::control::Principal::new("acme", "ada");

        // The probe: the id comes off the item, so the tool an agent calls and
        // the item its client resends name one string.
        let deposited = steer_for_completion(
            &session,
            &principal,
            &Item::tool_call("rhsteer_resp_1", "fetch_steer", "{}"),
            "go back to the parser".into(),
            7,
        )
        .expect("a completion carrying a call is a steer");
        assert_eq!(deposited.steer_id, "rhsteer_resp_1");
        assert_eq!(deposited.guidance, "go back to the parser");
        assert_eq!(deposited.session, session);
        assert_eq!(deposited.principal, principal);
        assert_eq!(deposited.emitted_at_ms, 7);
        assert!(
            deposited.outcome.is_none() && deposited.outcome_note.is_none(),
            "an outcome is the agent's to report, not the emitter's to assume"
        );

        // The control: a completion that emitted no call has nothing an agent
        // could fetch, so nothing is deposited under an id nobody named.
        assert!(
            steer_for_completion(
                &session,
                &principal,
                &Item::user_text("not a call"),
                "unreachable".into(),
                7,
            )
            .is_none()
        );
    }
}
