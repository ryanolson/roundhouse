<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Router.com: what it takes from us, what it cannot, and the two numbers we now owe

> **Status: ruling.** Produced 2026-08-20 against roundhouse @ `92c5747`, on
> the evidence in `../research/router-com-deep-dive.md`. That evidence is
> **second-hand** — egress to `router.com` and every primary host was blocked,
> so every external claim is search synthesis, marked for confidence in its §0.
> This ruling therefore **changes documents, priorities, and one test we owe
> ourselves. It changes no code today.** Where it and
> `ecosystem-round-2.md` disagree, this one wins; that document gets the dated
> addendum this one's §1 specifies.

---

## 1. The correction, first, because it is ours to make

`ecosystem-round-2.md` — dated **2026-08-19** — opens with:

> "Nobody in this survey owns the turn: the durable log under a fenced lease,
> prefix admission, per-principal policy and budgets, exact per-turn pricing
> across local and frontier, steering, and the arm-instrumented judge. That is
> the product, and after reading four neighboring trees closely, it is still
> only ours."

Router.com launched **the same day**. Of that seven-item list, **Ramp** now
ships two — precisely: not Router.com alone, but Router.com together with its
companion **AI Token Spend Management** (shipped ~2026-07-16), which is the
half Ramp actually sells and which Router is explicitly marketed as feeding.
Across the pair: **per-principal policy and budgets** (limits by team, project,
and key) and **per-request cost attribution across providers** — the frontier
half of our fourth item. The routing layer is free; the controls are the
product; the company already owns the CFO relationship.

The claim survives, **narrowed to five**: the durable log under a fenced lease,
prefix admission, local-and-frontier on one comparison axis, steering, and the
arm-instrumented judge. Budgets and cross-provider pricing are no longer
differentiators. They are table stakes, and a free product ships them.

**The process defect is worth more than the correction.** That survey read four
*trees* — Relay, Switchyard, agentic-api, GAIE. It found no rival because it
looked only where source is published. Router.com had been running inside Ramp
for three years at 2.75 trillion tokens a month and was invisible to a
repository survey. **A dedup survey that enumerates repositories and calls the
result a market position is measuring the wrong set.** Future rounds read
shipped products alongside trees, or they will keep returning this answer.

---

## 2. The findings, ranked by what a wrong answer costs

### R-1 — Their quality prior is measured; ours is asserted, and it carries the whole dashboard

`CLAUDE.md` already says this out loud, and the code agrees in two places:
`ReferenceModel::quality_prior` is "Configuration, not measurement — the
capability gate is only as good as this number"
(`crates/roundhouse-core/src/metrics/pricing.rs:58-59`), and the module doc
repeats it citing `FrontierModelSpec`
(`pricing.rs:37-40`, `crates/roundhouse-fleet/src/frontier.rs:32`). The gate is
the only thing stopping a small local model being priced against a flagship. Every savings figure the
dashboard publishes stands on a number a person typed into
`examples/catalog.example.json`.

Ramp's answer is **Ramp SWE-Bench**: production-derived agentic tasks, scored as
resolve rate against measured cost per task, per `(model, effort)`, continuously
re-run as models ship. That is the defensible version of exactly our weakest
number, and they will publish savings claims backed by it while ours are backed
by configuration.

**But we have a better substrate than they do, and it is already built.** The
validate loop's judge already renders per-turn verdicts
(`crates/roundhouse-core/src/validate/verdict.rs`, `arm.rs`), and the event log
already carries exact per-turn cost for the same turns. A fold of judge verdicts
by served model *is* a resolve-rate analogue over our own production traffic —
the measurement `CLAUDE.md` asks for, from machinery built for another purpose,
on our own workload rather than someone else's leaderboard. `CLAUDE.md` names
OpenRouter's published index as the intended source; that remains the right
**bootstrap** for models we have never served, but it should be the cold-start
value, not the steady state.

**Ruling.** `quality_prior` becomes a **two-source field**: a declared prior
(bootstrap, from a named, dated, normalized published index) and an **observed**
posterior folded from judge verdicts, with the catalog recording which one the
gate used. The dashboard already distinguishes measured from estimated dollars
and refuses to merge them; the capability gate must earn the same honesty. Until
it does, no savings figure sourced from a hand-written prior should be quoted
externally without saying so.

### R-2 — We compute the stage signal and route on none of it

`crates/roundhouse-core/src/validate/trigger.rs:154` defines the `Signal` trait;
`:178-298` implement `NoProgressRepeat`, `PingPong`, `ToolFailureStreak`, and
`CostAnomaly`. That is the same family Switchyard's stage router uses — "severe
errors, repeated unproductive work, or prolonged exploration push the turn
toward the capable model; steady writes and edits, especially once tests are
passing, favor the efficient model."

`RoutingContext` (`crates/roundhouse-core/src/routing/mod.rs:162-191`) carries
`session_id`, `turn_index`, `isl_tokens`, `candidates`, `ledger`, `turn_policy`,
`frontier_history`, `budget`. **No signals.** We wired the whole family to the
*expensive* consumer — a paid frontier judge side-call — and the router, which
could act on them for free on every turn, cannot see them.

`EscalationPolicy`'s own doc comment
(`crates/roundhouse-core/src/routing/policy.rs:211-226`) says the richer variant
needs a quality signal that "is no longer uncollected so much as unadopted." It
is more unadopted than that comment knows: the *cheap* signal is collected too,
and the router still audits on a fixed `audit_every` counter that the comment
concedes "benchmarks below boundary-triggered review on weak executors."

Ramp measured the alternative at **58% cost and 33% runtime** against
single-model controls.

**Ruling.** Highest-value change in this document. `RoutingContext` gains a
signal projection, and `EscalationPolicy` gains a signal-triggered arm beside
`audit_every`. The `ToolSignals` port that `ecosystem-round-2.md` §3 scheduled
into the validate seam feeds **both** consumers, not one. Test-first per
`CLAUDE.md`: a failing test that a session in a confirmed trouble streak routes
to the capable pool while `audit_every` says it is not an audit turn.

### R-3 — The routing unit is `(model, effort)`, and ours is `model`

Router "treats `(model, effort)` as the measured operating point" and prunes to
the **efficient frontier** — a variant survives only if no other variant is both
better and cheaper. On reasoning models, effort swings cost by an order of
magnitude on identical weights, so a router that cannot choose effort is leaving
the largest single lever on the table.

This is a schema decision, not a tweak: `catalog_config.rs` keys
`FrontierModelSpec` by `(provider, model)` and the boundary now **rejects**
duplicates on that key (commit `8962462`, finding F3) precisely so the router and
the dashboard cannot resolve an ambiguity differently. Adding effort makes the
key `(provider, model, effort)`, and that rejection rule has to move with it.

**Ruling.** Adopt `(provider, model, effort)` as the catalog key and the routing
unit, and adopt frontier pruning as an explicit, testable catalog-boundary rule:
a dominated variant is a configuration error worth reporting, not a candidate
worth scoring. Sequenced after R-1, because pruning compares quality, and
pruning on an asserted quality number just launders the assertion.

### R-4 — An open-loop router on a closed-loop-ready log

Ramp Labs, verbatim: "Ramp Router learns failure rates with an **EWMA**, models
latency with **Thompson sampling**, then chooses the lowest-cost route to
reliably meet each request's **deadline**."

`AffinityPolicy` scores a weighted sum with **static** weights
(`policy.rs:77-95`; `ttft: 0.25`). No learning, no uncertainty, no deadline.

The gap is not that they have a fancier algorithm. It is that **we already
record everything the algorithm needs and feed none of it back.** `README.md`
states it as a tested property: "TTFT is derivable from the log: first delta
minus the routing decision that preceded it." The log holds every decision,
every realized TTFT, every failure, and every settled cost — and the next
decision consults none of them. The one closed loop we have is the cache ledger,
and it is closed only over *frontier cache TTL*, not over quality, latency, or
reliability.

**Ruling.** Direction, not a milestone: the router's inputs become a projection
of realized outcomes, the same way conversation items and the routing ledger
already are. Start with the cheapest honest rung — an EWMA of realized TTFT and
failure rate per target, folded from the log, replacing the static point
estimates on `Candidate`. Thompson sampling and deadline-constrained selection
are the rung above and need R-1's quality posterior first.

### R-5 — We made failure safe, not rare

`grep -rn "fallback\|failover\|retry"` over `crates/roundhouse-fleet/src/`
returns **nothing**. Router advertises automatic fallback and 99.9%+.

What we have is genuinely good and genuinely different: `README.md` — "A failed
turn settles… the lease comes back immediately, and the same turn id is
retryable without waiting out a TTL." That is *durability*. A provider 429 or
500 still ends the agent's turn; the agent, not roundhouse, decides what happens
next. For a product whose sentence is "coding agents transparently hook up," a
dead inner loop is a broken promise no audit trail repairs.

**Ruling.** A bounded, ledger-visible failover arm: on a retryable provider
failure, re-dispatch to the next admissible candidate within the same turn,
recording both dispatches so the fold cannot double-count and the audit trail
shows what actually happened. Two invariants it must not break — the budget
grant is opened once per turn, and a failover dispatch is not a second billable
turn. This is the smallest item here with the largest gap to parity.

### R-6 — The number we have never measured, and it is the thesis

Router reports 30–40% savings **without owning the conversation**. Our README
opens by asserting that the re-upload "is the dominant cost of agentic work — in
bytes on the wire, in prefill FLOPs, and in dollars."

Against a provider with prompt caching enabled, that assertion is materially
weaker than it reads. A cached prefix is billed at a fraction of input price and
its prefill FLOPs are already skipped **by the provider**. So of the three costs
the sentence names, caching alone substantially answers two — for the exact
traffic shape agents produce, which is a long stable prefix with a short suffix.

Our own test suite makes the honest version visible: "Client bytes stay flat
while context grows" measures **bytes**, not **dollars against a cached
stateless baseline**. Those are different claims and we only have the first.

This does not overturn the thesis. It relocates it. What survives a cached
baseline is what caching cannot do: knowing the *exact* prefix is what makes
**local** cache-aware routing possible at all — the `select`/`reserve` split, the
one comparison axis, `effective_prefill_tokens`. That is uncontested and Router
cannot reach it. But "we save you the re-upload" and "we can route your turn to
the worker that already holds it" are different products, and only the second is
defensible against what shipped yesterday.

**Ruling.** We owe ourselves this measurement before the next external savings
claim: roundhouse's delta-upload path against a stateless full-resend baseline
**with provider prompt caching enabled**, priced from the same catalog, on the
same conversation. `README.md`'s opening paragraph is provisional until that
test exists. If the dollar gap is small, the honest headline becomes TTFT and
local routing — both real, both ours — and the README says so.

### R-7 — Their monetized surface is our "not yet built" list

Ramp AI Token Spend Management sells anomaly detection, alerts to stakeholders,
weekly briefings, invoice reconciliation, and limits by team, project, and key.

Our primitives are stronger where they overlap — fenced holds, per-member
budgets, and watermarks (`crates/roundhouse-store-redis/src/spend.rs:17-19`) are
a harder guarantee than a spend limit. Our *surface* is weaker in the way that
matters to a person: `README.md` concedes metrics are per-process and the
dashboard "reports totals over all history with no time-window selector, because
the in-memory fold keeps no per-interval buckets."

**Ruling.** No CFO product; that is not what roundhouse is for. But the
time-window gap is now a credibility gap rather than a nicety — an all-history
total cannot answer "did last week's routing change help," which is the only
question a savings dashboard exists to answer. Bucket the fold. The README
already names the fix ("a time window requires buckets in the fold, not a
different query"); this promotes it from *Not yet built* to *scheduled*.

### R-8 — Ramp is a Switchyard reference customer, which cuts our way

Roundhouse is NVIDIA. Switchyard is NVIDIA. Ramp took Switchyard's stage router
to production and published **58% cost / 33% runtime** on its own benchmark, and
it is live in Router.com today.

That is not only competitive pressure. It is the strongest external validation
NVIDIA's routing work has, it raises the value of the contributions
`ecosystem-round-2.md` already scheduled into Switchyard (`enforce_usage_reporting`,
the capability gate, the replay-stable re-arming breaker, the structured-verdict
design), and it gives those contributions a production adopter downstream. It
also confirms the version-identity rule that ruling set: a production adopter is
evidence about which rev is real.

And the shape of the competition is favorable where it counts. Router cannot
serve a customer's own GPUs. Every dollar it moves onto hosted endpoints is a
dollar not served locally — which is precisely the substitution roundhouse
exists to reverse, and Router.com's own growth is the market proof that the
routing decision is worth owning.

**Ruling.** Treat Ramp as an ecosystem datapoint, not only a rival. Ramp
SWE-Bench is the most relevant public yardstick for agentic routing that exists;
if our stage-routing work (R-2) is worth anything, that is where the comparison
should be drawn.

---

## 3. What changes

| # | Change | Where | Sequencing |
|---|---|---|---|
| R-2 | Signals reach `RoutingContext`; `EscalationPolicy` gains a signal-triggered arm | `routing/mod.rs`, `routing/policy.rs`, `validate/trigger.rs` | **First.** Largest measured payoff, code already present, test-first |
| R-5 | Bounded in-turn provider failover, ledger-visible | `roundhouse-fleet`, engine dispatch | **First.** Smallest item, largest parity gap |
| R-6 | Cached-baseline savings measurement; README opening marked provisional | test suite, `README.md` | **Before any external savings claim** |
| R-1 | `quality_prior` splits into declared prior and observed posterior | `metrics/pricing.rs`, `catalog_config.rs`, `validate/verdict.rs` fold | After R-6 (it is the same honesty argument) |
| R-3 | `(provider, model, effort)` catalog key; efficient-frontier pruning at the boundary | `catalog_config.rs`, routing | After R-1 — pruning on an asserted prior launders the assertion |
| R-7 | Per-interval buckets in the metrics fold | `metrics/fold.rs`, `snapshot.rs` | Promoted from *Not yet built* to scheduled |
| R-4 | EWMA of realized TTFT and failure rate folded from the log onto `Candidate` | `routing/`, `metrics/fold.rs` | Direction; first rung after R-1 |
| §1 | Dated addendum narrowing the seven-item claim to five | `ecosystem-round-2.md` | With this ruling |
| §1 | Future surveys read shipped products, not only published trees | `agent-docs/README.md` practice | Standing |

**Not adopted.** A CFO-facing spend product (R-7) — not what roundhouse is for,
and Ramp is better at it. Competing on hosted-model breadth — Router's catalog
is its business and matching it is a treadmill; ours needs to be deep enough to
price the local counterfactual honestly, which is a much smaller number of
models.

**Re-open when the evidence improves.** `../research/router-com-deep-dive.md`
§8 lists seven items needing primary sources. Two would change this ruling: if
Router exposes an **Anthropic Messages surface**, the standing decision not to
build one (`ecosystem-round-2.md`) hands the largest coding-agent client to a
competitor and must be re-argued; and if Router routes to **customer-owned
GPUs**, then R-6's relocation of the thesis is not a relocation but a
contraction, and the differentiation section of this ruling is wrong.

---

## 4. What this leaves the product sentence

Unchanged in words, sharper in emphasis. *Agentic coding agents transparently
hook up to roundhouse and, through it, take advantage of NeMo Relay and
Switchyard — routing to local models served by Dynamo and to frontier-lab models
via their public endpoints, co-optimizing function, cost, and time to solution.*

Router.com is the same sentence with **"local models served by Dynamo" deleted**,
sold by a company that already invoices the CFO. It proves the market for the
rest of the sentence at 2.75 trillion tokens a month. What it cannot do is the
clause it dropped — and after this round, that clause, the durable log it rests
on, and the steering it enables are the whole of the defensible position. The
budgets are not. The cross-provider pricing is not.

The moat was never the wire format, and as of yesterday it is not the ledger
either. It is the turn — owned, exactly priced across two planes one of which
nobody else can reach, and steerable.
