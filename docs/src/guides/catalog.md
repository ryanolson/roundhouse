# Configure providers and the catalog

The catalog is one JSON file that lists the hosted models the router can choose, their prices, and where each provider lives. This chapter describes every field of the file, the checks that refuse a bad file at boot, and how to source the numbers.

`examples/catalog.example.json` is the file to copy. A test parses it, so it always loads. Every price in it is a placeholder.

## Why one file

The price the router optimizes against and the price the dashboard reports as saved must be the same number. If they came from two places, they can drift, and then the savings figure means nothing. So the catalog holds one rate card, and both sides read it.

Prices are not in source code. A rate card changes, and a constant in a binary goes stale without any error. The file is the only place a price lives.

## Load the catalog

| Variable | Effect |
|---|---|
| `ROUNDHOUSE_CATALOG` | Path to the catalog JSON. If it is unset, the binary serves its offline echo stub, and every price is zero. |
| `ROUNDHOUSE_FRONTIER_UPSTREAM` | Set it to `openai_responses` to dispatch to real providers. If it is unset, the echo stub answers. Any other value stops the boot. |
| `ROUNDHOUSE_OPENAI_API_BASE` | Base URL for a stored key on the built-in `openai` provider. Default `https://api.openai.com/v1`. |
| `ROUNDHOUSE_OPENAI_PASS_THROUGH_BASE` | Base URL for a forwarded ChatGPT login on the built-in `openai` provider. Default `https://chatgpt.com/backend-api/codex`. |

A catalog that is named but unreadable stops the process. Roundhouse does not fall back to a default, because then every turn is served under prices nobody chose.

The value `openai_responses` of `ROUNDHOUSE_FRONTIER_UPSTREAM` is a switch. It means "dispatch to real providers". It does not name a wire. Each catalog entry names its own wire in `wire_protocol`, and the provider registry reads it from there.

## Top-level fields

| Field | Meaning | Default |
|---|---|---|
| `models` | The hosted models the router can choose. See [Model entries](#model-entries). The list must not be empty. | required |
| `providers` | Where each provider is. See [Providers](#providers). | `{}` |
| `correlaries` | Declared equivalences between one of your local models and a hosted model. | `[]` |
| `local_quality` | Declared capability of each local model, `0.0..=1.0`, keyed by model name. | `{}` |
| `default_local_quality` | Capability for a local model that is not in `local_quality`. | `0.5` |
| `capability_band` | How far apart two quality priors can be and still be compared. | `0.10` |
| `local_base_ttft_ms` | Time to first token that a local worker is quoted before any prefill. | `60.0` |
| `local_ttft_ms_per_prefill_token` | Milliseconds per effective prefill token for local workers. | `0.0` |
| `fleet_quote_deadline_ms` | How long a turn waits for the Dynamo residency answer. | `500` |
| `local_capacity_price` | What your own GPU time costs. See [Local capacity](#local-capacity). | absent |
| `$comment` | Free text. Roundhouse ignores it. | none |

Every other unknown key stops the load. A misspelled field is refused instead of silently dropped.

## Model entries

Each entry in `models` is one hosted target. The pair `(provider, model)` is its identity.

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

All fields are required. A model entry ignores an unknown key, so a misspelled field name stops the load only because the correct name is then missing. A price, `base_ttft_ms`, or `ttft_ms_per_uncached_token` below zero stops the load. A negative rate reports the fleet as paid to serve traffic, and the dashboard shows that as a saving. A `quality_prior` outside `0.0..=1.0` also stops the load, because the capability gate compares values on that scale.

### Dialects

`wire_protocol` takes one of three names.

| Value | Wire | Client in this build |
|---|---|---|
| `openai_responses` | OpenAI Responses | Yes |
| `anthropic_messages` | Anthropic Messages | Yes |
| `openai_chat_completions` | OpenAI Chat Completions and its clones (vLLM, SGLang, the Dynamo frontend) | No |

The catalog accepts `openai_chat_completions`, but the binary has no client for it. If `ROUNDHOUSE_FRONTIER_UPSTREAM` is set and an entry speaks it, the boot stops. The example file keeps such a provider, `dynamo-fleet`, as a template with no `models` entry that names it.

One registry client serves one provider name. So all entries of one provider must speak the same dialect. If they do not, the boot stops and names both entries. OpenRouter serves `/responses` and `/messages`, so to use both you define it twice under two names. Both definitions point at the same `base_url`. The example file does this with `openrouter` and `openrouter-messages`.

### Cache model

`cache_model` has a `kind` field and three forms.

| `kind` | Fields | Use it for |
|---|---|---|
| `inactivity_decay` | `half_life_ms`, `max_ttl_ms`, `min_prefix_tokens` | Providers that cache automatically above a minimum prefix, such as OpenAI. A prefix that is shorter than `min_prefix_tokens` is never cached. |
| `deterministic` | `ttl_ms` | Providers that cache only at explicit markers for a fixed time, such as Anthropic. A hit inside `ttl_ms` is certain. |
| `observed` | none | Targets that report their own overlap. The router does not guess. |

The router uses this model to predict the cost of the next turn. The prediction is the reason the same model on two providers has two entries.

### Pricing

`pricing` has four fields:

| Field | Meaning |
|---|---|
| `input_per_mtok_usd` | Uncached input tokens. |
| `cached_input_per_mtok_usd` | Input tokens that hit a warm cache. |
| `cache_write_per_mtok_usd` | Writing a prefix into the cache. |
| `output_per_mtok_usd` | Output tokens. |

If `cache_write_per_mtok_usd` is zero, the provider does not price the write separately. It does not mean that writes are free. Roundhouse then bills an uncached token at the input rate. Anthropic does price the write, so never leave it at zero on that dialect once you fill in real prices.

### Cache lifetime on Anthropic Messages

The Anthropic wire has two cache lifetimes. With no `ttl` field, the lifetime is five minutes. With `ttl: "1h"`, it is one hour. The catalog accepts only the forms that map onto them.

| Entry | Result |
|---|---|
| `deterministic` with `ttl_ms` of `300000` | Default lifetime. No `ttl` field is sent. |
| `deterministic` with `ttl_ms` of `3600000` | Roundhouse sends `1h`. `cache_write_per_mtok_usd` must equal twice `input_per_mtok_usd`. The error names the required value. |
| `deterministic` with any other `ttl_ms` | The load is refused. |
| `inactivity_decay` with `max_ttl_ms` of `300000` or less | Default lifetime. |
| `inactivity_decay` with `max_ttl_ms` above `300000` | The load is refused. |
| `observed` | Default lifetime. |

The refusals prevent one fault. If the ledger models a 60-second TTL as warm but the wire requests five minutes, the router prices a cache hit that the wire never asks for.

The same rules apply to every `anthropic_messages` entry, whatever the provider is called. So they also apply to a gateway that fronts Messages.

Roundhouse sets the tool cache markers to the same lifetime as its conversation markers. A one-hour target sends `1h` on both. This keeps a shorter marker from preceding a longer one. Tool definitions, schema contents, marker positions, and the four-marker allowance do not change.

### Identity and pricing rules

The catalog refuses two entries for one `(provider, model)`. The router and the dashboard resolve a duplicate in different ways. The router keeps the last entry. The dashboard finds the first. A turn is chosen on one price and reported at another.

One identity carries exactly one price. That matters because OpenRouter prices a route to a model, not the model. The same model has several upstream providers at different prices. On 2026-08-24, `moonshotai/kimi-k3` had 15 endpoints with a 2.3 times spread in price. `deepseek/deepseek-v4-pro` had 17 endpoints with a 3.7 times spread. A price in the catalog is therefore a choice of route.

The same model can appear twice if it is reachable through two providers. `openai/gpt-5.6-sol` through `openrouter` and through `openai` are two identities with two prices.

The catalog also refuses an OpenRouter id that starts with `~`. Such an id is a rolling pointer. OpenRouter re-points it at will, and after a re-point every turn is dispatched on one model and priced on another. Write the full dated id instead.

Two more facts help when you write ids:

- A bare id such as `deepseek/deepseek-v4-pro` is frozen to an earlier snapshot. It does not track the newest release. The newer line is a separate id with its own price.
- An id suffix such as `:free`, `:extended`, `:nitro`, or `:floor` changes which endpoint serves the request. That changes the price and the quality. The catalog does not refuse these suffixes. Write the suffix you mean.

## Providers

A provider is a base URL, the route of each dialect, the variable that holds its key, and optional static headers. Roundhouse builds one client per provider at boot. Each client has its own connection pool.

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
| `base_url` | The origin that every route joins onto. It must begin with `http://` or `https://`. Roundhouse trims a trailing slash. |
| `routes` | Path of each dialect: `chat_completions`, `responses`, `messages`. Also `models`, which no serving path reads. Every path must begin with `/`. |
| `auth.env` | The name of the environment variable that holds the key. It must be a valid variable name. |
| `auth.style` | For `anthropic_messages` only: `x_api_key` (default) or `bearer`. See [Stored-key header](#stored-key-header). |
| `extra_headers` | Static headers that every request carries. Use them for identification headers, not for credentials. |

Every field is checked at load, and unknown keys are refused.

### Rules that keep the registry complete

Three boot checks make sure that the router can never name a provider that has no client.

1. The `provider` of every entry names a definition, or it is the built-in `openai`.
2. A provider declares a route for the dialect that its entries speak.
3. The build has a client for that dialect, and all entries of a provider agree on one dialect. This check runs at boot, not at file load. It applies when `ROUNDHOUSE_FRONTIER_UPSTREAM` names a real upstream.

The built-in `openai` provider needs no definition. Its endpoints come from `ROUNDHOUSE_OPENAI_API_BASE` and `ROUNDHOUSE_OPENAI_PASS_THROUGH_BASE`. If you write an `openai` definition, it takes precedence, and the two variables are not read. Roundhouse logs a warning when a variable is set and shadowed.

There is no implicit `anthropic` provider. An `anthropic_messages` entry must have a definition. Without this rule, a typo in the `wire_protocol` of an `openai` entry opens a connection to `api.anthropic.com`.

The catalog refuses a provider that is undefined at load time, not at the turn that uses it. If the router picked it, one tenant's turn fails because of a line in a file.

### Keys

The key is never in the catalog. `auth.env` only names the variable. If a configured provider has no key in the environment, the boot logs a warning.

The credential that a turn uses is resolved for each turn from the control plane. There are three tiers: deployment, project, and member. `openrouter` is just another provider name there. Each tier attaches a provider key through the same credentials schema. A tier that falls back to the deployment key spends the wrong account, so tests hold the tier order. See [Configure tenancy and keys](tenancy.md).

The admin plane does not attach or rotate a provider key at runtime. `POST /v1/admin/credentials` answers 501. A body that looks like an OAuth refresh token is refused with a 400.

### Stored-key header

Two providers speak `anthropic_messages` and authenticate a stored key in different headers.

| Provider | Header | `auth.style` |
|---|---|---|
| Anthropic | `x-api-key` | `x_api_key` (default) |
| OpenRouter `/messages` | `Authorization: Bearer` | `bearer` |

Set `"auth": {"env": "OPENROUTER_API_KEY", "style": "bearer"}` on the OpenRouter Messages definition. An unknown spelling stops the load. A wrong header gives a 401 on every turn, and a 401 looks like a bad key, not a wrong file.

Roundhouse does not guess the style from the URL or the provider name. A guess mis-authenticates every gateway that fronts either provider under another host name. `auth.style` has no effect on the OpenAI wires, which have one spelling.

The client always sends `anthropic-version: 2023-06-01`. That is the only value the Messages API has ever had. Do not change it. Newer features are gated by the `anthropic-beta` header.

## Local side of the comparison

These fields sit in the catalog so that both halves of one comparison are written in one file. The shipped binary attaches no local fleet. It quotes the catalog and nothing else. These values matter for a binary that does attach one.

### Local latency

The quote for a local worker is `local_base_ttft_ms` plus `local_ttft_ms_per_prefill_token` times the effective prefill tokens that Dynamo reports. Set the slope to `1000 / tokens_per_second`, where the rate is a measured prefill rate. Leave it at zero until you have a measurement. An invented slope loses real turns to a hosted model. Negative values stop the load.

### Fleet residency bound

`fleet_quote_deadline_ms` bounds the Dynamo residency call on a turn that can fail open. If the fleet errors or is late, the turn drops the local candidate and routes among its hosted targets. It records `local_quote_skipped` as `fleet_error` or `fleet_timeout`. Zero is refused, because it drops every local candidate while the fleet is healthy.

A turn that cannot fail open ignores this bound. That is a turn whose policy admits only local targets, a turn with no credential for any hosted provider, or a turn whose frontier cadence is spent. Such a turn waits up to the turn deadline. A fleet failure still fails it, so a local-only session never goes to a hosted model.

### Local capacity

`local_capacity_price` is optional:

```json
"local_capacity_price": { "input_per_mtok_usd": 0.4, "output_per_mtok_usd": 1.6 }
```

With it, the quote for a local turn is the effective prefill tokens at the input rate, plus the expected output tokens at the output rate. Matched local tokens are free. A local worker can now lose on cost to a cheaper hosted target. The dashboard reports local capacity spend and a routing saving net of it.

Without it, local turns quote $0 and the dashboard marks local cost as unpriced, never as free. An approximate figure is fine. The one rule is that it must not make local look cheaper than it is. The object has no cache rates. An unknown key inside it, or a negative rate, stops the load.

A budget never refuses a local candidate because of this price. The price is GPU time that the deployment owns, not budget spend. So an exhausted budget still degrades to local.

### Correlaries and the capability gate

A correlary says that one of your local models stands in for a hosted model when the dashboard prices a saving.

```json
{ "local_model": "my-local-model", "provider": "openai", "model": "gpt-x", "note": "Why these two compare." }
```

The `note` is shown verbatim on the dashboard. A reader who decides whether to trust the savings figure is deciding whether to trust that sentence. A correlary that names a model not in the catalog stops the load, because that model's traffic goes unpriced without a sign.

The capability gate decides whether two models can be priced against each other. It compares their quality priors, and they must be within `capability_band`. The gate is the only thing that stops a small local model from being priced against a flagship.

## Source quality priors

`quality_prior` is a hand-written number unless you import it. `import-benchmarks` is a binary target of `roundhouse-fleet`. No shipped binary links it, and nothing in a running server calls OpenRouter. It writes files that you read and merge.

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
| `--from-file` | Read a saved response instead of calling the route. |

Use `--from-file` while you tune a filter. The route allows 30 requests a minute and 500 a day for each account.

The tool reads `GET /api/v1/benchmarks`. That route needs a key. Its response carries a version, an `as_of` date, and a citation. The unauthenticated `Model.benchmarks` block in `GET /models` has none of these, so the tool refuses it as an input. An unversioned score re-ranks models without notice when the upstream leaderboard moves.

Normalization rules:

- An OpenRouter `accuracy` value is already on `0..1`, and the tool uses it as it is.
- An Artificial Analysis index is divided by 100. The 100 is a stated denominator, not a measured maximum. A measured maximum makes every prior depend on which models the fetch listed, and a rerun next month re-ranks models you did not touch. The division caps the corpus seen on 2026-08-24 at about 0.631. A low prior narrows what a model can be compared to, so this is the safe direction.
- Each entry records its basis, `openrouter.accuracy` or `artificial_analysis.intelligence_index/100`. One fetch can mix both, and they are not on one scale.

The tool refuses an entry that it cannot attribute. That is an entry with neither a `meta.citation` nor its own `source`. A null `meta.citation` is an ordinary multi-source response, and the tool emits each entry with its own `source`.

The fragment holds model identity and `quality_prior` and nothing else. The attribution that OpenRouter requires on republication is in the provenance file. Keep the two files together. The server looks for `quality-prior.provenance.json` beside the file that `ROUNDHOUSE_CATALOG` names. If it finds one, the dashboard shows the citation under the savings figure. If it finds none, there is no line, and the boot does not fail. A renamed provenance file is not discovered.

## OpenRouter facts

These facts were read from OpenRouter's public API on 2026-08-24, with no API key. Treat them as a snapshot.

- `POST /api/v1/responses` is generally available. It is stateless. `store` must be `false`, and a non-null `previous_response_id` gets a 400. There is no WebSocket route.
- `/api/v1/messages` exists and uses the Anthropic envelope. No page that was fetched states its stability tier, so do not assume it is stable.
- Responses streams carry comment keep-alives, for example `: OPENROUTER PROCESSING`. Roundhouse skips comment and non-`data` lines. It maps `response.failed` and `error` frames to an upstream error. It also accepts a `[DONE]` sentinel.
- After the first token, an error arrives as a stream event while the HTTP status stays 200. A client that counts a 200 plus `[DONE]` as success records failed turns as successful.
- The outbound Responses body names only fields that both the OpenAI and OpenRouter schemas have: `model`, `stream`, `input`, `prompt_cache_key`, `max_output_tokens`, `store`, plus the client's own `tools` and `tool_choice` and the usage settings. Codex sends `client_metadata` and `stream_options`, and OpenRouter's schema has neither. The body is a whitelist, because the fault to prevent is someone adding a field.
- Roundhouse sends `store: false` and never sends `previous_response_id`. It rebuilds every prompt from its own log. A provider-side conversation is a second history that can disagree with the log.
- OpenRouter reports a dollar cost in each usage object. Roundhouse decodes it and logs it. It does not add it to the usage record or to the savings figure.
- Roundhouse sends no OpenRouter routing preference: no `provider` object, no `models` list, and no `session_id`. OpenRouter's defaults apply. Fallbacks are on, and OpenRouter picks the provider. Roundhouse does not read the served model or provider back. If a fallback fires, cost and quality are attributed to the requested model. To pin a provider you must set it on the OpenRouter side.

The provider prices in the catalog example are zeros. Replace each one with the current published rate for the route you call.
