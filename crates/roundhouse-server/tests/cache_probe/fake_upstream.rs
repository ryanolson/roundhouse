// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The loopback Messages upstream the offline claims dispatch against.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::Value;

use roundhouse_server::catalog_config::ProviderConfig;

/// A Messages upstream that answers with the cache counters a test chose, and
/// records what arrived.
#[derive(Clone)]
pub(super) struct Upstream {
    bodies: Arc<Mutex<Vec<Value>>>,
    seen: Arc<Mutex<Vec<HeaderMap>>>,
    calls: Arc<AtomicUsize>,
    /// `(cache_read, cache_creation)` per request, in order.
    usage: Arc<Mutex<VecDeque<(u64, u64)>>>,
}

impl Upstream {
    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub(super) fn body(&self, index: usize) -> Value {
        self.bodies.lock().expect("no panic holds this")[index].clone()
    }

    /// Every header that arrived, as `name: value` lines.
    ///
    /// A rendered string because the assertions are negative as often as
    /// positive — *this* must appear nowhere — and a scan over the whole thing
    /// is the only way to say that without enumerating where a leak could hide.
    pub(super) fn headers(&self) -> String {
        self.seen
            .lock()
            .expect("no panic holds this")
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

/// One complete stream, carrying the counters this turn is meant to report.
fn sse(read: u64, written: u64) -> String {
    format!(
        concat!(
            "event: message_start\n",
            r#"data: {{"type":"message_start","message":{{"type":"message","id":"msg_probe","#,
            r#""role":"assistant","model":"claude-probe","content":[],"stop_reason":null,"#,
            r#""stop_sequence":null,"usage":{{"input_tokens":{input},"#,
            r#""cache_read_input_tokens":{read},"cache_creation_input_tokens":{written},"#,
            r#""output_tokens":1}}}}}}"#,
            "\n\n",
            "event: content_block_start\n",
            r#"data: {{"type":"content_block_start","index":0,"content_block":{{"type":"text","text":""}}}}"#,
            "\n\n",
            "event: content_block_delta\n",
            r#"data: {{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"ok"}}}}"#,
            "\n\n",
            "event: content_block_stop\n",
            r#"data: {{"type":"content_block_stop","index":0}}"#,
            "\n\n",
            "event: message_delta\n",
            r#"data: {{"type":"message_delta","delta":{{"stop_reason":"end_turn","stop_sequence":null}},"#,
            r#""usage":{{"output_tokens":4}}}}"#,
            "\n\n",
            "event: message_stop\n",
            r#"data: {{"type":"message_stop"}}"#,
            "\n\n"
        ),
        input = 100,
        read = read,
        written = written,
    )
}

async fn handle(State(state): State<Upstream>, headers: HeaderMap, body: String) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    state
        .seen
        .lock()
        .expect("no panic holds this")
        .push(headers);
    state
        .bodies
        .lock()
        .expect("no panic holds this")
        .push(serde_json::from_str(&body).expect("the client sends json"));
    let (read, written) = state
        .usage
        .lock()
        .expect("no panic holds this")
        .pop_front()
        .unwrap_or((0, 0));
    ([("content-type", "text/event-stream")], sse(read, written)).into_response()
}

/// Bind the upstream at the client's default path.
pub(super) async fn spawn(usage: Vec<(u64, u64)>) -> (String, Upstream) {
    spawn_at("", "/v1/messages", usage).await
}

/// Bind the upstream at `{base_suffix}{route}`, and return the base URL
/// including `base_suffix`.
///
/// Two parts because that is how a gateway is configured: the base carries a
/// version segment and the route is relative to it, so a client that ignored
/// the configured route would post to a path this mount does not answer.
pub(super) async fn spawn_at(
    base_suffix: &str,
    route: &str,
    usage: Vec<(u64, u64)>,
) -> (String, Upstream) {
    let state = Upstream {
        bodies: Arc::new(Mutex::new(Vec::new())),
        seen: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(AtomicUsize::new(0)),
        usage: Arc::new(Mutex::new(usage.into())),
    };
    let app = Router::new()
        .route(&format!("{base_suffix}{route}"), post(handle))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is free");
    let addr: SocketAddr = listener.local_addr().expect("the listener is bound");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}{base_suffix}"), state)
}

/// A provider definition at the client's default path, with a stored key.
pub(super) fn loopback_provider(base_url: &str) -> ProviderConfig {
    serde_json::from_value(serde_json::json!({
        "base_url": base_url,
        "routes": { "messages": "/v1/messages" },
        "auth": { "env": "ROUNDHOUSE_PROBE_FIXTURE_KEY" },
    }))
    .expect("the fixture definition parses")
}

/// A gateway: a versioned base, a relative route, a bearer, static headers.
///
/// The shape `examples/catalog.example.json` demonstrates for OpenRouter's
/// Messages route, which is the one that refuses an `x-api-key`.
pub(super) fn gateway_provider(base_url: &str) -> ProviderConfig {
    serde_json::from_value(serde_json::json!({
        "base_url": base_url,
        "routes": { "messages": "/messages" },
        "auth": { "env": "ROUNDHOUSE_PROBE_FIXTURE_KEY", "style": "bearer" },
        "extra_headers": { "X-Probe-Gateway": "roundhouse-cache-probe" },
    }))
    .expect("the fixture definition parses")
}

/// The text of every content block in a recorded request.
pub(super) fn blocks(body: &Value) -> Vec<String> {
    body["messages"][0]["content"]
        .as_array()
        .expect("the request carries content blocks")
        .iter()
        .map(|block| block["text"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Every block index carrying a `cache_control` marker.
pub(super) fn markers(body: &Value) -> Vec<usize> {
    body["messages"][0]["content"]
        .as_array()
        .expect("the request carries content blocks")
        .iter()
        .enumerate()
        .filter(|(_, block)| block.get("cache_control").is_some())
        .map(|(index, _)| index)
        .collect()
}
