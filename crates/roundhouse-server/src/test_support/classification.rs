// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Classification fixtures shared across `classify_runtime`'s own unit
//! tests and the classification integration binaries under `tests/`.
//!
//! Server-7's review (PR 18, round 1): the `ClassifyConfig` JSON body was
//! written out by hand in seven files, `ANSWER` was retyped byte-for-byte in
//! five of them (a sixth carried it under the name `CLASSIFIER_ANSWER`), and
//! the loopback `/systemone` service, the one-model catalog, `Answering` and
//! `admission_allowing` were each copied at least once — `classification_runtime.rs`'s
//! own `local_only` said so out loud ("kept local here rather than shared
//! because this file has exactly one caller for it"), which stopped being true
//! the moment `classification_settlement_recovery.rs` carried the identical
//! construction. One copy here, reached through the same `test-support`
//! feature [`super`]'s own fixtures already use.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use roundhouse_core::control::{TargetFilter, TurnPolicy};
use roundhouse_core::event::CacheReadSource;
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierError, FrontierModelSpec, FrontierQuote, FrontierStream,
    StaticFrontierCatalog, WireProtocol,
};

use crate::Admission;
use crate::classify_config::ClassifyConfig;
use crate::test_support::frontier_spec;

/// A complete, valid answer set across all three taxonomy axes — the shape a
/// deployment's classifier answers with, byte-for-byte, on the ordinary path.
///
/// Retyped identically in `typesafe_shadow/tests/mod.rs`,
/// `classify_runtime/tests.rs`, `tests/classification_runtime.rs`,
/// `tests/classification_settlement_recovery.rs` and
/// `tests/roundhouse_binary_classification_boot.rs`
/// (`tests/classification_prefix_admission.rs` carried it too, under the name
/// `CLASSIFIER_ANSWER`) before this consolidation.
pub const ANSWER: &str = r#"{"model":"jev-1.12","answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
},"usage":{"input_tokens":312,"output_tokens":48}}"#;

/// [`ANSWER`] with no `usage` object at all — the service answering a
/// classification with nothing to report it cost, as opposed to
/// `typesafe_shadow`'s own `ANSWER_PARTIAL_USAGE` (a `usage` object present
/// but missing `input_tokens`), which is a different failure and stays local
/// to that suite.
pub const ANSWER_WITHOUT_USAGE: &str = r#"{"model":"jev-1.12","answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
}}"#;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The `ClassifyConfig` JSON body six of the seven copies this replaces
/// agreed on byte-for-byte apart from `base_url`, `enabled` and the auth env
/// var's name: revision 4, `jev-1.12`, eight in-flight calls at two-wide HTTP
/// concurrency, an 8 KiB request/prompt cap and a $25 total budget.
/// `overrides` is for the one axis a caller genuinely needs different —
/// `classification_window_engine.rs` narrows `executor.max_in_flight` to 1 so
/// its classifier stays saturated for the whole test, and
/// `roundhouse_binary_classification_boot.rs` pins its own `revision` and
/// `auth.env` because both are asserted on elsewhere in that file.
pub fn classify_config_json(
    base_url: &str,
    overrides: impl FnOnce(&mut serde_json::Value),
) -> String {
    let mut value: serde_json::Value = serde_json::from_str(&format!(
        r#"{{
          "enabled": true,
          "revision": 4,
          "model": "jev-1.12",
          "base_url": "{base_url}",
          "auth": {{ "env": "CLASSIFY_TEST_KEY" }},
          "pricing": {{ "input_per_mtok_usd": 0.042, "output_per_mtok_usd": 0.084 }},
          "expected_output_tokens": 24,
          "caps": {{
            "max_prior_classifications": 4,
            "max_prior_turns": 4,
            "max_prompt_chars": 2000,
            "max_total_bytes": 8192
          }},
          "transport": {{
            "max_request_bytes": 65536,
            "max_response_bytes": 16384,
            "deadline_ms": 4000
          }},
          "executor": {{
            "max_in_flight": 8,
            "max_http_concurrency": 2,
            "call_ttl_ms": 60000,
            "result_retention_ms": 900000,
            "sweep_interval_ms": 50
          }},
          "budget": {{ "limit_usd": 25.0, "window": "total", "warn_at": 0.8 }}
        }}"#
    ))
    .expect("the literal above is valid JSON");
    overrides(&mut value);
    value.to_string()
}

/// [`classify_config_json`], parsed — the shape every call site but
/// `classify_config/tests.rs` (whose literals are themselves the input under
/// test, and stay local) actually wants.
pub fn classify_config(
    base_url: &str,
    overrides: impl FnOnce(&mut serde_json::Value),
) -> ClassifyConfig {
    ClassifyConfig::from_json(&classify_config_json(base_url, overrides), "<test>")
        .expect("a valid configuration")
}

// ---------------------------------------------------------------------------
// The loopback classifier
// ---------------------------------------------------------------------------

/// What the loopback service answers with, and after how long.
struct Behavior {
    body: &'static str,
    delay: Duration,
}

/// A loopback `/systemone` service, for `ClassificationRuntime`'s own HTTP
/// client to call against instead of a real provider.
///
/// [`Self::start`] answers the shared [`ANSWER`], for
/// `tests/classification_runtime.rs`; [`Self::answering`] takes a
/// caller-chosen fixed body, for `tests/classification_settlement_recovery.rs`.
/// Every shape still counts calls and captures bodies, and derives each
/// body's own `state` field through [`Self::states`] — doing so costs nothing
/// a caller that does not read them notices.
///
/// **Not the only loopback fixture in this crate**, and deliberately so:
/// `classify_runtime`'s own unit tests keep a local `upstream`/`counted_upstream`
/// pair because several of their callers need an answer *delayed* by a caller-
/// chosen [`Duration`] — a shape this type does not offer, and adding it back
/// would only relocate the dead constructor a round of review already found
/// zero callers for. `tests/classification_prefix_admission.rs` keeps its own
/// `Classifier` because it deliberately bumps its call count *after* the body
/// is captured — the reverse of [`handle`]'s order below — so its
/// `await_calls` can wait on the count and then read the body it counted
/// without racing the write; see [`handle`]'s own doc for why this type's
/// order is safe for its own callers instead.
pub struct ClassifierUpstream {
    pub base_url: String,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone)]
struct AppState {
    behavior: Arc<Behavior>,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

/// Bumps the count before the body is captured. Safe for every caller in
/// this crate today: none of them waits on [`ClassifierUpstream::count`]
/// reaching a value and then immediately reads [`ClassifierUpstream::bodies`]
/// expecting that request's body to already be there — the classify_runtime
/// suites that read the count wait on the mailbox's own `await_parked`
/// instead, which only resolves once this handler has returned. A caller
/// that did want "count `N` implies body `N` is captured" — as
/// `tests/classification_prefix_admission.rs`'s own loopback does — needs
/// the reverse order; see [`ClassifierUpstream`]'s own doc for why that copy
/// stays separate rather than swapping this one.
async fn handle(
    axum::extract::State(state): axum::extract::State<AppState>,
    body: String,
) -> axum::response::Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    state.bodies.lock().unwrap().push(body);
    if !state.behavior.delay.is_zero() {
        tokio::time::sleep(state.behavior.delay).await;
    }
    axum::response::Response::new(axum::body::Body::from(state.behavior.body))
}

impl ClassifierUpstream {
    /// Answers the shared [`ANSWER`] immediately.
    pub async fn start() -> Self {
        Self::configured(Behavior {
            body: ANSWER,
            delay: Duration::ZERO,
        })
        .await
    }

    /// Answers `body` immediately.
    pub async fn answering(body: &'static str) -> Self {
        Self::configured(Behavior {
            body,
            delay: Duration::ZERO,
        })
        .await
    }

    async fn configured(behavior: Behavior) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let state = AppState {
            behavior: Arc::new(behavior),
            calls: Arc::clone(&calls),
            bodies: Arc::clone(&bodies),
        };
        let app = axum::Router::new()
            .route("/systemone", axum::routing::post(handle))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base_url: format!("http://{addr}"),
            calls,
            bodies,
        }
    }

    pub fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Every request body received, verbatim, in arrival order.
    pub fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }

    /// Every captured body's own `state` field — the rendered projection
    /// actually sent — in arrival order.
    pub fn states(&self) -> Vec<String> {
        self.bodies()
            .iter()
            .map(|body| {
                let sent: serde_json::Value = serde_json::from_str(body).expect("JSON");
                sent["state"].as_str().expect("a state").to_string()
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Fleet: one priced frontier model, and a client that always answers
// ---------------------------------------------------------------------------

/// The provider name [`classification_catalog`] and [`Answering`] agree on.
pub const PROVIDER: &str = "alpha";

/// A catalog of one priced frontier model at [`PROVIDER`] — the exact
/// construction `tests/classification_runtime.rs` and
/// `tests/classification_settlement_recovery.rs` each hand-rolled: quality
/// 0.9, free pricing, negligible TTFT, so nothing about routing preference or
/// cost competes with what a test is actually classifying.
pub fn classification_catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        quality_prior: 0.9,
        pricing: roundhouse_core::routing::ProviderPricing::free(),
        base_ttft_ms: 1.0,
        ttft_ms_per_uncached_token: 0.0,
        ..frontier_spec(PROVIDER, "m", WireProtocol::OpenAiResponses)
    }])
}

/// A [`FrontierClient`] that always answers `"done"` — the shape both
/// classification integration suites used to dispatch a turn without caring
/// what came back, only that a turn ran to completion.
pub struct Answering;

#[async_trait]
impl FrontierClient for Answering {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        Ok(FrontierChunk::whole_response(
            "done".to_string(),
            quote.prompt.len() as u64,
            0,
            CacheReadSource::Provider,
            4,
            0,
        ))
    }
}

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

/// An [`Admission`] whose policy allows routing only to targets matching
/// `pattern` — the construction `tests/classification_settlement_recovery.rs`
/// named and `tests/classification_runtime.rs`'s own `local_only` duplicated,
/// its doc admitting the duplication existed ("kept local here rather than
/// shared because this file has exactly one caller for it").
pub fn admission_allowing(pattern: &str) -> Admission {
    Admission {
        policy: Arc::new(TurnPolicy {
            min_quality: 0.0,
            allow: TargetFilter::parse([pattern]).expect("a valid filter"),
            frontier_cadence: None,
        }),
        ..Admission::open()
    }
}
