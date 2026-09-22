// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract: real prefix admission, not a hand-built `PromptCapture`, is what
//! keeps old history and system instructions out of the classifier's HTTP
//! body -- on both dialects, for both the shapes a resent history can end in,
//! and for both the ways a turn's input can be the whole claimed conversation
//! rather than a delta.
//!
//! `PromptCapture::of` finds its boundary -- the last assistant item -- inside
//! whatever `input` the engine's `run_turn` receives, and `run_turn` receives
//! exactly what `prefix_admission::bind_prefix` decided: a delta for an
//! ordinary continuing turn, or the **whole claimed conversation** for an
//! import or a rewritten history that forks. Unit tests already prove
//! `PromptCapture::of` computes the right boundary given a hand-built `Vec`;
//! they cannot prove that the wire-level pipeline actually hands it the whole
//! claimed array in the shape those hand-built vectors assume. These tests
//! drive the real Anthropic Messages and Responses surfaces, with a real
//! classifier upstream capturing what it was actually sent.
//!
//! Eight cases, the full product of three independent axes: **dialect**
//! (`/v1/messages` or `/v1/responses`), **shape** (a brand-new session
//! importing earlier conversation, or an existing session whose resend
//! disagrees with what is stored and forks to a fresh generation), and
//! **ending** (new user text, or a tool continuation with no prompt of its
//! own). Every case sends a unique old-user, old-tool-output, and
//! system-instruction sentinel and asserts all three are absent from the
//! *entire* captured HTTP body -- not just its parsed `state` field, since a
//! leak into another field of the wire envelope would be invisible to a check
//! that only read `state`. A fork's evidence is the store, not the response
//! status: an HTTP 200 answers every turn whether or not prefix admission
//! actually forked, so the rewrite cases read the original and forked
//! generations back out and assert they hold distinct content.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::MemorySpendLedger;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::{Item, ItemContent};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{EchoFrontierClient, FrontierClient, WireProtocol};
use roundhouse_server::classify_config::ClassifyConfig;
use roundhouse_server::classify_runtime::compose;
use roundhouse_server::test_support::{engine_over_echo, frontier_spec, single_model_catalog};
use roundhouse_server::{
    ControlPlane, Conversations, Engine, EngineConfig, messages_router, responses_router,
};

const ANSWER: &str = "frontier answer";
const PROVIDER: &str = "prefix-classify";

const CLASSIFIER_ANSWER: &str = r#"{"model":"jev-1.12","answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
},"usage":{"input_tokens":312,"output_tokens":48}}"#;

// ---------------------------------------------------------------------------
// Shared sentinels
//
// One finite set, reused across every case on both dialects (each test opens
// its own store, so there is no cross-test collision to guard against). A
// forbidden sentinel keeps its meaning wherever it appears: `OLD_USER` and
// `OLD_ANSWER` name history that must never reach the classifier at all,
// `TOOL_OUTPUT` names the current turn's own tool result (never sent,
// whatever the origin), `SYSTEM` names the system/instructions text (never
// sent, on any turn), and `NEW_QUESTION` names the one thing a user-ending
// turn is permitted to send.
// ---------------------------------------------------------------------------

const OLD_USER_SENTINEL: &str = "OLD_SENTINEL_do_not_resend_me";
const OLD_ANSWER_SENTINEL: &str = "OLD_ANSWER_do_not_resend_me";
const TOOL_OUTPUT_SENTINEL: &str = "TOOL_OUTPUT_SENTINEL_do_not_resend_me";
const SYSTEM_SENTINEL: &str = "SYSTEM_INSTRUCTION_SENTINEL_do_not_resend_me";
const NEW_QUESTION: &str = "NEW_QUESTION_only_me";
/// What a rewrite test's first (pre-fork) turn asks, so a forked call can be
/// checked for its absence -- the pre-fork generation's own prompt must not
/// leak into the forked generation's call.
const PRIMING_TURN_TEXT: &str = "FIRST_TURN_unrelated_priming_text";

fn classify_config(base_url: &str) -> ClassifyConfig {
    let json = format!(
        r#"{{
          "enabled": true,
          "revision": 4,
          "model": "jev-1.12",
          "base_url": "{base_url}",
          "auth": {{ "env": "CLASSIFY_PREFIX_TEST_KEY" }},
          "pricing": {{ "input_per_mtok_usd": 0.042, "output_per_mtok_usd": 0.084 }},
          "expected_output_tokens": 24,
          "caps": {{
            "max_prior_classifications": 4,
            "max_prompt_chars": 2000,
            "max_total_bytes": 8192
          }},
          "transport": {{
            "max_request_bytes": 65536,
            "max_response_bytes": 16384,
            "deadline_ms": 4000
          }},
          "executor": {{
            "max_in_flight": 8,
            "max_http_concurrency": 2,
            "call_ttl_ms": 60000,
            "result_retention_ms": 900000,
            "sweep_interval_ms": 50
          }},
          "budget": {{ "limit_usd": 25.0, "window": "total", "warn_at": 0.8 }}
        }}"#
    );
    ClassifyConfig::from_json(&json, "<test>").expect("a valid configuration")
}

fn env(name: &str) -> Option<String> {
    match name {
        "CLASSIFY_PREFIX_TEST_KEY" => Some("sk-classification-prefix-test".to_string()),
        _ => None,
    }
}

/// A loopback classifier that records every body it was sent, verbatim.
#[derive(Clone, Default)]
struct Classifier {
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<String>>>,
}

/// The body is captured before the count is published, not after. A std
/// `Mutex` unlock is a release operation and the `SeqCst` store that follows
/// it is what makes the body visible to any thread that later observes the
/// count through `Classifier::count`'s `SeqCst` load: `await_calls` below
/// waits on the count precisely so it can assume the corresponding body is
/// already in `seen`, and that assumption only holds in this order.
async fn handle_classify(State(state): State<Classifier>, body: String) -> Response {
    state.seen.lock().unwrap().push(body);
    state.calls.fetch_add(1, Ordering::SeqCst);
    Response::new(Body::from(CLASSIFIER_ANSWER))
}

async fn classifier_upstream() -> (String, Classifier) {
    let state = Classifier::default();
    let app = Router::new()
        .route("/systemone", post(handle_classify))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), state)
}

impl Classifier {
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Every captured body's own `state` field -- the rendered projection
    /// actually sent -- in arrival order.
    fn states(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|body| {
                let sent: Value = serde_json::from_str(body).expect("a JSON body arrived");
                sent["state"]
                    .as_str()
                    .expect("`state` is a string")
                    .to_string()
            })
            .collect()
    }

    /// Every captured body verbatim, in arrival order.
    ///
    /// Sentinel exclusion is checked against this, not against `states()`: a
    /// leak into `model` or into a `questions` entry would be invisible to a
    /// check that only parsed out `state`, and the egress ruling this suite
    /// exists to hold (T6) is about the whole wire body, not one field of it.
    fn bodies(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

async fn await_calls(classifier: &Classifier, count: usize) {
    for _ in 0..300 {
        if classifier.count() >= count {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "the classifier never reached {count} call(s); it saw {}",
        classifier.count()
    );
}

// ---------------------------------------------------------------------------
// The services under test
//
// One composition shared by both dialects: the same catalog, the same echo
// frontier, the same classifier runtime wired through `Engine::with_classifier`.
// Only the router differs, which is what makes a difference between the two
// surfaces' captured bodies a fact about the dialect rather than an accident
// of two independently hand-built fixtures.
// ---------------------------------------------------------------------------

fn engine_with_classifier(
    store: &Arc<MemoryStore>,
    classify_base_url: &str,
) -> Engine<MemoryStore, ByteTokenizer> {
    let catalog = single_model_catalog(frontier_spec(PROVIDER, "m", WireProtocol::OpenAiResponses));
    let engine = engine_over_echo(
        Arc::clone(store),
        catalog,
        Arc::new(EchoFrontierClient::new(ANSWER)) as Arc<dyn FrontierClient>,
        EngineConfig::default(),
    );
    let runtime = compose(
        "<test>",
        &classify_config(classify_base_url),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    engine.with_classifier(runtime)
}

/// The real Anthropic Messages surface, over a real prefix-admission engine
/// with a real classifier wired in.
fn messages_surface(store: &Arc<MemoryStore>, classify_base_url: &str) -> Router {
    messages_router(
        ControlPlane::open(),
        Arc::new(engine_with_classifier(store, classify_base_url)),
        Arc::clone(store),
        Arc::new(Conversations::new()),
    )
}

/// The real Responses surface, built identically but for the router --
/// `responses_router` and `messages_router` take the same
/// engine/store/conversations components (see the module doc).
fn responses_surface(store: &Arc<MemoryStore>, classify_base_url: &str) -> Router {
    responses_router(
        ControlPlane::open(),
        Arc::new(engine_with_classifier(store, classify_base_url)),
        Arc::clone(store),
        Arc::new(Conversations::new()),
    )
}

// ---------------------------------------------------------------------------
// Messages fixtures
// ---------------------------------------------------------------------------

fn messages_body(system: &str, messages: Vec<Value>) -> Value {
    json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "system": system,
        "messages": messages,
    })
}

fn user(text: &str) -> Value {
    json!({ "role": "user", "content": text })
}

fn assistant_text(text: &str) -> Value {
    json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] })
}

fn assistant_tool_call(call_id: &str) -> Value {
    json!({
        "role": "assistant",
        "content": [{
            "type": "tool_use",
            "id": call_id,
            "name": "Grep",
            "input": { "pattern": "fn main" },
        }],
    })
}

fn tool_result(call_id: &str, text: &str) -> Value {
    json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": call_id,
            "content": text,
        }],
    })
}

/// `POST /v1/messages`, draining the streamed body so the turn actually runs
/// to completion before this returns.
async fn post_messages(app: &Router, headers: &[(&str, &str)], body: &Value) -> StatusCode {
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
    let _ = response
        .into_body()
        .collect()
        .await
        .expect("a readable body");
    status
}

// ---------------------------------------------------------------------------
// Responses fixtures
// ---------------------------------------------------------------------------

fn responses_body(instructions: &str, cache_key: &str, input: Vec<Value>) -> Value {
    json!({
        "stream": true,
        "instructions": instructions,
        "prompt_cache_key": cache_key,
        "input": input,
    })
}

fn resp_user(text: &str) -> Value {
    json!({ "type": "message", "role": "user", "content": text })
}

fn resp_assistant(text: &str) -> Value {
    json!({ "type": "message", "role": "assistant", "content": text })
}

fn resp_function_call(call_id: &str) -> Value {
    json!({
        "type": "function_call",
        "call_id": call_id,
        "name": "shell",
        "arguments": r#"{"command":["ls"]}"#,
    })
}

fn resp_function_call_output(call_id: &str, output: &str) -> Value {
    json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output,
    })
}

/// `POST /v1/responses`, draining the streamed body for the same reason
/// [`post_messages`] does. No headers: the conversation's identity comes from
/// `prompt_cache_key` alone, the same shape
/// `codex_conformance.rs::supplied_cache_key_survives_a_history_rewrite`
/// drives.
async fn post_responses(app: &Router, body: &Value) -> StatusCode {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(body).expect("a JSON body")))
                .expect("a well-formed request"),
        )
        .await
        .expect("the router answers");
    let status = response.status();
    let _ = response
        .into_body()
        .collect()
        .await
        .expect("a readable body");
    status
}

// ---------------------------------------------------------------------------
// Stored-history evidence
//
// An HTTP 200 answers every turn whether or not a fork happened underneath
// it, so the rewrite cases below read the log back rather than trust the
// status code. `stored_items` panics on an absent session rather than
// answering an empty vector -- the store's own `SessionNotFound` (see
// `roundhouse_core::store::StoreError`) is what makes that panic possible
// rather than a silent, indistinguishable-from-empty read.
// ---------------------------------------------------------------------------

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

/// The session id a Messages `x-claude-code-session-id` header resolves to.
/// The Responses dialect carries no such namespace -- its selected session id
/// is the cache key verbatim (`ControlPlane::open()`'s `qualify` is the
/// identity function), so the rewrite tests below read it back directly
/// rather than through a helper like this one.
fn named(session: &str) -> String {
    format!("anthropic_messages/{session}")
}

// ---------------------------------------------------------------------------
// Messages: import
// ---------------------------------------------------------------------------

/// **An imported history ending in new user text sends only that prompt, and
/// not the system instructions either.**
///
/// A brand-new session's very first request already carries earlier
/// conversation -- exactly the shape `bind_prefix` answers with
/// `Search::Fresh`, handing the engine the *whole* claimed array as `input`.
#[tokio::test]
async fn messages_import_ending_in_user_text_sends_only_the_new_prompt() {
    let (base_url, classifier) = classifier_upstream().await;
    let store = Arc::new(MemoryStore::new());
    let app = messages_surface(&store, &base_url);
    let headers = [("x-claude-code-session-id", "sess-msg-import-user")];

    let imported = messages_body(
        SYSTEM_SENTINEL,
        vec![
            user(OLD_USER_SENTINEL),
            assistant_text(OLD_ANSWER_SENTINEL),
            user(NEW_QUESTION),
        ],
    );
    assert_eq!(
        post_messages(&app, &headers, &imported).await,
        StatusCode::OK
    );

    await_calls(&classifier, 1).await;
    assert_eq!(
        classifier.count(),
        1,
        "one turn must cost exactly one classification call"
    );

    for body in classifier.bodies() {
        for forbidden in [OLD_USER_SENTINEL, OLD_ANSWER_SENTINEL, SYSTEM_SENTINEL] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` must be excluded from the whole captured body, not just `state`: {body}"
            );
        }
    }
    let state = &classifier.states()[0];
    assert!(
        state.contains(NEW_QUESTION),
        "the permitted current prompt must be retained: {state}"
    );
    assert!(state.contains("origin: user_text"), "{state}");
    assert!(
        state.contains("3 items of earlier conversation presented with this turn"),
        "the import is counted, not silently absorbed: {state}"
    );

    let items = stored_items(&store, &named("sess-msg-import-user")).await;
    assert!(
        items
            .iter()
            .any(|item| *item == Item::user_text(NEW_QUESTION)),
        "the permitted prompt is durable: {items:#?}"
    );
}

/// **An imported history ending in a tool continuation sends no prompt text
/// at all**, and in particular not the old user sentinel, the tool output, or
/// the system instructions.
#[tokio::test]
async fn messages_import_ending_in_tool_continuation_sends_no_prompt_text() {
    let (base_url, classifier) = classifier_upstream().await;
    let store = Arc::new(MemoryStore::new());
    let app = messages_surface(&store, &base_url);
    let headers = [("x-claude-code-session-id", "sess-msg-import-tool")];

    let imported = messages_body(
        SYSTEM_SENTINEL,
        vec![
            user(OLD_USER_SENTINEL),
            assistant_tool_call("toolu_import_1"),
            tool_result("toolu_import_1", TOOL_OUTPUT_SENTINEL),
        ],
    );
    assert_eq!(
        post_messages(&app, &headers, &imported).await,
        StatusCode::OK
    );

    await_calls(&classifier, 1).await;
    assert_eq!(
        classifier.count(),
        1,
        "one turn must cost exactly one classification call"
    );

    for body in classifier.bodies() {
        for forbidden in [OLD_USER_SENTINEL, TOOL_OUTPUT_SENTINEL, SYSTEM_SENTINEL] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` must be excluded from the whole captured body, not just `state`: {body}"
            );
        }
    }
    let state = &classifier.states()[0];
    assert!(state.contains("origin: tool_continuation"), "{state}");
    assert!(
        state.contains("no prompt: the agent is continuing its own tool loop"),
        "{state}"
    );
    assert!(
        state.contains("3 items of earlier conversation presented with this turn"),
        "{state}"
    );

    let items = stored_items(&store, &named("sess-msg-import-tool")).await;
    assert!(
        items.iter().any(|item| matches!(
            &item.content,
            ItemContent::ToolResult { output, .. } if output == TOOL_OUTPUT_SENTINEL
        )),
        "the tool result is durable even though it is never sent to the classifier: {items:#?}"
    );
}

// ---------------------------------------------------------------------------
// Messages: rewrite
// ---------------------------------------------------------------------------

/// **A divergent resend that forks to a fresh generation also sends only the
/// new prompt** -- the fork path is a second call site for `Search::Fresh`
/// (`prefix_admission::search`'s "nothing agreed anywhere" arm) and must keep
/// the same property an ordinary import does. The fork is proved against the
/// store: the original generation keeps what it was told, and the forked one
/// starts empty rather than inheriting.
#[tokio::test]
async fn messages_rewrite_ending_in_user_text_forks_and_sends_only_the_new_prompt() {
    let (base_url, classifier) = classifier_upstream().await;
    let store = Arc::new(MemoryStore::new());
    let app = messages_surface(&store, &base_url);
    let headers = [("x-claude-code-session-id", "sess-msg-rewrite-user")];

    let priming = messages_body(SYSTEM_SENTINEL, vec![user(PRIMING_TURN_TEXT)]);
    assert_eq!(
        post_messages(&app, &headers, &priming).await,
        StatusCode::OK
    );
    await_calls(&classifier, 1).await;

    // The client edited its own history out from under us: the same session
    // name, a first message that disagrees with what is stored.
    let divergent = messages_body(
        SYSTEM_SENTINEL,
        vec![
            user(OLD_USER_SENTINEL),
            assistant_text(OLD_ANSWER_SENTINEL),
            user(NEW_QUESTION),
        ],
    );
    assert_eq!(
        post_messages(&app, &headers, &divergent).await,
        StatusCode::OK
    );
    await_calls(&classifier, 2).await;
    assert_eq!(
        classifier.count(),
        2,
        "one classification call per turn, across the fork"
    );

    for body in classifier.bodies() {
        for forbidden in [OLD_USER_SENTINEL, OLD_ANSWER_SENTINEL, SYSTEM_SENTINEL] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` must be excluded from the whole captured body, not just `state`: {body}"
            );
        }
    }
    let forked_call = &classifier.bodies()[1];
    assert!(
        !forked_call.contains(PRIMING_TURN_TEXT),
        "the pre-fork generation's own prompt must not leak into the forked call: {forked_call}"
    );
    let state = &classifier.states()[1];
    assert!(state.contains(NEW_QUESTION), "{state}");
    assert!(state.contains("origin: user_text"), "{state}");
    assert!(
        state.contains("3 items of earlier conversation presented with this turn"),
        "{state}"
    );

    let original_log = stored_items(&store, &named("sess-msg-rewrite-user")).await;
    assert!(!original_log.is_empty());
    assert!(
        original_log
            .iter()
            .any(|item| *item == Item::user_text(PRIMING_TURN_TEXT)),
        "the original generation keeps the history it was told: {original_log:#?}"
    );
    let fork_log = stored_items(&store, &format!("{}#g1", named("sess-msg-rewrite-user"))).await;
    assert!(!fork_log.is_empty());
    assert!(
        fork_log
            .iter()
            .any(|item| *item == Item::user_text(NEW_QUESTION)),
        "the rewritten history opens a fresh generation: {fork_log:#?}"
    );
    assert!(
        !fork_log
            .iter()
            .any(|item| *item == Item::user_text(PRIMING_TURN_TEXT)),
        "and the fork starts empty rather than inheriting: {fork_log:#?}"
    );
}

/// The same fork property, but the divergent resend ends in a tool
/// continuation rather than new user text.
#[tokio::test]
async fn messages_rewrite_ending_in_tool_continuation_forks_and_sends_no_prompt_text() {
    let (base_url, classifier) = classifier_upstream().await;
    let store = Arc::new(MemoryStore::new());
    let app = messages_surface(&store, &base_url);
    let headers = [("x-claude-code-session-id", "sess-msg-rewrite-tool")];

    let priming = messages_body(SYSTEM_SENTINEL, vec![user(PRIMING_TURN_TEXT)]);
    assert_eq!(
        post_messages(&app, &headers, &priming).await,
        StatusCode::OK
    );
    await_calls(&classifier, 1).await;

    let divergent = messages_body(
        SYSTEM_SENTINEL,
        vec![
            user(OLD_USER_SENTINEL),
            assistant_tool_call("toolu_rewrite_1"),
            tool_result("toolu_rewrite_1", TOOL_OUTPUT_SENTINEL),
        ],
    );
    assert_eq!(
        post_messages(&app, &headers, &divergent).await,
        StatusCode::OK
    );
    await_calls(&classifier, 2).await;
    assert_eq!(
        classifier.count(),
        2,
        "one classification call per turn, across the fork"
    );

    for body in classifier.bodies() {
        for forbidden in [OLD_USER_SENTINEL, TOOL_OUTPUT_SENTINEL, SYSTEM_SENTINEL] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` must be excluded from the whole captured body, not just `state`: {body}"
            );
        }
    }
    let forked_call = &classifier.bodies()[1];
    assert!(
        !forked_call.contains(PRIMING_TURN_TEXT),
        "the pre-fork generation's own prompt must not leak into the forked call: {forked_call}"
    );
    let state = &classifier.states()[1];
    assert!(state.contains("origin: tool_continuation"), "{state}");
    assert!(
        state.contains("no prompt: the agent is continuing its own tool loop"),
        "{state}"
    );
    assert!(
        state.contains("3 items of earlier conversation presented with this turn"),
        "{state}"
    );

    let original_log = stored_items(&store, &named("sess-msg-rewrite-tool")).await;
    assert!(!original_log.is_empty());
    assert!(
        original_log
            .iter()
            .any(|item| *item == Item::user_text(PRIMING_TURN_TEXT)),
        "the original generation keeps the history it was told: {original_log:#?}"
    );
    let fork_log = stored_items(&store, &format!("{}#g1", named("sess-msg-rewrite-tool"))).await;
    assert!(!fork_log.is_empty());
    assert!(
        fork_log.iter().any(|item| matches!(
            &item.content,
            ItemContent::ToolResult { output, .. } if output == TOOL_OUTPUT_SENTINEL
        )),
        "the fork's own tool result is durable: {fork_log:#?}"
    );
    assert!(
        !fork_log
            .iter()
            .any(|item| *item == Item::user_text(PRIMING_TURN_TEXT)),
        "and the fork starts empty rather than inheriting: {fork_log:#?}"
    );
}

// ---------------------------------------------------------------------------
// Responses: import
// ---------------------------------------------------------------------------

/// The Responses-dialect twin of
/// `messages_import_ending_in_user_text_sends_only_the_new_prompt`. The same
/// shape, the same boundary rule, the same egress contract -- proved over
/// `responses_router` rather than assumed to follow from the Messages result,
/// because `PromptCapture::of` sits downstream of `canonicalize`, and each
/// dialect canonicalizes its own way.
#[tokio::test]
async fn responses_import_ending_in_user_text_sends_only_the_new_prompt() {
    let (base_url, classifier) = classifier_upstream().await;
    let store = Arc::new(MemoryStore::new());
    let app = responses_surface(&store, &base_url);

    let imported = responses_body(
        SYSTEM_SENTINEL,
        "resp-import-user",
        vec![
            resp_user(OLD_USER_SENTINEL),
            resp_assistant(OLD_ANSWER_SENTINEL),
            resp_user(NEW_QUESTION),
        ],
    );
    assert_eq!(post_responses(&app, &imported).await, StatusCode::OK);

    await_calls(&classifier, 1).await;
    assert_eq!(
        classifier.count(),
        1,
        "one turn must cost exactly one classification call"
    );

    for body in classifier.bodies() {
        for forbidden in [OLD_USER_SENTINEL, OLD_ANSWER_SENTINEL, SYSTEM_SENTINEL] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` must be excluded from the whole captured body, not just `state`: {body}"
            );
        }
    }
    let state = &classifier.states()[0];
    assert!(
        state.contains(NEW_QUESTION),
        "the permitted current prompt must be retained: {state}"
    );
    assert!(state.contains("origin: user_text"), "{state}");
    assert!(
        state.contains("3 items of earlier conversation presented with this turn"),
        "the import is counted, not silently absorbed: {state}"
    );

    // Responses reads its selected session id directly off the cache key --
    // no `anthropic_messages/` namespace, unlike the Messages dialect (see
    // `named`'s doc comment).
    let items = stored_items(&store, "resp-import-user").await;
    assert!(
        items
            .iter()
            .any(|item| *item == Item::user_text(NEW_QUESTION)),
        "the permitted prompt is durable: {items:#?}"
    );
}

/// The Responses-dialect twin of
/// `messages_import_ending_in_tool_continuation_sends_no_prompt_text`.
#[tokio::test]
async fn responses_import_ending_in_tool_continuation_sends_no_prompt_text() {
    let (base_url, classifier) = classifier_upstream().await;
    let store = Arc::new(MemoryStore::new());
    let app = responses_surface(&store, &base_url);

    let imported = responses_body(
        SYSTEM_SENTINEL,
        "resp-import-tool",
        vec![
            resp_user(OLD_USER_SENTINEL),
            resp_function_call("call_import_1"),
            resp_function_call_output("call_import_1", TOOL_OUTPUT_SENTINEL),
        ],
    );
    assert_eq!(post_responses(&app, &imported).await, StatusCode::OK);

    await_calls(&classifier, 1).await;
    assert_eq!(
        classifier.count(),
        1,
        "one turn must cost exactly one classification call"
    );

    for body in classifier.bodies() {
        for forbidden in [OLD_USER_SENTINEL, TOOL_OUTPUT_SENTINEL, SYSTEM_SENTINEL] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` must be excluded from the whole captured body, not just `state`: {body}"
            );
        }
    }
    let state = &classifier.states()[0];
    assert!(state.contains("origin: tool_continuation"), "{state}");
    assert!(
        state.contains("no prompt: the agent is continuing its own tool loop"),
        "{state}"
    );
    assert!(
        state.contains("3 items of earlier conversation presented with this turn"),
        "{state}"
    );

    let items = stored_items(&store, "resp-import-tool").await;
    assert!(
        items.iter().any(|item| matches!(
            &item.content,
            ItemContent::ToolResult { output, .. } if output == TOOL_OUTPUT_SENTINEL
        )),
        "the tool result is durable even though it is never sent to the classifier: {items:#?}"
    );
}

// ---------------------------------------------------------------------------
// Responses: rewrite
// ---------------------------------------------------------------------------

/// The Responses-dialect twin of
/// `messages_rewrite_ending_in_user_text_forks_and_sends_only_the_new_prompt`.
/// No `session-id`/`thread-id` headers: the conversation's identity is the
/// `prompt_cache_key` alone, which is also the name prefix admission forks
/// under -- `bound_session(cache_key, 1)` is `"{cache_key}#g1"`.
#[tokio::test]
async fn responses_rewrite_ending_in_user_text_forks_and_sends_only_the_new_prompt() {
    let (base_url, classifier) = classifier_upstream().await;
    let store = Arc::new(MemoryStore::new());
    let app = responses_surface(&store, &base_url);
    let cache_key = "resp-rewrite-user";

    let priming = responses_body(
        SYSTEM_SENTINEL,
        cache_key,
        vec![resp_user(PRIMING_TURN_TEXT)],
    );
    assert_eq!(post_responses(&app, &priming).await, StatusCode::OK);
    await_calls(&classifier, 1).await;

    let divergent = responses_body(
        SYSTEM_SENTINEL,
        cache_key,
        vec![
            resp_user(OLD_USER_SENTINEL),
            resp_assistant(OLD_ANSWER_SENTINEL),
            resp_user(NEW_QUESTION),
        ],
    );
    assert_eq!(post_responses(&app, &divergent).await, StatusCode::OK);
    await_calls(&classifier, 2).await;
    assert_eq!(
        classifier.count(),
        2,
        "one classification call per turn, across the fork"
    );

    for body in classifier.bodies() {
        for forbidden in [OLD_USER_SENTINEL, OLD_ANSWER_SENTINEL, SYSTEM_SENTINEL] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` must be excluded from the whole captured body, not just `state`: {body}"
            );
        }
    }
    let forked_call = &classifier.bodies()[1];
    assert!(
        !forked_call.contains(PRIMING_TURN_TEXT),
        "the pre-fork generation's own prompt must not leak into the forked call: {forked_call}"
    );
    let state = &classifier.states()[1];
    assert!(state.contains(NEW_QUESTION), "{state}");
    assert!(state.contains("origin: user_text"), "{state}");
    assert!(
        state.contains("3 items of earlier conversation presented with this turn"),
        "{state}"
    );

    let original_log = stored_items(&store, cache_key).await;
    assert!(!original_log.is_empty());
    assert!(
        original_log
            .iter()
            .any(|item| *item == Item::user_text(PRIMING_TURN_TEXT)),
        "the original generation keeps the history it was told: {original_log:#?}"
    );
    let fork_log = stored_items(&store, &format!("{cache_key}#g1")).await;
    assert!(!fork_log.is_empty());
    assert!(
        fork_log
            .iter()
            .any(|item| *item == Item::user_text(NEW_QUESTION)),
        "the rewritten history opens a fresh generation: {fork_log:#?}"
    );
    assert!(
        !fork_log
            .iter()
            .any(|item| *item == Item::user_text(PRIMING_TURN_TEXT)),
        "and the fork starts empty rather than inheriting: {fork_log:#?}"
    );
}

/// The Responses-dialect twin of
/// `messages_rewrite_ending_in_tool_continuation_forks_and_sends_no_prompt_text`.
#[tokio::test]
async fn responses_rewrite_ending_in_tool_continuation_forks_and_sends_no_prompt_text() {
    let (base_url, classifier) = classifier_upstream().await;
    let store = Arc::new(MemoryStore::new());
    let app = responses_surface(&store, &base_url);
    let cache_key = "resp-rewrite-tool";

    let priming = responses_body(
        SYSTEM_SENTINEL,
        cache_key,
        vec![resp_user(PRIMING_TURN_TEXT)],
    );
    assert_eq!(post_responses(&app, &priming).await, StatusCode::OK);
    await_calls(&classifier, 1).await;

    let divergent = responses_body(
        SYSTEM_SENTINEL,
        cache_key,
        vec![
            resp_user(OLD_USER_SENTINEL),
            resp_function_call("call_rewrite_1"),
            resp_function_call_output("call_rewrite_1", TOOL_OUTPUT_SENTINEL),
        ],
    );
    assert_eq!(post_responses(&app, &divergent).await, StatusCode::OK);
    await_calls(&classifier, 2).await;
    assert_eq!(
        classifier.count(),
        2,
        "one classification call per turn, across the fork"
    );

    for body in classifier.bodies() {
        for forbidden in [OLD_USER_SENTINEL, TOOL_OUTPUT_SENTINEL, SYSTEM_SENTINEL] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` must be excluded from the whole captured body, not just `state`: {body}"
            );
        }
    }
    let forked_call = &classifier.bodies()[1];
    assert!(
        !forked_call.contains(PRIMING_TURN_TEXT),
        "the pre-fork generation's own prompt must not leak into the forked call: {forked_call}"
    );
    let state = &classifier.states()[1];
    assert!(state.contains("origin: tool_continuation"), "{state}");
    assert!(
        state.contains("no prompt: the agent is continuing its own tool loop"),
        "{state}"
    );
    assert!(
        state.contains("3 items of earlier conversation presented with this turn"),
        "{state}"
    );

    let original_log = stored_items(&store, cache_key).await;
    assert!(!original_log.is_empty());
    assert!(
        original_log
            .iter()
            .any(|item| *item == Item::user_text(PRIMING_TURN_TEXT)),
        "the original generation keeps the history it was told: {original_log:#?}"
    );
    let fork_log = stored_items(&store, &format!("{cache_key}#g1")).await;
    assert!(!fork_log.is_empty());
    assert!(
        fork_log.iter().any(|item| matches!(
            &item.content,
            ItemContent::ToolResult { output, .. } if output == TOOL_OUTPUT_SENTINEL
        )),
        "the fork's own tool result is durable: {fork_log:#?}"
    );
    assert!(
        !fork_log
            .iter()
            .any(|item| *item == Item::user_text(PRIMING_TURN_TEXT)),
        "and the fork starts empty rather than inheriting: {fork_log:#?}"
    );
}
