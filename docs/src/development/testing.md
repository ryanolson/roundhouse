# Testing

This chapter shows how to build and run the test suite, which test families need something extra, and which invariants the suite proves. It also records the rules for writing a test that is worth keeping.

## Build and test

The build needs the Rust toolchain that `rust-toolchain.toml` pins (1.96.1). It also needs the system package `libzmq3-dev`. The package comes in through `dynamo-kv-router/standalone-selection`.

```bash
apt-get install -y libzmq3-dev   # or: brew install zeromq
timeout 900 cargo test --workspace
```

The first build clones `ai-dynamo/dynamo` to resolve the pinned Dynamo crates. This takes a while. Later builds reuse the cached checkout.

The default run needs no GPU, no worker process and no network. The selection plane runs inside the test binary.

### Bounded runs

Always run the tests under a coreutils `timeout`. Use `timeout 900` for the full workspace and about `timeout 300` for a targeted run.

The reason is not slow tests. A hung test hangs the whole cargo run, silently. The runs most likely to hang a test are the adversarial reviews that mutate timeout and deadline code on purpose. When a timeout path breaks, its guard does not go red. It waits forever. A bounded run turns "stalled for hours" into `exit 124` in minutes.

If a run exits with 124, suspect the newest test or the mutation that you just applied. Run the suspect test binary again with `--test-threads=1 --nocapture` under a short timeout to find the hang.

The repository enforces this with a pre-tool hook, `.claude/hooks/cargo-test-timeout.sh`.

### File descriptor limit

On the development machine, the soft limit of 1024 open files made the `embedded_selection` test binary fail with `Too many open files`. After `ulimit -Sn 65536`, that binary passed all 7 tests, and the full workspace run passed. If you see that error, raise the limit before the run.

## Test families that you opt into

Each family below reaches something that the default run must not assume.

### Real Redis

Every test that needs a real Redis is marked `#[ignore]`. This group holds these tests:

- The contract suites of the session store, the spend ledger, the fair-use ledger, the correlation maps, the admin directory and the learner store.
- The boot suites.

Set `ROUNDHOUSE_TEST_REDIS_URL` to a reachable Redis and pass `--include-ignored`.

After you opt in, an unreachable URL makes the test panic. It does not skip. A silent skip is how a backend suite stops running without anyone noticing.

The gate is `#[ignore]` and not an early return on a missing variable. An early return prints "passed" for a test that verified nothing. `#[ignore]` is the one skip that the harness reports. Asking for the tests without the variable fails loudly.

Rules for a real-Redis test:

- **Contract suites run over both stores.** A macro runs each contract suite over the memory implementation and the Redis implementation. The memory side is the specification. See [Session store contract](store-contract.md#the-contract-suite).
- **Use a fresh namespace.** Each suite test connects under its own fresh namespace, so a run does not lengthen the permanent index that the next run reads.
- **Prove time without sleeping where possible.** On the memory side, a scripted clock proves staleness. On the Redis side, expiry follows the wall clock of the server, so a test waits about 250 ms over a shortened time-to-live. Forcing expiry would test the seam and not the server. A test checks a production time-to-live by reading `PTTL`, and it compares the Redis default against the core constant. A re-exported alias is only a promise until something compares it.
- **Keep one `INFO commandstats` loop per test binary.** That counter is server-wide. A lock serializes the measuring test against every other real-Redis test in the binary. Without the lock, the neighbours of the test landed reads inside the measurement: nine and eleven where the script issues seven.
- **Run tests that change `maxmemory` alone.** Two tests force an out-of-memory state by changing the global `maxmemory` of the shared Redis. One test can force the state while the other retries. Run the binary with `--test-threads=1`. These tests also need `redis-cli` on `PATH`.
- **The memory document store cannot race.** `MemoryDocumentStore::commit` has no await. The racing test of the contract therefore races only the Redis instance. 64 threads synchronized by a barrier, over 50 rounds, never landed in a split-lock window.

The outage test `a_ceiling_that_cannot_be_checked_refuses_within_a_bounded_time` runs against a real severed connection. See [Deploy with Redis](../operations/redis.md#outages).

### The real `codex` binary

The feature `e2e-codex` compiles `tests/codex_e2e.rs` at all. The flag `--include-ignored` opts in to spawning processes. See [Hook up Codex](../guides/codex.md) for the command.

The feature is off by default. The crate dev-dependency does not turn it on. A developer with no `codex` on `PATH` gets an empty test binary and not a failure to explain. `ROUNDHOUSE_TEST_CODEX_BIN` overrides the `PATH` lookup.

### The real `claude` binary

The feature `e2e-claude` works the same way. The chained tests also need `ROUNDHOUSE_TEST_RELAY_BIN`. The two closure tests need a built launcher:

```bash
cargo build -p topham
ROUNDHOUSE_TEST_TOPHAM_BIN=$PWD/target/debug/topham
```

The variable `ROUNDHOUSE_TEST_TOPHAM_BIN` has no `PATH` fallback. The `topham` binary is installed nowhere, so a bare name would resolve to whatever a developer happened to have.

CAUTION: Clearing the environment and `CLAUDE_CONFIG_DIR` does not isolate a real client. One `claude config list` under `env -i`, with no base URL override, still reached a real model through credentials that live outside both. Point every rig at loopback through `ANTHROPIC_BASE_URL`. Treat any run that nothing logged as a run that went to production.

Claude Code Remote changes the rig. Inside a Claude Code Remote container, `CLAUDE_CODE_REMOTE=true` is ambient. No API key source then suppresses OAuth. At version 2.1.257, a rig that only unset some variables sent a managed OAuth bearer token. It sent the betas `oauth-2025-04-20` and `extended-cache-ttl-2025-04-11`, the headers `x-claude-remote-container-id` and `x-claude-remote-session-id`, and a `GET /v1/code/agent-proxy/ca-cert` probe before every turn. A rig with a fully cleared environment sent `x-api-key` and none of these. That rig keeps only `HOME`, `CLAUDE_CONFIG_DIR`, `PATH`, `DISABLE_AUTOUPDATER=1`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`.

### Cache probe

The cache probe has an offline suite:

```bash
timeout 300 cargo test -p roundhouse-server --test cache_probe
```

The feature `e2e-frontier` adds a live cache probe. It sends two turns to one configured Messages target. The second turn appends 20 items. The report keeps the cache reads, the cache writes and the usage provenance of each turn separate. The offline tests cover the shared driver, the request markers, the configured transport and the budget refusal. They do not establish how a provider caches.

To run the live probe, supply three things: a catalog, a pinned `provider/model`, and a USD spend cap. The catalog must contain real prices and a deterministic cache model. Inject the key through `openv` into the variable that the `auth.env` of the provider names. Before the run, check the minimum cacheable prefix of the pinned model. The fixture holds at least 8,192 words before its marker. That is not a provider token count.

```bash
openv env ROUNDHOUSE_PROBE_CATALOG=/path/to/catalog.json \
  ROUNDHOUSE_PROBE_MODEL='anthropic/<pinned-model-id>' \
  ROUNDHOUSE_PROBE_LIMIT_USD='<approved-cap>' \
  timeout 300 cargo test -p roundhouse-server --features e2e-frontier \
    --test cache_probe live -- --nocapture
```

The live test has no `#[ignore]`. Enabling the feature puts it in an unfiltered test run. Missing configuration fails before dispatch. The project budget governs both turns, with 16 output tokens and a 30-second deadline per turn. A cache read of zero stays a reported observation for investigation.

## Validating a claim

A claim about behavior is an opinion until a failing test confirms it. This applies to review findings, bug reports and every "I think X is broken". The order is: write a test that fails for the stated reason, rule on the claim, then fix it.

- If the test passes, the claim is wrong, or the test does not exercise what the claim is about.
- If nothing can test the claim, change the seam. Add the accessor, extract the pure function, or split the type. Keep that change additive and behavior-preserving.
- A ruling is valid, partially valid or invalid. "Partially valid" means that the defect is real and the stated mechanism is wrong. Fix the real mechanism. A fix for the stated mechanism leaves the defect in place behind a passing test.
- When evidence lands before the fix, mark the failing assertion `#[ignore = "<finding>: <why it fails>"]` and keep the passing control tests live. An ignored test enforces nothing. Removing the `ignore` is the first step of the fix.

## What the tests establish

The suite proves these invariants. Names in backticks are test functions that you can run by name.

### Hashing, pricing and routing

- **Incremental hashing is exact.** It matches a full recompute across unaligned appends. A shared prefix yields an identical sequence-hash chain.
- **Client bytes stay flat while context grows.** Over 20 turns, the client bytes of a turn vary by 2 or less while the server-side context grows more than 10 times.
- **Pricing books no load.** Two `select` calls yield distinct pending selections and zero booked prefill.
- **A quote can be abandoned for free.** The fleet prices identically afterwards.
- **Routing reacts to cache state.** A warmed target is priced below its prompt length on the following turn.
- **Local cache hits are measured, not modelled.** A real mock vLLM engine (`dynamo-mocker`) runs the turns and publishes KV events over ZMQ in the engine-native wire format. The embedded selection service indexes them. A repeat turn under the real TinyLlama BPE prices at about 2% of its prompt length (32 of 1536 tokens). Every complete block of the prior context matches.
- **What was hashed is what is dispatched.** Local execution consumes the ids of the token buffer verbatim. The canonical rendering tokenizes back to the buffer exactly. A captured dispatch is byte-identical to the quoted stream.
- **A policy knob visibly changes routing.** A quality floor excludes a target that the default policy would pick. A filtered target never appears in `considered`. Savings are never priced against a model that the key could not reach.
- **A dead provider costs one attempt, not the turn.** A transport failure moves to the next candidate of the same tier inside one turn, one deadline and one grant that settles once. A cost-guarded turn is the exception. It tries the capable members that are cheaper than the efficient head, then the efficient tier, then the rest of the capable tier. The failover crosses transports as well as targets. A refusal, a 401 and a 404 fail where they stand. An exhausted tier fails with every attempt on the record, including the terminal failure. The promise to degrade to local survives any recipe.
- **The tier of a turn is read from the session, not asserted.** A stalling session is driven through the real signal extractor as items, and it lands on the capable tier. A session that produced work and passed its tests comes back down. A quiet session falls open. The control calls of roundhouse count toward none of this. Each case has a control that varies only the exchanges.

### Turns, the log and failover

- **Replays are redeliveries.** A retried turn returns byte-identical text and the original usage. It does not return a re-rendering with zeroed accounting.
- **A retry does not regenerate.** A re-sent `turn_id` replays the existing response and does not open a second turn.
- **A failed turn settles.** The response ends with an incomplete event. The lease comes back at once. The same turn id can be retried without waiting for a time-to-live.
- **Streaming is genuine.** Deltas are durable before the response completes. A stream that breaks halfway commits its partial output, which the ledger reads as prefill evidence. The metrics fold measures the interval from the turn start to the first nonempty text delta.
- **A turn outlives its lease.** The heartbeat renews the lease while the turn works. A model call that is longer than the time-to-live commits and is not fenced at its own finish line. A displaced owner still loses. A hung provider settles at the turn deadline and does not renew forever.
- **The log is the streaming bus.** The SSE transport tails the same event log that serves replay and audit. Live frames, reconnect frames and log entries are one thing. Resumption from `starting_after` or `Last-Event-ID` is exact. A deduplicated retry replays the entries of the original response and ends on its terminal event.
- **Reservations settle.** Load returns to zero. A `selection_id` that was consumed cannot be booked twice.
- **Failover loses nothing.** When the owner is killed mid-session, a successor claims the lease, replays the log and continues with contiguous sequence numbers.

### Metrics and cost

- **The numbers on the dashboard fold out of the log.** A cold rebuild from the stored events reproduces what the running process reports. A node that restarts and picks a session back up recovers its history exactly once, with nothing dropped and nothing counted twice. A deduplicated retry adds no second call.
- **An unaccounted call is marked and not counted as free.** A provider that streams an answer and no usage lands as an accounting gap with estimated token counts. It never lands as zero tokens for zero dollars.
- **Capability gates the correlary.** Local traffic with no comparable hosted model is reported as unpriced. It adds nothing to the savings figure. It does not fall back to the nearest rate card.

### Tenancy, budgets and keys

- **Two tenants that share a cache key do not share a session.** A turn is attributed to the principal that paid for it. The per-principal folds and the deployment fold are asserted to sum. The tests are `two_principals_using_one_cache_key_do_not_share_a_session` and `a_turn_is_attributed_to_the_principal_that_paid_for_it`.
- **An exhausted budget degrades and does not fail.** This is the loudest test in the suite: `an_exhausted_frontier_budget_routes_local_instead_of_failing`. The member ceiling binds even when the project has room. A hold that a killed turn left expires. The next open of the session repairs a lost settle.
- **A quote never carries a secret.** This is asserted over both `Debug` and the serialized log. A principal with no credential for a provider never sees it among the candidates.
- **A minted key is returned once.** Revoking it stops the key within one cache time-to-live. The budget view reports committed and measured spend separately. Drift goes negative and stays visible when a settle is lost.
- **Recreating an archived project keeps its spend.** The test `recreating_an_archived_project_after_a_restart_inherits_its_spend` shows why the durable directory matters. See [Deploy with Redis](../operations/redis.md).
- **A fair-use ceiling that cannot be checked refuses the turn.** It refuses within two seconds of a severed Redis connection.

### Validate and steer

- **A steered turn does not fork the conversation.** The guidance answer is an ordinary stored item. The resend of the next turn admits it as prefix. The turn that fulfils a steer is never validated itself. A configuration that still names the retired `tool_call` channel is refused at load by name.
- **A verdict never becomes a conversation item.** Stored items are byte-identical around a validation, which stops every later turn from forking. The side call books under its own model row and never reaches the cache ledger. A validator timeout releases the turn unchanged and marks it as not free. The cadence alone never fires without a signal.
- **An overlay cannot widen the ceiling.** An overlay that asks for too much is narrowed and says so. `fetch_steer` is byte-identical on a second call and makes no calls into the control reads. A steer of another principal is refused without naming it.

### Real clients

- **A real `codex` binary drives all of it.** It completes the MCP handshake against our mount and prints our steering directive as its own answer. On a resumed run, it admits the guidance as prefix without forking the session, and the fulfilling turn is never validated. The usage that a steered turn reports is the context that it admitted. A key revoked between runs stops the client. The flat tool name that codex resolves for a generated skill equals what `codex_launch` renders.
- **A profile that a person wrote reaches the wire.** A real `claude` is launched by a real `topham`, from hand-written TOML in an isolated configuration directory and a child environment with no `ANTHROPIC_*` variable. It arrives with the turn key on its dedicated header, an inert sentinel and no bearer token. The same profile, marked chained, goes through the generated wiring and preflight of `topham relay`. It arrives through the gateway of Relay with all of that intact. Every other launch test leaves this link open. They show that the generated map works. This test shows that something an operator can run produces it.

## Test-design rules

These rules come from defects that a test missed.

### Put the boot composition in a library function

The same failure appeared three times. A test claimed to cover the composition root but re-derived the boot predicate in a local closure, or copied the match of `main.rs` by hand. A mutation of the real boot site stayed green, because `main()` is a binary that no test calls.

- The warning for fair use at boot was never exercised.
- A mutation that wired the per-process correlation table into the shared arm left the workspace green.
- A fail-open fallback that retried the directory over a fresh memory store left both boot suites green.

The rule is that the composition is a library function: `shared_backend::open` and `control_config::boot_directory`. `main.rs` only maps its error. The boot tests call the same function. One limit remains. A guard on a message that only `main.rs` prints still copies the message.

### Run fixture tests through the production code

The fixture tests of `roundhouse-sequence-id` first ran over a hand-copied Messages canonicalizer, because the crate cannot depend on the server. Changing the production `messages_api::wire::is_budget_notice` to return `false` left all the crate tests green. The pinned divergence numbers described a canonicalizer that shipped nowhere. The tests now live in `crates/roundhouse-server/tests/sequence_identity_fixtures.rs` and run through the production `canonicalize`.

### Pin labels byte for byte

`crates/roundhouse-server/tests/fixtures/golden-labels.json` holds 50 Messages cases and 22 Responses cases. The test `every_label_matches_the_golden_capture` checks every one.

- The Messages cases are the 7 captured Claude bodies. Each runs under the 6 captured header sets and with no headers. One more case uses a body with no `metadata`, because every captured body carries a `user_id`. Without it, the anonymous rung goes untested.
- The Responses cases cover every precedence rung, blank and non-ASCII values, the unnamed 422, and combined-invalid cases that pin the order of refusals.

A label is the key that a log is stored under. A spelling change would fail no request. It would open a cold session for every live conversation after a deploy, and every turn would still answer.

### Check the Messages stream with a strict oracle

The two official SDKs do not validate what they parse. The TypeScript SDK (commit `7ba6a3fc`) yields `JSON.parse(sse.data) as Item`, a bare cast. Its `checkNever` does nothing at run time, so unknown delta types are ignored. It does enforce event order, with seven throws in `src/lib/MessageStream.ts`. The Python SDK (commit `181e2e57`) builds models with Pydantic `construct`, which does no validation and keeps unknown keys.

A spawned SDK can therefore check only liveness and event order. The first tier of tests uses a strict parser that is written from the pinned spec and lives in `crates/roundhouse-server/tests/common/anthropic.rs`. It uses closed enums and `deny_unknown_fields`, the deliberate opposite of the shipped module. It reads the SSE output of the serve surface. The ordering rules of the SDKs are encoded as ordering tests. An addition upstream turns it red first. Update it together with the spec pin, and never loosen it. The second tier is the `e2e-claude` suite with a real binary.

A dev-only parser can pull in a default feature set that includes OpenSSL. Under resolver v3, the features of a dev-dependency do not unify into `cargo build`.

### Pin two client lines

Claude Code drifts between versions. A capture of 2.1.247 and a capture of 2.1.251, one day apart, disagreed on one shape. The shape was an interior `role: "system"` message, which would refuse every request of the new line. Version 2.1.257 added a second silent change: a trailing notice, and one extra item for each `--continue`. No request was rejected, failed to parse, or returned 422.

Swapping the 2.1.257 fixtures under the 2.1.251 suite gave 36 passed and 9 failed. One failure was the real finding: 9 canonical items against 8. Four came from a tool count that differed, 21 captured against 24. `DesignSync`, `Monitor` and `PushNotification` are tools of the interactive surface, and a plain `-p` rig never declares them. Three hard-coded a session UUID of 2.1.251 into the tests. One hard-coded the prefix `cc_version=2.1.251`.

Drift is the normal cadence. For this reason the suite pins two lines, 2.1.251 and 2.1.257, and every fixture test runs against both. The e2e suite prints the client version. The directory `crates/roundhouse-server/tests/fixtures/` also holds a sanitized capture of request shapes from 2.1.272, taken on 2026-09-16. No test loads it. It records these facts for that version:

- Each request is `POST /v1/messages?beta=true`.
- `x-claude-code-session-id` equals the session id that `--session-id` set.
- `metadata.user_id` is a JSON string of an object with the keys `account_uuid`, `device_id` and `session_id`.
- The body has no `prompt_cache_key`.
- The headers `session-id`, `thread-id` and `x-client-request-id` are absent.

### Keep `CODEX_HOME` under `target/`

A reading of Codex 0.146.0 claimed that a release build does not create its helper symlinks when `CODEX_HOME` is under the temporary directory. A sandboxed shell would then depend on a run-time `bwrap` probe. A test run corrected this. With `CODEX_HOME` and the working directory in `/tmp`, two full runs passed, including the whole steering suite. The suite uses the `read-only` sandbox posture and dispatches no `exec_command`, so it never reaches the path in question. The suite still keeps `CODEX_HOME` under `target/`. "Never reached here" is a narrower claim than "release builds refuse it", and no test proves or disproves the refusal.

### Capture warnings with one global subscriber

In `tracing-core` 0.1.36, the first registration of a callsite asks only the default dispatcher of the registering thread. The condition is that a scoped dispatcher is the only live one. A test without a capture that reached a `warn` callsite first, in the middle of another capture, cached `never` for the whole process. Tests that expect a warning once then failed in 12 of 40 runs, and 20 of 80 runs beside another suite, with an empty capture.

The fix in `crates/roundhouse-server/src/test_support.rs` installs one global subscriber, once. Its filter asks per event whether the calling thread is capturing. After the fix, 0 of 40 and 0 of 80 runs failed.
