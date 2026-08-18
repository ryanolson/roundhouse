// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M1 of `PLAN-agentic-control-plane.md`: identity in the log, attribution in
//! the fold, and the gate in front of both.
//!
//! Every test here drives a real router. The claims are about what a *second*
//! tenant can see and be charged for, and none of them can be made against a
//! unit-tested function: the collision this milestone closes lives in the seam
//! between a client's chosen cache key, the session id it binds to, and the
//! principal that resolved from the key in the header.
//!
//! Two of these deserve their reasons stated up front. The tenancy collision
//! (`two_principals_using_one_cache_key_do_not_share_a_session`) was written
//! and run against the un-namespaced surface first — it failed by reporting
//! that `acme/ada/main` did not exist, because both tenants' turns had landed
//! in a session called `main` — so the fix is aimed at a demonstrated defect
//! rather than at a described one. And `open_mode_serves_exactly_as_before` is
//! the regression guard for the whole milestone: an unconfigured deployment
//! must not acquire auth, a namespace, or a changed session id, because every
//! other suite in this crate is an Open-mode deployment.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use codex_api::{
    ApiError, AuthProvider, ResponseEvent, ResponsesApiRequest, ResponsesClient, ResponsesOptions,
};
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{Principal, PrincipalKey};
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::SessionId;
use roundhouse_core::item::Item;
use roundhouse_core::metrics::{MetricsConfig, MetricsRecorder, ReferenceModel, ShadowPricing};
use roundhouse_core::routing::{AffinityPolicy, ProviderPricing};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::EchoFrontierClient;
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, EchoLocalExecutor, Engine, EngineConfig, http, metrics_api,
    responses_api,
};

mod common;
use common::codex::{NoAuth, RouterTransport, StaticToken, collect, request, user_message};
use common::{frontier_catalog, path_segment};

/// What the echo provider answers with, and therefore what every turn here
/// contains.
const ANSWER: &str = "frontier answer";

// ---------------------------------------------------------------------------
// Keys and the control plane they are declared in
// ---------------------------------------------------------------------------

/// A well-shaped secret with `tag` legible inside it.
///
/// Padded to the 43 base62 characters `rh_(turn|admin)_` requires rather than
/// written out, because a hand-counted 43-character literal is a fixture that
/// fails as `malformed_key` for a reason no assertion names.
fn secret(kind: &str, tag: &str) -> String {
    format!("rh_{kind}_{tag:A<43}")
}

fn acme_key() -> String {
    secret("turn", "acme")
}

fn globex_key() -> String {
    secret("turn", "globex")
}

fn admin_key() -> String {
    secret("admin", "root")
}

fn unknown_key() -> String {
    secret("turn", "nobody")
}

fn sha256_hex(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn acme() -> Principal {
    Principal::new("acme", "ada")
}

fn globex() -> Principal {
    Principal::new("globex", "bob")
}

/// Two memberships and one admin, declared the way a deployment declares them.
///
/// Built through `ControlPlaneConfig::from_json` rather than by constructing
/// the lookup tables directly: the config file *is* the format, and a fixture
/// that skipped it would leave the validate boundary untested by every test in
/// this file. The hashes are computed from the secrets above rather than
/// transcribed, so a fixture cannot drift into authenticating nothing while
/// still parsing.
fn configured() -> Arc<ControlPlane> {
    let json = serde_json::json!({
        "projects": [{ "id": "acme", "name": "Acme Corp" }, { "id": "globex" }],
        "users": [{ "id": "ada" }, { "id": "bob" }],
        "keys": [
            { "project": "acme", "user": "ada", "key_sha256": sha256_hex(&acme_key()) },
            { "project": "globex", "user": "bob", "key_sha256": sha256_hex(&globex_key()) },
        ],
        "admin_keys": [sha256_hex(&admin_key())],
    })
    .to_string();
    let config = ControlPlaneConfig::from_json(&json, "tenancy fixture")
        .expect("the fixture config must validate");
    Arc::new(ControlPlane::configured(config))
}

// ---------------------------------------------------------------------------
// The deployment under test
// ---------------------------------------------------------------------------

/// All three surfaces over one engine, one store and one control plane —
/// the same composition `main::serve` performs.
///
/// Merged rather than tested one router at a time because the claims here span
/// them: a session created on the native surface must be refused to the wrong
/// tenant on the responses surface, and the metrics surface must report on
/// both.
struct Deployment {
    app: Router,
    store: Arc<MemoryStore>,
    metrics: Arc<MetricsRecorder>,
    metrics_config: Arc<MetricsConfig>,
}

fn metrics_config() -> Arc<MetricsConfig> {
    Arc::new(MetricsConfig::new(ShadowPricing::new(vec![
        ReferenceModel {
            provider: "anthropic".into(),
            model: "claude".into(),
            pricing: ProviderPricing {
                input_per_mtok_usd: 3.0,
                cached_input_per_mtok_usd: 0.3,
                cache_write_per_mtok_usd: 3.75,
                output_per_mtok_usd: 15.0,
            },
            quality_prior: 0.95,
        },
    ])))
}

/// The two-tenant configured deployment every gated test below starts from.
///
/// `ensure_rustls_crypto_provider` belongs here rather than at the top of each
/// test: Codex's client installs a rustls provider on first use, and the one
/// test that forgot the call would fail for a reason with nothing to do with
/// tenancy.
fn two_tenants() -> Deployment {
    ensure_rustls_crypto_provider();
    deployment(configured())
}

fn deployment(plane: Arc<ControlPlane>) -> Deployment {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new(ANSWER)),
        Arc::new(AffinityPolicy::new()),
        EngineConfig::default(),
    ));
    let metrics = engine.metrics();
    let metrics_config = metrics_config();
    let app = http::router(Arc::clone(&plane), Arc::clone(&engine), Arc::clone(&store))
        .merge(metrics_api::metrics_router(
            Arc::clone(&plane),
            Arc::clone(&metrics),
            Arc::clone(&metrics_config),
        ))
        .merge(responses_api::responses_router(
            plane,
            engine,
            Arc::clone(&store),
        ));
    Deployment {
        app,
        store,
        metrics,
        metrics_config,
    }
}

// ---------------------------------------------------------------------------
// Driving it
// ---------------------------------------------------------------------------

/// Drive one turn through Codex's own client, authenticated however the caller
/// says.
///
/// The auth provider is a parameter because that *is* the variable under test
/// on this surface: the same request with a different bearer must reach a
/// different session, and with no bearer at all must be refused.
async fn turn(
    app: &Router,
    auth: Arc<dyn AuthProvider>,
    request: ResponsesApiRequest,
) -> Result<Vec<ResponseEvent>, ApiError> {
    let client = ResponsesClient::new(
        RouterTransport { app: app.clone() },
        common::codex::provider("http://roundhouse.test/v1", "roundhouse-tenancy"),
        auth,
    );
    collect(
        client
            .stream_request(request, ResponsesOptions::default())
            .await?,
    )
    .await
}

fn bearer(key: &str) -> Arc<dyn AuthProvider> {
    Arc::new(StaticToken::new(key))
}

/// The answer as the client assembled it from deltas.
fn answer(events: &[ResponseEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputTextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

fn response_id(events: &[ResponseEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            ResponseEvent::Completed { response_id, .. } => Some(response_id.clone()),
            _ => None,
        })
        .expect("a completed turn names its response")
}

/// One session's whole log, straight out of the store.
async fn log(store: &MemoryStore, session_id: &str) -> Vec<SessionEvent> {
    store
        .read_events(&SessionId::new(session_id), 0, 1024)
        .await
        .unwrap_or_else(|error| panic!("session `{session_id}` should exist: {error}"))
}

/// The items one session committed.
async fn items(store: &MemoryStore, session_id: &str) -> Vec<Item> {
    log(store, session_id)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ItemAppended { item } => Some(item),
            _ => None,
        })
        .collect()
}

/// The principal a session's log names, and `None` for a log that names none.
///
/// Reads the log rather than the fold: the fold's answer is downstream of this
/// one, and a test that checked only the fold could not tell "attributed
/// correctly" from "attributed nowhere, and the fold guessed".
async fn logged_principal(store: &MemoryStore, session_id: &str) -> Option<Principal> {
    log(store, session_id)
        .await
        .into_iter()
        .find_map(|event| match event.kind {
            SessionEventKind::SessionCreated { principal, .. } => Some(principal),
            _ => None,
        })
        .expect("the session's log must open with session_created")
}

async fn no_such_session(store: &MemoryStore, session_id: &str) -> bool {
    store.last_seq(&SessionId::new(session_id)).await.is_err()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// An unconfigured deployment gains nothing from this milestone but the log
/// entry.
///
/// Every other suite in this crate is an Open-mode deployment, so this is the
/// test that says what "byte-for-byte preserved" means: no key is required, the
/// session id is the client's cache key verbatim with no namespace in front of
/// it, and the identity the log now carries is the single built-in membership
/// rather than an absence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_mode_serves_exactly_as_before() {
    ensure_rustls_crypto_provider();
    let rig = deployment(ControlPlane::open());

    let events = turn(
        &rig.app,
        Arc::new(NoAuth),
        request("main", vec![user_message("hello")]),
    )
    .await
    .expect("an unconfigured deployment must serve a turn with no credential");
    assert_eq!(answer(&events), ANSWER);

    // The cache key, verbatim. A namespace here would strand every session an
    // existing deployment already has.
    let items = items(&rig.store, "main").await;
    assert!(
        items.iter().any(|item| *item == Item::user_text("hello")),
        "the bare cache key must still be the session id: {items:#?}"
    );

    assert_eq!(
        logged_principal(&rig.store, "main").await,
        Some(Principal::default_open()),
        "an open deployment attributes to the one built-in membership, and says so \
         in the log rather than leaving the field absent — absent means a log older \
         than tenancy, which this is not"
    );

    // And it is the *first* thing in the log, which is what makes attribution
    // knowable to a fold before any event that could spend money.
    let first = log(&rig.store, "main")
        .await
        .into_iter()
        .next()
        .expect("a served session has a log");
    assert_eq!(first.seq, 1);
    assert!(
        matches!(first.kind, SessionEventKind::SessionCreated { .. }),
        "session_created must be seq 1: {first:?}"
    );
}

// ---------------------------------------------------------------------------
// Raw requests, for the answers a client library would swallow
// ---------------------------------------------------------------------------

/// One request, with an optional bearer, decoded as status plus JSON body.
///
/// Driven directly rather than through Codex's client for every refusal: the
/// client reports a non-2xx as a transport error, and these assertions are
/// about *which* refusal — `unknown_key` and `wrong_key_kind` are different
/// bugs on the operator's side and must be different codes on the wire.
async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    key: Option<&str>,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        builder = builder.header(AUTHORIZATION, format!("Bearer {key}"));
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
    let payload = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, payload)
}

/// The status of a request whose body is a stream, without reading it.
///
/// `/v1/sessions/{id}/events` never ends by itself — that is the point of the
/// endpoint — so the collecting helper above would hang on a successful one.
/// The status is the whole of what this file asks of that route anyway.
async fn status_of(app: &Router, method: &str, uri: &str, key: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = key {
        builder = builder.header(AUTHORIZATION, format!("Bearer {key}"));
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("call")
        .status()
}

/// A well-formed turn request, as bytes, for the paths that must be refused
/// before the body is even read.
fn turn_body(cache_key: &str, text: &str) -> String {
    serde_json::to_string(&request(cache_key, vec![user_message(text)])).expect("encodes")
}

// ---------------------------------------------------------------------------
// Tenancy
// ---------------------------------------------------------------------------

/// The collision this milestone exists to close.
///
/// `prompt_cache_key` is chosen by the client, and nothing stops two clients
/// choosing `main`. Before namespacing, both landed on the session called
/// `main`: one log, one lease, one warm prefix — so one tenant's conversation
/// would arrive inside the other's prompt, and an identical first turn would be
/// *deduplicated* onto the other tenant's response, handing back an answer it
/// never paid for. This test was run against the un-namespaced surface first
/// and failed at the `acme/ada/main` probe, which is what makes the fix aimed
/// at a demonstrated defect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_principals_using_one_cache_key_do_not_share_a_session() {
    let rig = two_tenants();

    let mine = turn(
        &rig.app,
        bearer(&acme_key()),
        request("main", vec![user_message("acme's secret question")]),
    )
    .await
    .expect("acme's turn completes");
    let theirs = turn(
        &rig.app,
        bearer(&globex_key()),
        request("main", vec![user_message("globex's own question")]),
    )
    .await
    .expect("globex's turn completes");

    // Two sessions, each named for its owner.
    let mine_items = items(&rig.store, "acme/ada/main").await;
    let theirs_items = items(&rig.store, "globex/bob/main").await;
    assert!(
        mine_items
            .iter()
            .any(|item| *item == Item::user_text("acme's secret question")),
        "acme's turn must land in acme's own session: {mine_items:#?}"
    );
    assert!(
        theirs_items
            .iter()
            .any(|item| *item == Item::user_text("globex's own question")),
        "globex's turn must land in globex's own session: {theirs_items:#?}"
    );

    // Neither log contains a trace of the other. Rendered rather than compared
    // item-by-item, so a leak in any field of any item is caught, not only one
    // that happens to be an exact `Item::user_text`.
    let mine_text: String = mine_items
        .iter()
        .map(|item| item.content.render())
        .collect();
    let theirs_text: String = theirs_items
        .iter()
        .map(|item| item.content.render())
        .collect();
    assert!(
        !mine_text.contains("globex"),
        "globex's words reached acme's log: {mine_text}"
    );
    assert!(
        !theirs_text.contains("secret"),
        "acme's words reached globex's log: {theirs_text}"
    );

    // Nothing was ever written under the bare cache key, which is the id both
    // tenants would have shared.
    assert!(
        no_such_session(&rig.store, "main").await,
        "a configured deployment must never bind the bare cache key"
    );

    // Each log names its own payer, which is what the fold will read.
    assert_eq!(
        logged_principal(&rig.store, "acme/ada/main").await,
        Some(acme())
    );
    assert_eq!(
        logged_principal(&rig.store, "globex/bob/main").await,
        Some(globex())
    );

    // And neither turn was answered with the other's response — the shape the
    // collision took when both tenants asked the *same* question.
    assert_ne!(response_id(&mine), response_id(&theirs));
    assert_eq!(answer(&mine), ANSWER);
    assert_eq!(answer(&theirs), ANSWER);
}

/// A turn is billed to the membership whose key admitted it — and the bills
/// add up.
///
/// The second half is the one that matters over time. Two accumulators fed
/// from one fold can only be trusted if somebody checks they still agree, and
/// the check has to be against the deployment-wide number the dashboard has
/// always reported: a per-tenant bill that quietly stopped summing to it would
/// be wrong in whichever direction nobody was looking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_is_attributed_to_the_principal_that_paid_for_it() {
    let rig = two_tenants();

    turn(
        &rig.app,
        bearer(&acme_key()),
        request("work", vec![user_message("short")]),
    )
    .await
    .expect("acme's turn completes");
    turn(
        &rig.app,
        bearer(&globex_key()),
        // Deliberately longer, so the two rows are distinguishable by tokens
        // and a fold that attributed both to one principal cannot pass.
        request(
            "work",
            vec![user_message(
                "a considerably longer question, so the input token counts differ",
            )],
        ),
    )
    .await
    .expect("globex's turn completes");

    let (status, mine) = send(&rig.app, "GET", "/v1/metrics", Some(&acme_key()), "").await;
    assert_eq!(status, StatusCode::OK);
    let (_, theirs) = send(&rig.app, "GET", "/v1/metrics", Some(&globex_key()), "").await;
    let (_, all) = send(&rig.app, "GET", "/v1/metrics", Some(&admin_key()), "").await;

    assert_eq!(mine["calls"], 1, "one turn, one call: {mine}");
    assert_eq!(theirs["calls"], 1);
    assert_eq!(all["calls"], 2);
    assert_eq!(mine["sessions"], 1);
    assert_eq!(all["sessions"], 2);

    let tokens = |doc: &serde_json::Value| doc["tokens"]["input"].as_u64().expect("input tokens");
    assert!(
        tokens(&mine) > 0,
        "acme's own tokens must be counted: {mine}"
    );
    assert_ne!(
        tokens(&mine),
        tokens(&theirs),
        "the fixture must make the two rows distinguishable, or this test could \
         pass with both turns folded into one principal"
    );

    // The anti-drift assertion.
    assert_eq!(
        tokens(&mine) + tokens(&theirs),
        tokens(&all),
        "the per-principal folds must sum to the deployment fold"
    );
    assert_eq!(
        mine["tokens"]["output"].as_u64().unwrap() + theirs["tokens"]["output"].as_u64().unwrap(),
        all["tokens"]["output"].as_u64().unwrap()
    );
    // And so must the money — to within the last bit of an f64, which is the
    // strongest honest claim available here. The token counts above are
    // integers and are asserted exactly; dollars are a sum of products over
    // rows, and the deployment fold adds them in a different order from this
    // test, so `a + b == c` can fail by one ulp with nothing wrong. A tolerance
    // this tight (1e-12 relative) still catches a whole call attributed to the
    // wrong row, which is the drift under test.
    let spend = |doc: &serde_json::Value| {
        doc["savings"]["frontier_spend_usd"]
            .as_f64()
            .expect("frontier spend")
    };
    let deployment_spend = spend(&all);
    assert!(deployment_spend > 0.0, "the fixture must spend something");
    assert!(
        ((spend(&mine) + spend(&theirs)) - deployment_spend).abs() <= deployment_spend * 1e-12,
        "the per-principal spend must sum to the deployment's: {} + {} != {}",
        spend(&mine),
        spend(&theirs),
        deployment_spend
    );
}

/// Every row of the error table, on the surface that serves turns — and none
/// of them reaches the store.
///
/// The ordering claim is the load-bearing one: a key is resolved before the
/// body is parsed, so an unauthenticated request cannot name a session, cannot
/// create one, and costs this process a hash lookup rather than a store round
/// trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_key_is_refused_before_a_session_is_created() {
    let rig = two_tenants();
    let body = turn_body("main", "hello");

    for (key, status, code) in [
        (None, StatusCode::UNAUTHORIZED, "missing_key"),
        (
            Some("not-a-roundhouse-key"),
            StatusCode::UNAUTHORIZED,
            "malformed_key",
        ),
        (
            Some(unknown_key().as_str()),
            StatusCode::UNAUTHORIZED,
            "unknown_key",
        ),
    ] {
        let (got, payload) = send(&rig.app, "POST", "/v1/responses", key, &body).await;
        assert_eq!(got, status, "for key {key:?}: {payload}");
        assert_eq!(payload["error"]["code"], code, "for key {key:?}");
    }

    // Not under the namespace it would have used, and not under the bare cache
    // key either: nothing was created at all.
    assert!(no_such_session(&rig.store, "acme/ada/main").await);
    assert!(no_such_session(&rig.store, "main").await);
}

/// The native surface takes a session id as input and streams the raw log for
/// it, so the namespace check *is* its authorization.
///
/// The hole this closes was made worse by namespacing, not created by it:
/// before, a session id was an unguessable cache key; now it is
/// `{project}/{user}/{whatever the client called its conversation}`, which is
/// eminently guessable. A refusal that leaked existence would be an oracle
/// over other tenants' sessions, which is why the refusal is the same whether
/// the session exists or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_surface_session_outside_the_callers_namespace_is_refused() {
    let rig = two_tenants();

    // Globex creates one of its own, so the id acme reaches for below really
    // exists — a refusal that only worked on absent sessions would prove
    // nothing.
    let (status, _) = send(
        &rig.app,
        "POST",
        "/v1/sessions",
        Some(&globex_key()),
        r#"{"session_id":"globex/bob/private"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let theirs = path_segment("globex/bob/private");
    for (uri, method, body) in [
        (
            "/v1/sessions".to_string(),
            "POST",
            r#"{"session_id":"globex/bob/private"}"#,
        ),
        (
            format!("/v1/sessions/{theirs}/responses"),
            "POST",
            r#"{"turn_id":"t1","input":[{"role":"user","text":"hello"}]}"#,
        ),
        (format!("/v1/sessions/{theirs}/events"), "GET", ""),
    ] {
        let (status, payload) = send(&rig.app, method, &uri, Some(&acme_key()), body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "for {method} {uri}: {payload}"
        );
        assert_eq!(
            payload["error"]["code"], "session_out_of_namespace",
            "for {method} {uri}"
        );
        assert!(
            !payload["error"]["message"]
                .as_str()
                .expect("a message")
                .contains("not found"),
            "the refusal must not say whether the session exists: {payload}"
        );
    }

    // A compliant id is served, and so is the id this endpoint mints — which
    // must itself be inside the namespace, or it would be created and then
    // immediately unreachable by the key that asked for it.
    let (status, adopted) = send(
        &rig.app,
        "POST",
        "/v1/sessions",
        Some(&acme_key()),
        r#"{"session_id":"acme/ada/mine"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{adopted}");
    assert_eq!(adopted["session_id"], "acme/ada/mine");
    assert_eq!(adopted["created"], true);

    let (status, minted) = send(&rig.app, "POST", "/v1/sessions", Some(&acme_key()), "{}").await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let minted_id = minted["session_id"].as_str().expect("an id");
    assert!(
        minted_id.starts_with("acme/ada/"),
        "a minted id must be inside the caller's namespace: {minted_id}"
    );

    // And the caller can then use it, percent-encoded — which is what proves a
    // namespaced id is not merely accepted but routable. A `/` in a session id
    // is new with this milestone, and a path pattern that stopped matching it
    // would leave every configured deployment unable to reach its own sessions.
    assert_eq!(
        status_of(
            &rig.app,
            "GET",
            &format!(
                "/v1/sessions/{}/events?starting_after=0",
                path_segment("acme/ada/mine")
            ),
            Some(&acme_key()),
        )
        .await,
        StatusCode::OK
    );
}

/// Attribution survives the process that recorded it.
///
/// The fold in memory is a convenience; the log is the authority. A rebuild
/// that read the same events and produced a *different* scoped document would
/// mean the live numbers were coming from somewhere other than the log — the
/// exact drift `metrics` exists to make impossible. Compared as serialized
/// bytes rather than field by field, so a field added later is covered without
/// anyone remembering to add it here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaying_a_log_recovers_the_principal() {
    let rig = two_tenants();

    turn(
        &rig.app,
        bearer(&acme_key()),
        request("work", vec![user_message("hello")]),
    )
    .await
    .expect("acme's turn completes");
    turn(
        &rig.app,
        bearer(&globex_key()),
        request("work", vec![user_message("hello")]),
    )
    .await
    .expect("globex's turn completes");

    let scope = PrincipalKey::from(&acme());
    // A fixed instant, so the two documents differ only where the fold does.
    let at_ms = 4_242;
    let live = rig.metrics.snapshot_for(&scope, &rig.metrics_config, at_ms);

    let rebuilt = MetricsRecorder::new();
    rebuilt.record(&log(&rig.store, "acme/ada/work").await);
    let rebuilt = rebuilt.snapshot_for(&scope, &rig.metrics_config, at_ms);

    assert_eq!(
        serde_json::to_string(&live).expect("encodes"),
        serde_json::to_string(&rebuilt).expect("encodes"),
        "a fold rebuilt from acme's log alone must be byte-identical to the live \
         fold scoped to acme — including its session count, turn count and event \
         window, none of which may carry globex's traffic"
    );
    assert_eq!(live.calls, 1);
    // Not a vacuous comparison: the same fold, scoped to a membership that
    // served nothing, reports nothing. Two empty documents would have compared
    // equal just as happily.
    assert_eq!(
        rig.metrics
            .snapshot_for(
                &PrincipalKey::from(&Principal::new("acme", "nobody")),
                &rig.metrics_config,
                at_ms,
            )
            .calls,
        0
    );
}

/// A key's scope is structural, in both directions.
///
/// An admin key may not serve a turn — it has no membership to bill, and
/// minting one would put spend on a row no project owns — and a turn key sees
/// only its own numbers. These are the two directions M1's surfaces can
/// actually express; the rest of the admin plane is a later milestone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admin_key_cannot_serve_a_turn_and_a_turn_key_sees_only_its_own_metrics() {
    let rig = two_tenants();

    // One real turn each, so there is something to see and something to hide.
    turn(
        &rig.app,
        bearer(&acme_key()),
        request("work", vec![user_message("hello")]),
    )
    .await
    .expect("acme's turn completes");
    turn(
        &rig.app,
        bearer(&globex_key()),
        request("work", vec![user_message("hello")]),
    )
    .await
    .expect("globex's turn completes");

    // Direction one: an admin key on a turn-serving route.
    for (method, uri, body) in [
        ("POST", "/v1/responses", turn_body("admin-attempt", "hello")),
        ("POST", "/v1/sessions", "{}".to_string()),
    ] {
        let (status, payload) = send(&rig.app, method, uri, Some(&admin_key()), &body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "for {method} {uri}: {payload}"
        );
        assert_eq!(payload["error"]["code"], "wrong_key_kind");
    }
    assert!(
        no_such_session(&rig.store, "admin-attempt").await,
        "a refused admin turn must not have created a session"
    );

    // Direction two: a turn key on the metrics surface sees one row, and it is
    // its own.
    let (status, mine) = send(&rig.app, "GET", "/v1/metrics", Some(&acme_key()), "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mine["calls"], 1);
    assert_eq!(mine["sessions"], 1);
    assert_eq!(mine["turns"], 1);

    let (status, all) = send(&rig.app, "GET", "/v1/metrics", Some(&admin_key()), "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["calls"], 2, "an admin sees the deployment: {all}");
    assert_eq!(all["sessions"], 2);
    assert_eq!(all["turns"], 2);

    // The window is scoped too — the field a filter written only over the
    // money rows would leave describing everybody.
    assert!(
        mine["last_event_at_ms"].as_u64() <= all["last_event_at_ms"].as_u64(),
        "a tenant's window cannot extend past the deployment's: {mine} / {all}"
    );

    let (status, payload) = send(&rig.app, "GET", "/v1/metrics", None, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{payload}");
    assert_eq!(payload["error"]["code"], "missing_key");
}

/// A stock client, authenticated the only way a stock client can be.
///
/// Codex takes a static bearer from an environment variable
/// (`model_providers.*.env_key`) and sends it on every request. If that is
/// enough to drive a configured roundhouse through a whole turn, then adopting
/// tenancy costs an existing Codex user a config line and no code — which is
/// the claim the whole key format was chosen to make good on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_client_parses_a_full_turn_under_a_bearer_key() {
    let rig = two_tenants();

    let events = turn(
        &rig.app,
        bearer(&acme_key()),
        request("stock-client", vec![user_message("hello")]),
    )
    .await
    .expect("a stock bearer must be enough to serve a turn");

    let sequence: Vec<&str> = events
        .iter()
        .map(|event| match event {
            ResponseEvent::Created => "response.created",
            ResponseEvent::OutputItemAdded(_) => "response.output_item.added",
            ResponseEvent::OutputTextDelta(_) => "response.output_text.delta",
            ResponseEvent::OutputItemDone(_) => "response.output_item.done",
            ResponseEvent::Completed { .. } => "response.completed",
            _ => "other",
        })
        .collect();
    assert_eq!(
        sequence,
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ],
        "gating must not change a single frame of what a client sees"
    );
    assert_eq!(answer(&events), ANSWER);

    // And the turn landed where the key says it should.
    assert_eq!(
        logged_principal(&rig.store, "acme/ada/stock-client").await,
        Some(acme())
    );
}
