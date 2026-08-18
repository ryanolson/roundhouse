// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The write path for the spend ledger: three Lua scripts and their reply
//! decoding, in the same idiom as [`crate::scripts`].
//!
//! **Why `now_ms` is an argument and never `redis.call('TIME')` here, unlike
//! every script in [`crate::scripts`].** The session store's lease and log
//! need one clock authority across a fleet of writers with skewed clocks, so
//! they read the Redis server's own clock. The ledger's TTL lapse and monthly
//! reset need the opposite property: a test has to reach them without
//! sleeping, which is exactly why
//! `roundhouse_core::control::spend::SpendLedger` takes `now_ms` as data on
//! every call in the first place (see that trait's module doc). A script that
//! called `TIME` here would make the memory and Redis backends agree on
//! everything except the one property the contract suite exists to pin.
//!
//! **Every dollar amount crosses the Lua boundary as a string, never a Lua
//! number.** A Redis script's numeric return values are converted to RESP
//! *integers* — a Lua `4.5` comes back as `4`, silently. Every amount is
//! therefore formatted with fixed decimal precision (`fmtusd`, in the
//! prelude) before it is returned or stored in a hash field, and parsed back
//! with `str::parse::<f64>` on the Rust side. Only genuinely integral values
//! (`ttl_ms`, `seq`, the `member_remaining_present` flag) are left as Lua
//! numbers.
//!
//! **The calendar math is a second copy, in a second language, on purpose.**
//! `roundhouse_core::control::spend::month_start_ms` is private to that
//! module — the ledger's window boundary is not something a caller composes
//! from parts, precisely so it cannot drift from what a grant enforces. That
//! privacy is exactly what forces this file to carry its own port of Howard
//! Hinnant's civil-calendar algorithm rather than calling the Rust one.
//!
//! What keeps the two copies honest is that both are *executed* against one
//! list of boundaries,
//! [`MONTH_START_CASES`](roundhouse_core::control::spend::contract::MONTH_START_CASES):
//! the Rust port in that crate's own unit tests, this one through an embedded
//! Lua interpreter in this file's test module. That runs on every `cargo test`
//! with no Redis anywhere. Be clear about what it does *not* cover: the Redis
//! script sandbox itself — `redis.call`, the RESP conversion rules, and Lua
//! 5.1 rather than the 5.4 the embedded interpreter builds. Those belong to
//! the ignore-gated suite, where
//! `a_monthly_window_resets_committed_at_its_boundary` drives a real
//! August-18 / September-1 rollover through a real server.

use redis::Value;
use redis::aio::ConnectionManager;

use roundhouse_core::control::{LedgerState, SpendError};

/// Lua helpers shared by all three scripts: truncating integer division (the
/// civil-calendar algorithm assumes Rust's `/` on `i64`, which truncates
/// toward zero — Lua's default division is float, and `math.floor` alone
/// would round the wrong way for a negative operand), the calendar port
/// itself, fixed-precision dollar formatting, and the hold-record pack/unpack
/// used by [`OPEN_GRANT`]/[`SETTLE_GRANT`]/[`BALANCE`] alike.
///
/// Textually spliced into each script's source in [`Scripts::new`] rather
/// than shared via a Redis Lua library: each `EVAL`/`EVALSHA` is a fresh
/// sandbox with no cross-script state, so "shared" can only mean "one
/// definition in the Rust source, copied into three compiled scripts" — which
/// is what keeps the port a single place to get right instead of three.
const LUA_PRELUDE: &str = r"
local function idiv(a, b)
  local q = a / b
  if q < 0 then return math.ceil(q) else return math.floor(q) end
end

local function civil_from_days(days)
  local z = days + 719468
  local era
  if z >= 0 then era = idiv(z, 146097) else era = idiv(z - 146096, 146097) end
  local doe = z - era * 146097
  local yoe = idiv(doe - idiv(doe, 1460) + idiv(doe, 36524) - idiv(doe, 146096), 365)
  local y = yoe + era * 400
  local doy = doe - (365 * yoe + idiv(yoe, 4) - idiv(yoe, 100))
  local mp = idiv(5 * doy + 2, 153)
  local d = doy - idiv(153 * mp + 2, 5) + 1
  local m
  if mp < 10 then m = mp + 3 else m = mp - 9 end
  if m <= 2 then y = y + 1 end
  return y, m, d
end

local function days_from_civil(year, month, day)
  local y = year
  if month <= 2 then y = year - 1 end
  local era
  if y >= 0 then era = idiv(y, 400) else era = idiv(y - 399, 400) end
  local yoe = y - era * 400
  local mm
  if month > 2 then mm = month - 3 else mm = month + 9 end
  local doy = idiv(153 * mm + 2, 5) + day - 1
  local doe = yoe * 365 + idiv(yoe, 4) - idiv(yoe, 100) + doy
  return era * 146097 + doe - 719468
end

local MS_PER_DAY = 86400000

local function month_start_ms(now_ms)
  local days = math.floor(now_ms / MS_PER_DAY)
  local year, month, _day = civil_from_days(days)
  return days_from_civil(year, month, 1) * MS_PER_DAY
end

-- 'monthly' resets at the calendar boundary; anything else (only 'total' is
-- ever sent) never rolls over, mirroring `BudgetWindow::Total` reporting the
-- epoch.
local function window_start(mode, now_ms)
  if mode == 'monthly' then return month_start_ms(now_ms) else return 0 end
end

local function fmtusd(x)
  return string.format('%.10f', x)
end

-- Unit separator: not a character a project or user slug can contain, and
-- never produced by `fmtusd` or `tostring` on an integer millisecond.
local HOLD_SEP = string.char(31)

local function pack_hold(user, amount, expires_at_ms)
  return user .. HOLD_SEP .. fmtusd(amount) .. HOLD_SEP .. tostring(expires_at_ms)
end

local function unpack_hold(value)
  local a, b = string.find(value, HOLD_SEP, 1, true)
  local rest = string.sub(value, b + 1)
  local c, d = string.find(rest, HOLD_SEP, 1, true)
  local user = string.sub(value, 1, a - 1)
  local amount = tonumber(string.sub(rest, 1, c - 1))
  local expires = tonumber(string.sub(rest, d + 1))
  return user, amount, expires
end

-- Roll one project's account to `now_ms`, and read the position that leaves.
--
-- **The Lua twin of three Rust things at once**, in
-- `roundhouse_core::control::spend`: `ProjectAccount::settle_time` (the window
-- roll plus the lazy hold expiry every operation begins with), `remaining`
-- (both ceilings, each floored at zero), and `Remaining::effective` (the
-- tighter of the two). One function here because the alternative was measured
-- and rejected: the ceiling rule was copied verbatim into OPEN_GRANT and
-- BALANCE, so the rule that decides how much money a project has had two
-- spellings a hundred lines apart, and the holds hash was walked twice per
-- call -- once to expire, once to sum -- for an answer the first pass already
-- had in hand.
--
-- The crash story ('a leaked hold self-heals within one TTL, no sweeper')
-- depends on this running lazily on whichever call happens to come next, not
-- on a background task. That is why a *read* calls it too.
--
-- `limit_usd` is nil for SETTLE_GRANT, which releases a hold and applies a
-- realized amount without ever asking what remained: the remaining fields are
-- then left unset rather than computed, because a ceiling nothing reads is a
-- second answer nothing checks.
local function roll_and_read(account_key, holds_key, mode, now_ms, user, limit_usd, member_ceiling)
  local stored = tonumber(redis.call('HGET', account_key, 'window_start_ms'))
  local current = window_start(mode, now_ms)
  if stored == nil or current > stored then
    local fields = redis.call('HKEYS', account_key)
    for _, field in ipairs(fields) do
      if field == 'committed' or string.sub(field, 1, 7) == 'member:' then
        redis.call('HDEL', account_key, field)
      end
    end
    redis.call('HSET', account_key, 'window_start_ms', current)
  end

  -- One HGETALL: a lapsed hold is deleted and a live one is summed in the same
  -- pass. The sums are read fresh on every call rather than kept as running
  -- counters, because a counter would be a second number the deletions here
  -- would have to keep in lockstep -- and this hash holds one field per turn
  -- *in flight*, not per turn ever served.
  local p = {committed = 0.0, member_committed = 0.0, held = 0.0, member_held = 0.0}
  local raw = redis.call('HGETALL', holds_key)
  local i = 1
  while i <= #raw do
    local response_id = raw[i]
    local hold_user, amount, expires = unpack_hold(raw[i + 1])
    if expires <= now_ms then
      redis.call('HDEL', holds_key, response_id)
    else
      p.held = p.held + amount
      if hold_user == user then p.member_held = p.member_held + amount end
    end
    i = i + 2
  end

  -- Read after the roll above, never before: a stale window's committed spend
  -- is exactly what the roll exists to have deleted.
  p.committed = tonumber(redis.call('HGET', account_key, 'committed')) or 0.0
  p.member_committed = tonumber(redis.call('HGET', account_key, 'member:' .. user)) or 0.0

  if limit_usd ~= nil then
    p.project_remaining = limit_usd - p.committed - p.held
    if p.project_remaining < 0.0 then p.project_remaining = 0.0 end
    p.effective = p.project_remaining
    if member_ceiling ~= nil then
      p.member_remaining = member_ceiling - p.member_committed - p.member_held
      if p.member_remaining < 0.0 then p.member_remaining = 0.0 end
      if p.member_remaining < p.effective then p.effective = p.member_remaining end
    end
  end
  return p
end

-- The committed-plus-held level at which a ceiling starts warning. Both
-- ceilings warn on the same configured fraction, so both go through here --
-- the Lua twin of `Budget::warn_level_for`, and for the same reason: an inline
-- product at the member site is how one fraction becomes two thresholds.
local function warn_level(ceiling, warn_at)
  return ceiling * warn_at
end

-- Ports `roundhouse_core::control::spend::state_for` verbatim, including its
-- load-bearing asymmetry: exhaustion is judged on `available` — what there
-- was to hand out before this call's own hold, if any — while a warning is
-- judged on `project_used`/`member_used`, the position the caller passes in
-- for *after* it. A grant that took the entire remaining budget must not
-- read back as exhausted, because the router reads `exhausted` as `ceiling
-- zero` and that is what arms the overflow valve and triggers a refusal.
local function state_for(project_used, member_used, member_ceiling, limit_usd, warn_at, available)
  if available <= 0.0 then return 'exhausted' end
  local warned = project_used >= warn_level(limit_usd, warn_at)
  if member_ceiling ~= nil and member_used >= warn_level(member_ceiling, warn_at) then
    warned = true
  end
  if warned then return 'warned' else return 'unconstrained' end
end
";

/// `KEYS[1]` account, `KEYS[2]` holds.
/// `ARGV`: user, response_id, requested_usd, ttl_ms, now_ms, limit_usd,
/// member_ceiling_usd (`''` for [`Allocation::Pooled`](roundhouse_core::control::Allocation::Pooled)),
/// warn_at, window mode.
///
/// Reads both ceilings and places the hold in one round trip — the reason
/// the account and holds hashes share the project's hash tag rather than
/// being an optimization: on Redis Cluster, two keys that did not share a
/// slot could not appear in the same script at all, and a grant computed
/// from two separate round trips is exactly the read-then-write race
/// `concurrent_grants_cannot_jointly_exceed_the_limit` exists to close.
const OPEN_GRANT_BODY: &str = r"
local account_key, holds_key = KEYS[1], KEYS[2]
local user = ARGV[1]
local response_id = ARGV[2]
local requested_usd = tonumber(ARGV[3])
local now_ms = tonumber(ARGV[5])
local limit_usd = tonumber(ARGV[6])
local member_ceiling
if ARGV[7] == '' then member_ceiling = nil else member_ceiling = tonumber(ARGV[7]) end
local warn_at = tonumber(ARGV[8])
local mode = ARGV[9]
local expires_at_ms = now_ms + tonumber(ARGV[4])

-- A turn has one hold, and this response's own comes off *before* the pool is
-- read. That is what makes a re-grant under the same id (the engine never
-- issues one — a deduplicated retry short-circuits before `plan` — but a buggy
-- caller could still reach this) cost the difference rather than the whole
-- amount again, and stops it being capped by the very hold it is replacing.
redis.call('HDEL', holds_key, response_id)

local p = roll_and_read(account_key, holds_key, mode, now_ms, user, limit_usd, member_ceiling)

local granted = requested_usd
if granted > p.effective then granted = p.effective end
if granted < 0.0 then granted = 0.0 end

if granted > 0.0 then
  redis.call('HSET', holds_key, response_id, pack_hold(user, granted, expires_at_ms))
end

-- Warn is read off the position *after* this grant's own hold; exhaustion,
-- off `effective` — the position before it. See `state_for`'s comment.
local state = state_for(p.committed + p.held + granted,
                        p.member_committed + p.member_held + granted,
                        member_ceiling, limit_usd, warn_at, p.effective)

return {'OK', fmtusd(granted), state}
";

/// `KEYS[1]` account, `KEYS[2]` holds, `KEYS[3]` watermarks.
/// `ARGV`: user, session_id, seq, response_id, actual_usd, now_ms, window
/// mode.
///
/// Idempotent by `(session_id, seq)` through the watermark hash, in the same
/// round trip that releases the hold and applies the spend — the Redis half
/// of the rule `roundhouse_core::metrics::MetricsFold` states for itself.
const SETTLE_GRANT_BODY: &str = r"
local account_key, holds_key, watermarks_key = KEYS[1], KEYS[2], KEYS[3]
local user = ARGV[1]
local session_id = ARGV[2]
local seq = tonumber(ARGV[3])
local response_id = ARGV[4]
local actual_usd = tonumber(ARGV[5])
local now_ms = tonumber(ARGV[6])
local mode = ARGV[7]

-- No limit and no member ceiling: a settle applies a realized amount and
-- releases a hold, and never asks what was left. It still rolls the window and
-- expires lapsed holds, because every op does — that is the whole of the
-- no-sweeper crash story.
local p = roll_and_read(account_key, holds_key, mode, now_ms, user, nil, nil)

local watermark = tonumber(redis.call('HGET', watermarks_key, session_id)) or 0
if seq <= watermark then
  -- The replay case, and the ordinary one: every open of a session re-drives
  -- its terminal events through here. Nothing may change.
  return {'NOOP', fmtusd(p.committed), fmtusd(0.0)}
end
redis.call('HSET', watermarks_key, session_id, seq)

local raw = redis.call('HGET', holds_key, response_id)
local held = 0.0
if raw then
  local _user, amount = unpack_hold(raw)
  held = amount
  redis.call('HDEL', holds_key, response_id)
end
-- Floored at zero: settling above the hold overcommits — realized spend is a
-- fact — and there is nothing left to give back.
local released = held - actual_usd
if released < 0.0 then released = 0.0 end

local committed = p.committed + actual_usd
redis.call('HSET', account_key, 'committed', fmtusd(committed))
local member_committed = p.member_committed + actual_usd
redis.call('HSET', account_key, 'member:' .. user, fmtusd(member_committed))

return {'OK', fmtusd(committed), fmtusd(released)}
";

/// `KEYS[1]` account, `KEYS[2]` holds.
/// `ARGV`: user, now_ms, limit_usd, member_ceiling_usd (`''` for pooled),
/// warn_at, window mode.
///
/// A read in the trait's vocabulary, but not read-only on the wire: like
/// every op it runs `settle_time` first, so a balance query is also how a
/// lapsed hold or a rolled-over window gets cleaned up when nothing else has
/// looked in a while.
const BALANCE_BODY: &str = r"
local account_key, holds_key = KEYS[1], KEYS[2]
local user = ARGV[1]
local now_ms = tonumber(ARGV[2])
local limit_usd = tonumber(ARGV[3])
local member_ceiling
if ARGV[4] == '' then member_ceiling = nil else member_ceiling = tonumber(ARGV[4]) end
local warn_at = tonumber(ARGV[5])
local mode = ARGV[6]

local p = roll_and_read(account_key, holds_key, mode, now_ms, user, limit_usd, member_ceiling)

-- A pooled membership has no second ceiling, which is not a ceiling of zero:
-- the flag is what tells the two apart on the wire, and the dollar field
-- beside it is meaningless when the flag is 0.
local member_remaining_present = 0
local member_remaining = 0.0
if member_ceiling ~= nil then
  member_remaining_present = 1
  member_remaining = p.member_remaining
end

local state = state_for(p.committed + p.held, p.member_committed + p.member_held,
                        member_ceiling, limit_usd, warn_at, p.effective)

return {'OK', fmtusd(p.committed), fmtusd(p.held), fmtusd(p.project_remaining),
        fmtusd(p.member_committed), member_remaining_present, fmtusd(member_remaining), state}
";

/// What [`Scripts::open_grant`] resolves to.
pub(crate) struct GrantOutcome {
    pub(crate) granted_usd: f64,
    pub(crate) state: LedgerState,
}

/// What [`Scripts::settle_grant`] resolves to.
pub(crate) enum SettleOutcome {
    Applied {
        committed_usd: f64,
        released_usd: f64,
    },
    /// `(session_id, seq)` was at or below the watermark.
    NoOp { committed_usd: f64 },
}

/// What [`Scripts::balance`] resolves to.
pub(crate) struct BalanceOutcome {
    pub(crate) committed_usd: f64,
    pub(crate) held_usd: f64,
    pub(crate) project_remaining_usd: f64,
    pub(crate) member_committed_usd: f64,
    pub(crate) member_remaining_usd: Option<f64>,
    pub(crate) state: LedgerState,
}

/// What [`Scripts::open_grant`] needs, bundled rather than eleven positional
/// arguments: this is the same reason `roundhouse_core::control::spend`
/// carries [`GrantRequest`](roundhouse_core::control::GrantRequest) as one
/// value instead of a long parameter list — the two keys plus every field
/// the Lua script's `ARGV` reads, in the order it reads them.
pub(crate) struct OpenGrantArgs<'a> {
    pub(crate) account_key: &'a str,
    pub(crate) holds_key: &'a str,
    pub(crate) user: &'a str,
    pub(crate) response_id: &'a str,
    pub(crate) requested_usd: f64,
    pub(crate) ttl_ms: u64,
    pub(crate) now_ms: u64,
    pub(crate) limit_usd: f64,
    pub(crate) member_ceiling_arg: &'a str,
    pub(crate) warn_at: f64,
    pub(crate) window_mode: &'a str,
}

/// What [`Scripts::settle_grant`] needs.
pub(crate) struct SettleGrantArgs<'a> {
    pub(crate) account_key: &'a str,
    pub(crate) holds_key: &'a str,
    pub(crate) watermarks_key: &'a str,
    pub(crate) user: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) seq: u64,
    pub(crate) response_id: &'a str,
    pub(crate) actual_usd: f64,
    pub(crate) now_ms: u64,
    pub(crate) window_mode: &'a str,
}

/// What [`Scripts::balance`] needs.
pub(crate) struct BalanceArgs<'a> {
    pub(crate) account_key: &'a str,
    pub(crate) holds_key: &'a str,
    pub(crate) user: &'a str,
    pub(crate) now_ms: u64,
    pub(crate) limit_usd: f64,
    pub(crate) member_ceiling_arg: &'a str,
    pub(crate) warn_at: f64,
    pub(crate) window_mode: &'a str,
}

/// The three scripts, compiled once per ledger.
pub(crate) struct Scripts {
    open_grant: redis::Script,
    settle_grant: redis::Script,
    balance: redis::Script,
}

impl Scripts {
    pub(crate) fn new() -> Self {
        Self {
            open_grant: redis::Script::new(&format!("{LUA_PRELUDE}\n{OPEN_GRANT_BODY}")),
            settle_grant: redis::Script::new(&format!("{LUA_PRELUDE}\n{SETTLE_GRANT_BODY}")),
            balance: redis::Script::new(&format!("{LUA_PRELUDE}\n{BALANCE_BODY}")),
        }
    }

    pub(crate) async fn open_grant(
        &self,
        conn: &mut ConnectionManager,
        args: OpenGrantArgs<'_>,
    ) -> Result<GrantOutcome, SpendError> {
        let reply: Vec<Value> = self
            .open_grant
            .key(args.account_key)
            .key(args.holds_key)
            .arg(args.user)
            .arg(args.response_id)
            .arg(args.requested_usd)
            .arg(args.ttl_ms)
            .arg(args.now_ms)
            .arg(args.limit_usd)
            .arg(args.member_ceiling_arg)
            .arg(args.warn_at)
            .arg(args.window_mode)
            .invoke_async(conn)
            .await
            .map_err(backend)?;
        match (tag_of(&reply), f64_at(&reply, 1), str_at(&reply, 2)) {
            (Some("OK"), Some(granted_usd), Some(state_tag)) => Ok(GrantOutcome {
                granted_usd,
                state: parse_state(state_tag)?,
            }),
            _ => Err(unexpected(&reply)),
        }
    }

    pub(crate) async fn settle_grant(
        &self,
        conn: &mut ConnectionManager,
        args: SettleGrantArgs<'_>,
    ) -> Result<SettleOutcome, SpendError> {
        let reply: Vec<Value> = self
            .settle_grant
            .key(args.account_key)
            .key(args.holds_key)
            .key(args.watermarks_key)
            .arg(args.user)
            .arg(args.session_id)
            .arg(args.seq)
            .arg(args.response_id)
            .arg(args.actual_usd)
            .arg(args.now_ms)
            .arg(args.window_mode)
            .invoke_async(conn)
            .await
            .map_err(backend)?;
        match (tag_of(&reply), f64_at(&reply, 1), f64_at(&reply, 2)) {
            (Some("OK"), Some(committed_usd), Some(released_usd)) => Ok(SettleOutcome::Applied {
                committed_usd,
                released_usd,
            }),
            (Some("NOOP"), Some(committed_usd), Some(_)) => {
                Ok(SettleOutcome::NoOp { committed_usd })
            }
            _ => Err(unexpected(&reply)),
        }
    }

    pub(crate) async fn balance(
        &self,
        conn: &mut ConnectionManager,
        args: BalanceArgs<'_>,
    ) -> Result<BalanceOutcome, SpendError> {
        let reply: Vec<Value> = self
            .balance
            .key(args.account_key)
            .key(args.holds_key)
            .arg(args.user)
            .arg(args.now_ms)
            .arg(args.limit_usd)
            .arg(args.member_ceiling_arg)
            .arg(args.warn_at)
            .arg(args.window_mode)
            .invoke_async(conn)
            .await
            .map_err(backend)?;
        // **A malformed reply must never impersonate "no member ceiling."**
        // The flag at index 5 and the dollar amount at index 6 are decoded
        // together and exhaustively, because the alternative reads a failed
        // decode as `None` — and `None` is not "unknown", it is the positive
        // claim that this membership is pooled and has no second ceiling to
        // bind. A member cap that silently stopped binding because a field did
        // not parse is precisely the shadowing bug
        // `a_grant_never_exceeds_the_member_ceiling_even_when_the_project_has_room`
        // exists to forbid, arriving through the back door.
        let member_remaining_usd = match (int_at(&reply, 5), f64_at(&reply, 6)) {
            // Pooled. Index 6 is present and meaningless, so it is not read.
            (Some(0), _) => None,
            (Some(1), Some(member_remaining_usd)) => Some(member_remaining_usd),
            _ => return Err(unexpected(&reply)),
        };
        match (
            tag_of(&reply),
            f64_at(&reply, 1),
            f64_at(&reply, 2),
            f64_at(&reply, 3),
            f64_at(&reply, 4),
            str_at(&reply, 7),
        ) {
            (
                Some("OK"),
                Some(committed_usd),
                Some(held_usd),
                Some(project_remaining_usd),
                Some(member_committed_usd),
                Some(state_tag),
            ) => Ok(BalanceOutcome {
                committed_usd,
                held_usd,
                project_remaining_usd,
                member_committed_usd,
                member_remaining_usd,
                state: parse_state(state_tag)?,
            }),
            _ => Err(unexpected(&reply)),
        }
    }
}

/// The three tags `state_for` in [`LUA_PRELUDE`] can return, and nothing else.
///
/// A ledger's alphabet is [`LedgerState`], which has exactly these three
/// variants — the fourth budget state, the overflow valve's mark, is not a
/// thing a counter and two ceilings can observe and so is not a thing this
/// function can produce. It used to say so in a comment beside a fall-through
/// arm; the type says it now.
fn parse_state(tag: &str) -> Result<LedgerState, SpendError> {
    match tag {
        "unconstrained" => Ok(LedgerState::Unconstrained),
        "warned" => Ok(LedgerState::Warned),
        "exhausted" => Ok(LedgerState::Exhausted),
        other => Err(SpendError::Backend(anyhow::anyhow!(
            "spend ledger script returned an unknown budget state `{other}`"
        ))),
    }
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

fn f64_at(reply: &[Value], index: usize) -> Option<f64> {
    str_at(reply, index)?.parse().ok()
}

fn int_at(reply: &[Value], index: usize) -> Option<i64> {
    match reply.get(index)? {
        Value::Int(number) => Some(*number),
        _ => None,
    }
}

fn unexpected(reply: &[Value]) -> SpendError {
    SpendError::Backend(anyhow::anyhow!(
        "spend ledger script returned an unexpected reply: {reply:?}"
    ))
}

fn backend(error: redis::RedisError) -> SpendError {
    SpendError::Backend(anyhow::Error::new(error))
}

#[cfg(test)]
mod tests {
    //! The Lua calendar port, *executed* — against the same list of boundaries
    //! the Rust port is executed against.
    //!
    //! What was here before proved nothing: it built a [`redis::Script`] from
    //! the prelude and asserted the call did not panic. `Script::new` computes
    //! a SHA-1 of a string. It cannot fail, it never parses the Lua, and it
    //! would have been just as green with the calendar deleted.
    //!
    //! A dev-only `mlua` supplies the interpreter the crate does not otherwise
    //! have, so the port answers
    //! [`MONTH_START_CASES`](roundhouse_core::control::spend::contract::MONTH_START_CASES)
    //! on every `cargo test`. **The honest scope**: this executes the
    //! arithmetic, and it executes it under Lua 5.4 while Redis embeds 5.1.
    //! That is sound for what is tested here because the port is written to be
    //! version-independent — float division plus an explicit truncation
    //! (`idiv`), never 5.3's `//` operator, and every intermediate well inside
    //! the range a double represents exactly — but it is not a test of the
    //! Redis sandbox, of `redis.call`, or of RESP conversion. Those are the
    //! ignore-gated suite's business.

    use super::*;
    use roundhouse_core::control::spend::contract::MONTH_START_CASES;

    /// The prelude, loaded as one chunk, with `month_start_ms` handed back as
    /// a callable.
    ///
    /// One chunk because that is what [`Scripts::new`] does: the prelude's
    /// helpers are `local`, so they exist only inside the chunk that declares
    /// them, and "shared" here means "spliced into each script's source". A
    /// probe that loaded the prelude separately would be testing a different
    /// arrangement of the code than the one that ships.
    ///
    /// No `redis` global is stubbed and none is needed — nothing on the path
    /// from `month_start_ms` down to `idiv` calls one. Loading the chunk also
    /// parses [`LUA_PRELUDE`] in full, including `roll_and_read`, so a syntax
    /// error anywhere in the prelude fails here rather than at first grant
    /// against a live server.
    fn month_start_ms_in_lua(lua: &mlua::Lua) -> mlua::Function {
        lua.load(format!(
            "{LUA_PRELUDE}\nreturn function(now_ms) return month_start_ms(now_ms) end"
        ))
        .eval()
        .expect("the Lua prelude must parse and expose its calendar")
    }

    #[test]
    fn the_lua_calendar_port_answers_the_same_boundaries_the_rust_one_does() {
        let lua = mlua::Lua::new();
        let month_start = month_start_ms_in_lua(&lua);

        for case in MONTH_START_CASES {
            // Read back as `f64` rather than an integer: Lua 5.1 has only
            // doubles and 5.4 would hand back an integer, and the assertion
            // should be about the calendar rather than about which numeric
            // subtype the host interpreter chose. Every value in the table is
            // far inside the range a double holds exactly.
            let answer: f64 = month_start
                .call(case.now_ms)
                .expect("month_start_ms must return a number");
            assert_eq!(
                answer, case.month_start_ms as f64,
                "{}: month_start_ms({})",
                case.what, case.now_ms
            );
        }

        assert!(
            MONTH_START_CASES.len() >= 3,
            "the control on the loop itself: an empty table would make every \
             assertion above vacuous, which is exactly the failure this test replaced"
        );
    }
}
