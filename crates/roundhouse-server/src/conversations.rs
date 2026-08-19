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
//! # Two questions, one table, and why they belong together
//!
//! A client names a conversation in one of two ways, and both are answered from
//! the same node-local state:
//!
//! - **By cache key.** `{project}/{user}/{key}` at generation zero, plus a
//!   `#g{n}` suffix once a client has edited its own history out from under a
//!   session. Only this table knows what `n` is.
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

use std::collections::HashMap;
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
