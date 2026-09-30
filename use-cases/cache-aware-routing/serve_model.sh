#!/bin/bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Serve the local-tier model (Qwen2.5-Coder-32B) on Dynamo for the cache-aware-routing use case.
#
# Adapted from the pinned Dynamo rev roundhouse builds against:
#   ai-dynamo/dynamo @ ac7b7513790ef1d619b46f805aea03c9f21200ba
#   examples/backends/vllm/launch/agg_router.sh
# so the KV-event wire it stands up is the one roundhouse-fleet's EmbeddedFleet
# (dynamo-kv-router, standalone-selection) expects. If you bump the Dynamo pin in
# roundhouse's Cargo.toml, re-check this recipe against the new rev's launch script
# (CLAUDE.md: "synergy dependencies are watched, not just pinned").
#
# Prereqs (on the GPU cluster node, inside the Dynamo container or venv where
# `python -m dynamo.vllm` is importable):
#   - Dynamo + vLLM installed (`uv pip install -e '.[vllm]'` inside an ai-dynamo/dynamo
#     clone at the pinned rev -- see docs/fern/pages/developer-guide/advanced-customizations/
#     building-from-source.md in that clone -- or use the Dynamo runtime container).
#   - etcd + nats running: `docker compose -f dev/docker-compose.yml up -d`
#     (2026-09-23: `deploy/docker-compose.yml` no longer exists at the pinned rev
#     ac7b7513790ef1d619b46f805aea03c9f21200ba; the file moved to `dev/docker-compose.yml`.
#     Verified against a real clone checked out at that rev.)
#   - Weights downloaded: `./use-cases/cache-aware-routing/pull_model.sh pull`
#   - huggingface-cli logged in if model is gated (Qwen Coder is public).
#
# Usage:
#   ./serve_model.sh            # serve only (assumes weights present)
#   ./serve_model.sh serve      # explicit serve
#
# Override anything via env, e.g.:
#   MODEL=Qwen/Qwen2.5-Coder-32B-Instruct TP=4 GPUS=0,1,2,3 ./serve_model.sh
#
# For weight download, use pull_model.sh in this directory.

set -euo pipefail

# --- Configuration ----------------------------------------------------------

# The coding-specialized local model. 32B in bf16 is ~64 GB of weights, so on a
# single 80 GB GPU it leaves little headroom for KV cache; 14B (~28 GB) is the
# documented fallback if 32B's KV cache allocation fails at startup (OOM). See
# GAPS.md's 2026-09-23 addendum for what was actually observed on a 1x H100 80GB box.
MODEL="${MODEL:-Qwen/Qwen2.5-Coder-32B-Instruct}"

# MUST match roundhouse's WorkerRegistration.block_size when the local tier is wired.
# The Dynamo indexer drops KV events whose block_size differs from its own view,
# so this number is a contract, not a tuning knob.
BLOCK_SIZE="${BLOCK_SIZE:-64}"

# GPUs and tensor-parallel width. TP must divide the GPU count you expose.
# Defaults to a single GPU (TP=1) -- this use case has been run on a 1x H100 80GB
# box. Override GPUS=0,1 TP=2 (or more) on a real multi-GPU cluster node.
GPUS="${GPUS:-0}"
TP="${TP:-1}"

# Dynamo frontend (OpenAI-compatible) HTTP port — use this to smoke-test the model.
HTTP_PORT="${DYN_HTTP_PORT:-8000}"

# ZMQ endpoint the vLLM worker PUBLISHES KV cache events on. This is the endpoint
# roundhouse's EmbeddedFleet subscribes to (WorkerRegistration.kv_events_endpoints).
# Bind address for the worker; roundhouse connects to it as tcp://<host>:<port>.
KV_EVENTS_PORT="${KV_EVENTS_PORT:-20080}"
KV_EVENTS_ENDPOINT="${KV_EVENTS_ENDPOINT:-tcp://*:${KV_EVENTS_PORT}}"

# Dynamo worker system/metrics port (must be unique per worker on a host).
DYN_SYSTEM_PORT="${DYN_SYSTEM_PORT:-8081}"

# Deterministic hashing for KV event IDs, so the hashes roundhouse computes over a
# prompt match the ones the worker emits. agg_router.sh sets this for the same reason.
export PYTHONHASHSEED=0

# On a CUDA 13 install, vLLM's FlashInfer sampler JIT-compiles against
# version-skewed headers (torch pins CUDA 13.0, vLLM's tilelang dep pulls
# nvidia-cuda-nvcc 13.2) and aborts worker startup -- see the pinned Dynamo
# rev's docs/fern/.../building-from-source.md#troubleshooting and
# https://github.com/flashinfer-ai/flashinfer/issues/3493. Falls back to
# vLLM's native sampler; unset this if your CUDA toolkit and torch's CUDA
# runtime match exactly.
export VLLM_USE_FLASHINFER_SAMPLER="${VLLM_USE_FLASHINFER_SAMPLER:-0}"

# --- Serve ------------------------------------------------------------------

serve() {
  if ! python -c "import dynamo.vllm" >/dev/null 2>&1; then
    echo "ERROR: 'python -m dynamo.vllm' is not importable in this environment." >&2
    echo "       Install Dynamo's python packages from your clone, or run inside the" >&2
    echo "       Dynamo runtime container. See the Dynamo repo's vLLM backend README." >&2
    exit 1
  fi

  trap 'echo "Cleaning up..."; kill 0' EXIT

  echo "=================================================================="
  echo " Serving (aggregated + KV routing) on Dynamo"
  echo "   model         : ${MODEL}"
  echo "   block_size    : ${BLOCK_SIZE}   <- roundhouse WorkerRegistration.block_size"
  echo "   GPUs / TP     : ${GPUS} / ${TP}"
  echo "   HTTP (OpenAI) : http://0.0.0.0:${HTTP_PORT}"
  echo "   KV events ZMQ : ${KV_EVENTS_ENDPOINT}   <- roundhouse subscribes here"
  echo "=================================================================="

  # Dynamo frontend + its own KV router. This gives you a working OpenAI endpoint on
  # :$HTTP_PORT to verify the model independently of roundhouse.
  python -m dynamo.frontend --router-mode kv --http-port "${HTTP_PORT}" &

  # The vLLM worker. --enforce-eager is for quick bring-up; drop it for throughput.
  # --enable-prefix-caching is what makes KV-cache-events meaningful (no reused
  # prefixes, nothing to publish). Recent vLLM enables it by default, but we ask
  # explicitly so the contract does not depend on a default.
  DYN_SYSTEM_PORT="${DYN_SYSTEM_PORT}" \
  CUDA_VISIBLE_DEVICES="${GPUS}" \
  python3 -m dynamo.vllm \
    --model "${MODEL}" \
    --block-size "${BLOCK_SIZE}" \
    --tensor-parallel-size "${TP}" \
    --enable-prefix-caching \
    --enforce-eager \
    --kv-events-config "{\"publisher\":\"zmq\",\"topic\":\"kv-events\",\"endpoint\":\"${KV_EVENTS_ENDPOINT}\",\"enable_kv_cache_events\":true}" &

  wait -n
}

case "${1:-serve}" in
  serve) serve ;;
  *) echo "usage: $0 [serve]" >&2; exit 2 ;;
esac
