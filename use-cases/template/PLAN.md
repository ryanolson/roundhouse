<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Implementation Plan — TODO: Use Case Name

---

## Objective

<!-- TODO: One sentence — what does success look like for this use case?
  Example: "Demonstrate that roundhouse eliminates the re-discovery tax for a shared corpus,
  with cache% ≥ 80% by turn 10, and the savings dashboard reporting real dollar figures." -->

---

## Deployment topology

<!-- TODO: State where roundhouse-server itself runs. This determines which prerequisites
  belong on which machine, and whether a tunnel is needed.

  Two canonical shapes:

  Shape A — Frontier-only (roundhouse on laptop):
    Codex → localhost:8080 → roundhouse → outbound HTTPS → frontier provider
    No GPU, no cluster, no tunnel needed.

  Shape B — Mixed local + frontier (roundhouse on cluster, co-located with Dynamo):
    Codex → SSH tunnel :8080 → roundhouse on cluster → Dynamo (in-process ZMQ)
                                                      → outbound HTTPS → frontier
    Required because EmbeddedFleet subscribes to Dynamo's ZMQ KV-event streams
    in-process; those streams cannot be practically tunneled. Only :8080 crosses
    the tunnel.

  State which shape this use case targets, and if Shape B, what cluster access is assumed.
-->

---

## Prerequisites

### Client (dev machine / laptop)

<!-- TODO: What must be set up on the machine where the agent (Codex) runs? -->

| Prerequisite | Notes |
|---|---|
| Codex CLI installed | `npm install -g @openai/codex` |
| `ROUNDHOUSE_API_KEY` set | From `mint_keys.py` output |
| `CODEX_HOME` pointed at roundhouse config | See `codex_launch.rs` for config.toml shape |
| TODO | TODO |

### Server (compute node where roundhouse runs)

<!-- TODO: What must be in place on the node where roundhouse-server will run?
  For Shape A (laptop), this is the same machine as the client.
  For Shape B (cluster), this is the GPU cluster node. -->

| Prerequisite | Notes |
|---|---|
| `INFERENCE_API_KEY` | From Vault / provider dashboard |
| Real model ID in `catalog.json` | From provider API docs |
| Real pricing in `catalog.json` | From openrouter.ai |
| TODO (local tier only) | etcd + nats running (Dynamo worker discovery) |
| TODO (local tier only) | Dynamo installed from pinned rev `ac7b7513` |
| TODO (local tier only) | Model weights downloaded (`pull_model.sh pull`) |

### Network

<!-- TODO: What network connections or tunnels are needed?
  For Shape A: nothing beyond outbound HTTPS to the provider.
  For Shape B: -->

| Connection | Command |
|---|---|
| TODO: SSH port-forward roundhouse to laptop | `ssh -L 8080:localhost:8080 user@cluster` |
| TODO: outbound HTTPS from cluster to frontier provider | Verify no egress firewall block |

---

## Phase 0 — Baseline (run as-is, frontier-only)

**Goal:** Verify the end-to-end path is healthy on the frontier tier with the exact deployment
topology above. Establishes baseline numbers before any gap work.

**Steps:**
1. Replace `TODO_MODEL_ID` in `catalog.json` with the real model id.
2. Set up the deployment topology (Shape A or B from above).
3. Run `mint_keys.py`, launch roundhouse, run `run.py`.
4. Observe per-turn `cached` column — record turn-by-turn cache%.

**Expected output:**
<!-- TODO: What numbers confirm Phase 0 is working?
  Include: first-turn cache%, late-turn cache%, session total cache%.
  These become the baseline against which later phases are measured. -->

**Gaps closed:** none (this is the baseline)

---

## Phase 1 — TODO

**Goal:** TODO

**Steps:**
<!-- TODO: Ordered list of changes. Distinguish client-side vs. server-side steps.
  Reference specific files. -->

**Expected output:**
<!-- TODO: Metrics or behaviors to observe that confirm this phase succeeded. -->

**KV cache note:**
<!-- TODO: Does this phase introduce or remove any provider switches?
  If yes: on the first turn after a switch, the new provider processes the full conversation
  prefix cold (one-time cost). Subsequent turns on the same provider warm up.
  AffinityPolicy minimizes switches by preferring the provider with the warm prefix cache
  (tracked via CacheLedger). State whether this cold-start cost is visible or acceptable. -->

**Gaps closed:**
<!-- TODO: List the rows from GAPS.md that this phase resolves, including any "Needs deployment"
  rows if this phase sets up the required topology. -->

---

## Phase 2 — TODO

**Goal:** TODO

**Steps:**
<!-- TODO -->

**Expected output:**
<!-- TODO -->

**KV cache note:**
<!-- TODO -->

**Gaps closed:**
<!-- TODO -->

---

## Phase 3 — TODO

**Goal:** TODO

**Steps:**
<!-- TODO -->

**Expected output:**
<!-- TODO -->

**KV cache note:**
<!-- TODO -->

**Gaps closed:**
<!-- TODO -->

---

## Score trajectory

| After phase | Expected score | Key unlock |
|---|---|---|
| Baseline (Phase 0) | TODO / 24 | Frontier path confirmed live |
| Phase 1 | TODO / 24 | TODO |
| Phase 2 | TODO / 24 | TODO |
| Phase 3 | TODO / 24 (target) | TODO |
