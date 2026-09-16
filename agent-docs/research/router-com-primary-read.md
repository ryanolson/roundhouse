<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

> **Status: evidence base, primary-sourced.** Produced 2026-08-20 against
> roundhouse @ `92c5747`. This document **supersedes the external claims** in
> `router-com-deep-dive.md`, which was written the same day from search
> synthesis because the authoring container had no egress. That document keeps
> its §0 provenance warning and gains a banner pointing here; the two are kept
> side by side deliberately, because the diff between them *is* the lesson its
> ruling drew about method. The ruling both feed is
> `../synergies/router-com-commercial-overlap.md`.
>
> Per `agent-docs/README.md`, this snapshot gains dated bracketed notes when
> the world moves — never silent rewrites.

# Router.com: the primary read

## 0) Method, so this can be re-run

Everything below was read directly. Two sources, both reproducible:

**The docs.** `docs.router.com` publishes an `llms.txt` index, and every page
is served as Markdown at the same path with a `.md` suffix. The whole corpus is
twenty pages:

```bash
curl -s https://docs.router.com/sitemap.xml \
  | grep -o '<loc>[^<]*</loc>' | sed 's/<[^>]*>//g' \
  | while read -r u; do curl -s "$u.md" -o "$(basename "$u").md"; done
```

**The live API.** Route existence and dialect were probed unauthenticated. A
`401` proves the gateway accepted the path; the *shape of the error envelope*
proves which dialect that path is dispatched in. `/v1/zzznotaroute` is the
control — it establishes what the OpenAI-shaped default looks like, so an
Anthropic-shaped envelope is a positive signal and not a catch-all:

```bash
for r in /v1/models /v1/responses /v1/chat/completions /v1/messages \
         /v1/messages/count_tokens /v1/zzznotaroute; do
  printf '%-30s ' "$r"
  curl -s -X POST -H 'Content-Type: application/json' -d '{}' \
    "https://api.router.com/v1${r#/v1}" | head -c 120; echo
done
```

**The CLI.** `agents.ramp.com/install.sh` installs pre-built binaries from the
public GitHub repo `ramp-public/ramp-cli`. Read at tag **`v0.2.24`**, published
`2026-08-19T22:02:34Z` — launch day. The binary is a Nuitka-compiled Python
distribution; the claims below come from `strings` over
`main.dist/ramp-linux-amd64`, which preserves module names, environment-variable
names, and docstrings. **That is weaker than reading source**: it establishes
that a symbol exists, not the logic around it. Claims from the binary are marked
**[binary]** and none of them is load-bearing on its own.

No account was created and no authenticated request was made. Everything
requiring a key — actually serving a Claude Code turn, the dashboard, the
benchmark explorer — remains unverified, and is listed in §8.

---

## 1) What Router is

An **LLM gateway** in front of hosted providers, sold by Ramp, launched
2026-08-19. Base URL `https://api.router.com/v1`. Free routing through 2026 with
$26 of model credits (`ramp.com/router`).

Ramp **does not host model weights**. From the FAQ, verbatim:

> "All models run on U.S.-based infrastructure. Ramp does not host the model
> weights. Models run in the infrastructure of their underlying provider, such
> as OpenAI, Anthropic, Google Vertex AI, Fireworks, or xAI. Router is the
> gateway between your application and those providers."
> — `resources/faq.md`, "Where are the models hosted?"

This is the **primary-source confirmation of the negative our differentiation
rests on**, which the earlier document could only infer. Router routes among
*hosted* providers. There is no path to a customer-owned GPU, no self-hosted
backend, no BYO endpoint. Five providers: OpenAI, Anthropic, Google Vertex AI,
Fireworks, xAI.

---

## 2) The API surface — four routes, and the endpoint reference is stale

`api/endpoint.md` says Router "implements two OpenAI routes" and that
`POST /v1/chat/completions` "is not supported yet, so pointing a Chat
Completions client at this base URL will 404."

**That page is wrong, or at least behind.** Three other primary sources and a
live probe say there are four routes, across two dialects:

| Route | Dialect | Evidence |
|---|---|---|
| `GET /v1/models` | OpenAI | `api/endpoint.md`; probe |
| `POST /v1/responses` | OpenAI Responses | `api/endpoint.md`; probe |
| `POST /v1/messages` | **Anthropic Messages** | FAQ; `connect.md`; `cache-optimizations.md`; probe |
| `POST /v1/messages/count_tokens` | **Anthropic Messages** | `connect.md`; probe |

The FAQ states it flatly:

> "Ramp Router supports two API surfaces:
> - `POST /v1/responses` - OpenAI Responses API format
> - `POST /v1/messages` - Anthropic Messages API format
>
> The OpenAI chat completions endpoint (`POST /v1/chat/completions`) is not
> supported. […] Both surfaces route to the same underlying providers.
> Responses requests always return an OpenAI Response object; Messages requests
> return the Anthropic Messages shape."
> — `resources/faq.md`, "What API format does Router use?"

And `getting-started/connect.md`, inside the migration prompt Router tells you
to paste into your coding agent:

> "Router also serves an Anthropic-compatible surface at `POST /v1/messages`
> and `POST /v1/messages/count_tokens`. A codebase already on Anthropic's
> Messages API can point at that instead of being rewritten."

### The probe, and why it is independent evidence

Documentation can be aspirational. The live gateway is not. Unauthenticated
`POST` to each route, bodies verbatim:

```
/v1/models                {"error":{"message":"Invalid API key.","type":"authentication_error","param":null,"code":"invalid_api_key"}}
/v1/responses             {"error":{"message":"Invalid API key.","type":"authentication_error","param":null,"code":"invalid_api_key"}}
/v1/chat/completions      {"error":{"message":"Invalid API key.","type":"authentication_error","param":null,"code":"invalid_api_key"}}
/v1/zzznotaroute          {"error":{"message":"Invalid API key.","type":"authentication_error","param":null,"code":"invalid_api_key"}}   <- CONTROL
/v1/messages              {"type":"error","error":{"type":"authentication_error","message":"Invalid API key."}}
/v1/messages/count_tokens {"type":"error","error":{"type":"authentication_error","message":"Invalid API key."}}
```

Every path returns `401`, including the nonsense one — so `401` alone proves
nothing. What proves something is that **only the two Messages paths answer in
Anthropic's error envelope**, on both `GET` and `POST`, while the control and
every OpenAI path answer in OpenAI's. The auth layer knows, before
authenticating, that those two paths speak a different dialect. A route that
did not exist would fall through to the default, as `/v1/zzznotaroute` does.

**Conclusion.** `/v1/messages` is a real, dialect-aware Anthropic Messages
surface. What is *not* established without a key is whether it serves a
complete Claude Code turn — `tool_use` blocks, streaming events,
`cache_control`. See §8.

---

## 3) How routing actually works

Four mechanisms, and only two of them are "routing" in the sense we mean.

**1. Pinned model.** `model` is one exact ID from `GET /v1/models`. No routing.

**2. Client-supplied fallback list.** `models` is an ordered array of 1–15
`provider:provider-model[:service-tier]` candidates, tried in order. The
*application* picks the candidates and the order; Router walks the list. This is
failover, not selection.

**3. Benchmark aliases** — the first real routing. An alias appears in
`GET /v1/models` with `"owned_by": "router"` and a `router_alias` object, and is
used as an ordinary `model` value:

> "Each benchmark alias represents multiple models and their configured weights.
> Router samples from that mix for each request, so traffic follows the current
> benchmark results without requiring application changes when the mix is
> updated."
> — `strategies/benchmark-routing.md`

Note what this is: **weighted sampling from a mix**, with the weights derived
from benchmark results. It is not a per-request decision about *this* request.
The alias exposes its candidate list and a floor context window ("the
alias-level `limits` are the floor across every candidate") so clients can
budget prompts against whichever candidate wins.

**4. Switchyard** — the second real routing, and the one that matters to us.
The FAQ describes it in full, verbatim:

> "Switchyard is an opt-in routing strategy for coding-agent traffic (Claude
> Code, Codex, and similar harnesses). For eligible requests, it extracts
> signals from the conversation - error severity in recent tool outputs,
> tool-call categories, test results, and turn depth - and uses those to decide
> whether to route to a capable frontier model or a cost-efficient model.
> Simpler tasks route down; complex or error-heavy turns route up."
> — `resources/faq.md`, "How does Switchyard routing work?"

Pass-through cases are enumerated: Switchyard not enabled, image input present,
the requested model is already cost-efficient, or no eligible cheap model is
available.

**This is the single most important paragraph in the corpus.** The signal family
it names maps almost one-to-one onto signals roundhouse already computes in
`crates/roundhouse-core/src/validate/trigger.rs` and routes on none of —
`ToolFailureStreak` against "error severity in recent tool outputs",
`NoProgressRepeat`/`PingPong` against "turn depth", and tool-call categories
against a classification we do not yet make. Router consumes them at the router;
we consume them only at a paid judge.

**5. Flex tier** — worth separating out, because it is *not* model routing:

> "Router keeps the same provider and model and only chooses between that
> model's standard and Flex service tiers. It sends a request to Flex when doing
> so is not expected to degrade it, based on how each tier has been performing
> recently."
> — `strategies/cost-efficient-routing.md`

Same model, cheaper tier, worse and less predictable latency. On by default.
This is a different mechanism from cross-model routing and it is folded into the
same headline savings number (§7).

---

## 4) Failover, in the detail we would otherwise get wrong

`guides/fallbacks.md` and `guides/bring-your-own-key.md` together specify a
failover model more careful than "retry on 5xx". The parts worth copying:

- **Advance conditions are enumerated**: rate limit, provider `5xx`, network
  failure, timeout, or a stream that fails before it starts. Invalid requests
  and unauthorized models fail *immediately* without walking the list, "since
  retrying them on another provider would fail the same way." No candidate is
  retried twice.
- **The streaming commit boundary.** "Router can switch models right up until
  the client receives `200 text/event-stream`. After that the response is
  committed and a mid-stream provider failure ends the stream." Two separate
  timeouts bound the two halves: `timeout_before_headers` caps the window in
  which failover is still possible, `provider_timeout` caps idle time after
  commit. This is the subtle part — the point at which a failover stops being
  transparent and starts being a truncated stream, named explicitly and given
  its own knob.
- **Key fallback is a distinct axis from model fallback.** "Key fallback retries
  a model with another credential. Model fallback advances to another
  candidate." Key fallback fires on `401`, `403`, `429`, any `5xx`, network
  error, timeout, or key-decryption failure; other `4xx` do not trigger it. For
  streaming, key fallback is only possible "before the response produces
  billable output."
- **Both are observable in the log**: `credential_source` (`byok` | `shared`)
  and `key_fallback_used`.
- **Compound errors are preserved, not flattened.** When every candidate fails,
  Router keeps the *last* failure's status, type, code and `param` and appends
  `(all N candidates failed: provider:model STATUS code; ...)`. Only if it
  cannot name a specific last failure does it return `502 all_candidates_failed`.
- **Cost of the feature is stated**: `provider_timeout` applies per candidate,
  so a buffered request can take timeout × candidates. "Set your client deadline
  accordingly."

Roundhouse has none of this. `grep -rn 'fallback\|failover' crates/roundhouse-fleet/src`
returns nothing.

---

## 5) Credentials — and the sharpest structural difference

Router's model is **custodial**. From `getting-started/overview.md`:

> "Your application holds a single Router key; Router holds the provider
> credentials and never asks you to send them."

BYOK exists but is still custodial — you hand Router your provider key and it
stores it:

- Keys are "write-only and stored encrypted", per user and provider, exactly one
  key per provider, no organization tier (`guides/bring-your-own-key.md`).
- **Anthropic and Google Vertex AI are not supported for BYOK.** "BYOK is not
  currently available for Anthropic or Google Vertex AI models; those use
  Router's shared credentials" (FAQ). Supported: OpenAI, Fireworks, xAI.
- Router "does not call the provider to validate a key when you save it. An
  invalid key is not automatically disabled and continues to be tried."
- When your key serves a request, the provider bills you and Router does not
  charge; on shared-key fallback Router bills normally.

This is the exact inverse of what M7 landed in `92c5747`: roundhouse *forwards*
the agent's own credential and stamps a payer, holding nothing. Against Router,
"we never hold your provider key, and your Anthropic subscription keeps working"
is a true statement that Router cannot currently make — and note the second half
is not a small point, because Anthropic is precisely where BYOK is unavailable.

---

## 6) The cache story, and why it sharpens rather than weakens ours

`strategies/cache-optimizations.md` is the answer to the earlier document's
open question about whether Router holds any context. **It does not.** The
provider owns the cache; Router preserves controls and accounts for the reported
cache tokens. `prompt_cache_key` and `prompt_cache_retention` pass through;
Anthropic `cache_control` values are passed unchanged when a Messages request
resolves to an Anthropic model, and become "advisory" when it resolves elsewhere.

But the page also contains the most interesting sentence in the corpus:

> "Provider caches are scoped to a provider and model. If a routed model or
> optimization selects a different provider or model on a later request, it
> cannot reuse the first provider's warm cache. Eligible multi-candidate OpenAI
> routes carrying `prompt_cache_key` use a **five-minute routing-affinity lease**
> by default, extended to 24 hours for `prompt_cache_retention: "24h"`, while
> still failing over when the pinned candidate degrades."

Read that as an admission of a design tension. **Router's freedom to route and
its ability to keep a prefix warm are in direct conflict**, because the cache
lives in a provider it does not own. Its resolution is to *stop routing* — pin
the request to a candidate for five minutes and give up the routing decision to
keep the cache.

And a forward-looking note in the same page: "Self-service Router response
caching, which would reuse an entire previous response without calling a model
provider, is a separate optimization and is not currently configurable" — i.e.
it exists or is planned, just not exposed.

This reframes our own claim. `README.md:10-12` says the re-upload is "the
dominant cost of agentic work — in bytes on the wire, in prefill FLOPs, and in
dollars," and the earlier document was right that provider prompt caching
already blunts the dollars for cached-hit traffic. The defensible claim is not
"we save you the re-upload." It is: **roundhouse does not have to choose between
routing freely and keeping the prefix resident**, because it owns the workers
the prefix lives in. Router has to choose, and its documentation says which way
it chooses.

The measurement we still owe is unchanged and still ours to run: our suite
proves client bytes stay flat (`README.md:279-280`), which is a bytes claim, not
a dollars claim against a cached stateless baseline.

---

## 7) The numbers, and what each one actually means

| Number | Source | What it is |
|---|---|---|
| **40%** average cost cut | `ramp.com/router` headline | Marketing aggregate. Bundles Flex-tier arbitrage (same model, cheaper tier) with cross-model routing. No baseline named. |
| **~30%** internal | launch PR | Ramp's own spend, three years of internal use. |
| **58% cost / 33% runtime** | Switchyard/Ramp SWE-Bench | The only figure with a named baseline (single-model controls). |
| **92%** | Delphi customer quote on `ramp.com/router` | Single customer, no baseline, no workload description. |
| **~2.75T tokens/month** | launch PR | Scale claim. |

The product page carries a chart legend reading "40% Default / 60% Flex". That
reads as **traffic share**, not savings attribution — it is not evidence that
the 40% headline is mostly Flex arbitrage, and should not be quoted as such.
What is fair to say: the headline number does not separate same-model tier
arbitrage from cross-model routing, and those are mechanically different things.
That is the same critique `CLAUDE.md` levels at our own savings dashboard, which
is what makes it a fair one to make.

**The model catalog is directly useful to us.** `supported-models.md` publishes
per-model input/output rates in USD per million tokens, and it covers exactly
the open-weights models roundhouse serves locally through Dynamo —
`nemotron-3-ultra-nvfp4` ($0.60/$2.40), `gpt-oss-120b` ($0.15/$0.60),
`gpt-oss-20b` ($0.07/$0.30), `deepseek-v4-flash` ($0.14/$0.28), `qwen3p7-plus`
($0.40/$1.60), `kimi-k2p7-code` ($0.95/$4.00), `glm-5p2` ($1.40/$4.40),
`minimax-m3` ($0.30/$1.20). That is a curated, single-price-per-model source for
the hosted-counterpart correlary table `CLAUDE.md` describes — with the caveat
that these are Router's negotiated rates, not list, and the same page warns the
display labels "are not necessarily valid `model` values."

Reasoning effort is enumerated per model (`none`, `minimal`, `low`, `medium`,
`high`, `xhigh`, `max`), which confirms the routing unit is `(model, effort)`.

---

## 8) The hookup surface — the part that is aimed at us

`ramp.com/router` advertises a one-line install:

```
curl -fsSL https://agents.ramp.com/install.sh | sh && ~/.local/bin/ramp router configure
```

The installer (read directly) installs a binary from `ramp-public/ramp-cli` and
drops skills into `~/.claude/skills` and `~/.codex/skills`. From
`install.sh` lines 315–341, verbatim:

```sh
install_agent_skills "Claude" "$HOME/.claude/skills"
install_agent_skills "Codex"  "$HOME/.codex/skills"
...
info "Configuring coding agents for Ramp Router..."
```

**[binary]** `strings` over the `v0.2.24` Linux binary shows dedicated modules
`ramp_cli.commands.claude_code`, `ramp_cli.commands.codex`,
`ramp_cli.commands.router`, `ramp_cli.commands.router_sync`,
`ramp_cli.commands.skills`, plus OpenCode handling (`OPENCODE_CONFIG`,
`OPENCODE_TUI_CONFIG`, `https://opencode.ai/tui.json`). The Claude Code
configuration is an environment-variable swap — the symbols present are
`ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_CUSTOM_HEADERS`,
`ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU,FABLE}_MODEL`, and
`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`, alongside `_configure_claude_code`
/ `_unconfigure_claude_code` and ownership bookkeeping (`_OWNED_ENV_KEYS`,
`_CONFIGURE_WRITTEN_ENV_KEYS`, `_ENV_KEYS_OWNED_LATER`) so the configuration can
be cleanly reverted. Codex gets `[model_providers.ramp-router]` with an `auth`
sub-table written into `$CODEX_HOME/config.toml`. There are also
`SUBAGENT_TIER_ENV_KEYS` / `SUBAGENT_TIER_NAME_ENV_KEYS`, so subagent tiers are
configured too, and `_install_claude_code_statusline` downloading from
`https://app.router.com/claude-code-statusline` — which is the in-agent cost
display the product page mocks up ("Switchyard enabled $45.62 / Frontier
(Generic) $297.85").

**So Router's Claude Code hookup is an environment-variable swap, plus a
statusline** — the same transparency mechanism roundhouse claims, shipped on
launch day. That is the sharpest competitive fact in this document, and it is
worth stating exactly as strongly as the evidence allows and no more:

- **Established:** the CLI writes `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN`
  (symbols present, with explicit configure/unconfigure and owned-key
  bookkeeping), and `/v1/messages` exists and dispatches in Anthropic dialect
  (§2).
- **Inference, not verification:** that the base URL those symbols write
  *resolves to that route*. Nothing in the strings dump connects the two. It is
  the obvious reading — there is no other Anthropic-dialect surface for a
  Claude Code base URL to point at — but it was not observed.

The agent list — **Claude Code, Codex, OpenCode** — is assembled from three
partial sources (the FAQ's "Claude Code, Codex, and similar harnesses",
`install.sh`'s two skill targets, and the binary's module names). The docs page
`getting-started/coding-agents.md` renders a `CodingAgentConfigurator`
component that was not readable without executing the SPA, so **treat this list
as a lower bound, not an enumeration.**

**[binary]** One further observation worth recording because of where it points:
the CLI carries `ramp_cli.auth.agent_wallet`, `ramp_cli.client.agent_wallet`,
`ramp_cli.commands.agent_wallet` and `agent_wallet_validation`. Ramp appears to
be wiring agents to actual spend authority on its card rails. That is a
direction no open-source neighbor is positioned to follow, and it is the
strategic reason the routing layer is free.

---

## 9) Other capabilities worth knowing

- **Shadow models** (`strategies/shadow-models.md`). Router mirrors a
  configurable percentage of `POST /v1/responses` traffic to one to three
  additional models in the background. Shadow responses are never returned, are
  not billed, and cannot delay or fail the primary request. Compared on **cost**
  and **latency** today, with **agreement via an LLM judge** listed as coming
  soon. Two restrictions matter: it "requires content recording to be enabled"
  and "is not available on keys with BYOK credentials attached."
- **Content recording is on by default.** "Router records model inputs,
  outputs, and tool calls for debugging, semantic analysis, and product
  improvement. Recorded content is retained for one year by default." Opt-out is
  account-wide and asynchronous, and "does not delete existing archives" (FAQ).
- **Usage-policy screening can deactivate a key**, and the FAQ acknowledges the
  agent-specific hazard: "Screening runs asynchronously, and coding agents retry
  automatically. A single prompt that is retried several times can register as
  several separate violations before you see the first one." Repeated violations
  lock the account.
- **Spend caps** are per key, lifetime or recurring, and enforced *after* usage
  is recorded — "so concurrent requests can take a key slightly over its cap."
  A capped key fails `401`, not `402`.
- **Attribution** is via the standard Responses `metadata` field (feature, team,
  environment), stored with the usage record.
- **Correlation** via `x-request-id` / `x-trace-id`, echoed and generated when
  absent.
- **Limits**: 100 MiB request body, 1–15 fallback candidates, 100 stored keys
  per user, 90-day dashboard range.
- **No file upload API**, no audio on Fireworks, no web search off OpenAI
  (`guides/choose-a-model.md` capability table).

---

## 10) The seven questions the second-hand read left open

| # | Question | Answer |
|---|---|---|
| 1 | Anthropic Messages surface? | **Yes.** `/v1/messages` and `/v1/messages/count_tokens`, confirmed by FAQ, `connect.md`, `cache-optimizations.md`, and a dialect probe. `api/endpoint.md` is stale. §2 |
| 2 | Routes to customer-owned models? | **No.** "Ramp does not host the model weights"; five hosted providers only. Primary-sourced, not inferred. §1 |
| 3 | Ramp SWE-Bench methodology | **Still open.** The page is a client-rendered explorer; task count, provenance, and contamination controls were not readable without executing it. §11 |
| 4 | The 30ms latency claim | **Not found in any primary source.** Treat as unsupported until it is. |
| 5 | The 40% figure's baseline | **No baseline is published.** It bundles Flex tier arbitrage with cross-model routing. §7 |
| 6 | Does Router hold state? | **No context state.** Provider owns the cache. It does hold *routing* state: a 5-minute affinity lease (24h opt-in). §6 |
| 7 | Switchyard rev live at Ramp | **Still open.** No revision is published; the FAQ describes behavior, not version. |

## 11) What is still unverified, and what it would take

1. **Whether `/v1/messages` serves a complete Claude Code turn** — `tool_use`
   blocks, streaming, `cache_control` round-trip. The route and its dialect are
   proven; end-to-end behavior is not. Requires a free key.
2. **Ramp SWE-Bench methodology** (task count, provenance, contamination
   controls, how `(model, effort)` cost per task is measured, whether scores can
   be normalized onto `quality_prior`'s `0.0..=1.0` scale). The explorer is
   client-rendered; reading it needs a browser or its backing API.
3. **Whether benchmark-alias weights or Switchyard thresholds are inspectable**
   by a customer, which decides whether their quality prior is auditable or just
   measured-then-hidden.
4. **The exact `ramp router configure` write set** — the [binary] claims in §8
   name symbols, not logic. Running it in a throwaway HOME would settle it.
5. **Switchyard revision** deployed at Ramp.

None of these blocks the ruling. Items 1 and 2 are the ones whose answers would
change a milestone.

---

## 12) Sources

All read directly 2026-08-20 unless marked.

**Docs** (Markdown, `.md` suffix on each path under `https://docs.router.com/`):
`llms.txt`, `sitemap.xml`, `supported-models`, `getting-started/overview`,
`getting-started/quickstart`, `getting-started/connect`,
`getting-started/coding-agents`, `guides/choose-a-model`, `guides/fallbacks`,
`guides/control-spend`, `guides/bring-your-own-key`, `guides/monitor`,
`strategies/cost-efficient-routing`, `strategies/benchmark-routing`,
`strategies/shadow-models`, `strategies/cache-optimizations`, `api/endpoint`,
`api/request-fields`, `api/errors-and-limits`, `resources/faq`,
`resources/support-and-feedback`.

**Live API**: `https://api.router.com/v1` route probes (§0).

**CLI**: `https://agents.ramp.com/install.sh`; `ramp-public/ramp-cli` release
`v0.2.24` (`ramp-linux-amd64.tar.gz`), published 2026-08-19T22:02:34Z.

**Product**: `https://ramp.com/router` (rendered copy, customer quotes,
headline numbers).

**Not read directly** (client-rendered, needs a browser):
`https://labs.ramp.com/swebench`, `https://app.router.com/*`.

**Secondary**, carried over from `router-com-deep-dive.md` §9 and unchanged in
status: the launch press release, Ramp Labs posts on EWMA/Thompson sampling and
the 58% figure, NVIDIA's Switchyard blog, and the trade coverage.
