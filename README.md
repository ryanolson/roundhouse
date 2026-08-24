<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Roundhouse

A stateful agentic front-end that sits **in front of** Dynamo.

An unmodified coding agent — Codex, Claude Code — points at roundhouse and,
through it, reaches both the models Dynamo serves locally and the frontier
labs' public endpoints, with each turn routed on function, cost and time to
solution together. Nothing in the agent's own stack changes: pass-through
auth, prefix admission of resent history, and an MCP control surface are what
make hooking up invisible.

A coding agent today re-uploads its entire conversation on every turn. At 100k+
tokens of context and hundreds of turns, that is the dominant cost of agentic
work — in bytes on the wire, in prefill FLOPs, and in dollars. Roundhouse owns
the conversation instead, so the client sends only deltas.

Once the service owns the context, a second capability falls out for free:
because it knows the exact token prefix, it can ask *which engine already has
this prefix cached* and route accordingly. Statefulness and routing are not two
features — the first is what makes the second possible.

> **Status.** Real and tested: the session core, routing layer, embedded
> Dynamo selection, streaming turn engine, HTTP/SSE transport, Redis session
> store and spend ledger; the control plane (principals, projects,
> memberships, keys, per-key policy, budgets); the admin plane; the MCP
> control surface; the validate/steer loop; real frontier provider clients;
> a real `codex` binary driving all of it end to end behind a feature gate;
> and emission of NeMo Relay's interchange formats from the same log.
>
> Not built: the WebSocket and gRPC transports, and resuming an interrupted
> generation from the partial output already durable in the log. Metrics are
> per-process. Two things the M9 addendum defers by name: there is no
> operator entry point that *produces* the generated Codex config (it is a
> library function), and the MCP surface still ignores the `_meta.threadId`
> a Codex client sends on every `tools/call`.

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
| `roundhouse-core` | Session state machine, event log, lease, context assembly, routing vocabulary and policies, the control vocabulary (principals, policy, spend ledger), the validate/steer loop, metrics projection |
| `roundhouse-fleet` | Local Dynamo fleet (embedded selection service) and frontier providers |
| `roundhouse-mcp` | The control surface as an MCP server: eight tools, overlays that only narrow, and one file that knows what JSON-RPC is |
| `roundhouse-relay` | NeMo Relay's published formats — ATOF events, ATIF v1.7 trajectories, `LlmOptimizationSummary` — produced from the same session log |
| `roundhouse-store-redis` | Redis Streams `SessionStore` and spend ledger: entry id == seq, `PX` lease on the Redis clock, fenced appends via Lua. Selected by `ROUNDHOUSE_REDIS_URL`; absent means in-memory sessions and spend that die with the process |
| `roundhouse-server` | Turn engine and six surfaces over one log — native HTTP/SSE, the OpenAI Responses API at `/v1/responses`, the MCP mount at `/mcp`, the admin REST plane under `/v1/admin`, `/v1/metrics` and its dashboard, and Relay's three session reads — plus `codex_launch`, which writes the config a client reads, and the binary |

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

## The control plane

Who is asking, what they may be routed to, and what it may cost. Three
entities: a **project** (the thing with a budget and a policy), a **user**, and
the **membership** that joins them — which is the unit a turn is attributed to,
as the `Principal` pair `project/user`. Keys hang off memberships.

**`ROUNDHOUSE_CONTROL_PLANE` names a JSON file**, exactly the
`ROUNDHOUSE_CATALOG` idiom: the config format *is* the deserialized types, a
named-but-unreadable file stops the process, and a validate boundary rejects at
load time the shapes that would otherwise resolve ambiguously at request time.
Unset means **Open mode** — every request resolves to the single
`default/default` principal with no key required anywhere, which is what makes
the offline demo runnable. Configured means every surface demands a key.

**No secret is in the file.** A key is `rh_turn_<43 base62 chars>` or
`rh_admin_<43 base62 chars>` — 32 CSPRNG bytes behind a role prefix a scope
match can trust structurally — and the file holds only `sha256(secret)`,
hex-encoded, so resolving a presented key is a hash-and-look-up rather than a
comparison against anything secret held in memory. A turn key rides
`Authorization: Bearer …` or the dedicated `x-roundhouse-key` header; an admin
key on a turn route is `403 wrong_key_kind`, because only the route knows what
it wanted.

**Policy narrows and never widens.** A project entry's `"policy"` becomes its
`TurnPolicy`; a key entry's `"overrides"` composes onto it through
`TurnPolicy::narrow`, which is the only composition there is. An override wider
than its project on any numeric axis is rejected at the config boundary naming
both entries, because an operator-authored file that silently means less than
it says is worse than one that fails to load.

**A budget is a grant ledger, not a counter.** `SpendLedger::open_grant` reserves
`min(requested, project_remaining, member_remaining)` and holds it under the
turn's `ResponseId`; both ceilings are read and the hold placed in one atomic
step, or concurrent grants under one membership jointly exceed the limit.
`settle_grant` is idempotent by `(session_id, seq)`. A hold whose turn died
lapses on its TTL, and the next open of the same session repairs a lost settle.
Exhaustion **degrades to local** rather than failing the turn.

**The admin plane** (`/v1/admin/...`) is the only surface that writes tenancy:
projects, users, memberships, and key mint/revoke, plus one read that exists
nowhere else. It refuses Open mode *before* it looks at a header — in a
deployment where no key exists and none can be issued, "use a different key"
is the wrong sentence. A minted secret is returned once and never again.

`GET /v1/admin/projects/{project}/budget` is the **reconciliation view**, and
its whole design is that the columns are stamped and never summed:
`committed_usd` is what the ledger charged against a ceiling within a budget
window, `measured_usd` is what this process's metrics fold measured since it
started, and `drift_usd` is `committed − measured`, unclamped in both
directions. They are produced by different machinery over different periods; a
reader who does not know that would read their difference as an error, so the
difference is published and so is the reason it is not one.

**Revocation is bounded by a snapshot, not instant.** Every surface holds a
`ControlDirectory` rather than a compiled plane, and re-resolves per request
against an admission cache whose TTL defaults to
`DEFAULT_ADMISSION_CACHE_TTL_MS` (30 s). A mutation affects the *next*
admission and nothing in flight: a key revoked while a turn is streaming does
not interrupt that turn, and a budget raised mid-turn does not raise that
turn's ceiling.

Configuration is environment variables and nothing else, because a flag parser
in the composition root is the first place a deployment concern leaks in:
`ROUNDHOUSE_ADDR`, `ROUNDHOUSE_REDIS_URL`, `ROUNDHOUSE_CATALOG`,
`ROUNDHOUSE_CONTROL_PLANE`, `ROUNDHOUSE_JUDGE_MODEL`,
`ROUNDHOUSE_FRONTIER_UPSTREAM`, and — for the two auth modes, which address
genuinely different origins — `ROUNDHOUSE_OPENAI_API_BASE` and
`ROUNDHOUSE_OPENAI_PASS_THROUGH_BASE`.

## The MCP control surface

An agent talking to roundhouse over the Responses API can see what it is being
routed to only by inference. `roundhouse-mcp` says so directly, in eight tools:
`status`, `init_session`, `declare_intent`, `prefer`, `set_quality_floor`,
`fetch_steer`, `report_outcome`, `explain_last_route`.

**Overlays narrow and never widen**, and that is what makes two of them safe to
put in front of a model — a model reading its own context is one prompt
injection away from being someone else's. `prefer` and `set_quality_floor`
compose through `TurnPolicy::narrow`, which is total and can only shrink the
admissible set: an overlay asking for more than the deployment's ceiling is
*clamped and reported*, never honored and never refused.

**No tool appends to a session log.** An MCP request arrives on its own HTTP
request and a session log has exactly one writer at a time, so every tool is
either a pure read of committed state or a write to a node-local control store
the engine reads at the start of the next turn.

The surface is mounted at `/mcp` as streamable HTTP, behind the same key
resolution as every other route, with the turn key as the bearer; a `GET` on it
is 405. Every descriptor states all three MCP annotations — `readOnlyHint`,
`destructiveHint`, `openWorldHint` — with the last two `false` on all eight,
because the tools reach nothing outside this deployment and their writes only
narrow. That is not decoration: under `approval_policy = "never"`, which
`codex exec` forces, a Codex client treats a tool it sees no annotations on as
destructive and open-world and **cancels** the call, handing the agent a
cancellation notice where the output should have been.

`fetch_steer` is how a correction reaches the agent. It is a pure read that
returns exactly the bytes written when the steering call was emitted — twice
gives the same bytes and does no paid work — which is what lets the validate
loop hand an agent a real tool call it can execute without the tool doing
anything a turn could be billed for.

## The validate/steer loop

Interposing on a turn to tell an agent it is going the wrong way. **Off unless
a project says otherwise**, and a project that turns it on still gets the
Shadow arm by default: the Intervention Paradox says an excellent critic can
collapse one agent and leave another untouched under the identical policy, and
the property that decides is measurable only per deployment.

**The trigger is a budget gate conjoined with a signal, never a cadence
alone.** The gate is a projection of the log — tokens since the last
validation (20 000), a cooldown (60 s), at most 2 consecutive interventions and
8 validations per session — so it needs no counter and survives a restart
exactly. Six signals read the same prepared evidence:

| Signal | Fires on |
|---|---|
| `NoProgressRepeat` | the same call, the same arguments, the same answer, repeatedly |
| `PingPong` | two tools alternating with nothing else between them |
| `ToolFailureStreak` | consecutive tool calls all returning failures |
| `CostAnomaly` | a turn far outside this session's own trailing distribution |
| `ErrorSeverity` | a named failure — traceback, import error, timeout — at `HARD` (0.7) or worse within the last three results |
| `PureBashStreak` | four consecutive shell or unrecognised calls with nothing read, written or edited between them |

The last two are ported from Switchyard's `ToolSignals`, attributed at the
module and pinned by a test. They are kept alongside `ToolFailureStreak` rather
than folded into it because that one is anchored and needs a consecutive run,
which is a different question from "did anything in the recent window fail
badly".

A signal states what it saw in the indicative and never suggests: "this call
has produced identical output four times" is a fact the judge weighs, while
"this looks like a loop, consider escalating" is roundhouse asking the judge to
agree with it, and a judge that agrees with the trigger is an expensive way to
re-read the trigger.

**The judge is a side call, booked on its own row.** It runs on the catalog
model `ROUNDHOUSE_JUDGE_MODEL` names, never reaches the cache ledger, and a
judge that cannot be reached releases the turn — there is no error arm anywhere
on this path, because the checker must never break the checked. What is not
allowed is for the failure to be silent: a timed-out validator is marked, never
free.

**Three arms, stamped into the session at creation.** `Live` takes the action;
`Shadow` runs the judge, logs everything and discards the action; `Placebo`
runs no judge and intervenes anyway on deterministic timing — the control
without which "tokens fell after we steered" is consistent with the steer
having said anything at all, because the disruption itself changes the
trajectory.

**Outcome B is a synthetic tool call.** The held turn *completes* — never fails,
because only a completion registers as a completed turn and an incomplete one
would re-enter the interjection on every retry — carrying a `function_call`
under the client's namespace (`mcp__roundhouse`) naming `fetch_steer`, whose
`call_id` *is* the steer id. Four frames and no text: `response.created` →
`output_item.added` → `output_item.done` → `response.completed`. The log stores
the bare neutral name, so a namespaced Codex resend and a flat resend from
another client canonicalize to the same stored item and prefix admission cannot
fork on a dialect.

**What a steered turn reports is not what it books**, and the split is a ruling
rather than an oversight (PLAN §10.2, decided on M9 evidence). Codex's
compaction gate reads `last_token_usage`, which is *replaced* on every
response — so reporting the judge's usage on the wire made the client believe
its live context was ~1147 tokens when the history it was about to resend was
~5700, a five-fold under-report on exactly the turn it had just been told to
change approach. So `response.completed.usage` now reports the steered turn's
own context contribution, while the log books what it always booked: the
judge's usage on the turn record and the side call on its own model row, so the
dashboard's pricing is unchanged.

## Hooking up Codex

`POST /v1/responses` is an OpenAI Responses API surface over the same event
log, and `roundhouse_server::codex_launch` writes the two files that point a
stock [Codex CLI](https://github.com/openai/codex) at it: the `config.toml` the
client reads out of its `CODEX_HOME`, and the model catalog that config points
at. Nothing else about the client changes — no wrapper, no patched binary, no
forked provider.

- **One environment variable, both entries.** `ROUNDHOUSE_API_KEY` by default
  feeds `env_key` and the `[model_providers.roundhouse.env_http_headers]` entry
  for `x-roundhouse-key`, whose name is read from the router's own constant
  rather than retyped. The secret is never in the file: a generator that took
  it would put an `rh_turn_…` into something that ends up in a dotfile repo.
- **`requires_openai_auth` is set by the route's auth kind.** A client holding
  its own roundhouse key gets `false` **with** `env_key` — never without,
  because at `codex-cli 0.146.0` the flag gates nothing the auth resolver
  reads, so a provider with no `env_key` sends whatever ambient login sits in
  `CODEX_HOME` to *our* `base_url`, or no `Authorization` at all. A client
  whose ChatGPT login roundhouse forwards upstream gets `true` and no
  `env_key`, plus a stated precondition §3 did not have: a completed
  `codex login` in that `CODEX_HOME`. Skip it and requests arrive
  unauthenticated, which roundhouse *admits* and degrades to local-only — turns
  keep answering and no frontier route ever happens.
- **The catalog is pinned in both stanzas.** The `GET {base_url}/models` fetch
  is gated on the ambient auth mode in `CODEX_HOME`, not on the flag, so a
  bring-your-own-key client fetches too. A catalog on disk swaps in a static
  models manager with no network path at all, which is also what makes the
  suite hermetic by construction.
- **`default_tools_approval_mode = "approve"`** on the MCP stanza, as the
  Direct topology's belt beside the annotations described above. Scoping the
  grant to the read tools was proposed and refused: under the forced
  `approval_policy = "never"` a writer tool still needs the grant, and the
  overlays would have stopped working silently.

The generator refuses the three input shapes whose output would be silently
wrong — a relative catalog path (Codex resolves it against the config's
directory, not ours), a base URL that does not end in the API prefix (turns
404 while the MCP handshake, derived from the same string, still succeeds and
makes the client look healthy), and a non-UTF-8 path. What it does not yet have
is an **operator entry point**: no CLI subcommand or admin route produces
these files, and whether that is a subcommand or an admin read beside key
minting is deferred by name.

### The gated real-binary suite

`crates/roundhouse-server/tests/codex_e2e.rs` spawns `codex exec` against a
loopback roundhouse with exactly that generated config and doubles nothing on
the client side:

```bash
timeout 300 cargo test -p roundhouse-server --features e2e-codex \
    --test codex_e2e -- --include-ignored --test-threads=1 --nocapture
```

`--features e2e-codex` compiles the file at all; `--include-ignored` opts into
spawning processes; `--test-threads=1` is not politeness, because each test owns
a `CODEX_HOME` and `codex exec resume --last` resolves "last" inside it. Once
opted in, a missing binary is a loud panic naming `ROUNDHOUSE_TEST_CODEX_BIN`
rather than a silent skip. No network is needed: loopback only, a pinned
catalog, no login, and a cleared child environment — the last asserted on the
constructed command rather than on the wire, because a credential that was
available but never consulted leaves every wire assertion green.

**Version vigilance.** The binary under test is `codex-cli 0.146.0` (tree
`e363b08`, 2026-07-28); the Cargo pin for the conformance crates is `6344a65`
(2026-08-13). Neither is an ancestor of the other, and the guard the earlier
pass-through ruling leaned on — "leave `requires_openai_auth` unset and codex
attaches nothing" — **exists at neither**. The version is printed on every run
and a mismatch warns rather than fails: a suite that silently passes against
0.146.0 and silently changes meaning against the next release is exactly the
drift the house vigilance rule exists to catch.

Running against the real binary disproved things source reading had accepted.
Codex wraps every tool result — `Wall time: …\nOutput:\n…` for MCP, a
`Chunk ID` / `Process exited` block for exec — before it becomes a
`function_call_output`, which meant `ToolFailureStreak` and `NoProgressRepeat`
could never fire on a real transcript; the wrapper is now stripped at one seam
before either signal reads an output, while the stored item stays the client's
verbatim bytes, because prefix admission depends on them.

### Codex as the compliance oracle

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
`ROUNDHOUSE_FRONTIER_UPSTREAM` is the same load-or-die posture for the
transport: unset serves the echo stub, `openai_responses` dispatches over the
real OpenAI Responses wire, and an unrecognised name is refused rather than
quietly demoted.

### The same numbers, in NeMo Relay's formats

Roundhouse's log is a better producer of Relay's interchange formats than Relay's
own exporter is: totally ordered, durable, and replayable from cold storage,
where theirs accumulates in memory and is lost with the process. So it emits
theirs rather than inventing parallel ones — a shared type is a conversation, a
copy is a fork — through three reads, gated by the same namespace check as every
other session route:

| Route | Document |
|---|---|
| `GET /v1/sessions/{id}/atof` | the **ATOF** event stream, NDJSON, one event per line |
| `GET /v1/sessions/{id}/trajectory` | one **ATIF v1.7** trajectory, by cold replay |
| `GET /v1/sessions/{id}/optimization` | one **`LlmOptimizationSummary`** per dispatched turn |

All three are pure functions of the log: no clock, no random ids, no engine
involvement. Two exports of one finished session are byte-identical, which is
what lets a consumer diff two trajectories to see what a re-run changed — every
identifier is a UUIDv5 digest of facts already in the log rather than a v4 or a
v7. Routing decisions ride a declared `data_schema` (`roundhouse/route`) as
`category: "context"` scope-ends rather than as marks, because that is the one
path the shipped NeMo-Agent-Toolkit converter copies a producer's schema into the
ATIF step's `extra` — as a mark our routing facts would arrive stringified and
structurally invisible.

Two accounting rules from the chapter above survive into these documents, which
is the point of producing them here rather than in a sidecar. **A forwarded
subscription seat is priced into no field at all**: its tokens ride the typed
contribution payload as a bare count, because roundhouse holds no rate card for a
seat. And the **capability gate's outcome is carried, never recomputed** —
`limitations[]` names the band the gate used, so a summary of ours never sits
indistinguishable beside an ungated one. Relay derives `status` from
`limitations`, so a locally-served turn always publishes as `Partial`; a hosted
turn on our own key, whose usage the provider reported, is the only shape that is
`Complete`, which is the honest reading rather than a defect.

The types come from `nemo-relay-types`, pinned at exactly `=0.7.3` — the
optimization surface and the whole ATOF envelope are byte-identical from there
through Relay's HEAD. That pin imposes `uuid = "=1.18.1"` on the entire
workspace, a six-release downgrade and a ceiling; the manifest records it and
names its unlock condition. ATIF is not in that crate — it lives in Relay's heavy
`crates/core` — so its twelve wire structs are ported under Apache-2.0
attribution, with a test pinning every field name against the upstream list so
drift arrives as a diff rather than as a consumer that cannot parse our export.

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

What has been adopted is ideas, with attribution and a pinning test rather than
a dependency edge: the two `ToolSignals`-derived trigger signals above, and the
`caller_auth_kind` conditional that `codex_launch`'s two auth kinds mirror.

## Examples

`examples/catalog.example.json` and `examples/control-plane.example.json` are
the two files an operator copies to start from — the rate card the router and
the dashboard share, and the tenancy file `ROUNDHOUSE_CONTROL_PLANE` names.
Neither is decoration: both are parsed by tests in this workspace, so an
example that stopped loading is a red suite rather than a support ticket.

`examples/agentic-api-mcp/` is a **compatibility test, not a demonstration of
the product**. It puts vLLM agentic-api's MCP client in front of roundhouse's
`/mcp` and drives one scripted turn through it, proving that our surface
survives a second gateway's MCP client, its tool-name flattening, and its
bearer forwarding. It proves nothing about routing, because in that topology
the Responses turn goes to agentic-api and roundhouse never sees it — the
topology that carries the product sentence is the one the real `codex` binary
exercises. Read its README before quoting any of it at anyone.

## Build and test

Requires the Rust toolchain pinned in `rust-toolchain.toml` (1.96.1) and system
`libzmq3-dev` (pulled in transitively by `dynamo-kv-router/standalone-selection`).

```bash
apt-get install -y libzmq3-dev   # or: brew install zeromq
timeout 900 cargo test --workspace
```

The `timeout` is not a formality. A hung test hangs the whole cargo run
silently, and the sessions most likely to hang one are the adversarial reviews
that mutate timeout and deadline code on purpose — break a timeout path and its
guard does not go red, it waits forever. A bounded run turns "stalled for hours"
into `exit 124` in minutes.

The first build clones `ai-dynamo/dynamo` to resolve the pinned Dynamo crates,
so expect it to take a while; later builds reuse the cached checkout.

That default run needs no GPUs, no worker processes, and no network: the
selection plane runs inside the test binary. Two families are opted into
explicitly, because each reaches something the default run must not assume:

- **Redis.** The store and spend-ledger contract suites are `#[ignore]`d. Set
  `ROUNDHOUSE_TEST_REDIS_URL` to a reachable Redis and pass `--include-ignored`;
  once opted in, an unreachable URL panics rather than skipping, because a
  silent skip is how a backend suite stops running without anyone noticing.
- **The real `codex` binary.** `--features e2e-codex` compiles
  `tests/codex_e2e.rs` at all, and `--include-ignored` opts into spawning
  processes — see the command under *Hooking up Codex*. It is off by default and
  deliberately not enabled by the crate's own dev-dependency, so a developer with
  no `codex` on PATH gets an empty test binary rather than a failure to explain.

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
- **Two tenants sharing a cache key do not share a session**, and a turn is
  attributed to the principal that paid for it — with the per-principal and
  deployment folds asserted to sum
  (`two_principals_using_one_cache_key_do_not_share_a_session`,
  `a_turn_is_attributed_to_the_principal_that_paid_for_it`).
- **A policy knob visibly changes routing** — a quality floor excludes a target
  the default policy would pick, and a filtered target never appears in
  `considered`, so savings are never priced against a model the key could not
  reach.
- **An exhausted budget degrades instead of failing** — the loudest test in the
  suite (`an_exhausted_frontier_budget_routes_local_instead_of_failing`); the
  member ceiling binds even when the project has room, a hold left by a killed
  turn expires, and a lost settle is repaired by the next open of the session.
- **A steered turn does not fork the conversation** — the synthetic call emits
  exactly four frames and no others, the resent call and its output *extend*
  rather than fork, and a third turn after a steer still matches its prefix.
- **A verdict never becomes a conversation item** — stored items are
  byte-identical around a validation, which is what stops every later turn
  forking; the side call books under its own model row and never reaches the
  cache ledger; a validator timeout releases the turn unchanged and is marked
  not free; and the cadence alone never fires without a signal.
- **An overlay cannot widen the ceiling** — an over-asking overlay is narrowed
  and says so, `fetch_steer` is byte-identical on a second call and makes no
  calls into the control reads, and another principal's steer is refused
  without naming it.
- **A quote never carries a secret** — asserted over both `Debug` and the
  serialized log; a principal with no credential for a provider never sees it
  among the candidates.
- **A minted key is returned once** and revoking it stops the key within one
  cache TTL; the budget view reports committed and measured separately, and
  drift goes negative and stays visible when a settle is lost.
- **A real `codex` binary drives all of it** — it completes the MCP handshake
  against our mount, executes our synthetic tool call and returns its output,
  resends the call and its output without forking the session, and the next
  turn reflects the correction; a steered turn's reported usage is the context
  it admitted, and a key revoked between runs stops the client.

## Not yet built

Roundhouse does not have WebSocket and gRPC transports. It cannot resume an
interrupted generation from its partial output, which is already durable in the
log. One real provider transport is wired (`openai_responses`); a second is a
value on `ROUNDHOUSE_FRONTIER_UPSTREAM` rather than a second variable, and it
does not exist yet.

The generated Codex launch config is a library function with no operator entry
point — no CLI subcommand and no admin route produces it — and the MCP surface
ignores the `_meta.threadId` a Codex client sends on every `tools/call`, because
`init_session` is the client-agnostic path and reading `_meta` is a
codex-native shortcut deferred to a plan of its own. The forwarded-ChatGPT-login
stanza is exercised with a crafted `auth.json`; no real login has been forwarded
through this code.

Metrics are per-process: the recorder folds what this node served plus whatever
it replayed from the sessions it opened. A fleet-wide view means either scraping
each node or running a shared fold over the Redis log rather than adding another
per-process counter. The dashboard also reports totals over all history with no
time-window selector, because the in-memory fold keeps no per-interval buckets.
A time window requires buckets in the fold, not a different query — which is
also why the reconciliation view's `measured_usd` cannot be windowed and says so
rather than pretending.

The admin plane has no audit trail, no key rotation without a service gap, no
per-key rate limiting (every ceiling today is dollar-shaped, so a local-only
principal has no volume ceiling), no pagination, and no credential CRUD.
