// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The in-memory [`CorrelationMaps`]: the specification, and the whole of a
//! deployment that has not named a Redis.
//!
//! Its own file rather than the trait's, in the shape `conversations` one
//! crate over already had (M14.2 review, F1). What is here is only what a
//! backend is: the handle, the two per-principal tables it keeps, and the
//! trait impl. The bounds those tables are built with, and the
//! [`AgedTable`] that enforces them, are the module above's — a backend does
//! not get to have its own opinion about how long a binding is a plausible
//! answer for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{
    AgedTable, CALL_BINDING_STALENESS_MS, Clock, CorrelationError, CorrelationMaps,
    REMEMBERED_CALLS, REMEMBERED_THREADS, THREAD_BINDING_STALENESS_MS,
};
use crate::control::Principal;
use crate::ids::SessionId;

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
    /// See [`Clock`]. Handed to every per-principal table this handle builds,
    /// so one `with_clock` moves time for all of them. Not `Debug`, so
    /// [`MemoryCorrelationMaps`] implements it by hand rather than deriving —
    /// the same shape `Conversations`' hand-written `Debug` already has for
    /// its own trait object field.
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
pub(super) struct Tables {
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
    pub(super) calls: CallTable,
    pub(super) threads: ThreadTable,
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
pub(super) struct CallTable {
    per_principal: HashMap<Principal, AgedTable<CallSite>>,
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
    /// This principal's table, built on first use with the bound and the cap
    /// this family names — once, here.
    fn table(&mut self, principal: &Principal, clock: &Clock) -> &mut AgedTable<CallSite> {
        self.per_principal
            .entry(principal.clone())
            .or_insert_with(|| {
                AgedTable::new(REMEMBERED_CALLS, Some(CALL_BINDING_STALENESS_MS))
                    .sharing_clock(clock)
            })
    }

    fn bind(&mut self, principal: &Principal, call_id: &str, session: &SessionId, clock: &Clock) {
        self.table(principal, clock)
            .write(call_id, |held| match held {
                // A resend or a dedup replay re-binds an id this node already
                // holds to the session that already holds it. That is one call
                // seen twice, not two calls, and treating it as a collision would
                // throw away a binding that is still exactly right — only its
                // lifetime is refreshed, the same shape the Redis script's
                // `PEXPIRE`-on-every-branch has.
                Some(CallSite::Bound(held)) if held == *session => CallSite::Bound(held),
                // Two of this principal's sessions have claimed the id. `held` is
                // live by construction — [`AgedTable::write`] has already dropped
                // it if it aged out — so a second claim past the bound arrives
                // here as `None` and binds, exactly as it does against Redis,
                // whose key has expired by then (M14.2 review, F3).
                Some(_) => CallSite::Ambiguous,
                None => CallSite::Bound(session.clone()),
            });
    }

    fn session_of(&mut self, principal: &Principal, call_id: &str) -> Option<SessionId> {
        match self.per_principal.get_mut(principal)?.get(call_id)? {
            CallSite::Bound(session) => Some(session.clone()),
            CallSite::Ambiguous => None,
        }
    }

    /// How many bindings this table holds for `principal`, and how long its
    /// eviction queue is.
    ///
    /// One call returning both rather than two accessors, so a test asserts
    /// the invariant on the type that owns it instead of reaching through a
    /// lock into private fields.
    #[cfg(test)]
    pub(super) fn sizes(&self, principal: &Principal) -> (usize, usize) {
        self.per_principal
            .get(principal)
            .map(|calls| (calls.len(), calls.queue_len()))
            .unwrap_or_default()
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
/// id names a conversation, and a conversation legitimately moves. That is the
/// whole of the difference between the two tables: one value type, one update
/// rule, and the same [`AgedTable`] underneath.
#[derive(Debug, Default)]
pub(super) struct ThreadTable {
    per_principal: HashMap<Principal, AgedTable<SessionId>>,
}

impl ThreadTable {
    /// See [`CallTable::table`]: this family's bound and cap, named once.
    fn table(&mut self, principal: &Principal, clock: &Clock) -> &mut AgedTable<SessionId> {
        self.per_principal
            .entry(principal.clone())
            .or_insert_with(|| {
                AgedTable::new(REMEMBERED_THREADS, Some(THREAD_BINDING_STALENESS_MS))
                    .sharing_clock(clock)
            })
    }

    fn bind(&mut self, principal: &Principal, thread_id: &str, session: &SessionId, clock: &Clock) {
        // The fork case, and the resend case, and they are the same write:
        // this thread's newest turn decided this session, whatever the last
        // one decided.
        self.table(principal, clock)
            .write(thread_id, |_| session.clone());
    }

    fn session_of(&mut self, principal: &Principal, thread_id: &str) -> Option<SessionId> {
        self.per_principal
            .get_mut(principal)?
            .get(thread_id)
            .cloned()
    }

    /// How many bindings this table holds for `principal`, and how long its
    /// eviction queue is. See [`CallTable::sizes`].
    #[cfg(test)]
    pub(super) fn sizes(&self, principal: &Principal) -> (usize, usize) {
        self.per_principal
            .get(principal)
            .map(|threads| (threads.len(), threads.queue_len()))
            .unwrap_or_default()
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
    /// the trait every caller would have to carry. Every per-principal table
    /// is built with whatever clock the handle holds when the table is first
    /// touched, which is why this is a builder taking `self` rather than a
    /// setter: a clock swapped in halfway would move time for the tables
    /// built after it and no others.
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
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, Tables> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        self.lock()
            .calls
            .bind(principal, call_id, session, &self.clock);
        Ok(())
    }

    async fn session_of_call(
        &self,
        principal: &Principal,
        call_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        Ok(self.lock().calls.session_of(principal, call_id))
    }

    async fn bind_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError> {
        self.lock()
            .threads
            .bind(principal, thread_id, session, &self.clock);
        Ok(())
    }

    async fn session_of_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        Ok(self.lock().threads.session_of(principal, thread_id))
    }
}
