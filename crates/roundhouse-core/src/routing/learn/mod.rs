// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The data half of the online routing learner: serving strategies, the learned
//! input and its keys, the configuration a learner runs under, and the evidence
//! a learned decision records.
//!
//! **Data and pure functions only.** No gate, no constraint check, and no
//! policy lives here yet; `agent-docs/PLAN-online-routing-learner.md` builds
//! those in later milestones on top of these types. Nothing in the engine
//! composes a learner, so every turn still routes exactly as
//! [`StagePolicy`](super::StagePolicy) routes it.
//!
//! A strategy is a *tier pick*, never a target. Each one is planned through
//! [`StagePolicy::route_pick`](super::StagePolicy::route_pick), the same code
//! the stage router serves with, so the recipe order, the dominance cost guard
//! and degrade-to-local are the same for every strategy. A strategy that routed
//! by its own rules would earn quality evidence for a plan the stage router
//! could never serve.

pub mod evidence;
pub mod input;

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::selection::STAGE_SELECTOR_REVISION;
use super::stage::{DecisionSource, Pick, StagePolicy, Tier, TierRecipe};
use super::{Admitted, Decision, RoutingError};

pub use evidence::{
    CacheReuse, CostCorrection, CostEvidence, Draw, ExplorationEvidence, GateEvidence, GateResult,
    GrantCheck, JevCounts, LatencySum, LatencyTerm, LearnedChoice, LearnedEvidence, LevelView,
    PlanEvidence, ReadFailure, ReadView, RecipeEvidence, StoreRead, StrategyCounts, TargetOps,
    TtftEvidence, Unmet,
};
pub use input::{Band, KeyLevel, LearnedInput, LevelKey, PriorBand, SEQUENCE_LEN};

/// The revision of the learned input: its fields, the complexity bands, the
/// sequence length and the three key levels.
///
/// Part of the epoch id, so a change here starts learned state afresh rather
/// than reading counters that were keyed by a different question.
pub const LEARNING_INPUT_REVISION: u32 = 1;

/// The revision of the rule that turns an accepted interval into credit.
///
/// Recorded on every learned decision, so the fold can refuse to credit a
/// decision that was taken under a rule this build does not apply.
pub const LEARNING_CREDIT_REVISION: u32 = 1;

/// The revision of the two fixed-tier strategies.
///
/// `rules` is versioned by [`STAGE_SELECTOR_REVISION`] instead, because its
/// pick *is* the stage router's pick.
pub const STRATEGY_REVISION: u32 = 1;

/// One serving strategy: a way to pick a tier for a turn.
///
/// Three, and no target arms, by the 2026-09-28 ruling 14. A calibrated `rules`
/// strategy is deferred: its pick would have to join the key, or it would earn
/// evidence where it agrees with `rules` and be served where it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// The stage router's own pick, from the recipe picker and threshold.
    Rules,
    /// Always the efficient tier.
    Efficient,
    /// Always the capable tier.
    Capable,
}

impl Strategy {
    /// The stable name, as configuration and the store spell it.
    pub fn label(self) -> &'static str {
        match self {
            Strategy::Rules => "rules",
            Strategy::Efficient => "efficient",
            Strategy::Capable => "capable",
        }
    }

    /// The revision that versions this strategy's pick.
    pub fn revision(self) -> u32 {
        match self {
            Strategy::Rules => STAGE_SELECTOR_REVISION,
            Strategy::Efficient | Strategy::Capable => STRATEGY_REVISION,
        }
    }

    /// This strategy's tier pick, given what the stage scorer picked.
    ///
    /// **`rules` returns the scorer's pick unchanged, source included.** A
    /// `rules` plan is then byte for byte the stage router's decision, which is
    /// what lets `shadow` mode serve it without changing a single route.
    ///
    /// **A fixed-tier pick carries [`DecisionSource::Strategy`], never the
    /// scorer's source.** Copying `Override` onto a forced capable pick would
    /// tell the handoff gate that a signal escalated the turn, and the capable
    /// model would read a note saying the previous steps were in trouble when
    /// nothing measured that. No score and no confidence either: the scorer was
    /// not asked.
    pub fn pick(self, rules: Pick) -> Pick {
        let tier = match self {
            Strategy::Rules => return rules,
            Strategy::Efficient => Tier::Efficient,
            Strategy::Capable => Tier::Capable,
        };
        Pick {
            tier,
            source: DecisionSource::Strategy,
            score: 0.0,
            confidence: None,
        }
    }

    /// This strategy's plan over one admitted pool.
    ///
    /// Admission is the caller's, taken once per turn, so every strategy of a
    /// turn is planned over the same pool and the same budget state.
    pub fn plan(
        self,
        recipe: &TierRecipe,
        rules: Pick,
        admitted: &Admitted<'_>,
    ) -> Result<Decision, RoutingError> {
        StagePolicy::route_pick(recipe, self.pick(rules), admitted)
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Why a strategy list was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StrategySetError {
    #[error("a learner needs 2 or 3 strategies, got {count}")]
    Count { count: usize },
    #[error("strategy `{strategy}` is listed twice")]
    Repeated { strategy: Strategy },
    #[error("the strategy list must contain `rules`, which shadow mode serves")]
    NoRules,
}

/// The configured strategies, in the configured order.
///
/// **The order is configuration, not presentation:** it breaks cost ties
/// between passing strategies and orders the exploration set. It is kept as
/// written, never sorted.
///
/// **`rules` is always present.** It is what `shadow` mode serves and what an
/// infeasible turn falls back to, so a list without it has no answer for either
/// and is refused here rather than on the first turn that needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Strategy>", into = "Vec<Strategy>")]
pub struct StrategySet(Vec<Strategy>);

impl StrategySet {
    pub fn new(strategies: Vec<Strategy>) -> Result<Self, StrategySetError> {
        if !(2..=3).contains(&strategies.len()) {
            return Err(StrategySetError::Count {
                count: strategies.len(),
            });
        }
        for (index, strategy) in strategies.iter().enumerate() {
            if strategies[..index].contains(strategy) {
                return Err(StrategySetError::Repeated {
                    strategy: *strategy,
                });
            }
        }
        if !strategies.contains(&Strategy::Rules) {
            return Err(StrategySetError::NoRules);
        }
        Ok(Self(strategies))
    }

    pub fn as_slice(&self) -> &[Strategy] {
        &self.0
    }
}

impl TryFrom<Vec<Strategy>> for StrategySet {
    type Error = StrategySetError;

    fn try_from(strategies: Vec<Strategy>) -> Result<Self, Self::Error> {
        Self::new(strategies)
    }
}

impl From<StrategySet> for Vec<Strategy> {
    fn from(set: StrategySet) -> Self {
        set.0
    }
}

/// Whether a project runs the learner, as configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearnerMode {
    /// Today's stage decision, and no learned evidence at all.
    #[default]
    Off,
    /// Serve `rules`, and record what the learner would have chosen.
    Shadow,
    /// Serve the learner's choice.
    Live,
}

impl LearnerMode {
    /// The mode a learned decision records, or `None` for `off`.
    pub fn active(self) -> Option<ActiveMode> {
        match self {
            LearnerMode::Off => None,
            LearnerMode::Shadow => Some(ActiveMode::Shadow),
            LearnerMode::Live => Some(ActiveMode::Live),
        }
    }
}

/// The mode on a learned decision.
///
/// **A separate type from [`LearnerMode`] because `off` writes no evidence.** A
/// record that said `off` would be describing a decision the learner never
/// took; this type has no way to say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveMode {
    Shadow,
    Live,
}

impl ActiveMode {
    pub fn label(self) -> &'static str {
        match self {
            ActiveMode::Shadow => "shadow",
            ActiveMode::Live => "live",
        }
    }
}

/// What a `live` turn does when no strategy satisfies the constraints.
///
/// `serve_rules` is the default by the 2026-09-28 ruling 7: `refuse` fails
/// every live turn during a learner-store outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnInfeasible {
    /// Serve the `rules` decision and record the unmet constraints.
    #[default]
    ServeRules,
    /// Fail the turn with a routing error that names the unmet constraints.
    Refuse,
}

/// The quality gate's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QualityTerms {
    /// The positive-review rate a strategy's lower bound must reach.
    pub floor: f64,
    /// The Wilson interval's z.
    pub z: f64,
    /// Live units at a level before the gate reads that level.
    pub min_evidence: u64,
    /// Sessions at a level before the gate can pass it.
    pub min_sessions: u64,
}

/// The exploration block of a `live` project.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExplorationTerms {
    /// The fraction of eligible turns that explore, in `(0, 1]`.
    pub rate: f64,
}

/// The identity of one learned model: 16 bytes of SHA-256 over the artifact,
/// the strategy list and the revisions.
///
/// **Written as hex on the wire**, because the id is a key part in the learner
/// store and a label in every record, and both are read by people.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EpochId([u8; 16]);

impl EpochId {
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The first 8 hex digits, which is what a rationale names.
    ///
    /// Enough to tell two epochs of one project apart in an audit trail; the
    /// record carries the whole id.
    pub fn prefix(&self) -> String {
        hex::encode(&self.0[..4])
    }
}

impl fmt::Display for EpochId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

impl From<EpochId> for String {
    fn from(epoch: EpochId) -> Self {
        epoch.to_string()
    }
}

impl TryFrom<String> for EpochId {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let bytes = hex::decode(&text).map_err(|error| format!("epoch id `{text}`: {error}"))?;
        let bytes: [u8; 16] = bytes
            .try_into()
            .map_err(|_| format!("epoch id `{text}` is not 16 bytes"))?;
        Ok(Self(bytes))
    }
}

/// Positive and total credit units for one strategy at one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Units {
    pub pos: u64,
    pub n: u64,
}

/// The calibration artifact's prior units, for each level key and strategy.
///
/// **Only the artifact's prior.** The Jev prior is computed on the turn from
/// the Jev counts in the [`ReadView`], and it applies only where this prior is
/// zero (plan section 4): an artifact prior is review evidence carried across
/// epochs, and a scout's opinion does not outrank it.
///
/// A missing entry is zero units, which is what an artifact with no evidence
/// for a key means.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PriorUnits(BTreeMap<(LevelKey, Strategy), Units>);

impl PriorUnits {
    pub fn new(entries: impl IntoIterator<Item = ((LevelKey, Strategy), Units)>) -> Self {
        Self(entries.into_iter().collect())
    }

    /// The prior for `strategy` at `key`, zero where the artifact holds none.
    pub fn get(&self, key: &LevelKey, strategy: Strategy) -> Units {
        self.0.get(&(*key, strategy)).copied().unwrap_or_default()
    }
}

/// One project's resolved learner: its configuration, its artifact prior and
/// its epoch.
///
/// Data only. The configuration loader that builds and validates it is a later
/// milestone; [`StrategySet`] already refuses the one shape no policy could
/// serve.
#[derive(Debug, Clone, PartialEq)]
pub struct LearnerTerms {
    pub mode: LearnerMode,
    pub strategies: StrategySet,
    pub epoch: EpochId,
    pub prior: PriorUnits,
    pub quality: QualityTerms,
    /// First output from turn start, in milliseconds (ruling 10).
    pub latency_limit_ms: u64,
    pub latency_min_samples: u64,
    pub cache_min_samples: u64,
    pub on_infeasible: OnInfeasible,
    /// `None` is a project that does not explore.
    pub exploration: Option<ExplorationTerms>,
}
