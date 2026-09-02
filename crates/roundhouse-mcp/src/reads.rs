// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Everything the surface reads about a running deployment.
//!
//! One trait, implemented by `roundhouse-server` — which is where a control
//! plane, a catalog, a spend ledger and a session store are all visible at
//! once, and where this crate must never depend. Every method is a read; there
//! is no write here, because the only writes the surface performs go to
//! [`ControlStore`](crate::store::ControlStore).
//!
//! # Why the admissibility question lives behind the seam
//!
//! [`Self::admissible_targets`] takes a [`TurnPolicy`] and answers with targets
//! rather than handing this crate a catalog to filter itself. The alternative —
//! a `catalog()` read plus a local filter — needs a
//! [`Candidate`](roundhouse_core::routing::Candidate) to call
//! [`TurnPolicy::permits`], and the only Candidate available here would be one
//! fabricated with zero prices and a made-up prefill estimate. That is a second
//! opinion about admissibility built out of numbers nobody measured, and the
//! first time it disagreed with the router the disagreement would show up as a
//! `status` that promised a target the next turn refused.
//!
//! `admits_when_spent` in core sets the precedent and states the rule: when a
//! question cannot be asked honestly from outside, the question moves to where
//! it can be.

use async_trait::async_trait;

use roundhouse_core::control::{Balance, Principal, TurnPolicy};
use roundhouse_core::ids::SessionId;
use roundhouse_core::routing::{DecisionRecord, Target};

use crate::surface::SurfaceError;

/// What a session's log says, projected.
///
/// Both fields are folds of committed events, which is what keeps the tools
/// that render them pure reads: nothing here is derived from a table this crate
/// maintains beside the log.
#[derive(Debug, Clone, Default)]
pub struct SessionFacts {
    /// The most recent correction this deployment put in the conversation, or
    /// `None` for a session it has never interjected on.
    ///
    /// **A fold of the log, which is what M10.0 made possible.** The guidance
    /// used to live in a node-local store, deposited beside the synthetic call
    /// that named it and lost on restart. It is a conversation item now, and the
    /// session's own projection says which item it is — so `fetch_steer` serves
    /// it as a pure read of the log, and a restart costs nothing.
    pub latest_guidance: Option<String>,
    /// The most recent routing decision, or `None` for a session whose first
    /// turn has not been routed yet.
    pub last_decision: Option<DecisionRecord>,
}

/// Steps (2) and (3) of [`ControlReads::resolve_session`]'s order, in the one
/// place that decides them.
///
/// **A function taking lookups rather than a rule each implementor re-types**
/// (M12 review, F4). The order used to live only in the doc comment above,
/// which meant `roundhouse-server`'s `ControlPlaneReads` and this crate's own
/// test double each encoded it separately, with nothing — not the trait, not
/// the surface, not a shared test — holding the two together. An implementor
/// that tried its "most recent session" before the tool-use id compiled,
/// satisfied the trait, ran unmodified through `ControlPlaneSurface`, and
/// answered a subagent's `status` about its parent's conversation: a wrong
/// answer with a 200 on it, which is the failure R-M2 exists to remove.
/// Lookups differ per deployment; the order does not, so the order is what
/// moves here.
///
/// The caller supplies the two reads and this applies R-M2: the id the client
/// attached is exact, so it decides; the principal's most recent conversation
/// is a guess, so it is only ever the fallback; and neither is
/// [`SurfaceError::NoSession`], which is a refusal rather than a default
/// because a node that has served this principal no turn has nothing to
/// answer about.
///
/// Both lookups are `FnOnce` and lazy: `latest` is not consulted at all when
/// the id resolves, which is what keeps a hot path from paying for the answer
/// it did not use.
pub fn session_without_a_name(
    tool_use_id: Option<&str>,
    session_of_call: impl FnOnce(&str) -> Option<SessionId>,
    latest: impl FnOnce() -> Option<SessionId>,
) -> Result<SessionId, SurfaceError> {
    tool_use_id
        .and_then(session_of_call)
        .or_else(latest)
        .ok_or(SurfaceError::NoSession)
}

#[async_trait]
pub trait ControlReads: Send + Sync + 'static {
    /// Which conversation this call concerns.
    ///
    /// **Three answers in a fixed order, and the order is the ruling** (M12,
    /// R-M2):
    ///
    /// 1. `conversation` — a name the model wrote, resolved through the same
    ///    namespacing the Responses surface qualifies a `prompt_cache_key`
    ///    with. First because it is the only one the *agent* chose: a tool call
    ///    that names a conversation is asking about that conversation, and an
    ///    id inferred from the transport must never overrule a name the model
    ///    wrote.
    ///
    ///    **That namespacing is the Responses surface's, and a Messages client
    ///    has no name that lands in it** (M12). A Messages session is keyed
    ///    `anthropic_messages/<id>` (`messages_api::wire::session_key`), so a
    ///    Claude Code model passing its own session id here resolves to
    ///    nothing and is refused as foreign rather than answered. That is why
    ///    (2) exists and why the tools' own `conversation` description no
    ///    longer invites a client to fill this in: on the Messages surface the
    ///    exact answer is the tool-use id, not a name. Left as it is rather
    ///    than made to try both spellings — one qualification per call is what
    ///    keeps a probe for another tenant's id indistinguishable from a
    ///    typo — and recorded here because the next reader will ask.
    /// 2. `tool_use_id` — the id of the `tool_use` block this call is
    ///    answering, which is an id roundhouse emitted into exactly one
    ///    session. Exact where the fallback below is a guess: a parent agent
    ///    and its subagents share a principal and race for the same "most
    ///    recent" slot, and the id is what tells them apart.
    /// 3. Neither — the principal's most recent session.
    ///
    /// Two failures are distinct and both are errors rather than defaults: a
    /// principal with no session at all is [`SurfaceError::NoSession`], and a
    /// named conversation outside the caller's namespace is
    /// [`SurfaceError::ForeignConversation`] — never a silent fall back to the
    /// caller's own most recent one, which would let a probe for someone else's
    /// key read as an ordinary answer.
    ///
    /// A `tool_use_id` that names no conversation *of this caller's* — unknown,
    /// evicted, or another tenant's — falls through to (3) rather than
    /// refusing. Unknown and foreign answer alike on purpose: telling them
    /// apart would make the id an enumeration oracle for ids the caller does
    /// not hold, and the caller has no use for another tenant's session either
    /// way.
    ///
    /// The unnamed half of that order — steps (2) and (3), the two an
    /// implementor answers from its own tables — is
    /// [`session_without_a_name`], and an implementor is expected to call it
    /// rather than re-encode it.
    async fn resolve_session(
        &self,
        principal: &Principal,
        conversation: Option<&str>,
        tool_use_id: Option<&str>,
    ) -> Result<SessionId, SurfaceError>;

    /// The policy this principal's turns are admitted under, before any
    /// overlay: project profile composed with the membership's overrides.
    async fn ceiling_policy(&self, principal: &Principal) -> Result<TurnPolicy, SurfaceError>;

    /// Every target in the deployment's catalog a turn of `principal`'s under
    /// `policy` could actually be routed to.
    ///
    /// **Both gates, because the router applies both.** `policy` says what this
    /// key may reach and the principal's credentials say what it can
    /// authenticate to; a target that fails either is a target the next turn
    /// will not be given. Answering with only the policy half is what makes
    /// `status` promise a hosted model to a member holding no key for it, and
    /// what lets the overlay guard admit a narrowing onto a provider the turn
    /// then refuses — two answers to one question, disagreeing invisibly.
    /// `principal` is a parameter for that reason and not for logging.
    ///
    /// A catalog read and a policy filter, not a quote: this must never reach
    /// the fleet, because `status` is called from a model's context and a tool
    /// that costs a round trip per call is a tool an agent can turn into load.
    async fn admissible_targets(
        &self,
        principal: &Principal,
        policy: &TurnPolicy,
    ) -> Result<Vec<Target>, SurfaceError>;

    /// This membership's position against its project and member ceilings.
    ///
    /// `None` when the membership has no budget configured at all — an open
    /// deployment, or a project whose file names no `"budget"`. That is a
    /// distinct answer from a balance of zero and it has to stay distinct here,
    /// because the engine treats it as one: an admission with no budget never
    /// calls the ledger, so there is no position to report. Rendering it as
    /// "0.00 remaining" would tell an agent to wrap up on a deployment that
    /// meters nothing, and rendering it as an enormous number would put a
    /// figure nobody wrote in a model's context.
    async fn balance(&self, principal: &Principal) -> Result<Option<Balance>, SurfaceError>;

    /// What `session`'s log projects to.
    ///
    /// The expensive read on this trait: on a real deployment it is a replay of
    /// the whole log. [`Self::session_cursor`] is what lets the surface skip it.
    async fn session_facts(&self, session: &SessionId) -> Result<SessionFacts, SurfaceError>;

    /// Where `session`'s log ends, if that can be answered without replaying it.
    ///
    /// A projection of a log that has not advanced cannot have changed, so this
    /// is the whole freshness check behind the surface's memo of
    /// [`Self::session_facts`] — see `ControlPlaneSurface::session_facts` for
    /// why a memo is needed at all (`status` and `explain_last_route` are
    /// called from a model's context, and a model can call a tool in a loop).
    ///
    /// The default answers `None`, meaning "not cheaply" — and a `None` makes
    /// the surface project on every call, which is exactly what every caller
    /// paid before the memo existed. It is a default rather than a required
    /// method because a cheap cursor is a property of the *store* behind an
    /// implementation and not of the seam: an implementation over a durable
    /// session store has one, and a test double may not care to have one.
    ///
    /// `roundhouse-server`'s `ControlPlaneReads` overrides this over the
    /// `SessionStore::last_seq` that `resolve_session` already calls on every
    /// session-scoped tool call, so on the shipped deployment a repeat `status`
    /// or `explain_last_route` between turns is one cursor read rather than a
    /// full log replay. The default here stays `None` for implementations —
    /// test doubles among them — whose store cannot answer the cursor cheaply.
    ///
    /// A cursor must be *monotone per session and moved by every append*. A
    /// value that repeats across two different log states would serve a stale
    /// projection, which is a worse failure than the cost this exists to avoid:
    /// answer `None` rather than something approximate.
    async fn session_cursor(&self, session: &SessionId) -> Result<Option<u64>, SurfaceError> {
        let _ = session;
        Ok(None)
    }

    /// Wall-clock milliseconds, as the deployment tells the time.
    ///
    /// Here rather than behind a third seam of its own. A clock is a fact about
    /// the deployment exactly as a catalog and a ledger are, the two records
    /// this crate stamps with it — a declared intent and a minted binding — are
    /// only ever compared against timestamps the same deployment wrote, and a
    /// separate `Clock` trait would be a second thing every implementor and
    /// every test double has to supply for one integer.
    fn now_ms(&self) -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subagent() -> SessionId {
        SessionId::new("acme/ada/sub")
    }

    fn most_recent() -> SessionId {
        SessionId::new("acme/ada/main")
    }

    /// R-M2's two unnamed steps, asserted where they are decided rather than
    /// through a surface — this is the function F4 moved them into, and the
    /// one an implementor is free to get wrong only by not calling it.
    #[test]
    fn the_tool_use_id_decides_and_the_most_recent_conversation_only_catches() {
        assert_eq!(
            session_without_a_name(
                Some("toolu_sub"),
                |_| Some(subagent()),
                || Some(most_recent())
            )
            .ok(),
            Some(subagent()),
            "an id the node emitted is exact, so it outranks a guess"
        );
        assert_eq!(
            session_without_a_name(Some("toolu_foreign"), |_| None, || Some(most_recent())).ok(),
            Some(most_recent()),
            "an id that names none of this caller's sessions falls through \
             rather than refusing — unknown, evicted, ambiguous and foreign \
             all answer alike"
        );
        assert_eq!(
            session_without_a_name(None, |_| Some(subagent()), || Some(most_recent())).ok(),
            Some(most_recent()),
            "and with no id at all the fallback is the whole answer"
        );
        assert!(
            matches!(
                session_without_a_name(None, |_| Some(subagent()), || None),
                Err(SurfaceError::NoSession)
            ),
            "a node that has served this principal no turn refuses rather \
             than inventing a conversation"
        );
    }
}
