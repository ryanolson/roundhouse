// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The online learner's section of the metrics document (draft section 12.4).
//!
//! Counts, never money: what the learner decided, why a turn was infeasible,
//! how often its store read failed, and how its entries were delivered. The
//! decisions are folded from the log; `delivery` is process memory, see
//! [`crate::metrics::learning`].
//!
//! **A `refuse` project's refused turn is not here.** A turn refused for a
//! store outage or an infeasible plan writes no `Routed`, so it is counted
//! under none of `decisions`, `unmet`, or `read_failures`; it terminates as
//! `PolicyRefused` like any other policy refusal. Giving it a `read_failures`
//! reason of its own would count the same event twice under two names.

use serde::Serialize;

use crate::metrics::learning::{DeliveryCounts, LearningCounts};

/// One scope's learned decisions and deliveries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LearningMetrics {
    /// Turns a learner decided, one per turn however many dispatches it made.
    pub decisions: u64,
    pub modes: LearnedModes,
    /// The strategy whose plan served each decision: `rules` in `shadow` mode
    /// and on every infeasible turn.
    pub served: LearnedServed,
    pub choices: LearnedChoices,
    /// Infeasible decisions, by each constraint some plan failed. One decision
    /// can count under several.
    pub unmet: LearnedUnmet,
    /// Decisions whose store read failed, by reason.
    pub read_failures: LearnedReadFailures,
    /// `LearningApplied` acknowledgements written to the log.
    pub acknowledgements: u64,
    /// What delivery attempts met since this process started. Present in the
    /// deployment and project scopes, `null` in a member's scope: the counts
    /// are per project and are not a projection of the log.
    pub delivery: Option<LearningDeliveryMetrics>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LearnedModes {
    pub shadow: u64,
    pub live: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LearnedServed {
    pub rules: u64,
    pub efficient: u64,
    pub capable: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LearnedChoices {
    pub exploit: u64,
    pub explore: u64,
    pub constraint_unmet: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LearnedUnmet {
    pub quality: u64,
    pub latency: u64,
    pub grant: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LearnedReadFailures {
    pub store_unavailable: u64,
    pub read_timed_out: u64,
}

/// Delivery outcomes, in process memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LearningDeliveryMetrics {
    /// Entries a store apply accepted.
    pub applied_entries: u64,
    /// Entries a store apply skipped because it already held them: resends
    /// after a lost or refused acknowledgement, or a successor's stale hint.
    pub duplicate_entries: u64,
    /// Backfill replays, each a full read of the session log.
    pub backfills: u64,
    /// Applies the store refused with a gap.
    pub gaps: u64,
    /// Sessions whose delivery stopped on a diverged chain.
    pub diverged: u64,
    /// Sessions whose delivery stopped on a counter out of range or a
    /// malformed batch.
    pub stopped: u64,
    /// Applies the store did not answer, or refused for a key of the wrong
    /// type. The entries stay pending.
    pub unavailable: u64,
    /// Applies that did not return within `apply_timeout_ms`. The entries stay
    /// pending.
    pub timed_out: u64,
    /// Applies that landed and whose mark clear or acknowledgement append then
    /// failed. The mark stays for the next turn or the recovery task.
    pub acknowledgement_failures: u64,
}

impl LearningMetrics {
    pub(super) fn build(counts: &LearningCounts) -> Self {
        Self {
            decisions: counts.decisions,
            modes: LearnedModes {
                shadow: counts.shadow,
                live: counts.live,
            },
            served: LearnedServed {
                rules: counts.served_rules,
                efficient: counts.served_efficient,
                capable: counts.served_capable,
            },
            choices: LearnedChoices {
                exploit: counts.exploit,
                explore: counts.explore,
                constraint_unmet: counts.constraint_unmet,
            },
            unmet: LearnedUnmet {
                quality: counts.unmet_quality,
                latency: counts.unmet_latency,
                grant: counts.unmet_grant,
            },
            read_failures: LearnedReadFailures {
                store_unavailable: counts.read_unavailable,
                read_timed_out: counts.read_timed_out,
            },
            acknowledgements: counts.acknowledgements,
            delivery: None,
        }
    }
}

impl LearningDeliveryMetrics {
    pub(crate) fn build(counts: &DeliveryCounts) -> Self {
        Self {
            applied_entries: counts.applied_entries,
            duplicate_entries: counts.duplicate_entries,
            backfills: counts.backfills,
            gaps: counts.gaps,
            diverged: counts.diverged,
            stopped: counts.stopped,
            unavailable: counts.unavailable,
            timed_out: counts.timed_out,
            acknowledgement_failures: counts.acknowledgement_failures,
        }
    }
}
