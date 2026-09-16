<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Scorecard — cache-aware-routing

Fitness of this use case for the current roundhouse codebase. Each dimension is scored 0–3:
- **3** Strong fit — the capability exists and is exercised.
- **2** Moderate fit — the capability exists but needs wiring or configuration.
- **1** Weak fit — the capability is partially built or applies only in a constrained way.
- **0** Not applicable — the dimension does not apply, or the capability is entirely absent.

Max score: 24. See `use-cases/README.md` for band interpretations.

---

## Scores

| # | Dimension | Current | Target | Notes |
|---|---|---|---|---|
| 1 | Cache utilization | **3** | 3 | Corpus is large, stable, repeated; `prompt_cache_key` wired; `inactivity_decay` configured |
| 2 | Routing coverage | **1** | 3 | Frontier-only today; local tier not executable (no real `LocalExecutor`) |
| 3 | Session durability | **2** | 2 | 20-turn growing conversation; prefix admission and session log exercised |
| 4 | Budget sensitivity | **1** | 3 | Placeholder pricing; `correlaries: []` so savings dashboard shows $0 |
| 5 | MCP control surface | **1** | 2 | Tools exist but driver doesn't call them; no `prefer` / `declare_intent` in `run.py` |
| 6 | Validate / steer loop | **0** | 1 | No trigger signals in a factual Q&A loop; `PingPong` / `ToolFailureStreak` won't fire |
| 7 | Multi-agent / multi-user | **2** | 3 | Tax B design is correct; only one user in current `control-plane.json` |
| 8 | Implementation readiness | **2** | 3 | Frontier path works end-to-end; local executor and correlary are the gaps |
| | **Total** | **12 / 24** | **20 / 24** | Partial fit today; Good fit after Phase 2–3 |

---

## Dimension rationale

### 1. Cache utilization — 3 / 3

`corpus.md` is approximately 4 KB (the Ledgerline Payments service reference), well above the
`min_prefix_tokens: 1024` threshold in `catalog.json`. The corpus is sent verbatim as
`instructions` on every turn of every session — the definition of a stable, repeated prefix.
`prompt_cache_key` is passed per-session by `run.py` to anchor the KV cache entry roundhouse
tracks via `CacheLedger` in `crates/roundhouse-core/src/routing/ledger.rs`. The cache model is
`inactivity_decay` with `half_life_ms: 300000` (5 min) and `max_ttl_ms: 3600000` (1 hr) —
appropriate for a demo with back-to-back turns. This dimension is already at maximum and is not
a target for improvement.

### 2. Routing coverage & provider stability — 1 / 3 (target 3)

**(a) Cost gradient:** Today the demo routes frontier-only. The local tier path (Dynamo + Qwen)
exists in two halves: the Dynamo serving recipe (`use-cases/cache-aware-routing/serve_model.sh`) is ready, but two
Rust items are missing: (a) a real `LocalExecutor` implementation that calls a worker endpoint
(only `EchoLocalExecutor` exists in the repo), and (b) a custom binary that constructs
`EmbeddedFleet`, registers the Qwen worker, and calls `Engine::with_fleet(...)`. Without these,
`AffinityPolicy` in `crates/roundhouse-core/src/routing/policy.rs` never sees a local candidate
and always routes to the frontier.

**(b) KV cache cold-start on provider switch:** When `AffinityPolicy` switches providers
(frontier → local or local → frontier), the new provider processes the FULL conversation prefix
cold on the first turn after the switch. For this use case (20 turns, ~4 KB corpus), that means
one cold-start penalty per provider switch, after which the new provider warms up. `AffinityPolicy`
minimizes switches by preferring the provider whose `CacheLedger` entry is still warm — the
`prompt_cache_key` and `inactivity_decay` half_life_ms are the signals. In practice, a
well-configured routing policy should cross the local/frontier boundary at most once or twice
per 20-turn session. The cold-start cost is tolerable; the savings on the remaining turns exceed
the one-turn penalty if the prefix is long enough (> `min_prefix_tokens: 1024`).

**(c) Deployment topology for local routing:** `EmbeddedFleet` runs `SelectionService` in-process
and subscribes to Dynamo's ZMQ KV-event streams (`kv_events_endpoints`). These streams cannot
be practically tunneled over SSH. The required topology: roundhouse and Dynamo co-located on the
same GPU cluster node; SSH port-forward only roundhouse's `:8080` HTTP port to the laptop
(`ssh -L 8080:localhost:8080 user@cluster`). Score 1 today; target 3 requires Phase 2 (Rust)
+ cluster setup (deployment).

### 3. Session durability — 2 / 3 (target unchanged)

The 20-turn growing conversation exercises the append-only session log (`SessionStore` trait in
`crates/roundhouse-core/src/store.rs`), prefix admission (`responses_api.rs` checks resent
history as a prefix claim), and `turn_id` dedup (content hash, idempotent retry safe). Crash
recovery (repair settle) and the fencing token are not stress-tested by this demo. Score 2 is
appropriate — the log is exercised but not adversarially. Keeping it at 2 is correct; this is
a cache demo, not a reliability demo.

### 4. Budget sensitivity — 1 / 3 (target 3)

`catalog.json` has `$comment: "PRICES BELOW ARE PLACEHOLDERS"` and `correlaries: []`. The
savings dashboard (`/v1/metrics`) folds per-session spend via `MetricsFold` in
`crates/roundhouse-core/src/metrics/fold.rs`, but `routing_savings_at_decision_usd` will be
zero because no correlary pair is configured. The savings claim requires: (a) real pricing from
openrouter.ai, and (b) a correlary pairing the local model with a hosted peer within
`capability_band: 0.10`. Both are config changes (GAPS P1/P2), not code changes. Target 3
requires Phase 3.

### 5. MCP control surface — 1 / 3 (target 2)

The 8 MCP tools (`crates/roundhouse-mcp/`) are fully implemented and mounted at `/mcp`. The
demo driver (`run.py`) does not call any of them. A turn-1 `declare_intent` recording the demo
goal and a per-session `prefer local` / `prefer frontier` call would exercise the overlay
narrowing path (`TurnPolicy::narrow` in `crates/roundhouse-core/src/control/policy.rs`) and
make the demo more didactic. `fetch_steer` and `explain_last_route` could be polled after each
session to show routing rationale. Target 2 (not 3) because the Q&A loop is not the use case
where MCP steering is most natural — it can be added to the driver without changing the routing.

### 6. Validate / steer loop — 0 / 3 (target 1)

The validate/steer loop triggers on signals defined in
`crates/roundhouse-core/src/validate/trigger.rs`: `NoProgressRepeat`, `PingPong`,
`ToolFailureStreak`, `CostAnomaly`, `ErrorSeverity`, `PureBashStreak`. A factual Q&A loop
answering closed-world questions from a fixed document will not produce these patterns. The gate
also requires 20k tokens since last validation and a 60s cooldown, so a 20-turn demo is unlikely
to trigger even if it wanted to. Score 0 is correct and target 1 is aspirational — a longer run
with deliberately malformed questions could trigger `CostAnomaly`, but that is not the primary
purpose of this use case. Not a priority dimension for improvement.

### 7. Multi-agent / multi-user — 2 / 3 (target 3)

The Tax B design (cross-session cache hit) is correct in `run.py`: it loops over all
`(project, user)` memberships from `control-plane.json` and observes whether the second user's
session opens with a higher initial cache%. Currently there is only one `dev` user in
`control-plane.json`, so Tax B cannot be observed. Adding a second user and key entry (a
one-minute config change) completes the design. Target 3 requires Phase 1.

### 8. Implementation & deployment readiness — 2 / 3 (target 3)

**(a) Code readiness:** The frontier path is complete and was demonstrated in the prototype run
(`use-cases/cache-aware-routing/run.py` → NVIDIA gpt-5.5, see commit `2f08271`). Two Rust items are missing:
the real `LocalExecutor` (only `EchoLocalExecutor` and test mocks exist) and the custom binary
that wires `EmbeddedFleet`. These are Phase 2 items.

**(b) Deployment readiness:** Shape A (frontier-only) is immediately runnable on a laptop —
no cluster or GPU needed. Shape B (mixed local + frontier) requires: a GPU cluster node with
Dynamo installed from the pinned rev (`ac7b7513`), etcd + nats for worker discovery, Qwen weights
downloaded (~64 GB, TP≥2), and roundhouse running on the same node as Dynamo. The SSH tunnel
for the client (`:8080` only) is straightforward. The "Needs deployment" gap in GAPS.md captures
this explicitly. Target 3 after Phase 2 (Rust + cluster setup).

---

## Score history

| Date | Current score | Author | Change |
|---|---|---|---|
| 2026-09-01 | 12 / 24 | zcharpy | Initial scoring; formalized from prototype run (commit `2f08271`) |
