// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The open review interval, folded from the log.
//!
//! A frontier review labels the routing decisions made since the previous
//! checkpoint. This fold keeps exactly those decisions, the turns they belong
//! to, and the versions they ran under, so that a review can be captured from
//! it and a recorded review can be checked against it.
//!
//! **Bounded whether or not a review ever runs.** Turns and decisions are
//! capped. Past the cap the oldest turn is dropped and the interval is marked
//! overflowed until a checkpoint covers everything that was dropped. An
//! overflowed interval can still be closed, but only as unknown: labelling the
//! retained suffix would claim a review of decisions it never saw.
//!
//! **Offsets, not copies.** A turn records where its items start in the history
//! region of [`SessionState::items`](super::SessionState::items). History is
//! append-only there, while the configuration run at its head is replaced in
//! place, so history offsets stay valid when absolute indices move.
//!
//! **One instruction snapshot, taken lazily.** The configuration a decision
//! ran under is compared at the decision against the last snapshot, and copied
//! only when it changed. A client that resends identical instructions every
//! turn pays one slice comparison per turn and no hashing.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::ids::ResponseId;
use crate::item::Item;
use crate::validate::{
    CoverageGap, IntervalLabel, IntervalReview, ObjectiveVersion, REVIEW_RULE_REVISION,
    ReviewedDecision, Verdict, label_for,
};

/// How many turns one open interval tracks before it overflows.
pub const MAX_REVIEW_TURNS: usize = 64;

/// How many routing decisions one open interval tracks before it overflows.
pub const MAX_REVIEW_DECISIONS: usize = 256;

/// How many accepted reviews the fold keeps for readers.
pub const REVIEW_OUTCOME_WINDOW: usize = 16;

/// One review the fold accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutcome {
    /// The sequence of the `ValidationDecided` event that carried the review.
    pub validation_seq: u64,
    pub rule_revision: u32,
    pub after_seq: u64,
    pub through_seq: u64,
    /// The covered `Routed` sequences, oldest first.
    pub decisions: Vec<u64>,
    /// The label as written, or [`IntervalLabel::Unknown`] when this build
    /// could not verify the review's membership.
    pub label: IntervalLabel,
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnEnd {
    Completed,
    Incomplete,
}

/// A turn's place in the review section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnState {
    Ended(TurnEnd),
    /// The most recent turn, still open: context for the review, never labelled.
    InProgress,
    /// A later turn started while this one was open, so its owner is gone.
    Abandoned,
}

#[derive(Debug, Clone)]
struct TrackedDecision {
    seq: u64,
    /// The instruction generation the decision ran under.
    generation: u64,
    objective: Option<ObjectiveVersion>,
}

#[derive(Debug, Clone)]
struct TurnSpan {
    turn_index: u64,
    response_id: ResponseId,
    started_seq: u64,
    /// Where this turn's items start in the history region.
    history_from: usize,
    /// The history offset of the last user request before this turn.
    request_before: Option<usize>,
    decisions: Vec<TrackedDecision>,
    ended: Option<(u64, TurnEnd)>,
}

impl TurnSpan {
    /// The highest sequence this span is known to reach.
    fn last_seq(&self) -> u64 {
        let decided = self.decisions.last().map_or(0, |decision| decision.seq);
        let ended = self.ended.map_or(0, |(seq, _)| seq);
        self.started_seq.max(decided).max(ended)
    }
}

/// The instructions decisions ran under, as generations of one snapshot.
#[derive(Debug)]
struct Instructions {
    /// A configuration item arrived since the last snapshot.
    dirty: bool,
    generation: u64,
    /// The content of `generation`.
    snapshot: Arc<[Item]>,
}

impl Default for Instructions {
    fn default() -> Self {
        Self {
            dirty: true,
            generation: 0,
            snapshot: Arc::from(Vec::new()),
        }
    }
}

impl Instructions {
    /// The generation `configuration` belongs to, copying it only on change.
    fn generation_of(&mut self, configuration: &[Item]) -> u64 {
        if self.dirty {
            self.dirty = false;
            if *self.snapshot != *configuration {
                self.generation += 1;
                self.snapshot = Arc::from(configuration.to_vec());
            }
        }
        self.generation
    }
}

/// Review-interval state for one session.
#[derive(Debug, Default)]
pub(crate) struct ReviewTracker {
    /// Only sessions whose arm consults a judge can close an interval, so only
    /// they track one.
    enabled: bool,
    checkpoint: u64,
    spans: VecDeque<TurnSpan>,
    decisions: usize,
    /// The highest sequence dropped for the bound. Sticky until a checkpoint
    /// reaches it.
    overflow_through: Option<u64>,
    instructions: Instructions,
    last_user_request: Option<usize>,
    accepted: u64,
    rejected: u64,
    outcomes: Vec<ReviewOutcome>,
}

/// What the fold can say about a candidate interval before any rendering.
pub(crate) struct IntervalFacts<'a> {
    pub(crate) after_seq: u64,
    pub(crate) decisions: Vec<ReviewedDecision>,
    pub(crate) gaps: Vec<CoverageGap>,
    /// The one objective every covered decision recorded, when they agree.
    pub(crate) objective: Option<&'a ObjectiveVersion>,
    /// The instructions every covered decision ran under, when they agree.
    pub(crate) instructions: Option<&'a [Item]>,
    pub(crate) turns: Vec<ReviewTurn<'a>>,
    /// The last user request before the first turn, when that turn has none.
    pub(crate) request_before: Option<&'a str>,
    /// History before the first turn, where a result's call may have been made.
    pub(crate) prefix: &'a [Item],
}

/// One turn's items and state, for rendering.
pub(crate) struct ReviewTurn<'a> {
    pub(crate) items: &'a [Item],
    pub(crate) state: TurnState,
}

impl ReviewTracker {
    pub(crate) fn enable(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(crate) fn checkpoint(&self) -> u64 {
        self.checkpoint
    }

    pub(crate) fn accepted(&self) -> u64 {
        self.accepted
    }

    pub(crate) fn rejected(&self) -> u64 {
        self.rejected
    }

    pub(crate) fn outcomes(&self) -> &[ReviewOutcome] {
        &self.outcomes
    }

    pub(crate) fn pending(&self) -> impl Iterator<Item = u64> + '_ {
        self.spans
            .iter()
            .flat_map(|span| span.decisions.iter().map(|decision| decision.seq))
    }

    pub(crate) fn turn_started(
        &mut self,
        seq: u64,
        turn_index: u64,
        response_id: &ResponseId,
        history_len: usize,
    ) {
        if !self.enabled {
            return;
        }
        if self.spans.len() >= MAX_REVIEW_TURNS {
            self.drop_oldest();
        }
        self.spans.push_back(TurnSpan {
            turn_index,
            response_id: response_id.clone(),
            started_seq: seq,
            history_from: history_len,
            request_before: self.last_user_request,
            decisions: Vec::new(),
            ended: None,
        });
    }

    /// A configuration item was folded in.
    pub(crate) fn configuration_appended(&mut self) {
        self.instructions.dirty = true;
    }

    /// A history item was folded in at `offset`.
    pub(crate) fn history_appended(&mut self, offset: usize, is_user_request: bool) {
        if self.enabled && is_user_request {
            self.last_user_request = Some(offset);
        }
    }

    pub(crate) fn routed(
        &mut self,
        seq: u64,
        response_id: &ResponseId,
        objective: Option<&ObjectiveVersion>,
        configuration: &[Item],
    ) {
        if !self.enabled {
            return;
        }
        let generation = self.instructions.generation_of(configuration);
        while self.decisions >= MAX_REVIEW_DECISIONS && self.spans.len() > 1 {
            self.drop_oldest();
        }
        let span = self
            .spans
            .iter_mut()
            .rev()
            .find(|span| span.response_id == *response_id);
        match span {
            Some(span) if self.decisions < MAX_REVIEW_DECISIONS => {
                span.decisions.push(TrackedDecision {
                    seq,
                    generation,
                    objective: objective.cloned(),
                });
                self.decisions += 1;
            }
            // A decision this interval cannot hold: no turn of its own in the
            // tracked window, or no room. It is still a decision nobody has
            // reviewed, so the interval cannot be complete until a checkpoint
            // passes it.
            _ => self.mark_overflow(seq),
        }
    }

    pub(crate) fn ended(&mut self, seq: u64, response_id: &ResponseId, end: TurnEnd) {
        if !self.enabled {
            return;
        }
        if let Some(span) = self
            .spans
            .iter_mut()
            .rev()
            .find(|span| span.response_id == *response_id)
        {
            span.ended.get_or_insert((seq, end));
        }
    }

    /// A judged validation was folded in at `event_seq`.
    pub(crate) fn judged(
        &mut self,
        event_seq: u64,
        verdict: &Verdict,
        review: Option<&IntervalReview>,
    ) {
        if !self.enabled {
            return;
        }
        let Some(review) = review else {
            // Written before coverage existed: a real review of everything
            // before it, whose coverage nobody recorded. It closes the interval
            // and labels nothing.
            self.advance(event_seq);
            return;
        };
        match self.verify(event_seq, verdict, review) {
            Some(verified) => {
                self.accepted += 1;
                self.outcomes.push(ReviewOutcome {
                    validation_seq: event_seq,
                    rule_revision: review.rule_revision,
                    after_seq: review.after_seq,
                    through_seq: review.through_seq,
                    decisions: review.decisions.iter().map(|d| d.routed_seq).collect(),
                    label: match verified {
                        true => review.label,
                        false => IntervalLabel::Unknown,
                    },
                });
                if self.outcomes.len() > REVIEW_OUTCOME_WINDOW {
                    self.outcomes.remove(0);
                }
                self.advance(review.through_seq);
            }
            None => self.rejected += 1,
        }
    }

    /// Whether `review` may close the open interval: `Some(true)` when its
    /// membership and label are verified, `Some(false)` when only its bounds
    /// are, and `None` when it must be rejected.
    fn verify(&self, event_seq: u64, verdict: &Verdict, review: &IntervalReview) -> Option<bool> {
        let (after, through) = (review.after_seq, review.through_seq);
        // Continuity rejects duplicates, overlaps and gaps; the upper bound
        // rejects a capture that claims to have seen its own record.
        if after != self.checkpoint || through < after || through >= event_seq {
            return None;
        }
        let ascending = review
            .decisions
            .windows(2)
            .all(|pair| pair[0].routed_seq < pair[1].routed_seq);
        let in_range = review
            .decisions
            .iter()
            .all(|decision| decision.routed_seq > after && decision.routed_seq <= through);
        if !ascending || !in_range {
            return None;
        }
        // No revision may label an interval it did not see completely.
        if review.label != IntervalLabel::Unknown
            && (!review.gaps.is_empty() || review.decisions.is_empty())
        {
            return None;
        }
        let current = review.rule_revision == REVIEW_RULE_REVISION;
        let claims_overflow = review.gaps.contains(&CoverageGap::MetadataOverflow);
        let overflowed = self.overflow_through.is_some();
        if claims_overflow {
            // Bounds and label are all an overflowed review can be checked on.
            // The current build refuses an overflow it did not itself see.
            return (overflowed || !current).then_some(true);
        }
        if overflowed {
            // Membership cannot be verified past the bound. A review written
            // under other bounds is kept as a checkpoint without its label.
            return (!current).then_some(false);
        }
        let expected: Vec<ReviewedDecision> = self.covered(after, through).collect();
        if expected != review.decisions {
            return None;
        }
        if current {
            let derived = self.gaps(after, through);
            if !derived.iter().all(|gap| review.gaps.contains(gap))
                || review.label != label_for(&review.gaps, verdict)
            {
                return None;
            }
        }
        Some(true)
    }

    /// `(span, decision)` pairs for every decision in `(after, through]`,
    /// oldest first.
    ///
    /// The one filter [`Self::covered`], [`Self::gaps`] and [`Self::facts`]
    /// all need, so a rule about which decisions an interval covers has one
    /// spelling to update rather than three that could drift apart.
    fn covered_spans(
        &self,
        after: u64,
        through: u64,
    ) -> impl Iterator<Item = (&TurnSpan, &TrackedDecision)> + '_ {
        self.spans.iter().flat_map(move |span| {
            span.decisions
                .iter()
                .filter(move |decision| decision.seq > after && decision.seq <= through)
                .map(move |decision| (span, decision))
        })
    }

    /// Tracked decisions in `(after, through]`, oldest first.
    fn covered(&self, after: u64, through: u64) -> impl Iterator<Item = ReviewedDecision> + '_ {
        self.covered_spans(after, through)
            .map(|(span, decision)| ReviewedDecision {
                routed_seq: decision.seq,
                turn_index: span.turn_index,
                response_id: span.response_id.clone(),
            })
    }

    /// The gaps `(after, through]` alone establishes, from the decisions the
    /// interval itself covers.
    ///
    /// **Reads one piece of tip state, `self.overflow_through`, and it is
    /// safe to.** `verify` -- the only caller besides capture -- returns
    /// before it ever reaches this call while the fold is overflowed, so the
    /// two never disagree about what "overflowed" means; capture always runs
    /// at the tip, so its own read is current by construction.
    /// `self.instructions.generation` is tip state too and stays out for the
    /// opposite reason: comparing a covered decision's generation against
    /// *now* would make this depend on whatever landed after the interval
    /// closed, and a review captured correctly could come to look incomplete
    /// only because the log kept moving underneath it.
    fn gaps(&self, after: u64, through: u64) -> Vec<CoverageGap> {
        let mut gaps = Vec::new();
        if self.overflow_through.is_some() {
            gaps.push(CoverageGap::MetadataOverflow);
        }
        let mut any = false;
        let mut unterminated = false;
        let mut unstamped = false;
        let mut generation = None;
        let mut generations_differ = false;
        let mut objective: Option<&ObjectiveVersion> = None;
        let mut objectives_differ = false;
        for (span, decision) in self.covered_spans(after, through) {
            any = true;
            unterminated |= span.ended.is_none_or(|(seq, _)| seq > through);
            generations_differ |=
                *generation.get_or_insert(decision.generation) != decision.generation;
            match &decision.objective {
                None => unstamped = true,
                Some(stamp) => {
                    objectives_differ |= *objective.get_or_insert(stamp) != stamp;
                }
            }
        }
        if !any && gaps.is_empty() {
            gaps.push(CoverageGap::NoDecisions);
        }
        if unterminated {
            gaps.push(CoverageGap::UnterminatedTurn);
        }
        if unstamped {
            gaps.push(CoverageGap::VersionsUnavailable);
        }
        if generations_differ {
            gaps.push(CoverageGap::InstructionsChanged);
        }
        if objectives_differ {
            gaps.push(CoverageGap::ObjectiveChanged);
        }
        gaps.sort();
        gaps.dedup();
        gaps
    }

    /// The open interval as a review captured at `through` would see it.
    pub(crate) fn facts<'a>(&'a self, history: &'a [Item], through: u64) -> IntervalFacts<'a> {
        let after = self.checkpoint;
        let decisions: Vec<ReviewedDecision> = self.covered(after, through).collect();
        let gaps = self.gaps(after, through);
        let first = self
            .covered_spans(after, through)
            .next()
            .map(|(_, decision)| decision);
        let objective = first.and_then(|decision| decision.objective.as_ref());
        // The snapshot is safe to show only while it is still what the first
        // covered decision ran under. A mismatch here is not a gap this
        // method adds on its own: `gaps()` above is a pure function of
        // `(after, through]` and never reads `self.instructions.generation`,
        // so at capture -- the only time `facts` runs, with `through` at the
        // tip -- a mismatch can only mean the `Routed` that last set the
        // generation is one of two things: a later covered decision whose own
        // generation already disagrees with the first (`InstructionsChanged`
        // above already fired), or one dropped from tracking
        // (`MetadataOverflow`). Either way `gaps` is not empty.
        //
        // `Self::routed` is the only thing that moves the generation, and it
        // is the whole guard: it either tracks the decision -- covered,
        // since at capture `through` is the tip and every tracked decision
        // is past the checkpoint -- or falls to `mark_overflow`, which is
        // `MetadataOverflow`. Keep that `mark_overflow` arm: a `routed()`
        // that silently dropped an unknown response instead is the one
        // change that would make the assertion below fire.
        let instructions = match first {
            Some(decision) if decision.generation == self.instructions.generation => {
                Some(&*self.instructions.snapshot)
            }
            Some(_) => {
                debug_assert!(
                    gaps.contains(&CoverageGap::MetadataOverflow)
                        || gaps.contains(&CoverageGap::InstructionsChanged),
                    "a stale instruction snapshot at capture with neither gap means a \
                     generation-bumping Routed landed that this interval neither covers \
                     nor lost to overflow"
                );
                None
            }
            None => None,
        };

        let last = self.spans.len().saturating_sub(1);
        let turns = self
            .spans
            .iter()
            .enumerate()
            .map(|(index, span)| {
                let end = self
                    .spans
                    .get(index + 1)
                    .map_or(history.len(), |next| next.history_from);
                ReviewTurn {
                    items: history.get(span.history_from..end).unwrap_or(&[]),
                    state: match span.ended {
                        Some((_, end)) => TurnState::Ended(end),
                        None if index == last => TurnState::InProgress,
                        None => TurnState::Abandoned,
                    },
                }
            })
            .collect::<Vec<_>>();
        let first_has_request = turns
            .first()
            .is_some_and(|turn| turn.items.iter().any(|item| item.user_request().is_some()));
        let request_before = match first_has_request {
            true => None,
            false => self
                .spans
                .front()
                .and_then(|span| span.request_before)
                .and_then(|offset| history.get(offset))
                .and_then(Item::user_request),
        };
        let from = self
            .spans
            .front()
            .map_or(history.len(), |span| span.history_from);
        IntervalFacts {
            after_seq: after,
            decisions,
            gaps,
            objective,
            instructions,
            turns,
            request_before,
            prefix: history.get(..from).unwrap_or(history),
        }
    }

    /// Close the interval through `through`.
    fn advance(&mut self, through: u64) {
        self.checkpoint = through;
        let mut removed = 0;
        for span in &mut self.spans {
            let before = span.decisions.len();
            span.decisions.retain(|decision| decision.seq > through);
            removed += before - span.decisions.len();
        }
        self.decisions -= removed;
        // Leading turns wholly before the checkpoint leave; the first turn that
        // is still open, or that has a later decision, stays with everything
        // after it — its input is what the next review shows with its decision.
        while let Some(span) = self.spans.front() {
            let closed = match span.ended {
                Some((seq, _)) => seq <= through,
                None => self
                    .spans
                    .get(1)
                    .is_some_and(|next| next.started_seq <= through),
            };
            if !closed || !span.decisions.is_empty() || span.started_seq > through {
                break;
            }
            self.spans.pop_front();
        }
        if self
            .overflow_through
            .is_some_and(|dropped| dropped <= through)
        {
            self.overflow_through = None;
        }
    }

    fn drop_oldest(&mut self) {
        if let Some(span) = self.spans.pop_front() {
            self.decisions -= span.decisions.len();
            self.mark_overflow(span.last_seq());
        }
    }

    fn mark_overflow(&mut self, seq: u64) {
        self.overflow_through = Some(
            self.overflow_through
                .map_or(seq, |dropped| dropped.max(seq)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two turns tracked and covered under one configuration, then a
    /// `Routed` naming a response no `turn_started` ever opened -- exactly
    /// the shape `Session::record_routing` can write for any response id,
    /// whatever the caller passes, since nothing upstream of this fold
    /// checks that a response was ever admitted. `routed()`'s
    /// `_ => mark_overflow` arm is the one guard that keeps this from
    /// reaching `facts()` as a silent mismatch: the covered decisions still
    /// name the generation they ran under, the tip's own generation moves on
    /// regardless, and the recorded gap is why the two are allowed to
    /// disagree.
    #[test]
    fn a_routed_response_with_no_span_marks_overflow_not_a_silent_mismatch() {
        let mut tracker = ReviewTracker::default();
        tracker.enable(true);

        let config_a = [Item::user_text("config-a")];
        let config_b = [Item::user_text("config-b")];
        let objective = ObjectiveVersion::Undeclared;

        let r0 = ResponseId::new("r0");
        tracker.turn_started(1, 0, &r0, 0);
        tracker.routed(2, &r0, Some(&objective), &config_a);
        tracker.ended(3, &r0, TurnEnd::Completed);

        let r1 = ResponseId::new("r1");
        tracker.turn_started(4, 1, &r1, 0);
        tracker.routed(5, &r1, Some(&objective), &config_a);
        tracker.ended(6, &r1, TurnEnd::Completed);

        // The configuration changed, but nothing has routed under it yet --
        // `dirty` is set, `generation` has not moved.
        tracker.configuration_appended();

        // A `Routed` for a response with no span: `routed()` still resolves
        // the new generation before the span lookup fails, so the tip moves
        // to generation 2 while both covered decisions above still name 1.
        let ghost = ResponseId::new("ghost");
        tracker.routed(7, &ghost, Some(&objective), &config_b);

        let facts = tracker.facts(&[], 7);
        assert!(
            facts.gaps.contains(&CoverageGap::MetadataOverflow),
            "the ghost response has no span, so routed() must mark overflow \
             rather than drop it silently: {:?}",
            facts.gaps
        );
        assert!(
            !facts.gaps.contains(&CoverageGap::VersionsUnavailable),
            "both covered decisions stamped their objective: {:?}",
            facts.gaps
        );
        assert_eq!(
            facts.instructions, None,
            "the first covered decision's generation (1) disagrees with the \
             tip's (2, bumped by the ghost's own routed() call); \
             MetadataOverflow is why facts() may say so without its debug \
             assertion firing"
        );
    }
}
