// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What actually arrives at an Anthropic Messages upstream, asserted against a
//! real socket.
//!
//! The twin of `openai_responses_upstream.rs`, deliberately the same shape: a
//! hand-rolled axum mock that records the *whole* header map of every request,
//! three behaviours, and assertions about what a server received rather than
//! about what a client built. The unit tests beside the client already assert
//! the second; every layer between — `reqwest`'s header handling, the redirect
//! policy, `hyper`'s sensitive-header treatment — sits in the gap, and a
//! credential leak is precisely a thing that happens there.
//!
//! Three claims are specific to this dialect and have no analogue next door:
//!
//! 1. **A stored key goes out bare in `x-api-key`, never as a bearer.** Porting
//!    the Responses client's `Authorization: Bearer` here produces a 401 whose
//!    message does not say why, so the negative half of that assertion is worth
//!    as much as the positive half.
//! 2. **`anthropic-version` is on every request**, both auth modes, stamped by
//!    the client rather than copied from anything a caller sent.
//! 3. **The seat carries `anthropic-beta` through.** Stripping the
//!    `oauth-2025-04-20` beta from a subscription bearer is a documented 401 —
//!    so a forwarding path that dropped it would look like a revoked login.

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
use roundhouse_fleet::anthropic_messages::AnthropicMessagesClient;
use roundhouse_fleet::{FrontierChunk, FrontierClient, FrontierError, FrontierQuote, WireProtocol};

/// A Claude Code subscription seat's OAuth bearer. Shaped like the real thing
/// (an `sk-ant-oat`-class token arrives as a bearer) with a tail that appears
/// nowhere else in this file, so a scan that finds it found the real thing.
const SEAT_BEARER: &str = "Bearer sk-ant-oat01-ZZZQQQ-seat-token";
/// The betas a real Claude Code request carries. `oauth-2025-04-20` is the one
/// the upstream refuses the bearer without.
const SEAT_BETAS: &str = "oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14";
/// A header the caller sent that no allowlist row names. It must not arrive.
const SEAT_SESSION_HEADER: &str = "5f3b9a10-ZZZQQQ-not-forwardable";
/// The deployment's own stored key. Also unique, for the same reason.
const STORED_KEY: &str = "sk-ant-api03-ZZZQQQ1111-deployment-key";

/// One complete Messages stream, on a warm prefix.
///
/// Everything this dialect's decoder has to fold is in here on purpose:
/// `message_start` carries the three disjoint input counters *and* the
/// `cache_creation` TTL breakdown no other crate models; a `ping` sits between
/// two text deltas, because that is what a real keepalive looks like and a
/// decoder that treated it as an unknown frame would be indistinguishable until
/// it was not; and the output count arrives only on the final `message_delta`.
const SSE_BODY: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"type":"message","id":"msg_ZZZ","#,
    r#""role":"assistant","model":"claude-x","content":[],"stop_reason":null,"#,
    r#""stop_sequence":null,"usage":{"input_tokens":12,"cache_read_input_tokens":9000,"#,
    r#""cache_creation_input_tokens":500,"output_tokens":1,"#,
    r#""cache_creation":{"ephemeral_5m_input_tokens":400,"ephemeral_1h_input_tokens":100}}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}"#,
    "\n\n",
    "event: ping\n",
    r#"data: {"type":"ping"}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"#,
    r#""usage":{"output_tokens":64}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n"
);

/// The same stream with the upstream disappearing after the accounting is
/// complete but before the terminal frame.
///
/// The *hardest* place to cut it: every count a `Done` needs has already
/// arrived, so a decoder that emitted one here would produce a
/// correct-looking accounting record for a turn that never finished — worse
/// than none, because the engine's estimated-and-marked path would never run.
const SSE_CUT_SHORT: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"type":"message","id":"msg_ZZZ","#,
    r#""role":"assistant","model":"claude-x","content":[],"stop_reason":null,"#,
    r#""stop_sequence":null,"usage":{"input_tokens":12,"cache_read_input_tokens":9000,"#,
    r#""cache_creation_input_tokens":500,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half"}}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"#,
    r#""usage":{"output_tokens":64}}"#,
    "\n\n"
);

/// A tool-use turn: a sentence, then a `tool_use` block whose arguments arrive
/// as fragments, then `stop_reason: tool_use`.
///
/// The shape of every turn in an agentic loop, and the one this dialect could
/// not carry through roundhouse before M11.2. The fragments are split
/// mid-token on purpose — `{"pat` is not JSON — because that is what the wire
/// sends and it is the whole reason a decoder needs an accumulator here.
const SSE_TOOL_USE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"type":"message","id":"msg_ZZZ","#,
    r#""role":"assistant","model":"claude-x","content":[],"stop_reason":null,"#,
    r#""stop_sequence":null,"usage":{"input_tokens":12,"cache_read_input_tokens":9000,"#,
    r#""cache_creation_input_tokens":500,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","#,
    r#""text":"Let me search."}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","#,
    r#""id":"toolu_01ZZZ","name":"Grep","input":{}}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","#,
    r#""partial_json":"{\"pat"}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","#,
    r#""partial_json":"tern\":\"fn main\"}"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":1}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"#,
    r#""usage":{"output_tokens":64}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n"
);

/// A stream that fails halfway, in the shape Anthropic actually sends.
const SSE_MID_STREAM_ERROR: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"type":"message","id":"msg_ZZZ","#,
    r#""role":"assistant","model":"claude-x","content":[],"stop_reason":null,"#,
    r#""stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"part"}}"#,
    "\n\n",
    "event: error\n",
    r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    "\n\n"
);

/// What the mock upstream does when a request arrives.
#[derive(Clone)]
enum Behaviour {
    /// Answer with the body given.
    Stream(&'static str),
    /// Answer `401` with a body that quotes the caller's credential back, which
    /// is what a real provider does and what makes redaction load-bearing.
    EchoTheCredential,
    /// Answer `307` to `location`, which a credential must not follow.
    RedirectTo(String),
}

#[derive(Clone)]
struct Upstream {
    behaviour: Behaviour,
    seen: Arc<Mutex<Vec<HeaderMap>>>,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl Upstream {
    /// Bind a mock upstream on a loopback port and return its base URL.
    ///
    /// Mounted at `/v1/messages` because that is the client's own
    /// `DEFAULT_MESSAGES_PATH`: a test that mounted the handler at `/` would
    /// pass on a client that had lost the path entirely.
    async fn spawn(behaviour: Behaviour) -> (String, Upstream) {
        let state = Upstream {
            behaviour,
            seen: Arc::new(Mutex::new(Vec::new())),
            bodies: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/v1/messages", post(handle))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), state)
    }

    /// Every header the upstream received, lowercased, as `name: value` lines.
    ///
    /// A rendered string rather than a map because the control assertions are
    /// negative — *this* must appear nowhere in what arrived — and a substring
    /// scan over the whole map is the only way to say that without enumerating
    /// the headers a leak might hide in.
    fn arrived(&self) -> String {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .flat_map(|headers| {
                headers.iter().map(|(name, value)| {
                    format!("{name}: {}", value.to_str().unwrap_or("<not utf-8>"))
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

async fn handle(State(state): State<Upstream>, headers: HeaderMap, body: String) -> Response {
    state.seen.lock().unwrap().push(headers.clone());
    state.bodies.lock().unwrap().push(body);
    // Whichever header this request authenticated with, so the 401 arm can
    // quote it back the way a real provider does. Both are tried because the
    // two auth modes use different names and only one of them is ever set.
    let credential = ["authorization", "x-api-key"]
        .into_iter()
        .filter_map(|name| headers.get(name))
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join(" ");
    match &state.behaviour {
        Behaviour::Stream(body) => (
            StatusCode::OK,
            [("content-type", "text/event-stream")],
            *body,
        )
            .into_response(),
        Behaviour::EchoTheCredential => (
            StatusCode::UNAUTHORIZED,
            format!(
                r#"{{"type":"error","error":{{"type":"authentication_error",
                 "message":"invalid x-api-key: {credential}"}}}}"#
            ),
        )
            .into_response(),
        Behaviour::RedirectTo(location) => (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", format!("{location}/v1/messages"))],
        )
            .into_response(),
    }
}

fn quote(credential: TurnCredential) -> FrontierQuote {
    FrontierQuote {
        target: Target::Frontier {
            provider: "anthropic".into(),
            model: "claude-x".into(),
        },
        wire_protocol: WireProtocol::AnthropicMessages,
        prompt: "<|system|>be brief<|user|>how many tokens did that turn bill?".into(),
        // A real quote from the engine carries the item boundaries; this one
        // does too, so the body that goes over the socket is the blocked shape
        // rather than the degenerate single-block one.
        segment_boundaries: vec!["<|system|>be brief".len()],
        prompt_cache_key: "sess_anthropic_upstream".into(),
        expected_output_tokens: Some(512),
        // No client declared a ceiling on these fixtures, which is what
        // every internal caller looks like; see `output_token_cap`. Nor tools,
        // so these dispatches are also the control for "a quote with none sends
        // no `tools` key".
        output_token_cap: None,
        tools: None,
        tool_choice: None,
        credential,
    }
}

fn stored() -> TurnCredential {
    TurnCredential::Stored(Secret::api_key(STORED_KEY).expect("an ordinary API key"))
}

/// What the request edge captures on a pass-through turn, narrowed to Anthropic.
///
/// The non-allowlisted header is presented too, because the claim under test is
/// that the *table* decides rather than the client: a capture that never held it
/// would prove nothing about the narrowing.
fn seat() -> TurnCredential {
    TurnCredential::Forwarded(
        PresentedCredential::captured(|name| match name {
            "authorization" => Some(SEAT_BEARER.to_string()),
            "anthropic-beta" => Some(SEAT_BETAS.to_string()),
            "x-claude-code-session-id" => Some(SEAT_SESSION_HEADER.to_string()),
            _ => None,
        })
        .expect("a bearer was presented")
        .for_provider("anthropic")
        .expect("anthropic has an allowlist row"),
    )
}

async fn drain(
    stream: roundhouse_fleet::FrontierStream,
) -> Result<Vec<FrontierChunk>, FrontierError> {
    stream.collect::<Vec<_>>().await.into_iter().collect()
}

#[tokio::test]
async fn a_stored_key_arrives_bare_in_x_api_key_and_never_as_a_bearer() {
    let (base, upstream) = Upstream::spawn(Behaviour::Stream(SSE_BODY)).await;
    let client = AnthropicMessagesClient::with_bases(&base, &base).unwrap();

    let chunks = drain(client.execute(&quote(stored())).await.unwrap())
        .await
        .unwrap();

    let arrived = upstream.arrived();
    // PROBE: the key arrives, in Anthropic's header, with no scheme prefix.
    assert!(
        arrived.contains(&format!("x-api-key: {STORED_KEY}")),
        "the stored key must arrive bare in `x-api-key`; upstream saw:\n{arrived}"
    );
    // CONTROL, and it is the porting mistake this whole client exists to not
    // make: a `Bearer` to `api.anthropic.com` is a 401 whose message does not
    // say why. A `contains(STORED_KEY)` alone would pass on a client that sent
    // the key both ways.
    assert!(
        !arrived.to_ascii_lowercase().contains("authorization:"),
        "no `Authorization` header may ride a stored Anthropic key; upstream saw:\n{arrived}"
    );
    assert!(!arrived.contains("Bearer"), "{arrived}");
    // The envelope header the upstream refuses a request without.
    assert!(
        arrived.contains("anthropic-version: 2023-06-01"),
        "every Messages request must declare a version; upstream saw:\n{arrived}"
    );
    // And nothing of a seat's rides a BYOK turn.
    assert!(!arrived.contains(SEAT_BEARER), "{arrived}");
    assert!(!arrived.contains(SEAT_SESSION_HEADER), "{arrived}");

    // The body really is the blocked shape, so the header assertions above are
    // about a request the upstream could have served rather than one this
    // client mangled on the way out.
    let body: serde_json::Value =
        serde_json::from_str(&upstream.bodies.lock().unwrap()[0]).expect("the body is JSON");
    let blocks = body["messages"][0]["content"]
        .as_array()
        .expect("one user message carrying several blocks");
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["text"], "<|system|>be brief");
    assert_eq!(
        blocks[0]["cache_control"]["type"], "ephemeral",
        "the breakpoint sits on the stable prefix, which is the whole reason \
         this client blocks the prompt at all: {body}"
    );
    assert!(blocks[1].get("cache_control").is_none());

    // **The fold, over a socket.** Anthropic's three input counters are
    // disjoint; roundhouse's `input_tokens` is their total, with the two details
    // as components of it. 12 fresh + 9 000 read + 500 written = 9 512.
    assert_eq!(
        chunks,
        vec![
            FrontierChunk::OutputText("hel".into()),
            FrontierChunk::OutputText("lo".into()),
            FrontierChunk::Done {
                input_tokens: 9_512,
                cached_input_tokens: 9_000,
                cache_write_tokens: 500,
                output_tokens: 64,
                reasoning_tokens: 0,
                provider_reported_cost: None,
                // The fixture's final `message_delta` says `end_turn`, and the
                // word survives the whole dispatch rather than only the
                // decoder's unit tests.
                stop_reason: Some("end_turn".into()),
            },
        ],
        "a client reading only `message_delta` reports zero input and no cache reads, \
         which is the one quantity this system exists to maximize"
    );
}

/// **The agentic turn, end to end over a socket: tools out, a call back.**
///
/// The two halves of M11.2's core, joined at the only place they meet — the
/// client's `tools` reach the upstream on the request, and the `tool_use` block
/// the model answers with reaches the caller as one `FrontierChunk::ToolCall`.
/// Each half is pinned by unit tests already; what nothing pinned is that they
/// survive `reqwest`, a real body serialization and a real chunked SSE read
/// together.
#[tokio::test]
async fn a_tool_using_turn_sends_the_clients_tools_and_yields_one_completed_call() {
    let (base, upstream) = Upstream::spawn(Behaviour::Stream(SSE_TOOL_USE)).await;
    let client = AnthropicMessagesClient::with_bases(&base, &base).unwrap();

    let tools = serde_json::json!([{
        "name": "Grep",
        "description": "search the tree",
        "input_schema": {
            "type": "object",
            "properties": { "pattern": { "type": "string" } },
            "required": ["pattern"],
        },
        // The client's own breakpoint on its last tool. A typed re-encoding
        // would have dropped it, and with it the discount on the largest stable
        // block in the request.
        "cache_control": { "type": "ephemeral" },
    }]);
    let tool_choice = serde_json::json!({ "type": "auto" });
    let mut quote = quote(stored());
    quote.tools = Some(tools.clone());
    quote.tool_choice = Some(tool_choice.clone());

    let chunks = drain(client.execute(&quote).await.unwrap()).await.unwrap();

    // Half one: what the upstream received.
    let body: serde_json::Value =
        serde_json::from_str(&upstream.bodies.lock().unwrap()[0]).expect("the body is JSON");
    assert_eq!(
        body["tools"], tools,
        "the model is told about exactly the toolbox the client declared: {body}"
    );
    assert_eq!(body["tool_choice"], tool_choice);

    // Half two: what came back. One call, assembled from two fragments neither
    // of which is JSON alone, with the text that preceded it still ordered
    // before it.
    assert_eq!(
        chunks,
        vec![
            FrontierChunk::OutputText("Let me search.".into()),
            FrontierChunk::ToolCall {
                id: "toolu_01ZZZ".into(),
                name: "Grep".into(),
                arguments: r#"{"pattern":"fn main"}"#.into(),
            },
            FrontierChunk::Done {
                input_tokens: 9_512,
                cached_input_tokens: 9_000,
                cache_write_tokens: 500,
                output_tokens: 64,
                reasoning_tokens: 0,
                provider_reported_cost: None,
                // The signal that the turn is waiting on the client rather than
                // finished. Unreachable in this system before M11.2.
                stop_reason: Some("tool_use".into()),
            },
        ],
    );
}

#[tokio::test]
async fn a_seat_forwards_exactly_the_allowlisted_headers_and_the_client_stamps_the_version() {
    let (base, upstream) = Upstream::spawn(Behaviour::Stream(SSE_BODY)).await;
    let client = AnthropicMessagesClient::with_bases(&base, &base).unwrap();

    drain(client.execute(&quote(seat())).await.unwrap())
        .await
        .unwrap();

    let arrived = upstream.arrived();
    // PROBE: byte-for-byte. "Verbatim" is the claim, so it is an exact match
    // rather than a `contains` of the token: a client that re-wrapped the value
    // ("Bearer Bearer sk-ant-oat...") passes a looser test and fails every real
    // request.
    assert!(
        arrived.contains(&format!("authorization: {SEAT_BEARER}")),
        "the seat's own bearer must arrive unchanged; upstream saw:\n{arrived}"
    );
    // The beta is the half a reader would think optional. It is not: stripping
    // `oauth-2025-04-20` from a subscription bearer is a documented 401, which
    // surfaces to the user as a login that stopped working.
    assert!(
        arrived.contains(&format!("anthropic-beta: {SEAT_BETAS}")),
        "upstream saw:\n{arrived}"
    );
    // CONTROL 1: a header the caller sent that no row names never reaches the
    // wire. The capture held it -- `seat()` presents it -- so this is the
    // allowlist narrowing, not an absence of input.
    assert!(
        !arrived.contains(SEAT_SESSION_HEADER),
        "a header nobody's allowlist row names must not be forwarded; upstream saw:\n{arrived}"
    );
    // CONTROL 2, and it is what makes pass-through *pass-through*: roundhouse's
    // own key is nowhere in the request. A client that resolved a stored key
    // beside the forwarded bearer would authenticate as the deployment while
    // claiming to be the seat.
    assert!(
        !arrived.contains(STORED_KEY),
        "no key of roundhouse's own may ride a forwarded turn; upstream saw:\n{arrived}"
    );
    assert!(
        !arrived.to_ascii_lowercase().contains("x-api-key"),
        "{arrived}"
    );
    // And the version is stamped on this route too, by the client rather than
    // by the caller: it describes the body this client serialized.
    assert!(
        arrived.contains("anthropic-version: 2023-06-01"),
        "{arrived}"
    );
}

#[tokio::test]
async fn a_forwarded_seat_never_follows_a_redirect_to_another_origin() {
    // The second origin, which must never see the seat's bearer.
    let (elsewhere, elsewhere_upstream) = Upstream::spawn(Behaviour::Stream(SSE_BODY)).await;
    let (base, _) = Upstream::spawn(Behaviour::RedirectTo(elsewhere.clone())).await;
    let client = AnthropicMessagesClient::with_bases(&base, &base).unwrap();

    // The leak is asserted first, and before the call's own outcome, because it
    // is the claim: a client that followed the redirect and then succeeded must
    // fail this test on the disclosure rather than on a return value.
    let outcome = client.execute(&quote(seat())).await;
    let leaked = elsewhere_upstream.arrived();
    assert!(
        leaked.is_empty(),
        "a forwarded seat followed a redirect to another origin; that origin saw:\n{leaked}"
    );

    let Err(error) = outcome else {
        panic!("a redirect is not a response this client accepts")
    };
    assert!(error.to_string().contains("307"), "{error}");
}

/// F1 (thermo-nuclear review of d0821f9, **valid**): the twin of the seat test
/// above, for the *stored* route instead of the forwarded one.
///
/// `route()` put the stored key on `self.direct`, which `with_bases` built with
/// reqwest's default redirect-following policy — the arrangement ported from
/// the Responses client, where it is safe. It is not safe here, and the reason
/// is one header name: reqwest's cross-host sanitizer
/// (`redirect.rs::remove_sensitive_headers`) strips `Authorization`, `Cookie`,
/// `Cookie2`, `Proxy-Authorization` and `WWW-Authenticate`, and nothing else.
/// A stored Anthropic key rides `x-api-key`, so it followed the 307 to
/// `elsewhere` bare — and this test, before the fix, printed it.
///
/// Both transports are `Policy::none()` now. The negative is asserted against a
/// real socket rather than against the builder, because "which policy the client
/// was constructed with" is not the claim; "what the second origin received" is,
/// and every layer between the two is exactly where a leak of this kind lives.
#[tokio::test]
async fn a_stored_key_never_follows_a_redirect_to_another_origin() {
    // The second origin, which must never see the deployment's own key.
    let (elsewhere, elsewhere_upstream) = Upstream::spawn(Behaviour::Stream(SSE_BODY)).await;
    let (base, _) = Upstream::spawn(Behaviour::RedirectTo(elsewhere.clone())).await;
    let client = AnthropicMessagesClient::with_bases(&base, &base).unwrap();

    // The leak is asserted first, and before the call's own outcome, for the
    // same reason as the seat test: a client that followed the redirect and
    // then succeeded must fail this test on the disclosure, not on the shape
    // of its return value.
    let outcome = client.execute(&quote(stored())).await;
    let leaked = elsewhere_upstream.arrived();
    assert!(
        leaked.is_empty(),
        "a stored key followed a redirect to another origin; that origin saw:\n{leaked}"
    );

    let Err(error) = outcome else {
        panic!("a redirect is not a response this client accepts")
    };
    assert!(error.to_string().contains("307"), "{error}");
    // The refusal names what it refused. A 3xx carries no body, so without this
    // an operator whose gateway started redirecting reads `the upstream
    // answered 307:` and has nothing to act on.
    assert!(
        error.to_string().contains("redirected to") && error.to_string().contains(&elsewhere),
        "{error}"
    );
}

#[tokio::test]
async fn an_upstream_that_echoes_a_credential_is_redacted_before_anyone_reads_it() {
    for (credential, secret) in [(stored(), STORED_KEY), (seat(), SEAT_BEARER)] {
        let (base, _) = Upstream::spawn(Behaviour::EchoTheCredential).await;
        let client = AnthropicMessagesClient::with_bases(&base, &base).unwrap();

        // PROBE: a 401 whose body quotes the credential back, which is what a
        // real provider does. What comes out of `execute` is what a client sees,
        // what a `tracing` line carries, and what an event payload would hold.
        let Err(error) = client.execute(&quote(credential)).await else {
            panic!("a 401 is an error")
        };
        let message = error.to_string();
        assert!(
            !message.contains(secret),
            "the upstream echoed the credential and it survived to the caller: {message}"
        );
        assert!(message.contains("[REDACTED]"), "{message}");
        // The diagnosis survives the redaction, or an operator is left with an
        // error that says only that something was removed.
        assert!(message.contains("401"), "{message}");
        assert!(message.contains("authentication_error"), "{message}");
        // CONTROL: the scrub takes out the credential and not the upstream's
        // meaning. A blanket wipe of the body satisfies every assertion above.
        assert!(!message.contains("[REDACTED][REDACTED]"), "{message}");
    }
}

#[tokio::test]
async fn a_stream_cut_before_message_stop_yields_no_accounting_frame() {
    let (base, _) = Upstream::spawn(Behaviour::Stream(SSE_CUT_SHORT)).await;
    let client = AnthropicMessagesClient::with_bases(&base, &base).unwrap();

    // PROBE: the upstream closed the connection with every count already
    // reported. A `Done` here reads downstream as a completed turn, and its
    // token counts would be booked as the bill.
    let chunks = drain(client.execute(&quote(stored())).await.unwrap())
        .await
        .unwrap();
    assert_eq!(chunks, vec![FrontierChunk::OutputText("half".into())]);

    // CONTROL: the same client against the same mock with the terminal frame
    // present. One fixture different, and the accounting arrives -- which is
    // what makes the assertion above about `message_stop` rather than about the
    // fold being broken over a socket.
    let (base, _) = Upstream::spawn(Behaviour::Stream(SSE_BODY)).await;
    let client = AnthropicMessagesClient::with_bases(&base, &base).unwrap();
    let complete = drain(client.execute(&quote(stored())).await.unwrap())
        .await
        .unwrap();
    assert!(matches!(complete.last(), Some(FrontierChunk::Done { .. })));
}

#[tokio::test]
async fn a_mid_stream_error_event_fails_the_dispatch() {
    let (base, _) = Upstream::spawn(Behaviour::Stream(SSE_MID_STREAM_ERROR)).await;
    let client = AnthropicMessagesClient::with_bases(&base, &base).unwrap();

    // The HTTP response is a 200: the failure arrives inside a stream that
    // already started, which is the shape a status-code check cannot see. A
    // client that skipped the frame would hand the engine a truncated answer
    // and no accounting, and the turn would read as a short reply.
    let outcome = drain(client.execute(&quote(stored())).await.unwrap()).await;
    let Err(error) = outcome else {
        panic!("a mid-stream error must fail the turn, not truncate it")
    };
    assert!(
        error.to_string().contains("overloaded_error"),
        "the upstream's own reason has to survive, because it decides whether a \
         retry is worth making: {error}"
    );
}
