// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The one directory write, and the only thing in this family that has a
//! condition in it.
//!
//! `load` is an `HMGET` and `version` is an `HGET`, because neither asks a
//! question of the state it reads. `commit` asks exactly one — "is the store
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
//! writes over the life of a deployment and sits nowhere near that.

use redis::Value;
use redis::aio::ConnectionManager;
use roundhouse_core::control::directory::DocumentStoreError;

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
/// - **the stored version is not a number** — a foreign writer owns this key,
///   and the script refuses rather than treating it as zero. Read as zero,
///   this very commit would be admitted and would overwrite whatever is there;
///   the `false` case is different and *is* zero, because an absent key is the
///   empty directory by contract.
///
/// The absent-key branch is what makes the first commit of a deployment's life
/// work with no seeding step: `commit(0, ..)` against a Redis that has never
/// held this key is the ordinary first write, not a `Concurrent`.
const COMMIT: &str = r"
local held = redis.call('HGET', KEYS[1], ARGV[1])
local current
if held == false then
  current = 0
else
  current = tonumber(held)
  if current == nil or current < 0 or current ~= math.floor(current) then
    return {'CORRUPT', tostring(held)}
  end
end
local expected = tonumber(ARGV[3])
if current ~= expected then
  return {'STALE', current}
end
local next_version = current + 1
redis.call('HSET', KEYS[1], ARGV[1], next_version, ARGV[2], ARGV[4])
return {'OK', next_version}
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
    /// so there is exactly one place in this crate that knows what the two
    /// fields are called and no way for the script and the readers beside it
    /// to drift apart.
    pub(crate) async fn commit(
        &self,
        conn: &mut ConnectionManager,
        key: &str,
        expected_version: u64,
        document: Vec<u8>,
    ) -> Result<u64, DocumentStoreError> {
        let reply: Vec<Value> = self
            .commit
            .key(key)
            .arg(super::VERSION_FIELD)
            .arg(super::DOCUMENT_FIELD)
            .arg(expected_version)
            .arg(document)
            .invoke_async(conn)
            .await
            .map_err(super::backend_at(key))?;

        match (tag_of(&reply), int_at(&reply, 1)) {
            (Some("OK"), Some(version)) => Ok(version),
            (Some("STALE"), Some(found)) => Err(DocumentStoreError::Concurrent {
                expected: expected_version,
                found,
            }),
            (Some("CORRUPT"), _) => Err(DocumentStoreError::Unavailable(format!(
                "directory key `{key}` holds `{}` = `{}`, which is not a version; refusing to \
                 overwrite a key this store did not write",
                super::VERSION_FIELD,
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
