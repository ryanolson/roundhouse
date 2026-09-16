<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Long-term roadmap: the agentic platform and session-aware KV lifecycle

> **Status: direction and delivery sequence, 2026-08-19.** This roadmap starts
> from the implementation on `main`: control-plane milestones M0-M6 from
> `PLAN-agentic-control-plane.md` landed in `4413ba9`; M7-M9 remain. It folds
> in the two synergy rulings and adds the missing cross-plane contract between
> agent activity, Roundhouse sessions, fleet placement, and Narwhal/KVBM cache
> residency. Milestone labels here are delivery gates, not calendar promises.

Inputs reviewed: `PLAN-agentic-control-plane.md`,
`synergies/nemo-relay.md`, `synergies/ecosystem-round-2.md`, the current
Roundhouse fleet seam, Rhino's Narwhal/KVBM and cross-tier design, the pinned
Codex source at `6344a65`, and the current llm-d primary sources linked in
§2.3.

## 1. Decision summary

1. **Narwhal is the worker-local scheduler and KV-lifecycle authority.** It
   already owns request admission, pause/evict/resume, eviction permits, and
   the block-lifecycle callbacks that KVBM uses to propagate reset and eviction
   signals. Roundhouse must not recreate that logic.
2. **Roundhouse owns durable agent-session intent.** It observes client and
   turn activity, applies tenant policy, persists lifecycle decisions, and
   sends idempotent archive or warm *hints*. It never manipulates GPU blocks
   directly.
3. **Fleet placement and worker-local scheduling are separate layers.** The
   embedded Dynamo selector answers which worker should receive a request.
   Narwhal answers what runs and what cache moves inside that worker. Calling
   both of them “the scheduler” hid the interface this roadmap needs.
4. **llm-d is not the default scheduler of record.** Evaluate it as an
   optional fleet-router and interoperability adapter after the Narwhal
   lifecycle contract works end to end. Do not replace Narwhal, and do not
   adopt an llm-d cache index as a second source of truth for Narwhal-owned
   residency.
5. **A quiet WebSocket is evidence, not proof that a human is away.** Archive
   only after a completed turn, a configurable grace period, and safety
   checks. Cancel stale work with a monotonic session epoch. Wake-up is
   similarly debounced and bounded by the latency benefit it can still buy.
6. **Do not wait for Codex to add a new signal.** The pinned Codex source
   already reuses a Responses WebSocket across turns and sends
   `session_id`/`thread_id`/`turn_id` metadata. Roundhouse can implement a
   conservative traffic-derived signal first, while separately proposing an
   explicit provider-visible lifecycle event upstream.

The desired steady-state path is:

```text
Codex / Relay activity
        |
        v
Roundhouse lifecycle + policy ---- archive/warm hint ----> Narwhal + KVBM
        |                                                   |
        | route demand                                      | committed
        v                                                   | residency
Dynamo fleet selector <-------------------------------------+
        |
        v
worker reservation; Narwhal performs local admission
```

## 2. Findings from the existing plan and synergy documents

### 2.1 What is strong and should remain

`PLAN-agentic-control-plane.md` is rigorous where it is hardest to be
rigorous: tenant identity precedes session lookup, routing policy only
narrows, grants and settlement separate committed from measured spend, log
events are replayable, synthetic tool calls match Codex's real wire shape,
and validation never contaminates the conversation prefix. The M0-M6
implementation is the right base for later agentic cache policy because it
gives every lifecycle action a principal, session, generation, and audit log.

The synergy rulings also establish useful ownership boundaries:

- Relay owns the experiment harness and instrumented agent front end.
- Roundhouse owns the durable turn, policy, budget, and control surface.
- Dynamo owns fleet cache-aware endpoint selection.
- Standard correlation metadata, ATOF/ATIF, optimization summaries,
  `ToolSignals`, and SLO headers should be reused instead of invented again.

### 2.2 What has drifted or is missing

1. The plan banner still described M0-M6 as unbuilt even though they landed in
   `4413ba9`. Roadmap work must start at M7, not repeat the walking skeleton.
2. The scheduler-dedup ruling collapsed fleet routing and worker-local
   execution into one problem. It therefore never evaluated Narwhal's stronger
   cache-lifecycle seam.
3. The current Roundhouse fleet interface prices, reserves, reports prefill
   completion/output blocks, and releases. It has no durable concept of an
   inactive session cache bundle or an archive/warm lifecycle.
4. Narwhal's current public lifecycle is request-oriented. It can protect a
   G2 mirror before evicting an active request from G1 and restore that request,
   but that is not the same contract as archiving the reusable cache of a
   completed, quiet session.
5. The Rhino cross-tier design already calls out session/task identity, time
   since the previous turn, active tails, inactive bodies, rapid turns, and
   long think gaps. Roundhouse does not yet supply those features or consume
   their decisions.
6. The control-plane plan has no WebSocket activity ledger. It consequently
   cannot distinguish an active turn, a completed-but-connected session, a
   disconnected client, or a session waiting on a human.

### 2.3 Current external posture on llm-d

As of 2026-08-19, llm-d is more than the old synergy snapshot described. Its
official architecture now divides KV management into cache-aware routing,
event-driven indexing, and multi-tier offload, and its router supports
session affinity and Responses API traffic. Its current agentic work includes
an experimental Session Control Protocol integration for session affinity,
idle cleanup, and capacity-aware placement. See the primary sources:

- [llm-d KV management architecture](https://github.com/llm-d/llm-d/blob/main/docs/architecture/advanced/kv-management/README.md)
- [llm-d Router architecture](https://github.com/llm-d/llm-d-router/blob/main/docs/architecture.md)
- [experimental Session Control Protocol integration](https://github.com/llm-d/llm-d-router/issues/2003)
- [router v0.10 agentic goals](https://github.com/llm-d/llm-d-router/issues/2006)

That makes llm-d a serious comparison and possible interop target. It does
not make it the right owner of Rhino's exact block lifecycle, eviction fences,
branch safety, or G1/G2/G3 policy. The evaluation gate is in §6 K7.

## 3. Ownership boundaries

| Concern | Owner | Contract to neighbors |
|---|---|---|
| Human/agent presence evidence | Codex, Relay, or another client adapter | Emits normalized activity observations; never issues block operations |
| Durable session state and policy | Roundhouse | Turns observations into versioned, auditable lifecycle hints |
| Cross-worker request placement | `roundhouse-fleet` using embedded Dynamo today | Quotes/reserves workers using prefix locality, load, and future residency advice |
| Worker-local scheduling | Narwhal | Owns admission, preemption, pause/evict/resume, and feasibility |
| Physical cache residency | KVBM and tiering policy | Executes fenced G1/G2/G3 moves and publishes committed residency events |
| Kubernetes/Envoy routing | Optional llm-d adapter | May consume Roundhouse/Narwhal signals; is never a second residency authority |
| Experiment harness and trajectory | NeMo Agent Toolkit / Relay | Replays workloads and consumes standard telemetry |

Two prohibitions make the split operational rather than aspirational:

- Roundhouse may request an outcome such as “retain this session no hotter
  than G2” or “make this bundle usable before this deadline.” It may not name
  an active Narwhal evictee or directly release a G1 block.
- Narwhal/KVBM may reject, delay, or partially satisfy a hint under pressure.
  Roundhouse records the result and routes accordingly; it does not pretend a
  requested move has happened.

## 4. Session lifecycle model

### 4.1 States

```text
HotActive
   | turn terminal and no blocking work
   v
HotQuiet -- activity --> HotActive
   | quiet grace expires
   v
ArchiveEligible -- activity --> HotActive
   | accepted archive hint
   v
Archiving -- activity --> WarmPending
   | committed
   v
Archived(G2 | G3) -- activity --> WarmPending
   | warm committed                 | bounded wait expires
   v                                v
HotActive                       ServingCold
                                    | prefix materialized
                                    v
                                HotActive
```

`Disconnected` is an observation, not a residency state. A network flap can
disconnect an active human, while an open socket can remain quiet for hours.
Likewise, `WaitingOnApproval` and `WaitingOnUserInput` are active agent states
but strong evidence that a longer human pause may follow. Policy may give
them a different grace period; it must not classify them as terminal.

### 4.2 Activity observations

Normalize all sources into a small internal vocabulary:

```rust
enum SessionActivity {
    TurnStarted,
    TurnTerminal,
    WaitingOnHuman,
    ClientPresent,
    ClientDisconnected,
    ThreadArchived,
}
```

Sources, in descending confidence:

1. an explicit client lifecycle event carrying the authenticated session and
   monotonically increasing epoch;
2. a Codex app-server `thread/status/changed` bridge;
3. a Responses WebSocket `response.create` / terminal response boundary;
4. HTTP turn arrival and completion;
5. transport close or timeout.

Socket silence alone only moves `HotActive` to `HotQuiet`. It cannot skip the
grace and safety checks.

### 4.3 Archive eligibility

A session is archive-eligible only when all of these are true:

- the last turn has a terminal log fact;
- no tool result, steer, validator, lease settlement, or provider stream is in
  flight;
- the quiet deadline for the tenant/workload class has elapsed;
- the session epoch still equals the epoch that scheduled the timer;
- KVBM identifies a branch-safe, reusable cache bundle for the session;
- policy has a colder tier with positive expected value after transfer cost,
  write amplification, and predicted reuse are considered.

Thread archival is a stronger policy hint, but still not permission to delete
shared anchors or poison-lineage dependencies. Confirmed-dead content follows
the KVBM release/compaction rules rather than the quiet-session path.

The first production slice does not archive a mid-turn approval, user-input,
or tool wait. That requires a separately proven suspended-request checkpoint
whose continuation state and cache bundle can be restored exactly; transport
quietness is not such a checkpoint.

### 4.4 Wake and debounce

Any positive activity increments `session_epoch`, cancels older archive work,
and issues at most one coalesced warm hint for the newest epoch. A minimum-hot
hold prevents a rapid sequence of turns from oscillating G1↔G2. A separate
archive grace prevents a short think pause from causing writeback.

There are two wake modes:

- **Predictive wake:** a client or presence adapter says the human is back
  before the next turn. Start warming immediately. This requires a new Codex,
  Relay, desktop, or terminal integration.
- **Demand wake:** the next `response.create` or HTTP request is the first
  signal. Begin warming as soon as identity and cache key are parsed. Hold
  dispatch only for a bounded restore budget and only when predicted saved
  prefill time exceeds that wait; otherwise route to an already-hot copy or
  recompute.

Without a provider-visible Codex lifecycle signal, Roundhouse can implement
demand wake and conservative archive. It cannot honestly claim to detect the
instant a human walks back to an idle terminal.

## 5. Minimal cross-plane contract

The API should describe lifecycle intent, not expose scheduler internals.
Names below are design vocabulary; their exact Rust home is decided in K2.

```rust
struct SessionCacheKey {
    principal: PrincipalId,
    session: SessionId,
    model_layout: ModelLayoutId,
    reuse_scope: ReuseScope,
}

struct CacheBundleRef {
    key: SessionCacheKey,
    generation: u64,
    bundle: CacheBundleId,
}

enum CacheLifecycleHint {
    Archive {
        cache: CacheBundleRef,
        not_before: Timestamp,
        desired_max_tier: Tier,
        reason: ArchiveReason,
    },
    Warm {
        cache: CacheBundleRef,
        deadline: Timestamp,
        target_tier: Tier,
    },
    Cancel {
        cache: CacheBundleRef,
    },
}
```

Required semantics:

- hints are authenticated, advisory, idempotent, and safe to replay;
- `(SessionCacheKey, generation)` fences stale archive and warm races;
- acknowledgements distinguish accepted, rejected, superseded, and committed;
- committed events carry actual source/target tier and bytes moved;
- a warm acknowledgement never promises that route selection will choose the
  worker unless the fleet reservation says so;
- cache identity includes model layout, adapter/tenant reuse scope, and branch
  generation so incompatible or poisoned blocks cannot alias;
- raw block hashes remain inside the cache/fleet boundary unless an existing
  Dynamo protocol already requires them.

Roundhouse persists intent and outcome events such as
`ArchiveRequested/Committed/Cancelled` and `WarmRequested/Ready/Bypassed`.
KVBM remains the authoritative source for physical residency.

## 6. Delivery tracks

Each milestone begins with the named failing contract or integration tests.
No policy is enabled from a green unit test alone; every behavioral milestone
passes through observe-only mode first.

### Track CP — finish the current control plane

#### CP7 — credentials, providers, and edge metadata

Complete the existing M7 scope: sealed credential resolution, real provider
clients, payer stamping, and redirect-safe pass-through. In the same edge
change, adopt `switchyard-protocol` correlation metadata and the established
SLO headers from the synergy ruling.

Gate: all existing M7 tests plus tests that identity and SLO metadata survive
both HTTP and WebSocket request paths without entering the durable prompt.

#### CP8 — admin and lifecycle policy

Complete existing M8 CRUD, key lifecycle, and budget reconciliation. Add
project-scoped cache policy fields, initially disabled:

- quiet grace and minimum-hot hold;
- allowed archive tiers;
- maximum wake wait;
- write-amplification and minimum-benefit limits;
- per-workload opt-in and kill switch.

Gate: policy can only narrow deployment ceilings, secrets are returned once,
and cache controls are tenant-scoped in every list/read/update path.

#### CP9 — real Codex E2E on both transports

Retain the existing synthetic-tool-call E2E and add a feature-gated Responses
WebSocket run. Verify the pinned Codex behavior actually observed in source:
connection reuse, `previous_response_id`, session/thread/turn client metadata,
terminal response boundaries, reconnect, and HTTP fallback.

Gate: the same session neither forks nor changes principal across HTTP,
WebSocket reuse, reconnect, and a forced synthetic steer.

### Track SY — realize the synergy rulings

#### SY0 — zero-code interoperability demo

Run Codex through agentic-api with Roundhouse's MCP server configured as a
remote tool. Record the supported topology and its chain guards. This remains
the fastest proof that the components compose.

#### SY1 — common telemetry and catalog provenance

Adopt the reviewed `nemo-relay-types` surface at an exact git revision; emit
`LlmOptimizationSummary` and ATOF; add ATIF when the trajectory endpoint
lands. Port `ToolSignals` with attribution, add ACG stability as an optional
validation signal, and consume Relay's output-length hint when present for
grant sizing. Extend the pricing catalog with aliases, tiered rates,
`pricing_as_of`, and `pricing_source`. Keep measured and estimated effects
separate. Every pre-1.0 dependency follows the newest synergy ruling: pin the
reviewed git revision, not a floating version or tag.

#### SY2 — topology conformance

Exercise Direct and Relay-chained topologies with correlation continuity,
single budget ownership, one retry owner, no recursive base URL, and no
decode/re-encode corruption of Roundhouse prefix admission.

#### SY3 — contribute proven seams upstream

Only after CP7 proves the wire and accounting seams, prepare small upstream
changes already justified by the synergy evidence:

- Relay: enforce streamed usage reporting without overriding callers, gate
  optimization baselines by capability, and accept measured cache evidence;
- Switchyard: replay-stable review re-arming and structured judge verdicts;
- agentic-api: rustls first, then observability, sequence-based stream resume,
  and the promised store trait.

Each contribution is independently useful and remains optional to Roundhouse.
No delivery milestone depends on upstream acceptance.

#### SY4 — narrow watching briefs

Re-read agentic-api when its Interactions surface ships and GAIE when a real
Kubernetes deployment is requested. llm-d is no longer in this undifferentiated
brief; K7 gives it a specific contract and decision gate.

### Track KV — session-aware cache lifecycle

#### K0 — baseline and trace corpus

Before a control API, collect a replayable corpus of real agentic timing:
inter-turn gaps, tool waits, approval waits, reconnects, prompt growth,
prefix reuse, and current G1/G2 residency. Use Relay/ATIF where available and
synthetic traces for rare races.

Tests first: a trace classifier distinguishes compute, tool-active,
waiting-on-human, completed quiet, disconnect, and reconnect without using
wall-clock gaps alone.

Exit gate: publish baseline TTFT, reusable unique bytes, hit rate, G1 pressure,
and the distribution of quiet intervals. Do not choose default timeouts before
this gate.

#### K1 — activity ledger, observe only

Add the WebSocket/HTTP activity ledger and lifecycle state machine to
Roundhouse. Timers emit proposed actions to telemetry only. Add an optional
adapter for explicit Codex/Relay thread status, but do not require it.

Tests first:

- an open WebSocket after a terminal response enters `HotQuiet` and cannot
  skip the grace period;
- a long tool call and an incomplete stream remain active;
- `WaitingOnUserInput` remains non-terminal even when its observation grace
  elapses;
- reconnect increments the epoch and cancels an old timer;
- a terminal turn followed by rapid activity never emits an archive proposal;
- replay reconstructs the same lifecycle state and deadlines.

Exit gate: false-archive proposals are below the agreed threshold on trace
replay, and multi-node timer ownership is fenced.

#### K2 — session-to-cache identity and Narwhal protocol

Introduce the minimal `SessionCacheKey`, `CacheBundleRef`, hint, and committed
event contract. Prefer a small control module in `narwhal-protocols` backed by
KVBM rather than widening Narwhal's request scheduler API. Map a completed
Roundhouse session generation to a branch-safe KVBM bundle without leaking
tenant or adapter scope.

Tests first:

- two principals with the same client cache key never share a bundle;
- model-layout or adapter mismatch refuses reuse;
- an old generation cannot archive a newer turn;
- compaction/poison lineage is never warmed as a valid descendant;
- duplicate archive, warm, and cancel hints are idempotent;
- request-active eviction remains behaviorally unchanged.

Exit gate: observe-only hints resolve to the exact bundle KVBM would move,
with no physical move and no new public block-management API.

#### K3 — shadow tiering policy

Feed session identity, lifecycle class, inter-turn age, reuse value, transfer
cost, and pressure into the Rhino cross-tier policy. Compare its decisions to
the existing independent-tier baseline. Roundhouse records the proposed
action and reason; KVBM reports feasibility.

Tests first: rapid turns retain active tails, long think gaps prefer inactive
bodies, shared anchors outlive private tails, and suspected-dead branches do
not outrank known reusable bundles.

Exit gate: offline replay shows net expected benefit after transfer time and
write cost, with no branch-safety regression. Still no production move.

#### K4 — production G1 to G2 archive and G2 to G1 restore

Enable only the path Narwhal/KVBM can fence end to end: establish a usable G2
copy, pin it, cross the visibility fence, then release eligible G1 residency.
Restore verifies exact bundle identity before admission. Start with one worker,
one model layout, and opt-in tenants.

Tests first:

- G1 is never released before the G2 fence;
- wake racing archive converges on the newest epoch;
- a failed or partial copy leaves the hot copy usable;
- worker restart reconciles requested versus committed residency;
- pressure may reject a warm hint without failing the user turn;
- cancellation cannot strand G2 pins or capacity accounting.

Exit gate: production canary improves reusable G1 capacity without regressing
P95/P99 wake TTFT beyond the project SLO. A kill switch returns to advisory
mode without restart.

#### K5 — debounced wake and route coordination

Coalesce activity into one warm operation, add minimum-hot hold, and connect
warm readiness to fleet quoting/reservation. Demand wake is mandatory;
predictive wake is an optional client adapter.

Tests first:

- repeated keystroke/presence events create one warm operation;
- an already-hot worker wins without a redundant transfer;
- the dispatch wait never exceeds the configured bound;
- a late `WarmReady` from an old generation cannot redirect a new turn;
- a warm destination cannot disappear between quote and reservation without a
  recorded fallback;
- multi-agent branches sharing an anchor do not warm unrelated private tails.

Exit gate: trace replay and canary show lower wake TTFT than recompute, and
promotion churn/write amplification remain within limits.

#### K6 — G2 to G3 recursive archive

Add G3 only after the Rhino recursive cross-tier milestone has a production
transfer path and exact residency events. G3 is for longer predicted gaps and
pressure relief; it is not the default consequence of a quiet socket.

Tests first: every G2 release has a fenced G3 copy or an explicit recompute
decision; G3 corruption/miss degrades to recompute; hot shared anchors are not
dragged cold by one inactive session; and endurance/capacity limits bind.

Exit gate: G3 yields positive cost-adjusted retention over the measured gap
distribution and does not create unacceptable SSD write amplification.

#### K7 — fleet-scale placement and llm-d evaluation

Publish only the residency and feasibility information needed for
cross-worker selection. Extend the embedded Dynamo path first. In parallel,
prototype an llm-d adapter against the same black-box contract; do not fork
Roundhouse policy or KVBM truth to fit it.

The llm-d comparison must answer:

- Can it carry Roundhouse principal/session/generation identity end to end?
- Can its Session Control path express quiet archive, cancel, and deadline
  warm without treating session close as deletion?
- Can it consume exact Narwhal/KVBM committed residency and eviction events
  instead of building a contradictory index?
- Can it distinguish G1/G2/G3 usability, model layout, reuse scope, branch
  poison, and in-progress transfers?
- Can reserve/release and assumed load remain exact across retries,
  disconnects, and horizontally scaled routers?
- Does full-duplex Responses traffic preserve Roundhouse's prefix and budget
  invariants?
- Does the added Envoy/EPP path beat the embedded Dynamo selector on agentic
  TTFT, tail latency, hit rate, operational cost, and failure recovery?

Decision gate:

- **Adopt as optional adapter** if it consumes the same truth and adds useful
  Kubernetes, P/D, or multi-cluster placement without weakening invariants.
- **Contribute a narrow plugin/protocol** if a small missing hook blocks the
  adapter and has clear upstream value.
- **Remain on embedded Dynamo** if llm-d requires a second cache authority,
  lossy identity, or proxy semantics that break the Responses stream.
- **Never replace Narwhal** based solely on fleet-routing results. A worker
  scheduler replacement would require a separate execution-quality study.

#### K8 — calibration and default-on readiness

Fit workload-specific reuse and return-time models, define safe static
fallbacks, and graduate one policy at a time. Keep all decisions explainable
from persisted features and policy version.

Exit gate: default-on requires a multi-week canary with bounded regressions,
replay-deterministic decisions, no cross-tenant or branch-safety incident,
successful disaster reconciliation, and independently reviewed rollback.

## 7. Dependency order and parallel work

```text
CP7 ----> CP8 ----> CP9
  |                  |
  v                  v
 K0 --------------> K1 ----> K2 ----> K3 ----> K4 ----> K5 ----> K6
                     |                         |
                     +------------------------>K7 ----> K8

SY0 can start now.
SY1 starts with CP7 and supplies K0 telemetry.
SY2 closes with CP9 and is rerun for K7 adapters.
```

Important sequencing constraints:

- K0 and the Narwhal protocol design can proceed while CP7 is built.
- K1's pure state-machine tests can start in parallel with CP9, but its exit
  gate requires CP9's real WebSocket path.
- No archive timer performs a move before K2 identity/fencing and K3 shadow
  evidence are complete.
- G3 is not on the critical path for a useful first product; G1↔G2 plus
  demand wake proves the lifecycle.
- llm-d evaluation starts from a stable K2 contract, not from its internal
  plugin API. That keeps the comparison honest and the core portable.

## 8. Measures that decide policy

Report by workload class, model layout, tenant, and lifecycle reason:

- unique reusable bytes by tier, not allocation bytes alone;
- archive precision: fraction of archived bundles not reused inside the hot
  hold window;
- missed archives and false archives;
- G1 pressure relieved and G2/G3 capacity consumed;
- archive/warm bytes, duration, cancellation, failure, and write amplification;
- warm lead time and readiness before dispatch;
- prefix hit tokens and recomputed tokens after wake;
- TTFT P50/P95/P99 for hot, restored, bypassed, and recomputed turns;
- queueing and reservation delay;
- promotion/demotion oscillation per session;
- cost per useful restored token and energy/storage cost where available.

Never collapse estimated saved prefill, measured transfer cost, and measured
user latency into one “KV saved” number. The control-plane accounting rule
applies here too: provenance survives aggregation.

## 9. Failure and adversarial test matrix

Every production milestone must cover at least:

- open-but-quiet WebSocket;
- closed socket during an active turn;
- long tool call and long approval wait;
- explicit thread idle, active, archived, and not-loaded transitions;
- client reconnect to the same and a different Roundhouse node;
- worker loss during archive and during warm;
- duplicate, reordered, and delayed lifecycle messages;
- archive/wake/cancel races across session epochs;
- turn compaction and poisoned lineage;
- shared anchor plus private branch tails;
- tenant/model/adapter identity collision attempts;
- capacity pressure rejecting a requested move;
- G2 or G3 data loss and metadata/data disagreement;
- router retry, provider retry, and user retry without double accounting;
- policy/config change while a timer or transfer is outstanding.

Property tests should generate event reorderings and assert convergence on the
newest session generation. Contract suites should run against memory, Redis,
Narwhal/KVBM, and every fleet adapter with the same cases.

## 10. Upstream and interoperability work

### Codex

The pinned source already contains the building blocks but not their bridge:

- `ModelClient` is session-scoped and caches a Responses WebSocket across
  turns;
- WebSocket `response.create` includes session/thread/turn client metadata;
- app-server publishes `thread/status/changed` with `Idle`, `Active`, and
  `NotLoaded`, including active flags for approval or user input.

Propose a provider-visible, authenticated lifecycle extension only after K1
has evidence for the useful distinctions. Prefer a small event carrying
session/thread id, epoch, state, and timestamp. Do not require Codex to name a
storage tier or expose terminal keystrokes. If upstream declines, keep the
bridge in Relay/app-server integrations and retain traffic-derived fallback.

### llm-d

Track Session Control Protocol, agentic routing profiles, Responses API state
lookup, multi-tier offload, and exact cache-event ingestion. The first useful
contribution is likely a narrow external lifecycle/residency producer or
scorer, not migration of Roundhouse's control plane.

### Dynamo and Narwhal

Extend the existing embedded-selector and connector seams before introducing
another daemon. Keep new Rust APIs in focused `module/mod.rs` modules, keep
the primary object at the top, minimize public types, and provide reusable
contract tests before implementations, following repository policy.

## 11. Explicit non-goals

- Perfectly infer human presence from transport silence.
- Archive on every terminal response or socket close.
- Let Roundhouse select a Narwhal evictee or mutate block managers.
- Treat thread archival as permission to delete shared cache content.
- Block a turn indefinitely waiting for a restore.
- Make G3 a prerequisite for G1 pressure relief.
- Operate Dynamo and llm-d as competing authorities in one request path.
- Replace Narwhal without a separately scoped worker-scheduler evaluation.
- Promise savings before trace replay and canary measurements demonstrate
  them.

## 12. Immediate backlog

The next bounded sequence is:

1. complete CP7 edge identity/provider work;
2. run SY0 and record the real topology;
3. create K0's activity/residency trace schema and corpus;
4. write the K1 state-machine tests before the WebSocket lifecycle ledger;
5. write a short Narwhal/KVBM K2 protocol RFC with contract tests and review it
   jointly against the Rhino cross-tier program;
6. implement observe-only archive/warm proposals;
7. set timeout and benefit thresholds from traces, not intuition;
8. enable one opt-in G1↔G2 canary only after K2-K4 gates pass;
9. evaluate llm-d against the stable K2/K7 contract, not before.

This sequence produces useful evidence early, preserves the current product
invariants, and reaches the user's desired quiet-session archive/wake behavior
without making either Codex integration or G3 storage a blocking dependency.
