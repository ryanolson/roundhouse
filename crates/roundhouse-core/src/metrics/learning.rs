// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The online learner's decisions and deliveries (draft section 12.4).
//!
//! Two halves, and they are kept apart on purpose.
//!
//! - [`LearningFold`] is a projection of the log, like everything else in
//!   [`MetricsFold`](super::MetricsFold): learned decisions read off the
//!   `Routed` that carries them, and acknowledgements off `LearningApplied`.
//!   A replay reaches the same counts.
//! - [`LearningDelivery`] is **not** a projection of the log. What a delivery
//!   attempt met (a duplicate, a gap, a diverged chain, an outage) writes
//!   nothing to the log by design: a failed delivery must leave the entries
//!   pending and append nothing. So the engine counts those outcomes here, in
//!   process memory, and they reset with the process. The snapshot names them
//!   `delivery` and carries them only in the deployment and project scopes.
//!
//! **Causes are not here.** Why an accepted review credited nothing is decided
//! by the credit rule in the session fold (`SessionState::learning_causes`).
//! Counting causes here would need a second spelling of that rule, and two
//! spellings drift.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::control::{PrincipalKey, ProjectId};
use crate::metrics::fold::Scope;
use crate::routing::learn::{
    ActiveMode, LearnedChoice, LearnedEvidence, ReadFailure, StoreRead, Strategy, Unmet,
};
use crate::routing::{DecisionRecord, SelectorBranch};

/// One principal's learned decisions, add-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LearningCounts {
    pub(crate) decisions: u64,
    pub(crate) shadow: u64,
    pub(crate) live: u64,
    pub(crate) served_rules: u64,
    pub(crate) served_efficient: u64,
    pub(crate) served_capable: u64,
    pub(crate) exploit: u64,
    pub(crate) explore: u64,
    pub(crate) constraint_unmet: u64,
    pub(crate) unmet_quality: u64,
    pub(crate) unmet_latency: u64,
    pub(crate) unmet_grant: u64,
    pub(crate) read_unavailable: u64,
    pub(crate) read_timed_out: u64,
    pub(crate) acknowledgements: u64,
}

impl LearningCounts {
    fn absorb(&mut self, other: &Self) {
        self.decisions += other.decisions;
        self.shadow += other.shadow;
        self.live += other.live;
        self.served_rules += other.served_rules;
        self.served_efficient += other.served_efficient;
        self.served_capable += other.served_capable;
        self.exploit += other.exploit;
        self.explore += other.explore;
        self.constraint_unmet += other.constraint_unmet;
        self.unmet_quality += other.unmet_quality;
        self.unmet_latency += other.unmet_latency;
        self.unmet_grant += other.unmet_grant;
        self.read_unavailable += other.read_unavailable;
        self.read_timed_out += other.read_timed_out;
        self.acknowledgements += other.acknowledgements;
    }

    fn decision(&mut self, evidence: &LearnedEvidence) {
        self.decisions += 1;
        match evidence.mode {
            ActiveMode::Shadow => self.shadow += 1,
            ActiveMode::Live => self.live += 1,
        }
        match evidence.served_strategy() {
            Strategy::Rules => self.served_rules += 1,
            Strategy::Efficient => self.served_efficient += 1,
            Strategy::Capable => self.served_capable += 1,
        }
        match &evidence.choice {
            LearnedChoice::Exploit { .. } => self.exploit += 1,
            LearnedChoice::Explore { .. } => self.explore += 1,
            LearnedChoice::ConstraintUnmet { unmet } => {
                self.constraint_unmet += 1;
                for unmet in unmet {
                    match unmet {
                        Unmet::Quality => self.unmet_quality += 1,
                        Unmet::Latency => self.unmet_latency += 1,
                        Unmet::Grant => self.unmet_grant += 1,
                        // Counted from the view below, once per decision,
                        // whether or not the turn was infeasible.
                        Unmet::StoreUnavailable | Unmet::ReadTimedOut => {}
                    }
                }
            }
        }
        if let StoreRead::Unavailable { reason } = &evidence.view {
            match reason {
                ReadFailure::StoreUnavailable => self.read_unavailable += 1,
                ReadFailure::ReadTimedOut => self.read_timed_out += 1,
            }
        }
    }
}

#[derive(Default)]
pub(super) struct LearningFold {
    by_principal: BTreeMap<PrincipalKey, LearningCounts>,
}

impl LearningFold {
    /// Count a learned decision off its `Routed`.
    ///
    /// `failover` is whether this `Routed` is a later dispatch of a response
    /// the fold already saw routed. A failover writes one `Routed` per
    /// dispatch with the same selection, and the decision was taken once, so
    /// only the first counts. The caller already holds that answer (its
    /// pending response table), so the fold keeps no map of its own that
    /// would grow with every learned session.
    pub(super) fn routed(
        &mut self,
        payer: &PrincipalKey,
        failover: bool,
        decision: &DecisionRecord,
    ) {
        if failover {
            return;
        }
        let Some(SelectorBranch::Learned(evidence)) = decision
            .selection
            .as_deref()
            .and_then(|selection| selection.selector.as_ref())
            .map(|selector| &selector.branch)
        else {
            return;
        };
        self.by_principal
            .entry(payer.clone())
            .or_default()
            .decision(evidence);
    }

    pub(super) fn applied(&mut self, payer: &PrincipalKey) {
        self.by_principal
            .entry(payer.clone())
            .or_default()
            .acknowledgements += 1;
    }

    pub(super) fn tally(&self, scope: Scope<'_>) -> LearningCounts {
        let mut total = LearningCounts::default();
        for (owner, counts) in &self.by_principal {
            if scope.collects(owner) {
                total.absorb(counts);
            }
        }
        total
    }
}

/// What one delivery attempt met, as the engine reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The store applied `applied` entries and skipped `duplicates` it
    /// already held.
    Applied { applied: u64, duplicates: u64 },
    /// A backfill replay ran, for a gap or for a page that ran dry.
    Backfill,
    /// The store refused the batch with `ChainGap`.
    Gap,
    /// The store refused the batch with `ChainDiverged`; the session's
    /// delivery stopped.
    Diverged,
    /// The store refused the batch with `CounterRange` or `Malformed`; the
    /// session's delivery stopped.
    Stopped,
    /// The store did not answer, or answered `WrongType`; the entries stay
    /// pending.
    Unavailable,
    /// The apply did not return within its timeout; the entries stay pending.
    TimedOut,
    /// The store applied, and the source mark clear or the `LearningApplied`
    /// append failed afterwards; the mark stays for the next turn or the
    /// recovery task.
    AcknowledgementFailed,
}

/// One project's delivery outcomes since this process started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DeliveryCounts {
    pub(crate) applied_entries: u64,
    pub(crate) duplicate_entries: u64,
    pub(crate) backfills: u64,
    pub(crate) gaps: u64,
    pub(crate) diverged: u64,
    pub(crate) stopped: u64,
    pub(crate) unavailable: u64,
    pub(crate) timed_out: u64,
    pub(crate) acknowledgement_failures: u64,
}

impl DeliveryCounts {
    fn absorb(&mut self, other: &Self) {
        self.applied_entries += other.applied_entries;
        self.duplicate_entries += other.duplicate_entries;
        self.backfills += other.backfills;
        self.gaps += other.gaps;
        self.diverged += other.diverged;
        self.stopped += other.stopped;
        self.unavailable += other.unavailable;
        self.timed_out += other.timed_out;
        self.acknowledgement_failures += other.acknowledgement_failures;
    }
}

/// Delivery outcomes, per project, in process memory. See the module doc for
/// why these are not folded from the log.
#[derive(Default)]
pub struct LearningDelivery {
    by_project: Mutex<BTreeMap<ProjectId, DeliveryCounts>>,
}

impl LearningDelivery {
    pub fn record(&self, project: &ProjectId, outcome: DeliveryOutcome) {
        // Recovered rather than propagated, for `MetricsRecorder::record`'s
        // reason: these are counters, not invariants.
        let mut by_project = self
            .by_project
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counts = by_project.entry(project.clone()).or_default();
        match outcome {
            DeliveryOutcome::Applied {
                applied,
                duplicates,
            } => {
                counts.applied_entries += applied;
                counts.duplicate_entries += duplicates;
            }
            DeliveryOutcome::Backfill => counts.backfills += 1,
            DeliveryOutcome::Gap => counts.gaps += 1,
            DeliveryOutcome::Diverged => counts.diverged += 1,
            DeliveryOutcome::Stopped => counts.stopped += 1,
            DeliveryOutcome::Unavailable => counts.unavailable += 1,
            DeliveryOutcome::TimedOut => counts.timed_out += 1,
            DeliveryOutcome::AcknowledgementFailed => counts.acknowledgement_failures += 1,
        }
    }

    /// Every project's counts summed, or one project's.
    pub(crate) fn tally(&self, project: Option<&ProjectId>) -> DeliveryCounts {
        let by_project = self
            .by_project
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut total = DeliveryCounts::default();
        for (owner, counts) in by_project.iter() {
            if project.is_none_or(|project| project == owner) {
                total.absorb(counts);
            }
        }
        total
    }
}
