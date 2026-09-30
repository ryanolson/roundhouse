// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bounded exploration (the 2026-09-28 ruling 8, plan sections 3 and 4).
//!
//! Exploration is a quality bypass, so it is fenced on every side. The policy
//! decides *whether* a turn could explore ([`LearnedPolicy`](super::LearnedPolicy));
//! this module holds the three pieces that must be exactly reproducible
//! offline: the draw, the eligible set, and the probability that a turn served
//! the first target it served.
//!
//! **The draw is a hash, never a random number.** A replay of the log must
//! reach the decision the process reached, so the engine computes
//! [`Draw::for_turn`] from the arm salt, the session and the response, and the
//! record keeps it. The policy never draws.

use sha2::{Digest, Sha256};

use super::evidence::{Draw, GateResult, PlanEvidence};
use super::policy::exploit_order;
use super::{OnInfeasible, Strategy};
use crate::ids::{ResponseId, SessionId};
use crate::routing::Target;
use crate::session::same_route;

/// The version tag the exploration hash input opens with.
///
/// Moved first whenever the encoding changes, as `Arm::for_session`'s tag is:
/// an explored turn's draw is recorded, but a calibrator that recomputes one
/// to audit it must know which encoding wrote it.
pub const LEARNER_DRAW_VERSION: &str = "v1";

impl Draw {
    /// The draw for one turn: SHA-256 over
    /// `"{LEARNER_DRAW_VERSION}\nrouting-explore\nsalt={salt}\nsession={session}\nresponse={response}\n"`.
    ///
    /// **The `routing-explore` domain keeps this stream apart from the arm
    /// assignment**, which hashes the same salt and session under `arm`. Were
    /// they one stream, which sessions explore would correlate with which arm
    /// they are in, and the arm comparison would measure exploration too.
    ///
    /// The first 8 bytes give the rate draw and the next 8 the member draw.
    /// **The rate is the top 53 bits over 2^53**, which is exact in `f64` and
    /// so strictly below 1. Dividing the whole `u64` by 2^64 rounds the top
    /// 1024 values to exactly 1.0, and a draw of 1.0 would sit outside the
    /// `[0, 1)` the record promises.
    pub fn for_turn(salt: &str, session: &SessionId, response: &ResponseId) -> Draw {
        let digest = Sha256::digest(
            format!(
                "{LEARNER_DRAW_VERSION}\nrouting-explore\nsalt={salt}\nsession={session}\nresponse={response}\n"
            )
            .as_bytes(),
        );
        let word = |at: usize| {
            u64::from_be_bytes(
                digest[at..at + 8]
                    .try_into()
                    .expect("sha-256 yields 32 bytes"),
            )
        };
        Draw {
            rate: (word(0) >> 11) as f64 / (1u64 << 53) as f64,
            member: word(8),
        }
    }
}

/// The exploration set of a turn that could explore: every `Unproven`
/// strategy whose first target meets every hard constraint and costs strictly
/// less than the reference, in configured order, then `rules` when its first
/// target meets every hard constraint and it is not already a member.
///
/// **The reference is the exploit plan**, the head of `policy::exploit_order`,
/// or the `rules` plan when nothing passes. Derived here from the plans and
/// `on_infeasible` alone, so the policy that draws from the set and the
/// calibrator that checks a record's set
/// (`offline::extract::replays`) read one rule: two spellings would agree
/// until one changed, and a record would then replay against a set no build
/// drew from.
///
/// **Unproven, not below the floor.** Reviews already put a `BelowFloor`
/// strategy under the floor, and exploring it would buy evidence that exists.
///
/// **Strictly cheaper.** Exploration exists to find a cheaper route that
/// holds quality; a member at the reference's price would spend a quality
/// bypass to learn nothing about cost. [`propensity`] does not rely on it:
/// it sums over every way the served route could have been chosen.
///
/// **`rules` joins exempt from both filters** (the owner's ruling of
/// 2026-09-30). It is not a probe for a cheaper route but the baseline every
/// candidate is compared with: without it, `rules` has zero logging
/// probability wherever the learned choice differs, and the binding M11
/// quality comparison (test 1b) can never be evaluated there. So it joins
/// whether it passed, is unproven or sits below the floor, and whatever it
/// costs against the exploit. It is placed last so that the members before
/// it keep their configured order and their member indices; when the M4
/// filters already admit it, it keeps its place and is not listed twice,
/// since [`propensity`] counts members and a repeat would inflate its share.
///
/// **Every hard constraint**, by [`PlanEvidence::meets_hard`], the predicate
/// the exploit path uses, for `rules` too: a `rules` plan over the grant or
/// the latency limit never explores, as no other member does.
///
/// **Under `refuse`, `rules` joins only when some strategy passes** (the
/// owner's ruling of 2026-09-30). A `refuse` project has chosen to fail a
/// turn nothing passes rather than serve `rules` unvalidated; letting the
/// baseline in by exploration would serve exactly that turn at the rate.
/// Such a turn is refused as before `rules` joined, unless a cheaper
/// unproven member explores. Under `serve_rules` a turn nothing passes serves
/// `rules` anyway, so `rules` joins there too.
///
/// Changing this rule is a new
/// [`LEARNED_SELECTOR_REVISION`](crate::routing::LEARNED_SELECTOR_REVISION).
pub fn eligible(plans: &[PlanEvidence], on_infeasible: OnInfeasible) -> Vec<Strategy> {
    let rules = plans.iter().find(|plan| plan.strategy == Strategy::Rules);
    let exploit = exploit_order(plans).first().map(|&at| &plans[at]);
    let Some(reference) = exploit.or(rules) else {
        return Vec::new();
    };
    let mut set: Vec<Strategy> = plans
        .iter()
        .filter(|plan| {
            plan.gate.result == GateResult::Unproven
                && plan.meets_hard()
                && plan.cost.adjusted_usd < reference.cost.adjusted_usd
        })
        .map(|plan| plan.strategy)
        .collect();
    if has_default(exploit.is_some(), on_infeasible)
        && rules.is_some_and(PlanEvidence::meets_hard)
        && !set.contains(&Strategy::Rules)
    {
        set.push(Strategy::Rules);
    }
    set
}

/// Whether a turn that does not explore still serves a route: the exploit
/// plan's when some strategy passes, else `rules` under `serve_rules`, and
/// none under `refuse`.
///
/// One predicate for both readers: [`eligible`] admits `rules` only to a turn
/// that has one, and the policy credits the `1 - rate` share of
/// [`propensity`] to it. Were they spelled apart, a change to one would record
/// a set and a propensity drawn under two rules.
pub(crate) fn has_default(some_passes: bool, on_infeasible: OnInfeasible) -> bool {
    some_passes || on_infeasible == OnInfeasible::ServeRules
}

/// The probability that a turn which could explore over `set` served
/// `served`, summed over every way the policy could have served it (draft
/// 14.2, plan section 3 item 2).
///
/// With probability `1 - rate` the turn does not explore and serves
/// `default`: the exploit target, or the `rules` target under `serve_rules`
/// when nothing passes, or nothing under `refuse`. With probability `rate` it
/// explores and serves each member with probability `1 / |set|`, so a target
/// that is the first target of two members has twice that share.
///
/// **Targets are compared by [`same_route`]**, the rule credit and the
/// offline calibrator match a served target by. Two workers of one local
/// model are one route there, so they are one route here: comparing with
/// `==` would record half the probability for a route two members share on
/// different workers, and the calibrator would weight that turn double.
///
/// `set` must be non-empty; a turn that could not explore served its target
/// with probability 1 and does not call this.
pub fn propensity(
    plans: &[PlanEvidence],
    set: &[Strategy],
    served: &Target,
    default: Option<&Target>,
    rate: f64,
) -> f64 {
    let sharing = set
        .iter()
        .filter(|member| {
            plans
                .iter()
                .any(|plan| plan.strategy == **member && same_route(&plan.first, served))
        })
        .count();
    let exploit = if default.is_some_and(|default| same_route(default, served)) {
        1.0 - rate
    } else {
        0.0
    };
    exploit + rate * sharing as f64 / set.len() as f64
}
