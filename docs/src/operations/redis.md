# Deploy with Redis

This chapter explains how to run several Roundhouse nodes that share state through one Redis, what moves into Redis, and what stays in each process. For the design of the session log in Redis, see [Session store contract](../development/store-contract.md).

## Turn it on

One variable decides whether a deployment is shared. The rule has no other input.

| Variable | Effect |
|---|---|
| `ROUNDHOUSE_REDIS_URL` | A `redis://` URL. When it is set, every family in the next section lives in that Redis. When it is absent, every family lives in the memory of the process and ends with it |
| `ROUNDHOUSE_REDIS_NAMESPACE` | The deployment boundary that every shared key is built under. The default is `rh`. A value that is set but empty stops the boot |

The namespace rules:

- A namespace that is empty or blank is refused. A blank namespace would collide with the default namespace.
- A namespace that contains `{`, `}`, `:` or whitespace is refused. A brace would define a Redis Cluster hash tag. That tag would override the tag of every family and put every key of the deployment in one slot.
- Roundhouse reads the variable whether or not `ROUNDHOUSE_REDIS_URL` is set. A typo is caught at the boot that introduced it, and not on the day that you add a Redis.
- Two deployments on one Redis need two namespaces. A test proves that they cannot see each other's keys.

The requirements for the Redis server:

- **Version 6.2 or later.** `read_events` uses an exclusive `XRANGE` start, which needs 6.2.
- **One instance.** Roundhouse uses a single-node client. The key layout keeps one session in one Cluster slot, but some operations cannot work across slots. See [Durability, retention and scope](#durability-retention-and-scope).
- **Script support.** Every write that must be atomic is one Lua script. Scripts replicate by effects.

At boot, Roundhouse connects to every family before it serves a request. An unreachable URL stops the process. It never falls back to memory. A fallback would hand the budget back while the log that proves it was spent survives. The directory is the last check. Its first `load` must succeed, and a Redis that serves sessions but refuses the directory read stops the boot with `ROUNDHOUSE_REDIS_URL` named in the message.

Boot logs one line for the shared case. It names every family that is shared. It never prints the URL, because a URL can carry credentials.

## What moves into Redis

`shared_backend::open` builds every family in one match on the value of `ROUNDHOUSE_REDIS_URL`. All keys have the form `<namespace>:<version>:<family>:...`. Every family is version `v1`.

| Family | Key prefix | Holds |
|---|---|---|
| Sessions | `rh:v1:sess` | The session log, the lease, and the learning index. See [Session store contract](../development/store-contract.md) |
| Spend ledger | `rh:v1:spend` | Committed spend of each project and member, the holds of open grants, and session watermarks. A second ledger with its own keys counts the evaluation spend |
| Fair-use buckets | `rh:v1:fairuse` | The rolling usage counters of a project and of a member |
| Correlation maps | `rh:v1:corr` | Which session a client's name for a conversation belongs to. There is one map for generations, one for tool calls and one for threads |
| Admin directory | `rh:v1:dir` | The projects, members and keys that the admin plane created. One versioned document |
| Learner store | `rh:v1:learn` | The shared counters of the online routing learner. Roundhouse opens this family at boot, and only when a project runs the learner in `shadow` or `live` mode |

Without a Redis, the learner store is also in memory. It logs a warning that the learned state ends with the process. A second node then learns its own counts.

### What the sharing gives you

- Every node that serves a project shares one rolling fair-use ceiling.
- A cache key, a tool call or a client thread that one node bound resolves on every node. A name that no node ever bound is refused. It never resolves to a superseded log.
- A project, member or key that the admin plane created outlives the process and reaches every other node.
- A node that takes over a session finds its log and continues. See [Session store contract](../development/store-contract.md).

## What stays in each process

| State | Why it stays |
|---|---|
| The `latest` conversation of a principal | It is a guess. Two nodes that serve one agent would each write their own answer, and whichever wrote last would speak for both. A control call that lands on a node with no better correlation falls back to it, or is refused |
| The MCP `ControlStore` (overlays, intents, advisory outcomes and bindings) | It is lost on restart. That is acceptable. An overlay only narrows, so losing one widens the limit back to the deployment ceiling and never past it. A sweep drops records older than 24 hours |
| The turn gate of each session | One lock per session serializes the turns of one process. The fencing token of the lease is the only authority across nodes. The entries of the map are never removed |
| The metrics fold | Each node folds what it served, plus what it replayed from the sessions that it opened. To see a fleet, scrape each node |
| Projections of a session (state, cache ledger seed, SSE follower) | They rebuild from the log when a session opens |
| Warn-once guards | Each process says each caution once |

## Key rules

Every key comes from one function, `keys::build_key(namespace, family, parts)`. It joins the namespace, the version of the family, the family name and the parts with colons.

- The family is a closed set in the code. Each family has its own version. A change to the encoding of one family moves only that key space. It does not orphan every session log.
- A convention test scans every key function. It asserts that each one calls the builder and does not spell the version itself. A hand-formatted key that matched byte for byte would otherwise be invisible.
- The project or the session id is a hash tag in the keys of a family. This keeps the keys that one script touches in one slot.
- Roundhouse never migrates keys between versions. No deployment held keys from before the rule.

## Fair-use ledger

A project and a member each have one hash under `rh:v1:fairuse`. The project key is `rh:v1:fairuse:{<project_id>}:p`. The member key is `rh:v1:fairuse:{<project_id>}:m:<user_id>`. The project id is the hash tag of both, so one script touches one slot.

Each hash holds:

| Field | Holds |
|---|---|
| `b:<index>:t`, `b:<index>:u` | The tokens and the micro-dollars of one bucket. The index is `at_ms / BUCKET_MS`. A bucket is 5 minutes |
| `s:<window>:t`, `s:<window>:u` | The running sum of a window |
| `s:<window>:from`, `s:<window>:to` | The oldest and the newest bucket index that the sum covers |
| `mark` | The clock of the scope |

The two operations are one script each:

- `record_draw` reads, adds with a clamp, and writes the bucket and the sum of every window for both scopes. It contains no command that can fail.
- `would_exceed` runs per scope and per window, narrowest first. It decays the sum with one `HMGET` of the fields that aged out, and subtracts them with a floor at zero. It resets without a read only when `to` is older than the window. A sum at the domain ceiling is rebuilt from the buckets and not subtracted. It walks buckets only on a refusal, to compute the earliest retry time, in the same way as the memory ledger.

The decay of the widest window deletes the aged fields. This also runs in `record_draw`. Without it, a membership with a cap on 5 hours only would grow one field every 5 minutes forever. The hash has one `PEXPIRE` of the widest window plus one bucket. Every draw re-arms it, so an idle scope costs nothing. Bucket ranges go out in chunks of 400 fields, because Lua `unpack` is bounded.

`would_exceed` writes, so a read-only replica cannot serve it.

A layout with one key per scope per bucket was rejected. It summed every bucket in the window on each check. An admitted turn, which is the common case, lets no window bind. The check then reads every bucket of the widest window: 2017 `HMGET` calls for each capped scope under a seven-day cap. That took about 8 milliseconds of blocking Redis time per admission, ahead of every queued session-log append. The cost came from the number of commands and not from the number of bytes. The running sums move that cost to the write.

The running-sum layout issues six commands for an admitted turn on a membership with three windows, which is twice the number of windows. A clock that is ahead of the mark adds one `HSET` for each capped scope, once per run. The worst-case decay is one `HMGET` for each chunk of 400 fields, which is six for the seven-day case. A test pins the count through `INFO commandstats`. The hardware and the Redis version of the original measurement were not recorded.

The arithmetic is integer, in one bounded domain that both ledgers share. The memory ledger is the specification, and one contract suite runs against both.

### The warning for a per-process ledger

The fair-use caution comes from the memory ledger, not from the boot. `MemoryFairUseLedger` warns the first time it enforces a non-empty ceiling. The text says that two nodes enforce two independent ceilings and that every counter resets on restart. The warning happens once for each ledger, and not for each turn.

The ledger is the one place that knows both facts: a ceiling is enforced, and the counters are per process. A ceiling can reach the ledger from the file that the node booted from, or from a `PATCH` through the admin plane an hour later.

The choice of backend does not depend on whether a ceiling is configured. The admin plane can add a ceiling after boot. Choosing Redis only when a `fair_use` block exists at boot was rejected. A deployment that booted with Redis and no ceiling would then enforce every later ceiling in the memory of one node. Nothing would be in Redis, and no warning would appear. The argument for that choice was to spare a deployment without a ceiling a boot failure on an unreachable Redis. That argument fails, because the session store fails the boot on the same URL first.

A deployment with no Redis also gets one `warn!` at boot. This warning names every family. Sessions and committed spend die with the process. Admin-created tenancy dies with it. A fair-use ceiling is enforced per node. A control call that lands on a node that served none of its conversation falls back to a guess or is refused.

## Spend ledger

One project has four keys. The project id is the hash tag, which is why one script can read and debit a project ceiling and a member ceiling together. Two grants that race across two round trips could otherwise overspend.

| Key | Type | Holds |
|---|---|---|
| `rh:v1:spend:{<project_id>}:account` | hash | `committed` for the current window, `member:<user_id>` for each member, and `window_start_ms` |
| `rh:v1:spend:{<project_id>}:holds` | hash | `response_id` to a packed `user`, amount and `expires_at_ms`, one field for each live grant |
| `rh:v1:spend:{<project_id>}:watermarks` | hash | `session_id` to the highest settled `seq` |
| `rh:v1:spend:{<project_id>}:settled_calls` | set | The `response_id` of every evaluation call that has settled |

The ledger for evaluation spend uses the same four keys with an `eval` segment after the hash tag. This is a segment inside the family and not a second namespace. A namespace such as `tenant-eval`, derived from `tenant`, would be the same keys as a second deployment that is legitimately named `tenant-eval`. A classification is work of the deployment, not of the turn. The two ledgers therefore cannot exhaust each other.

Every method is one Lua script. A grant, a settle and a balance read each take one round trip. Dollar amounts cross as strings and not as Lua numbers. The `now_ms` value comes from the client and not from `TIME`. A test can then reach a time-to-live lapse and a monthly reset without sleeping.

`settled_calls` has no expiry and no compaction. Any expiry would be a window in which a duplicate charges a project twice. The cost is memory in Redis until a compaction protocol can prove that an identity will never settle again. The ledger keys use bare project ids, with no generation and no creation stamp.

## Correlation maps

The maps answer which session a client's own name for a conversation belongs to, from any node.

| Key | Type | Holds |
|---|---|---|
| `rh:v1:corr:gen:{<namespaced cache key>}` | string | The generation that a turn last committed to, in decimal |
| `rh:v1:corr:call:{<principal>}:<tool_use_id>` | string | `s:<session id>`, or the ambiguous marker |
| `rh:v1:corr:thread:{<principal>}:<thread_id>` | string | `s:<session id>` |

The design has these rules:

- **One key per binding, not one hash per principal.** A hash field cannot expire. The bound that these families need is a staleness bound. A hash would need a sweeper. A key per binding gives the bound to `PEXPIRE`, which the server enforces whether or not a node is running. The cost is that one command cannot read all bindings of a principal. No caller needs that.
- **The bounds.** A call binding expires after 6 hours. A thread binding expires after 7 days. A tool-use id is live for one leg of one tool loop. A client can resume a thread tomorrow. After seven days, a resumed thread has almost certainly compacted, which forks and rebinds the key anyway. The memory tables use the same constants.
- **Values are tagged.** A session id is an arbitrary string. A bare sentinel for "ambiguous" could be a session id that a client mints. A bound value carries the tag `s:`, and the ambiguous marker carries none. A value with neither shape came from a foreign writer and fails the read loudly.
- **Only the call binding is a script.** "This id is already held by a different session" must not be judged against a value that another node is replacing. Otherwise two sessions could both see an absent key and both write, which leaves one binding that is confidently wrong. The generation is a plain `SET`, because it is a hint about where a probe starts and not the answer. A thread binding is a plain `SET`, because the latest write is the answer.
- **An ambiguous call id is remembered.** A dropped id would read as never seen, and the next binding of the same id would start answering confidently again.

## Admin directory

The directory holds the projects, members and keys that the admin plane created. The Redis store sees it as one versioned, opaque document. The store knows nothing about projects or keys.

The contract lives in core. `DocumentStore` offers `load`, `commit(expected_version, document)` and `version`. `MemoryDocumentStore` is the specification. The server adapter serializes the records as a JSON envelope with `schema`, `records` and `compiled_under`. An unknown field in the envelope is tolerated. Each record keeps `deny_unknown_fields`, which stops a mistyped key in an operator's file from silently widening a policy. A document whose `schema` is newer than the build knows is refused at load, by name.

### Layout

| Key | Type | Holds |
|---|---|---|
| `rh:v1:dir:records` | hash | `version` (decimal), `lineage`, and `document` (bytes) |

The layout has three rules:

- **One key for the whole deployment.** The directory compiles a whole plane from what it reads. A half-read directory would compile a plane that admits and refuses the wrong keys. One key is one atomic read. A key per entity would make the read a scan, and its result is only as consistent as the interleaving that it saw.
- **No hash tag.** No operation touches several keys. A tag would pin the tenancy of the deployment to one slot and buy nothing.
- **Fields, not keys.** One `HSET` writes the version, the lineage and the document together. No reader can see a version that does not match the bytes beside it. Separate keys would need a multi-key script and a hash tag to get the atomicity that one hash already has.

`commit` is one Lua compare-and-set. `load` and `version` are each one `HMGET`, with no condition and no script.

A typed `DirectoryStore` with a Redis implementation in the server crate was rejected. It would duplicate the Redis key format in a third crate, or pull the configuration vocabulary into the storage crate. A serde change to a project entry would then change the API of the storage crate.

### Lineage and version

A document is identified by a lineage and a version. The version is a write counter and not a hash of the content. A commit of identical bytes still advances it. A caller compares versions to decide whether to recompile, and an idempotent-looking admin call must stay visible to other nodes.

The counter lives in the key. A key that an operator deleted with `DEL`, a flush or a restore from an older backup would restart at 1. That is a version that some node already serves. A check that compares numbers would not see it. The store therefore mints an opaque `lineage` the first time it writes the key. A key that was lost starts a new lineage.

- Within a lineage, versions strictly increase and are never reused.
- `commit` and `version` answer the pair. Only the commit can tell a writer which lineage it started.
- A reader that sees a lower version or a different lineage records a typed regression and warns once. It adopts the state of the store, because the store is the shared truth. It discards a late, older refresh.
- A node that boots against an empty Redis claims no lineage. Version zero supersedes it and regresses nothing.
- A stored version is 1 to 15 decimal digits and never zero. Any other shape of the key fails both `load` and `commit`. Lua `tonumber` accepts hex, exponents and whitespace that Rust `parse::<u64>` refuses. Both paths enforce the same grammar, so a key cannot be corrupt to one and silently taken over by the other.

### Size and time bounds

- A document is capped at `DIRECTORY_DOCUMENT_CEILING_BYTES`, which is 8 MiB. The server refuses a larger write before any wire call, with HTTP 413 and its own error code. The size fits a few thousand keys at about 330 bytes for each key record.
- The directory family connects with a 5-second response timeout and not the shared 300 ms. The client wraps the whole transfer in one timeout. A measurement over loopback to a real Redis 7.x showed that 300 ms times out a document around 50 MiB. A 5-second timeout carries 32 MiB. A 64 MiB document still times out at 300 ms. The 8 MiB ceiling is what a change trips, and the timeout is margin. The hardware was not recorded.

### Nodes that disagree

A shared directory lets one node compile a plane from inputs that another node did not have. Examples are the per-process control-plane file, the catalog, the fleet that the cross-checks use, and the time-to-live. Every commit stamps `CompiledUnder`. It holds these inputs:

- A SHA-256 of the bytes of the control-plane file.
- The catalog identities.
- The routing candidates that the cross-checks used.
- The time-to-live.
- The learner artifact digests of each project.

A reader whose own fingerprint differs names the difference once for each stored version. The difference is over the file, the catalog, the fleet or the time-to-live. The reader keeps serving the plane that its own inputs compile. Refusing was rejected. A node that stopped authenticating because a neighbor was one configuration ahead would turn every rolling configuration change into an outage.

A mutation that one node validated can fail to compile on another. That node warns once for each time-to-live and keeps serving the old plane. The warning is keyed on the version and not on the fingerprint. A refused version is otherwise reloaded every time-to-live forever. `ControlDirectory::status()` returns the served version, the refused version, the divergence and a count. It is read-only and never refreshes.

### Why the directory is shared

An archived project keeps its id. Re-creating a project under the id of a closed project would join the spend histories of two tenants under one name. The ledger keys use bare ids, with no creation stamp.

With a per-process directory, a restart loses the record of the archived project while a Redis ledger survives. The id can then be created again, and the new project silently inherits the old spend. The shared directory closes this gap. The test `recreating_an_archived_project_after_a_restart_inherits_its_spend` covers it, and a version against a real Redis exists. A deployment with no Redis keeps the gap, and its boot warning says so.

## Outages

Roundhouse treats the two directions of a store outage differently:

- **A ceiling check that cannot reach its store fails closed.** The turn gets a retryable 503. On the Messages surface the error type is `overloaded_error`. A ceiling that cannot be checked cannot be honored, and the operator set it on purpose.
- **A draw that cannot be recorded fails open.** The reason is logged. A bounded under-count is a fact about the outage. A wrong refusal is not.

A blanket fail-open was rejected for a ledger. It is right for a response cache. Degradation is recorded as a typed reason and never swallowed.

The server warns once for each outage, with the error text of the store. It logs an info line on recovery. The client body carries a fixed message and the Roundhouse code. It never carries the error text of the store.

One function builds the connection for the session store, the spend ledger, the fair-use ledger, the correlation maps and the learner store. It sets these bounds:

| Setting | Value |
|---|---|
| Connection timeout | 300 ms |
| Response timeout | 300 ms |
| Reconnect retries | 3 |
| Reconnect delay | 50 ms, growing by a factor of 2, capped at 300 ms |

The directory family is the exception. It uses a 5-second response timeout.

The bounds make a check against a severed store refuse within two seconds. The default of the `redis` crate is six retries with its own delays. With that default, every admission after the first waited about 9.5 seconds for its 503 while the shared reconnect future ran. The test `a_ceiling_that_cannot_be_checked_refuses_within_a_bounded_time` checks the new bound against a real severed connection. The hardware and the Redis version of the 9.5-second measurement were not recorded.

## Durability, retention and scope

- **Durability is a deployment fact.** The log is as durable as the Redis that holds it. The `appendfsync` setting of the AOF and replication decide. Roundhouse does not try to do better.
- **There is no retention.** Sessions and their logs grow without bound. Trimming a stream behind the trait would break replay.
- **Only a single Redis instance is supported.** An append with a learning mark touches six keys in several slots. The learner store has no test against a Cluster.
- **Some state has no compaction.** These structures keep one entry for each session or call forever:
  - The permanent learning index.
  - The watermark hash and the `seen` sets of the learner store.
  - The `settled_calls` set.

## Redis that NeMo Relay uses

Roundhouse does not manage this Redis. The notes come from reading the source of Relay 0.8.2. No run confirmed them.

- The response cache of Relay fails open with a 2-second deadline. A store error becomes a cache miss with the typed reason `StoreError`.
- The shared cache namespace is declared and not derived. An empty namespace is rejected when the section is enabled.
- A `CACHE_SCHEMA_VERSION` is part of every key. A change of shape makes old entries unreachable, and does not misread them.
- The adaptive Redis backend sets no time-to-live and trims no list. With ACG and Redis, up to 100 raw `PromptIR` snapshots for each profile are stored with no expiry. The source does not show whether this retention is intended.
