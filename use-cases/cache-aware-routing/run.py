#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Drive the cache-aware-routing roundhouse demo.

What it shows
-------------
Every turn of a conversation re-sends the whole history (system prompt + all
prior Q/A). A naive setup pays to reprocess that growing prefix on every turn --
the "re-discovery tax". Roundhouse admits only the new suffix onto an append-only
session log and reports, per turn, how many input tokens were served from cache
(`input_tokens_details.cached_tokens`) versus freshly processed.

This driver:
  1. Sends the SAME large system prompt (corpus.md) on every turn of every
     session -- the long, identical prefix whose reprocessing cost we measure.
  2. Replays turns.jsonl as a multi-turn conversation per membership, growing
     the history each turn (Tax A: within-session prefix growth).
  3. Runs it for each user in control-plane.json over the identical corpus
     (Tax B: cross-session identical prefix), so you can compare cached-token
     behavior between them.
  4. Prints per-turn cached vs. uncached input tokens, then the /v1/metrics
     snapshot roundhouse folded from the log.

Prereqs: roundhouse running (see vault/launch_roundhouse.py), and
`python use-cases/cache-aware-routing/mint_keys.py` already run so keys.local.json exists.

Usage:
    python use-cases/cache-aware-routing/run.py
    ROUNDHOUSE_URL=http://127.0.0.1:8080 python use-cases/cache-aware-routing/run.py
    CACHE_ASSERT_PCT=70 python use-cases/cache-aware-routing/run.py  # warn if cache% < 70
"""
from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
BASE_URL = os.environ.get("ROUNDHOUSE_URL", "http://127.0.0.1:8080").rstrip("/")
TURN_KEY_HEADER = "x-roundhouse-key"
CACHE_ASSERT_PCT = float(os.environ.get("CACHE_ASSERT_PCT", "50"))

SYSTEM_PREAMBLE = (
    "You are a precise engineering assistant answering questions about the "
    "Ledgerline service described below. Answer only from the document; if the "
    "document does not say, reply exactly 'not specified'. Be concise.\n\n"
)


def load_json(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def load_turns() -> list[str]:
    turns = []
    for line in (HERE / "turns.jsonl").read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            turns.append(json.loads(line)["q"])
    return turns


def stream_turn(secret: str, instructions: str, conversation: list[dict], cache_key: str) -> dict:
    """POST one turn, consume the SSE stream, return {text, usage, error, bytes_sent}."""
    body = json.dumps(
        {
            "instructions": instructions,
            "input": conversation,
            "stream": True,
            "prompt_cache_key": cache_key,
        }
    ).encode("utf-8")
    req = urllib.request.Request(
        f"{BASE_URL}/v1/responses",
        data=body,
        method="POST",
        headers={"content-type": "application/json", TURN_KEY_HEADER: secret},
    )

    text_parts: list[str] = []
    usage: dict = {}
    error: str | None = None
    try:
        with urllib.request.urlopen(req, timeout=300) as resp:
            for raw in resp:
                line = raw.decode("utf-8", "replace").strip()
                if not line or line.startswith(":") or not line.startswith("data:"):
                    continue
                payload = line[len("data:"):].strip()
                if payload == "[DONE]":
                    break
                try:
                    event = json.loads(payload)
                except json.JSONDecodeError:
                    continue
                etype = event.get("type", "")
                if etype == "response.output_text.delta":
                    text_parts.append(event.get("delta", ""))
                elif etype == "response.completed":
                    usage = event.get("response", {}).get("usage", {}) or {}
                elif etype == "response.failed":
                    error = json.dumps(event.get("response", {}).get("error", event))
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", "replace")
        error = f"HTTP {exc.code}: {detail}"
    except urllib.error.URLError as exc:
        error = f"connection error: {exc}. Is roundhouse running at {BASE_URL}?"

    return {"text": "".join(text_parts), "usage": usage, "error": error, "bytes_sent": len(body)}


def run_session(label: str, secret: str, instructions: str, turns: list[str], cache_key: str) -> dict:
    print(f"\n=== session: {label}  (prompt_cache_key={cache_key}) ===")
    print(f"{'turn':>4}  {'in_tok':>8}  {'cached':>8}  {'cache%':>7}  {'out_tok':>8}  {'sent_B':>8}")
    conversation: list[dict] = []
    totals = {"input": 0, "cached": 0, "output": 0}

    for i, q in enumerate(turns, start=1):
        conversation.append({"type": "message", "role": "user", "content": q})
        result = stream_turn(secret, instructions, conversation, cache_key)
        if result["error"]:
            print(f"{i:>4}  ERROR: {result['error']}")
            return totals
        usage = result["usage"]
        in_tok = int(usage.get("input_tokens", 0))
        cached = int(usage.get("input_tokens_details", {}).get("cached_tokens", 0))
        out_tok = int(usage.get("output_tokens", 0))
        pct = (100.0 * cached / in_tok) if in_tok else 0.0
        totals["input"] += in_tok
        totals["cached"] += cached
        totals["output"] += out_tok
        print(f"{i:>4}  {in_tok:>8}  {cached:>8}  {pct:>6.1f}%  {out_tok:>8}  {result['bytes_sent']:>8}")
        # Append the assistant answer so the next turn re-sends the grown history.
        conversation.append(
            {"type": "message", "role": "assistant", "content": result["text"]}
        )

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


def fetch_metrics(admin_secret: str | None) -> None:
    headers = {}
    if admin_secret:
        headers[TURN_KEY_HEADER] = admin_secret
    req = urllib.request.Request(f"{BASE_URL}/v1/metrics", headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            snapshot = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        print(f"\n/v1/metrics -> HTTP {exc.code}: {exc.read().decode('utf-8', 'replace')}")
        return
    except urllib.error.URLError as exc:
        print(f"\n/v1/metrics -> connection error: {exc}")
        return
    print("\n=== /v1/metrics snapshot (folded from the session log) ===")
    print(json.dumps(snapshot, indent=2))
    print(f"\nOpen the live dashboard at {BASE_URL}/v1/metrics/dashboard")


def main() -> None:
    keys_file = HERE / "keys.local.json"
    if not keys_file.exists():
        sys.exit("keys.local.json not found -- run `python use-cases/cache-aware-routing/mint_keys.py` first.")
    secrets_map = load_json(keys_file)
    admin_secret = secrets_map.get("__admin__")

    corpus = (HERE / "corpus.md").read_text(encoding="utf-8")
    instructions = SYSTEM_PREAMBLE + corpus
    turns = load_turns()

    plane = load_json(HERE / "control-plane.json")
    memberships = [(k["project"], k["user"]) for k in plane.get("keys", [])]

    print(f"roundhouse: {BASE_URL}")
    print(f"corpus: {len(corpus)} chars of shared prefix; {len(turns)} turns per session")
    print("The same corpus is the system prompt on every turn of every session.")
    print("Watch 'cached' climb within a session (Tax A) and across users (Tax B).")
    print(f"Cache% soft threshold: {CACHE_ASSERT_PCT:.0f}% (set CACHE_ASSERT_PCT to change)")

    for project, user in memberships:
        label = f"{project}/{user}"
        secret = secrets_map.get(label)
        if not secret:
            print(f"\n(skipping {label}: no secret in keys.local.json)")
            continue
        run_session(label, secret, instructions, turns, cache_key=f"{user}-run1")

    fetch_metrics(admin_secret)


if __name__ == "__main__":
    main()
