// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a learned decision records, and the store view it was taken from.
//!
//! **Replay reads this record; it never reads the store again and never draws
//! again.** Everything the policy consulted is here: the counts it read (or why
//! it could not read them), every strategy's plan with its corrected cost and
//! latency, each constraint and gate result, the draw, and the probability that
//! this turn served its first target. The offline calibrator weights a turn by
//! that probability, so a record without it could not be evaluated at all once
//! exploration is on.
//!
//! These types carry `SelectorBranch::Learned` on every `Routed` of a learned
//! turn. The branch holds them behind a `Box`, so a session event is the same
//! size whether or not the learner ran.

use std::ops::Deref;

use serde::{Deserialize, Serialize, Serializer};

use super::input::{KeyLevel, LearnedInput, LevelKey};
use super::{ActiveMode, EpochId, OnInfeasible, Strategy, StrategySet, StrategySetError};
use crate::routing::Target;
use crate::routing::selection::{RecipeEvidence, StageEvidence, StageOutcome};
use crate::routing::stage::{DecisionSource, Pick, Tier};

/// The fields of one learned decision's evidence, before they are checked.
///
/// The wire shape of [`LearnedEvidence`], and the only way to build one: pass
/// it to [`LearnedEvidence::new`]. No field is named `kind`:
/// `SelectorBranch` is internally tagged on it, and this struct is that
/// branch's payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearnedEvidenceParts {
    pub mode: ActiveMode,
    pub epoch: EpochId,
    /// [`LEARNING_INPUT_REVISION`](super::LEARNING_INPUT_REVISION) as of the
    /// process that took the decision.
    pub input_revision: u32,
    /// [`LEARNING_CREDIT_REVISION`](super::LEARNING_CREDIT_REVISION) as of the
    /// process that took the decision. Credit for this decision goes to this
    /// epoch under this rule, or nowhere.
    pub credit_revision: u32,
    /// The whole input. The three keys are its projections, see
    /// [`LearnedInput::keys`], so they are not written a second time.
    pub input: LearnedInput,
    pub view: StoreRead,
    /// The recipe every plan was routed under, once.
    pub recipe: RecipeEvidence,
    /// Every configured strategy's plan, in the configured order.
    ///
    /// **All of them, not only the served one.** Which strategies agreed with
    /// the served first target is what credit reads, and it is free: the plans
    /// are pure and were computed anyway.
    pub plans: Vec<PlanEvidence>,
    pub choice: LearnedChoice,
    /// `None` when the project configured no exploration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exploration: Option<ExplorationEvidence>,
    /// The logging probability of the served route: the probability that
    /// this turn served its first target's route, counting members by route
    /// (`same_route`), not by worker.
    ///
    /// `1.0` whenever the turn could not explore. With uniform exploration the
    /// exploit target has `1 - rate` plus `rate` times its share of the set,
    /// and every other target `rate` times its share (plan section 3, item 2).
    pub propensity: f64,
}

impl LearnedEvidenceParts {
    /// The strategy whose plan served the turn.
    ///
    /// `rules` in `shadow` mode and on an infeasible turn; the chosen strategy
    /// on a `live` turn that chose one.
    pub fn served_strategy(&self) -> Strategy {
        match (self.mode, self.choice.strategy()) {
            (ActiveMode::Live, Some(chosen)) => chosen,
            _ => Strategy::Rules,
        }
    }

    /// The plan of `strategy`, when it was configured.
    pub fn plan(&self, strategy: Strategy) -> Option<&PlanEvidence> {
        self.plans.iter().find(|plan| plan.strategy == strategy)
    }
}

/// Why a learned record was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LearnedEvidenceError {
    /// The plans' strategies are not a list a learner could be configured
    /// with: a repeat, a missing `rules`, or the wrong count.
    #[error("the plans are not a strategy list: {0}")]
    Plans(StrategySetError),
    /// The record served or chose a strategy it holds no plan for.
    #[error("the record names the `{strategy}` strategy and holds no plan for it")]
    MissingPlan { strategy: Strategy },
    /// The record explored a member that its recorded exploration set does
    /// not hold at that index, or it holds no exploration at all.
    #[error(
        "the record explored member {member} as `{strategy}` and its exploration set does not hold it"
    )]
    Exploration { strategy: Strategy, member: u64 },
}

/// One learned decision's evidence, checked.
///
/// **Everything the record names is present.** An explored choice's member
/// indexes the recorded exploration set and names the chosen strategy. The plans are one per
/// configured strategy with `rules` among them, and the served and chosen
/// strategies each have one. Without that, [`Self::source`] would answer
/// `None` and [`Self::rationale`] would drop its served clause, so a record
/// missing its served plan would read as a turn that served nothing rather than
/// failing where it was built or read. [`LearnedEvidence::new`] is the only
/// constructor and deserialization goes through it, so a bad durable record
/// fails loudly at decode.
///
/// The plans' strategies are checked as a [`StrategySet`], not against the
/// configured one: a record is read without the configuration that wrote it,
/// so the configured order cannot be checked here.
///
/// Read-only: the fields are reached through [`Deref`] to
/// [`LearnedEvidenceParts`], and there is no `DerefMut`, so the checked shape
/// cannot be edited after construction. [`Self::into_parts`] gives the fields
/// back to rebuild from.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "LearnedEvidenceParts")]
pub struct LearnedEvidence(LearnedEvidenceParts);

impl LearnedEvidence {
    pub fn new(parts: LearnedEvidenceParts) -> Result<Self, LearnedEvidenceError> {
        StrategySet::new(parts.plans.iter().map(|plan| plan.strategy).collect())
            .map_err(LearnedEvidenceError::Plans)?;
        for strategy in [Some(parts.served_strategy()), parts.choice.strategy()]
            .into_iter()
            .flatten()
        {
            if parts.plan(strategy).is_none() {
                return Err(LearnedEvidenceError::MissingPlan { strategy });
            }
        }
        // One comparison covers a missing exploration, a member outside the
        // set, and a member that names another strategy.
        if let LearnedChoice::Explore { strategy, member } = parts.choice {
            let recorded = parts
                .exploration
                .as_ref()
                .and_then(|exploration| exploration.set.get(usize::try_from(member).ok()?));
            if recorded != Some(&strategy) {
                return Err(LearnedEvidenceError::Exploration { strategy, member });
            }
        }
        Ok(Self(parts))
    }

    pub fn into_parts(self) -> LearnedEvidenceParts {
        self.0
    }

    /// The plan of a strategy [`Self::new`] checked is present.
    fn named(&self, strategy: Strategy) -> &PlanEvidence {
        self.plan(strategy)
            .expect("LearnedEvidence::new refuses a record missing a plan it names")
    }

    /// The plan that served the turn.
    pub fn served_plan(&self) -> &PlanEvidence {
        self.named(self.served_strategy())
    }

    /// The [`DecisionSource`] of the served plan.
    ///
    /// Read off the served plan's own pick and outcome by the same rule a
    /// stage decision uses, so a served forced pick reports
    /// [`DecisionSource::Strategy`] and never narrates a handoff. `None` only
    /// for a served plan that degraded past the recipe.
    pub fn source(&self) -> Option<DecisionSource> {
        let plan = self.served_plan();
        plan.outcome.source(&plan.pick)
    }

    /// The `rules` plan as the stage router itself would have recorded it.
    pub fn rules_stage(&self) -> StageEvidence {
        let plan = self.named(Strategy::Rules);
        StageEvidence {
            recipe: self.recipe.clone(),
            pick: plan.pick,
            outcome: plan.outcome.clone(),
        }
    }

    /// The account of this decision that reaches the audit trail and the
    /// calling model.
    ///
    /// It names the strategy, the tier, the served target, the epoch prefix and
    /// the key. **It never names a price**: `explain_last_route` republishes a
    /// rationale into the calling model's own context, and the prices that
    /// decided the turn are in [`PlanEvidence::cost`], which no model reads.
    ///
    /// **The key is the one the choice rests on**: the level the chosen
    /// strategy's gate read, or the full L2 key when nothing was chosen, since
    /// then no level decided anything and the whole input is the honest name.
    pub fn rationale(&self) -> String {
        let (choice, level) = match &self.choice {
            LearnedChoice::Exploit { strategy } => (
                format!("the {strategy} strategy passed"),
                self.named(*strategy).gate.level,
            ),
            LearnedChoice::Explore { strategy, .. } => (
                format!("the {strategy} strategy explored"),
                self.named(*strategy).gate.level,
            ),
            LearnedChoice::ConstraintUnmet { unmet } => (
                format!(
                    "no strategy met {}",
                    unmet
                        .iter()
                        .map(|unmet| unmet.label())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                None,
            ),
        };
        let key = self.input.key(level.unwrap_or(KeyLevel::L2));
        let mut rationale = format!(
            "learned router ({}, epoch {}, key {key}): {choice}",
            self.mode.label(),
            self.epoch.prefix(),
        );
        if self.mode == ActiveMode::Shadow
            && !matches!(self.choice, LearnedChoice::ConstraintUnmet { .. })
        {
            rationale.push_str("; shadow mode, so the learned choice was not applied");
        }
        let served = self.served_plan();
        rationale.push_str(&format!(
            "; served the {} strategy, {} tier ({})",
            served.strategy,
            self.recipe
                .tier_of(&served.first)
                .map(Tier::label)
                .unwrap_or("no recipe"),
            served.first.policy_identity(),
        ));
        rationale
    }
}

impl TryFrom<LearnedEvidenceParts> for LearnedEvidence {
    type Error = LearnedEvidenceError;

    fn try_from(parts: LearnedEvidenceParts) -> Result<Self, Self::Error> {
        Self::new(parts)
    }
}

impl Deref for LearnedEvidence {
    type Target = LearnedEvidenceParts;

    fn deref(&self) -> &LearnedEvidenceParts {
        &self.0
    }
}

/// Written as its parts, and by hand rather than with `serde(into)`, which
/// would clone the whole record on every learned `Routed` write.
impl Serialize for LearnedEvidence {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

/// One strategy's plan and what the policy made of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanEvidence {
    pub strategy: Strategy,
    /// The pick the plan was routed from: the scorer's own for `rules`, a
    /// forced tier with [`DecisionSource::Strategy`] for the others.
    pub pick: Pick,
    /// What `route_pick` did with the pick.
    pub outcome: StageOutcome,
    /// The plan's first target. Quality evidence for a strategy is evidence
    /// about this target only, which is why its fallbacks are not recorded.
    pub first: Target,
    pub cost: CostEvidence,
    pub ttft: TtftEvidence,
    pub grant: GrantCheck,
    /// The corrected first-output estimate is at or below the project limit.
    pub latency_met: bool,
    pub gate: GateEvidence,
}

impl PlanEvidence {
    /// The first target meets every hard constraint but the gate: admission
    /// (the plan was routed over the admitted pool), the grant on the
    /// corrected cost, and the latency limit.
    ///
    /// **One predicate for exploitation and exploration.** An explored member
    /// bypasses only the quality gate; were the two spellings to drift, an
    /// exploring turn could serve a route the exploit path would refuse on
    /// cost or latency. An overflow admission keeps its status and meets the
    /// grant (draft 7.2): only [`GrantCheck::Exceeds`] fails it.
    pub fn meets_hard(&self) -> bool {
        self.grant != GrantCheck::Exceeds && self.latency_met
    }
}

/// The first target's cost for this turn, before and after the cache
/// correction.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostEvidence {
    pub quoted_usd: f64,
    pub adjusted_usd: f64,
    pub correction: CostCorrection,
}

/// Whether the reuse correction ran, and why not when it did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostCorrection {
    /// The correction ran, and `adjusted_usd` is its result. That equals the
    /// quote when nothing moved, or when moving would have lowered it.
    Applied,
    /// The target's measured pairs predicted no reuse, so there is no ratio,
    /// and `adjusted_usd` is the quote re-priced with no cached tokens: a
    /// discount no pair measured is not banked.
    NoPredictedReuse,
    /// Fewer measured pairs than `cache_min_samples`. Also recorded for a
    /// target the store view holds nothing for, and for zero pairs under a
    /// configured minimum of zero: no samples is never a measurement.
    TooFewSamples,
    /// A local target. Its quote is the residency answer, at the configured
    /// capacity price when there is one, and it has no ledger model to
    /// re-price a shortfall against.
    NotFrontier,
}

/// First output from turn start, modeled (ruling 10): the quoted TTFT, the
/// target's mean residual, and the project's mean overhead before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TtftEvidence {
    pub quoted_ms: f64,
    pub adjusted_ms: f64,
    pub residual: LatencyTerm,
    pub overhead: LatencyTerm,
}

/// One additive latency term.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LatencyTerm {
    /// The mean was added, in whole milliseconds.
    Applied { mean_ms: i64 },
    /// Fewer samples than `latency_min_samples`, so nothing was added and the
    /// record says so rather than showing a zero that looks measured. Also
    /// recorded for a target the store view holds nothing for, and for zero
    /// samples under a configured minimum of zero.
    TooFewSamples,
}

/// The grant constraint on the corrected cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantCheck {
    /// The grant admits the corrected cost.
    Admits,
    /// The corrected cost is above what the grant admits.
    Exceeds,
    /// Admission re-admitted the candidate through the overflow valve, and the
    /// learner kept that status rather than adding or removing it.
    Overflow,
}

/// The quality gate's answer for one strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateEvidence {
    /// The most specific level whose live units met `min_evidence`, or `None`
    /// when no level met it.
    pub level: Option<KeyLevel>,
    pub result: GateResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateResult {
    Pass,
    Unproven,
    BelowFloor,
}

/// What the learner chose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LearnedChoice {
    /// The cheapest passing strategy.
    Exploit { strategy: Strategy },
    /// An exploring turn served `strategy`, which is `member` of the
    /// exploration set.
    ///
    /// `member` is [`Draw::member`] modulo the set size, and has the draw's
    /// width so the index is the remainder itself rather than a narrowing cast
    /// of it.
    Explore { strategy: Strategy, member: u64 },
    /// No strategy satisfied the constraints, and the turn served `rules`.
    ///
    /// The learner did not validate the route. It does not say the review of
    /// the turn is void: the served trajectory still earns credit.
    ConstraintUnmet { unmet: Vec<Unmet> },
}

impl LearnedChoice {
    /// The strategy chosen, or `None` when no strategy met the constraints.
    pub fn strategy(&self) -> Option<Strategy> {
        match self {
            LearnedChoice::Exploit { strategy } | LearnedChoice::Explore { strategy, .. } => {
                Some(*strategy)
            }
            LearnedChoice::ConstraintUnmet { .. } => None,
        }
    }
}

/// One reason a turn was infeasible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unmet {
    Quality,
    Latency,
    Grant,
    StoreUnavailable,
    ReadTimedOut,
}

impl Unmet {
    pub fn label(self) -> &'static str {
        match self {
            Unmet::Quality => "quality",
            Unmet::Latency => "latency",
            Unmet::Grant => "grant",
            Unmet::StoreUnavailable => "store_unavailable",
            Unmet::ReadTimedOut => "read_timed_out",
        }
    }
}

/// The exploration half of a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplorationEvidence {
    pub draw: Draw,
    /// Whether this turn could explore at all.
    pub possible: bool,
    /// The exploration set: the cheaper unproven members in configured
    /// strategy order, then `rules` when it is not already one (see
    /// [`eligible`](super::explore::eligible)). Empty when the turn could
    /// not explore.
    pub set: Vec<Strategy>,
    /// The project's `on_infeasible` when the turn was decided.
    ///
    /// **Recorded because the set depends on it** (the owner's ruling of
    /// 2026-09-30): under `refuse`, `rules` joins only when some strategy
    /// passes. Without it, the calibrator could not re-derive the set of a
    /// turn that nothing passed, and could not tell a `refuse` record from a
    /// `serve_rules` one.
    ///
    /// **`None` only on a record written before the field existed**; the
    /// policy always writes it. Optional so that such a record still decodes:
    /// a required field would fail the decode, and one undecodable event makes
    /// the whole log unreadable, to the Redis reader and to a dump alike.
    /// Replay refuses the record's interval instead
    /// ([`replays`](super::offline::extract::replays)), since the set rule it
    /// was drawn under is unknown. A non-optional default would assign a rule
    /// nobody recorded, and a set that happened to match it would replay.
    #[serde(default)]
    pub on_infeasible: Option<OnInfeasible>,
}

/// The deterministic exploration draw, recorded so replay never draws again.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Draw {
    /// The rate draw, a fraction in `[0, 1)`.
    pub rate: f64,
    /// The member draw; the member is this value modulo the set size.
    pub member: u64,
}

/// What the store read returned for this turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoreRead {
    Read(ReadView),
    Unavailable { reason: ReadFailure },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadFailure {
    StoreUnavailable,
    ReadTimedOut,
}

/// The counters one turn read, for one project and one epoch, as
/// [`LearnerStore::read`](crate::learn_store::LearnerStore::read) returns
/// them.
///
/// Integers throughout: integer addition gives the same state in every order of
/// updates, so an offline rebuild matches the store exactly.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReadView {
    /// One entry per key of the turn, most specific first.
    pub levels: Vec<LevelView>,
    /// The operational counters of each recipe target read.
    pub targets: Vec<TargetOps>,
    /// The project's overhead from `TurnStarted` to the served `Routed`, the
    /// third term of the latency model.
    pub overhead: LatencySum,
}

impl ReadView {
    /// The operational counters of `target`, found by its policy identity.
    ///
    /// Identity rather than ledger key, because the counters are per recipe
    /// target: every worker of one local model shares one residual.
    pub fn target(&self, target: &Target) -> Option<&TargetOps> {
        let identity = target.policy_identity();
        self.targets.iter().find(|ops| ops.target == identity)
    }
}

/// Quality counts and Jev answers at one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelView {
    pub key: LevelKey,
    pub strategies: Vec<StrategyCounts>,
    /// Jev's tier answers on this key. They supply a prior, never a reward.
    pub jev: JevCounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyCounts {
    pub strategy: Strategy,
    pub pos_units: u64,
    pub n_units: u64,
    pub sessions: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct JevCounts {
    pub capable: u64,
    pub efficient: u64,
}

/// The operational counters of one recipe target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetOps {
    /// The target's policy identity, as the recipe names it.
    pub target: String,
    /// Residuals from the served `Routed` to the first output, less the quote.
    pub latency: LatencySum,
    pub failover: u64,
    pub cache: CacheReuse,
}

/// A sum of millisecond samples and their count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LatencySum {
    pub sum_ms: i64,
    pub n: u64,
}

/// Predicted and observed cache reuse, in per-mille, over measured pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CacheReuse {
    pub predicted_permille: u64,
    pub observed_permille: u64,
    pub n: u64,
}
