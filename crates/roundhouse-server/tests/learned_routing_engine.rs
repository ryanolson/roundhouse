// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The online routing learner through `Engine::run_turn` (milestone M8 of
//! `agent-docs/PLAN-online-routing-learner.md`).
//!
//! The policy is proven pure one crate down (`learned_policy_examples.rs`
//! runs every row of draft 7.8 against `LearnedPolicy::choose`). What only an
//! engine can prove is the join: that `plan` reads the store for the key the
//! decision records, computes the draw from the session and the response,
//! reaches the policy only for `shadow` and `live`, maps a refusal to a failed
//! turn, and that the `run_turn` tail delivers what the log owes the store,
//! exactly once, through reopen, replay, failures and a successor.
//!
//! Rows 1 to 4 of draft 7.8 put evidence into keys where `rules` picks
//! capable. Serving cannot produce that state without exploration, so the
//! tests write it into the store directly, as the draft says.

#[allow(dead_code)]
#[path = "learned_routing_engine/rig.rs"]
mod rig;

#[path = "learned_routing_engine/classified.rs"]
mod classified;
#[path = "learned_routing_engine/delivery.rs"]
mod delivery;
#[path = "learned_routing_engine/recovery.rs"]
mod recovery;
#[path = "learned_routing_engine/recovery_holds.rs"]
mod recovery_holds;
#[path = "learned_routing_engine/recovery_warnings.rs"]
mod recovery_warnings;
#[path = "learned_routing_engine/stops.rs"]
mod stops;

mod common;

use std::time::Duration;

use roundhouse_core::control::{
    Allocation, Budget, BudgetTerms, BudgetWindow, DEFAULT_WARN_AT, Exhaustion,
};
use roundhouse_core::event::{IncompleteReason, SessionEventKind};
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::Item;
use roundhouse_core::routing::learn::{
    CacheReuse, CostCorrection, Draw, ExplorationTerms, GrantCheck, LatencySum, LearnedChoice,
    LearnerMode, OnInfeasible, ReadFailure, StoreRead, Strategy, Unmet, explore,
};
use roundhouse_core::session::TargetDelta;
use roundhouse_core::validate::{ArmShares, ValidationTerms};
use roundhouse_fleet::WireProtocol;
use roundhouse_server::{Admission, EngineError};

use rig::{
    BELOW, PASS, Rig, RigConfig, admission, catalog_with, fresh_input, l2, large, learned, seed,
    seed_ops, small, terms,
};

fn live() -> roundhouse_core::routing::learn::LearnerTerms {
    terms(LearnerMode::Live)
}

/// The admission of a project whose sessions the judge reviews, so a turn
/// may explore.
fn reviewed(admission: Admission) -> Admission {
    Admission {
        validation: Some(ValidationTerms {
            shares: ArmShares::new(0, 1, 0).expect("one weight is a table"),
            action: Default::default(),
            placebo_rate: 1.0,
            handoff_note: None,
        }),
        ..admission
    }
}

/// Row 1. `efficient` and `capable` both pass at the turn's L2 key: the
/// cheaper plan serves, which is `small`, where `rules` would serve `large`.
#[tokio::test]
async fn row_1_a_proven_cheap_strategy_serves_its_own_target() {
    let rig = Rig::new(RigConfig::default());
    let key = l2(&fresh_input());
    seed(
        &rig.learner,
        "acme",
        key,
        &[
            (Strategy::Rules, PASS),
            (Strategy::Efficient, PASS),
            (Strategy::Capable, PASS),
        ],
    )
    .await;
    let session = SessionId::new("acme/ada/row-1");
    let result = rig
        .turn(&session, "t1", &admission("acme", Some(live())))
        .await
        .expect("the turn is served");

    let decision = result.decision.expect("a routed turn");
    assert_eq!(decision.target, small(), "the cheaper passing plan serves");
    let record = rig.last_learned(&session).await;
    assert_eq!(
        record.choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Efficient
        }
    );
    assert_eq!(
        record
            .input
            .key(roundhouse_core::routing::learn::KeyLevel::L2),
        key
    );
    assert!(
        decision.rationale.starts_with("learned router (live"),
        "the learned rationale is what the decision and `explain_last_route` carry: {}",
        decision.rationale
    );
}

/// A learned route fails over to the passing plans' first targets (draft
/// 7.6): `small` is down, so the turn falls to `large`, both dispatches carry
/// the one learned selection, the metrics count one decision, and the turn's
/// operational rows count one failover on `small`.
#[tokio::test]
async fn a_learned_route_fails_over_to_a_passing_plan_and_counts_one_decision() {
    let rig = Rig::new(RigConfig {
        down: vec![rig::SMALL],
        ..RigConfig::default()
    });
    let key = l2(&fresh_input());
    seed(
        &rig.learner,
        "over",
        key,
        &[
            (Strategy::Rules, PASS),
            (Strategy::Efficient, PASS),
            (Strategy::Capable, PASS),
        ],
    )
    .await;
    let session = SessionId::new("over/ada/s");
    let result = rig
        .turn(&session, "t1", &admission("over", Some(live())))
        .await
        .expect("the fallback serves");
    let decision = result.decision.expect("routed");
    assert_eq!(decision.target, small(), "the learned choice");
    assert_eq!(
        decision.fallbacks,
        vec![large()],
        "the other passing plan's first target"
    );

    let routed = rig.decisions(&session).await;
    assert_eq!(
        routed
            .iter()
            .map(|record| record.chosen.clone())
            .collect::<Vec<_>>(),
        vec![small(), large()]
    );
    assert_eq!(
        learned(&routed[0]),
        learned(&routed[1]),
        "one selection, two dispatches"
    );

    let config = roundhouse_core::metrics::MetricsConfig::new(rig::catalog().shadow_pricing());
    let learning = rig.engine.metrics().snapshot(&config, 0).learning;
    assert_eq!(
        learning.decisions, 1,
        "one decision however many dispatches"
    );
    assert_eq!(learning.choices.exploit, 1);

    use roundhouse_core::learn_store::LearnerStore;
    let request = roundhouse_core::learn_store::ReadRequest::new(
        rig::project("over"),
        rig::epoch(),
        &fresh_input(),
        &live().strategies,
        [&large(), &small()],
    );
    let view = rig
        .learner
        .inner
        .read(&request)
        .await
        .expect("a memory read");
    assert_eq!(view.target(&small()).map(|ops| ops.failover), Some(1));
    assert_eq!(
        view.target(&large()).map(|ops| ops.latency.n),
        Some(1),
        "large served"
    );
}

/// Row 3. `efficient` passes, but its modeled first output is over the limit:
/// `capable` serves if it passes, and the turn is infeasible if it does not.
#[tokio::test]
async fn row_3_a_plan_over_the_latency_limit_fails_its_constraint() {
    let rig = Rig::new(RigConfig {
        catalog: catalog_with(20_000.0, WireProtocol::OpenAiResponses),
        ..RigConfig::default()
    });
    let key = l2(&fresh_input());
    seed(
        &rig.learner,
        "fast",
        key,
        &[
            (Strategy::Rules, PASS),
            (Strategy::Efficient, PASS),
            (Strategy::Capable, PASS),
        ],
    )
    .await;
    let session = SessionId::new("fast/ada/row-3");
    rig.turn(&session, "t1", &admission("fast", Some(live())))
        .await
        .expect("served");
    let record = rig.last_learned(&session).await;
    assert_eq!(
        record.choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Rules
        },
        "rules and capable tie on cost and latency; configured order breaks it"
    );
    let efficient = record.plan(Strategy::Efficient).expect("planned");
    assert!(!efficient.latency_met, "{efficient:?}");
    assert_eq!(rig.decisions(&session).await[0].chosen, large());

    // Nothing else passes: infeasible, with latency among the unmet.
    seed(
        &rig.learner,
        "slow",
        key,
        &[
            (Strategy::Rules, BELOW),
            (Strategy::Efficient, PASS),
            (Strategy::Capable, BELOW),
        ],
    )
    .await;
    let session = SessionId::new("slow/ada/row-3");
    rig.turn(&session, "t1", &admission("slow", Some(live())))
        .await
        .expect("serve_rules serves");
    assert_eq!(
        rig.last_learned(&session).await.choice,
        LearnedChoice::ConstraintUnmet {
            unmet: vec![Unmet::Quality, Unmet::Latency]
        }
    );
}

/// A session's first turn sends two items to an explicit-marker provider, so
/// the second turn's quote for `large` is warm.
fn long_items() -> Vec<Item> {
    vec![
        Item::user_text("a stable preamble ".repeat(300)),
        Item::user_text("the first question"),
    ]
}

fn budgeted(project: &str) -> Admission {
    Admission {
        budget: Some(BudgetTerms {
            budget: Budget {
                limit_usd: 1_000.0,
                window: BudgetWindow::Total,
                on_exhaustion: Exhaustion::degrade_with_overflow(),
                warn_at: DEFAULT_WARN_AT,
            },
            allocation: Allocation::Pooled,
        }),
        ..admission(project, Some(live()))
    }
}

/// Row 4. A measured reuse shortfall raises `large`'s corrected cost above the
/// grant, which was opened at `large`'s own warm quote: every plan that serves
/// `large` fails the grant. The control is the same session shape without the
/// shortfall.
#[tokio::test]
async fn row_4_a_cache_shortfall_that_breaks_the_grant_changes_the_decision() {
    let rig = Rig::new(RigConfig {
        catalog: catalog_with(10.0, WireProtocol::AnthropicMessages),
        ..RigConfig::default()
    });
    let key = l2(&fresh_input());
    for project in ["short", "whole"] {
        seed(
            &rig.learner,
            project,
            key,
            &[
                (Strategy::Rules, PASS),
                (Strategy::Efficient, BELOW),
                (Strategy::Capable, PASS),
            ],
        )
        .await;
    }
    // Twenty measured pairs on `large`, each predicting full reuse and
    // observing none.
    seed_ops(
        &rig.learner,
        "short",
        TargetDelta {
            target: large().policy_identity(),
            latency: LatencySum::default(),
            failover: 0,
            cache: CacheReuse {
                predicted_permille: 20_000,
                observed_permille: 0,
                n: 20,
            },
        },
    )
    .await;

    for project in ["short", "whole"] {
        let session = SessionId::new(format!("{project}/ada/row-4"));
        let admission = budgeted(project);
        rig.turn_with(&session, "t1", long_items(), &admission)
            .await
            .expect("the cold first turn is served");
        rig.turn(&session, "t2", &admission)
            .await
            .expect("the warm second turn is served");
    }

    let short = rig.last_learned(&SessionId::new("short/ada/row-4")).await;
    let rules = short.plan(Strategy::Rules).expect("planned");
    assert!(
        rules.cost.adjusted_usd > rules.cost.quoted_usd,
        "the shortfall re-priced the warm quote upward: {:?}",
        rules.cost
    );
    assert_eq!(rules.grant, GrantCheck::Exceeds);
    assert_eq!(
        short.choice,
        LearnedChoice::ConstraintUnmet {
            unmet: vec![Unmet::Quality, Unmet::Grant]
        }
    );

    let whole = rig.last_learned(&SessionId::new("whole/ada/row-4")).await;
    let rules = whole.plan(Strategy::Rules).expect("planned");
    assert_eq!(
        (rules.cost.correction, rules.grant),
        (CostCorrection::TooFewSamples, GrantCheck::Admits),
        "without measured pairs the warm quote stands: {:?}",
        rules.cost
    );
    assert_eq!(rules.cost.adjusted_usd, rules.cost.quoted_usd);
    assert_eq!(
        whole.choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Rules
        }
    );
}

/// Row 5. No live evidence: every live turn is infeasible, and `serve_rules`
/// serves the `rules` decision whole.
#[tokio::test]
async fn row_5_cold_start_is_infeasible_and_serve_rules_serves_rules() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("cold/ada/row-5");
    let result = rig
        .turn(&session, "t1", &admission("cold", Some(live())))
        .await
        .expect("served");
    let decision = result.decision.expect("routed");
    assert_eq!(decision.target, large());
    let record = rig.last_learned(&session).await;
    assert_eq!(
        record.choice,
        LearnedChoice::ConstraintUnmet {
            unmet: vec![Unmet::Quality]
        }
    );

    // The same turn with no learner is the same route.
    let off = Rig::new(RigConfig::default());
    let control = off
        .turn(&session, "t1", &admission("cold", None))
        .await
        .expect("served")
        .decision
        .expect("routed");
    assert_eq!(decision.target, control.target);
    assert_eq!(decision.fallbacks, control.fallbacks);
    assert_eq!(decision.budget_state, control.budget_state);
}

/// Row 6, and the brief's read-timeout case. A read slower than
/// `read_timeout_ms` is `ReadTimedOut`: `serve_rules` serves `large`, and
/// `refuse` fails the turn as a policy refusal naming the timeout.
#[tokio::test]
async fn row_6_a_store_read_timeout_serves_rules_under_serve_rules_and_fails_under_refuse() {
    let rig = Rig::new(RigConfig::default());
    rig.learner.delay_reads(Duration::from_secs(3));
    let quick = |on_infeasible| roundhouse_core::routing::learn::LearnerTerms {
        read_timeout_ms: 50,
        on_infeasible,
        ..live()
    };

    let session = SessionId::new("timeout/ada/serve");
    let served = rig
        .turn(
            &session,
            "t1",
            &admission("timeout", Some(quick(OnInfeasible::ServeRules))),
        )
        .await
        .expect("serve_rules serves through a store timeout");
    assert_eq!(served.decision.expect("routed").target, large());
    let record = rig.last_learned(&session).await;
    assert_eq!(
        record.view,
        StoreRead::Unavailable {
            reason: ReadFailure::ReadTimedOut
        }
    );
    assert_eq!(
        record.choice,
        LearnedChoice::ConstraintUnmet {
            unmet: vec![Unmet::ReadTimedOut]
        }
    );

    let session = SessionId::new("timeout/ada/refuse");
    match rig
        .turn(
            &session,
            "t1",
            &admission("timeout", Some(quick(OnInfeasible::Refuse))),
        )
        .await
    {
        Err(EngineError::LearnerRefused { unmet }) => {
            assert_eq!(unmet, vec![Unmet::ReadTimedOut])
        }
        other => panic!(
            "expected a learner refusal, got {:?}",
            other.map(|r| r.decision)
        ),
    }
    let reason = rig
        .events(&session)
        .await
        .into_iter()
        .find_map(|event| match event.kind {
            SessionEventKind::ResponseIncomplete { reason, .. } => Some(reason),
            _ => None,
        });
    assert_eq!(reason, Some(IncompleteReason::PolicyRefused));
}

/// Row 7. The state of row 1, mode `shadow`: the route is the stage router's
/// to the byte, and the record shows `efficient` as the learned choice, not
/// applied.
#[tokio::test]
async fn a_shadow_project_serves_rules_and_records_the_learned_choice() {
    let rig = Rig::new(RigConfig::default());
    let key = l2(&fresh_input());
    seed(
        &rig.learner,
        "shade",
        key,
        &[
            (Strategy::Rules, PASS),
            (Strategy::Efficient, PASS),
            (Strategy::Capable, PASS),
        ],
    )
    .await;
    let session = SessionId::new("shade/ada/row-7");
    let shadow = rig
        .turn(
            &session,
            "t1",
            &admission("shade", Some(terms(LearnerMode::Shadow))),
        )
        .await
        .expect("served")
        .decision
        .expect("routed");
    let off = Rig::new(RigConfig::default())
        .turn(&session, "t1", &admission("shade", None))
        .await
        .expect("served")
        .decision
        .expect("routed");
    assert_eq!(shadow.target, off.target, "shadow routes exactly as off");
    assert_eq!(shadow.fallbacks, off.fallbacks);
    assert_eq!(shadow.budget_state, off.budget_state);
    assert_eq!(shadow.admitted, off.admitted);

    let record = rig.last_learned(&session).await;
    assert_eq!(
        record.choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Efficient
        }
    );
    assert_eq!(record.served_strategy(), Strategy::Rules);
    assert!(
        shadow.rationale.contains("not applied"),
        "{}",
        shadow.rationale
    );
}

/// Row 8. Without exploration, the only evidence serving produces is for the
/// strategies that routed like `rules` on this pool: the live learner serves
/// the `rules` route.
#[tokio::test]
async fn row_8_without_exploration_serving_produced_state_serves_the_rules_route() {
    let rig = Rig::new(RigConfig::default());
    let key = l2(&fresh_input());
    seed(
        &rig.learner,
        "plain",
        key,
        &[(Strategy::Rules, PASS), (Strategy::Capable, PASS)],
    )
    .await;
    let session = SessionId::new("plain/ada/row-8");
    let decision = rig
        .turn(&session, "t1", &admission("plain", Some(live())))
        .await
        .expect("served")
        .decision
        .expect("routed");
    assert_eq!(decision.target, large());
    let record = rig.last_learned(&session).await;
    assert_eq!(
        record.choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Rules
        }
    );
    assert_eq!(record.exploration, None, "the project configured none");
}

/// The exploration row. A live project that explores, on a reviewed session,
/// with `efficient` unproven and cheaper than the exploit target: the turn
/// serves `small`, and the record's draw is the one the salt, the session and
/// the recorded response give, and its propensity is the policy's own.
#[tokio::test]
async fn an_explored_turn_records_its_draw_member_and_propensity() {
    let rig = Rig::new(RigConfig::default());
    let key = l2(&fresh_input());
    seed(
        &rig.learner,
        "scout",
        key,
        &[(Strategy::Rules, PASS), (Strategy::Capable, PASS)],
    )
    .await;
    // Below 1.0 so the propensity is not the value a turn that could not
    // explore records; close enough to 1.0 that the engine's random response
    // id cannot make the draw miss it.
    const RATE: f64 = 0.999_999;
    let exploring = roundhouse_core::routing::learn::LearnerTerms {
        exploration: Some(ExplorationTerms { rate: RATE }),
        ..live()
    };
    let session = SessionId::new("scout/ada/explore");
    let result = rig
        .turn(
            &session,
            "t1",
            &reviewed(admission("scout", Some(exploring.clone()))),
        )
        .await
        .expect("served");
    let decision = result.decision.expect("routed");
    assert_eq!(
        decision.target,
        small(),
        "the cheaper unproven member serves"
    );

    let record = rig.last_learned(&session).await;
    assert_eq!(
        record.choice,
        LearnedChoice::Explore {
            strategy: Strategy::Efficient,
            member: 0
        }
    );
    let exploration = record.exploration.clone().expect("the project explores");
    assert!(exploration.possible);
    assert_eq!(exploration.set, vec![Strategy::Efficient]);
    assert_eq!(
        exploration.draw,
        Draw::for_turn(rig::SALT, &session, &result.response_id),
        "the draw is recomputable from the salt, the session and the response"
    );
    let expected = explore::propensity(
        &record.plans,
        &exploration.set,
        &small(),
        Some(&large()),
        RATE,
    );
    assert_eq!(record.propensity, expected);
    assert!((record.propensity - RATE).abs() < 1e-12);

    // Control: the same state on a session no judge reviews never explores.
    let session = SessionId::new("scout/ada/unreviewed");
    rig.turn(&session, "t1", &admission("scout", Some(exploring)))
        .await
        .expect("served");
    let record = rig.last_learned(&session).await;
    assert_eq!(
        record.choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Rules
        }
    );
    assert_eq!(record.propensity, 1.0);
}

/// An `off` project, and a project with no learner block, on an engine that
/// has a learner: no learned evidence on any record, and not one learner-store
/// call, on a session with no learned history.
#[tokio::test]
async fn an_off_project_records_no_learned_evidence() {
    let rig = Rig::new(RigConfig::default());
    for (name, terms) in [("off", Some(terms(LearnerMode::Off))), ("none", None)] {
        let session = SessionId::new(format!("{name}/ada/quiet"));
        let admission = admission(name, terms);
        for turn in ["t1", "t2"] {
            rig.turn(&session, turn, &admission).await.expect("served");
        }
        for decision in rig.decisions(&session).await {
            assert!(learned(&decision).is_none(), "{name}: {decision:?}");
        }
        assert!(rig.acknowledged(&session).await.is_empty());
    }
    assert_eq!(rig.learner.reads(), 0, "an off project reads nothing");
    assert_eq!(rig.learner.applies(), 0, "and has nothing to deliver");
    assert_eq!(rig.sessions.clears(), 0);
    assert_eq!(rig.sessions.acks(), 0);
}

/// Two projects on one engine and one store: each reads only its own keys,
/// and each session's entries land under its own project.
#[tokio::test]
async fn two_projects_learn_independently_through_the_engine() {
    let rig = Rig::new(RigConfig::default());
    let key = l2(&fresh_input());
    seed(
        &rig.learner,
        "one",
        key,
        &[
            (Strategy::Rules, PASS),
            (Strategy::Efficient, PASS),
            (Strategy::Capable, PASS),
        ],
    )
    .await;
    let first = SessionId::new("one/ada/s");
    let second = SessionId::new("two/ada/s");
    rig.turn(
        &first,
        "t1",
        &admission("one", Some(terms(LearnerMode::Shadow))),
    )
    .await
    .expect("served");
    rig.turn(
        &second,
        "t1",
        &admission("two", Some(terms(LearnerMode::Shadow))),
    )
    .await
    .expect("served");

    assert_eq!(
        rig.last_learned(&first).await.choice,
        LearnedChoice::Exploit {
            strategy: Strategy::Efficient
        }
    );
    assert_eq!(
        rig.last_learned(&second).await.choice,
        LearnedChoice::ConstraintUnmet {
            unmet: vec![Unmet::Quality]
        },
        "project two reads none of project one's evidence"
    );
    use roundhouse_core::learn_store::LearnerStore;
    let one = rig::project("one");
    let two = rig::project("two");
    assert!(rig.learner.inner.watermark(&one, &first).await.unwrap() > 0);
    assert!(rig.learner.inner.watermark(&two, &second).await.unwrap() > 0);
    assert_eq!(rig.learner.inner.watermark(&one, &second).await.unwrap(), 0);
    assert_eq!(rig.learner.inner.watermark(&two, &first).await.unwrap(), 0);
}

/// Both scopes of the metrics document, and the project scope, carry the
/// learning section; the delivery counts are in the deployment and project
/// scopes only.
#[tokio::test]
async fn metrics_expose_the_learning_section_for_each_scope() {
    use roundhouse_core::control::{PrincipalKey, ProjectId};
    use roundhouse_core::metrics::MetricsConfig;

    let rig = Rig::new(RigConfig::default());
    let shadow_session = SessionId::new("north/ada/m");
    let live_session = SessionId::new("south/ada/m");
    rig.turn(
        &shadow_session,
        "t1",
        &admission("north", Some(terms(LearnerMode::Shadow))),
    )
    .await
    .expect("served");
    rig.turn(&live_session, "t1", &admission("south", Some(live())))
        .await
        .expect("served");

    let metrics = rig.engine.metrics();
    let config = MetricsConfig::new(rig::catalog().shadow_pricing());
    let deployment = metrics.snapshot(&config, 0).learning;
    assert_eq!(deployment.decisions, 2);
    assert_eq!((deployment.modes.shadow, deployment.modes.live), (1, 1));
    assert_eq!(deployment.served.rules, 2);
    assert_eq!(deployment.choices.constraint_unmet, 2);
    assert_eq!(deployment.unmet.quality, 2);
    assert_eq!(deployment.acknowledgements, 2);
    let delivery = deployment
        .delivery
        .expect("the deployment scope has delivery");
    assert_eq!(
        delivery.applied_entries, 2,
        "one terminal entry per session"
    );

    let member = metrics
        .snapshot_for(
            &PrincipalKey::from(&roundhouse_core::control::Principal::new("north", "ada")),
            &config,
            0,
        )
        .learning;
    assert_eq!((member.decisions, member.modes.shadow), (1, 1));
    assert_eq!(
        member.delivery, None,
        "per project, and not a projection of the log"
    );

    let project = metrics
        .snapshot_for_project(&ProjectId::from("south"), &config, 0)
        .learning;
    assert_eq!((project.decisions, project.modes.live), (1, 1));
    assert_eq!(
        project.delivery.expect("the project scope").applied_entries,
        1
    );

    let document = serde_json::to_value(metrics.snapshot(&config, 0)).unwrap();
    assert_eq!(document["learning"]["decisions"], 2);
    assert_eq!(document["learning"]["delivery"]["applied_entries"], 2);
}
