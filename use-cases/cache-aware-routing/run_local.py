#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Drive the cache-aware-routing demo directly against the local Dynamo worker --
no roundhouse in the loop at all.

Why this script exists
-----------------------
`run.py` measures cache% for the FRONTIER path: it goes through roundhouse's
`/v1/responses`, which dispatches to NVIDIA's inference-api over the
`anthropic_messages` dialect. That is the only path currently reachable end to
end, because roundhouse's local-tier wiring is not built yet -- see GAPS.md:
`crates/roundhouse-fleet/src/local.rs`'s `LocalExecutor` trait has no real
implementation (only `EchoLocalExecutor`), and `roundhouse-server`'s `main.rs`
wires no `EmbeddedFleet`/`LocalFleet` at all. There is currently no way to send
a turn "through roundhouse" and have it land on the local Dynamo worker.

So this script talks to Dynamo directly, the same way `serve_model.sh`'s own
smoke test does: `POST {DYNAMO_URL}/v1/chat/completions`, bypassing roundhouse
entirely. It replays the identical corpus + turns.jsonl fixture `run.py` uses,
in the same growing-conversation shape, so the two scripts' output is directly
comparable -- but where `run.py`'s cached_tokens is a number NVIDIA's frontier
API reports back over HTTP, this script's cached_tokens is the number vLLM's
own OpenAI-compatible endpoint reports (`usage.prompt_tokens_details.
cached_tokens`), which vLLM derives from its real prefix-cache block matches
(`--enable-prefix-caching`, on by default in `serve_model.sh`). Both are real,
provider/engine-reported measurements -- see DYNAMO_LOCAL_SERVING.md and the
cache%-tracing discussion in this use case's history for why neither is a
`CacheLedger`/decay-model estimate.

What it does NOT exercise
--------------------------
- No roundhouse session log, control plane, budgets, or routing decisions --
  this is Dynamo alone.
- No ZMQ KV-event stream reading -- that is a separate, roundhouse-side
  measurement (`EmbeddedFleet`'s block-hash indexing), unrelated to the
  `cached_tokens` this script reads from vLLM's own HTTP response.
- No `prompt_cache_key` concept -- vLLM's prefix cache is content-addressed
  (hash of the actual token prefix), not keyed, so "Tax B" here is just two
  sequential conversations sharing the same corpus prefix.

Prereqs: a Dynamo frontend + vLLM worker serving on DYNAMO_URL (default
http://localhost:8000) -- see DYNAMO_LOCAL_SERVING.md or run:
    GPUS=0 TP=1 ./use-cases/cache-aware-routing/serve_model.sh serve

Usage:
    python use-cases/cache-aware-routing/run_local.py
    DYNAMO_URL=http://localhost:8000 MODEL=Qwen/Qwen2.5-Coder-32B-Instruct \\
        python use-cases/cache-aware-routing/run_local.py
    CACHE_ASSERT_PCT=70 python use-cases/cache-aware-routing/run_local.py
"""
from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
DYNAMO_URL = os.environ.get("DYNAMO_URL", "http://localhost:8000").rstrip("/")
MODEL = os.environ.get("MODEL", "Qwen/Qwen2.5-Coder-32B-Instruct")
CACHE_ASSERT_PCT = float(os.environ.get("CACHE_ASSERT_PCT", "50"))
# How many "sessions" to replay -- >=2 shows Tax B (the second session's turn 1
# starting warm because the first session already primed the corpus prefix in
# vLLM's prefix-cache block table).
NUM_SESSIONS = int(os.environ.get("NUM_SESSIONS", "2"))

SYSTEM_PREAMBLE = (
    "You are a precise engineering assistant answering questions about the "
    "Ledgerline service described below. Answer only from the document; if the "
    "document does not say, reply exactly 'not specified'. Be concise.\n\n"
)


def load_turns() -> list[str]:
    turns = []
    for line in (HERE / "turns.jsonl").read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            turns.append(json.loads(line)["q"])
    return turns


def chat_turn(system_prompt: str, conversation: list[dict]) -> dict:
    """POST one non-streaming chat/completions turn, return {text, usage, error}."""
    body = json.dumps(
        {
            "model": MODEL,
            "messages": [{"role": "system", "content": system_prompt}] + conversation,
            "max_tokens": 256,
            "stream": False,
        }
    ).encode("utf-8")
    req = urllib.request.Request(
        f"{DYNAMO_URL}/v1/chat/completions",
        data=body,
        method="POST",
        headers={"content-type": "application/json"},
    )

    text = ""
    usage: dict = {}
    error: str | None = None
    try:
        with urllib.request.urlopen(req, timeout=300) as resp:
            payload = json.loads(resp.read().decode("utf-8"))
        choices = payload.get("choices", [])
        if choices:
            text = choices[0].get("message", {}).get("content", "") or ""
        usage = payload.get("usage", {}) or {}
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", "replace")
        error = f"HTTP {exc.code}: {detail}"
    except urllib.error.URLError as exc:
        error = f"connection error: {exc}. Is Dynamo serving at {DYNAMO_URL}?"

    return {"text": text, "usage": usage, "error": error, "bytes_sent": len(body)}


def run_session(label: str, system_prompt: str, turns: list[str]) -> dict:
    print(f"\n=== session: {label}  (model={MODEL}, direct to Dynamo) ===")
    print(f"{'turn':>4}  {'in_tok':>8}  {'cached':>8}  {'cache%':>7}  {'out_tok':>8}  {'sent_B':>8}")
    conversation: list[dict] = []
    totals = {"input": 0, "cached": 0, "output": 0}

    for i, q in enumerate(turns, start=1):
        conversation.append({"role": "user", "content": q})
        result = chat_turn(system_prompt, conversation)
        if result["error"]:
            print(f"{i:>4}  ERROR: {result['error']}")
            return totals
        usage = result["usage"]
        in_tok = int(usage.get("prompt_tokens", 0))
        # `or {}`, not a `.get` default: a server with prompt-token details off
        # sends the key as an explicit null, which a default does not cover.
        cached = int((usage.get("prompt_tokens_details") or {}).get("cached_tokens", 0) or 0)
        out_tok = int(usage.get("completion_tokens", 0))
        pct = (100.0 * cached / in_tok) if in_tok else 0.0
        totals["input"] += in_tok
        totals["cached"] += cached
        totals["output"] += out_tok
        print(f"{i:>4}  {in_tok:>8}  {cached:>8}  {pct:>6.1f}%  {out_tok:>8}  {result['bytes_sent']:>8}")
        conversation.append({"role": "assistant", "content": result["text"]})

    saved = totals["cached"]
    total_in = totals["input"]
    frac = (100.0 * saved / total_in) if total_in else 0.0
    print(
        f"  session totals: input={total_in} cached={saved} "
        f"({frac:.1f}% of input served from cache) output={totals['output']}"
    )
    if total_in > 0 and frac < CACHE_ASSERT_PCT:
        print(
            f"  WARNING: session cache% ({frac:.1f}%) is below threshold ({CACHE_ASSERT_PCT:.0f}%). "
            f"Set CACHE_ASSERT_PCT to adjust or suppress."
        )
    return totals


def main() -> None:
    corpus = (HERE / "corpus.md").read_text(encoding="utf-8")
    system_prompt = SYSTEM_PREAMBLE + corpus
    turns = load_turns()

    print(f"Dynamo: {DYNAMO_URL}  model={MODEL}")
    print(f"corpus: {len(corpus)} chars of shared prefix; {len(turns)} turns per session")
    print("This talks to Dynamo/vLLM directly -- roundhouse is not involved.")
    print("Watch 'cached' climb within a session (Tax A) and across sessions (Tax B).")
    print(f"Cache% soft threshold: {CACHE_ASSERT_PCT:.0f}% (set CACHE_ASSERT_PCT to change)")

    for n in range(1, NUM_SESSIONS + 1):
        run_session(f"local-session-{n}", system_prompt, turns)


if __name__ == "__main__":
    main()
