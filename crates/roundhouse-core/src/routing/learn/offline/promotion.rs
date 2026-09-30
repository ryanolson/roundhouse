// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The promotion summary: ruling 13's tests, staged by the owner's rulings of
//! 2026-09-29.
//!
//! **Every test compares its two sides on one interval set.** A `shadow`
//! project never explores, so the learned candidate has weight only on the
//! intervals where it agreed with `rules`. Comparing that subset with the
//! `rules` figures over every interval promotes by selection alone: which
//! intervals agreed decides the result, not how the learner routes. So:
//!
//! - **Cost** prices both sides on every eligible interval from the recorded
//!   plan quotes, as the M3 corrections left them (a *corrected quote
//!   estimate*). A quote M3 could not correct is unpriced, and the test is
//!   then `not evaluable`, never priced at $0 and never dropped from one side.
//! - **Latency** reads the same plans: each turn's modeled first output from
//!   turn start, with both M3 latency terms applied. Measuring it instead
//!   would need the learned target to have served, which `shadow` never does
//!   where the learner differs, so a measured gate could never pass there.
//! - **Quality** is gated only on the intervals where the learned choice
//!   agreed with `rules`, and the report prints the share that differed. The
//!   comparison over every interval is printed too, and reads `not evaluable`
//!   wherever a side has no logging probability; the M11 rerun after the
//!   first live sessions is the binding quality test.
//!
//! **The agreeing test is paired** (the M10 review-fix ruling of
//! 2026-09-30). It reads the bootstrap lower bound of learned minus `rules`,
//! both sides summed over the same resampled clusters, against
//! `-QUALITY_ALLOWANCE`. Comparing the learned lower bound with the `rules`
//! point estimate instead asks whether a bound lies within 0.02 of its own
//! estimate, since on these intervals the two sides are one route: that is
//! a sample-size test, and it fails on noise alone (twenty sessions,
//! seventeen positive, reads 0.70 against 0.85). On an agreeing interval the
//! two candidates have the same action on every turn, so the same weight in
//! either mode: the difference is zero on every resample, and the test
//! passes once the set meets `quality.min_sessions` with support. It cannot
//! show a loss, because on these intervals none can exist.
//!
//! **`quality.min_sessions` counts sessions that carry weight** (the round-2
//! ruling of 2026-09-30): the sessions with an agreeing interval where the
//! learned candidate has weight above zero. In `shadow` that is every
//! session with an agreeing interval. In `live` an explored interval weighs
//! nothing, and counting its session would let a set the bootstrap never
//! reads meet the minimum.
//!
//! A test without support reads `not evaluable`, never `pass`.

use std::fmt::Write as _;

use super::estimate::{
    BootstrapPlan, CostEstimate, Estimate, Money, Outcome, paired_bootstrap, weighted_p50,
};
use super::extract::{Evidence, IntervalFacts, TurnFacts};
use super::{
    CORRECTED_QUOTE_LABEL, COST_REDUCTION, CalibrationConfig, Candidate, ESTIMAND_LABEL,
    QUALITY_ALLOWANCE,
};
use crate::routing::learn::{CostCorrection, CostEvidence, LatencyTerm, Strategy, TtftEvidence};
use crate::session::same_route;

/// One ruled test's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestResult {
    Pass,
    Fail,
    /// The inputs the test compares do not exist, for the reason given.
    NotEvaluable(&'static str),
}

impl TestResult {
    pub fn label(self) -> String {
        match self {
            TestResult::Pass => "pass".to_owned(),
            TestResult::Fail => "fail".to_owned(),
            TestResult::NotEvaluable(reason) => format!("not evaluable ({reason})"),
        }
    }

    pub fn passed(self) -> bool {
        self == TestResult::Pass
    }

    /// Every result passes; else any fails; else the first that could not be
    /// evaluated.
    fn all(results: &[TestResult]) -> TestResult {
        if results.iter().all(|result| result.passed()) {
            TestResult::Pass
        } else if results.contains(&TestResult::Fail) {
            TestResult::Fail
        } else {
            results
                .iter()
                .copied()
                .find(|result| !result.passed())
                .expect("not every result passed")
        }
    }
}

/// Ruling 13's quality comparison: the candidate's lower bound at most
/// [`QUALITY_ALLOWANCE`] below the `rules` rate. Inclusive.
pub fn quality_gate(lower: f64, rate: f64) -> TestResult {
    if lower >= rate - QUALITY_ALLOWANCE {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// The paired quality comparison: the lower bound of learned minus `rules`
/// at most [`QUALITY_ALLOWANCE`] below zero. Inclusive.
pub fn paired_quality_gate(lower: f64) -> TestResult {
    if lower >= -QUALITY_ALLOWANCE {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// Ruling 13's cost comparison: the candidate at least [`COST_REDUCTION`]
/// below `rules`. Inclusive.
pub fn cost_gate(candidate: f64, rules: f64) -> TestResult {
    if candidate <= rules * (1.0 - COST_REDUCTION) {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

const NO_SUPPORT: &str = "the candidate has no interval with weight above zero";

const SPARSE: &str = "more bootstrap replicates held no weight than the 2.5% tail, so the \
                      lower bound is filler";

const EMPTY: &str = "no interval in the set";

const FEW_SESSIONS: &str = "fewer sessions hold an agreeing interval with learned weight than \
                            quality.min_sessions";

const NO_LEARNED_WEIGHT: &str = "learned has no interval in the set with weight above zero";

const NO_RULES_WEIGHT: &str = "rules has no interval in the set with weight above zero";

/// The quality gate on one candidate's estimate against a `rules` rate over
/// the same intervals.
pub fn quality_test(learned: &Estimate, rules_rate: Option<f64>) -> TestResult {
    if learned.weighted == 0 {
        return TestResult::NotEvaluable(NO_SUPPORT);
    }
    if learned.bootstrap.sparse {
        return TestResult::NotEvaluable(SPARSE);
    }
    match (learned.bootstrap.lower, rules_rate) {
        (Some(lower), Some(rate)) => quality_gate(lower, rate),
        (None, _) => TestResult::NotEvaluable("no bootstrap replicates"),
        (_, None) => TestResult::NotEvaluable("rules has no interval with weight above zero"),
    }
}

/// A plan's cost as a corrected quote estimate.
///
/// **The conservative side of the correction.** M3 only ever raises a quote,
/// so `adjusted_usd` is already the bound; the `max` keeps that true for any
/// record. `NoPredictedReuse` had its samples, and M3 re-priced its quote
/// with no cached tokens (the 2026-09-30 ruling), so its `adjusted_usd` is
/// the bound too. With too few samples there is no residual to correct by,
/// and a local quote has no ledger model: both are unpriced, never $0.
pub fn corrected_cost(cost: &CostEvidence) -> Money {
    match cost.correction {
        CostCorrection::Applied | CostCorrection::NoPredictedReuse
            if cost.adjusted_usd.is_finite() && cost.quoted_usd.is_finite() =>
        {
            Money::Priced(cost.adjusted_usd.max(cost.quoted_usd))
        }
        _ => Money::Unpriced,
    }
}

/// A plan's first output from turn start as a corrected quote estimate, in
/// whole milliseconds rounded up: only when both the target's residual and
/// the project's overhead were applied.
pub fn corrected_first_output(ttft: &TtftEvidence) -> Option<u64> {
    let applied = |term: LatencyTerm| matches!(term, LatencyTerm::Applied { .. });
    (applied(ttft.residual) && applied(ttft.overhead) && ttft.adjusted_ms.is_finite())
        .then(|| ttft.adjusted_ms.max(0.0).ceil() as u64)
}

/// How the plans the cost test reads were corrected: the learned and the
/// `rules` plan of every covered turn, one count per plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QuoteCensus {
    pub applied: u64,
    pub no_predicted_reuse: u64,
    pub too_few_samples: u64,
    pub local: u64,
    /// A strategy the record did not plan.
    pub missing: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostTest {
    pub learned: CostEstimate,
    pub rules: CostEstimate,
    /// Intervals where either side has a quote that cannot be corrected.
    pub uncorrectable: usize,
    pub intervals: usize,
    pub result: TestResult,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencyTest {
    pub p50_ms: Option<u64>,
    /// Turns whose learned plan has a first-output quote M3 did not correct.
    pub uncorrectable: usize,
    pub turns: usize,
    pub result: TestResult,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityTest {
    /// Intervals the comparison is over.
    pub intervals: usize,
    /// Of those, the ones each side has logging probability above zero on.
    pub learned_supported: usize,
    pub rules_supported: usize,
    pub lower: Option<f64>,
    pub rate: Option<f64>,
    pub result: TestResult,
}

/// The paired quality test on the agreeing intervals.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PairedQualityTest {
    /// Intervals the comparison is over.
    pub intervals: usize,
    /// Clusters with an interval in the set where the learned candidate has
    /// weight above zero: what the bootstrap reads, so what
    /// `quality.min_sessions` is met on.
    pub sessions: u64,
    /// The bootstrap lower bound of learned minus `rules`, both sides summed
    /// over the same resampled clusters.
    pub lower: Option<f64>,
    pub result: TestResult,
}

/// Ruling 13's tests for the learned candidate, staged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PromotionSummary {
    pub intervals: usize,
    /// Intervals where the learned choice was the `rules` route on every turn.
    pub agreeing: usize,
    /// Gates promotion: paired quality on the agreeing intervals.
    pub quality_agreeing: PairedQualityTest,
    /// Printed, and binding only at the M11 rerun: quality over every interval.
    pub quality_full: QualityTest,
    pub cost: CostTest,
    pub latency: LatencyTest,
    pub quotes: QuoteCensus,
    /// Clusters with an eligible interval.
    pub sessions: u64,
    /// Clusters with an agreeing interval where learned has weight above
    /// zero: what the quality test's bootstrap reads, so what
    /// `quality.min_sessions` is met on.
    pub agreeing_sessions: u64,
    pub min_sessions: u64,
    pub latency_limit_ms: u64,
}

impl PromotionSummary {
    /// The owner's staged condition for `shadow` to `live`: cost, latency, and
    /// no quality loss where the learned choice agreed with `rules`.
    pub fn promotable(&self) -> bool {
        self.cost.result.passed()
            && self.latency.result.passed()
            && self.quality_agreeing.result.passed()
    }

    /// The M11 rerun's condition: cost, latency, and quality over every
    /// eligible interval.
    pub fn binding(&self) -> TestResult {
        TestResult::all(&[
            self.cost.result,
            self.latency.result,
            self.quality_full.result,
        ])
    }

    /// `learned` and `rules` are the report's estimates of the two
    /// candidates over every eligible interval, so the full quality line
    /// reads the same bootstrap the candidate blocks print.
    pub fn build(
        config: &CalibrationConfig,
        evidence: &Evidence,
        learned: &Estimate,
        rules: &Estimate,
    ) -> Self {
        let intervals = &evidence.intervals;
        let agreeing: Vec<&IntervalFacts> = intervals
            .iter()
            .filter(|interval| interval.turns.iter().all(agrees))
            .collect();
        let mut quotes = QuoteCensus::default();
        for turn in intervals.iter().flat_map(|interval| &interval.turns) {
            quotes.count(turn, turn.learned_strategy);
            if turn.learned_strategy != Strategy::Rules {
                quotes.count(turn, Strategy::Rules);
            }
        }
        let quality_agreeing =
            paired_quality(&agreeing, config.bootstrap, config.quality.min_sessions);
        PromotionSummary {
            intervals: intervals.len(),
            agreeing: agreeing.len(),
            quality_agreeing,
            quality_full: quality(intervals.len(), learned, rules),
            cost: cost(intervals),
            latency: latency(intervals, config.latency_limit_ms),
            quotes,
            sessions: clusters(&intervals.iter().collect::<Vec<_>>()),
            agreeing_sessions: quality_agreeing.sessions,
            min_sessions: config.quality.min_sessions,
            latency_limit_ms: config.latency_limit_ms,
        }
    }
}

impl QuoteCensus {
    fn count(&mut self, turn: &TurnFacts, strategy: Strategy) {
        let counter = match turn.quote(strategy).map(|quote| quote.cost.correction) {
            Some(CostCorrection::Applied) => &mut self.applied,
            Some(CostCorrection::NoPredictedReuse) => &mut self.no_predicted_reuse,
            Some(CostCorrection::TooFewSamples) => &mut self.too_few_samples,
            Some(CostCorrection::NotFrontier) => &mut self.local,
            None => &mut self.missing,
        };
        *counter += 1;
    }
}

/// The learned choice is the `rules` route on this turn.
fn agrees(turn: &TurnFacts) -> bool {
    match (
        turn.action(Candidate::Learned),
        turn.action(Candidate::Fixed(Strategy::Rules)),
    ) {
        (Some(learned), Some(rules)) => same_route(learned, rules),
        _ => false,
    }
}

fn clusters(intervals: &[&IntervalFacts]) -> u64 {
    let mut clusters: Vec<usize> = intervals.iter().map(|interval| interval.cluster).collect();
    clusters.sort_unstable();
    clusters.dedup();
    clusters.len() as u64
}

/// The learned estimate's lower bound against the `rules` estimate, both over
/// the same `intervals` eligible intervals. Each side needs logging
/// probability above zero on every one, or the comparison would be over two
/// different sets again.
fn quality(intervals: usize, learned: &Estimate, rules: &Estimate) -> QualityTest {
    let supported = learned.supported == intervals && rules.supported == intervals;
    let result = if intervals == 0 {
        TestResult::NotEvaluable(EMPTY)
    } else if learned.supported < intervals {
        TestResult::NotEvaluable(
            "the learned choice has zero logging probability on some intervals; shadow \
             never serves it where it differs from rules",
        )
    } else if rules.supported < intervals {
        TestResult::NotEvaluable(
            "rules has zero logging probability on some intervals; live never serves it \
             where the learned choice differs",
        )
    } else {
        quality_test(learned, rules.snips)
    };
    QualityTest {
        intervals,
        learned_supported: learned.supported,
        rules_supported: rules.supported,
        // Printed only when both sides cover the set: a bound over the
        // learned side's own subset next to a rate over every interval is
        // the comparison this test exists to refuse.
        lower: learned.bootstrap.lower.filter(|_| supported),
        rate: rules.snips.filter(|_| supported),
        result,
    }
}

/// The paired lower bound of learned minus `rules` over `intervals`, against
/// the allowance. Not evaluable on an empty set, below `min_sessions`
/// clusters, without learned weight, without `rules` weight, or on sparse
/// support, in that order, each with its own message.
///
/// **Computes the learned outcomes once**, and counts `sessions` from them:
/// the clusters holding an interval where the learned candidate has weight
/// above zero. The caller used to take a second pass over the same intervals
/// to count that itself and hand the count in, which could name a session
/// total the outcomes computed here do not agree with. Public so a test can
/// hand it intervals the agreeing filter never passes: on the agreeing set
/// both sides are equal, and nothing about the order of the difference, or
/// which side lacks weight, can show there.
pub fn paired_quality(
    intervals: &[&IntervalFacts],
    plan: BootstrapPlan,
    min_sessions: u64,
) -> PairedQualityTest {
    let over = |candidate| -> Vec<_> {
        intervals
            .iter()
            .map(|interval| interval.outcome(candidate))
            .collect()
    };
    let learned = over(Candidate::Learned);
    let rules = over(Candidate::Fixed(Strategy::Rules));
    let sessions = clusters(
        &intervals
            .iter()
            .zip(&learned)
            .filter(|(_, outcome)| outcome.weight > 0.0)
            .map(|(interval, _)| *interval)
            .collect::<Vec<_>>(),
    );
    let bounds = paired_bootstrap(&learned, &rules, plan);
    let weighted = |side: &[Outcome]| side.iter().any(|outcome| outcome.weight > 0.0);
    let result = if intervals.is_empty() {
        TestResult::NotEvaluable(EMPTY)
    } else if sessions < min_sessions {
        TestResult::NotEvaluable(FEW_SESSIONS)
    } else if !weighted(&learned) {
        TestResult::NotEvaluable(NO_LEARNED_WEIGHT)
    } else if !weighted(&rules) {
        TestResult::NotEvaluable(NO_RULES_WEIGHT)
    } else if bounds.sparse {
        TestResult::NotEvaluable(SPARSE)
    } else {
        bounds.lower.map_or(
            TestResult::NotEvaluable("no bootstrap replicates"),
            paired_quality_gate,
        )
    };
    PairedQualityTest {
        intervals: intervals.len(),
        sessions,
        lower: bounds.lower,
        result,
    }
}

/// One interval's corrected quote estimate for `strategy`: the sum over its
/// turns, unpriced if any turn is.
fn interval_quote(interval: &IntervalFacts, strategy: impl Fn(&TurnFacts) -> Strategy) -> Money {
    interval.turns.iter().fold(Money::Priced(0.0), |sum, turn| {
        let quote = turn
            .quote(strategy(turn))
            .map_or(Money::Unpriced, |quote| corrected_cost(&quote.cost));
        sum.plus(quote)
    })
}

fn cost(intervals: &[IntervalFacts]) -> CostTest {
    let mut sums = (0.0, 0.0);
    let mut uncorrectable = 0;
    for interval in intervals {
        match (
            interval_quote(interval, |turn| turn.learned_strategy),
            interval_quote(interval, |_| Strategy::Rules),
        ) {
            (Money::Priced(learned), Money::Priced(rules)) => {
                sums.0 += learned;
                sums.1 += rules;
            }
            _ => uncorrectable += 1,
        }
    }
    let n = intervals.len() as f64;
    let (learned, rules, result) = if intervals.is_empty() {
        (
            CostEstimate::NoSupport,
            CostEstimate::NoSupport,
            TestResult::NotEvaluable("no eligible interval"),
        )
    } else if uncorrectable > 0 {
        (
            CostEstimate::Unpriced,
            CostEstimate::Unpriced,
            TestResult::NotEvaluable(
                "a quote the M3 terms did not correct is unpriced: too few cache samples, or \
                 a local target",
            ),
        )
    } else {
        let (learned, rules) = (sums.0 / n, sums.1 / n);
        (
            CostEstimate::Priced(learned),
            CostEstimate::Priced(rules),
            cost_gate(learned, rules),
        )
    };
    CostTest {
        learned,
        rules,
        uncorrectable,
        intervals: intervals.len(),
        result,
    }
}

fn latency(intervals: &[IntervalFacts], limit_ms: u64) -> LatencyTest {
    let modeled: Vec<Option<u64>> = intervals
        .iter()
        .flat_map(|interval| &interval.turns)
        .map(|turn| {
            turn.quote(turn.learned_strategy)
                .and_then(|quote| corrected_first_output(&quote.ttft))
        })
        .collect();
    let uncorrectable = modeled.iter().filter(|ms| ms.is_none()).count();
    let p50 = weighted_p50(modeled.iter().flatten().map(|ms| (*ms, 1.0)));
    let (p50_ms, result) = match (uncorrectable, p50) {
        (1.., _) => (
            None,
            TestResult::NotEvaluable(
                "a first-output quote without both M3 latency terms applied is not an estimate",
            ),
        ),
        (0, Some(p50)) if p50 <= limit_ms => (Some(p50), TestResult::Pass),
        (0, Some(p50)) => (Some(p50), TestResult::Fail),
        (0, None) => (None, TestResult::NotEvaluable("no eligible turn")),
    };
    LatencyTest {
        p50_ms,
        uncorrectable,
        turns: modeled.len(),
        result,
    }
}

fn rate(value: Option<f64>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| format!("{value:.4}"))
}

fn usd(value: CostEstimate) -> String {
    match value {
        CostEstimate::Priced(usd) => format!("${usd:.6}"),
        CostEstimate::Unpriced => "unpriced".to_owned(),
        CostEstimate::NoSupport => "no support".to_owned(),
    }
}

impl PromotionSummary {
    /// The summary as report text, every number with its label and every
    /// clustered count with `unit`.
    pub fn render(&self, o: &mut String, unit: &str) {
        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "## Promotion summary (ruling 13, staged by the owner's rulings of 2026-09-29, for \
             the learned candidate; the owner approves each promotion)"
        );
        let differed = self.intervals - self.agreeing;
        let _ = writeln!(
            o,
            "intervals where the learned choice was the rules route on every turn: {} of {}; \
             {differed} differed",
            self.agreeing, self.intervals
        );
        let q = &self.quality_agreeing;
        let _ = writeln!(
            o,
            "1. quality, on the intervals where learned agreed with rules ({differed} of {} \
             intervals differed): paired bootstrap lower bound of the per-interval difference, \
             learned minus rules, {ESTIMAND_LABEL}, both sides over the same resampled {unit}, \
             {} on the same {} intervals, against -{QUALITY_ALLOWANCE:.2}: {}",
            self.intervals,
            rate(q.lower),
            q.intervals,
            q.result.label()
        );
        let q = &self.quality_full;
        let _ = writeln!(
            o,
            "1b. quality, over every eligible interval (binding at the M11 live rerun, not a \
             promotion gate here): learned has logging probability above zero on {} of {} \
             intervals, rules on {} of {}; bootstrap lower bound, {ESTIMAND_LABEL}, clustered \
             by {unit}, {} against rules, {ESTIMAND_LABEL}, {}: {}",
            q.learned_supported,
            q.intervals,
            q.rules_supported,
            q.intervals,
            rate(q.lower),
            rate(q.rate),
            q.result.label()
        );
        let c = &self.cost;
        let _ = writeln!(
            o,
            "2. cost: cost per interval, {CORRECTED_QUOTE_LABEL}, over every eligible interval: \
             learned {} against rules {}, at least {:.0}% lower; uncorrectable: {} of {} \
             intervals: {}",
            usd(c.learned),
            usd(c.rules),
            COST_REDUCTION * 100.0,
            c.uncorrectable,
            c.intervals,
            c.result.label()
        );
        let n = &self.quotes;
        let _ = writeln!(
            o,
            "cost quotes read, the learned and rules plans of every covered turn: {} corrected, \
             {} with no predicted reuse (priced without the cache discount), {} with too few \
             cache samples (unpriced), {} local (unpriced), {} not planned (unpriced)",
            n.applied, n.no_predicted_reuse, n.too_few_samples, n.local, n.missing
        );
        let l = &self.latency;
        let _ = writeln!(
            o,
            "3. latency: p50 first output from turn start, {CORRECTED_QUOTE_LABEL}, over every \
             turn of every eligible interval: {} against latency_limit_ms {}; uncorrectable: \
             {} of {} turns: {}",
            l.p50_ms
                .map_or_else(|| "none".to_owned(), |ms| format!("{ms} ms")),
            self.latency_limit_ms,
            l.uncorrectable,
            l.turns,
            l.result.label()
        );
        let _ = writeln!(
            o,
            "{unit} with an eligible interval: {}; {unit} with an interval where learned \
             agreed with rules and has weight above zero, the quality test's set: {}, against \
             quality.min_sessions {}: {}",
            self.sessions,
            self.agreeing_sessions,
            self.min_sessions,
            if self.agreeing_sessions >= self.min_sessions {
                "met"
            } else {
                "not met"
            }
        );
        let _ = writeln!(
            o,
            "promotion to live, staged (tests 2 and 3, and test 1 on the agreeing intervals): \
             {}; the M11 rerun after the first 20 live sessions is the binding quality test, \
             and any failure there reverts to shadow",
            if self.promotable() { "yes" } else { "no" }
        );
        let _ = writeln!(
            o,
            "M11 binding tests (2, 3 and 1b): {}",
            self.binding().label()
        );
    }
}
