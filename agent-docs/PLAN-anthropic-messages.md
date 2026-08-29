<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Plan: the Anthropic Messages surface, the seat, and the launcher (M11)

> **Status: proposed design.** Direction set by the product owner on
> 2026-08-27: support Anthropic's Messages API the way roundhouse supports
> OpenAI's Responses API; the test topology is a real Claude Code session on a
> subscription login, intercepted by NeMo Relay and relayed to roundhouse; and
> an operator CLI with a TUI creates configs and launches the coding agents —
> using Relay's launcher where it already has one. Evidence, all produced and
> independently fact-checked 2026-08-27 (63 claims re-derived, 62 confirmed
> exactly, 1 count corrected):
> `research/anthropic-messages-wire-crates.md` (Dive A),
> `research/nemo-relay-0.8.0-published-read.md` (Dive B),
> `research/claude-code-client-surface.md` (Dive C),
> `research/anthropic-messages-seam-map.md` (Dive D). Where this plan and the
> evidence disagree, this plan wins and the disagreement is a bug in one of
> them. M10 declared Claude Code explicitly out of scope for its phase; this
> plan is the phase that brings it in, and it assumes M10.0–M10.2 (#13) as
> merged. One standing ruling is **superseded** here — see §2 — and the
> supersession is recorded as a dated addendum in
> `synergies/ecosystem-round-2.md`.

## 1. Ground truth this plan stands on

Each fact carries its dive citation; the rungs below are built not to
re-discover them.

- **There is no official Anthropic Rust SDK** — the official lineup is exactly
  seven languages, none Rust — **and no community crate is adoptable for the
  shipped path.** The best-shaped crate (`siumai-protocol-anthropic`) is
  client-direction only and speaks a foreign IR; the bidirectional ones are
  either 2025-era with closed enums that reject correct 2026 traffic
  (`claudius`, six of twelve response block types missing, OpenSSL in default
  features, no types-only gate) or carry an invented usage field that would
  silently mis-report the one counter this product is judged on
  (`adk-anthropic`'s `cache_creation_input_tokens_1h`, which does not exist on
  the wire). **No Rust crate anywhere models the real
  `usage.cache_creation.{ephemeral_5m,ephemeral_1h}_input_tokens` breakdown.**
  (Dive A §1, §5, §5.4.)
- **Anthropic's OpenAPI 3.1.0 spec is published and pinnable**: 2.4 MB,
  content-addressed URL, sha256 `942a1163…3d2ee87`, discovered via
  `anthropic-sdk-typescript@7ba6a3fc`'s `.stats.yml`. Its strictness is
  asymmetric — every `additionalProperties: false` sits on request/input
  schemas, zero on responses — and it does **not** describe the SSE transport
  events (`ping` and mid-stream `error` appear nowhere). (Dive A §2–§3.)
- **NeMo Relay 0.8.0 (published to crates.io 2026-08-26) already intercepts
  Claude Code**: env + argv injection behind a loopback gateway exposing
  `POST /v1/messages` and `POST /v1/messages/count_tokens`, a first-class
  `anthropic_base_url` override on three config layers, and verbatim
  forwarding of the client's `Authorization` — a subscription seat survives
  the hop untouched on Relay's side. Relay has an Anthropic **codec, not
  types**: `serde_json::Value` surgery into its own neutral IR, losing
  streamed thinking content, the cache TTL vocabulary, the `cache_creation`
  breakdown, and all but three `stop_reason` values. Its CLI has **no TUI and
  no named config profiles**. (Dive B §1–§4.)
- **Claude Code attaches its subscription OAuth Bearer to whatever host
  `ANTHROPIC_BASE_URL` names** — no allowlist on the inference path — and the
  gateway docs endorse the topology; stripping the `oauth-2025-04-20` beta
  fails the request with 401. Every inference request carries
  `metadata.user_id = user_<install>_account_<uuid>_session_<uuid>`, stable
  across compaction and `--resume`, changed by `/clear` (and adopted from the
  server under `--remote`). The current client line also sends
  `x-claude-code-session-id` (absent in the readable v2.1.42 bundle). The
  SSE consumer dispatches on the `event:` line, enforces `message_start`
  before content, index discipline, and the split-usage merge with a
  greater-than-zero guard on input/cache counts but `??` on `output_tokens` —
  and a stream it cannot parse triggers a **second, non-streaming request for
  the same turn**. Inference posts to `/v1/messages?beta=true` — match on the
  path, not the full URL. (Dive C §1–§3, §4.)
- **Roundhouse's own vocabulary is already half-laid.**
  `WireProtocol::AnthropicMessages` exists with pinned wire name and the
  split-usage semantics documented; `ProviderRoutes.messages` exists and is
  load-bearing in config validation; the example catalog ships an `anthropic`
  provider stanza with no model entry naming it. What does not exist: any
  client, any serve surface, any operator entry point, any cache-write field
  on `Usage`/`FrontierChunk::Done` (three surfaces independently document that
  gap), any `x-api-key` reader, any Anthropic `ALLOWLIST` row, and any
  breakpoint field on `FrontierQuote` despite `frontier.rs:5-12`'s promise.
  Three boot gates in `main.rs` refuse a second dialect today and none is a
  `match` the compiler will reopen. (Dive D §1–§2, §5.)
- **Two `claude` installs exist on the dev box**: the spawnable native binary
  is 2.1.247; the only *readable* bundle is a stale npm 2.1.42. The
  version-vigilance posture from the codex suite applies from day one.
  (Dive C header; Dive A §7.3.)

## 2. One supersession, recorded

`synergies/ecosystem-round-2.md` ruled (2026-08-19): *"The Messages surface:
agentic-api is the incumbent. Roundhouse does not build a native Anthropic
Messages tool loop. If Claude Code traffic materializes, the topology is
roundhouse fronting agentic-api's Messages surface, or Relay's
`ANTHROPIC_BASE_URL` path — not a fourth implementation."*

**Superseded 2026-08-27 by product-owner direction**, and the round-3 addendum
of that same ruling already supplied the reason the old answer cannot carry
this phase: *in every topology where the turn does not pass through
roundhouse, roundhouse owns nothing about it* — no session, no prefix
admission, no pricing, no steering. Claude Code traffic has materialized as a
directive, and fronting someone else's Messages surface forfeits the turn;
Relay's `ANTHROPIC_BASE_URL` path is an *interception* mechanism that still
needs a Messages-speaking upstream to point at. What survives from the old
ruling: roundhouse still builds no server-side **tool loop** — the client
runs its tools, exactly as `/v1/responses` works today. What roundhouse builds
is a wire surface over its own log. The addendum in `ecosystem-round-2.md`
carries this paragraph in dated form.

## 3. The rulings

Stage briefs carry these resolved; an implementation agent re-litigating one
mid-stage is the failure mode this section exists to prevent.

### R1 — Write the wire module; adopt no crate; pin the spec as the vocabulary oracle

The shipped path gets a roundhouse-owned wire module (working name
`roundhouse-fleet/src/anthropic_messages/wire.rs`, shared by the serve surface
in `roundhouse-server`), hand-written under the same discipline as the ATIF
port: **typed exactly where roundhouse reads or originates, open everywhere
else.** Typed: the SSE event set (including `ping` and mid-stream `error`,
which no spec schema and no crate carries), `Usage` with the full
`cache_creation` breakdown, `stop_reason` as an open enum (`Unknown(String)`
arm — Relay's three-arm mapping and claudius's closed enum are both
cautionary evidence), content blocks that map onto conversation items, and
`cache_control` with the real 5m/1h TTL vocabulary. Open: unknown request
fields, unknown block types, and unknown betas ride verbatim
(`#[serde(flatten)]` maps / opaque JSON), because the serve surface is also a
pass-through and `deny_unknown_fields` anywhere on it is the
pass-through-fatal condition Dive A §3 names. Field vocabulary is pinned
against the OpenAPI snapshot — sha256 `942a1163…3d2ee87`, from
`anthropic-sdk-typescript@7ba6a3fc` — by a test that reads the recorded
spelling, the way ATIF field names are pinned; the pin, its source SDK rev,
and the refresh mechanism ("diff `.stats.yml` at a newer SDK rev, re-fetch,
re-run the pinning test") are recorded where the pin lives. The rejected
alternatives and why, for the reader who would re-open this: every candidate
crate fails on direction, coverage, openness, or weight (Dive A §5); Relay's
codec is a lossy observer whose four gaps are precisely the fields a stateful
front end lives on (Dive B §3.5); generating from the spec inherits either
`deny_unknown_fields` on serve or a generation-time suppression switch, and
still hand-writes `ping`/`error` — the spec's value here is as a *vocabulary
oracle*, not a code generator.

### R2 — Widen `Usage` and `FrontierChunk::Done` with a cache-write count, now

The `provider_reported_cost` precedent ("a durable serde shape changes when
something reads it") is satisfied: three readers are already waiting —
`responses_api/wire.rs:309`'s hardcoded `"cache_write_tokens": 0`,
`ledger.rs:141-149`'s pricing of every uncached token at the cache-write rate
(a measured count makes the overcharge correctable), and
`roundhouse-relay/src/summary.rs:535-565`'s deliberately-absent field whose
own doc says it awaits a measurement. The Anthropic client folds
`message_start` + final `message_delta` into one `Done` carrying it; a stream
that never completes still yields **no** `Done` (the engine's
estimated-usage-and-marked path stays authoritative — a zero-token `Done`
reads as a saving, the one failure the metrics chapter is built against).

### R3 — The dispatch client mirrors `openai_responses.rs`, and the quote grows breakpoints

`AnthropicMessagesClient` follows the existing client's shape literally:
`const SPOKEN`, a static assertable `body()` separated from `execute()`, one
`route()` bundling client + base + headers + credential, redaction exhaustive
by error variant, `UnsupportedDialect` self-refusal. Stored credential goes
out as `x-api-key` (Anthropic's convention); `anthropic-version: 2023-06-01`
is pinned in the module beside the constant that names why. Providers are
registry entries: `api.anthropic.com` via the example catalog's existing
stanza, and OpenRouter's GA `/messages` route as the second
`anthropic_messages`-speaking provider (`routes.messages`, stored-key only).
**Anthropic caches nothing without explicit `cache_control` breakpoints**, so
flat-string parity alone would zero the provider cache discount for every
Anthropic turn — against the product sentence. Therefore `FrontierQuote`
gains an additive segment structure (item-boundary offsets into the canonical
`prompt`), the client re-blocks the flat render into content blocks at those
boundaries and sets `cache_control` at the stable prefix boundary — and a
test pins that the segments concatenate to `prompt` byte-exactly, so
`turn_id_for`, the block hashes, and `rendered()` remain one projection. This
discharges the promise `frontier.rs:5-12` has carried unfulfilled.

The three silent boot gates open deliberately, not incidentally:
client construction in `main.rs` becomes an exhaustive `match` on
`spec.wire_protocol`; `ROUNDHOUSE_FRONTIER_UPSTREAM` keeps exactly its
current role (unset = echo stub; `openai_responses` = real transports) rather
than growing per-dialect values — the provider registry made the dialect a
per-catalog-entry fact, so M9's "a second transport adds a value here"
sentence is superseded by the registry that postdates it. The judge's
transport must keep resolving through `for_provider` (Dive D §1.6).

### R4 — The seat: an Anthropic `ALLOWLIST` row lands with the client that exercises it

`("anthropic", ["authorization", "x-api-key", "anthropic-beta",
"anthropic-version"])` — the Bearer for a subscription seat, `x-api-key` for
a client bringing its own Anthropic key, and the two envelope headers the
upstream requires to accept either (stripping `anthropic-beta` under OAuth is
a documented 401). The row is written in the same commit as the client, per
`forwarded.rs:53-57`'s own rule that an unexercised row is a promise the
table must not make. Seat turns resolve `Payer::User` and land in
`seat_tokens`, which is keyed on payer, not dialect — no metrics change.
Admission stays exactly the codex shape: the turn key rides
`x-roundhouse-key` (injected via `ANTHROPIC_CUSTOM_HEADERS`), which is also
what lets `turn_admission` capture the forwardable `Authorization` — the
dedicated-header-only capture rule already implements this design. One
caution recorded, not resolved here: what Anthropic's terms permit a third
party to do with a forwarded `sk-ant-oat`-class token is a product-owner /
ToS question (Dive C open question 2); this plan proceeds for internal test
deployments per the direction that set it.

### R5 — The serve surface: `/v1/messages` over the same log

A `messages_api.rs` sibling of `responses_api.rs`: same engine, same store,
same admission, same fair-use refusal, same log-tailing SSE follower shape.
Specifics that are rulings, not options:

- **Route on the path** — `/v1/messages` and `/v1/messages/count_tokens`;
  the `?beta=true` query Claude Code appends is ignored by axum path routing
  and must be proven harmless by test, including through a chained Relay.
- **Session naming**: `x-claude-code-session-id` header first (confirmed
  live at 2.1.247 by the same-day capture — evidence doc §5.5); else
  `metadata.user_id`, parsed for the session component in **both shapes it
  has shipped in**: the 2.1.247 JSON-object string (`{"device_id":…,
  "account_uuid":…,"session_id":…}` — take `.session_id`) and the older
  underscore form (split on `_session_`); else the whole `metadata.user_id`
  string; else an anonymous fresh session. Each resolved key is
  qualified into the caller's namespace and bound through `Conversations`
  exactly as `prompt_cache_key` is. Claude Code always sends `user_id` (every
  version read), so the product path never reaches the anonymous arm; the
  arm exists so a bare curl client gets a served turn, not a 4xx.
- **Streaming obligations**, from the client's own parser: `event:` line on
  every frame (frames without one are silently dropped — the turn then costs
  the upstream a second non-streaming request); `message_start` prelude
  carrying input + cache counts, output on the final `message_delta`, and
  never an explicit `output_tokens: 0` in a delta (the client's `??` merge
  would clobber); strict start/delta/stop index discipline;
  **keepalives are real `ping` events with a `data:` payload** — a bare SSE
  comment satisfies Claude Code's 300-second byte watchdog but is *dropped by
  a chained Relay's re-encoder*, which discards frames with no `data:` line,
  so only a ping event survives both topologies; mid-stream failure is an
  `event: error` whose body spells `overloaded_error` when retry is intended,
  because under subscription OAuth nothing else mid-stream is ever retried.
- **Non-streaming requests are served genuinely** — Claude Code's auth and
  quota probes are 1-token `stream`-less creates and must not 500; a full
  non-streaming turn is legal but the fallback-cost note (one malformed SSE
  stream = one extra full-price turn) rides the module doc.
- **`count_tokens` is served from the process tokenizer** and marked as the
  estimate it is — cheaper than the client's fallback, which burns a real
  1-token create against the routed model; the foreign-vocabulary caveat from
  Dive D §5 is stated on the handler.
- **Item vocabulary extends additively**: `ItemContent` gains thinking,
  redacted-thinking, and an opaque-block variant so resent history —
  thinking blocks included — round-trips byte-exactly through prefix
  admission, with `render()` defined for each so `turn_id_for` and the block
  hashes stay total. The exact shapes are M11.1's core design work; what is
  ruled is *additive, render-total, round-trip-exact*, and that the
  attribution system block Claude Code prepends is ordinary stored prefix
  (stable per conversation in every version read).
- **`/v1/models` is not served in this phase.** Discovery is opt-in and off
  by default client-side; exposing the catalog would put roundhouse's routes
  in the user's `/model` picker, which is a product decision deliberately
  deferred (Dive C open question 5).

### R6 — The conformance oracle: two tiers, both roundhouse-built

No Anthropic-published strict parser exists in any language — both official
SDKs are deliberately non-validating (bare casts; Pydantic `construct`), so
spawning one is a sequencing oracle at best; and the strict community crates
would reject correct 2026 output (claudius) or mis-assert the cache counters
(adk). So the codex-oracle pattern is mirrored with different provenance:
**tier 1** is a dev-only strict parser written from the pinned spec — closed
enums, `deny_unknown_fields`, the deliberate opposite polarity of the shipped
module — driven over the serve surface's SSE output, plus the official SDKs'
sequencing rules (the seven `MessageStream` throws, the accumulator's five)
encoded as ordering tests. **Tier 2** is the gated real-binary suite:
`--features e2e-claude`, `ROUNDHOUSE_TEST_CLAUDE_BIN` override, one test per
`CLAUDE_HOME`-equivalent, `--test-threads=1`, version printed on every run
and mismatch warning — the codex_e2e discipline verbatim, with the vigilance
row seeded from day one (binary 2.1.247, readable bundle 2.1.42, docs line
≥2.1.229).

### R7 — Topologies: Direct is the reference; Chained instantiates the S3 guards for Anthropic

**Direct**: Claude Code → roundhouse (`ANTHROPIC_BASE_URL` +
`ANTHROPIC_CUSTOM_HEADERS` turn key) → {Dynamo | Anthropic | OpenRouter}.
**Chained**: Claude Code → `nemo-relay claude` (0.8.x) → roundhouse, via
Relay's `[upstream] anthropic_base_url`. Chained is supported only with
`synergies/nemo-relay.md` §S3's four chain guards instantiated for this
surface, plus the Anthropic-specific hazards Dive B pinned, each a guard test
or a documented refusal before the topology is called supported:

1. Relay re-serializes intercepted bodies through an alphabetizing
   `serde_json::Map` — prefix admission must be proven order-insensitive by a
   re-encoded-history test (the S3 guard that Switchyard's `#509` incident
   already proved is not optional).
2. Relay's SSE re-encoder drops `id:` lines — this surface must not carry a
   resumption cursor as an SSE id, or must document that resumption does not
   survive a chained Relay.
3. `?beta=true` survival through Relay's `upstream_url` concatenation is
   verified, not assumed.
4. Relay clears a configured `anthropic_auth_header` whenever the base URL
   changes layer-inconsistently — the chained runbook sets both in one layer.
5. A Relay plugin's internal dispatch-override strips provider credentials
   before redirecting — a seat never arrives on that path, so those turns are
   key-authed only; documented, not reconciled.
6. The S3 originals: no routing around roundhouse, credential attribution
   (which key actually went upstream), one authoritative accounting log.

### R8 — The launcher: Relay's CLI for instrumented runs; a roundhouse TUI for configs and Direct launches

The directive's conditional — "Relay might have this already, if so use it" —
resolves on the evidence to *half*. Relay 0.8.0 already owns the launch
mechanics for the **chained** topology (agent detection, version gates,
ephemeral env/argv injection, snapshot-and-restore persistent installs,
dry-run plans, secret-env hygiene) and is used as-is there; it has **no TUI,
no named profiles, and no vocabulary for models, routes, keys, or budgets**
(its config model is four sections, none naming a model). So roundhouse ships
the other half: a new workspace crate **`topham`** (binary `topham`; named
2026-08-27 for Sir Topham Hatt, the Fat Controller — the one who decides
which engine runs which route and dispatches them from the sheds, which is
exactly what a launcher does at a roundhouse; runner-up was `knapford`, the
departure station) — its own binary, which is what keeps `main.rs`'s
no-flag-parser rule intact — with plain
subcommands (scriptable, CI-able) and a `ratatui` TUI over them, that:
creates and manages **named config profiles** (the thing Relay lacks);
generates the codex launch files via the existing `codex_launch` library and
the Claude equivalents via a new `claude_launch` sibling; launches an agent
with the environment fully configured in-process (secrets ride env only —
never written to a file, the `codex_launch` rule); and can hand off to
`nemo-relay claude|codex` for an instrumented chained run. This closes the M9
operator-entry-point deferral for both agents at once — one decision, as the
seam map says, not two. `claude_launch` mirrors `CodexAuthKind` exactly:
`RoundhouseKey` (turn key via `ANTHROPIC_CUSTOM_HEADERS`; no OAuth
suppressors touched) and `ForwardedClaudeLogin` (base URL + turn key only,
precondition a completed `claude` login, and the generator **refuses** a
config that also sets any of the five OAuth-suppressing inputs Dive C §1.3
enumerates — the refusal-over-silently-wrong posture `codex_launch` set).

### R9 — Pins and vigilance

- `nemo-relay-types` stays `=0.7.3`. Moving to `=0.8.0` is proven zero-cost
  (every imported item byte-identical) but this phase needs nothing 0.8.0
  adds, and a pin moves for a reason, not for freshness; the manifest's stale
  "0.8.0 final is not out yet" parenthetical is corrected with a dated note
  in the same commit as this plan. The `uuid = "=1.18.1"` ceiling and its
  unlock condition are unchanged — 0.8.0 did not relax it.
- The OpenAPI snapshot becomes a recorded pin (`spec_pin.json`: URL, our own
  body sha256, `.stats.yml`'s opaque hash, source SDK rev, fetch date,
  vocabulary), per R1 — and the **`anthropic-spec-sync` skill**
  (`.claude/skills/anthropic-spec-sync/`) is the rot-prevention loop that
  keeps it honest: discover the current spec through the SDK's `.stats.yml`,
  diff the pinned vocabulary structurally, update the pin, let the pinning
  tests produce the worklist, fix breaks test-first, and record the move as a
  dated addendum. It runs before any milestone touching the dialect and on
  whatever recurring cadence the operator sets. (Building it same-day
  corrected an evidence claim: the URL-embedded hash is an opaque Stainless
  content address, not the body's sha256 — the pin records all three
  identifiers so nobody re-conflates them.)
- The Relay family cut five releases in seven days and `0.8.1-rc.1` shipped
  hours before Dive B's read; **M11.2 re-reads the then-current release
  before the chained-topology work**, per the synergy-vigilance rule.
- The claude-version vigilance row (2.1.42 readable / 2.1.247 spawnable /
  ≥2.1.229 documented) joins the codex row in the e2e suite's version print.

## 4. The rungs

Each rung is one milestone PR (implementation + its thermo-nuclear review
fixes + documents), on a branch cut from then-current `main`, per the house
cadence. Stage briefs carry §3 verbatim.

- **M11.0 — the dialect client.** The wire module (R1) and its spec pin;
  `AnthropicMessagesClient` + SSE decoder + split-usage fold;
  `Usage`/`FrontierChunk::Done` widened and the ledger overcharge corrected
  (R2); `FrontierQuote` segments + `cache_control` at the stable boundary
  (R3); the three `main.rs` gates opened into an exhaustive match; the
  `ALLOWLIST` row and seat pass-through (R4). Tests: the
  `openai_responses_upstream.rs` pattern with a Messages SSE fixture, the
  `finding1` analog proving both usage events fold, `provider_registry.rs`
  extended across dialects, and the catalog example gaining an `anthropic`
  model entry (un-gating the example's own anticipation note).
- **M11.1 — the serve surface.** `ItemContent` extended additively;
  `messages_api.rs` + `count_tokens` (R5); session naming; oracle tier 1
  (R6). The empirical unknown this rung was to settle first is **already
  settled** — the 2026-08-27 loopback capture of the real 2.1.247 binary
  (evidence doc §5.5): the request is the **beta shape** (`?beta=true` path,
  `context_management` in the body, betas riding the header only, no `betas`
  array), `system` arrives as blocks with the attribution pseudo-header as
  an uncached block 0, and a `--continue` turn resends full history. The
  serve types accept the `BetaCreateMessageParams` property surface, and
  the conformance fixtures start from the captured bodies.
- **M11.2 — the tool loop, then the real client, both topologies.**
  *(Re-scoped 2026-08-29 by M11.1's thermo review: the serve surface can only
  answer in prose — `FrontierChunk` carries no tool-call variant anywhere in
  the system, `stop_reason: tool_use` is unreachable, and Claude Code's
  entire agent loop is tool calls — so the real-binary e2e would stall on its
  first tool turn. M11.2 therefore begins with tool-use streaming end to end:
  `tool_use` blocks on the dispatch decode, a tool-call chunk variant, the
  serve projection emitting `content_block` tool_use with `input_json_delta`
  and `stop_reason: tool_use`, and `tool_result` already canonicalizes on
  the way back in. The same `Done`/emit widening carries F1's deferred
  reporting half from the M11.1 fix round: the dispatch decoder currently
  discards the upstream `stop_reason`, so a max_tokens-truncated turn is
  indistinguishable from `end_turn` in the log — the `#[ignore]`d evidence
  test in `anthropic_messages/stream.rs` names it and un-ignoring it is the
  first step of that change.)* `claude_launch` (R8's
  library half); the gated `e2e-claude` suite (R6 tier 2) driving Direct;
  the chained topology against `nemo-relay` at its then-current release with
  the seven guards of R7 as tests or documented refusals. This rung also
  settles empirically the one UNVERIFIED link in the seat chain: which
  credential the real client presents to a custom base URL under a
  subscription login (Relay's forwarding half is proven; the client half is
  a one-capture test).
- **M11.3 — the launcher.** `topham`: subcommands, profiles, TUI,
  Relay handoff (R8). Ships last because it composes everything the earlier
  rungs made launchable, and because it is the only rung whose absence
  blocks no other.

## 5. Open questions deliberately left

1. **The ToS posture on forwarded subscription tokens** (R4's caution) — a
   product-owner call, flagged before any deployment beyond internal test.
2. **Serving `/v1/models`** — deferred with the reasoning in R5; revisit if
   surfacing routes in the client's `/model` picker becomes a product goal.
3. **The flat-tool-name `ClientDialect` arm** (Claude Code spells MCP tools
   `mcp__server__tool` flat) — owed when the MCP control surface meets a
   Claude Code client, i.e. with or after M11.1's serve surface; the arm owes
   `canonical_item` the reverse split, per `dialect.rs`'s own module doc.
4. **Resumption on the Messages surface** — `/v1/responses` has
   `starting_after`; Messages has no client-visible cursor, and R7's hazard 2
   caps what an SSE id can carry through a chained Relay. Whether resumption
   is offered in-band, out-of-band, or not at all is M11.1 design work.
5. **A roundhouse entry in Claude Code's gateway fingerprint table** — the
   response-header prefix set is closed with no generic opt-in; staying
   unlabelled versus asking upstream for a row is a relationship question,
   not a code one.
