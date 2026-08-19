// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test-only access to the real Redis boundary.
//!
//! The environment contract and raw key helpers live here so integration tests
//! can prove the wire format without turning storage internals into production
//! API. This module only exists under the `test-support` feature.

use roundhouse_core::control::ProjectId;
use roundhouse_core::ids::SessionId;

use crate::spend::holds_key as spend_holds_key_impl;
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

/// The raw holds key, for the one test that inspects the hash field a hold
/// occupies rather than only the balance it is derived into.
///
/// Its two siblings — the account and watermark keys — were exported here too
/// and never called. A test-support export with no caller is not a spare
/// affordance: it is an untested surface that reads as a supported one, and
/// the key format it pins is already pinned by
/// `the_project_and_member_keys_share_one_hash_tag` beside the real functions.
pub fn spend_holds_key(project: &ProjectId) -> String {
    spend_holds_key_impl(project)
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
