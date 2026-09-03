// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.1 thermo-nuclear review, F1: the composition root's own choice of
//! correlation maps, unexercised.
//!
//! `main.rs` picks `Conversations::over(RedisCorrelationMaps)` or
//! `Conversations::new()` by matching on [`shared_backend`], the same match
//! [`fair_use_backend_boot.rs`](../tests/fair_use_backend_boot.rs) proves for
//! the fair-use ledger. That file exists because the M13 review's F1 found
//! the fair-use half of this exact wiring silently detached from the rule its
//! own boot log claimed to follow; nothing analogous existed for the
//! correlation maps this rung added, even though `shared_backend`'s own
//! module doc now calls them "the fourth family chosen by
//! `ROUNDHOUSE_REDIS_URL`, beside the session log, the spend ledger and the
//! fair-use buckets" — three of which had, or already had, an end-to-end boot
//! assertion. This file is the fourth.
//!
//! # Why re-deriving the match is still the right shape
//!
//! `main.rs` is a `[[bin]]` source with no `[lib]` counterpart, so nothing
//! outside it can call its composition root directly — the same limitation
//! `fair_use_backend_boot.rs` names in its own doc. What is testable, and
//! what actually carries the risk, is whether the *pattern* main.rs follows
//! — [`shared_backend`]'s `Shared` arm wired to `RedisCorrelationMaps`, its
//! `PerProcess` arm to the in-process table — reaches a real, independent
//! Redis handle end-to-end. This mirrors that match verbatim rather than
//! reimplementing its own idea of what main.rs should do, so a diff that
//! changes main.rs's wiring without changing this one is visible on review as
//! the two drifting apart, the same discipline the fair-use file already
//! established.
//!
//! Gated like every Redis-touching suite in this tree: `#[ignore]`, opted
//! into with `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL`
//! fails loudly rather than skipping quietly.

use std::sync::Arc;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{CorrelationMaps, Principal};
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::EchoFrontierClient;
use roundhouse_server::{
    ControlPlane, Conversations, EchoLocalExecutor, Engine, SharedBackend, responses_api,
    shared_backend,
};
use roundhouse_store_redis::RedisCorrelationMaps;
use roundhouse_store_redis::test_support::url_from_env;

mod common;
use common::codex::{request, user_message};
use common::{config, control_plane, frontier_catalog, key, sha256_hex};

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// One project, one user, one key — enough to authenticate a turn and
/// nothing the finding needs the admin plane's own mutation path for. The
/// fair-use file provisions through the admin API because *its* claim is
/// about a config that changes after boot; this one is about which maps a
/// boot-time choice wires, so a static file is the honest fixture.
fn plane(secret: &str) -> ControlPlane {
    ControlPlane::configured(control_plane(
        serde_json::json!({
            "projects": [{ "id": "acme", "policy": { "min_quality": 0.1 } }],
            "users": [{ "id": "ada" }],
            "keys": [{
                "project": "acme", "user": "ada",
                "key_sha256": sha256_hex(secret),
            }],
        }),
        "correlation boot fixture",
    ))
}

/// One `POST /v1/responses`, draining the SSE body so the turn's tail —
/// including the `Conversations::commit` inside `prefix_admission::bind_prefix`
/// — actually runs before this function returns.
async fn post(app: &Router, secret: &str, cache_key: &str) -> StatusCode {
    let body = request(cache_key, vec![user_message("count some tokens")]);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let _ = response.into_body().collect().await.unwrap().to_bytes();
    status
}

/// **The defect cell, proved rather than asserted.**
///
/// 1. Resolve `Conversations` by calling [`shared_backend`] and matching on
///    it exactly as `main.rs` does — the `Shared` arm connects
///    `RedisCorrelationMaps` and wraps it with `Conversations::over`, the
///    `PerProcess` arm builds `Conversations::new()`. Nothing here re-derives
///    the rule; it re-derives only the wiring the rule already chose.
/// 2. Drive one real turn against a fresh cache key through the live
///    `/v1/responses` surface, so the commit happens on the real turn path
///    (`prefix_admission::bind_prefix`) and not a fixture's shortcut.
/// 3. Assert from a **second, independently connected** `RedisCorrelationMaps`
///    that the binding this node made is visible — proof the wiring reached
///    the shared store a second node would also connect to, not this
///    process's own memory.
///
/// A fresh project id per run so the assertion is about what this run wrote:
/// the keys this family uses carry a staleness bound (R-C3) rather than a
/// fixed TTL tied to any one run's cache key, so a stale leftover key from an
/// earlier run must not be able to satisfy this assertion in its place.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_wired_conversations_reach_the_named_redis_not_this_processs_memory() {
    let url = url_from_env();
    let run = uuid::Uuid::new_v4();
    let secret = key("corrboot");
    let cache_key = format!("cache-{run}");

    // The composition root's own choice, re-derived by matching on the same
    // function main.rs calls -- not by asserting "it must be Redis" as a
    // fixture's premise.
    let conversations = Arc::new(match shared_backend(Some(&url)) {
        SharedBackend::Shared { url } => {
            let maps = RedisCorrelationMaps::connect(url)
                .await
                .expect("the test Redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable");
            Conversations::over(Arc::new(maps) as Arc<dyn CorrelationMaps>)
        }
        SharedBackend::PerProcess => Conversations::new(),
    });

    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(EchoFrontierClient::new("frontier answer")),
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    let plane = Arc::new(plane(&secret));
    let app = responses_api::responses_router(
        Arc::clone(&plane),
        Arc::clone(&engine),
        Arc::clone(&store),
        Arc::clone(&conversations),
    );

    let status = post(&app, &secret, &cache_key).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the one turn this fixture drives must be admitted"
    );

    let principal = Principal::new("acme".to_string(), "ada".to_string());
    let qualified = plane.qualify(&principal, &cache_key);

    // **The assertion the finding turns on.** A *fresh* handle on the real
    // Redis -- a second node, in every way that matters -- must already see
    // the generation this turn committed. Before this file existed, nothing
    // in the workspace would have gone red had `main.rs`'s own match wired
    // `Conversations::new()` unconditionally: every other suite touching
    // `Conversations` builds it directly, bypassing the composition root's
    // choice entirely.
    let second_node = RedisCorrelationMaps::connect(&url)
        .await
        .expect("the test Redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable");
    let seen = second_node
        .generation(&qualified)
        .await
        .expect("the real Redis maps answer");
    assert_eq!(
        seen,
        Some(0),
        "F1: a cache key committed by the wired Conversations must be visible -- as generation \
         zero, the one the first turn against a fresh key opens -- to a second, independent \
         RedisCorrelationMaps handle on the Redis this deployment names. If this is None, the \
         composition root's `Shared` arm is not the arm that actually ran"
    );
}
