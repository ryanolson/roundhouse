// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The corrections a learned decision applies to a candidate's quote before it
//! compares strategy plans: the cache reuse correction on cost, the latency
//! model of the 2026-09-28 ruling 10, and the grant check on the corrected
//! cost.
//!
//! Pure functions of the quote, the store view and the ledger's rate card.
//! Each one records whether it applied and, when it did not, why, so a record
//! never shows an unadjusted number that reads as a measured one.
//!
//! **Approximate, and never cheaper than the quote.** The owner's rule of
//! 2026-09-28 is that a cost estimate can be rough but must not be biased in
//! the direction that makes a route look cheaper than it is. The reuse
//! correction therefore only ever moves predicted-cached tokens back to
//! uncached: a target that reused more than predicted keeps the ledger's own
//! quote. See [`adjusted_cached_tokens`]. The re-priced difference is also
//! clamped at zero, so the rule holds whatever the rate card says; see
//! [`Corrections::cost`].
//!
//! **Predictions, not measurements.** A corrected quote does not establish a
//! measured cost reduction; the offline report measures serving cost from the
//! log. Judge and classifier charges are not in this cost at all, because they
//! are not known before selection.

use super::evidence::{
    CacheReuse, CostCorrection, CostEvidence, GrantCheck, LatencySum, LatencyTerm, ReadView,
    TargetOps, TtftEvidence,
};
use crate::control::{BudgetState, TurnBudget};
use crate::routing::{CacheLedger, Candidate};

/// One turn's corrections, over the store view that turn read.
///
/// Built once per turn, and that is where the overhead term is computed: the
/// overhead before dispatch is the project's, so it is one term for the turn,
/// never a per-target sum and never scaled by how many targets the view holds.
pub struct Corrections<'a> {
    view: &'a ReadView,
    ledger: &'a CacheLedger,
    isl_tokens: usize,
    latency_min_samples: u64,
    cache_min_samples: u64,
    overhead: LatencyTerm,
}

impl<'a> Corrections<'a> {
    /// `ledger` supplies the rate card of each frontier target, the same one
    /// its quote was priced with.
    pub fn new(
        view: &'a ReadView,
        ledger: &'a CacheLedger,
        isl_tokens: usize,
        latency_min_samples: u64,
        cache_min_samples: u64,
    ) -> Self {
        Self {
            view,
            ledger,
            isl_tokens,
            latency_min_samples,
            cache_min_samples,
            overhead: latency_term(&view.overhead, latency_min_samples),
        }
    }

    fn ops(&self, candidate: &Candidate) -> Option<&'a TargetOps> {
        self.view.target(&candidate.target)
    }

    /// The candidate's quote with the reuse correction (draft section 9).
    ///
    /// The shortfall is re-priced through
    /// [`ProviderPricing::price_tokens`](crate::routing::ProviderPricing::price_tokens),
    /// the one pricing contract, so a token moved from cached to uncached pays
    /// the effective write rate and a cache-write premium survives. Revision 1
    /// of the draft used the plain input rate less the read rate, which
    /// dropped the premium and priced a one-hour cache at half its cost.
    ///
    /// **The re-priced term stops at zero.** Moving a token to uncached raises
    /// the price only while the card's cached read rate is below its effective
    /// write rate. Nothing refuses a card where it is not, and under one a
    /// shortfall would make the route look cheaper, which the owner's rule
    /// forbids. The clamp holds the rule for any card.
    ///
    /// **The shortfall's cost appears once, here.** No separate cache-miss
    /// penalty is added on top, and a local target is not corrected: its quote
    /// is the residency answer, priced at the configured capacity rate when
    /// there is one, and it has no ledger model to re-price against.
    pub fn cost(&self, candidate: &Candidate) -> CostEvidence {
        let quoted_usd = candidate.expected_cost_usd;
        let unchanged = |correction| CostEvidence {
            quoted_usd,
            adjusted_usd: quoted_usd,
            correction,
        };
        if candidate.target.is_local() {
            return unchanged(CostCorrection::NotFrontier);
        }
        let reuse = self.ops(candidate).map(|ops| ops.cache).unwrap_or_default();
        if !enough(reuse.n, self.cache_min_samples) {
            return unchanged(CostCorrection::TooFewSamples);
        }
        if reuse.predicted_permille == 0 {
            return unchanged(CostCorrection::NoPredictedReuse);
        }
        let isl = self.isl_tokens as f64;
        let quoted_cached = quoted_cached_tokens(candidate, self.isl_tokens);
        let adjusted_cached = adjusted_cached_tokens(candidate, self.isl_tokens, &reuse);
        let (_, pricing) = self.ledger.model_for(&candidate.target);
        let repriced = pricing.price_tokens(isl - adjusted_cached, adjusted_cached, 0.0)
            - pricing.price_tokens(isl - quoted_cached, quoted_cached, 0.0);
        let adjusted_usd = quoted_usd + repriced.max(0.0);
        CostEvidence {
            quoted_usd,
            adjusted_usd,
            correction: CostCorrection::Applied,
        }
    }

    /// First output from turn start, modeled (ruling 10): the quoted TTFT, the
    /// target's mean residual from its `Routed` to first output, and the
    /// project's mean overhead from `TurnStarted` to `Routed`.
    ///
    /// The cache correction does not also move this: the residual already
    /// contains the latency effect of a reuse shortfall, and adding it again
    /// would count it twice.
    ///
    /// Clamped at zero. A residual can be negative, a target that answers
    /// faster than its quote, but a first output before the turn started is
    /// not an estimate of anything, and a clamp can only raise the number.
    pub fn first_output(&self, candidate: &Candidate) -> TtftEvidence {
        let residual = self
            .ops(candidate)
            .map(|ops| latency_term(&ops.latency, self.latency_min_samples))
            .unwrap_or(LatencyTerm::TooFewSamples);
        let quoted_ms = candidate.expected_ttft_ms;
        let adjusted_ms = (quoted_ms + residual.added_ms() + self.overhead.added_ms()).max(0.0);
        TtftEvidence {
            quoted_ms,
            adjusted_ms,
            residual,
            overhead: self.overhead,
        }
    }
}

impl LatencyTerm {
    /// What this term adds to the estimate: its mean, or nothing.
    fn added_ms(self) -> f64 {
        match self {
            LatencyTerm::Applied { mean_ms } => mean_ms as f64,
            LatencyTerm::TooFewSamples => 0.0,
        }
    }
}

/// Whether `n` samples meet a configured minimum.
///
/// Zero samples never do, whatever the minimum: a configured minimum of zero
/// would otherwise divide by zero, and no samples is no measurement. The gate
/// applies the same rule to live units and sessions, so a prior never passes
/// on its own under zero minimums.
pub(super) fn enough(n: u64, min_samples: u64) -> bool {
    n > 0 && n >= min_samples
}

/// The mean of a latency sum as a model term, or `TooFewSamples` below the
/// minimum.
///
/// **The mean rounds up**, towards positive infinity, so a mean that is not
/// whole never understates latency. Integer arithmetic over `i128`, so no sum
/// the store can hold overflows or loses precision on the way.
pub fn latency_term(sum: &LatencySum, min_samples: u64) -> LatencyTerm {
    if !enough(sum.n, min_samples) {
        return LatencyTerm::TooFewSamples;
    }
    let n = i128::from(sum.n);
    let total = i128::from(sum.sum_ms);
    let ceiling = total.div_euclid(n) + i128::from(total.rem_euclid(n) != 0);
    // The mean of `i64` samples lies within `i64`, so the conversion cannot
    // fail; saturating keeps the function total if a corrupt sum ever says
    // otherwise.
    let mean_ms = i64::try_from(ceiling).unwrap_or(if ceiling < 0 { i64::MIN } else { i64::MAX });
    LatencyTerm::Applied { mean_ms }
}

/// The quote's own predicted cached count: `isl - expected_prefill_tokens`,
/// within `0..=isl`.
fn quoted_cached_tokens(candidate: &Candidate, isl_tokens: usize) -> f64 {
    let isl = isl_tokens as f64;
    (isl - candidate.expected_prefill_tokens).clamp(0.0, isl)
}

/// The reused count after the correction:
/// `floor(quoted_cached * observed / predicted)`, bounded by the matched
/// prefix, the input, and the quote's own cached count.
///
/// **The last bound is the owner's rule**, and it is a bound here rather than a
/// convention of the quote producer. `FrontierCatalog::quote` happens to set
/// the matched prefix to the floor of the weighted cached count, which already
/// keeps the result at or below the quote. But a local worker reports the raw
/// prefix in that field (see [`Candidate::matched_prefix_tokens`]), and a
/// frontier producer that did the same would let a reuse surplus price a route
/// below the ledger's quote. The ledger's prediction is itself bounded by the last block marker a
/// request actually sent, so a target that reused more than that has an
/// upside the learner does not bank on.
///
/// `reuse.predicted_permille` must be nonzero; [`Corrections::cost`] records
/// `NoPredictedReuse` and never calls this otherwise. A zero is treated as a
/// ratio of one rather than dividing by it.
pub fn adjusted_cached_tokens(candidate: &Candidate, isl_tokens: usize, reuse: &CacheReuse) -> f64 {
    let quoted_cached = quoted_cached_tokens(candidate, isl_tokens);
    let ratio = match reuse.predicted_permille {
        0 => 1.0,
        predicted => reuse.observed_permille as f64 / predicted as f64,
    };
    (quoted_cached * ratio)
        .floor()
        .min(candidate.matched_prefix_tokens as f64)
        .min(isl_tokens as f64)
        .min(quoted_cached)
        .max(0.0)
}

/// The grant constraint on a corrected cost (draft section 7.2, constraint 2).
///
/// **Takes the [`CostEvidence`], not a number**, and reads its `adjusted_usd`.
/// A bare `f64` parameter would accept the quote as readily as the correction,
/// and a grant checked on the quote passes exactly the candidates the
/// correction exists to catch. With the evidence as the input, that mistake
/// does not compile:
///
/// ```compile_fail
/// # use roundhouse_core::control::{BudgetState, TurnBudget};
/// # use roundhouse_core::routing::Candidate;
/// # use roundhouse_core::routing::learn::grant;
/// fn check(budget: &TurnBudget, candidate: &Candidate) {
///     grant(budget, BudgetState::Unconstrained, candidate, candidate.expected_cost_usd);
/// }
/// ```
///
/// `admitted_as` is the budget state of the plan's decision, which is where
/// admission recorded whether the overflow valve opened. **A candidate the
/// valve re-admitted keeps [`GrantCheck::Overflow`]**: it is past the grant by
/// definition, and re-judging it against a ceiling of zero would remove what
/// admission deliberately added. Every other candidate is checked by
/// [`TurnBudget::admits`] on a copy carrying the corrected cost, so the budget
/// keeps its own rules, local exemption included, and a correction that lifts
/// a cost over the grant fails the constraint.
pub fn grant(
    budget: &TurnBudget,
    admitted_as: BudgetState,
    candidate: &Candidate,
    cost: &CostEvidence,
) -> GrantCheck {
    if admitted_as.overflowed() {
        return GrantCheck::Overflow;
    }
    let corrected = Candidate {
        expected_cost_usd: cost.adjusted_usd,
        ..candidate.clone()
    };
    if budget.admits(&corrected) {
        GrantCheck::Admits
    } else {
        GrantCheck::Exceeds
    }
}
