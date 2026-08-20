<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Ecosystem synergies, round 2: the ruling

> **Status: direction.** The synthesis of three evidence documents produced
> 2026-08-19 — `../research/relay-switchyard-dedup-deep-dive.md` (Relay @
> `ca08901`, Switchyard @ `5341f71`), `../research/vllm-agentic-api-deep-dive.md`
> (@ `d59d4b4`), and `../research/k8s-gateway-inference-deep-dive.md`
> (@ `84436a9`) — under the product owner's directive for this round: **use
> Relay and Switchyard heavily wherever they make sense, and dedup this
> project's efforts with theirs**; evaluate vllm-project/agentic-api and the
> Kubernetes Gateway API inference work as new neighbors. This ruling extends
> `nemo-relay.md`; where the two disagree, this one wins. Every claim relied on
> here carries a file:line in one of the three evidence documents, each
> independently fact-checked (26 of 27 spot-checks confirmed; the one refuted
> claim was a citation undercount that did not change its substance).

## The dedup verdict, first, because it is the headline

**Roundhouse duplicates almost nothing that exists anywhere else — and the
round proved it three separate ways.** Relay owns the harness and has no
conversation state, no tenancy, no budgets, no spend. Switchyard owns the
route and the proxy and has none of those either, and no MCP surface at all.
agentic-api owns server-held state for a *single* stateless vLLM upstream and
has explicitly disclaimed routing, cost, and observability — its ROADMAP
assigns routing to the deployment edge and its issue tracker assigns
cache-aware scheduling to llm-d. The Gateway API inference extension deleted
its entire scheduler to llm-d two months ago and is now an API, a wire
protocol, and a conformance suite. Nobody in this survey owns the turn: the
durable log under a fenced lease, prefix admission, per-principal policy and
budgets, exact per-turn pricing across local and frontier, steering, and the
arm-instrumented judge. That is the product, and after reading four
neighboring trees closely, it is still only ours.

Where we *did* overlap, the overlaps are already resolved or independently
convergent: the judge prompts were ported with attribution in M6; the
advisor-gate mechanisms (two-counter budget, anchored parsing, discarded-work
accounting) were ported as ideas; and Switchyard turns out to have
independently implemented our `enforce_usage_reporting` rule — same
`or_insert`, same never-override — which is not duplication but two projects
converging on the same correctness fact. The one genuine three-way duplication
is the **launch surface**: Relay ships 6,301 lines of Rust launchers,
Switchyard ships 2,503 lines of Python launchers, and our M9 plans a third.
That is the dedup target with real mass, and it is ruled on below.

## What "use heavily" resolves to

The directive could mean adopting crates or adopting designs. The evidence
splits it cleanly: the assets with the most novel content (`AdvisorGate`,
`EscalationClassifier`) require owning dispatch and default to fail-open —
both direct collisions with invariants our own M6 review just spent ten
findings enforcing — while the assets that fit our seams are light, stable,
and land on real gaps. So "heavily" lands as **two crate adoptions, one port,
two design references, one zero-code integration, and a widened contribution
flow** — more than the standing ruling's formats-only posture, short of
runtime coupling:

1. **Adopt `switchyard-protocol` as the correlation front door** (crate,
   rev-pinned — see the version-identity rule below). Its
   `Metadata::from_headers` normalizes fifteen fields from every coding-agent
   correlation header we care about — Codex turn metadata, Claude Code
   session/agent lineage, Relay's session id, and the `x-dynamo-*` aliases —
   including sub-agent lineage roundhouse has no vocabulary for. Six
   dependencies, all already in our tree. This is the cheapest adoption in
   either tree and the most concrete "use heavily" there is. Lands with M7,
   where the correlation headers first matter on a real wire.
2. **Adopt `nemo-relay-types` and emit** `LlmOptimizationSummary` + ATOF
   (the S2 plan, unchanged in content, now scheduled): pin the *published*
   0.7.3 — the types we emit exist there, and the tree's unpublished 0.8.0
   carries a `feat!` we do not need; move when they publish. ATIF is not in
   the types crate (the deep dive's row was right; the fact-check confirmed
   it), so the ~15 trajectory structs are re-implemented from the spec when
   `GET /v1/sessions/{id}/trajectory` lands.
3. **Port `ToolSignals`** (~1,100 lines, Apache-2.0, attributed like the
   prompts): a public, pure, no-model-call trouble detector with fields our
   M6 trigger vocabulary lacks — windowed error severity, edit/write/read
   counts, `compacted`, `pure_bash_streak`. It slots into the `Signal` trait
   seam M6 left open for exactly this, alongside the ACG-stability candidate.
4. **Design references, not dependencies, for M7's pass-through**:
   Switchyard's `forward_auth` is a working production answer to four
   questions M7 must answer — a redirect-disabled client so a credential
   cannot follow a redirect to another origin, forwarding mutually exclusive
   with a stored key, a per-provider forwarded-header allowlist, and
   `redact_forwarded_auth` scrubbing echoed credentials out of upstream error
   bodies. Read and cite; do not depend.
5. **The zero-code integration goes first.** agentic-api is an MCP *client*;
   roundhouse ships an MCP *server*. A Codex session running against
   agentic-api can declare roundhouse's `/mcp` as a tool with a URL and a
   bearer key — `status`, `declare_intent`, `prefer`, `set_quality_floor`,
   `explain_last_route` become available with a config file and no PR on
   either side. This is the fastest demonstrable proof of the directive and
   it precedes every dependency conversation.
6. **Adopt the SLO header vocabulary** from the inference-gateway ecosystem:
   `x-slo-ttft-ms`, `x-slo-tpot-ms`, `x-gateway-inference-fairness-id`. Our
   own `routing/policy.rs` names the demand-side gap these fill — "a
   client-supplied per-turn quality floor" is its stated cheapest honest next
   step — and adopting an established spelling costs one header parser and
   one `RoutingContext` field while buying wire-compat with every conformant
   gateway. Kubernetes-independent; lands with M7's server-edge work.

## The `requires_openai_auth` contradiction dissolves into a hypothesis

The standing caveat recorded a flat conflict: our read of codex `6344a65`
said leave it unset; Relay hardcodes `true`. Switchyard's launcher supplies
the missing frame: it sets the flag **conditionally** — `true` when the route
forwards the caller's own OpenAI login, `false` plus
`env_key="OPENAI_API_KEY"` otherwise — selected per-route from
`caller_auth_kind`. If that reading is right, it is a *route property, not a
client-version fact*, and both prior readings were correct for their own
configurations. M7's first verification item stays first, but it now tests a
hypothesis instead of adjudicating a contradiction: reproduce both
configurations against current codex in device-login mode and confirm the
conditional rule. PLAN §3's caveat is annotated accordingly.

## Rulings on the open questions the evidence could not decide

- **Continuation contract (agentic-api Q1): roundhouse's does not change.**
  Delta-upload (`previous_response_id`) and full-resend-with-prefix-admission
  cannot both be authoritative on one path, and prefix admission *is* the
  product — it is what transparency and exact pricing hang from. Roundhouse
  will not grow a second continuation key. The conflict never engages in the
  one supported topology: roundhouse in front, where our full-resend requests
  always take agentic-api's raw-proxy branch and its storage never activates.
- **agentic-api's place: a third target class, later, not a dependency now.**
  "vLLM via agentic-api" becomes a supported *local* target shape beside
  Dynamo when a deployment wants server-side tools or a Messages surface —
  one agentic-api per vLLM deployment, never one fronting the Dynamo fleet
  (its readiness and model listing assume a single upstream). No crate
  dependency today: `default-tls` pulls OpenSSL into any consumer (Dynamo's
  deny.toml and our manifest ban it), their lock carries two reqwest majors,
  and their rmcp 1.8 against our 3.1 would put two MCP implementations in one
  process. The rustls feature is the obvious first upstream PR and the test
  of whether the relationship works.
- **The Messages surface: agentic-api is the incumbent.** Roundhouse does not
  build a native Anthropic Messages tool loop. If Claude Code traffic
  materializes, the topology is roundhouse fronting agentic-api's Messages
  surface, or Relay's `ANTHROPIC_BASE_URL` path — not a fourth
  implementation.
- **Kubernetes: a watching brief, not a milestone.** Roundhouse has zero k8s
  footprint, the ladder through M9 adds none, and the product's stated
  topology has no k8s dependency. The extension's ext-proc model — whole-body
  buffering at the gateway before routing — is architecturally hostile to
  100k-token agentic turns, and its own scheduler has moved to llm-d. What
  survives k8s-independently is adopted above (the SLO headers); what waits
  is `InferencePool`-as-discovery (the one clean CRD use: a label selector
  plus active-ports replacing our hand-fed worker catalog) and a topology
  ruling that, if a Gateway API gateway ever fronts roundhouse, roundhouse is
  a plain `Service`, never an `InferencePool` backend — with the same four
  chain guards the Relay topology carries.
- **The dedup question for schedulers was mis-addressed, and is answered.**
  The scheduler GAIE once shipped lives in llm-d-router now; the sharper
  neighbor was always Dynamo's own KV router — which roundhouse *embeds
  rather than reimplements*. "Do not build a second scheduler" is a directive
  roundhouse already complies with by construction. The select/reserve
  closed-loop contribution (their scheduler proposal documents the
  assumed-load hole ours closes) targets llm-d-router, a different org; no
  second contribution track opens before M7 proves the first (S4's
  sequencing stands).
- **The launch-surface dedup (three implementations, one product):** the
  Direct topology remains the reference and M9 still generates its own
  minimal config — the one test M9 exists for (a real codex binary executing
  our synthetic tool call) cannot be delegated. Relay's CLI remains the
  supported instrumented front end (chained topology, chain guards, now with
  the fourth hazard the re-read added: its decode/re-encode against our
  prefix admission). Switchyard's Python launcher is evidence and reference —
  its `caller_auth_kind` conditional is exactly what M7 tests — but is not a
  blessed front end: no hooks, and a third stack to guard.
- **What is not adopted, re-affirmed with new evidence:** `AdvisorGate` and
  `EscalationClassifier` as code (own-dispatch requirement; `fail_open:
  true` by default; a failed consult audited as APPROVE; the REDO plan fed to
  the executor verbatim — each the exact opposite of an invariant our M6
  review enforced); `switchyard-server`/`switchyard-llm-client` (reqwest 0.13
  against our 0.12, and the boundary conveniently coincides with where their
  process-local session state begins); Relay core, pricing, and
  PII-redaction as crates (all hard-depend on the heavy core; the pricing
  *schema* copy stands).

## The version-identity rule (new, binding)

Switchyard's `v0.2.0` names three different APIs — the crates.io release, the
git tag every doc pins, and main — with four of eleven public names surviving
between them, and `AdvisorGate` exists in no published release.
`nemo-relay-types` on crates.io is one minor and one `feat!` behind its tree.
So: **any adoption from a pre-1.0 neighbor pins a git rev, never a version or
a tag** — the same posture as our Dynamo pin, with the same
unlock-condition-in-the-manifest discipline the redis upgrade set. A
`Cargo.toml` line naming a version of a fast-moving 0.x crate is not a
reproducible statement of what we depend on.

## Contribution flow, widened

The S4 list survives with better arguments and two additions: Relay gets
`enforce_usage_reporting` (now citing Switchyard's independent implementation
— a second NVIDIA team arriving at the same rule is a stronger argument than
our tree alone), the capability gate for `baseline_model`, and realized cache
evidence. Switchyard gets our replay-stable re-arming review breaker and the
structured-verdict design (their anchored prose scan feeds the judge's plan
to the executor verbatim — the injection path our M6 review closed). And a
third track opens toward agentic-api: the rustls feature first, then
observability (they have none and
name it as a gap), stream resumption over their existing sequence numbers,
and the store trait their own ADR already promises. Until T-2 lands upstream,
anything we emit into `LlmOptimizationSummary` carries the capability-gate
result in `limitations[]`, so our number never sits indistinguishable beside
ungated ones.

## What this changes in the plan

- **M7 grows four inputs and loses none**: the `requires_openai_auth`
  hypothesis test (first item, reframed), the `forward_auth` design
  reference, the SLO header vocabulary, and `switchyard-protocol` as the
  correlation front door.
- **M6 follow-on**: the `ToolSignals` port into the open `Signal` seam,
  beside the ACG-stability candidate.
- **S2 emission**: unchanged in content; pinned to `nemo-relay-types` 0.7.3
  published, ATIF structs re-implemented from spec.
- **New first proof**: the agentic-api MCP configuration demo — before any
  dependency work, because it demonstrates the product sentence end to end
  with zero code.
- **M9**: unchanged; the launch-surface ruling above is its frame.
- **Watching briefs**: GAIE/llm-d (re-read at M9 or at first k8s demand,
  whichever first), agentic-api's Interactions API (their next surface;
  re-read when it ships), and the third move GAIE has announced for its
  remaining pieces.

## What this buys

The product sentence — transparent hookup, Relay and Switchyard leveraged,
Dynamo local, frontier public, function-cost-time co-optimized — came through
a four-tree survey intact and sharper: every neighbor's centre of gravity is
confirmed disjoint from the turn, the two directive words ("heavily",
"dedup") now have concrete, priced content, and the two newcomers slot in as
a target class and a header vocabulary rather than as rivals. The moat was
never the wire format; it is the turn, priced exactly, under policy, with an
audit trail — and after this round, that claim carries four pinned trees of
evidence instead of an assertion.

---

## Addendum (2026-08-20): Router.com, and the narrowing of "only ours"

This document was produced 2026-08-19. **Ramp launched Router.com the same
day** — a commercial OpenAI-compatible routing gateway, run internally at Ramp
for three years at ~2.75 trillion tokens/month, free through 2026, with NVIDIA
NeMo Switchyard's stage router live in it. The evidence is
`../research/router-com-deep-dive.md` (second-hand: egress to every primary
host was blocked, confidence marked per claim); the ruling is
`router-com-commercial-overlap.md`. Where that ruling and this document
disagree, that one wins.

**The dedup verdict above survives, narrowed.** The seven-item claim — "nobody
in this survey owns the turn: the durable log under a fenced lease, prefix
admission, per-principal policy and budgets, exact per-turn pricing across local
and frontier, steering, and the arm-instrumented judge" — loses two items.
**Ramp** ships them — Router.com together with its companion AI Token Spend
Management, the half it actually sells: **per-principal policy and budgets**
(limits by team, project, and key) and **per-request cost attribution across
providers**, the frontier half of the fourth item.
Read the claim as five items from here: the durable log under a fenced lease,
prefix admission, local-and-frontier on one comparison axis, steering, and the
arm-instrumented judge. Budgets and cross-provider pricing are table stakes now,
not differentiators.

**The method defect matters more than the correction.** This round read four
*trees* and concluded a market position. Router.com was invisible to it because
repository surveys see published source, and a three-year-old internal product
publishes none. Future rounds read shipped products alongside trees.

**Three things here are re-weighted rather than reversed:**

- The `ToolSignals` port (§"What 'use heavily' resolves to", item 3) was
  scheduled into the validate seam only. Ramp measured Switchyard's stage router
  — the same signal family, consumed by the *router* instead of a judge — at 58%
  cost and 33% runtime against single-model controls. The port feeds both
  consumers. This is the highest-value item in the new ruling.
- The Switchyard contribution track gains a production adopter downstream, which
  raises the value of every item on it and confirms this document's
  version-identity rule: a production adopter is evidence about which rev is
  real.
- **The Messages-surface ruling is now conditional.** "Roundhouse does not build
  a native Anthropic Messages tool loop" was decided against open-source
  neighbors. Switchyard preserves Anthropic API compatibility and ships a Claude
  Code launcher; whether Router.com exposes `/v1/messages` was not established
  and is the highest-value unknown in the deep dive's §8. If it does, this
  ruling hands the largest coding-agent client to a competitor and must be
  re-argued.
