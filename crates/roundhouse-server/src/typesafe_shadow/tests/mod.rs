// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The policy boundary, against a loopback upstream and a recording ledger.
//!
//! Every assertion here is about a call that must *not* happen, or about what
//! was booked when one did. What the wire looks like is the fleet crate's.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use axum::routing::post;

use roundhouse_core::classify::projection::PromptCapture;
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    Allocation, Budget, BudgetWindow, Exhaustion, Grant, GrantRequest, LedgerState, Secret,
    Settled, SpendError,
};
use roundhouse_core::item::Item;
use roundhouse_fleet::typesafe::SystemOneLimits;

use super::*;

mod accounting;
mod admission;
mod ledger_deadline;
mod model_identity;
mod projection;
mod question;
mod settlement_order;

const KEY: &str = "sk-ZZZQQQ-typesafe-deployment-key";

// ---------------------------------------------------------------- upstream

#[derive(Clone)]
struct Upstream {
    body: &'static str,
    calls: Arc<AtomicUsize>,
    /// Every body that arrived, verbatim. The bound and the quote are both
    /// asserted against these bytes rather than against what the helper
    /// returned, which is the only way to catch either being lost on the way
    /// to the socket.
    seen: Arc<Mutex<Vec<String>>>,
}

async fn handle(State(state): State<Upstream>, body: String) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    state.seen.lock().unwrap().push(body);
    Response::new(Body::from(state.body))
}

/// A complete, valid answer set across all three axes.
pub(crate) const ANSWER: &str = r#"{"model":"jev-1.12","answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
},"usage":{"input_tokens":312,"output_tokens":48}}"#;

/// The same answers, with only half the accounting reported.
const ANSWER_PARTIAL_USAGE: &str = r#"{"model":"jev-1.12","answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
},"usage":{"output_tokens":48}}"#;

/// Two good axes and one whose distribution does not sum to one. Fully billed.
const ANSWER_UNUSABLE: &str = r#"{"model":"jev-1.12","answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.1,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
},"usage":{"input_tokens":312,"output_tokens":48}}"#;

/// One axis missing. A partial taxonomy supplies no classification at all.
const ANSWER_PARTIAL_TAXONOMY: &str = r#"{"model":"jev-1.12","answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61}
},"usage":{"input_tokens":312,"output_tokens":48}}"#;

async fn upstream(body: &'static str) -> (SocketAddr, Upstream) {
    let state = Upstream {
        body,
        calls: Arc::new(AtomicUsize::new(0)),
        seen: Arc::new(Mutex::new(Vec::new())),
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

impl Upstream {
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The one body that arrived, verbatim.
    fn body(&self) -> String {
        let seen = self.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one request arrived");
        seen[0].clone()
    }

    /// The `state` field out of that body.
    fn state(&self) -> String {
        let sent: serde_json::Value =
            serde_json::from_str(&self.body()).expect("a JSON body arrived");
        sent["state"]
            .as_str()
            .expect("`state` is a string")
            .to_string()
    }
}

// ------------------------------------------------------------------ ledger

/// What the ledger should answer, and what it was asked.
struct RecordingLedger {
    /// `None` makes `open_grant` fail, standing in for a ledger that is down.
    grants: Option<f64>,
    /// `false` makes `settle_grant` fail *after* the call was made and priced:
    /// the one state in which the settlement acknowledgement is the only record
    /// that the spend exists at all.
    settles: bool,
    requested: Mutex<Vec<f64>>,
    settled: Mutex<Vec<f64>>,
}

impl RecordingLedger {
    fn granting(amount: f64) -> Arc<Self> {
        Arc::new(Self {
            grants: Some(amount),
            settles: true,
            requested: Mutex::new(Vec::new()),
            settled: Mutex::new(Vec::new()),
        })
    }

    /// A ledger that funds the call and then cannot be told what it cost.
    fn granting_but_unsettleable(amount: f64) -> Arc<Self> {
        Arc::new(Self {
            grants: Some(amount),
            settles: false,
            requested: Mutex::new(Vec::new()),
            settled: Mutex::new(Vec::new()),
        })
    }

    fn unavailable() -> Arc<Self> {
        Arc::new(Self {
            grants: None,
            settles: true,
            requested: Mutex::new(Vec::new()),
            settled: Mutex::new(Vec::new()),
        })
    }

    fn settled(&self) -> Vec<f64> {
        self.settled.lock().unwrap().clone()
    }

    /// Every amount a hold was asked for. Empty means no grant was ever opened,
    /// which is a stronger claim than "the hold was handed back".
    fn requested(&self) -> Vec<f64> {
        self.requested.lock().unwrap().clone()
    }
}

#[async_trait]
impl SpendLedger for RecordingLedger {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        self.requested.lock().unwrap().push(request.requested_usd);
        match self.grants {
            // A ledger that grants "everything asked for" would hide a short
            // grant, so the amount is fixed by the test rather than by the ask.
            Some(granted_usd) => Ok(Grant {
                granted_usd,
                state: LedgerState::Unconstrained,
            }),
            None => Err(SpendError::Backend(anyhow::anyhow!("ledger unreachable"))),
        }
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        // Recorded before the failure branch, so `settled()` still says what the
        // module tried to commit rather than only what a ledger accepted.
        self.settled.lock().unwrap().push(settlement.actual_usd);
        if !self.settles {
            return Err(SpendError::Backend(anyhow::anyhow!(
                "the settle could not be applied"
            )));
        }
        Ok(Settled {
            applied: true,
            released_usd: 0.0,
            committed_usd: settlement.actual_usd,
        })
    }

    async fn balance(
        &self,
        _query: roundhouse_core::control::BalanceQuery,
    ) -> Result<roundhouse_core::control::Balance, SpendError> {
        unimplemented!("no test here reads a balance")
    }
}

// ------------------------------------------------------------------ set-up

pub(crate) fn caps() -> ProjectionCaps {
    ProjectionCaps {
        max_prior_classifications: 4,
        max_prompt_chars: 2_000,
        max_total_bytes: 8 * 1024,
    }
}

/// A rate card, stated by this test and not by the source. Deliberately not
/// the published one: nothing here should be read as roundhouse's price list.
///
/// Both axes are non-zero and different, so a quote that dropped either one is
/// visible in the number rather than hidden by a zero rate.
pub(crate) fn pricing() -> ProviderPricing {
    ProviderPricing {
        input_per_mtok_usd: 1.0,
        cached_input_per_mtok_usd: 0.0,
        cache_write_per_mtok_usd: 0.0,
        output_per_mtok_usd: 2.0,
    }
}

/// What this deployment tells the ledger a call of `input_tokens` will cost.
fn quote_usd(input_tokens: usize) -> f64 {
    (input_tokens as f64 * 1.0 + EXPECTED_OUTPUT_TOKENS as f64 * 2.0) / 1_000_000.0
}

/// What a reported usage of 312 input and 48 output tokens is priced at.
const REPORTED_USD: f64 = (312.0 * 1.0 + 48.0 * 2.0) / 1_000_000.0;

pub(crate) const EXPECTED_OUTPUT_TOKENS: u64 = 16;
pub(crate) const CONFIG_REVISION: u32 = 7;

pub(crate) fn config() -> ShadowConfig {
    ShadowConfig::new(
        "jev-1.12",
        pricing(),
        EXPECTED_OUTPUT_TOKENS,
        caps(),
        CONFIG_REVISION,
    )
}

pub(crate) fn limits() -> SystemOneLimits {
    SystemOneLimits {
        max_request_bytes: 64 * 1024,
        max_response_bytes: 16 * 1024,
        deadline_ms: 2_000,
    }
}

fn shadow(
    addr: SocketAddr,
    config: ShadowConfig,
    ledger: Arc<RecordingLedger>,
) -> TypeSafeShadow<ByteTokenizer> {
    let client = SystemOneClient::new(format!("http://{addr}"), limits()).unwrap();
    TypeSafeShadow::new(client, config, ledger, ByteTokenizer)
}

pub(crate) fn terms() -> BudgetTerms {
    BudgetTerms {
        budget: Budget {
            limit_usd: 1_000.0,
            window: BudgetWindow::Total,
            on_exhaustion: Exhaustion::Refuse,
            warn_at: 0.8,
        },
        allocation: Allocation::Pooled,
    }
}

fn credential() -> TurnCredential {
    TurnCredential::Stored(Secret::api_key(KEY).unwrap())
}

const NOW_MS: u64 = 1_000;
const EXPIRES_MS: u64 = 31_000;

fn call(credential: &TurnCredential) -> ShadowCall<'_> {
    ShadowCall {
        principal: Principal::new("proj_shadow", "user_shadow"),
        session_id: SessionId::new("sess_shadow"),
        call_id: ResponseId::new("shadow_1"),
        source_turn_index: 3,
        source_response_id: ResponseId::new("resp_3"),
        terms: terms(),
        credential,
        now_ms: NOW_MS,
        expires_at_ms: EXPIRES_MS,
    }
}

/// A frontier target this session's policy admitted.
fn frontier() -> Target {
    Target::Frontier {
        provider: "anthropic".into(),
        model: "claude-opus-4".into(),
    }
}

fn local() -> Target {
    Target::Local {
        worker_id: 7,
        dp_rank: 0,
        model: "llama-3.1-8b".into(),
    }
}

/// This turn's input, as the client sent it.
fn items() -> Vec<Item> {
    vec![
        Item::system_text("You are working in a Rust repository."),
        Item::user_text("the parser drops trailing commas; fix it and prove it"),
    ]
}

fn capture() -> PromptCapture {
    PromptCapture::of(&items(), &caps())
}

/// A deadline far enough away that nothing in these tests reaches it.
///
/// Expiry has its own tests in the runtime; here it must not be what a case is
/// measuring.
fn never() -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(3_600)
}

/// Project, prepare and execute one call, the way the engine and its worker do
/// between them.
///
/// The three steps stay separate in the source because the engine writes a
/// durable record between the second and the third, and because only the third
/// touches a ledger or a socket; a test that only ever wanted the end state
/// would otherwise re-type the sequence in every file.
///
/// `Err` is a refusal taken **before** an intent would exist. Everything a
/// ledger can refuse comes back as an `Ok` record carrying
/// [`ClassificationOutcome::Unfunded`], because by then the intent is durable.
async fn classify(
    shadow: &TypeSafeShadow<ByteTokenizer>,
    credential: &TurnCredential,
    admitted: Option<&[Target]>,
) -> Result<ClassificationRecord, NotRun> {
    let projection = shadow.projection(&capture(), &[], &[])?;
    let prepared = shadow.prepare(call(credential), &projection, admitted)?;
    Ok(shadow.execute(prepared, never()).await)
}
