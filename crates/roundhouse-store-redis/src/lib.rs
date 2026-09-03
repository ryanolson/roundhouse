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
//! | `rh:{<session_id>}:lease` | hash | holder `node_id` + fencing token, expiry enforced by Redis `PEXPIRE` |
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
//! The write path lives in `scripts`: the lease is a TTL'd hash on the Redis
//! clock, and lease-check plus append is one atomic Lua script, which
//! is what makes the fencing the trait promises actually hold under
//! concurrent writers. Requires Redis ≥ 6.2 (exclusive `XRANGE` starts,
//! effects-replicated scripts).
//!
//! The store passes the same contract suite as `MemoryStore` — instantiated
//! by the same `store_contract_suite!` macro — and the binary selects it when
//! `ROUNDHOUSE_REDIS_URL` is set (see `roundhouse-server`'s `main.rs`).

pub mod fair_use;
mod scripts;
pub mod spend;
#[cfg(feature = "test-support")]
pub mod test_support;

pub use fair_use::RedisFairUseLedger;
pub use spend::RedisSpendLedger;

use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::streams::StreamRangeReply;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::SessionId;
use roundhouse_core::now_ms;
use roundhouse_core::store::{Lease, SessionStore, StoreError};

// ---------------------------------------------------------------------------
// One `connect`, for all three Redis families this crate serves
// ---------------------------------------------------------------------------

/// How long a fresh connection attempt may take before this manager gives up
/// on it and moves to the next retry.
///
/// **Named because R-F7's fail-closed half accepts a latency, and a latency
/// nobody wrote down is one nobody can hold to.** Redis-1.2.4's own default
/// (`DEFAULT_CONNECTION_TIMEOUT`, one second) is sized for a manager that only
/// ever reconnects in the background; ours is on the critical path of a
/// ceiling check the M13.1 addendum promises refuses "within a couple of
/// seconds" of an outage, so it has to be tight enough that the retry budget
/// below still fits inside that promise even in the worst case this bounds —
/// a peer that accepts the TCP handshake and then never answers, rather than
/// the refused connection a closed port returns instantly.
const CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// How long a command may wait for a reply once a connection is up.
///
/// Reduced from the crate default's 500ms for the same reason as
/// [`CONNECTION_TIMEOUT`]: this manager sits under a ceiling check with its
/// own two-second budget, not under a background job that can afford to be
/// generous.
const RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// The smallest delay between reconnect attempts, and the base the backoff
/// grows from.
const RECONNECT_MIN_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// The delay between reconnect attempts never grows past this, so a run of
/// retries cannot itself blow the two-second budget even before
/// [`RECONNECT_RETRIES`] is reached.
const RECONNECT_MAX_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Each retry's delay is `RECONNECT_MIN_DELAY * RECONNECT_BACKOFF_FACTOR^n`,
/// clamped at [`RECONNECT_MAX_DELAY`], before jitter.
const RECONNECT_BACKOFF_FACTOR: f32 = 2.0;

/// How many times a severed connection is retried before `send_packed_command`
/// gives up and returns the error to the caller.
///
/// **The number R-F7's cost is paid in.** The crate default is six, and
/// six retries at the crate's own defaults is where the ~9.45s this bound
/// replaces came from (M13.1 review F2) — every admission after the first
/// during an outage waiting on the shared reconnect future those six retries
/// walk. Three, at the tighter delays above, keeps the worst-case sum
/// (backoff sleeps plus, if the peer black-holes rather than refuses, three
/// connection-timeout waits) comfortably under two seconds, measured against
/// a real severed connection by
/// `a_ceiling_that_cannot_be_checked_refuses_within_a_bounded_time` — while
/// still giving a connection that drops for one round trip a chance to heal
/// without every command in that window failing.
const RECONNECT_RETRIES: usize = 3;

/// Build the `ConnectionManager` every Redis-backed store, ledger and
/// fair-use tracker in this crate connects through.
///
/// One function rather than the three copies of `ConnectionManagerConfig::default()`
/// this replaces (session store, spend ledger, fair-use ledger, each
/// hand-rolled and each silently accepting the crate's six-retry default) —
/// so the outage-latency bound above is a fact about the crate, verified
/// once, rather than three unlabelled call sites that happened to agree by
/// copy-paste and could just as easily drift apart.
async fn connect_manager(url: &str) -> Result<ConnectionManager, redis::RedisError> {
    let client = redis::Client::open(url)?;
    let config = ConnectionManagerConfig::new()
        .set_connection_timeout(Some(CONNECTION_TIMEOUT))
        .set_response_timeout(Some(RESPONSE_TIMEOUT))
        .set_min_delay(RECONNECT_MIN_DELAY)
        .set_max_delay(RECONNECT_MAX_DELAY)
        .set_exponent_base(RECONNECT_BACKOFF_FACTOR)
        .set_number_of_retries(RECONNECT_RETRIES);
    ConnectionManager::new_with_config(client, config).await
}

/// Redis implementation of [`SessionStore`].
///
/// Cheap to clone: clones share one auto-reconnecting multiplexed connection.
#[derive(Clone)]
pub struct RedisSessionStore {
    conn: ConnectionManager,
    scripts: std::sync::Arc<scripts::Scripts>,
}

impl RedisSessionStore {
    /// Connect and fail fast: a store that cannot reach its Redis at startup
    /// should stop the process there, not on the first session.
    pub async fn connect(url: impl AsRef<str>) -> Result<Self, StoreError> {
        let conn = connect_manager(url.as_ref()).await.map_err(backend)?;
        Ok(Self {
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

/// Every key this store writes starts here. A constant, not configuration:
/// nothing selects a different prefix today, and an untested knob would be a
/// promise nobody checked. A future prefix parameter requires an isolation
/// test.
const KEY_PREFIX: &str = "rh";

// The braces are a Redis Cluster hash tag. Every key for one session hashes to
// one slot, which keeps the lease-fenced append single-slot scriptable. The
// keys are an internal storage detail. Feature-gated test helpers expose them
// only to the external wire-format tests that write raw Redis data.
fn meta_key(session_id: &SessionId) -> String {
    format!("{KEY_PREFIX}:{{{session_id}}}:meta")
}

fn lease_key(session_id: &SessionId) -> String {
    format!("{KEY_PREFIX}:{{{session_id}}}:lease")
}

fn log_key(session_id: &SessionId) -> String {
    format!("{KEY_PREFIX}:{{{session_id}}}:log")
}

/// The value under `…:meta`.
///
/// `created_at_ms` is informational only — nothing orders on it. Lease expiry
/// and event timestamps, where a single clock authority matters, use Redis.
#[derive(Serialize, Deserialize)]
struct SessionMeta {
    model_policy: String,
    created_at_ms: u64,
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

/// Translate a lease-script outcome into the trait's vocabulary.
fn lease_from(
    outcome: scripts::LeaseOutcome,
    session_id: &SessionId,
    node_id: &str,
    fencing_token: Uuid,
    ttl_ms: u64,
) -> Result<Option<Lease>, StoreError> {
    match outcome {
        scripts::LeaseOutcome::Granted { now_ms } => Ok(Some(Lease {
            session_id: session_id.clone(),
            node_id: node_id.to_string(),
            fencing_token,
            // Informational only: scripts check the live Redis record.
            expires_at_ms: now_ms.saturating_add(ttl_ms),
        })),
        scripts::LeaseOutcome::Refused => Ok(None),
        scripts::LeaseOutcome::NoSession => Err(StoreError::SessionNotFound(session_id.clone())),
    }
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
            .arg(meta_key(session_id))
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
        let fencing_token = Uuid::new_v4();
        let token_text = fencing_token.simple().to_string();
        let identity = scripts::LeaseIdentity::new(node_id, &token_text);
        let outcome = self
            .scripts
            .acquire(
                &mut self.conn.clone(),
                &meta_key(session_id),
                &lease_key(session_id),
                identity,
                ttl_ms,
            )
            .await?;
        lease_from(outcome, session_id, node_id, fencing_token, ttl_ms)
    }

    async fn renew_lease(&self, lease: &Lease, ttl_ms: u64) -> Result<Option<Lease>, StoreError> {
        let token_text = lease.fencing_token.simple().to_string();
        let identity = scripts::LeaseIdentity::new(&lease.node_id, &token_text);
        let outcome = self
            .scripts
            .renew(
                &mut self.conn.clone(),
                &meta_key(&lease.session_id),
                &lease_key(&lease.session_id),
                identity,
                ttl_ms,
            )
            .await?;
        lease_from(
            outcome,
            &lease.session_id,
            &lease.node_id,
            lease.fencing_token,
            ttl_ms,
        )
    }

    async fn release_lease(&self, lease: &Lease) -> Result<(), StoreError> {
        let token_text = lease.fencing_token.simple().to_string();
        let identity = scripts::LeaseIdentity::new(&lease.node_id, &token_text);
        self.scripts
            .release(
                &mut self.conn.clone(),
                &lease_key(&lease.session_id),
                identity,
            )
            .await
    }

    /// `EXISTS` on the lease key, which is the whole answer here.
    ///
    /// Expiry needs no arithmetic: the lease is a Redis key with a TTL, so a
    /// tenure that stopped renewing has already stopped existing — the same
    /// authority `acquire` runs on, rather than this process's clock compared
    /// against a stored deadline. The session's own existence is checked
    /// alongside it for the reason [`Self::last_seq`] checks it: a missing
    /// session is a caller error, and answering "not leased" for one would let
    /// it read as an ordinary idle session.
    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        let (exists, leased): (bool, bool) = redis::pipe()
            .exists(meta_key(session_id))
            .exists(lease_key(session_id))
            .query_async(&mut self.conn.clone())
            .await
            .map_err(backend)?;
        Self::require_session(exists, session_id)?;
        Ok(leased)
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let token_text = lease.fencing_token.simple().to_string();
        let identity = scripts::LeaseIdentity::new(&lease.node_id, &token_text);
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
                &meta_key(&lease.session_id),
                &lease_key(&lease.session_id),
                &log_key(&lease.session_id),
                identity,
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
        let log_key = log_key(session_id);
        // XRANGE treats COUNT 0 as unlimited. Fetch at most one entry for a
        // zero-limit request, then discard it below. The pipelined EXISTS still
        // enforces SessionNotFound without a second round trip or branch.
        let redis_limit = limit.max(1);

        // `(` is an exclusive start. Ids are always `<seq>-0`, so excluding
        // exactly `after_seq-0` is precisely "seq > after_seq" — with no
        // arithmetic on `after_seq` that could overflow at u64::MAX.
        let (exists, range): (bool, StreamRangeReply) = redis::pipe()
            .exists(meta_key(session_id))
            .xrange_count(&log_key, format!("({after_seq}-0"), "+", redis_limit)
            .query_async(&mut self.conn.clone())
            .await
            .map_err(backend)?;
        Self::require_session(exists, session_id)?;

        let events: Vec<SessionEvent> = range
            .ids
            .iter()
            .take(limit)
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
        let log_key = log_key(session_id);
        let (exists, len, newest): (bool, u64, StreamRangeReply) = redis::pipe()
            .exists(meta_key(session_id))
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
