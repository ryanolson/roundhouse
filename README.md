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
> control surface; the validate/steer loop; real frontier provider clients on
> two wire dialects (OpenAI Responses and Anthropic Messages) and serve
> surfaces for both, so an unmodified Claude Code points at `/v1/messages` the
> way an unmodified Codex points at `/v1/responses`;
> a real `codex` binary driving all of it end to end behind a feature gate;
> emission of NeMo Relay's interchange formats from the same log; providers
> as configuration behind a per-provider client registry; rolling fair-use
> session windows; two-tier model selection with per-dispatch failover,
> steered by text rather than by a tool call; `topham`, the operator
> entry point that turns a saved profile into a running agent on either
> client and either topology; and the control surface wired to Claude Code —
> the MCP registration and the signage generated as argv, control calls
> correlated back to the turn that asked for them, and a real client
> dispatching one end to end.
>
> Not built: the WebSocket and gRPC transports, and resuming an interrupted
> generation from the partial output already durable in the log. Metrics are
> per-process. The MCP surface still ignores the `_meta.threadId` a Codex
> client sends on every `tools/call` — its Claude Code counterpart,
> `_meta["claudecode/toolUseId"]`, *is* read — and a control call chained
> through NeMo Relay is stated rather than tested.

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
| `roundhouse-server` | Turn engine and seven surfaces over one log — native HTTP/SSE, the OpenAI Responses API at `/v1/responses`, the Anthropic Messages API at `/v1/messages`, the MCP mount at `/mcp`, the admin REST plane under `/v1/admin`, `/v1/metrics` and its dashboard, and Relay's three session reads — plus `codex_launch`, `claude_launch` and `relay_handoff`, which produce the configuration each client and a chained Relay read, and the binary |
| `topham` | The operator entry point, and the one crate that depends *upwards* on `roundhouse-server`: profiles, `plan`, `launch`, `relay`, `mint`, and an interactive screen over the first three of those — `mint` takes the tenancy arguments (`--project`, `--user`) a profile deliberately does not carry, so it stays a subcommand. It reads the generators rather than restating them, which is what keeps `roundhouse-server/src/main.rs` free of a flag parser |

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

**Fair-use session windows** cap a project's or a member's draw over a rolling
`5h`, `24h` or `7d` window — the shape a frontier lab's own session limits use,
not a calendar reset the way `budget` above is. Configure a `"fair_use":
{"windows": [{"window": "5h", "max_tokens": ..., "max_usd": ...}]}` block at
project level and, separately, on any key for a per-member ceiling on top of
it (see `examples/control-plane.example.json`); each window needs at least one
cap, or it is rejected at load for reading like a limit while enforcing
nothing. A project's and a member's windows are two independent ceilings that
both bind — the narrower one refuses first. A turn over its window gets HTTP
429 `fair_use_exceeded` with `error.type: "usage_limit_reached"`, naming the
scope, the window, the quantity that ran out, and `resets_at` rounded *up* to
the earliest second the window could have room — retryable, and no grant is
taken for a refused turn. Enforcement is single-node only in this milestone
(the counters live in the process's memory), so a deployment that sets
`ROUNDHOUSE_REDIS_URL` while also configuring `fair_use` gets a boot warning
that two nodes serving one project enforce two independent ceilings rather
than one shared one.

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
resolution as every other route — the turn key, presented however that client
presents it on a turn: a bearer for Codex, the dedicated header for Claude
Code, and the same `ControlPlane::scope` for both; a `GET` on it is 405.
**Which conversation a call concerns** is answered in a fixed order: a
`conversation` argument the model wrote, then the id of the `tool_use` block the
call is answering where the client sends one, and only then the caller's most
recent conversation. See "Control tools from Claude Code" below for why the
middle rung exists.

Every descriptor states all three MCP annotations — `readOnlyHint`,
`destructiveHint`, `openWorldHint` — with the last two `false` on all eight,
because the tools reach nothing outside this deployment and their writes only
narrow. That is not decoration: under `approval_policy = "never"`, which
`codex exec` forces, a Codex client treats a tool it sees no annotations on as
destructive and open-world and **cancels** the call, handing the agent a
cancellation notice where the output should have been.

`fetch_steer` is how a correction is *re-read*, not how it arrives — the
correction itself is delivered in-band, as the text of the steered turn's own
answer. The tool is a pure read of the most recent guidance in the caller's
log: twice gives the same bytes and does no paid work, so an agent (or a
human debugging one) can ask "what was I just told" without the asking being
billable.

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
badly". Roundhouse's own control calls — the `mcp__roundhouse__*` surface
above — are their own category and count toward none of these: an agent
polling `status` or adjusting its preferences is talking to us, not stuck, and
a genuinely unknown tool still counts as unrecognised. Recognising them takes
two tests rather than one, because the two clients spell the same call
differently by the time it reaches the log: the flat `mcp__roundhouse__<tool>`
a Claude Code session stores, and the bare tool name left after the Responses
wire has dropped the namespace into a field canonicalization discards. The bare
half can only ever check a name against the list of the eight, so a third
party's MCP server offering its own `status` is exempted too — an under-count
of a call or two, chosen over the failure it replaced, which was counting every
control call a Codex client made as work on the task and steering an agent that
had done nothing wrong.

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

**Outcome B is a text instruction.** The held turn *completes* — never fails,
because only a completion registers as a completed turn and an incomplete one
would re-enter the interjection on every retry — answered by an assistant
message carrying the rendered directive and then the pending request restated
as quotation, so the harness sees the guidance and the task in one place and
nothing in the restated request can read as roundhouse's own voice. The
guidance is an ordinary stored item: the next turn's resend admits it as
prefix, which is also how fulfillment is known, and the turn that fulfils a
steer is never itself validated. The steer used to be a synthetic tool call
into the MCP surface; that channel is retired — a config that still says
`channel = "tool_call"` is refused at load by name rather than silently
remapped — because a tool call has two cooperation points that fail silently
(the client must dispatch it, the model must heed the fetched output), and a
real client demonstrated both. Text has neither, and works for any client
with nothing but a provider stanza. A second, narrower surface exists for
route escalations: a project that configures `handoff_note` gets one gated
`[roundhouse-guidance]` sentence appended to the *forwarded request only* on
the first turn of a signal-driven escalation — never the stored conversation,
never accumulating, and never narrating a move no signal asked for.

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

## Selecting the model

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

Four properties of the lists are worth knowing before writing one:

- **Admission runs first and a tier can only narrow.** A target this key's
  policy, quality floor, or credentials do not admit is skipped and never
  resurrected; a tier that empties entirely falls to the other one, with the
  decision's rationale saying so — and the degrade-to-local promise survives
  any recipe: a spent cadence or budget still serves the local candidate even
  when no tier names it, because that is the one promise the configuration
  file itself makes.
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
- **A `local/` entry needs a fleet, and so does the shipped example.** The
  binary in this repository attaches none — it quotes the catalog and nothing
  else — so `examples/control-plane.example.json`, whose efficient tier names a
  local model and whose cadence promises local service on a spent window,
  describes a deployment that has one. Pointing `ROUNDHOUSE_CONTROL_PLANE` at it
  from a fleetless process is refused at boot, naming the keys and the capacity
  they do not have, rather than started with a cheap tier that is empty on every
  turn while the rationale blames the key's admission for it.

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
against one inferred from the catalog — and only **through the capability
gate**: declared-and-gated prices on the `Declared` basis, declared-and-refused
is `Unpriced` naming the model and the band, and an unresolvable value is
recorded verbatim while pricing falls back to inference on the `Inferred`
basis, never a silent upgrade. No line of routing reads it.

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
makes the client look healthy), and a non-UTF-8 path.

The **operator entry point** that calls it is `topham launch` — see
[Launching with topham](#launching-with-topham). It resolves a saved profile,
writes these two files into that profile's own `CODEX_HOME`, and `exec`s the
client; `topham plan` prints the same resolution and spawns nothing.

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
catalog, no login, and a cleared child environment. That last one is asserted
on the constructed command's key set, and the M11.2b review pinned exactly how
far that reaches: `Command::get_envs()` reports only the explicit additions, so
the guard checks what the harness adds and cannot see a dropped `env_clear()`
or an ambient variable riding through it. The shared harness in
`tests/common/e2e.rs` carries that fact as a test; the Claude suite closes the
gap on the wire (an ambient credential shows up as an `authorization` header
the seat test refuses), while here no credential is ever consulted, so the
wire has nothing to show — a documented gap, not a covered one.

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

## Hooking up Claude Code

`POST /v1/messages` is the same idea in the other dialect: an Anthropic Messages
surface over the same event log, same engine, same admission, same prefix check.
`/v1/messages/count_tokens` is served from this deployment's own tokenizer,
because the client's fallback when that route is missing is a real one-token
create against the routed model.

`roundhouse_server::claude_launch` is `codex_launch`'s sibling for the client
that speaks it. Two things about it are different, and both are facts about the
client rather than choices:

- **It writes no file.** Claude Code's whole redirect surface is environment —
  `ANTHROPIC_BASE_URL`, read by its vendored SDK, and `ANTHROPIC_CUSTOM_HEADERS`,
  a newline-separated `Name: Value` block merged *after* the SDK's own auth
  headers — so the output is an environment map and there is no settings overlay
  beside it.
- **The base URL is the deployment root, not the API prefix.** The SDK appends
  `/v1/messages` itself. A value that already carries the prefix is refused by
  name, which is the exact inverse of the codex generator's refusal and for the
  same reason: each client's constructor refuses the shape *its* client cannot
  use.

The turn key therefore rides the map rather than a variable *name* the way the
codex config does, because `ANTHROPIC_CUSTOM_HEADERS` offers no indirection. The
"secrets ride env only, never a file" rule is kept by the types instead: the key
is held as a redacting `Secret`, the rendered map has no `Serialize` and no
`Display`, and one documented seam yields plaintext to whatever spawns the
process.

- **`ClaudeAuthKind::RoundhouseKey`** sets `ANTHROPIC_API_KEY` to a roundhouse
  sentinel, the analogue of writing `env_key` beside `requires_openai_auth =
  false`. A subscription login is suppressed only when one of five inputs
  resolves, and `ANTHROPIC_BASE_URL` is not one of them — so an empty variable
  means a logged-in user's OAuth bearer is presented to *our* base URL, with no
  host check anywhere on the client's inference path. Two limits are recorded
  rather than reconciled: interactive mode prompts once before the key overrides
  a subscription, and `CLAUDE_CODE_REMOTE=true` defeats the suppression entirely,
  so a launch inside a Claude Code Remote container forwards a login whatever it
  says on the tin. The sentinel is safe to set because the admission boundary
  treats it as inert: it lives beside the forwarding gate that refuses to pass
  it upstream, so it can never arrive at Anthropic as an `x-api-key` beside a
  real seat — a `401` that reads exactly like a revoked login.
- **`ClaudeAuthKind::ForwardedClaudeLogin`** sets no API key and **refuses** a
  launch that also carries one of the five suppressing inputs, returning the same
  list in the form a launcher can enforce. Each of the five leaves every request
  valid, so the run looks healthy and the seat simply never arrives. The three
  cloud-provider selectors are refused under *both* kinds and with their own
  message, because they defeat the redirect rather than the login: the client
  never reads the base URL, never reaches this deployment, and answers anyway.
- **The MCP wiring is generated too, and it is argv rather than a file.** Two
  arguments go in front of the operator's own: `--mcp-config` with the
  registration inline, and `--append-system-prompt` with the signage. See
  "Control tools from Claude Code" below.
- **Chained through NeMo Relay, the same map is what is handed to the client.**
  Relay overwrites `ANTHROPIC_BASE_URL` with its own gateway and *merges* its
  proxy token into `ANTHROPIC_CUSTOM_HEADERS` — a line-wise replacement of the
  matching name only — then forwards headers it does not own untouched and
  strips its own credential before the hop. So the turn key survives on its
  dedicated header, a chained turn keeps the Direct semantics exactly, and one
  generator serves both topologies; Relay is aimed at this deployment's root
  through `[upstream] anthropic_base_url` with no auth header of its own. That
  is asserted against a real Relay rather than argued from its source. The
  module doc carries the runbook: the fallback wiring for a credential-less
  client (where Relay carries the key in `Authorization` and the turn is
  key-authed only), the two hazards that are documented refusals rather than
  guards, and why resumption is not offered in band on this surface.

Like the codex generator, this is a library function — and the thing that calls
it is `topham launch` for the Direct topology and `topham relay` for the
chained one, both handing the client the *same* generated map. See
[Launching with topham](#launching-with-topham).

The surface the map points at is shaped by three further facts about the same
client, and each is a cost rather than a preference:

- **It has no field to name its session with.** There is no
  `prompt_cache_key` on this wire, so the session is resolved from
  `x-claude-code-session-id`, then from `metadata.user_id` — which has shipped
  in two different spellings and is parsed in both, because a client upgrade
  that re-keyed every session would silently cold-start every warm prefix.
- **A malformed stream costs a whole extra turn, not an error.** Claude Code
  dispatches SSE frames on the `event:` name and drops a frame that has none in
  silence; a stream it cannot consume triggers a second, non-streaming request
  for the same turn at full price. So the emission is shaped to make the
  ordering mistakes its accumulator throws on unreachable rather than merely
  untested, and every stream the suite produces is judged by a strict
  conformance reader written from the pinned spec — the tier-1 oracle, which
  exists because both official SDKs are deliberately non-validating and would
  agree with anything we sent them.
- **Its accounting axes are not ours.** Anthropic's three input counters are
  disjoint and roundhouse's nest cached and written input inside the total, so
  the projection subtracts rather than forwards. Getting that backwards reports
  a warm turn as nearly two cold ones, in the direction that flatters the
  savings figure.

What is not here: no `/v1/models` (see the status note). The evidence for the
client's shape is request bodies captured from the shipping binaries through a
loopback mock, which live in `crates/roundhouse-server/tests/fixtures/` and are
driven through the surface on every run. **Two client lines are pinned, not
one** — 2.1.251 and 2.1.257 — and every fixture-driven test runs against both,
because a suite pinned to one answers either "does this still serve the client
it was written against" or "does it serve the client shipping today", never the
question a mixed fleet actually asks. The one shape difference between them is
what the current line appends after each `--continue`'s new question: a
remaining-budget notice it rewrites per request, which is ephemeral and
therefore never becomes a log item — a counter admitted as history forks the
session the first time it counts down, and every turn still answers.

### Control tools from Claude Code

The same eight tools the Codex client reaches over `/mcp`, reached by this one —
and everything that differs is a fact about the client.

**The tool name is flat, and the log stores it flat.** Codex sends an MCP call
as a bare `name` plus a separate `namespace` field; Claude Code folds the two
into one string, `mcp__roundhouse__status`, everywhere — in the `tools[]` it
declares, in the `tool_use` block it emits, and in the `--allowedTools` grant
that permits it. `ClientDialect` is one arm per surface saying which, and the
Messages surface keeps the flat name whole: nothing renders a tool call
outbound any more, so the only reader of a stored name is the validate loop's
control-traffic exclusion, and splitting on the way in would move the `turn_id`
of every already-stored tool-using session for no reader's benefit. The
exclusion learned to recognise both spellings in the same change — before it,
every control call made over the *Responses* wire was counted as the agent's
work, because the namespace the flat prefix test looked for had been dropped at
canonicalization.

**A call is correlated by the tool-use id it is answering.** Claude Code puts
`_meta["claudecode/toolUseId"]` — the `tool_use.id` roundhouse itself emitted —
on every `tools/call`. That names exactly one conversation, so a `status` from
inside a subagent's tool loop resolves to the subagent's log rather than to
whichever of the principal's turns opened most recently. The binding is written
as the call is streamed to the client, which is the one moment both halves are
in one place; the id is checked against the caller, and one that is not the
caller's is indistinguishable from one this node never emitted. A call carrying
no such key — every Codex call — falls back to the principal's most recent
conversation exactly as before.

**The registration is inline argv, and the key rides `${VAR}`.** Of the config
forms this client honours, `--mcp-config` is the only one that writes nothing:
a project `.mcp.json` would land in the operator's own repository, and a
`settings.json` `mcpServers` key is silently inert at 2.1.257 — verified, not
assumed. The header value is the literal `${ROUNDHOUSE_API_KEY}` (whatever the
profile's `key-env` names), expanded by the client from the environment the same
launch laid, so the secret is in no argv, no file, and no process listing; the
unexpanded-literal hazard is closed by `topham` refusing a launch whose key
variable is not exported. `--strict-mcp-config` is a profile switch, off by
default, because it drops *every* other MCP configuration and not merely a
colliding one.

**Signage rides `--append-system-prompt`.** The Claude analogue of the codex
skills directory, and a single appended block rather than a directory for two
reasons the client forces: a skills listing arrives as an interior system
message this surface admits strictly, so editing it would fork every live
session, and owning `CLAUDE_CONFIG_DIR` to write into evicts the login a
forwarded-login launch exists to forward. The text names the eight tools and
the *occasion* for each, never their descriptions — those already ride in
`tools[]` on every request, and a second copy would cost the fleet the same
context twice per turn.

**What the launcher will not decide for you.** Headless, this client
synthesises a permission refusal for an `mcp__*` tool its own argv does not
name — no request reaches `/mcp` at all — so a `-p` run needs
`--allowedTools mcp__roundhouse__status` (and `--dangerously-skip-permissions`
is refused outright when running as root). `topham plan` says so in its notes
rather than inventing a grant, and an operator argv that repeats a flag the
launcher generates is refused by name instead of silently shadowing it.

**Roundhouse adds no tool of its own to a Messages request.** The client's
`tools[]` is forwarded verbatim; anything injected there is a name the client's
own loop cannot dispatch, and it would move the admitted input token count the
client was quoted on.

### The real client, on both topologies

`crates/roundhouse-server/tests/claude_e2e.rs` is `codex_e2e`'s sibling: it
spawns the real `claude` binary against a loopback roundhouse with exactly the
environment `claude_launch` generates, and doubles nothing on the client side.

```bash
timeout 300 cargo test -p roundhouse-server --features e2e-claude \
    --test claude_e2e -- --include-ignored --test-threads=1 --nocapture
```

Real: the binary, the socket, the surface, the control directory and its minted
turn key, the log, the prefix check, and the tool the client chose to run.
Scripted: only the frontier, so the suite decides when a `tool_use` block is
emitted rather than asking a model to decide. The child's environment is
*cleared* and rebuilt from the generated map plus five named isolation
variables, because inside a Claude Code Remote container the ambient
`CLAUDE_CODE_REMOTE=true` would make the client present that container's managed
OAuth token to whatever base URL it was handed. Two guards stand behind that,
and they catch different things: a no-binary test asserts the key set of the
*constructed command* with `==`, which checks the generated map and only that —
`Command::get_envs()` reports the explicit `env()` diff and reports it
identically whether or not the clear ran — so a dropped `env_clear()` or an
ambient leak is caught by the wire test instead, as an `authorization` header on
a request that reached this deployment.

One of its tests is the closure of the paragraphs above: a real client,
launched through a real `topham`, is answered with a `tool_use` for
`mcp__roundhouse__status`, dispatches it against this deployment's own `/mcp`
mount, and comes back. Both routers are on the one socket, and the assertions
are at both edges — the turn key arrived on the control call, the flat name was
split back apart on the MCP wire, the answer named the conversation the call was
made from rather than the principal's most recent one, the resend rejoined the
session, and the validate fold counted none of it as the agent's work. The
"rather than the most recent one" half is not free: a rival conversation of the
same principal's takes the most-recent slot in front of every control call, so
an implementation that guessed would answer green about the wrong log, and does
— that is what the assertion catches when the correlation is removed.

Two of its tests drive the **chained** topology through a real NeMo Relay
(`ROUNDHOUSE_TEST_RELAY_BIN`, `nemo-relay run --agent claude`), and that is
where the chained wiring stopped being an argument from Relay's source: the turn
key arrives on its dedicated header, Relay's own proxy credential never leaves
Relay's gateway, `?beta=true` survives the base-URL concatenation, and a
`--continue` through Relay's alphabetizing re-encode extends the session rather
than forking it.

## Launching with topham

Both generators above are library functions, and until `topham` nothing an
operator could run produced their output. `topham` is that: one binary, above
the server in the dependency graph, that turns a saved profile into a running
agent.

```bash
topham mint --profile work --project acme --user ada   # prints an export line
export ROUNDHOUSE_API_KEY=rh_turn_…                    # the key rides the environment
topham plan work                                       # what it resolves to; spawns nothing
topham launch work -- -p "hello"                       # becomes the client
topham relay chained -- -p "hello"                     # becomes nemo-relay running the client
topham                                                 # plan, launch and relay, on a screen
```

**A profile names things and never holds a secret.** It is TOML under
`$XDG_CONFIG_HOME/topham/profiles/<name>.toml` (else `~/.config/…`, the rule
NeMo Relay follows) and carries an agent, a deployment root, an auth kind, the
**name** of the variable the turn key is read from, a topology, and for Codex an
optional model slug and catalog path:

```toml
agent = "claude"                        # claude | codex
deployment-root = "http://127.0.0.1:8080"   # the root, with no /v1
auth = "roundhouse-key"                 # roundhouse-key | forwarded-login
key-env = "ROUNDHOUSE_API_KEY"          # a name, never a value
topology = "direct"                     # direct | chained
strict-mcp = false                      # claude only: drop other MCP servers
```

A profile carrying a `rh_`-shaped value is **refused on load, naming the
field** — before deserialization, so a key parked in a field this vocabulary
does not have is still found. A configuration directory is exactly what ends up
in a dotfile repository, and nothing downstream can tell that copy from a live
credential. `topham mint` writes nothing to disk for the same reason: it posts
to `/v1/admin/projects/{p}/members/{u}/keys` with an admin key from
`ROUNDHOUSE_ADMIN_KEY` and prints the `export` line for the profile's variable.

**The refusals are the point of it being a program.** A launcher that merely
exported three variables would not catch any of these, and every one of them
fails by *running*:

- a `ForwardedLogin` profile next to an ambient `CLAUDE_CODE_USE_VERTEX`
  forwards nothing while every request stays valid. `topham launch` checks
  `ClaudeLaunch::must_be_unset` against the operator's own environment — the
  generator's table, not a copy — and refuses before it writes or spawns
  anything;
- a `RoundhouseKey` profile with no key exported reaches roundhouse with no
  credential, which roundhouse *admits*, degrading the turn to local-only
  routing rather than refusing it. Refused when the profile is resolved;
- `topham launch` on a chained profile, and `topham relay` on a direct one, are
  each refused naming the other subcommand: both would work, and both would run
  a topology the profile does not describe;
- `topham relay` runs the same isolated `nemo-relay run --dry-run` preflight the
  gated suite runs and refuses when `/etc/nemo-relay/config.toml` has re-aimed
  the upstream — the system layer is folded in *after* an explicit `--config`
  and wins — and it refuses an ambient `NEMO_RELAY_ANTHROPIC_BASE_URL`
  separately, because that layer sits above `--config` and the preflight
  deliberately clears it;
- `topham launch` on a claude profile also **generates argv**, not only
  environment — the MCP registration and the signage — and refuses an operator
  tail that repeats one of those flags. Both orders of a duplicated
  `--mcp-config` produce a session that runs: one where the control surface is
  silently absent, one where the operator's own servers are, and neither is
  reported by anything;
- `topham launch` and `topham plan` also read the settings files the client
  itself will load — `$CLAUDE_CONFIG_DIR/settings.json` (else
  `$HOME/.claude/settings.json`), `./.claude/settings.json` and
  `./.claude/settings.local.json` — and refuse one whose `env` block would
  override a generated variable or set a suppressor, naming the file and the
  key. An administrator's managed-settings file is deliberately *not* read: it
  is outside the operator's control and its path is platform-specific in a way
  nothing here verified, so that one layer is stated in the plan's notes rather
  than enforced.

**`topham plan` prints the whole resolution with every secret redacted**, and
the redaction is the generators' own `Debug` rather than this launcher's: the
turn key renders as `redacted:<fingerprint>` and any declared ambient variable
as `<set>`. The generated argv is printed one argument per line with the key
variable **unexpanded** — what is passed, not what it becomes — and the signage
is named by length rather than printed, the same call the plan already makes
about codex's generated `config.toml`. It also prints the limits no refusal can
close — under a subscription login an *interactive* Claude Code session asks
once before it will use the API key (the same gate that makes it ask before
calling a control tool), a headless one needs `--allowedTools` naming the tool
before it will call one at all, and a chained **codex** run is unproven because
Relay splices its own `--config model_provider=…` onto the client's argv and
that override outranks the generated `config.toml`.

`topham` with no subcommand opens an interactive screen: the profile list, an
editor for the fields above, a plan pane rendered from the same redacted
resolution, and launch/relay actions. Every action on it is a subcommand a
script can run, and the screen owns no state the profile files do not — the
list is re-read from the directory after every write. Its state transitions are
pure functions over key events and are tested without a terminal; what is left
is the draw-and-read loop.

### What proves it

`topham`'s own suite covers the profile round trip and the secret refusal,
whole-output plan snapshots for both agents and both auth kinds, the
`must_be_unset` refusal naming the variable, the env layering (a generated
variable beats an ambient one of the same name; an unrelated ambient variable
survives), and `mint` against the real `admin_router` on a loopback socket.

Above that, the gated real-binary suites close the loop the launcher exists to
close. `claude_e2e` drives the real client *through a real `topham`* — a
`topham launch` on Direct, a `topham relay` on Chained, and a third that adds
the control surface — and asserts at roundhouse's edge exactly what the
hand-built tests assert. The child they spawn is handed a turn key, two homes
and a `PATH` and **no `ANTHROPIC_*` variable at all**, so a launcher that
resolved the profile wrongly cannot pass by inheriting anything; the control
run adds the argv half of that, since nothing but the launcher registers the
`/mcp` mount with the client. `codex_e2e` gains the same shape (no `codex`
binary is available where it was written, so it has never been run):

```bash
cargo build -p topham
ROUNDHOUSE_TEST_TOPHAM_BIN=$PWD/target/debug/topham \
ROUNDHOUSE_TEST_CLAUDE_BIN=… ROUNDHOUSE_TEST_RELAY_BIN=… \
    timeout 900 cargo test -p roundhouse-server --features e2e-claude \
    --test claude_e2e -- --include-ignored --test-threads=1
```

A missing `ROUNDHOUSE_TEST_TOPHAM_BIN` under `--include-ignored` is a loud panic
naming it, never a silent skip — and it names a *freshly built* binary on
purpose, because a stale one reports green for code nobody compiled. That is now
visible rather than merely warned about: `topham --version` prints the commit
the binary was built from, and the suite compares it against `HEAD` and warns
when the two disagree.

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
transport: unset serves the echo stub, `openai_responses` dispatches to real
providers, and an unrecognised name is refused rather than quietly demoted. Its
name is historical — when it was written there was one client, so naming the
wire and switching real dispatch on were the same act. The provider registry
made the dialect a per-catalog-entry fact, so the value is now a switch and each
provider's wire comes from its entries' `wire_protocol`; it keeps its spelling
because renaming it would break every deployment's environment for cosmetics.

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
that would have hit the gap. A third is asked at the composition root, because
it is a fact about the binary rather than about the file: the dialect each
provider's entries declare has to be one this build compiled a client for, and
a provider whose entries declare *two* is refused as well — the registry holds
one transport per provider name, so a provider serving both wires (OpenRouter
serves `/responses` and `/messages`) is written down twice, under two names
pointing at the same base URL. A rolling-pointer model id (OpenRouter's
`~`-prefixed aliases) is refused at load for the same reason a duplicate
identity is: it mis-prices every turn after the upstream re-points it. The key
itself is never written here: `auth.env` only names the environment variable it
is expected to arrive in, so a configured provider with no key anywhere is a
boot warning rather than a surprise found one turn at a time — the credential a
turn actually authenticates with is still resolved per turn from the control
plane's deployment/project/member tiers.

**Sourcing `quality_prior`.** `FrontierModelSpec::quality_prior` is
configuration, not measurement, and `import-benchmarks` (a binary target in
`roundhouse-fleet`, not linked into any shipped binary) is what lets that
figure be sourced instead of guessed. It reads OpenRouter's
`GET /api/v1/benchmarks` (`OPENROUTER_API_KEY`) and writes two files: a catalog
fragment with each `quality_prior` normalized to `0.0..=1.0`, and a paired
provenance record naming the index, its snapshot date, and the attribution
OpenRouter requires when the data is republished. An entry it cannot attribute
at all — neither a `meta.citation` nor the item's own `source` discriminator —
is refused rather than emitted uncited; a null `meta.citation` is the ordinary
multi-source response and is emitted with each entry's `source` beside it. It
is configuration generation, never a runtime dependency: nothing in a shipped
`roundhouse` binary calls OpenRouter, and its own tests run entirely offline
against a committed response fixture.

**Republishing an imported number means shipping its provenance file.** The
catalog fragment carries model identity and `quality_prior` and nothing else —
deliberately, since a catalog entry is `deny_unknown_fields` and an attribution
field on it would be a schema this project invented for someone else's data.
The attribution lives in the paired `quality-prior.provenance.json`, so keep
the two files together: the server looks for that file *beside the file
`ROUNDHOUSE_CATALOG` names* and, when it finds one, renders its citation under
the dashboard's savings figure. No file, no line, and never a boot failure —
the catalog is named by an operator and load-or-die, while this one is
discovered, and a discovered file must not be able to stop a deployment
starting.

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
a dependency edge: the two `ToolSignals`-derived trigger signals above, the
`caller_auth_kind` conditional that `codex_launch`'s two auth kinds mirror, and
— the part that does not move — the **coding-agent scorer** and its constants,
ported into `roundhouse-core/src/routing/stage.rs` with the upstream revision
named in the module's attribution and pinned by a test that reads the
attribution text rather than its own literals. That is the asset: the table of
error patterns and the calibration behind the thresholds are trace-mined rather
than reasoned, so an editorial improvement made on the way across would be an
unmeasured heuristic wearing a measured one's provenance. Every divergence is
documented at the divergence: `compacted` has no input in this tree and so the
hard escalate is severity-only; `turn_depth` is the exchange count rather than
the message count; and upstream's `ConsultClassifier` outcome is folded away
entirely, because routing here makes no model calls — an undecided turn lands
on the picker's default tier and is marked as having got there by falling open,
which is what stops the handoff note narrating it.

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
- **The real `claude` binary**, the same way, under `--features e2e-claude`;
  its chained tests additionally need `ROUNDHOUSE_TEST_RELAY_BIN`, and its two
  closure tests a built launcher — `cargo build -p topham`, then
  `ROUNDHOUSE_TEST_TOPHAM_BIN=$PWD/target/debug/topham`. That variable has no
  `PATH` fallback: `topham` is installed nowhere, so a bare name would resolve
  to whatever a developer happened to have.

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
- **A steered turn does not fork the conversation** — the guidance answer is an
  ordinary stored item, the next turn's resend admits it as prefix, the turn
  that fulfils a steer is never itself validated, and a config still naming the
  retired `tool_call` channel is refused at load by name.
- **The tier a turn lands on is read from the session, not asserted** — a
  stalling session is driven through the real signal extractor as *items* and
  comes out on the capable tier; a session that produced work and passed its
  tests comes back down; a quiet one falls open; and roundhouse's own control
  calls count toward none of it. Each has a control that varies only the
  exchanges, so none of them is a test about the picker.
- **A dead provider costs one attempt, not the turn** — a transport failure
  advances to the next candidate of the same tier inside one turn, one deadline
  and **one grant settled once**; the failover crosses transports as well as
  targets (the second attempt goes out through the second provider's own
  client); a refusal, a 401 and a 404 fail where they stand; an exhausted tier
  fails with every attempt on the record — the terminal failure included — and
  the degrade-to-local promise survives any recipe.
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
  against our mount, prints our steering directive as its own answer, admits
  the guidance as prefix on a resumed run without forking the session, and the
  fulfilling turn is never validated; a steered turn's reported usage is the
  context it admitted, a key revoked between runs stops the client, and the
  flat tool name codex resolves for a generated skill equals what
  `codex_launch` renders.
- **A profile a person wrote reaches the wire** — a real `claude` launched by a
  real `topham`, from hand-written TOML in an isolated configuration directory
  and a child environment carrying no `ANTHROPIC_*` variable at all, arrives
  with the turn key on its dedicated header, the sentinel inert and no bearer;
  the same profile marked chained goes through `topham relay`'s own generated
  wiring and preflight and arrives through Relay's gateway with all of that
  intact. That is the link every other launch test leaves open: not that the
  generated map works, but that something an operator can run produces it.

## Not yet built

Roundhouse does not have WebSocket and gRPC transports. It cannot resume an
interrupted generation from its partial output, which is already durable in the
log. Two real provider *dialects* are wired — `openai_responses`, which
OpenRouter's GA `/responses` route also speaks, and `anthropic_messages`, which
OpenRouter's GA `/messages` route also speaks — with one registry client per
configured provider, and the dialect read from each catalog entry. A
chat-completions client is the one remaining `WireProtocol` arm with no
transport; the composition root's dialect gate is an exhaustive `match`, so
writing that client is a compile error there rather than a silent
mis-dispatch. On the Anthropic wire roundhouse now serves as well as
dispatches, and a real `claude` binary has driven it end to end on both
topologies (the gated `claude_e2e` suite, the counterpart of `codex_e2e`) —
what that surface still does not do is worth stating plainly. `/v1/models` is
deliberately not served, so a client with gateway model discovery enabled sees
no catalog:
exposing roundhouse's routes in a user's `/model` picker is a product decision
that has been deferred rather than made. Fair-use enforcement is
single-node: the rolling window
counters live in process memory, and the Redis implementation is deferred by
name with a boot warning where it matters.

The generated launch configuration now has an operator entry point — `topham`,
above — and a launched Claude Code now reaches the `/mcp` mount as well as the
Messages surface, proved by a real client dispatching a control call through a
real `topham`. Three things around that are still stated rather than solved.
**A control call chained through Relay is untested**: the direct closure run is
the one that exists, and whether Relay's gateway leaves a second protocol's
requests and their `Mcp-Session-Id` framing alone is a claim nothing here has
made. **Chained codex is unproven**, because Relay splices
`--config model_provider=…` onto the client's argv and a codex `--config`
override outranks the generated `config.toml`, so the turn-key header that
config names is not what the client presents; `topham plan` says so on that
profile rather than refusing it, since the remedy (Relay's own
`openai_auth_header`) is the fallback wiring the runbook records as deliberately
untested. And a `RoundhouseKey` profile under an existing subscription login
still asks once in an *interactive* session before the API key is used — the
same gate that makes an interactive run ask before it will call an
`mcp__roundhouse__*` tool at all, where a headless one is unblocked by
`--allowedTools`. Both prompts are stated in `topham plan`'s output, not
solved.

The MCP surface still ignores the `_meta.threadId` a Codex client sends on every
`tools/call`, because `init_session` is the client-agnostic path and reading
`_meta` is a codex-native shortcut deferred to a plan of its own. The Claude
Code counterpart, `_meta["claudecode/toolUseId"]`, *is* read, and it is not the
same bargain: it carries an id *roundhouse emitted*, so it needs no cooperation
from the model, and a caller presenting one that is not theirs learns nothing
from it. The forwarded-ChatGPT-login stanza is exercised with a crafted
`auth.json`; no real login has been forwarded through this code. The same is true of the Anthropic pass-through row that
landed with the Messages client: the four headers it admits are asserted against
a mock upstream on a real socket, and no real Claude subscription seat has been
forwarded through it.

Metrics are per-process: the recorder folds what this node served plus whatever
it replayed from the sessions it opened. A fleet-wide view means either scraping
each node or running a shared fold over the Redis log rather than adding another
per-process counter. The dashboard also reports totals over all history with no
time-window selector, because the in-memory fold keeps no per-interval buckets.
A time window requires buckets in the fold, not a different query — which is
also why the reconciliation view's `measured_usd` cannot be windowed and says so
rather than pretending.

The admin plane has no audit trail, no key rotation without a service gap, no
pagination, and no credential CRUD. (Per-key *volume* ceilings exist now — a
fair-use window with `max_tokens` is exactly that — but request-rate limiting
still does not.)
