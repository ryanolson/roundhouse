// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M12.1 review, finding F9 — refuted (valid); re-aimed at M14.1's wider
//! promise.
//!
//! **The claim.** `Conversations` is node-local but the `SessionStore` behind
//! it is shared. On a node that has never bound a client's cache key (a fresh
//! node, or one that simply never served this principal's turns),
//! `Conversations::resolve` defaulted the key's generation to zero, and if
//! that generation-zero session happened to exist in the *shared* store —
//! because some other node created and later forked it — `mcp_api.rs`'s
//! `named_session` saw `store.last_seq` succeed and returned it as though it
//! were the client's real conversation.
//!
//! **Ruling: valid, and fixed.** The assertion failed for exactly the stated
//! mechanism: `node_b` returned `Ok(g0)`, the stale, superseded session,
//! instead of either acceptable contract (refuse `NoSession`, or resolve to
//! `g1`, the client's actual current conversation). The fix, in
//! `Conversations::resolve`: a key no binding exists for answers `None` rather
//! than generation zero, and `named_session` refuses it with the same
//! `ForeignConversation` an unknown or another tenant's name gets.
//!
//! # What M14.1 changed here, and why the file stayed
//!
//! F9 offered two acceptable contracts and the fix took the *weaker* one,
//! because it was the only one a per-process table could reach: refuse. With
//! the three correlation maps in a store the nodes share (R12, R-C2), the
//! stronger one is available, and these tests now hold it — the topology below
//! is two `Conversations` over one set of maps, which is what a deployment
//! that names a `ROUNDHOUSE_REDIS_URL` is:
//!
//! - **bound elsewhere resolves.** `node_b` served none of this
//!   conversation's turns and still answers `g1`, the fork `node_a` made. It
//!   is no longer allowed to refuse, and it is still not allowed to answer
//!   `g0`;
//! - **never bound anywhere refuses.** The refusal did not go away; its scope
//!   widened from "this node has not seen it" to "nothing has", which is the
//!   promise it always should have made. That is the second topology below,
//!   over maps nothing has written;
//! - **`latest` stayed node-local and stayed a guess** (R12), so the control
//!   below — no correlator at all on a node that served no turn — still
//!   refuses `NoSession` exactly as it did.
//!
//! The Redis half of the same claim, over two real connections, is
//! `tests/correlation_any_node.rs`. This file stays on in-process maps
//! deliberately: what it exercises is the *resolver's* behaviour on a node
//! with no local history, and running that over a socket would make an
//! infrastructure outage look like a correlation regression.

use std::sync::Arc;

use roundhouse_core::control::{
    CorrelationMaps, MemoryCorrelationMaps, MemorySpendLedger, Principal,
};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_mcp::reads::ControlReads;
use roundhouse_mcp::surface::{Correlators, SurfaceError};
use roundhouse_server::test_support::{bind_conversation, fork_conversation};
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

/// Shared setup: `node_a` binds `ada`'s cache key at generation 0, then forks
/// it to generation 1, creating both sessions in one shared [`MemoryStore`].
/// Returns the fork ids and reads bound to a *second* `Conversations` over
/// that same store **and the same correlation maps** — the node the finding
/// says a Codex call can land on after another node forked the thread, on a
/// deployment that shares its maps (M14.1, R-C2).
///
/// The second node has its own `latest` and its own generation memo, which is
/// the whole of what stays node-local: everything it answers below it answers
/// from state another process wrote.
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
    let maps = Arc::new(MemoryCorrelationMaps::new());

    let conversations_a = Arc::new(Conversations::over(
        Arc::clone(&maps) as Arc<dyn CorrelationMaps>
    ));
    let g0 = bind_conversation(&conversations_a, &ada, &key).await;
    store
        .create_session(&g0, "claude")
        .await
        .expect("node_a creates ada's generation-0 session");
    let g1 = fork_conversation(&conversations_a, &ada, &key).await;
    store
        .create_session(&g1, "claude")
        .await
        .expect("node_a creates ada's generation-1 session (the fork)");
    assert_ne!(g0, g1, "sanity: the fork really is a different session");

    let conversations_b = Arc::new(Conversations::over(maps));
    let reads_b = reads_over(conversations_b, Arc::clone(&store));

    (ada, g0, g1, reads_b)
}

/// The same store, and maps nothing has ever written: a deployment where this
/// key was never bound *anywhere*.
fn never_bound_anywhere() -> (Principal, ControlPlaneReads<MemoryStore>) {
    let reads = reads_over(Arc::new(Conversations::new()), Arc::new(MemoryStore::new()));
    (Principal::new("acme", "ada"), reads)
}

/// F9, re-aimed (M14.1, R-C2): a node with no local history must not serve
/// another node's stale generation-zero session — and, where the maps are
/// shared, must serve the fork instead of refusing.
///
/// `node_b` used to resolve this call to `g0`, the stale superseded session.
/// The M12.1 fix made it refuse, which was the strongest answer a per-process
/// table could give. It resolves to `g1` now: the client's real current
/// conversation, decided by a turn this node never served.
#[tokio::test]
async fn a_node_that_served_no_turn_resolves_the_fork_another_node_made() {
    let (ada, g0, g1, reads_b) = two_node_topology().await;

    let resolved = reads_b
        .resolve_session(&ada, None, &in_thread("main"))
        .await
        .expect("the maps are shared, so this node knows the conversation");
    assert_eq!(
        resolved, g1,
        "the fork another node committed is what this key names now; \
         answering {g0:?} is F9 exactly, and refusing is the narrower promise \
         M12.1 could only half-make"
    );

    // The same, through the model-written argument rather than the correlator:
    // both arms reach one `Conversations::resolve`, and F9 was never only
    // about correlators.
    assert_eq!(
        reads_b
            .resolve_session(&ada, Some("main"), &uncorrelated())
            .await
            .expect("the named arm reaches the same shared map"),
        g1
    );
}

/// The refusal that stayed, with the scope it always should have had: a name
/// **no node** has ever bound.
///
/// This is what stops the assertion above from being "resolve anything that
/// exists in the store". The store here holds no such session and the maps
/// hold no such binding, and both arms refuse — the correlator by falling
/// through to a `latest` that is empty, the argument by
/// `ForeignConversation`, which is also what an unknown name and another
/// tenant's get.
#[tokio::test]
async fn a_name_no_node_ever_bound_still_refuses() {
    let (ada, reads) = never_bound_anywhere();

    assert!(
        matches!(
            reads.resolve_session(&ada, None, &in_thread("main")).await,
            Err(SurfaceError::NoSession)
        ),
        "a thread id naming a conversation nothing has bound falls through to \
         a `latest` that is empty"
    );

    let refused = reads
        .resolve_session(&ada, Some("main"), &uncorrelated())
        .await
        .expect_err("nothing has bound `main`, so nothing holds it");
    assert!(
        matches!(refused, SurfaceError::ForeignConversation(ref named) if named == "main"),
        "a name nothing has bound must refuse exactly as an unknown or a \
         foreign one does, never resolve to the generation-zero session a \
         first turn would have minted: {refused:?}"
    );
}

/// F9 control, kept live: the same second node, the same principal, but no
/// correlator at all. `latest` is node-local by contract (R12), so this must
/// still refuse `NoSession` — isolating every assertion above to the maps
/// rather than to `resolve_session` wholesale, and pinning the one piece of
/// state M14.1 deliberately did not share.
#[tokio::test]
async fn control_a_fresh_node_with_no_correlator_still_refuses_no_session() {
    let (ada, _g0, _g1, reads_b) = two_node_topology().await;

    let control = reads_b.resolve_session(&ada, None, &uncorrelated()).await;
    assert!(
        matches!(control, Err(SurfaceError::NoSession)),
        "a node that served this principal no turn has no most-recent \
         conversation to guess with, whatever the shared maps hold -- got \
         {control:?}"
    );
}
