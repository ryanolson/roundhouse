// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bounded exploration (ruling 8, plan sections 3 and 4).
//!
//! A turn explores only in `live` mode, only on a session whose validation
//! arm consults the judge, only when the admitted pool holds a frontier
//! target, and only when the draw is below the rate. It then serves a member
//! of the exploration set, chosen uniformly: an `Unproven` strategy whose
//! first target meets every hard constraint and costs strictly less than the
//! reference. The record carries the probability that the turn served its
//! first target, summed over every way the policy could have served it, which
//! is what the offline calibrator weights by.

mod learned_support;

use learned_support::*;
use roundhouse_core::control::{BudgetState, Exhaustion, TurnBudget};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::routing::learn::{
    Draw, JevCounts, KeyLevel, LEARNER_DRAW_VERSION, LearnedChoice, LearnerMode, LearnerTerms,
    QualityTerms, Strategy,
};
use roundhouse_core::routing::{PickerMode, TierRecipe};
use roundhouse_core::validate::Arm;
use sha2::{Digest, Sha256};

fn none() -> JevCounts {
    JevCounts::default()
}

fn explored(decision: &roundhouse_core::routing::Decision) -> bool {
    matches!(evidence(decision).choice, LearnedChoice::Explore { .. })
}

/// `capable` passes and is the exploit; `rules` picked efficient, so it and
/// `efficient` both route to `local/small`, and both are unproven. The set is
/// then two strategies with one shared first target: the largest set three
/// strategies can make, since the exploit is never in it.
fn two_member_rig() -> (Rig, Turn) {
    let rig = Rig::new(PickerMode::EfficientFirst, section_7_8_pool());
    let probe = Turn::new(exploring(LearnerMode::Live), cold(), STAY);
    let key = rig.input(&probe).key(KeyLevel::L2);
    let view = read(
        vec![level(
            key,
            &[
                (Strategy::Rules, UNPROVEN),
                (Strategy::Efficient, UNPROVEN),
                (Strategy::Capable, PASS),
            ],
            none(),
        )],
        Vec::new(),
    );
    (rig, Turn { view, ..probe })
}

/// Fixed member draws cycle through the set evenly, the index is the draw
/// modulo the set size, and the rate draw must be strictly below the rate.
#[test]
fn exploration_is_uniform_over_the_eligible_set() {
    let (rig, base) = two_member_rig();
    let mut served = [0usize; 2];
    for member in 0..6u64 {
        let turn = Turn {
            draw: go(member),
            ..Turn::new(base.terms.clone(), base.view.clone(), STAY)
        };
        let decision = rig.chosen(&turn);
        let record = evidence(&decision);
        assert_eq!(
            record.exploration.as_ref().unwrap().set,
            vec![Strategy::Rules, Strategy::Efficient]
        );
        match record.choice {
            LearnedChoice::Explore {
                strategy,
                member: index,
            } => {
                assert_eq!(index, member % 2);
                served[index as usize] += 1;
                let expected = [Strategy::Rules, Strategy::Efficient][index as usize];
                assert_eq!(strategy, expected);
            }
            ref other => panic!("draw {member} should explore, got {other:?}"),
        }
        assert_eq!(decision.target, small());
    }
    assert_eq!(served, [3, 3]);

    for (rate, explores) in [
        (0.0, true),
        (RATE - 1e-9, true),
        (RATE, false),
        (0.5, false),
    ] {
        let turn = Turn {
            draw: Draw { rate, member: 1 },
            ..Turn::new(base.terms.clone(), base.view.clone(), STAY)
        };
        assert_eq!(explored(&rig.chosen(&turn)), explores, "rate draw {rate}");
    }
}

/// A session whose arm does not consult the judge produces no review, so an
/// exploring turn there would buy a quality bypass with nothing to learn from.
#[test]
fn an_unreviewed_session_never_explores() {
    let rig = Rig::new(PickerMode::CapableFirst, section_7_8_pool());
    for arm in [None, Some(Arm::Placebo)] {
        let turn = Turn {
            arm,
            ..Turn::new(exploring(LearnerMode::Live), cold(), go(0))
        };
        let decision = rig.chosen(&turn);
        assert!(!explored(&decision), "{arm:?}");
        assert_eq!(decision.target, large());
        let record = evidence(&decision);
        assert!(!record.exploration.as_ref().unwrap().possible);
        assert_eq!(record.propensity, 1.0);
    }
    for arm in [Arm::Live, Arm::Shadow] {
        let turn = Turn {
            arm: Some(arm),
            ..Turn::new(exploring(LearnerMode::Live), cold(), go(0))
        };
        assert!(explored(&rig.chosen(&turn)), "{arm:?} consults the judge");
    }
}

/// `shadow` serves `rules`; an exploration block there never takes effect.
#[test]
fn a_shadow_project_never_explores() {
    let rig = Rig::new(PickerMode::CapableFirst, section_7_8_pool());
    let decision = rig.chosen(&Turn::new(exploring(LearnerMode::Shadow), cold(), go(0)));
    assert!(!explored(&decision));
    assert_eq!(decision.target, rig.rules().target);
    let record = evidence(&decision);
    assert!(!record.exploration.as_ref().unwrap().possible);
    assert_eq!(record.propensity, 1.0);
}

/// Local-only sessions never reach Jev (J4), so a pool with no frontier
/// target gets no Jev prior and no exploration. The state below passes
/// `capable` only through Jev's answers: 15 live intervals sit just under the
/// floor and three agreeing answers lift them over it. `local/big` carries a
/// capacity price, so `efficient` on `local/small` is a cheaper unproven plan
/// that a zero draw would explore if exploration were possible. With a
/// frontier target in the pool, the prior decides the route; without one,
/// neither the prior nor the draw changes anything, and `rules` serves.
#[test]
fn a_local_only_pool_serves_rules_and_never_explores() {
    let learner = LearnerTerms {
        quality: QualityTerms {
            min_evidence: 15_000,
            ..quality()
        },
        ..exploring(LearnerMode::Live)
    };
    let agree = JevCounts {
        capable: 3,
        efficient: 0,
    };
    let big = roundhouse_core::routing::Target::Local {
        worker_id: 2,
        dp_rank: 0,
        model: "big".into(),
    };
    let priced_big = roundhouse_core::routing::Candidate {
        expected_cost_usd: 0.01,
        ..local(big.clone(), 900.0)
    };
    let local_recipe = || {
        TierRecipe::new(
            vec![big.policy_identity()],
            vec![small().policy_identity()],
            PickerMode::CapableFirst,
            0.5,
        )
        .unwrap()
    };
    let state = |rig: &Rig| {
        let probe = Turn::new(learner.clone(), cold(), STAY);
        read(
            vec![level(
                rig.input(&probe).key(KeyLevel::L2),
                &[(Strategy::Capable, (15_000, 15_000, 20))],
                agree,
            )],
            Vec::new(),
        )
    };

    let local_only = Rig::with_recipe(
        local_recipe(),
        vec![priced_big.clone(), local(small(), 300.0)],
    );
    let decision = local_only.chosen(&Turn::new(learner.clone(), state(&local_only), go(0)));
    assert_eq!(decision.target, local_only.rules().target);
    assert_eq!(decision.target, big);
    let record = evidence(&decision);
    assert!(!explored(&decision));
    assert!(!record.exploration.as_ref().unwrap().possible);
    assert_eq!(record.propensity, 1.0);
    assert!(matches!(
        record.choice,
        LearnedChoice::ConstraintUnmet { .. }
    ));

    // The same recipe and state with a frontier target admitted beside them:
    // the prior is read and `capable` passes on it.
    let mixed = Rig::with_recipe(
        local_recipe(),
        vec![
            priced_big,
            local(small(), 300.0),
            hosted(large(), 0.0, 800.0),
        ],
    );
    let decision = mixed.chosen(&Turn::new(learner.clone(), state(&mixed), STAY));
    assert_eq!(
        evidence(&decision).choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Capable
        }
    );
    // And a zero draw there explores the cheaper unproven plan.
    let decision = mixed.chosen(&Turn::new(learner.clone(), state(&mixed), go(0)));
    assert!(explored(&decision));
    assert_eq!(decision.target, small());
}

/// Each exploration guard on the member itself: it must be unproven, meet the
/// latency limit, meet the grant on its corrected cost, and cost strictly less
/// than the reference. The explored route is only ever such a member.
#[test]
fn an_exploring_turn_serves_only_a_cheaper_unproven_member_that_meets_every_hard_constraint() {
    // The control: cold start, `local/small` is cheaper and fast.
    let rig = Rig::new(PickerMode::CapableFirst, section_7_8_pool());
    let decision = rig.chosen(&Turn::new(exploring(LearnerMode::Live), cold(), go(0)));
    assert_eq!(decision.target, small());
    let record = evidence(&decision);
    let member = record.plan(Strategy::Efficient).unwrap();
    assert!(member.latency_met);
    assert!(member.cost.adjusted_usd < record.plan(Strategy::Rules).unwrap().cost.adjusted_usd);

    // Too slow: not a member.
    let slowed = Turn::new(
        exploring(LearnerMode::Live),
        read(Vec::new(), vec![slow(&small())]),
        go(0),
    );
    let decision = rig.chosen(&slowed);
    assert!(!explored(&decision));
    assert!(
        evidence(&decision)
            .exploration
            .as_ref()
            .unwrap()
            .set
            .is_empty()
    );
    assert_eq!(decision.target, large());

    // Not strictly cheaper: an efficient frontier target quoting what the
    // reference quotes.
    let level_recipe = TierRecipe::new(
        vec![large().policy_identity()],
        vec![medium().policy_identity()],
        PickerMode::CapableFirst,
        0.5,
    )
    .unwrap();
    let even = Rig::with_recipe(
        level_recipe.clone(),
        vec![hosted(large(), 0.0, 800.0), hosted(medium(), 0.0, 600.0)],
    );
    let decision = even.chosen(&Turn::new(exploring(LearnerMode::Live), cold(), go(0)));
    assert!(!explored(&decision));
    assert_eq!(decision.target, large());

    // Cheaper, but over the grant once corrected. Both targets are quoted warm
    // inside a $0.015 grant; measured reuse lifts `frontier/large` to $0.020
    // and `frontier/medium` to $0.0183, so the member is cheaper than the
    // reference and still fails the grant.
    let mut granted = Rig::with_recipe(
        level_recipe,
        vec![
            hosted(large(), 9_000.0, 800.0),
            hosted(medium(), 9_000.0, 600.0),
        ],
    );
    granted.budget = TurnBudget::Granted {
        ceiling_usd: 0.015,
        state: BudgetState::Warned,
        on_exhaustion: Exhaustion::DegradeToLocal {
            overflow_when_local_saturated: false,
        },
    };
    let turn = Turn::new(
        exploring(LearnerMode::Live),
        read(Vec::new(), vec![reuse(&large(), 0), reuse(&medium(), 100)]),
        go(0),
    );
    let decision = granted.chosen(&turn);
    let record = evidence(&decision);
    let member = record.plan(Strategy::Efficient).unwrap();
    let reference = record.plan(Strategy::Rules).unwrap();
    assert!(member.cost.adjusted_usd < reference.cost.adjusted_usd);
    assert!(member.cost.adjusted_usd > 0.015);
    assert!(
        !explored(&decision),
        "a member over the grant is not explored"
    );
    assert_eq!(decision.target, large());
}

/// A strategy the reviews already put below the floor is not explored,
/// however cheap its target.
#[test]
fn a_below_floor_strategy_is_never_explored() {
    let rig = Rig::new(PickerMode::CapableFirst, section_7_8_pool());
    let probe = Turn::new(exploring(LearnerMode::Live), cold(), go(0));
    let view = read(
        vec![level(
            rig.input(&probe).key(KeyLevel::L2),
            &[(Strategy::Efficient, BELOW)],
            none(),
        )],
        Vec::new(),
    );
    for member in 0..4 {
        let turn = Turn {
            view: view.clone(),
            ..Turn::new(exploring(LearnerMode::Live), cold(), go(member))
        };
        let decision = rig.chosen(&turn);
        assert!(!explored(&decision));
        assert!(
            evidence(&decision)
                .exploration
                .as_ref()
                .unwrap()
                .set
                .is_empty()
        );
        assert_eq!(decision.target, large());
    }
}

/// The propensity of the served first target is the sum of the selection
/// probabilities of every way the policy could serve it. With two members
/// sharing `local/small`, an exploring turn served it with probability
/// `rate * 2/2`; a turn that did not explore served the exploit target with
/// `1 - rate`. With no possible exploration, it is 1.
#[test]
fn the_recorded_propensity_sums_selection_probabilities_by_first_target() {
    let (rig, base) = two_member_rig();
    let close = |left: f64, right: f64| (left - right).abs() < 1e-12;

    for member in [0, 1] {
        let turn = Turn::new(base.terms.clone(), base.view.clone(), go(member));
        let decision = rig.chosen(&turn);
        assert_eq!(decision.target, small());
        let propensity = evidence(&decision).propensity;
        assert!(close(propensity, RATE), "member {member}: {propensity}");
    }

    let stay = rig.chosen(&Turn::new(base.terms.clone(), base.view.clone(), STAY));
    assert_eq!(stay.target, large());
    assert!(close(evidence(&stay).propensity, 1.0 - RATE));

    let unexplorable = rig.chosen(&Turn::new(
        terms(LearnerMode::Live),
        base.view.clone(),
        go(0),
    ));
    assert_eq!(unexplorable.target, large());
    assert_eq!(evidence(&unexplorable).propensity, 1.0);
}

/// The draw is SHA-256 over the versioned `routing-explore` string with the
/// control plane's arm salt, the session and the response: the first 8 bytes
/// give the rate draw, the next 8 the member draw. A different salt or a
/// different response moves it, and it is not the arm-assignment stream.
#[test]
fn the_draw_uses_the_routing_explore_domain_and_the_arm_salt() {
    let session = SessionId::new("acme/ada/main");
    let response = ResponseId::new("resp_42");
    let canonical = format!(
        "{LEARNER_DRAW_VERSION}\nrouting-explore\nsalt=salt-1\nsession=acme/ada/main\nresponse=resp_42\n"
    );
    let digest = Sha256::digest(canonical.as_bytes());
    let rate_bits = u64::from_be_bytes(digest[..8].try_into().unwrap());
    let member = u64::from_be_bytes(digest[8..16].try_into().unwrap());
    // The top 53 bits as a fraction, which is exactly representable and so
    // strictly below 1.
    let rate = (rate_bits >> 11) as f64 / (1u64 << 53) as f64;

    let draw = Draw::for_turn("salt-1", &session, &response);
    assert_eq!(draw, Draw { rate, member });
    assert!((0.0..1.0).contains(&draw.rate));

    assert_ne!(Draw::for_turn("salt-2", &session, &response), draw);
    assert_ne!(
        Draw::for_turn("salt-1", &session, &ResponseId::new("resp_43")),
        draw
    );
    // The arm assignment hashes `arm` under the same salt and session; the
    // explore stream must not reuse its bytes.
    let arm_digest = Sha256::digest(b"v1\narm\nsalt=salt-1\nsession=acme/ada/main\n");
    assert_ne!(
        u64::from_be_bytes(arm_digest[..8].try_into().unwrap()),
        rate_bits
    );
}

/// **Golden values computed outside this crate** (Python `hashlib.sha256` over
/// `"v1\nrouting-explore\nsalt=salt-1\nsession=acme/ada/main\nresponse=resp_42\n"`,
/// 2026-09-28), so a change to the version tag, the domain string, the field
/// encoding or the bit extraction fails here. The test above rebuilds its
/// expected string from [`LEARNER_DRAW_VERSION`], so it cannot see the tag
/// move. A recorded draw must stay reproducible by the encoding that wrote it:
/// moving any of these is a new `LEARNER_DRAW_VERSION` and a new golden.
#[test]
fn the_draw_matches_a_golden_digest() {
    let draw = Draw::for_turn(
        "salt-1",
        &SessionId::new("acme/ada/main"),
        &ResponseId::new("resp_42"),
    );
    // Digest d80ca25f3dfef2a4 ea0dc90526389cc1 ...: the top 53 bits of the
    // first word, and the whole second word.
    assert_eq!(
        (draw.rate * (1u64 << 53) as f64) as u64,
        7_601_560_811_454_430
    );
    assert_eq!(draw.member, 16_865_357_203_525_639_361);
    assert_eq!(draw.member % 3, 2);
    assert_eq!(LEARNER_DRAW_VERSION, "v1");
}
