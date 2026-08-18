// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M3 of `PLAN-agentic-control-plane.md`: a budget that visibly changes
//! routing, and an exhausted one that keeps serving.
//!
//! Every claim here is about *what a deployment does once the money runs out*,
//! which is why none of them is a unit test. The contract suite one layer down
//! already proves that [`SpendLedger`] grants what it says it grants; what it
//! cannot prove is that the grant reaches the router, that a zero ceiling puts
//! a turn on a local worker instead of failing it, that the spend then settles
//! back into the same counter the next turn reads, and that a client sees a
//! perfectly ordinary answer throughout. That trip is the whole of this
//! milestone.
//!
//! # The fixture, and why it is arranged this way
//!
//! Three knobs are turned away from their defaults, each so that the *budget*
//! is the only thing left that could have moved a decision:
//!
//! - **The router's own cost axis is switched off** ([`Weights::cost`] at
//!   zero). Left on, the free local worker outscores a paid hosted model on
//!   every turn and the whole suite would be watching the scorer rather than
//!   the ledger. With it off, price stops being a routing input and becomes
//!   purely a spending one — which is exactly the separation the budget exists
//!   to enforce.
//! - **The local worker is given a five-second latency floor**, so the hosted
//!   model wins while there is money and local wins the moment there is not.
//! - **The hosted model is priced only on output, at a round rate**, so one
//!   turn costs exactly [`ACTUAL_TURN_USD`] no matter how long the
//!   conversation grows. A per-turn cost that drifted with the transcript
//!   would make "the fourth turn is the one that runs out" a fact about
//!   arithmetic rather than about budgets.
//!
//! The reply is deliberately twice as long as [`EXPECTED_OUTPUT_TOKENS`], so
//! every turn settles at twice its hold. That is not a trick to make the
//! numbers work: it is the authorization-hold limitation the ledger documents
//! about itself, and running the whole suite over it means the overcommit path
//! is the *ordinary* path here rather than an edge case with one test.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    Balance, BalanceQuery, BudgetState, BudgetTerms, Grant, GrantRequest, MemorySpendLedger,
    Principal, Settled, Settlement, SpendError, SpendLedger,
};
use roundhouse_core::event::{Accounting, IncompleteReason, SessionEvent, SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::metrics::{MetricsConfig, ShadowPricing};
use roundhouse_core::now_ms;
use roundhouse_core::routing::policy::Weights;
use roundhouse_core::routing::{AffinityPolicy, CacheModel, DecisionRecord, ProviderPricing};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierChunk, FrontierClient, FrontierError, FrontierModelSpec,
    FrontierQuote, FrontierStream, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, EchoLocalExecutor, Engine, EngineConfig, http, metrics_api,
    responses_api,
};

mod common;
use common::codex::{request, user_message};
use common::{BLOCK_SIZE, LOCAL_MODEL, MINUTE, embedded_fleet, path_segment};

/// What each executor answers with, so a target is legible in the answer as
/// well as in the log.
const LOCAL_ANSWER: &str = "local answer";
/// Sixteen bytes, which is [`EXPECTED_OUTPUT_TOKENS`] doubled — see the module
/// note on why every turn settles above its hold.
const FRONTIER_ANSWER: &str = "frontier answer!";

/// Dollars per million output tokens. Input is free in this catalog, so a
/// turn's price is a function of the answer and of nothing else.
const OUTPUT_PER_MTOK_USD: f64 = 12_500.0;
/// What the engine tells the catalog to quote against, and therefore what one
/// turn is *granted*.
const EXPECTED_OUTPUT_TOKENS: u32 = 8;
/// `EXPECTED_OUTPUT_TOKENS * OUTPUT_PER_MTOK_USD / 1e6` — the hold one frontier
/// turn takes out.
const EXPECTED_TURN_USD: f64 = 0.1;
/// `FRONTIER_ANSWER.len() * OUTPUT_PER_MTOK_USD / 1e6` — what one frontier turn
/// actually settles.
const ACTUAL_TURN_USD: f64 = 0.2;
/// Every budgeted project's ceiling.
///
/// Five and a half holds, which is the point: the third turn opens with
/// `LIMIT_USD - 2 * ACTUAL_TURN_USD` = $0.15 left, comfortably more than the
/// $0.10 it needs, and settles the project to $0.60 — past its limit and with
/// nothing left for a fourth. Neither comparison is near a knife edge, so no
/// assertion here rests on two floats being exactly equal.
const LIMIT_USD: f64 = 0.55;
/// How many frontier turns it takes to exhaust [`LIMIT_USD`].
const TURNS_TO_EXHAUSTION: usize = 3;
/// A member ceiling below one turn's hold: this key cannot afford a single
/// hosted turn however much its project has left.
const MEMBER_CAP_USD: f64 = 0.05;

/// Floating-point slack for a dollar comparison.
///
/// A tenth of a cent: far below any quantity this fixture distinguishes, far
/// above the rounding of a handful of multiplications.
const CENTS: f64 = 1e-4;

fn assert_usd(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < CENTS,
        "{what}: expected ${expected}, got ${actual}"
    );
}

// ---------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------

/// One hosted model, priced on output alone at a round rate.
fn metered_catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        provider: "anthropic".into(),
        model: "claude".into(),
        wire_protocol: WireProtocol::AnthropicMessages,
        cache_model: CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
        // Input free on purpose: it keeps a turn's price independent of how
        // long the conversation has grown, so the turn on which a budget runs
        // out is a fact about the budget.
        pricing: ProviderPricing {
            input_per_mtok_usd: 0.0,
            cached_input_per_mtok_usd: 0.0,
            cache_write_per_mtok_usd: 0.0,
            output_per_mtok_usd: OUTPUT_PER_MTOK_USD,
        },
        quality_prior: 0.95,
        base_ttft_ms: 1.0,
        ttft_ms_per_uncached_token: 0.0,
    }])
}

// ---------------------------------------------------------------------------
// Keys and the control plane that declares them
// ---------------------------------------------------------------------------

/// A well-shaped secret with `tag` legible inside it, padded to the 43 base62
/// characters the key format requires — the same fixture rule the policy and
/// tenancy suites use, and for the same reason: a hand-counted literal fails as
/// `malformed_key` for a reason no assertion names.
fn key(tag: &str) -> String {
    format!("rh_turn_{tag:A<43}")
}

fn sha256_hex(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// A project budget, spelled the way an operator writes one.
fn budget(on_exhaustion: &str, overflow: Option<bool>) -> serde_json::Value {
    let mut budget = serde_json::json!({
        "limit_usd": LIMIT_USD,
        "window": "total",
        "on_exhaustion": on_exhaustion,
    });
    if let Some(overflow) = overflow {
        budget["overflow_when_local_saturated"] = overflow.into();
    }
    budget
}

/// One project per exhaustion behavior under test, plus one with no budget at
/// all.
///
/// Built through `ControlPlaneConfig::from_json` rather than by assembling the
/// lookup tables: the file *is* the format, and the pairing of a project's
/// budget with a key's allocation happens inside `validate` — a fixture that
/// bypassed it would be testing terms no operator could write.
///
/// `shared` is the one project with two keys, and it is what makes the member
/// ceiling claim mean anything: `ada` and `bob` differ only by an
/// `"allocation"`, so whatever separates their routing came through that field
/// and through nothing else.
fn control_plane() -> Arc<ControlPlane> {
    let json = serde_json::json!({
        "projects": [
            { "id": "metered", "budget": budget("degrade_to_local", Some(true)) },
            { "id": "strict", "budget": budget("degrade_to_local", Some(false)) },
            { "id": "refusing", "budget": budget("refuse", None) },
            { "id": "shared", "budget": budget("degrade_to_local", Some(true)) },
            { "id": "openhanded" },
        ],
        "users": [{ "id": "ada" }, { "id": "bob" }],
        "keys": [
            { "project": "metered", "user": "ada", "key_sha256": sha256_hex(&key("metered")) },
            { "project": "strict", "user": "ada", "key_sha256": sha256_hex(&key("strict")) },
            { "project": "refusing", "user": "ada", "key_sha256": sha256_hex(&key("refusing")) },
            { "project": "shared", "user": "ada", "key_sha256": sha256_hex(&key("sharedada")) },
            {
                "project": "shared", "user": "bob", "key_sha256": sha256_hex(&key("sharedbob")),
                "allocation": { "capped": { "limit_usd": MEMBER_CAP_USD } }
            },
            { "project": "openhanded", "user": "ada", "key_sha256": sha256_hex(&key("openhanded")) },
        ],
    })
    .to_string();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "budget-routing fixture")
            .expect("the fixture config must validate"),
    ))
}

/// The same plane with every limit raised out of reach.
///
/// What an admin does when a project asks for more room, and the only way this
/// suite can express it: a `ControlPlane` is immutable once loaded, so raising
/// a limit means a second plane over the same engine and the same ledger —
/// which is exactly what a restart after an edit produces.
fn plane_with_a_raised_limit() -> Arc<ControlPlane> {
    let json = serde_json::json!({
        "projects": [{
            "id": "refusing",
            "budget": {
                "limit_usd": LIMIT_USD * 100.0,
                "window": "total",
                "on_exhaustion": "refuse",
            }
        }],
        "users": [{ "id": "ada" }],
        "keys": [
            { "project": "refusing", "user": "ada", "key_sha256": sha256_hex(&key("refusing")) },
        ],
    })
    .to_string();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "raised-limit fixture")
            .expect("the fixture config must validate"),
    ))
}

// ---------------------------------------------------------------------------
// Ledger doubles
// ---------------------------------------------------------------------------

/// A [`SpendLedger`] that counts what it was asked, and answers by delegating.
///
/// The only way to observe the claim decision 10 makes about the *absence* of
/// work: an unbudgeted admission must not touch the ledger at all, and "the
/// numbers came out the same" cannot tell a skipped call from a call that
/// happened to grant everything.
#[derive(Default)]
struct CountingLedger {
    inner: MemorySpendLedger,
    calls: AtomicUsize,
}

impl CountingLedger {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SpendLedger for CountingLedger {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.open_grant(request).await
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.settle_grant(settlement).await
    }

    async fn balance(&self, query: BalanceQuery) -> Result<Balance, SpendError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.balance(query).await
    }
}

/// A provider that answers and then says nothing about what it billed.
///
/// The ordinary case for a streaming OpenAI-compatible endpoint that was not
/// asked for usage, and the one the accounting-honesty claim turns on: the
/// engine fills the gap with its own estimate, and that estimate has to consume
/// budget like any other spend rather than being written off as free.
struct SilentFrontierClient {
    reply: String,
}

#[async_trait]
impl FrontierClient for SilentFrontierClient {
    async fn execute(&self, _quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        Ok(futures::stream::iter([Ok(FrontierChunk::OutputText(self.reply.clone()))]).boxed())
    }
}

// ---------------------------------------------------------------------------
// The deployment under test
// ---------------------------------------------------------------------------

struct Rig {
    app: Router,
    store: Arc<MemoryStore>,
    ledger: Arc<dyn SpendLedger>,
    plane: Arc<ControlPlane>,
    /// Kept so a test can put a *second* control plane in front of the same
    /// engine, ledger and log — which is the only honest way to express "an
    /// admin raised the limit", a plane being immutable once loaded.
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
}

impl Rig {
    /// The same deployment behind a different control plane.
    fn under(&self, plane: Arc<ControlPlane>) -> Rig {
        Rig {
            app: surfaces(&plane, &self.engine, &self.store),
            store: Arc::clone(&self.store),
            ledger: Arc::clone(&self.ledger),
            engine: Arc::clone(&self.engine),
            plane,
        }
    }
}

/// The three surfaces, over one plane, one engine and one log.
fn surfaces(
    plane: &Arc<ControlPlane>,
    engine: &Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: &Arc<MemoryStore>,
) -> Router {
    let metrics_config = Arc::new(MetricsConfig::new(ShadowPricing::new(Vec::new())));
    http::router(Arc::clone(plane), Arc::clone(engine), Arc::clone(store))
        .merge(metrics_api::metrics_router(
            Arc::clone(plane),
            engine.metrics(),
            metrics_config,
        ))
        .merge(responses_api::responses_router(
            Arc::clone(plane),
            Arc::clone(engine),
            Arc::clone(store),
        ))
}

/// How a rig is arranged. See the module note on why each knob is turned.
struct Fixture {
    /// Whether a local worker is quoted at all. `false` is the honest fixture
    /// for a saturated local pool: a deployment with no fleet has exactly the
    /// property the valve is about — no local candidate can take the turn —
    /// without a load number that would have to be tuned against a scheduler.
    fleet: bool,
    /// `false` restores the router's own cost axis, which makes the free local
    /// worker win every turn. Used only where the subject is a local turn.
    frontier_preferred: bool,
    frontier: Arc<dyn FrontierClient>,
    ledger: Arc<dyn SpendLedger>,
    /// An existing log to pick up rather than a fresh one — a successor
    /// process, from the store's point of view.
    store: Option<Arc<MemoryStore>>,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            fleet: true,
            frontier_preferred: true,
            frontier: Arc::new(EchoFrontierClient::new(FRONTIER_ANSWER)),
            ledger: Arc::new(MemorySpendLedger::new()),
            store: None,
        }
    }
}

async fn rig(plane: Arc<ControlPlane>, fixture: Fixture) -> Rig {
    ensure_rustls_crypto_provider();
    let store = fixture
        .store
        .unwrap_or_else(|| Arc::new(MemoryStore::new()));
    let weights = if fixture.frontier_preferred {
        // Price stops being a routing input and becomes purely a spending one.
        Weights {
            prefill: 1.0,
            cost: 0.0,
            ttft: 0.25,
        }
    } else {
        Weights::default()
    };
    let mut engine = Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new(LOCAL_ANSWER)),
        metered_catalog(),
        fixture.frontier,
        Arc::new(AffinityPolicy::new().with_weights(weights)),
        EngineConfig {
            block_size: BLOCK_SIZE,
            local_model: LOCAL_MODEL.to_string(),
            local_base_ttft_ms: 5_000.0,
            expected_output_tokens: EXPECTED_OUTPUT_TOKENS,
            ..Default::default()
        },
    )
    .with_spend_ledger(Arc::clone(&fixture.ledger));
    if fixture.fleet {
        engine = engine.with_fleet(embedded_fleet().await);
    }
    let engine = Arc::new(engine);
    let app = surfaces(&plane, &engine, &store);
    Rig {
        app,
        store,
        ledger: fixture.ledger,
        plane,
        engine,
    }
}

/// The default rig: a local worker, a hosted model the router prefers while
/// there is money, and a memory ledger.
async fn fleeted(plane: Arc<ControlPlane>) -> Rig {
    rig(plane, Fixture::default()).await
}

/// The same deployment with no local capacity at all — the saturated pool the
/// overflow valve exists for.
async fn fleetless(plane: Arc<ControlPlane>) -> Rig {
    rig(
        plane,
        Fixture {
            fleet: false,
            ..Default::default()
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Driving it
// ---------------------------------------------------------------------------

/// One turn on the native surface, which holds the history server-side.
///
/// The native surface throughout, rather than the Responses one, because every
/// claim here is about a *session over several turns* and replaying a growing
/// transcript through prefix admission would put the client's own history
/// bookkeeping between the test and the ledger. The two wire claims that do
/// need a client's view of one turn go through [`responses_turn`].
async fn native_turn(app: &Router, secret: &str, session_id: &str, turn_id: &str, prompt: &str) {
    let body = native_body(app, secret, session_id, turn_id, prompt).await;
    assert!(
        body.contains("event: response_completed"),
        "turn `{turn_id}` did not complete: {body}"
    );
}

/// One native turn, whatever it did, as the raw SSE body.
async fn native_body(
    app: &Router,
    secret: &str,
    session_id: &str,
    turn_id: &str,
    prompt: &str,
) -> String {
    let body = serde_json::json!({
        "turn_id": turn_id,
        "input": [{ "role": "user", "text": prompt }],
    })
    .to_string();
    let (status, text) = post(
        app,
        &format!("/v1/sessions/{}/responses", path_segment(session_id)),
        Some(secret),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "turn `{turn_id}`: {text}");
    text
}

/// One turn on the Responses surface, returning the raw SSE body.
async fn responses_turn(app: &Router, secret: &str, prompt: &str, cache_key: &str) -> String {
    let body = serde_json::to_string(&request(cache_key, vec![user_message(prompt)]))
        .expect("the request encodes");
    let (status, text) = post(app, "/v1/responses", Some(secret), &body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an exhausted budget must not change the HTTP status: {text}"
    );
    text
}

async fn create_session(app: &Router, secret: &str, session_id: &str) {
    let (status, text) = post(
        app,
        "/v1/sessions",
        Some(secret),
        &serde_json::json!({ "session_id": session_id }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "creating `{session_id}`: {text}");
}

async fn post(app: &Router, uri: &str, secret: Option<&str>, body: &str) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(CONTENT_TYPE, "application/json");
    if let Some(secret) = secret {
        builder = builder.header(AUTHORIZATION, format!("Bearer {secret}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).expect("request"))
        .await
        .expect("call");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Burn a project's whole budget on hosted turns, and prove that is what
/// happened.
///
/// Shared by every claim about what comes *after* exhaustion, so that no test
/// re-states the arithmetic and none of them can quietly disagree about which
/// turn the money ran out on.
async fn burn_the_budget(rig: &Rig, secret: &str, session_id: &str) {
    create_session(&rig.app, secret, session_id).await;
    for turn in 0..TURNS_TO_EXHAUSTION {
        native_turn(
            &rig.app,
            secret,
            session_id,
            &format!("burn{turn}"),
            "hello",
        )
        .await;
    }
    let decisions = decisions(&rig.store, session_id).await;
    assert_eq!(
        decisions
            .iter()
            .map(|decision| decision.chosen.policy_identity())
            .collect::<Vec<_>>(),
        vec!["anthropic/claude"; TURNS_TO_EXHAUSTION],
        "the burn has to actually reach the hosted model, or every claim below \
         is about a budget nothing ever spent"
    );
    // The fixture's arithmetic, checked rather than commented. Every count in
    // this file — three turns to exhaustion, the turn the warning lands on —
    // follows from these two numbers, and a catalog edit that moved either
    // would otherwise make those counts wrong in a way no assertion names.
    assert_usd(
        decisions[0].expected_cost_usd,
        EXPECTED_TURN_USD,
        "the hold one hosted turn takes out",
    );
    assert_usd(
        ACTUAL_TURN_USD,
        FRONTIER_ANSWER.len() as f64 * OUTPUT_PER_MTOK_USD / 1e6,
        "what one hosted turn settles",
    );
}

// ---------------------------------------------------------------------------
// Reading the log and the ledger
// ---------------------------------------------------------------------------

async fn log(store: &MemoryStore, session_id: &str) -> Vec<SessionEvent> {
    store
        .read_events(&SessionId::new(session_id), 0, 1024)
        .await
        .unwrap_or_else(|error| panic!("session `{session_id}` should exist: {error}"))
}

/// Every routing decision one session recorded, in log order.
async fn decisions(store: &MemoryStore, session_id: &str) -> Vec<DecisionRecord> {
    log(store, session_id)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .collect()
}

/// The one decision a single-turn session recorded.
async fn decision(store: &MemoryStore, session_id: &str) -> DecisionRecord {
    let mut all = decisions(store, session_id).await;
    assert_eq!(
        all.len(),
        1,
        "session `{session_id}` served exactly one turn"
    );
    all.remove(0)
}

/// Where a session's turns went, as policy identities in log order.
async fn route_sequence(store: &MemoryStore, session_id: &str) -> Vec<String> {
    decisions(store, session_id)
        .await
        .iter()
        .map(|decision| decision.chosen.policy_identity())
        .collect()
}

/// Every terminal event of one session, in log order.
async fn terminals(store: &MemoryStore, session_id: &str) -> Vec<(IncompleteReason, Usage)> {
    log(store, session_id)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ResponseIncomplete { reason, usage, .. } => Some((reason, usage)),
            _ => None,
        })
        .collect()
}

/// The usage every completed response of one session reported.
async fn completed_usage(store: &MemoryStore, session_id: &str) -> Vec<Usage> {
    log(store, session_id)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ResponseCompleted { usage, .. } => Some(usage),
            _ => None,
        })
        .collect()
}

/// The terms a key resolves to, read out of the plane rather than rebuilt.
///
/// A test that assembled its own [`BudgetTerms`] would be reading the ledger
/// under ceilings the deployment never applied, and would agree with a
/// validate boundary that had resolved them wrongly.
fn terms(plane: &ControlPlane, principal: &Principal) -> BudgetTerms {
    plane
        .configured_admissions()
        .find(|admission| &admission.principal == principal)
        .and_then(|admission| admission.budget.clone())
        .unwrap_or_else(|| panic!("{principal:?} must resolve to a budget for this read"))
}

/// One membership's position, as the ledger holds it.
async fn balance(rig: &Rig, project: &str, user: &str) -> Balance {
    let principal = Principal::new(project, user);
    let terms = terms(&rig.plane, &principal);
    rig.ledger
        .balance(BalanceQuery {
            principal,
            terms,
            now_ms: now_ms(),
        })
        .await
        .expect("the memory ledger answers every well-formed read")
}

/// The session id a Responses-surface cache key binds to for `principal`.
fn bound(project: &str, user: &str, cache_key: &str) -> String {
    format!("{project}/{user}/{cache_key}")
}

// ---------------------------------------------------------------------------
// The flagship
// ---------------------------------------------------------------------------

/// **The loudest test in the suite.** A project that has spent its budget keeps
/// answering, from our own fleet, at a perfectly ordinary 200.
///
/// The failure mode this guards is not a wrong number, it is a dead tenant: a
/// budget implemented as a tourniquet turns "you have spent your allowance"
/// into "every request now fails", and the client cannot tell that from an
/// outage. Degrade-to-local is the whole novel behavior of this milestone, and
/// it costs one comparison — local candidates are priced at zero, so a zero
/// ceiling excludes every hosted option and admits every local one through the
/// ordinary admissibility predicate, with no branch anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exhausted_frontier_budget_routes_local_instead_of_failing() {
    let rig = fleeted(control_plane()).await;
    let session = bound("metered", "ada", "burn");
    burn_the_budget(&rig, &key("metered"), &session).await;

    // The turn after the money ran out.
    native_turn(&rig.app, &key("metered"), &session, "after", "hello").await;

    let routes = route_sequence(&rig.store, &session).await;
    assert_eq!(
        routes.last().map(String::as_str),
        Some("local/local"),
        "the turn after exhaustion serves from our own fleet: {routes:?}"
    );
    let decisions = decisions(&rig.store, &session).await;
    assert_eq!(
        decisions.last().expect("four turns routed").budget_state,
        BudgetState::Exhausted,
        "and it says so on the decision, because a project that stayed under \
         budget by serving locally has not had the same month as one that \
         never needed to"
    );
    assert!(
        decisions[..TURNS_TO_EXHAUSTION]
            .iter()
            .all(|decision| decision.chosen.policy_identity() == "anthropic/claude"),
        "the control: while there was money the same prompt went to the hosted \
         model, so the last turn moving is the budget and not the scorer"
    );

    // And on the wire, for a client that has never heard of budgets: a fresh
    // conversation under the same exhausted project gets a 200 and an answer.
    let body = responses_turn(&rig.app, &key("metered"), "hello", "wire").await;
    assert!(
        body.contains("response.completed"),
        "an exhausted budget must still complete the response: {body}"
    );
    assert!(
        body.contains(LOCAL_ANSWER),
        "and the answer has to be a real one, served locally: {body}"
    );
    assert_eq!(
        decision(&rig.store, &bound("metered", "ada", "wire"))
            .await
            .budget_state,
        BudgetState::Exhausted,
    );

    // The ledger's own view: spent past the limit, nothing held, nothing left.
    let balance = balance(&rig, "metered", "ada").await;
    assert_usd(
        balance.committed_usd,
        TURNS_TO_EXHAUSTION as f64 * ACTUAL_TURN_USD,
        "three hosted turns settled and the two local ones settled nothing",
    );
    assert_usd(balance.held_usd, 0.0, "every hold was settled or released");
    assert_eq!(balance.project_remaining_usd, 0.0);
    assert_eq!(balance.state, BudgetState::Exhausted);
}

// ---------------------------------------------------------------------------
// The overflow valve
// ---------------------------------------------------------------------------

/// The escape valve: an exhausted budget over a local pool that cannot serve
/// dispatches on frontier anyway, and says so.
///
/// The budget is a ceiling on *choice*, not a tourniquet on service. Where
/// degrade-to-local has nowhere to degrade to, refusing the turn would trade a
/// cost overrun for an outage — so the turn goes out and the overspend is
/// recorded as loudly as the design can manage: it settles into committed spend
/// like any other dollar, the decision carries its own state rather than
/// passing for an ordinary exhausted turn, and the rationale names the reason
/// in words an operator can grep for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_overflow_valve_serves_frontier_when_local_is_saturated() {
    let rig = fleetless(control_plane()).await;
    let session = bound("metered", "ada", "valve");
    burn_the_budget(&rig, &key("metered"), &session).await;

    native_turn(&rig.app, &key("metered"), &session, "after", "hello").await;

    let overflowed = decisions(&rig.store, &session)
        .await
        .pop()
        .expect("four turns routed");
    assert_eq!(
        overflowed.chosen.policy_identity(),
        "anthropic/claude",
        "with no local candidate to take the turn, the valve re-admits the \
         hosted pool rather than failing"
    );
    assert_eq!(
        overflowed.budget_state,
        BudgetState::ExhaustedOverflow,
        "and it is a marked fact, not an ordinary exhausted turn: this is the \
         one number that answers `how much did this project spend past its \
         limit because its own fleet was full`"
    );
    assert!(
        overflowed.rationale.contains("no local candidate"),
        "the rationale has to name local saturation, or the audit trail says \
         only that money was spent: {}",
        overflowed.rationale
    );

    // The ledger visibly exceeds its own limit. Hiding the excess would make
    // the valve invisible to exactly the person who has to decide whether to
    // raise the limit or buy more workers.
    let balance = balance(&rig, "metered", "ada").await;
    assert_usd(
        balance.committed_usd,
        (TURNS_TO_EXHAUSTION + 1) as f64 * ACTUAL_TURN_USD,
        "the overspend settles like any other spend",
    );
    assert!(
        balance.committed_usd > LIMIT_USD,
        "and the ledger reads past its limit rather than clamping: ${} against ${LIMIT_USD}",
        balance.committed_usd
    );
}

/// With the valve off, the same deployment fails the turn — and the failure
/// names both facts.
///
/// The blame is the fleet's: the pool was emptied by having no capacity, not by
/// the tenant's policy, which is why this is not `policy_refused`. But an
/// operator told only that goes tuning workers without noticing that every
/// hosted candidate had already been excluded before capacity was ever
/// considered, so there was nothing to fall back to. Both facts, one message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overflow_off_fails_with_both_facts_named() {
    let rig = fleetless(control_plane()).await;
    let session = bound("strict", "ada", "novalve");
    burn_the_budget(&rig, &key("strict"), &session).await;

    let body = native_body(&rig.app, &key("strict"), &session, "after", "hello").await;
    assert!(
        !body.contains("event: response_completed"),
        "the valve is off, so this turn has nowhere to go: {body}"
    );
    assert!(
        body.contains("no candidate satisfied the routing policy's own constraints"),
        "the fleet is what emptied the pool, and the message has to say so: {body}"
    );
    assert!(
        body.contains("the budget is also exhausted"),
        "and the coincidence has to appear, or an operator tunes workers \
         without noticing there was nothing to fall back to: {body}"
    );

    // Fleet-shaped, not tenant-shaped. `policy_refused` would tell the client
    // that widening a policy is the fix for a deployment with no workers.
    assert_eq!(
        terminals(&rig.store, &session)
            .await
            .last()
            .map(|(reason, _)| reason.clone()),
        Some(IncompleteReason::UpstreamError),
    );

    // The control, and it is what keeps the assertion above from being "this
    // deployment fails everything": the identical fixture with the valve on.
    let armed = fleetless(control_plane()).await;
    let armed_session = bound("metered", "ada", "novalve");
    burn_the_budget(&armed, &key("metered"), &armed_session).await;
    native_turn(
        &armed.app,
        &key("metered"),
        &armed_session,
        "after",
        "hello",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Refusal
// ---------------------------------------------------------------------------

/// A project that refuses on exhaustion terminates its turn as a budget fact,
/// and the turn stays retryable.
///
/// Three refusals, three systems, and the blame is the whole point of keeping
/// them apart: `budget_exhausted` names a limit an admin can raise,
/// `policy_refused` names a decision only a widened policy moves, and
/// `upstream_error` names a fleet or a provider. A client that read one for
/// another would back off where it should give up, or give up where a retry
/// after an admin's edit would have worked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refuse_project_terminates_as_budget_exhausted_and_stays_retryable() {
    let rig = fleeted(control_plane()).await;
    let session = bound("refusing", "ada", "hard");
    burn_the_budget(&rig, &key("refusing"), &session).await;

    let body = native_body(&rig.app, &key("refusing"), &session, "after", "hello").await;
    assert!(
        !body.contains("event: response_completed"),
        "a refusing project refuses rather than degrading: {body}"
    );
    assert_eq!(
        terminals(&rig.store, &session)
            .await
            .last()
            .map(|(reason, _)| reason.clone()),
        Some(IncompleteReason::BudgetExhausted),
        "the refusal is a log fact naming the budget — not the policy, which \
         refused nothing, and not the fleet, which was never asked"
    );
    assert_eq!(
        decisions(&rig.store, &session).await.len(),
        TURNS_TO_EXHAUSTION,
        "a refused turn routes nowhere and records no decision"
    );

    // Retryable: an admin raises the limit and the identical turn serves. This
    // is the difference between a budget refusal and a policy refusal, and it
    // is why they are two reasons rather than one — the same engine, the same
    // ledger with its spend still in it, the same log, and one edited file.
    let raised = rig.under(plane_with_a_raised_limit());
    native_turn(&raised.app, &key("refusing"), &session, "again", "hello").await;
    assert_eq!(
        route_sequence(&raised.store, &session).await.pop(),
        Some("anthropic/claude".to_string()),
        "the retry serves, and serves hosted: the limit was the only thing \
         standing in its way"
    );
    assert!(
        balance(&raised, "refusing", "ada").await.committed_usd > LIMIT_USD,
        "and it spends against the spend the refusal already recorded, rather \
         than starting the project's month over"
    );
}

// ---------------------------------------------------------------------------
// Ceilings
// ---------------------------------------------------------------------------

/// A member ceiling binds on its own, while the project it draws on has money
/// to spare.
///
/// Both ceilings bind and the tighter wins — deliberately the opposite of the
/// shadowing rule LiteLLM ended up documenting as a gotcha, where a member cap
/// silently lifted the project's. `ada` and `bob` are in one project under one
/// budget and differ only by an `"allocation"`, so anything that separates
/// their routing came through that field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_member_ceiling_binds_even_when_the_project_has_room() {
    let rig = fleeted(control_plane()).await;

    responses_turn(&rig.app, &key("sharedada"), "hello", "chat").await;
    responses_turn(&rig.app, &key("sharedbob"), "hello", "chat").await;

    let ada = decision(&rig.store, &bound("shared", "ada", "chat")).await;
    let bob = decision(&rig.store, &bound("shared", "bob", "chat")).await;

    assert_eq!(
        ada.chosen.policy_identity(),
        "anthropic/claude",
        "the pooled member draws on the whole project budget"
    );
    assert_eq!(
        bob.chosen.policy_identity(),
        "local/local",
        "and the capped one cannot afford a single hosted turn, though the \
         project it belongs to has most of its budget left"
    );

    let project = balance(&rig, "shared", "ada").await;
    assert!(
        project.project_remaining_usd > EXPECTED_TURN_USD,
        "the project really did have room: ${} left",
        project.project_remaining_usd
    );
    let member = balance(&rig, "shared", "bob").await;
    assert_eq!(
        member.member_remaining_usd,
        Some(MEMBER_CAP_USD),
        "and bob spent nothing, because his ceiling never let him start"
    );
}

/// A turn served from our own fleet consumes no frontier budget.
///
/// The other half of "no unpriced frontier traffic": a local dispatch is not
/// unpriced, it is free, and charging it would make degrade-to-local drain the
/// very budget it exists to protect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_turn_consumes_no_frontier_budget() {
    // The router's own cost axis back on, so the free local worker wins on the
    // merits rather than because anything ran out.
    let counted = Arc::new(CountingLedger::default());
    let rig = rig(
        control_plane(),
        Fixture {
            frontier_preferred: false,
            ledger: Arc::clone(&counted) as Arc<dyn SpendLedger>,
            ..Default::default()
        },
    )
    .await;

    responses_turn(&rig.app, &key("metered"), "hello", "cheap").await;
    assert_eq!(
        decision(&rig.store, &bound("metered", "ada", "cheap"))
            .await
            .chosen
            .policy_identity(),
        "local/local",
        "the fixture has to actually route local, or this claim is vacuous"
    );

    let balance = balance(&rig, "metered", "ada").await;
    assert_usd(balance.committed_usd, 0.0, "a local turn settles nothing");
    assert_usd(
        balance.held_usd,
        0.0,
        "and leaves no hold behind either: the grant it opened was released",
    );
    assert_eq!(
        balance.state,
        BudgetState::Unconstrained,
        "an untouched budget is untouched"
    );
    assert!(
        counted.calls() > 1,
        "and the control that keeps every assertion above from being about a \
         ledger nobody wired: the turn did open a grant and did settle it, \
         both for nothing"
    );
}

/// A provider that reported nothing still spends money.
///
/// The engine fills the gap with its own estimate and stamps it as one. Both
/// halves matter and they pull in opposite directions: an estimate that did not
/// consume budget would let a provider with unreliable usage reporting spend a
/// project's whole month for free, and an estimate quietly merged into measured
/// spend would let the dashboard claim a precision nobody has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_estimated_usage_still_consumes_budget_and_stays_marked_estimated() {
    let rig = rig(
        control_plane(),
        Fixture {
            frontier: Arc::new(SilentFrontierClient {
                reply: FRONTIER_ANSWER.to_string(),
            }),
            ..Default::default()
        },
    )
    .await;

    responses_turn(&rig.app, &key("metered"), "hello", "quiet").await;

    let usage = completed_usage(&rig.store, &bound("metered", "ada", "quiet")).await;
    assert_eq!(
        usage.first().map(|usage| usage.accounting),
        Some(Accounting::Estimated),
        "a provider that said nothing about its billing gets an estimate, \
         marked as one"
    );

    let balance = balance(&rig, "metered", "ada").await;
    assert_usd(
        balance.committed_usd,
        ACTUAL_TURN_USD,
        "and the estimate consumes budget exactly as a reported usage would",
    );
}

// ---------------------------------------------------------------------------
// Crashes, replays, retries
// ---------------------------------------------------------------------------

/// A turn killed between its grant and its settle leaves a hold, and the hold
/// binds until it lapses — at which point the next call clears it.
///
/// The crash half is driven against the ledger directly, because there is no
/// honest way to kill a turn mid-flight from a test that also wants to watch
/// what the *next* turn sees. What the engine then does with the resulting hold
/// is the real subject: a leaked reservation that did not bind would let a dead
/// process's budget be spent twice, and one that never lapsed would strand it
/// forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_killed_between_grant_and_settle_leaves_a_hold_the_next_turn_expires() {
    let rig = fleeted(control_plane()).await;
    let principal = Principal::new("metered", "ada");
    let terms = terms(&rig.plane, &principal);

    // A process that died after its grant and before its settle.
    let hold_ttl_ms = 50;
    let grant = rig
        .ledger
        .open_grant(GrantRequest {
            principal: principal.clone(),
            session_id: SessionId::new("a session that died"),
            response_id: ResponseId::new("resp_orphaned"),
            requested_usd: LIMIT_USD,
            ttl_ms: hold_ttl_ms,
            terms: terms.clone(),
            now_ms: now_ms(),
        })
        .await
        .expect("the grant is well-formed");
    assert_usd(
        grant.granted_usd,
        LIMIT_USD,
        "the dead turn held the whole budget",
    );

    // While the hold stands, the next turn has nothing to spend and degrades.
    responses_turn(&rig.app, &key("metered"), "hello", "during").await;
    assert_eq!(
        decision(&rig.store, &bound("metered", "ada", "during"))
            .await
            .chosen
            .policy_identity(),
        "local/local",
        "a live hold binds exactly as committed spend does"
    );

    // Lazily, by whatever call comes next: no sweeper, no cross-session index.
    tokio::time::sleep(std::time::Duration::from_millis(hold_ttl_ms * 3)).await;
    let recovered = balance(&rig, "metered", "ada").await;
    assert_usd(
        recovered.held_usd,
        0.0,
        "the lapsed hold is gone, dropped by the first call to notice",
    );
    assert_usd(
        recovered.project_remaining_usd,
        LIMIT_USD,
        "and the whole budget is available again",
    );

    // Which the engine sees on the very next turn.
    responses_turn(&rig.app, &key("metered"), "hello", "after").await;
    assert_eq!(
        decision(&rig.store, &bound("metered", "ada", "after"))
            .await
            .chosen
            .policy_identity(),
        "anthropic/claude",
        "the control: with the hold gone the same turn reaches the hosted model"
    );
}

/// A settle lost to a crash is re-driven by the replay the next open performs.
///
/// No sweeper and no cross-session index: a session replays its own log every
/// time it is opened, so the repair rides on work that was already happening.
/// The second ledger here is a process that has never seen this session's
/// spend — which is exactly what a restart looks like from the ledger's side
/// when the settle was the thing that was lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_settle_is_repaired_by_the_next_open_of_the_same_session() {
    let first = fleeted(control_plane()).await;
    let session = bound("metered", "ada", "crash");
    create_session(&first.app, &key("metered"), &session).await;
    native_turn(&first.app, &key("metered"), &session, "t0", "hello").await;
    assert_usd(
        balance(&first, "metered", "ada").await.committed_usd,
        ACTUAL_TURN_USD,
        "the first process settled its turn",
    );

    // The same log, a ledger that never saw the settle.
    let successor = rig(
        control_plane(),
        Fixture {
            store: Some(Arc::clone(&first.store)),
            ..Default::default()
        },
    )
    .await;
    assert_usd(
        balance(&successor, "metered", "ada").await.committed_usd,
        0.0,
        "the successor starts believing nothing was ever spent",
    );

    // Re-sending the completed turn deduplicates, so nothing is routed and no
    // grant is opened — the repair is the only thing that can move this
    // number, which is what makes the assertion below unambiguous.
    native_turn(&successor.app, &key("metered"), &session, "t0", "hello").await;
    assert_usd(
        balance(&successor, "metered", "ada").await.committed_usd,
        ACTUAL_TURN_USD,
        "the replay re-drove the lost settle through the same idempotent \
         operation",
    );

    // And it is idempotent: opening the session again does not charge twice.
    native_turn(&successor.app, &key("metered"), &session, "t0", "hello").await;
    assert_usd(
        balance(&successor, "metered", "ada").await.committed_usd,
        ACTUAL_TURN_USD,
        "a settle at or below the session's watermark is a no-op",
    );
}

/// A deduplicated retry opens no second grant.
///
/// The client already paid for this answer and the accounting it was billed
/// under is durable in the log. A retry that opened a fresh grant would hold a
/// second turn's worth of budget against a turn that is not going to happen,
/// and a client retrying through a flaky connection would exhaust a project
/// without spending a cent at any provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deduplicated_retry_opens_no_second_grant() {
    let rig = fleeted(control_plane()).await;
    let session = bound("metered", "ada", "retry");
    create_session(&rig.app, &key("metered"), &session).await;
    native_turn(&rig.app, &key("metered"), &session, "t0", "hello").await;

    let after_one = balance(&rig, "metered", "ada").await;
    assert_usd(
        after_one.committed_usd,
        ACTUAL_TURN_USD,
        "the first turn really did spend something, or `unchanged` below would \
         be a comparison between two zeroes",
    );
    for _ in 0..3 {
        native_turn(&rig.app, &key("metered"), &session, "t0", "hello").await;
    }
    let after_retries = balance(&rig, "metered", "ada").await;

    assert_usd(
        after_retries.committed_usd,
        after_one.committed_usd,
        "three retries of an answered turn cost nothing",
    );
    assert_usd(
        after_retries.held_usd,
        0.0,
        "and hold nothing: a retry that reserved budget would strand it for a \
         whole TTL apiece",
    );
    assert_eq!(
        decisions(&rig.store, &session).await.len(),
        1,
        "the control: a second grant would have had to come with a second \
         routing decision, and there is only one"
    );
}

// ---------------------------------------------------------------------------
// Warnings, and the compatibility floor
// ---------------------------------------------------------------------------

/// A grant past the warn threshold says so on the decision it funded.
///
/// No notification, no side channel: the fact rides on the audit trail, which
/// is where an admin looking at "why did this project start degrading" already
/// is. `warn_at` defaults to four fifths, so the turn that crosses $0.44 of
/// $0.55 is the third.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_warnings_appear_on_the_decision_at_the_threshold() {
    let rig = fleeted(control_plane()).await;
    let session = bound("metered", "ada", "warn");
    burn_the_budget(&rig, &key("metered"), &session).await;
    native_turn(&rig.app, &key("metered"), &session, "after", "hello").await;

    let states: Vec<BudgetState> = decisions(&rig.store, &session)
        .await
        .iter()
        .map(|decision| decision.budget_state)
        .collect();
    assert_eq!(
        states,
        vec![
            BudgetState::Unconstrained,
            BudgetState::Unconstrained,
            BudgetState::Warned,
            BudgetState::Exhausted,
        ],
        "the warning arrives while there is still a fifth of the budget to act \
         on, which is the whole reason it is not simply the exhausted state \
         one turn early"
    );
}

/// Turning a budget on must not re-route anything that has no budget.
///
/// The compatibility claim of this milestone, and the one that makes enabling
/// budgets an operation rather than a migration: an unconfigured deployment and
/// a configured project with no `"budget"` must both route exactly as they did
/// before the ledger existed — and must not so much as *call* it, because an
/// unlimited budget is the absence of a ledger rather than a very large
/// ceiling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_mode_and_budgetless_projects_route_byte_identically_to_m2() {
    let counted = Arc::new(CountingLedger::default());
    let configured = rig(
        control_plane(),
        Fixture {
            ledger: Arc::clone(&counted) as Arc<dyn SpendLedger>,
            ..Default::default()
        },
    )
    .await;
    let open_counted = Arc::new(CountingLedger::default());
    let open = rig(
        ControlPlane::open(),
        Fixture {
            ledger: Arc::clone(&open_counted) as Arc<dyn SpendLedger>,
            ..Default::default()
        },
    )
    .await;

    // Open mode does not namespace, so the cache key is the session id.
    let (status, body) = post(
        &open.app,
        "/v1/responses",
        None,
        &serde_json::to_string(&request("chat", vec![user_message("hello")])).expect("encodes"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    responses_turn(&configured.app, &key("openhanded"), "hello", "chat").await;

    let unauthenticated = decision(&open.store, "chat").await;
    let budgetless = decision(&configured.store, &bound("openhanded", "ada", "chat")).await;

    assert_eq!(unauthenticated.chosen, budgetless.chosen);
    assert_eq!(unauthenticated.rationale, budgetless.rationale);
    assert_eq!(unauthenticated.considered, budgetless.considered);
    assert_eq!(
        (unauthenticated.budget_state, budgetless.budget_state),
        (BudgetState::Unconstrained, BudgetState::Unconstrained),
        "a turn taken under no budget was taken under no budget — the same \
         fact a pre-M3 log records by having no field at all"
    );
    assert!(
        !unauthenticated.rationale.contains("budget"),
        "and the rationale is the one M2 wrote, with nothing appended: {}",
        unauthenticated.rationale
    );

    assert_eq!(
        (open_counted.calls(), counted.calls()),
        (0, 0),
        "neither deployment may touch the ledger: the open-mode path is meant \
         to cost nothing at all, and `skipped` is not something equal numbers \
         could ever prove"
    );

    // The control, and it is what stops the assertion above from passing on a
    // ledger nobody wired: the same deployment, the same prompt, a key whose
    // project does have a budget.
    responses_turn(&configured.app, &key("metered"), "hello", "chat").await;
    assert!(counted.calls() > 0, "a budgeted key does reach the ledger");
}
