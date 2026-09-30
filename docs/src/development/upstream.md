# Upstream dependencies

Some dependencies are the other half of the product, not ordinary libraries: Dynamo, the Codex crates, NeMo Relay, Switchyard, redis, and the Anthropic Messages spec. This chapter gives, for each one, what Roundhouse takes, the exact pin, the revision each external claim was read from, and what unlocks a move.

The manifests are the source of truth for every pin: `Cargo.toml` and `crates/*/Cargo.toml`. Each exact pin carries its reason and unlock condition in a comment beside it.

## Summary

| Dependency | What Roundhouse takes | Pin | Unlock condition |
|---|---|---|---|
| Dynamo | `dynamo-kv-router` (feature `standalone-selection`), `dynamo-tokens`, and `dynamo-mocker` (tests only) | git rev `ac7b7513790ef1d619b46f805aea03c9f21200ba` | A crates.io release of `dynamo-kv-router` that exports the embeddable `SelectionService` |
| Codex | `codex-api`, `codex-client`, `codex-protocol`, `codex-utils-rustls-provider`, dev-dependencies only | git rev `6344a655a5966f92e009a74928fb0559b41f9093` | None. A move is a re-read of every wire claim. |
| NeMo Relay | `nemo-relay-types` (ATOF and `LlmOptimizationSummary` types) | `=0.7.3` | A `nemo-relay-types` release that relaxes its own `uuid = "=1.18.1"` |
| Switchyard | No crate. A ported scorer, a ported signal extractor, and an adapted judge prompt | The revision named in each file's attribution | None |
| redis | `redis` client | `1.2`, resolves to 1.2.4 | Dynamo's `tokio` pin reaches 1.51 |
| Anthropic Messages spec | The vocabulary of the hand-written wire module | `spec_pin.json`, SDK rev `4140e0ea` | None. A refresh is a sync run. |

## Dynamo

Roundhouse depends on Dynamo but is not part of it. It builds independently.

| Crate | Use | Where |
|---|---|---|
| `dynamo-kv-router` | The embedded selection service, with `default-features = false` and `standalone-selection` | `roundhouse-core`, `roundhouse-fleet` |
| `dynamo-tokens` | Token block hashing | `roundhouse-core`, `roundhouse-fleet` |
| `dynamo-mocker` | A mock engine that simulates vLLM or SGLang scheduling and prefix caching without GPUs, and publishes KV events over `zmq-events` | `roundhouse-server` dev-dependency |

All three resolve at version 1.4.0 from git rev `ac7b7513790ef1d619b46f805aea03c9f21200ba` of `ai-dynamo/dynamo`. The mocker lets the cache-hit path be measured rather than asserted (`crates/roundhouse-server/tests/mocker_cache_hits.rs`).

**Why a git rev.** The newest published `dynamo-kv-router` (1.3.1) predates DEP #10321. It does not export the embeddable `SelectionService`, and it says `tenant_id` where the pin says `routing_group`. The git dependency also resolves `dynamo-truthy`, which the workspace needs and which is not published.

**Not depended on.**

- `dynamo-llm`. Its default `block-manager` feature pulls `nixl-sys` and `cudarc`, which would bring CUDA into a service that never touches a GPU.
- `dynamo-runtime`. `dynamo-kv-router` under `standalone-selection` has none, and Dynamo enforces that in `lib/kv-router/src/services/CLAUDE.md`. The cost is that Roundhouse cannot read the Dynamo event plane.

**Transitive pin.** `dynamo-mocker` at the pin requires `tokio = "=1.48.0"`, and so does Dynamo main. The workspace declares the caret `tokio = "1.48"`, which resolves to 1.48.0. An exact pin here would restate the Dynamo constraint where nobody can see whose it is. This pin is also the ceiling on redis (see [redis](#redis)).

### What Roundhouse sends to Dynamo

- The embedded fleet passes the Roundhouse session id as `SelectRequest.session_id` (`crates/roundhouse-fleet/src/local.rs`). It always sets `pinned_worker: None` and `allowed_worker_ids: None`.
- `SequenceDigest` (`crates/roundhouse-core/src/sequence.rs`) is documented as the `x-dynamo-session-id` value. No dispatch path sends that header.
- Roundhouse reads `x-dynamo-session-final: true` from a client as a signal (`crates/roundhouse-sequence-id/src/signals.rs`). It sends no session-final request to Dynamo.

Worker-local scheduling and physical KV residency belong to the serving plane (Dynamo, and KVBM inside a worker). Roundhouse sends only advisory, idempotent hints. It never names a block to evict and never releases one. Two cache authorities in one request path would contradict each other. For the same reason, llm-d is not used as a scheduler or as a second index of cache residency.

### Facts at the pin

Read from Dynamo at `ac7b751`, unless a line says otherwise.

- **Session identity is passive.** `session_id` travels through `SelectionOperation`, `ScheduleRequest`, and `SchedulingRequest`, and nothing branches on it. `pinned_worker` and `allowed_worker_ids` are read for worker selection.
- **Headers.** `x-dynamo-session-id` wins over every client header. The agent header map takes the Codex `session-id` as the session, so a whole Codex agent family is one Dynamo session. For Claude Code it takes the agent id and falls back to the session header. `x-dynamo-worker-instance-id` pins a decode worker, and Roundhouse does not send it.
- **Session affinity** pins a session id to a worker for a TTL. It is off unless `--router-session-affinity-ttl-secs` is set, and it holds at most 65,536 entries.
- **Nothing releases the KV of one session.** No endpoint takes a sequence id and releases its blocks. `x-dynamo-session-final: true` becomes `kv_hints.evict_session`, which only tests read, and a Python ThunderAgent router drops its scheduler state on it. `clear_kv_blocks` is a per-worker control on the worker system port (`/engine/control/clear_kv_blocks`). It flushes the whole prefix cache (vLLM `reset_prefix_cache(reset_connector=True)`), and SGLang refuses it while a request is active. A live session-final request costs a scheduling slot, a prefill, and one decoded token, and frees nothing. That is why Roundhouse sends none.
- **The admin API is on by default and has no authentication**, including `/busy_threshold`, at `ac7b751` and at main `6822bab`.
- **Overload.** When every worker of a model is over its busy threshold, the frontend answers with `DYN_HTTP_OVERLOAD_STATUS_CODE` (default 529). The busy gate is off unless a threshold is set. Nothing under `lib/llm/src` sends `Retry-After`.
- **Cache-hit reporting.** The frontend copies `usage.prompt_tokens_details.cached_tokens` from the backend `completion_usage`, which a decoder can mark `CacheReadSource::Provider`, the only source that cache evidence counts as measured. `nvext.extra_fields: ["worker_id"]` returns `prefill_worker_id` and `decode_worker_id`. `["timing"]` returns a per-request `kv_hit_rate`, which is the router prediction at routing time, not an engine measurement. The same ratio feeds the `router_kv_hit_rate` histogram, which frontend `/metrics` includes only when `drt_metrics` is set. The mocker does not fill `cached_tokens` at the pin. Its prefill time comes from `isl - prefix`, so its time to first token falls on a cache hit even when its usage reports no cached count.
- **No single KV-load number for an outside caller.** `/health` lists instances, `/live` is liveness, and `/busy_threshold` returns thresholds, not loads. The gauge `dynamo_frontend_worker_active_decode_blocks` (labels `worker_id`, `dp_rank`, `worker_type`) is a prediction that exists only when the frontend runs the KV router. With the defaults `router_track_output_blocks: false`, `router_assume_kv_reuse: true`, and `router_replica_sync: false`, it under-counts decode growth, counts shared prefix blocks once, and sees only its own frontend. `dynamo_frontend_model_total_kv_blocks` has only a `model` label, and the last writer wins. The engine-reported `kv_used_blocks` travels only on the event plane (`kv_metrics` subject), which needs a `DistributedRuntime`. The standalone `GET /loads` gives per-worker `potential_prefill_tokens`, `potential_decode_blocks`, and `active_requests`, with no capacity. Unknown: whether vLLM `kv_cache_usage` counts evictable prefix-cached blocks as used.

### What differs at Dynamo main

Read at main `6822bab`. Moving the pin past `ac7b751` brings these changes:

- `KvHints` and `evict_session` are removed (`8b030e0ec1`). `KvHint` now means a typed KV-transfer envelope (`kv.fetch@1.0`).
- `SessionPrefixIndexer` (`adfe2aa1b1`) is off by default and removes sessions only by LRU at 16,384 entries.
- Session affinity has hard and soft modes (`052c1e2853`). A final request refreshes the binding instead of ending it.
- The explicit SGLang `open_session` and `close_session` and the sticky lifecycle module are removed (`5407083c16`).
- The mocker fills `cached_tokens` for its vLLM model from `8970b35248`, and now lives in `aisimulate-core` `=0.12.0`. An HTTP-level cache-hit measurement against the mocker needs a pin at or after `8970b35248`.
- Dynamo reads the Codex `thread-id` as the session, not `session-id` (`3b7675444c`). This has no effect when `x-dynamo-session-id` is sent.
- vLLM (`43b6344b87`) and SGLang (`b2d456f2bd`) receive the session id, so it can appear in engine logs. The ThunderAgent plugin (`cde7c9a8c5`) drops its program entry and then still dispatches the final request as inference.
- `kv_used_blocks` and `kv_total_blocks` are still not gauges, and `tokio = "=1.48.0"` is still pinned.

### Worker recipe for the embedded selector

A worker must publish KV events in the format the selector expects. This recipe is adapted from `examples/backends/vllm/launch/agg_router.sh` at the pin. Read it again on every move of the pin.

```bash
python -m dynamo.vllm --model <model> --block-size 64 --enable-prefix-caching \
  --kv-events-config '{"publisher":"zmq","topic":"kv-events","endpoint":"tcp://*:20080","enable_kv_cache_events":true}'
python -m dynamo.frontend --router-mode kv --http-port 8000
```

- The block size must equal `WorkerRegistration.block_size`. The Dynamo indexer drops KV events whose block size differs from its own.
- Request prefix caching explicitly, so the contract does not depend on a default.
- `PYTHONHASHSEED=0` makes the event hashes of the worker match the hashes Roundhouse computes over a prompt.
- Each DP rank has one ZMQ endpoint (`WorkerRegistration.kv_events_endpoints`).
- The frontend is only for an independent OpenAI-compatible smoke test. etcd and NATS provide discovery, and at the pin their compose file is `dev/docker-compose.yml`. `deploy/docker-compose.yml` does not exist.
- On CUDA 13.x, set `VLLM_USE_FLASHINFER_SAMPLER=0`. The FlashInfer sampler JIT of vLLM fails on version-skewed headers (torch pins 13.0, `tilelang` pulls `nvidia-cuda-nvcc` 13.2).

## Codex

### The conformance crates

`codex-api`, `codex-client`, `codex-protocol`, and `codex-utils-rustls-provider` are pinned to `openai/codex` rev `6344a655a5966f92e009a74928fb0559b41f9093` as dev-dependencies of `roundhouse-server`. The parser a real agent runs checks our `/v1/responses` output (`crates/roundhouse-server/tests/codex_conformance.rs`). `codex-core` is private, so only a real `codex exec` shows how Codex dispatches a tool.

- **Dev-only on purpose.** `codex-http-client` pulls `reqwest` with default features, which brings OpenSSL. The shipped binary is rustls-only, and with resolver v3 the features of a dev-dependency do not reach `cargo build`.
- **Two `[patch.crates-io]` entries.** The Codex workspace needs `tungstenite` features that exist only on the OpenAI forks, and a `[patch]` does not travel with a git dependency. The workspace restates it: `tokio-tungstenite` at rev `0e5b2d73` and `tungstenite` at rev `4fffad30` of `openai-oss-forks`. They affect only the Codex crates. axum's websocket feature stays on the 0.26 series.

### The binary and the pin are different trees

| Name | Revision | What it is |
|---|---|---|
| Binary | `e363b08` (`rust-v0.146.0`, 2026-07-28) | The `codex-cli 0.146.0` binary that the real-binary suite runs |
| Pin | `6344a65` (2026-08-13) | The Cargo pin of the conformance crates |
| Later | `3b45c29` (2026-08-19) | The first of the three to carry the `requires_openai_auth` guard |

The binary and the pin are siblings off merge-base `95637f7`. Neither is an ancestor of the other.

- **The `requires_openai_auth` guard.** `resolve_provider_auth` (`model-provider/src/auth.rs`) is byte-identical at the binary and the pin, with no `requires_openai_auth` gate. `3b45c29` adds `if !provider.requires_openai_auth && provider.auth.is_none() { return Ok(unauthenticated_auth_provider()); }`. So at 0.146.0, `requires_openai_auth = false` with no `env_key` does not stop an ambient credential. The launch generator therefore writes `requires_openai_auth = false` only beside `env_key` (`crates/roundhouse-server/src/codex_launch.rs`). See [Hook up Codex](../guides/codex.md).
- **Validate generated configuration against the binary, not the pin.** `ModelInfo` differs. `config_toml.rs` has `[debug]` and lockfile types only at the binary, and `responses_api_metadata` and `[goals] max_goal_token_budget` only at the pin. 0.146.0 silently ignores `responses_api_metadata`. The pin removes `--full-auto` and adds `codex exec fork`.
- The gated suite (`crates/roundhouse-server/tests/codex_e2e.rs`) states `VERIFIED_VERSION = "codex-cli 0.146.0"`, prints `codex --version` on every run, and warns on a mismatch.

### Wire facts

**MCP tools, at `6344a65`.**

- MCP tools appear in the request `tools` array only as a namespace object, `{"type":"namespace","name":"mcp__roundhouse","tools":[...]}`, never as flat functions (`core/src/tools/handlers/mcp.rs`).
- Dispatch is an exact lookup on `ToolName { name, namespace }` (`core/src/tools/router.rs`). Nothing splits a flat `mcp__server__tool` apart, so a call must carry `namespace` as its own wire field.
- Dispatch happens on `response.output_item.done`. It does not depend on `output_item.added` or on argument deltas.
- An unknown tool name becomes `RespondToModel("unsupported call: …")`. The agent does not crash. Deferred tools are left out of the list the model sees, so an absent namespace does not prove the MCP server is unregistered.
- For streamable HTTP, Codex requires `bearer_token_env_var` and rejects `bearer_token` (`config/src/mcp_types.rs`).

**How a prior tool call is resent, at 0.146.0.** Codex resends a prior `function_call` by structure, not byte for byte. It drops any item `id` without an underscore (`core/src/client.rs`), so `fc_<response_id>` survives. The `arguments` string comes back byte for byte, so tests compare parsed fields and treat `arguments` as the one byte-exact comparison.

**Request layout, at `6344a65`.** Derived from source and tests. No Codex request was captured for this.

- A root thread has four canonical items: 0 `System` (the `instructions`), 1 developer, 2 user (`AGENTS.md` plus `<environment_context>` with working directory, shell, `current_date`, and time zone), and 3 user (the typed prompt). Two sessions diverge at item 3 when working directory, date, and configuration agree, and at item 2 otherwise.
- A multi-agent v2 full-history fork (`fork_turns` default `all`) removes `AgentMessage` items and two developer messages, so the child is not a byte prefix of the parent.
- Codex sends `session-id` on every `/v1/responses` request, at both the binary and the pin.

## Claude Code

Claude Code is a client, not a crate, but its version is watched the same way.

| Version | Why it matters |
|---|---|
| 2.1.42 | The only readable (npm) bundle, and old |
| 2.1.229 | The minimum version the gateway documentation names |
| 2.1.247 | The first native binary that can be spawned |
| 2.1.251 and 2.1.257 | The two client lines the fixtures pin. Every fixture-driven test runs against both. |
| 2.1.272 | A sanitized capture of request shapes (`crates/roundhouse-server/tests/fixtures/claude-2.1.272-wire-shapes.json`). No test loads it. |

The gated suite states `VERIFIED_VERSION = "2.1.257"` (`crates/roundhouse-server/tests/common/claude_rig.rs`), prints the version on every run, and warns on a mismatch. A missing binary is a loud failure. The client updated itself twice in five days during capture work. So each wire claim names the client version it was captured from, and `topham` sets `DISABLE_AUTOUPDATER=1`. See [Hook up Claude Code](../guides/claude-code.md).

## Anthropic Messages spec

Anthropic publishes no Rust SDK, and no crate carries the Messages wire correctly (see [Design decisions](design-decisions.md#wire-surfaces)). So `crates/roundhouse-fleet/src/anthropic_messages/wire.rs` is written by hand. The OpenAPI spec is pinned only as a vocabulary oracle: a test asserts that the hand-written module agrees with the pinned vocabulary.

`crates/roundhouse-fleet/src/anthropic_messages/spec_pin.json` records:

| Field | Value |
|---|---|
| `source_sdk_rev` | `4140e0eaa597c0ad35218ffb20b66ef7fce7f639` (`anthropic-sdk-typescript`) |
| `spec_url` | `https://storage.googleapis.com/stainless-sdk-openapi-specs/anthropic/anthropic-e50cf35b74cc0471a2b5af7ea03765aa81c035f82588e9a1ba1b29aeaa17d064.yml` |
| `spec_sha256` | `d1d189d791d1b551b33edebe9a60d63836d167070863ef366969e0df1e9c4f55` |
| `openapi_spec_hash` | `e4ae88bdd84c7e293dd46037a6769b8d` |
| `fetched` | 2026-09-01 |
| `vocabulary` | The values the wire tests read |

The spec is OpenAPI 3.1.0, published without authentication at a content-addressed Stainless URL that comes from `openapi_spec_url` in the `.stats.yml` of `anthropic-sdk-typescript`. There is no `latest` alias. The hash in the URL and `openapi_spec_hash` are opaque Stainless addresses, not hashes of the body. Only the sha256 that Roundhouse computes over the downloaded body checks integrity. A changed URL means the spec moved. An unchanged URL with a changed body sha256 means a broken download, because the storage is immutable.

The vocabulary at this pin:

- `StopReason` has seven values: `end_turn`, `max_tokens`, `stop_sequence`, `tool_use`, `pause_turn`, `refusal`, `model_context_window_exceeded`. The wire module keeps an open `StopReason::Other(String)` arm for an eighth.
- `Usage` has nine properties. `CacheCreation` is `{ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}`, both required.
- `MessageStreamEvent` has six members and four delta variants. The response `ContentBlock` union has 12 members. `AnthropicBeta` is an open enum, so a bare string is valid.
- `AnthropicBeta` had 41 named values at the first pin and three more at the current one. The new values need no typed arm.
- The spec models the `?beta=true` surface as parallel paths: 140 paths, 124 of them beta duplicates at the current pin, and it carries `x-stainless-*` vendor extensions.

**What the spec does not describe.** The literal `"ping"` occurs zero times in the 2.4 MB spec, and no Messages-stream error event exists in it. Both are real on the wire, so the wire module types and emits them by hand and pins them with `the_two_transport_events_the_spec_omits_are_typed_anyway`. A spec sync must never read their absence as a removal.

**Refresh.** Run the `anthropic-spec-sync` skill (`.claude/skills/anthropic-spec-sync/SKILL.md`). It fetches the spec, diffs the vocabulary, updates the pin, and fixes the code test-first where the API moved.

## NeMo Relay

Roundhouse interoperates with NeMo Relay at the format layer. It emits the published Relay formats (see [NeMo Relay formats](../operations/relay-formats.md)) and supports a Relay in front of it as a deployment topology. It is not a Relay plugin and takes no dependency on the Relay core.

Relay owns the harness: hooks, scopes, redaction, and the tool-level view the wire cannot show. Roundhouse owns the turn: the durable log, prefix admission, policy, budgets, routing, and steering. Dynamo owns the hardware. A topology is a choice per site, and a dependency is carried everywhere. Treating the chained topology as architecture would make its risks permanent: two proxies, stripped credentials, and an upstream override.

### `nemo-relay-types` is pinned at `=0.7.3`

`crates/roundhouse-relay/Cargo.toml` declares `nemo-relay-types = { version = "=0.7.3", default-features = false }`. It is the only Relay crate in the graph.

- **Why 0.7.3 and not 0.8.** The code Roundhouse uses is byte-identical from 0.7.3 through 0.8.0-rc.1, 0.8.0, 0.8.2, and Relay tree `1a54812`. That code is `codec/optimization.rs` (`LlmOptimizationSummary`, `Contribution`, `limitations`) and the whole ATOF envelope. A pin moves for a reason, not for freshness.
- **Why `=`.** A caret against a 0.x crate is not a reproducible statement, and `version = "0.8"` would not resolve to `0.8.0-rc.1`, because cargo excludes pre-releases from ordinary requirements.
- **What it costs.** The crate requires `uuid = "=1.18.1"` at every version, which pulled the whole workspace down from the 1.24.0 its caret resolved to. Every other `uuid` dependent is satisfied (`moka` wants `1.1`, `rmcp` and `ts-rs` want `1`, `rama-http` wants `1.18`, Codex wants `1`, Dynamo wants a `1.18.1` caret). It is a ceiling: the day any dependency needs `uuid >= 1.19`, the workspace stops resolving.
- **Unlock condition.** A `nemo-relay-types` release that relaxes its own `=1.18.1` to a caret. A newer Relay alone does not help, because Relay HEAD pins it exactly too.
- **When 0.8 is the move.** The metric-mark surface (`MetricEnvelope`, `METRIC_DATA_SCHEMA_NAME`, `LogSeverity`) exists only from 0.8. Nothing in Roundhouse emits one. The day something does, the target is `=0.8.0`, with no code change. `default-features = false` keeps the `schema` feature off.

### Ported, never depended on

`nemo-relay` 0.8.0 declares 28 direct dependencies (four `opentelemetry` crates, `tonic`, `object_store`, `reqwest`, `tokio`, and more) and has about 156k lines. `nemo-relay-types` is the only cheap import (`bitflags`, `chrono`, `serde`, `typed-builder`, `uuid`). `nemo-relay-adaptive` and `nemo-relay-pii-redaction` depend on the core, and `nemo-relay-pii-redaction` also pins `sha2 0.11` against the workspace `0.10`. The Relay `PricingCatalog` lives in the core. ATIF lives in the core too, so Roundhouse ports its twelve wire structs (`ATIF_SCHEMA_VERSION = "ATIF-v1.7"`) with attribution at rev `1a548124` (`crates/roundhouse-relay/src/atif.rs`).

The Relay repository is `github.com/NVIDIA/NeMo-Relay`. `github.com/NVIDIA-NeMo/NeMo-Relay` returns 404, although Switchyard is under `NVIDIA-NeMo/`. `nemo-relay-types` declares no `rust-version` at any version, so do not state an MSRV for it. Edition 2024 implies at least 1.85.

### Relay 0.8.2 on the wire

The gated Claude Code suite runs the chained topology against Relay 0.8.2 (`VERIFIED_RELAY_VERSION = "0.8.2"` in `crates/roundhouse-server/tests/common/claude_rig.rs`) and warns on a different version. These facts are from the published 0.8.2 crates.

- Relay re-serializes each intercepted body through a map that sorts keys alphabetically. `ItemContent::Opaque` digests canonical JSON for this reason, and it depends on `serde_json` staying without `preserve_order` (see [Other library pins](#other-library-pins)).
- Relay merges its headers into `ANTHROPIC_CUSTOM_HEADERS` rather than replacing them, so the turn key survives the hop. `?beta=true` also survives.
- Relay injects `[upstream] anthropic_auth_header` only when the inbound request has no credential (`gateway/mod.rs:1070-1078`), and it forwards `x-api-key` untouched. It clears a configured `anthropic_auth_header` when another layer supplies the base URL, so set both in one layer.
- The SSE re-encoder drops `id:` lines and frames that have no `data:` line. A trailing `/v1` on the Relay `anthropic_base_url` breaks the chain.
- From 0.8.1, the gateway refuses a non-loopback bind (`server/mod.rs:92-97`).
- The internal dispatch override of a Relay plugin strips provider credentials before it redirects, so turns on that path are key-authenticated only.
- The gateway adds eight headers: `traceparent` and `x-nemo-relay-{agent-kind, identity-quality, parent-scope-id, request-id, root-scope-id, session-id, source, turn-id}`. Its `session-id` equals `x-claude-code-session-id`. Roundhouse ignores all of them. `x-nemo-relay-source: gateway` proves a request went through Relay, which is what makes negative assertions beside it meaningful.
- `GET /healthz` (`nemo-relay-cli-0.8.2/src/server/mod.rs:635`) is unauthenticated. It answers 200 when compatible and 409 when not, with `status`, `service`, `version`, `bootstrap_protocol`, and `instance_id`.
- Relay does not proxy MCP. The agent reaches the Roundhouse MCP endpoint directly.
- Session state is in process only. Sessions are swept 30 s after the last activity (`AGENT_IDLE_TIMEOUT = 30s`, sweep every 5 s), and correlation hints live 300 s and clear at each turn boundary. Correlation is scored (`hint_match_score`), where Roundhouse correlation is an ordered ladder of exact ids and only its last rung, `latest`, is a guess. See [MCP control surface](../concepts/mcp.md).
- Across the seven Relay crates there is no session store, conversation log, Redis, writer lease, idempotency key, or spend ledger. `RoutingPolicy.session_cost_cap` is declared and never read.

The Relay side of the chain is in [NeMo Relay formats](../operations/relay-formats.md#aim-a-nemo-relay-at-roundhouse).

### The Relay Anthropic codec is lossy

At Relay 0.8.0 (`src/codec/` has no diff at 0.8.2), `codec/anthropic.rs` is 1,389 lines that map `serde_json::Value` to a provider-neutral form. There is no typed Anthropic request struct.

- `thinking` and `redacted_thinking` become opaque blobs. The stream accumulator drops `thinking_delta` and `signature_delta`, so a streamed thinking block ends with empty content and no signature.
- `cache_control` has no TTL variants, and there is no `usage.cache_creation` breakdown, only the flat `cache_creation_input_tokens`.
- `stop_reason` maps only `end_turn`, `max_tokens`, and `tool_use`. The other four values become `Unknown`.
- `mcp_tool_use` and `server_tool_use` are excluded from `tool_calls`.

Roundhouse needs all of these. Cache TTL and `cache_creation` feed the frontier quote and the cache ledger. `stop_reason` decides whether a turn is finished. Thinking blocks are conversation state that prefix admission must reproduce byte for byte. Relay can lose all three and still export a trace. So Roundhouse writes its own Messages types.

## Switchyard

Roundhouse takes no Switchyard crate and does not call `switchyard-server` at runtime. A runtime call would tie every turn to an API whose core vocabulary changed three times in one week of 2026-08. Roundhouse takes ported code and an adapted prompt. Each file names its revision, and a test reads the attribution text.

| What | From | Revision | Where |
|---|---|---|---|
| The coding-agent scorer: five constants, the `tanh` arithmetic, the tier axis, `PickerMode` | `crates/libsy/src/algorithms/util/stage.rs` | `053a61e2c43ba15f0772952ec3b3060c24b317f2` | `crates/roundhouse-core/src/routing/stage.rs` |
| The `ToolSignals` extractor | `libsy` | `053a61e2c43ba15f0772952ec3b3060c24b317f2` | `crates/roundhouse-core/src/validate/tool_signals.rs` |
| The trouble-pattern taxonomy and injection-defense sentence of the judge prompt | `crates/libsy/src/prompts/escalation/prompt.md`, `crates/libsy/src/prompts/advisor-gate/reviewer-system-prompt.md` | `47babb1a933e952bc6997b9ea208b5903c61a48c` | `crates/roundhouse-core/src/validate/prompts/judge-system-prompt.md` |

The asset is the calibration. The error-pattern table and the thresholds were mined from traces, not reasoned, so an editorial change during the port would be an unmeasured heuristic with a measured provenance. The scorer diverges from upstream in four places, each documented at the divergence in `stage.rs`:

- `compacted` has no input in this tree, so the hard escalate is severity-only.
- `turn_depth` counts exchanges, not messages.
- Upstream's `ConsultClassifier` outcome is folded away, because routing makes no model calls. An undecided turn takes the picker's default tier and is marked `Ambiguous`.
- The cost guard on efficient picks is Roundhouse's own and has no upstream counterpart.

The `caller_auth_kind` conditional of Switchyard is what the two `CodexAuthKind` values of `codex_launch` mirror. See [Routing and the selection service](../concepts/routing.md).

**Calibration caveats, at `053a61e`.** The stage scorer thresholds were calibrated on SWE-Bench Pro Python-75 and do not transfer across model pairs or domains. Every published Switchyard number is `efficient_first`, and `capable_first` is not benchmarked. `TierRecipe::uncalibrated_warning` is the same startup warning that the Switchyard server prints. The Harbor harness of Switchyard pins Codex 0.144.5 and does not attribute cost per task. The two `tau2-telecom-custom-opus-qwen-*` profiles in `benchmark/routing-profiles/` state 0.903 ± 0.071 solve at 45% weak-tier turns (balanced) and 0.891 ± 0.029 at about 85% (aggressive), on tau2-bench telecom with the opus and qwen pair. Each file says its wiring is not as measured.

### Why no dependency on `switchyard-libsy`

The libsy `Algorithm` trait is a good fit in shape: an algorithm emits `Step::CallModel` with a semantic target, and the host executes it. Adoption was costed at `5341f71` and `053a61e` and rejected, and libsy describes itself as pre-alpha. `RoutingPolicy` in `crates/roundhouse-core/src/routing/mod.rs` keeps it possible as an option, not a dependency.

- **State is in memory.** `libsy::State` has no `Serialize`, no store trait, and no snapshot. Session state is three process-local structures: a `Mutex<HashMap>` with a one-hour TTL, a second map in `AffinityRouter`, and a third ledger in the advisor gate. Roundhouse must survive process death.
- **Only trivial algorithms fit a pure `choose`.** The novel ones (`LlmTaskClassifier`, `AdvisorGate`) must own dispatch. The rest duplicate logic Roundhouse already has.
- **The request type has no slot for the routing context.** `switchyard_protocol::Request` cannot carry `isl_tokens`, the cache ledger, the turn policy, the budget, or candidates with their expected prefill, TTFT, cost, quality prior, and load. The only escape is the string-typed `extra_metadata`.
- **Dependency weight.** libsy needs `jsonschema 0.49.4`, `jsonptr 0.8.1`, and `opentelemetry 0.32` with `tracing-opentelemetry 0.33`. Roundhouse has `opentelemetry 0.31` through the dev-only Codex crates, so two API majors would share one test binary. The Switchyard server uses `reqwest 0.13.4` against the workspace `0.12`.
- **The scorer is liftable, the extractor is the hard half.** The scorers are public, pure, and `Default`, so a port is about 40 lines. `Metadata::from_headers` in `switchyard-protocol` normalizes 16 fields from an alias table, and Roundhouse derives its own session labels instead (`crates/roundhouse-sequence-id/src/label.rs`).

The escalation route and the advisor gate were costed separately. See [Design decisions](design-decisions.md#switchyards-escalation-classifier-is-not-the-escalation-policy) and [Validate and steer](../concepts/validate-steer.md).

### The version-identity rule

"0.2.0" names three different libraries: the crates.io release (2026-08-10), the `v0.2.0` tag that every document tells you to pin, and main. The tag's `core::algorithm` exports eleven names. Main nine days later exported seven, and only four survived by name. `AdvisorGate` is in no published release. Later, `session_affinity` was replaced by `classify_trigger` (`c7b648d0`; values `every_request` (default), `user_turn`, `new_session`).

Measured churn, from shallow clones, so these are lower bounds: `switchyard/crates/libsy/src` had 14 commits and 7 breaking changes from 2026-08-11 to 2026-08-19, `switchyard/crates/protocol/src` had 9 and 2 in the same window, and `nemo-relay/crates/types/src` had 6 and 1 from 2026-08-03 to 2026-08-19.

The rule: any adoption from a pre-1.0 neighbor pins a git rev or an immutable exact version, never a caret or a tag, and the unlock condition goes beside the pin. Coupling to Relay buys a slow-moving format. Coupling to Switchyard would buy a fast-moving library.

### The decision service and the Relay seam

- The Relay `crates/switchyard` (at `c37b551`) was an HTTP client for a separately operated Decision API, not a router. Its only quantitative request field, `prompt_token_estimate`, was hard-coded `None`, and its timeout defaulted to 25 ms over the network. Relay deleted it and the CLI `switchyard` feature in `88d1b1b` (about 4,700 lines), and a config with `[[components]] kind = "switchyard"` is now a hard error. On crates.io, `nemo-relay-switchyard` stays at 0.7.3.
- The `Algorithm` trait variant is `Step::CallModel`. `Step::CallLlm` never existed.
- Switchyard main at `47babb1` serves only inference-proxy routes. At `053a61e`, `POST /v1/decision` returned a target, ordered fallbacks, and client wiring without calling the answer model.

## redis

`roundhouse-store-redis` is the only consumer. The Dynamo crates never touch redis.

```toml
redis = { version = "1.2", default-features = false, features = [
    "tokio-rustls-comp", "streams", "script", "connection-manager",
] }
```

It resolves to 1.2.4. The `server` crate takes the same workspace pin as a dev-dependency, for one test that writes a key of the wrong type.

- **Why not the newest 1.x.** redis 1.3.0 and later carry a target-gated `tokio ^1.51` requirement (for wasm). The resolver unifies it across the workspace, and Dynamo pins `tokio = "=1.48.0"`.
- **Unlock condition.** The day the Dynamo `tokio` pin reaches 1.51, redis moves to the newest 1.x in a one-line change.
- 1.2.4 already carries the fixes of the 1.0 line: default async timeouts, the `ConnectionManager` retry fix, the TCP deadlock fix, and idempotent stream producers.
- Any 1.x unifies with the Relay `^1.1`. `nemo-relay-adaptive-0.8.2` declares `redis = "1.1"` (caret, optional). Check it again at the next `nemo-relay-*` move.

**Client and server facts that shape the store.**

- **In redis-rs 1.2.4, error kinds cannot separate a wire fault from a decode fault.** The RESP parser and a client-side `FromRedisValue` conversion both raise `ErrorKind::Parse`. Classifying by that kind would treat a protocol desync as the fault of one session. So `read_events` and `last_seq` decode the pipeline reply as a generic `Value`, where a real protocol failure stays `Backend`, and only the local conversion of the received `Value` maps to `CorruptLog`.
- **`RedisError::code` does not see a pipelined `WRONGTYPE`.** The store reads the server errors directly.
- **Script `WRONGTYPE` depends on the Redis version.** Redis 7 and later pass it through with its code. 6.x wraps it as `ERR Error running script ...`. So pipelined reads classify a wrong-typed key as `CorruptLog` on every supported version, and the lease scripts do so on 7 and later, falling back to `Backend` on 6.x. No lease caller branches on the difference.
- The supported floor is Redis 6.2. A probe against Redis 7.4 showed that `HSET` writes Lua-number counters as exact decimal integers up to 2^53-1.

See [Deploy with Redis](../operations/redis.md) and [Session store contract](store-contract.md).

## Other watched projects

These are not dependencies. Roundhouse reads them to know where the product boundary is. Each fact names the revision or date it was read from.

### TypeSafe Jev

Read from `docs.typesafe.ai` on 2026-09-17, 2026-09-21, and 2026-09-22. Roundhouse calls it from `crates/roundhouse-fleet/src/typesafe.rs`, in the background only. See [The routing learner](../concepts/routing-learner.md).

- One endpoint: `POST https://api.typesafe.ai/v1/systemone`, bearer auth. Errors are 401, 422, 429, and 529, and the docs say to retry 429 and 529 with backoff.
- The request has `state`, `model` (`jev-latest`; cookbooks pin `jev-1.12`), and `questions`, a map of named typed questions. The types are `noul` (one probability), `choice`, and `score`. Roundhouse uses `choice` only, which returns `choice`, `probabilities` over every option, and `confidence`. The response carries `usage.input_tokens`, `usage.output_tokens`, and `model`, the model that answered, which can differ from the alias.
- The published price is $0.042 per million input tokens, with free output. Published example: 13 questions over about 11,800 tokens took 0.27 s and cost $0.000497, and 13 separate calls took 2.71 s and cost 12.2 times more. A 182-option `choice` took about 0.31 s. Nothing covers a coding-agent state of 50k to 200k tokens, so do not extrapolate.
- Not published: a context limit, a confidence formula, calibration evidence, or determinism. So the code treats `confidence` as an observation to gate on, never as confidence in an outcome. An authenticated `GET /v1/models` on 2026-09-22 listed `jev-latest` and `jev-preview`, not `jev-1.12`.

### vLLM agentic-api

Read at `d59d4b4` and again at `e35fbb2`. It is a Rust gateway that adds server-held state and server-side tools in front of one stateless vLLM endpoint. It has no model catalog, routing, cache affinity, cost, budget, or metrics.

- Routes: `/v1/responses` (POST, and GET as a WebSocket), `/v1/responses/compact`, `/v1/conversations`, `/v1/messages`, `/v1/messages/count_tokens`, and `/v1/models` (proxied). The tool loop is bounded at `MAX_GATEWAY_TOOL_ROUNDS = 10`, only `web_search` has a gateway-owned executor, and MCP is client-side only (`rmcp` 1.8).
- Its `conversation_id` path takes a row lock and an optimistic version. Its `previous_response_id` path has no version check, so two concurrent turns fork the chain.
- At `e35fbb2` it forces `parallel_tool_calls = false` upstream while its Codex catalog claims `supports_parallel_tool_calls: true`.
- An MCP `isError` result goes back to the model as `{"error": "<text>"}` in an ordinary `function_call_output`, and the turn completes with 200 (`tool/mcp/handler.rs:248-262`).
- `[mcp_servers.*].headers` takes a literal bearer with no environment indirection (`tool/mcp/pool.rs:15-40`).

Four build facts block a dependency on `agentic-server-core` (0.3.0): its `reqwest` uses native-tls (OpenSSL), while Roundhouse and the Dynamo `deny.toml` are rustls-only. It brings two `reqwest` majors (0.12.28 and 0.13.4 through `rmcp`). It uses `rmcp` 1.8 against the Roundhouse 3.1.3, which would put two MCP implementations in one process. It pins Rust 1.98.0 against the Roundhouse 1.96.1 with no `rust-version` in its crates.io package, so cargo fails to build rather than warns. That unlocks when Roundhouse moves to 1.98.0 or later. `axum` 0.8.9 and `tokio` 1.52.3 are not blockers on their own. The cheapest seam, if ever needed, is to copy its SSE normalizer, which reaches no `reqwest`, `sqlx`, or `rmcp`.

### Kubernetes Gateway API inference extension and llm-d

Read at `kubernetes-sigs/gateway-api-inference-extension` `84436a9` (v1.6.0 released 2026-08-17; protocol spec v1.0.0) and `llm-d-router` `e051872`.

- **The extension no longer contains a scheduler.** From v1.6.0 the endpoint picker, scheduler plugins, scorers, and flow control moved to `llm-d/llm-d-router` and `llm-d/llm-d-inference-payload-processor` (`a70292c`, 90,903 lines deleted). What remains is the `InferencePool` v1 CRD, `InferencePoolImport`, the picker protocol spec, the conformance suite, and a round-robin reference picker.
- **The picker protocol** is Envoy ext-proc with one gRPC stream per HTTP request. It picks at body `EndOfStream`, after the whole body is buffered, and returns the endpoint in the `x-gateway-destination-endpoint` header and `envoy.lb` metadata. It answers 503 for no ready endpoint and 429 to drop a sheddable request. `InferencePool.spec.endpointPickerRef.failureMode` defaults to `FailClose`. The reference picker caps bodies at 10 MB, and the llm-d ext-proc body path has no cap, so a huge turn fails by memory, not by a clean 413.
- **No session concept.** Nothing in the extension models a conversation, and its prefix-cache proposal rejected session affinity. The llm-d-router `agent-identity` plugin (since `f3ae7503`) resolves a fairness id from `x-claude-code-session-id`, `x-session-affinity`, `session-id` (Codex 0.131.0 and later), or `session_id` (Codex 0.130.x). That is a fairness queue label, not a session log, lease, or prefix admission.
- **In-flight load booking exists upstream** as `inflight-load-producer`. The default profile does not run it, and it books an estimate (`bytes/4`, output times 1.5), not a measured reservation.

Why Roundhouse sits in front of such a gateway and never behind it is in [Design decisions](design-decisions.md#product-boundary-and-topology).

## Other library pins

These are ordinary libraries. Their pins are exact, or they are carets with a stated reason.

| Pin | Kind | Reason |
|---|---|---|
| `axum = "=0.8.4"` | Exact | Matches the Dynamo workspace, so a move into Dynamo `lib/` is a no-op. |
| `clap = "=4.6.6"`, `ratatui = "=0.30.2"`, `crossterm = "=0.29.0"` | Exact | `topham` is the one binary an operator runs by hand, and a floating CLI parser changes what their muscle memory does. None constrains `tokio` or `uuid`, and toolchain 1.96.1 clears the ratatui floor of 1.88. |
| `rmcp = "3.1.3"` | Caret | The MCP server in `roundhouse-mcp`, with no TLS stack and no `axum` dependency. Codex 0.146.0 connects with an `rmcp` 1.8.0 client. |
| `reqwest = "0.12.24"`, rustls only | Caret | The Dynamo `deny.toml` bans OpenSSL, and Roundhouse matches it. The lock resolves 0.12.28. |
| `tokenizers = "0.21.4"` | Caret | Same major, minor, and feature shape as the Dynamo `lib/llm` pin, without the network features. Tokenizer files load from disk only. |
| Toolchain `1.96.1` | Exact | `rust-toolchain.toml`. |
| `serde_json` without `preserve_order` | Feature ceiling | `serde_json::Map` must stay a `BTreeMap`. `ItemContent::Opaque` digests canonical JSON, and a chained Relay reorders keys. With insertion order, every session behind a Relay would fork on its second turn. The guard is `an_opaque_block_is_insensitive_to_key_order`. Unlock condition: any crate in the graph that turns `preserve_order` on. Features unify, so check `ItemContent::render` and prefix admission before accepting it. |

There is no configuration-directory crate. `topham` finds its profile directory as `XDG_CONFIG_HOME` if that names something, else `$HOME/.config`, the same rule Relay uses (`crates/topham/src/env.rs`).

## When a pin moves

A synergy dependency is watched, not only pinned. An upgrade is never only a version change.

1. Read what changed upstream before the move lands. Diff the release notes or the tree between the old pin and the new one.
2. Map each change into the product. Ask whether it changes how an agent hooks up, what a turn costs, where a route can go, or what this book claims.
3. Update the chapter that states the affected contract, and name the revision it was read from.
4. Read every pinned-source claim again before work that relies on it. A claim read from one revision is stale the day the pin moves.
5. If a constraint blocks the newest version, write the unlock condition in the manifest beside the pin.

For Codex and Claude Code, the gated suites print the version they run and warn on a mismatch. For Relay, the Claude Code rig warns when the binary is not `VERIFIED_RELAY_VERSION`. For the Anthropic spec, run the `anthropic-spec-sync` skill.
