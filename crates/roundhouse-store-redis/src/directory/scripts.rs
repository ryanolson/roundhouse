// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The one directory write, and the only thing in this family that has a
//! condition in it.
//!
//! `load` and `version` are each one `HMGET`, because neither asks a question
//! of the state it reads. `commit` asks exactly one — "is the store
//! still at the version this writer read" — and it is precisely the question
//! that must not be evaluated against a value another node is replacing. Two
//! nodes that each read version *n*, each validated a change, and each wrote
//! would leave one of the two changes gone with a `2xx` already returned for
//! it: for this family that is a revocation overwritten by a concurrent
//! rename, which is the state the whole compare-and-set seam exists to
//! prevent. A Redis script executes with nothing in between, so the read, the
//! comparison and the write are one indivisible step.
//!
//! **No clock at all**, which makes `crate::scripts`' "all time comes from
//! `redis.call('TIME')`" convention vacuous here rather than departed from:
//! nothing in this family expires, because a directory is the deployment's
//! tenancy and not a guess with a staleness bound. There is no `PEXPIRE`
//! anywhere in this module, and that absence is the decision — a TTL on this
//! key would silently un-configure a deployment that had a quiet week.
//!
//! Lua numbers are doubles and exact through 2^53; this counter counts admin
//! writes over the life of a deployment and sits nowhere near that — and the
//! grammar below refuses a stored counter of more than fifteen digits, so
//! "nowhere near" is enforced rather than assumed (M16.1 review, F3).

use redis::Value;
use redis::aio::ConnectionManager;
use roundhouse_core::control::directory::{DocumentStoreError, DocumentVersion};

/// Replace the document if and only if the stored version is the expected one.
///
/// Three outcomes, and the middle one is the reason this is not a `HSETNX` or
/// a `WATCH`/`MULTI`:
///
/// - **the versions agree** — the write is admitted, both fields move together
///   in one `HSET` (so no reader can see a version that does not match the
///   bytes beside it), and the new version is returned;
/// - **they disagree** — the write is refused and the version actually found
///   comes back, so Rust can name *both* numbers in `Concurrent` rather than
///   telling the caller only that it lost;
/// - **the key is not one this store wrote** — a foreign writer, an `HDEL`, a
///   half-finished restore — and the script refuses rather than treating it as
///   zero. Read as zero, this very commit would be admitted and would
///   overwrite whatever is there; the *absent key* case is different and is
///   genuinely zero, because an absent key is the empty directory by contract.
///
/// The absent-key branch is what makes the first commit of a deployment's life
/// work with no seeding step: `commit(0, ..)` against a Redis that has never
/// held this key is the ordinary first write, not a `Concurrent`. It is also
/// where a **lineage** is minted (R-D2″, M16.1 review's F1): the candidate
/// rides in as an argument because a script may not call for randomness, and
/// it is used *only* on this branch, so a key that was deleted, flushed or
/// half-restored starts a new lineage at version 1 instead of silently
/// re-issuing versions some node is already serving.
///
/// # One grammar, checked here and in the Rust decoder (M16.1 review, F3)
///
/// The key this store wrote holds `version` (one to fifteen decimal digits,
/// never zero — the counter starts at one) and `lineage` (this store's own
/// shape) together, or the key does not exist at all. Anything else is
/// refused, and refused *the same way the read path refuses it*: `tonumber`
/// alone would take hex, exponent and whitespace forms that `str::parse::<u64>`
/// refuses, so the two halves of "a field this store did not write fails
/// loudly" would have disagreed about the same key — one clobbering it at some
/// number it invented, the other calling it corrupt.
const COMMIT: &str = r"
local held = redis.call('HMGET', KEYS[1], ARGV[1], ARGV[5])
local held_version = held[1]
local held_lineage = held[2]
local current
local lineage
if held_version == false and held_lineage == false then
  if redis.call('EXISTS', KEYS[1]) == 1 then
    return {'CORRUPT', 'exists but holds neither `' .. ARGV[1] .. '` nor `' .. ARGV[5] .. '`'}
  end
  current = 0
  lineage = ARGV[6]
else
  if held_version == false or held_lineage == false then
    return {'CORRUPT', 'holds one of `' .. ARGV[1] .. '` and `' .. ARGV[5] .. '` without the other'}
  end
  if string.match(held_version, '^%d+$') == nil or #held_version > 15 then
    return {'CORRUPT', 'holds `' .. ARGV[1] .. '` = `' .. held_version .. '`, which is not a version'}
  end
  current = tonumber(held_version)
  if current == 0 then
    return {'CORRUPT', 'holds `' .. ARGV[1] .. '` = `' .. held_version .. '`, and this counter starts at one'}
  end
  if string.match(held_lineage, '^[0-9a-f%-]+$') == nil or #held_lineage > 64 then
    return {'CORRUPT', 'holds `' .. ARGV[5] .. '` = `' .. held_lineage .. '`, which is not a lineage'}
  end
  lineage = held_lineage
end
if string.match(ARGV[3], '^%d+$') == nil or #ARGV[3] > 15 then
  return {'BADEXPECTED', ARGV[3]}
end
local expected = tonumber(ARGV[3])
if current ~= expected then
  return {'STALE', current}
end
local next_version = current + 1
redis.call('HSET', KEYS[1], ARGV[1], next_version, ARGV[5], lineage, ARGV[2], ARGV[4])
return {'OK', next_version, lineage}
";

/// The family's scripts, compiled once per handle.
///
/// `redis::Script` sends `EVALSHA` and falls back to `EVAL` on `NOSCRIPT`, so
/// a restarted or failed-over Redis re-learns them transparently.
pub(crate) struct Scripts {
    commit: redis::Script,
}

impl Scripts {
    pub(crate) fn new() -> Self {
        Self {
            commit: redis::Script::new(COMMIT),
        }
    }

    /// Run [`COMMIT`] and turn its tagged reply into the trait's vocabulary.
    ///
    /// The field names ride as `ARGV` rather than being spelled in the Lua,
    /// so there is exactly one place in this crate that knows what the three
    /// fields are called and no way for the script and the readers beside it
    /// to drift apart.
    ///
    /// `lineage_candidate` is minted per call and used only if the key holds
    /// no lineage of its own — a script may not call `math.random` or `TIME`
    /// for a value it stores, so the randomness has to arrive from Rust, and
    /// arriving unconditionally is what keeps the *decision* (is this key
    /// already ours) inside the one atomic step.
    pub(crate) async fn commit(
        &self,
        conn: &mut ConnectionManager,
        key: &str,
        expected_version: u64,
        document: Vec<u8>,
        lineage_candidate: &str,
    ) -> Result<DocumentVersion, DocumentStoreError> {
        let reply: Vec<Value> = self
            .commit
            .key(key)
            .arg(super::VERSION_FIELD)
            .arg(super::DOCUMENT_FIELD)
            .arg(expected_version)
            .arg(document)
            .arg(super::LINEAGE_FIELD)
            .arg(lineage_candidate)
            .invoke_async(conn)
            .await
            .map_err(super::backend_at(key))?;

        match (tag_of(&reply), int_at(&reply, 1)) {
            (Some("OK"), Some(version)) => Ok(DocumentVersion {
                lineage: str_at(&reply, 2)
                    .ok_or_else(|| {
                        DocumentStoreError::Unavailable(format!(
                            "the directory commit script admitted a write at `{key}` without \
                             naming the lineage it wrote it in: {reply:?}"
                        ))
                    })?
                    .to_string(),
                version,
            }),
            (Some("STALE"), Some(found)) => Err(DocumentStoreError::Concurrent {
                expected: expected_version,
                found,
            }),
            (Some("CORRUPT"), _) => Err(DocumentStoreError::Unavailable(format!(
                "directory key `{key}` {}; refusing to overwrite a key this store did not write",
                str_at(&reply, 1).unwrap_or("is not one this store wrote")
            ))),
            // Unreachable through this method, whose `expected_version` is a
            // `u64` and is rendered by the client as plain digits. Kept
            // because the script validates it anyway: the grammar it enforces
            // on the *stored* field is worth nothing if the argument it
            // compares against could be a shape the two halves read
            // differently, and a guard whose failure had no name would surface
            // as `an unexpected reply`.
            (Some("BADEXPECTED"), _) => Err(DocumentStoreError::Unavailable(format!(
                "the directory commit script was given `{}` as the expected version of \
                 `{key}`, which is not one",
                str_at(&reply, 1).unwrap_or("<unreadable>")
            ))),
            _ => Err(DocumentStoreError::Unavailable(format!(
                "the directory commit script returned an unexpected reply: {reply:?}"
            ))),
        }
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
