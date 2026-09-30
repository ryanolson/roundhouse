# Cost and savings

This chapter explains how Roundhouse counts tokens and dollars, and what each savings figure claims. The endpoints, the JSON fields, and the configuration are in [Metrics and the dashboard](../operations/metrics.md).

## Metrics are a projection of the log

Token counts, dollars, and savings are folded out of the same append-only log that carries the conversation and the routing decisions (see [Sessions and the event log](sessions.md)). There is one write path, so the dashboard cannot disagree with the audit trail. A test holds that a fold of the stored events gives exactly what the running process reports.

The fold (`MetricsFold`) is one accumulator, keyed by principal and model. A deployment answer is the sum of the per-principal rows (`Counters::absorb`), computed when the snapshot is read. Two accumulators fed at two sites would drift apart the first time one path returned early. The counters hold no money, because dollars folded into counters freeze whichever rate card was loaded. The snapshot applies prices when it is read.

The fold is idempotent per session: it ignores any event with `seq` at or below the watermark of the session. A turn replays its session into the fold when it opens the session. Nothing replays sessions at boot, so after a restart the dashboard shows only the sessions that a turn opened again (see [Metrics across nodes](../operations/metrics.md#metrics-across-nodes)).

## Tokens

The tokens of each model split into input, cached input, output, and reasoning. Cached input is part of input, and reasoning is part of output, because both providers report them that way. Storing them as separate addends would count every total twice. Rows roll up by **provider** (a rate card attaches to a provider) and by **serving mode** (local Dynamo or a remote endpoint).

## What "dollars saved" claims

The dashboard shows three figures. They are not equally solid, so it never merges them.

| Figure | Field | Basis |
|---|---|---|
| Spent on hosted endpoints | `savings.frontier_spend_usd` | **Split.** Provider-reported tokens at the rate card, and our own token counts at the rate card when a provider reported nothing, as two parts. |
| Provider cache discount | `savings.cache_savings_usd` | **Measured**, even at partial coverage. An unreported call records zero cache reads, not a guess. |
| Served locally instead | `savings.routing_savings_usd` | **Estimated.** A counterfactual: what local traffic costs on a comparable hosted model, less its local capacity cost when the catalog sets a price. |

`savings.total_usd` is `cache_savings_usd + routing_savings_usd`.

### Hosted spend has two parts

The fold keeps provider-reported tokens and self-counted tokens in separate accumulators and prices each. Pricing is linear in tokens, so this costs nothing. A call-weighted coverage ratio is not a substitute: one unreported turn of 200k tokens beside a reported turn of 2k tokens is 50% coverage by calls and 1% by tokens. The token figure tracks the money. So quote `coverage_token_fraction` next to a dollar figure.

### Forwarded subscription seats are not money

`seat_tokens` counts the traffic served through a **forwarded subscription seat**. Roundhouse holds no rate card for a seat. The catalog price describes what Roundhouse pays on its own key, which is a counterfactual and not a bill. So the spend ledger and the dashboard both refuse to price a seat: a pass-through turn counts in every token figure and in no dollar figure. The decision of the turn records which case applies. The ledger, a successor process that repairs a lost settle, and the dashboard read that one fact. A seat's local turn is not a routing saving, because the hosted call that it passed over was the caller's seat.

## Pricing local traffic: the correlary

A local worker bills nobody. Its saving is the difference against a call that did not happen, so the saving must name a hosted model that the local model stands in for. That stand-in is the **correlary** of the local model. The pricing code (`crates/roundhouse-core/src/metrics/pricing.rs`) chooses it in this order:

1. **Declared by the operator.** A `correlaries` entry in the catalog. It is the only answer that can account for an evaluation or a procurement decision. So it always wins and is exempt from the capability gate. A declaration that names a model with no rate card is `Unpriced`. It never falls back to inference.
2. **Declared by the client.** The `model` field of the request, recorded as the declared baseline (see [Choosing a model](model-selection.md)). It passes through the capability gate, and a baseline that the gate refuses is `Unpriced`. A baseline that names no known model falls through to inference.
3. **Inferred** from traffic shape: the output ratio, the cache ratio, the reasoning ratio, and log-scaled mean prompt and answer lengths. Only hosted models that pass the gate and that this deployment called are candidates. The lengths are log-scaled because agentic prompt lengths span orders of magnitude, and on a linear scale context length alone would decide the nearest model.

### The capability gate

Shape alone must never select a correlary. A 7B model and a frontier reasoning model that do the same summarization job have almost identical traffic shapes. Pricing the first against the second would grow the reported saving by an order of magnitude, and the number would look better as the comparison got more absurd.

So a candidate must first pass a **capability gate**. Its `quality_prior` must be within `capability_band` (default `0.10`) of the prior of the local model, from `local_quality` or `default_local_quality` in the catalog. Among the models that pass, shape decides. When none passes, the traffic is reported as unpriced and adds nothing to the saving. `quality_prior` is configuration, not measurement, so the gate is only as honest as its numbers (see [Sourcing prices and quality priors](#sourcing-prices-and-quality-priors)).

The counterfactual is like-for-like. It uses the same token counts, including the same cached fraction, at the rates of the reference model. "What if the request went cold" would assume that the hosted cache never warmed. That roughly doubles the figure on long sessions.

### The correlary depends on the scope

The correlary is inferred from the hosted traffic of the scope itself (`ScopeView::frontier_shapes`). A report scoped to one tenant infers from that tenant's prompts only. Otherwise the price of one tenant would move when a neighbor changes its workload. So two nodes that serve different mixes can choose different reference models for one local model, and the sum of their `routing_savings_usd` values is not a saving. Any merge across nodes must happen at the counters, before the pricing step, never on finished snapshots.

### The cross-check at decision time

The snapshot also carries `routing_savings_at_decision_usd`: the cheapest hosted quote that the router saw when it chose local, less the local quote that the turn was served on. It comes from the decision record, not from a rate card. Two independent estimates of one counterfactual land near each other when both models are correct, and their disagreement is worth more than either number. So it is reported beside the total, never added into it. One disagreement is expected: a past decision can quote local at a different `local_capacity_price`, or at none, while `routing_savings_usd` nets the same turn at the current price.

## Local capacity cost

Local GPU time is not free, and a saving that ignores it overstates what local serving saved. When the catalog sets `local_capacity_price` (see [Configure providers and the catalog](../guides/catalog.md#local-capacity)), the snapshot reports `savings.local_capacity_usd` and `capacity_usd` on each local row:

- Capacity cost is uncached prompt tokens at the input rate plus output tokens at the output rate. Cached local tokens are free. A serving plane that reports no cache reads is charged its whole prompt.
- It covers every local turn, including a forwarded seat's, because the hardware belongs to this deployment.
- `routing_savings_usd` subtracts the capacity cost of the same priceable turns, only on rows with a priced correlary. It can be negative and is not clamped. A negative value means that local cost more than the hosted alternative.
- `routing_savings_at_decision_usd` already carries the price, because the router quoted local at it.

Without a price, `local_capacity_price` and both capacity fields are `null`, and `routing_savings_usd` is the gross counterfactual. On a priced deployment, the local serving mode's `capacity_usd` is `0.0` before any local traffic, so `null` means unpriced, and only that. The dashboard shows the rate and each local row's capacity spend beside what the row avoided, or "capacity unpriced". The Relay optimization summary publishes the same figures per turn (see [NeMo Relay formats](../operations/relay-formats.md)).

## Provider-reported prices

OpenRouter includes `usage.cost` ("Cost in credits") on every response, as read on 2026-08-24. Its documentation does not state that 1 credit equals 1 USD. A provider-reported price is a price, never a token count. The Responses decoder reads it into `provider_reported_cost` on the frontier `Done` chunk, and the engine records it on `ResponseCompleted` as `provider_reported_cost_usd`. The Anthropic decoder reports none. The value never enters `Usage`, because there it would become a number from outside our rate card in the column that the savings figures come from.

It is published as `savings.provider_reported_usd` and in the reconciliation view, and added into no other figure. `null` means that no call in scope reported a price, not that the calls were free. The reconciliation view keeps it out of `drift_usd`. It is the only column that this deployment did not compute, so adding it would turn a cross-check into a self-check. Roundhouse does not call OpenRouter's post-hoc ledger (`GET /api/v1/generation?id=...`), so there is no automatic reconciliation against a provider's bill.

## Usage has to be asked for

A streaming OpenAI Chat Completions request returns **no usage object** unless it sets `stream_options.include_usage`. This holds for the OpenAI API, vLLM, SGLang, and the Dynamo frontend. The Responses API reports usage on `response.completed` unasked. Anthropic always reports usage, but it splits it: input and cache-read counts arrive on `message_start`, and output tokens on the final `message_delta`. A client that reads only the delta records zero input and no cache reads.

Unaccounted calls are the worst failure here, because they are silent. They fold in as zero tokens for zero dollars, which looks the same as a saving. The dashboard would look its best exactly when its instrumentation is broken. Roundhouse has two defenses, and it needs both:

- **Ask for usage.** `WireProtocol::enforce_usage_reporting` runs on every outbound body. It adds `stream_options.include_usage` to a streaming Chat Completions request, only adds, and never overrides a field that the caller set. A silent disagreement with a request is worse than an unaccounted call. Both shipped dialects (Responses and Messages) report usage unasked, so the function changes nothing for them. The catalog refuses `openai_chat_completions` at boot, because this build has no client for it. The guard exists for the dialect that Dynamo and vLLM speak.
- **Mark estimates.** A call that still returns without usage is recorded as `Accounting::Estimated`. Input comes from the prompt that Roundhouse tokenized and routed on, and output from our tokenizer over the received text. Cached input is zero, because nothing observable shows what a remote cache did. These calls are priced as the estimated part of hosted spend.

The direction of an estimate is unknown, not low, because a tokenizer mismatch can go either way. Only the cache discount is safely understated. A stream that ends before `message_stop` yields no Anthropic `Done`, so the engine books estimated usage and marks it. A synthesized zero-token `Done` would read as a saving.

NeMo Relay at commit `c37b551` does not add `include_usage`, so its gateway records zero-token, zero-dollar calls on streaming upstreams. Switchyard at commit `5341f71` uses the same add-never-override rule as Roundhouse.

The engine settles every admitted turn at one point, after the response terminates, and does not meter usage in a stream wrapper. A Responses client can drop the stream when `response.completed` arrives, and a wrapper then never resumes. Switchyard (commit `6aed489`) commits usage before it yields the terminal event for the same reason.

## Anthropic prompt cache markers

The cache discount depends on where a request puts its `cache_control` markers. These facts are from the Anthropic documentation, as read on 2026-09-16 and 2026-09-17:

- The cache covers the prefix in the order `tools`, `system`, `messages`, up to a `cache_control` breakpoint. A request can have at most 4 breakpoints, and a fifth is a 400 error.
- Anthropic caches nothing without an explicit marker. The minimum lifetime is five minutes, or one hour with `ttl: "1h"`.
- A change to tools invalidates the whole hierarchy. A change to `tool_choice`, images, thinking, or effort can invalidate part or all of it.
- The lookup examines at most 20 block positions back from a marker (`CACHE_LOOKBACK_BLOCKS`). Messages has no `prompt_cache_key` field.

So a zero `cache_read_input_tokens` can mean a short prompt, a changed prefix or configuration, expiry, or a cold provider. It never proves compaction.

### Where the markers go

The marker plan is a pure function (`cache_markers::plan` in `crates/roundhouse-fleet/src/anthropic_messages/cache_markers.rs`). The placement rules are in [Routing and the selection service](routing.md#anthropic-cache-markers). Two details decide the discount:

- The penultimate marker wins the last free slot. A lone reach-back marker reads this turn but never moves the entry forward, so every later turn would pay plain input on a longer tail.
- The reach-back marker is dropped when it is not strictly earlier than the penultimate block, or is already inside the 20-block window.

Limit: this is block arithmetic that tests prove. No live request has shown `cache_read_input_tokens > 0` after an append of 20 items.

The marker placement is a recorded fact. `Routed` carries `DecisionRecord::block_marker`, which is `BlockMarker::Unplaced` or `Placed { segment, prefix_tokens }`. `marker_placement()` and `body()` both call `cache_markers::plan`, so the recorded placement and the sent placement cannot differ. It is a fact and not a count, because a history with four client tool markers renders the same item count as one with none. Inferring `n - 2` from the count made the next request reach back for a cache write that never happened. A dialect that caches without being told, such as Responses or a local worker, records no marker.

Negative result: the plan never marks the final segment. A ledger that predicts the whole previous request as warm prices the previous final item as a cache read, but the provider bills it as a cache write. With a 30k-token `tool_result` as the final item on the Claude rate card, the quote was low by about $0.10 per turn, and the cost guard decided on that quote. Marking the last block would use one more of the four breakpoints. So the router predicts warmth only through the recorded marker prefix. A request that placed no marker predicts 0 cached tokens, toolbox included. Whether marking the last block pays needs a live cache measurement.

## Classifier evaluation costs

Classifier calls are a separate economy from serving. A call bills a service that this deployment chose to consult, at a price that the call itself recorded. So the snapshot keeps the two apart in `evaluation`, and classifier tokens never enter the serving counters. `observed_cost` adds them and names what it added.

- `evaluation.measured_usd` uses the rates that each call recorded. It is not a provider invoice, and catalog changes do not price it again.
- An unconfirmed settlement does not erase recorded usage or cost. A later repair updates the acknowledgement and adds no call and no charge.
- A call with unknown usage settles at the estimate of its grant when the request can have reached the service. `evaluation.estimated_usd` reports those booked estimates once per call. `measured_usd` never includes them, and the settlement `committed_usd` includes both.
- Missing usage and unanswered intents keep evaluation cost incomplete, even when an estimate stands in, because an estimate is not what the service billed.

Evaluation calls settle by call identity, so the order of completion does not discard charges. The rejected alternative settled on the ordered watermark of the serving session. Two classifier calls with intent seqs 10 and 20 that completed in reverse order then committed only the newer $0.00042, not the combined $0.00070. A regression run over loopback HTTP with a real memory spend ledger showed this. Time-limited deduplication was also rejected, because after a hold TTL a completed call becomes chargeable again. So the memory and Redis ledgers keep one completed identity per call across budget resets, with no expiry. A new attempt needs a fresh identity, and a settlement replay uses the original one.

`evaluation.agreement` compares the served tier with the tier answer of the classifier. The served tier is the recipe tier of the target that served the turn. So a turn that the cost guard moved counts as `capable`, although the scorer picked `efficient`. Agreement is not a quality score, and no component reads it as a reward. The learner reads the tier answers only as a small cold-start prior (see [The routing learner](routing-learner.md#cold-start-prior-from-classifier-tier-answers)).

## Sourcing prices and quality priors

Rate cards never go in source, because they change and a constant in a binary goes stale silently. `ROUNDHOUSE_CATALOG` is the mechanism, and `crates/roundhouse-fleet/src/frontier.rs` states the rule. The router optimizes against a price and the dashboard reports a price. They must be the same number, so both come from one catalog file.

openrouter.ai is the intended input for hosted prices and for `quality_prior`: it publishes comparable per-model prices and intelligence indexes. A prior from a published index makes the capability gate defensible, not asserted. The `import-benchmarks` tool does the import (see [Configure providers and the catalog](../guides/catalog.md#source-quality-priors)). Three cautions apply:

- OpenRouter prices a route to a model, so one model appears at several prices. Pick one deliberately. The catalog refuses two entries for one `(provider, model)`, because the router and the dashboard could resolve that ambiguity differently.
- Normalize the score to the `0.0..=1.0` scale of `quality_prior`, and record which index and snapshot date it came from. An unversioned score silently ranks models again when the leaderboard moves.
- Keep price and capability in separate fields from separate columns. A cheap lookup must not inflate the one number that the dashboard is judged by.

## Why the figures stay separate

A savings figure must never claim one of function, cost, and time without measuring the others, and it must keep cache savings apart from routing savings. A headline that mixes same-model tier price differences with cross-model routing and names no baseline cannot be checked. Ramp's Router headline "40% average cost cut" (ramp.com/router, read 2026-08-20) is an example.
