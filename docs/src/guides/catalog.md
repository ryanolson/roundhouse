# Configure providers and the catalog

The catalog is one JSON file that lists the hosted models the router can choose, their prices, and where each provider lives. This chapter describes every field, the checks that refuse a bad file, and how to source the numbers.

Copy `examples/catalog.example.json`, which a test parses. Its prices are zero placeholders: replace each with the published rate for the route you call.

## Why one file

The router optimizes against a price, and the dashboard reports a saving at a price. Two sources could drift and make the savings figure meaningless, so both read one rate card. Prices are not in source code, where a constant goes stale without an error.

## Load the catalog

Set `ROUNDHOUSE_CATALOG` to the file path. Without it, the offline echo stub serves every turn at a price of zero. A named but unreadable catalog stops the process, because a fallback would serve turns under prices nobody chose.

Set `ROUNDHOUSE_FRONTIER_UPSTREAM=openai_responses` to dispatch to real providers. The value is a switch, not a wire: each entry names its wire in `wire_protocol`. See the [configuration reference](../appendix/configuration.md) for the related variables.

## Top-level fields

| Field | Meaning | Default |
|---|---|---|
| `models` | The hosted models the router can choose. See [Model entries](#model-entries). Must not be empty. | required |
| `providers` | Where each provider is. See [Providers](#providers). | `{}` |
| `correlaries` | Declared equivalences between a local model and a hosted model. | `[]` |
| `local_quality` | Declared capability of each local model, keyed by model name. | `{}` |
| `default_local_quality` | Capability of a local model that is not in `local_quality`. | `0.5` |
| `capability_band` | How far apart two quality priors can be and still be compared. | `0.10` |
| `local_base_ttft_ms` | Time to first token quoted for a local worker before any prefill. | `60.0` |
| `local_ttft_ms_per_prefill_token` | Milliseconds per effective prefill token for local workers. | `0.0` |
| `fleet_quote_deadline_ms` | How long a turn waits for the Dynamo residency answer. | `500` |
| `local_capacity_price` | What your own GPU time costs. See [Local capacity](#local-capacity). | absent |
| `$comment` | Free text. Roundhouse ignores it. | none |

Any other key stops the load. `local_quality` values, `default_local_quality`, and `capability_band` must be in `0.0..=1.0`, the scale that the capability gate compares.

## Model entries

Each entry in `models` is one hosted target, identified by the pair `(provider, model)`. All fields are required.

| Field | Meaning |
|---|---|
| `provider` | Name of a `providers` entry, or the built-in `openai`. |
| `model` | The provider's own model id. Write the full id. |
| `wire_protocol` | The dialect this target speaks. See [Dialects](#dialects). |
| `cache_model` | How the router predicts this target's prompt cache. See [Cache model](#cache-model). |
| `pricing` | Four prices, in US dollars per million tokens. See [Pricing](#pricing). |
| `quality_prior` | Relative capability, `0.0..=1.0`. It is configuration, not measurement. |
| `base_ttft_ms` | Latency floor before any prefill: network plus queueing. |
| `ttft_ms_per_uncached_token` | Extra time to first token for each uncached prompt token. |

A model entry ignores unknown keys, so a misspelled field stops the load only because the correct one is then missing. A negative price or latency stops the load. A negative rate reports the fleet as paid to serve, which the dashboard shows as a saving. A `quality_prior` outside `0.0..=1.0` also stops the load.

### Dialects

| `wire_protocol` | Wire | Client in this build |
|---|---|---|
| `openai_responses` | OpenAI Responses | Yes |
| `anthropic_messages` | Anthropic Messages | Yes |
| `openai_chat_completions` | OpenAI Chat Completions and its clones (vLLM, SGLang, the Dynamo frontend) | No |

An `openai_chat_completions` entry loads, but it stops the boot when `ROUNDHOUSE_FRONTIER_UPSTREAM` is set. The example file keeps a `dynamo-fleet` provider of this dialect as a template that no model entry names.

The registry holds one client per provider, so all entries of one provider must speak one dialect, or the boot stops. To use both OpenRouter routes, `/responses` and `/messages`, define OpenRouter twice under two names with one `base_url`, as the example does with `openrouter` and `openrouter-messages`.

### Cache model

| `kind` | Fields | Use it for |
|---|---|---|
| `inactivity_decay` | `half_life_ms`, `max_ttl_ms`, `min_prefix_tokens` | Providers that cache automatically above a minimum prefix, such as OpenAI. A shorter prefix is never cached. |
| `deterministic` | `ttl_ms` | Providers that cache only at explicit markers for a fixed time, such as Anthropic. A hit inside `ttl_ms` is certain. |
| `observed` | none | Targets that report their own overlap. The router does not guess. |

The router predicts the cost of the next turn from the cache model. That is why the same model on two providers has two entries.

### Pricing

| Field | Meaning |
|---|---|
| `input_per_mtok_usd` | Uncached input tokens. |
| `cached_input_per_mtok_usd` | Input tokens that hit a warm cache. |
| `cache_write_per_mtok_usd` | Writing a prefix into the cache. |
| `output_per_mtok_usd` | Output tokens. |

A zero `cache_write_per_mtok_usd` means that the provider does not price writes separately, so written tokens bill at the input rate. It does not mean writes are free. Anthropic prices the write, so do not leave it at zero on that dialect.

### Cache lifetime on Anthropic Messages

The Anthropic wire has two cache lifetimes: five minutes with no `ttl` field, and one hour with `ttl: "1h"`. The catalog accepts only entries that map onto them.

| Entry | Result |
|---|---|
| `deterministic`, `ttl_ms` `300000` | Default lifetime. No `ttl` field is sent. |
| `deterministic`, `ttl_ms` `3600000` | Sends `1h`. `cache_write_per_mtok_usd` must be twice `input_per_mtok_usd`; the error names the value. |
| `deterministic`, any other `ttl_ms` | Refused. |
| `inactivity_decay`, `max_ttl_ms` at most `300000` | Default lifetime. |
| `inactivity_decay`, `max_ttl_ms` above `300000` | Refused. |
| `observed` | Default lifetime. |

A refused entry would let the ledger model a cache as warm for longer than the wire asks, such as ten minutes against five. The router would then price hits that never happen. The rules apply to every `anthropic_messages` entry, including a gateway that fronts Messages.

Roundhouse gives the client's tool cache markers the lifetime of its conversation markers, because a shorter marker must not come before a longer one.

### Identity rules

- Two entries for one `(provider, model)` are refused. The router keeps the last duplicate and the dashboard finds the first, so a turn would be chosen on one price and reported at another.
- One identity carries one price, and OpenRouter prices a route to a model, not the model. On 2026-08-24, `moonshotai/kimi-k3` had 15 endpoints with a 2.3 times price spread, and `deepseek/deepseek-v4-pro` had 17 with a 3.7 times spread. A catalog price is a choice of route.
- A model reachable through two providers, such as `openai/gpt-5.6-sol` through `openrouter` and `openai`, is two identities with two prices.
- On the provider named `openrouter`, a model id that starts with `~` is refused. It is a rolling pointer: after a re-point, turns are dispatched on one model and priced on another. An OpenRouter definition under another name is not checked.
- On OpenRouter (read 2026-08-24), a bare id such as `deepseek/deepseek-v4-pro` is frozen to an earlier snapshot. A suffix such as `:free`, `:extended`, `:nitro`, or `:floor` changes the serving endpoint, price, and quality. The catalog accepts suffixes.

## Providers

A provider is a base URL, a route per dialect, the variable that holds its key, and optional static headers. Roundhouse builds one client with its own connection pool per provider.

```json
"openrouter": {
  "base_url": "https://openrouter.ai/api/v1",
  "routes": { "models": "/models", "responses": "/responses" },
  "auth": { "env": "OPENROUTER_API_KEY" },
  "extra_headers": {
    "HTTP-Referer": "https://your-deployment.example",
    "X-OpenRouter-Title": "roundhouse"
  }
}
```

| Field | Meaning |
|---|---|
| `base_url` | The origin that every route joins onto. It must begin with `http://` or `https://`. A trailing slash is trimmed. |
| `routes` | Path of each dialect: `chat_completions`, `responses`, `messages`. Also `models`, which no serving path reads. Every path must begin with `/`. |
| `auth.env` | The name of the environment variable that holds the key. It must be a valid variable name. |
| `auth.style` | For `anthropic_messages` only: `x_api_key` (default) or `bearer`. See [Stored-key header](#stored-key-header). |
| `extra_headers` | Static headers on every request. They cannot replace the credential header or `anthropic-version`. |

Unknown keys are refused.

### Rules that keep the registry complete

A provider with no client, found at dispatch, would fail one tenant's turn because of a line in a file. So:

1. At load, every entry's `provider` names a definition, or is the built-in `openai`.
2. At load, a provider declares a route for its entries' dialect.
3. At boot, when `ROUNDHOUSE_FRONTIER_UPSTREAM` is set, the build has a client for that dialect, and each provider's entries share one dialect.

The built-in `openai` provider takes its endpoints from `ROUNDHOUSE_OPENAI_API_BASE` and `ROUNDHOUSE_OPENAI_PASS_THROUGH_BASE`. An `openai` definition overrides both, with a warning for each variable that is set. There is no implicit `anthropic` provider, so a typo in an `openai` entry's `wire_protocol` cannot send turns to `api.anthropic.com`.

### Keys

`auth.env` only names the key's variable. When `ROUNDHOUSE_FRONTIER_UPSTREAM` is set, a provider whose variable is unset gives a boot warning. Each turn resolves its credential from the deployment, project, and member tiers of the control plane. The admin plane cannot attach one at runtime. See [Configure tenancy and keys](tenancy.md).

### Stored-key header

| Provider | Header | `auth.style` |
|---|---|---|
| Anthropic | `x-api-key` | `x_api_key` (default) |
| OpenRouter `/messages` | `Authorization: Bearer` | `bearer` |

Set `"auth": {"env": "OPENROUTER_API_KEY", "style": "bearer"}` on the OpenRouter Messages definition. An unknown style stops the load. A wrong header gives a 401 on every turn, which looks like a bad key, not a wrong file. Roundhouse does not guess the style from the host name, because a guess mis-authenticates every gateway that fronts either provider. `auth.style` has no effect on the OpenAI wires.

The Messages client always sends `anthropic-version: 2023-06-01`. Newer features are gated by the `anthropic-beta` header.

## Local side of the comparison

These fields keep both halves of the local-versus-hosted comparison in one file. The shipped `roundhouse` binary attaches no local fleet, so they matter only for a binary that does.

### Local latency

A local quote is `local_base_ttft_ms` plus `local_ttft_ms_per_prefill_token` times the effective prefill tokens that Dynamo reports. Set the slope to `1000 / tokens_per_second` from a measured prefill rate. Until you have one, leave it at zero, because an invented slope loses real turns to a hosted model. Negative values stop the load.

### Fleet residency bound

`fleet_quote_deadline_ms` bounds the Dynamo residency call on a turn that can fail open. If the fleet errors or is late, the turn drops the local candidate, routes among hosted targets, and records `local_quote_skipped` as `fleet_error` or `fleet_timeout`. Zero is refused, because it drops every local candidate while the fleet is healthy.

A turn that cannot fail open waits up to the turn deadline instead, and a fleet failure fails it. So a local-only session never goes to a hosted model. Such a turn has a local-only policy, no credential for any hosted provider, or a spent frontier cadence.

### Local capacity

```json
"local_capacity_price": { "input_per_mtok_usd": 0.4, "output_per_mtok_usd": 1.6 }
```

With this optional price, a local quote is the effective prefill tokens at the input rate plus the expected output tokens at the output rate. Matched tokens are free. A local worker can then lose on cost to a cheaper hosted target, and the dashboard reports local spend and a saving net of it. Without it, local turns quote $0, and the dashboard marks local cost as unpriced, never free.

An approximate price is fine if it does not make local look cheaper than it is. There are no cache rates, and an unknown key or a negative rate stops the load. The price is GPU time the deployment owns, not budget spend, so no budget refuses a local candidate because of it.

### Correlaries and the capability gate

```json
{ "local_model": "my-local-model", "provider": "openai", "model": "gpt-x", "note": "Why these two compare." }
```

A correlary says that a local model stands in for a hosted model when the dashboard prices a saving. The dashboard shows `note` verbatim, because trusting the savings figure means trusting that sentence. A correlary that names a model not in the catalog stops the load, because that model's traffic would go unpriced without a sign.

The capability gate prices two models against each other only when their quality priors are within `capability_band`. Nothing else stops a small local model from being priced against a flagship.

## Source quality priors

`import-benchmarks`, a binary target of `roundhouse-fleet`, turns OpenRouter's benchmark index into `quality_prior` values. No shipped binary links it, and the server never calls the benchmarks route. It writes files that you review and merge.

```bash
OPENROUTER_API_KEY=... cargo run -p roundhouse-fleet --bin import-benchmarks -- \
  --provider openrouter \
  --out quality-prior.fragment.json \
  --provenance quality-prior.provenance.json
```

| Option | Meaning |
|---|---|
| `--provider` | The catalog provider name to write into each entry. Default `openrouter`. |
| `--out` | The catalog fragment. Default `quality-prior.fragment.json`. |
| `--provenance` | The provenance record. Default `quality-prior.provenance.json`. |
| `--source`, `--benchmark-type`, `--task-type` | Filters that the route accepts. |
| `--base-url` | Default `https://openrouter.ai/api/v1`. |
| `--from-file` | Read a saved response instead of calling the route. Use it while you tune a filter: the route allows 30 requests a minute and 500 a day per account. |

The tool reads `GET /api/v1/benchmarks`, which needs a key and returns a version, an `as_of` date, and a citation. The unauthenticated `Model.benchmarks` block of `GET /models` has none of these, so the tool does not use it. An unversioned score re-ranks models silently when the leaderboard moves.

- An OpenRouter `accuracy` is already on `0..1` and is used as it is.
- An Artificial Analysis index is divided by 100, a stated denominator. Dividing by the measured maximum instead would make each prior depend on which models one fetch listed. On the 2026-08-24 corpus, priors from this index cap at about 0.631. A low prior narrows comparisons, which is the safe direction.
- Each entry records its basis, `openrouter.accuracy` or `artificial_analysis.intelligence_index/100`, because one fetch can mix both scales.
- An entry with neither a `meta.citation` nor its own `source` is refused.

The fragment holds only model identity and `quality_prior`. The attribution that OpenRouter requires on republication is in the provenance file, so keep the two together. If `quality-prior.provenance.json` sits beside the file that `ROUNDHOUSE_CATALOG` names, the dashboard shows its citation under the savings figure. A missing, renamed, or unparsable file gives no line and does not stop the boot.

## OpenRouter facts

Read from OpenRouter's public API on 2026-08-24, with no API key:

- `POST /api/v1/responses` is generally available and stateless. `store` must be `false`, a non-null `previous_response_id` gets a 400, and there is no WebSocket route.
- `/api/v1/messages` uses the Anthropic envelope. No fetched page states its stability tier.
- Responses streams carry comment keep-alives such as `: OPENROUTER PROCESSING`. After the first token, an error arrives as a stream event while the HTTP status stays 200. A client that counts a 200 plus `[DONE]` as success miscounts failed turns.

What Roundhouse does about them:

- It skips comment and non-`data` lines, maps `response.failed` and `error` frames to an upstream error, and accepts a `[DONE]` sentinel.
- The outbound Responses body is a whitelist of fields in both the OpenAI and OpenRouter schemas. They are `model`, `stream`, `input`, `prompt_cache_key`, `max_output_tokens`, `store`, and the client's `tools` and `tool_choice`. The `client_metadata` and `stream_options` that Codex sends at `6344a65` are not in OpenRouter's schema.
- It sends `store: false` and never `previous_response_id`, because it rebuilds every prompt from its own log, and a provider-side conversation would be a second history.
- It records the dollar cost that OpenRouter reports in each usage object and publishes it as `provider_reported_usd`, added into no other figure. See [Metrics and the dashboard](../operations/metrics.md).
- It sends no routing preference (`provider`, `models`, or `session_id`), so OpenRouter's fallbacks are on. It does not read the served model or provider back, so a fallback's cost and quality are attributed to the requested model. To pin an upstream, set it on the OpenRouter side.
