// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The write path: four Lua scripts and their reply decoding, plus the
//! learning index's scripts in [`learning`].
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

pub(crate) mod learning;

use redis::Value;
use redis::aio::ConnectionManager;
use roundhouse_core::store::StoreError;

/// Claim or re-claim the lease.
///
/// An absent lease key *is* an expired lease — `PX` deletes it — so takeover
/// needs no expiry arithmetic here. Re-acquisition by the current holder is
/// recovery, not competition, but it replaces the fencing token so handles
/// from the holder's previous tenure fail immediately.
const ACQUIRE: &str = r"
if redis.call('EXISTS', KEYS[1]) == 0 then return {'NOSESSION'} end
local holder = redis.call('HGET', KEYS[2], 'node_id')
if holder ~= false and holder ~= ARGV[1] then return {'REFUSED'} end
redis.call('HSET', KEYS[2], 'node_id', ARGV[1], 'fencing_token', ARGV[2])
redis.call('PEXPIRE', KEYS[2], ARGV[3])
local t = redis.call('TIME')
return {'OK', tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)}
";

/// Extend a held lease. Refused unless the record still names the caller —
/// missing and someone-else's are the same answer, because both mean the
/// caller's tenure is over and only acquire may start a new one.
const RENEW: &str = r"
if redis.call('EXISTS', KEYS[1]) == 0 then return {'NOSESSION'} end
if redis.call('HGET', KEYS[2], 'node_id') ~= ARGV[1] then return {'REFUSED'} end
if redis.call('HGET', KEYS[2], 'fencing_token') ~= ARGV[2] then return {'REFUSED'} end
redis.call('PEXPIRE', KEYS[2], ARGV[3])
local t = redis.call('TIME')
return {'OK', tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)}
";

/// Compare-and-delete. Lenient by contract: releasing what you no longer
/// hold — or a session that no longer exists — is the cleanup path racing
/// reality, not an error worth reporting.
const RELEASE: &str = r"
if redis.call('HGET', KEYS[1], 'node_id') == ARGV[1]
  and redis.call('HGET', KEYS[1], 'fencing_token') == ARGV[2]
then
  redis.call('DEL', KEYS[1])
end
return {'OK'}
";

/// The largest sequence the append script writes exactly. Lua's `..` renders
/// numbers with `%.14g`, so `10^14` would come out as `1e+14`.
///
/// Lives here rather than in `learning` (fleet-redis-5): the range guard it
/// names belongs to `APPEND_BODY`, which every write path takes whether or
/// not the batch carries a mark, and the canonical session write path should
/// not depend on the deliberately unwired feature module for a fact about
/// its own log.
const LAST_EXACT_SEQ: u64 = 99_999_999_999_999;

/// The fenced append, and the reason this module exists.
///
/// Fence check, seq assignment, and the `XADD`s are one atomic step, so the
/// log stays contiguous under concurrent writers and a displaced owner cannot
/// slip a write behind its successor. Seqs continue from the newest entry id;
/// an id this store did not write (not `<seq>-0` shaped) aborts rather than
/// guessing, because appending after a foreign entry would launder it into a
/// log that otherwise proves its own integrity.
///
/// **Seqs are exact only through [`LAST_EXACT_SEQ`].** Lua numbers
/// are doubles, but `..` renders them with `%.14g`, so the id for seq
/// `10^14` would be written `1e+14-0` and its `XADD` would fail. Seqs count
/// events per conversation and sit nowhere near that, but a batch that would
/// cross the limit is still refused before its first write, marked or not:
/// without the check, the `XADD`s before the failing one stay in the log, so a
/// refused append would leave part of its batch durable.
///
/// With six keys, the append also writes a learning mark (see
/// [`learning`]). Every check the mark needs — the index keys' types, the
/// stored mark's shape and project — runs before the first `XADD`, as the
/// range check does: Redis keeps a script's earlier writes when a later command
/// fails, so a problem found at the index write would leave durable events
/// with no mark, the one loss the index exists to prevent. The index keys are
/// per namespace, not per session, so a marked append is not single-slot in a
/// Redis Cluster; an unmarked append still touches only its three
/// hash-tagged keys.
///
/// KEYS: meta, lease, log [, marks, marked, pending].
/// ARGV: node id, fencing token, session id, 1-based position of the marked
/// event in the batch, project, then one payload per event. The three mark
/// arguments are ignored without the index keys.
const APPEND_BODY: &str = r"
if redis.call('EXISTS', KEYS[1]) == 0 then return {'NOSESSION'} end
if redis.call('HGET', KEYS[2], 'node_id') ~= ARGV[1] then return {'FENCED'} end
if redis.call('HGET', KEYS[2], 'fencing_token') ~= ARGV[2] then return {'FENCED'} end
local last = 0
local newest = redis.call('XREVRANGE', KEYS[3], '+', '-', 'COUNT', 1)
if #newest > 0 then
  local seq = string.match(newest[1][1], '^(%d+)-0$')
  if not seq then return {'CORRUPT', newest[1][1]} end
  last = tonumber(seq)
end
local first_payload = 6
if last + (#ARGV - first_payload + 1) > LAST_EXACT_SEQ then return {'RANGE', last} end
local marked = #KEYS == 6
local mark_seq
if marked then
  for i = 4, 6 do
    local want = 'zset'
    if i == 4 then want = 'hash' end
    if not is_type_or_absent(KEYS[i], want) then return {'WRONGTYPE', KEYS[i]} end
  end
  local stored = redis.call('HGET', KEYS[4], ARGV[3])
  if stored then
    local _, _, project = parse_mark(stored)
    if not project then return {'BADMARK', stored} end
    if project ~= ARGV[5] then return {'PROJECT', project} end
  end
  mark_seq = last + tonumber(ARGV[4])
end
local t = redis.call('TIME')
local at_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
for i = first_payload, #ARGV do
  last = last + 1
  redis.call('XADD', KEYS[3], last .. '-0', 'at_ms', at_ms, 'kind', ARGV[i])
end
if marked then
  redis.call('HSET', KEYS[4], ARGV[3], mark_seq .. ':' .. at_ms .. ':' .. ARGV[5])
  redis.call('ZADD', KEYS[5], 0, ARGV[3])
  redis.call('ZADD', KEYS[6], 0, ARGV[3])
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
    /// The session is already marked for `marked`, another project. Nothing
    /// was written.
    ProjectMismatch {
        marked: String,
    },
}

/// What one append writes: a payload per event, and the mark, if any.
pub(crate) struct AppendBatch<'a> {
    pub(crate) kind_payloads: &'a [String],
    pub(crate) mark: Option<MarkArgs<'a>>,
}

/// What a marked append adds to the script call: the three index keys, the
/// member they are keyed by, where the marked event sits in the batch, and
/// the project it is marked for.
///
/// `index_keys` owned rather than borrowed: a borrow would need a binding
/// beside the `Option` this struct itself sits in at the one call site
/// ([`crate::RedisSessionStore::append_events`]), so `Option::map` could not
/// build both from one `mark.as_ref()` — see that call site's own doc.
pub(crate) struct MarkArgs<'a> {
    pub(crate) index_keys: learning::IndexKeys,
    pub(crate) session_id: &'a str,
    /// Zero-based, already checked against the batch.
    pub(crate) event_index: usize,
    pub(crate) project: &'a str,
}

/// The two fields every lease script checks together.
///
/// Keeping them in one value prevents a call site from mixing two leases.
/// This value also keeps the script boundary compact.
#[derive(Clone, Copy)]
pub(crate) struct LeaseIdentity<'a> {
    node_id: &'a str,
    fencing_token: &'a str,
}

impl<'a> LeaseIdentity<'a> {
    pub(crate) fn new(node_id: &'a str, fencing_token: &'a str) -> Self {
        Self {
            node_id,
            fencing_token,
        }
    }
}

/// The session scripts, compiled once per store.
///
/// `redis::Script` sends `EVALSHA` and falls back to `EVAL` on `NOSCRIPT`,
/// so a restarted or failed-over Redis re-learns them transparently.
pub(crate) struct Scripts {
    acquire: redis::Script,
    renew: redis::Script,
    release: redis::Script,
    append: redis::Script,
    pub(crate) learning: learning::LearningScripts,
}

impl Scripts {
    pub(crate) fn new() -> Self {
        Self {
            acquire: redis::Script::new(ACQUIRE),
            renew: redis::Script::new(RENEW),
            release: redis::Script::new(RELEASE),
            // The append is the only script that needs `LAST_EXACT_SEQ`, so
            // it is the only prelude that carries it — the three learning
            // scripts share `learning::MARK_FUNCTIONS` alone, none of them
            // reading a seq range at all.
            append: redis::Script::new(&format!(
                "local LAST_EXACT_SEQ = {LAST_EXACT_SEQ}\n{}\n{APPEND_BODY}",
                learning::MARK_FUNCTIONS
            )),
            learning: learning::LearningScripts::new(),
        }
    }

    pub(crate) async fn acquire(
        &self,
        conn: &mut ConnectionManager,
        meta_key: &str,
        lease_key: &str,
        identity: LeaseIdentity<'_>,
        ttl_ms: u64,
    ) -> Result<LeaseOutcome, StoreError> {
        let reply: Vec<Value> = self
            .acquire
            .key(meta_key)
            .key(lease_key)
            .arg(identity.node_id)
            .arg(identity.fencing_token)
            .arg(ttl_ms)
            .invoke_async(conn)
            .await
            .map_err(super::backend)?;
        decode_lease_reply(&reply)
    }

    pub(crate) async fn renew(
        &self,
        conn: &mut ConnectionManager,
        meta_key: &str,
        lease_key: &str,
        identity: LeaseIdentity<'_>,
        ttl_ms: u64,
    ) -> Result<LeaseOutcome, StoreError> {
        let reply: Vec<Value> = self
            .renew
            .key(meta_key)
            .key(lease_key)
            .arg(identity.node_id)
            .arg(identity.fencing_token)
            .arg(ttl_ms)
            .invoke_async(conn)
            .await
            .map_err(super::backend)?;
        decode_lease_reply(&reply)
    }

    pub(crate) async fn release(
        &self,
        conn: &mut ConnectionManager,
        lease_key: &str,
        identity: LeaseIdentity<'_>,
    ) -> Result<(), StoreError> {
        let _: Vec<Value> = self
            .release
            .key(lease_key)
            .arg(identity.node_id)
            .arg(identity.fencing_token)
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
        identity: LeaseIdentity<'_>,
        batch: AppendBatch<'_>,
    ) -> Result<AppendOutcome, StoreError> {
        let mut invocation = self.append.prepare_invoke();
        invocation
            .key(meta_key)
            .key(lease_key)
            .key(log_key)
            .arg(identity.node_id)
            .arg(identity.fencing_token);
        match &batch.mark {
            Some(mark) => {
                // Positional and load-bearing: `APPEND_BODY` reads `KEYS[4]`
                // as the marks hash and `KEYS[5]`/`KEYS[6]` as the marked and
                // pending zsets by index, so this order must agree with
                // `IndexKeys`' field order exactly — the marked and pending
                // zsets carry the same type, so a swap here would pass the
                // Lua type check and corrupt the wrong set silently.
                for key in [
                    mark.index_keys.marks.as_str(),
                    mark.index_keys.marked.as_str(),
                    mark.index_keys.pending.as_str(),
                ] {
                    invocation.key(key);
                }
                invocation
                    .arg(mark.session_id)
                    .arg(mark.event_index + 1)
                    .arg(mark.project);
            }
            None => {
                invocation.arg("").arg("").arg("");
            }
        }
        for payload in batch.kind_payloads {
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
            (Some("PROJECT"), ..) => match str_at(&reply, 1) {
                Some(marked) => Ok(AppendOutcome::ProjectMismatch {
                    marked: marked.to_string(),
                }),
                None => Err(unexpected(&reply)),
            },
            (Some("CORRUPT"), ..) => Err(StoreError::Backend(anyhow::anyhow!(
                "log `{log_key}` ends in entry `{}`, which this store did not write; \
                 refusing to append after a foreign entry",
                str_at(&reply, 1).unwrap_or("<unreadable>")
            ))),
            (Some("WRONGTYPE"), ..) => Err(StoreError::Backend(anyhow::anyhow!(
                "learning index key `{}` holds another type; refusing the marked \
                 append before writing any event",
                str_at(&reply, 1).unwrap_or("<unreadable>")
            ))),
            (Some("BADMARK"), ..) => Err(StoreError::Backend(anyhow::anyhow!(
                "the stored learning mark for this session is unreadable (`{}`); \
                 refusing the marked append before writing any event",
                str_at(&reply, 1).unwrap_or("<unreadable>")
            ))),
            (Some("RANGE"), Some(last), _) => Err(StoreError::Backend(anyhow::anyhow!(
                "log `{log_key}` is at seq {last}; this batch would pass seq {LAST_EXACT_SEQ}, \
                 the last one the append script writes exactly, so the append is refused \
                 before writing any event"
            ))),
            _ => Err(unexpected(&reply)),
        }
    }
}

/// Acquire and renew share a reply shape — and deliberately one refusal tag,
/// because the caller cannot act differently on "held by another" versus
/// "no longer yours": both mean the tenure is not this node's to use.
fn decode_lease_reply(reply: &[Value]) -> Result<LeaseOutcome, StoreError> {
    match (tag_of(reply), int_at(reply, 1)) {
        (Some("OK"), Some(now_ms)) => Ok(LeaseOutcome::Granted { now_ms }),
        (Some("REFUSED"), _) => Ok(LeaseOutcome::Refused),
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
