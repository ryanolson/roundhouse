// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The doubles that drive Codex's own client against this crate.
//!
//! Split out of [`super`] once a third suite needed them. The rule the split
//! follows: this file holds everything a test needs to *be* a Codex client —
//! transport, auth, provider, request, and the event collector — while
//! `common/mod.rs` keeps the fixtures that stand behind the server (catalog,
//! fleet, frontier client). The two sets have no reason to change together,
//! and before the split each new suite copied whichever half it needed.
//!
//! Duplication is not a style objection here. `codex_api` types are pinned to
//! a specific Codex revision, and the whole point of driving the real client
//! is that a field it adds or renames arrives without anyone transcribing it.
//! Three hand-copied `ResponsesApiRequest` literals is three places for that
//! property to quietly stop holding.

use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use futures::StreamExt;
use http_body_util::BodyExt;
use tower::ServiceExt;

use codex_api::{
    ApiError, AuthProvider, Provider, ResponseEvent, ResponsesApiRequest, RetryConfig,
};
use codex_client::{HttpTransport, Request, Response, StreamResponse, TransportError};
use codex_protocol::ResponseItemId;
use codex_protocol::models::{ContentItem, FunctionCallOutputPayload, ResponseItem};

/// Ceiling on a whole exchange.
///
/// Two suites arrived at the same number for two reasons, both of which still
/// apply. Against a live router it is generous and should never be reached: a
/// stall is a transport bug and must fail with a message rather than hang the
/// run. Against a canned body it can only be reached by a bug in the fixture
/// itself, since a byte string never stalls — but a fixture bug that hangs the
/// suite is worse than one that fails fast.
pub const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// An [`AuthProvider`] that sends no credential at all.
///
/// For surfaces that authenticate nothing — an Open-mode deployment, or a
/// canned body where the fact under test is about parsing rather than auth.
#[derive(Clone, Default)]
pub struct NoAuth;

impl AuthProvider for NoAuth {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) {}
}

/// An [`AuthProvider`] that always sends the same bearer token.
///
/// This is the shape a stock Codex install authenticates with — a static
/// secret read from the environment (`model_providers.*.env_key`) and sent as
/// `Authorization: Bearer …` on every request. Driving a Configured-mode
/// surface through it is what proves a roundhouse turn key needs no client
/// modification to be usable.
pub struct StaticToken {
    token: String,
}

impl StaticToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

impl AuthProvider for StaticToken {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token))
                .expect("a token supplied by test code is a valid header value"),
        );
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Codex's [`HttpTransport`], backed by an axum [`Router`] rather than by a
/// socket.
///
/// Everything the client does above the socket — building the request,
/// encoding the body, setting `Accept`, spawning the SSE reader — is
/// unchanged; only the dispatch is a `tower` call. That includes the headers
/// an [`AuthProvider`] added, which is what lets a gated surface be tested
/// without binding a port.
#[derive(Clone)]
pub struct RouterTransport {
    pub app: Router,
}

impl RouterTransport {
    async fn dispatch(&self, request: Request) -> Result<axum::response::Response, TransportError> {
        let prepared = request
            .prepare_body_for_send()
            .map_err(TransportError::Build)?;

        let mut builder = axum::http::Request::builder()
            .method(request.method.clone())
            .uri(&request.url);
        for (name, value) in prepared.headers.iter() {
            builder = builder.header(name, value);
        }
        let request = builder
            .body(Body::from(prepared.body_bytes()))
            .map_err(|error| TransportError::Build(error.to_string()))?;

        self.app
            .clone()
            .oneshot(request)
            .await
            .map_err(|error| TransportError::Network(error.to_string()))
    }
}

// The `Err` type is `codex_client`'s and the trait fixes the signature, so
// there is nothing here to box: silencing it once at the impl beats the same
// warning multiplied across every test binary that links this file.
#[allow(clippy::result_large_err)]
impl HttpTransport for RouterTransport {
    async fn execute(&self, request: Request) -> Result<Response, TransportError> {
        let url = request.url.clone();
        let response = self.dispatch(request).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|error| TransportError::Network(error.to_string()))?
            .to_bytes();
        if !status.is_success() {
            return Err(http_error(status, url, headers, body));
        }
        Ok(Response {
            status,
            headers,
            body,
        })
    }

    async fn stream(&self, request: Request) -> Result<StreamResponse, TransportError> {
        let url = request.url.clone();
        let response = self.dispatch(request).await?;
        let status = response.status();
        let headers = response.headers().clone();
        if !status.is_success() {
            let body = response
                .into_body()
                .collect()
                .await
                .map_err(|error| TransportError::Network(error.to_string()))?
                .to_bytes();
            return Err(http_error(status, url, headers, body));
        }
        // Left as a stream rather than collected: a response that has not ended
        // is the only kind this endpoint produces, and buffering it here would
        // hide any ordering the client depends on.
        let bytes = response
            .into_body()
            .into_data_stream()
            .map(|chunk| chunk.map_err(|error| TransportError::Network(error.to_string())));
        Ok(StreamResponse {
            status,
            headers,
            bytes: Box::pin(bytes),
        })
    }
}

/// A non-2xx answer, in the shape the client reports to its caller.
///
/// The body is carried through rather than dropped: a refusal this surface
/// makes before streaming says *why* in its JSON body, and a test that could
/// only see the status could not tell `unknown_key` from `wrong_key_kind`.
pub fn http_error(
    status: StatusCode,
    url: String,
    headers: HeaderMap,
    body: Bytes,
) -> TransportError {
    TransportError::Http {
        status,
        url: Some(url),
        headers: Some(headers),
        body: String::from_utf8(body.to_vec()).ok(),
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A provider that retries nothing.
///
/// Retries would mask exactly what these suites are for: a request answered
/// twice because the first answer was unreadable must fail here, not succeed
/// slowly — and against a canned body a retry would replay the identical bytes
/// and double every event a collector counts.
///
/// `name` is the provider label the client reports failures under, and it is a
/// parameter rather than a constant only so each suite's errors name the suite.
pub fn provider(base_url: &str, name: &str) -> Provider {
    Provider {
        name: name.to_string(),
        base_url: base_url.to_string(),
        query_params: None,
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_attempts: 0,
            base_delay: Duration::from_millis(1),
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: EXCHANGE_TIMEOUT,
    }
}

/// A request in the shape Codex sends, built from Codex's own type.
///
/// Serializing the client's struct rather than hand-writing JSON is what makes
/// this a conformance test: a field it adds or renames arrives here without
/// anyone having transcribed it.
pub fn request(cache_key: &str, input: Vec<ResponseItem>) -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "roundhouse".to_string(),
        instructions: "be brief".to_string(),
        input,
        tools: None,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: Some(cache_key.to_string()),
        text: None,
        client_metadata: None,
    }
}

pub fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

/// An assistant message as a client re-sends one, built from Codex's own type.
///
/// Shared here rather than redefined per suite because M10.0 gave three of them
/// the same need: the steer is an assistant message now, so replaying "the agent
/// carried on after being corrected" means appending one of these to the resent
/// history — and the bytes have to be the ones a real client would send, or the
/// prefix check is being tested against our own reconstruction.
///
/// `OutputText`, not `InputText`: an assistant item the client echoes back
/// carries the output part, and canonicalization reads the role from the item
/// rather than from the part — but a part that disagreed with the role is
/// exactly the drift an oracle fixture exists to prevent.
pub fn assistant_message(text: &str) -> ResponseItem {
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

/// A `function_call` item built from Codex's own type, never hand-written
/// JSON — the same rationale as [`request`]: a field this struct adds or
/// renames arrives here without anyone having transcribed it, which is the
/// whole point of an oracle test.
pub fn function_call_item(
    name: &str,
    namespace: Option<&str>,
    call_id: &str,
    arguments: &str,
) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: name.to_string(),
        namespace: namespace.map(str::to_string),
        arguments: arguments.to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

/// A `function_call_output` item carrying a plain-string result — the form
/// `FunctionCallOutputPayload::from_text` produces, as opposed to the
/// structured `content_items` array the same field can also hold.
pub fn function_call_output_item(call_id: &str, output_text: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_text(output_text.to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

/// The model's own scratch space, which this surface drops on the way in.
///
/// Built from Codex's own type for the reason every builder here is: the
/// suites that send one are asserting that a *real* agent's reasoning item
/// does not disturb a prefix, and a hand-written `{"type":"reasoning"}` would
/// only prove that our own idea of one does not.
pub fn reasoning_item(id: &str) -> ResponseItem {
    ResponseItem::Reasoning {
        id: Some(ResponseItemId::new(id)),
        summary: Vec::new(),
        content: None,
        encrypted_content: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

// ---------------------------------------------------------------------------
// Reading the bytes, for the assertions a parser cannot make
// ---------------------------------------------------------------------------

/// One SSE frame as it left the server: its `event:` name and its parsed
/// `data:` payload.
///
/// Codex's parser is forgiving by design — it ignores what it does not
/// recognize — so it cannot prove that *nothing extra* went out, or that the
/// terminal frame was last. That is what reading the bytes is for, and it is
/// why the two suites that make an exhaustive frame-list assertion share one
/// reader rather than each keeping its own idea of how an SSE body splits.
#[derive(Debug)]
pub struct Frame {
    pub name: String,
    pub payload: serde_json::Value,
}

impl Frame {
    /// The payload's own `type` tag, which is what a client reads.
    pub fn kind(&self) -> &str {
        self.payload["type"]
            .as_str()
            .expect("every frame this surface emits is typed")
    }
}

/// Every frame in a finished SSE body, in order.
///
/// Collected whole rather than read incrementally because the endpoints these
/// suites drive always end their own stream; a body that did not would be
/// caught by [`EXCHANGE_TIMEOUT`] rather than hanging the run.
pub async fn frames(body: Body) -> Vec<Frame> {
    let bytes = tokio::time::timeout(EXCHANGE_TIMEOUT, body.collect())
        .await
        .expect("the SSE stream stalled")
        .expect("body")
        .to_bytes();
    let text = std::str::from_utf8(&bytes).expect("SSE bodies are UTF-8");

    text.split("\n\n")
        .filter(|raw| !raw.trim().is_empty())
        .filter_map(|raw| {
            let mut name = None;
            let mut data = None;
            for line in raw.lines() {
                // A line starting with `:` is a keep-alive comment.
                let Some((field, value)) =
                    line.split_once(':').filter(|(field, _)| !field.is_empty())
                else {
                    continue;
                };
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "event" => name = Some(value.to_string()),
                    "data" => data = Some(value.to_string()),
                    _ => {}
                }
            }
            Some(Frame {
                name: name?,
                payload: serde_json::from_str(&data?).expect("frame data must be JSON"),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Reading what the client read
// ---------------------------------------------------------------------------

/// Everything the client parsed, minus what it invented.
///
/// `RateLimits` is synthesized from response headers on every response,
/// present or not, so it reports nothing about what an endpoint emitted.
/// Filtering it keeps these assertions about the wire; that nothing *else*
/// went out is what a frame-level test proves.
pub async fn collect(
    mut stream: codex_api::ResponseStream,
) -> Result<Vec<ResponseEvent>, ApiError> {
    tokio::time::timeout(EXCHANGE_TIMEOUT, async move {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            match event? {
                ResponseEvent::RateLimits(_) => {}
                event => events.push(event),
            }
        }
        Ok(events)
    })
    .await
    .expect("the response stream stalled")
}
