<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Scorecard — TODO: Use Case Name

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
| 1 | Cache utilization | TODO | TODO | TODO |
| 2 | Routing coverage & provider stability | TODO | TODO | TODO |
| 3 | Session durability | TODO | TODO | TODO |
| 4 | Budget sensitivity | TODO | TODO | TODO |
| 5 | MCP control surface | TODO | TODO | TODO |
| 6 | Validate / steer loop | TODO | TODO | TODO |
| 7 | Multi-agent / multi-user | TODO | TODO | TODO |
| 8 | Implementation & deployment readiness | TODO | TODO | TODO |
| | **Total** | **TODO / 24** | **TODO / 24** | |

---

## Dimension rationale

### 1. Cache utilization
<!-- TODO: Does the use case have a large, stable, repeated prefix?
  Is prompt_cache_key wired? What cache model is configured in catalog.json?
  Evidence: cite the corpus size, cache model config, and min_prefix_tokens.
  Key question: is the prefix long enough to exceed min_prefix_tokens (default 1024 tokens,
  roughly 4 KB of text) on the first turn? -->

### 2. Routing coverage & provider stability
<!-- TODO: This dimension has three sub-questions — answer all three.

  (a) Cost gradient:
  Does this use case benefit from local/frontier switching? Is there a meaningful cost
  difference between the local model and the frontier model for this workload?
  Evidence: compare quality_prior values and pricing in catalog.json; are correlaries configured?

  (b) KV cache cold-start on provider switch:
  When AffinityPolicy switches providers (frontier → local or local → frontier), the new
  provider processes the FULL conversation prefix cold on the first turn after the switch.
  It then warms up on subsequent turns. This cold-start cost is proportional to prefix length
  and grows with session length.
  Question: how tolerable is this cost for this use case? A 20-turn session with a 4 KB corpus
  pays it at most once or twice per session (AffinityPolicy stays on the warm provider). A
  very long agentic session with many forced escalations pays it more often.
  AffinityPolicy (crates/roundhouse-core/src/routing/policy.rs) minimizes switches by
  preferring the provider that already has the prefix warm, as tracked by CacheLedger
  (crates/roundhouse-core/src/routing/ledger.rs). The stability signal is prompt_cache_key.
  Evidence: is prompt_cache_key passed consistently? Does the corpus exceed min_prefix_tokens?

  (c) Deployment topology for local routing:
  The local tier requires roundhouse-server to run CO-LOCATED with the Dynamo worker on the
  same compute node. EmbeddedFleet runs SelectionService in-process and subscribes to the
  worker's ZMQ KV-event streams. These streams cannot be practically tunneled over SSH.
  The correct topology: Dynamo + roundhouse on the cluster node; SSH port-forward only
  roundhouse's single :8080 port back to the client.
  Question: is co-location on a GPU cluster feasible for this use case? Or is this use case
  frontier-only by design (no local tier needed)?
  Evidence: does the use case have a local_quality entry in catalog.json? Is serve_model.sh
  present? Is a GPU cluster available and accessible? -->

### 3. Session durability
<!-- TODO: How many turns does the session span?
  Does the durable log add value (prefix admission, crash recovery, dedup)?
  Evidence: turns.jsonl length, whether turn_id dedup is exercised.
  Key question: is this a short demo (20 turns) or a long agentic session (100+ turns)?
  Longer sessions benefit more from crash recovery and the fencing-token lease. -->

### 4. Budget sensitivity
<!-- TODO: Is there meaningful savings potential the dashboard can report?
  Are correlaries configured? Are real prices in catalog.json?
  Evidence: correlaries[] array contents, pricing fields.
  Key question: do the local and frontier models in correlaries[] fall within capability_band
  (default 0.10 quality_prior delta)? If they are too far apart in capability, the gate
  refuses to price them against each other (crates/roundhouse-core/src/metrics/pricing.rs). -->

### 5. MCP control surface
<!-- TODO: Will the agent or driver naturally call prefer, set_quality_floor, declare_intent?
  Is any MCP tool called in run.py?
  Evidence: whether run.py sends MCP requests, what the driver does with steer payloads.
  Key question: for agentic use cases (Codex running real tasks), the agent itself may call
  MCP tools. For scripted demos, the driver must call them explicitly. Which is this? -->

### 6. Validate / steer loop
<!-- TODO: Would quality degradation patterns appear (PingPong, ToolFailureStreak, CostAnomaly)?
  Is the use case a factual Q&A (unlikely to trigger), or an agentic loop (likely)?
  Evidence: trigger config in crates/roundhouse-core/src/validate/trigger.rs and what the
  turn sequence exercises. Note: the gate requires 20k tokens since last validation and a
  60s cooldown, so short demos rarely trigger it regardless of pattern. -->

### 7. Multi-agent / multi-user
<!-- TODO: Does the use case exercise cross-session cache sharing or concurrent sessions?
  How many users/projects are in control-plane.json?
  Evidence: keys[] array length, whether Tax B (cross-session cache hit) is measured.
  Key question: do multiple sessions share the same system prompt (corpus.md)?
  If yes, the second session's first turn should see a cache hit on the shared prefix —
  that is the Tax B effect. This requires at least two user entries in control-plane.json. -->

### 8. Implementation & deployment readiness
<!-- TODO: This dimension covers both code readiness and deployment readiness.

  (a) Code readiness:
  What fraction of what this use case needs is already built in the repo?
  Which paths are unimplemented (local executor, real pricing, correlary)?
  Evidence: GAPS.md P0/P1 entries.

  (b) Deployment readiness:
  Is the required compute topology available and configured?
  - Frontier-only: needs INFERENCE_API_KEY and a running roundhouse-server (can run on laptop).
  - Local tier: needs a GPU cluster node with Dynamo + roundhouse co-located, SSH tunnel for
    the client port (8080), etcd + nats running for Dynamo's worker discovery.
  Evidence: GAPS.md "Needs deployment" entries. Does serve_model.sh exist? Is a GPU cluster
  accessible? Are etcd + nats available in the cluster environment? -->

---

## Score history

| Date | Current score | Author | Change |
|---|---|---|---|
| TODO | TODO / 24 | TODO | Initial scoring |
