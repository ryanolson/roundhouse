<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Integration Paths — cache-aware-routing

**Audience:** roundhouse repo admin / milestone planner.

**Purpose:** This use case exposes four repo-level gaps. Three of them are cross-cutting — they
would appear in any use case that needs a local tier. This document presents two or three
resolution paths per gap so the admin can pick based on current milestone priorities rather than
inheriting the use case author's single recommended path.

---

## Gap summary (admin view)

| Gap | Blocks | Cross-cutting? | Effort signal |
|---|---|---|---|
| No real `LocalExecutor` implementation | All local-tier use cases | **Yes** — any use case routing to a local model | Medium (new crate file + streaming HTTP client) |
| `roundhouse-server` binary wires no `LocalFleet` | All local-tier use cases | **Yes** | Low–Medium (config flag or library entry point) |
| `EmbeddedFleet` requires co-location with Dynamo | Any local-tier use case on remote compute | **Yes** — structural constraint | Depends on resolution path chosen |
| `correlaries` / `quality_prior` import tooling | Savings dashboard for any mixed local+frontier use case | **Yes** — M10.1 already planned | Low (import tool is in scope) |

---

## Integration alternatives

---

### Gap 1: No real `LocalExecutor`

**What the use case needs:** When the router selects a local candidate, something must actually
call the Dynamo worker endpoint, stream tokens, and return `Usage`. Today only `EchoLocalExecutor`
exists, which returns a canned string.

**Option A — HTTP `LocalExecutor` calling Dynamo's OpenAI-compatible endpoint (recommended):**
- What changes: New implementation of the `LocalExecutor` trait in `crates/roundhouse-fleet/`
  (e.g., `http_executor.rs`). Receives a `LocalQuote` (worker HTTP endpoint + `prompt_tokens`),
  POSTs to the worker's `/v1/chat/completions`, streams deltas, returns `LocalExecution`.
- Enables: All local-tier use cases. Uses the standard OpenAI wire Dynamo already exposes — no
  new protocol, no Dynamo changes.
- Costs: One new source file; must handle streaming correctly and map token counts to `Usage`.
- Forecloses: Nothing. This is the minimal correct implementation and does not prevent a tighter
  gRPC or in-process executor later.
- **Evidence the seam exists:** `LocalExecutor` trait and `EchoLocalExecutor` at
  `crates/roundhouse-server/src/engine.rs:132–140`; `LocalQuote` at
  `crates/roundhouse-fleet/src/local.rs`.

**Option B — Route Dynamo as a second "frontier" entry in the catalog:**
- What changes: Add Dynamo's HTTP endpoint as a second model entry in `catalog.json` with
  `wire_protocol: "openai_responses"`, `provider: "dynamo_local"`, and a new credential
  pointing at the Dynamo endpoint. No Rust changes required.
- Enables: Turns route to Dynamo over the existing `FrontierClient` path. Works today with
  zero code changes.
- Costs: Loses KV-event-aware routing — the `SelectionService` / `EmbeddedFleet` path and
  its block-level cache-hit signals are bypassed entirely. `AffinityPolicy` falls back to
  TTL-decay estimation, not real KV event data. The `explain_last_route` output shows Dynamo
  as a "frontier" which is semantically incorrect and may confuse the dashboard.
- Forecloses: Nothing permanently, but the dashboard's local/frontier split will be wrong.
  Teams that adopt this workaround may not notice the KV accuracy gap until they compare it
  against Option A.
- **Use when:** You need a runnable local-tier demo immediately and can accept approximate
  cache accounting while the real `LocalExecutor` is in progress.

**Option C — In-process generation via a Dynamo embedding (future, not current):**
- What changes: Embed model weights directly in-process (not via HTTP/gRPC to a separate
  worker). Requires Dynamo's generation kernel to be importable as a Rust crate.
- Enables: Lowest possible latency; no HTTP round-trip for generation.
- Costs: Large dependency surface; Dynamo's Rust generation API is not stable. This is a
  future option contingent on Dynamo exposing a stable embedding API.
- Forecloses: Nothing, but don't block on this waiting for it.

**Admin recommendation:** Option A. It is the minimal correct implementation, unblocks all
local-tier use cases in one PR, and does not foreclose the tighter options. Option B is an
acceptable temporary workaround if a local-tier demo is needed before the Rust work is
scheduled.

---

### Gap 2: `roundhouse-server` binary wires no `LocalFleet`

**What the use case needs:** A way to run roundhouse with a `LocalFleet` attached without
writing a custom `main.rs` per deployment.

**Option A — Operator config flag in `main.rs` (recommended):**
- What changes: `ROUNDHOUSE_LOCAL_FLEET=dynamo` (or similar) causes `serve()` to construct
  `EmbeddedFleet`, call `register_worker(...)` from a new `[local_fleet]` stanza in the
  config, and attach it via `Engine::with_fleet(...)`. One `if` branch and a new config
  section.
- Enables: Any operator to enable local routing without touching Rust. This is the operator
  ergonomics story.
- Costs: New config schema to maintain; worker registration must be declarative
  (endpoint, block_size, kv_events_port).
- Forecloses: Nothing. The library entry point (`Engine::with_fleet`) remains for custom binaries.

**Option B — Keep library-only; require custom binary per deployment (current):**
- What changes: Nothing. The `Engine::with_fleet` seam is already there; use case authors
  write their own `main.rs`.
- Enables: Full flexibility for custom topologies.
- Costs: Every new local-tier use case requires a Rust binary. Non-Rust operators cannot
  enable local routing.
- Forecloses: Nothing technically, but raises the bar for adoption.

**Option C — Admin API endpoint to register workers at runtime:**
- What changes: New POST `/v1/admin/workers` endpoint that accepts a `WorkerRegistration`
  JSON body and calls `register_worker(...)` on a live `EmbeddedFleet`. Workers can be added
  or removed without restarting the server.
- Enables: Dynamic fleet management; Kubernetes-native (workers register on startup).
- Costs: Higher complexity; worker lifecycle (deregistration on crash) must be handled.
  `ControlDirectory` is already in `roundhouse-server` — this would live there.
- Forecloses: Nothing; compatible with Option A.

**Admin recommendation:** Option A for the near term — it unblocks operators without code
changes on their side. Option C is the right long-term answer for Kubernetes deployments and
aligns with the admin API already present in `crates/roundhouse-server/src/admin_api.rs`.

---

### Gap 3: `EmbeddedFleet` requires co-location with Dynamo (ZMQ constraint)

**What the use case needs:** roundhouse to subscribe to Dynamo's KV-event streams to get
accurate block-level cache-hit data. Today those streams are ZMQ PUB/SUB — not tunnelable
over SSH.

**Option A — Accept co-location; SSH-tunnel only `:8080` (recommended for now):**
- What changes: Nothing in the repo. Operators run roundhouse on the same GPU cluster node
  as Dynamo. Only the client (Codex) tunnels `:8080` from the laptop.
- Enables: Full KV-event accuracy; clean architecture; already documented in GAPS.md.
- Costs: Operators must have a process on the cluster node, not just a local machine. This
  is the correct operating model for a production deployment anyway.
- Forecloses: Nothing.

**Option B — Redis pub/sub for KV events (removes co-location requirement):**
- What changes: Dynamo publishes KV events to Redis instead of (or in addition to) ZMQ.
  `EmbeddedFleet` subscribes to Redis Streams instead of ZMQ. roundhouse already depends on
  Redis (`crates/roundhouse-store-redis/`), so no new dependency is added.
- Enables: roundhouse can run on the laptop while Dynamo runs on a remote cluster. The Redis
  instance is the shared bus.
- Costs: Dynamo would need to support a Redis publisher (not currently in the pinned rev
  `ac7b7513`); adds latency relative to in-process ZMQ. A Dynamo-side change is required —
  check whether the upstream Dynamo project would accept this, or whether it needs a fork.
- Forecloses: Nothing in roundhouse; but depends on Dynamo support.
- **Worth tracking:** If multiple use cases request remote-cluster topology, this becomes a
  higher-priority option. Record it as a potential S4 contribution back to the Dynamo project
  (see `agent-docs/synergies/nemo-relay.md` for the "contribute back" discipline).

**Option C — HTTP-based KV event webhook (no ZMQ dependency):**
- What changes: Dynamo POSTs KV events to a roundhouse HTTP endpoint (`/v1/internal/kv-event`).
  roundhouse processes them synchronously or queues them. No ZMQ dependency; pure HTTP.
- Enables: Remote-cluster topology without Redis.
- Costs: Event delivery is no longer fire-and-forget (HTTP vs. ZMQ pub/sub); adds roundtrip
  latency per KV event. High-frequency KV events could overwhelm the HTTP endpoint under load.
- Forecloses: Nothing, but the latency trade-off may make this unsuitable for production.

**Admin recommendation:** Option A for all current use cases — co-location is the correct
production topology and requires zero changes. Option B is worth a research spike if the
remote-cluster use case becomes common: the change is upstream in Dynamo, not in roundhouse.
Open a dated addendum in `agent-docs/synergies/` if Option B is pursued.

---

### Gap 4: `correlaries` and `quality_prior` import tooling

**What the use case needs:** A way to get defensible, sourced quality scores for both the local
and frontier model, and a correlary pairing so the savings dashboard reports real figures.

**Option A — M10.1 offline import tool (already planned):**
- What changes: The import tool sourced from OpenRouter `/api/v1/benchmarks` with provenance
  stamping is already in `PLAN-frontier-selection.md` M10.1. No new work needed.
- Enables: Sourced, versioned `quality_prior` for any model on OpenRouter.
- Costs: Depends on M10.1 shipping; use cases that need savings figures before then must use
  Option B.
- Forecloses: Nothing.

**Option B — Hand-written values with dated comments (current):**
- What changes: Nothing. Fill in `quality_prior` and `correlaries` manually with a `$comment`
  recording the source index and date.
- Enables: Savings dashboard today, without waiting for M10.1.
- Costs: Manual maintenance; values go stale as upstream benchmarks are updated.
- Forecloses: Nothing; Option A replaces this when it ships.

**Admin recommendation:** Option B immediately for this use case; Option A supersedes it when
M10.1 ships. No new milestone is needed — M10.1 already covers this.

---

## Priority signal for the roadmap

| Suggestion | Target document | Priority |
|---|---|---|
| Add `LocalExecutor` (HTTP, Option A) to M11 or as a standalone PR — it unblocks all local-tier use cases | `agent-docs/PLAN-frontier-selection.md` as M10.5 or new M11 plan | **High** — cross-cutting |
| Add operator config flag for `LocalFleet` (Gap 2, Option A) alongside the `LocalExecutor` PR | Same plan file | **High** — required for adoption without custom binary |
| Record ZMQ co-location constraint in synergies — so future use case authors know before they start | `agent-docs/synergies/nemo-relay.md` as a dated addendum, or new `agent-docs/synergies/dynamo.md` | **Medium** — prevents repeated rediscovery |
| Track Redis KV-event publisher as a potential Dynamo contribution (Gap 3, Option B) | `agent-docs/synergies/` research spike if remote-cluster demand grows | **Low for now** |
| M10.1 import tool already covers correlary / quality_prior gap — validate by using it for this use case when it ships | `agent-docs/PLAN-frontier-selection.md` — no new entry needed | **Validation** |

---

## Constraints worth documenting (promote to shared location)

These constraints were confirmed by this use case and will apply to any future local-tier use
case. Once a second use case hits the same constraint, promote from here to `use-cases/README.md`
or a new `use-cases/CONSTRAINTS.md`.

1. **EmbeddedFleet requires co-location with Dynamo.** ZMQ KV-event streams are not tunnelable.
   Use cases that need a local tier must plan for roundhouse to run on the same node as the
   Dynamo worker. The only tunnel needed is `:8080` (HTTP) from the client to roundhouse.

2. **`capability_band` constrains correlary pairing.** Local and frontier models must have
   `quality_prior` values within 0.10 (default `capability_band`) to be priced against each
   other by the savings dashboard. Models far apart in capability (e.g., Qwen-32B vs.
   Claude-Opus) cannot be directly compared without either widening the band deliberately or
   finding a closer hosted peer.

3. **`LocalExecutor` is the single largest missing piece for all local-tier use cases.** Any
   use case that wants to route to a local model depends on this being built. It is not a
   per-use-case gap; it is a repo-level gap that blocks the entire local-tier category.
