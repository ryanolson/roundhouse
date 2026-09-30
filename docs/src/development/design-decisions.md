# Design decisions

This chapter records the alternatives that Roundhouse rejected. Each entry gives the choice, the rejected alternative, and the failure the alternative would have caused. Entries are grouped by subsystem.

External facts name the revision they were read from. The dependency pins are in [Upstream dependencies](upstream.md).

## Product boundary and topology

### Roundhouse keeps a durable log, and there is no proxy-only mode

**Choice.** Two modes ship. The ephemeral mode has no Redis: one node, state in the process, and a restart survived by refusal and re-derivation. The durable mode puts Redis behind every state family that has a durable implementation. One switch chooses between them, `ROUNDHOUSE_REDIS_URL`, with no second predicate. See [Deploy with Redis](../operations/redis.md).

**Rejected.** A mode that keeps no log.

**Failure.** Without a log, four promises go away: replay and audit (the Relay-format exports are cold replays), exact settlement (a session replays its log when it next opens and settles what a dead process left unsettled), drift reconciliation between the committed ledger and the measured log, and steering (the correction is a conversation item). Prefix admission has nothing to compare a resent history against, and an idempotent retry across a reconnect becomes "run the turn again and bill twice". What remains (auth, tenancy, routing, fair use, grants, pass-through credentials, MCP tools) is the product of NeMo Relay.

### Roundhouse serves the Messages API over its own log

**Choice.** `/v1/messages` is a native surface over the same log as `/v1/responses` (`crates/roundhouse-server/src/messages_api.rs`). Roundhouse has no server-side tool loop. The client runs its own tools on both surfaces.

**Rejected.** Sending Claude Code traffic through the Messages surface of vLLM agentic-api, or through the `ANTHROPIC_BASE_URL` path of Relay.

**Failure.** Where the turn does not pass through Roundhouse, Roundhouse owns no session, no prefix admission, no pricing, and no steering. The Relay path is an interception mechanism and still needs a Messages-speaking upstream.

### Roundhouse is not a Relay plugin

**Rejected.** Running Roundhouse as a plugin component inside NeMo Relay.

**Failure.** The Relay runtime is process-global, in memory, and request-scoped. The fenced lease, the `seq`-ordered log, prefix admission, and crash replay have no home in a middleware callback. The Relay response cache bypasses stateful requests (`store`, `previous_response_id`, `conversation`, `container`, and any Responses body without `store: false`; `nemo-relay-adaptive-0.8.2` `response_cache/key.rs:128-147`), so every turn Roundhouse hosts would be outside Relay's jurisdiction.

### No surveyed neighbor owns the turn

A survey of four trees found that Roundhouse duplicates almost nothing: NeMo Relay (`ca08901`, and 0.8.2), Switchyard (`5341f71`, rechecked at `053a61e`), vLLM agentic-api (`d59d4b4`), and the Kubernetes Gateway API inference extension (`84436a9`). Relay owns the harness, Switchyard the route and the proxy, agentic-api server-held state for one stateless vLLM upstream, and the Gateway API extension moved its scheduler to llm-d. Absent from Relay and Switchyard:

- A fenced, single-writer, append-only log per session. The Relay bus is an in-memory subscriber bus, and the nearest Switchyard thing is a flat JSONL routing log with no `seq` and no lease.
- Conversation ownership and delta admission. The Relay `response_cache` bypasses stateful requests.
- Tenancy, budgets, and spend. A grep of Switchyard for `budget|quota|tenant|principal|spend` finds only retry budgets, token budgets, and review counters. Neither tree has dollars in its Rust code, and Relay has one credential concept, a per-invocation loopback proxy token (`nrp_`).
- Steering by injection. Relay can block a tool but cannot inject one, and Switchyard can append text but cannot inject a tool call.
- An MCP control surface. The Relay MCP server answers `tools/list` with `[]`, and Switchyard has none at `053a61e`.
- Shadow, placebo, or observe-only judge arms, and a capability gate (`quality_prior|capability_band` has zero hits in both trees, and the Relay `baseline_model` is not gated).
- Realized cache evidence. The Relay adaptive cache planner plans from an assumed `expected_reads`.
- Deduplicated turn identity and byte-identical replay.

Relay 0.8.2 makes the opposite state choice on purpose: a single-process, loopback-only gateway that treats the request as the conversation (see [Upstream dependencies](upstream.md#relay-082-on-the-wire)). The durable log of Roundhouse answers "the state has to live somewhere".

Two pieces overlap. The Relay pricing catalog has tiered rates, aliases, prompt-cache pricing, `pricing_as_of`, and `pricing_source`, and a `ROUNDHOUSE_CATALOG` price entry has no as-of date or source field. Relay also stamps `x-dynamo-session-id` and `x-dynamo-parent-session-id` on outbound requests and sends `x-nemo-relay-adaptive-agent-hints`. Roundhouse consumes neither.

### One continuation contract: full resend with prefix admission

**Choice.** The client resends the whole transcript, and Roundhouse admits the prefix against its stored log. The history binds by `thread-id`, then `session-id`, then `prompt_cache_key`. `ResponsesRequest` has no `previous_response_id` or `conversation_id` field. See [Sessions and the event log](../concepts/sessions.md).

**Rejected.** A second continuation key by delta upload, as agentic-api does (`previous_response_id` or `conversation_id` plus only the new input).

**Failure.** A client cannot both omit the history and send it, so two contracts cannot both be authoritative on one path. Prefix admission is the product: transparency and exact pricing depend on it. At agentic-api `d59d4b4` and `e35fbb2`, the inverse stack cannot work: agentic-api cannot point at Roundhouse as its `llm_api_base`, because its `UpstreamRequest` has no `prompt_cache_key`, and Roundhouse refuses a request with no conversation identity. The only coherent stack is Roundhouse in front and agentic-api behind it on its stateless raw-proxy branch. That branch holds only while tools are plain functions (agentic-api switches to its own executor on any non-`Function` tool), and a Codex turn carries `namespace` and `custom` tools. The default `db_url` of agentic-api also changed at HEAD from "stateless, proxy everything" to "create a local SQLite store silently".

### Roundhouse sits in front of a Gateway API gateway, not behind it and not as its picker

**Choice.** If a Kubernetes Gateway API gateway fronts Roundhouse, Roundhouse is a plain `Service`. An `InferencePool` can be at most one local target, used only for discovery and health, with `endpointPickerRef` unset (legal since v1.5.0 of the extension). The repository ships no Dockerfile, chart, manifest, or Kubernetes code.

**Rejected.** Roundhouse as an `InferencePool` backend, and Roundhouse as the Envoy ext-proc endpoint picker. A picker is buildable, and Roundhouse would pick better than the reference picker because it can ask Dynamo for real overlap.

**Failure.**

- Behind a gateway is incoherent. A gateway picks a pod per request, but Roundhouse pins a session to a fenced single-writer lease. Turn n+1 can land on a pod without the lease, and the picker protocol cannot say "keep this conversation on that writer".
- An `InferencePool` cannot name a frontier model. `spec.selector` matches Pods by label in one namespace, endpoints are `podIP:portNumber`, and the picker reference forbids `ExternalName` Services. The local-and-frontier half of the product cannot be expressed. GEP-4894 "Backend Resource" (Experimental, `ExternalHostname`) had no CRD in `kubernetes-sigs/gateway-api:apis/` at the read.
- As a picker, the return value is an `ip:port`, so "this turn went to Anthropic" cannot cross the seam. The protocol is per request and leader-elected, so the session lease, the log, and prefix admission have no home. It also puts a full-body buffer and parse of a 100k-token context on the gateway path of every turn, which is the cost Roundhouse exists to remove.
- A gateway in front can route around Roundhouse, change who pays, or re-encode the history. It needs the same guards as a chained Relay before it counts as supported.

The scheduler rule is met by construction: Roundhouse embeds the Dynamo KV router and writes no second scheduler.

### Roundhouse forwards the credential of the caller

**Choice.** Roundhouse forwards the agent's own credential (a stored key, or a forwarded login seat) and stamps a payer. It never holds the seat (`crates/roundhouse-fleet/src/openai_responses.rs`). See [Configure tenancy and keys](../guides/tenancy.md).

**Rejected.** The custodial model of Ramp Router (`https://api.router.com/v1`, read 2026-08-20): the application holds one Router key, and Router holds the provider credentials. Its bring-your-own-key option is also custodial, and it is not available for Anthropic or Google Vertex AI. Router fronts hosted providers only, so there is no path to a customer-owned GPU.

### An exhausted budget degrades to local

**Choice.** `on_exhaustion: degrade_to_local` routes a project to the local tier when its budget runs out (see [Configure tenancy and keys](../guides/tenancy.md)).

**Rejected.** The fallback chain of LiteLLM, OpenRouter, Portkey, and Kong. A survey found none that falls back to a free local tier. Every chain ends in another hosted, metered model.

**Failure.** A chain that ends in a metered model keeps spending after the budget is gone.

## Wire surfaces

### The Anthropic wire module is hand-written, typed where Roundhouse reads, open everywhere else

**Choice.** `crates/roundhouse-fleet/src/anthropic_messages/wire.rs` is written by hand and shared by the dispatch client and the serve surface. It is typed where Roundhouse reads or creates data:

- the SSE event set, including `ping` and a mid-stream `error`.
- `Usage` with the full `cache_creation` 5m and 1h breakdown.
- `stop_reason` as an open enum with an `Other(String)` arm.
- the content blocks that map to conversation items, and `cache_control` with the real 5m and 1h TTL vocabulary.

Everything else is open. Unknown request fields, block types, and betas pass through verbatim in `#[serde(flatten)]` extras or opaque JSON.

**Rejected, and the failure of each.**

- **`deny_unknown_fields` anywhere in the module.** The serve surface is also a pass-through. A client field newer than the pinned spec would make admission fail closed on a request that Roundhouse must pass through.
- **An existing Rust crate.** Anthropic has no official Rust SDK and owns no crate on crates.io. Read at the versions named, `siumai-protocol-anthropic` 0.11.0-beta.10 is client direction only (no `decode_request` or `encode_response`) and speaks a neutral form. `claudius` 0.33.0 has closed `StopReason` and `ContentBlock` enums, lacks 6 of the 12 response blocks, has no TTL on `cache_control` and no `error` stream event, and its default features pull OpenSSL. `adk-anthropic` 2.1.0 declares a non-existent `cache_creation_input_tokens_1h` instead of the nested `cache_creation` object, so its cache counters would be silently wrong. `shunt-gateway` 0.32.0 does untyped `serde_json::Value` surgery and cannot create responses, which Roundhouse must do for local turns. No surveyed crate models `ephemeral_5m_input_tokens` or `ephemeral_1h_input_tokens`, `dynamo-protocols` 5.4.0 has no Anthropic types, and the Relay codec is lossy too (see [Upstream dependencies](upstream.md#the-relay-anthropic-codec-is-lossy)).
- **Generating the types from the OpenAPI spec.** The spec is strict in one direction only. It has 693 `additionalProperties: false`: 149 on request schemas, 544 on other input shapes, and 0 on response schemas. A faithful generator would put `deny_unknown_fields` on the serve side, and no crate offers a switch to suppress it. `ping` and `error` would still need hand-written types. So the spec is pinned only to check field spellings, the way the ATIF field names are pinned.

### The stored tool name is not migrated

**Choice.** A forward-only optional `namespace` field on the stored tool call (`crates/roundhouse-core/src/item.rs`).

**Rejected.**

- A one-time rewrite. It cannot recover a namespace that was never written, so it can only guess which bare `status` was ours. The name is inside `Item::render` and the dedup key is the turn id, so a rewrite would move the turn id of every conversation with a control call, and an in-flight retry would buy a second billed answer. A Redis stream entry cannot be written over in place anyway.
- Canonicalizing at read time. That is the existing name-list recognizer under another name.
- A versioned record tag. That is a new variant on an internally tagged enum, which an older build cannot read.

## Sessions and prefix admission

### Prefix admission probes, then commits once

**Choice.** `probe` asks one generation one question and writes nothing. `bind_prefix` searches with it and commits exactly once, after the home is known (`crates/roundhouse-server/src/prefix_admission.rs`). A refusal commits nothing, so a verbatim retry probes the same generations and gets the same refusal.

**Rejected.** Forking on every probe attempt: moving the counter of the key and `latest` before anything was known.

**Failure.**

- A refusal left the counter and `latest` on a generation that no turn ran on, so a verbatim retry resumed past the bound and was admitted whole a few forks later. Claude Code retries a 409 without condition.
- A search only upward from the counter of this node duplicated a prefix that an older, agreeing generation already held.
- An empty generation that another node was mid-turn on was taken whole, and the turn died on the lease.

### The chain unit is the message, not the token block

**Choice.** The prefix chain has one link per item, at message boundaries (`crates/roundhouse-core/src/item/chain.rs`).

**Rejected.** Hashes of token blocks as the chain unit.

**Failure.** Message boundaries do not depend on the model, so one chain serves every local and frontier target and is computed before a target is chosen. Provider caches match exact prefixes, and a message boundary is where Roundhouse already places Anthropic cache markers. Block hashes need the tokenizer and block size of each pool, so a longest match across pools would need one chain per tokenizer.

**Cost.** An edit inside an item loses the whole item. Setup: `python3 scripts/measure-prefix-divergence.py crates/roundhouse-server/tests/fixtures`, Claude Code 2.1.257 fixtures, turn 1 to turn 2 of one session. The script approximates `Item::render`, so the byte counts are approximate. The two turns share 5,096 bytes in Roundhouse render order and only 162 bytes (the first 2 items) at chain level. That under-claims overlap, which is the safe direction. Inside one Dynamo deployment, the KV router already matches blocks.

### The anchor is not a hash of the first N items

**Choice.** Per-item links with a longest match. The anchor of a sequence is the last tip of the whole first prompt (`crates/roundhouse-sequence-id/src/keyed.rs`). The identity is per KV lineage, not per agent task tree: a fresh sub-agent spawn or partial fork shares little prefix, so one id across a task tree does not help routing.

**Rejected.** A hash over a fixed number N of leading items.

**Failure.** N differs by client and version: 4 items for Codex, 5 or 6 for Claude Code. One Claude Code session also changed its own item 2 between turn 1 and turn 2, because a second process started with `--continue` removed ` (1M context)` from the system prompt. So a hash computed again at any N of 3 or more drifts inside one session. A hash stored once is only the label binding again. A longest match needs no knowledge of the client and also gives the overlap in tokens, which a price needs.

### The sequence-identity crate has one API per client

**Choice.** `crates/roundhouse-sequence-id` exposes `messages_label(headers, user_id)` for Messages, and `CodexHeaders::read` then `.label(cache_key)` for Responses. `detect_client` returns a sum type with four shapes. The crate is pure: no store, network, clock, environment, or async.

**Rejected.** One input type and one `label()` for both surfaces, a product type `Detection { client, confidence }`, private `RequestContext` fields, and `prefix_fingerprint(chain, items)`.

**Failure.** One `label()` forced the Messages caller to map an error that cannot occur and the Responses caller to refuse an `Anonymous` that cannot occur. The product type admitted nine shapes where `detect_client` returns four. Private `RequestContext` fields forced conflicting edits in `engine.rs`, so the fields stay public and `conversation_key()` is computed from them. `prefix_fingerprint(chain, items)` silently fingerprinted a mismatched chain instead of the items, so it takes `items` only. A digest compared across nodes must be the same on every node, so a stray `now_ms()` in the crate would silently break it. The server keeps `anonymous_key` and `qualify`.

## Routing

### One engine, with per-project policy as data

**Choice.** Model access and policy per project arrive as data on one engine: `TurnPolicy` in `RoutingContext` (`crates/roundhouse-core/src/routing/mod.rs`).

**Rejected.** One engine per project, which would fork the metrics fold, the turn gates, and the catalog.

### Switchyard's escalation classifier is not the escalation policy

**Choice.** A periodic `audit_every` policy: serve most turns from the cheaper pool, and send every Nth turn to the best target (`crates/roundhouse-core/src/routing/policy.rs`). `switchyard-libsy` stays behind `RoutingPolicy` (see [Upstream dependencies](upstream.md#why-no-dependency-on-switchyard-libsy)).

**Rejected.** The Switchyard escalation route as `EscalationPolicy`, read at `5341f71` and `053a61e`. It calls the weak target and buffers the reply, and a judge rules on the completed turn. An escalate verdict increments a streak. The weak reply is served while the streak is below `confirmations` (default 2). At the threshold it is discarded and the strong target serves, and a latched session goes straight to strong with no judge call.

**Failure.**

- It is reachable only through `LlmTaskClassifier::new(LlmClassifierConfig::Escalation{..})` and owns the call to the efficient tier, so it needs dispatch control that the pure `choose` seam does not give.
- Its streak lives in `State.extra`, a process-local map with a one-hour TTL. A crash and replay loses the latch. Roundhouse derives state from the log.
- The discarded efficient turn is paid for and never accounted. Context overflow and transport failures on the efficient tier fall through to capable silently, which is a fail-open on cost.
- An unlatched turn waits for the weak call, then the judge call: two serial upstream round trips. Upstream says not to use it for latency-critical traffic.
- It needs a session identity (`x-switchyard-session-id`), or it never latches.

Its outage handling agrees with Roundhouse: a judge that errors, times out, or returns something unparseable holds the streak instead of clearing it.

### The learner draws by hash and has its own strategy type

**Choice.** The arms of the learner are serving strategies (`rules`, `efficient`, `capable`), not targets. The exploration draw is a hash, not an RNG, in its own `routing-explore` domain (`crates/roundhouse-core/src/routing/learn/explore.rs`). The Jev classifier runs in the background, off the routing path, and routing makes no model calls (`crates/roundhouse-core/src/routing/stage.rs` takes the default tier where upstream Switchyard consults a classifier). See [The routing learner](../concepts/routing-learner.md).

**Rejected.** An RNG draw, and reusing the validation `Arm` for routing strategies.

**Failure.** An RNG draw cannot replay from the log. Reusing `Arm` would mix two independent assignments.

## Validate and steer

### The steer is text, not a synthetic tool call

**Choice.** The steer is an assistant text instruction. `Auto` means text. `SteerChannel::ToolCall` stays in the enum only so the configuration refusal can name it (`crates/roundhouse-core/src/validate/verdict.rs`). See [Validate and steer](../concepts/validate-steer.md).

**Rejected.** Completing the held turn with a synthetic `function_call` to `mcp__roundhouse` `fetch_steer`. It was built and run against `codex-cli 0.146.0`, with `namespace` as its own field, an `fc_`-prefixed item id, `call_id` equal to the steer id, `arguments` created once and never serialized again, and four frames (`created`, `added`, `done`, `completed`) with no argument deltas.

**Failure.** It worked mechanically: Codex dispatched the call over the real `/mcp` (an `rmcp` 1.8.0 client against the `rmcp` 3.1.3 server), appended the output, and resent both, and the session did not fork. But under `codex exec`, `approval_policy` is forced to `never`, and a tool with no MCP annotations is treated as destructive and open-world. So the first real steer was answered "user cancelled MCP tool call", while the log recorded a fulfilled steer the agent never saw. The channel has two cooperation points that can fail silently: the client must dispatch the call, and the model must obey the output. A text answer needs only a client that can read.

### Switchyard's advisor gate is not the validate loop

**Rejected.** Switchyard `AdvisorGate` (route type `advisor`) in place of the validate loop. One executor serves every turn, and a judge-only advisor reviews terminal turns. APPROVE releases the buffered turn. REDO discards it and feeds the plan of the advisor back. It is in no released version.

**Failure.** Its `route` calls the executor itself (`driver.call_model(...)`), so it would replace the turn loop of the engine, not fill a hole in it. The Roundhouse interjector is consulted before the turn is planned, cannot see the candidate list, and cannot dispatch, and `choose` must be pure. Row by row, Roundhouse would lose:

| Roundhouse | Switchyard `AdvisorGate` at `5341f71` |
|---|---|
| Review budgets projected from the log, stable across replay | Re-arms on process restart. `Mutex<GateState>` with eviction at 1024 scopes. |
| A half-open breaker with `max_in_flight = 8` | Hard-coded `MAX_FAILED_CONSULTS = 3`, no in-flight cap, no re-arm |
| A strict typed JSON verdict with `deny_unknown_fields` | An anchored regex scans prose for APPROVE or REDO |
| Four actions (Continue, Escalate, Steer, Halt) clamped through the narrow-only policy lattice, and Live, Shadow, and Placebo arms | REDO only, and no arms |
| No fail-open. A timed-out validator is marked, never free. | `fail_open` defaults to `true`, and a failed consult is audited as `verdict: "APPROVE"` |

Taken as ideas: accounting for discarded work, the anchored verdict parse, and the stall checkpoint. One gap remains: the judge asks for JSON in the prompt only, while Switchyard sets `response_format` from a JSON-schema contract, so a parse failure costs a wasted consult and can trip the breaker.

## Launchers

### `topham` is its own crate above the server

**Choice.** A workspace crate, `crates/topham`, above `roundhouse-server` in the dependency graph. Key minting is a subcommand over the existing admin route, not a new route. See [Launch with topham](../guides/topham.md).

**Rejected.** A `[[bin]]` subcommand inside `roundhouse-server`, which puts an argument parser in the composition root, whose rule is "configuration is environment variables". Also rejected: an admin-plane read beside key minting.

The Relay 0.8.0 launcher has no TUI, no named profiles, and no vocabulary for models, routes, keys, or budgets, so topham only wraps it for the chained topology.

### Direct is the reference topology

**Choice.** The agent points straight at Roundhouse, and Roundhouse generates its own minimal client configuration (`crates/roundhouse-server/src/codex_launch.rs`, `crates/roundhouse-server/src/claude_launch.rs`). The Relay CLI is the supported instrumented front end for the chained topology. The same generated client configuration serves both, because Relay overwrites the base URL and merges its headers rather than replacing them.

**Rejected.** Delegating client launch to another implementation of the same surface (Relay: 6,301 lines of Rust launchers at `ca08901`; Switchyard: 2,503 lines of Python launchers at `5341f71`, later deleted).

**Failure.** The real-binary suites must prove what a real client does against a configuration that Roundhouse wrote. A launcher that Roundhouse does not own puts that proof outside the tree.

### One Relay handoff module for both agents

**Choice.** `crates/roundhouse-server/src/relay_handoff.rs` renders the Relay configuration for both agents from one template, parameterized by `RelayAgent`. See [NeMo Relay formats](../operations/relay-formats.md#aim-a-nemo-relay-at-roundhouse).

**Rejected.** A `relay_config_toml` function beside each client generator.

**Failure.** The template is the same four lines with two different values. The Relay `FileUpstreamConfig` is `#[serde(deny_unknown_fields)]`, so a key that drifted in one copy would be a hard parse error on exactly the topology that copy serves, found by whichever agent nobody ran that week.

## The routing learner

### Learning delivery appends first, then clears the mark

**Choice.** On a confirmed delivery, the engine appends `LearningApplied` to the session log, then clears the mark. A failed append skips the clear (`crates/roundhouse-server/src/engine/learning/delivery.rs`).

**Rejected.** Clearing the mark first.

**Failure.** If the append then failed, the mark would be gone and the session would never turn again, so its log would carry an unrecorded delivery forever. With append first, every failure leaves the mark, and the worst case is one redundant recovery apply (answered as duplicates) and one redundant clear. Once the learner store answers `Applied`, it durably holds the entries, so recovery never loses data either way.

### The first update-transaction design failed a model check

An independent Python model of the learner update rules was run against two designs. The scripts are not in the repository, and the model is evidence about the rules, not about the production code.

- **First design: 4 passes, 2 failures.** A lost acknowledgement credited an entry twice (total 3 instead of 2). Cache pricing dropped the write premium (100 instead of 200).
- **Revised two-phase design: 12 cases pass,** including 2,000 randomized trials of overlapping shuffled windows and a store-loss case. In that case entries 10, 20, and 30 were applied, and the store then went back to the state after 20. `ChainGap` returned watermark 20, and backfill restored a total of 10.
- **Mutations.** A batch-level watermark broke the lost-acknowledgement, gap, and overflow guards. Removing the `prev_seq` check broke only the gap guard, and removing staging broke only the overflow guard. In the Rust memory store, the batch-level-watermark mutation does not break the overflow case, because staging there is independent of the watermark rule, so that row of the model table does not transfer.

See [The routing learner](../concepts/routing-learner.md).
