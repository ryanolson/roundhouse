// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.1: the full [`CorrelationMaps`] contract against a real Redis, plus the
//! claims only a shared backend can make.
//!
//! The macro invocation is the milestone's headline, in the same idiom as
//! `spend_contract.rs` and `fair_use_contract.rs`: the *same* assertions that
//! judge `MemoryCorrelationMaps` judge these maps, whatever the list grows to
//! — the macro is the list, so a test added there is added here with no wiring
//! step. The key-layout assertions need no live Redis (key strings are pure
//! formatting) and so live as unit tests beside the functions that build them.
//!
//! Below the suite is what only a real, *shared* Redis can show:
//!
//! - **the unlock condition itself** — two handles over one Redis, which is
//!   the whole reason these maps exist. It is written as the four M12.1
//!   handoffs re-aimed from "this node" to "any node" (R-C6): a fork on one
//!   handle is the other's starting point, a call bound on one resolves on the
//!   other, a thread bound on one resolves on the other, and a key never bound
//!   anywhere refuses on both;
//! - **the collision through the script**, asserted on the stored marker and
//!   not only on the `None` it decodes to, so the test is about the mechanism
//!   rather than about its shadow;
//! - **the staleness expiry**, through the per-handle seam and never by
//!   changing a shipped TTL;
//! - **the read-through cost**, counted on Redis's own `commandstats` so "one
//!   read per key per node, then none" is a measurement rather than a claim.
//!
//! Gating is the same as every other file in this crate's `tests/`:
//! `#[ignore]`, opted into with `--include-ignored`, and a missing
//! `ROUNDHOUSE_TEST_REDIS_URL` fails loudly rather than skipping quietly.

mod common;

use common::raw_from_env;
use redis::AsyncCommands;
use roundhouse_core::control::correlation::contract::{AdvancePastTheBound, fresh_key};
use roundhouse_core::control::correlation::{
    CALL_BINDING_STALENESS_MS, THREAD_BINDING_STALENESS_MS,
};
use roundhouse_core::control::spend::contract::fresh_principal;
use roundhouse_core::control::{CorrelationMaps, MemoryCorrelationMaps};
use roundhouse_core::ids::SessionId;
use roundhouse_store_redis::RedisCorrelationMaps;
use roundhouse_store_redis::correlation::AMBIGUOUS_MARKER;
use roundhouse_store_redis::test_support::{
    correlation_call_key, correlation_thread_key, url_from_env,
};

roundhouse_core::correlation_maps_contract_suite!(
    ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
    connect_maps_from_env().await,
    aged = aged_maps_from_env().await,
);

async fn connect_maps_from_env() -> RedisCorrelationMaps {
    RedisCorrelationMaps::connect(url_from_env())
        .await
        .expect("Redis named by the env var must be reachable")
}

/// This backend's answer to the contract's "advance past the bound" hook
/// (M14.2 review, F4): a handle whose bindings expire in a moment, and a real
/// wait for that moment to pass.
///
/// Driven through the per-handle seam rather than by shortening
/// `CALL_BINDING_TTL_MS`: the shipped bound is six hours, and a test that
/// changed it would be asserting on a deployment nobody runs (R-C6). The wait
/// is real because Redis expiry is wall-clock driven and forcing it would
/// test the seam rather than the server — the one instantiation that waits,
/// waiting out an expiry it owns, while the shared assertion the hook is
/// handed to never sleeps. What the shipped bound actually is stays proven by
/// [`the_default_call_and_thread_ttls_reach_redis_as_the_core_staleness_bounds`],
/// which reads `PTTL` off a default handle rather than waiting for anything.
async fn aged_maps_from_env() -> (RedisCorrelationMaps, AdvancePastTheBound) {
    let maps = connect_maps_from_env().await.with_binding_ttls(80, 80);
    let advance: AdvancePastTheBound = Box::new(|| {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        })
    });
    (maps, advance)
}

fn session(name: &str) -> SessionId {
    SessionId::new(name)
}

/// **The unlock condition, and the whole of R-C6's re-aiming.**
///
/// Two handles are two nodes: separate connections, separate script caches,
/// nothing shared but the Redis. Every assertion below was true of *one*
/// `Conversations` before this rung and false across two processes, which is
/// exactly the handoff D1's inventory named.
///
/// The four are one test because they are one property — the maps are shared —
/// and splitting them would let three pass while the fourth silently exercised
/// a different pair of handles.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn what_one_node_bound_is_what_the_other_node_reads() {
    let first = connect_maps_from_env().await;
    let second = connect_maps_from_env().await;
    let ada = fresh_principal("ada");
    let key = fresh_key("main");

    // (a) A key never bound anywhere refuses on both handles. This is M12.1's
    // F9 with the scope it always should have had: not "this node has not seen
    // it" but "nothing has".
    assert_eq!(first.generation(&key).await.unwrap(), None);
    assert_eq!(second.generation(&key).await.unwrap(), None);

    // (b) A fork on one node is the other's starting point. The second handle
    // has served none of this conversation's turns, and it still knows the
    // client edited its history twice.
    first.set_generation(&key, 2).await.unwrap();
    assert_eq!(
        second.generation(&key).await.unwrap(),
        Some(2),
        "a client that reconnects to another node keeps its generation now; \
         before this rung the second node re-derived at zero and the search \
         paid a walk for it"
    );

    // (c) A call bound on one node resolves on the other.
    let subagent = session("acme/ada/sub");
    first
        .bind_call(&ada, "toolu_from_first", &subagent)
        .await
        .unwrap();
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
    let threaded = session("acme/ada/main#g2");
    first
        .bind_thread(&ada, "thread-from-first", &threaded)
        .await
        .unwrap();
    assert_eq!(
        second
            .session_of_thread(&ada, "thread-from-first")
            .await
            .unwrap(),
        Some(threaded)
    );

    // CONTROL: the second handle is not answering everything affirmatively.
    // Without this, a backend that returned the last value it saw for any
    // argument would pass all four assertions above.
    assert_eq!(
        second
            .session_of_call(&ada, "toolu_never_emitted")
            .await
            .unwrap(),
        None
    );
    assert_eq!(second.generation(&fresh_key("other")).await.unwrap(), None);
}

/// A collision resolved by two *nodes* leaves the ambiguous marker in Redis,
/// not one node's opinion.
///
/// The contract already asserts that a colliding id answers `None`. What this
/// adds is the mechanism: the script's third branch actually wrote the marker,
/// rather than the read happening to fail for some other reason — a decoder
/// that answered `None` for anything it could not parse would pass the
/// contract and would have quietly stopped enforcing anything.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_collision_across_two_nodes_marks_the_id_ambiguous_in_the_store() {
    let first = connect_maps_from_env().await;
    let second = connect_maps_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");

    first
        .bind_call(&ada, "call_0", &session("acme/ada/first"))
        .await
        .unwrap();
    second
        .bind_call(&ada, "call_0", &session("acme/ada/second"))
        .await
        .unwrap();

    let stored: Option<String> = raw.get(correlation_call_key(&ada, "call_0")).await.unwrap();
    assert_eq!(
        stored.as_deref(),
        Some(AMBIGUOUS_MARKER),
        "the second node's claim must leave the id marked, not replace the \
         first node's binding — a replaced binding answers the first \
         conversation's still-open tools/call with the second's session"
    );
    assert_eq!(first.session_of_call(&ada, "call_0").await.unwrap(), None);

    // And the marker survives a third claim, which is what "remembered rather
    // than forgotten" buys: a cleared id would let the next claimant start
    // answering confidently again.
    first
        .bind_call(&ada, "call_0", &session("acme/ada/first"))
        .await
        .unwrap();
    let stored: Option<String> = raw.get(correlation_call_key(&ada, "call_0")).await.unwrap();
    assert_eq!(stored.as_deref(), Some(AMBIGUOUS_MARKER));
}

// The per-backend expiry test that used to sit here is gone into the
// contract (M14.2 review, F4): "a binding older than the bound is absent" is
// a claim about both implementations, and one copy per backend is how the two
// came to disagree at exactly the bound they were supposed to share. It runs
// against this backend as
// `a_binding_past_its_staleness_bound_is_absent_and_the_next_write_is_a_first_write`,
// generated by the suite macro above from `aged_maps_from_env`, and it
// asserts strictly more than the copy it replaces — the write path at the
// bound as well as the read path, plus the same generation control.

/// The bound every production handle actually ships — `BindingTtls::default()`,
/// never `with_binding_ttls` — reaches Redis as `roundhouse-core`'s own
/// staleness constants.
///
/// Every other expiry test in this file (above) shortens the bound first, so
/// it proves the *mechanism* (a binding leaves when its `PEXPIRE` fires) but
/// never once exercises the number a real deployment runs (M14.2 review, F1)
/// — the unit test beside `BindingTtls::default()` in `correlation.rs`
/// pins that number in Rust; this is its live-Redis half, reading `PTTL`
/// straight off the key the default handle just wrote, the same proof
/// `fair_use_decay.rs`'s
/// `a_scope_is_armed_to_expire_one_bucket_past_the_widest_window` uses for
/// the same reason: waiting out six hours (or seven days) is not a test.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_default_call_and_thread_ttls_reach_redis_as_the_core_staleness_bounds() {
    let maps = connect_maps_from_env().await; // the shipped default, no with_binding_ttls
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");

    maps.bind_call(&ada, "toolu_default_ttl", &session("acme/ada/main"))
        .await
        .unwrap();
    maps.bind_thread(&ada, "thread-default-ttl", &session("acme/ada/main"))
        .await
        .unwrap();

    let call_pttl: i64 = redis::cmd("PTTL")
        .arg(correlation_call_key(&ada, "toolu_default_ttl"))
        .query_async(&mut raw)
        .await
        .unwrap();
    let thread_pttl: i64 = redis::cmd("PTTL")
        .arg(correlation_thread_key(&ada, "thread-default-ttl"))
        .query_async(&mut raw)
        .await
        .unwrap();

    // A window either side of the round trip's own cost, the same tolerance
    // and reasoning fair_use_decay.rs's PTTL check uses: PTTL starts ticking
    // on the server the instant the write lands, so what is pinned is the
    // policy, not the millisecond.
    assert!(
        call_pttl > CALL_BINDING_STALENESS_MS as i64 - 5_000
            && call_pttl <= CALL_BINDING_STALENESS_MS as i64,
        "F1: the shipped call binding TTL must be roundhouse-core's staleness \
         bound; PTTL was {call_pttl}, expected about {CALL_BINDING_STALENESS_MS}"
    );
    assert!(
        thread_pttl > THREAD_BINDING_STALENESS_MS as i64 - 5_000
            && thread_pttl <= THREAD_BINDING_STALENESS_MS as i64,
        "F1: the shipped thread binding TTL must be roundhouse-core's staleness \
         bound; PTTL was {thread_pttl}, expected about {THREAD_BINDING_STALENESS_MS}"
    );
}

/// A generation is read through the store once per key per node, and then not
/// again.
///
/// **The hot-path cost R-C2 resolves rather than accepts, counted rather than
/// asserted.** The write-through cache lives in `Conversations` — this rung
/// does not wire it — so what is pinned here is the half that makes the cache
/// legal: a `generation` read is exactly one Redis `GET`, so "one store read
/// per key per node, then none" is arithmetic over a number this test fixes
/// rather than a hope about how many round trips a read expands into.
///
/// Measured on Redis's own `commandstats` for the reason the fair-use read
/// path is: a test that counted the calls it made would be counting its own
/// arithmetic, and the thing at risk is what the *client library* sends. The
/// counter is server-wide, so the measurement is the *minimum* delta over
/// several attempts — anything else on this Redis can only inflate an
/// attempt, never deflate one, so the minimum is a true upper bound on what
/// one read costs. Deliberately not `CONFIG RESETSTAT`, which the fair-use
/// round-trip test uses: two binaries resetting one server's stats would each
/// zero the other's window, and a count of zero passes nothing.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn one_generation_read_is_one_round_trip() {
    let maps = connect_maps_from_env().await;
    let mut raw = raw_from_env().await;
    let key = fresh_key("main");
    maps.set_generation(&key, 1).await.unwrap();

    const ATTEMPTS: usize = 25;
    let mut reads = u64::MAX;
    for _ in 0..ATTEMPTS {
        let before = get_calls(&mut raw).await;
        assert_eq!(maps.generation(&key).await.unwrap(), Some(1));
        reads = reads.min(get_calls(&mut raw).await - before);
    }

    assert_eq!(
        reads, 1,
        "one read must be one GET: a read that fanned out would make the \
         node's first touch of a key cost more than the round trip R-C2 \
         budgets for it"
    );
}

/// How many `GET`s this Redis has served in its life.
///
/// Absent from `commandstats` until the first one, which is why a missing
/// counter reads as zero rather than as a failure.
async fn get_calls(raw: &mut redis::aio::MultiplexedConnection) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(raw)
        .await
        .expect("the test Redis must answer INFO commandstats");
    info.lines()
        .find_map(|line| line.strip_prefix("cmdstat_get:calls="))
        .and_then(|tail| tail.split(',').next())
        .and_then(|calls| calls.parse().ok())
        .unwrap_or(0)
}

/// The two backends answer the same question the same way at the one boundary
/// their representations part company: a session id that *looks like*
/// bookkeeping.
///
/// The memory maps hold a `SessionId` in an enum arm, so no session id can be
/// confused with the ambiguous state. The Redis maps hold a string, so the
/// question is real there and is answered by tagging the bound case. Asserted
/// as a differential rather than only inside the Redis unit tests, because
/// what matters is that the two agree — a backend that disagreed here would
/// resolve a conversation on one deployment and refuse it on another, with
/// nothing in the contract to catch it.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_conversation_named_like_the_ambiguous_marker_resolves_on_both_backends() {
    let redis = connect_maps_from_env().await;
    let memory = MemoryCorrelationMaps::new();
    let ada = fresh_principal("ada");
    let impostor = session(AMBIGUOUS_MARKER);

    for maps in [
        &redis as &dyn CorrelationMaps,
        &memory as &dyn CorrelationMaps,
    ] {
        maps.bind_call(&ada, "toolu_impostor", &impostor)
            .await
            .unwrap();
        assert_eq!(
            maps.session_of_call(&ada, "toolu_impostor").await.unwrap(),
            Some(impostor.clone()),
            "a client that names its conversation after this store's own \
             marker must not thereby look like a collision"
        );
    }
}
