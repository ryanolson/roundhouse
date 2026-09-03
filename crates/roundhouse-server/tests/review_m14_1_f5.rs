// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.1 thermo-nuclear review, finding F5 — refuted, then fixed.
//!
//! **The claim, narrowed to what a test can reach.** F5 bundles four
//! doc/code mismatches; the ordering half is the only one with a resolver
//! behavior to disagree about, so it is what this file proves. README.md:60
//! said codex's session id (the `x-codex-turn-metadata.session_id` cache key)
//! "is weighed last of the three" correlators. `ControlReads::resolve_session`
//! (`reads.rs:322-339`) matches `(named, thread.or(cache_key).or(call))` —
//! `.or()` short-circuits left to right, so when the thread arm answers
//! nothing and the cache-key arm answers *something*, the call arm
//! (`correlators.tool_use_id`, Claude Code's `claudecode/toolUseId`) is never
//! consulted at all. The cache key is weighed **before** the call, i.e.
//! second of the three, not last — the call is what is actually weighed
//! last. The PLAN addendum (`agent-docs/PLAN-anthropic-messages.md`) already
//! said as much in prose ("the arm sits after the thread arm and before the
//! tool-use id"), which is what this test turns into a resolver disagreement
//! an agent could actually hit: a codex call whose thread is unbound, whose
//! cache key names one conversation, and whose `claudecode/toolUseId` names
//! a *different* one it already emitted a tool call into.
//!
//! **Why this needs a real Redis and not `Conversations::new()`'s in-memory
//! maps.** `ControlReads` is `mcp_api::ControlPlaneReads<S>`, generic only
//! over the session store; the correlation maps underneath `Conversations`
//! are already behind `Arc<dyn CorrelationMaps>` (R-C4) specifically so this
//! ordering is one function regardless of which implementation answers
//! `generation`/`session_of_call`/`session_of_thread` — the module doc's own
//! words are "every method below reads identically either way". A refuter
//! could reach for the in-memory maps `mcp_surface.rs` already uses and prove
//! the identical disagreement with no Redis at all; this file uses
//! `RedisCorrelationMaps` anyway; so the resolver order is shown to hold over
//! the actual store a deployment runs on, not merely over the pure function's
//! easiest double, and any future coupling between the ordering and the
//! store's own read/write shape would show up here rather than only in a
//! fixture that never touched it.
//!
//! Gated as every Redis-touching suite in this tree: `#[ignore]`, opted into
//! with `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL` fails
//! loudly rather than skipping quietly.
//!
//! **Ruling: partially valid — and fixed the way the resolver, not the doc,
//! was already right about.** The ordering half of F5 was confirmed by the
//! test below at refute time: the resolved conversation was the cache key's,
//! not the call's, contradicting README's old "weighed last of the three".
//! But `reads.rs`'s own order — thread, then cache key, then call — is the
//! order R-C5 actually ruled (the PLAN addendum already had it right), so the
//! fix corrects the *sentence*, not the resolver: README now says the cache
//! key is weighed after the thread arm and before the tool-use id, and the
//! test below is turned from a red assertion of README's old (wrong) claim
//! into a live confirmation of the resolver's real order, over a real Redis.
//! The finding's other three claims — lib.rs's self-contradiction about
//! where an aged-out thread ends up, and the stale "this node" scope
//! language in `surface.rs`/`mcp_api.rs` — are prose-only defects with no
//! resolver behavior a test can disagree with (the aged-out-thread
//! contradiction is in fact already provable, without Redis, by the
//! *existing* passing test
//! `a_codex_call_falls_back_to_the_cache_key_its_turn_metadata_carries` in
//! `mcp_surface.rs`, which resolves an aged-out thread to its own family's
//! conversation and never to `latest` — matching lib.rs's earlier paragraph
//! and contradicting its later one, now fixed too). Those three were
//! grep/sed matters, as the finding's own `how_to_prove` said, and were
//! fixed as doc edits rather than restated as a second test here.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HOST};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use roundhouse_core::control::{CorrelationMaps, MemorySpendLedger, Principal, SpendLedger};
use roundhouse_core::ids::SessionId;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_mcp::ControlStore;
use roundhouse_server::test_support::bind_conversation;
use roundhouse_server::{ControlPlane, ControlPlaneReads, Conversations, mcp_router};
use roundhouse_store_redis::RedisCorrelationMaps;
use roundhouse_store_redis::test_support::url_from_env;

mod common;
use common::{control_plane, key, sha256_hex};

/// One node's worth of `Conversations`, over the real Redis the environment
/// names — the same pattern `review_m14_1_f2.rs`'s `node()` uses.
async fn node() -> Conversations {
    let maps = RedisCorrelationMaps::connect(url_from_env())
        .await
        .expect("Redis named by the env var must be reachable");
    Conversations::over(Arc::new(maps) as Arc<dyn CorrelationMaps>)
}

fn plane(secret: &str) -> Arc<ControlPlane> {
    Arc::new(ControlPlane::configured(control_plane(
        json!({
            "projects": [{ "id": "acme" }],
            "users": [{ "id": "ada" }],
            "keys": [{
                "project": "acme", "user": "ada",
                "key_sha256": sha256_hex(secret),
            }],
        }),
        "F5 fixture",
    )))
}

/// The `/mcp` router alone: F5's claim lives entirely in
/// `ControlReads::resolve_session`, so no engine, no `/v1/responses` and no
/// frontier client are needed to reach it — only a store that can answer
/// `named_session`'s existence check, and the correlation maps underneath
/// `Conversations` that carry the three correlators.
fn app_for(
    plane: &Arc<ControlPlane>,
    store: &Arc<MemoryStore>,
    conversations: Arc<Conversations>,
) -> Router {
    let spend: Arc<dyn SpendLedger> = Arc::new(MemorySpendLedger::new());
    let reads = Arc::new(ControlPlaneReads::new(
        Arc::clone(plane),
        Arc::clone(store),
        spend,
        conversations,
        Vec::new(),
    ));
    mcp_router(Arc::clone(plane), reads, Arc::new(ControlStore::new()))
}

/// Opens a conversation with [`bind_conversation`] alone (M14.0's "fixture
/// standing a conversation up without driving a turn through it", the shape
/// `Conversations::bind` used to spell before M15's H1 moved it here) plus
/// what `named_session`'s existence check separately requires: a session
/// record in the store, since the read path checks `last_seq` and not only
/// the correlation maps' generation entry. `create_session` is enough — an
/// empty log still answers `last_seq` `Ok(0)`.
async fn open_conversation(
    conversations: &Conversations,
    store: &MemoryStore,
    principal: &Principal,
    qualified_key: &str,
) -> SessionId {
    let session = bind_conversation(conversations, principal, qualified_key).await;
    store
        .create_session(&session, "policy")
        .await
        .expect("a fresh session id must be creatable");
    session
}

async fn post(app: &Router, secret: &str, body: &Value) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header(HOST, "roundhouse.internal.example.com")
        .header(ACCEPT, "application/json, text/event-stream")
        .header(AUTHORIZATION, format!("Bearer {secret}"))
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("served");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body reads")
        .to_bytes();
    (status, String::from_utf8(bytes.to_vec()).expect("utf8"))
}

fn served_conversation(reply_text: &str) -> String {
    let reply: Value =
        serde_json::from_str(reply_text).unwrap_or_else(|e| panic!("`{reply_text}`: {e}"));
    let result = &reply["result"];
    assert_eq!(
        result["isError"],
        json!(false),
        "F5: the `status` tool refused instead of resolving: {result}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .expect("F5: a served tool result carries a text block");
    let body: Value = serde_json::from_str(text).expect("F5: the text block is JSON");
    body["conversation"]
        .as_str()
        .expect("F5: `status` names a conversation")
        .to_string()
}

/// F5's ordering claim, made into a resolver disagreement: a thread the
/// deployment never bound (so the thread arm answers nothing), a cache key
/// naming `main`, and a `claudecode/toolUseId` already bound to `other` —
/// the same call carrying both the correlator README used to claim was
/// weighed last and the one `reads.rs`'s `.or()` chain actually consults
/// last (`call`).
///
/// README (fixed) now says the cache key is weighed *before* the tool-use
/// id, so a caller who also supplies an exact `claudecode/toolUseId` naming a
/// different, real conversation should still have the cache key win —
/// `reads.rs`'s own order (thread, then cache key, then call) is unchanged by
/// this rung, because it was the doc that was wrong, not the resolver. The
/// assertion below is what used to be README's (wrong) claim, now flipped to
/// match the resolver `reads.rs` actually ships and the doc now says.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_family_cache_key_outranks_a_bound_tool_use_id() {
    let secret = key("f5order");
    let plane = plane(&secret);
    let store = Arc::new(MemoryStore::new());
    let ada = Principal::new("acme", "ada");

    let conversations = Arc::new(node().await);
    let app = app_for(&plane, &store, Arc::clone(&conversations));

    let main_session = open_conversation(&conversations, &store, &ada, "acme/ada/main").await;
    let other_session = open_conversation(&conversations, &store, &ada, "acme/ada/other").await;
    assert_ne!(
        main_session, other_session,
        "sanity: two distinct conversations"
    );

    // What a dispatched turn on `other` writes as it streams a tool call —
    // the exact write `a_tools_call_is_correlated_by_the_tool_use_id_the_client_quotes_back`
    // (`mcp_surface.rs`) exercises the same way.
    conversations
        .bind_call(&ada, "toolu_from_other", other_session.clone())
        .await;

    // The thread id here is nobody's — never bound, never a name either
    // conversation was opened under — so `resolve_session`'s thread arm
    // answers `None` and both remaining arms are live: cache key names
    // `main`, call names `other`.
    let (status, text) = post(
        &app,
        &secret,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "status",
                "arguments": {},
                "_meta": {
                    "threadId": "thread-nothing-bound",
                    "x-codex-turn-metadata": { "session_id": "main" },
                    "claudecode/toolUseId": "toolu_from_other",
                },
            },
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "`status` should have been served: {text}"
    );

    let resolved = served_conversation(&text);
    assert_eq!(
        resolved,
        main_session.as_str(),
        "F5: README (fixed) says the cache key (here naming `main`) is \
         weighed *before* the tool-use id, so it should win over an exact \
         `claudecode/toolUseId` naming `other`. It did not: resolve_session \
         answered {resolved:?} instead of the cache key's session -- \
         `reads.rs`'s `.or()` chain (thread.or(cache_key).or(call)) should \
         never reach `call` once `cache_key` has already answered."
    );
}

/// CONTROL, kept live: with no `claudecode/toolUseId` in play at all, the
/// unbound thread plus the cache key resolves to `main` exactly as
/// `mcp_surface.rs`'s
/// `a_codex_call_falls_back_to_the_cache_key_its_turn_metadata_carries`
/// already proves — establishing that the cache-key arm here is answering
/// at all, so the failing assertion above is about the ordering and not
/// about the cache-key arm being silently unreachable in this fixture.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn control_the_cache_key_alone_resolves_the_unbound_thread() {
    let secret = key("f5control");
    let plane = plane(&secret);
    let store = Arc::new(MemoryStore::new());
    let ada = Principal::new("acme", "ada");

    let conversations = Arc::new(node().await);
    let app = app_for(&plane, &store, Arc::clone(&conversations));

    let main_session = open_conversation(&conversations, &store, &ada, "acme/ada/main").await;

    let (status, text) = post(
        &app,
        &secret,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "status",
                "arguments": {},
                "_meta": {
                    "threadId": "thread-nothing-bound",
                    "x-codex-turn-metadata": { "session_id": "main" },
                },
            },
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "`status` should have been served: {text}"
    );

    let resolved = served_conversation(&text);
    assert_eq!(resolved, main_session.as_str());
}
