// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The per-row wire columns: timing and cache-reuse evidence.
//!
//! Split out of `snapshot.rs`, which used to hold these beside the serving
//! projection `MetricsSnapshot::build` actually assembles. A reader chasing
//! what `build` does had to scroll past every basis constant and `publish`
//! rule here first; `snapshot.rs` now touches this module in the two lines
//! that call [`IntervalMetric::publish`] and [`CacheReuseEvidence::publish`]
//! (core-metrics-1).

use serde::Serialize;

use crate::metrics::cache_evidence::CacheEvidence;
use crate::metrics::timing::Elapsed;

/// What [`ModelMetrics::first_output`](super::ModelMetrics::first_output)
/// measures, published beside the number.
///
/// On the wire because the figure is unreadable without it. The interval is
/// the `TurnStarted` append stamp to the first non-empty `OutputTextDelta`
/// append stamp, attributed to the target that served; it includes any routing
/// and failover in between, and excludes work before `TurnStarted` and delivery
/// after the append. It is not the provider's service latency.
pub const FIRST_OUTPUT_BASIS: &str = "turn_start_to_first_output";

/// What the two `*_turn_elapsed` columns measure, published beside each
/// number.
///
/// The interval is the `TurnStarted` append stamp to the `ResponseCompleted` or
/// `ResponseIncomplete` append stamp, attributed to the turn's last routed
/// target. It includes routing and any failover in between, and excludes
/// admission work before `TurnStarted` and delivery after the terminal append.
///
/// It is not provider latency, not task success, and not time to solution — it
/// is how long this deployment took to finish with a turn, one way or another.
pub const TURN_ELAPSED_BASIS: &str = "turn_start_to_terminal";

/// One measured interval, wherever this snapshot publishes one: first output,
/// and each of the two `turn_elapsed` outcome classes beside it.
///
/// One type rather than three, because the three were field-for-field
/// identical structs that had grown their own `publish` rule apiece —
/// `TurnElapsed::publish`'s doc claimed it followed "`FirstOutputLatency`'s
/// rule" while that rule was written out a second time, by hand, at the
/// first-output call site. A basis that has drifted from its own doc is
/// exactly the failure mode one shared rule removes (core-metrics-2).
#[derive(Debug, Clone, Serialize)]
pub struct IntervalMetric {
    /// Mean milliseconds over [`Self::samples`].
    ///
    /// `None` when there are no samples, which is the whole point of the column
    /// being optional twice over: a row that measured nothing publishes no
    /// number rather than a zero that reads as instant.
    pub mean_ms: Option<f64>,
    pub samples: u64,
    /// Timings refused because the interval's stamp preceded its start's.
    pub rejected: u64,
    pub basis: &'static str,
}

impl IntervalMetric {
    /// One column, or `None` when nothing was measured.
    ///
    /// Published when there is either a timing or a refusal to report: a
    /// class with only refusals keeps the column and loses the mean, because
    /// dropping it would hide a clock that moved behind "not measured".
    pub(super) fn publish(elapsed: &Elapsed, basis: &'static str) -> Option<Self> {
        (elapsed.samples > 0 || elapsed.rejected > 0).then(|| Self {
            mean_ms: (elapsed.samples > 0)
                .then(|| elapsed.ms_total as f64 / elapsed.samples as f64),
            samples: elapsed.samples,
            rejected: elapsed.rejected,
            basis,
        })
    }
}

/// The denominator [`CacheReuseEvidence::predicted_mean_ratio`] is stated over.
///
/// The decision's own `isl_tokens` minus its `expected_prefill_tokens`, over
/// that same `isl_tokens` — our tokenizer's count of the prompt we were about
/// to send, including a toolbox the client re-declares every turn. It is the
/// router's belief at the moment it chose, read off the persisted
/// `DecisionRecord` and never recomputed from a live quote.
///
/// **The two numbers share that basis only because a tool-declaring turn cannot
/// go local**, and that is worth stating because it is contingent rather than
/// structural. A hosted quote is priced over the same request `isl_tokens` the
/// record carries, so a hosted row subtracts like with like. A local quote is
/// priced over the conversation buffer alone, which has no toolbox in it — and
/// the engine excludes every local candidate from a turn that declares tools,
/// so a local row's `isl_tokens` has no toolbox in it either. A later rung that
/// lets a tool-declaring turn reach a local worker would silently make the
/// whole toolbox render read as predicted cache reuse on that row.
pub const PREDICTED_CACHE_BASIS: &str = "routed_isl_minus_expected_prefill";

/// The denominator [`CacheReuseEvidence::observed_mean_ratio`] is stated over.
///
/// The provider's own `cached_input_tokens` over its own `input_tokens`, and
/// only where `Usage::cache_read_source` says the provider actually stated the
/// read. A stated zero counts; an omitted, null or unparseable field does not,
/// because both decoders fill it in with a zero that would otherwise divide as
/// a measured miss.
pub const OBSERVED_CACHE_BASIS: &str = "stated_cached_input_over_input";

/// What the router expected of a target's cache against what it got.
///
/// **An observation, not a verdict.** A gap in either direction says the
/// router's expectation and the provider's accounting disagree on this row. It
/// is not by itself evidence of cache pressure, of a routing mistake, or of
/// anything about answer quality, and nothing downstream may read it as a
/// reward.
///
/// The two means are over [`Self::samples`] — terminals where a usable
/// prediction met a *stated* cache read. The counts beside them say how much of
/// the row never got that far, which is what keeps a small sample from reading
/// as a whole population. A local row contributes counts and no samples by
/// construction: its cache credit is the router's own quote handed back, so
/// pairing it would check a number against itself.
#[derive(Debug, Clone, Serialize)]
pub struct CacheReuseEvidence {
    /// Mean predicted reuse over [`Self::samples`], on
    /// [`PREDICTED_CACHE_BASIS`].
    pub predicted_mean_ratio: Option<f64>,
    pub predicted_basis: &'static str,
    /// Mean observed reuse over the same samples, on [`OBSERVED_CACHE_BASIS`].
    pub observed_mean_ratio: Option<f64>,
    pub observed_basis: &'static str,
    /// Terminals where a usable prediction met a stated cache read.
    pub samples: u64,
    /// Mean of observed minus predicted. Negative where the router expected
    /// more reuse than the provider reported.
    ///
    /// Derived from the two totals rather than accumulated beside them, so it
    /// cannot come to disagree with the means published next to it.
    pub mean_signed_error: Option<f64>,
    /// Terminals whose decision carried a prediction that can be divided.
    pub predictions: u64,
    /// Terminals whose decision predicted nothing that can be divided.
    pub unusable_prediction: u64,
    /// Terminals where the provider stated its cache read, zero included.
    pub measured_cache_reads: u64,
    /// Terminals where nobody stated one — an omitted, null or unparseable
    /// field, or a locally derived credit.
    pub unverifiable_cache_read: u64,
    /// Terminals reporting more cached input than input.
    pub invalid_usage: u64,
    /// Terminals whose provider count could not be read at all.
    pub unusable_usage: u64,
}

impl CacheReuseEvidence {
    /// One row's column, or `None` when it observed no terminal at all.
    ///
    /// Published as soon as there is anything to report — including a row that
    /// paired nothing — on [`IntervalMetric::publish`]'s rule and for its reason:
    /// dropping the column would hide "nothing about this provider's cache is
    /// checkable" behind the same absence as "nothing ran here".
    pub(super) fn publish(evidence: &CacheEvidence) -> Option<Self> {
        (evidence.observed_terminals() > 0).then(|| {
            let mean = |total: f64| (evidence.paired > 0).then(|| total / evidence.paired as f64);
            Self {
                predicted_mean_ratio: mean(evidence.predicted_total),
                predicted_basis: PREDICTED_CACHE_BASIS,
                observed_mean_ratio: mean(evidence.observed_total),
                observed_basis: OBSERVED_CACHE_BASIS,
                samples: evidence.paired,
                mean_signed_error: mean(evidence.observed_total - evidence.predicted_total),
                predictions: evidence.predictions,
                unusable_prediction: evidence.unusable_prediction,
                measured_cache_reads: evidence.measured_cache_reads,
                unverifiable_cache_read: evidence.unverifiable_cache_read,
                invalid_usage: evidence.invalid_usage,
                unusable_usage: evidence.unusable_usage,
            }
        })
    }
}
