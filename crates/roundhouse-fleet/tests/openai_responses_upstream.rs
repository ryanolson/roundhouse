// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What actually arrives at the upstream, asserted against a real socket.
//!
//! The unit tests beside the client assert what it *builds*. These assert what
//! a server *receives*, which is a different claim and the one that matters:
//! every layer between — `reqwest`'s own header handling, the redirect policy,
//! `hyper`'s sensitive-header treatment — sits between the two, and a
//! credential leak is precisely a thing that happens in that gap.
//!
//! The mock is a hand-rolled axum server rather than a mocking crate, matching
//! how the rest of this workspace stands in for an HTTP dependency, and it
//! records the *whole* header map of every request. Recording only the headers
//! a test expects would make the control assertions — "nothing else
//! secret-shaped arrived" — impossible to write.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::StreamExt;

use roundhouse_core::control::{PresentedCredential, Secret, TurnCredential};
use roundhouse_core::routing::Target;
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierError, FrontierQuote, OpenAiResponsesClient,
    WireProtocol,
};

/// The caller's own credential on a pass-through turn.
///
/// A JWT because that is what a ChatGPT device login produces (stage 0's
/// ruling, codex `3b45c29`) — and therefore exactly the shape roundhouse
/// refuses to *store*. The tail is a string that appears nowhere else in this
/// file, so a scan that finds it found the real thing.
const SEAT_BEARER: &str = "Bearer eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJhZGEifQ.ZZZQQQ-seat-token";
const SEAT_ACCOUNT: &str = "acct-ZZZQQQ-0000";
/// The deployment's own stored key. Also unique, for the same reason.
const STORED_KEY: &str = "sk-proj-ZZZQQQ1111-deployment-key";

/// One `response.completed` stream with a cache hit in it.
const SSE_BODY: &str = concat!(
    "event: response.output_text.delta\n",
    r#"data: {"type":"response.output_text.delta","delta":"hello"}"#,
    "\n\n",
    "event: response.completed\n",
    r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":120,"#,
    r#""input_tokens_details":{"cached_tokens":100},"output_tokens":30,"#,
    r#""output_tokens_details":{"reasoning_tokens":12}}}}"#,
    "\n\n"
);

/// What the mock upstream does when a request arrives.
#[derive(Clone)]
enum Behaviour {
    /// Answer with [`SSE_BODY`].
    Stream,
    /// Answer `401` with a body that quotes the caller's bearer back, which is
    /// what a real provider does and what makes redaction load-bearing.
    EchoTheCredential,
    /// Answer `307` to `location`, which a credential must not follow.
    RedirectTo(String),
}

#[derive(Clone)]
struct Upstream {
    behaviour: Behaviour,
    seen: Arc<Mutex<Vec<HeaderMap>>>,
}

impl Upstream {
    /// Bind a mock upstream on a loopback port and return its base URL.
    async fn spawn(behaviour: Behaviour) -> (String, Arc<Mutex<Vec<HeaderMap>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = Upstream {
            behaviour,
            seen: Arc::clone(&seen),
        };
        let app = Router::new()
            .route("/responses", post(handle))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }
}

async fn handle(State(state): State<Upstream>, headers: HeaderMap, _body: String) -> Response {
    state.seen.lock().unwrap().push(headers.clone());
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    match &state.behaviour {
        Behaviour::Stream => (
            StatusCode::OK,
            [("content-type", "text/event-stream")],
            SSE_BODY,
        )
            .into_response(),
        Behaviour::EchoTheCredential => (
            StatusCode::UNAUTHORIZED,
            format!(r#"{{"error":{{"message":"invalid token: {authorization}"}}}}"#),
        )
            .into_response(),
        Behaviour::RedirectTo(location) => (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", format!("{location}/responses"))],
        )
            .into_response(),
    }
}

fn quote(credential: TurnCredential) -> FrontierQuote {
    FrontierQuote {
        target: Target::Frontier {
            provider: "openai".into(),
            model: "flagship".into(),
        },
        wire_protocol: WireProtocol::OpenAiResponses,
        prompt: "how many tokens did that turn bill?".into(),
        prompt_cache_key: "sess_upstream".into(),
        expected_output_tokens: Some(512),
        credential,
    }
}

fn stored() -> TurnCredential {
    TurnCredential::Stored(Secret::api_key(STORED_KEY).expect("an ordinary API key"))
}

/// What the request edge captures on a pass-through turn, narrowed to OpenAI.
fn seat() -> TurnCredential {
    TurnCredential::Forwarded(
        PresentedCredential::captured(|name| match name {
            "authorization" => Some(SEAT_BEARER.to_string()),
            "chatgpt-account-id" => Some(SEAT_ACCOUNT.to_string()),
            _ => None,
        })
        .expect("a bearer was presented")
        .for_provider("openai")
        .expect("openai has an allowlist row"),
    )
}

/// Drain a stream into the chunks it produced.
async fn drain(
    stream: roundhouse_fleet::FrontierStream,
) -> Result<Vec<FrontierChunk>, FrontierError> {
    stream.collect::<Vec<_>>().await.into_iter().collect()
}

/// Every header the upstream received, lowercased, as `name: value` lines.
///
/// A rendered string rather than a map because the control assertions are
/// negative — *this* must appear nowhere in what arrived — and a substring scan
/// over the whole map is the only way to say that without enumerating the
/// headers a leak might hide in.
fn arrived(seen: &Arc<Mutex<Vec<HeaderMap>>>) -> String {
    seen.lock()
        .unwrap()
        .iter()
        .flat_map(|headers| {
            headers
                .iter()
                .map(|(name, value)| format!("{name}: {}", value.to_str().unwrap_or("<not utf-8>")))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn a_stored_key_arrives_as_a_bearer_and_nothing_else_secret_shaped_does() {
    let (base, seen) = Upstream::spawn(Behaviour::Stream).await;
    let client = OpenAiResponsesClient::with_bases(&base, &base).unwrap();

    let chunks = drain(client.execute(&quote(stored())).await.unwrap())
        .await
        .unwrap();

    // PROBE: the bearer arrives, built from the one seam that reveals a secret.
    let arrived = arrived(&seen);
    assert!(
        arrived.contains(&format!("authorization: Bearer {STORED_KEY}")),
        "the stored key must arrive as a bearer; upstream saw:\n{arrived}"
    );

    // CONTROL: nothing *else* secret-shaped arrives. A BYOK turn forwards no
    // caller credential, so the pass-through headers must be absent -- if this
    // client set them unconditionally, a deployment's own key and a user's seat
    // identity would both go upstream on every turn.
    for absent in ["chatgpt-account-id", "x-openai-fedramp", "cookie"] {
        assert!(
            !arrived.to_ascii_lowercase().contains(absent),
            "`{absent}` must not arrive on a stored-key turn; upstream saw:\n{arrived}"
        );
    }
    assert!(!arrived.contains(SEAT_BEARER), "{arrived}");

    // And the response really was parsed, so the assertions above are about a
    // turn that worked rather than one that failed before it sent anything.
    assert_eq!(chunks[0], FrontierChunk::OutputText("hello".into()));
    assert_eq!(
        chunks[1],
        FrontierChunk::Done {
            input_tokens: 120,
            cached_input_tokens: 100,
            output_tokens: 30,
            reasoning_tokens: 12,
        },
        "the cached count is the quantity the whole system exists to maximize"
    );
}

#[tokio::test]
async fn a_pass_through_turn_forwards_the_callers_own_credential_verbatim() {
    let (base, seen) = Upstream::spawn(Behaviour::Stream).await;
    let client = OpenAiResponsesClient::with_bases(&base, &base).unwrap();

    drain(client.execute(&quote(seat())).await.unwrap())
        .await
        .unwrap();

    // PROBE: byte-for-byte, both headers codex's `BearerAuthProvider` emits.
    // "Verbatim" is the claim, so it is asserted as an exact match rather than
    // as a `contains` of the token: a client that re-wrapped the value ("Bearer
    // Bearer eyJ...") would pass a looser test and fail every real request.
    let arrived = arrived(&seen);
    assert!(
        arrived.contains(&format!("authorization: {SEAT_BEARER}")),
        "the caller's own bearer must arrive unchanged; upstream saw:\n{arrived}"
    );
    assert!(
        arrived.contains(&format!("chatgpt-account-id: {SEAT_ACCOUNT}")),
        "upstream saw:\n{arrived}"
    );

    // CONTROL, and it is the assertion that makes pass-through *pass-through*:
    // roundhouse's own stored key is nowhere in the request. A client that
    // added its own bearer beside the forwarded one -- or that resolved a
    // stored key and forwarded a header -- would authenticate as the deployment
    // while claiming to be the seat.
    assert!(
        !arrived.contains(STORED_KEY),
        "no key of roundhouse's own may ride a forwarded turn; upstream saw:\n{arrived}"
    );
}

#[tokio::test]
async fn an_upstream_that_echoes_the_forwarded_credential_is_redacted_before_anyone_reads_it() {
    let (base, _) = Upstream::spawn(Behaviour::EchoTheCredential).await;
    let client = OpenAiResponsesClient::with_bases(&base, &base).unwrap();

    // PROBE: a 401 whose body quotes the bearer back, which is what a real
    // provider does. What comes out of `execute` is what a client sees, what a
    // `tracing` line carries, and what an event payload would hold.
    let Err(error) = client.execute(&quote(seat())).await else {
        panic!("a 401 is an error")
    };
    let message = error.to_string();
    assert!(
        !message.contains(SEAT_BEARER),
        "the upstream echoed the credential and it survived to the caller: {message}"
    );
    assert!(message.contains("[REDACTED]"), "{message}");
    // The diagnosis survives the redaction, or an operator is left with an
    // error that says only that something was removed.
    assert!(message.contains("401"), "{message}");
    assert!(message.contains("invalid token"), "{message}");

    // CONTROL: the same upstream on the stored-key route. There is no forwarded
    // credential to redact, and the deployment's own key is echoed -- which is
    // a real disclosure, but a different one, and pretending this test covers
    // it would be worse than saying so. What it does prove is that the
    // redaction above is driven by the *forwarded* credential rather than by a
    // blanket scrub that would also hide the upstream's meaning.
    let Err(stored_error) = client.execute(&quote(stored())).await else {
        panic!("a 401 is an error")
    };
    assert!(stored_error.to_string().contains("invalid token"));
}

#[tokio::test]
async fn a_forwarded_credential_never_follows_a_redirect_to_another_origin() {
    // The second origin, which must never see the caller's bearer.
    let (elsewhere, elsewhere_seen) = Upstream::spawn(Behaviour::Stream).await;
    let (base, _) = Upstream::spawn(Behaviour::RedirectTo(elsewhere.clone())).await;
    let client = OpenAiResponsesClient::with_bases(&base, &base).unwrap();

    // PROBE: the configured upstream answers 307. With `reqwest`'s default
    // policy the client would follow it and re-present the credential at
    // whatever host the `Location` named -- ten times over, by default.
    //
    // The leak is asserted *first*, and before the call's own outcome, because
    // it is the claim: a client that followed the redirect and then succeeded
    // must fail this test on the disclosure rather than on the shape of its
    // return value.
    let outcome = client.execute(&quote(seat())).await;
    let leaked = arrived(&elsewhere_seen);
    assert!(
        leaked.is_empty(),
        "a forwarded credential followed a redirect to another origin; that origin saw:\n{leaked}"
    );

    // And the 3xx is surfaced as an error rather than swallowed: a turn that
    // reached nobody must not look to the engine like a turn that produced
    // nothing.
    let Err(error) = outcome else {
        panic!("a redirect is not a response this client accepts")
    };
    assert!(error.to_string().contains("307"), "{error}");
}
