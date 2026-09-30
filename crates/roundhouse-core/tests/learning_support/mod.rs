// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared fixtures for the learning-entry suite.
//!
//! [`Script`] crafts a log event by event with explicit timestamps, because
//! the latency rows are differences of log stamps and the memory store stamps
//! its own clock. The fold then runs over it through the real replay
//! (`SessionState::project`), exactly as a successor builds it.

#![allow(dead_code)]

use roundhouse_core::classify::{
    ClassificationIntent, ClassificationOutcome, ClassificationRecord, ClassifierIdentity,
    ContextDependence, EvaluationSpend, EvaluationUsage, Graded, ReservationRecord, SettlementAck,
    TierChoice, TurnClassification, TurnComplexity, TurnIntent,
};
use roundhouse_core::control::{BudgetWindow, Principal};
use roundhouse_core::event::{
    Accounting, CacheReadSource, IncompleteReason, NotRunReason, SessionEvent, SessionEventKind,
    Usage, ValidationOutcome,
};
use roundhouse_core::ids::{ResponseId, SessionId, SideCallId, TurnId, ValidationId};
use roundhouse_core::routing::learn::{
    ActiveMode, Band, CostCorrection, CostEvidence, EpochId, ExplorationEvidence, GateEvidence,
    GateResult, GrantCheck, LEARNING_CREDIT_REVISION, LEARNING_INPUT_REVISION, LatencyTerm,
    LearnedChoice, LearnedEvidence, LearnedEvidenceParts, LearnedInput, PlanEvidence, PriorBand,
    ReadFailure, StoreRead, Strategy, TtftEvidence, Unmet,
};
use roundhouse_core::routing::{
    AttemptClass, CacheLedger, Candidate, DecisionRecord, DecisionSource, DispatchAttempt,
    LocalFeatures, Pick, PickerMode, ProviderPricing, RecipeEvidence, SelectionSnapshot,
    SelectorSnapshot, StageOutcome, Target, Tier, TurnSignals,
};
use roundhouse_core::session::{LearningEntry, SessionState};
use roundhouse_core::store::doubles::ReplayLog;
use roundhouse_core::validate::{
    Arm, ControlCallDialect, IntervalLabel, IntervalReview, ObjectiveVersion, REVIEW_RULE_REVISION,
    ReviewedDecision, SteerAction, TriggerRecord, Verdict,
};

pub fn opus() -> Target {
    Target::Frontier {
        provider: "anthropic".into(),
        model: "opus".into(),
    }
}

pub fn haiku() -> Target {
    Target::Frontier {
        provider: "anthropic".into(),
        model: "haiku".into(),
    }
}

pub fn qwen() -> Target {
    Target::Local {
        worker_id: 3,
        dp_rank: 0,
        model: "qwen".into(),
    }
}

pub fn epoch() -> EpochId {
    EpochId::new([7; 16])
}

pub fn other_epoch() -> EpochId {
    EpochId::new([9; 16])
}

/// The quoted TTFT every fixture candidate carries.
pub const QUOTE_MS: f64 = 200.0;

/// The learned input of a turn whose newest classification band is `newest`.
pub fn input(rules_pick: Tier, newest: Band) -> LearnedInput {
    LearnedInput {
        rules_pick,
        newest,
        prior: PriorBand::Absent,
        tool_turn: false,
    }
}

/// What a fixture learned decision records.
#[derive(Clone)]
pub struct Spec {
    pub epoch: EpochId,
    pub credit_revision: u32,
    pub input: LearnedInput,
    pub mode: ActiveMode,
    /// Where this dispatch went.
    pub chosen: Target,
    /// Each strategy's first target, `rules` first.
    pub plans: Vec<(Strategy, Target)>,
    /// This dispatch follows a failed one.
    pub failed_before: bool,
    /// The recorded choice; `None` is the serve-rules turn every other
    /// fixture writes.
    pub choice: Option<LearnedChoice>,
    pub exploration: Option<ExplorationEvidence>,
    pub propensity: f64,
    /// Strategies whose plan passed the gate, at the corrected cost given.
    pub passing: Vec<(Strategy, f64)>,
    /// Strategies whose recorded quote the M3 terms corrected: the cost
    /// correction `Applied` at the dollars given, and both latency terms
    /// `Applied` with the first output given. Every other plan records
    /// `TooFewSamples`, which a corrected quote estimate cannot use.
    pub corrected: Vec<(Strategy, f64, f64)>,
    /// Strategies whose recorded quote is exactly the evidence given, applied
    /// after `corrected`.
    pub quotes: Vec<(Strategy, CostEvidence, TtftEvidence)>,
    pub rate_card: Option<ProviderPricing>,
}

impl Spec {
    /// A shadow decision that served `opus`, with `rules` and `capable` on
    /// `opus` and `efficient` on `haiku`.
    pub fn new() -> Self {
        Self {
            epoch: epoch(),
            credit_revision: LEARNING_CREDIT_REVISION,
            input: input(Tier::Capable, Band::None),
            mode: ActiveMode::Shadow,
            chosen: opus(),
            plans: vec![
                (Strategy::Rules, opus()),
                (Strategy::Efficient, haiku()),
                (Strategy::Capable, opus()),
            ],
            failed_before: false,
            choice: None,
            exploration: None,
            propensity: 1.0,
            passing: Vec::new(),
            corrected: Vec::new(),
            quotes: Vec::new(),
            rate_card: None,
        }
    }

    pub fn choice(mut self, choice: LearnedChoice) -> Self {
        self.choice = Some(choice);
        self
    }

    pub fn exploration(mut self, exploration: ExplorationEvidence) -> Self {
        self.exploration = Some(exploration);
        self
    }

    pub fn propensity(mut self, propensity: f64) -> Self {
        self.propensity = propensity;
        self
    }

    pub fn passing(mut self, strategy: Strategy, adjusted_usd: f64) -> Self {
        self.passing.push((strategy, adjusted_usd));
        self
    }

    /// `strategy`'s plan quoted at `usd` and `first_output_ms`, with every M3
    /// term applied.
    pub fn corrected(mut self, strategy: Strategy, usd: f64, first_output_ms: f64) -> Self {
        self.corrected.push((strategy, usd, first_output_ms));
        self
    }

    /// `strategy`'s plan recorded with exactly `cost` and `ttft`.
    pub fn quote(mut self, strategy: Strategy, cost: CostEvidence, ttft: TtftEvidence) -> Self {
        self.quotes.push((strategy, cost, ttft));
        self
    }

    pub fn rate_card(mut self, card: ProviderPricing) -> Self {
        self.rate_card = Some(card);
        self
    }

    pub fn input(mut self, input: LearnedInput) -> Self {
        self.input = input;
        self
    }

    pub fn epoch(mut self, epoch: EpochId) -> Self {
        self.epoch = epoch;
        self
    }

    pub fn credit_revision(mut self, revision: u32) -> Self {
        self.credit_revision = revision;
        self
    }

    pub fn chosen(mut self, chosen: Target) -> Self {
        self.chosen = chosen;
        self
    }

    pub fn plans(mut self, plans: Vec<(Strategy, Target)>) -> Self {
        self.plans = plans;
        self
    }

    pub fn failed_before(mut self) -> Self {
        self.failed_before = true;
        self
    }

    pub fn live(mut self) -> Self {
        self.mode = ActiveMode::Live;
        self
    }

    pub fn decision(&self) -> DecisionRecord {
        let recipe = RecipeEvidence {
            capable: vec!["anthropic/opus".into()],
            efficient: vec!["anthropic/haiku".into(), "local/qwen".into()],
            picker: PickerMode::EfficientFirst,
            confidence_threshold: 0.5,
        };
        let plans = self
            .plans
            .iter()
            .map(|(strategy, first)| {
                let mut plan = plan(*strategy, first.clone());
                if let Some((_, usd)) = self.passing.iter().find(|(pass, _)| pass == strategy) {
                    plan.gate.result = GateResult::Pass;
                    plan.cost.adjusted_usd = *usd;
                }
                if let Some((_, usd, ms)) =
                    self.corrected.iter().find(|(named, ..)| named == strategy)
                {
                    plan.cost = CostEvidence {
                        quoted_usd: *usd,
                        adjusted_usd: *usd,
                        correction: CostCorrection::Applied,
                    };
                    plan.ttft = TtftEvidence {
                        quoted_ms: *ms,
                        adjusted_ms: *ms,
                        residual: LatencyTerm::Applied { mean_ms: 0 },
                        overhead: LatencyTerm::Applied { mean_ms: 0 },
                    };
                }
                if let Some((_, cost, ttft)) =
                    self.quotes.iter().find(|(named, ..)| named == strategy)
                {
                    plan.cost = *cost;
                    plan.ttft = *ttft;
                }
                plan
            })
            .collect();
        let evidence = LearnedEvidence::new(LearnedEvidenceParts {
            mode: self.mode,
            epoch: self.epoch,
            input_revision: LEARNING_INPUT_REVISION,
            credit_revision: self.credit_revision,
            input: self.input,
            view: StoreRead::Unavailable {
                reason: ReadFailure::ReadTimedOut,
            },
            recipe,
            plans,
            // The serve-rules turn: `rules` served, in either mode.
            choice: self
                .choice
                .clone()
                .unwrap_or(LearnedChoice::ConstraintUnmet {
                    unmet: vec![Unmet::ReadTimedOut],
                }),
            exploration: self.exploration.clone(),
            propensity: self.propensity,
        })
        .expect("a fixture record with a rules plan");
        let mut decision = decision(
            self.chosen.clone(),
            Some(SelectorSnapshot::learned(evidence)),
        );
        decision.rate_card = self.rate_card;
        if self.failed_before {
            decision.attempts = vec![DispatchAttempt {
                target: haiku(),
                class: AttemptClass::Status { status: 503 },
                elapsed_ms: 50,
            }];
        }
        decision
    }
}

impl Default for Spec {
    fn default() -> Self {
        Self::new()
    }
}

fn plan(strategy: Strategy, first: Target) -> PlanEvidence {
    let tier = match strategy {
        Strategy::Efficient => Tier::Efficient,
        _ => Tier::Capable,
    };
    PlanEvidence {
        strategy,
        pick: Pick {
            tier,
            source: match strategy {
                Strategy::Rules => DecisionSource::Dimensions,
                _ => DecisionSource::Strategy,
            },
            score: 0.0,
            confidence: None,
        },
        outcome: StageOutcome::Served { tier },
        first,
        cost: CostEvidence {
            quoted_usd: 0.01,
            adjusted_usd: 0.01,
            correction: CostCorrection::TooFewSamples,
        },
        ttft: TtftEvidence {
            quoted_ms: QUOTE_MS,
            adjusted_ms: QUOTE_MS,
            residual: LatencyTerm::TooFewSamples,
            overhead: LatencyTerm::TooFewSamples,
        },
        grant: GrantCheck::Admits,
        latency_met: true,
        gate: GateEvidence {
            level: None,
            result: GateResult::Unproven,
        },
    }
}

/// A decision that went to `chosen`, stamped with an objective so a review
/// can label it, with one candidate per fixture target at [`QUOTE_MS`].
pub fn decision(chosen: Target, selector: Option<SelectorSnapshot>) -> DecisionRecord {
    let candidate = |target: Target| Candidate {
        target,
        expected_prefill_tokens: 400.0,
        matched_prefix_tokens: 600,
        expected_ttft_ms: QUOTE_MS,
        expected_cost_usd: 0.01,
        quality_prior: 0.9,
        load: None,
    };
    DecisionRecord {
        block_marker: None,
        selection: Some(Box::new(SelectionSnapshot {
            features: LocalFeatures {
                extractor_revision: 1,
                dialect: ControlCallDialect::ClaudeMessages,
                signals: TurnSignals::default(),
                turn_index: 0,
                observed_through_seq: 0,
            },
            selected: chosen.clone(),
            fallbacks: Vec::new(),
            admitted: None,
            selector,
            classifications: None,
            objective: Some(ObjectiveVersion::Undeclared),
        })),
        local_quote_skipped: None,
        chosen,
        rationale: "fixture".into(),
        policy: "stage".into(),
        isl_tokens: 1_000,
        // 60% of the prompt predicted from cache.
        expected_prefill_tokens: 400.0,
        expected_cost_usd: 0.01,
        considered: vec![candidate(opus()), candidate(haiku()), candidate(qwen())],
        turn_policy_digest: String::new(),
        budget_state: Default::default(),
        rate_card: None,
        payer: Default::default(),
        billing: Default::default(),
        budget_draw: None,
        withheld_providers: Vec::new(),
        declared_baseline: None,
        attempts: Vec::new(),
    }
}

/// A decision with no learned evidence.
pub fn unlearned(chosen: Target) -> DecisionRecord {
    decision(chosen, None)
}

/// A provider-measured usage: `cached` of 1000 input tokens from cache.
pub fn measured(cached: u64) -> Usage {
    Usage {
        input_tokens: 1_000,
        cached_input_tokens: cached,
        output_tokens: 10,
        accounting: Accounting::Reported,
        cache_read_source: CacheReadSource::Provider,
        ..Usage::default()
    }
}

/// A usage whose cache read nobody stated.
pub fn unmeasured() -> Usage {
    Usage {
        input_tokens: 1_000,
        output_tokens: 10,
        accounting: Accounting::Reported,
        ..Usage::default()
    }
}

pub fn classification(complexity: TurnComplexity, tier: Option<TierChoice>) -> TurnClassification {
    TurnClassification {
        taxonomy_version: 2,
        intent: Graded {
            value: TurnIntent::Implement,
            confidence: 0.8,
        },
        complexity: Graded {
            value: complexity,
            confidence: 0.7,
        },
        context_dependence: Graded {
            value: ContextDependence::Recent,
            confidence: 0.6,
        },
        tier: tier.map(|value| Graded {
            value,
            confidence: 0.75,
        }),
    }
}

/// One turn of a script.
#[derive(Debug, Clone)]
pub struct Turn {
    pub index: u64,
    pub response_id: ResponseId,
    /// Every `Routed` of the turn, in order.
    pub routed: Vec<u64>,
}

/// A crafted session log.
pub struct Script {
    pub session: SessionId,
    pub events: Vec<SessionEvent>,
    pub clock: u64,
    turns: u64,
    checkpoint: u64,
}

impl Script {
    /// A session of `acme/ada` in the judge-consulting shadow arm.
    pub fn new() -> Self {
        Self::with(Some(Principal::new("acme", "ada")), Some(Arm::Shadow))
    }

    pub fn with(principal: Option<Principal>, arm: Option<Arm>) -> Self {
        Self::named_with("acme/ada/learning", principal, arm)
    }

    /// A session of `acme/ada` in the judge-consulting shadow arm, under `id`.
    pub fn named(id: &str) -> Self {
        Self::named_with(id, Some(Principal::new("acme", "ada")), Some(Arm::Shadow))
    }

    pub fn named_with(id: &str, principal: Option<Principal>, arm: Option<Arm>) -> Self {
        let mut script = Self {
            session: SessionId::new(id),
            events: Vec::new(),
            clock: 1_000,
            turns: 0,
            checkpoint: 0,
        };
        script.push(SessionEventKind::SessionCreated {
            model_policy: "stage".into(),
            principal,
            arm,
        });
        script
    }

    /// Append at `at_ms`, and move the clock there.
    pub fn push_at(&mut self, at_ms: u64, kind: SessionEventKind) -> u64 {
        self.clock = at_ms;
        let seq = self.events.len() as u64 + 1;
        self.events.push(SessionEvent {
            seq,
            session_id: self.session.clone(),
            at_ms,
            kind,
        });
        seq
    }

    /// Append ten milliseconds after the last event.
    pub fn push(&mut self, kind: SessionEventKind) -> u64 {
        self.push_at(self.clock + 10, kind)
    }

    pub fn last_seq(&self) -> u64 {
        self.events.len() as u64
    }

    pub fn begin(&mut self) -> Turn {
        let at = self.clock + 10;
        self.begin_at(at)
    }

    pub fn begin_at(&mut self, at_ms: u64) -> Turn {
        let index = self.turns;
        self.turns += 1;
        let response_id = ResponseId::new(format!("r{index}"));
        self.push_at(
            at_ms,
            SessionEventKind::TurnStarted {
                turn_id: TurnId::new(format!("t{index}")),
                response_id: response_id.clone(),
            },
        );
        Turn {
            index,
            response_id,
            routed: Vec::new(),
        }
    }

    pub fn route_at(&mut self, turn: &mut Turn, at_ms: u64, decision: DecisionRecord) -> u64 {
        let seq = self.push_at(
            at_ms,
            SessionEventKind::Routed {
                response_id: turn.response_id.clone(),
                decision,
            },
        );
        turn.routed.push(seq);
        seq
    }

    pub fn route(&mut self, turn: &mut Turn, decision: DecisionRecord) -> u64 {
        let at = self.clock + 10;
        self.route_at(turn, at, decision)
    }

    pub fn delta_at(&mut self, turn: &Turn, at_ms: u64, text: &str) -> u64 {
        self.push_at(
            at_ms,
            SessionEventKind::OutputTextDelta {
                response_id: turn.response_id.clone(),
                text: text.into(),
            },
        )
    }

    pub fn complete_at(&mut self, turn: &Turn, at_ms: u64, usage: Usage) -> u64 {
        self.push_at(
            at_ms,
            SessionEventKind::ResponseCompleted {
                response_id: turn.response_id.clone(),
                usage,
                provider_reported_cost_usd: None,
                stop_reason: None,
            },
        )
    }

    pub fn complete(&mut self, turn: &Turn, usage: Usage) -> u64 {
        let at = self.clock + 10;
        self.complete_at(turn, at, usage)
    }

    pub fn incomplete(&mut self, turn: &Turn) -> u64 {
        self.push(SessionEventKind::ResponseIncomplete {
            response_id: turn.response_id.clone(),
            reason: IncompleteReason::UpstreamError,
            usage: unmeasured(),
            terminal_attempt: None,
        })
    }

    /// One whole turn dispatched once: begin, route, speak, complete.
    pub fn turn(&mut self, decision: DecisionRecord) -> Turn {
        let mut turn = self.begin();
        self.route(&mut turn, decision);
        let at = self.clock + 10;
        self.delta_at(&turn, at, "answer");
        self.complete(&turn, unmeasured());
        turn
    }

    /// A judged validation carrying `interval`.
    pub fn judged(&mut self, interval: Option<IntervalReview>, verdict: Verdict) -> u64 {
        let n = self.events.len();
        self.push(SessionEventKind::ValidationDecided {
            validation_id: ValidationId::new(format!("val{n}")),
            trigger: TriggerRecord::new(0, 0, Vec::new()),
            arm: Arm::Shadow,
            outcome: ValidationOutcome::Judged {
                side_call_id: SideCallId::new(format!("sc{n}")),
                verdict,
                action: SteerAction::Continue,
                interval: interval.map(Box::new),
            },
        })
    }

    /// A frontier review of exactly `covered`, from the last checkpoint to the
    /// tip, with the label `verdict` gives it. Returns its sequence.
    pub fn review(&mut self, covered: &[&Turn], verdict: Verdict) -> u64 {
        let label = match (verdict.missing_context.is_some(), verdict.on_track) {
            (true, _) => IntervalLabel::Unknown,
            (false, true) => IntervalLabel::Positive,
            (false, false) => IntervalLabel::Negative,
        };
        let decisions = covered
            .iter()
            .flat_map(|turn| {
                turn.routed.iter().map(|routed_seq| ReviewedDecision {
                    routed_seq: *routed_seq,
                    turn_index: turn.index,
                    response_id: turn.response_id.clone(),
                })
            })
            .collect();
        let through = self.last_seq();
        let review = IntervalReview {
            rule_revision: REVIEW_RULE_REVISION,
            after_seq: self.checkpoint,
            through_seq: through,
            decisions,
            gaps: Vec::new(),
            prompt_digest: "digest".into(),
            label,
        };
        self.checkpoint = through;
        self.judged(Some(review), verdict)
    }

    /// A validation that consulted nobody.
    pub fn not_run(&mut self) -> u64 {
        self.push(SessionEventKind::ValidationDecided {
            validation_id: ValidationId::new(format!("nr{}", self.events.len())),
            trigger: TriggerRecord::new(0, 0, Vec::new()),
            arm: Arm::Shadow,
            outcome: ValidationOutcome::NotRun {
                reason: NotRunReason::JudgeUnavailable,
            },
        })
    }

    pub fn intent_at(&mut self, turn: &Turn, requested_at_ms: u64, expires_at_ms: u64) -> u64 {
        self.push_at(
            requested_at_ms,
            SessionEventKind::ClassificationRequested {
                record: ClassificationIntent {
                    call_id: call_id(turn),
                    source_turn_index: turn.index,
                    source_response_id: turn.response_id.clone(),
                    requested_at_ms,
                    expires_at_ms,
                    identity: ClassifierIdentity {
                        model: "jev-1.12".into(),
                        schema: "typesafe.systemone.choice.v1".into(),
                        taxonomy_version: 2,
                        projection_revision: 1,
                        config_revision: 1,
                    },
                    reservation: ReservationRecord {
                        rate_card: ProviderPricing {
                            input_per_mtok_usd: 0.8,
                            cached_input_per_mtok_usd: 0.08,
                            cache_write_per_mtok_usd: 1.0,
                            output_per_mtok_usd: 4.0,
                        },
                        estimated_input_tokens: 300,
                        expected_output_tokens: 50,
                        requested_usd: 0.001,
                        hold_ttl_ms: 60_000,
                        budget_limit_usd: 25.0,
                        budget_window: BudgetWindow::Total,
                        member_ceiling_usd: None,
                        warn_at: 0.8,
                    },
                },
            },
        )
    }

    pub fn intent(&mut self, turn: &Turn) -> u64 {
        let at = self.clock + 10;
        self.intent_at(turn, at, at + 30_000)
    }

    pub fn answer_at(
        &mut self,
        turn: &Turn,
        at_ms: u64,
        classification: TurnClassification,
    ) -> u64 {
        self.push_at(
            at_ms,
            SessionEventKind::ClassificationRecorded {
                record: answer(turn, classification),
            },
        )
    }

    pub fn answer(&mut self, turn: &Turn, classification: TurnClassification) -> u64 {
        let at = self.clock + 10;
        self.answer_at(turn, at, classification)
    }

    pub fn applied(&mut self, through_seq: u64) -> u64 {
        self.push(SessionEventKind::LearningApplied { through_seq })
    }

    /// The fold a successor builds from this log.
    pub async fn state(&self) -> SessionState {
        SessionState::project(
            &ReplayLog::new(self.events.clone()),
            &self.session,
            CacheLedger::new(),
            None,
        )
        .await
        .unwrap()
    }

    /// The backfill replay above `hold_after`.
    pub async fn backfill(&self, hold_after: u64) -> SessionState {
        SessionState::project_learning(
            &ReplayLog::new(self.events.clone()),
            &self.session,
            hold_after,
        )
        .await
        .unwrap()
    }

    /// Every entry the fold produced, from a backfill below the first.
    ///
    /// Only for logs shorter than a page. Asserts the chain is whole, from
    /// `prev_seq == 0` through every link, so a `LearningApplied` in the log
    /// cannot silently drop the entries it acknowledged from what a test
    /// inspects.
    pub async fn entries(&self) -> Vec<LearningEntry> {
        let state = self.backfill(0).await;
        assert_eq!(state.learning_beyond(), 0, "a fixture log within one page");
        let entries = state.learning_page().to_vec();
        let mut prev = 0;
        for entry in &entries {
            assert_eq!(entry.prev_seq, prev, "a whole chain: {entries:?}");
            prev = entry.seq;
        }
        entries
    }

    /// The entry whose source event is `seq`.
    pub async fn entry(&self, seq: u64) -> LearningEntry {
        self.entries()
            .await
            .into_iter()
            .find(|entry| entry.seq == seq)
            .unwrap_or_else(|| panic!("no entry at seq {seq}"))
    }
}

impl Default for Script {
    fn default() -> Self {
        Self::new()
    }
}

pub fn call_id(turn: &Turn) -> ResponseId {
    ResponseId::new(format!("cls-{}", turn.index))
}

pub fn answer(turn: &Turn, classification: TurnClassification) -> ClassificationRecord {
    ClassificationRecord {
        call_id: call_id(turn),
        source_turn_index: turn.index,
        source_response_id: turn.response_id.clone(),
        completed_at_ms: 0,
        outcome: ClassificationOutcome::Classified {
            classification,
            spend: EvaluationSpend::Measured {
                usage: EvaluationUsage {
                    input_tokens: 300,
                    output_tokens: 50,
                },
                usd: 0.0005,
                granted_usd: 0.001,
                settled: SettlementAck::Committed,
            },
            reported_model: None,
        },
    }
}

pub fn on_track() -> Verdict {
    Verdict {
        on_track: true,
        confidence: 0.9,
        divergence: None,
        missing_context: None,
    }
}

pub fn off_track() -> Verdict {
    Verdict {
        on_track: false,
        confidence: 0.8,
        divergence: None,
        missing_context: None,
    }
}

/// A verdict that says it lacked context: the interval is labelled unknown.
pub fn blind() -> Verdict {
    Verdict {
        on_track: true,
        confidence: 0.5,
        divergence: None,
        missing_context: Some("the test output was not shown".into()),
    }
}
