// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The shapes an operator writes for a project's `"budget"` and a key's
//! `"allocation"`, and the boundary that turns them into the real
//! [`Budget`]/[`Allocation`] the ledger is checked against.
//!
//! Split out of [`config`](super::config) for the reason its module doc gives
//! for keeping large validated shapes apart from the rest: this file grew
//! past the point a single validators-plus-format module stays readable, and
//! the split line is the same one the crate already draws elsewhere -- one
//! file per config object, judged by one boundary. [`ControlPlaneError`]
//! itself stays in `config`, because it is the *one* error enum every
//! validator in this crate reports through; a second error type here would
//! be a second table an operator has to learn to read.
//!
//! **`"on_exhaustion"` and `"overflow_when_local_saturated"` are read
//! together on purpose.** The flag is only meaningful inside
//! [`Exhaustion::DegradeToLocal`], so a config that sets it under
//! `"refuse"` is not a value that silently does nothing -- it is refused at
//! load, naming the project, the same choice [`super::config`] makes for a
//! malformed glob or a widening override.

use serde::Deserialize;

use roundhouse_core::control::{Allocation, Budget, BudgetWindow, DEFAULT_WARN_AT, Exhaustion};

use super::config::ControlPlaneError;

/// The `"on_exhaustion"` tag, read as raw config data.
///
/// A config-only type rather than deserializing straight into
/// [`Exhaustion`]: the real enum carries the overflow flag *inside* its
/// `DegradeToLocal` arm, but the flag is spelled as a sibling field in the
/// file (`{ "on_exhaustion": "degrade_to_local", "overflow_when_local_saturated": true }`),
/// not nested under the tag. [`BudgetConfig::to_budget`] is what reunites
/// the two into the real type, once it has seen both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnExhaustionConfig {
    DegradeToLocal,
    Refuse,
}

/// One project's `"budget"` object.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    pub limit_usd: f64,
    pub window: BudgetWindow,
    pub on_exhaustion: OnExhaustionConfig,
    /// Meaningful only under `on_exhaustion: "degrade_to_local"` -- see the
    /// module doc. Absent means [`Exhaustion::degrade_with_overflow`]'s
    /// default of `true`.
    #[serde(default)]
    pub overflow_when_local_saturated: Option<bool>,
    /// Absent means [`DEFAULT_WARN_AT`].
    #[serde(default)]
    pub warn_at: Option<f64>,
}

impl BudgetConfig {
    /// Judge and resolve into the real [`Budget`] the ledger is checked
    /// against, naming `entry` (a project label) on every rejection.
    pub(super) fn to_budget(&self, path: &str, entry: &str) -> Result<Budget, ControlPlaneError> {
        if self.limit_usd <= 0.0 {
            return Err(ControlPlaneError::BudgetLimitNotPositive {
                path: path.to_string(),
                entry: entry.to_string(),
                limit_usd: self.limit_usd,
            });
        }
        let warn_at = match self.warn_at {
            Some(warn_at) => {
                if !(warn_at > 0.0 && warn_at <= 1.0) {
                    return Err(ControlPlaneError::WarnAtOutOfRange {
                        path: path.to_string(),
                        entry: entry.to_string(),
                        warn_at,
                    });
                }
                warn_at
            }
            None => DEFAULT_WARN_AT,
        };
        let on_exhaustion = match (self.on_exhaustion, self.overflow_when_local_saturated) {
            (OnExhaustionConfig::Refuse, Some(_)) => {
                return Err(ControlPlaneError::OverflowWithRefuse {
                    path: path.to_string(),
                    entry: entry.to_string(),
                });
            }
            (OnExhaustionConfig::Refuse, None) => Exhaustion::Refuse,
            (OnExhaustionConfig::DegradeToLocal, overflow) => Exhaustion::DegradeToLocal {
                overflow_when_local_saturated: overflow.unwrap_or(true),
            },
        };
        Ok(Budget {
            limit_usd: self.limit_usd,
            window: self.window,
            on_exhaustion,
            warn_at,
        })
    }
}

/// One key's `"allocation"` object: a member ceiling on top of its project's
/// budget.
///
/// The default externally-tagged `serde` representation, unannotated on
/// purpose -- it is what makes the wire shape decision 8 asks for
/// (`{ "capped": { "limit_usd": .. } }`, `{ "share": { "fraction": .. } }`)
/// fall out with no custom (de)serialization to keep in sync by hand. The
/// unit arm follows the same convention: `"pooled"` as a bare string, the
/// ordinary reading of a variant with nothing to carry.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationConfig {
    /// No member ceiling -- this key may spend the whole project budget.
    Pooled,
    Capped {
        limit_usd: f64,
    },
    Share {
        fraction: f64,
    },
}

impl AllocationConfig {
    /// Judge and resolve into the real [`Allocation`], naming `entry` (a key
    /// label) on every rejection.
    pub(super) fn to_allocation(
        &self,
        path: &str,
        entry: &str,
    ) -> Result<Allocation, ControlPlaneError> {
        match self {
            AllocationConfig::Pooled => Ok(Allocation::Pooled),
            AllocationConfig::Capped { limit_usd } => Ok(Allocation::Capped {
                limit_usd: *limit_usd,
            }),
            AllocationConfig::Share { fraction } => {
                if !(*fraction > 0.0 && *fraction <= 1.0) {
                    return Err(ControlPlaneError::ShareFractionOutOfRange {
                        path: path.to_string(),
                        entry: entry.to_string(),
                        fraction: *fraction,
                    });
                }
                Ok(Allocation::Share {
                    fraction: *fraction,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit coverage of the two pure resolvers. The JSON-shape and
    //! error-naming tests live in [`config`](super::config), where a whole
    //! [`ControlPlaneConfig`](super::config::ControlPlaneConfig) is parsed --
    //! that is the level at which "naming the entry" is actually observable.

    use super::*;

    fn config(
        limit_usd: f64,
        on_exhaustion: OnExhaustionConfig,
        overflow: Option<bool>,
        warn_at: Option<f64>,
    ) -> BudgetConfig {
        BudgetConfig {
            limit_usd,
            window: BudgetWindow::Total,
            on_exhaustion,
            overflow_when_local_saturated: overflow,
            warn_at,
        }
    }

    #[test]
    fn an_absent_warn_at_resolves_to_the_shared_default() {
        let budget = config(10.0, OnExhaustionConfig::DegradeToLocal, None, None)
            .to_budget("p", "e")
            .unwrap();
        assert_eq!(budget.warn_at, DEFAULT_WARN_AT);
    }

    #[test]
    fn an_absent_overflow_flag_resolves_to_the_valve_armed() {
        let budget = config(10.0, OnExhaustionConfig::DegradeToLocal, None, None)
            .to_budget("p", "e")
            .unwrap();
        assert_eq!(budget.on_exhaustion, Exhaustion::degrade_with_overflow());
    }

    #[test]
    fn an_explicit_overflow_false_resolves_to_the_valve_disarmed() {
        let budget = config(10.0, OnExhaustionConfig::DegradeToLocal, Some(false), None)
            .to_budget("p", "e")
            .unwrap();
        assert_eq!(
            budget.on_exhaustion,
            Exhaustion::DegradeToLocal {
                overflow_when_local_saturated: false
            }
        );
    }

    #[test]
    fn a_pooled_allocation_resolves_with_no_member_ceiling() {
        assert_eq!(
            AllocationConfig::Pooled.to_allocation("p", "e").unwrap(),
            Allocation::Pooled
        );
    }
}
