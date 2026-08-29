// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M5 of `PLAN-agentic-control-plane.md`: the roundhouse MCP surface, wired
//! into a running deployment.
//!
//! `roundhouse-mcp`'s own suite proves the tools against the [`ControlReads`]
//! seam with no engine and no socket. What it cannot prove is the half this
//! milestone is actually about: that a narrowing an *agent* asks for reaches the
//! router that serves its *next turn*, that a steer's payload deposited on the
//! way out of one turn is what `fetch_steer` hands back on the way into the
//! next, and that an MCP request does not queue behind the turn it is asking
//! about. Every claim here spans the surface, the store, the engine and a real
//! client at once, which is why they are integration tests.
//!
//! **The three seams under test, and the shape of each assertion.**
//!
//! - *The overlay reaches routing.* The observable is
//!   `DecisionRecord::turn_policy_digest` on the next `Routed` event — the audit
//!   trail, not the tool's own answer, because a tool that reported its overlay
//!   applied while nothing changed is exactly the failure worth catching.
//! - *The payload reaches the agent.* The observable is a byte comparison
//!   between what the interjector supplied and what `fetch_steer` returned,
//!   joined on the `call_id` the log holds.
//! - *The channel joins the conversation.* The observable is
//!   [`binding_in_items`] finding the minted id in the session's own committed
//!   items, after Codex's client resent the tool output as ordinary history.
//!
//! **What stands in for M6.** The decision to steer is [`TestInterjector`], a
//! scripted occupant of the real seam, exactly as in `steering_emission.rs`.
//! M5 builds the tool that reads a steer; M6 builds the thing that writes one.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HOST};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{MemorySpendLedger, Principal, SpendLedger};
use roundhouse_core::event::{
    Accounting, ControlRecord, SessionEventKind, Usage, ValidationOutcome,
};
use roundhouse_core::ids::{SessionId, SideCallId, ValidationId};
use roundhouse_core::interject::{Interjection, InterjectionContext, Interjector};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{
    AffinityPolicy, CacheLedger, CacheModel, Candidate, DecisionRecord, ProviderPricing, Target,
};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_core::validate::{Arm, Divergence, SteerAction, TriggerRecord, Verdict};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierChunk, FrontierClient, FrontierError, FrontierModelSpec,
    FrontierQuote, FrontierStream, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_mcp::{BindingId, ControlStore, binding_in_items};
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, ControlPlaneReads, Conversations, EchoLocalExecutor, Engine,
    EngineConfig, mcp_router, responses_router,
};

mod common;
use common::codex::{assistant_message, function_call_output_item, request, user_message};
use common::{BLOCK_SIZE, LOCAL_MODEL, MINUTE, admin_key, embedded_fleet, key, sha256_hex};

/// What each executor answers with, so a target is legible in the answer as
/// well as in the log.
const LOCAL_ANSWER: &str = "local answer";
const FRONTIER_ANSWER: &str = "frontier answer";

/// The correction one steer carries.
///
/// Distinctive on purpose: an assertion that this exact text came back from
/// `fetch_steer` is an assertion about a literal nothing else could produce.
const STEER_GUIDANCE: &str = "you are editing a file the task did not name; go back to the parser";

/// Ceiling on anything this suite waits for.
const PATIENCE: Duration = Duration::from_secs(10);

/// The `Host` a request to a *configured* deployment arrives with.
///
/// Deliberately not a loopback name: a configured deployment is served behind
/// whatever hostname an operator gave it and turns the transport's loopback-only
/// rebinding guard off, so a fixture that used `localhost` would pass whether or
/// not that had been done.
const HOST_HEADER: &str = "roundhouse.internal.example.com";

/// The `Host` a request to an *open* deployment arrives with.
///
/// An unconfigured deployment has no bearer key to stand in for the guard, so it
/// keeps rmcp's loopback allowlist and is reachable only as the laptop process
/// it is — see [`open_mode_refuses_a_tools_call_from_a_host_it_does_not_serve`].
const LOOPBACK_HOST: &str = "127.0.0.1";

/// A `Host` a rebound DNS name would arrive as.
const REBOUND_HOST: &str = "evil.example.com";

// ---------------------------------------------------------------------------
// The control plane that authenticates `common::key`/`common::admin_key`
// ---------------------------------------------------------------------------

/// `acme/ada` may be routed anywhere; `ourown/bob` may only be routed locally.
///
/// Two projects rather than one because the two halves of the narrowing rule
/// need different ceilings to be visible: `ada` proves an ask that *is* a
/// narrowing takes effect, and `bob` proves an ask that would be a widening is
/// reported rather than honored. An admin key is declared so the surface's
/// refusal of one is a refusal of a key this deployment really issued.
fn control_plane() -> Arc<ControlPlane> {
    let json = json!({
        "projects": [
            { "id": "acme" },
            { "id": "ourown", "policy": { "allow": ["local/*"] } },
        ],
        "users": [{ "id": "ada" }, { "id": "bob" }],
        "keys": [
            { "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("ada")) },
            { "project": "ourown", "user": "bob", "key_sha256": sha256_hex(&key("bob")) },
        ],
        "admin_keys": [sha256_hex(&admin_key("root"))],
    })
    .to_string();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "mcp-surface fixture")
            .expect("the fixture config must validate"),
    ))
}

// ---------------------------------------------------------------------------
// A fleet where the default answer is frontier
// ---------------------------------------------------------------------------

/// A free, instant hosted model.
///
/// Beside the deliberately slow local worker below, the router prefers it every
/// turn — which is what makes "this turn went local" an event with exactly one
/// possible cause: the agent asked for it.
fn free_catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        provider: "anthropic".into(),
        model: "claude".into(),
        wire_protocol: WireProtocol::AnthropicMessages,
        cache_model: CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
        pricing: ProviderPricing::free(),
        quality_prior: 0.95,
        base_ttft_ms: 1.0,
        ttft_ms_per_uncached_token: 0.0,
    }])
}

fn engine_config() -> EngineConfig {
    EngineConfig {
        block_size: BLOCK_SIZE,
        local_model: LOCAL_MODEL.to_string(),
        // A fixture knob and not a claim about any real fleet: it arranges that
        // an unnarrowed turn goes hosted.
        local_base_ttft_ms: 5_000.0,
        ..Default::default()
    }
}

/// Every target a turn of this deployment's could be routed to, priced the way
/// the router prices them.
///
/// The catalog *and* the local worker, assembled at the same site the fleet is
/// attached — which is the rule `main::reachable_candidates` states and the one
/// thing a caller of [`ControlPlaneReads`] has to get right. A list missing the
/// local worker would make `prefer local` an unhonorable ask on a deployment
/// that serves locally perfectly well.
fn reachable() -> Vec<Candidate> {
    let catalog = free_catalog();
    let mut ledger = CacheLedger::new();
    catalog.apply_to_ledger(&mut ledger);
    let mut candidates = catalog.quote(&ledger, roundhouse_core::now_ms(), 1_024, 256);
    candidates.push(Candidate {
        target: Target::Local {
            worker_id: 1,
            dp_rank: 0,
            model: LOCAL_MODEL.into(),
        },
        expected_prefill_tokens: 1_024.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: engine_config().local_base_ttft_ms,
        expected_cost_usd: 0.0,
        quality_prior: engine_config().local_quality_prior,
        load: Some(0.0),
    });
    candidates
}

// ---------------------------------------------------------------------------
// The occupant of the interjection seam
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Plan {
    Steer,
    Proceed,
}

/// The decision record a steering occupant owes the log.
///
/// One helper rather than an inline literal per call site, because the shape is
/// load-bearing in a way a literal invites forgetting: `SteerAction::Steer` is
/// what the session fold keys `steered_on_turn` and `last_guidance` off, so an
/// occupant that completed with guidance and recorded `Continue` would emit the
/// correction and leave nothing able to re-read it.
fn steer_record(directive: &str) -> ControlRecord {
    let mut record = ControlRecord::default();
    record.validation_decided(
        ValidationId::new("val_1"),
        TriggerRecord::new(2, 4_000, Vec::new()),
        Arm::Live,
        ValidationOutcome::Judged {
            side_call_id: SideCallId::new("side_1"),
            verdict: Verdict {
                on_track: false,
                confidence: 0.7,
                divergence: Some(Divergence {
                    at_step: 3,
                    description: "the judge's own prose, which never travels".into(),
                }),
                missing_context: None,
            },
            action: SteerAction::Steer {
                directive: directive.to_string(),
            },
        },
    );
    record
}

/// A scripted occupant, as in `steering_emission.rs`: what this suite is about
/// is what happens *after* something decides to steer.
struct TestInterjector {
    script: Mutex<VecDeque<Plan>>,
}

impl TestInterjector {
    fn new(script: impl IntoIterator<Item = Plan>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into_iter().collect()),
        })
    }
}

#[async_trait]
impl Interjector for TestInterjector {
    async fn consider(&self, context: &InterjectionContext<'_>) -> Interjection {
        let plan = self
            .script
            .lock()
            .expect("script mutex")
            .pop_front()
            .unwrap_or(Plan::Proceed);
        match plan {
            Plan::Proceed => Interjection::proceed(),
            // **The whole of the correction, in the item.** Before M10.0 this
            // arm minted a synthetic `fetch_steer` call and put the guidance in
            // a `guidance` field beside it, for the engine to deposit in the
            // control store. The steer is the turn's answer now, so the text
            // goes in the item and there is nothing beside it -- which is what
            // `the_steer_this_deployment_wrote_is_what_fetch_steer_re_reads`
            // below is really asserting about.
            Plan::Steer => Interjection::Complete {
                item: Item::assistant_text(STEER_GUIDANCE, context.response_id.clone()),
                usage: Usage {
                    input_tokens: 96,
                    cached_input_tokens: 32,
                    // Nothing was dispatched, so nothing was written into any
                    // provider's cache.
                    cache_write_tokens: 0,
                    output_tokens: 24,
                    reasoning_tokens: 8,
                    accounting: Accounting::Reported,
                },
                // **No longer empty, and that is M10.0's fold rule showing up in
                // a test double.** The record used to carry nothing here,
                // because the *item* said a steer had happened -- it was a tool
                // call bearing a response id, a shape no client can produce. A
                // steer is assistant text now, indistinguishable from every
                // dispatched turn's answer, so `ValidationDecided` is the only
                // event that can say one happened. A double that steers without
                // saying so leaves `last_guidance` empty and `fetch_steer` with
                // nothing to serve -- which is the same thing a *production*
                // occupant skipping the record would do.
                record: steer_record(STEER_GUIDANCE),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// A frontier client that can be held mid-turn
// ---------------------------------------------------------------------------

/// A [`FrontierClient`] that blocks inside `execute` until it is released.
///
/// The only way to make "an MCP call arrives while a turn is running" a fact
/// rather than a race: with a real echo client the turn is over before the
/// second request could be built, and the test would pass whether or not the
/// surface queued behind the session gate.
struct GatedFrontierClient {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl FrontierClient for GatedFrontierClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(FrontierChunk::whole_response(
            FRONTIER_ANSWER.to_string(),
            quote.prompt.len() as u64,
            0,
            FRONTIER_ANSWER.len() as u64,
            0,
        ))
    }
}

// ---------------------------------------------------------------------------
// The deployment under test
// ---------------------------------------------------------------------------

struct Rig {
    app: Router,
    store: Arc<MemoryStore>,
    /// The one control store, shared with the engine. Held so a test can look
    /// at what the surface wrote without going back through the wire.
    control: Arc<ControlStore>,
    /// A `Host` this deployment answers to, given its mode.
    ///
    /// Carried on the rig rather than fixed as one constant because the
    /// transport's rebinding guard now follows the control plane: a configured
    /// deployment serves any host and relies on the key, an open one serves only
    /// loopback because it has no key to rely on. A single constant would have
    /// made every open-mode assertion below a statement about whichever of the
    /// two the constant happened to name.
    host: &'static str,
}

async fn rig(plane: Arc<ControlPlane>) -> Rig {
    build(
        plane,
        Arc::new(EchoFrontierClient::new(FRONTIER_ANSWER)),
        [],
    )
    .await
}

async fn steering_rig(plane: Arc<ControlPlane>, script: impl IntoIterator<Item = Plan>) -> Rig {
    build(
        plane,
        Arc::new(EchoFrontierClient::new(FRONTIER_ANSWER)),
        script,
    )
    .await
}

/// The composition this milestone is about, arranged exactly as `main::serve`
/// arranges it: **one** [`Conversations`] and **one** [`ControlStore`] behind
/// both surfaces, or the agent and the engine would be talking past each other.
async fn build(
    plane: Arc<ControlPlane>,
    frontier: Arc<dyn FrontierClient>,
    script: impl IntoIterator<Item = Plan>,
) -> Rig {
    ensure_rustls_crypto_provider();
    let host = match plane.as_ref() {
        ControlPlane::Open => LOOPBACK_HOST,
        ControlPlane::Configured { .. } => HOST_HEADER,
    };
    let store = Arc::new(MemoryStore::new());
    let control = Arc::new(ControlStore::new());
    let conversations = Arc::new(Conversations::new());
    let spend: Arc<dyn SpendLedger> = Arc::new(MemorySpendLedger::new());

    let engine = Arc::new(
        Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new(LOCAL_ANSWER)),
            free_catalog(),
            frontier,
            Arc::new(AffinityPolicy::new()),
            engine_config(),
        )
        // A real local option, or "the overlay changed the route" would be a
        // claim about a one-candidate set.
        .with_fleet(embedded_fleet().await)
        .with_spend_ledger(Arc::clone(&spend))
        .with_interjector(TestInterjector::new(script) as Arc<dyn Interjector>)
        .with_control_store(Arc::clone(&control)),
    );

    let app = mcp_router(
        Arc::clone(&plane),
        Arc::new(ControlPlaneReads::new(
            Arc::clone(&plane),
            Arc::clone(&store),
            spend,
            Arc::clone(&conversations),
            reachable(),
        )),
        Arc::clone(&control),
    )
    .merge(responses_router(
        plane,
        Arc::clone(&engine),
        Arc::clone(&store),
        conversations,
    ));

    Rig {
        app,
        store,
        control,
        host,
    }
}

// ---------------------------------------------------------------------------
// Driving it
// ---------------------------------------------------------------------------

/// One conversation, grown the way a client grows it.
///
/// The Responses API has no server-side cursor: a client re-sends its whole
/// history every turn, and this surface admits only the suffix. A test that sent
/// one message per turn would therefore *fork* the session on turn two — the
/// prefix check would refuse a claim that no longer matches the log — and every
/// multi-turn assertion below would silently be about two different sessions.
/// So this accumulates, and it accumulates from the frames the server actually
/// emitted rather than from what the test expected: what a client appends is
/// whatever `response.output_item.done` announced, verbatim, which is the
/// property M4 pinned and the one that keeps a resend from forking.
struct Conversation {
    secret: String,
    cache_key: String,
    history: Vec<Value>,
}

impl Conversation {
    fn new(secret: &str, cache_key: &str) -> Self {
        Self {
            secret: secret.to_string(),
            cache_key: cache_key.to_string(),
            history: Vec::new(),
        }
    }

    /// One turn, returning its raw SSE body.
    async fn say(&mut self, rig: &Rig, prompt: &str) -> String {
        self.history.push(as_value(user_message(prompt)));
        self.send(rig).await
    }

    /// Append an item the *client* produced — an MCP tool's output, or the
    /// result of a synthetic call it dispatched — exactly as Codex appends one.
    fn append(&mut self, item: Value) {
        self.history.push(item);
    }

    async fn send(&mut self, rig: &Rig) -> String {
        let mut body = request_value(&self.cache_key);
        body["input"] = Value::Array(self.history.clone());
        let (status, text) = post(
            &rig.app,
            rig.host,
            "/v1/responses",
            Some(&self.secret),
            &body.to_string(),
            "application/json",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the turn must be admitted: {text}");
        assert!(
            text.contains("event: response.completed"),
            "the turn must complete: {text}"
        );
        self.history.extend(completed_items(&text));
        text
    }
}

/// A Responses request in the shape Codex sends, as a mutable value.
///
/// Serialized from the client's own struct rather than hand-written, for the
/// reason `common::codex::request` exists: a field it adds or renames arrives
/// here without anyone transcribing it. Only `input` is then replaced, because
/// the history a test grows is richer than the builder's argument type — it
/// holds items a client produced as well as ones it authored.
fn request_value(cache_key: &str) -> Value {
    serde_json::to_value(request(cache_key, Vec::new())).expect("the request encodes")
}

fn as_value(item: codex_protocol::models::ResponseItem) -> Value {
    serde_json::to_value(item).expect("a Codex item serializes")
}

/// Every item a response announced as finished, in order.
fn completed_items(sse: &str) -> Vec<Value> {
    sse.split("\n\n")
        .filter_map(|block| {
            let data = block.lines().find_map(|line| line.strip_prefix("data: "))?;
            let payload: Value = serde_json::from_str(data).ok()?;
            (payload["type"] == "response.output_item.done").then(|| payload["item"].clone())
        })
        .collect()
}

/// One MCP `tools/call`, as a client on the streamable-HTTP transport sends it.
///
/// Raw JSON-RPC over the real adapter rather than through an SDK client: the
/// claims below are about what *this deployment* answers — a status code for an
/// admin key, a 405 for a stream nobody offers — and a client library's job is
/// to turn those into one uniform outcome.
async fn tools_call(rig: &Rig, secret: Option<&str>, tool: &str, arguments: Value) -> Value {
    let (status, text) = post(
        &rig.app,
        rig.host,
        "/mcp",
        secret,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        })
        .to_string(),
        "application/json",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "`{tool}` should have been served: {text}"
    );
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("`{tool}` answered `{text}`: {error}"))
}

/// The one text block a served tool result carries, parsed.
///
/// Every assertion below goes through here rather than reading `content[0]`
/// itself, so "exactly one text block, and it is JSON" is checked on every
/// single call this suite makes rather than once in a test named for it.
fn served(reply: &Value) -> Value {
    let result = &reply["result"];
    assert_eq!(
        result["isError"],
        json!(false),
        "the tool refused: {result}"
    );
    let content = result["content"]
        .as_array()
        .unwrap_or_else(|| panic!("a tool result carries a content array: {result}"));
    assert_eq!(content.len(), 1, "exactly one block: {result}");
    assert_eq!(content[0]["type"], json!("text"));
    assert!(
        result.get("structuredContent").is_none(),
        "structured output would take a different path through the \
         canonicalizer than the text block does: {result}"
    );
    serde_json::from_str(content[0]["text"].as_str().expect("a text block"))
        .expect("every tool renders its answer as JSON inside the one block")
}

/// The refusal a tool answered with, as the agent reads it.
fn refused(reply: &Value) -> String {
    let result = &reply["result"];
    assert_eq!(
        result["isError"],
        json!(true),
        "this tool was expected to refuse: {result}"
    );
    result["content"][0]["text"]
        .as_str()
        .expect("a refusal is a text block")
        .to_string()
}

/// One POST, body drained to a string.
async fn post(
    app: &Router,
    host: &str,
    uri: &str,
    secret: Option<&str>,
    body: &str,
    content_type: &str,
) -> (StatusCode, String) {
    let (status, _headers, text) =
        post_with_extra_headers(app, host, uri, secret, body, content_type, &[]).await;
    (status, text)
}

/// [`post`], but the response headers are kept rather than discarded, and the
/// caller may attach extra request headers of its own — the two things
/// `post` cannot answer for a test that inspects the transport's session
/// framing rather than a tool's own JSON-RPC reply.
async fn post_with_extra_headers(
    app: &Router,
    host: &str,
    uri: &str,
    secret: Option<&str>,
    body: &str,
    content_type: &str,
    extra_headers: &[(HeaderName, &str)],
) -> (StatusCode, HeaderMap, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(CONTENT_TYPE, content_type)
        // Every HTTP/1.1 client sends one, and the streamable-HTTP transport's
        // DNS-rebinding guard refuses a request that does not — a `tower`
        // oneshot has to supply what a socket would have.
        .header(HOST, host)
        // Both, because the transport requires a client to accept either
        // framing even when this deployment only ever answers in one of them.
        .header(ACCEPT, "application/json, text/event-stream");
    if let Some(secret) = secret {
        builder = builder.header(AUTHORIZATION, format!("Bearer {secret}"));
    }
    let mut request = builder.body(Body::from(body.to_string())).expect("request");
    // `insert` rather than the builder's `header`, which appends. A caller that
    // names a header this fixture already set means to *replace* it — the
    // rebinding probes name `Host` — and an appended second value would be
    // silently ignored by a reader that takes the first, which is how a probe
    // for a guard ends up proving nothing.
    for (name, value) in extra_headers {
        request.headers_mut().insert(
            name.clone(),
            HeaderValue::from_str(value).expect("a header value fixture"),
        );
    }
    let response = app.clone().oneshot(request).await.expect("call");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

// ---------------------------------------------------------------------------
// Reading the log
// ---------------------------------------------------------------------------

async fn events(store: &MemoryStore, session: &str) -> Vec<roundhouse_core::event::SessionEvent> {
    store
        .read_events(&SessionId::new(session), 0, 4096)
        .await
        .unwrap_or_else(|error| panic!("session `{session}` should exist: {error}"))
}

/// Every routing decision one session recorded, in log order.
async fn decisions(store: &MemoryStore, session: &str) -> Vec<DecisionRecord> {
    events(store, session)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .collect()
}

async fn items(store: &MemoryStore, session: &str) -> Vec<Item> {
    events(store, session)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ItemAppended { item } => Some(item),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// The tool list, as the surface declares it. Pinned here as well as in the
/// crate's own golden test because this is the only place it is read off the
/// wire the adapter actually writes.
const TOOL_NAMES: [&str; 8] = [
    "status",
    "init_session",
    "declare_intent",
    "prefer",
    "set_quality_floor",
    "fetch_steer",
    "report_outcome",
    "explain_last_route",
];

async fn tools_list(rig: &Rig, secret: Option<&str>) -> (StatusCode, String) {
    post(
        &rig.app,
        rig.host,
        "/mcp",
        secret,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string(),
        "application/json",
    )
    .await
}

#[tokio::test]
async fn an_unauthenticated_tools_call_is_refused() {
    let rig = rig(control_plane()).await;
    let (status, text) = tools_list(&rig, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        text.contains("missing_key"),
        "the surface answers in the same error vocabulary as the turn routes, \
         so a client needs one parser and not two: {text}"
    );

    // The control: the same request with a real turn key is served, which is
    // what makes the refusal about the key rather than about the request — and
    // what proves the adapter publishes the list the surface declares.
    let (status, text) = tools_list(&rig, Some(&key("ada"))).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let reply: Value = serde_json::from_str(&text).expect("a JSON-RPC reply");
    let names: Vec<&str> = reply["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a named tool"))
        .collect();
    assert_eq!(names, TOOL_NAMES.to_vec());
}

#[tokio::test]
async fn an_admin_key_cannot_call_the_mcp_surface() {
    let rig = rig(control_plane()).await;
    let (status, text) = tools_list(&rig, Some(&admin_key("root"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        text.contains("wrong_key_kind"),
        "an admin acts on the deployment and has no membership whose routing \
         it could narrow, which is the same row the turn routes refuse it \
         under: {text}"
    );

    // The control: a turn key is not refused, so the 403 is about the *kind* of
    // key rather than about this route rejecting keys in general.
    let (status, _) = tools_list(&rig, Some(&key("ada"))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_get_on_the_mcp_endpoint_is_405() {
    let rig = rig(control_plane()).await;
    let response = rig
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/mcp")
                .header(ACCEPT, "text/event-stream")
                .header(HOST, rig.host)
                .header(AUTHORIZATION, format!("Bearer {}", key("ada")))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call");
    assert_eq!(
        response.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "this deployment offers no stream — §1 of the plan established that \
         nothing we could push would reach the model — and the specification \
         permits a server that offers none to refuse the GET"
    );

    // The control: the POST the same client falls back to is served, so the 405
    // is a statement about the method rather than about the route.
    let (status, _) = tools_list(&rig, Some(&key("ada"))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_mcp_transport_issues_no_session_id_and_requires_none() {
    // The other half of the statelessness claim `transport.rs`'s module doc
    // makes in prose: `NeverSessionManager` plus `legacy_session_mode: false`
    // means no `Mcp-Session-Id` is minted on a served response, and none is
    // required on a request. Neither half was ever inspected — `post`'s
    // callers only ever looked at status and body — so a regression in that
    // configuration (the session manager swapped, or `legacy_session_mode`
    // flipped back on) would change no test's outcome.
    let rig = rig(control_plane()).await;
    // `status` is session-scoped; give the key a conversation to answer about
    // before asking, exactly as the overlay tests do.
    let mut agent = Conversation::new(&key("ada"), "main");
    agent.say(&rig, "start the parser").await;

    let (status, headers, text) = post_with_extra_headers(
        &rig.app,
        rig.host,
        "/mcp",
        Some(&key("ada")),
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "status", "arguments": {} },
        })
        .to_string(),
        "application/json",
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        !headers.contains_key("mcp-session-id"),
        "a stateless server must not mint a session id on a served response: {headers:?}"
    );

    // A request that supplies one anyway — a bogus id nobody minted — is
    // served exactly the same, not rejected and not treated as identifying a
    // session: the header is ignored, which is the only honest reading of
    // "not required" for a transport that never issues one to begin with.
    let (status_with_bogus, headers_with_bogus, text_with_bogus) = post_with_extra_headers(
        &rig.app,
        rig.host,
        "/mcp",
        Some(&key("ada")),
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "status", "arguments": {} },
        })
        .to_string(),
        "application/json",
        &[(
            HeaderName::from_static("mcp-session-id"),
            "not-a-real-session",
        )],
    )
    .await;
    assert_eq!(status_with_bogus, StatusCode::OK, "{text_with_bogus}");
    assert!(!headers_with_bogus.contains_key("mcp-session-id"));
    assert_eq!(
        served(&serde_json::from_str(&text).unwrap()),
        served(&serde_json::from_str(&text_with_bogus).unwrap()),
        "a session id nobody minted must not change what the call answers"
    );
}

// ---------------------------------------------------------------------------
// The overlay reaches the router
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_overlay_set_by_the_agent_changes_the_next_turns_policy_digest() {
    // THE milestone. Turn one runs under the admission policy the key resolved
    // to; the agent then asks — through the MCP surface, on its own HTTP
    // request — to be kept local; turn two is routed under the narrowed policy.
    //
    // The observable is the *audit trail*, not the tool's answer. A surface that
    // stored an overlay nothing read would still answer `narrowed: false` and
    // list the local target, and every assertion made against the tool alone
    // would pass. `turn_policy_digest` is written by the engine at the moment it
    // records the decision, so it can only move if the overlay actually reached
    // the policy the router was handed.
    let rig = rig(control_plane()).await;
    let session = "acme/ada/main";
    let mut agent = Conversation::new(&key("ada"), "main");

    let first = agent.say(&rig, "start the parser").await;
    assert!(
        first.contains(FRONTIER_ANSWER),
        "left alone this fixture routes to the free hosted model: {first}"
    );

    let answer = served(
        &tools_call(
            &rig,
            Some(&key("ada")),
            "prefer",
            json!({
                "mode": "local",
                "scope": "session",
                "turns": 3,
                "reason": "the rest of this task is mechanical",
                "conversation": "main",
            }),
        )
        .await,
    );
    assert_eq!(
        answer["narrowed"],
        json!(false),
        "this key may be routed locally, so the ask is honored in full: {answer}"
    );
    assert_eq!(
        answer["admissible_targets"],
        json!(["local/local"]),
        "and what is left is what the agent asked for: {answer}"
    );

    let second = agent
        .say(&rig, "keep going, and mind the parser tests")
        .await;
    assert!(
        second.contains(LOCAL_ANSWER),
        "the second turn must have been served locally: {second}"
    );

    let decisions = decisions(&rig.store, session).await;
    assert_eq!(
        decisions.len(),
        2,
        "two turns, two routing decisions, one session — a forked session would \
         show up here as one"
    );
    assert_eq!(decisions[0].chosen.policy_identity(), "anthropic/claude");
    assert_eq!(decisions[1].chosen.policy_identity(), "local/local");
    assert_ne!(
        decisions[0].turn_policy_digest, decisions[1].turn_policy_digest,
        "the audit trail is where an overlay becomes checkable: a digest that \
         did not move is an overlay the router never saw"
    );
    assert_eq!(
        answer["policy_digest"], decisions[1].turn_policy_digest,
        "and the digest the tool promised the agent is the digest the next turn \
         was actually routed under — the one assertion that stops the tool and \
         the engine from being two opinions"
    );
}

#[tokio::test]
async fn an_overlay_cannot_widen_what_the_key_may_do() {
    // The plan's own example: `prefer frontier` on a local-only project. The
    // ask cannot be honored — this key may reach nothing hosted — and the rule
    // is that it is *reported*, never applied and never refused. Applying it
    // would empty the admissible set and fail every remaining turn of the
    // session at a seam the agent cannot reach to undo.
    let rig = rig(control_plane()).await;
    let session = "ourown/bob/main";
    let mut agent = Conversation::new(&key("bob"), "main");

    agent.say(&rig, "start").await;

    let answer = served(
        &tools_call(
            &rig,
            Some(&key("bob")),
            "prefer",
            json!({
                "mode": "frontier",
                "scope": "session",
                "reason": "this looks hard",
                "conversation": "main",
            }),
        )
        .await,
    );
    assert_eq!(
        answer["narrowed"],
        json!(true),
        "an over-ask is reported: an agent that gets an error has to guess, and \
         an agent that guesses asks again: {answer}"
    );
    assert_eq!(
        answer["admissible_targets"],
        json!(["local/local"]),
        "and it is left routable: {answer}"
    );

    agent.say(&rig, "keep going").await;
    let after = decisions(&rig.store, session).await;
    assert_eq!(after.len(), 2);
    assert_eq!(
        after[0].turn_policy_digest, after[1].turn_policy_digest,
        "nothing moved, which is the whole point: an ask that would widen \
         changes no policy at all"
    );
    for decision in &after {
        assert_eq!(
            decision.chosen.policy_identity(),
            "local/local",
            "and both turns still serve"
        );
    }
}

/// A member's own provider key, and the variable it lives in.
///
/// A credential resolves at *boot*, from a variable named in the file, so a
/// suite with a credential-gated fixture has to have one set.
/// `std::env::set_var` is unsound beside a concurrent `std::env::var` and
/// `cargo test` runs these on many threads, so the write happens inside one
/// [`LazyLock`] initializer — the same discipline `credential_gating.rs` runs
/// under, and for the same reason.
const MEMBER_KEY_VAR: &str = "ROUNDHOUSE_TEST_MCP_MEMBER_KEY";
const MEMBER_KEY: &str = "sk-ant-api03-MCPSURFACE0000-the-member-pays";

static ENV: LazyLock<()> = LazyLock::new(|| {
    // SAFETY: this closure runs exactly once and `LazyLock` blocks every other
    // thread inside `force` until it returns. Every read of this variable in
    // this binary is downstream of the `force` in `credential_gated_plane`, and
    // nothing unsets or rewrites it afterwards.
    unsafe {
        std::env::set_var(MEMBER_KEY_VAR, MEMBER_KEY);
    }
});

/// One project that gates on credentials, and one that forwards the caller's.
///
/// `byok/ada` has attached a key for the catalog's one hosted provider and
/// `byok/bob` has not — the same project, the same policy, the same catalog,
/// and the only difference between the two memberships is a credential. That is
/// what makes an assertion about what each is *told* an assertion about the
/// credential gate rather than about policy.
///
/// `seat/cleo` forwards: it holds no key and never will, because its credential
/// arrives with a turn. What the surface should say about it is the opposite of
/// what it says about `bob`, which is why it is in the same fixture.
fn credential_gated_plane() -> Arc<ControlPlane> {
    LazyLock::force(&ENV);
    let json = json!({
        "projects": [
            { "id": "byok", "credentials": { "mode": "user_only" } },
            { "id": "seat", "credentials": { "mode": "pass_through" } },
        ],
        "users": [{ "id": "ada" }, { "id": "bob" }, { "id": "cleo" }],
        "keys": [
            {
                "project": "byok", "user": "ada", "key_sha256": sha256_hex(&key("ada")),
                "credentials": { "providers": { "anthropic": { "env_var": MEMBER_KEY_VAR } } },
            },
            { "project": "byok", "user": "bob", "key_sha256": sha256_hex(&key("bob")) },
            { "project": "seat", "user": "cleo", "key_sha256": sha256_hex(&key("cleo")) },
        ],
    })
    .to_string();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "mcp-surface credential fixture")
            .expect("the fixture config must validate"),
    ))
}

/// What `status` says this key's turns could be routed to.
async fn admissible(rig: &Rig, secret: &str) -> Value {
    served(&tools_call(rig, Some(secret), "status", json!({})).await)["admissible_targets"].clone()
}

/// A `prefer frontier` ask, which is the overlay that needs a hosted target to
/// be honorable at all.
async fn prefer_frontier(rig: &Rig, secret: &str) -> Value {
    served(
        &tools_call(
            rig,
            Some(secret),
            "prefer",
            json!({
                "mode": "frontier",
                "scope": "session",
                "reason": "this looks hard",
                "conversation": "main",
            }),
        )
        .await,
    )
}

#[tokio::test]
async fn status_and_the_overlay_guard_withhold_a_target_the_key_cannot_authenticate_to() {
    // The engine gained a credential filter in M7 and this read never saw it,
    // so the control surface answered a strictly wider question than the router
    // does: `status` named a hosted target to a member holding no key for it,
    // and `prefer`'s guard waved a narrowing onto that provider through to a
    // turn the router would then withhold it from.
    let rig = rig(credential_gated_plane()).await;

    // A turn each, so `status` has a conversation to resolve — and so the
    // router's own answer is on the record beside the surface's.
    let mut with_key = Conversation::new(&key("ada"), "main");
    with_key.say(&rig, "start").await;
    let mut without_key = Conversation::new(&key("bob"), "main");
    without_key.say(&rig, "start").await;

    // PROBE: `bob` holds no key for `anthropic`, so no turn of his can ever be
    // routed there. Naming it is a promise the next turn breaks.
    assert_eq!(
        admissible(&rig, &key("bob")).await,
        json!(["local/local"]),
        "a member with no credential is told what the router would actually give them"
    );

    // CONTROL: the same project, the same policy, the same catalog. The only
    // difference is a key, so the omission above is the credential's doing.
    assert_eq!(
        admissible(&rig, &key("ada")).await,
        json!(["anthropic/claude", "local/local"]),
    );

    // And the router agrees with both, which is the point of intersecting the
    // same two predicates it does.
    assert_eq!(
        decisions(&rig.store, "byok/bob/main").await[0]
            .chosen
            .policy_identity(),
        "local/local",
    );
    assert_eq!(
        decisions(&rig.store, "byok/ada/main").await[0]
            .chosen
            .policy_identity(),
        "anthropic/claude",
    );

    // PROBE: the overlay guard reads the same answer. `prefer frontier` on a
    // key that can authenticate to nothing hosted is an ask that would leave
    // the session with nothing routable, so it is reported rather than applied
    // — never refused, and never honored into a turn that then fails.
    let over_ask = prefer_frontier(&rig, &key("bob")).await;
    assert_eq!(over_ask["narrowed"], json!(true), "{over_ask}");
    assert!(
        over_ask["narrowed_because"].is_string(),
        "an agent that is told `narrowed` without being told why has to guess: {over_ask}"
    );
    assert_eq!(
        over_ask["admissible_targets"],
        json!(["local/local"]),
        "and the session is left routable: {over_ask}"
    );

    // CONTROL: `ada`'s identical ask is honored in full.
    let honored = prefer_frontier(&rig, &key("ada")).await;
    assert_eq!(honored["narrowed"], json!(false), "{honored}");
    assert_eq!(honored["admissible_targets"], json!(["anthropic/claude"]));
}

#[tokio::test]
async fn a_pass_through_key_is_shown_the_hosted_target_its_next_turn_may_still_present_a_seat_for()
{
    // The one place the surface and the boot check both answer optimistically,
    // and it is the same argument in both: a forwarding project's credential is
    // a property of a *request*, and an MCP call is not the turn. Answering
    // `reaches` here would tell every pass-through agent it can reach nothing
    // hosted — on a deployment where every one of its turns can — because the
    // configured resolution has presented nothing yet.
    let rig = rig(credential_gated_plane()).await;
    let mut agent = Conversation::new(&key("cleo"), "main");
    agent.say(&rig, "start").await;

    assert_eq!(
        admissible(&rig, &key("cleo")).await,
        json!(["anthropic/claude", "local/local"]),
        "a forwarding project is shown what a turn carrying a seat could reach"
    );

    // And this really is the optimistic answer rather than an ungated one: the
    // turn above presented no seat, so the router withheld the provider. The
    // two disagree here on purpose, and the marker is what says so.
    let decision = &decisions(&rig.store, "seat/cleo/main").await[0];
    assert_eq!(decision.chosen.policy_identity(), "local/local");
    assert_eq!(decision.withheld_providers, vec!["anthropic".to_string()]);
}

#[tokio::test]
async fn an_expired_scope_restores_the_admission_policy() {
    // An overlay is a ration as well as a narrowing: one turn, one turn's worth
    // spent. What this pins is that the ration runs out on its own — an overlay
    // that never expired would be an agent able to pin its own routing for the
    // life of a session with one call.
    let rig = rig(control_plane()).await;
    let session = "acme/ada/main";
    let mut agent = Conversation::new(&key("ada"), "main");

    agent.say(&rig, "start").await;
    served(
        &tools_call(
            &rig,
            Some(&key("ada")),
            "prefer",
            json!({
                "mode": "local",
                "scope": "turn",
                "reason": "just this one",
                "conversation": "main",
            }),
        )
        .await,
    );
    agent.say(&rig, "the narrowed turn").await;
    agent.say(&rig, "the turn after it").await;

    let decisions = decisions(&rig.store, session).await;
    let routes: Vec<String> = decisions
        .iter()
        .map(|decision| decision.chosen.policy_identity())
        .collect();
    assert_eq!(
        routes,
        vec!["anthropic/claude", "local/local", "anthropic/claude"],
        "one turn's ration, one turn"
    );
    assert_eq!(
        decisions[0].turn_policy_digest, decisions[2].turn_policy_digest,
        "and what it returns to is the admission policy itself, not some third \
         thing: a spent overlay is an absence rather than a record of zeroes"
    );
    assert_ne!(
        decisions[1].turn_policy_digest,
        decisions[0].turn_policy_digest
    );

    // And the surface agrees the overlay is gone rather than reporting one it
    // no longer has.
    let status = served(
        &tools_call(
            &rig,
            Some(&key("ada")),
            "status",
            json!({ "conversation": "main" }),
        )
        .await,
    );
    assert!(
        status["overlay"].is_null(),
        "a spent overlay reads as no overlay: {status}"
    );
}

#[tokio::test]
async fn a_steered_turn_does_not_spend_the_agents_ration() {
    // "One turn, one ration" is a claim about *routed* turns, and the doc on
    // `Engine::narrowed_admission` says why in the strongest available form:
    // consuming where the turn's policy is fixed makes "the turn routed under
    // the overlay" and "the turn that spent it" the same turn by construction.
    // A steered turn is the case where that construction has nothing to hold
    // onto — the interjection seam answers the turn before `plan` runs, so no
    // `Routed` event, no `DecisionRecord` and no `turn_policy_digest` are
    // written at all. Spending the ration there charges the agent for a turn
    // that produced nothing to check the charge against, and `status` had
    // already promised the digest it reported would be "the same string the
    // next `DecisionRecord` will carry".
    let rig = steering_rig(control_plane(), [Plan::Proceed, Plan::Steer, Plan::Proceed]).await;
    let session = "acme/ada/main";
    let mut agent = Conversation::new(&key("ada"), "main");

    agent.say(&rig, "start the parser").await;
    let asked = served(
        &tools_call(
            &rig,
            Some(&key("ada")),
            "prefer",
            json!({
                "mode": "local",
                "scope": "turn",
                "reason": "just the next one",
                "conversation": "main",
            }),
        )
        .await,
    );

    // The steered turn. It commits its correction as the answer and never
    // reaches the router.
    agent.say(&rig, "keep editing whatever").await;
    assert_eq!(
        decisions(&rig.store, session).await.len(),
        1,
        "the premise: the steer really did answer at the seam, so this turn \
         wrote no decision the ration could be checked against"
    );

    // And the surface still holds the overlay, because nothing routed under it.
    let mid = served(
        &tools_call(
            &rig,
            Some(&key("ada")),
            "status",
            json!({ "conversation": "main" }),
        )
        .await,
    );
    assert!(
        !mid["overlay"].is_null(),
        "a turn that never asked the router what it may do cannot have spent a \
         turn's worth of asking: {mid}"
    );
    assert_eq!(
        mid["policy_digest"], asked["policy_digest"],
        "and the digest the tool keeps promising is still the one it promised \
         before the steer"
    );

    // The agent reads the correction and carries on, exactly as Codex does with
    // any other answer, and the next turn routes — under the overlay, which is
    // what the agent asked for and has not yet had. **No tool result is
    // appended, and that is the M10.0 difference**: the client answered nothing,
    // because it was handed an answer rather than a call to run.
    agent.append(as_value(assistant_message(STEER_GUIDANCE)));
    agent.say(&rig, "back to the parser, then").await;

    let routed = decisions(&rig.store, session).await;
    let routes: Vec<String> = routed
        .iter()
        .map(|decision| decision.chosen.policy_identity())
        .collect();
    assert_eq!(
        routes,
        vec!["anthropic/claude", "local/local"],
        "the ration was spent by the turn that was routed under it, and by no \
         other"
    );
    assert_eq!(
        asked["policy_digest"], routed[1].turn_policy_digest,
        "which is the promise `status` and `prefer` both make: the digest \
         reported is the digest the next `DecisionRecord` carries"
    );

    // And it is a ration still, not a subscription: the turn after the routed
    // one is back at the ceiling.
    agent.say(&rig, "and the turn after that").await;
    let after = decisions(&rig.store, session).await;
    assert_eq!(after.len(), 3);
    assert_eq!(
        after[2].chosen.policy_identity(),
        "anthropic/claude",
        "moving the consume must not have made the overlay outlive its scope"
    );
    assert_eq!(
        after[0].turn_policy_digest, after[2].turn_policy_digest,
        "and what it returns to is the admission policy itself"
    );
}

// ---------------------------------------------------------------------------
// The surface does not queue behind the turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_mcp_call_during_a_running_turn_does_not_take_the_session_gate() {
    // The property the whole surface rests on: every tool is a pure read of
    // committed state or a write to the node-local control store, and none of
    // them appends to a session log. If any did, it would need the lease, and
    // an agent calling `status` mid-turn would block until its own turn
    // finished — which is precisely when an agent wants to ask.
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let rig = build(
        control_plane(),
        Arc::new(GatedFrontierClient {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }),
        [],
    )
    .await;

    // Turn one runs to completion so there is a conversation to ask about; then
    // turn two is held inside the provider call, with the session gate taken and
    // the lease renewing.
    release.notify_one();
    let mut agent = Conversation::new(&key("ada"), "main");
    agent.say(&rig, "start").await;

    let mut held_history = agent.history.clone();
    held_history.push(as_value(user_message("and keep going")));
    let held = tokio::spawn({
        let app = rig.app.clone();
        let host = rig.host;
        let secret = key("ada");
        async move {
            let mut body = request_value("main");
            body["input"] = Value::Array(held_history);
            post(
                &app,
                host,
                "/v1/responses",
                Some(&secret),
                &body.to_string(),
                "application/json",
            )
            .await
        }
    });

    tokio::time::timeout(PATIENCE, entered.notified())
        .await
        .expect("the second turn should have reached the provider");

    let answer = tokio::time::timeout(
        PATIENCE,
        tools_call(
            &rig,
            Some(&key("ada")),
            "status",
            json!({ "conversation": "main" }),
        ),
    )
    .await
    .expect("an MCP call must not queue behind a running turn");
    let answer = served(&answer);
    assert_eq!(answer["conversation"], json!("acme/ada/main"));
    assert!(
        !held.is_finished(),
        "the assertion above is only about the gate if the turn was still \
         running when it was made"
    );

    release.notify_one();
    let (status, text) = tokio::time::timeout(PATIENCE, held)
        .await
        .expect("the held turn should finish once released")
        .expect("the turn task");
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        decisions(&rig.store, "acme/ada/main").await.len(),
        2,
        "and the held turn was a turn of the same session the tool answered \
         about — otherwise nothing was contended for"
    );
}

// ---------------------------------------------------------------------------
// The steer, re-readable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_steer_this_deployment_wrote_is_what_fetch_steer_re_reads() {
    // **The round trip M10.0 rebuilt.** It used to be: the interjector supplied
    // a correction, the engine committed a synthetic call to the log and *then*
    // deposited the payload in a node-local store, and the agent fetched it by
    // the id the call named. The correction is the turn's answer now, so there
    // is no id, no deposit and no store -- `fetch_steer` is a fold of the
    // session's own log, and the property worth asserting is that the two agree.
    //
    // What that buys, said plainly because it is the reason the tool was
    // re-purposed rather than removed: the guidance survives a restart, because
    // it never lived anywhere but the log; and the tool is a convenience for an
    // agent that compacted or resumed, not a channel the correction depends on.
    let rig = steering_rig(control_plane(), [Plan::Steer]).await;
    let session = "acme/ada/main";
    let mut agent = Conversation::new(&key("ada"), "main");
    agent.say(&rig, "keep editing whatever").await;

    // The inverse of the old assertion, and the pivot in one line: the
    // correction *is* in the log now, as the item the client was handed.
    let log = serde_json::to_string(&events(&rig.store, session).await).expect("events serialize");
    assert!(
        log.contains(STEER_GUIDANCE),
        "the correction is the turn's answer, so it is in the conversation: {log}"
    );

    let answer = served(
        &tools_call(
            &rig,
            Some(&key("ada")),
            "fetch_steer",
            json!({ "conversation": "main" }),
        )
        .await,
    );
    assert_eq!(answer["conversation"], json!(session));
    assert_eq!(
        answer["guidance"],
        json!(STEER_GUIDANCE),
        "what the agent re-reads is what the decision wrote, byte for byte"
    );

    // Pure: a second call is the same bytes, and did no work to produce them.
    let again = served(
        &tools_call(
            &rig,
            Some(&key("ada")),
            "fetch_steer",
            json!({ "conversation": "main" }),
        )
        .await,
    );
    assert_eq!(
        again, answer,
        "a fetch is a read of the log, not a judge call"
    );

    // Another tenant's key gets the refusal that names nothing -- through
    // `ForeignConversation` now rather than through a principal comparison the
    // tool made for itself, which is T4's "one door" in an assertion.
    let denial = refused(
        &tools_call(
            &rig,
            Some(&key("bob")),
            "fetch_steer",
            json!({ "conversation": session }),
        )
        .await,
    );
    assert!(
        !denial.contains(STEER_GUIDANCE),
        "a refusal that leaked the correction would hand another tenant the \
         contents of the conversation it was refused: {denial}"
    );

    // The advisory write lands, and the agent carrying on extends the session
    // rather than forking it -- the resend that used to be a tool result and is
    // now the guidance item itself.
    let reported = served(
        &tools_call(
            &rig,
            Some(&key("ada")),
            "report_outcome",
            json!({ "conversation": "main", "outcome": "applied", "note": "back on the parser" }),
        )
        .await,
    );
    assert_eq!(reported["recorded"], json!(true));

    agent.append(as_value(assistant_message(STEER_GUIDANCE)));
    agent.say(&rig, "back to the parser, then").await;
    assert_eq!(
        decisions(&rig.store, session).await.len(),
        1,
        "the steered turn routed nowhere and the turn after it routed once, in \
         the same session"
    );
}

// ---------------------------------------------------------------------------
// The correlation trick
// ---------------------------------------------------------------------------

#[tokio::test]
async fn init_session_joins_the_mcp_channel_to_the_wire_session() {
    // An MCP connection cannot carry a conversation id — Codex sources
    // `[mcp_servers.*]` headers from static config — so the correlation runs the
    // other way: the id goes out in a tool *output*, the client appends that
    // output to its conversation, and the next turn's resent history carries it
    // into the log. This is that round trip, with Codex's own item type doing
    // the appending.
    let rig = rig(control_plane()).await;
    let session = "acme/ada/main";
    let mut agent = Conversation::new(&key("ada"), "main");
    agent.say(&rig, "start").await;

    let answer = served(
        &tools_call(
            &rig,
            Some(&key("ada")),
            "init_session",
            json!({ "conversation": "main" }),
        )
        .await,
    );
    let minted = answer["session_binding_id"]
        .as_str()
        .expect("init_session mints an id")
        .to_string();
    assert!(
        answer["note"]
            .as_str()
            .expect("the note is what makes the trick work")
            .contains("Keep this tool output"),
        "a summarizing client drops an id it was not told to keep: {answer}"
    );

    // Before the resend, the log holds nothing to join on. The control that
    // makes the assertion after it about the *resend* rather than about the
    // scanner.
    assert_eq!(
        binding_in_items(&items(&rig.store, session).await),
        None,
        "the id reaches the log only by riding the client's own history"
    );

    // The client appends the tool output as an ordinary conversation item and
    // sends its next turn.
    agent.append(as_value(function_call_output_item(
        "call_mcp_1",
        &serde_json::to_string_pretty(&answer).expect("the tool output re-encodes"),
    )));
    agent.say(&rig, "carry on").await;

    let found = binding_in_items(&items(&rig.store, session).await)
        .expect("the resent history carries the id into the log");
    assert_eq!(
        found,
        BindingId::new(minted),
        "the projection recovers exactly the id this deployment minted"
    );

    // And the join answers the question it exists for: which wire session made
    // that MCP call? Asked with the caller's own `(principal, session)`, which
    // is what makes a token *pasted* into a log inert rather than authoritative
    // — the resolver refuses to answer for anyone but the pair that minted it.
    let ada = Principal::new("acme", "ada");
    let binding = rig
        .control
        .binding(&ada, &SessionId::new(session), &found)
        .expect("this node minted it for this caller, so this node can resolve it");
    assert_eq!(binding.session, SessionId::new(session));
    assert_eq!(binding.principal, ada);
    assert!(
        rig.control
            .binding(
                &Principal::new("acme", "bob"),
                &SessionId::new(session),
                &found
            )
            .is_none(),
        "and it answers nobody else, however they came by the token"
    );

    // The control: a *different* conversation of the same key holds nothing, so
    // the join names one session rather than every session this key has.
    let mut elsewhere = Conversation::new(&key("ada"), "other");
    elsewhere.say(&rig, "unrelated work").await;
    assert_eq!(
        binding_in_items(&items(&rig.store, "acme/ada/other").await),
        None,
        "the id identifies the conversation that appended it and no other"
    );
}

// ---------------------------------------------------------------------------
// Open mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn open_mode_serves_the_mcp_surface_under_the_default_principal() {
    // An unconfigured deployment authenticates nothing, so the control surface
    // has to work there too — and it has to work under the *same* value open
    // mode admits turns as, or an agent would be told about entitlements the
    // turns beside it are not routed under.
    let rig = rig(ControlPlane::open()).await;

    // No key, no namespace: the session is the cache key verbatim.
    let mut agent = Conversation::new("unused", "main");
    agent.say(&rig, "start").await;

    // `conversation` omitted, which is the path a client that never called
    // `init_session` takes: the principal's most recent conversation.
    let answer = served(&tools_call(&rig, None, "status", json!({})).await);
    assert_eq!(
        answer["conversation"],
        json!("main"),
        "an unconfigured deployment's client-supplied ids keep working: {answer}"
    );
    assert_eq!(
        answer["admissible_targets"],
        json!(["anthropic/claude", "local/local"]),
        "the unrestricted policy admits everything this deployment can reach"
    );
    assert!(
        answer["budget"].is_null(),
        "open mode meters nothing, and a zero would read as an exhausted budget \
         rather than as an absent one: {answer}"
    );

    // And an overlay set with no key at all still reaches the router, which is
    // what makes this a working surface rather than a reachable one.
    served(
        &tools_call(
            &rig,
            None,
            "prefer",
            json!({ "mode": "local", "scope": "session", "reason": "cheap work" }),
        )
        .await,
    );
    agent.say(&rig, "keep going").await;
    let decisions = decisions(&rig.store, "main").await;
    assert_eq!(
        decisions[1].chosen.policy_identity(),
        "local/local",
        "the overlay an unauthenticated agent set was spent by the next turn"
    );

    // And a conversation with no correction in it is refused rather than served
    // an empty payload.
    let denial = refused(&tools_call(&rig, None, "fetch_steer", json!({})).await);
    assert!(
        denial.contains("no correction"),
        "a conversation roundhouse has never steered is refused rather than \
         served empty guidance, in open mode exactly as in a configured one: \
         {denial}"
    );
}

#[tokio::test]
async fn open_mode_refuses_a_tools_call_from_a_host_it_does_not_serve() {
    // The DNS-rebinding guard rmcp ships exists for exactly this deployment: a
    // process on 127.0.0.1:8080 with no key, which is what the shipped binary is
    // whenever `ROUNDHOUSE_CONTROL_PLANE` is unset. A page in the developer's
    // browser cannot read a loopback response cross-origin, so the attack is to
    // point a hostname it *does* control at 127.0.0.1 and re-resolve it — the
    // browser then believes it is same-origin, sends no `Authorization`, and the
    // only header that still tells the truth is `Host`.
    //
    // The tools are not read-only: `prefer` and `set_quality_floor` write
    // overlays against the developer's live conversation, and `status` reports
    // the admissible fleet and the budget position. `allowed_origins` cannot
    // stand in — under rebinding the `Origin` is the attacker's own page and
    // reads as same-origin — so the host allowlist is the only check that fires.
    let open = rig(ControlPlane::open()).await;
    let mut agent = Conversation::new("unused", "main");
    agent.say(&open, "start").await;

    let (status, _headers, text) = post_with_extra_headers(
        &open.app,
        open.host,
        "/mcp",
        None,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "status", "arguments": {} },
        })
        .to_string(),
        "application/json",
        &[(HOST, REBOUND_HOST)],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an open deployment has no bearer key standing in for the loopback \
         guard, so a request from a host it does not serve must not reach a \
         tool: {text}"
    );
    assert!(
        !text.contains("admissible_targets"),
        "and it must not answer with the deployment's own posture on the way \
         out: {text}"
    );

    // The control, and the reason the assertion above is about the *host*: the
    // identical request from loopback is served, with no key, exactly as
    // `open_mode_serves_the_mcp_surface_under_the_default_principal` requires.
    let (served_status, _, served_text) = post_with_extra_headers(
        &open.app,
        open.host,
        "/mcp",
        None,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "status", "arguments": {} },
        })
        .to_string(),
        "application/json",
        &[],
    )
    .await;
    assert_eq!(served_status, StatusCode::OK, "{served_text}");

    // The other control: a *configured* deployment does clear the allowlist,
    // because it is served behind whatever hostname an operator gave it and the
    // key is what a rebound page cannot supply. The same attacker host with a
    // real key is served; without one it is refused by the key check rather than
    // by the host check, which is what makes the key the replacement rather than
    // an addition.
    let keyed = rig(control_plane()).await;
    let mut ada = Conversation::new(&key("ada"), "main");
    ada.say(&keyed, "start").await;
    let (keyed_status, _, keyed_text) = post_with_extra_headers(
        &keyed.app,
        REBOUND_HOST,
        "/mcp",
        Some(&key("ada")),
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "status", "arguments": {} },
        })
        .to_string(),
        "application/json",
        &[],
    )
    .await;
    assert_eq!(
        keyed_status,
        StatusCode::OK,
        "a configured deployment answers to any hostname an operator put in \
         front of it: {keyed_text}"
    );
    let (unkeyed_status, _, _) = post_with_extra_headers(
        &keyed.app,
        REBOUND_HOST,
        "/mcp",
        None,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "status", "arguments": {} },
        })
        .to_string(),
        "application/json",
        &[],
    )
    .await;
    assert_eq!(unkeyed_status, StatusCode::UNAUTHORIZED);
}
