// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Learning entries, folded from the log (draft sections 8 and 11 of
//! `agent-docs/DRAFT-online-routing-learner.md`, milestone M5 of
//! `agent-docs/PLAN-online-routing-learner.md`).
//!
//! **Which entries exist is decided by the event kind alone.** After the first
//! `Routed` of the session that carries learned evidence, every
//! [`produces_entry`] event is exactly one entry, even when it adds nothing.
//! Credit and review rules decide the deltas and never the existence, so a
//! newer build that replays an older session produces the same chain, and the
//! learner store's `prev_seq` check can refuse a batch that skips one. The
//! list is a one-way door: changing it needs a new `REVIEW_RULE_REVISION`.
//!
//! Three kinds of deltas, each a pure function of the log prefix:
//!
//! - **Credit** ([`credit`]), when the review tracker accepts a labelled
//!   review.
//! - **Operational rows**, at a turn's terminal event: the latency residual
//!   from the served `Routed` to the first output, less the quote; the
//!   overhead from `TurnStarted` to that `Routed`, from the same completed
//!   turns; a failover count; and a cache reuse sample when the provider
//!   measured it.
//! - **Jev counts**, when an accepted classification of a learned turn has a
//!   tier answer: one count on each of that turn's three keys. A prior, never
//!   a reward.
//!
//! **The cursor is a hint, not a loss policy.** The fold holds at most
//! [`LEARNING_PAGE`] entries above the last `LearningApplied`, and counts the
//! rest. The learner store's watermark is the authority; a backfill replay
//! ([`SessionState::project_learning`](super::SessionState::project_learning))
//! refills the page from it, and holds every entry above that watermark even
//! when the log's hint is higher (see [`Hold`]).
//!
//! **Marks.** [`learning_mark`] tells `Session::commit` which event of a batch
//! the source store must record as the session's learning mark, so every
//! durable entry-producing event is discoverable by recovery (draft section
//! 11.7). It reads the same [`produces_entry`] list the fold does.

mod credit;
mod entry;

use crate::classify::{ClassificationIntent, ClassificationRecord};
use crate::event::{SessionEventKind, Usage};
use crate::ids::ResponseId;
use crate::metrics::cache_evidence::measured_pair;
use crate::routing::learn::{CacheReuse, JevCounts, LEARNING_CREDIT_REVISION, LatencySum};
use crate::routing::{DecisionRecord, SelectorBranch, Target};
use crate::store::LearningMark;
use crate::validate::REVIEW_RULE_REVISION;

pub(crate) use credit::{CoveredRow, LearningRow, Reviewed};
pub use entry::{
    Deltas, JevDelta, LEARNING_PAGE, LearningCauses, LearningEntry, QualityDelta, TargetDelta,
};

use super::SessionState;

/// Whether an event of this kind is a learning entry, once the session has
/// learned evidence.
///
/// **The one spelling of the list**, read by the fold's existence rule and by
/// [`learning_mark`], so the store index and the entry chain cannot disagree
/// about which events a learner is owed.
pub(crate) fn produces_entry(kind: &SessionEventKind) -> bool {
    matches!(
        kind,
        SessionEventKind::ValidationDecided { .. }
            | SessionEventKind::ResponseCompleted { .. }
            | SessionEventKind::ResponseIncomplete { .. }
            | SessionEventKind::ClassificationRecorded { .. }
    )
}

/// Whether a `Routed` of this decision carries learned evidence, of any
/// revision: existence does not depend on the credit rule.
fn is_learned(kind: &SessionEventKind) -> bool {
    match kind {
        SessionEventKind::Routed { decision, .. } => decision
            .selection
            .as_deref()
            .and_then(|selection| selection.selector.as_ref())
            .is_some_and(|selector| matches!(selector.branch, SelectorBranch::Learned(_))),
        _ => false,
    }
}

/// The mark an append of `kinds` onto a session in `state` must carry: the
/// newest entry-producing event of the batch, under the session's project.
///
/// `None` when the batch holds no entry-producing event after learned
/// evidence (the session's, or a learned `Routed` earlier in the same batch),
/// and for a session with no principal, which never uses the learner.
///
/// The newest, because one mark covers every entry through it: the store's
/// clear predicate is `mark <= confirmed watermark`.
pub fn learning_mark(state: &SessionState, kinds: &[SessionEventKind]) -> Option<LearningMark> {
    let project = &state.principal.as_ref()?.project;
    let mut started = state.learning.started;
    let mut newest = None;
    for (index, kind) in kinds.iter().enumerate() {
        started |= is_learned(kind);
        if started && produces_entry(kind) {
            newest = Some(index);
        }
    }
    newest.map(|index| LearningMark::new(index, project.clone()))
}

/// The dispatch that served a turn, as its operational rows need it.
#[derive(Debug)]
struct Served {
    /// The decision's row, under this build's credit rule: any other turn
    /// adds no operational rows and is not tracked.
    row: LearningRow,
    target: Target,
    routed_at_ms: u64,
    /// The quoted TTFT of the served candidate, when the decision priced it.
    quoted_ttft_ms: Option<f64>,
    isl_tokens: u64,
    expected_prefill_tokens: f64,
}

/// The one open turn's facts, from `TurnStarted` to its terminal event.
///
/// One turn, not a map: a session's turns are serialized, and a `TurnStarted`
/// while one is open means its owner is gone.
#[derive(Debug)]
struct OpenTurn {
    response_id: ResponseId,
    started_at_ms: u64,
    /// The first target this turn dispatched to, and how many dispatches it
    /// made.
    first: Option<Target>,
    dispatches: u32,
    served: Option<Served>,
    /// The first non-empty output after the served `Routed`.
    first_output_at_ms: Option<u64>,
}

/// A learned turn's row, waiting for the classification of that turn.
#[derive(Debug)]
struct AwaitingAnswer {
    call_id: ResponseId,
    expires_at_ms: u64,
    row: LearningRow,
}

/// Which entries the page holds: the two replays that build a fold.
///
/// Two variants rather than `hint.max(floor)`, because the two thresholds
/// disagree exactly when it matters. The learner store can hold less than the
/// log acknowledged: it lost recent writes (draft 11.6), or the audit found
/// its watermark below the mark (11.7). A backfill from that watermark that
/// also honored the hint would skip the entries between the two, the store
/// would answer `ChainGap` with the same watermark, and delivery for the
/// session would stall for good.
#[derive(Debug, Default, Clone, Copy)]
enum Hold {
    /// The live fold and a plain replay: entries above the highest
    /// `LearningApplied`, which also prunes the page.
    #[default]
    Hint,
    /// A backfill: every entry above the learner store's watermark.
    /// `LearningApplied` still moves the hint but neither holds nor prunes.
    Floor(u64),
}

/// Learning state for one session.
#[derive(Debug, Default)]
pub(crate) struct LearningFold {
    /// A learned `Routed` has been folded: from here on every
    /// entry-producing event is an entry.
    started: bool,
    /// The `seq` of the newest entry, held or not: the next entry's
    /// `prev_seq`.
    last_entry: u64,
    /// The highest `through_seq` of a `LearningApplied`.
    hint: u64,
    /// Which entries the page holds.
    hold: Hold,
    page: Vec<LearningEntry>,
    /// Entries above the [`Hold`] threshold that the page does not hold.
    /// Exact until a `LearningApplied` reaches past the page without reaching
    /// the newest entry; from then an upper bound, which only costs a backfill
    /// that finds the true set. It is never an undercount, which would lose
    /// one.
    beyond: u64,
    causes: LearningCauses,
    open: Option<OpenTurn>,
    /// The newest learned turn's row, for the classification intent that
    /// follows its terminal event.
    last_learned: Option<(ResponseId, LearningRow)>,
    /// Rows waiting for an answer, in intent order.
    awaiting: Vec<AwaitingAnswer>,
}

impl LearningFold {
    /// A backfill fold: its page holds every entry above `floor`, whatever
    /// `LearningApplied` the log records.
    pub(crate) fn with_floor(floor: u64) -> Self {
        Self {
            hold: Hold::Floor(floor),
            ..Self::default()
        }
    }

    /// Opens the turn on every `TurnStarted`, learned or not: one
    /// `ResponseId` clone per turn, on the learner-off path too.
    ///
    /// Not deferred to the first learned `Routed`: that turn's `TurnStarted`
    /// comes before its learned evidence, so gating on `started` would lose
    /// the first learned turn's rows, and binding the id at the `Routed` would
    /// accept a dispatch of a response that is not the open turn, which the
    /// id match rejects.
    pub(crate) fn turn_started(&mut self, response_id: &ResponseId, at_ms: u64) {
        self.open = Some(OpenTurn {
            response_id: response_id.clone(),
            started_at_ms: at_ms,
            first: None,
            dispatches: 0,
            served: None,
            first_output_at_ms: None,
        });
    }

    /// A `Routed` folded in. Returns its learning row for the review tracker.
    pub(crate) fn routed(
        &mut self,
        response_id: &ResponseId,
        at_ms: u64,
        decision: &DecisionRecord,
    ) -> Option<LearningRow> {
        let row = LearningRow::of(decision);
        if let Some(row) = row {
            self.started = true;
            self.last_learned = Some((response_id.clone(), row));
        }
        // Only a turn that can add operational rows is tracked, so an
        // unlearned `Routed` clones nothing here (its turn's id was taken at
        // `turn_started`). Every `Routed` of one turn carries the same
        // selection, so a turn is learned on all of its dispatches or on none.
        let Some(current) = row.filter(LearningRow::is_current) else {
            return row;
        };
        if let Some(open) = self
            .open
            .as_mut()
            .filter(|open| open.response_id == *response_id)
        {
            open.dispatches += 1;
            open.first.get_or_insert_with(|| decision.chosen.clone());
            // The first output is measured from the dispatch that served, so a
            // failover restarts it.
            open.first_output_at_ms = None;
            open.served = Some(Served {
                row: current,
                target: decision.chosen.clone(),
                routed_at_ms: at_ms,
                quoted_ttft_ms: decision
                    .considered
                    .iter()
                    .find(|candidate| candidate.target == decision.chosen)
                    .map(|candidate| candidate.expected_ttft_ms),
                isl_tokens: decision.isl_tokens,
                expected_prefill_tokens: decision.expected_prefill_tokens,
            });
        }
        row
    }

    pub(crate) fn output(&mut self, response_id: &ResponseId, at_ms: u64, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(open) = self
            .open
            .as_mut()
            .filter(|open| open.response_id == *response_id && open.served.is_some())
        {
            open.first_output_at_ms.get_or_insert(at_ms);
        }
    }

    /// A terminal event folded in: the turn's operational rows.
    pub(crate) fn terminal(
        &mut self,
        response_id: &ResponseId,
        completed: bool,
        usage: &Usage,
    ) -> Option<Deltas> {
        let open = self.open.take_if(|open| open.response_id == *response_id)?;
        let served = open.served?;
        let mut deltas = Deltas::empty(served.row.epoch());

        // Residual and overhead from the same turns: a completed turn with a
        // first output and a quote supplies both, any other turn neither.
        // Missing output is not zero latency.
        if completed
            && let (Some(first_output), Some(quote)) =
                (open.first_output_at_ms, served.quoted_ttft_ms)
        {
            let residual = first_output as i64 - served.routed_at_ms as i64 - quote.round() as i64;
            target_row(&mut deltas, &served.target).latency = LatencySum {
                sum_ms: residual,
                n: 1,
            };
            deltas.overhead = LatencySum {
                sum_ms: served.routed_at_ms as i64 - open.started_at_ms as i64,
                n: 1,
            };
        }
        if open.dispatches > 1
            && let Some(first) = &open.first
        {
            target_row(&mut deltas, first).failover = 1;
        }
        if !served.target.is_local()
            && let Some((predicted, observed)) =
                measured_pair(served.isl_tokens, served.expected_prefill_tokens, usage)
        {
            target_row(&mut deltas, &served.target).cache = CacheReuse {
                predicted_permille: permille(predicted),
                observed_permille: permille(observed),
                n: 1,
            };
        }
        deltas.non_empty()
    }

    /// A judged validation folded in: the credit of the review it carried,
    /// when the review tracker accepted one.
    pub(crate) fn judged(&mut self, reviewed: Option<Reviewed>) -> Option<Deltas> {
        credit::credit(&reviewed?, &mut self.causes)
    }

    /// A classification intent folded in.
    ///
    /// Keeps the row of the turn the intent is about, when that turn was
    /// learned. **Retention ends at the result or at the intent's expiry**,
    /// and expiry is read only when a later intent is requested: a result is
    /// delivered at the start of the next turn, however long after its call's
    /// deadline that is, and the next intent is always written after that
    /// delivery. So a row is dropped only once its result can no longer be
    /// on its way, and at most the intents not yet expired at the newest
    /// request are held: what the classification runtime can have in flight.
    pub(crate) fn intent(&mut self, record: &ClassificationIntent) {
        self.awaiting
            .retain(|waiting| waiting.expires_at_ms > record.requested_at_ms);
        if let Some((_, row)) = self.last_learned.as_ref().filter(|(response_id, row)| {
            *response_id == record.source_response_id && row.is_current()
        }) {
            self.awaiting.push(AwaitingAnswer {
                call_id: record.call_id.clone(),
                expires_at_ms: record.expires_at_ms,
                row: *row,
            });
        }
    }

    /// A classification result folded in, `accepted` by the classification
    /// fold: one Jev count on each key of its turn, when it has a tier answer.
    pub(crate) fn classified(
        &mut self,
        record: &ClassificationRecord,
        accepted: bool,
    ) -> Option<Deltas> {
        if !accepted {
            return None;
        }
        let at = self
            .awaiting
            .iter()
            .position(|waiting| waiting.call_id == record.call_id)?;
        let row = self.awaiting.remove(at).row;
        let tier = record.outcome.classification()?.tier?.value;
        let counts = match tier.tier() {
            crate::routing::Tier::Capable => JevCounts {
                capable: 1,
                efficient: 0,
            },
            crate::routing::Tier::Efficient => JevCounts {
                capable: 0,
                efficient: 1,
            },
        };
        let mut deltas = Deltas::empty(row.epoch());
        deltas.jev = row
            .keys()
            .into_iter()
            .map(|key| JevDelta { key, counts })
            .collect();
        Some(deltas)
    }

    /// A `LearningApplied` folded in: the store had every entry through
    /// `through_seq`. A backfill records the hint and keeps its page.
    pub(crate) fn applied(&mut self, through_seq: u64) {
        self.hint = self.hint.max(through_seq);
        if let Hold::Floor(_) = self.hold {
            return;
        }
        self.page.retain(|entry| entry.seq > through_seq);
        if through_seq >= self.last_entry {
            self.beyond = 0;
        }
    }

    /// Close the event at `seq`: one entry if its kind produces one.
    pub(crate) fn entry(&mut self, seq: u64, kind: &SessionEventKind, deltas: Option<Deltas>) {
        if !self.started || !produces_entry(kind) {
            return;
        }
        let entry = LearningEntry {
            seq,
            prev_seq: self.last_entry,
            credit_revision: LEARNING_CREDIT_REVISION,
            review_rule_revision: REVIEW_RULE_REVISION,
            deltas,
        };
        self.last_entry = seq;
        let held_after = match self.hold {
            Hold::Hint => self.hint,
            Hold::Floor(floor) => floor,
        };
        if seq <= held_after {
            return;
        }
        if self.beyond == 0 && self.page.len() < LEARNING_PAGE {
            self.page.push(entry);
        } else {
            self.beyond += 1;
        }
    }

    pub(crate) fn page(&self) -> &[LearningEntry] {
        &self.page
    }

    pub(crate) fn beyond(&self) -> u64 {
        self.beyond
    }

    pub(crate) fn hint(&self) -> u64 {
        self.hint
    }

    pub(crate) fn causes(&self) -> LearningCauses {
        self.causes
    }
}

/// The row of `target` in `deltas`, added when absent.
fn target_row<'a>(deltas: &'a mut Deltas, target: &Target) -> &'a mut TargetDelta {
    let identity = target.policy_identity();
    let at = match deltas.targets.iter().position(|row| row.target == identity) {
        Some(at) => at,
        None => {
            deltas.targets.push(TargetDelta {
                target: identity,
                latency: LatencySum::default(),
                failover: 0,
                cache: CacheReuse::default(),
            });
            deltas.targets.len() - 1
        }
    };
    &mut deltas.targets[at]
}

/// A ratio in `[0, 1]` as whole per-mille, rounded to nearest. Unbiased, so a
/// sum of many samples keeps the ratio of the sums the correction reads.
fn permille(ratio: f64) -> u64 {
    (ratio.clamp(0.0, 1.0) * 1_000.0).round() as u64
}
