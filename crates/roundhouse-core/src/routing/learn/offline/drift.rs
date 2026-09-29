// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The drift check: the counters a point-in-time copy of the learner store
//! holds, against the counters its own watermarks say it should hold (draft
//! 14.1).
//!
//! **Only against a copy.** For each session, the entries at or below the
//! copy's watermark are rebuilt from the log and summed the way the store sums
//! them, and every counter the manifest's decisions read is compared. A live
//! store moves while the logs are read, so the same comparison against it
//! would report the read's own lag as drift; without a copy the report says
//! `drift check not run`.
//!
//! **Reads only**: `watermark` and `read`, never `apply`.

use std::collections::{BTreeMap, BTreeSet};

use super::extract::Evidence;
use crate::control::ProjectId;
use crate::ids::SessionId;
use crate::learn_store::{LearnerError, LearnerStore, ReadRequest};
use crate::routing::Target;
use crate::routing::learn::{
    CacheReuse, EpochId, JevCounts, LatencySum, LearnedInput, LevelKey, Strategy, StrategySet,
};

/// Whether the check ran, and what it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftCheck {
    /// No point-in-time copy was given.
    NotRun,
    Ran(DriftResult),
}

/// What one comparison found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DriftResult {
    /// Sessions whose watermark the copy was asked for.
    pub sessions: u64,
    /// Counters compared.
    pub compared: u64,
    /// Each counter that differs, `name: copy holds x, rebuild gives y`, in
    /// key order.
    pub differences: Vec<String>,
    /// Sessions whose copy watermark is above the last sequence the manifest
    /// read: the copy is newer than the cutoff, and their entries past it
    /// cannot be rebuilt from this manifest.
    pub beyond_cutoff: Vec<SessionId>,
}

/// Counters rebuilt from the log, keyed the way the store keys them.
#[derive(Default)]
struct Rebuilt {
    quality: BTreeMap<(EpochId, LevelKey, Strategy), (u64, u64)>,
    sessions: BTreeMap<(EpochId, LevelKey, Strategy), BTreeSet<SessionId>>,
    jev: BTreeMap<(EpochId, LevelKey), JevCounts>,
    ops: BTreeMap<(EpochId, String), (LatencySum, u64, CacheReuse)>,
    overhead: BTreeMap<EpochId, LatencySum>,
}

/// Compare `copy` with the entries its watermarks cover.
pub async fn check(
    copy: &dyn LearnerStore,
    project: &ProjectId,
    evidence: &Evidence,
) -> Result<DriftResult, LearnerError> {
    let mut result = DriftResult::default();
    let mut rebuilt = Rebuilt::default();
    let mut reads: BTreeMap<EpochId, Vec<LearnedInput>> = BTreeMap::new();
    let mut targets: BTreeMap<EpochId, Vec<Target>> = BTreeMap::new();
    for session in &evidence.sessions {
        result.sessions += 1;
        let watermark = copy.watermark(project, &session.session).await?;
        if watermark > session.through_seq {
            result.beyond_cutoff.push(session.session.clone());
        }
        for entry in session
            .entries
            .iter()
            .filter(|entry| entry.seq <= watermark)
        {
            let Some(deltas) = &entry.deltas else {
                continue;
            };
            let epoch = deltas.epoch;
            for quality in &deltas.quality {
                let key = (epoch, quality.key, quality.strategy);
                let sum = rebuilt.quality.entry(key).or_default();
                sum.0 += quality.units.pos;
                sum.1 += quality.units.n;
                rebuilt
                    .sessions
                    .entry(key)
                    .or_default()
                    .insert(session.session.clone());
            }
            for jev in &deltas.jev {
                let sum = rebuilt.jev.entry((epoch, jev.key)).or_default();
                sum.capable += jev.counts.capable;
                sum.efficient += jev.counts.efficient;
            }
            for target in &deltas.targets {
                let sum = rebuilt
                    .ops
                    .entry((epoch, target.target.clone()))
                    .or_default();
                sum.0.sum_ms += target.latency.sum_ms;
                sum.0.n += target.latency.n;
                sum.1 += target.failover;
                sum.2.predicted_permille += target.cache.predicted_permille;
                sum.2.observed_permille += target.cache.observed_permille;
                sum.2.n += target.cache.n;
            }
            let overhead = rebuilt.overhead.entry(epoch).or_default();
            overhead.sum_ms += deltas.overhead.sum_ms;
            overhead.n += deltas.overhead.n;
        }
        for (epoch, input) in &session.reads {
            let inputs = reads.entry(*epoch).or_default();
            if !inputs.contains(input) {
                inputs.push(*input);
            }
        }
        for (epoch, target) in &session.targets {
            let known = targets.entry(*epoch).or_default();
            if !known.contains(target) {
                known.push(target.clone());
            }
        }
    }

    let every = StrategySet::new(vec![
        Strategy::Rules,
        Strategy::Efficient,
        Strategy::Capable,
    ])
    .expect("the full strategy list is valid");
    let mut compared_keys: BTreeSet<(EpochId, LevelKey)> = BTreeSet::new();
    let mut differences = BTreeMap::new();
    for (epoch, inputs) in &reads {
        let epoch_targets = targets.get(epoch).map(Vec::as_slice).unwrap_or_default();
        let mut first_read = true;
        for input in inputs {
            let request =
                ReadRequest::new(project.clone(), *epoch, input, &every, epoch_targets.iter());
            let view = copy.read(&request).await?;
            for level in &view.levels {
                if !compared_keys.insert((*epoch, level.key)) {
                    continue;
                }
                for counts in &level.strategies {
                    let key = (*epoch, level.key, counts.strategy);
                    let (pos, n) = rebuilt.quality.get(&key).copied().unwrap_or_default();
                    let sessions = rebuilt.sessions.get(&key).map_or(0, BTreeSet::len) as u64;
                    let name = format!("{epoch}:q:{}:{}", level.key, counts.strategy);
                    for (field, held, want) in [
                        ("pos", counts.pos_units, pos),
                        ("n", counts.n_units, n),
                        ("sessions", counts.sessions, sessions),
                    ] {
                        compare(&mut result, &mut differences, &name, field, held, want);
                    }
                }
                let jev = rebuilt
                    .jev
                    .get(&(*epoch, level.key))
                    .copied()
                    .unwrap_or_default();
                let name = format!("{epoch}:q:{}", level.key);
                compare(
                    &mut result,
                    &mut differences,
                    &name,
                    "jev_capable",
                    level.jev.capable,
                    jev.capable,
                );
                compare(
                    &mut result,
                    &mut differences,
                    &name,
                    "jev_efficient",
                    level.jev.efficient,
                    jev.efficient,
                );
            }
            if !first_read {
                continue;
            }
            first_read = false;
            for ops in &view.targets {
                let (latency, failover, cache) = rebuilt
                    .ops
                    .get(&(*epoch, ops.target.clone()))
                    .copied()
                    .unwrap_or_default();
                let name = format!("{epoch}:ops:{}", ops.target);
                for (field, held, want) in [
                    ("lat_sum", ops.latency.sum_ms, latency.sum_ms),
                    ("lat_n", ops.latency.n as i64, latency.n as i64),
                    ("failover", ops.failover as i64, failover as i64),
                    (
                        "cache_pred",
                        ops.cache.predicted_permille as i64,
                        cache.predicted_permille as i64,
                    ),
                    (
                        "cache_obs",
                        ops.cache.observed_permille as i64,
                        cache.observed_permille as i64,
                    ),
                    ("cache_n", ops.cache.n as i64, cache.n as i64),
                ] {
                    compare(&mut result, &mut differences, &name, field, held, want);
                }
            }
            let overhead = rebuilt.overhead.get(epoch).copied().unwrap_or_default();
            let name = format!("{epoch}:ops:turn");
            compare(
                &mut result,
                &mut differences,
                &name,
                "pre_sum",
                view.overhead.sum_ms,
                overhead.sum_ms,
            );
            compare(
                &mut result,
                &mut differences,
                &name,
                "pre_n",
                view.overhead.n,
                overhead.n,
            );
        }
    }
    result.differences = differences.into_values().collect();
    Ok(result)
}

fn compare<T: PartialEq + std::fmt::Display>(
    result: &mut DriftResult,
    differences: &mut BTreeMap<String, String>,
    name: &str,
    field: &str,
    held: T,
    want: T,
) {
    result.compared += 1;
    if held != want {
        let at = format!("{name}:{field}");
        differences.insert(
            at.clone(),
            format!("{at}: copy holds {held}, rebuild gives {want}"),
        );
    }
}
