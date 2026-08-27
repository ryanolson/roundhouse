// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The shape an operator writes for a project's or a key's `"fair_use"`, and
//! the boundary that turns it into the real [`FairUseLimit`]s the ledger is
//! checked against.
//!
//! Split out of [`config`](super::config) for the reason [`budget`](super::budget)
//! is: one file per config object, judged by one boundary. [`ControlPlaneError`]
//! stays in `config`, because it is the one error enum every validator in this
//! crate reports through and a second table is a second thing an operator has
//! to learn to read.
//!
//! **The same shape serves both scopes**, exactly as [`PolicyConfig`] serves a
//! project's `policy` and a key's `overrides` — and unlike that pair, an absent
//! block means the same thing at both: no ceiling at that scope. There is no
//! narrowing rule here and deliberately none: a member window is not an overlay
//! on the project's, it is a second ceiling that binds independently, which is
//! the whole reason `the_member_window_binds_even_when_the_projects_has_room`
//! is a test rather than a comment.
//!
//! [`PolicyConfig`]: super::config::PolicyConfig

use serde::{Deserialize, Serialize};

use roundhouse_core::control::{FairUseLimit, FairUseTerms, FairUseWindow};

use super::config::ControlPlaneError;

/// One project's or one key's `"fair_use"` object.
///
/// **`Serialize` as well as `Deserialize`, which the other config shapes are
/// deliberately not.** [`ProjectDto`](crate::admin_api) shows a project's
/// budget as a bare `budgeted: bool` and sends a reader to the budget view for
/// the number, on the ground that a limit shown without what has been spent
/// against it is the figure people quote. Fair use has no such second view —
/// the rolling counters are not a ledger anyone can read a balance out of — so
/// the block an operator wrote *is* the whole answer to "what is in force", and
/// the admin plane accepts that same shape on create and on `PATCH`. One
/// vocabulary read back and written, rather than a hand-rolled view struct that
/// would be the second spelling of a window (G14).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FairUseConfig {
    pub windows: Vec<FairUseWindowConfig>,
}

/// One rolling window and what it caps.
///
/// `deny_unknown_fields` sharpened by what the fields are: `max_token` for
/// `max_tokens` is a window that caps *nothing*, which reads in the file as a
/// limit and behaves as an absence — the exact class of mistake the control
/// plane's other shapes carry this attribute for.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FairUseWindowConfig {
    pub window: FairUseWindow,
    /// `skip_serializing_if` on the read path only: an absent cap and a `null`
    /// one mean the same thing to the loader, and echoing `"max_usd": null`
    /// back would show an operator a field they did not write beside one they
    /// did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usd: Option<f64>,
}

impl FairUseConfig {
    /// Judge and resolve into the ceilings the ledger checks, naming `entry` on
    /// every rejection.
    pub(super) fn to_limits(
        &self,
        path: &str,
        entry: &str,
    ) -> Result<Vec<FairUseLimit>, ControlPlaneError> {
        let mut seen: Vec<FairUseWindow> = Vec::new();
        let mut limits = Vec::with_capacity(self.windows.len());
        for window in &self.windows {
            // A window naming neither cap is the whole of the addendum's
            // "refuse an empty window entry": it reads as a limit, enforces
            // nothing, and is indistinguishable afterwards from an operator who
            // meant to leave the scope uncapped.
            if window.max_tokens.is_none() && window.max_usd.is_none() {
                return Err(ControlPlaneError::FairUseWindowCapsNothing {
                    path: path.to_string(),
                    entry: entry.to_string(),
                    window: window.window.wire_name(),
                });
            }
            // A cap of zero — or of `NaN`, which JSON cannot spell but a future
            // producer of this struct could — is a filter wearing a window's
            // clothes: it refuses every turn forever, which is
            // `"allow": ["local/*"]` said in the one vocabulary that promises a
            // limit clears on its own. Refused for the same reason
            // `max_frontier: 0` is, and with a second consequence worth naming:
            // a cap nothing can get under has no earliest retry time, so the
            // refusal would carry a `retry_at_ms` that is a fiction.
            if let Some(max_tokens) = window.max_tokens
                && max_tokens == 0
            {
                return Err(ControlPlaneError::FairUseCapNotPositive {
                    path: path.to_string(),
                    entry: entry.to_string(),
                    window: window.window.wire_name(),
                    field: "max_tokens",
                    value: 0.0,
                });
            }
            if let Some(max_usd) = window.max_usd
                && !matches!(max_usd.partial_cmp(&0.0), Some(std::cmp::Ordering::Greater))
            {
                return Err(ControlPlaneError::FairUseCapNotPositive {
                    path: path.to_string(),
                    entry: entry.to_string(),
                    window: window.window.wire_name(),
                    field: "max_usd",
                    value: max_usd,
                });
            }
            // Two entries for one window is the same ambiguity the catalog
            // refuses two prices for one model over: the ledger finds the first
            // and a reader assumes the tighter, so the limit enforced and the
            // limit written differ silently.
            if seen.contains(&window.window) {
                return Err(ControlPlaneError::FairUseDuplicateWindow {
                    path: path.to_string(),
                    entry: entry.to_string(),
                    window: window.window.wire_name(),
                });
            }
            seen.push(window.window);
            limits.push(FairUseLimit {
                window: window.window,
                max_tokens: window.max_tokens,
                max_usd: window.max_usd,
            });
        }
        Ok(limits)
    }
}

/// Pair a project's resolved windows with one membership's own.
///
/// The one place [`FairUseTerms`] is built from a configured pair, for the
/// reason [`budget_terms`](super::budget::budget_terms) is one function: two
/// spellings of "this membership's ceilings" is two things able to disagree
/// about whether an absent member block means *no member ceiling* or *the
/// project's ceiling again*, and the second reading would silently make every
/// member's window the project's.
pub(super) fn fair_use_terms(
    project: Vec<FairUseLimit>,
    member: Vec<FairUseLimit>,
) -> FairUseTerms {
    FairUseTerms { project, member }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> Result<FairUseConfig, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn a_window_is_read_with_the_caps_the_file_names() {
        let limits = config(
            r#"{ "windows": [
                 { "window": "5h", "max_tokens": 2000000 },
                 { "window": "7d", "max_usd": 40.0 }
               ] }"#,
        )
        .unwrap()
        .to_limits("test", "project `acme`")
        .unwrap();

        assert_eq!(limits.len(), 2);
        assert_eq!(limits[0].window, FairUseWindow::FiveHours);
        assert_eq!(limits[0].max_tokens, Some(2_000_000));
        assert_eq!(
            limits[0].max_usd, None,
            "a cap nobody wrote is absent, not zero"
        );
        assert_eq!(limits[1].max_usd, Some(40.0));
    }

    #[test]
    fn the_three_shapes_that_would_enforce_nothing_are_refused_at_load() {
        let refused = |json: &str| {
            config(json)
                .unwrap()
                .to_limits("test", "project `acme`")
                .expect_err("this shape enforces nothing")
        };

        // Neither cap: a limit that limits nothing.
        assert!(matches!(
            refused(r#"{ "windows": [{ "window": "5h" }] }"#),
            ControlPlaneError::FairUseWindowCapsNothing { .. }
        ));
        // A cap of zero: refuses every turn forever, and has no honest retry
        // time to report.
        assert!(matches!(
            refused(r#"{ "windows": [{ "window": "24h", "max_tokens": 0 }] }"#),
            ControlPlaneError::FairUseCapNotPositive {
                field: "max_tokens",
                ..
            }
        ));
        assert!(matches!(
            refused(r#"{ "windows": [{ "window": "24h", "max_usd": 0.0 }] }"#),
            ControlPlaneError::FairUseCapNotPositive {
                field: "max_usd",
                ..
            }
        ));
        // Two entries for one window: the ledger reads one and a reader assumes
        // the other.
        assert!(matches!(
            refused(
                r#"{ "windows": [
                     { "window": "5h", "max_tokens": 10 },
                     { "window": "5h", "max_tokens": 20 }
                   ] }"#
            ),
            ControlPlaneError::FairUseDuplicateWindow { .. }
        ));

        // CONTROL: an empty `windows` list is accepted and means no ceiling.
        // That is not the same mistake — an operator who wrote no windows wrote
        // no limit, and there is nothing about it that reads as one.
        assert!(
            config(r#"{ "windows": [] }"#)
                .unwrap()
                .to_limits("test", "project `acme`")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_misspelled_cap_is_a_refusal_and_not_a_window_that_enforces_nothing() {
        // PROBE: `max_token`. Without `deny_unknown_fields` this parses as a
        // window naming neither cap, and the refusal above would fire with a
        // message about an empty window — pointing an operator at the wrong
        // remedy.
        assert!(config(r#"{ "windows": [{ "window": "5h", "max_token": 10 }] }"#).is_err());
        // And a window this build does not have.
        assert!(config(r#"{ "windows": [{ "window": "1h", "max_tokens": 10 }] }"#).is_err());
        // CONTROL: spelled right.
        assert!(config(r#"{ "windows": [{ "window": "5h", "max_tokens": 10 }] }"#).is_ok());
    }

    #[test]
    fn the_two_scopes_stay_two_ceilings() {
        let terms = fair_use_terms(
            vec![FairUseLimit {
                window: FairUseWindow::SevenDays,
                max_tokens: Some(100),
                max_usd: None,
            }],
            Vec::new(),
        );
        assert!(
            terms.member.is_empty(),
            "an absent member block is no member ceiling, never a copy of the \
             project's -- copying it would make every member's window the \
             project's and silently refuse the second member of a busy project"
        );
        assert!(!terms.is_empty(), "the project's ceiling is still in force");
    }
}
