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

use super::Strategy;
use super::evidence::{Draw, GateResult, PlanEvidence};
use crate::ids::{ResponseId, SessionId};
use crate::routing::Target;

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

/// The exploration set: every `Unproven` strategy whose first target meets
/// every hard constraint and costs strictly less than `reference`'s, in
/// configured order.
///
/// **Unproven, not below the floor.** Reviews already put a `BelowFloor`
/// strategy under the floor, and exploring it would buy evidence that exists.
///
/// **Strictly cheaper.** Exploration exists to find a cheaper route that
/// holds quality; a member at the reference's price would spend a quality
/// bypass to learn nothing about cost. It also keeps every member's first
/// target distinct from the reference's, which [`propensity`] relies on.
///
/// **Every hard constraint**, by [`PlanEvidence::meets_hard`], the predicate
/// the exploit path uses.
pub fn eligible(plans: &[PlanEvidence], reference: &PlanEvidence) -> Vec<Strategy> {
    plans
        .iter()
        .filter(|plan| {
            plan.gate.result == GateResult::Unproven
                && plan.meets_hard()
                && plan.cost.adjusted_usd < reference.cost.adjusted_usd
        })
        .map(|plan| plan.strategy)
        .collect()
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
                .any(|plan| plan.strategy == **member && &plan.first == served)
        })
        .count();
    let exploit = if default == Some(served) {
        1.0 - rate
    } else {
        0.0
    };
    exploit + rate * sharing as f64 / set.len() as f64
}
