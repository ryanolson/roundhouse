<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

> **Status: evidence base, and weaker than this directory's usual bar — read
> §0 before relying on anything here.** Produced 2026-08-20 against
> roundhouse @ `92c5747`. The ruling that synthesizes this into direction is
> `../synergies/router-com-commercial-overlap.md`. Per `agent-docs/README.md`,
> this snapshot gains dated bracketed notes when the world moves — never
> silent rewrites.

# Router.com: the routing thesis, commercialized

Ramp launched **Router.com** on **2026-08-19** — the same day
`../synergies/ecosystem-round-2.md` ruled that "nobody in this survey owns the
turn." This document is the read of what shipped, and it exists because that
ruling's headline claim needs re-testing against a product that did not exist
when the survey was taken.

---

## 0) Provenance, and why this document is second-hand

**Every external claim below comes from search-engine synthesis over pages
this session could not open.** Outbound egress from this container is closed
to `router.com`, `docs.router.com`, `ramp.com`, `developer.nvidia.com`,
`prnewswire.com`, `thenewstack.io`, and every other host tried — the proxy
answers `403` to `CONNECT` (`recentRelayFailures` in
`curl "$HTTPS_PROXY/__agentproxy/status"` records each rejection). Only the
search tool reached the network.

That is materially below the bar this directory sets. `README.md` here asks
for "a read of an external codebase or a design space, pinned to a revision,
every claim carrying a file:line." Nothing below carries a file:line, nothing
is pinned to a revision, and several claims are paraphrases of a paraphrase.
Confidence is therefore marked per claim:

| Mark | Meaning |
|---|---|
| **[direct]** | A verbatim quote from a named primary author (Ramp Labs, a Ramp employee, the press release) that the search tool reproduced. |
| **[reported]** | Consistent across two or more independent secondary sources. |
| **[single]** | One secondary source, unconfirmed. Treat as a hypothesis. |

§8 lists what must be re-read from primary sources before any of this is
allowed to move a milestone. **No claim here is load-bearing enough to change
code today**; the ruling it feeds changes documents and priorities only.

---

## 1) The one-pager

Router.com is an **OpenAI-compatible LLM gateway** that routes each request to
the cheapest model clearing a quality bar. Base URL `https://api.router.com/v1`;
adoption is advertised as a one-line base-URL swap. **[reported]**

- **Provenance.** Built inside Ramp three years ago as internal tooling to keep
  "100+ AI use cases at Ramp on the right model." **[direct]**
- **Scale.** ~2.75 trillion tokens/month routed today. **[direct]**
- **Internal result.** Cut Ramp's own LLM costs ~30% "while making our features
  smarter and faster." **[direct]** Other framings say ">25% without
  sacrificing performance" and "~30% for the same output while delivering
  99.9%+ reliability." **[reported]**
- **Customer result.** Early customers report **40% average** inference cost
  reduction. **[reported]**
- **Price.** Free through 2026; users pay list price for tokens. Waitlist-gated,
  with credits for early invitees. **[reported]**
- **Models.** OpenAI, Anthropic, xAI, with Gemini "coming soon"; open weights
  (Nemotron, Kimi, DeepSeek, GLM, Qwen) served through providers such as
  **Fireworks AI**. **[reported]** *(One source rendered the third lab as
  "SpaceXAI"; read as xAI, and re-verify.)*
- **Ownership.** Ramp — the corporate spend-management company. This is the
  single most important fact in the document, and §6 is about it.

---

## 2) The routing algorithm, as its authors describe it

This is the most technically specific material found, and it is **[direct]** —
Ramp Labs' own account:

> "Ramp Router learns failure rates with an **EWMA**, models latency with
> **Thompson sampling**, then chooses the lowest-cost route to reliably meet
> each request's **deadline**. We route across **providers, models, and service
> tiers**, scoring each combination on reliability, latency, and cost."

Four things in that sentence that roundhouse's router does not do:

1. **Online learning.** Reliability is *learned* from observed outcomes (EWMA),
   not configured.
2. **Uncertainty is modelled.** Thompson sampling is a bandit — it explores
   under uncertainty rather than committing to a point estimate.
3. **The objective is a deadline, not a weight.** "Lowest-cost route that
   reliably meets this request's deadline" is a constrained optimization.
   Roundhouse scores a weighted sum instead (§7).
4. **Service tier is a routing axis.** Provider × model × *tier* (priority /
   standard / flex-class SKUs). Roundhouse has no tier vocabulary at all.

A second **[reported]** description covers model selection proper:

> "Task difficulty chooses the mode, while the continuous difficulty score sets
> a **capability floor** inside that mode. The router picks the cheapest
> effective-cost model meeting the floor, then permits only affordable
> willingness upgrades inside the same mode. Task difficulty is judged from
> **language-neutral signals: context size, prompt length, and recent tool
> activity**."

And the pruning rule, **[reported]**:

> "The router keeps to the **efficient frontier**: a **model-effort variant** is
> only worth considering if no other available variant is both better and
> cheaper." … "When the table reports reasoning effort, the router treats
> **(model, effort)** as the measured operating point."

Note what the routing unit is: **not a model, but a `(model, effort)` pair**,
pruned to a Pareto frontier over measured quality and measured cost.

Beyond selection, "more than 100 optimizations across model selection,
**caching, compression, timing**, and request handling," plus automatic
fallback on provider failure. **[reported]** One source adds "**semantic
attribution**" to that list. **[single]**

Latency overhead is quoted as **~30ms added**. **[single]**

---

## 3) Ramp SWE-Bench: quality priors that were measured

Router's quality bar is sourced from **Ramp SWE-Bench** (`labs.ramp.com/swebench`),
a private agentic benchmark built from **real production engineering work** at
Ramp, because "public leaderboards couldn't answer the questions they had."
**[reported]** It uses `mini-swe-agent` as the harness. **[reported]**

The table reports **coding-agent resolve rate and measured cost per task**, and
"when measuring effectiveness versus cost, the frontier presents as a tradeoff
rather than a single winner." **[reported]** Router "continuously tests new
models against real Ramp engineering work." **[reported]**

This matters to us more than anything else in the document, and §7 says why:
it is the defensible version of the one number roundhouse admits it made up.

---

## 4) The Switchyard connection — they shipped our neighbor's router

**Ramp is a production adopter of NVIDIA NeMo Switchyard**, which
`../synergies/ecosystem-round-2.md` already tracks as a synergy dependency.

- Ramp Labs: "We tested NVIDIA NeMo Switchyard's **stage router for coding
  agents** in Ramp SWE-Bench. Routed agents showed comparable performance to
  single-model controls while substantially reducing costs and runtime."
  **[direct]**
- Veeral Patel (Ramp): "We implemented NVIDIA Switchyard on our internal
  SWE-Bench and **cut costs by 58%**. **It's live in Ramp Router today** — we're
  actively removing people from the waitlist while we manage load." **[direct]**
- Quantified elsewhere as matching frontier performance while cutting **cost
  58% and runtime 33%** (one NVIDIA-side framing says 59% / 32%). **[reported]**

The stage router's mechanism, **[reported]** from NVIDIA's own description:

> "A coding agent moves through different stages. Early on, it explores the
> codebase and recovers from errors. Later it settles into a more mechanical
> implementation. These stages require different levels of model capability…
> For each turn, the stage router examines **recent tool activity** to decide
> how much model capability the agent needs. **Severe errors, repeated
> unproductive work, or prolonged exploration** push the turn toward the capable
> model. **Steady writes and edits, especially once tests are passing**, favor
> the efficient model."

Switchyard also ships three integration paths — a **launcher that runs Claude
Code through Switchyard**, a standalone proxy server, and a library for
embedding in a Rust application — and "preserves native **OpenAI and Anthropic**
API compatibility." **[reported]**

**The load-bearing observation:** roundhouse already computes this exact signal
family. `crates/roundhouse-core/src/validate/trigger.rs:178-298` implements
`NoProgressRepeat`, `PingPong`, `ToolFailureStreak`, and `CostAnomaly` behind a
`Signal` trait (`trigger.rs:154`). We wired them to the **expensive** consumer —
triggering a paid frontier judge side-call through the validate loop — and to
nothing else. Switchyard wired the same family to the **free** one: the routing
decision itself, no judge call, and Ramp measured that at 58% cost and 33%
runtime. See the ruling, finding **R-2**.

---

## 5) The API surface

Thin, and the weakest-sourced section.

- OpenAI-compatible; base URL `https://api.router.com/v1`; "one-line change."
  **[reported]**
- Docs at `docs.router.com` covering "endpoints, request fields, errors, and
  limits." **[single]**
- BYOK "for select model providers." **[single]**
- Managed service only. Models run "on U.S.-based infrastructure by their
  underlying model providers." **No evidence of routing to customer-owned
  GPUs or on-prem serving was found.** **[reported, negative]** — and negatives
  from search synthesis are the least reliable claim class there is. §8.
- **Anthropic-format (`/v1/messages`) support: unknown.** Switchyard preserves
  it; whether Router exposes it was not established. This is the single
  highest-value unknown for us, because Claude Code speaks it. §8.

---

## 6) The commercial model — the router is not the product

The strategic read, **[reported]** and consistent across sources:

> "Give away the infrastructure, capture the data, sell the controls."

The companion product is **Ramp AI Token Spend Management**
(`ramp.com/ai-cost-monitoring`), launched ~2026-07-16, ahead of Router:

- Costs by **provider, model, team, project, and key**, in one dashboard.
- **Anomaly detection** with alerts routed to stakeholders before usage runs
  past plan.
- **Spend limits** by team / project / key, notifying owner, manager, or team.
- **Weekly briefings** on spend trends and savings opportunities.
- **Invoice reconciliation** against actual measured usage.

Ramp's own market data explains the timing: AI token spend across its customer
base is up **13x since early 2025** by one framing, **20.7x since June 2025** by
another; business token *usage* up 1,001% and *spend* up 497% from Jan 2025 to
Apr 2026. **[reported]**

So Router is a customer-acquisition and data-acquisition wedge for a finance
product Ramp already knows how to sell. The routing layer is priced at **zero**
because it is not what is being sold.

---

## 7) Side by side with roundhouse @ `92c5747`

Read the middle column as "what the evidence says Router does", with §0's
confidence marks still attached.

| Capability | Router.com | roundhouse | Verdict |
|---|---|---|---|
| Client hookup | Base-URL swap, OpenAI-compatible | Base-URL swap, Responses API, conformance-tested against Codex's own parser | **Parity** |
| Conversation state | Stateless proxy; client re-uploads full context each turn | **Owns the turn** — append-only log, prefix admission, deltas only | **Ours, uncontested** |
| Local / own-GPU serving | None found | Dynamo fleet, embedded `SelectionService`, KV-cache-aware `select`/`reserve` | **Ours, uncontested** |
| Cross-plane comparison | Hosted providers only | Local and frontier on one cache-adjusted prefill axis | **Ours, uncontested** |
| Durability | Not applicable to a stateless proxy | Fenced lease, replay, resumption, failover | **Ours, uncontested** |
| Steering mid-turn | No | Validate/interject loop, judge, synthetic tool call | **Ours, uncontested** |
| Quality priors | **Measured** — Ramp SWE-Bench, production-derived, per `(model, effort)` | **Hand-written config**; `pricing.rs:58-59` says so outright | **Theirs, decisively** |
| Routing unit | `(model, effort)` on an efficient frontier | Model only | **Theirs** |
| Adaptation | EWMA reliability + Thompson-sampled latency, online | Static weights (`policy.rs:80-90`, `ttft: 0.25`) | **Theirs** |
| Objective | Cheapest route meeting a **deadline** | Weighted sum of prefill, dollars, TTFT | **Theirs** |
| Stage / difficulty routing | Yes — Switchyard stage router, live, measured at −58% cost | Signals exist but reach the judge, never the router | **Theirs** |
| Provider failover | Automatic, 99.9%+ claimed | **None** — no `fallback`/`failover`/`retry` anywhere in `roundhouse-fleet` | **Theirs** |
| Service tiers | A routing axis | No vocabulary | **Theirs** |
| Model breadth | OpenAI, Anthropic, xAI, +Gemini, open weights via Fireworks | OpenAI + Anthropic clients (M7) | **Theirs** |
| Anthropic `/v1/messages` | Unknown; Switchyard preserves it | Deliberately not built (ruled to agentic-api / Relay) | **Unknown / at risk** |
| Budgets and holds | Limits by team / project / key | Project + per-member budgets, fenced holds, watermarks (`roundhouse-store-redis/src/spend.rs:17-19`) | **Parity, ours is stronger** |
| Spend attribution UX | Anomaly detection, alerts, weekly briefings, invoice reconciliation | Per-process dashboard, all-history totals, **no time windows** (README) | **Theirs** |
| Savings honesty | "40% average", single headline number | Measured / estimated split, coverage ratios, capability gate, unpriced traffic reported as unpriced | **Ours, and it is not close** |
| Price | Free through 2026 | n/a (not a commercial product) | — |

---

## 8) What must be re-verified before this moves anything

In rough order of how much a wrong answer would cost:

1. **Does Router expose an Anthropic Messages surface?** If yes, our ruling to
   not build one hands the largest coding-agent client to a competitor. Read
   `docs.router.com` endpoint list.
2. **Does Router route to customer-owned or self-hosted models?** §5 records a
   *negative* from search synthesis, which is the least trustworthy class of
   claim in this document — and it is the negative our entire differentiation
   rests on. Read the docs and the BYOK page directly.
3. **Ramp SWE-Bench methodology** — task count, provenance, contamination
   controls, how `(model, effort)` cost per task is measured, and whether the
   table or the scores are published in a form we could normalize onto
   `quality_prior`'s `0.0..=1.0` scale.
4. **The 30ms latency claim** — measured where, and against what baseline.
   **[single]**, and it is the number a hot-path comparison against our
   tokenize→select→reserve path would turn on.
5. **The 40% customer figure** — sample size, workload mix, and whether the
   baseline is "no router" or "one frontier model for everything." The 58%
   Switchyard figure has a named baseline (single-model controls on Ramp
   SWE-Bench); the 40% does not.
6. **Whether Router does anything stateful.** "Caching, compression, timing" is
   compatible with pure provider-prompt-cache exploitation *and* with
   server-held context. Which one decides how much of our thesis is
   differentiated. See ruling finding **R-6**.
7. **Switchyard rev now live at Ramp** — our ecosystem ruling pinned `5341f71`
   and set a git-rev-not-tag rule. A production adopter is a strong signal
   about which rev is real.

---

## 9) Sources

All accessed 2026-08-20 via search synthesis; **none opened directly** (§0).

- `https://router.com/` — product page (unreachable; via synthesis)
- `https://docs.router.com/` — docs (unreachable; via synthesis)
- `https://ramp.com/router` — product page (unreachable; via synthesis)
- `https://ramp.com/ai-cost-monitoring` — AI Token Spend Management
- `https://www.prnewswire.com/news-releases/ramp-launches-routercom-to-cut-companies-rising-ai-bills-302855572.html`
- `https://www.prnewswire.com/news-releases/ramp-launches-ai-token-spend-controls-302827389.html`
- `https://labs.ramp.com/swebench` — Ramp SWE-Bench
- `https://x.com/RampLabs/status/2079279488395788759` — EWMA / Thompson sampling **[direct]**
- `https://x.com/RampLabs/status/2087163448513765449` — Switchyard stage router in Ramp SWE-Bench **[direct]**
- `https://x.com/vral/status/2087168532333117849` — "cut costs by 58% … live in Ramp Router today" **[direct]**
- `https://x.com/sytaylor/status/2079490091408363848` — 2.75T tokens/month, 30% **[direct]**
- `https://developer.nvidia.com/blog/route-ai-agent-workloads-across-models-with-nvidia-nemo-switchyard/`
- `https://github.com/NVIDIA-NeMo/Switchyard`
- `https://www.zenml.io/llmops-database/cost-efficient-llm-routing-with-online-learning-and-thompson-sampling`
- `https://linas.substack.com/p/weeklyfintechpulse409` — "isn't the product"
- `https://thenewstack.io/cursor-ramp-meta-model-router/`
- `https://venturebeat.com/orchestration/nvidias-switchyard-router-reshuffles-ai-models-mid-task-cutting-task-costs-to-a-third-in-its-own-tests`
- `https://www.theregister.com/ai-and-ml/2026/08/12/nvidias-latest-solution-for-soaring-enterprise-costs-nemo-switchyard-software-router/5286911`
- `https://siliconangle.com/2026/07/16/ramp-targets-ais-fastest-growing-cost-expanded-token-spend-tracking/`
