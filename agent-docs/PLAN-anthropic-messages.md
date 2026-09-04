<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Plan: the Anthropic Messages surface, the seat, and the launcher (M11)

> **Status: shipped through M17; D2 ruled (2026-09-04).** The rulings in §3 stand
> as written; where an implementation round moved one, the dated addenda at
> the end of this document record the move and its reason, and win over §3
> for the current tree. Direction set by the product owner on
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


## Addendum (2026-09-01): M11.2b — the real client, on both topologies

M11.2 was split on 2026-08-29 (§4): M11.2a shipped the tool loop end to
end; this addendum records M11.2b, which drove a **real `claude` binary**
against the surface on both topologies and closed the rung's empirical
unknowns. Three pre-flight reads preceded the design, per R9's vigilance
rule, and each moved something:

- **Relay is at 0.8.2** (0.8.0 was the Dive B read; 0.8.1-rc.1 and 0.8.2
  landed after). The delta read (`research/nemo-relay-0.8.0-published-read.md`,
  addendum 2026-09-01) finds all five R7 hazards **holding byte-for-byte**
  — the files carrying them did not change — and two surprises a chained
  deployment must know: the gateway now **refuses a non-loopback bind**
  (`server/mod.rs:92-97`), and a new hook-request authorization gate sits
  in front of the coding-agent hook endpoints (§A.8). Neither touches the
  Anthropic route. `nemo-relay-types` 0.8.2 is byte-identical to 0.8.0 and
  still pins `uuid = "=1.18.1"`, so the `=0.7.3` pin's unlock condition is
  unmet and the pin stands (R9; a dated note joins the manifest).
- **The client line moved again.** The binary self-updated to 2.1.257 and
  the re-capture (`research/claude-code-client-surface.md` §5.7, §5.7.1)
  caught a second blocking shape: on every `--continue`, the client appends
  a trailing `role: "system"` message carrying
  `<total_tokens>N tokens left</total_tokens>` with the cache breakpoint on
  it, after the new user turn (itself now a bare string). A three-turn
  capture settled the mechanism: the notice is a **client-side counter
  regenerated per request** — the prior turn's copy is resent flattened to
  a bare string and a fresh one is appended, so the raw `messages` list grows
  by three per turn while the conversation grows by two. Under the surface as
  M11.1 left it the notice became an ordinary `System` item, and a resend
  whose `N` had moved **forked the session** — pinned by a failing test
  before the fix.
- **The spec pin moved additively**: three new open-enum beta values and one
  new path; every other vocabulary category byte-identical. Synced per the
  `anthropic-spec-sync` skill (`research/anthropic-messages-wire-crates.md`,
  addendum 2026-09-01); the new betas flow to the open arm.

### The rulings, as carried in the stage briefs

**R-A — ephemeral client notices are not log items.** The principle,
stated precisely (the first draft cited the attribution block as "dropped",
which it is not — it becomes a Developer item *loosely admitted* through
F7's leading-run replacement): a client-rewritten item is either loosely
admitted, when it leads and a run can be replaced, or dropped, when it
trails and nothing downstream can replace it. The budget notice trails,
so `wire::canonicalize` drops a system message whose entire content is
exactly the `<total_tokens>…</total_tokens>` tag, in either container,
anchored at both ends so the same tag inside the client's environment block
(configuration, kept, covered by F7) and a user message quoting it are
untouched. Cost, argued in the module doc: the model no longer sees the
client's budget figure. Both client lines stay pinned — the 2.1.251
fixtures as the prior line, the 2.1.257 fixtures (three turns) as the
current one — and the pinned tests are parameterized over both rather than
duplicated. §5.7's "undercounts by one" paragraph describes the pre-fix
surface; its §5.7.2 pointer says so.

**R-B — `claude_launch`, a pure generator beside `codex_launch`.**
`crates/roundhouse-server/src/claude_launch.rs` (R8's library half; the
binary is M11.3's). Output is an **environment map, never a file** — the
Direct surface for Claude Code is `ANTHROPIC_BASE_URL` plus
`ANTHROPIC_CUSTOM_HEADERS` carrying `<TURN_KEY_HEADER>: <key>` in the exact
syntax §1.6 says the client parses — and the turn key is a `Secret` that no
`Debug`/`Display` renders. Two auth kinds mirror `CodexAuthKind`:
`RoundhouseKey` also sets `ANTHROPIC_API_KEY` to a fixed sentinel
(`ROUNDHOUSE_API_KEY_SENTINEL`), the analog of writing `env_key` beside
`requires_openai_auth = false`: it makes the client's auth resolution
deterministic (§1.3) so an ambient login is never silently presented as if
the operator had chosen forwarding. That obligated a serve-side change:
`x-api-key` *is* a captured Anthropic seat at the edge
(`control/credential/forwarded.rs`), so the sentinel is made inert by name
and a test proves it is neither forwarded nor captured. `ForwardedClaudeLogin`
sets base URL and turn key only, refuses each of §1.3's five suppressors by
name, and returns the `must_be_unset` list M11.3's launcher enforces. One
addition beyond the brief, accepted: the three cloud-provider selectors are
refused under **both** kinds (`RedirectDefeated`), because `I7()` picks the
provider before any credential resolves and a non-first-party provider never
reads `ANTHROPIC_BASE_URL` — the sentinel would do its job and the client
would still never arrive. `CLAUDE_CODE_REMOTE=true` is the one input that
defeats the RoundhouseKey sentinel specifically (§5.7 — this box's own
container presented its managed OAuth token until the environment was fully
cleared); the review round (F3) made it a suppressor-table row that
RoundhouseKey refuses and ForwardedClaudeLogin admits, so the table carries
which kind each row defeats — eight rows, seven owed by the forwarded login
and four by the roundhouse key. One documented, unreconciled limit remains:
interactive mode prompts once before the key overrides a login. MCP wiring
for Claude Code is deferred with open question 3.

**R-C — the `e2e-claude` suite, the codex_e2e discipline verbatim.**
`tests/claude_e2e.rs` behind `--features e2e-claude`,
`ROUNDHOUSE_TEST_CLAUDE_BIN`, `--test-threads=1`, `VERIFIED_VERSION =
"2.1.257"` printed with a mismatch warning, a missing binary a loud failure,
`Command::env_clear()` then exactly the generated map plus the isolation set
(a no-binary guard asserts key-set equality on the constructed command —
it cannot see an ambient leak or a dropped `env_clear()`, which only the
real-wire seat test catches, as the refute round proved). Real: the binary,
the socket, the router over a production `ControlDirectory` with a minted key,
the log, the prefix check, the tool the client chose to run. Scripted: the
frontier. Against 2.1.257 it closes four claims only prose carried: a real
client completes a prose turn through roundhouse; **a real agent executes a
`tool_use` turn and its `tool_result` resend rejoins the same session**
(M11.2a's loop, first real-binary evidence); three `-p`/`--continue` processes
are one session with the notice never an item; and the seat-chain evidence
block — `x-roundhouse-key` beside the inert sentinel on `x-api-key`, no
`authorization`. The forwarded-login half remains the one-capture §1.3
predicts (a bearer beside the turn key); no login exists here and none may
be created for a rig.

**R-D — Direct is the reference; Chained is supported with the guards
instantiated, and the carrier is the client's own environment.** The brief
first ruled the turn key onto Relay's `[upstream] anthropic_auth_header`;
Core-B's source read overturned it: Relay injects that header **only when
the inbound request carries no credential** (`gateway/mod.rs:1070-1078`,
`already_authed`), forwards `x-api-key` untouched, **merges** its proxy token
into `ANTHROPIC_CUSTOM_HEADERS` rather than replacing it, and strips no
unknown header. So the same `claude_launch` map launched through
`nemo-relay run --agent claude`, with `[upstream] anthropic_base_url` aimed
at the deployment root and no auth header configured, lands the turn key on
the dedicated header with Direct's exact semantics — one generator, two
topologies. The upstream-layer carrier (`"Bearer <key>"`, same layer as the
base URL — hazard 4 — credential-less client only, key-authed only) is the
documented fallback. Hazards 1–3 already had unit guards from M11.1
(`wire.rs`'s Relay-alphabetized resend, `emit.rs`'s no-`data:`-frame rule,
the route ignoring `?beta=true`); M11.2b makes them real end to end. Hazards
4 and 5 are documented refusals; resumption is not offered in-band on this
surface (open question 4, closed for this rung: the emitter carries no SSE
id and hazard 2 caps what one could carry).

### The chained topology, run for real

Through `nemo-relay` 0.8.2 built from the published crate, with claude
2.1.257: **nine real-binary tests green on both topologies**. The chained
launch is the Direct launch wrapped — a `Topology` enum threads one
`build_child_command`, and a no-binary guard asserts the client's argv is
byte-identical across topologies and the chained environment is the Direct
one plus exactly Relay's four XDG state variables. At roundhouse's edge on a
chained turn: the turn key on `x-roundhouse-key` (Relay's merge preserved
it); `x-nemo-relay-source: gateway` (the proof of hop that makes every
negative beside it non-vacuous); no `x-nemo-relay-proxy-token`; `?beta=true`
intact (hazard 3, now observed rather than argued from source); the sentinel
on `x-api-key` and not captured as a seat. A `--continue` through the
alphabetizing re-encoder lands in the same session (hazard 1, now on the
wire). Two mutations proved the guards load-bearing — a trailing `/v1` on
Relay's upstream base URL and a bypass of Relay both went red. One claim
stays source-cited: that the client presented Relay's own token to Relay's
gateway — nothing in-repo observes Relay's inbound side, and a recorder in
front of Relay is a fourth process for one claim.

Relay's gateway adds eight headers to the dispatched request that no Direct
capture carries (`research/nemo-relay-0.8.0-published-read.md` §A.12):
`traceparent` and `x-nemo-relay-{agent-kind, identity-quality,
parent-scope-id, request-id, root-scope-id, session-id, source, turn-id}`,
with `session-id` equal to the client's `x-claude-code-session-id`. That is
a session and turn identity Relay asserts and roundhouse ignores — recorded
as a correlation opportunity for a later rung, not acted on here.

**The cadence, honestly.** The implementation workflow's Refute stage was
blocked by disk exhaustion (the full-workspace build filled the box) and
never mutated anything; the Fix stage freed the disk and re-ran every touched
suite green with real binaries on both topologies. The eight-mutation
adversarial pass was re-run as its own workflow before this commit, and its
rulings are in the commit message.

### What M11.2b leaves

- **M11.3 (`topham`)** consumes `ClaudeEnv::vars()` and `must_be_unset`
  verbatim; the launcher, not the generator, owns argv, the interactive
  approval caveat, and the Relay handoff.
- The forwarded-login capture on a real subscription (R4's UNVERIFIED link)
  is now a one-command test on any operator box with a login; the suite's
  evidence block is where it prints.
- Whether `N` in the budget notice ever tracks server-reported usage is
  under-determined (§5.7.1 varied only `output_tokens`); the drop rule is
  indifferent to it, which is why it was not chased.

## Addendum (2026-09-02): M11.3 — `topham`, the rulings

R8 named the crate and the split (Relay's CLI for instrumented chained runs;
a roundhouse binary for profiles, Direct launches and the handoff). What the
M11.2b round settled, and what it leaves for this rung to decide, is recorded
here so the stage briefs carry it resolved.

**R-T1 — one new workspace crate, above the server.** `crates/topham`
(binary `topham`, with a library target so its subcommands are testable
without spawning) depends on `roundhouse-server` for the two generators and
the four constants; the dependency direction the seam map worried about is
the right one — a launcher sits *above* the composition root, and `main.rs`
keeps its no-flag-parser rule because the parser lives in `topham` alone.
Three new dependencies, exact-pinned like the rest of the workspace and
commented as ordinary libraries rather than synergy dependencies:
`clap = "=4.6.6"`, `ratatui = "=0.30.2"`, `crossterm = "=0.29.0"` (toolchain
1.96.1 clears ratatui's 1.88 floor; none of the three constrains `tokio` or
`uuid`, which is what would have collided with the Dynamo pins). No
config-directory crate: XDG resolution is `XDG_CONFIG_HOME` else
`$HOME/.config`, two lines, the same rule Relay follows so an operator's two
tools agree on where profiles live.

**R-T2 — a profile names things; it never holds a secret.** A profile is a
TOML file under `<config>/topham/profiles/<name>.toml` carrying: the agent
(`claude` | `codex`), the deployment root, the auth kind
(`RoundhouseKey` | `Forwarded…Login`), the **environment variable name** the
turn key is read from (default `ROUNDHOUSE_API_KEY`, the codex generator's
`DEFAULT_KEY_ENV`), optional model slug and catalog path for Codex, and the
topology (`direct` | `chained`). The turn key itself rides the environment,
exactly as both generators already require — `codex_launch`'s "secrets ride
env only" rule and `ClaudeEnv`'s non-`Serialize` design are the constraint
this rung inherits, not a choice it makes. A profile that tries to carry a
key is refused on load, the way the generators refuse a config that sets an
OAuth suppressor.

**R-T3 — minting is a subcommand over the admin API, not a new route.**
`topham mint --profile <p>` posts to `/v1/admin/projects/{p}/members/{u}/keys`
with an admin key read from `ROUNDHOUSE_ADMIN_KEY` and prints the export
line for the profile's key variable; it writes nothing to disk. The README
deferral ("a subcommand or an admin read beside key minting") resolves to
the subcommand, because the read already exists and the deferral was about
who calls it.

**R-T4 — launch is in-process and refuses before it spawns.**
`topham launch <profile> [-- <agent argv>]` resolves the profile, builds the
env through the generator, checks `must_be_unset()` against the *operator's*
environment (the launcher inherits it — this is a real session, not a rig —
so the refusal is what stands between an ambient login and a silent
forwarding), writes the Codex files under a per-profile `CODEX_HOME` when the
agent is Codex, and `exec`s the agent with the generated variables layered on
top. `topham plan <profile>` prints the same resolution with every secret
redacted through the generators' own `Debug` (which the review round made
safe to print) and spawns nothing — it is the dry run Relay's `--dry-run`
taught this design to have.

**R-T5 — the chained handoff reuses the rig's template, moved to a library
seam.** The `[upstream]`/`[agents.*]` config the e2e rig writes in
`Rig::wire_relay` becomes a rendering in the server crate (beside the
generators, one per agent), consumed by both the rig and
`topham relay <profile>`, which writes it to the profile's scratch and execs
`nemo-relay run --agent <agent> --config <toml> -- <argv>`. The reference
chained wiring is R-D′'s: no upstream auth header, the turn key on the
client's environment. `topham relay` runs the same isolated `--dry-run`
preflight the rig runs (F8) and refuses when a system Relay layer re-aims
the upstream.

**R-T6 — the TUI is a front end over the subcommands, never a second
implementation.** `topham` with no subcommand opens a ratatui screen: profile
list, an editor for the fields R-T2 names, a plan pane rendered from the same
redacted `Debug`, and launch/relay actions that call the same functions the
subcommands do. Every action the TUI can take is a subcommand a script can
run; the TUI owns no state the profile files do not.

**R-T7 — what proves it.** Unit: profile round-trip and the secret refusal;
`plan` output snapshots for both agents and both auth kinds with the key
redacted; the `must_be_unset` refusal naming the variable. Integration: the
gated real-binary suites each gain one test that launches the real client
*through* `topham launch` on Direct (and, for Claude, `topham relay` on
Chained) — the closure R8 asked for, "the environment fully configured
in-process". The TUI is exercised by its model, not its terminal: the
screen's state transitions are pure functions over key events, tested
without a backend.

**Left open, on purpose.** A `topham` home for MCP wiring waits on open
question 3 (the flat-tool-name dialect arm); `/v1/models` stays unserved
(open question 2); the interactive-approval limit on `RoundhouseKey` under
a subscription login is documented in `topham plan`'s output, not solved.

### What the implementation settled beyond the rulings (2026-09-02)

Seven decisions the stage briefs did not make were flagged rather than made
silently, and each is accepted here with its reason:

- **`topham launch` sets `DISABLE_AUTOUPDATER=1` and
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` on a Claude child** (one
  const, `CLAUDE_DEPLOYMENT_POLICY`, one test). Deployment policy on an
  operator's own session, accepted because the client line moved twice in
  five days under this phase's own captures (§5.6, §5.7) and a session
  pointed at roundhouse has no business updating itself mid-turn or
  reporting home; `claude_launch`'s doc already named the launcher as the
  place these are set. Reversing it is the const and the test.
- **`topham mint` takes `--project` and `--user`.** The mint route is under a
  membership and a profile carries no tenancy; storing one there would be a
  second copy of a tenancy edge.
- **A wrong admin key is 401 for an unknown secret and 403 for a known one of
  the wrong kind**; the mint suite pins both.
- **Chained Codex is rendered but not credential-correct — a documented
  limit, not a refusal.** A real 0.8.2 `--dry-run --agent codex` shows Relay
  splicing `--config model_provider="nemo-relay-openai"` onto codex's argv,
  and a codex `--config` override outranks the generated `config.toml`, so
  the turn key the config puts on the dedicated header is not what the
  client presents; roundhouse admits a credential-less turn and degrades to
  local-only routing. Stated in `topham plan`'s notes and `relay.rs`'s doc
  (evidence: `research/nemo-relay-0.8.0-published-read.md` §A.14). The
  remedy is the upstream-layer carrier (key-authed only) or a Relay change
  that lets a generated config win; neither is this rung's.
- **`topham relay` does not isolate Relay's XDG state** (the rig does): an
  operator's `plugins.toml` — exporters, pricing, PII — is the point of
  chaining. The cost is that the isolated preflight and the inherited launch
  resolve under different environments; the gap is exactly the
  `NEMO_RELAY_*_BASE_URL` layer, which is why a second refusal
  (`UpstreamOverriddenByEnv`) exists for it.
- **`--relay <path>`** names this box's binary and is not a profile field;
  a profile names things about a deployment.
- **The TUI uses `try_init`/`restore`, not `ratatui::run`**, because `init`
  panics without a tty; a piped `topham` now refuses naming the subcommands
  and writes nothing to stdout. `topham relay`'s banner goes to stderr —
  the chained closure test caught it corrupting `-p --output-format json`,
  the first defect this rung found by a test rather than by reading.

One observation for a later rung: `LaunchValue::Declared` is unreachable on
a successful `topham` resolution (the generator refuses every declared
suppressor), so its redaction is exercised only by `claude_launch`'s own
tests; an operator-supplied pass-through variable would make it live.

### What the review round changed about the launcher's contract (2026-09-02)

The M11.3 thermo-nuclear review (twenty-five findings, all valid or
partially valid; rulings in the commit message) moved four things an
operator would notice, recorded here because R-T2/R-T4 read differently
without them:

- **Settings files are read, and can refuse a launch.** The client applies
  a settings-file `env` block over the process environment, so a block left
  by another tool (Relay's persistent install writes `env.ANTHROPIC_BASE_URL`)
  silently re-aimed a Direct launch with nothing reporting it. `topham
  plan`/`launch` now read `$CLAUDE_CONFIG_DIR/settings.json`,
  `./.claude/settings.json` and `./.claude/settings.local.json` and refuse,
  naming the file and key, when an `env` block would override a generated
  variable or set a suppressor; managed settings are not read, and the doc
  says so. This is the launcher's first ambient read below the environment.
- **A credential beside the sentinel is refused under both kinds.** R-B
  refused the five OAuth suppressors only under the forwarded login; a
  `RoundhouseKey` launch beside an ambient `ANTHROPIC_AUTH_TOKEN` was
  admitted, the client put it on `Authorization`, and the edge captured it
  as the caller's seat — the profile promised a turn key and nothing else
  and delivered the operator's gateway token upstream. The suppressor
  table now carries a per-row "refused beside the sentinel" flag
  (`ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR`,
  `apiKeyHelper`); `ANTHROPIC_API_KEY` stays admitted because the
  generated sentinel overrides it.
- **A profile is scanned before it is parsed.** A paste that was not TOML
  echoed the pasted key through the parse error; the scan now runs on the
  raw text, over keys and values, as a substring test, and a parse error
  renders position only.
- **The screen's relay action runs the real preflight before it closes**,
  which costs writing the Relay config before the operator commits and a
  second `nemo-relay` spawn on launch; the alternative was a refusal the
  operator could not read. `topham --version` names the commit it was
  built from, and the suite warns when that is not HEAD — the stale-binary
  hazard the README named is now detected rather than described.

## Addendum (2026-09-02): M12 — the MCP control surface for Claude Code

Open question 3 is answered by capture (`research/claude-code-client-surface.md`
§5.8; `research/anthropic-messages-seam-map.md` §6), and the answer changes
the shape of the rung the question anticipated. The product sentence's
"through it, take advantage of NeMo Relay and Switchyard" is, for Claude Code,
still false after M11: a launched client reaches `/v1/messages` and never
`/mcp`, so the validate/steer loop, `declare_intent`, `status` and the
routing overlays are unreachable from the one agent this phase brought in.
M12 closes that.

**What the client does (2.1.257, observed).** An HTTP MCP server is taken
from `--mcp-config` (inline JSON or a file) or a project `.mcp.json`;
`settings.json`'s `mcpServers` is inert; `--mcp-config` wins outright over
`.mcp.json` and `--strict-mcp-config` ignores every other source. A
`headers` map is honoured on every MCP request, and a value spelled
`${VAR}` is expanded from the client's own environment — unset, it is sent
as the literal string with no warning. In the Messages request the tools
are spelled flat, `mcp__<server>__<tool>`, `inputSchema` renamed
`input_schema`, description verbatim. On `tools/call` the client sends the
bare `<tool>` with `_meta.{"claudecode/toolUseId": <the tool_use.id>,
"progressToken"}`, and the JSON-RPC result flows into the resend's
`tool_result.content` byte for byte. In `-p` mode a tool is called only when
`--allowedTools` names its flat name; `--permission-mode dontAsk` denies,
and `--dangerously-skip-permissions` is refused under root. Correlation at
the transport level is only the server-issued `Mcp-Session-Id`, which
roundhouse's stateless MCP does not issue.

### The rulings

**R-M0 — the log's "bare neutral name" is verified before it is relied on.**
`dialect.rs` rules that the log stores a bare tool name and applies the
spelling on the way out, and owes the flat arm a reverse split on the way
in. But `is_control_call` matches the *flat* `mcp__roundhouse__` prefix on
the *stored* name, the validate suites use only flat names, and the seam
read could not find a runtime rendering site the enum forces. Either Codex
sends flat names on the Responses wire — in which case the log already
stores flat names, the separate-`namespace` handling is dead, and the
"neutral name" is a doctrine the code never enacted — or it sends bare
names with a `namespace` field, in which case control-traffic exclusion has
been silently inert on the Responses path since M10. M12's first stage
settles which, from the pinned codex source (`6344a65`) and the M9 harness
captures, and writes the answer into `dialect.rs`'s own doc. The rest of
this addendum is written for both outcomes and says which branch each
ruling takes.

**R-M1 — the Messages surface stores the flat name the client spells; the
dialect is per surface, not per deployment.** A `ClientDialect` arm for
Claude Code (`ClaudeMessages`) whose inbound canonicalization keeps
`mcp__<server>__<tool>` whole. Reasons: nothing renders a tool call
outbound any more (the steer is text), so the only consumer of the stored
name is `is_control_call`, which wants the flat prefix; splitting would move
`turn_id` hashes for every tool-using session already stored; and a session
is written by one client — the cross-dialect resumption `dialect.rs`
guarded against is not a scenario this product has. If R-M0 finds Codex
also spells flat on the wire, the same rule is simply made explicit for
both surfaces and the dead `namespace` handling is retired with a test; if
Codex spells bare, the Responses arm keeps its behaviour and
`is_control_call` gains a Responses-side recognizer (bare name plus the
namespace the deployment configured) with the failing test that proves it
was inert. The dialect is stamped where the Messages handler already stamps
one for tools (`messages_api.rs`), never read from the deployment-wide
`mcp_namespace`, which cannot serve two clients at once.

**R-M2 — MCP calls from Claude Code are correlated by the tool-use id, and
`latest` is the fallback, never the rule.** The `tool_use.id` in
`_meta["claudecode/toolUseId"]` is an id roundhouse itself emitted and
stored as the call's `call_id`; a `tools/call` carrying it resolves to
exactly the session and turn that asked, with no race between a parent and
its subagents over the principal's `latest` slot and no node-locality. The
MCP surface reads it on every call; absent (Codex, which sends no such
key), the existing `latest` path stands.

**R-M3 — the launcher injects the MCP registration as inline argv, and the
key rides `${VAR}`.** `topham launch` for a Claude profile prepends
`--mcp-config '{"mcpServers":{"roundhouse":{"type":"http","url":"<root>/mcp",
"headers":{"x-roundhouse-key":"${<key variable>}"}}}}'` to the operator's
argv; no file is written and the secret appears in neither argv nor file —
the client expands the variable from the environment `topham` already
laid. `--strict-mcp-config` is a profile switch (default off: an operator's
own servers coexist). `topham plan` shows the registration with the
variable unexpanded. The unexpanded-literal hazard is closed by the refusal
that already exists: an unexported key variable refuses the launch before
anything is spawned. This adds the argv seam `LaunchPlan` lacks — a
generated leading argv, distinct from the operator's verbatim tail — which
signage (R-M4) uses too.

**R-M4 — signage rides `--append-system-prompt`, not a file.** Of the three
places text can land (an appended system block → loosely admitted
Developer configuration; `$CLAUDE_CONFIG_DIR/skills` → a strictly admitted
interior system message that forks a session when it changes, and owning
the config dir evicts a forwarded login; `CLAUDE.md` → the first user
message, in the operator's repo), only the first is safe. `claude_launch`
renders the Claude analogue of `codex_launch::skills` as one text — the
eight control tools, when to reach for each, spelled flat — and `topham`
appends it. The tool descriptors' `conversation` wording, which speaks
Responses vocabulary (`prompt_cache_key`), is made dialect-neutral once;
the golden pin moves with it and the prompt-cache cost is accepted.

**R-M5 — the Messages surface accepts the client's own MCP tools and adds
none of its own.** `tools[]` is taken verbatim today and stays so; roundhouse
never injects a tool the client's loop has no dispatcher for, and
`admitted_input_tokens` therefore stays what the client declared.

**R-M6 — what proves it.** Unit and surface: the flat name round-trips
through `canonicalize` and back out of the log unchanged on the Messages
side, with the captured `mcp-turn-1` / `mcp-turn-2-toolresult` fixtures as
the pins; the `is_control_call` outcome for a flat Claude call and for
whatever R-M0 found on the Responses side, each with the failing test that
motivated it; the MCP surface resolving a `tools/call` by tool-use id, and
falling back to `latest` without one; `topham plan` snapshots carrying the
registration and the signage. Closure: a real claude 2.1.257 launched
through `topham launch` with `--allowedTools mcp__roundhouse__status`, a
scripted upstream answering a `tool_use` for that tool, and assertions at
both edges — the MCP call arrived at `/mcp` with the turn key and was
correlated to the session by tool-use id; the resend rejoined the same
session; the log holds one flat-named call and its result; the validate
fold counted no control call.

**Left open, on purpose.** Chained MCP through Relay (Relay proxies only
the Anthropic route; an MCP server behind it is reached directly — stated,
not tested); `/v1/models`; the interactive-approval limit.

### What the implementation settled beyond the rulings (2026-09-02, M12)

- **R-M0 settled as bare-with-namespace, and it was a defect.** At codex pin
  `6344a65` an MCP server reaches the model as one `namespace` object
  (`mcp__roundhouse`) whose tools carry bare names, the model's call comes
  back as two wire fields (`name: "status"`, `namespace: "mcp__roundhouse"`),
  and dispatch is an exact `ToolName{name, namespace}` lookup — a flat
  spelling is unresolvable, not merely unused. The log therefore stores
  `status`, and `is_control_call`, which matched only the flat prefix, had
  been inert on the Responses path since it was written: the validate fold
  counted roundhouse's own control traffic as agent work (the G04 failure)
  and every fixture hand-wrote flat names, which is why nothing was red. A
  premise of `dialect.rs` fell with it: nothing renders a tool call
  outbound (`ControlPlane::client_dialect()` has had no caller since
  M10.0), so "the spelling is applied on the way out" described a rendering
  that does not happen; the doc now says so.
- **The Responses-side recognizer is an exact match on the control tool
  names, not "bare name plus configured namespace" as ruled**, because the
  namespace is not in the stored record and there is nothing to compare a
  configured value against. One list (`CONTROL_TOOL_NAMES` in core,
  re-exported as the MCP surface's `TOOL_NAMES`) serves the fold and the
  surface. The cost is pinned by a test rather than hidden: a third party's
  MCP tool literally named `status` arriving bare over the Responses wire is
  exempted from the task view with ours — an under-count of a call or two,
  against G04's over-count of all our chatter. Closing it properly means
  keeping the namespace in the stored record, which moves the `turn_id` of
  every stored tool-using session; deferred as a migration question, with
  the test named to delete when it lands.
- **A foreign or unknown tool-use id falls through to the caller's own
  `latest`**, never the other tenant's session and never a distinguishable
  refusal — the same anti-oracle reasoning `resolve_session` already applies
  to a foreign conversation name. The binding is written by the Messages
  follower only (Codex sends no such key); the call table is node-local and
  capped, and an evicted binding costs one call falling back to `latest`.
- **The `${VAR}` expansion the registration relies on is observed, not
  assumed**: the seam read captured an expanded header value at a loopback
  MCP server, and the closure test asserts the minted key — not the literal
  — arriving at `/mcp` from a real client launched by a real `topham`.
- **The tools' `conversation` argument cannot resolve on the Messages
  surface**: the MCP surface qualifies a named conversation through the
  Responses namespacing, while a Messages session is keyed
  `anthropic_messages/<id>`. Moot for Claude Code because the tool-use id
  answers the question and the descriptors now steer away from the field;
  left open with two remedies (qualify both spellings, or carry the caller's
  dialect into the MCP surface) for a later rung.
- **The closure test proves the loop, not the signage.** Its scripted
  upstream emits the `tool_use` directly, so removing `--append-system-prompt`
  leaves it green by design; the signage is pinned by the generator's own
  argv tests and the plan snapshots. Chained MCP through Relay stays stated,
  not tested. Every rig in the gated suite now mounts the MCP router beside
  the Messages router.

### What the review round changed (2026-09-02, M12)

The M12 thermo-nuclear review (fifteen findings, thirteen valid and two
partially valid; rulings in the commit message) moved four things the
rulings above state differently, recorded here so R-M1 and R-M2 read
correctly for the current tree:

- **The control-call recogniser is parameterised by surface, and the fold
  learns the surface from the session it folds.** R-M1's Responses-side
  recognizer was an exact match on the control tool names *regardless of
  surface*, so a Messages client's own tool literally named `status` was
  folded out of the task view with roundhouse's (F8). `ControlCallDialect
  ::{CodexResponses, ClaudeMessages}` now gates the recogniser
  (`is_control_call_on`, `task_exchanges_on`): the Messages dialect accepts
  only the flat spelling and the Responses dialect only the bare names, and
  the session key (`anthropic_messages/…` versus the Responses namespacing)
  tells the engine which to hand the validate fold. The third-party-`status`
  cost R-M1 pinned is therefore narrowed to the Responses surface, where the
  namespace is genuinely not in the stored record.
- **The `mcp_namespace` knob is retired, not stamped.** It was validated and
  documented and read by no runtime path — every spelling is
  `mcp__roundhouse` by construction, shared by both launchers, the signage
  and the fold (F2). A control-plane config naming it is refused at load,
  `ClientDialect` is a fieldless enum, and `ControlPlane::client_dialect()`
  is gone; R-M1's "stamped where the Messages handler already stamps one"
  and R-M0's note about that function's missing caller are both moot.
- **The call table is per principal and keyed by principal.** A colliding
  upstream call id across two sessions of one principal (a local backend
  numbering calls per response) answered the later writer confidently
  (F14); a second binding of one id to a different session now marks it
  ambiguous, which resolves exactly as an unknown id does — the caller's own
  `latest`. The remembered-calls cap was one node-wide queue a co-tenant
  could exhaust (F15); it is per principal, which raises the node's bound to
  the cap times the principals served and never reaps a quiet principal —
  stated, and left with M8's durable-mapping question. The resolution order
  (named > tool-use id > latest) lives in one shared function the real
  reads and the test fake both call (F4).
- **A key variable naming a generator-written variable is refused** (F9):
  `${ANTHROPIC_API_KEY}` under a roundhouse key expanded to the sentinel,
  every control call was rejected, and inference kept working, so nothing
  said why. `PlanError::KeyEnvIsGenerated` names it at resolve. The
  generated flags are structured pairs rather than literals (F5), so the
  launcher's collision refusal no longer guesses which argv entries are
  flags.

## Addendum (2026-09-02): M12.1 — Codex's `_meta.threadId` on the same seam

M12 read a client-native correlator for Claude Code
(`_meta["claudecode/toolUseId"]`) and made the principal's `latest` the
fallback rather than the rule. The standing deferral of Codex's
`_meta.threadId` (`roundhouse-mcp/src/lib.rs`, "deferred rather than
pending") rested on a premise M12 changed: that reading a client-native
shortcut would bind the surface to one client's conventions and that
`init_session` was the client-agnostic path. The surface now reads one
client's convention; refusing the other's leaves Codex's subagents in the
`latest` race M12 closed for Claude. The rung is small and well-defined.

**R-M7 — `_meta.threadId` is the conversation the client names.** Codex
stamps it on every `tools/call` and it is byte-identical to the turn's
`prompt_cache_key` (M9 capture), which is exactly what the tools'
`conversation` argument already means on the Responses surface. So the
transport reads it beside the tool-use id and hands it to the shared
resolver as a *named* conversation, qualified into the caller's namespace
and tenancy-checked by the existing path. Order in
`session_without_a_name`'s caller: an explicit `conversation` argument, then
the client's own correlators (threadId as a name; tool-use id as a call),
then `latest`. **When a caller's own inputs disagree** — an explicit
argument naming one conversation and a correlator resolving to another —
the call is refused naming both: this is the caller contradicting itself,
not a tenancy oracle, so loudness costs nothing and hides a client bug from
nobody.

**R-M8 — what proves it.** Hermetic: a Codex-shaped `tools/call` (`_meta`
with `threadId` and the `x-codex-turn-metadata` sibling the capture shows)
resolves to the named conversation with a rival `latest` in front of it;
a foreign or unknown threadId falls through as an unknown correlator does;
a threadId and an explicit `conversation` that disagree are refused with
both named; a Claude-shaped `_meta` is unaffected. The gated codex suite
gains the real-binary counterpart (ignored on this box — no codex binary —
with its doc saying so). The deferral paragraph in `roundhouse-mcp/src/lib.rs`
is replaced by the current contract; README's `_meta.threadId` sentences
say what exists.

### What the implementation settled beyond the rulings (2026-09-02, M12.1)

- **Correlator-versus-correlator disagreement is ordered, not refused.** R-M7
  named the refusal for an explicit argument against a correlator — the model
  and the client answering separately. Two correlators (`threadId` and the
  tool-use id) are one client naming one call in two vocabularies, so the
  thread wins silently and the argument is compared against the effective
  correlator; no shipped client sends both, and the behaviour is pinned by a
  test rather than left implicit.
- **Two assertions M12 shipped were R-M7's disagreement case.** They had
  asserted that a `conversation` argument outranks a tool-use id resolving
  elsewhere ("an argument the model wrote outranks metadata the client
  attached"); both are rewritten to the agreeing case with the change
  recorded beside them.
- **The Codex real-binary counterpart is ignored for two reasons, and a
  binary lifts only one.** This rig's upstream echoes and emits no tool call
  (the M10.0 T7 ruling), so a runnable version needs a scripted upstream
  emitting the namespaced `function_call` codex 0.146.0 routes to MCP — a
  wire shape not established anywhere in this tree. The test states both
  unlock conditions and fails loudly rather than passing vacuously.
- **A named call costs one extra store round trip**: detecting a
  contradiction means resolving the client's correlator even when the
  argument would have decided. Recorded where the order lives; `latest` stays
  lazy. `Caller` became a builder so two adjacent optional correlators cannot
  be transposed, and one reader serves both `_meta` keys.

### What the review round changed (2026-09-02, M12.1)

Nine findings from two lenses, eight valid and one partially valid, every
one ruled test-first and every fix re-broken. Two of them moved a ruling.

**R-M9 — the thread id is bound where the turn is served, so subagents
resolve exactly.** R-M7's premise — `_meta.threadId` is byte-identical to
the turn's `prompt_cache_key` — holds only for a codex *root* thread. At
the pinned checkout a subagent's turn sends the root's `session_id` as
`prompt_cache_key` (`agent/control.rs:104-110`, `session.rs:671-676`) while
stamping its *own* thread id on every `tools/call`, so under R-M7 alone a
subagent's thread id named nothing and fell through to `latest` — the race
the rung claimed to close, untouched for the topology that motivated it
(review F2). The marker that closes it is already on the wire: every codex
turn carries `x-codex-turn-metadata`, a JSON object whose `thread_id` is the
turn's own (`responses_metadata.rs:281`, `turn_context.rs:618-622`). The
Responses ingest binds that id to the session the turn was bound or forked
to, per principal, in a thread table on `Conversations` — bounded per
principal like the call table, but with rebinding as the normal case and no
ambiguous state, because a thread's session legitimately moves on every
fork. The binding is written in the Responses surface's own `bind`, after
`bind_prefix` has decided the session, rather than inside `bind_prefix`,
which the Messages surface shares: a codex header is one dialect's
vocabulary and does not belong in the one function both dialects must agree
in. `ControlReads::session_of_thread` is a defaulted read, like
`session_cursor`, so a deployment without the table is one that never
binds a thread. The thread arm of the resolver
reads the table first (exact, no store round trip), then R-M7's named path
(a root thread's id is its cache key, and a client without the header still
gets it), then falls through as before. What still falls through, stated:
a node that never served the thread, and an evicted binding. The header is
untrusted input — bounded, parsed leniently, used only as a lookup key
partitioned by principal, never for tenancy.

**A key this node never bound is refused, not guessed at generation zero**
(F9). `Conversations::resolve` used to answer generation zero for any key
it had no entry for, which on a node that served none of the conversation's
turns served a stale pre-fork session with `isError: false` — the quiet
wrong answer the module doc promised never to give. Never-bound now refuses
with the same shape as unknown and foreign (the anti-oracle collapse), and
the doc's "a restart is survivable" paragraph says what it now costs: a
named call outside any served turn refuses after a restart, exactly as
`latest` already did.

**The resolver is one provided method** (F1, F4, F5, F8). The order, the
`ForeignConversation`-only swallow on the thread arm, the contradiction
refusal and the `NoSession` fallback were spelled in the real reads and
re-spelled, differently, in both test doubles (`.ok()` swallowed a store
outage too). `ControlReads` now requires only the table reads and provides
`resolve_session`; the rationale has one home, in that method's doc, and
the guard pins it there. An argument and a thread id that are the same
string resolve once. The correlator pair is one owned `Correlators` type
carried from the transport to the resolver — the builder M12.1 introduced
answered a transposition risk that named fields already answer.

**Open, recorded rather than left in an ignored test** (F6): the codex
real-binary counterpart of R-M8 left `codex_e2e.rs`'s test pool — its
assertions stay in the file as a plain function, so they are compiled and
not run. It had been ignored
for two reasons with a bespoke reason string, so on a box with a codex
binary the suite's sanctioned `--include-ignored` run failed it by design.
Unlock conditions, both still true: a scripted upstream that emits the
namespaced `function_call` codex routes to MCP, and a `Rig` constructor
that accepts an upstream at all. A guard now pins that every ignore in that
suite carries the one uniform reason.

Housekeeping: `mcp_api.rs`'s test module moved to a sibling file as the
crate's other large modules do (F3); the test fake models a store set and a
`latest` map as two tables rather than a union, and can stage a store
outage (F7, F1); R-M7's text above named `session_without_a_name`, which
was the M12 name for what is now the provided `resolve_session`, and the
"`Caller` became a builder" note above is superseded by F5.

Two things confirmed in passing and handed to the D1/M8 design round rather
than widened here: after a restart, `bind` re-derives generation zero, a
client history that disagrees with that log forks to a re-derived `#g1`,
and the fork arm appends the claimed history whole on the premise that a
fresh session is empty — false when a pre-restart `#g1` log already holds
it, so the cost is a duplicated prefix, not a wrong session; and
`Conversations::generations` now holds one entry per cache key this node
serves rather than one per key that forked, which is the honest price of
"never bound" meaning something — the same growth profile the store already
carries a log for, and process state the durable mapping replaces.

## Addendum (2026-09-02): M13 — the Redis fair-use ledger, the rulings

`control/fair_use.rs` deferred its Redis implementation by name with the
unlock condition written down: fair use across nodes is only true with
shared buckets, and the one undecided question was the key layout. This
rung lands it. (Recorded in `PLAN-anthropic-messages.md` because the loop
that drives this branch lives here; the frontier plan's 2026-08-24 addendum
is the ruling it implements.)

**R-F1 — one hash per (scope, bucket), expired by Redis, summed by Lua.**
Bucket-per-key at `BUCKET_MS` (five minutes), keyed by scope (the project,
and the member) and bucket index, holding two integer fields — tokens and
micro-dollars, so `HINCRBY` stays exact and the f64 the trait speaks is
converted once at the edge with the rounding stated. `PEXPIRE` at the
widest window plus one bucket makes Redis the pruning pass the hash-per-scope
layout would have needed and nothing owns; an idle scope costs nothing.
`record_draw` is one script: two `HINCRBY` on each of the two scopes plus
the expiry, one round trip. `would_exceed` is one script: the bucket keys
for each window are built server-side from the caller's `now_ms` and
`BUCKET_MS`, summed cheapest-window-first (5h, then 24h, then 7d) for both
scopes, compared against the caps, and the narrowest refusing window and its
earliest retry — the walk from the oldest bucket forward — returned exactly
as the memory ledger computes them. The memory ledger's arithmetic is the
specification; the two implementations pass one contract.

**R-F2 — the clock is the caller's, as the spend ledger already ruled.**
`at_ms`/`now_ms` stay caller-supplied (the trait says why: a window boundary
must be reachable in a test without waiting for one); the scripts never read
Redis `TIME`, matching `RedisSpendLedger`'s documented departure from the
`scripts` module convention, for the same reason.

**R-F3 — the composition root chooses by the same rule as sessions and
spend.** With `ROUNDHOUSE_REDIS_URL` set and a `fair_use` block configured,
the Redis ledger is wired and the single-node boot warning goes; without
Redis the memory ledger and the warning stay — the honesty mechanism is not
weakened, it is satisfied.

**R-F4 — what proves it.** A shared contract suite the memory ledger already
passes, run over the Redis ledger against a real Redis (gated on
`ROUNDHOUSE_TEST_REDIS_URL` with `--include-ignored`, the store crate's
discipline; a missing URL fails loudly). The unlock condition itself as a
test: two ledger handles over one Redis, a draw through one refused by the
other. Bucket boundaries, window roll-over, the retry time, expiry of a
stale bucket, and the two-scope update from one call, each against the real
server. The `redis` crate is a watched dependency: no version moves here.

### What the implementation settled beyond the rulings (2026-09-02, M13)

- **The contract is a macro both crates instantiate.** The memory ledger's
  behavioural tests moved into `control/fair_use/contract.rs` as
  `fair_use_ledger_contract_suite!`, in the spend-contract idiom; the
  memory ledger runs it unchanged and the Redis ledger runs it against a
  real server, gated on `ROUNDHOUSE_TEST_REDIS_URL` with a missing URL
  failing loudly. One new assertion the memory ledger never had — the
  retry time names the oldest bucket that has to age out — exists because
  every other test records a single draw and would not tell a walk from
  the newest bucket apart from one from the oldest.
- **Both scopes hash to one slot.** The project's key is the cluster hash
  tag for the member's, so one script touches one slot on a cluster, and a
  unit test pins that two projects do not share one.
- **Two divergences from the specification, documented rather than
  hidden.** The script sums buckets from the window's first index through
  `now_ms / BUCKET_MS`, where the memory ledger's range is open at the top;
  the difference is reachable only when a draw is stamped after the
  `now_ms` of a later check — a clock going backwards across nodes by more
  than the remainder of a bucket — and closing it would narrow the memory
  ledger, which is changing the specification to fit an implementation. The
  Redis counters are `i64`, so a draw past `i64::MAX` tokens or
  micro-dollars is refused where the memory ledger saturates; the contract
  asserts the shared middle of the range and each end is asserted where it
  belongs. Sub-micro-dollar draws round to zero, half away from zero, at
  the edge — the only information this backend loses.
- **The composition root's predicate is a function, not a condition.** The
  refute pass found one mutation nothing caught: the boot warning's real
  `if` in `main.rs` was never exercised, because the test that claimed to
  cover it re-derived the predicate in a local closure. `fair_use_backend`
  chooses the ledger and `fair_use_warning_owed` decides the warning; the
  boot site calls both and the test calls the same functions.
- **What is not covered, stated.** No integration test boots the binary
  against Redis with a `fair_use` block — the composition-root choice is
  covered by unit tests on the two predicates. One gated test sleeps 400 ms
  through a test-support TTL seam, because key expiry is the one clock this
  ledger does not own; the production TTL is asserted without sleeping via
  `PTTL`. There may be only one `INFO commandstats` measuring loop per test
  binary — two such tests are each other's competitor on a server-wide
  counter — and the reason is written beside the one that exists.
- **Refute by mutation against the real server**: nine mutations — drop the
  expiry, sum one scope, the oldest bucket off by one in both directions,
  widest window first, retry from the newest bucket, `TIME` instead of the
  caller's clock, two handles with different prefixes, floor instead of
  round at the edge, and the boot warning kept beside a wired Redis; eight
  went red under a named guard and the ninth became the predicate above.

### What the review round changed (2026-09-02, M13)

Ten raw findings from two lenses, seven after triage, every one ruled valid
test-first against a real Redis, every fix re-broken. Two moved a ruling and
one opened a rung.

**R-F5 — one integer domain, bounded, saturating, shared by both ledgers.**
R-F1 named the memory ledger's arithmetic as the specification, and the
memory ledger accumulated dollars in `f64`: seventy cents plus ten cents
under an eighty-cent cap admitted in memory and refused in Redis, and a
differential fuzz put the two backends' retry times up to thirty-two hours
apart (F3). Money was never defensibly specified in floating point, so the
specification is now the exact one and the memory ledger adopts it: tokens
and micro-dollars are integers in both, the trait's `f64` is converted once
at the edge by one function in core that both crates call (`DrawCounts::of`,
half away from zero), and a window sum, a cap and a retry walk are computed
in integers everywhere; a cap above the domain clamps into it rather than
being refused, so a saturated window meets it. The domain
is bounded at 2^53 for both fields — a single draw past it is refused at the
edge before any write, and a window sum saturates there — because every
integer up to 2^53 is exact in a Lua double and the sum of two such is exact
below 2^54, so the record script reads, adds with a clamp and writes back
in one script with no command left in it that can fail. That closes F5 (an overflow on the member's bucket had left the
project's already moved — the script's "one indivisible step" was a claim,
not a property) and F7 (amounts crossed the boundary as decimal strings for
a reason that did not hold, and were summed as doubles anyway), and retires
the M13 addendum's "divergence 2": neither ledger refuses at `i64::MAX` now;
both saturate at the same bound.

**R-F3′ — the fair-use ledger follows the Redis URL alone.** R-F3 had made
the Redis ledger conditional on a `fair_use` block being configured *at
boot*, and the admin plane accepts one after boot: a deployment booted with
Redis and no ceiling enforced every later-patched ceiling per process, with
nothing in Redis and no warning (F1). The predicate's stated justification
— sparing a ceiling-less deployment a boot failure on an unreachable Redis
— was false, since the session store fails boot on the same URL first. The
ledger is now chosen by the same rule as sessions and spend, the URL and
nothing else — `fair_use_backend` lives in the library so the gated boot
test calls the predicate the boot site branches on rather than re-deriving
it, which is the shape the M13 refute pass had caught catching nothing — and
the single-node warning follows the ceiling rather than the boot snapshot:
`MemoryFairUseLedger` warns once, the first time it is asked to enforce a
non-empty ceiling, because it is the one thing that knows both halves
(a ceiling is being enforced; these counters are per-process). The
boot-time `fair_use_warning_owed` predicate the M13 addendum describes is
gone. One ordering consequence, recorded at the call site: the fair-use
ledger is resolved before the store match, so an unreachable Redis now fails
boot naming the fair-use ledger rather than the session store — both name
the variable the operator acts on.

**M13.1 — the read path, opened rather than rushed** (F4). The module doc
said an admitted turn costs sixty-one reads because the five-hour window
binds first; an admitted turn is the common case and it is exactly the one
where no window binds, so the scan widens to the widest configured window —
2017 buckets per capped scope under a seven-day cap, measured at about
eight milliseconds of blocking Redis time per admission. The claim is
corrected in both docs and the true count is pinned inside the file's one
`INFO commandstats` test as a derived expression, so the rung that lowers
it turns that assertion red; the fix is a redesign of
the read path — per-window running sums maintained on write, aged-out
buckets subtracted on read from a decay pointer, the bucket scan reserved
for computing a retry time on refusal, amortised O(1) per admitted turn —
and it ships as its own rung with its own contract and review rather than
inside a fix stage.

The rest: the script learns its window count from its arguments and the
Rust side passes `FairUseWindow::ALL.len()`, so a fourth window cannot
compile without reaching the script (F6); three doc sites in `engine.rs` and
`mutation.rs` that still called the Redis ledger deferred say what exists,
and the PATCH-axis constraint records that the bucket-per-key layout is what
satisfied it; those doc guards now read the working tree, since a pinned
blob can never observe a fix (F2). The M13 addendum's 'divergence 2' and
its `fair_use_warning_owed` sentence are superseded by R-F5 and R-F3′
above.

## Addendum (2026-09-02): D1 ruled — the rungs it opens

The state-spectrum design round the frontier plan deferred (R10) has
ruled; the ruling is `PLAN-frontier-selection.md`'s 2026-09-02 addendum
(R11–R15) and its evidence is the three D1 documents under `research/`.
For this branch it settles the M12.1 handoffs and the shape of what comes
after M13.1:

- **M14.0 — the fork arm admits** (R13). `bind_prefix`'s fork arm runs the
  same admission against the forked-to session's log instead of assuming
  it empty; an agreeing log continues, a disagreeing one forks again, and
  a bounded number of disagreements refuses loudly. Test-first: a
  restart-then-fork whose re-derived `#g1` already holds history no longer
  duplicates the prefix. Well-defined; ships alone.
- **M14.1 — durable generations, calls and threads** (R12). Three maps in
  the store crate beside the spend and fair-use ledgers, one shared
  contract the memory implementation passes first, `Conversations` reading
  through on a node's first touch of a key and writing through on a fork
  and a bind; the M12.1 "never bound" refusal widens from this node to
  anywhere. With it, `_meta["x-codex-turn-metadata"].session_id` is read
  so a codex control call at generation zero needs no table at all.
- **M14.2 — staleness bounds and key discipline** (R14). A TTL beside the
  capacity cap on the call and thread tables; a declared namespace and a
  schema version in every shared-store key, rejected when empty.
- **Folded into M13.1**: the ledger-outage posture — a ceiling check that
  cannot reach its store fails closed with a retryable refusal, a draw that
  cannot be recorded fails open with the reason logged — pinned by tests
  beside the read-path redesign.

Not scheduled here: the durable admin directory (R15, M8-owned) and a
cross-node aggregator for the metrics fold.

### What the implementation settled beyond the rulings (2026-09-02, M14.0)

- **The bound is eight, and the refusal is a 409.** `MAX_PREFIX_FORK_ATTEMPTS`
  [renamed `MAX_PREFIX_PROBES` in the review round below — nothing forks
  per attempt any more] is the number of generations one request may
  disagree with before
  `prefix_admission_exhausted` refuses it, naming the cache key and the
  count; a client that has rewritten its history more times than that
  inside one request is a loop, not a client. The refusal is a conflict,
  not an internal error: nothing failed, the caller's claim and every log
  this deployment holds disagree.
- **The store's answer is read with its own polarity.** `create_session`
  returns the store's "newly created" boolean rather than discarding it;
  the first draft inverted it and the failing test caught it before any
  fix landed — the reason the test comes first.
- **Coverage the refute pass added.** The bound is pinned from the
  admission side, not only from the refusal side (an off-by-one that tried
  one generation too few passed every test that asserted the refusal); and
  the Messages surface, which shares `bind_prefix`, has its own
  restart-then-fork test over two routers on one store, so a regression in
  the shared function is red on both surfaces.
- **Not proven end to end on the Responses surface**: no Codex-dialect
  integration suite exists in this tree, so the Responses-side proof is at
  `bind_prefix`'s own level; the Messages suite carries the HTTP-level
  proof.

### What the review round changed (2026-09-03, M14.0)

Twelve findings from two lenses: eleven valid, one partially valid, one
invalid — the store contract already catches the mutation F12 described,
and the refuter's control stays as a pin. Six of the valid ones were one
defect with six faces, and it moved the ruling.

**R13′ — prefix admission probes, then commits, in its own module.** The
M14.0 loop forked on every attempt: each `fork` advanced the key's counter
and moved `latest` before anything was known. So a refusal left the counter
and `latest` on a generation no turn ran on (F9), a verbatim retry resumed
past the bound and was admitted whole a few forks later — and Claude Code
retries a 409 unconditionally (F6); the reported count was the constant,
not what was probed (F8); the search looked only upward from this node's
counter, so a node that had served a divergent turn forked past an older
generation that agreed and duplicated its prefix where a fresh node would
have continued it (F11); and an empty generation another node was mid-turn
on looked fresh and was taken whole, to die on the lease (F10). The
admission step itself was spelled twice, once with the store's answer
discarded (F2). The shape that removes all six: the *home* of a claim is
the existing generation that agrees with it and holds the most of it; a
fresh generation is created only when none agrees. The node's current
generation is probed first and the common case still costs one read;
otherwise the other existing generations are probed — upward to the first
missing one, which is the fresh slot, and downward to zero — bounded in
each direction. An empty existing generation is a home only if no other
writer holds its lease: leased, it is another node's fresh slot; unleased,
it is the shape a request leaves when it opened a generation and appended
nothing — a client that hung up, or a turn refused downstream — and it is
ours to use. (The ruling's brief had named `init_session` as the source of
that shape; it is not — `init_session` mints a binding in the control
store and never creates a session — and the fix pass said so rather than
building on it.) Nothing is committed until the home is known — one
`commit` on `Conversations` sets the counter and `latest`, and the session
is created there and only for a fresh home — so a refusal mutates nothing
in the table or the store, a retry is refused identically, and the refusal
counts what it probed. The first fix pass had asked existence by creating,
which minted the first free slot before the home was known whenever the
home lay below the counter; the second pass asks existence through
`last_seq`, so the probe writes nothing. The cluster the two dialects share — admission, the
stored-conversation projection, the bound, the refusal, their tests —
lives in `prefix_admission.rs` (F3), which is also the one home of this
rationale (F4); `responses_api.rs` keeps only its dialect, and
`conversations.rs`'s module doc has its F9 paragraph's referent back and
says what a restart now costs: an agreeing restart forks nothing and loses
no warm prefix, only one extra read of the prior generation (F7). The
constant and the function each carry their own doc again (F1), the bound
is named `MAX_PREFIX_PROBES` for what it now does, and the module's tests
live in a sibling file as the crate's other large modules do.

Also: the rung's tests run on one rig, and `Conversations` exposes its
generation naming to the crate's tests so no test re-spells `#g{n}` (F5,
partially valid — the crate's eleven echo-engine fixtures of one shape are
a crate-wide hygiene item recorded here, not this rung's). With M14.1's
durable generation map the probe is unchanged; only the counter's home
moves. Left for a hygiene pass, by name: `Conversations::bind` and `fork`
have no production caller now and survive as test fixtures; a refusal
whose every probed generation was busy reports zero disagreements; the
subagent fixture in the MCP surface suite changed its expected generation
because the downward walk now puts the parent's third turn back on the
generation it continues, which is the fix working, not a regression.

## Addendum (2026-09-02): M13.1 — the fair-use read path, the rulings

The M13 review measured what R-F1's layout costs where it matters: an
admitted turn — the common case — lets no window bind, so `would_exceed`
widens to the widest configured window and reads every bucket in it, 2017
`HMGET`s per capped scope under a seven-day cap, about eight milliseconds
of blocking Redis time per admission ahead of every queued session-log
append (F4). The cost was per command, not per byte. This rung replaces
the read path; the contract of M13 and R-F5's integer domain are
unchanged, and the shared contract suite is what proves it.

**R-F6 — one hash per scope, running sums per window, decay owned by the
read.** R-F1 rejected a hash per scope because it would have needed a
pruning pass nothing owned; running sums give the pruning an owner. Each
scope is one hash holding two field families: per-bucket amounts
(`b:<index>:t`, `b:<index>:u`) and, per window, a running sum with the
oldest bucket index it includes (`s:<window>:t`, `s:<window>:u`,
`s:<window>:from`). `record_draw` is one script that read-add-clamp-writes
the bucket fields and every window's sum for both scopes (R-F5's domain,
no command that can fail). `would_exceed` is one script that, per scope
and window narrowest first, *decays* the sum — one `HMGET` of the fields
that aged out since `from`, subtracted with a floor at zero, `from`
advanced; a `from` older than the whole window resets the sum to zero
without reading anything — compares the sum with the cap, and only on a
refusal walks the window's fields from `from` forward to compute the
earliest retry exactly as the memory ledger does. The widest window's
decay deletes the bucket fields it ages out, which is the pruning pass,
owned. The hash carries one `PEXPIRE` at the widest window plus one bucket,
re-armed on every draw, so an idle scope still costs nothing. Per admitted
turn the cost is a handful of field reads in one round trip, amortised
O(1); the bounded worst case — a scope idle for almost the widest window
and then resumed — is one `HMGET` of at most a window's fields, once.

**R-F7 — the outage posture, pinned** (D1 R14). A ceiling check that
cannot reach its store fails closed: the turn is refused with the
retryable error an outage calls for, because a ceiling that cannot be
checked cannot be honoured and the operator configured it on purpose. A
draw already made that cannot be recorded fails open with the reason
logged, because a bounded under-count is a fact about the outage and a
wrong refusal is not. The engine's seam already has this shape; this rung
pins both halves with tests against a Redis that is stopped mid-test.

**R-F8 — what proves it, and what moves.** The shared contract suite,
unchanged in its assertions, over both ledgers; the read-count pin the
M13 review folded into the single `INFO commandstats` test goes red and is
re-pinned to the new cost, derived not pasted; decay tests at bucket
boundaries, after an idle period shorter and longer than each window, and
across the widest window's expiry; the retry walk agreeing with the memory
ledger after decay; a two-node test through one Redis as before. No
migration: no deployment holds M13-layout keys — M13 landed on this branch
the same day — and the module doc says so rather than shipping a converter
for data that does not exist. The `redis` crate does not move.

### What the implementation settled beyond the rulings (2026-09-03, M13.1)

- **A window's sum carries the newest bucket it covers, not only the
  oldest.** R-F6 said a `from` older than the whole window resets the sum
  with no reads; that is sound only while draws arrive in non-decreasing
  time order, and with decay owned by the read a scope drawn at bucket
  zero and again long after, with no check between, has an ancient `from`
  and recent content. A fourth field per window, `to`, makes the read-free
  reset exact: reset only when `to` is older than the window's first
  bucket. One field per window on the write.
- **A saturated sum rebuilds rather than subtracts.** A sum clamped at the
  domain ceiling has forgotten how far past it the true total went, so
  subtracting an aged-out bucket would take a still-full window to nearly
  empty while the memory ledger, which re-sums, stays at the ceiling; the
  decay rebuilds from the bucket fields when the sum sits at the ceiling,
  and the same branch absorbs a gap wider than the window, which keeps
  every read bounded by one window's width. A test goes red without it.
- **The pruning pass runs in `record_draw`, not only in the check.** A
  membership capped only on the five-hour window never asks the widest
  window anything, so a read-only pruner would never run for it and the
  hash would gain a field every five minutes forever. And `would_exceed`
  now writes: the decay is persisted and the widest window's decay
  deletes, so a ceiling check cannot be served by a read-only replica —
  said at the script; nothing routes it to one today.
- **The fail-closed half is a 503.** The seam already failed closed and
  open in the right directions, but the closed half was a 500, which no
  client retries; it is now `ApiError::unavailable`, and the Messages
  surface already spells 503, and only 503, as `overloaded_error` — the
  one string Claude Code retries on (R-F7).
- **Divergences, stated.** [Superseded 2026-09-03 by R-F9 in the review
  section below: under the high-water-mark clock neither divergence
  exists.] After a rebuild, a draw stamped later than the
  `now_ms` a check supplies is excluded — fields cannot be enumerated
  forward without bound — extending the backwards-clock divergence M13
  recorded and reachable only with a clock that steps backwards between a
  settle and the next admission. Bucket ranges are chunked at four hundred
  fields per command because Lua's `unpack` is bounded; only refusal walks
  and the rare rebuild reach a second chunk, and the admitted path reads no
  buckets at all.
- **The read count.** Six commands per admitted turn on a three-window
  membership, derived as twice the window count and red first at the old
  4034; the pin is deliberately the decay-free steady state, and two
  sibling tests guard the reset branch — the refute pass showed the pin
  alone did not, and a second counting loop in that binary is the dense
  competitor the M13 implementation warned about. The retry-walk
  agreement test's fixture now forces two buckets to depart, since with
  one the naive formula agreed by coincidence.
- **Refute by mutation against the real server**: ten mutations — no
  decay, the oldest bucket off by one each way, the reset disabled, the
  pruning suppressed, one window's sum only, the retry from the sum, the
  expiry dropped, a store error swallowed into admitted, a record error
  turned into a refusal, `TIME` instead of the caller's clock — all red
  under named guards; two coverage findings closed test-first with no
  production change. The fix stage also reported the warn-once test as
  order-dependent in one full library run; an independent pass could not
  reproduce it in 135 bounded runs, and the capture is a thread-local
  subscriber serialized by one mutex, so the claim is recorded as not
  reproducible rather than papered over with a retry.

### What the review round changed (2026-09-03, M13.1)

Nine findings from two lenses, eight ruled valid by their refuters and one
whose refuter exhausted its output budget and was ruled test-first by the
fix stage. Three of them were one defect, and it moved a ruling.

**R-F9 — the ledger's clock is the high-water mark of every time it has
seen, in both implementations.** Decay owned by the read made the Redis
ledger's answer a function of the largest `now_ms` ever supplied for a
scope rather than of the `now_ms` handed to a call, and three faces
followed: a draw stamped below a decayed `from` lowered `from` and was
subtracted twice on the next decay, under-counting in the permissive
direction (F6); a check clock one millisecond behind an earlier check
admitted where the memory ledger refused (F9); and a draw stamped a few
milliseconds past the check's clock across a bucket boundary sat beyond
the retry walk's reach, so the refusal said "retry now" instead of the
real departure time (F8). The honest rule is the one the store had
already half-adopted: the clock is monotone. A check earlier than the mark
is evaluated at the mark; a draw earlier than the mark lands in its own
bucket and never widens a window backwards; a draw later than the mark
advances it, so the next check's window and walk cover every bucket the
sum does. No admission can be made more permissive by a clock stepping
back. The memory ledger — the specification of outputs — adopts the same
rule, so the contract now asserts agreement under a backwards check clock
and under an out-of-order draw, and the "divergences" the M13 and M13.1
addenda recorded are gone: under the mark there are none. Two things the
implementation settled: the per-window `to` field stays beside the mark,
because the read-free reset needs to know whether *this window's* newest
bucket has aged out, which a per-scope mark cannot say; and a check whose
clock is ahead of the mark pays one `HSET` per capped scope, once per
run, with the pinned counts otherwise unchanged. One storage-only
asymmetry is documented rather than fixed: a draw stamped more than the
widest window behind the mark still writes its bucket field, below every
`from` the pruning walk starts at, and is reaped by the hash's own
expiry; no window on either side counts it.

**The outage posture has a time bound and a redacted body** (F2, F4, F5).
R-F7 said a check that cannot reach its store fails closed with a
retryable refusal and named no time; the redis crate's default reconnect
backoff made every admission after the first wait about nine and a half
seconds for its 503 while the shared reconnect future ran. One `connect`
now serves the session store, the spend ledger and the fair-use ledger
with named bounds — 300 ms connection and response timeouts, three
retries, a 50 to 300 ms backoff at factor two — chosen so a check against
a severed store refuses within two seconds, measured, and the doc beside the constants says this
is the latency failing closed accepts. The fail-closed branch warns
server-side once per outage — with the store's error text in the log and
an info line on recovery — and the client body carries a fixed message
and the roundhouse code, never the operator's store error. Both dialects'
wire envelopes are pinned by a fair-use test, so the 503 that the
Messages surface spells `overloaded_error` cannot drift out from under
the retryability claim; the severed-store fixture moved out of the
production module into test support.

Housekeeping: `decay` is two functions, one that computes and one that
persists, and its doc states the true bound — one `HMGET` per chunk of
four hundred fields, six for the seven-day worst case — beside the steady
state (F7); the window type is split so the draw script builds no dummy
caps, and the refuter's test for it is a compile error now rather than a
runtime check (F3); the contract suite is three sibling files with one shared
raw-connection helper, and a guard pins the crate's convention (F1). One more thing the round
settled after its fixes: a read-count measurement and any other real-Redis
test in the same binary are serialised by a lock, because `INFO
commandstats` is a server-wide counter and a neighbour's own reads landed
inside the measurement — nine and eleven where the script issues seven —
which the "one measuring loop per binary" rule had not covered.

## Addendum (2026-09-03): M14.1 — durable generations, calls and threads, the rulings

D1's R12 named the minimum durable set that closes the M12.1 handoffs:
the generation of each cache key, the session each emitted tool call
belongs to, and the session each codex thread is in. This rung lands it,
after M14.0's probe-then-commit made the generation counter a *hint* —
the place a search starts — rather than the answer, which is what makes
a durable copy of it cheap to be right about.

**R-C1 — one trait, two implementations, one contract.** A
`CorrelationMaps` trait in roundhouse-core beside the spend and fair-use
ledgers: the generation a key was last committed at; the binding of a
`(principal, tool_use_id)` to a session, which a second binding to a
different session turns *ambiguous* and never silently overwrites; the
binding of a `(principal, thread_id)` to a session, where rebinding is
the normal case because a thread legitimately moves on every fork. The
memory implementation is the existing tables, moved behind the trait
with their three written properties intact (partitioned by principal;
ambiguous remembered rather than forgotten; threads rebind where calls
collide). The Redis implementation lives in roundhouse-store-redis, and a
shared contract macro the memory implementation passes first is what the
Redis one is proven against, gated as every store suite is.

**R-C2 — the counter is a hint; the store is the truth; the node caches.**
A durable generation map needs no atomicity: two nodes committing
different generations for one key both leave a value the other's next
search merely starts from, and the probe reaches the right home either
way. So `Conversations` keeps its in-process table as a write-through
cache — read through the store on a node's first touch of a key, written
through on every commit — and the per-turn cost in the common case stays
a local lookup. `resolve` for a named conversation answers from the
store when the node has no entry, so M12.1's "never bound on this node
refuses" becomes "never bound anywhere refuses", which is the same
promise with a wider scope. `latest` stays node-local and a guess by
contract, as R12 ruled.

**R-C3 — call and thread bindings are keys with a lifetime, not a table
with a cap.** In Redis each binding is one key with a `PEXPIRE` at the
staleness bound (a binding older than any plausible turn is a stale guess
whatever a table's size — D1 R14, brought forward here because the
durable shape needs a bound and a TTL is the one Redis owns), written at
the moment the call is streamed or the thread's turn is bound, exactly
where the memory tables are written today; a second write of a call id to
a different session marks it ambiguous in one script. The memory tables
keep their capacity cap and gain the same TTL under M14.2; the contract
asserts the semantics both share, not the bound each uses.

**R-C4 — the composition root chooses by the one rule.** `ROUNDHOUSE_REDIS_URL`
set means the Redis maps, wired beside sessions, spend and fair use; no
second predicate, and the boot line says which maps were wired. Every
shared key carries the declared namespace and a schema version, the
discipline M14.2 audits across the older families.

**R-C5 — codex's cache key on the control surface.** A codex
`tools/call` carries `_meta["x-codex-turn-metadata"].session_id`, which
is the turn's `prompt_cache_key`; the transport reads it beside
`threadId` and hands it to the resolver as a *named* conversation after
the thread arm, so a codex root thread's `status` at generation zero
resolves with no table at all and a subagent's resolves through the
thread binding first. A Claude-shaped call is unaffected.

**R-C6 — what proves it.** The contract over both implementations; the
M12.1 handoff tests re-aimed from "this node" to "any node" — two
`Conversations` over one Redis where a fork on one is the other's
starting point, a call bound on one resolved on the other, a thread bound
on one resolved on the other, and a refusal for a key never bound
anywhere; the ambiguous-call collision through the script; the staleness
expiry through a test seam, never a production TTL change; the read-through
cost pinned (one store read per key per node, then none). No migration: no
deployment holds these maps. The `redis` crate does not move.

### What the implementation settled beyond the rulings (2026-09-03, M14.1)

- **The node cache serves the turn path only** (R-C2, refined by the
  wiring stage test-first and accepted). R-C2 asked for a write-through
  cache read local-first on every family. The generation memo is exactly
  that for the turn path — three-state, unread / absent / at-n, read on a
  node's first touch of a key and primed by `commit`, so the common turn
  costs no store read — but the control surface's reads (`resolve` for a
  named conversation, `session_of_call`, `session_of_thread`) go to the
  store on every ask: a cached generation goes stale when another node
  forks and would narrow a session the client left; a cached call binding
  cannot see the ambiguous marker another node's colliding claim wrote;
  a cached thread binding goes stale on any fork served elsewhere. Each
  of those is a wrong-conversation answer with a network in the middle,
  and a control call is rare beside a turn. `latest` stays node-local.
- **A store outage on a correlator read is an outage, not "unknown".**
  `session_of_call` and `session_of_thread` on the reads seam return a
  `Result`; the justification for `Option` — the deployment wrote this
  table itself, so nothing can fail — died with the durable maps, and
  answering an outage as an unknown correlator would quietly hand the
  caller its `latest`, which the M12.1 thread arm already refused to do.
- **R-C5's gain, stated exactly** (partially valid as ruled): a codex root
  thread's `status` at generation zero already resolved through R-M7's
  named path, because a root thread's id is its cache key. The cache-key
  arm's real gain is the member whose thread id is nobody's cache key and
  whose thread binding the deployment does not hold — never recorded, or
  aged past its staleness bound — which reached `latest` before and
  reaches its own family's conversation now. The arm sits after the
  thread arm and before the tool-use id.
- **One predicate for four families.** `fair_use_backend` became
  `shared_backend`, the fourth caller being the correlation maps; the
  boot line names them. The composition-root wiring had no test — the
  refute pass's one green mutation — and gained the same real-Redis boot
  test the fair-use family has.
- **A write on the streaming path, deliberately.** In a durable
  deployment `bind_call` is awaited inline in the Messages follower's
  projection loop: one SET-shaped round trip per emitted tool call before
  the frame carrying the id leaves, because spawning it would race the
  client's answer to that very id.
- **Keys and values.** `rh:v1:corr:...`, namespaced and schema-versioned,
  values tagged so no client-spellable session id can impersonate the
  ambiguous marker, a delimiter in an id unable to make two members share
  a key, a foreign value refused rather than read as never bound; two
  named staleness bounds with a per-handle test seam.
- **Left for M14.2, by name:** the memory tables' staleness bound; the
  generation memo is uncapped (bounded by the conversations a node has
  served) and has no staleness bound; `Conversations::bind` is now
  read-then-write and has no serving-path caller.
- **The gate caught what the stages could not compile.** The async
  surface broke two calls in the real-binary suite's rig, which no stage
  compiles (it is feature-gated); the gate refused to commit, the two
  awaits were added, and the churn brief now compiles that suite too.
- **Refute by mutation against the real server**: ten mutations — the
  ambiguous marker overwritten, the call expiry dropped, the write-through
  removed, the memo never primed, generation zero minted for an unknown
  key, the two `_meta` keys swapped, the cache-key arm before the thread
  arm, the schema version dropped from a key, the composition root
  unconditional, call bindings unpartitioned — nine red under named
  guards and the tenth closed test-first.

### What the review round changed (2026-09-03, M14.1)

Eleven findings from two lenses, ten valid and one partially valid. Two
were coherence defects in the cache the rung introduced, one was the
resolver's shape, and one was the composition root; the rest were the
strict lens doing its job.

**R-C2″ — a hint that runs the search off its bound is stale, and is
refreshed before anything is refused** (F2). The generation memo is where
the probe starts, never what it concludes; but nothing refreshed it, and
a node whose memo lagged the store by more than the probe bound walked
its eight generations, never reached the store's, and refused — and
because a refusal commits nothing, refused every retry, while a fresh
node served the same claim at once. Now a walk that reaches the bound
without a free slot or an agreeing generation asks for a fresh hint (one
store read that re-primes the memo), restarts once from it if it
differs, and only a search that ran off the bound from a fresh hint
refuses. One extra read on the refusal path; the common turn unchanged.

**The node that committed must agree with itself** (F7). A commit whose
store write was lost primed the memo and moved `latest`, but the same
node's named reads went to the store and served the generation the
client had just left, with `Ok`. A memo entry now records whether its
last write reached the store; a named read answers from a dirty entry
ahead of the store, the next commit or read-through retries the write
and clears the flag, and the failure is logged once per outage. The turn
is not refused: it ran correctly here, and what the lost write costs
another node is still one walk. One consequence, stated: under a total
correlation-store outage a node answers a named read for a key it
committed during the outage from its own memo rather than refusing,
while every key it did not commit still refuses — the exception is the
staleness rule applied to the one node that knows, not a hole in it.

**One lazy chain** (F11). The cache-key arm M14.1 added was lazy and the
tool-use-id arm was not, and it carried a `?` — so an outage on the call
table refused a call the thread arm had already answered. Each arm is
consulted only when the previous answered nothing, and an arm not
consulted cannot refuse; the explicit argument is still compared against
the effective correlator, and correlator-versus-correlator stays ordered,
not refused.

**The lib builds the four families in one match** (F1). R-C4 promised one
predicate and `main.rs` evaluated it three times, with the wiring in a
binary no test can reach, so both boot tests mirrored the match by hand
and a mutation of the real wiring stayed green. `shared_backend::open`
builds the store, the spend ledger, the fair-use ledger and the
correlation maps in one match in the library, with one boot line per
arm; `main.rs` matches once on the result, and the boot tests call
`open`. The three per-family boot lines became two per-arm lines, which
is a visible log change; nothing in the tree asserted on the old
strings.

The rest: the memory maps lost a sync surface justified by a claim the
same rung had falsified (F4); the four contract macros share one helper
for their recursion plumbing (F6); `conversations.rs`'s tests are a
sibling file with one double instead of two (F3); README's ordering
sentence and two scope sentences say what the resolver does (F5, partial);
one `get` in the Redis maps (F8); the TTL lever takes its sibling's
shape (F9); the new suites use core's fixtures (F10). Left for M15 by name:
roundhouse-mcp has no sibling-test-file convention and `reads.rs` now
stands at 1072 lines with its tests inline; and the refreshed hint has
the memo's staleness question, which M14.2 owns.

## Addendum (2026-09-03): M14.2 — staleness bounds and shared-key discipline, the rulings

D1's R14 adopted two of Relay's disciplines by name: a staleness bound on
correlation state beside the capacity bound it has, and a declared
namespace and schema version on every shared-store key, rejected when
empty. M14.1 gave the Redis maps both; this rung gives the memory tables
the first and every older key family the second.

**R-S1 — one staleness bound per family, shared by both implementations.**
The call and thread binding lifetimes M14.1 named in the store crate move
to roundhouse-core beside the trait, so the memory tables and the Redis
keys expire by the same constant. A memory binding records the instant it
was written; a read past the bound answers absent and drops it, and a
write sweeps the queue's head, so the tables are bounded by age and by
count and neither bound waits on the other. The contract asserts the
semantics both share — a binding older than the bound is absent — through
the clock seam each implementation already has for tests, never by
sleeping and never by changing a production bound.

**R-S2 — the generation memo is a cache and is bounded like one.** It
holds one entry per key this node has touched, evicted oldest-first at a
named cap; an eviction costs one store read on the next touch and nothing
else, because the probe tolerates a stale or absent hint. It carries no
staleness bound: a generation hint that is wrong costs a probe, not a
wrong answer.

**R-S3 — one deployment namespace, one builder per crate, every family
audited.** Every key any roundhouse family writes to a shared Redis is
built by one function in the store crate from a deployment namespace
(default `rh`, set from the composition root, rejected when empty), the
schema version, the family and the family's own parts — sessions and
their leases, the spend ledger, the fair-use hashes, the correlation
bindings. The families that predate the rule are converted to the
builder; a unit test per family pins its shape, and one table in the
store crate's module doc lists every family with its version. Two
deployments sharing one Redis under different namespaces cannot see each
other's keys, which a gated test proves. No migration: the version is
already in the correlation keys, the older families gain it with this
rung, and the module doc says no deployment holds pre-rule keys.

**R-S4 — what proves it.** The contract's staleness assertions over both
implementations; the memory tables' age-and-count eviction under a
scripted clock; the memo's cap; the per-family key-shape tests and the
two-namespace isolation test against a real Redis; the composition
root's namespace read pinned by the same boot-test shape the other
families have. The `redis` crate does not move.

### What the implementation settled beyond the rulings (2026-09-03, M14.2)

- **The bounds live in core and the store re-exports them.**
  `CALL_BINDING_STALENESS_MS` and `THREAD_BINDING_STALENESS_MS` sit beside
  the trait; the memory tables take an injectable clock behind the
  test-support feature and stamp each write, a read past the bound
  answers absent and drops the entry, a write sweeps the queue head, and
  age and count bound the tables independently — both proven under a
  scripted clock. The Redis maps' defaults are the same constants, and a
  test reads the live `PTTL` back to prove the shipped default reaches
  Redis at the core bound, because a re-exported alias is only a promise
  until something compares it.
- **The memo's cap is 4096, oldest-first**, with an eviction costing
  exactly one store read on the key's next touch; the M14.1 prediction
  that the memo would gain a staleness bound was wrong and its doc says
  so — a wrong hint costs a probe, never a wrong answer.
- **One `keys` module builds every key.** `KeyNamespace` refuses empty
  and blank, defaults to `rh`, and `build_key(namespace, family, parts)`
  joins the namespace, the schema version, the family and the parts;
  sessions and leases, spend, fair use and correlation all go through it,
  a shape test per family pins the result, a gated test proves two
  namespaces on one Redis are invisible to each other for all four
  families, and a convention test brace-extracts every key function's
  body to assert it calls the builder and never spells the version
  itself — the refute pass showed a byte-identical hand-formatted bypass
  was otherwise invisible. Each family keeps its old `connect` and gains
  `connect_namespaced`; `shared_backend::open` takes the namespace and
  `resolve_namespace` reads `ROUNDHOUSE_REDIS_NAMESPACE` at the
  composition root, pinned by a boot test in the shape the other families
  have.
- **One stated deviation.** R-S4 said the staleness assertions never
  sleep; the memory side never does, but the Redis side's expiry test
  still waits a quarter second over a shortened per-handle TTL, because
  Redis expiry is wall-clock driven and forcing it would test the seam
  rather than the server. Recorded rather than papered over; a
  convention guard now pins the core side, where the seam is the whole
  point.
- **Refute by mutation against the real server**: ten mutations — the
  read no longer drops an aged binding, the write no longer sweeps, the
  two constants diverge, the memo uncapped, an evicted key served stale,
  one family ignoring its namespace, the builder ignoring its namespace,
  an empty namespace accepted, one family hand-formatting its key, the
  scripted clock replaced by a sleep — seven red under named guards and
  the three that stayed green closed test-first: the Redis default
  compared with the core bound, the builder convention guard, and the
  no-sleep guard.

### What the review round changed (2026-09-03, M14.2)

Thirteen raw findings, eleven after triage, ten valid and one partially
valid. Four of them were one design.

**R-S5 — one bounded, aged table.** M14.2 gave two structurally identical
tables the same age sweep by hand and left a third capped queue in the
server crate (F2), and each copy carried the same three defects: a write
past the bound matched the stale entry and marked it ambiguous where the
Redis key had already expired, so the two implementations disagreed at
exactly the bound the rung introduced (F3); a rebind kept its queue
position with a fresh timestamp, so a live head shielded every stale
entry behind it from the sweep and the count cap then evicted the fresh
one (F8); and the generation memo's cap evicted dirty entries, the one
state where the memo is the fresher fact, re-opening the M14.1 F7 answer
(F9). One generic table in core now carries the rules once, each pinned
on the type: a read past the bound is absent and drops the entry; a write
past the bound is a first write; a rebind moves to the tail so the head is
always the oldest; the cap evicts oldest-first among evictable entries
and never a pinned one, and a dirty generation entry is pinned until its
write lands. The call table, the thread table and the memo are three thin
instantiations naming their bound and their cap. The contract gained the
staleness assertion R-S4 promised and M14.2 did not deliver (F4), written
once with an advance-past-the-bound hook each instantiation supplies —
the memory one moves its scripted clock, the Redis one waits out a
shortened expiry it does not own — which is what "never by sleeping"
honestly means. `correlation.rs` took the house shape, the memory
implementation and the tests in sibling files (F1).

**Key discipline, closed rather than asserted** (F6, F7, F10). A
namespace could contain a Redis Cluster hash tag and put every key of the
deployment in one slot; braces, colons and whitespace are refused with
the reason. The refuter's corroboration of the Cluster-slot collapse ran
against a cluster-mode Redis it started itself; that test is not kept,
because the crate's one infrastructure gate is a plain Redis and the gate
run would fail it by design — the Redis fact it corroborated is stated in
`keys.rs`'s doc and the roundhouse behaviour is pinned by the unit test
that refuses the brace. The family is a closed enum with a name and, per family, a
version, so a v2 of one family moves only its own key space rather than
orphaning every session log; the convention guard scans for every key
function rather than a hand list, and the module-doc table is pinned by
the enum.

Housekeeping: README names the namespace knob, the store crate's as-built
key table shows the post-rule shapes, the empty-namespace error no longer
says its message twice (F5 — whose guard the verifier found mirrors the
message rather than exercising `main.rs`, the same limit every boot-site
guard in this crate shares and the composition-root rung already named); the no-sleep guard says it is a spelling
guard and scans for the spellings that matter (F11). The gate caught one
thing the fix stages did not: the M14.1 round's doc guard read the field
doc from the file the memory implementation had just been moved out of;
it now reads both halves.

## Addendum (2026-09-03): M15 — the hygiene rung the reviews opened by name

Every review round since M12.1 recorded one or two items it found real
but out of its rung's scope, each with the file and the reason. This rung
closes them together, and its one rule is the one a hygiene rung has to
keep: **a move keeps behaviour** — every migrated fixture asserts what it
asserted before, every folded fixture keeps the variation each copy
carried, and the two items that are behaviours (the all-busy refusal
count; the thread-table case on the control surface's fake) land
test-first like any other. The seven, by name: `Conversations::bind` and
`fork`, which lost their serving-path caller to M14.0's probe-then-commit
and survived as fixtures; the eleven echo-engine test fixtures of one
shape; the pointers the M14 moves left stale; the prefix-admission
refusal that counted only disagreements and so reported none when every
probed generation was busy; the thread-table case the control surface's
fake could model but no test did; the read-then-write on the maps; and
roundhouse-mcp's largest module taking the sibling-test convention the
server crate already keeps.

### What the implementation settled beyond the rulings (2026-09-03, M15)

- **The dead entry points went with their fixtures migrated**: `bind` and
  `fork` left `Conversations`; the fixtures that used them call the two
  test-support helpers or `commit` directly, and migrating the codex e2e
  rig fixed a missing await that no stage had compiled.
- **Eleven fixtures became three parameterised helpers** (a frontier spec,
  a single-model catalog, an engine over echo doubles); one fixture with
  a genuinely different constructor stays and says why. Every migrated
  suite's count is unchanged.
- **The refusal counts busy separately from disagreed**, so an all-busy
  search reports nine probed rather than none, and `attempts` stays their
  sum for the message.
- **The thread arm is exercised on the control surface's fake** at last,
  ahead of the cache-key name and the tool-use id.
- **The audit found no other read-then-write on the maps**: the
  hint-then-commit in prefix admission is the probe-then-commit design,
  not the defect shape.
- **roundhouse-mcp took the sibling-test convention** for its largest
  module; no other module there is past the line.
- **One incident, recorded.** The refute stage undid a mutation with a
  checkout that discarded the rung's own uncommitted change to the same
  file, and reconstructed it from a capture it had taken; the orchestrator
  re-ran the affected suite before the gate and the chain's own gate ran
  it again. Future refute briefs say: restore from your byte backup, never
  from HEAD.

### What the review round changed (2026-09-03, M15)

The M15 thermo-nuclear review (six findings, six valid; rulings in the
commit message) ran one reviewer on the strict maintainability lens a
hygiene rung deserves — did any move change a behaviour, and does the
crate read better after — and moved four things:

- **The fixture helpers name their fields.** `frontier_spec` took seven
  positional arguments, three of them bare `f64`s; transposing
  `quality_prior` and `base_ttft_ms` type-checked and moved the capability
  gate and the router's TTFT term together, silently (F1).
  `frontier_spec(provider, model, wire_protocol)` now returns the shape
  the fixtures agreed on, every departure is a named field in
  struct-update syntax, `single_model_catalog` wraps one spec, and a live
  shape guard reads the source and refuses an `f64` parameter on either.
- **The fold is by shape, not by name.** H2 retired the fixtures literally
  named `engine` and `catalog` and left ten hand-rolled copies of the same
  constructor in the very files that took the helpers, with
  `frontier_catalog` still spelling the literal `single_model_catalog`
  produces (F3). Folded; the review's two guards are live.
- **Two orders no test pinned are pinned.** The cache-key arm ahead of the
  tool-use id when the thread arm answers nothing (F2), and H4's split
  seen from the disagreed side plus the downward walk's busy tally from a
  non-zero hint (F4): each was reachable by no test, each has one now,
  landed by the refuter and left live with nothing to fix.
- **Prose the doc-warning check cannot see.** Five backtick mentions of the
  removed `Conversations::fork` and `bind`, and the 409 doc that still said
  "disagreed with all `attempts` generations" after H4 split the count
  (F6): corrected, with a word-boundary shape guard that does not flag the
  live `bind_call` and `bind_thread`. And `hold_busy` is used where its own
  doc said a hand-rolled copy stood (F5).
- **One more incident, recorded.** A refuter's mutation of the downward
  walk's busy arm was still in the tree after the review round returned;
  the orchestrator found it by diff before the fix round, restored the
  line and re-ran the suite. The rule from M15's own incident stands and is
  in every refuter and verifier brief: back up bytes, restore from the
  backup, and leave `git status` as you found it.
- **What the gate found on the way.** The full-workspace run turned one
  end-to-end test red once, with a worker the reservation path said did
  not exist. `EmbeddedFleet::register_worker` returned when Dynamo's
  `upsert_worker` did, which marks the worker schedulable and publishes
  the topology synchronously — but the table a *reservation* books against
  is kept current by a separate task that consumes that publication on the
  executor's own schedule, so under a saturated box a select followed at
  once by a reserve could name a worker the booking table had not yet
  seen. `register_worker` now waits, bounded, until the worker is routable
  on the same table `loads` reads, with a typed error if it never is; a
  flood test in the fleet crate reproduces the window deterministically
  (between one and eighteen of three hundred before, none after). Every
  caller is a fixture today, and the wait lives in the one place they all
  register rather than as a sleep copied into each.

## Addendum (2026-09-03): D2 ruled — the stored namespace, and the rungs it opens

R-M1 pinned a cost on the Responses surface and named the day it would be
re-examined: the log keeps the bare name codex sends, so a third party's
tool literally named `status` is folded out of the task view with ours,
and the day the log keeps a namespace, that exemption's test is the one to
delete. D2 is the re-examination, run on the tree at `1b85d64` alongside
the durable-directory question (ruled R16–R19 in
`PLAN-frontier-selection.md`). Its evidence is
`research/stored-control-call-namespace-1b85d64.md` — what each surface
stores, the sixteen consumers of the stored name, what moves a turn id and
what does not, why a Redis stream entry cannot be rewritten in place, and
the two gaps the read surfaced on the way.

**R-N1 — nothing stored is rewritten, renamed or re-read differently.**
R-M1 stands for both surfaces: the log keeps the name each client sent,
verbatim. The three migration shapes are closed by evidence, not
preference. A one-shot rewrite cannot recover a namespace that was never
written, so it could only guess which bare `status` was ours — the
ambiguity the change exists to remove — and it would move the turn id of
every conversation holding a control call, because the name is inside
`Item::render` and the dedup key *is* the turn id: an in-flight retry
misses its own completed response and buys a second billed answer. A
read-time canonicalisation is today's `CONTROL_TOOL_NAMES.contains`
recogniser under another name and resolves none of the collision shapes.
A versioned record tag is a new variant on an internally tagged enum, which
an older build cannot read — the failure the log's additive discipline
exists to prevent. Nothing has shipped that holds a pre-rule key, and that
is the state to preserve, not to spend.

**R-N2 — the namespace is carried beside the name, forward-only, and the
render leaves it out.** `ItemContent::ToolCall` gains `namespace:
Option<String>`, `#[serde(default, skip_serializing_if =
"Option::is_none")]`. The Responses canonicalisation reads codex's own
`namespace` field into it; the Messages surface stores `None`, because
Claude Code folds the registration into the name it declares, calls and
permits, and the flat spelling *is* the namespace there. `Item::render`
omits the field, so no turn id moves and no pre-change record changes a
byte; `call_id` already separates any two calls in one conversation, which
is why this is the opposite of the `Thinking::signature` call and is stated
as such in a comment beside it. Because the existing pinned turn-id
literal's fixture tool is `search` and cannot see this edit, the rung adds
two pinned literals over control-call conversations, one bare and one
namespaced, so the render decision has a guard that goes red. Every
construction site names the field — struct literals, no default —
`Item::tool_call` keeps meaning `None`, and the Responses wire gets the one
constructor that takes a namespace. An older build reads a namespaced
record as a bare one: the one-way door `SessionCreated::principal` and
`::arm` already walked through.

**R-N3 — prefix admission treats a stored `None` as agreeing with any
claimed namespace, and a stored `Some` as requiring equality.** Not blind:
a comparison blind to the field never notices a client changing which
server a name came from, which is a genuinely different call and should
fork like a different name does. Not symmetric: a stored `None` is a
record written before the field existed, or by the Messages surface, and
it agrees with any claim, so a conversation that straddles the change
continues rather than forking on its next request. `same_item` is already
blind to `response_id` for a stated reason; this is the second stated
exception, and it is the single load-bearing edit of the rung.

**R-N4 — the Responses recogniser matches on the field when present and
falls back to the bare arm when absent; the exemption narrows and does
not disappear; the outbound projection emits what it stored.**
`ControlCallDialect::CodexResponses` recognises a control call by
`namespace == Some("mcp__roundhouse")` and a name in the eight; with
`namespace == None` it falls back to the bare-name arm, because records
written before the change stay ambiguous forever and the fold has to read
them. So `a_third_partys_bare_status_tool_is_exempted_with_ours_on_the_
responses_wire_only` narrows to the `None` arm rather than being deleted,
and a new test proves that a third party's `status` under another
namespace is *not* ours — the realistic exposure was one name of eight,
and it is now zero for every record written after the change.
`function_call_item` emits the carried `namespace` when present:
re-emitting what codex sent is not the guess `codex_e2e.rs:1552` declined
to make, and the shape is pinned in `codex_wire_shapes.rs` against the
encoder in the pinned codex crates, which is the oracle that test file
exists for. A real-binary round trip stays with the blocked codex
counterpart of R-M8.

**R-N5 — the hygiene that ships with it, and the gap that does not.**
`Item::tool_call`'s doc still carries the claim the M12 review's F10
falsified ("a namespaced resend and a flat one arrive as this same item"),
and `dialect.rs` still says nothing renders a tool call outbound, which is
true of the `ClientDialect` type and false of the stored name on both
surfaces. Both are corrected in the same rung: the doc on the constructor
is the one a migration author reads first, and a reason that does not hold
is what gets a future change waved through. `TurnSignals::turn_depth`
counting control calls before they are dropped is real, pinned, and not
this ruling's — no spelling of the stored name changes it — so it stays
open by name for a validate rung with its own failing test.

### The rungs D2 opens

- **M16.0 — the directory seam** (R17): `DirectoryStore` becomes async
  and `Managed::compiled` compiles outside the write guard, judged under
  the memory store by the directory's existing tests plus the one guard
  R17 names. The lock span is the load-bearing reasoning; the rest is
  churn and refute.
- **M16.1 — the durable directory** (R16, R18, R19): the versioned
  opaque-document contract in core with its memory implementation and
  contract suite; `KeyFamily::Directory` in the store crate under one key
  with a Lua compare-and-set; the typed adapter at the directory's
  boundary; `Serialize` on the wrapped config entries with a pinned
  byte-for-byte round trip; the boot re-order and the fail-closed
  directory read; the flag and the warning deleted; the ignored
  `recreating_an_archived_project_after_a_restart_inherits_its_spend` live
  with its line numbers corrected; the input fingerprint and the typed
  divergence reason. The contract and the boot order carry the reasoning;
  the adapter and the serde are churn.
- **M17 — the carried namespace** (R-N2..R-N5): the field, the render
  decision with its two pinned literals, the prefix-admission rule, the
  recogniser and the narrowed exemption, the outbound projection pinned
  against the oracle, and the two doc corrections. The recogniser and the
  admission rule carry the reasoning; the rest is churn.

M16.0 goes first: it is small, behaviour-preserving, and the prerequisite
of the rung that closes the one restart loss that corrupts a surviving
ledger. M17 is independent of both and can be taken whenever the cadence
has room.

## Addendum (2026-09-03): M16.0 — the directory seam, the rulings

D2's R17 put the seam first and alone: `DirectoryStore` becomes async and
`Managed::compiled` stops compiling under the write guard, as one rung
with no Redis in it, judged under the memory store by the directory's
existing tests. The four rulings below are what "stops compiling under
the guard" means precisely, resolved in advance so the stage does not
re-derive them.

**R-D1 — the store trait is async the way `CorrelationMaps` is, and a
`std` guard never crosses an await.** `DirectoryStore::{load, commit,
version}` become `async fn` on a `Send + Sync + 'static` trait held as
`Arc<dyn DirectoryStore>`, the shape M14.1 gave the correlation maps;
`PlaneSource::plane` and `ControlDirectory`'s `new`, `plane`, `view`,
`snapshot`, `apply`, the two mints and `version` become async, and every
caller awaits — the two Messages handlers, the admin auth layer and its
routes, the boot, the fixtures and the test doubles. `current:
RwLock<Compiled>` stays a `std` lock and is never held across an await:
that is the rung's whole point, and the compiler enforces it, because a
`std` guard is not `Send` and a `PlaneSource` future has to be. The write
mutex, which *is* held across the load and the commit in `apply`, becomes
a `tokio::sync::Mutex`, because a lock held across an await has to be one
the runtime knows about.

**R-D2 — a refresh runs outside every lock and publishes by version.**
`Managed::compiled` takes the write guard twice and briefly. Once to
re-check the TTL and stamp `refreshed_at_ms`: the stamp is the
single-flight token — the first caller past the TTL stamps and refreshes,
every later caller sees a fresh stamp and serves the current plane — so
one revocation costs one compile however busy the node, which is the cost
the old comment feared from compiling outside the lock, and the reason it
held the lock. Once more to publish the compiled value, and only if the
loaded version is newer than the version already published, so two
refreshes that raced cannot let the older overwrite the newer.
`version()`, `load()` and `compile()` all run between the two, with no
guard held. `apply` keeps its order — load, mutate, compile, commit —
under the tokio write mutex, and publishes under the same version rule.

**R-D3 — the backoff is uniform: every refresh failure waits one TTL,
`version()` included.** Today a `version()` failure returns before the
stamp and is retried on every request for the length of the outage, while
a `load` or compile failure backs off one TTL; the doc already describes
the second as the rule and calls it deliberate. The stamp lands before
any fallible call, so all three back off alike: the price the doc names —
a revocation made during a failure can take two TTLs — is the same for
all three, and the warning fires once per TTL rather than once per
admission.

**R-D4 — what proves it.** Four guards over a scripted store double (a
`load` that waits on a signal, a counting `version` and `load`, failures
on demand) under a zero TTL, written against the ported shape before the
restructure so each is red first: *no stall* — while one caller's refresh
is blocked in `load`, a second caller's `plane()` returns the current plane
inside a short bound; *single flight* — N concurrent callers past the TTL
cost exactly one `load`; *publish by version* — a refresh that loaded
version 2 and finished after one that loaded version 3 does not replace
it; *uniform backoff* — a `version()` failure is retried one TTL later, not
on the next request. Every existing directory test and the admin-plane
suite pass unchanged; the R8 ignored test stays ignored with its reason
still true; no Redis, no serde, no boot re-order — those are M16.1's.

### What the implementation settled beyond the rulings (2026-09-04, M16.0)

- **The traits are async behind `#[async_trait]`, as the correlation maps
  already are.** A native `async fn` in a trait is not dyn-compatible on
  this toolchain, and both `Arc<dyn DirectoryStore>` and `Arc<dyn
  PlaneSource>` are held erased at their boundaries; the stage brief had
  misdescribed `CorrelationMaps` as native, and the store's trait doc now
  says which it is and why.
- **The guards run at a nonzero TTL under a scripted clock, not at zero.**
  A zero TTL has meant "refresh on every call" since M8, and the `>=`
  elapsed test exists to make it mean that — so at zero every caller is
  past the TTL by definition, the stamp cannot be a single-flight token,
  and three of the four guards would pin nothing. R-D4 said zero; the
  guards say `GUARD_TTL_MS` and carry the reason. The three pre-existing
  zero-TTL tests are unchanged and green: zero still refreshes per call,
  with each caller's compile made safe by publish-by-version.
- **`membership_terms` stayed synchronous** — it reads no store, only the
  records the caller already holds — under R-D1's own qualifier.
- **`current` is taken in three brief windows**: the read-guard TTL check,
  the write-guard re-check and stamp, and the write-guard publish. The
  re-check in the second window is defence for the scheduling gap between
  the first two, which no scripted clock can reach without a hook inside
  the directory; deleting it leaves every test green. It stays, and its
  comment says what it is for and that no test drives it.
- **Two gaps the refute stage found and the fix stage closed test-first**:
  `apply`'s version-guarded publish was correct and untested (a scripted
  commit-side gate now drives a refresh racing an apply, and the older of
  the two does not win); and `MemoryDirectoryStore::commit`'s own
  compare-and-set had never been exercised directly, because every
  Concurrent-path test used a hand-rolled double — the store has its own
  test now.
- **Every caller across the workspace awaits** — the two Messages
  handlers, the admin auth layer and routes, the MCP, metrics, Relay and
  Responses surfaces, the boot, topham's mint and CLI tests, both e2e rigs;
  the feature-gated real-binary suites compile; the doc-warning count did
  not move. Nothing in this rung touches Redis, serde or the boot order:
  those are M16.1's.

### What the review round changed (2026-09-04, M16.0)

The M16.0 thermo-nuclear review (five findings after triage from eight,
four valid and one partially valid; rulings in the commit message) ran two
reviewers — one on lock spans and races, one on the async port as a
change to the crate's surface — primed with the rung's own eight
mutations, and moved three things the rulings above state differently:

- **R-D2′ — the store's version is monotone by contract, and a regression
  is adopted and named, never silently discarded.** Publish-by-version
  assumed a version that only rises, which the trait never said (F3).
  Under the memory store that holds within a process; under M16.1's Redis
  store a restore from backup, a flush or a lagging failover makes it
  false — and then every refresh loaded, compiled and discarded the
  store's state forever with no line in the log, while a node's own
  `apply` returned success and dropped its own write, so a key an
  operator revoked stayed live until restart. `DirectoryStore::commit`
  and `version` now state the monotone requirement, so M16.1's store
  inherits it. In the refresh, a `version()` below the claimed version is
  a regression: recorded as a typed reason naming both versions, warned
  once inside the single-flight claim, and the store's state adopted if
  nobody published meanwhile — the store is the shared truth, and a node
  that diverges from it silently is the worse failure — while a late
  older refresh is discarded exactly as R-D2 said. In `apply` the
  orchestrator's ruling was unsound as written: "a load below the
  published version" also describes a write overtaken by a concurrent
  refresh of another node's newer commit, the very case guard 5 pins, and
  the committed version alone cannot tell the two apart because what
  separates them is whether the version this node serves is one the store
  still holds. So on the rare branch where its commit is not newer than
  what it publishes, `apply` asks the store once: a store at or beyond
  the served version means the write was overtaken and the newer plane
  stays; a store below it means the served version is a phantom, so the
  commit is published and the regression recorded. A failed probe there
  is warned and left to the next refresh.
- **Cancellation gives the single-flight token back** (F4). The claim
  stamp was written before two awaits with nothing to undo it if the
  claiming future was dropped — a client disconnecting mid-load left a
  claim no task held, nothing refreshing until the next TTL, and no
  warning. Unreachable under the memory store, whose futures resolve on
  first poll; reachable under any store whose load genuinely awaits,
  which is the store the next rung lands. A guard armed at the stamp
  restores the previous value if the future is dropped before publish,
  and only if the stamp is still the one it wrote; every failure return
  disarms it first, so R-D3's backoff stands: cancellation is not a
  failure and does not spend the slot. The compile is CPU, so the live
  drop points are exactly the two store awaits.
- **One scripted store double wraps the production store** (F1,
  partially valid: three of three test-double commits were dead, not two
  of three). Four hand-rolled copies of the memory store's records-and-
  version core existed and no test drove a stale version through any of
  them, so the production compare-and-set was pinned by one test and
  copied by three that pinned nothing. The one double in test support
  delegates to the production store and carries every knob the three had
  between them — gated loads and commits, counted reads, failures on
  demand, a write landed between two reads, a version set for the
  regression topologies — so every fixture exercises the real
  compare-and-set, and a stale commit through the double is refused by
  production code. It is the double that wraps the document-backed
  adapter next rung.
- **Two guards the refuters landed live**: a confirmed-unchanged version
  still stamps the TTL (F2 — the quiet-node path, one cheap version read
  per TTL, was pinned by no test), and the field doc of the stamp now
  says what it is since M16.0 (F5): the claim, kept on failure, given
  back on cancellation.

Left open by name, for M16.1: an `apply` dropped between its commit and
its publish loses the publish (the commit is in the store, and the next
refresh picks it up); the claim guard covers the refresh only, and a
give-back on the write path needs the tokio mutex's span reasoned about
beside the durable store.

## Addendum (2026-09-04): M16.1 — the durable directory, the rulings

D2's R16, R18 and R19 said what the durable directory is; M16.0 landed the
seam it needs. The five rulings below say precisely what lands, resolved
in advance so the stage does not re-derive them.

**R-D5 — the contract is a versioned opaque document in core, and the
memory implementation is the specification.** `roundhouse_core::control::
directory` carries `VersionedDocument { document: Option<Vec<u8>>,
version: u64 }` — version zero with no document is the empty store — and
a `DocumentStore` behind `#[async_trait]`: `load`, `commit(expected_version,
document) -> version`, `version()`, with `DocumentStoreError::{Concurrent
{ expected, found }, Unavailable }`, the two answers the directory's own
`StoreFailure` already distinguishes, so the adapter maps them one for
one. `MemoryDocumentStore` sits beside it as the specification, and a
`document_store_contract_suite!` over `__contract_suite` pins what both
backends must share: empty is version zero and no document; a commit
against the version read returns the next version and the bytes come back
exact; a commit against a stale version refuses `Concurrent` naming both
versions and changes nothing; `version()` tracks commits without reading
the document; identical bytes committed again still advance the version;
two handles racing to commit against one version admit exactly one; a
document of a few megabytes round-trips. Where the Redis instantiation
needs isolation it takes a fresh namespace per test, because this family
has one key.

**R-D6 — `KeyFamily::Directory`: one key, and a compare-and-set in Lua.**
Family `dir`, version `v1`, one hash key `<ns>:v1:dir:records` with fields
`version` and `document` — no hash tag, because nothing here is a
multi-key operation. `commit` is one script in `scripts.rs`'s idiom (read
the stored version, compare with the expected one, write both fields and
return the new version, or return the version found so Rust can name it);
`load` is one `HMGET`; `version()` is one `HGET`; the connection comes
through `connect_manager` like every other family's. The key-builder
convention table gains the row, the two-namespace isolation test gains the
family, and the contract suite is instantiated in the store crate's gated
tests, serialized like the others.

**R-D7 — the typed adapter serializes at the directory's boundary, and
every test runs through it.** `DirectoryStore` in the server crate keeps
its shape; `DocumentDirectoryStore` implements it over `Arc<dyn
DocumentStore>`, serializing `DirectoryRecords` as a JSON envelope —
`schema`, `records`, `compiled_under` — and `MemoryDirectoryStore` is
deleted: every fixture that built one builds the adapter over
`MemoryDocumentStore`, so the round trip is exercised by every directory
test there is rather than by one. The wrapped config entries take
`Serialize`, with defaults on every field a newer build might add so an
older build still reads a newer document; a document whose `schema` is
newer than the build knows is refused at load with a typed error, because
compiling a plane from half a document admits and refuses the wrong keys
silently, which is worse than a boot that stops and says why. A fully
populated `DirectoryRecords` is pinned byte for byte in both directions,
the way `a_pre_m11_log_record_still_deserializes` pins an item.

**R-D8 — the one switch widens to the directory; the boot re-orders; a
directory the store cannot read refuses the boot.** `shared_backend::open`
builds the fifth family in the same match — `Backends::Shared` carries an
`Arc<dyn DocumentStore>` over Redis, `PerProcess` a `MemoryDocumentStore`
— and the composition root constructs `ControlDirectory` after `open`,
over the adapter; the directory's first `load` is the boot check, as
constructing it always was, so a Redis that serves sessions and refuses
the directory read stops the boot with a reason naming
`ROUNDHOUSE_REDIS_URL`. With no memory-backed Redis branch left,
`control_plane_file_configured` and the warning it gates are deleted and
the comments that explained them rewritten. The ignored
`recreating_an_archived_project_after_a_restart_inherits_its_spend` goes
live with its assertions unchanged and its rig sharing one document store
across its two boots, its stale line numbers corrected; and a
`directory_backend_boot.rs` in the boot-suite shape proves against a real
Redis that a project archived through one directory is refused
`identity_collision` by a second directory opened over the same Redis
after the first is gone.

**R-D9 — divergence is fingerprinted, warned once per stored version with
a typed reason, and never refused.** The writer stamps `compiled_under`:
the SHA-256 of the control-plane file's bytes, the sorted identities of
the catalog and of the routing candidates the cross-checks were built from
(`CrossChecks` gains a fingerprint), and the TTL. A reader whose own
fingerprint differs — at boot or on refresh — warns once per stored
version with `DirectoryDivergence` naming which input differs, and keeps
serving the plane it compiles; refusing would make a rolling file change
impossible. A refresh that loads but will not compile keeps the last good
plane, as today, and records the version it could not take beside the
version it serves, readable through the directory for tests and for the
node-status surface D2 deferred by name. Nothing here refuses, and M8's
"still deferred" list is unchanged.

### What the implementation settled beyond the rulings (2026-09-04, M16.1)

- **Unknown fields are tolerated at the envelope and refused inside a
  record.** R-D7 said same-schema unknown fields are tolerated; the
  wrapped config entries keep `deny_unknown_fields`, because that
  attribute is what stops a mistyped key in an operator's file from
  silently widening a policy, and softening it for a storage concern
  would hand the file that failure mode back. So the envelope accepts a
  key it does not know, a record does not, and a build that adds a field
  to an entry has changed what a document can hold and bumps `schema`,
  which an older node refuses by name. Pinned by one test that asserts
  both halves.
- **The fingerprint rides in the load.** `VersionedRecords` carries
  `compiled_under`, `DirectoryStore::load` returns it, and the trait has a
  defaulted `compiled_under()` for the reader's own — one round trip,
  nothing thrown away. A second read to ask what a version was written
  under would answer about whatever the store held by then: a node
  warning about inputs belonging to a version it never compiled.
- **`ControlDirectory::status()` exposes the served version, the refused
  version and the divergence together**, out of one guard, rather than
  three accessors a caller could assemble from three lock acquisitions.
- **The R8 test could not keep every assertion aimed where it was.** The
  fix makes recreating `shutco` impossible, which is the point; the
  `identity_collision` refusal is now the assertion the test turns on,
  and the two budget assertions are kept verbatim against a fresh tenant.
  Its doc names `shared_backend`'s one match and the boot suite instead
  of line numbers, which is how the old ones went stale.
- **The boot composition is a library function.** The rung's own refute
  stage found that a fail-open fallback in `main.rs` — retrying the
  directory over a fresh memory store when the wired one refused — left
  both boot suites green, because `main()` is a `[[bin]]` nothing calls
  and the suites re-derived its composition. `control_config::boot_directory`
  is now the one composition, `main.rs` only maps its error, and the boot
  suite calls it — the M13 lesson applied a third time.
- **The judge is not fingerprinted.** `CrossChecks::fingerprint()` covers
  the routing candidates R-D9 named; two nodes differing only in
  `ROUNDHOUSE_JUDGE_MODEL` will not report divergence although one
  cross-check reads it. Documented at the function; open by name for the
  review round.
- **The memory store's compare-and-set is one critical section, and no
  test can race it.** `MemoryDocumentStore::commit` has no await, so the
  contract's racing test genuinely races only the Redis instantiation;
  sixty-four barrier-synchronised threads over fifty rounds never landed
  in a split-lock window on this platform, and the harness that tried
  starved the Redis connection driver. The single lock scope is the
  guarantee, the doc comment says so, and a model checker for a two-line
  critical section is disproportionate.
- **`redis` entered the server crate's dev-dependencies at the workspace
  pin** so the boot suite can write a key no roundhouse handle would —
  the watched dependency did not move. A `WRONGTYPE` from the store names
  the family's key, because the boot refusal has to name what an operator
  goes and looks at.
- **The stage's disk kept running out**; the whole-crate integration run
  was completed by the churn stage (fifty-nine server suites, the Redis
  suites against a real server), and the gate below is the record.

### What the review round changed (2026-09-04, M16.1)

The M16.1 thermo-nuclear review (six findings after triage from
fourteen, all valid; rulings in the commit message) ran two reviewers —
one on the document contract and the Redis compare-and-set, one on the
boot, the adapter and the divergence machinery — primed with the rung's
own fourteen mutations, and moved four things the rulings above state
differently:

- **R-D2″ — a document's identity is a lineage and a version, not a
  version alone.** R-D2′ made the version monotone by contract and the
  reader adopt a regression it could see; the Redis store could still
  break the contract invisibly (F1). Deleting the key — an operator's
  `DEL`, a flush, a restore — made the script's first-write branch
  restart the counter at one, a number already handed out, and a
  deployment young enough to be flushed and re-populated inside one TTL
  saw its claimed version come back equal, took the cheap "unchanged"
  exit, and served a revoked key from a plane the store no longer held,
  with no line in the log. The memory store cannot regress, so the
  contract suite could not express it. Now a store mints an opaque
  lineage the first time it writes a key and a key that was lost starts a
  new one; `commit` and `version` answer the pair, because only the
  commit can tell a writer which lineage it just started; within a
  lineage versions strictly increase and a version is never handed out
  twice; and a reader whose claimed lineage differs names a regression
  with a typed cause and adopts, exactly as it does for a lower version.
  One exemption is load-bearing and pinned only by a unit test: a node
  booted against an empty Redis claims no lineage, so version zero
  supersedes and regresses nothing, or that node would refuse the
  deployment's first write for the life of the process — a Redis-backed
  boot test for that shape is owed.
- **The write path refuses what the read path refuses.** The commit
  script's number grammar was looser than the decoder's — hex,
  whitespace and exponent forms read as numbers in Lua and as corruption
  in Rust — so a foreign key the read path called unreadable could be
  clobbered by a first commit (F3); and a hash holding a document with no
  version field read as version zero with a document, a shape the
  contract defines away and the memory store cannot produce, which the
  adapter then compiled and the script overwrote (F4). Both paths now
  accept only the shapes this store writes — plain decimal digits under a
  written ceiling, a lineage of this store's shape, both fields or
  neither — refuse the rest naming the key and the field, and the adapter
  refuses a document at version zero the way it refuses a lost one above
  it. The two ceilings are literals on both sides, each side's doc naming
  the other, because a Lua script cannot share a Rust constant without an
  argument.
- **The directory family has a size ceiling and a timeout that carries
  it** (F6). The store crate's 300 ms response timeout is sized for a
  ceiling check; a document of tens of megabytes timed out through it as
  an outage nobody could tell from a Redis being down. The adapter refuses
  a document over a written ceiling with an error naming both numbers,
  before any wire, and the directory family connects with its own timeout
  sized to carry that ceiling with margin, leaving the other four
  families' bound as it was. The margin is margin, not a guard: at the
  ceiling the old bound also passed on this box, so the ceiling is what a
  mutation trips and the timeout is what the measured crossover justifies.
  The refusal rides the existing unavailable variant, so over HTTP it
  reads as an outage; a variant of its own is owed with the next change
  to that error surface.
- **Two smaller moves.** The catalog half of the fingerprint was computed
  inside the binary where no boot test could reach it, and its doc block
  had swallowed a neighbour's (F2): the catalog computes its own
  identities in the fleet crate, the boot composition takes the catalog,
  and the boot suite asserts the real identities. And `status().divergence`
  never cleared once a node agreed again, unlike its sibling (F5): an
  agreeing load — a node's own commit included — clears it, and the
  once-per-version warning memory stays.

## Addendum (2026-09-04): M17 — the carried namespace, the rulings

D2 ruled the stored namespace in R-N1..R-N5; the five refinements below
say precisely what lands, resolved in advance so the stage does not
re-derive them.

**R-N6 — one field, three producers, one reader that does not care.**
`ItemContent::ToolCall` gains `namespace: Option<String>` with
`#[serde(default, skip_serializing_if = "Option::is_none")]`. It is set by
the Responses canonicalisation from codex's own `namespace` wire field, by
the fleet's Responses decoder from an upstream model's `namespace` (the
gap the D2 read surfaced: a model calling one of roundhouse's own tools
had its namespace dropped on the way in and could not be dispatched on
the way out), and by nothing on the Messages surface, where the flat
spelling is the namespace and the field stays `None`. `Item::render`
leaves it out, so no turn id moves; the comment beside the render says
why this is the opposite of the `Thinking::signature` call. Every
construction names the field: `Item::tool_call` keeps meaning `None`, and
one new constructor takes a namespace for the two inbound paths that have
one; struct literals gain the field explicitly, nowhere a default.

**R-N7 — the render decision has guards that go red.** Two pinned turn-id
literals over control-call conversations join the existing one over
`search`: a bare `status` and a namespaced `status`, whose fixtures differ
only in the field, pin the same turn id — the existing literal cannot see
the edit, and an implementation that folded the namespace into the render
would move both. A pre-change record deserialises byte for byte and
re-serialises without the field; a namespaced record round-trips with it.

**R-N8 — prefix admission: a stored `None` agrees, a stored `Some` must
match.** `same_item` compares role and content as before except for the
namespace, where a stored `None` agrees with any claim and a stored
`Some` requires equality — so a conversation whose early turns were
stored before the field existed continues, and a client that changes
which server a name came from forks as a changed name would. Three tests:
the straddling conversation continues; a stored namespace against a
different claimed one forks; a stored namespace against an absent claimed
one forks.

**R-N9 — the recogniser reads the field first and the bare arm second;
the exemption narrows.** `ControlCallDialect::CodexResponses` recognises
a control call by `namespace == Some("mcp__roundhouse")` with a name in
the eight; a `None` namespace falls back to the bare-name arm for records
written before the change; any other `Some` is not ours. The existing
exemption test narrows to the `None` arm rather than being deleted, and a
new test proves a third party's `status` under another namespace is
folded as the client's own tool. The Messages arm is unchanged.

**R-N10 — the outbound projection emits what it stored, pinned against
the oracle.** `function_call_item` emits `namespace` when the stored call
carries one and omits it otherwise; the shape is pinned in the codex wire
suite against the encoder in the pinned codex crates — the emitted object
for a namespaced call equals what codex's own `FunctionCall` with the same
namespace serialises to — which is the oracle that suite exists for, and
not the real-binary round trip R-M8's counterpart still owes. The two
stale docs (the constructor's, and the dialect module's "nothing renders
a tool call outbound") are corrected in the same rung; `turn_depth`
counting control calls stays open by name.

### What the implementation settled beyond the rulings (2026-09-04, M17)

- **Every construction site landed in the core stage**, not the churn
  stage: the fleet's chunk type gained the field because its decoder is
  a producer, and the Relay exporters destructure the call, so the server
  crate could not compile without them. The exporters drop the namespace
  on purpose, with a comment at each site: neither the Relay trace format
  nor the chat-completions tool-call shape has a field for an MCP server,
  and adding one is a schema decision in a format another project owns.
  Open by name for a Relay rung.
- **Two shipped tests were the pre-M17 statements of the old ruling and
  were rewritten rather than deleted**: the one asserting that a client's
  namespace and item id do not perturb the turn hash (its dedup half
  depended on prefix admission, and a bare resend of a namespaced call
  now forks by R-N8; the hash half is pinned as a literal at the unit
  level), and the one asserting the log stores no namespace, inverted.
  The item-id half of the old test was already vacuous — no fixture in
  that file ever sent one — and is now asserted directly on a
  hand-written wire object.
- **The oracle pin reads one function.** The Responses wire module is
  public for exactly `function_call_item`, so the codex wire suite can
  compare what roundhouse emits with what codex's own encoder produces
  for the same value; everything else in the module stays crate-private,
  and the module declaration says so.
- **The render blindness D2 named was reproduced before it was
  guarded.** Folding the namespace into the render only when present
  leaves the pre-existing pinned literal green — its fixture tool is
  `search` — and reddens only the new control-call pin; that is the exact
  edit the old guard could not see, and the reason R-N7 asked for two
  literals over one value.
- **More stale docs than the two R-N10 named made the same false
  claim** — the recogniser's module header and its constant's doc, the
  dialect module's Responses bullet, the fleet's join doc and its
  decoder's "deliberately not read" note — and were corrected in the same
  pass, because a reason that does not hold is what waves the next change
  through.
- **The refute stage found nothing to fix**: ten mutations went red
  under the guards the rulings named and the two inspections confirmed
  the constant is reused and the oracle is codex's encoder, not a
  literal. `turn_depth` counting control calls stays open by name.

### What the review round changed (2026-09-04, M17)

The M17 thermo-nuclear review (seven findings after triage from eight,
all valid; rulings in the commit message) ran two reviewers — one on the
field as a change to the durable log, one on the three consumers that
changed behaviour and the crate's surface — primed with the rung's own
twelve mutations and inspections, and moved three things the rulings
above state differently:

- **R-N6′ — the engine's join is dialect-aware.** R-N6 said the Messages
  surface stores no namespace by construction, and the rung's own join
  broke that from the other direction (F7): a namespace decoded from an
  upstream on a Messages-surface session was stored, the Messages wire
  has no field to send it back through, and R-N8 then forked the session
  into a new generation on every tool-using turn — unobserved only
  because the translated Messages toolbox happens to keep an upstream
  from returning one. The engine stores the decoded namespace only on the
  Responses surface, the dialect it already derives from the session key,
  and stores none on the Messages surface, where a value that cannot come
  back is a fork trap rather than a fact. The Responses round trip — a
  namespaced upstream call stored and re-emitted on the outbound frame,
  which no fixture exercised (F3) — is pinned by a live guard that the
  verifier reddened at both join sites.
- **One reading of a stored `None`.** The fleet decoder's doc read an
  absent namespace as the fact that the tool has no server; the item's
  field doc read it as "this client did not spell one", an unknown — and
  R-N8's stored-`None`-agrees rule is sound only under the second (F5).
  The item doc owns the meaning, and the decoder's doc now points at it.
  The README's two pre-M17 sentences and the canonical-arguments doc that
  still named the old three-argument join are corrected (F1, F4), and the
  control-plane plan's own account of the bare stored name carries a
  dated superseded note. The refuter's guard for F4 was itself a
  tautology — its assertion quoted the sentence it searched the whole
  file for — and took the crate's slice-before-the-tests pattern before
  it could go green; the verifier re-broke that half separately.
- **Two guards the rung lacked.** The Relay exporters' deliberate drop of
  the namespace was unguarded, since no exporter fixture ever carried a
  namespaced call (F6): both exporters now have one. And the Responses
  wire module's test half crossed the crate's size line (F2): it lives in
  its own file, the prefix-admission precedent, with every test name
  unchanged.

Two process notes. The engine-join cluster verified red-before-green with
a `git stash` on a tree carrying another cluster's uncommitted work — the
M15 incident's shape; the stash was popped and the tree matched the
verifier's snapshot, so nothing was lost this time, and the fix briefs now
say byte backups only, on the write path as on the refute path. And the
F5 guard is conjunctive: a reword of the item doc would disarm it rather
than fail it, which M18 pins with a control.
