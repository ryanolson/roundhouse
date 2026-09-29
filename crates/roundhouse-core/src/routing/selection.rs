// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Persisted inputs and configuration for one routing selection.
//!
//! The engine captures local features before selection and copies the returned
//! policy evidence into each dispatch record. A later recipe or extractor change
//! therefore cannot replace the recorded values.
//!
//! These types describe a decision; they do not construct or validate runtime
//! policy. Admitted targets record the admission result, not its credential,
//! cadence, or budget inputs. Candidate quotes remain on
//! [`DecisionRecord::considered`](super::DecisionRecord::considered).

use serde::{Deserialize, Serialize};

use super::learn::LearnedEvidence;
use super::stage::{DecisionSource, Pick, PickerMode, Tier, TierRecipe, TurnSignals};
use super::{Decision, Target};
use crate::classify::ClassificationWindow;
use crate::validate::{ControlCallDialect, ObjectiveVersion};

/// The revision of the local feature extractor whose output [`LocalFeatures`]
/// carries.
///
/// **Bumped when the extractor's *interpretation* changes**, not when a caller
/// changes. The routing plan names exactly this failure: a record indexed by
/// what a later build believes the same exchanges mean is a record that cannot
/// be replayed, and a version stamp is what turns that into a filter rather than
/// into a silent re-reading. `1` is `TurnSignals::from_exchanges` over
/// `task_exchanges_on`, which is the extractor that shipped with this field.
pub const FEATURE_EXTRACTOR_REVISION: u32 = 1;

/// The revision of [`AffinityPolicy`](super::AffinityPolicy)'s scoring.
pub const AFFINITY_SELECTOR_REVISION: u32 = 1;

/// The revision of [`EscalationPolicy`](super::EscalationPolicy)'s audit branch.
pub const ESCALATION_AUDIT_SELECTOR_REVISION: u32 = 1;

/// The revision of [`StagePolicy`](super::StagePolicy)'s tier selection.
pub const STAGE_SELECTOR_REVISION: u32 = 1;

/// The revision of the learned selector: the gate, the constraints, the
/// exploration rule and the choice among passing strategies.
///
/// Part of the epoch id, like the input and credit revisions in
/// [`learn`](super::learn).
pub const LEARNED_SELECTOR_REVISION: u32 = 1;

/// What the extractor computed for this turn, and where in the log it read to.
///
/// One struct rather than five fields on the snapshot, because they are only
/// meaningful together: signals taken at one log position and a sequence number
/// taken at another describe no turn that ever happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalFeatures {
    /// [`FEATURE_EXTRACTOR_REVISION`] as of the process that wrote this record.
    pub extractor_revision: u32,
    /// How the client that wrote this session's log spells a call to one of our
    /// own tools — the parameter the extractor was run *under*, which decides
    /// which exchanges it dropped before counting anything.
    pub dialect: ControlCallDialect,
    /// Exactly the signals handed to `choose`, not a projection of them.
    pub signals: TurnSignals,
    /// The turn these features were taken for, `0` for a session's first.
    pub turn_index: u64,
    /// The log sequence the extractor read through.
    ///
    /// **Captured with the features and before the first `Routed`**, which is
    /// what makes it a statement about the inputs rather than about the write.
    /// Recomputed per dispatch it would creep forward on every failover, and a
    /// second attempt would then claim to have seen the record of the first.
    pub observed_through_seq: u64,
}

/// The [`AffinityPolicy`](super::AffinityPolicy) tuning a decision was scored
/// under.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AffinityEvidence {
    pub prefill_weight: f64,
    pub cost_weight: f64,
    pub ttft_weight: f64,
    /// The ceiling this instance passed to `admissible`, in potential prefill
    /// tokens. `None` is "do not exclude on load" and is the shipped default.
    pub max_load: Option<f64>,
}

/// Which branch of [`StagePolicy`](super::StagePolicy) produced the target.
///
/// Four arms because the four are reached by different code and an operator
/// reading a log needs to tell them apart: a turn served by the tier the scorer
/// picked, a turn whose picked tier admitted nothing, a turn a quote moved on
/// price, and a turn that left the recipe altogether to keep the degrade-to-local
/// promise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StageOutcome {
    /// The picked tier had admitted members, and its head took the turn.
    Served { tier: Tier },
    /// The picked tier admitted nothing, so the other tier served.
    PickedTierEmpty { served: Tier },
    /// An admitted capable candidate quoted below the efficient tier's head.
    ///
    /// Carries no `served` field: the guard only ever fires out of the
    /// capable pool, so a served tier here could only ever say `Capable` — a
    /// field with one reachable value is not a fact worth recording, and the
    /// caller that needs to know which tier served reads it off its own
    /// resolution rather than reconstructing it from this arm.
    ///
    /// **The head's tier, not every dispatch's.** A guarded turn's fallbacks
    /// run through the efficient tier before the rest of the capable one
    /// (2026-09-28 ruling 4), so a failover can dispatch an efficient member
    /// under a record that still carries this arm. A reader that needs the
    /// tier of a dispatched target reads it off the recipe lists by the
    /// `Routed` record's own target ([`RecipeEvidence::tier_of`]), never off
    /// this arm. `engine::opened_a_tier_escalation` asks the same question of
    /// the live recipe's capable list.
    ///
    /// `displaced` is the head it dominated, by
    /// [`Target::policy_identity`](super::Target::policy_identity) — the same
    /// spelling the recipe uses, so the two read as one language.
    CostGuard { displaced: String },
    /// Nothing the recipe names was admitted, and a local worker took the turn.
    ///
    /// No tier served, which is why this arm carries none: stamping one would
    /// tell a reader the scorer's pick had been honoured when it was bypassed.
    DegradedPastRecipe { degraded_to: String },
}

impl StageOutcome {
    /// The [`DecisionSource`] this outcome implies for `pick`.
    ///
    /// The one home for the rule, shared by [`StageEvidence::source`] and by a
    /// learned decision's served plan, so the two cannot come to disagree about
    /// which turns narrate. A cost guard moved the decision off the pick, so it
    /// names itself; a recipe degrade served no tier at all, so it names none;
    /// a signal that picked the cheap tier and found it empty did not pick the
    /// capable tier that served, so it is the fall-open
    /// [`DecisionSource::Ambiguous`] names; every other arm is exactly what the
    /// pick said.
    ///
    /// The fallthrough arm matters because the handoff note gates on this
    /// value. Carried through, a `Dimensions` de-escalation would have the
    /// note tell the capable model the previous steps were in trouble when
    /// the signals said the opposite — the same reason `CostGuard` is not
    /// signal-driven.
    pub fn source(&self, pick: &Pick) -> Option<DecisionSource> {
        match self {
            StageOutcome::CostGuard { .. } => Some(DecisionSource::CostGuard),
            StageOutcome::DegradedPastRecipe { .. } => None,
            StageOutcome::PickedTierEmpty {
                served: Tier::Capable,
            } if pick.source.is_signal_driven() => Some(DecisionSource::Ambiguous),
            StageOutcome::Served { .. } | StageOutcome::PickedTierEmpty { .. } => Some(pick.source),
        }
    }
}

/// The recipe a tier decision ran under.
///
/// **Full typed configuration rather than a digest.** A digest tells a reader
/// that two turns ran under different settings and never which settings; these
/// are the operator's own lists, in the operator's own order, which is what a
/// reader needs to explain why a turn went where it did.
///
/// One struct for both readers: [`StageEvidence`] carries it flattened, so a
/// stage record's wire shape is the four fields side by side, and a learned
/// decision holds it once for every plan. Two copies of these fields would be
/// two tier lookups that could come to disagree about which tier a dispatched
/// target was served on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeEvidence {
    /// The recipe's capable tier, in the operator's order.
    pub capable: Vec<String>,
    /// The recipe's efficient tier, in the operator's order.
    pub efficient: Vec<String>,
    pub picker: PickerMode,
    pub confidence_threshold: f64,
}

impl RecipeEvidence {
    pub fn of(recipe: &TierRecipe) -> Self {
        Self {
            capable: recipe.list(Tier::Capable).to_vec(),
            efficient: recipe.list(Tier::Efficient).to_vec(),
            picker: recipe.picker(),
            confidence_threshold: recipe.confidence_threshold(),
        }
    }

    /// The recipe tier that names `target`, or `None` when neither list does.
    ///
    /// **The tier of a dispatched target, read off the recipe, never off the
    /// pick or the outcome.** A cost guard serves a capable target on a turn
    /// the scorer picked efficient, and a guarded turn's failover can then
    /// dispatch an efficient member under the same record (see
    /// [`StageOutcome::CostGuard`]). Only the target that was dispatched says
    /// which tier served. `None` is a recipe degrade to a local worker the
    /// recipe does not name.
    pub fn tier_of(&self, target: &Target) -> Option<Tier> {
        let identity = target.policy_identity();
        if self.capable.contains(&identity) {
            Some(Tier::Capable)
        } else if self.efficient.contains(&identity) {
            Some(Tier::Efficient)
        } else {
            None
        }
    }
}

/// The recipe a staged decision ran under, and what the scorer answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageEvidence {
    /// Flattened, so the four recipe fields sit beside `pick` and `outcome` on
    /// the wire exactly as they did before the struct existed.
    #[serde(flatten)]
    pub recipe: RecipeEvidence,
    /// What `pick_tier` answered, carried verbatim rather than recomputed.
    ///
    /// The scorer is pure, so a reader *could* re-run it — on the extractor and
    /// the thresholds of whatever build is reading, which is the substitution
    /// this whole module exists to prevent.
    pub pick: Pick,
    pub outcome: StageOutcome,
}

impl StageEvidence {
    /// The recipe and the scorer's answer, paired with what the resolution
    /// actually did.
    ///
    /// One constructor for the two paths that record a stage decision
    /// (`StagePolicy::route_pick` and `StagePolicy::degrade_past_the_recipe`),
    /// with the recipe half read by [`RecipeEvidence::of`]: a field added to the
    /// recipe needs one edit, and the learned record picks it up too.
    pub fn new(recipe: &TierRecipe, pick: Pick, outcome: StageOutcome) -> Self {
        Self {
            recipe: RecipeEvidence::of(recipe),
            pick,
            outcome,
        }
    }

    /// The [`DecisionSource`] this evidence implies.
    ///
    /// **Derived, never stored.** A [`Decision`](super::Decision)'s own
    /// `source` is read from the evidence recorded beside it rather than
    /// computed a second time by the caller and carried next to it hoping the
    /// two agree. The rule itself is [`StageOutcome::source`].
    pub fn source(&self) -> Option<DecisionSource> {
        self.outcome.source(&self.pick)
    }
}

/// Which builtin selector ran, and the configuration it ran under.
///
/// `None` on a [`Decision`] a policy outside this module assembled by hand: the
/// vocabulary here names the branches this module has, and a fourth policy's
/// branch is honestly unknown to it. Silence is the correct answer there, and a
/// nearest-fit arm would be a claim nothing measured.
///
/// **No in-tree path produces `None` today.**
/// [`Admitted::decide`](super::Admitted::decide) and
/// [`Admitted::decide_staged`](super::Admitted::decide_staged) both take a
/// `SelectorSnapshot` as a required argument, and every builtin policy goes
/// through one of the two — so `None` is reachable only from a `Decision` a
/// policy outside this module assembles field by field, or from a record
/// written before this field existed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectorSnapshot {
    /// The revision of the branch's algorithm, so a later change to how a
    /// weight or a threshold is applied is visible without re-reading the code
    /// the record was written by.
    pub algorithm_revision: u32,
    pub branch: SelectorBranch,
}

/// The builtin branches, one arm each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectorBranch {
    Affinity(AffinityEvidence),
    /// The escalation policy's *audit* branch — the only one it has of its own.
    ///
    /// A non-audit turn delegates to the inner affinity policy and carries that
    /// policy's evidence unchanged, because that is the code that chose: an
    /// arm here would report an escalation on a turn that never escalated.
    EscalationAudit {
        audit_every: u64,
    },
    Stage(StageEvidence),
    /// A learned decision: every strategy's plan, the counts it read, and what
    /// it chose. Written only by a project whose learner is not `off`.
    ///
    /// **Boxed**, because the evidence carries every plan and the store view,
    /// and the branch is moved with every selection snapshot: inline it would
    /// widen the other three arms by its own size. The box keeps
    /// `SelectorBranch` exactly as wide as it was without this arm.
    Learned(Box<LearnedEvidence>),
}

impl SelectorBranch {
    /// The [`DecisionSource`] this branch's evidence implies.
    ///
    /// **The one home for which branches state a source**, read by
    /// [`SelectionSnapshot::source`] and so by the handoff gate. A stage
    /// decision's is [`StageEvidence::source`], a learned decision's is its
    /// served plan's ([`LearnedEvidence::source`]); affinity and the
    /// escalation audit pick a candidate, not a tier, and have none. No
    /// wildcard arm: a new branch has to say which it is.
    pub fn source(&self) -> Option<DecisionSource> {
        match self {
            SelectorBranch::Stage(evidence) => evidence.source(),
            SelectorBranch::Learned(evidence) => evidence.source(),
            SelectorBranch::Affinity(_) | SelectorBranch::EscalationAudit { .. } => None,
        }
    }

    /// The recipe tier that names `target`, or `None` for a branch no tier
    /// recipe made or a target the recipe does not name.
    ///
    /// **The one home for which branches carry a recipe**, read by the tier
    /// agreement report. A learned turn served a target its recipe names and is
    /// read by the same lists a stage turn is ([`RecipeEvidence::tier_of`]);
    /// read as "no recipe", every learned turn would drop out of the report.
    /// No wildcard arm, for the same reason as [`Self::source`].
    pub fn tier_of(&self, target: &Target) -> Option<Tier> {
        match self {
            SelectorBranch::Stage(evidence) => evidence.recipe.tier_of(target),
            SelectorBranch::Learned(evidence) => evidence.recipe.tier_of(target),
            SelectorBranch::Affinity(_) | SelectorBranch::EscalationAudit { .. } => None,
        }
    }
}

impl SelectorSnapshot {
    /// Pair the affinity branch with its current algorithm revision.
    pub fn affinity(evidence: AffinityEvidence) -> Self {
        Self {
            algorithm_revision: AFFINITY_SELECTOR_REVISION,
            branch: SelectorBranch::Affinity(evidence),
        }
    }

    pub fn escalation_audit(audit_every: u64) -> Self {
        Self {
            algorithm_revision: ESCALATION_AUDIT_SELECTOR_REVISION,
            branch: SelectorBranch::EscalationAudit { audit_every },
        }
    }

    pub fn stage(evidence: StageEvidence) -> Self {
        Self {
            algorithm_revision: STAGE_SELECTOR_REVISION,
            branch: SelectorBranch::Stage(evidence),
        }
    }

    pub fn learned(evidence: LearnedEvidence) -> Self {
        Self {
            algorithm_revision: LEARNED_SELECTOR_REVISION,
            branch: SelectorBranch::Learned(Box::new(evidence)),
        }
    }
}

/// The inputs and the branch behind one turn's route, frozen before dispatch.
///
/// Built once per turn and cloned onto every `Routed` the turn writes. Each
/// dispatch keeps its own `chosen`, `rate_card` and `attempts` — those describe
/// the attempt — while this describes the *selection*, which happened once and
/// does not happen again because a provider was down.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionSnapshot {
    pub features: LocalFeatures,
    /// The target the policy named, before any failover advanced past it.
    pub selected: Target,
    /// The ordered plan behind it, empty for a policy that picks a candidate
    /// rather than a tier.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<Target>,
    /// The pool the policy's own admission call returned.
    ///
    /// `None` means unknown — a policy that assembled its [`Decision`] without
    /// going through [`Admitted`](super::Admitted). `Some` is the *actual*
    /// resolution and never an engine reconstruction: the escalation audit
    /// branch and the stage router both pass `None` for `max_load` where the
    /// affinity policy may pass a ceiling, and the overflow valve can re-admit a
    /// pool no second call would reproduce, so asking `admissible` again would
    /// answer a different question and record it as this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted: Option<Vec<Target>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<SelectorSnapshot>,
    /// The background classifications that had landed by this decision's
    /// cutoff, bounded and named rather than copied.
    ///
    /// **References, not labels.** The records themselves are immutable events
    /// in this same log, so copying their contents here would put a second copy
    /// beside the first, and reading them back out of mutable history during a
    /// replay would let a later build re-interpret what an earlier decision saw.
    ///
    /// **Bounded, because the unbounded version is quadratic** — see
    /// [`ClassificationWindow`], which also carries how many were available
    /// beyond the ones it names, so an omission is a number rather than a
    /// silence.
    ///
    /// **Available, not consumed.** No routing policy shipping today reads a
    /// classification; this records what a later learner would need in order to
    /// reconstruct the feature set, and says nothing about the choice that was
    /// made.
    ///
    /// `None` means one of three things and deliberately does not distinguish
    /// them: no classifier is configured, this deployment never opted in, or the
    /// record predates the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifications: Option<ClassificationWindow>,
    /// The objective this turn was decided under, for frontier review coverage.
    ///
    /// `None` on records written before the field and on hand-built decisions.
    /// A review covering such a decision cannot show that it applied the same
    /// objective, so it records the gap instead of a label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<ObjectiveVersion>,
}

impl SelectionSnapshot {
    /// Copy the returned decision evidence alongside captured local features.
    ///
    /// The caller must capture both the features and the classification
    /// references before selection — a reference gathered afterwards could name
    /// a result that landed during this very turn, which is exactly the
    /// backdating [`crate::classify::ClassificationRef::available_seq`] exists to make impossible.
    pub fn of(
        decision: &Decision,
        features: LocalFeatures,
        classifications: Option<ClassificationWindow>,
        objective: Option<ObjectiveVersion>,
    ) -> Self {
        Self {
            features,
            selected: decision.target.clone(),
            fallbacks: decision.fallbacks.clone(),
            admitted: decision.admitted.clone(),
            selector: decision.selector.clone(),
            classifications,
            objective,
        }
    }

    /// The [`DecisionSource`] the selection ran under, read off the same
    /// evidence [`Self::selector`] already carries rather than kept as a
    /// second copy ([`SelectorBranch::source`]). An unknown policy's `None`
    /// selector has no source to state.
    pub fn source(&self) -> Option<DecisionSource> {
        self.selector
            .as_ref()
            .and_then(|selector| selector.branch.source())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(pick: Tier, outcome: StageOutcome) -> StageEvidence {
        StageEvidence {
            recipe: RecipeEvidence {
                capable: vec!["anthropic/opus".into()],
                efficient: vec!["anthropic/haiku".into(), "local/qwen".into()],
                picker: PickerMode::EfficientFirst,
                confidence_threshold: 0.5,
            },
            pick: Pick {
                tier: pick,
                source: DecisionSource::Dimensions,
                score: 0.0,
                confidence: Some(0.9),
            },
            outcome,
        }
    }

    fn local(model: &str) -> Target {
        Target::Local {
            worker_id: 7,
            dp_rank: 1,
            model: model.into(),
        }
    }

    fn frontier(provider: &str, model: &str) -> Target {
        Target::Frontier {
            provider: provider.into(),
            model: model.into(),
        }
    }

    /// **A target neither list names has no recipe tier**, whatever the pick
    /// was. A recipe degrade to a local worker the recipe does not name is the
    /// case this exists for: reporting the pick there would book a tier the
    /// turn was never served on, and the agreement report would compare the
    /// classifier against the scorer rather than against what served.
    #[test]
    fn a_target_outside_both_recipe_lists_has_no_tier() {
        for pick in [Tier::Capable, Tier::Efficient] {
            let evidence = evidence(
                pick,
                StageOutcome::DegradedPastRecipe {
                    degraded_to: "local/llama".into(),
                },
            );
            assert_eq!(
                evidence.recipe.tier_of(&local("llama")),
                None,
                "pick {pick:?}"
            );
            assert_eq!(
                evidence.recipe.tier_of(&frontier("openai", "gpt")),
                None,
                "pick {pick:?}"
            );
            // Controls: a named target reads its own list, not the pick, and a
            // local worker is named by model whatever its worker and rank.
            assert_eq!(
                evidence.recipe.tier_of(&frontier("anthropic", "opus")),
                Some(Tier::Capable),
                "pick {pick:?}"
            );
            assert_eq!(
                evidence.recipe.tier_of(&frontier("anthropic", "haiku")),
                Some(Tier::Efficient),
                "pick {pick:?}"
            );
            assert_eq!(
                evidence.recipe.tier_of(&local("qwen")),
                Some(Tier::Efficient),
                "pick {pick:?}"
            );
        }
    }
}
