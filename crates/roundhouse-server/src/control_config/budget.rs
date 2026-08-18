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

/// Whether `value` fails the "must be a positive number of dollars" rule that
/// every ceiling in this file is judged by.
///
/// **`NaN` fails it**, and that is the whole reason this is a named function
/// rather than `value <= 0.0` written twice. `NaN <= 0.0` is *false*, so the
/// direct spelling waves a `NaN` limit through to a ledger where it then loses
/// every comparison too and grants zero forever — a budget that refuses every
/// turn, configured by an operator who wrote a number. Spelled through
/// [`f64::partial_cmp`] because that is the only comparison that admits the
/// third answer: strictly greater, or anything else.
fn not_positive(value: f64) -> bool {
    !matches!(value.partial_cmp(&0.0), Some(std::cmp::Ordering::Greater))
}

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
        // See `not_positive`: this is not `<= 0.0`, and the difference is a
        // `NaN` limit reaching a ledger that would then grant zero forever.
        if not_positive(self.limit_usd) {
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
///
/// `deny_unknown_fields` for the reason [`BudgetConfig`] carries it, which is
/// not the obvious one: a *missing* required field already fails without it.
/// What it closes is an operator writing the ceiling they meant *beside* the
/// field name serde reads — `{ "limit_usd": 5.0, "limit_used": 9.0 }` resolves
/// silently to a $5 cap, and nothing in the file or the log ever says which of
/// the two numbers won.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
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
            AllocationConfig::Capped { limit_usd } => {
                // The same rule `BudgetConfig::to_budget` applies to a project
                // limit, through the same predicate and for the same reasons.
                // Both ceilings bind identically at runtime — the ledger takes
                // the minimum of whatever it is given — so a cap of zero
                // refuses every turn this key sends, which is a revoked key
                // written as a budget rather than a budget.
                if not_positive(*limit_usd) {
                    return Err(ControlPlaneError::MemberCapNotPositive {
                        path: path.to_string(),
                        entry: entry.to_string(),
                        limit_usd: *limit_usd,
                    });
                }
                Ok(Allocation::Capped {
                    limit_usd: *limit_usd,
                })
            }
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

    #[test]
    fn a_zero_member_cap_is_refused_the_way_a_zero_project_limit_is() {
        // The two ceilings bind identically at runtime — the ledger takes the
        // minimum of whatever it is given — so a cap of zero refuses every
        // turn a key ever sends, exactly as a project limit of zero refuses
        // every turn a project ever sends. One of those was refused at the
        // boundary and the other was not, and the asymmetry had no reason: a
        // key that may spend nothing is a revoked key spelled as a budget, and
        // the operator who wrote it will be reading a `min_quality` error
        // somewhere else wondering why their turns degrade.
        let error = AllocationConfig::Capped { limit_usd: 0.0 }
            .to_allocation("path", "key `ada@acme`")
            .expect_err("a member cap of zero must be refused at the boundary");
        match error {
            ControlPlaneError::MemberCapNotPositive {
                entry, limit_usd, ..
            } => {
                assert_eq!(
                    entry, "key `ada@acme`",
                    "the message names the entry an operator has to go and edit"
                );
                assert_eq!(limit_usd, 0.0);
            }
            other => panic!("expected MemberCapNotPositive, got {other:?}"),
        }
    }

    #[test]
    fn a_negative_member_cap_is_refused() {
        assert!(matches!(
            AllocationConfig::Capped { limit_usd: -1.0 }.to_allocation("path", "e"),
            Err(ControlPlaneError::MemberCapNotPositive { .. })
        ));
    }

    #[test]
    fn a_positive_member_cap_validates() {
        // The control, without which the two refusals above would be equally
        // satisfied by a boundary that refused every cap.
        assert_eq!(
            AllocationConfig::Capped { limit_usd: 5.0 }
                .to_allocation("path", "e")
                .unwrap(),
            Allocation::Capped { limit_usd: 5.0 }
        );
    }

    #[test]
    fn a_ceiling_that_is_not_a_number_is_refused_rather_than_passing_every_comparison() {
        // **`NaN <= 0.0` is false**, so the obvious spelling of "refuse a
        // non-positive limit" lets a `NaN` through — and a `NaN` limit reaches
        // the ledger, where it loses every comparison there too and grants
        // zero for the life of the deployment. Both ceilings are therefore
        // written as "must be greater than zero" negated, which a `NaN` fails.
        assert!(matches!(
            config(f64::NAN, OnExhaustionConfig::DegradeToLocal, None, None).to_budget("p", "e"),
            Err(ControlPlaneError::BudgetLimitNotPositive { .. })
        ));
        assert!(matches!(
            AllocationConfig::Capped {
                limit_usd: f64::NAN
            }
            .to_allocation("p", "e"),
            Err(ControlPlaneError::MemberCapNotPositive { .. })
        ));
        // And the controls, which are what stop the two assertions above from
        // being about a boundary that refuses everything.
        assert!(
            config(1.0, OnExhaustionConfig::DegradeToLocal, None, None)
                .to_budget("p", "e")
                .is_ok()
        );
        assert!(
            AllocationConfig::Capped { limit_usd: 1.0 }
                .to_allocation("p", "e")
                .is_ok()
        );
    }

    #[test]
    fn a_misspelled_allocation_field_is_refused_rather_than_silently_ignored() {
        // The failure mode `deny_unknown_fields` closes, and it is not the
        // obvious one: a *missing* required field already fails, because serde
        // says so. What passed silently was an operator writing the cap they
        // meant *beside* the field name serde reads — the allocation resolves
        // to the wrong ceiling, and nothing in the file or the log says which
        // of the two numbers won.
        let error = serde_json::from_str::<AllocationConfig>(
            r#"{"capped": {"limit_usd": 5.0, "limit_used": 9.0}}"#,
        )
        .expect_err("a field name nothing reads must be refused, not dropped");
        assert!(
            error.to_string().contains("limit_used"),
            "the message has to name the field the operator misspelled: {error}"
        );

        // The same for a share, where the stray field is a plausible one: an
        // operator writing both a fraction and a dollar cap has asked for two
        // different allocations and must be told, not silently given one.
        assert!(
            serde_json::from_str::<AllocationConfig>(
                r#"{"share": {"fraction": 0.5, "limit_usd": 5.0}}"#
            )
            .is_err()
        );

        // The controls: both well-spelled shapes still parse.
        assert!(matches!(
            serde_json::from_str::<AllocationConfig>(r#"{"capped": {"limit_usd": 5.0}}"#).unwrap(),
            AllocationConfig::Capped { limit_usd } if limit_usd == 5.0
        ));
        assert!(matches!(
            serde_json::from_str::<AllocationConfig>(r#""pooled""#).unwrap(),
            AllocationConfig::Pooled
        ));
    }
}
