<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Frontier interval feedback: scoped verification

## Implementation checkpoint, 2026-09-22

A parsed frontier review records its exact routing-decision interval and prompt digest. The snapshot precedes the judge call. Failover attempts remain separate decisions. Complete verdicts provide positive or negative interval feedback independently of the delivered steering action. Missing context and incomplete coverage remain unknown.

The session fold checks continuity, membership, configuration versions, and labels. Parsed unknown reviews advance the boundary. Failed, skipped, refused, and placebo reviews do not. Tracking retains at most 64 turns and 256 decisions before marking an overflow. Oversized or incomplete intervals cannot label a retained suffix.

The fold trusts content-gap declarations recorded by the validator. It does not reconstruct the prompt or independently detect omitted content. This checkpoint supplies attribution, not a durable learning consumer or learned routing.

## Independent findings

| Finding | Ruling and evidence | Change |
|---|---|---|
| Withheld control content received positive feedback | Valid. The independent assertion expected `Unknown` and received `Positive` with no gaps. | Record `WithheldControlTraffic` and omit the interval section. |
| Routing details reached the complete judge prompt | Partially valid. The classic brief's output head already exposed the details; the new interval label made that prompt a quality observation. | Withhold recognized control-call contents in the classic brief while retaining step numbers. |
| Rejected sections allocated according to item count | Valid. Under a one-byte limit, incremental allocation grew from 1,595 bytes for 20 calls to 208,883 bytes for 2,000 calls. | Count section bytes before constructing the control-traffic lookup. |

The restored allocation test reports the same incremental allocation for both sizes. A rendered-section positive control still grows. The measurement isolates incremental allocation through the review path; it does not establish constant total review cost or elapsed time.

The independent replay review found no additional material issue. That review used source inspection. The independent contract file remained unchanged through the fixes.

## Validation before commit

The initial feature tests produced 38 runtime failures with six passing controls. The later independent contract tests produced two failures with one passing control. The allocation test produced one failure with one passing control. Additional control-pairing tests produced three failures with one passing control. These runs reached assertions, rather than compiler errors.

After the fixes, the worker passed 595 core tests and 627 affected server tests. The server run retained five existing ignores. Strict Clippy passed for core and server across all targets. The parent independently passed 42 interval tests and 82 server integration tests. Formatting and whitespace checks passed.

Evidence logs: `/tmp/roundhouse-frontier-interval-red-core.log`, `/tmp/roundhouse-frontier-interval-red-server.log`, `/tmp/roundhouse-interval-contract-check.log`, `/tmp/roundhouse-interval-fix-red-allocation.log`, `/tmp/roundhouse-interval-fix-red-control.log`, `/tmp/roundhouse-interval-fix-green-core-final.log`, `/tmp/roundhouse-interval-fix-green-server-final.log`, `/tmp/roundhouse-interval-fix-clippy-final.log`, `/tmp/roundhouse-interval-parent-core.log`, and `/tmp/roundhouse-interval-parent-server.log`.

## Remaining gates and limits

Independent post-commit mutation checks and full workspace validation remain pending. This is not a full-PR approval. No live provider, Redis outage, process-crash, or deployment evidence is claimed.

Control tools such as `declare_intent` can make an interval unknown. The filter does not remove routing details that an agent repeats in ordinary text. Some scans still depend on transcript size. These limits must remain visible when evaluating learning coverage and performance.

The full goal still requires learned per-project selection, once-only durable learning updates, offline calibration and promotion, cache prediction updates, and deployment measurements. The accepted cost, latency, project-scope, and oversized-interval decisions remain in `PLAN-routing-strategy-bandit.md`.
