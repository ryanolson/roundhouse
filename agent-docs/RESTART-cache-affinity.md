<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Restart prompt: extend PR #18 (cache affinity and the TypeSafe ruling)

Paste everything below the line into a new session that runs in this repository. Written 2026-09-18 at `a3f8d00`. When the branch moves, update the "State" section before you paste.

---

You are continuing PR #18 in `ryanolson/roundhouse`: https://github.com/ryanolson/roundhouse/pull/18. The branch is `ai/typesafe-roundhouse-routing-6016d1`. The worktree is `/home/ryan/repos/roundhouse/.claude/worktrees/typesafe-roundhouse-routing-6016d1`. Run every command from that directory. Do not `cd` to the main checkout.

## Read first, in this order

1. `CLAUDE.md` at the repository root. Its rules on bounded test runs, test-first validation, and comment style are binding.
2. `agent-docs/synergies/typesafe-selector-and-cache-affinity.md`. This is the ruling: T1 to T8 and two dated addenda.
3. `agent-docs/PLAN-cache-affinity.md`. Section 1 is the status table. Section 2 says how to continue. Section 3 holds the settled design brief for each rung, C1 to C7.
4. `agent-docs/research/typesafe-jev-primary-read.md`, only if a question about TypeSafe itself comes up.

## State

- `git fetch origin` first. Trust only pushed state. The 2026-09-19 continuation started from the clean, pushed commit `29665d8`. A separate worktree holds the continuation at `/home/ryan/repos/roundhouse/.claude/worktrees/pr18-cache-affinity-continuation`, on branch `codex/pr18-cache-affinity-continuation`. The PR branch remains `ai/typesafe-roundhouse-routing-6016d1`. Check both heads before further work.
- Built, mutation-checked, and pushed: C1 (dominance guard on `Efficient` picks), C2 (second Anthropic breakpoint), C3 (the fleet residency call runs only when its answer can matter), C5 (local TTFT slope, mechanism only).
- The full workspace suite passed at `29665d8` on 2026-09-19: 1652 passed, 0 failed, 141 ignored. It covered 104 test binaries and 7 doc-test suites. Command: `ulimit -Sn 65536 && timeout 900 cargo test --workspace`. The inherited descriptor limit of 1024 caused `Too many open files` in `embedded_selection`. That binary passed all 7 tests with the raised limit before the full rerun.
- The owner requests a bandit with serving strategies and background evaluation arms, including online TypeSafe/Jev. The ruling addenda of 2026-09-19 record this direction. C4 catalog rejection and T6 are accepted. PR #18 stays one unit. C6 timing and C3 fleet fail-open behavior still await answers. The bandit reward and promotion criteria remain open.
- `PLAN-routing-strategy-bandit.md` contains the proposed runtime contracts, measurement requirements, and six delivery milestones. Claude ACP works with `acceptEdits` and the exposed `opus[1m]` alias. The host disables `bypassPermissions`, which caused the initial startup errors. `/tmp/roundhouse-phone-accept-edits.py` is a temporary copy of the phone-a-friend runner with only that mode preference changed. No workflow substitution is needed.
- C5 loader is committed at `3b21e98`, with failing-first catalog and engine tests. Independent mutations of the slope, base, and negative-value guard were caught. The source was restored and 25 affected tests passed clean. Defaults remain 60 ms and 0.0. No measured deployment slope or production local fleet is supplied by this change.
- C4 has a partial checkpoint: TTL propagation, one-hour wire markers, and the catalog price guard pass their tests. The full run passed 1670 tests with 141 ignored before the mixed-tool regression. That later regression failed on `[300, 3600, 3600]` and is explicitly ignored pending the owner's normalize-or-reject decision. Remove its ignore before implementing that decision. Do not call C4 complete or deploy it with shorter tool markers. The README records this limit.
- B1 first-output measurement is committed at `765daf2`. The JSON field records its event basis, sample count, mean, and rejected timestamps. Tests cover replay, failed streamed responses, empty output, and scopes. A confirmed cross-session collision also required scoping the supersession key by session and turn. The final targeted gate passed 53 metrics tests; broader checks passed 417 core tests and 404 server checks, with 5 pre-existing server ignores. Independent mutations and a full workspace rerun are next. B1's completion and strategy-outcome observations remain open.
- Verification at `1142348`: 10 C4/B1 mutations failed the intended tests. One additional mutation exposed a missing timing-cleanup assertion. That guard is committed in `1142348` and caught the mutation on recheck. All source was restored. The full workspace suite passed 1684 tests, with 0 failures and 142 ignores, across 106 test binaries and 7 doc-test suites. The extra ignore belongs to C4's unresolved tool-marker policy. Logs: `/tmp/roundhouse-c4-b1-workspace.log` and `/tmp/roundhouse-c4-b1-refute-*.log`.
- The live C2 prerequisite check failed: `openv true` reports no configured 1Password CLI account. No provider request was sent.
- C2 probe checkpoint: `101eb59` adds the shared two-turn driver, offline controls, and gated live entry. Commit `d582e62` asserts that the received request contains at least 20 appended items. The first mutation pass exposed that missing assertion: 19 new items still produce two markers because the first response adds a block. The restored probe passes all 10 offline tests, and the live entry compiles without execution. Live inputs remain pending.
- C2 verification at `d582e62`: all seven probe mutations were caught, including the short-append mutation against its committed guard. All tracked source was restored. The full workspace suite passed 1694 tests, with 0 failures and 142 ignores, across 107 test binaries and 7 doc-test suites. Logs: `/tmp/roundhouse-c2-workspace-final.log` and `/tmp/roundhouse-c2-refute-*.log`. The C4 owner-decision ignore remains.
- B4 now has a standalone System One transport and a budgeted server adapter. It defaults disabled, checks admitted frontier availability, and builds a bounded judge brief internally. Its prepared bytes are shared by the quote and HTTP send. The binary does not construct it. B2/B3 still own durable identity, scheduling, replay, and duplicate-call prevention. Focused checks pass: 14 fleet unit tests, 8 HTTP tests, and 17 server tests. Independent post-commit mutations and the next full workspace run remain. Logs: `/tmp/roundhouse-b4-initial-red-evidence.log`, `/tmp/roundhouse-b4-estimate-*.log`, `/tmp/roundhouse-b4-urlprivacy-*.log`, and `/tmp/roundhouse-b4-final-green.log`.
- The PR is a draft. Its body is in the PR. `gh pr edit` is broken in this container. Update the body with `gh api -X PATCH repos/ryanolson/roundhouse/pulls/18 --input body.json`.

## Owner decisions

Settled on 2026-09-19. Do not ask for these decisions again:

1. T2 includes both serving strategies and background evaluation arms, with online TypeSafe/Jev arms.
2. T6 is accepted: explicit opt-in, off by default, no calls for local-only sessions, and only the bounded judge brief.
3. PR #18 stays one unit, with separate logical commits.
4. C4 rejects a one-hour catalog entry unless its write rate equals twice its input rate.

Questions already sent and still awaiting answers:

1. C4 tool markers: normalize existing marker TTLs to the target setting, or preserve them and reject conflicting requests. Normalization is the recommendation. Neither answer is assumed.
2. C6: design the return-trip cost now, or wait for live C2 cache evidence. Waiting is the recommendation.
3. C3: retain fleet-error failure, or add a sub-budget and fail-open path to admitted frontier targets.
4. Bandit objective: quality and latency constraints followed by cost minimization, a weighted score, or fixed allocation until evidence exists. Deployment must supply the limits.
5. B2 cadence: one strategy per cache segment, or one selection per turn. The proposed segment boundaries are session start, fork, and current-target cache loss. Failover retains the assignment.
6. Live C2 inputs: catalog path, pinned model, approved USD spend cap, and restored `openv` access. No live run starts without these inputs.

The segment boundary cases and promotion evidence also need a settled brief before serving allocation changes.

## Original work inventory

Use the State section for completed work and current blockers. The C5 loader is complete. C4 has a partial checkpoint at `4405da3`.

Each item has its tests-first list in the plan. Write the tests, watch them fail, then write the fix. Mark nothing `#[ignore]` unless a fix is a design question that waits for the owner.

1. **C4, the 1-hour TTL as one per-target setting.** Source of truth is `FrontierModelSpec.cache_model` (`CacheModel::Deterministic { ttl_ms }`). Carry `cache_ttl_ms: Option<u64>` on `FrontierQuote`. In `body()`, call `CacheControl::ephemeral_for("1h")` when the value is `3_600_000`. Add the price guard from decision 4. Tests first: `a_one_hour_cache_model_requests_the_one_hour_ttl`, `a_five_minute_cache_model_keeps_the_default_marker`, `the_judge_quote_still_carries_no_cache_control`, `a_one_hour_cache_model_requires_the_one_hour_write_rate`. Do not add a second TTL setting anywhere.
2. **C5, the configuration loader.** `main.rs` builds `EngineConfig` from defaults and sets only `arm_salt`, so `local_ttft_ms_per_prefill_token` and `local_base_ttft_ms` are not settable in a deployment. Find how other deployment settings reach `main.rs` (the directory, the catalog config) and add both fields the same way. The slope value must come from a measured prefill rate. Do not guess a default other than `0.0`.
3. **C2, live evidence.** One real session against Anthropic with a key from `openv`, spend capped by R9. Send a request, append 20 or more items, send again to the same target, and read `cache_read_input_tokens` from the `ResponseCompleted` event. Record the numbers as a dated bracketed note in the ruling, section 5, gap 2. If the read is zero, the block arithmetic in the C2 test is wrong about the provider, and that is the more important result.
4. **C6.** After decision 5.
5. **C7, the shadow classifier.** Only after decisions 1 and 2. The shadow records a tier distribution next to the `pick_tier` answer at each segment start and affects no route. The promotion test is in ruling T5.
6. **Smaller items with no rung.** `metadata.user_id` is not forwarded to Anthropic. `CacheLedger::invalidate` has no production caller and a prefix hash per target would remove the need for it. The render into one user message sends tool calls and thinking signatures as text, which is a function question, not a cache question.

## How to work here

- Run each cargo test command under a coreutils timeout: `timeout 300` for one crate or one test binary, `timeout 900` for the workspace. A PreToolUse hook enforces this and also greps heredocs, so write any file that mentions a test command with the Write tool, not with a shell heredoc.
- Run cargo commands one at a time. The box has four cores and one build lock.
- Commit before any mutation check. Mutate with `sed`, run the suite, restore with `sed`, and confirm with `git diff --quiet crates/`. Never use `git checkout --` or `git stash` to restore.
- Commit with `--no-gpg-sign`. Push with `GIT_CONFIG_GLOBAL=/dev/null git -c credential.helper='!gh auth git-credential' push https://github.com/ryanolson/roundhouse.git ai/typesafe-roundhouse-routing-6016d1`. The global config rewrites HTTPS to SSH, and the SSH agent does not answer headlessly.
- Never set or read a GitHub token. `gh` is the only credential.
- No commit message, PR title, or PR body names the assistant. No `Co-Authored-By` line.
- Write documents, commit messages, and PR text with the `simple-english` skill. No hard-wrapped Markdown.
- Subagents: Opus implements a rung from its settled brief. Sonnet refutes by mutation and writes tests for characterized behavior. The orchestrator gates, commits, and writes the documents. An adversarial stage never runs the model that produced the claim.
- Before a PR asks for human review, run `wills-mega-review` on it.

## When you stop

1. Update the status table in `agent-docs/PLAN-cache-affinity.md`, section 1, with the commit for each rung you finished and the result of the last full suite.
2. Add a dated addendum to the ruling for anything that changed a ruling.
3. Update the "State" section of this file.
4. Commit, push, and confirm that `git rev-parse --short HEAD` equals the remote head.
5. Update the PR body through `gh api`.
