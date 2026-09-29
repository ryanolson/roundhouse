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
//! **Not a [`RoutingPolicy`](crate::routing::RoutingPolicy) yet.** The learner
//! needs inputs that [`RoutingContext`] does not carry, and adding a field
//! there reaches every constructor of the context, including the credential
//! module. The engine seam, and the error mapping for [`LearnedError`], are the
//! M8 milestone of `agent-docs/PLAN-online-routing-learner.md`.

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
use crate::routing::selection::{RecipeEvidence, SelectorBranch, SelectorSnapshot, StageOutcome};
use crate::routing::stage::{Pick, StagePolicy, pick_tier};
use crate::routing::{Admitted, Decision, RoutingContext, RoutingError, Target};
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
    /// **The input is encoded here, not passed in**, because its `rules_pick`
    /// is the pick this policy computes. An input built by the caller could
    /// carry a pick that disagrees with the plan served under it, and the
    /// turn's evidence would be counted under the wrong key.
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
pub struct LearnedPolicy;

/// One strategy's plan, with the decision `route_pick` built for it.
struct Planned {
    evidence: PlanEvidence,
    decision: Decision,
}

impl Planned {
    /// The first target meets every hard constraint but the gate: admission
    /// (it was routed over the admitted pool), the grant, and latency.
    fn meets_hard(&self) -> bool {
        self.evidence.grant != GrantCheck::Exceeds && self.evidence.latency_met
    }

    fn passes(&self) -> bool {
        self.meets_hard() && self.evidence.gate.result == GateResult::Pass
    }
}

impl LearnedPolicy {
    /// The decision for one turn, with its [`LearnedEvidence`] on the
    /// selector.
    ///
    /// - `off`: the stage decision, with stage evidence and nothing learned.
    /// - `shadow`: the `rules` decision, route unchanged, and the learned
    ///   choice recorded as not applied.
    /// - `live`: the exploit strategy, an explored member, or the
    ///   `on_infeasible` path.
    ///
    /// **`serve_rules` and `shadow` serve the `rules` decision whole**: its
    /// target, its own fallbacks, its budget state. Only the rationale and the
    /// selector change, so a project moved to `shadow` routes every turn
    /// exactly as it did under `off`.
    pub fn choose(
        ctx: &RoutingContext<'_>,
        turn: &LearningTurn<'_>,
    ) -> Result<Decision, LearnedError> {
        let recipe = ctx.tiers.ok_or(LearnedError::NoRecipe)?;
        if ctx.candidates.is_empty() {
            return Err(RoutingError::NoCandidates.into());
        }
        // Admission once, as `StagePolicy::choose` takes it, so every plan is
        // routed over the pool and budget state `rules` is.
        let admitted = ctx.admissible(None)?;
        let signals = ctx.signals.cloned().unwrap_or_default();
        let rules_pick = pick_tier(&signals, recipe.picker(), recipe.confidence_threshold());
        let terms = turn.terms;
        let Some(mode) = terms.mode.active() else {
            return Ok(StagePolicy::route_pick(recipe, rules_pick, &admitted)?);
        };
        let input =
            LearnedInput::encode(rules_pick.tier, turn.tool_turn, turn.window, turn.available);
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

        let corrections = Corrections::new(
            view,
            ctx.ledger,
            ctx.isl_tokens,
            terms.latency_min_samples,
            terms.cache_min_samples,
        );
        let mut planned = Vec::with_capacity(terms.strategies.as_slice().len());
        for &strategy in terms.strategies.as_slice() {
            let decision = strategy.plan(recipe, rules_pick, &admitted)?;
            let candidate = admitted
                .pool()
                .iter()
                .copied()
                .find(|candidate| candidate.target == decision.target)
                .ok_or_else(|| policy_bug("a plan served a target outside the admitted pool"))?;
            let (pick, outcome) = stage_parts(&decision)?;
            let cost = corrections.cost(candidate);
            let ttft = corrections.first_output(candidate);
            let grant = grant(ctx.budget, decision.budget_state, candidate, &cost);
            let latency_met = ttft.adjusted_ms <= terms.latency_limit_ms as f64;
            // A failed read gives no gate result. `Unproven` with no level is
            // the record's closest statement, and the read failure in the view
            // and in the unmet list says why.
            let gate = match failure {
                Some(_) => GateEvidence {
                    level: None,
                    result: GateResult::Unproven,
                },
                None => read_gate(
                    view,
                    &input,
                    strategy,
                    &terms.prior,
                    &terms.quality,
                    frontier_admitted,
                )
                .evidence(),
            };
            planned.push(Planned {
                evidence: PlanEvidence {
                    strategy,
                    pick,
                    outcome,
                    first: decision.target.clone(),
                    cost,
                    ttft,
                    grant,
                    latency_met,
                    gate,
                },
                decision,
            });
        }

        let order = exploit_order(&planned);
        let rules_at = planned
            .iter()
            .position(|plan| plan.evidence.strategy == Strategy::Rules)
            .ok_or_else(|| policy_bug("a strategy set always holds `rules`"))?;
        let reference = order.first().copied().unwrap_or(rules_at);

        // Ruling 8: live only, a reviewed session only, a frontier target in
        // the pool, and a read that gave gate results to explore on.
        let rate = terms.exploration.map(|exploration| exploration.rate);
        let possible = mode == ActiveMode::Live
            && rate.is_some()
            && turn.arm.is_some_and(Arm::consults_judge)
            && frontier_admitted
            && failure.is_none();
        let plans: Vec<PlanEvidence> = planned.iter().map(|plan| plan.evidence.clone()).collect();
        let set = match possible {
            true => eligible(&plans, &plans[reference]),
            false => Vec::new(),
        };
        let explored = match rate {
            Some(rate) if possible && !set.is_empty() && turn.draw.rate < rate => {
                let member = turn.draw.member % set.len() as u64;
                // `member < set.len()`, so the index is in range.
                Some((set[member as usize], member))
            }
            _ => None,
        };

        let choice = match (explored, order.first()) {
            (Some((strategy, member)), _) => LearnedChoice::Explore { strategy, member },
            (None, Some(&exploit)) => LearnedChoice::Exploit {
                strategy: planned[exploit].evidence.strategy,
            },
            (None, None) => LearnedChoice::ConstraintUnmet {
                unmet: unmet(&planned, failure),
            },
        };

        // The route served, before any record exists to describe it.
        let rules = &planned[rules_at].decision;
        let route = match (mode, &choice) {
            (ActiveMode::Live, LearnedChoice::ConstraintUnmet { unmet })
                if terms.on_infeasible == OnInfeasible::Refuse =>
            {
                return Err(LearnedError::Refused {
                    unmet: unmet.clone(),
                });
            }
            (ActiveMode::Live, LearnedChoice::Exploit { strategy })
            | (ActiveMode::Live, LearnedChoice::Explore { strategy, .. }) => {
                let served = planned
                    .iter()
                    .find(|plan| plan.evidence.strategy == *strategy)
                    .map(|plan| plan.evidence.first.clone())
                    .ok_or_else(|| policy_bug("the chosen strategy was planned"))?;
                let fallbacks = fallbacks(&planned, &order, &served);
                (served, fallbacks)
            }
            _ => (rules.target.clone(), rules.fallbacks.clone()),
        };

        let propensity = match rate {
            Some(rate) if possible && !set.is_empty() => {
                let default = match order.first() {
                    Some(&exploit) => Some(&planned[exploit].evidence.first),
                    None if terms.on_infeasible == OnInfeasible::ServeRules => Some(&rules.target),
                    None => None,
                };
                propensity(&plans, &set, &route.0, default, rate)
            }
            _ => 1.0,
        };

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
                set,
            }),
            propensity,
        })
        .map_err(|error| policy_bug(&error.to_string()))?;
        Ok(decide(&admitted, route, evidence))
    }
}

/// The passing plans, cheapest corrected cost first, then lowest modeled first
/// output, then configured order.
///
/// Configured order is the index itself, so a stable sort on the two numbers
/// breaks the remaining ties by it. `total_cmp` keeps the order total even for
/// a NaN a broken quote could carry.
fn exploit_order(planned: &[Planned]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..planned.len())
        .filter(|&index| planned[index].passes())
        .collect();
    order.sort_by(|&left, &right| {
        let (left, right) = (&planned[left].evidence, &planned[right].evidence);
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
fn fallbacks(planned: &[Planned], order: &[usize], served: &Target) -> Vec<Target> {
    let mut fallbacks: Vec<Target> = Vec::new();
    for &index in order {
        let first = &planned[index].evidence.first;
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
fn unmet(planned: &[Planned], failure: Option<ReadFailure>) -> Vec<Unmet> {
    let any = |failed: fn(&PlanEvidence) -> bool| planned.iter().any(|plan| failed(&plan.evidence));
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
fn decide(
    admitted: &Admitted<'_>,
    route: (Target, Vec<Target>),
    evidence: LearnedEvidence,
) -> Decision {
    let (target, fallbacks) = route;
    let rationale = evidence.rationale();
    let source = evidence.source();
    Decision {
        fallbacks,
        source,
        ..admitted.decide(target, rationale, SelectorSnapshot::learned(evidence))
    }
}

/// The pick and outcome `route_pick` recorded for a plan.
fn stage_parts(decision: &Decision) -> Result<(Pick, StageOutcome), LearnedError> {
    match decision.selector.as_ref().map(|selector| &selector.branch) {
        Some(SelectorBranch::Stage(evidence)) => Ok((evidence.pick, evidence.outcome.clone())),
        _ => Err(policy_bug("route_pick records a stage branch")),
    }
}

/// An invariant of this module failed. Surfaced as a routing failure rather
/// than a panic, so one broken turn fails as a turn.
fn policy_bug(what: &str) -> LearnedError {
    RoutingError::Policy(anyhow::anyhow!("learned policy: {what}")).into()
}
