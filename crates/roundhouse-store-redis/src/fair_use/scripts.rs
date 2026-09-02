// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The fair-use write and read paths: two Lua scripts and their reply
//! decoding, in the same idiom as [`crate::scripts`] and
//! [`crate::spend::scripts`].
//!
//! **`now_ms` and `at_ms` are arguments and never `redis.call('TIME')`**, the
//! same departure from [`crate::scripts`]'s convention that
//! [`crate::spend::scripts`] documents, for the same reason and with the same
//! force: `roundhouse_core::control::fair_use::FairUseLedger` takes the clock
//! as data precisely so a five-hour window boundary is reachable in a test
//! without waiting five hours, and a script that read the server clock would
//! make the two backends agree on everything except the one property the
//! contract suite exists to pin.
//!
//! **Counters are integers, and the trait's `f64` is converted once at the
//! Rust edge.** `HINCRBY` is exact and `INCRBYFLOAT` is not; dollars therefore
//! cross as *micro-dollars*, and the token and micro-dollar counts are passed
//! through to `HINCRBY` as the caller's own decimal strings rather than being
//! round-tripped through `tonumber` — a Lua number is a double, and an
//! `i64` near its ceiling does not survive one. This is the mirror image of
//! the spend ledger's rule (dollars as strings, never Lua numbers) arrived at
//! from the other side: there the hazard was RESP truncating a float reply,
//! here it is a double truncating an integer argument.
//!
//! **The reply's vocabulary comes from Rust.** The scope, window and quantity
//! names a refusal carries are handed *in* as `ARGV` — sourced from
//! `FairUseScope::wire_name` and friends — and handed straight back out, so
//! there is no second spelling of the vocabulary living in a Lua string
//! literal that could drift from the enum. The Rust side still parses them
//! back exhaustively, because a reply that named a scope no enum has is a
//! failure to report rather than a value to guess at.

use redis::Value;
use redis::aio::ConnectionManager;

use roundhouse_core::control::{
    FairUseError, FairUseQuantity, FairUseRefusal, FairUseScope, FairUseWindow,
};

/// `KEYS[1]` the project scope's bucket-key prefix, `KEYS[2]` the member's.
/// `ARGV`: at_ms, bucket_ms, tokens, micro-dollars, bucket TTL in ms.
///
/// Two `HINCRBY` on each of the two scopes plus the expiry, in one indivisible
/// step. Atomicity is not decorative here: the member ceiling and the project
/// ceiling are two counters over *one* draw, and a pair of round trips that
/// updated one and lost the other would leave a member enforced against a
/// project's counter — which is not a member ceiling.
///
/// The bucket key is built here rather than passed in, so the caller cannot
/// send `at_ms` and a bucket index that disagree. Both prefixes carry the
/// project's Redis Cluster hash tag, so every key this touches — declared or
/// derived — is in one slot.
///
/// `PEXPIRE` on every write rather than only on creation: a bucket's lifetime
/// is measured from the last draw that landed in it, which is the cheap
/// direction to be wrong in (a bucket outside every window is never summed, so
/// an over-long TTL costs storage and never correctness) and needs no `TTL`
/// read to decide.
const RECORD_DRAW: &str = r"
local index = math.floor(tonumber(ARGV[1]) / tonumber(ARGV[2]))
for i = 1, 2 do
  local key = KEYS[i] .. ':' .. string.format('%d', index)
  redis.call('HINCRBY', key, 't', ARGV[3])
  redis.call('HINCRBY', key, 'u', ARGV[4])
  redis.call('PEXPIRE', key, ARGV[5])
end
return {'OK'}
";

/// `KEYS[1]` the project scope's bucket-key prefix, `KEYS[2]` the member's.
///
/// `ARGV[1]` now_ms, `ARGV[2]` bucket_ms, `ARGV[3..4]` the two scope names,
/// `ARGV[5..6]` the two quantity names, then three fixed groups of eight —
/// one per window, *narrowest first* — each holding: span_ms, the window's
/// name, and for each scope in turn a present flag (`'1'`/`'0'`), a token cap
/// and a micro-dollar cap (`''` for "not capped on this quantity").
///
/// **A line-by-line twin of `FairUseTerms::exceeded_by` and
/// `earliest_retry_ms`**, and it has to be: the memory ledger's arithmetic is
/// the specification and one contract judges both. Windows ascending, project
/// before member inside a window, tokens before dollars inside a scope, the
/// first cap to be met wins, and the retry time is the moment the oldest
/// bucket that has to leave actually leaves.
///
/// The scan is widened lazily and never re-walked. `exceeded_by` asks "what
/// has this scope drawn inside this window" once per (window, scope) and
/// short-circuits on the first refusal; done literally, the 7-day window would
/// re-read the buckets the 5-hour one just read, and a fully-configured
/// membership would cost ~4700 reads on the admission path of every turn.
/// Because the windows are asked narrowest-first, each scope's scan only ever
/// extends *backwards*, so keeping the buckets found so far and reading only
/// the older stretch a wider window newly asks for is the same answer for at
/// most 2017 reads per scope — and, in the common case where the narrowest
/// window is the one that binds, 61.
///
/// Empty buckets are dropped on the way in. That is not a filter on the
/// arithmetic: a bucket of zeroes changes no sum and can never be the bucket
/// whose departure brings a window under its cap, because dropping zero from
/// an over-cap total leaves it over.
const WOULD_EXCEED: &str = r"
local now_ms = tonumber(ARGV[1])
local bucket_ms = tonumber(ARGV[2])
local scope_names = {ARGV[3], ARGV[4]}
local name_tokens, name_usd = ARGV[5], ARGV[6]
local now_index = math.floor(now_ms / bucket_ms)

-- Per-scope scan state: the non-empty buckets seen so far, ascending by index,
-- and the oldest index the scan has reached.
local scanned = {
  {list = {}, from = now_index + 1},
  {list = {}, from = now_index + 1},
}

local function widen(s, first)
  local state = scanned[s]
  if first >= state.from then return state.list end
  local older = {}
  for index = first, state.from - 1 do
    local pair = redis.call('HMGET', KEYS[s] .. ':' .. string.format('%d', index), 't', 'u')
    local t = tonumber(pair[1])
    local u = tonumber(pair[2])
    if t or u then
      older[#older + 1] = {index, t or 0, u or 0}
    end
  end
  for i = 1, #state.list do older[#older + 1] = state.list[i] end
  state.list = older
  state.from = first
  return state.list
end

-- The sum over one window, and -- if it is over a cap -- which quantity ran
-- out and when the window could next have room.
local function check(s, first, span_ms, max_tokens, max_micros)
  local list = widen(s, first)
  local tokens, micros = 0, 0
  for i = 1, #list do
    if list[i][1] >= first then
      tokens = tokens + list[i][2]
      micros = micros + list[i][3]
    end
  end
  local function over(t, u)
    return (max_tokens ~= nil and t >= max_tokens) or (max_micros ~= nil and u >= max_micros)
  end
  if not over(tokens, micros) then return nil end
  -- Tokens before dollars where both are capped: the token cap is the one an
  -- agent can reason about, because it is the quantity in its own context.
  local quantity = name_usd
  if max_tokens ~= nil and tokens >= max_tokens then quantity = name_tokens end
  -- Walk the buckets oldest-first, dropping each in turn, until what remains
  -- is under every cap; the answer is when that bucket's end leaves the
  -- window. Every bucket dropped and still over is only reachable with a cap
  -- of zero or below -- a window that can never have room -- and is answered
  -- with now_ms rather than a lie about the future, exactly as the memory
  -- ledger's `None` is.
  local retry = now_ms
  for i = 1, #list do
    if list[i][1] >= first then
      tokens = tokens - list[i][2]
      micros = micros - list[i][3]
      if not over(tokens, micros) then
        retry = (list[i][1] + 1) * bucket_ms + span_ms
        break
      end
    end
  end
  return {quantity, retry}
end

for w = 1, 3 do
  local base = 6 + (w - 1) * 8
  local span_ms = tonumber(ARGV[base + 1])
  local window_name = ARGV[base + 2]
  -- Floor division from a start clamped at the epoch, which is what the
  -- memory ledger's saturating subtraction does -- and the flooring is what
  -- includes the partially-overlapping trailing bucket whole.
  local start = now_ms - span_ms
  if start < 0 then start = 0 end
  local first = math.floor(start / bucket_ms)
  for s = 1, 2 do
    local sbase = base + 2 + (s - 1) * 3
    if ARGV[sbase + 1] == '1' then
      local hit = check(s, first, span_ms, tonumber(ARGV[sbase + 2]), tonumber(ARGV[sbase + 3]))
      if hit then
        return {'REFUSED', scope_names[s], window_name, hit[1], hit[2]}
      end
    end
  end
end
return {'NONE'}
";

/// One scope's caps for one window, already in the script's vocabulary.
///
/// The caps are strings because `''` is the script's "not capped on this
/// quantity" sentinel and `tonumber('')` is `nil` — the same `''`-means-absent
/// idiom [`crate::spend::scripts`] uses for a pooled allocation's missing
/// member ceiling, and for the same reason: a sentinel the script tests by
/// string equality can never be confused with a cap of zero.
pub(crate) struct ScopeCaps {
    pub(crate) present: bool,
    pub(crate) max_tokens: String,
    pub(crate) max_micros: String,
}

impl ScopeCaps {
    pub(crate) fn absent() -> Self {
        Self {
            present: false,
            max_tokens: String::new(),
            max_micros: String::new(),
        }
    }

    fn flag(&self) -> &'static str {
        if self.present { "1" } else { "0" }
    }
}

/// One window's group of arguments.
pub(crate) struct WindowArgs {
    pub(crate) span_ms: u64,
    pub(crate) name: &'static str,
    pub(crate) project: ScopeCaps,
    pub(crate) member: ScopeCaps,
}

pub(crate) struct RecordDrawArgs<'a> {
    pub(crate) project_key: &'a str,
    pub(crate) member_key: &'a str,
    pub(crate) at_ms: u64,
    pub(crate) bucket_ms: u64,
    /// Decimal strings, not integers: they reach `HINCRBY` untouched by Lua's
    /// double arithmetic. See the module doc.
    pub(crate) tokens: String,
    pub(crate) micros: String,
    pub(crate) ttl_ms: u64,
}

pub(crate) struct WouldExceedArgs<'a> {
    pub(crate) project_key: &'a str,
    pub(crate) member_key: &'a str,
    pub(crate) now_ms: u64,
    pub(crate) bucket_ms: u64,
    /// Narrowest first, and the script relies on it: see [`WOULD_EXCEED`].
    pub(crate) windows: [WindowArgs; 3],
}

/// The two scripts, compiled once per ledger.
pub(crate) struct Scripts {
    record_draw: redis::Script,
    would_exceed: redis::Script,
}

impl Scripts {
    pub(crate) fn new() -> Self {
        Self {
            record_draw: redis::Script::new(RECORD_DRAW),
            would_exceed: redis::Script::new(WOULD_EXCEED),
        }
    }

    pub(crate) async fn record_draw(
        &self,
        conn: &mut ConnectionManager,
        args: RecordDrawArgs<'_>,
    ) -> Result<(), FairUseError> {
        let reply: Vec<Value> = self
            .record_draw
            .key(args.project_key)
            .key(args.member_key)
            .arg(args.at_ms)
            .arg(args.bucket_ms)
            .arg(args.tokens.as_str())
            .arg(args.micros.as_str())
            .arg(args.ttl_ms)
            .invoke_async(conn)
            .await
            .map_err(backend)?;
        match tag_of(&reply) {
            Some("OK") => Ok(()),
            _ => Err(unexpected(&reply)),
        }
    }

    pub(crate) async fn would_exceed(
        &self,
        conn: &mut ConnectionManager,
        args: WouldExceedArgs<'_>,
    ) -> Result<Option<FairUseRefusal>, FairUseError> {
        let mut invocation = self.would_exceed.prepare_invoke();
        invocation
            .key(args.project_key)
            .key(args.member_key)
            .arg(args.now_ms)
            .arg(args.bucket_ms)
            .arg(FairUseScope::Project.wire_name())
            .arg(FairUseScope::Member.wire_name())
            .arg(FairUseQuantity::Tokens.wire_name())
            .arg(FairUseQuantity::Usd.wire_name());
        for window in &args.windows {
            invocation.arg(window.span_ms).arg(window.name);
            for scope in [&window.project, &window.member] {
                invocation
                    .arg(scope.flag())
                    .arg(scope.max_tokens.as_str())
                    .arg(scope.max_micros.as_str());
            }
        }
        let reply: Vec<Value> = invocation.invoke_async(conn).await.map_err(backend)?;

        match tag_of(&reply) {
            Some("NONE") => Ok(None),
            Some("REFUSED") => {
                // Decoded exhaustively and with no fallback arm. A refusal
                // whose scope or window could not be read is not a refusal to
                // downgrade into "served" — that is the ceiling silently
                // ceasing to exist, which is the failure this whole seam is
                // about.
                let (Some(scope), Some(window), Some(quantity), Some(retry_at_ms)) = (
                    str_at(&reply, 1).and_then(scope_named),
                    str_at(&reply, 2).and_then(window_named),
                    str_at(&reply, 3).and_then(quantity_named),
                    int_at(&reply, 4),
                ) else {
                    return Err(unexpected(&reply));
                };
                Ok(Some(FairUseRefusal {
                    scope,
                    window,
                    quantity,
                    retry_at_ms,
                }))
            }
            _ => Err(unexpected(&reply)),
        }
    }
}

/// The three lookups below close the loop on the vocabulary the script was
/// handed: the names go out from `wire_name` and come back through it, so a
/// renamed variant cannot leave a stale literal behind in either direction —
/// there is no literal to leave.
fn scope_named(name: &str) -> Option<FairUseScope> {
    [FairUseScope::Project, FairUseScope::Member]
        .into_iter()
        .find(|scope| scope.wire_name() == name)
}

fn window_named(name: &str) -> Option<FairUseWindow> {
    FairUseWindow::ALL
        .into_iter()
        .find(|window| window.wire_name() == name)
}

fn quantity_named(name: &str) -> Option<FairUseQuantity> {
    [FairUseQuantity::Tokens, FairUseQuantity::Usd]
        .into_iter()
        .find(|quantity| quantity.wire_name() == name)
}

fn tag_of(reply: &[Value]) -> Option<&str> {
    str_at(reply, 0)
}

fn str_at(reply: &[Value], index: usize) -> Option<&str> {
    match reply.get(index)? {
        Value::BulkString(bytes) => std::str::from_utf8(bytes).ok(),
        Value::SimpleString(text) => Some(text),
        _ => None,
    }
}

fn int_at(reply: &[Value], index: usize) -> Option<u64> {
    match reply.get(index)? {
        Value::Int(number) => u64::try_from(*number).ok(),
        _ => None,
    }
}

fn unexpected(reply: &[Value]) -> FairUseError {
    FairUseError::Backend(anyhow::anyhow!(
        "fair-use ledger script returned an unexpected reply: {reply:?}"
    ))
}

fn backend(error: redis::RedisError) -> FairUseError {
    FairUseError::Backend(anyhow::Error::new(error))
}
