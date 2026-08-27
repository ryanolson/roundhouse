<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# DIVE D — Roundhouse seam map for `anthropic_messages` + `/v1/messages` + a launcher

> **Independently fact-checked 2026-08-27**: 16 load-bearing claims re-derived
> against the same working tree by a separate agent; all 16 confirmed (two
> one-line citation drifts corrected below). Two externally-facing facts the
> first draft asserted without citation were verified by the checker and now
> carry theirs: Anthropic's Messages request body has no session/cache-key
> field (https://platform.claude.com/docs/en/api/messages, accessed
> 2026-08-27) and Anthropic publishes no local tokenizer (WebSearch,
> 2026-08-27 — only community reverse-engineered approximations exist). One
> material addition outside this dive's original scope is folded into §5 (the
> `roundhouse-relay` emission boundary documents the same cache-write gap).

**Source:** `/home/user/roundhouse`, working tree at `git rev-parse HEAD` =
`306e6e0d8be7f643c0fa63910cfd876cff658d6b`, branch
`claude/anthropic-messages-api-wpc17p`, tree clean (`git status --porcelain`
empty). Head commit: "M10.0–M10.2: the text steer, providers and fair use, the
selection brain (#13)".
**Read date:** 2026-08-27. **Method:** local read only — `rg`, `sed`, `git`. No
cargo build or test was run (per brief). Every `file:line` below is against that
revision.
**Secondary pins named by the tree and not re-verified here:** codex crates at
`6344a655a5966f92e009a74928fb0559b41f9093`
(`crates/roundhouse-server/Cargo.toml:99-102`), the box binary `codex-cli
0.146.0` @ `e363b08` (cited throughout `codex_launch.rs`), Switchyard `5341f71`,
Dynamo `ac7b7513790ef1d619b46f805aea03c9f21200ba` (`Cargo.toml:39-42`).

This is evidence, not a ruling. Where a design decision could go two ways the
trade is stated both ways and stopped.

---

## 0. One-paragraph orientation

Six HTTP surfaces are merged over one log at
`crates/roundhouse-server/src/main.rs:871-917`; the module map is
`crates/roundhouse-server/src/lib.rs:58-93`. On the dispatch side there is
exactly one `FrontierClient` implementation that reaches a network
(`OpenAiResponsesClient`), plus `EchoFrontierClient`
(`crates/roundhouse-fleet/src/frontier.rs:515-538`). On the serve side there is
exactly one model-facing dialect (`/v1/responses`) plus the native session
transport. The vocabulary for a second frontier dialect
(`WireProtocol::AnthropicMessages`) and for a second provider route
(`ProviderRoutes::messages`) already exists and is already exercised by config
validation and tests — what does not exist is any client, any serve surface, and
any operator entry point.

---

## 1. THE DISPATCH SIDE (client)

### 1.1 The trait and its three value objects

`crates/roundhouse-fleet/src/frontier.rs`:

- `FrontierClient` (`:416-419`) is one method: `async fn execute(&self, quote:
  &FrontierQuote) -> Result<FrontierStream, FrontierError>`. Object-safe on
  purpose (`:172-177`).
- `FrontierQuote` (`:263-305`) is **the only argument** a client receives. Its
  fields: `target: Target`, `wire_protocol: WireProtocol`, `prompt: String`,
  `prompt_cache_key: String`, `expected_output_tokens: Option<u32>`,
  `credential: TurnCredential`. The doc at `:266-275` states the reason the
  dialect rides here: `Target` keys on `(provider, model)` only, and one model
  over two dialects is an ordinary deployment.
- `FrontierChunk` (`:180-225`) has two variants: `OutputText(String)` and
  `Done { input_tokens, cached_input_tokens, output_tokens, reasoning_tokens,
  provider_reported_cost: Option<f64> }`. **There is no cache-write field.**
- `FrontierStream` = `BoxStream<'static, Result<FrontierChunk, FrontierError>>`
  (`:177`).
- `FrontierError` (`:307-373`): `UnknownProvider`, `Upstream`, `Credential`,
  `UnsupportedDialect { expected, got, target }`, `Transport { message,
  timed_out }`, `Status { status, message }`.
- `FrontierError::failover_class` (`:391-405`) is the whole failover trigger:
  `Transport` → `Timeout`/`Transport`, retryable `Status` →
  `AttemptClass::Status`, everything else `None`. `UnsupportedDialect` is
  explicitly non-retryable — and the existing unit test at `:594-598` already
  uses `expected: "openai_responses", got: "anthropic_messages"` as its fixture.
- `FrontierClients` (`:452-512`) is the per-provider registry: `uniform` (one
  transport answers every name) or `keyed` (map). `for_provider` (`:494-502`).

`FrontierModelSpec` (`:33-51`) is the catalog row: `provider`, `model`,
`wire_protocol`, `cache_model`, `pricing`, `quality_prior`, `base_ttft_ms`,
`ttft_ms_per_uncached_token`. `spec_for` (`:125-132`) is the `Target` →
spec lookup, sound only because the catalog boundary rejects duplicate
`(provider, model)`.

### 1.2 `WireProtocol` and every exhaustive match a new client wiring visits

`crates/roundhouse-fleet/src/usage.rs:50-84` defines three variants with pinned
serde names (`:61-83`): `openai_chat_completions`, `openai_responses`,
`anthropic_messages`. **`AnthropicMessages` already exists as vocabulary.**

Workspace-wide `match` sites that are exhaustive on `WireProtocol` (a fourth
dialect would break these; a *client* for an existing variant breaks none of
them, which is the point):

| site | what it decides |
|---|---|
| `roundhouse-fleet/src/usage.rs:102-109` | `enforce_usage_reporting` — `AnthropicMessages` already returns `Vec::new()` |
| `roundhouse-fleet/src/usage.rs:120-124` | `wire_name` — already returns `"anthropic_messages"` |
| `roundhouse-server/src/catalog_config/providers.rs:120-126` | `ProviderRoutes::for_dialect` — already maps `AnthropicMessages → self.messages` |
| `roundhouse-server/src/catalog_config/providers.rs:133-139` | `ProviderRoutes::field_for` — already returns `"messages"` |

`reports_usage_before_completion` (`usage.rs:131-134`) is a `matches!`, not a
`match`, and already answers `true` for `AnthropicMessages` only — with the doc
at `:74-83` stating the split-usage fact an Anthropic client must honour
(input/cache-read/cache-write on `message_start`, output on the final
`message_delta`) and naming the failure mode of reading only one.

**The non-exhaustive gate that decides everything:**
`crates/roundhouse-server/src/main.rs:233-243` bails at boot if
`spec.wire_protocol != WireProtocol::OpenAiResponses`. That is a hard equality
against a single constant, not a match — the compiler will not point at it when
a second client lands. `main.rs:189-195` similarly rejects any
`ROUNDHOUSE_FRONTIER_UPSTREAM` value other than the literal `"openai_responses"`.

### 1.3 `openai_responses.rs` as the template, end to end

`crates/roundhouse-fleet/src/openai_responses.rs` (725 lines, plus
`openai_responses/stream.rs`, 479 lines):

- Constants: `DEFAULT_API_BASE` (`:76`), `DEFAULT_PASS_THROUGH_BASE` (`:86`),
  `DEFAULT_RESPONSES_PATH` (`:97`), and `const SPOKEN: WireProtocol` (`:101`) —
  the dialect the client refuses to deviate from.
- Struct (`:109-132`): **two** `reqwest::Client`s (`direct`, `forwarding` with
  `redirect::Policy::none()`), `api_base`, `pass_through_base`,
  `responses_path`, `extra_headers: HeaderMap`.
- Builders: `with_bases` (`:150-173`), `with_responses_path` (`:182-185`),
  `with_extra_headers` (`:200-217`, fallible so a bad header is a boot refusal).
- **Request build** is a static `fn body(quote, model) -> Value` (`:229-249`),
  deliberately separated from `execute` so it is assertable without a socket. It
  emits exactly six fields (`model`, `stream`, `input`, `prompt_cache_key`,
  `max_output_tokens`, `store: false`) and calls
  `quote.wire_protocol.enforce_usage_reporting(&mut body)` (`:247`) even though
  it adds nothing on this dialect.
- **Auth routing** is one function, `route()` (`:257-321`), returning a `Route`
  (`:325-340`) that bundles client + base + headers + the whole `TurnCredential`.
  Stored → `Bearer` from `require_api_key`, applied *after* `extra_headers` so a
  config file can never displace `Authorization` (`:271-279`). Forwarded → the
  allowlisted headers verbatim on the redirect-disabled client (`:287-313`).
  Absent → refused before a socket (`:316-320`).
- **`execute`** (`:344-397`): destructure `Target::Frontier`, compare
  `quote.wire_protocol != SPOKEN` → `UnsupportedDialect` (`:351-357`), POST
  `format!("{}{}", route.base, self.responses_path)`, classify transport errors
  with `is_timeout` (`:377-380`), non-2xx → `Status` with
  `route.credential.redact(body)` (`:384-390`), success → `decode(...)`.
- **Error redaction** is exhaustive by variant (`redact_error`, `:461-473`), so a
  new error arm cannot silently join the set that carries an upstream body.
- `sensitive()` (`:483-487`) marks header values so hyper will not print them;
  `trim_base()` (`:491-493`).

### 1.4 The SSE decoder (`openai_responses/stream.rs`)

- `MAX_EVENT_BYTES = 1 << 20` (`:40`) bounds an unterminated stream.
- `SseDecoder { buffer: String, pending: VecDeque<FrontierChunk>, finished: bool }`
  (`:45-55`); `feed(&[u8])` uses `from_utf8_lossy` so a chunk boundary
  mid-codepoint is not a failure (`:77-83`); `eof()` decodes a final event with
  no blank line (`:87-93`); `drain()` splits on `"\n\n"` (`:96-117`).
- `decode_event` (`:121-145`) joins every `data:` line, skips comments and
  non-`data` fields, and tolerates the `[DONE]` sentinel.
- **Dispatch is on the JSON payload's own `type`, never the `event:` line**
  (module doc `:10-15`; `dispatch` `:147-192`). Four handled types:
  `response.output_text.delta`, `response.completed`, `response.failed`,
  `error`; everything else is skipped.
- `usage_chunk` (`:201-221`) reads `input_tokens`,
  `input_tokens_details.cached_tokens`, `output_tokens`,
  `output_tokens_details.reasoning_tokens`, and the OpenRouter-only `cost`.
- **A stream that never completes yields no `Done`** (module doc `:16-22`) — the
  engine then substitutes `estimated_usage` and marks it. An Anthropic client
  folding two usage events into one `Done` must preserve this property: emitting
  a zero-token `Done` reads as a saving.

### 1.5 The provider registry — `routes.messages` already exists

`crates/roundhouse-server/src/catalog_config/providers.rs`:
`BUILT_IN_OPENAI = "openai"` (`:47`); `ProviderConfig { base_url, routes, auth,
extra_headers }` with `deny_unknown_fields` (`:56-91`); `ProviderRoutes { models,
chat_completions, responses, messages }` (`:100-110`) — **all four optional, and
`messages` is already a field**; `ProviderAuth { env }` (`:143-148`);
`validate()` (`:158-204`) checks scheme, leading slash on every route including
`"messages"` (`:169-189`), and env-var plausibility.

`crates/roundhouse-server/src/catalog_config.rs:474-497` is the boundary
cross-check: an entry naming an undefined provider is refused
(`UndefinedProvider`), and an entry whose `wire_protocol` has no route on its
provider is refused (`ProviderMissingRoute`). The existing unit test at
`catalog_config.rs:815-846` uses `dialect == "anthropic_messages" && field ==
"messages"` as its fixture — so the messages route is already load-bearing in
config validation with no client behind it.

`examples/catalog.example.json:53-57` ships a real `anthropic` provider stanza
(`base_url: https://api.anthropic.com/v1`, `routes: { models: "/models",
messages: "/messages" }`, `auth.env: ANTHROPIC_API_KEY`) and the file's own
`$comment` (lines 20-31) says explicitly that no `models` entry names it because
"Neither dialect has a client in today's build". `crates/roundhouse-server/tests/example_catalog.rs`
asserts the shipped example still parses and validates — so adding an
`anthropic` **models** entry to the example before a client exists would make
the example un-bootable, which that file's comment already anticipates.

### 1.6 How clients are built (`main.rs`)

`frontier_clients(catalog, providers, env)` at
`crates/roundhouse-server/src/main.rs:170-346`:

1. Unset `ROUNDHOUSE_FRONTIER_UPSTREAM` (`:114`) → `FrontierClients::uniform(EchoFrontierClient)` (`:175-188`).
2. Any value other than `"openai_responses"` → `anyhow::bail!` (`:189-195`).
3. Per provider: undefined and not `openai` → bail (`:216-226`); **any entry whose
   dialect is not `OpenAiResponses` → bail** (`:232-243`).
4. With a definition: read `routes.for_dialect(OpenAiResponses)`, build
   `OpenAiResponsesClient::with_bases(base, base).with_responses_path(route)
   .with_extra_headers(...)` (`:245-320`); warn if the named `auth.env` is unset
   (`:302-311`); warn if an explicit `openai` definition shadows
   `ROUNDHOUSE_OPENAI_API_BASE` / `ROUNDHOUSE_OPENAI_PASS_THROUGH_BASE`
   (`:280-295`).
5. Without a definition (implicit `openai`): the two env vars, defaulting to
   `DEFAULT_API_BASE` / `DEFAULT_PASS_THROUGH_BASE` (`:325-341`).

The judge gets its own transport by name at `main.rs:767-782` — a second dialect
must keep `judge_client` resolving through `for_provider`, or the validate loop
silently bills a different provider.

### 1.7 Pass-through auth and the forwarded-seat accounting

`crates/roundhouse-core/src/control/credential/forwarded.rs`:

- **`ALLOWLIST` (`:58-61`) has exactly one row: `openai` → `["authorization",
  "chatgpt-account-id", "x-openai-fedramp"]`.** The comment immediately above
  (`:53-57`) states there is deliberately no Anthropic row, that Switchyard's
  would be `(authorization, x-api-key)`, and that "roundhouse's only
  pass-through client speaks the Responses wire… a row nothing exercises is a
  promise made to whoever reads the table". The unit test at `:285-296` uses
  `("x-api-key", "sk-ant-not-ours")` as the *negative* control that a
  non-allowlisted header never reaches the wire.
- `CREDENTIAL_HEADER = "authorization"` (`:89`) — a capture with no
  `Authorization` is not a credential at all (`captured`, `:132-144`).
- `PresentedCredential::for_provider` (`:161-175`) is the only constructor of
  `ForwardedCredential`; `covers()` (`:154-156`) is the cheap candidate filter.
- `ForwardedCredential::headers()` (`:196-200`) is the one plaintext seam;
  `redact()` (`:222-226`).

`crates/roundhouse-core/src/control/credential/access.rs:322-350`: `Forwarding`
resolution yields `TurnCredential::Forwarded` with `payer: Payer::User`;
`reachable()` (`:379-401`) drops candidates whose provider is unreachable and
records `withheld_providers`.

Edge capture: `ControlPlane::turn_admission`
(`crates/roundhouse-server/src/control_config/mod.rs:882-899`) captures the
caller's headers **only when the turn key arrived in `TURN_KEY_HEADER`**
(`= "x-roundhouse-key"`, `:177`) and only if the value does not itself carry a
roundhouse secret (`carries_a_roundhouse_secret`, `:401-404`). `presented_key`
(`:900-…`) reads exactly two headers: `TURN_KEY_HEADER` then `Authorization`.
**Nothing anywhere in `crates/` reads `x-api-key`** — the only two occurrences in
`.rs` files workspace-wide are the doc comment and the negative test in
`forwarded.rs` (verified by `rg 'x-api-key' --include=*.rs` over `/home/user/roundhouse`).

Seat accounting: `seat_tokens` is a dollar-free column carried through
`crates/roundhouse-core/src/metrics/snapshot.rs:154,184,267-270,295,309,439,581-634,699`
and surfaced on the wire at `crates/roundhouse-server/src/metrics_api.rs:327` and
in the reconciliation view at
`crates/roundhouse-server/src/admin_api/reconciliation.rs:239,284,320,548,575`.
An Anthropic-subscription-seat analog touches: one `ALLOWLIST` row, the
provider-side base-URL split (`api_base` vs `pass_through_base`), and nothing
else in metrics — the seat column is keyed on `Payer`, not on dialect.

---

## 2. THE SERVE SIDE

### 2.1 What `/v1/responses` actually does

`crates/roundhouse-server/src/responses_api.rs`:

- Route: `format!("{API_PREFIX}/responses")` with `API_PREFIX = "/v1"` (`:125`,
  `:157-161`). State is `Compat { engine, store, planes: Arc<dyn PlaneSource>,
  conversations }` (`:78-95`).
- `ResponsesRequest` (`:190-211`) reads only `instructions`, `model`, `input`,
  `stream`, `prompt_cache_key`. Everything else is accepted and ignored.
- `create_response` (`:218-331`) order: `plane.turn_admission(&headers)` →
  `refuse_over_fair_use` → `parse_body` → require `stream: true` (`:248-252`) →
  require non-empty `prompt_cache_key` (`:257-263`) → `canonicalize` →
  `turn_id_for` → `admitted_input_tokens` → `bind` → `last_seq` → spawn
  `run_turn` → return an SSE stream tailing the log.
- **Prefix admission**: `bind` (`:347-378`) reads the session's committed items
  (`stored_items`, `:419-436`), computes `suffix_after` (`:446-453`) using
  `same_item` (role + content, never the response stamp, `:461-463`); a
  disagreement forks to a new generation (`:375-377`).
- **Turn id = content hash**: `turn_id_for` (`wire.rs:191-204`) is FNV-1a over
  `Item::render()` of the whole canonicalized conversation, with the constants
  written out (`wire.rs:215-216`) because `DefaultHasher` is not stable across
  releases. Pinned by a literal at `wire.rs:516-529`.
- **SSE emission from the log**: `ResponsesFollower` (`:504-539`) has three
  phases (`Phase`, `:473-483`), `concerns()` (`:639-665`) claims events by
  identity, `emitted()` (`:705-716`) is the single predicate deciding what goes
  out, `project()` (`:719-857`) turns one log entry into frames.
- **Terminal semantics**: `Step::End` (`:492-495`) — nothing may follow the
  terminal frame in either direction.

### 2.2 The internal item vocabulary (what a Messages surface must map onto)

`crates/roundhouse-core/src/item.rs`:

- `Role` (`:17-23`): `System`, `Developer`, `User`, `Assistant`, `Tool`.
- `ItemContent` (`:44-57`): **three variants only** — `Text { text }`,
  `ToolCall { call_id, name, arguments }`, `ToolResult { call_id, output }`.
  There is no image, no document, no thinking/redacted-thinking variant. The doc
  at `:38-41` says images and audio "slot in as further variants".
- `Item { role, content, response_id: Option<ResponseId> }` (`:84-107`).
  `response_id` is the provenance stamp; client input always canonicalizes to
  `None`.
- `Item::render()` (`:166-168`) = `format!("<|{role}|>{content}")`, and
  `ItemContent::render()` (`:67-79`) flattens tool calls to
  `<tool_call id=… name=…>args</tool_call>`. This is the *only* prompt encoding;
  a frontier client receives it as one flat string.
- `spoken_text()` (`:184-189`) returns `""` for anything not `Text`.

Mapping direction on the way in is `canonical_item`
(`responses_api/wire.rs:54-112`): `message` (with role mapping, `:66-77`),
`function_call`, `function_call_output`, `reasoning` → dropped (`:107`),
anything else → 422 naming the type (`:108-110`). `message_text` (`:120-150`)
concatenates `input_text`/`output_text` parts into one item and refuses any
other part type. `output_text` (`:159-167`) keeps a structured tool output as its
own JSON encoding.

### 2.3 Session naming — what the serve surface keys on today

`prompt_cache_key` is the client's own session name (`responses_api.rs:257-263`,
module doc `:13-22`), qualified into the caller's namespace by
`Compat::namespaced_key` → `ControlPlane::qualify`
(`responses_api.rs:397-404`), then resolved to a `SessionId` by
`Conversations::bind` (`crates/roundhouse-server/src/conversations.rs:84-90`).
The binding table is node-local `HashMap` behind a `Mutex` and carries a
generation counter keyed on the whole namespaced string
(`conversations.rs:56-71`); a fork appends `#g{n}`.

The **other** precedent in the tree is the native transport, which takes the
session id in the path: `/v1/sessions/{session_id}/responses`
(`crates/roundhouse-server/src/http.rs:131-140`), with namespace enforcement by
`in_namespace` (`http.rs:418-430`).

So: a Messages surface has two existing, working precedents to choose between —
a client-chosen name resolved through `Conversations` (what `/v1/responses`
does) or a server-issued id in the path (what `/v1/sessions/...` does).
Anthropic's Messages request body has no `prompt_cache_key` field. *Trade both
ways:* keying on a header/metadata field the client can be configured to send
preserves the one-client-session ↔ one-roundhouse-session property and the warm
prefix, at the cost of requiring client configuration; deriving a key from the
conversation content itself (e.g. hashing the first system+user item) needs no
client cooperation but re-derives on every edit and would fork exactly when a
long session is compacted — which is when the warm prefix is worth the most.
Nothing in the tree decides this.

### 2.4 Usage projection back onto the wire

`responses_api/wire.rs:298-320` (`completed_frame`) emits
`input_tokens`, `input_tokens_details.cached_tokens`,
`input_tokens_details.cache_write_tokens` **hardcoded to `0`** (`:309`, with the
doc at `:291-293` saying "no provider Roundhouse routes to reports it separately
yet"), `output_tokens`, `output_tokens_details.reasoning_tokens`, and
`total_tokens` from `Usage::total()`.

`Usage` (`crates/roundhouse-core/src/event.rs:31-58`) carries
`input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_tokens`,
`accounting`. **There is no cache-write field on `Usage` and none on
`FrontierChunk::Done`** — so an Anthropic provider's `cache_creation_input_tokens`
has nowhere to land today without widening a durable serde shape.

The wire's usage is *not* always the log's: `ResponsesFollower.context_contribution`
(`responses_api.rs:523,834-846,755`) substitutes
`Engine::context_contribution` (`engine.rs:1429-1441`) when the turn was answered
at the interjection seam.

### 2.5 Error mapping

Log vocabulary → wire: `IncompleteReason`
(`crates/roundhouse-core/src/event.rs:146-186`) has six variants; `/v1/responses`
translates `PolicyRefused` and `BudgetExhausted` into `response.failed` with two
distinct English messages (`responses_api.rs:775-796`) and forwards everything
else as `response.incomplete` (`:797-808`). Pre-stream failures use `ApiError`
(`http.rs:207-…`), whose `IntoResponse` (`:432-448`) emits
`{"error": {"code", "message", …detail}}`; the fair-use 429 additionally carries
`type: "usage_limit_reached"` and `resets_at` because that is the only
machine-readable 429 codex recognizes (`http.rs:497-531`).

### 2.6 Where a second dialect mounts

`main.rs:871-917` merges six routers, each built by a `*_router(...)` free
function taking `Arc<P: PlaneSource>` plus its own dependencies. The Responses
router (`responses_api.rs:145-168`) is the closest template: four arguments
(`planes`, `engine`, `store`, `conversations`), one `.route()`, one
`.with_state()`. What is `pub(crate)` in `http.rs` and shared by a second
transport is stated at `http.rs:25-28`: `ApiError`, `LogTail` (`:756`),
`POLL_INTERVAL` (`:64`), `READ_BATCH` (`:67`), `parse_body` (`:545`),
`store_error` (`:533`), `refuse_over_fair_use` (`:468`), `in_namespace` (`:418`).

### 2.7 `ClientDialect` — the *client*-facing dialect enum, distinct from `WireProtocol`

`crates/roundhouse-server/src/dialect.rs:79-89` — one variant,
`CodexResponses { namespace }`. Its module doc (`:22-37`) already anticipates a
flat `mcp__server__tool` variant and states what that arm owes: the *reverse*
mapping in `canonical_item`, splitting a flat name back apart on the way in.
`responses_api/wire.rs:461-503` pins the current divergence as a live test.
`PLAN-agentic-control-plane.md:1025-1026` says outright: "Claude Code's native
dialect is the Messages API, so the `Flat` branch is future-proofing for a
Messages surface".

`ClientDialect` is carried on `ControlPlane::Configured` and read through
`ControlPlane::client_dialect()`
(`control_config/mod.rs:517,587-588,782-789`). **Grepping `client_dialect()`
across `crates/` returns only `control_config/mod.rs:1808,1827,1840,1841` — all
four inside `#[cfg(test)]`.** No production path reads it today; the rendering
site it was built for was deleted by M10.0 T4 (`responses_api.rs:696-704`).

---

## 3. THE LAUNCHER

`crates/roundhouse-server/src/codex_launch.rs` (1080 lines) + `codex_launch/skills.rs`
(609 lines).

**Inputs** — `CodexLaunch` (`:280-299`): `base_url`, `key_env`, `auth:
CodexAuthKind`, `model`, `model_catalog_path`. Constructed by
`CodexLaunch::new(base_url, &Path)` (`:311-340`), with builders
`forwarding_openai_login` (`:343-346`), `with_key_env` (`:349-352`),
`with_model` (`:355-358`).

**Outputs** — three, all pure functions returning strings/values, none of which
touches the filesystem:
1. `config_toml()` (`:366-468`) — the `CODEX_HOME/config.toml`.
2. `model_catalog_json()` (`:477-550`) — the pinned model catalog.
3. `skills::skill_files() -> Vec<GeneratedFile>` (`skills.rs:230`), each a
   `{ path (relative, `/`-separated), contents }` pair (`skills.rs:127-…`) under
   `SKILLS_DIR = "skills"` (`skills.rs:~96`).

**Refusals** — `CodexLaunchError` (`:233-255`): `RelativeCatalogPath`,
`NonUtf8CatalogPath`, `BaseUrlMissingApiPrefix`. Each names what the *client*
does with the bad value. A trailing slash is normalised, not refused (`:316`).

**Auth kinds** — `CodexAuthKind` (`:191-224`): `RoundhouseKey`
(`requires_openai_auth = false` **with** `env_key`) and `ForwardedOpenAiLogin`
(`requires_openai_auth = true`, **no** `env_key`, precondition: a completed
`codex login`). The doc at `:38-59` is the strongest evidence in the tree about
why these two lines move together.

**Derived constants that must not be retyped**: `mcp_endpoint(base_url)`
(`:566-568`) = `deployment_root(base_url, API_PREFIX)` + `MCP_MOUNT_PATH`
(`= "/mcp"`, `mcp_api.rs:311`); `deployment_root` (`:583-588`) strips the prefix
rather than a path segment; `mcp_server_key()` (`:597-601`) derives the config
table key by stripping `mcp__` off `DEFAULT_MCP_NAMESPACE`.

**The deferred operator entry point.** `README.md:463-465`: "What it does not
yet have is an **operator entry point**: no CLI subcommand or admin route
produces these files, and whether that is a subcommand or an admin read beside
key minting is deferred by name." `README.md:943-…` ("Not yet built") repeats it,
and adds the dialect sentence: "a chat-completions or Anthropic-messages client
is a new `WireProtocol` arm the compiler will force through every exhaustive
match, and neither exists yet." `agent-docs/PLAN-agentic-control-plane.md:1531-1535`
carries the same deferral verbatim, and `README.md:41-43` repeats it in the
status block.

**Shape a `claude_launch` sibling would take, from this repo's side only.** The
existing module is a pure config *generator* with no I/O and no binary. Every
input it needs is already in the server crate (`lib.rs:53-56` states exactly
why): the bound address, `TURN_KEY_HEADER`, `MCP_MOUNT_PATH`, `API_PREFIX`. A
sibling would need the same four plus whatever prefix a Messages surface is
served at. Nothing in `crates/` today reads a CLI argument: `main.rs:4-9` says
"Configuration is one environment variable, because a flag parser here would be
the first place a deployment concern leaked into the composition root", and there
is no `clap`/`argh` anywhere in `Cargo.toml`. *Trade both ways:* a `[[bin]]`
subcommand inside `roundhouse-server` reuses all four constants with zero
plumbing but puts an argument parser into the composition root that module doc
argues against; a new workspace crate (`crates/roundhouse-launch`) keeps
`main.rs` clean but has to depend on `roundhouse-server` for the four constants,
inverting the current dependency direction (`roundhouse-server` is the top of the
graph — see `Cargo.toml:6-13` and `crates/roundhouse-server/Cargo.toml:20-30`).
An admin-plane read (`admin_api.rs`, 841 lines, `/v1/admin`) is the second of
the two options the deferral itself names (a CLI subcommand or an admin read —
`README.md:463-465` names exactly those two; the new-workspace-crate option
above is this document's addition, not the deferral's) and needs no new crate
or binary at all.

---

## 4. TESTS AS TEMPLATES

| suite | what it is | reusable for `anthropic_messages` |
|---|---|---|
| `crates/roundhouse-fleet/tests/openai_responses_upstream.rs` (347 lines) | **The closest template.** A hand-rolled axum mock upstream (`:77-94`) recording the *whole* `HeaderMap` of every request (`:78`), three `Behaviour`s (`:58-70`): stream a canned SSE body (`SSE_BODY`, `:47-56`), echo the credential in a 401, and 307-redirect. | Direct: swap `SSE_BODY` for a `message_start`/`content_block_delta`/`message_delta`/`message_stop` fixture and the auth assertions for whatever header row is added. |
| `crates/roundhouse-fleet/tests/finding1_usage_enforcement.rs` (213 lines) | Proves the `FrontierQuote` seam can discharge the usage obligation; `target_alone_does_not_identify_a_dialect` (`:181-212`) is why the dialect rides on the quote. | The analogous claim for Anthropic is that both usage events are folded — the same shape of test. |
| `crates/roundhouse-server/tests/codex_conformance.rs` (648 lines) | The `/v1/responses` conformance oracle: drives the **pinned real codex parser** (`codex_api::ResponsesClient`) over a `tower::Service` router with no socket (`:116-133`), plus one live-socket round trip (`:601`). | A `/v1/messages` equivalent needs an Anthropic client crate as oracle. **No `anthropic`/`@anthropic-ai` dependency exists anywhere in the workspace** — verified by reading `Cargo.toml` and `crates/roundhouse-server/Cargo.toml` in full; the only pinned client crates are the four `openai/codex` ones (`crates/roundhouse-server/Cargo.toml:99-102`). |
| `crates/roundhouse-server/tests/codex_wire_shapes.rs` (419 lines) | Parser facts pinned against a `CannedTransport` — a fixed SSE byte body with no router behind it (`:39-45`). | The pattern for pinning Messages-wire facts without a server. |
| `crates/roundhouse-server/tests/codex_e2e.rs` (2819 lines) | The gated real-binary suite. `#![cfg(feature = "e2e-codex")]` (`:3`), binary overridable by `ROUNDHOUSE_TEST_CODEX_BIN` (`:263`), invoked per `README.md:470-476` with `--features e2e-codex --include-ignored --test-threads=1`. | A `claude_e2e.rs` sibling would need the same feature-gate + env-override + `--test-threads=1` shape (each test owns its own agent home). |
| `crates/roundhouse-server/tests/provider_registry.rs` (468 lines) | The M10.1 registry proofs: per-provider transport (`:138`), uniform control (`:179`), unknown provider fails the turn (`:202`), failover across providers uses the second provider's transport (`:350`). | Directly extendable to a second *dialect* client in the registry. |
| `crates/roundhouse-server/tests/common/mod.rs:180-240` | `ScriptedFrontierClient` — records every `FrontierQuote` (`quotes_seen`, `:211`), branches on `prompt_cache_key.ends_with("#validate")`. | The in-process double for anything that does not need a socket. |
| `crates/roundhouse-server/tests/common/codex.rs` (`:1-17` states the split rule) | Everything needed to *be* a codex client: `RouterTransport` (`:103`), `NoAuth`/`StaticToken`, `request`/`user_message`/`function_call_item` builders, `frames`/`collect`. | The shape a `common/anthropic.rs` would take. |

**Existing fixture infrastructure for a mock SSE server:** the hand-rolled axum
pattern, twice — `openai_responses_upstream.rs:77-94` (real socket) and
`codex_conformance.rs`/`common/codex.rs` (`tower::Service`, no socket). Dev-deps
that make both possible: `axum` (`crates/roundhouse-fleet/Cargo.toml:49`),
`http-body-util` + `tower` (`crates/roundhouse-server/Cargo.toml:88-90`). There
is no `wiremock`/`mockito` anywhere — `crates/roundhouse-fleet/Cargo.toml:45-48`
says so explicitly as a house rule.

---

## 5. RISKS, COLLISIONS, AND THE SILENT-GAP LIST

**Places that assume exactly one frontier wire dialect:**

1. `main.rs:189-195` — `ROUNDHOUSE_FRONTIER_UPSTREAM` accepts one literal string.
   Stringly typed; the compiler will not name it.
2. `main.rs:232-243` — boot bail on `spec.wire_protocol != OpenAiResponses`. An
   `!=` against one constant, not a match.
3. `main.rs:254-257` — `.for_dialect(WireProtocol::OpenAiResponses).expect(...)`.
   The `expect` message says the catalog boundary guarantees it; with a second
   dialect the constant here is wrong, not the invariant.
4. `openai_responses.rs:101` — `const SPOKEN`. Correct and self-refusing; the
   *risk* is a second client copying the pattern and forgetting the check.

**Places that assume exactly one serve dialect:**

5. `responses_api.rs:125` — `API_PREFIX = "/v1"` is deliberately *not* shared
   with admin/metrics/session routes (`:120-124`), but **is** consumed by
   `codex_launch::deployment_root` (`:583-588`). A `/v1/messages` served under
   the same prefix inherits that coupling; served under a different one, the
   launcher's `BaseUrlMissingApiPrefix` check (`:317-319`) needs a second answer.
6. `dialect.rs:79-89` — one `ClientDialect` variant, and `client_dialect()` has
   **no production reader** (§2.7). Adding a `Flat` variant compiles a dead enum
   wider unless a renderer is wired at the same time.
7. `responses_api/wire.rs:462-503` — the pinned test asserting a flat
   `mcp__roundhouse__fetch_steer` canonicalizes to that whole string. This is the
   test a Messages/Claude-Code surface must deliberately revisit; it will not go
   red on its own.
8. `crates/roundhouse-relay/src/atof.rs:81` — `const OPENAI_CHAT_COMPLETIONS:
   (&str, &str) = ("openai/chat-completions", "1")`, the schema every ATOF LLM
   scope declares its `data` under, unconditionally.

**Metrics/provider labels:** `ModelKey { mode, provider, model }`
(`crates/roundhouse-core/src/metrics/mod.rs:123-127`) carries **no dialect
axis** — two entries for one `(provider, model)` over different dialects would
collapse into one row, which is precisely why `CatalogConfig` refuses that shape
(`catalog_config.rs:383-391`). `LOCAL_PROVIDER = "dynamo"` (`metrics/mod.rs:95`).
`Target::policy_identity()`
(`crates/roundhouse-core/src/routing/mod.rs:95-102`) renders `provider/model` and
its doc already uses `anthropic/*` as its worked example.

**Tokenizer story for Anthropic models.** There is one tokenizer per process,
chosen at composition (`main.rs:809` wires `ByteTokenizer`; `HfTokenizer`
—`crates/roundhouse-server/src/tokenizer.rs:22-33` — is the real-deployment
alternative, loaded from a single `tokenizer.json`). Every one of these is
computed with *that* tokenizer regardless of which target serves the turn:
`isl_tokens` from `assembler.buffer()` (`engine.rs:1482`), the frontier quote's
prices (`engine.rs:1527-1531`), `admitted_input_tokens` (`engine.rs:1385-1390`),
`context_contribution` (`engine.rs:1429-1441`), and `estimated_usage`
(`engine.rs:1455-1460`). Anthropic publishes no local tokenizer. Consequences
today, for a turn served by a frontier Anthropic model: (a) routing prices it
against a *Llama-ish* token count, which is a systematic estimate error, not a
random one; (b) the local block hashes are meaningless to it anyway — they only
route local workers (`tokenizer.rs:38-48`); (c) if the provider reports usage,
the log records the provider's numbers and marks them `Accounting::Reported`
(`event.rs:63-79`), so only the *quote* is wrong, not the bill; (d) if it does
not, `estimated_usage` books our tokenizer's count and marks it `Estimated`.
Nothing in the tree treats this as a defect; nothing in the tree measures it
either.

**The compiler will NOT force a change at (the silent-gap list):**

- `main.rs:114,189-195` — the upstream-name string.
- `main.rs:233` — the `!=` dialect gate.
- `main.rs:254-257` — the hardcoded `for_dialect` argument.
- `forwarded.rs:58-61` — `ALLOWLIST` is a `const &[(&str, &[&str])]`; adding a
  provider is a data edit no type checks.
- `control_config/mod.rs:900-…` — `presented_key` reads two header names as
  literals; `x-api-key` is not one of them.
- `responses_api/wire.rs:309` — `"cache_write_tokens": 0` is a JSON literal.
- `frontier.rs:180-225` / `event.rs:31-58` — no cache-write field on
  `FrontierChunk::Done` or `Usage`; an Anthropic
  `cache_creation_input_tokens` has nowhere to go and nothing goes red.
  The same gap is independently documented at a third surface this dive's
  brief did not cover (fact-check addition):
  `crates/roundhouse-relay/src/summary.rs:535-565` — `relay_usage()` and
  `relay_tokens()` hardcode `cache_write_tokens: None`, with the doc at
  `:537-541` stating why: roundhouse *prices* uncached tokens at the
  cache-write rate but does not *measure* a cache write, and a field named
  for a measurement must not publish a pricing convention. Widening
  `Usage` therefore has three readers waiting, not two.
- `ledger.rs:121-149` — `price_tokens` bills **all** uncached input at
  `effective_write_per_mtok_usd`. On a provider that prices cache *creation*
  separately from ordinary uncached input (Anthropic's model, per the
  `CacheModel::Deterministic` doc at `ledger.rs:33-38`), that overcharges every
  uncached token that was never written. *Trade both ways:* leaving it
  overstates our own cost, which biases the savings figure conservatively (the
  safe direction); fixing it requires the measured cache-write count that (per
  the bullet above) the log has nowhere to store.
- `frontier.rs:5-12` — the module doc promises `cache_control` markers "at the
  same prefix boundary each turn", but `rg cache_control --include=*.rs` over
  `crates/` returns exactly two hits, both comments (`ledger.rs:35`,
  `frontier.rs:10`). **`FrontierQuote` carries no breakpoint field**, so the
  routing model's `Deterministic` arm is being applied to a provider we would
  not, today, be able to send a breakpoint to.
- `frontier.rs:276` — `prompt: String`, filled from
  `ContextAssembler::rendered()` (`context.rs:229-231`), a single
  `<|role|>`-prefixed concatenation. `OpenAiResponsesClient::body` wraps the
  whole thing in **one** `user` message (`openai_responses.rs:233-236`). A
  Messages client can do the same, but Anthropic's `system` is a top-level field
  and its cache breakpoints attach to message/system blocks — so parity with the
  existing client means a structurally degenerate request. *Trade both ways:*
  keeping the flat string preserves byte-identical prompt semantics with every
  other target and keeps the prefix hashes honest; splitting the render back into
  roles requires a second projection of the item list that could disagree with
  `rendered()` and therefore with `turn_id_for` and the block hashes.
- `ClientDialect::client_dialect()` — production-unread (§2.7); widening it
  changes no behaviour by itself.

---

## 6. Where a decision could go two ways (stated, not ruled)

1. **Session naming for `/v1/messages`** — client-configured key vs. derived
   from content vs. a path-carried session id. All three have precedent in this
   tree (§2.3).
2. **Serve prefix** — under `/v1` (inherits the `codex_launch` coupling at
   `responses_api.rs:120-124` and `codex_launch.rs:583-588`) or its own.
3. **Launcher home** — `[[bin]]` subcommand, new crate, or admin route; the
   deferral names the first and third and rules on none (`README.md:463-465`;
   the new-crate option is §3's addition).
4. **Prompt shape on the Anthropic wire** — flat single-user-message parity vs.
   role-structured (§5, last bullets).
5. **Cache-write accounting** — widen `Usage`/`FrontierChunk::Done` now (a
   durable serde shape change with no reader, which `frontier.rs:210-221`
   argues against by precedent for `provider_reported_cost`) or defer until a
   consumer exists.
6. **The Anthropic `ALLOWLIST` row** — adding `("anthropic", ["authorization",
   "x-api-key"])` is one line, and `forwarded.rs:53-57` says explicitly that
   writing it before a client exercises it is a promise the table should not
   make.
