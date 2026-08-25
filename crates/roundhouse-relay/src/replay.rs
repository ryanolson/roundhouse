// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! One cold replay, three documents.
//!
//! All three producers in this crate answer the same question first — *what
//! happened in this session, turn by turn* — and they answered it three times
//! before this module existed. That is the shape that drifts: a fix to how an
//! abandoned dispatch is recognized would land in one walk and not the other
//! two, and the ATOF stream would then describe a session the trajectory does
//! not.
//!
//! So the walk is here, once, and it is the only place in this crate that knows
//! the event vocabulary. [`atof`](crate::atof), [`atif`](crate::atif) and
//! [`summary`](crate::summary) consume [`SessionReplay`] and never a
//! `SessionEvent`.
//!
//! # What a turn is here
//!
//! The engine writes a turn as `TurnStarted`, then the input items it admitted,
//! then `Routed`, then output deltas and items, then a terminal event. So the
//! items belonging to a turn are unambiguous from position alone: everything
//! before its `Routed` is what the client sent, everything after is what the
//! deployment produced. Nothing needs a turn id on `ItemAppended`, which is
//! good, because it does not carry one.
//!
//! **Only the suffix arrives.** The Responses surface checks a client's resent
//! conversation against the log as a prefix and admits only what is new, so a
//! turn's `input` here is the new messages and not the whole history. Every
//! document this crate produces inherits that: no step, event or summary
//! restates a message an earlier turn already carried.

use roundhouse_core::control::Principal;
use roundhouse_core::event::{
    IncompleteReason, SessionEvent, SessionEventKind, Usage, ValidationOutcome,
};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::routing::DecisionRecord;
use roundhouse_core::validate::Arm;

/// How a turn ended.
///
/// Four states rather than `Option<IncompleteReason>`, because "no terminal
/// event" has two meanings a document has to keep apart: a turn still running
/// when the log was read, and a turn whose owner died mid-dispatch and whose
/// client retried under a fresh response id. The second is a *fact* — a second
/// `TurnStarted` for one turn id is positive proof the first response will never
/// terminate, which is the same supersession rule the metrics fold uses — and a
/// trajectory that presented it as "still running" would be describing a session
/// that ended some time ago.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// No terminal event, and no retry either: the log simply ends here.
    Open,
    /// The client re-admitted this turn id, so this response was abandoned.
    Superseded,
    Completed,
    Incomplete(IncompleteReason),
}

impl TurnOutcome {
    /// Whether this turn reached an end of any kind.
    pub fn is_settled(&self) -> bool {
        matches!(self, TurnOutcome::Completed | TurnOutcome::Incomplete(_))
    }
}

/// One turn, as the log recorded it.
#[derive(Debug, Clone)]
pub struct TurnRecord {
    pub turn_id: TurnId,
    pub response_id: ResponseId,
    /// `None` for a turn that was admitted and never routed — refused before a
    /// target was chosen, or still being decided when the log was read.
    pub decision: Option<DecisionRecord>,
    /// What the terminal event reported. Empty for a turn that has none.
    pub usage: Usage,
    pub outcome: TurnOutcome,
    /// `TurnStarted`'s sequence number: this turn's position in the session, and
    /// a stable ordering key that does not depend on wall-clock ties.
    pub started_seq: u64,
    pub started_at_ms: u64,
    /// When the routing decision was recorded — the moment the dispatch began,
    /// which is what an ATOF LLM scope opens at. Absent for a turn that was
    /// never routed.
    pub routed_at_ms: Option<u64>,
    /// The terminal event's timestamp, absent while the turn is open.
    pub ended_at_ms: Option<u64>,
    /// What the client sent for this turn — the suffix, not the history.
    pub input: Vec<Item>,
    /// What the deployment committed: the answer, or a tool call it emitted.
    pub output: Vec<Item>,
    /// Streamed text, concatenated. The answer as the client received it.
    pub text: String,
    /// Whether the validate loop interjected on this turn.
    ///
    /// Carried so the divergence the crate documentation names is *visible*
    /// rather than merely admitted: a steered turn publishes as an ordinary tool
    /// call, and this is the flag a reader of our own log can join on to find
    /// which ones those were.
    pub steered: bool,
}

impl TurnRecord {
    /// Whether this turn's prompt reached a provider.
    ///
    /// A completion always consumed tokens; an incomplete only did if it reports
    /// billed input, because the engine also terminates dispatches that failed
    /// before anything was sent and those carry an empty usage. Publishing a
    /// summary for one of those would put a zero-dollar saving on a call that
    /// never happened.
    ///
    /// The same rule the metrics fold applies, spelled again rather than shared:
    /// the fold's copy is private to a `match` arm over the terminal-event
    /// vocabulary, and lifting it into core's public surface would export a
    /// judgement about *event kinds* that has no meaning outside a projection.
    /// The cost is that a third terminal kind has to be taught to both; the
    /// alternative was a public `fn consumed(&SessionEventKind, &Usage)` on
    /// core, which is a wider door for a narrower need.
    pub fn reached_a_provider(&self) -> bool {
        match self.outcome {
            TurnOutcome::Completed => true,
            TurnOutcome::Incomplete(_) => self.usage.input_tokens > 0,
            TurnOutcome::Open | TurnOutcome::Superseded => false,
        }
    }

    /// A turn this crate has anything to publish about: routed, settled, and
    /// dispatched.
    pub fn is_publishable(&self) -> bool {
        self.decision.is_some() && self.reached_a_provider()
    }

    /// The decision, for a turn known to be publishable.
    pub fn decision(&self) -> Option<&DecisionRecord> {
        self.decision.as_ref()
    }
}

/// A session as these producers see it.
#[derive(Debug, Clone)]
pub struct SessionReplay {
    pub session_id: SessionId,
    /// Whose session this is. `None` for a log written before the control plane
    /// existed — an absence that means "older than tenancy", never "unknown".
    pub principal: Option<Principal>,
    pub model_policy: Option<String>,
    /// Which arm of the validate experiment this session was enrolled in.
    pub arm: Option<Arm>,
    pub turns: Vec<TurnRecord>,
    pub first_at_ms: Option<u64>,
    pub last_at_ms: Option<u64>,
}

impl SessionReplay {
    /// Fold a log into turns.
    ///
    /// Total: there is no error case. An empty slice is an empty session, a
    /// truncated log is a session whose last turn is [`TurnOutcome::Open`], and
    /// an event kind this walk has no use for is skipped. A producer of a
    /// *report about the past* that could refuse to describe a log is a producer
    /// that goes dark exactly when something has gone wrong.
    ///
    /// `session_id` is taken from the events rather than from the caller, so the
    /// document cannot claim to be about a session the log is not.
    pub fn of(events: &[SessionEvent]) -> Self {
        let mut replay = Self {
            session_id: events
                .first()
                .map(|event| event.session_id.clone())
                .unwrap_or_else(|| SessionId::new("")),
            principal: None,
            model_policy: None,
            arm: None,
            turns: Vec::new(),
            first_at_ms: events.first().map(|event| event.at_ms),
            last_at_ms: events.last().map(|event| event.at_ms),
        };

        // The turn currently open, as an index into `turns`. Items and deltas
        // attach to it; a terminal event closes it.
        let mut open: Option<usize> = None;
        for event in events {
            match &event.kind {
                SessionEventKind::SessionCreated {
                    model_policy,
                    principal,
                    arm,
                } => {
                    replay.model_policy = Some(model_policy.clone());
                    replay.principal.clone_from(principal);
                    replay.arm = *arm;
                }
                SessionEventKind::TurnStarted {
                    turn_id,
                    response_id,
                } => {
                    // A second start for this turn id retires the first: the
                    // client would not have re-admitted it if the earlier
                    // response were ever going to terminate.
                    if let Some(earlier) = replay
                        .turns
                        .iter_mut()
                        .find(|turn| &turn.turn_id == turn_id && turn.outcome == TurnOutcome::Open)
                    {
                        earlier.outcome = TurnOutcome::Superseded;
                    }
                    replay.turns.push(TurnRecord {
                        turn_id: turn_id.clone(),
                        response_id: response_id.clone(),
                        decision: None,
                        usage: Usage::default(),
                        outcome: TurnOutcome::Open,
                        started_seq: event.seq,
                        started_at_ms: event.at_ms,
                        routed_at_ms: None,
                        ended_at_ms: None,
                        input: Vec::new(),
                        output: Vec::new(),
                        text: String::new(),
                        steered: false,
                    });
                    open = Some(replay.turns.len() - 1);
                }
                SessionEventKind::ItemAppended { item } => {
                    if let Some(turn) = open.and_then(|index| replay.turns.get_mut(index)) {
                        // Position decides, not the item: everything before the
                        // routing decision is what the client sent, everything
                        // after is what this deployment produced.
                        match turn.decision.is_some() {
                            false => turn.input.push(item.clone()),
                            true => turn.output.push(item.clone()),
                        }
                    }
                }
                SessionEventKind::Routed {
                    response_id,
                    decision,
                } => {
                    let at_ms = event.at_ms;
                    if let Some(turn) = replay.turn_mut(response_id) {
                        turn.decision = Some(decision.clone());
                        turn.routed_at_ms = Some(at_ms);
                    }
                }
                SessionEventKind::OutputTextDelta { response_id, text } => {
                    if let Some(turn) = replay.turn_mut(response_id) {
                        turn.text.push_str(text);
                    }
                }
                // The two M10 fields are bound and unread for now, not wildcarded
                // away: provider-reported dollars belong in the summary's
                // actual_cost as a ProviderReported CostEstimate, and a terminal
                // attempt belongs on the trajectory step that failed — both are
                // emission design (which Relay fields, which basis stamps), not
                // replay mechanics, and are deferred to the S2 follow-on rather
                // than half-shipped inside a merge. Binding them by name means
                // the next field the engine adds still breaks this match loudly.
                SessionEventKind::ResponseCompleted {
                    response_id,
                    usage,
                    provider_reported_cost_usd: _,
                } => {
                    if let Some(turn) = replay.turn_mut(response_id) {
                        turn.usage = usage.clone();
                        turn.outcome = TurnOutcome::Completed;
                        turn.ended_at_ms = Some(event.at_ms);
                    }
                    open = None;
                }
                SessionEventKind::ResponseIncomplete {
                    response_id,
                    reason,
                    usage,
                    terminal_attempt: _,
                } => {
                    if let Some(turn) = replay.turn_mut(response_id) {
                        turn.usage = usage.clone();
                        turn.outcome = TurnOutcome::Incomplete(reason.clone());
                        turn.ended_at_ms = Some(event.at_ms);
                    }
                    open = None;
                }
                SessionEventKind::ValidationDecided { outcome, arm, .. } => {
                    // An intervention only counts where the arm acts: the Shadow
                    // arm computes an action and takes none, and marking its
                    // turns steered would report this deployment interrupting
                    // turns it deliberately left alone.
                    let intervened = matches!(
                        outcome,
                        ValidationOutcome::Judged { action, .. } if action.intervenes()
                    );
                    if let Some(turn) = open.and_then(|index| replay.turns.get_mut(index))
                        && arm.acts()
                        && intervened
                    {
                        turn.steered = true;
                    }
                }
                // Money and control facts that belong to no turn's transcript.
                // A side call emits no conversation item and appears on no
                // client's stream, so it appears in no trajectory either; what
                // it *spent* is the metrics fold's business and is reported on
                // the dashboard, not here.
                SessionEventKind::SideCallCompleted { .. }
                | SessionEventKind::SideCallAbandoned { .. }
                | SessionEventKind::TurnDeduplicated { .. }
                | SessionEventKind::Error { .. } => {}
            }
        }
        replay
    }

    /// The turn a response id names, if this log has one.
    fn turn_mut(&mut self, response_id: &ResponseId) -> Option<&mut TurnRecord> {
        self.turns
            .iter_mut()
            .find(|turn| &turn.response_id == response_id)
    }

    /// The tool results a later turn carried back, keyed by the call they
    /// answer.
    ///
    /// Correlation runs *forward across turns* and that is not incidental: the
    /// deployment emits a tool call at the end of one turn and the client runs
    /// the tool and returns the result as input to the next. So the observation
    /// for a step is never in the same turn as the call it observes, and a
    /// producer that looked only within a turn would publish every tool call
    /// with an empty observation.
    pub fn tool_results(&self) -> Vec<(String, String)> {
        let mut results = Vec::new();
        for turn in &self.turns {
            for item in &turn.input {
                if let ItemContent::ToolResult { call_id, output } = &item.content {
                    results.push((call_id.clone(), output.clone()));
                }
            }
        }
        results
    }
}

/// Whether an item is something a person said to the agent.
///
/// Tool results are input too, and are deliberately not this: they are the
/// *observation* of a call the agent made, and emitting them as user messages
/// would put the transcript of every tool run into the conversation twice.
pub(crate) fn spoken_input(item: &Item) -> Option<(&'static str, &str)> {
    match (&item.content, item.role) {
        (ItemContent::Text { text }, Role::User) => Some(("user", text)),
        // ATIF has three sources and `developer` is not one of them. A developer
        // message is an instruction from the deployment rather than from the
        // person, which is what `system` means in every consumer of this format.
        (ItemContent::Text { text }, Role::System | Role::Developer) => Some(("system", text)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, Log};

    #[test]
    fn a_turns_items_split_on_the_routing_decision() {
        let mut log = Log::new("s1");
        log.created(Some(Principal::new("acme", "ada")));
        log.turn(
            "t1",
            "r1",
            fixtures::local("llama"),
            fixtures::usage(100, 0, 10),
        );

        let replay = SessionReplay::of(log.events());
        assert_eq!(replay.turns.len(), 1);
        let turn = &replay.turns[0];
        assert_eq!(turn.input.len(), 1, "the client's message");
        assert_eq!(turn.output.len(), 1, "the answer");
        assert_eq!(turn.outcome, TurnOutcome::Completed);
        assert_eq!(replay.principal, Some(Principal::new("acme", "ada")));
    }

    #[test]
    fn a_re_admitted_turn_supersedes_the_response_that_never_terminated() {
        let mut log = Log::new("s1");
        log.created(None);
        log.abandoned_turn("t1", "r1");
        log.turn(
            "t1",
            "r2",
            fixtures::local("llama"),
            fixtures::usage(100, 0, 10),
        );

        let replay = SessionReplay::of(log.events());
        assert_eq!(replay.turns.len(), 2);
        assert_eq!(replay.turns[0].outcome, TurnOutcome::Superseded);
        assert!(
            !replay.turns[0].is_publishable(),
            "an abandoned dispatch has nothing to publish: no terminal event \
             means no tokens and no outcome"
        );
        assert!(replay.turns[1].is_publishable());
    }

    #[test]
    fn a_dispatch_that_never_reached_the_provider_is_not_publishable() {
        let mut log = Log::new("s1");
        log.created(None);
        log.refused_turn("t1", "r1", IncompleteReason::PolicyRefused);

        let replay = SessionReplay::of(log.events());
        assert_eq!(replay.turns.len(), 1);
        assert!(
            !replay.turns[0].reached_a_provider(),
            "an empty usage on an incomplete is the engine saying nothing was sent"
        );

        // CONTROL: the same terminal kind with billed input *is* a dispatch, so
        // the assertion above is about the usage and not about the reason.
        let mut burned = Log::new("s2");
        burned.created(None);
        burned.truncated_turn("t1", "r1", fixtures::usage(4_000, 0, 0));
        let burned = SessionReplay::of(burned.events());
        assert!(burned.turns[0].reached_a_provider());
    }

    #[test]
    fn a_tool_result_answers_a_call_made_in_an_earlier_turn() {
        let mut log = Log::new("s1");
        log.created(None);
        log.tool_call_turn("t1", "r1", "call_1", "grep", r#"{"q":"x"}"#);
        log.tool_result_turn("t2", "r2", "call_1", "3 matches");

        let replay = SessionReplay::of(log.events());
        assert_eq!(
            replay.tool_results(),
            vec![("call_1".to_string(), "3 matches".to_string())]
        );
        assert!(
            replay.turns[0]
                .output
                .iter()
                .any(|item| matches!(&item.content, ItemContent::ToolCall { .. })),
            "the call belongs to the turn that emitted it"
        );
    }
}
