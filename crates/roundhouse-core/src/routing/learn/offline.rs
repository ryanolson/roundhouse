// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The offline calibrator: it lists a project's learner sessions from the
//! source marks, replays their logs, writes a calibration artifact, and builds
//! the report a promotion from `shadow` to `live` is decided on (design in
//! `docs/src/concepts/routing-learner.md`).
//!
//! **Read-only against both stores.** Sessions are enumerated through
//! `SessionStore::learning_sessions`, every log is read through `read_events`,
//! and the drift check reads a learner store only through `watermark` and
//! `read`. Nothing here appends, clears, requeues, or takes a lease: a
//! calibration that wrote would change the evidence it is measuring.
//!
//! **One estimand, and one label for it.** Every weighted number is the
//! logged-boundary *conditional interval value*:
//! an interval start taken from the histories the logging policy produced,
//! the candidate followed to the next review. The report prints that label on
//! every such number and never names any other value, because an
//! interval-local weight cannot estimate the value of running a candidate for
//! a whole conversation.
//! Numbers about the `rules` route itself are labeled `factual`, and numbers
//! priced or timed from recorded plan quotes are labeled
//! [`CORRECTED_QUOTE_LABEL`].
//!
//! **The mechanics.** The evaluation unit is an accepted
//! interval with a `Positive` or `Negative` label, no failover, and learned
//! evidence of one epoch on every covered decision: exactly what credit
//! accepts, through the same screen (`session::screen`). Each turn's weight is
//! `1[candidate == served] / propensity`, with the propensity the decision
//! recorded, and a trajectory's weight is the product over its turns, so one
//! mismatch zeroes it. The estimate is self-normalized. Uncertainty comes
//! from a bootstrap that resamples whole clusters with a recorded seed.
//!
//! **The cluster unit is named, not assumed.** Today it is the session, the
//! ruled sequence key (`SessionId`, `label#g{n}`), so two generations of one
//! label are two clusters. [`ClusterUnit`] carries the name onto every number
//! that depends on it, so a later milestone that clusters by the KV-overlap
//! anchor adds a variant rather than rewriting the report.
//!
//! **Deterministic by construction.** The same manifest gives byte-identical
//! artifact and report bytes: sessions are read in byte order of their ids,
//! every map that reaches an output is ordered, the bootstrap draws from a
//! [`SplitMix64`](estimate::SplitMix64) stream seeded by the manifest, and no
//! wall-clock time enters either file. The creation time and host go in the
//! sidecar ([`sidecar_bytes`]), which the epoch never hashes.
//!
//! Submodules: [`source`] reads the stores, [`extract`] turns logs into
//! intervals, [`estimate`] holds the arithmetic, [`drift`] compares a
//! point-in-time copy of the learner store, [`write`] writes the artifact and
//! the sidecar, [`promotion`] holds the promotion tests, [`report`] renders the
//! report, and [`dump`] is the read-only in-memory source a fixture run reads.

pub mod drift;
pub mod dump;
pub mod estimate;
pub mod extract;
pub mod promotion;
pub mod report;
pub mod source;
pub mod write;

use serde::{Deserialize, Serialize};

use super::artifact::ArtifactError;
use super::{Strategy, StrategySet};
use crate::control::ProjectId;
use crate::ids::SessionId;
use crate::learn_store::LearnerError;
use crate::store::StoreError;

pub use drift::{DriftCheck, DriftResult};
pub use dump::{DumpError, DumpStore, DumpedMark, DumpedSession, LogDump};
pub use estimate::{
    Bootstrap, BootstrapPlan, CostEstimate, Estimate, Factual, Money, Outcome, SplitMix64,
    TurnTrace,
};
pub use extract::{
    Cause, Evidence, IntervalFacts, PlanQuote, SessionEntries, Stratum, StratumSpend, TurnFacts,
    replays,
};
pub use promotion::{
    CostTest, LatencyTest, PairedQualityTest, PromotionSummary, QualityTest, QuoteCensus,
    TestResult, corrected_cost, corrected_first_output, cost_gate, paired_quality,
    paired_quality_gate, quality_gate, quality_test,
};
pub use report::Report;
pub use source::{
    Calibrated, Census, CutoffEntry, ENUMERATION_PAGE, InputManifest, SessionLog, Source, assemble,
    calibrate, log_digest, read_source,
};
pub use write::{artifact_bytes, sidecar_bytes};

/// The label every weighted number carries.
pub const ESTIMAND_LABEL: &str = "conditional interval value";

/// The label every number about the served `rules` route carries.
pub const FACTUAL_LABEL: &str = "factual";

/// The label every number priced or timed from recorded plan quotes carries: a
/// prediction corrected by measured residuals, never a measurement.
pub const CORRECTED_QUOTE_LABEL: &str = "corrected quote estimate";

/// The candidate's positive-rate lower bound may be at most this far
/// below the `rules` rate. On the agreeing intervals the comparison is paired:
/// the lower bound of learned minus `rules` may be at most this far below
/// zero.
pub const QUALITY_ALLOWANCE: f64 = 0.02;

/// The candidate's estimated cost must be at least this fraction
/// below the `rules` cost.
pub const COST_REDUCTION: f64 = 0.10;

/// What a clustered number is clustered by.
///
/// **One variant today.** The owner's routing-identity ruling (PR #32) says
/// the learner will group by the KV-overlap anchor, which does not exist yet.
/// Until it does, the unit is the session, the ruled sequence key; a
/// compaction starts a new generation, and a new generation is a new cluster.
/// The anchor's variant goes beside this one, and every number labeled with
/// [`Self::label`] follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterUnit {
    /// One `SessionId`, `label#g{n}`: the ruled sequence key.
    #[default]
    Session,
}

impl ClusterUnit {
    /// The plural name every clustered number is printed with.
    pub fn label(self) -> &'static str {
        match self {
            ClusterUnit::Session => "sessions (sequence key)",
        }
    }
}

/// Whose actions a weighted estimate is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Candidate {
    /// The learner as `live` would have served each logged turn, without
    /// exploration: the recorded plans' exploit strategy, else `rules`.
    ///
    /// **The logged learner, not a frozen state.** Its gate readings are the
    /// ones each turn recorded, from the counters it read then. Re-gating every
    /// turn over counters rebuilt from this manifest would evaluate a policy on
    /// the data that trained it.
    Learned,
    /// One configured strategy, always.
    Fixed(Strategy),
}

impl Candidate {
    pub fn label(self) -> String {
        match self {
            Candidate::Learned => "learned (exploit, else rules)".to_owned(),
            Candidate::Fixed(strategy) => format!("fixed {strategy}"),
        }
    }
}

/// Where the artifact's prior units come from.
///
/// Required, with no default: a calibration that silently carried a prior, or
/// silently dropped one, would start a project from a belief nobody chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactPrior {
    /// The review credit this manifest's intervals earned, per key and
    /// strategy: review evidence carried into the artifact's new epoch. Jev
    /// counts never enter it; Jev is a prior computed on the turn, never a
    /// reward.
    Credit,
    /// No prior units, for a project that starts fresh.
    Zero,
}

/// The quality term the report states the session count against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityMinimum {
    /// The configuration key `quality.min_sessions`, under its own name: what
    /// it counts is the cluster unit, and renaming it waits on the owner.
    pub min_sessions: u64,
}

/// What one calibration run is asked to do, as the manifest writes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationConfig {
    pub project: ProjectId,
    /// The artifact's strategy list, in its order.
    pub strategies: StrategySet,
    pub prior: ArtifactPrior,
    pub quality: QualityMinimum,
    /// First output from turn start, in milliseconds.
    pub latency_limit_ms: u64,
    pub bootstrap: BootstrapPlan,
    /// Each session's cutoff, when the run is pinned to a manifest an earlier
    /// run wrote. Absent, every marked session is read to its current end, and
    /// the resolved cutoff is what [`InputManifest`] records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff: Option<Vec<CutoffEntry>>,
}

/// Why a calibration did not complete.
#[derive(Debug, thiserror::Error)]
pub enum CalibrationError {
    #[error("the session store failed: {0}")]
    Source(#[from] StoreError),
    #[error("the manifest cutoff does not match the store: {0}")]
    Cutoff(String),
    #[error("the written artifact does not parse: {0}")]
    Artifact(#[from] ArtifactError),
    #[error("the drift check's learner store failed: {0}")]
    Drift(#[from] LearnerError),
    #[error("session `{session}` is marked for project `{marked}`, not `{project}`")]
    Project {
        session: SessionId,
        marked: ProjectId,
        project: ProjectId,
    },
}
