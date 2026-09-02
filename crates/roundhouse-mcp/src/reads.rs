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

use crate::surface::{Correlators, SurfaceError};

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

/// What each of the caller's correlators resolved to, in this implementor's
/// own tables.
///
/// **Resolved, not raw** — the counterpart to [`Correlators`], which is what the
/// client sent. The lookups are per deployment (one is a namespace
/// qualification, the other a call table) and the *order they are weighed in* is
/// not, so the two live on opposite sides of
/// [`session_this_call_is_about`]: the implementor fills this in, the shared
/// function rules on it.
///
/// Named fields rather than two positional `Option<SessionId>` arguments for
/// the reason [`Correlators`] is a struct: transposing them would invert R-M7
/// with nothing red.
#[derive(Debug, Clone, Default)]
pub struct Correlated {
    /// What `_meta.threadId` named, or `None` when the client sent none **or
    /// when what it sent named no conversation of this caller's**. Unknown and
    /// foreign collapse here, exactly as they do for a tool-use id — see
    /// [`ControlReads::resolve_session`].
    pub thread: Option<SessionId>,
    /// What `_meta["claudecode/toolUseId"]` named, on the same terms.
    pub call: Option<SessionId>,
}

/// [`ControlReads::resolve_session`]'s order, in the one place that decides it.
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
/// The implementor supplies three already-resolved answers and this applies
/// R-M2 as R-M7 extended it:
///
/// 1. `named` — what the model's own `conversation` argument resolved to. It
///    is `None` when the model wrote none; a name that resolved to *nothing*
///    never reaches here, because that is [`SurfaceError::ForeignConversation`]
///    and the implementor raises it before asking this.
/// 2. `correlated.thread` then `correlated.call` — the client's own two
///    correlators, threadId first (R-M7). Ordered rather than refused against
///    each other because they come from one source: a client that spelled both
///    is naming one call in two vocabularies, where an argument and a
///    correlator are the *model* and the *client* answering separately.
/// 3. `latest` — the principal's most recent conversation, a guess and never
///    more.
///
/// **The disagreement arm is the one thing here that is not an order** (R-M7).
/// When the model named a conversation and the client correlated the call to a
/// different one, both are named back in
/// [`SurfaceError::ContradictoryConversation`] and neither is served. Only the
/// *effective* correlator is compared — the one the order above would have used
/// — because that is the single answer the client gave.
///
/// `latest` stays `FnOnce` and lazy: it is not consulted at all when anything
/// above it resolved, which is what keeps a hot path from paying for the answer
/// it did not use. The correlators cannot be lazy any more, and that is R-M7's
/// direct cost: detecting a contradiction means resolving the client's
/// correlator even on a call whose argument would have decided it.
pub fn session_this_call_is_about(
    named: Option<SessionId>,
    correlated: Correlated,
    latest: impl FnOnce() -> Option<SessionId>,
) -> Result<SessionId, SurfaceError> {
    let correlated = correlated.thread.or(correlated.call);
    match (named, correlated) {
        (Some(named), Some(correlated)) if named != correlated => {
            Err(SurfaceError::ContradictoryConversation {
                named: named.to_string(),
                correlated: correlated.to_string(),
            })
        }
        (Some(agreed), _) => Ok(agreed),
        (None, Some(correlated)) => Ok(correlated),
        (None, None) => latest().ok_or(SurfaceError::NoSession),
    }
}

#[async_trait]
pub trait ControlReads: Send + Sync + 'static {
    /// Which conversation this call concerns.
    ///
    /// **Four answers in a fixed order, and the order is the ruling** (M12,
    /// R-M2; M12.1, R-M7):
    ///
    /// 1. `conversation` — a name the model wrote, resolved through the same
    ///    namespacing the Responses surface qualifies a `prompt_cache_key`
    ///    with. First because it is the only one the *agent* chose: a tool call
    ///    that names a conversation is asking about that conversation.
    ///
    ///    First, but no longer *overruling*: since R-M7 an argument that
    ///    disagrees with the client's own correlator refuses instead of
    ///    winning. The order below decides which conversation a call is about
    ///    when the caller gave one answer in several places; it is not a
    ///    licence to pick a side when the caller gave two.
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
    /// 2. `correlators.thread_id` — `_meta.threadId`, **resolved as a name**
    ///    through that same qualification (M12.1, R-M7). Codex stamps it on
    ///    every `tools/call` and captured traffic shows it byte-identical to
    ///    the turn's `prompt_cache_key`, which is exactly the string the
    ///    Responses surface qualifies into a session id — so the client is
    ///    naming a conversation, in the one vocabulary this deployment already
    ///    understands, and it goes through the named path's tenancy check
    ///    rather than beside it.
    /// 3. `correlators.tool_use_id` — the id of the `tool_use` block this call
    ///    is answering, which is an id roundhouse emitted into exactly one
    ///    session. Exact where the fallback below is a guess: a parent agent
    ///    and its subagents share a principal and race for the same "most
    ///    recent" slot, and the id is what tells them apart.
    /// 4. None of them — the principal's most recent session.
    ///
    /// Two failures are distinct and both are errors rather than defaults: a
    /// principal with no session at all is [`SurfaceError::NoSession`], and a
    /// named conversation outside the caller's namespace is
    /// [`SurfaceError::ForeignConversation`] — never a silent fall back to the
    /// caller's own most recent one, which would let a probe for someone else's
    /// key read as an ordinary answer.
    ///
    /// A **correlator** that names no conversation of this caller's — unknown,
    /// evicted, or another tenant's — falls through to the next step rather
    /// than refusing, and that holds for the thread id exactly as it does for
    /// the tool-use id even though the thread id resolves down the named path.
    /// Unknown and foreign answer alike on purpose: telling them apart would
    /// make either key an enumeration oracle for conversations the caller does
    /// not hold, and the caller has no use for another tenant's session either
    /// way. The `conversation` *argument* is the one input that refuses instead,
    /// because a model that wrote a name is asking about that name and nothing
    /// else; a correlator is the client volunteering context it may simply be
    /// wrong about.
    ///
    /// A third failure is R-M7's: when the argument and the effective
    /// correlator resolve to two different conversations, neither is served and
    /// [`SurfaceError::ContradictoryConversation`] names both.
    ///
    /// The order — every step of it, including that refusal — is
    /// [`session_this_call_is_about`], and an implementor is expected to fill
    /// in [`Correlated`] from its own tables and call it rather than re-encode
    /// the ruling.
    async fn resolve_session(
        &self,
        principal: &Principal,
        conversation: Option<&str>,
        correlators: Correlators<'_>,
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

    fn thread() -> SessionId {
        SessionId::new("acme/ada/thread")
    }

    /// Just the call correlator, as R-M2 left it: exact where `latest` is a
    /// guess, and both of them absent is a refusal rather than an invention.
    ///
    /// Asserted where it is decided rather than through a surface — this is the
    /// function F4 moved the ruling into, and the one an implementor is free to
    /// get wrong only by not calling it.
    #[test]
    fn the_tool_use_id_decides_and_the_most_recent_conversation_only_catches() {
        assert_eq!(
            session_this_call_is_about(
                None,
                Correlated {
                    call: Some(subagent()),
                    ..Correlated::default()
                },
                || Some(most_recent())
            )
            .ok(),
            Some(subagent()),
            "an id the node emitted is exact, so it outranks a guess"
        );
        assert_eq!(
            session_this_call_is_about(None, Correlated::default(), || Some(most_recent())).ok(),
            Some(most_recent()),
            "an id that names none of this caller's sessions resolves to \
             nothing and falls through rather than refusing — unknown, \
             evicted, ambiguous and foreign all answer alike, and so does an \
             absent id"
        );
        assert!(
            matches!(
                session_this_call_is_about(None, Correlated::default(), || None),
                Err(SurfaceError::NoSession)
            ),
            "a node that has served this principal no turn refuses rather \
             than inventing a conversation"
        );
    }

    /// R-M7: the thread id is a correlator too, and it is the first one.
    #[test]
    fn the_thread_id_is_weighed_ahead_of_the_tool_use_id_and_both_ahead_of_latest() {
        assert_eq!(
            session_this_call_is_about(
                None,
                Correlated {
                    thread: Some(thread()),
                    call: Some(subagent()),
                },
                || Some(most_recent())
            )
            .ok(),
            Some(thread()),
            "threadId first (R-M7): it is a *name* the client resolved through \
             the caller's own namespace, where the tool-use id is a lookup in \
             a node-local table"
        );
        assert_eq!(
            session_this_call_is_about(
                None,
                Correlated {
                    thread: Some(thread()),
                    call: None,
                },
                || Some(most_recent())
            )
            .ok(),
            Some(thread()),
            "and on its own it still outranks the guess — the control that \
             proves the assertion above is about the order and not about the \
             tool-use id being present"
        );
        assert_eq!(
            session_this_call_is_about(
                None,
                Correlated {
                    thread: None,
                    call: Some(subagent()),
                },
                || Some(most_recent())
            )
            .ok(),
            Some(subagent()),
            "a client sending only the other correlator is unaffected by R-M7"
        );
    }

    /// R-M7's refusal: the model's argument and the client's correlator
    /// disagreeing is not a precedence question.
    #[test]
    fn an_argument_that_contradicts_the_clients_correlator_is_refused_naming_both() {
        let refused = session_this_call_is_about(
            Some(most_recent()),
            Correlated {
                thread: Some(thread()),
                call: None,
            },
            || panic!("`latest` must not be consulted once either input resolved"),
        )
        .expect_err("a caller contradicting itself is refused");
        let message = refused.to_string();
        assert!(
            matches!(refused, SurfaceError::ContradictoryConversation { .. }),
            "and refused as its own variant, not as a tenancy verdict about \
             either conversation: {message}"
        );
        // **Both**, because either one alone leaves the agent guessing which of
        // its own two inputs the deployment disliked — and the argument is the
        // half a model can actually change.
        assert!(
            message.contains(most_recent().as_str()) && message.contains(thread().as_str()),
            "the refusal must name both conversations: {message}"
        );

        // The control, and the ordinary case: an argument that *agrees* with
        // the correlator is served, so the refusal above is about the
        // disagreement and not about sending both at once.
        assert_eq!(
            session_this_call_is_about(
                Some(most_recent()),
                Correlated {
                    thread: Some(most_recent()),
                    call: Some(most_recent()),
                },
                || None
            )
            .ok(),
            Some(most_recent()),
        );

        // And the correlator that is compared is the *effective* one — the one
        // the order would have used — so a tool-use id behind an agreeing
        // thread id does not manufacture a contradiction the client never had.
        assert_eq!(
            session_this_call_is_about(
                Some(most_recent()),
                Correlated {
                    thread: Some(most_recent()),
                    call: Some(subagent()),
                },
                || None
            )
            .ok(),
            Some(most_recent()),
        );

        // A named argument with no correlator at all is the pre-R-M7 path and
        // still answers: the refusal needs two answers to be a contradiction.
        assert_eq!(
            session_this_call_is_about(Some(most_recent()), Correlated::default(), || None).ok(),
            Some(most_recent()),
        );
    }
}
