<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

> **Status: evidence.** Produced 2026-08-27 as Dive A of the Anthropic Messages
> API research round. Surveys the Rust crates that carry the Anthropic Messages
> wire protocol, in both directions roundhouse needs (SERVE `/v1/messages` to an
> unmodified Claude Code client; DISPATCH to `api.anthropic.com`), and surveys
> what could serve as the conformance oracle for the serve surface. Facts only.
> Where a fact forces a decision the fork is named both ways and left open; the
> ruling is not this document's.
>
> **Independently fact-checked 2026-08-27**: 17 load-bearing claims re-derived
> from the named sources by a separate agent; 16 confirmed exactly, 1 corrected
> in transit — a claim-summary said "five of the twelve spec response content
> blocks" are missing from claudius where the count is **six** (the §5.2 list
> below was already right; the summary undercounted). One-crate drift on the
> live `q=claude` crates.io count (7 225 vs 7 226, same-day index churn) —
> immaterial. The ruling this evidence feeds is `../PLAN-anthropic-messages.md`.

# Dive A — Rust crates carrying the Anthropic Messages wire protocol

## 0. Provenance and reproducibility

Everything below is checkable against one of these pinned artefacts.

| Artefact | Source | Rev / checksum | Fetched |
|---|---|---|---|
| roundhouse working tree | `/home/user/roundhouse` | `306e6e0d8be7f643c0fa63910cfd876cff658d6b` | 2026-08-27 |
| `anthropic-sdk-typescript` | `github.com/anthropics/anthropic-sdk-typescript` | `7ba6a3fc3000f9bd1f6f9f45526cc66db3167e6b` | 2026-08-27 |
| `anthropic-sdk-python` | `github.com/anthropics/anthropic-sdk-python` | `181e2e5715c89a1451e384adb01d70a59d3ca10d` | 2026-08-27 |
| **Anthropic OpenAPI 3.1.0 spec** | `storage.googleapis.com/stainless-sdk-openapi-specs/anthropic/anthropic-446ddab751d9f0172400b17fc72736e9353d3b49780317f90c24aa98357fd39e.yml` | sha256 `942a1163…3d2ee87`, 2 448 030 B | 2026-08-27 |
| 15 candidate crates | crates.io `.crate` tarballs | version-pinned, listed in §4–§5 | 2026-08-27 |

Crate tarballs were downloaded as `https://static.crates.io/crates/<n>/<n>-<v>.crate`
and extracted; all crate `file:line` citations below are into those extracted
trees, whose directory names carry the exact published version.

**Environment limits that shaped method.** `docs.anthropic.com` is blocked by
this environment's egress proxy (`EGRESS_BLOCKED`); `platform.claude.com` is
reachable and is what §1 cites. `github.com` HTML returns proxy `403`, so repo
existence was established with `git ls-remote` rather than page fetches — see
§1 for why that distinction matters. The GitHub MCP API is repo-scoped here and
refuses org-level listing, so `orgs/anthropics/repos` was not available.

---

## 1. There is no official Anthropic Rust SDK — and Rust is not on the roadmap page

**Negative claim, method stated in full.** Two independent probes.

*Probe A — the official docs page.* `https://platform.claude.com/docs/en/api/client-sdks.md`
(fetched 2026-08-27) redirects to `…/cli-sdks-libraries/overview`, which names
the official surface verbatim: "**Client SDKs:** General-purpose Messages API
clients for **Python, TypeScript, C#, Go, Java, PHP, and Ruby**." Seven
languages, individually carded. The separate "Libraries and integrations"
section lists exactly two entries — Apple Foundation Models (Swift) and OpenAI
SDK compatibility. **Rust appears nowhere on the page**, in neither section.

*Probe B — repo existence by `git ls-remote`.* Against `github.com/anthropics/*`,
an existing public repo returns refs; a nonexistent-or-private one makes git
prompt for credentials and fail (`could not read Username`). Results:

| Repo | `git ls-remote … HEAD` | Verdict |
|---|---|---|
| `anthropic-sdk-typescript` | `7ba6a3fc…` | exists, public |
| `anthropic-sdk-python` | `181e2e57…` | exists, public |
| `anthropic-sdk-go` | `f6f79610…` | exists, public |
| `anthropic-sdk-java`, `-ruby`, `-php`, `-csharp` | refs returned | exist, public |
| **`anthropic-sdk-rust`** | credential prompt | **not public** |
| **`anthropic-rust`** | credential prompt | **not public** |
| `anthropic-openapi`, `anthropic-api-spec` | credential prompt | not public |

The seven repos that resolve are exactly the seven languages the docs page
names — which is what makes the method trustworthy rather than a bare 404: the
oracle's positives match the documented list one-for-one. **No Rust SDK repo
exists publicly under `anthropics`, and no crates.io crate is owned by
Anthropic** (§4 enumerates the namespace; every crate found is self-described
as unofficial or belongs to an unrelated vendor).

---

## 2. Anthropic's OpenAPI spec IS published and IS fetchable — this is the decision-changer

The brief flagged that a generated-types-from-spec path "would change the whole
decision." It is available.

`anthropic-sdk-typescript@7ba6a3fc` carries `.stats.yml`:

```
configured_endpoints: 200
openapi_spec_url: https://storage.googleapis.com/stainless-sdk-openapi-specs/anthropic/anthropic-446ddab751d9f0172400b17fc72736e9353d3b49780317f90c24aa98357fd39e.yml
openapi_spec_hash: f05934667b8dd6e84a29590f11f8737c
```
— `.stats.yml:1-3`. That URL returns **HTTP 200, unauthenticated**, 2 448 030
bytes of OpenAPI **3.1.0**, `info.title: "Anthropic API"`, **139 paths** and
**1 219 component schemas**.

It covers precisely what roundhouse needs to serve and dispatch:

- `/v1/messages` `[post]`, `/v1/messages/count_tokens` `[post]`,
  `/v1/models` `[get]`, `/v1/models/{model_id}` `[get]`, plus the batches
  family — and a `?beta=true` variant of each.
- Of the 139 paths, **123 are `?beta=true` duplicates** and 16 are non-beta.
  Stainless models the beta surface as parallel path entries, so a faithful
  generator emits roughly two of everything.

**Two cautions on this artefact, both load-bearing.**

1. **The URL is content-addressed, not stable.** *[2026-08-27, same-day
   correction from building the sync tooling: the 64-hex hash embedded in the
   filename (`446ddab7…`) is **not** the sha256 of the spec body — the body
   hashes to `942a1163…3d2ee87` (sha256) and `40dd485e…` (md5), and
   `.stats.yml`'s 32-hex `openapi_spec_hash` (`f0593466…`) matches neither.
   Both upstream hashes are opaque Stainless-internal content addresses. The
   snapshot-pin property survives — a moved spec is a changed URL — but body
   integrity is our own recorded sha256, checkable only against a re-download
   of the same URL.]* It is a *snapshot pin* — excellent for
   this repo's pin-vigilance discipline (`CLAUDE.md`, "Synergy dependencies are
   watched, not just pinned"), because a moved pin is a changed URL and cannot
   drift silently. But there is **no `latest` alias**: refreshing means reading
   `.stats.yml` at a newer SDK rev and re-fetching. The mechanism for noticing
   an upstream change is "diff the SDK's `.stats.yml`", not "re-GET a URL".
2. **It is Anthropic's spec but Stainless's rendering.** It carries
   `x-stainless-*` vendor extensions (`x-stainless-override-schema`,
   `x-stainless-param`, `x-stainless-skip`, `x-stainless-nominal`) that a
   generator must be taught to ignore or honour deliberately.

---

## 3. The spec's strictness is asymmetric — and that asymmetry is the whole trade

This is the single most consequential structural fact in the dive.

**Request side is closed.** `CreateMessageParams` (the `POST /v1/messages`
requestBody `$ref`) carries `additionalProperties: false`, `required:
[model, messages, max_tokens]`, and 18 properties: `cache_control, container,
inference_geo, max_tokens, messages, metadata, model, output_config,
service_tier, stop_sequences, stream, system, temperature, thinking,
tool_choice, tools, top_k, top_p`. `BetaCreateMessageParams` is likewise
`additionalProperties: false`, with 25 properties — beta-only delta:
`context_management, diagnostics, fallback_credit_token, fallbacks,
mcp_servers, output_format, speed`.

**Response side is open.** Across the whole spec there are **693** occurrences
of `"additionalProperties": false`. Partitioned by schema name: **149**
Request-side (`Request*` / `*Param`), **544** "other" (all input shapes on
inspection — `Base64ImageSource`, `BashTool_20250124`, `BetaBrowser*Config`,
…), and **0 response-side**. Spot-checked individually, every core response
type leaves it unset: `Message`, `Usage`, `MessageStreamEvent`,
`ResponseTextBlock`, `CacheCreation`, `MessageDeltaEvent`,
`ContentBlockDeltaEvent` — all `additionalProperties: <unset>`.

**The fork, both ways.**

- *Generate faithfully.* DISPATCH gets open response types for free — new
  Anthropic response fields never break the parse. But SERVE inherits
  `deny_unknown_fields` on `CreateMessageParams` and 148 sibling request
  schemas, and the day Claude Code sends a field newer than roundhouse's pinned
  spec snapshot, admission fails closed on a request roundhouse was supposed to
  pass through. That is precisely the pass-through-fatal condition.
- *Generate with `additionalProperties:false` suppressed.* SERVE becomes
  pass-through-safe. The cost is that roundhouse no longer rejects genuinely
  malformed client requests at the type boundary and must decide separately
  where (or whether) request validation lives — and it silently diverges from
  what the upstream API itself would have rejected, so a request roundhouse
  accepts may still 400 at `api.anthropic.com`.

Note this is a *generation-time switch*, not an inherent property of the spec.
No hand-written crate surveyed offers the choice at all.

**System-as-string-or-blocks is in the spec**: `system` is
`anyOf: [{type: string}, {items: $ref RequestTextBlock, type: array}]` — both
forms, as the brief required.

### 3.1 The spec does NOT describe the SSE transport events

**Negative, method stated.** The literal string `"ping"` occurs **zero** times
in the entire 2.4 MB spec (`json.dumps(spec).count('"ping"') == 0`). Schema-key
search for `Ping` or `*ErrorEvent` returns only
`BetaManagedAgentsSessionErrorEvent` — nothing for the Messages stream.
`MessageStreamEvent` is a union of exactly **six** members:
`MessageStartEvent, MessageDeltaEvent, MessageStopEvent, ContentBlockStartEvent,
ContentBlockDeltaEvent, ContentBlockStopEvent`.

But `ping` and `error` are real on the wire — the house `claude-api` skill's own
event list names them, and `claudius` handles both at its SSE layer
(`src/sse.rs:235` `"ping" => Ok(MessageStreamEvent::Ping)`, `src/sse.rs:254`
`"error" => Err(parse_stream_error(…))`). `overloaded_error` appears 11 times in
the spec, but as an *error-body* schema, not a stream event.

**Consequence for the generated path:** a serve implementation generated purely
from the spec would know six event types and would not emit `ping` keepalives
nor frame a mid-stream `error` event. Those two must be hand-written whatever
else is generated. This is a genuine gap in the spec-as-oracle story and the
one place a "just generate it" plan silently under-delivers.

### 3.2 Vocabulary the spec fixes (the grading rubric for §5)

- `StopReason` enum — exactly seven: `end_turn, max_tokens, stop_sequence,
  tool_use, pause_turn, refusal, model_context_window_exceeded`. (`pause_turn`
  and `model_context_window_exceeded` both exist; the brief's "if it exists"
  is resolved yes.)
- `Usage` — nine properties: `cache_creation, cache_creation_input_tokens,
  cache_read_input_tokens, inference_geo, input_tokens, output_tokens,
  output_tokens_details, server_tool_use, service_tier`.
- `CacheCreation` — `{ephemeral_1h_input_tokens, ephemeral_5m_input_tokens}`,
  both `required`. **This is the exact wire spelling of the 5m/1h breakdown.**
- `Message` — `container, content, id, model, role, stop_details, stop_reason,
  stop_sequence, type, usage`.
- Response `ContentBlock` union — **twelve** members: `ResponseTextBlock,
  ResponseThinkingBlock, ResponseRedactedThinkingBlock, ResponseToolUseBlock,
  ResponseServerToolUseBlock, ResponseWebSearchToolResultBlock,
  ResponseWebFetchToolResultBlock, ResponseCodeExecutionToolResultBlock,
  ResponseBashCodeExecutionToolResultBlock,
  ResponseTextEditorCodeExecutionToolResultBlock,
  ResponseToolSearchToolResultBlock, ResponseContainerUploadBlock`.
- Delta variants: `TextContentBlockDelta, InputJsonContentBlockDelta,
  ThinkingContentBlockDelta, SignatureContentBlockDelta` (+ `MessageDelta`).
- **`AnthropicBeta` is an OPEN enum**: `anyOf: [{type: string}, {type: string,
  enum: [41 values], x-stainless-nominal: false}]` — a bare string is valid
  alongside the named vocabulary. Named values include `compact-2026-01-12`,
  `task-budgets-2026-03-13`, `context-management-2025-06-27`,
  `mid-conversation-tool-changes-2026-07-01`, `thinking-display-updates-2026-08-18`,
  `server-side-fallback-2026-07-01`, `fast-mode-2026-02-01`,
  `extended-cache-ttl-2025-04-11`, `structured-outputs-2025-11-13`,
  `skills-2025-10-02`, `managed-agents-2026-04-01`.
  Openness is expressed in the spec itself — a generator that honours it gets
  the forward-compatible `anthropic-beta` handling roundhouse wants for free.

*[2026-09-01, `anthropic-spec-sync` re-run per CLAUDE.md's synergy-vigilance
cadence, ruling R-E: the pinned spec moved.* `anthropic-sdk-typescript` HEAD
advanced `7ba6a3fc…` → `4140e0ea…` (`4140e0eaa597c0ad35218ffb20b66ef7fce7f639`);
its `.stats.yml` now names
`anthropic-e50cf35b74cc0471a2b5af7ea03765aa81c035f82588e9a1ba1b29aeaa17d064.yml`,
body sha256 `d1d189d7…9c4f55` (was `942a1163…3d2ee87`), `openapi_spec_hash`
`e4ae88bd…9b8d` (was `f0593466…737c`) — three opaque identifiers, all moved
together, none conflated, per the caution recorded above.

Vocabulary diff (`spec_sync.py --diff-only`, structural comparison of both
bodies): everything §3.2 pins by name — `StopReason`'s seven values, `Usage`'s
nine properties, `CacheCreation`'s two fields, `Message`'s ten top-level
properties, the twelve response `ContentBlock` members, the four delta
variants, `CreateMessageParams`/`BetaCreateMessageParams`'s property sets and
`additionalProperties: false`, `CacheControlEphemeral`'s `{1h, 5m}` — is
**byte-identical to 2026-08-27**. The only movement is additive and inside the
one deliberately-open vocabulary: three new named `AnthropicBeta` values
(`mid-conversation-output-config-2026-07-01`,
`mid-conversation-system-clear-at-2026-08-21`,
`thinking-binding-controls-2026-08-01` — the enum's `anyOf [string, enum]`
open shape itself did not close), plus `path_count` 139→140 and
`beta_path_count` 123→124 (one new endpoint, mirrored beta-true per the
existing convention). `roundhouse-fleet`'s 56 pinning tests
(`timeout 300 cargo test -p roundhouse-fleet anthropic_messages`) stayed
green against the new pin with no source change beyond
`spec_pin.json` itself — three new open-enum values need no typed arm (per
this skill's step-5 classification: "new beta value… nothing *parses*
wrong… decide per field" — none of the three names a mid-conversation
control roundhouse currently drives, so passthrough is correct as-is, not a
gap). `spec_pin.json`'s `fetched` moved 2026-08-27 → 2026-09-01.]*

---

## 4. Enumeration and the discard tier

crates.io API queried 2026-08-27 with a `User-Agent` header:
`q=anthropic` → **3 257** crates; `q=claude` → **7 225**. Both listings were
read to 60 entries by relevance, plus a `sort=recent-downloads` pass.

**Discarded, one line each** (crates.io metadata, 2026-08-27; source not read
except where noted):

| Crate | Max ver | Last release | Why discarded |
|---|---|---|---|
| `anthropic` (abdelhamidbakhta) | 0.0.8 | 2024-09-03 | Dead ~2 years; pre-dates thinking, tools-as-shipped, caching. |
| `anthropic-sdk` (mixpeal) | 0.1.5 | 2024-07-23 | Dead; 77k downloads are legacy. |
| `anthropic-rs` (roushou/mesh) | 0.1.7 | 2024-09-07 | Dead. |
| `clust` | 0.9.0 | 2024-06-30 | Dead; MSRV 1.76. |
| `misanthropy` | 0.0.8 | 2025-06-08 | Stale >14 months; 0.0.x. |
| `anthropic-sdk-rust` (dimichgh) | 0.1.1 | 2025-06-11 | Stale; edition 2021, rust-version 1.70. Source read: no `pause_turn`/`refusal`/`output_config`/`effort` (all 0 hits). |
| `async-anthropic` | 0.6.0 | 2025-05-03 | Stale; superseded by its own live fork `async-llm`. Source read: essentially no 2026 vocabulary (`thinking_delta` 0, `redacted_thinking` 0). |
| `anthropic-ai-sdk` | 0.2.27 | 2026-01-11 | Source read; near-zero 2026 coverage — `cache_creation_input_tokens` 0, `output_config` 0, `effort` 0, `pause_turn` 0, `service_tier` 0, `stop_details` 0. Thin client. |
| `langchain-rust` | 4.6.0 | 2024-10-06 | Dead ~2 years. |
| `llm` (graniet) | 1.3.8 | 2026-04-19 | Multi-backend aggregator; Anthropic is one of many, no wire-level fidelity goal. |
| `rig-core` | 0.42.0 | 2026-08-17 | Live and popular (1.4M recent dl) but an agent-framework abstraction, not a wire codec. |
| `genai` | 0.7.0-beta.19 | 2026-08-18 | Multi-provider normalizer; lowest-common-denominator by design. |
| `anthropic-auth` / `link-assistant-router` / `swapdex` / `clauth` | — | 2026 | OAuth/seat-management only; no Messages wire types. Relevant to pass-through auth (Dive B territory), not to this dive. |

Also noted, not deep-read: `llm-bridge-core` 0.5.0 (Anthropic↔OpenAI protocol
transform, Apache-2.0), `oven-sdk-anthropic` 0.6.0, `everruns-anthropic` 0.18.2,
`klieo-llm-anthropic` 3.16.0, `agentkit-provider-anthropic` 0.10.7,
`async-llm` 0.10.0, `trimwire` 0.6.0 and `claude-proxy` 1.2.0 (both Claude Code
gateway proxies). `dynamo-protocols` 5.4.0 was checked because roundhouse
matches Dynamo: it is OpenAI-shaped only and carries no Anthropic types.

---

## 5. Deep read of the serious candidates

Coverage was measured by grepping the *exact* wire spellings §3.2 takes from the
spec, so a miss is a real miss and not a naming artefact.

### 5.1 `siumai-protocol-anthropic` 0.11.0-beta.10 — best 2026 coverage, wrong direction

The only crate that is *architecturally* what roundhouse wants: a standalone
wire-codec crate. `src/lib.rs:1-11` — "Anthropic Messages protocol mapping…
owns the vendor-agnostic protocol layer"; `src/messages/mod.rs:2-6` — "It
intentionally owns no credentials, endpoints, HTTP execution, retries, model
catalog, or provider construction." 12 237 lines across 14 files. MIT OR
Apache-2.0, edition 2024, rust-version 1.95.

**Weight — by far the lightest.** Dependencies are `base64, secrecy, serde,
serde_json, siumai-core, thiserror, url`. **No reqwest, no tokio, no TLS stack
at all.** Nothing to reconcile against the workspace's `tokio = "=1.48.0"` or
`uuid = "=1.18.1"` pins, and no OpenSSL exposure. `default = []`.

**Coverage — the widest surveyed, and alone in several places.** It is the only
crate carrying `task_budget` (35 hits), `allowed_callers` (20), `mcp_toolset`
(21), `context_management` (39), `stop_details` (34), `defer_loading` (26),
`container` (56), `inference_geo` (21), `service_tier` (29). `CacheTtl` is a
real 5m/1h enum — `src/messages/annotations.rs:11-16`. Count-tokens is a first
class target — `src/messages/mod.rs:69` `MESSAGES_COUNT_TOKENS_TARGET`.

**Fatal for roundhouse: it is one-directional, client-side only.** The public
surface is `encode_request*` (11 overloads, `src/messages/request.rs:102-229`),
`decode_response` (`src/messages/response.rs:16-24`) and
`MessagesStreamDecoder` (`src/messages/stream.rs:26`).
`rg "fn (encode_response|decode_request)" src/` returns **nothing**. There is no
way to parse an inbound Messages request or emit a Messages response — exactly
the two operations SERVE is made of.

**Second problem: it is not a types library.** `encode_request` consumes and
`decode_response` produces *siumai's* neutral `LanguageRequest`/`LanguageResponse`,
not Anthropic-shaped structs, so adopting it means adopting `siumai-core`'s
vocabulary as roundhouse's internal representation.

**`deny_unknown_fields` — 25 sites, but read them before judging.** They sit on
*internal* `*Wire` helpers and `#[cfg(test)]` projections, not on the top-level
response: `ContextManagementWire` (`options.rs:698`), `ContextThresholdWire`
(`options.rs:736`), `McpServerWire` (`options.rs:1063`), `TokenTaskBudget`
(`options.rs:234`), `ContainerSkill` (`options.rs:296`), `CacheControl`
(`annotations.rs:29`), plus test structs at `request_contract_tests.rs:33,43`.
Meanwhile the response path is deliberately open: `MessageResponseWire`
(`wire.rs:8-23`) and `UsageWire` (`wire.rs:25-37`) each carry
`#[serde(flatten)] extra: BTreeMap<String, Value>`, and `UsageWire::merge`
extends `extra` across `message_start`/`message_delta`. So unknown *response*
fields survive; unknown fields inside those five *request* sub-objects do not.
Both types derive `Deserialize` only — no `Serialize`.

### 5.2 `claudius` 0.33.0 — bidirectional derives, 2025-era coverage, heavy

Apache-2.0, edition 2024, 31 584 lines. `github.com/rescrv/claudius`.

**Derive direction is right.** `MessageCreateParams`
(`src/types/message_create_params.rs:17-18`), `MessageParam`
(`src/types/message_param.rs:17-18`) and `MessageStreamEvent`
(`src/types/message_stream_event.rs:14-16`) all derive **both** `Serialize` and
`Deserialize`. This is the property a client-only SDK usually lacks and the one
SERVE requires.

**`StopReason` is complete** — `src/types/stop_reason.rs:6-29` carries all seven
spec values including `PauseTurn`, `Refusal`, `ModelContextWindowExceeded`.
Closed, though: `#[serde(rename_all = "snake_case")]` with no `#[serde(other)]`,
so an eighth value fails the parse.

**Where it falls short of the 2026 surface:**

- **`cache_control` has no TTL.** `src/types/cache_control_ephemeral.rs:7-12` is
  `struct CacheControlEphemeral { r#type: String }` — one field. The 5m/1h TTL
  variants are *unrepresentable*.
- **`Usage` has six fields** (`src/types/usage.rs:12-34`):
  `cache_creation_input_tokens, cache_read_input_tokens, input_tokens,
  output_tokens, output_tokens_details, server_tool_use`. Missing against the
  spec's nine: **`cache_creation` (the 5m/1h breakdown), `service_tier`,
  `inference_geo`**. Grep confirms `service_tier` 0 hits and `inference_geo` 0
  hits crate-wide.
- **`ContentBlock` is closed and short.** `src/types/content_block.rs:15-51` has
  nine variants; the hand-written `Deserialize` at `:53-93` ends
  `other => Err(serde::de::Error::unknown_variant(other, &[…]))`. Against the
  spec's twelve response blocks it lacks `web_fetch_tool_result`,
  `code_execution_tool_result`, `bash_code_execution_tool_result`,
  `text_editor_code_execution_tool_result`, `tool_search_tool_result`,
  `container_upload`. Crate-wide: `web_fetch` 0, `code_execution` 0,
  `tool_search` 0. **Real 2026 traffic hits that error arm.**
- **No `error` stream event.** `MessageStreamEvent`
  (`src/types/message_stream_event.rs:16-60`) has seven variants — `Ping,
  MessageStart, MessageDelta, ContentBlockStart, ContentBlockDelta,
  ContentBlockStop, MessageStop` — and no `Error`. At the SSE layer,
  `src/sse.rs:254` turns `"error"` into `Err(parse_stream_error(…))` and
  `src/sse.rs:256` turns any unrecognised event into
  `Err("Unknown SSE event type: …")`. So a mid-stream `overloaded_error` is
  representable only as a terminal Rust error, never as a value roundhouse could
  log, re-emit, or route on — and for SERVE there is no variant to *emit* at all.
- Absent crate-wide: `task_budget` 0, `context_management` 0, `mcp_toolset` 0,
  `container` 0, `stop_details` 0, `allowed_callers` 0, `eager_input_streaming` 0.

**Weight is the disqualifier for the shipped graph.** Cargo.toml `features`:
`default = ["binaries", "native-tls"]`, `native-tls = ["reqwest/native-tls"]`,
`rustls-tls = ["reqwest/rustls"]`. **Default features pull OpenSSL**, which
Dynamo's `deny.toml` bans and roundhouse matches. `default-features = false,
features = ["rustls-tls"]` avoids it, but `default` also carries `binaries`,
which drags `rustyline`, `ctrlc`, `getopts`, `libc`, and the author's own
`arrrg`/`arrrg_derive`/`biometrics`/`utf8path` ecosystem crates. `tokio` is
requested with `features = ["full"]`. **There is no types-only feature** — the
feature list is exactly `binaries`/`native-tls`/`rustls-tls`, so the type
definitions cannot be imported without the client, the agent (`src/agent.rs`,
5 737 lines), the PTY driver (`src/pty.rs`, 2 571 lines) and the chat REPL.
`reqwest` is pinned `0.13.4`.

### 5.3 `adk-anthropic` 2.1.0 — best-balanced bidirectional candidate

Apache-2.0, edition 2024, rust-version 1.95, 21 922 lines,
`github.com/zavora-ai/adk-rust`, released 2026-08-25 (freshest serious
candidate).

**Weight is clean.** `reqwest 0.12` with `default-features = false, features =
["json","stream","rustls-tls-native-roots","multipart"]` — **rustls, no
OpenSSL**. `tokio 1.40` with `default-features = false, features =
["time","macros"]` — compatible with the workspace `=1.48.0` pin. No `uuid`
dependency, so no conflict with `=1.18.1`. Features `default = []`, plus
optional `files` and `managed-agents`.

**Coverage is the broadest of the bidirectional crates.** `ContentBlock`
(`src/types/content_block.rs:13-62`) derives `Serialize, Deserialize` with
`#[serde(tag = "type")]` and twelve variants — including
`WebFetchToolResult`, `CodeExecutionResult`, `ProgrammaticToolUse`, which
claudius lacks. Crate-wide it carries `web_fetch` 42, `code_execution` 8,
`mcp_toolset` 7, `context_management` 11, `stop_details` 3, `inference_geo` 5,
`pause_turn` 7. It is also the **only** crate with `#[serde(other)]` forward-
compatibility arms — `src/managed_agents/events.rs:150-151` (unknown session
event types) and `src/types/web_fetch_tool_result_error.rs:30-32` (unrecognised
error codes "from a future API version").

**But those arms are not on the paths that matter.** `ContentBlock` has no
catch-all variant and no custom `Deserialize`, so an unknown block type fails.
`StopReason` (`src/types/stop_reason.rs:6-30`) is likewise closed — it carries
all seven spec values plus a non-spec `PauseRun`, i.e. a superset in one place
and closed in general.

**One concrete conformance defect.** `src/types/usage.rs:33` declares
`cache_creation_input_tokens_1h: Option<i32>`. **That field name does not exist
in the Anthropic wire protocol.** The spec spells the breakdown as a nested
object `cache_creation: {ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}`.
So adk neither parses Anthropic's real `cache_creation` object nor emits the
right shape — an invented flattening. For roundhouse, whose savings dashboard is
judged on exactly these counters, that is a silent-wrong-number hazard.

**`deny_unknown_fields`: zero occurrences crate-wide** — the only serious
candidate with none at all.

### 5.4 The 5m/1h cache breakdown is carried by nobody — verified negative

Grepping the **exact spec field names** across all five serious candidates:

| Field | claudius | siumai | adk | shunt | async-llm |
|---|---|---|---|---|---|
| `ephemeral_5m_input_tokens` | 0 | 0 | 0 | 0 | 0 |
| `ephemeral_1h_input_tokens` | 0 | 0 | 0 | 0 | 0 |
| `inference_geo` | 0 | 21 | 5 | 0 | 0 |
| `service_tier` | 0 | 29 | 4 | 1 | 0 |
| `stop_details` | 0 | 34 | 3 | 0 | 0 |
| `container` | 0 | 56 | 8 | 2 | 0 |

The first two rows are the rigorous form of the brief's "cache_creation
ephemeral-5m/1h breakdown" question: **no surveyed Rust crate can represent it**,
under its real wire spelling, in either direction. Whatever roundhouse adopts,
this field is hand-work or generated.

---

## 6. `shunt-gateway` 0.32.0 — the serve-side existence proof, and it is untyped

Worth its own section because it is the only Rust project surveyed that actually
*serves* `/v1/messages` to real Claude Code clients. MIT OR Apache-2.0,
66 126 lines, `github.com/pleaseai/shunt`. It is both a lib (`Cargo.toml:47-49`,
`name = "shunt"`) and a bin (`:51-53`).

- `src/server.rs:182-183` — `.route("/v1/messages", post(proxy::post))` and
  `.route("/v1/messages/count_tokens", post(proxy::post))`, on axum 0.8.
- `src/protocol.rs:60,66` — both paths declared in its advertised endpoint list.

**And it does it with no typed Anthropic structs whatsoever.** Verified
negatives, method stated: `rg "struct (Anthropic|Messages)(Request|CreateParams|
MessageRequest)" src/` → no matches; `rg "enum (ContentBlock|StopReason|
StreamEvent|MessageStreamEvent)" src/` → no matches. There is no
`src/model/anthropic.rs` (the `src/model/` directory holds `gemini*` and
`responses*` only). Instead `src/adapters/anthropic/mod.rs` performs targeted
`serde_json::Value` surgery: reading `model` (`:701`), rewriting it (`:714`),
rewriting `metadata.user_id` (`:736-759`), and passing everything else through
byte-for-byte. The Anthropic path does not parse SSE events at all — the only
mention of `message_start` in that file is a comment at `:623`; every
event-name match in the crate lives in the *cursor* and *responses* (OpenAI)
adapters, which do translate.

**Read as evidence, not endorsement, this names the second architecture** the
ruling must weigh against typed structs: touch the handful of fields policy and
routing actually need, keep the rest as opaque JSON. It cannot break on a new
Anthropic field, and it buys nothing for a surface that must *originate*
responses rather than relay them — which is what roundhouse does when it serves
a locally-routed turn. The trade is field-level access and compile-time
guarantees versus unconditional forward compatibility.

---

## 7. Conformance oracle for a Messages serve surface

### 7.1 What roundhouse's codex oracle actually is, and why

Two tiers, both in `roundhouse-server`:

- **Library tier.** `crates/roundhouse-server/Cargo.toml:99-102` pins
  `codex-api`, `codex-client`, `codex-protocol`, `codex-utils-rustls-provider`
  to `openai/codex` rev `6344a655a5966f92e009a74928fb0559b41f9093`, as
  **dev-dependencies**. `tests/codex_conformance.rs:5-12` states the purpose:
  "A hand-written assertion on our SSE bytes only ever proves that we agree with
  our reading of the spec, and the failures this surface can have are exactly
  the ones a reading misses: an item whose type a client knows but whose shape
  it cannot parse is dropped in silence, so a turn arrives looking empty rather
  than looking wrong."
- **Binary tier.** `Cargo.toml:65-72` gates `tests/codex_e2e.rs` behind feature
  `e2e-codex`, off by default and deliberately excluded from the
  self-dev-dependency, so a machine without `codex` on PATH "gets a test binary
  with nothing in it rather than a failure to explain."
  `tests/codex_e2e.rs:3-14` — a real `codex exec` against a loopback roundhouse.

**A constraint this resolves in the oracle's favour.** `Cargo.toml:92-98`
records that `codex-http-client` pulls reqwest with default features
(native-tls → OpenSSL) and that this is acceptable *because* it is dev-only:
"with resolver v3, dev-dependency features never unify into `cargo build`."
So for an **oracle** crate, an OpenSSL-tainted default feature set is tolerable;
for the shipped graph it is not. This materially widens the oracle candidate
pool relative to the shipped-dependency pool — `claudius`, disqualified in §5.2
for `default = [… "native-tls"]`, is not disqualified as a dev-only oracle.

### 7.2 The official SDKs are NOT schema oracles — verified

This is the load-bearing negative of the oracle survey. Both official parsers
are *deliberately* non-validating.

**TypeScript (`7ba6a3fc`).** `src/core/streaming.ts:70`, `:127`, `:195` all do
`yield JSON.parse(sse.data) as Item` — a bare TypeScript cast, zero runtime
validation. `zod` is an *optional peer dependency* (`package.json:36,39,69`)
used for structured-output helpers, not response validation. The exhaustiveness
guard `checkNever` is a runtime no-op: `src/internal/utils/values.ts:119` is
literally `export function checkNever(_value: never): void {}`, so the
`default: checkNever(event.delta)` arm at `src/lib/MessageStream.ts:498`
silently ignores unknown delta types.

What it *does* enforce is stream **sequencing** — seven runtime throws in
`src/lib/MessageStream.ts`: `:572` "Unexpected event order, got X before
receiving `message_stop`", `:578` "…before `message_start`", `:340`/`:357`
"stream ended without producing a Message with role=assistant", `:364` "…without
producing a content block with type=text", `:527` "request ended without sending
any chunks", `:523`.

**Python (`181e2e57`).** Uses Pydantic's *non-validating* constructor.
`src/anthropic/_models.py:557` — `"""Construct a BaseModel class without
validation."""`. `:584-587` — `construct_type`: `"""Loose coercion to the
expected type… If the given value does not match the expected type then it is
returned as-is."""`. `:578-581` — `construct_type_unchecked`: "the returned
value from this function is not guaranteed to match the given type."
`:279-282` — unknown keys are preserved into `__pydantic_extra__`, never
rejected. `:290` aliases `model_construct = construct`.

**Therefore:** driving either official SDK as a spawned process is a **liveness
and sequencing oracle, not a shape oracle.** It will accept malformed roundhouse
output silently — precisely the "dropped in silence" failure
`codex_conformance.rs:7-10` exists to catch. There is no Anthropic-published
equivalent of `codex-api`'s strict serde parser, in any language, because
Stainless generates for forward-compatibility by design.

### 7.3 The candidate ledger

| Candidate | Strength | Evidence |
|---|---|---|
| **Real `claude` binary, gated e2e** | **Strongest available.** Doubles nothing on the client side. | The binary is present in this very environment: `/opt/node22/bin/claude`, `claude --version` → `2.1.247 (Claude Code)`. `ANTHROPIC_BASE_URL` is already an active env var here (`https://api.anthropic.com`), which is the redirection hook a loopback rig needs — the exact analog of `src/codex_launch.rs`. Cost: process spawn, feature gate, and it tests *Claude Code's* tolerance, not the API's contract. |
| **Generated-from-spec strict types** | **Only true shape oracle identified.** | §2/§3. Generate a *second*, deliberately strict type set from the pinned spec (response schemas + `additionalProperties:false` restored) purely as a dev-dependency test parser, independent of whatever the shipped path uses. Catches shape errors the official SDKs swallow. Does not cover `ping`/`error` (§3.1). |
| `claudius` as dev-only in-process parser | Moderate. | Bidirectional derives (§5.2) and a strict hand-written `ContentBlock::deserialize` that errors on unknown variants — genuinely strict where it has coverage. But its coverage stops at 2025 (no `web_fetch`, `service_tier`, `cache_creation`), so it would reject *correct* 2026 output. OpenSSL default is tolerable dev-only per §7.1. |
| `adk-anthropic` as dev-only parser | Moderate, broader, laxer. | Widest block coverage (§5.3) but `serde(tag="type")` without catch-alls and the invented `cache_creation_input_tokens_1h` would mis-assert on the cache counters. |
| Official TS/Python SDK spawned | Weak for shape, useful for sequencing. | §7.2. Best used to assert event *order* and stream termination, not field shapes. |
| NeMo Relay gateway parser | **Not assessed here** — Dive B covers Relay. Noted as an option only. | — |

---

## 8. Open questions and forks left unresolved

1. **Typed vs. `Value` pass-through for the serve surface.** §3 and §6 give the
   trade both ways. Roundhouse is not a pure relay — it originates responses for
   locally-routed turns — so the `shunt` posture cannot be adopted wholesale;
   but it may be right for the *inbound* half.
2. **If generated: restore or suppress `additionalProperties:false`?** §3. The
   choice can differ per direction (strict for dispatch-side request emission,
   open for serve-side request admission) and per build (open in the shipped
   path, strict in the oracle). Nothing forces one answer.
3. **Does Claude Code 2.1.247 send the beta or non-beta request shape?**
   `BetaCreateMessageParams` adds seven properties (§3). **UNVERIFIED** — I did
   not capture real Claude Code traffic. This is directly answerable with the
   loopback rig of §7.3 and should be settled before the serve schema is fixed.
4. **Spec-refresh cadence.** The spec URL is content-addressed with no `latest`
   alias (§2). If it becomes a pin, the manifest needs the unlock/refresh
   condition written next to it, per `CLAUDE.md` — the mechanism is "diff
   `.stats.yml` at a newer SDK rev", and the pin should record which SDK rev the
   snapshot came from.
5. **`ping` and `error` are hand-work regardless** (§3.1). No spec, no crate,
   and no official SDK type covers the SSE-transport events as *emittable*
   values.
6. **`siumai-core` coupling.** `siumai-protocol-anthropic` is the only crate
   with the right *shape* (§5.1) and the best 2026 coverage, but adopting it
   means adopting siumai's neutral vocabulary and writing the reverse direction
   anyway. Whether its options/annotations types are worth vendoring
   independently of its codec is a judgement not made here.
7. **Not deep-read:** `llm-bridge-core`, `oven-sdk-anthropic`,
   `everruns-anthropic`, `klieo-llm-anthropic`, `agentkit-provider-anthropic`,
   `trimwire`, `claude-proxy`. The §5 matrix scored them; none showed coverage
   or a crate split that would change the shortlist, but their source was not
   read line by line.
