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

### Tax A and Tax B, in plain terms

Every time you send a message to a chatbot, the *whole conversation so far* — not just your new
message — gets sent to the model again, because the model itself is stateless and remembers
nothing between calls. If your system prompt is a 5,000-word reference document, that document
gets shipped and re-read on turn 1, turn 2, turn 3, every turn, forever. That's the
"re-discovery tax": paying, in time and money, to have the model re-read something it already
read a moment ago.

KV caching is the fix: the model provider (or your own local server) remembers the *processed*
form of text it has already seen, keyed by its exact wording. Resend the same text, and instead
of re-reading it from scratch, it's recalled almost for free. This use case measures how well
that recall actually works, split into two separate questions:

- **Tax A — "does it get cheaper as *my own* conversation goes on?"** Turn 1 of a brand-new
  conversation is always the expensive one — nothing has been cached yet. But by turn 2, 3, 20,
  the document and all the earlier back-and-forth should increasingly be served from cache
  instead of reprocessed, because you're the same person continuing the same conversation. This
  is the tax you'd expect *any* reasonable system to make go away on its own, and it's the easier
  of the two to get right.
- **Tax B — "does it get cheaper because *someone else* already asked first?"** This is the
  sharper question. If a hundred different engineers all paste the same onboarding doc as their
  system prompt, should the 100th person's first message really cost as much to process as the
  1st person's did? Tax B asks whether the cache is shared *across* people and sessions, not just
  remembered within one person's own conversation. A system that only solves Tax A but not Tax B
  is still re-paying the same "first-time" cost for every new person who shows up with the same
  document.

In the results below, look for: cache% starting low and climbing turn over turn within one
session (Tax A working), and a *second* session's very first turn already starting high instead
of at 0% (Tax B working) — because someone before it already "warmed up" the same document.

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

`catalog.json` now targets NVIDIA's `inference-api.nvidia.com` **Anthropic Messages** dialect
(`POST /v1/messages`, model `aws/anthropic/bedrock-claude-opus-4-8`, `wire_protocol:
"anthropic_messages"`) — updated 2026-09-23, see the file's own `$comment` for why. This also
required adding a top-level `"providers"` block naming NVIDIA's `base_url` and auth style, a
schema the M10.1 provider registry now requires for any `provider` other than the implicit
`"openai"`; the previous version of this file (`provider: "nvidia"` with no `"providers"` entry)
would be refused at boot on the current `main`. Then mint keys:

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

**Dynamo itself is runnable and validated; roundhouse routing to it is not.** Two separate
claims, proven separately:

- ✅ **Dynamo reachability** — validated hands-on 2026-09-23 on a single NVIDIA H100 80GB.
  `Qwen/Qwen2.5-Coder-32B-Instruct` fit and served with **no 14B fallback needed**: 61.04 GiB
  weights + 9.97 GiB KV cache (40,832 tokens, 1.25× concurrency at `max_model_len=32768`),
  steady-state GPU memory ~76.9 / 81.6 GB used. A real completion came back from Dynamo's own
  OpenAI-compatible endpoint:
  ```bash
  $ curl -s localhost:8000/v1/chat/completions -d '{"model":"Qwen/Qwen2.5-Coder-32B-Instruct",
      "messages":[{"role":"user","content":"Say OK"}],"max_tokens":10}'
  {"choices":[{"message":{"content":"OK","role":"assistant"},"finish_reason":"stop"}], ...}
  ```
  Full reproducible steps (sudo package list, pinned-rev clone, `uv`/`maturin` build, the
  CUDA-13/FlashInfer workaround, the corrected `dev/docker-compose.yml` path, weight pull, serve
  command) are in `DYNAMO_LOCAL_SERVING.md`.
- ❌ **roundhouse routing to it** — still not runnable. That `curl` above talks to Dynamo
  directly; roundhouse never sees it. See `GAPS.md` for the two missing pieces (a real
  `LocalExecutor`, and a custom binary or config flag wiring `EmbeddedFleet` into
  `roundhouse-server`).

Serving recipe: `use-cases/cache-aware-routing/serve_model.sh` (now defaults to `GPUS=0 TP=1`
for a single-GPU box; override for a real multi-GPU cluster node).

**`run_local.py` — the local-only counterpart to `run.py`.** Since roundhouse cannot route to
Dynamo yet, this script replays the identical corpus + `turns.jsonl` fixture directly against
Dynamo's own `/v1/chat/completions` (`DYNAMO_URL`, default `http://localhost:8000`) — no
roundhouse in the loop. It reads vLLM's own real `usage.prompt_tokens_details.cached_tokens`
(the same shape OpenAI's API uses), so the printed cache% is vLLM's genuine prefix-cache
accounting, not an estimate. A real run against the live 32B server (2026-09-23) showed exactly
the two effects this use case is named for:
```
=== session: local-session-1 ===
turn    in_tok    cached   cache%
   1      1490         0     0.0%   <- cold: corpus prefix not cached yet
   2      1539      1472    95.6%   <- Tax A: cache climbing turn over turn
  ...
  20      2833      2752    97.1%
  session totals: 93.6% of input served from cache

=== session: local-session-2 ===
turn    in_tok    cached   cache%
   1      1490      1472    98.8%   <- Tax B: second session opens warm
```
```bash
python3 use-cases/cache-aware-routing/run_local.py
```

## Notes

- `keys.local.json` holds real secrets and is gitignored; only `sha256(secret)` lands in
  `control-plane.json`.
- Rate cards in `catalog.json` are **placeholders**. Replace before trusting any dollar figures.
- `correlaries` is empty — Qwen-Coder-32B and the frontier model are too far apart in quality
  for the default `capability_band: 0.10`. The savings dashboard shows $0 until this is resolved.
- See `SCORECARD.md` for the fitness score, `GAPS.md` for gaps, `PLAN.md` for phases.
