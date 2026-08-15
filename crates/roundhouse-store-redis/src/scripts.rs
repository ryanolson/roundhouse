// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The write path: four Lua scripts and their reply decoding.
//!
//! Scripts because the contract demands *atomicity*, not because Lua is nice:
//! checking the lease and acting on it must be one step, or a writer fenced
//! out between the check and the act would still get its write in — the exact
//! interleaving the lease exists to prevent. A Redis script executes with
//! nothing in between, which makes each of these a single indivisible
//! compare-and-mutate.
//!
//! All time comes from `redis.call('TIME')`, never from the client: lease
//! expiry is enforced by `PX` on the server, and event timestamps are stamped
//! server-side, so a fleet of writers with skewed clocks still agrees on one
//! clock authority. Calling `TIME` before writes is safe because scripts
//! replicate by effects (the default since Redis 5, the only mode in 7).
//!
//! Each script returns a small status table — `{tag, numbers…}` — decoded
//! here into typed outcomes. The tags are a wire contract between the Lua and
//! the Rust below and appear nowhere else.

use redis::Value;
use redis::aio::ConnectionManager;
use roundhouse_core::store::StoreError;

/// Claim or re-claim the lease.
///
/// An absent lease key *is* an expired lease — `PX` deletes it — so takeover
/// needs no expiry arithmetic here. Re-acquisition by the current holder is
/// recovery, not competition, and refreshes the TTL.
const ACQUIRE: &str = r"
if redis.call('EXISTS', KEYS[1]) == 0 then return {'NOSESSION'} end
local holder = redis.call('GET', KEYS[2])
if holder ~= false and holder ~= ARGV[1] then return {'HELD'} end
redis.call('SET', KEYS[2], ARGV[1], 'PX', ARGV[2])
local t = redis.call('TIME')
return {'OK', tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)}
";

/// Extend a held lease. Refused unless the record still names the caller —
/// missing and someone-else's are the same answer, because both mean the
/// caller's tenure is over and only acquire may start a new one.
const RENEW: &str = r"
if redis.call('EXISTS', KEYS[1]) == 0 then return {'NOSESSION'} end
if redis.call('GET', KEYS[2]) ~= ARGV[1] then return {'REFUSED'} end
redis.call('PEXPIRE', KEYS[2], ARGV[2])
local t = redis.call('TIME')
return {'OK', tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)}
";

/// Compare-and-delete. Lenient by contract: releasing what you no longer
/// hold — or a session that no longer exists — is the cleanup path racing
/// reality, not an error worth reporting.
const RELEASE: &str = r"
if redis.call('GET', KEYS[1]) == ARGV[1] then redis.call('DEL', KEYS[1]) end
return {'OK'}
";

/// The fenced append, and the reason this module exists.
///
/// Fence check, seq assignment, and the `XADD`s are one atomic step, so the
/// log stays contiguous under concurrent writers and a displaced owner cannot
/// slip a write behind its successor. Seqs continue from the newest entry id;
/// an id this store did not write (not `<seq>-0` shaped) aborts rather than
/// guessing, because appending after a foreign entry would launder it into a
/// log that otherwise proves its own integrity. Lua numbers are doubles, but
/// exact through 2^53 — seqs count events per conversation and sit nowhere
/// near that.
const APPEND: &str = r"
if redis.call('EXISTS', KEYS[1]) == 0 then return {'NOSESSION'} end
if redis.call('GET', KEYS[2]) ~= ARGV[1] then return {'FENCED'} end
local last = 0
local newest = redis.call('XREVRANGE', KEYS[3], '+', '-', 'COUNT', 1)
if #newest > 0 then
  local seq = string.match(newest[1][1], '^(%d+)-0$')
  if not seq then return {'CORRUPT', newest[1][1]} end
  last = tonumber(seq)
end
local t = redis.call('TIME')
local at_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
for i = 2, #ARGV do
  last = last + 1
  redis.call('XADD', KEYS[3], last .. '-0', 'at_ms', at_ms, 'kind', ARGV[i])
end
return {'OK', at_ms, last}
";

/// What acquire and renew resolve to.
pub(crate) enum LeaseOutcome {
    /// The record now names the caller. `now_ms` is the Redis clock.
    Granted {
        now_ms: u64,
    },
    /// Someone else's tenure (acquire) or the caller's is over (renew).
    Refused,
    NoSession,
}

/// What the fenced append resolves to.
pub(crate) enum AppendOutcome {
    /// `last_seq` is the seq of the final event written this call.
    Appended {
        at_ms: u64,
        last_seq: u64,
    },
    Fenced,
    NoSession,
}

/// The four scripts, compiled once per store.
///
/// `redis::Script` sends `EVALSHA` and falls back to `EVAL` on `NOSCRIPT`,
/// so a restarted or failed-over Redis re-learns them transparently.
pub(crate) struct Scripts {
    acquire: redis::Script,
    renew: redis::Script,
    release: redis::Script,
    append: redis::Script,
}

impl Scripts {
    pub(crate) fn new() -> Self {
        Self {
            acquire: redis::Script::new(ACQUIRE),
            renew: redis::Script::new(RENEW),
            release: redis::Script::new(RELEASE),
            append: redis::Script::new(APPEND),
        }
    }

    pub(crate) async fn acquire(
        &self,
        conn: &mut ConnectionManager,
        meta_key: &str,
        lease_key: &str,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<LeaseOutcome, StoreError> {
        let reply: Vec<Value> = self
            .acquire
            .key(meta_key)
            .key(lease_key)
            .arg(node_id)
            .arg(ttl_ms)
            .invoke_async(conn)
            .await
            .map_err(super::backend)?;
        decode_lease_reply(&reply, "HELD")
    }

    pub(crate) async fn renew(
        &self,
        conn: &mut ConnectionManager,
        meta_key: &str,
        lease_key: &str,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<LeaseOutcome, StoreError> {
        let reply: Vec<Value> = self
            .renew
            .key(meta_key)
            .key(lease_key)
            .arg(node_id)
            .arg(ttl_ms)
            .invoke_async(conn)
            .await
            .map_err(super::backend)?;
        decode_lease_reply(&reply, "REFUSED")
    }

    pub(crate) async fn release(
        &self,
        conn: &mut ConnectionManager,
        lease_key: &str,
        node_id: &str,
    ) -> Result<(), StoreError> {
        let _: Vec<Value> = self
            .release
            .key(lease_key)
            .arg(node_id)
            .invoke_async(conn)
            .await
            .map_err(super::backend)?;
        Ok(())
    }

    pub(crate) async fn append(
        &self,
        conn: &mut ConnectionManager,
        meta_key: &str,
        lease_key: &str,
        log_key: &str,
        node_id: &str,
        kind_payloads: &[String],
    ) -> Result<AppendOutcome, StoreError> {
        let mut invocation = self.append.prepare_invoke();
        invocation
            .key(meta_key)
            .key(lease_key)
            .key(log_key)
            .arg(node_id);
        for payload in kind_payloads {
            invocation.arg(payload.as_str());
        }
        let reply: Vec<Value> = invocation
            .invoke_async(conn)
            .await
            .map_err(super::backend)?;

        match (tag_of(&reply), int_at(&reply, 1), int_at(&reply, 2)) {
            (Some("OK"), Some(at_ms), Some(last_seq)) => {
                Ok(AppendOutcome::Appended { at_ms, last_seq })
            }
            (Some("FENCED"), ..) => Ok(AppendOutcome::Fenced),
            (Some("NOSESSION"), ..) => Ok(AppendOutcome::NoSession),
            (Some("CORRUPT"), ..) => Err(StoreError::Backend(anyhow::anyhow!(
                "log `{log_key}` ends in entry `{}`, which this store did not write; \
                 refusing to append after a foreign entry",
                str_at(&reply, 1).unwrap_or("<unreadable>")
            ))),
            _ => Err(unexpected(&reply)),
        }
    }
}

/// Acquire and renew share a reply shape; only the refusal tag differs.
fn decode_lease_reply(reply: &[Value], refusal: &str) -> Result<LeaseOutcome, StoreError> {
    match (tag_of(reply), int_at(reply, 1)) {
        (Some("OK"), Some(now_ms)) => Ok(LeaseOutcome::Granted { now_ms }),
        (Some(tag), _) if tag == refusal => Ok(LeaseOutcome::Refused),
        (Some("NOSESSION"), _) => Ok(LeaseOutcome::NoSession),
        _ => Err(unexpected(reply)),
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

fn int_at(reply: &[Value], index: usize) -> Option<u64> {
    match reply.get(index)? {
        Value::Int(number) => u64::try_from(*number).ok(),
        _ => None,
    }
}

fn unexpected(reply: &[Value]) -> StoreError {
    StoreError::Backend(anyhow::anyhow!(
        "store script returned an unexpected reply: {reply:?}"
    ))
}
