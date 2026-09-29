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
    visited: std::sync::Mutex<Vec<String>>,
}

impl MemoryLearnerStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a key a read visits, spelled as the Redis key parts after the
    /// project. A no-op outside test support.
    fn visit(&self, _key: impl FnOnce() -> String) {
        #[cfg(any(test, feature = "test-support"))]
        self.probe
            .visited
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(_key());
    }
}

#[cfg(any(test, feature = "test-support"))]
impl MemoryLearnerStore {
    /// Fail the next `apply` after its check phase and before its write phase.
    pub fn fail_next_apply_between_phases(&self) {
        self.probe
            .fail_between_phases
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The keys reads visited since the last call, in visit order.
    pub fn take_visited(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .probe
                .visited
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
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
                self.visit(|| format!("q:{epoch}:{}:{}", key.level().label(), key.part()));
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
        self.visit(|| format!("ops:{epoch}"));
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

    async fn apply(&self, batch: &LearningBatch) -> Result<Applied, LearnerError> {
        batch.check()?;
        let mut state = self.state.write().await;
        let staged = stage(state.get(&batch.project), batch)?;
        #[cfg(any(test, feature = "test-support"))]
        if self.fault_between_phases() {
            return Err(LearnerError::Unavailable(
                "injected fault between the check and the write".to_owned(),
            ));
        }
        let applied = Applied {
            applied: staged.applied,
            watermark: staged.watermark,
        };
        if staged.applied > 0 {
            staged.commit(
                state.entry(batch.project.clone()).or_default(),
                &batch.session,
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
/// `prev_seq` is that watermark. A gap refuses the whole batch and reports the
/// watermark the store holds.
fn stage(current: Option<&ProjectState>, batch: &LearningBatch) -> Result<Staged, LearnerError> {
    let empty = ProjectState::default();
    let current = current.unwrap_or(&empty);
    let store_watermark = current.watermarks.get(&batch.session).copied().unwrap_or(0);
    let mut staged = Staged {
        watermark: store_watermark,
        ..Staged::default()
    };
    for entry in &batch.entries {
        if entry.seq <= staged.watermark {
            continue;
        }
        if entry.prev_seq != staged.watermark {
            return Err(LearnerError::ChainGap { store_watermark });
        }
        if let Some(deltas) = &entry.deltas {
            staged.add(current, &batch.session, deltas)?;
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
            let hash = self.quality.entry((epoch, quality.key)).or_insert_with(|| {
                current
                    .quality
                    .get(&(epoch, quality.key))
                    .cloned()
                    .unwrap_or_default()
            });
            let cell = hash.strategies.entry(quality.strategy).or_default();
            let name = |field: &str| {
                format!(
                    "{epoch}:q:{}:{}:{}",
                    quality.key.level().label(),
                    quality.key.part(),
                    format_args!("{}:{field}", quality.strategy)
                )
            };
            cell.pos = add(cell.pos, quality.units.pos, || name("pos"))?;
            cell.n = add(cell.n, quality.units.n, || name("n"))?;
            if new_session {
                cell.sessions = add(cell.sessions, 1, || name("sessions"))?;
            }
        }
        for jev in &deltas.jev {
            let hash = self.quality.entry((epoch, jev.key)).or_insert_with(|| {
                current
                    .quality
                    .get(&(epoch, jev.key))
                    .cloned()
                    .unwrap_or_default()
            });
            let name = |field: &str| {
                format!(
                    "{epoch}:q:{}:{}:jev_{field}",
                    jev.key.level().label(),
                    jev.key.part()
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
            let name = |field: &str| format!("{epoch}:ops:{}:{field}", target.target);
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
            format!("{epoch}:ops:turn:pre_sum")
        })?;
        ops.overhead.n = add(ops.overhead.n, deltas.overhead.n, || {
            format!("{epoch}:ops:turn:pre_n")
        })?;
        Ok(())
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
    type Snapshot = ProjectState;

    async fn snapshot(&self, project: &ProjectId) -> ProjectState {
        self.state
            .read()
            .await
            .get(project)
            .cloned()
            .unwrap_or_default()
    }

    async fn restore(&self, project: &ProjectId, snapshot: ProjectState) {
        self.state.write().await.insert(project.clone(), snapshot);
    }
}

#[cfg(test)]
mod tests;
