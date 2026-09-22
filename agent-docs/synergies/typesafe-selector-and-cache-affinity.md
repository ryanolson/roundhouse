<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# TypeSafe as a selector, and cache affinity: the ruling

> **Status: proposed direction, 2026-09-17, roundhouse @ `e521855`.** The rulings below are proposals until the product owner accepts them. T2 amends R3 of `../PLAN-frontier-selection.md`, and R3 stands until then. Evidence: `../research/typesafe-jev-primary-read.md` for TypeSafe and the pst2154 benchmark. The roundhouse claims come from two Opus dives over this tree. One Sonnet fact-checker re-derived each dive: 21 claims examined, 19 confirmed, 2 corrected. The corrections are in section 8.

## The verdict, first

1. **Do not use TypeSafe as the only criterion.** Jev classifies a task against sentences that a person wrote. It does not know a price, a TTFT, a budget, a context limit, a load, or a cache state. The product sentence requires all of those. TypeSafe's own routing patterns also keep the decision in the caller's code.
2. **TypeSafe fits one slot that already exists.** `pick_tier` has a band that it labels `Ambiguous`. Upstream Switchyard consults a classifier in that band. This tree removed the classifier and takes the default tier. Jev is a candidate for that band, and only that band.
3. **A bandit is sound only in a specific form.** Guardrails come first and are absolute. The bandit chooses inside the admitted set. It decides once per cache-cold segment, not once per turn. Its draw is a hash, and its posterior is a versioned document. Three prerequisites do not exist today, so the bandit is not the next step.
4. **The two questions are one question.** A selector that decides per turn without the cache state destroys the cache. The selector that ships today already does this when a tier recipe is configured. This is the largest cache finding in the review, and it is independent of TypeSafe.
5. **The state that roundhouse holds is close to minimal and is sufficient for reconstruction.** `ItemAppended` plus `SessionCreated` reconstruct a request for any destination. No provider-opaque state goes to any provider, so a switch cannot send state that the new provider rejects. Two small projections are missing. Both derive from the log, so no new event is necessary.

## 1. Why TypeSafe cannot be the only criterion

**It lacks the inputs.** The `autoModel` call in the post is one `choice` question. The options are model ids. The criteria are one sentence per model. The pst2154 script is the same shape with four handler classes. Neither input contains a number. Roundhouse selection reads exact prices, quoted prefill, quoted TTFT, `quality_prior`, the budget, the frontier cadence, and provider reachability (`engine.rs:1968-2326`). A selector that reads none of these cannot co-optimize function, cost, and time.

**The evidence is a label-agreement test.** The benchmark has 36 synthetic cases. Each case is one sentence. One person wrote the labels. No case measures whether the selected handler did the task. Against Qwen3.8-27B the accuracy is a tie at 64/64. The measured advantage is latency only: 228 ms against 1,887 ms mean, hosted end to end. The Qwen baseline generated JSON text. A single prefill with option logprobs on local hardware is a different and faster baseline that nobody measured.

**The input in roundhouse is different.** A coding-agent request carries 50k to 200k tokens of history. TypeSafe publishes no context limit. The largest published state is about 11,800 tokens at 0.27 s. No published number covers the size of a real request.

**It asks the wrong question.** `brief.rs:11-19` rules that a model never sees a price, the candidate list, or a target name. The reason is neutrality: "The routing question is asked exactly once, of code." An option list of model ids breaks this rule. The criteria sentences are also hand-written quality claims. `CLAUDE.md` already rules that `quality_prior` must be sourced, not asserted.

**It is a hosted dependency on the path to first token.** The API returns 429 and 529. The service has no self-hosted option and no zero-retention option. R3 says that routing guidance is "never a runtime dependency". `stage.rs:56-58` makes "routing makes no model calls, ever" a property of the type.

**It moves user content to a new third party.** No roundhouse ruling governs this today. The fact-check searched `crates/`, `agent-docs/`, `README.md`, and `CLAUDE.md` and found none. The judge sends a truncated brief, but only to a model in the deployment's own catalog under its own credential. When a route is local, the content does not leave the deployment at all. A TypeSafe call changes that for every turn that it classifies.

**It has no memory.** Jev answers each call alone. A per-turn answer changes when the last message changes. Section 4 gives the cost of that.

## 2. The slot that TypeSafe fits

`pick_tier` (`stage.rs:366-404`) is a four-rule cascade, ported from Switchyard:

```text
severity >= 1.0            -> Capable    (Override)
tests passed, work done    -> Efficient  (TestsPassed)
dimension score, confident -> Capable or Efficient (Dimensions)
otherwise                  -> default tier (Ambiguous)   <- upstream consults a classifier here
```

The last arm carries this comment: "Upstream consults a classifier here. This tree does not." `policy.rs:47-53` leaves the same door open: "a turn classifier can come later".

So the correct statement of the opportunity is narrow. TypeSafe can implement the classifier arm of the Switchyard cascade. It answers a demand-side question about the turn. The tier recipe, the admitted pool, and the quotes still map the answer to a target.

**The questions to ask are about the turn, never about the models.** One call can carry all of them, because fan-out adds no latency:

- `choice` — which tier fits this turn: `capable` or `efficient`. The criteria describe the work, not a model.
- `score` — the complexity of the turn, on a rubric of three or four levels.
- `noul` — can a test or a tool check the result of this turn. This is the verifiability signal that `policy.rs:47-53` names as missing.
- `noul` — does an error in this turn have a high cost. This is the stakes signal that the same comment names.

**The state to send is a digest, not the transcript.** The judge brief is the precedent (`brief.rs:5-9`): truncated instructions, the objective, the last K tool pairs as fingerprints and heads. A digest of 1k to 2k tokens is inside the published latency range and costs about $0.00008 per call.

## 3. The bandit question

"A multi-armed bandit with guardrails that force TypeSafe" has two readings. This ruling takes the second.

- **Reading A: the arms are selectors.** One arm is "route by TypeSafe", and a guardrail gives that arm a minimum share. This is an A/B experiment. `validate/arm.rs` already does this with a hash. It needs no bandit.
- **Reading B: the arms are targets.** TypeSafe supplies the context and the prior. Guardrails fix what the bandit cannot change. When Jev is confident, its tier is forced and the bandit does not explore across tiers.

### The shape that fits this tree

```text
turn
 |
 v
guardrails (exist today, absolute)          allow list, quality floor, cadence, budget,
 |                                          fair use, credential, tool support
 v
admitted set
 |
 +-- warm segment? -- yes --> stay on the current target unless the quote says
 |                            that a move costs less for this turn (T4)
 no  (session start, fork, TTL expired, forced failover)
 |
 v
tier:  pick_tier rules 1-3 --> decided
       Ambiguous band      --> classifier (TypeSafe digest call, own deadline, fail open to default tier)
 |
 v
tier confidence >= floor?  -- yes --> tier is fixed. Explore only among targets inside the tier.
                           -- no  --> explore across the two tiers.
 |
 v
draw = hash(session id, segment index, salt, posterior version)   never an RNG
 |
 v
Routed { target, propensity, classifier distribution, posterior version }
```

Four properties make this compatible with the existing rulings.

1. **Guardrails are upstream of the policy.** `RoutingContext::admissible` and `Admitted::decide` (`routing/mod.rs:294-331, 452-492`) already bind before `choose` returns. A learned policy cannot mint a decision outside the admitted set.
2. **The draw is a hash.** `arm.rs:12-24` rules "Assignment is a hash, never a draw", because a draw breaks fold-equals-log. A Thompson sample seeded by a hash is replayable from the log.
3. **The posterior is configuration.** An offline job reads the log and writes a versioned posterior document. `choose` stays pure. This is R3's "calibrated configuration" with a shorter refresh period.
4. **Exploration costs no cache.** The bandit decides only where the prefix is already cold. Inside a warm segment the answer is "stay".

### Why the bandit is not the next step

A bandit learns what its reward measures. Today the log can supply only one of the three legs.

| Leg | Durable signal today | Gap |
|---|---|---|
| Cost | `Routed` rate card, `ResponseCompleted.usage` with cached and cache-write tokens | None |
| Time | `expected_ttft_ms` only | No observed TTFT anywhere. R21 rules it and it is not shipped. |
| Function | `ValidationDecided` verdicts on sampled turns, `ResponseIncomplete` | No outcome attributed to a routing decision. No tool-success or re-ask signal. |

A bandit with only the cost leg learns to select the cheapest target. `CLAUDE.md` names that failure: "a router that optimizes cost alone ships worse answers". Traffic volume is a second limit. Each context cell and target pair needs many segments before a posterior is better than `quality_prior`.

## 4. The link between the two questions: what a switch costs

Let `N` be the prefix tokens, `p` the input price, `r` the cache-read multiplier, and `w` the cache-write multiplier. Anthropic publishes `r = 0.1` (`0.025` for Claude Fable 5.1 and Mythos 5.1) and `w = 1.25` for the 5-minute TTL.

- Stay on warm target A: `r * N * p_A`.
- Move to cold target B: `w * N * p_B`. For an OpenAI-dialect target the write premium is zero, so `N * p_B`.
- A move pays on input only if `w * p_B < r * p_A`. Target B must be 12.5 times cheaper per input token, or 50 times cheaper against a model with `r = 0.025`.
- A return to A after its TTL costs `w * N * p_A`. That is 12.5 to 50 times the warm read.

One worked number, derived from two published figures. TypeSafe states that $0.042 per million is "238x lower" than the Claude Fable 5.1 input price, which gives about $10 per million. At `N = 100,000` the warm read is $0.025 and the cold write is $1.25. The output tokens that a cheaper model saves on one routine turn are far less than $1.22 at any realistic price. A local target pays in time: a cold 100k-token prefill is the whole TTFT.

**The decision unit for selection is the cache-cold segment, not the turn.** A segment starts at session start, at a fork by prefix admission, when the TTL of the current target expires, and on a forced failover.

### What the tree does today

- **Without a recipe**, `AffinityPolicy` scores normalized prefill, cost, and TTFT at weights 1.0, 0.5, and 0.25 (`policy.rs:83-92, 164-175`). The ledger prices a new target cold (`ledger.rs:407-416`). The pull toward the warm target is real but soft. Cost and TTFT can outvote it, and the weights are caller-set.
- **With a recipe**, `StagePolicy` selects the tier from `TurnSignals` alone and takes the first recipe-ordered member of the tier's admitted pool (`stage.rs:572-586, 666-761`). It never reads a quote. The inner `AffinityPolicy` runs only when `ctx.tiers` is `None` (`stage.rs:667-669`). `main.rs:982-984` wires this in production when tiers are configured.
- **The result.** Tests pass, so the turn de-escalates to `Efficient` on provider B, cold. The next turn has an error with severity 1.0, so it escalates to `Capable` on provider A. If 5 minutes passed, A is cold too. A coding session alternates between these states many times. No term resists the flip. The words `hysteresis`, `sticky`, and `previous_target` have zero hits in `stage.rs` and `policy.rs`.

A per-turn Jev call in place of `TurnSignals` inherits this defect unchanged. That is why T3 and T4 come before T2 in the order of work.

## 5. Cache review, by destination

Every outbound request is a render of the roundhouse log. No path forwards the client's message array (`anthropic_messages.rs:441`, `openai_responses.rs:241-244`, and `execute()` always calls `body()`). The conversation goes out as one `role:"user"` message. For Anthropic, the content blocks split at item boundaries.

| Destination | What is right | Gap, by expected effect on hit rate |
|---|---|---|
| All | The render is deterministic: an ordered `Vec<Item>`, a pure `Item::render`, and `preserve_order` deliberately off so opaque JSON has one key order (`Cargo.toml:100-119`). Nothing that varies by request enters the prefix. Steer text and handoff notes append at the tail. | 1. With a recipe, selection ignores the quote (section 4). |
| Anthropic | Client block-level `cache_control` is stripped and one breakpoint goes on the penultimate segment, which is the stable prefix (`anthropic_messages.rs:367-400`). Tool-level markers ride verbatim and are counted against the limit of 4. `anthropic-beta` is forwarded. Cache read and write tokens reach `Usage` and the price. | 2. One breakpoint only. Anthropic examines 20 block positions back from a breakpoint. An append of 20 or more items puts the previous write outside the window, and the request reads nothing. 3. The 1-hour TTL is modeled and never requested (`ephemeral_for` has a test caller only). An agent that waits more than 5 minutes on a build returns to a cold prefix. 4. `metadata.user_id` is read and not forwarded. |
| OpenAI dialect | `prompt_cache_key` is stable per session. `store: false` is explicit. `previous_response_id` is never sent, so no second history exists. `cached_tokens` is captured and priced. | 5. Encrypted reasoning items are never requested or resent. This is a function cost on reasoning models, not a cache cost. |
| Dynamo | The price query sends block hashes, sequence hashes, and `session_id`, so the KV router can apply its own affinity (`local.rs:56-70, 130`). It reads back the matched prefix. The engine sends token ids, never text, because `encode(a)+encode(b) != encode(a+b)` (`engine.rs:405-410`). | 6. No inference path exists. The only production `LocalExecutor` is `EchoLocalExecutor` (`engine.rs:434`, `main.rs:975`). No code compares the hash chain of the roundhouse render with the chat template of a worker. 7. The local TTFT quote is a flat 60 ms (`engine.rs:497, 2042-2044`). It ignores the matched prefix, so a cold local target has no time penalty in the score. |
| Frontier as judge | The rubric leads and the transcript excerpt trails. The model is fixed per deployment. The cache key is `{session_id}#validate`, so the judge does not cool the entry that the router quoted (`judge.rs:13-19, 103, 584-620`). The judge is outside the cache ledger. | 8. On Anthropic the judge request carries no `cache_control`, because `segment_boundaries` is empty (`judge.rs:609`). The code declines this on purpose (`judge.rs:600-608`). A marker also does nothing if the rubric is shorter than the minimum cacheable length of the model, which is 512 to 4,096 tokens. Measure the rubric before any change. |

Two items that the audit raised are not defects.

- **The yield to four client tool markers** leaves the conversation without a breakpoint. The comment at `anthropic_messages.rs:389-396` argues this case and accepts it. Claude Code sets one tool marker. Record it as an accepted edge.
- **`CacheLedger::invalidate` has no production caller** (`ledger.rs:400`, only caller at `:708` in a test). Its own module doc says that it "must be called whenever the assembler rewrites history". Prefix admission forks a rewritten history into a new session with a cold ledger, which covers the known path. The missing projection in section 6 removes the need for the call.

One observation is outside the cache question. A render into one user message sends tool calls, tool results, and thinking signatures as text. That is a function question for the "transparently" word in the product sentence. It needs its own measurement and is not ruled on here.

## 6. State: what is held, and what is missing

| Class | Events |
|---|---|
| Necessary for reconstruction to any destination | `ItemAppended`, `SessionCreated` |
| Accounting and policy | `TurnStarted`, `Routed`, `ResponseCompleted`, `ResponseIncomplete`, `TurnDeduplicated`, `SideCallCompleted`, `SideCallAbandoned`, `ValidationDecided`, `Error` |
| Replay of the client stream only | `OutputTextDelta` |

Reconstruction is safe across destinations by construction. Nothing specific to one provider ever goes out, so nothing can be rejected after a switch.

Missing, and each one is a projection of events that exist:

1. **The last breakpoint position for each target.** The item count at each dispatch is in the log. The position closes gap 2: a second breakpoint at the last written position keeps the previous write reachable after any append.
2. **The prefix identity for each target.** `TargetState` holds `last_call_at_ms` and a token count (`ledger.rs:340-344`). A hash of the rendered prefix at the last dispatch lets the ledger detect a rewritten history without a call to `invalidate`.
3. **The observed TTFT.** `OutputTextDelta.at_ms` minus `Routed.at_ms` gives it. R21 already rules that the fold must exist.
4. **The observed cache deadline.** The TTL comes from configuration. The time of the last dispatch plus the TTL that the request asked for gives the real deadline for each target.

Nothing in the log is surplus to the stated purpose.

## 7. Proposed rulings

**T1 — TypeSafe is never the only criterion.** No selector that lacks the quote, the budget, and the cache state makes the final decision. This ruling applies to any classifier, hosted or local.

**T2 — A classifier enters only at the `Ambiguous` arm, and only as data.** The call happens before `choose`, under its own deadline fraction, and fails open to the default tier. The judge seam is the template (`judge.rs:108-131`, `interject.rs:228-239`). The result enters `RoutingContext` as a tier distribution with a confidence. `choose` stays pure, so "routing makes no model calls" holds as written. This amends R3: a hosted service becomes a signal source on the turn path. R3's objection to a version-unstable dependency still applies, and the fail-open default is the answer to it.

**T3 — The decision unit is the cache-cold segment.** A classifier call and any exploration occur only at a segment start. Expected call volume is one to three per session, not one per turn.

**T4 — Inside a warm segment, a tier change does not move the session unless the quote for this turn is lower on the new target.** Escalation on severity is the exception, because function outranks cost there. The exact form is an open decision (section 9).

**T5 — Shadow first.** Before T2 affects any route, record the classifier answer next to the `pick_tier` answer for every segment start. The promotion test has three parts: the agreement rate by band, the outcome by disagreement cell from `ValidationDecided` and `ResponseIncomplete`, and the classifier latency at real digest sizes. If no promotion test passes, T2 does not ship.

**T6 — Content egress to a classifier is opt-in by configuration, off by default, and never applies to a session whose policy admits local targets only.** The digest format is the judge brief format. This is the first egress ruling in the tree, and it needs the owner's acceptance independently of TypeSafe.

**T7 — The signal is the interface, not the vendor.** The carrier is a tier distribution with a confidence. A local classifier on Dynamo can produce it when an inference path exists. No trait is added for a second producer that does not exist yet.

**T8 — The bandit waits for three prerequisites:** the observed-TTFT fold (R21), an outcome signal attributed to a routing decision, and a propensity field in `DecisionRecord`. When they exist, the form in section 3 is the design.

## 8. Claims and their tests

Fact-check corrections to the dives:

- The audit said that no TTFT penalty exists for a cold prefix. This is wrong for frontier targets: `frontier.rs:191-193` adds `uncached * ttft_ms_per_uncached_token`, and a test at `frontier.rs:1306` asserts it. It is correct for local targets.
- The selection dive cited `Usage` at `metrics/pricing.rs:113-118`. The struct is at `event.rs:36-80`.

Evidence tests, written before any fix:

| Claim | Claim test, ignored | Control test, live | Ruling |
|---|---|---|---|
| Cache affinity: with a recipe, a `TestsPassed` de-escalation leaves a warm target for a cold target that costs more for this turn. | `stage.rs:1609` `a_deescalation_does_not_move_a_warm_session_onto_a_costlier_cold_target` | `stage.rs:1660` `a_deescalation_moves_to_a_target_that_is_genuinely_cheaper` | Valid |
| Anthropic lookback: an append of 20 or more segments puts the previous cache write outside the 20-block window. | `anthropic_messages.rs:1585` `a_long_append_keeps_the_previous_cache_write_inside_a_lookback_window` | `anthropic_messages.rs:1635` `a_short_append_keeps_the_previous_cache_write_inside_a_lookback_window` | Valid |

The failure output, from `timeout 300 cargo test -p <crate> --lib <name> -- --ignored`:

```text
stage.rs:1646  a de-escalation must never raise the quoted cost of the turn
  left: Frontier { provider: "openai", model: "luna" }
 right: Frontier { provider: "openai", model: "sol" }

a twenty-five-segment append put the only breakpoint at block 29, 25 positions past the previous write at block 4
```

The warm target quotes $0.03 and the cold target quotes $0.12. `StagePolicy` selects the cold target. Both modules are green with the ignores in place. An ignored test enforces nothing, so the first step of each fix removes the ignore.

The lookback claim rests on the Anthropic prompt-caching page as read on 2026-09-17: "If a growing conversation pushes your breakpoint 20 or more blocks past the last cache write, the lookback window misses it." The test proves the block arithmetic. It does not call the provider. A live measurement of `cache_read_input_tokens` after a long append is the remaining evidence.

Claims that have no test yet: the flat local TTFT quote, the 1-hour TTL that is never requested, and `metadata.user_id`. Each one is a one-assertion test on a body builder or a quote.

## 9. Decisions for the owner

1. **Accept or reject the amendment to R3 in T2.** The alternative keeps R3 as written: run the classifier offline only, as a labeler that calibrates the `pick_tier` thresholds.
2. **Select the form of T4.** Option A is a cost guard: a tier change moves the session only if the quoted cost for this turn is lower. Option B is a lease, the form that Router.com uses with a 5-minute window. Option C sends recipe turns through `AffinityPolicy` inside the tier pool. A and C read the quote. B is simpler and ignores it.
3. **Accept or reject T6** as the egress posture.
4. **Set the order of the cache work.** The proposed order is: T4, the second breakpoint, the 1-hour TTL as a per-target setting, the local TTFT quote, then the shadow classifier.

## Addendum (2026-09-17): the owner accepts the order, and Dynamo residency

**The order of the cache work is accepted.** The owner accepted item 4 of section 9 as proposed: T4, the second breakpoint, the 1-hour TTL as a per-target setting, the local TTFT quote, then the shadow classifier. Items 1 and 3 of section 9 stay open. R3 stands as written, and no classifier call ships before the owner rules on T2 and T6.

**T4 ships as a dominance guard on `Efficient` picks.** Section 9 listed three forms. Form C orders the members inside one tier, so it cannot resist a move between tiers. The evidence test has one member in each tier, and form C does not change its result. Form B ignores the quote. Form A reads the quote, and the quote already carries the cache state, because the ledger prices each target warm or cold. The built rule is narrower than T4 as first written:

- When the picked tier is `Efficient`, the policy compares the head of that tier with the admitted `Capable` pool in recipe order.
- If a `Capable` member quotes strictly less for this turn, the policy serves the first such member. The decision records the new source `cost_guard`. The rationale names both targets and no price, because `explain_last_route` republishes the rationale to the calling model. Both quotes are in the `considered` list of the `DecisionRecord`.
- A `Capable` pick is never redirected by cost. Escalation is for function.
- The policy holds no session state. A tie keeps the tier pick.

The reason is dominance. The `Efficient` tier exists to save cost. A `Capable` target that is also cheaper for the turn is better on function and on cost together. In practice this occurs only when the `Capable` target is warm and the `Efficient` target is cold.

The guard does not price the return trip. A move that is cheaper for this turn can still make the old target go cold before the session returns to it. That cost needs the observed cache deadline from section 6, and it is carried over.

**Dynamo residency is observed, not assumed.** The owner notes two facts. Dynamo can keep a KV cache much longer than 5 minutes. Dynamo can expose a realtime endpoint that reports residency. The owner also notes that the router can decide whether to make that call. This changes three things in this ruling:

1. **No TTL applies to a local target.** The 5-minute and 1-hour figures in sections 4 and 5 are Anthropic figures. For a local target the only honest cache model is an observed one. The price query already returns the matched prefix (`local.rs:149-153`), so it is a residency check today.
2. **The local TTFT quote must read the residency answer.** Gap 7 in section 5 stands. The observed matched prefix is the input that the flat 60 ms quote ignores.
3. **The residency call becomes a routing decision.** The call is an HTTP request on the path to first token. The router makes the call only when the answer can change the decision. The skip conditions and the record that a skip leaves in `Routed` are in the carry-over plan.

A warm local target that stays warm for hours also changes the arithmetic of section 4. A return to a local target after a long build costs nothing extra. A return to an Anthropic target after 5 minutes costs a full write. This asymmetry is a reason to prefer the local target for sessions with long idle periods, and the observed deadline from section 6 is the input that lets a policy see it.

## Addendum (2026-09-17, later): C1, C2, C3, and C5 are built

Section 8 names two ignored claim tests. Both are live now, and both fixes are on the branch. `../PLAN-cache-affinity.md` section 1 has the commits and the mutation evidence.

- **C1, the dominance guard (T4).** Built as the first addendum describes.
- **C2, the second Anthropic breakpoint.** `body()` also marks the block that the previous request to the same target marked, when that block is 20 or more positions before the penultimate block and two marker slots are free. The count comes from the ledger, and the ledger takes it at the `Routed` fold. This is projection 1 of section 6. The evidence test now states the reach of the provider as a literal, so a wrong production constant fails the test.
- **C3, the residency call as a decision.** The engine asks the fleet for a local quote only when the turn declares no tools and the turn policy admits a local target. `DecisionRecord.local_quote_skipped` records the reason. Before this change, a fleet error failed a tool turn that was always going to a frontier target. Four tests proved that under mutation.

- **C5, the local TTFT quote.** `LocalQuote::to_candidate` adds `effective_prefill_tokens` times a slope to the base. The slope is `EngineConfig.local_ttft_ms_per_prefill_token` and its default is `0.0`. No configuration loader sets it yet, so a deployment still quotes the flat value.

Gap 1 and gap 2 of section 5 are closed in code. Gap 2 still needs one live request as evidence. Gap 7 has its mechanism and needs a loader and a measured slope. Gap 3 is rung C4 of the plan, not started. The line numbers in the table of section 8 are from `35839e2` and moved when the fixes landed. The test names did not change.

## Addendum (2026-09-19): bandit direction from the owner

The owner requests a multi-armed bandit with arms that run offline and TypeSafe/Jev arms that run online. This changes the direction in section 3, which selected targets as arms. The new direction does not yet settle the arm contract.

The owner clarified that this includes both serving strategies and background evaluation arms. Offline-trained strategies can compete with online Jev for live routing. Background evaluation also belongs in the design.

The design must distinguish observed serving outcomes from estimates produced by background evaluation. A background recommendation alone does not show what its proposed target would have achieved. T6 remains an independent decision. The reward, selection cadence, and promotion criteria still need a settled brief. No bandit or classifier call ships from this direction alone.

## Addendum (2026-09-19, later): C4 price guard and T6 accepted

The owner accepts catalog rejection for C4. A one-hour cache entry requires a write rate of twice its input rate. A warning does not satisfy this rule. The target's existing `cache_model` remains the TTL source.

The owner accepts T6: hosted classification is opt-in, off by default, excluded from local-only sessions, and restricted to the bounded judge brief. This permits implementation of the Jev shadow adapter under that policy. It does not establish the quality evidence or utility weights needed to promote a serving bandit.

The owner keeps PR #18 as one unit. Separate commits preserve the boundaries between concerns within that PR.

## Addendum (2026-09-19): C4 tool-marker conflict remains open

The C4 checkpoint carries the target TTL into Messages conversation markers and rejects one-hour catalog entries with mismatched write rates. A body-level regression exposes a remaining conflict: preserved five-minute tool markers precede the new one-hour conversation markers. The emitted TTL order is `[300, 3600, 3600]` seconds. Anthropic requires longer lifetimes before shorter ones.

Two remedies need an owner decision: normalize tool markers to the target TTL, or reject requests whose preserved markers conflict. Normalization changes the earlier verbatim-tool rule. Rejection refuses requests that the caller otherwise expects to work. The failing regression is explicitly ignored until that decision. C4 remains incomplete.

## Addendum (2026-09-21): per-turn routing, background classification, and cache ownership

The owner requires model selection on every turn. Inputs include cache state at each eligible destination, current request complexity, and context complexity. Cache segments do not lock model selection or selector assignment.

Fast local logic selects the live route. Jev classifies turns outside the routing path. Those classifications enrich the sequence metadata that the online bandit uses on later turns. Jev does not delay the current route. This supersedes the proposed segment allocation and the interpretation of Jev as a competing synchronous selector for this implementation.

The owner proposes bounded turn history through metadata and classifications, together with the current user prompt. This avoids transmission of full contexts. T6 still applies: explicit opt-in, disabled by default, no local-only sessions, and bounded content. The classification schema and history projection need an implementation brief. The existing capable-versus-efficient adapter does not yet supply this richer metadata.

Classification results are features, not observed rewards. A result must identify its source turn, classifier version, and availability time. A later classification cannot change a recorded earlier decision. Failed or missing classifications remain explicit. Learning still needs a defined outcome signal and a cost-versus-latency policy.

The owner gives Roundhouse control over cache markers. Provider adapters can inject, modify, or remove markers as the situation requires. Each adapter must obey its provider's marker rules. For C4, existing tool markers adopt the target TTL, which is already the source for conversation markers and catalog pricing. Tool definitions and schema contents remain unchanged. This supersedes the normalize-or-reject gate and permits removal of its test ignore.

**C4 verification, 2026-09-21.** Commit `1280855` implements that rule for Messages tool markers. Before the fix, five assertions failed and 69 controls passed. After the fix, all 74 focused tests passed. Independent mutations caught bypassed normalization, retained one-hour TTLs on default-TTL targets, and corrupted markers. Each restore matched the committed source. The full workspace suite passed 1760 tests, with 0 failures and 141 ignores. The C4 ignore is removed. No live provider request was made. Logs: `/tmp/roundhouse-c4-tooltl-prefix-red.log`, `/tmp/roundhouse-c4-final-refute-M{1,2,3}.log`, and `/tmp/roundhouse-c4-normalization-workspace.log`.

## Addendum (2026-09-21): frontier review supplies interval feedback

The owner defines the quality signal through the frontier judge. A successful review with no corrections gives positive feedback to routing choices since the previous frontier review boundary. A review with corrections gives negative feedback to those choices. This is shared feedback for the reviewed interval, rather than an independent quality observation for each turn.

The review must identify its starting boundary, ending boundary, and covered routing decisions. Delayed results apply to that recorded interval. They cannot include turns that occurred after the prompt snapshot. Each review result updates learning once, including after replay. Cache state informs the frontier review, but a cache hit or expiry alone is not a quality verdict.

A skipped, failed, malformed, or insufficient-context review supplies no positive label. The next successful review can cover the outstanding decisions only if its prompt represents them. A bounded brief that silently omits part of the interval cannot label the full interval as reviewed.

The current `Verdict` separates `on_track`, `divergence`, and `missing_context` from the mapped `SteerAction`. An off-track verdict without a located divergence maps to `Continue`. Thus `Continue`, suppressed intervention, or absence of a delivered correction is not sufficient evidence of a no-correction review. The integration must retain an explicit review result and its coverage.

This settles the source and interval attribution of the quality reward. Jev classifications remain contextual features. Cost and latency remain separate measured inputs to the routing objective. The reward scale, learning update, and cache-aware frontier review integration remain implementation work. The existing judge has a separate cache key, but its Messages quote has no cache breakpoints or requested TTL. Those fields do not yet establish the requested cache-aware review path.
