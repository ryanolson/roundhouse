// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Anthropic Messages surface, end to end and against the real client's
//! bodies.
//!
//! Three things are being proved here and they need different instruments.
//!
//! **That the stream is conformant** — asserted by the tier-1 oracle in
//! [`common::anthropic`], a strict reader written from the pinned spec whose
//! polarity is the opposite of the shipped types. The Responses surface can
//! borrow Codex's own parser for this; there is no equivalent for this dialect
//! (both official SDKs are deliberately non-validating, and the strict community
//! crates reject correct 2026 traffic), so the oracle is roundhouse-built and
//! every stream this suite produces goes through it. What it catches that an
//! eyeball cannot: a frame whose `event:` line and payload `type` disagree, a
//! `stop_reason` outside the spec's seven, an invented `usage` property, and the
//! ordering mistakes Claude Code's accumulator *throws* on rather than skips.
//!
//! **That a session survives a resend** — asserted against the store, because
//! the failure is silent: a prefix check that forks on every second turn still
//! answers every turn, and only the log shows the conversation being appended
//! twice.
//!
//! **That it is the real client's shape** — asserted against
//! `tests/fixtures/claude-2.1.251-*.json`, two request bodies captured from the
//! native 2.1.251 binary on 2026-08-29 through a loopback mock (isolated
//! `CLAUDE_CONFIG_DIR`, cleared environment, fake API key, `ANTHROPIC_BASE_URL`
//! pointed at the mock). Only `metadata.user_id`'s `device_id` is edited, to a
//! placeholder of the same shape; everything else is verbatim, tools and
//! 9 KB system prompt included. Two of those bytes falsified a ruling made from
//! reading alone — see
//! `the_shipping_clients_two_turns_are_one_conversation_but_for_the_prompt_it_changed`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use futures::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_core::validate::{BriefConfig, Objective, ValidationBrief};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierChunk, FrontierClient, FrontierError, FrontierQuote, FrontierStream,
};
use roundhouse_server::messages_api::wire::{CreateMessageParams, canonicalize, session_key};
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, Conversations, EchoLocalExecutor, Engine, messages_router,
};

mod common;
use common::anthropic::{Accumulated, StrictErrorKind, audit, split_frames};
use common::{ScriptedFrontierClient, config, frontier_catalog, key, sha256_hex};

/// What the echo provider answers with, and therefore what a turn says.
const ANSWER: &str = "frontier answer";

/// F2: what [`PartialThenFailClient`]'s first call streams before it dies.
const PARTIAL: &str = "the first half of the answer, ";
/// F2: what every later call streams to completion — deliberately unrelated
/// text, so a byte match with [`PARTIAL`] cannot happen by coincidence.
const CONTINUATION: &str = "and only the second half.";

/// The two captured bodies, verbatim but for the redacted device fingerprint.
const TURN_ONE: &str = include_str!("fixtures/claude-2.1.251-turn-1.json");
const TURN_TWO: &str = include_str!("fixtures/claude-2.1.251-turn-2-continue.json");

// ---------------------------------------------------------------------------
// The service under test
// ---------------------------------------------------------------------------

fn engine(store: Arc<MemoryStore>) -> Arc<Engine<MemoryStore, ByteTokenizer>> {
    Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new(ANSWER)),
        Arc::new(AffinityPolicy::new()),
        config(),
    ))
}

/// A router over a fresh in-memory store, plus that store for direct probing.
///
/// One store and not two: the surface reads a session's stored items to compute
/// the resent-prefix delta, so a router holding its own store would recompute
/// every conversation from empty and every test here would pass while the
/// property under test was gone.
fn surface() -> (Router, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    (
        messages_router(
            ControlPlane::open(),
            engine(Arc::clone(&store)),
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
    )
}

/// As [`engine`], but dispatching through [`ScriptedFrontierClient`] rather
/// than the plain echo, so a test can recover what the engine actually asked
/// the frontier for — not just what the turn answered with. Same catalog, same
/// config, same policy: only the client's type changes, so a test built on
/// this must route exactly as the ordinary `surface()` tests do.
fn engine_scripted(
    store: Arc<MemoryStore>,
    client: Arc<ScriptedFrontierClient>,
) -> Arc<Engine<MemoryStore, ByteTokenizer>> {
    Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        client,
        Arc::new(AffinityPolicy::new()),
        config(),
    ))
}

/// As [`surface`], plus the [`ScriptedFrontierClient`] handle for reading back
/// `quotes_seen()` after a request.
fn surface_scripted() -> (Router, Arc<MemoryStore>, Arc<ScriptedFrontierClient>) {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ScriptedFrontierClient::new(ANSWER));
    (
        messages_router(
            ControlPlane::open(),
            engine_scripted(Arc::clone(&store), Arc::clone(&client)),
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
        client,
    )
}

/// F2's provider double: the first call streams some text and then dies
/// mid-answer (the shape that commits a partial and reports
/// `overloaded_error`); every later call streams a distinct, independent reply
/// to completion. Every `quote.prompt` handed to the client is recorded, so a
/// test can see exactly what context a retried generation was built from —
/// not just what it answered with.
struct PartialThenFailClient {
    calls: AtomicUsize,
    prompts_seen: Mutex<Vec<String>>,
}

impl PartialThenFailClient {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            prompts_seen: Mutex::new(Vec::new()),
        }
    }

    fn prompts_seen(&self) -> Vec<String> {
        self.prompts_seen
            .lock()
            .expect("the recording mutex is never held across a panic in this harness")
            .clone()
    }
}

#[async_trait]
impl FrontierClient for PartialThenFailClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.prompts_seen
            .lock()
            .expect("the recording mutex is never held across a panic in this harness")
            .push(quote.prompt.clone());
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(futures::stream::iter([
                Ok(FrontierChunk::OutputText(PARTIAL.to_string())),
                Err(FrontierError::Upstream(
                    "provider exploded mid-answer".into(),
                )),
            ])
            .boxed());
        }
        Ok(futures::stream::iter([
            Ok(FrontierChunk::OutputText(CONTINUATION.to_string())),
            Ok(FrontierChunk::Done {
                input_tokens: quote.prompt.len() as u64,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: CONTINUATION.len() as u64,
                reasoning_tokens: 0,
                provider_reported_cost: None,
            }),
        ])
        .boxed())
    }
}

/// As [`surface`], but over [`PartialThenFailClient`].
fn surface_partial_then_fail() -> (Router, Arc<MemoryStore>, Arc<PartialThenFailClient>) {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(PartialThenFailClient::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::clone(&client) as Arc<dyn FrontierClient>,
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

// ---------------------------------------------------------------------------
// Driving one request
// ---------------------------------------------------------------------------

/// A minimal streaming create, the way the client shapes one.
fn body(text: &str) -> Value {
    json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [{ "role": "user", "content": text }],
    })
}

/// `POST` a body with a caller-chosen header set, over the router as a service.
async fn post(
    app: &Router,
    uri: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
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
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a readable body")
        .to_bytes();
    (
        status,
        headers,
        String::from_utf8(bytes.to_vec()).expect("bodies are UTF-8"),
    )
}

/// One streaming turn, put through the strict oracle.
///
/// The oracle rather than a frame-name list, because the frame names are the
/// part a wrong implementation gets right: what it gets wrong is a payload the
/// client cannot parse, and a name-only assertion is green for both.
async fn stream(app: &Router, headers: &[(&str, &str)], body: &Value) -> Accumulated {
    let (status, _, text) = post(app, "/v1/messages", headers, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    audit(&text).unwrap_or_else(|error| panic!("the stream is not conformant: {error}\n\n{text}"))
}

/// The session's committed items, read straight out of the store.
async fn stored_items(store: &MemoryStore, session_id: &str) -> Vec<Item> {
    store
        .read_events(&SessionId::new(session_id), 0, 4096)
        .await
        .expect("the session exists")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ItemAppended { item } => Some(item),
            _ => None,
        })
        .collect()
}

/// The session id a client-chosen name actually resolves to on this surface.
///
/// Every name this dialect derives lives in its own namespace (M11.1 review,
/// F6): a Messages session id and a Responses `prompt_cache_key` that read the
/// same string are not the same conversation, and putting them in one is a
/// contested log that forks on every alternating turn. A test that spelled the
/// bare header value would be asserting about a session nothing ever writes to
/// — which passes for the wrong reason.
fn named(session: &str) -> String {
    format!("anthropic_messages/{session}")
}

/// Whether the store has never heard of this session.
///
/// The honest spelling of "did not fork": `stored_items` on an absent session
/// panics rather than answering empty, and an empty answer would in any case be
/// indistinguishable from a session that exists and holds nothing.
async fn no_such_session(store: &MemoryStore, session_id: &str) -> bool {
    store.last_seq(&SessionId::new(session_id)).await.is_err()
}

fn parse(fixture: &str) -> CreateMessageParams {
    serde_json::from_str(fixture).expect("a captured body is a well-formed request")
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// **`?beta=true` is ignored, and it is proved rather than assumed.**
///
/// Claude Code posts to `/v1/messages?beta=true` on every inference request
/// (confirmed again at 2.1.251). Axum routes on the path, so the query should be
/// invisible — but "should be" is exactly the assumption that, if wrong, 404s
/// every request the shipping client makes and does so only in production.
/// Asserted on both routes and with a second, invented query parameter, because
/// a router matching on the full path-and-query would pass a one-parameter test
/// written against the parameter it was built for.
#[tokio::test]
async fn the_beta_query_the_client_appends_reaches_the_same_route() {
    let (app, _store) = surface();

    let plain = stream(&app, &[], &body("hello")).await;
    let (status, _, with_beta) = post(&app, "/v1/messages?beta=true", &[], &body("hello")).await;
    assert_eq!(status, StatusCode::OK);
    let with_beta = audit(&with_beta).expect("the query must not change the stream");
    assert_eq!(plain.text, with_beta.text);

    let (status, _, _) = post(
        &app,
        "/v1/messages?beta=true&something_from_the_next_release=1",
        &[],
        &body("hello"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a query parameter this build has never seen must not change the route"
    );

    let (status, _, _) = post(
        &app,
        "/v1/messages/count_tokens?beta=true",
        &[],
        &body("hello"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// `/v1/models` is not served, and the refusal is the router's own.
///
/// Plan R5 defers discovery: exposing the catalog would put roundhouse's routing
/// choices into the user's `/model` picker. Asserted so that "deferred" is a
/// fact about the build rather than a note in a plan — and asserted as a 4xx of
/// any shape, because which one axum picks for an unrouted path is its business
/// and not this surface's contract.
#[tokio::test]
async fn model_discovery_is_not_served() {
    let (app, _store) = surface();
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .body(Body::empty())
                .expect("a well-formed request"),
        )
        .await
        .expect("the router answers");
    assert!(
        response.status().is_client_error(),
        "the catalog must not be discoverable: {}",
        response.status()
    );
}

// ---------------------------------------------------------------------------
// One turn
// ---------------------------------------------------------------------------

/// A whole turn, judged by the strict oracle rather than by our own reading.
#[tokio::test]
async fn a_dispatched_turn_is_a_conformant_stream() {
    let (app, _store) = surface();

    let accumulated = stream(
        &app,
        &[("x-claude-code-session-id", "sess-one-turn")],
        &body("hello"),
    )
    .await;

    assert_eq!(accumulated.text, ANSWER);
    assert_eq!(accumulated.model, "claude-opus-5", "the client's own name");
    assert!(!accumulated.message_id.is_empty());
    assert_eq!(accumulated.completed_blocks, 1);
    assert_eq!(accumulated.error, None);

    // The usage the *client* computes after its own merge, not the numbers we
    // put in the frames. The three input axes are disjoint on this wire, so a
    // prompt counted once is the property; counting it under both
    // `input_tokens` and `cache_read_input_tokens` would double the total here
    // and nowhere else.
    assert!(
        accumulated.usage.total_input() > 0,
        "a turn that carried a prompt must not report a free one: {:?}",
        accumulated.usage
    );
    assert!(
        accumulated.usage.output_tokens > 0,
        "the terminal frame must carry the real output count: {:?}",
        accumulated.usage
    );
}

/// The one-token probes Claude Code opens a session with are served genuinely.
///
/// Its auth probe and its quota probe are both `stream`-less creates with
/// `max_tokens: 1` (§3.6). A surface that 4xx'd or 500'd them would fail before
/// the first turn — and the failure would look like a broken deployment rather
/// than an unimplemented mode.
#[tokio::test]
async fn the_clients_non_streaming_probe_gets_a_whole_message() {
    let (app, _store) = surface();
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[],
        &json!({
            "model": "claude-opus-5",
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "test" }],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{text}");
    let message: Value = serde_json::from_str(&text).expect("a JSON message");
    assert_eq!(message["type"], "message");
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["model"], "claude-opus-5");
    assert_eq!(message["stop_reason"], "end_turn");
    assert_eq!(
        message["content"],
        json!([{ "type": "text", "text": ANSWER }]),
        "the non-streaming body must carry the same answer the stream does"
    );
    assert!(
        message["usage"]["output_tokens"].as_u64().unwrap_or(0) > 0,
        "a turn answered without streaming still costs output: {message}"
    );
}

/// **F1 (M11.1 thermo-nuclear review), fixed and pinned.** The claim was that
/// `CreateMessageParams::max_tokens` (`wire.rs`) is read and never used anywhere
/// in `messages_api` — so the ceiling sent upstream was always
/// `EngineConfig::expected_output_tokens` (256 by default, and `main.rs` never
/// overrides it), and every real answer truncated at roughly a paragraph.
/// Ruled **valid**, and fixed by splitting the two meanings apart:
/// `FrontierQuote::output_token_cap` is the client's declared ceiling and
/// `expected_output_tokens` stays the router's pricing estimate.
///
/// PROBE: two otherwise-identical requests whose only difference is the
/// client's declared `max_tokens` — one asking for a single token, the other
/// for a million. Both halves of the split are asserted, and the second is not
/// decoration: a "fix" that wrote the client's ceiling into
/// `expected_output_tokens` would satisfy the first assertion while inflating
/// every quote, every spend reservation and every projected saving by three
/// orders of magnitude on a turn that answers in forty tokens.
///
/// The pipeline is asserted end to end rather than at the wire, because the
/// finding was about the *seam*: `AnthropicMessagesClient::body`'s own unit
/// tests already pin what a cap becomes on the wire, and what nothing pinned
/// was that a client's number reaches the quote at all.
#[tokio::test]
async fn f1_the_clients_max_tokens_is_the_dispatch_ceiling_and_not_the_estimate() {
    let (app, _store, client) = surface_scripted();

    let mut low = body("hello");
    low["max_tokens"] = json!(1);
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-f1-low")],
        &low,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let mut high = body("hello");
    high["max_tokens"] = json!(1_000_000);
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[("x-claude-code-session-id", "sess-f1-high")],
        &high,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let quotes = client.quotes_seen();
    assert_eq!(
        quotes.len(),
        2,
        "one frontier dispatch per session: {quotes:?}"
    );
    assert_eq!(
        (quotes[0].output_token_cap, quotes[1].output_token_cap),
        (Some(1), Some(1_000_000)),
        "F1: the ceiling each dispatch carried is the one its client declared, \
         verbatim — a `max_tokens: 1` auth probe (client surface §3.6) and a \
         64 000-token coding turn are not the same request: {quotes:?}"
    );
    assert_eq!(
        quotes[0].expected_output_tokens, quotes[1].expected_output_tokens,
        "F1, the other half: the *estimate* is the router's and must not move \
         with what a client declared — a quote priced at a million tokens \
         reserves a million tokens of budget for a turn that answers in forty"
    );
}

/// **F5 (post-M11.1 thermo-nuclear review, 724dba8), fixed and pinned.** The
/// claim: `ItemContent::render` is simultaneously the identity encoding, the
/// token-count encoding, and the literal upstream prompt — so an opaque
/// block (an `image`, a `document`, anything this build does not name) is
/// billed and dispatched as its raw rendered JSON, base64 payload included,
/// rather than as the media type it actually is. Ruled **valid**, and fixed at
/// the one seam all three readings share: `ItemContent::Opaque::render` is a
/// `sha256` digest placeholder, so the payload reaches none of them. The block
/// is still stored verbatim, and a model that could *see* the image needs a
/// typed content-block path, which is the future work R5 names — this is the
/// bound on what the milestone bills and ships, not an image feature.
///
/// PROBE: an `image` block (Claude Code's own shape for a pasted screenshot)
/// carrying an easily-recognized base64 payload. The two assertions state the
/// *healthy* contract — bounded billing, no raw image bytes loose in the
/// text prompt — so a red run here is the defect, not a passing one: (1) the
/// turn's client-visible, ledger-drawn input count
/// (`message_start.usage.input_tokens`, sourced from
/// `Engine::admitted_input_tokens`) must not scale with the base64 payload's
/// *character* length the way byte-for-byte tokenizing would; (2) the string
/// the frontier client actually receives (`FrontierQuote::prompt`, what
/// `anthropic_messages::body` slices into `ContentBlock::Text` —
/// `wire::ContentBlock` has no image/document variant to slice it into
/// instead) must not carry that base64 payload verbatim, i.e. as prose.
#[tokio::test]
async fn f5_an_opaque_image_block_is_neither_billed_nor_dispatched_as_raw_base64() {
    let (app, _store, client) = surface_scripted();

    // 4096 repeated 'A's: long enough to dominate the turn's token count, and
    // distinctive enough that its appearance downstream cannot be anything
    // but this block's own payload.
    let payload = "A".repeat(4096);
    let request = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "describe this image" },
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": payload,
                    },
                },
            ],
        }],
    });

    let accumulated = stream(
        &app,
        &[("x-claude-code-session-id", "sess-f5-image")],
        &request,
    )
    .await;
    assert_eq!(accumulated.error, None, "{:?}", accumulated.error);

    // An image-aware estimate reports a small constant regardless of
    // resolution; billing at or above one token per base64 character is the
    // signature of tokenizing the raw rendered JSON instead.
    assert!(
        (accumulated.usage.total_input() as usize) < payload.len(),
        "F5: a {}-byte base64 image was billed as {} total input tokens for \
         the whole turn — that is >= one token per base64 character, which is \
         what tokenizing ItemContent::Opaque::render()'s literal \
         `<block type=\"image\">{{...\"data\":\"AAAA...\"}}</block>` JSON \
         produces, not any image-aware estimate: {:?}",
        payload.len(),
        accumulated.usage.total_input(),
        accumulated.usage
    );

    let quotes = client.quotes_seen();
    assert_eq!(quotes.len(), 1, "one frontier dispatch: {quotes:?}");
    assert!(
        !quotes[0].prompt.contains(&payload),
        "F5: the dispatched prompt carries the image's raw base64 data \
         verbatim as prose text (not as an image content block), because \
         wire::ContentBlock has no image/document variant for \
         anthropic_messages::body to slice a segment into instead: {}",
        quotes[0].prompt
    );
}

/// `count_tokens` answers from this deployment's tokenizer.
///
/// The number is an estimate and the handler's doc says so; what this asserts is
/// that it is *served* and that it moves with the input. A refusal here does not
/// save the estimate's cost — the client falls back to a real one-token create
/// against the routed model — so the endpoint existing is a spend decision, not
/// a completeness one.
#[tokio::test]
async fn count_tokens_answers_and_grows_with_the_conversation() {
    let (app, _store) = surface();

    let (status, _, small) = post(&app, "/v1/messages/count_tokens", &[], &body("hi")).await;
    assert_eq!(status, StatusCode::OK, "{small}");
    let small: Value = serde_json::from_str(&small).expect("JSON");
    let small = small["input_tokens"].as_u64().expect("a count");

    let (_, _, large) = post(
        &app,
        "/v1/messages/count_tokens",
        &[],
        &body("hi, and then a great deal more text than that first one carried"),
    )
    .await;
    let large: Value = serde_json::from_str(&large).expect("JSON");
    let large = large["input_tokens"].as_u64().expect("a count");

    assert!(
        small > 0,
        "an estimate of zero for a real prompt is not one"
    );
    assert!(large > small, "{large} is not more than {small}");
}

// ---------------------------------------------------------------------------
// The session across turns
// ---------------------------------------------------------------------------

/// **Full-history resend admits the prefix and appends only the suffix.**
///
/// This is what the whole surface exists for. Claude Code re-sends the entire
/// conversation on every turn — verified again at 2.1.251, where the
/// `--continue` body replayed the mock's own reply verbatim — so a server that
/// treated the resend as input would append the conversation again on turn two,
/// bill the doubled prompt, and never match a warm prefix. The failure is
/// invisible from the client's side because every turn still answers, which is
/// why the assertion is on the log rather than on the reply.
#[tokio::test]
async fn a_resent_history_is_admitted_as_a_prefix_and_not_appended_twice() {
    let (app, store) = surface();
    let headers = [("x-claude-code-session-id", "sess-two-turns")];

    let first = stream(&app, &headers, &body("hello")).await;
    assert_eq!(first.text, ANSWER);

    // Exactly what the client sends next: everything it had, plus the answer it
    // was just given, plus the new question.
    let grown = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": [{ "type": "text", "text": ANSWER }] },
            { "role": "user", "content": "and again" },
        ],
    });
    let second = stream(&app, &headers, &grown).await;
    assert_ne!(
        second.message_id, first.message_id,
        "a new question is a new response"
    );

    let items = stored_items(&store, &named("sess-two-turns")).await;
    assert_eq!(
        items
            .iter()
            .filter(|item| **item == Item::user_text("hello"))
            .count(),
        1,
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
}

/// A history the client rewrote forks to a fresh session rather than merging.
///
/// The other half of the same rule. A compaction or an edited message is not a
/// continuation, and appending the difference would produce a conversation
/// neither side believes in — so the fork is the conservative answer, at the
/// price of a cold prefix.
#[tokio::test]
async fn a_divergent_resend_forks_rather_than_merging() {
    let (app, store) = surface();
    let headers = [("x-claude-code-session-id", "sess-fork")];

    stream(&app, &headers, &body("hello")).await;
    // The same session name, a different first message: the client edited its
    // own history out from under us.
    let forked = stream(&app, &headers, &body("actually, goodbye")).await;
    assert_eq!(forked.text, ANSWER);

    let original = stored_items(&store, &named("sess-fork")).await;
    let forked = stored_items(&store, &named("sess-fork#g1")).await;
    assert!(
        original
            .iter()
            .any(|item| *item == Item::user_text("hello")),
        "the original session keeps the history it was told: {original:#?}"
    );
    assert!(
        forked
            .iter()
            .any(|item| *item == Item::user_text("actually, goodbye")),
        "the rewritten history opens a fresh generation: {forked:#?}"
    );
    assert!(
        !forked.iter().any(|item| *item == Item::user_text("hello")),
        "and the fork starts empty rather than inheriting: {forked:#?}"
    );
}

/// **A retried turn replays its answer instead of generating a second one.**
///
/// The idempotency this dialect needs most. Claude Code re-POSTs after a 5xx and
/// after a stream that died mid-answer, and it re-sends the *same conversation*
/// when it does — so the turn id, a content hash of the whole canonicalized
/// conversation, is the same and the engine replays. A surface that generated
/// again would answer correctly, cost twice, and show nothing wrong anywhere the
/// client can see.
///
/// This is also the only test that drives the follower's replay phase, which is
/// a different code path from tailing: it re-reads the log from zero, bounded by
/// the `turn_deduplicated` marker, and projects the *earlier* response's entries
/// through the same emission. A stream assembled wrongly there is one the client
/// throws on rather than one it ignores.
#[tokio::test]
async fn a_retried_turn_replays_rather_than_answering_twice() {
    let (app, store) = surface();
    let headers = [("x-claude-code-session-id", "sess-retry")];

    let first = stream(&app, &headers, &body("hello")).await;
    // Byte for byte the request the client sends again when its connection
    // dropped before the terminal frame.
    let replayed = stream(&app, &headers, &body("hello")).await;

    assert_eq!(
        replayed.message_id, first.message_id,
        "a retry must land on the response it already paid for"
    );
    assert_eq!(
        replayed.text, first.text,
        "and carry that response's answer, assembled from the replayed deltas"
    );
    assert_eq!(replayed.completed_blocks, 1);

    let items = stored_items(&store, &named("sess-retry")).await;
    assert_eq!(
        items
            .iter()
            .filter(|item| item.role == Role::Assistant)
            .count(),
        1,
        "one answer, not two: {items:#?}"
    );
}

/// **F2: a retry after a mid-answer `overloaded_error` keeps the conversation
/// on the session it has been using all along.**
///
/// The scenario `mark_incomplete`'s own doc names as the reason it commits a
/// partial at all: "the successor can resume from it." Here the successor is
/// the *same* turn id retried after a transient failure, exactly as `Z59`
/// (`research/claude-code-client-surface.md` §3.2/§2.5) retries a mid-stream
/// `overloaded_error` by re-issuing the identical request — the client never
/// saw the partial (its parser throws on `event: error` before any
/// `message_stop`), so the retry's body cannot and does not carry it.
///
/// **What used to happen.** The log held the partial and the continuation as
/// two assistant items; the client's next resend carried only the continuation
/// it had actually received; the two disagreed at that item under `same_item`
/// and the session forked — punishing a client that did exactly what the retry
/// contract asked of it, on a turn a transient upstream failure had already
/// cost it once, and taking the routing history and warm prefix with it.
///
/// **What happens now.** An item stamped by a response the log records as
/// *incomplete* is provisional: prefix admission leaves it out of what a claim
/// is checked against, so a client that discarded it continues on the same
/// session and a client that resends it has it re-admitted as ordinary history.
/// Supersession is a reading of what the log already records — the
/// `ResponseIncomplete` event — rather than a rewrite of it; nothing committed
/// is edited or removed.
///
/// Everything else about the mechanism is asserted unchanged, because the fix
/// is deliberately the admission half only:
///
/// 1. The retry does not deduplicate (the first attempt never completed) and
///    its own prompt — captured off the double, never off the wire — still
///    contains the partial, because `Engine::plan` rehydrates from
///    `session.state().items` and the partial is a genuine cache hit on the
///    target that produced it. The wire gives no sign of this: the retry's
///    stream is an ordinary `message_start`…`message_stop`.
/// 2. The log still holds the partial and the continuation as two separate
///    assistant items. Append-only means append-only.
/// 3. The client's *next* turn, carrying only what it actually received, lands
///    on the same session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_after_a_mid_stream_failure_keeps_the_conversation_on_one_session() {
    let (app, store, client) = surface_partial_then_fail();
    let headers = [("x-claude-code-session-id", "sess-partial-retry")];

    // Attempt 1: the model gets partway through, then the provider dies.
    let first = stream(&app, &headers, &body("hello")).await;
    let failure = first
        .error
        .clone()
        .expect("a mid-stream death must end the turn in an error event");
    assert_eq!(
        failure.kind,
        StrictErrorKind::OverloadedError,
        "only this spelling is retried under subscription OAuth (§2.5's `Z59`): {failure:?}"
    );
    assert_eq!(
        first.text, PARTIAL,
        "what a live client would have rendered before the throw"
    );
    assert_eq!(
        first.completed_blocks, 1,
        "the block the prelude opened must still be closed before the error"
    );

    // Attempt 2: byte-for-byte the same request — the client's own retry,
    // unaware the partial exists.
    let retry = stream(&app, &headers, &body("hello")).await;
    assert_eq!(
        retry.error, None,
        "the retry must not itself end in an error: {:?}",
        retry.error
    );
    assert_ne!(
        retry.message_id, first.message_id,
        "the failed attempt never completed, so this is a fresh response, not a replay"
    );
    assert_eq!(
        retry.text, CONTINUATION,
        "the wire carries only the continuation — nothing marks it as one, and nothing \
         restates the partial the client already lost: {:?}",
        retry.text
    );
    assert!(
        retry.stop_reason.is_some() && retry.completed_blocks == 1,
        "an ordinary, unremarkable-looking completed turn: {retry:?}"
    );

    // The mechanism: the retried generation's own prompt silently carried the
    // partial as context, which is *why* the model produced a bare
    // continuation instead of a fresh, self-contained answer.
    let prompts = client.prompts_seen();
    assert_eq!(prompts.len(), 2, "exactly one prompt per dispatch attempt");
    assert!(
        prompts[1].contains(PARTIAL.trim_end()),
        "the retry's own prompt must contain the partial the client discarded, or the \
         continuation could not follow it as prose: {:?}",
        prompts[1]
    );

    // The log never merges the two halves into one answer.
    let after_retry = stored_items(&store, &named("sess-partial-retry")).await;
    let assistant_texts: Vec<String> = after_retry
        .iter()
        .filter(|item| item.role == Role::Assistant)
        .map(|item| item.content.render())
        .collect();
    assert_eq!(
        assistant_texts,
        vec![PARTIAL.to_string(), CONTINUATION.to_string()],
        "two separate assistant items, not one spliced answer: {assistant_texts:?}"
    );
    assert_eq!(
        after_retry
            .iter()
            .filter(|item| **item == Item::user_text("hello"))
            .count(),
        1,
        "the retry's empty suffix must not re-append the user turn: {after_retry:#?}"
    );

    // The next turn: the client's own history now, carrying only what it ever
    // actually received as "the assistant's reply."
    let next_turn = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": [{ "type": "text", "text": CONTINUATION }] },
            { "role": "user", "content": "and then?" },
        ],
    });
    let third = stream(&app, &headers, &next_turn).await;
    assert_eq!(third.error, None, "{:?}", third.error);

    // The invariant a client's honest retry-then-continue is owed: resending
    // exactly what it received must carry the conversation forward on the
    // *same* session, not fork away from it. `bind_prefix`'s own doc calls a
    // fork "the conservative answer, at the price of a cold prefix" for a
    // client that edited or compacted its history — but this client did
    // neither; it resent the unedited answer it was actually given.
    let original_after_next_turn = stored_items(&store, &named("sess-partial-retry")).await;
    assert!(
        original_after_next_turn
            .iter()
            .any(|item| *item == Item::user_text("and then?")),
        "the next turn must land on the session the client has been using all along, not fork \
         away from it over a split its own retry could not have avoided: {original_after_next_turn:#?}"
    );
    assert!(
        no_such_session(&store, &named("sess-partial-retry#g1")).await,
        "and it must not have landed in a fresh generation, which is where the routing \
         history and the warm prefix would have been left behind"
    );
    // The partial is still on the log — superseded, not deleted. Append-only
    // means the supersession is a reading of what was recorded (the
    // `ResponseIncomplete` beside it), never an edit to it.
    assert!(
        original_after_next_turn
            .iter()
            .any(|item| item.content.render() == PARTIAL),
        "the partial stays committed: {original_after_next_turn:#?}"
    );
}

/// CONTROL for F2: a client that *keeps* the partial is not punished for it
/// either — it is re-admitted as ordinary history and the conversation
/// continues on the same session.
///
/// The other half of the same rule, and the reason the fix is not "drop
/// partials on the floor": `mark_incomplete`'s own doc commits the partial so a
/// successor can resume from it, and a client whose SSE layer *did* surface the
/// bytes before the error will resend them. Both readings of the same failure
/// have to land on one session, or the surface has merely moved which honest
/// client it punishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f2_control_a_client_that_resends_the_partial_also_stays_on_one_session() {
    let (app, store, _client) = surface_partial_then_fail();
    let headers = [("x-claude-code-session-id", "sess-partial-kept")];

    let first = stream(&app, &headers, &body("hello")).await;
    assert!(first.error.is_some(), "the fixture must fail mid-answer");

    // What a client that rendered the partial before the throw resends: the
    // question, the half-answer it saw, and the next question.
    let next_turn = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": [{ "type": "text", "text": PARTIAL }] },
            { "role": "user", "content": "and then?" },
        ],
    });
    let second = stream(&app, &headers, &next_turn).await;
    assert_eq!(second.error, None, "{:?}", second.error);

    let items = stored_items(&store, &named("sess-partial-kept")).await;
    assert!(
        items
            .iter()
            .any(|item| *item == Item::user_text("and then?")),
        "the resent partial must be admitted as history rather than forked over: {items:#?}"
    );
    assert!(
        no_such_session(&store, &named("sess-partial-kept#g1")).await,
        "a client that kept the partial must not fork either"
    );
}

/// **The header and both `user_id` spellings name one session.**
///
/// R5's resolution order, asserted where it matters: a deployment serving a
/// mixed fleet — one user on 2.1.42, one on 2.1.251, one behind a Relay that
/// strips headers — must put each client's own session on one log. A reader that
/// preferred `user_id` over the header would bind a subagent's turns to its
/// parent's session; a reader that parsed only one `user_id` shape would re-key
/// every session the day a user upgraded.
#[tokio::test]
async fn the_header_and_both_user_id_shapes_reach_one_session() {
    let (app, store) = surface();
    let session = "e13acbde-ab70-46ff-b094-fd8ce95d286d";
    let modern = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [{ "role": "user", "content": "hello" }],
        "metadata": { "user_id": format!(
            "{{\"device_id\":\"{}\",\"account_uuid\":\"\",\"session_id\":\"{session}\"}}",
            "0".repeat(64),
        )},
    });
    let legacy = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": ANSWER },
            { "role": "user", "content": "again" },
        ],
        "metadata": { "user_id": format!("user_abc_account_def_session_{session}") },
    });

    // Turn one names the session in the 2.1.251 body shape and no header.
    stream(&app, &[], &modern).await;
    // Turn two names it in the pre-2.1.247 shape *and* in the header, which is
    // the mixed-fleet case: they agree, so they must resolve together.
    stream(&app, &[("x-claude-code-session-id", session)], &legacy).await;

    let items = stored_items(&store, &named(session)).await;
    assert_eq!(
        items
            .iter()
            .filter(|item| **item == Item::user_text("hello"))
            .count(),
        1,
        "the two spellings named one session, so the second turn saw a prefix: {items:#?}"
    );
    assert!(
        items.iter().any(|item| *item == Item::user_text("again")),
        "and the new question was appended to it: {items:#?}"
    );
}

/// An unnamed conversation gets a session of its own rather than a refusal.
///
/// The anonymous arm is for a bare `curl`, not for the product path: every
/// version of the client read sends `metadata.user_id` on every request. What it
/// must not do is put two unrelated callers on one log, which a content-derived
/// name would have done for two identical bodies.
#[tokio::test]
async fn two_unnamed_turns_do_not_share_a_conversation() {
    let store = Arc::new(MemoryStore::new());
    // Held rather than minted inside the router, because the session an
    // anonymous turn lands in has no name the test can predict — the binding
    // table is the only thing that knows it.
    let conversations = Arc::new(Conversations::new());
    let app = messages_router(
        ControlPlane::open(),
        engine(Arc::clone(&store)),
        Arc::clone(&store),
        Arc::clone(&conversations),
    );
    let anonymous = roundhouse_core::control::Principal::default_open();

    // Two byte-identical bodies, which is what makes this a test rather than a
    // tautology: a name derived from the request's content would put them both
    // in one session and every assertion about *answers* would still pass.
    stream(&app, &[], &body("hello")).await;
    let first = conversations
        .latest(&anonymous)
        .expect("the turn bound a session");
    stream(&app, &[], &body("hello")).await;
    let second = conversations
        .latest(&anonymous)
        .expect("the second turn bound one too");

    assert_ne!(
        first, second,
        "two identical anonymous bodies must not land in one conversation"
    );
    assert!(first.to_string().starts_with("anonymous-"), "{first}");
    assert_eq!(
        stored_items(&store, &second.to_string()).await.len(),
        2,
        "the second session holds its own question and answer and nothing else"
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// A control plane whose one project caps tokens over five hours.
fn capped_plane(max_tokens: u64) -> Arc<ControlPlane> {
    let json = json!({
        "projects": [{
            "id": "bench",
            "fair_use": { "windows": [{ "window": "5h", "max_tokens": max_tokens }] },
        }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "bench", "user": "ada", "key_sha256": sha256_hex(&key("ada")) }],
    })
    .to_string();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "messages fair-use fixture")
            .expect("the fixture config must validate"),
    ))
}

/// **The fair-use `429` in the envelope and the header this client reads.**
///
/// Two halves, and the second is the one a reading would miss. The body must say
/// `rate_limit_error`, because that is what routes a subscription-OAuth client to
/// its rate-limit UI rather than to its retry loop. And `retry-after` must be
/// *present*: that path sleeps on the header and, absent it, defaults to thirty
/// minutes floored at ten — so a two-minute window reported only in the body is a
/// two-minute ceiling the agent waits half an hour on.
#[tokio::test]
async fn a_fair_use_refusal_is_a_rate_limit_error_with_a_retry_time() {
    let plane = capped_plane(1);
    let store = Arc::new(MemoryStore::new());
    let app = messages_router(
        plane,
        engine(Arc::clone(&store)),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );
    let authorized = [(AUTHORIZATION.as_str(), &*format!("Bearer {}", key("ada")))];

    // The first turn fills the one-token window.
    let (status, _, _) = post(&app, "/v1/messages", &authorized, &body("hello")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, headers, text) = post(&app, "/v1/messages", &authorized, &body("again")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{text}");
    let refusal: Value = serde_json::from_str(&text).expect("an error envelope");
    assert_eq!(refusal["type"], "error", "{refusal}");
    assert_eq!(refusal["error"]["type"], "rate_limit_error", "{refusal}");
    assert!(
        refusal["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{refusal}"
    );
    // The machine-readable half of the same refusal, carried through unchanged.
    // An agent acts on this; a person acts on the sentence above.
    assert!(
        refusal["error"]["resets_at"].as_u64().is_some(),
        "{refusal}"
    );
    assert_eq!(refusal["error"]["roundhouse_code"], "fair_use_exceeded");
    assert!(
        headers.contains_key("retry-after"),
        "the client's 429 path sleeps on `retry-after` and defaults to half an \
         hour without it: {headers:?}"
    );
}

/// Authentication is decided before the body is read.
///
/// Ordering, asserted the only way it can be: an unauthenticated request whose
/// body is *also* unreadable must answer `401`, not `422`. A handler that parsed
/// first would let a stranger's malformed body choose this process's error path
/// — and, worse, would let a stranger name a session.
#[tokio::test]
async fn an_unauthenticated_request_is_refused_before_its_body_is_parsed() {
    let plane = capped_plane(1_000_000);
    let store = Arc::new(MemoryStore::new());
    let app = messages_router(
        plane,
        engine(Arc::clone(&store)),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{this is not JSON"))
                .expect("a well-formed request"),
        )
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let refusal: Value = serde_json::from_slice(&bytes).expect("an error envelope");

    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refusal}");
    assert_eq!(refusal["type"], "error");
    assert_eq!(
        refusal["error"]["type"], "authentication_error",
        "{refusal}"
    );

    // CONTROL: with a key, the same unreadable body is a `422` naming the body.
    // Without this the assertion above would pass for a handler that answered
    // `401` to everything.
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, format!("Bearer {}", key("ada")))
                .body(Body::from("{this is not JSON"))
                .expect("a well-formed request"),
        )
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let refusal: Value = serde_json::from_slice(&bytes).expect("an error envelope");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refusal}");
    assert_eq!(
        refusal["error"]["type"], "invalid_request_error",
        "{refusal}"
    );
}

/// A router with nowhere to route, so a turn is admitted and then fails.
fn nowhere() -> Router {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        roundhouse_fleet::StaticFrontierCatalog::new(vec![]),
        Arc::new(EchoFrontierClient::new(ANSWER)),
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    messages_router(
        ControlPlane::open(),
        engine,
        store,
        Arc::new(Conversations::new()),
    )
}

/// **A turn that fails after admission ends the stream, and says how.**
///
/// The failure arrives once the headers are out, so a status code is no longer
/// expressible and an `error` event is the only answer left. Two things have to
/// be true of it and neither is obvious.
///
/// The block opened by the prelude must be *closed* before the error — a stream
/// that ends with a block still open is one the client's accumulator never
/// finishes, and the oracle refuses it.
///
/// And the error type has to be the one the client's recovery reads. Here it is
/// `overloaded_error`, which is correct for *this* reason and only this one: a
/// catalog with nowhere to route is `IncompleteReason::UpstreamError`, a
/// transient fault that a retry can clear, and `overloaded_error` is the only
/// spelling Claude Code retries a mid-stream failure on. The complementary claim
/// — that a policy refusal or a spent budget is *not* spelled that way, because
/// an agent would then burn its whole retry budget on a turn that can never
/// succeed — is the partition asserted over every
/// [`IncompleteReason`](roundhouse_core::event::IncompleteReason) in
/// `messages_api::emit`'s own suite, where all six can be reached without
/// building six engines.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_with_nowhere_to_go_ends_the_stream_with_an_error_event() {
    let (status, _, text) = post(&nowhere(), "/v1/messages", &[], &body("hello")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the headers were already out: {text}"
    );

    let accumulated = audit(&text)
        .unwrap_or_else(|error| panic!("even a failure must be conformant: {error}\n{text}"));
    let failure = accumulated
        .error
        .expect("a turn that produced no answer must say so");
    assert_eq!(
        failure.kind,
        StrictErrorKind::OverloadedError,
        "a transient upstream failure is the one case a retry clears: {failure:?}"
    );
    assert!(!failure.message.is_empty(), "{failure:?}");
    assert_eq!(
        accumulated.completed_blocks, 1,
        "the block the prelude opened must be closed before the error"
    );
}

/// The same failure without streaming is a status code, not a `200`.
///
/// The non-streaming path still has the status line available, and a client that
/// had to parse a success body to discover a failure would not — the SDK's error
/// handling runs off the status off the streaming path. This is also the only
/// test that reaches the inverse mapping from the emission's wire vocabulary
/// back to a status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_failure_without_streaming_is_a_status_code() {
    let (status, _, text) = post(
        &nowhere(),
        "/v1/messages",
        &[],
        &json!({
            "model": "claude-opus-5",
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "test" }],
        }),
    )
    .await;
    assert!(
        status.is_client_error() || status.is_server_error(),
        "a failed turn must not answer 200 with an error body: {status} {text}"
    );
    let refusal: Value = serde_json::from_str(&text).expect("an error envelope");
    assert_eq!(refusal["type"], "error", "{refusal}");
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "the mid-stream `overloaded_error` inverts to the one status whose own \
         mapping spells it back the same way: {refusal}"
    );
    assert_eq!(refusal["error"]["type"], "overloaded_error", "{refusal}");
}

/// A content shape that cannot be stored is a `422` naming what was wrong.
#[tokio::test]
async fn an_unstorable_block_is_refused_in_the_clients_envelope() {
    let (app, _store) = surface();
    let (status, _, text) = post(
        &app,
        "/v1/messages",
        &[],
        &json!({
            "model": "claude-opus-5",
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": [{ "text": "no type here" }] }],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
    let refusal: Value = serde_json::from_str(&text).expect("an error envelope");
    assert_eq!(refusal["error"]["type"], "invalid_request_error");
    assert!(
        refusal["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("type")),
        "the refusal must name the rule it broke: {refusal}"
    );
}

/// F3 CONTROL: an ordinary large body is admitted and answered normally.
///
/// Two sizes, and the second is the finding itself. 1.5 MB was always served;
/// 4 MB is *over* axum's undisclosed 2,097,152-byte default and was refused
/// before the fix — a legitimate resent history, well inside the 32 MB the
/// platform documents, turned away for a limit nobody chose. Kept live beside
/// the probe below so that what the probe proves is specific: not "this router
/// refuses any large body" but "this router refuses exactly the bodies the
/// upstream would".
#[tokio::test]
async fn f3_control_an_ordinary_large_body_is_served_normally() {
    let (app, _store) = surface();
    for size in [1_500_000, 4_000_000] {
        let under = body(&"a".repeat(size));
        let (status, _, text) = post(&app, "/v1/messages", &[], &under).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a well-formed {size}-byte body is inside the documented 32 MB \
             ceiling and must be served: {text}"
        );
    }
}

/// **F3 (M11.1 thermo-nuclear review), fixed and pinned.** The claim:
/// `create_message` and `count_tokens` extracted `body: Bytes` with no
/// `DefaultBodyLimit` override anywhere in the workspace, so axum's implicit
/// 2 MiB cap applied — and a request over it never reached the handler at all.
/// The client got axum's own plain-text 413 ("Failed to buffer the request
/// body: length limit exceeded"), not this dialect's JSON envelope: no
/// `"type":"error"`, no `roundhouse_code`. `error_kind`'s own
/// `PAYLOAD_TOO_LARGE => "request_too_large"` row was unreachable in
/// production, exercised only by the unit test that calls `error_kind` as a
/// pure function. Ruled **valid** on both halves — the wrong limit *and* the
/// wrong envelope — and both are fixed: the routes carry
/// `DefaultBodyLimit::max(MAX_REQUEST_BYTES)` at the platform's documented
/// 32 MB (`research/claude-code-client-surface.md` §3.6), and the `Bytes`
/// rejection is translated by the `RequestBody` extractor into a refusal
/// `MessagesError` renders.
///
/// PROBE: a well-formed, otherwise-legitimate request that crosses the *new*
/// ceiling — the control above is the same shape under it. What is asserted is
/// the envelope as much as the status: a client that cannot parse a refusal
/// treats it as an unparseable stream, and this dialect's client answers that
/// by re-issuing the whole turn (§3.6).
#[tokio::test]
async fn f3_an_oversized_body_is_refused_in_the_clients_envelope() {
    let (app, _store) = surface();

    // Over the 32 MB ceiling by a comfortable margin, and legitimate in every
    // other way: the finding is about *where* the limit is and what a client
    // is told when it crosses it, not about malformed input.
    let over = body(&"a".repeat(34_000_000));
    let (status, headers, text) = post(&app, "/v1/messages", &[], &over).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{text}");
    assert!(
        headers
            .get(CONTENT_TYPE)
            .is_some_and(|value| value.as_bytes().starts_with(b"application/json")),
        "the refusal must be served as this dialect's JSON envelope, not axum's \
         raw rejection: content-type was {:?}, body was {text:?}",
        headers.get(CONTENT_TYPE)
    );
    let refusal: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("an error envelope: {error}\n\nraw body: {text}"));
    assert_eq!(refusal["type"], "error", "{refusal}");
    assert_eq!(refusal["error"]["type"], "request_too_large", "{refusal}");
    assert!(
        refusal["error"]["roundhouse_code"].is_string(),
        "every refusal on this path carries roundhouse's own code: {refusal}"
    );

    // Both routes, because the limit is a property of the router rather than of
    // a handler — and because `count_tokens` is the endpoint a client calls to
    // find out whether a body is too big. One that answered a plain-text 413
    // there would send the client to the path that spends money instead.
    let (status, _, text) = post(&app, "/v1/messages/count_tokens", &[], &over).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{text}");
    let refusal: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("an error envelope: {error}\n\nraw body: {text}"));
    assert_eq!(refusal["error"]["type"], "request_too_large", "{refusal}");
}

// ---------------------------------------------------------------------------
// The forwarded seat
// ---------------------------------------------------------------------------

/// **A seat is captured only when the turn key rode the dedicated header.**
///
/// The rule belongs to `ControlPlane::turn_admission` and is shared by every
/// surface; what this asserts is that *this* surface's header set does not
/// disturb it. That is a real question rather than a formality: a Messages
/// request carries `x-claude-code-session-id`, `anthropic-version` and ten
/// `x-stainless-*` headers that no other surface sees, and the capture walks the
/// header map. The property is one-directional and both directions are asserted,
/// because "no seat was captured" is trivially true of an implementation that
/// captures nothing.
#[test]
fn a_seat_rides_only_beside_a_dedicated_turn_key() {
    let plane = ControlPlane::configured(
        ControlPlaneConfig::from_json(
            &json!({
                "projects": [{ "id": "seat", "credentials": { "mode": "pass_through" } }],
                "users": [{ "id": "ada" }],
                "keys": [{
                    "project": "seat", "user": "ada",
                    "key_sha256": sha256_hex(&key("seat")),
                }],
            })
            .to_string(),
            "messages pass-through fixture",
        )
        .expect("the fixture config must validate"),
    );

    let mut with_seat = client_headers();
    with_seat.insert(
        HeaderName::from_static("x-roundhouse-key"),
        HeaderValue::from_str(&key("seat")).expect("a header value"),
    );
    with_seat.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer sk-ant-oat01-not-a-real-seat"),
    );
    let admitted = plane
        .turn_admission(&with_seat)
        .expect("a well-formed turn key is admitted");
    // `reaches` and not `is_forwarding`: the latter is a property of the
    // *project's mode* and is true of every turn under it, captured seat or
    // none. What is being asserted is that a credential was actually taken off
    // this request, which under a forwarding resolution is exactly what makes a
    // hosted provider reachable at all.
    assert!(
        admitted.credentials.reaches("anthropic"),
        "the seat beside a dedicated turn key must be captured"
    );

    // The other direction: the same key in `Authorization` is the roundhouse
    // secret itself, and forwarding it upstream would send our own credential to
    // a provider.
    let mut key_only = client_headers();
    key_only.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", key("seat"))).expect("a header value"),
    );
    let admitted = plane
        .turn_admission(&key_only)
        .expect("a well-formed turn key is admitted");
    assert!(
        !admitted.credentials.reaches("anthropic"),
        "roundhouse's own turn key must never be forwarded as a seat"
    );
}

/// The header set a 2.1.251 inference request actually carries.
///
/// Read off `tests/fixtures/claude-2.1.251-headers.json` in spirit and written
/// out here so the assertion above is about *these* headers rather than about an
/// empty map.
fn client_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("anthropic-version", "2023-06-01"),
        (
            "anthropic-beta",
            "claude-code-20250219,interleaved-thinking-2025-05-14,\
             mid-conversation-system-2026-04-07",
        ),
        ("x-app", "cli"),
        (
            "x-claude-code-session-id",
            "e13acbde-ab70-46ff-b094-fd8ce95d286d",
        ),
        ("x-stainless-lang", "js"),
        ("x-stainless-retry-count", "0"),
        ("user-agent", "claude-cli/2.1.251 (external, sdk-cli)"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    headers
}

// ---------------------------------------------------------------------------
// The captured client bodies
// ---------------------------------------------------------------------------

/// **The shipping client's body canonicalizes, block by block.**
///
/// The whole 84 KB request as 2.1.251 sends it: three system blocks, a
/// two-block user message, a mid-conversation `system` message, twenty-four tool
/// definitions, `context_management`, `thinking`, `output_config`. Everything
/// but `system` and `messages` is accepted and ignored, and the assertion that
/// matters is the one about what a *stored prefix* looks like — because that is
/// what every later turn is checked against.
#[test]
fn the_shipping_clients_body_becomes_the_prefix_it_will_be_checked_against() {
    let params = parse(TURN_ONE);
    let items = canonicalize(&params).expect("the live client's body must be servable");

    assert_eq!(
        items.len(),
        6,
        "three system blocks, two user blocks and the mid-conversation system \
         message: {:#?}",
        items.iter().map(|item| item.role).collect::<Vec<_>>()
    );
    // Block 0 of `system` is the attribution pseudo-header, stored as ordinary
    // prefix with no special case (§5.5 ¶5). It is stable per conversation, so
    // its stability is the client's to keep and a server stripping it would be
    // guessing at which parts of a system prompt matter.
    assert_eq!(items[0].role, Role::Developer);
    assert!(
        matches!(&items[0].content, ItemContent::Text { text }
            if text.starts_with("x-anthropic-billing-header: cc_version=2.1.251")),
        "{:?}",
        items[0].content
    );
    // **The leading run of `system` blocks is turn configuration, and carries
    // `Role::Developer` to say so** (M11.1 review, F7). An interior system
    // message is not: it happened at a position both sides agree on, so it is
    // history and keeps `Role::System`. The split is decided once, here, by
    // position — everything downstream reads the role rather than re-deriving
    // the boundary, because a run of identical-looking system items is not
    // splittable by any later reader.
    assert_eq!(
        items.iter().map(|item| item.role).collect::<Vec<_>>(),
        vec![
            Role::Developer,
            Role::Developer,
            Role::Developer,
            Role::User,
            Role::User,
            // The `mid-conversation-system-2026-04-07` beta's message. Refusing
            // this — which the first reading of this surface did — 422s every
            // request the current client line makes; and treating it as
            // configuration would take a message out of the history both sides
            // are checked against.
            Role::System,
        ]
    );
    // The `cache_control` breakpoints on system blocks 1 and 2 leave no trace:
    // roundhouse places its own from the segment boundaries it knows, and
    // keeping the client's would let it name a prefix boundary in a prompt it
    // does not assemble.
    assert!(
        items
            .iter()
            .all(|item| matches!(item.content, ItemContent::Text { .. })),
        "{items:#?}"
    );
    // The session name the client gave, in the shape it gives it in.
    assert_eq!(
        session_key(&HeaderMap::new(), &params).as_deref(),
        Some("anthropic_messages/e13acbde-ab70-46ff-b094-fd8ce95d286d"),
        "the 2.1.251 `metadata.user_id` is a JSON object string, and the name it \
         yields lives in this dialect's own namespace (F6)"
    );
}

/// **Two real turns of one conversation, and the one item that moved.**
///
/// The `--continue` body resends the whole history, so the first six items ought
/// to be the prefix the session already holds. Five of them are — including the
/// mid-conversation system message, which the client sends as a **one-block list
/// on turn one and as a bare string on the resend**, and which must therefore
/// canonicalize identically or the session forks at item 5 on every second turn.
/// That is the property this fixture pair was captured to prove.
///
/// The sixth is a genuine divergence and it is recorded rather than papered
/// over: the client rebuilt its own system prompt between the two turns (the
/// model-identity line changed as the 1M-context variant dropped out of the beta
/// header), so this pair really does fork. The fork is correct behaviour on a
/// rewritten prompt; what would be wrong is forking for the *other* five items,
/// and that is what the equality below rules out.
#[test]
fn the_shipping_clients_two_turns_are_one_conversation_but_for_the_prompt_it_changed() {
    let first = canonicalize(&parse(TURN_ONE)).expect("turn one is servable");
    let second = canonicalize(&parse(TURN_TWO)).expect("turn two is servable");

    assert_eq!(
        second.len(),
        first.len() + 2,
        "the answer and the new question"
    );
    let diverged: Vec<usize> = first
        .iter()
        .zip(&second)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        diverged,
        vec![2],
        "only the system prompt the client itself rewrote may differ: {diverged:?}"
    );
    assert_eq!(
        first[5], second[5],
        "the mid-conversation system message is a block list on turn one and a \
         string on the resend; if those canonicalize differently, every second \
         turn of every session forks and every turn still answers"
    );
    assert_eq!(
        second[6],
        Item {
            role: Role::Assistant,
            content: ItemContent::Text {
                text: "MOCKED".to_string()
            },
            response_id: None,
        },
        "the client replays the assistant's reply verbatim, unstamped"
    );

    // And both turns name the same session, which is what makes the prefix check
    // reach the same log at all.
    assert_eq!(
        session_key(&HeaderMap::new(), &parse(TURN_ONE)),
        session_key(&HeaderMap::new(), &parse(TURN_TWO)),
    );
}

/// **F7: replaying the two live turns through the running server continues one
/// conversation across ordinary system-prompt volatility.**
///
/// [`the_shipping_clients_two_turns_are_one_conversation_but_for_the_prompt_it_changed`]
/// proves `canonicalize()` disagrees at item 2 only, between two consecutive
/// real turns of what is, from the user's point of view, one `--continue`d
/// conversation. Nothing before this test drove both fixtures through the
/// *running* router to see what `bind_prefix` does with that one-item
/// disagreement — this does. `ScriptedFrontierClient` is primed to answer
/// `"MOCKED"`, exactly the text turn two's own fixture replays verbatim as
/// history, so the *only* disagreement between the two turns really is the one
/// line this test is about (the model-identity line the CLI itself rewrote as
/// `context-1m-2025-08-07` dropped out of the beta header, §5.6 addendum 2) —
/// nothing here rests on the test double answering differently than the live
/// capture rig did.
///
/// **What used to happen, and why it was the finding.** Every item was admitted
/// under one strict rule, so item 2's rewritten line forked the session to a
/// fresh generation: cold routing history and, per `conversations.rs`'s own
/// `fork()` doc, a silently orphaned MCP `scope=session` narrowing — on
/// precisely the turn a warm prefix would first have paid off. The trigger
/// recurs for the life of every session on an unpredictable cadence (the date,
/// cwd, git branch, any beta flag, an overnight client self-update, §5.6), so
/// the warm-prefix thesis this surface exists to serve did not survive contact
/// with the shipping client.
///
/// What happens now: the leading system run is turn configuration, so a resend
/// that rewrote it *replaces* it and continues — while the conversation itself
/// (the two user blocks, the mid-conversation system message, the assistant
/// reply) is still admitted strictly and still forks on a real edit, which is
/// what [`a_divergent_resend_forks_rather_than_merging`] pins.
///
/// The third turn is not decoration. It is what proves the replacement is a
/// *stable* projection rather than a one-off tolerance: turn three is checked
/// against a session whose configuration run was rewritten in place, and it has
/// to agree with it.
#[tokio::test]
async fn f7_the_live_continue_pair_continues_across_ordinary_system_volatility() {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ScriptedFrontierClient::new("MOCKED"));
    let app = messages_router(
        ControlPlane::open(),
        engine_scripted(Arc::clone(&store), Arc::clone(&client)),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );

    let mut turn_one: Value = serde_json::from_str(TURN_ONE).expect("the fixture is JSON");
    turn_one["stream"] = json!(true);
    stream(&app, &[], &turn_one).await;

    const SESSION: &str = "anthropic_messages/e13acbde-ab70-46ff-b094-fd8ce95d286d";
    let after_turn_one = stored_items(&store, SESSION).await;
    assert_eq!(
        after_turn_one.len(),
        7,
        "turn one's own six canonicalized items plus its answer: {after_turn_one:#?}"
    );

    let mut turn_two: Value = serde_json::from_str(TURN_TWO).expect("the fixture is JSON");
    turn_two["stream"] = json!(true);
    stream(&app, &[], &turn_two).await;

    // What the product's own value proposition requires: a `--continue` naming
    // the same session id appends its new question and answer to the
    // conversation the user believes is one conversation.
    //
    // Twelve raw appends, not nine: the three-block configuration run is
    // *re-recorded* because it changed, and the log is append-only — the
    // replacement happens in the projection, not by rewriting what was
    // committed. The control below, where nothing about the configuration
    // moved, records nothing extra and lands on nine.
    let continued = stored_items(&store, SESSION).await;
    assert_eq!(
        continued.len(),
        12,
        "turn two must extend the session it named, not silently orphan it: \
         {continued:#?}"
    );
    assert!(
        no_such_session(&store, &format!("{SESSION}#g1")).await,
        "and it must not have landed in a freshly forked generation instead"
    );

    // Turn three: the client's own next `--continue`, carrying turn two's
    // configuration unchanged, the answer it was just given, and a new
    // question. It is checked against a session whose configuration was
    // replaced in place, which is the property a one-turn test cannot see.
    let mut turn_three = turn_two.clone();
    let messages = turn_three["messages"]
        .as_array_mut()
        .expect("the fixture's `messages` is a list");
    messages
        .push(json!({ "role": "assistant", "content": [{ "type": "text", "text": "MOCKED" }] }));
    messages.push(json!({ "role": "user", "content": "and what about the other one?" }));
    stream(&app, &[], &turn_three).await;

    let after_turn_three = stored_items(&store, SESSION).await;
    assert_eq!(
        after_turn_three.len(),
        14,
        "the new question and its answer, and nothing re-recorded: turn three's \
         configuration is the one already stored: {after_turn_three:#?}"
    );
    assert!(
        no_such_session(&store, &format!("{SESSION}#g1")).await,
        "a replaced configuration run must be a stable projection, not a \
         tolerance that expires on the next turn"
    );

    // And the replacement is a replacement: what the session holds at the head
    // is turn two's rewritten block, exactly once.
    let rewritten = turn_two["system"][2]["text"]
        .as_str()
        .expect("the fixture's system block 2 is text");
    let superseded = turn_one["system"][2]["text"]
        .as_str()
        .expect("the fixture's system block 2 is text");
    assert_ne!(rewritten, superseded, "control: the fixtures still differ");
}

/// CONTROL for the F7 probe above: neutralize the one line that differs
/// between the two live captures (patch turn two's `system[2]` back to turn
/// one's own text, undoing exactly the `context-1m-2025-08-07` beta drift)
/// and the same replay must NOT fork.
///
/// What this rules out: that the probe above fails because of something else
/// in the harness — a stray whitespace byte in how `serde_json` round-trips
/// the fixture, the `ScriptedFrontierClient` double, the router wiring — and
/// not because of the one line the probe's doc names. If this control ever
/// starts failing too, the probe's failure has stopped being about system-
/// prompt volatility specifically and the F7 finding needs re-reading before
/// anyone trusts it.
#[tokio::test]
async fn f7_control_the_same_pair_does_not_fork_once_the_one_line_is_neutralized() {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ScriptedFrontierClient::new("MOCKED"));
    let app = messages_router(
        ControlPlane::open(),
        engine_scripted(Arc::clone(&store), Arc::clone(&client)),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );

    let mut turn_one: Value = serde_json::from_str(TURN_ONE).expect("the fixture is JSON");
    turn_one["stream"] = json!(true);
    stream(&app, &[], &turn_one).await;

    let mut turn_two: Value = serde_json::from_str(TURN_TWO).expect("the fixture is JSON");
    turn_two["stream"] = json!(true);
    // The only edit: item 2 of `system` reset to turn one's own words, so
    // every byte `suffix_after` compares now agrees.
    turn_two["system"][2]["text"] = turn_one["system"][2]["text"].clone();
    stream(&app, &[], &turn_two).await;

    let continued = stored_items(&store, &named("e13acbde-ab70-46ff-b094-fd8ce95d286d")).await;
    assert_eq!(
        continued.len(),
        9,
        "with the one volatile line neutralized the configuration run is the \
         one already stored, so nothing is re-recorded and the session grows by \
         exactly the new question and its answer: {continued:#?}"
    );
}

/// The captured body, served — not just parsed.
///
/// The unit above proves canonicalization; this proves the whole path handles
/// 84 KB of real request, including twenty-four tool definitions this surface
/// ignores and a `thinking` object whose shape changed between 2.1.247 and
/// 2.1.251 (`budget_tokens` became `{"type":"adaptive"}`). An accepted-and-
/// ignored field is only accepted if a request carrying it is answered.
#[tokio::test]
async fn the_captured_client_body_is_served_as_a_conformant_stream() {
    let (app, store) = surface();
    let mut body: Value = serde_json::from_str(TURN_ONE).expect("the fixture is JSON");
    body["stream"] = json!(true);

    let accumulated = stream(&app, &[], &body).await;
    assert_eq!(accumulated.text, ANSWER);
    assert_eq!(accumulated.model, "claude-opus-5");

    let items = stored_items(&store, &named("e13acbde-ab70-46ff-b094-fd8ce95d286d")).await;
    assert_eq!(
        items.len(),
        7,
        "the six canonicalized items plus the answer: {:#?}",
        items.iter().map(|item| item.role).collect::<Vec<_>>()
    );
}

/// CONTROL for F4, live: the captured body really does carry a real system
/// prompt past the attribution header, so the probe below is not vacuously
/// checking a session with no instructions to lose. Item 1 is the agent-SDK
/// identity line and item 2 is the actual multi-KB system prompt — both
/// Developer-role Text (the leading system run is turn configuration, F7), i.e.
/// both shapes `instructions_of` (`validate/brief.rs`) accepts, and both
/// textually distinct from the attribution header at item 0. If this test ever
/// fails, the fixture changed shape and F4's probe needs re-deriving, not just
/// re-running.
#[test]
fn f4_control_the_captured_body_carries_a_real_system_prompt_past_the_header() {
    let items = canonicalize(&parse(TURN_ONE)).expect("the live client's body must be servable");

    assert_eq!(items[1].role, Role::Developer);
    assert!(
        matches!(&items[1].content, ItemContent::Text { text }
            if text.contains("Claude Agent SDK")),
        "control: item 1 should be the agent-SDK identity line: {:?}",
        items[1].content
    );
    assert_eq!(items[2].role, Role::Developer);
    assert!(
        matches!(&items[2].content, ItemContent::Text { text }
            if text.contains("interactive agent that helps users with software engineering")),
        "control: item 2 should be the real system prompt: {:?}",
        items[2].content
    );
}

/// **F4: the judge is briefed on the whole instruction block, not on its first
/// block.**
///
/// The finding: `instructions_of` (`validate/brief.rs`) took the *first*
/// system/developer text item as "the session's instructions", which for every
/// Claude Code Messages session is the ~70-byte billing attribution
/// pseudo-header — so every drift, no-progress and steer verdict was decided
/// against billing metadata rather than against the task.
///
/// The fix is client-agnostic on purpose: the leading run is concatenated
/// oldest first and the pre-existing instruction budget does the bounding.
/// Nothing here knows what an attribution header looks like, because a rule
/// that recognised one would break the next time the client re-orders its
/// blocks or another client ships a different preamble. So this test asserts
/// *what the judge can now see* — the real prompt — rather than the absence of
/// the header, which the honest fix does not remove: the header is genuinely
/// part of what the client sent, it is small, and it costs the budget almost
/// nothing.
///
/// Driven through the real `wire::canonicalize` on the real captured 2.1.251
/// body and the real `ValidationBrief::build`, matching
/// `validate::mod::consult`'s call shape exactly (same items, same
/// `Objective::from_items`, same `BriefConfig::default()`), because the
/// finding was about those two functions meeting.
#[test]
fn f4_the_judge_is_briefed_on_the_whole_leading_instruction_run() {
    let items = canonicalize(&parse(TURN_ONE)).expect("the live client's body must be servable");

    let brief = ValidationBrief::build(
        &items,
        Objective::from_items(&items),
        Vec::new(),
        BriefConfig::default(),
    );

    let instructions = brief
        .instructions
        .as_deref()
        .expect("a session with three leading instruction items must produce some instructions");

    assert!(
        instructions.contains("interactive agent that helps users with software engineering"),
        "F4: the judge's `instructions` must reach the real system prompt at item 2 and not \
         stop at the billing attribution header — every drift/steer verdict for this session \
         is otherwise judged against billing metadata, not the task: {instructions:?}"
    );
    // And the run really is a run: the identity line between the header and the
    // prompt is carried too, in the order the client sent it. A fix that
    // skipped to the "real" block would pass the assertion above and still be
    // guessing at which parts of a system prompt matter.
    assert!(
        instructions.contains("Claude Agent SDK"),
        "the whole leading run, oldest first: {instructions:?}"
    );
    // Bounded, as it always was: the budget truncates rather than the reader
    // choosing one block.
    assert_eq!(
        instructions.chars().count(),
        BriefConfig::default().instruction_chars,
        "a multi-KB system prompt still leaves the brief on its existing budget"
    );

    // The mid-conversation system message is *history*, not instructions, and
    // it must not be dragged into the block the judge reads as the task. This
    // is the same boundary prefix admission draws (F7), asserted here so the
    // two cannot drift apart silently.
    let mid_conversation = items
        .last()
        .expect("the fixture ends with the mid-conversation system message");
    assert_eq!(mid_conversation.role, Role::System);
    let ItemContent::Text { text } = &mid_conversation.content else {
        panic!("control: the mid-conversation message is text: {mid_conversation:?}");
    };
    assert!(
        !instructions.contains(text.trim()),
        "an interior system message is history and must stay out of the instructions"
    );
}

// ---------------------------------------------------------------------------
// The oracle's own proofs
// ---------------------------------------------------------------------------

/// A well-formed stream, as SSE text.
fn conformant() -> Vec<(&'static str, Value)> {
    vec![
        (
            "message_start",
            json!({ "type": "message_start", "message": {
                "type": "message", "id": "resp_1", "role": "assistant",
                "model": "claude-opus-5", "content": [],
                "usage": { "input_tokens": 900, "output_tokens": 1 },
            }}),
        ),
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0,
                    "content_block": { "type": "text", "text": "" } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": "hi" } }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta",
            json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 7 } }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]
}

fn sse(frames: &[(&str, Value)]) -> String {
    frames
        .iter()
        .map(|(name, payload)| format!("event: {name}\ndata: {payload}\n\n"))
        .collect()
}

/// **The oracle rejects each defect it exists to catch.**
///
/// Without this, every green assertion above is compatible with an oracle that
/// accepts anything — which is the failure mode of a hand-written validator and
/// the reason the Responses surface borrows Codex's parser instead. Each case
/// below is one line changed from [`conformant`], and each is a defect that
/// costs something specific: a re-issued turn at full price, a thrown
/// accumulator, or a turn billed as free.
#[test]
fn the_oracle_is_not_a_rubber_stamp() {
    // CONTROL first: the unmodified stream passes, so every rejection below is
    // about the one thing that changed.
    let good = audit(&sse(&conformant())).expect("the conformant stream must pass");
    assert_eq!(good.text, "hi");
    assert_eq!(good.usage.input_tokens, 900);
    assert_eq!(
        good.usage.output_tokens, 7,
        "the terminal frame's count replaces the prelude's `1`"
    );

    // A frame with no `event:` line. Claude Code dispatches on the name, so this
    // frame is dropped in silence and the whole turn is re-issued non-streaming.
    let nameless = sse(&conformant()).replace("event: content_block_delta\n", "");
    assert!(
        audit(&nameless).is_err(),
        "a frame with no `event:` line must be refused"
    );

    // A `data:`-less frame — alive on a direct connection, discarded by a
    // chained Relay's re-encoder.
    let dataless = format!("{}event: ping\n\n", sse(&conformant()));
    assert!(audit(&dataless).is_err(), "a frame with no `data:` line");

    // The name and the payload disagreeing. Two readers then understand one
    // frame differently: the client believes the line, our own dispatch decoder
    // believes the payload.
    let mismatched = sse(&conformant()).replace("event: message_stop", "event: message_delta");
    assert!(audit(&mismatched).is_err(), "a lying `event:` line");

    // A delta at an index nothing opened: `RangeError("Content block not
    // found")`, a thrown accumulator rather than a dropped frame.
    let mut orphaned = conformant();
    orphaned.remove(1);
    assert!(audit(&sse(&orphaned)).is_err(), "a delta with no start");

    // The most expensive frame this surface could emit. The client merges
    // `output_tokens` with `??`, so an explicit zero overwrites a real count and
    // the turn bills as free.
    let free = sse(&conformant()).replace("\"output_tokens\":7", "\"output_tokens\":0");
    assert!(
        audit(&free).is_err(),
        "an explicit `output_tokens: 0` in a `message_delta`"
    );

    // A `stop_reason` outside the pinned seven.
    let invented = sse(&conformant()).replace("end_turn", "finished_normally");
    assert!(audit(&invented).is_err(), "a stop reason the spec lacks");

    // A usage property nobody publishes — the `adk-anthropic` defect, which
    // reported a cache counter to nobody for a year.
    let extra = sse(&conformant()).replace(
        "\"output_tokens\":7",
        "\"output_tokens\":7,\"cache_creation_input_tokens_1h\":5",
    );
    assert!(audit(&extra).is_err(), "an invented usage property");

    // A stream that completes no content block: the second of the two
    // non-streaming-fallback triggers, and a full extra turn's cost.
    let blockless: Vec<(&str, Value)> = conformant()
        .into_iter()
        .filter(|(name, _)| !name.starts_with("content_block"))
        .collect();
    assert!(
        audit(&sse(&blockless)).is_err(),
        "a stream with no completed content block"
    );

    // Anything after the terminal frame.
    let trailing = format!(
        "{}event: message_delta\ndata: {}\n\n",
        sse(&conformant()),
        json!({ "type": "message_delta", "delta": {} })
    );
    assert!(audit(&trailing).is_err(), "a frame after `message_stop`");

    // A delta whose type disagrees with its block's: `Error("Content block is
    // not a text block")`.
    // (`serde_json` renders object keys in sorted order, so `text` precedes
    // `type` — the substring is written the way the fixture actually serializes
    // rather than the way it is spelled above.)
    let crossed = sse(&conformant()).replace(
        "\"text\":\"hi\",\"type\":\"text_delta\"",
        "\"thinking\":\"hm\",\"type\":\"thinking_delta\"",
    );
    assert_ne!(
        crossed,
        sse(&conformant()),
        "the mutation must have applied"
    );
    assert!(audit(&crossed).is_err(), "a thinking delta on a text block");

    // And two things that must *not* be refused, or the oracle is simply strict
    // rather than correct: a `ping` before the prelude, and an `error` event as
    // a stream's only terminal.
    let mut with_ping = vec![("ping", json!({ "type": "ping" }))];
    with_ping.extend(conformant());
    audit(&sse(&with_ping)).expect("a ping is legal anywhere, including first");
    let failed = vec![
        conformant()[0].clone(),
        (
            "error",
            json!({ "type": "error", "error": {
                "type": "overloaded_error", "message": "try again" } }),
        ),
    ];
    let failed = audit(&sse(&failed)).expect("an error event is a legal terminal on its own");
    assert!(failed.error.is_some());
}

// ---------------------------------------------------------------------------
// A real socket
// ---------------------------------------------------------------------------

/// The same turn over a real connection, with a real chunked body.
///
/// Everything above drives the router as a `tower::Service`, which is enough for
/// the protocol and keeps the tests hermetic — but it skips the parts a socket
/// does not: the chunked transfer encoding, the response headers axum's `Sse`
/// sets, and the fact that the frames arrive as bytes rather than as a
/// pre-assembled body. The client is written by hand here rather than pulled in,
/// because the one thing a borrowed HTTP client would hide is exactly what this
/// test is for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_socket_round_trip() {
    let (app, _store) = surface();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let payload = serde_json::to_vec(&body("hello")).expect("a JSON body");
    let mut socket = tokio::net::TcpStream::connect(addr)
        .await
        .expect("the server is listening");
    // The path the client actually posts to, query and all.
    let request = format!(
        "POST /v1/messages?beta=true HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    socket
        .write_all(request.as_bytes())
        .await
        .expect("write headers");
    socket.write_all(&payload).await.expect("write body");

    let mut raw = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        socket.read_to_end(&mut raw),
    )
    .await
    .expect("the stream must end rather than hang")
    .expect("read");
    let raw = String::from_utf8(raw).expect("HTTP/1.1 responses here are UTF-8");

    let (head, body) = raw.split_once("\r\n\r\n").expect("a complete response");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.to_ascii_lowercase().contains("text/event-stream"),
        "the content type is what makes a client stream rather than buffer: {head}"
    );
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)
    } else {
        body.to_string()
    };

    let accumulated = audit(&body)
        .unwrap_or_else(|error| panic!("the socket stream is not conformant: {error}\n\n{body}"));
    assert_eq!(accumulated.text, ANSWER);
    // Every frame carried both lines. Asserted here as well as inside the oracle
    // because this is the only path where the framing is produced by the real
    // encoder rather than by a collected body.
    assert!(
        split_frames(&body)
            .iter()
            .all(|frame| frame.name.is_some() && frame.data.is_some()),
        "{body}"
    );
}

/// Undo HTTP/1.1 chunked transfer encoding.
///
/// Written out rather than pulled in for the reason the test above gives: the
/// chunk framing is part of what is under test, so decoding it with the same
/// library that produced it would be a tautology.
fn dechunk(body: &str) -> String {
    let mut rest = body;
    let mut out = String::new();
    while let Some((header, tail)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(header.trim(), 16).expect("a chunk size in hex");
        if size == 0 {
            break;
        }
        out.push_str(&tail[..size]);
        rest = &tail[size + 2..];
    }
    out
}
