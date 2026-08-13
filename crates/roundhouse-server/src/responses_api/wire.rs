// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Responses API wire vocabulary.
//!
//! Pure translation, no state: what a request's JSON items mean as canonical
//! [`Item`]s, what a conversation's content hash is, and what each `response.*`
//! frame looks like on the way out. The endpoint and its follower live in the
//! parent module; everything here is a function of its arguments, which is what
//! keeps the conformance surface testable without a store or an engine.

use axum::response::sse::Event;
use serde_json::{Value, json};

use roundhouse_core::event::{IncompleteReason, Usage};
use roundhouse_core::ids::{ResponseId, TurnId};
use roundhouse_core::item::{Item, ItemContent, Role};

use crate::http::ApiError;

/// The id of the one message item a response produces.
///
/// One assistant message per turn, so a fixed id is enough. The prefix is not
/// decoration: a client discards an item id that has none, and an item it cannot
/// name is an item it cannot attach deltas to.
const MESSAGE_ITEM_ID: &str = "msg_1";

// ---------------------------------------------------------------------------
// Canonicalizing a resent conversation
// ---------------------------------------------------------------------------

/// Convert `instructions` and `input` into canonical items.
///
/// The result is compared against the session's own items, so this has to be a
/// function of the request alone: the same conversation must canonicalize the
/// same way on every turn, on every node, or the prefix check fails and the
/// session forks.
pub(super) fn canonicalize(instructions: &str, input: &[Value]) -> Result<Vec<Item>, ApiError> {
    let mut items = Vec::with_capacity(input.len() + 1);
    // Sent whole on every turn and always first, which is exactly how it was
    // stored on the first turn.
    if !instructions.is_empty() {
        items.push(Item::system_text(instructions));
    }
    for value in input {
        if let Some(item) = canonical_item(value)? {
            items.push(item);
        }
    }
    Ok(items)
}

/// One input item, or `None` for one this model deliberately does not keep.
fn canonical_item(value: &Value) -> Result<Option<Item>, ApiError> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::unprocessable("every input item needs a `type`"))?;

    match kind {
        "message" => {
            // `developer` stays developer rather than collapsing into `system`.
            // The canonical model has the role, the two render differently, and
            // folding them here would make a conversation that used both
            // unreconstructable from the log it was written to.
            let role = match value.get("role").and_then(Value::as_str) {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                Some("system") => Role::System,
                Some("developer") => Role::Developer,
                Some(other) => {
                    return Err(ApiError::unprocessable(format!(
                        "message role `{other}` is not a role this model has"
                    )));
                }
                None => return Err(ApiError::unprocessable("a message item needs a `role`")),
            };
            Ok(Some(Item {
                role,
                content: ItemContent::Text {
                    text: message_text(value)?,
                },
                response_id: None,
            }))
        }
        "function_call" => Ok(Some(Item {
            role: Role::Assistant,
            content: ItemContent::ToolCall {
                call_id: required_str(value, "call_id")?,
                name: required_str(value, "name")?,
                arguments: required_str(value, "arguments")?,
            },
            response_id: None,
        })),
        "function_call_output" => Ok(Some(Item {
            role: Role::Tool,
            content: ItemContent::ToolResult {
                call_id: required_str(value, "call_id")?,
                output: output_text(value)?,
            },
            response_id: None,
        })),
        // The model's own scratch space, which has no canonical item yet.
        // Dropped rather than refused — refusing would lock out every client
        // that reasons — and dropped on every request alike, which is what
        // keeps the prefix a client claims equal to the prefix we stored.
        "reasoning" => Ok(None),
        other => Err(ApiError::unprocessable(format!(
            "input item type `{other}` is not supported"
        ))),
    }
}

/// A message's text, from typed content parts or a bare string.
///
/// Parts are concatenated into one item rather than becoming one item each,
/// because that is the shape this surface emits: an answer goes out as a single
/// `output_text` part, and it has to canonicalize back to the single assistant
/// item the log holds when the client sends it again next turn.
fn message_text(value: &Value) -> Result<String, ApiError> {
    match value.get("content") {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text") => {
                        text.push_str(
                            part.get("text").and_then(Value::as_str).ok_or_else(|| {
                                ApiError::unprocessable("a text part needs `text`")
                            })?,
                        );
                    }
                    Some(other) => {
                        return Err(ApiError::unprocessable(format!(
                            "message content of type `{other}` is not supported"
                        )));
                    }
                    None => {
                        return Err(ApiError::unprocessable("every content part needs a `type`"));
                    }
                }
            }
            Ok(text)
        }
        Some(_) | None => Err(ApiError::unprocessable(
            "a message item needs `content` as a string or a list of typed parts",
        )),
    }
}

/// A tool result's output as one string.
///
/// The wire form is either a string or a list of structured content items. The
/// canonical item carries a string, so the structured form is kept as its own
/// JSON encoding rather than flattened to the text inside it: flattening would
/// discard what a tool returned, and would make two different outputs
/// canonicalize identically.
fn output_text(value: &Value) -> Result<String, ApiError> {
    match value.get("output") {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(other) => Ok(other.to_string()),
        None => Err(ApiError::unprocessable(
            "a function_call_output item needs `output`",
        )),
    }
}

fn required_str(value: &Value, field: &str) -> Result<String, ApiError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            ApiError::unprocessable(format!("`{field}` is required and must be a string"))
        })
}

// ---------------------------------------------------------------------------
// The turn id
// ---------------------------------------------------------------------------

/// A turn id that is a function of the conversation rather than of the request.
///
/// Clients of this API retry on their own — a 5xx, a stream that died
/// mid-answer — and there is no idempotency key on the wire for them to hold
/// steady across those attempts. An id minted per request would make each retry
/// a new turn: a second answer, generated and billed, for a question already
/// answered. Hashing the canonicalized conversation makes the retry identical to
/// its original by construction, and the engine replays rather than regenerates.
pub(super) fn turn_id_for(items: &[Item]) -> TurnId {
    // Renders concatenate unambiguously: `Item::render` prefixes `<|role|>`, so
    // every item is self-delimiting and no separator is needed to keep two
    // different conversations from hashing to one string. This is the same
    // property `ContextAssembler::rendered` relies on.
    let mut hash = FNV_OFFSET;
    for item in items {
        for byte in item.render().bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    TurnId::new(format!("turn_{hash:016x}"))
}

/// FNV-1a, written out rather than reached for.
///
/// The id is durable: a client may retry minutes later, and a successor node
/// must derive the same id from the same conversation, so a hash that is only
/// stable within one build — `DefaultHasher` is documented not to be stable
/// across releases — cannot be used. The block hashes Roundhouse routes on are
/// Dynamo's, over token ids, and answer a different question; this one only has
/// to be deterministic, cheap, and spread out enough that two conversations do
/// not collide.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// One SSE frame.
///
/// The name goes on the `event:` line and again as the payload's own `type`,
/// because clients of this API read the type out of the JSON and ignore the
/// line; the line is for whoever is reading the stream with their eyes. No `id`
/// is set: the only ids available are the log's sequence numbers, this surface
/// has no resume protocol to spend them on, and emitting them would set a
/// `Last-Event-ID` that means nothing to a client that reconnects.
fn frame(name: &str, payload: Value) -> Event {
    Event::default().event(name).data(payload.to_string())
}

pub(super) fn created_frame(response_id: &ResponseId) -> Event {
    frame(
        "response.created",
        json!({ "type": "response.created", "response": { "id": response_id } }),
    )
}

pub(super) fn item_added_frame() -> Event {
    frame(
        "response.output_item.added",
        json!({ "type": "response.output_item.added", "item": message_item("") }),
    )
}

pub(super) fn delta_frame(text: &str) -> Event {
    frame(
        "response.output_text.delta",
        json!({
            "type": "response.output_text.delta",
            "item_id": MESSAGE_ITEM_ID,
            "delta": text,
        }),
    )
}

pub(super) fn item_done_frame(text: &str) -> Event {
    frame(
        "response.output_item.done",
        json!({ "type": "response.output_item.done", "item": message_item(text) }),
    )
}

/// The assistant message, in the shape the Responses API defines for it.
///
/// The content entry is a typed `output_text` part rather than a bare string
/// because of how a client handles the difference: an item whose type it knows
/// but whose shape it cannot parse is dropped in silence, so the turn arrives
/// looking empty rather than looking wrong.
fn message_item(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "id": MESSAGE_ITEM_ID,
        "content": [{ "type": "output_text", "text": text }],
    })
}

/// `response.completed`, which ends the stream.
///
/// `id` and `total_tokens` are load-bearing: a client parses this event into its
/// own accounting, and a completion it cannot parse is a turn it treats as
/// failed. `cached_input_tokens` goes out as `input_tokens_details.cached_tokens`
/// — the quantity this whole system exists to maximize, in the field a Responses
/// client already reads. `cache_write_tokens` stays zero because no provider
/// Roundhouse routes to reports it separately yet, and a number invented here
/// would be billed as if it had been measured.
pub(super) fn completed_frame(response_id: &ResponseId, usage: &Usage) -> Event {
    frame(
        "response.completed",
        json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "usage": {
                    "input_tokens": usage.input_tokens,
                    "input_tokens_details": {
                        "cached_tokens": usage.cached_input_tokens,
                        "cache_write_tokens": 0,
                    },
                    "output_tokens": usage.output_tokens,
                    "output_tokens_details": null,
                    "total_tokens": usage.total(),
                },
            },
        }),
    )
}

/// `response.incomplete`, which ends the stream.
///
/// The reason is the log's own, not a translation: a client surfaces it verbatim
/// to whoever is watching, and the log's vocabulary is the accurate one.
pub(super) fn incomplete_frame(response_id: &ResponseId, reason: &IncompleteReason) -> Event {
    frame(
        "response.incomplete",
        json!({
            "type": "response.incomplete",
            "response": {
                "id": response_id,
                "incomplete_details": { "reason": reason },
            },
        }),
    )
}

/// `response.failed`, which ends the stream.
///
/// Carries an `id` only when the log named a response. A turn refused before
/// admission has none, and inventing one would advertise a response the session
/// does not contain — to a client that may well quote it back in a support
/// ticket.
pub(super) fn failed_frame(response_id: Option<&ResponseId>, message: &str) -> Event {
    let mut response = json!({ "error": { "code": "server_error", "message": message } });
    if let Some(response_id) = response_id {
        response["id"] = json!(response_id);
    }
    frame(
        "response.failed",
        json!({ "type": "response.failed", "response": response }),
    )
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Item {
        Item::user_text(text)
    }

    fn assistant(text: &str) -> Item {
        Item::assistant_text(text, ResponseId::new("resp_1"))
    }

    #[test]
    fn the_turn_id_is_the_conversation_and_nothing_else() {
        let conversation = vec![user("hello"), assistant("hi")];
        assert_eq!(turn_id_for(&conversation), turn_id_for(&conversation));
        assert_ne!(turn_id_for(&conversation), turn_id_for(&[user("hello")]));
        // Two conversations that concatenate to the same text must not collide;
        // the role prefix is what keeps them apart.
        assert_ne!(
            turn_id_for(&[user("ab")]),
            turn_id_for(&[user("a"), user("b")])
        );
    }

    #[test]
    fn reasoning_is_dropped_and_unknown_items_are_refused() {
        let items = canonicalize(
            "be brief",
            &[
                json!({ "type": "reasoning", "summary": [] }),
                json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }),
            ],
        )
        .expect("reasoning is skipped rather than refused");
        assert_eq!(items, vec![Item::system_text("be brief"), user("hi")]);

        assert!(canonicalize("", &[json!({ "type": "web_search_call" })]).is_err());
    }

    #[test]
    fn tool_items_canonicalize_to_the_call_and_its_result() {
        let items = canonicalize(
            "",
            &[
                json!({
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "grep",
                    "arguments": "{\"q\":\"x\"}",
                }),
                json!({ "type": "function_call_output", "call_id": "call_1", "output": "3 hits" }),
            ],
        )
        .expect("both tool shapes are representable");
        assert_eq!(
            items,
            vec![
                Item {
                    role: Role::Assistant,
                    content: ItemContent::ToolCall {
                        call_id: "call_1".into(),
                        name: "grep".into(),
                        arguments: "{\"q\":\"x\"}".into(),
                    },
                    response_id: None,
                },
                Item {
                    role: Role::Tool,
                    content: ItemContent::ToolResult {
                        call_id: "call_1".into(),
                        output: "3 hits".into(),
                    },
                    response_id: None,
                },
            ]
        );
    }
}
