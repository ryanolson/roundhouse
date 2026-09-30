# Metrics and the dashboard

This chapter tells an operator how to turn on cost metrics, how to read the metrics document and the dashboard, and which configuration controls them. What each dollar figure claims, and why, is in [Cost and savings](../concepts/cost-and-savings.md).

## Endpoints

| Endpoint | Returns | Access |
|---|---|---|
| `GET /v1/metrics` | The metrics snapshot as JSON. | Scoped by key (see below). |
| `GET /v1/metrics/dashboard` | A static HTML page that fetches `/v1/metrics` from the browser and polls it on a timer. | Not gated. |
| `GET /v1/admin/projects/{project}/budget` | The reconciliation view of one project. | Admin key. |

The metrics reads take no lease and cost the store nothing. The numbers are a fold that is already done, not a sweep over the log. Responses carry `Cache-Control: no-store`, because the numbers change on every turn.

### Who sees what

| Control plane | Key | Document |
|---|---|---|
| `Open` (no control plane) | Any, or none | The whole deployment. There is one tenant. |
| `Configured` | Admin key | The whole deployment. |
| `Configured` | Turn key | The rows of the membership of that key only. Session count, turn count, and event window are scoped too. |
| `Configured` | No key | Refused. |

A turn key gets a scoped document, not a filtered copy of the deployment document. Filtering only the money leaves other fields that describe the neighbors. The routing policy of a key has no bearing on what the key can read about itself.

In `Open` mode, sessions logged before a control plane existed fold under the unattributed principal. So the `Open` document reports everything, and does not scope to `default/default`.

## Turn on cost metrics

1. Write a catalog file. Start from `examples/catalog.example.json`.
2. Set `ROUNDHOUSE_CATALOG` to the path of the file.
3. Set `ROUNDHOUSE_FRONTIER_UPSTREAM=openai_responses` to dispatch to real providers.
4. If local traffic uses GPU time that has a cost, set `local_capacity_price` in the catalog.
5. If the catalog uses imported quality priors, put `quality-prior.provenance.json` beside the catalog file.
6. Start the server.
7. If the control plane is configured, read `GET /v1/metrics` with an admin key.
8. If the control plane is `Open`, open `/v1/metrics/dashboard` in a browser.

Without `ROUNDHOUSE_CATALOG`, the binary serves its offline echo stub, and every price is zero. The demo then shows the token breakdown, not the savings.

## The metrics document

The top-level fields of `GET /v1/metrics`:

| Field | Meaning |
|---|---|
| `generated_at_ms` | When the snapshot was built. |
| `first_event_at_ms`, `last_event_at_ms` | The first and last event that this process folded in the scope. |
| `sessions`, `turns` | Sessions and admitted turns in the scope. A turn abandoned mid-dispatch and retried counts twice. |
| `calls` | Dispatches that reached a provider and were accounted for. `turns` above `calls` is the shape of failover. |
| `unrouted_terminals` | Responses with a start and a terminal, but no routed target. They create no model rows. |
| `tokens` | Input, cached input, output, and reasoning. |
| `seat_tokens` | The share of `tokens` served under a forwarded subscription seat. A count, never money. |
| `savings` | The dollar figures (see below). |
| `evaluation` | Classifier costs, apart from serving. |
| `observed_cost` | Serving plus classifier cost, with both price bases named. |
| `learning` | What the routing learner decided and delivered. |
| `coverage`, `coverage_fraction`, `coverage_token_fraction` | How much the providers accounted for, by calls and by tokens. Quote the token fraction next to a dollar figure. |
| `models`, `providers`, `serving_modes` | Rows by model, by provider, and by serving mode (local or remote). |
| `capability_band` | The band that the correlary capability gate used. |
| `quality_prior_citation` | The attribution for imported quality priors, or `null`. |
| `local_capacity_price` | The price of local capacity, or `null` when the catalog sets none. |

The `savings` object:

| Field | Meaning |
|---|---|
| `frontier_spend_usd` | Money that hosted providers billed: measured plus estimated. |
| `frontier_spend_measured_usd` | The part priced from provider-reported counts. |
| `frontier_spend_estimated_usd` | The part priced from our own tokenizer, because the provider reported nothing. |
| `cache_savings_usd` | Measured discount from hosted caches. |
| `local_capacity_usd` | GPU time at `local_capacity_price`, or `null`. |
| `routing_savings_usd` | Estimated saving of local traffic against its correlary, net of capacity cost. Can be negative. |
| `routing_savings_at_decision_usd` | The cross-check from router quotes. Not added into any total. |
| `total_usd` | `cache_savings_usd + routing_savings_usd`. |
| `provider_reported_usd` | What providers reported they billed, or `null`. Added into no figure. |

### Model rows

Each model row has `provider`, `model`, `calls`, `tokens`, `coverage`, and a `mode` of `local` or `frontier`, with the fields of that mode:

| Mode | Fields |
|---|---|
| `local` | `shadow_usd` (the cost on its correlary), `correlary`, `seat_tokens`, `seat_estimated_calls`, `capacity_usd` (`null` when unpriced, never `0.0`). |
| `frontier` | `priced_by_catalog`, `billed_usd`, `billed_measured_usd`, `billed_estimated_usd`, `cache_savings_usd`, `seat_tokens`, `seat_estimated_calls`. |

`priced_by_catalog` tells a configured zero rate apart from a missing catalog entry. Missing-price counts and dashboard warnings use this field. A zero-dollar total alone does not mean that pricing is missing.

### Turn timing

Model rows carry three timing fields. Each is an object with `mean_ms`, `samples`, `rejected`, and `basis`:

| Field | Basis | Interval |
|---|---|---|
| `first_output` | `turn_start_to_first_output` | From `TurnStarted` to the first non-empty durable text delta. |
| `completed_turn_elapsed` | `turn_start_to_terminal` | From the start append to the terminal append, for completed responses. |
| `incomplete_turn_elapsed` | `turn_start_to_terminal` | The same, for incomplete responses. |

The rules:

- The last routed target gets the interval, including routing and failover delay. Work before the start event and delivery after the delta append are not included.
- A response that emits text and then fails still has a `first_output` sample. Timing does not depend on billed usage, so an incomplete response can have a sample and zero calls.
- A superseded attempt contributes no sample. Supersession is keyed by (`SessionId`, `TurnId`), because a key of turn id alone crosses sessions.
- Missing observations produce no mean. A backward timestamp increments `rejected`. Equal timestamps are a valid zero interval.
- A missing start produces no sample.
- Scoped means use summed elapsed time and sample counts, not a mean of means.
- These fields do not measure task success or time to solution.

The interval starts at turn start and not at dispatch. A dispatch-to-first-output interval hides routing and classifier delay from the time part of the product claim.

### Cache reuse evidence

Model rows carry `cache_reuse_evidence`. It compares the predicted and observed cache-reuse ratios for the same dispatches.

| Field | Meaning |
|---|---|
| `predicted_mean_ratio`, `predicted_basis` | From the token count of the router (`routed_isl_minus_expected_prefill`). |
| `observed_mean_ratio`, `observed_basis` | From the token count of the provider (`stated_cached_input_over_input`). |
| `samples` | Paired samples. |
| `mean_signed_error` | Mean of observed minus predicted. A negative value means that the router expected more reuse than the provider reported. |
| `predictions`, `unusable_prediction`, `measured_cache_reads`, `unverifiable_cache_read`, `invalid_usage`, `unusable_usage` | Coverage counters. |

Only explicit provider cache counts supply observations, including a reported zero. Missing counts, records without cache provenance, and locally derived counts supply no sample. The last routed target gets the observation after failover. These observations do not show cache pressure or answer quality, and they do not change routing.

## Classifier evaluation

The `evaluation` object reports classifier costs apart from serving costs. The rules are in [Cost and savings](../concepts/cost-and-savings.md#classifier-evaluation-costs).

| Field | Meaning |
|---|---|
| `intents`, `results`, `pending` | Classification intents, results, and intents without a result. |
| `measured_calls`, `measured_usd` | Calls with measured usage, at the rates that each call recorded. `price_basis` names the basis. |
| `unknown_usage_calls`, `estimated_calls`, `estimated_usd` | Calls with unknown usage, and the estimates booked for them. |
| `refused_calls` | Calls refused before HTTP. |
| `tokens` | Classifier input, output, and total tokens. |
| `settlement` | `acknowledged_calls`, `committed_usd`, `unconfirmed_calls`, `unconfirmed_usd`, `repaired_calls`. |
| `cost_incomplete` | True while usage is missing or intents are unanswered. |
| `unbooked` | `duplicate_results`, `unattributed_results`, `unmatched_repairs`. |
| `models` | Rows by `requested_model` and `reported_model`. A missing reported model stays unknown. |
| `agreement` | Tier agreement (see below). |

### Tier agreement

`evaluation.agreement` compares the served tier with the tier answer of the classifier, in the same scopes as the rest of `evaluation`. Local-only sessions make no classifier call, so they add nothing.

- `answered` counts the results that have a tier answer. Each answer is in one of `agree`, `disagree`, or `not_comparable`.
- `not_comparable` counts answers with no served tier to compare. There are four reasons:
  - no tier recipe routed the turn,
  - the target is in neither recipe list,
  - the route is not the latest route of the session in the folded log,
  - the fold stopped waiting for the answer at the limit below.
- `disagreements` splits the disagreements by direction (`jev_capable_served_efficient`, `jev_efficient_served_capable`). It also splits them by the label of the frontier review that covered the turn: `positive`, `negative`, `unknown`, or `unlabeled`.
- A review can arrive before or after the answer. Each session keeps at most 256 turns (`MAX_REVIEW_DECISIONS`) that wait for an answer or a label. `evicted` counts the unlabeled disagreements that the fold dropped at this limit.

The dashboard shows this object in the "Tier agreement" tile.

## Learner decisions and delivery

The `learning` object counts what the [routing learner](../concepts/routing-learner.md) decided, in the same scopes as the rest of the document. It holds counts, never money.

| Field | Meaning |
|---|---|
| `decisions` | Learned turns, one per turn however many dispatches it made. |
| `modes` | `shadow` and `live`. |
| `served` | The strategy whose plan served each turn: `rules`, `efficient`, `capable`. Every `shadow` turn and every infeasible turn counts as `rules`. |
| `choices` | `exploit`, `explore`, and `constraint_unmet`. |
| `unmet` | Infeasible turns by each constraint that some plan failed (`quality`, `latency`, `grant`). One turn can count more than once. |
| `read_failures` | Turns whose store read failed, by reason: `store_unavailable` or `read_timed_out`. |
| `acknowledgements` | `LearningApplied` events. |
| `delivery` | Delivery outcomes (see below). |

A turn of a `refuse` project that fails on a store outage or an infeasible plan writes no `Routed`. So it counts under none of `decisions`, `unmet`, or `read_failures`. It terminates as `PolicyRefused`, with every other policy refusal.

`delivery` is not a projection of the log. A failed delivery writes nothing to the log, so the engine counts outcomes in process memory. The counts reset when the process restarts. The recovery task counts in the same counters. `delivery` reports:

- `applied_entries`, and `duplicate_entries` that the store skipped,
- `backfills` and `gaps`,
- `diverged` (sessions stopped on a diverged chain) and `stopped` (sessions stopped on a counter out of range or a malformed batch). Every stop is one session, and the other sessions of the project continue.
- `unavailable` and `timed_out` applies,
- `acknowledgement_failures` after an apply.

Admin credentials and the project reconciliation view see `delivery`. A turn key sees `null`, because the counts are per project.

The object does not say why an accepted review credited nothing. The credit rule decides that in the session fold, and a second copy of the rule here can drift from the first.

## Observed cost and gaps

`observed_cost` adds three costs and names the bases:

| Field | Meaning |
|---|---|
| `serving_usd`, `serving_basis` | Catalog-priced serving spend. |
| `local_capacity_usd` | Local capacity spend when the catalog prices it, else `null`. |
| `evaluation_usd` | `evaluation_measured_usd` plus `evaluation_estimated_usd`, with `evaluation_basis`. |
| `total_usd` | The sum. |
| `covers` | `hosted_serving_and_classifier_calls`, or `hosted_serving_local_capacity_and_classifier_calls` when local capacity is priced. |
| `serving_gaps` | `estimated_calls`, `unpriced_models` (hosted rows without a price), and `local_calls` (local calls without a capacity price). |
| `evaluation_incomplete`, `incomplete` | Whether the evaluation cost, or the whole total, is incomplete. |

The rules:

- The booked classifier estimates are in the total, so a classifier call that the service possibly billed never reads as free.
- Judge side calls stay in serving spend and count once.
- The sum is not an invoice. Its local capacity part is a configured approximation, not a measurement of hardware cost.
- Forwarded subscription seats stay outside the hosted dollar amounts. The local turns of a seat still count in `local_capacity_usd`, because the hardware belongs to this deployment.
- `savings.total_usd` keeps its meaning: cache savings plus routing savings.
- Local calls count in `serving_gaps.local_calls` only while the catalog sets no `local_capacity_price`.
- The gap counts can overlap. Do not add them together.
- `incomplete` includes the serving gaps and incomplete evaluation cost. So complete classifier accounting cannot make an incomplete serving total look complete. With local traffic and no local capacity price, `incomplete` stays true even with fully reported usage. Use the individual gap counts to tell excluded hardware costs apart from missing usage or pricing.

## The reconciliation view

`GET /v1/admin/projects/{project}/budget` puts what the ledger says beside what the log says. It returns `committed_usd`, `held_usd`, `measured_usd`, `provider_reported_usd`, `seat_tokens`, and `drift_usd` as separate fields. There is no total field. The figures are not addends of one quantity.

Each dollar column carries a stamp with a `basis`, a `window` (`total`, `monthly`, or `lifetime`), and a `window_start_ms`:

| Basis | Meaning |
|---|---|
| `ledger` | Committed spend in a real budget window. |
| `revoked_keys` | A real ledger figure for a membership that held a key, spent against it, and holds none now. |
| `unenforced` | A membership with no budget, which the engine never grants against. The dollars are `null`, never 0.0. |
| `no_keys` | A membership that never held a key. The dollars are `null`. |
| `archived` | An archived project. Committed is `null`, and measured stays real, because spend history outlives the project. |
| `process-fold` | `measured_usd`: what this process measured since it started. Lifetime only, because the fold cannot prune its watermarks. |
| `provider-reported` | `provider_reported_usd`: the arithmetic of the upstream, not ours. |

`seat_tokens` is a column without dollars. It stops a pass-through project from reading as under-billed.

`drift_usd` is `committed_usd - measured_usd`, not clamped. `provider_reported_usd` stays out of that difference. Negative drift has three causes:

- a settle that failed and was logged as a warning,
- a process that restarted between the dispatch and the settle,
- nothing wrong. The engine writes the terminal usage event before it settles the ledger, so a turn between those two steps is measured but not committed. `held_usd` tells this case apart.

Known structural drift is labeled, not hidden:

- Under `ProjectPaidOnly`, user-paid spend shows in measured but never in committed.
- A catalog whose prices changed since turns settled reads as drift.
- A restart makes drift positive until the log is folded again, because the fold is per process.

The shared spend ledger answers committed dollars per membership per budget window, across nodes. It holds no tokens, per-model rows, serving-mode split, cache figures, or counterfactual. So cost limits apply across the deployment, and only the reporting of cost is per node.

The view reads one ledger balance per budgeted member. Each read can change state, because it rolls a lapsed window over. So the view reads each balance only under the terms that the admission of the membership carries. For a membership with only revoked keys, it uses the terms that the directory pairs with that membership. There is no pagination, so a project with N budgeted members costs N round trips.

## Metrics across nodes

Every number on the dashboard comes from the fold of one process. Scoping is by principal or project, never by node. The metrics have no time-window selector, and the reconciliation `measured_usd` cannot be windowed.

No cross-node aggregate is built. These alternatives were examined and not adopted:

- **Replay every session when the page is read.** It is correct, because the fold is idempotent. But each poll costs a number of reads proportional to all events, against the same Redis that carries the write path. It also needs a list of all sessions, which the session store does not offer. The store can list only sessions that carry a learning mark.
- **A durable fold state per node, merged when read.** This is unsound. `Counters::absorb` is unguarded addition, which is valid only across the disjoint principal rows of one fold. A failover replays the whole log of a session into the fold of the second node, so a sum counts every shared event twice. The (session, seq) identity needed to remove duplicates is gone once events become sums. A merge of watermarks by maximum does not repair the counters. The per-node key also orphans itself on every restart, because the node id is a fresh UUID.
- **A shared running aggregate written per settled call.** It costs one extra shared write per settle. But it is the second accumulator that the fold design refuses. Not every field is a sum, either: `DeclaredBaseline` is a three-state collapse, the principal window is a min and max, and `sessions` is a cardinality.

The only sound shape keeps the session as the unit of idempotency. Each session has a shared row of its fold to date. Scope totals change by atomic delta against that row. The snapshot must be built from summed counters before the pricing step, because the correlary of a local model depends on the scope (see [Cost and savings](../concepts/cost-and-savings.md#the-correlary-depends-on-the-scope)).

## Dashboard page limits

- The page header reads `since <first_event_at_ms>`. For the deployment scope, that is the earliest event that this process folded, not the earliest event of the deployment. After a restart, it is the first event of whichever session a turn opened again.
- In `Configured` mode, a browser sends no key on a navigation. The fetch of `/v1/metrics` is refused, and the page shows "cannot reach /v1/metrics -- HTTP 401". The page has no place to enter a key. Use the JSON endpoint with a key instead.
- The page does not show `first_output`, `completed_turn_elapsed`, `incomplete_turn_elapsed`, `unrouted_terminals`, `cache_reuse_evidence`, or `learning`. Read them from the JSON.

## Configuration

### Environment variables

| Variable | Meaning |
|---|---|
| `ROUNDHOUSE_CATALOG` | Path of the catalog JSON file: hosted models, prices, providers, correlaries. Unset serves the echo stub at zero prices. |
| `ROUNDHOUSE_FRONTIER_UPSTREAM` | Unset serves the echo stub. `openai_responses` dispatches to real providers. Any other value is refused. |
| `ROUNDHOUSE_OPENAI_API_BASE` | The base URL of the built-in `openai` provider. |
| `ROUNDHOUSE_CLASSIFY_CONFIG` | Path of the background classifier configuration. Optional. |

### The catalog

`ROUNDHOUSE_CATALOG` names one JSON file. It carries the hosted models that the router can choose between, their prices, the providers, and any declared correlaries. The price that the router optimizes against and the price that the dashboard reports come from this one file. A catalog that is named but unreadable stops the process. Every field, check, and default of the file is in [Configure providers and the catalog](../guides/catalog.md).

The catalog fields that change what the metrics report:

| Field | Effect on the metrics |
|---|---|
| `models[].pricing` | The rate card for hosted spend, the cache discount, and the correlary counterfactual. |
| `models[].quality_prior`, `local_quality`, `default_local_quality`, `capability_band` | The inputs of the capability gate that decides which hosted model can price a local model. |
| `correlaries` | Declared correlaries. The `note` shows verbatim on the dashboard. |
| `local_capacity_price` | Turns on `savings.local_capacity_usd`, `capacity_usd` on local rows, and the net `routing_savings_usd`. Without it, those fields are `null` and local cost is unpriced. |

### Show the quality-prior citation

If the catalog uses quality priors from `import-benchmarks` (see [Configure providers and the catalog](../guides/catalog.md#source-quality-priors)), publish the attribution with them:

1. Put `quality-prior.provenance.json` in the same directory as the file that `ROUNDHOUSE_CATALOG` names.
2. Restart the server.
3. Make sure that the dashboard shows the citation under the savings figure, and that `/v1/metrics` has `quality_prior_citation`.

The dashboard is itself a republication of numbers priced through the imported priors. So the attribution must travel with it. A missing or bad provenance file never stops the boot, because the server discovers the file and no operator names it.

### Background turn classification

The binary reads optional JSON configuration from `ROUNDHOUSE_CLASSIFY_CONFIG`. Classification is off unless the file sets `enabled: true`. The binary supplies a separate evaluation ledger for it.

An unreadable file or an invalid configuration stops startup. An unknown key at any depth is refused, so a misspelled field is never silently dropped. A disabled configuration is still parsed, but its credential is not resolved. The fields are required unless a default is stated:

| Field | Meaning |
|---|---|
| `enabled` | Turn on background classification. Defaults to `false`. |
| `revision` | Configuration revision that the operator assigns, recorded with each intent. |
| `model` | Requested model identifier, recorded apart from the model that the service reports. |
| `base_url` | Classifier API root. Defaults to the TypeSafe API root. |
| `auth.env` | Name of the environment variable that holds the deployment credential. |
| `pricing.input_per_mtok_usd`, `pricing.output_per_mtok_usd` | Input and output rates, in dollars per million tokens. |
| `expected_output_tokens` | Output-token estimate for the budget quote. |
| `caps.max_prior_classifications` | Maximum prior classifications in the projection. |
| `caps.max_prior_turns` | Maximum prior local metadata entries in the projection. Independent of `caps.max_prior_classifications`. |
| `caps.max_prompt_chars`, `caps.max_total_bytes` | Character limit for the current prompt, and byte limit for the rendered projection. |
| `transport.max_request_bytes`, `transport.max_response_bytes`, `transport.deadline_ms` | Transport size limits and network deadline. |
| `executor.max_in_flight`, `executor.max_http_concurrency` | Capacity from prompt capture through result retention, and the separate limit on concurrent HTTP calls. |
| `executor.call_ttl_ms` | One expiry shared by queue wait, budget grant, HTTP, and settlement. |
| `executor.result_retention_ms`, `executor.sweep_interval_ms` | Retention of finished results, and the cleanup interval. |
| `budget.limit_usd`, `budget.window`, `budget.warn_at` | Evaluation ceiling, `total` or `monthly` window, and warning fraction. |
| `budget.member_share` | Optional member fraction. Without it, the project budget is pooled. |

What the classifier sees and answers:

- The projection holds bounded prior turn metadata, available classifications, and the current user text.
- Local-only sessions are excluded, because a classification needs an admitted frontier target.
- One request asks four questions: request intent, complexity, dependence on context, and the tier of work that the turn needs (`capable` or `efficient`). The tier options describe kinds of work. They name no model and no price.
- Under taxonomy 2, a reply without the tier answer is unusable. Records from taxonomy 1 decode with no tier.
- The classifications describe the turn. They do not show answer quality.
- The engine records a classification intent and schedules the work in the background. Results become available to later turns. The routing learner reads them as input and as a cold-start prior (see [The routing learner](../concepts/routing-learner.md#cold-start-prior-from-classifier-tier-answers)). The tier answer is never a reward.

Capacity:

- The engine reserves classification capacity before it captures the current prompt. A saturated or stopped classifier skips the capture.
- The permit stays held during the serving turn, the background execution, and the result retention. So long serving turns can reduce classification throughput at a fixed capacity.
- Routing snapshots select the newest bounded references, without collecting the full classification history.
- The end of the classifier lifetime stops admission and signals cancellation to its workers. Cancellation leaves unanswered intents with unknown outcomes, because a request can already have reached the service.

Transport and usage:

- The transport sends the checked bytes once, without retries, under the configured size and duration limits. Budget estimates use the complete serialized request.
- The transport requires one valid answer per requested key, and it rejects unexpected answer keys. An empty question map is refused before HTTP.
- Received envelopes keep the model that the service reported, apart from usable answers. Missing or malformed model metadata stays unknown.
- The adapter keeps reported usage when the answers are unusable. Missing or partial usage stays unknown.

Settlement:

- A call that returns no usage settles the evaluation ledger at the estimate of its grant when the request can have reached the service. That is a transport error after the send, a timeout, an oversized or unparseable reply, or a reply without usage.
- A call that never connected, or that the service refused with an error status, settles at zero.
- The record keeps the usage unknown and stores the amount that it settled. A settlement repair sends that same amount.
- Evaluation calls settle by call identity (see [Cost and savings](../concepts/cost-and-savings.md#settlement-by-call-identity)). Serving turns keep their ordered session watermarks.
- A settlement repair uses the original call identity, the recorded amount, the durable session principal, and the budget-window mode configured when the repair runs. It does not replay the recorded mode, because the ledger keeps one account per project and resets it at a window boundary. A repair makes no classifier request.
- An unconfirmed acknowledgement does not show whether the ledger applied a charge.
- Each turn considers at most `max_in_flight` repair candidates. Capacity and identity claims stay held through execution and acknowledgement delivery, including outstanding delivery handles.
