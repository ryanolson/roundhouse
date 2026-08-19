// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What an agent may ask of its own routing, and for how long.
//!
//! # Narrowing has two halves, and only one of them lives in core
//!
//! [`TurnPolicy::narrow`] is total and can only shrink the admissible set, so
//! an overlay asking for something *wider* than the deployment's ceiling is
//! clamped by construction. That is the half the type system enforces, and it
//! is the half that makes these tools safe to hand to a model.
//!
//! The other half it cannot see: an overlay that shrinks the admissible set to
//! **nothing** is a perfectly good narrowing and a catastrophic policy. M2
//! settled what happens then — `an_empty_admissible_set_fails_rather_than_
//! silently_going_local` — so an overlay that emptied the set would fail every
//! remaining turn of the session, at a seam the agent cannot reach to undo it.
//! An agent asking for frontier on a local-only project must therefore be told
//! `narrowed: true` and left routable, which is exactly the plan's example.
//!
//! Both halves are one rule stated once, in [`crate::plane`]: *the overlay is
//! applied only insofar as it leaves at least one admissible target, and any
//! shortfall between the ask and what was applied is reported as `narrowed`.*
//!
//! # Why a mode resolves to identities, not to a provider glob
//!
//! [`PreferMode::Frontier`] means "not local", and the filter dialect has no
//! negation — `*` crosses `/`, so no pattern excludes `local/…`. The resolution
//! is therefore to enumerate: a mode narrows to the exact
//! [`policy_identity`](Target::policy_identity) list the ceiling admits on that
//! side of the fleet, computed once when the agent asks.
//!
//! That the catalog is baked in at ask time is a property and not a leak. An
//! overlay is a *narrowing*, and a narrowing that silently grew to cover a
//! model an operator added an hour later would be a widening with an
//! agent-authored trigger. The list going stale downward is harmless — a
//! removed model is not admissible anyway — and going stale upward is the
//! behavior we want.

use serde::{Deserialize, Serialize};

use roundhouse_core::control::{PolicyOverrides, TargetFilter, TurnPolicy};
use roundhouse_core::routing::Target;

/// Which side of the fleet the agent asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferMode {
    Local,
    Frontier,
    /// Neither: release whatever mode overlay this session was carrying.
    ///
    /// A release, not a widening, and the distinction is the reason it is safe.
    /// Narrowing-only is a rule about the *ceiling*, which an overlay never
    /// touches; dropping the overlay returns the session to
    /// `narrow(ceiling, nothing) == ceiling` and no further. An agent may
    /// always let go of a restriction it imposed on itself.
    Auto,
}

impl PreferMode {
    /// Whether `target` is on the side of the fleet this mode asked for.
    fn wants(&self, target: &Target) -> bool {
        match self {
            PreferMode::Local => target.is_local(),
            PreferMode::Frontier => !target.is_local(),
            PreferMode::Auto => true,
        }
    }
}

/// How long an overlay lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayScope {
    /// The next turn, and only the next turn.
    Turn,
    /// Until it is replaced, or until a turn count runs out.
    Session,
}

/// A mode ask, together with the narrowing it resolved to.
///
/// Both halves are kept because they answer different questions. The `mode` is
/// what the agent said and what `status` renders back to it; the `allow` filter
/// is what the engine applies, resolved against the catalog in force when the
/// ask was made. Deriving one from the other at read time would need the
/// catalog at every turn start and would make the overlay's meaning drift under
/// an operator's edits — see the module note.
#[derive(Debug, Clone, PartialEq)]
pub struct ModeNarrowing {
    pub mode: PreferMode,
    /// `None` for [`PreferMode::Auto`], which narrows nothing.
    pub allow: Option<TargetFilter>,
}

impl ModeNarrowing {
    /// Resolve `mode` against the targets the ceiling admits.
    ///
    /// `None` means the ask cannot be honored at all: this side of the fleet
    /// holds nothing this key may be routed to, so applying it would empty the
    /// admissible set. The caller reports that as `narrowed` and stores
    /// nothing — see [`crate::plane`].
    pub fn resolve(mode: PreferMode, ceiling_targets: &[Target]) -> Option<Self> {
        if mode == PreferMode::Auto {
            return Some(Self { mode, allow: None });
        }
        let wanted: Vec<String> = ceiling_targets
            .iter()
            .filter(|target| mode.wants(target))
            .map(Target::policy_identity)
            .collect();
        if wanted.is_empty() {
            return None;
        }
        // `parse` refuses the characters the digest encoding separates on, and
        // a `policy_identity` cannot contain them — both halves are
        // `provider/model` slugs. A failure here would mean the catalog holds a
        // target no `TargetFilter` could ever name, which is a deployment
        // problem and not this agent's; it is reported as an unhonorable ask
        // rather than swallowed.
        let allow = TargetFilter::parse(wanted).ok()?;
        Some(Self {
            mode,
            allow: Some(allow),
        })
    }
}

/// One axis of an overlay, and how many turns it has left.
#[derive(Debug, Clone, PartialEq)]
pub struct TimedOverlay<T> {
    pub ask: T,
    /// `None` means "until replaced". A `Some(0)` is not representable in a
    /// live overlay: [`SessionOverlay::consume`] drops an axis the moment its
    /// last turn is spent, so a reader never has to decide whether zero means
    /// expired or unlimited.
    pub remaining_turns: Option<u32>,
    pub reason: String,
}

/// The narrowing one session is carrying, at most one record per axis.
///
/// Replacing rather than stacking. Two live mode overlays would need a
/// composition rule, and the only honest composition rule is `narrow` itself,
/// which the ceiling already applies — so a second one would be a second
/// spelling of the same operator. An agent that wants a different preference
/// says so, and the newer sentence is the one it meant.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionOverlay {
    pub mode: Option<TimedOverlay<ModeNarrowing>>,
    pub floor: Option<TimedOverlay<f64>>,
}

impl SessionOverlay {
    pub fn is_empty(&self) -> bool {
        self.mode.is_none() && self.floor.is_none()
    }

    /// The narrowing this overlay means, in the vocabulary
    /// [`TurnPolicy::narrow`] takes.
    ///
    /// `frontier_cadence` is absent and stays absent: a cadence is a
    /// deployment's ration of hosted turns across a session, and an agent
    /// tightening its own ration would be indistinguishable in the audit trail
    /// from an operator having done so. The two overlay tools are the two axes
    /// an agent may move.
    pub fn overrides(&self) -> PolicyOverrides {
        PolicyOverrides {
            min_quality: self.floor.as_ref().map(|floor| floor.ask),
            allow: self.mode.as_ref().and_then(|mode| mode.ask.allow.clone()),
            frontier_cadence: None,
        }
    }

    /// The policy `ceiling` becomes once this overlay is applied.
    pub fn apply_to(&self, ceiling: &TurnPolicy) -> TurnPolicy {
        ceiling.narrow(&self.overrides())
    }

    /// Spend one turn of every axis, dropping the ones that run out.
    ///
    /// Called once per turn by the engine, at the same seam the admission
    /// policy is resolved — so the turn that consumes an overlay is the turn
    /// routed under it, and the `turn_policy_digest` on that turn's decision is
    /// the observable an operator checks the overlay against.
    pub fn consume(&mut self) {
        fn spend<T>(axis: &mut Option<TimedOverlay<T>>) {
            let expired = match axis {
                Some(TimedOverlay {
                    remaining_turns: Some(left),
                    ..
                }) => {
                    *left = left.saturating_sub(1);
                    *left == 0
                }
                _ => false,
            };
            if expired {
                *axis = None;
            }
        }
        spend(&mut self.mode);
        spend(&mut self.floor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(model: &str) -> Target {
        Target::Local {
            worker_id: 1,
            dp_rank: 0,
            model: model.into(),
        }
    }

    fn frontier(provider: &str, model: &str) -> Target {
        Target::Frontier {
            provider: provider.into(),
            model: model.into(),
        }
    }

    #[test]
    fn a_mode_resolves_to_the_identities_on_its_side_of_the_fleet() {
        let fleet = [
            local("llama-3.1-8b"),
            frontier("anthropic", "claude-opus-4"),
            frontier("openai", "gpt-5"),
        ];

        let to_local = ModeNarrowing::resolve(PreferMode::Local, &fleet).expect("local exists");
        let allow = to_local.allow.expect("a narrowing");
        assert!(allow.matches(&local("llama-3.1-8b")));
        assert!(!allow.matches(&frontier("anthropic", "claude-opus-4")));

        let to_frontier =
            ModeNarrowing::resolve(PreferMode::Frontier, &fleet).expect("frontier exists");
        let allow = to_frontier.allow.expect("a narrowing");
        assert!(allow.matches(&frontier("anthropic", "claude-opus-4")));
        assert!(allow.matches(&frontier("openai", "gpt-5")));
        assert!(
            !allow.matches(&local("llama-3.1-8b")),
            "`frontier` means not-local, and the dialect has no negation -- so it \
             has to be spelled by enumeration or it is not spelled at all"
        );

        assert!(
            ModeNarrowing::resolve(PreferMode::Auto, &fleet)
                .expect("auto always resolves")
                .allow
                .is_none(),
            "auto releases the axis rather than narrowing it"
        );
    }

    #[test]
    fn a_mode_with_nothing_on_its_side_of_the_fleet_does_not_resolve() {
        // The probe: the plan's own example. A local-only key asking for
        // frontier must not produce a filter that admits nothing, because an
        // empty admissible set fails every remaining turn of the session.
        assert!(
            ModeNarrowing::resolve(PreferMode::Frontier, &[local("llama")]).is_none(),
            "an unhonorable ask has to be visible to the caller as unhonorable"
        );
        // The control: the same key asking for local resolves fine.
        assert!(ModeNarrowing::resolve(PreferMode::Local, &[local("llama")]).is_some());
    }

    #[test]
    fn an_overlay_axis_expires_the_turn_its_last_ration_is_spent() {
        let mut overlay = SessionOverlay {
            mode: Some(TimedOverlay {
                ask: ModeNarrowing {
                    mode: PreferMode::Local,
                    allow: None,
                },
                remaining_turns: Some(2),
                reason: "cheap work".into(),
            }),
            floor: Some(TimedOverlay {
                ask: 0.9,
                remaining_turns: None,
                reason: "hard work".into(),
            }),
        };

        overlay.consume();
        assert_eq!(overlay.mode.as_ref().unwrap().remaining_turns, Some(1));
        overlay.consume();
        assert!(overlay.mode.is_none(), "two turns, two consumes, then gone");
        assert!(
            overlay.floor.is_some(),
            "an unbounded axis is not touched by the count of a bounded one"
        );
        assert_eq!(
            overlay.floor.as_ref().unwrap().remaining_turns,
            None,
            "and does not acquire a count by being consumed"
        );
    }
}
