<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Plan: the Anthropic Messages surface, the seat, and the launcher (M11)

> **Status: shipped through M13 (2026-09-02).** The rulings in §3 stand
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
