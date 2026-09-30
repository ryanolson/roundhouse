// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the three builtin policies write down about their own choice.
//!
//! Every assertion here reads a [`Decision`] a real `choose` returned. The
//! claims are about the two evidence fields it now carries:
//!
//! - `admitted` — the pool the policy's *own* `admissible` call produced, which
//!   is not the candidate set and cannot be recovered from it. `max_load` is
//!   per-policy tuning and the overflow valve re-admits a set no second call
//!   would reproduce, so a caller asking again asks a different question.
//! - `selector` — which branch ran, with the configuration it ran under. An
//!   escalation audit is not its delegated affinity turn, and a stage router
//!   without a recipe is honestly the inner policy rather than an unconsumed
//!   recipe.

use roundhouse_core::control::{
    BudgetState, Exhaustion, FrontierHistory, TargetFilter, TurnBudget, TurnPolicy,
};
use roundhouse_core::ids::SessionId;
use roundhouse_core::routing::policy::Weights;
use roundhouse_core::routing::stage::DEFAULT_CONFIDENCE_THRESHOLD;
use roundhouse_core::routing::{
    AffinityPolicy, CacheLedger, Candidate, Decision, DecisionSource, EscalationPolicy, PickerMode,
    RoutingContext, RoutingPolicy, SelectorBranch, StageEvidence, StageOutcome, StagePolicy,
    Target, Tier, TierRecipe, TurnSignals,
};
use roundhouse_core::validate::ToolSignals;

// ---------------------------------------------------------------------------
// The fleet and the context
// ---------------------------------------------------------------------------

fn hosted(model: &str, quality: f64, cost: f64) -> Candidate {
    Candidate {
        target: Target::Frontier {
            provider: "openai".into(),
            model: model.into(),
        },
        expected_prefill_tokens: 1_000.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: 500.0,
        expected_cost_usd: cost,
        quality_prior: quality,
        load: None,
    }
}

fn local(worker_id: u64, load: f64) -> Candidate {
    Candidate {
        target: Target::Local {
            worker_id,
            dp_rank: 0,
            model: "llama".into(),
        },
        expected_prefill_tokens: 400.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: 50.0,
        expected_cost_usd: 0.0,
        quality_prior: 0.60,
        load: Some(load),
    }
}

fn hosted_target(model: &str) -> Target {
    Target::Frontier {
        provider: "openai".into(),
        model: model.into(),
    }
}

/// The three-hosted fleet the stage assertions route over: `sol` is capable and
/// dear, `luna` and `terra` are the efficient tier.
fn fleet() -> Vec<Candidate> {
    vec![
        hosted("sol", 0.95, 1.00),
        hosted("luna", 0.70, 0.10),
        hosted("terra", 0.80, 0.30),
    ]
}

/// capable = [sol], efficient = [luna, terra].
fn recipe(picker: PickerMode) -> TierRecipe {
    TierRecipe::new(
        vec!["openai/sol".into()],
        vec!["openai/luna".into(), "openai/terra".into()],
        picker,
        DEFAULT_CONFIDENCE_THRESHOLD,
    )
    .expect("a two-tier recipe at the shipped threshold")
}

struct Fixture {
    session_id: SessionId,
    ledger: CacheLedger,
    turn_policy: TurnPolicy,
    frontier_history: FrontierHistory,
    budget: TurnBudget,
    recipe: Option<TierRecipe>,
    signals: Option<TurnSignals>,
    turn_index: u64,
}

impl Fixture {
    fn open() -> Self {
        Self {
            session_id: SessionId::new("s"),
            ledger: CacheLedger::new(),
            turn_policy: TurnPolicy::unrestricted(),
            frontier_history: FrontierHistory::default(),
            budget: TurnBudget::Unlimited,
            recipe: None,
            signals: None,
            turn_index: 3,
        }
    }

    fn with_recipe(mut self, recipe: TierRecipe) -> Self {
        self.recipe = Some(recipe);
        self
    }

    fn with_signals(mut self, signals: TurnSignals) -> Self {
        self.signals = Some(signals);
        self
    }

    fn under(mut self, turn_policy: TurnPolicy) -> Self {
        self.turn_policy = turn_policy;
        self
    }

    fn with_budget(mut self, budget: TurnBudget) -> Self {
        self.budget = budget;
        self
    }

    fn on_turn(mut self, turn_index: u64) -> Self {
        self.turn_index = turn_index;
        self
    }

    fn ctx<'a>(&'a self, candidates: &'a [Candidate]) -> RoutingContext<'a> {
        RoutingContext {
            session_id: &self.session_id,
            turn_index: self.turn_index,
            isl_tokens: 10_000,
            candidates,
            ledger: &self.ledger,
            turn_policy: &self.turn_policy,
            frontier_history: &self.frontier_history,
            budget: &self.budget,
            signals: self.signals.as_ref(),
            tiers: self.recipe.as_ref(),
        }
    }
}

async fn choose(
    policy: &dyn RoutingPolicy,
    fixture: &Fixture,
    candidates: &[Candidate],
) -> Decision {
    policy
        .choose(&fixture.ctx(candidates))
        .await
        .expect("this pool always holds something")
}

/// The admitted pool as identities, in the order the resolution produced.
fn admitted_of(decision: &Decision) -> Vec<String> {
    decision
        .admitted
        .as_ref()
        .expect("a builtin policy's decision knows what it was offered")
        .iter()
        .map(Target::policy_identity)
        .collect()
}

fn stage_evidence(decision: &Decision) -> StageEvidence {
    match &decision
        .selector
        .as_ref()
        .expect("a staged decision records its branch")
        .branch
    {
        SelectorBranch::Stage(evidence) => evidence.clone(),
        other => panic!("expected the stage branch, got {other:?}"),
    }
}

/// Signals that put the scorer over the threshold toward the capable tier: a
/// critical result is the hard escalate, which no arithmetic can soften.
fn escalating_signals() -> TurnSignals {
    TurnSignals {
        tools: ToolSignals {
            severity: roundhouse_core::validate::CRITICAL,
            ..ToolSignals::default()
        },
        turn_depth: 9,
    }
}

// ---------------------------------------------------------------------------
// Admitted is not considered
// ---------------------------------------------------------------------------

/// **The claim.** An affinity policy's own `max_load` empties part of the
/// candidate set, and the decision records the pool that survived rather than
/// the set it started from.
#[tokio::test]
async fn max_load_makes_the_admitted_pool_smaller_than_the_candidate_set() {
    let candidates = vec![
        local(1, 120_000.0),
        local(2, 500.0),
        hosted("sol", 0.95, 1.0),
    ];
    let fixture = Fixture::open();

    let decision = choose(
        &AffinityPolicy::new().with_max_load(50_000.0),
        &fixture,
        &candidates,
    )
    .await;
    // **Whole targets, not policy identities.** Both workers spell
    // `local/llama` -- `Target::policy_identity` drops `worker_id` deliberately,
    // because a policy names a capability -- so a name check here would pass on
    // a pool that admitted the *overloaded* worker and excluded the idle one,
    // which is the exact inversion the ceiling exists to prevent.
    assert_eq!(
        decision.admitted,
        Some(vec![
            candidates[1].target.clone(),
            candidates[2].target.clone(),
        ]),
        "worker 2 and the hosted model, in the order the resolution produced; \
         worker 1 is over the ceiling and the candidate set still has all three"
    );

    // CONTROL: the identical set under a policy with no ceiling admits all
    // three, worker 1 included. Same candidates, same context, one knob.
    let unlimited = choose(&AffinityPolicy::new(), &fixture, &candidates).await;
    assert_eq!(
        unlimited.admitted,
        Some(
            candidates
                .iter()
                .map(|candidate| candidate.target.clone())
                .collect::<Vec<_>>()
        )
    );
}

/// **The claim.** A spent budget excludes every priced candidate, and the
/// decision records the local pool that was left.
#[tokio::test]
async fn a_spent_budget_records_the_local_pool_it_degraded_onto() {
    let candidates = vec![local(1, 500.0), hosted("sol", 0.95, 1.0)];
    let fixture = Fixture::open().with_budget(TurnBudget::exhausted(Exhaustion::DegradeToLocal {
        overflow_when_local_saturated: false,
    }));

    let decision = choose(&AffinityPolicy::new(), &fixture, &candidates).await;
    assert_eq!(admitted_of(&decision), vec!["local/llama".to_string()]);
    assert_eq!(decision.budget_state, BudgetState::Exhausted);
}

/// **The claim.** The overflow valve's pool is the post-valve one — the set the
/// valve re-admitted, marked as such — and not the empty set the budget filter
/// left behind.
#[tokio::test]
async fn the_overflow_valve_records_the_pool_it_reopened() {
    // No local candidate at all, so the budget filter empties the set and the
    // valve is the only thing that can produce a pool.
    let candidates = vec![hosted("sol", 0.95, 1.00), hosted("luna", 0.70, 0.10)];
    let fixture =
        Fixture::open().with_budget(TurnBudget::exhausted(Exhaustion::degrade_with_overflow()));

    let decision = choose(&AffinityPolicy::new(), &fixture, &candidates).await;
    assert_eq!(
        decision.budget_state,
        BudgetState::ExhaustedOverflow,
        "the valve is what produced this pool"
    );
    assert_eq!(
        admitted_of(&decision),
        vec!["openai/sol".to_string(), "openai/luna".to_string()],
        "both hosted candidates came back past the budget, which is a set no \
         post-hoc budget filter would reproduce"
    );
}

/// **The claim.** The policy filter runs before the pool is recorded, so an
/// unreachable target is absent from the evidence as well as from the choice.
#[tokio::test]
async fn a_policy_excluded_target_is_absent_from_the_admitted_pool() {
    let candidates = fleet();
    let fixture = Fixture::open().under(TurnPolicy {
        allow: TargetFilter::parse(["openai/luna"]).expect("a literal pattern"),
        ..TurnPolicy::unrestricted()
    });

    let decision = choose(&AffinityPolicy::new(), &fixture, &candidates).await;
    assert_eq!(admitted_of(&decision), vec!["openai/luna".to_string()]);
}

// ---------------------------------------------------------------------------
// Which branch ran
// ---------------------------------------------------------------------------

/// **The claim.** An affinity decision records the weights and ceiling it
/// scored under, and two differently tuned instances record differently.
#[tokio::test]
async fn affinity_records_its_own_weights_and_ceiling() {
    let candidates = fleet();
    let fixture = Fixture::open();

    let tuned = choose(
        &AffinityPolicy::new()
            .with_weights(Weights {
                prefill: 0.25,
                cost: 2.0,
                ttft: 0.75,
            })
            .with_max_load(7_000.0),
        &fixture,
        &candidates,
    )
    .await;
    let snapshot = tuned.selector.as_ref().expect("a builtin names its branch");
    assert_eq!(snapshot.algorithm_revision, 1);
    match &snapshot.branch {
        SelectorBranch::Affinity(evidence) => {
            assert_eq!(evidence.prefill_weight, 0.25);
            assert_eq!(evidence.cost_weight, 2.0);
            assert_eq!(evidence.ttft_weight, 0.75);
            assert_eq!(evidence.max_load, Some(7_000.0));
        }
        other => panic!("expected the affinity branch, got {other:?}"),
    }

    // CONTROL: the shipped tuning records the shipped numbers, so the
    // assertions above are about what this instance was built with.
    let default = choose(&AffinityPolicy::new(), &fixture, &candidates).await;
    match &default.selector.as_ref().unwrap().branch {
        SelectorBranch::Affinity(evidence) => {
            assert_eq!(evidence.prefill_weight, 1.0);
            assert_eq!(evidence.cost_weight, 0.5);
            assert_eq!(evidence.ttft_weight, 0.25);
            assert_eq!(evidence.max_load, None);
        }
        other => panic!("expected the affinity branch, got {other:?}"),
    }
}

/// **The claim.** An audit turn and its delegated neighbour record different
/// branches, because different code chose them.
#[tokio::test]
async fn an_audit_turn_and_a_delegated_turn_record_different_branches() {
    let candidates = fleet();
    let policy = EscalationPolicy::new(AffinityPolicy::new().with_max_load(9_000.0), 4);

    let audit = choose(&policy, &Fixture::open().on_turn(4), &candidates).await;
    assert_eq!(
        audit.selector.as_ref().unwrap().branch,
        SelectorBranch::EscalationAudit { audit_every: 4 },
        "the audit branch is the only choice this policy makes itself"
    );
    assert_eq!(
        audit.target,
        hosted_target("sol"),
        "and it escalated, which is what makes the branch above the one that ran"
    );

    // CONTROL: turn 3 delegates, so the evidence is the *inner* policy's --
    // including the inner's ceiling, which the audit branch deliberately does
    // not apply.
    let ordinary = choose(&policy, &Fixture::open().on_turn(3), &candidates).await;
    match &ordinary.selector.as_ref().unwrap().branch {
        SelectorBranch::Affinity(evidence) => assert_eq!(evidence.max_load, Some(9_000.0)),
        other => panic!("a delegated turn reports the policy that chose, got {other:?}"),
    }
}

/// **The claim.** A stage router with no recipe is the inner policy, and says
/// so: an unread recipe is never labelled as one a tier decision consumed.
#[tokio::test]
async fn a_stage_router_without_a_recipe_reports_the_inner_policys_evidence() {
    let candidates = fleet();
    let policy = StagePolicy::new(Box::new(AffinityPolicy::new().with_max_load(3_000.0)));

    let decision = choose(&policy, &Fixture::open(), &candidates).await;
    match &decision.selector.as_ref().unwrap().branch {
        SelectorBranch::Affinity(evidence) => assert_eq!(evidence.max_load, Some(3_000.0)),
        other => panic!("no recipe means no tier decision, got {other:?}"),
    }
    assert_eq!(
        decision.source, None,
        "and no tier was decided, so there is no source to state"
    );
}

/// **The claim.** The stage router passes no ceiling to `admissible`, exactly
/// as it does today, so an inner policy's `max_load` does not narrow a staged
/// turn's pool — and the recorded pool is the one that really resolved.
#[tokio::test]
async fn a_staged_turn_ignores_the_inner_policys_max_load() {
    let mut candidates = fleet();
    candidates.push(local(1, 120_000.0));
    let policy = StagePolicy::new(Box::new(AffinityPolicy::new().with_max_load(50_000.0)));
    let fixture = Fixture::open().with_recipe(recipe(PickerMode::EfficientFirst));

    let decision = choose(&policy, &fixture, &candidates).await;
    assert!(
        admitted_of(&decision).contains(&"local/llama".to_string()),
        "the overloaded worker is admitted under a recipe: {:?}",
        admitted_of(&decision)
    );

    // CONTROL: the same policy object with no recipe delegates to the inner
    // affinity policy, whose ceiling *does* bind -- so the difference above is
    // the stage router's `None` and not a broken ceiling.
    let delegated = choose(&policy, &Fixture::open(), &candidates).await;
    assert!(
        !admitted_of(&delegated).contains(&"local/llama".to_string()),
        "{:?}",
        admitted_of(&delegated)
    );
}

// ---------------------------------------------------------------------------
// The stage branches
// ---------------------------------------------------------------------------

/// **The claim.** A staged turn records the recipe as configured — both tiers,
/// in the operator's order, with the picker and the threshold — and the
/// scorer's own answer beside it.
#[tokio::test]
async fn a_staged_turn_records_the_recipe_it_ran_under() {
    let candidates = fleet();
    let policy = StagePolicy::new(Box::new(AffinityPolicy::new()));
    let ordered = TierRecipe::new(
        vec!["openai/sol".into()],
        vec!["openai/terra".into(), "openai/luna".into()],
        PickerMode::EfficientFirst,
        0.75,
    )
    .expect("a recipe whose efficient tier is deliberately terra-first");
    let fixture = Fixture::open().with_recipe(ordered);

    let decision = choose(&policy, &fixture, &candidates).await;
    let evidence = stage_evidence(&decision);
    assert_eq!(evidence.capable, vec!["openai/sol".to_string()]);
    assert_eq!(
        evidence.efficient,
        vec!["openai/terra".to_string(), "openai/luna".to_string()],
        "the operator's order, not the quoter's -- `luna` is cheaper and still \
         second"
    );
    assert_eq!(evidence.picker, PickerMode::EfficientFirst);
    assert_eq!(evidence.confidence_threshold, 0.75);
    assert_eq!(
        decision.target,
        hosted_target("terra"),
        "and the head of that order took the turn, which is what makes the \
         recorded order the one that ran"
    );
    assert_eq!(evidence.pick.tier, Tier::Efficient);
    assert_eq!(evidence.pick.source, DecisionSource::Ambiguous);
    assert_eq!(
        evidence.outcome,
        StageOutcome::Served {
            tier: Tier::Efficient
        }
    );
}

/// **The claim.** The cost guard changes the source and the serving tier, and
/// the evidence keeps both halves: what the scorer picked, and what the quote
/// did to it.
#[tokio::test]
async fn the_cost_guard_keeps_the_original_pick_beside_the_final_source() {
    // `sol` is capable and, on this turn, cheaper than the efficient head --
    // the inversion cache affinity produces and the guard exists for.
    let candidates = vec![
        hosted("sol", 0.95, 0.05),
        hosted("luna", 0.70, 0.10),
        hosted("terra", 0.80, 0.30),
    ];
    let policy = StagePolicy::new(Box::new(AffinityPolicy::new()));
    let fixture = Fixture::open().with_recipe(recipe(PickerMode::EfficientFirst));

    let decision = choose(&policy, &fixture, &candidates).await;
    assert_eq!(decision.target, hosted_target("sol"));
    assert_eq!(decision.source, Some(DecisionSource::CostGuard));

    let evidence = stage_evidence(&decision);
    assert_eq!(
        evidence.pick.tier,
        Tier::Efficient,
        "the scorer picked the cheap tier, and a record that lost that would \
         report an escalation nothing measured"
    );
    assert_eq!(
        evidence.pick.source,
        DecisionSource::Ambiguous,
        "by falling open, which is not the `CostGuard` the decision carries"
    );
    assert_eq!(
        evidence.outcome,
        StageOutcome::CostGuard {
            displaced: "openai/luna".to_string(),
        },
        "and the head it dominated is named"
    );

    // CONTROL: the identical recipe over the shipped prices, where `sol` is
    // dear, serves the efficient head and records no guard.
    let unguarded = choose(&policy, &fixture, &fleet()).await;
    assert_eq!(unguarded.target, hosted_target("luna"));
    assert_eq!(
        stage_evidence(&unguarded).outcome,
        StageOutcome::Served {
            tier: Tier::Efficient
        }
    );
}

/// **The claim.** A picked tier that admits nothing is recorded as such, with
/// the tier that actually served.
#[tokio::test]
async fn an_empty_picked_tier_is_recorded_with_the_tier_that_served() {
    // The capable tier's only member is unreachable for this principal, and the
    // signals pick capable, so the efficient tier takes the turn.
    let candidates = fleet();
    let policy = StagePolicy::new(Box::new(AffinityPolicy::new()));
    let fixture = Fixture::open()
        .with_recipe(recipe(PickerMode::EfficientFirst))
        .with_signals(escalating_signals())
        .under(TurnPolicy {
            allow: TargetFilter::parse(["openai/luna", "openai/terra"])
                .expect("two literal patterns"),
            ..TurnPolicy::unrestricted()
        });

    let decision = choose(&policy, &fixture, &candidates).await;
    let evidence = stage_evidence(&decision);
    assert_eq!(
        evidence.pick.tier,
        Tier::Capable,
        "a critical result is the hard escalate"
    );
    assert_eq!(evidence.pick.source, DecisionSource::Override);
    assert_eq!(
        evidence.outcome,
        StageOutcome::PickedTierEmpty {
            served: Tier::Efficient
        }
    );
    assert_eq!(
        decision.source,
        Some(DecisionSource::Override),
        "the source still names what picked the tier, which is the gate the \
         handoff note reads"
    );
}

/// **The claim.** A turn that leaves the recipe altogether to keep the
/// degrade-to-local promise records that branch, and does not claim a tier
/// served.
#[tokio::test]
async fn degrading_past_the_recipe_is_recorded_as_its_own_branch() {
    // The recipe names hosted targets only; a spent budget leaves exactly the
    // local worker admitted.
    let mut candidates = fleet();
    candidates.push(local(1, 500.0));
    let policy = StagePolicy::new(Box::new(AffinityPolicy::new()));
    let fixture = Fixture::open()
        .with_recipe(recipe(PickerMode::EfficientFirst))
        .with_budget(TurnBudget::exhausted(Exhaustion::DegradeToLocal {
            overflow_when_local_saturated: false,
        }));

    let decision = choose(&policy, &fixture, &candidates).await;
    assert!(decision.target.is_local());
    assert_eq!(
        decision.source, None,
        "no tier served, so stamping a source would tell the handoff gate a \
         scorer's pick had been honoured when it was bypassed"
    );

    let evidence = stage_evidence(&decision);
    assert_eq!(
        evidence.outcome,
        StageOutcome::DegradedPastRecipe {
            degraded_to: "local/llama".to_string(),
        }
    );
    assert_eq!(
        evidence.capable,
        vec!["openai/sol".to_string()],
        "the recipe that was bypassed is still the recipe this turn ran under"
    );
    assert_eq!(admitted_of(&decision), vec!["local/llama".to_string()]);
}

/// **The claim.** `StageEvidence::source` is the one rule for what
/// `Decision.source` carries, so the two cannot disagree -- on every one of
/// the four `StageOutcome` arms, not only the ones the other tests in this
/// file happen to check both halves of.
///
/// Each scenario below reruns a fixture from one of the tests above,
/// specifically to keep this claim from being provable only against a shape
/// nothing else in the suite reaches.
#[tokio::test]
async fn the_evidences_own_source_never_disagrees_with_the_decisions() {
    let policy = StagePolicy::new(Box::new(AffinityPolicy::new()));

    // `Served`: the picked tier's head takes the turn plainly.
    let fixture = Fixture::open().with_recipe(recipe(PickerMode::EfficientFirst));
    let decision = choose(&policy, &fixture, &fleet()).await;
    assert_eq!(
        stage_evidence(&decision).outcome,
        StageOutcome::Served {
            tier: Tier::Efficient
        }
    );
    assert_eq!(decision.source, stage_evidence(&decision).source());

    // `PickedTierEmpty`: the picked tier admits nothing this key allows.
    let fixture = Fixture::open()
        .with_recipe(recipe(PickerMode::EfficientFirst))
        .with_signals(escalating_signals())
        .under(TurnPolicy {
            allow: TargetFilter::parse(["openai/luna", "openai/terra"])
                .expect("two literal patterns"),
            ..TurnPolicy::unrestricted()
        });
    let decision = choose(&policy, &fixture, &fleet()).await;
    assert!(matches!(
        stage_evidence(&decision).outcome,
        StageOutcome::PickedTierEmpty { .. }
    ));
    assert_eq!(decision.source, stage_evidence(&decision).source());

    // `CostGuard`: a cheaper capable candidate dominates the efficient head.
    let guarded = vec![
        hosted("sol", 0.95, 0.05),
        hosted("luna", 0.70, 0.10),
        hosted("terra", 0.80, 0.30),
    ];
    let fixture = Fixture::open().with_recipe(recipe(PickerMode::EfficientFirst));
    let decision = choose(&policy, &fixture, &guarded).await;
    assert!(matches!(
        stage_evidence(&decision).outcome,
        StageOutcome::CostGuard { .. }
    ));
    assert_eq!(decision.source, stage_evidence(&decision).source());

    // `DegradedPastRecipe`: no tier this recipe names is admissible, and a
    // spent budget leaves exactly the local worker. This is the one arm
    // whose evidence is never handed to `decide_staged` at all -- `choose`
    // routes it through `Admitted::decide` instead, which hard-codes
    // `source: None` independently of `StageEvidence::source`. Asserting the
    // two still agree here is what proves that independence has not drifted,
    // not that the derivation ran.
    let mut candidates = fleet();
    candidates.push(local(1, 500.0));
    let fixture = Fixture::open()
        .with_recipe(recipe(PickerMode::EfficientFirst))
        .with_budget(TurnBudget::exhausted(Exhaustion::DegradeToLocal {
            overflow_when_local_saturated: false,
        }));
    let decision = choose(&policy, &fixture, &candidates).await;
    assert!(matches!(
        stage_evidence(&decision).outcome,
        StageOutcome::DegradedPastRecipe { .. }
    ));
    assert_eq!(decision.source, stage_evidence(&decision).source());
    assert_eq!(
        decision.source, None,
        "the one arm whose source is genuinely always absent"
    );
}
