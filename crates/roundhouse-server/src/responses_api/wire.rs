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

use crate::dialect::ClientDialect;
use crate::http::ApiError;

/// The id of the one message item a response produces.
///
/// One assistant message per turn, so a fixed id is enough. The prefix is not
/// decoration: a client discards an item id that has none, and an item it cannot
/// name is an item it cannot attach deltas to.
const MESSAGE_ITEM_ID: &str = "msg_1";

/// The id space an emitted function-call item is named in.
///
/// Separate from [`MESSAGE_ITEM_ID`] and not merely different: a client indexes
/// items by id and attaches deltas by it, so a call sharing the message's id
/// would be the message as far as the client is concerned. One response emits
/// at most one call — a steered turn commits its item and its completion in one
/// batch and dispatches nothing — so the response's own id makes the item's id
/// unique without a counter to keep.
const FUNCTION_CALL_ITEM_PREFIX: &str = "fc_";

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

// ---------------------------------------------------------------------------
// A tool call this deployment emitted
// ---------------------------------------------------------------------------

/// One synthetic call, and everything needed to spell it on this wire.
///
/// A named struct rather than five positional arguments repeated across two
/// builders: the two frames must carry *the same* item — a client that was
/// announced one call and handed another has no way to reconcile them — and
/// building both from one value is how that stops being a thing to remember.
///
/// The three content fields are borrowed straight from the stored
/// [`ItemContent::ToolCall`], never rebuilt: `arguments` in particular is
/// minted once at emission and echoed here verbatim, which is what makes the
/// client's own verbatim resend of it match the stored item by construction. A
/// re-serialization here — even a semantically identical one, with keys in a
/// different order — would canonicalize to a different item next turn and fork
/// the session.
pub(super) struct EmittedCall<'a> {
    /// How this deployment's clients spell a tool call. See [`ClientDialect`].
    pub dialect: &'a ClientDialect,
    /// The response that emitted the call, which names the wire item.
    pub response_id: &'a ResponseId,
    pub call_id: &'a str,
    /// The bare tool name as the log holds it, with no namespace folded in.
    pub name: &'a str,
    pub arguments: &'a str,
}

impl EmittedCall<'_> {
    /// The item both frames carry.
    ///
    /// The match is the dialect's, and it is a match rather than a namespace
    /// interpolated into a fixed shape because a second dialect changes the
    /// *shape* — a flat `mcp__roundhouse__fetch_steer` with no `namespace`
    /// field at all — and not just the string.
    fn item(&self) -> Value {
        match self.dialect {
            ClientDialect::CodexResponses { namespace } => json!({
                "type": "function_call",
                "id": format!("{FUNCTION_CALL_ITEM_PREFIX}{}", self.response_id),
                // A separate field, never folded into `name`: Codex dispatches
                // on an exact `ToolName { name, namespace }` lookup and nothing
                // in its tree splits a flat name back apart, so a folded name
                // resolves against nothing and comes back to the model as
                // `unsupported call: …`.
                "namespace": namespace,
                "name": self.name,
                "call_id": self.call_id,
                "arguments": self.arguments,
            }),
        }
    }
}

/// `response.output_item.added`, announcing an emitted call.
///
/// Carries the complete item rather than an empty shell, unlike the message
/// path's [`item_added_frame`]: a message is announced empty because its text
/// arrives as deltas afterwards, and a call's arguments never do — argument
/// deltas are traced and dropped by the pinned client, so anything not in this
/// frame and its `done` twin is not on the wire at all.
pub(super) fn tool_call_added_frame(call: &EmittedCall<'_>) -> Event {
    frame(
        "response.output_item.added",
        json!({ "type": "response.output_item.added", "item": call.item() }),
    )
}

/// `response.output_item.done`, the frame the client actually dispatches off.
///
/// The pinned Codex client builds its tool call from whatever item arrives
/// here, with no dependency on the `added` frame before it — which is why this
/// one carries the whole item and why `added` is a courtesy rather than a
/// prerequisite.
pub(super) fn tool_call_done_frame(call: &EmittedCall<'_>) -> Event {
    frame(
        "response.output_item.done",
        json!({ "type": "response.output_item.done", "item": call.item() }),
    )
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
///
/// `reasoning_tokens` rides in `output_tokens_details` for the same reason it
/// is stored that way: it is a component of `output_tokens`, not an addition
/// to it, so a client that checks the details against the total still balances.
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
                    "output_tokens_details": {
                        "reasoning_tokens": usage.reasoning_tokens,
                    },
                    "total_tokens": usage.total(),
                },
            },
        }),
    )
}

/// `response.incomplete`, which ends the stream.
///
/// The reason is the log's own: a client surfaces it verbatim to whoever is
/// watching, and the log's vocabulary is the accurate one. The single reason
/// that never reaches this function is
/// [`IncompleteReason::PolicyRefused`] — a refusal is not a truncated answer,
/// and its caller renders it as `response.failed` instead. That is the only
/// translation on this surface, and it is spelled out at the call site.
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

    /// The canonical bytes of an emitted call, pinned whole.
    ///
    /// Whole rather than field by field, for the reason
    /// `codex_wire_shapes.rs` gives about the same object: a field-by-field
    /// check cannot see a field that should not be there, and an extra key —
    /// or a `namespace` that quietly became part of `name` — is exactly the
    /// drift that leaves Codex's exact `HashMap` lookup with nothing to match.
    /// The failure would be silent on our side and arrive as `unsupported
    /// call: …` on the model's.
    ///
    /// Pinned on the item rather than on the two frames because
    /// [`axum::response::sse::Event`] exposes no reader — the frames
    /// themselves are asserted byte for byte in `steering_emission.rs`, which
    /// reads a real response body. Both builders render through
    /// [`EmittedCall::item`], so this is what they carry.
    #[test]
    fn an_emitted_call_renders_the_golden_item_under_the_codex_dialect() {
        let response_id = ResponseId::new("resp_01J");
        let dialect = ClientDialect::CodexResponses {
            namespace: "mcp__roundhouse".to_string(),
        };
        let call = EmittedCall {
            dialect: &dialect,
            response_id: &response_id,
            call_id: "rhsteer_resp_01J",
            name: "fetch_steer",
            arguments: r#"{"steer_id":"rhsteer_resp_01J"}"#,
        };

        assert_eq!(
            call.item(),
            json!({
                "type": "function_call",
                "id": "fc_resp_01J",
                "namespace": "mcp__roundhouse",
                "name": "fetch_steer",
                "call_id": "rhsteer_resp_01J",
                "arguments": r#"{"steer_id":"rhsteer_resp_01J"}"#,
            })
        );
    }

    /// The call's item id is its own, and never the message's.
    ///
    /// A control for the pin above: with one dialect and one hard-coded
    /// namespace, the golden test would pass just as well if the id were
    /// `msg_1`, and a client that indexes items by id would then have the call
    /// and the assistant message as one item.
    #[test]
    fn an_emitted_calls_item_id_is_not_the_message_item_id() {
        let response_id = ResponseId::new("resp_02K");
        let dialect = ClientDialect::default();
        let call = EmittedCall {
            dialect: &dialect,
            response_id: &response_id,
            call_id: "rhsteer_resp_02K",
            name: "fetch_steer",
            arguments: "{}",
        };
        let id = call.item()["id"]
            .as_str()
            .expect("the item is named")
            .to_string();
        assert_ne!(id, MESSAGE_ITEM_ID);
        assert!(id.starts_with(FUNCTION_CALL_ITEM_PREFIX));
        assert!(
            id.contains(response_id.as_str()),
            "the id is minted from the response, so two responses cannot \
             collide on it: {id}"
        );
    }

    /// An emitted call and the client's resend of it are the same item.
    ///
    /// The single fact the whole steering choreography rests on, asserted at
    /// the seam where it could break: the projection puts a `namespace` and an
    /// `id` on the wire, and canonicalization must read neither. If it did,
    /// the claimed prefix would disagree with the stored one on the very next
    /// turn and the session would rebind to a cold generation — silently, since
    /// every turn would still answer.
    #[test]
    fn the_clients_resend_of_an_emitted_call_canonicalizes_back_to_it() {
        let response_id = ResponseId::new("resp_03L");
        let dialect = ClientDialect::default();
        let call = EmittedCall {
            dialect: &dialect,
            response_id: &response_id,
            call_id: "rhsteer_resp_03L",
            name: "fetch_steer",
            arguments: r#"{"steer_id":"rhsteer_resp_03L"}"#,
        };

        let resent = canonicalize("", &[call.item()]).expect("the emitted item is resendable");
        assert_eq!(
            resent,
            vec![Item::tool_call(
                "rhsteer_resp_03L",
                "fetch_steer",
                r#"{"steer_id":"rhsteer_resp_03L"}"#,
            )],
            "the wire's namespace and item id must leave no trace in the \
             canonical item: {resent:#?}"
        );
    }

    /// The turn id of a fixed pre-M4-shaped conversation, pinned as a literal.
    ///
    /// The idempotency story rests on this hash being a pure function of the
    /// conversation, stable across processes, machines, and releases: a client
    /// retry hashes to the same turn and replays instead of paying twice. An
    /// unchanged-code argument held that property through M4; a literal holds
    /// it through every future change, because any edit to `Item::render`, the
    /// FNV constants, or canonicalization that moves historical hashes fails
    /// here first — and such an edit orphans every in-flight retry, so it must
    /// be a decision, not a side effect.
    #[test]
    fn the_turn_id_of_a_fixed_conversation_is_pinned() {
        let claimed = canonicalize(
            "be brief",
            &[
                serde_json::json!({"type": "message", "role": "user", "content": "hello"}),
                serde_json::json!({"type": "function_call", "call_id": "call_1",
                                    "name": "search", "arguments": "{\"q\":\"rust\"}"}),
                serde_json::json!({"type": "function_call_output", "call_id": "call_1",
                                    "output": "3 hits"}),
            ],
        )
        .expect("a fixed, well-formed conversation canonicalizes");
        assert_eq!(turn_id_for(&claimed).to_string(), "turn_6a7aaa94e5b59fd2");
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
