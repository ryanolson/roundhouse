// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The deployment's `learner_recovery` block: how often the recovery task
//! sweeps, how much one sweep does, and how long each of its calls may take
//! (draft section 12.1, milestone M9 of
//! `agent-docs/PLAN-online-routing-learner.md`).
//!
//! **Required once any project enables the learner, and never defaulted.**
//! The task is what delivers a session that went idle with entries owed, so a
//! learner with no cadence would be a learner whose idle sessions are never
//! delivered. The check runs in `ControlPlaneConfig::validate`, which judges
//! the file at boot and the file merged with the admin records on every admin
//! write, so an admin write that turns a learner on under a file with no block
//! is refused the same way the boot is. A block with no learner enabled is
//! accepted: it is how a deployment prepares for a learner the admin plane
//! adds later.
//!
//! **No field may be zero.** A zero timeout times out every call before it is
//! sent, a zero interval sweeps in a busy loop, a zero page examines nothing,
//! and a zero idle window makes the task contend with every live turn's own
//! delivery. The same rule every other timeout in `control_config` follows.

use std::num::NonZeroUsize;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::learner_recovery::RecoveryCadence;

/// The `"learner_recovery"` object, as written.
///
/// Every field is required: the section 4 starting values are written into a
/// deployment's file, never supplied by code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearnerRecoveryConfig {
    /// The wait between two sweeps when the stores answer.
    pub sweep_interval_ms: u64,
    /// How long a session's newest mark must have stood before a sweep
    /// delivers it, by the session store's clock.
    pub idle_after_ms: u64,
    /// Pending sessions one sweep examines.
    pub max_sessions_per_sweep: u64,
    /// Pages of entries one sweep applies for one session.
    pub pages_per_session_per_sweep: u64,
    /// Marked sessions one sweep audits.
    pub audit_sessions_per_sweep: u64,
    /// One learner-store watermark read.
    pub read_timeout_ms: u64,
    /// One learner-store apply.
    pub apply_timeout_ms: u64,
    /// One session-store call: an index page, a replay, a clear or a requeue.
    pub source_timeout_ms: u64,
}

/// Why a deployment's learner recovery configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LearnerRecoveryError {
    #[error(
        "project `{project}` enables the learner, and the file has no `learner_recovery` block \
         -- without one, a session that goes idle with entries owed is never delivered"
    )]
    Missing { project: String },
    #[error("`learner_recovery.{0}` is 0; every field of the block must be at least 1")]
    Zero(&'static str),
}

impl LearnerRecoveryConfig {
    /// The resolved cadence, or the first zero field.
    pub(super) fn to_cadence(self) -> Result<RecoveryCadence, LearnerRecoveryError> {
        let fields = [
            ("sweep_interval_ms", self.sweep_interval_ms),
            ("idle_after_ms", self.idle_after_ms),
            ("max_sessions_per_sweep", self.max_sessions_per_sweep),
            (
                "pages_per_session_per_sweep",
                self.pages_per_session_per_sweep,
            ),
            ("audit_sessions_per_sweep", self.audit_sessions_per_sweep),
            ("read_timeout_ms", self.read_timeout_ms),
            ("apply_timeout_ms", self.apply_timeout_ms),
            ("source_timeout_ms", self.source_timeout_ms),
        ];
        if let Some((field, _)) = fields.iter().find(|(_, value)| *value == 0) {
            return Err(LearnerRecoveryError::Zero(field));
        }
        // Checked non-zero above; a count past `usize` saturates, which on
        // every supported target is far beyond anything a page could hold.
        let count = |value: u64| {
            NonZeroUsize::new(usize::try_from(value).unwrap_or(usize::MAX))
                .unwrap_or(NonZeroUsize::MIN)
        };
        Ok(RecoveryCadence {
            sweep_interval: Duration::from_millis(self.sweep_interval_ms),
            idle_after_ms: self.idle_after_ms,
            max_sessions_per_sweep: count(self.max_sessions_per_sweep),
            pages_per_session_per_sweep: count(self.pages_per_session_per_sweep),
            audit_sessions_per_sweep: count(self.audit_sessions_per_sweep),
            read_timeout: Duration::from_millis(self.read_timeout_ms),
            apply_timeout: Duration::from_millis(self.apply_timeout_ms),
            source_timeout: Duration::from_millis(self.source_timeout_ms),
        })
    }
}

/// The deployment's resolved cadence: `None` when the file writes no block,
/// refused when a project enables the learner without one.
///
/// `enabling` is the first project, in file order, whose learner resolved to
/// `shadow` or `live`.
pub(super) fn resolve(
    block: Option<LearnerRecoveryConfig>,
    enabling: Option<&str>,
) -> Result<Option<RecoveryCadence>, LearnerRecoveryError> {
    match (block, enabling) {
        (Some(block), _) => block.to_cadence().map(Some),
        (None, Some(project)) => Err(LearnerRecoveryError::Missing {
            project: project.to_string(),
        }),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
pub(super) mod tests;
