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

/// The six counters of a target row, in the order a read returns them.
const TARGET_COUNTERS: [&str; 6] = [
    "lat_sum",
    "lat_n",
    "failover",
    "cache_pred",
    "cache_obs",
    "cache_n",
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
        let mut ops_fields = Vec::with_capacity(request.targets().len() * 6 + 2);
        for target in request.targets() {
            for counter in TARGET_COUNTERS {
                ops_fields.push(target_field(target, counter));
            }
        }
        ops_fields.extend([PRE_SUM.to_owned(), PRE_N.to_owned()]);
        Self {
            keys: [
                quality(l2),
                quality(l1),
                quality(l0),
                ops_key(namespace, request.project(), request.epoch()),
            ],
            quality_fields,
            ops_fields,
        }
    }
}

/// One argument of the apply script: a field or member name, or an integer.
///
/// Integers cross as their decimal text, which `tonumber` reads exactly up to
/// [`MAX_EXACT`]; [`LearningBatch::check`] has already refused anything past
/// it.
#[derive(Debug, Clone)]
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
/// `KEYS` is the watermark hash, then every counter hash, then every `seen`
/// set, so the script can check each key against the type its position gives
/// it before it reads any. `ARGV` is the session id, the count of hash keys
/// (the watermark included), the entry count, then per entry `seq`,
/// `prev_seq`, the width of its ops and the ops themselves. An op names its
/// keys by their 1-based position in `KEYS`:
///
/// - `OP_ADD key field delta` and `OP_ADD_SIGNED key field delta` add to one
///   counter, which must stay within `0..=2^53 - 1` or `±(2^53 - 1)`;
/// - `OP_SESSION set member key field` adds 1 to `field` the first time
///   `member` is new to the session's `seen` set, stored or staged.
struct ApplyPlan {
    keys: Vec<String>,
    args: Vec<Arg>,
}

/// A key an op names before the plan knows where the sets start.
#[derive(Clone, Copy)]
enum Slot {
    Hash(usize),
    Set(usize),
}

impl ApplyPlan {
    fn new(namespace: &KeyNamespace, batch: &LearningBatch<'_>) -> Self {
        let mut hashes: Vec<String> = vec![watermark_key(namespace, batch.project)];
        let mut sets: Vec<String> = Vec::new();
        let mut entries: Vec<(u64, u64, Vec<Op>)> = Vec::with_capacity(batch.entries.len());
        for entry in batch.entries {
            let mut ops = Vec::new();
            if let Some(deltas) = &entry.deltas {
                let mut builder = OpBuilder {
                    namespace,
                    batch,
                    hashes: &mut hashes,
                    sets: &mut sets,
                    ops: &mut ops,
                };
                builder.add(deltas);
            }
            entries.push((entry.seq, entry.prev_seq, ops));
        }
        let hash_count = hashes.len();
        let position = |slot: Slot| -> i64 {
            let index = match slot {
                Slot::Hash(index) => index,
                Slot::Set(index) => hash_count + index,
            };
            // 1-based, as Lua indexes `KEYS`.
            index as i64 + 1
        };
        let mut args = vec![
            Arg::Text(batch.session.as_str().to_owned()),
            Arg::Int(hash_count as i64),
            Arg::Int(entries.len() as i64),
        ];
        for (seq, prev_seq, ops) in entries {
            let mut encoded = Vec::new();
            for op in ops {
                op.encode(&position, &mut encoded);
            }
            args.push(Arg::Int(seq as i64));
            args.push(Arg::Int(prev_seq as i64));
            args.push(Arg::Int(encoded.len() as i64));
            args.extend(encoded);
        }
        let mut keys = hashes;
        keys.extend(sets);
        Self { keys, args }
    }

    /// The counter a `CounterRange` reply names: the key and the field,
    /// spelled the way `HGET` takes them.
    fn counter(&self, key: usize, field_arg: usize) -> String {
        let key = self
            .keys
            .get(key.wrapping_sub(1))
            .map_or("<unknown key>", String::as_str);
        let field = match self.args.get(field_arg.wrapping_sub(1)) {
            Some(Arg::Text(text)) => text.clone(),
            Some(Arg::Int(number)) => number.to_string(),
            None => "<unknown field>".to_owned(),
        };
        format!("{key} {field}")
    }
}

enum Op {
    Add {
        key: Slot,
        field: String,
        delta: i64,
        signed: bool,
    },
    Session {
        set: Slot,
        member: String,
        key: Slot,
        field: String,
    },
}

impl Op {
    fn encode(self, position: &impl Fn(Slot) -> i64, out: &mut Vec<Arg>) {
        match self {
            Op::Add {
                key,
                field,
                delta,
                signed,
            } => out.extend([
                Arg::Int(if signed { OP_ADD_SIGNED } else { OP_ADD }),
                Arg::Int(position(key)),
                Arg::Text(field),
                Arg::Int(delta),
            ]),
            Op::Session {
                set,
                member,
                key,
                field,
            } => out.extend([
                Arg::Int(OP_SESSION),
                Arg::Int(position(set)),
                Arg::Text(member),
                Arg::Int(position(key)),
                Arg::Text(field),
            ]),
        }
    }
}

/// Turns one entry's deltas into ops, in the order the memory backend's
/// `Staged::add` stages them, so both backends refuse the same step of a
/// batch that leaves a range.
struct OpBuilder<'p, 'b> {
    namespace: &'p KeyNamespace,
    batch: &'p LearningBatch<'b>,
    hashes: &'p mut Vec<String>,
    sets: &'p mut Vec<String>,
    ops: &'p mut Vec<Op>,
}

impl OpBuilder<'_, '_> {
    fn hash(&mut self, key: String) -> Slot {
        Slot::Hash(slot_of(self.hashes, key))
    }

    fn set(&mut self, key: String) -> Slot {
        Slot::Set(slot_of(self.sets, key))
    }

    fn add(&mut self, deltas: &Deltas) {
        let (namespace, project, epoch) = (self.namespace, self.batch.project, deltas.epoch);
        for quality in &deltas.quality {
            let key = self.hash(quality_key(namespace, project, epoch, quality.key));
            let set = self.set(seen_key(namespace, project, epoch, self.batch.session));
            let unsigned = |counter: &str, delta: u64| Op::Add {
                key,
                field: strategy_field(quality.strategy, counter),
                delta: delta as i64,
                signed: false,
            };
            self.ops.push(unsigned("pos", quality.units.pos));
            self.ops.push(unsigned("n", quality.units.n));
            self.ops.push(Op::Session {
                set,
                member: seen_member(quality.key, quality.strategy),
                key,
                field: strategy_field(quality.strategy, "sessions"),
            });
        }
        for jev in &deltas.jev {
            let key = self.hash(quality_key(namespace, project, epoch, jev.key));
            for (field, delta) in [
                (JEV_CAPABLE, jev.counts.capable),
                (JEV_EFFICIENT, jev.counts.efficient),
            ] {
                self.ops.push(Op::Add {
                    key,
                    field: field.to_owned(),
                    delta: delta as i64,
                    signed: false,
                });
            }
        }
        // The memory backend leaves the operations key alone when an entry
        // has no row for it; so does this one.
        if deltas.targets.is_empty() && deltas.overhead.n == 0 && deltas.overhead.sum_ms == 0 {
            return;
        }
        let key = self.hash(ops_key(namespace, project, epoch));
        for target in &deltas.targets {
            let values = [
                target.latency.sum_ms,
                target.latency.n as i64,
                target.failover as i64,
                target.cache.predicted_permille as i64,
                target.cache.observed_permille as i64,
                target.cache.n as i64,
            ];
            for (index, (counter, delta)) in TARGET_COUNTERS.into_iter().zip(values).enumerate() {
                self.ops.push(Op::Add {
                    key,
                    field: target_field(&target.target, counter),
                    delta,
                    // `lat_sum` is the one signed counter of a target row.
                    signed: index == 0,
                });
            }
        }
        self.ops.push(Op::Add {
            key,
            field: PRE_SUM.to_owned(),
            delta: deltas.overhead.sum_ms,
            signed: true,
        });
        self.ops.push(Op::Add {
            key,
            field: PRE_N.to_owned(),
            delta: deltas.overhead.n as i64,
            signed: false,
        });
    }
}

/// The index of `key` in `keys`, appended the first time it is named.
fn slot_of(keys: &mut Vec<String>, key: String) -> usize {
    match keys.iter().position(|existing| *existing == key) {
        Some(index) => index,
        None => {
            keys.push(key);
            keys.len() - 1
        }
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
            Some(text) => text
                .parse::<u64>()
                .ok()
                .filter(|watermark| *watermark <= MAX_EXACT)
                .ok_or_else(|| {
                    LearnerError::Unavailable(format!(
                        "`{key}` holds `{text}` for session `{session}`, not a watermark"
                    ))
                }),
        }
    }
}

#[cfg(test)]
mod tests;
