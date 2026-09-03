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
    /// resolved. The correlators cannot be, and that is R-M7's direct cost:
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

        let thread = match correlators.thread_id.as_deref() {
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
        // (3) R-C5. Resolved only where the thread arm answered nothing: the
        // arms are ordered, so a name the order would discard is a store round
        // trip spent for no reader — and for a codex root thread, where
        // `threadId` and this string are the same value, it is the identical
        // lookup a second time.
        let cache_key = match (&thread, correlators.cache_key.as_deref()) {
            (Some(_), _) | (None, None) => None,
            // One string, one answer (F8), as on the thread arm: an argument
            // and a correlator spelling one name must not manufacture a
            // disagreement between two of this deployment's own reads.
            (None, Some(key)) if Some(key) == conversation => named.clone(),
            (None, Some(key)) => named_correlator(self, principal, key).await?,
        };
        let call = match correlators.tool_use_id.as_deref() {
            Some(id) => self.session_of_call(principal, id).await?,
            None => None,
        };

        match (named, thread.or(cache_key).or(call)) {
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
mod tests {
    use std::collections::HashMap;

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

    fn ada() -> Principal {
        Principal::new("acme", "ada")
    }

    /// The three tables an implementor supplies, and nothing else.
    ///
    /// The order, the swallow and both refusals are the *provided*
    /// [`ControlReads::resolve_session`]'s, so a double that fills these in is
    /// exercising the shipped decision rather than a re-typed copy of it —
    /// which is the whole of what M12.1 review F1 moved here.
    #[derive(Default)]
    struct Tables {
        names: HashMap<&'static str, SessionId>,
        /// The ingest's record of which session each thread's latest turn went
        /// to — a fourth table since R-M9, and the only one that can answer a
        /// subagent whose thread id was never anyone's cache key.
        threads: HashMap<&'static str, SessionId>,
        calls: HashMap<&'static str, SessionId>,
        latest: Option<SessionId>,
        /// A store that cannot answer, so a test can drive the one error the
        /// thread arm must *not* swallow.
        outage: bool,
    }

    #[async_trait]
    impl ControlReads for Tables {
        async fn named_session(
            &self,
            _principal: &Principal,
            named: &str,
        ) -> Result<SessionId, SurfaceError> {
            if self.outage {
                return Err(SurfaceError::Internal("redis connection reset".into()));
            }
            self.names
                .get(named)
                .cloned()
                .ok_or_else(|| SurfaceError::ForeignConversation(named.to_string()))
        }

        async fn session_of_call(
            &self,
            _principal: &Principal,
            tool_use_id: &str,
        ) -> Result<Option<SessionId>, SurfaceError> {
            if self.outage {
                return Err(SurfaceError::Internal("redis connection reset".into()));
            }
            Ok(self.calls.get(tool_use_id).cloned())
        }

        async fn session_of_thread(
            &self,
            _principal: &Principal,
            thread_id: &str,
        ) -> Result<Option<SessionId>, SurfaceError> {
            if self.outage {
                return Err(SurfaceError::Internal("redis connection reset".into()));
            }
            Ok(self.threads.get(thread_id).cloned())
        }

        async fn latest_session(&self, _principal: &Principal) -> Option<SessionId> {
            self.latest.clone()
        }

        async fn ceiling_policy(&self, _principal: &Principal) -> Result<TurnPolicy, SurfaceError> {
            unimplemented!("these tests ask this trait exactly one question")
        }

        async fn admissible_targets(
            &self,
            _principal: &Principal,
            _policy: &TurnPolicy,
        ) -> Result<Vec<Target>, SurfaceError> {
            unimplemented!("these tests ask this trait exactly one question")
        }

        async fn balance(&self, _principal: &Principal) -> Result<Option<Balance>, SurfaceError> {
            unimplemented!("these tests ask this trait exactly one question")
        }

        async fn session_facts(&self, _session: &SessionId) -> Result<SessionFacts, SurfaceError> {
            unimplemented!("these tests ask this trait exactly one question")
        }

        fn now_ms(&self) -> u64 {
            0
        }
    }

    fn correlators(thread_id: Option<&str>, tool_use_id: Option<&str>) -> Correlators {
        Correlators {
            thread_id: thread_id.map(str::to_string),
            tool_use_id: tool_use_id.map(str::to_string),
            cache_key: None,
        }
    }

    /// The `_meta` a codex client actually sends: its thread id and, beside it,
    /// the turn metadata's `session_id` — which is that turn's
    /// `prompt_cache_key` (R-C5).
    ///
    /// Both together rather than one helper each, because the ordering between
    /// them is the thing under test and a fixture that could send only one of
    /// them would make the interesting case unspellable.
    fn codex_meta(thread_id: Option<&str>, cache_key: Option<&str>) -> Correlators {
        Correlators {
            thread_id: thread_id.map(str::to_string),
            tool_use_id: None,
            cache_key: cache_key.map(str::to_string),
        }
    }

    /// Just the call correlator, as R-M2 left it: exact where `latest` is a
    /// guess, and both of them absent is a refusal rather than an invention.
    #[tokio::test]
    async fn the_tool_use_id_decides_and_the_most_recent_conversation_only_catches() {
        let tables = Tables {
            calls: HashMap::from([("toolu_sub", subagent())]),
            latest: Some(most_recent()),
            ..Tables::default()
        };
        assert_eq!(
            tables
                .resolve_session(&ada(), None, &correlators(None, Some("toolu_sub")))
                .await
                .ok(),
            Some(subagent()),
            "an id the node emitted is exact, so it outranks a guess"
        );
        assert_eq!(
            tables
                .resolve_session(&ada(), None, &correlators(None, Some("toolu_nobody")))
                .await
                .ok(),
            Some(most_recent()),
            "an id that names none of this caller's sessions resolves to \
             nothing and falls through rather than refusing — unknown, \
             evicted, ambiguous and foreign all answer alike, and so does an \
             absent id"
        );
        assert!(
            matches!(
                Tables::default()
                    .resolve_session(&ada(), None, &Correlators::default())
                    .await,
                Err(SurfaceError::NoSession)
            ),
            "a node that has served this principal no turn refuses rather \
             than inventing a conversation"
        );
    }

    /// R-M7: the thread id is a correlator too, and it is the first one.
    #[tokio::test]
    async fn the_thread_id_is_weighed_ahead_of_the_tool_use_id_and_both_ahead_of_latest() {
        let tables = Tables {
            names: HashMap::from([("thread", thread())]),
            calls: HashMap::from([("toolu_sub", subagent())]),
            latest: Some(most_recent()),
            ..Tables::default()
        };
        assert_eq!(
            tables
                .resolve_session(
                    &ada(),
                    None,
                    &correlators(Some("thread"), Some("toolu_sub"))
                )
                .await
                .ok(),
            Some(thread()),
            "threadId first (R-M7): it is a *name* the client resolved through \
             the caller's own namespace, where the tool-use id is a lookup in \
             a node-local table"
        );
        assert_eq!(
            tables
                .resolve_session(&ada(), None, &correlators(Some("thread"), None))
                .await
                .ok(),
            Some(thread()),
            "and on its own it still outranks the guess — the control that \
             proves the assertion above is about the order and not about the \
             tool-use id being present"
        );
        assert_eq!(
            tables
                .resolve_session(&ada(), None, &correlators(None, Some("toolu_sub")))
                .await
                .ok(),
            Some(subagent()),
            "a client sending only the other correlator is unaffected by R-M7"
        );
    }

    /// M12.1 review, F2 (R-M9): within the thread step, the ingest's own
    /// record outranks reading the thread id as a cache key.
    ///
    /// The two assertions are the two halves of why the order matters. A
    /// subagent's thread id is *nobody's* cache key, so the name lookup can
    /// only miss and the table is the sole thing that can answer it. And when
    /// both could answer, the table is the one that watched this thread's own
    /// turn go past, where the name is the whole agent family's cache key at
    /// whatever generation it has since reached.
    #[tokio::test]
    async fn a_threads_own_binding_outranks_reading_its_id_as_a_cache_key() {
        let tables = Tables {
            names: HashMap::from([("thread", most_recent())]),
            threads: HashMap::from([("thread", thread()), ("subagent-thread", subagent())]),
            latest: Some(most_recent()),
            ..Tables::default()
        };

        assert_eq!(
            tables
                .resolve_session(&ada(), None, &correlators(Some("subagent-thread"), None))
                .await
                .ok(),
            Some(subagent()),
            "a thread id that was never a cache key is answerable only from \
             the ingest's own record; without it the call falls through to \
             the parent's conversation, which is F2"
        );
        assert_eq!(
            tables
                .resolve_session(&ada(), None, &correlators(Some("thread"), None))
                .await
                .ok(),
            Some(thread()),
            "and where both could answer, the binding this deployment wrote \
             for *this thread* outranks the name it shares with its family"
        );

        // The control: strip the table and the same call falls back to the
        // name, which is the path a root thread takes on a deployment that
        // recorded no binding — so the assertions above are about the order
        // and not about the name lookup having stopped working.
        let unrecorded = Tables {
            names: HashMap::from([("thread", most_recent())]),
            latest: None,
            ..Tables::default()
        };
        assert_eq!(
            unrecorded
                .resolve_session(&ada(), None, &correlators(Some("thread"), None))
                .await
                .ok(),
            Some(most_recent()),
        );
    }

    /// M12.1 review, F1: the thread arm swallows `ForeignConversation` and
    /// nothing else, and it does so *here* rather than once per implementor.
    ///
    /// The asymmetry used to be re-typed at every call site: the server spelled
    /// it as a match on the variant, both test doubles spelled it `.ok()`, and
    /// nothing was red because neither double had a store that could fail.
    /// Whatever an implementor's `named_session` reads, only one arm of it is
    /// the caller's business.
    #[tokio::test]
    async fn a_thread_id_swallows_a_foreign_conversation_and_nothing_else() {
        let unknown = Tables {
            latest: Some(most_recent()),
            ..Tables::default()
        };
        assert_eq!(
            unknown
                .resolve_session(&ada(), None, &correlators(Some("nobodys"), None))
                .await
                .ok(),
            Some(most_recent()),
            "a thread id naming no conversation of this caller's falls through \
             as an unknown tool-use id does"
        );

        let outage = Tables {
            latest: Some(most_recent()),
            outage: true,
            ..Tables::default()
        };
        let error = outage
            .resolve_session(&ada(), None, &correlators(Some("main"), None))
            .await
            .expect_err("a store that cannot answer has not answered");
        assert!(
            matches!(error, SurfaceError::Internal(_)),
            "a deployment that cannot answer must say so rather than hand the \
             caller its `latest` — a plausible answer about the wrong \
             conversation is the failure R-M7 exists to remove: {error}"
        );
    }

    /// R-M7's refusal: the model's argument and the client's correlator
    /// disagreeing is not a precedence question.
    #[tokio::test]
    async fn an_argument_that_contradicts_the_clients_correlator_is_refused_naming_both() {
        let tables = Tables {
            names: HashMap::from([("main", most_recent()), ("thread", thread())]),
            calls: HashMap::from([("toolu_sub", subagent())]),
            // Never consulted below: every call names something that resolves.
            latest: None,
            ..Tables::default()
        };

        let refused = tables
            .resolve_session(&ada(), Some("main"), &correlators(Some("thread"), None))
            .await
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
            tables
                .resolve_session(&ada(), Some("main"), &correlators(Some("main"), None))
                .await
                .ok(),
            Some(most_recent()),
        );

        // And the correlator that is compared is the *effective* one — the one
        // the order would have used — so a tool-use id behind an agreeing
        // thread id does not manufacture a contradiction the client never had.
        assert_eq!(
            tables
                .resolve_session(
                    &ada(),
                    Some("main"),
                    &correlators(Some("main"), Some("toolu_sub"))
                )
                .await
                .ok(),
            Some(most_recent()),
        );

        // A named argument with no correlator at all is the pre-R-M7 path and
        // still answers: the refusal needs two answers to be a contradiction.
        assert_eq!(
            tables
                .resolve_session(&ada(), Some("main"), &Correlators::default())
                .await
                .ok(),
            Some(most_recent()),
        );
    }

    /// M14.1, R-C5: a codex root thread resolves from the cache key it was
    /// already sending, on a deployment holding no thread binding at all.
    ///
    /// This is the whole of what the third correlator buys. The thread table is
    /// *empty* here — the state of any node that served none of this
    /// conversation's turns — and before this arm the call fell through to
    /// `latest`, which for a principal running several agents is a coin toss
    /// and for a fresh node is `NoSession`. The name is in `names` because
    /// that is what a durable generation map makes true: at generation zero the
    /// session id is a pure function of the caller and this string.
    #[tokio::test]
    async fn a_codex_root_thread_resolves_from_its_cache_key_with_no_thread_binding() {
        let tables = Tables {
            names: HashMap::from([("cache-key", thread())]),
            latest: Some(most_recent()),
            ..Tables::default()
        };

        assert_eq!(
            tables
                .resolve_session(
                    &ada(),
                    None,
                    &codex_meta(Some("cache-key"), Some("cache-key"))
                )
                .await
                .ok(),
            Some(thread()),
            "a root thread stamps one string as both its thread id and its              session id, and either route reaches the same conversation"
        );

        // The case the arm exists for: a thread id that is *not* the cache key
        // and that nothing has bound. The thread arm misses both ways — no
        // binding, and no conversation under that name — and the cache key is
        // what is left before the guess.
        assert_eq!(
            tables
                .resolve_session(
                    &ada(),
                    None,
                    &codex_meta(Some("unbound-thread"), Some("cache-key"))
                )
                .await
                .ok(),
            Some(thread()),
            "the cache key answers where the thread arm found nothing, rather              than the call falling to `latest`"
        );

        // CONTROL: the arm is a *fallback* and not a promotion. A subagent
        // whose own thread is bound stays in its own conversation, where its
        // family's shared cache key would have answered about the parent —
        // which is F2 exactly.
        let with_binding = Tables {
            names: HashMap::from([("cache-key", most_recent())]),
            threads: HashMap::from([("subagent-thread", subagent())]),
            latest: None,
            ..Tables::default()
        };
        assert_eq!(
            with_binding
                .resolve_session(
                    &ada(),
                    None,
                    &codex_meta(Some("subagent-thread"), Some("cache-key"))
                )
                .await
                .ok(),
            Some(subagent()),
            "the thread binding is exact and the family's cache key is not, so              reading the cache key first would answer every subagent about its              parent"
        );

        // CONTROL: a cache key naming no conversation of this caller's falls
        // through like any other correlator, rather than refusing.
        assert_eq!(
            tables
                .resolve_session(&ada(), None, &codex_meta(None, Some("nobodys")))
                .await
                .ok(),
            Some(most_recent()),
        );

        // CONTROL: a Claude-shaped call carries none of this and is unaffected.
        let claude = Tables {
            calls: HashMap::from([("toolu_sub", subagent())]),
            latest: Some(most_recent()),
            ..Tables::default()
        };
        assert_eq!(
            claude
                .resolve_session(&ada(), None, &correlators(None, Some("toolu_sub")))
                .await
                .ok(),
            Some(subagent()),
        );
    }

    /// M14.1: a table lookup that could not reach its store refuses, where an
    /// id the store answered "no" about falls through.
    ///
    /// The two are one line apart in the resolver and were one answer before
    /// the tables became shared: `Option` could only say "nothing of yours",
    /// so an unreachable store handed the caller `latest` — a plausible answer
    /// about the wrong conversation. The control below is what proves this
    /// test is about the outage and not about the correlator being unknown.
    #[tokio::test]
    async fn a_correlator_whose_store_is_unreachable_refuses_rather_than_guessing() {
        let outage = Tables {
            calls: HashMap::from([("toolu_sub", subagent())]),
            latest: Some(most_recent()),
            outage: true,
            ..Tables::default()
        };
        let error = outage
            .resolve_session(&ada(), None, &correlators(None, Some("toolu_sub")))
            .await
            .expect_err("a store that cannot answer has not answered");
        assert!(
            matches!(error, SurfaceError::Internal(_)),
            "an unreachable call table must not read as an unknown id: {error}"
        );

        let thread_outage = Tables {
            latest: Some(most_recent()),
            outage: true,
            ..Tables::default()
        };
        assert!(
            matches!(
                thread_outage
                    .resolve_session(&ada(), None, &correlators(Some("thread"), None))
                    .await,
                Err(SurfaceError::Internal(_))
            ),
            "and neither must an unreachable thread table"
        );

        // CONTROL: the same tables, reachable. An id nothing bound falls
        // through to the guess, which is the answer the outage must not be
        // confused with.
        let reachable = Tables {
            latest: Some(most_recent()),
            ..Tables::default()
        };
        assert_eq!(
            reachable
                .resolve_session(&ada(), None, &correlators(None, Some("toolu_nobody")))
                .await
                .ok(),
            Some(most_recent()),
        );
    }
}
