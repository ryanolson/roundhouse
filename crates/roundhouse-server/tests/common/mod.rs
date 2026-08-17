// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fixtures shared by the integration-test binaries.
//!
//! One canonical copy: a change to the catalog shape or the worker registration
//! touches this file, not one file per test binary. Each binary compiles its
//! own copy via `mod common;`, and none uses every item, so the module opts out
//! of dead-code analysis rather than sprinkling `allow`s per item.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};

use codex_api::AuthProvider;
use codex_protocol::models::{FunctionCallOutputPayload, ResponseItem};

use roundhouse_core::routing::{CacheModel, ProviderPricing};
use roundhouse_fleet::{
    EmbeddedFleet, FrontierChunk, FrontierClient, FrontierError, FrontierModelSpec, FrontierQuote,
    FrontierStream, KvRouterConfig, SelectionServiceBuilder, StaticFrontierCatalog, WireProtocol,
    WorkerRegistration,
};
use roundhouse_server::EngineConfig;

pub const BLOCK_SIZE: u32 = 16;
pub const LOCAL_MODEL: &str = "local";
pub const MINUTE: u64 = 60_000;

/// One priced frontier model, so a turn always has somewhere to go.
pub fn frontier_catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        provider: "anthropic".into(),
        model: "claude".into(),
        wire_protocol: WireProtocol::AnthropicMessages,
        cache_model: CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
        pricing: ProviderPricing {
            input_per_mtok_usd: 3.0,
            cached_input_per_mtok_usd: 0.3,
            cache_write_per_mtok_usd: 3.75,
            output_per_mtok_usd: 15.0,
        },
        quality_prior: 0.95,
        base_ttft_ms: 350.0,
        ttft_ms_per_uncached_token: 0.002,
    }])
}

pub fn config() -> EngineConfig {
    EngineConfig {
        block_size: BLOCK_SIZE,
        local_model: LOCAL_MODEL.to_string(),
        ..Default::default()
    }
}

/// A selection service with one registered worker, so local is a real option.
///
/// KV events stay off: these binaries test the session, routing, and transport
/// layers, and a cold indexer keeps their pricing deterministic. The one test
/// that needs a warm worker builds its own fleet (`mocker_cache_hits.rs`).
pub async fn embedded_fleet() -> Arc<EmbeddedFleet> {
    let service = SelectionServiceBuilder::new(KvRouterConfig {
        use_kv_events: false,
        router_queue_threshold: None,
        ..Default::default()
    })
    .indexer_threads(1)
    .build()
    .await
    .expect("selection service should start");
    let fleet = Arc::new(EmbeddedFleet::new(Arc::new(service)));
    fleet
        .register_worker(WorkerRegistration {
            worker_id: 1,
            model_name: LOCAL_MODEL.to_string(),
            routing_group: "default".to_string(),
            endpoint: "http://worker-1:8000".to_string(),
            block_size: BLOCK_SIZE,
            kv_events_endpoints: HashMap::new(),
        })
        .await
        .expect("the worker must register");
    fleet
}

// ---------------------------------------------------------------------------
// Steering harness (unused before M1+ — see each item's doc comment)
// ---------------------------------------------------------------------------

/// The default validate-call reply: a canned verdict that never triggers a
/// steer, so a milestone that has not built the trigger yet still gets a
/// well-formed JSON body if it ever exercises this path.
pub const DEFAULT_VERDICT_JSON: &str = r#"{"on_track":true,"reason":"scripted default"}"#;

/// A [`FrontierClient`] whose reply is chosen by the cache key, not by a real
/// model.
///
/// Milestone M6 needs a judge that answers deterministically and a way to
/// prove the side-call was priced and shaped correctly without a network; this
/// is that double, built now so M1–M5 do not have to touch this file again to
/// get it. It is not referenced by any M0 test — `#![allow(dead_code)]` above
/// is what keeps that from being a build warning — and its branching rule
/// (`prompt_cache_key` ending in `#validate` gets the verdict body, everything
/// else gets the plain reply) is the one the validate/steer design in
/// `PLAN-agentic-control-plane.md` §6 assumes a judge call is tagged with.
pub struct ScriptedFrontierClient {
    plain_reply: String,
    verdict_reply: String,
    /// Every quote this client was asked to execute, in call order.
    ///
    /// A later milestone's tests assert on what the engine *sent* — the model,
    /// the cache key, the prompt — not just on what came back, and a stream
    /// already consumed cannot be replayed to check that. Recording the quote
    /// at the point of the call is the only way to keep both usable.
    quotes_seen: Arc<Mutex<Vec<FrontierQuote>>>,
}

impl ScriptedFrontierClient {
    /// A client that answers ordinary calls with `plain_reply` and validate
    /// calls with [`DEFAULT_VERDICT_JSON`].
    pub fn new(plain_reply: impl Into<String>) -> Self {
        Self::with_verdict(plain_reply, DEFAULT_VERDICT_JSON)
    }

    /// As [`Self::new`], but with a caller-chosen verdict body — for a test
    /// that needs a specific `on_track` value or a malformed body to prove the
    /// engine's parser rejects it rather than trusting the judge blindly.
    pub fn with_verdict(plain_reply: impl Into<String>, verdict_reply: impl Into<String>) -> Self {
        Self {
            plain_reply: plain_reply.into(),
            verdict_reply: verdict_reply.into(),
            quotes_seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every quote seen so far, in call order.
    pub fn quotes_seen(&self) -> Vec<FrontierQuote> {
        self.quotes_seen
            .lock()
            .expect("the recording mutex is never held across a panic in this harness")
            .clone()
    }
}

#[async_trait]
impl FrontierClient for ScriptedFrontierClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.quotes_seen
            .lock()
            .expect("the recording mutex is never held across a panic in this harness")
            .push(quote.clone());
        // Mirrors `EchoFrontierClient::execute`'s stream construction: a whole
        // response presented as a two-chunk stream is the same durable-delta
        // shape a real streaming provider would produce, just front-loaded.
        let reply = if quote.prompt_cache_key.ends_with("#validate") {
            self.verdict_reply.clone()
        } else {
            self.plain_reply.clone()
        };
        Ok(FrontierChunk::whole_response(
            reply.clone(),
            quote.prompt.len() as u64,
            0,
            reply.len() as u64,
            0,
        ))
    }
}

/// A [`codex_api::AuthProvider`] that always sends the same bearer token.
///
/// Unused before M1, which is the first milestone that drives a real
/// `codex_api::ResponsesClient` against an endpoint that checks auth at all —
/// M0's oracle tests authenticate with `NoAuth` because the wire-shape facts
/// they pin do not depend on it. Built here rather than in M1 so the harness
/// convention (auth doubles live in `common`, next to the transport doubles)
/// is set once.
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

/// A `function_call` item built from Codex's own type, never hand-written
/// JSON — the same rationale as `codex_conformance.rs`'s `request()`: a field
/// this struct adds or renames arrives here without anyone having transcribed
/// it, which is the whole point of an oracle test.
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
