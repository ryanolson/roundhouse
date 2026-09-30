# Upstream dependencies

Some dependencies are the other half of the product, not ordinary libraries. They are Dynamo, the Codex crates, NeMo Relay, Switchyard, redis, and the Anthropic Messages spec. For each one, this chapter gives what Roundhouse takes from it and the exact pin. It also gives the revision of each external claim, why the pin is exact, and what unlocks a move.

The manifests are the source of truth for every pin. The workspace pins are in `Cargo.toml`. The crate pins are in `crates/*/Cargo.toml`. Each exact pin carries its reason and its unlock condition in a comment beside it.

## Summary

| Dependency | What Roundhouse takes | Pin | Unlock condition |
|---|---|---|---|
| Dynamo | `dynamo-kv-router` (feature `standalone-selection`), `dynamo-tokens`, and `dynamo-mocker` (tests only) | git rev `ac7b7513790ef1d619b46f805aea03c9f21200ba` | A crates.io release of `dynamo-kv-router` that exports the embeddable `SelectionService` |
| Codex | `codex-api`, `codex-client`, `codex-protocol`, `codex-utils-rustls-provider`, dev-dependencies only | git rev `6344a655a5966f92e009a74928fb0559b41f9093` | A move is a re-read of every wire claim, not a condition |
| NeMo Relay | `nemo-relay-types` (ATOF and `LlmOptimizationSummary` types) | `=0.7.3` | A `nemo-relay-types` release that relaxes its own `uuid = "=1.18.1"` |
| Switchyard | No crate. A ported scorer, a ported signal extractor, and an adapted judge prompt | Revisions named in the attribution of each file | Not applicable |
| redis | `redis` client | `1.2`, resolves to 1.2.4 | Dynamo's `tokio` pin reaches 1.51 |
| Anthropic Messages spec | The vocabulary of the hand-written wire module | `spec_pin.json`, SDK rev `4140e0ea` | A refresh is a sync run, not a condition |

## Dynamo

Roundhouse depends on Dynamo but is not part of it. It builds independently.

| Crate | Use | Where |
|---|---|---|
| `dynamo-kv-router` | The embedded selection service, with `default-features = false` and `standalone-selection` | `roundhouse-core`, `roundhouse-fleet` |
| `dynamo-tokens` | Token block hashing | `roundhouse-core`, `roundhouse-fleet` |
| `dynamo-mocker` | A mock engine that simulates vLLM or SGLang scheduling and prefix caching without GPUs, and publishes KV events over `zmq-events` | `roundhouse-server` dev-dependency |

All three resolve at version 1.4.0 from git rev `ac7b7513790ef1d619b46f805aea03c9f21200ba` of `ai-dynamo/dynamo`. The mocker is what lets the cache-hit path be measured rather than asserted (`crates/roundhouse-server/tests/mocker_cache_hits.rs`).

### Why a git rev and not crates.io

- The newest published `dynamo-kv-router` (1.3.1) predates DEP #10321. It does not export the embeddable `SelectionService`, and it says `tenant_id` where the pin says `routing_group`.
- The git dependency also resolves `dynamo-truthy`, which the workspace needs and which is not published.
- When a release that carries the selection service reaches crates.io, the pin becomes a plain version.

### What Roundhouse deliberately does not depend on

- **`dynamo-llm`.** Its default `block-manager` feature pulls `nixl-sys` and `cudarc`, which would bring CUDA into a service that never touches a GPU.
- **`dynamo-runtime`.** `dynamo-kv-router` under `standalone-selection` has no `dynamo-runtime` dependency, and Dynamo enforces that in `lib/kv-router/src/services/CLAUDE.md`. The cost is that Roundhouse cannot read the Dynamo event plane.

### Transitive pins

`dynamo-mocker` at the pin requires `tokio = "=1.48.0"` exactly, and so does Dynamo main. The workspace declares `tokio = "1.48"` as a caret, which resolves to 1.48.0. An exact pin in the workspace would repeat the Dynamo constraint in a place where nobody can see whose it is. When Dynamo relaxes, the workspace floats. This pin is also the ceiling on redis (see [redis](#redis)).

### What Roundhouse sends to Dynamo

- The embedded fleet passes the Roundhouse session id as `SelectRequest.session_id` (`crates/roundhouse-fleet/src/local.rs`).
- It always sets `pinned_worker: None` and `allowed_worker_ids: None`. Roundhouse pulls no worker-stickiness lever.
- `roundhouse-sequence-id` defines a keyed `SequenceDigest` whose documentation names it as the `x-dynamo-session-id` value. No dispatch path in the tree sends that header.
- Roundhouse reads `x-dynamo-session-final: true` from a client as a signal. It does not send a session-final request to Dynamo.

The boundary of authority: worker-local scheduling and physical KV residency belong to the serving plane (Dynamo, and KVBM inside a worker). Roundhouse holds durable session intent. It can send only advisory, idempotent hints. It never names a block to evict and never releases one. Two cache authorities in one request path would contradict each other. For the same reason, llm-d is not used as a scheduler or as a second index of cache residency.

### Facts at the pin

These were read from Dynamo at `ac7b751`, unless a line says otherwise.

**Session identity is passive.** `session_id` travels through `SelectionOperation`, `ScheduleRequest`, `SchedulingRequest`, and `lib/llm/src/kv_router.rs`. Nothing branches on it, hashes it, or compares it. `pinned_worker` and `allowed_worker_ids` in the same structs are read for worker selection.

**Session headers and affinity.**

- `x-dynamo-session-id` wins over every client header.
- The agent header map takes the Codex `session-id` as the session, with no child header, so a whole Codex agent family is one Dynamo session.
- For Claude Code the map takes the agent id and falls back to the session header.
- Session affinity pins a session id to a worker for a TTL. It is off unless `--router-session-affinity-ttl-secs` is set. It holds at most 65,536 entries.
- `x-dynamo-worker-instance-id` pins a decode worker. Roundhouse does not send it.

**Nothing releases the KV of one session.**

- No endpoint takes a sequence id and releases its blocks.
- `x-dynamo-session-final: true` is documented as "a dedicated minimal request when the session ends". The frontend turns it into `kv_hints.evict_session`, which only tests read. A Python ThunderAgent router drops its scheduler state on it. No reader releases KV.
- `clear_kv_blocks` is a per-worker control on the worker system port (`/engine/control/clear_kv_blocks`). It flushes the whole prefix cache (vLLM `reset_prefix_cache(reset_connector=True)`). SGLang refuses it while a request is active. It is not a frontend route and has no sequence scope.
- A live session-final request costs a scheduling slot, a prefill, and one decoded token, and frees nothing. That is why Roundhouse sends none.

**The admin API is on by default and has no authentication.** This includes `/busy_threshold`, at both `ac7b751` and main `6822bab`. A lifecycle or release route mounted beside it would need its own bearer token.

**Overload.** The frontend refuses an overload with the status in `DYN_HTTP_OVERLOAD_STATUS_CODE`, default 529, when every worker of a model is over its busy threshold. The busy gate is off unless a threshold is set. No code under `lib/llm/src` sends `Retry-After`. A caller that wants to tell a client when to retry must compute the value itself.

**What the frontend reports about cache hits.**

- It copies `usage.prompt_tokens_details.cached_tokens` from the backend `completion_usage`. A decoder can mark that count `CacheReadSource::Provider`, the only source that cache evidence counts as measured.
- `nvext.extra_fields: ["worker_id"]` returns `prefill_worker_id` and `decode_worker_id`.
- `nvext.extra_fields: ["timing"]` returns a per-request `kv_hit_rate`. That is the router prediction at routing time, not an engine measurement.
- The same ratio feeds the `router_kv_hit_rate` histogram. The frontend `/metrics` includes it only when `drt_metrics` is set.
- The mocker does not fill `cached_tokens` at the pin. Its prefill time is computed from `isl - prefix`, so its time to first token falls on a cache hit even when its usage reports no cached count.

**No single KV-load number for an outside caller.**

- `/health` lists instances, `/live` is liveness, and `/busy_threshold` returns thresholds, not loads.
- Frontend `/metrics` has `dynamo_frontend_worker_active_decode_blocks` (labels `worker_id`, `dp_rank`, `worker_type`). It is the predicted in-flight block count of the KV router, and it exists only when the frontend runs that router.
- With the defaults `router_track_output_blocks: false`, `router_assume_kv_reuse: true`, and `router_replica_sync: false`, that gauge under-counts decode growth, counts shared prefix blocks once, and sees only the traffic of its own frontend.
- `dynamo_frontend_model_total_kv_blocks` has only a `model` label and holds the block count of one worker. The last writer wins.
- The engine-reported `kv_used_blocks` travels only on the event plane (`kv_metrics` subject). Reading it needs a `DistributedRuntime`, which Roundhouse deliberately does not depend on.
- The standalone selection service `GET /loads` gives `potential_prefill_tokens`, `potential_decode_blocks`, and `active_requests` per worker, with no capacity.
- It is not known whether vLLM `kv_cache_usage` counts evictable prefix-cached blocks as used.

### What differs at Dynamo main

Read at main `6822bab`. A move of the pin past `ac7b751` would bring these changes:

- `KvHints` and `evict_session` are removed (`8b030e0ec1`). `KvHint` now means a typed KV-transfer envelope (`kv.fetch@1.0`).
- The native ThunderAgent plugin (`cde7c9a8c5`) drops its program entry, then still dispatches the final request as inference.
- `SessionPrefixIndexer` (`adfe2aa1b1`) is off by default and removes sessions only by LRU at 16,384 entries.
- Session affinity has hard and soft modes (`052c1e2853`). A final request refreshes the binding instead of ending it.
- Dynamo removed the explicit SGLang `open_session` and `close_session`, and the sticky lifecycle module (`5407083c16`).
- The mocker fills `cached_tokens` for its vLLM scheduler model from `8970b35248`. The SGLang model still reports `None`. The mocker engine now lives in `aisimulate-core` `=0.12.0`. A cache-hit measurement at the HTTP level against the mocker needs a pin at or after `8970b35248`. The in-process KV-event harness does not.
- Dynamo reads the Codex `thread-id` as the session, not `session-id` (`3b7675444c`). This has no effect when `x-dynamo-session-id` is sent.
- vLLM (`43b6344b87`) and SGLang (`b2d456f2bd`) receive the session id, so it can appear in engine logs.
- `kv_used_blocks` and `kv_total_blocks` are still not exported as gauges.
- The `tokio = "=1.48.0"` pin is still there.

### Worker recipe for the embedded selector

For the embedded fleet, a worker must publish KV events in the format the selector expects. This recipe is adapted from `examples/backends/vllm/launch/agg_router.sh` at the pin. Read it again on every move of the pin.

```bash
python -m dynamo.vllm --model <model> --block-size 64 --enable-prefix-caching \
  --kv-events-config '{"publisher":"zmq","topic":"kv-events","endpoint":"tcp://*:20080","enable_kv_cache_events":true}'
python -m dynamo.frontend --router-mode kv --http-port 8000
```

- The block size must equal `WorkerRegistration.block_size`. The Dynamo indexer drops KV events whose block size differs from its own. This is a contract, not a tuning value.
- Prefix caching is requested explicitly, so the contract does not depend on a default.
- `PYTHONHASHSEED=0` makes the event hashes of the worker match the hashes Roundhouse computes over a prompt.
- Each DP rank has one ZMQ endpoint (`WorkerRegistration.kv_events_endpoints`).
- The frontend is only for an independent OpenAI-compatible smoke test. etcd and NATS provide discovery. At the pin, their compose file is `dev/docker-compose.yml`. `deploy/docker-compose.yml` does not exist.
- `--enforce-eager` is for a quick first start only.
- On CUDA 13.x, the FlashInfer sampler JIT of vLLM fails on version-skewed headers (torch pins 13.0, `tilelang` pulls `nvidia-cuda-nvcc` 13.2). Set `VLLM_USE_FLASHINFER_SAMPLER=0`.

## Codex

### The conformance crates

`codex-api`, `codex-client`, `codex-protocol`, and `codex-utils-rustls-provider` are pinned to `openai/codex` rev `6344a655a5966f92e009a74928fb0559b41f9093` as dev-dependencies of `roundhouse-server`. They are the conformance oracle for `/v1/responses`: the output is checked by the parser a real agent runs, not by a reading of the spec (`crates/roundhouse-server/tests/codex_conformance.rs`).

- **Dev-only on purpose.** `codex-http-client` pulls `reqwest` with default features, which brings native-tls and OpenSSL. The shipped binary is rustls-only. With resolver v3, the features of a dev-dependency do not reach `cargo build`.
- **Two `[patch.crates-io]` entries.** The Codex workspace requests `tungstenite` features (`proxy`, `deflate`) that exist only on the OpenAI forks. A `[patch]` does not travel with a git dependency, so the workspace states it again: `tokio-tungstenite` at rev `0e5b2d73` and `tungstenite` at rev `4fffad30` of `openai-oss-forks`. The patches affect only the 0.27 and 0.28 requirements inside the Codex crates. axum's websocket feature stays on the 0.26 series from crates.io.
- `codex-core` is private. Only a real `codex exec` can prove how Codex dispatches a tool.

### The binary and the pin are different trees

Three Codex revisions matter:

| Name | Revision | What it is |
|---|---|---|
| Binary | `e363b08` (`rust-v0.146.0`, 2026-07-28) | The `codex-cli 0.146.0` binary that the real-binary suite runs |
| Pin | `6344a65` (2026-08-13) | The Cargo pin of the conformance crates |
| Later | `3b45c29` (2026-08-19) | The first of the three to carry the `requires_openai_auth` guard |

The binary and the pin are siblings off merge-base `95637f7`. Neither is an ancestor of the other.

**The `requires_openai_auth` guard.** `resolve_provider_auth` (`model-provider/src/auth.rs:179-196`) is byte-identical at the binary and the pin, and has no `requires_openai_auth` gate. `3b45c29` adds `if !provider.requires_openai_auth && provider.auth.is_none() { return Ok(unauthenticated_auth_provider()); }`. So at 0.146.0, `requires_openai_auth = false` with no `env_key` does not stop an ambient credential. At the binary the flag still gates the login screen. With `true` and no login, Codex sends no `Authorization` and retries once on a 401. `exec` cannot refresh a ChatGPT token. For this reason the launch generator writes `requires_openai_auth = false` only beside `env_key` (`crates/roundhouse-server/src/codex_launch.rs`). See [Hook up Codex](../guides/codex.md).

**Validate generated configuration against the binary, not the pin.**

- Byte-identical at both: `model-provider/src/auth.rs`, `model-provider/src/models_endpoint.rs`, `exec/src/exec_events.rs`.
- `ModelInfo` differs. Validate the catalog against the binary.
- `config_toml.rs` has `[debug]` and lockfile types only at the binary. It has `responses_api_metadata` and `[goals] max_goal_token_budget` only at the pin. 0.146.0 silently ignores a `responses_api_metadata` key.
- `supports_remote_compaction()` exists only at the binary.
- The pin removes `--full-auto` and adds `codex exec fork`.
- The gated suite (`crates/roundhouse-server/tests/codex_e2e.rs`) states `VERIFIED_VERSION = "codex-cli 0.146.0"`. It prints `codex --version` on every run and warns on a mismatch.

A useful method for the comparison: compare the two checkouts byte for byte before reading. An empty diff is the verdict. A blobless shallow clone supports `merge-base` and `--is-ancestor` but not a `git log` walk.

### Wire facts

**MCP tools, at `6344a65`.**

- MCP tools appear in the request `tools` array only as a namespace object: `{"type":"namespace","name":"mcp__roundhouse","tools":[...]}`. They never appear as flat functions (`core/src/tools/handlers/mcp.rs:362-394`).
- Dispatch is an exact `HashMap` lookup on `ToolName { name, namespace }` (`core/src/tools/router.rs:164`). Nothing splits a flat `mcp__server__tool` back apart. So a call must carry `namespace` as its own wire field.
- Dispatch happens on `response.output_item.done`. It does not depend on `output_item.added` or on argument deltas. `response.function_call_arguments.delta` is trace-only in `codex-api`.
- An unknown tool name becomes `FunctionCallError::RespondToModel("unsupported call: …")`. The model is confused, but the agent does not crash.
- Deferred tools are left out of the list the model sees. So an absent namespace does not prove that the MCP server is unregistered.
- For streamable HTTP, Codex requires `bearer_token_env_var`. It rejects `bearer_token` (`config/src/mcp_types.rs:429`).

**How a prior tool call is resent, at 0.146.0.** Codex resends a prior `function_call` by structure, not byte for byte. It serializes again in its own field order. It drops any item `id` without an underscore inside it (`core/src/client.rs:927-933`), so an id shaped `fc_<response_id>` survives. The `arguments` string comes back byte for byte. Tests therefore compare parsed fields, with `arguments` as the one byte-exact comparison.

**Request layout, at `6344a65`.** Derived from source and tests. No Codex request was captured for this, and the bytes of the tool schemas were not measured.

- A root thread has four canonical items: 0 `System` (the `instructions`, which vary by model and Codex version), 1 developer (permissions, skills, developer sections), 2 user (`AGENTS.md` plus `<environment_context>` with working directory, shell, `current_date` as `%Y-%m-%d`, and time zone), and 3 user (the typed prompt).
- Two sessions diverge at item 3 when working directory, date, and configuration agree, and at item 2 otherwise. Four items make a root request unique.
- A configuration change between turns is appended, not written over. A date change is an appended diff.
- Multi-agent v2 (`MultiAgentV2`, `default_enabled: false`) adds a usage-hint developer message before the contextual user message, which moves the prompt to item 4.
- A v2 full-history fork (`fork_turns` default `all`) removes `AgentMessage` items and the usage-hint and current-time developer messages. So the child diverges from the parent at item 2 and is not a byte prefix of the parent.
- v1 `Collab` is on by default and forks history only with `fork_context: true`.
- Codex sends `session-id` on every `/v1/responses` request, at both the binary and the pin.

## Claude Code

Claude Code is a client, not a crate, but its version is watched the same way.

| Version | Why it matters |
|---|---|
| 2.1.42 | The only readable (npm) bundle, and old |
| 2.1.229 | The minimum version the gateway documentation names |
| 2.1.247 | The first native binary that can be spawned |
| 2.1.251 and 2.1.257 | The two client lines the fixtures pin. Every fixture-driven test runs against both. |

The gated suite states `VERIFIED_VERSION = "2.1.257"` (`crates/roundhouse-server/tests/common/claude_rig.rs`), prints it on every run, and warns on a mismatch. A missing binary is a loud failure. The client updated itself twice in five days during capture work. That is why each wire claim names the client version it was captured from, and why `topham` turns off auto-update. See [Hook up Claude Code](../guides/claude-code.md).

## Anthropic Messages spec

Anthropic publishes no Rust SDK, and no crate carries the Messages wire correctly (see [Design decisions](design-decisions.md#wire-surfaces)). So `crates/roundhouse-fleet/src/anthropic_messages/wire.rs` is written by hand. The OpenAPI spec is pinned only as a vocabulary oracle: a test reads the pinned vocabulary and asserts that the hand-written module agrees.

### The pin

`crates/roundhouse-fleet/src/anthropic_messages/spec_pin.json` records:

| Field | Value |
|---|---|
| `source_sdk_rev` | `4140e0eaa597c0ad35218ffb20b66ef7fce7f639` (`anthropic-sdk-typescript`) |
| `spec_url` | `https://storage.googleapis.com/stainless-sdk-openapi-specs/anthropic/anthropic-e50cf35b74cc0471a2b5af7ea03765aa81c035f82588e9a1ba1b29aeaa17d064.yml` |
| `spec_sha256` | `d1d189d791d1b551b33edebe9a60d63836d167070863ef366969e0df1e9c4f55` |
| `openapi_spec_hash` | `e4ae88bdd84c7e293dd46037a6769b8d` |
| `fetched` | 2026-09-01 |
| `vocabulary` | The values the wire tests read |

The earlier pin was SDK `7ba6a3fc`, body sha256 `942a1163…3d2ee87`, 2,448,030 bytes, fetched 2026-08-27. Between the two pins, everything except the open beta enum and one new path was byte-identical.

### Three identifiers, not one

- The spec is OpenAPI 3.1.0, published without authentication at a content-addressed Stainless URL. The URL comes from `openapi_spec_url` in the `.stats.yml` of `anthropic-sdk-typescript`.
- There is no `latest` alias. A refresh reads `.stats.yml` at a newer SDK revision, fetches again, and runs the pinning tests again.
- The 64-hex hash in the URL filename and the 32-hex `openapi_spec_hash` are both opaque Stainless content addresses. Neither is a hash of the body.
- Only the sha256 that Roundhouse computes over the downloaded body checks integrity.
- A changed URL means the spec moved. An unchanged URL with a changed body sha256 means a broken download, because the storage is immutable.

### The pinned vocabulary

- `StopReason` has seven values: `end_turn`, `max_tokens`, `stop_sequence`, `tool_use`, `pause_turn`, `refusal`, `model_context_window_exceeded`. The wire module keeps an open `StopReason::Other(String)` arm for an eighth.
- `Usage` has nine properties. `CacheCreation` is `{ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}`, both required.
- `MessageStreamEvent` has six members: message start, delta, and stop, and content-block start, delta, and stop.
- There are four delta variants: text, input JSON, thinking, and signature.
- The response `ContentBlock` union has 12 members. `Message` has ten top-level properties.
- `system` is a string or an array of text blocks.
- `AnthropicBeta` is an open enum (`anyOf [string, enum]`), so a bare string is valid. It had 41 named values at the first pin and three more at the current one. The new values need no typed arm.
- The spec models the `?beta=true` surface as parallel paths: 140 paths, 124 of them beta duplicates, at the current pin. It carries `x-stainless-*` vendor extensions.

### What the spec does not describe

The literal `"ping"` occurs zero times in the 2.4 MB spec. No Messages-stream error event exists in it. `overloaded_error` appears only as an error-body schema. Both `ping` and a mid-stream `error` are real on the wire. So the wire module types and emits them by hand, and its own tests pin them (`the_two_transport_events_the_spec_omits_are_typed_anyway`). A spec sync must never read their absence as a removal.

### Refresh

Run the `anthropic-spec-sync` skill (`.claude/skills/anthropic-spec-sync/SKILL.md`). It fetches the spec, diffs the vocabulary, updates the pin, and fixes the code test-first where the API moved.

## NeMo Relay

Roundhouse interoperates with NeMo Relay at the format layer. It emits the published Relay formats (see [NeMo Relay formats](../operations/relay-formats.md)) and supports a Relay in front of it as a deployment topology. It is not a Relay plugin, and it takes no dependency on the Relay core.

The division of labor: Relay owns the harness (hooks, scopes, redaction, and the tool-level view the wire cannot show). Roundhouse owns the turn (the durable log, prefix admission, policy, budgets, routing, and steering). Dynamo owns the hardware. A topology is a choice per site. A dependency is carried everywhere. Treating the chained topology as architecture would make its risks permanent: two proxies, stripped credentials, and an upstream override.

### `nemo-relay-types` is pinned at `=0.7.3`

`crates/roundhouse-relay/Cargo.toml` declares `nemo-relay-types = { version = "=0.7.3", default-features = false }`. It is the only Relay crate in the graph.

- **Why 0.7.3 and not 0.8.** The code Roundhouse uses is byte-identical from 0.7.3 through 0.8.0-rc.1, 0.8.0, 0.8.2, and Relay tree `1a54812`. That code is `codec/optimization.rs` (`LlmOptimizationSummary`, `Contribution`, `Partial`, `limitations`) and the whole ATOF envelope (`BaseEvent`, `ScopeEvent`, `MarkEvent`, `Event`, `DataSchema`, `ScopeCategory`). The whole `src/` tree is identical from 0.8.0 to 0.8.2. A pin moves for a reason, not for freshness.
- **Why `=`.** A caret against a 0.x crate is not a reproducible statement. `version = "0.8"` would not even resolve to `0.8.0-rc.1`, because cargo excludes pre-releases from ordinary requirements.
- **What it costs.** The crate requires `uuid = "=1.18.1"` at every version. That pulled the whole workspace down from the 1.24.0 its caret resolved to, a six-release downgrade. Every other `uuid` dependent is satisfied (`moka` wants `1.1`, `rmcp` and `ts-rs` want `1`, `rama-http` wants `1.18`, Codex wants `1`, Dynamo wants a `1.18.1` caret). It is a ceiling: the day any dependency needs `uuid >= 1.19`, the workspace stops resolving.
- **Unlock condition.** A `nemo-relay-types` release that relaxes its own `=1.18.1` to a caret. A newer Relay alone does not help, because Relay HEAD pins it exactly too. The exact pin is the choice of the types crate. `nemo-relay` core depends on it by caret.
- **When 0.8 is the move.** The metric-mark surface (`MetricEnvelope`, `MetricMeasurement`, `METRIC_DATA_SCHEMA_NAME`, `LogSeverity`) exists only from 0.8. Nothing in Roundhouse emits one. The day something does, the target is `=0.8.0`, and the move needs no code change.
- **One 0.8 behavior change that does not affect Roundhouse.** `CostEstimate::total_or_component_sum` returns `None` unless `source == ProviderReported`. Roundhouse always sets `total: Some(_)`.
- **`default-features = false`** keeps the `schema` feature off. Roundhouse emits these types and never generates a JSON Schema from them.

Release timeline, from crates.io: 0.8.0-rc.1 on 2026-08-21, 0.8.0 on 2026-08-26, 0.8.1-rc.1 on 2026-08-27, and 0.8.2 on 2026-08-31.

### Ported, never depended on

- `nemo-relay` 0.8.0 declares 28 direct dependencies (four `opentelemetry` crates, `tonic`, `libloading`, `object_store`, `spdlog-rs`, `reqwest`, `tokio`, and more) and has about 156k lines. `nemo-relay-types` is the only cheap import (`bitflags`, `chrono`, `serde`, `typed-builder`, `uuid`).
- `nemo-relay-adaptive` and `nemo-relay-pii-redaction` depend on the core. `nemo-relay-pii-redaction` also pins `sha2 0.11` against the workspace `0.10`.
- The Relay `PricingCatalog` lives in the core, not in a pricing crate.
- ATIF lives in the core (`ATIF_SCHEMA_VERSION = "ATIF-v1.7"`, `atif.rs:55`, twelve wire structs). Roundhouse ports those structs with attribution at rev `1a548124` (`crates/roundhouse-relay/src/atif.rs`).

### Repository and crate facts

- The repository is `github.com/NVIDIA/NeMo-Relay`. `github.com/NVIDIA-NeMo/NeMo-Relay` returns 404. Switchyard is under `NVIDIA-NeMo/`. The crate metadata (`nemo-relay-types-0.7.3/Cargo.toml:23`) is the authority.
- `nemo-relay-types` declares no `rust-version` at any version. Do not state an MSRV for it. Edition 2024 implies at least 1.85.
- The 0.8.0-rc.1 tag is `513b7da`.
- In `LlmOptimizationSummary`, `limitations` is a free-form `Vec<String>`, and `status` is derived: `Complete` only when there are no limitations.
- `LlmOptimizationPayload` (`SCHEMA_NAME`, `SCHEMA_VERSION`, `Contribution::with_payload`) is the typed extension point for fields the summary has no slot for.

### Relay 0.8.2 on the wire

The gated Claude Code suite runs the chained topology against Relay 0.8.2 (`VERIFIED_RELAY_VERSION = "0.8.2"` in `crates/roundhouse-server/tests/common/claude_rig.rs`). These facts are from the published 0.8.2 crates.

- Relay re-serializes each intercepted body through a map that sorts keys alphabetically. `ItemContent::Opaque` digests canonical JSON for this reason, and it depends on `serde_json` staying without `preserve_order` (see [Other exact pins](#other-exact-pins-and-ceilings)).
- Relay merges its headers into `ANTHROPIC_CUSTOM_HEADERS` rather than replacing them, so the turn key survives the hop. `?beta=true` also survives.
- Relay injects `[upstream] anthropic_auth_header` only when the inbound request has no credential (`gateway/mod.rs:1070-1078`). It forwards `x-api-key` untouched and strips no unknown header.
- Relay clears a configured `anthropic_auth_header` when another layer supplies the base URL. Set both in one layer.
- The SSE re-encoder of Relay drops `id:` lines, and drops frames that have no `data:` line.
- A trailing `/v1` on the Relay `anthropic_base_url` breaks the chain.
- From 0.8.1, the gateway refuses a non-loopback bind (`server/mod.rs:92-97`).
- The internal dispatch override of a Relay plugin strips provider credentials before it redirects. Turns on that path are key-authenticated only.
- The gateway adds eight headers: `traceparent` and `x-nemo-relay-{agent-kind, identity-quality, parent-scope-id, request-id, root-scope-id, session-id, source, turn-id}`. Its `session-id` equals `x-claude-code-session-id`. Roundhouse ignores all of them.
- `x-nemo-relay-source: gateway` proves that a request went through Relay. That is what makes the negative assertions beside it meaningful.
- Relay does not proxy MCP. The agent reaches the Roundhouse MCP endpoint directly.

The Relay side of the chain is in [NeMo Relay formats](../operations/relay-formats.md#aim-a-nemo-relay-at-roundhouse).

### The Relay Anthropic codec is lossy

At Relay 0.8.0 (`src/codec/` has no diff at 0.8.2), `codec/anthropic.rs` is 1,389 lines that map `serde_json::Value` to a provider-neutral form. There is no typed Anthropic request struct.

- `thinking` and `redacted_thinking` become opaque `ProviderNative` blobs. The stream accumulator drops `thinking_delta` and `signature_delta`, so a streamed thinking block ends with empty content and no signature.
- `cache_control` has no TTL variants, and there is no `usage.cache_creation` breakdown, only the flat `cache_creation_input_tokens`.
- `stop_reason` maps only `end_turn`, `max_tokens`, and `tool_use`. The other four values become `Unknown`.
- `mcp_tool_use` and `server_tool_use` are excluded from `tool_calls`.

Roundhouse needs all of these. Cache TTL and `cache_creation` feed the frontier quote and the cache ledger. `stop_reason` decides whether a turn is finished. Thinking blocks are conversation state that prefix admission must reproduce byte for byte. Relay can lose all three and still export a trace. So Roundhouse writes its own Messages types.

### The Relay 0.8.2 gateway

- `GET /healthz` is the first route (`nemo-relay-cli-0.8.2/src/server/mod.rs:635`). The body has `status`, `service`, `version`, `bootstrap_protocol`, and `instance_id`. It answers 200 when compatible and 409 when not. It is unauthenticated, but the body proves identity rather than showing state.
- Session state is in process only. Sessions are swept 30 s after the last activity (`AGENT_IDLE_TIMEOUT = 30s`, sweep every 5 s).
- Correlation is scored (`hint_match_score`: subagent or agent 8, conversation 4, generation 4, request 4, model 1). Roundhouse correlation is exact, because Roundhouse binds an id that it emitted itself or that the client stamped, and it refuses on ambiguity.
- There is no session store, log, Redis, or persistence of conversation state anywhere in `nemo-relay` 0.8.2. Its ATIF exporter accumulates in memory and is lost with the process.

Ideas taken from Relay:

- A staleness bound on the call and thread tables, beside their capacity bound.
- A tenancy namespace and a schema version in every shared-store key. An empty one is refused.
- Refuse-if-foreign arbitration (file lock, owner record, ready file) for locally managed processes.
- Typed degradation on an outage.

Ideas not taken: correlation by hint scoring, a loopback-only single process as the deployment shape, and blanket fail-open on a shared store.

## Switchyard

Roundhouse takes no Switchyard crate, and it does not call `switchyard-server` at runtime. At `053a61e`, `POST /v1/decision` returns a target, ordered fallbacks, and client wiring without calling the answer model. A runtime call would tie every turn to an API that changed shape several times in one week. What Roundhouse takes is ported code and an adapted prompt. Each file names its revision, and a test reads the attribution text.

| What | From | Revision | Where |
|---|---|---|---|
| The coding-agent scorer: five constants, the `tanh` arithmetic, the tier axis, `PickerMode` | `crates/libsy/src/algorithms/util/stage.rs` | `053a61e2c43ba15f0772952ec3b3060c24b317f2` | `crates/roundhouse-core/src/routing/stage.rs` |
| The `ToolSignals` extractor | `libsy` | `053a61e2c43ba15f0772952ec3b3060c24b317f2` | `crates/roundhouse-core/src/validate/tool_signals.rs` |
| The trouble-pattern taxonomy and injection-defense sentence of the judge prompt | `crates/libsy/src/prompts/escalation/prompt.md`, `crates/libsy/src/prompts/advisor-gate/reviewer-system-prompt.md` | `47babb1a933e952bc6997b9ea208b5903c61a48c` | `crates/roundhouse-core/src/validate/prompts/judge-system-prompt.md` |

The asset is the calibration. The error-pattern table and the thresholds were mined from traces, not reasoned. An editorial change during the port would be an unmeasured heuristic with a measured provenance. Each divergence is stated at the divergence in `stage.rs`. See [Routing and the selection service](../concepts/routing.md).

### Calibration caveats, at `053a61e`

- The stage scorer thresholds were calibrated on SWE-Bench Pro Python-75. They do not transfer across model pairs or domains.
- Every published Switchyard number is `efficient_first`. `capable_first` is not benchmarked, and the Switchyard server warns at startup. `TierRecipe::uncalibrated_warning` is the same warning in Roundhouse.
- The route types at `053a61e` are `noop`, `random`, `passthrough`, `llm_classifier` (capability, escalation, or custom), `stage_router`, and `advisor`. "Recipe" is dead vocabulary. The live nouns are `[llm_clients.*]`, `[targets.*]`, and `[routes.*]`.
- The calibrated profiles in `benchmark/routing-profiles/` include `tb21-escalation-opus-glm-deepseek.toml` and two `tau2-telecom-custom-opus-qwen-*` files. The tau2 files state 0.903 ± 0.071 solve at 45% weak-tier turns (balanced) and 0.891 ± 0.029 at about 85% (aggressive). The setup is tau2-bench telecom, the opus and qwen pair, per the upstream file headers. Each file says that its wiring is not as measured and that the thresholds do not transfer.
- The Harbor harness of Switchyard pins Codex 0.144.5 and does not attribute cost per task.

### Why no dependency on `switchyard-libsy`

Adoption was costed at `5341f71` and `053a61e` and rejected. `RoutingPolicy` in `crates/roundhouse-core/src/routing/mod.rs` keeps it possible, as an option and not a dependency.

- **State is in memory.** `libsy::State` has no `Serialize`, no store trait, and no snapshot. Session state is three separate process-local structures: a `Mutex<HashMap>` with a one-hour TTL, a second map in `AffinityRouter`, and a third ledger in the advisor gate. None is behind a trait. Roundhouse must survive process death.
- **Only trivial algorithms fit a pure `choose`.** The algorithms with no `call_model` reference (Noop, Passthrough, Random, StageRouter, FallThrough, AffinityRouter, StageClassifier) duplicate logic Roundhouse already has. The novel ones (`LlmTaskClassifier`, `AdvisorGate`) must own dispatch.
- **The request type has no slot for the routing context.** `switchyard_protocol::Request` cannot carry `isl_tokens`, the cache ledger, the turn policy, or the budget. It also cannot carry candidates with their expected prefill, TTFT, cost, quality prior, and load. The only escape is the string-typed `extra_metadata`.
- **Dependency weight.** libsy needs `jsonschema 0.49.4`, `jsonptr 0.8.1`, `opentelemetry 0.32` with `tracing-opentelemetry 0.33`, `regex`, and `parking_lot`. Roundhouse has `opentelemetry 0.31` only through the dev-only Codex crates, so libsy would put two OpenTelemetry API majors in one test binary.
- **`reqwest` majors.** The Switchyard server and LLM-client crates use `reqwest 0.13.4`. The workspace uses `0.12`. Any adoption is bounded to `switchyard-libsy` plus `switchyard-protocol`.
- **The scorer is liftable, the extractor is the hard half.** The scorers are public, pure, and `Default`, so a port is about 40 lines.
- **`switchyard-protocol` header normalization was also costed.** `Metadata::from_headers` normalizes 16 fields from an alias table over Codex, Claude Code, Relay, and Dynamo headers. It is the cheapest adoption in either tree. Roundhouse derives its own session labels instead (`crates/roundhouse-sequence-id/src/label.rs`). Current Codex marks spawned children by lineage (`thread_source == "subagent"` plus a parent id), not by `subagent_kind`.

### The version-identity rule

"0.2.0" names three different libraries: the crates.io release (2026-08-10), the `v0.2.0` tag that every document tells you to pin, and main. The tag `core::algorithm` exports eleven names. Main nine days later exported seven, and only four survived by name. `AdvisorGate` is in no published release.

Measured churn, from shallow clones, so these are lower bounds:

| Tree | Window | Commits | Breaking changes |
|---|---|---|---|
| `switchyard/crates/libsy/src` | 2026-08-11 to 2026-08-19 | 14 (1.75 per day) | 7 |
| `switchyard/crates/protocol/src` | 2026-08-11 to 2026-08-19 | 9 | 2 |
| `nemo-relay/crates/types/src` | 2026-08-03 to 2026-08-19 | 6 (0.4 per day) | 1 |

Later moves: `session_affinity` was replaced by `classify_trigger` (`c7b648d0`, values `every_request` (default), `user_turn`, `new_session`). `ToolSignalProcessor` and the extractor stay private.

The rule: any adoption from a pre-1.0 neighbor pins a git rev or an immutable exact version, never a caret or a tag. The unlock condition goes beside the pin. A crates.io `=x.y.z` is as reproducible as a git rev. This is the same posture as the Dynamo pin. Coupling to Relay buys a slow-moving format. Coupling to Switchyard would buy a fast-moving library.

### The decision service and the Relay seam are gone

- The Relay `crates/switchyard` (at `c37b551`, about 1,800 lines) was an HTTP client for a separately operated Decision API, not a router. Its only quantitative request field, `prompt_token_estimate`, was hard-coded `None`. Its timeout defaulted to 25 ms over the network, a hop that the in-process `select` of Roundhouse exists to remove.
- Relay deleted that crate and the CLI `switchyard` feature in `88d1b1b` (about 4,700 lines). A config with `[[components]] kind = "switchyard"` is now a hard error with a migration diagnostic. On crates.io, `nemo-relay-switchyard` stays at 0.7.3.
- Switchyard main at `47babb1` serves only inference-proxy routes (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`, stats, metrics, health). The decision vocabulary (`decision_profile`, `baseline_route`, `reason_code`, `decision_id`) occurs zero times.
- At `053a61e`, Switchyard had no Relay plugin crate. The replacement existed only on an unmerged branch (`origin/feature/nemo-relay-plugin-owned-http-client`, tip `06dd8ea`) as a `cdylib` with `publish = false`. Switchyard also deleted its launcher.
- The variant in the Switchyard `Algorithm` trait is `Step::CallModel`. `Step::CallLlm` never existed.

## redis

`roundhouse-store-redis` is the only consumer. The Dynamo crates never touch redis.

```toml
redis = { version = "1.2", default-features = false, features = [
    "tokio-rustls-comp", "streams", "script", "connection-manager",
] }
```

It resolves to 1.2.4. The `server` crate also takes the workspace pin as a dev-dependency for one test that writes a key of the wrong type.

- **Why not the newest 1.x.** redis 1.3.0 and later carry a target-gated `tokio ^1.51` requirement (for wasm). The resolver unifies it across the workspace, and Dynamo pins `tokio = "=1.48.0"`.
- **Unlock condition.** The day the Dynamo `tokio` pin reaches 1.51, redis moves to the newest 1.x in a one-line change.
- 1.2.4 already carries the fixes of the 1.0 line: default async timeouts, the `ConnectionManager` retry fix, the TCP deadlock fix, and idempotent stream producers.
- Any 1.x unifies with the Relay `^1.1`. `nemo-relay-adaptive-0.8.2` declares `redis = "1.1"` (caret, optional), and the Relay CLI turns it on. Check it again at the next `nemo-relay-*` move.

### Client and server facts that shape the store

- **In redis-rs 1.2.4, error kinds cannot separate a wire fault from a decode fault.** The RESP parser raises `ErrorKind::Parse` on a wire-decode failure (`parser.rs:340`, `:373`). A client-side `FromRedisValue` conversion raises the same kind. A classification by `ErrorKind::Parse` would treat a protocol desync as the fault of one session. So `read_events` and `last_seq` decode the pipeline reply as a generic `Value`, where a real protocol failure stays `Backend`. Only the local conversion of the received `Value` maps to `CorruptLog`.
- **`RedisError::code` does not see a pipelined `WRONGTYPE`.** The store reads the server errors directly.
- **Script `WRONGTYPE` depends on the Redis version.** Redis 7 and later pass a script `WRONGTYPE` through with its code. 6.x wraps it as `ERR Error running script ...`. So plain pipelined reads classify a wrong-typed key as `CorruptLog` on every supported version. The lease scripts classify it on 7 and later, and fall back to `Backend` on 6.x. No lease caller branches on the difference.
- The supported floor is Redis 6.2.
- A probe against Redis 7.4 showed that `HSET` writes Lua-number counters as exact decimal integers up to 2^53-1.

See [Deploy with Redis](../operations/redis.md) and [Session store contract](store-contract.md).

## Other watched projects

These are not dependencies. Roundhouse reads them to know where the product boundary is. Each fact names the revision or date it was read from.

### TypeSafe Jev

Read from `docs.typesafe.ai` as served on 2026-09-17, and again on 2026-09-21 and 2026-09-22. Roundhouse calls it from `crates/roundhouse-fleet/src/typesafe.rs`, in the background only. See [The routing learner](../concepts/routing-learner.md).

- One endpoint: `POST https://api.typesafe.ai/v1/systemone`, bearer auth.
- The request has `state` (a string, a JSON object, or an array of text), `model` (`jev-latest`; cookbooks pin `jev-1.12`), and `questions`, a map of named typed questions.
- Question types: `noul` (one probability from 0 to 1), `choice` (`choice`, `probabilities` over every option, `confidence`), and `score` (a probability-weighted score over ordered levels, with `probabilities` and `confidence`). Roundhouse uses `choice` only.
- The answers come back under the same keys. The response carries `usage.input_tokens`, `usage.output_tokens`, and `model` (the model that answered, which can differ from the alias).
- Questions are evaluated in parallel and in isolation against one state. Several questions share one transmitted state and one HTTP call. That does not guarantee the same answers, cost, or latency as separate calls.
- Errors are 401, 422, 429, and 529. The docs say to retry 429 and 529 with backoff.
- The published price is $0.042 per million input tokens, with free output.
- Published example: 13 questions over about 11,800 tokens took 0.27 s and cost $0.000497. 13 separate calls took 2.71 s and cost 12.2 times more. A 182-option `choice` took about 0.31 s. Nothing covers a coding-agent state of 50k to 200k tokens, so do not extrapolate the 0.27 s figure to one.
- Not published: a context limit, a confidence formula, calibration evidence, a self-hosted or regional option, zero retention, fine-tuning, or determinism.
- An authenticated `GET /v1/models` on 2026-09-22 listed `jev-latest` and `jev-preview`, not `jev-1.12`.
- Because no formula or calibration is published, the code treats `confidence` as an observation to gate on, never as confidence in an outcome.

### vLLM agentic-api

Read at `d59d4b4` and again at `e35fbb2`. It is a Rust gateway that adds server-held state and server-side tools in front of one stateless vLLM endpoint. It has no model catalog, routing, cache affinity, cost, budget, or metrics.

- Routes: `/v1/responses` (POST, and GET as a WebSocket), `/v1/responses/compact`, `/v1/conversations`, `/v1/messages`, `/v1/messages/count_tokens`, and `/v1/models` (proxied).
- Absent: retrieve, delete, and cancel, stream resumption, and background execution (advertised, not implemented).
- The tool loop is bounded at `MAX_GATEWAY_TOOL_ROUNDS = 10`. Only `web_search` has a gateway-owned executor. MCP is client-side only (`rmcp` 1.8).
- Storage is three tables on SQLite or Postgres. The `conversation_id` path takes a row lock and an optimistic version. The `previous_response_id` path has no version check, so two concurrent turns fork the chain.
- At `e35fbb2` it forces `parallel_tool_calls = false` upstream while its Codex catalog claims `supports_parallel_tool_calls: true`.
- Its approval gate never reads tool annotations (`tool/mcp/handler.rs:248-262`). An MCP `isError` result goes back to the model as `{"error": "<text>"}` in an ordinary `function_call_output`, and the turn completes with 200.
- `[mcp_servers.*].headers` in `~/.agentic-api/config.toml` takes a literal bearer with no environment indirection (`tool/mcp/pool.rs:15-40`). A real key cannot be deployed on that path without writing it to disk.

Four build facts block a dependency on `agentic-server-core` (0.3.0):

- Its `reqwest` uses native-tls (OpenSSL). Roundhouse and the Dynamo `deny.toml` are rustls-only, and Cargo features unify across the graph.
- It brings two `reqwest` majors (0.12.28 and 0.13.4 through `rmcp`).
- It uses `rmcp` 1.8 against the Roundhouse 3.1.3, which would put two MCP implementations in one process.
- It pins Rust 1.98.0 against the Roundhouse 1.96.1, and its crates.io package has no `rust-version`, so cargo fails to build rather than warns. That unlocks when Roundhouse moves to 1.98.0 or later.

`axum` 0.8.9 and `tokio` 1.52.3 are not blockers on their own. The cheapest seam, if ever needed, is to copy its SSE normalizer, which reaches no `reqwest`, `sqlx`, or `rmcp`.

### Kubernetes Gateway API inference extension and llm-d

Read at `kubernetes-sigs/gateway-api-inference-extension` `84436a9` (v1.6.0 released 2026-08-17; protocol spec v1.0.0) and `llm-d-router` `e051872`.

- **The extension no longer contains a scheduler.** From v1.6.0 the endpoint picker, scheduler plugins, scorers, flow control, and several APIs moved to `llm-d/llm-d-router` and `llm-d/llm-d-inference-payload-processor` (`a70292c`, 90,903 lines deleted). What remains is the `InferencePool` v1 CRD, `InferencePoolImport`, the picker protocol spec, the conformance suite, and a round-robin reference picker.
- **The picker protocol.** The picker implements Envoy ext-proc with one gRPC stream per HTTP request. It picks at body `EndOfStream`, after the whole body is buffered. It returns the endpoint in both the `x-gateway-destination-endpoint` header and `envoy.lb` metadata. It answers 503 for no ready endpoint and 429 to drop a sheddable request. Only the elected leader reports `SERVING`.
- `InferencePool.spec.endpointPickerRef.failureMode` defaults to `FailClose`.
- The reference picker caps bodies at 10 MB. The llm-d ext-proc body path has no cap, so a huge turn fails by memory, not by a clean 413.
- **No session concept.** Nothing in the extension models a conversation. Its prefix-cache proposal rejected session affinity. llm-d-router has had an `agent-identity` plugin since `f3ae7503`. It resolves a fairness id from `x-claude-code-session-id`, `x-session-affinity`, `session-id` (Codex 0.131.0 and later), or `session_id` (Codex 0.130.x). That is a fairness queue label, not a session log, lease, or prefix admission.
- **In-flight load booking exists upstream.** Both trees carry an `inflight-load-producer` that books at `PreRequest` and releases at `StartOfStream` and `EndOfStream`. It discounts the cached prefix. The default profile does not run it, and what it books is an estimate (`bytes/4`, output times 1.5), not a measured reservation.

Why Roundhouse sits in front of such a gateway and never behind it is in [Design decisions](design-decisions.md#product-boundary-and-topology).

## Other exact pins and ceilings

These are ordinary libraries, but their pins are exact or carry an unlock condition.

| Pin | Reason |
|---|---|
| `axum = "=0.8.4"` | Matches the Dynamo workspace, so a move into Dynamo `lib/` is a no-op. |
| `clap = "=4.6.6"`, `ratatui = "=0.30.2"`, `crossterm = "=0.29.0"` | `topham` is the one binary an operator runs by hand, and a floating CLI parser changes what their muscle memory does. None constrains `tokio` or `uuid`. Toolchain 1.96.1 clears the ratatui floor of 1.88. |
| `rmcp = "3.1.3"` | The MCP server in `roundhouse-mcp`. Codex 0.146.0 connects with an `rmcp` 1.8.0 client. |
| `reqwest = "0.12.24"`, rustls only | The Dynamo `deny.toml` bans OpenSSL, and Roundhouse matches it. |
| `tokenizers = "0.21.4"` | Same major, minor, and feature shape as the Dynamo `lib/llm` pin, without the network features. Tokenizer files load from disk only. |
| Toolchain `1.96.1` | `rust-toolchain.toml`. |
| `serde_json` without `preserve_order` | `serde_json::Map` must stay a `BTreeMap`. `ItemContent::Opaque` digests canonical JSON, and a chained Relay reorders keys. With insertion order, every session behind a Relay would fork on its second turn. The guard is `an_opaque_block_is_insensitive_to_key_order`. Unlock condition: any crate in the graph that turns `preserve_order` on. Features unify, so check `ItemContent::render` and prefix admission before accepting it. |

There is no configuration-directory crate. XDG resolution is `XDG_CONFIG_HOME`, else `$HOME/.config`, the same rule Relay uses.

## When a pin moves

A synergy dependency is watched, not only pinned. An upgrade is never only a version change.

1. Read what changed upstream before the move lands. Diff the release notes or the tree between the old pin and the new one.
2. Map each change into the product. Ask whether it changes how an agent hooks up, what a turn costs, where a route can go, or what this book claims.
3. Update the chapter that states the affected contract, and name the revision it was read from.
4. Read every pinned-source claim again before work that relies on it. A claim read from one revision is stale the day the pin moves.
5. If a constraint blocks the newest version, write the unlock condition in the manifest beside the pin.

For Codex and Claude Code, the gated suites print the version they run and warn on a mismatch. For Relay, the Claude Code rig warns when the binary is not `VERIFIED_RELAY_VERSION`. For the Anthropic spec, run the `anthropic-spec-sync` skill.
