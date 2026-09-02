// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M12.1 review, finding F9 — refuted (valid).
//!
//! **The claim.** `Conversations` is node-local but the `SessionStore` behind
//! it is shared. On a node that has never bound a client's cache key (a fresh
//! node, or one that simply never served this principal's turns),
//! `Conversations::resolve` defaults the key's generation to zero
//! (`conversations.rs:280`, `generations.get(key).unwrap_or(0)`), and if that
//! generation-zero session happens to exist in the *shared* store — because
//! some other node created and later forked it — `mcp_api.rs`'s
//! `named_session` sees `store.last_seq` succeed and returns it as though it
//! were the client's real conversation. Pre-M12.1, the same unnamed-thread
//! path went through `Conversations::latest`, which is empty on a node that
//! served none of this principal's turns, and refused `NoSession` instead.
//!
//! **The test below** builds the exact two-node topology `how_to_prove`
//! describes: `node_a`'s `Conversations` binds `ada`'s cache key at
//! generation 0, then forks it to generation 1 (both sessions created in one
//! shared `MemoryStore`); `node_b` shares the same store but holds a
//! *fresh, empty* `Conversations` — never having bound or forked
//! anything for `ada`. It then drives the identical call `how_to_prove`
//! names — `resolve_session(&ada, None, in_thread("main"))` — through
//! `node_b`'s reads.
//!
//! **Ruling: valid, and fixed.** The assertion below failed for exactly the
//! stated mechanism: `node_b` returned `Ok(g0)`, the stale, superseded
//! session, instead of either acceptable contract (refuse `NoSession`, or
//! resolve to `g1`, the client's actual current conversation). The control —
//! `resolve_session(&ada, None, uncorrelated())` on `node_b` — passed
//! throughout and still refuses `NoSession`, which isolated the regression to
//! the threadId path exactly as the finding says, and ruled out "the whole
//! function is broken" as an alternative, weaker explanation.
//!
//! **The fix**, in `Conversations::resolve`: a key this node holds no binding
//! for answers `None` rather than generation zero, and `named_session` refuses
//! it with the same `ForeignConversation` an unknown or another tenant's name
//! gets — the three stay indistinguishable to the caller on purpose. The
//! `latest` fall-through then refuses `NoSession`, which is the loud answer the
//! `conversations` module doc promises. The third test below is the twin on the
//! *argument* arm: the same defect was reachable through a model-written
//! `conversation` too, and both arms now refuse.

use std::sync::Arc;

use roundhouse_core::control::{MemorySpendLedger, Principal};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_mcp::reads::ControlReads;
use roundhouse_mcp::surface::{Correlators, SurfaceError};
use roundhouse_server::{ControlPlane, ControlPlaneConfig, ControlPlaneReads, Conversations};

/// The one-tenant plane the finding's topology needs: `named_session` must
/// qualify `ada`'s cache key the same way on both nodes, which only a
/// `Configured` plane (rather than `ControlPlane::Open`) exercises — `Open`
/// leaves `qualify` a no-op and would not touch the namespacing this
/// finding is about.
fn ada_plane() -> ControlPlane {
    let json = serde_json::json!({
        "projects": [{ "id": "acme", "policy": { "min_quality": 0.1 } }],
        "users": [{ "id": "ada" }],
        "keys": [{
            "project": "acme", "user": "ada",
            "key_sha256": "a".repeat(64),
        }],
    })
    .to_string();
    ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "F9 fixture").expect("the fixture config validates"),
    )
}

fn reads_over(
    conversations: Arc<Conversations>,
    store: Arc<MemoryStore>,
) -> ControlPlaneReads<MemoryStore> {
    ControlPlaneReads::new(
        Arc::new(ada_plane()),
        store,
        Arc::new(MemorySpendLedger::new()),
        conversations,
        Vec::new(),
    )
}

fn uncorrelated() -> Correlators {
    Correlators::default()
}

fn in_thread(thread_id: &str) -> Correlators {
    Correlators {
        thread_id: Some(thread_id.to_string()),
        ..Correlators::default()
    }
}

/// Shared setup for both tests below: `node_a` binds `ada`'s cache key at
/// generation 0, then forks it to generation 1, creating both sessions in
/// one shared [`MemoryStore`]. Returns the store, the fork ids, and reads
/// bound to a *fresh, empty* `node_b` `Conversations` over that same store —
/// the node the finding says a Codex call can land on after another node
/// forked the thread.
async fn two_node_topology() -> (
    Principal,
    roundhouse_core::ids::SessionId,
    roundhouse_core::ids::SessionId,
    ControlPlaneReads<MemoryStore>,
) {
    let ada = Principal::new("acme", "ada");
    let key = ada_plane().qualify(&ada, "main");
    assert_eq!(
        key, "acme/ada/main",
        "sanity: the qualified cache key F9 assumes"
    );

    let store = Arc::new(MemoryStore::new());

    let conversations_a = Arc::new(Conversations::new());
    let g0 = conversations_a.bind(&ada, &key);
    store
        .create_session(&g0, "claude")
        .await
        .expect("node_a creates ada's generation-0 session");
    let g1 = conversations_a.fork(&ada, &key);
    store
        .create_session(&g1, "claude")
        .await
        .expect("node_a creates ada's generation-1 session (the fork)");
    assert_ne!(g0, g1, "sanity: the fork really is a different session");

    let conversations_b = Arc::new(Conversations::new());
    let reads_b = reads_over(conversations_b, Arc::clone(&store));

    (ada, g0, g1, reads_b)
}

/// F9 (M12.1 review, correlation-boundary): a node with no local generation
/// entry for a principal's cache key must not quietly resolve an unnamed
/// `threadId` to a stale generation-0 session just because that session
/// happens to exist in the shared store.
///
/// `node_b` used to resolve the call to `g0` — the stale, superseded session
/// — where the module doc's stated design ("never a wrong session served
/// quietly") requires either a loud `NoSession` refusal or resolution to
/// `g1`, the client's real current conversation. It now takes the first of
/// those: `node_b` bound nothing, so it says so.
#[tokio::test]
async fn a_fresh_node_does_not_serve_another_nodes_stale_generation_zero_session() {
    let (ada, g0, g1, reads_b) = two_node_topology().await;

    // The call under test: an unnamed thread id naming `ada`'s cache key,
    // served by the fresh node.
    let result = reads_b
        .resolve_session(&ada, None, &in_thread("main"))
        .await;
    match result {
        // Correct per the module doc's stated design ("never a wrong session
        // served quietly"): either refuse loudly...
        Err(SurfaceError::NoSession) => {}
        // ...or resolve to the client's *actual* current conversation.
        Ok(session) if session == g1 => {}
        // What F9 says happens today: the stale, superseded generation-0
        // session, served with no hint it is stale.
        Ok(session) if session == g0 => panic!(
            "F9: node_b resolved ada's unnamed thread id to the stale \
             generation-0 session {g0:?} even though node_a had already \
             forked it to {g1:?} in the store they share — the exact \
             loud-refusal-to-quiet-wrong-answer regression the \
             conversations.rs module doc says the design avoids"
        ),
        other => panic!("unexpected resolution: {other:?}"),
    }
}

/// F9 control, kept live: the same fresh node, the same principal, but no
/// correlator at all. `latest` is empty on `node_b` for `ada`, so this must
/// still refuse `NoSession` -- isolating the regression to the threadId path
/// (the ignored test above) rather than to `resolve_session` wholesale. If
/// this control ever goes red too, the ignored test above is no longer
/// isolating anything and needs re-diagnosis.
#[tokio::test]
async fn control_a_fresh_node_with_no_correlator_still_refuses_no_session() {
    let (ada, _g0, _g1, reads_b) = two_node_topology().await;

    let control = reads_b.resolve_session(&ada, None, &uncorrelated()).await;
    assert!(
        matches!(control, Err(SurfaceError::NoSession)),
        "control: a fresh node with no correlator at all must still refuse \
         NoSession rather than guess -- got {control:?}"
    );
}

/// The twin on the *argument* arm, and the reason F9 was never only about
/// correlators.
///
/// A model-written `conversation` reaches the same `Conversations::resolve`
/// the threadId correlator does, so on a node that bound nothing it was served
/// the same stale generation-0 session — and with a 200 on it rather than a
/// refusal, because the argument arm does not fall through. It refuses now,
/// and it refuses with `ForeignConversation`: "this node never served it",
/// "no such conversation" and "somebody else's" are one answer here on
/// purpose, since three distinguishable answers would make the argument an
/// enumeration oracle and none of them is anything the caller can act on
/// differently.
#[tokio::test]
async fn a_fresh_node_refuses_a_named_conversation_it_has_bound_nothing_for() {
    let (ada, _g0, _g1, reads_b) = two_node_topology().await;

    let refused = reads_b
        .resolve_session(&ada, Some("main"), &uncorrelated())
        .await
        .expect_err("node_b has bound nothing, so it holds no `main`");
    assert!(
        matches!(refused, SurfaceError::ForeignConversation(ref named) if named == "main"),
        "a name this node has bound nothing for must refuse exactly as an \
         unknown or a foreign one does, never resolve to the generation-zero \
         session another node has already forked away from: {refused:?}"
    );
}
