// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M7 of `PLAN-agentic-control-plane.md`: real frontier credentials, at the
//! seam a deployment actually runs.
//!
//! The core suite proves each piece in isolation — a secret redacts, a filter
//! filters, a payer draws. What it cannot prove is that the pieces are *wired*:
//! that the filter runs before `choose()` and therefore before `considered` is
//! written, that the credential the log names is the credential the dispatch
//! used, that a project's `"credentials"` block reaches an `Admission` at all,
//! and that a forwarded `Authorization` crosses the whole request path without
//! landing in a single event. Every claim here is about that trip.
//!
//! # Environment variables, and why they are set exactly once
//!
//! A credential resolves at *boot*, from a variable named in the file, which
//! means this suite has to have variables set. `std::env::set_var` is unsound
//! beside a concurrent `std::env::var`, and `cargo test` runs these in one
//! process on many threads — so every write happens inside one [`LazyLock`]
//! initializer, which the runtime guarantees runs once with every other caller
//! *blocked* until it finishes. Each test's first line forces it, so no test
//! can read a variable while another is writing one. Nothing here ever unsets
//! or rewrites a variable, which is what keeps that guarantee true for the life
//! of the process.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{MemorySpendLedger, Payer, SpendLedger};
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::SessionId;
use roundhouse_core::metrics::MetricsConfig;
use roundhouse_core::routing::policy::Weights;
use roundhouse_core::routing::{AffinityPolicy, DecisionRecord, ProviderPricing};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierModelSpec, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_mcp::{ControlStore, ModeNarrowing, PreferMode, TimedOverlay};
use roundhouse_server::test_support::frontier_spec;
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, Conversations, EchoLocalExecutor, Engine, EngineConfig, http,
    metrics_api, responses_api,
};

mod common;
use common::{BLOCK_SIZE, LOCAL_MODEL, embedded_fleet, path_segment};

// ---------------------------------------------------------------------------
// The secrets, and the variables that hold them
// ---------------------------------------------------------------------------

/// Every plaintext below is unique and shares no substring with a fingerprint,
/// a field name or a marker — so a scan that finds one found the real thing
/// rather than a coincidence.
const DEPLOYMENT_KEY: &str = "sk-proj-ZZZQQQ0000-deployment-pays";
const USER_KEY: &str = "sk-ant-api03-YYYWWW1111-the-member-pays";
/// A JWT, which is what a device login produces and what §3 refuses to store.
const OAUTH_TOKEN: &str = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJhZGEifQ.XXXVVV2222-never-stored";
/// The caller's own credential on a pass-through turn — forwarded in-flight,
/// never persisted.
const SEAT_BEARER: &str = "Bearer eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJib2IifQ.WWWUUU3333-seat";
const SEAT_ACCOUNT: &str = "acct-WWWUUU-3333";

const DEPLOYMENT_VAR: &str = "ROUNDHOUSE_TEST_M7_DEPLOYMENT_KEY";
const USER_VAR: &str = "ROUNDHOUSE_TEST_M7_USER_KEY";
const OAUTH_VAR: &str = "ROUNDHOUSE_TEST_M7_OAUTH_TOKEN";
/// Named by a fixture and deliberately never set. No write is needed to make
/// this true, which is what keeps the boot-failure test free of the
/// single-writer discipline the others live under.
const NEVER_SET_VAR: &str = "ROUNDHOUSE_TEST_M7_VARIABLE_NOBODY_SET";

/// The one place this process writes to its own environment. See the module
/// note.
static ENV: LazyLock<()> = LazyLock::new(|| {
    // SAFETY: this closure runs exactly once, and `LazyLock` blocks every other
    // thread inside `force` until it returns. Every read of these variables in
    // this binary is downstream of a `force`, so no read overlaps this write.
    // Nothing unsets or rewrites them afterwards.
    unsafe {
        std::env::set_var(DEPLOYMENT_VAR, DEPLOYMENT_KEY);
        std::env::set_var(USER_VAR, USER_KEY);
        std::env::set_var(OAUTH_VAR, OAUTH_TOKEN);
        std::env::remove_var(NEVER_SET_VAR);
    }
});

fn with_env() {
    LazyLock::force(&ENV);
}

// ---------------------------------------------------------------------------
// The deployment
// ---------------------------------------------------------------------------

/// Dollars per million output tokens, so a turn's price is a function of the
/// answer alone and every assertion about money is arithmetic a reader can do.
const OUTPUT_PER_MTOK_USD: f64 = 1_000_000.0;
const FRONTIER_ANSWER: &str = "frontier answer";
const LOCAL_ANSWER: &str = "local answer";
/// `FRONTIER_ANSWER` is 15 bytes and the tokenizer is one byte per token, so a
/// hosted turn costs exactly this.
const TURN_USD: f64 = 15.0;
const LIMIT_USD: f64 = 1_000.0;

/// Two hosted providers, so "this one is withheld and that one is not" is a
/// claim a single catalog can make.
///
/// [`frontier_spec`] (M15, H2): the per-provider closure this used to
/// hand-roll was the same eight-field literal H2 named ten other copies of;
/// only `provider` ever varied here, so it is the one argument left open.
fn catalog() -> StaticFrontierCatalog {
    let spec = |provider: &str| FrontierModelSpec {
        pricing: ProviderPricing {
            input_per_mtok_usd: 0.0,
            cached_input_per_mtok_usd: 0.0,
            cache_write_per_mtok_usd: 0.0,
            output_per_mtok_usd: OUTPUT_PER_MTOK_USD,
        },
        base_ttft_ms: 10.0,
        ttft_ms_per_uncached_token: 0.0,
        ..frontier_spec(provider, "flagship", WireProtocol::OpenAiResponses)
    };
    StaticFrontierCatalog::new(vec![spec("openai"), spec("anthropic")])
}

fn key(tag: &str) -> String {
    format!("rh_turn_{tag:A<43}")
}

/// This deployment's admin key — a real secret of roundhouse's own, and the
/// one a client is most likely to be holding beside a turn key.
fn admin_key() -> String {
    format!("rh_admin_{:A<43}", "ADMIN")
}

fn sha256_hex(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// One project per credential arrangement this milestone has a claim about.
///
/// Built through `ControlPlaneConfig::from_json` rather than by assembling
/// `Admission`s, for the reason every other suite here does: the file *is* the
/// format, and the pairing of a project's mode with a member's keys happens
/// inside `validate` — a fixture that bypassed it would be testing an
/// arrangement no operator could write.
fn control_plane_json() -> String {
    serde_json::json!({
        // The deployment's own keys: one provider only, so a project that falls
        // back to this tier reaches `openai` and not `anthropic`.
        "credentials": { "providers": { "openai": { "env_var": DEPLOYMENT_VAR } } },
        "projects": [
            // Falls back to the deployment's key. `anthropic` is quoted and
            // unreachable.
            { "id": "fallback", "budget": budget() },
            // The member's own key pays.
            {
                "id": "byok",
                "budget": budget(),
                "credentials": { "mode": "prefer_user" }
            },
            // The same, with the project exempting member-paid turns from its
            // own ceiling.
            {
                "id": "exempt",
                "budget": budget(),
                "credentials": { "mode": "prefer_user", "budget_counts": "project_paid_only" }
            },
            // Forwards the caller's own credential and stores nothing. It
            // carries a budget like every other metered project here, which is
            // a claim in its own right: what a dollar ceiling does over traffic
            // roundhouse is never billed for is a question an operator can
            // write and this suite therefore has to answer.
            { "id": "seat", "budget": budget(), "credentials": { "mode": "pass_through" } },
        ],
        "users": [{ "id": "ada" }, { "id": "bob" }],
        "keys": [
            { "project": "fallback", "user": "ada", "key_sha256": sha256_hex(&key("fallback")) },
            {
                "project": "byok", "user": "ada", "key_sha256": sha256_hex(&key("byok")),
                "credentials": { "providers": { "openai": { "env_var": USER_VAR } } }
            },
            {
                "project": "exempt", "user": "ada", "key_sha256": sha256_hex(&key("exempt")),
                "credentials": { "providers": { "openai": { "env_var": USER_VAR } } }
            },
            { "project": "seat", "user": "bob", "key_sha256": sha256_hex(&key("seat")) },
        ],
        // Declared so a client can present one, which is the point: an admin
        // key is a secret of roundhouse's own, and the capture must refuse it
        // for the same reason it refuses a turn key.
        "admin_keys": [sha256_hex(&admin_key())],
    })
    .to_string()
}

/// The same two projects with their credential modes exchanged.
///
/// What a successor process boots with after an operator has edited the file —
/// an ordinary edit, and the only arrangement in which the live configuration
/// and the log disagree about whether a turn was billable. Same key digests and
/// same ids, so the sessions the first process wrote resolve to the same
/// memberships here.
fn swapped_plane_json() -> String {
    serde_json::json!({
        "credentials": { "providers": { "openai": { "env_var": DEPLOYMENT_VAR } } },
        "projects": [
            { "id": "byok", "budget": budget(), "credentials": { "mode": "pass_through" } },
            { "id": "seat", "budget": budget(), "credentials": { "mode": "prefer_user" } },
        ],
        "users": [{ "id": "ada" }, { "id": "bob" }],
        "keys": [
            { "project": "byok", "user": "ada", "key_sha256": sha256_hex(&key("byok")) },
            {
                "project": "seat", "user": "bob", "key_sha256": sha256_hex(&key("seat")),
                "credentials": { "providers": { "openai": { "env_var": USER_VAR } } }
            },
        ],
    })
    .to_string()
}

fn budget() -> serde_json::Value {
    serde_json::json!({
        "limit_usd": LIMIT_USD,
        "window": "total",
        "on_exhaustion": "degrade_to_local",
    })
}

fn control_plane() -> Arc<ControlPlane> {
    with_env();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&control_plane_json(), "M7 credential fixture")
            .expect("the fixture config must validate"),
    ))
}

struct Rig {
    app: Router,
    store: Arc<MemoryStore>,
    ledger: Arc<MemorySpendLedger>,
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    /// The node-local store an agent's overlay lands in, shared with the engine
    /// exactly as the composition root shares it. Written directly here rather
    /// than through the MCP surface: what this suite has claims about is what a
    /// *narrowing* does to the credential filter, and the tool call that
    /// installs one is the subject of its own suite.
    control: Arc<ControlStore>,
}

async fn rig(plane: Arc<ControlPlane>) -> Rig {
    rig_over(plane, None).await
}

/// The same deployment, optionally over a log some other process wrote.
///
/// A successor: same sessions, fresh ledger, and a control plane an operator
/// may have edited in between. That is the one arrangement in which the engine
/// settles a turn it did not run, so it is the only place a settle that reads
/// the *live* configuration can be told apart from one that reads the log.
async fn rig_over(plane: Arc<ControlPlane>, over: Option<Arc<MemoryStore>>) -> Rig {
    ensure_rustls_crypto_provider();
    let store = over.unwrap_or_else(|| Arc::new(MemoryStore::new()));
    let ledger = Arc::new(MemorySpendLedger::new());
    let control = Arc::new(ControlStore::new());
    let engine = Arc::new(
        Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new(LOCAL_ANSWER)),
            catalog(),
            Arc::new(EchoFrontierClient::new(FRONTIER_ANSWER)),
            // Price off the routing axis and a slow local worker, so the hosted
            // model wins whenever it is *reachable*. That is what makes a local
            // decision here evidence of the credential filter rather than of
            // the scorer preferring free capacity.
            Arc::new(AffinityPolicy::new().with_weights(Weights {
                prefill: 1.0,
                cost: 0.0,
                ttft: 0.25,
            })),
            EngineConfig {
                block_size: BLOCK_SIZE,
                local_model: LOCAL_MODEL.to_string(),
                local_base_ttft_ms: 5_000.0,
                ..Default::default()
            },
        )
        .with_spend_ledger(Arc::clone(&ledger) as Arc<dyn SpendLedger>)
        .with_control_store(Arc::clone(&control))
        .with_fleet(embedded_fleet().await),
    );
    // Priced off the same catalog the engine routes against, and that is
    // load-bearing rather than tidy: with an empty rate card every hosted row
    // costs zero dollars, so a dashboard that invented a bill for a seat turn
    // and one that refused to would look identical here. The catalog's card is
    // what makes "this turn was priced" an observable claim.
    let metrics_config = Arc::new(MetricsConfig::new(catalog().shadow_pricing()));
    let app = http::router(Arc::clone(&plane), Arc::clone(&engine), Arc::clone(&store))
        .merge(metrics_api::metrics_router(
            Arc::clone(&plane),
            engine.metrics(),
            metrics_config,
        ))
        .merge(responses_api::responses_router(
            Arc::clone(&plane),
            Arc::clone(&engine),
            Arc::clone(&store),
            Arc::new(Conversations::new()),
        ));
    Rig {
        app,
        store,
        ledger,
        engine,
        control,
    }
}

// ---------------------------------------------------------------------------
// Driving it
// ---------------------------------------------------------------------------

/// How a client presents its roundhouse turn key.
#[derive(Clone, Copy)]
enum Present {
    /// The BYOK stanza: `Authorization: Bearer rh_turn_…`.
    AuthorizationOnly,
    /// The pass-through stanza: the turn key in `X-Roundhouse-Key`, and
    /// `Authorization` carrying the caller's own ChatGPT bearer.
    DedicatedHeaderWithASeat,
    /// The BYOK stanza **as PLAN §3 writes it**: one `rh_turn_…` value, sent in
    /// `env_key` and in `env_http_headers` both, so the dedicated header and
    /// `Authorization` carry the same roundhouse secret.
    BothHeaders,
    /// The dedicated header beside an `Authorization` carrying this
    /// deployment's *admin* key — the sharpest thing a client could put there,
    /// and one it has every reason to be holding.
    DedicatedHeaderWithAnAdminKey,
}

async fn post(
    app: &Router,
    uri: &str,
    secret: &str,
    present: Present,
    body: &str,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(CONTENT_TYPE, "application/json");
    builder = match present {
        Present::AuthorizationOnly => builder.header(AUTHORIZATION, format!("Bearer {secret}")),
        Present::DedicatedHeaderWithASeat => builder
            // Bare, exactly as codex's `env_http_headers` copies an environment
            // variable's value through.
            .header("x-roundhouse-key", secret)
            .header(AUTHORIZATION, SEAT_BEARER)
            .header("chatgpt-account-id", SEAT_ACCOUNT),
        Present::BothHeaders => builder
            .header("x-roundhouse-key", secret)
            .header(AUTHORIZATION, format!("Bearer {secret}")),
        Present::DedicatedHeaderWithAnAdminKey => builder
            .header("x-roundhouse-key", secret)
            .header(AUTHORIZATION, format!("Bearer {}", admin_key())),
    };
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

async fn create_session(app: &Router, secret: &str, present: Present, session_id: &str) {
    let (status, text) = post(
        app,
        "/v1/sessions",
        secret,
        present,
        &serde_json::json!({ "session_id": session_id }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "creating `{session_id}`: {text}");
}

/// One turn on the native surface, which holds the history server-side.
async fn turn(
    app: &Router,
    secret: &str,
    present: Present,
    session_id: &str,
    turn_id: &str,
) -> String {
    let body = serde_json::json!({
        "turn_id": turn_id,
        "input": [{ "role": "user", "text": "how many tokens did that turn bill?" }],
    })
    .to_string();
    let (status, text) = post(
        app,
        &format!("/v1/sessions/{}/responses", path_segment(session_id)),
        secret,
        present,
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "turn `{turn_id}`: {text}");
    assert!(
        text.contains("event: response_completed"),
        "turn `{turn_id}` did not complete: {text}"
    );
    text
}

async fn one_turn(rig: &Rig, secret: &str, present: Present, session_id: &str) -> String {
    create_session(&rig.app, secret, present, session_id).await;
    turn(&rig.app, secret, present, session_id, "t1").await
}

async fn log(store: &MemoryStore, session_id: &str) -> Vec<SessionEvent> {
    store
        .read_events(&SessionId::new(session_id), 0, 1024)
        .await
        .unwrap_or_else(|error| panic!("session `{session_id}` should exist: {error}"))
}

async fn decision(store: &MemoryStore, session_id: &str) -> DecisionRecord {
    log(store, session_id)
        .await
        .into_iter()
        .find_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .expect("the turn recorded a routing decision")
}

/// Every provider named in a decision's `considered` list.
fn considered(decision: &DecisionRecord) -> Vec<String> {
    decision
        .considered
        .iter()
        .map(|candidate| candidate.target.policy_identity())
        .collect()
}

/// Everything one session's log would show a reader, as one string.
///
/// The serialized form and the `Debug` form both, because a credential can leak
/// through either and they are produced by different code: `serde` writes the
/// events a store persists and an operator streams, and `Debug` is what a
/// `tracing` field renders.
async fn everything_the_log_shows(store: &MemoryStore, session_id: &str) -> String {
    let events = log(store, session_id).await;
    format!(
        "{}\n{:?}",
        serde_json::to_string(&events).expect("events serialize"),
        events
    )
}

// ---------------------------------------------------------------------------
// The four §9 rung tests, at the seam a deployment runs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_principal_without_a_credential_for_a_provider_never_sees_it_in_candidates() {
    let rig = rig(control_plane()).await;
    one_turn(
        &rig,
        &key("fallback"),
        Present::AuthorizationOnly,
        "fallback/ada/s1",
    )
    .await;

    // PROBE: the catalog quoted two hosted providers and this deployment holds
    // a key for one. The other must not be in `considered` -- which is the list
    // `best_frontier_alternative` prices a local turn's saving against, so a
    // provider left in for want of a credential becomes a dashboard number
    // invented out of a missing variable.
    let decision = decision(&rig.store, "fallback/ada/s1").await;
    assert_eq!(
        considered(&decision),
        vec!["local/local", "openai/flagship"],
        "`anthropic` was quoted and has no key anywhere; it must not be a candidate"
    );
    assert_eq!(
        decision.withheld_providers,
        vec!["anthropic".to_string()],
        "and the fact it was dropped exists nowhere else in the log"
    );

    // CONTROL: the turn still served, on the provider that *is* reachable. A
    // filter that emptied the pool would prove nothing about credentials.
    assert_eq!(
        decision.chosen.policy_identity(),
        "openai/flagship",
        "a reachable hosted model still wins"
    );
}

#[tokio::test]
async fn a_config_that_says_nothing_about_credentials_routes_exactly_as_it_did_before_m7() {
    // **The compatibility promise, and the sharpest regression this milestone
    // could have shipped.** A configured plane whose file mentions no
    // credentials anywhere resolves, without this rule, to a stored resolution
    // with three empty tiers -- which reaches no provider, which withholds
    // every hosted candidate. Turning M7 on would then silently re-route every
    // existing M1-M6 workload to local capacity a deployment may not have.
    with_env();
    let json = serde_json::json!({
        "projects": [{ "id": "legacy" }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "legacy", "user": "ada", "key_sha256": sha256_hex(&key("legacy")) }],
    })
    .to_string();
    let plane = Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "pre-M7 fixture").expect("must validate"),
    ));
    let ungated = rig(plane).await;
    one_turn(
        &ungated,
        &key("legacy"),
        Present::AuthorizationOnly,
        "legacy/ada/s1",
    )
    .await;

    let before = decision(&ungated.store, "legacy/ada/s1").await;
    assert_eq!(
        considered(&before),
        vec!["local/local", "openai/flagship", "anthropic/flagship"],
        "every quoted provider stays in the candidate set on a deployment that \
         has not written a credentials block"
    );
    assert!(
        before.withheld_providers.is_empty(),
        "and nothing is marked as withheld, because nothing was: {before:?}"
    );
    assert_eq!(
        before.payer,
        Payer::Deployment,
        "which is the correct reading of an un-gated deployment, not a placeholder"
    );

    // CONTROL: declaring the block *anywhere* turns the gate on, so the rule
    // above is "this file said nothing" and not "credentials never gate".
    let declared = serde_json::json!({
        "projects": [{ "id": "legacy", "credentials": { "mode": "user_only" } }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "legacy", "user": "ada", "key_sha256": sha256_hex(&key("legacy")) }],
    })
    .to_string();
    let plane = Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&declared, "declared fixture").expect("must validate"),
    ));
    let gated = rig(plane).await;
    one_turn(
        &gated,
        &key("legacy"),
        Present::AuthorizationOnly,
        "legacy/ada/s2",
    )
    .await;
    let after = decision(&gated.store, "legacy/ada/s2").await;
    assert_eq!(considered(&after), vec!["local/local"]);
    assert_eq!(
        after.withheld_providers,
        vec!["anthropic".to_string(), "openai".to_string()]
    );
}

#[tokio::test]
async fn an_oauth_shaped_credential_is_refused_with_a_reason() {
    with_env();

    // PROBE: the *value* axis. A variable named by an ordinary `"api_key"`
    // entry holds a device-login token, which is the shape an operator
    // actually produces -- `codex login` writes one into its auth file, and
    // copying it into a variable is the obvious next move.
    let json = serde_json::json!({
        "credentials": { "providers": { "openai": { "env_var": OAUTH_VAR } } },
        "projects": [{ "id": "acme" }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("acme")) }],
    })
    .to_string();
    let error = ControlPlaneConfig::from_json(&json, "oauth fixture")
        .expect_err("an OAuth-shaped value must stop the boot")
        .to_string();
    assert!(
        error.contains("oauth credentials are unsupported"),
        "{error}"
    );
    assert!(error.contains("eyJ"), "the reason names the shape: {error}");
    assert!(
        error.contains("pass-through"),
        "and the way forward, or an operator's next move is a workaround \
         nobody reviewed: {error}"
    );

    // PROBE: the *kind* axis, refused under the same words so a request that
    // names `"kind": "oauth"` and one that pastes a token read alike.
    let by_kind = serde_json::json!({
        "credentials": {
            "providers": { "openai": { "env_var": DEPLOYMENT_VAR, "kind": "chatgpt" } }
        },
        "projects": [{ "id": "acme" }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("acme")) }],
    })
    .to_string();
    let by_kind = ControlPlaneConfig::from_json(&by_kind, "oauth kind fixture")
        .expect_err("an OAuth-shaped kind must stop the boot")
        .to_string();
    assert!(
        by_kind.contains("oauth credentials are unsupported"),
        "{by_kind}"
    );

    // PROBE: a secret inlined where a variable's name belongs. Structural
    // rather than a review convention -- the key's own characters fail the
    // alphabet an environment variable name is drawn from.
    let inlined = serde_json::json!({
        "credentials": { "providers": { "openai": { "env_var": DEPLOYMENT_KEY } } },
        "projects": [{ "id": "acme" }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("acme")) }],
    })
    .to_string();
    let inlined = ControlPlaneConfig::from_json(&inlined, "inlined fixture")
        .expect_err("a pasted key must stop the boot")
        .to_string();
    assert!(inlined.contains("environment variable name"), "{inlined}");

    // CONTROL: the ordinary arrangement loads, so the three refusals above are
    // about OAuth and inlining rather than about credentials being rejected
    // generally.
    ControlPlaneConfig::from_json(&control_plane_json(), "control")
        .expect("an ordinary API key in an ordinary variable must load");
}

#[tokio::test]
async fn user_paid_spend_draws_the_project_budget_under_all_frontier_spend() {
    // PROBE: the default. The member's own key paid, and the project's ceiling
    // moved -- because a project budget is a statement about how much frontier
    // traffic this project may generate at all, not about which card it was
    // billed to.
    let drawn = committed_after_one_hosted_turn("byok", "byok").await;
    assert_eq!(drawn, TURN_USD);

    // CONTROL, and the other direction asserted rather than assumed: the same
    // turn under `project_paid_only` draws nothing. A knob that only ever
    // tightens is indistinguishable from a knob that does nothing.
    let exempt = committed_after_one_hosted_turn("exempt", "exempt").await;
    assert_eq!(
        exempt, 0.0,
        "`project_paid_only` exempts a member's own credential"
    );

    // CONTROL: the axis is *who paid*, not *which project*. A deployment-paid
    // turn draws under the default exactly as a member-paid one does, which is
    // what keeps a pre-M7 project's meter meaning the same thing after BYOK is
    // switched on.
    let deployment = committed_after_one_hosted_turn("fallback", "fallback").await;
    assert_eq!(deployment, TURN_USD);
}

/// One hosted turn on `project`, and what the project's ledger committed.
///
/// Through `Engine::settle` and a real ledger rather than a hand-built
/// `Settlement`: the claim is that `payer` survives the log and reaches the
/// draw, and a settlement assembled in the test would carry the payer the test
/// chose rather than the one the decision recorded.
async fn committed_after_one_hosted_turn(project: &str, tag: &str) -> f64 {
    let rig = rig(control_plane()).await;
    let session_id = format!("{project}/ada/s1");
    one_turn(&rig, &key(tag), Present::AuthorizationOnly, &session_id).await;

    let decision = decision(&rig.store, &session_id).await;
    assert!(
        decision.chosen.policy_identity().starts_with("openai/"),
        "this fixture's claim is about a *hosted* turn: {decision:?}"
    );

    committed(&rig, project, "ada").await
}

/// What a membership's ledger says it has spent.
async fn committed(rig: &Rig, project: &str, user: &str) -> f64 {
    rig.ledger
        .balance(roundhouse_core::control::BalanceQuery {
            principal: roundhouse_core::control::Principal::new(project, user),
            terms: roundhouse_core::control::BudgetTerms {
                budget: roundhouse_core::control::Budget {
                    limit_usd: LIMIT_USD,
                    window: roundhouse_core::control::BudgetWindow::Total,
                    on_exhaustion: roundhouse_core::control::Exhaustion::DegradeToLocal {
                        overflow_when_local_saturated: false,
                    },
                    warn_at: roundhouse_core::control::DEFAULT_WARN_AT,
                },
                allocation: roundhouse_core::control::Allocation::Pooled,
            },
            now_ms: roundhouse_core::now_ms(),
        })
        .await
        .expect("the ledger answers")
        .committed_usd
}

#[tokio::test]
async fn a_quote_never_carries_a_secret_across_a_whole_turn() {
    // The server-level half of the core test of the same name: not "a `Debug`
    // of one quote redacts" but "no secret this deployment holds appears
    // anywhere in the events a full turn produced".
    let rig = rig(control_plane()).await;
    one_turn(
        &rig,
        &key("byok"),
        Present::AuthorizationOnly,
        "byok/ada/s1",
    )
    .await;

    let shown = everything_the_log_shows(&rig.store, "byok/ada/s1").await;
    for (whose, secret) in [
        ("the member's own key", USER_KEY),
        ("the deployment's key", DEPLOYMENT_KEY),
        ("the client's roundhouse turn key", &key("byok")[..]),
    ] {
        assert!(
            !shown.contains(secret),
            "{whose} appears in the log of one turn:\n{shown}"
        );
    }

    // CONTROL: the log really did record this turn, so the assertions above are
    // about redaction rather than about an empty log. The payer is what the log
    // carries *instead* of a credential.
    let decision = decision(&rig.store, "byok/ada/s1").await;
    assert_eq!(decision.payer, Payer::User);
    assert!(shown.contains("\"payer\":\"user\""), "{shown}");
}

// ---------------------------------------------------------------------------
// Pass-through, end to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pass_through_turns_authorization_never_appears_in_any_event_or_debug_output() {
    let rig = rig(control_plane()).await;
    one_turn(
        &rig,
        &key("seat"),
        Present::DedicatedHeaderWithASeat,
        "seat/bob/s1",
    )
    .await;

    // PROBE: the caller's own credential crossed the whole request path -- the
    // header seam, the admission, the candidate filter, the decision, the quote
    // and the dispatch -- and lands in none of the events that trip produced.
    let shown = everything_the_log_shows(&rig.store, "seat/bob/s1").await;
    for (what, value) in [
        ("the forwarded bearer", SEAT_BEARER),
        // Without the "Bearer " prefix too: a client that stripped the scheme
        // before logging the token would pass a naive scan.
        ("the bare token", SEAT_BEARER.trim_start_matches("Bearer ")),
        ("the account id", SEAT_ACCOUNT),
    ] {
        assert!(
            !shown.contains(value),
            "{what} appears in the log of one pass-through turn:\n{shown}"
        );
    }

    // CONTROL, and it is what makes the scan above meaningful: the turn really
    // was routed to a hosted provider on the strength of that credential, and
    // the log says the seat paid.
    let decision = decision(&rig.store, "seat/bob/s1").await;
    assert!(
        decision.chosen.policy_identity().starts_with("openai/"),
        "the forwarded credential must have made `openai` reachable: {decision:?}"
    );
    assert_eq!(
        decision.payer,
        Payer::User,
        "the seat the client logged in with pays"
    );
    // And a `Debug` of the decision -- what a `tracing` field on a dispatch
    // renders -- is the same story.
    assert!(!format!("{decision:?}").contains("WWWUUU"), "{decision:?}");
}

#[tokio::test]
async fn a_pass_through_turn_with_no_seat_degrades_to_local_rather_than_going_anonymous() {
    // The other half of the same mode, and the one codex gets wrong on its own
    // side: with `requires_openai_auth` unset it sends an anonymous request and
    // reports nothing. Here the provider simply goes unreachable and the turn
    // serves locally with a marker saying why.
    let rig = rig(control_plane()).await;
    // The dedicated header carries the turn key and no seat credential rides
    // beside it -- a client that has not logged in.
    let (status, text) = post(
        &rig.app,
        "/v1/sessions",
        &key("seat"),
        Present::AuthorizationOnly,
        &serde_json::json!({ "session_id": "seat/bob/s2" }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    turn(
        &rig.app,
        &key("seat"),
        Present::AuthorizationOnly,
        "seat/bob/s2",
        "t1",
    )
    .await;

    let decision = decision(&rig.store, "seat/bob/s2").await;
    assert_eq!(
        decision.chosen.policy_identity(),
        "local/local",
        "no credential was presented, so no hosted provider is reachable"
    );
    assert_eq!(
        decision.withheld_providers,
        vec!["anthropic".to_string(), "openai".to_string()],
        "and the marker is the only place that is visible"
    );
    assert_eq!(
        decision.payer,
        Payer::Deployment,
        "a local dispatch is the deployment's own capacity"
    );
}

#[tokio::test]
async fn a_locally_routed_turn_under_pass_through_never_touches_the_credential() {
    // A seat *was* presented, and the turn still went local because the policy
    // said so. The claim is that the credential is not consulted on that path
    // at all -- roundhouse terminates the API rather than tunnelling it, so a
    // local turn is free and anonymous by construction.
    let json = serde_json::json!({
        "projects": [{
            "id": "seat",
            "policy": { "allow": ["local/*"] },
            "credentials": { "mode": "pass_through" }
        }],
        "users": [{ "id": "bob" }],
        "keys": [{ "project": "seat", "user": "bob", "key_sha256": sha256_hex(&key("seat")) }],
    })
    .to_string();
    with_env();
    let plane = Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "local-only pass-through fixture")
            .expect("the fixture must validate"),
    ));
    let rig = rig(plane).await;
    one_turn(
        &rig,
        &key("seat"),
        Present::DedicatedHeaderWithASeat,
        "seat/bob/s3",
    )
    .await;

    let decision = decision(&rig.store, "seat/bob/s3").await;
    assert_eq!(decision.chosen.policy_identity(), "local/local");
    assert_eq!(
        decision.payer,
        Payer::Deployment,
        "local capacity is the deployment's own, which is literally true and is \
         why a local turn needs no credential at all"
    );
    // Nothing withheld: the policy took the hosted options out before the
    // credential filter saw them, so this is a policy decision and not a
    // credential one. The distinction matters -- an operator debugging a
    // local-only project should not be told a credential is missing.
    assert!(decision.withheld_providers.is_empty(), "{decision:?}");
    let shown = everything_the_log_shows(&rig.store, "seat/bob/s3").await;
    assert!(!shown.contains("WWWUUU"), "{shown}");
}

// ---------------------------------------------------------------------------
// The boot boundary, and the fold
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unset_credential_env_var_stops_the_boot() {
    with_env();

    // PROBE: a file naming a variable this process does not have. Refused at
    // load, naming the variable -- because the alternative is a deployment that
    // starts, serves, and quietly loses a provider from every candidate set,
    // which is exactly the silent failure this milestone's auth ruling spent
    // itself on.
    let json = serde_json::json!({
        "credentials": { "providers": { "openai": { "env_var": NEVER_SET_VAR } } },
        "projects": [{ "id": "acme" }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("acme")) }],
    })
    .to_string();
    let error = ControlPlaneConfig::from_json(&json, "unset variable fixture")
        .expect_err("an unset variable must stop the boot")
        .to_string();
    assert!(error.contains(NEVER_SET_VAR), "name the variable: {error}");
    assert!(error.contains("openai"), "and the provider: {error}");

    // The same refusal from a member's own entry, so no tier is exempt.
    let on_a_key = serde_json::json!({
        "projects": [{ "id": "acme" }],
        "users": [{ "id": "ada" }],
        "keys": [{
            "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("acme")),
            "credentials": { "providers": { "openai": { "env_var": NEVER_SET_VAR } } }
        }],
    })
    .to_string();
    assert!(
        ControlPlaneConfig::from_json(&on_a_key, "unset on a key")
            .expect_err("a member's unset variable must stop the boot too")
            .to_string()
            .contains(NEVER_SET_VAR)
    );

    // CONTROL: the same file with a variable that *is* set loads. Without this
    // the assertions above would pass on a boundary that refused every
    // credential block.
    let set = serde_json::json!({
        "credentials": { "providers": { "openai": { "env_var": DEPLOYMENT_VAR } } },
        "projects": [{ "id": "acme" }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("acme")) }],
    })
    .to_string();
    ControlPlaneConfig::from_json(&set, "set variable control")
        .expect("a variable this process has must load");
}

#[tokio::test]
async fn a_key_may_not_decide_who_pays_for_its_own_turns() {
    with_env();
    // `mode` and `budget_counts` are the two axes that decide whose money a
    // turn spends. A member who could set either could spend the project's key
    // or exempt their own turns from the ceiling they are meant to draw --
    // refused rather than ignored, because a file that says one thing while the
    // deployment does another is the shape this milestone exists to remove.
    for (field, value) in [
        ("mode", serde_json::json!("project_only")),
        ("budget_counts", serde_json::json!("project_paid_only")),
    ] {
        let json = serde_json::json!({
            "projects": [{ "id": "acme" }],
            "users": [{ "id": "ada" }],
            "keys": [{
                "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("acme")),
                "credentials": { field: value }
            }],
        })
        .to_string();
        let error = match ControlPlaneConfig::from_json(&json, "member-set axis fixture") {
            Err(error) => error.to_string(),
            Ok(_) => panic!("a key setting `credentials.{field}` must stop the boot"),
        };
        assert!(error.contains(field), "the refusal names the axis: {error}");
        assert!(
            error.contains("only a project may set"),
            "and says whose decision it is: {error}"
        );
    }

    // The same refusal at the deployment tier: a top-level `"credentials"`
    // block is one set of keys, not a mode -- there is no project there for a
    // mode to be about.
    let deployment = serde_json::json!({
        "credentials": { "mode": "user_only" },
        "projects": [{ "id": "acme" }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("acme")) }],
    })
    .to_string();
    assert!(ControlPlaneConfig::from_json(&deployment, "deployment mode fixture").is_err());

    // CONTROL: a *project* setting both axes is the ordinary configuration and
    // loads, so the refusals above are about which tier wrote them.
    let by_a_project = serde_json::json!({
        "projects": [{
            "id": "acme",
            "credentials": { "mode": "project_only", "budget_counts": "project_paid_only" }
        }],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("acme")) }],
    })
    .to_string();
    ControlPlaneConfig::from_json(&by_a_project, "project axis control")
        .expect("a project may decide who pays for its own turns");
}

#[tokio::test]
async fn the_payer_stamp_survives_to_the_fold_and_the_dashboard_row() {
    let rig = rig(control_plane()).await;
    one_turn(
        &rig,
        &key("byok"),
        Present::AuthorizationOnly,
        "byok/ada/s1",
    )
    .await;

    // PROBE: a replay of the log -- the same path a successor process takes --
    // reaches the same payer. This is what makes `payer` a fact about the turn
    // rather than about the process that happened to run it: the decision is
    // read back out of the store, not held in memory.
    let replayed = decision(&rig.store, "byok/ada/s1").await;
    assert_eq!(replayed.payer, Payer::User);

    // And the dashboard row this principal's own key resolves to carries the
    // turn, which is what "reaches the fold" means operationally. The snapshot
    // a turn key gets is *already* scoped to its membership — it does not name
    // the principal, it is the principal's — so what is asserted is the
    // attribution rather than the label.
    let (status, body) = get(&rig.app, "/v1/metrics", &key("byok")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let snapshot: serde_json::Value = serde_json::from_str(&body).expect("a snapshot");
    assert_eq!(snapshot["turns"], serde_json::json!(1), "{body}");
    assert_eq!(
        snapshot["models"][0]["provider"],
        serde_json::json!("openai"),
        "the member's own key paid for a hosted turn, and it is on that row: {body}"
    );

    // CONTROL, and the point of doing this over HTTP rather than off the
    // recorder: no credential of any kind reached the dashboard on the way, and
    // the dashboard is the one surface a deployment shows to people who are not
    // its operators.
    for (whose, secret) in [
        ("the member's own key", USER_KEY),
        ("the deployment's key", DEPLOYMENT_KEY),
    ] {
        assert!(
            !body.contains(secret),
            "{whose} reached the dashboard: {body}"
        );
    }

    // CONTROL: a project with no traffic has no row to confuse this one with,
    // so the numbers above are this membership's rather than the deployment's.
    let (status, other) = get(&rig.app, "/v1/metrics", &key("fallback")).await;
    assert_eq!(status, StatusCode::OK, "{other}");
    let other: serde_json::Value = serde_json::from_str(&other).expect("a snapshot");
    assert_eq!(other["turns"], serde_json::json!(0));
}

async fn get(app: &Router, uri: &str, secret: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .expect("request"),
        )
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

#[tokio::test]
async fn a_turn_key_is_accepted_from_either_header_and_only_one_licenses_forwarding() {
    let rig = rig(control_plane()).await;

    // PROBE: the same key, presented both ways, resolves to the same
    // membership -- which is what lets one deployment serve both client
    // stanzas.
    for (label, present, session) in [
        ("Authorization", Present::AuthorizationOnly, "seat/bob/h1"),
        (
            "X-Roundhouse-Key",
            Present::DedicatedHeaderWithASeat,
            "seat/bob/h2",
        ),
    ] {
        create_session(&rig.app, &key("seat"), present, session).await;
        turn(&rig.app, &key("seat"), present, session, "t1").await;
        let decision = decision(&rig.store, session).await;
        // The difference the header makes is exactly one thing: whether
        // `Authorization` is treated as somebody else's credential. Presented
        // in `Authorization`, the key is roundhouse's own and nothing is
        // forwarded, so no hosted provider is reachable.
        let expected = match label {
            "Authorization" => "local/local",
            _ => "openai/flagship",
        };
        assert_eq!(
            decision.chosen.policy_identity(),
            expected,
            "presented in `{label}`"
        );
    }
}

#[tokio::test]
async fn roundhouses_own_turn_key_is_never_the_credential_that_gets_forwarded() {
    // The sharpest of the pass-through claims, and the reason the capture is
    // conditional on which header the key arrived in. Under the BYOK stanza a
    // client puts `rh_turn_...` in `Authorization`; a capture that took that
    // header regardless would forward roundhouse's own turn key to a frontier
    // provider on every turn of a pass-through project.
    let rig = rig(control_plane()).await;
    let engine_is_alive = Arc::strong_count(&rig.engine) > 0;
    assert!(engine_is_alive);
    one_turn(
        &rig,
        &key("seat"),
        Present::AuthorizationOnly,
        "seat/bob/s4",
    )
    .await;

    let in_authorization = decision(&rig.store, "seat/bob/s4").await;
    assert_eq!(
        in_authorization.chosen.policy_identity(),
        "local/local",
        "a turn key in `Authorization` is roundhouse's own and is not forwardable; \
         a hosted decision here would mean it had been forwarded"
    );
    let shown = everything_the_log_shows(&rig.store, "seat/bob/s4").await;
    assert!(!shown.contains(&key("seat")), "{shown}");

    // And the case the header rule alone does not cover, which is the
    // *documented* one: PLAN §3's BYOK stanza sends the same `rh_turn_…` value
    // in `env_key` and in `env_http_headers`, so the dedicated header arrives
    // beside an `Authorization` that is also roundhouse's own key. Gated on the
    // header alone, that request forwards it. Gated on the value too, there is
    // nothing to forward, `openai` is unreachable, and the turn degrades to
    // local exactly as a pass-through turn with no seat does.
    for (label, present, session) in [
        (
            "the PLAN's own BYOK stanza",
            Present::BothHeaders,
            "seat/bob/s5",
        ),
        (
            "an admin key beside the turn key",
            Present::DedicatedHeaderWithAnAdminKey,
            "seat/bob/s6",
        ),
    ] {
        one_turn(&rig, &key("seat"), present, session).await;
        let routed = decision(&rig.store, session).await;
        assert_eq!(
            routed.chosen.policy_identity(),
            "local/local",
            "{label}: a hosted decision here means one of roundhouse's own \
             secrets was captured and forwarded upstream"
        );
        let shown = everything_the_log_shows(&rig.store, session).await;
        assert!(!shown.contains(&key("seat")), "{label}: {shown}");
        assert!(!shown.contains(&admin_key()), "{label}: {shown}");
    }
}

// ---------------------------------------------------------------------------
// What the dashboard may put a price on
// ---------------------------------------------------------------------------

/// The accounting-honesty rule, at the surface that publishes the number.
///
/// The ledger has kept it since M3 — a forwarded seat is `AccountedNotBilled`
/// and draws nothing — and the metrics path is the half nobody wired: it prices
/// every hosted row off the rate card, whoever paid. So a pass-through
/// deployment reads a dollar figure for traffic it was never billed for, on the
/// one surface it shows to people who are not its operators.
#[tokio::test]
async fn the_dashboard_never_prices_a_seat_forwarded_turn() {
    let rig = rig(control_plane()).await;
    one_turn(
        &rig,
        &key("seat"),
        Present::DedicatedHeaderWithASeat,
        "seat/bob/m1",
    )
    .await;

    let (status, body) = get(&rig.app, "/v1/metrics", &key("seat")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc: serde_json::Value = serde_json::from_str(&body).expect("a snapshot");

    // CONTROL first, because every assertion below is worthless if the turn did
    // not actually go to a hosted provider on the strength of the forwarded
    // seat: a local turn is free on every code path there is.
    assert_eq!(
        doc["models"][0]["provider"],
        serde_json::json!("openai"),
        "the seat made a hosted provider reachable: {body}"
    );
    let tokens = doc["tokens"]["total"].as_u64().expect("a token total");
    assert!(tokens > 0, "the turn measured tokens: {body}");

    // PROBE: those tokens are real and the dollars are not. Roundhouse holds no
    // rate card for a subscription seat, so the catalog's per-token price
    // describes what *it* would have paid on its own key — a counterfactual,
    // not a bill.
    assert_eq!(
        doc["savings"]["frontier_spend_usd"]
            .as_f64()
            .expect("a spend figure"),
        0.0,
        "a seat-forwarded turn was priced at the rate card: {body}"
    );
    assert_eq!(
        doc["models"][0]["billed_usd"]
            .as_f64()
            .expect("a row price"),
        0.0,
        "and the row it merges out of says the same: {body}"
    );

    // And the tokens are published as what they are: a count with no price
    // beside it. Reporting them nowhere would leave a pass-through deployment
    // unable to see the traffic it is carrying, which is the other half of
    // being honest about a seat.
    assert_eq!(
        doc["seat_tokens"]["total"].as_u64().expect("a seat total"),
        tokens,
        "every token of this turn was a seat's: {body}"
    );
    assert_eq!(
        doc["models"][0]["seat_tokens"]["total"]
            .as_u64()
            .expect("a row seat total"),
        tokens,
        "and the row carries its own share: {body}"
    );
}

/// The control for the test above: an ordinary keyed turn still prices.
///
/// Without it, "the dashboard reports no dollars" is satisfied by a dashboard
/// that reports no dollars at all — which is the regression the honesty rule
/// would otherwise invite.
#[tokio::test]
async fn a_turn_on_a_key_this_deployment_holds_still_prices_at_the_rate_card() {
    let rig = rig(control_plane()).await;
    one_turn(
        &rig,
        &key("byok"),
        Present::AuthorizationOnly,
        "byok/ada/m1",
    )
    .await;

    let (status, body) = get(&rig.app, "/v1/metrics", &key("byok")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc: serde_json::Value = serde_json::from_str(&body).expect("a snapshot");
    assert_eq!(
        doc["savings"]["frontier_spend_usd"]
            .as_f64()
            .expect("a spend figure"),
        TURN_USD,
        "a member's own API key is a rate card roundhouse can read: {body}"
    );
    assert_eq!(
        doc["seat_tokens"]["total"].as_u64().expect("a seat total"),
        0,
        "and nothing here was a seat's: {body}"
    );
}

/// A dollar budget on a pass-through project meters nothing, however much
/// traffic it serves.
///
/// Not a bug being fixed — the ledger is right to refuse a price it never paid
/// — but the semantics an operator gets from writing `"budget"` beside
/// `"mode": "pass_through"`, pinned so that "this setting quietly means
/// something else" is a claim with a test behind it rather than a paragraph.
/// The ceiling still bounds each turn (see `Engine::open_grant`); what it never
/// does is accumulate, so its exhaustion arm and its warn threshold cannot
/// fire.
#[tokio::test]
async fn a_dollar_budget_over_a_forwarded_seat_never_commits() {
    let rig = rig(control_plane()).await;
    create_session(
        &rig.app,
        &key("seat"),
        Present::DedicatedHeaderWithASeat,
        "seat/bob/b1",
    )
    .await;
    for turn_id in ["t1", "t2", "t3"] {
        turn(
            &rig.app,
            &key("seat"),
            Present::DedicatedHeaderWithASeat,
            "seat/bob/b1",
            turn_id,
        )
        .await;
    }
    assert_eq!(
        decision(&rig.store, "seat/bob/b1")
            .await
            .chosen
            .policy_identity(),
        "openai/flagship",
        "three hosted turns, or this says nothing about hosted traffic"
    );
    assert_eq!(
        committed(&rig, "seat", "bob").await,
        0.0,
        "three turns that would have cost ${} each on a key of ours, and the \
         project's meter has not moved -- because none of it was ours to bill",
        TURN_USD * 3.0
    );

    // CONTROL: the same budget on a project that pays with a stored key does
    // accumulate, so the flatline above is the credential mode and not a ledger
    // that has stopped counting.
    assert_eq!(
        committed_after_one_hosted_turn("byok", "byok").await,
        TURN_USD
    );
}

/// The ledger and the dashboard read one recorded fact, so they cannot
/// disagree about whether a turn was billed.
///
/// Two spellings of the billed/accounted rule is one spelling too many. The
/// engine's settle asked the *live* admission whether the project forwards,
/// while the dashboard asked nothing at all and priced every hosted row — so
/// the two answers were free to differ, and an operator editing one line of the
/// control plane made them differ. Both now read the decision the log recorded.
///
/// Driven through the repair seam because that is the only moment the engine
/// settles a turn it did not run: same log, fresh ledger, a control plane
/// edited in between. Re-sending the completed turn deduplicates, so nothing is
/// routed and the repair is the only thing that can move the number.
#[tokio::test]
async fn a_settle_and_the_dashboard_price_the_same_turn_the_same_way() {
    with_env();
    let swapped = || {
        Arc::new(ControlPlane::configured(
            ControlPlaneConfig::from_json(&swapped_plane_json(), "swapped-mode fixture")
                .expect("the successor's file must validate"),
        ))
    };

    // ---- a turn billed on a stored key, repaired after the project was
    // ---- switched to pass-through.
    let first = rig(control_plane()).await;
    one_turn(
        &first,
        &key("byok"),
        Present::AuthorizationOnly,
        "byok/ada/s9",
    )
    .await;
    assert_eq!(
        committed(&first, "byok", "ada").await,
        TURN_USD,
        "the live settle billed the member's own key"
    );

    let successor = rig_over(swapped(), Some(Arc::clone(&first.store))).await;
    assert_eq!(
        committed(&successor, "byok", "ada").await,
        0.0,
        "the successor starts believing nothing was ever spent"
    );
    turn(
        &successor.app,
        &key("byok"),
        Present::AuthorizationOnly,
        "byok/ada/s9",
        "t1",
    )
    .await;
    let (status, body) = get(&successor.app, "/v1/metrics", &key("byok")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc: serde_json::Value = serde_json::from_str(&body).expect("a snapshot");
    let dashboard = doc["savings"]["frontier_spend_usd"]
        .as_f64()
        .expect("a spend figure");
    assert_eq!(
        committed(&successor, "byok", "ada").await,
        dashboard,
        "the repaired settle and the dashboard read the same turn: {body}"
    );
    assert_eq!(
        dashboard, TURN_USD,
        "and the fact they read is the one the log recorded, not the mode the \
         project happens to carry today: {body}"
    );

    // ---- and the mirror: a seat turn repaired after the project was switched
    // ---- to a stored key must stay accounted-and-not-billed.
    let first = rig(control_plane()).await;
    one_turn(
        &first,
        &key("seat"),
        Present::DedicatedHeaderWithASeat,
        "seat/bob/s9",
    )
    .await;
    assert_eq!(
        committed(&first, "seat", "bob").await,
        0.0,
        "a forwarded seat draws nothing, which is the rule this mirror is about"
    );

    let successor = rig_over(swapped(), Some(Arc::clone(&first.store))).await;
    turn(
        &successor.app,
        &key("seat"),
        Present::AuthorizationOnly,
        "seat/bob/s9",
        "t1",
    )
    .await;
    let (status, body) = get(&successor.app, "/v1/metrics", &key("seat")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc: serde_json::Value = serde_json::from_str(&body).expect("a snapshot");
    let dashboard = doc["savings"]["frontier_spend_usd"]
        .as_f64()
        .expect("a spend figure");
    assert_eq!(
        committed(&successor, "seat", "bob").await,
        dashboard,
        "the two agree in this direction too: {body}"
    );
    assert_eq!(
        dashboard, 0.0,
        "a seat roundhouse held no card for stays unpriced however the project \
         is configured afterwards: {body}"
    );
}

/// A narrowing installed mid-session can empty the credential-reachable set on
/// a deployment whose boot check passed.
///
/// The boot check asks its question of a *project's* policy, which is the only
/// policy that exists before any session does. An overlay is a second
/// narrowing, composed a turn at a time, and it can take the local half of the
/// pool away from a pass-through session whose caller has not presented a seat
/// — leaving the credential filter with nothing to keep.
#[tokio::test]
async fn an_overlay_can_narrow_a_session_onto_providers_it_holds_no_credential_for() {
    let narrowed = rig(control_plane()).await;
    let session_id = "seat/bob/o1";
    // No seat presented: the turn key arrives in `Authorization`, so nothing is
    // forwardable and every hosted provider is unreachable this turn.
    create_session(
        &narrowed.app,
        &key("seat"),
        Present::AuthorizationOnly,
        session_id,
    )
    .await;

    // The agent asks for frontier. Honorable against the ceiling — the project's
    // policy allows both hosted models — and it is what takes local out of the
    // pool before the credential filter runs.
    narrowed.control.set_mode_axis(
        &SessionId::new(session_id),
        Some(TimedOverlay {
            ask: ModeNarrowing {
                mode: PreferMode::Frontier,
                allow: Some(
                    roundhouse_core::control::TargetFilter::parse([
                        "openai/flagship",
                        "anthropic/flagship",
                    ])
                    .expect("two ordinary patterns"),
                ),
            },
            remaining_turns: None,
            reason: "the agent asked for a hosted model".into(),
        }),
        roundhouse_core::now_ms(),
    );

    let body = serde_json::json!({
        "turn_id": "t1",
        "input": [{ "role": "user", "text": "how many tokens did that turn bill?" }],
    })
    .to_string();
    let (status, text) = post(
        &narrowed.app,
        &format!("/v1/sessions/{}/responses", path_segment(session_id)),
        &key("seat"),
        Present::AuthorizationOnly,
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    // PROBE: the branch the engine used to describe as unreachable after boot.
    // Nothing went missing here — the deployment's configuration is exactly
    // what it booted with, and the narrowing that emptied the pool was composed
    // this turn.
    assert!(
        text.contains("event: response_incomplete"),
        "an overlay onto unreachable providers must terminate the turn rather \
         than dispatch it anonymously: {text}"
    );
    assert!(
        text.contains("no credential resolved for provider"),
        "and the reason names the credential axis, not the policy: {text}"
    );

    // CONTROL: the same overlay on the same project *with* a seat presented
    // serves, so the failure above is the missing credential and not the
    // narrowing itself.
    let served = rig(control_plane()).await;
    let session_id = "seat/bob/o2";
    create_session(
        &served.app,
        &key("seat"),
        Present::DedicatedHeaderWithASeat,
        session_id,
    )
    .await;
    served.control.set_mode_axis(
        &SessionId::new(session_id),
        Some(TimedOverlay {
            ask: ModeNarrowing {
                mode: PreferMode::Frontier,
                allow: Some(
                    roundhouse_core::control::TargetFilter::parse([
                        "openai/flagship",
                        "anthropic/flagship",
                    ])
                    .expect("two ordinary patterns"),
                ),
            },
            remaining_turns: None,
            reason: "the agent asked for a hosted model".into(),
        }),
        roundhouse_core::now_ms(),
    );
    turn(
        &served.app,
        &key("seat"),
        Present::DedicatedHeaderWithASeat,
        session_id,
        "t1",
    )
    .await;
    assert_eq!(
        decision(&served.store, session_id)
            .await
            .chosen
            .policy_identity(),
        "openai/flagship"
    );
}
