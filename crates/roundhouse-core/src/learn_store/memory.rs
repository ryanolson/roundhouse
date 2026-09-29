// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The in-process [`LearnerStore`]: one lock over every project's counters.
//!
//! **The layout mirrors the Redis layout of draft section 11.4**, one map per
//! key family, so the Lua script of M7 is a translation of `stage` and
//! `Staged::commit` rather than a second design:
//!
//! | Redis key | Here |
//! |---|---|
//! | `{project}:wm` | `ProjectState::watermarks` |
//! | `{project}:{epoch}:q:{level}:{key}` | `ProjectState::quality` |
//! | `{project}:{epoch}:ops` | `ProjectState::ops` |
//! | `{project}:{epoch}:seen:{session}` | `ProjectState::seen` |
//!
//! [`key_name`] spells the quality and operations keys after `{project}:`, and
//! both the test-support visit recorder and the counter a
//! [`LearnerError::CounterRange`] names use it, so an operator reading a
//! refusal and a test reading the visits see the key M7 will store.
//!
//! **The check phase borrows the state immutably.** `stage` takes
//! `&ProjectState` and returns every new value; only `Staged::commit` takes
//! `&mut`. That the check writes nothing is then the borrow checker's claim,
//! not a convention, and a failure anywhere before the commit leaves the state
//! as it was. The test-support fault hook fails a call at exactly that point.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use super::{Applied, LearnerError, LearnerStore, LearningBatch, MAX_EXACT, ReadRequest};
use crate::control::ProjectId;
use crate::ids::SessionId;
use crate::routing::learn::{
    CacheReuse, EpochId, JevCounts, LatencySum, LevelKey, LevelView, ReadView, Strategy,
    StrategyCounts, TargetOps,
};
use crate::session::Deltas;

/// One project's stored counters, and the snapshot type of the contract's
/// [`LearnerStoreControl`](super::contract::LearnerStoreControl).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectState {
    watermarks: HashMap<SessionId, u64>,
    quality: HashMap<(EpochId, LevelKey), QualityHash>,
    ops: HashMap<EpochId, OpsHash>,
    /// The `(level key, strategy)` pairs each session has counted in `sessions`.
    seen: HashMap<(EpochId, SessionId), BTreeSet<(LevelKey, Strategy)>>,
}

/// One quality key: `{strategy}:pos`, `{strategy}:n`, `{strategy}:sessions`,
/// and the Jev answers on the key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct QualityHash {
    strategies: HashMap<Strategy, StrategyCell>,
    jev: JevCounts,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StrategyCell {
    pos: u64,
    n: u64,
    sessions: u64,
}

/// The operations key: per-target rows, and the project's overhead sum under
/// `turn:pre_sum` and `turn:pre_n` (plan section 4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OpsHash {
    targets: HashMap<String, TargetCell>,
    overhead: LatencySum,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TargetCell {
    latency: LatencySum,
    failover: u64,
    cache: CacheReuse,
}

/// See the module doc.
#[derive(Clone, Default)]
pub struct MemoryLearnerStore {
    state: Arc<RwLock<HashMap<ProjectId, ProjectState>>>,
    #[cfg(any(test, feature = "test-support"))]
    probe: Arc<Probe>,
}

/// Test-support instrumentation: the fault hook and the keys a read visited.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct Probe {
    fail_between_phases: std::sync::atomic::AtomicBool,
    /// `None` until a test calls
    /// [`record_visits`](MemoryLearnerStore::record_visits). A dependent that
    /// enables test support for other reasons reads on every turn, and an
    /// always-on list would grow with every read for the life of its store.
    visited: std::sync::Mutex<Option<Vec<String>>>,
}

impl MemoryLearnerStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a key a read visits, spelled by [`key_name`], when a test has
    /// armed the recorder. A no-op otherwise, and outside test support.
    fn visit(&self, _epoch: EpochId, _key: StoreKey) {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(visited) = self
            .probe
            .visited
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            visited.push(key_name(_epoch, _key));
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl MemoryLearnerStore {
    /// Fail the next `apply` that reaches its write phase, after the check
    /// phase passed and before anything is stored.
    ///
    /// An apply the check refuses, or one that skips every entry and so has
    /// nothing to write, leaves the fault armed for the next one.
    pub fn fail_next_apply_between_phases(&self) {
        self.probe
            .fail_between_phases
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Start recording the keys reads visit. Until this is called nothing is
    /// recorded.
    pub fn record_visits(&self) {
        *self
            .probe
            .visited
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Vec::new());
    }

    /// The keys reads visited since the recorder was armed or last taken, in
    /// visit order; empty when it was never armed.
    pub fn take_visited(&self) -> Vec<String> {
        self.probe
            .visited
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    fn fault_between_phases(&self) -> bool {
        self.probe
            .fail_between_phases
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl LearnerStore for MemoryLearnerStore {
    async fn read(&self, request: &ReadRequest) -> Result<ReadView, LearnerError> {
        let state = self.state.read().await;
        let project = state.get(request.project());
        let epoch = request.epoch();
        let levels = request
            .keys()
            .iter()
            .map(|key| {
                self.visit(epoch, StoreKey::Quality(*key));
                let hash = project.and_then(|project| project.quality.get(&(epoch, *key)));
                LevelView {
                    key: *key,
                    strategies: request
                        .strategies()
                        .iter()
                        .map(|&strategy| {
                            let cell = hash
                                .and_then(|hash| hash.strategies.get(&strategy))
                                .copied()
                                .unwrap_or_default();
                            StrategyCounts {
                                strategy,
                                pos_units: cell.pos,
                                n_units: cell.n,
                                sessions: cell.sessions,
                            }
                        })
                        .collect(),
                    jev: hash.map(|hash| hash.jev).unwrap_or_default(),
                }
            })
            .collect();
        self.visit(epoch, StoreKey::Ops);
        let ops = project.and_then(|project| project.ops.get(&epoch));
        let targets = request
            .targets()
            .iter()
            .map(|target| {
                let cell = ops
                    .and_then(|ops| ops.targets.get(target))
                    .cloned()
                    .unwrap_or_default();
                TargetOps {
                    target: target.clone(),
                    latency: cell.latency,
                    failover: cell.failover,
                    cache: cell.cache,
                }
            })
            .collect();
        Ok(ReadView {
            levels,
            targets,
            overhead: ops.map(|ops| ops.overhead).unwrap_or_default(),
        })
    }

    async fn apply(&self, batch: &LearningBatch<'_>) -> Result<Applied, LearnerError> {
        batch.check()?;
        let mut state = self.state.write().await;
        let staged = stage(state.get(batch.project), batch)?;
        let applied = Applied {
            applied: staged.applied,
            watermark: staged.watermark,
        };
        if staged.applied > 0 {
            #[cfg(any(test, feature = "test-support"))]
            if self.fault_between_phases() {
                return Err(LearnerError::Unavailable(
                    "injected fault between the check and the write".to_owned(),
                ));
            }
            staged.commit(
                state.entry(batch.project.clone()).or_default(),
                batch.session,
            );
        }
        Ok(applied)
    }

    async fn watermark(
        &self,
        project: &ProjectId,
        session: &SessionId,
    ) -> Result<u64, LearnerError> {
        let state = self.state.read().await;
        Ok(state
            .get(project)
            .and_then(|project| project.watermarks.get(session))
            .copied()
            .unwrap_or(0))
    }
}

/// A quality or operations key of one epoch.
#[derive(Debug, Clone, Copy)]
enum StoreKey {
    Quality(LevelKey),
    Ops,
}

/// The Redis spelling of a key after `{project}:`, as the module doc's table
/// has it: `{epoch}:q:{level}:{key}` or `{epoch}:ops`.
fn key_name(epoch: EpochId, key: StoreKey) -> String {
    match key {
        StoreKey::Quality(key) => format!("{epoch}:q:{}:{}", key.level().label(), key.part()),
        StoreKey::Ops => format!("{epoch}:ops"),
    }
}

/// Every value the write phase will store, computed from the current state
/// without changing it.
#[derive(Debug, Default)]
struct Staged {
    watermark: u64,
    applied: usize,
    /// Whole new values of every quality key the batch touches.
    quality: HashMap<(EpochId, LevelKey), QualityHash>,
    /// Whole new values of every operations key the batch touches.
    ops: HashMap<EpochId, OpsHash>,
    /// The `seen` members this batch adds for its session.
    seen: HashMap<EpochId, BTreeSet<(LevelKey, Strategy)>>,
}

/// The check phase of draft section 11.2, over one project's state.
///
/// Each entry is compared with the watermark as the entries before it moved
/// it: at or below, skipped by its identity; above, applied only if its
/// `prev_seq` is that watermark. A gap or a divergence refuses the whole batch
/// and reports the watermark the store holds.
fn stage(
    current: Option<&ProjectState>,
    batch: &LearningBatch<'_>,
) -> Result<Staged, LearnerError> {
    let empty = ProjectState::default();
    let current = current.unwrap_or(&empty);
    let store_watermark = current.watermarks.get(batch.session).copied().unwrap_or(0);
    let mut staged = Staged {
        watermark: store_watermark,
        ..Staged::default()
    };
    for entry in batch.entries {
        if entry.seq <= staged.watermark {
            continue;
        }
        // Above the watermark, the predecessor decides: equal applies, above
        // is an entry the store has not seen (a backfill supplies it), below
        // is a chain without the store's entry at the watermark (a backfill
        // would resend the same entry forever).
        if entry.prev_seq > staged.watermark {
            return Err(LearnerError::ChainGap { store_watermark });
        }
        if entry.prev_seq < staged.watermark {
            return Err(LearnerError::ChainDiverged { store_watermark });
        }
        if let Some(deltas) = &entry.deltas {
            staged.add(current, batch.session, deltas)?;
        }
        staged.watermark = entry.seq;
        staged.applied += 1;
    }
    Ok(staged)
}

impl Staged {
    fn add(
        &mut self,
        current: &ProjectState,
        session: &SessionId,
        deltas: &Deltas,
    ) -> Result<(), LearnerError> {
        let epoch = deltas.epoch;
        let seen = current.seen.get(&(epoch, session.clone()));
        for quality in &deltas.quality {
            let member = (quality.key, quality.strategy);
            let new_session = !seen.is_some_and(|seen| seen.contains(&member))
                && self.seen.entry(epoch).or_default().insert(member);
            let cell = self
                .quality_hash(current, epoch, quality.key)
                .strategies
                .entry(quality.strategy)
                .or_default();
            let name = |field: &str| {
                format!(
                    "{}:{}:{field}",
                    key_name(epoch, StoreKey::Quality(quality.key)),
                    quality.strategy
                )
            };
            cell.pos = add(cell.pos, quality.units.pos, || name("pos"))?;
            cell.n = add(cell.n, quality.units.n, || name("n"))?;
            if new_session {
                cell.sessions = add(cell.sessions, 1, || name("sessions"))?;
            }
        }
        for jev in &deltas.jev {
            let hash = self.quality_hash(current, epoch, jev.key);
            let name = |field: &str| {
                format!(
                    "{}:jev_{field}",
                    key_name(epoch, StoreKey::Quality(jev.key))
                )
            };
            hash.jev.capable = add(hash.jev.capable, jev.counts.capable, || name("capable"))?;
            hash.jev.efficient = add(hash.jev.efficient, jev.counts.efficient, || {
                name("efficient")
            })?;
        }
        if deltas.targets.is_empty() && deltas.overhead.n == 0 && deltas.overhead.sum_ms == 0 {
            return Ok(());
        }
        let ops = self
            .ops
            .entry(epoch)
            .or_insert_with(|| current.ops.get(&epoch).cloned().unwrap_or_default());
        for target in &deltas.targets {
            let cell = ops.targets.entry(target.target.clone()).or_default();
            let name = |field: &str| {
                format!(
                    "{}:{}:{field}",
                    key_name(epoch, StoreKey::Ops),
                    target.target
                )
            };
            cell.latency.sum_ms = add_signed(cell.latency.sum_ms, target.latency.sum_ms, || {
                name("lat_sum")
            })?;
            cell.latency.n = add(cell.latency.n, target.latency.n, || name("lat_n"))?;
            cell.failover = add(cell.failover, target.failover, || name("failover"))?;
            cell.cache.predicted_permille = add(
                cell.cache.predicted_permille,
                target.cache.predicted_permille,
                || name("cache_pred"),
            )?;
            cell.cache.observed_permille = add(
                cell.cache.observed_permille,
                target.cache.observed_permille,
                || name("cache_obs"),
            )?;
            cell.cache.n = add(cell.cache.n, target.cache.n, || name("cache_n"))?;
        }
        ops.overhead.sum_ms = add_signed(ops.overhead.sum_ms, deltas.overhead.sum_ms, || {
            format!("{}:turn:pre_sum", key_name(epoch, StoreKey::Ops))
        })?;
        ops.overhead.n = add(ops.overhead.n, deltas.overhead.n, || {
            format!("{}:turn:pre_n", key_name(epoch, StoreKey::Ops))
        })?;
        Ok(())
    }

    /// The staged whole value of one quality key, seeded from the stored one
    /// the first time the batch touches it.
    fn quality_hash(
        &mut self,
        current: &ProjectState,
        epoch: EpochId,
        key: LevelKey,
    ) -> &mut QualityHash {
        self.quality.entry((epoch, key)).or_insert_with(|| {
            current
                .quality
                .get(&(epoch, key))
                .cloned()
                .unwrap_or_default()
        })
    }

    /// The write phase: store every staged value, the `seen` members and the
    /// watermark. Nothing here can fail.
    fn commit(self, project: &mut ProjectState, session: &SessionId) {
        project.quality.extend(self.quality);
        project.ops.extend(self.ops);
        for (epoch, members) in self.seen {
            project
                .seen
                .entry((epoch, session.clone()))
                .or_default()
                .extend(members);
        }
        project.watermarks.insert(session.clone(), self.watermark);
    }
}

/// A nonnegative counter plus a delta, within `0..=2^53 - 1`.
///
/// Checked after every addition, not only on the final value: a Lua number
/// past `2^53` is no longer exact, so the Redis script must refuse the step
/// that crosses, and this backend refuses the same batches.
fn add(value: u64, delta: u64, counter: impl FnOnce() -> String) -> Result<u64, LearnerError> {
    value
        .checked_add(delta)
        .filter(|sum| *sum <= MAX_EXACT)
        .ok_or_else(|| LearnerError::CounterRange { counter: counter() })
}

/// A signed sum plus a delta, within `±(2^53 - 1)`, checked on every step for
/// the reason [`add`] gives.
fn add_signed(
    value: i64,
    delta: i64,
    counter: impl FnOnce() -> String,
) -> Result<i64, LearnerError> {
    value
        .checked_add(delta)
        .filter(|sum| sum.unsigned_abs() <= MAX_EXACT)
        .ok_or_else(|| LearnerError::CounterRange { counter: counter() })
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl super::contract::LearnerStoreControl for MemoryLearnerStore {
    /// `None` for a project the map has no entry for, so a call that created
    /// an empty entry does not compare equal to one that created nothing.
    type Snapshot = Option<ProjectState>;

    async fn snapshot(&self, project: &ProjectId) -> Option<ProjectState> {
        self.state.read().await.get(project).cloned()
    }

    async fn restore(&self, project: &ProjectId, snapshot: Option<ProjectState>) {
        let mut state = self.state.write().await;
        match snapshot {
            Some(snapshot) => state.insert(project.clone(), snapshot),
            None => state.remove(project),
        };
    }
}

#[cfg(test)]
mod tests;
