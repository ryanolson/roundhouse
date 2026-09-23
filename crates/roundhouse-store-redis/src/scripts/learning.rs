// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learning index's scripts: clear, requeue, and one page script for both
//! enumerations. The marked append that writes the index is `APPEND_BODY` in
//! the parent module; it shares this module's prelude, so the stored mark
//! format and its comparisons are spelled once.
//!
//! Three keys per namespace, under the session family:
//!
//! | Key | Type | Holds |
//! |---|---|---|
//! | `…:learning:marks` | hash | session id → `<seq>:<marked_at_ms>:<project>`, permanent |
//! | `…:learning:marked` | sorted set, every score 0 | every session ever marked, permanent |
//! | `…:learning:pending` | sorted set, every score 0 | sessions whose mark is not confirmed delivered |
//!
//! Both sets hold every member at score 0 so `ZRANGE … BYLEX` pages them in
//! session id byte order from an exclusive cursor. That is the whole reason
//! they are sorted sets: Redis's lexicographic range is only defined when all
//! scores are equal, and it is the one ordered range Redis offers whose resume
//! point is exact and whose page size is a hard bound (`HSCAN`'s `COUNT` is a
//! hint and may return a whole small hash at once). The project sits last in
//! the stored mark so it may contain `:`.
//!
//! Durability and atomicity are the session log's, no more: each script is
//! one atomic step on one Redis, and the index is exactly as durable as that
//! Redis. The recovery design assumes the index and the log share one
//! replication unit — a failover that kept one and lost the other would break
//! it, and nothing here can detect that. A script that fails part-way keeps
//! its earlier writes; the only script here that writes more than one key is
//! the marked append, which checks everything before its first write.

use std::num::NonZeroUsize;

use redis::Value;
use redis::aio::ConnectionManager;
use roundhouse_core::control::ProjectId;
use roundhouse_core::ids::SessionId;
use roundhouse_core::store::{
    ClearOutcome, LearningCursor, LearningPage, MarkedSession, RequeueOutcome, StoreError,
};

use super::{int_at, str_at, tag_of, unexpected};

/// The largest sequence the append script writes exactly. Lua's `..` renders
/// numbers with `%.14g`, so `10^14` would come out as `1e+14`.
pub(crate) const LAST_EXACT_SEQ: u64 = 99_999_999_999_999;

/// Lua shared by every script that reads or writes a stored mark.
///
/// `covers` compares canonical decimal strings digit by digit rather than
/// through `tonumber`, so a confirmed watermark anywhere in the `u64` range —
/// including values a double cannot hold exactly — compares exactly against a
/// stored sequence.
const MARK_FUNCTIONS: &str = r"
local function parse_mark(value)
  return string.match(value, '^([1-9]%d*):(%d+):(.*)$')
end
local function covers(seq, through)
  if #seq ~= #through then return #seq < #through end
  for i = 1, #seq do
    local a, b = string.byte(seq, i), string.byte(through, i)
    if a ~= b then return a < b end
  end
  return true
end
local function is_type_or_absent(key, want)
  local found = redis.call('TYPE', key).ok
  return found == want or found == 'none'
end
";

/// The prelude every learning-aware script starts with.
pub(super) fn mark_prelude() -> String {
    format!("local LAST_EXACT_SEQ = {LAST_EXACT_SEQ}\n{MARK_FUNCTIONS}")
}

/// Drop pending membership if the confirmed watermark covers the current
/// mark. KEYS: marks, pending. ARGV: session id, confirmed watermark.
const CLEAR_BODY: &str = r"
local stored = redis.call('HGET', KEYS[1], ARGV[1])
if not stored then return {'UNMARKED'} end
local seq = parse_mark(stored)
if not seq then return {'BADMARK', stored} end
if covers(seq, ARGV[2]) then
  redis.call('ZREM', KEYS[2], ARGV[1])
  return {'COVERED'}
end
return {'NEWER', seq}
";

/// Restore pending membership if the current mark is still the named one.
/// KEYS: marks, pending. ARGV: session id, mark seq. Equality is on the
/// canonical decimal, so it is exact for any `u64`.
const REQUEUE_BODY: &str = r"
local stored = redis.call('HGET', KEYS[1], ARGV[1])
if not stored then return {'UNMARKED'} end
local seq = parse_mark(stored)
if not seq then return {'BADMARK', stored} end
if seq ~= ARGV[2] then return {'MISMATCH', seq} end
redis.call('ZADD', KEYS[2], 0, ARGV[1])
return {'REQUEUED'}
";

/// One page of a membership set, with each member's stored mark.
///
/// KEYS: marks, the set to page (marked or pending). ARGV: the exclusive lex
/// start (`-` or `(<session id>`), the limit, and the idle window in ms or
/// `''` for none. The idle cutoff is computed from `TIME` here, the clock the
/// append stamped the marks with. Every examined member moves the cursor,
/// idle or not; a member with no stored mark is corruption and fails the page
/// rather than being skipped, since skipping would drop it from recovery.
///
/// Reply: `OK`, the number examined, the last member examined (or `''`), then
/// four fields per returned member: id, seq, marked-at, project.
const PAGE_BODY: &str = r"
local members = redis.call('ZRANGE', KEYS[2], ARGV[1], '+', 'BYLEX', 'LIMIT', 0, ARGV[2])
local cutoff
if ARGV[3] ~= '' then
  local t = redis.call('TIME')
  cutoff = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000) - tonumber(ARGV[3])
end
local reply = {'OK', #members, members[#members] or ''}
for _, member in ipairs(members) do
  local stored = redis.call('HGET', KEYS[1], member)
  if not stored then return {'ORPHAN', member} end
  local seq, at, project = parse_mark(stored)
  if not seq then return {'BADMARK', stored} end
  if cutoff == nil or tonumber(at) <= cutoff then
    reply[#reply + 1] = member
    reply[#reply + 1] = seq
    reply[#reply + 1] = at
    reply[#reply + 1] = project
  end
end
return reply
";

/// The learning scripts, compiled once per store beside the session scripts.
pub(crate) struct LearningScripts {
    clear: redis::Script,
    requeue: redis::Script,
    page: redis::Script,
}

/// The index keys a learning script call needs, built once by the caller.
pub(crate) struct IndexKeys<'a> {
    pub(crate) marks: &'a str,
    pub(crate) marked: &'a str,
    pub(crate) pending: &'a str,
}

/// Which membership set a page walks.
pub(crate) enum PageOf {
    /// Every session ever marked.
    Marked,
    /// Pending sessions marked at least this many ms ago.
    Pending { idle_for_ms: u64 },
}

impl LearningScripts {
    pub(super) fn new() -> Self {
        let prelude = mark_prelude();
        Self {
            clear: redis::Script::new(&format!("{prelude}\n{CLEAR_BODY}")),
            requeue: redis::Script::new(&format!("{prelude}\n{REQUEUE_BODY}")),
            page: redis::Script::new(&format!("{prelude}\n{PAGE_BODY}")),
        }
    }

    pub(crate) async fn clear(
        &self,
        conn: &mut ConnectionManager,
        keys: &IndexKeys<'_>,
        session_id: &SessionId,
        confirmed_through: u64,
    ) -> Result<ClearOutcome, StoreError> {
        let reply: Vec<Value> = self
            .clear
            .key(keys.marks)
            .key(keys.pending)
            .arg(session_id.as_str())
            .arg(confirmed_through.to_string())
            .invoke_async(conn)
            .await
            .map_err(crate::backend)?;
        match tag_of(&reply) {
            Some("COVERED") => Ok(ClearOutcome::Covered),
            Some("NEWER") => Ok(ClearOutcome::Newer {
                mark_seq: seq_at(&reply, 1)?,
            }),
            Some("UNMARKED") => Ok(ClearOutcome::Unmarked),
            Some("BADMARK") => Err(bad_mark(&reply, session_id)),
            _ => Err(unexpected(&reply)),
        }
    }

    pub(crate) async fn requeue(
        &self,
        conn: &mut ConnectionManager,
        keys: &IndexKeys<'_>,
        session_id: &SessionId,
        mark_seq: u64,
    ) -> Result<RequeueOutcome, StoreError> {
        let reply: Vec<Value> = self
            .requeue
            .key(keys.marks)
            .key(keys.pending)
            .arg(session_id.as_str())
            .arg(mark_seq.to_string())
            .invoke_async(conn)
            .await
            .map_err(crate::backend)?;
        match tag_of(&reply) {
            Some("REQUEUED") => Ok(RequeueOutcome::Requeued),
            Some("MISMATCH") => Ok(RequeueOutcome::Mismatch {
                mark_seq: seq_at(&reply, 1)?,
            }),
            Some("UNMARKED") => Ok(RequeueOutcome::Unmarked),
            Some("BADMARK") => Err(bad_mark(&reply, session_id)),
            _ => Err(unexpected(&reply)),
        }
    }

    pub(crate) async fn page(
        &self,
        conn: &mut ConnectionManager,
        keys: &IndexKeys<'_>,
        of: PageOf,
        after: Option<&LearningCursor>,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        let (set, idle) = match of {
            PageOf::Marked => (keys.marked, String::new()),
            PageOf::Pending { idle_for_ms } => (keys.pending, idle_for_ms.to_string()),
        };
        let start = after.map_or_else(
            || "-".to_string(),
            |cursor| format!("({}", cursor.session_id()),
        );
        let reply: Vec<Value> = self
            .page
            .key(keys.marks)
            .key(set)
            .arg(start)
            .arg(limit.get())
            .arg(idle)
            .invoke_async(conn)
            .await
            .map_err(crate::backend)?;
        decode_page(&reply, set, limit)
    }
}

fn decode_page(
    reply: &[Value],
    set: &str,
    limit: NonZeroUsize,
) -> Result<LearningPage, StoreError> {
    match tag_of(reply) {
        Some("OK") => {}
        Some("ORPHAN") => {
            return Err(StoreError::Backend(anyhow::anyhow!(
                "`{set}` lists session `{}` with no stored learning mark; refusing \
                 to page an index that would drop it from recovery",
                str_at(reply, 1).unwrap_or("<unreadable>")
            )));
        }
        Some("BADMARK") => {
            return Err(StoreError::Backend(anyhow::anyhow!(
                "a stored learning mark behind `{set}` is unreadable (`{}`)",
                str_at(reply, 1).unwrap_or("<unreadable>")
            )));
        }
        _ => return Err(unexpected(reply)),
    }
    let examined = int_at(reply, 1).ok_or_else(|| unexpected(reply))?;
    let last = str_at(reply, 2).ok_or_else(|| unexpected(reply))?;
    let fields = &reply[3..];
    if !fields.len().is_multiple_of(4) {
        return Err(unexpected(reply));
    }
    let sessions = fields
        .chunks_exact(4)
        .map(|entry| {
            let text = |index| str_at(entry, index).ok_or_else(|| unexpected(reply));
            Ok(MarkedSession {
                session_id: SessionId::new(text(0)?),
                seq: parse_seq(text(1)?, reply)?,
                marked_at_ms: parse_seq(text(2)?, reply)?,
                project: ProjectId::new(text(3)?),
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    // A full page may have more behind it; a short one reached the end.
    let next = (usize::try_from(examined).ok() == Some(limit.get()))
        .then(|| LearningCursor::after(SessionId::new(last)));
    Ok(LearningPage { sessions, next })
}

fn seq_at(reply: &[Value], index: usize) -> Result<u64, StoreError> {
    let text = str_at(reply, index).ok_or_else(|| unexpected(reply))?;
    parse_seq(text, reply)
}

fn parse_seq(text: &str, reply: &[Value]) -> Result<u64, StoreError> {
    text.parse().map_err(|_| unexpected(reply))
}

fn bad_mark(reply: &[Value], session_id: &SessionId) -> StoreError {
    StoreError::Backend(anyhow::anyhow!(
        "the stored learning mark for session `{session_id}` is unreadable (`{}`)",
        str_at(reply, 1).unwrap_or("<unreadable>")
    ))
}
