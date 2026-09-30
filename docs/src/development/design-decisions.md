# Design decisions

This chapter records the alternatives that Roundhouse rejected and the results that did not work. Each entry gives the choice made, the rejected alternative, and the failure that the alternative would have caused. The entries are grouped by subsystem.

External facts in this chapter name the revision they were read from. The dependency pins themselves are in [Upstream dependencies](upstream.md).

## Product boundary and topology

### Roundhouse keeps a durable log, and there is no proxy-only mode

**Choice.** Two modes ship. The ephemeral mode has no Redis: one node, state in the process, and a restart survived by refusal and re-derivation. The durable mode puts Redis behind every state family that has a durable implementation. One switch chooses between them, `ROUNDHOUSE_REDIS_URL`, and no second predicate. See [Deploy with Redis](../operations/redis.md).

**Rejected.** A mode that keeps no log.

**Why.** A mode without a log deletes four promises:

- Replay and audit. The Relay-format exports are cold replays of a finished session.
- Exact settlement. A settle is driven again by the replay a session does when it next opens. Without a log, a process that died between dispatch and settle would leave spend that is never applied.
- Drift reconciliation between the committed ledger and the measured log.
- Steering. The correction is a conversation item.

It also reduces three more promises to guesses. Prefix admission has nothing to compare a resent history against, so fork detection and the warm-prefix premise lose their trigger. An idempotent retry across a reconnect becomes "run the turn again and bill twice". What would remain (auth, tenancy, routing, fair use, grants, pass-through credentials, MCP tools) is the product of NeMo Relay.

### Roundhouse serves the Messages API over its own log

**Choice.** `/v1/messages` is a native surface over the same log as `/v1/responses` (`crates/roundhouse-server/src/messages_api.rs`). Roundhouse has no server-side tool loop. The client runs its own tools on both surfaces.

**Rejected.** Sending Claude Code traffic through the Messages surface of vLLM agentic-api, or through the `ANTHROPIC_BASE_URL` path of Relay.

**Why.** In every topology where the turn does not pass through Roundhouse, Roundhouse owns nothing about the turn: no session, no prefix admission, no pricing, and no steering. The Relay `ANTHROPIC_BASE_URL` path is an interception mechanism, and it still needs a Messages-speaking upstream to point at.

### Roundhouse is not a Relay plugin

**Rejected.** Running Roundhouse as a plugin component inside NeMo Relay.

**Why.** The Relay runtime is process-global, in memory, and request-scoped. The fenced lease, the durable `seq`-ordered log, prefix admission, and crash replay have no home in a middleware callback. The Relay response cache explicitly bypasses stateful requests (`store`, `previous_response_id`, `conversation`, `container`, and any Responses body without `store: false`; `nemo-relay-adaptive-0.8.2` `response_cache/key.rs:128-147`). So every turn that Roundhouse hosts would be outside the jurisdiction of Relay. The one precedent for a routing plugin, `nemo-relay-switchyard`, was deprecated in the release that would have hosted Roundhouse.

### Nobody in the surveyed neighbors owns the turn

A survey of four trees found that Roundhouse duplicates almost nothing: NeMo Relay (`ca08901`, and 0.8.2), Switchyard (`5341f71`, rechecked at `053a61e`), vLLM agentic-api (`d59d4b4`), and the Kubernetes Gateway API inference extension (`84436a9`). Relay owns the harness. Switchyard owns the route and the proxy. agentic-api owns server-held state for one stateless vLLM upstream. The Gateway API extension moved its scheduler to llm-d.

| Absent from Relay and Switchyard | Evidence |
|---|---|
| A fenced, single-writer, append-only log per session | The Relay bus is an in-memory fire-and-forget subscriber bus. The nearest Switchyard thing is a flat JSONL routing log with no `seq` and no lease. |
| Conversation ownership and delta admission | The Relay `response_cache` bypasses stateful requests. |
| Tenancy, budgets, and spend | A Switchyard grep for `budget\|quota\|tenant\|principal\|spend` finds only retry budgets, token budgets, and review counters. Neither tree has dollars in its Rust code. Relay has one credential concept, a per-invocation loopback proxy token (`nrp_`). |
| Steering by injection | Relay hook adapters always allow. Relay can block a tool but cannot inject one. Switchyard can append text but cannot inject a tool call. |
| An MCP control surface | The Relay MCP server answers `tools/list` with `[]`. It exists only to keep a shared gateway alive. Switchyard has no MCP server or client at `053a61e`. |
| Shadow, placebo, or observe-only judge arms | None in either judge algorithm. |
| A capability gate | `quality_prior\|capability_band` has zero hits in both trees. The Relay `baseline_model` is not gated. |
| Realized cache evidence | The Relay adaptive cache planner plans from an assumed `expected_reads`. |
| Deduplicated turn identity and byte-identical replay | Not present. |

Relay 0.8.2 makes the opposite state choice on purpose. The gateway is a single-process, loopback-only proxy that stops itself when idle. Its session table is `Arc<Mutex<HashMap<String, Session>>>`, swept 30 s after the last activity. Correlation hints live 300 s and clear at each turn boundary. A grep across all seven Relay crates finds no conversation log, no embedded database, and no prefix admission. It also finds no idempotency key, no resumption cursor, no writer lease, no spend ledger, and no read-back route. `RoutingPolicy.session_cost_cap` is declared and never read. On restart, the session state is lost and the trajectory file is kept. Relay treats the request as the conversation, and treats any protocol with server-held conversation state as outside its jurisdiction. The durable log of Roundhouse is the answer to "the state has to live somewhere".

Some pieces overlap. The Relay pricing catalog is richer than `ROUNDHOUSE_CATALOG`: it has tiered rates, aliases, prompt-cache pricing, `pricing_as_of`, `pricing_source`, and a validating CLI. `ROUNDHOUSE_CATALOG` does not record provenance. Relay also stamps `x-dynamo-session-id` and `x-dynamo-parent-session-id` on outbound requests and sends an `x-nemo-relay-adaptive-agent-hints` header. Roundhouse consumes neither.

### One continuation contract: full resend with prefix admission

**Choice.** The client resends the whole transcript, and Roundhouse admits the prefix against its stored log. The history binds by `thread-id`, then `session-id`, then `prompt_cache_key`. `ResponsesRequest` has no `previous_response_id` or `conversation_id` field. See [Sessions and the event log](../concepts/sessions.md).

**Rejected.** A second continuation key by delta upload, the way agentic-api does it (`previous_response_id` or `conversation_id` plus only the new input).

**Why.** A client cannot both omit the history and send it, so the two contracts cannot both be authoritative on one path. Prefix admission is the product: transparency and exact pricing depend on it. At agentic-api `d59d4b4` and `e35fbb2`:

- The inverse topology cannot work. agentic-api cannot point at Roundhouse as its `llm_api_base`, because its `UpstreamRequest` has no `prompt_cache_key`, and Roundhouse refuses a request with no conversation identity.
- The only coherent stack is Roundhouse in front and agentic-api behind it, on its stateless raw-proxy branch. That branch is taken only while Roundhouse renders tools as plain functions. agentic-api switches to its own executor on any non-`Function` tool, and a Codex turn carries `namespace` and `custom` tools.
- The default `db_url` of agentic-api changed at HEAD from "stateless, proxy everything" to "create a local SQLite store silently".

### In front of a Gateway API gateway, never an InferencePool backend

**Choice.** If a Kubernetes Gateway API gateway fronts Roundhouse, Roundhouse is a plain `Service`. An `InferencePool` can be at most one local target, used only for discovery and health, with `endpointPickerRef` unset (legal since v1.5.0 of the extension). The repository ships no Dockerfile, chart, manifest, or Kubernetes code.

**Rejected.** Roundhouse as an `InferencePool` backend behind the gateway.

**Why.**

- Behind is incoherent. A gateway picks a pod per request, but Roundhouse pins a session to a fenced single-writer lease. Turn n+1 can land on a pod without the lease. The picker protocol has no way to say "keep this conversation on that writer".
- Behind is also pointless. The picker is valuable for prefix-aware pod choice, but with Roundhouse the client stops uploading the prefix, so a picker downstream would see only the delta.
- An `InferencePool` cannot name a frontier model. `spec.selector` matches Pods by label in one namespace, endpoints are `podIP:portNumber`, and the picker reference forbids `ExternalName` Services. The local-and-frontier half of the product cannot be expressed. GEP-4894 "Backend Resource" (Experimental, `ExternalHostname`) had no CRD in `kubernetes-sigs/gateway-api:apis/` at the read.
- The ext-proc model buffers the whole body at the gateway before it routes. That is hostile to agent turns of 100k tokens.
- A gateway in front can route around Roundhouse, change who pays, or re-encode the history. It needs the same guards as a chained Relay before it counts as supported.
- Multi-replica session affinity would have to come from Gateway API session persistence. The prefix-cache proposal of the extension rejected session affinity.

### Roundhouse is not an endpoint picker

**Rejected.** Implementing the Envoy ext-proc endpoint picker in Roundhouse. It is buildable, and Roundhouse would pick better than the reference picker because it can ask Dynamo for real overlap.

**Why.**

- The return value is an `ip:port`, so "this turn went to Anthropic" cannot cross the seam. Half the product would be lost.
- The protocol is per request and leader-elected. The session lease, the log, and prefix admission would have no home.
- It puts a full-body buffer and a parse of a 100k-token context on the gateway path of every turn. That is the cost Roundhouse exists to remove.
- A narrow picker that asks the Dynamo selection service for a pod duplicates the router that the pod already runs.

The scheduler rule is met by construction: Roundhouse embeds the Dynamo KV router rather than writing a second scheduler.

### Roundhouse forwards the credential of the caller

**Choice.** Roundhouse forwards the agent's own credential (a stored key, or a forwarded login seat) and stamps a payer. It never holds the seat (`crates/roundhouse-fleet/src/openai_responses.rs`). See [Configure tenancy and keys](../guides/tenancy.md).

**Contrast.** Ramp Router (`https://api.router.com/v1`, read 2026-08-20) is custodial. The application holds one Router key, and Router holds the provider credentials. Its bring-your-own-key option is also custodial, and it is not available for Anthropic or Google Vertex AI. Router fronts hosted providers only. Its FAQ says: "Ramp does not host the model weights." So there is no path to a customer-owned GPU.

A method worth keeping from that read: every unauthenticated path on Router returns 401, including a nonsense path, so a 401 alone proves nothing. Only the two `/v1/messages*` paths answered in the Anthropic error envelope. That proved a dialect-aware route.

### No gateway degrades to local, and none exposes routing-control tools

A survey of LiteLLM, OpenRouter, Portkey, and Kong found no gateway that falls back to a free local tier when a budget runs out. Every fallback chain ends in another hosted, metered model. So degrade-to-local is new, and it follows from the accounting. No prior art was found of an inference gateway that exposes agent-callable tools to control its own routing or budget. Every vendor with an admin plane separates the admin credential from the model credential by its shape (OpenAI `sk-admin-` and `sk-proj-`, Anthropic `sk-ant-admin-` and `sk-ant-api03-`). That is the precedent for `rh_admin_` and `rh_turn_`. See [Control plane](../concepts/control-plane.md).

## Wire surfaces

### The Anthropic wire module is hand-written, typed where Roundhouse reads, open everywhere else

**Choice.** `crates/roundhouse-fleet/src/anthropic_messages/wire.rs` is written by hand and shared by the dispatch client and the serve surface. It is typed where Roundhouse reads or creates data:

- the SSE event set, including `ping` and a mid-stream `error`.
- `Usage` with the full `cache_creation` 5m and 1h breakdown.
- `stop_reason` as an open enum with an `Other(String)` arm.
- the content blocks that map to conversation items.
- `cache_control` with the real 5m and 1h TTL vocabulary.

Everything else is open. Unknown request fields, block types, and betas pass through verbatim in `#[serde(flatten)]` extras or opaque JSON.

**Rejected.** `deny_unknown_fields` anywhere in the module.

**Why.** The serve surface is also a pass-through. A client field newer than the pinned spec would make admission fail closed on a request that Roundhouse must pass through.

### No Rust crate carries the Messages wire correctly

Anthropic has no official Rust SDK. The official list names Python, TypeScript, C#, Go, Java, PHP, and Ruby. Anthropic owns no crate on crates.io. The candidates, read from crates.io at the versions named:

| Crate | Why it fails |
|---|---|
| `siumai-protocol-anthropic` 0.11.0-beta.10 | Client direction only (no `decode_request` or `encode_response`). It speaks a neutral form, not Anthropic structs. |
| `claudius` 0.33.0 | `StopReason` and `ContentBlock` are closed enums. It lacks 6 of the 12 response blocks, so real traffic hits its unknown-variant error. No TTL on `cache_control`, no `error` stream event. Default features pull OpenSSL and a REPL. |
| `adk-anthropic` 2.1.0 | Declares a non-existent `cache_creation_input_tokens_1h` instead of the nested `cache_creation` object. The cache counters would be silently wrong. |
| `shunt-gateway` 0.32.0 | Untyped `serde_json::Value` surgery. It cannot create responses, which Roundhouse must do for local turns. |

No surveyed crate models `ephemeral_5m_input_tokens` or `ephemeral_1h_input_tokens` in either direction. `dynamo-protocols` 5.4.0 has no Anthropic types. The Relay Anthropic codec is also lossy (see [Upstream dependencies](upstream.md#the-relay-anthropic-codec-is-lossy)).

### The spec is a vocabulary oracle, not a type generator

**Rejected.** Generating the wire types from the Anthropic OpenAPI spec.

**Why.** The spec is strict in one direction only. It has 693 `additionalProperties: false`: 149 on request schemas, 544 on other input shapes, and 0 on response schemas. `CreateMessageParams` and `BetaCreateMessageParams` are both closed. A faithful generator would put `deny_unknown_fields` on the serve side. No crate offers a switch to suppress it. `ping` and `error` would still need hand-written types. So the spec is pinned only to check field spellings, the same way the ATIF field names are pinned.

### The stored tool name is not migrated

**Choice.** A forward-only optional `namespace` field on the stored tool call (`crates/roundhouse-core/src/item.rs`).

**Rejected, in three shapes.**

- A one-time rewrite. It cannot recover a namespace that was never written, so it can only guess which bare `status` was ours. The name is inside `Item::render`, and the dedup key is the turn id, so a rewrite would move the turn id of every conversation with a control call. An in-flight retry would then miss its own completed response and buy a second billed answer.
- Canonicalizing at read time. That is the existing name-list recognizer under another name.
- A versioned record tag. That is a new variant on an internally tagged enum, which an older build cannot read.

A Redis stream entry cannot be written over in place anyway.

## Sessions and prefix admission

### Prefix admission probes, then commits once

**Choice.** `probe` asks one generation one question and writes nothing. `bind_prefix` searches with it and commits exactly once, after the home is known (`crates/roundhouse-server/src/prefix_admission.rs`). A refusal commits nothing, so a verbatim retry probes the same generations and gets the same refusal.

**Rejected.** Forking on every probe attempt: moving the counter of the key and `latest` before anything was known.

**Why.** That shape had six defects:

- A refusal left the counter and `latest` on a generation that no turn ran on.
- A verbatim retry resumed past the bound and was admitted whole a few forks later. Claude Code retries a 409 without condition.
- The reported count was the constant, not what was probed.
- A search only upward from the counter of this node duplicated a prefix that an older, agreeing generation already held.
- An empty generation that another node was mid-turn on was taken whole, and the turn died on the lease.
- The admission step was written twice.

### The chain unit is the message, not the token block

**Choice.** The prefix chain has one link per item, at message boundaries (`crates/roundhouse-core/src/item/chain.rs`).

**Rejected.** Hashes of token blocks as the chain unit.

**Why.** Message boundaries do not depend on the model, so one chain serves every local and frontier target, and it is computed before a target is chosen. Provider caches also match exact prefixes, and a message boundary is where Roundhouse already places Anthropic cache markers. Block hashes need the tokenizer and block size of each pool, so a longest match across pools would need one chain per tokenizer. The cost is granularity: an edit inside an item loses the whole item. On the Claude Code fixtures, turn 1 and turn 2 share 5,096 bytes at byte level and only 162 bytes (2 items) at item level. That under-claims overlap, which is the safe direction. Inside one Dynamo deployment, the KV router already matches blocks.

### The anchor is not a hash of the first N items

**Choice.** Per-item links with a longest match. The anchor of a sequence is the last tip of the whole first prompt (`crates/roundhouse-sequence-id/src/keyed.rs`).

**Rejected.** A hash over a fixed number N of leading items.

**Why.** N differs by client and version: 4 items for Codex, 5 or 6 for Claude Code. One Claude Code session also changed its own item 2 between turn 1 and turn 2: a second process started with `--continue` removed ` (1M context)` from the system prompt. So a hash computed again at any N of 3 or more drifts inside one session. A hash stored once is only the label binding again. A longest match needs no knowledge of the client and also gives the overlap in tokens, which a price needs. The first request of any client ends just after the first typed prompt. So the tip of the whole first request already marks the items that make a sequence unique.

### One identity per KV lineage, not per agent task tree

**Rejected.** One keyed "program" id for a root conversation and all its sub-agents. Three mechanisms were considered: a hash of the root prefix, declared lineage headers, and an emitted-item link (a new conversation that resends a tool-call id Roundhouse emitted joins that program).

**Why.** The routing unit is KV overlap, not the agent task tree.

- A fresh spawn or a partial fork shares little or no prefix, so one id across them does not help routing. A sub-agent that starts a new root is a new request.
- A root-prefix hash misses every fresh spawn and every partial fork, which drops the root user item.
- A root-prefix hash also joins unrelated sessions of one client version, which share a system prompt and tool preamble.
- Client-specific headers can only speed a match up, and only when the client is detected with very high confidence.

### The sequence-identity crate has one API per client

The shape of `crates/roundhouse-sequence-id` is `messages_label(headers, user_id)` for Messages, and `CodexHeaders::read` then `.label(cache_key)` for Responses.

- **Rejected: one input type and one `label()` for both surfaces.** It forced the Messages caller to map an error that cannot occur, and the Responses caller to refuse an `Anonymous` that cannot occur. Replacing either arm with `unreachable!()` left every suite green. It also kept a private label copy that a test showed can disagree with `RequestContext`.
- **Rejected: a product type `Detection { client, confidence }`.** It admitted nine shapes where `detect_client` returns four. It is a sum type.
- **Rejected: private `RequestContext` fields.** That forced conflicting edits in `engine.rs`. The fields stay public, and `conversation_key()` is computed from them.
- **Rejected: `prefix_fingerprint(chain, items)`.** A mismatched but long-enough chain was silently fingerprinted instead of the items. It takes `items` only.
- **The crate is pure:** no store, network, clock, environment, or async. A digest compared across nodes must be the same on every node, and a stray `now_ms()` would silently break that. The server keeps `anonymous_key` and `qualify`.

## Routing

### One engine, with per-project policy as data

**Choice.** Model access and policy per project arrive as data on one engine: `TurnPolicy` in `RoutingContext` (`crates/roundhouse-core/src/routing/mod.rs`).

**Rejected.** One engine per project.

**Why.** It would fork the metrics fold, the turn gates, and the catalog.

### Switchyard's escalation classifier is not the escalation policy

**Choice.** A periodic `audit_every` policy: serve most turns from the cheaper pool, and send every Nth turn to the best target (`crates/roundhouse-core/src/routing/policy.rs`).

**Rejected.** The Switchyard escalation route as `EscalationPolicy`. Read at `5341f71` and `053a61e`, it works like this. It calls the weak target and buffers the reply. A judge rules on the completed turn. An escalate verdict increments a streak. The buffered weak reply is served while the streak is below `confirmations` (default 2). At the threshold, the weak reply is discarded and the strong target serves. A latched session goes straight to strong with no judge call.

**Why.**

- It is reachable only through `LlmTaskClassifier::new(LlmClassifierConfig::Escalation{..})`, and it owns the call to the efficient tier. It needs dispatch control that the pure `choose` seam does not give.
- Its streak lives in `State.extra`, a process-local map with a one-hour TTL. A crash and replay loses the latch. Roundhouse derives state from the log.
- The discarded efficient turn is paid for and never accounted.
- Context overflow and transport failures on the efficient tier fall through to capable silently. That is a fail-open on cost.
- An unlatched turn waits for the weak call, then the judge call: two serial upstream round trips per turn. Upstream itself says not to use it for latency-critical traffic.
- It needs a session identity (`x-switchyard-session-id`), or it never latches.

Its outage handling agrees with Roundhouse: a judge that errors, times out, or returns something unparseable holds the streak instead of clearing it.

### Background classification, per-turn local selection

**Choice.** Model selection runs on every turn from fast local logic, with the cache state at each destination as an input. The Jev classifier runs in the background, off the routing path, and its classifications feed features for later turns. The arms of the learner are serving strategies (`rules`, `efficient`, `capable`), not targets. A cache segment does not lock selection. See [The routing learner](../concepts/routing-learner.md).

**Rejected.**

- A classifier at the `Ambiguous` arm of `pick_tier`, under its own deadline on the turn path, failing open to the default tier. That arm is where upstream Switchyard consults a classifier. Roundhouse takes the default tier there (`crates/roundhouse-core/src/routing/stage.rs`), so routing makes no model calls.
- A bandit whose arms are targets, deciding once per cache-cold segment (session start, fork, TTL expiry, forced failover).
- Allocating a strategy at cache-cold segment boundaries, with background arms that hold their own probability mass.

**What survives from the rejected designs.** Guardrails bind before the policy. The draw is a hash, not an RNG, so it replays from the log (the `routing-explore` draw domain in `crates/roundhouse-core/src/routing/learn/explore.rs`). The posterior is configuration. A background-only arm never consumes serving probability mass. The validation `Arm` is not reused for routing strategies, because that would mix two independent assignments.

### `switchyard-libsy` stays behind `RoutingPolicy`

The costed reasons are in [Upstream dependencies](upstream.md#why-no-dependency-on-switchyard-libsy).

## Validate and steer

### The steer is text, not a synthetic tool call

**Choice.** The steer is an assistant text instruction. `Auto` means text. `SteerChannel::ToolCall` stays in the enum only so the configuration refusal can name it (`crates/roundhouse-core/src/validate/verdict.rs`). See [Validate and steer](../concepts/validate-steer.md).

**Rejected.** Completing the held turn with a synthetic `function_call` to `mcp__roundhouse` `fetch_steer`. That design was built and run against `codex-cli 0.146.0`. Its shape:

- `namespace` as its own field, and an `fc_`-prefixed item id.
- `call_id` equal to the steer id.
- `arguments` created once and never serialized again.
- Four frames (`created`, `added`, `done`, `completed`) with no argument deltas.

**Why.** It worked mechanically. Codex dispatched the call over the real `/mcp` (an `rmcp` 1.8.0 client against the `rmcp` 3.1.3 server), appended the output, and resent both, and the session did not fork. But under `codex exec`, `approval_policy` is forced to `never`, and a tool with no MCP annotations is treated as destructive and open-world. So the first real steer was answered "user cancelled MCP tool call", while the log recorded a fulfilled steer the agent never saw. The channel has two cooperation points that can fail silently: the client must dispatch the call, and the model must obey the output. A text answer needs only a client that can read. The tests that proved the dispatch round trip were deleted with the emission code.

### Switchyard's advisor gate is not the validate loop

**Rejected.** Switchyard `AdvisorGate` (route type `advisor`) in place of the validate loop. One executor serves every turn, and a judge-only advisor reviews terminal turns. APPROVE releases the buffered turn. REDO discards it and feeds the plan of the advisor back. It is in no released version.

**Why.** Its `route` calls the executor itself (`driver.call_model(...)`), so it would replace the turn loop of the engine, not fill a hole in it. The Roundhouse interjector is consulted before the turn is planned, cannot see the candidate list, and cannot dispatch, and `choose` must be pure. Row by row, Roundhouse would lose:

| Roundhouse | Switchyard `AdvisorGate` at `5341f71` |
|---|---|
| Review budgets projected from the log, stable across replay | Re-arms on process restart. `Mutex<GateState>` with eviction at 1024 scopes. |
| A half-open breaker with `max_in_flight = 8` | Hard-coded `MAX_FAILED_CONSULTS = 3`, no in-flight cap, no re-arm |
| A bounded, step-indexed brief with a declared objective | Serializes messages and drops the middle |
| A strict typed JSON verdict with `deny_unknown_fields` | An anchored regex scans prose for APPROVE or REDO |
| Judge prose never reaches the agent | The REDO plan goes to the executor verbatim |
| Four actions (Continue, Escalate, Steer, Halt) clamped through the narrow-only policy lattice | REDO only |
| Live, Shadow, and Placebo arms | No arms |
| Dollar accounting of the judge call | Tokens only. No dollars anywhere in the Rust tree. |
| No fail-open. A timed-out validator is marked, never free. | `fail_open` defaults to `true`, and a failed consult is audited as `verdict: "APPROVE"` |

Taken as ideas: accounting for discarded work, the anchored verdict parse, and the stall checkpoint. One gap remains: the judge asks for JSON in the prompt only, while Switchyard sets `response_format` from a JSON-schema contract. A parse failure costs a wasted consult and can trip the breaker.

## Launchers

### `topham` is its own crate above the server

**Choice.** A new workspace crate, `crates/topham`, above `roundhouse-server` in the dependency graph. A launcher belongs above the composition root. Key minting is a subcommand over the existing admin route, not a new route. See [Launch with topham](../guides/topham.md).

**Rejected.**

- A `[[bin]]` subcommand inside `roundhouse-server`. That puts an argument parser in the composition root, whose rule is "configuration is environment variables".
- An admin-plane read beside key minting.

The Relay 0.8.0 launcher is used as it is for the chained topology. It has no TUI, no named profiles, and no vocabulary for models, routes, keys, or budgets. Its config has four sections, and none names a model.

The name comes from Sir Topham Hatt, the Fat Controller, who decides which engine runs which route.

### Direct is the reference topology

**Choice.** The Direct topology is the reference: the agent points straight at Roundhouse, and Roundhouse generates its own minimal client configuration (`crates/roundhouse-server/src/codex_launch.rs`, `crates/roundhouse-server/src/claude_launch.rs`). The Relay CLI is the supported instrumented front end for the chained topology. The same generated client configuration serves both, because Relay overwrites the base URL and merges its headers rather than replacing them.

**Rejected.** Delegating client launch to one of the other two implementations of the same surface. Relay ships 6,301 lines of Rust launchers and Switchyard 2,503 lines of Python launchers (line counts of the launcher sources only, at `ca08901` and `5341f71`).

**Why.** The real-binary suites must prove what a real client does against a configuration that Roundhouse wrote. A launcher that Roundhouse does not own would put that proof outside the tree. The Switchyard launcher was reference only: it has no hooks, and it would be a third stack to guard. Its `caller_auth_kind` conditional is what the two auth kinds of `codex_launch` mirror. Switchyard later deleted its launchers.

### One Relay handoff module for both agents

**Choice.** `crates/roundhouse-server/src/relay_handoff.rs` renders the Relay configuration for both agents from one template, parameterized by `RelayAgent`. See [NeMo Relay formats](../operations/relay-formats.md#aim-a-nemo-relay-at-roundhouse).

**Rejected.** A `relay_config_toml` function beside each client generator.

**Why.** The template is the same four lines with two different values. The Relay `FileUpstreamConfig` is `#[serde(deny_unknown_fields)]`. A key that drifted in one copy would be a hard parse error on exactly the topology that copy serves, found by whichever agent nobody ran that week.

## The routing learner

### Learning delivery appends first, then clears the mark

A review claimed that clearing the mark before appending `LearningApplied` can lose a needed recovery. The claim is invalid. Once the store answers `Applied`, it durably holds the entries. The mark only tracks whether this session log recorded the delivery, and recovery never appends. An early clear only means that recovery skips a session the store already has.

Append-first was kept as a cost choice (`crates/roundhouse-server/src/engine/learning/delivery.rs`). Keeping the mark on every failure branch costs at most one redundant recovery apply (answered as duplicates) and one redundant clear. That is cheaper than a session that never turns again and carries an unrecorded delivery in its log forever.

### Model checks of the update transaction

An independent Python model of the rules of the learner update transaction was run against two designs. These are models of the rules, not evidence about the production code, and the scripts are not in the repository.

- **First design: 4 passes, 2 failures.** A lost acknowledgement credited an entry twice (total 3 instead of 2). Cache pricing dropped the write premium (100 instead of 200).
- **Revised two-phase design: 12 cases pass.** These include 2,000 randomized trials of overlapping shuffled windows, and a store-loss case. In that case entries 10, 20, and 30 were applied and the store then went back to the state after 20. `ChainGap` returned watermark 20, and backfill restored a total of 10.
- **Mutations.** A batch-level watermark broke the lost-acknowledgement, gap, and overflow guards. Removing the `prev_seq` check broke only the gap guard. Removing staging broke only the overflow guard.
- In the Rust memory store, the batch-level-watermark mutation does not break the overflow case, because staging there is independent of the watermark rule. That row of the model table does not transfer.
