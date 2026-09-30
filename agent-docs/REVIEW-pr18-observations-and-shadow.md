<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Scoped review: observations and shadow adapter

Review date: 2026-09-19. Reviewed source: `9abf5e3`. Scope: B1 metrics and B4 TypeSafe adapter in PR #18. This does not establish human readiness for the full PR. Runtime bandit contracts and live cache evidence remain open.

Two independent read-only passes examined the source, tests, callers, resource bounds, and module structure. Their claims were then tested before fixes. The implementing and claim-testing models differed from the reviewers.

| Claim | Ruling | Evidence and result |
|---|---|---|
| The terminal failover fixture omits emitted attempt metadata. | Valid fixture defect. | With the engine's second `Routed` shape, the existing assertion failed on `Some(0)` versus `None`. Commit `d448a4c` checks the failed target's existing row, one failed attempt, and zero timing samples. The final target retains the complete interval. No production fold change was necessary. |
| That fixture defect invalidates the earlier target-attribution mutation evidence. | Invalid. | The mutation also failed the realistic first-output fixture. The terminal fixture failed because the expected final target had no timing row under the mutation. Its row-absence assumption needed correction, but the wrong-target catch remains evidence. |
| A failed shadow settlement warning cannot identify the affected call. | Valid. | A real loopback classification with a failing ledger produced a warning without the session identifier. Commit `2686fc9` adds structured `session_id` and `hold_key` fields. The test retains the measured outcome and settlement amount and excludes transcript and credential text from the warning. |
| Equal state and request byte caps indicate broken bound composition. | Invalid as a behavior defect. | A state at its own cap exceeds the equal wire cap after JSON framing and escaping. The test observes explicit refusal, zero grants, and zero HTTP calls. Widening only the wire cap sends the same state successfully. The two limits bound different representations. Commit `2686fc9` adds this control and clarifies the comment without changing admission. |
| The first-output tests remain inline in a growing fold module. | Valid structural observation. | Commit `76a4052` moves all 12 added tests into `fold/first_output_tests.rs`. Root compared the moved region with its parent and found it byte-identical after indentation normalization. |

The settlement comments no longer claim reconciliation against a durable evaluation log. B2/B3 have not implemented that log. The warning identifies the failed call and does not prove that settlement succeeded.

Before commit, the corrected source passed 72 core metrics tests and 19 server shadow tests. Formatting and whitespace checks passed. Raw evidence is in `/tmp/roundhouse-scoped-review-r1-{red,green}.log`, `/tmp/roundhouse-scoped-review-r2-{red,green}.log`, `/tmp/roundhouse-scoped-review-r3-observed.log`, and `/tmp/roundhouse-scoped-review-root-*.log`.

Commit `9c56c18` replaces three warning-capture copies with one helper in server test support. Root compared the implementation bodies and found no change beyond comments, imports, and indentation. Caller assertions are unchanged. One serialization mutex now covers capture users within each test binary.

The mechanical gates passed 72 core metrics tests, 396 server library tests, and 21 binary tests, with 5 existing library ignores. Root repeated these gates from the continuation worktree. An initial mechanical command ran in the main checkout and was discarded as evidence. The main checkout has no tracked changes. Preservation evidence is in `/tmp/roundhouse-review-first-output-preservation.txt` and `/tmp/roundhouse-review-warning-helper-preservation.txt`.

Four independent mutations against `9c56c18` failed at runtime: removal of either warning identifier, first-target attribution, and a disabled wire-size guard. Each inverse edit restored the committed source. The corrected failover fixture and the moved first-output fixture both caught the wrong-target mutation. Raw logs, exit codes, and restoration checks are in `/tmp/roundhouse-scoped-review-refute-M1.log` through `/tmp/roundhouse-scoped-review-refute-M4.log`.

After restoration, 72 core metrics tests and 19 server shadow tests passed. Formatting and whitespace checks passed. No new ignore was added. The full local workspace suite at `9c56c18` passed 1754 tests, with 0 failures and 142 ignores, across 108 test binaries and 7 doc-test suites. Log: `/tmp/roundhouse-scoped-review-workspace.log`.

**Verdict: APPROVE for this scoped B1/B4 review.** The full PR remains a draft. This verdict does not close the owner decisions, the C4 ignored regression, runtime bandit integration, or live cache evidence.
