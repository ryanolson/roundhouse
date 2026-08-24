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
//! way.
//!
//! **The steer payload used to be the one real hole here, and M10.0 closed it
//! by moving the payload.** A correction deposited in this store was lost on
//! restart, and `fetch_steer` then refused an id the log still named. The
//! correction is a conversation item now, so it is in the session log with
//! everything else and this store no longer holds it — which is why
//! `deposit_steer` and `steer_for_completion` are gone rather than kept. Nothing
//! left here can produce a turn routed somewhere its key does not allow, which
//! is the only failure that would not be survivable.

use std::sync::Arc;

use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::{PolicyOverrides, TurnPolicy};
use roundhouse_core::ids::SessionId;
use roundhouse_core::routing::Candidate;
use roundhouse_core::session::SessionState;
use roundhouse_core::store::SessionStore;
use roundhouse_core::validate::{Arm, EscalationOverrides, Objective};
use roundhouse_mcp::ControlStore;

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
    ///
    /// **The judge's escalation is the other narrowing, and it is deliberately
    /// not composed here.** Both are [`PolicyOverrides`] and both compose
    /// through [`TurnPolicy::narrow`], so "neither an agent nor a judge can
    /// widen what a key may do" is a property of the operator either way. What
    /// separates them is what happens when a narrowing empties the candidate
    /// set: an overlay is an *ask* — the agent made it, and a refusal is the
    /// answer to a question it asked — while an escalation is an ask nobody
    /// made, so it may not refuse. Deciding that needs the quoted pool, which
    /// exists one layer down, so the escalation is applied by
    /// [`escalate_within_reach`] inside `plan` and this function composes the
    /// overlay alone. See [`Escalated`] for what the split buys.
    ///
    /// [`PolicyOverrides`]: roundhouse_core::control::PolicyOverrides
    /// [`TurnPolicy::narrow`]: roundhouse_core::control::TurnPolicy::narrow
    pub(super) fn narrowed_admission(
        &self,
        session_id: &SessionId,
        admission: &Admission,
    ) -> Admission {
        let Some(overlay) = self
            .control
            .as_ref()
            .and_then(|control| control.consume_overlay(session_id))
        else {
            return admission.clone();
        };
        admission.with_policy(admission.policy.narrow(&overlay))
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
}

/// What the audit trail is told when an escalation asked for more than the pool
/// could reach.
///
/// One string, for the reason [`OVERFLOW_NOTE`] is one: the fact is one fact,
/// and an operator asking "why did my escalation not do what the floor says"
/// should find every instance with a single pattern. No number in it, and
/// nothing about money — the floor is on the [`DecisionRecord`]'s digest and the
/// candidate it selected is in `considered`, so the note says what happened and
/// the record says what it happened to.
///
/// [`OVERFLOW_NOTE`]: roundhouse_core::routing
/// [`DecisionRecord`]: roundhouse_core::routing::DecisionRecord
pub(super) const ESCALATION_CLAMPED_NOTE: &str = "; the validator's quality floor was above every candidate this key admits, so it selected the best of them rather than refusing the turn";

/// This turn's admission with the judge's escalation applied as far as the
/// quoted pool allows.
///
/// **An escalation is best-effort narrowing, and that is the whole type.** It
/// is the one narrowing in the system that nobody asked for: an operator's
/// policy and an agent's overlay are both asks, and an ask that admits nothing
/// is answered honestly with [`RoutingError::PolicyRefused`]. A floor this
/// deployment invented has no such standing — "the checker must never break the
/// checked" is stated three times across the validate tree, and an escalation
/// that empties the candidate set breaks it for `escalation_turns` turns
/// running, on exactly the deployment whose pool is too modest to meet a
/// shipped default. So the floor is clamped to
/// [`TurnPolicy::reachable_quality_ceiling`] and the escalation *selects* the
/// strongest admissible candidate instead of refusing.
///
/// [`RoutingError::PolicyRefused`]: roundhouse_core::routing::RoutingError::PolicyRefused
/// [`TurnPolicy::reachable_quality_ceiling`]: roundhouse_core::control::TurnPolicy::reachable_quality_ceiling
pub(super) struct Escalated {
    /// What routing runs under: the admission, with the composed floor.
    pub(super) admission: Admission,
    /// `true` when the floor served is not the floor the judge asked for.
    ///
    /// Carried out rather than re-derived at the recording site, because the
    /// only other way to spot it is to run the composition a second time — and
    /// two places computing "was this clamped" is how the audit trail starts
    /// disagreeing with the routing it describes.
    pub(super) clamped: bool,
}

/// Compose the escalation onto `admission`, then clamp it into reach.
///
/// A free function with no `&self`, and the reason is its own: what a floor
/// composes to is separable from the engine that quoted the pool, and separating
/// them is what makes the degenerate cases — an empty pool, a floor already met,
/// a floor nothing can meet — checkable without an engine to build. (This
/// sentence used to borrow `steer_for_completion`'s reasoning by reference;
/// M10.0 T4 deleted that function, so the argument is spelled out here rather
/// than pointing at something a reader cannot find.)
///
/// **The order is composition, then reachability, and it cannot be the other
/// way.** `admission.policy` already carries the membership's own floor and any
/// overlay the agent spent this turn, composed by
/// [`TurnPolicy::narrow`](roundhouse_core::control::TurnPolicy::narrow)'s
/// `max`. The clamp is applied last, to the result, and against what *that*
/// policy permits — so an overlay that admits nothing still refuses, which is
/// the agent's own ask answered, while the escalation can only ever move the
/// floor within a set the base admission had already agreed to.
///
/// The `None` ceiling is the case worth naming: a policy that permits none of
/// the quoted candidates has nothing to clamp onto, so the composed floor is
/// left exactly as it was and the refusal below falls out of the membership's
/// policy — where an operator can find it — rather than being papered over
/// here.
pub(super) fn escalate_within_reach(
    admission: &Admission,
    escalation: Option<EscalationOverrides>,
    candidates: &[Candidate],
) -> Escalated {
    let Some(escalation) = escalation else {
        return Escalated {
            admission: admission.clone(),
            clamped: false,
        };
    };
    let asked = admission.policy.narrow(&PolicyOverrides::from(escalation));
    let floor = match admission.policy.reachable_quality_ceiling(candidates) {
        Some(ceiling) => asked.min_quality.min(ceiling),
        None => asked.min_quality,
    };
    Escalated {
        clamped: floor < asked.min_quality,
        admission: admission.with_policy(TurnPolicy {
            min_quality: floor,
            ..asked
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quoted candidate at `quality_prior`, with the axes the clamp does not
    /// read left at zero.
    fn candidate(name: &str, quality_prior: f64) -> Candidate {
        Candidate {
            target: roundhouse_core::routing::Target::Frontier {
                provider: name.into(),
                model: name.into(),
            },
            expected_prefill_tokens: 0.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 0.0,
            expected_cost_usd: 0.0,
            quality_prior,
            load: None,
        }
    }

    fn admission(min_quality: f64) -> Admission {
        Admission::open().with_policy(TurnPolicy {
            min_quality,
            ..TurnPolicy::unrestricted()
        })
    }

    fn escalation(min_quality: f64) -> Option<EscalationOverrides> {
        Some(EscalationOverrides { min_quality })
    }

    #[test]
    fn an_escalation_narrows_as_far_as_the_pool_reaches_and_never_past_it() {
        let modest = candidate("modest", 0.6);
        let flagship = candidate("flagship", 0.95);

        // The probe: a floor above everything quoted. Clamped to the best
        // candidate, so the escalation *selects* rather than empties — the
        // checker must never break the checked.
        let out_of_reach = escalate_within_reach(
            &admission(0.0),
            escalation(0.8),
            std::slice::from_ref(&modest),
        );
        assert_eq!(out_of_reach.admission.policy.min_quality, 0.6);
        assert!(out_of_reach.clamped);
        assert!(
            out_of_reach.admission.policy.permits(&modest),
            "the clamped floor has to leave a candidate standing, or the clamp \
             bought nothing"
        );

        // The control that makes the probe about reachability rather than about
        // escalations being ignored: the identical floor over a pool that can
        // meet it is applied in full, and says nothing about a clamp.
        let in_reach = escalate_within_reach(
            &admission(0.0),
            escalation(0.8),
            &[modest.clone(), flagship.clone()],
        );
        assert_eq!(in_reach.admission.policy.min_quality, 0.8);
        assert!(!in_reach.clamped);
        assert!(!in_reach.admission.policy.permits(&modest));

        // A floor *below* the membership's own is not a narrowing, so `narrow`
        // discards it — and the clamp must not then read the discarded value as
        // something it lowered.
        let widening = escalate_within_reach(
            &admission(0.9),
            escalation(0.5),
            std::slice::from_ref(&flagship),
        );
        assert_eq!(widening.admission.policy.min_quality, 0.9);
        assert!(!widening.clamped);

        // The refusal that stays a refusal: the base policy permits none of the
        // quoted candidates, so there is no ceiling to clamp onto and the
        // composed floor is left exactly where the membership put it. `plan`
        // then reports `PolicyRefused` against the operator's own policy, which
        // is where an operator can act on it.
        let nothing_admitted = escalate_within_reach(&admission(0.99), escalation(0.8), &[modest]);
        assert_eq!(nothing_admitted.admission.policy.min_quality, 0.99);
        assert!(!nothing_admitted.clamped);

        // And a turn with no escalation in force is handed back untouched,
        // which is every turn of every deployment that has not enabled the loop.
        let unescalated = escalate_within_reach(&admission(0.3), None, &[flagship]);
        assert_eq!(unescalated.admission.policy.min_quality, 0.3);
        assert!(!unescalated.clamped);
    }
}
