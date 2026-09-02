// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which session a client's own name for a conversation resolves to, on this
//! node.
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
//! # Three questions, one table, and why they belong together
//!
//! A client names a conversation in one of three ways, and all three are
//! answered from the same node-local state:
//!
//! - **By cache key.** `{project}/{user}/{key}` at generation zero, plus a
//!   `#g{n}` suffix once a client has edited its own history out from under a
//!   session. Only this table knows what `n` is.
//! - **By the tool-use id of the call it is answering** (M12, R-M2). Claude
//!   Code puts `_meta["claudecode/toolUseId"]` on every `tools/call` it makes,
//!   and that id is one *roundhouse emitted* — so the session that emitted it is
//!   knowable exactly, with no guess and no race, for as long as one session
//!   claims it. See [`Conversations::bind_call`] and [`CallTable`].
//! - **By the thread the client says the call belongs to** (M12.1 review,
//!   R-M9). Codex stamps `_meta.threadId` on every `tools/call`, and the same
//!   id rides the turn that opened the thread in the `x-codex-turn-metadata`
//!   header — so the session a thread is in is knowable exactly, and knowable
//!   *per thread* where the cache key is shared by a whole agent family. See
//!   [`Conversations::bind_thread`] and [`ThreadTable`].
//! - **Not at all.** The MCP surface's `conversation` argument is optional, and
//!   omitted it means the principal's most recent conversation — which is only
//!   knowable by having watched the turns go past, which is what
//!   [`Conversations::bind`] does on every request the Responses surface serves.
//!
//! # Node-local, deliberately, and on a stated precedent
//!
//! A `HashMap` behind a `Mutex` in one process, exactly as the generations map
//! it grew out of always was: process state standing in for a durable mapping
//! the Redis store will own (M8). What the choice costs, said plainly: a client
//! that reconnects to another node keeps its cache key and loses its generation,
//! which re-derives on the first request that disagrees with the log; and an MCP
//! call on a node that has served none of this principal's turns is refused
//! rather than answered — whether it named a conversation, correlated one, or
//! omitted both. Refusals and re-derivations, never a wrong session served
//! quietly.
//!
//! **Re-deriving a generation is not minting one, and the difference is R13**
//! (M14.0). A fresh process's counter starts at zero, so the first disagreeing
//! claim after a restart forks straight back to `#g1` — a name the *shared
//! store* may already hold a log under, from before the restart forgot it. The
//! fork this node computes is checked against whatever that name already
//! holds, exactly as [`bind_prefix`](crate::responses_api::bind_prefix) checks
//! any other session: an agreeing log continues from the delta, a disagreeing
//! one forks again. Only a generation the store has genuinely never seen takes
//! the claim whole. So a restart costs the one avoidable fork's warm prefix —
//! the same honest cost this table's fork always names — and never the
//! duplicated log that treating a re-derived generation as empty would have
//! produced.
//!
//! **That last clause used to be false for a name** (M12.1 review, F9).
//! [`Conversations::resolve`] answered generation zero for a key this node had
//! never bound, and a generation-zero id *exists in the shared store* whenever
//! any node ever created it — so a call landing on a fresh node was served the
//! pre-fork log with a 200 on it while another node held the conversation the
//! client was actually in. A key this node holds no binding for is `None` now,
//! and the surface refuses it exactly as it refuses an unknown or a foreign
//! name. What survives a restart is a *turn* re-binding its own cache key —
//! [`Conversations::bind`] still defaults an unknown key to generation zero, so
//! the common never-forked conversation re-binds to the same log — not a reader
//! guessing at one on a node that has served it nothing.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use roundhouse_core::control::Principal;
use roundhouse_core::ids::SessionId;

/// This node's binding from a client's names to the sessions holding them.
#[derive(Debug, Default)]
pub struct Conversations {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// How many times each *namespaced* cache key's history has failed the
    /// prefix check, for every key this node has bound.
    ///
    /// **Presence is load-bearing and not merely a counter's storage** (M12.1
    /// review, F9): an entry means "this node bound this key", which is the
    /// question [`Conversations::resolve`] refuses on. That is why
    /// [`Conversations::bind`] writes a zero rather than reading past a
    /// missing entry — a node serving a never-forked conversation's turns
    /// would otherwise look, to its own reader, exactly like a node that had
    /// never heard of it.
    ///
    /// Keyed by the whole namespaced string — `{project}/{user}/{cache_key}`
    /// where there is a namespace, the bare cache key where there is not —
    /// rather than by the cache key the client sent. Two tenants both naming a
    /// conversation `main` own separate logs, and a shared fork counter would
    /// let an edited history in one of them cold-start the other: the second
    /// tenant's next request would compute a session id at a generation it
    /// never forked to, find it empty, and lose its warm prefix. One string
    /// rather than a `(Principal, key)` tuple because the same string is the
    /// session id's stem, so the counter and the id cannot be keyed on
    /// different things.
    generations: HashMap<String, u32>,
    /// The session each principal most recently drove a turn on.
    latest: HashMap<Principal, SessionId>,
    /// Which session emitted each tool call this node has served, per
    /// principal. See [`CallTable`].
    calls: CallTable,
    /// Which session each client-declared thread is in, per principal. See
    /// [`ThreadTable`].
    threads: ThreadTable,
}

/// Which session emitted each tool call this node has served, remembered per
/// principal and bounded per principal.
///
/// # Why a type and not three more fields of [`Inner`]
///
/// [`Conversations`] holds this behind the same lock as `generations` and
/// `latest`, but not for their reason: [`Conversations::bind_call`] touches
/// neither of the other two, so the module doc's "one map cannot disagree"
/// argument — which is about a reader and a turn agreeing on one generation —
/// never covered this table. What it has instead is an invariant of its own,
/// one queue entry per binding, and that is what makes the cap a bound rather
/// than an occasional tidy-up. Kept inline in one method's body, an invariant
/// like that is one the next edit to that method breaks with nothing red
/// (M12 review, F13).
///
/// # Why the partition is by principal and not by id
///
/// Because a tool-call id is a name only within one tenant, and only for as
/// long as one of that tenant's sessions claims it. Anthropic and OpenAI mint
/// globally unique ids, but a local backend that numbers calls within a
/// response (`call_0`, `call_1`) hands the same string to every conversation
/// it serves, so two concurrent conversations of one principal can claim one
/// id — which a plain `insert` resolves in favour of whoever wrote last, and
/// answers the *other* conversation's `tools/call` with a confident 200 about
/// a session it never asked about (F14). Separately, one node-wide eviction
/// queue spends a quiet tenant's remembered calls on a busy co-tenant's
/// traffic, costing it exactly the exact answer R-M2 exists to give (F15).
#[derive(Debug, Default)]
struct CallTable {
    per_principal: HashMap<Principal, PrincipalCalls>,
}

/// One principal's remembered calls, oldest-first.
#[derive(Debug, Default)]
struct PrincipalCalls {
    sites: HashMap<String, CallSite>,
    /// Insertion order of [`PrincipalCalls::sites`], so the cap evicts the
    /// oldest. Exactly one entry per site, which is this type's invariant.
    order: VecDeque<String>,
}

/// What one remembered call id names, if anything.
#[derive(Debug, Clone)]
enum CallSite {
    /// The single session that emitted it.
    Bound(SessionId),
    /// Two of this principal's sessions bound it, so it names neither.
    ///
    /// Remembered as ambiguous rather than forgotten: an id dropped from the
    /// table reads as never-seen, so the *next* binding of the same colliding
    /// id would look like a first one and start answering confidently again —
    /// which is the defect, one turn later.
    Ambiguous,
}

impl CallTable {
    /// How many emitted tool calls this node remembers the session of, per
    /// principal.
    ///
    /// A cap and not a policy. Every tool call a dispatched turn emits lands
    /// here, so an uncapped map is a leak proportional to traffic — unlike
    /// `latest`, which is bounded by the number of principals. What losing an
    /// entry costs is exactly one MCP call falling back to the principal's most
    /// recent conversation, which is the answer it got before R-M2 existed; the
    /// window that matters is a single turn's tool loop, and four thousand
    /// calls is far more than any turn emits.
    ///
    /// Per principal, so the node's worst case is this times the number of
    /// principals it has served rather than this outright. That is the same
    /// factor `latest` already carries, and it is the right trade: a tenant
    /// count is something an operator knows and provisions for, where "whose
    /// traffic happened to arrive first" is not.
    const REMEMBERED_CALLS: usize = 4096;

    fn bind(&mut self, principal: &Principal, call_id: &str, session: SessionId) {
        self.per_principal
            .entry(principal.clone())
            .or_default()
            .bind(call_id, session);
    }

    fn session_of(&self, principal: &Principal, call_id: &str) -> Option<SessionId> {
        self.per_principal.get(principal)?.session_of(call_id)
    }

    /// How many bindings this table holds for `principal`, and how long its
    /// eviction queue is.
    ///
    /// One call returning both rather than two accessors, so a test asserts
    /// the invariant on the type that owns it instead of reaching through
    /// `Conversations`' lock into private fields — which is what F13 named.
    #[cfg(test)]
    fn sizes(&self, principal: &Principal) -> (usize, usize) {
        self.per_principal
            .get(principal)
            .map(|calls| (calls.sites.len(), calls.order.len()))
            .unwrap_or_default()
    }
}

impl PrincipalCalls {
    fn bind(&mut self, call_id: &str, session: SessionId) {
        match self.sites.get_mut(call_id) {
            // A resend or a dedup replay re-binds an id this node already holds
            // to the session that already holds it. That is one call seen
            // twice, not two calls, and treating it as a collision would throw
            // away a binding that is still exactly right.
            Some(CallSite::Bound(held)) if *held == session => {}
            Some(site) => *site = CallSite::Ambiguous,
            None => {
                self.sites
                    .insert(call_id.to_string(), CallSite::Bound(session));
                self.order.push_back(call_id.to_string());
            }
        }
        while self.order.len() > CallTable::REMEMBERED_CALLS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.sites.remove(&oldest);
        }
    }

    fn session_of(&self, call_id: &str) -> Option<SessionId> {
        match self.sites.get(call_id)? {
            CallSite::Bound(session) => Some(session.clone()),
            CallSite::Ambiguous => None,
        }
    }
}

/// Which session each client-declared thread is in, remembered per principal
/// and bounded per principal.
///
/// # Why this exists at all when a cache key is already a name
///
/// R-M7 read `_meta.threadId` as a `prompt_cache_key`, on a capture where the
/// two were byte-identical. They are identical only for a codex **root**
/// thread. At the pinned oracle (`6344a65`) a non-root agent takes its
/// `session_id` — and therefore its `prompt_cache_key` — from the shared
/// `AgentControl` (`core/src/agent/control.rs:104-110`, whose own comment says
/// "every sub-agents from a common root share the same session ID";
/// `core/src/session/session.rs:671-676` picks it for any non-root source),
/// while `_meta.threadId` is that agent's *own* `thread_id`
/// (`core/src/session/turn_context.rs:618-622`). So the whole family names one
/// cache key and each member names a different thread, and resolving a thread
/// id as a cache key finds nothing for exactly the callers R-M7 existed to
/// serve (M12.1 review, F2).
///
/// The per-thread marker is on the wire regardless: every turn carries
/// `x-codex-turn-metadata`, whose `thread_id` field
/// (`core/src/responses_metadata.rs:281`, `THREAD_ID_KEY`) is that turn's own
/// thread. The ingest binds it to the session it just decided, and this table
/// is where that binding lives.
///
/// # Why rebinding is the normal case here, where [`CallTable`] calls it a
/// collision
///
/// A tool-call id names one emission for ever, so a second session claiming
/// one is two callers claiming one name and neither may be answered. A thread
/// id names a *conversation*, and a conversation legitimately moves: every
/// fork mints a new session for the same thread, and the turn that forked is
/// the one the client is in. Remembering an `Ambiguous` state here would
/// un-answer every thread the moment its client compacted — which is the
/// ordinary case, not the pathological one — so the latest binding wins and
/// there is no ambiguous state to reach.
#[derive(Debug, Default)]
struct ThreadTable {
    per_principal: HashMap<Principal, PrincipalThreads>,
}

/// One principal's remembered threads, oldest-first.
#[derive(Debug, Default)]
struct PrincipalThreads {
    sessions: HashMap<String, SessionId>,
    /// Insertion order of [`PrincipalThreads::sessions`], so the cap evicts
    /// the oldest. Exactly one entry per thread, which is this type's
    /// invariant — a rebinding must not push a second one, or the cap drops a
    /// key that is still live.
    order: VecDeque<String>,
}

impl ThreadTable {
    /// How many client threads this node remembers the session of, per
    /// principal.
    ///
    /// A cap for [`CallTable::REMEMBERED_CALLS`]' reason — this is written on
    /// every turn a header rides, so uncapped it is a leak proportional to
    /// traffic — but an order of magnitude smaller, because the thing counted
    /// is different: a tool loop emits many calls per conversation, where a
    /// client has one thread id per conversation and rebinds it in place. What
    /// losing an entry costs is one MCP call falling back to the R-M7 named
    /// path and then to `latest`, which is the answer it got before this table
    /// existed.
    const REMEMBERED_THREADS: usize = 1024;

    fn bind(&mut self, principal: &Principal, thread_id: &str, session: SessionId) {
        self.per_principal
            .entry(principal.clone())
            .or_default()
            .bind(thread_id, session);
    }

    fn session_of(&self, principal: &Principal, thread_id: &str) -> Option<SessionId> {
        self.per_principal
            .get(principal)?
            .sessions
            .get(thread_id)
            .cloned()
    }

    /// How many bindings this table holds for `principal`, and how long its
    /// eviction queue is. See [`CallTable::sizes`] for why this is one call on
    /// the owning type rather than two accessors reached through the lock.
    #[cfg(test)]
    fn sizes(&self, principal: &Principal) -> (usize, usize) {
        self.per_principal
            .get(principal)
            .map(|threads| (threads.sessions.len(), threads.order.len()))
            .unwrap_or_default()
    }
}

impl PrincipalThreads {
    fn bind(&mut self, thread_id: &str, session: SessionId) {
        if let Some(held) = self.sessions.get_mut(thread_id) {
            // The fork case, and the resend case, and they are the same
            // write: this thread's newest turn decided this session.
            *held = session;
            return;
        }
        self.sessions.insert(thread_id.to_string(), session);
        self.order.push_back(thread_id.to_string());
        while self.order.len() > ThreadTable::REMEMBERED_THREADS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.sessions.remove(&oldest);
        }
    }
}

impl Conversations {
    pub fn new() -> Self {
        Self::default()
    }

    /// The session `key` names now, and a note that `principal` is using it.
    ///
    /// Called by the surface that actually serves turns. The `latest` half is
    /// recorded here rather than at the end of the turn because it answers
    /// "which conversation is this agent working in", and an agent that opened a
    /// turn is working in it whether or not the turn went on to succeed.
    pub fn bind(&self, principal: &Principal, key: &str) -> SessionId {
        let mut inner = self.lock();
        // Written rather than read-with-a-default, because the entry's
        // *presence* is what tells a reader this node bound the key at all
        // (M12.1 review, F9). A `get(..).unwrap_or(0)` here would leave every
        // never-forked conversation indistinguishable — to `resolve`, on the
        // very node serving its turns — from a key this process has never
        // heard of. The cost is one entry per distinct cache key served
        // instead of one per key that forked; both are the process state M8's
        // durable mapping replaces.
        let generation = *inner.generations.entry(key.to_string()).or_insert(0);
        let session = bound_session(key, generation);
        inner.latest.insert(principal.clone(), session.clone());
        session
    }

    /// Rebind `key` to a fresh session, because the client's history disagreed
    /// with the log.
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
    pub fn fork(&self, principal: &Principal, key: &str) -> SessionId {
        let mut inner = self.lock();
        let generation = inner.generations.entry(key.to_string()).or_insert(0);
        *generation += 1;
        let session = bound_session(key, *generation);
        inner.latest.insert(principal.clone(), session.clone());
        session
    }

    /// The session `key` names now, without claiming to be using it, or `None`
    /// for a key this node holds no binding for.
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
    /// created it, and answering with one on a node that bound nothing hands
    /// the caller a superseded log that another node has already forked away
    /// from — quietly, with a 200 on it. A reader that does not know refuses;
    /// see the module doc.
    pub fn resolve(&self, key: &str) -> Option<SessionId> {
        let inner = self.lock();
        let generation = *inner.generations.get(key)?;
        Some(bound_session(key, generation))
    }

    /// The last session this principal drove a turn on, on this node.
    pub fn latest(&self, principal: &Principal) -> Option<SessionId> {
        self.lock().latest.get(principal).cloned()
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
    /// [`CallTable`] for why an id is not a name on every backend.
    pub fn bind_call(&self, principal: &Principal, call_id: &str, session: SessionId) {
        self.lock().calls.bind(principal, call_id, session);
    }

    /// The session that emitted `call_id`, if this node emitted it *for this
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
    pub fn session_of_call(&self, principal: &Principal, call_id: &str) -> Option<SessionId> {
        self.lock().calls.session_of(principal, call_id)
    }

    /// Remember that `principal`'s thread `thread_id` is in `session`.
    ///
    /// **Written by the turn ingest, from the client's own header**, because
    /// that is the one moment both halves are in one place: the surface has
    /// just decided which session this turn's history belongs to, and the
    /// request that carried the history also carried the thread it came from.
    /// Nothing later can reconstruct the pairing — the thread id is not in the
    /// log, and the cache key that is in it is the whole agent family's rather
    /// than this thread's (see [`ThreadTable`]).
    ///
    /// Rebinding is ordinary and the latest write wins: a fork moves a thread
    /// to a new session, and the thread is then in the new one.
    ///
    /// Deliberately *not* a claim about [`Self::latest`], for
    /// [`Self::bind_call`]'s reason turned around: the ingest that calls this
    /// has already moved `latest` for the turn it is serving, and moving it a
    /// second time from a subagent's header is how the parent's next unnamed
    /// call gets redirected.
    pub fn bind_thread(&self, principal: &Principal, thread_id: &str, session: SessionId) {
        self.lock().threads.bind(principal, thread_id, session);
    }

    /// The session `principal`'s thread `thread_id` is in, if this node served
    /// a turn of it.
    ///
    /// A foreign thread id and an unknown one answer alike, for the reason
    /// [`Self::session_of_call`] spells out at length: the caller has no use
    /// for another tenant's session, so distinguishing the two would buy an
    /// enumeration oracle and nothing else.
    pub fn session_of_thread(&self, principal: &Principal, thread_id: &str) -> Option<SessionId> {
        self.lock().threads.session_of(principal, thread_id)
    }

    /// The lock, in one place.
    ///
    /// Recovering a poisoned guard rather than propagating the panic: every
    /// entry here is a binding that re-derives — a lost generation is one cold
    /// prefix, a lost `latest` is one MCP call that has to name its
    /// conversation — and failing every later request over one poisoned map is
    /// a worse outcome than serving the next one from possibly-stale state.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// This node's session id for a namespaced cache key at a given generation.
///
/// Generation zero is the key verbatim, so a session survives a process
/// restart that loses the generation map: the common case is a conversation
/// that never forked, and it re-binds to the same log.
fn bound_session(key: &str, generation: u32) -> SessionId {
    match generation {
        0 => SessionId::new(key),
        n => SessionId::new(format!("{key}#g{n}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ada() -> Principal {
        Principal::new("acme", "ada")
    }

    fn bob() -> Principal {
        Principal::new("globex", "bob")
    }

    #[test]
    fn a_reader_and_a_turn_resolve_one_cache_key_to_one_session() {
        // The whole reason this table is shared rather than owned by the
        // Responses surface: an overlay installed against `resolve`'s answer
        // has to reach the session `bind` hands the engine, generation and all.
        let conversations = Conversations::new();
        let key = "acme/ada/main";

        // F9 (M12.1 review): before a turn has bound it, the key names nothing
        // *here* — not generation zero, which is a real session id in the
        // shared store the moment any node mints it.
        assert_eq!(
            conversations.resolve(key),
            None,
            "a reader on a node that has bound nothing must say so rather \
             than mint the id another node's first turn would have minted"
        );

        assert_eq!(
            Some(conversations.bind(&ada(), key)),
            conversations.resolve(key)
        );

        let forked = conversations.fork(&ada(), key);
        assert_eq!(forked.as_str(), "acme/ada/main#g1");
        assert_eq!(
            conversations.resolve(key),
            Some(forked.clone()),
            "a rebound key stays rebound: a reader that kept answering \
             generation zero would narrow a session no turn will run in"
        );
        assert_eq!(conversations.bind(&ada(), key), forked);
    }

    /// R-M2 (M12): a tool-use id names one session exactly, and only for the
    /// principal it was emitted for.
    ///
    /// The three cases that matter are one test because they are one rule: a
    /// binding answers its own caller, and answers *nothing* — indistinguishably
    /// — to anyone else or for any id this node never emitted.
    #[test]
    fn an_emitted_tool_call_names_its_session_and_only_for_the_caller_it_was_emitted_for() {
        let conversations = Conversations::new();
        let subagent = conversations.bind(&ada(), "acme/ada/sub");
        let parent = conversations.bind(&ada(), "acme/ada/main");
        conversations.bind_call(&ada(), "toolu_sub", subagent.clone());

        assert_eq!(
            conversations.session_of_call(&ada(), "toolu_sub"),
            Some(subagent),
            "the session that emitted the call is the session the answer to it \
             concerns, whatever else the principal has been doing since"
        );
        assert_eq!(
            conversations.session_of_call(&bob(), "toolu_sub"),
            None,
            "another tenant presenting the id learns nothing from it"
        );
        assert_eq!(
            conversations.session_of_call(&ada(), "toolu_never_emitted"),
            None,
            "and an id this node never emitted answers exactly as a foreign \
             one does"
        );

        // Binding a call is not a claim that the principal is now working in
        // that conversation — the very race this exists to remove would come
        // straight back if a subagent's tool call moved its parent's `latest`.
        assert_eq!(conversations.latest(&ada()), Some(parent));
    }

    /// The remembered-calls cap evicts oldest-first, and the fallback is what
    /// an evicted binding costs.
    ///
    /// Asserted because the failure it prevents is not a wrong answer but an
    /// unbounded map: every tool call of every turn lands here, so a cap that
    /// silently stopped evicting would be a leak proportional to traffic, and
    /// nothing else in the process would notice.
    #[test]
    fn the_call_table_is_capped_and_forgets_its_oldest_bindings_first() {
        let conversations = Conversations::new();
        let session = conversations.bind(&ada(), "acme/ada/main");
        for n in 0..=CallTable::REMEMBERED_CALLS {
            conversations.bind_call(&ada(), &format!("toolu_{n}"), session.clone());
        }

        assert_eq!(
            conversations.session_of_call(&ada(), "toolu_0"),
            None,
            "the oldest binding is the one the cap gives up"
        );
        assert_eq!(
            conversations
                .session_of_call(&ada(), &format!("toolu_{}", CallTable::REMEMBERED_CALLS)),
            Some(session),
            "and the newest is kept, which is the one a live tool loop is \
             about to answer"
        );
        assert_eq!(
            conversations.lock().calls.sizes(&ada()),
            (CallTable::REMEMBERED_CALLS, CallTable::REMEMBERED_CALLS)
        );

        // Re-binding an id already held must not grow the order queue past the
        // map, or the cap evicts a key that is still live and the two halves
        // drift apart. Asserted on `CallTable`'s own accessor rather than by
        // reading `Inner`'s fields through the lock: the invariant belongs to
        // the type that keeps it (F13).
        let other = conversations.bind(&ada(), "acme/ada/other");
        conversations.bind_call(&ada(), "toolu_1", other);
        let (held, ordered) = conversations.lock().calls.sizes(&ada());
        assert_eq!(ordered, held);
    }

    /// F14: a colliding call id from two sessions of one principal must not
    /// resolve confidently to whichever session bound it last.
    ///
    /// A frontier backend's tool-call ids are globally unique, so this never
    /// happens on the routes M12 was built against. A local/Dynamo backend
    /// that numbers calls per response (`call_0`, `call_1`, ...) can hand the
    /// same id to two concurrent conversations of one principal, and
    /// `bind_call`'s plain `insert` makes the second binding silently replace
    /// the first — with no record that the id was ever ambiguous.
    #[test]
    fn a_colliding_call_id_from_two_sessions_of_one_principal_does_not_resolve_confidently() {
        let conversations = Conversations::new();
        let first = conversations.bind(&ada(), "acme/ada/first");
        let second = conversations.bind(&ada(), "acme/ada/second");

        conversations.bind_call(&ada(), "call_0", first.clone());
        conversations.bind_call(&ada(), "call_0", second);

        assert_eq!(
            conversations.session_of_call(&ada(), "call_0"),
            None,
            "an id this node has bound to two different sessions of one \
             principal no longer names either unambiguously, so it must \
             answer exactly as an unknown id does — a fall back to latest — \
             rather than confidently resolving to the second conversation's \
             session while the first conversation's tools/call is still \
             answering it"
        );
    }

    #[test]
    fn reading_a_conversation_does_not_make_it_the_principals_most_recent_one() {
        let conversations = Conversations::new();
        assert_eq!(
            conversations.latest(&ada()),
            None,
            "a principal this node has served no turn for has no most-recent \
             conversation, which is an answer rather than a default"
        );

        conversations.bind(&ada(), "acme/ada/main");
        assert_eq!(
            conversations.resolve("acme/ada/other"),
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
        conversations.bind(&ada(), "acme/ada/other");
        assert_eq!(
            conversations.latest(&ada()).unwrap().as_str(),
            "acme/ada/other"
        );
        // And one principal's turns are not another's.
        assert_eq!(conversations.latest(&bob()), None);
    }

    /// F15: the remembered-calls cap is per principal, so a co-tenant's tool
    /// traffic cannot evict a *different* principal's binding — which is the
    /// half `the_call_table_is_capped_and_forgets_its_oldest_bindings_first`
    /// above does not cover, that one being the control that a tenant still
    /// ages out its *own* oldest entry.
    ///
    /// Bind one call for Ada's subagent, then drive
    /// `CallTable::REMEMBERED_CALLS` insertions for Bob alone. Under one
    /// node-wide queue Ada's binding is its oldest entry and is gone, and
    /// `session_of_call` falls through to `None` — indistinguishable from Ada
    /// presenting an id this node never emitted at all, on a table that had
    /// room for it.
    #[test]
    fn a_co_tenants_call_traffic_does_not_evict_another_principals_call_binding() {
        let conversations = Conversations::new();
        let subagent = conversations.bind(&ada(), "acme/ada/sub");
        conversations.bind_call(&ada(), "toolu_ada_sub", subagent.clone());

        let bob_session = conversations.bind(&bob(), "globex/bob/main");
        for n in 0..CallTable::REMEMBERED_CALLS {
            conversations.bind_call(&bob(), &format!("toolu_bob_{n}"), bob_session.clone());
        }

        assert_eq!(
            conversations.session_of_call(&ada(), "toolu_ada_sub"),
            Some(subagent),
            "a principal's own call binding must survive another tenant's \
             tool traffic; a node-wide cap makes it fall through to the same \
             None a foreign id would answer with"
        );
    }

    /// The control F14's ruling turns on: only a *different* session claiming a
    /// held id makes it ambiguous.
    ///
    /// Without this, the cheapest way to satisfy F14 — treat every re-bind as a
    /// collision — would silently un-answer every id a resend or a dedup replay
    /// binds twice, which is the ordinary case rather than the pathological
    /// one. The queue length is asserted for the same reason it is in the cap
    /// test: a repeat that pushed a second entry would evict a live binding.
    #[test]
    fn re_binding_one_id_to_the_session_that_already_holds_it_changes_nothing() {
        let conversations = Conversations::new();
        let session = conversations.bind(&ada(), "acme/ada/main");

        conversations.bind_call(&ada(), "toolu_replayed", session.clone());
        conversations.bind_call(&ada(), "toolu_replayed", session.clone());

        assert_eq!(
            conversations.session_of_call(&ada(), "toolu_replayed"),
            Some(session),
            "one call seen twice is one call, and the binding it already had \
             is still exactly right"
        );
        assert_eq!(conversations.lock().calls.sizes(&ada()), (1, 1));
    }

    /// R-M9 (M12.1 review, F2): a thread is in the session its own latest turn
    /// decided, and the thread's family sharing one cache key does not change
    /// that.
    ///
    /// The topology is the oracle's: parent and subagent send one
    /// `prompt_cache_key` and two `thread_id`s, so the cache key forks under
    /// them while each thread stays pinned to the fork its own turn produced.
    #[test]
    fn a_thread_is_in_the_session_its_own_latest_turn_decided() {
        let conversations = Conversations::new();
        let key = "acme/ada/main";

        let parent_g0 = conversations.bind(&ada(), key);
        conversations.bind_thread(&ada(), "thread-parent", parent_g0.clone());
        let child_g1 = conversations.fork(&ada(), key);
        conversations.bind_thread(&ada(), "thread-child", child_g1.clone());
        let parent_g2 = conversations.fork(&ada(), key);
        conversations.bind_thread(&ada(), "thread-parent", parent_g2.clone());

        assert_eq!(
            conversations.session_of_thread(&ada(), "thread-child"),
            Some(child_g1),
            "the subagent's thread stays in the fork its own turn produced, \
             however far the shared cache key has moved since — this is the \
             whole of F2"
        );
        assert_eq!(
            conversations.session_of_thread(&ada(), "thread-parent"),
            Some(parent_g2.clone()),
            "and a thread that forked is in the session it forked *to*: the \
             latest binding wins, where a colliding call id is refused"
        );
        assert_eq!(
            conversations.session_of_thread(&bob(), "thread-child"),
            None,
            "another tenant presenting the id learns nothing from it"
        );
        assert_eq!(
            conversations.session_of_thread(&ada(), "thread-never-seen"),
            None,
            "and a thread this node never served answers exactly as a foreign \
             one does"
        );

        // Binding a thread is not a claim about who is working where: the
        // ingest has already moved `latest` for the turn it is serving.
        assert_eq!(conversations.latest(&ada()), Some(parent_g2));
    }

    /// The thread cap evicts oldest-first, and a rebinding does not spend a
    /// queue slot.
    ///
    /// The second half is the one with teeth: rebinding is the *ordinary* case
    /// here (every fork rebinds), so a `bind` that pushed a second order entry
    /// would evict live threads at a rate set by how often clients compact.
    #[test]
    fn the_thread_table_is_capped_and_a_rebinding_does_not_grow_its_queue() {
        let conversations = Conversations::new();
        let session = conversations.bind(&ada(), "acme/ada/main");
        for n in 0..=ThreadTable::REMEMBERED_THREADS {
            conversations.bind_thread(&ada(), &format!("thread-{n}"), session.clone());
        }

        assert_eq!(
            conversations.session_of_thread(&ada(), "thread-0"),
            None,
            "the oldest binding is the one the cap gives up"
        );
        assert_eq!(
            conversations.session_of_thread(
                &ada(),
                &format!("thread-{}", ThreadTable::REMEMBERED_THREADS)
            ),
            Some(session),
            "and the newest is kept, which is the thread a live tool loop is \
             about to answer"
        );
        assert_eq!(
            conversations.lock().threads.sizes(&ada()),
            (
                ThreadTable::REMEMBERED_THREADS,
                ThreadTable::REMEMBERED_THREADS
            )
        );

        let forked = conversations.fork(&ada(), "acme/ada/main");
        conversations.bind_thread(&ada(), "thread-1", forked.clone());
        let (held, ordered) = conversations.lock().threads.sizes(&ada());
        assert_eq!(ordered, held);
        assert_eq!(
            conversations.session_of_thread(&ada(), "thread-1"),
            Some(forked)
        );
    }
}
