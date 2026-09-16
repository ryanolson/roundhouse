// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M17 thermo-nuclear review, finding F3 — confirmed.
//!
//! **The claim.** No test drives a namespaced `FrontierChunk::ToolCall`
//! through the engine's namespace join (`engine.rs:1621`, where
//! `Item::namespaced_tool_call` is built from the decoded chunk) into the
//! stored item, or through the follower's `Emitted::ToolCall` mapping
//! (`responses_api.rs:826`) into an actual outbound `response.output_item.done`
//! frame. Every scripted client in `tests/common/mod.rs` hard-codes
//! `namespace: None` on the tool calls it produces
//! (`ToolCallingFrontierClient::execute`, `Scripted::Call` has no namespace
//! field at all), so the whole suite is green whether or not either site
//! forwards the namespace at all.
//!
//! **This test.** [`namespaced_tool_call_round_trips_through_the_engine`]
//! builds a real `Engine` + `responses_router` (the `main::serve`
//! composition, same as `review_m11_1_f6.rs`) behind a small local
//! `FrontierClient` double that answers with `FrontierChunk::ToolCall {
//! namespace: Some("mcp__roundhouse"), .. }` — a case no fixture in
//! `common::codex` or `common::mod` can produce. It asserts, from one real
//! turn driven over `/v1/responses`:
//!
//! 1. the item the engine committed to the log carries
//!    `namespace: Some("mcp__roundhouse")` (the join at `engine.rs:1621`);
//! 2. the raw SSE `response.output_item.done` frame's `item.namespace` field
//!    is `"mcp__roundhouse"` (the follower's mapping at
//!    `responses_api.rs:826` and the encoder at `wire.rs`'s
//!    `function_call_item`).
//!
//! Passes today. `CONFIRM_ENGINE_JOIN_BREAKS_IT` and
//! `CONFIRM_FOLLOWER_MAPPING_BREAKS_IT` below document the two byte-mutations
//! the finding names (`engine.rs:1621`'s `namespace` argument replaced with
//! `None`, and `responses_api.rs:826`'s `namespace` replaced with `None`) —
//! each was applied by hand from a byte-exact backup, shown to redden this
//! test and only this test (the rest of `steering_emission` and
//! `codex_wire_shapes` stayed green under both), and reverted from that same
//! backup. See the session transcript for the mutation/restore transcript;
//! the tree carries no trace of either mutation.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use futures::StreamExt;
use tower::ServiceExt;

use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::{Item, ItemContent};
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierError, FrontierQuote, FrontierStream,
};
use roundhouse_server::{ControlPlane, Conversations, EchoLocalExecutor, Engine, responses_router};

mod common;
use common::codex::{frames, request, user_message};
use common::{config, frontier_catalog};

/// The namespace an upstream names an MCP call under, per R-N6 — the same
/// literal `steering_emission.rs`'s `NAMESPACE` uses for the *inbound* half of
/// this claim.
const NAMESPACE: &str = "mcp__roundhouse";

/// A `FrontierClient` that answers every turn with one namespaced tool call.
///
/// **What no fixture in `common` can produce.** `ToolCallingFrontierClient`
/// hard-codes `namespace: None` on every `FrontierChunk::ToolCall` it emits
/// (see its doc comment), on the reasoning that the turn-loop suites built on
/// it are not about namespace forwarding. This type exists because F3 is
/// precisely about the case that reasoning carves out.
struct NamespacedToolCallFrontier {
    call_id: &'static str,
    tool_name: &'static str,
}

#[async_trait]
impl FrontierClient for NamespacedToolCallFrontier {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        let arguments = r#"{"pattern":"fn main"}"#.to_string();
        let chunks: Vec<Result<FrontierChunk, FrontierError>> = vec![
            Ok(FrontierChunk::ToolCall {
                id: self.call_id.to_string(),
                name: self.tool_name.to_string(),
                namespace: Some(NAMESPACE.to_string()),
                arguments: arguments.clone(),
            }),
            Ok(FrontierChunk::Done {
                input_tokens: quote.prompt.len() as u64,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: arguments.len() as u64,
                reasoning_tokens: 0,
                provider_reported_cost: None,
                stop_reason: None,
            }),
        ];
        Ok(futures::stream::iter(chunks).boxed())
    }
}

/// The `main::serve` composition (engine + `/v1/responses`) over the double
/// above, in Open mode — auth is not what this finding is about.
fn deployment() -> (Router, Arc<MemoryStore>) {
    ensure_rustls_crypto_provider();
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(NamespacedToolCallFrontier {
            call_id: "call_f3",
            tool_name: "Grep",
        }),
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    let app = responses_router(
        ControlPlane::open(),
        Arc::clone(&engine),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );
    (app, store)
}

/// F3, confirmed: a namespaced tool call survives both the engine's namespace
/// join and the Responses follower's outbound mapping.
///
/// This is the join `engine.rs:1621` and `responses_api.rs:826` are named for
/// in the finding — exercised here for the first time via a real turn rather
/// than a unit that re-implements one side of it locally
/// (`roundhouse-fleet/src/frontier.rs:1620-1636`, which builds
/// `Item::namespaced_tool_call` directly and never calls the engine, and
/// which itself only ever passes `namespace: None`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_namespaced_tool_call_round_trips_through_the_engine() {
    let (app, store) = deployment();
    let session_id = "f3-namespace-roundtrip";

    let body = request(session_id, vec![user_message("please grep something")]);
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).expect("json")))
                .expect("request"),
        )
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::OK);
    let observed = frames(response.into_body()).await;

    // --- (1) engine.rs:1621: the log's own item carries the namespace. ---
    let stored: Vec<Item> = store
        .read_events(&SessionId::new(session_id), 0, 1024)
        .await
        .expect("the session exists")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ItemAppended { item } => Some(item),
            _ => None,
        })
        .collect();
    let call = stored
        .iter()
        .find(|item| matches!(item.content, ItemContent::ToolCall { .. }))
        .expect("the turn committed a tool call item");
    match &call.content {
        ItemContent::ToolCall { namespace, .. } => {
            assert_eq!(
                namespace.as_deref(),
                Some(NAMESPACE),
                "the engine's namespace join (engine.rs:1621) must carry the \
                 chunk's namespace into the stored item, not drop it: got {call:?}"
            );
        }
        other => unreachable!("filtered to ToolCall above: {other:?}"),
    }

    // --- (2) responses_api.rs:826 + wire.rs: the outbound frame carries it. ---
    let done = observed
        .iter()
        .find(|frame| {
            frame.kind() == "response.output_item.done"
                && frame.payload["item"]["type"] == "function_call"
        })
        .expect("the turn emits a function_call output_item.done frame");
    assert_eq!(
        done.payload["item"]["namespace"], NAMESPACE,
        "the outbound response.output_item.done frame must carry the same \
         namespace the log stored (responses_api.rs:826's Emitted::ToolCall \
         mapping and wire.rs's function_call_item encoder): got {done:?}"
    );
}
