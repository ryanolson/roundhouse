# Deploy with Redis

This chapter explains how several Roundhouse nodes share state through one Redis: what moves into Redis, what stays in each process, and how each family lays out its keys. The session log's design is in [Session store contract](../development/store-contract.md).

## Turn it on

| Variable | Effect |
|---|---|
| `ROUNDHOUSE_REDIS_URL` | A `redis://` URL. When set, every family in [What moves into Redis](#what-moves-into-redis) lives in that Redis. When absent, every family lives in process memory and ends with the process |
| `ROUNDHOUSE_REDIS_NAMESPACE` | The prefix of every shared key. Default `rh`. A value that is set but empty stops the boot |

The namespace rules (`KeyNamespace::new`, `crates/roundhouse-store-redis/src/keys.rs`):

- An empty or blank namespace is refused. Reading it as "no namespace" would reuse the keys of an unnamespaced deployment.
- A namespace that contains `{`, `}`, `:` or whitespace is refused. A brace would define a Cluster hash tag that overrides every family's tag and puts the whole deployment in one slot.
- The variable is read whether or not `ROUNDHOUSE_REDIS_URL` is set, so a typo fails the boot that introduced it.
- Two deployments on one Redis need two namespaces. A test proves they cannot see each other's keys.

The Redis server needs:

- **Version 6.2 or later**, for the exclusive `XRANGE` start that `read_events` uses.
- **A single instance.** See [Durability, retention and scope](#durability-retention-and-scope).
- **Lua scripts.** Every write that must be atomic is one script. Scripts replicate by effects.

At boot, `shared_backend::open` connects every family before the server serves a request, and the directory's first `load` must succeed. An unreachable Redis stops the process with `ROUNDHOUSE_REDIS_URL` named in the message. It never falls back to memory: a fallback would hand the budget back while the log that proves it was spent survives.

The shared case logs one line naming the shared families, never the URL, because a URL can carry credentials. A deployment with no Redis logs one warning naming what dies with the process.

## What moves into Redis

`shared_backend::open` builds every family in one match on `ROUNDHOUSE_REDIS_URL`. Keys have the form `<namespace>:<version>:<family>:...`, and every family is at `v1`.

| Family | Key prefix | Holds |
|---|---|---|
| Sessions | `rh:v1:sess` | The session log, the lease and the learning index. See [Session store contract](../development/store-contract.md#the-redis-backend) |
| Spend ledger | `rh:v1:spend` | Committed spend per project and member, the holds of open grants, and session watermarks. A second ledger with its own keys counts evaluation spend |
| Fair-use buckets | `rh:v1:fairuse` | The rolling usage counters of a project and a member |
| Correlation maps | `rh:v1:corr` | Which session a client's name for a conversation belongs to |
| Admin directory | `rh:v1:dir` | The projects, members and keys that the admin plane created, as one versioned document |
| Learner store | `rh:v1:learn` | The counters of the routing learner. Opened at boot only when a project runs the learner in `shadow` or `live` |

So with Redis, nodes share one fair-use ceiling per project, resolve each other's conversation bindings, see the same admin-created tenancy, and take over each other's sessions. Without Redis, the learner store is also in memory, and the process warns that learned state ends with it.

## What stays in each process

| State | Why it stays |
|---|---|
| The `latest` conversation of a principal | It is a guess. Shared, the last node to write it would speak for every node |
| The MCP `ControlStore` (overlays, intents, outcomes, bindings) | An overlay only narrows, so losing one on restart widens back to the deployment ceiling, never past it. Records older than 24 hours are swept |
| The turn gate of each session | It serializes turns in one process. Across nodes, the lease's fencing token is the only authority. Entries are never removed |
| The metrics fold | Each node folds what it served and replayed. Scrape each node to see a fleet |
| Session projections (state, cache-ledger seed, SSE follower) | They rebuild from the log when a session opens |
| Warn-once guards | Each process gives each caution once |

## Key rules

Every key comes from one function, `keys::build_key(namespace, family, parts)`, which joins the namespace, the family's version, the family name and the parts with colons.

- The family is a closed enum, and each family has its own version. A change to one family's encoding moves only that key space and does not orphan the session logs.
- A convention test asserts that every key function calls the builder and does not spell the version itself. A hand-formatted key that matched byte for byte would otherwise be invisible.
- Keys that one script touches together carry the same hash tag (the project or the session id), so they share a slot. The directory key and the learning index keys carry none.
- Keys are never migrated between versions.

## Fair-use ledger

A project and a member each have one hash: `rh:v1:fairuse:{<project_id>}:p` and `rh:v1:fairuse:{<project_id>}:m:<user_id>`. The project id is the hash tag of both, so one script reads both scopes.

| Field | Holds |
|---|---|
| `b:<index>:t`, `b:<index>:u` | Tokens and micro-dollars of one bucket. The index is `at_ms / BUCKET_MS`; a bucket is 5 minutes |
| `s:<window>:t`, `s:<window>:u` | The running sum of a window (`5h`, `24h`, `7d`) |
| `s:<window>:from`, `s:<window>:to` | The oldest and newest bucket index that the sum covers |
| `mark` | The scope's clock: the newest time any call has handed it |

Each operation is one script:

- `record_draw` adds, with a clamp, to the bucket and every window sum of both scopes, with no command that can fail. It re-arms a `PEXPIRE` of the widest window plus one bucket, so an idle scope costs nothing.
- `would_exceed` checks each scope and window, narrowest first. It decays a sum by one `HMGET` of the aged-out buckets, subtracted with a floor at zero. When `to` is older than the window, it drops the sum with no read. A sum at the domain ceiling is rebuilt from its buckets. It walks buckets only on a refusal, to compute the retry time. It writes, so a read-only replica cannot serve it.

The widest window's decay, which `record_draw` also runs, deletes aged bucket fields; without it, a `5h`-only membership would grow one field every 5 minutes forever. Bucket reads go out in chunks of 400 buckets (800 fields), because Lua `unpack` is bounded.

**Rejected: one key per scope per bucket.** An admitted turn, where no window binds, then read every bucket of the widest window. Under a seven-day cap that is 2017 `HMGET`s per capped scope, about 8 ms of blocking Redis time per admission, ahead of every queued log append. The cost was per command, not per byte. Running sums move it to the write. An admitted turn issues one `HMGET` per scope and window (six for two scopes and three windows), plus one `HSET` when its clock advances the mark. The worst-case seven-day decay is six chunk reads plus the sum read. `INFO commandstats` pins these counts. The hardware and Redis version of the 8 ms measurement were not recorded.

The integer domain, the caller clock and the high-water mark are in [Control plane](../concepts/control-plane.md#where-the-counters-live). The memory ledger is the specification, and one contract suite runs against both.

Without Redis, `MemoryFairUseLedger` warns once, the first time it enforces a non-empty ceiling, that each node enforces its own ceiling and resets on restart. It warns at enforcement and not at boot, because the ledger is the one place that knows both facts, and a ceiling can arrive later through the admin plane.

**Rejected: choosing Redis only when a `fair_use` block exists at boot.** A deployment that booted with Redis and no ceiling would enforce every later ceiling in one node's memory, with no warning. The argument for it, sparing such a deployment a boot failure on an unreachable Redis, fails: the session store fails the boot on the same URL.

## Spend ledger

One project has four keys with the project id as hash tag, so one script can read and debit a project ceiling and a member ceiling together. Two grants racing across two round trips could otherwise overspend.

| Key | Type | Holds |
|---|---|---|
| `rh:v1:spend:{<project_id>}:account` | hash | `committed` for the current window, `member:<user_id>` per member, `window_start_ms` |
| `rh:v1:spend:{<project_id>}:holds` | hash | `response_id` to a packed user, amount and `expires_at_ms`, one field per live grant |
| `rh:v1:spend:{<project_id>}:watermarks` | hash | `session_id` to the highest settled `seq` |
| `rh:v1:spend:{<project_id>}:settled_calls` | set | The `response_id` of every settled evaluation call |

The evaluation ledger uses the same four keys with an `eval` segment after the hash tag. **Rejected: a derived namespace such as `tenant-eval`.** It would be the same keys as a second deployment legitimately named `tenant-eval`. The two ledgers are separate so that neither can exhaust the other.

Every method is one script, so a grant, a settle and a balance read each take one round trip. Dollar amounts cross as strings, not Lua numbers. `now_ms` comes from the client, not from `TIME`, so a test can reach a time-to-live lapse and a monthly reset without sleeping.

`settled_calls` has no expiry and no compaction. Any expiry would open a window in which a duplicate charges a project twice. The ledger keys use bare project ids, with no creation stamp.

## Correlation maps

| Key | Type | Holds | Expiry |
|---|---|---|---|
| `rh:v1:corr:gen:{<namespaced cache key>}` | string | The generation that a turn last committed to, in decimal | None |
| `rh:v1:corr:call:{<principal>}:<tool_use_id>` | string | `s:<session id>`, or the ambiguous marker | 6 hours |
| `rh:v1:corr:thread:{<principal>}:<thread_id>` | string | `s:<session id>` | 7 days |

- **One key per binding, not one hash per principal.** A hash field cannot expire, so a hash would need a sweeper; `PEXPIRE` enforces the bound whether or not any node runs. No caller reads all bindings of a principal.
- **The bounds.** A tool-use id lives for one leg of one tool loop. A thread can resume tomorrow, but after seven days it has almost certainly compacted, which forks and rebinds anyway. The memory tables use the same constants.
- **The generation never expires.** An expired generation would reset the fork counter and point a live conversation at a log it forked away from.
- **Values are tagged.** A session id is client-spelled text, so a bare "ambiguous" sentinel could be a real id. A value with neither shape came from a foreign writer and fails the read.
- **Only the call binding is a script.** "Held by another session" must not be judged against a value another node is replacing, or two sessions could both see an absent key and leave a confidently wrong binding. The generation and thread bindings are plain `SET`s: the latest write is the answer.

The rules for ambiguity, rebinding and failure are in [Sessions and the event log](../concepts/sessions.md#correlation-maps).

## Admin directory

The Redis store sees the directory as one opaque, versioned document through core's `DocumentStore` (`load`, `commit(expected_version, document)`, `version`), with `MemoryDocumentStore` as the specification. The server adapter writes a JSON envelope with `schema`, `records` and `compiled_under`. The envelope tolerates unknown fields, but each record keeps `deny_unknown_fields`, so a mistyped key cannot widen a policy. A `schema` newer than the build is refused at load.

| Key | Type | Holds |
|---|---|---|
| `rh:v1:dir:records` | hash | `version` (decimal), `lineage`, and `document` (bytes) |

- **One key for the deployment.** A plane compiles from the whole directory, and a half-read one would admit and refuse the wrong keys. A key per entity would make the read a scan. No operation touches two keys, so there is no hash tag.
- **Fields, not keys.** One `HSET` writes version, lineage and document together, so no reader sees a version that does not match its bytes.
- `commit` is one Lua compare-and-set. `load` and `version` are each one `HMGET`.

**Rejected: a typed `DirectoryStore` with its Redis implementation in the server crate.** It would copy the key format into a third crate, or pull the configuration vocabulary into the storage crate. A serde change to a project would then change the storage API.

### Lineage and version

The version is a write counter, not a content hash. A commit of identical bytes still advances it, because nodes compare versions to decide whether to recompile.

A key deleted by `DEL`, a flush or a restore from an old backup would restart the counter at 1, a version some node already serves. The store therefore mints an opaque `lineage` the first time it writes the key, and a lost key starts a new lineage.

- Within a lineage, versions strictly increase and are never reused.
- A reader that sees a lower version or a different lineage records a typed regression, warns once, adopts the store's state, and discards a late older refresh.
- A node that boots against an empty Redis claims no lineage.
- A stored version is 1 to 15 decimal digits, never zero. Any other shape fails both `load` and `commit`. Lua `tonumber` accepts hex, exponents and whitespace that Rust `parse::<u64>` refuses, so both paths enforce one grammar.

### Size and time bounds

- A document is capped at `DIRECTORY_DOCUMENT_CEILING_BYTES`, 8 MiB. The server refuses a larger write with HTTP 413 before any wire call. A key record is about 330 bytes.
- The `dir` family uses a 5-second response timeout, not the shared 300 ms, because the client times the whole transfer. Over loopback to Redis 7.x, 300 ms timed out around 50 MiB and 5 seconds carried 32 MiB. The hardware was not recorded.

Each commit stamps `compiled_under`, the writer's input fingerprint, and a node whose fingerprint differs warns and keeps serving. The directory shares the ledger's switch so that a restart cannot re-create an archived project's id over its surviving spend (`recreating_an_archived_project_after_a_restart_inherits_its_spend`). Both are in [Control plane](../concepts/control-plane.md#divergent-nodes-and-durability).

## Learner store

`RedisLearnerStore` (`crates/roundhouse-store-redis/src/learn.rs`) holds the learner's counters. Every key carries a `{project}` hash tag.

| Key | Type | Holds |
|---|---|---|
| `rh:v1:learn:{<project>}:wm` | hash | Session id to watermark |
| `rh:v1:learn:{<project>}:<epoch>:q:<level>:<key>` | hash | `<strategy>:pos`, `<strategy>:n`, `<strategy>:sessions`, `jev_capable`, `jev_efficient` |
| `rh:v1:learn:{<project>}:<epoch>:ops` | hash | `<target>:lat_sum`, `lat_n`, `failover`, `cache_pred`, `cache_obs`, `cache_n`, and `turn:pre_sum`, `turn:pre_n` |
| `rh:v1:learn:{<project>}:<epoch>:seen:<session>` | set | The `<level>:<key>:<strategy>` members that the session counted in `sessions` |

- `read` is one read-only script over a turn's three quality keys and the ops key; `apply` is one script over one project's keys.
- Rust passes every key through `KEYS` and every field through `ARGV`, so `build_key` is the only spelling of a key. A convention test checks both halves.
- The scripts use only `TYPE`, `HGET`, `HMGET`, `HSET`, `SISMEMBER` and `SADD`, and reply with integer arrays. No counter passes through Lua `tostring` or `..`, which format with `%.14g` and drop digits past the 14th.
- Stored counters parse strictly (decimal digits, `-` only for signed fields); plain `tonumber` would also read `0x10`, `1e3` and ` 5`. Writes go out in chunks of at most 1000 values, because Lua `unpack` fails past about 8000 results.

The semantics of `read` and `apply` are in [The routing learner](../concepts/routing-learner.md#delivering-learning-to-the-learner-store).

## Outages

The two directions of a store outage are treated differently:

- **A ceiling check that cannot reach its store fails closed.** The turn gets a retryable 503 `fair_use_unavailable` (`overloaded_error` on the Messages surface). A ceiling that cannot be checked cannot be honored, and the operator set it on purpose.
- **A draw that cannot be recorded fails open** and logs the reason. A bounded under-count is a fact about the outage; a wrong refusal is not.

**Rejected: a blanket fail-open.** It is right for a response cache, not for a ledger.

The server warns once per outage with the store's error text, and logs an info line on recovery. The client body carries a fixed message and the Roundhouse code, never the store's error text.

One function (`connect_manager`) builds the connection for every family but `dir`:

| Setting | Value |
|---|---|
| Connection timeout | 300 ms |
| Response timeout | 300 ms (5 s for `dir`) |
| Reconnect retries | 3 |
| Reconnect delay | 50 ms, growing by a factor of 2, capped at 300 ms |

These bounds make a check against a severed store refuse within two seconds (`a_ceiling_that_cannot_be_checked_refuses_within_a_bounded_time`, against a real severed connection). With the `redis` crate's default of six retries, every admission after the first waited about 9.45 seconds for its 503 on the shared reconnect. The hardware and Redis version of that measurement were not recorded.

## Durability, retention and scope

- **Durability is a deployment fact.** The log is as durable as the Redis that holds it: the AOF `appendfsync` setting and replication decide.
- **There is no retention.** The trait has no delete, and sessions and their logs grow without bound. Trimming a stream behind the trait would break replay.
- **A single Redis instance only.** The client is single-node. A session's keys share a slot, but an append with a learning mark touches six keys across slots, and no learner-store test runs against a Cluster.
- **No compaction** for the permanent learning index, the learner store's watermark hash and `seen` sets, the spend `watermarks` hash, or `settled_calls`. Each keeps one entry per session or call forever.

## Redis that NeMo Relay uses

Roundhouse does not manage this Redis. These notes come from reading the Relay 0.8.2 source; no run confirmed them.

- Relay's response cache fails open with a 2-second deadline. A store error becomes a cache miss with the reason `StoreError`.
- The shared cache namespace is declared, not derived. An empty namespace is rejected when the section is enabled.
- Every key includes `CACHE_SCHEMA_VERSION`, so a shape change makes old entries unreachable instead of misread.
- The adaptive Redis backend sets no time-to-live and trims no list. With ACG and Redis, up to 100 raw `PromptIR` snapshots per profile are stored with no expiry. The source does not show whether that is intended.
