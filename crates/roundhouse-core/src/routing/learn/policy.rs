// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `LearnedPolicy::choose`: the learned decision for one turn (draft sections
//! 7.1 to 7.7, as the 2026-09-28 rulings amend them).
//!
//! **Quality first, then cost, within a latency limit.** Every configured
//! strategy is planned over one admitted pool. A plan's first target must meet
//! the grant on its corrected cost, the latency limit on its modeled first
//! output, and the quality gate. The cheapest plan that meets all three serves;
//! ties go to the lower modeled latency, then to the configured order.
//!
//! **Pure.** The policy never reads the store and never draws: the engine
//! passes the [`StoreRead`] and the [`Draw`] in a [`LearningTurn`], and the
//! record keeps both, so replay reaches the same decision without either.
//!
//! **The learned decision is not reached through [`RoutingPolicy::choose`].**
//! The learner needs inputs that [`RoutingContext`] does not carry, and adding
//! a field there reaches every constructor of the context, including the
//! credential module. So the inputs travel as a [`LearningTurn`] beside the
//! context, and the server's engine (`engine::learning`) calls the associated
//! function [`LearnedPolicy::choose`] directly for a `shadow` or `live`
//! project; it maps [`LearnedError::Refused`] to a turn that fails as a policy
//! refusal.
//!
//! **A [`LearnedPolicy`] value is still a [`RoutingPolicy`]** (milestone M9):
//! the router a process composes when a project enables the learner at boot.
//! It wraps the [`StagePolicy`] and serves every turn that reaches it through
//! the trait, which is every turn of a project with no learner, or with an
//! `off` one, exactly as the stage router would. What it changes is the name
//! on the record, `learned`, because that is the object in force: the same
//! reason `StagePolicy` reports `stage` for a project with no recipe.

use super::corrections::{Corrections, grant};
use super::evidence::{
    Draw, ExplorationEvidence, GateEvidence, GateResult, GrantCheck, LearnedChoice,
    LearnedEvidence, LearnedEvidenceParts, PlanEvidence, ReadFailure, ReadView, StoreRead, Unmet,
};
use super::explore::{eligible, propensity};
use super::gate::read_gate;
use super::input::LearnedInput;
use super::{
    ActiveMode, LEARNING_CREDIT_REVISION, LEARNING_INPUT_REVISION, LearnerTerms, OnInfeasible,
    Strategy,
};
use crate::classify::{AvailableClassification, ClassificationWindow};
use crate::control::TurnBudget;
use crate::routing::selection::{RecipeEvidence, SelectorSnapshot};
use crate::routing::stage::{Pick, RoutedPick, StagePolicy, TierRecipe, pick_tier};
use crate::routing::{
    Admitted, Decision, RoutingContext, RoutingError, RoutingPolicy, Target, TurnSignals,
};
use crate::validate::Arm;

/// What a learned decision needs beyond the [`RoutingContext`].
pub struct LearningTurn<'a> {
    /// The project's learner.
    pub terms: &'a LearnerTerms,
    /// What the store read returned for this turn, or why it returned nothing.
    pub view: &'a StoreRead,
    /// This turn's [`Draw::for_turn`].
    pub draw: Draw,
    /// The session's validation arm. Only a session whose arm consults the
    /// judge can explore: an exploring turn is a quality bypass, and only a
    /// reviewed one pays for itself in evidence.
    pub arm: Option<Arm>,
    /// The client declared tools on this turn.
    pub tool_turn: bool,
    /// The classification window the decision records, and the session's
    /// accepted classifications it names. See [`LearnedInput::encode`].
    ///
    /// **The input is derived by [`LearnedPolicy::turn_input`], not passed
    /// in**, because its `rules_pick` is the pick this policy plans under. An
    /// input built another way could carry a pick that disagrees with the
    /// plan served under it, and the turn's evidence would be counted under
    /// the wrong key. The engine keys its store read with the same function.
    pub window: Option<&'a ClassificationWindow>,
    pub available: &'a [AvailableClassification],
}

/// Why a learned decision served nothing.
#[derive(Debug, thiserror::Error)]
pub enum LearnedError {
    /// Admission or planning failed, exactly as it fails for the stage router.
    #[error(transparent)]
    Routing(#[from] RoutingError),
    /// The context carries no tier recipe. A strategy is a tier pick, so a
    /// learner without a recipe has nothing to choose between (draft 4.2).
    #[error("a learner routes by a tier recipe, and this project has none")]
    NoRecipe,
    /// `on_infeasible = refuse`, and no strategy met the constraints.
    #[error("no strategy met {}", labels(.unmet))]
    Refused { unmet: Vec<Unmet> },
}

fn labels(unmet: &[Unmet]) -> String {
    unmet
        .iter()
        .map(|unmet| unmet.label())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The learned router.
///
/// As a value, the composed [`RoutingPolicy`] of a process that runs the
/// learner: see the module doc. Its associated functions [`Self::turn_input`]
/// and [`Self::choose`] are the learned decision itself, and take no `self`
/// because the decision reads nothing the value holds.
pub struct LearnedPolicy {
    stage: StagePolicy,
}

/// The name a learned process records on every decision.
pub const LEARNED_POLICY_NAME: &str = "learned";

#[async_trait::async_trait]
impl RoutingPolicy for LearnedPolicy {
    fn name(&self) -> &str {
        LEARNED_POLICY_NAME
    }

    /// A strategy is a tier pick, so the learned router reads the recipe too.
    /// Answering `false` here would make the engine's unread-recipe warning
    /// fire for every tier project of a learned process.
    fn reads_tier_recipes(&self) -> bool {
        true
    }

    /// The stage decision, for every turn the engine does not route through
    /// the learned seam.
    async fn choose(&self, ctx: &RoutingContext<'_>) -> Result<Decision, RoutingError> {
        self.stage.choose(ctx).await
    }
}

impl LearnedPolicy {
    /// The learned router over the stage router it wraps.
    pub fn new(stage: StagePolicy) -> Self {
        Self { stage }
    }

    /// The `rules` pick and the [`LearnedInput`] one turn is keyed under.
    ///
    /// **The one derivation the store read and the decision share.** The
    /// engine reads the store at this input's keys before it calls
    /// [`Self::choose`], and `choose` plans and records under the same call.
    /// Two derivations would agree until one of them changed, and then the
    /// read would fetch one key's evidence for a decision recorded under
    /// another.
    pub fn turn_input(
        ctx: &RoutingContext<'_>,
        tool_turn: bool,
        window: Option<&ClassificationWindow>,
        available: &[AvailableClassification],
    ) -> Result<(Pick, LearnedInput), LearnedError> {
        let recipe = ctx.tiers.ok_or(LearnedError::NoRecipe)?;
        let (picker, threshold) = (recipe.picker(), recipe.confidence_threshold());
        let rules_pick = match ctx.signals {
            Some(signals) => pick_tier(signals, picker, threshold),
            None => pick_tier(&TurnSignals::default(), picker, threshold),
        };
        let input = LearnedInput::encode(rules_pick.tier, tool_turn, window, available);
        Ok((rules_pick, input))
    }

    /// The decision for one turn, with its [`LearnedEvidence`] on the
    /// selector.
    ///
    /// - `shadow`: the `rules` decision, route unchanged, and the learned
    ///   choice recorded as not applied.
    /// - `live`: the exploit strategy, an explored member, or the
    ///   `on_infeasible` path.
    ///
    /// **Takes an [`ActiveMode`], so an `off` project never reaches it.** An
    /// `off` turn is the stage decision and needs no store read and no draw,
    /// so the engine branches on [`LearnerMode::active`](super::LearnerMode::active)
    /// before it reads or draws anything. `terms.mode` is not read here: the
    /// mode passed is the one in force.
    ///
    /// **`serve_rules` and `shadow` serve the `rules` decision whole**: its
    /// target, its own fallbacks, its budget state. Only the rationale and the
    /// selector change, so a project moved to `shadow` routes every turn
    /// exactly as it did under `off`.
    pub fn choose(
        ctx: &RoutingContext<'_>,
        mode: ActiveMode,
        turn: &LearningTurn<'_>,
    ) -> Result<Decision, LearnedError> {
        let recipe = ctx.tiers.ok_or(LearnedError::NoRecipe)?;
        if ctx.candidates.is_empty() {
            return Err(RoutingError::NoCandidates.into());
        }
        // Admission once, as `StagePolicy::choose` takes it, so every plan is
        // routed over the pool and budget state `rules` is.
        let admitted = ctx.admissible(None)?;
        let (rules_pick, input) =
            Self::turn_input(ctx, turn.tool_turn, turn.window, turn.available)?;
        let terms = turn.terms;
        let empty = ReadView::default();
        let (view, failure) = match turn.view {
            StoreRead::Read(view) => (view, None),
            StoreRead::Unavailable { reason } => (&empty, Some(*reason)),
        };
        // J4: a pool with no frontier target is a local-only turn. Jev never
        // saw it, so its answers on the key are other sessions' and supply no
        // prior here.
        let frontier_admitted = admitted
            .pool()
            .iter()
            .any(|candidate| !candidate.target.is_local());

        let Plans {
            plans,
            rules_at,
            rules,
        } = Planner {
            recipe,
            rules_pick,
            admitted: &admitted,
            budget: ctx.budget,
            corrections: Corrections::new(
                view,
                ctx.ledger,
                ctx.isl_tokens,
                terms.latency_min_samples,
                terms.cache_min_samples,
            ),
            terms,
            input: &input,
            view,
            failure,
            frontier_admitted,
        }
        .plan_all()?;

        let order = exploit_order(&plans);
        let reference = order.first().copied().unwrap_or(rules_at);

        // Ruling 8: live only, a reviewed session only, a frontier target in
        // the pool, and a read that gave gate results to explore on. The
        // arena is the rate and the set, and exists only when a turn could
        // explore and something is eligible, so the draw and the propensity
        // read one guard.
        let rate = terms.exploration.map(|exploration| exploration.rate);
        let possible = mode == ActiveMode::Live
            && turn.arm.is_some_and(Arm::consults_judge)
            && frontier_admitted
            && failure.is_none();
        let arena = rate
            .filter(|_| possible)
            .map(|rate| (rate, eligible(&plans, &plans[reference])))
            .filter(|(_, set)| !set.is_empty());
        let explored = arena
            .as_ref()
            .filter(|(rate, _)| turn.draw.rate < *rate)
            .map(|(_, set)| {
                let member = turn.draw.member % set.len() as u64;
                // `member < set.len()`, so the index is in range.
                (set[member as usize], member)
            });

        let choice = match (explored, order.first()) {
            (Some((strategy, member)), _) => LearnedChoice::Explore { strategy, member },
            (None, Some(&exploit)) => LearnedChoice::Exploit {
                strategy: plans[exploit].strategy,
            },
            (None, None) => LearnedChoice::ConstraintUnmet {
                unmet: unmet(&plans, failure),
            },
        };

        // The route served, before any record exists to describe it.
        let route = route(mode, &choice, terms.on_infeasible, &plans, &order, &rules)?;

        let propensity = arena.as_ref().map_or(1.0, |(rate, set)| {
            let default = match order.first() {
                Some(&exploit) => Some(&plans[exploit].first),
                None if terms.on_infeasible == OnInfeasible::ServeRules => Some(&rules.target),
                None => None,
            };
            propensity(&plans, set, &route.target, default, *rate)
        });

        let evidence = LearnedEvidence::new(LearnedEvidenceParts {
            mode,
            epoch: terms.epoch,
            input_revision: LEARNING_INPUT_REVISION,
            credit_revision: LEARNING_CREDIT_REVISION,
            input,
            view: turn.view.clone(),
            recipe: RecipeEvidence::of(recipe),
            plans,
            choice,
            exploration: rate.map(|_| ExplorationEvidence {
                draw: turn.draw,
                possible,
                set: arena.map(|(_, set)| set).unwrap_or_default(),
            }),
            propensity,
        })
        .map_err(|error| policy_bug(&error.to_string()))?;
        Ok(decide(&admitted, route, evidence))
    }
}

/// What every strategy of one turn is planned against: one recipe, one
/// `rules` pick, one admitted pool, one store view.
struct Planner<'a> {
    recipe: &'a TierRecipe,
    rules_pick: Pick,
    admitted: &'a Admitted<'a>,
    budget: &'a TurnBudget,
    corrections: Corrections<'a>,
    terms: &'a LearnerTerms,
    input: &'a LearnedInput,
    view: &'a ReadView,
    failure: Option<ReadFailure>,
    frontier_admitted: bool,
}

/// Every configured strategy's plan evidence, in configured order, and the
/// `rules` decision.
///
/// **Only the `rules` decision is kept.** A learned route is assembled from
/// the plans' first targets (draft 7.6), so the other strategies' decisions
/// are never read; `shadow` and `serve_rules` serve the `rules` one whole.
struct Plans {
    plans: Vec<PlanEvidence>,
    rules_at: usize,
    rules: Decision,
}

impl Planner<'_> {
    fn plan_all(&self) -> Result<Plans, LearnedError> {
        let strategies = self.terms.strategies.as_slice();
        let mut plans = Vec::with_capacity(strategies.len());
        let mut rules = None;
        for &strategy in strategies {
            let (plan, decision) = self.plan(strategy)?;
            if strategy == Strategy::Rules {
                rules = Some((plans.len(), decision));
            }
            plans.push(plan);
        }
        let (rules_at, rules) =
            rules.ok_or_else(|| policy_bug("a strategy set always holds `rules`"))?;
        Ok(Plans {
            plans,
            rules_at,
            rules,
        })
    }

    /// One strategy's plan, its corrections, its hard constraints and its
    /// gate.
    fn plan(&self, strategy: Strategy) -> Result<(PlanEvidence, Decision), LearnedError> {
        let RoutedPick {
            decision,
            pick,
            outcome,
        } = strategy.plan(self.recipe, self.rules_pick, self.admitted)?;
        let candidate = self
            .admitted
            .pool()
            .iter()
            .copied()
            .find(|candidate| candidate.target == decision.target)
            .ok_or_else(|| policy_bug("a plan served a target outside the admitted pool"))?;
        let cost = self.corrections.cost(candidate);
        let ttft = self.corrections.first_output(candidate);
        let grant = grant(self.budget, decision.budget_state, candidate, &cost);
        let latency_met = ttft.adjusted_ms <= self.terms.latency_limit_ms as f64;
        // A failed read gives no gate result. `Unproven` with no level is the
        // record's closest statement, and the read failure in the view and in
        // the unmet list says why.
        let gate = match self.failure {
            Some(_) => GateEvidence {
                level: None,
                result: GateResult::Unproven,
            },
            None => read_gate(
                self.view,
                self.input,
                strategy,
                &self.terms.prior,
                &self.terms.quality,
                self.frontier_admitted,
            )
            .evidence(),
        };
        let plan = PlanEvidence {
            strategy,
            pick,
            outcome,
            first: decision.target.clone(),
            cost,
            ttft,
            grant,
            latency_met,
            gate,
        };
        Ok((plan, decision))
    }
}

/// The target a turn serves and where it fails over to, in order.
struct Route {
    target: Target,
    fallbacks: Vec<Target>,
}

/// The target and fallbacks a turn serves, or the refusal.
///
/// A `live` choice serves the chosen plan's first target with the passing
/// plans' first targets behind it. Everything else (`shadow`, and a `live`
/// turn under `serve_rules` that nothing passed) serves the `rules` decision's
/// target and its own fallbacks.
fn route(
    mode: ActiveMode,
    choice: &LearnedChoice,
    on_infeasible: OnInfeasible,
    plans: &[PlanEvidence],
    order: &[usize],
    rules: &Decision,
) -> Result<Route, LearnedError> {
    match (mode, choice) {
        (ActiveMode::Live, LearnedChoice::ConstraintUnmet { unmet })
            if on_infeasible == OnInfeasible::Refuse =>
        {
            Err(LearnedError::Refused {
                unmet: unmet.clone(),
            })
        }
        (ActiveMode::Live, LearnedChoice::Exploit { strategy })
        | (ActiveMode::Live, LearnedChoice::Explore { strategy, .. }) => {
            let served = plans
                .iter()
                .find(|plan| plan.strategy == *strategy)
                .map(|plan| plan.first.clone())
                .ok_or_else(|| policy_bug("the chosen strategy was planned"))?;
            let fallbacks = fallbacks(plans, order, &served);
            Ok(Route {
                target: served,
                fallbacks,
            })
        }
        _ => Ok(Route {
            target: rules.target.clone(),
            fallbacks: rules.fallbacks.clone(),
        }),
    }
}

/// The passing plans, cheapest corrected cost first, then lowest modeled first
/// output, then configured order.
///
/// Configured order is the index itself, so a stable sort on the two numbers
/// breaks the remaining ties by it. `total_cmp` keeps the order total even for
/// a NaN a broken quote could carry.
fn exploit_order(plans: &[PlanEvidence]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..plans.len())
        .filter(|&index| plans[index].meets_hard() && plans[index].gate.result == GateResult::Pass)
        .collect();
    order.sort_by(|&left, &right| {
        let (left, right) = (&plans[left], &plans[right]);
        left.cost
            .adjusted_usd
            .total_cmp(&right.cost.adjusted_usd)
            .then(left.ttft.adjusted_ms.total_cmp(&right.ttft.adjusted_ms))
    });
    order
}

/// The fallbacks of a learned route: the first targets of the passing plans,
/// in exploit order, without repeats and without the served target (draft
/// 7.6).
///
/// **Not a strategy's own second choices.** Its evidence is about its first
/// target only; a fallback it never earned evidence for would be an unchecked
/// target behind a checked one.
fn fallbacks(plans: &[PlanEvidence], order: &[usize], served: &Target) -> Vec<Target> {
    let mut fallbacks: Vec<Target> = Vec::new();
    for &index in order {
        let first = &plans[index].first;
        if first != served && !fallbacks.contains(first) {
            fallbacks.push(first.clone());
        }
    }
    fallbacks
}

/// Every constraint some plan failed, in one fixed order: quality, latency,
/// grant, then the read failure.
///
/// A failed read replaces quality, because quality was never evaluated: the
/// record says the store did not answer, not that the reviews were bad.
fn unmet(plans: &[PlanEvidence], failure: Option<ReadFailure>) -> Vec<Unmet> {
    let any = |failed: fn(&PlanEvidence) -> bool| plans.iter().any(failed);
    let quality = match failure {
        None => any(|plan| plan.gate.result != GateResult::Pass).then_some(Unmet::Quality),
        Some(_) => None,
    };
    let read = failure.map(|failure| match failure {
        ReadFailure::StoreUnavailable => Unmet::StoreUnavailable,
        ReadFailure::ReadTimedOut => Unmet::ReadTimedOut,
    });
    [
        quality,
        any(|plan| !plan.latency_met).then_some(Unmet::Latency),
        any(|plan| plan.grant == GrantCheck::Exceeds).then_some(Unmet::Grant),
        read,
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The served decision: `route` with the learned record on its selector.
///
/// Minted through [`Admitted::decide`], so the budget state, the admitted
/// list and the overflow note are admission's own, as for every other policy.
fn decide(admitted: &Admitted<'_>, route: Route, evidence: LearnedEvidence) -> Decision {
    let Route { target, fallbacks } = route;
    let rationale = evidence.rationale();
    let source = evidence.source();
    Decision {
        fallbacks,
        source,
        ..admitted.decide(target, rationale, SelectorSnapshot::learned(evidence))
    }
}

/// An invariant of this module failed. Surfaced as a routing failure rather
/// than a panic, so one broken turn fails as a turn.
fn policy_bug(what: &str) -> LearnedError {
    RoutingError::Policy(anyhow::anyhow!("learned policy: {what}")).into()
}
