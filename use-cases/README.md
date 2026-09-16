<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Use Cases

Each subdirectory is a structured evaluation of roundhouse applied to a specific domain. Every use
case gets the same four documents: a fitness **SCORECARD**, a **GAPS** analysis with architecture
diagram, and a **PLAN** of phased implementation steps.

## Index

| Use case | Domain | Score (current → target) | Status |
|---|---|---|---|
| [`cache-aware-routing`](cache-aware-routing/) | KV-cache re-discovery tax measurement | 12 / 24 → 20 / 24 | Frontier-only baseline running |

## Score bands

| Range | Meaning |
|---|---|
| 20–24 | Strong fit — proceed immediately |
| 14–19 | Good fit with known gaps — proceed with gap plan |
| 8–13 | Partial fit — evaluate whether gaps are M10-adjacent or need new milestones |
| 0–7 | Weak fit — document why and park |

## Adding a new use case

1. Copy `template/` to a new snake_case folder (e.g., `code_generation/`).
2. Replace all `# TODO:` markers in each file.
3. Run the scorecard against the 8 dimensions (0–3 each, max 24).
4. Fill GAPS.md — gap table first, then regenerate the Mermaid diagram.
5. Draft PLAN.md — four phases, each closing ≥ 1 gap row.
6. Add a row to this index.

The `template/` folder is designed so a Claude workflow can generate a first-pass scorecard and
gap analysis from codebase analysis; a human then edits and finalizes.
