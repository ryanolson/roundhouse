// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M13 thermo-nuclear review, F1: the fair-use backend must not be a boot-time
//! snapshot of a runtime-patchable axis.
//!
//! `main.rs` reads whether any membership carries a `fair_use` block exactly
//! once, at boot. The admin plane accepts a `fair_use` block on a running
//! project afterwards (`admin_api.rs` `patch_project` ->
//! `DirectoryMutation::PatchProject` -> `directory.rs`), and the engine reads
//! that live plane on every request. So the *window* a turn is judged against
//! is live while the *ledger* it is judged through is fixed at boot: while the
//! ceiling was part of that choice, a deployment that booted with
//! `ROUNDHOUSE_REDIS_URL` set and no `fair_use` block anywhere enforced every
//! later-PATCHed ceiling in one process's memory, forever, with nothing ever
//! reaching the Redis it names for everything else — and no warning, because
//! the warning was computed from the same dead snapshot.
//!
//! [`fair_use_backend`] now follows `ROUNDHOUSE_REDIS_URL` alone, exactly as
//! the session store and the spend ledger do, and this is the end-to-end proof
//! of it: the ledger under the engine is resolved by calling that function —
//! not by re-deriving its rule here, which is the shape M13's own refute pass
//! caught catching nothing — and the assertion is against a *separate* handle
//! on the real Redis.
//!
//! Gated like the store's own integration tests: `#[ignore]`, opted into with
//! `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL` fails loudly.

use std::sync::Arc;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::spend::contract::fresh_principal;
use roundhouse_core::control::{
    FairUseLedger, FairUseWindow, MemoryFairUseLedger, MemorySpendLedger, Principal,
};
use roundhouse_core::now_ms;
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::EchoFrontierClient;
use roundhouse_server::control_config::crosscheck::CrossChecks;
use roundhouse_server::control_config::{FairUseConfig, FairUseWindowConfig, ProjectPatch};
use roundhouse_server::{
    Conversations, DirectoryMutation, EchoLocalExecutor, Engine, FairUseBackend,
    MemoryDirectoryStore, fair_use_backend, responses_api,
};
use roundhouse_store_redis::RedisFairUseLedger;
use roundhouse_store_redis::test_support::url_from_env;

mod common;
use common::codex::{request, user_message};
use common::{admin_key, config, control_plane, frontier_catalog, sha256_hex};

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// The file half of the fixture: an admin plane and nothing else. The
/// project, its member and the turn key are all minted through the API
/// below rather than declared here, on purpose: only an *admin-owned*
/// project can later be `PATCH`ed (`refuse_config_project` at
/// `directory.rs`, which refuses a patch to anything the file declared), and
/// the finding's premise is about a project an operator can still reach
/// through the admin plane after boot — exactly the shape a deployment
/// that provisions tenants through the API has.
fn boot_file() -> roundhouse_server::ControlPlaneConfig {
    control_plane(
        serde_json::json!({
            "projects": [],
            "users": [],
            "keys": [],
            "admin_keys": [sha256_hex(&admin_key("root"))],
        }),
        "F1 fixture",
    )
}

fn reachable() -> Vec<roundhouse_core::routing::Candidate> {
    vec![roundhouse_core::routing::Candidate {
        target: roundhouse_core::routing::Target::Frontier {
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

/// One `POST /v1/responses` as the fixture's membership, authenticating with
/// the secret `mint_turn_key` minted for it.
async fn post(app: &Router, secret: &str) -> StatusCode {
    let body = request("cache-key", vec![user_message("count some tokens")]);
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
    // Drain the SSE body so the turn's tail — including
    // `record_fair_use_draw` — actually runs before this function returns.
    let _ = response.into_body().collect().await.unwrap().to_bytes();
    status
}

/// **The defect cell, proved rather than asserted.**
///
/// 1. Boot a directory from a file with no `fair_use` block — the boot-time
///    snapshot is empty, exactly as the finding's premise requires.
/// 2. Resolve the engine's fair-use ledger by *calling* [`fair_use_backend`]
///    with what the composition root knows at that point — this deployment
///    names a Redis — and wiring whichever backend it answers, the way
///    `main.rs` does. Re-deriving the rule here instead is the shape M13's
///    own refute pass found catching nothing.
/// 3. PATCH the project through the same `DirectoryMutation::PatchProject`
///    the admin plane's `patch_project` route applies, adding a fair-use
///    window.
/// 4. Drive one real turn through the live-plane HTTP surface. The window is
///    live, and after this rung so is the ledger it is judged through.
/// 5. Assert the draw is where a second node would look for it: a
///    **separate, fresh `RedisFairUseLedger` against the same real Redis**
///    refuses the next turn under the very cap that was PATCHed in. Before
///    the fix that handle saw nothing at all — the ceiling lived and died in
///    one process's memory.
///
/// A fresh project id per run because the assertion is about what is *in*
/// Redis: a fixed id would let one green run's keys — they expire at the
/// widest window plus a bucket, not at process exit — make a later run pass
/// without the draw it claims to observe.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_ceiling_patched_in_after_boot_is_counted_in_the_shared_buckets() {
    let now = now_ms();
    let principal = fresh_principal("ada");
    let project = principal.project.to_string();
    let user = principal.user.to_string();
    let directory = Arc::new(
        roundhouse_server::ControlDirectory::new(
            boot_file(),
            "ROUNDHOUSE_CONTROL_PLANE",
            Arc::new(MemoryDirectoryStore::new()),
            CrossChecks::new(reachable(), None),
            now,
        )
        .expect("the file alone compiles, since it is what a boot would have loaded"),
    );

    // Provision the project and its member entirely through the admin plane's
    // own mutations, so the project is `Provenance::Admin` and therefore
    // patchable later -- the shape a deployment that provisions tenancy
    // through the API has, and the one the finding is about.
    directory
        .apply(
            DirectoryMutation::CreateProject {
                entry: roundhouse_server::control_config::ProjectEntry {
                    id: project.clone(),
                    name: None,
                    policy: None,
                    budget: None,
                    fair_use: None,
                    validate: None,
                    credentials: None,
                    tiers: None,
                },
            },
            now,
        )
        .expect("a fresh project id");
    directory
        .apply(
            DirectoryMutation::CreateUser {
                entry: roundhouse_server::control_config::UserEntry { id: user.clone() },
            },
            now,
        )
        .expect("a fresh user id");
    directory
        .apply(
            DirectoryMutation::UpsertMembership {
                project: project.clone(),
                user: user.clone(),
                role: roundhouse_server::control_config::MembershipRole::Member,
                allocation: None,
                overrides: None,
            },
            now,
        )
        .expect("a membership neither half declares yet");
    let turn_key = directory
        .mint_turn_key(&project, &user, now)
        .expect("minting a turn key for a membership that exists");

    // The premise, stated as an assertion rather than taken on faith: the
    // boot-time snapshot really does see no fair-use ceiling anywhere in this
    // plane. That snapshot is what used to decide the ledger.
    let fair_use_configured_at_boot = directory
        .plane(now)
        .configured_admissions()
        .any(|admission| !admission.fair_use.is_empty());
    assert!(
        !fair_use_configured_at_boot,
        "fixture premise: the file this deployment boots from must declare no fair-use window"
    );

    // The composition root's own choice, called rather than re-derived. This
    // deployment names a Redis; whether the boot snapshot happened to carry a
    // ceiling is not this function's business any more.
    let url = url_from_env();
    let fair_use_ledger: Arc<dyn FairUseLedger> = match fair_use_backend(Some(&url)) {
        FairUseBackend::Shared { url } => Arc::new(
            RedisFairUseLedger::connect(url)
                .await
                .expect("the test Redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable"),
        ),
        FairUseBackend::PerProcess => Arc::new(MemoryFairUseLedger::new()),
    };

    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local answer")),
            frontier_catalog(),
            Arc::new(EchoFrontierClient::new("frontier answer")),
            Arc::new(AffinityPolicy::new()),
            config(),
        )
        .with_spend_ledger(Arc::new(MemorySpendLedger::new()))
        .with_fair_use_ledger(Arc::clone(&fair_use_ledger)),
    );
    let app = responses_api::responses_router(
        Arc::clone(&directory),
        Arc::clone(&engine),
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );

    // The admin plane's own mutation, applied directly to the directory —
    // the exact `DirectoryMutation::PatchProject` the `patch_project` route
    // compiles. A tight cap so one echoed turn certainly crosses it.
    directory
        .apply(
            DirectoryMutation::PatchProject {
                id: project.clone(),
                patch: ProjectPatch {
                    fair_use: Some(Some(FairUseConfig {
                        windows: vec![FairUseWindowConfig {
                            window: FairUseWindow::FiveHours,
                            max_tokens: Some(1),
                            max_usd: None,
                        }],
                    })),
                    ..Default::default()
                },
            },
            now_ms(),
        )
        .expect("patching a live project's fair_use block is exactly what the admin plane allows");

    // The window is live: the very next admission already carries it, with no
    // restart. This is the half of the finding that was never in dispute.
    let principal = Principal::new(project.clone(), user.clone());
    let admission_after_patch = directory
        .plane(now_ms())
        .membership(&principal)
        .expect("the membership just provisioned still resolves");
    assert!(
        !admission_after_patch.fair_use.is_empty(),
        "the PATCHed window must be visible on the very next admission, with no restart -- \
         this is the live half of the seam, not the half under test"
    );

    // Drive one real turn large enough to cross a 1-token cap.
    let status = post(&app, &turn_key.secret).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the first turn against an empty window must be admitted"
    );

    // The ceiling really was crossed and really was enforced, through
    // whichever ledger the composition root chose.
    let breached = fair_use_ledger
        .would_exceed(&principal, &admission_after_patch.fair_use, now_ms())
        .await
        .expect("the wired ledger answers");
    assert!(
        breached.is_some(),
        "the draw from the one served turn must have pushed the wired ledger over its \
         1-token cap -- if this is None the fixture never exercised the ceiling at all"
    );

    // **The assertion the finding turns on.** A *fresh* handle on the real
    // Redis — a second node, in every way that matters — is over the same cap
    // the PATCH installed. Before this rung it saw nothing at all for this
    // principal, because the ledger had been chosen at boot from a snapshot
    // that predated the ceiling, and every draw landed in one process's
    // memory instead.
    let second_node = RedisFairUseLedger::connect(&url)
        .await
        .expect("the test Redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable");
    let shared = second_node
        .would_exceed(&principal, &admission_after_patch.fair_use, now_ms())
        .await
        .expect("the real Redis ledger answers");
    assert!(
        shared.is_some(),
        "F1: a ceiling PATCHed in after boot must be counted in the shared buckets. A \
         separate handle on the real Redis this deployment names sees no draw for this \
         principal at all, so a second node would serve the whole ceiling again"
    );
}
