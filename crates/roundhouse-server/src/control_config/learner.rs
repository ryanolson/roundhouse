// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a project says about the online routing learner, and what it resolves
//! to (draft section 12.1, milestone M8 of
//! `agent-docs/PLAN-online-routing-learner.md`).
//!
//! **Off unless a project says otherwise.** `mode` defaults to `off`, and an
//! `off` block resolves to no learner at all: the engine takes today's path,
//! reads no store and computes no draw.
//!
//! **Two ruled defaults, and only two.** `on_infeasible` defaults to
//! `serve_rules` (2026-09-28 ruling 7) and `exploration.rate` to 5% when the
//! block is present (ruling 8). Every other field is required once the mode is
//! `shadow` or `live`, and none has a default: the starting numbers of ruling
//! 13 are written into a deployment's file, never supplied by code, so a
//! number a project runs under is always one somebody wrote down.
//!
//! **Checked whatever the mode.** A present field is checked even on an `off`
//! block: a project that writes a broken floor and leaves the learner off has
//! still written a broken floor, and the day it flips `shadow` is the worst
//! moment to find out. The same argument the validate block makes for its
//! share table.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use roundhouse_core::routing::learn::{
    Artifact, ArtifactError, ExplorationTerms, LearnerMode, LearnerTerms, OnInfeasible,
    QualityTerms, Strategy, StrategySet,
};

use super::config::ControlPlaneError;

/// The exploration rate a present `exploration` block runs at when it names
/// none (2026-09-28 ruling 8).
pub const DEFAULT_EXPLORATION_RATE: f64 = 0.05;

/// One project's `"learner"` object.
///
/// `deny_unknown_fields` for the reason every project axis is: a misspelt
/// `on_infeasable` would silently mean the default, and a misspelt `mode`
/// field would silently mean `off`.
///
/// **Every field but the two with ruled defaults is an `Option`**, because an
/// `off` block may be partial, and the refusal of a missing field has to name
/// it rather than surface as a serde error about a struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearnerConfig {
    #[serde(default)]
    pub mode: LearnerMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategies: Option<Vec<Strategy>>,
    /// The calibration artifact's path. Its bytes decide the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<QualityConfig>,
    /// First output from turn start, in milliseconds (ruling 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_limit_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_min_samples: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_min_samples: Option<u64>,
    #[serde(default)]
    pub on_infeasible: OnInfeasible,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_timeout_ms: Option<u64>,
    /// Accepted on a `live` project only, so a block never sits inert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exploration: Option<ExplorationConfig>,
}

/// The gate's configuration. Every field is required: the gate has no number
/// it may assume.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityConfig {
    pub floor: f64,
    pub z: f64,
    pub min_evidence: u64,
    pub min_sessions: u64,
}

/// A `live` project's exploration block.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplorationConfig {
    #[serde(default = "default_rate")]
    pub rate: f64,
}

fn default_rate() -> f64 {
    DEFAULT_EXPLORATION_RATE
}

/// Why a `learner` block was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LearnerConfigError {
    /// A strategy is a tier pick, so a learner has nothing to choose between
    /// without a recipe (draft 4.2).
    #[error("a `learner` block needs the project's `tiers` recipe, and this project has none")]
    NoTiers,
    #[error("`{field}` is required when the mode is `{mode}`")]
    Missing {
        field: &'static str,
        mode: &'static str,
    },
    #[error("quality.floor {0} is outside 0.0..=1.0")]
    Floor(f64),
    #[error("quality.z {0} is not a positive number")]
    Z(f64),
    #[error("`{0}` is 0, which would time out every call before it is sent")]
    ZeroTimeout(&'static str),
    #[error("the strategy list is refused: {0}")]
    Strategies(String),
    /// Refused rather than left inert: exploration is live-only by ruling 8,
    /// and a block on a `shadow` project would read as a setting that does
    /// something.
    #[error("an `exploration` block is accepted only when the mode is `live`")]
    ExplorationNotLive,
    #[error("exploration.rate {0} is outside (0, 1]")]
    Rate(f64),
    #[error("the artifact `{path}` cannot be read: {reason}")]
    ArtifactUnreadable { path: String, reason: String },
    #[error("the artifact `{path}` is refused: {source}")]
    Artifact { path: String, source: ArtifactError },
    /// The epoch hashes the artifact's list, and the policy plans the
    /// configured one: two lists would count evidence under one and serve
    /// another.
    #[error("the artifact `{path}` lists {artifact:?}, and the block lists {configured:?}")]
    ArtifactStrategies {
        path: String,
        artifact: Vec<Strategy>,
        configured: Vec<Strategy>,
    },
}

impl LearnerConfig {
    /// The project's resolved learner, or `None` for an `off` block.
    ///
    /// `has_tiers` is whether the project configured a `tiers` recipe.
    pub(super) fn to_terms(
        &self,
        path: &str,
        entry: &str,
        has_tiers: bool,
    ) -> Result<Option<Arc<LearnerTerms>>, ControlPlaneError> {
        self.resolve(has_tiers)
            .map_err(|source| ControlPlaneError::LearnerRejected {
                path: path.to_string(),
                entry: entry.to_string(),
                source,
            })
    }

    fn resolve(&self, has_tiers: bool) -> Result<Option<Arc<LearnerTerms>>, LearnerConfigError> {
        if !has_tiers {
            return Err(LearnerConfigError::NoTiers);
        }
        // Every present field, whatever the mode. See the module doc.
        if let Some(quality) = &self.quality {
            if !(0.0..=1.0).contains(&quality.floor) {
                return Err(LearnerConfigError::Floor(quality.floor));
            }
            // `!(z > 0.0)` rather than `z <= 0.0`, so a NaN is refused too.
            if !(quality.z > 0.0 && quality.z.is_finite()) {
                return Err(LearnerConfigError::Z(quality.z));
            }
        }
        for (field, value) in [
            ("read_timeout_ms", self.read_timeout_ms),
            ("apply_timeout_ms", self.apply_timeout_ms),
        ] {
            if value == Some(0) {
                return Err(LearnerConfigError::ZeroTimeout(field));
            }
        }
        let strategies = self
            .strategies
            .clone()
            .map(StrategySet::new)
            .transpose()
            .map_err(|error| LearnerConfigError::Strategies(error.to_string()))?;
        if let Some(exploration) = &self.exploration {
            if self.mode != LearnerMode::Live {
                return Err(LearnerConfigError::ExplorationNotLive);
            }
            if !(exploration.rate > 0.0 && exploration.rate <= 1.0) {
                return Err(LearnerConfigError::Rate(exploration.rate));
            }
        }

        let Some(active) = self.mode.active() else {
            return Ok(None);
        };
        let mode = active.label();
        let required = |field: &'static str| LearnerConfigError::Missing { field, mode };
        let strategies = strategies.ok_or_else(|| required("strategies"))?;
        let path = self.artifact.as_ref().ok_or_else(|| required("artifact"))?;
        let quality = self.quality.ok_or_else(|| required("quality"))?;
        let latency_limit_ms = self
            .latency_limit_ms
            .ok_or_else(|| required("latency_limit_ms"))?;
        let latency_min_samples = self
            .latency_min_samples
            .ok_or_else(|| required("latency_min_samples"))?;
        let cache_min_samples = self
            .cache_min_samples
            .ok_or_else(|| required("cache_min_samples"))?;
        let read_timeout_ms = self
            .read_timeout_ms
            .ok_or_else(|| required("read_timeout_ms"))?;
        let apply_timeout_ms = self
            .apply_timeout_ms
            .ok_or_else(|| required("apply_timeout_ms"))?;

        let bytes =
            std::fs::read(path).map_err(|error| LearnerConfigError::ArtifactUnreadable {
                path: path.clone(),
                reason: error.to_string(),
            })?;
        let artifact = Artifact::parse(&bytes).map_err(|source| LearnerConfigError::Artifact {
            path: path.clone(),
            source,
        })?;
        if artifact.strategies() != &strategies {
            return Err(LearnerConfigError::ArtifactStrategies {
                path: path.clone(),
                artifact: artifact.strategies().as_slice().to_vec(),
                configured: strategies.as_slice().to_vec(),
            });
        }
        let (_, prior, epoch) = artifact.into_parts();
        Ok(Some(Arc::new(LearnerTerms {
            mode: self.mode,
            strategies,
            epoch,
            prior,
            quality: QualityTerms {
                floor: quality.floor,
                z: quality.z,
                min_evidence: quality.min_evidence,
                min_sessions: quality.min_sessions,
            },
            latency_limit_ms,
            latency_min_samples,
            cache_min_samples,
            on_infeasible: self.on_infeasible,
            exploration: self.exploration.map(|exploration| ExplorationTerms {
                rate: exploration.rate,
            }),
            read_timeout_ms,
            apply_timeout_ms,
        })))
    }
}

#[cfg(test)]
mod tests;
