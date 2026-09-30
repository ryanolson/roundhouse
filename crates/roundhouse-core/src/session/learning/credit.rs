// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Consistent-trajectory credit.
//!
//! A strategy is consistent with an accepted interval when, on every covered
//! turn, its plan had the same first target as the served dispatch. Each
//! consistent strategy receives [`CREDIT_SCALE`] units at each level, split
//! over the keys the interval visited. No strategy receives evidence for an
//! action another strategy took, and a failover anywhere in the interval
//! credits nothing: the served propensity of a failover turn is not the
//! selection propensity.

use crate::routing::learn::{
    CREDIT_SCALE, EpochId, KeyLevel, LEARNING_CREDIT_REVISION, LevelKey, Strategy, Units,
};
use crate::routing::{DecisionRecord, SelectorBranch, Target};
use crate::validate::IntervalLabel;

use super::entry::{Deltas, LearningCauses, QualityDelta};

/// What credit needs from one covered decision, copied from its `Routed`.
///
/// **Fixed-size and `Copy`**, so the review tracker's per-interval decision
/// bound is also the bound on these rows. A `Routed` names the served target
/// and each plan's first target as recipe indices; this row holds the one fact
/// credit reads from them, which plans agreed with the served dispatch, as a
/// bit per strategy. Comparing on the `Routed` keeps a target that no recipe
/// names (a recipe degrade to a local worker) comparable too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LearningRow {
    epoch: EpochId,
    credit_revision: u32,
    keys: [LevelKey; 3],
    /// Bit `strategy_bit(s)` is set when `s`'s plan's first target is the
    /// target this dispatch went to.
    agrees: u8,
    /// This dispatch followed a failed one in the same turn.
    failed_before: bool,
}

impl LearningRow {
    /// The row of a learned decision, or `None` for any other decision,
    /// including every record written before learned evidence existed.
    pub(crate) fn of(decision: &DecisionRecord) -> Option<Self> {
        let selection = decision.selection.as_deref()?;
        let SelectorBranch::Learned(evidence) = &selection.selector.as_ref()?.branch else {
            return None;
        };
        let agrees = evidence
            .plans
            .iter()
            .filter(|plan| same_route(&plan.first, &decision.chosen))
            .fold(0, |bits, plan| bits | strategy_bit(plan.strategy));
        Some(Self {
            epoch: evidence.epoch,
            credit_revision: evidence.credit_revision,
            keys: evidence.input.keys(),
            agrees,
            failed_before: !decision.attempts.is_empty(),
        })
    }

    pub(crate) fn epoch(&self) -> EpochId {
        self.epoch
    }

    pub(crate) fn keys(&self) -> [LevelKey; 3] {
        self.keys
    }

    /// Recorded under the credit rule this build applies. A row that is not
    /// contributes no deltas of any kind.
    pub(crate) fn is_current(&self) -> bool {
        self.credit_revision == LEARNING_CREDIT_REVISION
    }
}

/// Two targets are the same route when the recipe would name them the same:
/// the provider and model of a hosted target, the model of a local one.
///
/// **Structural, not by comparing `policy_identity` strings**: that would
/// allocate twice per plan on every learned `Routed`, and would equate a
/// hosted provider named `local` with a local worker.
pub(crate) fn same_route(a: &Target, b: &Target) -> bool {
    match (a, b) {
        (Target::Local { model: a, .. }, Target::Local { model: b, .. }) => a == b,
        (
            Target::Frontier {
                provider: pa,
                model: ma,
            },
            Target::Frontier {
                provider: pb,
                model: mb,
            },
        ) => pa == pb && ma == mb,
        _ => false,
    }
}

fn strategy_bit(strategy: Strategy) -> u8 {
    match strategy {
        Strategy::Rules => 1,
        Strategy::Efficient => 2,
        Strategy::Capable => 4,
    }
}

/// Every strategy, in the fixed order credit writes its deltas in.
const STRATEGIES: [Strategy; 3] = [Strategy::Rules, Strategy::Efficient, Strategy::Capable];

/// One decision an accepted review covered.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CoveredRow {
    pub(crate) turn_index: u64,
    /// `None` for a decision without learned evidence.
    pub(crate) row: Option<LearningRow>,
}

/// A review the fold accepted, as credit reads it.
#[derive(Debug, Clone)]
pub(crate) struct Reviewed {
    /// [`IntervalLabel::Unknown`] also for a review whose membership this
    /// build could not verify.
    pub(crate) label: IntervalLabel,
    /// The covered decisions, oldest first. Empty unless the label is
    /// `Positive` or `Negative`, because nothing else is credited.
    pub(crate) rows: Vec<CoveredRow>,
}

/// Why an accepted review credits nothing: the checks, in the order they run.
///
/// **The one spelling of the screen**, read by [`credit`] and by the offline
/// calibrator (`routing::learn::offline`), which excludes exactly the
/// intervals credit refuses and reports them by these causes. Two spellings
/// would let the report count an interval the store never credited, or miss
/// one it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Exclusion {
    /// The label was `Unknown`, as written or because this build could not
    /// verify the review's membership.
    UnknownLabel,
    /// A covered turn failed over.
    FailoverInInterval,
    /// A covered decision carries no learned evidence.
    MissingRow,
    /// The covered decisions belong to more than one epoch.
    MixedEpoch,
    /// A covered decision was taken under a credit rule this build does not
    /// apply.
    OtherCreditRevision,
}

/// Screen one accepted review: `Ok(positive)` when it may be credited, or the
/// first check it fails.
///
/// The checks run in this order: the label, then a failover anywhere,
/// then a decision with no row, then more than one epoch, then a foreign
/// credit revision. Each makes the whole interval credit nothing.
pub(crate) fn screen<I>(label: IntervalLabel, rows: I) -> Result<bool, Exclusion>
where
    I: IntoIterator<Item = Option<LearningRow>>,
    I::IntoIter: Clone,
{
    let rows = rows.into_iter();
    let positive = match label {
        IntervalLabel::Positive => true,
        IntervalLabel::Negative => false,
        IntervalLabel::Unknown => return Err(Exclusion::UnknownLabel),
    };
    if rows.clone().flatten().any(|row| row.failed_before) {
        return Err(Exclusion::FailoverInInterval);
    }
    if rows.clone().any(|row| row.is_none()) {
        return Err(Exclusion::MissingRow);
    }
    let mut learned = rows.flatten();
    if let Some(first) = learned.next() {
        if learned.clone().any(|row| row.epoch != first.epoch) {
            return Err(Exclusion::MixedEpoch);
        }
        if !first.is_current() || learned.any(|row| !row.is_current()) {
            return Err(Exclusion::OtherCreditRevision);
        }
    }
    Ok(positive)
}

/// The quality deltas of one accepted review, or `None` with the cause
/// counted.
///
/// [`screen`] decides whether the interval is credited at all; a review is
/// never accepted without decisions, so a screened interval has rows.
pub(crate) fn credit(reviewed: &Reviewed, causes: &mut LearningCauses) -> Option<Deltas> {
    let positive = match screen(
        reviewed.label,
        reviewed.rows.iter().map(|covered| covered.row),
    ) {
        Ok(positive) => positive,
        Err(exclusion) => {
            causes.count(exclusion);
            return None;
        }
    };
    let rows = reviewed
        .rows
        .iter()
        .map(|covered| covered.row.map(|row| (covered.turn_index, row)))
        .collect::<Option<Vec<_>>>()?;
    let (_, first) = *rows.first()?;

    let consistent = rows
        .iter()
        .fold(u8::MAX, |bits, (_, row)| bits & row.agrees);
    let mut deltas = Deltas::empty(first.epoch);
    for level in 0..KeyLevel::ALL.len() {
        let shares = split(&visits(&rows, level));
        for strategy in STRATEGIES
            .into_iter()
            .filter(|strategy| consistent & strategy_bit(*strategy) != 0)
        {
            for (key, n) in &shares {
                deltas.quality.push(QualityDelta {
                    key: *key,
                    strategy,
                    units: Units {
                        pos: if positive { *n } else { 0 },
                        n: *n,
                    },
                });
            }
        }
    }
    deltas.non_empty()
}

/// The keys the interval visited at one level, in order of first appearance,
/// with the number of covered turns on each.
///
/// Turns, not decisions: a failover turn writes two `Routed`, but credit
/// refused those above, so here the two counts agree and turns is the rule's
/// own word.
fn visits(rows: &[(u64, LearningRow)], level: usize) -> Vec<(LevelKey, u64)> {
    let mut visits: Vec<(LevelKey, u64)> = Vec::new();
    let mut last_turn = None;
    for (turn_index, row) in rows {
        if last_turn == Some(*turn_index) {
            continue;
        }
        last_turn = Some(*turn_index);
        let key = row.keys[level];
        match visits.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, turns)) => *turns += 1,
            None => visits.push((key, 1)),
        }
    }
    visits
}

/// [`CREDIT_SCALE`] split over `counts` in proportion, the remainder one unit
/// at a time to the keys in order of first appearance.
///
/// The floors lose less than one unit per key, so the remainder is smaller
/// than the number of keys and one pass hands it out.
fn split(counts: &[(LevelKey, u64)]) -> Vec<(LevelKey, u64)> {
    let total: u64 = counts.iter().map(|(_, turns)| turns).sum();
    if total == 0 {
        return Vec::new();
    }
    let mut shares: Vec<(LevelKey, u64)> = counts
        .iter()
        .map(|(key, turns)| (*key, CREDIT_SCALE * turns / total))
        .collect();
    let mut remainder = CREDIT_SCALE - shares.iter().map(|(_, n)| n).sum::<u64>();
    for (_, n) in &mut shares {
        if remainder == 0 {
            break;
        }
        *n += 1;
        remainder -= 1;
    }
    shares
}
