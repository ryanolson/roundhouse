<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# TODO: Use Case Name

<!-- TODO: One sentence — what cost or behavior does this use case demonstrate? -->

## What it shows

<!-- TODO: Describe the two-axis structure:
  - What happens within a single session (e.g., cache hits grow, cost per turn falls)?
  - What happens across sessions/users (e.g., shared prefix warms the cache for the next user)?
  If only one axis applies, say so. -->

## Deployment topology

<!-- TODO: State where each component runs and what network connections are needed.

  If this use case is frontier-only (Shape A):
    roundhouse can run on your dev machine; no GPU or cluster needed.

  If this use case involves a local tier (Shape B):
    roundhouse MUST run co-located with Dynamo on the GPU cluster node.
    EmbeddedFleet runs SelectionService in-process and subscribes to Dynamo's ZMQ
    KV-event streams. These streams cannot be practically tunneled over SSH.
    Only roundhouse's :8080 HTTP port crosses the tunnel.

    Topology (Shape B):
      [Laptop]                         [GPU cluster node]
        Codex ──SSH tunnel :8080──▶   roundhouse-server
                                            │  in-process EmbeddedFleet
                                            │  ZMQ subscribe
                                       Dynamo worker (model)
                                            │
                                       NVIDIA / OpenAI endpoint  (outbound HTTPS)

  State which shape applies and note any cluster-access assumptions.
-->

## Files

| File | What it is |
|---|---|
| `corpus.md` | The shared, stable prefix — sent as the system prompt on every turn. |
| `turns.jsonl` | <!-- TODO: describe how many turns and what they cover --> |
| `catalog.json` | Rate card + quality priors. **Edit the model id and real prices** before trusting the dashboard. |
| `control-plane.json` | Projects/users/keys, credentials, policy. |
| `mint_keys.py` | Mints `rh_turn_`/`rh_admin_` secrets, patches the hashes, writes `keys.local.json`. |
| `run.py` | The driver: replays turns per membership, prints per-turn metrics + `/v1/metrics`. |
| `pull_model.sh` *(local tier only)* | Downloads model weights to the compute node. |
| `serve_model.sh` *(local tier only)* | Launches Dynamo + vLLM worker with KV-event publishing. |

## Expected output (baseline)

<!-- TODO: What numbers should you see when the demo runs correctly?
  Example: "By turn 10, cached% should be ≥ 80%. Session total cache rate ≥ 70%."
  This makes the run pass/fail rather than "looks right."

  Also note: if this use case mixes local and frontier routing, the first turn after any
  provider switch will show lower cached% (cold-start). Subsequent turns on the same provider
  recover. State the expected per-turn pattern explicitly. -->

## Run it — frontier-only (Shape A)

<!-- TODO: These steps apply when roundhouse runs on the dev machine. -->

1. **Edit `catalog.json`** — replace `TODO_MODEL_ID` with the exact model id the endpoint accepts,
   and fill in real pricing from openrouter.ai.

2. **Mint keys:**
   ```bash
   python use-cases/TODO_NAME/mint_keys.py
   ```

3. **Launch roundhouse:**
   ```bash
   python vault/launch_roundhouse.py
   ```

4. **Drive the demo:**
   ```bash
   python use-cases/TODO_NAME/run.py
   ```

## Run it — with local tier (Shape B)

<!-- TODO: These steps apply when Dynamo serves a local model on a GPU cluster.
  Fill in actual cluster hostname, paths, and model. -->

**On the cluster node:**
```bash
# 1. Download weights (first time only)
DYNAMO_CLONE=/path/to/dynamo ./use-cases/TODO_NAME/pull_model.sh pull

# 2. Start etcd + nats (Dynamo worker discovery)
docker compose -f /path/to/dynamo/deploy/docker-compose.yml up -d

# 3. Serve the model
./use-cases/TODO_NAME/serve_model.sh serve

# 4. Launch roundhouse (pointing at this use case's configs)
export INFERENCE_API_KEY=...
export ROUNDHOUSE_API_KEY=$(cat keys.local.json | python3 -c "import json,sys; print(list(json.load(sys.stdin).values())[0])")
python vault/launch_roundhouse.py \
  --catalog use-cases/TODO_NAME/catalog.json \
  --control-plane use-cases/TODO_NAME/control-plane.json
```

**On the laptop:**
```bash
# SSH tunnel — forward roundhouse's port to localhost
ssh -L 8080:localhost:8080 user@your-cluster-node

# In another shell, run the driver
python use-cases/TODO_NAME/run.py
```

## Notes

- `keys.local.json` holds real secrets and is gitignored; only `sha256(secret)` lands in
  `control-plane.json`.
- Rate cards in `catalog.json` are **placeholders** until you replace them.
- On the first turn after a provider switch (frontier → local or local → frontier),
  the new provider processes the full prefix cold — one-time cost, then warms up.
  AffinityPolicy minimizes switches by staying on the provider with the warm cache.
- See `SCORECARD.md` for the fitness score, `GAPS.md` for what's not built/deployed yet,
  and `PLAN.md` for the phased implementation plan.
