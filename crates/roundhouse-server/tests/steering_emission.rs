// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Answering a turn with a correction instead of running it, and surviving the
//! client's resend of that correction.
//!
//! One primitive and its consequences. A turn can complete carrying an item
//! this deployment produced instead of the completion the client asked for; the
//! client reads it, carries on, and comes back next turn with that item in the
//! history it re-sends. Everything here is about that round trip: that Codex's
//! own parser sees the item rather than silently dropping one it cannot read,
//! that the four frames are the only four, that the resend extends the session
//! instead of forking it, and that none of it books a model row or opens a
//! grant — because nothing was dispatched.
//!
//! # What M10.0 changed, and what it deliberately did not
//!
//! Until M10.0 the emitted item was a synthetic `function_call` naming
//! `fetch_steer`: the client dispatched it, fetched the correction over MCP, and
//! returned its output as a tool result the next turn. Ruling R1/T1 of
//! `PLAN-frontier-selection.md` retired that channel. The steered turn is
//! answered with **assistant text** — the rendered directive followed by the
//! pending request restated, quoted line by line — so the agent reads the
//! correction where it reads every other answer and decides with no round trip.
//!
//! Three consequences run through every test below.
//!
//! - **The resend is one item, not two.** The old round trip put a call *and*
//!   its output back in the history; the new one puts back the assistant message
//!   the client was handed. The prefix property being asserted is unchanged and
//!   the item being asserted about is different — which is why the fork tests
//!   are re-aimed rather than deleted.
//! - **The frame count is the same four**, and that is not a coincidence worth
//!   losing: a seam answer is committed whole and streamed as nothing, so it is
//!   announced and finished in one pair either way. `STEERED_FRAMES` is
//!   therefore still the list, and a halt and a steer are now literally the same
//!   sequence.
//! - **The inbound machinery is untouched (T4).** An agent still runs its own
//!   MCP tools between our turns and still re-sends them namespaced, so
//!   canonicalization still has to ignore a `namespace` field and an item id.
//!   The tests that pin that are re-pointed at a *client's* call, which is the
//!   only kind there is now — see
//!   `a_clients_own_tool_call_is_not_projected_as_an_emitted_one`, which used to
//!   be a control and is now the whole rule.
//!
//! **What stands in for the validator.** The decision to steer is made by
//! [`TestInterjector`], a test-only occupant of the *real* seam
//! ([`roundhouse_core::interject::Interjector`]) that the validator occupies
//! unchanged. It is a scripted queue rather than anything clever on purpose:
//! what this suite is about is what happens *after* something decides to steer,
//! and a decision procedure with opinions of its own would make every assertion
//! below partly about the decision. `validate_loop.rs` is where the real one
//! runs.
//!
//! **Why these are integration tests.** Every claim spans the seam, the log,
//! the projection and a real client parser at once. The unit tests one layer
//! down already prove that `complete_with_item` commits one batch and that
//! `suffix_after` admits a suffix; what they cannot prove is that the item a
//! session committed comes back out of the wire as an item Codex will surface,
//! and re-enters as the same canonical item it went in as.

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
use roundhouse_core::event::{
    Accounting, ControlRecord, SessionEventKind, Usage, ValidationOutcome,
};
use roundhouse_core::ids::{SessionId, SideCallId, ValidationId};
use roundhouse_core::interject::{Interjection, InterjectionContext, Interjector};
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::metrics::{MetricsConfig, MetricsSnapshot, ShadowPricing};
use roundhouse_core::now_ms;
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_core::validate::{Arm, Divergence, SteerAction, TriggerRecord, Verdict};
use roundhouse_fleet::EchoFrontierClient;
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, Conversations, EchoLocalExecutor, Engine, EngineConfig,
    responses_router,
};

mod common;
use common::codex::{
    Frame, NoAuth, RouterTransport, StaticToken, assistant_message, collect, frames,
    function_call_item, function_call_output_item, reasoning_item, request, user_message,
};
use common::{frontier_catalog, sha256_hex};

/// What the echo provider answers an *ordinary* turn with.
const ANSWER: &str = "frontier answer";

/// The namespace a codex client puts on the MCP tools it runs of its own
/// accord, and the one Codex's exact `ToolName { name, namespace }` lookup
/// resolves against.
///
/// Inbound only since M10.0: this deployment projects no tool call any more, so
/// the only place a namespace appears on this wire is on a call the *client*
/// sent us.
const NAMESPACE: &str = "mcp__roundhouse";

// ---------------------------------------------------------------------------
// The occupant of the seam
// ---------------------------------------------------------------------------

/// What [`TestInterjector`] does with one turn.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Plan {
    /// Answer the turn with the correction and the task restated, instead of
    /// running it — outcome B, and since M10.0 an assistant message.
    Steer,
    /// Answer the turn with plain guidance and nothing to act on — outcome C,
    /// which ends the client's loop. Named here because it is the *other*
    /// completion shape the seam admits, and this file is where completion
    /// shapes are pinned to frames.
    ///
    /// The two are the same *shape* now and differ only in what the text
    /// invites, which is exactly why both are still scripted here: the
    /// projection cannot tell them apart, so a test that covered only one would
    /// leave the other's substitution unreached.
    Halt,
    /// Leave the turn alone, exactly as the production default does.
    Proceed,
}

/// The directive half of the correction — roundhouse's own words.
///
/// Distinctive on purpose: an assertion that this text reached the agent *as the
/// turn's answer* is an assertion about a literal. Before M10.0 the assertion
/// was the opposite one — that this text reached the agent through `fetch_steer`
/// and never through the wire — and the literal is kept so the inversion is
/// visible in the diff rather than hidden behind a renamed constant.
const STEER_GUIDANCE: &str = "you are editing a file the task did not name; go back to the parser";

/// The whole answer a steered turn hands back: the directive, then the pending
/// request restated and quoted.
///
/// Written out rather than composed by calling
/// [`render_steer_answer`](roundhouse_core::validate::render_steer_answer),
/// deliberately. This suite is about what happens to an item *after* something
/// decided to emit it, so its fixture must not be a call into the function under
/// test in some other suite — the composition's own golden lives beside
/// `render_steer_answer`, and `validate_loop.rs` is where the real validator's
/// output is asserted end to end. A literal here means a change to the
/// composition cannot silently change what this file is asserting about.
const STEER_ANSWER: &str = "you are editing a file the task did not name; go back to the parser\n\n\
     The request you are working on is restated below. Every line of it is quoted: the guidance \
     above is roundhouse's, and the quoted lines are the ones you sent.\n\n> hello";

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
        // The exception to "non-zero on every axis": an interjection is
        // answered without dispatching, so nothing was written into any
        // provider's cache and a non-zero count here would be fiction.
        cache_write_tokens: 0,
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
}

impl TestInterjector {
    fn new(script: impl IntoIterator<Item = Plan>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into_iter().collect()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// The decision record a steering occupant owes the log.
///
/// **The bookkeeping M10.0 moved, and the reason it had to move.** Until M10.0
/// the *item* said a steer had happened: a tool call bearing a response id is a
/// shape no client can produce, so the session fold read provenance and knew. A
/// steer is assistant text now — the same shape every dispatched turn's answer
/// has — so no property of the item can tell them apart, and the fact is taken
/// off `ValidationDecided` instead. A double that steered without recording one
/// would leave `steered_on_turn` unset and the trigger's hysteresis silently
/// off, which is exactly what a *production* occupant skipping it would do.
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
                    description: "the judge's prose, which never travels".into(),
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
            // Built with **no response stamp**, deliberately, where the steer
            // below is built with one. `complete_with_item` overwrites it in
            // exactly one place, so the two spellings have to produce the same
            // committed item — which is what makes "an item bearing a response
            // id is one this deployment wrote" a property of the commit rather
            // than of every occupant's care. The pair here is what goes red if
            // that stamp ever moves back out to the caller.
            Plan::Halt => Interjection::Complete {
                item: Item {
                    role: Role::Assistant,
                    content: ItemContent::Text {
                        text: HALT_GUIDANCE.to_string(),
                    },
                    response_id: None,
                },
                usage: steer_usage(),
                // Nothing beside the item. A halt restates no task, so there
                // was never anything for a `guidance` field to hold that the
                // item did not already carry — and since M10.0 that is true of
                // the steer as well, which is why the field is gone.
                record: ControlRecord::default(),
            },
            Plan::Steer => Interjection::Complete {
                item: Item::assistant_text(STEER_ANSWER, context.response_id.clone()),
                usage: steer_usage(),
                record: steer_record(STEER_GUIDANCE),
            },
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

/// The whole premise, checked against the parser that will consume it: the
/// correction arrives as an assistant `Message` and **not** as
/// `ResponseItem::Other`.
///
/// `Other` is what an item of an unknown or unparseable shape silently becomes
/// (`codex_conformance.rs` names the same failure for messages). Every
/// sequence-level assertion in this file would still pass with an `Other` in
/// place of the answer, and the agent would be handed nothing at all — so the
/// negative half is the half that matters.
///
/// **This replaces `a_synthetic_function_call_arrives_as_codex_parses_it`**,
/// which asserted the same property of a `FunctionCall` carrying a separate
/// `namespace` field. That item no longer exists (T4). What survives the change
/// is the *reason* for an oracle test at all — codex's parser is the contract,
/// not our reading of it — and the shape it is asked about is now the shape a
/// steered turn actually emits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_arrives_as_a_message_codex_will_surface() {
    let rig = rig([Plan::Steer]);

    let events = drive(&rig, request("sess-parses", vec![user_message("hello")]))
        .await
        .expect("a steered turn is a completed turn, not a failure");

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
        ResponseItem::Message { role, content, .. } => {
            assert_eq!(role, "assistant", "the correction is roundhouse speaking");
            let text = content
                .iter()
                .find_map(|part| match part {
                    ContentItem::OutputText { text } => Some(text.as_str()),
                    _ => None,
                })
                .expect("an assistant message carries output text");
            assert_eq!(
                text, STEER_ANSWER,
                "the answer arrives whole: the directive and the restated request"
            );
            assert!(
                text.contains("\n> hello"),
                "and the restatement is quoted line by line, which is what stops \
                 a request reading as roundhouse's own voice: {text}"
            );
        }
        other => panic!(
            "the item must parse as an assistant Message, not silently become \
             ResponseItem::Other — an `Other` here is an answer the agent never \
             sees, and every other assertion in this suite would still pass: \
             {other:?}"
        ),
    }

    // The announced item is the same message, not something else: a client that
    // was told about one item and handed another has nowhere to put it.
    let ResponseEvent::OutputItemAdded(added) = &events[1] else {
        panic!("expected an added item second: {:?}", events[1]);
    };
    assert!(
        matches!(added, ResponseItem::Message { .. }),
        "the added frame must announce the message itself: {added:?}"
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

    let added = &frames[1].payload["item"];
    let done = &frames[2].payload["item"];

    // **The `added` frame is an empty shell and the `done` frame carries the
    // text, and that asymmetry is the message path's, not a regression.** A
    // call was announced complete because its arguments never streamed; a
    // message is announced empty because ordinarily its text arrives as deltas.
    // A seam answer streams no deltas, so the pair is "announce, then finish
    // whole" — which is exactly what the halted turn has always done, and is
    // now what a steered turn does too.
    assert_eq!(
        *added,
        serde_json::json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "" }],
        }),
        "the added frame announces the message item with an empty text part -- \
         a shell for deltas that, on this path, never come"
    );
    assert_eq!(
        *done,
        serde_json::json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": STEER_ANSWER }],
        }),
        "the done frame is the whole answer, and its shape is the contract with \
         Codex's parser"
    );
    assert_eq!(
        frames_carrying_a_call(&frames),
        0,
        "no verdict maps to a tool call any more (T2/T4), so nothing this \
         deployment emits may arrive as one for a client to dispatch"
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
        .expect("the correction is the last item this turn appended");
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

/// The other half of the round trip: the agent carries on, re-sending the
/// correction in its history, and the session extends rather than forks.
///
/// **One item comes back now, not two.** The old shape put the emitted call
/// *and* the output the client produced by running it back into the history;
/// the correction is the turn's answer now, so what returns is the assistant
/// message itself — and prefix admission of it is ordinary, which is the whole
/// of T1's "the item is stored and response_id-stamped like any answer".
///
/// Played with Codex's own types, so the resent message carries the shape a real
/// client sends rather than our reconstruction of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_resent_guidance_extends_rather_than_forks() {
    let rig = rig([Plan::Steer]);
    let session = "sess-resend";

    drive(&rig, request(session, vec![user_message("hello")]))
        .await
        .expect("the steered turn completes");

    let after_steer = stored_items(&rig.store, session).await;
    assert_eq!(
        after_steer.len(),
        3,
        "instructions, the question, and the correction: {after_steer:#?}"
    );

    // Exactly what the agent sends next: everything it had, plus the answer it
    // was handed, plus what it decided to say having read it.
    let resent = vec![
        user_message("hello"),
        assistant_message(STEER_ANSWER),
        user_message("right — back to the parser"),
    ];
    let second = drive(&rig, request(session, resent))
        .await
        .expect("the turn after the steer completes");
    assert!(!response_id(&second).is_empty());

    assert_never_forked(&rig.store, session).await;
    let items = stored_items(&rig.store, session).await;
    assert_eq!(
        items[3].content,
        ItemContent::Text {
            text: "right — back to the parser".to_string(),
        },
        "the suffix admitted is exactly the new question, and nothing before \
         it: {items:#?}"
    );
    assert_eq!(items[3].role, Role::User);
    assert_eq!(
        items
            .iter()
            .filter(|item| item.role == Role::Assistant
                && matches!(&item.content, ItemContent::Text { text } if text == STEER_ANSWER))
            .count(),
        1,
        "the resent correction must be recognized as the prefix it is, not \
         appended a second time: {items:#?}"
    );
    // The correction kept its stamp and the client's own message never had one:
    // that asymmetry is what lets a projection tell an item this deployment
    // wrote from one a client sent.
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

/// The N+2 case, where the correction has moved *inside* the overlap.
///
/// On turn N+1 the guidance item is the tail of the claimed prefix; on N+2 it is
/// deep inside it, compared against the client's own stored copy. That is the
/// turn where a canonicalization that renders an assistant message differently
/// coming in than it stored going out would fork — and it would fork one turn
/// *after* the one anybody was watching.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_third_turn_after_a_steer_still_matches_its_prefix() {
    let rig = rig([Plan::Steer]);
    let session = "sess-third";

    drive(&rig, request(session, vec![user_message("hello")]))
        .await
        .expect("the steered turn completes");

    let after = vec![
        user_message("hello"),
        assistant_message(STEER_ANSWER),
        user_message("right — back to the parser"),
    ];
    drive(&rig, request(session, after.clone()))
        .await
        .expect("the turn after the steer completes");

    let mut third = after;
    third.push(assistant_message(ANSWER));
    third.push(user_message("and now this"));
    drive(&rig, request(session, third))
        .await
        .expect("the turn after that completes");

    assert_never_forked(&rig.store, session).await;
    let items = stored_items(&rig.store, session).await;
    assert_eq!(
        items
            .iter()
            .filter(
                |item| matches!(&item.content, ItemContent::Text { text } if text == STEER_ANSWER)
            )
            .count(),
        1,
        "the correction is stored once however many turns re-send it: {items:#?}"
    );
    assert_eq!(
        items.len(),
        7,
        "instructions, question, correction, question, answer, question, \
         answer: {items:#?}"
    );
}

/// An agent that reasons after being corrected does not fork the session
/// either.
///
/// `reasoning` items are dropped on the way in, on every request alike — which
/// is what keeps the claimed prefix equal to the stored one. Worth its own test
/// because a steered turn is exactly when a real agent reasons: it was told its
/// approach was going nowhere and handed its own request back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reasoning_item_after_the_correction_does_not_fork() {
    let rig = rig([Plan::Steer]);
    let session = "sess-reasoning";

    drive(&rig, request(session, vec![user_message("hello")]))
        .await
        .expect("the steered turn completes");

    let resent = vec![
        user_message("hello"),
        assistant_message(STEER_ANSWER),
        reasoning_item("rs_1"),
        user_message("right — back to the parser"),
    ];
    drive(&rig, request(session, resent))
        .await
        .expect("the turn carrying a reasoning item completes");

    assert_never_forked(&rig.store, session).await;
    let items = stored_items(&rig.store, session).await;
    assert_eq!(
        items.len(),
        5,
        "instructions, question, correction, question, answer — the reasoning \
         item is dropped rather than stored: {items:#?}"
    );
    assert_eq!(items[3].role, Role::User);
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
                assistant_message(STEER_ANSWER),
                user_message("right — back to the parser"),
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

/// A client's verbatim retry of a namespaced conversation replays, and the
/// same conversation with the namespace dropped does not.
///
/// **Rewritten by M17, and the rewrite is the ruling.** Until M17 this asserted
/// that a namespaced spelling and a bare one were *one* turn, on the reasoning
/// that a namespace is wire decoration the hash must not read. Half of that
/// survives and half of it was overturned:
///
/// - **The hash still does not read it.** `Item::render` leaves the field out
///   deliberately, so no already-stored conversation moved when the field
///   landed, and `responses_api::wire`'s
///   `the_turn_id_of_a_control_call_conversation_is_pinned_bare_and_namespaced`
///   pins that as one literal over two fixtures. That property is asserted at
///   the unit level now, where it can be pinned as a *value*, rather than
///   inferred here from a deduplication that also depends on prefix admission.
/// - **The two spellings are no longer one conversation.** The log keeps the
///   namespace beside the name (R-N6), and prefix admission requires a stored
///   `Some` to match (R-N8) — because a client that changes which MCP server a
///   tool name came from is describing a different call, and appending it onto
///   the first client's conversation would be the silent error. So the bare
///   resend forks, deliberately, and this is the end-to-end guard for that.
///
/// The retry half is what the deduplication claim was really protecting and it
/// is asserted first: an identical resend must still land on the response it
/// already paid for, or a client's ordinary retry buys a second billed answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verbatim_namespaced_retry_replays_and_a_bare_resend_is_a_different_conversation() {
    let rig = rig([Plan::Proceed, Plan::Proceed]);
    let session = "sess-hash";

    let namespaced = || {
        vec![
            user_message("hello"),
            function_call_item("grep", Some(NAMESPACE), "call_theirs", r#"{"q":"x"}"#),
            function_call_output_item("call_theirs", "3 hits"),
        ]
    };
    let first = drive(&rig, request(session, namespaced()))
        .await
        .expect("the namespaced turn completes");

    let retry = drive(&rig, request(session, namespaced()))
        .await
        .expect("the verbatim retry completes");
    assert_eq!(
        response_id(&retry),
        response_id(&first),
        "a client's retry of the conversation it already sent must replay the \
         answer it already paid for"
    );
    assert_never_forked(&rig.store, session).await;
    assert_eq!(
        stored_items(&rig.store, session).await.len(),
        5,
        "instructions, question, call, output, answer — and nothing appended a \
         second time"
    );

    // The same call with the namespace dropped. The turn id is unmoved — the
    // render never saw the field — so this is not a hash change; it is prefix
    // admission refusing to continue somebody else's conversation, which is
    // R-N8's stated asymmetry running in the direction it does not forgive.
    let bare = vec![
        user_message("hello"),
        function_call_item("grep", None, "call_theirs", r#"{"q":"x"}"#),
        function_call_output_item("call_theirs", "3 hits"),
    ];
    let forked = drive(&rig, request(session, bare))
        .await
        .expect("the bare resend completes");
    assert_ne!(
        response_id(&forked),
        response_id(&first),
        "a claim that dropped the namespace is not the stored conversation, \
         and answering it out of the stored one would attribute a call to a \
         server it never went to"
    );
    assert!(
        rig.store
            .last_seq(&SessionId::new(format!("{session}#g1")))
            .await
            .is_ok(),
        "the disagreeing claim must open its own generation rather than \
         appending onto the first client's"
    );
}

/// The log stores the bare tool name **and the namespace beside it** — two
/// fields, exactly as the client sent two fields.
///
/// **Inverted by M17 (R-N6), and the inversion is the point.** This used to
/// assert the namespace appeared nowhere in the stored bytes, on the reasoning
/// that keeping it would make a namespaced resend and a flat resend two
/// different items. They *are* two different items, that is now the ruling
/// rather than the hazard, and what the old assertion cost was named in the
/// evidence: a third party's tool called `status` was indistinguishable from
/// ours in the log, and a call re-emitted without its namespace resolved
/// against nothing in codex's exact `ToolName { name, namespace }` lookup.
///
/// Asserted over the log's own bytes rather than over the typed item, because
/// what a future migration reads is the stored JSON: the key is `namespace`,
/// it sits beside `name` rather than folded into it, and a build that started
/// writing `mcp__roundhouse__grep` into `name` would pass a typed check and
/// fail here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_log_stores_the_bare_tool_name_with_the_namespace_beside_it() {
    let rig = rig([Plan::Proceed]);

    drive(
        &rig,
        request(
            "sess-bare",
            vec![
                user_message("hello"),
                function_call_item("grep", Some(NAMESPACE), "call_theirs", r#"{"q":"x"}"#),
                function_call_output_item("call_theirs", "3 hits"),
            ],
        ),
    )
    .await
    .expect("the turn completes");

    let items = stored_items(&rig.store, "sess-bare").await;
    let ItemContent::ToolCall {
        name, namespace, ..
    } = &items[2].content
    else {
        panic!("the third item is the client's own call: {items:#?}");
    };
    assert_eq!(name, "grep", "the log keeps the bare tool name");
    assert_eq!(
        namespace.as_deref(),
        Some(NAMESPACE),
        "and the server it went to, beside the name rather than folded into it"
    );

    let encoded: Value = serde_json::to_value(&items[2]).expect("an item serializes");
    assert_eq!(
        encoded["content"]["namespace"],
        Value::String(NAMESPACE.to_string()),
        "the stored record's own key: {encoded}"
    );
    assert_eq!(
        encoded["content"]["name"],
        Value::String("grep".to_string()),
        "and the name is still the bare one — a build that folded the \
         namespace in would pass a typed check and fail here: {encoded}"
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

/// An earlier turn's correction does not leak into a later response's replay.
///
/// The provenance check compares *stamps* rather than merely requiring one. A
/// replay re-reads the session's log from the beginning, so every item this
/// session ever emitted passes under `concerns()` — and only the one bearing
/// *this* response's id may go out. Without the comparison, the retry below
/// would replay turn one's correction alongside turn two's answer, and the agent
/// would be handed a correction it read two turns ago as though it were fresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_earlier_steer_is_not_replayed_into_a_later_response() {
    let rig = rig([Plan::Steer]);
    let session = "sess-earlier-steer";

    drive(&rig, request(session, vec![user_message("hello")]))
        .await
        .expect("the steered turn completes");

    let after = vec![
        user_message("hello"),
        assistant_message(STEER_ANSWER),
        user_message("right — back to the parser"),
    ];
    drive(&rig, request(session, after.clone()))
        .await
        .expect("the turn after the steer completes");

    // The identical body again: deduplicated onto the second response, whose
    // entries are replayed out of a log that also holds the first response's
    // correction.
    let replay = drive_frames(&rig, request(session, after)).await;
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
    let answered = &replay[3].payload["item"]["content"][0]["text"];
    assert_eq!(
        answered, ANSWER,
        "and what it replays is that response's own answer, not the earlier \
         correction: {answered}"
    );
    assert_eq!(frames_carrying_a_call(&replay), 0);
}
