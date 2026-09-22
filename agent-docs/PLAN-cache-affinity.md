<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Plan: cache affinity, from the selector to the wire

> **Status: in progress, 2026-09-17.** The ruling is `synergies/typesafe-selector-and-cache-affinity.md`. The owner accepted the order of work in its addendum of 2026-09-17. This plan holds the design brief for each rung, so that a new session can continue without a second design pass. The status table in section 1 is the first thing to read and the last thing to update.

## 1. Status

**Owner ruling, 2026-09-21.** Roundhouse owns cache markers and can inject, modify, or remove them under provider rules. C4 now normalizes tool markers to the target TTL, with tests and mutation evidence in `1280855`. The owner also requires per-turn local routing with asynchronous Jev classifications as sequence metadata. The dated addendum in `PLAN-routing-strategy-bandit.md` supersedes the earlier segment-allocation proposal.

Branch: `ai/typesafe-roundhouse-routing-6016d1`. Last update: 2026-09-19. The latest local full run covers `9c56c18`: 1754 passed, 0 failed, 142 ignored, across 108 test binaries and 7 doc-test suites.

| Rung | What | Status | Commit |
|---|---|---|---|
| Evidence | Two ignored claim tests with live controls | done | `35839e2` |
| Documents | The TypeSafe read, the ruling, the addendum, this plan | done | `e48b13b`, `be2bd66` |
| C1 | Dominance guard on `Efficient` picks (T4) | done, mutation-checked | `83ac785` |
| C2 | Second Anthropic breakpoint | mechanism and probe mutation-checked. Live evidence is still necessary. | `22e58ce`, `d7f69ce`, `101eb59`, `d582e62` |
| C3 | The Dynamo residency call becomes a decision | done, mutation-checked | `8817db0` |
| C4 | 1-hour TTL as one per-target setting | Built and mutation-checked, including tool TTL normalization. The owner-decision ignore is removed. | `4405da3`, `1280855` |
| C5 | Local TTFT quote reads the residency answer | mechanism and catalog loader done, mutation-checked. Measured deployment slope still needed. | `6e905ba`, `3b21e98` |
| C6 | Observed cache deadline and the return trip | not designed | |
| C7 | Shadow classifier (T5) | B4 standalone adapter built and mutation-checked under T6. Runtime records, scheduling, and bandit allocation remain open. | `51b0fb2` |

**Cache observations, 2026-09-21:** Commit `330cacb` adds predicted-versus-observed cache reuse to the metrics JSON. Only provider-reported counts supply observed samples, including explicit zero. Commit `a4766a1` fixes aggregation provenance and closes the local-path test gap found by mutation. The final workspace run passed 1784 tests, with 0 failures, 141 existing ignores, and no compiler warnings. Mutation evidence and restoration checks are in `PLAN-routing-strategy-bandit.md`. This does not supply live C2 evidence or complete C6/C7.

Open items that belong to a finished rung:

- **C4 checkpoint, 2026-09-19.** The engine carries the target TTL into `FrontierQuote`. Messages bodies select one-hour conversation markers, and the catalog rejects a mismatched write rate, including Messages gateways. Fail-first wire, catalog, and engine tests passed after the change. The full workspace run passed 1670 tests with 141 ignored across 106 test binaries and 7 doc-test suites. A later body regression failed with tool/message TTLs `[300, 3600, 3600]`. That regression is now explicitly ignored pending the owner's normalize-or-reject decision. An ignored test enforces nothing. The subsequent fleet run passed with that one additional ignore. C4 is not ready for use with shorter tool markers.
- **C4 verification, 2026-09-19.** Independent mutations of engine TTL propagation, one-hour wire selection, the catalog guard, and gateway handling all failed their intended tests. Controls passed. Source restoration was byte-checked after each mutation. This verifies the checkpoint and does not close the ignored mixed-marker regression.

- **C1.** Closed in `4bffdcb`. `crates/roundhouse-server/tests/handoff_escalation.rs` had a table of six ways that a turn gets or does not get a handoff note. A turn that the cost guard redirects is the seventh way. The test `a_cost_guarded_turn_narrates_nothing` proves it through the engine with a priced catalog and a warm `Capable` target. With the guard disabled, that test goes red. This commit landed after the last full workspace run. Its gate was the `handoff_escalation` binary: 8 passed, 0 failed.
- **C1.** A `Dimensions` pick of the `Efficient` tier is not reachable at the shipped threshold, because `production_intensity` has a maximum of `tanh(0.5)`. The guard test for a pick that is not `TestsPassed` uses the `Ambiguous` default.
- **C3.** Both skips shipped: `tools_declared` and `policy_admits_no_local`. A skipped quote leaves no local candidate to count, so the audit note for the tool exclusion and the `NoToolCapableTarget` refusal now read `local_withheld_by_tools`. The plan did not predict this. The test `messages_api_surface::f2_...` found it.
- **C3.** On a turn where the call is made, a fleet error still fails the turn. That is unchanged on purpose. A sub-budget and a fail-open arm for the quote are a separate decision.
- **C5.** Loader added in `3b21e98`: the catalog supplies both local TTFT fields, and `main.rs` carries them into the engine. The defaults remain 60 ms and 0.0. Tests failed before wiring: the local quote was 60 ms instead of the configured 842 ms. Catalog, engine integration, and affected routing tests now pass. A real slope still needs a prefill measurement. The production binary does not attach a local fleet, so this loader does not itself enable local inference. Independent mutations of the slope, base, and negative-value guard each failed the intended tests. The source was restored after each mutation, and 25 affected tests passed clean. The integration tests cover the library configuration-to-quote path, not the binary startup path.
- **C2.** No live request exists yet that shows `cache_read_input_tokens > 0` after an append of 20 or more items.
- **All.** The full workspace suite ran under `timeout 900` two times. At `6ab8908` (C1 to C3): 111 test binaries, 1649 passed, 0 failed. At `6e905ba` (C5 added): 111 test binaries, 1651 passed, 0 failed. No ignore from this work remains in `crates/`.
- **Baseline, 2026-09-19.** At `29665d8`, `ulimit -Sn 65536 && timeout 900 cargo test --workspace` passed: 1652 passed, 0 failed, 141 ignored. The run covered 104 test binaries and 7 doc-test suites. The initial run failed in `embedded_selection` with `Too many open files` at the inherited soft limit of 1024. With the raised limit, that binary passed all 7 tests before the full rerun. No source changes were necessary.

Decisions that only the owner can make:

1. T2: the owner requests a bandit with serving strategies and background evaluation arms, including online TypeSafe/Jev. See the ruling addendum of 2026-09-19. The runtime contract still needs a settled brief before implementation.
2. T6 is accepted as proposed on 2026-09-19. The owner keeps PR #18 as one unit. The remaining cache decisions are C6 timing and C3 fleet fail-open behavior.

The mutation checks used `sed` to change one token, ran the suite, and used `sed` to restore the token. `git diff --quiet crates/` confirmed each restore. C1: `<` to `<=` turned `a_tie_in_quoted_cost_keeps_the_efficient_pick_and_its_source` red. C2: `CACHE_LOOKBACK_BLOCKS` from 20 to 21 turned the evidence test red. C3: a predicate that never skips turned four of the five tests in `tests/local_quote_skip.rs` red, and the control stayed green. The C3 stage wrote its tests before the fix but did not run them before the fix, so this mutation is the evidence that they fail without it.

**C2 probe verification, 2026-09-19.** The full workspace suite at `d582e62` passed 1694 tests, with 0 failures and 142 ignores, across 107 test binaries and 7 doc-test suites. Seven independent probe mutations failed: missing second request, short append, missing run nonce, bypassed budget, ignored route, swapped counters, and disabled cap preflight. The short-append mutation first exposed a missing assertion. Commit `d582e62` adds that guard, and the committed recheck failed on 19 received items. All mutations were restored. The probe passes 10 offline tests, and its gated live entry compiles without execution. Logs: `/tmp/roundhouse-c2-workspace-final.log` and `/tmp/roundhouse-c2-refute-*.log`. No live cache evidence is claimed.

## 2. How to continue

1. Read `CLAUDE.md`, then the ruling and its addendum, then this plan.
2. Run `git fetch origin` and compare the branch in the status table with `origin`. Trust only pushed state.
3. Complete the C4 tool-marker decision and its regression first. C5's loader is complete. Live C2 evidence needs working credentials, and C6 timing awaits the owner. Independent bandit observation work can proceed under R21. Serving allocation still needs the decisions in `PLAN-routing-strategy-bandit.md`. Each implementation starts with failing tests.
4. Run cargo commands one at a time. The box has four cores and one build lock. Put `timeout 300` before a targeted test command and `timeout 900` before a workspace test command.
5. Commit before any mutation stage. Commit with `--no-gpg-sign`. Push through the `gh` credential helper. `~/.gitconfig` rewrites `https://github.com/` to SSH, so put `GIT_CONFIG_GLOBAL=/dev/null` before the push command when the SSH agent does not answer.
6. The owner chose to keep PR #18 as one unit on 2026-09-19. Keep each concern in a separate commit. The earlier split inventory remains useful for reviewing the existing rungs:
   - Documents: `e48b13b`, `be2bd66`, `6ab8908`, and each later commit that changes only `agent-docs/`.
   - C1: `35839e2` (the `stage.rs` part), `83ac785`, and `4bffdcb`.
   - C2: `35839e2` (the `anthropic_messages.rs` part), `22e58ce`, and `d7f69ce`.
   - C3: `8817db0`.
   - C5: `6e905ba`. It has no overlap with C1 to C3 outside `engine.rs`.
   - C2 and C3 both add a field to literals in shared test files, so put C2 before C3 or expect small conflicts.
7. Before a PR asks for human review, run `wills-mega-review` on it. Write the PR text with the `simple-english` skill. No PR text and no commit message names the assistant.

## 3. The rungs

### C1 — the dominance guard on `Efficient` picks (T4)

**Finding.** With a tier recipe, `StagePolicy::choose` selects a tier from `TurnSignals` and serves the first admitted member of that tier in recipe order. It never reads a quote. A `TestsPassed` de-escalation moved a warm $0.03 target to a cold $0.12 target in the evidence test.

**Rule.** When the picked tier is `Efficient`, compare the head of that tier with the admitted `Capable` pool in recipe order. If a `Capable` member quotes strictly less for the turn, serve the first such member and record the decision source `cost_guard`. The rationale names both targets and no price. Both quotes are in the `considered` list of the `DecisionRecord`. A `Capable` pick is never redirected by cost. A tie keeps the tier pick. The policy holds no session state.

**Tests first.** The evidence test `a_deescalation_does_not_move_a_warm_session_onto_a_costlier_cold_target` in `routing/stage.rs` loses its ignore. Add: a tie keeps the pick, a cheaper `Capable` target that is not admitted does not fire the guard, the guard fires on a non-`TestsPassed` pick, the source and rationale name both quotes, and a severity override is never redirected.

**Not in this rung.** The guard does not price the return trip to a target that goes cold. That needs the observed cache deadline (C6).

### C2 — the second Anthropic breakpoint

**Finding.** `AnthropicMessagesClient::body` places one `cache_control` marker on the penultimate segment. The provider examines 20 block positions back from a marker. An append of 20 or more items since the last request to the same target reads nothing from the cache. The evidence test is `a_long_append_keeps_the_previous_cache_write_inside_a_lookback_window` in `anthropic_messages.rs`.

**Facts that the design rests on.**

- Segments equal items. `rendered_with_boundaries` (`context.rs:251-260`) gives `n - 1` interior offsets for `n` items. `engine.rs:2736-2737` fills the dispatch quote from it.
- The log order in one turn is: `TurnStarted` and the input `ItemAppended` events, then `Routed`, then the output items, then `ResponseCompleted` (`session.rs:1304-1400`). So the item count at dispatch equals `items.len()` at the `Routed` fold, and not at the terminal fold.
- `PendingRouting` (`session.rs:618-627`) already carries data from `Routed` to `CacheLedger::record` (`session.rs:651-653`, `ledger.rs:385-394`).
- The handoff note goes on the prompt after the render (`engine.rs:2765-2770`). It lands inside the last segment and moves no interior offset. It is not in the log, and it is after every marker, so it is not a cache defect.
- The ledger is a projection of the log. The new field derives on replay. No event changes.

**Steps.**

1. Add `pub last_segment_count: u64` to `TargetState` (`ledger.rs:340`) with `#[serde(default)]`.
2. Add a `segment_count` parameter to `CacheLedger::record`. The one production caller is `session.rs:651`.
3. Add `segment_count: u64` to `PendingRouting`. Fill it in the `Routed` arm from `self.items.len()`.
4. Add `pub previous_breakpoint: Option<usize>` to `FrontierQuote` (`frontier.rs:422`). At `engine.rs:2737` derive it as `state_for(target)` then `last_segment_count.checked_sub(2)`. The judge (`judge.rs:592`) and `main.rs:1328` pass `None`.
5. In `body()` at `anthropic_messages.rs:397`, keep `previous` only if it is `Some(p)`, `p < segments.len() - 1`, `p != penultimate`, and `penultimate - p > 19`. If `riding + 2 <= MAX_CACHE_BREAKPOINTS`, place both markers. If only one slot is free, place the penultimate marker only. If no slot is free, place none.
6. Extend the existing comment about the yield to client tool markers. Do not replace it.

**Why the penultimate marker wins the last slot.** A lone marker at `previous` reads this turn but never moves the entry forward, so each later turn pays plain input on a longer tail. A lone penultimate marker pays one write and makes each later turn a hit.

**Churn.** About 35 `FrontierQuote {` literals in 10 files. Most fleet tests use `..quote(...)`, so the helper covers them. Three production literals and a few full literals in tests need the field.

**Tests first.**

1. Remove the ignore from the evidence test. Its control stays live.
2. `a_request_with_the_client_holding_four_tool_markers_still_sends_none`.
3. `a_request_with_three_riding_markers_keeps_the_penultimate_and_drops_the_previous`.
4. `a_short_append_adds_no_second_marker`.
5. `the_judge_quote_still_carries_no_cache_control`.
6. `the_ledger_records_the_item_count_the_turn_was_dispatched_with`, at the fold level in `session.rs`.
7. `a_replayed_log_reconstructs_the_same_last_segment_count`.

**Live evidence that is still necessary.** The tests prove the block arithmetic. A real session with a long append must show `cache_read_input_tokens > 0` on the next turn. Spend is capped by R9.

**Probe brief, 2026-09-19.** One shared harness drives two turns through the engine, a configured project budget, stored credentials, and the Messages client. The loop has a two-request maximum. A loopback server and the live endpoint use the same turn-driving code. The second turn appends at least 20 items. Offline tests inspect the received breakpoint positions, both usage events, and a zero-budget refusal that sends no request.

The live test uses a non-default `e2e-frontier` feature and mandatory preflight. It requires the operator's catalog, pinned model, spend cap, and stored key. No model, rate card, or cap has a guessed default. The latest handoff prohibits new ignores outside owner-design questions, so this probe uses no `#[ignore]`. This supersedes R9's earlier ignore pattern for this probe.

The report records `ResponseCompleted.usage.cached_input_tokens` and `cache_write_tokens` for both turns. These correspond to the provider's `cache_read_input_tokens` and `cache_creation_input_tokens`. A zero second-turn read requires investigation. Without a first-turn read or write, a second-turn miss cannot establish a lookback failure. A second-turn read does not establish the entry's origin. The live prerequisite remains blocked because `openv` reports no configured 1Password CLI account.

### C3 — the Dynamo residency call becomes a decision

**Finding.** `fleet.price()` is a realtime residency check. It sends block and sequence hashes and gets back `effective_prefill_tokens` and `longest_matched_tokens` (`local.rs:50-68, 150-159`). It runs on every turn when a fleet is configured (`engine.rs:2021-2038`). It runs before the tool exclusion at `engine.rs:2078-2085`, so each tool turn makes the HTTP call and then discards the answer. Its only bound is the whole-turn deadline of 120 s. A fleet error fails the turn even when the route was always a frontier target.

**Steps.**

1. Add a pure function `local_quote_can_matter` next to `plan` in `engine.rs`. It is true only if a fleet is configured, the turn declares no tools, and the turn policy admits some local target. Budget and cadence are not skip reasons, because a budget squeeze makes a local route more likely.
2. Gate the `fleet.price` block on it.
3. Add `local_quote_skipped: Option<&'static str>` to `DecisionRecord` (`routing/mod.rs:547`) with `#[serde(default)]`, the convention in `event.rs:56-79`. The dashboard must tell "not quoted" from "quoted and rejected".

**Tests first.** `a_tool_declaring_turn_makes_no_fleet_call` with a counting fleet stub, `a_fleet_error_on_a_tool_turn_does_not_fail_the_turn`, `a_turn_whose_policy_admits_no_local_target_makes_no_fleet_call`, and `a_skipped_local_quote_is_named_in_the_decision_record`.

**Open for the owner.** Dynamo can keep a KV cache for hours. A later form of this decision can also skip the call when the ledger shows a recent local dispatch with a full match and the fleet reports no eviction. That needs an eviction signal from Dynamo, and none is wired today.

### C4 — the 1-hour TTL as one per-target setting

**Rule.** Do not add a second setting. `FrontierModelSpec.cache_model` already holds `CacheModel::Deterministic { ttl_ms }` (`frontier.rs:49, 126`). Carry `cache_ttl_ms: Option<u64>` on `FrontierQuote` from the same field. `body()` calls `CacheControl::ephemeral_for("1h")` when the value is `3_600_000`. The ledger and the wire then read one field, so they cannot disagree.

**The price guard.** `ProviderPricing` has one write rate, `cache_write_per_mtok_usd` (`ledger.rs:96-104`). The provider bills a 1-hour write at 2 times the input price and a 5-minute write at 1.25 times. The catalog boundary (`catalog_config.rs`) must refuse a spec that declares a 1-hour TTL without the 2 times write rate. Test: `a_one_hour_cache_model_requires_the_one_hour_write_rate`.

### C5 — the local TTFT quote reads the residency answer

**Steps.** Add `pub local_ttft_ms_per_prefill_token: f64` to `EngineConfig` (`engine.rs:463`) with default `0.0`, documented next to `local_base_ttft_ms`. `LocalQuote::to_candidate` (`local.rs:174-186`) returns `base + effective_prefill_tokens * per_token`, the mirror of `frontier.rs:191-193`. The call site is `engine.rs:2042-2044`.

**Tests first.** `a_local_candidate_ttft_rises_with_effective_prefill_tokens` and the control `a_zero_slope_reproduces_the_flat_quote`. The default of `0.0` keeps the tests that pin the flat value green: `mcp_surface.rs:166, 192`, `budget_routing.rs:432`, `credential_gating.rs:298`, `policy_routing.rs:181`.

### C6 — the observed cache deadline, and the return trip

Not designed yet. The input is the time of the last dispatch to each target plus the TTL that the request asked for. The use is a term in the C1 guard for the cost of a return to a target that goes cold. Do C4 first, because the requested TTL comes from it.

### C7 — the shadow classifier (T5)

Blocked on the owner. It needs a ruling on T2 (the amendment to R3) and on T6 (the egress posture). No classifier call ships before both. The shadow records a tier distribution next to the `pick_tier` answer at each segment start and affects no route.

**Continuation, 2026-09-19.** The owner requests serving strategies and background evaluation arms, including online Jev. `PLAN-routing-strategy-bandit.md` develops that direction into proposed contracts, delivery milestones, and tests. It distinguishes live outcomes from background estimates. T6 and the reward and promotion criteria remain open.

**Later ruling, 2026-09-19.** T6 is accepted. The earlier statement that T6 remains open is superseded. Reward, promotion, and segment allocation contracts remain unsettled. A standalone shadow adapter can proceed under T6; runtime allocation must wait for its durable record contract.

**B4 checkpoint, 2026-09-19.** Commit `51b0fb2` adds the standalone transport and budgeted server adapter. Twelve independent mutations were caught, and all 39 focused tests pass after restoration. The adapter is deliberately unwired. B2/B3 own durable records, scheduling, replay, and duplicate-call prevention. No live classifier evidence or serving allocation is claimed. See `PLAN-routing-strategy-bandit.md` for the contract and verification limits.

## 4. Smaller findings with no rung yet

- `metadata.user_id` is read for the session name and is not forwarded to Anthropic. A one-assertion test on `body()` proves it.
- `CacheLedger::invalidate` has no production caller. A prefix hash for each target (section 6 of the ruling) removes the need for it.
- The render into one user message sends tool calls, tool results, and thinking signatures as text. This is a function question and needs its own measurement.
