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
- The owner requests a bandit with serving strategies and background evaluation arms, including online TypeSafe/Jev. The ruling addenda of 2026-09-19 record this direction. C4 catalog rejection and T6 are accepted. PR structure, C6 timing, and C3 fleet fail-open behavior still await answers. The bandit reward and promotion criteria remain open.
- `PLAN-routing-strategy-bandit.md` contains the proposed runtime contracts, measurement requirements, and six delivery milestones. Claude ACP works with `acceptEdits` and the exposed `opus[1m]` alias. The host disables `bypassPermissions`, which caused the initial startup errors. `/tmp/roundhouse-phone-accept-edits.py` is a temporary copy of the phone-a-friend runner with only that mode preference changed. No workflow substitution is needed.
- The live C2 prerequisite check failed: `openv true` reports no configured 1Password CLI account. No provider request was sent.
- The PR is a draft. Its body is in the PR. `gh pr edit` is broken in this container. Update the body with `gh api -X PATCH repos/ryanolson/roundhouse/pulls/18 --input body.json`.

## Decisions that need the owner

Ask before you act on any of these. Offer the options below. Do not assume an answer.

1. **T2, the classifier on the turn path.** Ruling R3 in `agent-docs/PLAN-frontier-selection.md` says routing guidance is "never a runtime dependency". T2 proposes an amendment: a classifier may supply a tier distribution as data, before `choose`, under its own deadline, with fail-open to the default tier. Options: (a) accept T2 and build C7 as a shadow first, (b) keep R3 and use a classifier offline only, as a labeler that calibrates the `pick_tier` thresholds, (c) defer. Recommendation: (b) until a shadow period has numbers, because (b) needs no egress ruling.
2. **T6, content egress.** No ruling governs sending prompt content to a third party that is not the destination model. T6 proposes: opt-in by configuration, off by default, never for a session whose policy admits local targets only, digest format equals the judge brief. Options: accept as written, narrow it further, or reject and keep classifiers local. This decision is independent of TypeSafe.
3. **The PR split.** The branch holds documents plus four rungs. `CLAUDE.md` asks for one concern per PR. Options: (a) merge PR #18 as one unit, (b) split into per-rung PRs from `main` with the commit lists in the plan, section 2, item 6. If (b), put C2 before C3 to avoid conflicts in shared test literals.
4. **C4, the form of the price guard.** `ProviderPricing` has one write rate. A 1-hour TTL is billed at 2 times the input price and a 5-minute TTL at 1.25 times. Options: (a) the catalog boundary refuses a spec that declares a 1-hour TTL without the 2 times write rate, (b) the catalog accepts it and logs a warning, (c) `ProviderPricing` gains a second write rate. Recommendation: (a). A wrong write rate makes the savings dashboard lie, and the dashboard is the number the product is judged by.
5. **C6, the return-trip cost.** Not designed. The question is whether the C1 guard also prices the cost of the old target going cold before the session returns to it. This needs the observed cache deadline per target. Options: design it now, or wait for live data from C2. Recommendation: wait until C2 has one live measurement.
6. **C3, a fail-open arm for the fleet quote.** On a turn where the call is made, a fleet error still fails the turn. That is unchanged on purpose. Ask whether a sub-budget and a fail-open arm for the quote are wanted. If yes, the judge seam (`judge.rs`, `deadline_fraction`) is the template.

## Work that remains, in order

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
