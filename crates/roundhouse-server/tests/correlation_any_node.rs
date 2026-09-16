// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.1 (R-C6): the M12.1 handoffs, re-aimed from "this node" to "any node",
//! over a real Redis.
//!
//! Two `Conversations` sharing one `RedisCorrelationMaps` are two nodes of one
//! deployment: separate connections, separate `latest`, separate generation
//! memos, nothing in common but the store. Every assertion below was true of
//! *one* `Conversations` before this rung and false across two processes,
//! which is exactly the handoff inventory D1's R12 named.
//!
//! # Why this is here and not only in `roundhouse-store-redis`
//!
//! That crate's `tests/correlation_contract.rs` proves the *maps* answer
//! across two handles. What this file proves is the layer the deployment
//! actually calls: `Conversations`, with its `#g{n}` naming, its node-local
//! memo and its node-local `latest` — the three places a correct backend can
//! still be wrapped into a wrong answer. A cache that answered a reader would
//! pass every assertion in the store crate and fail the first one here.
//!
//! Gated as every Redis-touching suite in this tree is: `#[ignore]`, opted
//! into with `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL`
//! fails loudly rather than skipping quietly.

use std::sync::Arc;

use roundhouse_core::control::CorrelationMaps;
use roundhouse_core::control::correlation::contract::fresh_key;
use roundhouse_core::control::spend::contract::fresh_principal;
use roundhouse_core::ids::SessionId;
use roundhouse_server::Conversations;
use roundhouse_store_redis::RedisCorrelationMaps;
use roundhouse_store_redis::test_support::url_from_env;

/// One node: its own connection to the Redis every node shares.
///
/// A separate `connect` per node rather than a cloned handle, because a clone
/// shares a connection manager and would prove less than the claim: what is
/// under test is two processes, and two processes do not share a socket.
async fn node() -> Conversations {
    let maps = RedisCorrelationMaps::connect(url_from_env())
        .await
        .expect("Redis named by the env var must be reachable");
    Conversations::over(Arc::new(maps) as Arc<dyn CorrelationMaps>)
}

// `fresh_principal` and `fresh_key` used to be re-spelled here -- the
// dev-dependency graph already provides both
// (`roundhouse_core::control::spend::contract::fresh_principal`,
// `roundhouse_core::control::correlation::contract::fresh_key`), and
// `Conversations::commit`/`bind_call`/`bind_thread` take `principal` and
// `key` as independent parameters, so the mismatch between `fresh_key`'s own
// internally-minted principal and the `ada` held throughout a test below
// costs nothing (M14.1 review, F10; confirmed live over a real Redis before
// this fix, not merely reasoned about).

/// **The unlock condition, at the layer the deployment calls.**
///
/// The four M12.1 handoffs, each re-asked of the node that did *not* do the
/// binding: a fork is the other node's starting point, a call resolves, a
/// thread resolves, and a key nothing bound anywhere refuses on both.
///
/// One test because they are one property — the maps are shared — and
/// splitting them would let three pass while the fourth silently exercised a
/// different pair of handles.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn what_one_node_bound_is_what_the_other_node_answers() {
    let first = node().await;
    let second = node().await;
    let ada = fresh_principal("ada");
    let key = fresh_key("main");

    // (a) Never bound anywhere: both nodes refuse, and neither mints the
    // generation-zero id a first turn would have minted (M12.1 F9, widened).
    assert_eq!(first.resolve(&key).await.unwrap(), None);
    assert_eq!(second.resolve(&key).await.unwrap(), None);

    // (b) A fork on one node is the other's answer. `commit` is what a turn
    // makes, so this is the real write path and not a fixture's shortcut.
    let forked = first.commit(&ada, &key, 2).await;
    assert_eq!(forked.as_str(), format!("{key}#g2"));
    assert_eq!(
        second.resolve(&key).await.unwrap(),
        Some(forked.clone()),
        "a control call landing on the node that served none of this \
         conversation's turns must reach the fork the other node committed — \
         before this rung it refused, and before F9 it answered generation \
         zero"
    );
    assert_eq!(
        second.generation(&key).await,
        2,
        "and the second node's next *turn* starts its prefix search there \
         rather than walking up from zero"
    );

    // (c) A call bound on one node resolves on the other.
    let subagent = SessionId::new(format!("{key}#g2"));
    first
        .bind_call(&ada, "toolu_from_first", subagent.clone())
        .await;
    assert_eq!(
        second
            .session_of_call(&ada, "toolu_from_first")
            .await
            .unwrap(),
        Some(subagent),
        "an MCP call answering a tool this deployment emitted reaches the \
         emitting session whichever node the control call lands on"
    );

    // (d) A thread bound on one node resolves on the other.
    first
        .bind_thread(&ada, "thread-from-first", forked.clone())
        .await;
    assert_eq!(
        second
            .session_of_thread(&ada, "thread-from-first")
            .await
            .unwrap(),
        Some(forked),
    );

    // CONTROL: the second node is not answering everything affirmatively. A
    // `Conversations` that returned its last answer for any argument would
    // pass all four assertions above.
    assert_eq!(
        second
            .session_of_call(&ada, "toolu_never_emitted")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        second
            .session_of_thread(&ada, "thread-never-served")
            .await
            .unwrap(),
        None
    );
    assert_eq!(second.resolve(&fresh_key("other")).await.unwrap(), None);

    // CONTROL: `latest` is deliberately *not* shared (R12). The first node
    // committed a turn and the second still has no guess to offer, which is
    // the one piece of state this rung left node-local on purpose.
    assert_eq!(second.latest(&ada), None);
    assert!(first.latest(&ada).is_some());
}

/// The node that bound a name still reads the store, so what another node did
/// afterwards is what it answers.
///
/// **The memo's boundary, over a real Redis.** `Conversations` keeps a
/// node-local memo of the generation map so the common turn is a local lookup
/// (R-C2), and this is the half that keeps it honest: the memo is a *probe's*
/// starting point, and a reader is answered from the store. A `resolve` served
/// from the memo would hand an agent the pre-fork log with a 200 on it — F9's
/// defect one fork later — and no assertion in the maps' own contract could
/// see it, because the maps would have been right.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_node_that_committed_a_key_still_reads_what_another_node_did_next() {
    let first = node().await;
    let second = node().await;
    let ada = fresh_principal("ada");
    let key = fresh_key("main");

    let here = first.commit(&ada, &key, 0).await;
    assert_eq!(
        first.resolve(&key).await.unwrap(),
        Some(here.clone()),
        "sanity: the node that committed it answers it"
    );

    let elsewhere = second.commit(&ada, &key, 1).await;
    assert_ne!(here, elsewhere);
    assert_eq!(
        first.resolve(&key).await.unwrap(),
        Some(elsewhere),
        "the client compacted on the other node, and this node's reader must \
         follow it there rather than answering from what it last committed"
    );

    // CONTROL, and the reason the memo is legal at all: the *turn* path is
    // still allowed to start from the stale local value, because prefix
    // admission checks whatever it starts from against the log before
    // committing to it (M14.0). If this ever answers 1, the memo has stopped
    // existing and the read-through cost test is measuring nothing.
    assert_eq!(first.generation(&key).await, 0);
}

/// A tool-use id two nodes claimed is ambiguous on both, including on the node
/// that claimed it first.
///
/// M12's F14 with a network in the middle: the first node holds a binding it
/// wrote itself, and the second node's colliding claim is what makes it
/// unanswerable. A `Conversations` that read a local table before the store
/// would answer the first conversation's still-open `tools/call` confidently
/// about the second's session.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_call_id_two_nodes_claimed_is_unanswerable_on_both() {
    let first = node().await;
    let second = node().await;
    let ada = fresh_principal("ada");
    let key = fresh_key("main");

    first.bind_call(&ada, "call_0", SessionId::new(&key)).await;
    second
        .bind_call(&ada, "call_0", SessionId::new(format!("{key}#g1")))
        .await;

    assert_eq!(first.session_of_call(&ada, "call_0").await.unwrap(), None);
    assert_eq!(second.session_of_call(&ada, "call_0").await.unwrap(), None);

    // CONTROL: an id only one node ever claimed is still exactly answerable
    // on both, so the assertion above is about the collision and not about
    // call bindings having stopped working.
    let sole = SessionId::new(&key);
    first.bind_call(&ada, "call_1", sole.clone()).await;
    assert_eq!(
        second.session_of_call(&ada, "call_1").await.unwrap(),
        Some(sole)
    );
}

/// A thread whose latest turn was served by another node is in the session
/// *that* turn decided.
///
/// R-M9's rule (M12.1 review, F2) asked across nodes: a thread is in the
/// session its own latest turn decided, and on a real deployment the latest
/// turn is wherever the load balancer sent it. The first node bound this
/// thread and must follow it to the fork the second node's turn produced.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_thread_follows_the_node_that_served_its_latest_turn() {
    let first = node().await;
    let second = node().await;
    let ada = fresh_principal("ada");
    let key = fresh_key("main");

    let before = first.commit(&ada, &key, 0).await;
    first.bind_thread(&ada, "thread-parent", before).await;

    let after = second.commit(&ada, &key, 1).await;
    second
        .bind_thread(&ada, "thread-parent", after.clone())
        .await;

    assert_eq!(
        first
            .session_of_thread(&ada, "thread-parent")
            .await
            .unwrap(),
        Some(after),
        "the thread moved on the other node, and this node has to say so"
    );
}
