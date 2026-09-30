# Choosing a model

This chapter describes two-tier model selection. It covers how a project names an efficient tier and a capable tier, what moves a turn between them, and failover. The routing learner, which learns a serving strategy on top of the tiers, is in [The routing learner](routing-learner.md).

## The tier recipe

A project's `"tiers"` block in the control plane turns routing into a choice between two ordered lists of targets:

```json
"tiers": {
  "capable": ["openrouter/openai/gpt-5.6-sol", "openrouter/moonshotai/kimi-k3"],
  "efficient": ["local/my-local-model"],
  "picker": "efficient_first",
  "confidence_threshold": 0.5
}
```

| Field | Meaning |
|---|---|
| `capable` | Ordered target names for the capable tier: `provider/model` for a hosted model, or `local/model` for a model of the local fleet. |
| `efficient` | The same, for the efficient tier. |
| `picker` | Where an undecided turn lands. `efficient_first` (default) or `capable_first`. |
| `confidence_threshold` | How sure the scorer must be before it overrules the picker, from `0.0` to `1.0`. Default `0.5`. A value outside the range is refused at load. |

The block refuses unknown fields. A misspelled `capible` otherwise gives an empty capable tier, which is legal, and every turn then goes to the other tier with no message.

A project without `"tiers"` routes with `AffinityPolicy`. See [Routing and the selection service](routing.md#the-routing-policies). When any project sets a recipe, every decision of the deployment reports the policy `stage`, or `learned` when a learner is enabled. This includes the decisions of projects without a recipe. Their target and rationale are the same as `AffinityPolicy` gives. See the same section for when that composition happens.

The shape follows Switchyard at `053a61e`. Switchyard's tier is a two-variant enum, `Efficient` and `Capable`, reported as `weak` and `strong`. Roundhouse's `Tier` is the same two variants with the same labels. A label is independent of what a deployment calls its models, so a rationale reads the same in two deployments. In Switchyard, "recipe" names a removed Python API. The live upstream term is a `[routes.*]` stanza of type `stage_router`.

## What moves a turn between tiers

The session's own recent tool results move a turn between the tiers. No model call is involved anywhere in the decision. The scorer is a port of Switchyard's coding-agent scorer (`crates/libsy/src/algorithms/util/stage.rs` at `053a61e`).

The scorer reads four numbers from the log that the session fold already holds:

| Dimension | Meaning |
|---|---|
| `severity` | How severe the recent tool errors are. |
| `spinning` | 1 when the session is at least 8 exchanges deep and made no recent write, edit, read, or plan. Else 0. |
| `exploring` | 1 when the session is at least 8 exchanges deep, made no recent write or edit, but did read or plan. Else 0. |
| `production_intensity` | The share of recent operations that were writes or edits. |

`spinning` and `exploring` cannot both be 1, so nothing counts the production axis twice. Depth counts task exchanges only. Control calls to Roundhouse's own MCP tools do not count.

The arithmetic:

```text
raw        = 0.10 * (severity / 0.7 + spinning + exploring - production_intensity)
score      = tanh(5.0 * raw)        positive means capable, negative means efficient
confidence = |score|
```

`tanh` is used rather than a clamp, so a second signal that agrees moves the score less than the first. An operator reads the threshold as a count of signals. About 0.3 escalates on one signal. About 0.5 needs about one and a half. About 0.7 needs two that agree. A single maximum severity scores about 0.46, just under the default.

`pick_tier` applies four rules in order:

1. **Hard escalate.** If severity is 1.0 or more, pick capable. Source `override`.
2. **Hard de-escalate.** If tests passed on a turn that wrote or edited, and severity is 0 or less, pick efficient. Source `tests_passed`.
3. **Scorer.** If confidence is at or above the threshold, pick by the sign of the score. Source `dimensions`.
4. **Fall open.** Otherwise, pick the picker's default tier. Source `ambiguous`.

Switchyard's fourth rule asks an LLM classifier. Roundhouse removes that arm, so a routing decision can never make a model call. Upstream also escalates on a compaction flag. That flag has no input here, so the hard escalate uses severity only. At these thresholds, rules 1 and 2 can never both hold. Escalate stays first to match upstream.

With no tool history, as in a pure chat, every undecided turn lands on the default tier.

The decision records its source:

| Source | Meaning | Narrated to the next model |
|---|---|---|
| `override` | Hard escalate on critical severity. | yes |
| `tests_passed` | Hard de-escalate. | no |
| `dimensions` | The scorer decided. | yes |
| `ambiguous` | Nothing decided. The default tier served, or the picked tier admitted nothing and the other tier served. | no |
| `cost_guard` | An efficient pick yielded to a cheaper capable target. See [The cost guard](#the-cost-guard). | no |
| `strategy` | A learned strategy forced the tier. See [The routing learner](routing-learner.md). | no |

A handoff note tells the capable model that the previous model was in trouble only when a signal said so. See [Validate and steer](validate-steer.md).

### Where the default threshold comes from

Switchyard states that the default threshold of 0.5 comes from a calibration on SWE-Bench Pro Python-75. Every published Switchyard threshold and result uses `efficient_first`. Switchyard accepts `capable_first` but has not benchmarked it, and warns at startup. Roundhouse does the same: a recipe with `capable_first` logs an uncalibrated warning when the control plane loads. A session that starts on the strong model and moves some calls down is a `capable_first` shape. That shape has no published calibration.

The thresholds do not transfer across model pairs or domains. Switchyard's recalibration protocol is:

1. Run the capable model alone over about 40 to 75 tasks.
2. Run the efficient model alone over about 20 tasks, spread over the four quadrants below.
3. Put each task in a quadrant: RESCUE (capable fails, efficient passes), LOSS (capable passes, efficient fails), SAFE, or HARD.
4. Choose the lowest threshold that rescues the RESCUE tasks without escalating too many LOSS tasks.

## Rules of a tier list

**Admission runs first, and a tier can only narrow.** A target that this key's policy, quality floor, or credentials refuse is skipped. It never comes back. A tier that becomes empty falls to the other tier, and the rationale says so. A spent cadence or budget still serves the local candidate, even when no tier names it. This is the one promise the configuration file itself makes.

**Order is the operator's, and it is the failover order.** The first admitted entry of the picked tier serves the turn. The rest of that tier are its fallbacks, in order.

**A target cannot be named twice**, in one tier or across both. The recipe is refused at load, not deduplicated. A repeat in one tier is a retry of the model that just failed, dressed as a failover. A name in both tiers makes the scorer's choice a no-op that still reads like a decision.

**Boot checks.** The boot refuses a recipe that names a hosted model the catalog cannot route to. Otherwise that tier scores, finds nothing, and hands the turn to the other tier at another price. `local/` names are not checked at boot, because a local worker joins through a different seam. The boot also refuses a key whose cadence or budget promises local service when no local capacity exists for that key. The shipped `examples/control-plane.example.json` has such a promise, so the `roundhouse` binary, which attaches no fleet, refuses it.

## Failover

A fallback fires only on a hosted dispatch that never reached a model:

| Class | Trigger |
|---|---|
| `transport` | Nothing came back: DNS, connect, TLS, or a reset. |
| `timeout` | The origin took longer than the client waited. |
| `status` | HTTP 408, 429, or any 5xx. |

- All attempts share the turn's one deadline and its one budget grant. A provider that fails cannot stack holds or spend the turn's allowance N times.
- A refusal or a content filter is an answer. It is not retried.
- A body that fails after the stream opened is not retried.
- A local target does not fail over.
- Each failed attempt is recorded with its target, its class, and the time it used from the turn deadline.

**A cadence counts attempts, not turns.** `frontier_cadence` is counted at each `Routed` event, and there is one `Routed` per dispatch. A project at `max_frontier: 1, per_turns: 3` that fell forward twice has spent two rations on one turn. This is the conservative direction: a dispatch that failed on the way out did reach for a hosted model. So a provider outage tightens the ration. Size `max_frontier` for attempts if a project expects to fail over.

**Why this failover is Roundhouse's own.** Nothing upstream fails over across targets on a failed dispatch:

- Switchyard at `053a61e` retries the same target on transport errors, timeouts, 408, 429, and 5xx, up to `max_retries + 1` attempts. It falls back to another target only on a context-overflow error. Its tier movement comes from signals and judges, never from transport failure. No Switchyard router, classifier, judge, or retry path reads a refusal.
- OpenRouter offers a `models` fallback field on its inference routes. Roundhouse does not use it. A failover must be a fact in the Roundhouse log, priced by Roundhouse, not a silent substitution before the meter.
- Router.com, as read on 2026-08-20, can switch models until the client receives `200 text/event-stream`. After that, a failure ends the stream. It fails over on rate limits, 5xx, network failures, timeouts, and streams that fail before they start. Roundhouse draws the commit boundary at the same point.

## The cost guard

When the picked tier is `efficient`, the policy compares that tier's head with the admitted `capable` targets, in recipe order. If a capable target quotes strictly less for this turn, the policy serves the first such target. The decision source is `cost_guard`. A tie keeps the tier pick.

- A capable target that is also cheaper is better on function and cost together. This happens when the capable target is warm and the efficient one is cold.
- A `capable` pick is never redirected by cost, because escalation is for function.
- A guarded turn's fallbacks are, in order: the other capable targets that also quote below the efficient head, the efficient tier, and the rest of the capable tier.
- The rationale names both targets and no price, because `explain_last_route` returns the rationale to the calling model. Both quotes are in `considered`.
- The policy holds no session state.

The test `a_deescalation_does_not_move_a_warm_session_onto_a_costlier_cold_target` in `crates/roundhouse-core/src/routing/stage.rs` pins the case. A tests-passed de-escalation there picks the efficient tier, whose cold head quotes $0.12. The warm capable target quotes $0.03. Without the guard, the session moves to the costlier target. A control test with a cheaper efficient head makes sure that the guard does not block a move that saves money.

Two other forms were rejected. A time lease, as Router.com uses, ignores the quote. Ordering by `AffinityPolicy` inside one tier cannot resist a move between tiers, and it did not change that test's result.

Limit: the guard does not price the return trip. A move that is cheaper for this turn can let the old target go cold before the session returns. Pricing the return needs an observed cache deadline per target, which Roundhouse does not have.

### Why a switch is expensive

Let N be the prefix tokens, p the input price, r the cache-read multiplier, and w the cache-write multiplier. Anthropic publishes r = 0.1 (0.025 for Claude Fable 5.1 and Mythos 5.1) and w = 1.25 for the 5-minute TTL, as read on 2026-09-17.

- Staying on a warm target A costs `r * N * p_A`.
- Moving to a cold target B costs `w * N * p_B`. On a target with no write premium (OpenAI dialect) it costs `N * p_B`.
- A move pays on input only if `w * p_B < r * p_A`. B must be 12.5 times cheaper per input token, or 50 times against a model with r = 0.025.
- A return to A after its TTL costs `w * N * p_A`, which is 12.5 to 50 times the warm read.

At about $10 per million input tokens and N = 100,000, the warm read costs $0.025 and the cold write costs $1.25. The output that a cheaper model saves on one routine turn is far below the $1.22 difference. A local target pays in time instead: a cold 100k-token prefill is the whole TTFT. This is why each routing decision carries the cache state in its quote. A per-turn selector that ignores the quote destroys the cache.

## The selection snapshot

Each routing event stores the evidence of its selection:

- the local signals and the extractor version
- the selector settings
- the admitted targets
- the original fallback plan

Failover keeps that snapshot and records each dispatch's target separately. A later configuration change does not rewrite stored evidence. An older record without a snapshot reads as unknown. The snapshot records the result of admission. It does not hold enough to rerun the credential, cadence, and budget checks.

## The client's model field

`/v1/responses` and `/v1/messages` accept a `model` field. Roundhouse does not route on it. Roundhouse chooses the target.

The value is written verbatim on the decision as `declared_baseline`. One consumer reads it: the dashboard's counterfactual. The dashboard prices a local turn against the model the client said it was using, through the capability gate:

- A declared model that passes the gate is priced on the `Declared` basis.
- A declared model that the gate refuses is `Unpriced`, with the model and the band named.
- A value that does not resolve is recorded verbatim, and pricing falls back to inference on the `Inferred` basis. It is never a silent upgrade.

No routing code reads the field. See [Cost and savings](cost-and-savings.md).
