// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The validate/steer loop: deciding whether a turn is worth interrupting.
//!
//! This module is the occupant of the [`interject`](crate::interject) seam that
//! milestone M6 exists to install. Everything it does is either pure or one
//! network call:
//!
//! ```text
//! turn admitted ── Trigger::evaluate(&SessionState)          pure, no I/O
//!     │                    │
//!     │ no fire            │ fires
//!     ▼                    ▼
//!  Proceed              arm (stamped in SessionCreated)
//!  unchanged            ├─ Shadow  → judge runs, action discarded, all logged
//!                       ├─ Placebo → no judge; sham intervention on hashed timing
//!                       └─ Live    → judge → Verdict → map → action
//! ```
//!
//! **The occupant decides and the engine writes.** Nothing here appends to a
//! log: a [`Validator`] returns the facts it produced inside the
//! [`Interjection`], and the session that holds the lease commits them. One
//! writer is what makes an interjection's record atomic with the completion it
//! justifies, which closes the window between a decision and its realization.
//!
//! **A judge that cannot be reached releases the turn.** There is no error arm
//! anywhere on this path — not on the trait, not on the seam. The checker must
//! never break the checked, so every failure here is a logged fact plus
//! `Proceed`. What is *not* allowed is for the failure to be silent: a timed-out
//! validator is marked, never free.
//!
//! ## The review budget, in two places on purpose
//!
//! The advisor-gate mechanism this borrows reserves a review *before* the await
//! so concurrent work cannot overdraw, refunds a failed consult, and counts
//! that failure against a separate cap so a down judge neither drains the
//! budget nor hangs every turn. Here that splits across two owners, because
//! the two questions have different truth sources:
//!
//! - **Per session**, how many reviews one conversation may buy, and how much
//!   it must spend between them. That is a projection of the log —
//!   [`TriggerConfig`] reads it out of [`SessionState`] — so it needs no
//!   counter and survives a process restart exactly.
//! - **Per node**, how many consults may be outstanding at once and how many
//!   consecutive failures are tolerated. That is a fact about *concurrency*,
//!   which no single session's log can see, so it is [`ReviewBudget`]: two
//!   counters, reserved before the await, released on the way out.
//!
//! The failure counter is a breaker and not a fuse: once tripped it re-arms
//! after a quiet period and admits one probe, whose answer either clears the
//! streak or starts the cooldown again. The quiet period is measured on the
//! log's own timestamps like everything else here — see
//! [`ReviewBudget::reserve`] — because a node whose validator switched itself
//! off for good after three transient timeouts is indistinguishable, from
//! outside, from one where somebody turned the loop off.

pub mod arm;
pub mod brief;
pub mod control_call;
pub mod exchange;
pub mod handoff;
pub mod prompt;
pub mod tool_signals;
pub mod trigger;
pub mod verdict;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use async_trait::async_trait;

use crate::control::{BudgetTerms, Principal};
use crate::event::Usage;
use crate::event::{
    ControlRecord, NotRunReason, PlaceboTiming, SideCallAbandonReason, ValidationOutcome,
};
use crate::ids::{SessionId, SideCallId, ValidationId};
use crate::interject::{Interjection, InterjectionContext, Interjector};
use crate::item::Item;
use crate::routing::Target;

pub use arm::{Arm, ArmShares, placebo_intervenes};
pub use brief::{BriefConfig, BriefStep, Objective, ValidationBrief, trailing_user_request};
pub use control_call::{
    CONTROL_TOOL_DELIMITER, CONTROL_TOOL_NAMES, CONTROL_TOOL_NAMESPACE, ControlCallDialect,
    flat_control_call_name, is_control_call_on, is_flat_control_call, task_exchanges_on,
};
pub use exchange::{Exchange, exchanges, exec_exit_code, tool_output_body};
pub use handoff::{EXAMPLE_HANDOFF_NOTE, HANDOFF_MARKER, append_handoff_note};
pub use prompt::judge_system_prompt;
pub use tool_signals::{
    CRITICAL, DEFAULT_RECENT_WINDOW, ERROR_SEVERITY_THRESHOLD, ErrorSeverity, HARD,
    PURE_BASH_STREAK_LENGTH, PureBashStreak, ResultSeverity, SOFT, ToolSignals, classify_body,
    classify_result, recent_severities,
};
pub use trigger::{
    CostAnomaly, Evidence, NoProgressRepeat, PingPong, Signal, SignalFired, SignalKind,
    ToolFailureStreak, Trigger, TriggerConfig, TriggerRecord, default_signals,
};
pub use verdict::{
    ActionPolicy, Divergence, EscalationOverrides, SteerAction, SteerChannel, Verdict,
    VerdictParseError, map, render_steer_answer,
};

/// What the judge answered, and what asking cost.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeAnswer {
    /// The judge's reply, exactly as it arrived.
    ///
    /// Unparsed on purpose: parsing is [`Verdict::parse`]'s job and it is
    /// strict, so a client that pre-parsed would be a second, laxer parser in
    /// the position that matters most.
    pub raw: String,
    pub usage: Usage,
    /// Which model answered, so the money books under its own row.
    pub target: Target,
}

/// Why a consult produced nothing usable.
///
/// Three arms because the log has three reasons and they must not be guessable
/// from one another: an enum makes "abandoned against no target" — a phantom
/// row on the dashboard — unrepresentable, where the earlier
/// `{ target: Option<Target>, reason }` pair made it merely unlikely. Each arm
/// maps to exactly one [`NotRunReason`], which is why the mapping in
/// [`Validator::consult`] is a `match` with no defaulting.
#[derive(Debug, Clone, PartialEq)]
pub enum JudgeFailure {
    /// The budget could not cover the check, so nothing was attempted.
    ///
    /// The rule the whole side call is subject to: never fail a turn because we
    /// could not afford to check it. Nothing is booked, because nothing was
    /// spent — the check is the thing that did not happen.
    Unaffordable,
    /// No judge could be dialled at all: none configured, or none admissible.
    ///
    /// Distinct from [`Self::Abandoned`] because nothing was attempted and
    /// therefore nothing may be booked. Recording an abandoned call against a
    /// target nobody dialled would put a phantom row on the dashboard.
    Unavailable,
    /// A call was made against `target` and produced nothing usable.
    Abandoned {
        target: Target,
        reason: SideCallAbandonReason,
    },
}

/// What one side call is made under, and for whom.
///
/// The facts a judge implementation needs that are properties of *this turn*
/// rather than of the deployment: which conversation it is checking, what the
/// check is called, where in that conversation's log it is being made, who pays
/// for it, and what ceiling that payer is under. They travel as one struct
/// rather than as five arguments for the reason the server's admission does:
/// resolved together from one turn, they must not be recombinable across two —
/// one tenant's identity against another's ceiling has no compile-time answer
/// and no runtime symptom either.
///
/// **What is deliberately absent is a deadline.** The checker's deadline is a
/// bounded fraction of the turn's, which is deployment configuration the
/// implementation already holds; passing it here would let one caller hand a
/// judge a deadline longer than the turn it is checking.
#[derive(Debug, Clone, Copy)]
pub struct SideCall<'a> {
    /// The conversation being checked, and the *only* input to the side call's
    /// own cache key. See [`JudgeClient`] on why that key is
    /// `{session_id}#validate` and not the conversation's own.
    pub session_id: &'a SessionId,
    /// What this check is called, minted before it is made.
    ///
    /// **Before, so that one string names the check everywhere it appears.**
    /// The log books `SideCallCompleted` or `SideCallAbandoned` under it and a
    /// metered deployment holds the check's money under it, so an operator
    /// reconciling committed spend against the log joins on a field rather than
    /// on an assumption. Minted by the caller rather than returned by the judge
    /// because the money question is asked *first* — a hold has to be keyed
    /// before there is an answer to key it by.
    pub id: &'a SideCallId,
    /// Where in the checked session's log this check is being made.
    ///
    /// The turn's own position, read after its `TurnStarted` and before
    /// anything a check could cause — so it rises with every turn of the
    /// session and is the same number a replay would compute. That makes it the
    /// idempotency key a settle needs: a ledger keyed on `(session, seq)`
    /// requires one that only goes up, and a wall clock or a process-local
    /// counter would either regress across nodes or reset on restart, silently
    /// dropping a settle in both cases.
    pub at_seq: u64,
    pub principal: &'a Principal,
    /// The payer's ceiling, or `None` when the membership has no budget.
    ///
    /// `None` is "no ledger to ask", exactly as it is on the server's
    /// admission — never an unlimited budget, so an implementation cannot
    /// confuse a deployment that meters nothing with one that granted a great
    /// deal.
    pub budget: Option<&'a BudgetTerms>,
}

/// The one network call this loop makes.
///
/// Narrow on purpose: it takes two prompts and the three facts about *this
/// turn* that decide who the call is billed to and under what key, and answers
/// with one string. Everything else about the fleet — quotes, credentials,
/// rate cards, the deadline, the transport — stays on the far side of it and
/// this module stays testable with a struct. The four isolations the side call
/// needs (its own cache key, its own deadline, its own budget grant, and never
/// the cache ledger) are properties of the implementation, and they are stated
/// here because an implementor who does not know them will get all four wrong:
///
/// - **Its own cache key**, `{session_id}#validate`: distinct from the
///   conversation's, or a judge prompt cools the hit the router just priced,
///   yet stable across validations so the judge's own prefix warms and the
///   marginal cost of checking falls with use.
/// - **Its own deadline**, a bounded fraction of the turn's remaining budget.
///   The checker must never break the checked.
/// - **Its own budget question**, asked of the payer's own ledger. If the
///   budget cannot cover the check, the check does not happen and the turn
///   proceeds ([`JudgeFailure::Unaffordable`]) — never fail a turn because we
///   could not afford to check it. What the trait requires is that the budget
///   can refuse, that refusing costs the turn nothing, and that what a check
///   spends reaches the ledger afterwards: a budget that is only ever *read*
///   answers the same way on the first check and the thousandth, so it is not
///   a ceiling. [`SideCall::id`] and [`SideCall::at_seq`] are what an
///   implementation keys the hold and its settle by.
/// - **Never the cache ledger.** A judge prompt is not a prefix of the
///   conversation, and feeding it to the ledger would falsely warm that target
///   for the next real turn.
#[async_trait]
pub trait JudgeClient: Send + Sync + 'static {
    async fn consult(
        &self,
        side_call: &SideCall<'_>,
        system_prompt: &str,
        brief: &str,
    ) -> Result<JudgeAnswer, JudgeFailure>;
}

/// How much review one node may have in flight, and how much failure it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewLimits {
    pub max_in_flight: u32,
    /// Consecutive failures before this node stops asking.
    ///
    /// A circuit breaker, and the reason the failure counter is *separate* from
    /// the reservation: a judge that is down refunds every reservation it takes,
    /// so a single counter would show a node with all its capacity free
    /// cheerfully timing out every turn.
    pub max_consecutive_failures: u32,
    /// How quiet a tripped breaker must be before it admits one probe.
    ///
    /// The half that makes it a breaker rather than a kill switch. Only a
    /// [`Reservation::succeeded`] clears the streak, and the only way to a
    /// reservation is [`ReviewBudget::reserve`] — so without a re-arm the
    /// tripped counter blocks the one call that could clear it, and three
    /// transient failures end validation on this node for the life of the
    /// process. A cooldown rather than an immediate retry because the failure
    /// this guards against is a judge that is *down*: probing it every turn
    /// would pay a full deadline per turn to learn what the counter already
    /// says.
    pub breaker_cooldown_ms: u64,
}

impl Default for ReviewLimits {
    fn default() -> Self {
        Self {
            max_in_flight: 8,
            max_consecutive_failures: 3,
            breaker_cooldown_ms: 60_000,
        }
    }
}

/// The node-local half of the review budget: two counters and a re-arm clock.
#[derive(Debug)]
pub struct ReviewBudget {
    limits: ReviewLimits,
    in_flight: AtomicU32,
    consecutive_failures: AtomicU32,
    /// The log timestamp the breaker's cooldown runs from.
    ///
    /// Written by a failure, and moved forward by each probe the breaker
    /// admits so that two callers arriving in one cooldown window cannot both
    /// take one — a half-open breaker that admitted every concurrent caller
    /// would be no breaker at all for exactly the deployment that has enough
    /// traffic to need one.
    cooldown_from_ms: AtomicU64,
}

impl ReviewBudget {
    pub fn new(limits: ReviewLimits) -> Self {
        Self {
            limits,
            in_flight: AtomicU32::new(0),
            consecutive_failures: AtomicU32::new(0),
            cooldown_from_ms: AtomicU64::new(0),
        }
    }

    /// Take capacity for one consult, **before** the await.
    ///
    /// Compare-and-swap rather than fetch-add-then-check: the check-then-take
    /// version admits every concurrent caller that read the count before any of
    /// them wrote it, which is the overdraw this exists to prevent and which
    /// only appears under the load that makes it expensive.
    ///
    /// **`now_ms` is the log's clock, never the wall's.** The caller passes
    /// [`SessionState::last_event_at_ms`](crate::session::SessionState::last_event_at_ms)
    /// — the timestamp the store stamped on the turn already being decided —
    /// for the same reason [`Trigger::evaluate`] takes no clock: a replay must
    /// reach the decision the original process reached, and a breaker consulting
    /// `Instant::now()` inside the fold would make "was this session validated"
    /// depend on when anybody asked. The counters themselves are node-local and
    /// deliberately outside the fold; what has to be replay-stable is the
    /// *answer this call gives*, and a monotonic log timestamp gives it.
    pub fn reserve(&self, now_ms: u64) -> Option<Reservation<'_>> {
        if self.consecutive_failures.load(Ordering::Acquire) >= self.limits.max_consecutive_failures
        {
            // Half-open: after a quiet period, exactly one probe gets through.
            // Its `succeeded()` clears the streak and its `failed()` leaves the
            // breaker tripped and the cooldown running again from that failure.
            let since = self.cooldown_from_ms.load(Ordering::Acquire);
            if now_ms.saturating_sub(since) < self.limits.breaker_cooldown_ms {
                return None;
            }
            // Claiming the probe is the same compare-and-swap discipline the
            // capacity below uses, and for the same reason: whoever loses the
            // race waits out another cooldown rather than joining the winner.
            if self
                .cooldown_from_ms
                .compare_exchange(since, now_ms, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return None;
            }
        }
        let mut current = self.in_flight.load(Ordering::Acquire);
        loop {
            if current >= self.limits.max_in_flight {
                return None;
            }
            match self.in_flight.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Reservation {
                        budget: self,
                        at_ms: now_ms,
                    });
                }
                Err(seen) => current = seen,
            }
        }
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Acquire)
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Acquire)
    }
}

/// One consult's capacity, released when this is dropped.
///
/// A guard rather than a matching `release()` call, because the release has to
/// happen on every path out — including the one where the future is cancelled
/// mid-await, which is exactly what a timeout does. A leaked reservation is a
/// node that quietly stops validating and never says why.
#[derive(Debug)]
pub struct Reservation<'a> {
    budget: &'a ReviewBudget,
    /// The log time this reservation was taken at, carried so a failure dates
    /// itself rather than reading a clock the fold cannot reproduce.
    at_ms: u64,
}

impl Reservation<'_> {
    /// The consult answered. Clears the failure streak.
    pub fn succeeded(self) {
        self.budget.consecutive_failures.store(0, Ordering::Release);
    }

    /// The consult did not answer. Counts against the separate failure cap.
    ///
    /// The reservation itself is refunded by the drop below — a failed consult
    /// must not also cost capacity, or a slow judge would look like a busy one.
    ///
    /// The cooldown is dated from *this* failure rather than from the one that
    /// tripped the breaker, so a judge that keeps failing keeps the breaker
    /// open: a window anchored to the first failure would expire while the
    /// judge was still down and re-admit a probe every turn thereafter.
    pub fn failed(self) {
        self.budget
            .consecutive_failures
            .fetch_add(1, Ordering::AcqRel);
        self.budget
            .cooldown_from_ms
            .store(self.at_ms, Ordering::Release);
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.budget.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What *one membership* permits of the validate loop.
///
/// **The half of the configuration that is a tenancy decision**, and the reason
/// it is separate from [`ValidatorConfig`]: how often to ask and how long to
/// wait are properties of this node and its judge, while which arms a
/// population splits into and how strong an intervention may be are properties
/// of whose traffic is being experimented on. One project running Live with a
/// tool-call channel and another observing in Shadow is the ordinary case, and
/// it is unrepresentable if the occupant reads one global answer.
///
/// Resolved beside the policy and the budget, from the same key, and handed to
/// the occupant on [`InterjectionContext`].
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationTerms {
    /// How this membership's population splits.
    ///
    /// Read once per *session*, by the engine, to stamp an arm — never by the
    /// occupant, which reads the stamp. Both halves are here anyway because
    /// they are one operator's one decision, and a share table living
    /// somewhere else from the channel it applies to is two files to keep in
    /// agreement.
    pub shares: ArmShares,
    /// Everything the action map needs beyond the verdict.
    pub action: ActionPolicy,
    /// The fraction of fired triggers the placebo arm intervenes on.
    pub placebo_rate: f64,
    /// What to say on the first turn served under a signal-driven escalation,
    /// in the *forwarded request only*.
    ///
    /// `None` — the shipped answer — decorates nothing, which is what R2 means
    /// by "neither is on by default". See [`handoff`] for the three properties
    /// this rides under and for the wording an operator can start from.
    ///
    /// **Here rather than on [`ActionPolicy`]**, and the boundary is worth
    /// keeping: `ActionPolicy` is documented as "the deployment-side inputs to
    /// [`map`]", and [`map`] is a pure function of a verdict, a trigger and what
    /// a membership permits. The note is an input to *dispatch* — it is read by
    /// the engine, one layer below, on a turn the map has already been asked
    /// about and possibly on a turn it was never asked about at all. Putting it
    /// where `map` can see it would put a decoration in the deliberation.
    pub handoff_note: Option<String>,
}

impl Default for ValidationTerms {
    /// Enrolled, observing, acting on nothing — the posture the whole loop
    /// ships in. See [`ArmShares::shadow_only`] and [`ActionPolicy::default`].
    fn default() -> Self {
        Self {
            shares: ArmShares::shadow_only(),
            action: ActionPolicy::default(),
            placebo_rate: DEFAULT_PLACEBO_RATE,
            // R2 ships the second steering surface off, like the first.
            handoff_note: None,
        }
    }
}

/// The placebo intervention rate a deployment that says nothing gets.
///
/// One fired trigger in four, which is enough to see a disruption effect
/// without making the control arm a worse version of the live one. Calibration
/// rather than measurement: matching the sham's rate to the live arm's observed
/// rate is something a dashboard does across many sessions, not something one
/// turn can know.
pub const DEFAULT_PLACEBO_RATE: f64 = 0.25;

/// What *this node* sets about the validate loop.
///
/// The deployment half: how often to ask, how much to show, how much review may
/// be in flight, and what assignment hashes against. Nothing here is a tenancy
/// decision — see [`ValidationTerms`] for the half that is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidatorConfig {
    pub trigger: TriggerConfig,
    pub brief: BriefConfig,
    pub review: ReviewLimits,
    /// The salt placebo timing hashes against, which is the same
    /// deployment-wide salt arm assignment uses.
    ///
    /// Stable: moving it re-randomizes the experiment, which is something an
    /// operator does deliberately between studies and never in the middle of
    /// one. See [`Arm::for_session`]. The engine holds the same value for the
    /// assignment itself — two holders of one operator-written string, set at
    /// the one composition site, because the two hashes happen on opposite
    /// sides of the seam and neither can reach the other's configuration.
    pub arm_salt: String,
}

/// The occupant of the interjection seam.
///
/// Owns the trigger, the arm's meaning, the review budget, the brief, the
/// verdict parse and the action map — and returns events rather than writing
/// them.
pub struct Validator {
    judge: Arc<dyn JudgeClient>,
    trigger: Trigger,
    budget: ReviewBudget,
    config: ValidatorConfig,
}

impl Validator {
    pub fn new(judge: Arc<dyn JudgeClient>, config: ValidatorConfig) -> Self {
        Self {
            judge,
            trigger: Trigger::new(config.trigger, default_signals()),
            budget: ReviewBudget::new(config.review),
            config,
        }
    }

    /// The same validator with a different signal set.
    ///
    /// The additive seam the trigger's trait exists for: a deployment adding
    /// the prompt-stability signal, or an experiment removing one, changes the
    /// list and touches no gate.
    pub fn with_signals(mut self, signals: Vec<Box<dyn Signal>>) -> Self {
        self.trigger = Trigger::new(self.config.trigger, signals);
        self
    }

    /// Everything after the trigger has fired.
    async fn decide(
        &self,
        context: &InterjectionContext<'_>,
        terms: &ValidationTerms,
        fired: TriggerRecord,
        arm: Arm,
    ) -> Interjection {
        let validation_id = ValidationId::generate();
        let mut record = ControlRecord::default();

        let (outcome, action) = if arm.consults_judge() {
            self.consult(context, terms, &fired, &mut record).await
        } else {
            // The placebo arm: no judge, and an intervention whose *timing* is
            // a hash rather than a draw — for the reason the arm itself is one.
            // The action it takes is deliberately the weakest that is still an
            // interruption, because the control has to isolate the disruption
            // from the correction: a sham with a plausible-looking correction
            // in it would be measuring a worse judge, not no judge.
            let selected = placebo_intervenes(
                context.response_id,
                &self.config.arm_salt,
                terms.placebo_rate,
            );
            // **The channel gates the sham exactly as it gates the real thing.**
            // The Live arm's action goes through `map`, which collapses to
            // `Continue` under `Off`; the placebo's never did, so `Off` — the
            // shipped default, documented as "never interject" — was the one
            // configuration where the control arm interrupted turns the treated
            // arm left alone. That is worse than an unmeasured experiment: it
            // is a deployment that installed a validator to observe and got a
            // sham halt on live traffic instead.
            //
            // Withheld rather than quiet, because the timing did fire. The
            // marker is what the fold compares; the disruption is what `Off`
            // forbids, and only one of those is the measurement.
            let timing = match (selected, terms.action.channel) {
                (false, _) => PlaceboTiming::Quiet,
                (true, SteerChannel::Off) => PlaceboTiming::Withheld,
                (true, _) => PlaceboTiming::Intervened,
            };
            (
                ValidationOutcome::NotRun {
                    reason: NotRunReason::PlaceboArm { timing },
                },
                matches!(timing, PlaceboTiming::Intervened).then(|| SteerAction::Halt {
                    reason: sham_directive(),
                }),
            )
        };

        let action = action.unwrap_or(SteerAction::Continue);
        record.validation_decided(validation_id, fired, arm, outcome);

        // The arm decides whether the action happens. A Shadow run has computed
        // everything and logged everything, and does nothing — which is what
        // makes it the control the whole instrumentation is built around.
        if !arm.acts() {
            return Interjection::Proceed { record };
        }
        match action {
            // `Escalate` proceeds. The narrowing is not carried across the seam
            // in a side channel: it is a fact in the log, folded into
            // `SessionState::active_escalation`, so the turns it applies to read
            // it from the same projection a replay would.
            SteerAction::Continue | SteerAction::Escalate { .. } => {
                Interjection::Proceed { record }
            }
            // **Both outcomes are assistant text since M10.0.** Outcome B used
            // to mint a synthetic `fetch_steer` call whose payload the agent
            // fetched over MCP; the correction is the turn's answer now, so the
            // agent reads it where it reads every other answer and decides with
            // no round trip — and the whole cancelled-steer hazard class (a
            // call declined at the approval prompt, a call whose turn was
            // dropped) has nothing left to be about.
            //
            // What still separates them is what the answer *invites*. A steer
            // restates the pending request, so the agent has the correction and
            // the task in one place and its loop carries on; a halt does not,
            // so the loop ends and a human picks it up.
            SteerAction::Steer { directive } => Interjection::Complete {
                item: Item::assistant_text(
                    // Composed here rather than inside `map`, because the
                    // request lives in the session and `map` is deliberately
                    // pure over the judge's answer and this membership's terms.
                    // The log books the directive alone — see
                    // `SteerAction::Steer` — so the user's words appear once.
                    render_steer_answer(&directive, trailing_user_request(&context.state.items)),
                    context.response_id.clone(),
                ),
                usage: record.usage(),
                record,
            },
            SteerAction::Halt { reason } => Interjection::Complete {
                item: Item::assistant_text(reason, context.response_id.clone()),
                usage: record.usage(),
                record,
            },
        }
    }

    /// Ask the judge, book what it cost, and turn its answer into an action.
    ///
    /// Returns the outcome to record and the action to consider taking. Every
    /// failure path here returns `None` for the action, which is the "release
    /// the turn" rule spelled once.
    async fn consult(
        &self,
        context: &InterjectionContext<'_>,
        terms: &ValidationTerms,
        fired: &TriggerRecord,
        record: &mut ControlRecord,
    ) -> (ValidationOutcome, Option<SteerAction>) {
        // Before the await, always. See the module note on the two counters.
        // The clock is the log's — the turn being decided has already committed
        // its `TurnStarted`, so this is "now" as the log understands it, and a
        // replay reads the same value the original process did.
        let Some(reservation) = self.budget.reserve(context.state.last_event_at_ms()) else {
            return (
                ValidationOutcome::NotRun {
                    reason: NotRunReason::ReviewBudgetSpent,
                },
                None,
            );
        };

        let brief = ValidationBrief::build(
            &context.state.items,
            context.objective.clone(),
            fired.facts().map(str::to_string).collect(),
            self.config.brief,
        );
        let answer = self
            .judge
            .consult(&context.side_call, judge_system_prompt(), &brief.render())
            .await;

        let answer = match answer {
            Ok(answer) => {
                reservation.succeeded();
                answer
            }
            // Every arm here releases the turn, and the three differ only in
            // what the log is told. Spelled as a `match` with no default: the
            // failure vocabulary and the not-run vocabulary are the same size
            // on purpose, and a new arm on either must be paired here rather
            // than swept into whichever reason happened to be the fallback.
            Err(JudgeFailure::Unaffordable) => {
                // Neither counter moves, and that is the third state the two
                // of them exist to express. A budget refusal is not evidence
                // the judge answers (which would clear a real failure streak)
                // and not evidence it is down (which would trip the breaker
                // for every *other* tenant on this node); the reservation is
                // simply given back by the drop.
                drop(reservation);
                return (
                    ValidationOutcome::NotRun {
                        reason: NotRunReason::BudgetRefused,
                    },
                    None,
                );
            }
            Err(JudgeFailure::Unavailable) => {
                reservation.failed();
                // Nothing was attempted, so nothing is booked. An abandoned
                // side call against a target nobody dialled would be a phantom
                // row on the dashboard.
                return (
                    ValidationOutcome::NotRun {
                        reason: NotRunReason::JudgeUnavailable,
                    },
                    None,
                );
            }
            Err(JudgeFailure::Abandoned { target, reason }) => {
                reservation.failed();
                record.side_call_abandoned(context.side_call.id.clone(), target, reason);
                return (
                    ValidationOutcome::NotRun {
                        reason: NotRunReason::JudgeFailed,
                    },
                    None,
                );
            }
        };

        // Booked before the answer is parsed, and that ordering is the point:
        // the money was spent whatever the answer said, and a parse failure
        // that also lost the cost would make a broken judge look free.
        //
        // Under the id the *caller* minted, not a fresh one. The judge held
        // this check's money under that string before the call was made, so
        // booking it under another would leave a ledger row and a log row for
        // one call that nothing can join.
        let side_call_id = context.side_call.id.clone();
        record.side_call_completed(side_call_id.clone(), answer.target, answer.usage);

        let verdict = match Verdict::parse(&answer.raw) {
            Ok(verdict) => verdict,
            Err(error) => {
                tracing::warn!(%error, "judge answer did not parse; releasing the turn");
                return (
                    ValidationOutcome::NotRun {
                        reason: NotRunReason::VerdictUnparseable,
                    },
                    None,
                );
            }
        };
        // Clamped against the turn's own ceiling before it is recorded, so the
        // log holds the narrowing the membership's policy leaves standing
        // rather than the one the map asked for. This is the occupant's read of
        // `turn_policy`, and it is why the seam carries it: an escalation
        // recorded at a floor the membership forbids would make the audit trail
        // describe a decision that did not happen. What it deliberately cannot
        // clamp against is the quoted pool — this seam has no candidate list, by
        // the contract in `interject` — so the engine clamps a second time when
        // it applies the escalation, and records that on the turn's own
        // decision. See `SteerAction::clamped_to`.
        let action = map(
            &verdict,
            fired,
            &terms.action,
            context.state.consecutive_interventions(),
        )
        .clamped_to(context.turn_policy);
        (
            ValidationOutcome::Judged {
                side_call_id,
                verdict,
                action: action.clone(),
            },
            Some(action),
        )
    }
}

/// The placebo's interruption: an interruption and nothing else.
///
/// Deliberately content-free. The control has to isolate *being interrupted*
/// from *being corrected*, so a sham carrying a plausible correction would be
/// measuring a worse judge rather than no judge at all.
fn sham_directive() -> String {
    "Pausing here. Re-read the task and state what you believe the remaining \
     work is before continuing."
        .to_string()
}

#[async_trait]
impl Interjector for Validator {
    async fn consider(&self, context: &InterjectionContext<'_>) -> Interjection {
        // A log with no arm stamp predates the experiment, and it is not
        // enrolled in one. Assigning it here would work — the hash is
        // deterministic — and would silently re-assign every historical session
        // the day the salt moved, which is the exact hazard the stamp exists to
        // prevent. Not enrolled is the honest reading and it costs nothing.
        let Some(arm) = context.state.arm() else {
            return Interjection::proceed();
        };
        // The second half of the same question, asked of the *membership*
        // rather than of the log — and independent of the first on purpose.
        // A session stamped under a project whose operator has since turned
        // the loop off is not validated, which is what turning it off means;
        // and a project that turned it on today does not retroactively enrol
        // sessions created before the stamp existed, which is what keeps the
        // arm comparison from being computed over a control group that was
        // never eligible. Either absence releases the turn.
        let Some(terms) = context.validation else {
            return Interjection::proceed();
        };
        let Some(fired) = self.trigger.evaluate(context.state, context.dialect) else {
            return Interjection::proceed();
        };
        self.decide(context, terms, fired, arm).await
    }
}

#[cfg(test)]
mod tests;
