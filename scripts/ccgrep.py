#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Search an installed Claude Code executable as bytes and print a window around each hit.

The executable is never run. It is a Bun single-file binary with minified
JavaScript inside, so a byte search is the only reliable read. Use it to
read a claim about the client again when the pinned Claude Code version
moves.

Usage:
  ccgrep.py <executable> <pattern> <before> <after> [max_hits=3] [start_offset=150000000]

Example:
  ccgrep.py ~/.local/share/claude/versions/2.1.284 CLAUDE_CODE_GATEWAY_HINT_HEADERS 200 400
"""
import sys

exe, pattern = sys.argv[1], sys.argv[2].encode()
before, after = int(sys.argv[3]), int(sys.argv[4])
max_hits = int(sys.argv[5]) if len(sys.argv) > 5 else 3
# The JavaScript payload sits after the Bun runtime; starting past it skips
# false hits in the native code.
start = int(sys.argv[6]) if len(sys.argv) > 6 else 150_000_000

data = open(exe, "rb").read()
i, hits = start, 0
while hits < max_hits:
    j = data.find(pattern, i)
    if j < 0:
        break
    print(f"--- @{j}")
    print(data[max(0, j - before) : j + after].decode("utf-8", "replace"))
    i, hits = j + 1, hits + 1
