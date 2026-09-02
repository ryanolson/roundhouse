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
//! **Counters are integers in one bounded domain, and the trait's `f64` is
//! converted once at the Rust edge** by
//! `roundhouse_core::control::fair_use::DrawCounts::of`. Dollars cross as
//! micro-dollars, and both counts cross as ordinary integer `ARGV` — the
//! `redis` crate formats an integer argument with `itoa`, which is the same
//! decimal bytes a `String` would have carried, so the decimal-string plumbing
//! this started with was never load-bearing on the wire.
//!
//! What *is* load-bearing is the domain. Every number here is read back with
//! `tonumber` and added with Lua's `+`, and a Lua number is a double: exactness
//! is a property of staying at or below 2^53, not of how the argument was
//! spelled. So `MAX_COUNT` is passed in and every sum is clamped to it after
//! each addition — which makes the clamp exact (a sum that leaves the domain
//! can only round further out, never back under) and needs no overflow error
//! path at all. The counts a script writes back are formatted with
//! `string.format('%d', …)`, because Lua 5.1's own number-to-string conversion
//! is `%.14g` and would turn a large count into scientific notation.
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

use roundhouse_core::control::fair_use::DrawCounts;
use roundhouse_core::control::{
    FairUseError, FairUseQuantity, FairUseRefusal, FairUseScope, FairUseWindow,
};

/// The `would_exceed` script's own text, for the one test that must invoke it
/// with an argument list [`WouldExceedArgs`] cannot express — a window group
/// past the ones `FairUseWindow::ALL` names.
///
/// A seam rather than a copy in the test file: a copy of a script is a second
/// spelling that drifts, and a test asserting a property of a stale copy
/// asserts nothing about what ships.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn would_exceed_source() -> &'static str {
    WOULD_EXCEED
}

/// `KEYS[1]` the project scope's bucket-key prefix, `KEYS[2]` the member's.
/// `ARGV`: at_ms, bucket_ms, tokens, micro-dollars, MAX_COUNT, bucket TTL in
/// ms.
///
/// Both scopes' counters and both expiries, in one indivisible step. Atomicity
/// is not decorative here: the member ceiling and the project ceiling are two
/// counters over *one* draw, and an update that moved one and lost the other
/// would leave a member enforced against a project's counter — which is not a
/// member ceiling.
///
/// **Read-add-write rather than `HINCRBY`, and that is what makes
/// "indivisible" true rather than aspirational.** Redis runs a script
/// atomically but does *not* roll back the writes it already made when a later
/// command errors — so the `HINCRBY` pair this started as could overflow on the
/// member's bucket after the project's had already moved and re-armed its TTL,
/// which is a half-applied draw wearing a refusal's clothes (M13 review, F5).
/// Reading the two fields, adding with a clamp at `MAX_COUNT` and writing them
/// back has no failing command in it: the saturation replaces the error, the
/// out-of-domain draw is refused in Rust before the script runs, and the only
/// remaining outcomes are "both scopes moved" and "the script never ran".
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
local tokens = tonumber(ARGV[3])
local micros = tonumber(ARGV[4])
local max_count = tonumber(ARGV[5])
for i = 1, 2 do
  local key = KEYS[i] .. ':' .. string.format('%d', index)
  local pair = redis.call('HMGET', key, 't', 'u')
  local t = (tonumber(pair[1]) or 0) + tokens
  local u = (tonumber(pair[2]) or 0) + micros
  if t > max_count then t = max_count end
  if u > max_count then u = max_count end
  redis.call('HSET', key, 't', string.format('%d', t), 'u', string.format('%d', u))
  redis.call('PEXPIRE', key, ARGV[6])
end
return {'OK'}
";

/// `KEYS[1]` the project scope's bucket-key prefix, `KEYS[2]` the member's.
///
/// `ARGV[1]` now_ms, `ARGV[2]` bucket_ms, `ARGV[3..4]` the two scope names,
/// `ARGV[5..6]` the two quantity names, `ARGV[7]` `MAX_COUNT`, then one group
/// of eight per window, *narrowest first* — each holding: span_ms, the
/// window's name, and for each scope in turn a present flag (`'1'`/`'0'`), a
/// token cap and a micro-dollar cap (`''` for "not capped on this quantity").
///
/// **How many groups there are is read off `ARGV`, never written down here.**
/// The count was `for w = 1, 3` while the Rust side derived its array from
/// `FairUseWindow::ALL`, so widening that enum would have compiled and left
/// the new — typically widest — window silently unsummed, returning `NONE`
/// where the memory ledger refuses (M13 review, F6). Deriving the bound from
/// the argument list means the two cannot drift: whatever the caller sends is
/// what gets checked.
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
/// most 2017 reads per scope.
///
/// **That ceiling, not 61, is what an admission pays (M13 review, F4).**
/// Short-circuiting happens on a *refusal* — a window binding — and an
/// admitted turn is the case where no window ever binds, so the scan never
/// gets to stop early: it widens through every present window to the widest
/// one. Admission is the common case a fleet actually serves, so the common
/// cost is up to 2017 reads per scope, not the 61 a refusal at the narrowest
/// window would cost. See `fair_use`'s module doc ("Bucket-per-key makes
/// Redis the sweeper") for the measured cost and the M13.1 rung that
/// replaces this scan.
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
local max_count = tonumber(ARGV[7])
-- The header above, and eight ARGV per window after it: the number of windows
-- is whatever the caller sent, not a number written down here.
local header, group = 7, 8
local now_index = math.floor(now_ms / bucket_ms)

-- Clamped after every addition, exactly as the memory ledger's `add_count`
-- is: a running sum that never leaves the domain is a running sum every
-- addition to which is exact in a double.
local function add(sum, value)
  sum = sum + value
  if sum > max_count then sum = max_count end
  return sum
end

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
      tokens = add(tokens, list[i][2])
      micros = add(micros, list[i][3])
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
      -- Floored at zero, the saturating subtraction the memory ledger's walk
      -- does -- which is what keeps the two walks identical once a sum has
      -- saturated.
      tokens = tokens - list[i][2]
      if tokens < 0 then tokens = 0 end
      micros = micros - list[i][3]
      if micros < 0 then micros = 0 end
      if not over(tokens, micros) then
        retry = (list[i][1] + 1) * bucket_ms + span_ms
        break
      end
    end
  end
  return {quantity, retry}
end

for w = 1, (#ARGV - header) / group do
  local base = header + (w - 1) * group
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
    /// Already converted and already inside the domain: `DrawCounts` is the
    /// only way to build a pair of counts, so the script never sees a *draw*
    /// it would have to reject — the clamp it applies is to the running
    /// bucket total, which is arithmetic rather than validation.
    pub(crate) counts: DrawCounts,
    /// The domain's ceiling, handed in rather than written into the Lua, so
    /// the bound has one definition and it is core's.
    pub(crate) max_count: u64,
    pub(crate) ttl_ms: u64,
}

pub(crate) struct WouldExceedArgs<'a> {
    pub(crate) project_key: &'a str,
    pub(crate) member_key: &'a str,
    pub(crate) now_ms: u64,
    pub(crate) bucket_ms: u64,
    pub(crate) max_count: u64,
    /// Narrowest first, and the script relies on it: see [`WOULD_EXCEED`].
    ///
    /// Sized from `FairUseWindow::ALL` rather than from the literal 3, so a
    /// window added to the enum cannot reach the script as a group the caller
    /// forgot to build — and the script's own loop is bounded by what arrives,
    /// so it cannot reach the script as a group nothing reads either.
    pub(crate) windows: [WindowArgs; FairUseWindow::ALL.len()],
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
            .arg(args.counts.tokens)
            .arg(args.counts.micros)
            .arg(args.max_count)
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
            .arg(FairUseQuantity::Usd.wire_name())
            .arg(args.max_count);
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
