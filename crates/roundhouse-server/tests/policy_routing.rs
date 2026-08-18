// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M2 of `PLAN-agentic-control-plane.md`: a per-principal policy that visibly
//! changes routing.
//!
//! Every claim here is about a *difference between two principals given the
//! identical prompt*, which is why none of them is a unit test. The unit tests
//! one layer down already prove that `TurnPolicy::admits` refuses what it says
//! it refuses; what they cannot prove is that the refusal survives the trip
//! from an `Authorization` header, through the admission seam, into the
//! candidate set, past the router, and out into the log — and that trip is the
//! whole of this milestone.
//!
//! Two fixtures, named for what they make the router prefer, because half the
//! claims here need the policy to *change* the answer and the other half need
//! it to change it back:
//!
//! - [`local_preferred`] — a paid, slow hosted model beside a free, fast local
//!   worker. The default policy picks local, so a quality floor is what makes
//!   the hosted model win.
//! - [`frontier_preferred`] — a free, fast hosted model beside a slow local
//!   worker. The default policy picks the hosted model every turn, so a cadence
//!   is what makes local win, and local winning is the cadence working rather
//!   than a coincidence of scoring.
//!
//! Neither is a claim about pricing. They are two ways of arranging the same
//! three scoring axes so that the policy is the only thing left that could have
//! moved the decision.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{Principal, PrincipalKey, TurnPolicy};
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::SessionId;
use roundhouse_core::metrics::{MetricsConfig, MetricsRecorder, ShadowPricing};
use roundhouse_core::routing::{AffinityPolicy, CacheModel, DecisionRecord, ProviderPricing};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierModelSpec, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, EchoLocalExecutor, Engine, EngineConfig, http, metrics_api,
    responses_api,
};

mod common;
use common::codex::{request, user_message};
use common::{BLOCK_SIZE, LOCAL_MODEL, MINUTE, embedded_fleet};

/// What each executor answers with, so a target is legible in the answer as
/// well as in the log.
const LOCAL_ANSWER: &str = "local answer";
const FRONTIER_ANSWER: &str = "frontier answer";

// ---------------------------------------------------------------------------
// Catalogs
// ---------------------------------------------------------------------------

/// A paid, slow hosted model. Beside a free local worker the router prefers
/// local, so this is the fixture in which a *quality floor* is what changes the
/// answer.
fn paid_catalog() -> StaticFrontierCatalog {
    common::frontier_catalog()
}

/// A free, instant hosted model. Beside the deliberately slow local worker in
/// [`frontier_preferred`] the router prefers it every turn, so this is the
/// fixture in which a *cadence* is what changes the answer.
fn free_catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        provider: "anthropic".into(),
        model: "claude".into(),
        wire_protocol: WireProtocol::AnthropicMessages,
        cache_model: CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
        pricing: ProviderPricing::free(),
        quality_prior: 0.95,
        base_ttft_ms: 1.0,
        ttft_ms_per_uncached_token: 0.0,
    }])
}

// ---------------------------------------------------------------------------
// Keys and the control plane that declares them
// ---------------------------------------------------------------------------

/// A well-shaped secret with `tag` legible inside it, padded to the 43 base62
/// characters the key format requires — the same fixture rule the tenancy suite
/// uses, and for the same reason: a hand-counted literal fails as
/// `malformed_key` for a reason no assertion names.
fn key(tag: &str) -> String {
    format!("rh_turn_{tag:A<43}")
}

fn sha256_hex(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// One project per policy shape under test, each with one key.
///
/// Built through `ControlPlaneConfig::from_json` rather than by assembling the
/// lookup tables: the file *is* the format, and the narrowing that produces
/// `mixed/ada`'s effective policy happens inside `validate` — a fixture that
/// bypassed it would be testing a policy no operator could write.
///
/// `mixed` is the one project with two keys, and it is the reason the whole
/// suite is Configured rather than a set of hand-built policies: `ada` and
/// `bob` differ only by an override, so anything that separates their routing
/// came through `narrow`.
fn control_plane() -> Arc<ControlPlane> {
    let json = serde_json::json!({
        "projects": [
            { "id": "unbounded" },
            { "id": "discerning", "policy": { "min_quality": 0.9 } },
            { "id": "ourown", "policy": { "allow": ["local/*"] } },
            { "id": "nowhere", "policy": { "allow": ["openai/*"] } },
            {
                "id": "rationed",
                "policy": { "frontier_cadence": { "max_frontier": 1, "per_turns": 3 } }
            },
            { "id": "mixed", "policy": { "min_quality": 0.5 } },
        ],
        "users": [{ "id": "ada" }, { "id": "bob" }],
        "keys": [
            { "project": "unbounded", "user": "ada", "key_sha256": sha256_hex(&key("unbounded")) },
            { "project": "discerning", "user": "ada", "key_sha256": sha256_hex(&key("discerning")) },
            { "project": "ourown", "user": "ada", "key_sha256": sha256_hex(&key("ourown")) },
            { "project": "nowhere", "user": "ada", "key_sha256": sha256_hex(&key("nowhere")) },
            { "project": "rationed", "user": "ada", "key_sha256": sha256_hex(&key("rationed")) },
            {
                "project": "mixed", "user": "ada", "key_sha256": sha256_hex(&key("mixedada")),
                "overrides": { "min_quality": 0.9 }
            },
            { "project": "mixed", "user": "bob", "key_sha256": sha256_hex(&key("mixedbob")) },
        ],
    })
    .to_string();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "policy-routing fixture")
            .expect("the fixture config must validate"),
    ))
}

// ---------------------------------------------------------------------------
// The deployment under test
// ---------------------------------------------------------------------------

struct Deployment {
    app: Router,
    store: Arc<MemoryStore>,
    metrics: Arc<MetricsRecorder>,
    metrics_config: Arc<MetricsConfig>,
}

/// The paid-hosted-model fixture: left alone, the router serves locally.
async fn local_preferred(plane: Arc<ControlPlane>) -> Deployment {
    deployment(
        plane,
        paid_catalog(),
        EngineConfig {
            block_size: BLOCK_SIZE,
            local_model: LOCAL_MODEL.to_string(),
            ..Default::default()
        },
    )
    .await
}

/// The free-hosted-model fixture: left alone, the router serves from the
/// hosted model every turn.
///
/// The local worker is given a five-second latency floor to arrange that. It is
/// a fixture knob and not a claim about any real fleet — what it buys is that
/// when a turn *does* land locally, the cadence is the only thing that could
/// have put it there.
async fn frontier_preferred(plane: Arc<ControlPlane>) -> Deployment {
    deployment(
        plane,
        free_catalog(),
        EngineConfig {
            block_size: BLOCK_SIZE,
            local_model: LOCAL_MODEL.to_string(),
            local_base_ttft_ms: 5_000.0,
            ..Default::default()
        },
    )
    .await
}

async fn deployment(
    plane: Arc<ControlPlane>,
    catalog: StaticFrontierCatalog,
    config: EngineConfig,
) -> Deployment {
    ensure_rustls_crypto_provider();
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new(LOCAL_ANSWER)),
            catalog,
            Arc::new(EchoFrontierClient::new(FRONTIER_ANSWER)),
            Arc::new(AffinityPolicy::new()),
            config,
        )
        // A real local option, or every one of these claims would be about a
        // one-candidate set and none of them would mean anything.
        .with_fleet(embedded_fleet().await),
    );
    let metrics = engine.metrics();
    // No reference models: `routing_savings_at_decision_usd`, the number these
    // tests read, is folded from the decision's own `considered` list and owes
    // nothing to shadow pricing. Leaving the shadow table empty keeps the two
    // counterfactuals from being confused for one another.
    let metrics_config = Arc::new(MetricsConfig::new(ShadowPricing::new(Vec::new())));
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

/// One turn on the Responses surface, returning the raw SSE body.
///
/// Raw rather than through Codex's client because two of the claims below are
/// about which *terminal frame* the client is sent, and a client library's job
/// is to turn those into one uniform outcome — exactly the distinction under
/// test.
async fn responses_turn(app: &Router, secret: &str, prompt: &str, cache_key: &str) -> String {
    let body = serde_json::to_string(&request(cache_key, vec![user_message(prompt)]))
        .expect("the request encodes");
    let (status, text) = post(app, "/v1/responses", Some(secret), &body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the turn must be admitted; refusals are a different milestone's subject: {text}"
    );
    text
}

/// One turn on the native surface, which holds the history server-side.
///
/// The multi-turn claim uses this rather than the Responses surface because a
/// cadence is a fact about a *session* over several turns, and replaying a
/// growing transcript through prefix admission would put the client's history
/// bookkeeping between the test and the thing being tested.
async fn native_turn(app: &Router, secret: &str, session_id: &str, turn_id: &str, prompt: &str) {
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
    // The native surface forwards the log's own event names, so this is
    // `response_completed` rather than the Responses dialect's dotted spelling.
    assert!(
        text.contains("event: response_completed"),
        "turn `{turn_id}` did not complete: {text}"
    );
}

/// A namespaced session id as one path segment.
///
/// A Configured deployment's ids carry `/`, and a route parameter matches a
/// single segment, so the separators have to be escaped or the request routes
/// nowhere. That is a fact about the native surface rather than about policy —
/// it is spelled out here rather than worked around silently because a `404`
/// from a mistyped path and a `404` from an unescaped id read identically.
fn path_segment(session_id: &str) -> String {
    session_id.replace('/', "%2F")
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

/// One POST, body drained to a string.
///
/// Every response this suite asks for terminates by itself — a turn's stream
/// ends when its response does — so draining is safe and is what makes the
/// turn's completion observable before the next assertion runs.
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

// ---------------------------------------------------------------------------
// Reading the log
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

/// The session id a Responses-surface cache key binds to for `principal`.
fn bound(project: &str, user: &str, cache_key: &str) -> String {
    format!("{project}/{user}/{cache_key}")
}

// ---------------------------------------------------------------------------
// The flagship: the same prompt, two principals, two targets
// ---------------------------------------------------------------------------

/// A quality floor takes a target away from the router, and the router's answer
/// changes.
///
/// The two turns differ in exactly one byte of input — the bearer token — and
/// the fixture is arranged so the unrestricted principal's answer is local. If
/// the floor did nothing, both would be local, which is what this failed as
/// before the policy reached `RoutingContext`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quality_floor_excludes_a_target_the_default_policy_would_pick() {
    let rig = local_preferred(control_plane()).await;

    responses_turn(&rig.app, &key("unbounded"), "hello", "chat").await;
    responses_turn(&rig.app, &key("discerning"), "hello", "chat").await;

    let unbounded = decision(&rig.store, &bound("unbounded", "ada", "chat")).await;
    let discerning = decision(&rig.store, &bound("discerning", "ada", "chat")).await;

    assert_eq!(
        unbounded.chosen.policy_identity(),
        "local/local",
        "the control: with no floor the default policy takes the free, warm-enough local worker"
    );
    assert_eq!(
        discerning.chosen.policy_identity(),
        "anthropic/claude",
        "a 0.9 floor is above the local worker's 0.6 prior, so the hosted model is all that is left"
    );

    // And it is exclusion, not preference: the local worker is not merely
    // outscored, it is absent from what the decision says it weighed.
    assert!(
        !discerning
            .considered
            .iter()
            .any(|candidate| candidate.target.is_local()),
        "a floored-out target must not appear in `considered`: {:#?}",
        discerning.considered
    );
    assert_eq!(
        unbounded.considered.len(),
        2,
        "the control again: without a floor both options were weighed"
    );
}

/// The accounting half of the same fact.
///
/// `best_frontier_alternative` reads a decision's own `considered` list, so a
/// hosted model left in it after the principal was forbidden to use it would be
/// reported as a saving the deployment never made — the one number the whole
/// dashboard is judged by, inflated by a filter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filtered_target_never_appears_in_considered() {
    let rig = local_preferred(control_plane()).await;

    responses_turn(&rig.app, &key("ourown"), "hello", "chat").await;
    responses_turn(&rig.app, &key("unbounded"), "hello", "chat").await;

    let filtered = decision(&rig.store, &bound("ourown", "ada", "chat")).await;
    assert_eq!(filtered.chosen.policy_identity(), "local/local");
    assert_eq!(
        filtered
            .considered
            .iter()
            .map(|candidate| candidate.target.policy_identity())
            .collect::<Vec<_>>(),
        vec!["local/local".to_string()],
        "`allow: [local/*]` leaves the hosted model out of the record entirely"
    );

    let at_ms = 4_242;
    let savings = |principal: &Principal| {
        rig.metrics
            .snapshot_for(&PrincipalKey::from(principal), &rig.metrics_config, at_ms)
            .savings
            .routing_savings_at_decision_usd
    };

    assert_eq!(
        savings(&Principal::new("ourown", "ada")),
        0.0,
        "a model this key may not use is not a saving when the turn goes local"
    );
    // The control, and it is what makes the assertion above non-vacuous: the
    // identical turn, same fixture, same local target, does book a
    // counterfactual — because for that principal the hosted model really was
    // an option that was passed over.
    assert!(
        savings(&Principal::new("unbounded", "ada")) > 0.0,
        "the unfiltered principal's local turn is priced against the hosted model it declined"
    );
}

/// A policy that admits nothing fails the turn. It does not quietly serve it
/// locally.
///
/// The failure mode this guards is shaped like a success: a deployment whose
/// filter silently routed every turn to the cheapest target would show up on
/// the dashboard as a cost win, and nobody investigates a cost win.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_admissible_set_fails_the_turn_rather_than_silently_going_local() {
    let rig = local_preferred(control_plane()).await;

    let body = responses_turn(&rig.app, &key("nowhere"), "hello", "chat").await;

    assert!(
        body.contains("response.failed"),
        "a refusal is a failed response, not a truncated one: {body}"
    );
    assert!(
        !body.contains("response.incomplete"),
        "`incomplete` would tell the client the model ran out of room: {body}"
    );
    assert!(
        !body.contains(LOCAL_ANSWER) && !body.contains(FRONTIER_ANSWER),
        "nothing may have been served: {body}"
    );

    // Nothing was dispatched, so nothing was recorded as a decision — and the
    // response is terminated all the same, or the client's next retry would
    // append its input a second time rather than be refused again.
    assert!(
        decisions(&rig.store, &bound("nowhere", "ada", "chat"))
            .await
            .is_empty(),
        "a refused turn routes nowhere and records no decision"
    );
    assert!(
        log(&rig.store, &bound("nowhere", "ada", "chat"))
            .await
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::ResponseIncomplete { .. })),
        "the response must still be terminated in the log"
    );

    // The control: the same deployment, the same prompt, a key whose policy
    // admits something. A suite in which every turn failed would have passed
    // the assertions above just as happily.
    let served = responses_turn(&rig.app, &key("unbounded"), "hello", "chat").await;
    assert!(served.contains("response.completed"), "{served}");
}

/// The cadence is a ration, and a spent ration serves locally rather than
/// failing — which is the whole difference between it and the filter above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontier_cadence_serves_the_ration_then_local_until_the_window_slides() {
    let rig = frontier_preferred(control_plane()).await;
    let session = bound("rationed", "ada", "work");
    create_session(&rig.app, &key("rationed"), &session).await;

    // `per_turns` is three, and five turns is what it takes to see all three
    // states: the ration spent, the window holding, and the window sliding
    // open again. Four would stop one turn short of the recovery, which is the
    // half of a cadence that a broken implementation still gets right.
    for turn in 0..5 {
        native_turn(
            &rig.app,
            &key("rationed"),
            &session,
            &format!("t{turn}"),
            "hello",
        )
        .await;
    }

    assert_eq!(
        route_sequence(&rig.store, &session).await,
        vec![
            "anthropic/claude",
            "local/local",
            "local/local",
            "local/local",
            "anthropic/claude",
        ],
        "one hosted dispatch per three turns: spent on turn 0, held through turns 1-3, \
         and re-admitted on turn 4 once turn 0 has fallen out of the trailing window"
    );

    // The control, in the same fixture: an unrationed key on the identical
    // deployment goes to the hosted model every single turn, which is what
    // proves the local turns above were the cadence and not the scoring.
    let unrationed = bound("unbounded", "ada", "work");
    create_session(&rig.app, &key("unbounded"), &unrationed).await;
    for turn in 0..5 {
        native_turn(
            &rig.app,
            &key("unbounded"),
            &unrationed,
            &format!("t{turn}"),
            "hello",
        )
        .await;
    }
    assert_eq!(
        route_sequence(&rig.store, &unrationed).await,
        vec!["anthropic/claude"; 5],
        "with no cadence the same five turns all reach the hosted model"
    );
}

/// Every decision carries a fingerprint of the policy it was made under, and
/// two principals under different policies do not share one.
///
/// Without this the audit trail can say what was chosen but not what the
/// choice was allowed to range over, and a policy change would be invisible in
/// the log it changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_decision_records_the_policy_digest_it_was_made_under() {
    let rig = local_preferred(control_plane()).await;

    responses_turn(&rig.app, &key("unbounded"), "hello", "chat").await;
    responses_turn(&rig.app, &key("discerning"), "hello", "chat").await;

    let unbounded = decision(&rig.store, &bound("unbounded", "ada", "chat")).await;
    let discerning = decision(&rig.store, &bound("discerning", "ada", "chat")).await;

    // The digest of the *effective* policy, recomputed here from the shape the
    // config declared rather than copied from the record — a test that read
    // the digest back off the thing that wrote it would agree with any digest
    // at all.
    assert_eq!(
        unbounded.turn_policy_digest,
        TurnPolicy::unrestricted().digest(),
        "a project with no policy routes under the unrestricted one"
    );
    assert_eq!(
        discerning.turn_policy_digest,
        TurnPolicy {
            min_quality: 0.9,
            ..TurnPolicy::unrestricted()
        }
        .digest(),
    );
    assert_ne!(
        unbounded.turn_policy_digest, discerning.turn_policy_digest,
        "two policies that route differently must fingerprint differently"
    );
    assert!(
        !unbounded.turn_policy_digest.is_empty(),
        "the empty digest is reserved for logs older than this milestone; the \
         unrestricted policy has a real one"
    );
}

/// A key's overrides narrow its project's policy, and the narrowed policy is
/// what routes.
///
/// `ada` and `bob` are in one project, under one project policy, and differ
/// only by an `overrides` block — so anything that separates their routing came
/// through `narrow` and through nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_override_narrowed_key_routes_under_the_narrowed_policy() {
    let rig = local_preferred(control_plane()).await;

    responses_turn(&rig.app, &key("mixedada"), "hello", "chat").await;
    responses_turn(&rig.app, &key("mixedbob"), "hello", "chat").await;

    let ada = decision(&rig.store, &bound("mixed", "ada", "chat")).await;
    let bob = decision(&rig.store, &bound("mixed", "bob", "chat")).await;

    assert_eq!(
        bob.chosen.policy_identity(),
        "local/local",
        "the project's own 0.5 floor is below the local worker's 0.6 prior, so it stands"
    );
    assert_eq!(
        ada.chosen.policy_identity(),
        "anthropic/claude",
        "ada's key raises the floor to 0.9, which the local worker no longer clears"
    );
    assert_eq!(
        ada.turn_policy_digest,
        TurnPolicy {
            min_quality: 0.9,
            ..TurnPolicy::unrestricted()
        }
        .digest(),
        "the digest is of the effective policy, not of the project's"
    );
    assert_ne!(ada.turn_policy_digest, bob.turn_policy_digest);
}

/// Turning the control plane on must not re-route anything.
///
/// An unconfigured deployment resolves every request to
/// `TurnPolicy::unrestricted`, and the compatibility claim is that this changes
/// no routing decision at all. The rationale is pinned as a literal because it
/// is the one string that reports how many candidates were weighed and what the
/// winner scored — a filter that quietly dropped one would move it.
///
/// The digest question this test settles: an Open-mode decision records the
/// digest *of the unrestricted policy*, not the empty string. The empty string
/// means "written before per-principal policy existed", which is a fact about a
/// log's age and not a policy; an Open-mode deployment running this code has a
/// policy, and it is the unrestricted one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_mode_routing_is_byte_identical_to_m1() {
    let open = local_preferred(ControlPlane::open()).await;
    let configured = local_preferred(control_plane()).await;

    // Open mode does not namespace, so the cache key is the session id.
    let (status, body) = post(
        &open.app,
        "/v1/responses",
        None,
        &serde_json::to_string(&request("chat", vec![user_message("hello")])).expect("encodes"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    responses_turn(&configured.app, &key("unbounded"), "hello", "chat").await;

    let unauthenticated = decision(&open.store, "chat").await;
    let unrestricted = decision(&configured.store, &bound("unbounded", "ada", "chat")).await;

    assert_eq!(unauthenticated.chosen.policy_identity(), "local/local");
    assert_eq!(unauthenticated.considered.len(), 2);
    assert_eq!(
        unauthenticated.rationale, M1_RATIONALE,
        "the decision an unconfigured deployment makes is the one it made before \
         tenancy existed, down to the score and the candidate count"
    );
    assert_eq!(
        unauthenticated.turn_policy_digest,
        TurnPolicy::unrestricted().digest(),
        "an open deployment routes under a real policy that happens to permit everything"
    );

    // And a configured principal with no policy of its own is indistinguishable
    // from open mode where routing is concerned — which is what makes "turn the
    // control plane on" a safe operation rather than a migration.
    assert_eq!(unauthenticated.chosen, unrestricted.chosen);
    assert_eq!(unauthenticated.rationale, unrestricted.rationale);
    assert_eq!(unauthenticated.considered, unrestricted.considered);
    assert_eq!(
        unauthenticated.turn_policy_digest,
        unrestricted.turn_policy_digest
    );
}

/// The decision `local_preferred` produces for a one-item `"hello"` prompt with
/// nothing narrowed.
///
/// A literal rather than a recomputation, because a recomputation would agree
/// with whatever the code now does. It is the same quantity
/// `roundhouse-core`'s own `an_unrestricted_policy_reproduces_m1_routing_byte_for_byte`
/// pins one layer down, where the expectations were captured against the tree
/// before any M2 code existed.
const M1_RATIONALE: &str =
    "score 0.0000 over 2 candidate(s); expected prefill 31 of 31 tokens (0% cached), $0.00000";

/// A filter and a cadence are different kinds of narrowing, and the difference
/// is which failure they are allowed to produce.
///
/// Stated as its own test because the two live one line apart in
/// `TurnPolicy::admits` and the comment there is the only thing that says why —
/// a future edit that unified them would pass every other test in this file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spent_cadence_serves_where_an_empty_filter_refuses() {
    let rig = frontier_preferred(control_plane()).await;

    // A cadence of one per three, spent on the first turn: the second turn has
    // no hosted option and serves anyway.
    let rationed = bound("rationed", "ada", "pair");
    create_session(&rig.app, &key("rationed"), &rationed).await;
    native_turn(&rig.app, &key("rationed"), &rationed, "t0", "hello").await;
    native_turn(&rig.app, &key("rationed"), &rationed, "t1", "hello").await;
    assert_eq!(
        route_sequence(&rig.store, &rationed).await,
        vec!["anthropic/claude", "local/local"],
        "the turn served; that is the knob working, not a silent fallback"
    );

    // A filter that names nothing has no such second option, and must not
    // borrow the cadence's answer.
    let body = responses_turn(&rig.app, &key("nowhere"), "hello", "pair").await;
    assert!(body.contains("response.failed"), "{body}");
}

/// Retrying an already-completed `turn_id` deduplicates rather than re-routing,
/// so it must not spend a second unit of a spent cadence.
///
/// Probe added during the M2 adversarial review (attack 3, "dedup retries"):
/// `begin_turn` short-circuits to `TurnAdmission::Deduplicated` before
/// `record_routing` is ever called, so no second `Routed` event should be
/// logged and the window three turns later should still see only the original
/// dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deduplicated_retry_does_not_spend_a_second_ration() {
    let rig = frontier_preferred(control_plane()).await;
    let session = bound("rationed", "ada", "dedup");
    create_session(&rig.app, &key("rationed"), &session).await;

    native_turn(&rig.app, &key("rationed"), &session, "t0", "hello").await;
    // Retry the identical turn id three times in a row. If each retry spent
    // its own unit of the cadence the window would still read "spent" long
    // after it should have slid open.
    for _ in 0..3 {
        native_turn(&rig.app, &key("rationed"), &session, "t0", "hello").await;
    }
    assert_eq!(
        decisions(&rig.store, &session).await.len(),
        1,
        "a deduplicated retry must not append a second routing decision"
    );

    // Three more real turns: the ration was spent once, on t0, so all three
    // must serve locally rather than the window somehow already having room
    // again.
    native_turn(&rig.app, &key("rationed"), &session, "t1", "hello").await;
    native_turn(&rig.app, &key("rationed"), &session, "t2", "hello").await;
    native_turn(&rig.app, &key("rationed"), &session, "t3", "hello").await;
    assert_eq!(
        route_sequence(&rig.store, &session).await,
        vec![
            "anthropic/claude",
            "local/local",
            "local/local",
            "local/local"
        ],
        "one real dispatch on t0, held through t1, t2 and t3 by the same \
         window a non-deduplicated t0 would have produced"
    );

    // And the window recovers on schedule: t0's window is turns 1-3, so it is
    // the 5th real turn (t4) — not the 4th (t3) — where t0 has finally fallen
    // out of the trailing window of three. (First draft of this probe issued
    // only four real turns and asserted recovery on the fourth; the window a
    // turn checks is the *preceding* `per_turns` turns, so t3's preceding
    // three real turns are t0, t1, t2 and still include t0.)
    native_turn(&rig.app, &key("rationed"), &session, "t4", "hello").await;
    assert_eq!(
        route_sequence(&rig.store, &session).await,
        vec![
            "anthropic/claude",
            "local/local",
            "local/local",
            "local/local",
            "anthropic/claude"
        ],
        "the retries were transparent to the cadence: recovery lands exactly \
         where an unretried session would land"
    );
}
