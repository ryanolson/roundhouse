<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Plan: the stateful store component (Redis first, backend-portable by contract)

> **Status: delivered.** All four milestones landed; this document remains as
> the design rationale. Where implementation diverged from the plan, the text
> says so inline (the `#[ignore]` gating in §4.3 is the notable case).

This crate is the durable half of the statefulness claim in the README. The
plan below is written against the code as it stands: the abstraction already
exists and is already consumed everywhere — `SessionStore` in
`roundhouse-core/src/store.rs`, taken generically by `Session<S>`, `Engine<S,
T>`, and every transport. Nothing above the trait changes. The work is (1) to
state the contract precisely enough that any backend can be held to it, (2) to
implement it on Redis, and (3) to keep the trait honest about portability so a
NATS JetStream (or FoundationDB, or SQL) backend is a new crate, not a
redesign.

## 1. The seam is the trait, and it stays this small

`SessionStore` is seven methods: `create_session`, `acquire_lease`,
`renew_lease`, `release_lease`, `append_events`, `read_events`, `last_seq`.
That smallness is load-bearing. Items, the routing ledger, and metrics are all
projections of the one event log, so a backend provides exactly two things — an
append-only log with store-assigned contiguous sequence numbers, and a
single-writer lease that fences appends — and everything else in the system
falls out above the trait.

The contract a backend must satisfy, extracted from the trait docs and the
`MemoryStore` reference semantics:

- **Fenced append.** `append_events` validates the lease against the store's
  *current* record, atomically with the append. A displaced writer fails with
  `LeaseLost`; it can never interleave with its successor.
- **Lease is an identity, not a snapshot.** A handle whose own `expires_at_ms`
  has passed still appends while the stored record it names is live. The
  session heartbeat renews on a separate task from the writer and depends on
  this.
- **Acquisition:** idempotent for the holding node; blocked by another live
  holder; an expired lease is takeable. `renew_lease → None` is final — the
  caller stops rather than re-acquiring behind a successor.
- **Sequencing:** `seq` starts at 1, is contiguous per session, and is assigned
  by the store at append time. `read_events(after_seq)` returns oldest-first
  with no gap or repeat; `last_seq` is 0 for an empty session.
- **Errors:** operations on unknown sessions fail `SessionNotFound`;
  `create_session` returns `false` (not an error) for an existing session.

Two rules keep this portable, and they are rules about what *not* to add:

- **The contract names outcomes, not mechanisms.** "Atomically fenced" says
  nothing about Lua scripts or CAS headers. Redis satisfies it with a script;
  JetStream would satisfy it with `Nats-Expected-Last-Subject-Sequence`
  optimistic concurrency; SQL with a transaction. All conform.
- **Reads are polls, not subscriptions.** The SSE follower polls
  `read_events` on an interval. A `watch`/blocking-read capability (Redis
  `XREAD BLOCK`, a JetStream push consumer) is a latency optimization some
  backends offer cheaply and others don't — if it ever lands, it lands as a
  separate optional trait with a polling default, so a backend without cheap
  push still conforms. It is explicitly out of scope here.

Nothing Redis-shaped may leak upward: stream entry IDs, connection handles,
script SHAs, key names. The store returns `SessionEvent`s and `Lease`s, both
already defined in core.

## 2. Redis implementation

### Key layout

All keys for a session share a Redis Cluster hash tag so the multi-key lease
and append operations stay single-slot — cluster-safe from day one even though
the first client is single-node:

| Key | Type | Holds |
|---|---|---|
| `rh:{<session_id>}:meta` | string | JSON `{model_policy, created_at_ms}`, written with `SET NX` |
| `rh:{<session_id>}:lease` | string | holder's `node_id`, with `PX` expiry |
| `rh:{<session_id>}:log` | stream | one entry per event, explicit ID `<seq>-0` |

`create_session` is `SET NX` on `meta`; the reply distinguishes created from
already-existed. Session ids are minted as `sess_<uuid-simple>` so they are
key-safe; client-adopted ids are opaque bytes to Redis and only affect cluster
slot choice if they contain braces — worth a validation note, not a blocker.

### The event log is a stream with `entry ID == seq`

`XADD` with explicit IDs `<seq>-0` makes the stream ID and the event's `seq`
the same number. That one decision collapses most of the read surface:

- `read_events(after_seq, limit)` → `XRANGE log <after_seq+1>-0 + COUNT limit`
  — exact, no client-side filtering (IDs are always `X-0`, so the inclusive
  start at `after_seq+1` is precisely "seq > after_seq").
- `last_seq` → `XREVRANGE log + - COUNT 1`, empty stream reads as 0.
- Contiguity is enforced where the seq is assigned: inside the append script,
  next id = (max existing id) + 1, read with `XREVRANGE ... COUNT 1` under the
  script's atomicity. No separate counter key that could drift from the
  stream.

Each entry carries two fields: `at_ms` and `kind`, where `kind` is the
serde_json encoding of `SessionEventKind` — the same tagged representation the
core types already define. `seq` (from the entry ID), `session_id` (from the
key), and `at_ms` are recombined into `SessionEvent` on read. Lua never parses
or splices JSON; serialization stays entirely in Rust, so a schema change is a
core-crate concern and the scripts never move.

`read_events`/`last_seq` pipeline an `EXISTS meta` alongside the read to
distinguish "empty session" from `SessionNotFound`.

### Lease: a `SET PX` key, on the Redis clock

The lease is a string key whose value is the holder's `node_id` and whose
*expiry is enforced by Redis itself* via `PX`. This deliberately moves the
lease clock from process time (`now_ms()`, fine for the single-process
`MemoryStore`) to the store's clock: a multi-node deployment is exactly the
deployment where node clocks skew, and a lease whose validity two nodes
compute from two different clocks is a fencing hole. One clock authority, and
it's the one all writers already agree on by construction.

Four operations, each one Lua script executed via `redis::Script` (which
handles `EVALSHA`-with-`NOSCRIPT`-fallback):

- **acquire** — fail `SessionNotFound` if `meta` absent; if lease key absent
  or already ours, `SET PX ttl`; else return "held". Idempotent re-take by the
  holder, blocked for everyone else, and expiry-is-takeable comes free from
  `PX` — an expired key is an absent key.
- **renew** — if lease value == our `node_id`, `PEXPIRE`; else "lost", which
  the session layer treats as final.
- **release** — compare-and-`DEL` (delete only if still ours).
- **append** — the heart, and the reason these are scripts at all: *fence and
  append must be one atomic step*. Check `meta` exists → check lease value ==
  `node_id` → read max entry ID → `XADD` each event at `max+1, max+2, …` with
  `at_ms` from Redis `TIME` → return the assigned seqs and timestamp, from
  which the Rust side rebuilds the `SessionEvent`s without a re-read. Scripts
  replicate by effects (the default since Redis 5), so `TIME` inside the
  script is replication-safe.

Scripts return small status sentinels (`OK` / `NOSESSION` / `LEASELOST` /
`HELD`), mapped in Rust to `Ok`, `StoreError::SessionNotFound`,
`StoreError::LeaseLost`, and `Ok(None)` respectively. Transport and protocol
failures map to `StoreError::Backend`.

The returned `Lease.expires_at_ms` is computed from Redis `TIME + ttl` and is
informational: by the trait's own doc, validity is always decided against the
stored record, never against the handle's copy — the session layer already
depends on that.

### Client, config, durability

- **Client:** `redis` 0.27 `ConnectionManager` (already pinned in the
  workspace with `tokio-rustls-comp`, `streams`, `script`,
  `connection-manager` — the feature set was chosen for exactly this design).
  Multiplexed, auto-reconnecting; one manager per store, cheap to clone.
- **Config:** a `RedisStoreConfig { url, key_prefix }` built by the composition
  root from `ROUNDHOUSE_REDIS_URL`. The prefix defaults to `rh` and exists so
  two deployments can share an instance in a pinch — not for multi-tenancy
  claims.
- **Durability is a deployment fact, not a code path.** The log is as durable
  as the Redis it lives in (AOF `appendfsync`, replication). The crate docs
  state this the way `frontier.rs` states the rate-card rule; the code does
  not try to out-engineer the operator's persistence config.
- **Retention:** none in v1. The trait has no delete, sessions have no
  lifecycle end yet, and `XTRIM` behind the trait's back would break replay.
  When session deletion/archival becomes a trait concern, trimming becomes its
  implementation — noted under open questions.

## 3. Portability check: the same contract on NATS JetStream

Not to be built now — the point is to verify the trait doesn't secretly
require Redis. Mapping each obligation:

| Contract obligation | Redis mechanism | JetStream mechanism |
|---|---|---|
| Append-only per-session log | stream key per session | one stream, subject `rh.sess.<id>` per session (JetStream prefers subjects-in-a-stream over a stream per session) |
| Store-assigned contiguous `seq` | entry ID `<seq>-0` assigned in-script | seq carried in the message, enforced by publish with `Nats-Expected-Last-Subject-Sequence` CAS; retry on conflict |
| Fenced append, atomic with fencing check | Lua: check lease + `XADD` in one script | lease epoch in a KV bucket read-before-publish + the CAS above; a displaced writer's CAS fails |
| Lease with store-side expiry | `SET PX` / `PEXPIRE` | KV bucket entry with per-key TTL (NATS ≥ 2.11) or expiry timestamp in the value, updated with revision CAS |
| `read_events(after_seq)` poll | `XRANGE` | direct get by subject sequence / ordered pull consumer from a known position |
| `last_seq` | `XREVRANGE COUNT 1` | stream info, last sequence for subject |

Everything maps; where Redis gets atomicity from a script, JetStream gets it
from optimistic CAS. Both produce the contracted *outcome* (a fenced,
contiguous log), which is exactly why the contract must keep naming outcomes.
The one wrinkle worth recording: JetStream's own stream sequence is global to
the stream, so per-session `seq` must be data, not infrastructure — which the
trait already forces by defining `seq` on `SessionEvent` rather than exposing
any backend cursor type.

## 4. Test plan — the contract suite is the spec

Per `CLAUDE.md`, tests come first, and here the tests are the deliverable that
makes "flexible enough for another backend" checkable rather than asserted:

1. **Extract the contract suite.** The tests currently inside
   `roundhouse-core/src/store.rs` (`a_live_lease_blocks_another_node`,
   `an_expired_lease_is_takeable_and_the_loser_cannot_append`,
   `sequence_numbers_are_contiguous_and_replay_is_gapless`,
   `renew_fails_once_the_lease_was_taken_over`, create-idempotency) *are* the
   contract — today they can only judge `MemoryStore`. Move them into a
   `store::contract` module in `roundhouse-core` behind a `test-support`
   feature, generic over the store, plus the assertions the docs make but no
   test yet enforces: stale-handle-still-appends-while-record-lives (the
   heartbeat invariant), `renew → None` finality, `SessionNotFound` on unknown
   sessions, and batch appends assigning contiguous seqs in one call.
2. **Generalize the one test hook.** Expiry-without-waiting needs a store-side
   lever. `MemoryStore::expire_lease_now` becomes the memory impl of a small
   test-only `LeaseControl` trait in the contract module; the Redis impl is
   `DEL` on the lease key. This is the "make it testable" seam, additive and
   behavior-preserving.
3. **Run the suite against both stores.** For `MemoryStore` it runs always
   (proving the extraction changed nothing); for `RedisSessionStore` it runs
   in `crates/roundhouse-store-redis/tests/` against a real Redis named by
   `ROUNDHOUSE_TEST_REDIS_URL`, gated by `#[ignore = "needs a real Redis…"]`
   and opted into with `--include-ignored` — a suite that fails without
   infrastructure teaches people to ignore red. `#[ignore]` rather than an
   env-var check that returns early, because it is the one skip the harness
   *reports*: the early return would print "passed" for tests that verified
   nothing, and asking for the tests without the variable set fails loudly
   instead of skipping again. (M2 learned this the honest way — the
   eprintln-and-return version's notices were swallowed by libtest's output
   capture.)
4. **Redis-only adversarial tests**, beyond the shared suite: two nodes racing
   acquire-after-expiry with the loser's in-flight append rejected;
   interleaved append/renew from separate connections proving script
   atomicity; event round-trip fidelity across every `SessionEventKind`
   variant (serde through stream fields and back); recovery after a dropped
   connection mid-sequence (ConnectionManager reconnect, no gap, no repeat).

## 5. Wiring and delivery order

Each milestone lands failing-test-first, and each is a commit that leaves the
workspace green:

1. **M1 — contract suite extraction** (`roundhouse-core`): `test-support`
   feature, `store::contract` generic suite + `LeaseControl`, `MemoryStore`
   running under it. Pure refactor plus the new doc-promised assertions.
2. **M2 — read path on Redis**: `RedisSessionStore` with `create_session`,
   `read_events`, `last_seq`; key layout and event field round-trip landed and
   tested (appends via a test-only raw `XADD` until M3).
3. **M3 — lease + fenced append**: the four scripts, error mapping, the full
   contract suite green against Redis, the adversarial tests. This is the
   milestone where the crate's reason to exist is proven.
4. **M4 — composition root**: `main.rs` selects the store from
   `ROUNDHOUSE_REDIS_URL` (set → Redis, absent → `MemoryStore` demo, matching
   the existing "one environment variable" stance); `Engine` is already
   generic, so this is a two-arm match that monomorphizes twice. README and
   crate-table status updated from *(not yet implemented)*.

## 6. Open questions (decisions deliberately not made here)

- **Lease epochs.** Fencing is by `node_id`, in `MemoryStore` and in this
  plan alike. The ABA case — node A loses the session, B holds and releases,
  A re-acquires, and a *stale handle from A's first tenure* validates again —
  is inherited from the current contract, not introduced by Redis. The fix is
  a fencing token/epoch on `Lease`, which is a core-trait change touching the
  heartbeat contract, so it should be its own finding-shaped change with its
  own failing test, not a rider on this crate.
- **Blocking tail.** `XREAD BLOCK` (and a JetStream push consumer) could
  replace follower polling; needs the optional-capability shape sketched in §1
  and a latency number showing the poll interval actually costs something.
- **Session lifecycle.** Deletion, archival, and log trimming are one future
  design (a `delete_session` on the trait, `XTRIM`/`DEL` behind it), and
  retention policy is a deployment fact that belongs in config when it exists.
- **Redis Cluster.** The key layout is already slot-safe; adopting the cluster
  client is a config/client-type decision to make when a deployment needs it.
