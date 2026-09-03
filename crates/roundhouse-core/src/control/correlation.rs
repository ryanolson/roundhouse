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
//! Both bindings are bounded by *age* now (M14.2, R-S1) as well as by count —
//! [`CALL_BINDING_STALENESS_MS`] and [`THREAD_BINDING_STALENESS_MS`], named
//! here rather than in `roundhouse-store-redis` so the memory tables and the
//! Redis keys expire against the same constant. A binding older than any
//! plausible turn is a stale guess whatever a table's size — D1's R14 — and
//! that reasoning does not care which backend is holding the guess. What
//! still differs by backend is the *mechanism*: Redis has no natural place to
//! keep an eviction queue, so it hands the bound straight to `PEXPIRE`
//! (`roundhouse-store-redis::correlation`); the memory tables have no
//! background sweeper, so [`MemoryCorrelationMaps`] records the instant of
//! every write and enforces the bound itself — a read past it answers absent
//! and drops the entry, and a write sweeps the queue's head for anything else
//! that has aged out, independently of the capacity sweep
//! ([`REMEMBERED_CALLS`], [`REMEMBERED_THREADS`]) that already runs there.
//! Neither bound waits on the other: a table under its cap still ages out an
//! idle entry, and a table with everything fresh still evicts at the cap.
//!
//! **What one rung's doc got wrong about the next one, left here rather than
//! quietly fixed.** M14.1's version of this section predicted that
//! `roundhouse-server`'s `Conversations` would give its own generation memo
//! "the staleness bound the two binding families already have" — but R-S2
//! gives that memo a *capacity* cap with no staleness bound at all, for the
//! opposite reason: a wrong generation hint costs a probe, not a wrong
//! answer, so aging it out would only trade a cheap correction for a
//! needless one. The binding families here are not that memo, and this doc
//! no longer conflates them.
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
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::control::Principal;
use crate::ids::SessionId;

/// How long a tool-call binding is a plausible answer for, in either
/// implementation (M14.2, R-S1).
///
/// Moved here from `roundhouse-store-redis` so the memory tables and the
/// Redis keys age out against the *same* constant rather than two numbers
/// that happen to agree today and are free to drift tomorrow. The value and
/// its reasoning are unchanged from M14.1: a tool-use id is minted as
/// roundhouse streams the call and consumed by the client's answer to it,
/// one leg of one tool loop, and six hours is orders of magnitude beyond any
/// such leg. Expiring early costs one MCP call falling back to the
/// principal's most recent conversation — the answer it got before R-M2
/// existed; expiring late costs memory and nothing else, because the id it
/// names was emitted once and never re-minted.
pub const CALL_BINDING_STALENESS_MS: u64 = 6 * 60 * 60 * 1_000;

/// How long a thread binding is a plausible answer for, in either
/// implementation. See [`CALL_BINDING_STALENESS_MS`] for why this moved.
///
/// Longer by two orders of magnitude because the thing bounded is different:
/// a call id is live for one leg of one tool loop, where a thread id names a
/// conversation a client may resume tomorrow. Seven days is the point past
/// which a resumed thread has almost certainly compacted — which forks,
/// which rebinds this key anyway — so an expiry beyond it would be keeping an
/// answer no client is going to ask for.
pub const THREAD_BINDING_STALENESS_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// What time [`MemoryCorrelationMaps`] thinks it is.
///
/// A function pointer rather than a parameter on every trait method: the
/// trait's methods are shared with [`RedisCorrelationMaps`](https://docs.rs/roundhouse-store-redis)
/// wire-compatible signatures, and threading a clock through all of them
/// for the sake of one backend's test seam would be the tail wagging the
/// dog. The production default is [`crate::now_ms`]; a test that wants to
/// watch a binding age out without sleeping replaces it with
/// [`MemoryCorrelationMaps::with_clock`] — the same shape
/// `RedisFairUseLedger::with_bucket_ttl_ms` and
/// `RedisCorrelationMaps::with_binding_ttls` already give their own
/// backends, a lever on the handle rather than a knob on the constant.
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

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
/// Its own methods are `async` like the trait's, with nothing to await
/// underneath — a `HashMap` behind a `Mutex` never yields — because every
/// caller now holds it behind `Arc<dyn CorrelationMaps>` (`Conversations`
/// included, since R-C4 lets the composition root swap in the Redis backend)
/// and a second, synchronous surface here would have no caller to be thin
/// for.
pub struct MemoryCorrelationMaps {
    inner: Mutex<Tables>,
    /// See [`Clock`]. Not `Debug`, so [`MemoryCorrelationMaps`] implements it
    /// by hand rather than deriving — the same shape `Conversations`'
    /// hand-written `Debug` already has for its own trait object field.
    clock: Clock,
}

impl std::fmt::Debug for MemoryCorrelationMaps {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryCorrelationMaps")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl Default for MemoryCorrelationMaps {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
struct Tables {
    /// The generation each *namespaced* cache key was last committed at, for
    /// every key this node has committed.
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

/// One principal's remembered calls, oldest-written-first.
#[derive(Debug, Default)]
struct PrincipalCalls {
    sites: HashMap<String, CallEntry>,
    /// Write order of [`PrincipalCalls::sites`], so a sweep sees the oldest
    /// first — both the capacity sweep, which evicts from here regardless of
    /// age, and the staleness sweep, which stops at the first entry whose own
    /// timestamp says it is still live (M14.2, R-S1). A rebind refreshes the
    /// entry in place rather than pushing a second position, exactly as
    /// before: the invariant is still exactly one entry per site, kept here
    /// rather than inline in one method's body because an invariant like that
    /// is one the next edit breaks with nothing red (M12 review, F13).
    order: VecDeque<String>,
}

/// What one remembered call id names, if anything, and when this table last
/// heard it asserted.
#[derive(Debug, Clone)]
struct CallEntry {
    site: CallSite,
    /// When this entry was last written — bound, refreshed, or turned
    /// ambiguous by a collision. A collision refreshes it for the same reason
    /// the Redis script's `PEXPIRE` fires on every branch: a binding's
    /// staleness is measured from the last time anything asserted it, not
    /// from the first.
    written_at_ms: u64,
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
    fn bind(&mut self, principal: &Principal, call_id: &str, session: &SessionId, now: u64) {
        self.per_principal
            .entry(principal.clone())
            .or_default()
            .bind(call_id, session, now);
    }

    /// `&mut self`, not `&self`: a read past the staleness bound drops the
    /// entry it answers absent for (M14.2, R-S1) — the same rule Redis
    /// enforces by simply no longer having the key.
    fn session_of(&mut self, principal: &Principal, call_id: &str, now: u64) -> Option<SessionId> {
        self.per_principal
            .get_mut(principal)?
            .session_of(call_id, now)
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
    fn bind(&mut self, call_id: &str, session: &SessionId, now: u64) {
        match self.sites.get_mut(call_id) {
            // A resend or a dedup replay re-binds an id this node already
            // holds to the session that already holds it. That is one call
            // seen twice, not two calls, and treating it as a collision would
            // throw away a binding that is still exactly right — only its
            // lifetime is refreshed, the same shape the Redis script's
            // `PEXPIRE`-on-every-branch has.
            Some(entry) if matches!(&entry.site, CallSite::Bound(held) if held == session) => {
                entry.written_at_ms = now;
            }
            Some(entry) => {
                entry.site = CallSite::Ambiguous;
                entry.written_at_ms = now;
            }
            None => {
                self.sites.insert(
                    call_id.to_string(),
                    CallEntry {
                        site: CallSite::Bound(session.clone()),
                        written_at_ms: now,
                    },
                );
                self.order.push_back(call_id.to_string());
            }
        }
        self.sweep(now);
    }

    fn session_of(&mut self, call_id: &str, now: u64) -> Option<SessionId> {
        let entry = self.sites.get(call_id)?;
        if is_stale(entry.written_at_ms, now, CALL_BINDING_STALENESS_MS) {
            self.drop_entry(call_id);
            return None;
        }
        match &self.sites.get(call_id)?.site {
            CallSite::Bound(session) => Some(session.clone()),
            CallSite::Ambiguous => None,
        }
    }

    /// Age out whatever has aged out at the queue's head, then cap by count —
    /// two independent walks, so a table under its cap still ages out an idle
    /// entry and a table with nothing stale still evicts at the cap.
    ///
    /// Stops at the first entry whose *own* timestamp is not stale: a
    /// rebound entry keeps its original queue position (a rebind spends no
    /// slot) but its timestamp is current, so the age sweep correctly leaves
    /// it — and anything behind it — for a later write or for the read-side
    /// drop to catch. That is a bound on how much one write's sweep does, not
    /// a bound on correctness: nothing is ever answered past its staleness
    /// bound, because [`Self::session_of`] checks the entry it is about to
    /// answer regardless of what any sweep has gotten to.
    fn sweep(&mut self, now: u64) {
        while let Some(front) = self.order.front() {
            let stale = self
                .sites
                .get(front)
                .is_none_or(|entry| is_stale(entry.written_at_ms, now, CALL_BINDING_STALENESS_MS));
            if !stale {
                break;
            }
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.sites.remove(&oldest);
        }
        while self.order.len() > REMEMBERED_CALLS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.sites.remove(&oldest);
        }
    }

    fn drop_entry(&mut self, call_id: &str) {
        self.sites.remove(call_id);
        self.order.retain(|id| id != call_id);
    }
}

/// Whether an entry written at `written_at_ms` is stale as of `now`, against
/// `bound_ms`.
///
/// `saturating_sub` rather than plain subtraction: a clock seam a test moves
/// backwards, or a node whose wall clock jumps, must read as "not yet stale"
/// rather than panic or wrap into a number bigger than any bound.
fn is_stale(written_at_ms: u64, now: u64, bound_ms: u64) -> bool {
    now.saturating_sub(written_at_ms) > bound_ms
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

/// One principal's remembered threads, oldest-written-first.
#[derive(Debug, Default)]
struct PrincipalThreads {
    sessions: HashMap<String, ThreadEntry>,
    /// Write order of [`PrincipalThreads::sessions`], for
    /// [`PrincipalCalls::order`]'s reason. Exactly one entry per thread — a
    /// rebinding must not push a second one, or the cap drops a key that is
    /// still live.
    order: VecDeque<String>,
}

/// Which session a thread is in, and when this table last heard it asserted.
#[derive(Debug, Clone)]
struct ThreadEntry {
    session: SessionId,
    written_at_ms: u64,
}

impl ThreadTable {
    fn bind(&mut self, principal: &Principal, thread_id: &str, session: &SessionId, now: u64) {
        self.per_principal
            .entry(principal.clone())
            .or_default()
            .bind(thread_id, session, now);
    }

    /// `&mut self` for [`CallTable::session_of`]'s reason: a read past the
    /// staleness bound drops the entry it answers absent for.
    fn session_of(
        &mut self,
        principal: &Principal,
        thread_id: &str,
        now: u64,
    ) -> Option<SessionId> {
        self.per_principal
            .get_mut(principal)?
            .session_of(thread_id, now)
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
    fn bind(&mut self, thread_id: &str, session: &SessionId, now: u64) {
        if let Some(held) = self.sessions.get_mut(thread_id) {
            // The fork case, and the resend case, and they are the same
            // write: this thread's newest turn decided this session, and
            // either way the binding's lifetime is measured from now.
            held.session = session.clone();
            held.written_at_ms = now;
            self.sweep(now);
            return;
        }
        self.sessions.insert(
            thread_id.to_string(),
            ThreadEntry {
                session: session.clone(),
                written_at_ms: now,
            },
        );
        self.order.push_back(thread_id.to_string());
        self.sweep(now);
    }

    fn session_of(&mut self, thread_id: &str, now: u64) -> Option<SessionId> {
        let entry = self.sessions.get(thread_id)?;
        if is_stale(entry.written_at_ms, now, THREAD_BINDING_STALENESS_MS) {
            self.drop_entry(thread_id);
            return None;
        }
        self.sessions
            .get(thread_id)
            .map(|entry| entry.session.clone())
    }

    /// See [`PrincipalCalls::sweep`]: age first, from the head, then cap by
    /// count — two independent bounds.
    fn sweep(&mut self, now: u64) {
        while let Some(front) = self.order.front() {
            let stale = self.sessions.get(front).is_none_or(|entry| {
                is_stale(entry.written_at_ms, now, THREAD_BINDING_STALENESS_MS)
            });
            if !stale {
                break;
            }
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.sessions.remove(&oldest);
        }
        while self.order.len() > REMEMBERED_THREADS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.sessions.remove(&oldest);
        }
    }

    fn drop_entry(&mut self, thread_id: &str) {
        self.sessions.remove(thread_id);
        self.order.retain(|id| id != thread_id);
    }
}

impl MemoryCorrelationMaps {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Tables::default()),
            clock: Arc::new(crate::now_ms),
        }
    }

    /// Replace this handle's notion of "now", so a test can watch a binding
    /// age out without sleeping and without touching the production bound.
    ///
    /// The same lever `RedisFairUseLedger::with_bucket_ttl_ms` and
    /// `RedisCorrelationMaps::with_binding_ttls` already give their own
    /// backends — one seam per implementation, not one signature change on
    /// the trait every caller would have to carry.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
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

    fn now(&self) -> u64 {
        (self.clock)()
    }
}

#[async_trait]
impl CorrelationMaps for MemoryCorrelationMaps {
    async fn generation(&self, key: &str) -> Result<Option<u32>, CorrelationError> {
        Ok(self.lock().generations.get(key).copied())
    }

    async fn set_generation(&self, key: &str, generation: u32) -> Result<(), CorrelationError> {
        self.lock().generations.insert(key.to_string(), generation);
        Ok(())
    }

    async fn bind_call(
        &self,
        principal: &Principal,
        call_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError> {
        let now = self.now();
        self.lock().calls.bind(principal, call_id, session, now);
        Ok(())
    }

    async fn session_of_call(
        &self,
        principal: &Principal,
        call_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        let now = self.now();
        Ok(self.lock().calls.session_of(principal, call_id, now))
    }

    async fn bind_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError> {
        let now = self.now();
        self.lock().threads.bind(principal, thread_id, session, now);
        Ok(())
    }

    async fn session_of_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        let now = self.now();
        Ok(self.lock().threads.session_of(principal, thread_id, now))
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
    #[tokio::test]
    async fn the_call_table_is_capped_and_forgets_its_oldest_bindings_first() {
        let maps = MemoryCorrelationMaps::new();
        let ada = fresh_principal("ada");
        let session = SessionId::new("acme/ada/main");
        for n in 0..=REMEMBERED_CALLS {
            maps.bind_call(&ada, &format!("toolu_{n}"), &session)
                .await
                .unwrap();
        }

        assert_eq!(
            maps.session_of_call(&ada, "toolu_0").await.unwrap(),
            None,
            "the oldest binding is the one the cap gives up"
        );
        assert_eq!(
            maps.session_of_call(&ada, &format!("toolu_{REMEMBERED_CALLS}"))
                .await
                .unwrap(),
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
        maps.bind_call(&ada, "toolu_1", &SessionId::new("acme/ada/other"))
            .await
            .unwrap();
        let (held, ordered) = maps.lock().calls.sizes(&ada);
        assert_eq!(ordered, held);
    }

    /// The remembered-calls cap is per principal, so a co-tenant's tool
    /// traffic cannot evict a *different* principal's binding (M12 review,
    /// F15) — the half the oldest-first test above does not cover, that one
    /// being the control that a tenant still ages out its own oldest entry.
    #[tokio::test]
    async fn a_co_tenants_call_traffic_does_not_evict_another_principals_call_binding() {
        let maps = MemoryCorrelationMaps::new();
        let ada = fresh_principal("ada");
        let bob = fresh_principal("bob");
        let subagent = SessionId::new("acme/ada/sub");
        maps.bind_call(&ada, "toolu_ada_sub", &subagent)
            .await
            .unwrap();

        let bobs = SessionId::new("globex/bob/main");
        for n in 0..REMEMBERED_CALLS {
            maps.bind_call(&bob, &format!("toolu_bob_{n}"), &bobs)
                .await
                .unwrap();
        }

        assert_eq!(
            maps.session_of_call(&ada, "toolu_ada_sub").await.unwrap(),
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
    #[tokio::test]
    async fn the_thread_table_is_capped_and_a_rebinding_does_not_grow_its_queue() {
        let maps = MemoryCorrelationMaps::new();
        let ada = fresh_principal("ada");
        let session = SessionId::new("acme/ada/main");
        for n in 0..=REMEMBERED_THREADS {
            maps.bind_thread(&ada, &format!("thread-{n}"), &session)
                .await
                .unwrap();
        }

        assert_eq!(
            maps.session_of_thread(&ada, "thread-0").await.unwrap(),
            None,
            "the oldest binding is the one the cap gives up"
        );
        assert_eq!(
            maps.session_of_thread(&ada, &format!("thread-{REMEMBERED_THREADS}"))
                .await
                .unwrap(),
            Some(session),
            "and the newest is kept, which is the thread a live tool loop is \
             about to answer"
        );
        assert_eq!(
            maps.lock().threads.sizes(&ada),
            (REMEMBERED_THREADS, REMEMBERED_THREADS)
        );

        let forked = SessionId::new("acme/ada/main#g1");
        maps.bind_thread(&ada, "thread-1", &forked).await.unwrap();
        let (held, ordered) = maps.lock().threads.sizes(&ada);
        assert_eq!(ordered, held);
        assert_eq!(
            maps.session_of_thread(&ada, "thread-1").await.unwrap(),
            Some(forked)
        );
    }

    // -----------------------------------------------------------------------
    // M14.2, R-S1: age, under a scripted clock rather than a sleep
    // -----------------------------------------------------------------------

    /// A shared, settable clock a test can move without sleeping — the memory
    /// side's half of R-S4's "clock seam each implementation already has for
    /// tests", the Redis side's being its per-handle TTL lever.
    fn scripted_clock() -> (
        impl Fn() -> u64 + Send + Sync + 'static,
        Arc<std::sync::atomic::AtomicU64>,
    ) {
        let now = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let read = Arc::clone(&now);
        (move || read.load(std::sync::atomic::Ordering::Relaxed), now)
    }

    /// **The claim R-S1 states in the module doc, proved rather than only
    /// documented:** a binding older than its family's staleness bound
    /// answers exactly as one nothing ever wrote does, on both families, and
    /// a binding well inside the bound is untouched by the same clock advance.
    #[tokio::test]
    async fn a_binding_older_than_the_bound_is_absent_under_a_scripted_clock() {
        let (clock, now) = scripted_clock();
        let maps = MemoryCorrelationMaps::new().with_clock(clock);
        let ada = fresh_principal("ada");
        let session = SessionId::new("acme/ada/main");

        maps.bind_call(&ada, "toolu_ages_out", &session)
            .await
            .unwrap();
        maps.bind_thread(&ada, "thread-ages-out", &session)
            .await
            .unwrap();

        // CONTROL: well inside both bounds, both bindings still answer.
        now.store(60_000, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            maps.session_of_call(&ada, "toolu_ages_out").await.unwrap(),
            Some(session.clone())
        );
        assert_eq!(
            maps.session_of_thread(&ada, "thread-ages-out")
                .await
                .unwrap(),
            Some(session.clone())
        );

        // Past the call bound, short of the (much wider) thread bound: the
        // call binding is gone and the thread binding is not, which is the
        // proof the two ages are independent rather than one clock tripping
        // both at once.
        now.store(
            CALL_BINDING_STALENESS_MS + 1,
            std::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(
            maps.session_of_call(&ada, "toolu_ages_out").await.unwrap(),
            None,
            "a binding older than the bound answers exactly as an id nothing \
             ever emitted does"
        );
        assert_eq!(
            maps.session_of_thread(&ada, "thread-ages-out")
                .await
                .unwrap(),
            Some(session.clone()),
            "the thread bound is wider, and this clock has not reached it yet"
        );

        // Past both bounds: the thread binding is gone too.
        now.store(
            THREAD_BINDING_STALENESS_MS + 1,
            std::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(
            maps.session_of_thread(&ada, "thread-ages-out")
                .await
                .unwrap(),
            None
        );
    }

    /// **Age and count are independent bounds, neither waiting on the
    /// other.** A table well under its capacity cap still ages out an idle
    /// entry on the next write to a *different* key, and a table with
    /// nothing stale still evicts at the cap.
    #[tokio::test]
    async fn a_write_sweeps_aged_out_entries_from_the_head_independently_of_the_cap() {
        let (clock, now) = scripted_clock();
        let maps = MemoryCorrelationMaps::new().with_clock(clock);
        let ada = fresh_principal("ada");
        let first = SessionId::new("acme/ada/first");
        let second = SessionId::new("acme/ada/second");

        maps.bind_call(&ada, "toolu_stale", &first).await.unwrap();

        // Advance well past the call bound, then bind a second, unrelated
        // call. The table holds two entries, nowhere near REMEMBERED_CALLS —
        // the sweep that drops the first one is the age sweep, not the cap.
        now.store(
            CALL_BINDING_STALENESS_MS + 1,
            std::sync::atomic::Ordering::Relaxed,
        );
        maps.bind_call(&ada, "toolu_fresh", &second).await.unwrap();

        let (held, ordered) = maps.lock().calls.sizes(&ada);
        assert_eq!(
            (held, ordered),
            (1, 1),
            "the write that bound the fresh id must have swept the aged-out \
             one from the queue's head, or the table only shrinks when a \
             reader happens to ask about the stale key"
        );
        assert_eq!(
            maps.session_of_call(&ada, "toolu_fresh").await.unwrap(),
            Some(second)
        );
    }

    // -------------------------------------------------------------------
    // M14.2 thermo-nuclear review, F3: the clock seam is a convention
    // above; this pins it as a checked one.
    // -------------------------------------------------------------------

    /// **R-S4's methodology — through the clock seam, never by waiting out
    /// a real timer — checked, not merely written down.** A test author who
    /// drops `with_clock`/`scripted_clock` for a real, awaited timer does
    /// not fail an assertion here: the test just gets slow, and the only
    /// thing that notices today is the workspace's bounded-timeout house
    /// rule, which reports an opaque `exit 124` that names neither the
    /// timer nor the test. Scanning this file's own source for the banned
    /// call is what `fair_use_contract_convention.rs` does for its
    /// sibling-file convention, aimed here instead at the seam-vs-timer one.
    ///
    /// **The banned spelling is assembled at runtime, deliberately**, so
    /// this doc comment can describe it in prose without the scan tripping
    /// over its own description the way `fair_use_contract_convention.rs`'s
    /// scan has to special-case its one unavoidable self-match. There is no
    /// legitimate reason for `crate::time::sleep` (Tokio's async wait) to
    /// appear anywhere in this file, this doc comment included.
    ///
    /// Scoped to this file rather than the whole crate: `session.rs` waits
    /// on a real timer on purpose (a background poll loop and its test), and
    /// a crate-wide ban would be a false positive on a use this file's own
    /// staleness tests have nothing to do with.
    #[test]
    fn this_files_staleness_tests_move_time_through_the_seam_not_a_real_wait() {
        let banned = ["tokio", "time", "sleep"].join("::");
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest_dir).join("src/control/correlation.rs");
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("reading {path:?}: {error}"));
        assert!(
            !src.contains(&banned),
            "F3: this module ages a binding out by moving `with_clock`'s \
             scripted clock forward, never by waiting out a real one — a \
             test that waited instead would still pass every assertion, \
             only slower, and nothing but the workspace's bounded-timeout \
             habit would notice, as a bare exit 124 that points at no timer \
             and no test"
        );
    }
}
