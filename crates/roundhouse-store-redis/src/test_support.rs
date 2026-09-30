// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test-only access to the real Redis boundary.
//!
//! The environment contract and raw key helpers live here so integration tests
//! can prove the wire format without turning storage internals into production
//! API. This module only exists under the `test-support` feature.

use roundhouse_core::control::{Principal, ProjectId};
use roundhouse_core::ids::SessionId;
use roundhouse_core::routing::learn::{EpochId, LevelKey};

use crate::KeyNamespace;
use crate::correlation::call_key as correlation_call_key_impl;
use crate::correlation::thread_key as correlation_thread_key_impl;
use crate::fair_use::{
    bucket_fields, bucket_index, member_scope_key, project_scope_key, window_sum_fields,
};
use crate::spend::{SpendLeaf, spend_key};
use crate::{RedisSessionStore, lease_key as store_lease_key, log_key as store_log_key};

/// The namespace every helper in this module builds a key under.
///
/// Every gated test in this crate connects through [`connect_from_env`] or
/// the family-specific analogues, none of which have opted into a namespace
/// of their own — so the raw key a test computes to inspect a handle's state
/// must agree with the default the handle itself connected under.
fn default_namespace() -> KeyNamespace {
    KeyNamespace::default()
}

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

/// A namespace no other test, and no earlier run, has used.
///
/// For the session contract suite and the learning-index tests. The learning
/// index is one set of keys per namespace, so under the shared default a
/// permanent-index pass would grow with every run against the same Redis,
/// and a test that sabotages an index key's type would break every test
/// running beside it.
pub fn fresh_namespace() -> KeyNamespace {
    KeyNamespace::new(format!("rhtest-{}", uuid::Uuid::new_v4().simple()))
        .expect("a hex suffix contains no forbidden character")
}

/// The store under test, connected under `namespace`.
pub async fn connect_in(namespace: KeyNamespace) -> RedisSessionStore {
    RedisSessionStore::connect_namespaced(url_from_env(), namespace)
        .await
        .expect("Redis named by the env var must be reachable")
}

/// The three raw learning-index keys under `namespace`: the permanent mark
/// hash, the permanent membership set, and the pending set — for the tests
/// that sabotage their types or assert no write reached them.
pub fn learning_index_keys(namespace: &KeyNamespace) -> [String; 3] {
    let keys = crate::scripts::learning::IndexKeys::new(namespace);
    [keys.marks, keys.marked, keys.pending]
}

/// The raw log key under `namespace`, for the test that seeds a log near the
/// top of the exact sequence range.
pub fn log_key_in(namespace: &KeyNamespace, session_id: &SessionId) -> String {
    store_log_key(namespace, session_id)
}

/// The raw lease key used by adversarial tests.
pub fn lease_key(session_id: &SessionId) -> String {
    store_lease_key(&default_namespace(), session_id)
}

/// The raw log key used by wire-format tests.
pub fn log_key(session_id: &SessionId) -> String {
    store_log_key(&default_namespace(), session_id)
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
    spend_key(
        &default_namespace(),
        crate::spend::SpendPurpose::Serving,
        project,
        SpendLeaf::Holds,
    )
}

/// The two raw hashes one draw touches, for the tests that assert on the
/// storage mechanism rather than only on the refusal derived from it.
///
/// Returned as a pair because that is the fact under test: `record_draw` takes
/// one [`Principal`] and moves *both* scopes' counters, so a helper that
/// handed back one key at a time would let a test assert half of it and look
/// green.
pub fn fair_use_scope_keys(principal: &Principal) -> (String, String) {
    let namespace = default_namespace();
    (
        project_scope_key(&namespace, &principal.project),
        member_scope_key(&namespace, &principal.project, &principal.user),
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

/// The raw key one call binding occupies, for the test that reads the
/// ambiguous marker itself rather than only the `None` it decodes to.
///
/// Its generation sibling is pinned by
/// `every_key_carries_the_namespace_the_version_and_its_family` beside the
/// functions that build them, and exporting it here with no caller would be
/// an untested surface reading as a supported one. The thread key gained a
/// caller of its own (M14.2 review, F1) — see [`correlation_thread_key`].
pub fn correlation_call_key(principal: &Principal, call_id: &str) -> String {
    correlation_call_key_impl(&default_namespace(), principal, call_id)
}

/// The raw key one thread binding occupies, for the gated test that reads
/// the shipped `PTTL` a production handle's default arms — the half of R-S1
/// no unit test can reach, since a unit test never touches a real Redis
/// clock (M14.2 review, F1).
pub fn correlation_thread_key(principal: &Principal, thread_id: &str) -> String {
    correlation_thread_key_impl(&default_namespace(), principal, thread_id)
}

/// The one key the directory family occupies, under a namespace the caller
/// names.
///
/// **Takes its namespace explicitly, unlike every other helper here**, and
/// that is the family's shape rather than an inconsistency: this family has a
/// single key for the whole deployment (R-D6), so a gated test isolates itself
/// by connecting under a fresh [`KeyNamespace`] instead of by minting a fresh
/// principal or session id inside a shared one. A helper that assumed the
/// default namespace would compute a key none of those tests' handles ever
/// writes.
pub fn directory_records_key(namespace: &KeyNamespace) -> String {
    crate::directory::records_key(namespace)
}

/// The conformance suite's expiry lever. Deleting the key is exactly what
/// Redis `PX` eventually does, so takeover behaves as if the TTL had elapsed.
#[async_trait::async_trait]
impl roundhouse_core::store::contract::LeaseControl for RedisSessionStore {
    async fn force_expire_lease(&self, session_id: &SessionId) {
        let _: i64 = redis::cmd("DEL")
            .arg(store_lease_key(&self.namespace, session_id))
            .query_async(&mut self.conn.clone())
            .await
            .expect("the test Redis must accept a DEL");
    }
}

/// The conformance suite's unreadable-mark lever: a value in the marks hash
/// that `parse_mark` rejects, as a foreign writer would leave.
#[async_trait::async_trait]
impl roundhouse_core::store::contract::learning::LearningMarkControl for RedisSessionStore {
    async fn make_mark_unreadable(&self, session_id: &SessionId) {
        let _: i64 = redis::cmd("HSET")
            .arg(crate::scripts::learning::IndexKeys::new(&self.namespace).marks)
            .arg(session_id.as_str())
            .arg("x")
            .query_async(&mut self.conn.clone())
            .await
            .expect("the test Redis must accept an HSET");
    }

    async fn orphan_pending_mark(&self, session_id: &SessionId) {
        let _: i64 = redis::cmd("HDEL")
            .arg(crate::scripts::learning::IndexKeys::new(&self.namespace).marks)
            .arg(session_id.as_str())
            .query_async(&mut self.conn.clone())
            .await
            .expect("the test Redis must accept an HDEL");
    }
}

/// The learner store under test, connected under `namespace`.
pub async fn connect_learner_in(namespace: KeyNamespace) -> crate::RedisLearnerStore {
    crate::RedisLearnerStore::connect_namespaced(url_from_env(), namespace)
        .await
        .expect("Redis named by the env var must be reachable")
}

/// The learner family's watermark hash of `project`.
pub fn learn_watermark_key(namespace: &KeyNamespace, project: &ProjectId) -> String {
    crate::learn::watermark_key(namespace, project)
}

/// One quality hash of `project` under `epoch`.
pub fn learn_quality_key(
    namespace: &KeyNamespace,
    project: &ProjectId,
    epoch: EpochId,
    key: LevelKey,
) -> String {
    crate::learn::quality_key(namespace, project, epoch, key)
}

/// The operations hash of `project` under `epoch`.
pub fn learn_ops_key(namespace: &KeyNamespace, project: &ProjectId, epoch: EpochId) -> String {
    crate::learn::ops_key(namespace, project, epoch)
}

/// The `seen` set of `session` in `project` under `epoch`.
pub fn learn_seen_key(
    namespace: &KeyNamespace,
    project: &ProjectId,
    epoch: EpochId,
    session: &SessionId,
) -> String {
    crate::learn::seen_key(namespace, project, epoch, session)
}

/// Every key under `pattern`, by a full `SCAN`.
async fn scan(conn: &mut redis::aio::ConnectionManager, pattern: &str) -> Vec<String> {
    let mut cursor = 0u64;
    let mut keys = Vec::new();
    loop {
        let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(1000)
            .query_async(conn)
            .await
            .expect("the test Redis must accept a SCAN");
        keys.extend(batch);
        if next == 0 {
            break;
        }
        cursor = next;
    }
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// `text` as a `SCAN MATCH` pattern that matches only itself: a namespace may
/// hold `*`, `?`, `[` or `]`, and a project id anything.
fn glob_literal(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '*' | '?' | '[' | ']' | '\\') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern
}

/// The `SCAN MATCH` pattern for every key of `project`, and no other
/// project's: its prefix, through the `}:` after the tag, then `*`.
fn learn_project_pattern(store: &crate::RedisLearnerStore, project: &ProjectId) -> String {
    format!(
        "{}*",
        glob_literal(&crate::learn::project_prefix(store.namespace(), project))
    )
}

/// The learner store's snapshot lever: `SCAN` over the project's key prefix,
/// then `DUMP` each key; a restore deletes every key under the prefix and
/// `RESTORE`s the snapshot's. The snapshot holds the watermark hash and the
/// `seen` sets with the counters, so a staged store loss takes the `seen`
/// members with it.
///
/// **An absent project is the empty key set**, because Redis holds no empty
/// key, and restoring it deletes every key under the prefix. A snapshot is
/// compared by its `DUMP` bytes, which change with any write to a key; a
/// restored key may dump differently from the original, so compare only
/// snapshots taken without a restore between them.
///
/// **Restore deletes only under the project's prefix.** The prefix ends at
/// the `}:` after the project's hash tag, so `proj_ab` does not match
/// `proj_ab12`, and no other family's key or other project's key is touched.
/// A project id that itself contains `}:` could still match another
/// project's keys; every test mints `proj_<hex>` ids.
///
/// **`SCAN MATCH` walks the whole keyspace**: the pattern filters each reply,
/// it does not narrow the walk, so a snapshot costs one pass over every key
/// the Redis holds. That is fine for a test Redis and is why this lives in
/// test support rather than in the store.
#[async_trait::async_trait]
impl roundhouse_core::learn_store::contract::LearnerStoreControl for crate::RedisLearnerStore {
    type Snapshot = std::collections::BTreeMap<String, Vec<u8>>;

    async fn snapshot(&self, project: &ProjectId) -> Self::Snapshot {
        let mut conn = self.connection();
        let pattern = learn_project_pattern(self, project);
        let mut snapshot = std::collections::BTreeMap::new();
        for key in scan(&mut conn, &pattern).await {
            let dump: Option<Vec<u8>> = redis::cmd("DUMP")
                .arg(&key)
                .query_async(&mut conn)
                .await
                .expect("the test Redis must accept a DUMP");
            if let Some(dump) = dump {
                snapshot.insert(key, dump);
            }
        }
        snapshot
    }

    async fn restore(&self, project: &ProjectId, snapshot: Self::Snapshot) {
        let mut conn = self.connection();
        let pattern = learn_project_pattern(self, project);
        let live = scan(&mut conn, &pattern).await;
        if !live.is_empty() {
            let _: i64 = redis::cmd("DEL")
                .arg(&live)
                .query_async(&mut conn)
                .await
                .expect("the test Redis must accept a DEL");
        }
        for (key, dump) in snapshot {
            let _: () = redis::cmd("RESTORE")
                .arg(&key)
                .arg(0)
                .arg(dump)
                .query_async(&mut conn)
                .await
                .expect("the test Redis must accept a RESTORE of its own DUMP");
        }
    }
}

/// Every key under `namespace`, for the test that proves the learner store
/// writes no key outside the ones its key functions name.
pub async fn keys_in(namespace: &KeyNamespace) -> Vec<String> {
    let mut conn = crate::connect_manager(&url_from_env())
        .await
        .expect("Redis named by the env var must be reachable");
    scan(
        &mut conn,
        &format!("{}:*", glob_literal(&namespace.to_string())),
    )
    .await
}
