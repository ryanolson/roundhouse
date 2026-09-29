// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learner's serving strategies are tier picks routed by the stage router's
//! own code.
//!
//! Two claims. `rules` through `StagePolicy::route_pick` is the stage decision,
//! byte for byte, so `shadow` mode changes no route. And every strategy keeps
//! what the stage router promises about a pick: the dominance cost guard on an
//! efficient pick, the fall to the other tier when the picked one admits
//! nothing, and degrade-to-local when the recipe names nothing admitted. A
//! strategy that routed by its own rules would earn quality evidence for plans
//! the stage router never serves.

use roundhouse_core::control::{FrontierHistory, TurnBudget, TurnPolicy};
use roundhouse_core::ids::SessionId;
use roundhouse_core::routing::learn::{
    ActiveMode, LearnerMode, Strategy, StrategySet, StrategySetError,
};
use roundhouse_core::routing::stage::{DEFAULT_CONFIDENCE_THRESHOLD, pick_tier};
use roundhouse_core::routing::{
    AffinityPolicy, CacheLedger, Candidate, Decision, DecisionSource, PickerMode, RoutingContext,
    RoutingPolicy, SelectorBranch, StageOutcome, StagePolicy, Target, Tier, TierRecipe,
    TurnSignals,
};
use roundhouse_core::validate::ToolSignals;

fn hosted(model: &str, cost: f64) -> Candidate {
    Candidate {
        target: Target::Frontier {
            provider: "openai".into(),
            model: model.into(),
        },
        expected_prefill_tokens: 1_000.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: 500.0,
        expected_cost_usd: cost,
        quality_prior: 0.9,
        load: None,
    }
}

fn local() -> Candidate {
    Candidate {
        target: Target::Local {
            worker_id: 3,
            dp_rank: 0,
            model: "llama".into(),
        },
        expected_prefill_tokens: 400.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: 50.0,
        expected_cost_usd: 0.0,
        quality_prior: 0.6,
        load: Some(0.0),
    }
}

/// capable = [sol, vega], efficient = [luna, terra].
fn recipe(picker: PickerMode) -> TierRecipe {
    TierRecipe::new(
        vec!["openai/sol".into(), "openai/vega".into()],
        vec!["openai/luna".into(), "openai/terra".into()],
        picker,
        DEFAULT_CONFIDENCE_THRESHOLD,
    )
    .expect("a two-tier recipe")
}

/// The ordinary fleet: capable dear, efficient cheap.
fn ordinary() -> Vec<Candidate> {
    vec![
        hosted("sol", 1.00),
        hosted("vega", 0.90),
        hosted("luna", 0.10),
        hosted("terra", 0.30),
    ]
}

/// The efficient head quotes above `vega`, so an efficient pick is guarded.
fn inverted() -> Vec<Candidate> {
    vec![
        hosted("sol", 1.00),
        hosted("vega", 0.20),
        hosted("luna", 0.50),
        hosted("terra", 0.30),
    ]
}

/// Only the capable tier is admitted.
fn capable_only() -> Vec<Candidate> {
    vec![hosted("sol", 1.00), hosted("vega", 0.90)]
}

/// Nothing the recipe names, and a local worker.
fn unnamed_and_local() -> Vec<Candidate> {
    vec![hosted("orion", 0.40), local()]
}

fn escalating() -> TurnSignals {
    TurnSignals {
        tools: ToolSignals {
            severity: roundhouse_core::validate::CRITICAL,
            ..ToolSignals::default()
        },
        turn_depth: 3,
    }
}

struct Fixture {
    session_id: SessionId,
    ledger: CacheLedger,
    turn_policy: TurnPolicy,
    frontier_history: FrontierHistory,
    budget: TurnBudget,
    recipe: TierRecipe,
    signals: TurnSignals,
}

impl Fixture {
    fn new(picker: PickerMode, signals: TurnSignals) -> Self {
        Self {
            session_id: SessionId::new("s"),
            ledger: CacheLedger::new(),
            turn_policy: TurnPolicy::unrestricted(),
            frontier_history: FrontierHistory::default(),
            budget: TurnBudget::Unlimited,
            recipe: recipe(picker),
            signals,
        }
    }

    fn ctx<'a>(&'a self, candidates: &'a [Candidate]) -> RoutingContext<'a> {
        RoutingContext {
            session_id: &self.session_id,
            turn_index: 2,
            isl_tokens: 10_000,
            candidates,
            ledger: &self.ledger,
            turn_policy: &self.turn_policy,
            frontier_history: &self.frontier_history,
            budget: &self.budget,
            signals: Some(&self.signals),
            tiers: Some(&self.recipe),
        }
    }

    fn rules_pick(&self) -> roundhouse_core::routing::Pick {
        pick_tier(
            &self.signals,
            self.recipe.picker(),
            self.recipe.confidence_threshold(),
        )
    }

    fn plan(&self, strategy: Strategy, candidates: &[Candidate]) -> Decision {
        let ctx = self.ctx(candidates);
        let admitted = ctx.admissible(None).expect("the pool holds something");
        let routed = strategy
            .plan(&self.recipe, self.rules_pick(), &admitted)
            .expect("every fixture pool can be planned");
        // What `route_pick` returns beside the decision is what the decision
        // records, on every branch these fixtures reach.
        match &routed
            .decision
            .selector
            .as_ref()
            .map(|selector| &selector.branch)
        {
            Some(SelectorBranch::Stage(evidence)) => {
                assert_eq!(routed.pick, evidence.pick, "{strategy}");
                assert_eq!(routed.outcome, evidence.outcome, "{strategy}");
            }
            other => panic!("a plan is a stage decision, got {other:?}"),
        }
        routed.decision
    }
}

fn outcome(decision: &Decision) -> StageOutcome {
    match &decision
        .selector
        .as_ref()
        .expect("a plan records its branch")
        .branch
    {
        SelectorBranch::Stage(evidence) => evidence.outcome.clone(),
        other => panic!("a plan is a stage decision, got {other:?}"),
    }
}

fn identity(decision: &Decision) -> String {
    decision.target.policy_identity()
}

/// **A control that passes before and after the extraction.** `rules` routed
/// through `route_pick` is `StagePolicy::choose`, decision for decision, over
/// every branch of the resolution: served, cost-guarded, picked tier empty, and
/// degraded past the recipe — each under both pickers and an escalation.
#[tokio::test]
async fn route_pick_with_the_rules_pick_equals_stage_policy_choose() {
    let stage = StagePolicy::new(Box::new(AffinityPolicy::new()));
    let fleets: [(&str, Vec<Candidate>); 4] = [
        ("ordinary", ordinary()),
        ("inverted", inverted()),
        ("capable only", capable_only()),
        ("unnamed and local", unnamed_and_local()),
    ];
    let cases = [
        (PickerMode::EfficientFirst, TurnSignals::default()),
        (PickerMode::CapableFirst, TurnSignals::default()),
        (PickerMode::EfficientFirst, escalating()),
    ];
    let mut outcomes = Vec::new();
    for (picker, signals) in cases {
        let fixture = Fixture::new(picker, signals);
        for (name, fleet) in &fleets {
            let chosen = stage
                .choose(&fixture.ctx(fleet))
                .await
                .expect("every fixture pool routes");
            let planned = fixture.plan(Strategy::Rules, fleet);
            assert_eq!(planned, chosen, "{picker:?} over the {name} fleet");
            outcomes.push(outcome(&chosen));
        }
    }
    // The cases above must actually reach every branch, or equality over them
    // proves less than it claims.
    for expected in [
        "served",
        "cost_guard",
        "picked_tier_empty",
        "degraded_past_recipe",
    ] {
        assert!(
            outcomes
                .iter()
                .any(|outcome| serde_json::to_value(outcome).unwrap()["kind"] == expected),
            "no case reached the {expected} branch: {outcomes:?}"
        );
    }
}

/// **The claim.** Every strategy's plan is the stage router's plan for that
/// strategy's pick: the guard, the empty-tier fall and degrade-to-local are not
/// the `rules` strategy's privilege.
#[test]
fn every_strategy_keeps_the_dominance_cost_guard_and_degrade_to_local() {
    // Efficient-first with no signals: `rules` picks efficient by fall-open.
    let fixture = Fixture::new(PickerMode::EfficientFirst, TurnSignals::default());

    // The dominance guard fires on every efficient pick and on no capable one:
    // an escalation says the cheap tier cannot finish the turn, and no price
    // makes it able to.
    for strategy in [Strategy::Rules, Strategy::Efficient] {
        let plan = fixture.plan(strategy, &inverted());
        assert_eq!(
            outcome(&plan),
            StageOutcome::CostGuard {
                displaced: "openai/luna".into()
            },
            "{strategy}"
        );
        assert_eq!(identity(&plan), "openai/vega", "{strategy}");
        assert_eq!(plan.source, Some(DecisionSource::CostGuard), "{strategy}");
        // Ruling 4's order: cheaper capable, then the efficient tier, then
        // the dearer capable members.
        let fallbacks: Vec<String> = plan.fallbacks.iter().map(Target::policy_identity).collect();
        assert_eq!(
            fallbacks,
            ["openai/luna", "openai/terra", "openai/sol"],
            "{strategy}"
        );
    }
    let capable = fixture.plan(Strategy::Capable, &inverted());
    assert_eq!(
        outcome(&capable),
        StageOutcome::Served {
            tier: Tier::Capable
        }
    );
    assert_eq!(identity(&capable), "openai/sol");
    assert_eq!(capable.source, Some(DecisionSource::Strategy));

    // A picked tier that admits nothing falls to the other tier, and a forced
    // pick keeps its own source there too rather than reading as a fall-open.
    let efficient = fixture.plan(Strategy::Efficient, &capable_only());
    assert_eq!(
        outcome(&efficient),
        StageOutcome::PickedTierEmpty {
            served: Tier::Capable
        }
    );
    assert_eq!(identity(&efficient), "openai/sol");
    assert_eq!(efficient.source, Some(DecisionSource::Strategy));

    // Degrade-to-local outranks the recipe for every strategy.
    for strategy in [Strategy::Rules, Strategy::Efficient, Strategy::Capable] {
        let plan = fixture.plan(strategy, &unnamed_and_local());
        assert_eq!(
            outcome(&plan),
            StageOutcome::DegradedPastRecipe {
                degraded_to: "local/llama".into()
            },
            "{strategy}"
        );
        assert!(plan.target.is_local(), "{strategy}");
        assert_eq!(plan.source, None, "{strategy}");
    }

    // Control: on the ordinary fleet the strategies really do differ, so the
    // agreement above is the routing's and not a fixture that serves one
    // target whatever the pick.
    assert_eq!(
        identity(&fixture.plan(Strategy::Efficient, &ordinary())),
        "openai/luna"
    );
    assert_eq!(
        identity(&fixture.plan(Strategy::Capable, &ordinary())),
        "openai/sol"
    );
}

/// The configured list is two or three distinct strategies and holds `rules`,
/// which is what `shadow` mode serves and an infeasible turn falls back to.
#[test]
fn a_strategy_list_without_rules_or_with_a_repeat_is_refused() {
    use Strategy::{Capable, Efficient, Rules};
    assert_eq!(
        StrategySet::new(vec![Efficient, Capable]),
        Err(StrategySetError::NoRules)
    );
    assert_eq!(
        StrategySet::new(vec![Rules, Efficient, Rules]),
        Err(StrategySetError::Repeated { strategy: Rules })
    );
    assert_eq!(
        StrategySet::new(vec![Rules]),
        Err(StrategySetError::Count { count: 1 })
    );
    let set = StrategySet::new(vec![Capable, Rules]).expect("two strategies with rules");
    assert_eq!(
        set.as_slice(),
        [Capable, Rules],
        "the configured order is kept"
    );
    assert!(
        serde_json::from_str::<StrategySet>(r#"["efficient","capable"]"#).is_err(),
        "the wire refuses what the constructor refuses"
    );
}

/// **The count is checked before the repeats**, so a list longer than three
/// is refused for its length. Four distinct strategies do not exist, so a
/// four-entry list always repeats one; checked the other way round, the count
/// bound would be unreachable and nothing would hold it at three.
#[test]
fn a_four_entry_strategy_list_is_refused_for_its_count() {
    use Strategy::{Capable, Efficient, Rules};
    assert_eq!(
        StrategySet::new(vec![Rules, Efficient, Capable, Rules]),
        Err(StrategySetError::Count { count: 4 })
    );
    // Control: the same list without its repeat is accepted.
    assert!(StrategySet::new(vec![Rules, Efficient, Capable]).is_ok());
}

/// **`off` writes no learned evidence**, so it has no active mode, and each
/// other mode records itself rather than its neighbour.
#[test]
fn only_shadow_and_live_have_an_active_mode() {
    assert_eq!(LearnerMode::Off.active(), None);
    assert_eq!(LearnerMode::Shadow.active(), Some(ActiveMode::Shadow));
    assert_eq!(LearnerMode::Live.active(), Some(ActiveMode::Live));
}
