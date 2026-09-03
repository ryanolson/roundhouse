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

#[async_trait]
pub trait ControlReads: Send + Sync + 'static {
    /// The conversation `named` denotes *for this caller*, or the refusal that
    /// says it denotes none.
    ///
    /// One lookup for both named inputs — the model's `conversation` argument
    /// and the client's `_meta.threadId` — because they are the same kind of
    /// thing: a name in this caller's own namespace. What differs is only what
    /// a failure *means*, and that is [`Self::resolve_session`]'s business
    /// rather than this lookup's.
    ///
    /// [`SurfaceError::ForeignConversation`] is the answer for a name this
    /// caller holds no conversation under — unknown and another tenant's
    /// collapse into it on purpose, because telling them apart would make the
    /// name an enumeration oracle. Every *other* error is a fact about the
    /// deployment and must stay one: rendering a store outage as "not yours"
    /// tells an agent its own conversation belongs to somebody else, which is
    /// both wrong and the least actionable answer available.
    async fn named_session(
        &self,
        principal: &Principal,
        named: &str,
    ) -> Result<SessionId, SurfaceError>;

    /// The conversation `tool_use_id` was emitted into, if this caller holds
    /// it.
    ///
    /// `Ok(None)` covers unknown, evicted, ambiguous and another tenant's
    /// alike: the deployment wrote this binding itself at the moment it
    /// streamed the call, so there is no existence question to fail on, and an
    /// id that is not this caller's is indistinguishable from one nothing ever
    /// emitted.
    ///
    /// **The `Result` is M14.1's, and it is the same asymmetry
    /// [`Self::named_session`] carries** (R-C1). This was an `Option` while the
    /// table was a `HashMap` in the answering process: there was nothing to
    /// fail. The bindings are in a store shared across nodes now, and a store
    /// that cannot be *reached* is not one of the four answers above — it is a
    /// fact about the deployment. Left as an `Option` it would spell "no
    /// conversation of yours", and [`Self::resolve_session`] would hand the
    /// caller its `latest`: a plausible answer about the wrong conversation,
    /// which is the failure this whole ruling removes.
    async fn session_of_call(
        &self,
        principal: &Principal,
        tool_use_id: &str,
    ) -> Result<Option<SessionId>, SurfaceError>;

    /// The principal's most recent conversation, or `None` for a principal
    /// this deployment has served no turn for.
    ///
    /// A guess, and [`Self::resolve_session`] weighs it as one.
    async fn latest_session(&self, principal: &Principal) -> Option<SessionId>;

    /// The conversation this caller's thread `thread_id` is in, if the
    /// deployment served a turn of it.
    ///
    /// **Exact where [`Self::named_session`] is a name lookup** (M12.1 review,
    /// F2, R-M9). A thread id is not a cache key: a codex agent family shares
    /// one `prompt_cache_key` across the root and every subagent, and stamps a
    /// *different* `_meta.threadId` per member — so reading a subagent's
    /// thread id as a name finds nothing at all, and the call falls through to
    /// the parent's conversation. What makes the pairing knowable is that the
    /// same per-thread id rides the turn itself (codex's
    /// `x-codex-turn-metadata` header), so a deployment that ingests turns can
    /// record which session each thread's latest turn went to and answer this
    /// with no guess.
    ///
    /// `Ok(None)` for an unknown, evicted or foreign thread, and the `Result`
    /// for an unreachable store, exactly as [`Self::session_of_call`] carries
    /// both and for the reason spelled out there.
    ///
    /// The default answers `Ok(None)`, meaning "this deployment records no such
    /// thing" — and a `None` costs only a fall through to the R-M7 named path,
    /// which is where every implementation was before this method existed. It
    /// is a default rather than a required method for [`Self::session_cursor`]'s
    /// reason: the table is a property of an implementation that *ingests the
    /// turns*, and a double that only answers questions about sessions has no
    /// ingest to have watched.
    async fn session_of_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, SurfaceError> {
        let _ = (principal, thread_id);
        Ok(None)
    }

    /// Which conversation this call concerns.
    ///
    /// **Provided, and the whole ruling is here** (M12, R-M2; M12.1, R-M7;
    /// M12.1 review, F1, F2). An implementor supplies the table reads above —
    /// a name lookup, a thread-table lookup, a call-table lookup, a
    /// most-recent lookup — and this
    /// weighs them. The lookups differ per deployment; the order and the two
    /// refusals do not, so they are not an implementor's to re-type. They were
    /// once: the order moved into a shared function in M12 but the
    /// *refuse-versus-swallow* asymmetry below stayed at each call site, and
    /// the two shipped test doubles both spelled it `.ok()` — which also eats
    /// a store outage — while the server spelled it correctly. Nothing was red,
    /// because neither double had a store that could fail.
    ///
    /// Five answers in a fixed order:
    ///
    /// 1. `conversation` — a name the model wrote, resolved through
    ///    [`Self::named_session`]. First because it is the only one the *agent*
    ///    chose: a tool call that names a conversation is asking about that
    ///    conversation. First, but no longer *overruling* — see the refusal
    ///    below.
    ///
    ///    **That namespacing is the Responses surface's, and a Messages client
    ///    has no name that lands in it** (M12). A Messages session is keyed
    ///    `anthropic_messages/<id>` (`messages_api::wire::session_key`), so a
    ///    Claude Code model passing its own session id here resolves to
    ///    nothing and is refused as foreign rather than answered. That is why
    ///    (2) and (3) exist and why the tools' own `conversation` description
    ///    no longer invites a client to fill this in: on the Messages surface
    ///    the exact answer is the tool-use id, not a name. Left as it is rather
    ///    than made to try both spellings — one qualification per call is what
    ///    keeps a probe for another tenant's id indistinguishable from a typo.
    /// 2. `correlators.thread_id` — `_meta.threadId`, resolved through
    ///    [`Self::session_of_thread`] first and only then as a name (M12.1,
    ///    R-M7; M12.1 review, F2, R-M9).
    ///
    ///    **The order within this step is the correction F2 forced.** R-M7
    ///    read the thread id purely as a name, on captured traffic where it
    ///    was byte-identical to the turn's `prompt_cache_key`. That identity
    ///    holds for a codex *root* thread and for nothing else: an agent
    ///    family shares one cache key across root and subagents while each
    ///    member stamps its own thread id, so the name lookup misses for
    ///    exactly the callers the step exists to serve, and every subagent
    ///    fell through to its parent's conversation. The thread table is what
    ///    the deployment watched go past on its own ingest, so it answers the
    ///    subagent exactly; the name lookup stays behind it as the path a root
    ///    thread still takes on a deployment (or a node) that recorded no
    ///    binding.
    ///
    ///    Both spellings resolve *in this caller's namespace*: the table is
    ///    partitioned by principal and the name is qualified, so neither can
    ///    reach another tenant's session.
    /// 3. `correlators.cache_key` — codex's
    ///    `_meta["x-codex-turn-metadata"].session_id`, resolved as a **name**
    ///    through [`Self::named_session`] (M14.1, R-C5). It is the client's own
    ///    session id, byte-identical to the `prompt_cache_key` its turns carry,
    ///    and it needs no table at all: a never-forked conversation's session id
    ///    is a pure function of the caller and that string, and the generation
    ///    map answers the forked case from a store every node shares.
    ///
    ///    **What it adds, stated narrowly, because a root thread was already
    ///    served.** For a codex *root* thread this string and `threadId` are
    ///    one value, so (2)'s own name lookup answers it and this arm is the
    ///    same answer by a second route — which is why (3) does not run at all
    ///    when (2) answered. Where it earns its place is the member whose
    ///    thread id is *nobody's* cache key and whose thread binding this
    ///    deployment does not hold: never recorded, or aged past its staleness
    ///    bound. Both halves of (2) then miss, and before this arm the call
    ///    fell straight to `latest` — "whatever this principal did most
    ///    recently", which for an agent family is a coin toss between its
    ///    members. The family's own cache key names the family's conversation,
    ///    which is not the subagent's exactly but is not somebody else's
    ///    either.
    ///
    ///    Behind (2) rather than in front of it, and that ordering is the same
    ///    F2 correction: a whole codex agent family shares this one string
    ///    while each member stamps its own thread id, so reading it first would
    ///    answer *every* subagent about its parent even where the exact
    ///    binding was available. Resolved only when (2) answered nothing — the
    ///    arms are ordered, so resolving a name the order will discard would
    ///    spend a store round trip for nothing, and for a root thread it is the
    ///    *same* lookup twice.
    ///
    ///    `ForeignConversation` is swallowed here exactly as on the thread
    ///    arm's name lookup, and every other error is returned, for the reason
    ///    below.
    /// 4. `correlators.tool_use_id` — the id of the `tool_use` block this call
    ///    is answering, which is an id roundhouse emitted into exactly one
    ///    session. Exact where the fallback below is a guess: a parent agent
    ///    and its subagents share a principal and race for the same "most
    ///    recent" slot, and the id is what tells them apart.
    ///
    ///    Ordered against (2) and (3) rather than refused against them, unlike
    ///    (1) against any of them: three correlators are one *client* naming
    ///    one call in three vocabularies, where an argument and a correlator
    ///    are the *model* and the *client* answering separately.
    /// 5. None of them — [`Self::latest_session`], a guess and never more.
    ///
    /// # The two refusals, and the one swallow between them
    ///
    /// A correlator that names no conversation of this caller's — unknown,
    /// evicted, or another tenant's — falls through to the next step rather
    /// than refusing, and that holds for the thread id exactly as it does for
    /// the tool-use id even though the thread id resolves down the named path.
    /// Unknown and foreign answer alike on purpose: telling them apart would
    /// make either key an enumeration oracle for conversations the caller does
    /// not hold. The `conversation` *argument* is the one input that refuses
    /// instead ([`SurfaceError::ForeignConversation`]), because a model that
    /// wrote a name is asking about that name and nothing else; a correlator is
    /// the client volunteering context it may simply be wrong about.
    ///
    /// **Only `ForeignConversation` is swallowed on the two named arms.** That
    /// oracle argument is about one question — does this conversation exist for
    /// this caller — and it justifies collapsing only the answers to *it*.
    /// Every other error is a fact about the deployment, and answering a store
    /// outage as "unknown correlator" would quietly hand the caller its
    /// `latest`: a plausible answer about the wrong conversation, which is the
    /// failure this whole ruling removes. The same asymmetry is why the two
    /// table lookups return a `Result` since M14.1: the tables are in a store
    /// shared across nodes now, and an unreachable one is not an unknown id.
    ///
    /// A principal with no session at all is [`SurfaceError::NoSession`] —
    /// never a silent empty answer.
    ///
    /// **The disagreement arm is the one step here that is not an order**
    /// (R-M7). When the model named a conversation and the client correlated
    /// the call to a different one, both are named back in
    /// [`SurfaceError::ContradictoryConversation`] and neither is served.
    /// Picking a winner is the tempting alternative and the dangerous one:
    /// whichever side loses, the tool answers about a conversation the caller
    /// did not ask about with a 200 on it — and a rule that let the argument
    /// win would make the argument a way to steer *past* the client's own
    /// correlator, which is precisely the tenancy claim the correlator exists
    /// to make. Only the *effective* correlator is compared — the one the order
    /// above would have used — because that is the single answer the client
    /// gave.
    ///
    /// # What it costs
    ///
    /// `latest` stays lazy: it is not consulted at all when anything above it
    /// resolved. So are the correlators *against each other* — one chain, each
    /// arm resolved only where the one above it answered nothing, which is
    /// what keeps a table that cannot be reached from refusing a call no arm
    /// of it was going to decide (review M14.1, F11).
    ///
    /// What is not lazy is the correlators against the *argument*, and that
    /// asymmetry is R-M7's direct cost:
    /// detecting a contradiction means resolving the client's correlator even
    /// on a call whose argument would have decided it.
    ///
    /// The one case that costs nothing is the ordinary Codex one, where the
    /// argument and the thread id are the *same string* (M12.1 review, F8).
    /// They are compared before either is resolved, so one name is qualified
    /// against one plane snapshot and looked up once — and the contradiction
    /// check below is then trivially satisfied, because a string cannot
    /// disagree with itself.
    async fn resolve_session(
        &self,
        principal: &Principal,
        conversation: Option<&str>,
        correlators: &Correlators,
    ) -> Result<SessionId, SurfaceError> {
        // The argument first, and it refuses where a correlator falls through:
        // there is no correlator worth resolving for a call already refused.
        let named = match conversation {
            Some(name) => Some(self.named_session(principal, name).await?),
            None => None,
        };

        // (2), (3) and (4) as one lazy chain, each arm consulted only where
        // every arm above it answered nothing. Stated once rather than per arm
        // because two shapes drifted apart (review M14.1, F11): the cache-key
        // arm short-circuited on the thread arm's answer and the tool-use-id
        // arm did not, and since this rung the table lookups return a `Result`
        // — so the eagerness that was free while they returned `Option` became
        // a call-table outage refusing a call the thread arm had already
        // served. An arm the order would have discarded cannot refuse.
        let mut correlated = match correlators.thread_id.as_deref() {
            None => None,
            // One string, one answer (F8). The argument and the correlator
            // agreeing is the ordinary Codex case, and re-deriving that one
            // name by a second route could only manufacture a disagreement
            // between two of *this deployment's* tables — never one the
            // caller had, which is all `ContradictoryConversation` is for.
            Some(thread) if Some(thread) == conversation => named.clone(),
            // Exact first, then the name (R-M9). See rule (2) above for why a
            // thread id is not reliably a cache key.
            Some(thread) => match self.session_of_thread(principal, thread).await? {
                Some(session) => Some(session),
                None => named_correlator(self, principal, thread).await?,
            },
        };
        // (3) R-C5. A name the order would discard is a store round trip spent
        // for no reader — and for a codex root thread, where `threadId` and
        // this string are the same value, it is the identical lookup a second
        // time.
        if correlated.is_none()
            && let Some(key) = correlators.cache_key.as_deref()
        {
            correlated = if Some(key) == conversation {
                // One string, one answer (F8), as on the thread arm: an
                // argument and a correlator spelling one name must not
                // manufacture a disagreement between two of this deployment's
                // own reads.
                named.clone()
            } else {
                named_correlator(self, principal, key).await?
            };
        }
        // (4) The id roundhouse itself emitted — exact, and last because a
        // client that stamped a thread or a cache key on the call has already
        // said which conversation it is in.
        if correlated.is_none()
            && let Some(id) = correlators.tool_use_id.as_deref()
        {
            correlated = self.session_of_call(principal, id).await?;
        }

        match (named, correlated) {
            (Some(named), Some(correlated)) if named != correlated => {
                Err(SurfaceError::ContradictoryConversation {
                    named: named.to_string(),
                    correlated: correlated.to_string(),
                })
            }
            (Some(agreed), _) => Ok(agreed),
            (None, Some(correlated)) => Ok(correlated),
            (None, None) => self
                .latest_session(principal)
                .await
                .ok_or(SurfaceError::NoSession),
        }
    }

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

/// `named`, resolved as a conversation of `principal`'s, with the one swallow a
/// *correlator* is allowed and no other.
///
/// **One function because the asymmetry has to have one home** (M12.1 review,
/// F1). Two arms of [`ControlReads::resolve_session`] resolve a client-supplied
/// string as a name — the thread id behind its binding, and codex's cache key —
/// and each must fall through for "no conversation of yours" while returning
/// everything else. F1 is what happened the last time that rule was written
/// twice: two doubles spelled it `.ok()`, which eats a store outage as well,
/// and nothing was red because neither double had a store that could fail.
///
/// A free function rather than a trait method: it is a rule about how the
/// provided resolver reads an implementor's [`ControlReads::named_session`],
/// not a question an implementor answers, and a method would be one more thing
/// a double could override into disagreement.
async fn named_correlator<R: ControlReads + ?Sized>(
    reads: &R,
    principal: &Principal,
    named: &str,
) -> Result<Option<SessionId>, SurfaceError> {
    match reads.named_session(principal, named).await {
        Ok(session) => Ok(Some(session)),
        Err(SurfaceError::ForeignConversation(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests;
