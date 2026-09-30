// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Draft section 7.8, row by row, as pure-policy tests.
//!
//! The recipe is `efficient: [local/small]`, `capable: [frontier/large]`, and
//! `rules` picks capable, so its route is `frontier/large` on every row. Rows
//! 1 to 4 put evidence for `efficient` into a key where `rules` picks capable.
//! Serving cannot produce that state without exploration, so the tests write
//! it into the view directly. Two rows changed after the draft: cold start
//! now reads `serve_rules` by default (ruling 7), and an exploration row is
//! added (ruling 8).

mod learned_support;

use learned_support::*;
use roundhouse_core::classify::TurnComplexity;
use roundhouse_core::control::{BudgetState, Exhaustion, TurnBudget};
use roundhouse_core::routing::PickerMode;
use roundhouse_core::routing::learn::{
    ActiveMode, Band, GateResult, JevCounts, KeyLevel, LearnedChoice, LearnedError, LearnerMode,
    LearnerTerms, OnInfeasible, PriorBand, ReadFailure, StoreRead, Strategy, Unmet,
};

fn none() -> JevCounts {
    JevCounts::default()
}

fn rig() -> Rig {
    Rig::new(PickerMode::CapableFirst, section_7_8_pool())
}

/// The L2 key of a turn with no classification sequence.
fn l2(rig: &Rig) -> roundhouse_core::routing::learn::LevelKey {
    rig.input(&Turn::new(terms(LearnerMode::Live), cold(), STAY))
        .key(KeyLevel::L2)
}

fn both_pass(rig: &Rig) -> StoreRead {
    read(
        vec![level(
            l2(rig),
            &[(Strategy::Efficient, PASS), (Strategy::Capable, PASS)],
            none(),
        )],
        Vec::new(),
    )
}

/// Row 1, cheap strategy proven: `efficient` and `capable` both pass at L2,
/// and the cheaper one serves.
#[test]
fn row_1_a_proven_cheap_strategy_serves() {
    let rig = rig();
    assert_eq!(rig.rules().target, large());
    let decision = rig.chosen(&Turn::new(terms(LearnerMode::Live), both_pass(&rig), STAY));
    assert_eq!(decision.target, small());
    assert_eq!(
        evidence(&decision).choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Efficient
        }
    );
}

/// Row 2, the sequence changes the route. Both sequences end on a `low`
/// classification, so L1 is shared; only the older classifications differ,
/// and with them the L2 key. `efficient` is below the floor under
/// `some_high` and passes under `no_high`.
#[test]
fn a_different_prior_sequence_changes_the_route_when_l2_evidence_differs() {
    let rig = rig();
    let a = Turn::new(terms(LearnerMode::Live), cold(), STAY)
        .with_sequence(&[TurnComplexity::Deep, TurnComplexity::Routine]);
    let b = Turn::new(terms(LearnerMode::Live), cold(), STAY)
        .with_sequence(&[TurnComplexity::Trivial, TurnComplexity::Routine]);
    let (input_a, input_b) = (rig.input(&a), rig.input(&b));
    assert_eq!(
        (input_a.newest, input_a.prior),
        (Band::Low, PriorBand::SomeHigh)
    );
    assert_eq!(
        (input_b.newest, input_b.prior),
        (Band::Low, PriorBand::NoHigh)
    );
    assert_eq!(input_a.key(KeyLevel::L1), input_b.key(KeyLevel::L1));

    let view = read(
        vec![
            level(
                input_a.key(KeyLevel::L2),
                &[(Strategy::Efficient, BELOW), (Strategy::Capable, PASS)],
                none(),
            ),
            level(
                input_b.key(KeyLevel::L2),
                &[(Strategy::Efficient, PASS), (Strategy::Capable, PASS)],
                none(),
            ),
            // The shared L1 would pass `efficient` for both; L2 is read first.
            level(
                input_a.key(KeyLevel::L1),
                &[(Strategy::Efficient, PASS), (Strategy::Capable, PASS)],
                none(),
            ),
        ],
        Vec::new(),
    );
    let a = Turn {
        view: view.clone(),
        ..a
    };
    let b = Turn { view, ..b };
    let served_a = rig.chosen(&a);
    let served_b = rig.chosen(&b);
    assert_eq!(rig.rules().target, large());
    assert_eq!(served_a.target, large());
    assert_eq!(
        evidence(&served_a).choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Capable
        }
    );
    assert_eq!(served_b.target, small());
    assert_eq!(
        evidence(&served_b).choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Efficient
        }
    );
}

/// Row 3, latency limit: `efficient` passes, but the modeled first output of
/// `local/small` is over the limit. `capable` serves if it passes, and the
/// turn is infeasible if it does not.
#[test]
fn row_3_a_slow_passing_strategy_yields_to_capable_or_infeasible() {
    let rig = rig();
    let passing = Turn::new(
        terms(LearnerMode::Live),
        read(
            vec![level(
                l2(&rig),
                &[(Strategy::Efficient, PASS), (Strategy::Capable, PASS)],
                none(),
            )],
            vec![slow(&small())],
        ),
        STAY,
    );
    let decision = rig.chosen(&passing);
    assert_eq!(decision.target, large());
    assert_eq!(
        evidence(&decision).choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Capable
        }
    );

    let alone = Turn::new(
        terms(LearnerMode::Live),
        read(
            vec![level(l2(&rig), &[(Strategy::Efficient, PASS)], none())],
            vec![slow(&small())],
        ),
        STAY,
    );
    let decision = rig.chosen(&alone);
    assert_eq!(decision.target, large());
    assert_eq!(
        evidence(&decision).choice,
        LearnedChoice::ConstraintUnmet {
            unmet: vec![Unmet::Quality, Unmet::Latency]
        }
    );
}

/// Row 4, cache correction and grant: `frontier/large` is quoted warm, well
/// inside the grant, and its measured reuse is zero, so the corrected cost is
/// above the grant. `efficient` serves if it passes; otherwise the turn is
/// infeasible and says the grant was one reason.
#[test]
fn row_4_a_reuse_shortfall_over_the_grant_yields_to_efficient_or_infeasible() {
    let mut rig = Rig::new(
        PickerMode::CapableFirst,
        vec![hosted(large(), 9_000.0, 800.0), local(small(), 300.0)],
    );
    rig.budget = TurnBudget::Granted {
        ceiling_usd: 0.01,
        state: BudgetState::Warned,
        on_exhaustion: Exhaustion::DegradeToLocal {
            overflow_when_local_saturated: false,
        },
    };
    assert!(
        rig.candidates[0].expected_cost_usd < 0.01,
        "admitted on its quote"
    );

    let view = |strategies: &[(Strategy, Counts)]| {
        read(
            vec![level(l2(&rig), strategies, none())],
            vec![reuse(&large(), 0)],
        )
    };
    let passing = Turn::new(
        terms(LearnerMode::Live),
        view(&[
            (Strategy::Rules, PASS),
            (Strategy::Efficient, PASS),
            (Strategy::Capable, PASS),
        ]),
        STAY,
    );
    let decision = rig.chosen(&passing);
    assert_eq!(decision.target, small());
    assert!(decision.fallbacks.is_empty(), "large is over the grant");
    let record = evidence(&decision);
    let capable = record.plan(Strategy::Capable).unwrap();
    assert!(capable.cost.adjusted_usd > 0.01);

    let unproven = Turn::new(
        terms(LearnerMode::Live),
        view(&[(Strategy::Rules, PASS), (Strategy::Capable, PASS)]),
        STAY,
    );
    let decision = rig.chosen(&unproven);
    assert_eq!(decision.target, large());
    assert_eq!(
        evidence(&decision).choice,
        LearnedChoice::ConstraintUnmet {
            unmet: vec![Unmet::Quality, Unmet::Grant]
        }
    );
}

/// Row 5, cold start: no live evidence, so every live turn is infeasible, and
/// the default `on_infeasible` serves `rules`.
#[test]
fn row_5_cold_start_serves_rules_by_default() {
    let rig = rig();
    let live = terms(LearnerMode::Live);
    assert_eq!(live.on_infeasible, OnInfeasible::ServeRules);
    let decision = rig.chosen(&Turn::new(live, cold(), STAY));
    assert_eq!(decision.target, large());
    assert_eq!(decision.fallbacks, rig.rules().fallbacks);
    let record = evidence(&decision);
    assert_eq!(
        record.choice,
        LearnedChoice::ConstraintUnmet {
            unmet: vec![Unmet::Quality]
        }
    );
    for plan in &record.plans {
        assert_eq!(plan.gate.result, GateResult::Unproven);
        assert_eq!(plan.gate.level, None);
    }
    assert_eq!(record.propensity, 1.0);
}

/// Row 6, store timeout: no strategy has a gate result. `serve_rules` serves
/// `frontier/large`; `refuse` fails the turn. The unmet list names the read
/// failure in place of quality, because quality was never evaluated.
#[test]
fn row_6_a_store_timeout_is_infeasible_with_its_reason() {
    let rig = rig();
    for (reason, unmet) in [
        (ReadFailure::ReadTimedOut, Unmet::ReadTimedOut),
        (ReadFailure::StoreUnavailable, Unmet::StoreUnavailable),
    ] {
        let view = StoreRead::Unavailable { reason };
        let served = rig.chosen(&Turn::new(terms(LearnerMode::Live), view.clone(), go(0)));
        assert_eq!(served.target, large());
        let record = evidence(&served);
        assert_eq!(
            record.choice,
            LearnedChoice::ConstraintUnmet { unmet: vec![unmet] }
        );
        assert_eq!(record.view, view);

        let refusing = LearnerTerms {
            on_infeasible: OnInfeasible::Refuse,
            ..terms(LearnerMode::Live)
        };
        match rig.choose(&Turn::new(refusing, view, STAY)) {
            Err(LearnedError::Refused { unmet: named }) => assert_eq!(named, vec![unmet]),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}

/// Row 7, shadow: the state of row 1, mode `shadow`. The route is the stage
/// router's to the byte, and the record shows `efficient` as the learned
/// choice, not applied. Mode `off` never reaches `choose`: the engine serves
/// the stage decision without reading the store or drawing.
#[test]
fn row_7_shadow_serves_rules_and_records_the_learned_choice() {
    let rig = rig();
    let rules = rig.rules();
    let decision = rig.chosen(&Turn::new(
        terms(LearnerMode::Shadow),
        both_pass(&rig),
        STAY,
    ));
    assert_eq!(decision.target, rules.target);
    assert_eq!(decision.fallbacks, rules.fallbacks);
    assert_eq!(decision.budget_state, rules.budget_state);
    assert_eq!(decision.source, rules.source);
    assert_eq!(decision.admitted, rules.admitted);
    let record = evidence(&decision);
    assert_eq!(record.mode, ActiveMode::Shadow);
    assert_eq!(
        record.choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Efficient
        }
    );
    assert_eq!(record.served_strategy(), Strategy::Rules);
    assert!(
        decision.rationale.contains("not applied"),
        "{}",
        decision.rationale
    );
}

/// Row 8, no exploration: only the state serving can produce. `rules` served
/// every crediting turn, so only strategies routing like it hold evidence,
/// and the learned route is `frontier/large` or infeasible. The exhaustive
/// form is `without_exploration_the_learned_route_equals_rules_or_is_infeasible`.
#[test]
fn row_8_without_exploration_the_route_is_rules() {
    let rig = rig();
    for strategies in [
        vec![(Strategy::Rules, PASS), (Strategy::Capable, PASS)],
        vec![(Strategy::Rules, PASS)],
        vec![(Strategy::Capable, UNPROVEN)],
    ] {
        let view = read(vec![level(l2(&rig), &strategies, none())], Vec::new());
        let decision = rig.chosen(&Turn::new(terms(LearnerMode::Live), view, STAY));
        assert_eq!(decision.target, large(), "{strategies:?}");
    }
}

/// The added row, exploration: cold start on a reviewed session with a draw
/// below the rate. The set is the one cheaper unproven strategy,
/// `efficient`, then `rules` (the 2026-09-30 ruling); member 0 serves, and
/// the record carries the draw, the set and the propensity, `rate / 2`.
#[test]
fn row_9_a_reviewed_cold_start_can_explore_the_cheaper_strategy() {
    let rig = rig();
    let decision = rig.chosen(&Turn::new(exploring(LearnerMode::Live), cold(), go(4)));
    assert_eq!(decision.target, small());
    let record = evidence(&decision);
    assert_eq!(
        record.choice,
        LearnedChoice::Explore {
            strategy: Strategy::Efficient,
            member: 0
        }
    );
    let exploration = record.exploration.as_ref().unwrap();
    assert!(exploration.possible);
    assert_eq!(exploration.set, vec![Strategy::Efficient, Strategy::Rules]);
    assert_eq!(exploration.draw, go(4));
    assert!((record.propensity - RATE / 2.0).abs() < 1e-12);
}
