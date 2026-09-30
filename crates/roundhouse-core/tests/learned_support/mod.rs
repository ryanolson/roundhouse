// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared fixtures for the learned policy suites.
//!
//! The recipe is `efficient: [local/small]` and
//! `capable: [frontier/large]`. The quality terms are the starting
//! numbers (floor 0.8, z 1.96, 5 intervals of live evidence, 20 sessions), so
//! a fixture that passes here passes under the configuration a deployment
//! would actually write.
//!
//! The `rules` pick comes from the picker default on empty signals:
//! `CapableFirst` picks capable and `EfficientFirst` picks efficient. The
//! expected `rules` decision is computed with `pick_tier` and
//! `StagePolicy::route_pick`, which the M2 suite proves equal to
//! `StagePolicy::choose`.

#![allow(dead_code)]

use roundhouse_core::classify::projection::PROJECTION_REVISION;
use roundhouse_core::classify::{
    AvailableClassification, ClassificationRef, ClassificationWindow, ContextDependence, Graded,
    TAXONOMY_VERSION, TurnClassification, TurnComplexity, TurnIntent,
};
use roundhouse_core::control::{FrontierHistory, TurnBudget, TurnPolicy};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::routing::learn::{
    CacheReuse, Draw, EpochId, ExplorationTerms, JevCounts, LatencySum, LearnedError,
    LearnedEvidence, LearnedInput, LearnedPolicy, LearnerMode, LearnerTerms, LearningTurn,
    LevelKey, LevelView, OnInfeasible, PriorUnits, QualityTerms, ReadView, StoreRead, Strategy,
    StrategyCounts, StrategySet, TargetOps,
};
use roundhouse_core::routing::stage::{DEFAULT_CONFIDENCE_THRESHOLD, pick_tier};
use roundhouse_core::routing::{
    CacheLedger, CacheModel, Candidate, Decision, PickerMode, ProviderPricing, RoutingContext,
    SelectorBranch, StagePolicy, Target, TierRecipe, TurnSignals,
};
use roundhouse_core::validate::Arm;

pub const FLOOR: f64 = 0.8;
pub const Z: f64 = 1.96;
pub const MIN_EVIDENCE: u64 = 5_000;
pub const MIN_SESSIONS: u64 = 20;
pub const LATENCY_LIMIT_MS: u64 = 10_000;
pub const MIN_SAMPLES: u64 = 20;
pub const RATE: f64 = 0.05;
pub const ISL: usize = 10_000;

/// Live counts `(pos_units, n_units, sessions)`.
pub type Counts = (u64, u64, u64);

/// 20 intervals, all positive, over 20 sessions: the Wilson lower bound is
/// 0.839, so the gate passes at the ruled floor.
pub const PASS: Counts = (20_000, 20_000, 20);
/// 5 intervals, all positive: enough live evidence to be read, but the lower
/// bound is 0.566, so the gate cannot pass it yet.
pub const UNPROVEN: Counts = (5_000, 5_000, 20);
/// 20 intervals, all negative: the upper bound is 0.161, below the floor.
pub const BELOW: Counts = (0, 20_000, 20);

/// The parent rate card: input 1, write 2, read 0.1 per MTok.
pub const CARD: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 1.0,
    cached_input_per_mtok_usd: 0.1,
    cache_write_per_mtok_usd: 2.0,
    output_per_mtok_usd: 0.0,
};

pub fn large() -> Target {
    Target::Frontier {
        provider: "frontier".into(),
        model: "large".into(),
    }
}

pub fn medium() -> Target {
    Target::Frontier {
        provider: "frontier".into(),
        model: "medium".into(),
    }
}

pub fn small() -> Target {
    Target::Local {
        worker_id: 1,
        dp_rank: 0,
        model: "small".into(),
    }
}

/// A frontier quote with `cached` of the input predicted warm, priced on
/// [`CARD`] the way `FrontierCatalog::quote` prices one.
pub fn hosted(target: Target, cached: f64, ttft_ms: f64) -> Candidate {
    let uncached = ISL as f64 - cached;
    Candidate {
        target,
        expected_prefill_tokens: uncached,
        matched_prefix_tokens: cached as u64,
        expected_ttft_ms: ttft_ms,
        expected_cost_usd: CARD.price_tokens(uncached, cached, 0.0),
        quality_prior: 0.9,
        load: None,
    }
}

pub fn local(target: Target, ttft_ms: f64) -> Candidate {
    Candidate {
        target,
        expected_prefill_tokens: ISL as f64,
        matched_prefix_tokens: 0,
        expected_ttft_ms: ttft_ms,
        expected_cost_usd: 0.0,
        quality_prior: 0.6,
        load: Some(0.0),
    }
}

/// `frontier/large` cold at 800 ms, and `local/small` at 300 ms.
pub fn section_7_8_pool() -> Vec<Candidate> {
    vec![hosted(large(), 0.0, 800.0), local(small(), 300.0)]
}

/// `efficient: [local/small]`, `capable: [frontier/large]`, or with
/// `frontier/medium` after `local/small` in the efficient list when `medium`.
pub fn recipe(picker: PickerMode, medium_efficient: bool) -> TierRecipe {
    let mut efficient = vec![small().policy_identity()];
    if medium_efficient {
        efficient.push(medium().policy_identity());
    }
    TierRecipe::new(
        vec![large().policy_identity()],
        efficient,
        picker,
        DEFAULT_CONFIDENCE_THRESHOLD,
    )
    .expect("a two-tier recipe")
}

pub fn quality() -> QualityTerms {
    QualityTerms {
        floor: FLOOR,
        z: Z,
        min_evidence: MIN_EVIDENCE,
        min_sessions: MIN_SESSIONS,
    }
}

pub fn all_strategies() -> StrategySet {
    StrategySet::new(vec![
        Strategy::Rules,
        Strategy::Efficient,
        Strategy::Capable,
    ])
    .expect("three strategies with rules")
}

pub fn terms(mode: LearnerMode) -> LearnerTerms {
    LearnerTerms {
        mode,
        strategies: all_strategies(),
        epoch: EpochId::new([7; 16]),
        prior: PriorUnits::default(),
        quality: quality(),
        latency_limit_ms: LATENCY_LIMIT_MS,
        latency_min_samples: MIN_SAMPLES,
        cache_min_samples: MIN_SAMPLES,
        on_infeasible: OnInfeasible::default(),
        exploration: None,
        read_timeout_ms: 25,
    }
}

pub fn exploring(mode: LearnerMode) -> LearnerTerms {
    LearnerTerms {
        exploration: Some(ExplorationTerms { rate: RATE }),
        ..terms(mode)
    }
}

pub fn counts(strategy: Strategy, (pos_units, n_units, sessions): Counts) -> StrategyCounts {
    StrategyCounts {
        strategy,
        pos_units,
        n_units,
        sessions,
    }
}

pub fn level(key: LevelKey, strategies: &[(Strategy, Counts)], jev: JevCounts) -> LevelView {
    LevelView {
        key,
        strategies: strategies
            .iter()
            .map(|&(strategy, live)| counts(strategy, live))
            .collect(),
        jev,
    }
}

pub fn read(levels: Vec<LevelView>, targets: Vec<TargetOps>) -> StoreRead {
    StoreRead::Read(ReadView {
        levels,
        targets,
        overhead: LatencySum::default(),
    })
}

pub fn cold() -> StoreRead {
    read(Vec::new(), Vec::new())
}

/// A rate draw above any configured rate: the turn does not explore.
pub const STAY: Draw = Draw {
    rate: 0.99,
    member: 0,
};

/// A rate draw of zero, below any configured rate, with `member`.
pub fn go(member: u64) -> Draw {
    Draw { rate: 0.0, member }
}

/// A classification of `source_turn` with `complexity`, landed at
/// `source_turn + 1`.
pub fn classified(source_turn: u64, complexity: TurnComplexity) -> AvailableClassification {
    AvailableClassification {
        reference: ClassificationRef {
            call_id: ResponseId::new(format!("eval_{source_turn}")),
            source_turn_index: source_turn,
            available_seq: source_turn + 1,
        },
        classification: TurnClassification {
            taxonomy_version: TAXONOMY_VERSION,
            intent: Graded {
                value: TurnIntent::Implement,
                confidence: 0.9,
            },
            complexity: Graded {
                value: complexity,
                confidence: 0.9,
            },
            context_dependence: Graded {
                value: ContextDependence::Recent,
                confidence: 0.9,
            },
            tier: None,
        },
    }
}

/// One turn's learner inputs, owned, so a test can build a [`LearningTurn`].
pub struct Turn {
    pub terms: LearnerTerms,
    pub view: StoreRead,
    pub draw: Draw,
    pub arm: Option<Arm>,
    pub tool_turn: bool,
    pub available: Vec<AvailableClassification>,
    pub window: Option<ClassificationWindow>,
}

impl Turn {
    /// A reviewed turn (`Arm::Live`) with no classifications and no tools.
    pub fn new(terms: LearnerTerms, view: StoreRead, draw: Draw) -> Self {
        Self {
            terms,
            view,
            draw,
            arm: Some(Arm::Live),
            tool_turn: false,
            available: Vec::new(),
            window: None,
        }
    }

    /// The classification sequence, oldest first, as the window at a cutoff
    /// after all of them names it.
    pub fn with_sequence(mut self, complexities: &[TurnComplexity]) -> Self {
        self.available = complexities
            .iter()
            .enumerate()
            .map(|(turn, &complexity)| classified(turn as u64, complexity))
            .collect();
        let references: Vec<ClassificationRef> = self
            .available
            .iter()
            .map(|entry| entry.reference.clone())
            .collect();
        self.window = Some(ClassificationWindow::of(
            PROJECTION_REVISION,
            1_000,
            8,
            references.iter(),
        ));
        self
    }

    pub fn learning(&self) -> LearningTurn<'_> {
        LearningTurn {
            terms: &self.terms,
            view: &self.view,
            draw: self.draw,
            arm: self.arm,
            tool_turn: self.tool_turn,
            window: self.window.as_ref(),
            available: &self.available,
        }
    }
}

pub struct Rig {
    pub session_id: SessionId,
    pub ledger: CacheLedger,
    pub turn_policy: TurnPolicy,
    pub frontier_history: FrontierHistory,
    pub budget: TurnBudget,
    pub recipe: TierRecipe,
    pub signals: TurnSignals,
    pub candidates: Vec<Candidate>,
}

impl Rig {
    pub fn new(picker: PickerMode, candidates: Vec<Candidate>) -> Self {
        Self::with_recipe(recipe(picker, false), candidates)
    }

    pub fn with_recipe(recipe: TierRecipe, candidates: Vec<Candidate>) -> Self {
        let mut ledger = CacheLedger::new();
        for target in [large(), medium()] {
            ledger.register(&target, CacheModel::Deterministic { ttl_ms: 300_000 }, CARD);
        }
        Self {
            session_id: SessionId::new("acme/ada/main"),
            ledger,
            turn_policy: TurnPolicy::unrestricted(),
            frontier_history: FrontierHistory::default(),
            budget: TurnBudget::Unlimited,
            recipe,
            signals: TurnSignals::default(),
            candidates,
        }
    }

    pub fn ctx(&self) -> RoutingContext<'_> {
        RoutingContext {
            session_id: &self.session_id,
            turn_index: 3,
            isl_tokens: ISL,
            candidates: &self.candidates,
            ledger: &self.ledger,
            turn_policy: &self.turn_policy,
            frontier_history: &self.frontier_history,
            budget: &self.budget,
            signals: Some(&self.signals),
            tiers: Some(&self.recipe),
        }
    }

    pub fn choose(&self, turn: &Turn) -> Result<Decision, LearnedError> {
        let mode = turn
            .terms
            .mode
            .active()
            .expect("an `off` project is the engine's branch and never reaches `choose`");
        LearnedPolicy::choose(&self.ctx(), mode, &turn.learning())
    }

    pub fn chosen(&self, turn: &Turn) -> Decision {
        self.choose(turn)
            .expect("the learned policy routes this turn")
    }

    /// The stage router's own decision for this turn.
    pub fn rules(&self) -> Decision {
        let ctx = self.ctx();
        let admitted = ctx.admissible(None).expect("the pool admits something");
        let pick = pick_tier(
            &self.signals,
            self.recipe.picker(),
            self.recipe.confidence_threshold(),
        );
        StagePolicy::route_pick(&self.recipe, pick, &admitted)
            .expect("rules routes")
            .decision
    }

    /// The learned input the policy encodes for `turn`.
    pub fn input(&self, turn: &Turn) -> LearnedInput {
        let pick = pick_tier(
            &self.signals,
            self.recipe.picker(),
            self.recipe.confidence_threshold(),
        );
        LearnedInput::encode(
            pick.tier,
            turn.tool_turn,
            turn.window.as_ref(),
            &turn.available,
        )
    }
}

pub fn evidence(decision: &Decision) -> &LearnedEvidence {
    match &decision
        .selector
        .as_ref()
        .expect("a learned decision records its branch")
        .branch
    {
        SelectorBranch::Learned(evidence) => evidence,
        other => panic!("expected a learned branch, got {other:?}"),
    }
}

pub fn ops(target: &Target, latency: LatencySum) -> TargetOps {
    TargetOps {
        target: target.policy_identity(),
        latency,
        failover: 0,
        cache: Default::default(),
    }
}

/// A residual that puts any target over the 10 s limit once applied.
pub fn slow(target: &Target) -> TargetOps {
    ops(
        target,
        LatencySum {
            sum_ms: 20_000 * MIN_SAMPLES as i64,
            n: MIN_SAMPLES,
        },
    )
}

/// Measured pairs that predicted full reuse of `target` and observed
/// `observed_permille` of it.
pub fn reuse(target: &Target, observed_permille: u64) -> TargetOps {
    TargetOps {
        cache: CacheReuse {
            predicted_permille: 1_000 * MIN_SAMPLES,
            observed_permille: observed_permille * MIN_SAMPLES,
            n: MIN_SAMPLES,
        },
        ..ops(target, LatencySum::default())
    }
}
