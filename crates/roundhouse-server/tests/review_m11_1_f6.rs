// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! F6 (M11.1 thermo-nuclear review), fixed and pinned here. The finding:
//! `session_key` (`messages_api/wire.rs`) and `prompt_cache_key`
//! (`responses_api.rs`) both fed the same `bind_prefix` ->
//! `ControlPlane::qualify` -> `Conversations` pipeline with no dimension for
//! *which dialect* is asking, on top of `qualify`'s principal namespace. So a
//! Messages client and a Responses client under one principal that happened to
//! name a conversation with the same raw string did not get two conversations
//! -- they got one contested one, and because their histories were never
//! actually the same conversation, every alternating turn looked like a
//! divergent resend and forked.
//!
//! The ruling: **cross-dialect continuation is not a feature.** Every name the
//! Messages surface derives lives in that dialect's own namespace, so the two
//! never collide; and when `x-claude-code-agent-id` is present it joins the
//! name, so a Task-tool subagent that inherits its parent's session id gets a
//! sibling session rather than interleaving with the parent on one log. The
//! shared `turn_id_for` deliberately stays shared -- a turn id is a content
//! hash, and two dialects hashing one conversation differently would each be
//! idempotent alone and neither across a chained deployment.
//!
//! No suite in this crate mounts `messages_router` and
//! `responses_api::responses_router` together (`messages_api_surface.rs`
//! only ever builds the former; `tenancy_attribution.rs` builds the latter
//! plus `http::router`, never `messages_router`), so the seam this finding
//! is about had no test standing in it either way -- which is why this file
//! builds the composition `main::serve` actually performs.
//!
//! The subagent half is pinned here as a *server-side* property: given the
//! header, the two agents get two sessions. Whether the shipping client sends
//! it on a Task-tool request is a claim about upstream behaviour this crate
//! cannot settle -- `claude-code-client-surface.md` sources the subagent path
//! only from a v2.1.42 static read (§4.3), and both live captures (§5.5, §5.6)
//! were single-agent. What is inside roundhouse's control is that the name is
//! *ready* for it, and that a deployment whose clients never send the header
//! sees exactly the names it saw before.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use codex_api::{ResponseEvent, ResponsesClient, ResponsesOptions};
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::Principal;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::Item;
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::EchoFrontierClient;
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, Conversations, EchoLocalExecutor, Engine, EngineConfig,
    messages_router, responses_api,
};

mod common;
use common::codex::{RouterTransport, StaticToken, collect, provider, request, user_message};
use common::{frontier_catalog, sha256_hex};

/// What the echo provider answers with on both surfaces -- one engine, one
/// client, so a difference in stored content can only come from the request.
const ANSWER: &str = "frontier answer";

fn acme_key() -> String {
    format!("rh_turn_{:A<43}", "acme")
}

fn acme() -> Principal {
    Principal::new("acme", "ada")
}

/// One project, one user, one turn key -- deliberately *not*
/// `tenancy_attribution.rs`'s two-tenant fixture. F6 is about a collision
/// *within* one principal's own namespace, which a second tenant would not
/// exercise.
fn configured() -> Arc<ControlPlane> {
    let json = json!({
        "projects": [{ "id": "acme" }],
        "users": [{ "id": "ada" }],
        "keys": [
            { "project": "acme", "user": "ada", "key_sha256": sha256_hex(&acme_key()) },
        ],
    })
    .to_string();
    let config = ControlPlaneConfig::from_json(&json, "f6 fixture")
        .expect("the fixture config must validate");
    Arc::new(ControlPlane::configured(config))
}

/// Both dialects, one engine, one store, one [`Conversations`] -- the exact
/// composition `main::serve` performs, and the one no existing suite builds.
fn deployment() -> (Router, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new(ANSWER)),
        Arc::new(AffinityPolicy::new()),
        EngineConfig::default(),
    ));
    let plane = configured();
    let conversations = Arc::new(Conversations::new());
    let app = messages_router(
        Arc::clone(&plane),
        Arc::clone(&engine),
        Arc::clone(&store),
        Arc::clone(&conversations),
    )
    .merge(responses_api::responses_router(
        plane,
        engine,
        Arc::clone(&store),
        conversations,
    ));
    (app, store)
}

/// One non-streaming Messages turn, authenticated as `acme`, naming the
/// session with `x-claude-code-session-id`. Non-streaming so the response
/// body is one JSON object rather than SSE frames this file has no reason to
/// parse -- the module doc's "folded from the same frames the stream would
/// emit" is what makes that a faithful stand-in.
async fn messages_turn(app: &Router, session_id: &str, text: &str) -> StatusCode {
    messages_turn_as(app, session_id, None, text).await
}

/// The same turn, optionally identifying the Task-tool subagent that made it.
async fn messages_turn_as(
    app: &Router,
    session_id: &str,
    agent_id: Option<&str>,
    text: &str,
) -> StatusCode {
    let body = json!({
        "model": "claude-opus-5",
        "max_tokens": 1024,
        "stream": false,
        "messages": [{ "role": "user", "content": text }],
    });
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, format!("Bearer {}", acme_key()))
        .header(
            HeaderName::from_static("x-claude-code-session-id"),
            HeaderValue::from_str(session_id).expect("a valid header value"),
        );
    if let Some(agent_id) = agent_id {
        builder = builder.header(
            HeaderName::from_static("x-claude-code-agent-id"),
            HeaderValue::from_str(agent_id).expect("a valid header value"),
        );
    }
    let request = builder
        .body(Body::from(serde_json::to_vec(&body).expect("a JSON body")))
        .expect("a well-formed request");
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    // Drained rather than dropped: `complete_message` has already written the
    // turn to the store by the time it responds, but a response body dropped
    // mid-read is not a guarantee this crate's own `Response` type makes, and
    // this file would rather depend on nothing it did not check.
    let _ = response
        .into_body()
        .collect()
        .await
        .expect("a readable body");
    status
}

/// One Responses turn, driven through Codex's own client -- the same
/// transport double `tenancy_attribution.rs` uses -- with `prompt_cache_key`
/// set to the *same raw string* a Messages client would name a session with.
async fn responses_turn(app: &Router, cache_key: &str, text: &str) -> Vec<ResponseEvent> {
    let client = ResponsesClient::new(
        RouterTransport { app: app.clone() },
        provider("http://roundhouse.test/v1", "roundhouse-f6"),
        Arc::new(StaticToken::new(acme_key())),
    );
    collect(
        client
            .stream_request(
                request(cache_key, vec![user_message(text)]),
                ResponsesOptions::default(),
            )
            .await
            .expect("the turn dispatches"),
    )
    .await
    .expect("the turn completes")
}

async fn items(store: &MemoryStore, session_id: &str) -> Vec<Item> {
    store
        .read_events(&SessionId::new(session_id), 0, 1024)
        .await
        .unwrap_or_else(|error| panic!("session `{session_id}` should exist: {error}"))
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ItemAppended { item } => Some(item),
            _ => None,
        })
        .collect()
}

async fn no_such_session(store: &MemoryStore, session_id: &str) -> bool {
    store.last_seq(&SessionId::new(session_id)).await.is_err()
}

fn rendered(items: &[Item]) -> String {
    items.iter().map(|item| item.content.render()).collect()
}

/// The session id a Messages turn under `acme/ada` actually lands in.
///
/// Two scopes on top of the principal's, and this file exists to pin both: the
/// dialect (so a Responses `prompt_cache_key` reading the same string is a
/// different conversation) and, when the client identifies one, the agent (so a
/// Task-tool subagent inheriting its parent's session id is a sibling rather
/// than an interleaving co-writer).
fn messages_session(session_id: &str, agent_id: Option<&str>) -> String {
    let prefix = acme().namespace_prefix();
    match agent_id {
        Some(agent) => format!("{prefix}anthropic_messages/{session_id}/agent/{agent}"),
        None => format!("{prefix}anthropic_messages/{session_id}"),
    }
}

/// The session id a Responses turn naming `cache_key` lands in — unchanged by
/// this fix, which is half the point of scoping the *other* dialect's names.
fn responses_session(cache_key: &str) -> String {
    format!("{}{cache_key}", acme().namespace_prefix())
}

// ---------------------------------------------------------------------------
// Control
// ---------------------------------------------------------------------------

/// Control: two Messages-only turns on one key, the second a genuine resend
/// of the first plus a new question, stay in the *same* session.
///
/// Kept live so the fork observed below cannot be dismissed as an artefact of
/// this file's merged-router harness (a `Conversations` not actually shared,
/// a store not actually shared, a fixture that forks on every second turn
/// regardless of content). It pins the harness's own correctness: one
/// dialect, one real continuation, no fork -- the same property
/// `a_resent_history_is_admitted_as_a_prefix_and_not_appended_twice`
/// (`messages_api_surface.rs`) pins for the plain `messages_router` case,
/// re-proved here over the composition F6 is actually about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f6_control_two_messages_turns_under_one_key_do_not_fork() {
    let (app, store) = deployment();

    let status = messages_turn(&app, "control-key", "hello").await;
    assert_eq!(status, StatusCode::OK, "turn one must be served");

    // Exactly what the client resends next: what it had, plus the answer it
    // was just given, plus the new question -- `same_item` compares role and
    // content, so this must agree with what was actually stored.
    let grown = json!({
        "model": "claude-opus-5",
        "max_tokens": 1024,
        "stream": false,
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": [{ "type": "text", "text": ANSWER }] },
            { "role": "user", "content": "and again" },
        ],
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, format!("Bearer {}", acme_key()))
        .header("x-claude-code-session-id", "control-key")
        .body(Body::from(serde_json::to_vec(&grown).expect("json")))
        .expect("a well-formed request");
    let response = app.clone().oneshot(request).await.expect("router answers");
    assert_eq!(response.status(), StatusCode::OK);

    let session = messages_session("control-key", None);
    let stored = items(&store, &session).await;
    assert!(
        stored.iter().any(|item| *item == Item::user_text("hello")),
        "the resent history must not be re-appended: {stored:#?}"
    );
    assert!(
        stored
            .iter()
            .any(|item| *item == Item::user_text("and again")),
        "the second question must land in the same session: {stored:#?}"
    );
    assert!(
        no_such_session(&store, &format!("{session}#g1")).await,
        "a genuine resend must never fork"
    );
}

// ---------------------------------------------------------------------------
// F6
// ---------------------------------------------------------------------------

/// **The collision that motivated F6, pinned:** one principal (`acme/ada`), one
/// raw cache-key string (`"shared-key"`), three turns alternating Messages /
/// Responses / Messages -- three genuinely different, unrelated conversations
/// that merely happen to share a name.
///
/// **What used to happen.** `qualify` added only the principal's `acme/ada/`
/// prefix (`control_config/mod.rs::qualify`), identically for both callers
/// (`messages_api.rs`'s `cache_key = session_key(...)` and
/// `responses_api.rs`'s `cache_key = &request.prompt_cache_key`), so all three
/// turns resolved the *same* key into `bind_prefix`. Turn two's claimed history
/// disagreed with what turn one had stored at the one item they overlapped on,
/// as any two unrelated conversations will, so `bind_prefix` forked to
/// `acme/ada/shared-key#g1`; turn three then read *that* generation's content
/// as its own prefix, disagreed with it too, and forked again to `#g2`. Three
/// turns, three sessions, one nominal conversation -- and per
/// `Conversations::commit`'s own doc each such move drops `ControlStore`'s
/// overlay, intent, steer-payload and session-binding records for the
/// generation it left, so an agent that narrowed a session's routing over MCP
/// silently
/// stopped being narrowed the moment the *other* dialect took a turn.
///
/// What is asserted now: the two dialects' names never meet, so each
/// conversation is whole and neither forks. The Responses session is checked to
/// still be exactly where it always was, because a fix that moved *its* names
/// would have relocated every session an existing deployment holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f6_alternating_dialects_under_one_name_are_two_conversations() {
    ensure_rustls_crypto_provider();
    let (app, store) = deployment();
    let messages = messages_session("shared-key", None);
    let responses = responses_session("shared-key");
    assert_ne!(
        messages, responses,
        "the whole fix in one line: one raw name, two dialects, two sessions"
    );

    let status = messages_turn(&app, "shared-key", "messages dialect turn one").await;
    assert_eq!(status, StatusCode::OK, "turn one (Messages) must be served");

    let events = responses_turn(&app, "shared-key", "responses dialect turn one").await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ResponseEvent::Completed { .. })),
        "turn two (Responses) must complete: {events:#?}"
    );

    // Turn three is the Messages client's *genuine* continuation — its own
    // history, the answer it was given, and a new question — because the
    // finding is about a conversation that should continue and did not. A
    // bare new question would fork on its own merits (it claims a history the
    // session does not have), and would prove nothing about the collision.
    let grown = json!({
        "model": "claude-opus-5",
        "max_tokens": 1024,
        "stream": false,
        "messages": [
            { "role": "user", "content": "messages dialect turn one" },
            { "role": "assistant", "content": [{ "type": "text", "text": ANSWER }] },
            { "role": "user", "content": "messages dialect turn two" },
        ],
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, format!("Bearer {}", acme_key()))
        .header("x-claude-code-session-id", "shared-key")
        .body(Body::from(serde_json::to_vec(&grown).expect("a JSON body")))
        .expect("a well-formed request");
    let response = app.clone().oneshot(request).await.expect("router answers");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "turn three (Messages) must be served"
    );
    let _ = response
        .into_body()
        .collect()
        .await
        .expect("a readable body");

    // Diagnostics gathered before the assertions, so a failure message can show
    // exactly what landed where rather than only that something did.
    let messages_text = rendered(&items(&store, &messages).await);
    let responses_text = rendered(&items(&store, &responses).await);

    assert!(
        messages_text.contains("messages dialect turn one")
            && messages_text.contains("messages dialect turn two"),
        "both Messages turns belong to one conversation: {messages_text:?}"
    );
    assert!(
        !messages_text.contains("responses dialect turn one"),
        "and the other dialect's turn is not in it: {messages_text:?}"
    );
    assert!(
        responses_text.contains("responses dialect turn one"),
        "the Responses turn keeps the session name it has always had, \
         `{responses}`: {responses_text:?}"
    );

    for base in [&messages, &responses] {
        assert!(
            no_such_session(&store, &format!("{base}#g1")).await,
            "no generation of `{base}` may be minted by a *different dialect* taking a \
             turn: that is not an edited conversation, it is two conversations"
        );
    }
}

/// **A Task-tool subagent gets a sibling session, not its parent's log.**
///
/// The other half of F6. A subagent runs inside the parent's process and the
/// client-surface read has it inheriting the parent's session id, so without a
/// second dimension the two interleave their turns on one log -- and since
/// neither one's resent history contains the other's items, every alternating
/// turn diverges and forks, exactly as the cross-dialect collision above did.
///
/// Three properties are pinned, and the third is why this is not just "add a
/// header to the key": the parent's own name is *unchanged* when the header is
/// absent, so a deployment whose clients never send it sees the names it always
/// saw. Two different subagents under one parent also get one session each,
/// which is what makes a subagent's own second turn a continuation rather than
/// a fresh cold start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f6_a_subagent_sharing_the_parents_session_id_gets_a_sibling_session() {
    let (app, store) = deployment();
    const SHARED: &str = "parent-session";

    assert_eq!(
        messages_turn(&app, SHARED, "the parent's own question").await,
        StatusCode::OK
    );
    assert_eq!(
        messages_turn_as(&app, SHARED, Some("agent-7"), "the subagent's question").await,
        StatusCode::OK
    );
    assert_eq!(
        messages_turn_as(
            &app,
            SHARED,
            Some("agent-8"),
            "a second subagent's question"
        )
        .await,
        StatusCode::OK
    );

    let parent = rendered(&items(&store, &messages_session(SHARED, None)).await);
    let seven = rendered(&items(&store, &messages_session(SHARED, Some("agent-7"))).await);
    let eight = rendered(&items(&store, &messages_session(SHARED, Some("agent-8"))).await);

    assert!(
        parent.contains("the parent's own question") && !parent.contains("the subagent's question"),
        "the parent's log must hold only the parent's turns: {parent:?}"
    );
    assert!(
        seven.contains("the subagent's question")
            && !seven.contains("a second subagent's question"),
        "each subagent gets its own conversation: {seven:?}"
    );
    assert!(
        eight.contains("a second subagent's question"),
        "including the second one: {eight:?}"
    );
    assert!(
        no_such_session(&store, &format!("{}#g1", messages_session(SHARED, None))).await,
        "and nobody forks: interleaving was the only reason they would have"
    );
}
