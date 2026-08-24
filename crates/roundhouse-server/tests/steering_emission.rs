// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M4 of `PLAN-agentic-control-plane.md`: emitting a synthetic tool call, and
//! surviving the client's resend of it.
//!
//! The milestone is one primitive and its consequences. A turn can complete
//! carrying a `function_call` this deployment invented instead of running the
//! completion the client asked for; the client dispatches that call, and comes
//! back next turn with the call *and* its output in the history it re-sends.
//! Everything here is about that round trip: that Codex's own parser sees a
//! `FunctionCall` rather than silently dropping an item it cannot read, that
//! the four frames are the only four, that the resend extends the session
//! instead of forking it, and that none of it books a model row or opens a
//! grant — because nothing was dispatched.
//!
//! **What stands in for M6.** The trigger, the judge and the action map are
//! not built yet, so the decision to steer is made by [`TestInterjector`], a
//! test-only occupant of the *real* seam
//! ([`roundhouse_core::interject::Interjector`]) that M6's validator will
//! occupy unchanged. It is a scripted queue rather than anything clever on
//! purpose: what this suite is about is what happens *after* something decides
//! to steer, and a decision procedure with opinions of its own would make
//! every assertion below partly about the decision.
//!
//! **Why these are integration tests.** Every claim spans the seam, the log,
//! the projection and a real client parser at once. The unit tests one layer
//! down already prove that `complete_with_item` commits one batch and that
//! `suffix_after` admits a suffix; what they cannot prove is that the item a
//! session committed comes back out of the wire as an item Codex will
//! dispatch, and re-enters as the same canonical item it went in as.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use tower::ServiceExt;

use codex_api::{ApiError, ResponseEvent, ResponsesApiRequest, ResponsesClient, ResponsesOptions};
use codex_protocol::models::{ContentItem, ResponseItem};
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    Balance, BalanceQuery, Grant, GrantRequest, MemorySpendLedger, Settled, Settlement, SpendError,
    SpendLedger,
};
use roundhouse_core::event::{Accounting, ControlRecord, SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::interject::{Interjection, InterjectionContext, Interjector};
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::metrics::{MetricsConfig, MetricsSnapshot, ShadowPricing};
use roundhouse_core::now_ms;
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::EchoFrontierClient;
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, Conversations, EchoLocalExecutor, Engine, EngineConfig,
    responses_router,
};

mod common;
use common::codex::{
    Frame, NoAuth, RouterTransport, StaticToken, collect, frames, function_call_item,
    function_call_output_item, reasoning_item, request, user_message,
};
use common::{frontier_catalog, sha256_hex};

/// What the echo provider answers an *ordinary* turn with.
const ANSWER: &str = "frontier answer";

/// The neutral tool name the log stores, with no namespace on it.
const STEER_TOOL: &str = "fetch_steer";

/// The namespace the wire projection renders, and the one Codex's exact
/// `ToolName { name, namespace }` lookup would resolve against.
const NAMESPACE: &str = "mcp__roundhouse";

// ---------------------------------------------------------------------------
// The occupant of the seam
// ---------------------------------------------------------------------------

/// What [`TestInterjector`] does with one turn.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Plan {
    /// Answer the turn with a synthetic call instead of running it.
    Steer,
    /// Answer the turn with plain guidance and no call — the degrade path, and
    /// M6's outcome C. Named here because it is the *other* completion shape
    /// the seam admits, and this file is where completion shapes are pinned to
    /// frames.
    Halt,
    /// Leave the turn alone, exactly as the production default does.
    Proceed,
}

/// One emitted steer, as the interjector minted it.
///
/// Kept so a test can play the agent afterwards with the *same* bytes the
/// server emitted, rather than with its own reconstruction of them — which is
/// the whole point of minting the arguments once and storing them in the item.
#[derive(Clone, Debug)]
struct Steer {
    call_id: String,
    arguments: String,
    /// The correction itself, which the engine deposits into the control store
    /// for `fetch_steer` to serve. Never on the wire and never in the log: what
    /// the client is handed is the call, and the payload is fetched separately.
    guidance: String,
}

/// The steer this response would carry.
///
/// Minted from the [`ResponseId`], which is what makes two steers unable to
/// collide and a steer that no emitted call named impossible to name.
fn mint(response_id: &ResponseId) -> Steer {
    let call_id = format!("rhsteer_{response_id}");
    let arguments = serde_json::json!({ "steer_id": call_id }).to_string();
    Steer {
        call_id,
        arguments,
        guidance: STEER_GUIDANCE.to_string(),
    }
}

/// What the correction says.
///
/// Distinctive on purpose: an assertion that this text reached an agent through
/// `fetch_steer` and never through the wire is an assertion about a literal.
const STEER_GUIDANCE: &str = "you are editing a file the task did not name; go back to the parser";

/// What a halt says.
///
/// Distinctive on purpose, for the same reason [`STEER_GUIDANCE`] is: an
/// assertion that this text reached the client *through the conversation* is an
/// assertion about a literal.
const HALT_GUIDANCE: &str =
    "Stopping here: the last four steps repeated without progress. Re-read the task.";

/// What the interjection reports the turn cost.
///
/// Non-zero and non-round on every axis, so an assertion that the wire carries
/// *this* usage cannot be satisfied by a default, and so the totals-balance
/// check has something to balance.
fn steer_usage() -> Usage {
    Usage {
        input_tokens: 96,
        cached_input_tokens: 32,
        output_tokens: 24,
        reasoning_tokens: 8,
        accounting: Accounting::Reported,
    }
}

/// A scripted occupant of the interjection seam.
///
/// Test-only and living in the test crate on purpose: production's occupant is
/// [`roundhouse_core::interject::production_default`], which decides nothing,
/// and M6's is the validator. Nothing about this type is a branch in the
/// engine — it arrives through the same builder a deployment would use.
struct TestInterjector {
    /// One decision per admitted turn, in order. An exhausted script means
    /// `Proceed`, which is what makes "the turn after the steer" the ordinary
    /// case rather than a second thing to configure.
    script: Mutex<VecDeque<Plan>>,
    /// How many times the seam was consulted, which is the assertion behind
    /// "a retry never re-runs the judge".
    calls: AtomicUsize,
    steers: Mutex<Vec<Steer>>,
}

impl TestInterjector {
    fn new(script: impl IntoIterator<Item = Plan>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into_iter().collect()),
            calls: AtomicUsize::new(0),
            steers: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The one steer emitted so far. Every test here emits at most one.
    fn steer(&self) -> Steer {
        let steers = self.steers.lock().expect("recording mutex");
        assert_eq!(
            steers.len(),
            1,
            "these fixtures emit exactly one steer: {steers:?}"
        );
        steers[0].clone()
    }
}

#[async_trait]
impl Interjector for TestInterjector {
    async fn consider(&self, context: &InterjectionContext<'_>) -> Interjection {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let plan = self
            .script
            .lock()
            .expect("script mutex")
            .pop_front()
            .unwrap_or(Plan::Proceed);
        match plan {
            Plan::Proceed => Interjection::proceed(),
            Plan::Halt => Interjection::Complete {
                // Assistant text, with no response stamp: the stamp is
                // `complete_with_item`'s to put on, exactly as it is for a
                // call. Nothing is deposited for a halt, because there is no
                // call an agent could fetch by — the engine's deposit already
                // answers `None` for this shape.
                item: Item {
                    role: Role::Assistant,
                    content: ItemContent::Text {
                        text: HALT_GUIDANCE.to_string(),
                    },
                    response_id: None,
                },
                usage: steer_usage(),
                guidance: HALT_GUIDANCE.to_string(),
                record: ControlRecord::default(),
            },
            Plan::Steer => {
                let steer = mint(context.response_id);
                self.steers
                    .lock()
                    .expect("recording mutex")
                    .push(steer.clone());
                Interjection::Complete {
                    // The bare name, with no namespace: the log keeps one
                    // spelling per tool and the dialect supplies the rest.
                    item: Item::tool_call(
                        steer.call_id.as_str(),
                        STEER_TOOL,
                        steer.arguments.as_str(),
                    ),
                    usage: steer_usage(),
                    guidance: steer.guidance,
                    // Empty: this double stands in for the interjector, not for
                    // the validate loop behind it, and a steer with no
                    // validation behind it is exactly what M4's assertions are
                    // about.
                    record: ControlRecord::default(),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// A ledger that counts the one call this milestone claims never happens
// ---------------------------------------------------------------------------

/// A [`SpendLedger`] that counts grants, and delegates everything.
///
/// Narrower than `budget_routing.rs`'s three-counter double and deliberately
/// not shared with it: the claim here is about *one* call that must not
/// happen, and a double that also counted settles and reads would invite an
/// assertion on a total that cannot distinguish them.
#[derive(Default)]
struct GrantCountingLedger {
    inner: MemorySpendLedger,
    grants: AtomicUsize,
}

impl GrantCountingLedger {
    fn grants(&self) -> usize {
        self.grants.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SpendLedger for GrantCountingLedger {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        self.grants.fetch_add(1, Ordering::SeqCst);
        self.inner.open_grant(request).await
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        self.inner.settle_grant(settlement).await
    }

    async fn balance(&self, query: BalanceQuery) -> Result<Balance, SpendError> {
        self.inner.balance(query).await
    }
}

// ---------------------------------------------------------------------------
// The deployment under test
// ---------------------------------------------------------------------------

struct Rig {
    app: Router,
    store: Arc<MemoryStore>,
    interjector: Arc<TestInterjector>,
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    /// `Some` for a Configured deployment, whose requests carry a key.
    key: Option<String>,
}

impl Rig {
    /// The deployment's own accounting, folded from the log it just wrote.
    fn metrics(&self) -> MetricsSnapshot {
        self.engine.metrics().snapshot(
            &MetricsConfig::new(ShadowPricing::new(Vec::new())),
            now_ms(),
        )
    }
}

/// An open-mode deployment following `script`.
///
/// No fleet, so every ordinary turn goes to the one priced hosted model and a
/// steered turn is visibly the turn that went nowhere.
fn rig(script: impl IntoIterator<Item = Plan>) -> Rig {
    build(
        ControlPlane::open(),
        script,
        Arc::new(MemorySpendLedger::new()),
        None,
    )
}

fn build(
    plane: Arc<ControlPlane>,
    script: impl IntoIterator<Item = Plan>,
    spend: Arc<dyn SpendLedger>,
    key: Option<String>,
) -> Rig {
    ensure_rustls_crypto_provider();
    let store = Arc::new(MemoryStore::new());
    let interjector = TestInterjector::new(script);
    let engine = Arc::new(
        Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local answer")),
            frontier_catalog(),
            Arc::new(EchoFrontierClient::new(ANSWER)),
            Arc::new(AffinityPolicy::new()),
            EngineConfig::default(),
        )
        .with_spend_ledger(spend)
        .with_interjector(Arc::clone(&interjector) as Arc<dyn Interjector>),
    );
    Rig {
        app: responses_router(
            plane,
            Arc::clone(&engine),
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ),
        store,
        interjector,
        engine,
        key,
    }
}

/// A budgeted, Configured deployment — the only shape in which "no grant was
/// opened" is a claim about behavior rather than about configuration: an
/// admission with no budget never reaches the ledger at all, so an open-mode
/// fixture would report zero grants for a reason that has nothing to do with
/// steering.
fn budgeted_rig(script: impl IntoIterator<Item = Plan>) -> (Rig, Arc<GrantCountingLedger>) {
    let secret = "rh_turn_DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD";
    let json = serde_json::json!({
        "projects": [{
            "id": "acme",
            "budget": { "limit_usd": 100.0, "window": "total", "on_exhaustion": "degrade_to_local" },
        }],
        "users": [{ "id": "ada" }],
        "keys": [{
            "project": "acme", "user": "ada",
            "key_sha256": sha256_hex(secret),
        }],
    })
    .to_string();
    let plane = Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "steering-emission fixture")
            .expect("the fixture config must validate"),
    ));
    let ledger = Arc::new(GrantCountingLedger::default());
    let rig = build(
        plane,
        script,
        Arc::clone(&ledger) as Arc<dyn SpendLedger>,
        Some(secret.to_string()),
    );
    (rig, ledger)
}

// ---------------------------------------------------------------------------
// Driving it
// ---------------------------------------------------------------------------

/// One turn through Codex's own client.
async fn drive(rig: &Rig, request: ResponsesApiRequest) -> Result<Vec<ResponseEvent>, ApiError> {
    let auth: Arc<dyn codex_api::AuthProvider> = match &rig.key {
        Some(key) => Arc::new(StaticToken::new(key.clone())),
        None => Arc::new(NoAuth),
    };
    let client = ResponsesClient::new(
        RouterTransport {
            app: rig.app.clone(),
        },
        common::codex::provider("http://roundhouse.test/v1", "roundhouse-steering"),
        auth,
    );
    collect(
        client
            .stream_request(request, ResponsesOptions::default())
            .await?,
    )
    .await
}

/// The same turn, read as bytes rather than as parsed events.
async fn drive_frames(rig: &Rig, request: ResponsesApiRequest) -> Vec<Frame> {
    let body = serde_json::to_string(&request).expect("the request encodes");
    let mut builder = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header(CONTENT_TYPE, "application/json");
    if let Some(key) = &rig.key {
        builder = builder.header(AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = rig
        .app
        .clone()
        .oneshot(builder.body(Body::from(body)).expect("request"))
        .await
        .expect("call");
    assert_eq!(response.status(), StatusCode::OK);
    frames(response.into_body()).await
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

fn response_id(events: &[ResponseEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            ResponseEvent::Completed { response_id, .. } => Some(response_id.clone()),
            _ => None,
        })
        .expect("a completed turn names its response")
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

/// The frame types, in order — the list an exhaustive assertion compares.
fn kinds(frames: &[Frame]) -> Vec<&str> {
    frames.iter().map(Frame::kind).collect()
}

/// The four frames a steered turn is, and nothing else.
const STEERED_FRAMES: [&str; 4] = [
    "response.created",
    "response.output_item.added",
    "response.output_item.done",
    "response.completed",
];

/// Assert that a session was never re-bound to a fresh generation.
///
/// A fork is silent from the client's side — every turn still answers — so it
/// can only be caught by asking the store whether the generation-one session
/// exists at all.
async fn assert_never_forked(store: &MemoryStore, session: &str) {
    assert!(
        store
            .last_seq(&SessionId::new(format!("{session}#g1")))
            .await
            .is_err(),
        "the resend must have matched its prefix: a `{session}#g1` session \
         exists, which means the prefix check refused the claim and rebound"
    );
}

/// How many frames in a body carry a `function_call` item.
fn frames_carrying_a_call(frames: &[Frame]) -> usize {
    frames
        .iter()
        .filter(|frame| frame.payload["item"]["type"] == "function_call")
        .count()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The whole milestone's premise, checked against the parser that will consume
/// it: a synthetic call arrives as a `FunctionCall` with its namespace in its
/// own field, and **not** as `ResponseItem::Other`.
///
/// `Other` is what an item of an unknown or unparseable shape silently becomes
/// (`codex_conformance.rs` names the same failure for messages). Every
/// sequence-level assertion in this file would still pass with an `Other` in
/// place of the call, and Codex would dispatch nothing at all — so the negative
/// half is the half that matters.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_synthetic_function_call_arrives_as_codex_parses_it() {
    let rig = rig([Plan::Steer]);

    let events = drive(&rig, request("sess-parses", vec![user_message("hello")]))
        .await
        .expect("a steered turn is a completed turn, not a failure");

    let steer = rig.interjector.steer();
    let done: Vec<&ResponseItem> = events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputItemDone(item) => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(
        done.len(),
        1,
        "a steered turn produces exactly one item: {events:#?}"
    );
    match done[0] {
        ResponseItem::FunctionCall {
            name,
            namespace,
            call_id,
            arguments,
            ..
        } => {
            assert_eq!(name, STEER_TOOL, "the wire carries the bare tool name");
            assert_eq!(
                namespace.as_deref(),
                Some(NAMESPACE),
                "the namespace must be a separate field: Codex dispatches on an \
                 exact ToolName {{ name, namespace }} lookup and a flat name \
                 resolves against nothing"
            );
            assert_eq!(call_id, &steer.call_id);
            assert_eq!(
                arguments, &steer.arguments,
                "the arguments are minted once and echoed, never re-serialized"
            );
        }
        other => panic!(
            "the item must parse as FunctionCall, not silently become \
             ResponseItem::Other — an `Other` here is a call Codex will never \
             dispatch, and every other assertion in this suite would still \
             pass: {other:?}"
        ),
    }

    // The announced item is the same call, not an empty message: a client that
    // was told about a message and then handed a call has nowhere to put it.
    let ResponseEvent::OutputItemAdded(added) = &events[1] else {
        panic!("expected an added item second: {:?}", events[1]);
    };
    assert!(
        matches!(added, ResponseItem::FunctionCall { .. }),
        "the added frame must announce the call itself: {added:?}"
    );
}

/// The frames themselves: four, in order, and no others.
///
/// Extends `codex_conformance.rs`'s `ordering_is_enforced_at_the_frame_level`
/// with a new exhaustive list, because the client's parser cannot make this
/// assertion — it ignores what it does not recognize, so a stray
/// `output_text.delta` or a leftover empty `msg_1` item would pass through it
/// unremarked and only show up as an agent that saw an empty answer beside its
/// call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_steered_turn_emits_exactly_four_frames_and_no_others() {
    let rig = rig([Plan::Steer]);

    let frames = drive_frames(&rig, request("sess-frames", vec![user_message("hello")])).await;
    assert_eq!(
        kinds(&frames),
        STEERED_FRAMES,
        "a steered turn is created, added, done, completed — no text deltas \
         (the pinned parser swallows argument deltas, so nothing may be \
         streamed) and no empty message item"
    );
    for frame in &frames {
        assert_eq!(
            frame.name, frame.payload["type"],
            "the SSE event name must be the payload's own type tag"
        );
    }

    let steer = rig.interjector.steer();
    let added = &frames[1].payload["item"];
    let done = &frames[2].payload["item"];
    assert_eq!(added, done, "both frames carry the complete item");

    // The golden shape, asserted whole rather than field by field: an extra
    // field, or a `namespace` folded into `name`, is exactly the drift that
    // would leave Codex's exact lookup with nothing to match.
    let item_id = added["id"].as_str().expect("the item is named");
    assert_eq!(
        *added,
        serde_json::json!({
            "type": "function_call",
            "id": item_id,
            "namespace": NAMESPACE,
            "name": STEER_TOOL,
            "call_id": steer.call_id,
            "arguments": steer.arguments,
        }),
        "the emitted item's shape is the contract with Codex's parser"
    );
    assert!(
        item_id.starts_with("fc_"),
        "a call's item id lives in its own space, never the message space: \
         {item_id}"
    );
    assert_ne!(
        item_id, "msg_1",
        "msg_1 is the assistant message's id and nothing else's"
    );

    // ---- what the completion reports, and what it is a measure of (F03) ----
    //
    // **Not the interjection's usage.** The judge's side call is what this turn
    // *cost*, and the log books exactly that (asserted below). What the wire
    // reports is what this turn contributed to the *client's context*, because
    // that is the question codex asks of `response.completed.usage`: it folds
    // the value into `last_token_usage`, replacing it, and that is what drives
    // auto-compaction and `get_context_remaining`. Reporting the judge's number
    // there told a real client its context had collapsed on the one turn before
    // it resent the whole conversation — measured at 5.0x understated.
    //
    // Recomputed here from the stored items rather than re-using
    // `Engine::admitted_input_tokens`, so the assertion is about the
    // conversation and not about the function under test.
    // [`ByteTokenizer`] is one token per byte, which is what makes the
    // recomputation a `len()`.
    let stored = stored_items(&rig.store, "sess-frames").await;
    let (emitted_item, admitted) = stored
        .split_last()
        .expect("the emitted call is the last item this turn appended");
    let admitted_tokens: u64 = admitted.iter().map(|item| item.render().len() as u64).sum();
    let emitted_tokens = emitted_item.render().len() as u64;

    let usage = &frames[3].payload["response"]["usage"];
    assert_eq!(
        usage["input_tokens"].as_u64(),
        Some(admitted_tokens),
        "the steered turn reports the input this deployment admitted, which is \
         what the client is holding and what the next turn resends"
    );
    assert_eq!(
        usage["output_tokens"].as_u64(),
        Some(emitted_tokens),
        "and the item it emitted, in the prompt encoding the next turn will \
         count that item under"
    );
    assert_eq!(
        usage["total_tokens"].as_u64(),
        Some(admitted_tokens + emitted_tokens),
        "totals must balance on a steered turn exactly as on a served one"
    );
    assert_eq!(
        usage["input_tokens_details"]["cached_tokens"].as_u64(),
        Some(0),
        "nothing was dispatched, so no provider's prefix cache served any of it"
    );
    // The line that would have to go red for the F03 split to be undone: the
    // judge's usage is non-round on every axis precisely so that reporting it
    // by accident is unmistakable.
    let judge = steer_usage();
    assert_ne!(
        usage["input_tokens"].as_u64(),
        Some(judge.input_tokens),
        "the wire must not be reporting the interjection's usage again"
    );

    // ---- and the other half of the split: the log still books the judge -----
    //
    // Byte-identical to what it booked before the wire changed, which is what
    // keeps the dashboard's total equal to the sum of its rows: a steered turn
    // books no model row of its own and the side call books under the judge's.
    let booked = rig
        .store
        .read_events(&SessionId::new("sess-frames"), 0, 1024)
        .await
        .expect("the session exists")
        .into_iter()
        .find_map(|event| match event.kind {
            SessionEventKind::ResponseCompleted { usage, .. } => Some(usage),
            _ => None,
        })
        .expect("the steered turn completed");
    assert_eq!(
        booked, judge,
        "the ledger's number is the interjection's and did not move; only the \
         wire's did"
    );
}

/// The other half of the round trip: the client dispatches the call and comes
/// back with the call *and* its output, and the session extends rather than
/// forks.
///
/// Played with Codex's own types, so the resent `function_call` carries the
/// namespace and the item id our projection put on it — the two fields
/// canonicalization ignores. If it did not ignore them, the claimed prefix
/// would disagree with the stored one and this session would silently rebind
/// to a fresh, cold generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_resent_call_and_its_output_extend_rather_than_fork() {
    let rig = rig([Plan::Steer]);
    let session = "sess-resend";

    drive(&rig, request(session, vec![user_message("hello")]))
        .await
        .expect("the steered turn completes");
    let steer = rig.interjector.steer();

    let after_steer = stored_items(&rig.store, session).await;
    assert_eq!(
        after_steer.len(),
        3,
        "instructions, the question, and the emitted call: {after_steer:#?}"
    );

    // Exactly what the agent sends next: everything it had, plus the call it
    // was handed, plus the output it produced by running it.
    let resent = vec![
        user_message("hello"),
        function_call_item(
            STEER_TOOL,
            Some(NAMESPACE),
            &steer.call_id,
            &steer.arguments,
        ),
        function_call_output_item(&steer.call_id, "slow down and re-read the failure"),
    ];
    let second = drive(&rig, request(session, resent))
        .await
        .expect("the turn fulfilling the steer completes");
    assert!(!response_id(&second).is_empty());

    assert_never_forked(&rig.store, session).await;
    let items = stored_items(&rig.store, session).await;
    assert_eq!(
        items[3].content,
        ItemContent::ToolResult {
            call_id: steer.call_id.clone(),
            output: "slow down and re-read the failure".to_string(),
        },
        "the suffix admitted is exactly the output, and nothing before it: \
         {items:#?}"
    );
    assert_eq!(
        items[3].role,
        Role::Tool,
        "a tool result is the tool's turn to speak"
    );
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(item.content, ItemContent::ToolCall { .. }))
            .count(),
        1,
        "the resent call must be recognized as the prefix it is, not appended \
         a second time: {items:#?}"
    );
    // The call kept its stamp and the result never had one: that asymmetry is
    // what lets a projection tell an emitted call from a client-sent one.
    assert!(items[2].response_id.is_some());
    assert!(items[3].response_id.is_none());
    // And the fifth item is this turn's ordinary answer, which is what proves
    // the turn ran rather than being steered again.
    assert_eq!(
        items[4].content,
        ItemContent::Text {
            text: ANSWER.to_string()
        }
    );
}

/// The N+2 case, where the tool result has moved *inside* the overlap.
///
/// On turn N+1 the result is fresh suffix and never compared; on N+2 it is
/// compared, against the client's own stored copy of it. That is the turn where
/// a canonicalization that renders a tool result differently coming in than it
/// stored going out would fork — and it would fork one turn *after* the one
/// anybody was watching.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_third_turn_after_a_steer_still_matches_its_prefix() {
    let rig = rig([Plan::Steer]);
    let session = "sess-third";

    drive(&rig, request(session, vec![user_message("hello")]))
        .await
        .expect("the steered turn completes");
    let steer = rig.interjector.steer();

    let fulfilled = vec![
        user_message("hello"),
        function_call_item(
            STEER_TOOL,
            Some(NAMESPACE),
            &steer.call_id,
            &steer.arguments,
        ),
        function_call_output_item(&steer.call_id, "understood"),
    ];
    drive(&rig, request(session, fulfilled.clone()))
        .await
        .expect("the fulfilling turn completes");

    let mut third = fulfilled;
    third.push(assistant_message(ANSWER));
    third.push(user_message("and now this"));
    drive(&rig, request(session, third))
        .await
        .expect("the turn after the fulfilment completes");

    assert_never_forked(&rig.store, session).await;
    let items = stored_items(&rig.store, session).await;
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(item.content, ItemContent::ToolCall { .. }))
            .count(),
        1,
        "the call is stored once however many turns re-send it: {items:#?}"
    );
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(item.content, ItemContent::ToolResult { .. }))
            .count(),
        1,
        "and so is its output: {items:#?}"
    );
    assert_eq!(
        items.len(),
        7,
        "instructions, question, call, output, answer, question, answer: \
         {items:#?}"
    );
}

/// An agent that reasons between the call and its output does not fork the
/// session either.
///
/// `reasoning` items are dropped on the way in, on every request alike — which
/// is what keeps the claimed prefix equal to the stored one. Worth its own test
/// because a steered turn is exactly when a real agent reasons: it was handed a
/// tool call it did not ask for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reasoning_item_between_the_call_and_its_output_does_not_fork() {
    let rig = rig([Plan::Steer]);
    let session = "sess-reasoning";

    drive(&rig, request(session, vec![user_message("hello")]))
        .await
        .expect("the steered turn completes");
    let steer = rig.interjector.steer();

    let resent = vec![
        user_message("hello"),
        function_call_item(
            STEER_TOOL,
            Some(NAMESPACE),
            &steer.call_id,
            &steer.arguments,
        ),
        reasoning_item("rs_1"),
        function_call_output_item(&steer.call_id, "understood"),
    ];
    drive(&rig, request(session, resent))
        .await
        .expect("the turn carrying a reasoning item completes");

    assert_never_forked(&rig.store, session).await;
    let items = stored_items(&rig.store, session).await;
    assert_eq!(
        items.len(),
        5,
        "instructions, question, call, output, answer — the reasoning item is \
         dropped rather than stored: {items:#?}"
    );
    assert!(matches!(items[3].content, ItemContent::ToolResult { .. }));
}

/// An identical retry of a steered turn replays it, and never re-enters the
/// seam.
///
/// The seam sits *after* the dedup short-circuit, so whatever the occupant
/// costs — in M6, a paid call to a judge — is spent once per turn id and not
/// once per attempt. Counting the consultations is the only way to see that:
/// the frames alone would look identical either way, because a second steer of
/// the same response would mint the same call id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steered_turn_is_deduplicated_on_retry() {
    let rig = rig([Plan::Steer]);
    let session = "sess-retry";

    let first = drive_frames(&rig, request(session, vec![user_message("hello")])).await;
    // The client never saw the answer — a dropped stream, a 5xx on the way
    // back — and re-POSTs the identical body.
    let retry = drive_frames(&rig, request(session, vec![user_message("hello")])).await;

    assert_eq!(kinds(&first), STEERED_FRAMES);
    assert_eq!(
        kinds(&retry),
        STEERED_FRAMES,
        "the replay must reproduce the same four frames from the log — which \
         is the concrete reason the emitted item lives in the log rather than \
         being a wire-only flourish"
    );
    assert_eq!(
        first[1].payload, retry[1].payload,
        "the same item, byte for byte"
    );
    assert_eq!(first[2].payload, retry[2].payload);
    assert_eq!(
        first[3].payload["response"]["id"], retry[3].payload["response"]["id"],
        "a retry is answered with the response it already paid for"
    );
    assert_eq!(
        first[3].payload["response"]["usage"],
        retry[3].payload["response"]["usage"]
    );

    assert_eq!(
        rig.interjector.calls(),
        1,
        "the seam is consulted once per turn id, never once per attempt"
    );
    let items = stored_items(&rig.store, session).await;
    assert_eq!(items.len(), 3, "a replay appends nothing: {items:#?}");
}

/// The control for the narrowed projection: an ordinary turn is unchanged.
///
/// `concerns()` now admits `ItemAppended`, and the reason it admits only a
/// *tool call bearing this response's stamp* is right here — an assistant text
/// item is already on the wire through the delta path, and forwarding it would
/// emit a second `output_item.done` for the same message. Without this test the
/// narrowness claim is untested and the regression is invisible to a client,
/// which merely sees the answer twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_assistant_text_item_is_not_forwarded_twice() {
    let rig = rig([Plan::Proceed]);

    let frames = drive_frames(&rig, request("sess-ordinary", vec![user_message("hello")])).await;
    assert_eq!(
        kinds(&frames),
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ],
        "an ordinary turn is exactly what it was before the projection changed"
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.kind() == "response.output_item.done")
            .count(),
        1,
        "one done frame per item, and the assistant item has exactly one"
    );
    assert_eq!(
        frames[3].payload["item"]["type"], "message",
        "and it is the message, not a call: {:?}",
        frames[3].payload
    );
}

/// The degrade path reaches the client as an answer, not as silence.
///
/// Outcome C is what happens when the correction cannot be a tool call — no MCP
/// registered, or a membership whose channel forbids one — and it is named
/// honestly in the plan: Codex ends its loop on a message with no tool call, so
/// a halt *hands control back to the human*. That only works if the human is
/// handed something. Before this projection existed a halted turn streamed
/// `created` then `completed` with no text at all: the guidance sat in the log
/// and the agent saw an empty answer, which is the most complete failure in the
/// design and the most silent from the deployment's side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_halted_turn_carries_its_guidance_as_the_answer() {
    let halting = rig([Plan::Halt]);

    let frames = drive_frames(
        &halting,
        request("sess-halted", vec![user_message("hello")]),
    )
    .await;
    assert_eq!(
        kinds(&frames),
        STEERED_FRAMES,
        "four frames, the same four a steered turn is: a halt streams no deltas, \
         so the item is announced and finished in one pair rather than opened by \
         a first delta"
    );
    assert_eq!(
        frames[2].payload["item"]["type"], "message",
        "and the item is a message, not a call — there is nothing here for a \
         client to dispatch: {:?}",
        frames[2].payload
    );
    assert_eq!(
        frames_carrying_a_call(&frames),
        0,
        "a halt emits no call, so no client may be handed one to run"
    );

    // The text itself, which is the whole point.
    let done = &frames[2].payload["item"]["content"][0]["text"];
    assert_eq!(done, HALT_GUIDANCE);

    // The same F03 split the steered turn gets, and for the same reason: a halt
    // is answered at the seam too, so its completion would otherwise report the
    // judge's usage as the client's context contribution. That a halt ends the
    // agent's loop makes the wrong number less consequential, not more correct
    // — and without this the halt branch of the substitution is a code path no
    // test reaches.
    let stored = stored_items(&halting.store, "sess-halted").await;
    let (guidance, admitted) = stored
        .split_last()
        .expect("the guidance is the last item this turn appended");
    let usage = &frames[3].payload["response"]["usage"];
    assert_eq!(
        usage["input_tokens"].as_u64(),
        Some(admitted.iter().map(|item| item.render().len() as u64).sum()),
    );
    assert_eq!(
        usage["output_tokens"].as_u64(),
        Some(guidance.render().len() as u64),
    );
    assert_ne!(
        usage["input_tokens"].as_u64(),
        Some(steer_usage().input_tokens),
        "the wire must not be reporting the interjection's usage on a halt either"
    );

    // And Codex's own parser sees a message it will surface, rather than the
    // `Other` an unrecognized shape silently becomes. A fresh rig, because the
    // script above is spent: a second turn on the same one would proceed, and
    // this assertion would then be about an ordinary answer.
    let parsed = rig([Plan::Halt]);
    let events = drive(
        &parsed,
        request("sess-halted-parsed", vec![user_message("hello")]),
    )
    .await
    .expect("a halted turn is a completed turn, not a failure");
    let done: Vec<&ResponseItem> = events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputItemDone(item) => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(done.len(), 1);
    assert!(
        matches!(done[0], ResponseItem::Message { role, content, .. }
            if role == "assistant"
                && content
                    .iter()
                    .any(|part| matches!(part, ContentItem::OutputText { text } if text == HALT_GUIDANCE))),
        "the guidance has to arrive as an assistant message: got {:?}",
        done[0]
    );
}

/// A steered turn dispatched nothing, so nothing may be booked for it.
///
/// Two claims, and the ordinary turn after it is the control that keeps both
/// honest: without it, "no grant, no model row" would be equally satisfied by a
/// ledger that was never wired and a fold that never books anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steered_turn_opens_no_grant_and_books_no_model_row() {
    let (rig, ledger) = budgeted_rig([Plan::Steer]);

    drive(&rig, request("sess-books", vec![user_message("hello")]))
        .await
        .expect("the steered turn completes");

    assert_eq!(
        ledger.grants(),
        0,
        "the seam short-circuits before `plan`, so no grant is ever requested"
    );
    let steered = rig.metrics();
    assert_eq!(steered.turns, 1, "the turn was admitted");
    assert_eq!(
        steered.calls, 0,
        "but no dispatch reached a provider, so nothing is accounted for"
    );
    assert!(
        steered.models.is_empty(),
        "a turn with no `Routed` books no model row: {:#?}",
        steered.models
    );

    // The control. The same rig, the same ledger, the same fold — and the very
    // next turn, which is not steered, does all three things.
    drive(
        &rig,
        request(
            "sess-books",
            vec![
                user_message("hello"),
                function_call_item(
                    STEER_TOOL,
                    Some(NAMESPACE),
                    &rig.interjector.steer().call_id,
                    &rig.interjector.steer().arguments,
                ),
                function_call_output_item(&rig.interjector.steer().call_id, "understood"),
            ],
        ),
    )
    .await
    .expect("the ordinary turn completes");

    assert_eq!(
        ledger.grants(),
        1,
        "a dispatched turn does reserve budget — which is what proves the \
         zero above is about steering and not about an unwired ledger"
    );
    let served = rig.metrics();
    assert_eq!(served.calls, 1);
    assert_eq!(
        served.models.len(),
        1,
        "and books exactly one model row: {:#?}",
        served.models
    );
}

/// The namespace and the item id are wire decoration, and the turn id is a
/// hash of the conversation — so neither may move it.
///
/// Observable only through deduplication, which is the honest place for it:
/// a client that re-sends the same conversation spelled *flat* — no namespace,
/// no item id, the shape a future Messages surface would send — must land on
/// the same turn id and be answered with the response the namespaced spelling
/// already produced. If canonicalization read either field, this would be a new
/// turn, generated and billed a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_clients_namespace_and_item_id_do_not_perturb_the_turn_hash() {
    let rig = rig([Plan::Steer]);
    let session = "sess-hash";

    drive(&rig, request(session, vec![user_message("hello")]))
        .await
        .expect("the steered turn completes");
    let steer = rig.interjector.steer();

    let namespaced = vec![
        user_message("hello"),
        function_call_item(
            STEER_TOOL,
            Some(NAMESPACE),
            &steer.call_id,
            &steer.arguments,
        ),
        function_call_output_item(&steer.call_id, "understood"),
    ];
    let first = drive(&rig, request(session, namespaced))
        .await
        .expect("the namespaced resend completes");

    // The same conversation, spelled without a namespace and without an item
    // id. Everything a dialect adds is gone; everything the hash reads is the
    // same.
    let flat = vec![
        user_message("hello"),
        function_call_item(STEER_TOOL, None, &steer.call_id, &steer.arguments),
        function_call_output_item(&steer.call_id, "understood"),
    ];
    let second = drive(&rig, request(session, flat))
        .await
        .expect("the flat resend completes");

    assert_eq!(
        response_id(&second),
        response_id(&first),
        "two spellings of one conversation are one turn: a namespace or an \
         item id reaching the hash would bill this twice"
    );
    assert_never_forked(&rig.store, session).await;
    let items = stored_items(&rig.store, session).await;
    assert_eq!(
        items.len(),
        5,
        "and nothing was appended a second time: {items:#?}"
    );
}

/// The wire projection is the *only* place a namespace exists.
///
/// A negative assertion over the log's own bytes, because the cost of getting
/// this wrong is invisible until a second dialect appears: a namespace stored
/// in the item would make Codex's resend and a flat resend canonicalize to two
/// different items, and every steered session would fork on the turn a client
/// changed dialect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_log_stores_the_bare_tool_name_and_no_namespace() {
    let rig = rig([Plan::Steer]);

    drive(&rig, request("sess-bare", vec![user_message("hello")]))
        .await
        .expect("the steered turn completes");

    let items = stored_items(&rig.store, "sess-bare").await;
    let ItemContent::ToolCall { name, .. } = &items[2].content else {
        panic!("the third item is the emitted call: {items:#?}");
    };
    assert_eq!(name, STEER_TOOL);
    let encoded: Value = serde_json::to_value(&items[2]).expect("an item serializes");
    assert!(
        !encoded.to_string().contains(NAMESPACE),
        "the stored item must carry no namespace anywhere: {encoded}"
    );
}

/// The agent's own tool calls stay the agent's own.
///
/// The second control for the narrowed projection, and the one that keeps the
/// *provenance* half of `concerns()` honest rather than only the content half.
/// An agent runs tools between our turns; those calls arrive as ordinary input
/// and are appended as fresh suffix with no response stamp on them. Projecting
/// them would hand the client back a call it just made — which it would
/// dispatch a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clients_own_tool_call_is_not_projected_as_an_emitted_one() {
    let rig = rig([Plan::Proceed]);

    // A call this deployment never emitted, with its output: the ordinary
    // shape of an agent that ran a tool of its own before asking us anything.
    let frames = drive_frames(
        &rig,
        request(
            "sess-own-tool",
            vec![
                user_message("hello"),
                function_call_item("grep", Some(NAMESPACE), "call_theirs", r#"{"q":"x"}"#),
                function_call_output_item("call_theirs", "3 hits"),
            ],
        ),
    )
    .await;

    assert_eq!(
        kinds(&frames),
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ],
        "an unstamped tool call is the client's own and belongs to no response \
         of ours: projecting it would hand the agent back a call it just ran"
    );
    assert_eq!(frames[1].payload["item"]["type"], "message");
    assert_eq!(frames[3].payload["item"]["type"], "message");

    // It did reach the log — which is what makes the absence above a decision
    // rather than an item that was never there to project.
    let items = stored_items(&rig.store, "sess-own-tool").await;
    assert!(
        items.iter().any(
            |item| matches!(&item.content, ItemContent::ToolCall { call_id, .. }
                if call_id == "call_theirs")
        ),
        "the client's call is stored like any other item: {items:#?}"
    );
}

/// An earlier turn's steer does not leak into a later response's replay.
///
/// The third narrowness control, for the half of the provenance check that
/// compares stamps rather than merely requiring one. A replay re-reads the
/// session's log from the beginning, so every emitted call this session ever
/// made passes under `concerns()` — and only the one bearing *this* response's
/// id may go out. Without the comparison, the retry below would replay turn
/// one's steer alongside turn two's answer, and the agent would dispatch a
/// call it answered two turns ago.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_earlier_steer_is_not_replayed_into_a_later_response() {
    let rig = rig([Plan::Steer]);
    let session = "sess-earlier-steer";

    drive(&rig, request(session, vec![user_message("hello")]))
        .await
        .expect("the steered turn completes");
    let steer = rig.interjector.steer();

    let fulfilled = vec![
        user_message("hello"),
        function_call_item(
            STEER_TOOL,
            Some(NAMESPACE),
            &steer.call_id,
            &steer.arguments,
        ),
        function_call_output_item(&steer.call_id, "understood"),
    ];
    drive(&rig, request(session, fulfilled.clone()))
        .await
        .expect("the fulfilling turn completes");

    // The identical body again: deduplicated onto the second response, whose
    // entries are replayed out of a log that also holds the first response's
    // emitted call.
    let replay = drive_frames(&rig, request(session, fulfilled)).await;
    assert_eq!(
        kinds(&replay),
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ],
        "the replay is the second response and nothing else: {replay:#?}"
    );
    assert_eq!(frames_carrying_a_call(&replay), 0);
}
