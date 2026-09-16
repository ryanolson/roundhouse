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

/// The id of the `index`-th message item of a response, counting from zero.
///
/// **Per item rather than per response since M11.2**, where a fixed `msg_1` used
/// to be enough: a turn that speaks, calls a tool and speaks again produces two
/// message items, and two items sharing an id are two items a client cannot tell
/// apart — it attaches deltas by this string.
///
/// The `msg_` prefix is not decoration: a client discards an item id that has
/// none, and an item it cannot name is an item it cannot attach deltas to.
/// Numbered from one on the wire so the first item keeps the `msg_1` every
/// fixture in this repo already names.
pub(super) fn message_item_id(index: usize) -> String {
    format!("msg_{}", index + 1)
}

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
        // **The `namespace` is carried into the item since M17 (R-N6), and the
        // item `id` is still dropped.** The two look alike on the wire and are
        // not alike at all: `id` names the *item* within one response and means
        // nothing on a resend, while `namespace` names the MCP server the call
        // was dispatched to and is half of what codex resolves a call against
        // (`ToolName { name, namespace }`). Dropping it made a third party's
        // tool named `status` indistinguishable from ours in the log, and made
        // the outbound projection re-emit a call the client could not route.
        //
        // Carrying it does not move any turn id: `Item::render` leaves the
        // field out, deliberately and with the reasoning stated there.
        "function_call" => Ok(Some(Item::namespaced_tool_call(
            required_str(value, "call_id")?,
            required_str(value, "name")?,
            optional_str(value, "namespace"),
            required_str(value, "arguments")?,
        ))),
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

/// A string field that may be absent, as `None` when it is.
///
/// Separate from [`required_str`] rather than folded into it with a flag,
/// because the two answer different questions about a malformed request: a
/// missing `call_id` is a client bug worth a 422, while a missing `namespace`
/// is the ordinary shape of a plain (non-MCP) function tool. A non-string value
/// under the key reads as absent rather than as a refusal for the same reason
/// the item `id` is ignored — this field is decoration to everything below the
/// wire except the projection that puts it back, and refusing a turn over it
/// would fail a conversation that is otherwise entirely well formed.
fn optional_str(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_string)
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
pub(crate) fn turn_id_for(items: &[Item]) -> TurnId {
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

pub(super) fn item_added_frame(id: &str) -> Event {
    frame(
        "response.output_item.added",
        json!({ "type": "response.output_item.added", "item": message_item(id, "") }),
    )
}

pub(super) fn delta_frame(id: &str, text: &str) -> Event {
    frame(
        "response.output_text.delta",
        json!({
            "type": "response.output_text.delta",
            "item_id": id,
            "delta": text,
        }),
    )
}

pub(super) fn item_done_frame(id: &str, text: &str) -> Event {
    frame(
        "response.output_item.done",
        json!({ "type": "response.output_item.done", "item": message_item(id, text) }),
    )
}

/// The assistant message, in the shape the Responses API defines for it.
///
/// The content entry is a typed `output_text` part rather than a bare string
/// because of how a client handles the difference: an item whose type it knows
/// but whose shape it cannot parse is dropped in silence, so the turn arrives
/// looking empty rather than looking wrong.
fn message_item(id: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "id": id,
        "content": [{ "type": "output_text", "text": text }],
    })
}

// ---------------------------------------------------------------------------
// A tool call this deployment emitted
// ---------------------------------------------------------------------------

/// The call, in the shape the pinned codex parser deserializes.
///
/// **Read from the oracle, not from the docs**: `ResponseItem::FunctionCall` at
/// `6344a65` (`protocol/src/models.rs`) is `{type, name, arguments, call_id}`
/// with an optional `id`, and `arguments` is a *string* holding JSON — its own
/// comment says the Responses API returns it that way and that the client parses
/// it later. That is also why [`ItemContent::ToolCall`] stores a string: the
/// value crosses this boundary in both directions without a re-encoding, and a
/// re-encoding would reorder an object's keys and stop matching what the client
/// resends.
///
/// `id` is set to the call id rather than omitted, because a streaming consumer
/// pairs `output_item.added` with its `done` on the item id, and two calls in
/// one turn that shared one id would be indistinguishable.
///
/// **`namespace` is emitted when the stored call carries one and omitted
/// otherwise** (M17, R-N10), which is not a guess about what the client wants
/// but the field it sent coming back. Codex dispatches a call by an exact
/// `ToolName { name, namespace }` registry lookup, so a namespaced call
/// re-emitted flat — or bare — resolves against nothing there and the tool
/// simply never runs. Omitted rather than sent as `null` because that is what
/// the oracle's own encoder does (`skip_serializing_if = "Option::is_none"` on
/// `ResponseItem::FunctionCall::namespace` @ the pin), and
/// `codex_wire_shapes.rs` asserts this object against that encoder field for
/// field rather than against a shape this module typed.
///
/// Public, and that is what makes the pin possible: the oracle suite is an
/// integration test and cannot see a private helper, so the alternative was to
/// assert the *frames* — and an axum [`Event`] is write-only, which is how this
/// projection went unpinned through the whole of its life.
pub fn function_call_item(
    call_id: &str,
    name: &str,
    namespace: Option<&str>,
    arguments: &str,
) -> Value {
    let mut item = json!({
        "type": "function_call",
        "id": call_id,
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    });
    if let Some(namespace) = namespace {
        item["namespace"] = json!(namespace);
    }
    item
}

/// The call announced, with its arguments still empty.
///
/// Empty deliberately, mirroring the upstream wire: the arguments stream in
/// afterwards, and a consumer that acted on this frame's `arguments` would act
/// on an empty object. The pinned parser turns this into `OutputItemAdded` and
/// waits for the `done`.
pub(super) fn call_added_frame(call_id: &str, name: &str, namespace: Option<&str>) -> Event {
    frame(
        "response.output_item.added",
        json!({
            "type": "response.output_item.added",
            "item": function_call_item(call_id, name, namespace, ""),
        }),
    )
}

/// The whole of the call's arguments, as the one fragment this call has.
///
/// **The pinned codex parser ignores this event** — it sits in
/// `process_responses_event`'s explicitly-unhandled arm at `6344a65` — so it is
/// emitted for the other consumers of this dialect rather than for the oracle,
/// and nothing downstream may depend on it. One fragment rather than several
/// because the log holds the call whole: the dispatch decoder already
/// reassembled it, and re-splitting it here would invent boundaries no upstream
/// chose.
pub(super) fn call_arguments_delta_frame(call_id: &str, arguments: &str) -> Event {
    frame(
        "response.function_call_arguments.delta",
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": call_id,
            "call_id": call_id,
            "delta": arguments,
        }),
    )
}

/// The finished call. **This is the frame that makes it real.**
///
/// The pinned parser reads `ResponseItem` off `output_item.done` and only there;
/// an item it cannot parse is dropped with a `debug!` and no error, so a turn
/// whose call was malformed arrives looking like a turn that called nothing.
pub(super) fn call_done_frame(
    call_id: &str,
    name: &str,
    namespace: Option<&str>,
    arguments: &str,
) -> Event {
    frame(
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "item": function_call_item(call_id, name, namespace, arguments),
        }),
    )
}

/// `response.completed`, which ends the stream.
///
/// `id` and `total_tokens` are load-bearing: a client parses this event into its
/// own accounting, and a completion it cannot parse is a turn it treats as
/// failed. `cached_input_tokens` goes out as `input_tokens_details.cached_tokens`
/// — the quantity this whole system exists to maximize, in the field a Responses
/// client already reads.
///
/// `cache_write_tokens` is the log's own measurement now rather than the literal
/// `0` this frame carried through M10. It is zero on every turn served over the
/// Responses wire, because that dialect does not report a cache write at all —
/// which is the honest reading, not a placeholder — and non-zero exactly when
/// the turn was dispatched to an `anthropic_messages` provider that reported
/// `cache_creation_input_tokens`. The field is read straight off [`Usage`] and
/// never back-derived from `uncached_input_tokens()`: roundhouse *prices* every
/// uncached token at the cache-write rate as a conservative approximation, and
/// publishing that convention in a field named for a measurement is exactly the
/// confusion the widened `Usage` exists to end.
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
                "usage": completed_usage(usage),
            },
        }),
    )
}

/// The `usage` object of [`completed_frame`], as a value.
///
/// Split out because an [`Event`] is write-only — axum exposes no way to read a
/// frame's payload back — so the projection from log axes to wire axes was only
/// assertable through a whole turn over a socket, which is a test about six
/// other things. Extracting it makes the one claim that matters here (each
/// stored count lands in the field a Responses client reads, and none is
/// invented) a unit test beside the code.
fn completed_usage(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "input_tokens_details": {
            "cached_tokens": usage.cached_input_tokens,
            "cache_write_tokens": usage.cache_write_tokens,
        },
        "output_tokens": usage.output_tokens,
        "output_tokens_details": {
            "reasoning_tokens": usage.reasoning_tokens,
        },
        "total_tokens": usage.total(),
    })
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
mod tests;
