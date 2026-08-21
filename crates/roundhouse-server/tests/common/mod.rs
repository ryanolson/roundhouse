// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fixtures shared by the integration-test binaries.
//!
//! One canonical copy: a change to the catalog shape or the worker registration
//! touches this file, not one file per test binary. Each binary compiles its
//! own copy via `mod common;`, and none uses every item, so the module opts out
//! of dead-code analysis rather than sprinkling `allow`s per item.
//!
//! Split in two once a third suite needed the client side. **This file holds
//! what stands behind the server** — catalog, fleet, frontier client, engine
//! config — and [`codex`] holds what stands in front of it: the transport,
//! auth, request and collector doubles that make a test *be* a Codex client.
//! The two halves have no reason to change together, and the `use` line at the
//! top of a suite now says which side of the wire it is exercising.
//!
//! [`validate`] is the third of those axes rather than a fourth pile in this
//! file: it holds the judge and signal doubles that turn the validate/steer
//! loop on, which neither stands behind the server nor in front of it — it
//! occupies a seam inside it.
#![allow(dead_code)]

pub mod codex;
pub mod validate;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use roundhouse_core::routing::{CacheModel, ProviderPricing};
use roundhouse_fleet::{
    EmbeddedFleet, FrontierChunk, FrontierClient, FrontierError, FrontierModelSpec, FrontierQuote,
    FrontierStream, KvRouterConfig, SelectionServiceBuilder, StaticFrontierCatalog, WireProtocol,
    WorkerRegistration,
};
use roundhouse_server::{ControlPlaneConfig, EngineConfig};
use sha2::{Digest, Sha256};

pub const BLOCK_SIZE: u32 = 16;
pub const LOCAL_MODEL: &str = "local";
pub const MINUTE: u64 = 60_000;

// ---------------------------------------------------------------------------
// Keys, and the config that declares them
// ---------------------------------------------------------------------------

/// A well-shaped turn secret with `tag` legible inside it.
///
/// Padded to the 43 base62 characters
/// [`has_valid_key_shape`](roundhouse_server::has_valid_key_shape) requires,
/// because a hand-counted literal fails as `malformed_key` for a reason no
/// assertion names — and the tag is inside the secret so a failure message says
/// *which* key was refused rather than showing forty-three identical letters.
///
/// Here rather than once per suite: six binaries had their own copy of this and
/// of the two functions below, which meant the fixture rule and the deployment's
/// real key format were six independent restatements of one fact.
pub fn key(tag: &str) -> String {
    format!("rh_turn_{tag:A<43}")
}

/// The same, wearing the admin prefix.
pub fn admin_key(tag: &str) -> String {
    format!("rh_admin_{tag:A<43}")
}

/// What a control-plane file carries in place of a secret.
///
/// Computed from the secret rather than transcribed, so a fixture cannot drift
/// into declaring a hash nothing hashes to — which parses perfectly and
/// authenticates nothing.
pub fn sha256_hex(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// A validated config from the JSON an operator would have written.
///
/// Through [`ControlPlaneConfig::from_json`] rather than by assembling the
/// lookup tables, because the file *is* the format: the pairing of a project's
/// budget with a key's allocation, the narrowing of a policy by an override and
/// the credential tiers all happen inside `validate`, and a fixture that
/// bypassed it would be testing terms no deployment could be given.
pub fn control_plane(json: serde_json::Value, label: &str) -> ControlPlaneConfig {
    ControlPlaneConfig::from_json(&json.to_string(), label)
        .unwrap_or_else(|error| panic!("the {label} config must validate: {error}"))
}

/// A namespaced session id as one path segment.
///
/// A Configured deployment's ids carry `/`, and a route parameter matches a
/// single segment, so the separators have to be escaped or the request routes
/// nowhere. Spelled out here rather than worked around silently because a
/// `404` from a mistyped path and a `404` from an unescaped id read
/// identically — and shared rather than written per suite because a
/// hand-written `%2F` literal in a URL is the same fact stated a second time,
/// where the next reader cannot tell an escape from a typo.
pub fn path_segment(session_id: &str) -> String {
    session_id.replace('/', "%2F")
}

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
