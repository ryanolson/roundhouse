// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The calibration report: the numbers a promotion is decided on, and the
//! text they are printed as.
//!
//! **Labels are part of the number.** Every weighted figure is printed with
//! [`ESTIMAND_LABEL`], every figure about the served `rules` route with
//! [`FACTUAL_LABEL`], and every figure that counts or resamples clusters with
//! the cluster unit's name. The text names no other estimand, so no reader
//! can take an interval-local weight for the value of running a candidate end
//! to end.
//!
//! **Unpriced is printed as unpriced.** A cost the log cannot price is never
//! printed as a dollar figure, and a promotion test that needs it says it
//! could not be evaluated.
//!
//! The rendering is a pure function of the [`Report`], which holds no clock
//! and no hash-ordered collection, so one manifest gives one text.

use std::fmt::Write as _;

use super::drift::DriftCheck;
use super::estimate::{
    BOOTSTRAP_LEVEL, BOOTSTRAP_STREAM, BootstrapPlan, CostEstimate, Estimate, Factual, estimate,
    factual,
};
use super::extract::{Cause, Evidence, Stratum, StratumSpend};
use super::source::Census;
use super::{
    ArtifactPrior, COST_REDUCTION, CalibrationConfig, Candidate, ClusterUnit, ESTIMAND_LABEL,
    FACTUAL_LABEL, QUALITY_ALLOWANCE,
};
use crate::control::ProjectId;
use crate::metrics::TierAgreement;
use crate::routing::learn::artifact::Artifact;
use crate::routing::learn::{EpochId, Strategy, Units};

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
}

/// Ruling 13's three tests for the learned candidate, and the session count.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PromotionSummary {
    /// The candidate's bootstrap lower bound at least the `rules` factual rate
    /// less [`QUALITY_ALLOWANCE`].
    pub quality: TestResult,
    /// The candidate's estimated cost at least [`COST_REDUCTION`] below the
    /// `rules` factual cost.
    pub cost: TestResult,
    /// The candidate's p50 first output from turn start within the limit.
    pub latency: TestResult,
    /// Clusters with at least one eligible interval.
    pub sessions: u64,
    pub min_sessions: u64,
    pub latency_limit_ms: u64,
}

impl PromotionSummary {
    pub fn all_pass(&self) -> bool {
        self.quality.passed() && self.cost.passed() && self.latency.passed()
    }
}

/// Everything the report prints.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub project: ProjectId,
    pub manifest_digest: String,
    pub epoch: EpochId,
    pub artifact_sha256: String,
    pub unit: ClusterUnit,
    pub census: Census,
    pub accepted_reviews: u64,
    pub eligible_intervals: usize,
    /// Clusters with at least one eligible interval.
    pub eligible_clusters: u64,
    /// Every cause, in [`Cause::ALL`] order, zeros included.
    pub exclusions: Vec<(Cause, u64)>,
    pub prior: ArtifactPrior,
    pub prior_entries: usize,
    pub dropped_prior: Vec<(Strategy, Units)>,
    pub bootstrap: BootstrapPlan,
    pub candidates: Vec<(Candidate, Estimate)>,
    pub rules: Factual,
    pub promotion: PromotionSummary,
    pub spend: Vec<(Stratum, StratumSpend)>,
    pub agreement: TierAgreement,
    pub drift: DriftCheck,
}

impl Report {
    pub fn build(
        config: &CalibrationConfig,
        census: &Census,
        evidence: &Evidence,
        drift: DriftCheck,
        artifact: &Artifact,
    ) -> Self {
        let candidates: Vec<Candidate> = std::iter::once(Candidate::Learned)
            .chain(
                config
                    .strategies
                    .as_slice()
                    .iter()
                    .map(|strategy| Candidate::Fixed(*strategy)),
            )
            .collect();
        let estimates: Vec<(Candidate, Estimate)> = candidates
            .iter()
            .map(|candidate| {
                let outcomes: Vec<_> = evidence
                    .intervals
                    .iter()
                    .map(|interval| interval.outcome(*candidate))
                    .collect();
                (*candidate, estimate(&outcomes, config.bootstrap))
            })
            .collect();
        let rules_outcomes: Vec<_> = evidence
            .intervals
            .iter()
            .map(|interval| interval.outcome(Candidate::Fixed(Strategy::Rules)))
            .collect();
        let rules = factual(&rules_outcomes);
        let mut clusters: Vec<usize> = evidence.intervals.iter().map(|i| i.cluster).collect();
        clusters.sort_unstable();
        clusters.dedup();
        let learned = &estimates[0].1;
        let promotion = PromotionSummary {
            quality: quality_test(learned, &rules),
            cost: cost_test(learned, &rules),
            latency: match learned.p50_first_output_ms {
                _ if learned.weighted == 0 => TestResult::NotEvaluable(NO_SUPPORT),
                Some(p50) if p50 <= config.latency_limit_ms => TestResult::Pass,
                Some(_) => TestResult::Fail,
                None => TestResult::NotEvaluable("no first-output sample has weight"),
            },
            sessions: clusters.len() as u64,
            min_sessions: config.quality.min_sessions,
            latency_limit_ms: config.latency_limit_ms,
        };
        Report {
            project: config.project.clone(),
            manifest_digest: artifact.manifest_digest().to_owned(),
            epoch: artifact.epoch(),
            artifact_sha256: artifact.sha256().to_owned(),
            unit: ClusterUnit::Session,
            census: census.clone(),
            accepted_reviews: evidence.accepted_reviews,
            eligible_intervals: evidence.intervals.len(),
            eligible_clusters: clusters.len() as u64,
            exclusions: Cause::ALL
                .iter()
                .map(|cause| (*cause, evidence.exclusions.get(cause).copied().unwrap_or(0)))
                .collect(),
            prior: config.prior,
            prior_entries: evidence.prior.len(),
            dropped_prior: evidence
                .dropped_prior
                .iter()
                .map(|(strategy, units)| (*strategy, *units))
                .collect(),
            bootstrap: config.bootstrap,
            candidates: estimates,
            rules,
            promotion,
            spend: evidence
                .spend
                .iter()
                .map(|(stratum, spend)| (*stratum, *spend))
                .collect(),
            agreement: evidence.agreement,
            drift,
        }
    }

    /// The estimate of `candidate`, when the report holds one.
    pub fn estimate(&self, candidate: Candidate) -> Option<&Estimate> {
        self.candidates
            .iter()
            .find(|(held, _)| *held == candidate)
            .map(|(_, estimate)| estimate)
    }
}

const NO_SUPPORT: &str = "the candidate has no interval with weight above zero; \
                          where no turn explored, a candidate that differs from the \
                          served route has zero weight";

fn quality_test(learned: &Estimate, rules: &Factual) -> TestResult {
    if learned.weighted == 0 {
        return TestResult::NotEvaluable(NO_SUPPORT);
    }
    match (learned.bootstrap.lower, rules.positive_rate) {
        (Some(lower), Some(rate)) if lower >= rate - QUALITY_ALLOWANCE => TestResult::Pass,
        (Some(_), Some(_)) => TestResult::Fail,
        (None, _) => TestResult::NotEvaluable("no bootstrap replicates"),
        (_, None) => TestResult::NotEvaluable("rules served no eligible interval"),
    }
}

fn cost_test(learned: &Estimate, rules: &Factual) -> TestResult {
    match (learned.cost, rules.cost) {
        (CostEstimate::Priced(candidate), CostEstimate::Priced(rules)) => {
            if candidate <= rules * (1.0 - COST_REDUCTION) {
                TestResult::Pass
            } else {
                TestResult::Fail
            }
        }
        (CostEstimate::NoSupport, _) => TestResult::NotEvaluable(NO_SUPPORT),
        (CostEstimate::Unpriced, _) | (_, CostEstimate::Unpriced) => TestResult::NotEvaluable(
            "a weighted interval has an unpriced turn: a local dispatch records no rate card",
        ),
        (_, CostEstimate::NoSupport) => {
            TestResult::NotEvaluable("rules served no eligible interval")
        }
    }
}

fn rate(value: Option<f64>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| format!("{value:.4}"))
}

fn cost(value: CostEstimate) -> String {
    match value {
        CostEstimate::Priced(usd) => format!("${usd:.6}"),
        CostEstimate::Unpriced => "unpriced (a weighted interval has a turn \
                                   with no recorded rate card)"
            .to_owned(),
        CostEstimate::NoSupport => "no support".to_owned(),
    }
}

fn millis(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_owned(), |ms| format!("{ms} ms"))
}

impl Report {
    /// The report as text. Deterministic for one [`Report`].
    pub fn render(&self) -> String {
        let unit = self.unit.label();
        let mut out = String::new();
        let o = &mut out;
        let _ = writeln!(o, "# Learner calibration report");
        let _ = writeln!(o);
        let _ = writeln!(o, "project: {}", self.project);
        let _ = writeln!(o, "manifest digest: {}", self.manifest_digest);
        let _ = writeln!(o, "artifact epoch: {}", self.epoch);
        let _ = writeln!(o, "artifact sha256: {}", self.artifact_sha256);
        let _ = writeln!(
            o,
            "estimand: {ESTIMAND_LABEL} (logged-boundary, interval-local weights, ruling 12)"
        );
        let _ = writeln!(o, "cluster unit: {unit}");

        let _ = writeln!(o);
        let _ = writeln!(o, "## Source");
        let census = &self.census;
        let _ = writeln!(
            o,
            "marked {unit} of this project, from the source marks: {}",
            census.project_sessions
        );
        let _ = writeln!(
            o,
            "marked {unit} of other projects, not read: {}",
            census.other_projects
        );
        let _ = writeln!(
            o,
            "excluded, unreadable index mark: {} {unit}{}",
            census.unreadable.len(),
            names(&census.unreadable)
        );
        let _ = writeln!(
            o,
            "excluded, marked after the manifest cutoff: {} {unit}{}",
            census.after_cutoff.len(),
            names(&census.after_cutoff)
        );

        let _ = writeln!(o);
        let _ = writeln!(o, "## Intervals");
        let _ = writeln!(o, "accepted reviews: {}", self.accepted_reviews);
        let _ = writeln!(
            o,
            "eligible intervals: {} over {} {unit}",
            self.eligible_intervals, self.eligible_clusters
        );
        let _ = writeln!(o, "excluded intervals, by cause:");
        for (cause, count) in &self.exclusions {
            let _ = writeln!(o, "- {}: {count}", cause.label());
        }

        let _ = writeln!(o);
        let _ = writeln!(o, "## Artifact prior");
        let _ = match self.prior {
            ArtifactPrior::Credit => writeln!(
                o,
                "prior: review credit from this manifest, {} key and strategy entries",
                self.prior_entries
            ),
            ArtifactPrior::Zero => writeln!(o, "prior: zero units"),
        };
        for (strategy, units) in &self.dropped_prior {
            let _ = writeln!(
                o,
                "dropped, {strategy} is not in the artifact's strategy list: pos {} of n {}",
                units.pos, units.n
            );
        }

        let _ = writeln!(o);
        let _ = writeln!(o, "## Candidates");
        let level = BOOTSTRAP_LEVEL * 100.0;
        let bootstrap = self.bootstrap;
        let _ = writeln!(
            o,
            "bootstrap: {} resamples of {unit}, stream {BOOTSTRAP_STREAM}, seed {}",
            bootstrap.resamples, bootstrap.seed
        );
        for (candidate, estimate) in &self.candidates {
            let _ = writeln!(o);
            let _ = writeln!(o, "### {}", candidate.label());
            let _ = writeln!(
                o,
                "positive rate, {ESTIMAND_LABEL} (self-normalized): {}",
                rate(estimate.snips)
            );
            let _ = writeln!(
                o,
                "positive rate, {ESTIMAND_LABEL} (unnormalized): {:.4}",
                estimate.ips
            );
            let _ = writeln!(
                o,
                "{level:.0}% bootstrap interval, {ESTIMAND_LABEL}, clustered by {unit}: \
                 [{}, {}], {} of {} replicates held no weight",
                rate(estimate.bootstrap.lower),
                rate(estimate.bootstrap.upper),
                estimate.bootstrap.undefined,
                bootstrap.resamples
            );
            let _ = writeln!(
                o,
                "weighted intervals: {} of {}, over {} {unit}",
                estimate.weighted, estimate.intervals, estimate.weighted_clusters
            );
            let _ = writeln!(
                o,
                "effective sample size: {:.2} intervals",
                estimate.effective_sample_size
            );
            let _ = writeln!(
                o,
                "support census: {} of {} intervals have logging probability above zero \
                 for every candidate action; {} have zero logging probability",
                estimate.supported,
                estimate.intervals,
                estimate.intervals - estimate.supported
            );
            let _ = writeln!(
                o,
                "cost per interval, {ESTIMAND_LABEL}: {}",
                cost(estimate.cost)
            );
            let _ = writeln!(
                o,
                "p50 first output from turn start, {ESTIMAND_LABEL}: {}",
                millis(estimate.p50_first_output_ms)
            );
        }

        let _ = writeln!(o);
        let _ = writeln!(o, "## rules, {FACTUAL_LABEL}");
        let rules = &self.rules;
        let _ = writeln!(
            o,
            "intervals rules served: {} over {} {unit}",
            rules.intervals, rules.clusters
        );
        let _ = writeln!(
            o,
            "positive rate, {FACTUAL_LABEL}: {}",
            rate(rules.positive_rate)
        );
        let _ = writeln!(
            o,
            "cost per interval, {FACTUAL_LABEL}: {}",
            cost(rules.cost)
        );
        let _ = writeln!(
            o,
            "p50 first output from turn start, {FACTUAL_LABEL}: {}",
            millis(rules.p50_first_output_ms)
        );

        self.render_promotion(o);
        self.render_spend(o);
        self.render_agreement(o);

        let _ = writeln!(o);
        let _ = writeln!(o, "## Drift");
        match &self.drift {
            DriftCheck::NotRun => {
                let _ = writeln!(
                    o,
                    "drift check not run: no point-in-time copy of the learner store was named"
                );
            }
            DriftCheck::Ran(result) => {
                let _ = writeln!(
                    o,
                    "drift check ran against a point-in-time copy: {} {unit}, {} counters \
                     compared, {} differ",
                    result.sessions,
                    result.compared,
                    result.differences.len()
                );
                for difference in &result.differences {
                    let _ = writeln!(o, "- {difference}");
                }
                if !result.beyond_cutoff.is_empty() {
                    let _ = writeln!(
                        o,
                        "copy watermark above the manifest cutoff: {} {unit}{}",
                        result.beyond_cutoff.len(),
                        names(&result.beyond_cutoff)
                    );
                }
            }
        }
        out
    }

    fn render_promotion(&self, o: &mut String) {
        let unit = self.unit.label();
        let promotion = &self.promotion;
        let learned = self.estimate(Candidate::Learned);
        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "## Promotion summary (ruling 13, for the learned candidate; the owner approves \
             each promotion)"
        );
        let _ = writeln!(
            o,
            "1. quality: bootstrap lower bound, {ESTIMAND_LABEL}, {} against the rules \
             {FACTUAL_LABEL} positive rate {} less {QUALITY_ALLOWANCE:.2}: {}",
            rate(learned.and_then(|estimate| estimate.bootstrap.lower)),
            rate(self.rules.positive_rate),
            promotion.quality.label()
        );
        let _ = writeln!(
            o,
            "2. cost: cost per interval, {ESTIMAND_LABEL}, {} against the rules \
             {FACTUAL_LABEL} {}, at least {:.0}% lower: {}",
            learned.map_or_else(|| "none".to_owned(), |estimate| cost(estimate.cost)),
            cost(self.rules.cost),
            COST_REDUCTION * 100.0,
            promotion.cost.label()
        );
        let _ = writeln!(
            o,
            "3. latency: p50 first output from turn start, {ESTIMAND_LABEL}, {} against \
             latency_limit_ms {}: {}",
            millis(learned.and_then(|estimate| estimate.p50_first_output_ms)),
            promotion.latency_limit_ms,
            promotion.latency.label()
        );
        let _ = writeln!(
            o,
            "{unit} with an eligible interval: {} against quality.min_sessions {}: {}",
            promotion.sessions,
            promotion.min_sessions,
            if promotion.sessions >= promotion.min_sessions {
                "met"
            } else {
                "not met"
            }
        );
        let _ = writeln!(
            o,
            "all three ruled tests pass: {}",
            if promotion.all_pass() { "yes" } else { "no" }
        );
    }

    fn render_spend(&self, o: &mut String) {
        let _ = writeln!(o);
        let _ = writeln!(o, "## Judge and classifier spend by strategy stratum");
        let _ = writeln!(
            o,
            "Judge side calls record no rate card, so judge dollars are unpriced. No claim is \
             made that spend is equal across strata."
        );
        if self.spend.is_empty() {
            let _ = writeln!(o, "no judge or classifier calls");
        }
        for (stratum, spend) in &self.spend {
            let _ = writeln!(
                o,
                "- {}: judge {} calls ({} abandoned), {} input and {} output tokens, dollars \
                 unpriced; classifier {} measured calls ${:.6}, {} calls with unknown usage \
                 estimated at ${:.6}",
                stratum.label(),
                spend.judge_calls,
                spend.judge_abandoned,
                spend.judge_input_tokens,
                spend.judge_output_tokens,
                spend.classifier_measured_calls,
                spend.classifier_measured_usd,
                spend.classifier_unknown_calls,
                spend.classifier_estimated_usd
            );
        }
    }

    fn render_agreement(&self, o: &mut String) {
        let agreement = &self.agreement;
        let disagreements = &agreement.disagreements;
        let _ = writeln!(o);
        let _ = writeln!(o, "## Jev agreement (a comparison, never a reward)");
        let _ = writeln!(
            o,
            "answered {}, agree {}, disagree {}, not comparable {}",
            agreement.answered, agreement.agree, agreement.disagree, agreement.not_comparable
        );
        let _ = writeln!(
            o,
            "disagreements: jev capable served efficient {}, jev efficient served capable {}; \
             covering review positive {}, negative {}, unknown {}, unlabeled {} ({} evicted)",
            disagreements.jev_capable_served_efficient,
            disagreements.jev_efficient_served_capable,
            disagreements.positive,
            disagreements.negative,
            disagreements.unknown,
            disagreements.unlabeled,
            disagreements.evicted
        );
    }
}

fn names(sessions: &[crate::ids::SessionId]) -> String {
    if sessions.is_empty() {
        return String::new();
    }
    let listed: Vec<&str> = sessions.iter().map(|session| session.as_str()).collect();
    format!(" ({})", listed.join(", "))
}
