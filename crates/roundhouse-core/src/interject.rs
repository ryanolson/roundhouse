// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The interjection seam: answering an admitted turn instead of running it.
//!
//! One question, asked once per admitted turn: is this turn dispatched as the
//! client asked, or completed here carrying something we produced instead? The
//! second answer is the steered turn — the client gets an assistant message
//! carrying roundhouse's correction and its own request restated, reads it
//! where it reads every other answer, and decides what to do next.
//!
//! **The contract with the engine**, which is what makes the answers to that
//! question comparable across milestones:
//!
//! - Consulted *after* the dedup short-circuit, so an identical retry of a
//!   steered turn replays the log and never reaches here. Whatever the
//!   occupant costs — in M6, a paid call to a judge — it is spent once per
//!   turn id, not once per attempt.
//! - Consulted *before* the turn is planned. A held turn should not spend a
//!   fleet round trip to price options it will not use, no `Routed` is
//!   recorded for it, and no grant is opened. Nothing about a steered turn
//!   reaches the cache ledger, because nothing was dispatched.
//! - Consulted with the session's projection, never with the candidate list.
//!   A decision that could see what the turn *would have cost* is a decision
//!   that can be argued into or out of by price.
//! - The interjector decides; the engine writes. The log has exactly one
//!   writer and it is the session that holds the lease, so an occupant that
//!   needs facts recorded — M6's side-call and verdict events — returns them
//!   rather than committing them.
//!
//! **A steered turn completes.** [`Interjection::Complete`] is not a refusal
//! and there is deliberately no incomplete arm: only a completion registers in
//! the session's completed turns, so an incomplete steered turn would re-enter
//! this seam on every retry and never settle, and `response.incomplete` reads
//! as an error in the client rather than as a call to run.
//!
//! **What M6 replaced:** [`production_default`] — and nothing else here. The
//! trigger, the arms, the judge side-call and the verdict-to-action map landed
//! as [`Validator`](crate::validate::Validator), an occupant of this trait
//! whose `consider` runs the trigger against the same [`InterjectionContext`]
//! and returns [`Interjection`]. The seam's position and its two answers were
//! inherited rather than re-decided; two things grew, both reserved above:
//!
//! - **Both variants now carry a [`ControlRecord`]** — the side-call and
//!   verdict facts the occupant produced. Returned rather than committed,
//!   because the log has one writer and it is the session holding the lease.
//! - **[`InterjectionContext`] gained four fields**, `turn_policy`,
//!   `objective`, `side_call` and `validation`. The first was
//!   reserved here by name; the others arrived with it because they answer the
//!   same shape of question — what may this decision see — and because none is
//!   derivable from the log: a declared objective lives in the control store,
//!   and who pays for a check and what
//!   their project permits of the loop both live in the key that presented
//!   itself. A field is added when an occupant reads it and not before, which
//!   is why they are here now and were not before.
//!
//!   A fifth, `capability`, arrived with them and left in M10.0. It said what
//!   the client's dialect could carry a correction through, which mattered only
//!   while the strongest correction was a synthetic tool call; the text steer
//!   needs nothing of the dialect, and the engine had in any case never supplied
//!   anything but `Absent`. The rule the field was added under is the rule that
//!   removed it.
//!
//! The production default is still the no-op. A deployment opts in by
//! installing the validator; nothing about validation is on by having upgraded.

use std::sync::Arc;

use async_trait::async_trait;

use crate::control::TurnPolicy;
use crate::event::{ControlRecord, Usage};
use crate::ids::ResponseId;
use crate::item::Item;
use crate::session::SessionState;
use crate::validate::{Objective, SideCall, ValidationTerms};

/// What the engine is to do with a turn it has already admitted.
#[derive(Debug, Clone, PartialEq)]
pub enum Interjection {
    /// Run the turn as the client asked.
    ///
    /// `record` is what the decision to *not* interject nonetheless cost and
    /// concluded — a judge that was consulted and said carry on, a judge that
    /// timed out, a Shadow run whose action was discarded. It is committed by
    /// [`Session::record_control`](crate::session::Session::record_control)
    /// and is empty for an occupant that decided nothing, which is what keeps
    /// the unconfigured deployment's turn free of an extra store round trip.
    ///
    /// **An empty record is not the same as no record**, and that is the
    /// property the whole instrumentation rests on: a validator that could not
    /// reach its judge proceeds like this one, and the difference between the
    /// two has to survive into the log or a timed-out validator reads as a free
    /// one.
    Proceed { record: ControlRecord },
    /// Complete the turn carrying `item`, without dispatching it.
    ///
    /// `item` is committed by
    /// [`Session::complete_with_item`](crate::session::Session::complete_with_item),
    /// which **overwrites** the response id with the one it is completing — so
    /// an occupant that named the wrong response, or none, cannot claim one it
    /// was not given. The stamp is applied in exactly one place, which is what
    /// makes "an item bearing a response id is one this deployment wrote" a
    /// property of the commit rather than of every occupant's care.
    ///
    /// `usage` is what the decision itself cost, and it is reported to the
    /// client as this turn's usage. Reporting an empty usage instead would
    /// make the deployment's own dashboard exceed what clients were told they
    /// spent, which is the one direction an accounting error must never run.
    ///
    /// **The item is the whole of what the agent gets.** A `guidance: String`
    /// used to travel beside it — the correction, deposited in the control store
    /// for `fetch_steer` to serve, because the item itself was a synthetic tool
    /// call carrying only an id. Since M10.0 both completions carry assistant
    /// *text* and the correction is in the item, so a second copy in a
    /// node-local store would be a second source of truth for one string, lost
    /// on restart, for a fetch nobody needs to make.
    ///
    /// `record` carries the same facts [`Self::Proceed`]'s does, and is
    /// committed in the *same append batch* as the item and the completion. See
    /// [`Session::complete_with_item`](crate::session::Session::complete_with_item):
    /// a decision and its realization split across two batches leaves a crash
    /// window in which a steered turn exists with nothing in the log saying
    /// why.
    Complete {
        item: Item,
        usage: Usage,
        record: ControlRecord,
    },
}

impl Interjection {
    /// Run the turn, having decided nothing.
    ///
    /// The spelling for an occupant that did not act — including the production
    /// default. A named constructor rather than `Proceed { record:
    /// Default::default() }` at each site, so that "nothing happened" and
    /// "something happened and the turn proceeds anyway" do not look alike in a
    /// diff.
    pub fn proceed() -> Self {
        Interjection::Proceed {
            record: ControlRecord::default(),
        }
    }
}

/// Everything a decision may see.
///
/// A named struct rather than a tuple of arguments, because each field answers
/// a different question and a reader should be able to see which without
/// opening the occupant. The rule the struct is grown under is the one its
/// first version stated: **a field is added when an occupant reads it and not
/// before.** A `turn_policy` supplied before anything consulted it would have
/// been a claim ("a decision to steer is subject to the policy") that nothing
/// enforced.
///
/// What is deliberately *not* here is the candidate list. A decision that could
/// see what the turn would have cost is a decision that can be argued into or
/// out of by price, and the seam sits before `plan` precisely so the question
/// cannot be asked.
pub struct InterjectionContext<'a> {
    /// The conversation and its projections, as the log has them: the items,
    /// the turn index, the frontier history, the steers still outstanding.
    /// Everything a trigger can compute without a model call is in here.
    pub state: &'a SessionState,
    /// The response this turn has already opened.
    ///
    /// Load-bearing rather than incidental: it is the provenance stamp on
    /// whatever this seam completes with, which is what makes an item this
    /// deployment produced distinguishable from one a client sent.
    pub response_id: &'a ResponseId,
    /// What this turn's membership permits.
    ///
    /// The ceiling every narrowing composes through, including the validate
    /// loop's own: an escalation is a [`PolicyOverrides`](crate::control::PolicyOverrides)
    /// applied with [`TurnPolicy::narrow`], which is total and can only shrink,
    /// so "the validator cannot escalate past the ceiling" is a property of the
    /// composition operator rather than a rule an occupant is trusted to
    /// remember. Read here so the *recorded* action is the one this membership
    /// permits — not, and it cannot be, the one this turn's pool can serve:
    /// that needs the candidate list, which is the one thing this context
    /// deliberately withholds, so the engine applies the second clamp where the
    /// quotes are.
    pub turn_policy: &'a TurnPolicy,
    /// What the agent said it is trying to do.
    ///
    /// Supplied rather than derived, because the best answer is not in the log:
    /// an agent declares its goal through the control surface, and that record
    /// lives in a store this crate cannot see. The fallback —
    /// [`Objective::from_items`] — is derivable and is what an occupant gets
    /// when nothing was declared.
    pub objective: Objective,
    /// Who a call this decision makes on its own behalf is billed to, and
    /// under what key.
    ///
    /// The one field here that is not about *judging* the turn but about
    /// *paying* for the judgement, and it is here for the same reason the
    /// three above are: an occupant reads it. A judge is a model call, a model
    /// call costs money, and the money question — whose budget, which cache
    /// key — is a per-turn fact no occupant can derive from the log.
    ///
    /// It carries no prices, which is the line that matters. The candidate
    /// list is still absent, so a decision can be told there is no room for a
    /// check and never what the turn it is checking would have cost.
    pub side_call: SideCall<'a>,
    /// What this membership permits of the validate loop, or `None` for one
    /// that is not enrolled.
    ///
    /// A tenancy fact, resolved from the same key the policy and the budget
    /// are, and the reason the arm stamp is not the whole answer: the stamp
    /// says which arm a session was *created* in, and this says what its
    /// project permits *now*. An operator turning the loop off means the turns
    /// that follow are not validated, whatever their sessions were stamped
    /// with — see [`Validator::consider`](crate::validate::Validator).
    pub validation: Option<&'a ValidationTerms>,
}

/// Consulted once per admitted turn, under the contract in the module docs.
///
/// Asynchronous because the occupant this seam exists for makes a network call
/// — M6's judge — and a seam shaped for the no-op would have to change shape to
/// admit it, which is the one thing a seam is for. The return type has no error
/// arm on purpose: an occupant that cannot reach its judge releases the turn
/// (`Proceed`) rather than failing it, because the checker must never break the
/// checked, and a `Result` here would make failing the turn expressible.
#[async_trait]
pub trait Interjector: Send + Sync + 'static {
    async fn consider(&self, context: &InterjectionContext<'_>) -> Interjection;
}

/// The occupant that ships, and the reason this seam needs no `Option`.
///
/// An `Option<Arc<dyn Interjector>>` would put a `None` check on every turn's
/// path and leave "absent" and "present but silent" as two spellings of the
/// same behavior. A default occupant that decides `Proceed` collapses them:
/// the engine always consults, and until M6 the answer is always the same one.
struct NoInterjection;

#[async_trait]
impl Interjector for NoInterjection {
    async fn consider(&self, _context: &InterjectionContext<'_>) -> Interjection {
        Interjection::proceed()
    }
}

/// The interjector an engine runs with unless a caller replaces it.
///
/// The only constructor: the no-op type itself is private, so "what does an
/// unconfigured deployment interject?" has exactly one answer to read.
pub fn production_default() -> Arc<dyn Interjector> {
    Arc::new(NoInterjection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Principal;
    use crate::ids::SessionId;

    #[tokio::test]
    async fn the_noop_interjector_is_the_production_default_and_decides_nothing() {
        let interjector = production_default();
        let response_id = ResponseId::new("resp_1");
        let policy = TurnPolicy::unrestricted();
        let session_id = SessionId::new("acme/ada/main");
        let principal = Principal::new("acme", "ada");

        let side_call_id = crate::ids::SideCallId::new("sc_1");
        let side_call = SideCall {
            session_id: &session_id,
            id: &side_call_id,
            at_seq: 1,
            principal: &principal,
            budget: None,
        };
        // A named function rather than a closure: the context borrows from its
        // argument, and a closure cannot say so in its return type.
        fn context<'a>(
            state: &'a SessionState,
            response_id: &'a ResponseId,
            policy: &'a TurnPolicy,
            side_call: SideCall<'a>,
        ) -> InterjectionContext<'a> {
            InterjectionContext {
                state,
                response_id,
                turn_policy: policy,
                objective: Objective::Unknown,
                side_call,
                // Enrolled in nothing, which is what the production default's
                // deployment is: the assertions below are that no *state*
                // makes it interject, and enrolment is not state.
                validation: None,
            }
        }

        let mut state = SessionState::default();
        assert_eq!(
            interjector
                .consider(&context(&state, &response_id, &policy, side_call))
                .await,
            Interjection::proceed(),
            "the seam ships installed and empty: a deployment that has not \
             installed the validator interjects on nothing"
        );

        // The two states an occupant would most want to act on: a session with
        // history behind it, and one this deployment steered on the turn before.
        // The production default acts on neither.
        state.items.push(Item::user_text("still going in circles"));
        state.turn_index = 12;
        state.steered_on_turn = Some(11);
        let decided = interjector
            .consider(&context(&state, &response_id, &policy, side_call))
            .await;
        assert_eq!(
            decided,
            Interjection::proceed(),
            "no state makes the default interject, which is what makes it the \
             production occupant rather than a disabled feature"
        );

        // And it produces nothing for the engine to commit. An occupant that
        // decided nothing must cost the turn no store round trip, or every
        // deployment that never enables validation pays for it anyway.
        let Interjection::Proceed { record } = decided else {
            panic!("the default never completes a turn");
        };
        assert!(record.is_empty());
    }
}
