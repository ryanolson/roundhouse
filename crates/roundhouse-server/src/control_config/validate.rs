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

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// `0`, the default, means the steer path is off: escalation claims the
    /// uninterrupted turn, so a cap of zero admits nothing after it. Set `1` or
    /// more to turn outcome B on. See
    /// [`ActionPolicy::steer_after_interventions`].
    pub steer_after_interventions: u32,
    /// What to say on the first turn served under a signal-driven escalation,
    /// appended to the forwarded request and never to the stored conversation.
    ///
    /// **Absent is off**, which is R2's shipped posture and the reason this is
    /// an `Option<String>` rather than a string with a default: a deployment
    /// that has not decided what to tell an escalated turn has not decided to
    /// decorate one, and substituting the example wording for a missing value
    /// would put roundhouse's words in a request nobody asked for them in.
    /// [`EXAMPLE_HANDOFF_NOTE`](roundhouse_core::validate::EXAMPLE_HANDOFF_NOTE)
    /// is what a project turning it on can copy.
    ///
    /// The `[roundhouse-guidance]` marker is *not* part of this value —
    /// roundhouse prepends it, so an operator cannot accidentally ship an
    /// unattributable note. Write the sentences only.
    pub handoff_note: Option<String>,
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
            handoff_note: None,
        }
    }
}

/// The `"arms"` object: three weights.
///
/// Weights rather than percentages so "one session in fifty is a placebo" is
/// expressible without anybody writing `0.02` and wondering about rounding.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
        // Above the `enabled` short-circuit, on the same argument the share
        // table is checked under: a project that names a retired channel and
        // leaves the loop off has still named a retired channel, and the day it
        // flips `enabled` is the worst possible moment to find out that the
        // channel it chose stopped existing.
        if self.channel == SteerChannel::ToolCall {
            return Err(ControlPlaneError::SteerChannelRetired {
                path: path.to_string(),
                entry: entry.to_string(),
            });
        }
        // Above the short-circuit for the third time, and for the third time on
        // the same argument. A note made of whitespace is the one value that is
        // worse than no note: it renders as a bare `[roundhouse-guidance]` in
        // the middle of the agent's own request — roundhouse telling a model
        // that something is wrong and refusing to say what — and the project
        // that wrote it believes it configured a narration. `non_empty` in the
        // session fold refuses an empty *correction* for the same reason; this
        // is that rule applied one layer earlier, where an operator can still
        // fix it.
        if self
            .handoff_note
            .as_ref()
            .is_some_and(|note| note.trim().is_empty())
        {
            return Err(ControlPlaneError::HandoffNoteEmpty {
                path: path.to_string(),
                entry: entry.to_string(),
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
            handoff_note: self.handoff_note.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roundhouse_core::validate::{EXAMPLE_HANDOFF_NOTE, HANDOFF_MARKER};

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

    /// **T2.** `tool_call` is refused by name, never remapped.
    ///
    /// The remap is the tempting fix and it is the wrong one: a deployment that
    /// wrote `tool_call` chose the protocol-heavy path deliberately, and serving
    /// it text while its config file still says otherwise leaves it believing
    /// something false about its own installation. The refusal has to cite the
    /// plan, because "unknown value" reads like a typo and names nothing an
    /// operator can go and read.
    #[test]
    fn a_retired_tool_call_channel_is_refused_by_name_and_never_remapped() {
        let error = parse(serde_json::json!({
            "enabled": true,
            "channel": "tool_call",
        }))
        .expect_err("a retired channel must not load as something else");
        let message = error.to_string();
        assert!(
            message.contains("project `acme`"),
            "the refusal names the entry an operator would go and fix: {message}"
        );
        assert!(
            message.contains("tool_call"),
            "and names the value that is retired, not just the field: {message}"
        );
        assert!(
            message.contains("PLAN-frontier-selection.md"),
            "and cites the ruling, so the answer to `why` is one file away: \
             {message}"
        );

        // Refused with the loop *off* too, on the share table's own argument:
        // the day `enabled` flips is the worst moment to discover the channel
        // stopped existing.
        assert!(parse(serde_json::json!({ "enabled": false, "channel": "tool_call" })).is_err());

        // The controls: the two spellings that mean what `tool_call` was chosen
        // for now load, and they load to the same thing.
        for channel in ["auto", "text"] {
            let terms = parse(serde_json::json!({ "enabled": true, "channel": channel }))
                .unwrap_or_else(|error| panic!("`{channel}` must load: {error}"))
                .expect("an enabled project resolves to terms");
            assert!(matches!(
                terms.action.channel,
                SteerChannel::Auto | SteerChannel::Text
            ));
        }
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

    /// **R2/T6.** The handoff note is off unless a project writes one, and a
    /// note that says nothing is refused rather than shipped.
    ///
    /// Three states, and the middle one is the whole reason this has its own
    /// test: absent means "do not narrate", a written note means "narrate with
    /// these words", and `""` means a project that meant to narrate and would
    /// ship a bare `[roundhouse-guidance]` marker into an agent's own request.
    /// Treating the third as the first is the tempting repair and it is the
    /// wrong one — it leaves the deployment believing it configured something.
    #[test]
    fn a_handoff_note_is_absent_by_default_and_never_configured_empty() {
        // Absent: R2's shipped posture, and the state most deployments are in.
        let quiet = parse(serde_json::json!({ "enabled": true }))
            .expect("valid")
            .expect("an enabled project resolves to terms");
        assert_eq!(
            quiet.handoff_note, None,
            "a project that said nothing about narration narrates nothing"
        );

        // Written: carried through verbatim, marker *not* included — roundhouse
        // prepends it, so an operator cannot ship an unattributable note.
        let narrating = parse(serde_json::json!({
            "enabled": true,
            "handoff_note": EXAMPLE_HANDOFF_NOTE,
        }))
        .expect("valid")
        .expect("an enabled project resolves to terms");
        assert_eq!(
            narrating.handoff_note.as_deref(),
            Some(EXAMPLE_HANDOFF_NOTE)
        );
        assert!(
            !EXAMPLE_HANDOFF_NOTE.contains(HANDOFF_MARKER),
            "the example a deployment copies must not carry the marker, or a \
             decorated request would say it twice"
        );

        // Empty, in both spellings a config file can produce it.
        for empty in ["", "   \n "] {
            let error = parse(serde_json::json!({ "enabled": true, "handoff_note": empty }))
                .expect_err("a note with nothing in it must not load");
            let message = error.to_string();
            assert!(
                message.contains("project `acme`"),
                "the refusal names the entry an operator would go and fix: {message}"
            );
            assert!(
                message.contains("handoff_note"),
                "and the key, since a project may have several strings: {message}"
            );
        }

        // Refused with the loop off too, on the share table's own argument.
        assert!(parse(serde_json::json!({ "enabled": false, "handoff_note": "" })).is_err());
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
