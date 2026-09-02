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
//!   knowable exactly, with no guess and no race. See
//!   [`Conversations::bind_call`].
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
//! call that omits `conversation` on a node that has served none of this
//! principal's turns is refused as [`SurfaceError::NoSession`] rather than
//! guessing. Both are refusals or re-derivations, never a wrong session served
//! quietly.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use roundhouse_core::control::Principal;
use roundhouse_core::ids::SessionId;

/// How many emitted tool calls this node remembers the session of.
///
/// A cap and not a policy. Every tool call a dispatched turn emits lands here,
/// so an uncapped map is a leak proportional to traffic — unlike `latest`,
/// which is bounded by the number of principals. What losing an entry costs is
/// exactly one MCP call falling back to the principal's most recent
/// conversation, which is the answer it got before R-M2 existed; the window
/// that matters is a single turn's tool loop, and four thousand calls is far
/// more than any turn emits.
const REMEMBERED_CALLS: usize = 4096;

/// This node's binding from a client's names to the sessions holding them.
#[derive(Debug, Default)]
pub struct Conversations {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// How many times each *namespaced* cache key's history has failed the
    /// prefix check.
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
    /// Which session emitted each tool call this node has served, and to whom.
    ///
    /// The principal is stored beside the session rather than folded into the
    /// key because it is a *check*, not part of the name: a tool-use id is
    /// unique on its own, and keying by `(Principal, id)` would make a lookup
    /// with the wrong principal read as "never seen" — which is the same answer
    /// as an evicted binding and would hide the thing worth noticing.
    calls: HashMap<String, CallSite>,
    /// Insertion order of [`Inner::calls`], so the cap evicts the oldest.
    call_order: VecDeque<String>,
}

/// Where one emitted tool call came from.
#[derive(Debug, Clone)]
struct CallSite {
    principal: Principal,
    session: SessionId,
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
        let generation = inner.generations.get(key).copied().unwrap_or(0);
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

    /// The session `key` names now, without claiming to be using it.
    ///
    /// What a *reader* asks — the MCP surface resolving an explicit
    /// `conversation` argument. Distinct from [`Self::bind`] because an agent
    /// asking `status` about a conversation must not thereby make that
    /// conversation its most recent one: the two tools that take the argument
    /// and the tool that omits it would then disagree about what "most recent"
    /// means, in an order the agent chose.
    ///
    /// Generation zero for a key this node has never bound, which is the same
    /// answer [`Self::bind`] would give and the reason a restart is survivable:
    /// the common case is a conversation that never forked, and it re-binds to
    /// the same log.
    pub fn resolve(&self, key: &str) -> SessionId {
        let inner = self.lock();
        let generation = inner.generations.get(key).copied().unwrap_or(0);
        bound_session(key, generation)
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
    pub fn bind_call(&self, principal: &Principal, call_id: &str, session: SessionId) {
        let mut inner = self.lock();
        if inner
            .calls
            .insert(
                call_id.to_string(),
                CallSite {
                    principal: principal.clone(),
                    session,
                },
            )
            .is_none()
        {
            inner.call_order.push_back(call_id.to_string());
        }
        while inner.call_order.len() > REMEMBERED_CALLS {
            let Some(oldest) = inner.call_order.pop_front() else {
                break;
            };
            inner.calls.remove(&oldest);
        }
    }

    /// The session that emitted `call_id`, if this node emitted it *for this
    /// principal*.
    ///
    /// **A foreign id and an unknown one answer the same way, and that is the
    /// decision.** Returning something distinguishable for "this id exists but
    /// is somebody else's" would make the argument an enumeration oracle — the
    /// reasoning `mcp_api::resolve_session` already collapses a foreign cache
    /// key under — and it would buy nothing: the caller has no use for another
    /// tenant's session, so both answers lead to the same next step, which is
    /// to fall back to the caller's own most recent conversation.
    pub fn session_of_call(&self, principal: &Principal, call_id: &str) -> Option<SessionId> {
        self.lock()
            .calls
            .get(call_id)
            .filter(|site| &site.principal == principal)
            .map(|site| site.session.clone())
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

        assert_eq!(conversations.bind(&ada(), key), conversations.resolve(key));

        let forked = conversations.fork(&ada(), key);
        assert_eq!(forked.as_str(), "acme/ada/main#g1");
        assert_eq!(
            conversations.resolve(key),
            forked,
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
        for n in 0..=REMEMBERED_CALLS {
            conversations.bind_call(&ada(), &format!("toolu_{n}"), session.clone());
        }

        assert_eq!(
            conversations.session_of_call(&ada(), "toolu_0"),
            None,
            "the oldest binding is the one the cap gives up"
        );
        assert_eq!(
            conversations.session_of_call(&ada(), &format!("toolu_{REMEMBERED_CALLS}")),
            Some(session),
            "and the newest is kept, which is the one a live tool loop is \
             about to answer"
        );
        assert_eq!(conversations.lock().call_order.len(), REMEMBERED_CALLS);

        // Re-binding an id already held must not grow the order queue past the
        // map, or the cap evicts a key that is still live and the two halves
        // drift apart.
        let other = conversations.bind(&ada(), "acme/ada/other");
        conversations.bind_call(&ada(), "toolu_1", other);
        // Two reads of one lock, taken one at a time: the guards are temporaries
        // that live to the end of the statement, and `Mutex` is not reentrant —
        // a single `assert_eq!` over both would hang here rather than fail.
        let ordered = conversations.lock().call_order.len();
        let held = conversations.lock().calls.len();
        assert_eq!(ordered, held);
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
        conversations.resolve("acme/ada/other");
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
}
