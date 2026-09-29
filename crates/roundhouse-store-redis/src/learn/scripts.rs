// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learner store's two scripts, `read` and `apply`, and their reply
//! decoding.
//!
//! **Every reply is an array of integers.** The first element is a reply code
//! and the rest are counters, a watermark, or 1-based positions in `KEYS` and
//! `ARGV` that the Rust side turns back into names. A Lua number returned from
//! a script becomes a RESP integer, exact up to `2^53 - 1`; nothing here is
//! returned through `tostring` or `..`, which format with `%.14g` in Lua 5.1
//! and drop digits past the fourteenth (draft section 11.3). The counters a
//! script stores are passed to `HSET` as Lua numbers too, which Redis writes
//! as exact decimal integers.
//!
//! **`apply` checks everything before its first write.** Redis runs a script
//! without interleaving other commands, but it does not undo the writes a
//! script made before an error. So the script checks the type of every key
//! the batch names, then the chain and every counter's range entry by entry,
//! and only then writes, with `HSET` and `SADD` alone, on keys whose type it
//! has checked. An out-of-memory refusal or a server failure during the write
//! phase is outside this guarantee, and no test here can inject one.
//!
//! **Types are checked for every key the batch names**, including keys only a
//! skipped entry would touch. A key of the wrong type is foreign data in this
//! family's key space, and the recovery for it is the same whichever entry
//! meets it first.

use redis::Value;
use redis::aio::ConnectionManager;

use roundhouse_core::learn_store::{Applied, LearnerError, ReadRequest};
use roundhouse_core::routing::learn::{
    CacheReuse, JevCounts, LatencySum, LevelView, ReadView, StrategyCounts, TargetOps,
};

use super::{ApplyPlan, ReadPlan, unavailable};

/// The reply codes, the first element of every reply.
const OK: i64 = 0;
const WRONG_TYPE: i64 = 1;
const CHAIN_GAP: i64 = 2;
const CHAIN_DIVERGED: i64 = 3;
const RANGE: i64 = 4;

/// Lua shared by both scripts: the limits, the stored-counter parse, and the
/// type check, which takes a position in `KEYS` so no `redis.call` here names
/// a key any other way.
///
/// `counter` reads a stored field: absent is 0, and anything that is not an
/// integer within the counter's range is `nil`, which the caller reports. A
/// stored value past `2^53 - 1` could not be added to exactly, so it is
/// refused rather than rounded.
const PRELUDE: &str = r"
local MAX = 9007199254740991
local OK, WRONG_TYPE, CHAIN_GAP, CHAIN_DIVERGED, RANGE = 0, 1, 2, 3, 4
local function counter(stored, signed)
  if not stored then return 0 end
  local n = tonumber(stored)
  if n == nil or n ~= math.floor(n) or n > MAX then return nil end
  if signed then
    if n < -MAX then return nil end
  elseif n < 0 then
    return nil
  end
  return n
end
local function type_is(index, want)
  local found = redis.call('TYPE', KEYS[index]).ok
  return found == want or found == 'none'
end
";

/// One turn's counters, with no write.
///
/// KEYS: the three quality keys, most specific first, then the operations
/// key. ARGV: the count `F` of quality fields, the `F` quality fields, then
/// the operations fields. Reply: `OK` then every quality field of each
/// quality key and every operations field, absent as 0; or `WRONG_TYPE key`;
/// or `RANGE key field` for a stored value that is not an exact integer.
const READ: &str = r"
for i = 1, #KEYS do
  if not type_is(i, 'hash') then return {WRONG_TYPE, i} end
end
local reply = {OK}
local function read(key, first, last)
  local values = redis.call('HMGET', KEYS[key], unpack(ARGV, first, last))
  for j = 1, last - first + 1 do
    local n = counter(values[j], true)
    if n == nil then return {RANGE, key, first + j - 1} end
    reply[#reply + 1] = n
  end
end
local fields = tonumber(ARGV[1])
for key = 1, 3 do
  local refused = read(key, 2, fields + 1)
  if refused then return refused end
end
local refused = read(4, fields + 2, #ARGV)
if refused then return refused end
return reply
";

/// Apply the entries above the session's watermark, all or nothing.
///
/// KEYS and ARGV are laid out by [`ApplyPlan`], which documents them. The
/// check phase stages every new value in `staged` and every new `seen` member
/// in `members`, in first-touch order; the write phase stores them only when
/// an entry applied. Reply: `OK applied watermark`, `WRONG_TYPE key`,
/// `CHAIN_GAP store_watermark`, `CHAIN_DIVERGED store_watermark`, or
/// `RANGE key field`.
///
/// Each entry is compared with the watermark as the entries before it moved
/// it, as the memory backend's `stage` does, and a refusal reports the
/// watermark the store holds.
const APPLY: &str = r"
local hashes = tonumber(ARGV[2])
for i = 1, #KEYS do
  local want = 'hash'
  if i > hashes then want = 'set' end
  if not type_is(i, want) then return {WRONG_TYPE, i} end
end
local session = ARGV[1]
local stored = counter(redis.call('HGET', KEYS[1], session), false)
if stored == nil then return {RANGE, 1, 1} end
local watermark = stored
local staged, fields, members, touched = {}, {}, {}, {}
local function add(key, field_arg, delta, signed)
  local field = ARGV[field_arg]
  if staged[key] == nil then
    staged[key], fields[key] = {}, {}
    touched[#touched + 1] = key
  end
  local value = staged[key][field]
  if value == nil then
    value = counter(redis.call('HGET', KEYS[key], field), signed)
    if value == nil then return {RANGE, key, field_arg} end
    fields[key][#fields[key] + 1] = field
  end
  value = value + delta
  if value > MAX or (signed and value < -MAX) or (not signed and value < 0) then
    return {RANGE, key, field_arg}
  end
  staged[key][field] = value
end
local applied = 0
local p = 4
for _ = 1, tonumber(ARGV[3]) do
  local seq, prev, width = tonumber(ARGV[p]), tonumber(ARGV[p + 1]), tonumber(ARGV[p + 2])
  local q, last = p + 3, p + 2 + width
  p = last + 1
  if seq > watermark then
    if prev > watermark then return {CHAIN_GAP, stored} end
    if prev < watermark then return {CHAIN_DIVERGED, stored} end
    while q <= last do
      local op, refused = tonumber(ARGV[q])
      if op == 3 then
        local set, member = tonumber(ARGV[q + 1]), ARGV[q + 2]
        if members[set] == nil then members[set] = {list = {}, has = {}} end
        if not members[set].has[member]
          and redis.call('SISMEMBER', KEYS[set], member) == 0 then
          members[set].has[member] = true
          members[set].list[#members[set].list + 1] = member
          refused = add(tonumber(ARGV[q + 3]), q + 4, 1, false)
        end
        q = q + 5
      else
        refused = add(tonumber(ARGV[q + 1]), q + 2, tonumber(ARGV[q + 3]), op == 2)
        q = q + 4
      end
      if refused then return refused end
    end
    watermark = seq
    applied = applied + 1
  end
end
if applied == 0 then return {OK, 0, watermark} end
for _, key in ipairs(touched) do
  local args = {}
  for _, field in ipairs(fields[key]) do
    args[#args + 1] = field
    args[#args + 1] = staged[key][field]
  end
  redis.call('HSET', KEYS[key], unpack(args))
end
for set = hashes + 1, #KEYS do
  if members[set] and #members[set].list > 0 then
    redis.call('SADD', KEYS[set], unpack(members[set].list))
  end
end
redis.call('HSET', KEYS[1], session, watermark)
return {OK, applied, watermark}
";

/// The two scripts, compiled once per store.
pub(super) struct Scripts {
    read: redis::Script,
    apply: redis::Script,
}

impl Scripts {
    pub(super) fn new() -> Self {
        Self {
            read: redis::Script::new(&format!("{PRELUDE}\n{READ}")),
            apply: redis::Script::new(&format!("{PRELUDE}\n{APPLY}")),
        }
    }

    pub(super) async fn read(
        &self,
        conn: &mut ConnectionManager,
        plan: &ReadPlan,
    ) -> Result<Vec<Value>, LearnerError> {
        let mut invocation = self.read.prepare_invoke();
        for key in &plan.keys {
            invocation.key(key.as_str());
        }
        invocation.arg(plan.quality_fields.len());
        for field in plan.quality_fields.iter().chain(&plan.ops_fields) {
            invocation.arg(field.as_str());
        }
        invocation.invoke_async(conn).await.map_err(unavailable)
    }

    pub(super) async fn apply(
        &self,
        conn: &mut ConnectionManager,
        plan: &ApplyPlan,
    ) -> Result<Vec<Value>, LearnerError> {
        let mut invocation = self.apply.prepare_invoke();
        for key in &plan.keys {
            invocation.key(key.as_str());
        }
        for arg in &plan.args {
            invocation.arg(arg);
        }
        invocation.invoke_async(conn).await.map_err(unavailable)
    }
}

/// Every element as an integer, or the reply refused whole: a script that
/// returned anything else is not the script this build compiled.
fn integers(reply: &[Value]) -> Result<Vec<i64>, LearnerError> {
    reply
        .iter()
        .map(|value| match value {
            Value::Int(number) => Ok(*number),
            _ => Err(unexpected(reply)),
        })
        .collect()
}

fn unexpected(reply: &[Value]) -> LearnerError {
    LearnerError::Unavailable(format!(
        "learner store script returned an unexpected reply: {reply:?}"
    ))
}

/// A position in `KEYS` or `ARGV` as the script reported it, 1-based.
fn position(numbers: &[i64], index: usize, reply: &[Value]) -> Result<usize, LearnerError> {
    numbers
        .get(index)
        .and_then(|number| usize::try_from(*number).ok())
        .ok_or_else(|| unexpected(reply))
}

pub(super) fn decode_read(
    reply: &[Value],
    plan: &ReadPlan,
    request: &ReadRequest,
) -> Result<ReadView, LearnerError> {
    let numbers = integers(reply)?;
    let key_at = |index: usize| plan.keys.get(index.wrapping_sub(1)).cloned();
    match numbers.first() {
        Some(&OK) => {}
        Some(&WRONG_TYPE) => {
            let key = key_at(position(&numbers, 1, reply)?).ok_or_else(|| unexpected(reply))?;
            return Err(LearnerError::WrongType { key });
        }
        Some(&RANGE) => {
            let key = key_at(position(&numbers, 1, reply)?).unwrap_or_default();
            return Err(LearnerError::Unavailable(format!(
                "`{key}` holds a counter that is not an integer within 2^53 - 1"
            )));
        }
        _ => return Err(unexpected(reply)),
    }
    let expected = 1 + 3 * plan.quality_fields.len() + plan.ops_fields.len();
    if numbers.len() != expected {
        return Err(unexpected(reply));
    }
    let mut values = numbers[1..].iter().copied();
    // Every field but the two signed sums is a nonnegative count; a negative
    // one is a value this store never writes.
    let mut unsigned = || {
        values
            .next()
            .and_then(|number| u64::try_from(number).ok())
            .ok_or_else(|| unexpected(reply))
    };
    let mut levels = Vec::with_capacity(3);
    for &key in request.keys() {
        let mut strategies = Vec::with_capacity(request.strategies().len());
        for &strategy in request.strategies() {
            strategies.push(StrategyCounts {
                strategy,
                pos_units: unsigned()?,
                n_units: unsigned()?,
                sessions: unsigned()?,
            });
        }
        let jev = JevCounts {
            capable: unsigned()?,
            efficient: unsigned()?,
        };
        levels.push(LevelView {
            key,
            strategies,
            jev,
        });
    }
    let mut values = numbers[1 + 3 * plan.quality_fields.len()..].iter().copied();
    let mut next = || values.next().ok_or_else(|| unexpected(reply));
    let count = |number: i64| u64::try_from(number).map_err(|_| unexpected(reply));
    let mut targets = Vec::with_capacity(request.targets().len());
    for target in request.targets() {
        let sum_ms = next()?;
        targets.push(TargetOps {
            target: target.clone(),
            latency: LatencySum {
                sum_ms,
                n: count(next()?)?,
            },
            failover: count(next()?)?,
            cache: CacheReuse {
                predicted_permille: count(next()?)?,
                observed_permille: count(next()?)?,
                n: count(next()?)?,
            },
        });
    }
    let overhead = LatencySum {
        sum_ms: next()?,
        n: count(next()?)?,
    };
    Ok(ReadView {
        levels,
        targets,
        overhead,
    })
}

pub(super) fn decode_apply(reply: &[Value], plan: &ApplyPlan) -> Result<Applied, LearnerError> {
    let numbers = integers(reply)?;
    let unsigned = |index: usize| {
        numbers
            .get(index)
            .and_then(|number| u64::try_from(*number).ok())
            .ok_or_else(|| unexpected(reply))
    };
    match numbers.first() {
        Some(&OK) if numbers.len() == 3 => Ok(Applied {
            applied: position(&numbers, 1, reply)?,
            watermark: unsigned(2)?,
        }),
        Some(&WRONG_TYPE) => {
            let index = position(&numbers, 1, reply)?;
            let key = plan
                .keys
                .get(index.wrapping_sub(1))
                .cloned()
                .ok_or_else(|| unexpected(reply))?;
            Err(LearnerError::WrongType { key })
        }
        Some(&CHAIN_GAP) => Err(LearnerError::ChainGap {
            store_watermark: unsigned(1)?,
        }),
        Some(&CHAIN_DIVERGED) => Err(LearnerError::ChainDiverged {
            store_watermark: unsigned(1)?,
        }),
        Some(&RANGE) => Err(LearnerError::CounterRange {
            counter: plan.counter(position(&numbers, 1, reply)?, position(&numbers, 2, reply)?),
        }),
        _ => Err(unexpected(reply)),
    }
}
