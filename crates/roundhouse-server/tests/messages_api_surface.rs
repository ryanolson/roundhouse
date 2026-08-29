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

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
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
use roundhouse_fleet::EchoFrontierClient;
use roundhouse_server::messages_api::wire::{CreateMessageParams, canonicalize, session_key};
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, Conversations, EchoLocalExecutor, Engine, messages_router,
};

mod common;
use common::anthropic::{Accumulated, StrictErrorKind, audit, split_frames};
use common::{config, frontier_catalog, key, sha256_hex};

/// What the echo provider answers with, and therefore what a turn says.
const ANSWER: &str = "frontier answer";

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

    let items = stored_items(&store, "sess-two-turns").await;
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

    let original = stored_items(&store, "sess-fork").await;
    let forked = stored_items(&store, "sess-fork#g1").await;
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

    let items = stored_items(&store, "sess-retry").await;
    assert_eq!(
        items
            .iter()
            .filter(|item| item.role == Role::Assistant)
            .count(),
        1,
        "one answer, not two: {items:#?}"
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

    let items = stored_items(&store, session).await;
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
    assert_eq!(items[0].role, Role::System);
    assert!(
        matches!(&items[0].content, ItemContent::Text { text }
            if text.starts_with("x-anthropic-billing-header: cc_version=2.1.251")),
        "{:?}",
        items[0].content
    );
    assert_eq!(
        items.iter().map(|item| item.role).collect::<Vec<_>>(),
        vec![
            Role::System,
            Role::System,
            Role::System,
            Role::User,
            Role::User,
            // The `mid-conversation-system-2026-04-07` beta's message. Refusing
            // this — which the first reading of this surface did — 422s every
            // request the current client line makes.
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
        Some("e13acbde-ab70-46ff-b094-fd8ce95d286d"),
        "the 2.1.251 `metadata.user_id` is a JSON object string"
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

    let items = stored_items(&store, "e13acbde-ab70-46ff-b094-fd8ce95d286d").await;
    assert_eq!(
        items.len(),
        7,
        "the six canonicalized items plus the answer: {:#?}",
        items.iter().map(|item| item.role).collect::<Vec<_>>()
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
