// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! ATOF events, from the session log.
//!
//! Relay's runtime emits these live; roundhouse produces them by replaying a
//! durable log, which is the whole argument for emitting theirs rather than
//! inventing ours — a stream that survives the death of the process that made it
//! is a strictly better producer of the same format.
//!
//! # The shape of one session
//!
//! ```text
//! scope start  agent    <- the session; parent of everything below
//!   scope start/end  context  <- one routing decision, data_schema-tagged
//!   scope start/end  llm      <- one dispatched turn
//!   ... per turn ...
//! scope end    agent
//! ```
//!
//! **One session-wide `agent` scope, and every other event names it as its
//! parent.** Both halves are load-bearing against the shipped ATOF→ATIF
//! converter, not decoration. It finds the trajectory root by looking for the
//! `agent` scope-start with no parent; without one it falls back to whichever
//! parentless scope it meets first. And it deduplicates repeated input messages
//! per `(parent_uuid, role)` — so turns that did not share a parent would each
//! re-emit the conversation's history as fresh user steps, which is exactly what
//! a client resending its history produces.
//!
//! # Why routing decisions are scopes and not marks
//!
//! S2 asks for "a declared `data_schema` for our routing marks so the existing
//! converter consumes them without new code". Read against the converter as
//! shipped, the mark path does not deliver that: `MARK_EXTRACTOR_REGISTRY` is
//! empty, an unregistered `(name, version)` silently falls through to a default
//! that lifts `data.role`-shaped payloads and otherwise emits
//! `json.dumps(data)` as an opaque string in a system step — **with no `extra`
//! and no `data_schema` at all**. The declaration would survive the wire and
//! die at the consumer.
//!
//! A `category: "context"` **scope-end** is the one path that copies
//! `data_schema` into the ATIF step's `extra` verbatim
//! (`atof_to_atif_converter.py`, the R10 branch). So that is what a routing
//! decision is emitted as. The alternative — marks, plus a twenty-line
//! `register_mark_extractor` pull request against NeMo-Agent-Toolkit — is a
//! better long-term shape and is on the contribution list; until it lands, a
//! roundhouse export must be consumable by the converter people actually have.
//!
//! A scope is a span, so the decision is emitted as a start/end **pair** sharing
//! one uuid even though only the end carries the payload. A lone end would be a
//! malformed span, and the cost of the start is one line of NDJSON.
//!
//! # The one hard failure this format has, and how it is avoided
//!
//! The converter's LLM path raises `ShapeMismatchError` when a **non-empty**
//! `data` yields no assistant content and no tool calls — a hard failure, not a
//! silent degrade. Roundhouse produces exactly that case routinely: a turn
//! refused by policy or by a spent budget has no answer and no tool call. So a
//! turn with nothing to say emits `data: None` rather than an empty envelope,
//! and the payload is built only when there is text or a call to put in it. See
//! [`tests::a_turn_with_no_answer_carries_no_payload_at_all`].

use nemo_relay_types::api::event::{
    BaseEvent, CategoryProfile, DataSchema, Event, EventCategory, ScopeCategory, ScopeEvent,
};
use roundhouse_core::event::SessionEvent;
use roundhouse_core::item::ItemContent;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ids::{self, Name};
use crate::replay::{SessionReplay, TurnRecord};
use crate::wire::{route_facts, route_schema, timestamp};

/// The schema our LLM scopes declare their `data` under.
///
/// Declared rather than left absent, even though the converter's default
/// extractor is this same one: an absent schema means "guess", and a producer
/// that means OpenAI chat-completions should say so — the day the default
/// changes, a declared payload keeps being read the way it was written.
const OPENAI_CHAT_COMPLETIONS: (&str, &str) = ("openai/chat-completions", "1");

/// One session's ATOF event stream, in log order.
pub fn events(events: &[SessionEvent]) -> Vec<Event> {
    from_replay(&SessionReplay::of(events))
}

/// The same, from a replay a caller already has.
pub fn from_replay(replay: &SessionReplay) -> Vec<Event> {
    if replay.turns.is_empty() && replay.first_at_ms.is_none() {
        return Vec::new();
    }
    let session_uuid = ids::derive(&replay.session_id, &Name::session());
    let opened = replay.first_at_ms.unwrap_or(0);
    let closed = replay.last_at_ms.unwrap_or(opened);

    let mut stream = Vec::with_capacity(replay.turns.len() * 4 + 2);
    stream.push(session_scope(
        replay,
        session_uuid,
        ScopeCategory::Start,
        opened,
    ));
    for turn in &replay.turns {
        if turn.decision().is_some() {
            stream.extend(route_scope(replay, session_uuid, turn));
        }
        stream.extend(turn_scope(replay, session_uuid, turn));
    }
    stream.push(session_scope(
        replay,
        session_uuid,
        ScopeCategory::End,
        closed,
    ));
    stream
}

/// The stream as NDJSON — one event per line, which is how ATOF is stored and
/// what the converter's `read_jsonl` expects.
pub fn ndjson(events: &[Event]) -> String {
    let mut out = String::new();
    for event in events {
        // `to_json_string` is Relay's own serializer for the tagged union, so
        // the `kind` discriminant is theirs rather than ours to get right.
        if let Ok(line) = event.to_json_string() {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// The session-wide agent scope every other event hangs off.
fn session_scope(replay: &SessionReplay, uuid: Uuid, phase: ScopeCategory, at_ms: u64) -> Event {
    Event::Scope(ScopeEvent::new(
        BaseEvent {
            atof_version: nemo_relay_types::api::event::ATOF_VERSION.to_string(),
            // The root, and its parentlessness is what makes it the root to the
            // converter. Nothing else in this stream may be parentless.
            parent_uuid: None,
            uuid,
            timestamp: timestamp(at_ms),
            name: replay.session_id.as_str().to_string(),
            data: None,
            data_schema: None,
            metadata: Some(json!({
                "session_id": replay.session_id.as_str(),
                "model_policy": replay.model_policy,
                "principal": replay.principal,
            })),
        },
        phase,
        Vec::new(),
        EventCategory::agent(),
        None,
    ))
}

/// One routing decision, as the context scope the converter preserves.
fn route_scope(replay: &SessionReplay, parent: Uuid, turn: &TurnRecord) -> Vec<Event> {
    let uuid = ids::derive(&replay.session_id, &Name::route(turn.response_id.as_str()));
    let at_ms = turn.routed_at_ms.unwrap_or(turn.started_at_ms);
    let base = |data: Option<Value>, data_schema: Option<DataSchema>| BaseEvent {
        atof_version: nemo_relay_types::api::event::ATOF_VERSION.to_string(),
        parent_uuid: Some(parent),
        uuid,
        timestamp: timestamp(at_ms),
        name: "roundhouse.route".to_string(),
        data,
        data_schema,
        metadata: Some(json!({
            "session_id": replay.session_id.as_str(),
            "session_seq": turn.started_seq,
        })),
    };
    vec![
        // The payload rides the end alone, because the end is the only half the
        // converter reads. Putting it on both would double every decision in
        // any consumer that reads starts too.
        Event::Scope(ScopeEvent::new(
            base(None, None),
            ScopeCategory::Start,
            Vec::new(),
            EventCategory::new("context"),
            None,
        )),
        Event::Scope(ScopeEvent::new(
            base(Some(route_facts(turn)), Some(route_schema())),
            ScopeCategory::End,
            Vec::new(),
            EventCategory::new("context"),
            None,
        )),
    ]
}

/// One dispatched turn, as an LLM scope.
fn turn_scope(replay: &SessionReplay, parent: Uuid, turn: &TurnRecord) -> Vec<Event> {
    let uuid = ids::derive(&replay.session_id, &Name::turn(turn.response_id.as_str()));
    let model = turn.decision().map(|d| d.chosen.model().to_string());
    let profile = Some(CategoryProfile {
        model_name: model.clone(),
        ..CategoryProfile::default()
    });
    let base = |at_ms: u64, data: Option<Value>| BaseEvent {
        atof_version: nemo_relay_types::api::event::ATOF_VERSION.to_string(),
        parent_uuid: Some(parent),
        uuid,
        timestamp: timestamp(at_ms),
        name: model
            .clone()
            .unwrap_or_else(|| "roundhouse.turn".to_string()),
        // A schema is a claim about a payload, so it is declared only where
        // there is one. Declaring it beside `data: None` would tell a consumer
        // to expect a shape that is not there.
        data_schema: data.is_some().then(|| DataSchema {
            name: OPENAI_CHAT_COMPLETIONS.0.to_string(),
            version: OPENAI_CHAT_COMPLETIONS.1.to_string(),
        }),
        data,
        metadata: Some(json!({
            "session_id": replay.session_id.as_str(),
            "session_seq": turn.started_seq,
            "response_id": turn.response_id.as_str(),
            "turn_id": turn.turn_id.as_str(),
        })),
    };
    let scope = |at_ms: u64, data: Option<Value>, phase: ScopeCategory| {
        Event::Scope(ScopeEvent::new(
            base(at_ms, data),
            phase,
            Vec::new(),
            EventCategory::llm(),
            profile.clone(),
        ))
    };
    vec![
        scope(
            turn.routed_at_ms.unwrap_or(turn.started_at_ms),
            request_payload(turn),
            ScopeCategory::Start,
        ),
        scope(
            turn.ended_at_ms.unwrap_or(turn.started_at_ms),
            response_payload(turn),
            ScopeCategory::End,
        ),
    ]
}

/// What the client sent this turn, in OpenAI chat-completions shape.
///
/// `None` when the turn admitted nothing new — which is ordinary, since prefix
/// admission appends only the suffix and a client can resend a history with
/// nothing added to it.
fn request_payload(turn: &TurnRecord) -> Option<Value> {
    let messages: Vec<Value> = turn
        .input
        .iter()
        .filter_map(|item| match &item.content {
            ItemContent::Text { text } => Some(json!({
                "role": item.role.as_str(),
                "content": text,
            })),
            // A tool result is the client answering a call the deployment made.
            // It is a message on this wire and an *observation* in ATIF, and
            // the converter drops it from the input side for exactly that
            // reason — so it is carried here for fidelity and costs nothing.
            ItemContent::ToolResult { call_id, output } => Some(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output,
            })),
            // Dropped, for the reason the tool-call arm above is dropped: ATOF's
            // chat-completions shape has no field for any of them. The nearest
            // fit would be an assistant message whose `content` is the
            // reasoning, and that is worse than omission — it publishes the
            // model's scratch space into an instrumentation feed *as the
            // answer*, where a downstream consumer scoring response quality
            // would read deliberation as output. An opaque block is a shape
            // nobody here has interpreted, and inventing a message body for it
            // would be interpreting it.
            ItemContent::ToolCall { .. }
            | ItemContent::Thinking { .. }
            | ItemContent::RedactedThinking { .. }
            | ItemContent::Opaque { .. } => None,
        })
        .collect();
    (!messages.is_empty()).then(|| json!({ "messages": messages }))
}

/// What the deployment answered, in the same shape.
///
/// **`None` unless there is content or a tool call**, and this is the one place
/// in the crate where a wrong `Some` is a hard failure downstream rather than a
/// cosmetic one: the converter raises `ShapeMismatchError` on a non-empty
/// payload that extracts to neither. A refused turn, a budget-exhausted turn and
/// a turn whose upstream died before the first token all land here with nothing
/// to say, and all three are ordinary.
fn response_payload(turn: &TurnRecord) -> Option<Value> {
    let text = if turn.text.is_empty() {
        None
    } else {
        Some(turn.text.clone())
    };
    let tool_calls: Vec<Value> = turn
        .output
        .iter()
        .filter_map(|item| match &item.content {
            // Not published, for the reason `atif::tool_calls` states: this is
            // the OpenAI chat-completions tool-call shape, which spells a
            // function name flat and has no namespace field at all (M17).
            ItemContent::ToolCall {
                call_id,
                name,
                arguments,
                namespace: _,
            } => Some(json!({
                "id": call_id,
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            })),
            _ => None,
        })
        .collect();
    if text.is_none() && tool_calls.is_empty() {
        return None;
    }

    let mut message = json!({ "role": "assistant" });
    if let Some(text) = text {
        message["content"] = Value::String(text);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    Some(json!({
        "choices": [{ "index": 0, "message": message }],
        "usage": {
            "prompt_tokens": turn.usage.input_tokens,
            "completion_tokens": turn.usage.output_tokens,
            "total_tokens": turn.usage.total(),
            "cache_read_tokens": turn.usage.cached_input_tokens,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, Log};
    use roundhouse_core::event::IncompleteReason;

    fn stream(log: &Log) -> Vec<Event> {
        events(log.events())
    }

    #[test]
    fn a_session_is_one_agent_scope_and_everything_hangs_off_it() {
        let mut log = Log::new("acme/ada/main");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(1_000, 0, 50),
        );
        log.turn(
            "t2",
            "r2",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(1_000, 0, 50),
        );

        let stream = stream(&log);
        let roots: Vec<&Event> = stream
            .iter()
            .filter(|event| event.parent_uuid().is_none())
            .collect();
        assert_eq!(roots.len(), 2, "one agent span: its start and its end");
        assert_eq!(
            roots[0].category().map(EventCategory::as_str),
            Some("agent")
        );

        let root_uuid = roots[0].uuid();
        assert!(
            stream
                .iter()
                .filter(|event| event.uuid() != root_uuid)
                .all(|event| event.parent_uuid() == Some(root_uuid)),
            "every turn shares one parent, or the converter re-emits the \
             conversation's history as fresh user steps on every turn"
        );
    }

    #[test]
    fn a_routing_decision_is_a_context_scope_end_carrying_its_schema() {
        let mut log = Log::new("s1");
        log.created(None);
        log.routed_turn(
            "t1",
            "r1",
            fixtures::decision(
                fixtures::local("llama"),
                vec![fixtures::candidate(
                    fixtures::frontier("anthropic", "claude"),
                    0.05,
                )],
            ),
            fixtures::usage(1_000, 0, 50),
        );

        let stream = stream(&log);
        let ends: Vec<&Event> = stream
            .iter()
            .filter(|event| {
                event.category().map(EventCategory::as_str) == Some("context")
                    && event.is_scope_end()
            })
            .collect();
        assert_eq!(ends.len(), 1);
        let schema = ends[0].data_schema().expect("the declaration");
        assert_eq!(schema.name, "roundhouse/route");
        assert_eq!(schema.version, "1");
        assert_eq!(ends[0].data().unwrap()["serving_mode"], "local");
        assert_eq!(
            ends[0].data().unwrap()["quoted_frontier_alternative_usd"],
            0.05
        );

        // The start of the same span carries no payload: the converter reads
        // only the end, and a payload on both would double every decision for a
        // consumer that reads starts too.
        let starts: Vec<&Event> = stream
            .iter()
            .filter(|event| {
                event.category().map(EventCategory::as_str) == Some("context")
                    && event.is_scope_start()
            })
            .collect();
        assert_eq!(starts.len(), 1, "a scope is a span, so the end has a start");
        assert!(starts[0].data().is_none());
        assert_eq!(
            starts[0].uuid(),
            ends[0].uuid(),
            "and the two halves are one span"
        );
    }

    #[test]
    fn a_dispatched_turn_carries_the_conversation_in_the_declared_shape() {
        let mut log = Log::new("s1");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::frontier("anthropic", "claude"),
            fixtures::usage(1_000, 0, 50),
        );

        let stream = stream(&log);
        let start = stream
            .iter()
            .find(|event| {
                event.category().map(EventCategory::as_str) == Some("llm") && event.is_scope_start()
            })
            .expect("the dispatch");
        assert_eq!(
            start.data_schema().map(|s| s.name.as_str()),
            Some("openai/chat-completions")
        );
        assert_eq!(start.data().unwrap()["messages"][0]["role"], "user");
        assert_eq!(start.model_name(), Some("claude"));

        let end = stream
            .iter()
            .find(|event| {
                event.category().map(EventCategory::as_str) == Some("llm") && event.is_scope_end()
            })
            .expect("the answer");
        assert_eq!(
            end.data().unwrap()["choices"][0]["message"]["content"],
            "answer t1"
        );
    }

    #[test]
    fn a_tool_call_answer_is_a_tool_call_on_the_wire() {
        let mut log = Log::new("s1");
        log.created(None);
        log.tool_call_turn("t1", "r1", "call_1", "grep", r#"{"q":"x"}"#);

        let stream = stream(&log);
        let end = stream
            .iter()
            .find(|event| {
                event.category().map(EventCategory::as_str) == Some("llm") && event.is_scope_end()
            })
            .unwrap();
        let call = &end.data().unwrap()["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["function"]["name"], "grep");
        assert!(
            end.data().unwrap()["choices"][0]["message"]
                .get("content")
                .is_none(),
            "a call with no prose beside it carries no content key, which the \
             converter treats as legitimate rather than as a shape mismatch"
        );
    }

    /// The one payload rule whose violation is a *hard* failure downstream.
    #[test]
    fn a_turn_with_no_answer_carries_no_payload_at_all() {
        let mut log = Log::new("s1");
        log.created(None);
        log.refused_turn("t1", "r1", IncompleteReason::PolicyRefused);

        let stream = stream(&log);
        let end = stream
            .iter()
            .find(|event| {
                event.category().map(EventCategory::as_str) == Some("llm") && event.is_scope_end()
            })
            .expect("a refused turn is still a turn");
        assert!(
            end.data().is_none(),
            "an empty envelope here raises ShapeMismatchError in the shipped \
             converter -- a non-empty `data` that extracts to neither content \
             nor a tool call is a hard failure, not a silent degrade"
        );
        assert!(
            end.data_schema().is_none(),
            "and a schema without a payload is a claim about nothing"
        );
    }

    #[test]
    fn two_streams_from_one_log_are_byte_identical() {
        let mut log = Log::new("acme/ada/main");
        log.created(None);
        log.turn(
            "t1",
            "r1",
            fixtures::local("llama"),
            fixtures::usage(1_000, 0, 50),
        );

        let first = ndjson(&stream(&log));
        let second = ndjson(&stream(&log));
        assert_eq!(
            first, second,
            "every id is a v5 digest of the log; a v4 or a v7 would make two \
             exports of one finished session differ in every uuid"
        );
        assert_eq!(
            first.lines().count(),
            6,
            "session start, route start/end, llm start/end, session end"
        );
        for line in first.lines() {
            let parsed: Event = serde_json::from_str(line).expect("each line is one ATOF event");
            assert!(parsed.uuid() != Uuid::nil());
        }
    }

    #[test]
    fn an_empty_log_produces_no_stream() {
        assert!(events(&[]).is_empty());
    }
}
