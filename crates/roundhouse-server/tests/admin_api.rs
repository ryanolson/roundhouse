// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M8: the admin plane over HTTP, and the reconciliation view.
//!
//! None of these is a unit test, and that is the point of the milestone. The
//! directory's own suite already proves what a mutation may do and what it
//! compiles to; what it cannot prove is that a secret minted over HTTP appears
//! in exactly one response body, that revoking it stops a *turn* on a different
//! router, and that the budget view's two dollar columns are produced by two
//! independent machines and published without being reconciled into one number.
//! All three of those are claims about a deployment rather than about a type.
//!
//! # The fixture
//!
//! One hosted model priced on output alone at a round rate, no local fleet, and
//! an echo client whose answer is a known length — so a turn costs exactly
//! [`ACTUAL_TURN_USD`] and "the ledger says X" is arithmetic rather than a
//! measurement. The control-plane file declares one project, one member and one
//! admin key; everything else in these tests is created through the API, which
//! is also the only way to get a key that may be minted at all (a membership the
//! file declares is owned by the file, and minting under it is refused 409).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    Balance, BalanceQuery, Grant, GrantRequest, MemorySpendLedger, Settled, Settlement, SpendError,
    SpendLedger,
};
use roundhouse_core::metrics::{MetricsConfig, ReferenceModel, ShadowPricing};
use roundhouse_core::now_ms;
use roundhouse_core::routing::{
    AffinityPolicy, CacheModel, Candidate, ProviderPricing, Target, policy::Weights,
};
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::{
    EchoFrontierClient, FrontierModelSpec, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::control_config::crosscheck::CrossChecks;
use roundhouse_server::{
    ControlDirectory, Conversations, EchoLocalExecutor, Engine, EngineConfig, MemoryDirectoryStore,
    admin_api, has_valid_key_shape, http, metrics_api, responses_api,
};

mod common;
use common::{admin_key, control_plane, key, path_segment, sha256_hex};

/// Sixteen bytes, twice [`EXPECTED_OUTPUT_TOKENS`] — so a turn settles above its
/// hold, which is the ordinary path the ledger documents about itself.
const FRONTIER_ANSWER: &str = "frontier answer!";
const OUTPUT_PER_MTOK_USD: f64 = 12_500.0;
const EXPECTED_OUTPUT_TOKENS: u32 = 8;
/// `FRONTIER_ANSWER.len() * OUTPUT_PER_MTOK_USD / 1e6` — what one turn settles.
const ACTUAL_TURN_USD: f64 = 0.2;
/// Far above one turn, so nothing in this file is about exhaustion.
const LIMIT_USD: f64 = 10.0;

/// A tenth of a cent: below every quantity this fixture distinguishes, above the
/// rounding of a handful of multiplications.
const CENTS: f64 = 1e-4;
/// What "these two JSON numbers are the same number" means here.
const EPSILON: f64 = 1e-6;

fn assert_usd(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < CENTS,
        "{what}: expected ${expected}, got ${actual}"
    );
}

// ---------------------------------------------------------------------------
// The deployment under test
// ---------------------------------------------------------------------------

/// One hosted model, priced on output alone so a turn's cost does not drift with
/// the length of the conversation.
fn catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        provider: "anthropic".into(),
        model: "claude".into(),
        wire_protocol: WireProtocol::AnthropicMessages,
        cache_model: CacheModel::Deterministic { ttl_ms: 5 * 60_000 },
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

/// The same rate card, declared to the metrics fold.
///
/// **Both halves are needed and they are read by different machinery**, which is
/// exactly what the reconciliation view is about. The catalog above is what the
/// *engine* prices a turn against on its way to the ledger; this is what the
/// *fold* prices the same turn against on its way to the dashboard. A fixture
/// that declared only the catalog would leave `measured_usd` at zero for a turn
/// that really did spend, which is a drift the view would report honestly and
/// which would have nothing to do with the deployment.
///
/// The two carry the same numbers here so that a correct deployment reads as
/// zero drift, and every non-zero drift below is something the fixture did on
/// purpose.
fn metrics_config() -> Arc<MetricsConfig> {
    Arc::new(MetricsConfig::new(ShadowPricing::new(vec![
        ReferenceModel {
            provider: "anthropic".into(),
            model: "claude".into(),
            pricing: ProviderPricing {
                input_per_mtok_usd: 0.0,
                cached_input_per_mtok_usd: 0.0,
                cache_write_per_mtok_usd: 0.0,
                output_per_mtok_usd: OUTPUT_PER_MTOK_USD,
            },
            quality_prior: 0.95,
        },
    ])))
}

/// What this deployment can route to, as the cross-checks read it.
///
/// Hand-built rather than quoted: the two checks read a candidate's target
/// identity and its quality prior, and a quote would decide neither
/// differently. It matters that the list is non-empty — an empty one refuses
/// every policy, including the default.
fn reachable() -> Vec<Candidate> {
    vec![Candidate {
        target: Target::Frontier {
            provider: "anthropic".into(),
            model: "claude".into(),
        },
        expected_prefill_tokens: 1_024.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: 1.0,
        expected_cost_usd: 0.0,
        quality_prior: 0.95,
        load: None,
    }]
}

/// The admin secret every request below authenticates with, declared in the
/// file — the only root of trust an admin key can come from.
fn root() -> String {
    admin_key("root")
}

/// The file half: one project, one member, one admin key.
///
/// `acme` and its membership are here rather than created over the API on
/// purpose: they are what the config-ownership tests need something *file-owned*
/// to refuse a change to, and what makes the escape hatch visible — a new
/// `(project, user)` pair inside `acme` is a create and works, while a key under
/// `acme/ada` is refused.
fn file() -> roundhouse_server::ControlPlaneConfig {
    control_plane(
        json!({
            "projects": [{ "id": "acme", "name": "Acme Corp" }],
            "users": [{ "id": "ada" }],
            "keys": [
                { "project": "acme", "user": "ada", "key_sha256": sha256_hex(&key("ada")) },
            ],
            "admin_keys": [sha256_hex(&root())],
        }),
        "admin-api fixture",
    )
}

struct Rig {
    app: Router,
    directory: Arc<ControlDirectory>,
}

/// A deployment whose admin plane is real: a managed directory over a memory
/// store, so writes compile and take effect on this node immediately.
async fn rig(ledger: Arc<dyn SpendLedger>) -> Rig {
    ensure_rustls_crypto_provider();
    let directory = Arc::new(
        ControlDirectory::new(
            file(),
            "ROUNDHOUSE_CONTROL_PLANE",
            Arc::new(MemoryDirectoryStore::new()),
            CrossChecks::new(reachable(), None),
            now_ms(),
        )
        .expect("the file alone compiles, since it is what a boot would have loaded"),
    );
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local answer")),
            catalog(),
            Arc::new(EchoFrontierClient::new(FRONTIER_ANSWER)),
            // Price switched off as a routing input, for the reason
            // `budget_routing` switches it off: with it on, a free local worker
            // would outscore the hosted model and these turns would cost nothing
            // — which is a fine deployment and a useless fixture for a view
            // about money.
            Arc::new(AffinityPolicy::new().with_weights(Weights {
                prefill: 1.0,
                cost: 0.0,
                ttft: 0.25,
            })),
            EngineConfig {
                expected_output_tokens: EXPECTED_OUTPUT_TOKENS,
                ..Default::default()
            },
        )
        .with_spend_ledger(Arc::clone(&ledger)),
    );
    let metrics_config = metrics_config();
    let app = admin_api::admin_router(
        Arc::clone(&directory),
        ledger,
        engine.metrics(),
        Arc::clone(&metrics_config),
    )
    .merge(http::router(
        Arc::clone(&directory),
        Arc::clone(&engine),
        Arc::clone(&store),
    ))
    .merge(metrics_api::metrics_router(
        Arc::clone(&directory),
        engine.metrics(),
        metrics_config,
    ))
    .merge(responses_api::responses_router(
        Arc::clone(&directory),
        engine,
        store,
        Arc::new(Conversations::new()),
    ));
    Rig { app, directory }
}

async fn plain() -> Rig {
    rig(Arc::new(MemorySpendLedger::new())).await
}

/// A project that forwards its members' subscription seats rather than
/// billing through this deployment's own rate card -- what R5's `seat_tokens`
/// column exists to make visible.
///
/// Kept out of [`file`], which every other test in this module shares, so
/// this fixture's presence does not change what `GET /v1/admin/projects` or
/// `GET /v1/admin/keys` return elsewhere. No `"budget"` on the project:
/// forwarded traffic bills nothing this deployment may name, so there is no
/// ceiling to give it.
fn pass_through_file() -> roundhouse_server::ControlPlaneConfig {
    control_plane(
        json!({
            "projects": [
                { "id": "forwarding", "credentials": { "mode": "pass_through" } },
            ],
            "users": [{ "id": "faye" }],
            "keys": [
                { "project": "forwarding", "user": "faye", "key_sha256": sha256_hex(&key("faye")) },
            ],
            "admin_keys": [sha256_hex(&root())],
        }),
        "admin-api pass-through fixture",
    )
}

/// A deployment shaped for one test, `seat_tokens_are_visible_at_both_the_project_and_the_member_level`.
///
/// [`rig`] is deliberately frontier-only (see the module doc) — routing is
/// then arithmetic rather than a race, which is what the money-column tests
/// need. That is also why it is the wrong fixture here: a forwarding project's
/// turn with nothing captured to forward is exactly the shape
/// `mcp_surface.rs`'s pass-through tests degrade to local for, and `rig` has
/// no local to degrade to — the turn would fail at dispatch rather than
/// complete, and an incomplete turn measures no tokens at all. This rig adds a
/// real local worker via [`common::embedded_fleet`] so a plain, header-free
/// `turn()` completes the way it does in `mcp_surface.rs`, and still books as
/// [`Billing::AccountedNotBilled`](roundhouse_core::control::Billing::AccountedNotBilled)
/// — that reads off the project's *mode*, never off whether a credential was
/// presented (see `payer.rs`'s `Billing::of`).
async fn pass_through_rig() -> Rig {
    ensure_rustls_crypto_provider();
    let directory = Arc::new(
        ControlDirectory::new(
            pass_through_file(),
            "ROUNDHOUSE_CONTROL_PLANE",
            Arc::new(MemoryDirectoryStore::new()),
            CrossChecks::new(reachable(), None),
            now_ms(),
        )
        .expect("the file alone compiles, since it is what a boot would have loaded"),
    );
    let store = Arc::new(MemoryStore::new());
    let ledger: Arc<dyn SpendLedger> = Arc::new(MemorySpendLedger::new());
    let engine = Arc::new(
        Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local answer")),
            catalog(),
            Arc::new(EchoFrontierClient::new(FRONTIER_ANSWER)),
            Arc::new(AffinityPolicy::new().with_weights(Weights {
                prefill: 1.0,
                cost: 0.0,
                ttft: 0.25,
            })),
            EngineConfig {
                expected_output_tokens: EXPECTED_OUTPUT_TOKENS,
                block_size: common::BLOCK_SIZE,
                local_model: common::LOCAL_MODEL.to_string(),
                ..Default::default()
            },
        )
        .with_fleet(common::embedded_fleet().await)
        .with_spend_ledger(Arc::clone(&ledger)),
    );
    let metrics_config = metrics_config();
    let app = admin_api::admin_router(
        Arc::clone(&directory),
        ledger,
        engine.metrics(),
        Arc::clone(&metrics_config),
    )
    .merge(http::router(
        Arc::clone(&directory),
        Arc::clone(&engine),
        Arc::clone(&store),
    ))
    .merge(metrics_api::metrics_router(
        Arc::clone(&directory),
        engine.metrics(),
        metrics_config,
    ))
    .merge(responses_api::responses_router(
        Arc::clone(&directory),
        engine,
        store,
        Arc::new(Conversations::new()),
    ));
    Rig { app, directory }
}

// ---------------------------------------------------------------------------
// Driving it
// ---------------------------------------------------------------------------

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    secret: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(secret) = secret {
        builder = builder.header(AUTHORIZATION, format!("Bearer {secret}"));
    }
    let request = match body {
        Some(body) => builder
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("a well-formed request"),
        None => builder.body(Body::empty()).expect("a well-formed request"),
    };
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("the router answers every request");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body that reads")
        .to_bytes();
    (status, String::from_utf8(bytes.to_vec()).expect("UTF-8"))
}

/// A request as the root admin key, asserted to have succeeded.
async fn admin(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let (status, text) = send(app, method, uri, Some(&root()), body).await;
    assert!(
        status.is_success(),
        "{method} {uri} was refused {status}: {text}"
    );
    if text.is_empty() {
        return Value::Null;
    }
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{uri} answered non-JSON: {error}"))
}

/// A `GET` as the root admin key, asserted to have succeeded.
async fn read(app: &Router, uri: &str) -> Value {
    admin(app, "GET", uri, None).await
}

/// The refusal an admin request produced: its status and its `error.code`.
async fn refused(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    refused_as(app, method, uri, Some(root()), body).await
}

async fn refused_as(
    app: &Router,
    method: &str,
    uri: &str,
    secret: Option<String>,
    body: Option<Value>,
) -> (StatusCode, String) {
    let (status, text) = send(app, method, uri, secret.as_deref(), body).await;
    assert!(
        status.is_client_error() || status.is_server_error(),
        "{method} {uri} was expected to be refused and answered {status}: {text}"
    );
    let json: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("a refusal must carry the error envelope: {error}: {text}"));
    let code = json["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("a refusal must carry `error.code`: {text}"))
        .to_string();
    (status, code)
}

/// Project, user and membership in three writes — what every mint needs first.
async fn tenancy(app: &Router, project: Value, user: &str, allocation: Option<Value>) -> String {
    let id = project["id"].as_str().expect("a project id").to_string();
    admin(app, "POST", "/v1/admin/projects", Some(project)).await;
    admin(app, "POST", "/v1/admin/users", Some(json!({ "id": user }))).await;
    let mut body = json!({ "role": "member" });
    if let Some(allocation) = allocation {
        body["allocation"] = allocation;
    }
    admin(
        app,
        "PUT",
        &format!("/v1/admin/projects/{id}/members/{user}"),
        Some(body),
    )
    .await;
    id
}

/// A budgeted project with one member, and a freshly minted secret for them.
async fn budgeted_member(app: &Router, project: &str, user: &str) -> String {
    let id = tenancy(
        app,
        json!({
            "id": project,
            "budget": {
                "limit_usd": LIMIT_USD,
                "window": "total",
                "on_exhaustion": "degrade_to_local",
                // The valve armed, so a fleetless deployment promises nothing
                // it cannot keep and the boot cross-check is satisfied.
                "overflow_when_local_saturated": true,
            },
        }),
        user,
        None,
    )
    .await;
    let minted = admin(
        app,
        "POST",
        &format!("/v1/admin/projects/{id}/members/{user}/keys"),
        None,
    )
    .await;
    minted["secret"]
        .as_str()
        .expect("a minted secret")
        .to_string()
}

/// One turn on the native surface, asserted to have completed.
async fn turn(app: &Router, secret: &str, session: &str) {
    let (status, text) = send(
        app,
        "POST",
        "/v1/sessions",
        Some(secret),
        Some(json!({ "session_id": session })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "creating `{session}`: {text}");
    let (status, body) = send(
        app,
        "POST",
        &format!("/v1/sessions/{}/responses", path_segment(session)),
        Some(secret),
        Some(json!({ "turn_id": "t1", "input": [{ "role": "user", "text": "hello" }] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the turn: {body}");
    assert!(
        body.contains("event: response_completed"),
        "the turn did not complete: {body}"
    );
}

fn keys_of(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// Every number anywhere in a JSON document, however deeply nested.
///
/// Recursive rather than a scan of the top level, because "no field holds the
/// sum" has to be true of the stamps and the member rows too — a total hidden
/// one level down would be exactly as misleading as one at the root.
fn every_number(value: &Value, found: &mut Vec<f64>) {
    match value {
        Value::Number(number) => found.push(number.as_f64().unwrap_or(f64::NAN)),
        Value::Array(items) => items.iter().for_each(|item| every_number(item, found)),
        Value::Object(fields) => fields.values().for_each(|item| every_number(item, found)),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// The four milestone claims
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_minted_key_secret_is_returned_once_and_never_again() {
    // The property is about the *types*: `ApiKeyRecord` has no field a plaintext
    // could go in, so every read surface is structurally incapable of showing
    // one. This is the deployment-level check of that — asserted on raw response
    // bodies rather than on parsed fields, because a leak would most likely
    // arrive as a field nobody thought to parse.
    let rig = plain().await;
    let id = tenancy(&rig.app, json!({ "id": "globex" }), "bob", None).await;
    let minted = admin(
        &rig.app,
        "POST",
        &format!("/v1/admin/projects/{id}/members/bob/keys"),
        None,
    )
    .await;
    let secret = minted["secret"]
        .as_str()
        .expect("the mint returns a secret");
    let key_id = minted["id"].as_str().expect("and the row's id").to_string();

    // It is a real key of this deployment's own format, judged by the deployment's
    // own predicate rather than by a copy of it spelled here.
    assert!(
        has_valid_key_shape(secret),
        "a minted secret this deployment would itself refuse: {secret}"
    );
    assert!(secret.starts_with("rh_turn_"), "{secret}");
    // And it authenticates, which is the only claim that makes the rest matter.
    let (status, body) = send(&rig.app, "GET", "/v1/metrics", Some(secret), None).await;
    assert_eq!(status, StatusCode::OK, "the minted key must work: {body}");

    // Now every other read surface, on the raw body.
    for uri in [
        "/v1/admin/keys".to_string(),
        format!("/v1/admin/keys/{key_id}"),
        format!("/v1/admin/projects/{id}"),
        format!("/v1/admin/projects/{id}/members"),
        "/v1/admin/projects".to_string(),
    ] {
        let (status, text) = send(&rig.app, "GET", &uri, Some(&root()), None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {text}");
        assert!(
            !text.contains(secret),
            "`{uri}` handed the secret back a second time: {text}"
        );
    }

    // The control, and the reason the assertions above are not vacuous: the row
    // *is* on those surfaces, carrying everything about the key except the one
    // thing that must not be repeated.
    let listed = read(&rig.app, &format!("/v1/admin/keys/{key_id}")).await;
    assert_eq!(listed["id"], key_id.as_str());
    assert_eq!(listed["scope"], "turn");
    assert_eq!(listed["provenance"], "admin");
    assert_eq!(
        listed["display_tail"].as_str().expect("a tail"),
        &secret[secret.len() - 4..],
        "the tail is the operator's way to match a row against their secret \
         manager, so it has to be the secret's own last four characters"
    );
    assert!(
        listed.get("secret").is_none(),
        "the read surface has no secret field at all: {listed}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_a_key_stops_it_within_one_cache_ttl() {
    // The whole reason every surface holds a `PlaneSource` rather than a
    // compiled plane. The revocation is written through the admin router and
    // observed on a *different* router, which is what a router holding its own
    // captured plane would fail: the write happened on this node, so the
    // staleness bound is zero and "within one TTL" holds by construction. How
    // long a *second* node may disagree is the directory suite's two-view test.
    let rig = plain().await;
    let id = tenancy(&rig.app, json!({ "id": "globex" }), "bob", None).await;
    let minted = admin(
        &rig.app,
        "POST",
        &format!("/v1/admin/projects/{id}/members/bob/keys"),
        None,
    )
    .await;
    let secret = minted["secret"].as_str().expect("a secret").to_string();
    let key_id = minted["id"].as_str().expect("an id").to_string();

    let (status, body) = send(&rig.app, "GET", "/v1/metrics", Some(&secret), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the key admits before revocation: {body}"
    );

    let (status, _) = send(
        &rig.app,
        "DELETE",
        &format!("/v1/admin/keys/{key_id}"),
        Some(&root()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, code) =
        refused_as(&rig.app, "GET", "/v1/metrics", Some(secret.clone()), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        code, "revoked_key",
        "a revoked key must be told apart from one this deployment never \
         issued: an operator hunting a leak needs to see the thief still trying \
         it, and `unknown_key` reads as a typo"
    );

    // The two controls. A well-shaped secret nobody ever minted is still
    // `unknown_key`, so the row above is about revocation and not about every
    // refusal collapsing into one code; and the file's own key is untouched,
    // so revoking one key did not simply break authentication.
    let (_, code) = refused_as(&rig.app, "GET", "/v1/metrics", Some(key("nobody")), None).await;
    assert_eq!(code, "unknown_key");
    let (status, _) = send(&rig.app, "GET", "/v1/metrics", Some(&key("ada")), None).await;
    assert_eq!(status, StatusCode::OK);

    // And the tombstone is a row, not a deletion: an operator can still see that
    // the key existed and when it stopped working.
    let listed = read(&rig.app, &format!("/v1/admin/keys/{key_id}")).await;
    assert!(
        listed["revoked_at_ms"].is_number(),
        "a revoked key keeps its row: {listed}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_budget_view_reports_committed_and_measured_separately() {
    // Two numbers produced by two machines over two periods, published side by
    // side and never added. The ledger counts what a project was charged inside
    // its budget window; the metrics fold counts what this process measured it
    // spending since it started. A view that summed them would be quoting a
    // number with no referent, and a view that reported only one would be
    // hiding the disagreement this endpoint exists to surface.
    //
    // Two turns, the first eaten by `SwallowsOneSettle` -- so committed and
    // measured genuinely disagree here rather than only in principle. A
    // fixture where every turn settles leaves them equal, which makes
    // `drift_usd`'s own assertion below pass under a sign flip: `committed -
    // measured` and `measured - committed` are both zero when the two figures
    // match, so this broadest shape test would never catch that mutation.
    // `drift_goes_negative_and_stays_visible_when_a_settle_is_lost` stays the
    // primary, dedicated guard for the lost-settle behavior itself; this only
    // needs *some* real drift to make its own sign-sensitive assertion mean
    // something.
    let ledger = Arc::new(SwallowsOneSettle::default());
    let rig = rig(Arc::clone(&ledger) as Arc<dyn SpendLedger>).await;
    let secret = budgeted_member(&rig.app, "globex", "bob").await;
    turn(&rig.app, &secret, "globex/bob/main").await;
    turn(&rig.app, &secret, "globex/bob/second").await;
    assert!(
        ledger.swallowed(),
        "the double must have eaten exactly one settle, or this fixture is \
         back to zero drift and every assertion below passes vacuously"
    );

    let view = read(&rig.app, "/v1/admin/projects/globex/budget").await;

    // The exact key set. A field added without a decision is how a total gets
    // into a document that promises not to have one.
    assert_eq!(
        keys_of(&view),
        vec![
            "allocation_share_sum",
            "committed",
            "committed_usd",
            "drift_usd",
            "held_usd",
            "measured",
            "measured_usd",
            "members",
            "project",
            "seat_tokens",
        ],
        "{view}"
    );

    let committed = view["committed_usd"].as_f64().expect("a ledger figure");
    let measured = view["measured_usd"].as_f64().expect("a folded figure");
    assert_usd(
        committed,
        ACTUAL_TURN_USD,
        "the ledger's committed spend — only the second turn settled",
    );
    assert_usd(
        measured,
        2.0 * ACTUAL_TURN_USD,
        "the fold measures both turns, the swallowed one included",
    );

    // Independently correct: the folded figure is the same number the metrics
    // surface reports for this deployment, which has exactly one project with
    // traffic. Asserting it against a constant would only prove the constant.
    let metrics = read(&rig.app, "/v1/metrics").await;
    let deployment_spend = metrics["savings"]["frontier_spend_usd"]
        .as_f64()
        .expect("a deployment-wide figure");
    assert!(
        (measured - deployment_spend).abs() < EPSILON,
        "the project's measured column must be the same fold the dashboard \
         reads: {measured} vs {deployment_spend}"
    );

    // Every column stamped with where it came from and over what.
    assert_eq!(view["committed"]["basis"], "ledger");
    assert_eq!(view["committed"]["window"], "total");
    assert_eq!(view["committed"]["window_start_ms"], 0);
    assert_eq!(view["measured"]["basis"], "process-fold");
    assert_eq!(
        view["measured"]["window"], "lifetime",
        "the fold cannot window, and saying so is what keeps a reader from \
         comparing a month against a lifetime and calling the difference an error"
    );

    // The drift is the difference and is published as one.
    assert!(
        (view["drift_usd"].as_f64().expect("a drift") - (committed - measured)).abs() < EPSILON,
        "{view}"
    );

    // And nothing anywhere in the document is their sum.
    let sum = committed + measured;
    let mut numbers = Vec::new();
    every_number(&view, &mut numbers);
    assert!(
        !numbers.iter().any(|number| (number - sum).abs() < EPSILON),
        "some field of this document is `committed + measured` = {sum}, which is \
         a number with no referent: {view}"
    );

    // The member row follows the same discipline, and carries no `held_usd`:
    // the ledger's holds are project-wide and there is no honest per-member
    // decomposition of them.
    let member = &view["members"][0];
    assert_eq!(member["user"], "bob");
    assert_eq!(member["provenance"], "admin");
    assert_eq!(
        keys_of(member),
        vec![
            "allocation_share",
            "committed",
            "drift_usd",
            "measured",
            "measured_usd",
            "member_committed_usd",
            "member_remaining_usd",
            "provenance",
            "seat_tokens",
            "user",
        ],
        "{member}"
    );
    assert_usd(
        member["member_committed_usd"].as_f64().expect("a figure"),
        ACTUAL_TURN_USD,
        "the member's own committed spend",
    );
    assert!(
        member["member_remaining_usd"].is_null(),
        "a pooled membership has no *second* ceiling, which is not a ceiling of \
         zero: {member}"
    );

    // The dollar-free column is present rather than absent, so a dashboard can
    // always read it — zero here, because nothing in this fixture forwards a
    // subscription seat.
    assert_eq!(view["seat_tokens"]["total"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seat_tokens_are_visible_at_both_the_project_and_the_member_level() {
    // R5's dollar-free column, isolated. Every other test in this file drives
    // `file()`'s `acme`/`ada`, which bills through this deployment's own rate
    // card, so `seat_tokens.total` is legitimately `0` in every one of them —
    // and a mutation that zeroed the field, at either the project or the
    // member row, changed nothing any of those assertions checked.
    //
    // What this fixture does NOT cover: `faye`'s turn presents no forwarded
    // credential and degrades to local (mirroring `mcp_surface.rs`'s own
    // pass-through fixture, whose `seat/cleo` does the same for the same
    // reason — resolving a real forwarded credential runs through the
    // read-denied `credential.rs`, and this milestone's fixtures do not
    // exercise that path end to end). `Billing::of` reads the project's
    // *mode* rather than whether a credential arrived, so the billing
    // classification under test (`AccountedNotBilled`, landing in
    // `counters.seat`) is exercised correctly either way — but this test
    // cannot and does not distinguish "pass-through traffic is
    // counted-not-priced" from "local traffic is counted-not-priced", since
    // local traffic carries no dollars regardless of the project's mode.
    let rig = pass_through_rig().await;
    turn(&rig.app, &key("faye"), "forwarding/faye/main").await;

    let view = read(&rig.app, "/v1/admin/projects/forwarding/budget").await;
    let project_seat_tokens = view["seat_tokens"]["total"]
        .as_u64()
        .expect("a token count");
    assert!(
        project_seat_tokens > 0,
        "a turn under a forwarding project spends no dollars this deployment \
         may name, but it moves real tokens, and the project row is where an \
         operator looks first: {view}"
    );

    let member = &view["members"][0];
    assert_eq!(member["user"], "faye");
    let member_seat_tokens = member["seat_tokens"]["total"]
        .as_u64()
        .expect("a token count");
    assert!(member_seat_tokens > 0, "{view}");
    assert_eq!(
        member_seat_tokens, project_seat_tokens,
        "one member, one turn — the project's total is exactly hers, so a \
         mutation zeroing either level independently has to show up here: {view}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drift_goes_negative_and_stays_visible_when_a_settle_is_lost() {
    // The failure the whole view exists for. A settle that never lands is not
    // loud: the engine logs a warning and moves on, and every other surface goes
    // on reporting a project that has spent nothing. Only the gap between the
    // two columns says otherwise, and only if nothing clamps it.
    //
    // The double answers `Ok` rather than an error, which is the truer shape of
    // "silently swallowed": an error would at least produce a `tracing::warn`
    // somewhere. Here nothing anywhere has complained.
    let ledger = Arc::new(SwallowsOneSettle::default());
    let rig = rig(Arc::clone(&ledger) as Arc<dyn SpendLedger>).await;
    let secret = budgeted_member(&rig.app, "globex", "bob").await;
    turn(&rig.app, &secret, "globex/bob/main").await;
    assert!(
        ledger.swallowed(),
        "the double must have eaten a settle, or this test is about nothing"
    );

    let view = read(&rig.app, "/v1/admin/projects/globex/budget").await;
    let drift = view["drift_usd"].as_f64().expect("a drift figure");
    assert!(
        drift < 0.0,
        "a settle the ledger never saw must show as negative drift -- the fold \
         measured spend the ledger has no record of: {view}"
    );
    assert_usd(drift, -ACTUAL_TURN_USD, "the whole of the lost settle");
    assert_usd(
        view["committed_usd"].as_f64().expect("a committed figure"),
        0.0,
        "the ledger never saw the settle",
    );

    // Read again. A view that "repaired" the gap, or clamped it at zero, would
    // erase the only evidence the failure ever happened -- and the second read
    // is where a repair would show, because the first would have done it.
    let again = read(&rig.app, "/v1/admin/projects/globex/budget").await;
    let drift_again = again["drift_usd"].as_f64().expect("a drift figure");
    assert!(drift_again < 0.0, "{again}");
    assert!(
        (drift - drift_again).abs() < EPSILON,
        "reading the view must not move it: {drift} then {drift_again}"
    );
}

/// A ledger that loses exactly one settlement and reports success.
///
/// Everything else is forwarded, so the project's grants, its later settles and
/// its balances are the real in-memory ledger's — which is what makes the gap
/// this produces the size of one turn rather than the size of the fixture.
#[derive(Default)]
struct SwallowsOneSettle {
    inner: MemorySpendLedger,
    swallowed: AtomicBool,
}

impl SwallowsOneSettle {
    fn swallowed(&self) -> bool {
        self.swallowed.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SpendLedger for SwallowsOneSettle {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        self.inner.open_grant(request).await
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        if self
            .swallowed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // Reported as applied, and nothing was. The hold is left standing,
            // which is what a lost settle does to a real ledger too.
            return Ok(Settled {
                applied: true,
                released_usd: 0.0,
                committed_usd: 0.0,
            });
        }
        self.inner.settle_grant(settlement).await
    }

    async fn balance(&self, query: BalanceQuery) -> Result<Balance, SpendError> {
        self.inner.balance(query).await
    }
}

// ---------------------------------------------------------------------------
// Who may reach the surface
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_key_is_refused_on_the_admin_surface() {
    // The mirror image of `mcp_surface`'s
    // `an_admin_key_cannot_call_the_mcp_surface`: each surface refuses the other
    // key kind, through the same `ControlPlane::scope`, so neither has a key
    // vocabulary of its own to get subtly wrong.
    let rig = plain().await;
    for (method, uri, body) in [
        ("GET", "/v1/admin/projects", None),
        (
            "POST",
            "/v1/admin/projects",
            Some(json!({ "id": "sneaky" })),
        ),
        ("GET", "/v1/admin/projects/acme/budget", None),
    ] {
        let (status, code) = refused_as(&rig.app, method, uri, Some(key("ada")), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(code, "wrong_key_kind", "{method} {uri}");
    }

    // The controls: no key at all is a different answer, and the admin key
    // works — so the refusal above is about the *kind* of key rather than about
    // the surface being closed.
    let (status, code) = refused_as(&rig.app, "GET", "/v1/admin/projects", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code, "missing_key");
    read(&rig.app, "/v1/admin/projects").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_mode_refuses_the_admin_surface_with_a_named_error() {
    // Mode first, and the code is the whole assertion. In open mode every
    // request resolves to the built-in membership with no key at all, so a
    // surface that authenticated before checking the mode would answer
    // `wrong_key_kind` — "use a different key" — to a deployment where no key
    // exists and none can be issued, because the file is the only root of trust
    // an admin key can come from.
    let app = admin_api::admin_router(
        ControlDirectory::open(),
        Arc::new(MemorySpendLedger::new()),
        Arc::new(roundhouse_core::metrics::MetricsRecorder::new()),
        metrics_config(),
    );
    for (method, uri, secret) in [
        ("GET", "/v1/admin/projects", None),
        ("GET", "/v1/admin/projects", Some(root())),
        ("POST", "/v1/admin/keys", Some(root())),
        ("GET", "/v1/admin/projects/acme/budget", Some(key("ada"))),
    ] {
        let (status, code) = refused_as(&app, method, uri, secret, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(
            code, "admin_requires_control_plane",
            "{method} {uri} must name the deployment's missing root of trust, \
             not the caller's key"
        );
    }
}

// ---------------------------------------------------------------------------
// The rules the surface enforces
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_config_owned_project_cannot_be_patched() {
    // One owner per entity. The file declares `acme`, so every change to it is
    // refused naming the file -- an API change that shadowed the file would be
    // undone by the next restart, silently, which is the failure furthest in
    // time from its cause.
    let rig = plain().await;
    for (method, uri, body) in [
        (
            "PATCH",
            "/v1/admin/projects/acme".to_string(),
            Some(json!({ "name": "Renamed" })),
        ),
        ("DELETE", "/v1/admin/projects/acme".to_string(), None),
        (
            "POST",
            "/v1/admin/projects/acme/members/ada/keys".to_string(),
            None,
        ),
    ] {
        let (status, code) = refused(&rig.app, method, &uri, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {uri}");
        assert_eq!(code, "config_owned", "{method} {uri}");
    }

    // The message has to name the remedy, which is a file and not this API.
    let (_, text) = send(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/acme",
        Some(&root()),
        Some(json!({ "name": "Renamed" })),
    )
    .await;
    assert!(
        text.contains("ROUNDHOUSE_CONTROL_PLANE"),
        "the refusal has to name the document an operator would go and edit: {text}"
    );

    // The controls, and they are the whole reason config-before-CRUD is a
    // workable rule rather than a wall. A *new* member of the configured
    // project is a create, not a change, and it works -- as does a key under
    // that new membership, which is the escape hatch for an operator whose
    // memberships are all in the file.
    admin(
        &rig.app,
        "POST",
        "/v1/admin/users",
        Some(json!({ "id": "cleo" })),
    )
    .await;
    admin(
        &rig.app,
        "PUT",
        "/v1/admin/projects/acme/members/cleo",
        Some(json!({ "role": "owner" })),
    )
    .await;
    let minted = admin(
        &rig.app,
        "POST",
        "/v1/admin/projects/acme/members/cleo/keys",
        None,
    )
    .await;
    let secret = minted["secret"].as_str().expect("a secret");
    let (status, body) = send(&rig.app, "GET", "/v1/metrics", Some(secret), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a key minted under a new membership of a configured project must \
         authenticate: {body}"
    );

    // And the file's own project is listed, labelled with who owns it -- an
    // operator has to be able to see which half a row came from before they try
    // to edit it.
    let projects = read(&rig.app, "/v1/admin/projects").await;
    let acme = projects["data"]
        .as_array()
        .expect("a list")
        .iter()
        .find(|project| project["id"] == "acme")
        .expect("the file's project is listed");
    assert_eq!(acme["provenance"], "config");

    // The budget view over a project whose members come from *both* halves.
    // `ada` is declared in the file and her membership is projected from its
    // `keys` array rather than read from the store; `cleo` was created through
    // the API a few lines up. A view that enumerated only the store's rows would
    // silently omit every file-declared member -- which, on a deployment whose
    // tenancy is mostly in the file, is most of them.
    let view = read(&rig.app, "/v1/admin/projects/acme/budget").await;
    let members = view["members"].as_array().expect("a member list");
    let ada = members
        .iter()
        .find(|member| member["user"] == "ada")
        .expect("the file's own member is in the view");
    assert_eq!(ada["provenance"], "config");
    let cleo = members
        .iter()
        .find(|member| member["user"] == "cleo")
        .expect("and the API-created one beside it");
    assert_eq!(cleo["provenance"], "admin");

    // The discriminating assertion. `acme` declares no budget, so `ada`'s row is
    // `unenforced` -- she has a key and is spending, and nothing is counting it.
    // `no_keys` here would mean the projected membership had not resolved to the
    // file's own admission at all, which is the way this projection fails.
    assert_eq!(
        ada["committed"]["basis"], "unenforced",
        "a file-declared member has a key, so `no_keys` would mean the view \
         could not resolve her admission: {view}"
    );
    assert!(ada["member_committed_usd"].is_null(), "{view}");
    assert_eq!(view["committed"]["basis"], "unenforced");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oauth_shaped_credential_is_refused_with_a_reason() {
    // There is no credential CRUD in this milestone, and the route exists so
    // that fact is discoverable from the API rather than from a 404 that reads
    // like a typo. The two refusals are different sizes: an OAuth body is asking
    // this deployment to hold a credential with a lifecycle, which is refused on
    // its own terms; anything else is "this build cannot do that".
    let rig = plain().await;
    for body in [
        json!({ "kind": "oauth" }),
        json!({ "refresh_token": "rt_abc" }),
        json!({ "id_token": "it_abc" }),
        json!({ "client_id": "ci_abc" }),
    ] {
        let (status, code) = refused(
            &rig.app,
            "POST",
            "/v1/admin/credentials",
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(code, "oauth_credentials_unsupported", "{body}");
    }

    let (_, text) = send(
        &rig.app,
        "POST",
        "/v1/admin/credentials",
        Some(&root()),
        Some(json!({ "kind": "oauth" })),
    )
    .await;
    assert!(
        text.contains("pass-through"),
        "the refusal has to name the arrangement that *is* supported, or it \
         reads as 'forwarded logins are impossible here': {text}"
    );

    // The control, and the reason the shape check is not just a tag check: an
    // ordinary credential body is a different refusal, at a different status.
    let (status, code) = refused(
        &rig.app,
        "POST",
        "/v1/admin/credentials",
        Some(json!({ "provider": "anthropic", "api_key": "sk-x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(code, "credential_crud_not_available");
    let (_, text) = send(
        &rig.app,
        "POST",
        "/v1/admin/credentials",
        Some(&root()),
        Some(json!({ "provider": "anthropic" })),
    )
    .await;
    assert!(
        text.contains("ROUNDHOUSE_CONTROL_PLANE"),
        "and it has to name the mechanism that does work: {text}"
    );
}

// ---------------------------------------------------------------------------
// What the view says when there is nothing to say
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_view_reports_null_not_zero_for_an_unbudgeted_project() {
    // `0.0` and "nothing is counting" look identical on a dashboard, and only
    // one of them is a state somebody needs to fix. The engine skips the ledger
    // entirely for an unbudgeted admission, so there is no position to read and
    // none to invent.
    let rig = plain().await;
    let secret = {
        tenancy(&rig.app, json!({ "id": "openhanded" }), "bob", None).await;
        let minted = admin(
            &rig.app,
            "POST",
            "/v1/admin/projects/openhanded/members/bob/keys",
            None,
        )
        .await;
        minted["secret"].as_str().expect("a secret").to_string()
    };
    turn(&rig.app, &secret, "openhanded/bob/main").await;

    let view = read(&rig.app, "/v1/admin/projects/openhanded/budget").await;
    assert!(view["committed_usd"].is_null(), "{view}");
    assert!(view["held_usd"].is_null(), "{view}");
    assert!(
        view["drift_usd"].is_null(),
        "drift is a difference from a number that does not exist: {view}"
    );
    assert_eq!(view["committed"]["basis"], "unenforced");
    assert!(view["committed"]["window_start_ms"].is_null(), "{view}");

    // And the measured column is still real, which is the point of publishing
    // the two apart: an unbudgeted project is spending money, and the fold saw
    // it even though nothing metered it.
    assert!(
        view["measured_usd"].as_f64().expect("a folded figure") > 0.0,
        "an unbudgeted project still spends, and the fold still measures it: {view}"
    );
    assert_eq!(view["measured"]["basis"], "process-fold");
    let member = &view["members"][0];
    assert_eq!(member["committed"]["basis"], "unenforced");
    assert!(member["member_committed_usd"].is_null(), "{member}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keyless_membership_is_reported_as_having_no_keys_rather_than_unenforced() {
    // The third basis, and it is not a nicety. A membership with no key has no
    // admission, and therefore no `BudgetTerms` to read a balance under -- which
    // looks, from the view's side, exactly like a membership whose project has
    // no budget. The two are opposites: `unenforced` means somebody is spending
    // and nothing is counting it, `no_keys` means nobody can spend at all. One
    // label for both would send an operator to fix the wrong thing.
    let rig = plain().await;
    tenancy(
        &rig.app,
        json!({
            "id": "globex",
            "budget": {
                "limit_usd": LIMIT_USD,
                "window": "monthly",
                "on_exhaustion": "refuse",
            },
        }),
        "bob",
        None,
    )
    .await;

    let view = read(&rig.app, "/v1/admin/projects/globex/budget").await;
    let member = &view["members"][0];
    assert_eq!(member["user"], "bob");
    assert_eq!(
        member["committed"]["basis"], "no_keys",
        "a membership with no key has never spent anything, which is not the \
         same fact as a project nothing meters: {member}"
    );
    assert!(member["member_committed_usd"].is_null(), "{member}");
    assert!(member["drift_usd"].is_null(), "{member}");
    // The project inherits it: no member of this project can spend at all.
    assert_eq!(view["committed"]["basis"], "no_keys");
    assert!(view["committed_usd"].is_null(), "{view}");

    // The control, and the whole point of the distinction: mint a key for that
    // same membership and the basis becomes the ledger's, with a real window.
    admin(
        &rig.app,
        "POST",
        "/v1/admin/projects/globex/members/bob/keys",
        None,
    )
    .await;
    let view = read(&rig.app, "/v1/admin/projects/globex/budget").await;
    assert_eq!(view["committed"]["basis"], "ledger");
    assert_eq!(view["committed"]["window"], "monthly");
    assert_usd(
        view["committed_usd"].as_f64().expect("a ledger figure"),
        0.0,
        "a budgeted project that has spent nothing reports zero, not null",
    );
    assert!(
        view["committed_usd"]
            .as_f64()
            .expect("a figure")
            .is_sign_positive(),
        "the memory ledger sums an empty hold list to -0.0, which serialises as \
         `-0.0` and reads as nonsense: {view}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_allocation_share_sum_is_reported_not_refused() {
    // Shares are allowed to sum past 1.0 and this view says so rather than
    // objecting. The project's own limit binds regardless, so over-subscription
    // is a real arrangement an operator may want -- five people who will not all
    // spend at once -- and what they are owed is being able to see it.
    let rig = plain().await;
    tenancy(
        &rig.app,
        json!({
            "id": "globex",
            "budget": {
                "limit_usd": LIMIT_USD,
                "window": "total",
                "on_exhaustion": "degrade_to_local",
                "overflow_when_local_saturated": true,
            },
        }),
        "bob",
        Some(json!({ "share": { "fraction": 0.7 } })),
    )
    .await;
    admin(
        &rig.app,
        "POST",
        "/v1/admin/users",
        Some(json!({ "id": "cleo" })),
    )
    .await;
    admin(
        &rig.app,
        "PUT",
        "/v1/admin/projects/globex/members/cleo",
        Some(json!({ "role": "member", "allocation": { "share": { "fraction": 0.6 } } })),
    )
    .await;
    for user in ["bob", "cleo"] {
        admin(
            &rig.app,
            "POST",
            &format!("/v1/admin/projects/globex/members/{user}/keys"),
            None,
        )
        .await;
    }

    let view = read(&rig.app, "/v1/admin/projects/globex/budget").await;
    let sum = view["allocation_share_sum"]
        .as_f64()
        .expect("a reported sum");
    assert!(
        (sum - 1.3).abs() < EPSILON,
        "the sum is reported as it is, over-subscription and all: {view}"
    );
    let mut shares: Vec<f64> = view["members"]
        .as_array()
        .expect("two members")
        .iter()
        .map(|member| member["allocation_share"].as_f64().expect("a share"))
        .collect();
    shares.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    assert!(
        (shares[0] - 0.6).abs() < EPSILON && (shares[1] - 0.7).abs() < EPSILON,
        "{view}"
    );

    // Each member's own ceiling is the share of the project limit, and both are
    // reported -- which is what makes the sum readable as an arrangement rather
    // than as a number floating free of anything.
    for member in view["members"].as_array().expect("two members") {
        let share = member["allocation_share"].as_f64().expect("a share");
        assert_usd(
            member["member_remaining_usd"].as_f64().expect("a ceiling"),
            LIMIT_USD * share,
            "an unspent share is the whole of it",
        );
    }

    // The control: a project whose members are pooled reports no sum at all,
    // rather than 0.0 -- there is no share to add up, which is not the same as
    // the shares adding to nothing.
    let rig = plain().await;
    let _ = budgeted_member(&rig.app, "pooled", "bob").await;
    let view = read(&rig.app, "/v1/admin/projects/pooled/budget").await;
    assert!(view["allocation_share_sum"].is_null(), "{view}");
    assert!(view["members"][0]["allocation_share"].is_null(), "{view}");
}

// ---------------------------------------------------------------------------
// The routes' own shapes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_project_is_a_404_and_never_a_document_of_zeros() {
    // No fail-open. A view that answered a well-formed document full of zeros
    // for a project that does not exist would report an unspent budget to
    // whoever was about to decide whether to raise one.
    let rig = plain().await;
    for uri in [
        "/v1/admin/projects/nosuch",
        "/v1/admin/projects/nosuch/budget",
        "/v1/admin/projects/nosuch/members",
    ] {
        let (status, code) = refused(&rig.app, "GET", uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(code, "project_not_found", "{uri}");
    }
    let (status, code) = refused(&rig.app, "GET", "/v1/admin/keys/key_nosuch", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code, "key_not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_budget_window_change_is_refused_over_http_naming_the_mechanism() {
    // The one `PATCH` axis that is refused, and the reason has to travel with
    // the refusal: committed spend is counted *within* a window, so moving one
    // either zeroes what a project has already spent or reinterprets a total as
    // a month. A 400 rather than a 422 because the body is not unprocessable --
    // it is a change this API declines to make at all.
    let rig = plain().await;
    tenancy(
        &rig.app,
        json!({
            "id": "globex",
            "budget": { "limit_usd": LIMIT_USD, "window": "total", "on_exhaustion": "refuse" },
        }),
        "bob",
        // A share of the whole limit, so the member's own ceiling tracks the
        // project's -- which is what makes "the raised limit is in force" a
        // number this view actually prints.
        Some(json!({ "share": { "fraction": 1.0 } })),
    )
    .await;

    let (status, code) = refused(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/globex",
        Some(json!({
            "budget": { "limit_usd": LIMIT_USD, "window": "monthly", "on_exhaustion": "refuse" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code, "window_change_unsupported");
    let (_, text) = send(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/globex",
        Some(&root()),
        Some(json!({
            "budget": { "limit_usd": LIMIT_USD, "window": "monthly", "on_exhaustion": "refuse" }
        })),
    )
    .await;
    assert!(
        text.contains("committed spend") && text.contains("window"),
        "the refusal has to name the mechanism, or it reads as an arbitrary \
         restriction somebody will ask to have lifted: {text}"
    );

    // The control: the same budget on the same window, with a different limit,
    // is a change this API makes happily -- and it takes effect, which is what
    // the recompile is for.
    admin(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/globex",
        Some(json!({
            "budget": { "limit_usd": 25.0, "window": "total", "on_exhaustion": "refuse" }
        })),
    )
    .await;
    admin(
        &rig.app,
        "POST",
        "/v1/admin/projects/globex/members/bob/keys",
        None,
    )
    .await;
    let view = read(&rig.app, "/v1/admin/projects/globex/budget").await;
    assert_usd(
        view["members"][0]["member_remaining_usd"]
            .as_f64()
            .expect("a share of the project limit"),
        25.0,
        "the raised limit is in force, which is what the recompile is for",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mutation_this_deployment_could_not_boot_under_is_refused_naming_the_check() {
    // Every write re-runs the boot cross-checks, because a runtime-minted key
    // has to be exactly as validated as a boot-loaded one. Without this, an
    // admin could write a configuration the process would refuse to restart
    // under -- the failure furthest in time from its cause.
    // The check walks *keys*, not projects -- a policy admits nothing only for
    // somebody, and a project nobody can authenticate as has no admission to
    // judge. So the write that trips it is the mint, which is also the write
    // that would have created the unservable state.
    let rig = plain().await;
    tenancy(
        &rig.app,
        // Names a provider this deployment's catalog does not have, so the
        // policy admits nothing at all.
        json!({ "id": "nowhere", "policy": { "allow": ["openai/*"] } }),
        "bob",
        None,
    )
    .await;
    let (status, code) = refused(
        &rig.app,
        "POST",
        "/v1/admin/projects/nowhere/members/bob/keys",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        code, "refuse_policies_that_admit_nothing",
        "the code is the check's own name, so the sentence in the boot log and \
         the code on the wire name one rule"
    );
    let (_, text) = send(
        &rig.app,
        "POST",
        "/v1/admin/projects/nowhere/members/bob/keys",
        Some(&root()),
        None,
    )
    .await;
    assert!(
        text.contains("project `nowhere`, user `bob`"),
        "the refusal has to name the entry an operator would go and fix: {text}"
    );

    // And nothing was written: a refused mint must leave no key behind, or the
    // deployment now holds a secret it will refuse to start under.
    let keys = read(&rig.app, "/v1/admin/keys").await;
    assert!(
        !keys["data"]
            .as_array()
            .expect("a list")
            .iter()
            .any(|row| row["project"] == "nowhere"),
        "{keys}"
    );

    // The control: the same shape under a policy that names the catalog.
    tenancy(
        &rig.app,
        json!({ "id": "somewhere", "policy": { "allow": ["anthropic/*"] } }),
        "cleo",
        None,
    )
    .await;
    admin(
        &rig.app,
        "POST",
        "/v1/admin/projects/somewhere/members/cleo/keys",
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archiving_a_project_stops_its_keys_and_keeps_its_history_readable() {
    // Archived, never deleted, and terminal in this milestone. The keys stop
    // working with a *different* code from a revoked one, because the remedies
    // are opposite; and the budget view still answers, because a project's spend
    // history outliving the project is the entire reason archiving is not
    // deletion.
    let rig = plain().await;
    let secret = budgeted_member(&rig.app, "globex", "bob").await;
    turn(&rig.app, &secret, "globex/bob/main").await;

    let (status, _) = send(
        &rig.app,
        "DELETE",
        "/v1/admin/projects/globex",
        Some(&root()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, code) = refused_as(&rig.app, "GET", "/v1/metrics", Some(secret), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        code, "project_archived",
        "a fleet of keys going dark at once reads very differently from one key \
         going dark, and the two must not share a code"
    );

    // The view still answers, and says why its committed column is empty: there
    // is no admission left to take terms from, so the honest answer is `null`
    // with its own basis rather than a zero.
    let view = read(&rig.app, "/v1/admin/projects/globex/budget").await;
    assert_eq!(view["committed"]["basis"], "archived");
    assert!(view["committed_usd"].is_null(), "{view}");
    assert!(
        view["measured_usd"].as_f64().expect("a folded figure") > 0.0,
        "the spend outlives the project, which is why the row is kept at all: {view}"
    );

    // And archiving is final: nothing may be created under the id again.
    let (status, code) = refused(
        &rig.app,
        "POST",
        "/v1/admin/projects",
        Some(json!({ "id": "globex" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code, "identity_collision");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_membership_revokes_the_keys_minted_under_it() {
    // The cascade, over HTTP. A key whose membership is gone resolves to no
    // policy, no budget and no principal, so leaving it live would be a secret
    // that authenticates as nothing.
    let rig = plain().await;
    let id = tenancy(&rig.app, json!({ "id": "globex" }), "bob", None).await;
    let minted = admin(
        &rig.app,
        "POST",
        &format!("/v1/admin/projects/{id}/members/bob/keys"),
        None,
    )
    .await;
    let secret = minted["secret"].as_str().expect("a secret").to_string();
    let (status, _) = send(&rig.app, "GET", "/v1/metrics", Some(&secret), None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = send(
        &rig.app,
        "DELETE",
        &format!("/v1/admin/projects/{id}/members/bob"),
        Some(&root()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, code) = refused_as(&rig.app, "GET", "/v1/metrics", Some(secret), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code, "revoked_key");
    // Revoked and not dropped, so the operator who removed the member can still
    // see that the key existed and stopped working -- which is the question
    // they will have.
    let keys = read(&rig.app, "/v1/admin/keys").await;
    assert!(
        keys["data"]
            .as_array()
            .expect("a list")
            .iter()
            .any(|row| row["revoked_at_ms"].is_number()),
        "{keys}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_minted_admin_key_administers_and_a_config_owned_one_cannot_be_revoked() {
    // The bootstrap story, both halves. An admin key minted here is a real one;
    // and the file's own admin key cannot be revoked through this API, which is
    // what makes locking a deployment out of its own admin plane impossible by
    // construction rather than by a "refuse the last key" rule guarding a state
    // no sequence of calls reaches.
    let rig = plain().await;
    let minted = admin(&rig.app, "POST", "/v1/admin/keys", None).await;
    let secret = minted["secret"].as_str().expect("a secret").to_string();
    assert!(secret.starts_with("rh_admin_"), "{secret}");
    assert!(has_valid_key_shape(&secret), "{secret}");
    assert_eq!(minted["scope"], "admin");
    assert!(
        minted["project"].is_null(),
        "an admin belongs to no project"
    );

    let (status, body) = send(&rig.app, "GET", "/v1/admin/projects", Some(&secret), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The file's admin key, found by the id every key row carries, is refused.
    let keys = read(&rig.app, "/v1/admin/keys").await;
    let config_owned = keys["data"]
        .as_array()
        .expect("a list")
        .iter()
        .find(|row| row["scope"] == "admin" && row["provenance"] == "config")
        .expect("the file's admin key is listed")
        .clone();
    let (status, code) = refused(
        &rig.app,
        "DELETE",
        &format!(
            "/v1/admin/keys/{}",
            config_owned["id"].as_str().expect("an id")
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code, "config_owned");

    // And the API-minted one *can* be revoked, which is what makes the refusal
    // above about ownership rather than about admin keys being special.
    let (status, _) = send(
        &rig.app,
        "DELETE",
        &format!("/v1/admin/keys/{}", minted["id"].as_str().expect("an id")),
        Some(&root()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, code) =
        refused_as(&rig.app, "GET", "/v1/admin/projects", Some(secret), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code, "revoked_key");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_directory_write_reaches_every_surface_and_not_just_the_admin_one() {
    // The ripple's own claim, stated as a test rather than left to the four
    // routers' constructors. A project and a member created over the admin
    // plane are immediately spendable on the native surface and readable on the
    // metrics one -- because all of them resolve through the same directory per
    // request rather than through a plane captured when the router was mounted.
    let rig = plain().await;
    let before = rig.directory.version(now_ms());
    let secret = budgeted_member(&rig.app, "globex", "bob").await;
    assert!(
        rig.directory.version(now_ms()) > before,
        "the writes must have moved the store's version"
    );

    turn(&rig.app, &secret, "globex/bob/main").await;
    let (status, body) = send(&rig.app, "GET", "/v1/metrics", Some(&secret), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let metrics: Value = serde_json::from_str(&body).expect("JSON");
    assert!(
        metrics["savings"]["frontier_spend_usd"]
            .as_f64()
            .expect("a figure")
            > 0.0,
        "the new member's own turn is visible on the metrics surface: {body}"
    );
}
