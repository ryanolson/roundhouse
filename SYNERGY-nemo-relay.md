<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# NeMo Relay ↔ Roundhouse: the synergy ruling

> **Status: direction.** This is the synthesis of
> `SYNERGY-nemo-relay-deep-dive.md` into a plan. The deep dive is the
> evidence base — every claim relied on below carries a file:line there —
> and this document is the ruling: what roundhouse adopts, what it emits,
> what it contributes back, and what it deliberately does not do. Where
> the two documents disagree, this one wins and the disagreement is a bug
> in one of them.

## The review, first

The deep dive was produced by one deep read of Relay at `c37b551`; before
ruling on it, its highest-stakes claims were re-weighed rather than taken
on trust. Three survive with full weight, and they are the three the
report itself flagged:

1. **The pass-through proxy roundhouse ruled in PLAN §3 is already
   shipping inside NVIDIA** — same upstream constant, same second-header
   mechanism, plus one flat contradiction: Relay sets
   `requires_openai_auth = true` where our read of codex rev `6344a65`
   said leave it unset. Two production-adjacent reads of the same client
   disagree, which means at least one is stale. PLAN §3 now carries this
   caveat, and resolving it is M7's first verification item — *before*
   any forwarding code is written, because the two settings bootstrap
   auth differently and a proxy built on the wrong one fails only in
   enterprise device-login mode, exactly where we intend to run.
2. **`crates/switchyard` in Relay is a deprecated HTTP client, not the
   escalation router.** The router lives in `NVIDIA-NeMo/Switchyard`.
   Our `EscalationPolicy` docs cite "Switchyard's escalation router"
   without saying which artifact that is, and the nearest thing a reader
   would find by that name is the wrong one. The native reproduction was
   the right call; the citation needs one sentence of precision.
3. **The savings vocabulary already exists as a published NVIDIA type
   and is missing exactly the safeguard we built.** Relay's
   `LlmOptimizationSummary` carries baseline/effective/saved/`Partial`/
   `Observed|Estimated` — near field-for-field parity with our model —
   but its `baseline_model` is whatever a router asserted. No capability
   gate. That is the cleanest two-way trade in the whole comparison: we
   adopt their schema, they adopt our gate.

One place the deep dive's framing was *adjusted* in synthesis: it
presents F2 ("Relay CLI fronts roundhouse") and F5 ("independence with
contribution flow") as competing verdicts. They are not competitors —
F2 is a *deployment topology* and F5 is a *dependency posture*, and the
correct ruling holds both at once. A topology is something a deployment
chooses per site; a dependency is something this tree carries everywhere.
Conflating the two is how E1's risks (two proxies, JWT-stripping,
upstream override) would get imported as permanent architecture instead
of guarded as a configuration.

## The ruling

**F5, with F2 as a supported topology.** Roundhouse stays independent —
no `nemo-relay` core dependency, no plugin-ization, no Switchyard client
— and buys interoperability at the format layer: emit Relay's published
schemas, copy its catalog shape, adopt three of its decision-record
ideas, and contribute back the three safeguards we have that it lacks.
`nemo-relay codex` fronting a roundhouse upstream is documented and
tested as a way to *deploy* roundhouse, never required to *build* it.

The division of labor, in one line:

> **Relay owns the harness, roundhouse owns the turn, Dynamo owns the
> metal.**

Relay launches and instruments the agent process — hooks, scopes,
redaction, tool-level visibility roundhouse can never see from the wire.
Roundhouse owns everything between a request arriving and a response
leaving: the durable log, prefix admission, policy, budgets, routing,
steering. Dynamo schedules and serves the inference. Every seam in the
plan below respects that line; every non-adoption below is a case where
crossing it looked cheap and is not.

### Why not the alternatives

- **F3 (roundhouse as a Relay plugin) is structurally wrong.** Relay's
  runtime is process-global, in-memory, request-scoped. Roundhouse's
  invariants — fenced lease, durable seq-ordered log, prefix admission,
  crash replay — have no home in a middleware callback, Relay explicitly
  bypasses stateful requests, and the one precedent for a routing plugin
  is deprecated in the very release that would host ours.
- **F4 (consume Switchyard, drop `EscalationPolicy`) fails on every
  axis.** A deprecated client for a service in another repo on a topic
  branch, a wire contract that accepts none of our signals (its only
  quantitative field is hardcoded `None`), and a 25 ms networked hop
  against a design whose stated reason for being native was removing
  exactly that hop. The existing ruling is re-affirmed and strengthened.
- **`nemo-relay` core as a dependency is disproportionate.** It drags
  OTel ×3, tonic, libloading, object_store — and `redis 1.1` against our
  `0.27`, a hard conflict. `nemo-relay-types` is the only cheap import:
  bitflags, chrono, serde, typed-builder, and a `uuid` pin identical to
  ours. **The dependency rule is: `nemo-relay-types`, nothing else.**
- **`nemo-relay-adaptive`'s Redis backend is blocked** by the same
  version conflict. Where we want an adaptive idea (ACG stability,
  below), we port the analysis, not the crate.

## The plan

Five steps, ordered by how much later work each unblocks. S1 is
housekeeping that prevents wrong turns; S2–S3 are the integration
surface; S4–S5 are the flow back and forward. None is a milestone of its
own — each lands inside the milestone it serves.

### S1 — Correct the record (now, before M6)

Small edits that stop the tree from misleading its next reader:

- **Fix the Switchyard citation** in `routing/policy.rs` (and the README
  section): name `NVIDIA-NeMo/Switchyard` as where the router lives, and
  note that Relay's `crates/switchyard` is a deprecated HTTP client for
  it — so nobody "upgrades" our native policy to a dead crate.
- **The `requires_openai_auth` caveat** is in PLAN §3 (done with this
  ruling). Resolution — read the current codex source, both settings, in
  device-login mode — is M7's first verification item.
- **Adopt three decision-record ideas** from the Switchyard contract,
  as fields not dependencies: `reason_code` (machine-groupable) beside
  the human reason on `DecisionRecord`; `baseline_route` as a
  first-class field stamped at decision time rather than reconstructed
  at dashboard time; and `observe_only` as a routing rollout mode — we
  built `ValidationArm::Shadow` for the judge and never gave the router
  the same courtesy.
- **Copy the pricing-catalog schema ideas** into `ROUNDHOUSE_CATALOG`'s
  next revision: tiered rate schedules, aliases, and above all
  `pricing_as_of` + `pricing_source` provenance — which CLAUDE.md
  already demands for the OpenRouter import and our catalog cannot yet
  record. `quality_prior`/`capability_band` stay roundhouse-owned
  extensions; Relay's schema has no slot for them and that gap is
  S4's contribution, not a reason to fork the schema.

### S2 — Emit the published formats (with M6's metrics work)

Roundhouse's log is a strictly better producer than Relay's exporter:
totally ordered by `seq`, durable, replayable from cold storage —
Relay's ATIF producer is in-memory and lost on crash. So we emit their
formats rather than inventing parallel ones:

- **ATOF** events from the session log, with a declared `data_schema`
  for our routing/decision marks so the existing ATOF→ATIF converter in
  NeMo-Agent-Toolkit consumes them without new code.
- **ATIF v1.7** trajectories via `GET /v1/sessions/{id}/trajectory`,
  produced by cold replay. Routing facts (candidates considered,
  measured/estimated split, serving mode, `seq`) ride in `extra`,
  typed and `data_schema`-tagged — the documented extension path.
- **`LlmOptimizationSummary`/`Contribution`** for the savings story.
  The mapping is near field-for-field (deep dive §C.4); `Partial` +
  `limitations[]` is exactly our "unpriced because no comparable model"
  arm.

All three depend on `nemo-relay-types` only. If even that pin chafes,
the ~15 ATIF structs are re-implementable in an afternoon — the spec is
published — but start with the crate: a shared type is a conversation,
a copy is a fork.

### S3 — Rule the topology (with M7)

Two supported deployments, one of them guarded:

- **Direct**: Codex → roundhouse → {Dynamo | frontier}. The reference
  topology; everything in PLAN §3 assumes it.
- **Chained**: Codex → Relay gateway → roundhouse → {Dynamo | frontier}.
  Supported, because Relay's hook-level visibility is real value we
  cannot replicate from the wire — but only with **chain guards**
  against E1, verified by integration test before the topology is
  documented as supported:
  1. Relay must be configured with roundhouse as its upstream base URL
     *and* its ChatGPT-upstream override must not route around us —
     detect the `alignment.rs` redirect case and fail loudly if
     roundhouse never sees the traffic it is budgeting.
  2. Relay's `OPENAI_API_KEY` substitution silently changes who pays
     under `PassThrough`. In chained mode roundhouse must be able to
     tell which credential actually went upstream, or refuse to attest
     spend it cannot attribute.
  3. Prefix admission must survive Relay's decode/re-encode. Our
     `same_item` comparison is role+content only, which should be
     robust to reserialization — but "should" is a claim, and the
     codex wire-shape suite gains a re-encoded-history case to make it
     a fact.
  4. One event log is authoritative for accounting: ours. Relay's ATOF
     stream is observability; a steered turn will look like an ordinary
     tool call in Relay's trajectory and that divergence is documented,
     not reconciled.

### S4 — Contribute back (after M7 proves the seams)

Three upstream contributions, each closing a real Relay gap the deep
dive verified as absent, each small:

- **`enforce_usage_reporting`**: Relay's gateway silently records
  zero-token, zero-dollar calls on streaming upstreams nobody asked for
  usage. Our add-`include_usage`-never-override rule is a one-file fix
  and the single most valuable correctness transplant.
- **The capability gate**: `quality_prior`/`capability_band` validation
  for `LlmOptimizationSummary.baseline_model`, so Relay's savings
  accounting stops accepting a 7B priced against a flagship — the exact
  trap our README is built around.
- **Realized cache evidence**: `CacheLedger`'s measured hit ratios as
  ACG's `expected_reads` input, replacing assumed reuse horizons with
  measurement where a roundhouse sits in the path.

### S5 — Import the two adaptive ideas (with M6)

Both are ports of an idea, not adoptions of a crate:

- **ACG stability as an M6 trigger signal.** A prompt whose stability
  score collapses mid-session is evidence the agent has gone
  off-distribution, computable with no model call — a fifth trigger
  orthogonal to the four planned, strengthening the conjunction that
  gates the judge.
- **`AgentHints.osl` as grant-sizing input.** The predicted output
  length directly improves the `expected_output_tokens` that sizes M3's
  budget grant — a known honest limitation. Read the header when a
  Relay sits in front; never require it.

## What this buys, and what it costs

Bought: the interception surface we ruled but had not built exists and
is maintained by another NVIDIA team (as a topology, when a deployment
wants it); our savings and trajectory stories become standard
machine-readable artifacts an existing toolchain consumes; our catalog
gains the provenance fields its own import policy already demanded; and
the two genuinely novel things in this tree — the budgeted turn and the
steering primitive — stay novel instead of being rebuilt around someone
else's runtime.

Cost: one light dependency pin (`nemo-relay-types`, `uuid` already
identical); schema copies that can drift (versioned on both sides, so
detectably); and the chained topology's guards, which are integration
tests we arguably owe E1 anyway. Upstream contributions need upstream
engagement, which is a relationship, not a diff — but these are all
NVIDIA projects, and "mesh and work great together" is the point.
