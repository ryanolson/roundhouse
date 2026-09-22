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

use super::stage::{DecisionSource, Pick, PickerMode, Tier, TurnSignals};
use super::{Decision, Target};
use crate::validate::ControlCallDialect;

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
    /// `displaced` is the head it dominated, by
    /// [`Target::policy_identity`](super::Target::policy_identity) — the same
    /// spelling the recipe uses, so the two read as one language.
    CostGuard { served: Tier, displaced: String },
    /// Nothing the recipe names was admitted, and a local worker took the turn.
    ///
    /// No tier served, which is why this arm carries none: stamping one would
    /// tell a reader the scorer's pick had been honoured when it was bypassed.
    DegradedPastRecipe { degraded_to: String },
}

/// The recipe a staged decision ran under, and what the scorer answered.
///
/// **Full typed configuration rather than a digest.** A digest tells a reader
/// that two turns ran under different settings and never which settings; these
/// are the operator's own lists, in the operator's own order, which is what a
/// reader needs to explain why a turn went where it did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageEvidence {
    /// The recipe's capable tier, in the operator's order.
    pub capable: Vec<String>,
    /// The recipe's efficient tier, in the operator's order.
    pub efficient: Vec<String>,
    pub picker: PickerMode,
    pub confidence_threshold: f64,
    /// What `pick_tier` answered, carried verbatim rather than recomputed.
    ///
    /// The scorer is pure, so a reader *could* re-run it — on the extractor and
    /// the thresholds of whatever build is reading, which is the substitution
    /// this whole module exists to prevent.
    pub pick: Pick,
    pub outcome: StageOutcome,
}

/// Which builtin selector ran, and the configuration it ran under.
///
/// `None` on a [`Decision`] a policy outside this module assembled by hand: the
/// vocabulary here names the branches this module has, and a fourth policy's
/// branch is honestly unknown to it. Silence is the correct answer there, and a
/// nearest-fit arm would be a claim nothing measured.
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
    /// The [`DecisionSource`] the returned [`Decision`] carried.
    ///
    /// **A property of the selection, not of the attempt**: every record of one
    /// turn repeats it, because no fallback re-ran the scorer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<DecisionSource>,
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
}

impl SelectionSnapshot {
    /// Copy the returned decision evidence alongside captured local features.
    /// The caller must capture those features before selection.
    pub fn of(decision: &Decision, features: LocalFeatures) -> Self {
        Self {
            features,
            selected: decision.target.clone(),
            fallbacks: decision.fallbacks.clone(),
            source: decision.source,
            admitted: decision.admitted.clone(),
            selector: decision.selector.clone(),
        }
    }
}
