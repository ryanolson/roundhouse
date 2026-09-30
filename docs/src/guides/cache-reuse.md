# Measure cache reuse

This chapter shows how to measure how much of each prompt a provider serves from its cache. It covers the two costs to measure, the test commands, a manual experiment against `/v1/responses`, and the numbers measured so far.

## The two costs

A coding agent resends its whole conversation on every turn. Roundhouse admits only the new suffix to its log. But the provider still receives the full prompt. The provider's prompt cache decides how much of it is billed and processed again. The cost of processing a prefix again is the re-discovery tax. It has two forms.

| Tax | Where it occurs | What to measure |
|---|---|---|
| A | Within one session. The resent prefix grows each turn. | Expected: the cached share of input tokens climbs toward the whole prefix as the session grows. |
| B | Across sessions. A second user sends the same prefix. | Expected: the second session opens with a higher cached share than the first. |

The cached share of a turn is `cached_tokens / input_tokens`.

## Where the counts come from

| Dialect | Cached tokens | Cache writes |
|---|---|---|
| OpenAI Responses | `input_tokens_details.cached_tokens` on `response.completed` | Not a separate billed event |
| Anthropic Messages | `cache_read_input_tokens` | `cache_creation_input_tokens` |

Roundhouse stores the provider's numbers in the log as reported. If the provider sends none, Roundhouse books its own token count as estimated. An estimated record carries zero cached tokens. A zero like that is not an observed zero.

The `/v1/metrics` document adds `cache_reuse_evidence` to each model row. It compares the cache reuse that the router predicted with the reuse that the provider reported, for the same dispatches. The prediction uses the router's token count. The observation uses the provider's token count. A negative mean error means that the router expected more reuse than the provider reported. Only explicit provider counts give observations, and a reported zero counts. Missing counts and locally derived counts give none.

## How the prefix is marked on Anthropic Messages

Anthropic caches only what an explicit `cache_control` marker names. Without a marker, every turn reads nothing. Roundhouse places the markers like this.

- The first marker goes on the penultimate block. It caches the prefix that the previous turn already sent. The last segment is this turn's new input, and it stays unmarked.
- A second marker goes on the block that the previous request to the same target marked. Anthropic looks back at most 20 block positions from a marker. Without the second marker, a session that appends 20 or more items between two turns puts the old entry out of reach.
- A request carries at most four markers. If the client's tool definitions already use the allowance, Roundhouse sends fewer markers or none.
- A target with a one-hour cache model gets `ttl: "1h"` on every marker.

On the Responses wire, the `prompt_cache_key` steers a request to a cache node that caches by itself. Roundhouse always sends one. See [Configure providers and the catalog](catalog.md#cache-lifetime-on-anthropic-messages) for the lifetime rules.

## Run the offline suite

The offline suite drives the real engine against a loopback server. It checks the shared driver, the request markers, the configured transport, and the budget refusal. It does not show how a provider behaves.

```bash
timeout 300 cargo test -p roundhouse-server --test cache_probe
```

## Run the live probe

The `e2e-frontier` feature adds a live cache probe. It sends two turns to one configured Messages target. The second turn appends 20 items. The report keeps each turn's cache reads, writes, and usage provenance separate.

A zero cache read on turn two does not settle anything by itself. If turn one neither read nor wrote the cache, the run is inconclusive. It is not evidence against the marker placement.

To run the live probe, supply a catalog, a pinned `provider/model`, and a USD spend cap. The catalog must contain real prices and a deterministic cache model. Inject the key through `openv` into the variable that the provider's `auth.env` names. Check the minimum cacheable prefix of the pinned model before the run. The fixture contains at least 8,192 words before its marker. That is not a provider token count.

```bash
openv env ROUNDHOUSE_PROBE_CATALOG=/path/to/catalog.json \
  ROUNDHOUSE_PROBE_MODEL='anthropic/<pinned-model-id>' \
  ROUNDHOUSE_PROBE_LIMIT_USD='<approved-cap>' \
  timeout 300 cargo test -p roundhouse-server --features e2e-frontier \
    --test cache_probe live -- --nocapture
```

The live test has no `#[ignore]`. Enabling the feature includes it in an unfiltered test run. Missing configuration fails before any request. The project budget governs both turns, with 16 output tokens and a 30-second deadline per turn. A zero cache read remains a reported observation for investigation.

## Measure Tax A and Tax B by hand

This experiment needs only a running Roundhouse and a client that can send `POST /v1/responses`. Use a catalog with a real target and a prefix that is longer than the provider's minimum.

### Design

- Use one fixed reference document of about 4 KB. It must be longer than the `min_prefix_tokens` of the catalog entry, which is 1024 in the example. Providers do not cache a prefix below their minimum.
- Send the document as `instructions` on every turn of every session. This is what a stateless OpenAI client does.
- Use a list of about 20 short questions that only the document can answer. Ask for the exact words "not specified" when the document does not say. Closed questions keep the answers short and the outputs comparable.
- Send the questions one by one as a growing conversation, so the resent prefix grows each turn.
- Give each session its own `prompt_cache_key`, for example `alice-run1`. A request with no `thread-id`, no `session-id`, and no `prompt_cache_key` is refused with a 422.
- For Tax B, run a second session with the same document and a different key.

### Request

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

In Open mode, no key is needed. On each later turn, send the whole conversation in `input`, with the previous answers and the new question.

### Read the result

1. Read `usage` on the `response.completed` event of each turn.
2. Compute `input_tokens_details.cached_tokens / input_tokens` for each turn.
3. After the last turn, call `GET /v1/metrics` and read the model rows.

### What to expect

These are indicators. They are not assertions, and no measured frontier run is recorded for this experiment. The exact numbers depend on the provider's cache and on the `inactivity_decay` parameters in the catalog.

| Turns | Expected cached share |
|---|---|
| 1 to 3 | Low. The prefix is not cached yet, or it is warming. |
| 5 and later | Above 50%. |
| 10 and later | 70% to 85%, depending on the provider TTL and the `half_life_ms`. |
| First turn of a second session (Tax B) | Higher than the first turn of the first session. |

A cached share above 70% by turn 10 is the target that the experiment was designed around.

## Claude Code and cross-session reuse

Tax B is small for Claude Code. The start of its request differs between sessions, so the shared prefix is short.

Setup for these numbers: bodies that Claude Code 2.1.251 and 2.1.257 sent to a loopback mock server, with synthetic prompts such as "say hi". The bodies are the fixtures in `crates/roundhouse-server/tests/fixtures/`. A Python script approximated `canonicalize` and `Item::render` at revision `2dd40dd`. It is not kept in the repository. The byte counts are therefore not the counts of the Rust render, but the positions where two requests first differ are not affected. Token counts are bytes divided by 4, with no tokenizer. The shared bytes are given in Roundhouse item order, and then in Anthropic order (tools, system, messages). An independent rerun gave the same numbers.

| Comparison | First difference | Shared bytes (Roundhouse order) | Shared bytes (Anthropic order) |
|---|---|---|---|
| Two 2.1.257 sessions, with a different prompt, directory, day, and MCP tools | Item 0 (a fingerprint), byte 60 | 60 | 45,990 (about 11.5k tokens) |
| One session, turn 1 to turn 2, in a new process | Item 2 (model-name text), byte 4,934 | 5,096 | 4,199 |
| One session, turn 2 to turn 3 | None. The conversation only appends. | 17,223 (all) | 63,667 |
| One session, a tool loop in one process | None. The conversation only appends. | 17,249 (all) | 56,861 |
| 2.1.251 and 2.1.257, the same prompt | Item 0 (the version string), byte 58 | 58 | 396 |

What this shows:

- Inside one process, Claude Code only appends. Tax A reuse works.
- A new process changes item 2. A new session, a new day, or a new directory changes items 0 to 4. So two sessions share little in Roundhouse item order.
- In Anthropic order, the tools come first, and two sessions share about 46 KB. Whether Anthropic reuses those bytes depends on its own cache. This measurement does not test it.
- A client upgrade changes the first tool description, so it starts a new prefix for every session.

The first request of 2.1.257 has these items: two developer items of 87 and 75 bytes, the main system prompt of 9,670 bytes, a date reminder of 314 bytes, the typed prompt of 14 bytes, and a system item of 7,106 bytes. It has 21 tool schemas of 45,991 bytes in compact JSON. The session with MCP tools has 23 schemas of 46,416 bytes.

## Relation to NeMo Relay

NeMo Relay's Adaptive Cache Governor plans provider cache breakpoints from assumed reuse horizons. The source is Relay at commit `c37b551`, file `crates/adaptive/src/acg/economics.rs`. It places up to `max_cache_breakpoints` markers at semantic boundaries. It rewrites requests, and it works offline.

Roundhouse does the complementary job. It predicts whether a target's cache is warm, and it measures the realized hit ratio for each target. Its judge runs inline, and it changes who serves a turn.
