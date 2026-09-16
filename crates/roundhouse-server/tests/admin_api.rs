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
    Balance, BalanceQuery, DocumentStore, Grant, GrantRequest, MemoryDocumentStore,
    MemorySpendLedger, Settled, Settlement, SpendError, SpendLedger,
};
use roundhouse_core::metrics::{MetricsConfig, MetricsRecorder, ReferenceModel, ShadowPricing};
use roundhouse_core::now_ms;
use roundhouse_core::routing::{
    AffinityPolicy, Candidate, ProviderPricing, Target, policy::Weights,
};
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::{
    EchoFrontierClient, FrontierModelSpec, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::control_config::crosscheck::CrossChecks;
use roundhouse_server::test_support::{
    ScriptedDirectoryStore, frontier_spec, single_model_catalog,
};
use roundhouse_server::{
    ControlDirectory, Conversations, DirectoryStore, DocumentDirectoryStore, EchoLocalExecutor,
    Engine, EngineConfig, admin_api, has_valid_key_shape, http, metrics_api, responses_api,
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
///
/// [`single_model_catalog`] (M15, H2): one of the eleven fixtures of this
/// exact shape the rung named.
fn catalog() -> StaticFrontierCatalog {
    single_model_catalog(FrontierModelSpec {
        pricing: ProviderPricing {
            input_per_mtok_usd: 0.0,
            cached_input_per_mtok_usd: 0.0,
            cache_write_per_mtok_usd: 0.0,
            output_per_mtok_usd: OUTPUT_PER_MTOK_USD,
        },
        base_ttft_ms: 1.0,
        ttft_ms_per_uncached_token: 0.0,
        ..frontier_spec("anthropic", "claude", WireProtocol::AnthropicMessages)
    })
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
    rig_over(file(), ledger, Arc::new(MemoryDocumentStore::new())).await
}

/// The same deployment, over a document store the caller already holds.
///
/// **For the one test that boots twice** —
/// [`recreating_an_archived_project_after_a_restart_inherits_its_spend`] — and
/// the sharing belongs here, in the rig, rather than in that test's body: what
/// the test is about is that a restart inherits the directory the way it
/// already inherits the ledger, and a fixture that assembled the second boot's
/// directory by hand would be asserting against a wiring nothing else in the
/// process uses. One argument, two boots, and the composition each boot
/// performs is identical.
async fn rig_sharing(ledger: Arc<dyn SpendLedger>, documents: Arc<dyn DocumentStore>) -> Rig {
    rig_over(file(), ledger, documents).await
}

/// The same deployment over a caller-supplied control-plane file.
///
/// Split out for G14's key view, which needs a *file-declared* key carrying a
/// member `fair_use` block — the only provenance under which one can exist.
/// Putting that block on the shared [`file`] instead would have changed what
/// every other test in this module reads back from `GET /v1/admin/keys`, which
/// is the hazard `pass_through_file` already keeps its own fixture for.
async fn rig_over(
    file: roundhouse_server::ControlPlaneConfig,
    ledger: Arc<dyn SpendLedger>,
    documents: Arc<dyn DocumentStore>,
) -> Rig {
    ensure_rustls_crypto_provider();
    let directory = Arc::new(
        ControlDirectory::new(
            file,
            "ROUNDHOUSE_CONTROL_PLANE",
            Arc::new(DocumentDirectoryStore::over(documents)),
            CrossChecks::new(reachable(), None),
            now_ms(),
        )
        .await
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
            Arc::new(DocumentDirectoryStore::over(Arc::new(
                MemoryDocumentStore::new(),
            ))),
            CrossChecks::new(reachable(), None),
            now_ms(),
        )
        .await
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

/// One turn on the native surface, under a caller-chosen `turn_id`, so more
/// than one turn can be driven through the same session. Asserted to have
/// completed.
///
/// Session creation is idempotent (`POST /v1/sessions` adopting an id that
/// already exists is a successful retry), so this is safe to call more than
/// once for the same `session`.
async fn turn_with_id(app: &Router, secret: &str, session: &str, turn_id: &str) {
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
        Some(json!({ "turn_id": turn_id, "input": [{ "role": "user", "text": "hello" }] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the turn `{turn_id}`: {body}");
    assert!(
        body.contains("event: response_completed"),
        "the turn `{turn_id}` did not complete: {body}"
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

// ---------------------------------------------------------------------------
// R2, over the wire: budget_view's plane and view must be one call
// ---------------------------------------------------------------------------

/// The double this section arms is [`ScriptedDirectoryStore`]
/// (`roundhouse_server::test_support`, M16.0 review, F1) -- the same wrapper
/// over the real production store the directory suite's own coherence and
/// M16.0 guards use, rather than a second hand-rolled `(records, version)`
/// double with its own copy of `commit`'s compare-and-set.
///
/// `ScriptedDirectoryStore::arm` lands a staged write at an exact
/// `version()` call count from an armed instant, rather than at whichever
/// call happens to be second ever -- which is what lets this survive contact
/// with the HTTP boundary: `admin_auth_layer` reads the plane once, ahead of
/// every handler on this router (see its own doc comment), so a fixed call
/// count burns unpredictably on setup traffic before the request under test
/// ever fires. `arm` resets the count the instant the test is ready, so the
/// write lands `land_at` reads later regardless of how many earlier requests
/// this store has already answered.
///
/// The directory suite's own coherence guard
/// (`control_config::directory::tests::budget_view_s_plane_and_view_must_describe_the_same_version`)
/// proves `ControlDirectory::snapshot` itself is atomic against this failure
/// mode, by timing a write on the store's second `version()` call, ever --
/// that pins the *primitive*, and never reaches `budget_view`, because that
/// guard drives `snapshot` directly and has no seam at which to watch what
/// happens if a caller stopped using it. Nothing else in this suite routes
/// through the handler with a write timed to land between two independent
/// reads either, which is why a regression from its one
/// `state.directory.snapshot(at_ms)` back to two separate
/// `state.directory.plane(at_ms)` / `.view(at_ms)` calls would compile and
/// pass every other test here.

/// R2 (thermo-nuclear review, M8), reproduced at the HTTP boundary: nothing
/// else in this suite drives the real router with a write timed to land
/// between `budget_view`'s plane read and its listing read, so a regression
/// from its one `state.directory.snapshot(at_ms)` back to two independent
/// `plane(at_ms)` / `view(at_ms)` calls would compile and pass everything
/// else here.
///
/// The fixture is built so the coherent answers and the incoherent one are
/// all visibly different in the response body, not merely internally
/// inconsistent in a way an HTTP test has no handle on. `before` is a
/// project and a user with no membership between them at all; `after` adds
/// the membership *and* its live turn key in one staged write, the way a
/// real mint produces both together. A coherent read of `before` therefore
/// omits bob's row entirely -- the membership does not exist yet. A coherent
/// read of `after` resolves his admission and reports a real basis
/// (`unenforced`, since this project has no budget) -- never `no_keys`,
/// which this module reserves for a membership with no *live* admission. The
/// only way to see bob's row **and** `no_keys` together is a plane that has
/// not heard about him yet paired with a listing that has -- exactly the
/// split [`ScriptedDirectoryStore::arm`] is armed to produce.
///
/// **Reach:** this pins the call order the R2 regression actually had --
/// `plane(at_ms)` before `view(at_ms)`. A handler regressed to the opposite
/// order would iterate a listing that is still `before` (no row for bob at
/// all) and would pass this guard green; that direction is not covered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_view_over_http_reads_plane_and_view_from_one_version() {
    ensure_rustls_crypto_provider();

    // `before`/`after`, staged on a throwaway router over a plain store --
    // exactly what a real project, member and mint produce, rather than
    // hand-built records that could differ from one in the way that makes
    // this test pass for the wrong reason.
    let seed_store = Arc::new(DocumentDirectoryStore::over(Arc::new(
        MemoryDocumentStore::new(),
    )));
    let seed_directory = Arc::new(
        ControlDirectory::new(
            control_plane(
                json!({ "projects": [], "users": [], "admin_keys": [sha256_hex(&root())] }),
                "R2 http-guard seed",
            ),
            "ROUNDHOUSE_CONTROL_PLANE",
            Arc::clone(&seed_store) as Arc<dyn DirectoryStore>,
            CrossChecks::new(reachable(), None),
            now_ms(),
        )
        .await
        .expect("admin_keys alone compiles"),
    );
    let seed_app = admin_api::admin_router(
        Arc::clone(&seed_directory),
        Arc::new(MemorySpendLedger::new()) as Arc<dyn SpendLedger>,
        Arc::new(MetricsRecorder::new()),
        metrics_config(),
    );
    admin(
        &seed_app,
        "POST",
        "/v1/admin/projects",
        Some(json!({ "id": "widgets" })),
    )
    .await;
    admin(
        &seed_app,
        "POST",
        "/v1/admin/users",
        Some(json!({ "id": "bob" })),
    )
    .await;
    let before = seed_store
        .load()
        .await
        .expect("the seed store answers its own writes")
        .records;
    admin(
        &seed_app,
        "PUT",
        "/v1/admin/projects/widgets/members/bob",
        Some(json!({ "role": "member" })),
    )
    .await;
    admin(
        &seed_app,
        "POST",
        "/v1/admin/projects/widgets/members/bob/keys",
        None,
    )
    .await;
    let after = seed_store
        .load()
        .await
        .expect("the seed store answers its own writes")
        .records;

    // The router under test, over the armed double. `admission_cache_ttl_ms:
    // 0` so every `plane`/`view` call re-asks the store instead of answering
    // from a cache that would hide the whole race.
    let armed = Arc::new(ScriptedDirectoryStore::new(before, 1).await);
    let directory = Arc::new(
        ControlDirectory::new(
            control_plane(
                json!({
                    "projects": [], "users": [],
                    "admin_keys": [sha256_hex(&root())],
                    "admission_cache_ttl_ms": 0,
                }),
                "R2 http-guard target",
            ),
            "ROUNDHOUSE_CONTROL_PLANE",
            Arc::clone(&armed) as Arc<dyn DirectoryStore>,
            CrossChecks::new(reachable(), None),
            now_ms(),
        )
        .await
        .expect("admin_keys alone compiles"),
    );
    let app = admin_api::admin_router(
        Arc::clone(&directory),
        Arc::new(MemorySpendLedger::new()) as Arc<dyn SpendLedger>,
        Arc::new(MetricsRecorder::new()),
        metrics_config(),
    );

    // `admin_auth_layer` reads the plane once, ahead of any handler, which
    // consumes call 1 before the request under test even reaches
    // `budget_view`. A handler that still calls `snapshot` once consumes
    // call 2 and never reaches the landing point -- this whole request
    // answers from `before`, which has no membership for bob at all. A
    // handler regressed to call `plane` then `view` separately consumes
    // calls 2 and 3, and the write lands between them: `plane` (call 2)
    // still answers `before`, `view` (call 3) answers `after`.
    armed.arm(after, 3);

    let view = read(&app, "/v1/admin/projects/widgets/budget").await;
    let members = view["members"]
        .as_array()
        .expect("a budget view always has a members array");
    assert!(
        members.iter().all(|member| member["user"] != "bob"),
        "bob's membership does not exist in the `before` records this \
         request's plane read is pinned to, and the only route to a row for \
         him needs the fresher `after` listing paired with a plane that has \
         caught up to it -- a row for him here means the response mixed a \
         stale plane with a fresh listing: {view}"
    );

    // The assertion above is only a guard if this specific request really
    // produced the two reads the `land_at(3)` arithmetic assumes -- one in
    // `admin_auth_layer`, one in the handler's own call into the directory.
    // A count of anything else means the landing point no longer sits where
    // the comment above claims, and the assertion just passed without a
    // version mismatch in front of it to catch.
    assert_eq!(
        armed.reads_since_armed(),
        2,
        "expected exactly two directory reads for this request (auth, then \
         the handler) -- got a different count, so `arm(.., 3)` is no longer \
         pinned between a regressed handler's two calls and the assertion \
         above proves nothing"
    );

    // And the write really did land, seen coherently once a request's own
    // reads carry the count the rest of the way: this second GET's auth read
    // is the third since arming, so the swap fires *inside* that one
    // `compiled()` call and both halves of `current` move together. Bob's
    // key is live in `after`, so a plane that has caught up resolves a real
    // admission -- `unenforced`, since this project carries no budget --
    // never `no_keys`. Without this the test would also pass on a fixture
    // whose `after` never gave bob a working key at all.
    let landed = read(&app, "/v1/admin/projects/widgets/budget").await;
    let bob = landed["members"]
        .as_array()
        .and_then(|members| members.iter().find(|member| member["user"] == "bob"))
        .unwrap_or_else(|| {
            panic!(
                "sanity: the staged write is meant to have landed by now, or \
                 the assertion above never had a version mismatch to catch: \
                 {landed}"
            )
        });
    assert_eq!(
        bob["committed"]["basis"], "unenforced",
        "bob's key is live in the landed `after` records and this project \
         carries no budget -- a coherent read must resolve his admission, \
         not report `no_keys`: {landed}"
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
    //
    // `provider_reported_usd` and its stamp are the third column, added under
    // M10's G11 ruling: what the upstream itself billed, published beside our
    // two figures and summed into neither. `null` here, because this fixture's
    // scripted provider reports no price — which is the honest reading and not
    // a zero, and the difference is asserted below.
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
            "provider_reported",
            "provider_reported_usd",
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
            "provider_reported",
            "provider_reported_usd",
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

    // G11's column, and the two claims it has to make. A provider that reported
    // nothing leaves `null` rather than a confident `$0.00` — the fixture's
    // scripted upstream sends no `cost` — and the stamp says whose arithmetic
    // the number would be if there were one.
    assert!(
        view["provider_reported_usd"].is_null() && member["provider_reported_usd"].is_null(),
        "nothing in this fixture reported a price, and a zero here would be a \
         figure this deployment cannot support: {view}"
    );
    assert_eq!(view["provider_reported"]["basis"], "provider-reported");
    assert_eq!(
        view["provider_reported"]["window"], "lifetime",
        "folded in this process's memory like `measured`, and stamped the same \
         way so a reader does not compare it against the ledger's window"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archived_project_id_stays_refused_without_a_restart() {
    // The control for the restart test below: within one process's lifetime,
    // `refuse_taken` really does keep an archived id retired, exactly as
    // `ProjectRecord::archived_at_ms` documents. That is what makes the other
    // test's subject the *restart* -- whether the tombstone survived it --
    // rather than whether `refuse_taken` works at all. It was the control for
    // a failing, ignored test for eight milestones; since M16.1 both are
    // green, and it keeps exactly the same job.
    let rig = plain().await;
    let secret = budgeted_member(&rig.app, "shutco", "walt").await;
    turn(&rig.app, &secret, "shutco/walt/main").await;
    admin(&rig.app, "DELETE", "/v1/admin/projects/shutco", None).await;

    let (status, code) = refused(
        &rig.app,
        "POST",
        "/v1/admin/projects",
        Some(json!({ "id": "shutco" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        code, "identity_collision",
        "the archived row is still in the same store, so its id is still taken"
    );
}

/// **R8, closed by M16.1's durable directory (R-D8) — this test is its
/// evidence and its unlock condition, both.**
///
/// It stayed `#[ignore]`d for eight milestones with a note that named exactly
/// what would make it pass, and nothing about the test itself has changed
/// since: for as long as the directory's only backing store was rebuilt fresh
/// on every boot, independent of whether the session store and the spend
/// ledger were Redis-backed and durable, an archived project's tombstone — the
/// row `refuse_taken` reads to keep a closed id retired, see
/// `ProjectRecord::archived_at_ms` — was erased on restart while a durable
/// ledger's rows for the same `(project, user)` survived untouched. The
/// ordinary admin API then let the id be re-created as if it were new, and the
/// ledger silently handed the new tenant the old one's committed spend, with
/// no field anywhere in the budget view saying so.
///
/// What closes it is that the directory is now the fifth family
/// `shared_backend::open` chooses (`main.rs`, the `let backends` /
/// `let directory` pair, and `shared_backend.rs`'s one match), so a deployment
/// that names a Redis stores its tenancy in the same Redis its ledger is in.
/// The rig models that by sharing one document store across the two boots
/// below, exactly as it already shared the ledger — see [`rig_sharing`]. The
/// end-to-end version, against a real Redis and through `open` itself, is
/// `tests/directory_backend_boot.rs`.
///
/// [`an_archived_project_id_stays_refused_without_a_restart`] is the control
/// that keeps this honest: it proves `refuse_taken` retires an id within one
/// process, so a failure here is about what a restart inherits and not about
/// the refusal being broken in general.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recreating_an_archived_project_after_a_restart_inherits_its_spend() {
    // Stands in for a durable, Redis-backed spend ledger: nothing about this
    // test depends on `MemorySpendLedger` being in-memory, only on it being one
    // of the two things that survive the "restart" below unwiped -- which is
    // what a real deployment's Redis-backed ledger and, since M16.1, its
    // Redis-backed directory both do.
    let ledger: Arc<dyn SpendLedger> = Arc::new(MemorySpendLedger::new());
    // The directory's durable half, standing in for the same real deployment's
    // `dir` key exactly as `ledger` stands in for its spend keys: one store,
    // two boots. Before M16.1 the rig had no way to be handed one, which is
    // precisely what this test was ignored for.
    let documents: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::new());

    // Boot 1: a real tenant spends real money and is then closed down.
    let rig1 = rig_sharing(Arc::clone(&ledger), Arc::clone(&documents)).await;
    let secret = budgeted_member(&rig1.app, "shutco", "walt").await;
    turn(&rig1.app, &secret, "shutco/walt/main").await;
    let before = read(&rig1.app, "/v1/admin/projects/shutco/budget").await;
    assert_usd(
        before["committed_usd"].as_f64().expect("a ledger figure"),
        ACTUAL_TURN_USD,
        "spend really landed against `shutco` before the restart",
    );
    admin(&rig1.app, "DELETE", "/v1/admin/projects/shutco", None).await;

    // Boot 2: a fresh `ControlDirectory`, freshly compiled from the file, over
    // the *same* document store -- what `main.rs` builds on every boot of a
    // deployment that names a Redis, since M16.1 made the directory the fifth
    // family that URL chooses. The ledger is shared for the same reason it
    // always was: a restart does not wipe a durable backend.
    let rig2 = rig_sharing(Arc::clone(&ledger), Arc::clone(&documents)).await;

    // A different tenant, through the ordinary admin API, happens to choose the
    // same project id -- and is refused, because the tombstone is in the store
    // this boot inherited. That refusal is the fix: the id stays retired, so
    // no new tenant is ever joined to the old one's ledger rows, and the two
    // assertions below are about what an operator who works *around* the
    // refusal by re-reading the closed project still sees.
    let (status, code) = refused(
        &rig2.app,
        "POST",
        "/v1/admin/projects",
        Some(json!({ "id": "shutco" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        code, "identity_collision",
        "the archived row survived the restart, so its id is still taken -- this is the \
         assertion the whole finding turns on, and the one line of it that is new: before \
         the durable directory this create succeeded, and everything below described what \
         the new tenant then inherited"
    );

    // The user survived too, not just the project: the whole document did, so
    // the new tenant below needs a fresh id on both axes. Asserted rather than
    // worked around silently, because "which rows come back" is exactly what a
    // durable directory is being trusted for.
    let (status, code) = refused(
        &rig2.app,
        "POST",
        "/v1/admin/users",
        Some(json!({ "id": "walt" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code, "identity_collision");

    // So the new tenant takes ids of its own, which is the only route the
    // fixed deployment leaves it -- and the two assertions this test has
    // carried since M8 are what it is checked against. They no longer
    // discriminate the defect (the refusal above does that now) and they are
    // kept rather than deleted because they are the *end state* the finding
    // was about: a tenant created moments ago owes nothing, and the document
    // it is read out of says nothing that would let an operator tell
    // otherwise.
    budgeted_member(&rig2.app, "newco", "wilma").await;

    let after = read(&rig2.app, "/v1/admin/projects/newco/budget").await;
    assert_eq!(
        keys_of(&after),
        keys_of(&before),
        "the document has no field that could tell an operator this project's \
         history is not its own: {after}"
    );
    assert_usd(
        after["committed_usd"].as_f64().expect("a ledger figure"),
        0.0,
        "a project created moments ago, that has run no turn in this process, \
         must start at zero committed spend -- it must not inherit the archived \
         tenant's history just because the durable ledger's row for \
         (project, user) was never cleared: {after}",
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

/// The refusal is decided on shape at any depth and in either spelling, which
/// is what its own stated reason -- "whatever their client library serialized"
/// -- actually commits it to. A top-level-keys-only check answered 501 ("not
/// yet") to both of these, which is the wrong one of the two refusals: the
/// caller is not waiting on a feature, they are being told this deployment will
/// never hold a credential with a lifecycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oauth_shaped_credential_still_gets_the_oauth_refusal_camelcase_and_nested() {
    let rig = plain().await;

    for (label, body) in [
        // camelCase: the check's own stated rationale is "whatever their
        // client library serialized" (admin_api.rs doc comment on
        // `is_oauth_shaped`), and a JS/TS OAuth client serializes exactly
        // this. No nesting or analogy needed -- this falsifies the
        // function's stated design intent directly.
        (
            "camelCase refreshToken",
            json!({ "refreshToken": "rt_abc" }),
        ),
        // Nested one level under "providers", mirroring the exact nesting
        // CredentialsConfig documents for a real per-provider credential
        // block (see `mcp_surface.rs:1028`:
        // `{"providers": {"anthropic": {"env_var": ...}}}`). A caller who
        // read that shape and tried to hand over a refresh token instead of
        // an env var name would write exactly this body.
        (
            "nested under providers",
            json!({ "providers": { "anthropic": { "refresh_token": "rt_abc" } } }),
        ),
    ] {
        let (status, code) = refused(&rig.app, "POST", "/v1/admin/credentials", Some(body)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label}: an OAuth-shaped body must be refused on its own terms, not filed under \
             \"not yet\""
        );
        assert_eq!(code, "oauth_credentials_unsupported", "{label}");
    }
}

/// The controls that stop the test above being tautological: bodies that are
/// genuinely *not* OAuth-shaped at any depth or in any casing must keep
/// answering 501. A walk that widened until everything matched would pass every
/// assertion above and fail every one here, which is the only way to tell the
/// two apart from outside.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_oauth_credential_still_gets_the_ordinary_501_nested_or_camelcase() {
    let rig = plain().await;

    for (label, body) in [
        (
            "nested ordinary provider credential",
            json!({ "providers": { "anthropic": { "api_key": "sk-x" } } }),
        ),
        ("camelCase ordinary field", json!({ "apiKey": "sk-x" })),
        ("empty body", json!({})),
    ] {
        let (status, code) = refused(&rig.app, "POST", "/v1/admin/credentials", Some(body)).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{label}");
        assert_eq!(code, "credential_crud_not_available", "{label}");
    }
}

/// The array limb, and the discriminating pair that keeps the walk honest.
///
/// A client that batches its credentials is describing the same thing one
/// level out, so an array is walked rather than dismissed as
/// "not a credential object" -- but only the *contents* decide the answer. If
/// the walk answered 400 to any array at all it would pass the first assertion
/// here and fail the second, which is why both are in one test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_array_body_is_judged_by_what_is_in_it() {
    let rig = plain().await;
    let (status, code) = refused(
        &rig.app,
        "POST",
        "/v1/admin/credentials",
        Some(json!([{ "refresh_token": "rt_abc" }])),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "wrapping a refresh token in an array does not make it a credential this \
         deployment could hold"
    );
    assert_eq!(code, "oauth_credentials_unsupported");

    let (status, code) = refused(
        &rig.app,
        "POST",
        "/v1/admin/credentials",
        Some(json!([{ "api_key": "sk-x" }])),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_IMPLEMENTED,
        "and an array of ordinary credentials is still the ordinary refusal"
    );
    assert_eq!(code, "credential_crud_not_available");
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
async fn a_revoked_only_key_still_reports_its_committed_spend() {
    // R1. The membership below spends real money on a real key, and only then
    // has that key revoked. The ledger does not forget: the balance the engine
    // charged against `globex`'s ceiling is unaffected by a key's lifecycle,
    // and a live hold would still bind the project's limit too. But
    // `budget_view` resolves a row's basis by asking `plane.membership()` for
    // an *admission*, and a revoked key has no admission -- which is
    // indistinguishable there from a membership that was never issued one at
    // all. Reported as `no_keys` with every dollar blanked, this row discarded
    // a balance the system still had rather than declining to guess at one it
    // didn't; it now reports `revoked_keys` with the real figures, read under
    // terms derived from the directory through the compiler's own pairing.
    let rig = plain().await;
    let id = tenancy(
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
        None,
    )
    .await;
    let minted = admin(
        &rig.app,
        "POST",
        &format!("/v1/admin/projects/{id}/members/bob/keys"),
        None,
    )
    .await;
    let secret = minted["secret"]
        .as_str()
        .expect("a minted secret")
        .to_string();
    let key_id = minted["id"].as_str().expect("an id").to_string();

    turn(&rig.app, &secret, "globex/bob/main").await;

    // The spend is real before the key is touched: the same assertion the
    // no-drift tests make, so a failure below cannot be "the turn never
    // settled" wearing this finding's name.
    let mid_flight = read(&rig.app, &format!("/v1/admin/projects/{id}/budget")).await;
    assert_usd(
        mid_flight["committed_usd"]
            .as_f64()
            .expect("the ledger charged this turn before the key was revoked"),
        ACTUAL_TURN_USD,
        "committed spend before revocation",
    );

    admin(
        &rig.app,
        "DELETE",
        &format!("/v1/admin/keys/{key_id}"),
        None,
    )
    .await;

    let view = read(&rig.app, &format!("/v1/admin/projects/{id}/budget")).await;
    let member = &view["members"][0];
    assert_eq!(member["user"], "bob");
    assert_eq!(
        member["committed"]["basis"], "revoked_keys",
        "bob spent {ACTUAL_TURN_USD} before his only key was revoked -- the \
         ledger still holds that against him, which `no_keys` denies and \
         `ledger` alone would not admit: {member}"
    );
    assert_usd(
        member["member_committed_usd"]
            .as_f64()
            .expect("the ledger's committed balance for this principal is still real"),
        ACTUAL_TURN_USD,
        "bob's committed spend after his key was revoked",
    );

    // **The assertion this test exists for.** A figure is only worth reporting
    // if it was read under the terms the engine spent under: `balance` rolls a
    // lapsed window over, so terms assembled for the occasion -- a monthly
    // window where the project declared a total one -- would not merely
    // mislabel this row, it would zero the project's committed spend on the way
    // past. Comparing the stamp to the one taken *before* the revocation is
    // what catches that: same window, same instant it began, therefore the same
    // `BudgetTerms`.
    assert_eq!(
        member["committed"]["window"], mid_flight["committed"]["window"],
        "the derived terms must carry the project's own window, or reading a \
         balance under them rolls it: {member} vs {mid_flight}"
    );
    assert_eq!(
        member["committed"]["window_start_ms"], mid_flight["committed"]["window_start_ms"],
        "and the same window start: {member} vs {mid_flight}"
    );

    // Same story at the project level: `globex` has no other member, so the
    // project row inherits whatever basis `bob`'s row produced.
    assert_eq!(
        view["committed"]["basis"], "revoked_keys",
        "the project's own ceiling is still bound by bob's committed spend: {view}"
    );
    assert_usd(
        view["committed_usd"]
            .as_f64()
            .expect("the project-level committed figure survives the revocation"),
        ACTUAL_TURN_USD,
        "the project's committed spend after its only key was revoked",
    );
    assert_eq!(
        view["committed"]["window"], mid_flight["committed"]["window"],
        "{view} vs {mid_flight}"
    );
    assert_eq!(
        view["committed"]["window_start_ms"], mid_flight["committed"]["window_start_ms"],
        "{view} vs {mid_flight}"
    );
    // Held dollars are reported for the same reason the committed ones are: the
    // column exists and the ledger has an answer for it.
    assert!(
        view["held_usd"].as_f64().is_some(),
        "a real position has a real hold figure, even when it is zero: {view}"
    );

    // The control this finding depends on -- a membership that never had a key
    // at all must still report `no_keys` -- is
    // `a_keyless_membership_is_reported_as_having_no_keys_rather_than_unenforced`
    // just above, which stays live and passing: it is what proves this test is
    // about the view discarding real information rather than about the two
    // cases legitimately looking alike.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_key_on_an_unbudgeted_project_is_unenforced_and_not_revoked_keys() {
    // The boundary of R1's remedy. `revoked_keys` promises real dollars read
    // from the ledger, and an unbudgeted project has no ledger position at all
    // -- the engine never called it. So a revoked key here changes nothing about
    // why the figure is absent: nobody was counting this member's spend before
    // the revocation and nobody is counting it after, which is what `unenforced`
    // says and what makes it the honest label rather than `revoked_keys` with a
    // `null` under it (or `no_keys`, which would deny the spending outright).
    let rig = plain().await;
    let id = tenancy(&rig.app, json!({ "id": "openhanded" }), "bob", None).await;
    let minted = admin(
        &rig.app,
        "POST",
        &format!("/v1/admin/projects/{id}/members/bob/keys"),
        None,
    )
    .await;
    let secret = minted["secret"].as_str().expect("a secret").to_string();
    let key_id = minted["id"].as_str().expect("an id").to_string();
    turn(&rig.app, &secret, "openhanded/bob/main").await;
    admin(
        &rig.app,
        "DELETE",
        &format!("/v1/admin/keys/{key_id}"),
        None,
    )
    .await;

    let view = read(&rig.app, &format!("/v1/admin/projects/{id}/budget")).await;
    let member = &view["members"][0];
    assert_eq!(
        member["committed"]["basis"], "unenforced",
        "there is no position a revoked key could have left behind on a project \
         with no budget: {view}"
    );
    assert!(member["member_committed_usd"].is_null(), "{member}");
    assert_eq!(view["committed"]["basis"], "unenforced", "{view}");
    assert!(view["committed_usd"].is_null(), "{view}");
    // And the fold still measured the money that was spent, which is the whole
    // reason `unenforced` is a warning rather than a shrug.
    assert!(
        view["measured_usd"].as_f64().expect("a folded figure") > 0.0,
        "{view}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_live_key_is_what_the_projects_own_row_is_stamped_from() {
    // The project's basis is not simply the first member row's. `revoked_keys`
    // says "nobody can spend against this position any more", which is exactly
    // false of a project that still has a live key somewhere -- and the member
    // whose key was revoked is listed first here, so a first-row-wins rule would
    // stamp the whole project with it. The figures are the same either way (they
    // are project-wide), which is what makes the label the only thing at stake
    // and the only thing worth testing.
    let rig = plain().await;
    let id = tenancy(
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
        None,
    )
    .await;
    let minted = admin(
        &rig.app,
        "POST",
        &format!("/v1/admin/projects/{id}/members/bob/keys"),
        None,
    )
    .await;
    let secret = minted["secret"].as_str().expect("a secret").to_string();
    let key_id = minted["id"].as_str().expect("an id").to_string();
    turn(&rig.app, &secret, "globex/bob/main").await;
    admin(
        &rig.app,
        "DELETE",
        &format!("/v1/admin/keys/{key_id}"),
        None,
    )
    .await;

    // cleo joins after bob and keeps her key, so the members list is
    // [revoked, live] and the project's own row has to prefer the second.
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
        &format!("/v1/admin/projects/{id}/members/cleo"),
        Some(json!({ "role": "member" })),
    )
    .await;
    admin(
        &rig.app,
        "POST",
        &format!("/v1/admin/projects/{id}/members/cleo/keys"),
        None,
    )
    .await;

    let view = read(&rig.app, &format!("/v1/admin/projects/{id}/budget")).await;
    assert_eq!(view["members"][0]["user"], "bob", "{view}");
    assert_eq!(view["members"][0]["committed"]["basis"], "revoked_keys");
    assert_eq!(view["members"][1]["user"], "cleo", "{view}");
    assert_eq!(view["members"][1]["committed"]["basis"], "ledger");
    assert_eq!(
        view["committed"]["basis"], "ledger",
        "one live key means this project's ceiling is still being enforced \
         against something, whatever happened to bob's: {view}"
    );
    // Bob's spend is still in the project figure -- it is the same account --
    // which is what makes the label the whole of the difference.
    assert_usd(
        view["committed_usd"].as_f64().expect("a ledger figure"),
        ACTUAL_TURN_USD,
        "the project's committed spend still includes bob's turn",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_only_membership_still_counts_toward_the_share_sum() {
    // The other half of R1's remedy, and the reason the derived terms carry the
    // *membership's* allocation rather than a bare project budget. A share is an
    // arrangement between members -- what the sum is for is showing an operator
    // how the project's limit has been promised out -- and dropping a member
    // from it the moment their key is revoked would silently re-report the
    // arrangement as something nobody agreed to, while their spend goes on
    // binding the same limit.
    let rig = plain().await;
    let id = tenancy(
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
    let minted = admin(
        &rig.app,
        "POST",
        &format!("/v1/admin/projects/{id}/members/bob/keys"),
        None,
    )
    .await;
    let key_id = minted["id"].as_str().expect("an id").to_string();
    admin(
        &rig.app,
        "DELETE",
        &format!("/v1/admin/keys/{key_id}"),
        None,
    )
    .await;

    let view = read(&rig.app, &format!("/v1/admin/projects/{id}/budget")).await;
    let member = &view["members"][0];
    assert_eq!(member["committed"]["basis"], "revoked_keys", "{view}");
    assert!(
        (member["allocation_share"].as_f64().expect("a share") - 0.7).abs() < EPSILON,
        "{view}"
    );
    assert!(
        (view["allocation_share_sum"].as_f64().expect("a sum") - 0.7).abs() < EPSILON,
        "a revoked member is still allocated 70% of this project's limit: {view}"
    );
    // And their own ceiling is still the share of the project limit, which is
    // what makes the sum readable as an arrangement rather than a number
    // floating free of anything.
    assert_usd(
        member["member_remaining_usd"].as_f64().expect("a ceiling"),
        LIMIT_USD * 0.7,
        "an unspent share is the whole of it, key or no key",
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
async fn an_axis_patched_to_an_explicit_null_is_refused_naming_the_axis() {
    // The one JSON spelling that reads like an attempt at removal, and there is
    // nothing honest to do with it: an absent field means "leave alone", and
    // this milestone has no spelling for "remove this block" -- removing a
    // budget widens a ceiling to unlimited. A 200 that changed nothing would
    // tell an operator their clear worked.
    let rig = plain().await;
    tenancy(
        &rig.app,
        json!({
            "id": "globex",
            "budget": { "limit_usd": LIMIT_USD, "window": "total", "on_exhaustion": "refuse" },
        }),
        "bob",
        None,
    )
    .await;

    for axis in ["name", "policy", "budget", "validate", "credentials"] {
        let (status, code) = refused(
            &rig.app,
            "PATCH",
            "/v1/admin/projects/globex",
            Some(json!({ axis: Value::Null })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "`{axis}`: null");
        assert_eq!(code, "null_patch_unsupported", "`{axis}`: null");

        let (_, text) = send(
            &rig.app,
            "PATCH",
            "/v1/admin/projects/globex",
            Some(&root()),
            Some(json!({ axis: Value::Null })),
        )
        .await;
        assert!(
            text.contains(axis),
            "a caller who nulled one field of five must not have to guess which one \
             the refusal is about: {text}"
        );
    }

    // Nothing was cleared on the way through, which is the failure the refusal
    // exists to prevent -- `budgeted` is the one axis this API reads back.
    let project = read(&rig.app, "/v1/admin/projects/globex").await;
    assert_eq!(project["budgeted"], true, "{project}");

    // The control: an *absent* axis is still "leave alone", answered 200. If
    // this had turned into a refusal too, the fix would have replaced a silent
    // no-op with a surface nobody can patch.
    let patched = admin(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/globex",
        Some(json!({ "name": "Globex Corp" })),
    )
    .await;
    assert_eq!(patched["name"], "Globex Corp");
    assert_eq!(
        patched["budgeted"], true,
        "a budget nobody mentioned is a budget nobody touched: {patched}"
    );
}

/// Every refusal this surface can answer, hit once each and asserted on both
/// halves of the answer.
///
/// The gap this closes is the one the codes themselves are for: a client
/// branches on `error.code`, so a code that no test ever produces is a string
/// nobody has checked against the status it travels with. Four of these
/// (`user_not_found`, `membership_not_found`, `project_is_archived`,
/// `invalid_control_plane`) had no test at any layer.
///
/// A table for the eight that share one deployment, and three blocks for the
/// three that cannot: two need a key in a particular state, and one needs a
/// deployment with no control plane at all. A table with a setup discriminator
/// in it would be harder to read than the three blocks it absorbed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_refusal_code_this_surface_answers_is_produced_at_least_once() {
    let rig = plain().await;
    tenancy(
        &rig.app,
        json!({
            "id": "globex",
            "budget": { "limit_usd": LIMIT_USD, "window": "total", "on_exhaustion": "refuse" },
        }),
        "bob",
        None,
    )
    .await;
    // A second project that gets archived, so `project_is_archived` is reached
    // on a project this API owns -- `acme` would answer `config_owned` first,
    // which is a different row of this same table.
    let archived_key = budgeted_member(&rig.app, "gone", "gus").await;
    admin(&rig.app, "DELETE", "/v1/admin/projects/gone", None).await;

    for (code, status, method, uri, body) in [
        (
            "project_not_found",
            StatusCode::NOT_FOUND,
            "GET",
            "/v1/admin/projects/nosuch",
            None,
        ),
        (
            "user_not_found",
            StatusCode::NOT_FOUND,
            "PUT",
            "/v1/admin/projects/globex/members/ghost",
            Some(json!({ "role": "member" })),
        ),
        (
            "membership_not_found",
            StatusCode::NOT_FOUND,
            "DELETE",
            "/v1/admin/projects/globex/members/ghost",
            None,
        ),
        (
            "key_not_found",
            StatusCode::NOT_FOUND,
            "GET",
            "/v1/admin/keys/key_nosuch",
            None,
        ),
        (
            "config_owned",
            StatusCode::CONFLICT,
            "PATCH",
            "/v1/admin/projects/acme",
            Some(json!({ "name": "Renamed" })),
        ),
        (
            "identity_collision",
            StatusCode::CONFLICT,
            "POST",
            "/v1/admin/projects",
            Some(json!({ "id": "globex" })),
        ),
        (
            "project_is_archived",
            StatusCode::CONFLICT,
            "PATCH",
            "/v1/admin/projects/gone",
            Some(json!({ "name": "Back From The Dead" })),
        ),
        (
            "window_change_unsupported",
            StatusCode::BAD_REQUEST,
            "PATCH",
            "/v1/admin/projects/globex",
            Some(json!({
                "budget": { "limit_usd": LIMIT_USD, "window": "monthly", "on_exhaustion": "refuse" }
            })),
        ),
        (
            "null_patch_unsupported",
            StatusCode::BAD_REQUEST,
            "PATCH",
            "/v1/admin/projects/globex",
            Some(json!({ "budget": Value::Null })),
        ),
        (
            // A body the *compiler* refuses, reported in the compiler's own
            // words -- the same sentence a boot failure would have printed.
            "invalid_control_plane",
            StatusCode::UNPROCESSABLE_ENTITY,
            "PATCH",
            "/v1/admin/projects/globex",
            Some(json!({ "policy": { "min_quality": 2.0 } })),
        ),
        (
            "oauth_credentials_unsupported",
            StatusCode::BAD_REQUEST,
            "POST",
            "/v1/admin/credentials",
            Some(json!({ "kind": "oauth" })),
        ),
        (
            "credential_crud_not_available",
            StatusCode::NOT_IMPLEMENTED,
            "POST",
            "/v1/admin/credentials",
            Some(json!({ "provider": "anthropic" })),
        ),
    ] {
        let (answered_status, answered_code) = refused(&rig.app, method, uri, body).await;
        assert_eq!(answered_code, code, "{method} {uri}");
        assert_eq!(answered_status, status, "{method} {uri} -> {code}");
    }

    // `project_archived`: a live turn key whose project closed under it. 403 on
    // this surface as on every other, because the key is refused before the
    // route is reached.
    let (status, project_archived) = refused_as(
        &rig.app,
        "GET",
        "/v1/admin/projects",
        Some(archived_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(project_archived, "project_archived");

    // `revoked_key`: an admin key this API minted and then tombstoned.
    let minted = admin(&rig.app, "POST", "/v1/admin/keys", None).await;
    let secret = minted["secret"].as_str().expect("a secret").to_string();
    admin(
        &rig.app,
        "DELETE",
        &format!("/v1/admin/keys/{}", minted["id"].as_str().expect("an id")),
        None,
    )
    .await;
    let (status, code) =
        refused_as(&rig.app, "GET", "/v1/admin/projects", Some(secret), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code, "revoked_key");

    // `admin_requires_control_plane`: a deployment with no root of trust an
    // admin key could have been issued from, which needs its own router.
    let open = admin_api::admin_router(
        ControlDirectory::open(),
        Arc::new(MemorySpendLedger::new()),
        Arc::new(roundhouse_core::metrics::MetricsRecorder::new()),
        metrics_config(),
    );
    let (status, code) = refused_as(&open, "GET", "/v1/admin/projects", Some(root()), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code, "admin_requires_control_plane");

    // The near-collision, asserted on the two strings this test just pulled off
    // the wire rather than on two literals: `project_is_archived` (409, a
    // mutation this API refuses to make) and `project_archived` (403, a key
    // whose project closed) differ by one infix and mean opposite things to
    // whoever is holding the refusal. Comparing real answers is what makes a
    // rename that converged them break here instead of passing quietly.
    let (_, project_is_archived) = refused(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/gone",
        Some(json!({ "name": "Back From The Dead" })),
    )
    .await;
    assert_ne!(project_is_archived, project_archived);
    assert!(
        !project_is_archived.starts_with(&project_archived)
            && !project_archived.starts_with(&project_is_archived),
        "one code being a prefix of the other is how a client's `starts_with` \
         dispatch quietly handles the wrong refusal: `{project_is_archived}` vs \
         `{project_archived}`"
    );
}

/// The create body *is* a `"projects"` entry, so the strictness the file gained
/// is the strictness this route gained -- there is no second spelling of a
/// project for the two to disagree in. It matters here more than in the file:
/// nothing reads a project back afterwards (see `ProjectDto` and every admin
/// route, none of which echoes `policy`, `validate`, `credentials` or a budget
/// limit), so a dropped field would never surface anywhere at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_misspelled_field_on_project_create_is_refused_not_silently_dropped() {
    let rig = plain().await;
    let (status, text) = send(
        &rig.app,
        "POST",
        "/v1/admin/projects",
        Some(&root()),
        Some(json!({ "id": "typo-co", "credential": { "mode": "pass_through" } })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a field this API never reads must stop the create at 422, the same way a malformed \
         request body does -- not silently drop the field and answer 201: got {status}: {text}"
    );
}

/// Control for the test above: the correctly spelled field creates the project
/// as normal, proving the refusal is about the typo and not about some
/// unrelated fixture problem.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_correctly_spelled_field_on_project_create_still_succeeds() {
    let rig = plain().await;
    let (status, text) = send(
        &rig.app,
        "POST",
        "/v1/admin/projects",
        Some(&root()),
        Some(json!({ "id": "spelled-co", "credentials": { "mode": "pass_through" } })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the real field name is enough: {text}"
    );
}

/// G14: a project's `fair_use` block is settable, readable and movable.
///
/// `POST` has always accepted one (the body is just [`ProjectEntry`] on the
/// wire), and before this fix nothing on the admin surface could read it back or
/// change it: `ProjectDto` had no field for it and `ProjectPatch` is
/// `deny_unknown_fields`, so `PATCH {"fair_use": ...}` was a 422 for an axis the
/// create route on the same resource had just accepted. The only remedy left was
/// delete-and-recreate, which the archived-id tombstone exists to make
/// unnecessary.
///
/// The read asserts the *block*, not a flag: an operator asking "what is in
/// force" needs the window and the cap, and fair use has no second view (the
/// way `budgeted: bool` has the budget view) to send them to for the number.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_projects_fair_use_can_be_created_read_back_and_changed() {
    let rig = plain().await;

    // Created with a 5h fair-use window, same as the create route already
    // accepts today -- the create half of this claim is not in dispute.
    let (create_status, create_text) = send(
        &rig.app,
        "POST",
        "/v1/admin/projects",
        Some(&root()),
        Some(json!({
            "id": "axis-co",
            "fair_use": { "windows": [{ "window": "5h", "max_usd": 5.0 }] },
        })),
    )
    .await;
    assert_eq!(
        create_status,
        StatusCode::CREATED,
        "create with fair_use is accepted today: {create_text}"
    );

    // Read back, in the file's own vocabulary -- which is also the vocabulary
    // the PATCH below is written in, so there is one spelling of a window and
    // not two.
    let read_back = read(&rig.app, "/v1/admin/projects/axis-co").await;
    assert_eq!(
        read_back["fair_use"],
        json!({ "windows": [{ "window": "5h", "max_usd": 5.0 }] }),
        "GET must echo the block that is in force, cap and all -- a flag saying \
         only that some ceiling exists leaves the operator exactly where this \
         finding found them: {read_back}"
    );

    // Changed: the same axis the create route accepted, moved.
    let (patch_status, patch_text) = send(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/axis-co",
        Some(&root()),
        Some(json!({
            "fair_use": { "windows": [{ "window": "5h", "max_usd": 50.0 }] },
        })),
    )
    .await;
    assert_eq!(
        patch_status,
        StatusCode::OK,
        "PATCH fair_use must not be refused as an unknown field for an axis the \
         create route on this same resource accepts: {patch_text}"
    );
    let after = read(&rig.app, "/v1/admin/projects/axis-co").await;
    assert_eq!(
        after["fair_use"]["windows"][0]["max_usd"], 50.0,
        "the raised cap is what a later read reports -- a 200 that changed \
         nothing would be the same silence in a different place: {after}"
    );

    // And the patched block really went through the one compiler rather than
    // being stored as opaque JSON: a window that caps nothing is refused here
    // exactly as it is refused at boot. A PATCH that skipped validation would
    // answer 200 and leave a ceiling that enforces nothing until the next
    // restart failed to start.
    let (invalid_status, invalid_text) = send(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/axis-co",
        Some(&root()),
        Some(json!({ "fair_use": { "windows": [{ "window": "24h" }] } })),
    )
    .await;
    assert_eq!(
        invalid_status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a window naming neither cap must be refused on the way in: \
         {invalid_text}"
    );
    let unchanged = read(&rig.app, "/v1/admin/projects/axis-co").await;
    assert_eq!(
        unchanged["fair_use"]["windows"][0]["max_usd"], 50.0,
        "and the refused patch must have changed nothing: {unchanged}"
    );
}

/// The window axis a `PATCH` may move, and the one it may not, side by side.
///
/// A budget's window is refused because committed spend is counted *within* one
/// (see `a_budget_window_change_is_refused_over_http_naming_the_mechanism`). A
/// fair-use window has nothing committed to reinterpret -- the ledger buckets
/// draws by wall-clock index under `(project, member)` and reads the configured
/// span at admission time -- so moving one is an ordinary change, and this is
/// the test that says the asymmetry is deliberate rather than an oversight
/// nobody has hit yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fair_use_window_may_be_moved_where_a_budget_window_may_not() {
    let rig = plain().await;
    admin(
        &rig.app,
        "POST",
        "/v1/admin/projects",
        Some(json!({
            "id": "window-co",
            "fair_use": { "windows": [{ "window": "5h", "max_tokens": 1000 }] },
        })),
    )
    .await;

    let (status, text) = send(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/window-co",
        Some(&root()),
        Some(json!({
            "fair_use": { "windows": [{ "window": "7d", "max_tokens": 1000 }] },
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "widening a rolling window destroys no committed figure, so nothing \
         here refuses it: {text}"
    );
    let after = read(&rig.app, "/v1/admin/projects/window-co").await;
    assert_eq!(after["fair_use"]["windows"][0]["window"], "7d");

    // An explicit `null` is still refused, naming the axis -- the same rule
    // every other patchable axis is under, since "remove this ceiling" widens
    // silently and has no spelling here on purpose.
    let (null_status, null_code) = refused(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/window-co",
        Some(json!({ "fair_use": Value::Null })),
    )
    .await;
    assert_eq!(null_status, StatusCode::BAD_REQUEST);
    assert_eq!(null_code, "null_patch_unsupported");
}

/// The other half of G14's read: a *member's* own windows, on the key view.
///
/// A member ceiling binds independently of the project's, so a member refused
/// while the project has room is a refusal no project view can explain. Only a
/// file-declared key can carry one -- the admin plane mints keys under a
/// membership and has no route that writes a member window -- which is what
/// this fixture's own file is for, and what the `null` on the API-minted key
/// below pins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_members_own_fair_use_windows_are_readable_on_the_key_view() {
    let file = control_plane(
        json!({
            "projects": [{ "id": "acme" }],
            "users": [{ "id": "ada" }],
            "keys": [{
                "project": "acme",
                "user": "ada",
                "key_sha256": sha256_hex(&key("ada")),
                "fair_use": { "windows": [{ "window": "24h", "max_tokens": 2_000_000 }] },
            }],
            "admin_keys": [sha256_hex(&root())],
        }),
        "admin-api member fair-use fixture",
    );
    let rig = rig_over(
        file,
        Arc::new(MemorySpendLedger::new()),
        Arc::new(MemoryDocumentStore::new()),
    )
    .await;

    let keys = read(&rig.app, "/v1/admin/keys").await;
    let declared = keys["data"]
        .as_array()
        .expect("a listing")
        .iter()
        .find(|row| row["user"] == "ada")
        .expect("the file's key is listed")
        .clone();
    assert_eq!(
        declared["fair_use"],
        json!({ "windows": [{ "window": "24h", "max_tokens": 2_000_000 }] }),
        "the member ceiling in force has to be readable from the surface that \
         lists the key it belongs to: {declared}"
    );

    // The control, and the honest half: an admin key pays for nothing and has
    // no scope a rolling ceiling could be drawn against, so it reports `null`
    // rather than inheriting anything.
    let admin_row = keys["data"]
        .as_array()
        .expect("a listing")
        .iter()
        .find(|row| row["scope"] == "admin")
        .expect("the file's admin key is listed")
        .clone();
    assert!(admin_row["fair_use"].is_null(), "{admin_row}");
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
    let before = rig.directory.version(now_ms()).await;
    let secret = budgeted_member(&rig.app, "globex", "bob").await;
    assert!(
        rig.directory.version(now_ms()).await > before,
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

// ---------------------------------------------------------------------------
// R3: PATCHing a budget onto a previously-unbudgeted project
// ---------------------------------------------------------------------------
//
// `Engine::settle` (`engine/spend.rs`) used to gate on `admission.budget`
// read live off the *current* directory rather than off anything the log
// records: `let Some(terms) = &admission.budget else { return Ok(()) };`.
// That is a correct reading of "is there an account to talk to", which is why
// the line is still there and why
// `the_view_reports_null_not_zero_for_an_unbudgeted_project` above still
// holds. It was never a correct reading of "was *this turn* budgeted", and
// the two diverge the moment a turn that ran under `None` is re-read under a
// `Some` a later admin write installed: the directory's own window-change
// guard (`control_config/directory/mutation.rs`, `if let (Some(current),
// Some(next)) = (&project.entry.budget, &patch.budget)`) only compares two
// *existing* budgets, so a `PATCH` that turns budgeting on for the first time
// is invisible to it.
//
// `Engine::repair_settle` runs on every `run_turn`, before the new turn is
// even admitted, and replays the session's *last* terminal event through
// `Engine::settle` under whatever `Admission` this call resolved --
// unconditionally. A turn that ran while the project had no budget was never
// settled (the ledger was never called), so it has no watermark in the spend
// ledger's per-session map. The first turn served *after* the `PATCH`
// therefore found that pre-budget terminal event unsettled, charged it
// against the newly opened window, and only then ran its own turn and settled
// that too -- so one genuinely-budgeted turn paid for two.
//
// The fix is `DecisionRecord::budget_draw`: whether a budget was in force is
// recorded on the decision, so the settle asks the log rather than the plane
// and a `None -> Some` PATCH governs only the turns decided after it.

/// A turn that ran before a project had any budget must not be absorbed into
/// the ledger the first time a later, genuinely-budgeted turn on the same
/// session drives a repair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_a_turn_predating_a_project_s_budget_is_not_absorbed_when_the_budget_is_patched_on() {
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
    let session = "openhanded/bob/main";

    // `t0` runs while the project has no budget at all: no grant, no settle,
    // nothing for the ledger to hold an opinion about.
    turn_with_id(&rig.app, &secret, session, "t0").await;
    let before = read(&rig.app, "/v1/admin/projects/openhanded/budget").await;
    assert!(
        before["committed_usd"].is_null(),
        "the ledger must not have been touched by an unbudgeted turn, or \
         nothing below is about the patch: {before}"
    );

    // An admin turns budgeting on. `None -> Some`, which the window-change
    // guard's `(Some, Some)` pattern does not match.
    admin(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/openhanded",
        Some(json!({
            "budget": { "limit_usd": LIMIT_USD, "window": "total", "on_exhaustion": "refuse" },
        })),
    )
    .await;

    // Isolate the mechanism before running any new turn at all: re-send `t0`
    // under its own turn id. `Session::begin_turn` deduplicates it, so this
    // opens no grant and drives no settle of its own
    // (`a_deduplicated_retry_opens_no_second_grant`, `budget_routing.rs`) --
    // every `run_turn` still calls `Session::open_observed` and
    // `repair_settle` before the dedup check runs, though, so any *dollars*
    // appearing here can only be `repair_settle` re-pricing a terminal event
    // that predates the budget entirely, not any turn actually being served.
    //
    // Zero rather than `null`: the project has a budget now, so the view has
    // a real account to read and reports the position it finds. What the
    // assertion is about is the figure, and the figure is what the defect
    // moved.
    turn_with_id(&rig.app, &secret, session, "t0").await;
    let mid = read(&rig.app, "/v1/admin/projects/openhanded/budget").await;
    assert_usd(
        mid["committed_usd"]
            .as_f64()
            .expect("the project is budgeted now, so the view reports a figure"),
        0.0,
        "a dedup of `t0` served no turn and opened no grant, so nothing may \
         have been charged against the window the PATCH just opened",
    );
    // And the honest signal that the fold saw a turn the ledger did not.
    // Before the fix this column read a clean `0.0` at exactly this point --
    // the ledger had wrongly absorbed `t0`'s $0.2 and the fold had rightly
    // measured it, so the two agreed by coincidence and the one column built
    // to surface ledger-versus-log drift showed none. A project that served
    // turns before it was budgeted is *supposed* to look like this.
    assert_usd(
        mid["drift_usd"].as_f64().expect("a drift figure"),
        -ACTUAL_TURN_USD,
        "the fold still measures `t0` and the ledger rightly does not, which \
         is drift with a cause rather than an accounting error",
    );

    // One more turn on the *same* session -- the ordinary, user-visible path:
    // `run_turn` replays the log, `repair_settle` runs before `t1` is
    // admitted, and then `t1` itself settles.
    turn_with_id(&rig.app, &secret, session, "t1").await;

    let after = read(&rig.app, "/v1/admin/projects/openhanded/budget").await;
    let committed = after["committed_usd"]
        .as_f64()
        .expect("the project is budgeted now, so the view reports a figure");
    assert_usd(
        committed,
        ACTUAL_TURN_USD,
        "only `t1` ever ran under a budget -- `t0` predates it entirely and \
         must not be repaired into the window the patch just opened",
    );
}

/// The control: the same two-turns-one-session-with-a-PATCH-in-between shape,
/// but the budget is `Some` from before `t0` and the `PATCH` changes only the
/// limit (`Some -> Some`, which the window-change guard does compare). Both
/// turns were genuinely budgeted throughout, so `2 * ACTUAL_TURN_USD` is the
/// *correct* total here -- which is what proves the harness above is not
/// tautological. A ledger that always charges whatever the log's turns cost,
/// with no regard for whether a budget existed when each one ran, would
/// report this same `2 * ACTUAL_TURN_USD` in both this test and the ignored
/// one above -- right here, because both turns really were budgeted, and
/// wrong there, because `t0` never was. Only the ignored test's own
/// dedup-isolation step (no new turn served, ledger still moves) tells the
/// two apart; this control rules out `turn_with_id`, the `PATCH` machinery,
/// and the admin view's arithmetic as the source of that difference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_control_a_budget_raised_mid_session_still_settles_every_turn_it_covered() {
    let rig = plain().await;
    let secret = {
        tenancy(
            &rig.app,
            json!({
                "id": "metered",
                "budget": { "limit_usd": LIMIT_USD, "window": "total", "on_exhaustion": "refuse" },
            }),
            "bob",
            None,
        )
        .await;
        let minted = admin(
            &rig.app,
            "POST",
            "/v1/admin/projects/metered/members/bob/keys",
            None,
        )
        .await;
        minted["secret"].as_str().expect("a secret").to_string()
    };
    let session = "metered/bob/main";

    turn_with_id(&rig.app, &secret, session, "t0").await;

    // `Some -> Some`: the window is unchanged, only the limit moves, so the
    // guard admits it -- this is the axis PATCH is meant to move.
    admin(
        &rig.app,
        "PATCH",
        "/v1/admin/projects/metered",
        Some(json!({
            "budget": { "limit_usd": LIMIT_USD * 2.0, "window": "total", "on_exhaustion": "refuse" },
        })),
    )
    .await;

    turn_with_id(&rig.app, &secret, session, "t1").await;

    let view = read(&rig.app, "/v1/admin/projects/metered/budget").await;
    assert_usd(
        view["committed_usd"]
            .as_f64()
            .expect("a budgeted project reports a committed figure"),
        2.0 * ACTUAL_TURN_USD,
        "both t0 and t1 ran under a real budget throughout, so both belong in \
         committed_usd -- unlike the ignored case above, there is no turn \
         here that predates budgeting",
    );
}
