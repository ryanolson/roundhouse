// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Without exploration, a live learner cannot change a route (draft 7.5).
//!
//! A deterministic exhaustive enumeration, since the workspace carries no
//! property-testing crate. Consistent-trajectory credit (ruling 9) gives a
//! strategy live units only on turns where its plan's first target was the
//! served one, and without exploration the served one is the `rules` target.
//! So the enumeration gives live units only to strategies whose first target
//! equals the `rules` first target **on the same pool**, then checks that the
//! learned route is the `rules` target or the turn is infeasible.
//!
//! **The claim is exact only per pool.** Credit earned on one pool can pass a
//! strategy that routes differently on another pool of the same key, and
//! there the latency or grant constraint can separate it from `rules`: with
//! `rules` over the latency limit on this turn, a strategy credited on turns
//! where it agreed with `rules` serves instead. That is the constraints
//! working, not exploration, but it is a route change the draft's sentence
//! does not allow for. The plan's M4 status records it.

mod learned_support;

use learned_support::*;
use roundhouse_core::routing::learn::{
    JevCounts, KeyLevel, LearnedChoice, LearnedError, LearnerMode, LearnerTerms, OnInfeasible,
    PriorUnits, Strategy, TargetOps, Units,
};
use roundhouse_core::routing::{Candidate, PickerMode, Target};

/// What one consistent strategy holds: nothing, or one count at one level.
fn placements() -> Vec<Option<(KeyLevel, Counts)>> {
    let mut all = vec![None];
    for (level, counts) in [
        (KeyLevel::L2, UNPROVEN),
        (KeyLevel::L2, PASS),
        (KeyLevel::L1, PASS),
        (KeyLevel::L0, PASS),
        (KeyLevel::L2, BELOW),
        (KeyLevel::L0, BELOW),
    ] {
        all.push(Some((level, counts)));
    }
    all
}

fn unnamed() -> Target {
    Target::Frontier {
        provider: "frontier".into(),
        model: "orion".into(),
    }
}

/// Pools over `capable: [large]`, `efficient: [small, medium]`: every branch
/// of the stage resolution — served, cost-guarded, picked tier empty, and
/// degraded past the recipe.
fn pools() -> Vec<(&'static str, Vec<Candidate>)> {
    vec![
        (
            "large and small",
            vec![hosted(large(), 0.0, 800.0), local(small(), 300.0)],
        ),
        (
            "large and a cheaper medium",
            vec![
                hosted(large(), 0.0, 800.0),
                hosted(medium(), 9_000.0, 600.0),
            ],
        ),
        (
            "a warm large under a cold medium (cost guard)",
            vec![
                hosted(large(), 9_000.0, 800.0),
                hosted(medium(), 0.0, 600.0),
            ],
        ),
        ("large only", vec![hosted(large(), 0.0, 800.0)]),
        ("small only", vec![local(small(), 300.0)]),
        (
            "nothing named, and a local worker",
            vec![
                hosted(unnamed(), 0.0, 800.0),
                local(
                    Target::Local {
                        worker_id: 9,
                        dp_rank: 0,
                        model: "tiny".into(),
                    },
                    200.0,
                ),
            ],
        ),
    ]
}

fn operations() -> Vec<Vec<TargetOps>> {
    vec![
        Vec::new(),
        vec![slow(&small()), slow(&medium())],
        vec![slow(&large())],
        vec![reuse(&large(), 0)],
    ]
}

#[derive(Default)]
struct Coverage {
    exploit_rules: usize,
    exploit_other: usize,
    infeasible: usize,
    refused: usize,
}

#[test]
fn without_exploration_the_learned_route_equals_rules_or_is_infeasible() {
    let jev_agree = JevCounts {
        capable: 50,
        efficient: 50,
    };
    let mut coverage = Coverage::default();
    let mut cases = 0usize;
    for picker in [PickerMode::EfficientFirst, PickerMode::CapableFirst] {
        for (name, pool) in pools() {
            let rig = Rig::with_recipe(recipe(picker, true), pool);
            let rules = rig.rules();
            let probe = Turn::new(terms(LearnerMode::Live), cold(), STAY);
            let input = rig.input(&probe);
            // Which strategies can hold live units: those that route like
            // `rules` on this pool. Taken from a cold turn's own record.
            let cold_decision = rig.chosen(&probe);
            let consistent: Vec<Strategy> = evidence(&cold_decision)
                .plans
                .iter()
                .filter(|plan| plan.first == rules.target)
                .map(|plan| plan.strategy)
                .collect();
            let artifact = PriorUnits::new(input.keys().into_iter().flat_map(|key| {
                [Strategy::Rules, Strategy::Efficient, Strategy::Capable].map(|strategy| {
                    (
                        (key, strategy),
                        Units {
                            pos: 90_000,
                            n: 90_000,
                        },
                    )
                })
            }));
            for targets in operations() {
                for (prior, jev) in [
                    (PriorUnits::default(), JevCounts::default()),
                    (PriorUnits::default(), jev_agree),
                    (artifact.clone(), JevCounts::default()),
                ] {
                    for on_infeasible in [OnInfeasible::ServeRules, OnInfeasible::Refuse] {
                        let learner = LearnerTerms {
                            prior: prior.clone(),
                            on_infeasible,
                            ..terms(LearnerMode::Live)
                        };
                        for_each_state(&consistent, &mut |placed| {
                            cases += 1;
                            let levels = KeyLevel::ALL
                                .into_iter()
                                .map(|at| {
                                    let strategies: Vec<(Strategy, Counts)> = placed
                                        .iter()
                                        .filter(|(_, level, _)| *level == at)
                                        .map(|&(strategy, _, counts)| (strategy, counts))
                                        .collect();
                                    level(input.key(at), &strategies, jev)
                                })
                                .collect();
                            let turn =
                                Turn::new(learner.clone(), read(levels, targets.clone()), go(0));
                            match rig.choose(&turn) {
                                Ok(decision) => {
                                    let record = evidence(&decision);
                                    match &record.choice {
                                        LearnedChoice::Exploit { strategy } => {
                                            assert_eq!(
                                                decision.target, rules.target,
                                                "{name}, {picker:?}: {strategy} changed the route \
                                                 with {placed:?}"
                                            );
                                            match strategy {
                                                Strategy::Rules => coverage.exploit_rules += 1,
                                                _ => coverage.exploit_other += 1,
                                            }
                                        }
                                        LearnedChoice::ConstraintUnmet { .. } => {
                                            assert_eq!(on_infeasible, OnInfeasible::ServeRules);
                                            assert_eq!(decision.target, rules.target);
                                            assert_eq!(decision.fallbacks, rules.fallbacks);
                                            coverage.infeasible += 1;
                                        }
                                        LearnedChoice::Explore { .. } => {
                                            panic!("{name}: a project without exploration explored")
                                        }
                                    }
                                }
                                Err(LearnedError::Refused { .. }) => {
                                    assert_eq!(on_infeasible, OnInfeasible::Refuse);
                                    coverage.refused += 1;
                                }
                                Err(other) => panic!("{name}, {picker:?}: {other}"),
                            }
                        });
                    }
                }
            }
        }
    }
    // The enumeration must reach every outcome, or passing it proves less
    // than it claims: in particular a fixed-tier strategy must win some turns.
    assert!(cases > 10_000, "{cases} cases");
    assert!(coverage.exploit_rules > 0);
    assert!(
        coverage.exploit_other > 0,
        "no fixed-tier strategy ever won"
    );
    assert!(coverage.infeasible > 0);
    assert!(coverage.refused > 0);
}

/// One strategy's live counts at one level.
type Placed = (Strategy, KeyLevel, Counts);

/// Every assignment of a placement to each consistent strategy.
fn for_each_state(consistent: &[Strategy], visit: &mut dyn FnMut(&[Placed])) {
    fn walk(rest: &[Strategy], placed: &mut Vec<Placed>, visit: &mut dyn FnMut(&[Placed])) {
        let Some((&strategy, rest)) = rest.split_first() else {
            visit(placed);
            return;
        };
        for placement in placements() {
            match placement {
                None => walk(rest, placed, visit),
                Some((level, counts)) => {
                    placed.push((strategy, level, counts));
                    walk(rest, placed, visit);
                    placed.pop();
                }
            }
        }
    }
    walk(consistent, &mut Vec::new(), visit);
}

/// The negative control: give the same live units to a strategy that does
/// *not* route like `rules` on this pool — the state only exploration can
/// produce — and the route changes. Without this, the enumeration above could
/// pass on a policy that never changes any route at all.
#[test]
fn credit_for_an_inconsistent_strategy_changes_the_route() {
    let rig = Rig::new(PickerMode::CapableFirst, section_7_8_pool());
    let probe = Turn::new(terms(LearnerMode::Live), cold(), STAY);
    let key = rig.input(&probe).key(KeyLevel::L2);
    let turn = Turn::new(
        terms(LearnerMode::Live),
        read(
            vec![level(
                key,
                &[(Strategy::Efficient, PASS)],
                JevCounts::default(),
            )],
            Vec::new(),
        ),
        STAY,
    );
    assert_ne!(rig.chosen(&turn).target, rig.rules().target);
}

/// **The per-pool limit, as a fact.** `capable` earns its evidence on a pool
/// with only `frontier/large`, where `rules` (picking efficient, finding that
/// tier empty) also serves `frontier/large`: consistent credit. On a later
/// turn of the same key, `local/small` is admitted and too slow. `rules`
/// fails the latency limit there, `capable` passes everything, and the route
/// changes with no exploration anywhere.
#[test]
fn credit_earned_on_another_pool_can_change_the_route_through_a_constraint() {
    let earned = Rig::new(
        PickerMode::EfficientFirst,
        vec![hosted(large(), 0.0, 800.0)],
    );
    let probe = Turn::new(terms(LearnerMode::Live), cold(), STAY);
    let crediting = earned.chosen(&probe);
    let record = evidence(&crediting);
    assert_eq!(
        record.plan(Strategy::Capable).unwrap().first,
        record.plan(Strategy::Rules).unwrap().first,
        "capable routes like rules on the crediting pool"
    );

    let later = Rig::new(PickerMode::EfficientFirst, section_7_8_pool());
    let key = later.input(&probe).key(KeyLevel::L2);
    let turn = Turn::new(
        terms(LearnerMode::Live),
        read(
            vec![level(
                key,
                &[(Strategy::Rules, PASS), (Strategy::Capable, PASS)],
                JevCounts::default(),
            )],
            vec![slow(&small())],
        ),
        STAY,
    );
    assert_eq!(later.rules().target, small());
    assert_eq!(later.chosen(&turn).target, large());
}
