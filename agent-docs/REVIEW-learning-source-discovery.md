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

## Independent gate, 2026-09-23

A Sonnet verifier that did not write the code mutated the committed source at `a95da90`. Its `crates/` bytes equal `6a9eb07`. It applied one mutation at a time and restored exact bytes after each one, checking hashes against `HEAD`. The parent added two more mutations for the examined-member bound.

| Contract | Memory | Redis |
|---|---|---|
| Mark uses the assigned sequence, not the batch tail | caught | caught |
| Lease fence blocks index writes | caught | caught |
| Key-type preflight before writes | n/a | caught |
| Sequence-range preflight refuses the whole batch | n/a | caught |
| Newer mark survives a stale clear | caught | caught |
| Requeue needs the current sequence | caught | caught |
| Covering clear keeps permanent membership | caught | caught |
| Project mismatch refuses before writes | caught | caught |
| Cursor advances across empty filtered pages | caught | caught |
| Page bounds examined members before filtering (parent) | caught | caught |

All 18 mutations failed runtime assertions. None survived, so no assertion was added. After restoration, 22 memory, 26 shared Redis, and 6 Redis index tests passed with none skipped, and formatting passed.

Full workspace suite at `a95da90`, with `ulimit -n 65536` and a 900-second timeout around the workspace test command: 2183 passed, 0 failed, 165 ignored, across 138 suites. The ignore count grew from 149 because the new Redis tests are gated.

The ignored tests were then run on their own (`-- --ignored`) with a task-owned Redis: 163 passed and 2 failed. Both failures are in `review_m14_1_f7`, which has not changed since `main`. On the first run, `redis-cli` was not on `PATH`. With a shim, the binary passes 3 of 3 runs under `--test-threads=1` and fails 3 of 3 runs in parallel. Two of its tests change the global `maxmemory` of the shared Redis, so one test's forced out-of-memory window overlaps the other test's retry. This is a test-isolation defect on `main`, not in this PR. It is recorded as a follow-up.

Local logs: `/tmp/roundhouse-pr18-refute-report.md` and `/tmp/roundhouse-pr18-refute-*.log`.
