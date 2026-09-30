# Cost and savings

This chapter explains how Roundhouse counts tokens and dollars, and what each savings figure claims. The endpoints, the JSON fields, and the configuration are in [Metrics and the dashboard](../operations/metrics.md).

## Metrics are a projection of the log

Token counts, dollars, and the savings figures are folded out of the same append-only log that carries the conversation and the routing decisions (see [Sessions and the event log](sessions.md)). There is one write path. So the dashboard cannot disagree with the audit trail that it summarizes. A fold of the stored events gives exactly what the running process reports, and a test holds this equivalence.

The fold (`MetricsFold`) is one accumulator, keyed by principal and model. There is no deployment-wide copy. A deployment answer is the sum of the per-principal rows (`Counters::absorb`), computed when the snapshot is read. Two accumulators fed at two sites drift apart the first time one path returns early. That drift shows as a project bill that disagrees with the deployment bill.

The counters hold no money. Prices are configuration, and dollars folded into counters freeze whichever rate card was loaded. The snapshot applies prices when it is read.

The fold is idempotent per session. `apply` takes the watermark of the session and ignores any event with `seq <= watermark`. So a live feed and a rebuild from the log can run together. The watermark map gains one entry per session and never loses one. It cannot be pruned, because it is the idempotency guarantee.

One production path feeds the fold. `Engine::run_turn` opens the session with an observer (`Session::open_observed`), which replays the log in batches of 1024 events and then feeds every commit. The MCP control surface reads with no observer, because it takes no lease.

Nothing replays sessions into the fold at boot or outside a turn. So after a restart, the dashboard shows only the sessions that a turn opened again. Metrics are per process. See [Metrics across nodes](../operations/metrics.md#metrics-across-nodes).

## Tokens

The tokens of each model split into input, cached input, output, and reasoning. Cached input is part of input, and reasoning is part of output. Both providers report them that way. Storing them as separate addends counts every total twice, including the total billed to a client.

Rows roll up twice:

- by **provider** (Anthropic, OpenAI, the local fleet), because a rate card attaches to a provider,
- by **serving mode** (local Dynamo or a remote endpoint), because the savings argument depends on it.

## What "dollars saved" claims

The dashboard shows three figures. They are not equally solid, so the dashboard never merges them into one.

| Figure | Field | Basis |
|---|---|---|
| Spent on hosted endpoints | `savings.frontier_spend_usd` | **Split.** Provider-reported tokens at the rate card, and our own token counts at the rate card when a provider reported nothing, as two parts. |
| Provider cache discount | `savings.cache_savings_usd` | **Measured**, even at partial coverage. An unreported call records zero cache reads, not a guess, so it contributes nothing. |
| Served locally instead | `savings.routing_savings_usd` | **Estimated.** A counterfactual: what local traffic costs on a comparable hosted model, less its local capacity cost when the catalog sets a price. |

`savings.total_usd` is `cache_savings_usd + routing_savings_usd`.

### Hosted spend has two parts

The fold keeps provider-reported tokens and self-counted tokens in separate accumulators, and prices each. Pricing is linear in tokens, so this costs nothing. `frontier_spend_usd` is the sum of `frontier_spend_measured_usd` and `frontier_spend_estimated_usd`.

A call-weighted coverage ratio is not a substitute. One unreported turn of 200k tokens beside a reported turn of 2k tokens is 50% coverage by calls and 1% by tokens. The token figure tracks the money. The snapshot reports both. `coverage_token_fraction` is the one to quote next to a dollar figure.

### Forwarded subscription seats are not money

`seat_tokens` counts the traffic served through a **forwarded subscription seat**. Roundhouse holds no rate card for a seat. The catalog price describes what Roundhouse pays on its own key, which is a counterfactual and not a bill. The spend ledger refuses to draw against a seat, and the dashboard refuses in the same way:

- A pass-through turn counts in every token figure and is priced in none of them.
- The share of the seat is published as a token count, so a deployment can see the traffic it carries.
- The decision of the turn records which case applies. So the ledger, a successor process that repairs a lost settle, and the dashboard all read one recorded fact.

## Pricing local traffic: the correlary

A local worker bills nobody. Its saving is the difference against a call that did not happen. So the saving must name a hosted model that the local model stands in for. That stand-in is the **correlary** of the local model.

The pricing code (`crates/roundhouse-core/src/metrics/pricing.rs`) chooses a correlary in this order:

1. **Declared by the operator.** A `correlaries` entry in the catalog. This is the only kind of answer that can account for an evaluation or a procurement decision, so it always wins. It is exempt from the capability gate. A declaration that names a model with no rate card is `Unpriced`, and it never falls back to inference.
2. **Declared by the client.** The `model` field of the request, recorded as the declared baseline of the decision (see [Choosing a model](model-selection.md)). It passes through the capability gate. A baseline that the gate refuses is `Unpriced`, and the reason names the model and the band. A baseline that names no known model falls through to inference on the `Inferred` basis. It is never a silent upgrade.
3. **Inferred** from traffic shape. Only hosted models that pass the capability gate, and that this deployment called, are candidates.

The traffic shape is the output ratio, the cache ratio, the reasoning ratio, and log-scaled mean prompt and answer lengths. The magnitude terms are log-scaled because agentic prompt lengths span orders of magnitude. On a linear scale, context length alone decides the nearest model.

### The capability gate

Shape alone must never select a correlary. A 7B model and a frontier reasoning model that do the same summarization job have almost identical traffic shapes. If the first is priced against the second, the reported saving grows by an order of magnitude. The number also looks better as the comparison gets more absurd.

So a candidate must first pass a **capability gate**: its `quality_prior` must be within `capability_band` of the quality prior of the local model. The local prior comes from `local_quality` in the catalog, or `default_local_quality`. Among the models that pass, shape decides. When no model passes, the snapshot produces no correlary. That traffic contributes nothing to the saving, and the dashboard reports it as unpriced.

`quality_prior` is configuration, not measurement. The gate is only as honest as the numbers it compares. See [Sourcing prices and quality priors](#sourcing-prices-and-quality-priors).

### Like-for-like counterfactual

The counterfactual uses the same token counts, including the same cached fraction, at the rates of the reference model. It is not "what if the request went cold". That assumes that the hosted cache never warmed, and it roughly doubles the figure on long sessions. A deployment that routed there all along has a warm prefix cache about as often as the local one.

### The correlary depends on the scope

The correlary of a local model is inferred from the hosted traffic of the scope itself (`ScopeView::frontier_shapes`). A report scoped to one tenant infers from the prompts of that tenant only. The shapes of the deployment are not used, because then the price of one tenant moves when a neighbor changes its workload.

As a result, two nodes that serve different mixes can choose different reference models for one local model. Their `routing_savings_usd` values do not measure the same thing, and their sum is not a saving. Any merge of metrics across nodes must happen at the counters, before the pricing step, never on finished snapshots.

### The cross-check at decision time

The snapshot also carries `routing_savings_at_decision_usd`. It is the cheapest hosted quote that the router saw when it chose local, less the local quote that the turn was served on. It comes from the decision record, not from a rate card.

Two independent estimates of one counterfactual land near each other when both models are correct. When they do not, one model is wrong. That disagreement is worth more than either number alone. So the cross-check is reported beside the total, never added into it.

One disagreement is expected. A past decision can quote local at a different `local_capacity_price`, or at none (a gross $0 quote). `routing_savings_usd` nets the same turn at the current price. Over such history, the two figures differ by the difference in capacity cost of those turns.

## Local capacity cost

Local GPU time is not free. A saving that ignores it overstates what local serving saved. When the catalog sets `local_capacity_price` (see [Configure providers and the catalog](../guides/catalog.md#local-capacity)):

- The snapshot reports `savings.local_capacity_usd`, and `capacity_usd` on each local row.
- Capacity cost is uncached prompt tokens at the input rate plus output tokens at the output rate.
- Cached local tokens are free, because a local cache hit skips the prefill that the price stands for. A serving plane that reports no cache reads is charged its whole prompt, which errs toward more cost.
- The capacity cost covers every local turn, including the turn of a forwarded seat, because the hardware belongs to this deployment.
- `routing_savings_usd` subtracts the capacity cost of the same priceable turns whose saving it counts. It does this only on rows with a priced correlary. It can be negative, and it is not clamped. A negative value means that local cost more than the hosted alternative.
- `routing_savings_at_decision_usd` already carries the price, because the router quoted local at it.

Without a price:

- `local_capacity_price` is `null`, both capacity fields are `null`, and `routing_savings_usd` is the gross counterfactual.
- On a priced deployment, the `capacity_usd` of the local serving mode is `0.0` before any local traffic. So `null` means unpriced, and only that.

The dashboard says which case applies. It shows the rate, and it puts the capacity spend of each local row beside what the row avoided, or "capacity unpriced". The Relay optimization summary publishes the same figures per turn (see [NeMo Relay formats](../operations/relay-formats.md)).

## Provider-reported prices

Some providers attach a price to each response. OpenRouter includes `usage.cost` ("Cost in credits") on every response, as read on 2026-08-24. Its documentation does not state that 1 credit equals 1 USD.

Roundhouse follows one rule: a provider-reported price is a price, never a token count. The Responses decoder reads `usage.cost` into `provider_reported_cost` on the frontier `Done` chunk. The engine records it on `ResponseCompleted` as `provider_reported_cost_usd`. It does not enter `Usage`. In `Usage`, it becomes a number from outside our rate card, in the column that the savings figures come from.

The value is published beside the other figures and added into none of them:

- `savings.provider_reported_usd` in the metrics snapshot,
- `provider_reported_usd` in the reconciliation view, with its own stamp.

`null` means that no call in scope reported a price. It does not mean free. The reconciliation view keeps this column out of `drift_usd`. It is the only column that this deployment did not compute, so adding it turns a cross-check into a self-check.

Roundhouse does not call OpenRouter's post-hoc ledger (`GET /api/v1/generation?id=...`), and it does not read `openrouter_metadata` or `X-Provider-Name`. So there is no automatic reconciliation against the bill of a provider.

## Usage has to be asked for

A streaming OpenAI-compatible request returns **no usage object** unless the request sets `stream_options.include_usage`. This is true of the OpenAI API, vLLM, SGLang, and the Dynamo frontend. Anthropic always reports usage, but it splits it. Input and cache-read counts arrive on `message_start`, and output tokens on the final `message_delta`. A client that reads only the delta records zero input and no cache reads.

Unaccounted calls are the worst failure here, because they are silent. They fold in as zero tokens for zero dollars. Zero dollars on a hosted model looks the same as a saving. So the dashboard looks its best exactly when its instrumentation is broken. Roundhouse has two defenses, and it needs both:

- **Ask for usage.** `WireProtocol::enforce_usage_reporting` changes an outbound request to ask for accounting. It only adds, and it never overrides a field that the caller set. A silent disagreement with a request is worse than an unaccounted call, because an unaccounted call is at least marked. The dialect travels on the `FrontierQuote`, because that is the only argument a `FrontierClient` gets. The engine holds one client for providers whose transports have nothing in common, so a client cannot look the dialect up.
- **Mark estimates.** A call that still returns without usage is recorded as `Accounting::Estimated`. Input comes from the prompt that Roundhouse tokenized and routed on. Output comes from our own tokenizer over the received text. Cached input is zero, because nothing observable shows what a remote cache did. These calls are priced separately, as the estimated part of hosted spend.

NeMo Relay at commit `c37b551` does not add `include_usage`, so its gateway records zero-token, zero-dollar calls on streaming upstreams. Switchyard at commit `5341f71` uses the same add-never-override rule as Roundhouse.

The direction of an estimate is unknown, not low. A tokenizer mismatch can go either way. Only the cache discount is safely understated, because its estimated part is zero.

The Anthropic client folds `message_start` (input, cache read, cache write) and the final `message_delta` (output) into one `Done`. `Usage` and `FrontierChunk::Done` carry `cache_write_tokens`, a measured cache-write count. A stream that ends before `message_stop` yields no `Done`. The engine then books its estimated usage and marks it as estimated. A synthesized zero-token `Done` reads as a saving, which is the failure that these rules prevent. Records without the field deserialize with the count at 0.

### One settle point

The engine settles every admitted turn at one point, after the response terminates. It does not meter usage in a stream wrapper. A Responses client can drop the stream when `response.completed` arrives, and a stream wrapper then never resumes. For this reason, Switchyard (commit `6aed489`) commits usage before it yields the terminal event. Every pass-through meter has the same failure.

## Anthropic prompt cache markers

The cache discount depends on where a request puts its `cache_control` markers. These facts about Anthropic prompt caching are from its documentation, as read on 2026-09-16 and 2026-09-17:

- The cache covers the prefix in the order `tools`, `system`, `messages`, up to a `cache_control` breakpoint. A request can have at most 4 breakpoints, and a fifth is a 400 error.
- Anthropic caches nothing without an explicit marker.
- The minimum lifetime is five minutes, or one hour with `ttl: "1h"`. A use refreshes the lifetime. There is no manual clear.
- A change to tools invalidates the whole hierarchy. A change to `tool_choice`, images, thinking, or effort can invalidate part or all of it.
- The lookup examines at most 20 block positions back from a marker (`CACHE_LOOKBACK_BLOCKS`).
- Messages has no `prompt_cache_key` field.

What this means for Roundhouse:

- A zero `cache_read_input_tokens` can mean a short prompt, a different prefix, a changed configuration, expiry, or a cold provider. It never proves compaction.
- A cache-read miss lowers the reuse estimate of that target only. It never marks the conversation cold everywhere.
- Roundhouse observations can lower KV retention priority. They cannot revoke a live or shared block, because KV eviction is the decision of the worker.

### Where the markers go

The marker plan is a pure function (`cache_markers::plan` in `crates/roundhouse-fleet/src/anthropic_messages/cache_markers.rs`):

- The conversation marker goes on the penultimate block. That block ends the prefix that the previous turn sent, so the marker reads the entry of that turn and extends it. With fewer than two segments, there is no marker.
- Client tool definitions carry their own markers, and they share the allowance of 4. When the forwarded tools use the whole allowance, Roundhouse places no block marker. The tool marker caches the largest stable block, and sending both is a 400 error that costs the turn.
- An append of 20 or more items between two turns puts the previous write out of reach, although the prefix bytes are identical. So when two slots are free, the request also marks the block that the previous request to this target marked.
- The penultimate marker wins the last free slot. A lone reach-back marker reads this turn but never moves the entry forward, so every later turn pays plain input on a longer tail. A lone penultimate marker pays one write and makes every later turn a hit.
- The reach-back marker is dropped when it is not strictly earlier than the penultimate block (the conversation stopped being append-only), or when it is already inside the window.

Limit: this is block arithmetic that tests prove. No live request has shown `cache_read_input_tokens > 0` after an append of 20 items.

### The marker placement is a recorded fact

`Routed` carries `DecisionRecord::block_marker`. The value is `BlockMarker::Unplaced`, or `Placed { segment, prefix_tokens }`. `prefix_tokens` is the toolbox count (without `tool_choice`) plus the tokens of the items through the marked block, in `isl_tokens` units. The session fold copies it into `TargetState::last_block_marker`.

The engine builds the `FrontierQuote` before it writes `Routed`, asks `FrontierQuote::marker_placement()`, and sends the same quote. `marker_placement` and `body()` both call `cache_markers::plan`, so the recorded placement and the sent placement cannot differ.

It is a fact and not a count, for this reason. A history that carried four client tool markers renders the same item count as one that carried none, but the first placed no block marker. An inference of `n - 2` from the count made the next request reach back for a cache write that never happened. With the fact, four tool markers record `Unplaced`, and the next request places no reach-back marker. `PreviousMarker::Inferred` is used only for records written without the fact.

The router predicts warmth only through the recorded marker prefix. A request that placed no marker predicts 0 cached tokens, toolbox included.

Negative result: the plan never marks the final segment. A ledger that predicts the whole previous request as warm prices the previous final item as a cache read. The provider bills it as a cache write. Take a 30k-token `tool_result` as the final item, on the Claude rate card. The quote was then low by about $0.10 per turn, and the cost guard decided on that quote. Two remedies exist:

- Mark the last block as well. This uses one more of the four breakpoints.
- Predict warmth only through the recorded marker. This makes the quote conservative without a change to the wire.

Roundhouse uses the second remedy. Whether marking the last block pays is a question for a live cache measurement.

## Classifier evaluation costs

Classifier calls are a separate economy from serving. A classifier call bills a service that this deployment chose to consult, at a price that the call itself recorded. Serving traffic is priced by the current catalog. So the snapshot keeps the two apart in `evaluation`, and classifier tokens never enter the serving counters. `observed_cost` adds them and names what it added.

The rules:

- `evaluation.measured_usd` uses the rates that each call recorded. It is not a provider invoice, and catalog changes do not price it again.
- Cost and settlement are separate observations. An unconfirmed settlement does not erase recorded usage or cost. A later repair updates the acknowledgement, and it adds no call and no charge.
- A call with unknown usage settles at the estimate of its grant when the request can have reached the service. `evaluation.estimated_usd` reports those booked estimates once per call, whether the settle was acknowledged, repaired, or is still open. `measured_usd` never includes them. The settlement `committed_usd` includes both.
- Missing usage and unanswered intents keep evaluation cost incomplete, even when an estimate stands in, because an estimate is not what the service billed.
- Replayed events and duplicate results count once per session and call identity.

### Settlement by call identity

Evaluation calls settle by call identity, so the order of completion does not discard charges. The rejected alternative settled on the ordered watermark of the serving session. Two classifier calls with intent seqs 10 and 20 that complete in reverse order then committed only the newer $0.00042, not the combined $0.00070. A regression run over loopback HTTP, with a real memory spend ledger, showed this.

Time-limited deduplication was also rejected. After a hold TTL elapses, a completed call becomes chargeable again. So the memory and Redis ledgers keep one completed identity per call across budget resets, with no expiry and no compaction. A new attempt needs a fresh identity. A settlement replay uses the original identity.

Repair puts every settlement that owes money before zero-dollar entries, in arrival order within each group. One predicate, `owes`, decides both that order and whether a session holds back its next classification ticket.

### Tier agreement is not a reward

`evaluation.agreement` compares the served tier with the tier answer of the classifier. The served tier is the recipe tier of the target that served the turn. So a turn that the cost guard moved counts as `capable`, although the scorer picked `efficient`. Agreement is not a quality score, and no component reads it as a reward. The learner reads the tier answers only as a small cold-start prior (see [The routing learner](routing-learner.md#cold-start-prior-from-classifier-tier-answers)).

## Sourcing prices and quality priors

Rate cards never go in source. Rate cards change, and a constant in a binary goes stale silently. `ROUNDHOUSE_CATALOG` is the mechanism, and `crates/roundhouse-fleet/src/frontier.rs` states the rule. The router optimizes against a price, and the dashboard reports a price. These must be the same number, so both come from one catalog file.

openrouter.ai publishes comparable per-model prices across providers. It is one place where the hosted equivalents of an open-weights model that Roundhouse serves locally can be priced against each other. It also publishes per-model intelligence indexes and benchmark scores. Those are the source for `quality_prior`, the number that the capability gate compares. A prior from a published index makes the gate defensible, not asserted. The `import-benchmarks` tool does this import (see [Configure providers and the catalog](../guides/catalog.md#source-quality-priors)).

Three cautions apply to an import:

- **OpenRouter prices a route to a model.** The same model appears at several prices, one per upstream provider. Pick one deliberately. The catalog refuses two entries for one `(provider, model)`, because the router and the dashboard can resolve that ambiguity in different ways.
- **Normalize and record the index.** Normalize an index to the `0.0..=1.0` scale of `quality_prior`. Record which index and which snapshot date it came from. An unversioned score silently ranks models again when the upstream leaderboard moves.
- **Keep price and capability apart.** A price is not a capability claim, and a benchmark score is not a price. They stay separate fields from separate columns. That stops a cheap lookup from inflating the one number that the dashboard is judged by.

## Why the figures stay separate

Roundhouse routes to co-optimize function, cost, and time to solution. A savings figure must never claim one of them without measuring the others, and it must keep cache savings apart from routing savings. A headline that mixes same-model tier price differences with cross-model routing, and names no baseline, cannot be checked. Ramp's Router headline "40% average cost cut" (ramp.com/router, read 2026-08-20) is an example of that kind of figure.
