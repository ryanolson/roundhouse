// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which session a client's own name for a conversation belongs to, for every
//! node of the deployment.
//!
//! Three maps, and they are the minimum set D1's R12 named as closing the
//! M12.1 handoffs:
//!
//! - **generations**: the generation a namespaced cache key was last committed
//!   at, keyed by the whole `{project}/{user}/{key}` string because that string
//!   is also the session id's stem;
//! - **calls**: `(principal, tool_use_id)` → the session that emitted the call,
//!   or *ambiguous* where two of one principal's sessions have claimed the id;
//! - **threads**: `(principal, thread_id)` → the session that thread's latest
//!   turn decided.
//!
//! They were three tables inside `roundhouse-server`'s `Conversations` until
//! this rung, and the properties written there as decisions are the contract
//! here: partitioned by principal, an ambiguous call remembered rather than
//! forgotten, a thread rebinding where a call collides.
//!
//! # R-C1 — one trait, two implementations, one contract
//!
//! [`MemoryCorrelationMaps`] is the specification and `RedisCorrelationMaps`
//! in `roundhouse-store-redis` is proven against it, exactly as the spend and
//! fair-use ledgers one seam over are: `contract` (built only under `test` or
//! `test-support`, so it cannot doc-link from here) holds the behavioural
//! assertions as one list, and `correlation_maps_contract_suite!`
//! instantiates the whole list against a backend in one call. The alternative
//! — each backend keeping its own unit tests — is what the fair-use ledger
//! already rejected for a reason that applies with more force here: the two
//! implementations are written in two languages, and "an ambiguous call is
//! remembered" is a claim about *both* or it is not a claim at all.
//!
//! # R-C2 — the counter is a hint, the store is the truth, the node caches
//!
//! **The generation map needs no atomicity, and that is a fact about M14.0
//! rather than an optimism.** Since prefix admission became probe-then-commit,
//! a generation is where a *search* starts, not the answer it returns: two
//! nodes committing different generations for one key each leave a value the
//! other's next probe begins from, and the probe walks up or down from there
//! to the generation that actually agrees with the claim. So `set_generation`
//! is a plain write. The alternative — a compare-and-set script, or an
//! `INCR` — would have bought an ordering nothing reads, at the price of a
//! round trip on the commit of every turn and of a counter that can only go
//! up, which is precisely what M14.0 removed when it let the search walk
//! backwards to an older generation the claim still continues.
//!
//! **Absence is an answer.** `generation` returns `None` for a key nothing has
//! ever committed, and that is what widens M12.1's F9 refusal from "never
//! bound on this node" to "never bound anywhere": a reader that mints
//! generation zero for an unknown key hands back a log some other node may
//! already have forked away from, with a 200 on it. Prefix admission still
//! *starts* an unknown key's search at zero — that is the server's
//! `Conversations` to choose, not this map's — but it starts it knowing the
//! store said nothing rather than being told a number.
//!
//! # R-C3 — call and thread bindings are keys with a lifetime
//!
//! Both binding families are written where the memory tables are written
//! today: a call at the moment it is streamed to the client, a thread at the
//! moment the turn ingest decides which session the thread's history belongs
//! to. Nothing later can reconstruct either pairing.
//!
//! What bounds them differs by backend *by design*, and the contract asserts
//! the semantics both share rather than the bound each uses. In memory the
//! bound is a capacity cap per principal ([`REMEMBERED_CALLS`],
//! [`REMEMBERED_THREADS`]); in Redis it is a TTL per key, because a shared
//! store has no natural place to keep an eviction queue and because a binding
//! older than any plausible turn is a stale guess whatever a table's size —
//! D1's R14, brought forward here because the durable shape needs a bound and
//! a TTL is the one Redis already owns. The memory tables gain the same
//! staleness bound under M14.2; asserting a *cap* in the contract would have
//! made a backend that expires by time fail a test about a table it does not
//! have.
//!
//! # What is deliberately not here
//!
//! `latest` — the principal's most recent conversation — stays node-local and
//! a guess by contract (R12). It is answerable only by having watched turns go
//! past, and durability would make it more *confident* without making it more
//! correct: two nodes serving one agent would each write their own answer, and
//! whichever wrote last would speak for both.

#[cfg(any(test, feature = "test-support"))]
pub mod contract;

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::control::Principal;
use crate::ids::SessionId;

/// How many emitted tool calls one principal's in-memory table remembers.
///
/// A cap and not a policy. Every tool call a dispatched turn emits lands here,
/// so an uncapped map is a leak proportional to traffic — unlike a map of
/// principals, which is bounded by the tenant count. What losing an entry
/// costs is exactly one MCP call falling back to the principal's most recent
/// conversation, which is the answer it got before R-M2 existed; the window
/// that matters is a single turn's tool loop, and four thousand calls is far
/// more than any turn emits.
///
/// Per principal, so the node's worst case is this times the number of
/// principals it has served rather than this outright. That is the same factor
/// a per-principal `latest` already carries, and it is the right trade: a
/// tenant count is something an operator knows and provisions for, where
/// "whose traffic happened to arrive first" is not.
pub const REMEMBERED_CALLS: usize = 4096;

/// How many client threads one principal's in-memory table remembers.
///
/// A cap for [`REMEMBERED_CALLS`]' reason — this is written on every turn a
/// thread header rides, so uncapped it is a leak proportional to traffic — but
/// an order of magnitude smaller, because the thing counted is different: a
/// tool loop emits many calls per conversation, where a client has one thread
/// id per conversation and rebinds it in place.
pub const REMEMBERED_THREADS: usize = 1024;

/// What a correlation map can fail with.
///
/// **One arm, and the absence of a second is the decision.** There is no
/// "not found": a key nothing has bound, a call this deployment never emitted
/// and a thread nobody declared are all `Ok(None)`, because a caller can do
/// nothing different with any of them — the next step is the same fallback in
/// every case, and distinguishing them on a *shared* store would make the
/// argument an enumeration oracle across tenants. What is left is the one
/// thing a caller genuinely must be able to see: the store could not be
/// reached, said as a typed reason rather than swallowed into a `None` that
/// reads exactly like "never bound anywhere" (R14).
#[derive(Debug, thiserror::Error)]
pub enum CorrelationError {
    #[error("correlation store failure: {0}")]
    Backend(#[from] anyhow::Error),
}

/// The three maps that turn a client's own name for a conversation into the
/// session holding it.
///
/// One trait rather than three, because the three are written and read at the
/// same three moments by the same two surfaces, and a deployment that made
/// them durable one at a time would be a deployment where a call resolves
/// across nodes and the generation it resolves *to* does not.
#[async_trait]
pub trait CorrelationMaps: Send + Sync + 'static {
    /// The generation `key` was last committed at, or `None` if no node ever
    /// committed one.
    ///
    /// `key` is the whole namespaced cache key — `{project}/{user}/{cache_key}`
    /// where there is a namespace, the bare cache key where there is not —
    /// rather than a `(Principal, key)` pair, because that same string is the
    /// session id's stem: keying the counter on anything else would let the
    /// counter and the id it names disagree.
    async fn generation(&self, key: &str) -> Result<Option<u32>, CorrelationError>;

    /// Record that a turn on `key` landed on `generation`.
    ///
    /// *Set*, never advanced: the search that chose `generation` may have
    /// walked backwards to an older generation the claim still continues. See
    /// the module doc for why this needs no atomicity.
    async fn set_generation(&self, key: &str, generation: u32) -> Result<(), CorrelationError>;

    /// Remember that `session` emitted the tool call `call_id`, for
    /// `principal`.
    ///
    /// A second binding of one id to a *different* session of the same
    /// principal makes the id ambiguous, and it stays ambiguous: an id dropped
    /// instead would read as never-seen, so the next binding of the same
    /// colliding id would look like a first one and start answering
    /// confidently again — the defect, one turn later. Re-binding an id to the
    /// session that already holds it is a resend or a dedup replay, which is
    /// one call seen twice rather than two calls, and changes nothing.
    async fn bind_call(
        &self,
        principal: &Principal,
        call_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError>;

    /// The session that emitted `call_id` for `principal`, if one did
    /// unambiguously.
    ///
    /// A foreign id, an unknown one and an ambiguous one all answer alike —
    /// see [`CorrelationError`] for why that is the decision rather than the
    /// shortcut.
    async fn session_of_call(
        &self,
        principal: &Principal,
        call_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError>;

    /// Remember that `principal`'s thread `thread_id` is in `session`.
    ///
    /// Rebinding is the ordinary case and the latest write wins: a thread id
    /// names a *conversation*, and a conversation legitimately moves — every
    /// fork mints a new session for the same thread, and the turn that forked
    /// is the one the client is in. Remembering an ambiguous state here would
    /// un-answer every thread the moment its client compacted.
    async fn bind_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError>;

    /// The session `principal`'s thread `thread_id` is in, if any node served
    /// a turn of it.
    async fn session_of_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError>;
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

/// The three maps in this process's memory: the specification, and the whole
/// of a deployment that has not named a Redis.
///
/// **Its methods are inherent as well as trait methods, and the inherent ones
/// are not `async`.** A `HashMap` behind a `Mutex` has nothing to await; the
/// trait is `async` because the *other* implementation is. A caller holding
/// the concrete type — `Conversations`, whose surface is synchronous and whose
/// callers this rung deliberately does not touch — therefore pays no runtime
/// for a seam it is not crossing, and the [`CorrelationMaps`] impl below is
/// one delegating line per method, which is too thin to drift from what the
/// contract judges.
#[derive(Debug, Default)]
pub struct MemoryCorrelationMaps {
    inner: Mutex<Tables>,
}

#[derive(Debug, Default)]
struct Tables {
    /// How many times each *namespaced* cache key's history has failed the
    /// prefix check, for every key this node has committed.
    ///
    /// **Presence is load-bearing and not merely a counter's storage** (M12.1
    /// review, F9): an entry means "this key has been committed", which is the
    /// question a reader refuses on. Uncapped, unlike its two siblings, and
    /// deliberately: this is the one family whose loss is not a fallback but a
    /// wrong answer, and it is bounded by the number of distinct conversations
    /// a node has served rather than by their tool traffic.
    generations: HashMap<String, u32>,
    calls: CallTable,
    threads: ThreadTable,
}

/// Which session emitted each tool call, remembered per principal and bounded
/// per principal.
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
/// a session it never asked about (M12 review, F14). Separately, one node-wide
/// eviction queue spends a quiet tenant's remembered calls on a busy
/// co-tenant's traffic, costing it exactly the exact answer R-M2 exists to
/// give (F15).
#[derive(Debug, Default)]
struct CallTable {
    per_principal: HashMap<Principal, PrincipalCalls>,
}

/// One principal's remembered calls, oldest-first.
#[derive(Debug, Default)]
struct PrincipalCalls {
    sites: HashMap<String, CallSite>,
    /// Insertion order of [`PrincipalCalls::sites`], so the cap evicts the
    /// oldest. Exactly one entry per site, which is this type's invariant —
    /// kept here rather than inline in one method's body because an invariant
    /// like that is one the next edit breaks with nothing red (M12 review,
    /// F13).
    order: VecDeque<String>,
}

/// What one remembered call id names, if anything.
#[derive(Debug, Clone)]
enum CallSite {
    /// The single session that emitted it.
    Bound(SessionId),
    /// Two of this principal's sessions bound it, so it names neither.
    Ambiguous,
}

impl CallTable {
    fn bind(&mut self, principal: &Principal, call_id: &str, session: &SessionId) {
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
    /// the invariant on the type that owns it instead of reaching through a
    /// lock into private fields.
    #[cfg(test)]
    fn sizes(&self, principal: &Principal) -> (usize, usize) {
        self.per_principal
            .get(principal)
            .map(|calls| (calls.sites.len(), calls.order.len()))
            .unwrap_or_default()
    }
}

impl PrincipalCalls {
    fn bind(&mut self, call_id: &str, session: &SessionId) {
        match self.sites.get_mut(call_id) {
            // A resend or a dedup replay re-binds an id this node already
            // holds to the session that already holds it. That is one call
            // seen twice, not two calls, and treating it as a collision would
            // throw away a binding that is still exactly right.
            Some(CallSite::Bound(held)) if held == session => {}
            Some(site) => *site = CallSite::Ambiguous,
            None => {
                self.sites
                    .insert(call_id.to_string(), CallSite::Bound(session.clone()));
                self.order.push_back(call_id.to_string());
            }
        }
        while self.order.len() > REMEMBERED_CALLS {
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
/// thread: a non-root agent takes its `session_id` — and therefore its
/// `prompt_cache_key` — from the shared `AgentControl`, while `_meta.threadId`
/// is that agent's *own* `thread_id`. So the whole family names one cache key
/// and each member names a different thread, and resolving a thread id as a
/// cache key finds nothing for exactly the callers R-M7 existed to serve
/// (M12.1 review, F2).
///
/// # Why rebinding is the normal case here, where [`CallTable`] calls it a
/// collision
///
/// A tool-call id names one emission for ever, so a second session claiming
/// one is two callers claiming one name and neither may be answered. A thread
/// id names a conversation, and a conversation legitimately moves.
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
    fn bind(&mut self, principal: &Principal, thread_id: &str, session: &SessionId) {
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
    /// eviction queue is. See [`CallTable::sizes`].
    #[cfg(test)]
    fn sizes(&self, principal: &Principal) -> (usize, usize) {
        self.per_principal
            .get(principal)
            .map(|threads| (threads.sessions.len(), threads.order.len()))
            .unwrap_or_default()
    }
}

impl PrincipalThreads {
    fn bind(&mut self, thread_id: &str, session: &SessionId) {
        if let Some(held) = self.sessions.get_mut(thread_id) {
            // The fork case, and the resend case, and they are the same
            // write: this thread's newest turn decided this session.
            *held = session.clone();
            return;
        }
        self.sessions.insert(thread_id.to_string(), session.clone());
        self.order.push_back(thread_id.to_string());
        while self.order.len() > REMEMBERED_THREADS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.sessions.remove(&oldest);
        }
    }
}

impl MemoryCorrelationMaps {
    pub fn new() -> Self {
        Self::default()
    }

    /// See [`CorrelationMaps::generation`].
    pub fn generation(&self, key: &str) -> Option<u32> {
        self.lock().generations.get(key).copied()
    }

    /// See [`CorrelationMaps::set_generation`].
    pub fn set_generation(&self, key: &str, generation: u32) {
        self.lock().generations.insert(key.to_string(), generation);
    }

    /// See [`CorrelationMaps::bind_call`].
    pub fn bind_call(&self, principal: &Principal, call_id: &str, session: &SessionId) {
        self.lock().calls.bind(principal, call_id, session);
    }

    /// See [`CorrelationMaps::session_of_call`].
    pub fn session_of_call(&self, principal: &Principal, call_id: &str) -> Option<SessionId> {
        self.lock().calls.session_of(principal, call_id)
    }

    /// See [`CorrelationMaps::bind_thread`].
    pub fn bind_thread(&self, principal: &Principal, thread_id: &str, session: &SessionId) {
        self.lock().threads.bind(principal, thread_id, session);
    }

    /// See [`CorrelationMaps::session_of_thread`].
    pub fn session_of_thread(&self, principal: &Principal, thread_id: &str) -> Option<SessionId> {
        self.lock().threads.session_of(principal, thread_id)
    }

    /// The lock, in one place.
    ///
    /// Recovering a poisoned guard rather than propagating the panic: every
    /// entry here is a binding that re-derives — a lost generation is one cold
    /// prefix, a lost call binding is one MCP call that falls back — and
    /// failing every later request over one poisoned map is a worse outcome
    /// than serving the next one from possibly-stale state.
    fn lock(&self) -> std::sync::MutexGuard<'_, Tables> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl CorrelationMaps for MemoryCorrelationMaps {
    async fn generation(&self, key: &str) -> Result<Option<u32>, CorrelationError> {
        Ok(MemoryCorrelationMaps::generation(self, key))
    }

    async fn set_generation(&self, key: &str, generation: u32) -> Result<(), CorrelationError> {
        MemoryCorrelationMaps::set_generation(self, key, generation);
        Ok(())
    }

    async fn bind_call(
        &self,
        principal: &Principal,
        call_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError> {
        MemoryCorrelationMaps::bind_call(self, principal, call_id, session);
        Ok(())
    }

    async fn session_of_call(
        &self,
        principal: &Principal,
        call_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        Ok(MemoryCorrelationMaps::session_of_call(
            self, principal, call_id,
        ))
    }

    async fn bind_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError> {
        MemoryCorrelationMaps::bind_thread(self, principal, thread_id, session);
        Ok(())
    }

    async fn session_of_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        Ok(MemoryCorrelationMaps::session_of_thread(
            self, principal, thread_id,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::spend::contract::fresh_principal;

    // The behavioural assertions live in `contract` and run from here by the
    // macro below, for the reason the fair-use ledger's do: leaving them here
    // would judge the memory maps by one list and Redis by another, which is
    // the exact drift the suite exists to make impossible.
    crate::correlation_maps_contract_suite!(MemoryCorrelationMaps::new());

    /// The remembered-calls cap evicts oldest-first, and a re-binding does not
    /// spend a queue slot.
    ///
    /// Not in the contract: it is a claim about *this* backend's bound, and a
    /// backend that expires by time instead — as the Redis one does — has no
    /// queue to assert on. What losing an entry costs is the fallback the
    /// contract's own "an unknown id answers `None`" already pins.
    #[test]
    fn the_call_table_is_capped_and_forgets_its_oldest_bindings_first() {
        let maps = MemoryCorrelationMaps::new();
        let ada = fresh_principal("ada");
        let session = SessionId::new("acme/ada/main");
        for n in 0..=REMEMBERED_CALLS {
            maps.bind_call(&ada, &format!("toolu_{n}"), &session);
        }

        assert_eq!(
            maps.session_of_call(&ada, "toolu_0"),
            None,
            "the oldest binding is the one the cap gives up"
        );
        assert_eq!(
            maps.session_of_call(&ada, &format!("toolu_{REMEMBERED_CALLS}")),
            Some(session),
            "and the newest is kept, which is the one a live tool loop is \
             about to answer"
        );
        assert_eq!(
            maps.lock().calls.sizes(&ada),
            (REMEMBERED_CALLS, REMEMBERED_CALLS)
        );

        // Re-binding an id already held must not grow the order queue past the
        // map, or the cap evicts a key that is still live and the two halves
        // drift apart.
        maps.bind_call(&ada, "toolu_1", &SessionId::new("acme/ada/other"));
        let (held, ordered) = maps.lock().calls.sizes(&ada);
        assert_eq!(ordered, held);
    }

    /// The remembered-calls cap is per principal, so a co-tenant's tool
    /// traffic cannot evict a *different* principal's binding (M12 review,
    /// F15) — the half the oldest-first test above does not cover, that one
    /// being the control that a tenant still ages out its own oldest entry.
    #[test]
    fn a_co_tenants_call_traffic_does_not_evict_another_principals_call_binding() {
        let maps = MemoryCorrelationMaps::new();
        let ada = fresh_principal("ada");
        let bob = fresh_principal("bob");
        let subagent = SessionId::new("acme/ada/sub");
        maps.bind_call(&ada, "toolu_ada_sub", &subagent);

        let bobs = SessionId::new("globex/bob/main");
        for n in 0..REMEMBERED_CALLS {
            maps.bind_call(&bob, &format!("toolu_bob_{n}"), &bobs);
        }

        assert_eq!(
            maps.session_of_call(&ada, "toolu_ada_sub"),
            Some(subagent),
            "a principal's own call binding must survive another tenant's \
             tool traffic; a node-wide cap makes it fall through to the same \
             None a foreign id would answer with"
        );
    }

    /// The thread cap evicts oldest-first, and a rebinding does not spend a
    /// queue slot.
    ///
    /// The second half is the one with teeth: rebinding is the *ordinary* case
    /// here (every fork rebinds), so a `bind` that pushed a second order entry
    /// would evict live threads at a rate set by how often clients compact.
    #[test]
    fn the_thread_table_is_capped_and_a_rebinding_does_not_grow_its_queue() {
        let maps = MemoryCorrelationMaps::new();
        let ada = fresh_principal("ada");
        let session = SessionId::new("acme/ada/main");
        for n in 0..=REMEMBERED_THREADS {
            maps.bind_thread(&ada, &format!("thread-{n}"), &session);
        }

        assert_eq!(
            maps.session_of_thread(&ada, "thread-0"),
            None,
            "the oldest binding is the one the cap gives up"
        );
        assert_eq!(
            maps.session_of_thread(&ada, &format!("thread-{REMEMBERED_THREADS}")),
            Some(session),
            "and the newest is kept, which is the thread a live tool loop is \
             about to answer"
        );
        assert_eq!(
            maps.lock().threads.sizes(&ada),
            (REMEMBERED_THREADS, REMEMBERED_THREADS)
        );

        let forked = SessionId::new("acme/ada/main#g1");
        maps.bind_thread(&ada, "thread-1", &forked);
        let (held, ordered) = maps.lock().threads.sizes(&ada);
        assert_eq!(ordered, held);
        assert_eq!(maps.session_of_thread(&ada, "thread-1"), Some(forked));
    }
}
