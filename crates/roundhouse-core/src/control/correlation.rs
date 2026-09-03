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
//! # R-S5 — one bounded, aged table, three instantiations
//!
//! That mechanism is [`AgedTable`] and it is written once (M14.2 review, F2).
//! The rung that added the age bound added it to two structurally identical
//! per-principal tables here and to a third capped queue in
//! `roundhouse-server` — three hand-copied sweeps, three pairs of bound
//! constants, and nothing that would go red when one of them drifted. What
//! is left in each instantiation is only what genuinely differs: the value a
//! key names, the cap and bound it names once, and — for calls — the
//! collision arm and the three-state read that a thread rebinding has no
//! equivalent of. Those two really are variation points rather than
//! duplication, which is the correction the refute pass made to the finding
//! that opened this section.
//!
//! # Where this module's code lives
//!
//! The trait, the bounds and [`AgedTable`] are here; `memory` holds
//! [`MemoryCorrelationMaps`] and the two per-principal tables it keeps;
//! `contract` holds the assertions both backends answer to; `tests` holds
//! this module's unit tests. Four files rather than one, in the shape
//! `roundhouse-server`'s `conversations` already had, because a single file
//! holding the trait, a backend and its tests is the one a reader has to
//! read all of to find any of it (M14.2 review, F1).
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
mod memory;
#[cfg(test)]
mod tests;

pub use memory::MemoryCorrelationMaps;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

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

/// What time an [`AgedTable`] — and therefore the handle holding it — thinks
/// it is.
///
/// A field on the table rather than a parameter on every trait method: the
/// trait's methods are shared with [`RedisCorrelationMaps`](https://docs.rs/roundhouse-store-redis)
/// wire-compatible signatures, and threading a clock through all of them
/// for the sake of one backend's test seam would be the tail wagging the
/// dog. The production default is [`crate::now_ms`]; a test that wants to
/// watch a binding age out without sleeping replaces it with
/// [`AgedTable::with_clock`] or, one level up,
/// [`MemoryCorrelationMaps::with_clock`] — the same shape
/// `RedisFairUseLedger::with_bucket_ttl_ms` and
/// `RedisCorrelationMaps::with_binding_ttls` already give their own
/// backends, a lever on the handle rather than a knob on the constant.
///
/// `pub(crate)` and not private: a handle that owns several tables hands
/// each of them the *same* clock ([`AgedTable::sharing_clock`]), so a test
/// that moves time moves it for every table that handle keeps rather than
/// for whichever one it happened to reach first.
pub(crate) type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

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
// The one bounded, aged table (M14.2 review, F2 — R-S5)
// ---------------------------------------------------------------------------

/// A string-keyed table bounded by *count* and, optionally, by *age*.
///
/// The one type behind every local cache this deployment keeps for the
/// correlation families: one principal's remembered calls, one principal's
/// remembered threads, and — one crate over — a node's memo of the generation
/// map. Before this type they were three hand-copied `HashMap` + `VecDeque`
/// pairs with three copies of the same two-loop sweep, which is three places
/// for the next age-bound edit to land in two of (M14.2 review, F2).
///
/// # The rules, all of them
///
/// - **A read past the staleness bound answers absent and drops the entry**
///   (R-S1). There is no background sweeper here, so the read path is one of
///   the two places the bound is enforced; a table whose reader merely
///   *ignored* an aged-out entry would keep answering `Some` the moment the
///   caller stopped asking politely.
/// - **A write past the bound is a first write** (M14.2 review, F3): the
///   aged-out entry is gone before `update` is called, so a caller cannot
///   mistake it for a live one and treat a new claim as a collision with, or
///   a rebind of, something that no longer exists. This is exactly what the
///   Redis implementation gets for free — its key has expired by then — and
///   the two implementations have to agree *at* the bound, not near it.
/// - **A write moves the entry to the queue's tail** (M14.2 review, F8), so
///   the head is always the oldest write. A rebind that kept its original
///   position would leave a *fresh* entry at the head, where it stops the age
///   sweep before it reaches anything behind it and is then the very entry
///   the capacity cap pops — the table full of stale bindings, dropping the
///   one that was live. Exactly one position per key either way: a rebind
///   moves its position rather than pushing a second one.
/// - **The cap evicts oldest-first among *evictable* entries and never a
///   pinned one** (M14.2 review, F9). Most values are evictable and the
///   default predicate says so; a value whose loss would be a wrong answer
///   rather than one re-read says otherwise, and the table steps over it.
///
/// # Why a `BTreeMap` for the write order and not the obvious `VecDeque`
///
/// Because a rebind has to *move* a key's position, and moving one out of the
/// middle of a `VecDeque` is the `retain` walk the old tables paid on every
/// read-side drop. Keyed by a monotonic write counter instead, the order is
/// still "oldest first" by construction — `first_key_value` is the head — and
/// moving a position is one removal and one insertion. The counter is the
/// table's own and never a timestamp: two writes in the same millisecond
/// still order.
pub struct AgedTable<V> {
    entries: HashMap<String, AgedEntry<V>>,
    /// Every live key, keyed by the write that put it here — so the first
    /// pair is the oldest write and there is exactly one position per key.
    order: BTreeMap<u64, String>,
    next_write: u64,
    capacity: usize,
    /// `None` for a table bounded by count alone — the generation memo, whose
    /// entries are hints a probe corrects rather than answers a caller
    /// trusts, so aging one out would trade a free lookup for a needless
    /// store read (R-S2).
    staleness_bound_ms: Option<u64>,
    /// Whether an entry must survive the capacity cap. See
    /// [`AgedTable::with_pinned`].
    pinned: fn(&V) -> bool,
    clock: Clock,
}

/// One entry: what the key names, when that was last written, and which write
/// it was.
struct AgedEntry<V> {
    value: V,
    /// When this entry was last written — a first bind, a rebind, or anything
    /// else the caller's `update` decided. A rebind refreshes it for the same
    /// reason the Redis script's `PEXPIRE` fires on every branch: a binding's
    /// staleness is measured from the last time anything asserted it, not
    /// from the first.
    written_at_ms: u64,
    /// This entry's position in [`AgedTable::order`], so a write can move it
    /// and a removal can take it out without a walk.
    write: u64,
}

impl<V: std::fmt::Debug> std::fmt::Debug for AgedTable<V> {
    /// Hand-written because [`Clock`] is a trait object and `fn` pointers do
    /// not derive: what a reader of a debug line wants is the entries.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgedTable")
            .field("entries", &self.entries.len())
            .field("capacity", &self.capacity)
            .field("staleness_bound_ms", &self.staleness_bound_ms)
            .finish_non_exhaustive()
    }
}

impl<V: std::fmt::Debug> std::fmt::Debug for AgedEntry<V> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgedEntry")
            .field("value", &self.value)
            .field("written_at_ms", &self.written_at_ms)
            .finish_non_exhaustive()
    }
}

impl<V> AgedTable<V> {
    /// A table holding at most `capacity` entries, aging them out past
    /// `staleness_bound_ms` where there is one.
    ///
    /// Both bounds are named by the caller and named *once*: an instantiation
    /// that spelled its cap in the constructor and again in a sweep is how
    /// the two drift.
    pub fn new(capacity: usize, staleness_bound_ms: Option<u64>) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            next_write: 0,
            capacity,
            staleness_bound_ms,
            pinned: |_| false,
            clock: Arc::new(crate::now_ms),
        }
    }

    /// Declare which entries the capacity cap may not evict.
    ///
    /// The default is that none are pinned, which is right for every cache
    /// whose loss costs one re-read. It is wrong for exactly one value in
    /// this deployment — a generation this node committed and the store
    /// refused — because that entry is not a copy of anything: it is the
    /// node's only record of its own commit, and evicting it makes the node
    /// serve the generation it moved the client off (M14.1 review, F7,
    /// re-opened by M14.2 review, F9). A pinned entry stops being pinned when
    /// its value stops satisfying the predicate, which for that one is the
    /// next write that lands.
    ///
    /// The pinned population is therefore bounded by the number of keys whose
    /// commit was refused during one store outage, and it drains on the
    /// retry; a table whose entries were *all* pinned would simply grow past
    /// its cap rather than spin, which is the right failure for a store that
    /// has been refusing writes for that long.
    pub fn with_pinned(mut self, pinned: fn(&V) -> bool) -> Self {
        self.pinned = pinned;
        self
    }

    /// Replace this table's notion of "now", so a test can watch an entry age
    /// out without sleeping and without touching a production bound.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_clock(self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.sharing_clock(&(Arc::new(clock) as Clock))
    }

    /// Take the clock some handle above already holds, so every table that
    /// handle keeps reads the same time. See [`Clock`].
    pub(crate) fn sharing_clock(mut self, clock: &Clock) -> Self {
        self.clock = Arc::clone(clock);
        self
    }

    /// What `key` names, or `None` where nothing does *or* where what did has
    /// aged out — and in that second case the entry is dropped on the way
    /// past, so the table shrinks on reads as well as on writes.
    ///
    /// `&mut self` for exactly that reason: the read is the other half of the
    /// bound, and a `&self` read would have to answer absent while leaving
    /// the aged-out entry resident for the next sweep to find.
    pub fn get(&mut self, key: &str) -> Option<&V> {
        let now = self.now();
        if self
            .entries
            .get(key)
            .is_some_and(|entry| self.is_stale(entry.written_at_ms, now))
        {
            self.remove(key);
            return None;
        }
        self.entries.get(key).map(|entry| &entry.value)
    }

    /// Write `key`, deciding its new value from whatever is *live* there.
    ///
    /// `update` sees `None` for a key nothing holds and for one whose entry
    /// has aged out — the two are the same fact, which is the whole of "a
    /// write past the bound is a first write". The new entry is stamped now
    /// and moved to the queue's tail, and both bounds are then swept.
    pub fn write(&mut self, key: &str, update: impl FnOnce(Option<V>) -> V) {
        let now = self.now();
        // A removed entry that was already stale is not "held" (M14.2 review,
        // F3): showing it to `update` anyway is exactly the bug — a second
        // claim on a call id past its bound reads as a collision with a
        // binding R-S1 already calls absent, instead of as the first bind it
        // actually is. Redis gets this for free (the key has expired), so the
        // two implementations must agree at the bound rather than near it.
        let held = self
            .remove(key)
            .and_then(|entry| (!self.is_stale(entry.written_at_ms, now)).then_some(entry.value));
        let write = self.next_write;
        self.next_write += 1;
        self.entries.insert(
            key.to_string(),
            AgedEntry {
                value: update(held),
                written_at_ms: now,
                write,
            },
        );
        self.order.insert(write, key.to_string());
        self.sweep(now);
    }

    /// How many entries this table holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many positions the write-order queue holds.
    ///
    /// Equal to [`Self::len`], always: the invariant a rebind breaks by
    /// pushing a second position, which the cap then spends on a key that is
    /// still live. Public so a test asserts it on the type that owns it
    /// rather than reaching into private fields.
    pub fn queue_len(&self) -> usize {
        self.order.len()
    }

    /// Age out whatever has aged out at the queue's head, then cap by count —
    /// two independent walks, so a table under its cap still ages out an idle
    /// entry and a table with nothing stale still evicts at the cap.
    ///
    /// The age walk may stop at the first live head and that is not a bound
    /// on correctness: nothing is ever *answered* past its staleness bound,
    /// because [`Self::get`] checks the entry it is about to answer whatever
    /// a sweep has reached. What the head-stop would have cost — before a
    /// write moved its entry to the tail — is that a fresh head hid every
    /// stale entry behind it from the walk (M14.2 review, F8).
    fn sweep(&mut self, now: u64) {
        while let Some(oldest) = self.oldest_stale(now) {
            self.remove(&oldest);
        }
        while self.entries.len() > self.capacity {
            let Some(evictable) = self.oldest_evictable() else {
                break;
            };
            self.remove(&evictable);
        }
    }

    /// The head of the queue, if what is there has aged out.
    fn oldest_stale(&self, now: u64) -> Option<String> {
        let (_, key) = self.order.first_key_value()?;
        self.entries
            .get(key)
            .is_none_or(|entry| self.is_stale(entry.written_at_ms, now))
            .then(|| key.clone())
    }

    /// The oldest entry the cap is allowed to take. `None` where every entry
    /// is pinned — see [`Self::with_pinned`] for why that is a table over its
    /// cap rather than an eviction that ignores the pin.
    fn oldest_evictable(&self) -> Option<String> {
        self.order
            .values()
            .find(|key| {
                self.entries
                    .get(*key)
                    .is_none_or(|entry| !(self.pinned)(&entry.value))
            })
            .cloned()
    }

    /// Drop `key`'s entry and its queue position together — the two are one
    /// fact, and a removal that forgot the position is how the queue outgrows
    /// the map.
    fn remove(&mut self, key: &str) -> Option<AgedEntry<V>> {
        let entry = self.entries.remove(key)?;
        self.order.remove(&entry.write);
        Some(entry)
    }

    /// Whether an entry written at `written_at_ms` is stale as of `now`.
    ///
    /// `saturating_sub` rather than plain subtraction: a clock seam a test
    /// moves backwards, or a node whose wall clock jumps, must read as "not
    /// yet stale" rather than panic or wrap into a number bigger than any
    /// bound. A table with no staleness bound has nothing stale, ever.
    fn is_stale(&self, written_at_ms: u64, now: u64) -> bool {
        self.staleness_bound_ms
            .is_some_and(|bound_ms| now.saturating_sub(written_at_ms) > bound_ms)
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }
}
