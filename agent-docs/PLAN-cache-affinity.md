<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Plan: cache affinity, from the selector to the wire

> **Status: in progress, 2026-09-17.** The ruling is `synergies/typesafe-selector-and-cache-affinity.md`. The owner accepted the order of work in its addendum of 2026-09-17. This plan holds the design brief for each rung, so that a new session can continue without a second design pass. The status table in section 1 is the first thing to read and the last thing to update.

## 1. Status

**Owner ruling, 2026-09-21.** Roundhouse owns cache markers and can inject, modify, or remove them under provider rules. C4 now normalizes tool markers to the target TTL, with tests and mutation evidence in `1280855`. The owner also requires per-turn local routing with asynchronous Jev classifications as sequence metadata. The dated addendum in `PLAN-routing-strategy-bandit.md` supersedes the earlier segment-allocation proposal.

Branch: `ai/typesafe-roundhouse-routing-6016d1`. Last update: 2026-09-28. Review rounds 7 and 8 were clean. Their nit fixes are `3ca0095`, `1994b9c`, and `290656c`. At `290656c` the full workspace run passed 2233 tests, with 0 failures and 166 ignored, across 131 test binaries and seven doc-test suites. Log: `/tmp/roundhouse-round8-workspace.log`. Review round 6 fixes are committed at `35a4ab0`. They cover F1 (`7567070`), P1 (`8f5b3a2`), P3 (`35a4ab0`), and F2 to F6 (`5723732` to `50c6ba8`). Seven independent mutations of F1, P1, and P3 failed their guard tests, and each restore left the tree clean. At `35a4ab0` the full workspace run passed 2232 tests, with 0 failures and 166 ignored, across 131 test binaries and seven doc-test suites. No compiler warnings were emitted. Strict workspace Clippy and formatting passed. Log: `/tmp/roundhouse-round6-workspace.log`.

Earlier checkpoint, 2026-09-22. The runtime source at `3315820` passed the final workspace run: 2118 passed, 0 failed, and 147 ignored, across 123 test binaries and seven doc-test suites. No compiler warnings were emitted. Strict workspace Clippy and formatting passed. Scoped review fixes and independent mutation evidence are recorded in `PLAN-routing-strategy-bandit.md`. Log: `/tmp/roundhouse-runtime-publication-workspace.log`. This checkpoint does not complete frontier feedback, learned routing, live measurements, or the full-PR review gate.

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
| C7 | Background classifier (T5) | Rich classification, durable scheduling and delivery, deployment configuration, and evaluation accounting are implemented. Deadline and acknowledgement guards and indexed repair removal have focused mutation evidence. Final workspace checks passed. Frontier feedback and learned allocation remain open. | `438fe2f`, `8269e47`, `a9e7770`, `f62c621`, `3315820` |

**Cache observations, 2026-09-21:** Commit `330cacb` adds predicted-versus-observed cache reuse to the metrics JSON. Only provider-reported counts supply observed samples, including explicit zero. Commit `a4766a1` fixes aggregation provenance and closes the local-path test gap found by mutation. The final workspace run passed 1784 tests, with 0 failures, 141 existing ignores, and no compiler warnings. Mutation evidence and restoration checks are in `PLAN-routing-strategy-bandit.md`. This does not supply live C2 evidence or complete C6/C7.

Open items that belong to a finished rung:

- **Judge cache checkpoint, 2026-09-21.** Commit `e7bc859` prepares one prompt for counting and transport, marks its Messages system prefix, and requests the target TTL. Four independent mutations failed their intended assertions. The restored workspace suite passed with no compiler warnings. See `PLAN-routing-strategy-bandit.md` for evidence. This supplies cache-request mechanics, not live cache-hit evidence or complete review intervals.

- **C4 checkpoint, 2026-09-19.** The engine carries the target TTL into `FrontierQuote`. Messages bodies select one-hour conversation markers, and the catalog rejects a mismatched write rate, including Messages gateways. Fail-first wire, catalog, and engine tests passed after the change. The full workspace run passed 1670 tests with 141 ignored across 106 test binaries and 7 doc-test suites. A later body regression failed with tool/message TTLs `[300, 3600, 3600]`. That regression is now explicitly ignored pending the owner's normalize-or-reject decision. An ignored test enforces nothing. The subsequent fleet run passed with that one additional ignore. C4 is not ready for use with shorter tool markers.
- **C4 verification, 2026-09-19.** Independent mutations of engine TTL propagation, one-hour wire selection, the catalog guard, and gateway handling all failed their intended tests. Controls passed. Source restoration was byte-checked after each mutation. This verifies the checkpoint and does not close the ignored mixed-marker regression.

- **C1.** Closed in `4bffdcb`. `crates/roundhouse-server/tests/handoff_escalation.rs` had a table of six ways that a turn gets or does not get a handoff note. A turn that the cost guard redirects is the seventh way. The test `a_cost_guarded_turn_narrates_nothing` proves it through the engine with a priced catalog and a warm `Capable` target. With the guard disabled, that test goes red. This commit landed after the last full workspace run. Its gate was the `handoff_escalation` binary: 8 passed, 0 failed.
- **C1.** A `Dimensions` pick of the `Efficient` tier is not reachable at the shipped threshold, because `production_intensity` has a maximum of `tanh(0.5)`. The guard test for a pick that is not `TestsPassed` uses the `Ambiguous` default.
- **C3.** Both skips shipped: `tools_declared` and `policy_admits_no_local`. A skipped quote leaves no local candidate to count, so the audit note for the tool exclusion and the `NoToolCapableTarget` refusal now read `local_withheld_by_tools`. The plan did not predict this. The test `messages_api_surface::f2_...` found it.
- **C3.** On a turn where the call is made, a fleet error still fails the turn. That is unchanged on purpose. A sub-budget and a fail-open arm for the quote are a separate decision. Update, 2026-09-28: ruled (ruling 5 of that date in `synergies/typesafe-selector-and-cache-affinity.md`) and built. See the C3 addendum of that date below.
- **C5.** Loader added in `3b21e98`: the catalog supplies both local TTFT fields, and `main.rs` carries them into the engine. The defaults remain 60 ms and 0.0. Tests failed before wiring: the local quote was 60 ms instead of the configured 842 ms. Catalog, engine integration, and affected routing tests now pass. A real slope still needs a prefill measurement. The production binary does not attach a local fleet, so this loader does not itself enable local inference. Independent mutations of the slope, base, and negative-value guard each failed the intended tests. The source was restored after each mutation, and 25 affected tests passed clean. The integration tests cover the library configuration-to-quote path, not the binary startup path.
- **C2.** No live request exists yet that shows `cache_read_input_tokens > 0` after an append of 20 or more items.
- **All.** The full workspace suite ran under `timeout 900` two times. At `6ab8908` (C1 to C3): 111 test binaries, 1649 passed, 0 failed. At `6e905ba` (C5 added): 111 test binaries, 1651 passed, 0 failed. No ignore from this work remains in `crates/`.
- **Baseline, 2026-09-19.** At `29665d8`, `ulimit -Sn 65536 && timeout 900 cargo test --workspace` passed: 1652 passed, 0 failed, 141 ignored. The run covered 104 test binaries and 7 doc-test suites. The initial run failed in `embedded_selection` with `Too many open files` at the inherited soft limit of 1024. With the raised limit, that binary passed all 7 tests before the full rerun. No source changes were necessary.

Decisions that only the owner can make:

1. T2: the owner requests a bandit with serving strategies and background evaluation arms, including online TypeSafe/Jev. See the ruling addendum of 2026-09-19. The runtime contract still needs a settled brief before implementation.
2. T6 is accepted as proposed on 2026-09-19. The owner kept PR #18 as one unit until 2026-09-23, then re-scoped it to its implemented work (see the ruling addendum). The remaining cache decisions are C6 timing and C3 fleet fail-open behavior. Update, 2026-09-28: C3 fail-open is ruled and built (see the C3 addendum of that date). C6 stays deferred.
3. P2 and fleet-redis-3: the remedy choice for each waits for live C2 evidence. On 2026-09-28 the owner put both in the cache follow-up PR. See the addendum of that date in C2. Update, 2026-09-28: remedy (b) and the fleet-redis-3 fix are done (see the C2 addendum of that date). Only remedy (a) still waits for live C2.

The mutation checks used `sed` to change one token, ran the suite, and used `sed` to restore the token. `git diff --quiet crates/` confirmed each restore. C1: `<` to `<=` turned `a_tie_in_quoted_cost_keeps_the_efficient_pick_and_its_source` red. C2: `CACHE_LOOKBACK_BLOCKS` from 20 to 21 turned the evidence test red. C3: a predicate that never skips turned four of the five tests in `tests/local_quote_skip.rs` red, and the control stayed green. The C3 stage wrote its tests before the fix but did not run them before the fix, so this mutation is the evidence that they fail without it.

**C2 probe verification, 2026-09-19.** The full workspace suite at `d582e62` passed 1694 tests, with 0 failures and 142 ignores, across 107 test binaries and 7 doc-test suites. Seven independent probe mutations failed: missing second request, short append, missing run nonce, bypassed budget, ignored route, swapped counters, and disabled cap preflight. The short-append mutation first exposed a missing assertion. Commit `d582e62` adds that guard, and the committed recheck failed on 19 received items. All mutations were restored. The probe passes 10 offline tests, and its gated live entry compiles without execution. Logs: `/tmp/roundhouse-c2-workspace-final.log` and `/tmp/roundhouse-c2-refute-*.log`. No live cache evidence is claimed.

## 2. How to continue

1. Read `CLAUDE.md`, then the ruling and its addendum, then this plan.
2. Run `git fetch origin` and compare the branch in the status table with `origin`. Trust only pushed state.
3. Complete the C4 tool-marker decision and its regression first. C5's loader is complete. Live C2 evidence needs working credentials, and C6 timing awaits the owner. Independent bandit observation work can proceed under R21. Serving allocation still needs the decisions in `PLAN-routing-strategy-bandit.md`. Each implementation starts with failing tests.
4. Run cargo commands one at a time. The box has four cores and one build lock. Put `timeout 300` before a targeted test command and `timeout 900` before a workspace test command.
5. Commit before any mutation stage. Commit with `--no-gpg-sign`. Push through the `gh` credential helper. `~/.gitconfig` rewrites `https://github.com/` to SSH, so put `GIT_CONFIG_GLOBAL=/dev/null` before the push command when the SSH agent does not answer.
6. The owner chose to keep PR #18 as one unit on 2026-09-19. On 2026-09-23 the owner re-scoped it to its implemented work; the learning work follows in separate PRs. Keep each concern in a separate commit. The earlier split inventory remains useful for reviewing the existing rungs:
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
2. `normalizing_four_riding_markers_still_spends_the_whole_allowance`.
3. `a_request_with_three_riding_markers_keeps_the_penultimate_and_drops_the_previous`.
4. `a_short_append_adds_no_second_marker`.
5. `the_judge_quote_still_carries_no_cache_control`.
6. `the_ledger_records_the_item_count_the_turn_was_dispatched_with`, at the fold level in `session.rs`.
7. `a_replayed_log_reconstructs_the_same_last_segment_count`.

**Live evidence that is still necessary.** The tests prove the block arithmetic. A real session with a long append must show `cache_read_input_tokens > 0` on the next turn. Spend is capped by R9.

**Addendum, 2026-09-23 (PR 18, fleet-redis-3 review fix, ruled partially valid).** Step 4's design put the arithmetic in two crates: `engine.rs` computed `last_segment_count.checked_sub(2)` and handed the fleet client a bare block index. A thermo-nuclear review found the two copies could desynchronize, with a concrete case: when the previous dispatch rode four client tool markers, `body()` placed no block marker at all (the allowance was spent), but `engine.rs` still reported `n - 2` as "the block it marked," so the *next* quote reached back for a cache write that was never made. The extraction landed — `previous_breakpoint` is now `previous_segment_count` (the raw `last_segment_count`, not `n - 2`), `engine.rs` no longer does the subtraction, and the placement policy moved to `anthropic_messages::cache_markers::plan(segment_count, riding, previous_segment_count)` in its own module with table tests. **The desync itself is not fixed.** `plan` still cannot tell the two histories apart, because `previous_segment_count` is a count, not a "was a marker actually placed" fact. Proven with a live control and a red, `#[ignore]`d defect test in `anthropic_messages.rs` (`a_history_with_no_riding_markers_marks_its_own_penultimate_block` / `a_history_with_four_riding_markers_places_no_block_marker_the_next_quote_can_reach_back_for`). Closing it needs a new fact recorded at dispatch time — whether `plan()`'s own `penultimate` was actually sent — carried through `Routed` into `TargetState` (`roundhouse-core/src/routing/ledger.rs`), which is outside the crate set this fix stage was scoped to. **Open for the owner**: this is design work, not a mechanical fix — it touches the ledger's durable shape and what a replayed session reconstructs.

**Addendum, 2026-09-28 (PR 18 review round 6, owner decision).** The round-6 review found a second mismatch between the cache ledger and the markers (P2). The defect was already on main. `anthropic_messages::cache_markers::plan` never marks the final segment. But `CacheLedger::expected_cached_tokens` predicts the whole previous request as warm, up to `min(last_prefix_tokens, isl)`. As a result, each follow-up quote on a `Deterministic` target prices the previous final item as a cache read. The provider bills that item as a cache write. With a 30k-token `tool_result` as the final item on the Claude rate card, the quote is low by about $0.10 per turn. The C1 guard and the cost guard decide on that quote. The rationale in `cache_markers.rs` says that a marker on the last block "would write a cache entry the next turn cannot read". That rationale is false for an append-only log, because the next request sends that block again with the same bytes. There are two remedies:

- (a) Mark the last block, except on handoff-note turns and side calls. This uses one more of the four breakpoints.
- (b) Make the ledger predict warmth only up to the previous penultimate marker. The quote is then conservative, and the wire does not change.

The owner put P2 and fleet-redis-3 in the cache follow-up PR, not in PR 18. Both change what the ledger records about markers, and the live C2 measurement is necessary to choose between the remedies. The fleet-redis-3 defect test stays `#[ignore]`.

**Addendum, 2026-09-28 (cache follow-up, P2 remedy (b) and fleet-redis-3 fixed).** Branch `ai/cache-ledger-marker-fact`, under ruling 1 of the 2026-09-28 addendum in `synergies/typesafe-selector-and-cache-affinity.md`. One recorded fact fixes both defects. The wire does not change.

- **The fact.** `BlockMarker` (`roundhouse-core/src/routing/ledger.rs`) is `Unplaced` or `Placed { segment, prefix_tokens }`. It is stored in the new `DecisionRecord::block_marker` field on `Routed`. The session fold copies it into the new `TargetState::last_block_marker` field. Both fields use `#[serde(default)]`, so old logs replay.
- **One owner of the placement rule.** The engine now builds the `FrontierQuote` before it writes `Routed`. It asks `FrontierQuote::marker_placement()` where the marker goes, and then it sends the same quote. For Anthropic Messages, `marker_placement` and `body()` both call `anthropic_messages::marker_plan`, which calls `cache_markers::plan`. So the recorded placement and the sent placement cannot differ. The test `the_reported_placement_is_the_last_marker_the_body_carries` checks this. If the quote cannot be built, the error still occurs after `Routed`, as before. Responses and Chat Completions report `Automatic`, and they record nothing.
- **Prefix tokens.** The value is the toolbox count (without `tool_choice`) plus the tokens of the items through the marked block. `ContextAssembler::tokens_through` gives the item part. It uses the same units as `isl_tokens`.
- **P2 (remedy (b)).** `CacheLedger::expected_cached_tokens` now predicts warmth only through the recorded marker prefix. The previous request's final item is therefore priced as uncached input. If the previous request placed no marker, the prediction is 0 tokens, and this includes the toolbox. A record without the field keeps the whole-prompt prediction. This applies to old logs and to dialects without markers.
- **fleet-redis-3.** `FrontierQuote::previous_segment_count` is replaced by `previous_marker: PreviousMarker`. It is `Unmarked`, `Block(index)`, or `Inferred { segment_count }`, and `PreviousMarker::of` builds it from the ledger. `plan` uses a recorded block directly. It infers `n - 2` with `penultimate` only for a record from before this change. If the history rode four tool markers, the ledger records `Unplaced`, so the next request places no reach-back marker. The defect test is no longer ignored, and it passes. Its control also passes.
- **Test changes.** `end_to_end::a_warmed_frontier_target_wins_a_turn_it_would_otherwise_lose` now sends two items on its first turn. Its Anthropic target caches nothing from a one-item prompt, so the old premise was P2 itself.
- **Still open.** Remedy (a), which marks the last block, still waits for live C2 evidence.

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

**Addendum, 2026-09-28 (fail open within a bound, ruled under owner delegation).** Ruling 5 of the 2026-09-28 addendum in `synergies/typesafe-selector-and-cache-affinity.md` settles the fleet fail-open question that the first C3 note left open. The eviction-signal skip above is a separate question and is still open.

- The residency call has its own bound, `EngineConfig::fleet_quote_deadline_ms`. The default is 500 ms, and the catalog can set it (`fleet_quote_deadline_ms`, loaded by `catalog_config::engine_config`, with `deny_unknown_fields` kept). The bound that applies is the earlier of that deadline and the turn deadline.
- On a fleet error or a timeout at that bound, the turn drops the local candidate, records `local_quote_skipped` as `fleet_error` or `fleet_timeout`, logs a warning, and routes among its other admitted targets.
- The engine decides before the call whether the turn can fail open. It can when some quoted hosted candidate passes the policy's `admits` check (the policy filter plus the frontier cadence over this session's history) and the credential check (`hosted_fallback_admitted` in `engine.rs`). A turn whose cadence is spent therefore keeps the old path, because the local answer is the only one it can take.
- Known gap: the budget grant is opened after quoting, so a spent `degrade_to_local` budget is not known before the call. Such a turn still fails open at the short bound. If the fleet is slower than the bound or returns an error, the turn loses the local candidate that the budget would have routed it to. Closing this gap needs the budget state before the residency call, which would be an extra ledger round trip on every turn.
- A turn that cannot fail open (a local-only session) keeps the old path: the call is bounded only by the turn deadline, and a fleet error fails the turn with `EngineError::Fleet`. This is a deliberate reading of the brief: the short bound exists to fail open, and on a turn with nothing to fail open to it would only turn a slow turn into a failed one. A slow fleet on a local-only turn therefore still fails with `TurnDeadline`, the same error class as before.
- If the turn deadline ends the wait on a turn that could fail open, the turn fails with `TurnDeadline`, because a hosted dispatch would start already out of time.
- Tests, in `tests/local_quote_skip.rs`: `a_fleet_error_drops_the_local_candidate_and_the_turn_serves_a_frontier_target` and `a_fleet_that_hangs_past_its_bound_is_skipped_and_the_turn_serves_a_frontier_target` failed before the change (with `Fleet(Rejected(..))` and `TurnDeadline(5000)`) and pass after it. `a_local_only_session_with_an_erroring_fleet_still_fails_the_turn` and `a_local_only_session_waits_for_the_turn_deadline_not_the_residency_bound` are controls that passed before and after. `a_spent_frontier_cadence_is_not_held_to_the_residency_bound` failed with `Routing(PolicyRefused)` while the fail-open predicate checked only `permits`, and it passes with `admits`. The catalog tests are in `catalog_config/tests.rs`. No earlier test asserted that a residency-call error fails a turn that has a hosted target, so no test needed rewriting.

### C4 — the 1-hour TTL as one per-target setting

**Rule.** Do not add a second setting. `FrontierModelSpec.cache_model` already holds `CacheModel::Deterministic { ttl_ms }` (`frontier.rs:49, 126`). Carry `cache_ttl_ms: Option<u64>` on `FrontierQuote` from the same field. `body()` calls `CacheControl::ephemeral_for("1h")` when the value is `3_600_000`. The ledger and the wire then read one field, so they cannot disagree.

**The price guard.** `ProviderPricing` has one write rate, `cache_write_per_mtok_usd` (`ledger.rs:96-104`). The provider bills a 1-hour write at 2 times the input price and a 5-minute write at 1.25 times. The catalog boundary (`catalog_config.rs`) must refuse a spec that declares a 1-hour TTL without the 2 times write rate. Test: `a_one_hour_cache_model_requires_the_one_hour_write_rate`.

**Addendum, 2026-09-23 (PR 18, fleet-redis-2 review fix).** The rule above still holds — one setting, read once — but `cache_ttl_ms: Option<u64>` on `FrontierQuote` is superseded. A thermo-nuclear review found the catalog accepted a `Deterministic` `ttl_ms` that was neither `300_000` nor `3_600_000`: `body()`'s `match` fell through a `_ => None` arm to the wire's own five-minute default with no `ttl` field sent, while `CacheLedger` kept modelling the target as warm for the number the catalog declared — a router pricing a cache hit the wire was never asked to grant. The field is now `pub cache_lifetime: anthropic_messages::CacheLifetime` (`Default | OneHour`, no third state), resolved once by `FrontierModelSpec::requested_cache_lifetime` and refused at the catalog boundary (`CatalogError::UnsupportedCacheLifetime`) for any other deterministic TTL on `anthropic_messages`. **This is a boot-time behavior change**: a catalog that used to load with an unspellable Messages TTL now refuses to start. Evidence: `catalog_config::tests::a_deterministic_ttl_the_wire_has_no_spelling_for_is_refused` (red before the fix, green after, with dialect-scoping controls), `frontier::tests::requested_cache_lifetime_resolves_every_dialect_and_cache_model_pair`, and the dispatch-time refusal tests in `cache_ttl_from_catalog.rs` and `judge/tests.rs`.

### C5 — the local TTFT quote reads the residency answer

**Steps.** Add `pub local_ttft_ms_per_prefill_token: f64` to `EngineConfig` (`engine.rs:463`) with default `0.0`, documented next to `local_base_ttft_ms`. `LocalQuote::to_candidate` (`local.rs:174-186`) returns `base + effective_prefill_tokens * per_token`, the mirror of `frontier.rs:191-193`. The call site is `engine.rs:2042-2044`.

**Tests first.** `a_local_candidate_ttft_rises_with_effective_prefill_tokens` and the control `a_zero_slope_reproduces_the_flat_quote`. The default of `0.0` keeps the tests that pin the flat value green: `mcp_surface.rs:166, 192`, `budget_routing.rs:432`, `credential_gating.rs:298`, `policy_routing.rs:181`.

**Addendum, 2026-09-28: local capacity is now priceable.** Ruling 6 of the 2026-09-28 addendum in `synergies/typesafe-selector-and-cache-affinity.md` is implemented. The catalog accepts an optional `local_capacity_price` (`input_per_mtok_usd`, `output_per_mtok_usd`; no cache rates). `LocalQuote::to_candidate` charges `effective_prefill_tokens` at the input rate plus `expected_output_tokens` at the output rate; matched local tokens are free. Without a price the quote stays at $0. `TurnBudget::admits` now admits a local candidate by target, so a priced local quote never closes degrade-to-local. The metrics document reports `savings.local_capacity_usd` and each local row's `capacity_usd`, nets `routing_savings_usd` of the capacity cost of the priceable turns on rows with a priced correlary, nets `routing_savings_at_decision_usd` of the decision's own local quote, and adds a labelled `observed_cost.local_capacity_usd`. Without a price it publishes `local_capacity_priced: false` and `null` capacity fields. Tests: `tests/local_capacity_price.rs`, `catalog_config/tests.rs`, `metrics/mod.rs`, `metrics_api.rs`, `local.rs`, `budget.rs`. Follow-up: the Relay per-turn summary still reports gross savings and no `actual_cost` for a local turn when local is priced.

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
- Review round 6 (2026-09-24) reported these minor items. None of them blocks PR 18:
  - `EchoFrontierClient` tags its zero cache read as `CacheReadSource::Provider`. The stub makes up every count that it reports.
  - `FIRST_OUTPUT_BASIS` and `TURN_ELAPSED_BASIS` include the judge side call, which runs after `TurnStarted`. The docs do not say so.
  - The unwired learning index passes the page limit directly to `ZRANGE ... LIMIT 0 <count>`. A count above the Redis `i64` range fails there, but the memory double accepts it.
  - The doc on the `u64::MAX` clear in `store/contract/learning.rs` says that it guards against a lossy number comparison. It guards against a wrapped watermark.
  - `classify_config` does not compare `max_prompt_chars` times four bytes with `max_total_bytes`. At 2000 characters and 4096 bytes, the classifier refuses a long CJK prompt before any grant, so no money is lost.
  - The catalog refuses an Anthropic `Deterministic { ttl_ms: 60000 }` entry with a message that is false for a TTL shorter than five minutes.
  - Owner design question: the fallbacks of a cost-guarded turn can cost more than the head that the guard displaced. With capable `[sol $0.90, nova $0.04]` and efficient `[luna $0.12]`, the fallback after `nova` is `sol`, and `luna` is never tried. Answered 2026-09-28 (ruling 4 of that date in `synergies/typesafe-selector-and-cache-affinity.md`) and fixed in `StagePolicy::resolve`: a guarded turn fails over to the other capable members that quote strictly below the efficient head, then to the efficient tier, then to the rest of the capable tier, each part in recipe order. The example now fails over to `[luna, sol]`. Tests: `the_guard_serves_the_first_capable_target_that_is_cheaper_not_the_tier_head` (it asserted `[sol]` before) and `a_guarded_turn_fails_over_to_cheaper_capable_members_then_the_efficient_tier`. The rationale no longer calls those fallbacks "in the same tier".
