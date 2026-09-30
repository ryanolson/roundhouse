# Testing

This chapter shows how to run the tests, which suites need something extra, which invariants the suite proves, and the rules for writing a test worth keeping.

## Build and test

The build needs the Rust toolchain that `rust-toolchain.toml` pins (1.96.1) and the system package `libzmq3-dev`, which `dynamo-kv-router/standalone-selection` pulls in.

```bash
apt-get install -y libzmq3-dev   # or: brew install zeromq
timeout 900 cargo test --workspace
```

The first build clones `ai-dynamo/dynamo` to resolve the pinned Dynamo crates, so it takes a while. Later builds reuse the checkout. The default run needs no GPU, no worker process and no network, because the selection service runs inside the test binary.

### Bounded runs

Run every test command under a coreutils `timeout`: `timeout 900` for the workspace, about `timeout 300` for a targeted run. The hook `.claude/hooks/cargo-test-timeout.sh` enforces this.

A hung test hangs the whole cargo run, silently. The runs most likely to hang one are adversarial reviews that mutate timeout and deadline code on purpose; a broken timeout path does not go red, it waits forever. A bounded run turns that into exit 124 in minutes. On exit 124, suspect the newest test or the latest mutation. Run the suspect binary again with `--test-threads=1 --nocapture` under a short timeout to name the hang.

### File descriptor limit

With a soft limit of 1024 open files, the `embedded_selection` test binary (`crates/roundhouse-fleet/tests/embedded_selection.rs`) failed with `Too many open files`. After `ulimit -Sn 65536`, its 7 tests and the full workspace run passed.

## Suites that you opt into

| Suite | How to opt in | Command and details |
|---|---|---|
| Real Redis | `ROUNDHOUSE_TEST_REDIS_URL` plus `--include-ignored` | [Real Redis](#real-redis) |
| Real `codex` | `--features e2e-codex` plus `--include-ignored`; `ROUNDHOUSE_TEST_CODEX_BIN` overrides the `PATH` lookup | [Hook up Codex](../guides/codex.md#the-gated-real-binary-suite) |
| Real `claude` | `--features e2e-claude` plus `--include-ignored`; `ROUNDHOUSE_TEST_CLAUDE_BIN`, `ROUNDHOUSE_TEST_RELAY_BIN` for the chained tests, `ROUNDHOUSE_TEST_TOPHAM_BIN` for the launcher tests | [Hook up Claude Code](../guides/claude-code.md#the-gated-real-binary-suite) |
| Live cache probe | `--features e2e-frontier` and three `ROUNDHOUSE_PROBE_*` variables | [Cache probe](#cache-probe) |

The e2e features are off by default and not in the crate's self dev-dependency, so a developer with no `codex` or `claude` gets an empty test binary, not a failure. With a suite enabled, a missing binary panics.

The two closure tests of the `claude` suite need a built launcher:

```bash
cargo build -p topham
ROUNDHOUSE_TEST_TOPHAM_BIN=$PWD/target/debug/topham
```

`ROUNDHOUSE_TEST_TOPHAM_BIN` has no `PATH` fallback. `topham` is installed nowhere, so a bare name would resolve to whatever a developer happened to have.

### Real Redis

Every test that needs a real Redis is `#[ignore]`. That covers the boot suites and the contract suites of every Redis family. Set `ROUNDHOUSE_TEST_REDIS_URL` and pass `--include-ignored`.

Once you opt in, an unreachable URL panics instead of skipping; a silent skip is how a backend suite stops running unnoticed. **Rejected: an early return on a missing variable.** It prints "passed" for a test that verified nothing, and `#[ignore]` is the one skip the harness reports.

Rules for a real-Redis test:

- **Contract suites run over both stores.** One macro per suite runs it over the memory and the Redis implementation, and the memory side is the specification. See [Session store contract](store-contract.md#the-contract-suite).
- **Use a fresh namespace per test**, so a run does not lengthen the permanent index that the next run reads.
- **Prove time without sleeping where possible.** The memory side uses a scripted clock. Redis expiry follows the server's clock, so a test waits about 250 ms over a shortened time-to-live; forcing expiry would test the seam, not the server. A production time-to-live is checked by reading `PTTL`.
- **Serialize `INFO commandstats` measurements.** The counter is server-wide, so a lock serializes the measuring test against the other real-Redis tests in its binary. Without it, neighbours landed reads inside the measurement: nine and eleven where the script issues seven.
- **Run the `maxmemory` tests alone** (`--test-threads=1`, with `redis-cli` on `PATH`). Two tests change the global `maxmemory` of the shared Redis, so one can force an out-of-memory state while the other retries.
- **The memory document store cannot race**, because `MemoryDocumentStore::commit` has no `await`. The directory race test therefore races only Redis: 64 barrier-synchronized threads over 50 rounds never landed in a split-lock window.

### Isolating a real `claude`

Clearing the environment and `CLAUDE_CONFIG_DIR` does not isolate a real client. One `claude config list` under `env -i`, with no base URL override, still reached a real model through credentials outside both. Point every rig at loopback through `ANTHROPIC_BASE_URL`, and treat any run that nothing logged as a run that went to production.

Inside a Claude Code Remote container, `CLAUDE_CODE_REMOTE=true` is ambient. At 2.1.257, a rig that unset only some variables sent the container's OAuth bearer, two OAuth betas, two `x-claude-remote-*` headers, and a `GET /v1/code/agent-proxy/ca-cert` before every turn. A fully cleared rig sent `x-api-key` and none of these. It keeps only `HOME`, `CLAUDE_CONFIG_DIR`, `PATH`, `DISABLE_AUTOUPDATER=1`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`.

### Cache probe

The offline suite covers the shared driver, the request markers, the configured transport and the budget refusal. It does not establish how a provider caches.

```bash
timeout 300 cargo test -p roundhouse-server --test cache_probe
```

The `e2e-frontier` feature adds a live probe: two turns to one Messages target, the second appending 20 items, with each turn's cache reads, cache writes and usage provenance reported separately. It needs a catalog with real prices and a deterministic cache model, a pinned `provider/model`, and a USD cap. Inject the key with `openv` into the variable that the provider's `auth.env` names. Check the model's minimum cacheable prefix first: the fixture holds at least 8,192 words (not tokens) before its marker.

```bash
openv env ROUNDHOUSE_PROBE_CATALOG=/path/to/catalog.json \
  ROUNDHOUSE_PROBE_MODEL='anthropic/<pinned-model-id>' \
  ROUNDHOUSE_PROBE_LIMIT_USD='<approved-cap>' \
  timeout 300 cargo test -p roundhouse-server --features e2e-frontier \
    --test cache_probe live -- --nocapture
```

The live test has no `#[ignore]`, so enabling the feature puts it in an unfiltered run. Missing configuration fails before dispatch. The project budget governs both turns, with 16 output tokens and a 30-second deadline each. A cache read of zero stays a reported observation. See [Measure cache reuse](../guides/cache-reuse.md).

## Validating a claim

A claim about behavior is an opinion until a failing test confirms it. This covers review findings, bug reports and every "I think X is broken". Write a test that fails for the stated reason, rule on the claim, then fix it.

- If the test passes, the claim is wrong, or the test does not exercise the claim.
- If nothing can test the claim, move the seam: add the accessor, extract the pure function, or split the type. Keep that change additive and behavior-preserving.
- A ruling is valid, partially valid or invalid. "Partially valid" means the defect is real and the stated mechanism is wrong. Fix the real mechanism; a fix for the stated one leaves the defect behind a passing test.
- When evidence lands before the fix, mark the failing assertion `#[ignore = "<finding>: <why it fails>"]` and keep the passing controls live. An ignored test enforces nothing, so removing the `ignore` is the first step of the fix.

## What the tests establish

Names in backticks are test functions that you can run by name.

### Hashing, pricing and routing

- **Incremental hashing is exact**: it matches a full recompute across unaligned appends, and a shared prefix yields an identical sequence-hash chain.
- **Client bytes stay flat**: over 20 turns, per-turn client bytes vary by 2 or less while server-side context grows more than 10 times.
- **Pricing books no load**: two `select` calls yield distinct pending selections and zero booked prefill, and an abandoned quote changes no later price.
- **Routing reacts to cache state**: a warmed target is priced below its prompt length on the next turn.
- **Local cache hits are measured**: `dynamo-mocker` publishes real KV events over ZMQ, and a repeat turn under the TinyLlama BPE prices at about 2% of its prompt (32 of 1536 tokens).
- **What was hashed is dispatched**: local execution consumes the token buffer's ids verbatim, byte-identical to the quoted stream.
- **A policy knob changes routing**: a quality floor excludes a target the default would pick, and a filtered target never appears in `considered`, so savings are never priced against a model the key could not reach.
- **A dead provider costs one attempt**: a transport failure moves to the next same-tier candidate inside one turn, deadline and grant, and the failover crosses transports as well as targets. A refusal, a 401 and a 404 fail where they stand. An exhausted tier fails with every attempt on record, and the degrade-to-local promise survives any recipe.
- **The tier is read from the session**: a stalling session, driven as items through the real signal extractor, lands on the capable tier. A productive one comes back down, a quiet one falls open, and control calls count toward none of it. Each case has a control that varies only the exchanges.

### Turns, the log and failover

- **Retries redeliver**: a re-sent `turn_id` replays the response with byte-identical text and the original usage, and opens no second turn.
- **A failed turn settles**: it ends with an incomplete event, the lease returns at once, and the turn id is retryable without waiting for a time-to-live.
- **Streaming is genuine**: deltas are durable before completion, and a stream that breaks halfway commits its partial output, which the ledger reads as prefill evidence. The metrics fold measures time to the first nonempty text delta.
- **A turn outlives its lease**: the heartbeat lets a model call longer than the time-to-live commit, while a displaced owner still loses and a hung provider settles at the turn deadline.
- **The log is the streaming bus**: SSE tails the replay log, and resumption from `starting_after` or `Last-Event-ID` is exact.
- **Reservations settle**: load returns to zero, and a consumed `selection_id` cannot be booked twice.
- **Failover loses nothing**: after the owner is killed, a successor claims the lease, replays the log and continues with contiguous sequence numbers.

### Metrics and cost

- **The dashboard folds out of the log**: a cold rebuild reproduces the running process, a restarted node recovers history exactly once, and a deduplicated retry adds no call.
- **An unaccounted call is not free**: a stream with no usage lands as an accounting gap with estimated tokens, never as zero dollars.
- **Capability gates the correlary**: local traffic with no comparable hosted model is unpriced and adds nothing to savings. It never falls back to the nearest rate card.

### Tenancy, budgets and keys

- **Two tenants that share a cache key do not share a session**, and the per-principal and deployment folds sum (`two_principals_using_one_cache_key_do_not_share_a_session`, `a_turn_is_attributed_to_the_principal_that_paid_for_it`).
- **An exhausted budget degrades and does not fail** (`an_exhausted_frontier_budget_routes_local_instead_of_failing`); the member ceiling binds even when the project has room, a killed turn's hold expires, and the next open of a session repairs a lost settle.
- **A quote never carries a secret**, in `Debug` or in the log, and a principal never sees a provider it has no credential for.
- **A minted key is returned once**, revocation stops it within one admission-cache time-to-live, and drift stays visible when a settle is lost.
- **Recreating an archived project keeps its spend** (`recreating_an_archived_project_after_a_restart_inherits_its_spend`). See [Deploy with Redis](../operations/redis.md#admin-directory).
- **A fair-use ceiling that cannot be checked refuses the turn** within two seconds of a severed Redis connection (`a_ceiling_that_cannot_be_checked_refuses_within_a_bounded_time`). See [Deploy with Redis](../operations/redis.md#outages).

### Validate and steer

- **A steer does not fork the conversation**: the guidance is an ordinary stored item that the next resend admits as prefix, the fulfilling turn is never validated, and a configuration naming the retired `tool_call` channel is refused at load.
- **A verdict never becomes a conversation item**: stored items are byte-identical around a validation, the side call never reaches the cache ledger, and a validator timeout releases the turn unchanged.
- **An overlay cannot widen the ceiling**: an over-asking overlay is narrowed and says so, and another principal's steer is refused without naming it.

### Real clients

- **A real `codex` drives all of it**: MCP handshake, steering directive, prefix admission of the guidance on a resumed run, a key revoked between runs stopping the client, and the flat tool name it resolves for a generated skill equal to what `codex_launch` renders.
- **A hand-written profile reaches the wire**: a real `claude`, launched by a real `topham` from hand-written TOML, arrives with the turn key on its header and no bearer, directly and through Relay's gateway. Other launch tests show the generated map works; this one shows an operator can produce it.

## Test-design rules

Each rule comes from a defect that a test missed.

### Put the boot composition in a library function

Three times, a test that claimed to cover the composition root copied the boot logic of `main.rs`. A mutation of the real boot site stayed green, because no test calls `main()`. One mutation wired the per-process correlation table into the shared arm; another retried the directory over a fresh memory store. The composition is therefore a library function (`shared_backend::open`, `control_config::boot_directory`) that the boot tests call, and `main.rs` only maps its error. A guard on a message that only `main.rs` prints still copies the message.

### Run fixture tests through the production code

The fixture tests of `roundhouse-sequence-id` once ran over a hand-copied Messages canonicalizer, because the crate cannot depend on the server. Making the production `is_budget_notice` return `false` left them green: they measured a canonicalizer that shipped nowhere. They live in `crates/roundhouse-server/tests/sequence_identity_fixtures.rs` and run through the production `canonicalize`.

### Pin labels byte for byte

`crates/roundhouse-server/tests/fixtures/golden-labels.json` holds 50 Messages cases and 22 Responses cases, and `every_label_matches_the_golden_capture` checks each one.

- Messages: the 7 captured Claude bodies under each of the 6 captured header sets and with no headers, plus one body with no `metadata`. Every captured body carries a `user_id`, so without that case the anonymous rung goes untested.
- Responses: every precedence rung, blank and non-ASCII values, the unnamed 422, and combined-invalid cases that pin the refusal order.

A label is the key a log is stored under. A spelling change fails no request; it opens a cold session for every live conversation after a deploy.

### Check the Messages stream with a strict oracle

The official SDKs do not validate what they parse. The TypeScript SDK (commit `7ba6a3fc`) yields `JSON.parse(sse.data) as Item`, a bare cast, but enforces event order with seven throws in `src/lib/MessageStream.ts`. The Python SDK (commit `181e2e57`) builds models with Pydantic `construct`, which does not validate and keeps unknown keys. A spawned SDK can therefore check only liveness and order.

The first tier is a strict parser written from the pinned spec in `crates/roundhouse-server/tests/common/anthropic.rs`, with closed enums and `deny_unknown_fields`, the opposite of the shipped module. It reads the serve surface's SSE output and encodes the SDK ordering rules as tests, so an upstream addition turns it red first. Update it with the spec pin; never loosen it. The second tier is the `e2e-claude` suite.

### Pin two client lines

Claude Code drifts between versions. Captures of 2.1.247 and 2.1.251, one day apart, disagreed on an interior `role: "system"` message that would have refused every request of the new line. 2.1.257 silently added a trailing notice and one item per `--continue`. Running the 2.1.257 fixtures under the 2.1.251 suite gave 36 passed and 9 failed. One failure was the real finding (9 canonical items against 8). Four came from tool counts (21 against 24: `DesignSync`, `Monitor` and `PushNotification` exist only on the interactive surface). Four hard-coded a 2.1.251 session UUID or `cc_version`.

The suite therefore pins 2.1.251 and 2.1.257, every fixture test runs against both, and the e2e suite prints the client version. `crates/roundhouse-server/tests/fixtures/claude-2.1.272-wire-shapes.json` is a sanitized 2.1.272 capture that no test loads; its facts are in [Hook up Claude Code](../guides/claude-code.md#claude-code-wire-facts-by-version).

### Keep `CODEX_HOME` under `target/`

A reading of Codex 0.146.0 claimed that a release build skips its helper symlinks when `CODEX_HOME` is under the temporary directory. With `CODEX_HOME` in `/tmp`, two full runs passed, but the suite uses the `read-only` sandbox and dispatches no `exec_command`, so it never reaches that path. "Never reached here" is narrower than "release builds refuse it", and no test decides the refusal, so `CODEX_HOME` stays under `target/`.

### Capture warnings with one global subscriber

In `tracing-core` 0.1.36, when a scoped dispatcher is the only live one, a callsite's first registration asks only the registering thread's default dispatcher. A test with no capture that reached a `warn` callsite first cached `never` for the whole process. Tests that expect a warning then failed in 12 of 40 runs (20 of 80 beside another suite). `crates/roundhouse-server/src/test_support.rs` installs one global subscriber whose filter asks per event whether the calling thread is capturing. After that, 0 of 40 and 0 of 80 runs failed.
