// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.1 thermo-nuclear review, finding F7 — refuted.
//!
//! **The claim.** `Conversations::commit` (`conversations.rs:347-359`) primes
//! the node-local memo and moves `latest` even when `set_generation` fails
//! against the store — deliberate, and documented as costing only "another
//! node's next probe a walk". The finding says the unstated cost is on the
//! *same* node: `Conversations::resolve` (`conversations.rs:385-391`) reads
//! only the store, never the memo, so until some other write refreshes the
//! store, a control call naming the conversation resolves to the superseded
//! generation — and `ControlPlaneReads::named_session` (`mcp_api.rs:163-183`)
//! turns that into a wrong-session `Ok`, a 200 served by the very node that
//! just moved the client off it.
//!
//! Gated as every Redis-touching suite in this tree: `#[ignore]`, opted into
//! with `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL` fails
//! loudly rather than skipping quietly.
//!
//! # How the partial failure is forced
//!
//! Exactly the recipe the finding's own `how_to_prove` names: `CONFIG SET
//! maxmemory 1` against the shared test Redis makes every subsequent write
//! command fail with `OOM command not allowed` while `GET` keeps answering
//! from what is already stored — sanity-checked live below before the claim
//! is even attempted, so a Redis version that behaved differently would fail
//! loudly rather than let the claim's own test pass for the wrong reason.
//! `commit`'s `SET` to the generation key is exactly such a write, so a
//! `commit` issued while `maxmemory` is pinned to `1` is a `commit` whose
//! store write is lost by construction — no reconnect, timeout, or actual OOM
//! condition needs to be reproduced; the store side effect is identical.
//!
//! # What it guards now the finding is closed
//!
//! The ruling is that **the node that committed must agree with itself**: the
//! memo entry records that its write was refused, `resolve` answers a named
//! conversation from such an entry ahead of the store, and the next commit
//! retries the write and clears the flag. So the assertions below run in that
//! order — the wrong-session `Ok(g0)` is gone, and the retry and the clearing
//! are asserted after it, because a fix that served g1 for ever would trade
//! one stale answer for another.

use std::process::Command;
use std::sync::Arc;

use roundhouse_core::control::{CorrelationMaps, MemorySpendLedger, Principal, SpendLedger};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_mcp::reads::ControlReads;
use roundhouse_server::conversations::bound_session;
use roundhouse_server::{ControlPlane, ControlPlaneReads, Conversations};
use roundhouse_store_redis::RedisCorrelationMaps;
use roundhouse_store_redis::test_support::url_from_env;

mod common;
use common::{control_plane, key, sha256_hex};

fn plane() -> ControlPlane {
    ControlPlane::configured(control_plane(
        serde_json::json!({
            "projects": [{ "id": "acme", "policy": { "min_quality": 0.1 } }],
            "users": [{ "id": "ada" }],
            "keys": [{
                "project": "acme", "user": "ada",
                "key_sha256": sha256_hex(&key("f7lost")),
            }],
        }),
        "F7 fixture",
    ))
}

/// The bare TCP address (`host:port`) `redis-cli` needs, pulled out of
/// `ROUNDHOUSE_TEST_REDIS_URL` (`redis://host:port/` or `redis://host:port`).
fn redis_cli_addr() -> (String, String) {
    let url = url_from_env();
    let without_scheme = url.strip_prefix("redis://").expect(
        "F7: ROUNDHOUSE_TEST_REDIS_URL must be a redis:// URL, matching every other \
                 gated suite in this tree",
    );
    let hostport = without_scheme.trim_end_matches('/');
    let (host, port) = hostport
        .rsplit_once(':')
        .expect("F7: ROUNDHOUSE_TEST_REDIS_URL must carry an explicit port");
    (host.to_string(), port.to_string())
}

/// Runs `redis-cli -h <host> -p <port> <args...>` against the same Redis the
/// suite's `RedisCorrelationMaps` connects to, and panics loudly on anything
/// but success -- a `CONFIG SET` that silently failed would make the "forced
/// OOM" window a no-op and the whole test meaningless.
fn redis_cli(args: &[&str]) -> String {
    let (host, port) = redis_cli_addr();
    let output = Command::new("redis-cli")
        .args(["-h", &host, "-p", &port])
        .args(args)
        .output()
        .expect("F7: redis-cli must be on PATH, as the house rules assume");
    assert!(
        output.status.success(),
        "F7: redis-cli {args:?} exited non-zero: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Pin `maxmemory` down to a single byte, guaranteeing every subsequent write
/// command is refused with `OOM` while reads keep answering -- the window the
/// finding's `how_to_prove` calls for.
fn force_oom() {
    let policy = redis_cli(&["config", "get", "maxmemory-policy"]);
    assert!(
        policy.ends_with("noeviction"),
        "F7 sanity: this Redis's maxmemory-policy must be noeviction (the \
         default) for a pinned maxmemory to reject writes with OOM rather \
         than silently evicting keys to make room -- got {policy:?}"
    );
    let reply = redis_cli(&["config", "set", "maxmemory", "1"]);
    assert_eq!(
        reply, "OK",
        "F7 sanity: CONFIG SET maxmemory 1 must succeed"
    );
}

/// Undo [`force_oom`]. Run in every test that calls it, success or panic path
/// alike is not attempted here (an assertion failure inside the `#[ignore]`d
/// claim test still leaves `maxmemory` pinned for the rest of the process) --
/// each test that pins it also restores it before its own final assertions,
/// which is enough since suites run in separate processes per binary but not
/// necessarily per test; see the calls below.
fn restore_maxmemory() {
    let reply = redis_cli(&["config", "set", "maxmemory", "0"]);
    assert_eq!(
        reply, "OK",
        "F7: restoring maxmemory to 0 (unlimited) must succeed"
    );
}

/// One node: its own connection to the Redis every node here shares. Named
/// for what it stands for rather than for its type, as `review_m14_1_f2.rs`
/// names the same helper — the local binding below is the node under test and
/// would otherwise shadow it.
async fn node() -> Conversations {
    let maps = RedisCorrelationMaps::connect(url_from_env())
        .await
        .expect("Redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable");
    Conversations::over(Arc::new(maps) as Arc<dyn CorrelationMaps>)
}

/// **Sanity, kept live.** Confirms the OOM-forcing recipe itself does what
/// the finding's `how_to_prove` says it does against *this* Redis, isolated
/// from `Conversations` entirely: a `SET` fails while a `GET` of a
/// previously-written key keeps answering. If this regresses (a Redis build
/// or config where `maxmemory 1` does something else), the claim test below
/// would fail for an unrelated reason, so this is what makes that
/// distinguishable.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn sanity_pinning_maxmemory_fails_writes_and_not_reads() {
    let probe_key = format!("f7-oom-probe-{}", uuid::Uuid::new_v4());
    let set_ok = redis_cli(&["set", &probe_key, "before"]);
    assert_eq!(set_ok, "OK", "sanity: an unconstrained SET must succeed");

    force_oom();

    let (host, port) = redis_cli_addr();
    let output = Command::new("redis-cli")
        .args(["-h", &host, "-p", &port, "set", &probe_key, "after"])
        .output()
        .expect("redis-cli must be on PATH");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("OOM"),
        "sanity: a SET while maxmemory is pinned to 1 byte must be refused \
         with OOM -- got {combined:?}"
    );

    let read_back = redis_cli(&["get", &probe_key]);
    assert_eq!(
        read_back, "before",
        "sanity: GET must still serve the value the earlier, unconstrained \
         SET wrote -- reads are not blocked by the OOM guard, only writes"
    );

    restore_maxmemory();
    let _ = redis_cli(&["del", &probe_key]);
}

/// **F7, the claim itself.** `principal` commits generation 0 normally, then
/// commits generation 1 while every write to Redis is refused with OOM.
/// `commit` warns and returns `g1` anyway (per its own doc). The question is
/// what a control call naming this conversation gets back afterward:
///
/// - `Conversations::generation` (memo-backed) and `Conversations::latest`
///   already say `g1` -- this node's turn path believes it moved.
/// - `Conversations::resolve` (store-only) still says `g0`, because the store
///   write for `g1` never landed.
/// - `ControlPlaneReads::named_session` is built on `resolve`, and both
///   sessions exist in the store (so the existence check inside
///   `named_session` cannot itself refuse) -- if the finding is right, it
///   returns `Ok(g0)`: a 200 naming the session the client just left, on the
///   very node whose own bookkeeping says it moved on.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_commit_whose_store_write_is_lost_is_served_by_the_node_that_made_it_and_retried() {
    let plane = Arc::new(plane());
    let store = Arc::new(MemoryStore::new());
    let principal = Principal::new("acme", "ada");
    let cache_key = format!("f7-{}", uuid::Uuid::new_v4());
    let qualified = plane.qualify(&principal, &cache_key);

    let g0 = bound_session(&qualified, 0);
    let g1 = bound_session(&qualified, 1);
    assert!(
        store.create_session(&g0, "policy").await.unwrap(),
        "sanity: g0 must be a fresh session"
    );
    assert!(
        store.create_session(&g1, "policy").await.unwrap(),
        "sanity: g1 must be a fresh session"
    );

    let conversations = Arc::new(node().await);

    // Commit generation 0 normally -- the store write lands, and this is what
    // resolve() must still be seeing when the test's claim window opens.
    let committed_g0 = conversations.commit(&principal, &qualified, 0).await;
    assert_eq!(
        committed_g0, g0,
        "sanity: commit(0) returns the g0 session id"
    );
    assert_eq!(
        conversations.resolve(&qualified).await.unwrap(),
        Some(g0.clone()),
        "sanity: resolve() must see the store's g0 before the OOM window"
    );

    // Force every write to this Redis to fail, then commit generation 1 --
    // the write inside commit() is refused, exactly the "commit whose store
    // write is lost" the finding names.
    force_oom();
    let committed_g1 = conversations.commit(&principal, &qualified, 1).await;
    restore_maxmemory();

    // commit() itself still answers g1 -- that half is documented and not in
    // dispute; it establishes that the memo and `latest` really did move.
    assert_eq!(
        committed_g1, g1,
        "sanity: commit(1) returns g1 even though its store write was refused \
         -- this is commit()'s documented behaviour, not the claim under test"
    );
    assert_eq!(
        conversations.generation(&qualified).await,
        1,
        "sanity: the memo moved to 1 -- generation() is memo-backed and was \
         primed by commit() regardless of the store write's outcome"
    );
    assert_eq!(
        conversations.latest(&principal),
        Some(g1.clone()),
        "sanity: latest() moved to g1 -- this node's turn path believes the \
         principal is now working in g1"
    );

    // The store itself never received g1, and that half is unchanged: what a
    // node which did not make this commit reads is still g0, the bounded walk
    // R-C2 already accepts. It is also exactly why the committing node may not
    // read its own answer from there.
    let elsewhere = node().await;
    assert_eq!(
        elsewhere.resolve(&qualified).await.unwrap(),
        Some(g0.clone()),
        "sanity: the lost write means the store itself was never told about \
         g1, so a node that did not make the commit still reads g0"
    );

    // THE CLAIM, closed: the node that committed agrees with itself. Its memo
    // entry is dirty -- a commit the store refused -- which is the one state
    // where the memo is fresher than the store, so `resolve` answers from it
    // rather than handing back the generation this node just moved off.
    assert_eq!(
        conversations.resolve(&qualified).await.unwrap(),
        Some(g1.clone()),
        "F7: this node's own reader must answer g1 -- its commit landed here \
         even though the store refused the write"
    );

    let spend: Arc<dyn SpendLedger> = Arc::new(MemorySpendLedger::new());
    let reads = ControlPlaneReads::new(
        Arc::clone(&plane),
        Arc::clone(&store),
        spend,
        Arc::clone(&conversations),
        Vec::new(),
    );

    // And the control surface built on `resolve` follows it: `named_session`
    // answers g1, the session this node's own commit moved the principal to,
    // rather than g0 with a 200 on it.
    let named = reads
        .named_session(&principal, &cache_key)
        .await
        .expect("named_session must answer, not refuse: both generations exist in the store");
    assert_eq!(
        named, g1,
        "F7: named_session() must not resolve this conversation to g0 -- this \
         node's own commit() already moved the principal to g1, so answering \
         g0 hands a control call a session the client has left"
    );

    // **Retried, and cleared.** The next commit on this key is the retry --
    // a turn on it writes the key again anyway -- so once the store is taking
    // writes, the generation this node had been serving from its own memo
    // lands, and the node that never saw the commit reads it too.
    assert_eq!(conversations.commit(&principal, &qualified, 1).await, g1);
    assert_eq!(
        elsewhere.resolve(&qualified).await.unwrap(),
        Some(g1.clone()),
        "F7: the write the outage lost is retried by this key's next commit, \
         not carried as a private disagreement for the process's life"
    );

    // Cleared rather than pinning this node to its memo for good: a *third*
    // node forks the key to g2, and this node's reader follows the store to
    // it. A dirty entry that outlived its write would answer g1 here, which is
    // the very staleness `resolve` refuses to serve.
    let other_node = node().await;
    let g2 = bound_session(&qualified, 2);
    assert_eq!(other_node.commit(&principal, &qualified, 2).await, g2);
    assert_eq!(
        conversations.resolve(&qualified).await.unwrap(),
        Some(g2),
        "F7: with the write landed the entry is clean again, so this node's \
         reader is back to reading the store another node has moved"
    );
}
