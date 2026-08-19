// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The session state machine.
//!
//! A [`Session`] is a lease plus a projection. Everything it knows — the
//! conversation, the routing ledger, which turns are already done — is derived
//! by replaying the event log, so a successor process that acquires the lease
//! reconstructs identical state without any handoff from the process it
//! replaced.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::control::{FrontierHistory, Payer, Principal};
use crate::event::{
    ControlRecord, IncompleteReason, NotRunReason, PlaceboTiming, SessionEvent, SessionEventKind,
    SessionObserver, Usage, ValidationOutcome,
};
use crate::ids::{ResponseId, SessionId, TurnId};
use crate::item::{Item, ItemContent};
use crate::routing::{CacheLedger, DecisionRecord, ProviderPricing, Target};
use crate::store::{Lease, SessionStore, StoreError};
use crate::validate::{Arm, EscalationOverrides, SteerAction};

/// How many events to pull per replay batch.
const REPLAY_BATCH: usize = 1024;

/// How many turns of billing the cost-anomaly signal compares against.
///
/// A window rather than the whole history, and small on purpose: the question
/// is "is this turn unlike *this session's recent work*", and a session's early
/// turns are frequently a different kind of work from its later ones. Sixteen
/// `u64`s is also strictly less state than one conversation item, which is what
/// makes keeping it in the projection cheaper than deriving it on demand.
const TURN_TOKEN_WINDOW: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("session `{0}` is owned by another node")]
    NotOwner(SessionId),
    #[error("response `{0}` is not open")]
    ResponseNotOpen(ResponseId),
}

/// The result of admitting a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnAdmission {
    /// A new turn was opened.
    Started(ResponseId),
    /// The turn id was already completed; the existing response is replayed
    /// rather than a second one being generated.
    Deduplicated(ResponseId),
}

impl TurnAdmission {
    pub fn response_id(&self) -> &ResponseId {
        match self {
            TurnAdmission::Started(id) | TurnAdmission::Deduplicated(id) => id,
        }
    }
}

/// What a completed turn produced.
///
/// The usage is kept, not just the response id, because a deduplicated retry
/// has to answer with the accounting the original turn reported. Recomputing it
/// is not an option — the token counts came from the provider — and reporting
/// zeros would tell a client its turn was free.
struct CompletedTurn {
    response_id: ResponseId,
    usage: Usage,
}

/// Where a dispatch went, how much prompt it carried, and at what price.
struct PendingRouting {
    target: Target,
    isl_tokens: u64,
    /// The rate card the decision recorded. See
    /// [`DecisionRecord::rate_card`] for why a settle reads the log's card and
    /// never a live one.
    rate_card: Option<ProviderPricing>,
    /// Whose credential the decision resolved. Held here for exactly the
    /// reason the card is: what a turn draws depends on it, and a repair
    /// driven from the log alone has to reach the same answer.
    payer: Payer,
}

/// A response that terminated, and everything needed to charge it for.
///
/// The spend ledger's view of a turn, projected from the log rather than
/// handed across from whatever produced it: the repair that re-drives a lost
/// settle has only the log to work from, so if the live settle read its inputs
/// from anywhere else the two would be settling from two different accounts of
/// the same turn.
///
/// **The price is one of those inputs**, which it was not at first. A settle
/// priced against the running process's catalog is priced against a file an
/// operator edits, so the two moments really could see different numbers — and
/// a repair against a catalog that had *dropped* the model could see no number
/// at all, which failed the settle, which failed every turn of the session
/// after it. [`Self::rate_card`] is that hole closed at the seam it was open
/// at: the card travels in the log beside the target it prices.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalSettlement {
    pub response_id: ResponseId,
    /// The log sequence number of the terminal event, and half of the
    /// idempotency key a settle is applied under.
    pub seq: u64,
    /// Where the turn was dispatched.
    ///
    /// `None` for a response that terminated before any routing decision was
    /// recorded — a refusal, or a failure while pricing the options. That turn
    /// reached no provider and so owes nobody anything, which makes the
    /// absence a fact rather than a missing value.
    pub target: Option<Target>,
    /// The rate card that was in force when the turn was routed, as its
    /// decision recorded it — the price half of "projected from the log rather
    /// than handed across from whatever produced it".
    ///
    /// Carried beside the target rather than folded into it, because the two
    /// absences mean different things and a settle has to tell them apart: no
    /// target is a turn that reached nobody and owes nothing, while a frontier
    /// target with no card is a turn routed before the card was recorded, which
    /// no process alive can price. See [`DecisionRecord::rate_card`].
    pub rate_card: Option<ProviderPricing>,
    /// Whose credential paid, as the decision recorded it.
    ///
    /// Beside the card for the same reason the card is here rather than in the
    /// running process's configuration: what the project's ledger draws is
    /// `BudgetCounts::drawn_usd(payer, spend)`, and a repair driven from the
    /// log alone must reach the same number as the live settle it replaced. A
    /// payer re-derived from whichever credential the process happens to hold
    /// *now* would differ from the one that actually paid the moment a member
    /// attached or removed a key.
    ///
    /// [`Payer::Deployment`] for a response that recorded no decision, which is
    /// literally true: a turn that reached no provider spent nobody's
    /// credential, and a settle prices it at zero on the target rather than on
    /// this field.
    pub payer: Payer,
    /// What the terminal event reported, estimate or measurement alike. An
    /// estimate is what a provider that reported nothing gets charged on, and
    /// it is charged exactly as a measurement would be.
    pub usage: Usage,
}

/// State derived from the event log.
#[derive(Default)]
pub struct SessionState {
    pub items: Vec<Item>,
    pub ledger: CacheLedger,
    /// Number of turns started so far; the index the next turn will use.
    pub turn_index: u64,
    /// Turn ids that reached a terminal state, and what they produced.
    completed_turns: HashMap<TurnId, CompletedTurn>,
    /// Turn ids currently in flight.
    open_turns: HashMap<TurnId, ResponseId>,
    /// Routing facts of responses that have not terminated yet.
    ///
    /// `Routed` is committed before execution, so it records an intent rather
    /// than a transmission and is the wrong evidence for the cache ledger: a
    /// dispatch that failed on the way out would leave a phantom warm prefix
    /// on that target, and the retry would then be priced against a cache that
    /// does not exist — phantom-warm frontier beating genuinely cheaper local
    /// workers. The fold therefore happens at the terminal event, and only
    /// when that event proves the prompt was processed: a completion always
    /// does, an incomplete does only when its usage shows billed input
    /// (partial output cannot exist without a prefill, but the engine also
    /// terminates dispatches that failed before anything was sent, and those
    /// carry an empty usage). A response that never terminates — the process
    /// died mid-flight, and nobody knows what the provider saw — records
    /// nothing. Cold is the conservative reading in both unproven cases,
    /// because over-claiming warmth is the exact mispricing this fold exists
    /// to prevent.
    pending_routings: HashMap<ResponseId, PendingRouting>,
    /// Which routed turns went to a hosted model, in log order.
    ///
    /// Folded at `Routed` and *not* at the terminal event, which is the
    /// opposite of the rule `pending_routings` follows one field up, so the
    /// difference is worth stating. The cache ledger asks "did the provider
    /// process this prompt?", and a dispatch that failed on the way out is no
    /// evidence of that. A cadence asks "how often did this session reach for
    /// a hosted model?", and a dispatch that failed on the way out is exactly
    /// that. Counting only successes would let a provider outage multiply the
    /// frontier traffic of every session retrying through it, at the moment
    /// the knob is most supposed to hold.
    pub frontier_history: FrontierHistory,
    /// The most recent response to terminate, priced-ready.
    ///
    /// **One entry rather than a list, and that is a claim about the log
    /// rather than a convenience.** A session's turns are serialized — the
    /// engine gates them per session within a process and the store's lease
    /// fences them across processes — and every turn applies its spend before
    /// the next is admitted. So whenever a session is opened, at most one
    /// terminal event can still be unsettled, and it is the last one. Keeping
    /// the whole history here would be an unbounded second copy of the log,
    /// held to support a repair that can only ever concern its final entry.
    ///
    /// `None` until a response terminates, which is also the honest answer for
    /// a session whose only turns are still open.
    last_settlement: Option<TerminalSettlement>,
    /// Steers this deployment emitted that no client has answered yet, keyed
    /// by `call_id` and valued by the log timestamp of the `ItemAppended` that
    /// opened each one.
    ///
    /// The timestamp is what makes fulfilment latency derivable from the
    /// projection and the log alone — the closing item's own `at_ms` minus
    /// this one — without a second table to keep in step with the first.
    ///
    /// Filled by an `ItemAppended` whose item is a `ToolCall` *bearing a
    /// response id*, and cleared by an `ItemAppended` whose item is a
    /// `ToolResult` naming the same call. Provenance rather than shape is what
    /// selects the opening item: everything on the input path canonicalizes
    /// with no response id, so a client cannot open a steer by sending a tool
    /// call. One writer, one event kind, no second source of truth.
    ///
    /// `pub(crate)` rather than `pub` because its consumer is M6's trigger —
    /// the open-steer exclusion that stops a steer re-triggering the
    /// validation that emitted it. It ships with M4 anyway because it is a
    /// fact about M4's items: the events that fill it are emitted here, and a
    /// projection added later against an older log is a projection nobody has
    /// replayed.
    pub(crate) open_steers: HashMap<String, u64>,
    /// The most recent routing decision, whether or not its response has
    /// terminated.
    ///
    /// A different question from [`Self::pending_routings`] one field up, which
    /// is why it is a different field rather than a read of that one.
    /// `pending_routings` asks *what evidence is still outstanding* and is
    /// emptied at the terminal event; this asks *where did the last turn go*,
    /// which is exactly as true after the turn ends as during it. Deriving one
    /// from the other would answer "this session has never been routed" for
    /// every session between turns.
    ///
    /// The whole record rather than the target: its consumer renders the
    /// rationale, the policy digest and the budget state as well — see
    /// `explain_last_route` in `roundhouse-mcp`, the audit trail as a tool.
    last_decision: Option<DecisionRecord>,
    pub last_seq: u64,

    // ---- The validate loop's projections. -------------------------------
    //
    // Every one of these is a fold of the log and never a counter kept beside
    // it. That is the same rule `FrontierHistory` follows and it is load-bearing
    // for the same reason twice over: a successor process that replays a log has
    // to arrive at the trigger's answer, *and* the arm comparison is only
    // meaningful if a replay of a session reaches the decision the original
    // process reached. A second writer anywhere here would make "fold equals
    // log" false exactly where the experiment reads.
    /// Which arm of the validate experiment this session belongs to.
    ///
    /// `None` for a log written before the experiment: not enrolled, which the
    /// occupant reads as "do not validate". See
    /// [`SessionEventKind::SessionCreated`]'s `arm`.
    pub(crate) arm: Option<Arm>,
    /// Billable tokens this session's turns have reported since the last
    /// validation, or since it opened.
    ///
    /// The self-scaling half of the trigger: a validator budgeted as a fraction
    /// of spend-since-last-check needs no per-workload cadence to tune.
    ///
    /// Side-call tokens are deliberately absent. They are this deployment's own
    /// spend, not the conversation's, and counting them would let the act of
    /// checking bring the next check forward. Held structurally rather than by
    /// care: only a turn that recorded a
    /// [`Routed`](SessionEventKind::Routed) folds its usage in here, and a
    /// completing interjection records none — which is what keeps the judge's
    /// own billing out even though it arrives on an ordinary
    /// `ResponseCompleted`.
    ///
    /// Reset when a validation is *bought* and not merely decided: a check that
    /// reached no judge leaves the evidence standing, so a session whose judge
    /// was down validates as soon as it is back rather than having to earn the
    /// budget again.
    pub(crate) tokens_since_last_validation: u64,
    /// When the last validation was decided, on the log's own clock.
    pub(crate) last_validation_at_ms: Option<u64>,
    /// The timestamp of the most recent event, whatever kind it was.
    ///
    /// What makes the cooldown computable without a clock argument: the turn
    /// being decided has already committed its `TurnStarted`, so this is
    /// "now" as the log understands it — and a replay reads the same value the
    /// original process did, which a wall clock would not.
    pub(crate) last_event_at_ms: u64,
    /// Turns in a row that this deployment interrupted.
    ///
    /// Counted at the terminal event and reset there, so it means "trailing
    /// interrupted turns" rather than "interruptions ever". A turn the
    /// validator let through resets it, which is what stops the cap from
    /// disabling validation for the rest of a long session.
    pub(crate) consecutive_interventions: u32,
    /// Validations this session has bought.
    ///
    /// Bought, not decided: a judged outcome and the placebo arm's own
    /// non-consultation count, and the five ways a validation can reach no
    /// judge do not. This is the session's lifetime allowance and the counter
    /// only grows, so charging a judge outage to it would close the trigger's
    /// gate for good on a session that never got a check.
    pub(crate) validations_run: u32,
    /// The turn index at which a client's input last closed an open steer.
    ///
    /// The hysteresis rule's evidence. By the time the interjection seam runs,
    /// the turn's input is already committed, so `open_steers` has *already*
    /// been cleared by the fulfilling result — asking "is a steer open" would
    /// answer no on exactly the turn the rule is about. Recording the turn it
    /// closed on is what makes the question answerable at all.
    pub(crate) steer_fulfilled_on_turn: Option<u64>,
    /// Billable tokens per *dispatched* terminated turn, oldest first, bounded
    /// to [`TURN_TOKEN_WINDOW`].
    ///
    /// The trailing distribution the cost-anomaly signal compares each turn
    /// against, which is why only dispatched turns are in it: a completing
    /// interjection's usage is the judge's own side call, and a side call in
    /// this window makes the next ordinary turn look cheap by comparison and
    /// the next check correspondingly less likely.
    pub(crate) turn_tokens: Vec<u64>,
    /// A narrowing the validate loop asked for, and how many turns of it are
    /// left.
    ///
    /// Held in the projection rather than handed across the interjection seam
    /// in a side channel, because it outlives the turn that decided it: the
    /// turns it applies to read it from the same fold a replay would build.
    escalation: Option<ActiveEscalation>,
    /// Whether the turn currently in flight was interrupted by the validator.
    ///
    /// Set when the decision is committed and consumed at the terminal event,
    /// which is the one place every turn passes through exactly once.
    turn_intervened: bool,
}

/// A narrowing the validate loop asked for, with its remaining life.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveEscalation {
    pub overrides: EscalationOverrides,
    /// Turns still to be served under it, counting from and including the turn
    /// the escalation was decided on.
    pub turns_remaining: u32,
}

impl SessionState {
    /// Fold one event into the projection.
    ///
    /// The exhaustive match is deliberate: a new event kind should not silently
    /// fail to update derived state.
    fn apply(&mut self, event: &SessionEvent) {
        self.last_seq = event.seq;
        // Above the match, and from every kind rather than only the ones that
        // cost money: this is the clock the cooldown is measured on, and a
        // session whose last event was an error an hour ago has been quiet for
        // an hour.
        self.last_event_at_ms = event.at_ms;
        match &event.kind {
            SessionEventKind::ItemAppended { item } => {
                // The conversation itself is untouched by steering: an emitted
                // tool call is an ordinary item arriving on the ordinary path,
                // which is the whole reason this kind was reused rather than a
                // second item-carrying kind added. A second kind would have
                // given this fold, the wire layer's stored-items projection and
                // the context assembler two sources for one conversation, and
                // the first site to forget the second kind forks every steered
                // session.
                self.items.push(item.clone());
                // The projection beside it, folded from the same event. See
                // `open_steers`.
                match &item.content {
                    ItemContent::ToolCall { call_id, .. } => {
                        // Provenance, not shape. An item on the input path
                        // canonicalizes with no response id, so this arm is
                        // unreachable for anything a client sent — including
                        // the client's own verbatim resend of the very call it
                        // is about to answer, which must not re-open the steer
                        // its output closes.
                        if item.response_id.is_some() {
                            self.open_steers.insert(call_id.clone(), event.at_ms);
                        }
                    }
                    ItemContent::ToolResult { call_id, .. } => {
                        // Keyed on the call id and not on "a result arrived":
                        // an agent runs its own tools between our turns, and
                        // closing on any of those would report a steer
                        // fulfilled that nobody answered.
                        if self.open_steers.remove(call_id).is_some() {
                            // Which turn closed it, for the trigger's
                            // hysteresis. The removal above is what the
                            // question is really about, and asking `is a steer
                            // open` at the seam would answer `no` on exactly
                            // this turn — the input is committed before the
                            // seam runs.
                            self.steer_fulfilled_on_turn = Some(self.turn_index);
                        }
                    }
                    ItemContent::Text { .. } => {}
                }
            }
            SessionEventKind::TurnStarted {
                turn_id,
                response_id,
            } => {
                self.turn_index += 1;
                self.open_turns.insert(turn_id.clone(), response_id.clone());
            }
            SessionEventKind::Routed {
                response_id,
                decision,
            } => {
                self.frontier_history.record(&decision.chosen);
                self.last_decision = Some(decision.clone());
                // Held rather than recorded; see `pending_routings`.
                self.pending_routings.insert(
                    response_id.clone(),
                    PendingRouting {
                        target: decision.chosen.clone(),
                        isl_tokens: decision.isl_tokens,
                        rate_card: decision.rate_card,
                        payer: decision.payer,
                    },
                );
            }
            SessionEventKind::ResponseCompleted { response_id, usage }
            | SessionEventKind::ResponseIncomplete {
                response_id, usage, ..
            } => {
                // Recorded at the terminal event's timestamp — when the
                // provider stopped holding the prompt, which is what the TTL
                // runs from — and only under the evidence rule documented on
                // `pending_routings`.
                let routing = self.pending_routings.remove(response_id);
                // Whether this response ever reached a provider, which is the
                // same question `last_settlement` keys "owes nobody anything"
                // on one block down. The validate loop's two token projections
                // read it too — see below.
                let dispatched = routing.is_some();
                if let Some(routing) = &routing {
                    let processed =
                        matches!(event.kind, SessionEventKind::ResponseCompleted { .. })
                            || usage.input_tokens > 0;
                    if processed {
                        self.ledger
                            .record(&routing.target, event.at_ms, routing.isl_tokens);
                    }
                }

                // The spend ledger's view of the same event, and note that it
                // is folded under *no* evidence rule: the cache ledger asks
                // whether a provider processed the prompt, while a settlement
                // asks what the log says this turn is to be charged. A
                // dispatch that never reached anyone is priced at zero by
                // carrying no target, not by being left out — leaving it out
                // would strand its hold for a whole TTL.
                self.last_settlement = Some(TerminalSettlement {
                    response_id: response_id.clone(),
                    seq: event.seq,
                    rate_card: routing.as_ref().and_then(|routing| routing.rate_card),
                    payer: routing
                        .as_ref()
                        .map(|routing| routing.payer)
                        .unwrap_or_default(),
                    target: routing.map(|routing| routing.target),
                    usage: usage.clone(),
                });

                // A turn is only settled once its response terminates, which is
                // what makes a re-sent turn after a mid-generation crash
                // restartable rather than deduplicated into a partial answer.
                if let Some(turn_id) = self
                    .open_turns
                    .iter()
                    .find(|(_, open)| *open == response_id)
                    .map(|(turn_id, _)| turn_id.clone())
                {
                    self.open_turns.remove(&turn_id);
                    if matches!(event.kind, SessionEventKind::ResponseCompleted { .. }) {
                        self.completed_turns.insert(
                            turn_id,
                            CompletedTurn {
                                response_id: response_id.clone(),
                                usage: usage.clone(),
                            },
                        );
                    }
                }

                // The validate loop's per-turn bookkeeping, all of it here
                // because the terminal event is the one place every turn passes
                // through exactly once — whether it dispatched, was steered, or
                // was refused.

                // **Only a turn that dispatched bills the conversation.** A
                // completing interjection's usage is the *judge's* side call —
                // `complete_with_item` says so, and it is the honest number to
                // report to a client — but both fields below are about what the
                // conversation spent. Feeding them the check's own cost lets
                // the act of checking bring the next check forward, which is
                // exactly what `tokens_since_last_validation` documents itself
                // as not doing, and puts a side call into the trailing
                // distribution the cost-anomaly signal compares ordinary turns
                // against. `dispatched` is the same evidence the settlement
                // above keys on: no `Routed`, no provider, nothing the
                // conversation was billed for.
                if dispatched {
                    self.tokens_since_last_validation = self
                        .tokens_since_last_validation
                        .saturating_add(usage.total());
                    self.turn_tokens.push(usage.total());
                    if self.turn_tokens.len() > TURN_TOKEN_WINDOW {
                        self.turn_tokens.remove(0);
                    }
                }
                // Trailing interrupted turns, so a turn the validator let
                // through resets the count. Counting interruptions forever
                // would disable validation for the rest of any long session
                // that was ever interrupted twice.
                if self.turn_intervened {
                    self.consecutive_interventions =
                        self.consecutive_interventions.saturating_add(1);
                } else {
                    self.consecutive_interventions = 0;
                }
                self.turn_intervened = false;
                self.escalation = self.escalation.and_then(|escalation| {
                    escalation
                        .turns_remaining
                        .checked_sub(1)
                        .filter(|remaining| *remaining > 0)
                        .map(|turns_remaining| ActiveEscalation {
                            turns_remaining,
                            ..escalation
                        })
                });
            }
            SessionEventKind::ValidationDecided { arm, outcome, .. } => {
                // **The cooldown is spent by every decision; the cap and the
                // budget are spent only by a decision that bought something.**
                //
                // The cooldown is what stops a session with an open gate
                // re-dialling a judge that is down on every single turn, so it
                // has to run whatever the outcome was — a failure is precisely
                // the case it exists for.
                //
                // The other two are the session's *allowance*, and the five
                // failure reasons bought none of it: no verdict, no action, and
                // for three of them not even a side call. Charging them would
                // let a judge outage close `validations_run`'s gate for good
                // after `max_validations_per_session` turns — permanently,
                // since the counter only grows — and each no-op would discard
                // the accumulated token evidence too, so nothing would be left
                // to fund a check once the judge came back. A session must
                // validate when the budget returns; that is the whole reason
                // the gate is a projection of spend rather than a countdown.
                //
                // The placebo arm charges: it consults nobody by design, not by
                // failure, and it is the control the live arm is compared
                // against. A control that validated far more often than the arm
                // it controls for is not a control.
                let bought = match outcome {
                    ValidationOutcome::Judged { .. } => true,
                    ValidationOutcome::NotRun {
                        reason: NotRunReason::PlaceboArm { .. },
                    } => true,
                    ValidationOutcome::NotRun {
                        reason:
                            NotRunReason::BudgetRefused
                            | NotRunReason::ReviewBudgetSpent
                            | NotRunReason::JudgeUnavailable
                            | NotRunReason::JudgeFailed
                            | NotRunReason::VerdictUnparseable,
                    } => false,
                };
                self.last_validation_at_ms = Some(event.at_ms);
                if bought {
                    self.validations_run = self.validations_run.saturating_add(1);
                    // The budget the gate spends. Reset here rather than at the
                    // side call, because what the gate measures is conversation
                    // spend between checks and a check is not conversation.
                    self.tokens_since_last_validation = 0;
                }

                // Only an arm that *acts* has intervened. A Shadow run computed
                // everything and did nothing, and counting it would make the
                // observe-only arm suppress its own future observations —
                // which would quietly destroy the control the experiment leans
                // on.
                let action = match outcome {
                    ValidationOutcome::Judged { action, .. } => Some(action.clone()),
                    ValidationOutcome::NotRun {
                        reason:
                            NotRunReason::PlaceboArm {
                                timing: PlaceboTiming::Intervened,
                            },
                    } => Some(SteerAction::Halt {
                        reason: String::new(),
                    }),
                    ValidationOutcome::NotRun { .. } => None,
                };
                if arm.acts()
                    && let Some(action) = action
                {
                    self.turn_intervened |= action.intervenes();
                    if let SteerAction::Escalate { turns, overrides } = action
                        && turns > 0
                    {
                        self.escalation = Some(ActiveEscalation {
                            overrides,
                            turns_remaining: turns,
                        });
                    }
                }
            }
            SessionEventKind::SessionCreated { arm, .. } => self.arm = *arm,
            // Money facts, folded by the metrics layer and not here. This
            // projection answers "what may this session do next", and what a
            // side call billed does not bear on that — the *decision* beside it
            // does, and that is the arm above.
            SessionEventKind::SideCallCompleted { .. }
            | SessionEventKind::SideCallAbandoned { .. }
            | SessionEventKind::OutputTextDelta { .. }
            | SessionEventKind::TurnDeduplicated { .. }
            | SessionEventKind::Error { .. } => {}
        }
    }

    /// Which arm of the validate experiment this session is in, if any.
    pub fn arm(&self) -> Option<Arm> {
        self.arm
    }

    /// Billable tokens reported since the last validation, or since the log
    /// opened.
    pub fn tokens_since_last_validation(&self) -> u64 {
        self.tokens_since_last_validation
    }

    /// When the last validation was decided, on the log's own clock.
    pub fn last_validation_at_ms(&self) -> Option<u64> {
        self.last_validation_at_ms
    }

    /// The timestamp of the most recent event in the log.
    pub fn last_event_at_ms(&self) -> u64 {
        self.last_event_at_ms
    }

    /// Turns in a row this deployment interrupted.
    pub fn consecutive_interventions(&self) -> u32 {
        self.consecutive_interventions
    }

    /// Validations this session has bought.
    pub fn validations_run(&self) -> u32 {
        self.validations_run
    }

    /// Whether the input already committed for the turn in flight closed a
    /// steer this deployment emitted.
    ///
    /// The trigger's hysteresis rule, and the reason it is a question about a
    /// *turn index* rather than about `open_steers`: the fulfilling result is
    /// committed with the turn's input, before the interjection seam runs, so
    /// the steer is already closed by the time anybody asks.
    pub fn this_turn_fulfilled_a_steer(&self) -> bool {
        self.steer_fulfilled_on_turn == Some(self.turn_index)
    }

    /// Billable tokens per dispatched terminated turn, oldest first. See
    /// [`Self::turn_tokens`] on why a completing interjection is not one.
    pub fn recent_turn_tokens(&self) -> &[u64] {
        &self.turn_tokens
    }

    /// The narrowing a validation asked for, if one is still in force.
    ///
    /// Read by the engine on every turn while it lasts. Returning the overrides
    /// rather than the record is the same choice the MCP overlay makes: the
    /// caller wants `ceiling.narrow(&overrides)` and handing it the record
    /// would invite a second place that decides what an escalation means.
    pub fn active_escalation(&self) -> Option<EscalationOverrides> {
        self.escalation.map(|escalation| escalation.overrides)
    }

    pub fn completed_response_for(&self, turn_id: &TurnId) -> Option<&ResponseId> {
        self.completed_turns
            .get(turn_id)
            .map(|completed| &completed.response_id)
    }

    /// The most recent response to terminate, and what it is to be charged
    /// for.
    ///
    /// The one input the spend ledger's settle takes, both when this turn
    /// commits its own terminal event and when a successor replays a log whose
    /// last settle was lost to a crash. See [`Self::last_settlement`] on why
    /// one entry is enough for both.
    pub fn last_settlement(&self) -> Option<&TerminalSettlement> {
        self.last_settlement.as_ref()
    }

    /// Usage reported when `turn_id` completed, for replaying it verbatim.
    pub fn completed_usage_for(&self, turn_id: &TurnId) -> Option<&Usage> {
        self.completed_turns
            .get(turn_id)
            .map(|completed| &completed.usage)
    }

    /// Where the most recent routed turn went, and why.
    ///
    /// `None` for a session whose first turn has not been routed — a session
    /// that has only ever been steered has no routing decision, which is the
    /// accurate answer rather than an empty one.
    pub fn last_decision(&self) -> Option<&DecisionRecord> {
        self.last_decision.as_ref()
    }

    /// `call_id`s of steers this deployment emitted that no turn has answered.
    ///
    /// Sorted, so two reads of one projection are two identical answers: the
    /// underlying map has no order, and this is rendered into an agent's
    /// context by `status`, where a list that reshuffled between calls is a
    /// list an agent reads as having changed.
    ///
    /// The accessor rather than the field: [`Self::open_steers`] stays private
    /// because it is a fold this module owns, and its two consumers — M5's
    /// `status` tool and M6's trigger — both want the ids and neither wants the
    /// timestamps.
    pub fn open_steer_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.open_steers.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Rebuild a session's projection from its log, **taking no lease**.
    ///
    /// The read-only half of [`Session::open_observed`], which calls it: a
    /// reader that took the lease would evict the engine it is watching, and
    /// that rule is why the MCP control surface projects a session through here
    /// rather than opening one. One replay loop and one fold serve both, so a
    /// question answered for a reader and the same question answered for the
    /// engine cannot come back with two answers.
    pub async fn project<S: SessionStore>(
        store: &S,
        session_id: &SessionId,
        ledger: CacheLedger,
        observer: Option<&Arc<dyn SessionObserver>>,
    ) -> Result<Self, SessionError> {
        let mut state = SessionState {
            ledger,
            ..Default::default()
        };
        // Replay in batches so a long session does not need the whole log
        // resident at once.
        let mut cursor = 0u64;
        loop {
            let batch = store.read_events(session_id, cursor, REPLAY_BATCH).await?;
            if batch.is_empty() {
                break;
            }
            for event in &batch {
                state.apply(event);
            }
            if let Some(observer) = observer {
                observer.observe(&batch);
            }
            cursor = batch.last().map_or(cursor, |event| event.seq);
        }
        Ok(state)
    }
}

/// A running lease renewal, alive for exactly as long as this handle is.
///
/// Dropping it stops the renewal at that instant rather than at the next tick,
/// so the lease a finished turn leaves behind lapses on the normal failover
/// clock instead of being carried by a task nobody is watching any more.
pub struct LeaseHeartbeat {
    task: tokio::task::AbortHandle,
}

impl Drop for LeaseHeartbeat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A leased, projected session.
pub struct Session<S: SessionStore> {
    store: Arc<S>,
    session_id: SessionId,
    lease: Lease,
    state: SessionState,
    /// Watches every event this session commits, and every event it replayed
    /// on open. See [`Session::open_observed`].
    observer: Option<Arc<dyn SessionObserver>>,
}

impl<S: SessionStore> Session<S> {
    /// Claim a session and rebuild its state from the log.
    ///
    /// `ledger` carries the cache-model and pricing configuration; replayed
    /// dispatches that reached a terminal event are folded into it, so what
    /// comes back reflects both static configuration and session history.
    pub async fn open(
        store: Arc<S>,
        session_id: SessionId,
        node_id: &str,
        lease_ttl_ms: u64,
        ledger: CacheLedger,
    ) -> Result<Self, SessionError> {
        Self::open_observed(store, session_id, node_id, lease_ttl_ms, ledger, None).await
    }

    /// Claim a session with something watching its log.
    ///
    /// The observer sees the replay as well as subsequent commits, which is
    /// what lets a restarted process recover derived state for the sessions it
    /// picks back up rather than only for the turns it goes on to serve. That
    /// is only sound because the metrics fold is idempotent by `(session,
    /// seq)`; an observer without that property would double-count every
    /// session that is opened twice, which is every session that takes more
    /// than one turn.
    pub async fn open_observed(
        store: Arc<S>,
        session_id: SessionId,
        node_id: &str,
        lease_ttl_ms: u64,
        ledger: CacheLedger,
        observer: Option<Arc<dyn SessionObserver>>,
    ) -> Result<Self, SessionError> {
        let lease = store
            .acquire_lease(&session_id, node_id, lease_ttl_ms)
            .await?
            .ok_or_else(|| SessionError::NotOwner(session_id.clone()))?;

        // The same replay a lease-free reader performs, so the two cannot
        // diverge; the lease above is the only thing this adds to it.
        let state =
            SessionState::project(store.as_ref(), &session_id, ledger, observer.as_ref()).await?;

        Ok(Self {
            store,
            session_id,
            lease,
            state,
            observer,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn ledger(&self) -> &CacheLedger {
        &self.state.ledger
    }

    /// Turn index the next admitted turn will receive.
    pub fn turn_index(&self) -> u64 {
        self.state.turn_index
    }

    pub fn last_seq(&self) -> u64 {
        self.state.last_seq
    }

    /// Extend the lease. `false` means ownership was lost and this session
    /// handle must be discarded.
    pub async fn renew(&mut self, ttl_ms: u64) -> Result<bool, SessionError> {
        match self.store.renew_lease(&self.lease, ttl_ms).await? {
            Some(renewed) => {
                self.lease = renewed;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Renew this session's lease every `every_ms` until the handle is dropped.
    ///
    /// A model call routinely outlasts any TTL worth failing over on, and the
    /// TTL cannot simply be raised to cover it: it *is* the failover clock, the
    /// time a successor must wait before it may assume this node is gone.
    /// Renewal separates the two, so the lease lives as long as the owner is
    /// demonstrably alive while a genuinely dead owner is still replaced one
    /// TTL after its last tick.
    ///
    /// Renewal extends a lease that is still valid and does nothing else. The
    /// moment the store says otherwise — `None`, or an error that leaves the
    /// renewal unproven — the task stops for good. Re-acquiring here is
    /// the tempting repair and it is exactly the split-brain bug: a successor
    /// has already been admitted on the strength of this lease lapsing, and a
    /// second writer appending into the same log is the one thing leases exist
    /// to prevent. Stopping instead leaves fencing to fail this owner's next
    /// append, which is the correct outcome and the one the store guarantees.
    ///
    /// Renewal is only safe because the work it covers is bounded; an owner
    /// that is alive but stuck must still lose the session. The engine's
    /// `turn_deadline_ms` is what supplies that bound.
    pub fn heartbeat(&self, every_ms: u64, ttl_ms: u64) -> LeaseHeartbeat {
        let store = Arc::clone(&self.store);
        // The task owns a clone, so it renews without the session handle. Both
        // stay usable: [`SessionStore::append_events`] validates against the
        // store's current record rather than against the handle's copy of
        // `expires_at_ms`.
        let mut lease = self.lease.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(every_ms)).await;
                match store.renew_lease(&lease, ttl_ms).await {
                    Ok(Some(renewed)) => lease = renewed,
                    Ok(None) => {
                        tracing::warn!(
                            session_id = %lease.session_id,
                            node_id = %lease.node_id,
                            "lease lost; stopping renewal rather than re-acquiring"
                        );
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(
                            session_id = %lease.session_id,
                            node_id = %lease.node_id,
                            %error,
                            "lease renewal failed; stopping renewal"
                        );
                        return;
                    }
                }
            }
        });
        LeaseHeartbeat {
            task: task.abort_handle(),
        }
    }

    pub async fn release(self) -> Result<(), SessionError> {
        Ok(self.store.release_lease(&self.lease).await?)
    }

    /// Append events and fold them into the local projection.
    ///
    /// The projection is updated from what the store actually assigned rather
    /// than from what was submitted, so in-memory state can never claim a
    /// sequence number the log does not have.
    async fn commit(
        &mut self,
        kinds: Vec<SessionEventKind>,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let events = self.store.append_events(&self.lease, kinds).await?;
        for event in &events {
            self.state.apply(event);
        }
        // After the projection, not before: an observer that read session state
        // in response would otherwise see it one commit behind the events it
        // was just handed.
        if let Some(observer) = &self.observer {
            observer.observe(&events);
        }
        Ok(events)
    }

    /// Record the session-created event. Safe to call once, at creation.
    ///
    /// Called where the log is still empty and this handle already holds the
    /// lease, which is what makes "exactly once" a property of the log rather
    /// than of anyone's care: a second caller would have to win the lease and
    /// find `last_seq == 0`, and only one of those can be true at a time.
    ///
    /// The principal is taken by reference and not as an `Option`, so a
    /// `SessionCreated` this method writes always names its payer. That is what
    /// makes the absent case in the event unambiguous — it can only mean a log
    /// older than tenancy, never "this deployment forgot".
    /// `arm` is an `Option` and not a value with a default, and the difference
    /// is the same one the principal draws: absent means *not enrolled in the
    /// experiment*, which is a real and correct state for a deployment that has
    /// not installed the validator. A default arm would enrol every such
    /// session in a study nobody is running, and the arm comparison would be
    /// computed against a control group made of sessions that were never
    /// eligible.
    pub async fn record_created(
        &mut self,
        model_policy: &str,
        principal: &Principal,
        arm: Option<Arm>,
    ) -> Result<(), SessionError> {
        self.commit(vec![SessionEventKind::SessionCreated {
            model_policy: model_policy.to_string(),
            principal: Some(principal.clone()),
            arm,
        }])
        .await?;
        Ok(())
    }

    /// Admit a turn, appending its input items.
    ///
    /// A `turn_id` that already completed short-circuits: the prior response is
    /// returned and no items are appended, which is what makes a client retry
    /// after a dropped connection safe.
    pub async fn begin_turn(
        &mut self,
        turn_id: TurnId,
        input: Vec<Item>,
    ) -> Result<TurnAdmission, SessionError> {
        if let Some(existing) = self.state.completed_response_for(&turn_id).cloned() {
            self.commit(vec![SessionEventKind::TurnDeduplicated {
                turn_id,
                response_id: existing.clone(),
            }])
            .await?;
            return Ok(TurnAdmission::Deduplicated(existing));
        }

        let response_id = ResponseId::generate();
        let mut kinds = Vec::with_capacity(input.len() + 1);
        kinds.push(SessionEventKind::TurnStarted {
            turn_id,
            response_id: response_id.clone(),
        });
        kinds.extend(
            input
                .into_iter()
                .map(|item| SessionEventKind::ItemAppended { item }),
        );
        self.commit(kinds).await?;
        Ok(TurnAdmission::Started(response_id))
    }

    /// Record the routing choice before any execution begins.
    ///
    /// This is the audit trail, not yet ledger evidence: the dispatch is only
    /// folded into the cache ledger once its response terminates. See
    /// [`SessionState`]'s pending routings.
    pub async fn record_routing(
        &mut self,
        response_id: &ResponseId,
        decision: DecisionRecord,
    ) -> Result<(), SessionError> {
        self.commit(vec![SessionEventKind::Routed {
            response_id: response_id.clone(),
            decision,
        }])
        .await?;
        Ok(())
    }

    pub async fn append_output(
        &mut self,
        response_id: &ResponseId,
        text: impl Into<String>,
    ) -> Result<SessionEvent, SessionError> {
        let mut events = self
            .commit(vec![SessionEventKind::OutputTextDelta {
                response_id: response_id.clone(),
                text: text.into(),
            }])
            .await?;
        events
            .pop()
            .ok_or_else(|| SessionError::ResponseNotOpen(response_id.clone()))
    }

    /// Close a response successfully, committing the assistant item.
    ///
    /// The dispatched turn's spelling of [`Session::complete_with_item`], and
    /// expressed in terms of it rather than beside it: the atomicity rule — the
    /// produced item and the terminal event in one append batch — is a property
    /// of *completing*, not of completing with a particular shape of item, and
    /// stating it twice is how the two spellings drift apart.
    pub async fn complete(
        &mut self,
        response_id: &ResponseId,
        text: impl Into<String>,
        usage: Usage,
    ) -> Result<(), SessionError> {
        self.complete_with_item(
            response_id,
            Item::assistant_text(text, response_id.clone()),
            usage,
            ControlRecord::default(),
        )
        .await
    }

    /// Close a response successfully, committing `item` as what it produced.
    ///
    /// The one way a response completes. [`Session::complete`] is this method
    /// with the item fixed to assistant text, which is the shape every
    /// dispatched turn produces; a steered turn passes its synthetic tool call
    /// instead. Both spellings commit the same two events in the same batch
    /// because they are the same method, rather than because two methods agree.
    ///
    /// **The response id is stamped here rather than read off the item.** That
    /// is what makes a committed item bearing a response id mean "this response
    /// emitted it": everything on the input path arrives through
    /// [`Session::begin_turn`] carrying none, and this is the only method that
    /// puts one on anything at all. A caller that had to supply the stamp
    /// itself could forget it, and a forgotten stamp is an emitted call that no
    /// projection can tell from a client's own.
    ///
    /// **One append batch, never two.** The item and the completion are a
    /// decision and its realization. Committed separately, a process that died
    /// between them would leave a session holding an emitted tool call whose
    /// turn never completed: the retry would not deduplicate, the same call
    /// would be emitted a second time, and the client would hold two calls with
    /// one id. The store numbers a batch contiguously within one call, so the
    /// window does not exist rather than being narrow — the same property
    /// [`Session::begin_turn`] admits a turn and its input under.
    ///
    /// **Nothing here records a [`SessionEventKind::Routed`]**, and for the
    /// steered caller that absence is the point rather than an omission: the
    /// usage passed is whatever produced the interjection, and the turn itself
    /// dispatched nothing. A turn that reaches its completion with no routing
    /// of its own is priced at nothing by every projection that pairs a
    /// dispatch with its terminal event — the metrics fold books no model row
    /// for it and the cache ledger records no warm prefix — while the client is
    /// still told what its turn genuinely cost. (A dispatched turn recorded its
    /// own `Routed` before it got here, so the same absence costs it nothing.)
    /// See
    /// [`interject`](crate::interject) for why the decision is taken before the
    /// turn is planned, which is what makes that pairing absent rather than
    /// merely unused.
    /// **`record` goes in the same batch, and ahead of the item.** The facts an
    /// interjector produced are the *reason* this completion exists; committed
    /// afterwards, a crash between them would leave a steered turn in the log
    /// with no record of what decided it, and the arm comparison would count
    /// the intervention against no validation. Ahead of the item rather than
    /// behind it because the log then reads in causal order — the decision, and
    /// then what it produced.
    pub async fn complete_with_item(
        &mut self,
        response_id: &ResponseId,
        item: Item,
        usage: Usage,
        record: ControlRecord,
    ) -> Result<(), SessionError> {
        let item = Item {
            response_id: Some(response_id.clone()),
            ..item
        };
        let mut kinds = record.into_kinds();
        kinds.push(SessionEventKind::ItemAppended { item });
        kinds.push(SessionEventKind::ResponseCompleted {
            response_id: response_id.clone(),
            usage,
        });
        self.commit(kinds).await?;
        Ok(())
    }

    /// Commit facts an interjector produced for a turn that then proceeds.
    ///
    /// The `Proceed` half of the same contract [`Self::complete_with_item`]
    /// serves for `Complete`. There is no atomicity to buy here — nothing else
    /// is being committed alongside — but there is still exactly one writer,
    /// and this is how an occupant reaches it.
    ///
    /// An empty record commits nothing rather than an empty batch: the
    /// production default interjects on no turn, and a store round trip per
    /// turn to say so would be a cost paid by every deployment that never
    /// enables validation.
    pub async fn record_control(&mut self, record: ControlRecord) -> Result<(), SessionError> {
        if record.is_empty() {
            return Ok(());
        }
        self.commit(record.into_kinds()).await?;
        Ok(())
    }

    /// Close a response without a complete answer.
    ///
    /// The partial text is committed as an assistant item so the successor can
    /// resume from it. That partial is also, conveniently, a guaranteed cache
    /// hit on the target that produced it.
    pub async fn mark_incomplete(
        &mut self,
        response_id: &ResponseId,
        partial: impl Into<String>,
        reason: IncompleteReason,
        usage: Usage,
    ) -> Result<(), SessionError> {
        let partial = partial.into();
        let mut kinds = Vec::with_capacity(2);
        if !partial.is_empty() {
            kinds.push(SessionEventKind::ItemAppended {
                item: Item::assistant_text(partial, response_id.clone()),
            });
        }
        kinds.push(SessionEventKind::ResponseIncomplete {
            response_id: response_id.clone(),
            reason,
            usage,
        });
        self.commit(kinds).await?;
        Ok(())
    }

    /// Events after `after_seq`. Backs both SSE resumption and reconnect replay.
    pub async fn events_since(
        &self,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        Ok(self
            .store
            .read_events(&self.session_id, after_seq, limit)
            .await?)
    }

    /// Events belonging to one response, for the Responses API view.
    pub async fn response_events(
        &self,
        response_id: &ResponseId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        Ok(self
            .store
            .read_events(&self.session_id, after_seq, limit)
            .await?
            .into_iter()
            .filter(|event| event.response_id() == Some(response_id))
            .collect())
    }
}

#[cfg(test)]
mod tests;
