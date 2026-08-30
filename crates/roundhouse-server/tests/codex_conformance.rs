// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Responses API surface, validated by Codex's own client.
//!
//! The oracle is the point. A hand-written assertion on our SSE bytes only ever
//! proves that we agree with our reading of the spec, and the failures this
//! surface can have are exactly the ones a reading misses: an item whose type a
//! client knows but whose shape it cannot parse is dropped in silence, so a turn
//! arrives looking empty rather than looking wrong. So the frames go through
//! [`codex_api::ResponsesClient`] — the same parser a real agent runs — and the
//! assertions are on the [`ResponseEvent`]s that come out the other side.
//!
//! Five of these drive the router as a `tower::Service` with no socket bound,
//! because that is enough to test the protocol and it keeps the tests hermetic.
//! The sixth binds one, so that the parts a `Service` call skips — a real POST,
//! a real chunked body, the client's real HTTP stack — are covered too.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use http_body_util::BodyExt;
use tower::ServiceExt;

use codex_api::{
    ApiError, Compression, Provider, ResponseEvent, ResponsesApiRequest, ResponsesClient,
    ResponsesOptions,
};
use codex_client::{HttpClientBuilder, ReqwestTransport};
use codex_protocol::models::{ContentItem, ResponseItem};
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{EchoFrontierClient, StaticFrontierCatalog};
use roundhouse_server::{
    ControlPlane, Conversations, EchoLocalExecutor, Engine, EngineConfig, responses_router,
};

mod common;
use common::codex::{
    NoAuth, RouterTransport, collect, frames, function_call_output_item, request, user_message,
};
use common::{Scripted, ToolCallingFrontierClient, frontier_catalog};

/// What the echo provider answers with, and therefore what a turn's assistant
/// item contains.
const ANSWER: &str = "frontier answer";

// ---------------------------------------------------------------------------
// The service under test
// ---------------------------------------------------------------------------

/// A router over a fresh in-memory store, plus that store for direct probing.
fn surface() -> (Router, Arc<MemoryStore>) {
    surface_with(frontier_catalog())
}

/// As [`surface`], but with a caller-chosen catalog.
///
/// An empty one leaves an engine with nowhere to route, which is how a
/// post-admission failure is produced without a stub provider.
fn surface_with(catalog: StaticFrontierCatalog) -> (Router, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        catalog,
        Arc::new(EchoFrontierClient::new(ANSWER)),
        Arc::new(AffinityPolicy::new()),
        EngineConfig::default(),
    ));
    (
        responses_router(
            ControlPlane::open(),
            engine,
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
    )
}

// ---------------------------------------------------------------------------
// Codex's client, pointed at the router
// ---------------------------------------------------------------------------

/// This suite's provider: the shared retry-nothing builder, named for this
/// surface so a client-side failure says which suite raised it.
fn provider(base_url: &str) -> Provider {
    common::codex::provider(base_url, "roundhouse")
}

fn assistant_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

/// As [`surface`], but dispatching through a frontier that calls tools.
///
/// The engine is otherwise identical, so a turn served here routes exactly as
/// every other test's does: the only difference is what comes back off the
/// stream, which is the point.
fn surface_calling(script: Vec<Scripted>) -> (Router, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        // This wire names no stop reason for an ordinary completion, which is
        // what the Responses dispatch decoder reports — so the double says the
        // same thing rather than a convenient `tool_use`.
        Arc::new(ToolCallingFrontierClient::new(script, None)),
        Arc::new(AffinityPolicy::new()),
        EngineConfig::default(),
    ));
    (
        responses_router(
            ControlPlane::open(),
            engine,
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
    )
}

/// Drive one turn through Codex's client and collect what it parsed.
///
/// The `Err` is the client's own, so a stream that ends without a terminal event
/// arrives here as the error Codex would report to its user.
async fn drive(app: &Router, request: ResponsesApiRequest) -> Result<Vec<ResponseEvent>, ApiError> {
    let client = ResponsesClient::new(
        RouterTransport { app: app.clone() },
        provider("http://roundhouse.test/v1"),
        Arc::new(NoAuth),
    );
    collect(
        client
            .stream_request(request, ResponsesOptions::default())
            .await?,
    )
    .await
}

// ---------------------------------------------------------------------------
// Reading what the client read
// ---------------------------------------------------------------------------

/// The event sequence, named the way the wire names it.
fn sequence(events: &[ResponseEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            ResponseEvent::Created => "response.created",
            ResponseEvent::OutputItemAdded(_) => "response.output_item.added",
            ResponseEvent::OutputTextDelta(_) => "response.output_text.delta",
            ResponseEvent::OutputItemDone(_) => "response.output_item.done",
            ResponseEvent::Completed { .. } => "response.completed",
            _ => "other",
        })
        .collect()
}

/// The answer as the client assembled it from deltas.
fn answer(events: &[ResponseEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputTextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

fn response_id(events: &[ResponseEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            ResponseEvent::Completed { response_id, .. } => Some(response_id.clone()),
            _ => None,
        })
        .expect("a completed turn names its response")
}

/// The text a message item carries, or `None` if it is not a message.
///
/// `ResponseItem::Other` is the failure this returns `None` for: it is what an
/// item of an unknown *or unparseable* shape becomes, and it is silent.
fn message_text(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::Message { role, content, .. } if role == "assistant" => Some(
            content
                .iter()
                .filter_map(|part| match part {
                    ContentItem::OutputText { text } | ContentItem::InputText { text } => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// The session's committed items, read straight out of the store.
async fn stored_items(store: &MemoryStore, session_id: &str) -> Vec<Item> {
    store
        .read_events(&SessionId::new(session_id), 0, 1024)
        .await
        .expect("the session exists")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ItemAppended { item } => Some(item),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// One turn, parsed by the client that will consume this API in production.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_client_parses_a_full_turn() {
    ensure_rustls_crypto_provider();
    let (app, _store) = surface();

    let events = drive(&app, request("sess-full-turn", vec![user_message("hello")]))
        .await
        .expect("the client must parse the turn");

    assert_eq!(
        sequence(&events),
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ]
    );

    // The item events must be message items, not `Other`. That distinction is
    // the whole reason this test exists: an item the client cannot parse is
    // dropped without complaint, and every assertion about the sequence would
    // still pass while the turn arrived empty.
    let ResponseEvent::OutputItemAdded(added) = &events[1] else {
        panic!("expected an added item: {:?}", events[1]);
    };
    assert_eq!(
        message_text(added),
        Some(String::new()),
        "the announced item must parse as an empty assistant message: {added:?}"
    );
    let ResponseEvent::OutputItemDone(done) = &events[3] else {
        panic!("expected a done item: {:?}", events[3]);
    };
    assert_eq!(message_text(done), Some(ANSWER.to_string()));
    assert_eq!(answer(&events), ANSWER);

    let ResponseEvent::Completed {
        response_id,
        token_usage,
        ..
    } = &events[4]
    else {
        panic!("expected a completion: {:?}", events[4]);
    };
    assert!(!response_id.is_empty());
    let usage = token_usage
        .as_ref()
        .expect("a completion must carry usage the client can bill against");
    assert_eq!(
        usage.total_tokens,
        usage.input_tokens + usage.output_tokens,
        "total_tokens must be the sum the client would otherwise have to guess"
    );
    assert!(usage.input_tokens > 0);
}

/// **A tool-calling turn, read by the parser a real agent runs.**
///
/// The oracle is doing the load-bearing work here and it is worth naming what it
/// rules out. `ResponseItem` has an `Other` variant: an item whose shape the
/// client cannot deserialize becomes `Other` silently, and a turn made of those
/// arrives looking empty rather than looking wrong. So the assertion is not that
/// the frames are named right but that the *client* got a `FunctionCall` with
/// the call id, the name and the arguments it needs to run the tool — which is
/// the whole of what a codex agent does with our answer.
///
/// The pinned parser (`codex-api/src/sse/responses.rs` @ `6344a65`) reads the
/// call off `response.output_item.done` and puts
/// `response.function_call_arguments.delta` in its explicitly-unhandled arm, so
/// the delta frame this surface also sends must be *ignorable*: it may not
/// become an event, and it may not break the sequence around it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_client_parses_a_tool_call_this_deployment_emitted() {
    ensure_rustls_crypto_provider();
    let (app, store) = surface_calling(vec![
        Scripted::Text("looking"),
        Scripted::Call {
            id: "call_1",
            name: "shell",
            // Unsorted keys and loose spacing, so the canonical form the log
            // stores is visibly not the model's own bytes.
            arguments: r#"{"workdir": "/src", "command": ["ls"]}"#,
        },
    ]);

    let events = drive(&app, request("sess-tool-call", vec![user_message("list")]))
        .await
        .expect("the client must parse the turn");

    assert_eq!(
        sequence(&events),
        vec![
            "response.created",
            // The text item, announced by its first delta.
            "response.output_item.added",
            "response.output_text.delta",
            // Closed by the call that follows it, *before* the call goes out:
            // the client rebuilds its history in the order it was handed the
            // items, and the log holds the text ahead of the call.
            "response.output_item.done",
            // The call: announced, its arguments streamed (a frame this parser
            // ignores, and therefore no event), then handed over whole.
            "response.output_item.added",
            "response.output_item.done",
            "response.completed",
        ],
        "{events:#?}"
    );

    // The message closed first, and it carries the text — so a client
    // assembling items in `done` order gets `[message, call]`, which is the
    // order the log holds them in and therefore the order its resend has to be
    // admitted as a prefix.
    let ResponseEvent::OutputItemDone(message) = &events[3] else {
        panic!("expected the message's done item: {:?}", events[3]);
    };
    assert_eq!(message_text(message), Some("looking".to_string()));

    let ResponseEvent::OutputItemDone(done) = &events[5] else {
        panic!("expected the call's done item: {:?}", events[5]);
    };
    let ResponseItem::FunctionCall {
        name,
        arguments,
        call_id,
        ..
    } = done
    else {
        panic!(
            "the client parsed the call as {done:?} — an item it cannot read \
             becomes `Other` and the turn arrives looking empty"
        );
    };
    assert_eq!(name, "shell");
    assert_eq!(call_id, "call_1");
    assert_eq!(
        arguments, r#"{"command":["ls"],"workdir":"/src"}"#,
        "the arguments the client would run must be the ones the log holds"
    );

    // And the announcement is the same call, so a streaming consumer that
    // renders on `added` and completes on `done` sees one call rather than two.
    let ResponseEvent::OutputItemAdded(added) = &events[4] else {
        panic!("expected the call's added item: {:?}", events[4]);
    };
    let ResponseItem::FunctionCall {
        call_id: announced,
        arguments: announced_arguments,
        ..
    } = added
    else {
        panic!("the announcement must parse as the same kind of item: {added:?}");
    };
    assert_eq!(announced, "call_1");
    assert_eq!(
        announced_arguments, "",
        "the announcement carries no arguments; they arrive on the deltas and \
         the done"
    );

    // The log holds the call, stamped as ours — which is what makes the frames
    // above a projection of the session rather than a pass-through of the
    // upstream's bytes.
    let items = stored_items(&store, "sess-tool-call").await;
    let call = items
        .iter()
        .find(|item| matches!(item.content, ItemContent::ToolCall { .. }))
        .expect("the emitted call is durable");
    assert!(
        call.response_id.is_some(),
        "an emitted call carries this response's stamp: {call:?}"
    );
    assert_eq!(
        call.content,
        ItemContent::ToolCall {
            call_id: "call_1".into(),
            name: "shell".into(),
            arguments: r#"{"command":["ls"],"workdir":"/src"}"#.into(),
        }
    );
}

/// **The loop closes on this dialect too: the client runs the call and sends the
/// output back as `function_call_output`.**
///
/// The resend is built from the item the client actually parsed, so this asserts
/// prefix admission against a real round trip rather than a hand-written one — a
/// re-encoding anywhere on the path would show up here as a second session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_clients_tool_output_comes_back_onto_the_same_session() {
    ensure_rustls_crypto_provider();
    let session = "sess-tool-loop";
    // Text *and* a call, because the interleaving is what the resend's order has
    // to preserve: a turn whose only content is a call would pass this test even
    // if the two items came back in the wrong order.
    let (app, store) = surface_calling(vec![
        Scripted::Text("looking"),
        Scripted::Call {
            id: "call_1",
            name: "shell",
            arguments: r#"{"workdir": "/src", "command": ["ls"]}"#,
        },
    ]);

    let first = drive(&app, request(session, vec![user_message("list")]))
        .await
        .expect("the first turn completes");
    // Every item the client was handed, in the order it was handed them — which
    // is the order it will send them back in.
    let handed: Vec<ResponseItem> = first
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputItemDone(item) => Some(item.clone()),
            _ => None,
        })
        .collect();
    assert!(
        matches!(
            handed.as_slice(),
            [
                ResponseItem::Message { .. },
                ResponseItem::FunctionCall { .. }
            ]
        ),
        "the client must be handed the message before the call, as the log holds \
         them: {handed:#?}"
    );
    let after_first = stored_items(&store, session).await;

    // Turn two as codex composes it: the request, the items it was handed
    // *verbatim*, and the output of running the call.
    let mut resent = vec![user_message("list")];
    resent.extend(handed);
    resent.push(function_call_output_item("call_1", "main.rs\n"));
    let second = drive(&app, request(session, resent))
        .await
        .expect("the second turn completes");
    assert!(matches!(
        second.last(),
        Some(ResponseEvent::Completed { .. })
    ));

    let after_second = stored_items(&store, session).await;
    assert_eq!(
        after_second[..after_first.len()],
        after_first[..],
        "the first turn's items must survive unchanged, or the session forked"
    );
    let appended: Vec<&ItemContent> = after_second[after_first.len()..]
        .iter()
        .map(|item| &item.content)
        .collect();
    assert!(
        matches!(
            appended.first(),
            Some(ItemContent::ToolResult { call_id, .. }) if call_id == "call_1"
        ),
        "exactly the result should be new input: {appended:#?}"
    );
}

/// A second turn re-sends the whole conversation; only the new part is appended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_turn_extends_rather_than_duplicates() {
    ensure_rustls_crypto_provider();
    let (app, store) = surface();
    let session = "sess-two-turns";

    let first = drive(&app, request(session, vec![user_message("hello")]))
        .await
        .expect("the first turn completes");
    assert_eq!(answer(&first), ANSWER);

    // Exactly what a client sends next: everything it had, plus the answer it
    // was just given, plus the new question.
    let grown = vec![
        user_message("hello"),
        assistant_message(&answer(&first)),
        user_message("and again"),
    ];
    let second = drive(&app, request(session, grown))
        .await
        .expect("the second turn completes");
    assert_ne!(
        response_id(&second),
        response_id(&first),
        "a new question is a new response"
    );

    // The claim under test: the resent history was recognized as the prefix it
    // is, not appended a second time.
    let items = stored_items(&store, session).await;
    let first_question = items
        .iter()
        .filter(|item| **item == Item::user_text("hello"))
        .count();
    assert_eq!(
        first_question, 1,
        "the resent history must not be re-appended: {items:#?}"
    );
    assert_eq!(
        items
            .iter()
            .filter(|item| item.role == Role::Assistant)
            .count(),
        2,
        "one assistant item per answered turn: {items:#?}"
    );
    assert_eq!(
        items.first().map(|item| &item.content),
        Some(&ItemContent::Text {
            text: "be brief".to_string()
        }),
        "instructions are stored once, at the head"
    );
}

/// The same request twice is the same turn, answered with the same response.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identical_retry_replays_the_same_response() {
    ensure_rustls_crypto_provider();
    let (app, store) = surface();
    let session = "sess-retry";

    let first = drive(&app, request(session, vec![user_message("hello")]))
        .await
        .expect("the first attempt completes");
    // The client never saw the answer — a dropped stream, a 5xx on the way
    // back — and re-POSTs the identical body.
    let retry = drive(&app, request(session, vec![user_message("hello")]))
        .await
        .expect("the retry completes");

    assert_eq!(
        response_id(&retry),
        response_id(&first),
        "a retry must be answered with the response it already paid for"
    );
    assert_eq!(sequence(&retry), sequence(&first));
    assert_eq!(answer(&retry), ANSWER);

    // A replay is not a second turn: nothing new was generated and nothing new
    // was appended.
    assert_eq!(
        stored_items(&store, session)
            .await
            .iter()
            .filter(|item| item.role == Role::Assistant)
            .count(),
        1
    );
}

/// A history that disagrees with the log gets a fresh session, and keeps it.
///
/// Editing or compacting the conversation is something agents do, and what comes
/// back is no longer a continuation of what we stored. The rebinding matters
/// less than what follows it: the turn after must extend the new session rather
/// than fork again, or a client that edited once would get a cold session on
/// every turn from then on and never share a prefix with itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edited_history_rebinds_and_then_continues() {
    ensure_rustls_crypto_provider();
    let (app, store) = surface();
    let session = "sess-edited";

    let first = drive(&app, request(session, vec![user_message("hello")]))
        .await
        .expect("the first turn completes");

    // The client rewrote its own history: the question it now claims to have
    // asked is not the one this session recorded.
    let edited = vec![
        user_message("hello, rephrased"),
        assistant_message(&answer(&first)),
        user_message("and now this"),
    ];
    let second = drive(&app, request(session, edited.clone()))
        .await
        .expect("the rebound turn completes");
    assert_ne!(response_id(&second), response_id(&first));

    let mut grown = edited;
    grown.push(assistant_message(&answer(&second)));
    grown.push(user_message("one more"));
    drive(&app, request(session, grown))
        .await
        .expect("the turn after the rebinding completes");

    // The original session kept its history and gained nothing; the rebound one
    // holds the edited conversation exactly once, which is what proves the third
    // turn extended it rather than forking a third session.
    assert_eq!(stored_items(&store, session).await.len(), 3);
    let rebound = stored_items(&store, &format!("{session}#g1")).await;
    assert_eq!(
        rebound
            .iter()
            .filter(|item| **item == Item::user_text("hello, rephrased"))
            .count(),
        1,
        "the rebound session must be extended, not replaced: {rebound:#?}"
    );
    assert_eq!(
        rebound
            .iter()
            .filter(|item| item.role == Role::User)
            .count(),
        3
    );
}

/// A turn that fails after admission reaches the client as an error, not a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_turn_surfaces_as_a_stream_error() {
    ensure_rustls_crypto_provider();
    // Nothing to route to, so the turn is admitted and then fails.
    let (app, _store) = surface_with(StaticFrontierCatalog::new(vec![]));

    let error = drive(&app, request("sess-nowhere", vec![user_message("hello")]))
        .await
        .expect_err("a turn with nowhere to go must not report success");

    // That this returns at all is half the assertion: the client reports a
    // terminal failure only once the body ends, so a server that emitted
    // `response.incomplete` and held the connection open would stall here.
    let message = error.to_string().to_lowercase();
    assert!(
        message.contains("incomplete"),
        "the failure must name itself: {error}"
    );
}

/// The frames themselves, in the order the contract fixes them in.
///
/// The client's parser is forgiving by design — it ignores what it does not
/// recognize — so it cannot prove that nothing extra went out, or that the
/// terminal frame was last. Reading the body directly can. It is collected
/// whole rather than read incrementally because this endpoint's stream always
/// ends by itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordering_is_enforced_at_the_frame_level() {
    ensure_rustls_crypto_provider();
    let (app, _store) = surface();

    let body = serde_json::to_string(&request("sess-frames", vec![user_message("hello")]))
        .expect("the request encodes");
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("call");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");

    let frames = frames(response.into_body()).await;
    let types: Vec<&str> = frames.iter().map(|frame| frame.kind()).collect();
    assert_eq!(
        types,
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ],
        "no other frame may appear on this surface, and the terminal one is last"
    );

    for frame in &frames {
        assert_eq!(
            frame.name, frame.payload["type"],
            "the SSE event name must be the payload's own type tag"
        );
    }

    // Text is only ever attributed to an item the client has been told about,
    // and by the id it was told.
    let added = types
        .iter()
        .position(|kind| *kind == "response.output_item.added")
        .expect("the item is announced");
    let delta = types
        .iter()
        .position(|kind| *kind == "response.output_text.delta")
        .expect("the answer is streamed");
    assert!(added < delta);
    assert_eq!(frames[added].payload["item"]["id"], "msg_1");
    assert_eq!(frames[delta].payload["item_id"], "msg_1");
    assert_eq!(
        frames[added].payload["item"]["content"][0]["type"], "output_text",
        "a message part must be typed, or the client drops the whole item"
    );

    let completed = &frames[types.len() - 1].payload["response"];
    assert!(completed["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(
        completed["usage"]["total_tokens"].as_u64(),
        Some(
            completed["usage"]["input_tokens"].as_u64().unwrap()
                + completed["usage"]["output_tokens"].as_u64().unwrap()
        )
    );
    assert!(completed["usage"]["input_tokens_details"]["cached_tokens"].is_number());
}

/// What this surface cannot serve is refused before a stream opens.
///
/// Driven directly, because the two refusals are for requests Codex does not
/// make: it always streams and always sends its session id, and a client that
/// learned it was optional would get one conversation per turn — a failure that
/// answers every request and shows up only as a cache that never hits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_this_surface_cannot_serve_is_refused_before_it_streams() {
    ensure_rustls_crypto_provider();
    let (app, store) = surface();

    let mut without_stream = request("sess-refused", vec![user_message("hello")]);
    without_stream.stream = false;
    let mut without_key = request("sess-refused", vec![user_message("hello")]);
    without_key.prompt_cache_key = None;

    for (body, expected) in [
        (
            serde_json::to_string(&without_stream).expect("encodes"),
            "streaming",
        ),
        (
            serde_json::to_string(&without_key).expect("encodes"),
            "prompt_cache_key",
        ),
        ("not json at all".to_string(), "malformed"),
        (
            serde_json::json!({
                "stream": true,
                "prompt_cache_key": "sess-refused",
                "input": [{ "type": "web_search_call" }],
            })
            .to_string(),
            "web_search_call",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body.clone()))
                    .expect("request"),
            )
            .await
            .expect("call");
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "expected 422 for {body}"
        );
        let payload: serde_json::Value = serde_json::from_slice(
            &response
                .into_body()
                .collect()
                .await
                .expect("body")
                .to_bytes(),
        )
        .expect("a JSON error body");
        assert_eq!(payload["error"]["code"], "invalid_request");
        assert!(
            payload["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(expected)),
            "the refusal must say what was wrong: {payload}"
        );
    }

    // None of it reached the log, and none of it created a session.
    assert!(
        store
            .last_seq(&SessionId::new("sess-refused"))
            .await
            .is_err()
    );
}

/// The same turn over a real socket, driven by the client's real HTTP stack.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_socket_round_trip() {
    ensure_rustls_crypto_provider();
    let (app, _store) = surface();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let transport = ReqwestTransport::from_http_client(
        HttpClientBuilder::new()
            .build_direct()
            .expect("a direct client for a loopback address"),
    );
    let client = ResponsesClient::new(
        transport,
        provider(&format!("http://{addr}/v1")),
        Arc::new(NoAuth),
    );

    let stream = client
        .stream_request(
            request("sess-live", vec![user_message("hello")]),
            ResponsesOptions {
                compression: Compression::None,
                ..Default::default()
            },
        )
        .await
        .expect("the POST is accepted");
    let events = collect(stream).await.expect("the client parses the turn");

    assert_eq!(
        sequence(&events),
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(answer(&events), ANSWER);
    assert!(!response_id(&events).is_empty());
}
