// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The estimator arithmetic: trajectory weights, the self-normalized
//! estimate, the support census, the effective sample size, the clustered
//! bootstrap (of one estimate, and of the paired difference of two), and the
//! weighted median.
//!
//! Pure functions over plain numbers, so every rule is checked on hand-built
//! inputs before any log reaches it.
//!
//! **A mismatch anywhere zeroes the trajectory.** The weight is a
//! product over every covered turn of `1[candidate == served] / p_log`.
//! Multiplying only over the matching turns would give a uniform A/B case 0.5
//! and 1/3 where the true value is 0.
//!
//! **Resampling does not create support.** The bootstrap measures sampling
//! variation over the clusters the manifest holds; a candidate with no
//! weighted interval has no estimate, and no replicate invents one.

use serde::{Deserialize, Serialize};

/// One covered turn, as one candidate sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnTrace {
    /// The recorded probability that the logging policy served this turn's
    /// first target.
    pub propensity: f64,
    /// The candidate's first target is the one the turn served.
    pub matched: bool,
    /// The logging policy gave the candidate's first target a probability
    /// above zero on this turn.
    pub supported: bool,
}

/// The probability that the logging policy produced the served trajectory:
/// the product of its turns' recorded propensities.
pub fn trajectory_probability(turns: &[TurnTrace]) -> f64 {
    turns.iter().map(|turn| turn.propensity).product()
}

/// The candidate's importance weight on one trajectory: `1 / P_log` when it
/// matched every turn, and exactly zero when it missed any one.
pub fn trajectory_weight(turns: &[TurnTrace]) -> f64 {
    if turns.iter().all(|turn| turn.matched) {
        1.0 / trajectory_probability(turns)
    } else {
        0.0
    }
}

/// Whether every candidate action on the trajectory had a logging probability
/// above zero: the support census counts the trajectories where this fails.
pub fn trajectory_supported(turns: &[TurnTrace]) -> bool {
    turns.iter().all(|turn| turn.supported)
}

/// A dollar figure, or the statement that it has none.
///
/// One turn's or one interval's figure. [`CostEstimate`] is the figure of an
/// estimate over many intervals, and is not this type because it has a third
/// answer this one never has: [`CostEstimate::NoSupport`], no interval to
/// average. Folding that into `Money` would let a turn claim it.
///
/// **Unpriced is never zero.** A local dispatch records no rate card, and a
/// frontier record without one predates the field; a `0.0` for either would
/// make the route look free, which is the direction the owner's cost rule
/// forbids.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Money {
    Priced(f64),
    Unpriced,
}

impl Money {
    /// The sum of two figures: unpriced if either is.
    pub fn plus(self, other: Money) -> Money {
        match (self, other) {
            (Money::Priced(a), Money::Priced(b)) => Money::Priced(a + b),
            _ => Money::Unpriced,
        }
    }
}

/// One eligible interval, as one candidate's estimate reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// Its cluster, as an index into the manifest's clusters.
    pub cluster: usize,
    /// Its covered turns.
    pub turns: usize,
    pub positive: bool,
    /// [`trajectory_weight`] of its turns for the candidate.
    pub weight: f64,
    /// [`trajectory_supported`] of its turns for the candidate.
    pub supported: bool,
    /// The candidate matched the served first target on every turn.
    pub matched: bool,
    /// The measured cost of its served turns, at their recorded rate cards.
    pub cost: Money,
    /// First output from turn start, for each covered turn that has one.
    pub first_output_ms: Vec<u64>,
}

/// How many replicates and which stream.
///
/// A manifest that asks for fewer than [`MIN_RESAMPLES`] is refused when it
/// is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "BootstrapFields")]
pub struct BootstrapPlan {
    /// The seed of the [`SplitMix64`] stream, recorded in the report.
    pub seed: u64,
    pub resamples: u32,
}

/// The fewest resamples a manifest may ask for.
///
/// **Below 40 the 2.5% tail has no place.** The lower bound is the replicate
/// at `floor(0.025 B)`, which is index 0 for any `B` under 40: the smallest
/// replicate, not a percentile, and a single undefined replicate would make
/// the bound [`Bootstrap::sparse`].
pub const MIN_RESAMPLES: u32 = 40;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapFields {
    seed: u64,
    resamples: u32,
}

impl TryFrom<BootstrapFields> for BootstrapPlan {
    type Error = String;

    fn try_from(fields: BootstrapFields) -> Result<Self, String> {
        if fields.resamples < MIN_RESAMPLES {
            return Err(format!(
                "bootstrap.resamples is {}, and must be at least {MIN_RESAMPLES}: fewer leave \
                 the 2.5% tail no place, so the lower bound would be the smallest replicate",
                fields.resamples
            ));
        }
        Ok(BootstrapPlan {
            seed: fields.seed,
            resamples: fields.resamples,
        })
    }
}

/// The two-sided level of the bootstrap interval.
pub const BOOTSTRAP_LEVEL: f64 = 0.95;

/// The name of the resampling stream, as the report prints it.
pub const BOOTSTRAP_STREAM: &str = "splitmix64";

/// A percentile interval of a self-normalized estimate, or of the paired
/// difference of two, over cluster resamples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bootstrap {
    /// `None` with no clusters or no resamples.
    pub lower: Option<f64>,
    pub upper: Option<f64>,
    /// Replicates whose resampled clusters held no weight at all (on either
    /// side, for a paired difference). Each counts as the worst the statistic
    /// can be for each bound, `0.0` and `1.0` for a rate and `-1.0` and `1.0`
    /// for a difference of two, so a sparse candidate's interval widens rather
    /// than drops the replicates that would have said so.
    pub undefined: u32,
    /// More replicates held no weight than the lower tail has places, so the
    /// lower bound is one of those fillers rather than an estimate. A test
    /// that reads the bound then has no support to read, and says `not
    /// evaluable`: comparing the filler would turn sparse support into a
    /// `fail` that no data showed.
    pub sparse: bool,
}

/// The weighted dollar figure of a candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CostEstimate {
    Priced(f64),
    /// An interval the estimate weights above zero has an unpriced turn.
    Unpriced,
    /// No interval has weight above zero.
    NoSupport,
}

/// One candidate's weighted estimate, every number of it a conditional
/// interval value.
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    pub intervals: usize,
    /// Intervals where every candidate action had a logging probability above
    /// zero.
    pub supported: usize,
    /// Intervals with weight above zero.
    pub weighted: usize,
    /// Distinct clusters with weight above zero.
    pub weighted_clusters: usize,
    /// `sum(w r) / N`.
    pub ips: f64,
    /// `sum(w r) / sum(w)`, or `None` when no interval has weight.
    pub snips: Option<f64>,
    /// `sum(w)^2 / sum(w^2)`, in intervals.
    pub effective_sample_size: f64,
    pub cost: CostEstimate,
    /// The weighted lower median of first output from turn start.
    pub p50_first_output_ms: Option<u64>,
    /// Turns of weighted intervals with a first-output sample, and all turns
    /// of weighted intervals: how much of the p50 was measured.
    pub sampled_turns: usize,
    pub turns: usize,
    pub bootstrap: Bootstrap,
}

/// One candidate's estimate over the manifest's eligible intervals.
///
/// `outcomes` are in manifest order, and every float is summed in that order.
pub fn estimate(outcomes: &[Outcome], plan: BootstrapPlan) -> Estimate {
    let total_weight: f64 = outcomes.iter().map(|outcome| outcome.weight).sum();
    let weighted_positive: f64 = outcomes
        .iter()
        .filter(|outcome| outcome.positive)
        .map(|outcome| outcome.weight)
        .sum();
    let squares: f64 = outcomes.iter().map(|outcome| outcome.weight.powi(2)).sum();
    let weighted: Vec<&Outcome> = outcomes
        .iter()
        .filter(|outcome| outcome.weight > 0.0)
        .collect();
    let mut clusters: Vec<usize> = weighted.iter().map(|outcome| outcome.cluster).collect();
    clusters.sort_unstable();
    clusters.dedup();
    Estimate {
        intervals: outcomes.len(),
        supported: outcomes.iter().filter(|outcome| outcome.supported).count(),
        weighted: weighted.len(),
        weighted_clusters: clusters.len(),
        ips: match outcomes.len() {
            0 => 0.0,
            n => weighted_positive / n as f64,
        },
        snips: (total_weight > 0.0).then(|| weighted_positive / total_weight),
        effective_sample_size: if squares > 0.0 {
            total_weight.powi(2) / squares
        } else {
            0.0
        },
        cost: weighted_cost(&weighted),
        p50_first_output_ms: weighted_p50(weighted.iter().flat_map(|outcome| {
            outcome
                .first_output_ms
                .iter()
                .map(|ms| (*ms, outcome.weight))
        })),
        sampled_turns: weighted
            .iter()
            .map(|outcome| outcome.first_output_ms.len())
            .sum(),
        turns: weighted.iter().map(|outcome| outcome.turns).sum(),
        bootstrap: bootstrap(outcomes, plan),
    }
}

fn weighted_cost(weighted: &[&Outcome]) -> CostEstimate {
    if weighted.is_empty() {
        return CostEstimate::NoSupport;
    }
    let mut sum = 0.0;
    let mut weights = 0.0;
    for outcome in weighted {
        match outcome.cost {
            Money::Priced(cost) => sum += outcome.weight * cost,
            Money::Unpriced => return CostEstimate::Unpriced,
        }
        weights += outcome.weight;
    }
    CostEstimate::Priced(sum / weights)
}

/// The unweighted figures of the intervals a candidate served, which for
/// `rules` in `shadow` is every eligible interval.
#[derive(Debug, Clone, PartialEq)]
pub struct Factual {
    pub intervals: usize,
    pub clusters: usize,
    pub positive_rate: Option<f64>,
    pub cost: CostEstimate,
    pub p50_first_output_ms: Option<u64>,
    /// Served turns with a first-output sample, and all served turns.
    pub sampled_turns: usize,
    pub turns: usize,
}

/// The factual figures over the outcomes the candidate matched on every turn,
/// each counted once.
pub fn factual(outcomes: &[Outcome]) -> Factual {
    let served: Vec<&Outcome> = outcomes.iter().filter(|outcome| outcome.matched).collect();
    let mut clusters: Vec<usize> = served.iter().map(|outcome| outcome.cluster).collect();
    clusters.sort_unstable();
    clusters.dedup();
    let cost = match served.is_empty() {
        true => CostEstimate::NoSupport,
        false => served
            .iter()
            .try_fold(0.0, |sum, outcome| match outcome.cost {
                Money::Priced(cost) => Some(sum + cost),
                Money::Unpriced => None,
            })
            .map_or(CostEstimate::Unpriced, |sum| {
                CostEstimate::Priced(sum / served.len() as f64)
            }),
    };
    Factual {
        intervals: served.len(),
        clusters: clusters.len(),
        positive_rate: (!served.is_empty()).then(|| {
            served.iter().filter(|outcome| outcome.positive).count() as f64 / served.len() as f64
        }),
        cost,
        p50_first_output_ms: weighted_p50(
            served
                .iter()
                .flat_map(|outcome| outcome.first_output_ms.iter().map(|ms| (*ms, 1.0))),
        ),
        sampled_turns: served
            .iter()
            .map(|outcome| outcome.first_output_ms.len())
            .sum(),
        turns: served.iter().map(|outcome| outcome.turns).sum(),
    }
}

/// The weighted lower median: the smallest value whose cumulative weight
/// reaches half the total. `None` without a sample of positive weight.
pub fn weighted_p50(samples: impl IntoIterator<Item = (u64, f64)>) -> Option<u64> {
    let mut samples: Vec<(u64, f64)> = samples
        .into_iter()
        .filter(|(_, weight)| *weight > 0.0)
        .collect();
    samples.sort_by_key(|(value, _)| *value);
    let total: f64 = samples.iter().map(|(_, weight)| weight).sum();
    let mut reached = 0.0;
    for (value, weight) in &samples {
        reached += weight;
        if reached >= total / 2.0 {
            return Some(*value);
        }
    }
    samples.last().map(|(value, _)| *value)
}

/// Each of `plan.resamples` resamples of whole clusters, in draw order: per
/// side, the drawn clusters' `(sum of w r, sum of w)`.
///
/// Each replicate draws as many clusters as the outcomes hold, with
/// replacement, and every interval of a drawn cluster comes with it:
/// intervals of one session are correlated, so the cluster is the unit. The
/// clusters are indexed in ascending id order and drawn from one
/// [`SplitMix64`] stream seeded by `plan.seed`.
///
/// **Every side rides the same draw.** A paired statistic is paired only if
/// both of its sides are summed over the same resampled clusters; drawing
/// each side on its own would add back the between-side noise the pairing
/// exists to cancel. With one side, the stream and the summation order are
/// the ones a recorded seed was reported with.
fn resample<const N: usize>(sides: [&[Outcome]; N], plan: BootstrapPlan) -> Vec<[(f64, f64); N]> {
    let mut ids: Vec<usize> = sides
        .iter()
        .flat_map(|side| side.iter().map(|outcome| outcome.cluster))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Vec::new();
    }
    // Per cluster, in id order, per side: (sum of w r, sum of w).
    let mut sums = vec![[(0.0f64, 0.0f64); N]; ids.len()];
    for (side, outcomes) in sides.iter().enumerate() {
        for outcome in *outcomes {
            let at = ids
                .binary_search(&outcome.cluster)
                .expect("every outcome's cluster is in the id list");
            if outcome.positive {
                sums[at][side].0 += outcome.weight;
            }
            sums[at][side].1 += outcome.weight;
        }
    }
    let mut stream = SplitMix64::new(plan.seed);
    (0..plan.resamples)
        .map(|_| {
            let mut drawn = [(0.0, 0.0); N];
            for _ in 0..ids.len() {
                let cluster = &sums[stream.below(ids.len())];
                for (drawn, (p, t)) in drawn.iter_mut().zip(cluster) {
                    drawn.0 += p;
                    drawn.1 += t;
                }
            }
            drawn
        })
        .collect()
}

/// The self-normalized estimate of each of `plan.resamples` resamples of
/// whole clusters (see [`resample`]), in draw order; `None` for a replicate
/// whose drawn clusters hold no weight.
pub fn bootstrap_replicates(outcomes: &[Outcome], plan: BootstrapPlan) -> Vec<Option<f64>> {
    resample([outcomes], plan)
        .into_iter()
        .map(|[(positive, total)]| (total > 0.0).then(|| positive / total))
        .collect()
}

/// The paired difference of two candidates' self-normalized estimates,
/// `learned - rules`, on each of `plan.resamples` resamples of whole
/// clusters, both sides on the same draw (see [`resample`]); `None` for a
/// replicate where either side's drawn clusters hold no weight.
///
/// `learned` and `rules` are two candidates' outcomes on one interval list,
/// in the same order.
pub fn paired_replicates(
    learned: &[Outcome],
    rules: &[Outcome],
    plan: BootstrapPlan,
) -> Vec<Option<f64>> {
    assert!(
        learned.len() == rules.len()
            && learned
                .iter()
                .zip(rules)
                .all(|(learned, rules)| learned.cluster == rules.cluster),
        "a paired statistic needs both sides on one interval list"
    );
    resample([learned, rules], plan)
        .into_iter()
        .map(|[(lp, lt), (rp, rt)]| (lt > 0.0 && rt > 0.0).then(|| lp / lt - rp / rt))
        .collect()
}

/// The percentile interval of [`bootstrap_replicates`], an undefined
/// replicate filled with `0.0` and `1.0` (see [`percentile`]).
pub fn bootstrap(outcomes: &[Outcome], plan: BootstrapPlan) -> Bootstrap {
    percentile(&bootstrap_replicates(outcomes, plan), 0.0, 1.0)
}

/// The percentile interval of [`paired_replicates`], an undefined replicate
/// filled with `-1.0` and `1.0`: the worst each bound of a difference of two
/// rates can be.
pub fn paired_bootstrap(learned: &[Outcome], rules: &[Outcome], plan: BootstrapPlan) -> Bootstrap {
    percentile(&paired_replicates(learned, rules, plan), -1.0, 1.0)
}

/// The percentile interval of `replicates`.
///
/// The lower bound is the replicate at index `floor(0.025 B)` in ascending
/// order, and the upper the one at `ceil(0.975 B) - 1`. An undefined
/// replicate counts as `low_fill` for the lower bound and `high_fill` for
/// the upper, never as the point estimate: that would narrow the interval by
/// exactly the replicates that say the data is thin. When those fillers
/// reach the lower index, the bound is [`Bootstrap::sparse`].
fn percentile(replicates: &[Option<f64>], low_fill: f64, high_fill: f64) -> Bootstrap {
    if replicates.is_empty() {
        return Bootstrap {
            lower: None,
            upper: None,
            undefined: 0,
            sparse: false,
        };
    }
    let undefined = replicates.iter().filter(|value| value.is_none()).count() as u32;
    let mut lows: Vec<f64> = replicates
        .iter()
        .map(|value| value.unwrap_or(low_fill))
        .collect();
    let mut highs: Vec<f64> = replicates
        .iter()
        .map(|value| value.unwrap_or(high_fill))
        .collect();
    lows.sort_by(f64::total_cmp);
    highs.sort_by(f64::total_cmp);
    let count = replicates.len();
    let tail = (1.0 - BOOTSTRAP_LEVEL) / 2.0;
    let low_at = ((tail * count as f64).floor() as usize).min(count - 1);
    let high_at = (((1.0 - tail) * count as f64).ceil() as usize)
        .saturating_sub(1)
        .min(count - 1);
    Bootstrap {
        lower: Some(lows[low_at]),
        upper: Some(highs[high_at]),
        undefined,
        sparse: undefined as usize > low_at,
    }
}

/// The SplitMix64 stream (Steele, Lea and Flood, 2014).
///
/// Written out rather than taken from a crate: a library generator may change
/// its stream between versions, and the report promises that a recorded seed
/// reproduces the same interval on any later build.
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// An index below `n`, by the high half of a 128-bit product, which has no
    /// modulo bias worth the name at cluster counts.
    pub fn below(&mut self, n: usize) -> usize {
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sparse` starts one past the lower index: with 200 replicates the
    /// lower bound is the sixth smallest (index 5), so five fillers leave it
    /// an estimate and a sixth makes it filler.
    #[test]
    fn sparse_starts_one_past_the_lower_index() {
        let with = |undefined: usize| {
            let replicates: Vec<Option<f64>> = (0..200)
                .map(|at| (at >= undefined).then_some(0.5))
                .collect();
            percentile(&replicates, 0.0, 1.0)
        };
        let low_at = 5;
        assert!(!with(low_at).sparse);
        assert_eq!(with(low_at).lower, Some(0.5));
        assert!(with(low_at + 1).sparse);
        assert_eq!(with(low_at + 1).lower, Some(0.0));
    }
}
