// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a turn is *about*, as a versioned categorical vocabulary, and the
//! durable records one classification of it leaves behind.
//!
//! **These are uncertain features, never truth and never reward.** A label here
//! says a classifier answered; it does not say the answer was right, and nothing
//! downstream may read one as evidence that a route was good. The quality signal
//! is the frontier review interval (`PLAN-routing-strategy-bandit.md`), and it is
//! a different thing arriving on a different event.
//!
//! ## Why a taxonomy rather than a tier
//!
//! The adapter that shipped first asked one question — capable or efficient —
//! which is a *routing* answer wearing a classification's clothes: it names the
//! decision instead of describing the turn, so a later selector that wanted a
//! different mapping had nothing to re-map from. The three axes here describe the
//! turn and leave the mapping to code, which is the same rule
//! [`validate::brief`](crate::validate::brief) holds for the judge.
//!
//! Each axis offers `unknown` as a real option, and that is deliberate: a
//! classifier that cannot tell must be able to say so. It is **not** the same
//! state as a call that produced no usable answer set — that one is
//! [`ClassificationOutcome::Unusable`] and carries no classification at all.
//! Collapsing the two would read "the model could not tell" and "nobody asked"
//! as one fact, and a feature built from the pair would learn from the wrong one.
//!
//! ## Versions on every record
//!
//! [`TAXONOMY_VERSION`] moves when an option's *meaning* changes, not when a
//! caller does. A record indexed by what a later build believes `involved` means
//! is a record that cannot be replayed, and the same argument
//! [`FEATURE_EXTRACTOR_REVISION`](crate::routing::FEATURE_EXTRACTOR_REVISION)
//! makes applies here.

pub mod projection;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

use crate::control::BudgetWindow;
use crate::ids::ResponseId;
use crate::routing::ledger::ProviderPricing;

pub use projection::{ProjectionCaps, PromptOrigin, TurnProjection};

/// The revision of the option sets below and of what each option means.
pub const TAXONOMY_VERSION: u32 = 1;

/// One axis of the taxonomy: the key it is asked under, and its options.
///
/// **A trait rather than three hand-written enums, one per axis.** "Every
/// axis offers `unknown`" and "every option label round-trips" are properties
/// one test can assert once, over all three axis types, instead of three
/// times against three near-identical bodies.
///
/// **`OPTIONS` is the one table each axis itself writes, not three.** It is
/// the single place a label, its rubric and the variant it parses to are
/// written down; `from_label` and `label` are default methods derived from it
/// rather than a second and third hand-written match a reader would have to
/// keep in step with it by hand — a missed `from_label` arm would silently
/// drop a valid classifier answer as unusable. Adding an option is one
/// `OPTIONS` entry.
pub trait ClassificationAxis: Sized + Copy + PartialEq + 'static {
    /// The key this axis is asked and answered under.
    const KEY: &'static str;
    /// What the classifier is asked to decide.
    const INSTRUCTIONS: &'static str;
    /// Every option, as `(variant, label, rubric)`, in the order they are
    /// offered.
    const OPTIONS: &'static [(Self, &'static str, &'static str)];

    /// The variant a label names, or `None` for a label no option offers.
    fn from_label(label: &str) -> Option<Self> {
        Self::OPTIONS
            .iter()
            .find(|(_, candidate, _)| *candidate == label)
            .map(|(variant, _, _)| *variant)
    }

    fn label(self) -> &'static str {
        Self::OPTIONS
            .iter()
            .find(|(variant, _, _)| *variant == self)
            .map(|(_, label, _)| *label)
            .expect("every constructible variant has an OPTIONS entry")
    }
}

/// What the turn is trying to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnIntent {
    /// Write or change code.
    Implement,
    /// Find out why something is wrong.
    Diagnose,
    /// Answer a question about existing material.
    Explain,
    /// Judge work that already exists.
    Review,
    /// Run or inspect something, rather than change it.
    Operate,
    /// The classifier could not tell.
    Unknown,
}

impl ClassificationAxis for TurnIntent {
    const KEY: &'static str = "intent";
    const INSTRUCTIONS: &'static str = "What is this turn asking for?";
    const OPTIONS: &'static [(Self, &'static str, &'static str)] = &[
        (
            Self::Implement,
            "implement",
            "Write new code or change existing code to add or alter behaviour",
        ),
        (
            Self::Diagnose,
            "diagnose",
            "Find the cause of a failure, a wrong result or unexpected behaviour",
        ),
        (
            Self::Explain,
            "explain",
            "Answer a question about material that already exists, without changing it",
        ),
        (
            Self::Review,
            "review",
            "Judge work that already exists against a standard, and report on it",
        ),
        (
            Self::Operate,
            "operate",
            "Run, inspect or report on something rather than change it",
        ),
        (
            Self::Unknown,
            "unknown",
            "The request does not clearly fit any other option, or there is too little to tell",
        ),
    ];
}

/// How much work the turn is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnComplexity {
    /// One obvious step whose result is immediate.
    Trivial,
    /// Ordinary work along a known path.
    Routine,
    /// Several steps, or one step whose shape has to be worked out.
    Involved,
    /// Open-ended work whose plan is part of the answer.
    Deep,
    /// The classifier could not tell.
    Unknown,
}

impl ClassificationAxis for TurnComplexity {
    const KEY: &'static str = "complexity";
    const INSTRUCTIONS: &'static str = "How much work is this turn?";
    const OPTIONS: &'static [(Self, &'static str, &'static str)] = &[
        (
            Self::Trivial,
            "trivial",
            "One obvious step, and the result is immediately checkable",
        ),
        (
            Self::Routine,
            "routine",
            "Ordinary work along a path that is already clear",
        ),
        (
            Self::Involved,
            "involved",
            "Several dependent steps, or one step whose approach has to be worked out first",
        ),
        (
            Self::Deep,
            "deep",
            "Open-ended work where deciding what to do is part of the answer",
        ),
        (
            Self::Unknown,
            "unknown",
            "There is too little in the request to judge how much work it is",
        ),
    ];
}

/// How much of the conversation before it the turn needs.
///
/// The axis a cache-aware selector reads: a self-contained turn can move to a
/// cold target for a fraction of what moving a history-dependent one costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextDependence {
    /// Answerable from the request alone.
    SelfContained,
    /// Needs the last few exchanges.
    Recent,
    /// Needs material from much earlier in the session.
    Deep,
    /// The classifier could not tell.
    Unknown,
}

impl ClassificationAxis for ContextDependence {
    const KEY: &'static str = "context_dependence";
    const INSTRUCTIONS: &'static str = "How much of the earlier conversation does this turn need?";
    const OPTIONS: &'static [(Self, &'static str, &'static str)] = &[
        (
            Self::SelfContained,
            "self_contained",
            "The request can be answered from itself, with no earlier exchange",
        ),
        (
            Self::Recent,
            "recent",
            "The request continues the last few exchanges and needs them",
        ),
        (
            Self::Deep,
            "deep",
            "The request depends on material from much earlier in the session",
        ),
        (
            Self::Unknown,
            "unknown",
            "There is too little here to tell what earlier material is needed",
        ),
    ];
}

/// One axis's answer and the statistic the classifier published about it.
///
/// `confidence` is an observation about a distribution and never a measure of
/// answer quality — the service publishes no calibration evidence for it, so a
/// caller may gate on it and may not report it as confidence in an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Graded<T> {
    pub value: T,
    pub confidence: f64,
}

/// Every axis of one turn, under the taxonomy that produced them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TurnClassification {
    /// [`TAXONOMY_VERSION`] as of the process that wrote this record.
    pub taxonomy_version: u32,
    pub intent: Graded<TurnIntent>,
    pub complexity: Graded<TurnComplexity>,
    pub context_dependence: Graded<ContextDependence>,
}

/// What a classifier call billed, as the service reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Whether the ledger accepted this call's settle.
///
/// **Separate from the usage beside it, because they are separate facts.**
/// A spend known only as *submitted* would read, from every consumer, as
/// money committed even when the ledger rejected the settle. Reported usage
/// is what the service says it billed, and this is what this deployment's
/// own accounting did about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementAck {
    /// The ledger applied the settle and released the hold.
    Committed,
    /// No acknowledgement was received: the ledger refused, could not be
    /// reached, or did not answer inside the call's deadline. The hold is left
    /// to lapse on its TTL.
    ///
    /// **Absence of an acknowledgement, not proof of absence** — named for
    /// what this process knows rather than for what it guesses the backend
    /// did: a settle that was submitted and then abandoned — a timeout, a
    /// cancelled worker, a connection dropped after the write — may well have
    /// been applied, and nothing this process holds tells that apart from one
    /// that never landed. So this must never be read as a rollback or as a
    /// statement that the hold was released.
    ///
    /// [`ClassificationSettlementRepair`] is what resolves the ambiguity, and
    /// it resolves it by asking the ledger rather than by choosing a reading.
    Unconfirmed,
}

/// What one classifier call cost, and what became of that number.
///
/// `granted_usd` rides here rather than on the intent, and that is the whole of
/// "an intent records a quote, never a grant": the hold is opened by the
/// background worker, so at the moment the intent is written no ledger has said
/// anything and a number in that field would be an invention.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvaluationSpend {
    /// Priced from the usage the service reported.
    Measured {
        usage: EvaluationUsage,
        usd: f64,
        /// What the ledger actually held for this call.
        granted_usd: f64,
        settled: SettlementAck,
    },
    /// Usage was absent or incomplete. A release at zero was *submitted*, which
    /// is how a hold is released and not a claim that the call was free — and
    /// whether it landed is [`SettlementAck`]'s to say, not this arm's. See
    /// [`SettlementAck::Unconfirmed`].
    Unknown {
        granted_usd: f64,
        settled: SettlementAck,
    },
}

impl EvaluationSpend {
    /// What this deployment committed, and `None` where nobody can say.
    ///
    /// `Measured` with an unconfirmed settle answers `None` on purpose: the
    /// call billed, and nothing this deployment holds confirms the charge was
    /// committed. `None` is "nobody here can say", not "the charge is not
    /// there" — see [`SettlementAck::Unconfirmed`].
    pub fn committed_usd(&self) -> Option<f64> {
        match self {
            Self::Measured {
                usd,
                settled: SettlementAck::Committed,
                ..
            } => Some(*usd),
            _ => None,
        }
    }

    /// What a repair must re-drive to the ledger, or `None` where there is
    /// nothing to resolve.
    ///
    /// **The amount the record holds, never one derived here.** A repair that
    /// re-priced would charge a historical call at the live rate card, and the
    /// ledger and the log would then disagree about a finished turn with no
    /// reader able to say which was right. There is one pricing authority for a
    /// settled call and it is the record this reads.
    ///
    /// The zero on the [`Self::Unknown`] arm is a *release* and not a price:
    /// the service billed an amount nobody here can name, so the hold is handed
    /// back and the record goes on saying the accounting is unknown. Answering
    /// a measured zero instead would book a billed call as free, which is the
    /// one accounting lie this vocabulary exists to prevent.
    pub fn unconfirmed_settlement_usd(&self) -> Option<f64> {
        match self {
            Self::Measured {
                usd,
                settled: SettlementAck::Unconfirmed,
                ..
            } => Some(*usd),
            Self::Unknown {
                settled: SettlementAck::Unconfirmed,
                ..
            } => Some(0.0),
            _ => None,
        }
    }

    pub fn settled(&self) -> SettlementAck {
        match self {
            Self::Measured { settled, .. } | Self::Unknown { settled, .. } => *settled,
        }
    }

    /// What the ledger held before the call was made.
    pub fn granted_usd(&self) -> f64 {
        match self {
            Self::Measured { granted_usd, .. } | Self::Unknown { granted_usd, .. } => *granted_usd,
        }
    }
}

/// Why a call this deployment committed to never reached the service.
///
/// **Durable, and distinct from silence.** Each of these is a fact the
/// background worker established *after* the intent was written and *before*
/// any HTTP: nothing was sent, so the service billed nothing. An intent with no
/// result at all is the other thing — a process that died — and it stays
/// unknown.
///
/// **About the classifier, not about the evaluation ledger.** "Nothing was
/// bought" is the whole of what these say. Two of them are reached with a
/// ledger call already in flight — an [`Self::Expired`] taken inside the grant,
/// and a [`Self::BudgetRefused`] whose zero-dollar release ran out of time —
/// and neither can claim the backend left no hold behind. Whatever hold exists
/// lapses on the TTL the intent's [`ReservationRecord::hold_ttl_ms`] asked for.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FundingRefusal {
    /// The evaluation budget granted less than the call was quoted at. A
    /// partial reservation buys nothing: the prompt is written and its price is
    /// not negotiable downwards.
    BudgetRefused {
        requested_usd: f64,
        granted_usd: f64,
    },
    /// The evaluation ledger could not be reached. Fail closed — a call spent
    /// against a ceiling nobody could confirm is an unbudgeted call.
    LedgerUnavailable,
    /// The call's absolute expiry passed while it waited to run.
    Expired,
}

/// Which classifier, asked under which vocabulary, configured by which file.
///
/// Every field is an identity a later reader needs to decide whether two records
/// are comparable. A digest of the four would say that two turns differed and
/// never how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifierIdentity {
    /// The configured model identifier. The service-reported identity is recorded separately.
    pub model: String,
    /// The wire schema the request was made under.
    pub schema: String,
    /// [`TAXONOMY_VERSION`] at the time of the call.
    pub taxonomy_version: u32,
    /// [`projection::PROJECTION_REVISION`] at the time of the call.
    pub projection_revision: u32,
    /// The deployment's own configuration revision, as the operator set it.
    pub config_revision: u32,
}

/// The terms a call will be held under, in numbers rather than in a digest.
///
/// **A quote and a ceiling, never a grant.** The hold is opened by the
/// background worker, so at the moment this is written no ledger has been asked
/// anything — see [`ClassificationIntent`] on why that is the ordering. What was
/// actually granted arrives with the result, on
/// [`EvaluationSpend::granted_usd`].
///
/// **A digest is not enough here and that is the whole reason this type has
/// fields.** Later accounting has to answer "what was this allowed to cost, and
/// against which ceiling" from the log alone, long after the configuration that
/// produced those numbers has been edited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReservationRecord {
    /// The rate card this call was quoted and priced under.
    pub rate_card: ProviderPricing,
    /// What this deployment's tokenizer made of the exact request bytes.
    pub estimated_input_tokens: u64,
    pub expected_output_tokens: u64,
    /// The quote the hold will be asked for.
    pub requested_usd: f64,
    /// How long the hold will be asked to survive.
    pub hold_ttl_ms: u64,
    /// The project ceiling in force, and the window it resets on.
    pub budget_limit_usd: f64,
    pub budget_window: BudgetWindow,
    /// This membership's own ceiling, where it has one.
    pub member_ceiling_usd: Option<f64>,
    /// The fraction of a ceiling past which the ledger warns.
    pub warn_at: f64,
}

/// A classifier call this deployment has committed to making.
///
/// **Written before any HTTP and before any ledger call, and never replayed
/// into a second call.** Both halves of that ordering are load-bearing and for
/// different reasons. Before HTTP, so a process which dies mid-call leaves
/// evidence that a third party may have been paid rather than leaving nothing.
/// Before the *ledger*, so that writing it costs the serving turn no evaluation
/// I/O at all: the grant is a round trip to a store the turn does not otherwise
/// touch, and awaiting one here would make the classifier's budget a dependency
/// of every response's latency. A replay reads this record and learns that the
/// answer is unknown, which is cheaper than buying the answer again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassificationIntent {
    /// The identity the hold is taken under and the settle is deduplicated by.
    ///
    /// Fresh per external attempt: a settled identity can never settle again, so
    /// a retry would need a new one. Replay reuses the original, which is what
    /// stops a re-driven settle charging twice.
    pub call_id: ResponseId,
    /// The turn this classification is *about*.
    pub source_turn_index: u64,
    pub source_response_id: ResponseId,
    pub requested_at_ms: u64,
    /// Absolute, not a duration: queue wait and HTTP both bind against one
    /// instant, so a call that waited cannot then be given a full deadline.
    pub expires_at_ms: u64,
    pub identity: ClassifierIdentity,
    pub reservation: ReservationRecord,
}

/// How one classifier call ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClassificationOutcome {
    /// A complete, valid answer set.
    Classified {
        classification: TurnClassification,
        spend: EvaluationSpend,
        /// The service-reported identity, separate from the requested
        /// [`ClassifierIdentity::model`]. Unknown metadata stays `None`.
        /// Only outcomes with a received envelope can carry this field.
        #[serde(default)]
        reported_model: Option<String>,
    },
    /// An envelope arrived and its answers cannot be used. Its accounting
    /// survives, which is the whole reason usage sits outside the answers.
    ///
    /// **Not a classification of any kind**, including not `unknown` on every
    /// axis: nobody answered, so nothing was said about this turn.
    Unusable {
        /// The transport's own diagnosis, as a stable short token.
        reason: String,
        spend: EvaluationSpend,
        /// The service-reported identity survives unusable answers, so later
        /// analysis can identify the model that produced them.
        #[serde(default)]
        reported_model: Option<String>,
    },
    /// No envelope came back. The worker submitted a release at zero.
    /// [`SettlementAck`] records whether the ledger acknowledged it.
    Failed {
        reason: String,
        spend: EvaluationSpend,
    },
    /// Nothing was sent, and this deployment can say so rather than guess.
    ///
    /// The budget refused, the ledger was unreachable, or the call's life ran
    /// out in the queue — all three established before any socket. Carries no
    /// [`EvaluationSpend`] because there is no spend: not an unknown one, none.
    Unfunded { reason: FundingRefusal },
}

impl ClassificationOutcome {
    pub fn classification(&self) -> Option<&TurnClassification> {
        match self {
            Self::Classified { classification, .. } => Some(classification),
            _ => None,
        }
    }

    /// What the call cost, or `None` where no call was made.
    ///
    /// **`None` is "nothing was sent", not "the cost is unknown".** An
    /// [`EvaluationSpend::Unknown`] is the second statement and is a `Some`.
    pub fn spend(&self) -> Option<&EvaluationSpend> {
        match self {
            Self::Classified { spend, .. }
            | Self::Unusable { spend, .. }
            | Self::Failed { spend, .. } => Some(spend),
            Self::Unfunded { .. } => None,
        }
    }

    /// What this deployment committed for this call, and `None` where nobody
    /// can say or nothing was sent.
    pub fn committed_usd(&self) -> Option<f64> {
        self.spend().and_then(EvaluationSpend::committed_usd)
    }
}

/// What one classifier call produced, joined back to the turn it was about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassificationRecord {
    /// The [`ClassificationIntent::call_id`] this settles.
    pub call_id: ResponseId,
    pub source_turn_index: u64,
    pub source_response_id: ResponseId,
    pub completed_at_ms: u64,
    pub outcome: ClassificationOutcome,
}

/// One unconfirmed settlement, resolved.
///
/// **The smallest record that closes the question, and deliberately no
/// larger.** It carries no amount, no principal and no window: all three are
/// already on the intent and the result this names, and a second copy would be
/// a second pricing authority for one call — two numbers that can drift, with
/// nothing able to say which the ledger actually moved.
///
/// **It is an acknowledgement and never a re-classification.** It does not
/// touch the [`ClassificationRecord`] beside it, does not become an
/// [`AvailableClassification`], and does not change when any classification
/// became available to a decision. A repair is a fact about this deployment's
/// accounting; the answer the classifier gave is unchanged by it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassificationSettlementRepair {
    /// The [`ClassificationIntent::call_id`] whose settlement this resolves.
    pub call_id: ResponseId,
    /// What the ledger answered.
    ///
    /// **`false` is a success.** It means the ledger already held this call's
    /// settle — the first attempt had applied after all, and only its
    /// acknowledgement was lost — which is exactly the ambiguity the repair
    /// exists to resolve. Reading it as a failure and trying again is how one
    /// call comes to be charged twice.
    pub applied: bool,
    pub repaired_at_ms: u64,
}

/// A settlement the log says nobody has confirmed, and everything needed to
/// re-drive it.
///
/// **Derived, never durable.** Every field is folded back out of the intent and
/// the result the log already holds, which is what stops this being a second
/// record of a settled call that a later edit could put out of step with the
/// first. It exists so the repair path takes one value rather than re-deriving
/// the join between an intent and its result at each of its callers.
#[derive(Debug, Clone, PartialEq)]
pub struct UnconfirmedSettlement {
    /// The identity the hold was taken under and the settle deduplicates by.
    pub call_id: ResponseId,
    /// The amount the durable result recorded. See
    /// [`EvaluationSpend::unconfirmed_settlement_usd`].
    pub usd: f64,
    /// The window the *intent* recorded, so a repair lands against the same
    /// kind of period the grant was judged under rather than against whatever
    /// the configuration says now.
    pub window: BudgetWindow,
}

/// A classification a decision could see, named rather than copied.
///
/// **A reference, because the record is immutable and the log already holds
/// it.** Copying the labels onto the routing snapshot would put a second copy in
/// the log that a later correction could not reach, and reading them back from
/// mutable history during replay would let a build re-interpret a decision the
/// original never saw. The pair `(call_id, available_seq)` names exactly one
/// event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassificationRef {
    pub call_id: ResponseId,
    /// The turn the classification describes.
    pub source_turn_index: u64,
    /// The log sequence its result landed at.
    ///
    /// **Availability, not source.** A classification of turn 3 that arrives
    /// during turn 9 becomes usable at turn 9's sequence and not before, so a
    /// decision taken at turn 5 cannot name it and no backdating is possible.
    pub available_seq: u64,
}

/// A classification that has landed in the log, with the sequence it landed at.
#[derive(Debug, Clone, PartialEq)]
pub struct AvailableClassification {
    pub reference: ClassificationRef,
    pub classification: TurnClassification,
}

/// What a decision could see, bounded, with the bound stated.
///
/// **A window rather than a list, because a list is quadratic.** Naming every
/// classification a session had ever produced on every `Routed` record makes the
/// durable log grow with the square of the turn count — a hundred-turn session
/// writes a hundred references on its last decision and five thousand across the
/// session, for a feature set that only ever reads the newest few.
///
/// **These are recorded as *available*, not as consumed.** No selector shipping
/// today reads a classification; the routing policies are unchanged. What this
/// says is "these had landed by the cutoff", which is what a later learner needs
/// in order to reconstruct the feature set a decision was taken under, and it
/// would be false to read it as "this decision used them".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassificationWindow {
    /// [`projection::PROJECTION_REVISION`] at the time the window was taken —
    /// the same revision that decides which of these reach a classifier.
    pub revision: u32,
    /// The log sequence the window was taken at. Nothing above it is nameable.
    pub cutoff_seq: u64,
    /// The most references this window may carry.
    pub window: usize,
    /// How many had landed by the cutoff, whether or not they are named below.
    ///
    /// **The difference between this and `named.len()` is omission, and it is
    /// recorded rather than left to be inferred.** A reader that saw four
    /// references and no count could not tell a session with four
    /// classifications from one with four hundred.
    pub available: usize,
    /// The newest [`Self::window`] of them, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub named: Vec<ClassificationRef>,
}

impl ClassificationWindow {
    /// The newest `window` of `available`, at `cutoff_seq`.
    ///
    /// An exact-size, reversible iterator avoids collecting the full history.
    /// Only the newest references are cloned, then restored to chronological order.
    pub fn of<'a>(
        revision: u32,
        cutoff_seq: u64,
        window: usize,
        available: impl DoubleEndedIterator<Item = &'a ClassificationRef> + ExactSizeIterator,
    ) -> Self {
        let count = available.len();
        // Newest first on the way out, oldest first on the way in: `named` is
        // chronological because a reader of the record reconstructs a sequence
        // of features from it, not a stack.
        let mut named: Vec<ClassificationRef> = available.rev().take(window).cloned().collect();
        named.reverse();
        Self {
            revision,
            cutoff_seq,
            window,
            available: count,
            named,
        }
    }
}

/// What this deployment's own extractor made of one earlier turn.
///
/// **Local metadata, which is half of what the owner's direction permits**:
/// "bounded prior-turn metadata and classifications, together with the current
/// user prompt" (`PLAN-routing-strategy-bandit.md`, 2026-09-21). A projection
/// carrying only prior *classifications* leaves a tool-continuation turn with
/// almost nothing to describe, because that turn has no prompt of its own and
/// its predecessors may not have been classified yet.
///
/// Counts and one heuristic flag, from records this deployment already wrote.
/// No prompt text, no tool arguments or results, no instructions, no target and
/// no price — the same exclusions the projection applies to the current turn.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PriorTurnMetadata {
    pub turn_index: u64,
    /// The extractor revision these counts were produced by. A later build that
    /// reinterprets the same exchanges is a different number, so two turns
    /// counted differently are never read as one series.
    pub extractor_revision: u32,
    /// Task exchanges in the session at that turn.
    pub turn_depth: u32,
    pub edit_count: u32,
    pub read_count: u32,
    /// Worst recent tool-result severity, `0.0..=1.0`.
    pub severity: f32,
    /// **A heuristic, and labelled as one wherever it is rendered.** It means
    /// "a result in the recent window looked like a passing test run", which is
    /// a guess about text and never a verdict about correctness.
    pub tests_passed_heuristic: bool,
}
