<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# NeMo Relay 0.8.0, as published: Claude Code interception, Anthropic wire types, the launcher

> **Independently fact-checked 2026-08-27**: 15 load-bearing claims re-derived
> from freshly downloaded tarballs by a separate agent; all 15 confirmed on
> substance. Three line-number citations in §2.3 were wrong in the first draft
> (the functions were real and byte-identical, the numbers were transcribed
> wrong) and are corrected below. The checker also surfaced a tenth family
> crate (`nemo-relay-ffi`, same 0.8.0/0.8.1-rc.1 release batch) reinforcing
> §5.4, and noted that `crates/roundhouse-relay/Cargo.toml:33-57`'s own pin
> comment independently asserts the same ATOF byte-identity §5.2 establishes.
> The ruling this evidence feeds is `../PLAN-anthropic-messages.md`.

> **Status: evidence base (DIVE B).** A read of the **published crates.io
> tarballs** of the NeMo Relay 0.8.0 release cut on 2026-08-26, answering four
> questions for a coming design ruling: how Relay intercepts Claude Code at
> 0.8.0, what Anthropic wire vocabulary Relay actually owns in Rust, what the
> CLI's real command/config surface is, and what moving roundhouse's
> `nemo-relay-types = "=0.7.3"` pin would cost or free. This document produces
> evidence, not rulings. Where a call could go two ways it is stated both ways
> and stopped.

**Sources, pinned.** The NVIDIA GitHub repos are unreachable from this
environment, so every Relay citation below is against a published `.crate`
tarball downloaded from `static.crates.io` on **2026-08-27**:

| Crate | Version | Published (crates.io API, accessed 2026-08-27) | `.cargo_vcs_info.json` sha1 | `path_in_vcs` | sha256 of tarball |
|---|---|---|---|---|---|
| `nemo-relay-cli` | 0.8.0 | 2026-08-26T22:49:21Z | `812613b8503e02a012d01db590da5624c64b4e44` | `crates/cli` | `788335d0…7174c6` |
| `nemo-relay` | 0.8.0 | 2026-08-26T22:49:10Z | `812613b8503e02a012d01db590da5624c64b4e44` | `crates/core` | `ada298ff…6624e0` |
| `nemo-relay-types` | 0.8.0 | 2026-08-26T22:49:00Z | — | `crates/types` | `d72dc155…7b01f17` |
| `nemo-relay-types` | 0.7.3 | 2026-08-14T14:39:56Z | — | `crates/types` | `3701e3e3…4013fa6` |

Roundhouse read at `/home/user/roundhouse` @ **`306e6e0d8be7f643c0fa63910cfd876cff658d6b`**.
Citations are `<crate>-<version>/<path>:<line>` for Relay and plain repo-relative
paths for roundhouse. Prior reads this document updates:
`agent-docs/research/nemo-relay-deep-dive.md` (Relay @ `c37b551`, re-read @
`1a54812`) and `agent-docs/research/relay-switchyard-dedup-deep-dive.md`.

**One framing note the header must carry.** The CLI and core tarballs both
report VCS sha1 `812613b8…`, and the workspace version they carry is
**0.8.0** — *not* the 0.9.0 workspace at `1a54812` that the 2026-08-21 re-read
saw on the release/0.8 merge. The published 0.8.0 is therefore a **different
revision** from either prior read: newer than `c37b551`/`ca08901`, and on the
release line rather than main. Several things below are new relative to *both*
prior reads and should not be attributed to churn since `1a54812`.

---

## 0) What moved, in one page

1. **The Anthropic upstream is now a first-class configurable**, on three
   layers: a `--anthropic-base-url` flag, a `NEMO_RELAY_ANTHROPIC_BASE_URL`
   env var, and an `[upstream] anthropic_base_url` key in `config.toml`, each
   paired with an `anthropic_auth_header`. At `c37b551` the prior read recorded
   only `--openai-base-url`. This is the mechanism that chains Relay in front
   of roundhouse on the Messages surface (§2.5).
2. **The gateway grew one route** (`/v1/images/generations`) and still exposes
   exactly two Anthropic routes. No OAuth endpoints, no `/v1/complete`, no
   batches (§2.1).
3. **Relay owns no typed Anthropic wire structs.** `codec/anthropic.rs` is 1,389
   lines of `serde_json::Value` → Relay-neutral-IR mapping with exactly two
   private serde structs. `thinking`/`redacted_thinking` are handled as opaque
   provider-native blobs, and the streaming accumulator drops `thinking_delta`
   and `signature_delta` outright (§3).
4. **The CLI is a 13-subcommand launcher/installer/config-editor** with an
   interactive `dialoguer` wizard and structured editor — and **no TUI** in any
   sense of the word (§4).
5. **The `=0.7.3` pin's blockers all survive.** `nemo-relay-types` 0.8.0 still
   requires `uuid = "=1.18.1"`; its `Cargo.toml.orig` is byte-identical to
   0.7.3's. `codec/optimization.rs` is byte-identical; the ATOF envelope is
   field-identical; `api/event.rs` changes are purely additive (§5).
6. **`nemo-relay-switchyard` is dead on crates.io.** Every sibling crate
   published 0.8.0 on 2026-08-26 and 0.8.1-rc.1 on 2026-08-27; the switchyard
   crate's newest version is still 0.7.3 from 2026-08-14 (§5.4).

---

## 1) Claude Code interception: the gateway route table

### Finding 1.1 — The complete route table, exhaustively

`nemo-relay-cli-0.8.0/src/server/mod.rs:576-591` is the whole axum router. It
is a flat list with no `nest`, no fallback, and no wildcard:

```
/healthz                      GET   healthz
/bootstrap/tunnel             GET   bootstrap_tls_tunnel
/bootstrap/shutdown           POST  shutdown_bootstrap_sidecar
/hooks/codex                  POST  codex_hook
/hooks/claude-code            POST  claude_code_hook
/responses                    POST  gateway::passthrough
/chat/completions             POST  gateway::passthrough
/models                       GET   gateway::models
/v1/responses                 POST  gateway::passthrough
/v1/chat/completions          POST  gateway::passthrough
/v1/images/generations        POST  gateway::images_generations
/v1/messages                  POST  gateway::passthrough
/v1/messages/count_tokens     POST  gateway::passthrough
/v1/models                    GET   gateway::models
```

`ProviderRoute::from_path` (`src/gateway/routes.rs:52-65`) is the matching
classifier and returns `None` for anything else, which
`prepare_gateway_request` turns into `CliError::InvalidPayload("unsupported
gateway path …")` (`src/gateway/request.rs:45-47`).

**Answering the question's sub-parts, with the negatives stated as negatives:**

- `/v1/messages` — **yes**, `ProviderRoute::AnthropicMessages`.
- `/v1/messages/count_tokens` — **yes**, `ProviderRoute::AnthropicCountTokens`.
- `/v1/models` — **yes, but it is an OpenAI route.** `routes.rs:59-60` maps both
  `/models` and `/v1/models` to `ProviderRoute::OpenAiModels`, and
  `upstream_url` (`routes.rs:116-124`) sends `OpenAiModels` to
  `config.openai_base_url`. **Anthropic's `/v1/models` is unreachable through
  this gateway**: there is no route that forwards a models listing to
  `anthropic_base_url`.
- **OAuth endpoints — none.** `grep -rni oauth` over
  `nemo-relay-cli-0.8.0/src` returns four hits, all comments
  (`src/agents/codex/alignment.rs:119`,
  `src/commands/configure/wizard/prompt.rs:159,161,169`); zero route
  registrations, zero handler functions. Neither `/v1/oauth/*`, `/api/oauth/*`,
  nor any token-exchange path appears in the router.
- **No `/v1/complete`, no `/v1/messages/batches`, no `/v1/organizations`, no
  `/v1/files`.** Established by `grep -rn 'v1/complete\|messages/batches\|/v1/organizations\|/v1/files'`
  over `nemo-relay-cli-0.8.0/src`: zero hits.
- **`anthropic-version` and `anthropic-beta` are never named.** `grep -rn
  'anthropic-version\|anthropic_version\|anthropic-beta'` over both
  `nemo-relay-cli-0.8.0/src` and `nemo-relay-0.8.0/src`: zero hits. Relay
  neither injects nor validates the API-version header; it survives only as a
  generic forwarded header (§2.4).

### Finding 1.2 — A second, undocumented redirect surface: internal dispatch headers

`src/gateway/mod.rs:51-54` defines four internal headers, and
`dispatch_overrides` (`:965-984`) reads two of them off the **rewritten
`LlmRequest.headers` map** an intercept produced:

```rust
const INTERNAL_DISPATCH_URL_HEADER:   &str = "x-nemo-relay-internal-dispatch-url";
const INTERNAL_DISPATCH_ROUTE_HEADER: &str = "x-nemo-relay-internal-dispatch-route";
```

`effective_dispatch_request` (`:874-908`) then uses them to replace the upstream
URL *and* the target route, and `ProviderRoute::from_dispatch_override`
(`routes.rs:67-90`) accepts `"anthropic_messages" | "anthropic.messages" |
"/v1/messages"` among others. So **a plugin intercept can move any request to
any provider route and any URL, per request**, independent of configuration.

The safety property attached: when either header is present,
`credential_policy` becomes `TargetCredentialPolicy::ExplicitTarget` and
`remove_provider_credentials(&mut headers)` strips `Authorization`, `x-api-key`,
`api-key`, `anthropic-api-key` before the rewritten headers are applied
(`gateway/mod.rs:888-899`, `provider_auth.rs:159-164`). The comment at `:896-898`
states the reason: an explicit target must supply its own authorization rather
than inherit credentials meant for the original provider.

**Trade, both ways.** For roundhouse-as-upstream this is *better* than the
config path — a Relay plugin could route a Claude turn to roundhouse and the
caller's Anthropic credential would be stripped rather than leaked to us. But it
also means the configured `anthropic_base_url` is not the only thing that
decides where a Messages turn goes; any installed plugin can override it
silently, and nothing on the wire tells the downstream that it happened.

---

## 2) Claude Code interception: launch, auth, and the chaining key

### Finding 2.1 — What is injected into Claude Code, exactly

Two paths. **Ephemeral (transparent run)**, `src/agents/claude/launch.rs:13-95`,
reached from `nemo-relay claude` / `nemo-relay run --agent claude`:

*Environment* (`PreparedAgentLaunch::env`, built at
`src/process/launcher.rs:485-501` then extended):
```
NEMO_RELAY_GATEWAY_URL     = <gateway>            launcher.rs:487-490
NEMO_RELAY_TRANSPARENT_RUN = 1                    launcher.rs:493
PATH                       = <hook dir>:$PATH     launcher.rs:500-501 (when resolvable)
NEMO_RELAY_PROXY_CREDENTIAL= nrp_<64 hex>         agents/mod.rs:368-371 (secret)
ANTHROPIC_CUSTOM_HEADERS   = "x-nemo-relay-proxy-token: nrp_…"   claude/launch.rs:19-31 (secret)
ANTHROPIC_BASE_URL         = <gateway>            claude/launch.rs:92
```
`ANTHROPIC_CUSTOM_HEADERS` is *merged*, not overwritten: an existing value has
its same-named line replaced and the rest preserved
(`replace_custom_header`, `launch.rs:97-111`).

*Argv*, spliced immediately after the `claude` executable token
(`insert_after_host`, `process/prepared.rs:29-37`; call at `launch.rs:80-89`):
```
--plugin-dir <tmp>/nemo-relay-claude-plugin-<uuidv4>
--settings   <tmp>/nemo-relay-claude-plugin-<uuidv4>/settings.json
```
The temp dir gets `.claude-plugin/plugin.json` (`launch.rs:55-63`) and
`hooks/hooks.json` (`launch.rs:70-73`). The synthesized `settings.json` is the
user's own `--settings` (file **or inline JSON**, `read_settings`,
`launch.rs:165-181`) deep-merged with `env.ANTHROPIC_BASE_URL = <gateway>`
(`settings_overlay`, `launch.rs:113-134`) and written `atomic_write_private`
(`launch.rs:78`).

So `ANTHROPIC_BASE_URL` is set **twice** — process env and settings `env` —
which is a deliberate belt-and-braces against a host that filters inherited
environment (the same reasoning is stated for hook delivery at
`src/hooks/encoding.rs:69-70`).

**Hooks.** `CodingAgent::ClaudeCode`'s descriptor
(`src/agents/claude/mod.rs:14-36`) declares **fourteen** hook events —
`SessionStart, UserPromptSubmit, UserPromptExpansion, PreToolUse, PostToolUse,
PostToolUseFailure, PermissionRequest, SubagentStart, SubagentStop,
Notification, Stop, PreCompact, PostCompact, SessionEnd` — against Codex's ten
(`src/agents/codex/mod.rs:15-34`). Fail-closed eligibility is unchanged:
`event_requires_fail_closed` matches only `PreToolUse | PermissionRequest |
pre_tool_call` (`src/hooks/encoding.rs:412-414`).

**A version gate now exists for Claude too.** `minimum_version: (2, 1, 121)`
(`claude/mod.rs:21`), enforced before launch by a `claude --version` probe
whose output must parse after stripping the `" (Claude Code)"` suffix
(`claude/mod.rs:38-41`, `agents/mod.rs:87-105`, `process/launcher.rs:329-334`).
Pre-release versions are rejected outright (`agents/mod.rs:97`). Codex's gate is
still `(0, 143, 0)` (`codex/mod.rs:22`) — unchanged from the prior read.

### Finding 2.2 — The persistent Claude install path (new relative to both prior reads)

`src/agents/claude/host.rs` (256 lines) is the Claude analogue of the Codex
`host.rs` the 2026-08-21 re-read found. `enable_claude_provider`
(`host.rs:50-93`) rewrites `~/.claude/settings.json` (`claude_settings_path`,
`host.rs:243-245`) inserting `env.ANTHROPIC_BASE_URL = <gateway>`, after
snapshotting the original to a sidecar backup and stamping
`__nemo_relay_managed_anthropic_base_url` into it (`host.rs:95-102`) so
`restore_claude_provider` (`:104-149`) can tell an installer-owned value from a
user edit and refuse to revert the latter (`:142-147`). Absent-file and
dangling-symlink cases are recorded as explicit backup keys (`host.rs:19-21`,
`:221-228`).

Note what this path does **not** do: it writes only `ANTHROPIC_BASE_URL`. There
is no persistent `ANTHROPIC_CUSTOM_HEADERS` and therefore no persistent
`nrp_` proxy token — the persistent install relies on the shared-gateway
bootstrap client token instead (`src/server/mod.rs:552-566`).

### Finding 2.3 — Provider auth: the `nrp_` token, and what reaches upstream

`src/provider_auth.rs:18-22`:
```rust
pub(crate) const TRANSPARENT_PROXY_CREDENTIAL_ENV:    &str = "NEMO_RELAY_PROXY_CREDENTIAL";
pub(crate) const TRANSPARENT_PROXY_CREDENTIAL_HEADER: &str = "x-nemo-relay-proxy-token";
const TOKEN_BYTES: usize = 32;
const PROVIDER_API_KEY_HEADERS: [&str; 3] = ["x-api-key", "api-key", "anthropic-api-key"];
```
`generate()` (`:28-38`) mints `nrp_` + 64 hex chars per invocation.
`consume(&self, headers)` (`:46-86`) removes the proxy-token header if it
matches (constant-time, `:88-93`), and additionally removes `Authorization` or
any of the three API-key headers **only when their value equals the `nrp_`
token** (`:61-76`) — i.e. only the wrapper's own credential is consumed. If
nothing authenticated, the request is rejected `Unauthorized` (`:78-82`).

**The seat question, answered in three steps:**

1. **Relay never strips an Anthropic credential.** The only credential-stripping
   path is Codex-specific: `strip_chatgpt_auth_for_openai_route`
   (`src/agents/codex/alignment.rs:97-110`) returns `headers.clone()`
   unchanged unless `is_openai_route(route)`, and `is_openai_route`
   (`:130-138`) enumerates only the four OpenAI variants. The comment at
   `:128-129` says it in words: *"The ChatGPT auth transport fallback applies
   only to OpenAI-family routes. Anthropic routes use a different auth scheme
   and should never be redirected through Codex's ChatGPT backend."*
2. **Relay never redirects an Anthropic route.**
   `gateway_upstream_url_override` (`src/agents/shared/alignment.rs:428-443`)
   delegates to exactly one function, `codex::chatgpt_upstream_url_if_needed`,
   which is guarded by the same `is_openai_route`
   (`codex/alignment.rs:100-108`). There is no Anthropic analogue.
3. **The client's `Authorization` is forwarded verbatim.**
   `should_forward_request_header` (`src/gateway/response.rs:59-72`) excludes
   only hop-by-hop headers, headers named by `Connection`, `Host`,
   `Content-Length`, the bootstrap client token, the `nrp_` proxy token, and
   `Accept-Encoding`. `Authorization`, `x-api-key`, `anthropic-version`,
   `anthropic-beta` and every other Anthropic header are **not** on that list;
   `forward_upstream_request` (`gateway/mod.rs:798-848`) copies each surviving
   header onto the reqwest builder at `:825-829`. The *observability* filter is
   stricter and does redact them (`should_record_header`,
   `response.rs:81-88`) — but that is the recorded copy, not the wire.
4. **Injection happens only into a credential vacuum.**
   `inject_provider_auth_with_env` (`gateway/mod.rs:1059-1108`) returns the
   builder untouched when `already_authed` — any of `Authorization`,
   `x-api-key`, `api-key`, `anthropic-api-key` present (`:1073-1078`).
   Otherwise a configured `upstream.anthropic_auth_header` is sent as
   `Authorization` (`:1080-1082`), else `ANTHROPIC_API_KEY` is sent as a raw
   `x-api-key` with **no `Bearer` prefix** (`:1086-1090`, `:1101-1106` — the
   Bearer prefix is applied on OpenAI routes only).

**So: a subscription seat survives the Relay hop untouched.** Whatever
credential Claude Code presents reaches `anthropic_base_url` byte-identical.
**UNVERIFIED, and it is the one gap in this chain:** exactly *which* header
Claude Code populates when `ANTHROPIC_BASE_URL` points at a custom host and the
user is on a subscription seat. I did not read Claude Code's source (it is not
in this environment and is not open), so this document establishes what Relay
does to a credential, not what credential arrives. Any ruling that depends on
the seat surviving needs that half checked empirically.

### Finding 2.4 — The chaining key: `anthropic_base_url`, three layers

This is the direct answer to "is there an upstream-override config that can
point Relay's Anthropic upstream at a different base URL".

**Yes, and it is stable, documented, and interactive.** The resolved value lives
on `GatewayConfig` (`src/configuration/types.rs:22-32`) and defaults to
`https://api.anthropic.com` (`types.rs:115`). Three write layers, lowest
precedence first:

| Layer | Key / flag | Citation |
|---|---|---|
| `config.toml` `[upstream]` | `anthropic_base_url`, `anthropic_auth_header` | `configuration/mod.rs:69-73` (`FileUpstreamConfig`, `#[serde(deny_unknown_fields)]`), applied at `:1273-1308` |
| Environment | `NEMO_RELAY_ANTHROPIC_BASE_URL`, `NEMO_RELAY_ANTHROPIC_AUTH_HEADER` | `configuration/mod.rs:1596-1608` |
| CLI flag | `--anthropic-base-url` (top-level `ServerArgs`, also `env=NEMO_RELAY_ANTHROPIC_BASE_URL`) | `commands/serve.rs:20-22`; also on `run`, `commands/run.rs:32-33` |

File discovery is XDG-then-system, explicit `--config` replacing only the user
layer (`config_paths`, `configuration/mod.rs:1157-1166`;
`user_config_path`/`system_config_dir`, `:1218-1232`), deep-merged
(`merge_toml`, `:1656+`).

`nemo-relay config edit` gives an interactive path to the same keys:
`edit_upstream` offers `openai_base_url`, `openai_auth_header`,
`anthropic_base_url`, `anthropic_auth_header`
(`commands/configure/editor/prompt.rs:110-136`), with the auth header entered
through a `Password` prompt (`prompt.rs:237-247`) and redacted in previews
(`editor.rs:112-129`).

**Three guards worth naming:**

- **Changing a base URL silently clears its auth header.**
  `replace_upstream_base_url` (`configuration/mod.rs:1621-1630`) sets
  `*auth_header = None` whenever the new base differs from the old. So `--anthropic-base-url
  https://roundhouse.internal` on top of a file that configured an
  `anthropic_auth_header` drops that header — deliberate (credentials are
  per-endpoint), but it means a chained deployment must set both in the same
  layer.
- **Auth headers are refused in system config.** `ConfigDocument::write` for
  `TargetScope::Global` errors with *"global config cannot include upstream
  authorization headers; configure credentials in a user config"*
  (`editor.rs:89-104`, guard at `:107-111`); user files are written
  `atomic_write_private` (`:101`).
- **Auth-header values are validated as HTTP header values, not as URLs.**
  `validate_auth_header` (`configuration/mod.rs:1632-1639`) trims, rejects
  empty, and requires `HeaderValue::from_str` to succeed. **There is no
  validation of the base URL at all** — no scheme check, no loopback check, no
  https requirement. `upstream_url_with_base` just trims a trailing slash and
  concatenates (`routes.rs:141-151`); OpenAI routes additionally normalize a
  `/v1` prefix (`normalize_openai_path_for_base`, `:184-193`) and **Anthropic
  routes deliberately do not** (`:143-149`, the `_ =>` arm). So a roundhouse
  Messages surface chained behind Relay receives the path exactly as Claude
  Code sent it, appended to the configured base.

### Finding 2.5 — Chaining hazards specific to the Anthropic route

Recorded as evidence, not as an argument for or against chaining.

1. **Bodies are decoded and reserialized on the managed path.**
   `request_body_for_observability` (`gateway/request.rs:101-129`) decodes
   `identity`/`zstd` content-encodings (others pass through undecoded), and
   `reencode_request_body` (`gateway/mod.rs:926-946`) re-serializes
   `request.content` with `serde_json::to_vec` whenever an intercept produced a
   non-null content, stripping `Content-Encoding` afterwards
   (`:900-902`). serde_json's default `Map` is BTreeMap-backed, so **object key
   order is not preserved** through a rewrite. That is the same hazard class
   Switchyard fixed with `preserve_order` (recorded at
   `relay-switchyard-dedup-deep-dive.md`, `0acde7b`); `nemo-relay-0.8.0/Cargo.toml`
   declares `serde_json = "1"` with no features, so `preserve_order` is off.
2. **SSE is decoded and re-encoded per frame, and `id:`/`retry:` are dropped.**
   `sse_json_stream` (`gateway/mod.rs:605-635`) yields **only `event.data`**,
   discarding the decoded `SseEvent.event` name; `encode_sse_frame`
   (`:780-793`) reconstructs `event:` from the JSON `type` field for
   `AnthropicMessages | OpenAiResponses`. The decoder itself
   (`nemo-relay-0.8.0/src/codec/streaming.rs:182-198`) states the rest:
   *"Other lines (`id:`, `retry:`, comments starting with `:`) are ignored."*
   Frames with no `data:` line are dropped (`:196-198`). **A roundhouse
   resumption cursor carried as an SSE `id:` line would not survive a Relay hop.**
   Anthropic's `event: ping` frames do survive, because they carry a `data:`
   payload with a `type`.
3. **The `[DONE]` sentinel is correctly route-scoped.** `client_sse_body`
   appends `data: [DONE]\n\n` only `if matches!(route,
   ProviderRoute::OpenAiChatCompletions)` (`gateway/mod.rs:671-675`), and the
   decoder drops an inbound `[DONE]` (`streaming.rs:203-207`). No stray OpenAI
   terminator is injected into an Anthropic stream.
4. **A malformed SSE frame aborts the stream.** `push_bytes_results` stops
   after the first parse error (`streaming.rs:155-160`) and `client_sse_body`
   finishes the guard and yields `CliError::InvalidPayload` (`gateway/mod.rs:664-669`).
5. **Body-size ceilings.** `max_passthrough_body_bytes` defaults to 100 MiB and
   `max_hook_payload_bytes` to 20 MiB (`configuration/mod.rs:46-47`); the axum
   `DefaultBodyLimit` on the whole router is the **hook** limit
   (`server/mod.rs:592`), while passthrough reads to the passthrough limit
   (`gateway/request.rs:48-50`).

---

## 3) Anthropic wire types inside Relay

### Finding 3.1 — Where they are, and what shape they take

The whole Anthropic surface is `nemo-relay-0.8.0/src/codec/anthropic.rs`, 1,389
lines, `pub mod anthropic` off `src/codec/mod.rs:17`. It contains **six** type
definitions in total:

| Line | Type | Visibility | Role |
|---|---|---|---|
| `:43` | `AnthropicMessagesCodec` | `pub` | unit struct, impls `LlmCodec` + `LlmResponseCodec` |
| `:72` | `RawAnthropicResponse` | private | `#[derive(Deserialize)]`, 10 fields + `#[serde(flatten)] extra` |
| `:89` | `RawAnthropicUsage` | private | `#[derive(Deserialize)]`, 6 fields |
| `:1139` | `AnthropicMessagesStreamingCodec` | `pub` | `Arc<Mutex<State>>` wrapper |
| `:1186` | `AnthropicMessagesStreamingState` | private | JSON accumulator |
| `:1203` | `StreamingBlock` | private | per-index JSON accumulator |

**There is no typed Anthropic request struct at all.** `LlmCodec::decode`
(`:980-1098`) reads `request.content.as_object()` and pulls named keys out of a
`serde_json::Map` one at a time with private helpers
(`super::optional_string/_object/_bool/_u64`, defined at
`nemo-relay-0.8.0/src/codec/mod.rs:34-130` — **all private, none `pub`**), then
emits Relay's provider-neutral `AnnotatedLlmRequest`. Content blocks are decoded
by `decode_anthropic_content_part` (`:238-343`), a `match` on the block's `type`
string producing Relay's `ContentPart` enum.

So the honest characterization: **Relay has an Anthropic *codec*, not an
Anthropic *type system*.** Everything Anthropic-shaped is `serde_json::Value`
either side of a hand-written mapping into a provider-neutral IR owned by
`nemo-relay-types`.

### Finding 3.2 — Scorecard against the checklist

| Checklist item | Status at 0.8.0 | Evidence |
|---|---|---|
| `text` block | typed → `ContentPart::Text { text, extra }` | `anthropic.rs:245-258` |
| `image` / `document` | half-typed → `ContentPart::Image`/`File` carrying the raw object minus `type` | `:259-273` |
| `tool_use` | typed → `ContentPart::ToolUse { id, name, input, extra }`; missing `id`/`name`/`input` is a hard `InvalidArgument` | `:274-297` |
| `tool_result` | typed → `ContentPart::ToolResult { tool_use_id, content, is_error, extra }` | `:298-337` |
| **`thinking`** | **NOT typed** — falls to the `_ =>` arm → `ContentPart::ProviderNative { provider: "anthropic_messages", kind: "thinking", value }` | `:334-342`, `native_component` `:212-222` |
| **`redacted_thinking`** | **NOT typed** — same catch-all | same |
| `mcp_tool_use` / `server_tool_use` | not typed; and explicitly **excluded from `tool_calls`** on the response side | `:936` comment; `anthropic_tool_calls` `:867-890` |
| **`cache_control` TTL variants** | **absent.** `cache_control` exists only as a top-level `Option<Json>` blob on the request; per-block `cache_control` survives only inside a `ContentPart::*::extra` map | `:1059`, `nemo-relay-types-0.8.0/src/codec/request.rs:410-413` |
| **`ephemeral_5m` / `ephemeral_1h`** | **absent everywhere.** `grep -rn 'ephemeral\|_1h_\|_5m_'` over `nemo-relay-0.8.0/src` and `nemo-relay-types-0.8.0/src`: zero hits | — |
| **`usage.cache_creation` breakdown** | **absent.** Only the flat `cache_creation_input_tokens: Option<u64>` | `anthropic.rs:92-93`, mapped to `Usage.cache_write_tokens` at `:908-909` |
| `usage` normalization | `input_tokens`→`prompt_tokens`, `output_tokens`→`completion_tokens`, `total_tokens` **computed** (Anthropic does not send it), `cache_read_input_tokens`→`cache_read_tokens` | `anthropic_usage`, `:892-919` |
| **`stop_reason` vocabulary** | **three mapped, everything else `Unknown`**: `end_turn`→`Complete`, `max_tokens`→`Length`, `tool_use`→`ToolUse`, `other`→`Unknown(other)`. **`stop_sequence`, `pause_turn`, `refusal`, `model_context_window_exceeded` all land in `Unknown`** — including `stop_sequence`, which has a modelled sibling field | `map_anthropic_stop_reason`, `:104-111`; `FinishReason` at `nemo-relay-types-0.8.0/src/codec/response.rs:195-230` |
| `tool_choice` | fully typed both directions: `auto/any/none/tool` ↔ `Auto/Required/None/Specific`; `disable_parallel_tool_use` inverted into `parallel_tool_calls` | `:150-211` |
| Tool definitions | `input_schema` ↔ `parameters` handled | `:495-563` |
| **Unknown-field tolerance** | **strong.** `RawAnthropicResponse` has `#[serde(flatten)] extra` (`:84-85`); the request decode collects everything not in `MODELED_REQUEST_KEYS` (17 names, `:117-141`) into `AnnotatedLlmRequest.extra`; every `ContentPart` variant carries an `extra` map; unknown block types become `ProviderNative` rather than an error; unknown SSE event types are ignored (`:1226-1229`) |

**One encode-side asymmetry.** `validate_anthropic_supported_fields`
(`:731-754`) *rejects* an encode when an intercept changed `store`,
`previous_response_id`, `truncation`, `reasoning`, `include`, `user`,
`max_output_tokens`, `max_tool_calls`, or `top_logprobs` — i.e. the
OpenAI-shaped fields. It does not check anything Anthropic-shaped.

### Finding 3.3 — Streaming: an accumulator, not a typed event model

`AnthropicMessagesStreamingState::observe` (`:1217-1230`) matches on five
strings and nothing else:

```
message_start        → id, model, role, type, usage        :1231-1250
content_block_start  → per-index skeleton (raw JSON object) :1252-1271
content_block_delta  → text_delta | input_json_delta | citations_delta  :1273-1309
message_delta        → stop_reason, stop_sequence, usage    :1311-1323
_                    → ignored (content_block_stop, message_stop, ping, unknown)
```

**Two load-bearing consequences.**

- **Extended thinking is lost on the streaming path.** The comment at
  `:1305-1307` is explicit: *"thinking_delta, signature_delta, and any future
  delta types fall through; the block skeleton retains whatever shape was set at
  content_block_start."* The `content_block_start` skeleton for a `thinking`
  block carries `{"type":"thinking","thinking":""}`, so `finalize`
  (`:1358-1382`) emits a thinking block with **empty content and no
  signature**. Relay's observability of a thinking-enabled Claude turn records
  the reasoning as blank.
- **There is no typed stream-event enum in use.** `NormalizedStreamEvent`
  (`nemo-relay-0.8.0/src/codec/streaming.rs:31-68`) is a provider-neutral
  seven-variant enum with a promising shape (`TextDelta`, `ToolCallStart`,
  `ToolCallArgumentsDelta`, `Finish`, `Usage`, `Error`) — but `grep -rn
  'NormalizedStreamEvent' nemo-relay-0.8.0/src` returns **exactly one hit: its
  own definition**. Nothing constructs it, consumes it, or converts to it. It
  is declared vocabulary with no implementation behind it.

Assembled tool input is `partial_json` concatenated and parsed, falling back to
the raw string on parse failure (`:1362-1375`).

### Finding 3.4 — What `nemo-relay-types` 0.8.0 itself carries

The question was whether the types crate is still observability-only. **It is
not, and it was not at 0.7.3 either** — but what it carries is *annotation*
vocabulary, not wire structs:

1. **`ApiSpecificRequest::AnthropicMessages`** — 7 fields, all
   `Option`: `cache_control: Option<Json>`, `container: Option<String>`,
   `inference_geo: Option<String>`, `output_config: Option<Json>`,
   `thinking: Option<Json>`, `top_k: Option<u64>`,
   `user_profile_id: Option<String>`
   (`nemo-relay-types-0.8.0/src/codec/request.rs:408-432`). **Byte-identical at
   0.7.3** (the 0.7.3→0.8.0 `request.rs` diff is 13 lines, all of them the new
   `OCIGenAI` variant).
2. **`ApiSpecificResponse::AnthropicMessages`** — `object_type`, `role`,
   `stop_reason` (the *raw* string, beside the normalized `FinishReason`),
   `stop_sequence`, `service_tier`, `container`, and
   `content_blocks: Option<Vec<Json>>` — the whole content array preserved
   verbatim (`response.rs:313-336`).
3. **`BuiltinLlmCodec::AnthropicMessages => "anthropic_messages"`** — new in
   0.8.0, in the new `codec/identity.rs` module
   (`nemo-relay-types-0.8.0/src/codec/identity.rs:47-58`), alongside
   `OpenAiChat`, `OpenAiResponses`, `OCIGenAI`, `GeminiGenerateContent`.
4. **`ContentPart`** — the provider-neutral content model, 8 variants: `Text`,
   `ImageUrl`, `Image`, `Audio`, `File`, `Refusal`, `ToolUse`, `ToolResult`,
   plus `ProviderNative` (`request.rs:213-290`ff). This is what a port would
   have to either adopt or replace.

So: **the crate is not observability-only; it is provider-neutral-IR plus
per-provider annotation slots.** No Anthropic request/response/stream struct
exists in it, at either version.

### Finding 3.5 — Inputs to the depend / port / neither decision

**Depend on the codec directly?** The codec is in `nemo-relay` (the core), not
`nemo-relay-types`. `nemo-relay-0.8.0/Cargo.toml` declares 28 direct
dependencies with `default = ["guardrails-remote", "object-store"]` (`:28-40`),
including `opentelemetry` ×4, `tonic`, `libloading`, `object_store`,
`spdlog-rs`, `reqwest`, `tokio`, `nemo-relay-worker-proto`. **The standing rule
in `agent-docs/synergies/nemo-relay.md` — "nemo-relay-types, nothing else" —
is exactly the rule this would break**, and the crate that would come along is
the 156k-line runtime, not a wire-types library.

**Port with attribution?** The codec is Apache-2.0
(`anthropic.rs:1-2`). But a port drags more than the file: it uses seven private
helpers from `codec/mod.rs:34-130` (all non-`pub`, so they must be
reimplemented), and its output type is `AnnotatedLlmRequest`/`AnnotatedLlmResponse`
with `ContentPart`/`MessageContent`/`ToolChoice`/`Usage`/`FinishReason`, which
live in `nemo-relay-types` — so a faithful port either takes the types crate as
a dependency (allowed by the standing rule) or re-types the IR (a much larger
port). Ported, roundhouse would inherit the §3.2 gaps: no TTL cache_control, no
`cache_creation` breakdown, `stop_sequence` in `Unknown`, and blank thinking
blocks on streams.

**Too partial to be either?** That is a defensible reading of the same
evidence: for a *front end that owns the turn* — which is what roundhouse is —
the missing pieces are precisely the ones that matter. `cache_control` TTL and
`usage.cache_creation` are the inputs to `FrontierQuote`/`CacheLedger`;
`stop_reason` completeness decides whether a turn is finished or must continue;
thinking blocks are conversation state that prefix admission has to reproduce
byte-exactly on the next turn. Relay can lose all three and still export a
useful trace, because Relay's job is observation. Roundhouse cannot.

**The trade, stated both ways and stopped.** *For adopting:* the mapping is
1,389 lines of already-debugged Anthropic-shape knowledge under a compatible
licence, it is maintained by a team that ships weekly, and reusing its
`ContentPart` vocabulary keeps a future ATOF/ATIF emission structurally aligned
for free. *Against:* it is a lossy normalizer aimed at observation, its output
type is not a wire type, its helpers are private, and the four gaps above are
each load-bearing for a stateful front end in a way they are not for a proxy —
so the port would begin with a rewrite of the parts that matter most.

---

## 4) The launcher

### Finding 4.1 — Command surface (clap, exhaustive)

`nemo-relay-cli-0.8.0/src/commands/root.rs:48-112` defines thirteen
subcommands, all optional — a bare `nemo-relay` is a fourth mode (below):

| Command | Args | Purpose |
|---|---|---|
| `claude` | `--dry-run`, `-- <argv…>` | run Claude Code under an ephemeral gateway; first use runs the wizard (`root.rs:50-62`) |
| `codex` | same | run Codex; injects the `nemo-relay-openai` provider (`root.rs:63-76`) |
| `mcp` | — | hold a shared gateway for an MCP stdio client, default bind `127.0.0.1:47632` (`root.rs:77-90`) |
| `config [agent] [--reset]` / `config edit [--user\|--global]` | | wizard / structured editor (`commands/configure/mod.rs:18-49`) |
| `plugins` | edit/list subcommands | write `plugins.toml` (`commands/plugins/`) |
| `install <codex\|claude-code\|all>` | `--install-dir --force --dry-run --skip-doctor` | persistent integration (`commands/install.rs:12-42`) |
| `uninstall <…>` | `--install-dir --dry-run` | reverse it |
| `model-pricing` | validate/resolve | pricing catalogs (`commands/model_pricing/`) |
| `doctor` | `--offline` | diagnostics |
| `agents` | `--json` | list supported/detected agents |
| `completions <shell>` | | shell completion script |
| `run` | `--agent --config --openai-base-url --anthropic-base-url --session-metadata --dry-run --print -- <argv…>` | deterministic launch, no wizard (`commands/run.rs:24-44`) |
| `hook-forward` | hidden | the subprocess installed hooks call (`root.rs:109-111`) |

Top-level `ServerArgs` are flattened onto every invocation
(`root.rs:40-41`, `commands/serve.rs:9-35`): `--config`, `--bind`,
`--openai-base-url`, `--anthropic-base-url`, `--plugin-config-path` (hidden),
`--ready-file` (hidden), `--max-hook-payload-bytes`,
`--max-passthrough-body-bytes` — all but the two hidden ones with `env = …`.

**Bare `nemo-relay`** is a three-way branch (`commands/mod.rs:194-244`): any
daemon flag → run the gateway daemon; else if a config file exists → run
`doctor`; else → run the first-run wizard.

### Finding 4.2 — What it reads and persists

- **`config.toml`**, XDG user then system, deep-merged
  (`configuration/mod.rs:1157-1166`, `:1078-1120`). Typed shape:
  `[gateway]` (two byte limits), `[upstream]` (four keys, §2.4),
  `[agents.claude] / [agents.codex]` (one `command` each), `[logging]`
  (`FileConfig`, `:54-88`). Two legacy shapes are **hard errors** with a
  migration message: any `[plugins]` table, and legacy observability sections
  (`:1096-1110`).
- **`plugins.toml`**, discovered as a sibling of an explicit `config.toml` or
  through core's search path (`:1170-1200`, `explicit_plugin_config_path`
  `:1189-1196`). This is where observability, pricing, adaptive, PII and dynamic
  plugin components are configured.
- **Environment**: 35 distinct `NEMO_RELAY_*` variables are read across the
  crate (enumerated by `grep -rhoE 'NEMO_RELAY_[A-Z_]+' src | sort -u`); the
  user-facing ones are the eight `ServerArgs` mirrors plus
  `NEMO_RELAY_OPENAI_AUTH_HEADER` / `NEMO_RELAY_ANTHROPIC_AUTH_HEADER`, which
  have **no CLI flag** — env or file only (`configuration/mod.rs:1582,1596`).
- **The wizard writes very little.** `commands/configure/wizard/prompt.rs:25-73`
  asks exactly one question — a `MultiSelect` over `[Claude Code, Codex]`
  ("Which agents to observe?", `:220-226`) — pre-checked from existing config
  ∪ `$PATH` detection, then previews the exact TOML and confirms
  (`:232-253`). It writes only `[agents.*] command`. **It never asks about
  upstreams, credentials, or models.** It then offers to chain into
  `nemo-relay plugins edit` (`:109-141`). It requires a TTY and errors
  otherwise (`:173-182`).
- **`nemo-relay config edit`** is the richer surface: a `Select` menu over
  gateway limits, "Provider upstreams", and logging
  (`commands/configure/editor/prompt.rs:44-52`), reaching the four upstream
  keys at `:110-136`.
- **`nemo-relay install claude-code`** builds a **local Claude Code plugin
  marketplace**: a marketplace manifest, a plugin manifest, and a `.mcp.json`
  registering the Relay MCP server with `"alwaysLoad": true`
  (`agents/claude/assets.rs:7-38`), plus the `~/.claude/settings.json` rewrite
  of §2.2. `installation/marketplace/mod.rs` is 2,284 lines.

**Named profiles: no.** There is a `profile` concept —
`SessionConfig.profile` read from the `x-nemo-relay-config-profile` header
(`configuration/types.rs:48`), stamped onto session metadata as
`gateway_config_profile` (`sessions/mod.rs:1128`,
`agents/shared/adapters.rs:396-397`) and onto hook deliveries
(`hooks/delivery.rs:266`) — but it is **a label, not a configuration set**.
Nothing in `configuration/mod.rs` selects a config layer by profile name;
`grep -rn 'config_profile\|config-profile' src` returns five hits, all
metadata-stamping. The multi-config story is instead "one file per config, select
with `--config`".

### Finding 4.3 — There is no TUI

**Established negatively and exhaustively.** `grep -rniE
'ratatui|crossterm|cursive|termion|tui-rs|inquire|indicatif'` across the entire
`nemo-relay-cli-0.8.0` tarball — `src/`, `tests/`, `Cargo.toml`,
`Cargo.toml.orig`, `README.md` — returns **zero** matches on those crate names
(the eight hits are all substrings inside the words "recursively" and
"recursive"). The interactive surface is:

- **`dialoguer 0.11`**, `default-features = false, features = ["password"]`
  (`Cargo.toml`, `[dependencies.dialoguer]`) — `Select`, `MultiSelect`,
  `Confirm`, `Input`, `Password`, `ColorfulTheme`. Line-oriented prompts.
- **`console 0.16`** — styling and terminal detection.
- **`src/banner.rs`** (316 lines) — a figlet-style ASCII intro gated on
  `IsTerminal` (`banner.rs:15`, `print_intro` `:291`).

No alternate screen, no event loop, no widget tree, no full-screen redraw.

### Finding 4.4 — Mapped against "roundhouse wants a cli+tui binary"

Evidence only, no ruling. What Relay's CLI **already has** that such a binary
would need: agent detection on `$PATH`; a version gate per agent; ephemeral and
persistent install/uninstall with snapshot-and-restore that can tell
installer-owned edits from user edits; a `--dry-run` that prints the full launch
plan (argv + env + notes) without executing; secret-env marking so credentials
stay out of logs (`process/prepared.rs:20-27`); a structured config editor with
credential redaction and a global/user scope split; shell completions; a
`doctor` that probes live endpoints; and a Claude Code plugin-marketplace
installer that registers an MCP server. That is a substantial fraction of the
mechanical work, all Apache-2.0, all readable.

What it **does not have**: any full-screen UI; named/switchable configuration
profiles; any notion of a session log, budget, route, or model *choice* in the
config model (`FileConfig` has four sections and none of them names a model);
and a wizard that asks about anything except which agents to observe.

---

## 5) Vigilance on the `=0.7.3` pin

### Finding 5.1 — `uuid = "=1.18.1"` is unchanged; the unlock condition is not met

`nemo-relay-types-0.8.0/Cargo.toml` requires `uuid = "=1.18.1"` with features
`["v7","serde"]`. Stronger: **`Cargo.toml.orig` is byte-identical between 0.7.3
and 0.8.0** (`diff -u` produces no output) — same six dependencies, same
`schema` feature, same workspace-inherited `uuid`.

`crates/roundhouse-relay/Cargo.toml` records the unlock condition as *"a
`nemo-relay-types` release that relaxes its own `=1.18.1` to a caret"*. **0.8.0
does not.** The ceiling on roundhouse's graph (workspace declares a caret
`uuid = "1.18.1"`, `Cargo.toml:97`) stands unchanged whether the pin moves or not.

### Finding 5.2 — What actually changed, field by field

| Item | 0.7.3 → 0.8.0 |
|---|---|
| `src/codec/optimization.rs` | **byte-identical** (`cmp` exit 0). `LlmOptimizationSummary`, `…Contribution`, `…Kind`, `…Model`, `…ModelTransition`, `…Payload`, `…SummaryStatus`, `…TokenImpact`, `…Tokens`, `…EvidenceQuality` — all unchanged. `tests/fixtures/llm_optimization_contribution_v1.json` also byte-identical. |
| `src/api/scope.rs`, `src/api/llm.rs`, `src/lib.rs`, `src/plugin.rs` | **byte-identical** |
| `src/api/event.rs` | 865 → 1,577 lines, **purely additive**: `diff -u \| grep '^-[^-]'` returns **zero removed lines**. `ATOF_VERSION` still `"0.1"` (`:36`). `BaseEvent` fields unchanged (`atof_version, parent_uuid, uuid, timestamp, name, data, data_schema, metadata`); `ScopeEvent` (`base, scope_category, attributes, category, category_profile`), `MarkEvent` (`base, category, category_profile`) and `DataSchema` (`name, version`) all field-identical. Additions are the metric-mark surface: `MetricEnvelope`, `MetricMeasurement`, `MetricValue`, `MetricValueType`, `MetricKind`, `MetricAttributes`, `InstrumentDescriptor`, `InstrumentName`, `HistogramBoundaries`, `FiniteF64`, `ValidatedMetricMeasurement`, `MetricValidationError`, `validate_metric_measurements`, `AttributeValue`, `LogSeverity`, `ParseLogSeverityError`, `LOG_SEVERITY_METADATA_KEY`, `METRIC_DATA_SCHEMA_NAME`, `METRIC_DATA_SCHEMA_VERSION`. |
| `src/codec/request.rs` | +13 lines: one new `ApiSpecificRequest::OCIGenAI` variant. `AnthropicMessages` untouched. |
| `src/codec/response.rs` | +81/−~10: two new `ApiSpecificResponse` variants (`OCIGenAI`, `GeminiGenerateContent`), Gemini-aware doc updates on `FinishReason`, and **one behavior change**: `CostEstimate::total_or_component_sum` now returns `None` for a `ModelPricing`-sourced estimate with no explicit total, where 0.7.3 summed the components (`response.rs:151-158`). |
| `src/api/tool.rs` | 56 → 189 lines; new `ToolExecutionResult` plus two schema constants. Plugin-ABI territory; roundhouse does not touch it. |
| **New modules** | `src/api/registry.rs` (`RuntimeRegistrationKind` ×17, `…Owner`, `…Identity`) and `src/codec/identity.rs` (`BuiltinLlmCodec`, `LlmCodecIdentity`). |

### Finding 5.3 — What moving the pin would cost and free

Roundhouse touches `nemo-relay-types` in **exactly seven places**, all inside
`crates/roundhouse-relay` (`grep -rn nemo_relay_types --include=*.rs crates/`):
`atof.rs:63-65` (`BaseEvent, CategoryProfile, DataSchema, Event, EventCategory,
ScopeCategory, ScopeEvent`), `atof.rs:138,165,207` (`ATOF_VERSION`),
`wire.rs:22` (`DataSchema`), `summary.rs:71-77`
(the ten `LlmOptimization*` types plus `CostEstimate, CostSource, Usage`).

**Every one of those is byte-identical across 0.7.3 and 0.8.0.** So:

- **Cost of moving: zero code change, zero new transitive dependency.** The
  manifest is identical; the API surface we use is identical.
- **Freed by moving:** the metric-mark surface (`MetricEnvelope` et al.),
  `BuiltinLlmCodec`/`LlmCodecIdentity` (a stable string identity for
  `"anthropic_messages"` we could stamp on a decision record), and
  `RuntimeRegistrationKind`.
- **Not freed by moving:** the `uuid` ceiling (Finding 5.1).
- **One stale sentence in the manifest to fix either way.**
  `crates/roundhouse-relay/Cargo.toml` says the alternative is *"`=0.8.0-rc.1`
  (or to 0.8.0 final, **which is not out yet**)"*. 0.8.0 final shipped
  2026-08-26T22:49:00Z; and `0.8.1-rc.1` shipped 2026-08-27T15:32:36Z. If the
  pin stays, that parenthetical needs a dated addendum; if it moves, `=0.8.0`
  is now a released target.
- **The `total_or_component_sum` change is a non-event for us**, because
  roundhouse always constructs `CostEstimate { total: Some(_), .. }`
  (`crates/roundhouse-relay/src/summary.rs:294,313`). It matters only for a
  downstream consumer reading our output with 0.8.0 semantics.

### Finding 5.4 — `nemo-relay-switchyard` is dead; ATIF is now published

**Dead.** crates.io API, accessed 2026-08-27: every crate in the family
(`nemo-relay`, `-types`, `-cli`, `-adaptive`, `-ffi`, `-pii-redaction`,
`-plugin`, `-worker`, `-worker-proto`) has `max_version = 0.8.1-rc.1` published
2026-08-27T15:32Z, with 0.8.0 on 2026-08-26T22:49Z. **`nemo-relay-switchyard`
has `max_version = 0.7.3`, published 2026-08-14T14:40:17Z, and no 0.8 line at
all.** It was not yanked; it was simply left behind. This corroborates the
`88d1b1b` deletion recorded in `relay-switchyard-dedup-deep-dive.md` from the
publication side.

**ATIF is now published.** The previous position was that the ATIF structs lived
only in the unpublished heavy core. `nemo-relay` 0.8.0 is on crates.io and
carries `src/observability/atif.rs` with `ATIF_SCHEMA_VERSION = "ATIF-v1.7"`
(`:55`) and all twelve wire structs — `AtifAgentInfo:63`, `AtifStep:81`,
`AtifMetrics:122`, `AtifFinalMetrics:151`, `AtifToolCall:174`,
`AtifObservation:188`, `AtifObservationResult:195`,
`AtifSubagentTrajectoryRef:212`, `AtifAncestry:226`, `AtifInvocationInfo:244`,
`AtifStepExtra:267`, `AtifTrajectory:301` — plus the host-side
`AtifExporter:345`. Version is unchanged from every prior read: still v1.7, no
v1.8.

**What that does and does not change.** It makes the ATIF schema readable from a
versioned, immutable artifact rather than a repo revision — which is a real
improvement for the "re-verify pinned-source claims" discipline. It does **not**
make ATIF cheap to depend on: the structs are in `nemo-relay`, whose 28 direct
dependencies (§3.5) are exactly what the "nemo-relay-types, nothing else" rule
exists to keep out. The re-implementation-with-attribution position recorded in
`nemo-relay-deep-dive.md` D.1 row 2 is unaffected; only its source citation gets
better.

---

## 6) Open questions this evidence cannot close

1. **Which credential does Claude Code send to a custom `ANTHROPIC_BASE_URL`
   on a subscription seat?** Relay's side is fully established (§2.3): it
   forwards untouched. The client's side is UNVERIFIED — Claude Code's source
   is not readable in this environment. One live capture against a loopback
   listener settles it.
2. **Does prefix admission survive a Relay hop?** `reencode_request_body`
   reserializes with an alphabetizing `Map` (§2.5 hazard 1). Roundhouse's
   Responses-surface admission compares role + content, which should be robust,
   but no Messages-surface admission exists yet to test against, and the
   hazard's cost is silent session forking rather than a loud failure.
3. **Is the 0.8.1-rc line already changing any of this?** `0.8.1-rc.1` was
   published 2026-08-27T15:32Z, hours before this read. Nothing here was
   checked against it. Any milestone that depends on a §2 or §3 claim should
   re-read the then-current release rather than this one.
4. **Two readings of §3.5 are both defensible** — "port the codec, fix its four
   gaps" versus "write Anthropic wire types from the published API spec and take
   nothing" — and the evidence does not decide between them. What it does
   establish is that "depend on Relay's Anthropic types" is not an option at
   all, because there are no such types: there is a codec in the heavy core, and
   the standing dependency rule already excludes it.

---

## Addendum (2026-09-01): 0.8.2 as published — the chained-topology re-read

**Status: evidence base, re-read.** `agent-docs/PLAN-anthropic-messages.md` R9
requires the then-current Relay release to be re-read before chained-topology
work begins. This addendum re-derives R7's hazards 1–5 and the launcher/pin
questions against the **published crates.io tarballs of Relay 0.8.2**, diffed
byte-for-byte against the 0.8.0 tarballs §0–§6 above already established. Same
discipline: evidence only, no rulings; where a call could go two ways it is
stated both ways and stopped.

**Sources, pinned.** NVIDIA's GitHub repos remain unreachable from this
environment; `crates.io`'s JSON API also returned nothing over the configured
proxy (empty body, no error) — only `static.crates.io` tarball downloads
worked, so provenance below is `sha256` plus the CDN's `Last-Modified`
response header rather than the registry API's `created_at` the 0.8.0 table
used. Downloaded and extracted 2026-09-01 into
`/tmp/claude-0/-home-user-roundhouse/d6addde3-2039-5f5e-8af5-d560d8c0b623/scratchpad/dl/`:

| Crate | Version | `static.crates.io` `Last-Modified` | sha256 of tarball |
|---|---|---|---|
| `nemo-relay-cli` | 0.8.0 | Wed, 26 Aug 2026 22:49:22 GMT | `788335d0e0c0ef935b5618ad270de13916d88de4598bf2005f5277d7ca7174c6` |
| `nemo-relay-cli` | 0.8.2 | Mon, 31 Aug 2026 20:42:29 GMT | `c7af6bac293a917cd7890b0b47eadfcdd62b8aeff58c4ce763db9ee681cec9ab` |
| `nemo-relay` | 0.8.0 | Wed, 26 Aug 2026 22:49:11 GMT | `ada298ffbe51150d38eb8ee26a28b5b3c9d50dc69fa897ce6365a9e3396624e0` |
| `nemo-relay` | 0.8.2 | Mon, 31 Aug 2026 20:42:17 GMT | `beb2f214f4cf08f54a3f78ec0e643b6746eb6dbd19af0c055ce41dd841048719` |
| `nemo-relay-types` | 0.8.0 | Wed, 26 Aug 2026 22:49:01 GMT | `d72dc155be69eb9cc730441eed1dff1b326b51940bbf7feb1e788277d7b01f17` |
| `nemo-relay-types` | 0.8.2 | Mon, 31 Aug 2026 20:42:06 GMT | `5f8b1e6d9cd664315dc441030fcc457cfd4c22ddf48b2e39c71be89f3be64008` |

The 0.8.0 hashes match this document's original table exactly (§ header),
confirming the same 0.8.0 artifact is the diff base. **0.8.2 is the newest
published version of all three crates**: `static.crates.io` answers 200 for
`nemo-relay-cli-0.8.1.crate`, `-0.8.1-rc.1.crate`, and `-0.8.2.crate`, and 403
(the CDN's not-found status for this bucket, corroborated by
`nemo-relay-switchyard-0.8.0.crate` also 403-ing against a crate independently
known to be dead at 0.7.3, §5.4) for `-0.8.3.crate` and `-0.9.0.crate` — so
there is no 0.8.3, no 0.8.1 final release note beyond what 0.8.2 carries, and
no 0.9 line yet.

Citations below are `<crate>-0.8.2/<path>:<line>` unless marked
**[byte-identical to 0.8.0]**, in which case the 0.8.0 document's line numbers
above apply unchanged (verified by `diff -u` returning empty output on the
whole file, not just a hunk-free summary).

### A.1 — Method: which files moved at all

`diff -rq` of each crate's `src/` tree, 0.8.0 → 0.8.2:

- **`nemo-relay-types`: zero files differ.** `diff -ru
  nemo-relay-types-0.8.0/src nemo-relay-types-0.8.2/src` produces no output;
  13 files both sides, same names. `Cargo.toml.orig` also diffs to nothing
  except `version = "0.8.0"` → `"0.8.2"` (`diff -u
  nemo-relay-types-{0.8.0,0.8.2}/Cargo.toml`, one line changed, no dependency
  line touched).
- **`nemo-relay` (core), 11 of ~140 files differ**: `config_editor.rs`,
  `observability/{atif.rs,atof.rs,mod.rs,otel.rs,plugin_component.rs}` plus
  two new files (`observability/confined_fs.rs`, `observability/private_file.rs`),
  `plugin.rs`, `plugin/dynamic/{native.rs,worker.rs}`. **`src/codec/` — the
  entire Anthropic codec directory — has zero diffs**: `diff -ru
  nemo-relay-0.8.0/src/codec nemo-relay-0.8.2/src/codec` is empty, confirmed
  file-by-file (`anthropic.rs`, `streaming.rs`, `mod.rs` each `diff -u` exit
  0).
- **`nemo-relay-cli`, 34 of ~110 files differ** (full list captured in the
  scratchpad `diff -rq` output). Relevant to hazards 1–5: `gateway/mod.rs`,
  `gateway/routes.rs`, `agents/claude/host.rs`, `agents/claude/mod.rs`,
  `agents/codex/mod.rs`, `agents/codex/alignment.rs`, `process/launcher.rs`,
  `process/prepared.rs`, `commands/serve.rs`, `commands/run.rs`,
  `gateway/request.rs` — **all byte-identical** (`diff -u` exit 0 on each,
  individually confirmed, not inferred from the `-rq` omission alone). What
  *did* change in the cli crate: `configuration/mod.rs`, `provider_auth.rs`,
  `agents/claude/launch.rs`, `agents/claude/adapter.rs`,
  `agents/codex/adapter.rs`, `agents/mod.rs`, `agents/shared/{adapters,alignment}.rs`,
  `bootstrap/{mod,state}.rs`, `commands/{diagnostics,install,logging,mod,root}.rs`
  plus two new files (`commands/gateway.rs`, `commands/integrations.rs`),
  `configuration/logging.rs`, `gateway/client.rs`, `hooks/delivery.rs`,
  `installation/**`, `mcp/mod.rs`, `mcp_environment.rs`, `plugins/**`,
  `server/mod.rs`, `sessions/**`.

### A.2 — Hazard 1: the alphabetizing `serde_json::Map` re-encode

**Unchanged.** `reencode_request_body` (`gateway/mod.rs:927-946`, function
body identical to the 0.8.0 doc's `:926-946` citation — the file is
byte-identical, confirmed by `diff -u nemo-relay-cli-0.8.0/src/gateway/mod.rs
nemo-relay-cli-0.8.2/src/gateway/mod.rs` returning no output) still
`serde_json::to_vec`-serializes `request.content` whenever an intercept
produced non-null content. Both crates' `Cargo.toml` still declare
`serde_json = "1"` with **no features** — `grep -n
'serde_json\|preserve_order'` over `nemo-relay-0.8.2/Cargo.toml` and
`nemo-relay-cli-0.8.2/Cargo.toml` shows only the bare `version = "1"` line
each side, `preserve_order` absent, and a `diff -u` of the 0.8.0→0.8.2
`Cargo.toml`s shows no change to either `serde_json` stanza. So `serde_json`'s
default `Map` (`BTreeMap`-backed, alphabetizing) is still what a rewrite
serializes through at 0.8.2. **[byte-identical to 0.8.0]**

### A.3 — Hazard 2: the SSE re-encoder drops `id:` lines

**Unchanged.** `nemo-relay-0.8.2/src/codec/streaming.rs` is byte-identical to
0.8.0's (`diff -u`, exit 0) — the decoder comment *"Other lines (`id:`,
`retry:`, comments starting with `:`) are ignored"* and the frame-drop-on-no-
`data:` logic stand at the same lines the 0.8.0 document cites
(`streaming.rs:182-198`, `:196-198`). `gateway/mod.rs`'s `sse_json_stream`
(`:605-635`) and `encode_sse_frame` (`:780-793`) are likewise byte-identical.
**[byte-identical to 0.8.0]**

### A.4 — Hazard 3: `?beta=true` survival through `upstream_url` concatenation

**Unchanged — and now traced end to end, not just at the concatenation
site.** `nemo-relay-cli-0.8.2/src/gateway/routes.rs` is byte-identical to
0.8.0's: `upstream_url` (`:111-126`) and `upstream_url_with_base`
(`:141-151`) are the same functions at the same lines. `upstream_url_with_base`
trims a trailing slash off `base` and, for the `_ =>` arm covering
`AnthropicMessages | AnthropicCountTokens`, passes `path_and_query` through
untouched (`routes.rs:143-149`) before `format!("{base}{path_and_query}")`
(`:150`) — no query-string handling separate from the path at all; whatever
Axum captured as `path_and_query()` is one string throughout. Upstream of
that, `nemo-relay-cli-0.8.2/src/gateway/request.rs` is byte-identical to
0.8.0's and calls `.path_and_query()` (`:60`) on the inbound `Uri` to build
that string in the first place — so a client request to
`/v1/messages?beta=true` produces `path_and_query = "/v1/messages?beta=true"`,
which reaches `upstream_url_with_base` whole and is concatenated whole. Since
`request.rs`, `routes.rs`, and the caller in `gateway/mod.rs` (`:1279` also
calls `.path_and_query()`, likewise byte-identical) are all unchanged files,
this was already true at 0.8.0 and remains true at 0.8.2, now confirmed by
tracing the string from extraction to concatenation rather than reading the
concatenation function alone. **[byte-identical to 0.8.0]**

### A.5 — Hazard 4: `anthropic_auth_header` cleared on a layer-inconsistent base-URL change

**Unchanged.** `configuration/mod.rs` *did* change between 0.8.0 and 0.8.2 (a
123-line diff — new `NEMO_RELAY_TEST_SKIP_IMPLICIT_CONFIG_ENV`/`__skip-implicit-config`
test-only config-search-path override, and a new `HOOK_CLIENT_TOKEN_*`
HMAC-identity mechanism unrelated to upstream configuration), but the diff
touches nothing in `replace_upstream_base_url` or `validate_auth_header`. The
function itself, now at `nemo-relay-cli-0.8.2/src/configuration/mod.rs:1672-1681`
(shifted from 0.8.0's `:1621-1630` by the unrelated additions earlier in the
file), reads:

```rust
fn replace_upstream_base_url(
    base_url: &mut String,
    auth_header: &mut Option<String>,
    replacement: String,
) {
    if *base_url != replacement {
        *auth_header = None;
    }
    *base_url = replacement;
}
```

— textually identical body to the 0.8.0 citation. Both the CLI-flag call site
(`:1053-1056`, was `:1053-1055`ish at 0.8.0 lines cited generically) and the
env-var call site (`:1647-1657`) still route through it; `validate_auth_header`
(`:1683-1691`) is unchanged (trims, rejects empty, requires
`HeaderValue::from_str`; still no base-URL validation of any kind). The
top-level `--anthropic-base-url` flag's home files, `commands/serve.rs` and
`commands/run.rs`, are byte-identical to 0.8.0. So: a chained deployment must
still set `anthropic_base_url` and `anthropic_auth_header` in the same
config layer at 0.8.2, unchanged from the 0.8.0 finding.

### A.6 — Hazard 5: a plugin dispatch-override strips provider credentials before redirecting

**Unchanged, and now paired with a new (unrelated) credential-identity
helper.** `gateway/mod.rs` is byte-identical to 0.8.0: `INTERNAL_DISPATCH_URL_HEADER`
/ `INTERNAL_DISPATCH_ROUTE_HEADER` still at `:51-52`, `effective_dispatch_request`
still at `:874-908` clearing `TargetCredentialPolicy::ExplicitTarget` via
`remove_provider_credentials(&mut headers)` (`:888-899`), `dispatch_overrides`
still at `:965-984`. `provider_auth.rs` *did* change, but purely additively:
a new `pub(crate) fn identity(&self) -> String` on `TransparentProxyCredential`
and a new free function `credential_identity(value: &str) -> String` (SHA-256
hex digest, prefixed `sha256:`) — `provider_auth.rs:44-47,115-125` in 0.8.2 —
neither of which touches `consume`, `TRANSPARENT_PROXY_CREDENTIAL_ENV/HEADER`,
or `PROVIDER_API_KEY_HEADERS`, all unchanged at their 0.8.0 lines. This new
identity helper is consumed by `server/mod.rs`'s new `authorize_hook_request`
(§A.8) for hook-owner attribution, not by the dispatch-override path — the
credential the override strips is still stripped the same way. Also unchanged:
`codex/alignment.rs` (byte-identical — `strip_chatgpt_auth_for_openai_route`,
`is_openai_route`, and the "Anthropic routes use a different auth scheme"
comment all stand), and `agents/shared/alignment.rs`'s `gateway_upstream_url_override`
delegation (that file did change, but only to add `authenticated_owner` tracking
to `SessionAlias`/`PendingSubagentStart` for the same hook-authorization work
in §A.8 — the Codex-only redirect guard itself is untouched).

### A.7 — The `nemo-relay claude` subcommand, its injected surface, and the version gate

**Exists, unchanged in name and position.** `commands/root.rs`'s `Commands`
enum still carries `Claude(ClaudeCommand)` mapped to the string `"claude"`
(`root.rs:123` region, unchanged lines around the enum — the diff to this file
only adds two *new* sibling subcommands, `Gateway(GatewayCommand)` and
`Integrations(IntegrationsCommand)`, described below).

**Injected environment — every var, unchanged names and value shapes:**

| Var | Value shape | Citation | Status |
|---|---|---|---|
| `NEMO_RELAY_GATEWAY_URL` | gateway base URL string | `process/launcher.rs:490` (via `configuration::GATEWAY_URL_ENV`) | byte-identical file |
| `NEMO_RELAY_TRANSPARENT_RUN` | literal `"1"` | `process/launcher.rs:493` | byte-identical file |
| `PATH` | hook-dir-prepended `$PATH`, only when the current exe is resolvable | `process/launcher.rs:501` | byte-identical file |
| `NEMO_RELAY_PROXY_CREDENTIAL` | `nrp_` + 64 hex chars, secret | `provider_auth.rs:28-38` (`TransparentProxyCredential::generate`, unchanged), wired at `process/launcher.rs:484` | generator unchanged; call site unchanged |
| `ANTHROPIC_CUSTOM_HEADERS` | `x-nemo-relay-proxy-token: nrp_…`, merged not overwritten via `replace_custom_header`, secret | `agents/claude/launch.rs:19-31,113-127` | unchanged logic |
| `ANTHROPIC_BASE_URL` | gateway URL, set on `launch.env` **and** deep-merged into the synthesized Claude `--settings` JSON's `env` key (`settings_overlay`, `launch.rs:129-150`) | `agents/claude/launch.rs:46-48,93-95,145-148` | unchanged logic, same double-write |

**Injected argv — one real change.** At 0.8.0, `--plugin-dir <tmp> --settings
<tmp>/settings.json` were spliced as one contiguous block immediately after
the host token via `insert_after_host`. At 0.8.2
(`agents/claude/launch.rs:33-45,83-92,100-111`), the two flags are split:
`--plugin-dir <tmp>` still goes immediately after the host token via
`insert_after_host`, but `--settings <path>` is now placed by a **new**
function, `insert_before_argument_boundary`, which finds the first bare `--`
token after the host index (or the end of argv if none) and splices
`["--settings", path]` there instead:

```rust
fn insert_before_argument_boundary(
    argv: &mut Vec<String>,
    host_index: usize,
    values: impl IntoIterator<Item = String>,
) {
    let boundary = argv
        .iter()
        .skip(host_index + 1)
        .position(|argument| argument == "--")
        .map_or(argv.len(), |offset| host_index + 1 + offset);
    argv.splice(boundary..boundary, values);
}
```

Net effect: if the launch argv carries user-supplied flags between the host
token and a `--` separator (e.g. `nemo-relay claude --dry-run -- --resume`),
`--settings` now lands *after* those flags rather than immediately after
`--plugin-dir`, while `--plugin-dir` itself does not move. **Two readings,
stated both ways and stopped:** this could be read as a bug fix (some
Claude-Code-side flag ordering requirement `--plugin-dir` must satisfy that
`--settings` need not, or vice versa) or as an unrelated refactor with no
externally visible effect (both flags still land before any `--`-delimited
Claude-Code-native argv either way, so a well-formed launch's resulting
`ClaudeCode` invocation is unchanged; only a launch with unusual intervening
flags between the host token and `--` would see a different flag order). No
comment in `launch.rs` or `prepared.rs` explains the split, and CHANGELOG/git
history are not visible from this environment (§ sources note). Not one of
R7's five named hazards, but relevant to "every env var / flag it injects" and
worth a design read before M11.2's chained topology work assumes flag order.

**Version gate — unchanged.** `agents/claude/mod.rs` is byte-identical to
0.8.0 (`minimum_version: (2, 1, 121)`, `claude/mod.rs:21`). The check itself,
`AgentDescriptor::validate_version_output` (`agents/mod.rs:87-105` — this
region of `agents/mod.rs` is unchanged even though the file overall differs;
the diff is confined to a new `include_local_install` parameter on
`installed_integrations`, unrelated to version gating), still: takes the first
line of `claude --version` output, strips via `claude::parse_version` (which
itself strips the `" (Claude Code)"` suffix — `claude/mod.rs:40+`, unchanged),
rejects if `version < minimum_version()` **or** `!version.pre.is_empty()`
(pre-releases rejected outright, `:97`). Codex's gate
(`codex/mod.rs:22`, `(0, 143, 0)`) is likewise byte-identical.

**Config keys that aim Relay's Anthropic upstream — unchanged, same three
layers.** `[upstream] anthropic_base_url` / `anthropic_auth_header` in
`config.toml` (`configuration/mod.rs:76-77` struct fields, `:1349-1358`
application — file changed elsewhere but not here, see §A.5); env
`NEMO_RELAY_ANTHROPIC_BASE_URL` / `NEMO_RELAY_ANTHROPIC_AUTH_HEADER`
(`:1647-1657`); CLI flag `--anthropic-base-url` on both `ServerArgs`
(`commands/serve.rs`, byte-identical file) and `run` (`commands/run.rs`,
byte-identical file). `nemo-relay config edit`'s upstream-editing menu lives
in `commands/configure/editor/prompt.rs`, not in the 34-file diff list, so
unchanged.

**New at 0.8.2, adjacent to but not overlapping this surface:** two new
subcommands, `nemo-relay gateway` (`commands/gateway.rs`, new file —
"Manage the persistent shared Relay gateway") and `nemo-relay integrations`
(`commands/integrations.rs`, new file — "Refresh Relay-managed coding-agent
integrations after upgrading Relay"), both wired into `root.rs`'s `Commands`
enum alongside the unchanged `claude`/`codex`/`config`/etc. Neither touches
the ephemeral `nemo-relay claude` launch path read above.

### A.8 — Unrelated but load-bearing-looking: a new hook-request authorization gate

Not asked for, but surfaced by the `server/mod.rs` diff and worth flagging
because it changes what a chained deployment's hook traffic must present.
0.8.2 adds `AppState::authorize_hook_request` (`server/mod.rs:583-632`),
called from both `codex_hook` and `claude_code_hook` before payload
processing. It rejects any hook request carrying an `Origin` header outright
("browser-originated Relay hook requests are not accepted"), then requires
either the transparent proxy credential header or a `BOOTSTRAP_CLIENT_TOKEN_HEADER`
HMAC-verified via a **new** `HOOK_CLIENT_TOKEN_HEADER` / `hook_client_token`
mechanism (`configuration/mod.rs:564-574`, new `HmacKey` methods) to resolve a
stable `owner` identity string, which then flows into a new
`apply_authenticated_events`/`authorize_tool_permission` path and a new
`PermissionRequest` hook-event decision (allow/deny with reason) that 0.8.0
did not have. This is orthogonal to R7 (it is about *which agent process* may
call the hook endpoints, not about the Anthropic gateway route), but a
chained-topology design that spawns `nemo-relay claude` and expects the same
hook wire-shape as 0.8.0 should know the hook handlers now gate on identity
and can return a `deny` decision Claude Code's `PermissionRequest` hook must
handle. Evidence only — whether this affects the chained topology's guard
tests is a design question, not settled here.

### A.9 — `nemo-relay-types` 0.8.2 and the `=0.7.3` pin's unlock condition

**The unlock condition is not met, and the gap to 0.8.2 is now stronger
evidence of zero-cost than the 0.8.0 finding was.** `nemo-relay-types-0.8.2/Cargo.toml`
(the generated, post-normalization file) requires:

```toml
[dependencies.uuid]
version = "=1.18.1"
features = ["v7", "serde"]

[dependencies.chrono]
version = "0.4"
features = ["std", "serde", "now"]
default-features = false
```

— `uuid = "=1.18.1"` **exact**, unchanged from both 0.7.3 and 0.8.0.
`chrono = "0.4"` caret, unchanged. **`nemo-relay-types` declares no `tokio`
dependency at all, at either 0.8.0 or 0.8.2** — `grep -n tokio
nemo-relay-types-0.8.2/Cargo.toml.orig nemo-relay-types-0.8.2/Cargo.toml`
returns zero hits (exit 1) on both files. (Roundhouse's own `tokio` req is
independent of this crate; the pin comment's uuid-ceiling discussion is the
relevant one here, not a tokio one — this crate simply never asks for tokio.)

**Stronger than "byte-identical fields":** `diff -ru nemo-relay-types-0.8.0/src
nemo-relay-types-0.8.2/src` is empty — **the entire `src/` tree is
byte-identical**, not just the seven items roundhouse imports. `Cargo.toml.orig`
differs by exactly one line (`version = "0.8.0"` → `"0.8.2"`); no dependency,
no feature, no test target changed. So the roundhouse pin comment's framing
("cost of moving: zero code change, zero new transitive dependency") is, if
anything, more strongly supported at 0.8.2 than it was at 0.8.0 — there is
categorically nothing in the diff for roundhouse's seven call sites
(`atof.rs:63-65,138,165,207`, `wire.rs:22`, `summary.rs:71-77`) to react to,
because there is no diff in the crate at all. **The unlock condition itself —
"a `nemo-relay-types` release that relaxes its own `=1.18.1` to a caret" —
remains unmet.** `nemo-relay` (core)'s own dependency on `nemo-relay-types` is
still a caret (`nemo-relay-0.8.2/Cargo.toml:143-144`: `version = "0.8.2"`, no
`=`), so the exact pin is a `nemo-relay-types`-crate decision, not something
downstream crates in the family impose.

One count against the pin comment's now-stale text: it currently reads (in
`crates/roundhouse-relay/Cargo.toml:57-62`) that the 2026-08-27 re-read
verified 0.8.0's byte-identity and "the move stays zero-cost, the ceiling
stays, and the trigger above is unchanged" — that sentence is still accurate
as written (it was a statement about 0.8.0, and remains true of 0.8.0), but a
reader in 2026-09 would benefit from a dated note that 0.8.2 was independently
checked and found the same, per this addendum, since the manifest's own
2026-08-27 addendum only speaks to 0.8.0. Not fixed here — CLAUDE.md
prohibits modifying source under `crates/` from a research-evidence task; this
is a ruling document's job.

### A.10 — What else moved that R7/R8 should know about (surprises)

Evidence only, unranked:

1. **A loopback bind requirement is now enforced.** `server/mod.rs:92-97`
   (new): `run_server` now returns `CliError::Config` if `config.bind` is not
   a loopback address, for "explicit Relay gateways". Relevant to any chained
   deployment that was binding the gateway on a non-loopback interface
   (e.g. a container-network address) to let roundhouse reach it as a
   downstream client on a different host/container.
2. **ATOF file-sink output now writes through `private_file`'s restrictive
   permissions**, not raw `std::fs::OpenOptions` — new module
   `observability/private_file.rs` (and `confined_fs.rs`), wired into
   `atof.rs`'s `open_file`/`create_dir_all` call sites. `ATOF_VERSION` remains
   `"0.1"` (`nemo-relay-types-0.8.2/src/api/event.rs:36`, unchanged) and
   `ATIF_SCHEMA_VERSION` remains `"ATIF-v1.7"` (`nemo-relay-0.8.2/src/observability/atif.rs:55`,
   unchanged) — the wire schema this crate emits is unaffected; only how the
   bytes hit disk changed.
3. **`atif.rs`'s per-turn LLM-request stash changed shape** (`PendingAgentStep`
   gained an `llm_request: Option<Json>` finalize parameter and a
   `pending_llm_requests: HashMap<Uuid, Json>` replacing a single
   `stash_llm_request` call) — internal to ATIF trajectory assembly for
   concurrent/subagent turns; the *emitted* struct set (`AtifStep`,
   `AtifTrajectory`, etc.) is unchanged in the diff, so this reads as an
   internal correctness fix for interleaved requests rather than a schema
   change. Not independently verified beyond the diff read.
4. **New Cargo features gated `__`-prefixed** (`__skip-implicit-config`,
   `__test-cli-port-override`) are test-only surface (config-search-path and
   port-override escape hatches), not part of the public CLI/config contract;
   named here only so a reader of the Cargo.toml diff does not mistake them
   for user-facing knobs.
5. **`nemo-relay-switchyard` re-confirmed dead**, independent of the 0.8.0
   read's crates.io-API-based finding: `static.crates.io` answers 403 (this
   CDN's not-found status, corroborated against known-absent version probes)
   for `nemo-relay-switchyard-0.8.0.crate` through `-0.8.2.crate` and
   `-0.9.0.crate`, while `-0.7.3.crate` answers 200. Same conclusion as §5.4,
   reached without the registry API.

### A.11 — Summary against R7's guard list

| R7 item | 0.8.0 finding | 0.8.2 status |
|---|---|---|
| 1. Alphabetizing `Map` re-encode | Present, `serde_json` no `preserve_order` | **Unchanged** (§A.2) |
| 2. SSE re-encoder drops `id:` | Present | **Unchanged** (§A.3) |
| 3. `?beta=true` survival | Survives (no query handling) | **Unchanged, now traced end-to-end** (§A.4) |
| 4. Auth header cleared on layer-inconsistent base change | Present | **Unchanged, function body reproduced verbatim** (§A.5) |
| 5. Dispatch-override strips credentials | Present | **Unchanged**; new adjacent identity helper does not touch it (§A.6) |
| 6. S3 originals (routing, attribution, one log) | Roundhouse-side, not Relay-side | Out of scope for this crate re-read |

None of the five Relay-side hazards R7 names changed between 0.8.0 and 0.8.2.
The new material this addendum surfaces — the hook-authorization gate (§A.8),
the loopback-bind requirement (§A.10.1), and the `--settings` argv-placement
change (§A.7) — are not among R7's five, but each is close enough to the
chained-topology surface that M11.2's guard-test design should read them
before assuming 0.8.0's launch/hook mechanics apply unmodified.
   the standing dependency rule already excludes it.
### A.12 — Observed, not read: what the gateway adds to the dispatched request (2026-09-01)

§A.7 enumerates what `nemo-relay claude` injects into the *client* (env,
argv). M11.2b's chained e2e suite recorded, at roundhouse's edge, what the
gateway adds to the request it *dispatches* — eight headers no Direct
capture carries: `traceparent`, `x-nemo-relay-agent-kind: claude-code`,
`x-nemo-relay-identity-quality: native`, `x-nemo-relay-parent-scope-id`,
`x-nemo-relay-request-id` (`relay-request-<uuid>`),
`x-nemo-relay-root-scope-id`, `x-nemo-relay-session-id` (equal to the
client's `x-claude-code-session-id`), `x-nemo-relay-source: gateway`,
`x-nemo-relay-turn-id` (`1` on a first turn). Observed on every chained turn
of claude 2.1.257 through nemo-relay 0.8.2 (`tests/claude_e2e.rs`, which now
uses `x-nemo-relay-source` as its proof of hop). Not traced to source lines
here; a design read that wants to rely on any of them should find the
emitter first. Two readings, stated and stopped: `session-id`/`turn-id` are
a correlation key roundhouse could adopt for the chained topology, or they
are observability metadata whose stability across Relay releases nobody has
promised.

### A.13 — Read during M11.2b's core stage: the gateway's inbound-credential rules (2026-09-01)

Two 0.8.2 facts §A.5–A.7 did not ask about, read from the same tarballs
while the chained carrier was being designed, and load-bearing for R-D′ in
`../PLAN-anthropic-messages.md`:

1. **A configured `[upstream] anthropic_auth_header` is injected only when
   the inbound request carries no credential.** `gateway/mod.rs:1070-1078`
   (the `already_authed` short-circuit) checks for any of `authorization`,
   `x-api-key`, `api-key`, `anthropic-api-key` on the inbound request and
   skips the upstream header when one is present. Consequence: a client that
   presents any credential of its own — an API key, a sentinel, a
   subscription bearer — never receives Relay's configured upstream
   credential; the two carriers are mutually exclusive per request.
2. **Inbound `x-api-key` is forwarded untouched** (`gateway/response.rs:59-72`,
   `should_forward_request_header`), and unknown request headers are not
   stripped — which is what lets a client-environment `x-roundhouse-key`
   survive the hop. Relay's own `x-nemo-relay-proxy-token` is consumed at the
   gateway (`provider_auth.rs`, `TRANSPARENT_PROXY_CREDENTIAL_HEADER`) and
   does not reach the upstream; M11.2b's chained suite asserts that absence
   at roundhouse's edge.

Two readings, stated and stopped: (1) is a deliberate "bring your own
credential wins" policy, or an accident of the order in which the gateway
resolves headers; nothing in the source comments says which. Either way the
reference chained wiring in `crates/roundhouse-server/src/claude_launch.rs`
relies on it only in the direction the code guarantees today (a
credential-bearing client is left alone).

### A.14 — Observed during M11.3: `--agent codex` splices a `--config model_provider=` override (2026-09-02)

`nemo-relay run --agent codex --config <toml> --dry-run` at 0.8.2 reports an
argv that appends `--config model_provider="nemo-relay-openai"` plus Relay's
own `model_providers` table to the codex command line. Codex resolves a
`--config` override above its `config.toml`, so a generated
`config.toml` naming roundhouse's provider (the `codex_launch` output) is
outranked for the provider selection, and the client presents whatever
credential Relay's provider stanza implies — not the turn key the generated
config placed on the dedicated header. Observed from the dry-run plan only;
not traced to source lines here, and not exercised against a real codex
binary (none on this box). Two readings, stated and stopped: this is the
codex half of the "Relay owns the client's provider" design and an
`[upstream] openai_auth_header` is the intended carrier for a downstream key,
or it is an oversight that a generated config cannot be honoured. Either way
`topham relay` for Codex is a documented limit in M11.3.
