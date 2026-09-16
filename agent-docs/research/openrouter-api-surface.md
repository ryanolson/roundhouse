<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

> **Status: evidence.** Produced 2026-08-24 as M10 stage 0. OpenRouter's API surface, model ids and prices, and the versioned benchmark index, fetched live.
> Clone/snapshot paths named inside refer to the research session's own
> workspace; the revisions and URLs are what pin each claim. The ruling this
> evidence supports is agent-docs/PLAN-frontier-selection.md.

# M10 stage 0 — Dive 2: openrouter-surface

Facts only. Every claim below carries a URL fetched **2026-08-24** or a path into a snapshot
taken that day. No design proposals; where a fact forces a roundhouse decision the fork is
named, not resolved.

## 0. Provenance and reproducibility

Two artefacts were downloaded today and every "live" claim is checkable against them:

| Snapshot | Source URL | Fetched | Size |
|---|---|---|---|
| `scratchpad/or-models.json` | `https://openrouter.ai/api/v1/models` (HTTP 200, **no auth**) | 2026-08-24 | 682 713 B, 416 models |
| `scratchpad/or-openapi.json` | `https://openrouter.ai/openapi.json` (HTTP 200, no auth) | 2026-08-24 | 1 845 165 B, 76 paths |
| `scratchpad/ep-canon.json`, `ep-id.json` | `…/api/v1/models/moonshotai/kimi-k3{-20260715,}/endpoints` | 2026-08-24 | both HTTP 200 |
| `scratchpad/ep-ds.json` | `…/api/v1/models/deepseek/deepseek-v4-pro/endpoints` | 2026-08-24 | HTTP 200 |

`https://openrouter.ai/openapi.json` (also `/openapi.yaml`, named at
`https://openrouter.ai/docs/api-reference/overview`) is the authoritative route and schema list
and is the citation of record for everything marked "openapi" below; the prose doc pages are
cited where they say something the spec does not.

---

## Q1 — API surface

### Base URL and auth

- Base URL `https://openrouter.ai/api/v1` — `https://openrouter.ai/docs/quickstart`.
- Auth: `Authorization: Bearer <OPENROUTER_API_KEY>`. openapi `components.securitySchemes` has
  exactly two entries, `apiKey` and `bearer`, both `{"type":"http","scheme":"bearer",
  "description":"API key as bearer token in Authorization header"}`, and the document-level
  `security` is `[{"apiKey":[]}]`. **The string `x-api-key` does not appear anywhere in
  openapi.json (grep count 0), and neither does `anthropic-version`.** See Q5.
- Optional headers, from openapi `components.parameters`:
  - `HTTP-Referer` (`AppIdentifier`) — "should be your app's URL and is used as the primary
    identifier for rankings".
  - `X-OpenRouter-Title` (`AppDisplayName`) and `X-OpenRouter-Categories` — the quickstart page
    lists `X-OpenRouter-Title`, **not** the `X-Title` spelling that older third-party integration
    guides use. `X-Title` appears nowhere in openapi.json. Treat `X-OpenRouter-Title` as the
    current name; I found no doc page today that mentions `X-Title` at all, so I am not asserting
    it was ever renamed — only that the live spec names the `X-OpenRouter-*` forms.
  - `X-OpenRouter-Metadata: enabled` — per-route header on `/chat/completions`, `/responses` and
    `/messages`: "Opt-in to surface routing metadata on the response under `openrouter_metadata`.
    Defaults to `disabled`. The legacy header `X-OpenRouter-Experimental-Metadata` …".

### Route enumeration (openapi `paths`, 76 entries — the ones that matter here)

```
/chat/completions   [post]      /responses   [post]      /messages   [post]
/models             [get]       /models/{author}/{slug}/endpoints [get]
/model/{author}/{slug} [get]    /models/count [get]      /models/user [get]
/generation         [get]       /generation/content [get]
/benchmarks         [get]       /providers   [get]       /key [get]   /credits [get]
/presets/{slug}/chat/completions | /messages | /responses   [post]
```
There is **no websocket route** anywhere in the spec (relevant to Q5 / codex).

### GET /models

- Unauthenticated, `security: none` on the operation (openapi `paths["/models"].get`).
- Query params: `offset`, `limit` (default 500, max 1000; "when both are omitted, the full list is
  returned"), `category`, `supported_parameters`, output-modality filter.
- Response `ModelsListResponse` → `data[]` of `Model`. Documented `Model` properties:
  `id, canonical_slug, hugging_face_id, name, created, description, context_length,
  architecture, pricing, top_provider, per_request_limits, supported_parameters,
  default_parameters, supported_voices, knowledge_cutoff, expiration_date, links, reasoning,
  benchmarks, alias_target`.
- **Pricing is a map of USD-per-token decimal *strings***, not per-million numbers, and it has
  far more than two members. `PublicPricing` (openapi) documents:
  `prompt`, `completion` ("Price in USD per token"), `request` ("per request"),
  `input_cache_read` ("per cached input token (read)"), `input_cache_write` ("default 5-minute
  cache-write rate"), `input_cache_write_1h`, `internal_reasoning`, `image`, `image_token`,
  `image_output`, `audio`, `audio_output`, `input_audio_cache`, `web_search`,
  `discount` (number: "price is multiplied by (1 - discount)"), and
  `overrides` — "Conditional overrides of the base pricing (e.g. long-context or time-based
  pricing). An entry applies when all of its condition fields (e.g. `min_prompt_tokens`, or the
  `utc_start`/…) …".
  Per-1M USD = `value × 1e6`. The strings are not round: `deepseek/deepseek-v4-pro` prompt is
  `"0.000000522174"` → $0.522174/1M. Exact arithmetic (decimal/rational, or integer
  nano-USD-per-token) is required; f32 loses these.
- **Two different context numbers per row.** Top-level `context_length` ("Maximum context length
  in tokens") vs `top_provider.context_length`. They disagree in practice:
  `deepseek/deepseek-v4-pro` is `1048576` vs `1024000` (`max_completion_tokens: 384000`);
  `deepseek/deepseek-v4-flash-0731` is `1310720` vs `1048576`. The top-level number is the best
  any endpoint offers; `top_provider` is what the default route actually gives you.
- **`pricing` in `GET /models` is one provider's price, not the model's price.**
  `https://openrouter.ai/docs/guides/overview/models` states the `pricing` object is "Pricing from
  the top provider for this model". Verified: `moonshotai/kimi-k3` lists `3/15` per 1M, which is
  the Wafer/BaseTen/Together tier — the cheapest endpoint (Sail Research) is `2.6/13` and the
  dearest (Morph fast) is `6/22.5`.

### POST /chat/completions

- Request (openapi + `https://openrouter.ai/docs/api-reference/overview`): `messages` **or**
  `prompt`; `model`, `models`, `route`, `provider`, `plugins`, `response_format`, `stop`,
  `stream`, `max_tokens`, `temperature`, `tools`, `tool_choice`, `seed`, `top_p`, `top_k`,
  `frequency_penalty`, `presence_penalty`, `repetition_penalty`, `logit_bias`, `top_logprobs`,
  `min_p`, `top_a`, `prediction`, `user`, `debug`.
- Response: `{id, choices[], created, model, object: 'chat.completion'|'chat.completion.chunk',
  system_fingerprint?, usage?}`. Tool calls are supported (`tools`/`tool_choice` are first-class
  and per-model — see the `supported_parameters` array in each `Model` row).
- Streaming: SSE, `data: {json}` lines, terminated by `data: [DONE]`, with periodic **comment
  keep-alives** — literally `: OPENROUTER PROCESSING`, documented at
  `https://openrouter.ai/docs/api-reference/streaming` as "Comment payload can be safely ignored
  per the SSE specs". A parser that assumes every non-blank line begins `data:` breaks here.
  Usage arrives in the final chunk.

### POST /responses — an OpenAI Responses-compatible endpoint **exists and is GA**

Status: **GA**, since **2026-07-25**.
- `https://openrouter.ai/docs/changelog` entry dated **July 25, 2026**: "The Responses API is now
  GA. `beta.responses` was an OpenAPI grouping tag and SDK namespace, never a URL."
- `https://openrouter.ai/docs/api_reference/responses/overview` — "Generally Available (GA)",
  endpoint `https://openrouter.ai/api/v1/responses`.
- openapi `paths["/responses"].post`: `operationId: createResponses`, summary "Create a response",
  description "Creates a streaming or non-streaming response using **OpenResponses API format**".

Live probe (2026-08-24, no credentials used):

| Probe | Result | Reading |
|---|---|---|
| `POST /api/v1/responses` body `{}` | **401** `{"error":{"message":"No cookie auth credentials found","code":401}}` | route exists, auth-gated |
| `POST /api/v1/responses` + junk Bearer | **401** `{"error":{"message":"User not found.","code":401}}` | route exists |
| `POST /api/v1/bogus-nonexistent-route` | **404**, `text/html` Next.js page | this is what *absent* looks like |
| `POST /api/v1/responses/resp_123` | **404** `{"error":{"message":"Not Found","code":404}}` | no retrieve/cancel sub-route |

The doc + spec evidence carries the GA verdict; the status codes carry the existence verdict and
discriminate it from the HTML-404 signature of a genuinely absent route.

Constraints on the Responses surface (openapi `ResponsesRequest`, corroborated by the overview
page):
- `store` is `{"const": false, "default": false}` — `store: true` is rejected.
- `previous_response_id` description, verbatim: *"Not supported. The Responses API is stateless:
  no responses are stored, so a previous response cannot be referenced. Requests with a non-null
  value are rejected with a 400 error. Send the full conversation history in `input` instead."*
- `background` requests unsupported (overview page).
- Everything else looks like OpenAI's: `input, instructions, max_output_tokens, include,
  parallel_tool_calls, reasoning, text, tool_choice, tools, truncation, temperature, top_p,
  top_logprobs, metadata, prompt_cache_key, safety_identifier, service_tier, user, stream`.
- OpenRouter-only additions: `models` (fallback list), `provider` (`ProviderPreferences`),
  `plugins`, `route` (deprecated), `session_id` (sticky-routing key, ≤256 chars, "routing all
  requests in the session to the same provider to maximize prompt cache hits"; also settable as
  header `x-session-id`, body wins), `max_tool_calls`, `stop_server_tools_when`,
  `prompt_cache_options`, `cache_control`, `modalities`, `image_config`, `trace`, `debug`,
  `top_k`.
- **Not present in `ResponsesRequest`**: `client_metadata`, `stream_options`. Whether OpenRouter
  rejects unknown top-level fields or ignores them is **untested** — the route authenticates
  before validating, so an unauthenticated probe cannot reach the validator. (Other routes do
  validate first: `POST /images/generations` with `{}` and no auth returns 400 `ZodError`.)

Responses SSE event names — **49 event variants** in openapi `StreamEvents.anyOf`, literal `type`
strings taken from each schema's `example` block. Standard OpenAI Responses vocabulary:

```
response.created                         response.in_progress
response.completed                       response.incomplete            response.failed
error                                    response.debug
response.output_item.added / .done       response.content_part.added / .done
response.output_text.delta / .done       response.output_text.annotation.added
response.refusal.delta / .done
response.function_call_arguments.delta / .done
response.reasoning_text.delta / .done
response.reasoning_summary_part.added / .done
response.reasoning_summary_text.delta / .done
response.custom_tool_call_input.delta / .done
response.apply_patch_call_operation_diff.delta / .done      <- codex's apply_patch tool
response.web_search_call.{in_progress,searching,completed}
response.code_interpreter_call.{in_progress,interpreting,completed}
response.code_interpreter_call_code.{delta,done}
response.image_generation_call.{in_progress,generating,partial_image,completed}
response.fusion_call.*  (9 events)                          <- OpenRouter-only, not in OpenAI's set
```

### POST /messages — an Anthropic Messages-compatible endpoint **exists**

- openapi `paths["/messages"].post`: `operationId: createMessages`, summary "Create a message",
  description "Creates a message using the **Anthropic Messages API format**. Supports text,
  images, PDFs, tools, and extended thinking."
- Live probe: `POST /api/v1/messages` `{}` → **401** with an **Anthropic-shaped envelope**,
  `{"type":"error","error":{"type":"authentication_error","message":"No cookie auth credentials
  found","error_type":"authentication"},"request_id":null}` — distinct from the OpenAI-shaped
  envelope the other routes return. That is strong evidence it is a real Anthropic skin, not an
  alias.
- No explicit "GA"/"beta" label found on any page I fetched today. It is in the public OpenAPI
  spec and changelog entries dated **July 24** and **July 25, 2026** modify its schemas
  (`MessagesToolAdditionBlock`, `MessagesToolRemovalBlock`, `ProviderName`), so it is live and
  actively maintained. **I could not find a page that states its stability tier; do not claim GA
  for it.** OpenRouter's own marketing calls it the "Anthropic Skin"
  (`https://openrouter.ai/blog/tutorials/claude-code-openrouter/`, surfaced via site search).
- `MessagesRequest` required: `model, messages`. Also `max_tokens, system, stop_sequences,
  thinking, tools, tool_choice, temperature, top_k, top_p, stream, metadata` plus OpenRouter
  additions `models`, `fallbacks`, `provider`, `route`, `plugins`, `session_id`, `service_tier`,
  `speed`, `output_config`, `context_management`, `cache_control`, `stop_server_tools_when`,
  `trace`.
- Streaming: `MessagesStreamEvents` = `MessagesStartEvent, MessagesDeltaEvent, MessagesStopEvent,
  MessagesContentBlockStartEvent, MessagesContentBlockDeltaEvent, MessagesContentBlockStopEvent,
  MessagesPingEvent, MessagesErrorEvent` — the Anthropic event set. Response content types are
  `application/json` **and** `text/event-stream`.
- `MessagesResult.usage` = `AnthropicUsage` (`input_tokens, output_tokens,
  cache_read_input_tokens, cache_creation_input_tokens, …`) **plus** OpenRouter's `cost`,
  `cost_details`, `is_byok`, `service_tier`, `speed`, `iterations`.

---

## Q2 — model ids, prices, tool calling, provider variants

All from `or-models.json` and the `/endpoints` snapshots, 2026-08-24. Prices are **USD per 1M
tokens** (raw string × 1e6).

### Kimi K3 (Moonshot)

| field | value |
|---|---|
| id | `moonshotai/kimi-k3` |
| canonical_slug | `moonshotai/kimi-k3-20260715` |
| hugging_face_id | `moonshotai/Kimi-K3` |
| created | 2026-07-16 |
| pricing (top provider) | prompt **$3.00**/1M, completion **$15.00**/1M, `input_cache_read` **$0.30**/1M |
| context | `context_length` 1 048 576; `top_provider.context_length` 1 048 576, `max_completion_tokens` 1 048 576, `is_moderated: false` |
| tool calling | **yes** — `supported_parameters` includes `tools`, `tool_choice`, `structured_outputs`, `response_format` |
| reasoning | `mandatory: false`, `default_enabled: true`, efforts `["max","high","low"]`, default `max` |
| AA indexes | intelligence 59.7, coding 76.2, agentic 54.3 |

There is exactly **one** kimi-k3 row — no dated siblings. `~moonshotai/kimi-latest` resolves to it
(see the alias caveat below). Also present: `moonshotai/kimi-k2.7-code`,
`moonshotai/kimi-k2.7-code:batch`, `kimi-k2.6`, `kimi-k2.5`, `kimi-k2-thinking`, `kimi-k2-0905`,
`kimi-k2`.

**15 provider endpoints, 2.3× price spread** (`GET /models/moonshotai/kimi-k3/endpoints`,
`tag → prompt/completion per 1M`):

```
sail-research/fp4   2.60 / 13.00     wafer               3.00 / 15.00
morph/fp4           2.80 / 14.00     baseten/fp8         3.00 / 15.00
deepinfra/bf16      2.85 / 14.25     together            3.00 / 15.00
digitalocean        2.85 / 14.25     moonshotai/mxfp4    3.00 / 15.00
phala               3.00 / 15.00     fireworks           3.00 / 15.00
modal/mxfp4         3.00 / 15.00     chutes/mxfp4        3.00 / 15.00
fireworks/us        3.30 / 16.50     alibaba             3.45 / 17.25
morph/fast          6.00 / 22.50
```
Each endpoint also carries `quantization` (fp4 / fp8 / bf16 / unknown — `deepinfra/bf16`'s
`max_completion_tokens` is only 16 384 while others give 262 144–1 048 576), `status`,
`uptime_last_5m/30m/1d`, `latency_last_30m`, `throughput_last_30m`, `max_prompt_tokens`,
`supports_implicit_caching`.

### DeepSeek V4 — "deepseek v4" is **five concrete models plus an alias**, and the bare ids are frozen

| id | canonical_slug | created | $/1M in | $/1M out | ctx (top) | AA intelligence |
|---|---|---|---|---|---|---|
| `deepseek/deepseek-v4-pro` | `…-20260423` | 2026-04-24 | 0.522174 | 1.044348 | 1 024 000 | 45.3 |
| `deepseek/deepseek-v4-pro-0813` | `…-20260813` | 2026-08-12 | 1.122 | 3.366 | 1 048 575 | **53.2** |
| `deepseek/deepseek-v4-flash` | `…-20260423` | 2026-04-24 | 0.056 | 0.112 | 1 024 000 | 42.1 |
| `deepseek/deepseek-v4-flash-0731` | `…-20260731` | 2026-07-31 | 0.0658 | 0.1316 | 1 048 576 | 51.8 |
| `deepseek/deepseek-v4-flash-vision-exp` | `…-20260821` | 2026-08-21 | 0.22 | 0.66 | 1 048 576 | — |
| `~deepseek/deepseek-v4-flash-latest` | (itself) | 2026-08-01 | 0.04 | 0.08 | 1 048 576 | — |

The unsuffixed `deepseek/deepseek-v4-pro` is **pinned to the April snapshot**, not tracking; the
August line ships as a separate `-0813` id at 2.1×/3.2× the price and +7.9 AA intelligence points.
"DeepSeek V4" in a recipe is therefore ambiguous and must be written as a full id.

All V4 rows support `tools`/`tool_choice`/`structured_outputs`. `deepseek-v4-pro` has **17
endpoints, 3.7× spread**: `streamlake/fp8` 0.522174/1.044348 … `azure/us` 1.91/3.83, with
`together` capped at ctx 512 000 and `alibaba/fp8`/`venice` at 1 000 000.

### Pinning a specific upstream provider

`https://openrouter.ai/docs/features/provider-routing`: **there is no `model@provider` or
`model:provider` shorthand.** Pinning is done with the `provider` object
(`ProviderPreferences`, available on `/chat/completions`, `/responses` and `/messages`):

```json
{ "provider": { "only": ["azure"] } }
{ "provider": { "order": ["anthropic", "openai"], "allow_fallbacks": false } }
```

Field semantics, verbatim from the openapi descriptions:
- `order` — "An ordered list of provider slugs. The router will attempt to use the first provider
  in the subset of this list that supports your requested model, and fall back to the next if it
  is unavailable."
- `only` / `ignore` — allow- and deny-lists, "merged with your account-wide … settings".
- `allow_fallbacks` — "true: (default) when the primary provider (or your custom providers in
  `order`) is unavailable, use the next best provider. false: use only the primary/custom
  provider, and return the upstream error if it's unavailable."
- `sort` — enum `["price","throughput","latency","exacto"]`; "When set, no load balancing is
  performed." Also accepts an object `{"by":"price","partition":…}`.
- `max_price` — "USD price per **million** tokens, for prompt and completion" (note: the *only*
  place in the API where a price is per-million rather than per-token).
- `require_parameters`, `quantizations`, `data_collection` (`allow`/`deny`),
  `zdr`, `enforce_distillable_text`.
- `preferred_max_latency` — "Preferred maximum latency (in seconds) … Endpoints above the
  threshold(s) may still be used, but are **deprioritized** in routing. When using fallback
  models, this may cause a fallback model to be used instead of the primary model."
  `preferred_min_throughput` is the tokens/sec twin. Both accept a number (p50) or per-percentile
  cutoffs. These are *preferences*, not guarantees.

Provider slugs vs endpoint tags: a base slug (`deepinfra`) "matches **all** endpoints for that
provider"; the fuller forms seen in `/endpoints` (`deepinfra/bf16`, `sail-research/fp4`,
`fireworks/us`, `azure/us`, `google-vertex/us-east5`) target one variant or region. `GET
/api/v1/providers` enumerates them.

### Alias rows (`~` prefix) — usable, but they destroy provenance

`Model.alias_target` is documented as "Concrete model targeted by this tilde-latest alias".
12 alias rows live today, e.g. `~moonshotai/kimi-latest → moonshotai/kimi-k3`,
`~openai/gpt-latest → openai/gpt-5.6-sol`, `~anthropic/claude-sonnet-latest →
anthropic/claude-sonnet-5`, `~deepseek/deepseek-v4-flash-latest → deepseek/deepseek-v4-flash-0731`.

Two observed properties of alias rows, both from `or-models.json`:
1. **No `benchmarks` block.** All 12 alias rows have no `artificial_analysis` scores, while their
   targets do. An entry pinned to an alias has no quality index at all.
2. **Their `pricing`/`top_provider` disagrees with the target row.** `~moonshotai/kimi-latest`
   reports 2.6/13 and `top_provider.context_length` 974 842 (the Sail Research fp4 endpoint);
   its target `moonshotai/kimi-k3` reports 3.0/15 and 1 048 576. Same model, two different
   "the price" answers depending on which row you read.
   `~deepseek/deepseek-v4-flash-latest` reports 0.04/0.08 against its target's 0.0658/0.1316.
   Its own `canonical_slug` is the tilde string, i.e. not a stable pin.

Suffix variants: `https://openrouter.ai/docs/guides/overview/models` states "Variant suffixes are
also supported. Append `:free`, `:thinking`, etc. to the slug" and "The endpoint resolves aliases
automatically". The docs index `https://openrouter.ai/docs/llms.txt` enumerates seven variant
pages under `/docs/guides/routing/model-variants/`:

| variant | index line |
|---|---|
| `:free` | "Access free models with the `:free` variant" |
| `:extended` | "Extended context windows with `:extended`" |
| `:exacto` | "Route requests with quality-first provider sorting" |
| `:thinking` | "Enable extended reasoning with `:thinking`" |
| `:online` | "Real-time web search with `:online`" |
| **`:nitro`** | "High-speed model inference with `:nitro`" |
| **`:floor`** | "Lowest-cost model inference with `:floor`" |

`:nitro` and `:floor` are therefore real (they are the id-suffix twins of
`provider.sort: "throughput"` and `"price"`; `:exacto` matches the fourth `sort` enum value). A
`:batch` suffix is live in the data (`moonshotai/kimi-k2.7-code:batch`) but has no page in that
index. Each suffix silently changes which endpoint serves — and therefore the price and the
quality — without changing the base model name.

Both id forms resolve on the endpoints route: `…/models/moonshotai/kimi-k3/endpoints` and
`…/models/moonshotai/kimi-k3-20260715/endpoints` each returned 200. **Neither response carries a
canonical slug** — both bodies report `{"id":"moonshotai/kimi-k3","canonical_slug":null}`, so this
route never populates that field and the dated pin must come from `GET /models`.

---

## Q3 — usage, cost, generation stats, rate limits

### Per-response usage: always on, and it includes money

`https://openrouter.ai/docs/use-cases/usage-accounting`, verbatim: *"The `usage: { include: true
}` and `stream_options: { include_usage: true }` parameters are deprecated and have no effect.
Full usage details are now always included automatically in every response."*

- **Chat Completions** `ResponseUsage` (openapi + overview page):
  `prompt_tokens`, `completion_tokens`, `total_tokens`,
  `prompt_tokens_details{cached_tokens, cache_write_tokens?, audio_tokens?, video_tokens?}`,
  `completion_tokens_details{reasoning_tokens?, audio_tokens?, image_tokens?}`,
  `cost`, `cost_details`, `is_byok`, `server_tool_use`.
- **Responses** `Usage` (openapi): `input_tokens`, `input_tokens_details{cached_tokens}`,
  `output_tokens`, `output_tokens_details{reasoning_tokens}`, `total_tokens`, plus
  `cost` ("Cost of the completion"),
  `cost_details{upstream_inference_cost, upstream_inference_input_cost,
  upstream_inference_output_cost}`, `is_byok`, `server_tool_use_details`.
- **Messages** `usage` = `AnthropicUsage` + `cost`, `cost_details`, `is_byok`, `service_tier`,
  `speed`, `iterations`.
- **Units:** the usage-accounting page says `cost` is "Cost in credits"; `cost_details
  .upstream_inference_cost` is "the actual upstream provider cost" and is only populated for BYOK
  requests. Credits are OpenRouter's billing unit; the page's own sample prints
  `` `Cost: ${chunk.usage.cost} credits` ``. **Whether 1 credit == 1 USD is not stated on any
  page I fetched today** — do not assume the identity without confirming against `GET
  /api/v1/credits` on a real account.
- In streaming, usage arrives "in the last SSE message for streaming responses".

### `GET /api/v1/generation?id=<gen-id>` — the post-hoc ledger

Required query param `id` (probe: `GET /api/v1/generation` with no id → 400 `ZodError`,
`"path":["id"]`). `GenerationResponse.data` fields (openapi):

```
id, upstream_id, model, provider_name, router, origin, api_type, streamed, cancelled,
created_at, finish_reason, native_finish_reason,
tokens_prompt, tokens_completion, native_tokens_prompt, native_tokens_completion,
native_tokens_reasoning, native_tokens_cached, num_media_prompt, num_media_completion,
num_search_results, num_input_audio_prompt, num_fetches,
total_cost, upstream_inference_cost, cache_discount, usage, is_byok,
latency, generation_time, moderation_latency,
app_id, http_referer, user_agent, external_user, session_id, preset_id, workspace_id,
data_region, service_tier, request_id, provider_responses, response_cache_source_id,
web_search_engine
```

`latency` and `generation_time` (both ms in the schema example: 1250 / 1200) are the TTFT-adjacent
numbers available after the fact; `total_cost` is the authoritative charge; `provider_name` is
the authoritative record of who actually served. `GET /api/v1/generation/content` also exists.

The 401 probe's `access-control-expose-headers` names
**`X-Generation-Id, X-Provider-Name, request-id, cf-ray`**. That is a CORS declaration of what a
browser *may* read if the header is present — it is **not** evidence that those headers are
emitted on a 200. Presence on a successful call is unverified (no key). If they are emitted, the
served provider and the generation id are readable without a follow-up `/generation` call.

### Rate limits — `https://openrouter.ai/docs/api-reference/limits`

- Free-tier (`:free` ids): <10 credits ever purchased → 20 req/min, 50 req/day; ≥10 credits →
  20 req/min, 1000 req/day. Cloudflare DDoS protection sits on top.
- **429 Too Many Requests**. The limits page names three response headers —
  `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset` — and says "Honor the
  `Retry-After` header when present." Two separate fetches of that page (the rendered
  `/docs/api-reference/limits` and the source `/docs/api_reference/limits.md`) both name those
  three, so this is a doc claim rather than a summarizer artefact. **It is not an observed
  claim**: the live 401 I captured listed only `X-Generation-Id, X-Provider-Name, request-id,
  cf-ray` in `access-control-expose-headers`, with no `X-RateLimit-*`. Whether the headers are
  actually emitted (and whether a browser client can read them) is unverified — no key, and I did
  not trigger a 429.
- **Mid-stream limits do not surface as 429.** Because HTTP 200 has already been sent, a
  mid-stream rate limit arrives as an SSE event carrying `finish_reason: "error"`.
- Credit limits are separate: negative balance → **402 Payment Required**; per-key caps checked
  via `GET /api/v1/key`, whose shape is
  `{label, limit, limit_reset, limit_remaining, include_byok_in_limit, usage, usage_daily,
  usage_weekly, usage_monthly, byok_usage, is_free_tier}`.
- `GET /api/v1/benchmarks` has its own limit stated in its openapi description: **"Rate-limited to
  30 requests/minute per key and 500 requests/day per account."**

---

## Q4 — the intelligence / benchmark indexes

Two independent surfaces, and they differ in exactly the way provenance cares about.

### (a) Embedded in every `GET /models` row — documented, undated

`Model.benchmarks` is a **documented** field: `ModelBenchmarks`, "Third-party benchmark rankings
for this model. Omitted when no benchmark data is available."

- `benchmarks.artificial_analysis` = `AABenchmarkEntry`, three required members:
  `intelligence_index`, `coding_index`, `agentic_index` — each "Artificial Analysis … Index
  composite score. Higher is better." No scale bound is documented.
- `benchmarks.design_arena[]` = `DABenchmarkEntry`: `arena`, `category`, `elo`
  ("ELO rating from head-to-head arena battles"), `win_rate` ("as a percentage (0–100)"), `rank`,
  `avg_generation_time_ms`, `tournament_stats`.

Coverage in today's snapshot: 416 models total, **227 carry a `benchmarks` block, 147 carry a
non-null `artificial_analysis.intelligence_index`.** Observed range of `intelligence_index`
across those 147: **[5.5, 63.1]**.

**The models-list `benchmarks` block carries no date and no dataset version.** Normalizing to
`quality_prior`'s 0.0..=1.0 is therefore a choice with no upstream anchor: dividing by 100 caps
the corpus at 0.631 today, and dividing by the observed max makes the number a function of which
416 models happened to be listed on the fetch date. An importer must stamp its own fetch date
because the response supplies none.

### (b) `GET /api/v1/benchmarks` — the versioned, attributable surface

openapi description, verbatim: *"Unified benchmark endpoint that aggregates scores from multiple
benchmark sources (Artificial Analysis, Design Arena, and OpenRouter's own tau-bench, GPQA, and
web-search evals). Filter by source to reproduce the exact shapes from the legacy per-source
endpoints, or use `task_type` to find models suited for specific workloads. … Authenticate with
any valid OpenRouter API key. Rate-limited to 30 requests/minute per key and 500 requests/day per
account."* (Requires a key; I did not call it. Changelog entry **July 29, 2026** records schema
changes to it.)

- Query params: `source` ∈ `artificial-analysis | design-arena | openrouter`;
  `task_type` ∈ `coding | intelligence | agentic | search`;
  `benchmark_type` ∈ `gpqa_diamond | tau_bench_verified_airline | search_browsecomp | search_hle |
  search_dsqa | search_widesearch`; `include_run_config`, `search_engine`, `search_surface`.
- Response `UnifiedBenchmarksResponse` = `{data[], meta}` and **`meta` is the provenance record**:
  - `as_of` — "ISO-8601 timestamp of when this data was last updated."
  - `version` — "Dataset version." (`"v1"` in the schema example)
  - `citation` — "**Required attribution when republishing this data**, or null when results span
    multiple sources (attribute each item individually by its `source` discriminator)."
  - `source_url` — "URL of the upstream data source."
  - `source`, `task_type` (the filters applied), `model_count`.
- Item shapes:
  - `UnifiedBenchmarksAAItem`: `model_permaslug` ("Stable OpenRouter model identifier"),
    `display_name`, `intelligence_index`, `coding_index`, `agentic_index`, `pricing`, `source`.
  - `UnifiedBenchmarksORItem` (OpenRouter's own evals): `benchmark_type`, **`accuracy`
    ("Aggregate accuracy score from 0 to 1")**, `accuracy_stddev`, `total_tasks`,
    `avg_cost_per_task` ("in USD"), **`last_run_timestamp`**, `model_permaslug`, `display_name`.
  - `UnifiedBenchmarksDAItem`: as (a), plus `avg_generation_time_ms`, `tournament_stats`.
  - `UnifiedBenchmarksSearchItem` (+ `UnifiedBenchmarksSearchRunConfig`, gated behind
    `include_run_config`, whitelisted to "agent turn count, reasoning effort, and temperature so
    future harness configuration changes do not change the public contract").

**What an import must record for provenance**, reading straight off the schema: the `source`
discriminator per item, `meta.version`, `meta.as_of`, `meta.citation` (attribution is stated as
*required* for republication — the savings dashboard republishes), `meta.source_url`, the
`model_permaslug` the score is keyed on (not the request-time `id`), and the local fetch
timestamp. The models-list route supplies none of `version`/`as_of`/`citation`, so (b) is the
defensible input and (a) is the convenient one. That is a fork, not a conclusion.

Note the scale collision: AA indexes are ~0–100 unbounded-in-doc; the OpenRouter-native items are
`accuracy` **already on 0..1** with a stddev, which is the only index here that is both natively
in `quality_prior`'s range and carries an uncertainty and a run timestamp.

---

## Q5 — what would surprise an OpenAI-client (or an Anthropic-client) pointed at OpenRouter

1. **SSE comment keep-alives.** `: OPENROUTER PROCESSING` lines are injected into the stream. A
   line-oriented parser must skip lines beginning `:` before attempting JSON.
   (`https://openrouter.ai/docs/api-reference/streaming`)
2. **Errors after HTTP 200.** Pre-stream errors return a real 4xx/5xx *and may be silently
   retried against a backup provider*; once the first token is committed, errors arrive as an SSE
   event with `finish_reason: "error"` and the stream terminates, with HTTP still 200.
   (`https://openrouter.ai/docs/api-reference/errors`, `…/limits`) A client that treats HTTP 200 +
   clean `[DONE]` as success will silently record failed turns as successful.
3. **Error envelope.** `{"error": {"code": <number>, "message": <string>, "metadata"?: {...}}}`,
   and "the HTTP status code matches `error.code`". `metadata` carries `provider_name`,
   `error_type` (e.g. `rate_limit_exceeded`, `authentication`), `provider_code` (omitted on 500s).
   Moderation blocks carry `{reasons[], flagged_input (≤100 chars), provider_name, model_slug}`.
   Codes in use: 400, 401, 402 (insufficient credits), 403 (guardrail), 408, 429,
   **502 "Chosen model unavailable"**, **503 "No provider meets routing requirements"** — 502/503
   are routing outcomes, not upstream outages, and 402 is a class OpenAI clients do not model.
   **The `/messages` route returns a different envelope**: `{"type":"error","error":{"type":…,
   "message":…,"error_type":…},"request_id":null}` (observed live).
4. **`/messages` wants `Authorization: Bearer`, not `x-api-key`.** Probed live: `x-api-key:` +
   `anthropic-version:` → `"Missing Authentication header"`; junk `Authorization: Bearer` →
   `"User not found."`. Neither header name appears anywhere in openapi.json. Corroborated by
   OpenRouter's own integration guide
   `https://openrouter.ai/docs/guides/community/anthropic-agent-sdk.md`, which sets
   `ANTHROPIC_BASE_URL="https://openrouter.ai/api"`, `ANTHROPIC_AUTH_TOKEN="$OPENROUTER_API_KEY"`
   and — verbatim — `ANTHROPIC_API_KEY=""`, "Important: Must be explicitly empty". An Anthropic
   SDK configured the native way (api_key → `x-api-key`) fails; one configured with an auth token
   (→ `Authorization: Bearer`) works. Note the base URL there is `…/api`, not `…/api/v1` — the
   SDK appends `/v1/messages` itself.
5. **The served model and provider can differ from the requested one.** `models[]` (fallback
   list) and `provider.allow_fallbacks: true` (the **default**) both permit substitution, and
   `preferred_max_latency`/`preferred_min_throughput` explicitly say a fallback *model* may be
   used instead of the primary. The response's `model` field is documented as "which model was
   actually used"; `X-Provider-Name` / `openrouter_metadata.endpoints.available[].selected` /
   `GenerationResponse.provider_name` name the endpoint. **Cost or quality attributed to the
   requested model rather than the returned one is wrong whenever a fallback fires.**
   `openrouter_metadata` (header `X-OpenRouter-Metadata: enabled`) carries `requested`,
   `strategy`, `attempt`, `attempts[]`, `endpoints{available[],total}`, `region`, `is_byok`,
   `pipeline[]` — the full routing trace.
6. **Silent parameter dropping.** `require_parameters` defaults false, and the description is
   explicit: "If this setting is omitted or set to false, then providers will receive only the
   parameters they support, **and ignore the rest**." A request specifying `tools` against an
   endpoint that lacks them is not an error by default.
7. **Cancellation is only honoured while streaming.** "Cancellation only works for streaming
   requests with supported providers. For non-streaming requests or unsupported providers, the
   model will continue processing and you will be billed for the complete response."
8. **`store: true` and any non-null `previous_response_id` are 400s on `/responses`.** Checked
   against codex at the Cargo pin `6344a655a5966f92e009a74928fb0559b41f9093`:
   `codex-rs/core/src/client.rs:931` sets `store: false` on the HTTP `ResponsesApiRequest`, and
   that struct has **no** `previous_response_id` field. (The evidence is the struct literal at
   `client.rs:923-939`: it has no `..` rest-pattern, and Rust requires exhaustive field
   initialization, so the 14 fields listed there are the whole struct.) `previous_response_id`
   appears only on the **websocket** transport (`ResponseCreateWsRequest`, `client.rs:1697-1706`,
   `codex-rs/codex-api/src/endpoint/responses_websocket.rs`). OpenRouter publishes **no websocket
   route**. So the stateless constraint is compatible with codex's SSE path and incompatible with
   its websocket path; which one a session takes is a roundhouse question, not an OpenRouter one.
   Separately, codex's HTTP payload carries `client_metadata` and `stream_options`, neither of
   which is in OpenRouter's `ResponsesRequest` — unknown-field handling is **untested** (see Q1).
9. **`session_id` is a routing lever, not just a tag.** Setting it makes routing sticky to one
   provider "to maximize prompt cache hits" — which is the difference between paying
   `input_cache_read` ($0.30/1M for kimi-k3) and `prompt` ($3.00/1M) on resent history. It is
   also settable as the `x-session-id` header, with the body value winning.
10. **`GET /models` needs no key**, so a catalog importer can run unauthenticated; `/benchmarks`,
    `/generation`, `/key`, `/credits` all require one.
11. Cloudflare sits in front (`server: cloudflare`, `cf-ray`), and a `__cf_bm` cookie is set on
    responses. The 401 message on unauthenticated calls is literally
    `"No cookie auth credentials found"`, which reads as a browser-session error but is the
    ordinary missing-Bearer response.

---

## Open items I could not close today

- **`/messages` stability tier.** Searched: `/docs/llms.txt` (the complete docs index) lists **no
  page titled for the Messages API at all** — only `/docs/guides/community/anthropic-agent-sdk`,
  which does not state a tier either. The `create-messages` API-reference URL surfaced by site
  search 404s. Conclusion stands: it is in the public OpenAPI spec, actively changed in the July
  2026 changelog, and **no page labels it GA or beta**. Do not claim either.
- **Credits ↔ USD.** `usage.cost` is documented as credits; no page I read states the conversion.
- **Unknown-field strictness** on `/responses` and `/chat/completions` — unreachable without a key
  (both authenticate before validating).
- **`X-RateLimit-*` headers** — documented on two fetches of the limits page, never observed;
  absent from the CORS expose list on the responses I did capture.
- **`/benchmarks` live payload** — key-gated; everything in Q4(b) is read from the OpenAPI schema
  and its examples, not from a real call.
- **Nothing here was verified with a real API key.** Every 2xx in this report is from an
  unauthenticated public route (`/models`, `/models/**/endpoints`, `/openapi.json`); every
  inference route was probed only far enough to distinguish 401-exists from 404-absent.
