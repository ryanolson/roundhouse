#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""End to end: a Fabric-driven Codex against a real roundhouse.

Reads the `fabric.json` roundhouse's `fabric_launch` example emitted, plans and
diagnoses it through the NeMo Fabric SDK, starts one multi-turn runtime, runs
two ordered turns, then reads roundhouse's own metrics to see the two turns as
one session with the second turn's prefix admitted as cached input.
"""
from __future__ import annotations

import asyncio
import json
import os
import sys
import urllib.request
from pathlib import Path

from nemo_fabric import Fabric, FabricConfig

OUT = Path(sys.argv[1])
ROUNDHOUSE = os.environ.get("ROUNDHOUSE_URL", "http://127.0.0.1:8080")
NONCE = "FABRIC_E2E_NONCE_31337"


def plain(x):
    if x is None or isinstance(x, (str, int, float, bool)):
        return x
    if isinstance(x, Path):
        return str(x)
    if isinstance(x, (list, tuple)):
        return [plain(i) for i in x]
    if hasattr(x, "keys"):
        try:
            return {str(k): plain(x[k]) for k in x.keys()}
        except Exception:  # noqa: BLE001
            pass
    try:
        return {k: plain(v) for k, v in vars(x).items() if not k.startswith("_")}
    except Exception:  # noqa: BLE001
        return str(x)


def dump(model) -> dict:
    out = plain(model)
    return out if isinstance(out, dict) else {"repr": str(out)[:400]}


def get_json(path: str) -> dict:
    with urllib.request.urlopen(f"{ROUNDHOUSE}{path}", timeout=10) as resp:
        return json.loads(resp.read())


async def main() -> int:
    raw = json.loads((OUT / "fabric.json").read_text())
    config = FabricConfig.from_mapping(raw)
    fabric = Fabric()

    plan = fabric.plan(config, base_dir=OUT)
    pd = dump(plan)
    desc = (pd.get("adapter_descriptor") or {}).get("descriptor") or {}
    print("PLAN adapter:", desc.get("adapter_id"), "kind:", desc.get("adapter_kind"))
    cap = pd.get("capability_plan") or {}
    print("PLAN skill_paths:", cap.get("skill_paths"))
    print("PLAN mcp_servers:", list((cap.get("mcp_servers") or {}).keys()))
    doctor = await fabric.doctor(config, base_dir=OUT)
    dd = dump(doctor)
    print("DOCTOR status:", dd.get("status"))
    print("DOCTOR non-pass checks:", [str(c)[:160] for c in dd.get("checks", []) if "pass" not in str(c).lower()][:6])

    before = get_json("/v1/metrics")
    print("METRICS before: sessions=%s turns=%s tokens=%s" % (before.get("sessions"), before.get("turns"), before.get("tokens")))

    runtime = await fabric.start_runtime(config, base_dir=OUT)
    try:
        first = await runtime.invoke(input=f"Remember this value and nothing else: {NONCE}")
        print("TURN1 status:", first.status)
        r1 = dump(first); out1 = r1.get("output") or {}
        print("TURN1 result keys:", sorted(r1.keys()))
        print("TURN1 output keys:", sorted(out1.keys()) if isinstance(out1, dict) else type(out1))
        print("TURN1 thread_id:", out1.get("thread_id"))
        print("TURN1 response:", str(out1.get("response"))[:300])
        print("TURN1 usage:", r1.get("usage"), "| adapter usage:", (out1.get("usage") if isinstance(out1, dict) else None))
        second = await runtime.invoke(input="Reply with only the value I asked you to remember.")
        print("TURN2 status:", second.status)
        r2 = dump(second); out2 = r2.get("output") or {}
        print("TURN2 thread_id:", out2.get("thread_id"))
        print("TURN2 response:", str(out2.get("response"))[:300])
        print("TURN2 usage:", r2.get("usage"), "| adapter usage:", (out2.get("usage") if isinstance(out2, dict) else None))
        thread_id = out2.get("thread_id")
    finally:
        await runtime.stop()

    after = get_json("/v1/metrics")
    print("METRICS after: sessions=%s turns=%s calls=%s tokens=%s" % (after.get("sessions"), after.get("turns"), after.get("calls"), after.get("tokens")))
    if thread_id:
        for route in ("trajectory", "atof"):
            try:
                with urllib.request.urlopen(f"{ROUNDHOUSE}/v1/sessions/{thread_id}/{route}", timeout=10) as resp:
                    body = resp.read()
                    print(f"SESSION {route}: HTTP {resp.status}, {len(body)} bytes")
                    if route == "trajectory":
                        traj = json.loads(body)
                        steps = traj.get("steps", [])
                        print("TRAJECTORY steps:", len(steps), "final_metrics:", traj.get("final_metrics"))
            except Exception as exc:  # noqa: BLE001
                print(f"SESSION {route}: {exc}")
    ok = (
        str(first.status).lower().endswith("succeeded")
        and str(second.status).lower().endswith("succeeded")
        and after.get("turns", 0) - before.get("turns", 0) == 2
        and after.get("sessions", 0) - before.get("sessions", 0) == 1
    )
    print("E2E RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
