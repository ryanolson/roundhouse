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
> embedded Dynamo integration, streaming turn engine, and HTTP/SSE transport
> are real and tested. The WebSocket and gRPC transports and the Redis store
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
| `roundhouse-store-redis` | Redis Streams `SessionStore` *(not yet implemented)* |
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
| Spent on hosted endpoints | **Measured.** Reported token counts against the configured rate card. |
| Provider cache discount | **Measured.** Cache-read tokens the provider reported, times the gap between its two published rates. |
| Served locally instead | **Estimated.** A counterfactual: what our own fleet's traffic would have cost on a comparable hosted model. |

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
  because an unaccounted call is at least marked.
- Anything that still comes back without usage is recorded as
  `Accounting::Estimated`: input from the prompt we tokenized and routed on,
  output from our own tokenizer over what we received, and cached input left at
  zero because nothing observable bears on what a remote cache did. The
  dashboard reports the resulting coverage and says plainly that those figures
  understate.

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

## Switchyard

Kept behind our own `RoutingPolicy` trait rather than wired in directly.
Switchyard's `Algorithm` trait is a good fit — the algorithm emits
`Step::CallLlm` with a semantic target and the *host* executes it — but
`libsy::State` is in-memory with no pluggable persistence, which collides with
the requirement to survive process death. Behind the trait, it is an option
rather than a dependency.

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

WebSocket and gRPC transports; the Redis store; real provider clients for
OpenAI and Anthropic; cross-process fencing tokens on the lease (within a
node, turns are serialized by the engine; across nodes, by node identity and
TTL); and resuming an interrupted generation from its partial output — the
partial is already durable in the log, so the groundwork is there.

Metrics are per-process: the recorder folds what this node served plus whatever
it replayed from the sessions it opened. A fleet-wide view means either scraping
each node or folding the shared log centrally, which is a job for the Redis
store rather than for another counter. The dashboard also reports totals over
all history with no time-window selector, because the in-memory fold keeps no
per-interval buckets; adding a window means bucketing the fold, not querying it
differently.
