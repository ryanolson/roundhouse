<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Roundhouse

A stateful agentic front-end that sits **in front of** Dynamo.

A coding agent today re-uploads its entire conversation on every turn. At 100k+
tokens of context and hundreds of turns, that is the dominant cost of agentic
work — in bytes on the wire, in prefill FLOPs, and in dollars. Roundhouse owns
the conversation instead, so the client sends only deltas.

Once the service owns the context, a second capability falls out for free:
because it knows the exact token prefix, it can ask *which engine already has
this prefix cached* and route accordingly. Statefulness and routing are not two
features — the first is what makes the second possible.

> **Status: exploratory walking skeleton.** The session core, routing layer,
> embedded Dynamo integration, streaming turn engine, HTTP/SSE transport, and
> Redis session store are real and tested. The WebSocket and gRPC transports
> are not yet implemented.

Roundhouse depends on Dynamo but is not part of it. It pins two Dynamo crates
(`dynamo-kv-router` with the `standalone-selection` feature, and `dynamo-tokens`)
to an upstream commit of [`ai-dynamo/dynamo`](https://github.com/ai-dynamo/dynamo)
and builds independently.

Those come from git rather than crates.io because the newest published
`dynamo-kv-router` (1.3.1) predates DEP #10321: it neither exports the embeddable
`SelectionService` nor uses `routing_group` (it still says `tenant_id`). The
pinned git dependency also resolves `dynamo-truthy`, which the workspace needs
and which is unpublished. When a release carrying the selection service reaches
crates.io, the pin becomes a plain version.

## Layout

| Crate | Contents |
|---|---|
| `roundhouse-core` | Session state machine, event log, lease, context assembly, routing vocabulary and policies, metrics projection |
| `roundhouse-fleet` | Local Dynamo fleet (embedded selection service) and frontier providers |
| `roundhouse-store-redis` | Redis Streams `SessionStore`: entry id == seq, `PX` lease on the Redis clock, fenced appends via Lua. Selected by `ROUNDHOUSE_REDIS_URL`; absent means in-memory sessions that die with the process |
| `roundhouse-server` | Turn engine, HTTP/SSE transport, metrics API and dashboard, and the binary |

## Design

**One append-only event log per session, with a monotonic `seq`.** That single
structure serves three otherwise separate needs: Responses-API streaming
resumption (`starting_after`), reconnect replay for the bidirectional
transports, and the routing audit trail.

**Conversation items and the routing ledger are projections of that log**, not
separately stored collections. There is one write path, so nothing can disagree
after a crash — and `SessionStore` collapses to six methods as a result.

**A single-writer lease with fencing.** Every mutating call takes a `Lease`. An
owner that stalled, was partitioned, or died and came back fails its next append
rather than interleaving with its successor.

**Incremental tokenization.** Routing on cache locality means knowing the
prompt's block hashes before dispatch. Recomputing them each turn would cost
O(context) per turn and O(context × turns) per session — more work than the
routing decision can save. Because the conversation is append-only and Dynamo's
block hashes are computed per fixed-size block from tokens alone, only newly
completed blocks are hashed. Verified against a full recompute using Dynamo's own
hashing functions.

**One comparison axis.** "Serve this from our Llama on worker 7" and "send it to
Anthropic" become comparable once both are expressed as cache-adjusted expected
prefill. The two sides get that number very differently:

- **Local** — `SelectionService::select` is query-only and returns
  `effective_prefill_tokens`, the scheduler's own cache-credit-weighted prefill
  cost, *without booking anything*.
- **Frontier** — no provider exposes its cache, so it is modelled from the
  routing ledger: what we last sent, when, and under which provider TTL.

## Why embed the selection service

Dynamo's `SelectionService` exposes every HTTP endpoint of
`python -m dynamo.select_service` as a plain async method, so it can be called
in-process. Embedding removes the TCP round trip *and* the JSON serialization of
the prompt, which dominates per-call cost at long context.

Queries carry **block and sequence hashes, never token ids** — for a 100k-token
context, the difference between shipping a 400 KB array and a few kilobytes.

The **select/reserve split** is what makes cross-provider routing possible at
all: price the local option, compare it against a frontier model, and only book
if local wins. An abandoned quote costs nothing; the pending selection expires.

The reservation lifecycle (`prefill_complete` → `release`) is **mandatory** — a
leaked reservation permanently inflates the router's view of a worker and
silently distorts every later decision.

### Scaling shape

The selection service is stateful but **replicated, not sharded**. Every replica
holds the complete radix tree and processes the complete KV event firehose, so
neither index memory nor event-ingest CPU divides by N. Replica sync is
best-effort by design (no sequencing, acknowledgement, replay, or
resynchronization), and output-block growth is deliberately not synced — so each
replica underestimates load driven by its peers, and it worsens as N grows.

Embedding at N ≈ 3–10 is comfortable. The `select`→`reserve` stickiness that
bites multi-replica HTTP deployments is free here, since one process does both.

**At N = 1 replica sync is simply off** — `replica_sync` is never called, so no
PUB socket is bound and none of the O(N²) peer-mesh configuration applies. The
only remaining ZMQ is worker→selector KV-event ingest, which is an engine-side
wire contract.

## Metrics and the dashboard

`GET /v1/metrics/dashboard` renders what the fleet has served; `GET /v1/metrics`
is the same document as JSON. Both are reads, take no lease, and cost the store
nothing — the numbers are a fold already done, not a sweep over the log.

**Metrics are a projection, like everything else.** Token counts, dollars, and
the savings figure are folded out of the same append-only log that carries the
conversation and the routing ledger. There is one write path, so the dashboard
cannot disagree with the audit trail it summarizes, and a fold of the stored
events reproduces exactly what the running process reports. That equivalence is
a test, not an aspiration.

Every model's tokens break down into input, cached input, output, and reasoning.
Cached input is part of input and reasoning is part of output — both providers
report them that way, and storing them as separate addends would double-count
every total, including the one billed to a client. Rows roll up twice: by
**provider** (Anthropic, OpenAI, our own fleet), which is what a rate card
attaches to, and by **serving mode** — local Dynamo versus a remote endpoint —
which is what the savings argument turns on.

### What "dollars saved" actually claims

Three figures, and they are not equally solid, so the dashboard never merges
them into one:

| Figure | Basis |
|---|---|
| Spent on hosted endpoints | **Split.** Reported against the rate card, and reported *apart* from the part priced off our own tokenizer when a provider stayed silent — see below. |
| Provider cache discount | **Measured**, wholly, even at partial coverage. An unreported call records zero cache reads rather than a guess, so it contributes nothing here. |
| Served locally instead | **Estimated.** A counterfactual: what our own fleet's traffic would have cost on a comparable hosted model. |

Hosted spend is not one number labelled measured. The fold keeps
provider-reported and self-counted tokens in separate accumulators and prices
each — free, because pricing is linear in tokens — so `frontier_spend_usd`
carries `frontier_spend_measured_usd` and `frontier_spend_estimated_usd`
beside it. Merging first and reporting a call-weighted coverage ratio
afterwards does not substitute: one unreported 200k-token turn beside a
reported 2k-token turn is 50% coverage by calls and 1% by tokens, and it is the
token figure that tracks the money. Both are reported;
`coverage_token_fraction` is the one to quote next to a dollar.

There is a fourth quantity and it is deliberately not one of the three, because
it is not money: `seat_tokens`, the traffic served through a **forwarded
subscription seat**. Roundhouse holds no rate card for a seat, so the catalog's
per-token price would describe what *it* would have paid on its own key — a
counterfactual, not a bill — and the spend ledger has refused to draw against
one since budgets existed. The dashboard now refuses the same way: a
pass-through turn is counted in every token figure on the page and priced in
none of them, and the seat's share is published as a count so a deployment can
still see the traffic it is carrying. The turn's decision records which it is,
so the ledger, a successor process repairing a lost settle, and the dashboard
all read one recorded fact rather than three re-derivations of it.

Only the third needs an argument. A local worker bills nothing, so its saving is
the difference against a call that never happened — which means naming a hosted
model it stands in for. That stand-in is a model's **correlary**, chosen by
declaration where someone has stated one (the only kind of answer that can
account for an eval or a procurement decision), and otherwise inferred as the
nearest hosted model by *traffic shape*: output ratio, cache ratio, reasoning
ratio, and log-scaled mean prompt and answer lengths.

Shape alone must never select a correlary, and this is the trap the design is
built around. A 7B model and a frontier reasoning model doing the same
summarization job have nearly identical traffic shapes; pricing the first
against the second would multiply the reported saving by an order of magnitude,
and the number would look *better* the more absurd the comparison got. So a
candidate must first pass a **capability gate** — its configured `quality_prior`
within `capability_band` of the local model's — and only among models already
declared comparable does shape decide. Where nothing passes, no correlary is
produced and that traffic contributes nothing to the saving: the dashboard
reports it as unpriced rather than priced against a model it has no business
being compared to.

The counterfactual is deliberately like-for-like — the same token counts
*including the same cached fraction*, at the reference model's rates. It is not
"what if we had sent this cold", which would assume the hosted provider's cache
never warmed and would roughly double the headline on long sessions. Had we been
routing there all along, its prefix cache would have been warm about as often as
our own is.

As a cross-check the snapshot also carries what the router itself quoted for the
best hosted alternative at the moment it chose local, taken from the decision
record rather than from a rate card. Two independent estimates of one
counterfactual should land near each other; when they do not, one of the two
models is wrong, and that disagreement is worth more than either number alone.
It is reported beside the total, never added into it.

### Usage has to be asked for

A streaming OpenAI-compatible request — the real OpenAI API, vLLM, SGLang,
Dynamo's own frontend — returns **no usage object at all** unless the request
set `stream_options.include_usage`. Anthropic reports unconditionally but splits
it: input and cache-read counts on `message_start`, output tokens on the final
`message_delta`, so a client reading only the delta records zero input and no
cache reads.

Unaccounted calls are the worst possible failure here, because they are silent:
they fold in as zero tokens for zero dollars, and zero dollars on a hosted model
is indistinguishable from a saving. The dashboard would look its best exactly
when its instrumentation was broken. Two defences, and both are needed:

- `WireProtocol::enforce_usage_reporting` rewrites an outbound request to ask
  for accounting. It only ever *adds*, and never overrides a field the caller
  set — silently disagreeing with a request is worse than an unaccounted call,
  because an unaccounted call is at least marked. The dialect travels on the
  `FrontierQuote`, because that is the only argument a `FrontierClient` gets
  and the engine holds one client for providers whose transports have nothing
  in common: a client cannot look the dialect up, so it has to arrive.
- Anything that still comes back without usage is recorded as
  `Accounting::Estimated`: input from the prompt we tokenized and routed on,
  output from our own tokenizer over what we received, and cached input left at
  zero because nothing observable bears on what a remote cache did. Those calls
  are priced separately and reported as the estimated half of hosted spend.
  Note the direction is *unknown*, not low: a tokenizer mismatch cuts either
  way. Only the cache discount is safe to call understated, because its
  estimated contribution is pinned to zero.

### Configuring it

Prices are not in source — rate cards change, and a constant in a binary goes
stale silently. Point `ROUNDHOUSE_CATALOG` at a JSON file (see
`examples/catalog.example.json`) carrying the hosted models the router may
choose between, their prices, and any declared correlaries. One file, because
the price the router optimizes against and the price the dashboard reports
saving must be the same number or neither means anything. A catalog that is
named but unreadable stops the process rather than falling back to a default:
starting anyway would serve every turn under prices nobody chose.

Without the variable the binary serves its offline echo stub, for which every
price is zero — so the demo demonstrates the token breakdown, not the savings.

**Providers are data, not one hardwired transport.** The same catalog file
carries a `"providers"` section: `name -> { base_url, routes: { models?,
chat_completions?, responses?, messages? }, auth: { env }, extra_headers? }`
(see `examples/catalog.example.json`'s `openrouter` and `dynamo-fleet`
entries). `main` builds one client per definition at boot — its own connection
pool, base URL, and static headers — and every catalog entry's `provider` has
to name a definition or the built-in `openai`, which is the implicit provider
`ROUNDHOUSE_FRONTIER_UPSTREAM` / `ROUNDHOUSE_OPENAI_API_BASE` have always
named; a catalog written before this section existed still loads unchanged.
Two load-or-die cross-checks make the registry total rather than merely usual:
every entry's `provider` is defined, and a defined provider declares a route
for the dialect its entries speak — both refuse the boot, not the first turn
that would have hit the gap. (A third check, whether *this build* actually has
a transport for that dialect, is a fact about the binary rather than the file
and is made where the binary is composed.) The key itself is never written
here: `auth.env` only names the environment variable it is expected to arrive
in, so a configured provider with no key anywhere is a boot warning rather
than a surprise found one turn at a time — the credential a turn actually
authenticates with is still resolved per turn from the control plane's
deployment/project/member tiers.

**Fair-use session windows** cap a project's or a member's draw over a rolling
`5h`, `24h` or `7d` window — the shape a frontier lab's own session limits use,
not a calendar reset the way `budget` above is. Configure `ROUNDHOUSE_CONTROL_PLANE`
(see `examples/control-plane.example.json`) with a `"fair_use": {"windows":
[{"window": "5h", "max_tokens": ..., "max_usd": ...}]}` block at project level
and, separately, on any key for a per-member ceiling on top of it; each window
needs at least one cap, or it is rejected at load for reading like a limit
while enforcing nothing. A project's and a member's windows are two
independent ceilings that both bind — the narrower one refuses first. A turn
over its window gets HTTP 429 `fair_use_exceeded`, naming the scope, the
window, and the quantity that ran out, plus the earliest `retry_at_ms` the
window could have room again — retryable, and no grant is taken for a refused
turn. Enforcement is single-node only in this milestone (the counters live in
the process's memory), so a deployment that sets `ROUNDHOUSE_REDIS_URL` while
also configuring `fair_use` gets a boot warning that two nodes serving one
project enforce two independent ceilings rather than one shared one.

**Two-tier selection.** A project's `"tiers"` block turns routing into a choice
between two ordered lists — `"capable"` and `"efficient"`, each naming target
identities (`provider/model`, or `local/model` for one of the fleet's own) —
plus `"picker"` (`efficient_first`, the default and the only operating point
anyone has calibrated, or `capable_first`, which the process warns about at
boot) and `"confidence_threshold"` (`0.0..=1.0`, default `0.5`). Absent
`"tiers"` is the shipped answer, and a project without one routes exactly as it
did before this existed.

What moves a turn between the tiers is the session's own recent tool results —
error severity, whether the agent is producing work or spinning in place, how
deep the session is — scored by a port of Switchyard's coding-agent scorer.
**No model call is involved anywhere in the decision**: it is a `tanh` over four
numbers read out of the log the fold already holds, so an agent that starts
looping is moved up a tier and one that just made its tests pass is moved back
down, at no latency and no cost. (The judge that *does* call a model lives in
the validate loop, which is a different surface.)

Three properties of the lists are worth knowing before writing one:

- **Admission runs first and a tier can only narrow.** A target this key's
  policy, quality floor, or credentials do not admit is skipped and never
  resurrected; a tier that empties entirely falls to the other one, with the
  decision's rationale saying so.
- **Order is the operator's, and it is also the failover order.** The first
  admitted entry of the picked tier serves the turn and the rest of that same
  tier are its ordered fallbacks. A fallback fires only on a hosted dispatch
  that never reached a model — transport error, timeout, 408, 429, 5xx — under
  the *same turn deadline* and the *same single budget grant*, so a flaky
  provider cannot pyramid holds or spend N times the turn's allowance. A refusal
  or a content filter is an *answer* and is not retried, and a local target does
  not fail over at all.
- **A target may not be named twice**, within a tier or across both. Rejected at
  load rather than deduplicated: a repeat inside one tier is a retry of the model
  that just failed wearing a failover's clothes, and a name in both tiers makes
  the scorer's choice a no-op that still reads like a decision.

Two consequences that follow from the design and are easier to meet in the
README than to rediscover in a log:

- **A cadence counts attempts, not turns.** `frontier_cadence` is folded at each
  `Routed`, and there is one `Routed` per *dispatch*, so a project at
  `max_frontier: 1, per_turns: 3` that fell forward twice has spent two rations
  on one turn. That is deliberate and conservative — a dispatch that failed on
  the way out really did reach for a hosted model — and it means a provider
  outage tightens the ration rather than loosening it.
- **`DecisionRecord::policy` reads `stage` on a deployment that configured any
  recipe**, including for its projects that configured none, because the stage
  router is the object in force and reporting the inner policy's name would make
  the audit trail credit the wrong router. Their target and rationale are
  byte-identical to what the inner policy would have produced. The wrapper is
  composed **only when some project has a recipe at boot**, precisely so that
  this field does not move on a deployment whose routing did not; a recipe added
  through the admin plane *after* boot therefore selects nothing until a
  restart, and the process warns once, naming the router that could not read it.

**The client's `model` field is recorded, never routed on.** `/v1/responses`
accepts a `model` and has always ignored it — roundhouse chooses the target. It
is now written verbatim onto the decision as the *declared baseline* and read by
exactly one consumer: the dashboard's counterfactual, which prices a local turn
against the model the client said it thought it was talking to rather than
against one inferred from the catalog. When it resolves, the saving is reported
on the `Declared` basis; when it does not, the value is still recorded verbatim
and the pricing falls back to inference on the `Inferred` basis, never a silent
upgrade. No line of routing reads it.

**The handoff note now rides a tier escalation too.** A project that configured
`handoff_note` gets one sentence appended to the *forwarded request only* on the
first turn a signal moves the session onto the capable tier — the same mechanism
the validate loop's escalation already used, under the same rules: it never
touches the stored conversation, it rides once per switch rather than
accumulating, and it narrates only a signal-driven move. A fall-open onto the
capable tier under `capable_first`, a signal-driven move *down* a tier, and a
capable pick the key cannot reach all carry nothing — in each of those the model
reading the note would be told the preceding steps came from something in
trouble when nothing said they did, or that a change of hands happened when none
did.

**Sourcing `quality_prior`.** `FrontierModelSpec::quality_prior` is
configuration, not measurement, and `import-benchmarks` (a binary target in
`roundhouse-fleet`, not linked into any shipped binary) is what lets that
figure be sourced instead of guessed. It reads OpenRouter's
`GET /api/v1/benchmarks` (`OPENROUTER_API_KEY`) and writes two files: a catalog
fragment with each `quality_prior` normalized to `0.0..=1.0`, and a paired
provenance record naming the index, its snapshot date, and a `citation`
**required** for republication — the tool refuses to emit without one. It is
configuration generation, never a runtime dependency: nothing in a shipped
`roundhouse` binary calls OpenRouter, and its own tests run entirely offline
against a committed response fixture, never the live route.

## Switchyard

Kept behind our own `RoutingPolicy` trait rather than wired in directly. This
means the `NVIDIA-NeMo/Switchyard` library (`switchyard-libsy`); NeMo Relay's
`crates/switchyard` is a deprecated HTTP client for a decision service that
Switchyard's main branch no longer serves. Switchyard's `Algorithm` trait is a
good fit — the algorithm emits `Step::CallModel` with a semantic target and the
*host* executes it — but `libsy::State` is in-memory with no pluggable
persistence, which collides with the requirement to survive process death, and
the library is self-described pre-alpha whose core vocabulary changed shape
three times in one week of 2026-08. Behind the trait, it is an option rather
than a dependency.

What has been taken across is the part that does not move: the **coding-agent
scorer** and its constants, ported into `roundhouse-core/src/routing/stage.rs`
with the upstream revision named in the module's attribution and pinned by a
test that reads the attribution text rather than its own literals. That is the
asset — the table of error patterns and the calibration behind the thresholds
are trace-mined rather than reasoned, so an editorial improvement made on the
way across would be an unmeasured heuristic wearing a measured one's
provenance. Every divergence is documented at the divergence: `compacted` has no
input in this tree and so the hard escalate is severity-only; `turn_depth` is
the exchange count rather than the message count; and upstream's
`ConsultClassifier` outcome is folded away entirely, because routing here makes
no model calls — an undecided turn lands on the picker's default tier and is
marked as having got there by falling open, which is what stops the handoff note
narrating it.

## Build and test

Requires the Rust toolchain pinned in `rust-toolchain.toml` and system
`libzmq3-dev` (pulled in transitively by `dynamo-kv-router/standalone-selection`).

```bash
apt-get install -y libzmq3-dev   # or: brew install zeromq
cargo test --workspace
```

The first build clones `ai-dynamo/dynamo` to resolve the pinned Dynamo crates,
so expect it to take a while; later builds reuse the cached checkout.

Once built, the tests need no GPUs, no worker processes, and no network: the
selection plane runs inside the test binary.

## What the tests establish

- **Incremental hashing is exact** — matches a full recompute across unaligned
  appends, and a shared prefix yields an identical sequence-hash chain.
- **Client bytes stay flat while context grows** — over 20 turns, per-turn client
  bytes vary by ≤ 2 while server-side context grows more than 10×.
- **Pricing books no load** — two `select` calls yield distinct pending
  selections and zero booked prefill.
- **A quote can be abandoned for free** — the fleet prices identically afterwards.
- **Routing reacts to cache state** — a warmed target is priced below its prompt
  length on the following turn.
- **Local cache hits are measured, not modelled** — a real mock vLLM engine
  (`dynamo-mocker`) executes the turns and publishes KV events over ZMQ in the
  engine-native wire format; the embedded selection service indexes them, and a
  repeat turn under the real TinyLlama BPE prices at ~2% of its prompt length
  (32 of 1536 tokens), with every complete block of the prior context matched.
- **What was hashed is what is dispatched** — local execution consumes the token
  buffer's ids verbatim, and the canonical rendering tokenizes back to the
  buffer exactly; a captured dispatch is byte-identical to the quoted stream.
- **Replays are redeliveries** — a retried turn returns byte-identical text and
  the original usage, not a re-rendering with zeroed accounting.
- **A failed turn settles** — the response terminates with an incomplete event,
  the lease comes back immediately, and the same turn id is retryable without
  waiting out a TTL.
- **Streaming is genuine** — deltas are durable in the log before the response
  completes, a stream that breaks halfway commits its partial (which the ledger
  reads as prefill evidence), and TTFT is derivable from the log: first delta
  minus the routing decision that preceded it.
- **A turn outlives its lease** — the heartbeat renews while the turn works, so
  a model call longer than the TTL commits instead of being fenced at its own
  finish line; a displaced owner still loses, and a hung provider settles at
  the turn deadline instead of renewing forever.
- **The log is the streaming bus** — the SSE transport tails the same event log
  that serves replay and audit, so live frames, reconnect frames, and log
  entries are one thing; resumption from `starting_after` or `Last-Event-ID`
  is exact, and a deduplicated retry replays the original response's entries
  ending on its terminal event.
- **Reservations settle** — load returns to zero; a consumed `selection_id`
  cannot be booked twice.
- **Failover loses nothing** — killing the owner mid-session, a successor claims
  the lease, replays the log, and continues with contiguous sequence numbers.
- **Retries do not regenerate** — a re-sent `turn_id` replays the existing
  response instead of opening a second turn.
- **The dashboard's numbers fold out of the log** — a cold rebuild from the
  stored events reproduces what the running process reports, a node that
  restarts and picks a session back up recovers its history exactly once
  (neither dropped nor double-counted), and a deduplicated retry adds no
  second call.
- **An unaccounted call is marked, not counted as free** — a provider that
  streams an answer and no usage lands as an accounting gap with estimated
  token counts, never as zero tokens for zero dollars.
- **Capability gates the correlary** — local traffic with no comparable hosted
  model is reported unpriced and contributes nothing to the savings figure,
  rather than falling back to the nearest rate card.
- **The tier a turn lands on is read from the session, not asserted** — a
  stalling session is driven through the real signal extractor as *items* and
  comes out on the capable tier; a session that produced work and passed its
  tests comes back down; a quiet one falls open. Each has a control that varies
  only the exchanges, so none of them is a test about the picker.
- **A dead provider costs one attempt, not the turn** — a transport failure
  advances to the next candidate of the same tier inside one turn, one deadline
  and **one grant settled once**; a refusal, a 401 and a 404 fail where they
  stand; a stream that dies after the first delta is not retried; and an
  exhausted tier fails with every attempt on the record rather than spilling
  into the other tier.
- **A failover crosses transports as well as targets** — the second attempt goes
  out through the second provider's own client, with its own base URL and
  credential resolved for that dispatch; an engine that resolved one client per
  turn passes every single-dispatch registry test and fails this one.
- **The handoff note narrates only a switch that happened** — it rides the first
  signal-driven move onto the capable tier and survives a failover inside that
  turn, and carries nothing on a fall-open, a signal-driven de-escalation, a
  second consecutive capable turn, or a capable pick the key cannot reach.
- **A recipe nothing reads is not silent** — a `tiers` block on a process whose
  router cannot read one warns once, naming that router; the same recipe on the
  stage router says nothing at all.

## Codex as the compliance oracle

`POST /v1/responses` is an OpenAI Responses API surface over the same event
log: point a stock [Codex CLI](https://github.com/openai/codex) at it with a
custom provider (`base_url = "http://host:port/v1"`, `wire_api = "responses"`,
`requires_openai_auth = false`) and it streams `response.*` events end to end.

The conformance tests do not re-implement the spec — they depend on Codex's
own client crates (`codex-api`, pinned by git revision as dev-dependencies)
and drive our endpoint through the exact parser a real agent runs. That
catches the failure class a hand-written spec test cannot: Codex silently
drops a known item type with a malformed body, so only its parser can prove
we never emit one. The suite covers the full event sequence, the
`output_item.added`-before-delta ordering its client enforces, terminal
semantics (`response.completed` ends the stream; `response.failed` and
`response.incomplete` require the server to close the body), usage projection
(`cached_input_tokens` lands in Codex's `input_tokens_details.cached_tokens`),
and a real-socket round trip through Codex's HTTP stack.

The integration is also the thesis in miniature. Codex-over-HTTP re-sends the
whole conversation every turn (`previous_response_id` is a websocket feature),
and it names the conversation with `prompt_cache_key`. Against an append-only
log that resent history is a *claim*, not input: the surface checks it as a
prefix of the session named by the cache key and admits only the suffix — so a
stateless client gets stateful routing, one accumulated warm prefix, and
idempotent retries (the turn id is a content hash of the conversation) without
knowing any of it is happening.

## Not yet built

Roundhouse does not have WebSocket and gRPC transports. It does not have real
provider clients for OpenAI and Anthropic. It also cannot resume an interrupted
generation from its partial output. The partial output is already durable in
the log.

Metrics are per-process: the recorder folds what this node served plus whatever
it replayed from the sessions it opened. A fleet-wide view means either scraping
each node or running a shared fold over the Redis log rather than adding another
per-process counter. The dashboard also reports totals over all history with no
time-window selector, because the in-memory fold keeps no per-interval buckets.
A time window requires buckets in the fold, not a different query.
