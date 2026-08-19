// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which experiment a session is in, and how that is decided.
//!
//! The validate loop is shipped off and measured before it is enabled, because
//! the property that decides whether an excellent critic helps or collapses an
//! agent is the *agent's* disruption–recovery ratio, and that can only be
//! measured per deployment. An arm is how a deployment measures it: the same
//! trigger fires in all three, and what differs is what happens next.
//!
//! **Assignment is a hash, never a draw.** A random arm would make a replay of
//! the same log reach a different answer than the process that wrote it, which
//! breaks the invariant every rung of this plan is gated on — fold equals log.
//! That is disqualifying regardless of how much nicer a coin flip's statistics
//! are, so the arm is [`Arm::for_session`]: a deterministic function of the
//! session id and a deployment salt, computed once and *stamped* into
//! `SessionCreated`.
//!
//! Stamped and not merely recomputed, because the salt is configuration and an
//! operator edits configuration. Recomputing on replay would silently
//! re-assign every historical session the day the salt moved, and the arm
//! comparison — the entire point of the instrumentation — would be computed
//! across a boundary nobody recorded.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::{ResponseId, SessionId};

/// The version tag every arm-assignment hash input opens with.
///
/// The same rule [`DIGEST_VERSION`](crate::control::TurnPolicy::digest) carries:
/// when the encoding below changes, this moves first, so that a deployment
/// mid-experiment can tell an assignment made under the old encoding from one
/// made under the new. Editing the encoding without moving the tag silently
/// re-buckets every session that has not yet been stamped.
const ASSIGNMENT_VERSION: &str = "v1";

/// What a session's validations are for.
///
/// Three arms rather than a boolean because the question is not "does
/// validation work" but "does *the judge* work": an agent that recovers after
/// any interruption at all would make a useless judge look excellent, and only
/// a sham-intervention arm can tell those apart.
// `Ord` so an arm can key a `BTreeMap` in the metrics fold: the arm comparison
// is reported arm by arm, and a report whose rows reshuffled between polls
// would be a report nobody could diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    /// The judge runs and its action is taken.
    Live,
    /// The judge runs, everything is logged, and the action is discarded.
    ///
    /// The observe-only arm, and the one available from day one under every
    /// steer channel including `Off`: it is the literature's 50-task pilot
    /// expressed as configuration rather than as a procedure somebody has to
    /// remember to run.
    Shadow,
    /// No judge runs, and an intervention happens anyway on deterministic
    /// timing.
    ///
    /// The control for the Intervention Paradox. Without it, "tokens fell
    /// after we steered" is consistent with the steer having said anything at
    /// all — including nothing useful — because the disruption itself changes
    /// the trajectory.
    Placebo,
}

impl Arm {
    /// Whether an action computed in this arm is actually taken.
    ///
    /// The one place the difference between Shadow and the other two is
    /// spelled. A caller that branched on the arm itself would have to
    /// remember which of three names means "discard", and the fold, the
    /// occupant and the intervention counter all need the same answer.
    pub fn acts(self) -> bool {
        match self {
            Arm::Live | Arm::Placebo => true,
            Arm::Shadow => false,
        }
    }

    /// Whether this arm consults the judge.
    ///
    /// Not the negation of [`Self::acts`], and the asymmetry is the design:
    /// Shadow pays for a judge whose answer it throws away, Placebo throws away
    /// the judge and keeps the intervention. Collapsing the two questions into
    /// one flag is what would make the experiment unable to separate the cost
    /// of asking from the effect of interrupting.
    pub fn consults_judge(self) -> bool {
        match self {
            Arm::Live | Arm::Shadow => true,
            Arm::Placebo => false,
        }
    }

    /// The arm `session_id` belongs to under `salt`.
    ///
    /// SHA-256 over a versioned canonical string rather than [`std::hash`],
    /// whose output is explicitly not stable across releases: this number
    /// decides which arm a session's whole history is compared under, and a
    /// toolchain upgrade must not move it.
    pub fn for_session(session_id: &SessionId, salt: &str, shares: ArmShares) -> Arm {
        shares.pick(bucket(&format!(
            "{ASSIGNMENT_VERSION}\narm\nsalt={salt}\nsession={session_id}\n"
        )))
    }
}

/// How much of the population each arm gets.
///
/// Weights rather than percentages so that "one session in fifty is a placebo"
/// is expressible without anybody writing `0.02` and wondering about rounding.
/// All-zero is not representable: [`Self::new`] refuses it, because a share
/// table that sums to zero has no honest answer and the tempting fallback —
/// silently assigning everything to one arm — is the failure this type exists
/// to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmShares {
    live: u32,
    shadow: u32,
    placebo: u32,
}

impl ArmShares {
    /// The shipped default: everything observes, nothing acts.
    ///
    /// This is what "default off, Shadow from day one" *is* — a deployment
    /// that installs the validator without choosing shares gets a judge that
    /// costs money and changes nothing, which is the only configuration whose
    /// risk is bounded before anybody has measured their own agent.
    pub fn shadow_only() -> Self {
        Self {
            live: 0,
            shadow: 1,
            placebo: 0,
        }
    }

    /// Weights for the three arms. `None` when they are all zero.
    pub fn new(live: u32, shadow: u32, placebo: u32) -> Option<Self> {
        if live == 0 && shadow == 0 && placebo == 0 {
            return None;
        }
        Some(Self {
            live,
            shadow,
            placebo,
        })
    }

    fn total(self) -> u64 {
        self.live as u64 + self.shadow as u64 + self.placebo as u64
    }

    /// Which arm a uniform bucket lands in.
    ///
    /// Ordered Live, Shadow, Placebo and documented as such: the order is part
    /// of the assignment, so reordering the arms here re-buckets every session
    /// exactly as changing the salt would. It is pinned by test for that
    /// reason.
    fn pick(self, bucket: u64) -> Arm {
        let point = bucket % self.total();
        if point < self.live as u64 {
            Arm::Live
        } else if point < (self.live + self.shadow) as u64 {
            Arm::Shadow
        } else {
            Arm::Placebo
        }
    }
}

/// Whether the placebo arm stages a sham intervention on this turn.
///
/// **Deterministic for the reason the arm itself is.** A placebo whose timing
/// came from a random draw would replay differently, and the arm comparison
/// would be computed against a control that no longer exists in the log. The
/// coin is therefore the same construction as the assignment: a hash, over the
/// one identifier that is unique to this turn and durable in the log.
///
/// Keyed on the [`ResponseId`] rather than on a turn index because the response
/// id is what the log already holds for this turn — a replay reaches the same
/// answer with nothing extra recorded, and two concurrent sessions cannot share
/// a coin.
///
/// `rate` is the fraction of *fired triggers* that stage an intervention, and
/// it is configuration rather than measurement: matching the placebo's
/// intervention rate to the live arm's observed rate is a calibration a
/// dashboard does across many sessions, not something one turn can know. Values
/// outside `0.0..=1.0` are clamped rather than refused — a miscalibrated
/// placebo is a weaker control, while a panic here would take down the turn the
/// checker exists not to break.
pub fn placebo_intervenes(response_id: &ResponseId, salt: &str, rate: f64) -> bool {
    let rate = rate.clamp(0.0, 1.0);
    // Resolution of one part in ten thousand: finer than any intervention rate
    // a deployment can measure, and integer arithmetic throughout so the
    // comparison cannot move with a floating-point mode.
    const SCALE: u64 = 10_000;
    let threshold = (rate * SCALE as f64).round() as u64;
    bucket(&format!(
        "{ASSIGNMENT_VERSION}\nplacebo\nsalt={salt}\nresponse={response_id}\n"
    )) % SCALE
        < threshold
}

/// A uniform 64-bit value derived from a canonical string.
fn bucket(canonical: &str) -> u64 {
    let digest = Sha256::digest(canonical.as_bytes());
    u64::from_be_bytes(digest[..8].try_into().expect("sha-256 yields 32 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thirds() -> ArmShares {
        ArmShares::new(1, 1, 1).expect("a non-zero share table")
    }

    /// The invariant every rung is gated on, at the one place a coin flip would
    /// have been the obvious implementation.
    #[test]
    fn an_arm_is_a_hash_so_a_replay_reaches_the_same_answer() {
        let session = SessionId::new("acme/ada/main");
        let first = Arm::for_session(&session, "salt-1", thirds());
        for _ in 0..8 {
            assert_eq!(
                Arm::for_session(&session, "salt-1", thirds()),
                first,
                "a session's arm has to survive being asked again, or a replay \
                 folds a history that was recorded under a different experiment"
            );
        }

        // Pinned, not merely stable: `assert_eq!(f(x), f(x))` holds just as
        // well after a change to the encoding that re-buckets every session in
        // a running experiment. This literal is what notices. When it fails,
        // `ASSIGNMENT_VERSION` moves first and this literal after.
        assert_eq!(Arm::for_session(&session, "salt-1", thirds()), Arm::Shadow);

        // The controls: the two inputs both move the answer, or the hash is
        // reading one of them and pretending to read both.
        let by_salt: Vec<Arm> = ["salt-1", "salt-2", "salt-3", "salt-4"]
            .iter()
            .map(|salt| Arm::for_session(&session, salt, thirds()))
            .collect();
        assert!(
            by_salt.iter().any(|arm| *arm != first),
            "the salt is what lets a deployment re-randomize; if it moved nothing \
             the experiment could never be re-run"
        );
        let by_session: Vec<Arm> = ["acme/ada/one", "acme/ada/two", "acme/bo/three"]
            .iter()
            .map(|id| Arm::for_session(&SessionId::new(*id), "salt-1", thirds()))
            .collect();
        assert!(by_session.iter().any(|arm| *arm != first));
    }

    #[test]
    fn shares_decide_the_split_and_an_empty_table_is_not_a_table() {
        assert_eq!(
            ArmShares::new(0, 0, 0),
            None,
            "no arm can win a table of zeroes"
        );

        // A single-arm table assigns everything to that arm, whatever the id.
        for id in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            assert_eq!(
                Arm::for_session(&SessionId::new(id), "s", ArmShares::shadow_only()),
                Arm::Shadow,
                "the shipped default costs money and changes nothing, by construction"
            );
            assert_eq!(
                Arm::for_session(
                    &SessionId::new(id),
                    "s",
                    ArmShares::new(1, 0, 0).expect("live only")
                ),
                Arm::Live
            );
        }

        // And a real split reaches every arm. Sixty ids is far past the point
        // where a 1:1:1 table missing an arm would be chance.
        let mut seen = [false; 3];
        for n in 0..60 {
            match Arm::for_session(&SessionId::new(format!("sess_{n}")), "s", thirds()) {
                Arm::Live => seen[0] = true,
                Arm::Shadow => seen[1] = true,
                Arm::Placebo => seen[2] = true,
            }
        }
        assert_eq!(seen, [true; 3]);
    }

    #[test]
    fn the_two_arm_questions_are_asked_separately() {
        // Shadow pays for a judge it throws away; Placebo throws away the judge
        // and keeps the intervention. One flag could not express that, and the
        // experiment would lose the ability to separate the cost of asking from
        // the effect of interrupting.
        assert!(Arm::Live.acts() && Arm::Live.consults_judge());
        assert!(!Arm::Shadow.acts() && Arm::Shadow.consults_judge());
        assert!(Arm::Placebo.acts() && !Arm::Placebo.consults_judge());
    }

    #[test]
    fn a_placebo_intervention_is_timed_by_hash_and_not_by_a_draw() {
        let response = ResponseId::new("resp_01J");
        let first = placebo_intervenes(&response, "salt-1", 0.5);
        for _ in 0..8 {
            assert_eq!(
                placebo_intervenes(&response, "salt-1", 0.5),
                first,
                "the control arm's timing has to replay, or the arm comparison is \
                 against a control that is not in the log"
            );
        }

        // The extremes are exact rather than probabilistic: a rate of zero must
        // never fire and a rate of one must always, or "matched spend" is a
        // rate nobody can set.
        for n in 0..64 {
            let response = ResponseId::new(format!("resp_{n}"));
            assert!(!placebo_intervenes(&response, "salt-1", 0.0));
            assert!(placebo_intervenes(&response, "salt-1", 1.0));
            // Out of range is clamped, never a panic: the checker must not be
            // able to break the checked, and a misconfigured rate is a weaker
            // control rather than a dead turn.
            assert!(!placebo_intervenes(&response, "salt-1", -3.0));
            assert!(placebo_intervenes(&response, "salt-1", 12.0));
        }

        // And a middling rate reaches both answers across turns.
        let fired = (0..200)
            .filter(|n| placebo_intervenes(&ResponseId::new(format!("resp_{n}")), "s", 0.25))
            .count();
        assert!(
            (10..90).contains(&fired),
            "a quarter of two hundred turns should fire, not none and not all; saw {fired}"
        );
    }
}
