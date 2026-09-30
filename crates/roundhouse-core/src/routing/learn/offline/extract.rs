// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! From session logs to the facts the estimates read: the eligible intervals
//! and their turns, the exclusions by cause, the prior the artifact carries,
//! the judge and classifier spend by stratum, and the Jev agreement block.
//!
//! **The same screen credit uses.** Each accepted review is screened by
//! `session::screen`, the function credit calls, so an interval is eligible
//! exactly when credit would credit it. Only then does the calibrator add its
//! own check: every covered turn must replay (see [`replays`]).
//!
//! **Nothing is re-derived from configuration.** A turn's plans, its
//! propensity, its exploration set and its rate card all come off its own
//! `Routed`; the manifest names only which strategies the artifact lists.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::estimate::{
    Money, Outcome, TurnTrace, trajectory_probability, trajectory_supported, trajectory_weight,
};
use super::source::SessionLog;
use super::{ArtifactPrior, CalibrationConfig, Candidate};
use crate::classify::EvaluationSpend;
use crate::event::{Accounting, SessionEvent, SessionEventKind, Usage, ValidationOutcome};
use crate::ids::{ResponseId, SessionId, SideCallId};
use crate::metrics::TierAgreement;
use crate::metrics::{MetricsConfig, MetricsFold, MetricsSnapshot, Scope, ShadowPricing};
use crate::routing::learn::explore::eligible;
use crate::routing::learn::policy::exploit_order;
use crate::routing::learn::{
    ActiveMode, CostEvidence, EpochId, LearnedChoice, LearnedEvidence, LearnedInput, LevelKey,
    Strategy, TtftEvidence, Units,
};
use crate::routing::{DecisionRecord, SelectorBranch, Target};
use crate::session::{
    Exclusion, LearningCauses, LearningEntry, LearningRow, SessionState, same_route, screen,
};

/// Why an accepted review is not an evaluation unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cause {
    /// Credit's own screen refused it.
    Screen(Exclusion),
    /// A covered turn's record does not replay: see [`replays`].
    ReplayMismatch,
    /// The product of the covered turns' propensities is so small that its
    /// inverse, the weight of any candidate that matched every turn, is not a
    /// finite number.
    NonFiniteWeight,
}

impl Cause {
    /// Every cause, in the order the report lists them.
    pub const ALL: [Cause; 7] = [
        Cause::Screen(Exclusion::UnknownLabel),
        Cause::Screen(Exclusion::FailoverInInterval),
        Cause::Screen(Exclusion::MissingRow),
        Cause::Screen(Exclusion::MixedEpoch),
        Cause::Screen(Exclusion::OtherCreditRevision),
        Cause::ReplayMismatch,
        Cause::NonFiniteWeight,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Cause::Screen(Exclusion::UnknownLabel) => "unknown label",
            Cause::Screen(Exclusion::FailoverInInterval) => "failover in the interval",
            Cause::Screen(Exclusion::MissingRow) => "no learned row",
            Cause::Screen(Exclusion::MixedEpoch) => "mixed epoch",
            Cause::Screen(Exclusion::OtherCreditRevision) => "other credit revision",
            Cause::ReplayMismatch => "record does not replay",
            Cause::NonFiniteWeight => "trajectory weight not finite",
        }
    }
}

/// One covered turn of an eligible interval, before any candidate reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnFacts {
    /// The first target the turn dispatched to.
    pub served: Target,
    /// The recorded probability that the logging policy served it.
    pub propensity: f64,
    /// Each recorded strategy's first target, in recorded order.
    pub plans: Vec<(Strategy, Target)>,
    /// What `live` without exploration would have served: the exploit
    /// strategy's first target, else the `rules` one.
    pub learned: Target,
    /// The strategy whose plan `learned` is: the exploit strategy, else
    /// `rules`.
    pub learned_strategy: Strategy,
    /// Each recorded plan's quote with its M3 corrections, in recorded order:
    /// what a corrected quote estimate prices a strategy's action from.
    pub quotes: Vec<PlanQuote>,
    /// Every first target the logging policy could have served on this turn:
    /// its default (the `rules` target in `shadow`, the exploit-else-`rules`
    /// target in `live`) and, when the turn could explore, every member's.
    pub logged: Vec<Target>,
    /// First output from turn start, on a completed turn with a first output
    /// after this dispatch and a quote for its target (the turns M5 samples).
    pub first_output_ms: Option<u64>,
    /// The terminal usage at the dispatch's recorded rate card; unpriced for
    /// a local dispatch and for usage Roundhouse estimated.
    pub cost: Money,
}

/// One plan's recorded quote, as the M3 corrections left it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanQuote {
    pub strategy: Strategy,
    pub cost: CostEvidence,
    pub ttft: TtftEvidence,
}

impl TurnFacts {
    /// The first target `candidate` would have served on this turn, or `None`
    /// for a strategy the record did not plan.
    pub fn action(&self, candidate: Candidate) -> Option<&Target> {
        match candidate {
            Candidate::Learned => Some(&self.learned),
            Candidate::Fixed(strategy) => self
                .plans
                .iter()
                .find(|(planned, _)| *planned == strategy)
                .map(|(_, first)| first),
        }
    }

    /// The recorded quote of `strategy`'s plan on this turn.
    pub fn quote(&self, strategy: Strategy) -> Option<&PlanQuote> {
        self.quotes.iter().find(|quote| quote.strategy == strategy)
    }

    pub fn trace(&self, candidate: Candidate) -> TurnTrace {
        let action = self.action(candidate);
        let matched = action.is_some_and(|action| same_route(action, &self.served));
        TurnTrace {
            propensity: self.propensity,
            matched,
            supported: matched
                || action.is_some_and(|action| {
                    self.logged.iter().any(|logged| same_route(action, logged))
                }),
        }
    }
}

/// One evaluation unit.
#[derive(Debug, Clone, PartialEq)]
pub struct IntervalFacts {
    /// Index of its session in [`Evidence::sessions`]: the cluster.
    pub cluster: usize,
    pub positive: bool,
    /// The covered turns, oldest first.
    pub turns: Vec<TurnFacts>,
}

impl IntervalFacts {
    pub fn outcome(&self, candidate: Candidate) -> Outcome {
        let traces: Vec<TurnTrace> = self
            .turns
            .iter()
            .map(|turn| turn.trace(candidate))
            .collect();
        Outcome {
            turns: self.turns.len(),
            cluster: self.cluster,
            positive: self.positive,
            weight: trajectory_weight(&traces),
            supported: trajectory_supported(&traces),
            matched: traces.iter().all(|trace| trace.matched),
            cost: self
                .turns
                .iter()
                .fold(Money::Priced(0.0), |sum, turn| sum.plus(turn.cost)),
            first_output_ms: self
                .turns
                .iter()
                .filter_map(|turn| turn.first_output_ms)
                .collect(),
        }
    }
}

/// Which strategy a side call's spend belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stratum {
    /// Every turn the call is about was served by this strategy's plan.
    Strategy(Strategy),
    /// A judge call whose interval covered turns served by different
    /// strategies.
    Mixed,
    /// The call is about no learned turn.
    Unattributed,
}

impl Stratum {
    pub fn label(self) -> String {
        match self {
            Stratum::Strategy(strategy) => format!("served by {strategy}"),
            Stratum::Mixed => "mixed".to_owned(),
            Stratum::Unattributed => "unattributed".to_owned(),
        }
    }
}

/// Judge and classifier spend in one stratum.
///
/// **Judge dollars are not here.** A judge side call records its usage and
/// target but no rate card, so its price is not derivable from the log; the
/// report prints its tokens and calls it unpriced, never free.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StratumSpend {
    pub judge_calls: u64,
    pub judge_abandoned: u64,
    pub judge_input_tokens: u64,
    pub judge_output_tokens: u64,
    pub classifier_measured_calls: u64,
    /// Measured classifier spend at the rate card each call recorded.
    pub classifier_measured_usd: f64,
    /// Calls whose usage nobody reported.
    pub classifier_unknown_calls: u64,
    /// What those calls' settles submitted: an estimate, never a measurement.
    pub classifier_estimated_usd: f64,
}

/// One session's entries and reads, for the drift check.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEntries {
    pub session: SessionId,
    /// The last sequence the manifest read.
    pub through_seq: u64,
    /// The session's whole entry chain.
    pub entries: Vec<LearningEntry>,
    /// Every recorded learned input with its epoch, deduplicated.
    pub reads: Vec<(EpochId, LearnedInput)>,
    /// Every target a learned decision planned or served, with its epoch.
    pub targets: Vec<(EpochId, Target)>,
}

/// What the manifest's logs hold, before any candidate is estimated.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    /// Every session read, in manifest order; an interval's cluster indexes it.
    pub sessions: Vec<SessionEntries>,
    pub accepted_reviews: u64,
    pub intervals: Vec<IntervalFacts>,
    pub exclusions: BTreeMap<Cause, u64>,
    /// The session folds' own causes, summed. Credit and the calibrator run
    /// one screen, so these equal the `Screen` exclusions above.
    pub fold_causes: LearningCauses,
    /// Summed quality credit per key and strategy, for listed strategies.
    pub prior: BTreeMap<(LevelKey, Strategy), Units>,
    /// Credit for strategies the artifact does not list, which it cannot
    /// carry.
    pub dropped_prior: BTreeMap<Strategy, Units>,
    pub spend: BTreeMap<Stratum, StratumSpend>,
    pub agreement: TierAgreement,
}

impl Evidence {
    /// Replay every log and collect the facts.
    pub fn extract(config: &CalibrationConfig, logs: &[SessionLog]) -> Self {
        let mut evidence = Evidence {
            sessions: Vec::with_capacity(logs.len()),
            accepted_reviews: 0,
            intervals: Vec::new(),
            exclusions: BTreeMap::new(),
            fold_causes: LearningCauses::default(),
            prior: BTreeMap::new(),
            dropped_prior: BTreeMap::new(),
            spend: BTreeMap::new(),
            agreement: TierAgreement::default(),
        };
        let mut metrics = MetricsFold::new();
        for (cluster, log) in logs.iter().enumerate() {
            metrics.extend(&log.events);
            evidence.session(config, cluster, log);
        }
        if config.prior == ArtifactPrior::Zero {
            evidence.prior.clear();
            evidence.dropped_prior.clear();
        }
        evidence.agreement = MetricsSnapshot::build(
            &metrics,
            Scope::Project(&config.project),
            &MetricsConfig::new(ShadowPricing::new(Vec::new())),
            0,
        )
        .evaluation
        .agreement;
        evidence
    }

    fn session(&mut self, config: &CalibrationConfig, cluster: usize, log: &SessionLog) {
        let replay = SessionState::replay_learning(&log.events);
        let index = LogIndex::of(&log.events);
        self.accepted_reviews += replay.reviews.len() as u64;
        self.fold_causes += replay.causes;

        for review in &replay.reviews {
            let rows: Vec<Option<LearningRow>> = review
                .decisions
                .iter()
                .map(|seq| {
                    index
                        .routed
                        .get(seq)
                        .and_then(|(_, decision)| LearningRow::of(decision))
                })
                .collect();
            let positive = match screen(review.label, rows.iter().copied()) {
                Ok(positive) => positive,
                Err(exclusion) => {
                    *self.exclusions.entry(Cause::Screen(exclusion)).or_default() += 1;
                    continue;
                }
            };
            let turns: Option<Vec<TurnFacts>> = review
                .decisions
                .iter()
                .map(|seq| index.turn(*seq))
                .collect();
            match turns {
                Some(turns) if !turns.is_empty() && !finite_weight(&turns) => {
                    *self.exclusions.entry(Cause::NonFiniteWeight).or_default() += 1
                }
                Some(turns) if !turns.is_empty() => self.intervals.push(IntervalFacts {
                    cluster,
                    positive,
                    turns,
                }),
                _ => *self.exclusions.entry(Cause::ReplayMismatch).or_default() += 1,
            }
        }

        for entry in &replay.entries {
            for quality in entry.deltas.iter().flat_map(|deltas| &deltas.quality) {
                let sum = if config.strategies.as_slice().contains(&quality.strategy) {
                    self.prior
                        .entry((quality.key, quality.strategy))
                        .or_default()
                } else {
                    self.dropped_prior.entry(quality.strategy).or_default()
                };
                sum.pos += quality.units.pos;
                sum.n += quality.units.n;
            }
        }
        index.spend(&log.events, &mut self.spend);

        let mut reads = Vec::new();
        let mut targets = Vec::new();
        for (_, decision) in index.routed.values() {
            let Some(evidence) = learned(decision) else {
                continue;
            };
            let read = (evidence.epoch, evidence.input);
            if !reads.contains(&read) {
                reads.push(read);
            }
            let planned = evidence.plans.iter().map(|plan| &plan.first);
            for target in planned.chain([&decision.chosen]) {
                let target = (evidence.epoch, target.clone());
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        self.sessions.push(SessionEntries {
            session: log.session.clone(),
            through_seq: log.events.last().map_or(0, |event| event.seq),
            entries: replay.entries,
            reads,
            targets,
        });
    }
}

/// Whether a candidate that matched every turn would carry a finite weight.
///
/// **Excluded before any estimate reads it**, and counted under its own
/// cause. The weight is `1 / P_log`, the same for every candidate that
/// matched, and a product of many small propensities can fall below the
/// smallest normal float: whether it then rounds to a subnormal or flushes
/// to zero depends on the platform, and either way its inverse is infinite.
/// One infinite weight turns every sum it enters into `inf / inf`, so the
/// estimate and both bounds would read NaN, or a number that differs
/// between machines. The same rule holds for any propensity that makes the
/// weight not a number.
fn finite_weight(turns: &[TurnFacts]) -> bool {
    let traces: Vec<TurnTrace> = turns
        .iter()
        .map(|turn| turn.trace(Candidate::Learned))
        .collect();
    let probability = trajectory_probability(&traces);
    probability > 0.0 && probability.is_finite() && (1.0 / probability).is_finite()
}

/// The learned evidence on a decision, when it has any.
pub(super) fn learned(decision: &DecisionRecord) -> Option<&LearnedEvidence> {
    match &decision.selection.as_deref()?.selector.as_ref()?.branch {
        SelectorBranch::Learned(evidence) => Some(evidence),
        _ => None,
    }
}

/// Whether a learned record is what the policy would have written from its
/// own recorded inputs: the offline half of replay equivalence.
///
/// Checked, because the calibrator weights a turn by what the record says:
///
/// - the propensity is a probability, and exactly `1.0` when the turn could
///   not explore;
/// - the served dispatch went to the served plan's first target;
/// - an exploit choice is the first of the recorded plans' exploit order, and
///   a `ConstraintUnmet` choice has none;
/// - an explored member is the recorded draw modulo the recorded set, and
///   names the chosen strategy;
/// - the recorded set is the set [`eligible`] derives from the recorded
///   plans and `on_infeasible` when the turn could explore, and empty when
///   it could not. A record written under another set rule (before `rules`
///   joined the set on 2026-09-30, say) logged its turn under probabilities
///   this build's policy would not assign, so it does not replay. Neither
///   does a record without `on_infeasible`: its set rule is unknown, so its
///   set cannot be re-derived.
///
/// **The exploration rate is not recorded**, so a rate draw on its own cannot
/// be re-checked; what the record pins exactly is what is checked. A turn
/// that fails breaks the equivalence, and its interval is excluded as
/// [`Cause::ReplayMismatch`].
pub fn replays(evidence: &LearnedEvidence, decision: &DecisionRecord) -> bool {
    let propensity = evidence.propensity;
    if !(propensity > 0.0 && propensity <= 1.0) {
        return false;
    }
    if !same_route(&evidence.served_plan().first, &decision.chosen) {
        return false;
    }
    if let Some(exploration) = &evidence.exploration {
        let Some(on_infeasible) = exploration.on_infeasible else {
            return false;
        };
        let rule = if exploration.possible {
            eligible(&evidence.plans, on_infeasible)
        } else {
            Vec::new()
        };
        if exploration.set != rule {
            return false;
        }
    }
    let arena = evidence
        .exploration
        .as_ref()
        .filter(|exploration| exploration.possible && !exploration.set.is_empty());
    if arena.is_none() && propensity != 1.0 {
        return false;
    }
    let exploit = exploit_order(&evidence.plans)
        .first()
        .map(|&at| evidence.plans[at].strategy);
    match &evidence.choice {
        LearnedChoice::Exploit { strategy } => exploit == Some(*strategy),
        LearnedChoice::ConstraintUnmet { .. } => exploit.is_none(),
        LearnedChoice::Explore { strategy, member } => arena.is_some_and(|exploration| {
            *member == exploration.draw.member % exploration.set.len() as u64
                && exploration.set.get(*member as usize) == Some(strategy)
        }),
    }
}

/// When one response's turn started, dispatched, spoke and ended.
#[derive(Debug, Default)]
struct Timeline {
    started_at_ms: Option<u64>,
    /// The latest `Routed` of the response.
    routed_seq: Option<u64>,
    /// The first non-empty output after that `Routed`.
    first_output_at_ms: Option<u64>,
    /// `Some(true)` for a completed response.
    completed: Option<bool>,
    usage: Option<Usage>,
}

/// One log, indexed for lookups. Only looked up, never iterated in hash order.
struct LogIndex<'a> {
    routed: BTreeMap<u64, (&'a ResponseId, &'a DecisionRecord)>,
    timelines: HashMap<&'a ResponseId, Timeline>,
}

impl<'a> LogIndex<'a> {
    fn of(events: &'a [SessionEvent]) -> Self {
        let mut routed = BTreeMap::new();
        let mut timelines: HashMap<&ResponseId, Timeline> = HashMap::new();
        for event in events {
            match &event.kind {
                SessionEventKind::TurnStarted { response_id, .. } => {
                    timelines.entry(response_id).or_default().started_at_ms = Some(event.at_ms);
                }
                SessionEventKind::Routed {
                    response_id,
                    decision,
                } => {
                    routed.insert(event.seq, (response_id, decision));
                    let timeline = timelines.entry(response_id).or_default();
                    timeline.routed_seq = Some(event.seq);
                    timeline.first_output_at_ms = None;
                }
                SessionEventKind::OutputTextDelta { response_id, text } if !text.is_empty() => {
                    if let Some(timeline) = timelines.get_mut(response_id)
                        && timeline.routed_seq.is_some()
                        && timeline.completed.is_none()
                    {
                        timeline.first_output_at_ms.get_or_insert(event.at_ms);
                    }
                }
                SessionEventKind::ResponseCompleted {
                    response_id, usage, ..
                }
                | SessionEventKind::ResponseIncomplete {
                    response_id, usage, ..
                } => {
                    let timeline = timelines.entry(response_id).or_default();
                    timeline.completed = Some(matches!(
                        event.kind,
                        SessionEventKind::ResponseCompleted { .. }
                    ));
                    timeline.usage = Some(usage.clone());
                }
                _ => {}
            }
        }
        Self { routed, timelines }
    }

    /// The facts of the covered decision at `seq`, or `None` when it does not
    /// replay.
    fn turn(&self, seq: u64) -> Option<TurnFacts> {
        let (response, decision) = self.routed.get(&seq)?;
        let evidence = learned(decision)?;
        if !replays(evidence, decision) {
            return None;
        }
        let rules = &evidence.plan(Strategy::Rules)?.first;
        let exploit = exploit_order(&evidence.plans).first().copied();
        let learned_first = exploit.map_or(rules, |at| &evidence.plans[at].first);
        let learned_strategy = exploit.map_or(Strategy::Rules, |at| evidence.plans[at].strategy);
        let default = match evidence.mode {
            ActiveMode::Shadow => rules,
            ActiveMode::Live => learned_first,
        };
        let mut logged = vec![default.clone()];
        if let Some(exploration) = evidence
            .exploration
            .as_ref()
            .filter(|exploration| exploration.possible)
        {
            for member in &exploration.set {
                if let Some(plan) = evidence.plan(*member) {
                    logged.push(plan.first.clone());
                }
            }
        }
        let timeline = self.timelines.get(response);
        let quoted = decision
            .considered
            .iter()
            .any(|candidate| candidate.target == decision.chosen);
        let first_output_ms = timeline.and_then(|timeline| {
            let served = timeline.routed_seq == Some(seq) && timeline.completed == Some(true);
            match (
                served && quoted,
                timeline.started_at_ms,
                timeline.first_output_at_ms,
            ) {
                (true, Some(started), Some(first)) => Some(first.saturating_sub(started)),
                _ => None,
            }
        });
        // Estimated usage leaves cached input at zero, since no local
        // evidence says what a remote cache did, so pricing it as measured
        // overprices every turn that read from cache. It is unpriced rather
        // than a third arm, because no priced comparison may read it.
        let cost = match (
            timeline.and_then(|timeline| timeline.usage.as_ref()),
            decision.rate_card,
        ) {
            (Some(usage), Some(card))
                if !decision.chosen.is_local() && usage.accounting == Accounting::Reported =>
            {
                Money::Priced(card.price(usage))
            }
            _ => Money::Unpriced,
        };
        Some(TurnFacts {
            served: decision.chosen.clone(),
            propensity: evidence.propensity,
            plans: evidence
                .plans
                .iter()
                .map(|plan| (plan.strategy, plan.first.clone()))
                .collect(),
            learned: learned_first.clone(),
            learned_strategy,
            quotes: evidence
                .plans
                .iter()
                .map(|plan| PlanQuote {
                    strategy: plan.strategy,
                    cost: plan.cost,
                    ttft: plan.ttft,
                })
                .collect(),
            logged,
            first_output_ms,
            cost,
        })
    }

    /// Add this log's judge and classifier spend to `spend`, by stratum.
    fn spend(&self, events: &[SessionEvent], spend: &mut BTreeMap<Stratum, StratumSpend>) {
        let mut served: HashMap<&ResponseId, Strategy> = HashMap::new();
        for (response, decision) in self.routed.values() {
            if let Some(evidence) = learned(decision) {
                served.insert(response, evidence.served_strategy());
            }
        }
        let mut judged: HashMap<&SideCallId, Stratum> = HashMap::new();
        for event in events {
            if let SessionEventKind::ValidationDecided {
                outcome:
                    ValidationOutcome::Judged {
                        side_call_id,
                        interval,
                        ..
                    },
                ..
            } = &event.kind
            {
                let strategies: BTreeSet<Strategy> = interval
                    .iter()
                    .flat_map(|review| &review.decisions)
                    .filter_map(|covered| self.routed.get(&covered.routed_seq))
                    .filter_map(|(_, decision)| learned(decision))
                    .map(|evidence| evidence.served_strategy())
                    .collect();
                let stratum = match strategies.len() {
                    0 => Stratum::Unattributed,
                    1 => Stratum::Strategy(*strategies.first().expect("one strategy")),
                    _ => Stratum::Mixed,
                };
                judged.insert(side_call_id, stratum);
            }
        }
        // The metrics evaluation block's join: a result counts once, and only
        // against an open intent for the same turn, so a repeated or
        // misattributed delivery is not spend twice.
        let mut intents: HashMap<&ResponseId, (u64, &ResponseId)> = HashMap::new();
        for event in events {
            match &event.kind {
                SessionEventKind::ClassificationRequested { record } => {
                    intents.insert(
                        &record.call_id,
                        (record.source_turn_index, &record.source_response_id),
                    );
                }
                SessionEventKind::SideCallCompleted {
                    side_call_id,
                    usage,
                    ..
                } => {
                    let stratum = judged
                        .get(side_call_id)
                        .copied()
                        .unwrap_or(Stratum::Unattributed);
                    let row = spend.entry(stratum).or_default();
                    row.judge_calls += 1;
                    row.judge_input_tokens += usage.input_tokens;
                    row.judge_output_tokens += usage.output_tokens;
                }
                SessionEventKind::SideCallAbandoned { side_call_id, .. } => {
                    let stratum = judged
                        .get(side_call_id)
                        .copied()
                        .unwrap_or(Stratum::Unattributed);
                    spend.entry(stratum).or_default().judge_abandoned += 1;
                }
                SessionEventKind::ClassificationRecorded { record } => {
                    let joined = intents
                        .get(&record.call_id)
                        .is_some_and(|(turn, response)| {
                            *turn == record.source_turn_index
                                && **response == record.source_response_id
                        });
                    if !joined {
                        continue;
                    }
                    intents.remove(&record.call_id);
                    let stratum = served
                        .get(&record.source_response_id)
                        .map_or(Stratum::Unattributed, |strategy| {
                            Stratum::Strategy(*strategy)
                        });
                    match record.outcome.spend() {
                        Some(EvaluationSpend::Measured { usd, .. }) => {
                            let row = spend.entry(stratum).or_default();
                            row.classifier_measured_calls += 1;
                            row.classifier_measured_usd += usd;
                        }
                        Some(EvaluationSpend::Unknown { submitted_usd, .. }) => {
                            let row = spend.entry(stratum).or_default();
                            row.classifier_unknown_calls += 1;
                            row.classifier_estimated_usd += submitted_usd;
                        }
                        None => {}
                    }
                }
                _ => {}
            }
        }
    }
}
