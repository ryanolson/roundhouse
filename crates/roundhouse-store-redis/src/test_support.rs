// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test-only access to the real Redis boundary.
//!
//! The environment contract and raw key helpers live here so integration tests
//! can prove the wire format without turning storage internals into production
//! API. This module only exists under the `test-support` feature.

use roundhouse_core::ids::SessionId;

use crate::{RedisSessionStore, lease_key as store_lease_key, log_key as store_log_key};

/// The one variable every Redis-gated integration test reads.
pub const URL_VAR: &str = "ROUNDHOUSE_TEST_REDIS_URL";

/// Panics rather than skips after `--include-ignored` opted into real Redis.
pub fn url_from_env() -> String {
    std::env::var(URL_VAR).unwrap_or_else(|_| {
        panic!(
            "--include-ignored asks for the real backend; \
             set {URL_VAR} to a reachable Redis"
        )
    })
}

/// The store under test, connected to the Redis the environment names.
pub async fn connect_from_env() -> RedisSessionStore {
    RedisSessionStore::connect(url_from_env())
        .await
        .expect("Redis named by the env var must be reachable")
}

/// The raw lease key used by adversarial tests.
pub fn lease_key(session_id: &SessionId) -> String {
    store_lease_key(session_id)
}

/// The raw log key used by wire-format tests.
pub fn log_key(session_id: &SessionId) -> String {
    store_log_key(session_id)
}

/// The conformance suite's expiry lever. Deleting the key is exactly what
/// Redis `PX` eventually does, so takeover behaves as if the TTL had elapsed.
#[async_trait::async_trait]
impl roundhouse_core::store::contract::LeaseControl for RedisSessionStore {
    async fn force_expire_lease(&self, session_id: &SessionId) {
        let _: i64 = redis::cmd("DEL")
            .arg(store_lease_key(session_id))
            .query_async(&mut self.conn.clone())
            .await
            .expect("the test Redis must accept a DEL");
    }
}
