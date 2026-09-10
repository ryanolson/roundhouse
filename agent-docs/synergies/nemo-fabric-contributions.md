<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# NeMo Fabric: the contributions roundhouse owes upstream (F4)

> **Status: drafts, ready to file.** `nemo-fabric.md` F4 lists seven gaps the
> deep dive verified as absent in NeMo Fabric @ `6d9ebc3`, to be opened once
> F1 landed. F1 landed 2026-09-10. Filing on `NVIDIA/NeMo-Fabric` is an
> outward-facing act and is the owner's to trigger; each entry below is the
> issue as it would be filed — title, the fact, the evidence at the pinned
> rev, the proposed change — so filing is a copy, not a rewrite. Where a
> contribution has a roundhouse-side proof (a measurement, a test), the entry
> names it so the upstream reader can reproduce it. Order is by how much a
> Fabric consumer pointed at roundhouse loses without it.

---

## 1. A recognized model slug turns on `tool_search` for custom Responses providers

**Fact.** At `openai-codex==0.144.4` (the pin at
`adapters/python/codex/pyproject.toml:35`), the app-server puts a
`{"type": "tool_search"}` tool definition on every `/v1/responses` request
when `models.default.model` is a slug the Codex catalog recognizes
(`gpt-5.4`), on a custom `model_providers.<name>` stanza exactly as on the
built-in `openai` one. With an unrecognized slug (`mock-model`) it does not.
Fabric's Codex adapter passes the slug verbatim for a custom provider
(`adapters/python/codex/src/nemo_fabric_adapters/codex/adapter.py:500-507`)
with no default and no warning.

**Why it matters.** A Responses-compatible endpoint that does not implement
`tool_search` — roundhouse refuses the resent `tool_search_call` item with a
422 — fails on the turn after the model first uses the tool, and nothing in
Fabric's planning or diagnostics says why.

**Evidence.** `agent-docs/research/nemo-fabric-deep-dive.md` §6: three runs
against a mock Responses server (the SSE shapes from
`tests/_utils/mock_api_server.py`), payloads recorded; the driver is
reproduced in that section.

**Proposed change.** In the Codex adapter's `model_schema` or in `doctor`,
warn when a custom provider is configured with a slug that resolves catalog
metadata; document in `docs/integrations/harness/codex.mdx` that a custom
Responses provider should name a slug outside the OpenAI catalog unless the
endpoint implements `tool_search`. Optionally a `harness.settings` knob that
pins `supports_search_tool: false` through a generated model catalog.

## 2. `AgentUsage` cannot carry cached-input or reasoning tokens

**Fact.** `AgentUsage { input_tokens, output_tokens, total_tokens, cost_usd,
extensions }` (`crates/fabric-core/src/agent_execution.rs:85-103`) and its
re-projection `RunUsage` (`crates/fabric-core/src/runtime.rs:176-192`) have no
field for cached input or reasoning output. Both bundled Responses adapters
drop everything but the three counts (`codex/adapter.py:1041-1048`,
`remote-agent/adapter.py:262-267`). Fabric's own Harbor integration wants
`n_cache_tokens` and backfills it from the Relay ATIF because `RunUsage`
cannot carry it (`sdk/.../integrations/harbor/fabric_agent.py:594-600`).

**Why it matters.** Cached input is the quantity a prefix-caching endpoint
saves money on; a consumer reading `RunUsage` sees the same number for a
warm and a cold turn.

**Proposed change.** Add `cached_input_tokens: Option<u64>` and
`reasoning_tokens: Option<u64>` to `AgentUsage`/`RunUsage` (both are subsets
of `input` and `output` respectively — the OpenAI Responses
`input_tokens_details.cached_tokens` and `output_tokens_details.reasoning_tokens`
shapes), lift them in the Codex and Remote Agent adapters, and let Harbor's
`n_cache_tokens` read `RunUsage` before falling back to ATIF.

## 3. A static bearer for an MCP server lands in the payload as a literal

**Fact.** `McpServerConfig.custom_headers` is the only way to send a static
bearer to an HTTP MCP server (`McpAuthenticationConfig` has only OAuth 2.0
and service-account variants, `crates/fabric-core/src/config.rs:1155-1210`,
and the Codex adapter rejects most of their fields, `codex/adapter.py:206-240`).
`expand_http_headers` substitutes the environment variable's *value* into
the config sent over JSON-RPC and blanks the variable in the child
environment (`adapters/python/common/src/nemo_fabric_adapters/common/utils.py:96-102`;
asserted at `tests/adapters/test_codex_adapter.py:484` and `:508`).

**Why it matters.** The secret is now in every place the thread config is
logged or persisted. Codex itself has an indirection for exactly this —
`mcp_servers.<key>.bearer_token_env_var` — which Fabric reaches only through
the untyped `config_overrides` escape hatch.

**Proposed change.** A `McpAuthenticationConfig::BearerEnv { env }` variant
(or a `bearer_token_env` field on `McpServerConfig`) that the Codex adapter
maps to `bearer_token_env_var`, and that other adapters map to their native
equivalent or reject at planning.

## 4. No annotation vocabulary and no per-server approval mode, so an unannotated MCP tool cancels silently

**Fact.** Fabric has no `readOnlyHint` / `destructiveHint` / `openWorldHint`
vocabulary (0 matches tree-wide) and no `mcp_servers.<key>.default_tools_approval_mode`
(0 matches); its only approval knob is the thread-level
`approval_mode ∈ {auto_review, deny_all}` (`codex/adapter.py:67-71`, `:566-576`).
Codex 0.146.0 treats a tool it sees no annotations on as needing approval,
and under `approval_policy = never` an approval nobody can be asked for
resolves to *cancelled*: the agent receives a cancellation notice where the
tool output should be (`crates/roundhouse-server/src/codex_launch.rs`, the
`TOOLS_APPROVAL_MODE` ruling; proved against the real binary in M9).

**Proposed change.** Document the trap in `codex.mdx`; add
`default_tools_approval_mode` as a normalized per-server field the Codex
adapter maps, and have `doctor` warn when an HTTP MCP server is configured
and the thread's approval mode would cancel unannotated tools. Whether the
0.144.4 app-server reproduces the same branch is unverified — the issue
should say so and ask.

## 5. `base_url` for a Responses-compatible provider is not checked for its API prefix

**Fact.** `custom_model_provider_config` does `base_url.rstrip("/")` and
nothing else (`codex/adapter.py:539`). Codex posts to `{base_url}/responses`,
so a `base_url` without `/v1` plans, starts, 404s every turn, and — if the
MCP server was configured separately — still completes the MCP handshake and
looks healthy. Roundhouse refuses this shape at generation time for that
reason (`codex_launch.rs`, `BaseUrlMissingApiPrefix`).

**Proposed change.** A planning-time check (or `doctor` warning) that a
custom Responses provider's `base_url` ends in the API prefix the wire
expects, with the failure mode in the message.

## 6. The Remote Agent adapter never asks for usage on a streaming request

**Fact.** Its Responses payload is `{"model", "input", "stream": true}` plus
optional `instructions`/`temperature`
(`adapters/python/remote-agent/src/nemo_fabric_adapters/remote_agent/adapter.py:246-256`)
with no `stream_options.include_usage`. A strict OpenAI-compatible streaming
upstream returns no usage object unless asked, so the adapter records
zero-token, zero-cost turns — and zero cost on a hosted model is
indistinguishable from a saving.

**Proposed change.** Add `stream_options: {"include_usage": true}` when
streaming, only ever adding and never overriding a caller-set value — the
rule roundhouse's `enforce_usage_reporting` encodes
(`crates/roundhouse-fleet/src/usage.rs`).

## 7. `cost_usd` has no comparability gate

**Fact.** Fabric's only money field is `cost_usd: Option<f64>`, "Invocation
cost in US dollars when reported by the provider"
(`crates/fabric-core/src/agent_execution.rs:98-101`), passed through at
`runtime.rs:1637`. Nothing says what model it is comparable to. Relay's
`LlmOptimizationSummary` has the same gap and roundhouse's S4 contribution to
Relay is the same proposal.

**Proposed change.** Where a consumer compares costs across runs on different
models, carry a declared `quality_prior` (0.0..=1.0, sourced and dated) on
the model config and refuse a comparison outside a `capability_band` — the
gate `crates/roundhouse-core/src/metrics/pricing.rs` implements and
`CLAUDE.md` explains. A small addition to `ModelConfig.settings` conventions
and to the Harbor integration's cost columns; not a core type change.

---

## Two asks that are not fixes

- **`model_catalog_json` as a normalized field**, or a way to pin the Codex
  model catalog without `config_overrides`: it is the only carrier of
  `supports_search_tool` (item 1) and `include_skills_usage_instructions`
  (the flag the Fabric skills path loses, `nemo-fabric.md` F5).
- **Re-export `McpServerConfig` from the crate root.** It is `pub` in
  `config` and every sibling type is re-exported; a consumer building a
  `FabricConfig` in Rust reaches for it first
  (`crates/roundhouse-server/src/codex_launch/fabric.rs` carries the
  workaround import).
