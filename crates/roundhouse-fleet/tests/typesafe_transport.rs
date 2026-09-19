// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What actually reaches a socket, against a loopback upstream.
//!
//! A hand-rolled axum server rather than a mocking crate, matching the rest of
//! this workspace — and it is what lets a test assert the exact headers that
//! *arrived*, which is the whole question an authenticating client has to
//! answer. Loopback only: no test here reaches a third party.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use tokio::sync::Mutex;

use roundhouse_core::control::{PresentedCredential, Secret, TurnCredential};
use roundhouse_fleet::typesafe::{
    ChoiceQuestion, SystemOneClient, SystemOneError, SystemOneLimits, SystemOneRequest,
};

const KEY: &str = "sk-ZZZQQQ-typesafe-deployment-key";
const DEADLINE_MS: u64 = 400;

/// What the upstream should do when a request arrives.
#[derive(Clone, Copy)]
enum Behaviour {
    Answer,
    /// Headers after `0`, then the body after another `1`, as fractions of the
    /// deadline. Together they exceed one deadline but neither alone does.
    SlowHeadersThenSlowBody,
    /// Nothing at all until well past the deadline.
    SlowHeaders,
    Status(u16),
    /// A body far larger than the configured response bound.
    Huge,
}

#[derive(Clone)]
struct Upstream {
    behaviour: Behaviour,
    seen: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>,
    calls: Arc<AtomicUsize>,
}

fn answer_body() -> &'static str {
    r#"{"model":"jev-1.12","answers":{"tier":{"type":"choice","choice":"capable","probabilities":{"capable":0.85,"efficient":0.15},"confidence":0.82}},"usage":{"input_tokens":312,"output_tokens":48}}"#
}

async fn handle(State(state): State<Upstream>, headers: HeaderMap, body: String) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    let parsed = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    state.seen.lock().await.push((headers, parsed));
    let deadline = Duration::from_millis(DEADLINE_MS);
    match state.behaviour {
        Behaviour::Answer => Response::new(Body::from(answer_body())),
        Behaviour::Status(code) => Response::builder()
            .status(StatusCode::from_u16(code).unwrap())
            .body(Body::from(r#"{"error":"invalid"}"#))
            .unwrap(),
        Behaviour::Huge => Response::new(Body::from("x".repeat(64 * 1024))),
        Behaviour::SlowHeaders => {
            tokio::time::sleep(deadline * 3).await;
            Response::new(Body::from(answer_body()))
        }
        Behaviour::SlowHeadersThenSlowBody => {
            // Headers at 0.6x the deadline, the body at 1.2x. Neither phase
            // alone exceeds the bound; the call as a whole does.
            tokio::time::sleep(deadline.mul_f64(0.6)).await;
            let stream = futures::stream::once(async move {
                tokio::time::sleep(deadline.mul_f64(0.6)).await;
                Ok::<_, std::io::Error>(axum::body::Bytes::from_static(answer_body().as_bytes()))
            });
            Response::new(Body::from_stream(stream))
        }
    }
}

/// A loopback upstream, and the handle to what it saw.
async fn upstream(behaviour: Behaviour) -> (SocketAddr, Upstream) {
    let state = Upstream {
        behaviour,
        seen: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/systemone", post(handle))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, state)
}

fn limits() -> SystemOneLimits {
    SystemOneLimits {
        max_request_bytes: 8 * 1024,
        max_response_bytes: 16 * 1024,
        deadline_ms: DEADLINE_MS,
    }
}

fn question() -> ChoiceQuestion {
    ChoiceQuestion {
        key: "tier".into(),
        instructions: "Which kind of model should answer this?".into(),
        criteria: BTreeMap::from([
            ("capable".to_string(), "Hard, multi-step work".to_string()),
            (
                "efficient".to_string(),
                "Routine, checkable work".to_string(),
            ),
        ]),
    }
}

fn request(state: &str) -> SystemOneRequest {
    SystemOneRequest {
        model: "jev-1.12".into(),
        state: state.into(),
        question: question(),
    }
}

fn stored() -> TurnCredential {
    TurnCredential::Stored(Secret::api_key(KEY).unwrap())
}

/// Prepare and send, which is what every caller does. Two steps in the client
/// because the policy boundary reserves budget between them; one helper here
/// because no assertion in this file is about that gap.
async fn ask(
    client: &SystemOneClient,
    request: &SystemOneRequest,
    credential: &TurnCredential,
) -> Result<roundhouse_fleet::typesafe::SystemOneReply, SystemOneError> {
    client.send(client.prepare(request, credential)?).await
}

/// D7. A transport error must not carry the configured URL.
///
/// The module used to assert that a `reqwest` error's `Display` "carries the
/// URL and never a header or a body, so there is nothing here to redact". The
/// first half is true and the conclusion does not follow: a base URL is
/// deployment configuration, and deployments put tenant ids and gateway tokens
/// in one. The sentinel below stands in for anything of that kind — it is a
/// synthetic string, not a secret, and nothing here reads a real credential.
#[tokio::test]
async fn a_transport_error_does_not_carry_the_configured_url() {
    const SENTINEL: &str = "ZZZQQQ-sentinel-in-the-configured-url";
    // Port 1 on loopback refuses immediately: a connection failure with no
    // server in it, which is the shortest path to a `Transport` error.
    let client = SystemOneClient::new(format!("http://127.0.0.1:1/{SENTINEL}"), limits()).unwrap();

    let error = ask(&client, &request("z"), &stored())
        .await
        .expect_err("nothing is listening on port 1");
    let shown = format!("{error:?} {error}");
    assert!(
        !shown.contains(SENTINEL),
        "a transport error carried the configured URL into whatever logged it: {shown}"
    );
    // The controls: it is still the transport arm, still reports that this was
    // not a timeout, and still says something. A message emptied to pass the
    // assertion above would be useless to whoever has to diagnose it.
    assert!(
        matches!(
            error,
            SystemOneError::Transport {
                timed_out: false,
                ..
            }
        ),
        "{error:?}"
    );
    match &error {
        SystemOneError::Transport { message, .. } => {
            assert!(
                !message.is_empty(),
                "the diagnosis must survive the redaction"
            )
        }
        other => panic!("{other:?}"),
    }
}

/// The bearer and the documented body reach the upstream.
#[tokio::test]
async fn the_bearer_and_the_keyed_question_arrive() {
    let (addr, state) = upstream(Behaviour::Answer).await;
    let client = SystemOneClient::new(format!("http://{addr}"), limits()).unwrap();

    let reply = ask(
        &client,
        &request("the parser drops trailing commas"),
        &stored(),
    )
    .await
    .expect("the loopback upstream answers");
    assert_eq!(reply.usage.unwrap().input_tokens, 312);

    let seen = state.seen.lock().await;
    let (headers, body) = seen.first().expect("exactly one request arrived");
    assert_eq!(
        headers.get("authorization").map(|v| v.to_str().unwrap()),
        Some(format!("Bearer {KEY}").as_str()),
        "the deployment key must reach the upstream it is being spent at"
    );
    assert_eq!(
        headers
            .get("content-type")
            .map(|v| v.to_str().unwrap().split(';').next().unwrap()),
        Some("application/json")
    );
    assert_eq!(body["model"], "jev-1.12");
    assert_eq!(body["state"], "the parser drops trailing commas");
    assert_eq!(body["questions"]["tier"]["type"], "choice");
    assert_eq!(
        body["questions"]["tier"]["criteria"]["capable"],
        "Hard, multi-step work"
    );
}

/// D2. One deadline covers the whole call, and names the same failure whichever
/// phase is slow.
///
/// The two shapes exist to separate the bound from the reason. A per-phase pair
/// of timeouts still refuses *eventually*, so elapsed time alone does not show
/// the defect — what does is a caller that cannot read one reason, because a
/// slow body is reported as a transport error and a slow header as a deadline.
#[tokio::test]
async fn one_deadline_bounds_the_whole_call_and_names_one_reason() {
    for (why, behaviour) in [
        ("headers withheld past the deadline", Behaviour::SlowHeaders),
        (
            "headers and body each inside the deadline but not together",
            Behaviour::SlowHeadersThenSlowBody,
        ),
    ] {
        let (addr, _state) = upstream(behaviour).await;
        let client = SystemOneClient::new(format!("http://{addr}"), limits()).unwrap();

        let started = Instant::now();
        let outcome = ask(&client, &request("z"), &stored()).await;
        let elapsed = started.elapsed();

        assert_eq!(
            outcome,
            Err(SystemOneError::DeadlineExceeded),
            "{why}: a caller choosing a fallback reads one reason for one \
             condition, and a transport error here is the same timeout wearing \
             another name"
        );
        assert!(
            elapsed < Duration::from_millis(DEADLINE_MS * 2),
            "{why}: the deadline bounds the call, not each phase of it \
             (elapsed {elapsed:?})"
        );
    }
}

/// A call that finishes inside the deadline is not refused by it — the control
/// that keeps the two assertions above from passing on a client that refuses
/// everything.
#[tokio::test]
async fn a_prompt_upstream_is_not_refused_by_the_deadline() {
    let (addr, _state) = upstream(Behaviour::Answer).await;
    let client = SystemOneClient::new(format!("http://{addr}"), limits()).unwrap();
    assert!(ask(&client, &request("z"), &stored()).await.is_ok());
}

/// An oversized request is refused before a socket.
#[tokio::test]
async fn an_oversized_request_makes_no_http_call() {
    let (addr, state) = upstream(Behaviour::Answer).await;
    let client = SystemOneClient::new(format!("http://{addr}"), limits()).unwrap();

    let huge = "a".repeat(limits().max_request_bytes + 1);
    let outcome = ask(&client, &request(&huge), &stored()).await;

    assert!(
        matches!(outcome, Err(SystemOneError::RequestTooLarge { limit_bytes, .. })
            if limit_bytes == limits().max_request_bytes),
        "an over-cap body is refused by its own named error: {outcome:?}"
    );
    assert_eq!(
        state.calls.load(Ordering::SeqCst),
        0,
        "refused before a socket: a request that cannot be sent must not be \
         sent and then regretted"
    );
}

/// A response past the buffer bound is refused rather than accumulated.
#[tokio::test]
async fn an_oversized_response_is_refused() {
    let (addr, _state) = upstream(Behaviour::Huge).await;
    let client = SystemOneClient::new(format!("http://{addr}"), limits()).unwrap();
    assert!(matches!(
        ask(&client, &request("z"), &stored()).await,
        Err(SystemOneError::ResponseTooLarge { .. })
    ));
}

/// A refused status is one call, reported without its body, and never retried.
#[tokio::test]
async fn a_refused_status_is_reported_without_its_body_and_not_retried() {
    for code in [401u16, 422, 429, 529] {
        let (addr, state) = upstream(Behaviour::Status(code)).await;
        let client = SystemOneClient::new(format!("http://{addr}"), limits()).unwrap();

        let outcome = ask(&client, &request("z"), &stored()).await;
        assert_eq!(outcome, Err(SystemOneError::Status { status: code }));
        assert_eq!(
            state.calls.load(Ordering::SeqCst),
            1,
            "429 and 529 are the two the docs say to back off on, and a shadow \
             call answers a question nobody is waiting on: one charge, not two"
        );
    }
}

/// Neither a forwarded seat nor an absent credential reaches a socket.
///
/// The forwarded arm is the sharper one: a tenant's own bearer is not
/// roundhouse's to spend at a service the tenant never authenticated against,
/// and this evaluation is deployment work paid for with the deployment's key.
#[tokio::test]
async fn an_unspendable_credential_is_refused_before_a_socket() {
    let forwarded = TurnCredential::Forwarded(
        PresentedCredential::captured(|name| match name {
            "authorization" => Some("Bearer tenant-seat-ZZZQQQ".to_string()),
            _ => None,
        })
        .expect("a bearer was presented")
        .for_provider("anthropic")
        .expect("anthropic has an allowlist row"),
    );
    for (why, credential, expected) in [
        (
            "a forwarded tenant seat",
            forwarded,
            SystemOneError::ForwardedCredentialRefused,
        ),
        (
            "no credential at all",
            TurnCredential::Absent,
            SystemOneError::Credential(
                TurnCredential::Absent
                    .require_api_key("typesafe")
                    .expect_err("Absent never yields a key"),
            ),
        ),
    ] {
        let (addr, state) = upstream(Behaviour::Answer).await;
        let client = SystemOneClient::new(format!("http://{addr}"), limits()).unwrap();
        assert_eq!(
            ask(&client, &request("z"), &credential).await,
            Err(expected),
            "{why}"
        );
        assert_eq!(state.calls.load(Ordering::SeqCst), 0, "{why}");
        // The same refusal comes out of `prepare`, which is what lets the
        // policy boundary decide before it holds any budget.
        assert!(client.prepare(&request("z"), &credential).is_err(), "{why}");
    }
}
