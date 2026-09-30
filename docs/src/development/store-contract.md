# Session store contract

This chapter states the `SessionStore` contract that every backend must meet, and how the Redis backend meets it. The trait is in `crates/roundhouse-core/src/store.rs`, and the Redis backend is in `crates/roundhouse-store-redis/src/lib.rs` and `scripts.rs`. For running Redis, see [Deploy with Redis](../operations/redis.md).

## The seam is two primitives

A backend provides an append-only log with store-assigned, contiguous sequence numbers, and a single-writer lease that fences appends. Conversation items, the routing ledger and the metrics are projections of that log, so there is one write path, and the log and the materialized state cannot disagree after a crash. Stream ids, connection handles, script hashes and key names stay inside the backend; the store returns the `SessionEvent` and `Lease` types that core defines.

`MemoryStore` serves tests and single-process runs. `RedisSessionStore` serves deployments.

## The trait

| Method | Purpose |
|---|---|
| `create_session` | Create a session. Returns `false`, not an error, when it exists |
| `acquire_lease` | Claim single-writer ownership. `None` when another live node holds it |
| `renew_lease` | Extend a held lease. `None` means the tenure is over |
| `release_lease` | Give up a lease |
| `is_leased` | Ask whether a live tenure holds the lease, without acquiring it |
| `append_events` | Append a batch, fenced by the lease, with an optional learning mark |
| `read_events` | Read events with `seq > after_seq`, oldest first, up to a limit |
| `last_seq` | The highest assigned sequence number, or 0 for an empty session |
| `clear_learning_mark`, `requeue_learning` | Change the learning index. See [The learning index](#the-learning-index) |
| `pending_learning`, `learning_sessions` | Page through the learning index |

Every other read is by session id. The trait has no list, no scan and no per-project index. A caller that needs every session needs a new method, which is a new obligation for every backend.

## The contract

### Fenced append

`append_events` checks the lease against the store's current record, atomically with the append. A displaced writer fails with `LeaseLost` and can never interleave with its successor. This is the only barrier between a failover and a split-brain log.

### A lease is a tenure identity

A `Lease` holds a node id and a fencing token. It is not an expiry snapshot.

- Validity comes from the store's current record, never from the handle's `expires_at_ms`, so an expired-looking handle appends while its tenure is live. The heartbeat renews on a separate task while appends use the original handle; a store that rejected stale-looking handles would fail every append in a long turn.
- Another live holder blocks acquisition. An expired lease can be taken.
- Every successful acquisition mints a fresh fencing token, including a re-acquisition by the holder. Handles from the earlier tenure then fail.
- `renew_lease` returning `None` is final. The caller stops and does not re-acquire behind the successor.
- `release_lease` is lenient. Releasing a lease you no longer hold is not an error.

The engine's default lease time-to-live is 10,000 ms (`EngineConfig::default`). The heartbeat renews every third of that.

### Sequencing

- `seq` starts at 1, is contiguous per session, and is assigned by the store at append time.
- A batch append assigns contiguous sequence numbers in one call.
- `read_events(after_seq)` returns events oldest first, with no gap and no repeat.

### Errors

| Error | Meaning |
|---|---|
| `SessionNotFound` | The operation names an unknown session |
| `LeaseLost` | The lease is not the current tenure |
| `InvalidLearningMark` | The mark names no event of its batch. Refused before any write |
| `LearningProjectMismatch` | The session is already marked for another project |
| `CorruptLog` | One session holds stored data that the store never writes |
| `Backend` | The store failed |

`CorruptLog` is separate from `Backend` so that no caller reads it as an outage. A caller that pauses while the store is down would stop at the same session on every attempt, and one bad log would starve every session behind it. A caller without that distinction treats `CorruptLog` like `Backend`.

### `is_leased`

A projection of the log cannot tell a turn in progress from a turn whose writer died; both leave items with no terminal event. Prefix admission asks `is_leased` before it supersedes such items. It is not an acquisition, because acquiring a free session would evict the turn about to start. The trait default answers `true` ("cannot prove the session is idle"), the conservative direction. Every real backend overrides it.

### Rules that keep the contract portable

- **The contract names outcomes, not mechanisms.** "Atomically fenced" says nothing about Lua. Another backend can meet it with optimistic concurrency or a transaction.
- **Reads are polls, not subscriptions.** The SSE follower polls `read_events` every 25 ms (`POLL_INTERVAL`). A required blocking tail would exclude a backend without cheap push. No measurement shows that the poll costs anything.

## The Redis backend

### Keys

Each session has three keys with a hash tag on the session id, so the multi-key lease and append scripts stay in one Cluster slot.

| Key | Type | Holds |
|---|---|---|
| `rh:v1:sess:{<session_id>}:meta` | string | JSON `{model_policy, created_at_ms}`, written with `SET NX` |
| `rh:v1:sess:{<session_id>}:lease` | hash | `node_id` and `fencing_token`. Redis expires the key |
| `rh:v1:sess:{<session_id>}:log` | stream | One entry per event, with the explicit id `<seq>-0` |

`rh` is the default namespace, which `ROUNDHOUSE_REDIS_NAMESPACE` changes, and `v1` is the version of the `sess` family. See [Key rules](../operations/redis.md#key-rules). `create_session` is `SET NX` on `meta`, and its reply tells "created" from "already existed".

### The entry id is the sequence number

`XADD` with the explicit id `<seq>-0` makes the stream id and the `seq` one number:

- `read_events(after_seq, limit)` is one `XRANGE` from the exclusive start `(<after_seq>-0` to `+` with `COUNT`. No client-side filtering. The exclusive start needs Redis 6.2.
- `last_seq` is one `XREVRANGE ... COUNT 1`. An empty stream reads as 0.
- The append script assigns the next id as the newest id plus one. No separate counter key can drift from the stream.

Each entry has two fields: `at_ms`, and `kind` holding the `serde_json` encoding of `SessionEventKind`. The store rebuilds `SessionEvent` from the entry id, the key and those fields. Lua never parses JSON, so a schema change is a core concern and the scripts do not move. `read_events` and `last_seq` pipeline an `EXISTS` on `meta` to tell an empty session from `SessionNotFound`.

### The lease runs on the Redis clock

The lease hash expires through `PEXPIRE`, and every time comes from `redis.call('TIME')` inside the scripts. Node clocks can differ; one clock keeps that difference from opening a fencing hole. The `expires_at_ms` in a returned `Lease` is for information only.

Four scripts run through `redis::Script` (`EVALSHA`, loading on a miss):

| Script | Behavior |
|---|---|
| acquire | `NOSESSION` when `meta` is absent. Writes the new token and the time-to-live when the lease is absent or names this node, else refuses. An expired key is absent, so takeover needs no expiry arithmetic |
| renew | Applies `PEXPIRE` only when both node id and token match |
| release | Deletes the key only when node id and token still match |
| append | Checks `meta`, then node id and token, then reads the newest entry id. Runs `XADD` at the next ids with `at_ms` from `TIME`. Returns the timestamp and the last sequence number |

The append script is why scripts are used at all: the fence check and the write must be one step, or a writer that lost its lease between them would still write. Scripts replicate by effects, so calling `TIME` before a write is safe.

Each script returns a tag and optional values. Rust decodes the tag into a typed outcome:

| Tag | Script | Rust result |
|---|---|---|
| `OK` | all | Success |
| `NOSESSION` | acquire, renew, append | `SessionNotFound` |
| `REFUSED` | acquire, renew | The lease call returns `None` |
| `FENCED` | append | `LeaseLost` |
| `PROJECT` | append | `LearningProjectMismatch` |
| `CORRUPT` | append | `Backend`: the newest entry id is not `<seq>-0` |
| `WRONGTYPE` | append | `Backend`: a shared learning index key holds another type |
| `BADMARK` | append | `CorruptLog`: this session's stored mark cannot be read |
| `RANGE` | append | `Backend`: the batch would pass the last exact sequence number |

Every check that can fail runs before the first `XADD`. Redis keeps a script's earlier writes when a later command fails, so a late failure would leave part of a batch, or events with no mark.

### Damaged data fails loudly

A read of an entry that breaks the format (a missing field, a foreign id, a session key of another type) fails as `CorruptLog`. Skipping it would drop events from a replay silently, the one failure an event-sourced store must never have. The append script refuses to append after an entry it did not write, which would launder that entry into the log.

Lua renders numbers with `%.14g`, so the id for sequence 10^14 would be `1e+14-0` and its `XADD` would fail. The script refuses any batch that would pass sequence 99,999,999,999,999 (`LAST_EXACT_SEQ`), before the first write.

### Client

The client is `redis` 1.2 with `ConnectionManager`, multiplexed and reconnecting. One `connect_manager` function builds it for every family. The pin is in [Upstream dependencies](upstream.md), and the timeouts are in [Deploy with Redis](../operations/redis.md#outages). Durability, retention and Cluster scope are deployment facts, listed in [Deploy with Redis](../operations/redis.md#durability-retention-and-scope). The crate uses `SCAN` or `KEYS` only in its test helpers.

## Log schema changes are additive

A log can hold entries from any earlier build. A schema change adds a variant or a field with `#[serde(default)]`, and never changes an existing variant. Three alternatives were rejected:

- **Rewrite the stored logs.** A stream entry cannot be rewritten in place, because `XADD` needs strictly increasing ids. A rewrite means a new stream key swapped in under the lease with every follower stopped. It would move the turn id of every rewritten conversation, orphan retries in flight, and recover no data that was never written.
- **Canonicalize at read time.** It adds no information that the record lacked.
- **Version the tag, for example `"type":"tool_call_v2"`.** A new variant of an internally tagged enum makes an older build fail to deserialize, so a rollback cannot read the log.

The version segment of each Redis key family is a separate lever, for a change of key space in one family.

## The learning index

The routing learner folds some events into counters in a separate learner store, and every call to that store can fail. So the record of which sessions still owe entries lives in the session store, written by the same fenced step that appends an entry-producing event. `Session::commit`, the only production caller of `append_events`, computes the mark with the pure function `learning_mark(&state, &kinds)`. A session with no principal or no learned `Routed` event is never marked.

The index has two parts. The permanent mark of a session holds the newest marked sequence number, the project and the store clock at marking; an audit or offline rebuild enumerates it. The pending set holds the sessions whose mark no confirmed learner watermark covers yet.

- **A mark's identity is its log sequence number**, assigned in the same step as the append. Time only schedules recovery through the idle filter of `pending_learning`.
- **Clearing is a predicate.** `clear_learning_mark(session, confirmed_through)` removes pending membership only when the mark is at or below a watermark that the learner store returned. `requeue_learning(session, mark)` adds it back only when the mark is unchanged. Model checks of earlier designs failed: a delayed clear removed a newer registration, a clear on equal time removed one from the same millisecond, and a clear with no condition failed both cases. An opaque token proves only that the registration did not change; the predicate proves that every entry up to the mark is delivered. It assumes the log never reuses a sequence number after losing acknowledged appends.
- **Pages are ordered by session id bytes, never by time.** An oldest-first page returns the same head forever, so one session the consumer cannot finish starves the rest. A page examines at most its limit, and the cursor moves past every examined member.
- **An unreadable mark is named, not fatal.** `LearningPage::unreadable` lists it. A page that failed on one member would fail there on every attempt.

The Redis keys are in the `sess` family, with no hash tag, because no slot can hold them next to every session:

| Key | Type | Holds |
|---|---|---|
| `rh:v1:sess:learning:marks` | hash | Session id to `<seq>:<at_ms>:<project>`. Permanent |
| `rh:v1:sess:learning:marked` | sorted set | Every session ever marked. Permanent |
| `rh:v1:sess:learning:pending` | sorted set | Sessions whose mark is not confirmed delivered |

Both sorted sets hold every member at score zero and are paged lexicographically. A marked append adds one `HSET` and two `ZADD`s and touches six keys across slots, so it is single-node only. The index has no compaction. How the learner uses it is in [The routing learner](../concepts/routing-learner.md#recovery).

## Mapping the contract onto NATS JetStream

No second backend exists. This mapping checks that the trait does not secretly need Redis; the JetStream side was not tested against a server.

| Obligation | Redis | JetStream |
|---|---|---|
| Append-only log per session | One stream key per session | One stream, subject `rh.sess.<id>` per session |
| Store-assigned contiguous `seq` | Entry id `<seq>-0`, assigned in the script | `seq` in the message, enforced by publish with `Nats-Expected-Last-Subject-Sequence`, retried on conflict |
| Fenced append | Lua: check node and token, then `XADD` | Lease epoch in a KV bucket, read before publish, plus the sequence check |
| Lease with expiry in the store | TTL on a hash | KV entry with a per-key time-to-live, or an expiry in the value updated with a revision check |
| `read_events(after_seq)` | `XRANGE` | Direct get by subject sequence, or an ordered pull consumer |
| `last_seq` | `XREVRANGE ... COUNT 1` | Stream info for the subject |

JetStream gets atomicity from optimistic concurrency where Redis uses a script. Its stream sequence is global to the stream, so the per-session `seq` must be data. The trait already forces this: `seq` is a field of `SessionEvent`, and the trait exposes no backend cursor.

## The contract suite

The functions in `crates/roundhouse-core/src/store/contract.rs` (and `contract/learning.rs`) state every guarantee above as tests. The `store_contract_suite!` macro is the one list of them. A backend instantiates the whole suite with one call, so no test can be left out for one backend. `MemoryStore` runs it always and is the reference semantics. `RedisSessionStore` runs it under the `#[ignore]` gate in [Testing](testing.md#real-redis).

Expiring a lease without waiting needs a lever. The test-only trait `LeaseControl` has one method, `force_expire_lease`: `MemoryStore` backdates its record and the Redis implementation deletes the lease key.

Every test mints fresh session ids, so one Redis can host the suite. `crates/roundhouse-store-redis/tests/` adds cases that only a real backend shows:

- two nodes race to acquire after an expiry, and the loser's in-flight append is rejected
- appends and renewals interleave from separate connections
- every `SessionEventKind` variant round-trips through the stream fields
- the store recovers after its connection is killed mid-sequence, with no gap and no repeat.
