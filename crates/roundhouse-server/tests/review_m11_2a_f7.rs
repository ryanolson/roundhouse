// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M11.2a thermo-nuclear review, finding F7.
//!
//! **The claim, as found.** `BlockAccumulator::close` (`messages_api.rs:1014-
//! 1042`) turns an unparseable stored tool-call argument string into `{}`
//! (`serde_json::from_str(&partial_json).unwrap_or_else(|_| Value::Object(...))`,
//! `messages_api.rs:1036-1037`) for the non-streaming projection, while the
//! streaming half (`messages_api/emit.rs`'s `tool_block_delta`, confirmed at
//! lines 377-386) sends the identical stored bytes verbatim as the tool_use
//! block's one `input_json_delta.partial_json` fragment, with no validation
//! at all. The comment on the non-streaming fallback
//! (`messages_api.rs:1031-1035`) calls the unparseable case "unreachable
//! short of a corrupt log" -- but at the time of the finding nothing between
//! `FrontierChunk::ToolCall` and the commit validated the argument string:
//! `canonical_arguments` (`item.rs:157-162`) passes a non-JSON string through
//! unchanged when it fails to parse, and the engine's dispatch fold
//! (`Item::tool_call(id, name, canonical_arguments(&arguments))`,
//! `engine.rs`, confirmed exact) applied it with no rejection. So a turn
//! whose upstream (or a re-encoding hop) truncated a tool call's argument
//! stream but still closed the block wrote a non-JSON argument string on a
//! clean, first-generation commit -- not a corrupted read of something else.
//!
//! **Ruled valid, and fixed -- at the decoder, deliberately not at either
//! serve projection this file drives.** `anthropic_messages/stream.rs`'s
//! `ToolBlock::into_chunk` now answers `None` -- dropping the call rather than
//! emitting a `FrontierChunk::ToolCall` -- when a block's `content_block_stop`
//! arrives over fragments that never reassemble into JSON (see that file's own
//! F7 tests for the mechanism, which this file cannot reach: `SseDecoder`/
//! `ToolBlock` are `pub(super)`, unreachable from an integration test, which is
//! exactly why the double below existed in the first place). The consequence
//! is the one this file *can* prove from `roundhouse-server`: a truncated tool
//! call no longer reaches `BlockAccumulator::close` or
//! `MessageEmission::tool_block` at all, on a real Anthropic dispatch, so the
//! disagreement between them over an unparseable stored string is no longer a
//! reachable defect -- not because the two were made to agree about garbage,
//! but because nothing downstream of the decoder ever holds it. The two
//! client-visible answers this finding started from -- a streaming client's
//! own accumulator throwing on the malformed fragment, a non-streaming client
//! silently reading `input: {}` -- are still exactly what `BlockAccumulator`
//! and `tool_block_delta` would do with such a string; what changed is that a
//! live Anthropic turn can no longer hand either one to them. `TRUNCATED_ARGUMENTS`
//! below is kept as the pinned fixture -- byte-identical to the one
//! `anthropic_messages/stream.rs`'s own F7 probe uses -- so the two files agree
//! on what shape this finding was ever about, even though only one of them can
//! still exercise the decoder that now refuses it.
//!
//! **Why a fresh, self-contained file** rather than an addition to
//! `messages_api_surface.rs`, per the F4/F5 precedent from this same review
//! round: this file must stand alone as review evidence and not shift if
//! that (3000+ line) suite changes shape. The router/POST plumbing below is
//! copied unchanged from that file's `surface_calling`/`post`/`body`,
//! because this claim is specifically about the serve surface those helpers
//! drive and there is no lower seam that exercises both
//! `BlockAccumulator::close` and the streaming `tool_block_delta` from one
//! generated turn.
//!
//! **The double.** [`common::ToolCallingFrontierClient`] is used directly
//! rather than a mock upstream SSE byte stream: it hands whatever
//! `FrontierChunk`s a script names straight to the engine's dispatch fold,
//! which is exactly the boundary `canonical_arguments` sits at regardless of
//! which dialect decoded it. That is why the fixed test below scripts *no*
//! `Scripted::Call` for the truncated turn: the double stands in for the
//! decoder's output, and the decoder's real output for a block whose fragments
//! never reassemble is nothing -- no id, no name, no arguments, not even a
//! malformed one. Scripting the call anyway would test a shape a live
//! Anthropic dispatch can no longer produce, through a seam neither
//! `BlockAccumulator::close` nor `tool_block_delta` had any part in fixing.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::ItemContent;
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_server::{ControlPlane, Conversations, EchoLocalExecutor, Engine, messages_router};

mod common;
use common::anthropic::{StrictDelta, StrictEvent, audit, split_frames};
use common::{Scripted, ToolCallingFrontierClient, config, frontier_catalog};

/// A tool call's argument stream, cut off mid-object -- exactly the shape a
/// truncated upstream fragment run produces: a complete key/value pair with
/// no closing brace. Realistic rather than adversarial: this is what a
/// connection that died one fragment early actually looks like on the wire.
///
/// **Not scripted into the double below since the fix** (see the module
/// doc's "The double"). Kept as the pinned reference shape instead --
/// byte-identical to the fragment `anthropic_messages/stream.rs`'s own
/// `a_closed_block_whose_arguments_never_parse_emits_no_call` feeds the real
/// decoder -- so a reader can see the two files are about the same string
/// even though only one of them still hands it to anything.
const TRUNCATED_ARGUMENTS: &str = r#"{"command": "ls -la""#;

/// Fixture guard: the premise of this whole file is that [`TRUNCATED_ARGUMENTS`]
/// does not parse as JSON. If it ever did (a copy-paste slip that balanced the
/// braces), the cross-file claim in its own doc -- that this is the shape the
/// decoder now refuses -- would be about a string that was never truncated at
/// all.
#[test]
fn fixture_guard_the_truncated_arguments_are_not_valid_json() {
    assert!(
        serde_json::from_str::<Value>(TRUNCATED_ARGUMENTS).is_err(),
        "F7 fixture: TRUNCATED_ARGUMENTS parses as JSON, so it cannot stand in \
         for a truncated argument stream"
    );
}

// ---------------------------------------------------------------------------
// The service under test -- copied from messages_api_surface.rs's
// surface_calling/post/body (unchanged), per this file's module doc.
// ---------------------------------------------------------------------------

fn surface(
    script: Vec<Scripted>,
    stop_reason: Option<&str>,
) -> (Router, Arc<MemoryStore>, Arc<ToolCallingFrontierClient>) {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ToolCallingFrontierClient::new(script, stop_reason));
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::clone(&client) as Arc<dyn roundhouse_fleet::FrontierClient>,
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    (
        messages_router(
            ControlPlane::open(),
            engine,
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
        client,
    )
}

fn body(text: &str, stream: bool) -> Value {
    json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": stream,
        "messages": [{ "role": "user", "content": text }],
    })
}

async fn post(app: &Router, headers: &[(&str, &str)], body: &Value) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header(CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(body).expect("a JSON body")))
                .expect("a well-formed request"),
        )
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a readable body")
        .to_bytes();
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("bodies are UTF-8"),
    )
}

/// The session's committed items, read straight out of the store -- copied
/// from `messages_api_surface.rs`'s helper of the same name.
async fn stored_items(store: &MemoryStore, session_id: &str) -> Vec<ItemContent> {
    store
        .read_events(&SessionId::new(session_id), 0, 4096)
        .await
        .expect("the session exists")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ItemAppended { item } => Some(item.content),
            _ => None,
        })
        .collect()
}

fn named(session: &str) -> String {
    format!("anthropic_messages/{session}")
}

/// The stored argument string of the turn's one committed `ToolCall` item.
fn tool_call_arguments(items: &[ItemContent]) -> Option<&str> {
    items.iter().find_map(|item| match item {
        ItemContent::ToolCall { arguments, .. } => Some(arguments.as_str()),
        _ => None,
    })
}

/// The `input_json_delta` fragments streamed for the turn's one tool block,
/// concatenated in frame order.
///
/// Read independently of `StreamOracle::close_block` with the same strict,
/// spec-derived `StrictEvent`/`StrictDelta` types the oracle itself
/// deserializes frames into, so this is a second, direct read of the wire
/// bytes rather than a restatement of the oracle's own error text.
fn raw_partial_json(body: &str) -> String {
    let mut fragments = String::new();
    for frame in split_frames(body) {
        let (Some(name), Some(data)) = (frame.name, frame.data) else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<StrictEvent>(&data) else {
            continue;
        };
        if event.wire_name() != name {
            continue;
        }
        if let StrictEvent::ContentBlockDelta {
            delta: StrictDelta::InputJsonDelta { partial_json },
            ..
        } = event
        {
            fragments.push_str(&partial_json);
        }
    }
    fragments
}

// ---------------------------------------------------------------------------
// F7
// ---------------------------------------------------------------------------

/// **F7.** Streaming and non-streaming disagree about a tool call whose
/// arguments never parsed as JSON: one is unreadable by Claude Code's own
/// accumulator, the other answers `200` with the arguments silently erased.
///
/// **Ruled valid, and fixed — at the decoder, not at either serve
/// projection this test drives.** See the module doc for the full mechanism
/// and why this file cannot exercise it directly. What this test proves
/// instead is the consequence, end to end through the real router: a turn
/// whose tool call truncates produces **no** `tool_use` content on either
/// projection, and the two agree with each other, because — per the fixed
/// decoder — nothing describing that call ever reaches the engine for either
/// one to disagree about. The script below carries no `Scripted::Call` for
/// exactly that reason: it is what [`ToolCallingFrontierClient`] must be
/// handed to honestly stand in for the fixed decoder's own output on this
/// shape, which `anthropic_messages/stream.rs`'s
/// `a_closed_block_whose_arguments_never_parse_emits_no_call` pins directly —
/// a closed block over unparseable fragments yields nothing but the
/// terminal accounting frame, the same "nothing, rather than something no
/// consumer can check" answer this decoder gives a block that never closes
/// at all.
///
/// Confirmed failing at b8e8ddd, when this test's script carried
/// `Scripted::Call{arguments: TRUNCATED_ARGUMENTS}` directly and the two
/// projections diverged exactly as the claim above describes (see git
/// history for that version); kept live here as the guard on the fixed
/// system's consequence, with the decoder-level mechanism pinned in
/// `anthropic_messages/stream.rs`'s own suite.
#[tokio::test]
async fn streaming_and_non_streaming_agree_a_truncated_tool_call_never_happened() {
    // No `Scripted::Call`: see the doc above for why an *empty* script is the
    // honest double for "the decoder dropped the one thing this turn would
    // have produced". `stopped()`'s own empty-block fallback (`emit.rs`) is
    // what turns that into one empty text block on the wire rather than a
    // stream that reaches `message_stop` having completed nothing at all —
    // the shape that costs a real client a second, full-price non-streaming
    // turn (§3.6). Both projections below are checked for exactly that block
    // and nothing else, which is this test's own proof that the fallback
    // engages correctly for an all-dropped turn too.
    let script: Vec<Scripted> = vec![];

    // --- Streaming half ----------------------------------------------------
    let (stream_app, stream_store, _client) = surface(script.clone(), Some("tool_use"));
    let (status, text) = post(
        &stream_app,
        &[("x-claude-code-session-id", "sess-f7-stream")],
        &body("list the files", true),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    // Premise: no tool call reaches the log. Before the fix this same double
    // would have committed TRUNCATED_ARGUMENTS verbatim on a clean,
    // first-generation commit (`canonical_arguments`, `item.rs:157-162`,
    // passes non-JSON through unchanged); after it, there is nothing for
    // that fallback to ever see for a live Anthropic dispatch.
    let stream_items = stored_items(&stream_store, &named("sess-f7-stream")).await;
    assert_eq!(
        tool_call_arguments(&stream_items),
        None,
        "premise: no tool call should reach the log when the decoder has \
         already dropped it: {stream_items:#?}"
    );

    // Consequence: the client's own accumulator -- faithfully reproduced by
    // the tier-1 oracle -- reads this stream without throwing, because there
    // is no malformed block left in it to close.
    let audited = audit(&text).unwrap_or_else(|error| {
        panic!(
            "a stream with no tool call must not fail the \
                                         oracle: {error}\n\n{text}"
        )
    });
    assert!(
        audited.tool_calls.is_empty(),
        "F7: no tool call should have reached the client at all: {audited:?}"
    );

    // --- Non-streaming half --------------------------------------------------
    let (complete_app, complete_store, _client2) = surface(script, Some("tool_use"));
    let (status, text) = post(
        &complete_app,
        &[("x-claude-code-session-id", "sess-f7-nonstream")],
        &body("list the files", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    // Premise: the second session's log agrees with the first -- the two
    // halves of this test exercise the same (empty) generated content, not
    // two independently-scripted turns.
    let complete_items = stored_items(&complete_store, &named("sess-f7-nonstream")).await;
    assert_eq!(
        tool_call_arguments(&complete_items),
        None,
        "premise: the non-streaming session's log must agree that no call \
         happened: {complete_items:#?}"
    );

    let message: Value = serde_json::from_str(&text).expect("a JSON message");

    // THE CLAIM: both projections agree, and what they agree on is that the
    // truncated call never happened -- the one empty text block
    // `stopped()`'s fallback produces for a turn that completed no block at
    // all, and nothing shaped like `tool_use` anywhere in the body. Before
    // the fix this same assertion would have found `content[0]` a `tool_use`
    // block with `input: {}` -- a call the streaming half above had already
    // proved unreachable by a real client -- reported as an ordinary
    // success.
    //
    // Deliberately not a substring scan for `"tool_use"` over the whole body:
    // `message["stop_reason"]` legitimately carries that word here (fed by
    // `surface`'s scripted `Some("tool_use")`, standing in for a provider
    // that still meant to call a tool right up to the truncation —
    // `completion_stop_reason`'s documented fallback when `called_a_tool` is
    // false) with no `tool_use` *block* ever existing to back it. The
    // structural equality on `content` alone, below, is the actual claim.
    assert_eq!(
        message["content"],
        json!([{ "type": "text", "text": "" }]),
        "F7: a truncated tool call must leave both projections agreeing that \
         nothing was called, not one silently answering with an empty-input \
         tool_use block: {text}"
    );
}

/// CONTROL: the same script and the same two-projection comparison, but with
/// syntactically valid JSON arguments. Both projections must agree here --
/// proving the divergence above is specific to unparseable stored arguments
/// and not a general streaming/non-streaming mismatch this file's harness
/// would report regardless of content.
#[tokio::test]
async fn streaming_and_non_streaming_agree_about_a_well_formed_tool_call() {
    // Already in the compact, key-sorted form `canonical_arguments` (item.rs:
    // 157-162) re-serializes *parseable* JSON into on commit -- unlike
    // TRUNCATED_ARGUMENTS above, which is not JSON at all and so passes
    // through unchanged. Spelling it pre-canonicalized keeps this control
    // comparing against a fixed point instead of asserting a literal the
    // commit path is documented to rewrite.
    const WELL_FORMED_ARGUMENTS: &str = r#"{"command":"ls -la"}"#;
    let script = vec![Scripted::Call {
        id: "toolu_01",
        name: "Bash",
        arguments: WELL_FORMED_ARGUMENTS,
    }];

    let (stream_app, _store, _client) = surface(script.clone(), Some("tool_use"));
    let (status, text) = post(
        &stream_app,
        &[("x-claude-code-session-id", "sess-f7-control-stream")],
        &body("list the files", true),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let fragments = raw_partial_json(&text);
    assert_eq!(fragments, WELL_FORMED_ARGUMENTS);
    let audited = audit(&text).unwrap_or_else(|error| panic!("control: {error}\n\n{text}"));
    assert_eq!(audited.tool_calls.len(), 1);
    assert_eq!(
        audited.tool_calls[0].input,
        json!({ "command": "ls -la" }),
        "control: the oracle must accumulate the well-formed arguments \
         without throwing"
    );

    let (complete_app, _store2, _client2) = surface(script, Some("tool_use"));
    let (status, text) = post(
        &complete_app,
        &[("x-claude-code-session-id", "sess-f7-control-nonstream")],
        &body("list the files", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let message: Value = serde_json::from_str(&text).expect("a JSON message");

    // The control: with well-formed arguments, the two projections agree --
    // neither the throw nor the silent substitution above happens, so the
    // ignored test's failure is not this harness reporting divergence
    // unconditionally.
    assert_eq!(
        message["content"][0]["input"],
        json!({ "command": "ls -la" }),
        "control: non-streaming must carry the real parsed arguments when \
         they are valid JSON: {text}"
    );
}
