// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The session event log.
//!
//! Every observable thing a session does becomes an event with a monotonic
//! `seq`. A client that reconnects presents the last `seq` it saw and the
//! server replays forward from there, which is the same mechanism the Responses
//! API exposes as `starting_after`. Because the log is the source of truth
//! rather than a side-channel, a partially generated response survives the
//! death of the process that was generating it.

use serde::{Deserialize, Serialize};

use crate::control::Principal;
use crate::ids::{ResponseId, SessionId, SideCallId, TurnId, ValidationId};
use crate::item::Item;
use crate::routing::{DecisionRecord, DispatchAttempt, Target};
use crate::validate::{Arm, SteerAction, TriggerRecord, Verdict};

/// Token accounting for one completed model call.
///
/// Three of these five counts are *components* of the other two rather than
/// additions to them: `cached_input_tokens` and `cache_write_tokens` are each
/// part of `input_tokens`, and `reasoning_tokens` is part of `output_tokens`.
/// Both providers Roundhouse targets report them that way — OpenAI nests them
/// under `input_tokens_details` / `output_tokens_details`, and Anthropic bills
/// thinking as ordinary output — so storing them as separate addends would
/// double-count every total downstream, including the one billed to a client.
///
/// Anthropic is the exception that proves the rule and the reason
/// `cache_write_tokens` exists: on *its* wire the three input counters are
/// disjoint, and the client that speaks it converts them into these axes once,
/// at the decoder. By the time a `Usage` exists the conversion has happened.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    /// Portion of `input_tokens` served from a prefix cache.
    ///
    /// Locally this is derived from the scheduler's cache credit; for frontier
    /// providers it is whatever the provider reports. It is the number the
    /// whole design exists to maximize.
    pub cached_input_tokens: u64,
    /// Portion of `input_tokens` the provider *wrote* into its cache.
    ///
    /// **A measurement, and only ever a measurement.** Roundhouse already
    /// *prices* every uncached input token at the cache-write rate — a
    /// deliberate conservative approximation in `routing::ledger` — and three
    /// separate surfaces document the gap this field closes: the Responses
    /// wire's hardcoded `"cache_write_tokens": 0`, the relay summary's
    /// deliberately-absent field whose doc says it awaits a measurement, and the
    /// ledger's own note. A field named for a measurement must never be filled
    /// from a pricing convention, so it stays zero on every dialect that does
    /// not report one rather than being back-derived from `uncached_input`.
    ///
    /// `#[serde(default)]` because the durable log already holds entries written
    /// before this field existed, and they must keep deserializing. Zero is also
    /// the right reading for them: at the time they were written the only
    /// routable dialects reported no cache write at all.
    #[serde(default)]
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    /// Portion of `output_tokens` spent on reasoning the client never sees.
    ///
    /// Zero for models without a thinking mode, which is why it carries a
    /// serde default: logs written before this field existed deserialize as
    /// "no reasoning" rather than failing, and that reading is correct for
    /// every model that was routable at the time.
    #[serde(default)]
    pub reasoning_tokens: u64,
    /// Whether these counts came from the provider or from our own tokenizer.
    ///
    /// Load-bearing for the metrics layer rather than diagnostic. A streaming
    /// OpenAI-compatible endpoint reports no usage at all unless the request
    /// asked for it, and an unreported call folded into a rollup as zero
    /// tokens and zero dollars is indistinguishable from a saving — the
    /// dashboard would look its best exactly when its instrumentation was
    /// broken. Marking the call keeps that gap visible as a gap.
    #[serde(default)]
    pub accounting: Accounting,
}

/// Where a [`Usage`]'s counts came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accounting {
    /// The provider (or, locally, the scheduler) reported them.
    ///
    /// The default, and the right reading for every log written before this
    /// field existed: usage was only ever recorded from a provider's own
    /// accounting chunk, so a record that predates the field was reported.
    #[default]
    Reported,
    /// The provider returned no usage and these are Roundhouse's own counts.
    ///
    /// Input is trustworthy — it is the prompt we tokenized and routed on —
    /// and output is a tokenization of what we received. Cached input is not
    /// estimated at all but left at zero, because no local evidence bears on
    /// what a remote cache did, and guessing high would inflate the one number
    /// this whole system is judged by.
    Estimated,
}

impl Usage {
    /// Billable tokens for this call.
    ///
    /// Cached input and reasoning output are deliberately absent: they are
    /// already inside the two terms. See the type's own note.
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Prompt tokens that were not served from cache, and so had to be
    /// prefilled. The complement of the quantity the routing optimizes for.
    pub fn uncached_input_tokens(&self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_input_tokens)
    }

    /// Output tokens the client actually received, i.e. excluding reasoning.
    pub fn visible_output_tokens(&self) -> u64 {
        self.output_tokens.saturating_sub(self.reasoning_tokens)
    }

    /// Accumulate another call into this one.
    ///
    /// Saturating rather than wrapping: a metrics rollup that wrapped at
    /// `u64::MAX` would report a near-zero total for the busiest deployment on
    /// the fleet, which is the one case where the number matters most.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        // Provenance degrades on contact: a total that mixes reported and
        // estimated calls is an estimate, and rounding that up to "reported"
        // would launder exactly the uncertainty this field exists to carry.
        if other.accounting == Accounting::Estimated {
            self.accounting = Accounting::Estimated;
        }
    }
}

/// Why a response stopped short.
///
/// **Adding a variant is a one-way door, and that is a decision rather than an
/// oversight.** Every other growth in this file — [`Usage::reasoning_tokens`],
/// `SessionCreated::principal` — carries a serde default so that a *new* build
/// keeps reading an *old* log. This enum's compatibility runs the other way:
/// an old build cannot read a log containing a variant it has never heard of,
/// so a rollback past the release that added `policy_refused` fails to
/// deserialize any session that recorded one. That is accepted, not
/// mitigated. The alternatives are an `Other(String)` catch-all, which turns
/// every unknown reason into a shrug the surfaces then have to translate
/// anyway, or never naming a new reason at all — which is how a refusal ends
/// up filed as an upstream error forever. A reason is a small, closed,
/// operator-facing vocabulary; a rollback across a vocabulary change is a
/// migration, and pretending otherwise is what would make it a silent one.
///
/// `budget_exhausted` is the second variant added under that posture, and it
/// pays for itself the same way the first did: three terminal refusals, three
/// systems. [`Self::PolicyRefused`] names the control-plane file,
/// [`Self::BudgetExhausted`] names the budget, and [`Self::UpstreamError`]
/// names the fleet. Collapsing any two of them sends an operator to the wrong
/// one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteReason {
    /// The owning process lost its lease or died mid-generation.
    ///
    /// The partial output is already durable in the log, so a successor can
    /// resume from it rather than restarting the turn.
    OwnerLost,
    MaxOutputTokens,
    ClientCancelled,
    UpstreamError,
    /// No target the turn's principal may use was admissible.
    ///
    /// Distinct from [`Self::UpstreamError`] because no upstream was contacted:
    /// nothing was dispatched, there is no partial to resume from, and the
    /// cache ledger learns nothing about any target. Calling it an upstream
    /// error would blame a provider for a decision this deployment made, and
    /// an operator reading the log would go looking at the wrong system.
    ///
    /// It is also the one terminal reason a retry cannot fix on its own: the
    /// same turn under the same policy refuses again, and only an operator
    /// widening the policy changes the answer. Surfaces that speak a dialect
    /// with a separate "could not be served" terminal render it as that rather
    /// than as a truncated answer — see `responses_api`.
    PolicyRefused,
    /// The project's budget is spent and it is configured to refuse rather than
    /// degrade to its own fleet.
    ///
    /// Distinct from [`Self::PolicyRefused`] on the axis that decides what
    /// anybody does next. A policy refusal is *terminal for this turn under
    /// this configuration* — the same turn refuses again forever, and only a
    /// widened policy moves it. A budget refusal is a limit an admin can raise,
    /// and it lifts on its own at the next monthly boundary, so the turn stays
    /// retryable and a client that backs off and retries is behaving correctly
    /// rather than hammering a wall.
    ///
    /// It is also distinct from a *degraded* turn, which is not an incomplete
    /// response at all: degrade-to-local serves the turn, and the fact that its
    /// budget was spent is recorded on the
    /// [`DecisionRecord`](crate::routing::DecisionRecord) rather than as a
    /// terminal reason. Only `Exhaustion::Refuse` reaches here.
    BudgetExhausted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventKind {
    /// The first fact in a session's log: which policy serves it and who pays.
    ///
    /// Emitted once, when the log is empty, by the one caller that already
    /// holds the lease — so it is race-free and idempotent by construction, a
    /// log being empty exactly once. Everything downstream that needs to know
    /// whose turn this was reads it from here rather than from a side table: a
    /// replay starts at seq 0, so every fold sees this before the first event
    /// that costs money.
    SessionCreated {
        model_policy: String,
        /// The membership this session was opened for.
        ///
        /// `None` in exactly one case, and it is not "unknown": a log written
        /// before the control plane existed, when there was nobody to record.
        /// Everything that writes this field writes a principal, so the absent
        /// case can only ever mean "older than tenancy" — which is why the
        /// fold gives it a marked row of its own instead of guessing a project
        /// (see [`PrincipalKey`](crate::control::PrincipalKey)).
        ///
        /// Carries a serde default for the same reason
        /// [`Usage::reasoning_tokens`] does: history has to keep deserializing
        /// after the type grows, or an upgrade silently costs a deployment its
        /// past.
        #[serde(default)]
        principal: Option<Principal>,
        /// Which arm of the validate experiment this session belongs to.
        ///
        /// **Stamped, not recomputed**, and that is the whole reason it is a
        /// log field. Assignment is `hash(session_id, arm_salt)` — deterministic,
        /// because a random draw would break fold-equals-log on replay — but
        /// the salt is *configuration*, and an operator edits configuration.
        /// A recomputed arm would silently re-assign every historical session
        /// the day the salt moved, and the arm comparison the whole
        /// instrumentation exists for would be computed across a boundary
        /// nobody recorded.
        ///
        /// `None` in exactly one case, and it is not "unknown": a log written
        /// before the experiment existed, when there was no arm to record. A
        /// session that is not enrolled is not validated, which is the honest
        /// reading — guessing an arm for it would be inventing a control group
        /// out of history.
        ///
        /// **The serde default is a one-way door, as
        /// [`Self::SessionCreated::principal`]'s was.** A new build reads an
        /// old log; an old build reading a *new* log ignores the field
        /// entirely, which for this field means it reads an enrolled session as
        /// unenrolled and simply does not validate it. That is the benign
        /// direction, and it is why this widening is spelled the same way the
        /// last one was rather than as a required field with a migration.
        #[serde(default)]
        arm: Option<Arm>,
    },
    /// A turn was admitted. Carries the client's idempotency key.
    TurnStarted {
        turn_id: TurnId,
        response_id: ResponseId,
    },
    /// An item was committed to the canonical conversation.
    ItemAppended {
        item: Item,
    },
    /// The routing layer chose a target. Emitted before any bytes are produced
    /// so the audit trail records the decision even if execution then fails.
    Routed {
        response_id: ResponseId,
        decision: DecisionRecord,
    },
    OutputTextDelta {
        response_id: ResponseId,
        text: String,
    },
    ResponseCompleted {
        response_id: ResponseId,
        usage: Usage,
        /// What the provider itself said this call cost, in its own dollars.
        ///
        /// **A sidecar, never an addend.** It sits beside `usage` rather than
        /// inside it because the two answer different questions: `usage` is
        /// tokens this deployment prices from its own catalog, and this is the
        /// other side of the reconciliation — the external bill our
        /// `committed_usd` is to be checked against. Folded into `Usage` it
        /// would put a number nobody derived from the rate card into the column
        /// the savings claim is computed from, and the drift figure that exists
        /// to surface the gap between the two would be computed against itself.
        ///
        /// Recorded here rather than on the `Routed` decision, and that is a
        /// fact about ordering rather than a preference: `Routed` is written
        /// before the dispatch is attempted, and the provider's price arrives
        /// on the stream's final frame. An event is immutable once committed,
        /// so the decision record cannot learn it. (Review finding G11: before
        /// this field the value was parsed, carried to the engine, and spent on
        /// a `tracing::debug!` the binary's own default `info` filter drops.)
        ///
        /// `None` for every provider that reports nothing, which is most of
        /// them, and for every log written before this field existed. Skipped
        /// on the wire when absent, so a deployment whose upstreams stay silent
        /// writes the bytes it wrote before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_reported_cost_usd: Option<f64>,
    },
    ResponseIncomplete {
        response_id: ResponseId,
        reason: IncompleteReason,
        usage: Usage,
        /// The dispatch that failed last, when this turn failed by exhausting
        /// its targets.
        ///
        /// **The one attempt with no successor `Routed` to ride on.** Every
        /// other failed dispatch of a turn is carried by
        /// [`DecisionRecord::attempts`] on the record of the dispatch it caused;
        /// the final one caused no further dispatch, so without this field it
        /// reached no projection at all — and a single-provider deployment in
        /// an outage reported *zero* failed attempts for as long as the outage
        /// lasted, which is exactly inverted from when the number matters
        /// (review finding G03).
        ///
        /// It is deliberately not merged into `usage`'s evidence rule. "Was
        /// there a failed attempt to attribute" and "was there billable usage
        /// to consume" are different questions, and the settle path's
        /// `input_tokens > 0` gate — which correctly keeps a dispatch that
        /// reached nobody out of the call-count denominator — was answering the
        /// first with the second.
        ///
        /// `None` when the turn failed for a reason that names no target (a
        /// refusal, a deadline before any dispatch, a body that died
        /// mid-stream), and for every log written before this field existed.
        /// Skipped on the wire when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_attempt: Option<DispatchAttempt>,
    },
    /// A turn was re-sent after reconnect and served from the existing result.
    TurnDeduplicated {
        turn_id: TurnId,
        response_id: ResponseId,
    },
    /// A model call this deployment made for its own purposes, and what it
    /// cost.
    ///
    /// **A money fact and only a money fact.** It says a call happened, to
    /// whom, why, and what it billed. It says nothing about what the answer was
    /// or what was done about it — that is [`Self::ValidationDecided`], and
    /// keeping the two apart is what lets a Shadow run (verdict computed,
    /// action discarded) be told from a Live one at the fold, which is the
    /// entire point of the instrumentation.
    SideCallCompleted {
        side_call_id: SideCallId,
        purpose: SideCallPurpose,
        target: Target,
        usage: Usage,
    },
    /// A side call that produced nothing usable, and cost an unknown amount.
    ///
    /// **Deliberately not a completion carrying an empty usage**, and the
    /// distinction is one this vocabulary is free to make where the old one was
    /// not. An empty-usage completion is indistinguishable from a *free* call;
    /// the `consumed` heuristic in the metrics fold exists precisely because
    /// the terminal-event vocabulary could not tell those apart, and a new kind
    /// that reproduced the ambiguity would be repeating a known mistake on
    /// purpose. An unaccounted call is marked, never free.
    ///
    /// Carries no usage for the same reason: what a timed-out or refused call
    /// billed upstream is exactly what this deployment does not know.
    SideCallAbandoned {
        side_call_id: SideCallId,
        purpose: SideCallPurpose,
        target: Target,
        reason: SideCallAbandonReason,
    },
    /// One consultation of the validate/steer loop, and what came of it.
    ///
    /// **A control fact and only a control fact.** No money is here; the side
    /// call it names carries that. Merging the two would make "what did
    /// checking cost" and "what did checking decide" one row, and the arm
    /// comparison needs them as two.
    ValidationDecided {
        validation_id: ValidationId,
        /// What roundhouse observed without a model call.
        ///
        /// Kept whole rather than reduced to "the trigger fired", because the
        /// question the experiment answers is "did acting on *this kind* of
        /// evidence help", and a log that recorded only the boolean cannot.
        trigger: TriggerRecord,
        arm: Arm,
        outcome: ValidationOutcome,
    },
    Error {
        message: String,
    },
}

/// Why this deployment made a call nobody asked it for.
///
/// An enum with one variant today, and an enum rather than a marker because the
/// *fold* splits on it: money spent on the deployment's own behalf is a
/// different row from money spent serving a turn, and the second purpose — a
/// compaction pass, a summarizer — arrives as a variant rather than as a second
/// event kind that has to be added to every exhaustive match again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideCallPurpose {
    /// The validate/steer loop's judge.
    Validate,
}

/// Why a side call produced nothing.
///
/// Three reasons naming three systems, on the same principle
/// [`IncompleteReason`] is split under: an operator reading one of these must
/// be sent to the right place. A deadline is this deployment's own budget
/// binding, unreachability is the network or the provider being down, and a
/// refusal is the provider answering — which is the one of the three that does
/// not get better by waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideCallAbandonReason {
    /// The side call's own deadline elapsed. The checker must not break the
    /// checked, so the deadline binds before the turn's does.
    DeadlineExceeded,
    /// Nothing answered.
    Unreachable,
    /// The provider answered, and the answer was a refusal.
    Refused,
}

/// What one validation came to.
///
/// **Invalid states are unrepresentable, and these two are the ones that
/// matter.** `NotRun` cannot carry a verdict: there is no field for one, so a
/// validation that never asked cannot be recorded as though it had an answer.
/// `Judged` cannot lack a side call: the id is required, so a verdict cannot
/// appear in the log with no call to account for what producing it cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ValidationOutcome {
    NotRun {
        reason: NotRunReason,
    },
    Judged {
        side_call_id: SideCallId,
        verdict: Verdict,
        /// What the action map made of the verdict.
        ///
        /// Recorded whether or not it was *taken* — in the Shadow arm it is
        /// computed and discarded, and the arm field beside it is what says
        /// which happened. A log that recorded only taken actions could not
        /// answer the counterfactual the Shadow arm exists to measure.
        action: SteerAction,
    },
}

/// Why a validation asked nobody.
///
/// A closed vocabulary, and every variant is a *different* thing for an
/// operator to do about it — which is the same test [`IncompleteReason`] is
/// split under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotRunReason {
    /// The budget could not cover a check.
    ///
    /// The turn proceeds. Never fail a turn because we could not afford to
    /// check it.
    BudgetRefused,
    /// This node has no review capacity, or has stopped asking after repeated
    /// failures.
    ReviewBudgetSpent,
    /// No judge could be reached at all; nothing was attempted and nothing was
    /// spent. Distinct from [`Self::JudgeFailed`], which had a target and a
    /// side call.
    JudgeUnavailable,
    /// A side call was made and did not answer usably. The companion
    /// [`SessionEventKind::SideCallAbandoned`] names the target and the reason.
    JudgeFailed,
    /// The judge answered and the answer was not the verdict schema.
    ///
    /// Its own reason rather than folded into [`Self::JudgeFailed`], because
    /// the two send an operator to different places: one is a transport or a
    /// provider, the other is a prompt or a model that has stopped honoring the
    /// schema. The money is still booked — a
    /// [`SessionEventKind::SideCallCompleted`] precedes this — because it was
    /// still spent.
    VerdictUnparseable,
    /// This arm consults nobody, by design.
    PlaceboArm {
        /// What the sham's deterministic timing came to on this turn.
        ///
        /// On the reason rather than beside the outcome, because there is no
        /// verdict here for an action to have been derived *from*: what the
        /// placebo arm did is a property of its timing, and its timing is what
        /// this variant is about. The control the whole experiment leans on is
        /// unreadable without it.
        timing: PlaceboTiming,
    },
}

/// What the placebo arm's timing came to on one turn.
///
/// **Three states rather than a boolean, because "nothing happened" has two
/// meanings here and the arm comparison needs them apart.** A turn the coin did
/// not select was never in the sham's exposed group at all; a turn it selected
/// and the channel refused to act on *was* — the exposure is real, only the
/// disruption is missing. A boolean forced those together, and the fold reading
/// it would either understate the control arm's exposure or report an
/// interruption on a turn nothing interrupted. Neither reading is one an
/// operator could correct after the fact, because the log would no longer carry
/// the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceboTiming {
    /// The timing did not select this turn. An ordinary turn.
    Quiet,
    /// The timing selected this turn and the sham interruption was taken.
    Intervened,
    /// The timing selected this turn and the channel does not act, so nothing
    /// was altered.
    ///
    /// [`SteerChannel::Off`](crate::validate::SteerChannel::Off) is the shipped
    /// default and it means observe — for every arm, not only for the one that
    /// routes through the action map. The measurement survives the refusal
    /// because the measurement is what `Off` still permits.
    Withheld,
}

/// Facts an interjector produced for the engine to commit.
///
/// **The log has exactly one writer**, and it is the session that holds the
/// lease. An occupant of the interjection seam that needed a fact recorded
/// therefore returns it rather than appending it — see
/// [`interject`](crate::interject) — and this is the shape it returns it in.
///
/// A type rather than a bare `Vec<SessionEventKind>` because of what it must
/// *not* be able to carry. The three kinds it accepts are money and control
/// facts, none of which touches the conversation; an
/// [`SessionEventKind::ItemAppended`] smuggled in here would put a second
/// writer on the conversation and fork every projection built from it. There is
/// no constructor that takes an arbitrary kind, so that is unrepresentable
/// rather than merely discouraged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ControlRecord {
    kinds: Vec<SessionEventKind>,
}

impl ControlRecord {
    /// Book a side call that answered.
    pub fn side_call_completed(&mut self, side_call_id: SideCallId, target: Target, usage: Usage) {
        self.kinds.push(SessionEventKind::SideCallCompleted {
            side_call_id,
            purpose: SideCallPurpose::Validate,
            target,
            usage,
        });
    }

    /// Mark a side call that did not.
    pub fn side_call_abandoned(
        &mut self,
        side_call_id: SideCallId,
        target: Target,
        reason: SideCallAbandonReason,
    ) {
        self.kinds.push(SessionEventKind::SideCallAbandoned {
            side_call_id,
            purpose: SideCallPurpose::Validate,
            target,
            reason,
        });
    }

    /// Record what one validation came to.
    pub fn validation_decided(
        &mut self,
        validation_id: ValidationId,
        trigger: TriggerRecord,
        arm: Arm,
        outcome: ValidationOutcome,
    ) {
        self.kinds.push(SessionEventKind::ValidationDecided {
            validation_id,
            trigger,
            arm,
            outcome,
        });
    }

    /// What the side calls in this record billed, together.
    ///
    /// The usage a completing interjection reports to the client. Reporting an
    /// empty usage instead would make this deployment's own dashboard exceed
    /// what clients were told they spent, which is the one direction an
    /// accounting error must never run.
    pub fn usage(&self) -> Usage {
        let mut total = Usage::default();
        for kind in &self.kinds {
            if let SessionEventKind::SideCallCompleted { usage, .. } = kind {
                total.add(usage);
            }
        }
        total
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    pub fn kinds(&self) -> &[SessionEventKind] {
        &self.kinds
    }

    pub fn into_kinds(self) -> Vec<SessionEventKind> {
        self.kinds
    }
}

/// Notified of every event a session commits.
///
/// Lives beside the events it observes rather than beside its first
/// implementer. The session state machine is the lower layer, and having it
/// import its own observation seam from the metrics module — a reporting
/// concern built on top of it — pointed the dependency backwards. Anything
/// else wanting to watch the log, an exporter or a tracer, hangs off this same
/// seam instead of growing a second one.
///
/// Called while the session holds its lease and before the commit returns, so
/// an implementation must not block or await. A few integer additions is the
/// budget.
///
/// Implementations must be idempotent by `(session, seq)`. A session feeds its
/// replay through here as well as its subsequent commits, so an observer
/// without that property double-counts every session opened more than once,
/// which is every session that takes more than one turn.
pub trait SessionObserver: Send + Sync + 'static {
    fn observe(&self, events: &[SessionEvent]);
}

/// A sealed log entry. `seq` is assigned by the store on append and is
/// contiguous and strictly increasing within a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub seq: u64,
    pub session_id: SessionId,
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: SessionEventKind,
}

impl SessionEvent {
    /// The response this event belongs to, if any.
    ///
    /// Used to project the session-wide log down to the per-response view the
    /// Responses API exposes.
    pub fn response_id(&self) -> Option<&ResponseId> {
        match &self.kind {
            SessionEventKind::TurnStarted { response_id, .. }
            | SessionEventKind::Routed { response_id, .. }
            | SessionEventKind::OutputTextDelta { response_id, .. }
            | SessionEventKind::ResponseCompleted { response_id, .. }
            | SessionEventKind::ResponseIncomplete { response_id, .. }
            | SessionEventKind::TurnDeduplicated { response_id, .. } => Some(response_id),
            // The three validate-loop kinds answer `None`, and that is what
            // keeps them off every wire. A surface projects one response's
            // events, so a kind with no response id is never claimed by one —
            // and a side call genuinely belongs to no response: nobody asked
            // for it, it emitted no item, and borrowing the turn's id would
            // make the client's stream carry this deployment's own bookkeeping.
            SessionEventKind::SessionCreated { .. }
            | SessionEventKind::ItemAppended { .. }
            | SessionEventKind::SideCallCompleted { .. }
            | SessionEventKind::SideCallAbandoned { .. }
            | SessionEventKind::ValidationDecided { .. }
            | SessionEventKind::Error { .. } => None,
        }
    }

    /// Whether this event ends its response.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            SessionEventKind::ResponseCompleted { .. }
                | SessionEventKind::ResponseIncomplete { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_events_are_recognized() {
        let done = SessionEvent {
            seq: 4,
            session_id: SessionId::new("s"),
            at_ms: 0,
            kind: SessionEventKind::ResponseCompleted {
                response_id: ResponseId::new("r"),
                usage: Usage::default(),
                provider_reported_cost_usd: None,
            },
        };
        assert!(done.is_terminal());

        let delta = SessionEvent {
            seq: 3,
            session_id: SessionId::new("s"),
            at_ms: 0,
            kind: SessionEventKind::OutputTextDelta {
                response_id: ResponseId::new("r"),
                text: "hi".into(),
            },
        };
        assert!(!delta.is_terminal());
        assert_eq!(delta.response_id(), Some(&ResponseId::new("r")));
    }

    #[test]
    fn a_log_written_before_the_control_plane_deserializes_with_no_principal() {
        // Byte-for-byte what `SessionCreated` serialized to before tenancy
        // existed. Such logs are still being replayed after an upgrade, and a
        // fold that refused to parse them would take the deployment's whole
        // history with it.
        let json = r#"{"type":"session_created","model_policy":"affinity"}"#;
        let kind: SessionEventKind = serde_json::from_str(json).unwrap();
        assert_eq!(
            kind,
            SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: None,
                arm: None,
            },
            "an absent principal is `None`, which the fold marks rather than guesses at"
        );

        // And the same field one widening later: an M1-through-M5 log, which
        // names a payer and knows nothing about arms. It has to keep reading
        // after the experiment ships, and it has to read as *unenrolled* rather
        // than as a session in some default arm — a control group invented out
        // of history is worse than no control group.
        let m5 = r#"{"type":"session_created","model_policy":"affinity","principal":{"project":"acme","user":"ada"}}"#;
        assert_eq!(
            serde_json::from_str::<SessionEventKind>(m5).unwrap(),
            SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: Some(Principal::new("acme", "ada")),
                arm: None,
            }
        );
    }

    /// **A usage record written before the cache-write count existed still
    /// reads, and reads as zero.**
    ///
    /// The durable log is the source of truth and it is replayed, not migrated.
    /// A `Usage` that refused to deserialize without the new field would take
    /// every deployment's whole billing history with it on upgrade — and the
    /// reading it gets has to be right as well as parseable: at the time these
    /// entries were written the only routable dialects reported no cache write
    /// at all, so "zero written" is what actually happened rather than a
    /// placeholder.
    #[test]
    fn a_usage_written_before_the_cache_write_count_existed_still_reads() {
        // Byte-for-byte what `Usage` serialized to before this widening.
        let json = r#"{"input_tokens":9512,"cached_input_tokens":9000,"output_tokens":64,"reasoning_tokens":0,"accounting":"reported"}"#;
        let usage: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.input_tokens, 9_512);
        assert_eq!(usage.cached_input_tokens, 9_000);
        assert_eq!(usage.cache_write_tokens, 0);
        assert_eq!(usage.accounting, Accounting::Reported);

        // The count is a *component* of `input_tokens`, exactly as the cached
        // count is, so widening changes no total anywhere.
        let written = Usage {
            cache_write_tokens: 500,
            ..usage.clone()
        };
        assert_eq!(written.total(), usage.total());
        assert_eq!(
            written.uncached_input_tokens(),
            usage.uncached_input_tokens(),
            "a cache write is not a second kind of input token"
        );

        // And it folds like every other count: saturating, and additive across
        // calls. A rollup that dropped it would report a fleet that never
        // writes a cache while paying the write premium on every turn.
        let mut total = written.clone();
        total.add(&written);
        assert_eq!(total.cache_write_tokens, 1_000);
        let mut saturating = Usage {
            cache_write_tokens: u64::MAX,
            ..Usage::default()
        };
        saturating.add(&written);
        assert_eq!(saturating.cache_write_tokens, u64::MAX);
    }

    #[test]
    fn three_refusals_name_three_systems() {
        // The blame vocabulary, pinned as wire strings because a surface
        // renders them and an operator greps them. Each one sends its reader to
        // a different place: the control-plane file, the budget, the fleet.
        for (reason, wire) in [
            (IncompleteReason::PolicyRefused, "\"policy_refused\""),
            (IncompleteReason::BudgetExhausted, "\"budget_exhausted\""),
            (IncompleteReason::UpstreamError, "\"upstream_error\""),
        ] {
            assert_eq!(serde_json::to_string(&reason).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<IncompleteReason>(wire).unwrap(),
                reason
            );
        }
        assert_ne!(
            IncompleteReason::BudgetExhausted,
            IncompleteReason::PolicyRefused,
            "collapsing the two would tell a client to widen a policy when the \
             fix is to raise a limit -- and would hide that this one lifts on \
             its own at the next window boundary"
        );

        // A budget refusal is a terminal log fact with no usage: nothing was
        // dispatched, so there is nothing to have consumed.
        let event = SessionEventKind::ResponseIncomplete {
            response_id: ResponseId::new("resp_1"),
            reason: IncompleteReason::BudgetExhausted,
            usage: Usage::default(),
            terminal_attempt: None,
        };
        assert_eq!(
            serde_json::from_str::<SessionEventKind>(&serde_json::to_string(&event).unwrap())
                .unwrap(),
            event
        );
    }

    #[test]
    fn session_created_round_trips_its_principal_and_its_arm() {
        let kind = SessionEventKind::SessionCreated {
            model_policy: "affinity".into(),
            principal: Some(Principal::new("acme", "ada")),
            arm: Some(Arm::Shadow),
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(
            json,
            r#"{"type":"session_created","model_policy":"affinity","principal":{"project":"acme","user":"ada"},"arm":"shadow"}"#
        );
        assert_eq!(
            serde_json::from_str::<SessionEventKind>(&json).unwrap(),
            kind,
            "attribution has to survive the round trip, or a replay reattributes \
             the spend; so does the arm, or a replay compares a session against \
             the wrong control"
        );
    }

    /// The three new kinds, pinned on the two properties that make them safe to
    /// add: they belong to no response, and their shapes make the states the
    /// design forbids unspellable.
    #[test]
    fn the_validate_loop_kinds_belong_to_no_response_and_end_nothing() {
        let target = Target::Frontier {
            provider: "anthropic".into(),
            model: "claude".into(),
        };
        let kinds = [
            SessionEventKind::SideCallCompleted {
                side_call_id: SideCallId::new("sc_1"),
                purpose: SideCallPurpose::Validate,
                target: target.clone(),
                usage: Usage {
                    input_tokens: 4_000,
                    output_tokens: 40,
                    ..Usage::default()
                },
            },
            SessionEventKind::SideCallAbandoned {
                side_call_id: SideCallId::new("sc_2"),
                purpose: SideCallPurpose::Validate,
                target,
                reason: SideCallAbandonReason::DeadlineExceeded,
            },
            SessionEventKind::ValidationDecided {
                validation_id: ValidationId::new("val_1"),
                trigger: TriggerRecord::new(4, 30_000, Vec::new()),
                arm: Arm::Shadow,
                outcome: ValidationOutcome::NotRun {
                    reason: NotRunReason::PlaceboArm {
                        timing: PlaceboTiming::Intervened,
                    },
                },
            },
        ];
        for kind in kinds {
            let event = SessionEvent {
                seq: 9,
                session_id: SessionId::new("acme/ada/main"),
                at_ms: 1_700_000_000_000,
                kind,
            };
            assert_eq!(
                event.response_id(),
                None,
                "a side call belongs to no response, which is what keeps it off \
                 every client's stream"
            );
            assert!(
                !event.is_terminal(),
                "and it ends nothing: a validation is not an answer to a turn"
            );
            // Round-trips, because a replay has to reconstruct it exactly for
            // the arm comparison to mean anything.
            let json = serde_json::to_string(&event.kind).unwrap();
            assert_eq!(
                serde_json::from_str::<SessionEventKind>(&json).unwrap(),
                event.kind
            );
        }
    }

    #[test]
    fn a_control_record_carries_only_facts_and_totals_only_what_was_spent() {
        let target = Target::Frontier {
            provider: "openai".into(),
            model: "gpt".into(),
        };
        let mut record = ControlRecord::default();
        assert!(record.is_empty());
        assert_eq!(
            record.usage(),
            Usage::default(),
            "an empty record cost nothing, and says so"
        );

        record.side_call_completed(
            SideCallId::new("sc_1"),
            target.clone(),
            Usage {
                input_tokens: 4_000,
                output_tokens: 40,
                ..Usage::default()
            },
        );
        // An abandoned call adds nothing to the total, because what it billed
        // upstream is exactly what this deployment does not know. Marked, not
        // guessed at.
        record.side_call_abandoned(
            SideCallId::new("sc_2"),
            target,
            SideCallAbandonReason::Unreachable,
        );
        record.validation_decided(
            ValidationId::new("val_1"),
            TriggerRecord::new(4, 30_000, Vec::new()),
            Arm::Live,
            ValidationOutcome::NotRun {
                reason: NotRunReason::JudgeFailed,
            },
        );

        assert_eq!(record.kinds().len(), 3);
        assert_eq!(record.usage().total(), 4_040);
        // No constructor takes an arbitrary kind, so nothing here can carry a
        // conversation item. That is the property, and this is the reminder
        // rather than the proof — the proof is that the code below does not
        // compile, which is what a private field buys.
        assert!(
            record
                .kinds()
                .iter()
                .all(|kind| !matches!(kind, SessionEventKind::ItemAppended { .. }))
        );
    }
}
