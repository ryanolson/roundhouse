// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a project says about the validate/steer loop, and what it resolves to.
//!
//! **Off unless a project says otherwise.** The Intervention Paradox says an
//! excellent critic can collapse one agent and leave another untouched under
//! the identical policy, and the property that decides is the agent's own
//! disruption–recovery ratio — measurable only per deployment. So a deployment
//! that upgrades gets nothing: no arm stamp, no trigger, no judge, and the
//! `Interjector` it already had.
//!
//! **A project turning it on still gets Shadow by default.** `"validate": {
//! "enabled": true }` and nothing else is a judge that costs money and changes
//! nothing, which is the only configuration whose risk is bounded before
//! anybody has measured their own agent. `SteerChannel::Off` — also the default
//! — does not turn the loop off either: Shadow still runs, because measuring is
//! what turns the loop on later.
//!
//! **Per project and not per key**, unlike `overrides` and `allocation`. An arm
//! is the unit of a comparison, and two keys of one project running different
//! arm splits would put two experiments inside one project's numbers. A
//! deployment that wants a per-team split writes two projects, which is what a
//! separate comparison *is*.
//!
//! The refusals below are the boundary's usual posture, and the reason is the
//! usual one: every value here is read at a point where there is nothing to
//! fail into. A share table that sums to zero has no honest answer and the
//! tempting fallback — everything in one arm — is the failure the type exists
//! to prevent; a placebo rate outside `0.0..=1.0` is a control that is not one.

use serde::Deserialize;

use roundhouse_core::validate::{
    ActionPolicy, ArmShares, DEFAULT_PLACEBO_RATE, SteerChannel, ValidationTerms,
};

use super::config::ControlPlaneError;

/// One project's `"validate"` object.
///
/// Every field has a default, and every default is the shipped-off posture:
/// disabled, observing, never acting. The struct is `deny_unknown_fields` for
/// the reason the policy config is — a mistyped knob that silently did nothing
/// would be a deployment convinced it had configured an experiment.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ValidateConfig {
    /// Whether this project's sessions are enrolled at all.
    ///
    /// `false` resolves the whole object to `None`: no arm is stamped into
    /// `SessionCreated`, and an unstamped session is *not enrolled* rather than
    /// silently assigned — which is what stops the arm comparison being
    /// computed against a control group that was never eligible.
    pub enabled: bool,
    pub channel: SteerChannel,
    pub arms: ArmSharesConfig,
    /// The fraction of fired triggers the placebo arm intervenes on.
    ///
    /// Calibration, not measurement: matching the sham's rate to the live
    /// arm's observed rate is something a dashboard does across many sessions.
    pub placebo_rate: f64,
    /// The quality floor an escalation asks for, as a narrowing.
    pub escalation_floor: f64,
    /// How many subsequent turns that floor applies for.
    pub escalation_turns: u32,
    /// How many consecutive intervening validations a `Steer` may follow.
    ///
    /// `0`, the default, means the synthetic-call path is off: escalation
    /// claims the uninterrupted turn, so a cap of zero admits nothing after it.
    /// Set `1` or more to turn outcome B on. See
    /// [`ActionPolicy::steer_after_interventions`].
    pub steer_after_interventions: u32,
}

impl Default for ValidateConfig {
    fn default() -> Self {
        let action = ActionPolicy::default();
        Self {
            enabled: false,
            channel: action.channel,
            arms: ArmSharesConfig::default(),
            placebo_rate: DEFAULT_PLACEBO_RATE,
            // Read off `ActionPolicy::default` rather than restated, so the
            // config's silence and the library's default are one number. Two
            // copies would disagree the first time either moved, and the
            // disagreement would be invisible: both are plausible floors.
            escalation_floor: action.escalation_floor,
            escalation_turns: action.escalation_turns,
            steer_after_interventions: action.steer_after_interventions,
        }
    }
}

/// The `"arms"` object: three weights.
///
/// Weights rather than percentages so "one session in fifty is a placebo" is
/// expressible without anybody writing `0.02` and wondering about rounding.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArmSharesConfig {
    pub live: u32,
    pub shadow: u32,
    pub placebo: u32,
}

impl Default for ArmSharesConfig {
    /// Everything observes, nothing acts — the same table
    /// [`ArmShares::shadow_only`] names, spelled here so a config file's
    /// silence and the library's default cannot drift.
    fn default() -> Self {
        Self {
            live: 0,
            shadow: 1,
            placebo: 0,
        }
    }
}

impl ValidateConfig {
    /// Resolve this object, naming `entry` on anything unusable.
    ///
    /// `Ok(None)` for a project that did not enable the loop, which is the
    /// ordinary answer and not a failure.
    pub(super) fn to_terms(
        &self,
        path: &str,
        entry: &str,
    ) -> Result<Option<ValidationTerms>, ControlPlaneError> {
        // Checked before the `enabled` short-circuit on purpose: a project that
        // writes a broken share table and leaves the loop off has still written
        // a broken share table, and the day it flips `enabled` is the worst
        // possible moment to discover it.
        let shares = ArmShares::new(self.arms.live, self.arms.shadow, self.arms.placebo)
            .ok_or_else(|| ControlPlaneError::ArmSharesEmpty {
                path: path.to_string(),
                entry: entry.to_string(),
            })?;
        if !(0.0..=1.0).contains(&self.placebo_rate) {
            return Err(ControlPlaneError::PlaceboRateOutOfRange {
                path: path.to_string(),
                entry: entry.to_string(),
                placebo_rate: self.placebo_rate,
            });
        }
        if !(0.0..=1.0).contains(&self.escalation_floor) {
            return Err(ControlPlaneError::EscalationFloorOutOfRange {
                path: path.to_string(),
                entry: entry.to_string(),
                escalation_floor: self.escalation_floor,
            });
        }
        if !self.enabled {
            return Ok(None);
        }
        Ok(Some(ValidationTerms {
            shares,
            action: ActionPolicy {
                channel: self.channel,
                escalation_floor: self.escalation_floor,
                escalation_turns: self.escalation_turns,
                steer_after_interventions: self.steer_after_interventions,
            },
            placebo_rate: self.placebo_rate,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value) -> Result<Option<ValidationTerms>, ControlPlaneError> {
        let config: ValidateConfig =
            serde_json::from_value(json).expect("the fixture is well-shaped JSON");
        config.to_terms("fixture", "project `acme`")
    }

    #[test]
    fn a_project_that_says_nothing_but_enabled_gets_shadow_and_never_acts() {
        let terms = parse(serde_json::json!({ "enabled": true }))
            .expect("the default table is a table")
            .expect("an enabled project resolves to terms");
        assert_eq!(terms.shares, ArmShares::shadow_only());
        assert_eq!(
            terms.action.channel,
            SteerChannel::Off,
            "the strongest thing an unconfigured installation does is measure"
        );

        // The control: the identical object with `enabled` absent resolves to
        // nothing at all, which is what stamps no arm and enrols no session.
        assert_eq!(parse(serde_json::json!({})).expect("valid"), None);
    }

    #[test]
    fn a_share_table_that_sums_to_zero_is_refused_even_while_the_loop_is_off() {
        // Probe: all-zero weights have no honest reading, and the tempting
        // fallback — everything in one arm — is the failure `ArmShares` exists
        // to prevent.
        let error = parse(serde_json::json!({
            "enabled": false,
            "arms": { "live": 0, "shadow": 0, "placebo": 0 },
        }))
        .expect_err("a share table that shares nothing must not load");
        let message = error.to_string();
        assert!(
            message.contains("project `acme`"),
            "the refusal has to name the entry an operator would go and fix: \
             {message}"
        );

        // The control: a table with any weight at all loads, so the refusal is
        // about the sum and not about the object being present.
        assert!(
            parse(serde_json::json!({
                "enabled": false,
                "arms": { "live": 1, "shadow": 0, "placebo": 0 },
            }))
            .is_ok()
        );
    }

    #[test]
    fn a_rate_or_a_floor_outside_its_range_is_refused_rather_than_clamped() {
        for bad in [-0.1, 1.5] {
            assert!(
                parse(serde_json::json!({ "enabled": true, "placebo_rate": bad })).is_err(),
                "a placebo rate of {bad} is a control that is not one"
            );
            assert!(
                parse(serde_json::json!({ "enabled": true, "escalation_floor": bad })).is_err(),
                "an escalation floor of {bad} names a quality band no model is in"
            );
        }
        // The controls: the boundary values themselves are legal. A floor of
        // 1.0 is "only a flagship will do" and a rate of 0.0 is a placebo arm
        // that never intervenes — both are things an operator writes on purpose.
        assert!(
            parse(serde_json::json!({
                "enabled": true, "placebo_rate": 0.0, "escalation_floor": 1.0,
            }))
            .is_ok()
        );
    }

    #[test]
    fn a_mistyped_knob_fails_to_load_rather_than_failing_to_work() {
        let config: Result<ValidateConfig, _> =
            serde_json::from_value(serde_json::json!({ "enabeld": true }));
        assert!(
            config.is_err(),
            "a knob that silently did nothing would be a deployment convinced \
             it had configured an experiment"
        );
    }
}
