// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test-only access to the real Redis boundary.
//!
//! The environment contract and raw key helpers live here so integration tests
//! can prove the wire format without turning storage internals into production
//! API. This module only exists under the `test-support` feature.

use roundhouse_core::control::{Principal, ProjectId};
use roundhouse_core::ids::SessionId;

use crate::correlation::{RedisCorrelationMaps, call_key as correlation_call_key_impl};
use crate::fair_use::{
    bucket_fields, bucket_index, member_scope_key, project_scope_key, window_sum_fields,
};
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

/// The two raw hashes one draw touches, for the tests that assert on the
/// storage mechanism rather than only on the refusal derived from it.
///
/// Returned as a pair because that is the fact under test: `record_draw` takes
/// one [`Principal`] and moves *both* scopes' counters, so a helper that
/// handed back one key at a time would let a test assert half of it and look
/// green.
pub fn fair_use_scope_keys(principal: &Principal) -> (String, String) {
    (
        project_scope_key(&principal.project),
        member_scope_key(&principal.project, &principal.user),
    )
}

/// The two field names a draw at `at_ms` lands in.
///
/// The bucket index is derived from `at_ms` here exactly as the script derives
/// it server-side; a test that computed the index itself would be pinning its
/// own arithmetic rather than the ledger's.
pub fn fair_use_bucket_fields(at_ms: u64) -> (String, String) {
    bucket_fields(bucket_index(at_ms))
}

/// One window's four running-sum field names: tokens, micro-dollars, and the
/// oldest and newest bucket index the sum covers.
///
/// Exported because the running sums are the whole of M13.1: a test that only
/// read the per-bucket fields would pass against a ledger that maintained no
/// sum at all and re-scanned every bucket, which is exactly the read path this
/// rung replaced.
pub fn fair_use_window_sum_fields(
    window: roundhouse_core::control::FairUseWindow,
) -> (String, String, String, String) {
    window_sum_fields(window)
}

/// The `would_exceed` script's own text.
///
/// Exported for the one gated test that has to invoke it with a window group
/// past the ones `FairUseWindow::ALL` names — an argument list the production
/// `WouldExceedArgs` deliberately cannot build. Handing out the real script
/// rather than letting the test carry a copy is the whole point: a copy drifts
/// from what ships, and a test green against a stale copy proves nothing.
pub fn fair_use_would_exceed_source() -> &'static str {
    crate::fair_use::scripts::would_exceed_source()
}

/// One correlation handle whose staleness bounds are milliseconds rather than
/// hours.
///
/// **The seam R-C6 names, and the alternative it beat is the one that looks
/// cheaper**: shortening the shipped `CALL_BINDING_TTL_MS` under `cfg(test)`
/// would make the expiry test green against a deployment nobody runs, which is
/// worse than not testing expiry at all. A per-handle bound leaves the shipped
/// numbers shipped and still lets one test watch a binding actually leave.
pub fn correlation_with_binding_ttls(
    maps: RedisCorrelationMaps,
    call_ms: u64,
    thread_ms: u64,
) -> RedisCorrelationMaps {
    maps.with_binding_ttls(call_ms, thread_ms)
}

/// The raw key one call binding occupies, for the test that reads the
/// ambiguous marker itself rather than only the `None` it decodes to.
///
/// Its two siblings — the generation and thread keys — are pinned by
/// `every_key_carries_the_namespace_the_version_and_its_family` beside the
/// functions that build them, and exporting them here with no caller would be
/// an untested surface reading as a supported one.
pub fn correlation_call_key(principal: &Principal, call_id: &str) -> String {
    correlation_call_key_impl(principal, call_id)
}

/// What a call id two sessions of one principal have claimed holds instead of
/// a session.
///
/// Handed out rather than restated in the test, for the reason
/// `fair_use_would_exceed_source` is: a copy of a wire constant drifts from
/// what ships, and a test green against a stale copy proves nothing.
pub fn correlation_ambiguous_marker() -> &'static str {
    crate::correlation::AMBIGUOUS_MARKER
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
