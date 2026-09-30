// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `LearnedPolicy::choose` without exploration (draft sections 7.2 to 7.6).
//!
//! The served plan is the cheapest strategy that meets every hard constraint:
//! the grant on the corrected cost, the modeled first output under the limit,
//! and the gate. When none does, the turn serves the `rules` decision exactly
//! (`serve_rules`, the ruled default) or fails with the unmet constraints
//! (`refuse`). Fallbacks are only other passing strategies' first targets, so
//! no unchecked target rides along behind a checked one.

mod learned_support;

use learned_support::*;
use roundhouse_core::routing::learn::{
    GrantCheck, JevCounts, KeyLevel, LearnedChoice, LearnedError, LearnerMode, LearnerTerms,
    OnInfeasible, StoreRead, Strategy, StrategySet, Unmet,
};
use roundhouse_core::routing::{Decision, PickerMode, Target, TierRecipe};

fn none() -> JevCounts {
    JevCounts::default()
}

/// Every strategy with `live` at the L2 key of `rig`'s turn.
fn at_l2(rig: &Rig, turn: &Turn, strategies: &[(Strategy, Counts)]) -> StoreRead {
    read(
        vec![level(rig.input(turn).key(KeyLevel::L2), strategies, none())],
        Vec::new(),
    )
}

fn with_view(mut turn: Turn, view: StoreRead) -> Turn {
    turn.view = view;
    turn
}

/// The route half of a decision: what the engine dispatches, in order, and
/// under which budget state.
fn route(decision: &Decision) -> (Target, Vec<Target>) {
    (decision.target.clone(), decision.fallbacks.clone())
}

/// The cheapest passing plan serves. `local/small` quotes $0, so `efficient`
/// beats `rules` and `capable`, which both route to `frontier/large`. With
/// `efficient` below the floor, the cheapest remaining passing plan serves.
#[test]
fn the_exploit_strategy_is_the_cheapest_passing_plan() {
    let rig = Rig::new(PickerMode::CapableFirst, section_7_8_pool());
    let turn = Turn::new(terms(LearnerMode::Live), cold(), STAY);

    let all = with_view(
        Turn::new(terms(LearnerMode::Live), cold(), STAY),
        at_l2(
            &rig,
            &turn,
            &[
                (Strategy::Rules, PASS),
                (Strategy::Efficient, PASS),
                (Strategy::Capable, PASS),
            ],
        ),
    );
    let decision = rig.chosen(&all);
    assert_eq!(decision.target, small());
    assert_eq!(
        evidence(&decision).choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Efficient
        }
    );

    let dear = with_view(
        Turn::new(terms(LearnerMode::Live), cold(), STAY),
        at_l2(
            &rig,
            &turn,
            &[
                (Strategy::Rules, PASS),
                (Strategy::Efficient, BELOW),
                (Strategy::Capable, PASS),
            ],
        ),
    );
    let decision = rig.chosen(&dear);
    assert_eq!(decision.target, large());
    // `rules` and `capable` tie on cost and latency; the configured order
    // breaks it.
    assert_eq!(
        evidence(&decision).choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Rules
        }
    );
    let reordered = Turn {
        terms: LearnerTerms {
            strategies: StrategySet::new(vec![
                Strategy::Capable,
                Strategy::Efficient,
                Strategy::Rules,
            ])
            .unwrap(),
            ..terms(LearnerMode::Live)
        },
        ..with_view(
            Turn::new(terms(LearnerMode::Live), cold(), STAY),
            at_l2(
                &rig,
                &turn,
                &[
                    (Strategy::Rules, PASS),
                    (Strategy::Efficient, BELOW),
                    (Strategy::Capable, PASS),
                ],
            ),
        )
    };
    assert_eq!(
        evidence(&rig.chosen(&reordered)).choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Capable
        }
    );

    // Equal cost, different latency: two local tiers at $0, and the faster
    // one serves although it is last in the configured order.
    let big = Target::Local {
        worker_id: 2,
        dp_rank: 0,
        model: "big".into(),
    };
    let locals = Rig::with_recipe(
        TierRecipe::new(
            vec![big.policy_identity()],
            vec![small().policy_identity()],
            PickerMode::CapableFirst,
            0.5,
        )
        .unwrap(),
        vec![local(big.clone(), 900.0), local(small(), 300.0)],
    );
    let quick = with_view(
        Turn::new(terms(LearnerMode::Live), cold(), STAY),
        at_l2(
            &locals,
            &turn,
            &[
                (Strategy::Rules, PASS),
                (Strategy::Efficient, PASS),
                (Strategy::Capable, PASS),
            ],
        ),
    );
    let decision = locals.chosen(&quick);
    assert_eq!(decision.target, small());
    assert_eq!(decision.fallbacks, vec![big]);
}

/// The modeled first output, quote plus residual plus overhead, is compared
/// with the limit. A passing strategy whose target is slow fails the latency
/// constraint and does not serve, however cheap it is.
#[test]
fn a_plan_over_the_latency_limit_fails_the_latency_constraint() {
    let rig = Rig::new(PickerMode::CapableFirst, section_7_8_pool());
    let probe = Turn::new(terms(LearnerMode::Live), cold(), STAY);
    let key = rig.input(&probe).key(KeyLevel::L2);
    let levels = vec![level(
        key,
        &[
            (Strategy::Rules, PASS),
            (Strategy::Efficient, PASS),
            (Strategy::Capable, PASS),
        ],
        none(),
    )];

    let fast = Turn::new(
        terms(LearnerMode::Live),
        read(levels.clone(), Vec::new()),
        STAY,
    );
    assert_eq!(rig.chosen(&fast).target, small());

    let slowed = Turn::new(
        terms(LearnerMode::Live),
        read(levels, vec![slow(&small())]),
        STAY,
    );
    let decision = rig.chosen(&slowed);
    let record = evidence(&decision);
    let efficient = record.plan(Strategy::Efficient).unwrap();
    assert!(!efficient.latency_met);
    assert!(efficient.ttft.adjusted_ms > LATENCY_LIMIT_MS as f64);
    assert_eq!(decision.target, large());
    assert!(!decision.fallbacks.contains(&small()));
}

/// With no passing strategy the turn is infeasible. Under `serve_rules` it
/// serves exactly the `rules` decision — never the cheaper unproven plan, and
/// never a fallback the learner did not check beyond the ones `rules` itself
/// carries.
#[test]
fn no_passing_strategy_is_infeasible_and_never_serves_an_unchecked_route() {
    let rig = Rig::new(PickerMode::CapableFirst, section_7_8_pool());
    let probe = Turn::new(terms(LearnerMode::Live), cold(), STAY);
    for strategies in [
        vec![],
        vec![
            (Strategy::Rules, UNPROVEN),
            (Strategy::Efficient, UNPROVEN),
            (Strategy::Capable, UNPROVEN),
        ],
        vec![(Strategy::Rules, BELOW), (Strategy::Efficient, UNPROVEN)],
    ] {
        let turn = with_view(
            Turn::new(terms(LearnerMode::Live), cold(), STAY),
            at_l2(&rig, &probe, &strategies),
        );
        let decision = rig.chosen(&turn);
        assert!(matches!(
            evidence(&decision).choice,
            LearnedChoice::ConstraintUnmet { .. }
        ));
        let rules = rig.rules();
        assert_eq!(route(&decision), route(&rules), "{strategies:?}");
        assert_ne!(decision.target, small(), "the unproven cheap plan");
    }
}

/// The state: `efficient` passes the gate but its target is too slow, and the
/// two strategies that route to `frontier/large` are below the floor.
fn quality_and_latency(rig: &Rig, on_infeasible: OnInfeasible) -> Turn {
    let probe = Turn::new(terms(LearnerMode::Live), cold(), STAY);
    let key = rig.input(&probe).key(KeyLevel::L2);
    Turn::new(
        LearnerTerms {
            on_infeasible,
            ..terms(LearnerMode::Live)
        },
        read(
            vec![level(
                key,
                &[
                    (Strategy::Rules, BELOW),
                    (Strategy::Efficient, PASS),
                    (Strategy::Capable, BELOW),
                ],
                none(),
            )],
            vec![slow(&small())],
        ),
        STAY,
    )
}

/// `serve_rules` serves the `rules` route as the stage router would, and the
/// record names every constraint some strategy failed, in the fixed order
/// quality, latency, grant, then any read failure.
#[test]
fn serve_rules_serves_the_rules_decision_and_records_the_unmet_constraints() {
    let rig = Rig::new(PickerMode::CapableFirst, section_7_8_pool());
    let decision = rig.chosen(&quality_and_latency(&rig, OnInfeasible::ServeRules));
    let rules = rig.rules();
    assert_eq!(route(&decision), route(&rules));
    assert_eq!(decision.budget_state, rules.budget_state);
    assert_eq!(decision.source, rules.source);
    assert_eq!(decision.admitted, rules.admitted);
    let record = evidence(&decision);
    assert_eq!(
        record.choice,
        LearnedChoice::ConstraintUnmet {
            unmet: vec![Unmet::Quality, Unmet::Latency]
        }
    );
    assert_eq!(record.served_strategy(), Strategy::Rules);
    assert!(
        decision
            .rationale
            .contains("no strategy met quality, latency"),
        "{}",
        decision.rationale
    );
}

/// `refuse` fails the turn with a typed error that names the same unmet
/// constraints, and only when nothing passes.
#[test]
fn refuse_fails_the_turn_with_the_unmet_constraints() {
    let rig = Rig::new(PickerMode::CapableFirst, section_7_8_pool());
    let refused = rig.choose(&quality_and_latency(&rig, OnInfeasible::Refuse));
    match refused {
        Err(LearnedError::Refused { ref unmet }) => {
            assert_eq!(unmet, &vec![Unmet::Quality, Unmet::Latency]);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    let message = refused.unwrap_err().to_string();
    assert!(message.contains("quality, latency"), "{message}");

    // A passing strategy serves under `refuse` as under `serve_rules`.
    let mut passing = quality_and_latency(&rig, OnInfeasible::Refuse);
    passing.view = read(
        vec![level(
            rig.input(&passing).key(KeyLevel::L2),
            &[(Strategy::Capable, PASS)],
            none(),
        )],
        Vec::new(),
    );
    assert_eq!(rig.chosen(&passing).target, large());
}

/// Fallbacks are the first targets of the other passing strategies, in exploit
/// order, without repeats. A strategy's own second choices are not among them,
/// because its evidence is about its first target only, and a first target
/// that fails a constraint is not among them either.
#[test]
fn fallbacks_hold_only_targets_that_satisfy_every_constraint() {
    // efficient: [local/small, frontier/medium]; capable: [frontier/large].
    let pool = vec![
        hosted(large(), 0.0, 800.0),
        hosted(medium(), 9_000.0, 600.0),
        local(small(), 300.0),
    ];
    let rig = Rig::with_recipe(recipe(PickerMode::CapableFirst, true), pool);
    let probe = Turn::new(terms(LearnerMode::Live), cold(), STAY);
    let key = rig.input(&probe).key(KeyLevel::L2);
    let passing = [
        (Strategy::Rules, PASS),
        (Strategy::Efficient, PASS),
        (Strategy::Capable, PASS),
    ];

    let all = Turn::new(
        terms(LearnerMode::Live),
        read(vec![level(key, &passing, none())], Vec::new()),
        STAY,
    );
    let decision = rig.chosen(&all);
    assert_eq!(decision.target, small());
    // `rules` and `capable` share `frontier/large`: one entry. The efficient
    // plan's own fallback, `frontier/medium`, is not a checked target.
    assert_eq!(decision.fallbacks, vec![large()]);

    let slowed = Turn::new(
        terms(LearnerMode::Live),
        read(vec![level(key, &passing, none())], vec![slow(&large())]),
        STAY,
    );
    let decision = rig.chosen(&slowed);
    assert_eq!(decision.target, small());
    assert!(decision.fallbacks.is_empty(), "{:?}", decision.fallbacks);

    // Every fallback that does appear is a passing plan's first target.
    let record = evidence(&rig.chosen(&all)).clone();
    for fallback in &rig.chosen(&all).fallbacks {
        let plan = record
            .plans
            .iter()
            .find(|plan| &plan.first == fallback)
            .expect("a fallback is some plan's first target");
        assert!(plan.latency_met && plan.grant != GrantCheck::Exceeds);
    }
}
