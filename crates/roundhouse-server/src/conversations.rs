// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which session a client's own name for a conversation resolves to.
//!
//! # Why this is a shared thing rather than a field of one surface
//!
//! It used to live inside [`responses_api`](crate::responses_api)'s handler
//! state, because that surface was the only thing that named a conversation.
//! The MCP control surface names one too — `prefer`, `status` and
//! `declare_intent` all take an optional `conversation`, spelled as the client's
//! own `prompt_cache_key` — and it has to arrive at the *same* session id, or an
//! agent narrows the routing of a session no turn will ever run in. Two maps
//! would agree only while nothing had forked; one map cannot disagree at all.
//!
//! # Three questions, one set of maps, and why they belong together
//!
//! A client names a conversation in one of three ways, and all three are
//! answered from the same state:
//!
//! - **By cache key.** `{project}/{user}/{key}` at generation zero, plus a
//!   `#g{n}` suffix once a client has edited its own history out from under a
//!   session. Only the generation map knows what `n` is.
//! - **By the tool-use id of the call it is answering** (M12, R-M2). Claude
//!   Code puts `_meta["claudecode/toolUseId"]` on every `tools/call` it makes,
//!   and that id is one *roundhouse emitted* — so the session that emitted it is
//!   knowable exactly, with no guess and no race, for as long as one session
//!   claims it. See [`Conversations::bind_call`].
//! - **By the thread the client says the call belongs to** (M12.1 review,
//!   R-M9). Codex stamps `_meta.threadId` on every `tools/call`, and the same
//!   id rides the turn that opened the thread in the `x-codex-turn-metadata`
//!   header — so the session a thread is in is knowable exactly, and knowable
//!   *per thread* where the cache key is shared by a whole agent family. See
//!   [`Conversations::bind_thread`].
//! - **Not at all.** The MCP surface's `conversation` argument is optional, and
//!   omitted it means the principal's most recent conversation — which is only
//!   knowable by having watched the turns go past, which is what
//!   [`Conversations::commit`] does on every request either wire surface
//!   serves.
//!
//! # Where the three maps live, since M14.1
//!
//! Not here. Generations, call bindings and thread bindings are
//! [`CorrelationMaps`] in `roundhouse-core`, with an in-process implementation
//! and a Redis one, and this type is the *conversation vocabulary* over them:
//! the `#g{n}` naming convention, the reader-versus-turn distinction, `latest`,
//! and the node-local memo below.
//!
//! What that buys is the thing M12.1's F9 could only half-promise. This type
//! held a `HashMap` behind a `Mutex` in one process, so a client that
//! reconnected to another node kept its cache key and lost its generation, and
//! an MCP call on a node that had served none of this principal's turns was
//! refused rather than answered. With a durable map behind it, "never bound on
//! *this node*" becomes "never bound *anywhere*" — the same refusal with the
//! scope it always should have had (R12, R-C2).
//!
//! # The memo is the turn path's, and every reader goes to the store
//!
//! R-C2 asks for a write-through cache so the common turn stays a local
//! lookup, and [`Conversations::generation`] is where that cache lives: a
//! node's first touch of a key reads the store, every later turn on the same
//! key reads the memo, and [`Conversations::commit`] writes both. That is legal
//! for exactly one reason and it is M14.0's: **a generation is where a search
//! starts, not the answer it returns.** A memo that has gone stale — another
//! node forked the key in between — costs the probe a walk of one or two extra
//! reads and lands the turn in the same place, because prefix admission checks
//! whatever it starts from against the log before committing to it.
//!
//! **No reader is cached, and that is the same rule and not an exception.**
//! [`Conversations::resolve`], [`Conversations::session_of_call`] and
//! [`Conversations::session_of_thread`] answer an MCP control call *directly*:
//! nothing downstream checks their answer against a log, so a stale one is
//! served to the agent with a 200 on it. That is precisely M12.1 F9's defect
//! — "a log another node has already forked away from, quietly" — and a
//! per-node memo would reintroduce it one fork later:
//!
//! - a memoised **generation** goes stale the moment another node's turn forks
//!   the key, and `resolve` would then narrow the routing of the pre-fork
//!   session;
//! - a memoised **call binding** cannot see the ambiguous marker another node's
//!   colliding claim wrote, so the node that bound the id first would keep
//!   answering confidently for it — M12's F14 with a network in the middle;
//! - a memoised **thread binding** goes stale on any fork served elsewhere, and
//!   a thread is *defined* as the session its own latest turn decided (M12.1
//!   review, F2), which is the turn this node did not serve.
//!
//! So the memo covers the one family whose consumer re-checks it, and the
//! readers pay a store round trip they were already paying a `last_seq` beside.
//! `the_node_memo_does_not_answer_a_reader` and
//! `a_binding_another_node_moved_is_read_from_the_store` below are what hold
//! that line.
//!
//! **`latest` stays node-local and stays a guess**, and that is a decision
//! rather than an omission (R12). It answers "which conversation is this agent
//! working in", which is knowable only by having watched turns arrive; two
//! nodes serving one agent would each write their own answer to a shared map,
//! and whichever wrote last would speak for both. A guess that is honest about
//! its scope beats a shared one that is not.
//!
//! **A key no node holds a binding for is still `None`, never generation zero**
//! (M12.1 review, F9). Zero is a real session id in the shared store the moment
//! any node mints it, so a reader that minted one would hand back a pre-fork
//! log with a 200 on it. What survives a restart is a *turn* re-binding its own
//! cache key — [`Conversations::generation`] still starts an unknown key's
//! search at generation zero, so the common never-forked conversation re-binds
//! to the same log — not a reader guessing at one.
//!
//! **What a re-derived generation costs is one read, not a fork.** A counter
//! that starts at zero says nothing about which generations the shared store
//! holds, so prefix admission searches the key's family for the one that
//! agrees with the claim and lands the turn there — see
//! [`prefix_admission`](crate::prefix_admission), which is the one home of
//! that rule and of what the alternative cost. An agreeing restart therefore
//! forks nothing and loses no warm prefix; it pays one extra read of the
//! generation it walked past.
//!
//! # What a correlation-store failure does, by which half is asking
//!
//! The turn path **degrades and says so**; the reader path **refuses**.
//!
//! A turn asks for a hint and records where it landed. A hint that could not be
//! loaded is still a hint — the probe checks it — and a commit that could not
//! be written costs another node's next probe a walk of the same bounded size.
//! Failing the turn instead would make the serving path depend on the
//! correlation store for something nothing trusts unchecked, and on a
//! deployment where both live in one Redis the *session log* fails loudly on
//! its own a few lines later, which is the failure an operator has to act on.
//! Every degradation is logged at warn, because "the maps are unreachable" is
//! not something to learn from a latency graph.
//!
//! A reader has no such check. Answering `None` for an unreachable store would
//! spell "no conversation of yours" — the caller then falls through to
//! `latest`, a plausible answer about the wrong conversation, which is the one
//! failure the whole correlation ruling exists to remove. So the three readers
//! return the error, and `mcp_api` renders it as an internal fault rather than
//! as a tenancy answer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use roundhouse_core::control::{
    CorrelationError, CorrelationMaps, MemoryCorrelationMaps, Principal,
};
use roundhouse_core::ids::SessionId;

/// The binding from a client's names to the sessions holding them.
pub struct Conversations {
    /// The three families, wherever this deployment keeps them.
    ///
    /// Behind the trait rather than as the concrete memory maps, because
    /// which implementation is here is the composition root's one decision
    /// (R-C4) and every method below reads identically either way.
    maps: Arc<dyn CorrelationMaps>,
    /// This node's memo of the generation map. See the module doc for why the
    /// turn path may hold one and no reader may.
    ///
    /// `Option<u32>` rather than `u32`, so the memo distinguishes three states
    /// where the map itself has two: not read through yet, read and absent,
    /// read and committed at *n*. Collapsing the middle one into zero would be
    /// F9's defect stored locally — the node would stop asking the store about
    /// a key it had only ever heard silence about.
    ///
    /// Uncapped, like the generation map it memoises and for its reason: this
    /// is bounded by the number of distinct conversations a node has served
    /// rather than by their tool traffic. M14.2 gives it the staleness bound
    /// the two binding families already have.
    generations: Mutex<HashMap<String, Option<u32>>>,
    /// The session each principal most recently drove a turn on. Node-local by
    /// contract — see the module doc.
    latest: Mutex<HashMap<Principal, SessionId>>,
}

impl std::fmt::Debug for Conversations {
    /// Hand-written because [`CorrelationMaps`] is a trait object: what a
    /// reader of a debug line wants is the shape of this node's own state,
    /// which is the two maps below.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Conversations")
            .field("generations", &self.generations)
            .field("latest", &self.latest)
            .finish_non_exhaustive()
    }
}

impl Default for Conversations {
    fn default() -> Self {
        Self::new()
    }
}

impl Conversations {
    /// Over maps that live and die with this process.
    ///
    /// What a deployment that named no Redis gets, and what every test that is
    /// not about the durable seam gets.
    pub fn new() -> Self {
        Self::over(Arc::new(MemoryCorrelationMaps::new()))
    }

    /// Over whichever maps the composition root chose (R-C4).
    pub fn over(maps: Arc<dyn CorrelationMaps>) -> Self {
        Self {
            maps,
            generations: Mutex::new(HashMap::new()),
            latest: Mutex::new(HashMap::new()),
        }
    }

    /// The session `key` names now, and a note that `principal` is using it.
    ///
    /// The `latest` half is recorded on the way in rather than at the end of
    /// the turn because it answers "which conversation is this agent working
    /// in", and an agent that opened a turn is working in it whether or not
    /// the turn went on to succeed.
    ///
    /// **Not what a wire surface calls any more**, and the reason is M14.0's
    /// review: a turn's generation is not known until prefix admission has
    /// searched for it, so binding on the way in wrote a session the request
    /// might never land on. [`Self::commit`] is the write that happens once
    /// the answer is known, and this remains for the callers that genuinely do
    /// mean "generation zero, now" — a fixture standing a conversation up
    /// without driving a turn through it.
    pub async fn bind(&self, principal: &Principal, key: &str) -> SessionId {
        // Written rather than read-with-a-default, because the entry's
        // *presence* is what tells a reader this key was bound at all (M12.1
        // review, F9). A read that defaulted to zero here would leave every
        // never-forked conversation indistinguishable — to `resolve` — from a
        // key nothing has ever heard of.
        let generation = self.generation(key).await;
        self.commit(principal, key, generation).await
    }

    /// Rebind `key` to a fresh session, because the client's history disagreed
    /// with the log.
    ///
    /// **Superseded on the serving path by [`Self::commit`]** (M14.0 review):
    /// admission no longer advances a counter per attempt, so what used to be
    /// a fork is now a commit to whichever generation the search settled on.
    /// The paragraph below still describes what moving off a session costs,
    /// whichever call does the moving.
    ///
    /// # What a fork costs the control plane, stated rather than hidden
    ///
    /// `ControlStore`'s four families — overlay, intent, steer payload, session
    /// binding — are keyed by the `SessionId` this rebinds *away from*, and
    /// nothing migrates them. The visible consequence is one: an agent that
    /// asked for `scope=session` narrowing, on a client that then edited its own
    /// history mid-session, silently stops being narrowed — the engine asks the
    /// post-fork id for an overlay and finds none. The trigger is specific and
    /// worth naming, because it is not "a fork happens": a fork happens when a
    /// *client rewrites history it already sent*, which is a compaction or a
    /// user editing a message, not an ordinary turn.
    ///
    /// Not migrated on purpose. Carrying an overlay across a fork means a
    /// narrowing surviving a history rewrite that the agent which asked for it
    /// may no longer be running, and deciding that is a decision about what an
    /// overlay's identity *is* — which belongs to the milestone that gives it a
    /// durable one (M8), not to a rebind hook reaching across two crates. What
    /// holds in the meantime is a bound rather than a fix: the orphaned records
    /// are collected by `ControlStore`'s retention sweep like any other aged
    /// state, so a forking client leaks for a day and not for the process's
    /// life.
    pub async fn fork(&self, principal: &Principal, key: &str) -> SessionId {
        let generation = self.generation(key).await.saturating_add(1);
        self.commit(principal, key, generation).await
    }

    /// Which generation of `key` a turn was last committed to, or zero for a
    /// key no node has ever committed.
    ///
    /// Where [`Self::resolve`] refuses an unbound key, this answers zero for
    /// one, and the difference is who is asking. `resolve` answers a *reader* —
    /// an MCP call naming a conversation — and a reader that guesses hands back
    /// a log another node has already forked away from (M12.1 review, F9). This
    /// answers prefix admission, which is not guessing but choosing where to
    /// start a search it will check against the store anyway: zero is the
    /// generation a never-bound key would be at if it exists, and a search that
    /// begins there confirms or abandons it in one probe.
    ///
    /// **The node's first touch of a key reads the store; every later turn on
    /// it reads the memo** (R-C2). What that memo can be wrong about, and why
    /// being wrong about it is bounded rather than dangerous, is the module
    /// doc's; what it buys is one saved round trip on every turn after the
    /// first of a conversation, which on the turn path is the number that
    /// matters.
    ///
    /// Deliberately a read: it records nothing in the *store* and moves
    /// nothing, so a request that goes on to be refused leaves the maps exactly
    /// as it found them. See [`Self::commit`].
    pub async fn generation(&self, key: &str) -> u32 {
        if let Some(memoised) = self.memoised(key) {
            return memoised.unwrap_or(0);
        }
        match self.maps.generation(key).await {
            Ok(found) => {
                self.memoise(key, found);
                found.unwrap_or(0)
            }
            // Degraded, not refused — see the module doc. Deliberately *not*
            // memoised: an unreachable store is a transient this node should
            // ask about again on the next turn, where a remembered absence
            // would make one outage the answer for the process's life.
            Err(error) => {
                tracing::warn!(
                    %error,
                    key,
                    "the correlation maps could not be read for this cache key; the \
                     prefix search starts at generation zero and walks, which costs \
                     reads rather than correctness"
                );
                0
            }
        }
    }

    /// Record that `principal`'s turn on `key` is landing on `generation`.
    ///
    /// **The one write prefix admission makes, and it happens after the answer
    /// is known** (M14.0 review). Its predecessors — a `bind` on the way in and
    /// a `fork` per attempt — wrote as they searched, so a request that ended
    /// in a refusal still left the counter advanced and `latest` naming a
    /// generation no turn had run on: the refusal stopped one request while the
    /// retry behind it resumed past the bound, and an unnamed MCP call in
    /// between was answered with a dead session. Committing once, at the end,
    /// is what makes a refusal cost nothing.
    ///
    /// The counter is *set* rather than incremented, because the search that
    /// chose `generation` may have walked backwards to an older generation the
    /// claim still continues — see [`prefix_admission`](crate::prefix_admission).
    ///
    /// Written through to the store and to this node's memo, in that order.
    /// **The memo is written even when the store write failed**, and that is
    /// deliberate: this node's turn did land on `generation`, so this node's
    /// next probe should start there. What the lost write costs is another
    /// node's next probe walking to find it, which is the bounded cost R-C2
    /// already accepts for two nodes committing different generations.
    ///
    /// `latest` moves for [`Self::bind`]'s reason: it answers "which
    /// conversation is this agent working in", and an agent whose turn is about
    /// to open is working in it whether or not the turn goes on to succeed.
    ///
    /// What moving off a generation costs the control plane is
    /// [`Self::fork`]'s paragraph, unchanged: `ControlStore`'s records are
    /// keyed by the session id this commits *away from*, and nothing migrates
    /// them.
    pub async fn commit(&self, principal: &Principal, key: &str, generation: u32) -> SessionId {
        if let Err(error) = self.maps.set_generation(key, generation).await {
            tracing::warn!(
                %error,
                key,
                generation,
                "this turn's generation could not be recorded in the correlation maps; \
                 another node's next probe of this key walks to find it"
            );
        }
        self.memoise(key, Some(generation));
        self.mark_latest(principal, bound_session(key, generation))
    }

    /// The session `key` names now, without claiming to be using it, or `None`
    /// for a key no node holds a binding for.
    ///
    /// What a *reader* asks — the MCP surface resolving an explicit
    /// `conversation` argument. Distinct from [`Self::bind`] because an agent
    /// asking `status` about a conversation must not thereby make that
    /// conversation its most recent one: the two tools that take the argument
    /// and the tool that omits it would then disagree about what "most recent"
    /// means, in an order the agent chose.
    ///
    /// **`None` and not generation zero for an unbound key** (M12.1 review,
    /// F9). Zero is what [`Self::bind`] would mint, but a reader minting it is
    /// not the same act as a turn minting it: the store is shared across
    /// nodes, so a generation-zero id exists there whenever *any* node ever
    /// created it, and answering with one hands the caller a superseded log
    /// that another node has already forked away from — quietly, with a 200 on
    /// it. A reader that does not know refuses; see the module doc.
    ///
    /// **Read from the store and never from this node's memo**, which is the
    /// same rule seen from the reader's side: the memo is a probe's starting
    /// point and a probe corrects it, where this answer is served to an agent
    /// unchecked. A node that had committed generation 2 and then watched
    /// another node fork to 3 would otherwise narrow the routing of a session
    /// its client has left.
    pub async fn resolve(&self, key: &str) -> Result<Option<SessionId>, CorrelationError> {
        Ok(self
            .maps
            .generation(key)
            .await?
            .map(|generation| bound_session(key, generation)))
    }

    /// The last session this principal drove a turn on, on this node.
    pub fn latest(&self, principal: &Principal) -> Option<SessionId> {
        self.lock_latest().get(principal).cloned()
    }

    /// Remember that `session` emitted the tool call `call_id`, for `principal`.
    ///
    /// **Recorded at the moment the call is streamed to the client**, because
    /// that is the only moment both halves are in one place: the surface knows
    /// which session it is following, and the item it is about to project
    /// carries the id the client will quote back. A later reconstruction —
    /// scanning logs for a `call_id` — would need to know which log to scan,
    /// which is the question this answers.
    ///
    /// Deliberately *not* a claim that the principal is now working in this
    /// conversation: [`Self::latest`] is unmoved. An agent whose subagent runs a
    /// tool must not thereby redirect its parent's next unnamed MCP call, which
    /// is the very race R-M2 exists to remove.
    ///
    /// An id two of this principal's sessions have bound is remembered as
    /// *ambiguous* rather than resolved in favour of the later writer — see
    /// [`CorrelationMaps::bind_call`] for why an id is not a name on every
    /// backend.
    ///
    /// A store that refused the write degrades to the pre-R-M2 answer for that
    /// one id — the MCP call quoting it falls back to `latest` — and says so.
    pub async fn bind_call(&self, principal: &Principal, call_id: &str, session: SessionId) {
        if let Err(error) = self.maps.bind_call(principal, call_id, &session).await {
            tracing::warn!(
                %error,
                call_id,
                "a tool call could not be bound to its session; a control call \
                 answering it falls back to this principal's most recent conversation"
            );
        }
    }

    /// The session that emitted `call_id`, if it was emitted *for this
    /// principal*.
    ///
    /// **A foreign id, an unknown one and an ambiguous one all answer the same
    /// way, and that is the decision.** Returning something distinguishable for
    /// "this id exists but is somebody else's" would make the argument an
    /// enumeration oracle — the reasoning `mcp_api::resolve_session` already
    /// collapses a foreign cache key under — and it would buy nothing: the
    /// caller has no use for another tenant's session, so both answers lead to
    /// the same next step, which is to fall back to the caller's own most
    /// recent conversation. An ambiguous id joins them for the same reason
    /// turned around: nothing distinguishes *which* of the two claiming
    /// sessions the caller meant, so there is no answer to give that is better
    /// than the fallback.
    ///
    /// A store that could not be *reached* is none of those three and is
    /// returned as itself — see the module doc's last section.
    pub async fn session_of_call(
        &self,
        principal: &Principal,
        call_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        self.maps.session_of_call(principal, call_id).await
    }

    /// Remember that `principal`'s thread `thread_id` is in `session`.
    ///
    /// **Written by the turn ingest, from the client's own header**, because
    /// that is the one moment both halves are in one place: the surface has
    /// just decided which session this turn's history belongs to, and the
    /// request that carried the history also carried the thread it came from.
    /// Nothing later can reconstruct the pairing — the thread id is not in the
    /// log, and the cache key that is in it is the whole agent family's rather
    /// than this thread's.
    ///
    /// Rebinding is ordinary and the latest write wins: a fork moves a thread
    /// to a new session, and the thread is then in the new one.
    ///
    /// Deliberately *not* a claim about [`Self::latest`], for
    /// [`Self::bind_call`]'s reason turned around: the ingest that calls this
    /// has already moved `latest` for the turn it is serving, and moving it a
    /// second time from a subagent's header is how the parent's next unnamed
    /// call gets redirected.
    pub async fn bind_thread(&self, principal: &Principal, thread_id: &str, session: SessionId) {
        if let Err(error) = self.maps.bind_thread(principal, thread_id, &session).await {
            tracing::warn!(
                %error,
                thread_id,
                "a thread could not be bound to the session its turn landed in; a \
                 control call from that thread falls back to the named path and then \
                 to this principal's most recent conversation"
            );
        }
    }

    /// The session `principal`'s thread `thread_id` is in, if any node served a
    /// turn of it.
    ///
    /// A foreign thread id and an unknown one answer alike, for the reason
    /// [`Self::session_of_call`] spells out at length: the caller has no use
    /// for another tenant's session, so distinguishing the two would buy an
    /// enumeration oracle and nothing else.
    pub async fn session_of_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        self.maps.session_of_thread(principal, thread_id).await
    }

    /// What this node last read or wrote for `key`, if it has touched it.
    fn memoised(&self, key: &str) -> Option<Option<u32>> {
        self.lock_generations().get(key).copied()
    }

    /// Remember what the store said, or what this node just committed.
    fn memoise(&self, key: &str, generation: Option<u32>) {
        self.lock_generations().insert(key.to_string(), generation);
    }

    /// Record that `principal` is working in `session`, and hand it back.
    ///
    /// One helper rather than the same two lines in `bind`, `fork` and
    /// `commit`: the three differ in which generation they arrived at and in
    /// nothing else, and a fourth caller that forgot the `latest` half would
    /// leave an agent's next unnamed MCP call answered from a conversation it
    /// has moved on from.
    fn mark_latest(&self, principal: &Principal, session: SessionId) -> SessionId {
        self.lock_latest()
            .insert(principal.clone(), session.clone());
        session
    }

    /// The locks, in one place each.
    ///
    /// Recovering a poisoned guard rather than propagating the panic: a lost
    /// `latest` is one MCP call that has to name its conversation and a lost
    /// memo is one extra store read, and failing every later request over one
    /// poisoned map is a worse outcome than serving the next one from
    /// possibly-stale state.
    fn lock_latest(&self) -> std::sync::MutexGuard<'_, HashMap<Principal, SessionId>> {
        self.latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_generations(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<u32>>> {
        self.generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// This node's session id for a namespaced cache key at a given generation.
///
/// Generation zero is the key verbatim, so a session survives a process
/// restart that loses the generation map: the common case is a conversation
/// that never forked, and it re-binds to the same log.
///
/// **Public because the convention has to have exactly one home.** Prefix
/// admission searches a key's generations by name, and every test that seeds
/// or asserts on one names it the same way — a second spelling of `#g{n}`
/// anywhere is a second convention, and the two would agree only until one of
/// them moved.
pub fn bound_session(key: &str, generation: u32) -> SessionId {
    match generation {
        0 => SessionId::new(key),
        n => SessionId::new(format!("{key}#g{n}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    fn ada() -> Principal {
        Principal::new("acme", "ada")
    }

    fn bob() -> Principal {
        Principal::new("globex", "bob")
    }

    /// Two `Conversations` over one set of maps: the deployment this rung
    /// exists for, expressed without a Redis.
    ///
    /// A shared [`MemoryCorrelationMaps`] is the same seam a shared Redis is —
    /// one store, two nodes' vocabularies over it — and it isolates what this
    /// file is responsible for from what the backend is. The Redis half of the
    /// same claim is `tests/correlation_any_node.rs`, gated on a real server.
    fn two_nodes() -> (Conversations, Conversations) {
        let maps = Arc::new(MemoryCorrelationMaps::new());
        (
            Conversations::over(Arc::clone(&maps) as Arc<dyn CorrelationMaps>),
            Conversations::over(maps),
        )
    }

    #[tokio::test]
    async fn a_reader_and_a_turn_resolve_one_cache_key_to_one_session() {
        // The whole reason these maps are shared rather than owned by the
        // Responses surface: an overlay installed against `resolve`'s answer
        // has to reach the session `bind` hands the engine, generation and all.
        let conversations = Conversations::new();
        let key = "acme/ada/main";

        // F9 (M12.1 review): before a turn has bound it, the key names nothing
        // — not generation zero, which is a real session id in the shared
        // store the moment any node mints it.
        assert_eq!(
            conversations.resolve(key).await.unwrap(),
            None,
            "a reader with no binding must say so rather than mint the id a \
             first turn would have minted"
        );

        assert_eq!(
            Some(conversations.bind(&ada(), key).await),
            conversations.resolve(key).await.unwrap()
        );

        let forked = conversations.fork(&ada(), key).await;
        assert_eq!(forked.as_str(), "acme/ada/main#g1");
        assert_eq!(
            conversations.resolve(key).await.unwrap(),
            Some(forked.clone()),
            "a rebound key stays rebound: a reader that kept answering \
             generation zero would narrow a session no turn will run in"
        );
        assert_eq!(conversations.bind(&ada(), key).await, forked);
    }

    /// R-M2 (M12): a tool-use id names one session exactly, and binding one is
    /// not a claim about who is working where.
    ///
    /// The correlation semantics themselves — the partition by principal, the
    /// unknown and foreign ids — are the shared contract's now and are
    /// asserted against both backends in
    /// `roundhouse_core::control::correlation::contract`. What is *this*
    /// type's is the second assertion: `latest` does not move.
    #[tokio::test]
    async fn binding_a_tool_call_names_its_session_without_moving_latest() {
        let conversations = Conversations::new();
        let subagent = conversations.bind(&ada(), "acme/ada/sub").await;
        let parent = conversations.bind(&ada(), "acme/ada/main").await;
        conversations
            .bind_call(&ada(), "toolu_sub", subagent.clone())
            .await;

        assert_eq!(
            conversations
                .session_of_call(&ada(), "toolu_sub")
                .await
                .unwrap(),
            Some(subagent),
            "the session that emitted the call is the session the answer to it \
             concerns, whatever else the principal has been doing since"
        );

        // Binding a call is not a claim that the principal is now working in
        // that conversation — the very race this exists to remove would come
        // straight back if a subagent's tool call moved its parent's `latest`.
        assert_eq!(conversations.latest(&ada()), Some(parent));
    }

    #[tokio::test]
    async fn reading_a_conversation_does_not_make_it_the_principals_most_recent_one() {
        let conversations = Conversations::new();
        assert_eq!(
            conversations.latest(&ada()),
            None,
            "a principal no turn has been served for has no most-recent \
             conversation, which is an answer rather than a default"
        );

        conversations.bind(&ada(), "acme/ada/main").await;
        assert_eq!(
            conversations.resolve("acme/ada/other").await.unwrap(),
            None,
            "and a read of a key nothing has bound is a read all the same"
        );
        assert_eq!(
            conversations.latest(&ada()).unwrap().as_str(),
            "acme/ada/main",
            "a `status` call naming a conversation must not become the answer \
             the next `status` call gets for omitting one"
        );

        // The control: a turn on the other conversation does move it.
        conversations.bind(&ada(), "acme/ada/other").await;
        assert_eq!(
            conversations.latest(&ada()).unwrap().as_str(),
            "acme/ada/other"
        );
        // And one principal's turns are not another's.
        assert_eq!(conversations.latest(&bob()), None);
    }

    /// R-M9 (M12.1 review, F2): a thread is in the session its own latest turn
    /// decided, and the thread's family sharing one cache key does not change
    /// that.
    ///
    /// The topology is the oracle's: parent and subagent send one
    /// `prompt_cache_key` and two `thread_id`s, so the cache key forks under
    /// them while each thread stays pinned to the fork its own turn produced.
    /// Here rather than in the shared contract because what it exercises is the
    /// *`#g{n}` naming* interacting with the thread map — a fact about this
    /// type, not about a correlation backend.
    #[tokio::test]
    async fn a_thread_is_in_the_session_its_own_latest_turn_decided() {
        let conversations = Conversations::new();
        let key = "acme/ada/main";

        let parent_g0 = conversations.bind(&ada(), key).await;
        conversations
            .bind_thread(&ada(), "thread-parent", parent_g0.clone())
            .await;
        let child_g1 = conversations.fork(&ada(), key).await;
        conversations
            .bind_thread(&ada(), "thread-child", child_g1.clone())
            .await;
        let parent_g2 = conversations.fork(&ada(), key).await;
        conversations
            .bind_thread(&ada(), "thread-parent", parent_g2.clone())
            .await;

        assert_eq!(
            conversations
                .session_of_thread(&ada(), "thread-child")
                .await
                .unwrap(),
            Some(child_g1),
            "the subagent's thread stays in the fork its own turn produced, \
             however far the shared cache key has moved since — this is the \
             whole of F2"
        );
        assert_eq!(
            conversations
                .session_of_thread(&ada(), "thread-parent")
                .await
                .unwrap(),
            Some(parent_g2.clone()),
            "and a thread that forked is in the session it forked *to*: the \
             latest binding wins, where a colliding call id is refused"
        );

        // Binding a thread is not a claim about who is working where: the
        // ingest has already moved `latest` for the turn it is serving.
        assert_eq!(conversations.latest(&ada()), Some(parent_g2));
    }

    // -----------------------------------------------------------------------
    // M14.1: the memo, and the line it is not allowed to cross
    // -----------------------------------------------------------------------

    /// **The read-through cost R-C2 budgets, counted.** One store read per key
    /// per node, and then none.
    ///
    /// Counted at the seam rather than at Redis, because what this file
    /// decides is how many times it *asks*: the round trip one ask costs is
    /// pinned against a real server by `one_generation_read_is_one_round_trip`
    /// in `roundhouse-store-redis`, and the two together are the whole claim.
    ///
    /// The second half — that a commit primes the memo rather than merely
    /// invalidating it — is what keeps the *first* turn of a conversation to
    /// one read as well: probe, commit, and every later turn is local.
    #[tokio::test]
    async fn a_key_is_read_through_once_per_node_and_then_memoised() {
        let maps = Arc::new(CountingMaps::new());
        let conversations = Conversations::over(Arc::clone(&maps) as Arc<dyn CorrelationMaps>);
        let key = "acme/ada/main";

        assert_eq!(conversations.generation(key).await, 0);
        assert_eq!(
            maps.reads(),
            1,
            "the node's first touch of a key must go to the store, or a client \
             that reconnected elsewhere silently loses its generation"
        );

        for _ in 0..5 {
            assert_eq!(conversations.generation(key).await, 0);
        }
        assert_eq!(
            maps.reads(),
            1,
            "an absent generation is an answer and must be memoised as one; \
             re-asking the store on every turn is the round trip the \
             write-through cache exists to remove"
        );

        conversations.commit(&ada(), key, 3).await;
        assert_eq!(conversations.generation(key).await, 3);
        assert_eq!(
            maps.reads(),
            1,
            "a commit primes the memo with what this node just wrote, so the \
             next turn of the same conversation reads nothing"
        );

        // CONTROL: the memo is per key, not one slot. A second conversation on
        // the same node still pays its own first read.
        assert_eq!(conversations.generation("acme/ada/other").await, 0);
        assert_eq!(maps.reads(), 2);
    }

    /// **The line the memo may not cross.** A reader is answered from the
    /// store, whatever this node last committed.
    ///
    /// The topology is the one the durable maps exist for: this node committed
    /// generation 0 and another node then forked the same key to 1. A `resolve`
    /// answered from the memo would narrow the routing of the session the
    /// client has just left — F9's defect one fork later, and with a 200 on it.
    #[tokio::test]
    async fn the_node_memo_does_not_answer_a_reader() {
        let (node_a, node_b) = two_nodes();
        let key = "acme/ada/main";

        let served_here = node_a.bind(&ada(), key).await;
        let forked_elsewhere = node_b.fork(&ada(), key).await;
        assert_ne!(served_here, forked_elsewhere, "sanity: the fork moved");

        assert_eq!(
            node_a.resolve(key).await.unwrap(),
            Some(forked_elsewhere),
            "a reader must answer from the store: this node's memo says \
             generation 0, and the conversation is at 1"
        );

        // CONTROL: the turn path *is* allowed to start from the stale memo,
        // because prefix admission checks whatever it starts from against the
        // log before committing to it. If this ever changes, the read-through
        // cost test above is measuring something else.
        assert_eq!(node_a.generation(key).await, 0);
    }

    /// The same line for the two binding families: a call another node made
    /// ambiguous, and a thread another node moved.
    ///
    /// Both are read from the store on every ask for the same reason `resolve`
    /// is — nothing downstream re-checks them — and both would be wrong under
    /// a node-local table read first. The call half is M12's F14 with a
    /// network in the middle; the thread half is F2's "the session its own
    /// latest turn decided", where the latest turn was served elsewhere.
    #[tokio::test]
    async fn a_binding_another_node_moved_is_read_from_the_store() {
        let (node_a, node_b) = two_nodes();
        let first = SessionId::new("acme/ada/first");
        let second = SessionId::new("acme/ada/second");

        node_a.bind_call(&ada(), "call_0", first.clone()).await;
        node_b.bind_call(&ada(), "call_0", second.clone()).await;
        assert_eq!(
            node_a.session_of_call(&ada(), "call_0").await.unwrap(),
            None,
            "the node that bound the id first must see the collision the \
             second node's claim made, or it answers its own still-open \
             tools/call confidently about the wrong session"
        );

        node_a.bind_thread(&ada(), "thread-1", first).await;
        node_b.bind_thread(&ada(), "thread-1", second.clone()).await;
        assert_eq!(
            node_a.session_of_thread(&ada(), "thread-1").await.unwrap(),
            Some(second),
            "a thread is in the session its own latest turn decided, and that \
             turn was served on the other node"
        );
    }

    /// A store that cannot be reached is a fact about the deployment, and the
    /// two halves of this type treat it differently on purpose.
    ///
    /// The turn path degrades — a generation is a hint, and a hint nobody can
    /// load still leaves the probe a starting point — while every reader
    /// returns the error, because a reader's `None` reads as "no conversation
    /// of yours" and sends the caller to `latest`: a plausible answer about
    /// the wrong conversation.
    #[tokio::test]
    async fn an_unreachable_store_degrades_the_turn_path_and_refuses_the_readers() {
        let conversations = Conversations::over(Arc::new(OutageMaps));
        let key = "acme/ada/main";

        assert_eq!(
            conversations.generation(key).await,
            0,
            "the search still has a place to start"
        );
        // And the failure is not memoised: the next turn asks again rather
        // than serving one outage for the life of the process.
        assert_eq!(conversations.memoised(key), None);
        // A commit still moves `latest` and still names the session, so the
        // turn it belongs to is served.
        assert_eq!(
            conversations.commit(&ada(), key, 2).await.as_str(),
            "acme/ada/main#g2"
        );

        assert!(conversations.resolve(key).await.is_err());
        assert!(
            conversations
                .session_of_call(&ada(), "toolu_1")
                .await
                .is_err()
        );
        assert!(
            conversations
                .session_of_thread(&ada(), "thread-1")
                .await
                .is_err()
        );
    }

    /// Maps that count what the node asks of them.
    ///
    /// Wrapping the memory maps rather than reimplementing them: what is under
    /// test is how often `Conversations` reaches for the store, and a double
    /// with its own semantics would let the count be right while the answers
    /// were not.
    struct CountingMaps {
        inner: MemoryCorrelationMaps,
        reads: AtomicUsize,
    }

    impl CountingMaps {
        fn new() -> Self {
            Self {
                inner: MemoryCorrelationMaps::new(),
                reads: AtomicUsize::new(0),
            }
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl CorrelationMaps for CountingMaps {
        async fn generation(&self, key: &str) -> Result<Option<u32>, CorrelationError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            CorrelationMaps::generation(&self.inner, key).await
        }

        async fn set_generation(&self, key: &str, generation: u32) -> Result<(), CorrelationError> {
            CorrelationMaps::set_generation(&self.inner, key, generation).await
        }

        async fn bind_call(
            &self,
            principal: &Principal,
            call_id: &str,
            session: &SessionId,
        ) -> Result<(), CorrelationError> {
            CorrelationMaps::bind_call(&self.inner, principal, call_id, session).await
        }

        async fn session_of_call(
            &self,
            principal: &Principal,
            call_id: &str,
        ) -> Result<Option<SessionId>, CorrelationError> {
            CorrelationMaps::session_of_call(&self.inner, principal, call_id).await
        }

        async fn bind_thread(
            &self,
            principal: &Principal,
            thread_id: &str,
            session: &SessionId,
        ) -> Result<(), CorrelationError> {
            CorrelationMaps::bind_thread(&self.inner, principal, thread_id, session).await
        }

        async fn session_of_thread(
            &self,
            principal: &Principal,
            thread_id: &str,
        ) -> Result<Option<SessionId>, CorrelationError> {
            CorrelationMaps::session_of_thread(&self.inner, principal, thread_id).await
        }
    }

    /// Maps that are never reachable, which is the one failure the trait has.
    struct OutageMaps;

    fn outage() -> CorrelationError {
        CorrelationError::Backend(anyhow::anyhow!("the correlation store is unreachable"))
    }

    #[async_trait]
    impl CorrelationMaps for OutageMaps {
        async fn generation(&self, _key: &str) -> Result<Option<u32>, CorrelationError> {
            Err(outage())
        }

        async fn set_generation(
            &self,
            _key: &str,
            _generation: u32,
        ) -> Result<(), CorrelationError> {
            Err(outage())
        }

        async fn bind_call(
            &self,
            _principal: &Principal,
            _call_id: &str,
            _session: &SessionId,
        ) -> Result<(), CorrelationError> {
            Err(outage())
        }

        async fn session_of_call(
            &self,
            _principal: &Principal,
            _call_id: &str,
        ) -> Result<Option<SessionId>, CorrelationError> {
            Err(outage())
        }

        async fn bind_thread(
            &self,
            _principal: &Principal,
            _thread_id: &str,
            _session: &SessionId,
        ) -> Result<(), CorrelationError> {
            Err(outage())
        }

        async fn session_of_thread(
            &self,
            _principal: &Principal,
            _thread_id: &str,
        ) -> Result<Option<SessionId>, CorrelationError> {
            Err(outage())
        }
    }
}
