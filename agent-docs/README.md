<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# agent-docs

Plans, synergy rulings, deep-dive evidence, and design records live here —
the documents that say where the product is going and why, as opposed to
`README.md` and module docs, which say what the code does today.

Two document kinds, deliberately paired, each with its own folder:

- **Evidence** (`research/`): a read of an external codebase or a design
  space, pinned to a revision, every claim carrying a file:line.
  Evidence is a snapshot — when the world moves after it was taken, it
  gains *dated bracketed notes*, never silent rewrites.
- **Rulings** (`synergies/` for synergy directions; plans at this
  directory's root): the decisions the evidence justifies. A ruling
  names its evidence, and where the two disagree the ruling wins — the
  disagreement is a bug in one of them.

The dependency-synergy vigilance rule in `CLAUDE.md` says when documents
here must be updated; this directory is where those updates land.
