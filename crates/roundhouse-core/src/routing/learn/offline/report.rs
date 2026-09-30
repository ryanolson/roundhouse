// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The calibration report: the numbers a promotion is decided on, and the
//! text they are printed as.
//!
//! **Labels are part of the number.** Every weighted figure is printed with
//! [`ESTIMAND_LABEL`], every figure about the served `rules` route with
//! [`FACTUAL_LABEL`], every figure priced or timed from recorded plan quotes
//! with `corrected quote estimate` (see [`super::promotion`]), and every
//! figure that counts or resamples clusters with the cluster unit's name. The text names no other estimand, so no reader
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
use super::promotion::PromotionSummary;
use super::source::Census;
use super::{
    ArtifactPrior, CalibrationConfig, Candidate, ClusterUnit, ESTIMAND_LABEL, FACTUAL_LABEL,
};
use crate::control::ProjectId;
use crate::metrics::TierAgreement;
use crate::routing::learn::artifact::Artifact;
use crate::routing::learn::{EpochId, Strategy, Units};

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
        let held = |candidate| {
            estimates
                .iter()
                .find(|(held, _)| *held == candidate)
                .map(|(_, estimate)| estimate)
                .expect("the learned candidate and rules, which every strategy list holds")
        };
        let promotion = PromotionSummary::build(
            config,
            evidence,
            held(Candidate::Learned),
            held(Candidate::Fixed(Strategy::Rules)),
        );
        Report {
            project: config.project.clone(),
            manifest_digest: artifact.manifest_digest().to_owned(),
            epoch: artifact.epoch(),
            artifact_sha256: artifact.sha256().to_owned(),
            unit: ClusterUnit::Session,
            census: census.clone(),
            accepted_reviews: evidence.accepted_reviews,
            eligible_intervals: evidence.intervals.len(),
            eligible_clusters: promotion.sessions,
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

fn rate(value: Option<f64>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| format!("{value:.4}"))
}

fn cost(value: CostEstimate) -> String {
    match value {
        CostEstimate::Priced(usd) => format!("${usd:.6}"),
        CostEstimate::Unpriced => "unpriced (a weighted interval has a turn with no \
                                   rate card, a local dispatch, or usage Roundhouse estimated)"
            .to_owned(),
        CostEstimate::NoSupport => "no support".to_owned(),
    }
}

fn millis(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_owned(), |ms| format!("{ms} ms"))
}

fn sampled(sampled: usize, turns: usize) -> String {
    format!("{sampled} of {turns} turns sampled")
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
            "estimand: {ESTIMAND_LABEL} (logged-boundary, interval-local weights)"
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
                 [{}, {}], {} of {} replicates held no weight{}",
                rate(estimate.bootstrap.lower),
                rate(estimate.bootstrap.upper),
                estimate.bootstrap.undefined,
                bootstrap.resamples,
                if estimate.bootstrap.sparse {
                    ", more than the lower tail, so the lower bound is filler"
                } else {
                    ""
                }
            );
            let _ = writeln!(
                o,
                "weighted intervals: {} of {}, over {} {unit}",
                estimate.weighted, estimate.intervals, estimate.weighted_clusters
            );
            let _ = writeln!(
                o,
                "effective sample size, {ESTIMAND_LABEL}: {:.2} intervals",
                estimate.effective_sample_size
            );
            let _ = writeln!(
                o,
                "support census, {ESTIMAND_LABEL}: {} of {} intervals have logging \
                 probability above zero for every candidate action; {} have zero logging \
                 probability",
                estimate.supported,
                estimate.intervals,
                estimate.intervals - estimate.supported
            );
            let _ = writeln!(
                o,
                "measured cost per interval, {ESTIMAND_LABEL}: {}",
                cost(estimate.cost)
            );
            let _ = writeln!(
                o,
                "measured p50 first output from turn start, {ESTIMAND_LABEL}: {}, {}",
                millis(estimate.p50_first_output_ms),
                sampled(estimate.sampled_turns, estimate.turns)
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
            "measured cost per interval, {FACTUAL_LABEL}: {}",
            cost(rules.cost)
        );
        let _ = writeln!(
            o,
            "p50 first output from turn start, {FACTUAL_LABEL}: {}, {}",
            millis(rules.p50_first_output_ms),
            sampled(rules.sampled_turns, rules.turns)
        );

        self.promotion.render(o, unit);
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
