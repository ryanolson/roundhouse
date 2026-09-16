// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M14.2, R-S3: the composition root's namespace read, run rather than
//! re-derived — the same boot-test shape
//! [`fair_use_backend_boot.rs`](../tests/fair_use_backend_boot.rs) and
//! [`correlation_backend_boot.rs`](../tests/correlation_backend_boot.rs)
//! already give the other three families.
//!
//! `main.rs` resolves `ROUNDHOUSE_REDIS_NAMESPACE` with
//! [`resolve_namespace`] and hands the result to
//! [`shared_backend::open`], which is the composition root's whole decision
//! about which namespace every family's keys carry. This file runs both
//! calls rather than re-typing them — the same lesson M14.1's review (F1)
//! and M13's before it taught about `open` itself: a re-typed copy of the
//! wiring can drift from what ships and nothing goes red when it does.
//!
//! Gated like every Redis-touching suite in this tree: `#[ignore]`, opted
//! into with `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL`
//! fails loudly rather than skipping quietly.

use roundhouse_core::control::{CorrelationMaps, Principal};
use roundhouse_server::resolve_namespace;
use roundhouse_server::shared_backend::open;
use roundhouse_store_redis::test_support::url_from_env;
use roundhouse_store_redis::{KeyNamespace, RedisCorrelationMaps};

/// **The defect cell, proved rather than asserted.**
///
/// 1. Resolve a custom namespace exactly the way `main.rs` resolves
///    `ROUNDHOUSE_REDIS_NAMESPACE` — through [`resolve_namespace`], not a
///    hand-built [`KeyNamespace`].
/// 2. Open the backends through it, exactly as `main.rs` does.
/// 3. Commit one generation through the wired `Conversations` — the turn
///    path's own write, the same one `correlation_backend_boot.rs` drives.
/// 4. Read it back through two *independent* `RedisCorrelationMaps`
///    handles, one connected under the custom namespace and one under the
///    default: the first must see what was just committed, the second must
///    not. The second half is what makes this a proof that the custom
///    namespace was actually used, rather than a proof that *some*
///    namespace resolved to a working `Conversations`.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_composition_roots_namespace_read_reaches_the_wired_keys() {
    let url = url_from_env();
    let run = uuid::Uuid::new_v4();
    let key = format!("acme/ada/namespace-boot-{run}");

    let namespace = resolve_namespace(Some("boot-test-custom"))
        .expect("a non-empty literal namespace must resolve");

    let backends = open(Some(&url), &namespace)
        .await
        .expect("the test Redis named by ROUNDHOUSE_TEST_REDIS_URL must be reachable");
    let conversations = backends.conversations();

    let ada = Principal::new("acme".to_string(), "ada".to_string());
    conversations.commit(&ada, &key, 0).await;

    let under_custom = RedisCorrelationMaps::connect_namespaced(&url, namespace)
        .await
        .expect("the test Redis must be reachable")
        .generation(&key)
        .await
        .expect("the real Redis maps answer");
    assert_eq!(
        under_custom,
        Some(0),
        "the generation committed through the wired Conversations must be \
         readable through a fresh handle on the same custom namespace — if \
         this is None, main.rs's env read never reached shared_backend::open"
    );

    let under_default = RedisCorrelationMaps::connect_namespaced(&url, KeyNamespace::default())
        .await
        .expect("the test Redis must be reachable")
        .generation(&key)
        .await
        .expect("the real Redis maps answer");
    assert_eq!(
        under_default, None,
        "and it must be absent under the default namespace — if this is \
         Some, the custom namespace was resolved but never actually used, \
         which reads as success while every key still lands in \"rh\""
    );

    // CONTROL: `resolve_namespace`'s empty-string refusal, exercised as the
    // env var reader would see it — see `shared_backend`'s own unit test for
    // the pure-function half of this claim.
    assert!(resolve_namespace(Some("")).is_err());
}
