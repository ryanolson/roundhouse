# Choosing a model

This chapter describes two-tier model selection: how a project names an efficient tier and a capable tier, what moves a turn between them, and failover. The routing learner, which learns a serving strategy on top of the tiers, is in [The routing learner](routing-learner.md).

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

The block refuses unknown fields. A misspelled `capible` would otherwise give an empty capable tier, which is legal, and every turn would go to the other tier with no message. A recipe that names no target in either tier is refused.

A project without `"tiers"` routes with `AffinityPolicy`. When any project sets a recipe, every decision of the deployment reports the policy `stage`. See [Routing and the selection service](routing.md#the-routing-policies).

The shape follows Switchyard at `053a61e`, whose tier is a two-variant enum reported as `weak` and `strong`. Roundhouse's `Tier` has the same variants and labels, so a rationale reads the same in two deployments whatever they call their models.

## What moves a turn between tiers

The session's own recent tool results move a turn between the tiers. No model call is involved anywhere in the decision. The scorer is a port of Switchyard's coding-agent scorer (`crates/libsy/src/algorithms/util/stage.rs` at `053a61e`). It reads four numbers from the log that the session fold already holds:

| Dimension | Meaning |
|---|---|
| `severity` | How severe the recent tool errors are. |
| `spinning` | 1 when the session is at least 8 exchanges deep and made no recent write, edit, read, or plan. Else 0. |
| `exploring` | 1 when the session is at least 8 exchanges deep, made no recent write or edit, but did read or plan. Else 0. |
| `production_intensity` | The share of recent operations that were writes or edits. |

`spinning` and `exploring` cannot both be 1. Depth counts task exchanges only, and control calls to Roundhouse's own MCP tools do not count.

```text
raw        = 0.10 * (severity / 0.7 + spinning + exploring - production_intensity)
score      = tanh(5.0 * raw)        positive means capable, negative means efficient
confidence = |score|
```

`tanh` is used rather than a clamp, so a second signal that agrees moves the score less than the first. Read the threshold as a count of signals. About 0.3 escalates on one signal, 0.5 needs about one and a half, and 0.7 needs two that agree. One maximum signal scores about 0.46, just under the default.

`pick_tier` applies four rules in order:

1. **Hard escalate.** If severity is 1.0 or more, pick capable. Source `override`.
2. **Hard de-escalate.** If tests passed, a recent write or edit exists, and severity is 0 or less, pick efficient. Source `tests_passed`.
3. **Scorer.** If confidence is at or above the threshold, pick by the sign of the score. Source `dimensions`.
4. **Fall open.** Otherwise, pick the picker's default tier. Source `ambiguous`.

Switchyard's fourth rule asks an LLM classifier. Roundhouse removes that arm, so a routing decision can never make a model call. Rules 1 and 2 can never both hold, and escalate stays first to match upstream. With no tool history, every undecided turn lands on the default tier.

| Source | Meaning | Narrated to the next model |
|---|---|---|
| `override` | Hard escalate on critical severity. | yes |
| `tests_passed` | Hard de-escalate. | no |
| `dimensions` | The scorer decided. | yes |
| `ambiguous` | Nothing decided. The default tier served, or the picked tier admitted nothing and the other tier served. | no |
| `cost_guard` | An efficient pick yielded to a cheaper capable target. See [The cost guard](#the-cost-guard). | no |
| `strategy` | A learned strategy forced the tier. See [The routing learner](routing-learner.md). | no |

A handoff note says the previous model was in trouble only when a signal said so. See [Validate and steer](validate-steer.md).

### Where the default threshold comes from

Switchyard calibrated the default 0.5 on SWE-Bench Pro Python-75. Every published Switchyard threshold and result uses `efficient_first`. `capable_first` is not benchmarked, so Roundhouse logs Switchyard's uncalibrated warning for such a recipe when the control plane loads. The thresholds do not transfer across model pairs or domains.

Switchyard recalibrates by running the capable model alone over 40 to 75 tasks and the efficient model over about 20. It sorts tasks into RESCUE (capable fails, efficient passes), LOSS (capable passes, efficient fails), SAFE, and HARD. It takes the lowest threshold that rescues the RESCUE tasks without escalating many LOSS tasks. See [Upstream dependencies](../development/upstream.md) for the divergences from Switchyard.

## Rules of a tier list

- **Admission runs first, and a tier can only narrow.** A target that this key's policy, quality floor, or credentials refuse is skipped and never comes back. A tier that becomes empty falls to the other tier, and the rationale says so. A spent cadence or budget still serves the local candidate, even when no tier names it. This is the one promise the configuration file itself makes.
- **Order is the operator's, and it is the failover order.** The first admitted entry of the picked tier serves the turn. The rest of that tier are its fallbacks.
- **A target cannot be named twice**, in one tier or across both. The load refuses it and does not deduplicate. A repeat in one tier is a retry of the model that just failed, dressed as a failover. A name in both tiers makes the scorer's choice a no-op.
- **The boot checks the recipe against the catalog.** It refuses a hosted model that the catalog cannot route to. Otherwise that tier would find nothing and hand the turn to the other tier at another price. It does not check `local/` names, because a local worker joins through a different seam. It also refuses a key whose cadence or budget promises local service when that key has no local capacity. The shipped `examples/control-plane.example.json` makes that promise, so the `roundhouse` binary, which attaches no fleet, refuses it.

## Failover

A fallback fires only on a hosted dispatch that never reached a model:

| Class | Trigger |
|---|---|
| `transport` | Nothing came back: DNS, connect, TLS, or a reset. |
| `timeout` | The origin took longer than the client waited. |
| `status` | HTTP 408, 429, or any 5xx. |

- All attempts share the turn's one deadline and one budget grant, so a failing provider cannot spend the turn's allowance N times.
- A refusal or a content filter is an answer and is not retried.
- A body that fails after the stream opened is not retried. Deltas are already durable, and a second attempt would append a second answer to one response.
- A local target does not fail over.
- Each failed attempt is recorded with its target, its class, and the time it used from the turn deadline.

**A cadence counts attempts, not turns.** `frontier_cadence` is counted at each `Routed` event, and there is one per dispatch. A project at `max_frontier: 1, per_turns: 3` that fell forward twice has spent two rations on one turn. This is the conservative direction, because a dispatch that failed on the way out did reach for a hosted model. A provider outage tightens the ration, so size `max_frontier` for attempts if a project expects to fail over.

**Why this failover is Roundhouse's own.** Switchyard at `053a61e` retries the same target on transport errors, timeouts, 408, 429, and 5xx. It falls back to another target only on a context-overflow error. OpenRouter offers a `models` fallback field, which Roundhouse does not use. A failover must be a fact in the Roundhouse log, priced by Roundhouse, not a silent substitution before the meter.

Router.com, as read on 2026-08-20, switches models until the client receives `200 text/event-stream`. Roundhouse draws the commit boundary at the same point.

## The cost guard

When the picked tier is `efficient`, the policy compares that tier's head with the admitted `capable` targets, in recipe order. If a capable target quotes strictly less for this turn, the policy serves the first such target with source `cost_guard`. A tie keeps the tier pick. A capable target that is also cheaper wins on function and cost together. This happens when it is warm and the efficient one is cold. A `capable` pick is never redirected by cost, because escalation is for function.

- A guarded turn's fallbacks, in order, are the cheaper capable targets, the efficient tier, and the rest of the capable tier.
- The rationale names both targets and no price, because `explain_last_route` returns the rationale to the calling model. Both quotes are in `considered`.

`a_deescalation_does_not_move_a_warm_session_onto_a_costlier_cold_target` in `crates/roundhouse-core/src/routing/stage.rs` pins the case: a cold efficient head quotes $0.12 and the warm capable target quotes $0.03. Two other forms were rejected. A time lease, as Router.com uses, ignores the quote. Ordering by `AffinityPolicy` inside one tier cannot resist a move between tiers.

Limit: the guard does not price the return trip. A move that is cheaper for this turn can let the old target go cold before the session returns. Pricing the return needs an observed cache deadline per target, which Roundhouse does not have.

### Why a switch is expensive

Let N be the prefix tokens, p the input price, r the cache-read multiplier, and w the cache-write multiplier. Anthropic publishes r = 0.1 (0.025 for Claude Fable 5.1 and Mythos 5.1) and w = 1.25 for the 5-minute TTL, as read on 2026-09-17.

Staying on a warm target A costs `r * N * p_A`. Moving to a cold target B costs `w * N * p_B`, or `N * p_B` with no write premium (OpenAI dialect). A move pays on input only if `w * p_B < r * p_A`. B must be 12.5 times cheaper per input token, or 50 times against a model with r = 0.025. A return to A after its TTL costs `w * N * p_A`.

At about $10 per million input tokens and N = 100,000, the warm read costs $0.025 and the cold write costs $1.25. The output that a cheaper model saves on one routine turn is far below the $1.22 difference. A local target pays in time instead, because a cold 100k-token prefill is the whole TTFT. So each routing decision carries the cache state in its quote, and a per-turn selector that ignores the quote destroys the cache.

## The selection snapshot

Each routing event stores the evidence of its selection. That is the local signals and extractor version, the selector settings, the admitted targets, and the original fallback plan. Failover keeps that snapshot and records each dispatch's target separately. A later configuration change does not rewrite it, and an older record without a snapshot reads as unknown. The snapshot holds the result of admission, not enough to rerun the credential, cadence, and budget checks.

## The client's model field

`/v1/responses` and `/v1/messages` accept a `model` field. Roundhouse does not route on it. The value is written verbatim on the decision as `declared_baseline`. Only the dashboard's counterfactual reads it, through the capability gate:

- A declared model that passes the gate is priced on the `Declared` basis.
- A declared model that the gate refuses is `Unpriced`, with the model and the band named.
- A value that does not resolve is recorded verbatim, and pricing falls back to inference on the `Inferred` basis. It is never a silent upgrade.

See [Cost and savings](cost-and-savings.md).
