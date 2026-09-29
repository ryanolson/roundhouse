// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! How often the served tier matched the classifier's tier pick, as the
//! snapshot reports it (2026-09-28 addendum, "Jev as a scout", item 2).
//!
//! Built from hand-written logs rather than through the engine, because the
//! orders under test are ones the engine produces only by timing: a review that
//! lands before the classifier's answer, a result delivered twice, a session
//! that retains more unlabelled disagreements than its bound. The fold has to
//! be right about all of them, because the durable log is what a replay reads.

use roundhouse_core::classify::{
    ClassificationIntent, ClassificationOutcome, ClassificationRecord, ClassifierIdentity,
    ContextDependence, EvaluationSpend, EvaluationUsage, Graded, ReservationRecord, SettlementAck,
    TierChoice, TurnClassification, TurnComplexity, TurnIntent,
};
use roundhouse_core::control::{BudgetWindow, Principal, PrincipalKey, ProjectId};
use roundhouse_core::event::{
    Accounting, NotRunReason, SessionEvent, SessionEventKind, Usage, ValidationOutcome,
};
use roundhouse_core::ids::{ResponseId, SessionId, SideCallId, TurnId, ValidationId};
use roundhouse_core::metrics::{
    MetricsConfig, MetricsFold, MetricsRecorder, MetricsSnapshot, Scope, ShadowPricing,
    TierAgreement, TierDisagreements,
};
use roundhouse_core::routing::learn::{
    ActiveMode, Band, EpochId, LEARNING_CREDIT_REVISION, LEARNING_INPUT_REVISION, LearnedChoice,
    LearnedEvidence, LearnedInput, PriorBand, ReadFailure, RecipeEvidence, StoreRead, Unmet,
};
use roundhouse_core::routing::{
    DecisionRecord, DecisionSource, LocalFeatures, Pick, PickerMode, ProviderPricing,
    SelectionSnapshot, SelectorSnapshot, StageEvidence, StageOutcome, Target, Tier, TurnSignals,
};
use roundhouse_core::session::MAX_REVIEW_DECISIONS;
use roundhouse_core::validate::{
    Arm, ControlCallDialect, IntervalLabel, IntervalReview, REVIEW_RULE_REVISION, ReviewedDecision,
    SteerAction, TriggerRecord, Verdict,
};

const CAPABLE: (&str, &str) = ("anthropic", "opus");
const EFFICIENT: (&str, &str) = ("anthropic", "haiku");

fn frontier((provider, model): (&str, &str)) -> Target {
    Target::Frontier {
        provider: provider.into(),
        model: model.into(),
    }
}

fn local(model: &str) -> Target {
    Target::Local {
        worker_id: 1,
        dp_rank: 0,
        model: model.into(),
    }
}

/// The recipe every staged turn below ran under: one hosted model per tier and
/// a local worker at the tail of the efficient tier.
fn evidence(pick: Tier, outcome: StageOutcome) -> StageEvidence {
    StageEvidence {
        capable: vec!["anthropic/opus".into()],
        efficient: vec!["anthropic/haiku".into(), "local/qwen".into()],
        picker: PickerMode::EfficientFirst,
        confidence_threshold: 0.5,
        pick: Pick {
            tier: pick,
            source: DecisionSource::Dimensions,
            score: 0.0,
            confidence: Some(0.9),
        },
        outcome,
    }
}

fn decision(chosen: Target, selector: Option<SelectorSnapshot>) -> DecisionRecord {
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
            objective: None,
        })),
        local_quote_skipped: None,
        chosen,
        rationale: "test".into(),
        policy: "test".into(),
        isl_tokens: 100,
        expected_prefill_tokens: 0.0,
        expected_cost_usd: 0.0,
        considered: Vec::new(),
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

fn staged(chosen: Target, pick: Tier, outcome: StageOutcome) -> DecisionRecord {
    decision(
        chosen,
        Some(SelectorSnapshot::stage(evidence(pick, outcome))),
    )
}

/// A stage decision served by the tier the scorer picked.
fn served(tier: Tier) -> DecisionRecord {
    let target = match tier {
        Tier::Capable => frontier(CAPABLE),
        Tier::Efficient => frontier(EFFICIENT),
    };
    staged(target, tier, StageOutcome::Served { tier })
}

fn classification(tier: Option<TierChoice>) -> TurnClassification {
    TurnClassification {
        taxonomy_version: 2,
        intent: Graded {
            value: TurnIntent::Implement,
            confidence: 0.8,
        },
        complexity: Graded {
            value: TurnComplexity::Routine,
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

/// One session's log, appended at increasing sequence numbers like the store.
struct Log {
    session: SessionId,
    events: Vec<SessionEvent>,
    turns: u64,
}

impl Log {
    fn new(session: &str, principal: Principal) -> Self {
        let mut log = Self {
            session: SessionId::new(session),
            events: Vec::new(),
            turns: 0,
        };
        log.push(SessionEventKind::SessionCreated {
            model_policy: "stage".into(),
            principal: Some(principal),
            arm: None,
        });
        log
    }

    fn push(&mut self, kind: SessionEventKind) -> u64 {
        let seq = self.events.len() as u64 + 1;
        self.events.push(SessionEvent {
            seq,
            session_id: self.session.clone(),
            at_ms: 1_000 + seq * 10,
            kind,
        });
        seq
    }

    /// One turn dispatched once per decision, in order: the last dispatch is
    /// the one that served. Answers the turn's response id and the sequence of
    /// its serving `Routed`.
    fn turn(&mut self, dispatches: Vec<DecisionRecord>) -> Turn {
        let index = self.turns;
        self.turns += 1;
        let response_id = ResponseId::new(format!("r{index}"));
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new(format!("t{index}")),
            response_id: response_id.clone(),
        });
        let mut routed_seq = 0;
        for decision in dispatches {
            routed_seq = self.push(SessionEventKind::Routed {
                response_id: response_id.clone(),
                decision,
            });
        }
        self.push(SessionEventKind::ResponseCompleted {
            response_id: response_id.clone(),
            usage: Usage {
                input_tokens: 100,
                output_tokens: 10,
                accounting: Accounting::Reported,
                ..Default::default()
            },
            provider_reported_cost_usd: None,
            stop_reason: None,
        });
        Turn {
            index,
            response_id,
            routed_seq,
        }
    }

    /// The classification intent the engine writes after `turn`'s terminal.
    fn intent(&mut self, turn: &Turn) -> &mut Self {
        self.push(SessionEventKind::ClassificationRequested {
            record: ClassificationIntent {
                call_id: call_id(turn),
                source_turn_index: turn.index,
                source_response_id: turn.response_id.clone(),
                requested_at_ms: 0,
                expires_at_ms: 60_000,
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
        });
        self
    }

    /// The classifier's answer about `turn`, delivered by a later writer.
    fn answer(&mut self, turn: &Turn, tier: Option<TierChoice>) -> &mut Self {
        self.push(SessionEventKind::ClassificationRecorded {
            record: ClassificationRecord {
                call_id: call_id(turn),
                source_turn_index: turn.index,
                source_response_id: turn.response_id.clone(),
                completed_at_ms: 0,
                outcome: ClassificationOutcome::Classified {
                    classification: classification(tier),
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
            },
        });
        self
    }

    /// Intent and answer back to back: the common case.
    fn classified(&mut self, turn: &Turn, tier: TierChoice) -> &mut Self {
        self.intent(turn).answer(turn, Some(tier))
    }

    /// A frontier review covering exactly `covered`, labelled `label`.
    fn review(&mut self, covered: &[&Turn], label: IntervalLabel) -> &mut Self {
        let decisions = covered
            .iter()
            .map(|turn| ReviewedDecision {
                routed_seq: turn.routed_seq,
                turn_index: turn.index,
                response_id: turn.response_id.clone(),
            })
            .collect();
        let review = IntervalReview {
            rule_revision: REVIEW_RULE_REVISION,
            after_seq: 0,
            through_seq: self.events.len() as u64,
            decisions,
            gaps: Vec::new(),
            prompt_digest: "digest".into(),
            label,
        };
        self.judged(Some(review))
    }

    fn judged(&mut self, interval: Option<IntervalReview>) -> &mut Self {
        let n = self.events.len();
        self.push(SessionEventKind::ValidationDecided {
            validation_id: ValidationId::new(format!("val{n}")),
            trigger: TriggerRecord::new(0, 0, Vec::new()),
            arm: Arm::Shadow,
            outcome: ValidationOutcome::Judged {
                side_call_id: SideCallId::new(format!("sc{n}")),
                verdict: Verdict {
                    on_track: true,
                    confidence: 0.9,
                    divergence: None,
                    missing_context: None,
                },
                action: SteerAction::Continue,
                interval: interval.map(Box::new),
            },
        });
        self
    }

    fn not_run(&mut self) -> &mut Self {
        let n = self.events.len();
        self.push(SessionEventKind::ValidationDecided {
            validation_id: ValidationId::new(format!("val{n}")),
            trigger: TriggerRecord::new(0, 0, Vec::new()),
            arm: Arm::Shadow,
            outcome: ValidationOutcome::NotRun {
                reason: NotRunReason::BudgetRefused,
            },
        });
        self
    }

    /// The last event again at a new sequence: a redelivery the watermark
    /// cannot refuse.
    fn redeliver_last(&mut self) -> &mut Self {
        let kind = self.events.last().expect("an event").kind.clone();
        self.push(kind);
        self
    }
}

struct Turn {
    index: u64,
    response_id: ResponseId,
    routed_seq: u64,
}

fn call_id(turn: &Turn) -> ResponseId {
    ResponseId::new(format!("call-{}", turn.response_id))
}

fn config() -> MetricsConfig {
    MetricsConfig::new(ShadowPricing::new(Vec::new()))
}

fn recorder(logs: &[&Log]) -> MetricsRecorder {
    let recorder = MetricsRecorder::new();
    for log in logs {
        recorder.record(&log.events);
    }
    recorder
}

fn deployment(recorder: &MetricsRecorder) -> TierAgreement {
    recorder.snapshot(&config(), 0).evaluation.agreement
}

fn project(recorder: &MetricsRecorder, id: &str) -> TierAgreement {
    recorder
        .snapshot_for_project(&ProjectId::new(id), &config(), 0)
        .evaluation
        .agreement
}

/// Both partitions the wire type promises, on every document a test reads.
fn assert_partitions(agreement: &TierAgreement) {
    assert_eq!(
        agreement.answered,
        agreement.agree + agreement.disagree + agreement.not_comparable,
        "{agreement:?}"
    );
    let d = &agreement.disagreements;
    assert_eq!(
        agreement.disagree,
        d.jev_capable_served_efficient + d.jev_efficient_served_capable,
        "{agreement:?}"
    );
    assert_eq!(
        agreement.disagree,
        d.positive + d.negative + d.unknown + d.unlabeled,
        "{agreement:?}"
    );
}

// ---------------------------------------------------------------------------

/// **Each project sees its own comparison, and the deployment sees the sum.**
///
/// `acme` has one agreement and one disagreement in each direction; `globex`
/// has one agreement and one answer on a turn no recipe routed, which has no
/// tier to compare and must not be read as either. A result delivered a second
/// time at a new sequence is refused by the evaluation join, so it books no
/// second answer here either.
#[test]
fn agreement_counts_the_served_tier_against_the_jev_tier_for_each_project() {
    let mut acme = Log::new("s-acme", Principal::new("acme", "ada"));
    let agree = acme.turn(vec![served(Tier::Capable)]);
    acme.classified(&agree, TierChoice::Capable);
    let up = acme.turn(vec![served(Tier::Efficient)]);
    acme.classified(&up, TierChoice::Capable).redeliver_last();
    let down = acme.turn(vec![served(Tier::Capable)]);
    acme.classified(&down, TierChoice::Efficient);

    let mut globex = Log::new("s-globex", Principal::new("globex", "zoe"));
    let agree = globex.turn(vec![served(Tier::Efficient)]);
    globex.classified(&agree, TierChoice::Efficient);
    // Routed by a policy with no tiers at all.
    let untiered = globex.turn(vec![decision(frontier(CAPABLE), None)]);
    globex.classified(&untiered, TierChoice::Capable);

    let recorder = recorder(&[&acme, &globex]);

    let acme = project(&recorder, "acme");
    assert_eq!(
        acme,
        TierAgreement {
            answered: 3,
            agree: 1,
            disagree: 2,
            not_comparable: 0,
            disagreements: TierDisagreements {
                jev_capable_served_efficient: 1,
                jev_efficient_served_capable: 1,
                unlabeled: 2,
                ..Default::default()
            },
        }
    );
    let globex = project(&recorder, "globex");
    assert_eq!(
        globex,
        TierAgreement {
            answered: 2,
            agree: 1,
            disagree: 0,
            not_comparable: 1,
            disagreements: TierDisagreements::default(),
        }
    );
    let all = deployment(&recorder);
    assert_eq!(all.answered, acme.answered + globex.answered);
    assert_eq!(all.agree, acme.agree + globex.agree);
    assert_eq!(all.disagree, acme.disagree + globex.disagree);
    for agreement in [&acme, &globex, &all] {
        assert_partitions(agreement);
    }
}

/// **The served tier is the dispatched target's, not the scorer's pick.**
///
/// Both turns were picked `efficient` and moved by the cost guard onto the
/// capable model. The first was served there, so a classifier that said
/// `capable` agrees with it. The second failed over, and under ruling 4 the
/// guarded turn's fallback runs through the efficient tier first: the serving
/// dispatch is the efficient model, so the same answer disagrees. Reading the
/// pick, the outcome arm, or the first dispatch each gets one of the two wrong.
#[test]
fn a_cost_guarded_turn_compares_the_served_tier_not_the_pick() {
    let guard = || StageOutcome::CostGuard {
        displaced: "anthropic/haiku".into(),
    };
    let mut log = Log::new("s-guard", Principal::new("acme", "ada"));
    let guarded = log.turn(vec![staged(frontier(CAPABLE), Tier::Efficient, guard())]);
    log.classified(&guarded, TierChoice::Capable);
    let failed_over = log.turn(vec![
        staged(frontier(CAPABLE), Tier::Efficient, guard()),
        staged(frontier(EFFICIENT), Tier::Efficient, guard()),
    ]);
    log.classified(&failed_over, TierChoice::Capable);

    let agreement = deployment(&recorder(&[&log]));
    assert_eq!(agreement.answered, 2, "{agreement:?}");
    assert_eq!(agreement.agree, 1, "{agreement:?}");
    assert_eq!(agreement.disagree, 1, "{agreement:?}");
    assert_eq!(
        agreement.disagreements.jev_capable_served_efficient, 1,
        "{agreement:?}"
    );
    assert_partitions(&agreement);
}

/// A learned decision that served `chosen`, under the same recipe as
/// [`evidence`]. Only the recipe decides the served tier; the rest is the
/// smallest record a learned turn can write.
fn learned(chosen: Target) -> DecisionRecord {
    let evidence = LearnedEvidence {
        mode: ActiveMode::Live,
        epoch: EpochId::new([7; 16]),
        input_revision: LEARNING_INPUT_REVISION,
        credit_revision: LEARNING_CREDIT_REVISION,
        input: LearnedInput {
            rules_pick: Tier::Capable,
            newest: Band::None,
            prior: PriorBand::Absent,
            tool_turn: false,
        },
        view: StoreRead::Unavailable {
            reason: ReadFailure::ReadTimedOut,
        },
        recipe: RecipeEvidence {
            capable: vec!["anthropic/opus".into()],
            efficient: vec!["anthropic/haiku".into(), "local/qwen".into()],
            picker: PickerMode::EfficientFirst,
            confidence_threshold: 0.5,
        },
        plans: Vec::new(),
        choice: LearnedChoice::ConstraintUnmet {
            unmet: vec![Unmet::ReadTimedOut],
        },
        exploration: None,
        propensity: 1.0,
    };
    decision(chosen, Some(SelectorSnapshot::learned(evidence)))
}

/// **A learned turn is compared by the tier its recipe names for the served
/// target**, the same rule as a stage turn. Read as "no tier recipe made
/// this", every learned turn would drop out as not comparable, and the
/// agreement report would go blind the day a project enables the learner.
#[test]
fn a_learned_turn_compares_the_recipe_tier_of_the_served_target() {
    let mut log = Log::new("s-learned", Principal::new("acme", "ada"));
    let agree = log.turn(vec![learned(frontier(EFFICIENT))]);
    log.classified(&agree, TierChoice::Efficient);
    let disagree = log.turn(vec![learned(frontier(CAPABLE))]);
    log.classified(&disagree, TierChoice::Efficient);

    let agreement = deployment(&recorder(&[&log]));
    assert_eq!(agreement.answered, 2, "{agreement:?}");
    assert_eq!(agreement.not_comparable, 0, "{agreement:?}");
    assert_eq!(agreement.agree, 1, "{agreement:?}");
    assert_eq!(
        agreement.disagreements.jev_efficient_served_capable, 1,
        "{agreement:?}"
    );
    assert_partitions(&agreement);
}

/// **A disagreement takes the label of the review that covered it, and of no
/// other.**
///
/// One session holds two reviews: a positive one over the agreeing turn and a
/// negative one over the disagreeing turn. A second session reuses the same
/// response id and turn index, and its covering review is unknown. A join on
/// the wrong turn, or across sessions, lands a label in the wrong bucket.
#[test]
fn a_disagreement_takes_the_label_of_the_covering_interval() {
    let mut first = Log::new("s-one", Principal::new("acme", "ada"));
    let agree = first.turn(vec![served(Tier::Capable)]);
    first.classified(&agree, TierChoice::Capable);
    let disagree = first.turn(vec![served(Tier::Efficient)]);
    first.classified(&disagree, TierChoice::Capable);
    first.review(&[&agree], IntervalLabel::Positive);
    first.review(&[&disagree], IntervalLabel::Negative);

    let mut second = Log::new("s-two", Principal::new("acme", "ada"));
    let _ = second.turn(vec![served(Tier::Capable)]);
    let same_ids = second.turn(vec![served(Tier::Capable)]);
    second.classified(&same_ids, TierChoice::Efficient);
    second.review(&[&same_ids], IntervalLabel::Unknown);

    let agreement = deployment(&recorder(&[&first, &second]));
    assert_eq!(agreement.disagree, 2, "{agreement:?}");
    assert_eq!(
        agreement.disagreements,
        TierDisagreements {
            jev_capable_served_efficient: 1,
            jev_efficient_served_capable: 1,
            positive: 0,
            negative: 1,
            unknown: 1,
            unlabeled: 0,
            evicted: 0,
        }
    );
    assert_partitions(&agreement);
}

/// **The review can land before the answer**, because the classifier's result
/// is delivered at the start of a later turn and the validation runs inside
/// the turn before it. The label must still reach the disagreement.
#[test]
fn a_review_before_the_answer_still_labels_the_disagreement() {
    let mut log = Log::new("s-early", Principal::new("acme", "ada"));
    let turn = log.turn(vec![served(Tier::Efficient)]);
    log.intent(&turn);
    log.review(&[&turn], IntervalLabel::Negative);
    log.answer(&turn, Some(TierChoice::Capable));

    let agreement = deployment(&recorder(&[&log]));
    assert_eq!(agreement.disagree, 1, "{agreement:?}");
    assert_eq!(agreement.disagreements.negative, 1, "{agreement:?}");
    assert_eq!(agreement.disagreements.unlabeled, 0, "{agreement:?}");
    assert_partitions(&agreement);
}

/// **No covering review, no label.** A review of a different turn, a
/// validation that asked nobody, and a judged review written before coverage
/// existed all leave the disagreement unlabelled.
#[test]
fn a_disagreement_without_a_covering_review_stays_unlabeled() {
    let mut log = Log::new("s-open", Principal::new("acme", "ada"));
    let other = log.turn(vec![served(Tier::Capable)]);
    log.classified(&other, TierChoice::Capable);
    let disagree = log.turn(vec![served(Tier::Capable)]);
    log.classified(&disagree, TierChoice::Efficient);
    log.review(&[&other], IntervalLabel::Positive);
    log.not_run();
    log.judged(None);

    let agreement = deployment(&recorder(&[&log]));
    assert_eq!(agreement.disagree, 1, "{agreement:?}");
    assert_eq!(
        agreement.disagreements,
        TierDisagreements {
            jev_efficient_served_capable: 1,
            unlabeled: 1,
            ..Default::default()
        }
    );
    assert_partitions(&agreement);
}

/// **A local-only session reaches no classifier, so it adds no row.** Its
/// project reports nothing, and the deployment reports exactly the frontier
/// project's comparison beside it.
#[test]
fn a_local_only_session_reports_no_agreement_row() {
    let mut edge = Log::new("s-edge", Principal::new("edge", "eve"));
    for _ in 0..3 {
        let _ = edge.turn(vec![staged(
            local("qwen"),
            Tier::Efficient,
            StageOutcome::Served {
                tier: Tier::Efficient,
            },
        )]);
    }
    let mut acme = Log::new("s-acme", Principal::new("acme", "ada"));
    let turn = acme.turn(vec![served(Tier::Capable)]);
    acme.classified(&turn, TierChoice::Capable);

    let recorder = recorder(&[&edge, &acme]);
    assert_eq!(project(&recorder, "edge"), TierAgreement::default());
    let acme = project(&recorder, "acme");
    assert_eq!(acme.answered, 1, "{acme:?}");
    assert_eq!(deployment(&recorder), acme);
}

/// **A replay rebuilds the same counts**, whether the log arrives whole or
/// event by event with every event delivered twice.
#[test]
fn replay_rebuilds_the_same_agreement_counts() {
    let mut log = Log::new("s-replay", Principal::new("acme", "ada"));
    let agree = log.turn(vec![served(Tier::Capable)]);
    log.classified(&agree, TierChoice::Capable);
    let labelled = log.turn(vec![served(Tier::Efficient)]);
    log.classified(&labelled, TierChoice::Capable);
    let early = log.turn(vec![served(Tier::Capable)]);
    log.intent(&early);
    log.review(&[&agree, &labelled, &early], IntervalLabel::Positive);
    log.answer(&early, Some(TierChoice::Efficient));
    let open = log.turn(vec![served(Tier::Efficient)]);
    log.classified(&open, TierChoice::Capable);

    let expected = TierAgreement {
        answered: 4,
        agree: 1,
        disagree: 3,
        not_comparable: 0,
        disagreements: TierDisagreements {
            jev_capable_served_efficient: 2,
            jev_efficient_served_capable: 1,
            positive: 2,
            unlabeled: 1,
            ..Default::default()
        },
    };

    let mut whole = MetricsFold::new();
    whole.extend(&log.events);
    let mut live = MetricsFold::new();
    for event in &log.events {
        live.apply(event);
        live.apply(event);
    }
    let snapshot = |fold: &MetricsFold| {
        MetricsSnapshot::build(fold, Scope::Deployment, &config(), 0)
            .evaluation
            .agreement
    };
    assert_eq!(snapshot(&whole), expected);
    assert_eq!(snapshot(&live), expected);
    let key = PrincipalKey::from(&Principal::new("acme", "ada"));
    assert_eq!(
        MetricsSnapshot::build(&live, Scope::Principal(&key), &config(), 0)
            .evaluation
            .agreement,
        expected
    );
}

/// **The retained disagreements are bounded, and an eviction is counted.**
///
/// One more unlabelled disagreement than the bound evicts the oldest. A review
/// that then covers the evicted turn labels nothing, and one that covers the
/// next turn still labels it.
#[test]
fn retained_disagreements_are_bounded_and_an_eviction_is_counted() {
    let mut log = Log::new("s-bound", Principal::new("acme", "ada"));
    let mut turns = Vec::new();
    for _ in 0..=MAX_REVIEW_DECISIONS {
        let turn = log.turn(vec![served(Tier::Efficient)]);
        log.classified(&turn, TierChoice::Capable);
        turns.push(turn);
    }
    log.review(&[&turns[0]], IntervalLabel::Negative);
    log.review(&[&turns[1]], IntervalLabel::Positive);

    let agreement = deployment(&recorder(&[&log]));
    let total = MAX_REVIEW_DECISIONS as u64 + 1;
    assert_eq!(agreement.disagree, total, "{agreement:?}");
    assert_eq!(
        agreement.disagreements,
        TierDisagreements {
            jev_capable_served_efficient: total,
            positive: 1,
            negative: 0,
            unlabeled: total - 1,
            evicted: 1,
            ..Default::default()
        }
    );
    assert_partitions(&agreement);
}

/// One more unanswered intent than the bound: the oldest intent's slot is
/// evicted before its answer lands. Answers the log and the evicted turn.
fn a_session_past_the_bound_with_every_answer_pending() -> (Log, Turn) {
    let mut log = Log::new("s-pending", Principal::new("acme", "ada"));
    let mut turns = Vec::new();
    for _ in 0..=MAX_REVIEW_DECISIONS {
        let turn = log.turn(vec![served(Tier::Capable)]);
        log.intent(&turn);
        turns.push(turn);
    }
    (log, turns.swap_remove(0))
}

/// **An answer whose slot the bound dropped is not comparable**, even when it
/// names the tier the turn was served on: the fold no longer knows which tier
/// that was, and booking it as agreement would count a comparison nobody made.
#[test]
fn an_answer_to_an_evicted_intent_is_not_comparable() {
    let (mut log, evicted) = a_session_past_the_bound_with_every_answer_pending();
    log.answer(&evicted, Some(TierChoice::Capable));

    let agreement = deployment(&recorder(&[&log]));
    assert_eq!(
        agreement,
        TierAgreement {
            answered: 1,
            not_comparable: 1,
            ..Default::default()
        }
    );
    assert_partitions(&agreement);
}

/// **Only a dropped disagreement is an eviction.** A dropped intent has no
/// answer yet, so it is no disagreement the report stopped waiting to label;
/// counting it would make `evicted` exceed the unlabelled disagreements it is a
/// part of.
#[test]
fn an_evicted_unanswered_intent_is_not_counted_as_evicted() {
    let (log, _) = a_session_past_the_bound_with_every_answer_pending();

    let agreement = deployment(&recorder(&[&log]));
    assert_eq!(agreement, TierAgreement::default());
    assert_eq!(agreement.disagreements.evicted, 0, "{agreement:?}");
}
