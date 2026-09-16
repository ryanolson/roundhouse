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

---

## 6. Addendum (2026-09-02): the flat-name arm and the MCP mount, as the tree stands after M11.3

Evidence-only, per CLAUDE.md's "validating a claim" order: nothing here is ruled,
and every place the call could go two ways is stated as two ways. Tree state:
`566dd26` ("M11.3: topham, the launcher"). Live captures were taken against
`claude-cli 2.1.257` under `env -i` with a scratch `HOME`/`CLAUDE_CONFIG_DIR`, a
loopback mock at `ANTHROPIC_BASE_URL`, and `ANTHROPIC_API_KEY=sk-ant-capture-dummy`
— never the container's own credential (client-surface §5.7).

### 6.1 What the client actually spells (captured, not inferred)

Captured request to the mock at `POST /v1/messages?beta=true`, with one MCP server
registered by `--mcp-config`:

- The MCP tool arrives as an **ordinary top-level entry of `tools[]`**, flat-named
  and with no namespace field and no `mcp_servers` key anywhere in the body:
  `{"name":"mcp__roundhouse__ping","description":"p","input_schema":{"type":"object"}}`.
  The other 21 entries are the client's own built-ins (`Bash`, `Read`, `Skill`, …).
- The assistant's call comes back on the resend as
  `{"type":"tool_use","id":"toolu_cap1","name":"mcp__roundhouse__ping","input":{}}`
  inside an `assistant` message, and its answer as
  `{"tool_use_id":"toolu_cap1","type":"tool_result","content":"(mcp__roundhouse__ping completed with no output)"}`
  inside a **`user`** message — which is the transport convention
  `messages_api/wire.rs:585-591` already unwraps to `Role::Tool`.

So on this dialect there is nothing for a *namespace* field to be read from: the
flat spelling is the only spelling, on the way in and on the way out.

### 6.2 Inbound: what canonicalization owes, and where

`messages_api::wire::block_item` (`crates/roundhouse-server/src/messages_api/wire.rs:632-656`)
maps `ContentBlock::ToolUse { id, name, input }` to
`ItemContent::ToolCall { call_id: id, name, arguments: input.to_string() }` — `name`
verbatim, exactly as `responses_api::wire::canonical_item`
(`crates/roundhouse-server/src/responses_api/wire.rs:94-102`) takes `required_str(value,"name")`
verbatim. **Two sites, not one**, and only the Responses one is named by
`dialect.rs:73-77`'s F10 exception. The M11 Messages surface added the second and
nothing pins it.

The behaviour is pinned today by
`a_flat_spelling_is_a_different_canonical_call_until_the_wire_learns_to_split_it`
(`responses_api/wire.rs:618-660`), whose `assert_ne!` is the assertion that has to
go the other way when the arm lands; the namespaced control is
`a_clients_namespaced_call_canonicalizes_to_the_bare_stored_item`
(`responses_api/wire.rs:576-598`). There is **no Messages-side equivalent** of
either — `messages_api/wire.rs:875-920` asserts a `Grep` call round-trips, and no
test in that file names `mcp__`.

### 6.3 The collision the reverse split walks into

`roundhouse_core::validate::is_control_call` (`crates/roundhouse-core/src/validate/exchange.rs:194-198`)
recognises roundhouse's own control traffic by matching `CONTROL_TOOL_NAMESPACE`
(`"mcp__roundhouse"`, `exchange.rs:169`) plus `CONTROL_TOOL_DELIMITER` (`"__"`,
`exchange.rs:184`) against `Exchange.name`, and `Exchange.name` is the **stored
canonical** name (`exchange.rs:92-106`, `name: name.clone()` off `ItemContent::ToolCall`).
`task_exchanges` (`exchange.rs:215-220`) drops those from every signal the trigger
computes.

Two consequences, both load-bearing for the arm:

1. **On the Responses path the exemption is already inert.** Canonicalization
   stores the bare `status`, and `is_control_call("status")` is false. The validate
   suites exercise it only with flat names — `tool_signals.rs:1163-1168` uses
   `"mcp__roundhouse__status"`, `trigger.rs:568` the same — so no test distinguishes
   the two.
2. **The flat arm makes it live, and the reverse split would kill it again.** A
   Claude Code control call arrives as `mcp__roundhouse__status`, which
   `is_control_call` matches. Teaching `block_item` to split the flat name back to
   `status` — which is precisely what `dialect.rs:31-37` says the arm owes — makes
   it stop matching, and roundhouse's own control calls re-enter the streaks,
   windows and depth the steer trigger is computed over. That is the failure G04
   named (`exchange.rs:202-214`: four calls made because our own generated skill
   said to make them bought a judge side-call the session did not need).

Three ways out, stated and not ruled:

- **Split inbound, and re-key recognition off the bare tool names** (e.g. against
  `roundhouse_mcp::tools::TOOL_NAMES`, `crates/roundhouse-mcp/src/tools.rs:73-82`).
  Cost: an agent's own tool called `status` is then exempted from every signal,
  and there is no namespace left to tell them apart.
- **Split inbound, and thread the dialect into the fold.** `exchange.rs:159-168`
  already writes the unlock condition down: a `SignalContext` carrying the
  dialect, passed everywhere `ToolSignals::from_exchanges` is called. It also
  states the cost — deployment configuration inside the one part of the validate
  loop that is a function of the session log alone.
- **Do not split; store the flat name.** Contradicts `dialect.rs:14-21`'s
  neutral-stored-name argument directly, and forks any session that ever changes
  dialect.

### 6.4 Outbound: what the enum forces today

`grep -rn 'ClientDialect::'` over the workspace returns five sites, all in
`control_config/mod.rs` (`:213` the `OPEN_DIALECT` static, `:660-661` the
compile-from-config, `:2040`/`:2059` tests) plus `dialect.rs` itself.
`ControlPlane::client_dialect` (`control_config/mod.rs:855-862`) has **no production
caller** — every reference is in `control_config/mod.rs:2039-2072`'s tests. That is
consistent with `responses_api/wire.rs:558-562`: "the outbound half is gone — no
response projects a `function_call` frame any more, so `EmittedCall` and its two
builders were deleted with the steer they existed for".

So **the arm forces zero run-time rendering sites**. The namespace is spelled
outbound in exactly two generation-time places, neither of which reads the dialect:

- `codex_launch::mcp_server_key` (`codex_launch.rs:597-601`), which strips
  `MCP_NAMESPACE_PREFIX` off `DEFAULT_MCP_NAMESPACE` to get the `[mcp_servers.<key>]`
  table key;
- `codex_launch::skills::namespaced_tool_name` (`codex_launch/skills.rs:246-248`),
  `format!("{DEFAULT_MCP_NAMESPACE}{MCP_TOOL_NAME_DELIMITER}{tool}")`.

The one site F10 named that the compiler cannot name is therefore the *inbound*
one, and §6.2 above says it is now two sites rather than one.

### 6.5 How a deployment picks its dialect today

Per deployment, from the control-plane file, and nowhere else. `mcp_namespace` is
an `Option<String>` on the config (`control_config/config.rs:428`), validated at
load (`config.rs:1188-1195`; refused empty or whitespace-carrying,
`BadMcpNamespace` at `config.rs:550-554`), and compiled once into
`ControlPlane::Configured { dialect, .. }` (`control_config/mod.rs:589-590`,
`:659-661`). `ControlPlane::Open` answers a `LazyLock` default
(`mod.rs:207-213`). It is **not per key** (`mod.rs:1087-1092` says so out loud:
"the key decides who a key is; the dialect decides how a call is spelled") and
**not per request path** — nothing on either turn surface reads it.

That is the seam M12 has to decide at: a deployment that serves both
`/v1/responses` (codex) and `/v1/messages` (Claude Code) has one `ClientDialect`
value and two clients that spell a call differently. Either the dialect stops
being a deployment-wide value and becomes a property of the accepting surface —
which the Messages handler already does for tools (`messages_api.rs:461-468`
stamps `tools_dialect: Some(WireProtocol::AnthropicMessages)` because "this is the
only layer that knows") — or the enum stays deployment-wide and one of the two
clients is served a spelling it cannot dispatch.

### 6.6 The MCP mount, and how a Claude call would reach a session

**Mount and auth.** `MCP_MOUNT_PATH = "/mcp"` (`mcp_api.rs:311`), mounted
`post_service` so every other method is 405 (`mcp_api.rs:359-365`), behind
`auth_layer` (`mcp_api.rs:375-390`) which calls `ControlPlane::scope` on the request
headers, refuses `KeyScope::Admin`, and inserts the resolved `Principal` into the
request extensions. `RoundhouseMcp::caller` (`roundhouse-mcp/src/transport.rs:128-140`)
reads it back out of the `http::request::Parts`; a request with no principal is a
protocol error, not a default tenant. `TURN_KEY_HEADER = "x-roundhouse-key"`
(`control_config/mod.rs:177`) is the header, and `scope` also accepts
`Authorization: Bearer` (`mod.rs:1026`, `:897`).

**Protocol.** Stateless: `NeverSessionManager` + `legacy_session_mode = false` +
`json_response = true` (`transport.rs:255-277`), so no `Mcp-Session-Id` is issued
and there is no server-side session to hang a conversation on. Captured
handshake: the client offers `protocolVersion "2025-11-25"` and, once the server
answers `V_2025_06_18` (`transport.rs:164`), stamps `mcp-protocol-version: 2025-06-18`
on every later POST — negotiation works against the pinned version.

**What `init_session` binds.** `ControlStore::bind_session`
(`roundhouse-mcp/src/store.rs:429-454`) mints an `rhb_…` `BindingId` for
`(principal, session)`, idempotently. The read side is `binding_in_log`
(`store.rs:495-509`), which scans the *rendered text of every item*
(`binding_ids_in_items`, `store.rs:542-553`) and applies a tenancy check on both
halves. `store.rs:487-494` states plainly that it has **no production caller**:
`mcp_api::resolve_session` answers from the cache key and from
`Conversations::latest`, never from a binding.

**How the correlation would have to work for Claude Code.** `tools/call` carries
`{name, arguments}` and nothing else that this stack reads —
`transport.rs:210-216` builds `ToolCall` from `request.name` and
`request.arguments` only, so an `_meta` a client sent would be discarded before
`crate::tools::dispatch` ever sees it. The headers are **static per config file**
(captured: the same three headers on `initialize`, `notifications/initialized`
and `tools/list`), so there is no per-conversation header a client could set.
That leaves exactly two paths, and both are already in the tree:

1. **Omit `conversation`** → `ControlPlaneReads::resolve_session`
   (`mcp_api.rs:141-148`) answers `Conversations::latest(principal)`
   (`conversations.rs:146-148`), which every Messages turn sets through
   `bind_prefix` (`messages_api.rs:368-377` → `responses_api.rs:493`). Works for
   one agent, one conversation, one node. Fails silently for a Claude Code
   *subagent*, whose session is a sibling name `anthropic_messages/{session}/agent/{id}`
   (`messages_api/wire.rs:286-291`) racing the parent for `latest`; and across
   nodes, where `latest` is node-local.
2. **Pass `conversation`** = the client's own cache key. On this dialect that key
   is `anthropic_messages/{session_id}` (`DIALECT_NAMESPACE`, `wire.rs:101`;
   `scoped`, `wire.rs:286-291`), built from `metadata.user_id`'s `session_id`
   (captured: `{"device_id":"…","account_uuid":"","session_id":"8e124bec-…"}`) or
   the session header. **The model cannot know that string** — it never appears in
   the model's context — so this path needs either `init_session` returning it,
   or the `binding_in_log` join at `store.rs:495` finally acquiring a caller.

The tool descriptions currently tell a model the argument is "the client's own
prompt_cache_key" (`tools.rs:121-126`), which is Responses vocabulary; on this
dialect there is no `prompt_cache_key` field at all.

### 6.7 MCP config generation: where it belongs and what it carries

`claude_launch.rs:58-65` defers this by name — "No MCP wiring… Deferred with the
MCP control surface" — and `claude_launch.rs:10-19` states the module's whole
shape: *nothing here writes a file*. An `--mcp-config` JSON **is** a file, so the
deferral and the module's shape are the same decision.

**The secret tension, and the captured way out.** `codex_launch` can keep the key
out of its own hands because codex's config names a *variable*
(`bearer_token_env_var = {key_env}`, `codex_launch.rs:450`); `claude_launch.rs:21-32`
records that Claude Code offers no such indirection for
`ANTHROPIC_CUSTOM_HEADERS`, which is parsed as literal `Name: Value` lines. **For
`--mcp-config` that is not true.** Captured against 2.1.257: a header value of
`"${RH_TURN_KEY}"` in an `--mcp-config` file arrived at the server as
`x-roundhouse-turn-key: rh_turn_EXPANDED_SECRET_VALUE_…`. So the codex precedent
carries over exactly — the file names a variable, the launcher never writes a
secret to disk, and `topham` already guarantees the variable is exported
(`plan.rs:129-134` refuses to resolve at all when `profile.key_env` is unset, and
`launch.rs:166` starts the child map from the ambient environment, so `key_env` is
present in the child on the Claude arm too).

**The hazard that comes with it, also captured.** With the variable *unset*, the
client sends the header **literally** as `${RH_TURN_KEY}` — no warning, no
refusal, no failure to start. Against a `Configured` plane that is a rejected
handshake; against `ControlPlane::Open` (`mod.rs:1076`) every request resolves to
`Principal::default_open` regardless, so the mis-launch is invisible. The
short-lived-0600-file alternative is therefore a fallback rather than the
reference: it is only needed where the launcher cannot guarantee the variable
reaches the child, and it costs a secret on disk plus a deletion that an `exec`
(`launch.rs:246-261` — this process is gone) cannot perform.

**Where the generation would hang in `topham`.** `Resolved::Claude`
(`plan.rs:81-87`) carries `launch`, `env`, `must_be_unset` and no files;
`plan.rs:238-241` prints "files written by `topham launch`: (none) -- Claude
Code's whole redirect surface is environment". `launch::layered`
(`launch.rs:169-180`) populates `files` only on the Codex arm, though
`LaunchPlan.files` (`launch.rs:93`) is already `Vec<(PathBuf, String)>` and
`write_files` (`launch.rs:210-224`) is agent-agnostic. So the file half is a
`Resolved::Claude` field plus one `push` in `layered`.

The **argv half has no seam at all**: `LaunchPlan.argv` (`launch.rs:85-89`) is
"the operator's `-- <argv>`, verbatim… no default arguments are invented", and
`plan()` (`launch.rs:132-150`) passes it through untouched. `--mcp-config <path>`
(and, if the deployment wants the client to ignore the operator's own servers,
`--strict-mcp-config`) has to be injected somewhere, and today nothing injects
argv. Two ways: a generated-argv prefix on `LaunchPlan` that the operator's argv
is appended to, or a `.mcp.json` in the working directory, which needs no argv and
is project-scoped — and which the client marks "Pending approval" until a person
approves it (`claude mcp list --help`), so it does not connect on the first launch.

### 6.8 Signage: the Claude analogue of `codex_launch::skills`

Three surfaces, all captured against 2.1.257 by reading where the text lands in
the request the client then sends:

| Surface | Where the text lands | Admission |
|---|---|---|
| `--append-system-prompt <text>` | appended to the **leading `system` blocks** (captured: block `[2]` of three) | `mark_turn_configuration` (`messages_api/wire.rs:472-479`) makes the leading system run `Role::Developer` — **loosely admitted turn configuration** |
| `$CLAUDE_CONFIG_DIR/skills/<name>/SKILL.md` | an **interior `{"role":"system"}` message**, as `- {name}: {description}` beside the agent-type listing, carrying a cache breakpoint | `wire.rs:449-454` rules an interior system message **history, admitted strictly** |
| `CLAUDE.md` in the working directory | inside the `<system-reminder>` **text block of the first `user` message** | an ordinary `Role::User` item, **admitted strictly** |

Two facts worth pinning:

- **`CLAUDE_CONFIG_DIR`, not `HOME`, governs the user-scope skills root.** With
  `HOME=<a>` holding `.claude/skills/rh-status` and `CLAUDE_CONFIG_DIR=<b>` holding
  `skills/rh-alt`, only `rh-alt` was listed to the model. That makes
  `$CLAUDE_CONFIG_DIR/skills` the exact analogue of `$CODEX_HOME/skills`
  (`codex_launch/skills.rs:84-94`) — and the same hermeticity argument applies.
- **Owning `CLAUDE_CONFIG_DIR` is not free the way owning `CODEX_HOME` is.** It
  moves the client's whole config, including its login. Under
  `ClaudeAuthKind::ForwardedClaudeLogin` (`claude_launch.rs:300-307`: "the
  precondition is a completed `claude` login") a fresh config dir means no login,
  which roundhouse *admits* and degrades to local-only — the exact silent failure
  that variant exists to prevent.

`--append-system-prompt` is therefore the least invasive of the three: no file
written anywhere the operator owns, no config dir moved, and the text lands in the
one run this surface already admits loosely. Its cost is stated in the client's own
help: passing it "turns off [`--system-prompt-snapshot`] so the given text applies
fresh each launch", i.e. the leading run is re-rendered per launch rather than
recorded once — which is precisely the drift `mark_turn_configuration` absorbs, and
precisely what would fork the session if the same text were put in an interior
system message or in `CLAUDE.md` instead.

### 6.9 What the Messages surface would have to accept in `tools[]` that it does not today

Nothing, for a client-registered MCP server: `params.tools` is taken verbatim
(`messages_api.rs:431`, `:449-459` — "verbatim, and not canonicalized… never
replayed out of the log") and forwarded with a `tools_dialect` stamp. The flat
`mcp__roundhouse__*` entries ride through as ordinary client tools.

What *would* be new is roundhouse **adding** its own control tools to a toolbox the
client did not declare — the Codex topology never needs this, because codex
registers the MCP server itself and the tools reach the model through that
registration. On this dialect the same is true (captured), so the question only
arises for a deployment that wants the control surface present without an
`--mcp-config`. That would mean appending to `tools[]` on the way to the target,
which changes `admitted_input_tokens` (`messages_api.rs:359-363`, the figure
`message_start` reports) and puts a tool in the model's context the client's own
loop has no dispatcher for — the client would emit a `tool_use` it cannot execute.

### 6.10 Risk list

1. **Two inbound sites, one pinned.** `responses_api::wire::canonical_item:94-102`
   and `messages_api::wire::block_item:632-656` both take `name` verbatim; only the
   first has a test naming the flat spelling. An arm that fixes one and not the
   other forks Messages sessions on their second turn and nothing goes red.
2. **The reverse split un-recognises control traffic.** §6.3. Silent: the trigger
   simply starts counting roundhouse's own calls as agent trouble.
3. **A changed stored name is a changed `turn_id`.** `turn_id_for` hashes the
   canonical items (`responses_api/wire.rs:199-201`), and
   `the_turn_id_of_a_fixed_conversation_is_pinned` (`wire.rs:672-680`) exists
   because moving historical hashes orphans every in-flight retry. Splitting names
   inbound moves them for every tool-using session already in the store.
4. **A deployment-wide `ClientDialect` cannot serve two dialects at once.** §6.5.
5. **`${VAR}` that does not expand is sent literally.** §6.7. On
   `ControlPlane::Open` the mis-launch is completely invisible.
6. **A 0600 file the launcher writes is a file nothing deletes.** `ExecLauncher`
   replaces the process (`launch.rs:246-261`), so any "write then delete" scheme
   needs a supervisor topham deliberately does not have.
7. **`latest`-based correlation forks under subagents.** §6.6. Claude Code's Task
   tool gives a subagent a sibling session name (`wire.rs:286-291`); two of them
   plus a parent race one `latest` slot per principal, and the loser reads
   another conversation's status.
8. **`conversation` is documented in Responses vocabulary.** `tools.rs:121-126`
   tells a Claude Code model to pass "the client's own prompt_cache_key", a field
   this dialect does not have. Every tool schema repeats it (eight descriptors),
   and the descriptors are golden-pinned and prompt-cache-relevant
   (`tools.rs:12-18`), so correcting the wording invalidates prompt caches
   deployment-wide.
9. **`--mcp-config` widens what a turn key reaches.** The same key authenticates
   the turn surface and `/mcp`; a config file handed to a client is a durable
   grant of eight tools, two of which write overlays, on whatever conversation
   `latest` resolves to.
10. **Signage placed wrong forks sessions.** §6.8: a skill listing lives in a
    strictly-admitted interior system message, so adding or removing one
    mid-session forks it — the same class as the budget notice that
    `is_ephemeral_client_notice` (`wire.rs:393-416`) exists to drop.
