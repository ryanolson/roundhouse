// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Relay-format surface, over a real router.
//!
//! Everything about the *content* of these three documents is tested inside
//! `roundhouse-relay`, against slices of events and with no socket in sight.
//! What can only be tested here is what the transport adds: that the three
//! routes are mounted where they say they are, that they answer the right media
//! type, that the namespace check refuses another tenant's session without
//! revealing whether it exists, and that a session longer than one store batch
//! is not silently truncated — which no unit test can reach, because the paging
//! loop is the transport's.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use roundhouse_core::control::{Billing, BudgetState, Payer, Principal};
use roundhouse_core::event::{SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::metrics::{MetricsConfig, ReferenceModel, ShadowPricing};
use roundhouse_core::routing::{DecisionRecord, ProviderPricing, Target};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_server::{ControlPlane, ControlPlaneConfig, relay_api};

mod common;
use common::{path_segment, sha256_hex};

const HOSTED: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 3.0,
    cached_input_per_mtok_usd: 0.3,
    cache_write_per_mtok_usd: 3.75,
    output_per_mtok_usd: 15.0,
};

fn pricing() -> Arc<MetricsConfig> {
    Arc::new(
        MetricsConfig::new(
            ShadowPricing::new(vec![ReferenceModel {
                provider: "anthropic".into(),
                model: "claude".into(),
                pricing: HOSTED,
                quality_prior: 0.6,
            }])
            .declare("llama", "anthropic", "claude", "matched on our eval suite"),
        )
        .with_default_local_quality(0.6),
    )
}

/// Padded to the 43 base62 characters the key shape requires, for the reason the
/// tenancy fixture states: a hand-counted literal fails as `malformed_key` for a
/// reason no assertion names.
fn secret(tag: &str) -> String {
    format!("rh_turn_{tag:A<43}")
}

fn configured() -> Arc<ControlPlane> {
    let json = serde_json::json!({
        "projects": [{ "id": "acme" }, { "id": "globex" }],
        "users": [{ "id": "ada" }, { "id": "bob" }],
        "keys": [
            { "project": "acme", "user": "ada", "key_sha256": sha256_hex(&secret("acme")) },
            { "project": "globex", "user": "bob", "key_sha256": sha256_hex(&secret("globex")) },
        ],
    })
    .to_string();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "relay fixture").expect("the fixture must validate"),
    ))
}

fn decision(chosen: Target) -> DecisionRecord {
    let rate_card = if chosen.is_local() {
        None
    } else {
        Some(HOSTED)
    };
    DecisionRecord {
        attempts: Vec::new(),
        declared_baseline: None,
        chosen,
        rationale: "warmest prefix".into(),
        policy: "affinity".into(),
        isl_tokens: 1_000,
        expected_prefill_tokens: 1_000.0,
        expected_cost_usd: 0.01,
        considered: Vec::new(),
        turn_policy_digest: "0123456789abcdef".into(),
        budget_state: BudgetState::Unconstrained,
        rate_card,
        payer: Payer::Deployment,
        billing: Billing::Billed,
        budget_draw: None,
        withheld_providers: Vec::new(),
    }
}

/// One session in a store, with `turns` completed frontier turns in it.
async fn store_with(
    session_id: &str,
    principal: Option<Principal>,
    turns: usize,
) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::new());
    let session_id = SessionId::new(session_id);
    store
        .create_session(&session_id, "affinity")
        .await
        .expect("a fresh session");
    let lease = store
        .acquire_lease(&session_id, "test", 60_000)
        .await
        .expect("the store answers")
        .expect("nobody else holds it");

    let mut kinds = vec![SessionEventKind::SessionCreated {
        model_policy: "affinity".into(),
        principal,
        arm: None,
    }];
    for turn in 0..turns {
        let response_id = ResponseId::new(format!("r{turn}"));
        kinds.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new(format!("t{turn}")),
            response_id: response_id.clone(),
        });
        kinds.push(SessionEventKind::ItemAppended {
            item: Item::user_text(format!("ask {turn}")),
        });
        kinds.push(SessionEventKind::Routed {
            response_id: response_id.clone(),
            decision: decision(Target::Frontier {
                provider: "anthropic".into(),
                model: "claude".into(),
            }),
        });
        kinds.push(SessionEventKind::OutputTextDelta {
            response_id: response_id.clone(),
            text: format!("answer {turn}"),
        });
        kinds.push(SessionEventKind::ItemAppended {
            item: Item::assistant_text(format!("answer {turn}"), response_id.clone()),
        });
        kinds.push(SessionEventKind::ResponseCompleted {
            provider_reported_cost_usd: None,
            stop_reason: None,
            response_id,
            usage: Usage {
                input_tokens: 1_000,
                cached_input_tokens: 400,
                output_tokens: 50,
                ..Usage::default()
            },
        });
    }
    store
        .append_events(&lease, kinds)
        .await
        .expect("the fixture appends");
    store
}

fn app(plane: Arc<ControlPlane>, store: Arc<MemoryStore>) -> Router {
    relay_api::relay_router(plane, store, pricing())
}

async fn get(app: &Router, uri: &str, key: Option<&str>) -> (StatusCode, String, String) {
    let mut request = Request::builder().uri(uri).method("GET");
    if let Some(key) = key {
        request = request.header(AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .expect("the router answers");
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .map(|value| value.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8(body.to_vec()).unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_three_routes_project_one_session_three_ways() {
    let store = store_with("solo", None, 2).await;
    let app = app(ControlPlane::open(), store);

    let (status, content_type, body) = get(&app, "/v1/sessions/solo/atof", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        content_type, "application/x-ndjson",
        "ATOF is stored as one event per line, and the converter's reader \
         expects exactly that"
    );
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(
        lines.len(),
        10,
        "one agent span plus, per turn, a route span and an llm span: {body}"
    );
    for line in &lines {
        let event: serde_json::Value = serde_json::from_str(line).expect("each line is an event");
        assert_eq!(event["kind"], "scope");
        assert_eq!(event["atof_version"], "0.1");
    }

    let (status, content_type, body) = get(&app, "/v1/sessions/solo/trajectory", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/json");
    let trajectory: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(trajectory["schema_version"], "ATIF-v1.7");
    assert_eq!(trajectory["session_id"], "solo");
    assert_eq!(trajectory["steps"].as_array().unwrap().len(), 4);
    assert_eq!(trajectory["final_metrics"]["total_prompt_tokens"], 2_000);

    let (status, content_type, body) = get(&app, "/v1/sessions/solo/optimization", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/json");
    let summaries: serde_json::Value = serde_json::from_str(&body).unwrap();
    let summaries = summaries.as_array().expect("one summary per turn");
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0]["schema_version"], "1");
    assert_eq!(summaries[0]["status"], "complete");
    assert_eq!(summaries[0]["contributions"][0]["producer"], "roundhouse");

    // The three documents agree about the same session, which is the point of
    // one replay behind all three.
    assert_eq!(
        trajectory["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|step| step["source"] == "agent")
            .count(),
        summaries.len(),
        "one dispatched turn is one agent step and one summary"
    );
}

/// The same claim `a_native_surface_session_outside_the_callers_namespace_is_refused`
/// makes for the streaming routes, for the three that project the same log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_outside_the_callers_namespace_is_refused_on_every_route() {
    // Globex's session really exists, so a refusal that only worked on absent
    // sessions would prove nothing.
    let store = store_with(
        "globex/bob/private",
        Some(Principal::new("globex", "bob")),
        1,
    )
    .await;
    let app = app(configured(), store);
    let theirs = path_segment("globex/bob/private");

    for document in ["atof", "trajectory", "optimization"] {
        let uri = format!("/v1/sessions/{theirs}/{document}");
        let (status, _, body) = get(&app, &uri, Some(&secret("acme"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "for {uri}: {body}");
        let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(payload["error"]["code"], "session_out_of_namespace");
        assert!(
            !payload["error"]["message"]
                .as_str()
                .expect("a message")
                .contains("not found"),
            "the refusal must not say whether the session exists: {body}"
        );

        // CONTROL: the owner is served, so the refusals above are about the
        // namespace and not about the route being broken for everyone.
        let (status, _, body) = get(&app, &uri, Some(&secret("globex"))).await;
        assert_eq!(status, StatusCode::OK, "for {uri}: {body}");
    }

    // And a request with no key at all is refused before any of it: a session's
    // raw log is not a public document once there is more than one tenant.
    let (status, _, body) = get(&app, &format!("/v1/sessions/{theirs}/trajectory"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_does_not_exist_is_a_404() {
    let store = store_with("solo", None, 1).await;
    let app = app(ControlPlane::open(), store);
    let (status, _, body) = get(&app, "/v1/sessions/nobody/trajectory", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(payload["error"]["code"], "session_not_found");
}

/// A long session must not be silently truncated.
///
/// `SessionStore::read_events` returns at most `limit`, so a handler that read
/// one page would publish a trajectory that stopped part-way through with a
/// `200 OK` and no indication that anything was missing. Sixty turns is 361
/// events, comfortably past the transport's batch size.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_longer_than_one_store_batch_is_read_whole() {
    const TURNS: usize = 60;
    let store = store_with("long", None, TURNS).await;
    assert!(
        store.last_seq(&SessionId::new("long")).await.unwrap() > 256,
        "the fixture has to actually exceed one batch, or this test is vacuous"
    );
    let app = app(ControlPlane::open(), store);

    let (status, _, body) = get(&app, "/v1/sessions/long/trajectory", None).await;
    assert_eq!(status, StatusCode::OK);
    let trajectory: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        trajectory["steps"].as_array().unwrap().len(),
        TURNS * 2,
        "every turn, not just the first page of them"
    );
    assert_eq!(
        trajectory["final_metrics"]["total_prompt_tokens"],
        (TURNS as u64) * 1_000
    );

    let (_, _, body) = get(&app, "/v1/sessions/long/optimization", None).await;
    let summaries: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(summaries.as_array().unwrap().len(), TURNS);

    let (_, _, body) = get(&app, "/v1/sessions/long/atof", None).await;
    assert_eq!(body.lines().count(), TURNS * 4 + 2);
}
