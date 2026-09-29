// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! How often the served tier matched the classifier's tier pick, on the wire.
//!
//! **A comparison, never a score.** Agreement says the router and the
//! classifier made the same call. It does not say either call was right. The
//! frontier review is the only quality signal, which is why every disagreement
//! carries the label of the review that covered it: that pairing is what tells
//! the owner whether the classifier is worth what it costs (2026-09-28
//! addendum, "Jev as a scout", item 2).

use serde::Serialize;

use crate::metrics::agreement::AgreementCounts;

/// One scope's agreement between the served tier and the classifier's pick.
///
/// Two partitions hold on every document, and the dashboard relies on both:
/// `answered == agree + disagree + not_comparable`, and
/// `disagree == disagreements.positive + negative + unknown + unlabeled`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TierAgreement {
    /// Classifier results in scope that carried a tier answer.
    pub answered: u64,
    /// Answers that named the tier the turn was served on.
    pub agree: u64,
    /// Answers that named the other tier.
    pub disagree: u64,
    /// Answers with no served tier to compare against: no tier recipe routed
    /// the turn, its target is in neither recipe list, its route is not the
    /// session's latest route in the log this process folded, or the fold had
    /// stopped waiting for the answer at the per-session bound.
    pub not_comparable: u64,
    pub disagreements: TierDisagreements,
}

/// The disagreements, by direction and by what the covering review said.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TierDisagreements {
    /// The classifier picked `capable` and the turn was served `efficient`.
    pub jev_capable_served_efficient: u64,
    /// The classifier picked `efficient` and the turn was served `capable`.
    pub jev_efficient_served_capable: u64,
    /// The covering review labelled the interval positive.
    pub positive: u64,
    /// The covering review labelled the interval negative.
    pub negative: u64,
    /// The covering review could not label the interval.
    pub unknown: u64,
    /// No review has covered the turn yet.
    pub unlabeled: u64,
    /// Of `unlabeled`, the turns this process stopped waiting for, because a
    /// session retained more unlabeled turns than its bound. A later review of
    /// one of them labels nothing.
    pub evicted: u64,
}

impl TierAgreement {
    pub(super) fn build(counts: &AgreementCounts) -> Self {
        let disagree = counts.jev_capable_served_efficient + counts.jev_efficient_served_capable;
        let labeled = counts.positive + counts.negative + counts.unknown;
        Self {
            answered: counts.answered,
            agree: counts.agree,
            disagree,
            not_comparable: counts.not_comparable,
            disagreements: TierDisagreements {
                jev_capable_served_efficient: counts.jev_capable_served_efficient,
                jev_efficient_served_capable: counts.jev_efficient_served_capable,
                positive: counts.positive,
                negative: counts.negative,
                unknown: counts.unknown,
                unlabeled: disagree - labeled,
                evicted: counts.evicted,
            },
        }
    }
}
