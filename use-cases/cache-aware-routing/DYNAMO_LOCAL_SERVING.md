<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Dynamo local-tier serving — what we're trying to do, and how to reproduce it

**Goal.** This use case's Shape B (see `README.md`) is the co-optimization story: route
cheap/warm turns to a local Qwen model on Dynamo and only the hard ones to the NVIDIA frontier,
with roundhouse measuring cost, quality, and latency together. Getting there needs two things
proven independently before they can be wired together:

1. **Dynamo itself actually serving a real model on real hardware**, publishing KV-cache events
   on the ZMQ wire roundhouse-fleet's `EmbeddedFleet` is built to subscribe to. This document is
   that proof — a from-source install and a live model, reproducible end to end.
2. **roundhouse routing a turn to that worker.** Not yet true. `crates/roundhouse-fleet/src/
   local.rs`'s `LocalExecutor` trait has no real implementation (only `EchoLocalExecutor`, which
   returns a canned string), and `crates/roundhouse-server/src/main.rs`'s `serve()` wires no
   `LocalFleet` at all. See `GAPS.md` for the full gap list and `INTEGRATION.md` for the Rust-work
   options. **Nothing in this document closes that gap** — it closes the "is Dynamo even
   reachable" half of Shape B, not the "does roundhouse dispatch to it" half.

This file only covers step 1: installing Dynamo from source and getting `Qwen/Qwen2.5-Coder-32B-
Instruct` serving on a single GPU, validated hands-on on 2026-09-23.

## What was actually validated

- Single **NVIDIA H100 80GB** GPU (not the ≥2-GPU cluster node the use case's scripts used to
  assume — `serve_model.sh`'s defaults are now `GPUS=0 TP=1`).
- `ai-dynamo/dynamo` cloned and built from source at the exact rev roundhouse's `Cargo.toml`
  pins: `ac7b7513790ef1d619b46f805aea03c9f21200ba`.
- `Qwen/Qwen2.5-Coder-32B-Instruct` served successfully — **no 14B fallback needed** (see
  "KV cache result" below for why it fit and when 14B would still be the right call).
- Real completion returned from Dynamo's own OpenAI-compatible endpoint (bypassing roundhouse
  entirely — this is a Dynamo-only smoke test, not a roundhouse routing test).

## Prerequisites

| Prerequisite | Why |
|---|---|
| A GPU (this was run on 1× H100 80GB) | `python -m dynamo.vllm` needs CUDA |
| `sudo` access | System packages below need `apt install` |
| ~65 GB free disk | Model weights (safetensors, bf16) |
| Outbound network to GitHub + Hugging Face | Clone Dynamo, download weights |
| `uv` (https://docs.astral.sh/uv/) | Everything here is installed into a `uv venv`, no system `pip` needed |
| Rust toolchain | Dynamo's Python bindings are built with `maturin`, which needs `cargo` |

## Step-by-step

### 1. System packages (needs sudo)

```bash
sudo apt install -y build-essential libhwloc-dev libudev-dev pkg-config libclang-dev \
    protobuf-compiler python3-dev cmake libzmq3-dev
```

The first seven are Dynamo's own documented build dependencies (its
`docs/fern/pages/developer-guide/advanced-customizations/building-from-source.md`).
`libzmq3-dev` is what roundhouse's own `Cargo build` needs separately (transitively, via
`dynamo-kv-router`'s `standalone-selection` feature — see this repo's root `README.md`).

### 2. Rust toolchain (if you don't already have one)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

### 3. Clone Dynamo at roundhouse's pinned rev

```bash
git clone https://github.com/ai-dynamo/dynamo.git ~/dynamo
cd ~/dynamo
git checkout ac7b7513790ef1d619b46f805aea03c9f21200ba
```

Re-check this rev against `Cargo.toml:37-45` in this repo before reusing these steps later —
if the pin has moved, re-verify against the new rev's own build docs (CLAUDE.md: "synergy
dependencies are watched, not just pinned").

### 4. Python venv + Rust bindings + GPU memory service

```bash
cd ~/dynamo
uv venv .venv
source .venv/bin/activate
uv pip install pip 'maturin[patchelf]'
cd lib/bindings/python && maturin develop --uv && cd ~/dynamo   # ~3.5 min, compiles dynamo-kv-router etc.
uv pip install -e lib/gpu_memory_service
```

### 5. Install Dynamo with the vLLM backend

```bash
uv pip install -e '.[vllm]'    # pulls torch 2.11.0, vllm 0.26.0, tilelang — several GB
python3 -c "import dynamo.vllm"   # should print nothing / exit 0
```

**CUDA 13 gotcha.** If your CUDA toolkit is 13.x, vLLM's FlashInfer sampler JIT-compiles against
version-skewed headers (`torch` pins runtime headers to 13.0, vLLM's `tilelang` dependency pulls
`nvidia-cuda-nvcc` 13.2) and the worker aborts at startup with a `cuda_toolkit.h` incompatibility
error. This is a known upstream issue (Dynamo's own troubleshooting doc names it;
[flashinfer#3493](https://github.com/flashinfer-ai/flashinfer/issues/3493)). Work around it:

```bash
export VLLM_USE_FLASHINFER_SAMPLER=0
```

`use-cases/cache-aware-routing/serve_model.sh` now sets this by default (overridable).

### 6. Bring up etcd + nats

```bash
cd ~/dynamo/dev && docker compose up -d
```

**Note:** `deploy/docker-compose.yml`, named in this use case's older docs, no longer exists at
the pinned rev — the file moved to `dev/docker-compose.yml`. `serve_model.sh` and `PLAN.md` have
been corrected.

### 7. Pull the model weights

```bash
cd <roundhouse repo root>
MODEL=Qwen/Qwen2.5-Coder-32B-Instruct ./use-cases/cache-aware-routing/pull_model.sh pull
```

~62 GB, downloads in a couple of minutes on a fast connection/local disk.

### 8. Serve it

```bash
GPUS=0 TP=1 MODEL=Qwen/Qwen2.5-Coder-32B-Instruct \
    ./use-cases/cache-aware-routing/serve_model.sh serve
```

This starts `python -m dynamo.frontend --router-mode kv --http-port 8000` (OpenAI-compatible
HTTP) and `python -m dynamo.vllm ... --kv-events-config '{"publisher":"zmq",...,"endpoint":
"tcp://*:20080"}'` (the ZMQ stream roundhouse's `EmbeddedFleet` would subscribe to, once built).

### 9. Verify

```bash
curl -s localhost:8000/v1/chat/completions -H 'Content-Type: application/json' -d \
  '{"model":"Qwen/Qwen2.5-Coder-32B-Instruct","messages":[{"role":"user","content":"Say OK"}],"max_tokens":10}'
```

Expect a real `chat.completion` object back, e.g.:

```json
{"id":"chatcmpl-...","choices":[{"message":{"content":"OK","role":"assistant"},"finish_reason":"stop"}],...}
```

## KV cache result — why 32B fit, and when 14B is still the right call

| Metric | Value |
|---|---|
| Weight load | 61.04 GiB, 11 s (local ext4 disk) |
| KV cache budget (vLLM default `gpu_memory_utilization`) | 9.97 GiB |
| KV cache size | 40,832 tokens |
| Max concurrency at `max_model_len=32768` | 1.25× |
| Steady-state GPU memory | ~76.9 / 81.6 GB used, ~4.2 GB free |

32B fit **without OOM**, but with almost no headroom: 1.25× concurrency means the box can barely
serve two full-length (32K-token) requests at once. That is ample for *this* use case's actual
shape — a single session at a time, a ~1,000-token corpus, 20 turns — but is the ceiling. If a
future use case on this same single-GPU box needs real concurrency (multiple simultaneous
sessions, or a longer corpus), fall back to `MODEL=Qwen/Qwen2.5-Coder-14B-Instruct` (same `GPUS=0
TP=1`), which leaves substantially more KV headroom (~28 GB weights vs. 62 GB).

## Measuring Tax A / Tax B directly against Dynamo (no roundhouse)

`use-cases/cache-aware-routing/run_local.py` replays the same `corpus.md` + `turns.jsonl` fixture
`run.py` uses, but straight against Dynamo's `/v1/chat/completions` — bypassing roundhouse
entirely, since there is currently no way to route a turn there through it. It reads vLLM's own
`usage.prompt_tokens_details.cached_tokens`, a real measurement of vLLM's prefix-cache block
matches, not an estimate. Run it against the server brought up above:

```bash
python3 use-cases/cache-aware-routing/run_local.py
```

A real run on this box (2026-09-23) showed both effects the use case is named for: session 1
turn 1 at 0% (cold), climbing to 97% by turn 20 (Tax A); session 2 turn 1 opening at 98.8%
because the corpus prefix was already resident from session 1 (Tax B). See `README.md`'s
"Local tier" section for the full table.

## What this does not prove

- **Not a roundhouse routing test.** The `curl` above talks to Dynamo's own frontend on `:8000`,
  never touching roundhouse. roundhouse has no `LocalExecutor` that can call this worker yet.
- **Not multi-GPU / tensor-parallel.** Validated at `TP=1` only.
- **Not a KV-cache-hit measurement.** No repeat-prefix turns were sent in this smoke test, so
  the ZMQ KV-event stream was never exercised end to end — only confirmed to be configured and
  bound without error.

See `GAPS.md`'s 2026-09-23 addendum for the full record and `INTEGRATION.md` for the options on
closing the `LocalExecutor` / `EmbeddedFleet`-wiring gap this document does not touch.
