// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The one correlation write that has a condition in it.
//!
//! Everything else this family writes is a `SET`: a generation because it is a
//! hint a search starts from, a thread binding because the latest write is the
//! answer by contract. Binding a *call* is the exception, and the reason is
//! the condition itself — "an id already held by a different session of this
//! principal names neither of them". Evaluated by reading and then writing,
//! two nodes streaming two calls that happen to share an id each see an absent
//! key and each write itself in; one binding survives, confidently wrong,
//! which is the M12 F14 defect with a network in the middle instead of a lock.
//! A Redis script executes with nothing in between, so the check and the write
//! are one indivisible step.
//!
//! **The clock is the caller's, never `redis.call('TIME')`** — a departure
//! from `crate::scripts`' convention and the same one `spend::scripts`
//! documents. Here it is not even a departure of substance: the only time this
//! script handles is a *duration*, the staleness bound, and `PEXPIRE` is
//! evaluated against the server's own clock regardless. What is caller-supplied
//! is which bound to apply, so the gated suite can watch a binding expire
//! without waiting out a production TTL.

use redis::aio::ConnectionManager;
use roundhouse_core::control::CorrelationError;

/// Bind a tool-use id, or mark it ambiguous.
///
/// Three cases, and the middle one is the reason this is not a `SET NX`:
///
/// - **absent** — this is the first session to claim the id, so it is bound;
/// - **held by the same session** — a resend or a dedup replay, which is one
///   call seen twice rather than two calls. The binding it already has is
///   still exactly right, so only its lifetime is refreshed; overwriting it
///   would be the same value written twice, and treating it as a collision
///   would throw away a correct answer;
/// - **held by anything else** — a different session, *or* the marker already
///   written by an earlier collision. Both become the marker, which is what
///   makes an ambiguous id stay ambiguous rather than being reclaimed by its
///   next claimant.
///
/// `PX` on every branch, including the refresh: a binding's staleness is
/// measured from the last time anything asserted it, not from the first.
const BIND_CALL: &str = r"
local held = redis.call('GET', KEYS[1])
if held == false then
  redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[3])
elseif held == ARGV[1] then
  redis.call('PEXPIRE', KEYS[1], ARGV[3])
else
  redis.call('SET', KEYS[1], ARGV[2], 'PX', ARGV[3])
end
";

/// The family's scripts, compiled once per handle.
///
/// `redis::Script` sends `EVALSHA` and falls back to `EVAL` on `NOSCRIPT`, so
/// a restarted or failed-over Redis re-learns them transparently.
pub(crate) struct Scripts {
    bind_call: redis::Script,
}

impl Scripts {
    pub(crate) fn new() -> Self {
        Self {
            bind_call: redis::Script::new(BIND_CALL),
        }
    }

    /// Returns nothing, deliberately.
    ///
    /// Which branch fired is not a fact the caller may act on: the trait's
    /// `bind_call` promises only that the id is afterwards bound or ambiguous,
    /// and a caller that behaved differently on "you collided" would be making
    /// the streaming path depend on a race it cannot control. The next
    /// `session_of_call` is where the outcome becomes visible, which is where
    /// the contract asserts it.
    pub(crate) async fn bind_call(
        &self,
        conn: &mut ConnectionManager,
        key: &str,
        bound_value: &str,
        ttl_ms: u64,
    ) -> Result<(), CorrelationError> {
        self.bind_call
            .key(key)
            .arg(bound_value)
            .arg(super::AMBIGUOUS_MARKER)
            .arg(ttl_ms)
            .invoke_async::<()>(conn)
            .await
            .map_err(super::backend)
    }
}
