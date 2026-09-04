<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Integration Paths — TODO: Use Case Name

**Audience:** roundhouse repo admin / milestone planner.

**Purpose:** Translate the gaps from `GAPS.md` into decisions about what to build, in what order,
and whether alternatives exist. Each significant gap gets at least two resolution paths so the
admin can choose based on current milestone priorities rather than being handed a single option.

This document does not duplicate GAPS.md (the gap inventory) or PLAN.md (the use case author's
recommended execution path). It answers the question: *what does adding this use case tell us
about the repo, and what are our options?*

---

## Gap summary (admin view)

<!-- TODO: Distill GAPS.md into the subset of gaps that have implications for the repo roadmap.
  Not every gap belongs here — config placeholders and driver omissions are use-case-author
  responsibilities. Focus on gaps that require code changes in roundhouse itself, or that
  reveal a structural constraint the admin should weigh against other use cases.

  For each gap, note whether it blocks only this use case or is a cross-cutting concern
  (i.e., the same gap would appear in any use case that needs a local tier). -->

| Gap | Blocks | Cross-cutting? | Effort signal |
|---|---|---|---|
| TODO gap | This use case / All local-tier use cases | Yes / No | Low / Medium / High |

---

## Integration alternatives

<!-- TODO: For each significant gap, describe at least two resolution paths.
  Use the three standard options below as a starting point, but add or remove as needed.

  Option A is typically "build it correctly in roundhouse (the right long-term answer)."
  Option B is typically "work around it in the use case (defers the repo change)."
  Option C is typically "alternative design that avoids the gap differently."

  For each option, state: what changes, what it enables, what it costs, and what it forecloses.
  "Forecloses" is important — a workaround that prevents a clean future fix is worse than
  the gap itself.
-->

### Gap: TODO

**What the use case needs:** TODO

**Option A — TODO (recommended):**
- What changes: TODO
- Enables: TODO
- Costs: TODO
- Forecloses: TODO

**Option B — TODO:**
- What changes: TODO
- Enables: TODO
- Costs: TODO
- Forecloses: TODO

**Option C — TODO:**
- What changes: TODO
- Enables: TODO
- Costs: TODO
- Forecloses: TODO

**Admin recommendation:** TODO — cite the trade-off that makes one option preferable.

---

## Priority signal for the roadmap

<!-- TODO: Based on the gap summary and alternatives above, what should the admin consider
  adding to agent-docs/? Frame this as concrete suggestions, not vague wishes:
  - If a gap is cross-cutting: "Add a milestone for [specific capability] — it unblocks
    use cases [list them]."
  - If a gap is already planned: "This use case validates the priority of [M10.X item];
    no new milestone needed."
  - If a gap reveals a constraint that should be documented: "Record [architectural constraint]
    in agent-docs/synergies/ so future use cases know the co-location requirement upfront."

  Tie suggestions to specific files in agent-docs/ (PLAN-*.md, synergies/, research/) so
  the admin knows exactly where the new content would land. -->

| Suggestion | Target document | Priority |
|---|---|---|
| TODO | `agent-docs/TODO.md` | High / Medium / Low |

---

## Constraints worth documenting

<!-- TODO: Does this use case reveal an architectural constraint that future use case authors
  should know before they start their scorecard?

  Examples:
  - "EmbeddedFleet requires co-location with Dynamo — ZMQ KV events cannot be tunneled.
    Use cases that need a local tier must account for this in their deployment topology."
  - "The capability_band constraint (default 0.10) means local and frontier models must
    have similar quality_prior values to be priced against each other."

  These constraints belong in a shared document (e.g., use-cases/README.md or a new
  use-cases/CONSTRAINTS.md) once more than one use case confirms them. Record them here
  first; promote them when a second use case hits the same constraint. -->
