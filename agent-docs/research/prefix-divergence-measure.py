#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Measure where Claude Code sessions diverge, on Roundhouse's canonical items.

Approximates roundhouse-server messages_api::wire::canonicalize and
roundhouse-core Item::render at 2dd40dd. Byte-exact agreement with the Rust
render is not needed here: the measurement only needs one deterministic
per-item encoding applied to every fixture alike.
"""
import difflib
import hashlib
import json
import sys
from pathlib import Path

FIX = Path(sys.argv[1])


def compact(v):
    return json.dumps(v, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def is_budget_notice(text):
    t = text.strip()
    if not (t.startswith("<total_tokens>") and t.endswith("</total_tokens>")):
        return False
    inner = t[len("<total_tokens>"):-len("</total_tokens>")]
    return "<" not in inner


def is_ephemeral(msg):
    if msg.get("role") != "system":
        return False
    c = msg.get("content")
    if isinstance(c, str):
        return is_budget_notice(c)
    if isinstance(c, list) and len(c) == 1:
        b = c[0]
        return b.get("type") == "text" and is_budget_notice(b.get("text", ""))
    return False


def block_item(role, block):
    t = block.get("type")
    if t == "text":
        return (role, f"{block['text']}")
    if t == "tool_use":
        return ("assistant", f'<tool_call id="{block["id"]}" name="{block["name"]}">{compact(block["input"])}</tool_call>')
    if t == "tool_result":
        c = block.get("content")
        out = c if isinstance(c, str) else ("" if c is None else compact(c))
        return ("tool", f'<tool_result id="{block["tool_use_id"]}">{out}</tool_result>')
    if t == "thinking":
        return (role, f'<thinking signature="{block["signature"]}">{block["thinking"]}</thinking>')
    digest = hashlib.sha256(compact(block).encode()).hexdigest()
    return (role, f'<block type="{t}" sha256="{digest}">')


def canonicalize(body):
    items = []
    sysf = body.get("system")
    if isinstance(sysf, str) and sysf:
        items.append(("system", sysf))
    elif isinstance(sysf, list):
        for b in sysf:
            items.append(block_item("system", b))
    for m in body["messages"]:
        if is_ephemeral(m):
            continue
        c = m["content"]
        if isinstance(c, str):
            items.append((m["role"], c))
        else:
            for b in c:
                items.append(block_item(m["role"], b))
    # mark_turn_configuration: leading system run becomes developer
    out = []
    leading = True
    for role, text in items:
        if leading and role == "system":
            out.append(("developer", text))
        else:
            leading = False
            out.append((role, text))
    return out


def render(item):
    role, body = item
    return f"<|{role}|>{body}"


def item_digest(item):
    r = render(item).encode()
    return hashlib.sha256(len(r).to_bytes(8, "big") + r).digest()


def chain(items, key=b"demo-key"):
    out = []
    c = hashlib.sha256(b"roundhouse-prefix-chain-v1\0" + key).digest()
    for it in items:
        c = hashlib.sha256(c + item_digest(it)).digest()
        out.append(c)
    return out


def common_prefix_len(a, b):
    n = 0
    for x, y in zip(a, b):
        if x != y:
            break
        n += 1
    return n


def first_diff_byte(a, b):
    n = common_prefix_len(a, b)
    return n


def tools_digest(body):
    return hashlib.sha256(compact(body.get("tools", [])).encode()).hexdigest()[:12]


def load(name):
    return json.loads((FIX / name).read_text())


names = [
    "claude-2.1.251-turn-1.json",
    "claude-2.1.251-turn-2-continue.json",
    "claude-2.1.257-turn-1.json",
    "claude-2.1.257-turn-2-continue.json",
    "claude-2.1.257-turn-3-continue.json",
    "claude-2.1.257-mcp-turn-1.json",
    "claude-2.1.257-mcp-turn-2-toolresult.json",
]
bodies = {n: load(n) for n in names}
canon = {n: canonicalize(b) for n, b in bodies.items()}
chains = {n: chain(canon[n]) for n in names}

print("## Canonical items per request (index, role, render bytes, est tokens=bytes/4, first 60 chars)")
for n in names:
    b = bodies[n]
    tools_bytes = len(compact(b.get("tools", [])))
    print(f"\n### {n}  tools={len(b.get('tools', []))} tools_bytes={tools_bytes} tools_digest={tools_digest(b)}")
    cum = 0
    for i, it in enumerate(canon[n]):
        r = render(it)
        cum += len(r.encode())
        print(f"  [{i}] {it[0]:<9} {len(r.encode()):>6} B  cum {cum:>6} B  ~{len(r.encode())//4:>5} tok  {r[:60]!r}")

pairs = [
    ("same session, next turn (2.1.257 t1->t2)", "claude-2.1.257-turn-1.json", "claude-2.1.257-turn-2-continue.json"),
    ("same session, next turn (2.1.257 t2->t3)", "claude-2.1.257-turn-2-continue.json", "claude-2.1.257-turn-3-continue.json"),
    ("same session, next turn (2.1.251 t1->t2)", "claude-2.1.251-turn-1.json", "claude-2.1.251-turn-2-continue.json"),
    ("same session, tool loop (2.1.257 mcp t1->t2)", "claude-2.1.257-mcp-turn-1.json", "claude-2.1.257-mcp-turn-2-toolresult.json"),
    ("two sessions, one version (2.1.257 A vs B)", "claude-2.1.257-turn-1.json", "claude-2.1.257-mcp-turn-1.json"),
    ("two sessions, two versions (2.1.251 vs 2.1.257), same prompt", "claude-2.1.251-turn-1.json", "claude-2.1.257-turn-1.json"),
]
print("\n## Pairwise divergence")
for label, x, y in pairs:
    cx, cy = canon[x], canon[y]
    k = common_prefix_len(chains[x], chains[y])
    same_items = [i for i in range(min(len(cx), len(cy))) if cx[i] == cy[i]]
    diff_items = [i for i in range(min(len(cx), len(cy))) if cx[i] != cy[i]]
    matched_bytes = sum(len(render(it).encode()) for it in cx[:k])
    print(f"\n### {label}")
    print(f"  items {len(cx)} vs {len(cy)}; chain common prefix = {k} items ({matched_bytes} B)")
    print(f"  equal item positions: {same_items}; differing positions: {diff_items}")
    for i in diff_items:
        a, b = render(cx[i]), render(cy[i])
        fb = first_diff_byte(a, b)
        sm = difflib.SequenceMatcher(None, a, b, autojunk=False)
        edits = [(op, a[i1:i2][:40], b[j1:j2][:40]) for op, i1, i2, j1, j2 in sm.get_opcodes() if op != "equal"]
        print(f"   item[{i}] {cx[i][0]}: first differing byte {fb} of {len(a.encode())}/{len(b.encode())}; edits {edits[:6]}")
    tx, ty = compact(bodies[x].get("tools", [])), compact(bodies[y].get("tools", []))
    tfb = first_diff_byte(tx, ty)
    print(f"  tools: {len(tx)} B vs {len(ty)} B; equal={tx == ty}; first differing byte {tfb}")
    if tx != ty:
        nx = [t["name"] for t in bodies[x]["tools"]]
        ny = [t["name"] for t in bodies[y]["tools"]]
        print(f"   tool names only in first: {sorted(set(nx) - set(ny))}; only in second: {sorted(set(ny) - set(nx))}")
        for t1 in bodies[x]["tools"]:
            for t2 in bodies[y]["tools"]:
                if t1["name"] == t2["name"] and compact(t1) != compact(t2):
                    a, b = compact(t1), compact(t2)
                    sm = difflib.SequenceMatcher(None, a, b, autojunk=False)
                    edits = [(op, a[i1:i2][:50], b[j1:j2][:50]) for op, i1, i2, j1, j2 in sm.get_opcodes() if op != "equal"]
                    print(f"   tool {t1['name']} differs: {edits[:4]}")
        # index of first differing tool in order
        fi = next((i for i, (p, q) in enumerate(zip(bodies[x]["tools"], bodies[y]["tools"])) if compact(p) != compact(q)), None)
        print(f"   first differing tool index in array order: {fi} ({bodies[x]['tools'][fi]['name'] if fi is not None else '-'})")

# Anthropic cache order: tools, then system, then messages. Bytes shared before divergence.
print("\n## Shared bytes in Anthropic cache order (tools || system || messages), compact JSON")
def wire_prefix(body):
    return compact(body.get("tools", [])) + compact(body.get("system")) + compact(body["messages"])
for label, x, y in pairs:
    a, b = wire_prefix(bodies[x]), wire_prefix(bodies[y])
    fb = first_diff_byte(a, b)
    print(f"  {label}: shared {fb} B of {len(a)}/{len(b)} (~{fb//4} tok)")

# Roundhouse local render order: items only (tools are not items).
print("\n## Shared bytes in Roundhouse local render order (items only)")
for label, x, y in pairs:
    a = "".join(render(i) for i in canon[x])
    b = "".join(render(i) for i in canon[y])
    fb = first_diff_byte(a, b)
    print(f"  {label}: shared {fb} B of {len(a)}/{len(b)} (~{fb//4} tok)")

# Attribution fingerprint reproduction (claude-code-client-surface.md 4.4 formula)
print("\n## Attribution fingerprint reproduction")
def fp(msg, ver):
    y = "".join(msg[i] if i < len(msg) else "0" for i in (4, 7, 20))
    return hashlib.sha256(("59cf53e54c78" + y + ver).encode()).hexdigest()[:3]
for n in names:
    b = bodies[n]
    hdr = b["system"][0]["text"]
    ver = hdr.split("cc_version=")[1].split(";")[0]
    base, suffix = ver.rsplit(".", 1)
    first_typed = [c for c in b["messages"][0]["content"] if c.get("type") == "text"][-1]["text"]
    print(f"  {n}: header suffix {suffix}; sha256('59cf53e54c78'+chars[4,7,20] of {first_typed!r}+'{base}')[:3] = {fp(first_typed, base)}")
