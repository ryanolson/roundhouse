// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Redis-backed [`SessionStore`].
//!
//! One session maps to three keys, all sharing a Redis Cluster hash tag so the
//! multi-key lease and append operations stay single-slot even though the
//! first deployment target is a single node:
//!
//! | Key | Type | Holds |
//! |---|---|---|
//! | `rh:{<session_id>}:meta` | string | JSON `{model_policy, created_at_ms}`, written `SET NX` |
//! | `rh:{<session_id>}:lease` | string | holder's `node_id`, expiry enforced by Redis `PX` |
//! | `rh:{<session_id>}:log` | stream | one entry per event, explicit id `<seq>-0` |
//!
//! The log's wire format is the load-bearing decision. Entries are added with
//! *explicit* stream ids `<seq>-0`, so the entry id and the event's `seq` are
//! the same number: `read_events(after_seq)` is one `XRANGE` with an exclusive
//! start and no client-side filtering, `last_seq` is one `XREVRANGE`, and
//! contiguity is enforced at the single place ids are assigned. Each entry
//! carries two fields — `at_ms`, and `kind` as the serde_json encoding of
//! [`SessionEventKind`] — and [`SessionEvent`] is recombined on read from the
//! entry id, the key, and those fields. Serialization stays entirely in Rust;
//! the append script never parses or splices JSON.
//!
//! An entry that violates the format — a missing field, an id some foreign
//! writer auto-generated — fails the read loudly as [`StoreError::Backend`].
//! Skipping it would silently drop events from a replay, and a replay that
//! quietly disagrees with what was appended is the one failure mode an
//! event-sourced store must never have.
//!
//! Durability is a deployment fact, not a code path: the log is exactly as
//! durable as the Redis it lives in (AOF `appendfsync`, replication). This
//! crate does not try to out-engineer the operator's persistence config.
//!
//! The write path lives in [`scripts`]: the lease is a `SET PX` key on the
//! Redis clock, and lease-check plus append is one atomic Lua script, which
//! is what makes the fencing the trait promises actually hold under
//! concurrent writers. Requires Redis ≥ 6.2 (exclusive `XRANGE` starts,
//! effects-replicated scripts).
//!
//! The store passes the same contract suite as `MemoryStore` — instantiated
//! by the same `store_contract_suite!` macro — and the binary selects it when
//! `ROUNDHOUSE_REDIS_URL` is set (see `roundhouse-server`'s `main.rs`).

mod scripts;

use redis::aio::ConnectionManager;
use redis::streams::StreamRangeReply;
use serde::{Deserialize, Serialize};

use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::SessionId;
use roundhouse_core::now_ms;
use roundhouse_core::store::{Lease, SessionStore, StoreError};

/// Where and under what namespace sessions live.
#[derive(Debug, Clone)]
pub struct RedisStoreConfig {
    url: String,
    key_prefix: String,
}

impl RedisStoreConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            key_prefix: "rh".into(),
        }
    }

    /// Namespace the keys, e.g. to let two deployments share an instance.
    pub fn with_key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = prefix.into();
        self
    }

    // The key layout is public, documented API rather than an internal detail:
    // an operator debugging a deployment sees these keys, and the tests write
    // through them to prove the wire format from outside the crate. The braces
    // are a Redis Cluster hash tag — every key of one session hashes to one
    // slot, which is what keeps the lease-fenced append single-slot scriptable.

    pub fn meta_key(&self, session_id: &SessionId) -> String {
        format!("{}:{{{}}}:meta", self.key_prefix, session_id)
    }

    pub fn lease_key(&self, session_id: &SessionId) -> String {
        format!("{}:{{{}}}:lease", self.key_prefix, session_id)
    }

    pub fn log_key(&self, session_id: &SessionId) -> String {
        format!("{}:{{{}}}:log", self.key_prefix, session_id)
    }
}

/// The value under `…:meta`.
///
/// `created_at_ms` is the client's clock and is informational only — nothing
/// orders on it. Lease expiry and event timestamps, where a single clock
/// authority *does* matter, use the Redis clock (M3).
#[derive(Serialize, Deserialize)]
struct SessionMeta {
    model_policy: String,
    created_at_ms: u64,
}

/// Redis implementation of [`SessionStore`].
///
/// Cheap to clone: clones share one auto-reconnecting multiplexed connection.
#[derive(Clone)]
pub struct RedisSessionStore {
    config: RedisStoreConfig,
    conn: ConnectionManager,
    scripts: std::sync::Arc<scripts::Scripts>,
}

impl RedisSessionStore {
    /// Connect and fail fast: a store that cannot reach its Redis at startup
    /// should stop the process there, not on the first session.
    pub async fn connect(config: RedisStoreConfig) -> Result<Self, StoreError> {
        let client = redis::Client::open(config.url.as_str()).map_err(backend)?;
        let conn = ConnectionManager::new(client).await.map_err(backend)?;
        Ok(Self {
            config,
            conn,
            scripts: std::sync::Arc::new(scripts::Scripts::new()),
        })
    }

    /// `SessionNotFound` unless the session's meta key exists.
    ///
    /// `exists` rides in the same pipeline as the read it guards, so the check
    /// costs no extra round trip.
    fn require_session(exists: bool, session_id: &SessionId) -> Result<(), StoreError> {
        if exists {
            Ok(())
        } else {
            Err(StoreError::SessionNotFound(session_id.clone()))
        }
    }
}

fn backend(error: redis::RedisError) -> StoreError {
    StoreError::Backend(anyhow::Error::new(error))
}

/// The `seq` a stream entry id encodes, i.e. `N` from `N-0`.
///
/// The shape check alone cannot prove the entry is ours: a foreign writer
/// using auto-generated ids also produces `<ms>-0`, which parses as a huge but
/// plausible `seq`. What catches that is the cross-check at each call site —
/// a read batch must be contiguous from its cursor and the newest id must
/// equal the stream's length — because seqs run 1..=len with no gaps. The
/// suffix check here only rejects what is unambiguously malformed.
fn seq_of(entry_id: &str, log_key: &str) -> Result<u64, StoreError> {
    let parsed = entry_id
        .split_once('-')
        .filter(|(_, tail)| *tail == "0")
        .and_then(|(seq, _)| seq.parse::<u64>().ok());
    parsed.ok_or_else(|| {
        StoreError::Backend(anyhow::anyhow!(
            "stream entry `{entry_id}` in `{log_key}` is not `<seq>-0` shaped; \
             the log has a writer other than this store"
        ))
    })
}

/// Rebuild a [`SessionEvent`] from one stream entry.
fn event_of(
    entry: &redis::streams::StreamId,
    session_id: &SessionId,
    log_key: &str,
) -> Result<SessionEvent, StoreError> {
    let seq = seq_of(&entry.id, log_key)?;
    let corrupt = |what: &str| {
        StoreError::Backend(anyhow::anyhow!(
            "stream entry `{}` in `{log_key}` {what}; refusing to replay a log \
             that would come back different from what was appended",
            entry.id
        ))
    };

    let at_ms: u64 = entry
        .get::<String>("at_ms")
        .and_then(|raw| raw.parse().ok())
        .ok_or_else(|| corrupt("has no integer `at_ms` field"))?;
    let kind: SessionEventKind = entry
        .get::<String>("kind")
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| corrupt(&format!("has an undecodable `kind` field: {error}")))?
        .ok_or_else(|| corrupt("has no `kind` field"))?;

    Ok(SessionEvent {
        seq,
        session_id: session_id.clone(),
        at_ms,
        kind,
    })
}

#[async_trait::async_trait]
impl SessionStore for RedisSessionStore {
    async fn create_session(
        &self,
        session_id: &SessionId,
        model_policy: &str,
    ) -> Result<bool, StoreError> {
        let meta = serde_json::to_string(&SessionMeta {
            model_policy: model_policy.to_string(),
            created_at_ms: now_ms(),
        })
        .expect("a string and an integer always serialize");

        // NX both answers "did it exist" and refuses to overwrite the policy
        // an earlier creation recorded.
        let created: Option<String> = redis::cmd("SET")
            .arg(self.config.meta_key(session_id))
            .arg(meta)
            .arg("NX")
            .query_async(&mut self.conn.clone())
            .await
            .map_err(backend)?;
        Ok(created.is_some())
    }

    async fn acquire_lease(
        &self,
        session_id: &SessionId,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<Option<Lease>, StoreError> {
        let outcome = self
            .scripts
            .acquire(
                &mut self.conn.clone(),
                &self.config.meta_key(session_id),
                &self.config.lease_key(session_id),
                node_id,
                ttl_ms,
            )
            .await?;
        self.lease_from(outcome, session_id, node_id, ttl_ms)
    }

    async fn renew_lease(&self, lease: &Lease, ttl_ms: u64) -> Result<Option<Lease>, StoreError> {
        let outcome = self
            .scripts
            .renew(
                &mut self.conn.clone(),
                &self.config.meta_key(&lease.session_id),
                &self.config.lease_key(&lease.session_id),
                &lease.node_id,
                ttl_ms,
            )
            .await?;
        self.lease_from(outcome, &lease.session_id, &lease.node_id, ttl_ms)
    }

    async fn release_lease(&self, lease: &Lease) -> Result<(), StoreError> {
        self.scripts
            .release(
                &mut self.conn.clone(),
                &self.config.lease_key(&lease.session_id),
                &lease.node_id,
            )
            .await
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let payloads: Vec<String> = kinds
            .iter()
            .map(|kind| {
                serde_json::to_string(kind).expect("event kinds are plain data and serialize")
            })
            .collect();

        let outcome = self
            .scripts
            .append(
                &mut self.conn.clone(),
                &self.config.meta_key(&lease.session_id),
                &self.config.lease_key(&lease.session_id),
                &self.config.log_key(&lease.session_id),
                &lease.node_id,
                &payloads,
            )
            .await?;

        match outcome {
            scripts::AppendOutcome::Appended { at_ms, last_seq } => {
                // The script numbered this batch (last_seq - n, last_seq];
                // rebuild the events from what was sent rather than re-reading.
                let first_seq = last_seq - kinds.len() as u64 + 1;
                Ok(kinds
                    .into_iter()
                    .enumerate()
                    .map(|(offset, kind)| SessionEvent {
                        seq: first_seq + offset as u64,
                        session_id: lease.session_id.clone(),
                        at_ms,
                        kind,
                    })
                    .collect())
            }
            scripts::AppendOutcome::Fenced => Err(StoreError::LeaseLost {
                session_id: lease.session_id.clone(),
                node_id: lease.node_id.clone(),
            }),
            scripts::AppendOutcome::NoSession => {
                Err(StoreError::SessionNotFound(lease.session_id.clone()))
            }
        }
    }

    async fn read_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        if limit == 0 {
            // XRANGE treats COUNT 0 as unlimited; the trait means "none".
            return Ok(Vec::new());
        }
        let log_key = self.config.log_key(session_id);

        // `(` is an exclusive start. Ids are always `<seq>-0`, so excluding
        // exactly `after_seq-0` is precisely "seq > after_seq" — with no
        // arithmetic on `after_seq` that could overflow at u64::MAX.
        let (exists, range): (bool, StreamRangeReply) = redis::pipe()
            .exists(self.config.meta_key(session_id))
            .xrange_count(&log_key, format!("({after_seq}-0"), "+", limit)
            .query_async(&mut self.conn.clone())
            .await
            .map_err(backend)?;
        Self::require_session(exists, session_id)?;

        let events: Vec<SessionEvent> = range
            .ids
            .iter()
            .map(|entry| event_of(entry, session_id, &log_key))
            .collect::<Result<_, _>>()?;

        // Seqs run 1..=len with no gaps, so a batch after `after_seq` must be
        // exactly `after_seq+1, +2, …`. This is what actually catches a
        // foreign writer's auto-generated id — shaped like `<ms>-0`, it parses
        // as a huge seq that cannot sit where contiguity says it must.
        for (offset, event) in events.iter().enumerate() {
            let expected = after_seq + 1 + offset as u64;
            if event.seq != expected {
                return Err(StoreError::Backend(anyhow::anyhow!(
                    "log `{log_key}` is not contiguous: expected seq {expected}, \
                     found {}; the log has a writer other than this store",
                    event.seq
                )));
            }
        }
        Ok(events)
    }

    async fn last_seq(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        let log_key = self.config.log_key(session_id);
        let (exists, len, newest): (bool, u64, StreamRangeReply) = redis::pipe()
            .exists(self.config.meta_key(session_id))
            .xlen(&log_key)
            .xrevrange_count(&log_key, "+", "-", 1)
            .query_async(&mut self.conn.clone())
            .await
            .map_err(backend)?;
        Self::require_session(exists, session_id)?;

        let last = newest
            .ids
            .first()
            .map_or(Ok(0), |entry| seq_of(&entry.id, &log_key))?;
        // Contiguity from 1 means the newest seq *is* the entry count. An
        // entry a foreign writer added with an auto id passes the shape check
        // but not this one. Revisit if trimming ever lands: a trimmed log
        // breaks len == last deliberately, and this check must learn the
        // trim boundary then.
        if last != len {
            return Err(StoreError::Backend(anyhow::anyhow!(
                "log `{log_key}` has {len} entries but its newest id is {last}; \
                 the log has a writer other than this store"
            )));
        }
        Ok(last)
    }
}

impl RedisSessionStore {
    /// Translate a lease-script outcome into the trait's vocabulary.
    ///
    /// `expires_at_ms` comes from the Redis clock the script read, so the
    /// handle states the truth about the record it names. It is informational
    /// either way: per the trait, validity is always decided against the
    /// stored record, never against the handle's copy.
    fn lease_from(
        &self,
        outcome: scripts::LeaseOutcome,
        session_id: &SessionId,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<Option<Lease>, StoreError> {
        match outcome {
            scripts::LeaseOutcome::Granted { now_ms } => Ok(Some(Lease {
                session_id: session_id.clone(),
                node_id: node_id.to_string(),
                expires_at_ms: now_ms + ttl_ms,
            })),
            scripts::LeaseOutcome::Refused => Ok(None),
            scripts::LeaseOutcome::NoSession => {
                Err(StoreError::SessionNotFound(session_id.clone()))
            }
        }
    }
}

/// The conformance suite's expiry lever: deleting the lease key is exactly
/// what `PX` would eventually do, so takeover behaves as if the TTL had run
/// out. Feature-gated because it exists for the shared contract tests, not
/// for production code — nothing outside a test build may depend on it.
#[cfg(feature = "test-support")]
#[async_trait::async_trait]
impl roundhouse_core::store::contract::LeaseControl for RedisSessionStore {
    async fn force_expire_lease(&self, session_id: &SessionId) {
        let _: i64 = redis::cmd("DEL")
            .arg(self.config.lease_key(session_id))
            .query_async(&mut self.conn.clone())
            .await
            .expect("the test Redis must accept a DEL");
    }
}
