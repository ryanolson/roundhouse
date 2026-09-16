// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.1 thermo-nuclear review, finding F2 — refuted.
//!
//! **The claim.** `Conversations::generation` answers from the node-local
//! memo forever once a key is memoised, because nothing but this node's own
//! `commit` ever updates it. R-C2's premise that a stale memo costs "a walk of
//! one or two extra reads" is false once the memo has fallen more than
//! `MAX_PREFIX_PROBES` (8) generations behind the store: with the memo at 0
//! and the store at 9, `bind_prefix`'s upward walk probes 1..=8, never reaches
//! 9, and refuses with `prefix_admission_exhausted`. A refusal commits
//! nothing, so the memo stays 0 and every retry on that node is refused
//! identically, while a node with no memo at all serves the same claim on its
//! very first probe.
//!
//! Gated as every Redis-touching suite in this tree: `#[ignore]`, opted into
//! with `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL` fails
//! loudly rather than skipping quietly.
//!
//! # Topology
//!
//! Three `Conversations`, three independent `RedisCorrelationMaps`
//! connections, one shared `MemoryStore`/`Engine`/`ControlPlane` behind them
//! — the same "one process, three sockets" shape `correlation_any_node.rs`
//! uses to stand in for three real nodes, driven here through the live
//! `/v1/responses` surface so the memo and the commit are the real turn path's
//! (`prefix_admission::bind_prefix`) and not a fixture's shortcut:
//!
//! - **A** posts the opening turn, memoising generation 0.
//! - **B** posts nine sequential divergent-history turns on the same cache
//!   key, each one forking the conversation one generation further —
//!   0→1→2→…→9 — landing the store's generation counter at 9. B's own memo
//!   tracks each fork it makes, so this is not the defect; it is what puts the
//!   store nine generations ahead of a node that never saw any of it.
//! - **C**, a third node that served none of this, posts the generation-9
//!   history fresh and is admitted at once — the control that proves a
//!   memo-less node pays no penalty for the same gap.
//! - **A** posts the generation-9 history last. Its memo is still 0, the
//!   upward walk reaches only 1..=8, and every generation it can reach
//!   disagrees with the claim. The finding says this refuses with 409 and
//!   stays refused on a verbatim retry; that is the assertion under test.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{CorrelationMaps, MemoryCorrelationMaps, Principal};
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::EchoFrontierClient;
use roundhouse_server::conversations::bound_session;
use roundhouse_server::{ControlPlane, Conversations, EchoLocalExecutor, Engine, responses_api};
use roundhouse_store_redis::RedisCorrelationMaps;
use roundhouse_store_redis::test_support::url_from_env;

mod common;
use common::codex::{request, user_message};
use common::{config, control_plane, frontier_catalog, key, sha256_hex};

/// One node: its own connection to the Redis every node shares, exactly as
/// `correlation_any_node.rs`'s `node()` builds one — a separate `connect`
/// rather than a cloned handle, because what is under test is three
/// processes' worth of memo state and two of them sharing a connection would
/// prove less than the claim.
async fn node() -> Conversations {
    let maps = RedisCorrelationMaps::connect(url_from_env())
        .await
        .expect("Redis named by the env var must be reachable");
    Conversations::over(Arc::new(maps) as Arc<dyn CorrelationMaps>)
}

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
        "F2 fixture",
    ))
}

fn app_for(
    conversations: Arc<Conversations>,
    engine: &Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: &Arc<MemoryStore>,
    plane: &Arc<ControlPlane>,
) -> Router {
    responses_api::responses_router(
        Arc::clone(plane),
        Arc::clone(engine),
        Arc::clone(store),
        conversations,
    )
}

/// One `POST /v1/responses` with an arbitrary claimed history, draining the
/// SSE body so `prefix_admission::bind_prefix`'s commit actually runs before
/// this returns. Returns the status so a refusal is an assertion, not a
/// panic.
async fn post(app: &Router, secret: &str, cache_key: &str, text: &str) -> StatusCode {
    let body = request(cache_key, vec![user_message(text)]);
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

/// One node's worth of state after the topology's setup phase: the app that
/// drives a real turn through it, plus what a caller needs to keep asserting
/// against it.
struct Node {
    conversations: Arc<Conversations>,
    app: Router,
}

/// Builds the shared store/engine/plane and the three nodes, then drives the
/// setup phase every test below needs: A opens the key at generation 0, and B
/// forks it nine times in a row (0 -> 1 -> ... -> 9), each fork's claim
/// distinct so a probe cannot accidentally agree with an earlier generation's
/// content. Returns before the claim under test, so both the passing control
/// and the failing claim start from the identical, freshly-built topology.
async fn setup() -> (Node, Node, Node, Arc<ControlPlane>, String) {
    setup_over([
        Arc::new(node().await),
        Arc::new(node().await),
        Arc::new(node().await),
    ])
    .await
}

/// [`setup`]'s topology over whichever maps the caller's three nodes hold, so
/// the durable three-socket shape and the in-process twin below drive the
/// *same* nine forks through the same surface. Two spellings of the setup
/// would be two chances for the twin to stop standing for the gated test.
async fn setup_over(
    nodes: [Arc<Conversations>; 3],
) -> (Node, Node, Node, Arc<ControlPlane>, String) {
    let [a_conversations, b_conversations, c_conversations] = nodes;
    let secret = key("f2stale");
    let plane = Arc::new(plane(&secret));
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

    let run = uuid::Uuid::new_v4();
    let cache_key = format!("f2-{run}");

    let app_a = app_for(Arc::clone(&a_conversations), &engine, &store, &plane);
    let app_b = app_for(Arc::clone(&b_conversations), &engine, &store, &plane);
    let app_c = app_for(Arc::clone(&c_conversations), &engine, &store, &plane);

    // A posts the opening turn: generation 0, memoised on A.
    let opened = post(&app_a, &secret, &cache_key, "turn-zero").await;
    assert_eq!(
        opened,
        StatusCode::OK,
        "sanity: the opening turn is admitted"
    );

    // B posts nine sequential divergent-history turns, each one forking the
    // conversation one generation further: 0 -> 1 -> ... -> 9.
    for step in 1..=9u32 {
        let status = post(&app_b, &secret, &cache_key, &format!("fork-{step}")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "sanity: B's own fork #{step} must be admitted -- B's memo tracks \
             every commit it makes itself, which is why this is not the \
             defect under test"
        );
    }

    (
        Node {
            conversations: a_conversations,
            app: app_a,
        },
        Node {
            conversations: b_conversations,
            app: app_b,
        },
        Node {
            conversations: c_conversations,
            app: app_c,
        },
        plane,
        cache_key,
    )
}

/// **Sanity and control, kept live.** Neither assertion here is F2's claim —
/// they establish the ground the claim stands on, and both must hold or the
/// failing test below would be measuring nothing:
///
/// - A's own *reader* (`resolve`, which always goes to the store — see the
///   module doc) already answers generation 9, on the very node whose *turn*
///   path is about to refuse the identical claim.
/// - A third node (C) that served none of this history is admitted for the
///   generation-9 claim on its very first probe. A memo-less node pays no
///   penalty for the nine-generation gap A is about to be refused over,
///   which is what isolates the coming failure to A's *stale memo*, not to
///   the claim itself.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn sanity_the_store_already_has_the_answer_and_a_memo_less_node_is_admitted_at_once() {
    let (a, _b, c, plane, cache_key) = setup().await;

    let principal = Principal::new("acme".to_string(), "ada".to_string());
    let qualified = plane.qualify(&principal, &cache_key);
    assert_eq!(
        a.conversations.resolve(&qualified).await.unwrap(),
        Some(bound_session(&qualified, 9)),
        "sanity: A's resolve() reads the store directly and must already see \
         generation 9"
    );

    let control = post(&c.app, &key("f2stale"), &cache_key, "fork-9").await;
    assert_eq!(
        control,
        StatusCode::OK,
        "control: a memo-less node must be admitted for the exact same claim \
         A is refused for below -- otherwise the defect would be about the \
         claim, not about A's stale memo"
    );
}

/// **F2, closed.** A's memo is nine generations stale (`MAX_PREFIX_PROBES` is
/// 8), so the upward walk reaches 1..=8 and never the store's generation 9.
/// Before the fix that was a `409 prefix_admission_exhausted` on every
/// attempt, since a refusal commits nothing and the memo stayed at zero.
/// Under R-C2″ a walk that runs off its bound asks the store for a fresh hint
/// and searches once more from it, so the turn the store places in one read is
/// admitted — named for what it asserts now; the finding's own spelling of the
/// defect is the module doc above.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_memo_more_than_max_prefix_probes_stale_is_refreshed_rather_than_refusing_the_turn() {
    let (a, _b, _c, _plane, cache_key) = setup().await;

    // THE CLAIM: A posts the generation-9 history. Correct behaviour is 200 —
    // the store already holds this exact history at generation 9, one read
    // away from anyone who asks. F2 says this fails, with 409, instead.
    let admitted = post(&a.app, &key("f2stale"), &cache_key, "fork-9").await;
    assert_eq!(
        admitted,
        StatusCode::OK,
        "F2: expected the turn to be admitted -- the store already holds this \
         exact history at generation 9 -- but A's memo is stuck at 0 and the \
         upward walk cannot reach past current+MAX_PREFIX_PROBES (0+8=8)"
    );

    // And the retry: a refusal commits nothing (conversations.rs's own doc,
    // "deliberately a read: it records nothing in the store and moves
    // nothing"), so a verbatim retry after the first refusal is refused
    // identically rather than resuming past the bound. Reached only if the
    // assertion above somehow passed (i.e. once this is fixed), at which
    // point it is a second, independent confirmation that the fix does not
    // regress the ordinary case.
    let retried = post(&a.app, &key("f2stale"), &cache_key, "fork-9").await;
    assert_eq!(
        retried,
        StatusCode::OK,
        "the same claim, resent, must be admitted the same way"
    );
}

/// **F2's twin over the in-process maps**, live rather than gated: three
/// `Conversations` over *one* [`MemoryCorrelationMaps`], which is the same
/// topology the gated test builds out of three Redis sockets — a node whose
/// memo another node ran nine generations past.
///
/// Here so the guard on R-C2″ runs in the ordinary suite and not only where a
/// Redis is standing: the defect is the memo's, not the durable store's, and a
/// deployment with two nodes sharing an in-process map is exactly what the
/// contract macro says the two implementations owe each other.
#[tokio::test]
async fn the_same_stale_hint_over_the_memory_maps_is_refreshed_too() {
    let maps: Arc<dyn CorrelationMaps> = Arc::new(MemoryCorrelationMaps::new());
    let (a, _b, c, plane, cache_key) = setup_over([
        Arc::new(Conversations::over(Arc::clone(&maps))),
        Arc::new(Conversations::over(Arc::clone(&maps))),
        Arc::new(Conversations::over(Arc::clone(&maps))),
    ])
    .await;

    let principal = Principal::new("acme".to_string(), "ada".to_string());
    let qualified = plane.qualify(&principal, &cache_key);
    assert_eq!(
        a.conversations.resolve(&qualified).await.unwrap(),
        Some(bound_session(&qualified, 9)),
        "sanity: the shared map already holds generation 9"
    );

    let control = post(&c.app, &key("f2stale"), &cache_key, "fork-9").await;
    assert_eq!(
        control,
        StatusCode::OK,
        "control: a memo-less node is admitted for this claim at once"
    );

    let admitted = post(&a.app, &key("f2stale"), &cache_key, "fork-9").await;
    assert_eq!(
        admitted,
        StatusCode::OK,
        "F2: A's memo is at 0 and the walk reaches only 1..=8, so the search \
         runs off its bound -- and a hint that does that is refreshed from the \
         map before anything is refused"
    );
}
