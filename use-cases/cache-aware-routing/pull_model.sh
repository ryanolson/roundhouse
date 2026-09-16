#!/bin/bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Pull the local-tier model weights and (optionally) install Dynamo from your clone.
#
# Run this on the GPU cluster node where Dynamo will serve.
#
# Usage:
#   ./pull_model.sh                          # install dynamo from clone + pull weights
#   ./pull_model.sh pull                     # pull weights only
#   ./pull_model.sh install                  # install dynamo from clone only
#   DYNAMO_CLONE=/path/to/dynamo ./pull_model.sh
#   MODEL=Qwen/Qwen2.5-7B-Instruct ./pull_model.sh pull    # smaller model for testing

set -euo pipefail

MODEL="${MODEL:-Qwen/Qwen2.5-Coder-32B-Instruct}"

# Path to your cloned ai-dynamo/dynamo repo.
# The script will try to find it automatically; override with DYNAMO_CLONE if needed.
DYNAMO_CLONE="${DYNAMO_CLONE:-}"

# ---------------------------------------------------------------------------

find_dynamo_clone() {
  # Common locations to look for a Dynamo clone.
  local candidates=(
    "$HOME/dynamo"
    "$HOME/ai-dynamo"
    "$HOME/repos/dynamo"
    "/workspace/dynamo"
    "/mnt/c/Users/$USER/dynamo"     # WSL path to Windows home
  )
  for d in "${candidates[@]}"; do
    if [[ -f "$d/Cargo.toml" ]] && grep -q "dynamo" "$d/Cargo.toml" 2>/dev/null; then
      echo "$d"
      return 0
    fi
  done
  return 1
}

install_dynamo() {
  if [[ -z "$DYNAMO_CLONE" ]]; then
    if ! DYNAMO_CLONE=$(find_dynamo_clone); then
      echo "ERROR: Could not find your Dynamo clone." >&2
      echo "       Set DYNAMO_CLONE=/path/to/dynamo and re-run." >&2
      exit 1
    fi
  fi

  echo ">>> Found Dynamo clone at: $DYNAMO_CLONE"

  # Verify the pinned rev roundhouse builds against is present.
  # If the clone is at a different rev, the KV-event wire may differ.
  PINNED_REV="ac7b7513790ef1d619b46f805aea03c9f21200ba"
  if git -C "$DYNAMO_CLONE" cat-file -e "${PINNED_REV}" 2>/dev/null; then
    echo "    Pinned rev $PINNED_REV is present. Good."
  else
    echo "    WARNING: pinned rev $PINNED_REV not found in this clone."
    echo "    roundhouse builds against that exact rev. If your clone is ahead,"
    echo "    re-check the KV-event flags match (see CLAUDE.md: synergy deps are watched)."
  fi

  echo ">>> Installing Dynamo Python packages from clone..."
  # Install the vLLM backend and the core Dynamo package.
  # Adjust the subdirectory if your clone layout differs.
  if [[ -f "$DYNAMO_CLONE/python/dynamo/setup.py" ]] || [[ -f "$DYNAMO_CLONE/python/dynamo/pyproject.toml" ]]; then
    pip install -e "$DYNAMO_CLONE/python/dynamo"
  elif [[ -f "$DYNAMO_CLONE/pyproject.toml" ]]; then
    pip install -e "$DYNAMO_CLONE"
  else
    echo "ERROR: Could not find a Python package to install in $DYNAMO_CLONE." >&2
    echo "       Your Dynamo clone layout may differ. Run: pip install -e <path_to_python_pkg>" >&2
    exit 1
  fi

  # Verify the import works.
  if python -c "import dynamo.vllm" >/dev/null 2>&1; then
    echo ">>> dynamo.vllm is importable. Install OK."
  else
    echo ">>> WARNING: dynamo.vllm is not importable yet."
    echo "    You may need to also install the vLLM backend:"
    echo "      pip install -e $DYNAMO_CLONE/backends/vllm  (or similar path)"
    echo "    Then verify with: python -c 'import dynamo.vllm'"
  fi
}

pull_weights() {
  echo ">>> Pulling model weights: $MODEL"
  echo "    This is ~64 GB for the 32B variant; allow 30-60 min on first run."
  echo "    Weights cache to HF_HOME (default ~/.cache/huggingface/hub/)."

  if ! command -v huggingface-cli >/dev/null 2>&1; then
    echo ">>> huggingface-cli not found; installing huggingface_hub[cli]..."
    pip install -U "huggingface_hub[cli]"
  fi

  # --exclude "original/*" skips the original (non-safetensors) weights — not needed.
  huggingface-cli download "${MODEL}" --exclude "original/*"

  echo ""
  echo ">>> Weights downloaded. To verify:"
  echo "    huggingface-cli scan-cache | grep ${MODEL}"
  echo ""
  echo ">>> To serve (once Dynamo is installed):"
  echo "    On the cluster: cd \$(git rev-parse --show-toplevel) && ./use-cases/cache-aware-routing/serve_model.sh serve"
  echo "    Or from use-cases/: cp use-cases/cache-aware-routing/serve_model.sh use-cases/cache-aware-routing/"
  echo ""
  echo ">>> SSH tunnel from laptop:"
  echo "    ssh -L 8080:localhost:8080 user@your-gpu-cluster"
  echo "    # Then codex on laptop connects to localhost:8080"
}

case "${1:-all}" in
  pull)    pull_weights ;;
  install) install_dynamo ;;
  all)     install_dynamo; pull_weights ;;
  *) echo "usage: $0 [pull|install|all]" >&2; exit 2 ;;
esac
