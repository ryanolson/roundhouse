<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# NVIDIA NeMo Relay ↔ Roundhouse: overlap, synergy, and risk

> **Status: evidence base.** A read-only deep dive of NeMo Relay at
> `c37b551` (workspace 0.8.0) against this tree as it stands (M0–M5),
> produced to answer "what are we reinventing, and how do these NVIDIA
> projects mesh?". The ruling that synthesizes this into direction is
> `../synergies/nemo-relay.md`, which this document exists to justify. Every
> code claim carries a file:line into one of the two trees.

Source: `/workspace/nvidia/nemo-relay` @ `c37b551`, workspace version **0.8.0**, edition 2024, `rust-toolchain.toml:5` → **1.96.1**. Roundhouse read at `/home/user/roundhouse` (README.md, PLAN-agentic-control-plane.md §1–7, plus the tree as it stands: `control/`, `interject.rs`, `roundhouse-mcp` exist; `validate/` and real frontier clients do not).

---

## A) Relay in one page

**What it is.** NeMo Relay is an *observability-and-control runtime for agent runs*, not an inference gateway. Its center of gravity is a **process-global middleware + event runtime** (`crates/core`) that any host — an app, a framework binding, or the Relay CLI — drives, plus a **CLI that transparently wraps Codex/Claude Code** to feed that runtime from an unmodified agent stack.

**Runtime model** (`crates/core/src/lib.rs:12-56`, `docs/about-nemo-relay/architecture.mdx`):

- **Scope stack** — hierarchical, task-local/thread-local execution context. A *scope* is a unit of agent work with a UUID, a parent UUID, a semantic `category` (`agent | function | tool | llm | retriever | embedder | reranker | guardrail | evaluator | custom | unknown`, `crates/types/src/api/scope.rs:26-48`), and start/end events sharing one UUID. Scopes give ownership, lineage, cleanup boundaries, and request isolation.
- **Middleware registries** — priority-ordered, name-registered, three families: *request intercepts* (rewrite the real request), *conditional-execution guardrails* (allow / reject-with-reason), *execution intercepts* (wrap or replace execution via a `next` continuation), plus *sanitize guardrails* that only change emitted observability. Type aliases at `crates/core/src/api/runtime/callbacks.rs:64-441` (`ToolConditionalFn`, `LlmConditionalFn:356`, `LlmExecutionFn:427`, `LlmStreamExecutionFn:601`, `LlmRequestInterceptFn:381`).
- **Plugin system** (`crates/core/src/plugin.rs`, `crates/plugin/`) — config-driven components (`plugins.toml`) that install middleware/subscribers. Three loading modes: built-in (Rust), **native dynamic** (stable C ABI v4, `crates/plugin/src/lib.rs:53`), and **gRPC workers** (`crates/worker-proto/proto/.../plugin_worker.proto` — bidirectional: `PluginWorker` service the host calls, `RelayHostRuntime` service the worker calls back into for `EmitMark`, `PushScope`, `LlmNext`, codec ops).
- **Events** — two kinds only: `Event::Scope(ScopeEvent)` and `Event::Mark(MarkEvent)` (`crates/types/src/api/event.rs:1135`). This is **ATOF 0.1** (`ATOF_VERSION`, `event.rs:36`), a published normative spec (`docs/reference/atof-event-format.mdx`).
- **Subscribers/exporters** — an async FIFO dispatcher delivers immutable event snapshots to ATOF (JSONL file + HTTP/WS/NDJSON stream sinks), **ATIF** trajectories (`ATIF-v1.7`, `crates/core/src/observability/atif.rs:55`), OTel traces/logs/metrics with `full`/`genai`/`openinference` projections.
- **Codecs** — normalizers for OpenAI Chat, OpenAI Responses, Anthropic Messages, Gemini, OCI GenAI, producing `AnnotatedLlmRequest`/`AnnotatedLlmResponse` (`crates/types/src/codec/`), so all downstream policy and export is provider-neutral.

**worker / worker-proto pair.** `nemo-relay-worker-proto` is the generated protobuf/tonic contract (`nemo.relay.worker.v1`); `nemo-relay-worker` (2,849 lines, `crates/worker/src/lib.rs`) is the Rust *worker-side SDK* an out-of-process plugin links to implement that service. `crates/core/src/plugin/dynamic/worker.rs` is the host side. It is a plugin transport, unrelated to Dynamo workers.

**What it deliberately is not**: it does not own conversation state, does not schedule inference, has no tenancy, budgets, principals, or spend ledger (verified: `grep budget|quota|tenant|principal` across `crates/` returns only cache-byte budgets and ACG sharing scopes). `docs/about-nemo-relay/ecosystem.mdx:35` states the Dynamo relationship explicitly: *"Applications can send Relay-managed model calls to a Dynamo-served endpoint. Relay does not schedule or operate the inference workers."*

---

## B) Overlap table

Legend — **DUP** = roundhouse is reinventing something Relay ships; **COMP** = meshes cleanly, different layer; **DISJ** = no contact; **CONFL** = two sources of truth if both run in one path.

| # | Roundhouse capability | Relay counterpart | Class | Note |
|---|---|---|---|---|
| 1 | Append-only per-session event log, `seq`, single-writer fenced lease (M0, shipped) | none — Relay's event stream is a fire-and-forget subscriber bus with an in-memory buffer (`atif.rs:335-345`), no ordering guarantee beyond FIFO, no durability, no replay | **DISJ** | Relay has no durable log. Its ATIF exporter sorts by timestamp (`atif.rs:3441`), which roundhouse's `seq` makes unnecessary. |
| 2 | Conversation ownership / delta upload / prefix admission (`Compat::bound_session`) | none. Relay explicitly *bypasses* stateful requests: `previous_response_id`/`store`/`conversation` are cache-bypass reasons (`crates/adaptive/src/response_cache/key.rs:115-135`) | **DISJ** | The thesis roundhouse exists for has no Relay analogue. |
| 3 | Incremental tokenization + Dynamo block/sequence hashing | none | **DISJ** | |
| 4 | Local Dynamo selection (`SelectionService::select`, `effective_prefill_tokens`, select/reserve split) | none — Relay names Dynamo only as an *upstream endpoint* and injects `x-dynamo-session-id` / `x-dynamo-parent-session-id` (`crates/core/src/api/shared.rs:25-28,196-215`) | **COMP** | Relay already stamps Dynamo session lineage on outbound requests. Free correlation key. |
| 5 | Cross-provider routing on cache-adjusted expected prefill (`RoutingPolicy`, `AffinityPolicy`) | Switchyard plugin: routing is *delegated over HTTP* to an external Decision API; the only cost signal in the request is `prompt_token_estimate`, and it is hardcoded `None` (`crates/switchyard/src/component.rs:1001`) | **COMP** (near-DUP in shape, not in substance) | Same *seam*, opposite *content*. See §C.2. *[2026-08-21 @ `1a54812`: **the cited file no longer exists.** Relay deleted `crates/switchyard` outright in `88d1b1b` (2026-08-19), `refactor(switchyard)!: remove built-in service integration (#811)` — the crate, the CLI `switchyard` feature and the service-backed component, ~4,700 lines; a config with `[[components]] kind = "switchyard"` is now rejected with a migration diagnostic (`HEAD:crates/cli/src/server/mod.rs`). The claim is still true *of `c37b551`*, which is what this document reads, and is left as written per the no-silent-rewrite rule. `HEAD:docs/reference/migration-guides.mdx:75-101` hands the integration to Switchyard 0.3.0; the concurrent Switchyard re-read found main still at 0.2.0 with the replacement plugin on an unmerged branch, so this seam is currently open on both sides — see the Relay-half and Switchyard-half sections of `relay-switchyard-dedup-deep-dive.md`.]* |
| 6 | `EscalationPolicy` (audit-every-N, latch-on-trouble) | the Switchyard "StageRouter" profile, which lives in `NVIDIA-NeMo/Switchyard`, not here. `crates/switchyard` is only a client | **DUP-by-reference** | roundhouse's README already ruled correctly. See §C.2. |
| 7 | `FrontierQuote`/`CacheLedger` frontier cache modelling (`CacheModel::Deterministic`/`InactivityDecay`, `ledger.rs:33-53`) | ACG `CacheEconomics` (`write_short_multiplier`, `write_long_multiplier`, `read_multiplier`) + `min_cacheable_tokens` + `max_cache_breakpoints` (`crates/adaptive/src/acg/capability.rs:63-99`) and breakeven-reads planning (`acg/economics.rs:16-29`) | **COMP** | Roundhouse *predicts* whether a remote cache is warm. ACG *causes* it to be warm by planting breakpoints. Genuinely complementary halves of the same economics. |
| 8 | Rate card / `ROUNDHOUSE_CATALOG` / `ProviderPricing` (4 rates) | `pricing` plugin: `PricingCatalog` with aliases, `TokenPricingRates` (`model_pricing.rs:430`), **tiered `TokenRateSchedule`**, `PromptCachePricing{read_accounting: IncludedInPromptTokens\|Separate}` (`:573-586`), `pricing_as_of` + `pricing_source` provenance, multi-source precedence, `nemo-relay model-pricing validate/resolve` CLI | **DUP** (Relay's is strictly richer) | Roundhouse hand-rolls a narrower version of a published, CLI-validated catalog. |
| 9 | Savings model: `frontier_spend_measured/estimated`, correlary, capability gate, `coverage_token_fraction` | `LlmOptimizationSummary`: `baseline_model`/`effective_model`, `baseline_usage`/`effective_usage`, `baseline_cost`/`actual_cost`, `estimated_cost_saved`, `status: Complete\|Partial` + `limitations[]`, evidence `Observed\|Estimated` (`crates/types/src/codec/optimization.rs:143-293`) | **DUP** in schema, **roundhouse stronger in rigor** | Relay has *no capability gate*: `baseline_model` is whatever the router declared. Roundhouse's `quality_prior`/`capability_band` is the missing safeguard. |
| 10 | `enforce_usage_reporting` (adds `stream_options.include_usage`, never overrides) | **absent.** Relay only *classifies* `stream_options` as portable (`crates/switchyard/src/translation.rs:168-173`); it never injects it | **COMP — roundhouse ahead** | This is a real Relay gap roundhouse could contribute. *[2026-08-21 @ `1a54812`: the cited file went with `crates/switchyard` in `88d1b1b` — see the note on row 5. **The conclusion strengthens rather than weakens**: Relay no longer even classifies `stream_options`, so T-1 (contribute `enforce_usage_reporting`) has a larger gap to land in, not a smaller one. The contribution target moves from the deleted plugin to Relay's own gateway path.]* |
| 11 | `Accounting::Estimated` for unreported usage | `LlmOptimizationEvidenceQuality::{Observed,Estimated}` + `estimation_method` (`optimization.rs:143-168`); `CostSource::{ModelPricing,ProviderReported}` (`crates/types/src/codec/response.rs:102`) | **COMP** | Vocabulary matches almost 1:1. |
| 12 | Control plane: Project/User/Membership/ApiKey/Credential, `rh_turn_`/`rh_admin_` (M1) | **none** | **DISJ** | Relay has one credential concept: a random per-invocation loopback proxy token (`crates/cli/src/provider_auth.rs:18-38`). |
| 13 | Budget grant/settle ledger, degrade-to-local, overflow valve (M3) | **none** anywhere in Relay. ACG has an unused `RoutingPolicy.session_cost_cap: Option<f64>` field (`crates/adaptive/src/acg/policy.rs:119-121`) and nothing enforces it | **DISJ** | Roundhouse's genuinely novel territory, confirmed. |
| 14 | `TurnPolicy` + narrow-only overlays + `RoutingContext` | Relay "policy" = middleware chains + guardrail *reject-with-reason*; the CLI's `policy.rs` is **plugin-trust** policy, not run policy (`crates/cli/src/plugins/policy.rs:196-270`) | **COMP** | Different axis: Relay decides *whether work runs*; roundhouse decides *what may serve it*. |
| 15 | Interception: pass-through proxy for Codex device login (ruled, §3 of PLAN) | **already built and shipping**: `crates/cli/src/agents/codex/launch.rs:199-206`, `agents/claude/launch.rs:13-95`, `agents/codex/alignment.rs:23,85-126` | **DUP + CONFL** | Relay does the exact thing roundhouse ruled, including `https://chatgpt.com/backend-api/codex`. See §C.1. |
| 16 | MCP control surface `/mcp` — `status`, `init_session`, `declare_intent`, `prefer`, `set_quality_floor`, `fetch_steer`, … (M5) | Relay's MCP is a **stdio lifecycle client with `tools/list` → `[]`** (`crates/cli/src/mcp/protocol.rs:72-76`). Zero tools; it exists only to keep the shared gateway alive | **DISJ** | No tool-name collision, no functional overlap. Roundhouse's tool surface is genuinely novel — confirmed. |
| 17 | Synthetic tool-call steering / `Interjection::Complete` (M4 seam shipped) | **none.** Relay's hook adapters *always allow*: `permissionDecision: "allow"` (`crates/cli/src/agents/claude/adapter.rs:46-53`), Codex gets `{}` (`agents/codex/adapter.rs:32-35`). Blocking exists only as guardrail-rejection → HTTP 403 → CLI exit code 2 (`crates/cli/src/error.rs:145-149`, `commands/mod.rs:43-44`) | **COMP** | Relay can *stop* a tool. It cannot *inject* one. |
| 18 | Validate/steer loop, frontier judge, arms Live/Shadow/Placebo (M6, planned) | ACG/adaptive learners: offline, statistical, no LLM-judge. `Learner::process_run` (`crates/adaptive/src/learner/traits.rs:16-31`) | **COMP** | Different control loops. See §C.3. |
| 19 | Turn dedup / replay by content-hash `turn_id` | `response_cache`: exact-match request→response cache with canonical JSON key, streaming replay, byte budgets (`crates/adaptive/src/response_cache/`) | **CONFL if both on** | Two independently-keyed caches over one request path. |
| 20 | Metrics fold + `/v1/metrics/dashboard` | OTel metrics/logs/traces export + `MetricEnvelope` marks (`crates/types/src/api/event.rs:201-260`) | **COMP** | Relay exports; roundhouse aggregates. Roundhouse has no OTel path today. *[2026-08-21: **this row is the one that decides the S2 pin, because `MetricEnvelope` is 0.8-only.** It, `MetricMeasurement`, `InstrumentDescriptor`, `MetricAttributes`, `validate_metric_measurements`, `LogSeverity` and the constants `METRIC_DATA_SCHEMA_NAME`/`_VERSION` all live in `0.8.0-rc.1:src/api/event.rs:36-723` and **none of them exist in the published 0.7.3** (865 lines to 0.8's 1,577). Round-2 item 2 pins `nemo-relay-types` 0.7.3, which is byte-equal to HEAD for `codec/optimization.rs` and the whole ATOF envelope but cannot express a single metric mark. So if "S2 emission with M6's metrics work" means emitting Relay-conformant metrics, the pin must move to `=0.8.0-rc.1` or a git rev; if it means summaries plus scope/mark events, 0.7.3 is equivalent. Evidence and the three forms of the pin: `relay-switchyard-dedup-deep-dive.md`, notes at finding 11 and §2(e).]* |
| 21 | SSE/Responses streaming transport, resumption (`starting_after`/`Last-Event-ID`) | Relay's gateway decodes/re-encodes SSE per chunk ("Option B", `crates/cli/src/gateway/mod.rs:70-76`) but has no resumption | **CONFL (latency)** | Two SSE re-encodes stacked if chained. |
| 22 | Codex conformance oracle via pinned `codex-api`/`codex-protocol` | Relay pins by *behavioral* version check (`codex-cli >= 0.143.0`), not by depending on Codex crates | **COMP — roundhouse ahead** | Roundhouse's parser-level oracle is stronger evidence. |
| 23 | PII/secret handling: `a_quote_never_carries_a_secret` scan test (M7) | `nemo-relay-pii-redaction`: config-driven detectors + `remove/redact/regex_replace/hash/mask`, codec-aware overlays for 5 surfaces | **COMP** | Different scope (Relay redacts payloads, roundhouse protects credentials); Relay's is directly consumable. |
| 24 | Multi-language surface | Python/Node/Go/C-FFI bindings for the whole runtime | **DISJ** | Roundhouse is HTTP-only by design. |

---

## C) The five deep dives

### C.1 The interception mechanism — and how it compares to roundhouse's pass-through ruling

Relay's answer to "no changes to the existing agent stack" is: **launch the agent yourself with injected config, and stand a loopback HTTP proxy in the provider path.** Three mechanisms, all in `crates/cli`:

**(a) A local gateway.** An axum server exposing exactly the provider routes (`crates/cli/src/gateway/routes.rs:51-63`):
```
/responses, /v1/responses            → OpenAiResponses
/chat/completions, /v1/chat/completions → OpenAiChatCompletions
/models, /v1/models                  → OpenAiModels
/v1/messages                         → AnthropicMessages
/v1/messages/count_tokens            → AnthropicCountTokens
```
It buffers the body once, opens a *managed* LLM scope, runs the middleware pipeline, forwards upstream, and re-encodes streaming responses chunk-by-chunk so the runtime stays in the hot path (`gateway/mod.rs:58-83`). Upstream base is configurable per family (`configuration/types.rs:24-27`, default `https://api.openai.com/v1` / `https://api.anthropic.com`), overridable with `--openai-base-url` (`commands/root.rs:67-74`).

**(b) Per-agent launch injection.**

*Codex* (`agents/codex/launch.rs:36-53,199-206`) — `--config` overrides on the argv, not a config file:
```toml
features.hooks=true
features.multi_agent_v2.enabled=false
model_provider="nemo-relay-openai"
model_providers.nemo-relay-openai={name="NeMo Relay OpenAI",base_url=<gateway>,
  wire_api="responses",requires_openai_auth=true,supports_websockets=false,
  env_http_headers={"x-nemo-relay-proxy-token"="NEMO_RELAY_PROXY_CREDENTIAL"}}
hooks.<Event>=[...]                    # 10 events
hooks.state={...trusted_hash=sha256:...}   # canonical-JSON hash per handler
```

*Claude Code* (`agents/claude/launch.rs:31,45,80-93`) — env + a synthesized temp plugin dir:
```
ANTHROPIC_BASE_URL=<gateway>                          (env and --settings.env)
ANTHROPIC_CUSTOM_HEADERS="x-nemo-relay-proxy-token: nrp_…"
--plugin-dir <tmp>/  (with .claude-plugin/plugin.json + hooks/hooks.json)
--settings <tmp>/settings.json   (merged over any user --settings)
```

**(c) Hook forwarding.** Every generated hook is `nemo-relay hook-forward <agent> --gateway-url … --transparent-run`; it reads the hook payload on stdin, POSTs it to the gateway, and maps a `403 nemo_relay_guardrail_rejected` to **exit code 2** = block (`hooks/delivery.rs:23-95`, `hooks/response.rs:70-90`, `error.rs:120-149`, `commands/mod.rs:43-44`). `fail_closed` is opt-in per event; `PreToolUse`/`PermissionRequest`/`pre_tool_call` are the fail-closed-eligible set (`hooks/encoding.rs:412-414`).

**Auth handling — this is the exact ruling roundhouse made.** `provider_auth.rs` mints a 32-byte random `nrp_<hex>` per invocation and **consumes** it at the gateway boundary *before* intercepts run, constant-time, then leaves any genuine provider credential untouched (`provider_auth.rs:46-86`). Then `agents/codex/alignment.rs`:

```rust
const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";   // :23
fn has_chatgpt_auth_token(headers) -> bool {                                   // :121-126
    value.starts_with("Bearer eyJ") || value.starts_with("Bearer at-")
}
pub(crate) fn chatgpt_upstream_url_if_needed(...) -> Option<String> {           // :85-93
    (is_openai_route(route) && has_chatgpt_auth_token(headers) && !has_replacement_key)
        .then(|| chatgpt_upstream_url(path_and_query))
}
```

**Comparison with roundhouse's PLAN §3 pass-through ruling — precise deltas:**

| Point | Roundhouse plan | Relay as built | Verdict |
|---|---|---|---|
| Codex sends its ChatGPT bearer to a custom `base_url` | asserted from pinned source | relied on in production; token *shapes* (`eyJ`, `at-`) are matched | **Confirmed independently** |
| Upstream for ChatGPT-authed traffic | `https://chatgpt.com/backend-api/codex`, "one empirical check left" | same constant, live | **Confirmed** |
| `requires_openai_auth` | "leave unset — keeps the plain bearer path, avoids Agent-Identity bootstrap" | **set to `true`** (`launch.rs:201`) | **Direct disagreement. Worth resolving before M7** — Relay ships this against `codex-cli ≥ 0.143.0`; roundhouse read rev `6344a65`. One of the two reads is stale, or `requires_openai_auth` changed meaning. |
| Second header alongside `Authorization` | `env_http_headers { "X-Roundhouse-Key" = "ROUNDHOUSE_API_KEY" }` | `env_http_headers { "x-nemo-relay-proxy-token" = "NEMO_RELAY_PROXY_CREDENTIAL" }` | **Identical mechanism, validated** |
| Don't name the provider `"OpenAI"` (`is_openai()` side effects) | explicit warning | provider named `nemo-relay-openai` | **Consistent** |
| Credential never persisted, redacted from logs | rule | `remove_provider_credentials`, `set_secret_env`, header allowlist `observable_headers` | **Same posture** |
| Claude Code path | "future-proofing for a Messages surface" | **built today** via `ANTHROPIC_BASE_URL` + `ANTHROPIC_CUSTOM_HEADERS` | **Relay ahead** |
| Fallback when an `OPENAI_API_KEY` exists | not addressed | strips the ChatGPT JWT and substitutes the API key (`alignment.rs:97-110`) | **Relay ahead** |

**Bottom line:** roundhouse's pass-through ruling is correct and already has a production reference implementation inside NVIDIA. Building it a second time is the single clearest duplication in this comparison.

### C.2 `crates/switchyard` — what it actually is, and whether roundhouse should depend on it

**The crate is not a router.** `crates/switchyard` is a ~1,800-line *HTTP client plugin* for an external **Switchyard Decision API** that lives in `NVIDIA-NeMo/Switchyard` on branch `topic/nemo-relay-integration` @ `8f9db9a6` (`crates/switchyard/README.md`, `docs/configure-plugins/switchyard/about.mdx:20-28`). The escalation/StageRouter algorithm roundhouse's `policy.rs` refers to is in *that* repo. This crate contains no routing algorithm at all.

**And it is deprecated.** README.md header: *"The experimental `nemo-relay-switchyard` plugin **will be removed in NeMo Relay 0.8** and replaced by a Switchyard-owned native plugin."* The workspace is already at 0.8.0 (`Cargo.toml:22`), the docs page is titled "Switchyard (Deprecated)", the crate is behind an off-by-default `switchyard` CLI feature (`crates/cli/Cargo.toml:25`), and the replacement does not exist in this tree.

**Its full surface** (`crates/switchyard/src/`):

- **Config** `SwitchyardConfig` (`component.rs:188-230`): `mode: enforce|observe_only`, `decision_api_url`, `decision_profile_id`, `request_materialization`, `context_mode: payload_only|atof_required`, `decision_timeout_millis` (default **25 ms**), `max_retries` (3), `targets: BTreeMap<backend_id, TargetBinding>`, `default_targets` per protocol, `atof_endpoint_name`.
- **Registration**: two execution intercepts, `"decision"` (`LlmExecutionFn`) and `"decision_stream"` (`LlmStreamExecutionFn`), at a configured priority (`component.rs:352-380`). Activation performs a bounded `/health` probe and **fails registration** unless `{"status":"ok"}` (`component.rs:557`).
- **Wire contract** (`contract.rs`): `RoutingRequest{schema_version, decision_profile, identity, protocol, request_summary, current_request?, attempt}` (:116) → `RoutingDecision{decision_id, router, route: RoutingTarget{tier, target_model, backend_id, target_protocol_profile, target_endpoint}, baseline_route?, confidence?, reason_code?, reason_summary?, metadata}` (:160-187).
- **Signals it consumes** (`component.rs:942-1017`): `client_requested_model`, `tool_count_in_payload`, `has_system_prompt`, and — **`prompt_token_estimate: None`, hardcoded** (`:1001`). Optional request materialization from `none` → `summary_only` → `latest_user_prompt` → `recent_message_window` → `annotated_request` → `full_body`. For `atof_required` profiles, history reaches the router *out of band* via an ATOF HTTP stream sink into Switchyard's own accumulator (`README.md`, `docs/.../configuration.mdx:194-200`).
- **Escalation/latching**: not here. What *is* here is **retry-driven re-decision**: on a retryable upstream failure, Relay re-asks the Decision API with `previous_route` + `retry_reason` (`component.rs:678-690, 964-1023`), up to `max_retries`.
- **Safety**: every decision must match an *exact* pre-declared `TargetBinding` (model + protocol + endpoint), else rejected (`validate_target`, `:1068-1090`). Relay keeps credentials, dispatch, retries, and the trusted per-protocol fallback.
- **Accounting**: emits an `LlmOptimizationContribution{kind: model_routing, model_transition{baseline, effective}, payload{decision_id, tier, reason_code, …}}` (`:1093-1130`) — but *only when the router supplied a `baseline_route`*, and only if that baseline also validates.

**Could roundhouse depend on it instead of reproducing `EscalationPolicy`? No — and roundhouse's existing ruling should be strengthened, not revised.** Evidence:

1. **Wrong artifact.** Depending on `nemo-relay-switchyard` gets you an HTTP client for a service, not an algorithm. It also pulls `nemo-relay` (the whole core runtime: OTel ×3, tonic, libloading, object_store, spdlog-rs) as a hard dependency (`crates/switchyard/Cargo.toml:24`).
2. **Deprecated at the version in hand.** Adopting a crate whose own README schedules its removal in the release you would be adopting is not a dependency, it's a migration debt.
3. **API fit is poor.** `RoutingContext`/`TurnPolicy` carry `isl_tokens`, per-candidate `expected_prefill_tokens`, `matched_prefix_tokens`, `expected_cost_usd`, `quality_prior`, `CacheLedger` state, `frontier_turns`, and a budget grant. The Decision API accepts *none* of that — its only quantitative field is `prompt_token_estimate`, which the client never fills.
4. **Two round trips, one of them networked.** Decision timeout defaults to 25 ms and requires a *separately operated service* Relay refuses to start without. Roundhouse's `select` is an in-process async method chosen precisely to remove a TCP hop.
5. **Latency/statefulness collision.** Roundhouse's README already names the real blocker for the upstream library — in-memory state with no pluggable persistence, colliding with surviving process death. The Decision-API shape does not fix that; it moves the state into another process roundhouse doesn't own.

**What roundhouse *should* take from Switchyard** is not code but three schema ideas, all cheap:

- `baseline_route` as a **first-class field on the decision**, not a dashboard-time reconstruction.
- `reason_code` (machine) beside `reason_summary` (human). `DecisionRecord`'s reason is a formatted string today — ungroupable in a dashboard.
- `mode: observe_only` as a *routing* rollout mode. Roundhouse has `ValidationArm::Shadow` for the judge but no shadow mode for the router itself.

### C.3 `crates/adaptive` — what it adapts, on what feedback, with what loop

`nemo-relay-adaptive` (134 files) is a **background statistical learner** installed as one plugin component (`kind = "adaptive"`), with five sub-behaviors. Control loop shape (`crates/adaptive/src/subscriber.rs:24-70`, `learner/traits.rs:16-31`):

```
ATOF events → subscriber → RunRecord (batched at agent-scope boundaries)
           → Learner::process_run(run, backend, hot_cache)
           → StorageBackend (in-memory | Redis)  +  HotCache
           → read synchronously at request time by an LLM request intercept
```

The five behaviors:

1. **Telemetry** — observe only, build `RunRecord`s.
2. **Adaptive hints** (`adaptive_hints_intercept.rs`) — a prediction trie over observed runs emits `AgentHints{osl, iat, priority, latency_sensitivity, prefix_id, total_requests}` (`types/metadata.rs:44-57`), injected as `x-nemo-relay-adaptive-agent-hints` (`intercepts.rs:25`). **These are Dynamo SLA-planner/KV-router-shaped hints**: predicted output-sequence length, inter-arrival time, scheduling priority, prefix identity. Predictions come from a t-digest per trie node (`p90` output tokens, mean interarrival, mean remaining calls).
3. **Tool parallelism** (`tool_parallelism_learner.rs`) — modes `observe_only | inject_hints | schedule`; derives fan-out plans from observed runs.
4. **Adaptive Cache Governor (ACG)** — the deep one. Builds a **PromptIR** from the annotated request (`acg/prompt_ir.rs`, `ir_builder.rs`), canonicalizes and extracts variables, runs **stability analysis** over a bounded observation window per profile key (`acg_learner.rs:56-90`, `MIN_ACG_OBSERVATIONS = 2`, `acg/mod.rs:9`), then does **economics-aware breakpoint planning**:
   ```rust
   fn breakeven_reads(&self, write_multiplier) = (write_multiplier - 1.0) / (1.0 - read_multiplier)
   fn marginal_net_savings(tokens, expected_reads)
       = tokens * ((1.0 - read_multiplier) * expected_reads - (write_short_multiplier - 1.0))
   ```
   (`acg/economics.rs:16-29`.) It then places up to `max_cache_breakpoints` breakpoints at semantic boundaries ranked `user > retrieval > tool_cluster > system > structured > generic` (`economics.rs:52-83`), respecting `min_cacheable_tokens`. Provider plugins: `passthrough`, `anthropic` (4 `cache_control` breakpoints), `openai`. Policy document (`acg/policy.rs`): `CachePolicy{min_stability_score, min_evidence_count, default_sharing_scope, warm_first_enabled}`, `RewritePolicy{allowed_transformations, require_validation, max_auto_risk_tier}`, `RetentionPolicy`, and a `RoutingPolicy{default_model_class, archetype_overrides, session_cost_cap}` that nothing enforces.
5. **Response cache** — exact-match request→response with RFC 8785 canonical keys, streaming replay, byte-budget eviction, and explicit statefulness bypass (`response_cache/key.rs:115-135`).

The **adaptive-tuning skill** (`skills/nemo-relay-plugin-adaptive-tuning/SKILL.md`) is the operating discipline, and it is remarkably aligned with roundhouse's M6 posture: *"Observe first, compare against a baseline, then enable one behavior change at a time"*; *"Do not tune from a single run"*; *"Revert adaptive behavior when it increases the failure rate."*

**Does it overlap M6 (frontier-judge validate/steer)? No — it complements it, on three axes:**

| | Relay adaptive | Roundhouse M6 |
|---|---|---|
| Feedback signal | statistical over many runs (t-digest, stability score, observation counts) | per-turn behavioral signals (no-progress repeat, ping-pong, tool-failure streak, cost anomaly) + an LLM judge |
| Latency | offline/background; hints read from a hot cache | inline, holds the turn, pays a side-call |
| Action | rewrite the request (breakpoints, hints, headers) | change *who serves* the turn (Escalate), or answer the turn with a synthetic tool call (Steer) |
| Cost of being wrong | a colder cache | the Intervention Paradox — a disrupted agent |
| Instrumentation | baseline-vs-after by hand | `Live \| Shadow \| Placebo` arms stamped in `SessionCreated` |

The one true meeting point: ACG's stability analysis is **exactly the trigger-signal class roundhouse's M6 gate requires** — a prompt whose stability score collapses mid-session is evidence the agent's context has gone off-distribution, computable with no model call.

### C.4 Event / trajectory schemas — could roundhouse emit ATOF/ATIF instead of inventing?

**ATOF 0.1** (`crates/types/src/api/event.rs`, spec at `docs/reference/atof-event-format.mdx`). Envelope on every event: `kind ∈ {scope, mark}`, `atof_version`, `uuid`, `parent_uuid`, `timestamp`, `name`, `data`, `data_schema{name,version}`, `metadata`. `ScopeEvent` adds `scope_category ∈ {start,end}`, `attributes`, `category`, `category_profile{model_name, tool_call_id, subtype, tool_result_annotation, annotated_request, annotated_response, ..extra}` (`event.rs:910-1047`). A start/end pair shares one `uuid`. `EventCategory` is a *string newtype*, deliberately open for forward-compat (`event.rs:733-737`).

**ATIF v1.7** (`crates/core/src/observability/atif.rs:55-320`) — `AtifTrajectory{schema_version, session_id, trajectory_id, agent, steps, notes, final_metrics, continued_trajectory_ref, subagent_trajectories, extra}`; `AtifStep{step_id, source ∈ {system,user,agent}, message, timestamp, model_name, reasoning_effort, reasoning_content, tool_calls, observation, metrics, llm_call_count, is_copied_context, extra}`; `AtifMetrics{prompt_tokens, completion_tokens, cached_tokens, cost_usd, prompt_token_ids, completion_token_ids, logprobs, extra}`; `AtifFinalMetrics{total_*, total_cost_usd, total_steps, extra}`; `AtifStepExtra{ancestry, invocation, llm_request, llm_response, tool_ancestry, tool_invocations}`.

**The roundhouse-session → ATIF projection, concretely.** Roundhouse's log already carries every input:

| ATIF field | Roundhouse source |
|---|---|
| `session_id` / `trajectory_id` | `SessionId` (post-M1: `{project}/{user}/{cache_key}`) |
| `steps[].source: "user"` | `Item{role: User}` from `ItemAppended` |
| `steps[].source: "agent"`, `.message` | `Item::assistant_text` at `ResponseCompleted` |
| `steps[].tool_calls[]` | `ItemContent::ToolCall{call_id, name, arguments}` — **including a synthetic steer** |
| `steps[].observation.results[].source_call_id` | `ItemContent::ToolResult` |
| `steps[].model_name` | `DecisionRecord.target` |
| `steps[].metrics.{prompt,completion,cached}_tokens`, `cost_usd` | `Usage` + rate card |
| `metrics.extra.reasoning_tokens` | `Usage::reasoning_tokens` — ATIF explicitly names this key (`atif.rs:143`) |
| `step.extra.invocation.{start,end}_timestamp` | log timestamps; TTFT already derivable |
| `step.extra` (routing) | `DecisionRecord`: chosen target, `considered`, `turn_policy_digest`, `budget_state` |
| `final_metrics.total_cost_usd` | `MetricsSnapshot` fold |

**What's missing on each side:**

*Missing in ATIF for roundhouse's story* — no field for **which candidates were considered and at what price**, no field for **local-vs-hosted serving mode**, no place for the **measured/estimated split**, no per-step `seq`, no lease/writer identity, no resumption cursor. All fit in `extra` (typed, `data_schema`-tagged, the documented extension path), but they are `extra`, not first-class.

*Missing in roundhouse for ATIF* — `parent_uuid` lineage per turn (roundhouse has `seq`, a total order, but no tree), `AtifAgentInfo{name, version, tool_definitions}`, and `reasoning_content` (dropped at `wire.rs:107`).

*The `LlmOptimizationSummary` is the sharper fit than ATIF.* Roundhouse's savings model maps almost field-for-field:

| Roundhouse | Relay |
|---|---|
| correlary (declared or shape-inferred) | `LlmOptimizationSummary.baseline_model` |
| the local model actually used | `.effective_model` |
| counterfactual at same token counts incl. cached fraction | `.baseline_usage` / `.baseline_cost` |
| `frontier_spend_*` | `.actual_cost` |
| savings figure | `.estimated_cost_saved` + `.currency` |
| `Accounting::{Measured,Estimated}` | `LlmOptimizationEvidenceQuality::{Observed,Estimated}` |
| unpriced-because-no-comparable-model | `status: Partial` + `limitations: ["…"]` |
| `DecisionRecord` audit | `contributions: Vec<LlmOptimizationContribution>` (bounded: 64 entries, 16 KB each, 256 KB total) |

The **one thing roundhouse has that Relay does not** is the capability gate. Relay's `baseline_model` is whatever a router asserted; nothing stops a 7B being priced against a flagship — the exact trap roundhouse's README is built around. That is roundhouse's strongest upstream contribution.

Additional weight: there is a **reference ATOF→ATIF converter** in NeMo-Agent-Toolkit (`nvidia_nat_atif`, linked from `docs/reference/atof-event-format.mdx:17`) with registries keyed on `data_schema`. Roundhouse emitting ATOF with a declared `data_schema` for routing/decision marks plugs into an existing pipeline rather than needing new consumers.

### C.5 The policy model — what "control over agent runs" actually means

Relay's policy is **middleware placement**, not a policy language. Five hook points, ordered (`crates/core/src/lib.rs:44-56`): request intercepts (rewrite the real request), sanitize-request guardrails (observability only), conditional-execution guardrails (`None` = allow, `Some(reason)` = reject — the *only* enforcement primitive), execution intercepts (wrap/replace/retry via a `next` continuation — where Switchyard and the response cache live), sanitize-response guardrails. Plus event sanitizers and metadata injectors on the emission path (identity and lifecycle fields deliberately not rewritable, `event.rs:1063-1078`).

**So what can policy do?** `block` ✅, `redact` ✅, `route` ✅ (execution intercept), `transform/rewrite` ✅, `retry/fan-out` ✅. **`steer` ❌** — nothing can synthesize a tool call into the model's output stream. **`hold the turn and ask a judge` ❌** as a first-class concept.

**Overlap with roundhouse's `TurnPolicy`/overlays/MCP steering:** small and mostly complementary. Relay's `LlmConditionalFn` and roundhouse's `TurnPolicy::admits` sit on *different questions*: "may this call happen at all" vs "which of these candidates may serve it". The **narrow-only rule** has no Relay equivalent — Relay's plugin priorities are an ordering, not a lattice; a later intercept can widen what an earlier one narrowed. Roundhouse's totality guarantee is genuinely stronger and worth keeping.

---

## D) Synergy seams — concrete

### D.1 Roundhouse → Relay (what roundhouse consumes)

| # | Seam | Crate / API | Cost | Roundhouse gains |
|---|---|---|---|---|
| **1** | **Emit ATOF from the session log** | `nemo-relay-types` — a **light** crate: bitflags, chrono, serde, typed-builder, `uuid = "=1.18.1"` **exactly matching roundhouse's pin** | **S** | A published spec, an existing ATOF→ATIF converter, stream sinks to any collector. Roundhouse's log is the perfect producer: totally ordered, durable, replayable. *[2026-08-21: the deps list is right and the `uuid` half is now wrong — roundhouse's is a **caret** `1.18.1` (`Cargo.toml:95`) resolved to **1.24.0** (`Cargo.lock:5758`), Relay's an **exact** `=1.18.1`, so adopting the crate pins our whole graph down six releases and imposes a ceiling. It does resolve (every uuid dependent in our lock accepts 1.18.1), but the unlock condition belongs in the manifest. Two crate names are genuinely new — `typed-builder` and its macro — and `schemars` stays off by default. MSRV is **undeclared**: no `rust-version` key in the published tarballs and `rust_version: null` on the crates.io API, so 1.96.1 is Relay's dev toolchain, not a floor cargo will enforce. On "an existing ATOF→ATIF converter": alive and larger (1,039 lines at `NeMo-Agent-Toolkit@c933737`), but its mark-extractor registry ships empty, so a `data_schema` we declare is consumed as a JSON string in a `system` step rather than structurally — see the S-b note in `relay-switchyard-dedup-deep-dive.md`.]* |
| **2** | **Emit ATIF trajectories** | re-implement the ~15 structs rather than depend on heavy `nemo-relay` core *[2026-08-21: **twelve, not ~15** — `AtifTrajectory:301`, `AtifAgentInfo:63`, `AtifStep:81`, `AtifMetrics:122`, `AtifFinalMetrics:151`, `AtifToolCall:174`, `AtifObservation:188`, `AtifObservationResult:195`, `AtifSubagentTrajectoryRef:212`, `AtifAncestry:226`, `AtifInvocationInfo:244`, `AtifStepExtra:267` in `HEAD:crates/core/src/observability/atif.rs`, which is byte-identical to `ca08901`; `AtifExporter:345` is the host-side plugin, not a wire type. Still `ATIF-v1.7`, no v1.8. One qualifier on "the spec is published": the only field-level sources are that Apache-2.0 Rust file and NeMo-Agent-Toolkit's `atif-step-extra-guide.md` / `atof-to-atif-conversion-guide.md`; `docs/configure-plugins/observability/atif.mdx` is a `plugins.toml` configuration doc, not a schema. So the re-implementation is from licensed source and owes attribution in the M6 judge-prompt form.]* | **S–M** | `GET /v1/sessions/{id}/trajectory` handing eval pipelines a standard artifact — producible *from a cold replay of stored events*, which Relay's in-memory exporter cannot do. |
| **3** | **Adopt the pricing catalog schema** | copy the ~300-line schema (tiered rates, aliases, `pricing_as_of`/`pricing_source` provenance), not the crate | **M** | Provenance fields roundhouse's CLAUDE.md already wants for the OpenRouter import; a validating CLI pattern. Keep `quality_prior`/`capability_band` as roundhouse-owned extensions. |
| **4** | **Emit `LlmOptimizationSummary`/`Contribution`** | `nemo-relay-types` `codec::optimization::*` | **S** | The savings story becomes a standard machine-readable object. Near-exact field parity. |
| **5** | **Consume `AgentHints` as a routing input** | header `x-nemo-relay-adaptive-agent-hints` | **S** to read | `osl` (predicted output length) directly improves the `expected_output_tokens` that sizes the budget grant — a known honest limitation of M3. |
| **6** | **Delegate PII to `nemo-relay-pii-redaction`** | crate (drags core — weigh) | **M** | Judge-brief redaction (PLAN §10 risk 5) without writing detectors. |
| **7** | **Import ACG stability as an M6 trigger signal** | port the stability analysis (the redis version conflict this row cited is resolved as of 2026-08-19 — roundhouse runs 1.2.4 — but the crate still drags `nemo-relay` core, so the port-not-crate call stands) | **M** | A no-model-call trigger orthogonal to the four planned — strengthens the conjunction. |

### D.2 Relay → Roundhouse (the reverse direction)

| # | Seam | Mechanism | Cost | Relay gains |
|---|---|---|---|---|
| **R1** | **Roundhouse as the upstream behind Relay's gateway** | `nemo-relay --openai-base-url https://roundhouse.internal/v1 codex` | **S** — one flag | Cache-adjusted local/frontier routing, budgets, degrade-to-local; Relay stays "does not schedule the workers" and roundhouse becomes what does. |
| **R2** | **Token-exact routing and a real cost signal** | fill `prompt_token_estimate` (hardcoded `None` today) or replace the Decision API | **M** | Switchyard's routers currently classify on model name, tool count, and a boolean. |
| **R3** | **The capability gate for `baseline_model`** | port `quality_prior`/`capability_band` validation into `LlmOptimizationSummary` | **S** | Closes the largest correctness hole in Relay's savings accounting. |
| **R4** | **`enforce_usage_reporting`** | add `stream_options.include_usage` when absent, never override | **S** | Relay's gateway silently records zero-token, zero-dollar calls on streaming upstreams not asked for usage. Roundhouse's single most valuable correctness contribution. |
| **R5** | **A durable trajectory source** | roundhouse's log replays to identical ATIF from cold storage | **M** | Relay's ATIF producer is volatile (in-memory, lost on crash). |
| **R6** | **The steering primitive** | synthetic `function_call` + `fetch_steer` | **L** | The *control* half of "visibility into and control over agent runs" — Relay can block a tool but cannot inject one. |
| **R7** | **Multi-provider ACG evidence** | `CacheLedger`'s realized hit ratios as `expected_reads` | **M** | ACG plans from assumed reuse horizons; roundhouse measures realized reads per target. |

---

## E) Risks — where running both creates two sources of truth

**E1 — Two proxies in one path (highest risk).** Chained `Codex → Relay gateway → roundhouse → {Dynamo | frontier}`: double SSE re-encode per delta; Relay *strips* Codex's ChatGPT JWT and substitutes `OPENAI_API_KEY` whenever one is present (`alignment.rs:97-110`) — silently changing who pays under roundhouse's `PassThrough`; Relay may redirect to `chatgpt.com/backend-api/codex` *instead of* the configured base URL (`gateway_upstream_url_override`, `routes.rs:189-206`) — routing *around* roundhouse; Relay reserializes decoded JSON (prefix-admission survival untested); and `requires_openai_auth=true` (Relay) vs roundhouse's leave-unset ruling — one is wrong for a given Codex version.

**E2 — Two policy engines.** Guardrail-reject (403/exit 2) vs `TurnPolicy` refusals: two logs, two refusal vocabularies, no shared correlation id. Relay's priorities are an ordering; roundhouse's `narrow` is a lattice — layering them yields no guarantee.

**E3 — Two event logs.** `.nemo-relay/atof/events.jsonl` (timestamp-ordered, lossy on crash) vs roundhouse's Redis log (`seq`-ordered, durable). They *will* disagree on retried turns (roundhouse dedups; Relay sees two requests), steered turns (Relay's ATIF reads a synthetic call as a normal tool call with no LLM behind it), and unreported usage.

**E4 — Two caches.** Relay's `response_cache` vs roundhouse's `turn_id` dedup. Relay's bypasses stateful requests, so it self-disables in front of roundhouse — but incidentally, not by design.

**E5 — Two routers.** Switchyard's `backend_id` decision and roundhouse's cache-adjusted choice both stamping `model_routing` contributions with baselines: the dashboards will not agree.

**E6 — Version/dependency frictions.** toolchain 1.96.1 ✅ identical; edition 2024 ✅; `uuid =1.18.1` ✅ identical *[2026-08-21: **no longer identical, and the asymmetry is the whole point.** Roundhouse declares a caret `uuid = { version = "1.18.1", … }` (`Cargo.toml:95`, moved from `:80`) currently resolved to **1.24.0** (`Cargo.lock:5758-5759`); Relay declares an exact `uuid = "=1.18.1"` (`HEAD:Cargo.toml:41`, same in both published tarballs of `nemo-relay-types`). An exact requirement wins over a caret for the whole graph, so adopting the types crate silently downgrades us six releases and caps uuid at 1.18.1 until Relay moves. It resolves today — `moka 0.12.16` wants `1.1`, `rmcp 3.1.3` and `ts-rs 11.1.0` want `1`, `rama-http 0.3.0-alpha.4` wants `1.18`, codex `6344a65` wants `1`, Dynamo `ac7b751` wants `^1.18.1` — but the ceiling needs the same manifest note the redis pin got, and this row should no longer be read as a green tick]*; axum 0.8 ✅; reqwest root stores differ ⚠️ (native-roots vs webpki); `sha2` 0.11 vs 0.10 ⚠️ duplicate; **`redis` 1.1 vs 0.27 ❌ hard conflict** (blocks `nemo-relay-adaptive/redis-backend`) *[resolved after this snapshot: roundhouse moved to redis 1.2.4 on 2026-08-19 (commit `8578e8c`), which unifies with Relay's `^1.1`; the ceiling below latest 1.x is Dynamo's exact `tokio = "=1.48.0"` pin, recorded in the workspace manifest — see the ruling's addendum]*; OpenSSL banned on both sides ✅; `nemo-relay` core is heavy (OTel ×3, tonic, object_store, libloading) — **`nemo-relay-types` is the only cheap import**.

**E7 — Roadmap collision, from Relay's own docs.** Routing is moving *out* of Relay toward Switchyard ("replaced by a Switchyard-owned native plugin"); cache/scheduling optimization is moving *into* `nemo-relay-adaptive` ("performance-aware scheduling, hints, and cache behavior"). Roundhouse's routing collides less with Relay over time and more with Switchyard; its cache economics collide more with adaptive. `prompt_token_estimate: None` is a placeholder someone intends to fill.

**E8 — Maturity asymmetry.** Relay: 0.8.0, ten published crates, PyPI/npm/binaries, versioned docs with migration guides, `missing_docs = "deny"`, ~156k lines in core. Roundhouse: 0.1.0 exploratory skeleton. A coupling inherits Relay's real breaking-change cadence (0.8 broke the native ABI, the tool-result contract, config discovery).

---

## F) Verdict candidates (not ranked — the synthesis decides)

**F1 — Format compatibility only**: emit ATOF/ATIF/`LlmOptimizationSummary`, depend on `nemo-relay-types` and nothing else. Cheapest real integration; keeps every roundhouse invariant; roundhouse's ATIF is *better* (cold-replayable); zero runtime coupling. But leaves the interception duplication fully in place, and routing/budget facts land in `extra`, invisible to generic consumers.

**F2 — Relay CLI fronts roundhouse**: Relay owns interception, roundhouse owns the turn. Deletes roundhouse's launch surface and M9's real-CLI burden; Relay gets tool-level visibility roundhouse can never see from the wire. But E1 in full: double SSE re-encode, JWT-stripping/upstream-override can route around roundhouse or change who pays; two event logs; roundhouse loses the raw client bytes where `prompt_cache_key`/`session-id` correlation lives.

**F3 — Roundhouse becomes a Relay plugin component**: structurally wrong. Relay's runtime is process-global, in-memory, request-scoped; roundhouse's invariants (lease, durable log, prefix admission, crash replay) have no home in a middleware callback; Relay explicitly bypasses stateful requests; and the precedent (the one routing plugin) is deprecated in the release that would host this one.

**F4 — Consume Switchyard, drop `EscalationPolicy`**: weakest on the evidence. A deprecated HTTP client, the router in another repo on a topic branch, a wire contract that accepts none of roundhouse's signals, and a networked hop against a design whose stated reason for embedding was removing exactly that hop. Re-affirm the native ruling; fix the stale citation.

**F5 — Full independence with contribution flow**: copy the pricing *schema*, emit ATOF/ATIF/optimization via `nemo-relay-types`, adopt `baseline_route`/`reason_code`/`observe_only` decision ideas; contribute back `enforce_usage_reporting`, the capability gate, and realized cache evidence; document `nemo-relay codex` as the supported wrapper (F2 as a deployment *option*, not a dependency), resolving `requires_openai_auth` first. Retires duplication where it is pure cost, keeps roundhouse where it is stronger. Requires upstream engagement; schema copies can drift (versioned, so detectably).

---

### Three facts the synthesis should weight heavily

1. **The pass-through proxy roundhouse just ruled is already shipping inside NVIDIA**, down to the same upstream constant and the same second-header trick — with one flat contradiction (`requires_openai_auth`) that should be resolved before M7 rather than after. (`crates/cli/src/agents/codex/launch.rs:199-206`, `agents/codex/alignment.rs:23,85-126`)
2. **`crates/switchyard` is not the escalation router and is deprecated in the version in hand.** Roundhouse's native `EscalationPolicy` is the right call, and the citation in `routing/policy.rs` currently points a future reader at the wrong repository. (`crates/switchyard/README.md`, `Cargo.toml:22`) *[2026-08-21: **"deprecated" has completed to "deleted."** `88d1b1b` (2026-08-19, #811) removed the crate, the CLI feature and the component from Relay entirely; both files cited here are gone at `1a54812`, and a legacy `kind = "switchyard"` component is now a hard config error. This fact was right and is now stronger: `routing/policy.rs` should cite `NVIDIA-NeMo/Switchyard` and nothing in Relay. Note also that the Relay↔Switchyard integration is presently shipping on **neither** side — Relay rejects the old form and Switchyard's replacement plugin is an unmerged branch under a 0.3.0 it has not cut (see the two 2026-08-21 sections of `relay-switchyard-dedup-deep-dive.md`).]*
3. **The savings vocabulary already exists as a published NVIDIA type — and it is missing exactly the safeguard roundhouse built its dashboard around.** `LlmOptimizationSummary` has `baseline_model`/`effective_model`/`estimated_cost_saved`/`status: Partial`/`Observed|Estimated`, but no capability gate. That is a two-way trade with unusually clean edges. (`crates/types/src/codec/optimization.rs:143-293`)

---

## Re-read 2026-08-21 — what moved since `c37b551`

This document is a snapshot at `c37b551` (workspace 0.8.0). Re-read against
`NVIDIA/NeMo-Relay` @ **`1a54812`** (2026-08-21 17:22 −0400, "Merge pull request
#855 from NVIDIA/release/0.8", workspace **0.9.0**) — 48 commits on. The
detailed evidence lives in the Relay half of
`relay-switchyard-dedup-deep-dive.md`'s `Re-read 2026-08-21` section; this is
the short list for a reader of *this* document.

**Note the org.** The repository is `github.com/NVIDIA/NeMo-Relay`. This
document's header cites a container path, and it is worth writing the URL down
because `NVIDIA-NeMo/NeMo-Relay` 404s while Switchyard genuinely is under
`NVIDIA-NeMo/`.

**Three claims moved**, each bracketed in place above: overlap row 5 and row 10
(their cited files went with `crates/switchyard`, deleted in `88d1b1b`), row 20
(`MetricEnvelope` turns out to be 0.8-only and therefore decides the S2 pin),
E6's `uuid` tick (roundhouse's is a caret resolved to 1.24.0 against Relay's
exact `=1.18.1`), D.1 rows 1–2 (the deps line, "~15 structs" → twelve, and what
"the spec is published" actually means), and "Three facts" #2 (`crates/switchyard`
deprecated → deleted).

**Everything else this document asserts about Relay's *interception* is
byte-identical at HEAD**, which is the headline for M7 and M9:
`git diff ca08901 HEAD -- crates/cli/src/agents/codex/ crates/cli/src/agents/claude/ crates/cli/src/provider_auth.rs`
is empty, and widening to `c37b551..HEAD` including `gateway/` yields one line
in `crates/cli/src/gateway/mod.rs`. §C.1's `launch.rs:199-206`,
`alignment.rs:23,85-126` and `provider_auth.rs:46-86` all resolve verbatim;
`requires_openai_auth=true` is still hardcoded (`launch.rs:201`); the Codex
version gate is still `(0, 143, 0)` (`crates/cli/src/agents/codex/mod.rs:22`).
ATOF is still `0.1` and ATIF still `ATIF-v1.7`, with `atif.rs` byte-identical
across the window — the "one genuinely stable thing" reading holds.

**Two additions this snapshot missed rather than that changed.** First, a
*second* Codex launch surface that predates both pins:
`crates/cli/src/agents/codex/host.rs` (added `2e4ebd2`, 2026-07-14, #395)
installs Relay persistently by rewriting `~/.codex/config.toml` with
`toml_edit` — provider named `"NeMo Relay"`, credential as a **static**
`http_headers = { "x-nemo-relay-client-token" = … }` rather than the argv
path's env-indirect `env_http_headers`, plus a challenge/token proof so
uninstall can distinguish installer-owned fields from user edits
(`host.rs:800-846`). §C.1 describes only the argv `--config` path, and the
persistent one is the closer analogue of roundhouse's own `codex_launch.rs`.
Second, `nemo-relay-types 0.8.0-rc.1` was published to crates.io on
2026-08-21, byte-identical to the tree at `ca08901` — so E8's "ten published
crates" now includes the 0.8 types, and F1/F5's `nemo-relay-types` dependency
has a version question with a decidable answer (row 20's note).
