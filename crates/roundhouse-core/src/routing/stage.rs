// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which *tier* should serve this turn, from what the session's tools have
//! been doing.
//!
//! Five constants and two pure functions. There is no model call anywhere in
//! this file and there is not meant to be: the routing question is asked of
//! code, once, and the judge that reads prose lives on the other seam entirely
//! (`validate`). That separation is the reason the scorers were refused a home
//! in the `Signal` seam and pointed here instead — see the attribution below.
//!
//! The output side is [`StagePolicy`], which maps a picked tier onto an
//! *ordered* list of admitted candidates: the first is served, the rest are the
//! decision's fallbacks. Admission runs first and the recipe can only narrow —
//! a tier entry the key does not admit is skipped, never resurrected.
//!
//! # Attribution
//!
//! Ported from NVIDIA Switchyard (Apache-2.0), rev
//! `053a61e2c43ba15f0772952ec3b3060c24b317f2`:
//! `crates/libsy/src/algorithms/util/stage.rs`.
//!
//! **Taken verbatim:** the five constants (`STALL_MIN_TURN_DEPTH`,
//! `SCORE_GAIN`, `HARD_SEVERITY`, `SIGNAL_UNIT`, `SEVERITY_CRITICAL`), the
//! `tanh` scoring arithmetic and its operator reading, `dimensions_from_signal`
//! including the mutual exclusion of `spinning` and `exploring`, the
//! escalate-before-de-escalate order, the `Capable`/`Efficient` axis with its
//! `strong`/`weak` reported labels, `PickerMode` and its default tier, and the
//! construction-time refusal of a `confidence_threshold` outside `0.0..=1.0`.
//!
//! **The thresholds are SWE-Bench-Pro-Python-75-calibrated** and upstream says
//! so; they do not transfer across model pairs or domains. `capable_first`
//! carries a further caveat that is upstream's own: *every* published Switchyard
//! number is `efficient_first`, and their server warns at startup when a route
//! selects the other mode. [`TierRecipe::uncalibrated_warning`] is that warning,
//! kept because a borrowed calibration quoted for a shape nobody measured is
//! the one dishonest thing a port can do.
//!
//! **Deliberately different, each documented at the divergence:**
//!
//! - `compacted` — upstream's other half of the hard override. It has no input
//!   here: the [`ToolSignals`] port ruled it dead three ways (no text to scan,
//!   a Claude Code marker rather than codex's, and a compacted conversation
//!   forks onto a fresh session so nothing latches). The override is therefore
//!   severity-only. See [`should_escalate`].
//! - `turn_depth` — upstream counts *messages* and warns in its own doc that the
//!   count is "wire-format dependent … approximate across request origins".
//!   [`TurnSignals::turn_depth`] is a count of *exchanges*, which is the honest
//!   roundhouse quantity and the one every signal on the validate seam already
//!   has.
//! - `PickOutcome::ConsultClassifier` — upstream's fall-open arm hands the turn
//!   to an LLM classifier further down a cascade. There is no cascade here and
//!   there is no classifier: an undecided turn resolves to the picker's default
//!   tier with [`DecisionSource::Ambiguous`], which is what upstream's
//!   `DefaultTarget` terminal stage does anyway when no classifier is
//!   configured. Folding the arm away is what makes "routing makes no model
//!   calls, ever" a property of the type rather than a rule someone has to obey.
//!
//! Both trees carry the same `SPDX-FileCopyrightText` line and the same licence,
//! so what is owed is provenance and revision. The revision is the half that
//! rots, which is why a test below pins it against the file rather than against
//! a constant it could be deleted with.

use async_trait::async_trait;

use crate::routing::{
    Admitted, Candidate, Decision, RoutingContext, RoutingError, RoutingPolicy, Target,
};
use crate::validate::tool_signals::ToolSignals;

// ─── the constants, verbatim ─────────────────────────────────────────────────

/// Below this many exchanges, a turn that produced no writes is ordinary
/// exploration rather than a stall.
const STALL_MIN_TURN_DEPTH: u32 = 8;

/// `tanh` gain. Without it confidence caps near ±0.20 and no threshold worth
/// writing down could ever be crossed.
const SCORE_GAIN: f64 = 5.0;

/// Normalises severity onto one signal unit: a [`HARD`](crate::validate::tool_signals::HARD)
/// result contributes exactly as much as a maxed boolean signal does.
const HARD_SEVERITY: f64 = 0.7;

/// One maxed signal's weight.
const SIGNAL_UNIT: f64 = 0.10;

/// Severity at or above which the scorer is not consulted at all.
const SEVERITY_CRITICAL: f32 = 1.0;

/// Upstream's shipped operating point, and the one every published Switchyard
/// number was measured at.
///
/// The doc reading that comes with it: `~0.3` escalates on one signal, `~0.5`
/// needs about one-and-a-half, `~0.7` needs two to corroborate.
pub const DEFAULT_CONFIDENCE_THRESHOLD: f64 = 0.5;

// ─── the vocabulary ──────────────────────────────────────────────────────────

/// The two tiers a recipe names.
///
/// Two, not three, and that is upstream's shape rather than a simplification of
/// it: the axis is "is this turn worth the capable model", which has one answer
/// and its negation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Efficient,
    Capable,
}

impl Tier {
    /// The stable label a decision reports.
    ///
    /// Upstream's argument for the second vocabulary, kept because it is the
    /// same one here: the label is independent of what a deployment happens to
    /// call the models in each tier, so a rationale grepped across two
    /// deployments reads the same way.
    pub fn label(self) -> &'static str {
        match self {
            Tier::Capable => "strong",
            Tier::Efficient => "weak",
        }
    }

    /// The other one.
    pub fn other(self) -> Tier {
        match self {
            Tier::Capable => Tier::Efficient,
            Tier::Efficient => Tier::Capable,
        }
    }
}

/// Where a turn lands when the signals do not decide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickerMode {
    /// Serve the cheap tier unless something says otherwise. Every published
    /// Switchyard operating point is this one.
    #[default]
    EfficientFirst,
    /// Serve the capable tier unless something says otherwise.
    ///
    /// **Uncalibrated upstream and uncalibrated here.** See
    /// [`TierRecipe::uncalibrated_warning`].
    CapableFirst,
}

impl PickerMode {
    pub fn default_tier(self) -> Tier {
        match self {
            PickerMode::EfficientFirst => Tier::Efficient,
            PickerMode::CapableFirst => Tier::Capable,
        }
    }
}

/// What decided a tier.
///
/// Carried on the [`Decision`] as a typed value rather than left inside the
/// rationale prose, because a consumer that has to decide whether to *narrate*
/// an escalation must not be parsing English to do it. The handoff note gates
/// on exactly this: a signal-driven change ([`Self::Override`],
/// [`Self::Dimensions`]) is worth telling the capable model about, and a
/// fall-open ([`Self::Ambiguous`]) is not — narrating one would tell a model the
/// cheap tier had been stalling on a turn where nothing said it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    /// The hard escalate: a critical result. Not scored.
    Override,
    /// The hard de-escalate: tests passed on a turn that produced work.
    TestsPassed,
    /// The scorer, above the configured confidence threshold.
    Dimensions,
    /// Nothing was decisive, so the picker's default tier took the turn.
    Ambiguous,
}

impl DecisionSource {
    pub fn label(self) -> &'static str {
        match self {
            DecisionSource::Override => "override",
            DecisionSource::TestsPassed => "tests_passed",
            DecisionSource::Dimensions => "dimensions",
            DecisionSource::Ambiguous => "ambiguous",
        }
    }

    /// Whether a tier change decided this way is worth narrating to the model
    /// that inherits the turn.
    ///
    /// Upstream's `only_on_wrong_signal_escalation` rule, in one place so the
    /// two callers that need it cannot disagree: a note may claim the previous
    /// model was in trouble only when a *signal* said so.
    pub fn is_signal_driven(self) -> bool {
        matches!(self, DecisionSource::Override | DecisionSource::Dimensions)
    }
}

/// The signals one turn is scored on.
///
/// [`ToolSignals`] plus the one field the port refused, computed the way this
/// tree can honestly compute it. Bundled rather than passed as two arguments so
/// that a caller cannot pair one session's tool traffic with another's depth.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnSignals {
    pub tools: ToolSignals,
    /// How many *task* exchanges this session holds — roundhouse's own
    /// control calls dropped, exactly as [`task_exchanges_on`] drops them from
    /// every other window this file reads.
    ///
    /// Upstream's `turn_depth` is `messages.len()`; this is the task-exchange
    /// count. See the module attribution for why the substitution is the
    /// honest one and not merely the convenient one.
    ///
    /// **Counted after the control-call drop, not before (M18, H5).** Before
    /// this, `exchanges.len()` counted every exchange including our own —
    /// three reads of roundhouse's own `status` tool (the generated
    /// `rh-status` skill's own advice to an agent) opened the stall gate on a
    /// session with as few as five lines of real task work, exactly the depth
    /// [`STALL_MIN_TURN_DEPTH`] the tool-signal window itself already treats
    /// those three calls as absent from. A depth counted one way and a window
    /// counted another is how a shallow session reads as a stall.
    ///
    /// [`task_exchanges_on`]: crate::validate::task_exchanges_on
    pub turn_depth: u32,
}

impl TurnSignals {
    /// The signals over a session's exchanges, as `dialect`'s client wrote them.
    pub fn from_exchanges(
        exchanges: &[crate::validate::Exchange],
        dialect: crate::validate::ControlCallDialect,
    ) -> Self {
        let task_exchanges = crate::validate::task_exchanges_on(exchanges, dialect);
        Self {
            tools: ToolSignals::from_exchanges(exchanges, dialect),
            turn_depth: task_exchanges.len() as u32,
        }
    }
}

/// The four axes the scorer sums.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodingAgentDimensions {
    /// Worst recent tool-result severity, `0.0..=1.0`.
    pub severity: f64,
    /// Deep in, producing nothing, and not investigating either. `1.0` or `0.0`.
    pub spinning: f64,
    /// Deep in, producing nothing, but reading and planning. `1.0` or `0.0`.
    pub exploring: f64,
    /// Fraction of recent operations that wrote or edited. Pulls *down*.
    pub production_intensity: f64,
}

/// A signed score and the confidence that is its magnitude.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreResult {
    /// `(-1.0, 1.0)`. Positive means capable, negative means efficient.
    pub score: f64,
    /// `score.abs()`.
    pub confidence: f64,
}

/// What [`pick_tier`] answered, and why.
///
/// Always resolved — see the module attribution on the missing
/// `ConsultClassifier` arm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pick {
    pub tier: Tier,
    pub source: DecisionSource,
    /// The scorer's signed score, `0.0` when a hard rule decided instead.
    pub score: f64,
    /// The scorer's confidence, or `None` when a hard rule decided.
    ///
    /// `Option` rather than a `0.0` or a `1.0` that could be mistaken for a
    /// measurement: a hard override is not the scorer being very sure, it is
    /// the scorer not having been asked. Upstream draws the same distinction
    /// and this is the one place it is worth keeping.
    pub confidence: Option<f64>,
}

// ─── the arithmetic ──────────────────────────────────────────────────────────

/// Project the signals onto the four axes.
///
/// `spinning` and `exploring` are mutually exclusive by construction, which is
/// upstream's note and worth keeping: at most one fires, so there is no
/// double-counting on the production axis.
pub fn dimensions_from_signal(signals: &TurnSignals) -> CodingAgentDimensions {
    let tools = &signals.tools;
    let recent_ops = tools.recent_write_count
        + tools.recent_edit_count
        + tools.recent_read_count
        + tools.recent_todowrite_count;
    let deep_enough = signals.turn_depth >= STALL_MIN_TURN_DEPTH;
    let no_production = tools.recent_write_count == 0 && tools.recent_edit_count == 0;
    let investigating = tools.recent_read_count >= 1 || tools.recent_todowrite_count >= 1;

    CodingAgentDimensions {
        severity: tools.severity as f64,
        spinning: f64::from(deep_enough && no_production && !investigating),
        exploring: f64::from(deep_enough && no_production && investigating),
        production_intensity: match recent_ops {
            0 => 0.0,
            total => {
                f64::from(tools.recent_write_count + tools.recent_edit_count) / f64::from(total)
            }
        },
    }
}

/// One line of arithmetic, and the whole of the scorer.
///
/// `tanh` rather than a clamp so that the second corroborating signal moves the
/// number much less than the first did — which is what makes a threshold read
/// as "how many signals do I want" rather than as an arbitrary cut.
pub fn score_signal(dimensions: &CodingAgentDimensions) -> ScoreResult {
    let raw = SIGNAL_UNIT
        * (dimensions.severity / HARD_SEVERITY + dimensions.spinning + dimensions.exploring
            - dimensions.production_intensity);
    let score = (SCORE_GAIN * raw).tanh();
    ScoreResult {
        score,
        confidence: score.abs(),
    }
}

/// The hard escalate.
///
/// **Severity only.** Upstream fires this on `compacted || severity >= 1.0`;
/// `compacted` has no input in this tree — the [`ToolSignals`] port ruled it
/// dead three ways and the module attribution names them — so porting the
/// disjunction would have been porting a term that is permanently `false`,
/// which reads to the next person as a feature that is broken rather than as
/// one that was never wired.
fn should_escalate(signals: &TurnSignals) -> bool {
    signals.tools.severity >= SEVERITY_CRITICAL
}

/// The hard de-escalate: tests passed on a turn that actually produced work,
/// with nothing broken in the recent window.
fn should_deescalate(signals: &TurnSignals) -> bool {
    let tools = &signals.tools;
    tools.tests_passed
        && (tools.recent_write_count + tools.recent_edit_count) >= 1
        && tools.severity <= 0.0
}

/// Four ordered rules. Pure, sync, deterministic — which is what makes the
/// whole thing portable and testable without a fixture.
///
/// **Escalate is checked before de-escalate on purpose**, and it is upstream's
/// purpose: a critical error still wins on a turn whose tests also happened to
/// pass. At the thresholds this tree ships (`should_escalate` needs
/// `severity >= 1.0`, `should_deescalate` needs `severity <= 0.0`), the two
/// guards cannot both be true for any `severity`, so today the *order*
/// between them is dead code — no fixture can make it live without widening
/// one of those thresholds (M10.2 refute finding 1). It stays first anyway,
/// matching upstream, for the day either threshold moves and the two guards
/// start to overlap.
pub fn pick_tier(signals: &TurnSignals, mode: PickerMode, confidence_threshold: f64) -> Pick {
    if should_escalate(signals) {
        return Pick {
            tier: Tier::Capable,
            source: DecisionSource::Override,
            score: 0.0,
            confidence: None,
        };
    }
    if should_deescalate(signals) {
        return Pick {
            tier: Tier::Efficient,
            source: DecisionSource::TestsPassed,
            score: 0.0,
            confidence: None,
        };
    }
    let scored = score_signal(&dimensions_from_signal(signals));
    if scored.confidence >= confidence_threshold {
        return Pick {
            tier: match scored.score > 0.0 {
                true => Tier::Capable,
                false => Tier::Efficient,
            },
            source: DecisionSource::Dimensions,
            score: scored.score,
            confidence: Some(scored.confidence),
        };
    }
    // Upstream consults a classifier here. This tree does not: the turn lands
    // on the picker's default tier, which is where upstream's own terminal
    // `DefaultTarget` stage puts it when no classifier is configured.
    Pick {
        tier: mode.default_tier(),
        source: DecisionSource::Ambiguous,
        score: scored.score,
        confidence: Some(scored.confidence),
    }
}

// ─── the recipe ──────────────────────────────────────────────────────────────

/// Why a tier recipe was refused at load.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TierRecipeError {
    #[error(
        "`confidence_threshold` is {threshold}, which is outside 0.0..=1.0; a threshold no \
         confidence can reach makes every turn ambiguous and a threshold every confidence \
         clears makes the scorer unconditional"
    )]
    ThresholdOutOfRange { threshold: f64 },
    #[error(
        "a tier recipe names no targets in either tier, so there is nothing for the scorer to \
         pick between"
    )]
    Empty,
    #[error(
        "`{target}` is named twice in this recipe; a repeated entry becomes a failover onto the \
         target that just failed, which is a retry wearing a fallback's clothes -- same-target \
         retry belongs in the transport, where a backoff can bound it"
    )]
    RepeatedTarget { target: String },
}

/// Two ordered candidate lists and how to choose between them.
///
/// Targets are named the way every other operator-facing filter in this tree
/// names them — [`Target::policy_identity`], i.e. `provider/model` for a hosted
/// model and `local/model` for one of our own workers — so a recipe and an
/// `allow` list are sentences in the same language.
#[derive(Debug, Clone, PartialEq)]
pub struct TierRecipe {
    capable: Vec<String>,
    efficient: Vec<String>,
    picker: PickerMode,
    confidence_threshold: f64,
}

impl TierRecipe {
    /// **Validated at construction, not at first request.** Upstream refuses an
    /// out-of-range threshold as an `AlgorithmError` when the route is built,
    /// and the reason to mirror that is the reason it is worth mirroring
    /// anywhere: a deployment finds out at boot, when an operator is watching,
    /// rather than on a turn, when a client is.
    pub fn new(
        capable: Vec<String>,
        efficient: Vec<String>,
        picker: PickerMode,
        confidence_threshold: f64,
    ) -> Result<Self, TierRecipeError> {
        if !(0.0..=1.0).contains(&confidence_threshold) {
            return Err(TierRecipeError::ThresholdOutOfRange {
                threshold: confidence_threshold,
            });
        }
        if capable.is_empty() && efficient.is_empty() {
            return Err(TierRecipeError::Empty);
        }
        // Refused rather than deduplicated, and across *both* tiers rather than
        // within each: a repeat inside one tier becomes a second dispatch to the
        // target that just failed, and a name in both tiers makes the scorer's
        // choice a no-op that reads, in the file, like a decision. Silently
        // dropping either would leave an operator with a recipe that does not
        // do what it says.
        let mut seen = std::collections::BTreeSet::new();
        for named in capable.iter().chain(efficient.iter()) {
            if !seen.insert(named) {
                return Err(TierRecipeError::RepeatedTarget {
                    target: named.clone(),
                });
            }
        }
        Ok(Self {
            capable,
            efficient,
            picker,
            confidence_threshold,
        })
    }

    pub fn picker(&self) -> PickerMode {
        self.picker
    }

    pub fn confidence_threshold(&self) -> f64 {
        self.confidence_threshold
    }

    /// The ordered target names for one tier.
    pub fn list(&self, tier: Tier) -> &[String] {
        match tier {
            Tier::Capable => &self.capable,
            Tier::Efficient => &self.efficient,
        }
    }

    /// Every target this recipe names, capable tier first.
    ///
    /// The tier-blind question — *could this recipe select that target at
    /// all?* — which is what the boot cross-checks ask (`crosscheck.rs`:
    /// a recipe that names no local target cannot keep a degrade-to-local
    /// promise, and one that names a model no catalog holds is a typo the
    /// deployment should refuse over). Answered here rather than by two call
    /// sites chaining `list(Capable)` and `list(Efficient)` themselves,
    /// because a third tier — if one is ever added — would otherwise be
    /// invisible to checks written before it existed.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.capable
            .iter()
            .chain(self.efficient.iter())
            .map(String::as_str)
    }

    /// The startup warning a `capable_first` recipe earns, or `None`.
    ///
    /// Upstream's server prints its equivalent and this one says the same
    /// thing, because the honest version of "we ported a calibrated recipe" has
    /// to include which shape the calibration does *not* cover.
    pub fn uncalibrated_warning(&self) -> Option<&'static str> {
        match self.picker {
            PickerMode::CapableFirst => Some(
                "this recipe selects `capable_first`, for which no published Switchyard \
                 operating point exists: every measured number upstream is `efficient_first`. \
                 The thresholds here are SWE-Bench-Pro-Python-75 calibrations and do not \
                 transfer across model pairs or domains -- treat a capable-first deployment as \
                 an experiment, not a tuned configuration",
            ),
            PickerMode::EfficientFirst => None,
        }
    }
}

// ─── the policy ──────────────────────────────────────────────────────────────

/// Route by tier where a project configured one, and exactly as before where it
/// did not.
///
/// **Wraps an inner policy rather than replacing it**, because a recipe is a
/// per-project fact and the engine holds one policy object. A project with no
/// recipe reaches `inner` with the same context and produces a byte-identical
/// target and rationale — pinned by
/// `a_project_without_a_recipe_routes_byte_for_byte_as_its_inner_policy`.
///
/// **[`Self::name`] reports `stage` even on that path, and that is deliberate.**
/// The alternative — reporting the inner name — would make the audit trail say
/// `affinity` for turns a stage router served, which is the worse of the two
/// inaccuracies. The field names the object in force, and a deployment only
/// composes this one when some project has a recipe; the boot wiring is what
/// keeps that true, and it is stated here so the next reader does not compose
/// it unconditionally and quietly re-label every existing deployment's log.
pub struct StagePolicy {
    inner: Box<dyn RoutingPolicy>,
}

impl StagePolicy {
    pub fn new(inner: Box<dyn RoutingPolicy>) -> Self {
        Self { inner }
    }

    /// The admitted candidates a tier names, in the recipe's order.
    ///
    /// **The narrow-only rule, and it is one line: admission has already run.**
    /// This walks the recipe and keeps what the pool holds, so a listed target
    /// the key does not admit is skipped and there is no code path that could
    /// put it back. Walking the recipe rather than the pool is what makes the
    /// *order* the operator's rather than the quoter's.
    fn tier_pool<'a>(
        recipe: &TierRecipe,
        tier: Tier,
        pool: &[&'a Candidate],
    ) -> Vec<&'a Candidate> {
        recipe
            .list(tier)
            .iter()
            .filter_map(|named| {
                pool.iter()
                    .copied()
                    .find(|candidate| &candidate.target.policy_identity() == named)
            })
            .collect()
    }

    /// What a turn does when admission left capacity the recipe does not name.
    ///
    /// **Degrade-to-local outranks a recipe, and that is the whole of this
    /// function.** [`RoutingContext::admissible`] applies the cadence and the
    /// budget in its first filters, and both are frontier-only knobs — so a
    /// spent window or an exhausted `degrade_to_local` budget leaves precisely
    /// the local candidates admitted, which is the state two configuration
    /// surfaces promise in words ("the hosted options go inadmissible and the
    /// turn serves locally instead of failing"). A recipe is a preference
    /// among what admission already entitles and never a second gate on it —
    /// [`Self::tier_pool`] states the narrowing half of the same rule — so a
    /// recipe that happens to name no local target must not turn a documented
    /// degradation into a client-visible failure. M10 review G02.
    ///
    /// **Narrower than "fall back to `inner`" on purpose.** Handing the whole
    /// admitted pool to the inner policy would also let an unnamed *hosted*
    /// candidate take the turn, which is the one reading of a recipe an
    /// operator would not expect: they wrote down which hosted models may
    /// answer. The promise that survives a recipe is the local one, because it
    /// is the only one the configuration file makes.
    ///
    /// [`Admitted::decide`] rather than `decide_staged`: no tier served this
    /// turn, so `source` is honestly `None` and the handoff gate
    /// (`engine::opened_a_tier_escalation`) reads it as "no tier decision" —
    /// stamping a [`DecisionSource`] here would tell that gate a scorer's pick
    /// had been honoured when it was bypassed.
    fn degrade_past_the_recipe(
        recipe: &TierRecipe,
        admitted: &Admitted<'_>,
    ) -> Result<Decision, RoutingError> {
        let Some(degrade) = admitted
            .pool()
            .iter()
            .copied()
            .find(|candidate| candidate.target.is_local())
        else {
            // The recipe named targets, the pool holds none of them, and there
            // is no local capacity to degrade onto either. The error taxonomy
            // is deliberately not extended for this -- three empty-set arms
            // already send an operator to three different files, and a fourth
            // would be a fifth thing to learn -- so the recipe fact goes where
            // an operator can find it without a new variant.
            tracing::warn!(
                capable = ?recipe.list(Tier::Capable),
                efficient = ?recipe.list(Tier::Efficient),
                "no admitted candidate matches any target this project's tier recipe names, and \
                 no local worker is admitted either; the turn fails as an unviable candidate set, \
                 but the recipe is what to look at"
            );
            return Err(admitted.refuse_no_viable());
        };
        Ok(admitted.decide(
            degrade.target.clone(),
            // **No price in this string**, the same rule the staged rationale
            // below states at length: a rationale is republished into the
            // calling model's own context by `explain_last_route`.
            format!(
                "stage router: no target this project's tier recipe names is admissible on this \
                 turn, so the turn degrades to {} -- a spent allowance promises local service and \
                 a recipe does not override it",
                degrade.target.policy_identity()
            ),
        ))
    }
}

#[async_trait]
impl RoutingPolicy for StagePolicy {
    fn name(&self) -> &str {
        "stage"
    }

    /// The one policy that does. See [`RoutingPolicy::reads_tier_recipes`] for
    /// what the engine does with the answer.
    fn reads_tier_recipes(&self) -> bool {
        true
    }

    async fn choose(&self, ctx: &RoutingContext<'_>) -> Result<Decision, RoutingError> {
        let Some(recipe) = ctx.tiers else {
            return self.inner.choose(ctx).await;
        };
        if ctx.candidates.is_empty() {
            return Err(RoutingError::NoCandidates);
        }
        // `None` for `max_load`, like the escalation audit branch and for the
        // same reason: a recipe is an operator's statement about which model
        // should answer, and a busy local worker is not a reason to overrule
        // it. Everything else about admissibility -- the blame when the set
        // empties, and the overflow valve -- is the shared code every policy
        // reaches.
        let admitted = ctx.admissible(None)?;
        let pool = admitted.pool();

        // Absent signals are the first turn of a session, and they score to the
        // picker default through the ordinary arithmetic rather than through a
        // special case: an empty `ToolSignals` has no severity, no counts and
        // no depth, so the scorer returns zero and the fall-open takes it.
        let signals = ctx.signals.cloned().unwrap_or_default();
        let pick = pick_tier(&signals, recipe.picker(), recipe.confidence_threshold());

        let picked = Self::tier_pool(recipe, pick.tier, pool);
        let (serving, ordered) = match picked.is_empty() {
            false => (pick.tier, picked),
            true => {
                let other = Self::tier_pool(recipe, pick.tier.other(), pool);
                match other.is_empty() {
                    false => (pick.tier.other(), other),
                    // The recipe named targets and the pool holds none of them.
                    // Whether that is a failure depends on what admission left.
                    true => return Self::degrade_past_the_recipe(recipe, &admitted),
                }
            }
        };

        let winner = ordered[0];
        let fallbacks: Vec<Target> = ordered[1..]
            .iter()
            .map(|candidate| candidate.target.clone())
            .collect();

        // **No price in this string**, the same rule `AffinityPolicy::choose`
        // states at length: a rationale is republished into the calling model's
        // own context by `explain_last_route`, so it carries names and never
        // dollars.
        let mut rationale = format!(
            "stage router: {} tier ({}) by {}",
            serving.label(),
            winner.target.policy_identity(),
            pick.source.label(),
        );
        if let Some(confidence) = pick.confidence {
            rationale.push_str(&format!(
                "; score {:+.4}, confidence {:.4} against threshold {:.2}",
                pick.score,
                confidence,
                recipe.confidence_threshold()
            ));
        }
        if serving != pick.tier {
            // **"this turn", not "this key", and the edit is the whole of G09
            // at this seam.** An empty tier has four possible causes and this
            // policy can tell them apart from none of them: the key's own
            // filter, a spent cadence or budget, a credential that reaches no
            // provider — and a recipe entry naming a model this deployment does
            // not serve at all. Blaming the key by name sent an operator with a
            // transposed digit in a model id off to widen an `allow` list that
            // was never the problem, and the sentence is republished to the
            // calling model by `explain_last_route`, so it was wrong in two
            // places at once.
            //
            // The narrower sentence is not recoverable here: `ctx.candidates`
            // has already been filtered by `TurnPolicy::permits` and by the
            // credential filter before the router sees it (engine.rs), so a
            // name missing from it is a typo and a policy exclusion wearing the
            // same clothes. The typo is caught where both files are loaded --
            // `crosscheck::refuse_tier_recipes_naming_absent_targets` refuses
            // it at boot and at every admin write -- which leaves this string
            // saying only what a router honestly knows: nothing admissible on
            // this turn carries an identity the picked tier names.
            rationale.push_str(&format!(
                "; the {} tier was picked and this turn admits none of it",
                pick.tier.label()
            ));
        }
        if !fallbacks.is_empty() {
            rationale.push_str(&format!(
                "; {} fallback(s) in the same tier",
                fallbacks.len()
            ));
        }

        Ok(admitted.decide_staged(winner.target.clone(), fallbacks, pick.source, rationale))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{FrontierCadence, FrontierHistory, TargetFilter, TurnBudget, TurnPolicy};
    use crate::ids::SessionId;
    use crate::routing::CacheLedger;
    use crate::routing::policy::AffinityPolicy;
    use crate::validate::ControlCallDialect;

    /// The module's own source, so the attribution test reads what a reader
    /// would read rather than a constant that could be deleted with it.
    const SOURCE: &str = include_str!("stage.rs");

    /// [`SOURCE`] up to (not including) `mod tests`.
    ///
    /// The whole-file version is a tautology — this module's own assertions
    /// retype the rev and the path, so `SOURCE.contains(...)` finds its own
    /// literal however the doc comment above was mutated. `tool_signals.rs`
    /// learned that the expensive way; the slice is the fix.
    fn doc_and_code() -> &'static str {
        SOURCE.split("\n#[cfg(test)]").next().unwrap()
    }

    #[test]
    fn the_attribution_names_the_source_the_revision_and_every_divergence() {
        let doc_and_code = doc_and_code();
        assert!(doc_and_code.contains("NVIDIA Switchyard"));
        assert!(doc_and_code.contains("Apache-2.0"));
        assert!(doc_and_code.contains("053a61e2c43ba15f0772952ec3b3060c24b317f2"));
        assert!(doc_and_code.contains("crates/libsy/src/algorithms/util/stage.rs"));
        assert!(
            doc_and_code.contains("SWE-Bench-Pro-Python-75"),
            "the calibration the thresholds came from is half of what makes \
             quoting them honest"
        );
        for divergence in [
            "compacted",
            "turn_depth",
            "ConsultClassifier",
            "capable_first",
        ] {
            assert!(
                doc_and_code.contains(divergence),
                "the attribution has to name `{divergence}` as a divergence, or \
                 the next reader reads a transcription that is not one"
            );
        }
    }

    /// Upstream's `one_signal_scores_below_half`, re-derived rather than copied.
    ///
    /// This one assertion discriminates a mis-transcribed `SCORE_GAIN` *or* a
    /// mis-transcribed `HARD_SEVERITY` divisor, which is why it is the first
    /// test in the file: `tanh(5.0 * 0.10 * 1.0) = tanh(0.5) ≈ 0.4621`, and
    /// upstream chose the constants so that one maxed signal lands deliberately
    /// just *under* the 0.5 threshold. Two corroborating signals reach
    /// `tanh(1.0) ≈ 0.7616`.
    #[test]
    fn one_signal_scores_below_half_and_two_corroborate() {
        let one_severity = score_signal(&CodingAgentDimensions {
            severity: 0.7,
            spinning: 0.0,
            exploring: 0.0,
            production_intensity: 0.0,
        });
        let one_spinning = score_signal(&CodingAgentDimensions {
            severity: 0.0,
            spinning: 1.0,
            exploring: 0.0,
            production_intensity: 0.0,
        });
        for (label, scored) in [("severity", one_severity), ("spinning", one_spinning)] {
            assert!(
                (scored.score - 0.5f64.tanh()).abs() < 1e-9,
                "{label}: one maxed signal must score tanh(0.5), got {}",
                scored.score
            );
            assert!(
                scored.confidence < DEFAULT_CONFIDENCE_THRESHOLD,
                "{label}: and it must land under the shipped threshold, which is \
                 the whole of upstream's calibration argument"
            );
        }

        let two = score_signal(&CodingAgentDimensions {
            severity: 0.7,
            spinning: 1.0,
            exploring: 0.0,
            production_intensity: 0.0,
        });
        assert!((two.score - 1.0f64.tanh()).abs() < 1e-9);
        assert!(two.confidence > DEFAULT_CONFIDENCE_THRESHOLD);

        // And the sign is the axis: production pulls the other way.
        let producing = score_signal(&CodingAgentDimensions {
            severity: 0.0,
            spinning: 0.0,
            exploring: 0.0,
            production_intensity: 1.0,
        });
        assert!(producing.score < 0.0);
        assert!((producing.score + 0.5f64.tanh()).abs() < 1e-9);
    }

    /// G04 (review finding, now fixed): the trailing window
    /// `dimensions_from_signal` reads from is [`ToolSignals`]'s own `recent_*`
    /// counts, and those used to include `mcp__roundhouse__*` as uncategorised
    /// `Other` traffic — so a session that made six productive edits and then
    /// followed the `rh-status` skill (`status`, `explain_last_route`,
    /// `prefer`, `set_quality_floor`) read as `spinning` on the identity of
    /// its last three calls rather than on the agent being stuck.
    ///
    /// **The depth is load-bearing and a four-exchange fixture would make this
    /// vacuous**: at `turn_depth < STALL_MIN_TURN_DEPTH` nothing fires anyway
    /// and the assertion would have passed before the fix. Since M18 (H5)
    /// `turn_depth` itself drops the trailing control calls, so it takes eight
    /// real exchanges — not six — to clear `STALL_MIN_TURN_DEPTH` on their own
    /// and keep this fixture exercising the gate rather than falling short of
    /// it before the identity question is even asked. The production
    /// assertion is the other half: without it, "spinning is zero" would also
    /// be satisfied by a fix that zeroed every count instead of dropping our
    /// own calls from the walk.
    #[test]
    fn our_own_control_calls_do_not_synthesize_a_stall() {
        use crate::validate::Exchange;

        fn control_call(id: &str, name: &str, arguments: &str) -> Exchange {
            Exchange {
                call_id: id.into(),
                name: name.into(),
                // The flat spelling this fixture uses *is* the Messages
                // surface's namespace, so there is no field to fill (M17).
                namespace: None,
                arguments: arguments.into(),
                output: Some("ok".into()),
                failed: false,
            }
        }

        // Eight exchanges of real production work -- enough on their own to
        // clear `STALL_MIN_TURN_DEPTH` (8) once `turn_depth` counts only task
        // exchanges (M18, H5) -- then the four trailing control calls the
        // generated `rh-status` skill tells an agent to make. Twelve
        // exchanges total; `turn_depth` must stay at eight.
        let mut exchanges = Vec::new();
        for n in 0..8 {
            exchanges.push(control_call(&format!("p{n}"), "edit", "{}"));
        }
        exchanges.push(control_call("c0", "mcp__roundhouse__status", "{}"));
        exchanges.push(control_call(
            "c1",
            "mcp__roundhouse__explain_last_route",
            "{}",
        ));
        exchanges.push(control_call(
            "c2",
            "mcp__roundhouse__prefer",
            r#"{"mode":"cheap"}"#,
        ));
        exchanges.push(control_call(
            "c3",
            "mcp__roundhouse__set_quality_floor",
            r#"{"floor":0.5}"#,
        ));
        assert_eq!(exchanges.len(), 12);

        let signals = TurnSignals::from_exchanges(&exchanges, ControlCallDialect::ClaudeMessages);
        assert_eq!(
            signals.turn_depth, 8,
            "H5: the four trailing control calls must not inflate `turn_depth` past the real \
             task-exchange count"
        );
        let dims = dimensions_from_signal(&signals);
        assert_eq!(
            dims.spinning, 0.0,
            "four trailing reads of roundhouse's own control surface must not \
             read as the agent spinning: {dims:?}"
        );
        assert!(
            dims.production_intensity > 0.0,
            "and they must not zero the production the agent actually did: {dims:?}"
        );

        // The half of limb C this fix used to leave open, now closed (M18,
        // H5): `turn_depth` is `task_exchanges_on(exchanges, dialect).len()`
        // (below), counted *after* the control calls are dropped, so our own
        // surface no longer supplies the depth that opens the stall gate.
        // Five uncategorised `cargo` calls are a build loop too shallow to be
        // a stall; three reads of our own status tool must not make the same
        // session read as deep enough, because those three are dropped from
        // the window behind the gate already and the depth that gates it has
        // to agree.
        let shallow_pit: Vec<Exchange> = (0..5)
            .map(|n| control_call(&format!("b{n}"), "cargo", "{}"))
            .collect();
        let alone = TurnSignals::from_exchanges(&shallow_pit, ControlCallDialect::ClaudeMessages);
        assert_eq!(
            (alone.turn_depth, dimensions_from_signal(&alone).spinning),
            (5, 0.0),
            "five uncategorised calls are not deep enough to be a stall on their own"
        );

        let mut with_control = shallow_pit;
        with_control.extend(
            (0..3).map(|n| control_call(&format!("c{n}"), "mcp__roundhouse__status", "{}")),
        );
        let inflated =
            TurnSignals::from_exchanges(&with_control, ControlCallDialect::ClaudeMessages);
        let dims = dimensions_from_signal(&inflated);
        assert_eq!(
            (inflated.turn_depth, dims.spinning),
            (5, 0.0),
            "H5: three reads of our own control surface must not supply the depth that calls \
             the same session a stall -- they are dropped from `turn_depth` exactly as they \
             are already dropped from the tool-signal window behind the gate: {dims:?}"
        );
    }

    /// H5, second dialect: the same fix over `CodexResponses`, whose
    /// recogniser reads `namespace` rather than the flat name (M17, R-N9) --
    /// the two dialects tell a control call apart differently, and a fix
    /// pinned on one alone would leave the other supplying the old, inflated
    /// depth.
    #[test]
    fn our_own_control_calls_do_not_synthesize_a_stall_on_codex_responses() {
        use crate::validate::Exchange;

        fn task_call(id: &str) -> Exchange {
            Exchange {
                call_id: id.into(),
                name: "edit".into(),
                namespace: None,
                arguments: "{}".into(),
                output: Some("ok".into()),
                failed: false,
            }
        }

        fn control_call(id: &str, name: &str) -> Exchange {
            Exchange {
                call_id: id.into(),
                name: name.into(),
                // CodexResponses recognises a control call by the namespace
                // field, not the flat name (M17, R-N9).
                namespace: Some("mcp__roundhouse".into()),
                arguments: "{}".into(),
                output: Some("ok".into()),
                failed: false,
            }
        }

        let shallow_pit: Vec<Exchange> = (0..5).map(|n| task_call(&format!("b{n}"))).collect();
        let mut with_control = shallow_pit;
        with_control.extend((0..3).map(|n| control_call(&format!("c{n}"), "status")));
        let inflated =
            TurnSignals::from_exchanges(&with_control, ControlCallDialect::CodexResponses);
        let dims = dimensions_from_signal(&inflated);
        assert_eq!(
            (inflated.turn_depth, dims.spinning),
            (5, 0.0),
            "the namespaced dialect must drop its own three control calls from `turn_depth` \
             the same way the flat dialect does: {dims:?}"
        );
    }

    #[test]
    fn spinning_and_exploring_never_both_fire() {
        // Deep enough, no production, nothing investigative: spinning.
        let spinning = dimensions_from_signal(&TurnSignals {
            tools: ToolSignals {
                ..Default::default()
            },
            turn_depth: STALL_MIN_TURN_DEPTH,
        });
        assert_eq!(spinning.spinning, 1.0);
        assert_eq!(spinning.exploring, 0.0);

        // Same depth, same absence of production, but reading: exploring.
        let exploring = dimensions_from_signal(&TurnSignals {
            tools: ToolSignals {
                recent_read_count: 2,
                ..Default::default()
            },
            turn_depth: STALL_MIN_TURN_DEPTH,
        });
        assert_eq!(exploring.spinning, 0.0);
        assert_eq!(exploring.exploring, 1.0);
        assert_eq!(
            exploring.production_intensity, 0.0,
            "two reads and nothing written is a production intensity of zero"
        );

        // The control: one exchange short of the gate, neither fires. A shallow
        // session that has written nothing is exploring a task, not stalling on
        // one, and that is the whole reason the depth gate exists.
        let shallow = dimensions_from_signal(&TurnSignals {
            tools: ToolSignals::default(),
            turn_depth: STALL_MIN_TURN_DEPTH - 1,
        });
        assert_eq!(shallow.spinning, 0.0);
        assert_eq!(shallow.exploring, 0.0);
    }

    #[test]
    fn production_intensity_is_the_written_share_of_recent_work() {
        let mixed = dimensions_from_signal(&TurnSignals {
            tools: ToolSignals {
                recent_write_count: 1,
                recent_edit_count: 1,
                recent_read_count: 2,
                ..Default::default()
            },
            turn_depth: 20,
        });
        assert!((mixed.production_intensity - 0.5).abs() < 1e-9);
        // No recent operations at all divides by zero unless somebody wrote the
        // guard, so the guard has a test.
        let idle = dimensions_from_signal(&TurnSignals::default());
        assert_eq!(idle.production_intensity, 0.0);
    }

    #[test]
    fn a_critical_result_escalates_even_when_tests_also_passed() {
        // NOT an ordering test: `severity: SEVERITY_CRITICAL` makes
        // `should_deescalate` false on its own (`severity <= 0.0` fails), so
        // only `should_escalate` was ever going to fire here, in either
        // check order. What this proves is narrower and still real —
        // `tests_passed` and a fresh write do not talk `pick_tier` out of a
        // hard escalate. See `pick_tier`'s own doc comment: at the shipped
        // thresholds the two guards can never both be true, so the checked-
        // escalate-first order has no fixture that can exercise it
        // (M10.2 refute finding 1).
        let both = TurnSignals {
            tools: ToolSignals {
                severity: SEVERITY_CRITICAL,
                tests_passed: true,
                recent_write_count: 1,
                ..Default::default()
            },
            turn_depth: 12,
        };
        let pick = pick_tier(
            &both,
            PickerMode::EfficientFirst,
            DEFAULT_CONFIDENCE_THRESHOLD,
        );
        assert_eq!(pick.tier, Tier::Capable);
        assert_eq!(pick.source, DecisionSource::Override);
        assert_eq!(
            pick.confidence, None,
            "a hard override is not the scorer being certain, it is the scorer \
             not having been asked"
        );

        // The control: drop the severity and the same signals de-escalate.
        let settled = TurnSignals {
            tools: ToolSignals {
                severity: 0.0,
                ..both.tools.clone()
            },
            ..both
        };
        let pick = pick_tier(
            &settled,
            PickerMode::EfficientFirst,
            DEFAULT_CONFIDENCE_THRESHOLD,
        );
        assert_eq!(pick.tier, Tier::Efficient);
        assert_eq!(pick.source, DecisionSource::TestsPassed);
    }

    #[test]
    fn an_undecided_turn_lands_on_the_pickers_default_and_says_so() {
        // A quiet session: nothing to escalate on, nothing to settle on, and a
        // score of zero. Upstream would consult a classifier; this tree lands on
        // the default tier and stamps the source `ambiguous`, which is what the
        // handoff-note gate reads.
        let quiet = TurnSignals::default();
        for (mode, expected) in [
            (PickerMode::EfficientFirst, Tier::Efficient),
            (PickerMode::CapableFirst, Tier::Capable),
        ] {
            let pick = pick_tier(&quiet, mode, DEFAULT_CONFIDENCE_THRESHOLD);
            assert_eq!(pick.tier, expected);
            assert_eq!(pick.source, DecisionSource::Ambiguous);
            assert!(
                !pick.source.is_signal_driven(),
                "a fall-open must never be narrated as an intervention"
            );
        }
    }

    #[test]
    fn a_threshold_outside_the_unit_interval_is_refused_at_construction() {
        for bad in [-0.1, 1.1, f64::NAN] {
            assert!(matches!(
                TierRecipe::new(
                    vec!["a/b".into()],
                    Vec::new(),
                    PickerMode::EfficientFirst,
                    bad
                ),
                Err(TierRecipeError::ThresholdOutOfRange { .. })
            ));
        }
        assert!(matches!(
            TierRecipe::new(
                Vec::new(),
                Vec::new(),
                PickerMode::EfficientFirst,
                DEFAULT_CONFIDENCE_THRESHOLD
            ),
            Err(TierRecipeError::Empty)
        ));
        assert!(
            TierRecipe::new(
                vec!["a/b".into()],
                Vec::new(),
                PickerMode::EfficientFirst,
                DEFAULT_CONFIDENCE_THRESHOLD
            )
            .is_ok()
        );
    }

    /// A target named twice is refused, in either tier and across both.
    ///
    /// Inside one tier it would make the failover dispatch a second time to the
    /// model that just failed — a same-target retry, which belongs in a
    /// transport where a backoff can bound it and not in a fallback list.
    /// Across both tiers it makes the scorer's choice a no-op that reads, in the
    /// file, like a decision.
    #[test]
    fn a_target_named_twice_is_refused_rather_than_deduplicated() {
        for (capable, efficient) in [
            (vec!["a/b".to_string(), "a/b".to_string()], Vec::new()),
            (Vec::new(), vec!["a/b".to_string(), "a/b".to_string()]),
            (vec!["a/b".to_string()], vec!["a/b".to_string()]),
        ] {
            match TierRecipe::new(
                capable,
                efficient,
                PickerMode::EfficientFirst,
                DEFAULT_CONFIDENCE_THRESHOLD,
            ) {
                Err(TierRecipeError::RepeatedTarget { target }) => assert_eq!(target, "a/b"),
                other => panic!("a repeated target has to be refused, got {other:?}"),
            }
        }

        // The control: two *different* targets in one tier is the ordinary
        // shape, and the whole point of an ordered list.
        assert!(
            TierRecipe::new(
                vec!["a/b".into(), "c/d".into()],
                vec!["e/f".into()],
                PickerMode::EfficientFirst,
                DEFAULT_CONFIDENCE_THRESHOLD
            )
            .is_ok()
        );
    }

    #[test]
    fn capable_first_carries_the_uncalibrated_warning_and_efficient_first_does_not() {
        let capable_first = TierRecipe::new(
            vec!["openai/sol".into()],
            vec!["openai/luna".into()],
            PickerMode::CapableFirst,
            DEFAULT_CONFIDENCE_THRESHOLD,
        )
        .unwrap();
        let warning = capable_first
            .uncalibrated_warning()
            .expect("capable_first is unbenchmarked upstream and has to say so");
        assert!(warning.contains("efficient_first"));
        assert!(warning.contains("SWE-Bench-Pro-Python-75"));

        let efficient_first = TierRecipe::new(
            vec!["openai/sol".into()],
            vec!["openai/luna".into()],
            PickerMode::EfficientFirst,
            DEFAULT_CONFIDENCE_THRESHOLD,
        )
        .unwrap();
        assert_eq!(
            efficient_first.uncalibrated_warning(),
            None,
            "the control: the shape every published number was measured at \
             earns no warning"
        );
    }

    // ─── the policy ──────────────────────────────────────────────────────────

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

    struct Fixture {
        session_id: SessionId,
        ledger: CacheLedger,
        turn_policy: TurnPolicy,
        frontier_history: FrontierHistory,
        budget: TurnBudget,
        recipe: Option<TierRecipe>,
        signals: Option<TurnSignals>,
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

        fn ctx<'a>(&'a self, candidates: &'a [Candidate]) -> RoutingContext<'a> {
            RoutingContext {
                session_id: &self.session_id,
                turn_index: 3,
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

    /// capable = [sol], efficient = [luna, terra] — the shape the plan names.
    fn recipe(picker: PickerMode) -> TierRecipe {
        TierRecipe::new(
            vec!["openai/sol".into()],
            vec!["openai/luna".into(), "openai/terra".into()],
            picker,
            DEFAULT_CONFIDENCE_THRESHOLD,
        )
        .unwrap()
    }

    fn fleet() -> Vec<Candidate> {
        vec![
            hosted("sol", 0.95, 1.00),
            hosted("luna", 0.70, 0.10),
            hosted("terra", 0.80, 0.30),
        ]
    }

    /// A session deep enough to be judged, producing nothing, investigating
    /// nothing, and carrying a hard error: two corroborating signals.
    fn stalling() -> TurnSignals {
        TurnSignals {
            tools: ToolSignals {
                severity: crate::validate::tool_signals::HARD,
                ..Default::default()
            },
            turn_depth: STALL_MIN_TURN_DEPTH + 4,
        }
    }

    fn stage() -> StagePolicy {
        StagePolicy::new(Box::new(AffinityPolicy::new()))
    }

    /// One of our own workers, which no recipe in this module names.
    fn local_worker() -> Candidate {
        Candidate {
            target: Target::Local {
                worker_id: 7,
                dp_rank: 0,
                model: "small".into(),
            },
            expected_prefill_tokens: 1_000.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 900.0,
            expected_cost_usd: 0.0,
            quality_prior: 0.55,
            load: None,
        }
    }

    /// **M10 review G02.** A recipe that names no local target must not turn a
    /// documented degradation into a failed turn.
    ///
    /// The cadence is written spent — `max_frontier: 0` excludes every hosted
    /// candidate in `admissible`'s first filter and exempts local, which is the
    /// admitted set a *ration* leaves once it is used up. Constructed that way
    /// rather than by spending a window, because `FrontierHistory::record` is
    /// crate-private on purpose: the only truthful producer is the session
    /// projection, and the engine-level guard
    /// (`tests/tier_selection.rs::a_spent_cadence_serves_locally_even_under_a_tier_recipe`)
    /// is the one that spends a real one.
    #[tokio::test]
    async fn a_pool_narrowed_to_local_is_served_even_though_no_tier_names_it() {
        let mut candidates = fleet();
        candidates.push(local_worker());
        let spent = Fixture::open()
            .with_recipe(recipe(PickerMode::EfficientFirst))
            .under(TurnPolicy {
                frontier_cadence: Some(FrontierCadence {
                    max_frontier: 0,
                    per_turns: 10,
                }),
                ..TurnPolicy::unrestricted()
            });

        let decision = stage().choose(&spent.ctx(&candidates)).await.unwrap();
        assert_eq!(
            decision.target, candidates[3].target,
            "the recipe names sol, luna and terra and admission left none of \
             them; the local worker it does not name is what the cadence \
             promised: {}",
            decision.rationale
        );
        assert_eq!(
            decision.source, None,
            "no tier served this turn, and stamping one would tell the handoff \
             gate a scorer's pick had been honoured"
        );
        assert!(decision.fallbacks.is_empty());
        assert!(
            decision.rationale.contains("local/small"),
            "the rationale names what took the turn and why: {}",
            decision.rationale
        );

        // CONTROL: both tiers empty with no local candidate to degrade onto.
        // Nothing was promised here and nothing is invented — the turn refuses
        // exactly as it did before G02.
        let absent = Fixture::open().with_recipe(
            TierRecipe::new(
                vec!["openai/absent-a".into()],
                vec!["openai/absent-b".into()],
                PickerMode::EfficientFirst,
                DEFAULT_CONFIDENCE_THRESHOLD,
            )
            .unwrap(),
        );
        let hosted_only = fleet();
        let error = stage()
            .choose(&absent.ctx(&hosted_only))
            .await
            .expect_err("a recipe naming nothing admitted, with nowhere local to go");
        assert!(
            matches!(error, RoutingError::NoViableCandidate { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_stalling_session_escalates_and_a_quiet_one_does_not() {
        let candidates = fleet();

        let stalled = Fixture::open()
            .with_recipe(recipe(PickerMode::EfficientFirst))
            .with_signals(stalling());
        let decision = stage().choose(&stalled.ctx(&candidates)).await.unwrap();
        assert_eq!(
            decision.target, candidates[0].target,
            "{}",
            decision.rationale
        );
        assert_eq!(decision.source, Some(DecisionSource::Dimensions));
        assert!(
            decision.rationale.contains("strong tier"),
            "{}",
            decision.rationale
        );

        // The control, and it is what makes the assertion above about the
        // *signals*: the identical recipe over the identical fleet with nothing
        // wrong serves the efficient tier's first entry.
        let quiet = Fixture::open().with_recipe(recipe(PickerMode::EfficientFirst));
        let decision = stage().choose(&quiet.ctx(&candidates)).await.unwrap();
        assert_eq!(
            decision.target, candidates[1].target,
            "{}",
            decision.rationale
        );
        assert_eq!(decision.source, Some(DecisionSource::Ambiguous));
    }

    #[tokio::test]
    async fn the_fallbacks_are_the_rest_of_the_serving_tier_in_recipe_order() {
        let candidates = fleet();
        let fixture = Fixture::open().with_recipe(recipe(PickerMode::EfficientFirst));
        let decision = stage().choose(&fixture.ctx(&candidates)).await.unwrap();

        assert_eq!(decision.target, candidates[1].target, "luna leads the tier");
        assert_eq!(
            decision.fallbacks,
            vec![candidates[2].target.clone()],
            "and terra is behind it, in the recipe's order and not the quoter's"
        );
        // Never the other tier: a fallback is a second attempt at the same
        // question, not a tier change nobody scored.
        assert!(!decision.fallbacks.contains(&candidates[0].target));
    }

    #[tokio::test]
    async fn the_scorer_never_picks_outside_the_admitted_pool_and_neither_do_the_fallbacks() {
        // The signals say capable and the key does not admit the capable tier's
        // only member. A recipe may narrow what a key admits; it may never
        // widen it, so the turn falls to the other tier rather than reaching
        // sol.
        let candidates = fleet();
        let confined = Fixture::open()
            .with_recipe(recipe(PickerMode::EfficientFirst))
            .with_signals(stalling())
            .under(TurnPolicy {
                allow: TargetFilter::parse(["openai/luna", "openai/terra"]).unwrap(),
                ..TurnPolicy::unrestricted()
            });
        let decision = stage().choose(&confined.ctx(&candidates)).await.unwrap();
        assert_eq!(
            decision.target, candidates[1].target,
            "{}",
            decision.rationale
        );
        assert!(
            decision.rationale.contains("admits none of it"),
            "the fall to the other tier has to be legible: {}",
            decision.rationale
        );

        // The fallback arm of the same claim: terra is excluded, so the
        // decision's fallback list must not name it either. A fallback the key
        // cannot reach is a target the engine would dispatch to on the first
        // provider hiccup.
        let luna_only = Fixture::open()
            .with_recipe(recipe(PickerMode::EfficientFirst))
            .under(TurnPolicy {
                allow: TargetFilter::parse(["openai/luna"]).unwrap(),
                ..TurnPolicy::unrestricted()
            });
        let decision = stage().choose(&luna_only.ctx(&candidates)).await.unwrap();
        assert_eq!(decision.target, candidates[1].target);
        assert!(
            decision.fallbacks.is_empty(),
            "a fallback the policy excluded is not a fallback: {:?}",
            decision.fallbacks
        );

        // The control: unconfined, the same stalling signals do reach sol.
        let open = Fixture::open()
            .with_recipe(recipe(PickerMode::EfficientFirst))
            .with_signals(stalling());
        assert_eq!(
            stage().choose(&open.ctx(&candidates)).await.unwrap().target,
            candidates[0].target
        );
    }

    #[tokio::test]
    async fn a_recipe_no_admitted_candidate_matches_is_an_unviable_candidate_set() {
        // Both tiers name models this fleet does not quote. Not a new error
        // variant -- see the branch that produces it for why the taxonomy stays
        // at three.
        let candidates = fleet();
        let elsewhere = Fixture::open().with_recipe(
            TierRecipe::new(
                vec!["anthropic/opus".into()],
                vec!["anthropic/haiku".into()],
                PickerMode::EfficientFirst,
                DEFAULT_CONFIDENCE_THRESHOLD,
            )
            .unwrap(),
        );
        assert!(matches!(
            stage().choose(&elsewhere.ctx(&candidates)).await,
            Err(RoutingError::NoViableCandidate { .. })
        ));
    }

    #[tokio::test]
    async fn a_project_without_a_recipe_routes_byte_for_byte_as_its_inner_policy() {
        // The compatibility guarantee that lets a deployment compose the stage
        // router without re-routing the projects that have no recipe. Target,
        // rationale and budget state all come from the inner policy; only the
        // recorded policy *name* differs, which is the object in force and not
        // a routing change.
        let candidates = fleet();
        let plain = Fixture::open();
        let inner = AffinityPolicy::new()
            .choose(&plain.ctx(&candidates))
            .await
            .unwrap();
        let staged = stage().choose(&plain.ctx(&candidates)).await.unwrap();
        assert_eq!(staged, inner);
        assert_eq!(
            staged.source, None,
            "an unstaged decision has no tier source"
        );
        assert!(staged.fallbacks.is_empty());
    }
}
