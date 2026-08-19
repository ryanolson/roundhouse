// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The HTTP/SSE transport, driven as a `tower::Service`.
//!
//! No socket is bound: the router is called directly and its body is read frame
//! by frame, which is the only way to assert on a stream before it ends. The
//! `/events` stream never ends by design, so collecting it would hang; every
//! read here is bounded by [`READ_TIMEOUT`] so a stall fails the test instead of
//! the suite.
//!
//! What is being tested is the claim the transport rests on: the event log is
//! the streaming bus. If that is true then the frames a client sees live, the
//! frames it sees after reconnecting, and the entries in the log are all the
//! same thing, and their sequence numbers line up with no gap and no repeat.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::ids::SessionId;
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{EchoFrontierClient, StaticFrontierCatalog};
use roundhouse_server::{ControlPlane, EchoLocalExecutor, Engine, EngineConfig, router};

mod common;
use common::frontier_catalog;

/// Ceiling on any single stream read. Generous, because it should never be
/// reached: a transport bug shows up as a stall, and a stall must be a failure
/// with a message rather than a hung test run.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A router over a fresh in-memory store, plus that store for direct probing.
fn transport() -> (Router, Arc<MemoryStore>) {
    transport_with(frontier_catalog())
}

/// As [`transport`], but with a caller-chosen catalog.
///
/// An empty one leaves an engine with nowhere to route, which is how a
/// post-admission failure is produced without a stub provider — the trait shape
/// of a provider client is not this file's business.
fn transport_with(catalog: StaticFrontierCatalog) -> (Router, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        catalog,
        Arc::new(EchoFrontierClient::new("frontier answer")),
        Arc::new(AffinityPolicy::new()),
        EngineConfig::default(),
    ));
    (
        router(ControlPlane::open(), engine, Arc::clone(&store)),
        store,
    )
}

// ---------------------------------------------------------------------------
// Request helpers
// ---------------------------------------------------------------------------

fn post_json(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

async fn json_body(response: Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("JSON body")
}

async fn open_session(app: &Router) -> String {
    let response = app
        .clone()
        .oneshot(post_json("/v1/sessions", "{}"))
        .await
        .expect("call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["created"], true);
    body["session_id"].as_str().expect("session id").to_string()
}

async fn start_turn(app: &Router, session: &str, turn_id: &str, text: &str) -> Response {
    let body = serde_json::json!({
        "turn_id": turn_id,
        "input": [{ "role": "user", "text": text }],
    });
    app.clone()
        .oneshot(post_json(
            &format!("/v1/sessions/{session}/responses"),
            &body.to_string(),
        ))
        .await
        .expect("call")
}

// ---------------------------------------------------------------------------
// Incremental SSE reading
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SseFrame {
    id: Option<String>,
    event: Option<String>,
    data: String,
}

impl SseFrame {
    fn name(&self) -> &str {
        self.event.as_deref().unwrap_or("message")
    }

    fn seq(&self) -> u64 {
        self.id
            .as_ref()
            .expect("a log frame carries its sequence number as the SSE id")
            .parse()
            .expect("sequence number")
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.data).expect("frame data must be JSON")
    }

    fn response_id(&self) -> Option<String> {
        self.json()["response_id"].as_str().map(str::to_string)
    }
}

/// Reads whole SSE frames off a body that may still be open.
struct SseReader {
    body: Body,
    buffer: String,
}

impl SseReader {
    fn new(body: Body) -> Self {
        Self {
            body,
            buffer: String::new(),
        }
    }

    /// Next frame, or `None` once the body ends.
    async fn next(&mut self) -> Option<SseFrame> {
        tokio::time::timeout(READ_TIMEOUT, self.read_frame())
            .await
            .expect("the SSE stream stalled")
    }

    async fn read_frame(&mut self) -> Option<SseFrame> {
        loop {
            if let Some(index) = self.buffer.find("\n\n") {
                let raw: String = self.buffer.drain(..index + 2).collect();
                // `None` is a keep-alive comment, which carries no fields.
                if let Some(frame) = parse_frame(&raw) {
                    return Some(frame);
                }
                continue;
            }
            let chunk = self.body.frame().await?.expect("body frame");
            if let Ok(data) = chunk.into_data() {
                self.buffer
                    .push_str(std::str::from_utf8(&data).expect("SSE bodies are UTF-8"));
            }
        }
    }

    /// Everything left on a stream that ends by itself.
    async fn drain(mut self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next().await {
            frames.push(frame);
        }
        frames
    }

    /// Exactly `count` frames from a stream that does not end.
    async fn take(&mut self, count: usize) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        while frames.len() < count {
            frames.push(
                self.next()
                    .await
                    .expect("the stream ended before the tail was delivered"),
            );
        }
        frames
    }
}

fn parse_frame(raw: &str) -> Option<SseFrame> {
    let mut id = None;
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();

    for line in raw.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "id" => id = Some(value.to_string()),
            "event" => event = Some(value.to_string()),
            "data" => data.push(value),
            _ => {}
        }
    }

    if id.is_none() && event.is_none() && data.is_empty() {
        return None;
    }
    Some(SseFrame {
        id,
        event,
        data: data.join("\n"),
    })
}

/// Assert `wanted` appears within `names` in order, allowing anything between.
///
/// The engine is free to add event kinds — output deltas being the obvious one
/// — so pinning the exact list would make this a test of the engine's current
/// shape rather than of the order the transport preserves.
fn assert_ordered(names: &[&str], wanted: &[&str]) {
    let mut pending = wanted.iter().peekable();
    for name in names {
        if pending.peek().is_some_and(|expected| *expected == name) {
            pending.next();
        }
    }
    assert!(
        pending.peek().is_none(),
        "expected {wanted:?} in order within {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A turn streams the log it is writing, in the log's own order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_streams_its_events_in_order() {
    let (app, _store) = transport();
    let session = open_session(&app).await;

    let response = start_turn(&app, &session, "t0", "hello").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");

    let frames = SseReader::new(response.into_body()).drain().await;
    let names: Vec<&str> = frames.iter().map(SseFrame::name).collect();

    // A session's log opens with its identity and closes with the turn's
    // terminal event. `session_created` is named here rather than the check
    // being relaxed to "some event first": it is written into an empty log at
    // seq 1, ahead of anything that can spend money, and that ordering is what
    // lets the metrics fold attribute a turn without a side table. A stream
    // that began at `turn_started` would mean the identity was written late,
    // or not at all.
    assert_eq!(names.first(), Some(&"session_created"));
    assert_eq!(names.last(), Some(&"response_completed"));
    assert_ordered(
        &names,
        &[
            "session_created",
            "turn_started",
            "item_appended",
            "routed",
            "response_completed",
        ],
    );

    // The ids are the log's sequence numbers, contiguous: they are what a
    // client hands back to resume, so a gap here is a hole in the replay.
    let seqs: Vec<u64> = frames.iter().map(SseFrame::seq).collect();
    let expected: Vec<u64> = (seqs[0]..seqs[0] + seqs.len() as u64).collect();
    assert_eq!(seqs, expected, "SSE ids must be contiguous log sequences");

    for frame in &frames {
        let value = frame.json();
        assert_eq!(
            value["type"].as_str(),
            Some(frame.name()),
            "the SSE event name must be the payload's own type tag"
        );
        assert_eq!(value["seq"].as_u64(), Some(frame.seq()));
        assert_eq!(value["session_id"].as_str(), Some(session.as_str()));
    }
}

/// Resumption hands back the tail, and only the tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumption_replays_exactly_the_tail() {
    let (app, store) = transport();
    let session = open_session(&app).await;

    let completed = SseReader::new(start_turn(&app, &session, "t0", "hello").await.into_body())
        .drain()
        .await;
    assert!(completed.len() >= 4, "{completed:?}");

    let last = store
        .last_seq(&SessionId::new(session.clone()))
        .await
        .unwrap();
    let midpoint = last / 2;
    let expected: Vec<u64> = (midpoint + 1..=last).collect();
    assert!(!expected.is_empty());

    let response = app
        .clone()
        .oneshot(get(&format!(
            "/v1/sessions/{session}/events?starting_after={midpoint}"
        )))
        .await
        .expect("call");
    assert_eq!(response.status(), StatusCode::OK);
    let mut reader = SseReader::new(response.into_body());
    let tail: Vec<u64> = reader
        .take(expected.len())
        .await
        .iter()
        .map(SseFrame::seq)
        .collect();
    assert_eq!(
        tail, expected,
        "`starting_after` is exclusive and must produce no gap and no repeat"
    );

    // The standard reconnect header must mean exactly the same thing, because
    // that is what makes resumption work for a client that did not have to be
    // written for this API.
    let request = Request::builder()
        .method("GET")
        .uri(format!("/v1/sessions/{session}/events"))
        .header("last-event-id", midpoint.to_string())
        .body(Body::empty())
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("call");
    assert_eq!(response.status(), StatusCode::OK);
    let mut reader = SseReader::new(response.into_body());
    let resumed: Vec<u64> = reader
        .take(expected.len())
        .await
        .iter()
        .map(SseFrame::seq)
        .collect();
    assert_eq!(resumed, expected);

    // With both present the query parameter wins: it is this request's explicit
    // choice, while the header is whatever the last connection left behind.
    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/v1/sessions/{session}/events?starting_after={}",
            last - 1
        ))
        .header("last-event-id", "0")
        .body(Body::empty())
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("call");
    let mut reader = SseReader::new(response.into_body());
    assert_eq!(reader.take(1).await[0].seq(), last);
}

/// A re-sent turn is answered with the response it already produced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deduplicated_turn_replays_the_original_response() {
    let (app, _store) = transport();
    let session = open_session(&app).await;

    let first = SseReader::new(
        start_turn(&app, &session, "same-turn", "hello")
            .await
            .into_body(),
    )
    .drain()
    .await;
    let response_id = first
        .iter()
        .find(|frame| frame.name() == "turn_started")
        .expect("the first attempt started a turn")
        .response_id()
        .expect("turn_started names its response");

    // The client never saw the answer and re-sends the same turn id.
    let second = SseReader::new(
        start_turn(&app, &session, "same-turn", "hello")
            .await
            .into_body(),
    )
    .drain()
    .await;

    assert_eq!(
        second.first().map(SseFrame::name),
        Some("turn_deduplicated")
    );
    assert_eq!(
        second.last().map(SseFrame::name),
        Some("response_completed"),
        "a replay must still end on the terminal event"
    );
    for frame in &second {
        assert_eq!(
            frame.response_id().as_deref(),
            Some(response_id.as_str()),
            "a deduplicated turn must not produce a second response"
        );
    }

    // The replay is a redelivery of log entries, so it carries their sequence
    // numbers rather than new ones — and it is exactly the entries the first
    // stream delivered for that response.
    let original: Vec<u64> = first
        .iter()
        .filter(|frame| frame.response_id().as_deref() == Some(response_id.as_str()))
        .map(SseFrame::seq)
        .collect();
    let replayed: Vec<u64> = second[1..].iter().map(SseFrame::seq).collect();
    assert_eq!(replayed, original);
    assert!(
        replayed.iter().all(|seq| *seq < second[0].seq()),
        "the replayed entries predate the event that announced them"
    );

    // A third retry replays through a log that now also contains the second
    // retry's `turn_deduplicated` marker. Markers announce a replay, they are
    // not part of the response: forwarding them would push the stream's end
    // past the terminal event, one more marker per retry, forever.
    let third = SseReader::new(
        start_turn(&app, &session, "same-turn", "hello")
            .await
            .into_body(),
    )
    .drain()
    .await;
    assert_eq!(
        third.last().map(SseFrame::name),
        Some("response_completed"),
        "every retry's replay must still end on the terminal event"
    );
    assert_eq!(
        third[1..].iter().map(SseFrame::seq).collect::<Vec<_>>(),
        original,
        "earlier retries' markers must not leak into the replay"
    );
}

/// An unknown session is refused, not followed into a log that will never exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_session_is_a_404_not_a_hang() {
    let (app, _store) = transport();
    let body = r#"{"turn_id":"t0","input":[{"role":"user","text":"hello"}]}"#;

    let response = tokio::time::timeout(
        READ_TIMEOUT,
        app.clone()
            .oneshot(post_json("/v1/sessions/sess_missing/responses", body)),
    )
    .await
    .expect("the request must answer rather than poll for a session that cannot appear")
    .expect("call");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "session_not_found"
    );

    let response = tokio::time::timeout(
        READ_TIMEOUT,
        app.clone().oneshot(get("/v1/sessions/sess_missing/events")),
    )
    .await
    .expect("the stream must be refused before it opens")
    .expect("call");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// A turn that never reaches the log still closes its stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_admission_failure_surfaces_as_an_sse_error() {
    let (app, store) = transport();
    let session = open_session(&app).await;

    // Another node owns the session, so the engine cannot claim the lease and
    // returns before admitting anything. There is no response in the log to
    // terminate, so nothing but an out-of-band frame will ever end this stream.
    let held = store
        .acquire_lease(&SessionId::new(session.clone()), "another-node", 60_000)
        .await
        .unwrap();
    assert!(held.is_some());

    let response = start_turn(&app, &session, "t0", "hello").await;
    assert_eq!(response.status(), StatusCode::OK);
    let frames = SseReader::new(response.into_body()).drain().await;

    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].name(), "error");
    assert!(
        frames[0].id.is_none(),
        "an out-of-band error is not a log entry and must not move the client's cursor"
    );
    let payload = frames[0].json();
    assert_eq!(payload["type"], "error");
    assert!(
        payload["message"]
            .as_str()
            .is_some_and(|message| message.contains(&session)),
        "the error must name the session it could not claim: {payload}"
    );
}

/// A turn that fails after admission is closed by the log, then explained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_post_admission_failure_ends_on_the_log_and_then_names_itself() {
    // Nothing to route to, so the turn is admitted and then fails.
    let (app, _store) = transport_with(StaticFrontierCatalog::new(vec![]));
    let session = open_session(&app).await;

    let frames = SseReader::new(start_turn(&app, &session, "t0", "hello").await.into_body())
        .drain()
        .await;
    let names: Vec<&str> = frames.iter().map(SseFrame::name).collect();

    assert_ordered(
        &names,
        &["turn_started", "item_appended", "response_incomplete"],
    );
    assert_eq!(
        names.last(),
        Some(&"error"),
        "the failure is reported after the log has already closed the response"
    );

    // The terminal event is the close signal and is a real log entry; the error
    // frame that follows it is observability and is not, so it carries no
    // sequence number to move a resuming client's cursor onto.
    let terminal = frames
        .iter()
        .find(|frame| frame.name() == "response_incomplete")
        .expect("the settle seam terminates every admitted turn");
    assert!(terminal.seq() > 0);
    let trailer = frames.last().expect("frames");
    assert!(trailer.id.is_none());
    assert!(
        trailer.json()["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "an error frame with no message explains nothing: {trailer:?}"
    );
}

/// Input the transport cannot represent is refused before anything is logged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrepresentable_input_is_refused_with_422() {
    let (app, store) = transport();
    let session = open_session(&app).await;
    let responses = format!("/v1/sessions/{session}/responses");

    for body in [
        "not json at all",
        r#"{"input":[]}"#,
        r#"{"turn_id":"t0","input":[{"role":"assistant","text":"I said this"}]}"#,
        r#"{"turn_id":"t0","input":[{"role":"tool","text":"result"}]}"#,
    ] {
        let response = app
            .clone()
            .oneshot(post_json(&responses, body))
            .await
            .expect("call");
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "expected 422 for {body}"
        );
        assert_eq!(
            json_body(response).await["error"]["code"],
            "invalid_request"
        );
    }

    // A cursor that does not parse is refused rather than silently treated as a
    // request to replay the whole session.
    let response = app
        .clone()
        .oneshot(get(&format!(
            "/v1/sessions/{session}/events?starting_after=tomorrow"
        )))
        .await
        .expect("call");
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // None of it reached the log.
    assert_eq!(
        store
            .last_seq(&SessionId::new(session.clone()))
            .await
            .unwrap(),
        0
    );
}
