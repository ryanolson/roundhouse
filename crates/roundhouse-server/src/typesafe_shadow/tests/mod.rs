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

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    Allocation, Budget, BudgetState, BudgetWindow, Exhaustion, FrontierCadence, FrontierHistory,
    Grant, LedgerState, PresentedCredential, Secret, Settled, SpendError, TargetFilter, TurnBudget,
    TurnPolicy,
};
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::routing::{CacheLedger, Candidate, RoutingContext, Target};
use roundhouse_fleet::typesafe::SystemOneLimits;

use super::*;

mod accounting;
mod admission;
mod brief;
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

const ANSWER: &str = r#"{"model":"jev-1.12","answers":{"tier":{"type":"choice","choice":"capable","probabilities":{"capable":0.85,"efficient":0.15},"confidence":0.82}},"usage":{"input_tokens":312,"output_tokens":48}}"#;
/// A usable answer whose accounting is only half reported.
const ANSWER_PARTIAL_USAGE: &str = r#"{"model":"jev-1.12","answers":{"tier":{"type":"choice","choice":"capable","probabilities":{"capable":0.85,"efficient":0.15},"confidence":0.82}},"usage":{"output_tokens":48}}"#;
/// An unusable distribution, fully billed.
const ANSWER_UNUSABLE: &str = r#"{"model":"jev-1.12","answers":{"tier":{"type":"choice","choice":"capable","probabilities":{"capable":0.4,"efficient":0.2},"confidence":0.8}},"usage":{"input_tokens":312,"output_tokens":48}}"#;

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
    /// the one state in which this module's settle warning is the only record
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

fn caps() -> ShadowCaps {
    ShadowCaps {
        brief: BriefConfig::default(),
        max_tool_name_chars: 64,
        max_facts: 8,
        max_fact_chars: 200,
        max_plan_steps: 8,
        max_state_bytes: 8 * 1024,
    }
}

/// A rate card, stated by this test and not by the source. Deliberately not
/// the published one: nothing here should be read as roundhouse's price list.
///
/// Both axes are non-zero and different, so a quote that dropped either one is
/// visible in the number rather than hidden by a zero rate.
fn pricing() -> ProviderPricing {
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

const EXPECTED_OUTPUT_TOKENS: u64 = 16;

fn config() -> ShadowConfig {
    ShadowConfig::new("jev-1.12", pricing(), EXPECTED_OUTPUT_TOKENS, caps())
}

fn limits() -> SystemOneLimits {
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

fn terms() -> BudgetTerms {
    BudgetTerms {
        budget: Budget {
            limit_usd: 1_000.0,
            window: BudgetWindow::Total,
            on_exhaustion: Exhaustion::degrade_with_overflow(),
            warn_at: 0.8,
        },
        allocation: Allocation::Pooled,
    }
}

fn credential() -> TurnCredential {
    TurnCredential::Stored(Secret::api_key(KEY).unwrap())
}

fn call(credential: &TurnCredential) -> ShadowCall<'_> {
    ShadowCall {
        principal: Principal::new("proj_shadow", "user_shadow"),
        session_id: SessionId::new("sess_shadow"),
        hold_key: ResponseId::new("shadow_1"),
        terms: terms(),
        credential,
        now_ms: 1_000,
    }
}

fn candidate(target: Target) -> Candidate {
    Candidate {
        target,
        expected_prefill_tokens: 1_000.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: 100.0,
        expected_cost_usd: 0.01,
        quality_prior: 0.8,
        load: None,
    }
}

fn frontier() -> Candidate {
    candidate(Target::Frontier {
        provider: "anthropic".into(),
        model: "claude-opus-4".into(),
    })
}

/// Below the quality floor the relevant case sets.
fn dim_frontier() -> Candidate {
    Candidate {
        quality_prior: 0.1,
        ..frontier()
    }
}

/// Above that floor, so the quality case leaves a non-empty admitted pool and
/// exercises the local-only branch rather than an admission error.
fn high_quality_local() -> Candidate {
    Candidate {
        quality_prior: 0.9,
        ..local()
    }
}

/// Free, so a budget ceiling that excludes the frontier candidate still leaves
/// this one admissible — otherwise `admissible` returns an error rather than a
/// local-only pool, and the case under test never arises.
fn local() -> Candidate {
    Candidate {
        expected_cost_usd: 0.0,
        ..candidate(Target::Local {
            worker_id: 7,
            dp_rank: 0,
            model: "llama-3.1-8b".into(),
        })
    }
}

/// Everything `RoutingContext::admissible` needs, owned so the borrow lives.
struct Pool {
    session_id: SessionId,
    candidates: Vec<Candidate>,
    ledger: CacheLedger,
    policy: TurnPolicy,
    history: FrontierHistory,
    budget: TurnBudget,
}

impl Pool {
    fn of(candidates: Vec<Candidate>) -> Self {
        Self {
            session_id: SessionId::new("sess_shadow"),
            candidates,
            ledger: CacheLedger::default(),
            policy: TurnPolicy::unrestricted(),
            history: FrontierHistory::default(),
            budget: TurnBudget::Unlimited,
        }
    }

    fn under(mut self, policy: TurnPolicy) -> Self {
        self.policy = policy;
        self
    }

    fn with_budget(mut self, budget: TurnBudget) -> Self {
        self.budget = budget;
        self
    }

    fn admitted(&self) -> roundhouse_core::routing::Admitted<'_> {
        RoutingContext {
            session_id: &self.session_id,
            turn_index: 0,
            isl_tokens: 1_000,
            candidates: &self.candidates,
            ledger: &self.ledger,
            turn_policy: &self.policy,
            frontier_history: &self.history,
            budget: &self.budget,
            signals: None,
            tiers: None,
        }
        .admissible(None)
        .expect("the fixture pool is admissible")
    }
}

fn items() -> Vec<Item> {
    vec![
        Item::system_text("You are working in a Rust repository."),
        Item::user_text("the parser drops trailing commas; fix it and prove it"),
    ]
}
