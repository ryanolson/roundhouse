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
//!
//! [`anthropic`] is the fourth, and it is not a double at all: it is the tier-1
//! conformance oracle — a strict Messages reader written from the pinned spec,
//! deliberately the opposite polarity of the shipped types. It stands in front
//! of the server like [`codex`] does, but it is a *judge* rather than a client,
//! and the distinction is why it has its own file: nothing in it may be relaxed
//! to make a test pass.
//!
//! [`e2e`] is the fifth, and it is neither a double nor a judge: it is the rig
//! the two **real-binary** suites stand a real client inside — recorder,
//! bootstrap, fork probe, version probe. It exists because the second such suite
//! was written by copying the first, and the copies drifted inside one milestone
//! (M11.2b review F1).
#![allow(dead_code)]

pub mod anthropic;
pub mod codex;
pub mod e2e;
pub mod validate;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;

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

// ---------------------------------------------------------------------------
// A frontier that calls tools (M11.2)
// ---------------------------------------------------------------------------

/// One thing a scripted upstream produces, in the order it produces it.
///
/// Text and calls both, because the whole of M11.2's content model is that a
/// turn interleaves them: a double that could only answer one or the other
/// would let a serve projection pass every test while emitting the two in an
/// order no client can read.
#[derive(Debug, Clone)]
pub enum Scripted {
    Text(&'static str),
    Call {
        id: &'static str,
        name: &'static str,
        /// Owned, unlike the two beside it.
        ///
        /// M11.2b review F13: a real-client suite scripts a call against a path
        /// its own rig only learns at run time, and a `&'static str` field made
        /// that a `Box::leak` at every such call site — two of them in
        /// `claude_e2e.rs`, each with a paragraph explaining why leaking was
        /// acceptable. `id` and `name` stay borrowed because every caller writes
        /// a literal for them; widening only the field that actually varies is
        /// what keeps the common case free of `.to_string()` noise.
        arguments: String,
    },
}

/// A [`FrontierClient`] that streams a fixed script, tool calls included.
///
/// **The double the tool loop needs and [`ScriptedFrontierClient`] cannot be.**
/// That one answers with one string, front-loaded as a whole response — which is
/// exactly right for a turn whose only content is prose, and cannot express the
/// thing under test here: a `FrontierChunk::ToolCall` arriving *between* two
/// text deltas. The script is a list rather than a builder because the order is
/// the assertion.
///
/// The `Done` carries a caller-chosen `stop_reason`, so a test can drive the
/// cross-dialect case — a wire that answers with calls and names no reason at
/// all — as easily as the Anthropic one.
pub struct ToolCallingFrontierClient {
    script: Vec<Scripted>,
    stop_reason: Option<String>,
    quotes_seen: Arc<Mutex<Vec<FrontierQuote>>>,
}

impl ToolCallingFrontierClient {
    pub fn new(script: Vec<Scripted>, stop_reason: Option<&str>) -> Self {
        Self {
            script,
            stop_reason: stop_reason.map(str::to_string),
            quotes_seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn quotes_seen(&self) -> Vec<FrontierQuote> {
        self.quotes_seen
            .lock()
            .expect("the recording mutex is never held across a panic in this harness")
            .clone()
    }
}

#[async_trait]
impl FrontierClient for ToolCallingFrontierClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.quotes_seen
            .lock()
            .expect("the recording mutex is never held across a panic in this harness")
            .push(quote.clone());
        let mut chunks: Vec<Result<FrontierChunk, FrontierError>> = Vec::new();
        let mut output_tokens = 0u64;
        for step in &self.script {
            match step {
                Scripted::Text(text) => {
                    output_tokens += text.len() as u64;
                    chunks.push(Ok(FrontierChunk::OutputText((*text).to_string())));
                }
                Scripted::Call {
                    id,
                    name,
                    arguments,
                } => {
                    output_tokens += arguments.len() as u64;
                    chunks.push(Ok(FrontierChunk::ToolCall {
                        id: (*id).to_string(),
                        name: (*name).to_string(),
                        arguments: arguments.clone(),
                    }));
                }
            }
        }
        chunks.push(Ok(FrontierChunk::Done {
            input_tokens: quote.prompt.len() as u64,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens,
            reasoning_tokens: 0,
            provider_reported_cost: None,
            stop_reason: self.stop_reason.clone(),
        }));
        Ok(futures::stream::iter(chunks).boxed())
    }
}

// ---------------------------------------------------------------------------
// A frontier that answers a queue of turns and then terminates (M11.2b)
// ---------------------------------------------------------------------------

/// A [`FrontierClient`] that answers a queue of scripted turns and then answers
/// one fixed turn forever.
///
/// **The tail is the whole reason this type exists** rather than
/// [`ToolCallingFrontierClient`] being used directly by a real-client suite. A
/// real agent answers a `tool_use` by running the tool and dispatching again, so
/// an upstream whose fixed script *is* a call answers the resend with the same
/// call and the loop never closes: the run ends at whatever deadline the harness
/// set and reads as a hung client. Queue-then-tail makes termination a property
/// of the double instead of a hope about the client.
///
/// Here rather than in one suite (M11.2b review F13): every real-client tool-loop
/// suite needs exactly this double, and the one that had it kept it private, so
/// the next one would have written it again — which is how the harness copy this
/// module exists to undo started.
pub struct ScriptedTurns {
    queued: Mutex<VecDeque<ToolCallingFrontierClient>>,
    then: ToolCallingFrontierClient,
    /// Every quote this deployment dispatched, in call order — what roundhouse
    /// actually sent upstream, as opposed to what the client sent us.
    quotes: Mutex<Vec<FrontierQuote>>,
}

impl ScriptedTurns {
    /// One prose turn, on every dispatch.
    pub fn answering(text: &'static str) -> Self {
        Self::then_answering(Vec::new(), text)
    }

    /// The queued turns in order, then prose forever.
    pub fn then_answering(queued: Vec<ToolCallingFrontierClient>, text: &'static str) -> Self {
        Self {
            queued: Mutex::new(VecDeque::from(queued)),
            then: prose_turn(text),
            quotes: Mutex::new(Vec::new()),
        }
    }

    pub fn dispatches(&self) -> usize {
        self.quotes.lock().expect("recording").len()
    }

    /// Whether any dispatch carried a *forwarded* caller credential upstream.
    ///
    /// Read from the quote rather than from the wire because that is where the
    /// decision is made: `turn_admission` captures a presented credential into
    /// `TurnCredential::Forwarded` and the dispatch client then puts it on the
    /// upstream request. A launch sentinel that arrived on every turn and was
    /// never captured as a seat is what stops a deployment forwarding a value
    /// that authenticates nothing to a real frontier.
    pub fn any_credential_forwarded(&self) -> bool {
        self.quotes
            .lock()
            .expect("recording")
            .iter()
            .any(|quote| quote.credential.is_forwarded())
    }
}

/// One turn that speaks `text` and stops.
pub fn prose_turn(text: &'static str) -> ToolCallingFrontierClient {
    ToolCallingFrontierClient::new(vec![Scripted::Text(text)], Some("end_turn"))
}

#[async_trait]
impl FrontierClient for ScriptedTurns {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.quotes.lock().expect("recording").push(quote.clone());
        // Popped and the guard dropped before the await: holding a `std::sync`
        // lock across an await point in a multi-threaded runtime is how a rig
        // deadlocks under `--test-threads=1` and gets diagnosed as a client hang.
        let queued = self.queued.lock().expect("recording").pop_front();
        match queued {
            Some(turn) => turn.execute(quote).await,
            None => self.then.execute(quote).await,
        }
    }
}
