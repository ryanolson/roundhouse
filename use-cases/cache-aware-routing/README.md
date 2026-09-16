<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# cache-aware-routing — Re-discovery tax measurement

Measures how many input tokens roundhouse serves from KV cache instead of reprocessing them
on every turn. The corpus (a fictional payments service reference doc) is sent verbatim as the
system prompt on every turn of every session — a pattern every stateless OpenAI client uses.
Roundhouse admits only the new suffix onto its append-only session log and reports, per turn,
`cached_tokens` vs. freshly processed `input_tokens`.

Two effects are visible:

- **Tax A (within a session):** as the conversation grows, the resent prefix grows; `cached`
  should climb toward `in_tok` on later turns.
- **Tax B (across sessions/users):** multiple users send the *identical* corpus as the system
  prompt; the KV cache hit should be visible from the second user onward.

## Deployment topology

This use case supports two shapes. Phases 0–1 use Shape A. Phases 2–3 require Shape B.

**Shape A — Frontier-only (runs on your laptop today):**
```
[Laptop]
  Codex ──▶ roundhouse :8080 ──HTTPS──▶ NVIDIA inference-api.nvidia.com
```

**Shape B — Mixed local + frontier (requires GPU cluster):**
```
[Laptop]                           [GPU cluster node]
  Codex ──SSH tunnel :8080──▶  roundhouse-server
                                    │  in-process EmbeddedFleet
                                    │  ZMQ subscribe
                               Dynamo worker (Qwen2.5-Coder-32B)
                                    │
                               NVIDIA inference-api  (outbound HTTPS)
```

Shape B requires roundhouse and Dynamo to be co-located on the cluster node.
`EmbeddedFleet` subscribes to Dynamo's ZMQ KV-event streams in-process — tunneling ZMQ over
SSH is not viable. Only roundhouse's `:8080` HTTP port needs to cross the tunnel.

```bash
# SSH tunnel — run this on your laptop for Shape B
ssh -L 8080:localhost:8080 user@your-gpu-cluster-node
```

## Files

| File | What it is |
|---|---|
| `corpus.md` | The shared, stable prefix — Ledgerline Payments service reference (~4 KB). Sent as system prompt on every turn. |
| `turns.jsonl` | 20 closed-world questions answerable only from `corpus.md`. |
| `catalog.json` | Rate card + quality priors. **Replace model id and pricing** before trusting the dashboard. |
| `control-plane.json` | Two-level identity (project `kv-cache-demo`, user `dev`), credentials, policy. |
| `mint_keys.py` | Mints `rh_turn_`/`rh_admin_` secrets, patches hashes, writes `keys.local.json`. |
| `run.py` | Driver: replays 20 turns per membership, prints per-turn cache stats + `/v1/metrics`. |

## Expected output (baseline — frontier-only)

With a warm KV cache and a prefix longer than `min_prefix_tokens` (1024):
- **Turns 1–3:** cache% is low (prefix not yet cached or just warming).
- **Turns 5+:** cache% should climb above 50% as the corpus prefix stabilizes.
- **Turns 10+:** cache% should reach 70–85% depending on provider TTL and `half_life_ms`.
- **Tax B:** the second user's session should open with a higher initial cache% than the first,
  because the corpus prefix is already resident.

These are indicators, not hard assertions. The exact numbers depend on the provider's KV cache
implementation and the `inactivity_decay` parameters in `catalog.json`.

## Run it (frontier tier — works today, WSL)

This routes through roundhouse to the NVIDIA frontier endpoint. No GPU needed.
Run all commands from the repo root in WSL (`cd /mnt/c/Users/zcharpy/Documents/roundhouse`).

**Step 1 — one-time setup (Terminal 1):**

Verify `catalog.json` has the correct model id — `switchyard/openai/gpt-5.5` is already set
and was validated against the NVIDIA endpoint. Then mint keys:

```bash
python3 use-cases/cache-aware-routing/mint_keys.py
```

**Step 2 — launch roundhouse server (Terminal 2, leave running):**

```bash
export INFERENCE_API_KEY=nvapi-YOUR_KEY_HERE
python3 vault/launch_roundhouse.py \
    --catalog use-cases/cache-aware-routing/catalog.json \
    --control-plane use-cases/cache-aware-routing/control-plane.json
```

The first run compiles the server (~2–3 min via `cargo run --release`). Wait for:
`listening on 127.0.0.1:8080` before proceeding.

**Step 3 — run the demo (Terminal 3):**

```bash
python3 use-cases/cache-aware-routing/run.py
```

Watch the `cached` column climb within each session, then read the `/v1/metrics` snapshot.
Live dashboard: `http://127.0.0.1:8080/v1/metrics/dashboard`.

**Shutting down the server:**

`Ctrl+C` in Terminal 2 is the normal path. If the terminal is gone:

```bash
kill $(lsof -t -i :8080)
```

**Step 4 — add a second user** to unlock Tax B — see Phase 1 in PLAN.md.

## Local tier (Dynamo-served Qwen)

Not runnable yet. See GAPS.md for the two missing pieces (real `LocalExecutor`, custom binary
with `EmbeddedFleet`). The Dynamo serving recipe is in the original `use-cases/cache-aware-routing/serve_model.sh`.

## Notes

- `keys.local.json` holds real secrets and is gitignored; only `sha256(secret)` lands in
  `control-plane.json`.
- Rate cards in `catalog.json` are **placeholders**. Replace before trusting any dollar figures.
- `correlaries` is empty — Qwen-Coder-32B and the frontier model are too far apart in quality
  for the default `capability_band: 0.10`. The savings dashboard shows $0 until this is resolved.
- See `SCORECARD.md` for the fitness score, `GAPS.md` for gaps, `PLAN.md` for phases.
