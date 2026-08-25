<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Plan: frontier-only model selection, the text steer, and the Switchyard benchmark (M10)

> **Status: proposed design.** Direction set by the product owner on
> 2026-08-22: intercept codex and model-select per Switchyard guidance,
> frontier models only at first — mimic a user on a sol-only session with a
> fraction of calls rerouted to terra/luna, then swap the source so sol maps
> to kimi-k3 on OpenRouter with fallback to sol and terra + deepseek-v4 serve
> the intermediate tier; pivot the steer from a synthetic tool call to a text
> instruction; keep MCP as the behavior-change surface; re-evaluate how much
> of the server must be stateful. Dynamo and Claude Code are explicitly out of
> scope for this phase. Evidence: `research/switchyard-routing-live-read.md`
> (Switchyard @ `053a61e`, 2026-08-24) and `research/openrouter-api-surface.md`
> (live fetch, 2026-08-24), plus the M9-era as-built facts cited inline at
> `dbfd4fd`/`cc1245a`. Where this plan and PLAN-agentic-control-plane.md
> disagree, this plan wins; it assumes PRs #6–#11 merge as they stand.

## 1. Ground truth this plan stands on

Facts, each with its citation, that the rungs below are built not to
re-discover:

- **"Recipe" is dead Switchyard vocabulary.** The live nouns are
  `[llm_clients.*]` (format/base_url/api_key_env), `[targets.*]` (a model on a
  client), `[routes.*]` (algorithm + targets); route types today are `noop`,
  `random`, `passthrough`, `llm_classifier` (capability | escalation |
  custom), `stage_router`, `advisor` (`config.rs:464-578@053a61e`). "One of
  Switchyard's best recipes" defensibly means one of three calibrated files in
  `benchmark/routing-profiles/`: `tb21-escalation-opus-glm-deepseek.toml`
  (the configuration behind the published Terminal-Bench 2.1 blog result —
  weak-first, a judge each turn, two consecutive escalate verdicts latch
  strong) and the two `tau2-telecom-custom-opus-qwen-*` files whose headers
  carry measured operating points — 0.903 ± 0.071 solve at 45% weak-tier
  turns (balanced), 0.891 ± 0.029 at ~85% (aggressive) — each with the
  explicit caveat that the *wiring* is not as measured and thresholds do not
  transfer across model pairs or domains. Any citation we make carries those
  caveats verbatim.
- **The stage router is two tiers**, `Capable`/`Efficient` reported as
  `strong`/`weak`; the scorer is five constants and two pure functions
  (`tanh(5.0 · 0.10 · (severity/0.7 + spinning + exploring −
  production_intensity))`, threshold 0.5 calibrated on SWE-Bench Pro
  Python-75), with hard overrides — escalate on `compacted || severity ≥ 1.0`,
  de-escalate on tests-passed-with-production — checked in that order
  (`util/stage.rs:36-48, 250-408@053a61e`). `pick_tier` is pure, sync,
  deterministic, and re-exported; a faithful scorer is a ~40-line re-write
  with zero new dependencies. The extractor half (ToolSignals) is already
  ported at `cc1245a`.
- **`capable_first` is explicitly unbenchmarked upstream** — every published
  number is `efficient_first`, and their server warns at startup when a route
  selects the other mode (`stage_router_routing.md:77-84@053a61e`). The
  sol-default session this phase benchmarks is therefore *our* experiment;
  citing their numbers for it would be dishonest.
- **Switchyard steers by text already.** `HandoffNoteConfig.escalation_note`
  is appended to the trailing user message of the *forwarded request only* —
  never the caller's conversation, so notes cannot accumulate — and is gated
  by default (`only_on_wrong_signal_escalation = true`) so the note never
  tells the capable model the efficient one was stalling when it wasn't
  (`util/stage.rs:436-493@053a61e`). Per-tier system prompts are the second,
  every-turn injection surface. Their production wording ships in
  `dev-server/config.toml:50@053a61e`, `[router-guidance] …` — and that dev
  config is already written in sol/luna vocabulary, sol capable, luna
  efficient.
- **Their benchmark harness is real and runs codex against OpenRouter.**
  `benchmark/` is a Harbor (Terminal-Bench) runner with a direct-upstream arm
  and a routed arm selected by `--server-config`; task sets include
  Terminal-Bench Lite/2/2.1, SWE-Bench Pro, and a curated 20-task subset for
  fast A/B (`tb_lite_subset_20.txt`); artifacts carry solve rate
  (`result.json`) and routing stats, with the stated limitation that per-task
  cost attribution is not produced by the Rust server. Codex is pinned at
  0.144.5 in the task images — older than both our box binary (0.146.0) and
  our Cargo pin; the vigilance file gains that row.
- **Nothing upstream falls back across targets on a failed dispatch.** Tier
  movement is signal- and judge-driven; the only cross-target retry is the
  context-overflow path. "kimi errored, serve sol" has no upstream precedent
  — it is a mechanism this plan adds deliberately, not a port.
- **`POST /v1/decision` exists** upstream: a decision-only route returning the
  selected target, an ordered fallback list, and non-secret client wiring,
  paying for judge/classifier side-calls but skipping the answer model
  (`switchyard-server/src/lib.rs:587-718@053a61e`).
- **OpenRouter's `/api/v1/responses` is GA** (since 2026-07-25), speaking the
  OpenAI Responses shape our one real frontier client already speaks; `store`
  is `const: false` (we already send `store: false`), `previous_response_id`
  is rejected (we never send it), SSE streams carry comment keep-alives
  (`: OPENROUTER PROCESSING`) a parser must ignore, and usage arrives with
  dollars attached (`cost`, `cost_details`). `/messages` (Anthropic shape) and
  `/chat/completions` also exist; `GET /models` carries per-model pricing and
  undated benchmark scores, and `GET /api/v1/benchmarks` is the versioned
  surface with `meta.as_of`, `meta.version`, and a `citation` field whose
  attribution is **required for republication** — which a savings dashboard
  does. Model ids must be written in full: `moonshotai/kimi-k3` is one row,
  but "deepseek v4" is five concrete ids plus a tilde-alias, and the bare
  `deepseek/deepseek-v4-pro` is pinned to the April snapshot, not tracking.
  Provider variants of one model span a 2–4× price range; pinning is by the
  `provider` preferences object, and a native `models` fallback field exists
  on every inference route.
- **As built, the engine holds exactly one frontier client, chosen at boot**
  (`main.rs:131-169@dbfd4fd`; `Engine` stores one `Arc<dyn FrontierClient>`),
  the only real client is `OpenAiResponsesClient`
  (`openai_responses.rs:98-247`), routing picks one target and dispatches
  once with no second attempt (`engine.rs:1428-1477`), and the request's
  `model` field is accepted and ignored. The credential schema is **already
  three-tier** — deployment, project, key — with per-key credentials refused
  the fields that would let a member spend someone else's money
  (`control_config/config.rs:99-157, 740-960`); what does not exist is a
  runtime path to attach an external provider key without a file edit.

## 2. Rulings

**R1 — the steer is a text instruction; the tool call is retired as a steer
channel.** Outcome B becomes: the held turn is answered with an assistant
message carrying the rendered directive plus a restatement of the pending
request, so the harness sees the guidance and the task in one place and
decides. The injection boundary is unchanged (the judge's prose never reaches
the agent; directives are roundhouse's vocabulary only), the §10.2 usage
ruling carries over unchanged (the wire reports the turn's context
contribution; the ledger books the judge), and fulfillment becomes "the
guidance item is in the resent prefix" — which also deletes the
cancelled-steer hazard class entirely, since there is nothing to cancel.
Switchyard's two gating lessons are adopted as invariants: a steer narrates
only a signal-driven intervention (never an ambiguous fall-open), and
steering text rides at most once — it must not accumulate. The MCP surface is
re-purposed, not removed: mcp + slash-commands + skills are the *plugin*
surface for changing roundhouse's behavior (`prefer`, `set_quality_floor`,
`declare_intent`, …), and since codex loads `$CODEX_HOME/prompts` and skills,
`codex_launch` grows the ability to emit those files beside `config.toml`.
`fetch_steer` and the M4 emission machinery stay in the tree as proven
infrastructure (the frame vocabulary is still the dialect's), but no verdict
maps to a tool call any more.

**R2 — a second, cheaper steering surface: the escalation handoff note.** When
a tier change reroutes a turn, roundhouse may append a `[router-guidance]`
note to the *forwarded request only* — Switchyard's exact mechanism, with its
exact gating — never to the stored conversation, so the caller's log and the
prefix hash are untouched. This is distinct from R1 (which answers the held
turn); R2 decorates a turn that is being served. Both are arm-instrumented;
neither is on by default.

**R3 — Switchyard guidance is ported as calibrated configuration, not
consulted as a service.** The scorer is re-written (~40 lines, five constants,
attribution per the ToolSignals precedent) beside `routing/policy.rs`, where
the earlier ruling said scorers belong; the recipe files' calibrated constants
and their caveats are carried as the shipped defaults. The `/v1/decision`
sidecar is used in exactly one place: as the benchmark's A/B arm, where their
binary runs their config unmodified — the fairest possible comparison — never
as a runtime dependency (the version-identity rule and the three-API-changes-
in-a-week history both say no).

**R4 — tiers over the admitted pool.** A per-project recipe maps the two tiers
onto *ordered candidate lists* drawn from the catalog: e.g. capable =
[sol], efficient = [terra, luna]; later capable = [kimi-k3, sol], efficient =
[terra, deepseek-v4-flash-0731]. The scorer picks the tier; policy admission
is unchanged and runs first (a tier list never widens what the key admits —
the narrow-only rule). The request's `model` becomes the **declared
baseline**: recorded on the decision, priced as the counterfactual
(`baseline_model` in the savings vocabulary), and never routed on. The
`capable_first` shape (sol-default) ships flagged as uncalibrated, with the
recalibration protocol from `stage_router_routing.md:124-173` recorded as the
way to earn numbers for it.

**R5 — per-dispatch fallback is a new mechanism with narrow triggers.** Within
one turn's deadline, a transport error, timeout, 408/429/5xx from candidate k
advances to candidate k+1 of the same tier list (kimi-k3 → sol); a model
*refusal* or content filter is an answer, never a failover; every failed
attempt is booked (marked, never free) and the failover is a log fact. No
upstream precedent exists, so this ships with its own guard tests rather than
a citation. OpenRouter's native `models` fallback field is deliberately NOT
used: the failover must be a roundhouse log fact with roundhouse pricing, not
a silent substitution upstream of the meter.

**R6 — providers become data; clients become a registry.** An
`ExternalProvider` definition carries name, base URL, the four route paths
(models, chat-completions, responses, messages — each optional), auth (env
var or sealed ref), and extra headers. Shipped definitions: `openrouter`
(responses route; comment keep-alives tolerated; full model ids; provider
pinning via preferences when a specific upstream matters) and a customizable
OpenAI-compatible one (which is also how a Dynamo deployment or a
`switchyard-server` will be addressed later). The engine's single
`frontier_client` field becomes a registry keyed by the catalog entry's
`provider`, resolved per dispatch; boot still loads-or-dies per provider —
no silent stub. The existing Responses client is reused for OpenRouter (the
GA `/responses` route makes a chat-completions client unnecessary for this
phase; the dialect enum keeps its no-catch-all exhaustiveness so adding one
later is compile-forced).

**R7 — keys ride the existing three tiers.** An OpenRouter key attaches at
deployment, project, or key scope through the `CredentialsConfig` schema that
already exists; this phase adds no new credential variant and expects zero
edits to the denied `*credential*` files (the stage that finds otherwise
stops and reports rather than routing around the deny). Runtime attach/rotate
of external keys via the admin plane is deferred by name — the file is the
mechanism this phase, consistent with M8's bootstrap posture.

**R8 — `quality_prior` becomes sourced.** An offline import tool reads
`GET /api/v1/benchmarks` (the versioned surface), normalizes to 0.0..=1.0
with the normalization written down, and emits catalog entries stamped with
`meta.version`, `meta.as_of`, the per-item source, the model permaslug, the
fetch date, and the citation the meta block declares **required** for
republication. The models-list scores (undated) are refused as an input. The
import is a tool that generates configuration — never a runtime dependency,
per the rate-cards-never-in-source rule.

**R9 — spend is capped by our own budgets.** Every real-key test and benchmark
run executes under a project budget in the control plane — the grant ledger is
the cap, so a runaway loop hits `an_exhausted_frontier_budget_routes_local…`
behavior (here: refuses) rather than a surprise bill. Keys arrive as env vars
(`ROUNDHOUSE_TEST_OPENAI_KEY`, `ROUNDHOUSE_TEST_OPENROUTER_KEY`); suites are
feature-gated `e2e-frontier`, `#[ignore]` with a named reason, and skip loudly
without keys — the M9 gating pattern verbatim.

**R10 — the state question gets a design round, not a verdict here.** The
direction (lean proxy-ward; require state where it adds value) is recorded;
the evidence from this phase feeds it. Frontier-only per-turn selection needs
no durable state — the request carries the history, the extractor is
per-request, and after R1 even our own past interventions are visible in the
resent prefix — while exact settle, idempotent retry, replay/audit, and drift
reconciliation are what the log genuinely earns. The design round (D1) rules
on a declared mode spectrum — P0 proxy (auth + routing + metering), P1
ephemeral (today's no-Redis default), P2 durable — and on how much of Relay's
proxy posture to adopt rather than rebuild; it runs after M10.2 exists so the
ruling is made against a working stateless-shaped path, not a thought
experiment.

## 3. The rungs — every one a failing test first

- **M10.0 — the text steer.** `SteerChannel` re-defaulted; outcome B renders
  guidance + restated request as the turn's answer; fulfillment keys off the
  guidance item in the prefix; the e2e suite's steer tests updated to assert
  text (no tool call emitted, no MCP dependency for steering); the R2 handoff
  note behind the escalate action, gated. Tests:
  `a_steered_turn_answers_with_guidance_and_the_restated_request`,
  `the_next_turn_carries_the_guidance_in_its_admitted_prefix`,
  `a_steer_never_narrates_an_intervention_that_did_not_happen`,
  `steering_text_never_accumulates_across_turns`,
  `the_judges_prose_still_never_reaches_the_agent`, and the real-binary
  mirror: `a_real_codex_binary_receives_the_correction_as_text_and_acts_in_the_same_run`
  (one `codex exec`, not three — a text answer needs no dispatch round-trip).
- **M10.1 — providers, registry, keys, prices.** `ExternalProvider` config +
  the two shipped definitions; the provider-keyed client registry replacing
  the single boot-time client; OpenRouter reachability quirks (comment
  keep-alives, full ids) under test with a local fake; keys at all three
  scopes; the R8 import tool with a committed snapshot fixture. Tests:
  `a_catalog_entry_dispatches_through_its_own_providers_client`,
  `an_unknown_provider_is_refused_at_boot_not_at_first_dispatch`,
  `an_openrouter_shaped_stream_with_comment_keepalives_parses`,
  `a_key_scoped_to_a_project_is_invisible_to_another_project`,
  `an_imported_quality_prior_carries_its_version_date_and_citation`.
- **M10.2 — the selection brain.** The signal carrier into `RoutingContext`;
  the scorer beside `routing/policy.rs` with Switchyard's constants and
  attribution; per-project tier recipes with ordered candidates; declared
  baseline from `model`; per-dispatch fallback per R5. Tests:
  `a_stalling_session_escalates_to_the_capable_tier`,
  `a_tests_passed_production_turn_deescalates`,
  `the_scorer_never_picks_outside_the_admitted_pool`,
  `a_transport_failure_falls_forward_to_the_next_candidate_within_the_deadline`,
  `a_model_refusal_is_an_answer_not_a_failover`,
  `a_failed_attempt_is_booked_and_never_free`,
  `the_declared_baseline_prices_the_counterfactual_it_names`.
- **M10.3 — the real-key ladder** (feature `e2e-frontier`, budget-capped per
  R9). Rung by rung: (1) codex → roundhouse → api.openai.com, sol-only
  passthrough — the functionality baseline; (2) recipe on: sol capable,
  terra/luna efficient, `capable_first`, a session that stalls on purpose and
  escalates; (3) source swap: OpenRouter serving kimi-k3 with fallback to sol,
  terra + deepseek-v4-flash-0731 as the efficient tier; (4) reconciliation:
  the dashboard's committed/measured against OpenRouter's per-response `cost`
  and the `/generation` ledger — the first time the savings claim meets an
  external bill. Named tests:
  `a_real_codex_session_against_openai_completes_a_task_through_roundhouse`,
  `a_forced_stall_escalates_and_the_handoff_note_rides_the_forwarded_request`,
  `a_kimi_failure_falls_back_to_sol_and_the_log_says_so`,
  `the_dashboard_reconciles_against_the_providers_own_ledger`.
- **M10.4 — the Switchyard benchmark.** Adopt their Harbor harness with a
  roundhouse arm: codex → roundhouse → OpenRouter over
  `tb_lite_subset_20` first, then a fuller set; A/B arms: direct sol
  (baseline), `switchyard-server` running `tb21-escalation-…` unmodified
  (their guidance, their binary), roundhouse running the ported recipe.
  Report solve rate from Harbor, cost and routing narrative from our
  dashboard (which closes their own stated per-task-attribution gap), and the
  Shadow-arm judge evidence as the first real Intervention-Paradox data. The
  codex version pinned in their images (0.144.5) is recorded in the vigilance
  file and the harness prints all three codex versions in play.
- **D1 — the state-spectrum design round** (after M10.2): the P0/P1/P2 ruling
  and the Relay-posture ruling, run as a dedicated evidence + ruling pass per
  R10.

## 4. Risk register

1. `capable_first` has no upstream calibration; rung M10.3(2) may show the
   scorer rarely de-escalates from sol. That is a finding, not a failure —
   the recalibration protocol is the follow-up, and the tau2 profiles show
   what a calibrated operating point looks like.
2. OpenRouter's `/responses` unknown-field behavior is untested (the route
   authenticates before validating). First authenticated probe in M10.1
   settles it; until then the client sends only fields the schema names.
3. Per-dispatch fallback interacts with budgets: a failed attempt must settle
   its hold before the next candidate's grant, or a flaky provider could
   pyramid holds. The M10.2 tests cover the hold arithmetic explicitly.
4. The text steer changes what the *next* judge brief sees (guidance is now a
   conversation item). The brief renderer already quotes transcript lines;
   the M10.0 tests assert the guidance item is quoted, never re-executed.
5. Two keys and real spend enter the test surface. R9's budget cap plus the
   M9 hermeticity pattern (cleared child env, named env vars) bound it; the
   reconciliation rung is also the leak detector — an unexplained provider
   charge is a red test, not an anecdote.
6. Switchyard changed its route API twice in the week before this plan was
   written (`Algorithm::route` return type; `session_affinity` →
   `classify_trigger`). The constants this plan ports are pinned at
   `053a61e`; the vigilance rule applies before M10.4 runs their server as an
   A/B arm.

## 5. Out of scope, by name

Dynamo serving (returns after this phase proves the frontier path); Claude
Code (product owner: holding off); a chat-completions provider client (not
needed while OpenRouter's `/responses` is GA; compile-forced when it is);
runtime attach/rotate of external keys via the admin plane; consulting
`switchyard-server` at runtime; acting on codex's `_meta.threadId`; the P0
proxy mode itself (D1 rules on it first).

## Addendum (2026-08-24): three directives at build start

Recorded as M10.0–M10.2 implementation began. Where this addendum and the
rulings above disagree, the addendum wins.

**The benchmark's success criterion is impact-or-diagnosis.** M10.4 is not
"run the harness"; it is designed — by the orchestrator, not delegated — to
either show Switchyard-guided selection making a measurable difference or
produce the evidence for *why it does not*. Concretely: A/B/A arms (direct
sol; their server on their recipe unmodified; roundhouse on the ported
constants), enough trials on the 20-task subset for a confidence interval
before any full-set run, and — because their Rust server's own artifacts
"provide no task or trial attribution" — per-task cost, per-turn tier
decisions, and the declared-baseline counterfactual price from *our*
dashboard, which is the attribution their harness lacks. A null result must
decompose into one of: the scorer never fired (signal starvation — show the
signal log), it fired and the tier change didn't move solve rate (show the
paired tasks), or the operating point was mis-calibrated for the pair (show
the quadrant analysis from their own recalibration protocol). "It didn't
help and we can't say why" is the one outcome this design refuses.

**Budgets: unlimited now, session-window fair use as architecture.** The
benchmark projects run with no dollar ceiling — the `unenforced` basis the
reconciliation view already reports honestly — and R9's cap language is
relaxed accordingly (the reconciliation rung remains the leak detector).
What replaces hard caps in the architecture is **fair-use windows** shaped
like the frontier labs' own session limits: rolling 5-hour, 24-hour, and
7-day windows, each optionally capping tokens and/or dollars per project
and per member. Rulings, shaped by the M8 window hazard (a `balance()` read
under the wrong `BudgetWindow` destroys committed spend, which is why
`PATCH` of a window is refused): fair use is a **separate seam from
budgets** — a `FairUse` config and its own rolling, time-bucketed draw
ledger — never a new `BudgetWindow` variant and never a mutation of the
grant ledger's window arithmetic. Enforcement is admission-time: a turn
that would exceed a rolling cap is refused with a named
`429 fair_use_exceeded` carrying the window and the earliest retry time —
a refusal that is a log fact and stays retryable, like every other refusal
here. The memory implementation lands with M10.1; the Redis implementation
is deferred by name with its unlock condition (fair use across nodes is
only true with shared buckets; until then the boot warning names
single-node enforcement, the same honesty mechanism the directory store
uses). Windows are checked cheapest-first (5h before 24h before 7d) and a
member ceiling binds even when the project has room, mirroring the budget
ladder's own rule.

**R7 stands as ruled** (keys ride the existing three tiers; zero expected
edits under the credential deny).

One build-order note recorded for honesty: PR #10 (the ToolSignals port)
was merged into the M9 branch after #6 had already squash-merged, so its
content never reached `main`; the M10 implementation branch re-lands the
approved commit by cherry-pick and its PR says so.
