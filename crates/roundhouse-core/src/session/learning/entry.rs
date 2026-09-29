// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! One learning entry: what one source event adds to the learner store.
//!
//! **Integers only.** Integer addition gives the same state in every order of
//! updates, so two nodes that apply the same entries in different orders, and
//! an offline rebuild from the log, reach the same counters.
//!
//! **An entry says how much, never how many sessions.**
//! [`LearnerStore::apply`](crate::learn_store::LearnerStore::apply) counts a
//! session once per key and strategy from its own `seen` set (draft section
//! 11.4), so a session's second interval on a key adds units and no session.

use crate::routing::learn::{
    CacheReuse, EpochId, JevCounts, LatencySum, LevelKey, Strategy, Units,
};

/// How many entries the session fold holds for delivery at most (draft
/// section 11.5).
///
/// An implementation bound, not a policy: entries past it are counted in
/// [`crate::session::SessionState::learning_beyond`] and refilled by a
/// backfill replay, so none is lost.
pub const LEARNING_PAGE: usize = 64;

/// What one entry-producing event adds to the learner store.
///
/// `(project, session, seq)` is its identity, and `prev_seq` chains it to the
/// entry before it in the session, so the store can refuse a batch that skips
/// one (draft section 11.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningEntry {
    /// The log sequence of the source event.
    pub seq: u64,
    /// The `seq` of the previous entry of this session, or 0 for the first.
    pub prev_seq: u64,
    /// The credit revision this build computed the deltas under.
    pub credit_revision: u32,
    /// The review rule revision this build computed the deltas under. Review
    /// acceptance decides the quality deltas, so it is part of what the deltas
    /// are a function of.
    pub review_rule_revision: u32,
    /// `None` when the event adds nothing. The entry still exists: the event
    /// kind alone decides existence, so the chain is the same under every
    /// credit rule.
    pub deltas: Option<Deltas>,
}

/// The counters one entry adds, all under one epoch.
///
/// **One epoch per entry, by construction.** Credit refuses an interval whose
/// decisions span two epochs, a turn's operational rows come from its one
/// selection, and a Jev answer counts under the epoch of the turn it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deltas {
    pub epoch: EpochId,
    /// Interval credit, per key and strategy.
    pub quality: Vec<QualityDelta>,
    /// Operational rows, per recipe target (its policy identity).
    pub targets: Vec<TargetDelta>,
    /// The project's overhead from `TurnStarted` to the served `Routed`.
    pub overhead: LatencySum,
    /// Jev's tier answers, per key. Counts for the prior, never a reward.
    pub jev: Vec<JevDelta>,
}

impl Deltas {
    pub(crate) fn empty(epoch: EpochId) -> Self {
        Self {
            epoch,
            quality: Vec::new(),
            targets: Vec::new(),
            overhead: LatencySum::default(),
            jev: Vec::new(),
        }
    }

    /// `Some(self)` unless it adds nothing, so an entry with nothing to add
    /// has one spelling, `None`.
    pub(crate) fn non_empty(self) -> Option<Self> {
        let empty = self.quality.is_empty()
            && self.targets.is_empty()
            && self.overhead.n == 0
            && self.jev.is_empty();
        (!empty).then_some(self)
    }
}

/// The credit one interval adds to one strategy at one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityDelta {
    pub key: LevelKey,
    pub strategy: Strategy,
    /// `pos` is zero for a `Negative` interval; `n` is the key's share of
    /// [`CREDIT_SCALE`](crate::routing::learn::CREDIT_SCALE).
    pub units: Units,
}

/// The operational rows one turn adds to one recipe target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetDelta {
    /// The target's policy identity, as the recipe and
    /// [`TargetOps::target`](crate::routing::learn::TargetOps::target) name
    /// it.
    pub target: String,
    /// The residual from the served `Routed` to the first output, less the
    /// rounded quote.
    pub latency: LatencySum,
    /// One for the first dispatched target of a turn that failed over.
    pub failover: u64,
    pub cache: CacheReuse,
}

/// Jev's tier answers on one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JevDelta {
    pub key: LevelKey,
    pub counts: JevCounts,
}

/// Why an accepted interval credited nothing, counted per session.
///
/// Each count is one accepted review whose entry carries no quality deltas
/// (draft section 10). Reviews the fold rejected are already counted by
/// [`crate::session::SessionState::rejected_reviews`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LearningCauses {
    /// The label was `Unknown`, as written or because this build could not
    /// verify the review's membership.
    pub unknown_label: u64,
    /// A covered turn failed over: its served propensity is not the
    /// selection propensity.
    pub failover_in_interval: u64,
    /// A covered decision carries no learned evidence.
    pub missing_row: u64,
    /// The covered decisions belong to more than one epoch.
    pub mixed_epoch: u64,
    /// A covered decision was taken under a credit rule this build does not
    /// apply.
    pub other_credit_revision: u64,
}

impl LearningCauses {
    /// Count one review the screen excluded, under its cause.
    pub(crate) fn count(&mut self, exclusion: super::credit::Exclusion) {
        use super::credit::Exclusion;
        let counter = match exclusion {
            Exclusion::UnknownLabel => &mut self.unknown_label,
            Exclusion::FailoverInInterval => &mut self.failover_in_interval,
            Exclusion::MissingRow => &mut self.missing_row,
            Exclusion::MixedEpoch => &mut self.mixed_epoch,
            Exclusion::OtherCreditRevision => &mut self.other_credit_revision,
        };
        *counter += 1;
    }
}
