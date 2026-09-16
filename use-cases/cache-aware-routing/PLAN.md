<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Implementation Plan — cache-aware-routing

---

## Objective

Demonstrate that roundhouse eliminates the KV-cache re-discovery tax for a shared corpus, with
`cached_tokens / input_tokens` exceeding 70% by turn 10, a visible Tax B cross-session cache hit,
and the savings dashboard reporting real dollar figures against a calibrated local/frontier pair.

---

## Deployment topology

This use case progresses through two deployment shapes:

**Shape A — Frontier-only (Phases 0–1, runs on your laptop):**
```
[Laptop]
  Codex ──▶ roundhouse :8080 ──HTTPS──▶ NVIDIA inference-api.nvidia.com
```
No GPU, no cluster, no tunnel. roundhouse runs locally; every turn routes to the NVIDIA frontier.

**Shape B — Mixed local + frontier (Phases 2–3, requires GPU cluster):**
```
[Laptop]                             [GPU cluster node]
  Codex ──SSH tunnel :8080──▶   roundhouse-server
                                      │  in-process EmbeddedFleet
                                      │  ZMQ subscribe
                                 Dynamo worker (Qwen2.5-Coder-32B)
                                      │
                                 NVIDIA inference-api  (outbound HTTPS)
```
roundhouse and Dynamo must be co-located on the cluster node. `EmbeddedFleet` subscribes to
Dynamo's ZMQ KV-event streams in-process — tunneling ZMQ over SSH is not viable. Only the
`:8080` HTTP port crosses the tunnel to the laptop.

**SSH tunnel command (Shape B):**
```bash
ssh -L 8080:localhost:8080 user@your-gpu-cluster-node
```

---

## Prerequisites

### Client (laptop)

| Prerequisite | Notes |
|---|---|
| Codex CLI installed | `npm install -g @openai/codex` |
| `ROUNDHOUSE_API_KEY` set | From `mint_keys.py` output — `keys.local.json` |
| `CODEX_HOME` roundhouse config | `config.toml` + `models.json`; see `codex_launch.rs` for the exact shape |

### Server (Shape A: laptop — Shape B: GPU cluster node)

| Prerequisite | Notes |
|---|---|
| `INFERENCE_API_KEY` | NVIDIA NIM API key — from Vault / `.env` |
| Real model ID in `catalog.json` | Exact string sent on the wire as `"model"` |
| Real pricing in `catalog.json` | From openrouter.ai for the chosen model |
| *(Shape B only)* etcd + nats | `docker compose -f /path/to/dynamo/deploy/docker-compose.yml up -d` |
| *(Shape B only)* Dynamo from pinned rev `ac7b7513` | `pip install -e /path/to/dynamo/python/dynamo` |
| *(Shape B only)* Qwen weights downloaded | `./use-cases/cache-aware-routing/pull_model.sh pull` |

### Network

| Connection | Shape | Command / action |
|---|---|---|
| HTTPS to `inference-api.nvidia.com` | A + B | Verify outbound not blocked by firewall |
| SSH port-forward `:8080` | B only | `ssh -L 8080:localhost:8080 user@cluster` |

---

## Phase 0 — Frontier-only baseline (Shape A, runnable today)

**Goal:** Verify the end-to-end Shape A path is healthy and establish baseline cache% numbers.

**Steps:**
1. Replace `"model"` in `catalog.json` with the real NVIDIA model id.
2. Run `python use-cases/cache-aware-routing/mint_keys.py`.
3. Launch roundhouse on your laptop:
   ```bash
   python vault/launch_roundhouse.py \
     --catalog use-cases/cache-aware-routing/catalog.json \
     --control-plane use-cases/cache-aware-routing/control-plane.json
   ```
4. Run the driver: `python use-cases/cache-aware-routing/run.py`.
5. Record per-turn `cache%` and the session total.

**Expected output:**
- Turn 1: cache% near 0% (corpus not yet in provider KV cache).
- Turns 5+: cache% climbing as the growing prefix warms the provider's cache.
- `/v1/metrics`: `seat_tokens` counted, `routing_savings_at_decision_usd: 0` (no correlary).

**KV cache note:** All turns route to the same frontier provider (Shape A, no switching), so
AffinityPolicy never crosses a provider boundary. Every turn benefits from the same provider's
warming cache. This is the cleanest demonstration of Tax A (within-session prefix growth).

**Gaps closed:** none (this is the baseline)

---

## Phase 1 — Tax B and MCP wiring (Shape A)

**Goal:** Make the cross-session effect visible and wire basic MCP tool calls into the driver.
Score moves from 12/24 → 15/24.

**Steps:**

1. **Add a second user to `control-plane.json`:**
   ```json
   "users": [{"id": "dev"}, {"id": "dev2"}],
   "keys": [
     {"project": "kv-cache-demo", "user": "dev",  "key_sha256": "..."},
     {"project": "kv-cache-demo", "user": "dev2", "key_sha256": "..."}
   ]
   ```
   Re-run `mint_keys.py` to patch the hashes.

2. **Add a `declare_intent` call at session start in `run.py`** — POST to `/mcp` with the
   `declare_intent` tool, recording the demo goal and `"done-when": "cache% ≥ 70% by turn 10"`.
   This exercises the `IntentRecord` path in `crates/roundhouse-mcp/src/store.rs`.

3. **Add an `explain_last_route` call after each session** — POST to `/mcp` and print the
   chosen model, rationale, and budget state. Exercises the read path on the last `DecisionRecord`.

4. **Add a soft baseline assertion in `run.py`:** Warn (not exit) if session total cache% < 50%.
   Configurable via `CACHE_ASSERT_PCT` env var.

**Expected output:**
- Two sessions: `kv-cache-demo/dev` then `kv-cache-demo/dev2`.
- `dev2`'s turn-1 cache% visibly higher than `dev`'s turn-1 (Tax B effect: corpus already cached).
- `explain_last_route` output printed after each session.

**KV cache note:** Both users route to the same frontier provider throughout (no switching). The
Tax B effect is purely about the shared corpus prefix being warm in the provider's KV cache by
the time the second user's first turn arrives.

**Gaps closed:**
- "Only one user in control-plane.json (Tax B invisible)" → resolved
- "Driver does not call MCP tools" → partially resolved
- "No baseline assertions in run.py" → resolved

---

## Phase 2 — Local tier: Dynamo + real LocalExecutor (Shape B)

**Goal:** Route cheap/warm turns to local Qwen and only the hard ones to the frontier. This is
the co-optimization story: function + cost + latency all measured together.
Score moves from 15/24 → 18/24.

**Cluster setup (one-time, "Needs deployment" gap):**
1. SSH into the GPU cluster node.
2. Start etcd + nats: `docker compose -f /path/to/dynamo/deploy/docker-compose.yml up -d`
3. Download weights: `DYNAMO_CLONE=/path/to/dynamo ./use-cases/cache-aware-routing/pull_model.sh pull`
4. Serve Qwen: `./use-cases/cache-aware-routing/serve_model.sh serve` (or the equivalent from your Dynamo clone)
   — this publishes KV events on ZMQ `:20080` with `BLOCK_SIZE=64` and `PYTHONHASHSEED=0`.

**Rust work (code gaps):**
1. **Write a real `LocalExecutor`** in `crates/roundhouse-fleet/src/` — receives a `LocalQuote`
   (worker endpoint + `prompt_tokens`) and drives the worker's OpenAI-compatible HTTP endpoint,
   streaming tokens and returning `Usage`. This is the single largest missing piece.
2. **Write a custom binary** (e.g., `roundhouse-server/src/bin/roundhouse_with_dynamo.rs`) that
   constructs `SelectionService`, wraps it in `EmbeddedFleet`, calls `register_worker(
   WorkerRegistration { block_size: 64, ... })`, and attaches it via `Engine::with_fleet(...)`.

**Launch on the cluster node:**
```bash
export INFERENCE_API_KEY=...
python vault/launch_roundhouse.py \
  --catalog use-cases/cache-aware-routing/catalog.json \
  --control-plane use-cases/cache-aware-routing/control-plane.json \
  --binary roundhouse_with_dynamo   # the custom binary from step 2
```

**On the laptop:**
```bash
ssh -L 8080:localhost:8080 user@cluster-node
python use-cases/cache-aware-routing/run.py
```

**Expected output:**
- Some turns route to `Local { model: "Qwen/..." }`, others to `Frontier`.
- `/v1/metrics`: non-zero `seat_tokens` for the local model.
- `explain_last_route`: shows `"chosen": "local"` on cache-warm turns.

**KV cache note (critical):** When AffinityPolicy switches providers (frontier → local or
local → frontier), the new provider processes the FULL conversation prefix cold on the first
turn after the switch — one-time penalty proportional to prefix length. Subsequent turns on the
same provider benefit from its warmed cache. `AffinityPolicy` minimizes switches by preferring
the provider whose `CacheLedger` entry has not yet decayed. In practice, for a 20-turn session
with a 4 KB corpus, you may see one cold-start event per session at the first local/frontier
boundary. Expect a dip in `cache%` on that turn followed by recovery.

**Gaps closed:**
- "Real LocalExecutor not implemented" → resolved (Rust)
- "roundhouse-server binary does not wire LocalFleet" → resolved (Rust)
- "Dynamo worker not running + roundhouse not co-located" → resolved (deployment)

---

## Phase 3 — Real pricing and savings dashboard (Shape B)

**Goal:** Make the savings dashboard report defensible dollar figures. Score reaches 20/24.

**Steps:**

1. **Replace placeholder pricing** in `catalog.json` with real published rates from openrouter.ai.
   Record the source URL and snapshot date in a `$comment` field.

2. **Add a `correlary` pair** to `catalog.json`:
   ```json
   "correlaries": [
     {
       "local": "Qwen/Qwen2.5-Coder-32B-Instruct",
       "frontier": "TODO_FRONTIER_MODEL_ID"
     }
   ]
   ```
   The capability gate (`crates/roundhouse-core/src/metrics/pricing.rs`) requires both models'
   `quality_prior` values to be within `capability_band` (default 0.10). If Qwen at ~0.72 and
   the frontier at ~0.92 are too far apart: (a) find a closer hosted peer on openrouter.ai, or
   (b) widen `capability_band` deliberately and document why.

3. **Set `local_quality`** for the Qwen model in `catalog.json`:
   ```json
   "local_quality": {"Qwen/Qwen2.5-Coder-32B-Instruct": 0.72}
   ```
   Source from a published intelligence index (OpenRouter `/api/v1/benchmarks`) normalized to
   0–1. Until the M10.1 import tool ships, use a hand-written value with a dated comment.

4. **Verify the dashboard** at `http://localhost:8080/v1/metrics/dashboard` shows non-zero
   `routing_savings_at_decision_usd` after a session that routes at least one turn locally.

**Gaps closed:**
- "correlaries: [] — savings dashboard shows $0" → resolved
- "Placeholder pricing in catalog.json" → resolved
- "local_quality: {} — no quality prior for local model" → resolved

---

## Score trajectory

| After phase | Expected score | Key unlock |
|---|---|---|
| Baseline (Phase 0) | 12 / 24 | Frontier path confirmed live (Shape A) |
| Phase 1 | 15 / 24 | Tax B visible; MCP surface exercised; baseline assertions |
| Phase 2 | 18 / 24 | Local routing live; co-optimization observable (Shape B) |
| Phase 3 | 20 / 24 | Real pricing; savings dashboard reporting |
