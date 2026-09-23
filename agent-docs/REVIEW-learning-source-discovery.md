# Learning source discovery verification

## Scope, 2026-09-22

This checkpoint implements the source-store portion of L3b in `DRAFT-online-routing-learner.md`, sections 21 and 22. Marked appends record learning discovery atomically with source events. Sequence checks protect pending membership from stale acknowledgements. Permanent membership supports later audits. Bounded pagination advances past filtered sessions.

Ordinary session appends remain unmarked. Event projection, learner-store delivery, recovery workers, learned selection, and live evaluation remain unfinished.

## Test-first evidence

Inert implementations compiled before the new tests ran. Ten memory contracts and ten Redis contracts failed runtime assertions. The existing memory and Redis contract controls passed. Redis-specific tests also exposed a partial-write defect at the existing sequence limit.

The range defect is valid: a refused two-event batch left its first event durable. The shared preflight now rejects the complete batch before writing. Marked and unmarked regression cases pass, and a control reaches the last supported sequence. The numeric domain is unchanged.

## Verification before the checkpoint

| Check | Result |
|---|---|
| Parent memory-store run | 22 passed |
| Parent Redis shared contracts and index regressions | 32 passed, none skipped |
| Worker complete Redis crate, including gated tests | 177 passed, none skipped |
| Worker complete core crate | 605 passed |
| Worker affected server binaries | 597 passed, 7 existing ignores |
| Formatting and strict affected-crate Clippy | Passed |
| Workspace all-targets compilation | Passed |

Independent source reviews found no material implementation defect in the memory or Redis mechanisms. The memory review identified a pagination test gap. The strengthened contract now checks exact cursor progress for empty filtered pages on both backends.

Worker self-mutations are preliminary evidence only. Independent post-commit mutation checks and the full workspace test gate remain pending. No full-PR readiness or live-provider result is claimed.

Local evidence: `/tmp/roundhouse-learning-source-index-report.md`, `/tmp/roundhouse-learning-index-parent-memory.log`, `/tmp/roundhouse-learning-index-parent-redis.log`, and `/tmp/roundhouse-learning-index-redis-review-report.md`.

## Limits

Marked Redis appends require one Redis replication unit and do not support Redis Cluster. The permanent index has no compaction protocol. The store preserves the first recorded project binding; the caller must derive that binding from the session principal. Recovery must use bounded page sizes and continue through empty pages that carry a cursor.
