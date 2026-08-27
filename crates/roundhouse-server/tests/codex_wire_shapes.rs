// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M0 of `PLAN-agentic-control-plane.md`: pinning the wire-shape facts the
//! whole steering design rests on, against the *real* pinned Codex parser,
//! before any roundhouse production code exists to emit a synthetic tool call
//! at all.
//!
//! These are not conformance tests for our own `/v1/responses` surface — that
//! is `codex_conformance.rs`, and it stays untouched. Nothing here binds a
//! router or an engine; there is no roundhouse server in this file. The point
//! is narrower and comes first: is the fact this plan is built on actually
//! true of the crates it cites? A fact confirmed by reading source is an
//! opinion about source; a fact confirmed by a passing test against the real
//! parser is a fact about the parser. If one of these fails, that is a finding
//! against the plan, not a test to loosen.
//!
//! # Addendum (M10.0): what these facts now underwrite
//!
//! The design these were pinned for is gone. `PLAN-frontier-selection.md` R1
//! retired the synthetic tool call as a steering channel, so **roundhouse emits
//! no `function_call` at all** — the steered turn is answered with assistant
//! text and the outbound projection that built these frames was deleted with
//! it (T4).
//!
//! The facts themselves are unchanged and are kept for the half that is still
//! live: a codex agent runs its *own* MCP tools between our turns and re-sends
//! them namespaced, so the input path still meets exactly this object, and
//! `canonical_item` still has to read a separate `namespace` field and an item
//! `id` as decoration. The fixtures below are therefore written as a client's
//! own call rather than as one of ours — which is what they were always really
//! about, since nothing here has ever involved a roundhouse server.
//!
//! Read them as parser facts, not as emission requirements. Where the prose
//! below says what roundhouse "must emit", it now says what a client does send;
//! the assertions did not move.
//!
//! Test 1 needs no transport: `serde_json` round-trips `ResponseItem` on its
//! own. Tests 2 and 3 need a stream, so they drive Codex's real
//! `codex_api::ResponsesClient` over `CannedTransport` — a fixed SSE byte body
//! standing in for a socket, the same idea as `codex_conformance.rs`'s
//! `RouterTransport` but with no router behind it, because these two facts are
//! about what the client does with bytes it is handed, not about anything
//! roundhouse computes.

use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use codex_api::{
    ApiError, Provider, ResponseEvent, ResponsesApiRequest, ResponsesClient, ResponsesOptions,
};
use codex_client::{HttpTransport, Request, Response, StreamResponse, TransportError};
use codex_protocol::models::ResponseItem;
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use futures::StreamExt;

mod common;
use common::codex::{collect, function_call_item};

// ---------------------------------------------------------------------------
// A transport that plays back a fixed SSE body
// ---------------------------------------------------------------------------

/// [`HttpTransport`] backed by a byte string instead of a socket or a router.
///
/// `codex_conformance.rs`'s `RouterTransport` proves the client against our
/// own server; this proves it against a body we hand-assemble, which is what
/// lets a test construct frame sequences our server does not (yet) emit — a
/// lone `output_item.done`, or a run of `function_call_arguments.delta` frames
/// — to pin what the client does with them.
#[derive(Clone)]
struct CannedTransport {
    body: Bytes,
}

impl HttpTransport for CannedTransport {
    async fn execute(&self, _request: Request) -> Result<Response, TransportError> {
        unimplemented!("stream_request only ever calls stream(), never execute()")
    }

    async fn stream(&self, _request: Request) -> Result<StreamResponse, TransportError> {
        let chunk = self.body.clone();
        Ok(StreamResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            // One chunk is enough: `eventsource_stream` parses frame
            // boundaries out of the bytes it is given regardless of how they
            // arrived, and no assertion here is about chunking.
            bytes: futures::stream::once(async move { Ok(chunk) }).boxed(),
        })
    }
}

/// This suite's provider: the shared retry-nothing builder — consolidated
/// into `common::codex` now that a third caller exists, which is the move M0
/// deferred to M1 rather than the duplication it left behind.
fn provider() -> Provider {
    common::codex::provider("http://canned.test/v1", "roundhouse-wire-shapes")
}

/// A minimal well-formed request. Its content is inert — `CannedTransport`
/// never inspects it — but it has to be a real `ResponsesApiRequest` for
/// `stream_request` to encode, which is what makes the trip through the
/// client's own request path rather than a bypass of it. Only the session name
/// is this suite's; the field list is the shared builder's, so a field Codex
/// adds cannot arrive in one suite and not the other.
fn request(input: Vec<ResponseItem>) -> ResponsesApiRequest {
    common::codex::request("sess-wire-shapes", input)
}

/// One SSE frame, in the `event: <name>\ndata: <json>\n\n` shape both
/// `wire.rs`'s `frame()` and the upstream fixtures use. The client's own
/// parser (`codex-client/src/sse.rs`) only ever reads the `data:` line — the
/// `event:` line is for a human tailing the stream — but writing both keeps
/// this fixture readable as the frame sequence it is meant to document.
fn sse_frame(name: &str, payload: Value) -> String {
    format!("event: {name}\ndata: {payload}\n\n")
}

fn sse_body(frames: Vec<String>) -> Bytes {
    Bytes::from(frames.concat())
}

/// Drive one request through Codex's real client, against a canned body.
///
/// The `RateLimits` filtering lives in `common::codex::collect`, for the
/// reason stated there: `spawn_response_stream` synthesizes one from response
/// headers on every call, present or not, so it reports nothing about the
/// frames this fixture wrote and would only pad every event-count assertion
/// below by one.
async fn drive(body: Bytes, request: ResponsesApiRequest) -> Result<Vec<ResponseEvent>, ApiError> {
    let client = ResponsesClient::new(
        CannedTransport { body },
        provider(),
        Arc::new(common::codex::NoAuth),
    );
    collect(
        client
            .stream_request(request, ResponsesOptions::default())
            .await?,
    )
    .await
}

fn usage_object(input_tokens: u64, output_tokens: u64) -> Value {
    json!({
        "input_tokens": input_tokens,
        "input_tokens_details": { "cached_tokens": 0 },
        "output_tokens": output_tokens,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": input_tokens + output_tokens,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Pinned fact 1: `ResponseItem::FunctionCall`'s exact wire shape, field for
/// field, including which optionals are truly absent rather than `null`.
///
/// This is the object a codex client builds with `codex_protocol`'s own type
/// and places inside its request when it re-sends an MCP call it ran. Asserting
/// equality of the whole `Value` — not just the fields a
/// hand-written check would think to look at — is what proves a skipped
/// optional (`id`, `encrypted_function_args`,
/// `internal_chat_message_metadata_passthrough`) is really missing from the
/// object, not merely absent from this test's attention. An `id: null`
/// slipping through here would still resolve *this* assertion by accident if
/// the check only compared individual fields; comparing the whole `Value`
/// does not allow that.
#[test]
fn a_namespaced_function_call_round_trips_through_codex_protocol() {
    let arguments = r#"{"cursor":null}"#.to_string();
    let item = function_call_item("grep", Some("mcp__roundhouse"), "call_theirs", &arguments);

    let value = serde_json::to_value(&item).expect("a FunctionCall item always serializes");
    assert_eq!(
        value,
        json!({
            "type": "function_call",
            "name": "grep",
            "namespace": "mcp__roundhouse",
            "arguments": arguments,
            "call_id": "call_theirs",
        }),
        "this is the exact object a codex client sends for an MCP call it ran; \
         any drift here is a drift in what Codex's own exact-HashMap dispatch \
         (router.rs:164, registry.rs:440-444) resolved it against, and in what \
         `canonical_item` therefore has to strip on the way in: {value}"
    );

    let parsed: ResponseItem =
        serde_json::from_value(value).expect("the object just asserted on must parse back");
    match parsed {
        ResponseItem::FunctionCall {
            name, namespace, ..
        } => {
            assert_eq!(name, "grep");
            assert_eq!(
                namespace.as_deref(),
                Some("mcp__roundhouse"),
                "a namespaced call carries namespace as a separate wire field; \
                 a flat `mcp__server__tool` resolves against nothing in Codex's \
                 ToolName{{name, namespace}} lookup, which is why the log stores \
                 the bare name and canonicalization ignores this field"
            );
        }
        other => panic!("expected ResponseItem::FunctionCall, got {other:?}"),
    }
}

/// Pinned fact 2: dispatch is keyed off `response.output_item.done` alone.
///
/// `handle_output_item_done` (the private `core/src/stream_events_utils.rs:288`
/// the plan cites) calls `ToolRouter::build_tool_call` on whatever item
/// arrives, with no dependency on a preceding `response.output_item.added`.
/// `codex_api` cannot prove the *dispatch* half of that claim — `codex-core`
/// is private and not a pinned dependency — but it can prove the *parse* half:
/// that a lone `done` frame, with no `added` before it, parses into
/// `ResponseItem::FunctionCall` rather than silently becoming
/// `ResponseItem::Other`, which is the shape upstream's own
/// `parses_tool_search_call_items` (sse/responses.rs:938-972) proves for a
/// different item type. If this failed, the plan's optimistic-emission design
/// (§9 M4) would be building on a parser that drops the very frame it means to
/// send.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_function_call_done_frame_parses_without_a_preceding_added() {
    ensure_rustls_crypto_provider();

    let call_item = function_call_item(
        "grep",
        Some("mcp__roundhouse"),
        "call_theirs",
        r#"{"cursor":null}"#,
    );
    let body = sse_body(vec![
        sse_frame(
            "response.created",
            json!({ "type": "response.created", "response": { "id": "resp_wire_1" } }),
        ),
        // No `response.output_item.added` precedes this — that omission is
        // the fact under test, not an oversight in the fixture.
        sse_frame(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "item": serde_json::to_value(&call_item).expect("item encodes"),
            }),
        ),
        sse_frame(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": { "id": "resp_wire_1", "usage": usage_object(10, 5) },
            }),
        ),
    ]);

    let events = drive(body, request(vec![]))
        .await
        .expect("a lone done frame with no preceding added must still parse cleanly");

    let done_items: Vec<&ResponseItem> = events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputItemDone(item) => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(
        done_items.len(),
        1,
        "exactly one output_item.done in this fixture: {events:#?}"
    );
    match done_items[0] {
        ResponseItem::FunctionCall {
            name, namespace, ..
        } => {
            assert_eq!(name, "grep");
            assert_eq!(namespace.as_deref(), Some("mcp__roundhouse"));
        }
        // The failure mode this whole test exists to catch: an item whose
        // shape the parser could not match becomes `Other` in silence, and
        // every sequence-level assertion above would still have passed.
        other => panic!(
            "the item must parse as FunctionCall, not silently become \
             ResponseItem::Other: {other:?}"
        ),
    }
}

/// Pinned fact 3: `response.function_call_arguments.delta` produces no client
/// event at all — it sits in `process_responses_event`'s unhandled arm
/// (sse/responses.rs:488-499) and is traced, never forwarded.
///
/// This was the reason roundhouse never streamed a synthetic call's arguments:
/// no currently-pinned client surface observes the deltas, so the only
/// wire-correct way to deliver a `function_call` is whole, inside its
/// `response.output_item.done`. **Roundhouse emits no such call any more**
/// (M10.0), so the fact is kept as a parser fact rather than as a constraint on
/// us — and it is the one that would have to be re-read first if a future
/// surface ever wants to send one.
///
/// The fixture also carries one `response.custom_tool_call_input.delta`
/// control frame, interspersed with the frames under test. That event name
/// *is* handled (sse/responses.rs:365-375) and is proven by upstream's own
/// `parses_tool_call_input_deltas` (sse/responses.rs:974-1002) to yield
/// `ResponseEvent::ToolCallInputDelta`. Without it, this test cannot tell "no
/// frame in this fixture reaches the parser" apart from "this specific event
/// name reaches the parser and produces nothing" — a body of unparseable
/// bytes, or the event name silently misspelled, would pass exactly as
/// green. The control frame turns that gap into a live assertion: exactly
/// one `ToolCallInputDelta` must appear, and it must be the control's, not
/// one manufactured from a `function_call_arguments.delta` frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn function_call_argument_deltas_are_not_observed_by_this_client() {
    ensure_rustls_crypto_provider();

    let call_item = function_call_item(
        "grep",
        Some("mcp__roundhouse"),
        "call_mine",
        r#"{"cursor":null}"#,
    );

    let mut frames = vec![sse_frame(
        "response.created",
        json!({ "type": "response.created", "response": { "id": "resp_wire_2" } }),
    )];
    // Control: an event name process_responses_event does handle
    // (sse/responses.rs:365-375), placed in the same body via the same
    // sse_frame/drive path as the frames under test. This is what proves the
    // fixture's bytes actually reach the parser — a mutated `"delta": 42`
    // (dropped before decoding as `ResponsesStreamEvent`, sse/responses.rs
    // :573-583) or a misspelled event name both leave every other assertion
    // in this test green, because nothing else here can distinguish "reached
    // the parser and produced nothing" from "never reached the parser".
    frames.push(sse_frame(
        "response.custom_tool_call_input.delta",
        json!({
            "type": "response.custom_tool_call_input.delta",
            "item_id": "ctc_call_mine",
            "call_id": "call_mine",
            "delta": "control-frame-reaches-the-parser",
        }),
    ));
    for (index, chunk) in ["{\"cursor\"", ":null", "}"].into_iter().enumerate() {
        frames.push(sse_frame(
            "response.function_call_arguments.delta",
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": format!("fc_{index}"),
                "call_id": "call_mine",
                "delta": chunk,
            }),
        ));
    }
    frames.push(sse_frame(
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "item": serde_json::to_value(&call_item).expect("item encodes"),
        }),
    ));
    frames.push(sse_frame(
        "response.completed",
        json!({
            "type": "response.completed",
            "response": { "id": "resp_wire_2", "usage": usage_object(10, 5) },
        }),
    ));

    let events = drive(sse_body(frames), request(vec![]))
        .await
        .expect("the stream must still end cleanly despite the delta frames");

    let tool_call_input_deltas: Vec<&ResponseEvent> = events
        .iter()
        .filter(|event| matches!(event, ResponseEvent::ToolCallInputDelta { .. }))
        .collect();
    assert_eq!(
        tool_call_input_deltas.len(),
        1,
        "exactly the control frame's ToolCallInputDelta must appear — this is \
         what proves the fixture's frames reach the parser at all, and that \
         response.function_call_arguments.delta contributes none of its own: \
         {events:#?}"
    );
    match tool_call_input_deltas[0] {
        ResponseEvent::ToolCallInputDelta {
            item_id,
            call_id,
            delta,
        } => {
            assert_eq!(item_id, "ctc_call_mine");
            assert_eq!(call_id.as_deref(), Some("call_mine"));
            assert_eq!(delta, "control-frame-reaches-the-parser");
        }
        other => unreachable!("filtered to ToolCallInputDelta above: {other:?}"),
    }
    assert_eq!(
        events.len(),
        4,
        "created, the control's ToolCallInputDelta, the terminal done, and \
         completed — nothing for the three response.function_call_arguments.delta \
         frames in between: {events:#?}"
    );
    assert!(matches!(events[0], ResponseEvent::Created));
    assert!(matches!(
        events[1],
        ResponseEvent::ToolCallInputDelta { .. }
    ));
    assert!(matches!(events[2], ResponseEvent::OutputItemDone(_)));
    assert!(matches!(events[3], ResponseEvent::Completed { .. }));
}
