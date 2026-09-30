# Measure cache reuse

This chapter shows how to measure how much of each prompt a provider serves from its cache. It covers the two costs, the test commands, a manual experiment, and measurements of Claude Code captures.

## The two costs

A coding agent resends its whole conversation on every turn. Roundhouse admits only the new suffix to its log, but the provider receives the full prompt. The provider's cache decides how much of it is processed and billed again. That cost is the re-discovery tax.

| Tax | Where it occurs | Expected result |
|---|---|---|
| A | Within one session, as the resent prefix grows each turn. | The cached share of input tokens climbs toward the whole prefix. |
| B | Across sessions that send the same prefix. | The second session opens with a higher cached share than the first. |

The cached share of a turn is `cached_tokens / input_tokens`.

## Where the counts come from

| Dialect | Cached tokens | Cache writes |
|---|---|---|
| OpenAI Responses | `input_tokens_details.cached_tokens` on `response.completed` | Not a separate billed event |
| Anthropic Messages | `cache_read_input_tokens` | `cache_creation_input_tokens` |

The log stores the provider's numbers as reported. If the provider sends none, Roundhouse books its own count as estimated, with zero cached tokens, which is not an observed zero.

Each model row of `/v1/metrics` carries `cache_reuse_evidence`: the reuse the router predicted beside the reuse the provider reported, on the same dispatches. A negative `mean_signed_error` means the router expected more reuse than it got. Only a count the provider stated, zero included, is an observation. See [Metrics and the dashboard](../operations/metrics.md#cache-reuse-evidence).

## How the prefix is marked on Anthropic Messages

Anthropic caches only what an explicit `cache_control` marker names, so a request with no marker reads nothing.

- The first marker goes on the penultimate block, which ends the prefix that the previous turn sent. This turn's new input stays unmarked.
- A second marker goes on the block that the previous request to the same target marked. Anthropic looks back at most 20 blocks from a marker, so without it a turn that appends 20 or more items loses the old entry.
- A request carries at most four markers. If the client's tools already use them, Roundhouse sends fewer or none.
- A one-hour target gets `ttl: "1h"` on every marker. See [Configure providers and the catalog](catalog.md#cache-lifetime-on-anthropic-messages).

On the Responses wire, `prompt_cache_key` steers a request to a node that caches by itself, and Roundhouse always sends one.

## Run the offline suite

The offline suite drives the real engine against a loopback server. It checks the shared driver, the request markers, the configured transport, and the budget refusal, not provider behavior.

```bash
timeout 300 cargo test -p roundhouse-server --test cache_probe
```

## Run the live probe

The `e2e-frontier` feature adds a live probe that sends two turns to one Messages target, the second appending 20 items. It reports each turn's cache reads, writes, and usage provenance. If turn one neither read nor wrote the cache, a zero read on turn two is inconclusive, not evidence against the marker placement.

Supply a catalog with real prices and a deterministic cache model, a pinned `provider/model`, and a USD spend cap. Inject the key through `openv` into the variable that the provider's `auth.env` names. Check the model's minimum cacheable prefix first: the fixture has at least 8,192 words before its marker, which is not a token count.

```bash
openv env ROUNDHOUSE_PROBE_CATALOG=/path/to/catalog.json \
  ROUNDHOUSE_PROBE_MODEL='anthropic/<pinned-model-id>' \
  ROUNDHOUSE_PROBE_LIMIT_USD='<approved-cap>' \
  timeout 300 cargo test -p roundhouse-server --features e2e-frontier \
    --test cache_probe live -- --nocapture
```

The live test has no `#[ignore]`, so the feature adds it to an unfiltered run. Missing configuration fails before any request. The project budget governs both turns, each with 16 output tokens and a 30-second deadline.

## Measure Tax A and Tax B by hand

You need a running Roundhouse, a catalog with a real target, and a client that can send `POST /v1/responses`.

1. Write a reference document of about 4 KB, longer than the entry's `min_prefix_tokens` (1024 in the example).
2. Write about 20 short questions that only the document answers. Ask for "not specified" when it does not, to keep answers short.
3. Send the document as `instructions` on every turn, as a stateless OpenAI client does.
4. Send the questions as a growing conversation, with the whole conversation in `input`.
5. Give each session its own `prompt_cache_key`, such as `alice-run1`. A request with no `thread-id`, `session-id`, or `prompt_cache_key` gets a 422.
6. For Tax B, run a second session with the same document and another key.

```bash
curl -N http://127.0.0.1:8080/v1/responses \
  -H "x-roundhouse-key: $ROUNDHOUSE_API_KEY" \
  -H 'content-type: application/json' \
  -d '{
    "instructions": "<the reference document>",
    "input": [ {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "<question 1>"}]} ],
    "stream": true,
    "prompt_cache_key": "alice-run1"
  }'
```

In Open mode, omit the key header. Compute `input_tokens_details.cached_tokens / input_tokens` from `usage` on each `response.completed` event, then read the model rows of `GET /v1/metrics`.

No measured run of this experiment is recorded. Expect the cached share to rise as the session grows, and the second session to open higher than the first.

## Claude Code and cross-session reuse

Tax B is small for Claude Code, because the start of its request differs between sessions.

Setup: the fixtures in `crates/roundhouse-server/tests/fixtures/` are bodies that Claude Code 2.1.251 and 2.1.257 sent to a loopback mock, with synthetic prompts such as "say hi". `scripts/measure-prefix-divergence.py` approximates `canonicalize` and `Item::render` at revision `2dd40dd`: its byte counts differ from the Rust render, but not the positions of the first difference. Tokens are bytes divided by 4. Reproduce the table with:

```bash
python3 scripts/measure-prefix-divergence.py crates/roundhouse-server/tests/fixtures
```

Shared bytes are given in Roundhouse item order, and in Anthropic order (tools, system, messages).

| Comparison | First difference | Shared bytes (Roundhouse order) | Shared bytes (Anthropic order) |
|---|---|---|---|
| Two 2.1.257 sessions, with a different prompt, directory, day, and MCP tools | Item 0 (a fingerprint), byte 60 | 60 | 45,990 (about 11.5k tokens) |
| One session, turn 1 to turn 2, in a new process | Item 2 (model-name text), byte 4,934 | 5,096 | 4,199 |
| One session, turn 2 to turn 3 | None. The conversation only appends. | 17,223 (all) | 63,667 |
| One session, a tool loop in one process | None. The conversation only appends. | 17,249 (all) | 56,861 |
| 2.1.251 and 2.1.257, the same prompt | Item 0 (the version string), byte 58 | 58 | 396 |

What this shows:

- Inside one process, Claude Code only appends, so Tax A reuse works.
- A new process changes item 2. A new session, day, or directory changes items 0, 2, 3, and 4.
- In Anthropic order the tools come first, and two sessions share about 46 KB. Whether Anthropic's cache reuses those bytes is not tested here.
- A client upgrade changes the first tool description, which starts a new prefix for every session.

The first 2.1.257 request has six items of 87, 75, 9,670, 314, 14, and 7,106 bytes. They are two developer items, the main system prompt, a date reminder, the prompt, and a system item. Its 21 tool schemas are 45,991 bytes of compact JSON. With MCP tools there are 23 schemas of 46,416 bytes.

## Relation to NeMo Relay

NeMo Relay's Adaptive Cache Governor (Relay commit `c37b551`, `crates/adaptive/src/acg/economics.rs`) plans provider cache markers offline. It places up to `max_cache_breakpoints` markers at semantic boundaries from assumed reuse horizons, and rewrites requests. Roundhouse does the complementary job. It predicts whether each target's cache is warm, measures the realized hit ratio, and uses the prediction to choose who serves a turn.
