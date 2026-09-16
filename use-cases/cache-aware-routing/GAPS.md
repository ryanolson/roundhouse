<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Gap Analysis — cache-aware-routing

---

## Deployment topology

This use case has two operational shapes. Phase 0–1 use Shape A. Phase 2–3 require Shape B.

| Component | Shape A (frontier-only) | Shape B (mixed local + frontier) |
|---|---|---|
| Agent (Codex / Claude Code) | Laptop | Laptop |
| roundhouse-server | Laptop (same machine) | GPU cluster node |
| Dynamo worker (Qwen) | Not needed | GPU cluster node (co-located with roundhouse) |
| Redis | Laptop | GPU cluster node or shared |
| Frontier API (NVIDIA) | Outbound HTTPS from laptop | Outbound HTTPS from cluster node |

**Why co-location is required for Shape B:** `EmbeddedFleet` runs `SelectionService` in-process
(`crates/roundhouse-fleet/src/local.rs`) and subscribes to Dynamo's ZMQ KV-event streams. Those
streams are ZMQ PUB/SUB — tunneling them over SSH is not viable. Only roundhouse's single HTTP
port (`:8080`) needs to cross the tunnel back to the laptop.

**SSH tunnel for Shape B:**
```bash
# On your laptop — forwards roundhouse's port to localhost:8080
ssh -L 8080:localhost:8080 user@your-gpu-cluster-node
```

```mermaid
flowchart LR
  subgraph Laptop ["Client (laptop)"]
    Agent["Codex / Claude Code"]
  end

  subgraph Cluster ["GPU cluster node"]
    RH["roundhouse-server\n:8080"]
    EF["EmbeddedFleet\n(in-process)"]
    DY["Dynamo worker\nQwen2.5-Coder-32B\nZMQ :20080 · HTTP :8000"]
    RD["Redis\n(session store)"]
  end

  subgraph ProviderCloud ["Provider cloud"]
    FT["NVIDIA inference-api\n(OpenAI Responses wire)"]
  end

  Agent -->|"SSH tunnel :8080\n(Shape B)\nor direct :8080\n(Shape A)"| RH
  RH -->|"in-process ZMQ sub"| EF
  EF -->|"worker HTTP\n(LocalExecutor call)"| DY
  RH -->|"outbound HTTPS"| FT
  RH -->|"Redis Streams"| RD

  style Agent fill:#2d6a4f,color:#fff
  style RH fill:#2d6a4f,color:#fff
  style FT fill:#2d6a4f,color:#fff
  style RD fill:#2d6a4f,color:#fff
  style EF fill:#e07c00,color:#fff
  style DY fill:#e07c00,color:#fff
```

---

## What works today (Shape A — frontier-only)

| Component | File / module |
|---|---|
| OpenAI Responses API endpoint | `crates/roundhouse-server/src/responses_api.rs` |
| Prefix admission (resent history → suffix delta) | `crates/roundhouse-server/src/responses_api.rs` |
| `prompt_cache_key` tracking → `CacheLedger` | `crates/roundhouse-core/src/routing/ledger.rs` |
| Control plane (project / user / key resolution) | `crates/roundhouse-server/src/control_config/` |
| Frontier client (OpenAI Responses wire) | `crates/roundhouse-fleet/src/openai_responses.rs` |
| `AffinityPolicy` routing (cache-hit-first, load-aware) | `crates/roundhouse-core/src/routing/policy.rs` |
| `DecisionRecord` written to session log | `crates/roundhouse-core/src/routing/mod.rs` |
| `MetricsFold` → `/v1/metrics` + HTML dashboard | `crates/roundhouse-server/src/metrics_api.rs` |
| `inactivity_decay` cache model | `crates/roundhouse-fleet/src/frontier.rs` |
| SSE streaming, per-turn `cached_tokens` reported | `crates/roundhouse-server/src/responses_api.rs` |

---

## Gaps

| Gap | Type | Severity | Where it lives | Unblocked by |
|---|---|---|---|---|
| Real `LocalExecutor` not implemented | Not built | P1 | `crates/roundhouse-fleet/src/local.rs` — trait exists; only `EchoLocalExecutor` and mocks implement it | Phase 2 Rust work |
| `roundhouse-server` binary does not wire `LocalFleet` | Not wired | P1 | `crates/roundhouse-server/src/main.rs` — `serve()` attaches no `EmbeddedFleet` | Phase 2 custom binary |
| Dynamo worker not running + roundhouse not co-located on cluster | Needs deployment | P1 | GPU cluster node — requires etcd + nats, Dynamo from pinned rev `ac7b7513`, Qwen weights, roundhouse running on same node | Phase 2 cluster setup |
| `correlaries: []` — savings dashboard shows $0 | Needs config | P1 | `use-cases/cache-aware-routing/catalog.json` | Phase 3 |
| Placeholder pricing in `catalog.json` | Needs config | P1 | `use-cases/cache-aware-routing/catalog.json` | Phase 3 |
| Only one user in `control-plane.json` (Tax B invisible) | Needs config | P2 | `use-cases/cache-aware-routing/control-plane.json` — add second user + key entry | Phase 1 |
| Driver (`run.py`) does not call MCP tools | Not wired | P2 | `use-cases/cache-aware-routing/run.py` | Phase 1 |
| `local_quality: {}` — no quality prior for local model | Needs config | P2 | `use-cases/cache-aware-routing/catalog.json` | Phase 3 |
| No baseline assertions in `run.py` | Not wired | P2 | `use-cases/cache-aware-routing/run.py` | Phase 1 |

Severity:
- **P0** — blocks the demo from running at all (none currently for Shape A)
- **P1** — demo runs on Shape A but Shape B / savings / local routing are missing
- **P2** — demo runs and core numbers are visible, but improvements remain

---

## Architecture diagram

Color key:
- **Green** — implemented and wired for this use case
- **Orange** — implemented but not wired / needs config / needs deployment
- **Red** — not built yet

```mermaid
flowchart TD
  subgraph Client ["Client (laptop)"]
    Agent["Codex / Claude Code"]
  end

  subgraph ClusterNode ["Compute node (Shape B) / Laptop (Shape A)"]
    RH["/v1/responses\ncrates/roundhouse-server/src/responses_api.rs"]
    CP["ControlDirectory\ncrates/roundhouse-server/src/control_config/"]
    RL["AffinityPolicy\ncrates/roundhouse-core/src/routing/policy.rs"]
    CL["CacheLedger\ncrates/roundhouse-core/src/routing/ledger.rs"]
    FC["OpenAiResponsesClient\ncrates/roundhouse-fleet/src/openai_responses.rs"]
    ML["/v1/metrics\ncrates/roundhouse-server/src/metrics_api.rs"]
    MCP["MCP surface (8 tools)\ncrates/roundhouse-mcp/\n⚠️ not called by run.py"]
    LocalEx["LocalExecutor\n❌ not built\n(only EchoLocalExecutor exists)"]
    Fleet["EmbeddedFleet\ncrates/roundhouse-fleet/src/local.rs\n⚠️ not wired in binary"]
    DY["Dynamo + Qwen2.5-Coder-32B\nuse-cases/cache-aware-routing/serve_model.sh\n⚠️ not deployed"]
    Corr["correlaries config\n⚠️ empty — savings dashboard shows $0"]
  end

  subgraph ProviderCloud ["Provider cloud"]
    NV["NVIDIA inference-api\n✅ live"]
  end

  Agent -->|"x-roundhouse-key\nprompt_cache_key\n(SSH tunnel in Shape B)"| RH
  RH --> CP
  RH --> CL
  RH --> RL
  RL -->|"frontier-only today"| FC
  FC --> NV
  RH --> ML
  Agent -. "optional (Phase 1)" .-> MCP

  RL -. "needs LocalExecutor (Phase 2)" .-> LocalEx
  LocalEx -. "needs EmbeddedFleet (Phase 2)" .-> Fleet
  Fleet -. "needs Dynamo co-located\n(Phase 2 deployment)" .-> DY
  ML -. "needs correlaries (Phase 3)" .-> Corr

  %% KV cache cold-start: on the edge RL→LocalEx (first turn after switch to local)
  %% and on FC→NV (first turn after switch back to frontier), CacheLedger models
  %% the decay and AffinityPolicy minimizes how often this edge is crossed.

  style Agent fill:#2d6a4f,color:#fff
  style RH fill:#2d6a4f,color:#fff
  style CP fill:#2d6a4f,color:#fff
  style RL fill:#2d6a4f,color:#fff
  style CL fill:#2d6a4f,color:#fff
  style FC fill:#2d6a4f,color:#fff
  style ML fill:#2d6a4f,color:#fff
  style NV fill:#2d6a4f,color:#fff
  style MCP fill:#e07c00,color:#fff
  style Fleet fill:#e07c00,color:#fff
  style DY fill:#e07c00,color:#fff
  style Corr fill:#e07c00,color:#fff
  style LocalEx fill:#9b2335,color:#fff
```
