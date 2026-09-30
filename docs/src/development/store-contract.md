# Session store contract

This chapter states the `SessionStore` contract that every backend must meet, and shows how the Redis backend meets it. The trait is in `crates/roundhouse-core/src/store.rs`. The Redis backend is in `crates/roundhouse-store-redis/src/`. For the deployment side of Redis, see [Deploy with Redis](../operations/redis.md).

## The seam is two primitives

A backend provides exactly two things:

- An append-only log with store-assigned, contiguous sequence numbers.
- A single-writer lease that fences appends.

Conversation items, the routing ledger and the metrics are projections of the one log. They are not stored separately. There is one write path, so the log and the materialized state cannot disagree after a crash. Everything else in the system sits above the trait.

Nothing Redis-shaped can leak upward. Stream ids, connection handles, script hashes and key names stay inside the backend. The store returns `SessionEvent` and `Lease` values that core defines.

Two implementations exist. `MemoryStore` serves tests and single-process runs. `RedisSessionStore` serves deployments.

## The trait

| Method | Purpose |
|---|---|
| `create_session` | Create a session. Returns `false`, not an error, when the session exists |
| `acquire_lease` | Claim single-writer ownership. Returns `None` when another live node holds it |
| `renew_lease` | Extend a held lease. `None` means the tenure is over |
| `release_lease` | Give up a lease |
| `is_leased` | Ask whether a live tenure holds the lease. This is not an acquisition |
| `append_events` | Append a batch of events, fenced by the lease. It can carry a learning mark |
| `read_events` | Read events with `seq > after_seq`, oldest first |
| `last_seq` | The highest assigned sequence number, or 0 for an empty session |
| `clear_learning_mark`, `requeue_learning` | Change the learning index. See [The learning index](#the-learning-index) |
| `pending_learning`, `learning_sessions` | Page through the learning index |

Every read is by session id. The trait has no list, no scan and no per-project index of sessions. A caller that needs every session therefore needs a new enumeration method, and that method is a new contract obligation for every backend. Only the learning index enumerates, and only the sessions that it marked.

## The contract

### Fenced append

`append_events` checks the lease against the current record of the store. The check and the append are one atomic step. A displaced writer fails with `LeaseLost`. It can never interleave with its successor. This is the only thing between a failover and a split-brain log.

### A lease is a tenure identity

A `Lease` holds a node id and a fencing token. It is not a snapshot of an expiry time.

- The store decides validity from its current record, never from the `expires_at_ms` in the handle. A handle whose expiry has passed still appends while the tenure that it names is live.
- The session heartbeat renews the record on a separate task from the writer, and every append keeps using the original handle. A store that rejected a stale-looking handle would fail every append in a long turn.
- Another live holder blocks acquisition. An expired lease can be taken.
- Every successful acquisition mints a fresh fencing token. This includes a re-acquisition by the node that holds the lease. Handles from the earlier tenure then fail.
- `renew_lease` returning `None` is final. The record belongs to someone else. The caller stops, and does not re-acquire behind the successor.
- `release_lease` is lenient. Releasing a lease that you no longer hold, or one for a session that no longer exists, is not an error.

The engine uses a lease time-to-live of 10 000 ms by default. The heartbeat renews the lease every third of that time.

### Sequencing

- `seq` starts at 1, is contiguous per session, and the store assigns it at append time.
- `read_events(after_seq)` returns events oldest first, with no gap and no repeat.
- A batch append assigns contiguous sequence numbers in one call.

### Errors

| Error | Meaning |
|---|---|
| `SessionNotFound` | The operation names an unknown session |
| `LeaseLost` | The lease is not the current tenure |
| `InvalidLearningMark` | The mark names no event of its batch. The store refuses it before any write |
| `LearningProjectMismatch` | The session is already marked for another project |
| `CorruptLog` | One session holds stored data that the store never writes |
| `Backend` | The store failed |

`CorruptLog` is separate from `Backend` on purpose. It says that the store answered and that the fault is in the data of one session. A foreign writer or a partial restore can cause it. A caller that pauses when the store is down would stop at the same session on every attempt. One bad log would then starve every session behind it. A caller that does not need the distinction treats `CorruptLog` like `Backend`.

### `is_leased`

A projection of the log cannot tell a turn in progress from a turn whose writer died. Both leave items with no terminal event. Only the lease tells them apart. Prefix admission asks `is_leased` before it supersedes items that a turn that never ended stamped.

The method is not an acquisition. Acquiring a free session would evict the turn that is about to start. The trait default answers `true`, which means "cannot prove the session is idle". The conservative direction is to leave history alone. Every real backend overrides the default.

### Rules that keep the contract portable

- **The contract names outcomes, not mechanisms.** "Atomically fenced" says nothing about Lua scripts. Redis meets it with a script. Another backend can meet it with optimistic concurrency or a transaction.
- **Reads are polls, not subscriptions.** The SSE follower polls `read_events` every 25 ms. A required blocking tail would exclude a backend without cheap push. No measurement shows that the poll interval costs anything.

## The Redis backend

### Keys

Each session has three keys. They share a hash tag on the session id, so the multi-key lease and append operations stay in one Redis Cluster slot.

| Key | Type | Holds |
|---|---|---|
| `rh:v1:sess:{<session_id>}:meta` | string | JSON `{model_policy, created_at_ms}`, written with `SET NX` |
| `rh:v1:sess:{<session_id>}:lease` | hash | The `node_id` and the fencing token. Redis expires the key |
| `rh:v1:sess:{<session_id>}:log` | stream | One entry per event, with the explicit id `<seq>-0` |

`rh` is the default namespace. The `ROUNDHOUSE_REDIS_NAMESPACE` variable changes it. `v1` is the schema version of the `sess` family. Every key comes from one builder function, `build_key`, which joins the namespace, the version, the family and the parts with colons.

`create_session` is a `SET NX` on `meta`. The reply tells "created" from "already existed". The store mints session ids as `sess_<uuid-simple>`. A client can adopt its own id. An id that contains braces changes the slot choice.

### The entry id is the sequence number

`XADD` with the explicit id `<seq>-0` makes the stream id and the event `seq` one number. This decision shrinks the read surface:

- `read_events(after_seq, limit)` is one `XRANGE` with the exclusive start `(<after_seq>-0` and `+`, and a `COUNT`. No client-side filtering is needed. The exclusive start needs Redis 6.2 or later.
- `last_seq` is one `XREVRANGE ... COUNT 1`. An empty stream reads as 0.
- The append script enforces contiguity at the one place that assigns ids. The next id is the newest id plus one. No separate counter key can drift from the stream.

Each entry has two fields. `at_ms` holds the timestamp. `kind` holds the `serde_json` encoding of `SessionEventKind`. The store rebuilds `SessionEvent` on read from the entry id, the key and those fields. Lua never parses or splices JSON. A schema change is a core concern, and the scripts do not move.

`read_events` and `last_seq` pipeline an `EXISTS` on `meta`. This tells an empty session from `SessionNotFound`.

### The lease runs on the Redis clock

The lease hash expires through `PEXPIRE`. All time comes from `redis.call('TIME')` inside the scripts. The clock of the process, which `MemoryStore` uses, is not used here. Node clocks can differ in a deployment with several nodes. One Redis clock keeps that difference from opening a fencing hole. The `expires_at_ms` in a returned `Lease` is for information only.

Four Lua scripts run through `redis::Script`, which uses `EVALSHA` and falls back when the script is not loaded:

| Script | Behavior |
|---|---|
| acquire | Fails `SessionNotFound` when `meta` is absent. Writes the new token and the time-to-live when the lease is absent or names this node. Otherwise refuses. A lease key that Redis expired counts as absent, so takeover needs no expiry arithmetic |
| renew | Applies `PEXPIRE` only when both the node id and the token match. Otherwise refuses |
| release | Deletes the key only when the node id and the token still match |
| append | Checks `meta`, then the node id and the token, then reads the newest entry id. Then it runs `XADD` for each event at the next ids, with `at_ms` from `TIME`. It returns the assigned sequence numbers and the timestamp |

The append script is the reason that scripts are used at all. The fence check and the append must be one atomic step. A writer that lost its lease between a check and a write would otherwise still write. Scripts replicate by effects, so calling `TIME` before a write is safe.

The scripts return a tag with optional numbers. Rust decodes the tag into a typed outcome. The tags are a contract between the Lua and the Rust.

| Tag | Script | Rust result |
|---|---|---|
| `OK` | all | Success |
| `NOSESSION` | acquire, renew, append | `SessionNotFound` |
| `REFUSED` | acquire, renew | The lease call returns `None` |
| `FENCED` | append | `LeaseLost` |
| `PROJECT` | append | `LearningProjectMismatch` |
| `CORRUPT` | append | `Backend`. The newest entry id is not of the form `<seq>-0` |
| `WRONGTYPE` | append | `Backend`. A shared learning index key holds another type |
| `BADMARK` | append | `CorruptLog`. The stored mark of this session cannot be read |
| `RANGE` | append | `Backend`. The batch would pass the last exact sequence number |

Every check that can fail runs before the first `XADD`. Redis keeps the earlier writes of a script when a later command fails. A failure found after a write would leave durable events with no mark, or part of a batch.

### Damaged data fails loudly

A log entry that breaks the format fails the read as `CorruptLog`. A missing field, or an id that a foreign writer generated, are examples. So does a key of one session that a foreign writer replaced with another type. Skipping the entry would drop events from a replay silently. A replay that quietly disagrees with what was appended is the failure that an event-sourced store must never have.

The append script refuses to append after a newest entry that it did not write. Appending after a foreign entry would launder it into a log that otherwise proves its own integrity.

Lua renders numbers with `%.14g`. The id for sequence 10^14 would come out as `1e+14-0`, and its `XADD` would fail. The script therefore refuses any batch that would pass sequence 99 999 999 999 999. It makes this check before the first write, so a refused append leaves no partial batch.

### Client, durability and scope

- The client is `redis` 1.2 with `ConnectionManager`. It is multiplexed and reconnects by itself. One `connect_manager` function builds the manager for every family. See [Deploy with Redis](../operations/redis.md#outages) for the time bounds.
- **Durability is a deployment fact.** The log is as durable as the Redis that holds it. The AOF `appendfsync` setting and replication decide. The crate does not try to do better than the persistence configuration of the operator.
- **There is no retention.** The trait has no delete, and sessions have no end of life. Trimming a stream behind the trait would break replay. Sessions and their logs grow without bound.
- **Redis Cluster is not supported.** The key layout keeps each session in one slot, but the client is single-node. An append that carries a learning mark touches keys in several slots.
- The crate never uses `SCAN` or `KEYS` outside its test helpers.

## Log schema changes are additive

A log can hold entries from any earlier build. The rule is that a change to the schema adds a variant or a field with `#[serde(default)]`. It never changes an existing variant. Three other ways to change a stored record were rejected:

- **Rewrite the stored logs once.** A Redis stream entry cannot be rewritten in place, because `XADD` needs ids that increase strictly. A rewrite means reading everything, writing a new stream key, and swapping it under the lease with every follower stopped. It would also move the turn id of every rewritten conversation and orphan retries in flight. It could not recover data that was never written.
- **Canonicalize at read time.** This costs almost nothing and adds no information that the record lacked.
- **Tag the record with a version, for example `"type":"tool_call_v2"`.** A new variant on an internally tagged enum makes an older build fail to deserialize. A log that no longer reads cannot be recovered by a rollback.

The version segment in each Redis key family is a separate lever for a change of key space. A change to one family then leaves the key space of the other families in place.

## The learning index

The online routing learner folds some session events into shared counters in a separate store. Every call to that store can fail. The record of which sessions still owe entries to it therefore cannot live there. A session whose learner-store calls all failed would never be registered.

The index lives in the session store. The same atomic, fenced step that appends an entry-producing event also writes the index. A durable event is always discoverable. `Session::commit` is the only production caller of `append_events`. It computes the mark with the pure function `learning_mark(&state, &kinds)`, so a new write method cannot forget it. A session with no principal, or with no learned `Routed` event, is never marked. A project that never enables the learner gets appends that are byte-identical to unmarked appends.

The index has two parts:

- **The permanent mark.** It holds the sequence number of the newest marked event, the project and the store clock at marking time. It is never removed. An audit or an offline rebuild enumerates it.
- **Pending membership.** It holds the sessions whose mark no confirmed learner watermark covers yet.

The design has these rules:

- **The identity of a mark is its log sequence number.** The store assigns it in the same step as the append. Time only schedules recovery through the idle filter of `pending_learning`. It is never an identity.
- **Clearing is a predicate.** `clear_learning_mark(session, confirmed_through)` removes pending membership only when the mark is at or below a watermark that the learner store returned. `requeue_learning(session, mark)` adds the session back only when the mark is unchanged. Model checks of an earlier design failed here. A delayed clear removed a newer registration. A clear on an equal time removed a registration from the same millisecond. A clear with no condition failed both cases. A fresh opaque token was considered instead. The predicate is at least as strong. A token proves only that the registration did not change. The predicate proves that every entry up to the mark is delivered. The design assumes that the log never reuses a sequence number after it loses acknowledged appends.
- **Pages are ordered by session id bytes, never by time.** A repeated oldest-first page with no cursor returns the same head forever. One session that the consumer cannot finish would starve the sessions behind it. A time-ordered set also has no exact resume point in Redis. A page examines at most its limit of members. The cursor moves past every examined member. An empty page can therefore carry a cursor, and recovery moves past busy sessions.
- **An unreadable mark is named, not fatal.** `LearningPage::unreadable` lists such sessions. A page that failed on one would fail at that member on every attempt.

The Redis keys of the index are in the `sess` family. They carry no hash tag, because no one slot can hold them next to every session:

| Key | Type | Holds |
|---|---|---|
| `rh:v1:sess:learning:marks` | hash | Session id to mark: sequence, project and time. Permanent |
| `rh:v1:sess:learning:marked` | sorted set | Every session ever marked. Permanent |
| `rh:v1:sess:learning:pending` | sorted set | Sessions whose mark is not confirmed delivered |

Both sorted sets hold every member at score zero and are paged lexicographically. A marked append costs one hash write and two sorted-set writes. It touches six keys in several slots, so it runs on a single node only. The index has no compaction.

## The learner store in Redis

`RedisLearnerStore` holds the learned counters. It is a separate family (`learn`, version `v1`). Every key carries a `{project}` hash tag and is built by `build_key`.

| Key | Type | Holds |
|---|---|---|
| `rh:v1:learn:{<project>}:wm` | hash | Session id to watermark |
| `rh:v1:learn:{<project>}:<epoch>:q:<level>:<key>` | hash | `<strategy>:pos`, `<strategy>:n`, `<strategy>:sessions`, `jev_capable`, `jev_efficient` |
| `rh:v1:learn:{<project>}:<epoch>:ops` | hash | `<target>:lat_sum`, `lat_n`, `failover`, `cache_pred`, `cache_obs`, `cache_n`, and `turn:pre_sum`, `turn:pre_n` |
| `rh:v1:learn:{<project>}:<epoch>:seen:<session>` | set | The `<level>:<key>:<strategy>` members that the session counted in `sessions` |

The script discipline is:

- `read` is one script over the three quality keys of a turn and the ops key. Its cost grows with the number of strategies and targets. It does not grow with the number of keys or sessions. It writes nothing, so a read replica can serve it. `apply` is one script over the keys of one project.
- Rust names every key, through `KEYS`, and every field, through `ARGV`. The scripts name none. `build_key` is therefore the only spelling of a key. A convention test checks both halves.
- The scripts use only `TYPE`, `HGET`, `HMGET`, `HSET`, `SISMEMBER` and `SADD`. They pass counters as numbers. A Lua `tostring` or `..` formats with `%.14g` and loses digits past the 14th.
- The scripts reply with arrays of integers only. Refusals use 1-based positions.
- The scripts parse stored counters strictly: decimal digits, and a leading `-` only for signed fields. Plain `tonumber` would also read `0x10`, `1e3` and ` 5`.
- Writes go out in chunks of at most 1000 values, because Lua `unpack` raises an error past about 8000 results.

This family supports a single Redis instance. The hash tag keeps one project on one slot, but no test runs against a Cluster. The watermark hash and the `seen` sets keep one entry per session forever. There is no compaction.

## Mapping the contract onto NATS JetStream

No second backend exists. This mapping was a design check to show that the trait does not secretly need Redis. The JetStream semantics below were not tested against a server.

| Obligation | Redis | JetStream |
|---|---|---|
| Append-only log per session | One stream key per session | One stream with the subject `rh.sess.<id>` per session. JetStream prefers subjects in a stream to a stream per session |
| Store-assigned contiguous `seq` | Entry id `<seq>-0`, assigned inside the script | The `seq` travels in the message. A publish with `Nats-Expected-Last-Subject-Sequence` enforces it, and the writer retries on conflict |
| Fenced append | Lua: check node and token, then `XADD` | The lease epoch in a KV bucket, read before the publish, plus the same check on the last sequence. The check fails for a displaced writer |
| Lease with expiry in the store | TTL on a hash | A KV entry with a per-key time-to-live, or an expiry time in the value updated with a revision check |
| `read_events(after_seq)` | `XRANGE` | A direct get by subject sequence, or an ordered pull consumer |
| `last_seq` | `XREVRANGE ... COUNT 1` | The stream info for the subject |

Where Redis gets atomicity from a script, JetStream gets it from optimistic concurrency. Both give the contracted outcome, which is a fenced and contiguous log. The stream sequence of JetStream is global to the stream. The per-session `seq` must therefore be data and not infrastructure. The trait already forces this, because `seq` is a field of `SessionEvent` and the trait exposes no backend cursor type.

## The contract suite

The contract is also a test suite. The functions in `crates/roundhouse-core/src/store/contract.rs` state every guarantee above. They cover:

- Creation is idempotent, and unknown sessions are not found.
- A live lease blocks other nodes, and a re-take mints a fresh fence.
- An expired lease can be taken, and the loser cannot append.
- A lease reads as live only while someone holds it.
- A released lease is gone and cannot be renewed. A release by a node that does not hold the lease changes nothing.
- A stale handle still appends while its record is live.
- Appends assign contiguous sequence numbers, and replay has no gap.
- Reads page oldest first and reproduce the append.
- A renewal fails once another node took the lease.
- The learning index rules in `contract/learning.rs`.

The `store_contract_suite!` macro is the one list of these tests. A backend instantiates the whole suite with one macro call. It gets every test, or none. No wiring step exists where a test can be forgotten for one backend. The memory store runs the suite always. It is the reference semantics. The Redis store runs the same macro under the `ignore` gate described in [Testing](testing.md).

Expiring a lease without waiting needs a lever in the store. The test-only trait `LeaseControl` provides it with one method, `force_expire_lease`. `MemoryStore` backdates its record. The Redis implementation deletes the lease key. The contract only requires that the current holder stops being live.

Every test mints fresh session ids, so one shared Redis can host the whole suite. The Redis tests in `crates/roundhouse-store-redis/tests/` add cases that only a real backend shows:

- Two nodes race to acquire after an expiry, and the loser's append in flight is rejected.
- Appends and renewals from separate connections interleave, which shows that the scripts are atomic.
- Every `SessionEventKind` variant survives a round trip through the fenced append and the stream fields.
- The store recovers after its connection is killed mid-sequence, with no gap and no repeat.
