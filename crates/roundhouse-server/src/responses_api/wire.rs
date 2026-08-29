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

// ---------------------------------------------------------------------------
// A tool call this deployment emitted
// ---------------------------------------------------------------------------

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
mod tests {
    use super::*;

    fn user(text: &str) -> Item {
        Item::user_text(text)
    }

    fn assistant(text: &str) -> Item {
        Item::assistant_text(text, ResponseId::new("resp_1"))
    }

    /// **A measured cache write reaches the wire; nothing else invents one.**
    ///
    /// This field was the literal `0` for the whole of M10, with a doc saying no
    /// provider reported one — and it went out as zero on every turn including
    /// the ones that would have reported it, had a client existed. M11.0 added
    /// the client, so the literal is now a read. Two halves, and neither is the
    /// claim alone: an Anthropic turn's write count must arrive, and a Responses
    /// turn's must stay zero rather than being back-filled from the uncached
    /// count that roundhouse *prices* at the write rate.
    #[test]
    fn a_measured_cache_write_reaches_the_wire_and_an_unmeasured_one_stays_zero() {
        // PROBE: the shape an `anthropic_messages` turn folds to — the three
        // input counters already summed into `input_tokens` by the client, with
        // the write kept as its own component.
        let anthropic = Usage {
            input_tokens: 9_512,
            cached_input_tokens: 9_000,
            cache_write_tokens: 500,
            output_tokens: 64,
            reasoning_tokens: 0,
            accounting: Default::default(),
        };
        let usage = completed_usage(&anthropic);
        assert_eq!(
            usage["input_tokens_details"]["cache_write_tokens"],
            json!(500)
        );
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], json!(9_000));
        assert_eq!(
            usage["input_tokens"],
            json!(9_512),
            "the two details are components of the input total, not addends — a client that \
             checks the parts against the whole still balances"
        );
        assert_eq!(usage["total_tokens"], json!(9_512 + 64));

        // CONTROL: the same prompt over the Responses wire, where 512 tokens
        // were uncached and nothing reported a write. Zero is the honest answer
        // and `uncached_input_tokens()` — 512 — is the number a well-meaning
        // back-derivation would put here, which is why the assertion names it.
        let responses = Usage {
            cache_write_tokens: 0,
            ..anthropic.clone()
        };
        assert_eq!(responses.uncached_input_tokens(), 512);
        assert_eq!(
            completed_usage(&responses)["input_tokens_details"]["cache_write_tokens"],
            json!(0),
            "a pricing convention must never be published in a field named for a measurement"
        );
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

    /// A client's MCP tool call and the item the log keeps are the same call.
    ///
    /// **Inbound machinery, and M10.0 left it exactly where it was (T4).** The
    /// outbound half is gone — no response projects a `function_call` frame any
    /// more, so `EmittedCall` and its two builders were deleted with the steer
    /// they existed for — but the *input* path still meets namespaced calls on
    /// every turn, because a codex agent runs its own MCP tools between ours and
    /// re-sends them in the history. Canonicalization must read neither the
    /// `namespace` nor the item `id`, or the claimed prefix disagrees with the
    /// stored one on the very next turn and the session rebinds to a cold
    /// generation — silently, since every turn would still answer.
    ///
    /// The wire item is written out here rather than built by a projection,
    /// which is what the deleted version did. That is a real loss and it is
    /// named: the old test could not drift from what we emitted, because it
    /// asked the emitter. This one is a fixture of what *codex* emits, so it is
    /// pinned against a client instead — `codex_wire_shapes.rs` builds the same
    /// shape from codex's own types, and that is where the fixture is kept
    /// honest.
    #[test]
    fn a_clients_namespaced_call_canonicalizes_to_the_bare_stored_item() {
        let wire = json!({
            "type": "function_call",
            "id": "fc_resp_03L",
            "namespace": "mcp__roundhouse",
            "name": "status",
            "call_id": "call_03L",
            "arguments": r#"{"conversation":"main"}"#,
        });

        let stored = canonicalize("", &[wire]).expect("a client's own call is resendable");
        assert_eq!(
            stored,
            vec![Item::tool_call(
                "call_03L",
                "status",
                r#"{"conversation":"main"}"#,
            )],
            "the wire's namespace and item id must leave no trace in the \
             canonical item: {stored:#?}"
        );
    }

    /// A namespace folded into `name` is part of the name, and canonicalization
    /// does not split it back apart (F10).
    ///
    /// The corrected half of `dialect.rs`'s "why that direction" argument. That
    /// module's earlier draft justified keeping the namespace out of the log by
    /// claiming a namespaced resend and a flat resend already arrive as one
    /// canonical item, because `canonical_item` ignores `namespace` and `id` on
    /// the way in. It ignores a *separate* `namespace` field — which is what
    /// makes `CodexResponses`'s own resend round-trip, asserted directly above
    /// — and nothing more. No dialect emits the flat spelling today, so nothing
    /// is broken; what was wrong was the reason, and a reason that does not hold
    /// is what gets a future change waved through.
    ///
    /// Pinned as the divergence rather than deleted with the prose, because the
    /// day a flat variant lands this is the assertion that has to be revisited
    /// deliberately: making it pass means teaching `canonical_item` to split a
    /// flat name apart, and that is a wire-layer change no dialect variant makes
    /// the compiler ask for.
    #[test]
    fn a_flat_spelling_is_a_different_canonical_call_until_the_wire_learns_to_split_it() {
        let namespaced = json!({
            "type": "function_call",
            "call_id": "call_1",
            "namespace": "mcp__roundhouse",
            "name": "fetch_steer",
            "arguments": "{}",
        });
        let flat = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "mcp__roundhouse__fetch_steer",
            "arguments": "{}",
        });

        let namespaced_item = canonicalize("", &[namespaced]).expect("namespaced form parses");
        let flat_item = canonicalize("", &[flat]).expect("flat form parses");

        assert_eq!(
            namespaced_item,
            vec![Item::tool_call("call_1", "fetch_steer", "{}")],
            "a separate `namespace` field leaves no trace: this is the property \
             the steering round trip rests on"
        );
        assert_eq!(
            flat_item,
            vec![Item::tool_call(
                "call_1",
                "mcp__roundhouse__fetch_steer",
                "{}"
            )],
            "a namespace folded into `name` is kept verbatim, so the two \
             spellings of one call are two canonical items"
        );
        assert_ne!(
            namespaced_item, flat_item,
            "if these ever agree, `canonical_item` has learned to split a flat \
             name — which is the change a flat `ClientDialect` variant owes the \
             input path, and `dialect.rs`'s module doc is the paragraph that has \
             to be re-read when it lands"
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

    /// F18 (review): codex's `ResponseItem` enum has resendable variants far
    /// beyond `message`/`function_call`/`function_call_output`/`reasoning` —
    /// `tool_search_call`, `local_shell_call`, the three compaction shapes,
    /// and more. None of them can appear in a v1 turn (no tool loop means no
    /// tool_search/shell/compaction), so today's suite only ever resends the
    /// four shapes above and never proves what `canonical_item` does with the
    /// rest. This pins that boundary as documented, enumerated behavior: each
    /// of these types must 422 with an error that *names the type*, so a
    /// future tool-loop milestone that starts emitting one finds a named
    /// failure instead of a silent behavior change.
    #[test]
    fn the_item_types_a_real_client_can_resend_are_named() {
        let refused_types = [
            "agent_message",
            "local_shell_call",
            "tool_search_call",
            "tool_search_output",
            "custom_tool_call",
            "custom_tool_call_output",
            "web_search_call",
            "image_generation_call",
            "compaction",
            "compaction_trigger",
            "context_compaction",
        ];
        for kind in refused_types {
            let err = canonicalize("", &[json!({ "type": kind })])
                .expect_err(&format!("`{kind}` must be refused, not silently dropped"));
            let message = format!("{err:?}");
            assert!(
                message.contains(kind),
                "error for `{kind}` does not name the type it refused: {message}"
            );
        }
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
