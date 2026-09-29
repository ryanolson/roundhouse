// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The served tier against the classifier's tier pick, folded off the log.
//!
//! The owner's question is whether the classifier is worth what it costs
//! (2026-09-28 addendum, "Jev as a scout", item 2). The answer needs three
//! facts about one turn, and they arrive on three different events at three
//! different times:
//!
//! - the **served tier**, on the turn's last `Routed`, as the recipe tier of the
//!   dispatched target ([`StageEvidence::tier_of`]);
//! - the **classifier's pick**, on a `ClassificationRecorded` a later turn's
//!   writer delivers;
//! - the **review label**, on a `ValidationDecided` whose interval covers the
//!   turn.
//!
//! **The review can land before the answer.** A classifier result is delivered
//! at the start of a later turn, and the validation runs inside the turn before
//! that, so a slow classifier sees its turn reviewed first. A join that only
//! waited for labels after the answer would leave exactly those disagreements
//! unlabelled forever, and the unlabelled count is one the owner reads. So the
//! fold opens a slot for the turn at the classification *intent*, which the
//! engine writes after the turn's terminal and before any later turn, and a
//! review fills the slot's label whichever of the two arrives first.
//!
//! **Never a reward.** Nothing here is read by routing or by any learner. These
//! are counts on a report.
//!
//! ## Retention
//!
//! Per session, at most [`MAX_REVIEW_DECISIONS`] open slots: intents whose
//! answer has not landed, and disagreements whose label has not. The bound caps
//! what one session holds in this fold, which otherwise grows with every
//! classified turn that is never answered or never reviewed. It borrows the
//! value of the review tracker's per-interval decision bound because one review
//! covers at most that many decisions, but it is not the tracker's guarantee: a
//! review that arrives late can still name a dropped turn. So nothing assumes
//! eviction is harmless. Past the bound the oldest slot is dropped, a dropped
//! disagreement is counted as evicted so the report says how many it stopped
//! waiting for, and a dropped intent whose answer lands later is counted as
//! not comparable.
//!
//! [`AgreementFold::served`] holds one entry per session, the latest route, and
//! grows with the fold's watermarks for the same reason they do.

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::classify::{ClassificationIntent, ClassificationRecord, TierChoice};
use crate::control::PrincipalKey;
use crate::ids::{ResponseId, SessionId};
use crate::metrics::fold::Scope;
use crate::routing::{DecisionRecord, SelectorBranch, Tier};
use crate::session::MAX_REVIEW_DECISIONS;
use crate::validate::{IntervalLabel, IntervalReview};

/// One principal's agreement counts, add-only.
///
/// `disagree` and `unlabeled` are not stored: they are derived on the wire from
/// the directions and the labels, so the two partitions the report promises
/// hold by construction rather than by every booking site agreeing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AgreementCounts {
    pub(crate) answered: u64,
    pub(crate) agree: u64,
    pub(crate) not_comparable: u64,
    pub(crate) jev_capable_served_efficient: u64,
    pub(crate) jev_efficient_served_capable: u64,
    pub(crate) positive: u64,
    pub(crate) negative: u64,
    pub(crate) unknown: u64,
    pub(crate) evicted: u64,
}

impl AgreementCounts {
    fn absorb(&mut self, other: &Self) {
        self.answered += other.answered;
        self.agree += other.agree;
        self.not_comparable += other.not_comparable;
        self.jev_capable_served_efficient += other.jev_capable_served_efficient;
        self.jev_efficient_served_capable += other.jev_efficient_served_capable;
        self.positive += other.positive;
        self.negative += other.negative;
        self.unknown += other.unknown;
        self.evicted += other.evicted;
    }

    fn label(&mut self, label: IntervalLabel) {
        match label {
            IntervalLabel::Positive => self.positive += 1,
            IntervalLabel::Negative => self.negative += 1,
            IntervalLabel::Unknown => self.unknown += 1,
        }
    }
}

/// What one accepted answer books.
enum Booking {
    NotComparable,
    Agree,
    Disagree(TierChoice, Option<IntervalLabel>),
}

/// One classified turn the fold is still waiting on.
#[derive(Debug)]
struct Slot {
    response_id: ResponseId,
    /// The recipe tier the turn was served on, or `None` when it has none.
    served: Option<Tier>,
    state: SlotState,
}

/// What a slot waits for.
///
/// An enum rather than a label and a flag, because a disagreement waiting on a
/// label cannot also hold one: the review that labels it books the label and
/// frees the slot in the same step.
#[derive(Debug, Clone, Copy)]
enum SlotState {
    /// No answer yet. `label` is the covering review's, when a review landed
    /// first.
    AwaitingAnswer { label: Option<IntervalLabel> },
    /// The answer landed and disagreed, and no review has covered the turn.
    AwaitingLabel,
}

#[derive(Default)]
pub(super) struct AgreementFold {
    by_principal: BTreeMap<PrincipalKey, AgreementCounts>,
    /// Each session's latest route: its response and served tier.
    ///
    /// Only the latest, because the engine writes a turn's intent after the
    /// turn's terminal and before the next turn starts, so the route an intent
    /// is about is always the session's most recent one. A failover writes
    /// several `Routed` for one response, and the last is the one that served.
    served: HashMap<SessionId, (ResponseId, Option<Tier>)>,
    open: HashMap<SessionId, VecDeque<Slot>>,
}

impl AgreementFold {
    /// A dispatch. The last one for a response is the one that served.
    pub(super) fn routed(
        &mut self,
        session: &SessionId,
        response_id: &ResponseId,
        decision: &DecisionRecord,
    ) {
        let tier = served_tier(decision);
        // In place when the session already has an entry: every turn after the
        // first, and every failover of a turn. `clone_from` reuses the id's
        // buffer, so nothing is allocated for either.
        if let Some((latest, served)) = self.served.get_mut(session) {
            latest.clone_from(response_id);
            *served = tier;
        } else {
            self.served
                .insert(session.clone(), (response_id.clone(), tier));
        }
    }

    /// A classification the evaluation join accepted as a new call.
    pub(super) fn requested(
        &mut self,
        session: &SessionId,
        payer: &PrincipalKey,
        record: &ClassificationIntent,
    ) {
        let served = match self.served.get(session) {
            Some((response_id, tier)) if *response_id == record.source_response_id => *tier,
            _ => None,
        };
        let slots = self.open.entry(session.clone()).or_default();
        slots.push_back(Slot {
            response_id: record.source_response_id.clone(),
            served,
            state: SlotState::AwaitingAnswer { label: None },
        });
        if slots.len() > MAX_REVIEW_DECISIONS
            && let Some(dropped) = slots.pop_front()
            && matches!(dropped.state, SlotState::AwaitingLabel)
        {
            self.row(payer).evicted += 1;
        }
    }

    /// A result the evaluation join accepted. Books the answer against the
    /// served tier, once.
    ///
    /// A disagreement with no label yet stays where it is in its session's
    /// slots, so the bound evicts in the order the turns were classified.
    pub(super) fn recorded(
        &mut self,
        session: &SessionId,
        payer: &PrincipalKey,
        record: &ClassificationRecord,
    ) {
        let answer = record
            .outcome
            .classification()
            .and_then(|classification| classification.tier)
            .map(|graded| graded.value);
        // The slot comes out once, here. Only the arm that keeps waiting puts
        // it back, and at the index it came from, so the bound still evicts in
        // the order the turns were classified.
        let found = self.open.get_mut(session).and_then(|slots| {
            let at = slots
                .iter()
                .position(|slot| slot.response_id == record.source_response_id)?;
            let slot = slots.remove(at)?;
            Some((slots, at, slot))
        });
        let booking = match (answer, found) {
            // No tier answer: a taxonomy-1 record, or no usable answer at all.
            // The slot has nothing left to wait for.
            (None, _) => None,
            // The slot was evicted, or no intent opened one.
            (Some(_), None) => Some(Booking::NotComparable),
            (Some(answer), Some((slots, at, slot))) => match (slot.served, slot.state) {
                // An answer for a slot that was already answered. Unreachable
                // today: the engine mints one call per response and the
                // evaluation join accepts one result per call. Should that
                // ever change, the slot goes back untouched and nothing is
                // booked twice.
                (_, SlotState::AwaitingLabel) => {
                    slots.insert(at, slot);
                    None
                }
                (None, _) => Some(Booking::NotComparable),
                (Some(served), _) if served == answer.tier() => Some(Booking::Agree),
                (Some(_), SlotState::AwaitingAnswer { label: Some(label) }) => {
                    Some(Booking::Disagree(answer, Some(label)))
                }
                (Some(_), SlotState::AwaitingAnswer { label: None }) => {
                    slots.insert(
                        at,
                        Slot {
                            state: SlotState::AwaitingLabel,
                            ..slot
                        },
                    );
                    Some(Booking::Disagree(answer, None))
                }
            },
        };
        self.prune(session);
        let Some(booking) = booking else {
            return;
        };
        let row = self.row(payer);
        row.answered += 1;
        match booking {
            Booking::NotComparable => row.not_comparable += 1,
            Booking::Agree => row.agree += 1,
            Booking::Disagree(answer, label) => {
                match answer {
                    TierChoice::Capable => row.jev_capable_served_efficient += 1,
                    TierChoice::Efficient => row.jev_efficient_served_capable += 1,
                }
                if let Some(label) = label {
                    row.label(label);
                }
            }
        }
    }

    /// A review's label, taken as written, for every turn it covered.
    ///
    /// The first covering review wins: review intervals are contiguous and do
    /// not overlap, so a second one naming the same turn is a malformed log,
    /// and letting it relabel would count one disagreement twice.
    pub(super) fn reviewed(
        &mut self,
        session: &SessionId,
        payer: &PrincipalKey,
        review: &IntervalReview,
    ) {
        let Some(slots) = self.open.get_mut(session) else {
            return;
        };
        let mut labelled = Vec::new();
        for decision in &review.decisions {
            let unlabelled = |slot: &Slot| {
                slot.response_id == decision.response_id
                    && matches!(
                        slot.state,
                        SlotState::AwaitingAnswer { label: None } | SlotState::AwaitingLabel
                    )
            };
            if let Some(at) = slots.iter().position(unlabelled) {
                match slots[at].state {
                    SlotState::AwaitingLabel => {
                        slots.remove(at);
                        labelled.push(review.label);
                    }
                    SlotState::AwaitingAnswer { .. } => {
                        slots[at].state = SlotState::AwaitingAnswer {
                            label: Some(review.label),
                        };
                    }
                }
            }
        }
        self.prune(session);
        let row = self.row(payer);
        for label in labelled {
            row.label(label);
        }
    }

    pub(super) fn tally(&self, scope: Scope<'_>) -> AgreementCounts {
        let mut total = AgreementCounts::default();
        for (owner, counts) in &self.by_principal {
            if scope.collects(owner) {
                total.absorb(counts);
            }
        }
        total
    }

    /// Drop a session's slot list once it is empty, so a finished session
    /// holds nothing here.
    fn prune(&mut self, session: &SessionId) {
        if self.open.get(session).is_some_and(VecDeque::is_empty) {
            self.open.remove(session);
        }
    }

    fn row(&mut self, payer: &PrincipalKey) -> &mut AgreementCounts {
        self.by_principal.entry(payer.clone()).or_default()
    }
}

/// The recipe tier a dispatch was served on, or `None` for a decision no tier
/// recipe made.
fn served_tier(decision: &DecisionRecord) -> Option<Tier> {
    let selector = decision.selection.as_ref()?.selector.as_ref()?;
    match &selector.branch {
        SelectorBranch::Stage(evidence) => evidence.tier_of(&decision.chosen),
        // The same recipe lists, held once for every plan: a learned turn
        // served a target the recipe names, and its tier is read the same way.
        SelectorBranch::Learned(evidence) => evidence.tier_of(&decision.chosen),
        SelectorBranch::Affinity(_) | SelectorBranch::EscalationAudit { .. } => None,
    }
}
