// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The durable half of a learned decision: `SelectorBranch::Learned` on a
//! `Routed` record.
//!
//! Replay reads this record and never reads the learner store again, so the
//! record must come back whole. It must also cost nothing when absent — every
//! event of every kind is as wide as the widest — and a log written before it
//! existed must still read. The rationale beside it is republished into the
//! calling model's context, so it names what was chosen and never a price.

use std::mem::size_of;

use roundhouse_core::control::{Billing, BudgetState, Payer};
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::ResponseId;
use roundhouse_core::routing::learn::{
    ActiveMode, Band, CacheReuse, CostCorrection, CostEvidence, Draw, EpochId, ExplorationEvidence,
    GateEvidence, GateResult, GrantCheck, JevCounts, KeyLevel, LatencySum, LatencyTerm,
    LearnedChoice, LearnedEvidence, LearnedEvidenceError, LearnedEvidenceParts, LearnedInput,
    LevelView, OnInfeasible, PlanEvidence, PriorBand, ReadFailure, ReadView, StoreRead, Strategy,
    StrategyCounts, StrategySetError, TargetOps, TtftEvidence, Unmet,
};
use roundhouse_core::routing::{
    AffinityEvidence, Candidate, DecisionRecord, DecisionSource, LocalFeatures, Pick, PickerMode,
    RecipeEvidence, SelectionSnapshot, SelectorBranch, SelectorSnapshot, StageEvidence,
    StageOutcome, Target, Tier, TurnSignals,
};
use roundhouse_core::validate::ControlCallDialect;

/// `SessionEventKind`'s size at `75ccf2c`, the commit before the learned
/// branch, on the pinned toolchain (`rust-toolchain.toml`, 1.96.1).
///
/// A ceiling, not an exact pin: another toolchain may lay the enum out
/// smaller, and that is no regression. The claim that the learned arm costs
/// nothing is the `BranchBeforeLearned` comparison, which holds on any
/// toolchain; this bound only catches growth from elsewhere.
const SESSION_EVENT_KIND_MAX_BYTES: usize = 344;

fn hosted(model: &str) -> Target {
    Target::Frontier {
        provider: "openai".into(),
        model: model.into(),
    }
}

fn input() -> LearnedInput {
    LearnedInput {
        rules_pick: Tier::Capable,
        newest: Band::Low,
        prior: PriorBand::NoHigh,
        tool_turn: true,
    }
}

/// Every price in this fixture is exactly representable in binary, so the
/// round trip below compares bits rather than a decimal rendering.
fn plan(strategy: Strategy, pick: Pick, first: &str, gate: GateResult) -> PlanEvidence {
    PlanEvidence {
        strategy,
        pick,
        outcome: StageOutcome::Served { tier: pick.tier },
        first: hosted(first),
        cost: CostEvidence {
            quoted_usd: 0.4375,
            adjusted_usd: 0.5625,
            correction: CostCorrection::Applied,
        },
        ttft: TtftEvidence {
            quoted_ms: 812.5,
            adjusted_ms: 1_437.5,
            residual: LatencyTerm::Applied { mean_ms: 500 },
            overhead: LatencyTerm::TooFewSamples,
        },
        grant: GrantCheck::Admits,
        latency_met: true,
        gate: GateEvidence {
            level: Some(KeyLevel::L1),
            result: gate,
        },
    }
}

fn rules_pick() -> Pick {
    Pick {
        tier: Tier::Capable,
        source: DecisionSource::Override,
        score: 0.0,
        confidence: None,
    }
}

fn forced(tier: Tier) -> Pick {
    Pick {
        tier,
        source: DecisionSource::Strategy,
        score: 0.0,
        confidence: None,
    }
}

fn evidence(mode: ActiveMode, choice: LearnedChoice) -> LearnedEvidence {
    LearnedEvidence::new(parts(mode, choice)).expect("the fixture holds every configured plan")
}

fn parts(mode: ActiveMode, choice: LearnedChoice) -> LearnedEvidenceParts {
    let input = input();
    LearnedEvidenceParts {
        mode,
        epoch: EpochId::new([
            0xab, 0xcd, 0xef, 0x01, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13,
        ]),
        input_revision: 1,
        credit_revision: 1,
        input,
        view: StoreRead::Read(ReadView {
            levels: input
                .keys()
                .into_iter()
                .map(|key| LevelView {
                    key,
                    strategies: vec![StrategyCounts {
                        strategy: Strategy::Efficient,
                        pos_units: 4_500,
                        n_units: 5_000,
                        sessions: 21,
                    }],
                    jev: JevCounts {
                        capable: 1,
                        efficient: 4,
                    },
                })
                .collect(),
            targets: vec![TargetOps {
                target: "openai/sol".into(),
                latency: LatencySum {
                    sum_ms: -1_200,
                    n: 30,
                },
                failover: 2,
                cache: CacheReuse {
                    predicted_permille: 21_000,
                    observed_permille: 18_500,
                    n: 25,
                },
            }],
            overhead: LatencySum {
                sum_ms: 9_000,
                n: 30,
            },
        }),
        recipe: RecipeEvidence {
            capable: vec!["openai/sol".into()],
            efficient: vec!["openai/luna".into()],
            picker: PickerMode::EfficientFirst,
            confidence_threshold: 0.5,
        },
        plans: vec![
            plan(Strategy::Rules, rules_pick(), "sol", GateResult::Unproven),
            plan(
                Strategy::Efficient,
                forced(Tier::Efficient),
                "luna",
                GateResult::Pass,
            ),
            plan(
                Strategy::Capable,
                forced(Tier::Capable),
                "sol",
                GateResult::BelowFloor,
            ),
        ],
        choice,
        exploration: Some(ExplorationEvidence {
            on_infeasible: OnInfeasible::ServeRules,
            draw: Draw {
                rate: 0.75,
                member: 17,
            },
            possible: true,
            set: vec![Strategy::Efficient],
        }),
        propensity: 0.96875,
    }
}

fn features() -> LocalFeatures {
    LocalFeatures {
        extractor_revision: 1,
        dialect: ControlCallDialect::ClaudeMessages,
        signals: TurnSignals::default(),
        turn_index: 4,
        observed_through_seq: 37,
    }
}

fn record(chosen: &str, selector: SelectorSnapshot, rationale: String) -> DecisionRecord {
    DecisionRecord {
        block_marker: None,
        chosen: hosted(chosen),
        rationale,
        policy: "learned".into(),
        isl_tokens: 10_000,
        expected_prefill_tokens: 1_000.0,
        expected_cost_usd: 0.5,
        considered: Vec::<Candidate>::new(),
        turn_policy_digest: "digest".into(),
        budget_state: BudgetState::Unconstrained,
        rate_card: None,
        payer: Payer::Deployment,
        billing: Billing::Billed,
        budget_draw: None,
        withheld_providers: Vec::new(),
        declared_baseline: None,
        attempts: Vec::new(),
        local_quote_skipped: None,
        selection: Some(Box::new(SelectionSnapshot {
            features: features(),
            selected: hosted(chosen),
            fallbacks: Vec::new(),
            admitted: Some(vec![hosted("sol"), hosted("luna")]),
            selector: Some(selector),
            classifications: None,
            objective: None,
        })),
    }
}

/// **The claim.** A learned decision survives the log whole, and carrying it
/// widens neither the selector branch nor the session event.
#[test]
fn learned_evidence_round_trips_and_session_event_size_is_unchanged() {
    let learned = evidence(
        ActiveMode::Live,
        LearnedChoice::Explore {
            strategy: Strategy::Efficient,
            member: 0,
        },
    );
    let event = SessionEventKind::Routed {
        response_id: ResponseId::new("resp_1"),
        decision: record(
            "luna",
            SelectorSnapshot::learned(learned.clone()),
            "learned".into(),
        ),
    };
    let json = serde_json::to_string(&event).expect("a learned record serializes");
    let back: SessionEventKind = serde_json::from_str(&json).expect("and reads back");
    assert_eq!(back, event);
    let SessionEventKind::Routed { decision, .. } = &back else {
        unreachable!("the round trip kept the kind")
    };
    let selection = decision.selection.as_ref().expect("selection kept");
    match &selection.selector.as_ref().expect("selector kept").branch {
        SelectorBranch::Learned(evidence) => assert_eq!(**evidence, learned),
        other => panic!("expected the learned branch, got {other:?}"),
    }
    // A served forced pick reports its own source, so the handoff gate that
    // reads it does not narrate.
    assert_eq!(selection.source(), Some(DecisionSource::Strategy));
    assert_eq!(
        learned.recipe.tier_of(&hosted("luna")),
        Some(Tier::Efficient)
    );

    // An unavailable store is recorded as such, not as an empty view.
    let unavailable = LearnedEvidence::new(LearnedEvidenceParts {
        view: StoreRead::Unavailable {
            reason: ReadFailure::ReadTimedOut,
        },
        ..learned.clone().into_parts()
    })
    .expect("the same plans");
    let json = serde_json::to_string(&unavailable).expect("serializes");
    assert_eq!(
        serde_json::from_str::<LearnedEvidence>(&json).expect("reads"),
        unavailable
    );

    // The width claims. The mirror is the branch as it was before the learned
    // arm: equal size means the arm, boxed, costs the enum nothing.
    #[allow(dead_code)]
    enum BranchBeforeLearned {
        Affinity(AffinityEvidence),
        EscalationAudit { audit_every: u64 },
        Stage(StageEvidence),
    }
    assert_eq!(
        size_of::<SelectorBranch>(),
        size_of::<BranchBeforeLearned>(),
        "the learned arm widened SelectorBranch"
    );
    assert!(
        size_of::<SessionEventKind>() <= SESSION_EVENT_KIND_MAX_BYTES,
        "SessionEventKind grew past its size before the learned branch: {} bytes",
        size_of::<SessionEventKind>()
    );
}

/// **A control.** A stage decision as today's build writes it still decodes,
/// as the stage branch, with the same derived source: the learned arm is
/// additive and no existing record changes meaning.
#[test]
fn a_record_without_learned_evidence_still_decodes() {
    let historical = serde_json::json!({
        "chosen": { "kind": "frontier", "provider": "openai", "model": "sol" },
        "rationale": "stage router: strong tier (openai/sol) by override",
        "policy": "stage",
        "isl_tokens": 10_000,
        "expected_prefill_tokens": 1_000.0,
        "expected_cost_usd": 0.05,
        "considered": [],
        "selection": {
            "features": {
                "extractor_revision": 1,
                "dialect": "claude_messages",
                "signals": serde_json::to_value(TurnSignals::default()).unwrap(),
                "turn_index": 2,
                "observed_through_seq": 9
            },
            "selected": { "kind": "frontier", "provider": "openai", "model": "sol" },
            "selector": {
                "algorithm_revision": 1,
                "branch": {
                    "kind": "stage",
                    "capable": ["openai/sol"],
                    "efficient": ["openai/luna"],
                    "picker": "efficient_first",
                    "confidence_threshold": 0.5,
                    "pick": { "tier": "capable", "source": "override", "score": 0.0, "confidence": null },
                    "outcome": { "kind": "served", "tier": "capable" }
                }
            }
        }
    });
    let decoded: DecisionRecord =
        serde_json::from_value(historical).expect("a stage record from before the learner reads");
    let selection = decoded.selection.expect("the selection is kept");
    match &selection
        .selector
        .as_ref()
        .expect("the selector is kept")
        .branch
    {
        SelectorBranch::Stage(evidence) => {
            assert_eq!(evidence.pick.source, DecisionSource::Override);
        }
        other => panic!("expected the stage branch, got {other:?}"),
    }
    assert_eq!(selection.source(), Some(DecisionSource::Override));
}

/// **The claim.** The learned rationale names the strategy, the tier, the
/// target, the epoch prefix and the key, and no price — whatever it chose.
#[test]
fn the_rationale_carries_no_price() {
    let cases = [
        (
            evidence(
                ActiveMode::Live,
                LearnedChoice::Exploit {
                    strategy: Strategy::Efficient,
                },
            ),
            "efficient",
            "weak",
            "openai/luna",
            "l1/capable.low",
        ),
        // Shadow: the learned choice is recorded and `rules` serves.
        (
            evidence(
                ActiveMode::Shadow,
                LearnedChoice::Exploit {
                    strategy: Strategy::Efficient,
                },
            ),
            "rules",
            "strong",
            "openai/sol",
            "l1/capable.low",
        ),
        // Nothing was chosen, so the rationale names the whole input.
        (
            evidence(
                ActiveMode::Live,
                LearnedChoice::ConstraintUnmet {
                    unmet: vec![Unmet::Quality, Unmet::Latency],
                },
            ),
            "rules",
            "strong",
            "openai/sol",
            "l2/capable.low.no_high.tools",
        ),
    ];
    for (evidence, strategy, tier, target, key) in cases {
        let rationale = evidence.rationale();
        for named in [strategy, tier, target, "abcdef01", key] {
            assert!(
                rationale.contains(named),
                "the rationale must name {named}: {rationale}"
            );
        }
        assert!(!rationale.contains('$'), "{rationale}");
        for plan in &evidence.plans {
            for price in [plan.cost.quoted_usd, plan.cost.adjusted_usd] {
                for spelled in [
                    format!("{price}"),
                    format!("{price:.2}"),
                    format!("{price:.4}"),
                    format!("{price:.6}"),
                ] {
                    assert!(
                        !rationale.contains(&spelled),
                        "the rationale names the price {spelled}: {rationale}"
                    );
                }
            }
        }
    }
}

/// **The claim.** A learned record whose served strategy has no plan is not a
/// value anyone can hold: the constructor refuses it and so does the wire.
///
/// Without the check, `source()` answers `None` and the rationale drops its
/// served clause, so a bad durable record reads as a turn that served nothing
/// rather than failing where it was read.
#[test]
fn a_learned_record_whose_served_strategy_has_no_plan_is_refused() {
    let whole = evidence(
        ActiveMode::Live,
        LearnedChoice::Exploit {
            strategy: Strategy::Capable,
        },
    );
    let without = |strategy: Strategy| {
        let mut parts = whole.clone().into_parts();
        parts.plans.retain(|plan| plan.strategy != strategy);
        parts
    };
    // The wire half writes the unchecked parts, which is exactly what a bad
    // durable record looks like to the reader.
    let decode = |parts: &LearnedEvidenceParts| {
        serde_json::from_value::<LearnedEvidence>(serde_json::to_value(parts).expect("serializes"))
    };

    // The served strategy's plan is missing.
    assert_eq!(
        LearnedEvidence::new(without(Strategy::Capable)),
        Err(LearnedEvidenceError::MissingPlan {
            strategy: Strategy::Capable
        })
    );
    assert!(
        decode(&without(Strategy::Capable)).is_err(),
        "a live record that served `capable` with no `capable` plan must not read"
    );
    // The `rules` plan is missing, which shadow mode and an infeasible turn serve.
    assert_eq!(
        LearnedEvidence::new(without(Strategy::Rules)),
        Err(LearnedEvidenceError::Plans(StrategySetError::NoRules))
    );
    assert!(
        decode(&without(Strategy::Rules)).is_err(),
        "a record with no `rules` plan must not read"
    );
    // Shadow mode serves `rules`, and the rationale still reads the chosen
    // strategy's gate: a chosen strategy with no plan is refused too.
    let mut shadow = without(Strategy::Capable);
    shadow.mode = ActiveMode::Shadow;
    assert_eq!(
        LearnedEvidence::new(shadow.clone()),
        Err(LearnedEvidenceError::MissingPlan {
            strategy: Strategy::Capable
        })
    );
    assert!(decode(&shadow).is_err());

    // Control: the same record with every plan present reads, and states the
    // served plan's source.
    let read: LearnedEvidence =
        serde_json::from_value(serde_json::to_value(&whole).expect("serializes"))
            .expect("the whole record reads");
    assert_eq!(read.source(), Some(DecisionSource::Strategy));
}

/// **`rules_stage()` is the `rules` plan, whichever strategy served.** A reader
/// that wants what the stage router alone would have recorded gets the `rules`
/// pick and outcome, never the served plan's.
#[test]
fn rules_stage_reads_the_rules_plan_when_another_strategy_served() {
    let learned = evidence(
        ActiveMode::Live,
        LearnedChoice::Exploit {
            strategy: Strategy::Efficient,
        },
    );
    let rules = learned.plan(Strategy::Rules).expect("configured");
    let served = learned.served_plan();
    // Control: the two plans differ in both fields, so the assertions below
    // can tell them apart.
    assert_eq!(served.strategy, Strategy::Efficient);
    assert_ne!(served.pick, rules.pick);
    assert_ne!(served.outcome, rules.outcome);

    let stage = learned.rules_stage();
    assert_eq!(stage.pick, rules.pick);
    assert_eq!(stage.outcome, rules.outcome);
    assert_eq!(stage.recipe, learned.recipe);
    assert_eq!(stage.source(), Some(DecisionSource::Override));
}

/// **An explored record holds the exploration it names.** The choice's member
/// must index the recorded set and name the chosen strategy. Otherwise the
/// calibrator, which weights by the recorded set and propensity, would be the
/// first reader to meet a malformed record, far from the writer.
#[test]
fn an_explored_record_without_its_exploration_is_refused() {
    let explore = LearnedChoice::Explore {
        strategy: Strategy::Efficient,
        member: 0,
    };

    // The control: the fixture's set is [efficient] and member 0 names it.
    assert!(LearnedEvidence::new(parts(ActiveMode::Live, explore.clone())).is_ok());

    let mut no_exploration = parts(ActiveMode::Live, explore.clone());
    no_exploration.exploration = None;
    let mut out_of_range = parts(ActiveMode::Live, explore.clone());
    out_of_range.exploration.as_mut().unwrap().set = vec![];
    let mut wrong_member = parts(ActiveMode::Live, explore);
    wrong_member.exploration.as_mut().unwrap().set = vec![Strategy::Capable];
    // The chosen strategy is in the set, but not at the recorded index. A
    // check that asked only "is the strategy in the set" would accept this.
    let mut right_strategy_wrong_index = parts(
        ActiveMode::Live,
        LearnedChoice::Explore {
            strategy: Strategy::Capable,
            member: 0,
        },
    );
    right_strategy_wrong_index.exploration.as_mut().unwrap().set =
        vec![Strategy::Efficient, Strategy::Capable];
    for (case, broken) in [
        ("no exploration", no_exploration),
        ("member outside the set", out_of_range),
        ("member names another strategy", wrong_member),
        (
            "strategy present at another index",
            right_strategy_wrong_index,
        ),
    ] {
        assert!(
            matches!(
                LearnedEvidence::new(broken),
                Err(LearnedEvidenceError::Exploration { .. })
            ),
            "{case}: an explored record must hold the exploration it names"
        );
    }
}
