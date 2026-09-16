// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The fair-use write and read paths: two Lua scripts over one shared prelude,
//! and their reply decoding, in the same idiom as [`crate::scripts`] and
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
//! **One prelude, two scripts, and that is load-bearing rather than tidy.**
//! `decay` — the function that ages a running sum forward — is called by both
//! the write and the read (see [`DECAY`]'s own comment for what each asks of
//! it), and the two must age a sum *identically* or a draw and the check that
//! follows it would disagree about which buckets a window still holds. Two
//! copies of that arithmetic in two script literals is the drift this
//! composition removes; the cost is that a script's text is assembled at
//! [`Scripts::new`] rather than being one `const`, which is why
//! [`would_exceed_source`] hands out the assembled text rather than a literal.
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
//! literal that could drift from the enum. The window name is also the field
//! name its running sum lives under, which is the same fact used twice rather
//! than two facts that could disagree. The Rust side still parses the names
//! back exhaustively, because a reply that named a scope no enum has is a
//! failure to report rather than a value to guess at.

use std::sync::LazyLock;

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
    WOULD_EXCEED.as_str()
}

/// The arithmetic both scripts share.
///
/// # The layout it operates on
///
/// One hash per scope. Two field families live in it:
///
/// | Field | Holds |
/// |---|---|
/// | `b:<index>:t`, `b:<index>:u` | one bucket's tokens and micro-dollars |
/// | `s:<window>:t`, `s:<window>:u` | that window's running sum |
/// | `s:<window>:from`, `s:<window>:to` | the oldest and newest bucket index the sum includes |
/// | `mark` | the scope's clock: the newest time any caller has handed it |
///
/// # The mark, which is what "now" means here (M13.1 review, R-F9)
///
/// A scope's clock is the high-water mark of every `at_ms` and `now_ms` it has
/// been handed, and every window is aged and compared at *that* time rather
/// than at whatever the current call supplied. It has to be, because the decay
/// is owned by the read and is therefore irreversible: a check that ages a
/// bucket out deletes it, and a later check whose clock stepped back one
/// millisecond would read the hole and admit a turn the memory ledger — which
/// re-sums what it still holds — refuses (F9). Evaluating at the mark makes
/// both ledgers deterministic functions of (the draws, the mark), so no
/// admission can be made more permissive by a clock going backwards, and it is
/// what lets the retry walk reach a bucket stamped ahead of the checking
/// node's own clock (F8) — under the mark, `to` can never exceed `now_index`,
/// because the draw that set `to` advanced the mark past it in the same write.
///
/// One field per scope, read in the same `HMGET` that reads a window's sum and
/// written at most once per script run, so the mark costs no round trip and
/// no extra read; a check whose clock advances the mark pays one `HSET` for it.
/// A scope that has never been drawn against has no mark and nothing to
/// forget, so nothing is written for it — which is also what keeps a check
/// from creating a hash that no `PEXPIRE` has ever been armed on.
///
/// # `decay`, and why `to` is stored rather than inferred
///
/// A running sum is only usable if something ages the draws that have left
/// the window back out of it. `decay` is that something, and it is written
/// once here because both scripts need it to age a sum *the same way*.
///
/// **It computes and never writes.** The two writes a decay can imply are its
/// callers' to compose: `prune_decayed` deletes the bucket fields it aged out,
/// and `persist_sum` writes the moved sum back. That is a split rather than
/// the two boolean modes this started with (M13.1 review, F7) — the draw
/// prunes without persisting, because its own `HSET` already carries these
/// fields, and the check persists and prunes only at the widest window; a
/// `persist` flag and a `name` that only one of the two callers ever used made
/// the signature ten parameters wide and hid which caller reached which arm.
///
/// It has three branches, and which one runs is decided by `to` — the newest
/// bucket the sum includes:
///
/// - **`to < first`: everything the sum covers has aged out.** The sum is
///   dropped outright, with no reads at all. `to` is what makes this exact:
///   the M13.1 ruling reached the same branch from "a `from` older than the
///   whole window", which is only equivalent while draws arrive in
///   non-decreasing time order — a caller whose clock stepped backwards by
///   more than a window between two draws would have had a full window's
///   draws deleted by that rule. One extra field per window buys the branch
///   its precondition instead of assuming it. `to` is never `nil` here: both
///   scripts write the four fields of a window's sum as a set and delete them
///   as a set, so a `from` that survived the early return above has a `to`
///   beside it — the arm that guarded the other case was unreachable and is
///   gone (M13.1 review, F7), with
///   `a_windows_four_sum_fields_move_as_a_set` holding the premise.
/// - **the gap is no wider than the window, and the sum is inside the
///   domain**: read the fields that aged out, subtract them, floored at zero
///   exactly as the memory ledger's walk floors. This is the steady state,
///   and it reads the handful of buckets that elapsed since the last touch.
/// - **otherwise: rebuild the sum from the window's own buckets.** Two cases
///   land here, and both would be wrong to subtract. A gap wider than the
///   window would be an unbounded read where rebuilding is bounded by the
///   window's own width; and a sum sitting at `MAX_COUNT` has forgotten how
///   much it saturated *by*, so subtracting an aged-out bucket from it would
///   take a still-full window to nearly empty — a ceiling a big enough draw
///   walks straight through, and a disagreement with the memory ledger, which
///   re-sums its buckets every time and therefore stays at the ceiling.
///
/// Every bucket range a decay reads is bounded by one window's width by
/// construction, but it is *not* one `HMGET`: `read_buckets`/`drop_buckets`
/// chunk at `CHUNK` fields per command, because `unpack` is limited by Lua's C
/// stack (`LUAI_MAXCSTACK`, 8000 in Redis) and a range assembled into one call
/// would sit within a factor of two of that at today's widest window and past
/// it the moment a wider one is added. So the worst case is one command per
/// chunk — six for the seven-day window's 2016 buckets — and the steady state
/// is the handful of buckets that elapsed since the last touch, in one. The
/// module doc states the same bound; it said "one `HMGET`" until M13.1's
/// review measured seven (F7).
///
/// # The pruning pass, owned
///
/// `prune_decayed` deletes the bucket fields the decay just aged out, and it
/// is called for the *widest* window only — those fields are outside every
/// window, so nothing can still need them. Which is why `record_draw` decays
/// the widest window on every draw even though it compares nothing: that is
/// the one pass guaranteed to run, and without it a membership capped only on
/// a narrow window would accumulate bucket fields forever. It is bounded by
/// the same window width every other read here is: each draw advances the
/// widest window's `from`, so the fields left to delete are at most one
/// window's worth.
///
/// **Deleting cannot make a later decay under-count**, which is the one thing
/// a prune has to promise a subtraction. A prune deletes exactly the range the
/// decay that ran it just advanced `from` past, so every subsequent range —
/// for that window, and a fortiori for the narrower ones, whose `from` is
/// always newer — starts where the last prune stopped. A subtraction therefore
/// never reads a field a prune took away and never mistakes a deleted bucket
/// for an empty one.
const DECAY: &str = r"
local function fmt(n) return string.format('%d', n) end

-- Clamped after every addition, exactly as the memory ledger's `add_count`
-- is: a running sum that never leaves the domain is a running sum every
-- addition to which is exact in a double.
local function add(sum, value, max_count)
  sum = sum + value
  if sum > max_count then sum = max_count end
  return sum
end

local function sum_fields(name)
  return 's:' .. name .. ':t', 's:' .. name .. ':u',
         's:' .. name .. ':from', 's:' .. name .. ':to'
end

-- The scope's clock. One field, one spelling, both scripts.
local MARK = 'mark'

-- The oldest bucket index a window covers at `now_ms`. Floor division from a
-- start clamped at the epoch, which is what the memory ledger's saturating
-- subtraction does -- and the flooring is what includes the partially
-- overlapping trailing bucket whole.
local function window_first(now_ms, span_ms, bucket_ms)
  local start = now_ms - span_ms
  if start < 0 then start = 0 end
  return math.floor(start / bucket_ms)
end

-- At most this many buckets -- twice as many field names -- per command. See
-- the Rust doc above: `unpack` is bounded by Lua's C stack, and one call per
-- range would be a landmine for whoever adds a wider window.
local CHUNK = 400

local function bucket_names(args, lo, hi)
  for index = lo, hi do
    local name = 'b:' .. fmt(index)
    args[#args + 1] = name .. ':t'
    args[#args + 1] = name .. ':u'
  end
  return args
end

-- The bucket fields over [lo, hi], flat and in index order: 2 values per
-- bucket, nil where no draw ever landed in it.
local function read_buckets(key, lo, hi)
  local out = {}
  local at = lo
  while at <= hi do
    local upto = at + CHUNK - 1
    if upto > hi then upto = hi end
    local got = redis.call('HMGET', unpack(bucket_names({key}, at, upto)))
    for i = 1, #got do out[#out + 1] = got[i] end
    at = upto + 1
  end
  return out
end

local function drop_buckets(key, lo, hi)
  local at = lo
  while at <= hi do
    local upto = at + CHUNK - 1
    if upto > hi then upto = hi end
    redis.call('HDEL', unpack(bucket_names({key}, at, upto)))
    at = upto + 1
  end
end

-- Age one window's running sum forward so that it covers exactly the buckets
-- from `first`. `state` is {t, u, from, to} as read out of the hash; the four
-- are returned moved, and `from`/`to` come back nil when nothing is left.
--
-- Nothing is written here. The fifth return is the newest bucket index that
-- aged out, or nil when nothing did -- which is both what `prune_decayed`
-- deletes and the caller's signal that there is nothing to persist either.
local function decay(key, state, span_ms, bucket_ms, first, max_count)
  -- A window nothing has ever drawn against reads back as four nils. It
  -- returns as a sum of zero rather than as nils, because the caller compares
  -- it against a cap -- and a cap of zero refuses an empty window, which is
  -- the one configuration where an untouched sum still has to be a number.
  local t, u, from, to = state[1] or 0, state[2] or 0, state[3], state[4]
  if from == nil or from >= first then return t, u, from, to, nil end
  local last = first - 1
  local aged = last
  if to < first then
    -- Nothing the sum covers is inside the window any more.
    if to < aged then aged = to end
    t, u, from, to = 0, 0, nil, nil
  elseif (first - from) <= math.floor(span_ms / bucket_ms) + 1
     and t < max_count and u < max_count then
    local got = read_buckets(key, from, last)
    local i = 1
    for _ = from, last do
      t = t - (tonumber(got[i]) or 0)
      u = u - (tonumber(got[i + 1]) or 0)
      if t < 0 then t = 0 end
      if u < 0 then u = 0 end
      i = i + 2
    end
    from = first
  else
    -- `to` needs no clamp to the caller's clock: every draw advanced the mark
    -- past its own bucket, and every caller evaluates at the mark, so the
    -- newest bucket the sum covers is never in the future of `first`'s clock.
    local got = read_buckets(key, first, to)
    t, u = 0, 0
    local i = 1
    for _ = first, to do
      t = add(t, tonumber(got[i]) or 0, max_count)
      u = add(u, tonumber(got[i + 1]) or 0, max_count)
      i = i + 2
    end
    from = first
  end
  return t, u, from, to, aged
end

-- The pruning pass: delete the bucket fields a decay just aged out. Only the
-- widest window's caller does this, and only it may.
local function prune_decayed(key, state, aged)
  if aged ~= nil then drop_buckets(key, state[3], aged) end
end

-- Write a decayed sum back, or delete it when nothing is left. The check's
-- half of the split: a draw's own HSET already carries these fields.
local function persist_sum(key, name, t, u, from, to)
  local ft, fu, ff, fto = sum_fields(name)
  if from == nil then
    redis.call('HDEL', key, ft, fu, ff, fto)
  else
    redis.call('HSET', key, ft, fmt(t), fu, fmt(u), ff, fmt(from), fto, fmt(to))
  end
end
";

/// `KEYS[1]` the project scope's hash, `KEYS[2]` the member's.
///
/// `ARGV[1]` at_ms, `ARGV[2]` bucket_ms, `ARGV[3]` tokens, `ARGV[4]`
/// micro-dollars, `ARGV[5]` `MAX_COUNT`, `ARGV[6]` the hash TTL in ms, then
/// one pair per window *narrowest first*: span_ms and the window's name.
///
/// Both scopes' counters and both expiries, in one indivisible step. Atomicity
/// is not decorative here: the member ceiling and the project ceiling are two
/// counters over *one* draw, and an update that moved one and lost the other
/// would leave a member enforced against a project's counter — which is not a
/// member ceiling.
///
/// **One `HMGET` and one `HSET` per scope, whatever the window count.** The
/// bucket's two fields and every window's four are read together and written
/// back together, because they are one fact — this draw — recorded in the two
/// shapes the read path needs: the per-bucket amount, which is what a decay
/// subtracts and a retry walk drops, and the per-window running sum, which is
/// what a ceiling check compares against a cap without reading a bucket at
/// all.
///
/// **Read-add-write rather than `HINCRBY`, and that is what makes
/// "indivisible" true rather than aspirational.** Redis runs a script
/// atomically but does *not* roll back the writes it already made when a later
/// command errors — so the `HINCRBY` pair this started as could overflow on the
/// member's bucket after the project's had already moved and re-armed its TTL,
/// which is a half-applied draw wearing a refusal's clothes (M13 review, F5).
/// Reading the fields, adding with a clamp at `MAX_COUNT` and writing them
/// back has no failing command in it: the saturation replaces the error, the
/// out-of-domain draw is refused in Rust before the script runs, and the only
/// remaining outcomes are "both scopes moved" and "the script never ran".
///
/// `from` is lowered and `to` raised to admit this draw's bucket, so the sum
/// always names the exact stretch of buckets it covers — except where the
/// draw's bucket is older than the window's own first bucket *at the mark*,
/// which is outside the window and so joins no sum at all. The field names are
/// built here rather than passed in, so the caller cannot send `at_ms` and a
/// bucket index that disagree; both keys carry the project's Redis Cluster
/// hash tag, so every key this touches is in one slot.
///
/// `PEXPIRE` on the scope hash on every write rather than only on creation: a
/// scope's lifetime is measured from its last draw, which is the cheap
/// direction to be wrong in (an idle scope's hash is never read, so an
/// over-long TTL costs storage and never correctness) and needs no `TTL` read
/// to decide.
const RECORD_DRAW_BODY: &str = r"
local at_ms = tonumber(ARGV[1])
local bucket_ms = tonumber(ARGV[2])
local tokens = tonumber(ARGV[3])
local micros = tonumber(ARGV[4])
local max_count = tonumber(ARGV[5])
local header, group = 6, 2
-- How many windows there are is read off ARGV, never written down here: the
-- Rust side sizes its list from `FairUseWindow::ALL`, so a window added to
-- that enum reaches this loop rather than being silently unsummed.
local windows = (#ARGV - header) / group
local index = math.floor(at_ms / bucket_ms)
local bt, bu = 'b:' .. fmt(index) .. ':t', 'b:' .. fmt(index) .. ':u'

for s = 1, 2 do
  local key = KEYS[s]
  local names = {key, bt, bu, MARK}
  for w = 1, windows do
    local ft, fu, ff, fto = sum_fields(ARGV[header + (w - 1) * group + 2])
    names[#names + 1] = ft
    names[#names + 1] = fu
    names[#names + 1] = ff
    names[#names + 1] = fto
  end
  local got = redis.call('HMGET', unpack(names))

  -- The scope's clock, advanced by this draw if it is the newest time this
  -- scope has seen. A draw stamped *behind* the mark is still recorded in its
  -- own bucket -- it happened -- but the windows are judged at the mark, so
  -- such a draw cannot widen one backwards. A draw behind it by more than the
  -- widest window is therefore counted by nothing, and its bucket field sits
  -- below every `from` the pruning walk starts at: the memory ledger's prune
  -- drops the same bucket outright, and here it is left to the hash's own
  -- expiry rather than paid for with an unbounded delete range.
  local mark = tonumber(got[3]) or 0
  if at_ms > mark then mark = at_ms end

  local writes = {key}
  writes[#writes + 1] = bt
  writes[#writes + 1] = fmt(add(tonumber(got[1]) or 0, tokens, max_count))
  writes[#writes + 1] = bu
  writes[#writes + 1] = fmt(add(tonumber(got[2]) or 0, micros, max_count))
  writes[#writes + 1] = MARK
  writes[#writes + 1] = fmt(mark)
  local deletes = {key}

  for w = 1, windows do
    local base = header + (w - 1) * group
    local span_ms = tonumber(ARGV[base + 1])
    local name = ARGV[base + 2]
    local at = 3 + (w - 1) * 4
    local state = {tonumber(got[at + 1]), tonumber(got[at + 2]),
                   tonumber(got[at + 3]), tonumber(got[at + 4])}
    local t, u, from, to = state[1] or 0, state[2] or 0, state[3], state[4]
    local first = window_first(mark, span_ms, bucket_ms)
    if w == windows then
      -- The pruning pass, owned. The widest window is the one whose aged-out
      -- buckets are outside every window, and this is the call that is
      -- guaranteed to run: a membership capped only on the 5-hour window
      -- would otherwise never ask the 7-day window anything and would keep
      -- every bucket field it ever wrote. Nothing is persisted here because
      -- the write below carries these same fields.
      local aged
      t, u, from, to, aged = decay(key, state, span_ms, bucket_ms, first, max_count)
      prune_decayed(key, state, aged)
    end
    -- A draw older than this window's first bucket *at the mark* is outside
    -- it, so it neither adds to the sum nor drags `from` back over ground a
    -- decay already subtracted -- which is what made a later decay subtract
    -- an aged-out bucket twice (M13.1 review, F6). The memory ledger reaches
    -- the same answer from the other side: its window is a range from that
    -- same first bucket, so a draw below it is simply not in the range.
    if index >= first then
      t = add(t, tokens, max_count)
      u = add(u, micros, max_count)
      if from == nil or index < from then from = index end
      if to == nil or index > to then to = index end
    end
    local ft, fu, ff, fto = sum_fields(name)
    if from ~= nil then
      writes[#writes + 1] = ft
      writes[#writes + 1] = fmt(t)
      writes[#writes + 1] = fu
      writes[#writes + 1] = fmt(u)
      writes[#writes + 1] = ff
      writes[#writes + 1] = fmt(from)
      writes[#writes + 1] = fto
      writes[#writes + 1] = fmt(to)
    elseif state[3] ~= nil then
      -- The sum this draw found had aged out entirely and the draw itself is
      -- older than the window: the four fields go together or not at all, so
      -- the window is left with no sum rather than a stale one.
      deletes[#deletes + 1] = ft
      deletes[#deletes + 1] = fu
      deletes[#deletes + 1] = ff
      deletes[#deletes + 1] = fto
    end
  end

  redis.call('HSET', unpack(writes))
  if #deletes > 1 then redis.call('HDEL', unpack(deletes)) end
  redis.call('PEXPIRE', key, ARGV[6])
end
return {'OK'}
";

/// `KEYS[1]` the project scope's hash, `KEYS[2]` the member's.
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
/// **What it does *not* do any more is scan buckets to answer the question**
/// (M13.1). The sum a cap is compared against was maintained by
/// `record_draw`; all this has to do is age it forward — one `HMGET` of the
/// window's four sum fields and the scope's mark, plus, in the steady state,
/// nothing at all — which is what turns an
/// admitted turn from 2017 reads per capped scope into one. The bucket walk
/// survives in exactly one place: computing the earliest retry time on a
/// *refusal*, where the answer is which bucket has to leave and no running sum
/// can say. A refusal is the rare case and the one where a client is waiting
/// on a number rather than on a turn.
///
/// **This script writes, and that is what makes the cost amortised rather
/// than merely moved.** The decay is persisted — the sum, its `from` and its
/// `to`, and for the widest window the bucket fields it deleted — so the work
/// of ageing one bucket out is done once by whichever admission first looks
/// past it, not again by every admission after. The consequence to know is
/// that a ceiling check is not a read-only command: it cannot be served by a
/// replica, and it is one more reason the two operations are scripts rather
/// than pipelines, since a decay interleaved with a concurrent draw would
/// otherwise subtract from a sum the draw had already moved. It also advances
/// the scope's mark, once per run and only when the caller's clock is ahead of
/// it — which is what makes the persisted decay safe rather than a hazard: the
/// state a check destroys can never be asked for again by a clock that steps
/// back, because the mark is what the next check is evaluated at (R-F9).
///
/// Empty buckets in that walk are dropped on the way through. That is not a
/// filter on the arithmetic: a bucket of zeroes changes no sum and can never
/// be the bucket whose departure brings a window under its cap, because
/// dropping zero from an over-cap total leaves it over.
const WOULD_EXCEED_BODY: &str = r"
local now_ms = tonumber(ARGV[1])
local bucket_ms = tonumber(ARGV[2])
local scope_names = {ARGV[3], ARGV[4]}
local name_tokens, name_usd = ARGV[5], ARGV[6]
local max_count = tonumber(ARGV[7])
-- The header above, and eight ARGV per window after it: the number of windows
-- is whatever the caller sent, not a number written down here.
local header, group = 7, 8
local count = (#ARGV - header) / group
-- Each scope's clock, resolved the first time a window asks that scope
-- anything and reused by every window after it, so one check is one instant
-- however many windows it walks.
local clocks = {}

for w = 1, count do
  local base = header + (w - 1) * group
  local span_ms = tonumber(ARGV[base + 1])
  local name = ARGV[base + 2]
  for s = 1, 2 do
    local sbase = base + 2 + (s - 1) * 3
    if ARGV[sbase + 1] == '1' then
      local key = KEYS[s]
      local ft, fu, ff, fto = sum_fields(name)
      local got = redis.call('HMGET', key, ft, fu, ff, fto, MARK)
      local clock = clocks[s]
      if clock == nil then
        -- A scope never drawn against has no mark: every sum is empty, so
        -- there is no state a later call could disagree about and nothing
        -- worth writing -- and a mark written here would leave a hash no
        -- PEXPIRE had ever been armed on.
        local mark = tonumber(got[5])
        clock = now_ms
        if mark ~= nil then
          if mark > now_ms then
            clock = mark
          elseif mark < now_ms then
            redis.call('HSET', key, MARK, fmt(now_ms))
          end
        end
        clocks[s] = clock
      end
      local first = window_first(clock, span_ms, bucket_ms)
      local now_index = math.floor(clock / bucket_ms)
      local state = {tonumber(got[1]), tonumber(got[2]), tonumber(got[3]), tonumber(got[4])}
      local t, u, from, to, aged = decay(key, state, span_ms, bucket_ms, first, max_count)
      if aged ~= nil then
        if w == count then prune_decayed(key, state, aged) end
        persist_sum(key, name, t, u, from, to)
      end
      local max_tokens = tonumber(ARGV[sbase + 2])
      local max_micros = tonumber(ARGV[sbase + 3])
      local function over(tt, uu)
        return (max_tokens ~= nil and tt >= max_tokens)
            or (max_micros ~= nil and uu >= max_micros)
      end
      if over(t, u) then
        -- Tokens before dollars where both are capped: the token cap is the
        -- one an agent can reason about, because it is the quantity in its
        -- own context.
        local quantity = name_usd
        if max_tokens ~= nil and t >= max_tokens then quantity = name_tokens end
        -- Walk the buckets the sum covers, oldest first, dropping each in
        -- turn until what remains is under every cap; the answer is when that
        -- bucket's end leaves the window. Every bucket dropped and still over
        -- is only reachable with a cap of zero or below -- a window that can
        -- never have room -- and is answered with the clock rather than a lie
        -- about the future, exactly as the memory ledger's `None` is.
        local retry = clock
        local walk = from
        if walk == nil then walk = now_index + 1 end
        local buckets = read_buckets(key, walk, now_index)
        local i = 1
        for index = walk, now_index do
          -- Floored at zero, the saturating subtraction the memory ledger's
          -- walk does -- which is what keeps the two walks identical once a
          -- sum has saturated.
          t = t - (tonumber(buckets[i]) or 0)
          if t < 0 then t = 0 end
          u = u - (tonumber(buckets[i + 1]) or 0)
          if u < 0 then u = 0 end
          i = i + 2
          if not over(t, u) then
            retry = (index + 1) * bucket_ms + span_ms
            break
          end
        end
        return {'REFUSED', scope_names[s], name, quantity, retry}
      end
    end
  end
end
return {'NONE'}
";

/// The two scripts' assembled text: the shared prelude, then the body.
///
/// A `LazyLock` rather than a `const` because `concat!` takes literals and
/// these are two `const`s; assembling once per process keeps
/// [`would_exceed_source`] able to hand out `&'static str`, which is what
/// lets the gated test invoke *what ships* rather than a copy.
static RECORD_DRAW: LazyLock<String> = LazyLock::new(|| format!("{DECAY}{RECORD_DRAW_BODY}"));
static WOULD_EXCEED: LazyLock<String> = LazyLock::new(|| format!("{DECAY}{WOULD_EXCEED_BODY}"));

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

/// One window, as both scripts need it: how wide it is and what its running
/// sum is filed under.
pub(crate) struct WindowSpan {
    pub(crate) span_ms: u64,
    /// The window's wire name, which is also the field name its running sum
    /// lives under. One fact used twice rather than two that could disagree.
    pub(crate) name: &'static str,
}

/// One window *and the caps to judge it against*, which only the check has.
///
/// Two types rather than one with cap fields a draw fills with dummies
/// (M13.1 review, F3): `record_draw` reads neither cap, so every draw was
/// building two `ScopeCaps::absent()` per window for a script that ignores
/// them, and a unit test existed to assert the dummies were dummies. What
/// stops a caps field reaching the draw path now is that there is none to
/// send.
pub(crate) struct WindowCaps {
    pub(crate) span: WindowSpan,
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
    /// Every window, narrowest first — *not* only the configured ones.
    ///
    /// A draw has no terms: it moves the sums of every window the vocabulary
    /// names, because the ceiling a later admission is judged against is read
    /// off a live control plane and may name a window nobody had configured
    /// when this draw landed. Narrowest-first matters here too — the last
    /// group is the widest, and the widest is the one whose decay prunes.
    pub(crate) windows: [WindowSpan; FairUseWindow::ALL.len()],
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
    pub(crate) windows: [WindowCaps; FairUseWindow::ALL.len()],
}

/// The two scripts, compiled once per ledger.
pub(crate) struct Scripts {
    record_draw: redis::Script,
    would_exceed: redis::Script,
}

impl Scripts {
    pub(crate) fn new() -> Self {
        Self {
            record_draw: redis::Script::new(&RECORD_DRAW),
            would_exceed: redis::Script::new(&WOULD_EXCEED),
        }
    }

    pub(crate) async fn record_draw(
        &self,
        conn: &mut ConnectionManager,
        args: RecordDrawArgs<'_>,
    ) -> Result<(), FairUseError> {
        let mut invocation = self.record_draw.prepare_invoke();
        invocation
            .key(args.project_key)
            .key(args.member_key)
            .arg(args.at_ms)
            .arg(args.bucket_ms)
            .arg(args.counts.tokens)
            .arg(args.counts.micros)
            .arg(args.max_count)
            .arg(args.ttl_ms);
        for window in &args.windows {
            invocation.arg(window.span_ms).arg(window.name);
        }
        let reply: Vec<Value> = invocation.invoke_async(conn).await.map_err(backend)?;
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
            invocation.arg(window.span.span_ms).arg(window.span.name);
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
