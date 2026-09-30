// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Redis-backed [`LearnerStore`]: the online routing learner's shared
//! counters (draft section 11.4 of `agent-docs/DRAFT-online-routing-learner.md`,
//! milestone M7 of `agent-docs/PLAN-online-routing-learner.md`).
//!
//! **Nothing calls it yet.** The engine composes a learner store in M8.
//!
//! One project maps to these keys, all sharing a hash tag on the project id:
//!
//! | Key | Type | Holds |
//! |---|---|---|
//! | `rh:v1:learn:{<project>}:wm` | hash | session id → watermark |
//! | `rh:v1:learn:{<project>}:<epoch>:q:<level>:<key>` | hash | `<strategy>:pos`, `<strategy>:n`, `<strategy>:sessions`, `jev_capable`, `jev_efficient` |
//! | `rh:v1:learn:{<project>}:<epoch>:ops` | hash | `<target>:lat_sum`, `:lat_n`, `:failover`, `:cache_pred`, `:cache_obs`, `:cache_n`, and `turn:pre_sum`, `turn:pre_n` |
//! | `rh:v1:learn:{<project>}:<epoch>:seen:<session>` | set | `<level>:<key>:<strategy>` members the session has counted in `sessions` |
//!
//! `<epoch>` is the epoch id in hex, `<key>` is [`LevelKey::part`] and
//! `<target>` is the target's policy identity. The memory backend's module doc
//! names the same keys, one map per row.
//!
//! **Rust names every key and every field; the scripts name none.** Each key
//! reaches a script through `KEYS` and each field through `ARGV`, built by the
//! functions below, so `build_key` is the only spelling of a key and the field
//! names that `read` asks for are the ones `apply` wrote. The key-builder
//! convention test checks both halves: each `fn *_key` here calls
//! `build_key`, and every `redis.call` in the scripts reaches its key through
//! `KEYS[...]`.
//!
//! **Single instance.** The hash tag keeps one project's keys on one Cluster
//! slot, which is what the one-script `apply` needs, but this family claims
//! no Redis Cluster support: the recovery design of draft section 11.6
//! assumes a counter and its watermark live in one replication unit, and no
//! test here runs against a Cluster.
//!
//! The watermark hash and the `seen` sets keep one entry per session forever,
//! the same cost `spend` states for its watermarks. There is no compaction.
//!
//! [`scripts`] holds the two scripts, `read` and `apply`, and states the
//! reply shape: every reply is an array of integers.

mod scripts;

use std::sync::Arc;

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use roundhouse_core::control::ProjectId;
use roundhouse_core::ids::SessionId;
use roundhouse_core::learn_store::{
    Applied, LearnerError, LearnerStore, LearningBatch, MAX_EXACT, ReadRequest,
};
use roundhouse_core::routing::learn::{EpochId, LevelKey, ReadView, Strategy};
use roundhouse_core::session::Deltas;

use crate::keys::{self, KeyNamespace};

/// The hash tag every key of one project carries, first after the family.
fn project_tag(project: &ProjectId) -> String {
    format!("{{{project}}}")
}

/// `{project}:wm`: session id → watermark.
pub(crate) fn watermark_key(namespace: &KeyNamespace, project: &ProjectId) -> String {
    keys::build_key(
        namespace,
        keys::KeyFamily::Learn,
        &[&project_tag(project), "wm"],
    )
}

/// `{project}:<epoch>:q:<level>:<key>`: one quality key of one epoch.
pub(crate) fn quality_key(
    namespace: &KeyNamespace,
    project: &ProjectId,
    epoch: EpochId,
    key: LevelKey,
) -> String {
    keys::build_key(
        namespace,
        keys::KeyFamily::Learn,
        &[
            &project_tag(project),
            &epoch.to_string(),
            "q",
            key.level().label(),
            &key.part(),
        ],
    )
}

/// `{project}:<epoch>:ops`: the operations key of one epoch.
pub(crate) fn ops_key(namespace: &KeyNamespace, project: &ProjectId, epoch: EpochId) -> String {
    keys::build_key(
        namespace,
        keys::KeyFamily::Learn,
        &[&project_tag(project), &epoch.to_string(), "ops"],
    )
}

/// `{project}:<epoch>:seen:<session>`: what one session has counted in
/// `sessions` under one epoch.
pub(crate) fn seen_key(
    namespace: &KeyNamespace,
    project: &ProjectId,
    epoch: EpochId,
    session: &SessionId,
) -> String {
    keys::build_key(
        namespace,
        keys::KeyFamily::Learn,
        &[
            &project_tag(project),
            &epoch.to_string(),
            "seen",
            session.as_str(),
        ],
    )
}

/// The prefix every key of one project starts with, `…:learn:{project}:`.
///
/// For the test-support snapshot's `SCAN`. Built from [`watermark_key`] so it
/// cannot drift from the keys it has to find: every key above shares that
/// key's text up to and including the `:` after the tag.
#[cfg_attr(not(feature = "test-support"), allow(dead_code))]
pub(crate) fn project_prefix(namespace: &KeyNamespace, project: &ProjectId) -> String {
    let mut prefix = watermark_key(namespace, project);
    prefix.truncate(prefix.len() - "wm".len());
    prefix
}

/// One strategy's field in a quality hash: `pos`, `n` or `sessions`.
fn strategy_field(strategy: Strategy, counter: &str) -> String {
    format!("{strategy}:{counter}")
}

/// One target's field in the operations hash.
fn target_field(target: &str, counter: &str) -> String {
    format!("{target}:{counter}")
}

const JEV_CAPABLE: &str = "jev_capable";
const JEV_EFFICIENT: &str = "jev_efficient";
const PRE_SUM: &str = "turn:pre_sum";
const PRE_N: &str = "turn:pre_n";

/// The six counters of a target row, in the order a read returns them, each
/// with whether it is signed. `lat_sum` is the one signed counter of a row;
/// the read's range check and the apply's op code both come from this table.
const TARGET_COUNTERS: [(&str, bool); 6] = [
    ("lat_sum", true),
    ("lat_n", false),
    ("failover", false),
    ("cache_pred", false),
    ("cache_obs", false),
    ("cache_n", false),
];

/// A `seen` member: `<level>:<key>:<strategy>`.
fn seen_member(key: LevelKey, strategy: Strategy) -> String {
    format!("{}:{}:{strategy}", key.level().label(), key.part())
}

/// Redis implementation of [`LearnerStore`].
///
/// Cheap to clone: clones share one auto-reconnecting multiplexed connection,
/// like every other family in this crate.
#[derive(Clone)]
pub struct RedisLearnerStore {
    conn: ConnectionManager,
    scripts: Arc<scripts::Scripts>,
    namespace: KeyNamespace,
}

impl RedisLearnerStore {
    /// Connect under the default namespace (`rh`) and fail fast.
    pub async fn connect(url: impl AsRef<str>) -> Result<Self, LearnerError> {
        Self::connect_namespaced(url, KeyNamespace::default()).await
    }

    /// Connect under an explicit [`KeyNamespace`], which the composition root
    /// reads from `ROUNDHOUSE_REDIS_NAMESPACE` (R-S3).
    pub async fn connect_namespaced(
        url: impl AsRef<str>,
        namespace: KeyNamespace,
    ) -> Result<Self, LearnerError> {
        let conn = crate::connect_manager(url.as_ref())
            .await
            .map_err(unavailable)?;
        Ok(Self {
            conn,
            scripts: Arc::new(scripts::Scripts::new()),
            namespace,
        })
    }

    #[cfg_attr(not(feature = "test-support"), allow(dead_code))]
    pub(crate) fn namespace(&self) -> &KeyNamespace {
        &self.namespace
    }

    #[cfg_attr(not(feature = "test-support"), allow(dead_code))]
    pub(crate) fn connection(&self) -> ConnectionManager {
        self.conn.clone()
    }
}

fn unavailable(error: redis::RedisError) -> LearnerError {
    LearnerError::Unavailable(error.to_string())
}

/// A stored count: decimal digits alone, within `2^53 - 1`. The same rule the
/// apply script's `counter` applies to an unsigned field; a bare `parse`
/// would also take `+5`, which the script refuses, and the two would then
/// disagree on whether a session has a watermark.
fn stored_count(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u64>().ok().filter(|count| *count <= MAX_EXACT)
}

/// What one read asks the script for: the four keys of the turn and the
/// fields of each, in the order the view is built from.
struct ReadPlan {
    /// The three quality keys, most specific first, then the operations key.
    keys: [String; 4],
    /// Asked of each quality key: every strategy's three counters, then the
    /// two Jev counts.
    quality_fields: Vec<String>,
    /// Asked of the operations key: every target's six counters, then the
    /// overhead sum and count.
    ops_fields: Vec<String>,
    /// One letter per field, the quality fields then the operations fields:
    /// `s` for a signed sum, `u` for a count. The script refuses a negative
    /// count itself, so the refusal names the key and field it came from.
    signs: String,
}

impl ReadPlan {
    fn new(namespace: &KeyNamespace, request: &ReadRequest) -> Self {
        let [l2, l1, l0] = *request.keys();
        let quality = |key| quality_key(namespace, request.project(), request.epoch(), key);
        let mut quality_fields = Vec::with_capacity(request.strategies().len() * 3 + 2);
        for &strategy in request.strategies() {
            for counter in ["pos", "n", "sessions"] {
                quality_fields.push(strategy_field(strategy, counter));
            }
        }
        quality_fields.extend([JEV_CAPABLE.to_owned(), JEV_EFFICIENT.to_owned()]);
        let mut signs = "u".repeat(quality_fields.len());
        let mut ops_fields = Vec::with_capacity(request.targets().len() * 6 + 2);
        for target in request.targets() {
            for (counter, signed) in TARGET_COUNTERS {
                ops_fields.push(target_field(target, counter));
                signs.push(sign(signed));
            }
        }
        ops_fields.extend([PRE_SUM.to_owned(), PRE_N.to_owned()]);
        signs.extend([sign(true), sign(false)]);
        Self {
            keys: [
                quality(l2),
                quality(l1),
                quality(l0),
                ops_key(namespace, request.project(), request.epoch()),
            ],
            quality_fields,
            ops_fields,
            signs,
        }
    }

    /// The field at 1-based `ARGV` position `arg`, as `scripts::READ` lays
    /// out `ARGV`: the quality field count and the signs come first.
    fn field(&self, arg: usize) -> Option<&str> {
        self.quality_fields
            .iter()
            .chain(&self.ops_fields)
            .nth(arg.checked_sub(3)?)
            .map(String::as_str)
    }
}

fn sign(signed: bool) -> char {
    if signed { 's' } else { 'u' }
}

/// One argument of the apply script: a field or member name, or an integer.
///
/// Integers cross as their decimal text, which `tonumber` reads exactly up to
/// [`MAX_EXACT`]; [`LearningBatch::check`] has already refused anything past
/// it.
#[derive(Debug)]
enum Arg {
    Text(String),
    Int(i64),
}

impl redis::ToRedisArgs for Arg {
    fn write_redis_args<W: ?Sized + redis::RedisWrite>(&self, out: &mut W) {
        match self {
            Arg::Text(text) => text.write_redis_args(out),
            Arg::Int(number) => number.write_redis_args(out),
        }
    }
}

/// The op codes of the apply script. See `scripts::APPLY`.
const OP_ADD: i64 = 1;
const OP_ADD_SIGNED: i64 = 2;
const OP_SESSION: i64 = 3;

/// What one apply sends the script: every key the batch can touch, and each
/// entry's ops.
///
/// `KEYS` is every key in the order the batch first names it, the watermark
/// hash first. `ARGV` is the session id, a type string with one letter per
/// key (`h` for a hash, `s` for a set) so the script can check every key
/// before it reads any, the entry count, then per entry `seq`, `prev_seq`,
/// the width of its ops and the ops themselves. An op names its keys by
/// their 1-based position in `KEYS`:
///
/// - `OP_ADD key field delta` and `OP_ADD_SIGNED key field delta` add to one
///   counter, which must stay within `0..=2^53 - 1` or `±(2^53 - 1)`;
/// - `OP_SESSION set member key field` adds 1 to `field` the first time
///   `member` is new to the session's `seen` set, stored or staged.
struct ApplyPlan {
    keys: Vec<String>,
    args: Vec<Arg>,
}

impl ApplyPlan {
    fn new(namespace: &KeyNamespace, batch: &LearningBatch<'_>) -> Self {
        let mut layout = Layout::default();
        layout.position(watermark_key(namespace, batch.project), HASH);
        let mut body = Vec::new();
        for entry in batch.entries {
            let mut ops = Vec::new();
            if let Some(deltas) = &entry.deltas {
                OpBuilder {
                    namespace,
                    batch,
                    layout: &mut layout,
                    ops: &mut ops,
                }
                .add(deltas);
            }
            body.extend([
                Arg::Int(entry.seq as i64),
                Arg::Int(entry.prev_seq as i64),
                Arg::Int(ops.len() as i64),
            ]);
            body.extend(ops);
        }
        let mut args = vec![
            Arg::Text(batch.session.as_str().to_owned()),
            Arg::Text(layout.types),
            Arg::Int(batch.entries.len() as i64),
        ];
        args.extend(body);
        Self {
            keys: layout.keys,
            args,
        }
    }

    /// The counter a `CounterRange` reply names: the key and the field,
    /// spelled the way `HGET` takes them. `None` for a position outside
    /// `KEYS` or `ARGV`, which only another build's script could report.
    fn counter(&self, key: usize, field_arg: usize) -> Option<String> {
        let key = self.keys.get(key.checked_sub(1)?)?;
        let field = match self.args.get(field_arg.checked_sub(1)?)? {
            Arg::Text(text) => text.clone(),
            Arg::Int(number) => number.to_string(),
        };
        Some(format!("{key} {field}"))
    }
}

/// The type letters of `ApplyPlan`'s type string.
const HASH: char = 'h';
const SET: char = 's';

/// The apply's `KEYS` as the ops name them, and the type of each.
#[derive(Default)]
struct Layout {
    keys: Vec<String>,
    /// One of [`HASH`] or [`SET`] per key of `keys`.
    types: String,
}

impl Layout {
    /// The 1-based position of `key` in `KEYS`, as Lua indexes it, appended
    /// with its type the first time it is named.
    fn position(&mut self, key: String, kind: char) -> i64 {
        let index = match self.keys.iter().position(|existing| *existing == key) {
            Some(index) => {
                debug_assert_eq!(
                    self.types.as_bytes()[index],
                    kind as u8,
                    "`{key}` was named {} but is now requested as {kind}",
                    self.types.as_bytes()[index] as char
                );
                index
            }
            None => {
                self.keys.push(key);
                self.types.push(kind);
                self.keys.len() - 1
            }
        };
        index as i64 + 1
    }
}

/// Turns one entry's deltas into ops, in the order the memory backend's
/// `Staged::add` stages them, so both backends refuse the same step of a
/// batch that leaves a range.
struct OpBuilder<'p, 'b> {
    namespace: &'p KeyNamespace,
    batch: &'p LearningBatch<'b>,
    layout: &'p mut Layout,
    ops: &'p mut Vec<Arg>,
}

impl OpBuilder<'_, '_> {
    fn counter(&mut self, key: i64, field: String, delta: i64, signed: bool) {
        self.ops.extend([
            Arg::Int(if signed { OP_ADD_SIGNED } else { OP_ADD }),
            Arg::Int(key),
            Arg::Text(field),
            Arg::Int(delta),
        ]);
    }

    fn add(&mut self, deltas: &Deltas) {
        let (namespace, project, epoch) = (self.namespace, self.batch.project, deltas.epoch);
        for quality in &deltas.quality {
            let key = self
                .layout
                .position(quality_key(namespace, project, epoch, quality.key), HASH);
            let set = self
                .layout
                .position(seen_key(namespace, project, epoch, self.batch.session), SET);
            let field = |counter| strategy_field(quality.strategy, counter);
            self.counter(key, field("pos"), quality.units.pos as i64, false);
            self.counter(key, field("n"), quality.units.n as i64, false);
            self.ops.extend([
                Arg::Int(OP_SESSION),
                Arg::Int(set),
                Arg::Text(seen_member(quality.key, quality.strategy)),
                Arg::Int(key),
                Arg::Text(field("sessions")),
            ]);
        }
        for jev in &deltas.jev {
            let key = self
                .layout
                .position(quality_key(namespace, project, epoch, jev.key), HASH);
            for (field, delta) in [
                (JEV_CAPABLE, jev.counts.capable),
                (JEV_EFFICIENT, jev.counts.efficient),
            ] {
                self.counter(key, field.to_owned(), delta as i64, false);
            }
        }
        // The memory backend leaves the operations key alone when an entry
        // has no row for it; so does this one.
        if deltas.targets.is_empty() && deltas.overhead.n == 0 && deltas.overhead.sum_ms == 0 {
            return;
        }
        let key = self
            .layout
            .position(ops_key(namespace, project, epoch), HASH);
        for target in &deltas.targets {
            let values = [
                target.latency.sum_ms,
                target.latency.n as i64,
                target.failover as i64,
                target.cache.predicted_permille as i64,
                target.cache.observed_permille as i64,
                target.cache.n as i64,
            ];
            for ((counter, signed), delta) in TARGET_COUNTERS.into_iter().zip(values) {
                self.counter(key, target_field(&target.target, counter), delta, signed);
            }
        }
        self.counter(key, PRE_SUM.to_owned(), deltas.overhead.sum_ms, true);
        self.counter(key, PRE_N.to_owned(), deltas.overhead.n as i64, false);
    }
}

#[async_trait]
impl LearnerStore for RedisLearnerStore {
    async fn read(&self, request: &ReadRequest) -> Result<ReadView, LearnerError> {
        let plan = ReadPlan::new(&self.namespace, request);
        let reply = self.scripts.read(&mut self.conn.clone(), &plan).await?;
        scripts::decode_read(&reply, &plan, request)
    }

    async fn apply(&self, batch: &LearningBatch<'_>) -> Result<Applied, LearnerError> {
        batch.check()?;
        let plan = ApplyPlan::new(&self.namespace, batch);
        let reply = self.scripts.apply(&mut self.conn.clone(), &plan).await?;
        scripts::decode_apply(&reply, &plan)
    }

    async fn watermark(
        &self,
        project: &ProjectId,
        session: &SessionId,
    ) -> Result<u64, LearnerError> {
        let key = watermark_key(&self.namespace, project);
        let stored: Option<String> = redis::cmd("HGET")
            .arg(&key)
            .arg(session.as_str())
            .query_async(&mut self.conn.clone())
            .await
            .map_err(|error| match error.code() {
                Some("WRONGTYPE") => LearnerError::WrongType { key: key.clone() },
                _ => unavailable(error),
            })?;
        match stored {
            None => Ok(0),
            Some(text) => stored_count(&text).ok_or_else(|| {
                LearnerError::Unavailable(format!(
                    "`{key}` holds `{text}` for session `{session}`, not a watermark"
                ))
            }),
        }
    }
}

#[cfg(test)]
mod tests;
