// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The quality gate, `wilson-v1` (draft section 7.4, plan section 4).
//!
//! For one strategy the gate reads the most specific key level whose **live**
//! units meet `min_evidence`, adds that key's prior, and takes the Wilson
//! score bounds of the positive rate. `Pass` needs the lower bound at the
//! floor and `min_sessions` at that level; `BelowFloor` is an upper bound under
//! it; everything else, and every strategy with no evidenced level, is
//! `Unproven`.
//!
//! **A prior never opens a level.** `min_evidence` and `min_sessions` count
//! live units and live sessions only, so a prior, however large, only shifts
//! the bounds of a level that reviews already opened. Counting it toward the
//! minimums would let an artifact from another epoch, or a scout's opinion,
//! pass a strategy no review of this epoch has seen, which is the one thing
//! the Jev addendum rules out ("never passes the gate alone").
//!
//! **Zero never meets a minimum**, even a configured minimum of zero: the
//! same rule the corrections apply to their sample counts (`enough`). A level
//! with no live units is not opened, and a level no session reviewed does not
//! pass, so `min_evidence: 0` or `min_sessions: 0` cannot let a prior pass on
//! its own.
//!
//! Two priors, and the artifact's wins. The calibration artifact's units are
//! review evidence carried across epochs. Jev's tier answers are a cold-start
//! belief, worth [`JEV_PRIOR_PSEUDO_INTERVALS`] intervals, and apply only where
//! the artifact holds nothing for that key and strategy. Neither is ever a
//! reward: nothing here writes a count.

use super::enough;
use super::evidence::{GateEvidence, GateResult, JevCounts, ReadView};
use super::input::{KeyLevel, LearnedInput};
use super::{PriorUnits, QualityTerms, Strategy, Units};
use crate::routing::stage::Tier;

/// The units one interval adds to a strategy at each level.
///
/// Integer units rather than fractional credit, so the store adds integers
/// and every order of updates gives the same state. The gate divides by it to
/// read units as intervals.
pub const CREDIT_SCALE: u64 = 1_000;

/// The weight of Jev's prior, in intervals: small, so a handful of reviews
/// outweighs it (plan section 4).
pub const JEV_PRIOR_PSEUDO_INTERVALS: u64 = 3;

/// Fewer Jev answers than this on a key is no prior at all. One or two
/// answers are an anecdote, and a prior built on one would be all agree or
/// all disagree.
pub const JEV_PRIOR_MIN_ANSWERS: u64 = 3;

/// The Wilson score interval of one positive rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub lower: f64,
    pub upper: f64,
}

impl Bounds {
    /// No evidence: the whole interval.
    pub const UNKNOWN: Bounds = Bounds {
        lower: 0.0,
        upper: 1.0,
    };
}

/// Which prior entered a reading's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorSource {
    /// No prior: none held, Jev barred on this turn, or no level read.
    None,
    Artifact,
    Jev,
}

/// The gate's full reading for one strategy on one turn.
///
/// The record keeps only [`GateEvidence`], the level and the result (draft
/// section 7.7). The bounds and the prior are here so a test, and later the
/// metrics, can see why.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateReading {
    /// The level read, or `None` when no level had enough live evidence.
    pub level: Option<KeyLevel>,
    pub result: GateResult,
    /// [`Bounds::UNKNOWN`] when no level was read.
    pub bounds: Bounds,
    /// The prior units that entered the bounds.
    pub prior: Units,
    pub prior_source: PriorSource,
}

impl GateReading {
    pub fn evidence(&self) -> GateEvidence {
        GateEvidence {
            level: self.level,
            result: self.result,
        }
    }

    /// No level had enough live evidence.
    fn unread() -> Self {
        Self {
            level: None,
            result: GateResult::Unproven,
            bounds: Bounds::UNKNOWN,
            prior: Units::default(),
            prior_source: PriorSource::None,
        }
    }
}

/// `wilson-v1`: the Wilson score bounds of `units.pos / units.n`, with
/// `n = units.n / CREDIT_SCALE` intervals.
///
/// Wilson rather than the normal approximation because the interesting cases
/// are small `n` and rates near 1, where the normal interval collapses to a
/// point and would pass a strategy on five lucky reviews. `pos > n`, which only
/// a corrupt counter could hold, reads as a rate of 1 rather than above it.
pub fn wilson_v1(units: Units, z: f64) -> Bounds {
    if units.n == 0 {
        return Bounds::UNKNOWN;
    }
    let n = units.n as f64 / CREDIT_SCALE as f64;
    let p = (units.pos as f64 / units.n as f64).min(1.0);
    let z2 = z * z;
    let denominator = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denominator;
    let half = z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / denominator;
    Bounds {
        lower: (center - half).max(0.0),
        upper: (center + half).min(1.0),
    }
}

/// Jev's prior for a strategy that picks `tier` on a key with these answers:
/// [`JEV_PRIOR_PSEUDO_INTERVALS`] intervals of `n`, positive in proportion to
/// the answers that named `tier`, floored in integer arithmetic.
///
/// Zero below [`JEV_PRIOR_MIN_ANSWERS`].
pub fn jev_prior(jev: JevCounts, tier: Tier) -> Units {
    let answers = u128::from(jev.capable) + u128::from(jev.efficient);
    if answers < u128::from(JEV_PRIOR_MIN_ANSWERS) {
        return Units::default();
    }
    let agree = u128::from(match tier {
        Tier::Capable => jev.capable,
        Tier::Efficient => jev.efficient,
    });
    let n = JEV_PRIOR_PSEUDO_INTERVALS * CREDIT_SCALE;
    // `agree <= answers`, so the quotient is at most `n` and fits.
    let pos = u64::try_from(u128::from(n) * agree / answers).unwrap_or(n);
    Units { pos, n }
}

/// The tier `strategy` picks on a key whose `rules` pick is `rules_pick`.
///
/// One tier per strategy per key, because `rules_pick` is part of every key
/// level: that is what lets Jev's tier answers on a key speak to `rules` too.
fn tier_on(strategy: Strategy, rules_pick: Tier) -> Tier {
    match strategy {
        Strategy::Rules => rules_pick,
        Strategy::Efficient => Tier::Efficient,
        Strategy::Capable => Tier::Capable,
    }
}

/// The gate for `strategy` on this turn's input, over the counts the turn
/// read.
///
/// `jev_allowed` is `false` on a turn whose admitted pool has no frontier
/// target: local-only sessions never reach Jev, so they get no prior from it
/// (J4), even where the project's key holds answers from other sessions.
pub fn read_gate(
    view: &ReadView,
    input: &LearnedInput,
    strategy: Strategy,
    prior: &PriorUnits,
    quality: &QualityTerms,
    jev_allowed: bool,
) -> GateReading {
    for level in KeyLevel::ALL {
        let key = input.key(level);
        let Some(held) = view.levels.iter().find(|held| held.key == key) else {
            continue;
        };
        let Some(live) = held
            .strategies
            .iter()
            .find(|counts| counts.strategy == strategy)
        else {
            continue;
        };
        if !enough(live.n_units, quality.min_evidence) {
            continue;
        }
        let artifact = prior.get(&key, strategy);
        let jev = jev_prior(held.jev, tier_on(strategy, input.rules_pick));
        let (prior, prior_source) = if artifact.n > 0 {
            (artifact, PriorSource::Artifact)
        } else if jev_allowed && jev.n > 0 {
            (jev, PriorSource::Jev)
        } else {
            (Units::default(), PriorSource::None)
        };
        let bounds = wilson_v1(
            Units {
                pos: live.pos_units.saturating_add(prior.pos),
                n: live.n_units.saturating_add(prior.n),
            },
            quality.z,
        );
        let result = if enough(live.sessions, quality.min_sessions) && bounds.lower >= quality.floor
        {
            GateResult::Pass
        } else if bounds.upper < quality.floor {
            GateResult::BelowFloor
        } else {
            GateResult::Unproven
        };
        return GateReading {
            level: Some(level),
            result,
            bounds,
            prior,
            prior_source,
        };
    }
    GateReading::unread()
}
