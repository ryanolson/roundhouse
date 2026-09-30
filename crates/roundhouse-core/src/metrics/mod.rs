// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Metrics as a projection of the event log.
//!
//! Nothing here is separately recorded. Token counts, dollars, and the savings
//! figure are all folded out of the same append-only log that already carries
//! the conversation and the routing ledger, for the reason stated in
//! [`crate::store`]: one write path means the dashboard cannot disagree with
//! the audit trail. A counter incremented alongside the log would drift the
//! first time a turn failed between the two writes, and the drift would be
//! silent and permanent.
//!
//! The fold is [`MetricsFold::apply`], a pure function of the events it is
//! given, so the same code serves two jobs. A live process feeds it each event
//! as it is appended and answers `/v1/metrics` from memory; a process that
//! wants to rebuild — after a restart, or to check the live numbers — replays
//! the log through the identical fold and must get the identical answer. That
//! equivalence is what [`MetricsFold`] is tested on.
//!
//! ## The two axes
//!
//! A turn is grouped twice, because the two questions are different. **Who
//! serves it** — Anthropic, OpenAI, our own fleet — is [`ModelKey::provider`],
//! and it is what a rate card attaches to. **Where it runs** — on hardware we
//! own or on someone's endpoint — is [`ServingMode`], and it is what the
//! savings argument turns on.
//!
//! ## Where the money comes from
//!
//! Three figures, and they are not equally solid. Keeping them apart is the
//! point of [`Savings`]:
//!
//! - [`Savings::frontier_spend_usd`] is money that left the building. Measured
//!   token counts, published rate card.
//! - [`Savings::cache_savings_usd`] is a discount a provider actually applied,
//!   reconstructed from the cache-read tokens it reported and the gap between
//!   its two published rates. Measured.
//! - [`Savings::routing_savings_usd`] is a **counterfactual**: what our own
//!   fleet's traffic would have cost had it gone to a comparable hosted model
//!   instead, less that traffic's local capacity cost when the catalog sets a
//!   `local_capacity_price`. There is no measurement of a call that never
//!   happened, so this rests entirely on the correlary chosen in [`pricing`],
//!   and it is only as good as that choice.
//!
//! A single total would hide that distinction, so the snapshot reports all
//! three and lets the reader decide which claim to make.
//!
//! There is a fourth quantity and it is deliberately **not** money:
//! [`MetricsSnapshot::seat_tokens`], the traffic served under a forwarded
//! subscription seat. Roundhouse holds no rate card for a seat, so the honest
//! report is the token count with no dollar beside it — the same rule
//! [`SettledSpend`](crate::control::SettledSpend) states at the ledger, kept
//! here by [`Billing`](crate::control::Billing) travelling in the log and by
//! this projection pricing only what it marks as billable.
//!
//! ## The other economy
//!
//! [`MetricsSnapshot::evaluation`] is what this deployment spent *classifying*
//! its own turns, and it is on its own axis because it is priced by a different
//! authority: a classifier call carries the amount it was priced at, under the
//! rate card its own reservation recorded, so a corrected catalog reprices the
//! three figures above and leaves it exactly where it was. Adding it into
//! [`Savings`] would put a number no serving rate card produced into the column
//! the savings claim is computed from.
//!
//! [`MetricsSnapshot::observed_cost`] is the one field that adds the two, and it
//! publishes both bases beside the sum rather than quietly merging them. It is
//! a spend figure and never a saving: [`Savings::total_usd`] remains cache plus
//! routing savings, which is a different question with a different answer.
//!
//! **It is also not all economic cost, and it says so.** What it covers is
//! money that left the building, plus our own fleet's GPU time when the
//! catalog sets a `local_capacity_price` —
//! [`OBSERVED_COST_SCOPE`] or [`OBSERVED_COST_SCOPE_WITH_LOCAL_CAPACITY`]
//! names which on the wire. Otherwise two kinds of traffic are counted
//! everywhere else here and priced nowhere, so the total passes over both: a
//! turn our own fleet answered, and a turn on a forwarded subscription seat.
//! Neither has a per-token price this projection could state without inventing
//! one, so both are published as counts — [`ServingCostGaps::local_calls`] and
//! [`MetricsSnapshot::seat_tokens`]. A configured capacity price is the
//! deployment's own statement of what its GPU time is worth, so with one the
//! local turns are priced at it (ruled 2026-09-28) and are no longer a gap.
//!
//! Only the first makes the total *incomplete*, and the difference is whose
//! money it is: GPU time is this deployment's cost, paid in hardware rather than
//! in invoices, while a seat's tokens were charged to the caller's own
//! subscription. So a deployment serving most of its own traffic reports a small
//! combined cost *and* an incomplete one — the honest pair, because the first
//! number is true and would be the most misleading figure on the page without
//! the second — while a pass-through deployment reports a small one that is
//! complete, since nothing it paid for is missing.

//! ## Layout
//!
//! Three modules, split along the two seams the design already had. [`fold`]
//! turns events into token counters and touches no money. [`snapshot`] applies
//! a rate card to those counters and owns every dollar figure and every wire
//! type. [`pricing`] owns the correlary machinery [`snapshot`] consults. This
//! module keeps only the vocabulary all three share and the live recorder that
//! drives them, and re-exports the rest so callers see one surface.

pub(crate) mod cache_evidence;
pub(crate) mod evaluation;
pub mod fold;
pub mod pricing;
pub mod snapshot;
pub(crate) mod timing;

#[cfg(test)]
mod cache_reuse_evidence_tests;
#[cfg(test)]
mod first_output_snapshot_tests;
#[cfg(test)]
mod turn_elapsed_snapshot_tests;

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::control::{PrincipalKey, ProjectId};
use crate::event::{SessionEvent, SessionObserver};
use crate::routing::Target;
use crate::validate::Arm;

pub use fold::{MetricsFold, Scope, SideCallTally, ValidationTally};
pub use pricing::{
    Correlary, DEFAULT_CAPABILITY_BAND, IncoherentCorrelary, PricedBasis, ReferenceModel,
    ShadowPricing, TokenShape,
};
pub use snapshot::{
    CacheReuseEvidence, Coverage, EVALUATION_PRICE_BASIS, EvaluationMetrics,
    EvaluationModelMetrics, EvaluationSettlement, EvaluationTokens, EvaluationUnbooked,
    FIRST_OUTPUT_BASIS, IntervalMetric, MetricsConfig, MetricsSnapshot, ModelAccounting,
    ModelMetrics, OBSERVED_CACHE_BASIS, OBSERVED_COST_SCOPE,
    OBSERVED_COST_SCOPE_WITH_LOCAL_CAPACITY, ObservedCost, PREDICTED_CACHE_BASIS, ProviderMetrics,
    Rollup, SERVING_PRICE_BASIS, Savings, ServingCostGaps, ServingModeMetrics, TURN_ELAPSED_BASIS,
    TokenBreakdown,
};

/// The provider name local targets are grouped under.
///
/// [`Target::Local`] carries a worker and a model but no provider, because
/// locally there is nobody to bill. The rollup still needs a name in that
/// column, and the fleet's own is the honest one — the alternative, an empty
/// string or "none", reads as missing data rather than as the deliberate
/// absence of a vendor.
pub const LOCAL_PROVIDER: &str = "dynamo";

/// Whether a target runs on hardware we own.
///
/// The axis the whole savings argument is stated on, which is why it is its own
/// type rather than a `bool` named `is_local`: the two sides are accounted for
/// completely differently, and a boolean at a call site does not say which way
/// round it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServingMode {
    /// Served by our own Dynamo fleet. Bills nobody; costs GPU time, which the
    /// catalog may price with `local_capacity_price`.
    Local,
    /// Issued to an external endpoint. Bills real money.
    Frontier,
}

impl ServingMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServingMode::Local => "local",
            ServingMode::Frontier => "frontier",
        }
    }
}

/// One row of the breakdown.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelKey {
    pub mode: ServingMode,
    pub provider: String,
    pub model: String,
}

impl ModelKey {
    pub fn from_target(target: &Target) -> Self {
        match target {
            // Deliberately not keyed by worker: which of our GPUs served a turn
            // is a fleet-balance question, and putting it here would split one
            // model's row into one row per worker and make every per-model
            // number meaningless at a glance.
            Target::Local { model, .. } => Self {
                mode: ServingMode::Local,
                provider: LOCAL_PROVIDER.to_string(),
                model: model.clone(),
            },
            Target::Frontier { provider, model } => Self {
                mode: ServingMode::Frontier,
                provider: provider.clone(),
                model: model.clone(),
            },
        }
    }
}

/// Process-wide metrics, maintained as sessions run.
///
/// Cheap to clone and shared by every handler. The lock is `std` rather than
/// `tokio` on purpose: nothing inside the critical section awaits, and an async
/// lock would suggest it might.
#[derive(Clone, Default)]
pub struct MetricsRecorder {
    fold: Arc<RwLock<MetricsFold>>,
}

impl MetricsRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold events in. Safe to call with events already seen — see
    /// [`MetricsFold`] on idempotency, which is what lets a session's replay
    /// on open feed this without double counting what a live feed already had.
    pub fn record(&self, events: &[SessionEvent]) {
        // A poisoned lock is recovered rather than propagated. The fold holds
        // counters, not invariants another thread's panic could have corrupted
        // halfway, and taking the whole metrics surface down for the life of
        // the process because one request panicked is the worse failure.
        let mut fold = self.fold.write().unwrap_or_else(|e| e.into_inner());
        fold.extend(events);
    }

    pub fn snapshot(&self, config: &MetricsConfig, generated_at_ms: u64) -> MetricsSnapshot {
        let fold = self.fold.read().unwrap_or_else(|e| e.into_inner());
        MetricsSnapshot::build(&fold, Scope::Deployment, config, generated_at_ms)
    }

    /// The same report, restricted to one principal's share of the same fold.
    ///
    /// What a turn key is answered with. A separate method rather than a
    /// [`Scope`] argument on [`Self::snapshot`] so that "this document is
    /// somebody's only" is a decision named at the call site, in the surface
    /// that serves it. The two are one function underneath — the scope seam is
    /// [`MetricsSnapshot::build`] — so there is no second pricing walk for
    /// these two entry points to disagree over.
    pub fn snapshot_for(
        &self,
        scope: &PrincipalKey,
        config: &MetricsConfig,
        generated_at_ms: u64,
    ) -> MetricsSnapshot {
        let fold = self.fold.read().unwrap_or_else(|e| e.into_inner());
        MetricsSnapshot::build(&fold, Scope::Principal(scope), config, generated_at_ms)
    }

    /// The same report, restricted to everything one *project* spent.
    ///
    /// What the admin plane's reconciliation view measures against the ledger's
    /// committed figure. A separate method rather than a loop over
    /// [`Self::snapshot_for`] per configured member, and the difference is the
    /// point: a project's measured spend has to include the members who are no
    /// longer configured — a key deleted, a person removed — or the column
    /// would shrink whenever tenancy was tidied up, and the drift against the
    /// ledger would be blamed on the ledger. The fold knows who spent; only the
    /// config knows who may. See [`Scope::Project`].
    pub fn snapshot_for_project(
        &self,
        project: &ProjectId,
        config: &MetricsConfig,
        generated_at_ms: u64,
    ) -> MetricsSnapshot {
        let fold = self.fold.read().unwrap_or_else(|e| e.into_inner());
        MetricsSnapshot::build(&fold, Scope::Project(project), config, generated_at_ms)
    }

    /// What one arm of the validate experiment decided, and how often it acted.
    ///
    /// **Not on [`MetricsSnapshot`], and that is the deliberate half.** The
    /// snapshot is the money document — tokens, rate cards, savings — and the
    /// arm comparison is a *control* figure whose honest presentation is three
    /// numbers side by side (spend measured, tokens-after-intervention against
    /// the arm-matched control, prevented waste estimated), never a single
    /// "validation saved you $X" folded into a total. Until a surface exists
    /// that reports them that way, the fold answers directly, so the arm
    /// comparison is readable from the same projection the log builds rather
    /// than from a counter beside it.
    pub fn validation_tally(&self, scope: Scope<'_>, arm: Arm) -> ValidationTally {
        let fold = self.fold.read().unwrap_or_else(|e| e.into_inner());
        fold.validation_tally(scope, arm)
    }

    /// Side calls booked and side calls abandoned, in one scope.
    ///
    /// The discarded-work half of the same question: a check that produced
    /// nothing still happened, and a deployment that could not see the
    /// abandoned count would read a broken judge as a free one.
    pub fn side_call_tally(&self, scope: Scope<'_>) -> SideCallTally {
        let fold = self.fold.read().unwrap_or_else(|e| e.into_inner());
        fold.side_call_tally(scope)
    }
}

impl SessionObserver for MetricsRecorder {
    fn observe(&self, events: &[SessionEvent]) {
        self.record(events);
    }
}

#[cfg(test)]
mod tests;
