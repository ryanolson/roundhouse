<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Plan: the agentic control plane — tenancy, the roundhouse MCP, and the validate/steer loop

> **Status: proposed design.** Nothing below is built. Every code claim carries a
> `file:line` into the tree as it stands at this branch's base; every external
> claim about Codex was verified against the pinned rev `6344a65` this
> workspace already builds its conformance tests from. Three independent design
> passes were drafted and adversarially reviewed before this synthesis; where
> the reviews found real holes, the fix is in the text and the hole is named in
> §10 rather than silently patched.

Roundhouse today serves one anonymous tenant. This plan gives it three things
the agentic-front-end premise ultimately requires:

1. **A control plane** — users belong to projects, projects carry budgets,
   each user gets a fraction, and a key resolves all of it before a turn is
   admitted. Kept general enough that custom project/user/admin shapes can be
   built on it later.
2. **A roundhouse MCP server** — the agent (Codex today, Claude Code when a
   Messages surface exists) can read and *narrow* roundhouse's behavior
   mid-session: declare its intent, prefer local or frontier, set a quality
   floor, read its budget.
3. **A validate/steer loop** — after serving turns on a local Dynamo model,
   roundhouse can interject: hold the requested turn, pay for one side-call to
   a frontier judge, and either release the turn unchanged or answer it with a
   synthetic tool call into the roundhouse MCP that carries the correction.

The third is the reason the first two exist, and it completes an intention the
code already records: `EscalationPolicy`'s doc comment says the richer version
"latches to the strong target on a confirmed streak of trouble, which needs a
quality signal we do not yet collect" (`crates/roundhouse-core/src/routing/policy.rs:204-211`).
The validate loop *is* that signal, and the trigger/verdict/action machinery
below is what collecting it honestly costs.

---

## 1. Ground truth the design stands on

Facts first, because several of them killed otherwise-attractive designs.
Everything in this section was verified by reading code — ours, or the pinned
Codex tree at `6344a65` (a local checkout resolved from
`crates/roundhouse-server/Cargo.toml:59-62`) — or, where marked, by web
research with the source named.

**About our own wire surface:**

- The item vocabulary already holds tool calls: `ItemContent::ToolCall/ToolResult`
  (`crates/roundhouse-core/src/item.rs:48-56`), and `canonical_item` already
  parses incoming `function_call` / `function_call_output` items into them
  (`crates/roundhouse-server/src/responses_api/wire.rs:86-102`) — reading only
  `call_id`, `name`, `arguments` and ignoring `namespace` and `id`.
- Prefix admission compares role and content only (`same_item`,
  `crates/roundhouse-server/src/responses_api.rs:343-345`). So a tool-call item
  we commit and the client's verbatim resend of it match **by construction**,
  and the client-appended `function_call_output` arrives as ordinary new
  suffix. This is the single fact the whole steering choreography rests on.
- Nothing can *emit* a tool call today: `Session::complete` hardcodes
  `Item::assistant_text`, the SSE follower projects only `OutputTextDelta`
  (`responses_api.rs:520-564`), and `concerns()` filters `ItemAppended` out of
  the stream entirely (`responses_api.rs:503-517`). The emit side is the new
  work; the admit side already exists.
- The request's `tools`, `tool_choice`, `model` etc. are accepted and ignored
  (`responses_api.rs:114-122`). `tools` becomes the capability-detection
  channel; the ignore posture for everything else is kept.
- `SessionCreated` is dead vocabulary: `Session::record_created`
  (`crates/roundhouse-core/src/session.rs:404-410`) has no production caller,
  and store-side meta (`_model_policy`, Redis `SessionMeta`) is write-once and
  never read back. A session's log begins at `TurnStarted` today.
- `Compat::bound_session` makes generation 0 the cache key **verbatim**
  (`responses_api.rs:259-264`): two tenants both naming a conversation `main`
  would share one log, one lease, one warm prefix. Not a future scaling issue —
  a cross-tenant disclosure the moment a second tenant exists.

**About Codex, from the pinned source:**

- MCP tools appear in the request's tools array **only** as a namespace object
  — `{"type":"namespace","name":"mcp__roundhouse","tools":[...]}` — never as
  flat functions (`core/src/tools/handlers/mcp.rs:362-394`).
- Dispatch is an exact `HashMap` lookup on `ToolName { name, namespace }`
  (`core/src/tools/router.rs:164`, `registry.rs:440-444`), and nothing in the
  tree splits a flat `mcp__server__tool` back apart. **A synthetic call must
  carry `namespace` as a separate wire field** or it will not resolve.
- Dispatch happens directly off `response.output_item.done` — the adversarial
  review read the private `core/src/stream_events_utils.rs:288` and found
  `handle_output_item_done` calls `ToolRouter::build_tool_call` on whatever
  item arrives, with no dependency on a preceding `output_item.added` or on
  argument deltas. (`response.function_call_arguments.delta` is trace-only in
  the pinned `codex-api` — arguments must never be streamed.)
- An unresolvable tool name yields
  `FunctionCallError::RespondToModel("unsupported call: …")` (`registry.rs:828`)
  — returned to the model as tool output, turn continues. **Steering failure
  degrades to a confused model, never a crashed agent**, so optimistic emission
  is safe.
- Deferred tools are omitted from the model-visible list entirely
  (`tools/src/tool_executor.rs:58-62`): absence of our namespace from `tools`
  is *not* proof the MCP is unregistered.
- `codex-core` — the crate that actually executes tools and runs MCP clients —
  is private (`core/src/lib.rs:142` declares `mod tools;`) and not a pinned
  dependency. Our conformance tests can prove the wire shape through
  `codex_api::ResponsesClient` and `codex_protocol` serde; only a real
  `codex exec` E2E can prove dispatch.

**About MCP and the clients (web research, sources in §11):**

- **MCP sampling is dead.** Neither Codex (no handler anywhere in the pinned
  tree) nor Claude Code supports `sampling/createMessage`, and the 2026-07-28
  spec revision deprecates it outright. The idea of the validate call being
  sampled on the client, billed to the user's own subscription, **cannot be
  built**. Roundhouse holds the user's frontier API key server-side and spends
  it itself — that is the only mechanism, not one of two.
- Notifications never reach the model (Codex logs them and does nothing;
  `rmcp-client/src/logging_client_handler.rs:82-90`). Elicitation reaches the
  *human*, not the model. The only server→model influence channel that works
  on both clients is **the result of a tool the client itself called** — which
  validates the reply-with-a-tool-call design rather than constraining it.
  (The 2026-07-28 revision makes this explicit: server-initiated requests only
  while processing a client request.)
- Codex and Claude Code genuinely differ on naming: Codex namespaced-object,
  Claude Code flat `mcp__<server>__<tool>`. Both pass arbitrary static bearer
  headers; Codex requires `bearer_token_env_var` for streamable HTTP
  (`config/src/mcp_types.rs:429`).
- Consumer OAuth (ChatGPT/Claude subscription tokens) as a server-side
  credential has no vendor approval, no gateway precedent, and OpenAI's own
  guidance for programmatic Codex use is "switch to an API key."

**About prior art (web research):**

- LiteLLM's `team_member_budget` is the one existing per-user-fraction-of-a-
  shared-budget precedent; its single-key-plus-selector pattern
  (`x-litellm-end-user-id`) has a documented flaw (issue #28750): the budget
  cannot be scoped to a compound (key, team) — exactly the compound
  `(user, project)` attribution we need.
- Every vendor with an admin plane separates the admin credential from the
  model-calling credential structurally (OpenAI `sk-admin-`/`sk-proj-`,
  Anthropic `sk-ant-admin-`/`sk-ant-api03-`).
- **No surveyed gateway degrades to a free local tier on budget exhaustion** —
  every fallback chain (LiteLLM, OpenRouter, Portkey, Kong) terminates in
  another hosted, metered model. Degrade-to-local is roundhouse's novel move,
  and it falls out of the accounting (§4) rather than being bolted on.
- The inflight-supervision literature is mostly cautionary. The Intervention
  Paradox (arXiv 2602.03338): a critic with AUROC 0.94 caused a 26-point
  *collapse* in one agent's end-to-end success; harm tracks the agent's
  disruption–recovery ratio, not the critic's accuracy; even perfect failure
  prediction has a 4–8 pp ceiling. AEGIS (arXiv 2606.06660): a random-trigger
  placebo recovered nearly as much as budget-matched blind escalation — no
  trigger claim is defensible without a placebo arm at matched spend. "Steer,
  Don't Solve" (arXiv 2606.21811): a good critic made the system more accurate
  *and cheaper*, because it shortens trajectories — validation can be
  cost-negative. These three results shape §6 more than any protocol fact.
- No prior art was found of an inference gateway exposing agent-callable tools
  to control its own routing/budget. Stated as "searched and not found":
  feature 2 appears genuinely novel.

---

## 2. The user's question, answered: one key per (user, project)

**One API key per (user, project) membership. Not one key per user with a
project selector.** All three design passes reached this independently, for
converging reasons:

- **It makes the resolved caller total.** `Principal { project, user, key }`
  with no optional field — "which project is this?" is not a question any code
  path can ask. A request-time selector makes it `Option<ProjectId>`
  everywhere below the extractor.
- **The session namespace needs the project before the body is parsed.** The
  fix for the `bound_session` collision is
  `SessionId::new("{project}/{user}/{cache_key}")`, and if the project came
  from a per-request header it could change mid-conversation — forking the
  session and losing the warm prefix every time a client's header wobbled.
- **A client-supplied selector is unauthenticated**, and the one production
  system with that pattern imported exactly the flaw we cannot afford
  (LiteLLM #28750: budget unscopable to a compound key). If a request carries
  `X-Roundhouse-Project` anyway, it is verified against the key and mismatches
  are refused (`403 project_mismatch`) — an assertion the server checks, never
  a selector the server obeys.
- **One secret serves both surfaces.** Codex's `model_providers.*.env_key` and
  `mcp_servers.*.bearer_token_env_var` can name the same variable, so the same
  key that pays for a turn authenticates the MCP tool call that steers it —
  and the MCP server *knows* which membership's policy it may narrow rather
  than being told.
- **Revocation blast radius matches the org chart**, and it is the
  OpenAI/Anthropic precedent (`sk-proj-`, workspace keys).

The cost — a user on five projects holds five keys — is one config stanza per
project, in a file that is already per-project in practice. The credential is
still not the identity: a key *resolves to* a membership, so an SSO-JWT
resolver later (LiteLLM's JWT→virtual-key shape, the closest good precedent
for "one secret in hand, per-(user,project) record behind it") is an additive
resolver, not a schema change.

**Format:** `rh_turn_<43 base62 chars of 32 CSPRNG bytes>` and `rh_admin_<same>`.
Role in the prefix, enforced structurally: a `KeyScope` enum
(`Turn(Principal) | Admin`) that the extractor matches on, so an admin key
cannot serve a turn and a turn key cannot mutate tenancy — the one convention
every surveyed vendor shares. Stored as `sha256(secret)`; the hash *is* the
lookup key, so there is no comparison to time. SHA-256 rather than a slow KDF
because 256 bits of CSPRNG entropy are not password-shaped — a work factor
defends against a dictionary that does not exist, at 50–100 ms per turn
admission. (That reasoning goes in `auth.rs`'s module doc verbatim, because it
is exactly what a future reader would otherwise "fix.")

---

## 3. The control plane

### Entities

Five, in `crates/roundhouse-core/src/control.rs` (vocabulary and traits belong
in core — a separate `roundhouse-control` crate would need to be depended on
by core (`RoutingContext` borrows the policy), fleet (credentials) and server,
which is the graph position core already occupies):

- **Project** — name, `Budget`, profile name, `ModelAccess`, `CredentialMode`,
  archived-not-deleted (spend history outlives the project).
- **User** — stable unique handle (email / SSO subject).
- **Membership** — the load-bearing edge: role (`Owner | Member`),
  `Allocation` (the user's fraction), narrow-only `ProfileOverrides`. Budget
  fraction lives here, not on the user: frugal on one project and unconstrained
  on another is the ordinary case.
- **ApiKey** — `KeyScope`, hash, display tail, revoked-at.
- **Credential** (BYOK) — sealed provider API key (§3, BYOK below).

Deliberately absent in v1: an `Org` (the deployment is the org; additive
later) and policy-profile rows (profiles are deployment configuration and live
in a file, below).

### Configuration before CRUD

`ROUNDHOUSE_CONTROL_PLANE` names a JSON file, exactly the `ROUNDHOUSE_CATALOG`
idiom (`crates/roundhouse-server/src/catalog_config.rs`): the config format is
the deserialized types; a named-but-unreadable file stops the process; a
validate boundary rejects duplicate key hashes, unknown references, fractions
outside `0.0..=1.0`, and a `ModelAccess` filter that matches nothing in the
loaded catalog (a filter admitting nothing routes everything local and looks
like a cost win). **No secrets in the file**: frontier credentials are named
by env var; keys appear only as SHA-256.

Unset means `ControlPlane::Open`: every request resolves to a single
`default/default` membership with no budget and no filter, warned at boot in
the same voice as the echo-catalog stub. This keeps every existing test green
with one line of wiring and — more importantly — means there is no
`Option<Principal>` anywhere below the extractor.

The admin-plane HTTP API (REST-nested, Anthropic's shape:
`POST /v1/admin/projects`, `PUT /v1/admin/projects/{p}/members/{u}`, key mint
returning the secret exactly once, the budget reconciliation view) is a late
milestone (§9 M8), writing the same state the file expresses.

### Identity lives in the log

`SessionCreated` is widened and finally emitted:

```rust
SessionCreated {
    model_policy: String,
    #[serde(default)] principal: Option<Principal>,  // None only for pre-control-plane logs
    #[serde(default)] profile: ProfileName,
    #[serde(default)] arm: ValidationArm,            // §6 — stamped at creation, replay-stable
}
```

Emitted from `Engine::run_turn` after `open_observed`, when `last_seq == 0` —
the one place that already holds the lease, so it is race-free and idempotent
by construction (a log is empty exactly once). `#[serde(default)]` follows the
`Usage::reasoning_tokens` precedent (`event.rs:44`). Because `open_observed`
replays from seq 0, every fold sees `SessionCreated` before any terminal event
— attribution without a side table, a secondary index, or a store-contract
change. `SessionStore::create_session` keeps its signature.

Authorization on the native surface is then a string comparison: `http.rs`
session routes gain the same extractor and reject ids outside the caller's
`{project}/{user}/` namespace. **This closes a hole the adversarial review
flagged as made worse by this design**: today `http.rs` streams the full raw
event log — items, routing decisions, prices — for any session id, and
namespaced ids are guessable where cache keys were not. All four routers
(native, responses, metrics, mcp) require a key; the metrics surface scopes
its snapshot to the caller (admin key → deployment-wide; turn key → own
project, own rows).

The `generations` map is re-keyed by `(Principal, cache_key)` — the review
caught that two projects sharing a cache-key string would otherwise share one
fork counter, and a history edit in project A would cold-start project B.

### Budget: a grant ledger, honestly named

The honest answer to "what did this project spend" and the number cheap enough
to check before every turn are different objects. **Naming them differently
and never summing them is the design**:

- **`measured_usd` — the authority.** Tokens folded from session logs (the
  fold gains a `by_principal` dimension keyed
  `(ProjectId, UserId) → ModelKey → Counters`, reusing `Counters` verbatim so
  the two folds cannot disagree about tokens; pre-control-plane sessions fold
  under `Unattributed`, marked and never merged). Dollars applied at snapshot
  from the live rate card, exactly as `MetricsSnapshot::build` does today.
- **`committed_usd` — the enforcement number.** A durable counter behind a
  `SpendLedger` trait (memory impl in core, Redis impl in
  `roundhouse-store-redis` beside the session store, both run against one
  `control_contract_suite!`). Written by two atomic Lua scripts:

  **`open_grant`** — called in `Engine::plan` between `quote()` and `choose()`:
  requests the dearest frontier candidate's cost, is granted
  `min(requested, project_remaining, member_remaining)` (both keys under one
  hash tag, so both ceilings bind in one atomic script), holds that amount
  under the `ResponseId` with a TTL of `turn_deadline_ms + slack`.
  **`settle_grant`** — at the settle seam after the log commit: releases the
  hold, applies the actual priced usage, **idempotent by `(session_id, seq)`**
  — the same rule `MetricsFold` states for itself (`fold.rs:74-77`).

  Crash story: holds expire lazily (a leaked hold self-heals within one TTL);
  a settle lost to a crash is re-driven by the replay `open_observed` already
  performs on every turn, through the same idempotent script — no sweeper, no
  cross-session index (Redis deliberately has none, `roundhouse-store-redis/src/lib.rs:6-45`).
  The one unrepairable case — a session never opened again — is bounded by its
  last turn and *visible* as negative `drift_usd` on the reconciliation view.
  A wrong number with its own dashboard line is a bug report; a quiet wrong
  number is the failure mode this repo exists to avoid.

  Two honest limitations, disclosed rather than hidden: the granted ceiling is
  computed from `expected_output_tokens`, so a reasoning-heavy turn can settle
  above its hold — the grant is an admission ceiling, not a hard bound on
  realized spend (the standard authorization-hold limitation; the overshoot
  lands in `committed_usd` and the next grant sees it). And because each
  `run_turn` rebuilds its `Session`, the replay-driven re-settle costs one
  extra read for the watermark rather than the zero a persistent cache would
  give — the hot path is three single-slot Redis round trips, same class as
  the lease.

- **The enforcement mechanism is the reservation, from day one.** The
  adversarial review killed the "process-local fold as the gate until
  multi-node" staging: the turn gate is per-*session* (`engine.rs:250-257`),
  so two concurrent sessions under one membership would both read a stale fold
  and jointly overspend — the race exists single-process. The contract test
  `concurrent_grants_cannot_jointly_exceed_the_limit` is the proof, and it
  runs against the memory impl before Redis exists.

**Allocation**: `Pooled | Capped { limit_usd } | Share { fraction }` on the
membership. Both project and member ceilings bind; the tighter wins (LiteLLM's
shadowing rule is a documented gotcha, not a feature). Shares may sum past 1.0
— allocations are ceilings, not a partition; the admin view shows the sum
rather than refusing it (Anthropic's workspace precedent).

**Windows**: `Total` and `Monthly` in v1 — enforced on the ledger, which can
reset at a window boundary. The *fold* cannot window yet (`fold.rs:83-90`
already notes watermarks cannot be pruned without event-time windowing), so
`measured_usd` is lifetime and the reconciliation view labels the window
column as ledger-sourced until fold windowing lands. Named in §10.

**Exhaustion**: `DegradeToLocal` (default) or `Refuse`. Degrade-to-local costs
one predicate — local candidates are already priced at `expected_cost_usd: 0.0`
(`crates/roundhouse-fleet/src/local.rs:161-171`), so
`candidate.expected_cost_usd <= granted_usd` with a zero grant excludes every
frontier candidate and admits every local one. No branch, no special case.

**The overload escape valve** (a ruling made after the original draft): under
`DegradeToLocal`, when the budget is exhausted *and* the local pool cannot
serve — every local candidate load-rejected or no local capacity at all —
the turn overflows back to frontier rather than failing. The budget is a
ceiling on choice, not a tourniquet on service. Three rules keep it honest:
the overspend settles into `committed_usd` like any other spend, so the
ledger visibly exceeds its limit rather than hiding the excess; every
overflow dispatch is a marked fact on its `DecisionRecord`, so "served on
frontier past exhaustion because local was saturated" is its own dashboard
number; and overflow relaxes only the budget axis — the allow filter and
quality floor still bind, and a spent frontier *cadence* is not bypassed,
because cadence is policy and overflow is a budget valve. Configurable
(default on for degrade mode; meaningless and rejected with `Refuse`), and a
degrade-mode budget with overflow off in a deployment with no local capacity
is refused at startup, the same promise-keeping check the cadence already
gets.
The turn is recorded with `budget_state: Exhausted` on the `DecisionRecord`
(`#[serde(default)]`), because a project that stayed under budget by serving
400 turns on a 7B model has not had the same month as one that never needed
to. `Refuse` terminates as `ResponseIncomplete { reason: BudgetExhausted }` —
a new `IncompleteReason` variant, so refusals are log facts, foldable, and the
turn stays retryable, which is correct for a limit an admin can raise.

### BYOK

Users attach provider **API keys**; the vision's "attach their Codex/Claude
accounts" half is **refused loudly, not quietly deferred**: no vendor
approval, no gateway precedent, and the client-side mechanism that could have
made subscriptions spendable (MCP sampling) does not exist. `CredentialKind`
has one variant, so the unsupported state is unrepresentable;
`POST /v1/admin/credentials` rejects OAuth-shaped input with
`400 oauth_credentials_unsupported` and a message naming the reason.

Secrets are sealed (XChaCha20-Poly1305 under a key from
`ROUNDHOUSE_CONTROL_KEY`) in the control store — or, in the config-file phase,
named by env var and never inlined. They reach the transport **on the quote**:
`FrontierQuote` gains a credential field, following the module's own rationale
for `wire_protocol` (`frontier.rs:228-236` — the quote is the only argument
`execute` receives). The engine keeps its single `Arc<dyn FrontierClient>`; a
client-per-user would be connection-pool machinery asked to hold a secret. The
quote carries a redacting handle, resolved to plaintext only inside the
client's `execute`; a `Debug`/serialized-log scan test enforces that no secret
ever lands in an event.

Resolution order is **configured, not implicit**: `CredentialMode` on the
project — `ProjectOnly | PreferUser (default) | UserOnly`. Under `UserOnly` a
member with no credential simply loses that provider's models from their
candidate set and degrades to local — the same mechanism as budget exhaustion,
a served turn plus a marker rather than a 500. Credential availability and
`ModelAccess` filter the candidate set **before** `choose()` (the review
caught the original placement in the connect branch as too late twice over:
`payer` must be stampable on the `DecisionRecord`, and savings must never be
priced against a model the caller could not reach — `best_frontier_alternative`
reads `decision.considered`, `fold.rs:264-274`). `payer`
(`Deployment | Project | User`) is recorded on the decision;
`BudgetCounts::AllFrontierSpend` (default) vs `ProjectPaidOnly` decides
whether user-paid spend draws down the project budget.

### Pass-through mode (a ruling for enterprise device login, verified against the pinned Codex source)

The BYOK refusal above forbids *storing and re-presenting* OAuth tokens. It
does not forbid **forwarding a credential inside the request the client
itself made** — and for enterprises whose Codex access is ChatGPT device
login, that pass-through is the transparent answer: `CredentialMode` gains a
`PassThrough` arm under which roundhouse forwards a frontier turn verbatim —
`Authorization` header included, held in-flight only, never persisted,
redacted from every log and event — to the upstream endpoint, while locally
routed turns never touch the credential at all, because roundhouse terminates
the API rather than tunneling it. The seat pays for frontier turns; local
turns are free; policy, budgets (as accounted seat-consumption), attribution,
and steering all still work, because every request and every usage frame
still passes through the one write path.

Facts verified against codex `3b45c29` (2026-08-19, device-login mode, both
configurations traced end to end) that make this buildable as configuration,
not protocol work:

- `requires_openai_auth` is a **route property**, and it is load-bearing. It
  decides whether the caller's own first-party Codex credential attaches to
  *this* provider route at all; its default is `false`
  (`model-provider-info/src/lib.rs:138-143`). Set `true` — with no `env_key`,
  `experimental_bearer_token`, `auth` command, or `aws` — and codex forwards
  the active ChatGPT device login to the configured custom `base_url`:
  `Authorization: Bearer <access token>`, `ChatGPT-Account-ID`, and
  `X-OpenAI-Fedramp` for fedramp accounts (`model-provider/src/auth.rs:197-219`,
  `304-320`; `bearer_auth_provider.rs:32-46`). Leave it unset and codex
  attaches **nothing** — `resolve_provider_auth` short-circuits to the
  unauthenticated provider even when a valid login exists
  (`auth.rs:205-207`) — and the TUI never offers the login screen for that
  provider (`tui/src/lib.rs:1909-1917`). Codex's own probe/control pair
  proves it: `custom_provider_does_not_inherit_ambient_auth_headers`
  (`auth.rs:476-494`) asserts an empty header map against
  `openai_provider_preserves_ambient_auth_headers` (`auth.rs:558-576`) with
  the same ambient credential and only the flag differing. A custom
  `base_url` is honoured under either setting;
  `https://chatgpt.com/backend-api/codex` is only the *default* when
  `base_url` is unset (`model-provider-info/src/lib.rs:289-303`). `env_key`
  beats the forwarded login — `bearer_auth_for_provider` runs first
  (`auth.rs:201-203`) — and `validate()` does not reject the pair
  (`model-provider-info/src/lib.rs:190-256`), so the two must never both
  appear: that is codex enforcing natively the mutual exclusion Switchyard
  states in config (`crates/switchyard-server/src/config.rs:249-254` @
  `5341f71`). The Agent-Identity bootstrap that once argued for leaving the
  flag unset is no longer on this path: it sits behind
  `Feature::UseAgentIdentity`, under development and off by default
  (`features/src/lib.rs:1529-1533`, reached via
  `model-provider/src/provider.rs:285-291` and
  `core/src/session/session.rs:1328-1332`), and when it does run and fails it
  degrades to the ChatGPT bearer rather than erroring (`auth.rs:256-278`) —
  we pin `use_agent_identity = false` regardless, because an agent assertion
  is not a credential roundhouse may forward on a user's behalf. Two of the
  three failure modes are **silent**, which is why this stanza is documented
  rather than inferred: the flag unset means roundhouse sees an anonymous
  request with no client-side error, and the flag `true` without a login
  yields the same on non-interactive paths (`auth.rs:215-218`). Only a named
  `env_key` that is unset fails loudly (`model-provider-info/src/lib.rs:329-345`).
  A `401` from roundhouse buys exactly one auth-recovery attempt plus one
  retry (`core/src/client.rs:2235-2296`), which is the behaviour we want from
  a missing pass-through credential.
  *[History — recorded so nobody re-litigates it. The original ruling here
  was read from rev `6344a65` and said the bearer attached to a custom
  `base_url` **unconditionally** absent `env_key`, and therefore prescribed
  leaving `requires_openai_auth` unset. NeMo Relay shipped
  `requires_openai_auth = true` in the same position
  (`crates/cli/src/agents/codex/launch.rs:199-205` @ `ca08901`), a flat
  contradiction. 2026-08-19, round 2: Switchyard's launcher supplied the
  reconciling hypothesis — the flag set conditionally per route, `true` when
  forwarding the caller's own OpenAI login, `false` + `env_key` otherwise
  (`switchyard/cli/launchers/codex_cli_launcher.py:79-91` @ `5341f71`).
  2026-08-19, M7 stage 0: the hypothesis is **confirmed** and the original
  ruling **refuted** against codex `3b45c29`. Relay was right for its
  configuration; our stanza was wrong and would have forwarded nothing at
  all. Switchyard pays the cost of the same branch in the other direction —
  its non-forwarding route must inject a dummy `OPENAI_API_KEY="switchyard"`
  (`codex_cli_launcher.py:97-102`) precisely because
  `requires_openai_auth = false` sends no bearer. Whether `auth.rs:205-207`
  post-dates `6344a65` or was simply missed cannot be settled here — the
  pinned clone is a single-commit snapshot with no history — so the fact is
  restated against `3b45c29` rather than re-pinned to the old rev.]*
- The wire shape is the **same Responses API** roundhouse already speaks —
  only the base URL and headers differ by auth mode. The upstream target for
  ChatGPT-authed traffic is `https://chatgpt.com/backend-api/codex` (whether
  its SSE framing is byte-identical to the platform API is the one empirical
  check left; this repo only proves the client sends the same request).
- `[model_providers.*]` supports `http_headers` / `env_http_headers`, merged
  before auth and untouched by it — so `X-Roundhouse-Key` (the ordinary
  `rh_turn_` key) rides beside the pass-through `Authorization`, and M1's
  principal resolution works unchanged from a second header. Do **not** name
  the provider literally `"OpenAI"`: `is_openai()` matches the name and would
  turn on the routing-hint header and remote compaction against us.
- Codex already sends `session-id`, `thread-id`, and `x-codex-turn-metadata`
  headers on every model request, and `session_id` doubles as
  `prompt_cache_key` — per-session isolation and tracking on the wire side
  need nothing new.
- Under `requires_openai_auth = true` codex also fetches the model catalog
  from the provider: `GET {base_url}/models`, carrying the forwarded
  credential (`model-provider/src/models_endpoint.rs:39, 86-109`, selected at
  `model-provider/src/provider.rs:418-435`). Roundhouse must serve
  `/v1/models` on the same base URL, or the deployment must pin
  `model_catalog_json` to an absolute path (`config/src/config_toml.rs:355`)
  and skip the remote fetch — which is what Switchyard's launcher does
  (`codex_cli_launcher.py:92-93`). This is not optional polish: without one
  of the two, the client's first action against a pass-through route is a
  catalog request roundhouse does not answer.
- The one real gap: MCP handshake headers are static/env-sourced, so an MCP
  connection cannot carry the codex session id natively. Correlating the MCP
  channel to a conversation uses the init-tool trick: `init_session` returns
  a minted id in its tool *output*, the client appends that output to its
  conversation, and the next turn's resent history carries it into the log —
  the session whose log holds the id is the session that made the call.

The ruling makes this a **pair** of client configs, selected by route exactly
as Switchyard selects it from `caller_auth_kind`. Never set `env_key`
alongside `requires_openai_auth = true`: the key wins silently and
pass-through stops working with no error anywhere.

Enterprise device login — the `PassThrough` arm:

```toml
model_provider = "roundhouse"

# The prerequisite `requires_openai_auth = true` brings with it, and the reason
# it is in this stanza and not the BYOK one: under that flag codex fetches its
# model catalog from `GET {base_url}/models` with the forwarded credential,
# before the first turn. Roundhouse serves no `/v1/models` today, so the
# catalog is pinned to a local file and the remote fetch is skipped — the same
# move Switchyard's launcher makes (`codex_cli_launcher.py:92-93`). Serving
# `/v1/models` on the same base URL is the other half of the either/or and
# retires this line; with neither, the client's first action against this route
# is a request nothing answers.
model_catalog_json = "/etc/roundhouse/codex-model-catalog.json"

[model_providers.roundhouse]
# Not "OpenAI": is_openai() matches this exact name and would turn on the
# routing-hint header and remote compaction v2 against us.
name = "Roundhouse"
base_url = "https://roundhouse.internal.example.com/v1"
wire_api = "responses"
# The load-bearing line: forward this client's own ChatGPT login. Omitted or
# false, codex attaches no credential at all and never prompts to log in.
requires_openai_auth = true
# env_key / experimental_bearer_token / auth / aws deliberately absent: each
# resolves first and would replace the forwarded login with a stored secret.
supports_websockets = false

[model_providers.roundhouse.env_http_headers]
# Rides beside the forwarded Authorization; this is what M1's principal
# resolution reads. The Authorization is the user's, held in-flight only.
"X-Roundhouse-Key" = "ROUNDHOUSE_API_KEY"

[features]
# Pinned off, not merely left at its default: enabled, Authorization becomes
# an Agent-Identity assertion bootstrapped against auth.openai.com instead of
# the user's ChatGPT bearer.
use_agent_identity = false
```

The API-key alternative, for seats with no login to forward — the ordinary
BYOK path, where nothing is forwarded upstream and `payer` comes from the
stored credential:

```toml
model_provider = "roundhouse"

[model_providers.roundhouse]
name = "Roundhouse"
base_url = "https://roundhouse.internal.example.com/v1"
wire_api = "responses"
# Stated explicitly because it is a route decision, not an omission.
requires_openai_auth = false
# Resolved before any first-party auth; unset at request time is a hard error
# naming the variable — the one loud failure of the three configurations.
env_key = "ROUNDHOUSE_API_KEY"
supports_websockets = false

[model_providers.roundhouse.env_http_headers]
# Same variable, second header, so principal resolution reads one header name
# across both routes and nothing downstream branches on the stanza in use.
"X-Roundhouse-Key" = "ROUNDHOUSE_API_KEY"
```

---

## 4. Per-key policy, and how it reaches the router

The engine keeps one `Arc<dyn RoutingPolicy>`. Per-key policy arrives as
**data**, exactly as `routing/policy.rs:47-53` prescribes ("the cheapest
honest next step is a client-supplied per-turn quality floor" — here
server-resolved, which is strictly better, because a floor the client asserts
is a floor the client can lower):

```rust
pub struct RoutingContext<'a> {
    // existing: session_id, turn_index, isl_tokens, candidates, ledger
    pub frontier_turns: u64,          // folded from Routed events — a projection, not a counter
    pub turn_policy: &'a TurnPolicy,  // resolved at admission, immutable for the turn
}
```

`TurnPolicy` carries `min_quality`, `budget_ceiling_usd` (the grant),
`model_access`, `frontier_cadence`, and the validate profile. One
`TurnPolicy::admits(candidate, ...)` implementation serves every policy —
"how a decision is made" (`RoutingPolicy`) stays separate from "what it may
do" (`TurnPolicy`), so a deployment-authored policy never re-implements
tenancy, and "a policy that ignored its constraints" is testable once,
centrally. `EscalationPolicy`'s audit branch must consult it too — today it
takes `max_by(quality_prior)` over *all* candidates
(`policy.rs:247-255`), which would escalate straight past an exhausted budget.

Layering, with one rule: **narrowing only, in one function.**

```
ceiling  = profile(project) ∘ membership.overrides      (admin-controlled)
effective = narrow(ceiling, mcp_overlay) ∘ narrow(·, escalation)
```

`narrow` is total and can only shrink the admissible set. This is what makes
the MCP tools safe to expose to the agent (§5), and — a hole the adversarial
review caught — it applies to the **validator's own Escalate action too**: a
judge verdict cannot buy a frontier turn for a project whose ceiling is
local-only; the escalation clamps to the best admissible target or degrades to
Continue, recorded either way. `DecisionRecord` gains a
`turn_policy_digest`, so a mid-session overlay shows up in the audit trail on
the very next `Routed` event, with no side channel to disagree with it.

Per-project model access is a **filter on the quote's output**, not a second
catalog — `CatalogConfig`'s invariant that router and dashboard price one rate
card (`catalog_config.rs:4-17`) survives, and one-Engine-per-project (which
would fork the fold, the turn gates, and the catalog) stays rejected.

---

## 5. The roundhouse MCP server

**In-process, a fourth router at `/mcp`** merged in `main::serve` beside the
existing three — it reads the same store and resolves the same principals; a
separate process would be a second reader of everything. Transport: streamable
HTTP, 2025-06-18 semantics, stateless (no `Mcp-Session-Id`; `GET /mcp` → 405,
which the spec permits for a server offering no stream — deliberate, since §1
established nothing we could push would reach the model anyway). Stateless is
also exactly where the 2026-07-28 revision lands, so the surface is
forward-compatible, and `fetch_steer` is the natural seam for Multi
Round-Trip Requests when Codex flips `Feature::Mcp20260728` off `Legacy`.

Implementation: the official `rmcp` SDK, pinned like the `codex-*` deps —
Codex's own client is rmcp-based, so wire-level disagreement is unlikely in
the direction that matters — but **behind a hand-rolled `ControlSurface`
trait** (plain serde types per tool, in a new `crates/roundhouse-mcp`). Tests
exercise the trait hermetically; rmcp binds it in one adapter file; if rmcp
fights the `axum = "=0.8.4"` pin or the rustls-only rule, the swap to a
hand-rolled POST-only JSON-RPC handler moves no test. A `cargo tree` gate (no
OpenSSL, no second axum) lands with the crate.

Auth: `Authorization: Bearer rh_turn_…`, the same extractor as the turn
surfaces, resolving the same `Principal`. Client config that makes the whole
thing one secret:

```toml
[model_providers.roundhouse]
base_url = "https://roundhouse.example.com/v1"
wire_api = "responses"
env_key  = "ROUNDHOUSE_API_KEY"

[mcp_servers.roundhouse]
url = "https://roundhouse.example.com/mcp"
bearer_token_env_var = "ROUNDHOUSE_API_KEY"   # bearer_token is rejected for streamable_http
```

### The tool surface — seven tools, deliberately small

Every listed tool costs tokens in the client's context on every turn. Every
tool is a **pure read of committed state or a write to the control-plane
store; none appends to a session log** — an MCP request arrives on its own
HTTP request, and a second writer would contend with the turn gate and the
lease. Steer fulfilment is a *projection* of the ordinary write path
(`open_steers`, §6), which is what lets the MCP handler stay a pure reader of
a stateful loop.

| Tool | Kind | What it does |
|---|---|---|
| `status` | read | Effective policy digest, admissible target *names* (no prices — an agent that can see prices can argue about them), project/member budget remaining each stamped `basis: measured\|committed`, tokens-since-validation, `open_steer` |
| `declare_intent` | write (intent record) | `{goal, plan_steps?, done_when}` — mutates no routing; its whole value is turning the judge's question from "infer the goal, then judge drift" into "here is the stated goal, name the divergence." The highest-value tool per token spent. |
| `prefer` | write (overlay) | `{mode: local\|frontier\|auto, scope, turns, reason}` — applied as `narrow(ceiling, overlay)`; over-asking returns `narrowed: true`, not an error. `reason` is required and stored: an unexplained routing change is unauditable. |
| `set_quality_floor` | write (overlay) | `{floor, turns, reason}` — literally the signal `policy.rs:47-53` names as missing. |
| `fetch_steer` | read | The tool the synthetic call names. Returns the corrective payload **committed at emit time** — a pure read, byte-identical on retry, single text block (matching the `Value::String` branch our canonicalizer round-trips). **Never does paid work**: the review caught that a handler that ran the judge on invocation would let the model (or a prompt injection) drain the validate budget by calling the tool in a loop. The judge runs only server-side at the interject seam; this tool only reads its output. Verifies the steer belongs to the caller's principal. |
| `report_outcome` | write (advisory) | `{steer_id, outcome: applied\|rejected\|not_applicable, note}` — feeds arm evaluation; absence is never an error and never blocks a turn. |
| `explain_last_route` | read | The last `DecisionRecord`, agent-readable — the audit trail as a tool. |

Conversation reference: session-scoped tools take an optional
`conversation` (the client's own `prompt_cache_key`), resolved through the
same `bound_session` namespacing as the Responses surface, so the two surfaces
agree by construction; omitted, the key's most recent session.

---

## 6. The validate/steer loop

### Where it interposes

Between heartbeat-taken and `dispatch` in `Engine::run_turn`
(`engine.rs:384-386`): the turn is admitted and durable, the lease is
renewing, nothing has been priced — deliberately before `plan()`, because a
held turn should not spend a fleet round trip, and the judge must never see
the candidate list. The dedup short-circuit (`engine.rs:350-376`) returns
*before* this seam, so **a retry of a steered turn replays it and never
re-runs the judge** — free, and worth its own assertion.

```
turn admitted ── Trigger::evaluate(&SessionState, &TurnPolicy)   pure, no I/O
    │                    │
    │ no fire            │ fires
    ▼                    ▼
 dispatch()          arm? (stamped in SessionCreated)
 unchanged           ├─ Shadow  → judge runs, action discarded, everything logged
                     ├─ Placebo → no judge; random-timing intervention at matched spend
                     └─ Live    → side-call to judge → Verdict → action map:
                          ├─ Continue        → A   dispatch() unchanged
                          ├─ Escalate{k}     → A′  dispatch() with narrowed floor, k turns
                          ├─ Steer{directive}→ B   complete turn with synthetic function_call
                          └─ Halt{reason}    → C   complete turn with guidance text
```

### Trigger — a budget gate conjoined with a signal, never a cadence alone

The cascade literature triggers on evidence of trouble, never elapsed time;
"validate more when things look fine" has *negative* expected benefit
(benefit-reversal, arXiv 2605.06350). All computable from `SessionState`, no
model call:

- **Gate (all must hold):** `tokens_since_last_validation >= T` (tokens, not
  turns — roundhouse prices every turn exactly, so a validator budgeted as a
  fraction of spend-since-last-check is self-scaling); never turn 0 (the rule
  `EscalationPolicy::is_audit_turn` already encodes); cooldown elapsed;
  `consecutive_interventions < cap`; **the claimed suffix does not fulfil an
  open steer** (the hysteresis that stops a steer re-triggering its own
  validation).
- **Signal (at least one):** result-aware no-progress repeat (same
  `(name, arguments)` *and* same output hash N times — same input with
  different output is progress and must not fire); ping-pong alternation;
  tool-failure streak; **cost anomaly** against the session's own trailing
  distribution — unique to roundhouse, because no published monitor prices
  each turn exactly at monitor time.
- **Excluded from v1, by evidence:** model-judged semantic drift (it is the
  thing being triggered, and its first flag lands at a median 83–84% of
  trajectory elapsed — an autopsy); confidence thresholds (miscalibrated,
  need per-pair per-domain retuning, no formal bound).

### The side-call

A hand-built `FrontierQuote` through the existing `FrontierClient::execute` —
no new transport. Four isolations, each load-bearing:

- **Its own cache key**: `{session_id}#validate` — distinct from the
  conversation's (which `engine.rs:682` deliberately keeps stable; a judge
  prompt on it would cool the hit the router just priced) yet stable across
  validations, so the judge's own prefix warms and the marginal cost of
  checking falls with use.
- **Its own deadline**, a bounded fraction of the turn's remaining budget: the
  checker must never break the checked. Timeout → `SideCallAbandoned`, turn
  released unchanged.
- **Its own budget grant**: if the budget cannot cover the check, validation
  is *skipped and the turn proceeds* — never fail a turn because we could not
  afford to check it (`ValidationDecided { outcome: NotRun }`).
- **Never the cache ledger**: a judge prompt is not a prefix of the
  conversation; feeding it to `CacheLedger` would falsely warm that target for
  the next real turn.

A lease lost mid-side-call fails the subsequent commit exactly like the
existing mid-dispatch failure path — the turn settles incomplete and
retryable, and nothing the fenced owner decided reaches the log, the side
call's own cost included. `SideCallCompleted` and `ValidationDecided` are one
`ControlRecord`, written as a single atomic append (the log has one writer,
and there is no way to be half the writer), so a lease lost between the judge
answering and that commit takes the cost with it exactly as a lease lost
between a dispatch's model call and *its* commit already took the dispatch's
cost — the new seam inherits the old failure's shape rather than earning a
narrower one. A judge answer that never reaches the log is not spent twice:
the client's retry is what re-attempts it, in the same way a dropped dispatch
is retried rather than double-booked. (New exposure the review named: this
seam turns a zero-latency window into a network round trip. Handled by the
same failure vocabulary, tested explicitly, including the case where the
judge answered before the fence landed.)

### What the judge sees, and what it may say

A `ValidationBrief` — bounded, deterministic projection: instructions
(truncated), the `declare_intent` record or last user message, the last K tool
call/result pairs compacted (name, argument hash, output head, failure flag),
and roundhouse's computed signals **as facts** ("this call has produced
identical output 4 times"), never as suggestions.

**Never in the brief: the candidate list, any price, the words local /
frontier / escalate.** LLM judges carry self-preference and same-provider
family bias; a GPT-family judge asked "should we have used a frontier model?"
is not a neutral instrument. The judge answers a *task* question; code maps
the answer to an action under policy — the escalation question is asked
exactly once, of code.

```rust
struct Verdict {
    on_track: bool,
    confidence: f32,              // recorded for calibration; gates nothing in v1
    divergence: Option<Divergence>,   // { at_step, description }
    missing_context: Option<String>,
}       // no suggested_action field exists — by construction
```

The directive the agent eventually sees is built from roundhouse's own
vocabulary alone — fixed sentences, `at_step` as a number, and the trigger's
computed `SignalFired` facts. `divergence.description` is **not** among them,
not quoted and not summarized: it is a model's free text about a transcript
that is attacker-influenceable by construction, and a `Halt`'s text is
committed into the conversation permanently, so a sentence that lands there
prefixes every later turn. The description is recorded whole in
`ValidationDecided`, for the operator reading the log and for calibration.
(The brief renders transcript content compacted, hashed and line-prefixed as
quotation for the same reason — §10 keeps this on the risk register rather
than calling it solved.)

### The action map — weakest intervention first, evidence-ordered

`map(verdict, trigger, policy, capability) -> SteerAction`, pure, in core:

- **Continue** is the cheap default — the Intervention Paradox's disruption
  cost is paid on every unnecessary intervention.
- **Escalate { k }** (outcome A′) is the *default for a real divergence*:
  dispatch proceeds under a narrowed floor for k turns. Invisible to the
  client — no synthetic item, no prefix concern, no MCP required — and it is
  the best-evidenced repair (AEGIS: changing *who acts* beat budget-matched
  blind escalation 10.1% to 4.6%; a good critic makes the system cheaper by
  shortening trajectories). Clamped through the ceiling like every other
  narrowing.
- **Steer { directive }** (outcome B) requires policy `ToolCall|Auto` **and**
  detected capability **and** a consecutive-intervention count inside
  `steer_after_interventions`. That knob defaults to `0`, which makes outcome B
  **unreachable out of the box**: escalation above claims every zero-count turn
  on any channel that is not `Off`, so the steer branch only ever sees counts of
  one or more and a cap of zero admits none of them. Deliberate — injected
  guidance is the weakest, most oscillation-prone action in the literature and
  the protocol-heavy path is the last resort on purpose. Opting in is one
  number: `steer_after_interventions: 1` makes an already-interrupted session
  eligible.
- **Halt { reason }** (outcome C) completes the turn with plain guidance text
  — named honestly: Codex ends its loop on a message with no tool call, so
  this *hands control back to the human*. It is the degrade path when the MCP
  is not registered, and it is the strongest argument that MCP registration is
  a product requirement, not an enhancement.

### Outcome B on the wire — the steered turn

The held turn **completes** carrying a synthetic `function_call` instead of
running the requested completion. Completing (never failing) is what makes
retry correct: only `ResponseCompleted` registers in `completed_turns`
(`session.rs:159-167`); an incomplete steered turn would re-enter the
interject on every retry and loop forever, and `response.incomplete` reads as
an error in Codex so the agent would never see the call.

**The log side needs no new event kind.** `Item` already carries
`response_id: Option<ResponseId>`, and an `ItemAppended` whose item bears the
response's id already means "this response emitted this item" — input items
carry `None` (`canonical_item` sets it). A new sibling
`Session::complete_with_item(response_id, item, usage)` commits
`ItemAppended { ToolCall }` + `ResponseCompleted` **in one atomic append
batch** (the same multi-event commit `begin_turn` already uses), closing the
crash window the review found between a decision and its realization.
A dedicated `OutputItemEmitted` kind was considered and rejected: the
adversarial review of that variant caught that `Compat::stored_items`
(`responses_api.rs:301-318`) rebuilds the comparison prefix from `ItemAppended`
only — a second item-carrying kind gives `stored_items`, `SessionState::apply`
and `ContextAssembler::rehydrate` two sources for one conversation, and the
first site to forget the second kind silently forks every steered session.
Reusing `ItemAppended` means those three sites need **zero changes**.

**The wire side** is two narrow projection changes plus new frame builders:

- `concerns()` returns `true` for `ItemAppended` whose item is a `ToolCall`
  **and** whose `response_id` matches — narrow on purpose: input items have no
  response id, and assistant text is already covered by the delta path
  (forwarding it would double the done frame).
- `project()` renders `response.output_item.added` + `response.output_item.done`,
  both carrying the complete item. No argument deltas — the pinned parser
  swallows them. Frame sequence for a steered turn: `created` → `added` →
  `done` → `completed`. Four frames, no text.

```json
{"type":"response.output_item.done",
 "item":{"type":"function_call","id":"fc_resp_01J…",
         "namespace":"mcp__roundhouse","name":"fetch_steer",
         "call_id":"rhsteer_resp_01J…",
         "arguments":"{\"steer_id\":\"rhsteer_resp_01J…\"}"}}
```

`namespace` as a separate field (Codex's dispatch requirement); item id in its
own `fc_` space (not `msg_1`); `call_id` *is* the steer id, minted from the
`ResponseId`, so a steer that no call named cannot be fetched and two steers
cannot collide; `arguments` minted once and stored in the item, never
re-serialized, so the client's verbatim echo matches by construction.

**The log stores the bare neutral name** (`ToolCall { name: "fetch_steer" }`,
no namespace) [2026-09-04, M17: superseded — the stored call now carries
the namespace beside the bare name as a forward-only field left out of the
render, the Responses canonicalisation keeps it, the projection re-emits it,
and a flat resend is a different call (M12 review, F10); the rulings are
R-N1..R-N10 in `PLAN-anthropic-messages.md`]: `canonical_item` already ignores `namespace` and `id` on the
way in, so Codex's namespaced resend and a future Claude-Code-flat resend
canonicalize to the same stored item, prefix admission cannot fork on a
dialect, and `turn_id` hashing is untouched for every existing item. The
namespace lives only in the wire projection, supplied by the client-dialect
config — which means a replay after a namespace reconfiguration would render
the new namespace, the same class of edge as a rate-card change re-pricing a
snapshot, documented at the site.

**Prefix admission of the steered turn, proven from the code as it stands**:
turn N stores `[…, ToolCall{call_id, name, arguments}]`; turn N+1's claim
resends that call (namespace and id ignored by canonicalization, `arguments`
held by Codex as a raw `String` and never reparsed) plus the client-appended
`function_call_output` → canonicalizes to the stored prefix plus a
`ToolResult` suffix → `suffix_after` admits exactly the suffix. The
`ToolResult` is *past* the overlap on turn N+1 and is compared on N+2 only
against the client's own stored copy. `reasoning` items are dropped on every
request alike (`wire.rs:107`), and Codex never sends `previous_response_id`
over HTTP. Fulfilment is a projection: `SessionState.open_steers` fills from
the `ToolCall` append and clears on the matching `ToolResult` — no second
writer, no new event kind. One caveat the review named and the tests must not
paper over: prefix admission cannot distinguish a genuine MCP round trip from
a client that *invented* a `function_call_output` — so nothing security- or
accounting-relevant may treat the resent output as proof the tool ran; the
control-plane store's own `fetch_steer` record is that proof.

**Accounting for a steered turn**: no `Routed` is emitted (nothing was
dispatched), so the fold's `Routed`→terminal pairing books **nothing** to any
model row for the turn itself (`fold.rs:157-179`), while the judge's
side-call books once under its own row. The dashboard total equals the sum of
its rows exactly once — this needs a comment at the site or it reads as a bug.
The wire `response.completed` reports the judge's usage: the client's turn
genuinely cost that, totals balance (both existing conformance assertions
check it), and `Usage::default()` would make our own dashboard exceed what
clients were told. Flagged in §10: if the real-CLI E2E shows Codex's context
bookkeeping mis-handles it, the fallback is `Usage::default()` plus a
dashboard-only line.

### New event vocabulary — three kinds, all money-or-control, none carrying a conversation item

```rust
SideCallCompleted { side_call_id, purpose, target, usage },
SideCallAbandoned { side_call_id, purpose, target, reason },
ValidationDecided { validation_id, trigger, arm,
                    outcome: NotRun { reason } | Judged { side_call_id, verdict, action } },
```

`NotRun` cannot carry a verdict; `Judged` cannot lack a side call — invalid
states unrepresentable. All three return `None` from `response_id()` and are
never projected to a client. `SideCallAbandoned` is separate from an
empty-usage completion because an empty-usage completion is indistinguishable
from a free one — the `consumed` heuristic (`fold.rs:187-191`) exists because
the old vocabulary couldn't make that distinction, and a new vocabulary should
not reproduce an ambiguity it is free to avoid. `ValidationDecided` is
separate from `SideCallCompleted` because money-facts and control-facts must
not merge: a Shadow run (verdict computed, action discarded) must be
distinguishable at the fold level, because that comparison is the entire point
of the instrumentation. Adding the kinds is compiler-enforced across every
exhaustive match (`session.rs:107`, `event.rs:215-227`, `fold.rs:139`,
`responses_api.rs:503,520`).

### Ship it off, measure before enabling

Default: **validation off per membership; Shadow arm available from day one.**
The Intervention Paradox says an excellent critic can collapse one agent and
leave another untouched under the identical policy — the property that decides
is the *agent's* disruption–recovery ratio, which can only be measured
per-deployment. `SteerChannel::Off` still runs Shadow, which is the
literature's 50-task pilot expressed as configuration. Arms
(`Live | Shadow | Placebo`) are stamped per-session in `SessionCreated`,
derived from `hash(session_id, arm_salt)` — deterministic, because a random
draw would break fold-equals-log on replay, which is disqualifying regardless
of statistical appeal. The dashboard reports three figures separately in the
house idiom: validation spend (measured), tokens-after-intervention vs the
arm-matched control (measured), prevented waste (estimated, derivable *only*
from the arm comparison — sampled, never modelled). Never a single
"validation saved you $X."

---

## 7. Client dialects, detection, and degradation

`ResponsesRequest` gains `#[serde(default)] tools: Vec<Value>` (doc comment at
`responses_api.rs:114-122` updated in the same change — a stale comment
claiming a field is unread is worse than the change). Detection:

```rust
enum SteerCapability {
    Namespaced { namespace: String },   // Codex: {"type":"namespace","name":"mcp__roundhouse",…}
    Flat { name: String },              // Claude-Code-style: mcp__roundhouse__fetch_steer
    Absent,
}
```

Match the *configured expected* name first (`Feature::NonPrefixedMcpToolNames`
already exists and drops the prefix). **Absence is not proof** (deferred
tools), so the policy field decides:

```rust
enum SteerChannel { Auto, ToolCall, Text, Off }
```

`Auto`: capability → outcome B, else C. `ToolCall`: optimistic emission (safe:
failure is a `RespondToModel` string, not a crash). `Text`: never B. `Off`:
never interject; Shadow still measures. Honesty note carried from the review:
today roundhouse has exactly one agent-facing surface, the OpenAI Responses
API — Claude Code's native dialect is the Messages API, so the `Flat` branch
is future-proofing for a Messages surface, not a claim that Claude Code
traffic exists in v1. The log's neutral tool names are what make that surface
addable without forking steered sessions.

---

## 8. What lands where

| Crate | New / changed |
|---|---|
| `roundhouse-core` | new `src/control.rs` (+`control/contract.rs`, `control_contract_suite!`): ids, `Principal`, `KeyScope`, entities, `Budget`/`Allocation`, `TurnPolicy` + `narrow`, traits (`PrincipalDirectory`, `SpendLedger`, `ControlDirectory`), memory impls. New `src/validate/` (trigger, brief, verdict, action map). Changed: `event.rs` (widened `SessionCreated`, `IncompleteReason::BudgetExhausted`, three side-call/validation kinds), `session.rs` (`complete_with_item`, `open_steers`, emit `SessionCreated`), `routing/*` (`RoutingContext.turn_policy`, `frontier_turns`, `DecisionRecord` digest/budget_state/payer, admits-consulting policies), `metrics/fold.rs` (`by_principal`, side-call booking, shared `settle`) |
| `roundhouse-store-redis` | new `src/control.rs`, `src/control_scripts.rs` (`open_grant`/`settle_grant` Lua, hash-tagged project+member keys), same contract suite |
| `roundhouse-fleet` | `FrontierQuote` gains credential handle; `StaticFrontierCatalog::quote` takes the access filter; first real `FrontierClient` impls (OpenAI Responses, Anthropic Messages) |
| `roundhouse-mcp` **(new)** | `ControlSurface` trait + per-tool serde types; rmcp adapter; documented-assumption block for the dispatch invariant until M9 closes it |
| `roundhouse-server` | new `src/auth.rs`, `src/control_config.rs`, `src/admin_api.rs`; changed `engine.rs` (interject seam, grants, credential resolution, `SessionCreated` emission), `responses_api.rs` (+`wire.rs` frames, namespaced session ids, `tools` field), `http.rs` + `metrics_api.rs` (gating/scoping), `main.rs` (fourth router) |

---

## 9. Milestone ladder — every rung a failing test first

The organizing bet: **the riskiest unknowns are wire-shape facts, not
architecture**, and each is provable against the pinned `codex-*` crates
before production code exists. Three invariants gate every rung: fold equals
log (replay is byte-identical, which is why arms are hashes); the per-principal
fold and the deployment fold sum (they share `Counters` so it can be asserted,
not hoped); measured and estimated never merge, and an unaccounted call —
now including a timed-out validator and a degraded turn — is marked, never
free.

- **M0 — Oracle first, no production code.** `ScriptedFrontierClient` (branches
  on `#validate` cache keys), `ResponseItem`-built item builders.
  Tests: `a_namespaced_function_call_round_trips_through_codex_protocol`,
  `a_function_call_done_frame_parses_without_a_preceding_added`,
  `function_call_argument_deltas_are_not_observed_by_this_client`.
- **M1 — Identity in the log, attribution in the fold.** Config-file control
  plane, extractor, namespaced session ids, re-keyed generations map, widened
  + emitted `SessionCreated`, `by_principal` fold, gated routers.
  Tests: `two_principals_using_one_cache_key_do_not_share_a_session` (the
  collision, made a failing fact before the fix),
  `a_turn_is_attributed_to_the_principal_that_paid_for_it` (with the
  folds-sum anti-drift assertion),
  `an_unknown_key_is_refused_before_a_session_is_created`,
  `a_native_surface_session_outside_the_callers_namespace_is_refused`,
  `replaying_a_log_recovers_the_principal`.
- **M2 — A policy knob visibly changes routing.** `TurnPolicy` into
  `RoutingContext`; access filter before `choose`.
  Tests: `a_quality_floor_excludes_a_target_the_default_policy_would_pick`,
  `a_filtered_target_never_appears_in_considered` (savings never priced
  against an unreachable model),
  `an_empty_admissible_set_fails_rather_than_silently_going_local`,
  `the_escalation_audit_turn_cannot_escalate_past_the_policy`.
- **M3 — Budget: grant/settle, exhaustion degrades to local.** `SpendLedger`
  contract + memory + Redis impls; grants in `plan`; settle at the seam;
  replay-driven repair.
  Tests: `concurrent_grants_cannot_jointly_exceed_the_limit` (the
  single-process race, closed at birth),
  `an_exhausted_frontier_budget_routes_local_instead_of_failing` (the loudest
  test in the suite — the novel behavior),
  `the_member_ceiling_binds_even_when_the_project_has_room`,
  `a_refused_turn_is_a_log_fact_and_stays_retryable`,
  `a_turn_killed_between_grant_and_settle_leaves_a_hold_the_next_turn_expires`,
  `a_lost_settle_is_repaired_by_the_next_open_of_the_same_session`,
  `an_estimated_usage_still_consumes_budget_and_stays_marked_estimated`.
- **M4 — Emitting a tool call and surviving its resend.** `complete_with_item`
  (atomic batch), narrow `concerns()`/`project()`, frame builders, `fc_` ids.
  Tests: `a_synthetic_function_call_arrives_as_codex_parses_it` (asserting
  `ResponseItem::FunctionCall{namespace: Some(..)}` and **not**
  `ResponseItem::Other` — the silent-drop failure mode),
  `the_steered_turn_emits_exactly_four_frames_and_no_others` (byte-level,
  extending `ordering_is_enforced_at_the_frame_level`),
  `the_resent_call_and_its_output_extend_rather_than_fork` (playing the agent
  with Codex's own types), `a_steered_turn_is_deduplicated_on_retry`,
  `an_assistant_text_item_is_not_forwarded_twice`,
  `a_third_turn_after_a_steer_still_matches_its_prefix`.
- **M5 — The MCP surface.** `roundhouse-mcp`, read tools + overlays; the
  narrow-only rule.
  Tests: `set_policy_changes_the_next_turns_route_digest`,
  `an_overlay_cannot_widen_the_ceiling` (`prefer frontier` on a local-only
  project returns `narrowed: true`),
  `fetch_steer_is_byte_identical_on_a_second_call_and_does_no_paid_work`,
  `fetch_steer_for_another_principals_steer_is_refused`,
  `an_mcp_call_during_a_running_turn_does_not_take_the_session_gate`,
  `a_get_on_the_mcp_endpoint_is_405`, plus the cargo-tree gate.
- **M6 — The validate loop end to end.** Trigger, brief, verdict, action map,
  arms, three event kinds, interject seam; default off.
  Tests: `a_repeat_with_a_different_output_does_not_fire`,
  `a_turn_fulfilling_an_open_steer_never_fires`,
  `the_cadence_alone_never_fires_without_a_signal`,
  `the_brief_contains_no_price_no_candidate_and_no_target_name` (the
  family-bias guard, as a negative assertion over the rendered string),
  `a_validator_verdict_never_becomes_a_conversation_item` (stored items
  byte-identical around a validation — the sharpest assertion in the design;
  without it every later turn forks),
  `a_side_call_books_under_its_own_model_row_and_never_reaches_the_cache_ledger`,
  `a_validator_timeout_releases_the_turn_unchanged_and_is_marked_not_free`,
  `an_escalate_verdict_clamps_to_the_ceiling`,
  `the_shadow_arm_judges_and_releases_unchanged`,
  `the_placebo_arm_intervenes_without_calling_the_judge`,
  `an_identical_retry_of_a_steered_turn_never_revalidates` (judge called
  exactly once across both requests),
  `a_lease_lost_mid_side_call_settles_the_turn_like_any_mid_dispatch_failure`.
- **M7 — Real frontier credentials.** Sealed store / env-var resolution,
  credential-aware candidate filtering, payer stamping, first real provider
  clients.
  Tests: `a_quote_never_carries_a_secret` (Debug + serialized-log scan),
  `a_principal_without_a_credential_for_a_provider_never_sees_it_in_candidates`,
  `an_oauth_shaped_credential_is_refused_with_a_reason`,
  `user_paid_spend_draws_the_project_budget_under_all_frontier_spend`.
- **M8 — Admin plane.** REST-nested CRUD, key mint/revoke, the budget
  reconciliation view (`committed_usd` / `measured_usd` / `drift_usd`,
  never summed).
  Tests: `a_minted_key_secret_is_returned_once_and_never_again`,
  `revoking_a_key_stops_it_within_one_cache_ttl`,
  `the_budget_view_reports_committed_and_measured_separately`,
  `drift_goes_negative_and_stays_visible_when_a_settle_is_lost`.
- **M9 — Real `codex` CLI E2E, feature-gated, off by default.** `codex exec`
  against a bound port: a generated config with both entries sharing one env
  var, a scripted task, a forced steer.
  Tests: `a_real_codex_binary_executes_our_synthetic_tool_call_and_returns_its_output`
  (the one test that closes the dispatch assumption M0–M6 can only document),
  `a_real_codex_binary_resends_the_call_and_output_and_the_session_does_not_fork`,
  `the_next_turn_reflects_the_correction`. Until green,
  `roundhouse-mcp/src/lib.rs` carries the documented-assumption block citing
  `router.rs:164` / `registry.rs:440-444`.

---

## 10. Risk register and open questions

Findings the adversarial reviews raised, with their disposition — kept here so
they are not re-litigated from scratch:

**Resolved in this synthesis** (the fix is in the text above): the
`stored_items` fork under a second item-carrying event kind (reuse
`ItemAppended`); the single-process budget race (reservation from day one,
contract-tested); the validator escalating past the ceiling (all narrowings,
including the judge's, go through `narrow`); paid work in an agent-callable
tool (`fetch_steer` is a pure read); the unauthenticated native/metrics
surfaces under guessable namespaced ids (all routers gated, namespace-checked);
the decision/realization crash window (atomic multi-event commit for steered
and halted turns); the generations-map cross-tenant fork (re-keyed); credential
resolution ordered after routing (moved before candidate assembly); the
admin/turn key contradiction around `set_policy` (overlays are session-scoped
narrowings, never membership state).

**Open, tracked, and honest:**

1. **Does a real Codex resend our synthetic call verbatim?** The parse and
   dispatch paths are verified in source; the history-buffer resend is not
   verifiable without `codex-core`. M0 pins the serde shape; M9 is the only
   closure. Until then it is a documented assumption with citations, per the
   house rule about what a reading-confirmed claim is worth.
   *[2026-08-21: closed by M9 against codex-cli 0.146.0 — see the M9
   addendum. Verbatim for `arguments`; structural for the rest.]*
2. **Steered-turn usage on the wire.** Reporting the judge's usage keeps
   totals honest but could interact with Codex's own context bookkeeping; the
   fallback (`Usage::default()` + dashboard-only line) is specified. Decide on
   M9 evidence.
   *[2026-08-21: decided — neither. The wire reports the steered turn's
   context contribution; the ledger keeps booking the judge. See the M9
   addendum for the `last_token_usage` mechanism that ruled out both.]*
3. **Monthly windows vs the lifetime fold.** Enforced on the ledger in v1;
   `measured_usd` cannot window until the fold gains event-time buckets
   (`fold.rs:83-90` names the constraint). The reconciliation view labels the
   difference until then.
4. **Multi-node `measured_usd`.** Each node folds what it served;
   `committed_usd` is global. Per-node reporting or a fold-merge endpoint —
   decide before multi-node, alongside the README's existing per-process
   metrics caveat.
5. **Judge prompt-injection.** The brief renders transcript content compacted,
   hashed, and line-prefixed as quotation — every line, so nothing from the
   transcript can begin a line of the brief and forge one of its sections; the
   verdict is structured with no action field; directives carry roundhouse's
   vocabulary only, never the judge's prose; and interventions are capped. But
   adversarial content in the transcript influencing `on_track` is not
   *solved*, only bounded: a well-written instruction inside a quotation is
   still an instruction the judge can read. The Shadow arm is also the
   measurement instrument here.
6. **Does the Intervention Paradox reproduce for coding agents?** Unknown and
   deployment-specific by its own mechanism. Hence: default off, Shadow first,
   arms in the log, and the default flips only on arm evidence.
7. **Full transcript vs compacted brief for the judge** (cost vs judgment vs
   keeping the `#validate` prefix warm): a per-membership policy field; no
   data to pick a default until Shadow runs.
8. **Deferred with names attached**: admin audit trail; key rotation without a
   service gap; per-key rate limiting (every ceiling today is dollar-shaped —
   a local-only principal has no volume ceiling); BYOK liveness check at
   attach time; budget-warning push (elicitation is the eventual mechanism —
   it is real on both clients and correctly targets the human);
   `request_replan_from(seq)` as a rollback-shaped steer variant (the
   best-evidenced repair in the literature is state rollback, and roundhouse
   can rewind its log but not the agent's working tree — say so rather than
   shipping half a rollback); Claude Code channels via a stdio shim (strictly
   better than a synthetic call where available; stdio-only research preview
   behind an allowlist today); MRTR when Codex enables the 2026-07-28 mode.

---

## 11. External sources

Code claims: this tree, and the pinned Codex checkout at rev `6344a655…`
(`crates/roundhouse-server/Cargo.toml:59-62`). Research claims, best sources:

- MCP spec 2025-06-18 (transports, authorization, elicitation) and the
  2026-07-28 changelog / release posts — modelcontextprotocol GitHub + blog.
- Claude Code MCP + channels docs — code.claude.com; sampling gap:
  anthropics/claude-code issue #1785.
- Gateway survey: LiteLLM docs + issues #28750/#5345 (budgets, JWT→key
  mapping, budget fallbacks), Anthropic workspaces (platform.claude.com,
  fetched directly), OpenAI project/admin key docs, OpenRouter provisioning
  keys, Portkey/Kong vault patterns. Codex auth guidance (API key for
  programmatic use) — developers.openai.com/codex/auth.
- Steering literature: Intervention Paradox (arXiv 2602.03338), StepShield
  (2601.22136), Real-Time Detection & Repair (2608.02464), Steer-Don't-Solve
  (2606.21811), AEGIS (2606.06660), AgentTether (2607.06273), Signals
  (2604.00356), Strained Coherence (2606.07889), Coherence Collapse
  (2603.24631), Conformal Cascade (2607.25018), "Is Escalation Worth It?"
  (2605.06350), FrugalGPT, RouteLLM, speculative cascades (Google Research);
  Guardrails AI `OnFailAction`, NeMo Guardrails, LangGraph interrupts, AG2
  nested chats — framework docs.

Several paper claims were read from abstracts/summaries because the research
environment could not fetch full texts; they are used here for design shape,
not for their point estimates, and the arm instrumentation exists precisely so
roundhouse measures its own numbers rather than importing anyone else's.

## Addendum (2026-08-20): M8 rulings — the admin plane as built

Recorded at M8 implementation time. Where this addendum and the sections
above disagree, the addendum wins.

**Where the directory landed, and why not where §8 said.** §8's table put
`ControlDirectory` in core with a Redis impl in `roundhouse-store-redis`.
M1–M7 practice already diverged (KeyScope sits beside the resolver in the
server crate), and `control/mod.rs`'s own placement note — "a key record
... arrives with the admin plane, and it will arrive next to the resolver
too, not here" — is the later, argued ruling. M8 honors it:
`ControlDirectory`, its records, and its memory-backed store live in
`roundhouse-server::control_config::directory`. The store seam is
`load / commit(expected_version, records) / version` rather than §8's
implied mutation-apply, because validation must run between read and
write — the compile step judges the merged view, so the store cannot be
the thing that applies a mutation. The Redis impl is deferred with its
unlock condition in the module doc: when durable admin state or
multi-node arrives, either the records move to core (with a dated
amendment of the placement note) or the impl lands in the server crate
via its own redis handle — decided then. (`ControlStore` was not
available as a name: `roundhouse_mcp::ControlStore` holds the MCP overlay
maps, whose own durability remains deferred to the same unlock.)

**Configuration before CRUD, resolved.** The two paths cannot disagree
because admin-created entities are expressed in the same config
vocabulary the file uses and compiled by the same
`ControlPlaneConfig::validate` — "writing the same state the file
expresses", literally. Every entity carries provenance
(`Config` | `Admin`); one owner per entity; mutating a Config-owned
entity is refused `409 config_owned` naming `ROUNDHOUSE_CONTROL_PLANE`;
creates colliding with a Config identity are refused the same way, which
also means a key cannot be minted under a config-declared membership —
the file owns that edge. Every mutation re-runs the boot cross-checks
(now one callable list, `CrossChecks::refuse`, moved verbatim out of
`main.rs`) against catalog and fleet, so a runtime-minted key is exactly
as validated as a boot-loaded one. One honest consequence: the
admits-nothing check walks turn keys, so an unservable policy is accepted
at project-create and refused at the first mint — nothing can be harmed
while no key exists, and the refusal still lands before any admission.
Admin-created state is process-lifetime until the durable store lands —
the file stays the only restart-stable truth, which is why bootstrap is
file-only: in Open mode the admin surface refuses everything with
`403 admin_requires_control_plane`, and the first admin key hash always
enters through the file. API lockout is impossible on two branches:
config-owned admin keys are unrevocable via the API, and a file with no
admin keys never had an authenticated admin to lock out.

**Revocation is a snapshot bound, not a row delete.** Admission resolves
against an immutable compiled plane snapshot served by
`ControlDirectory::plane(now_ms)`; writes recompile and swap immediately
(same node), and a stale view refreshes once `admission_cache_ttl_ms`
(config field, default 30s; 0 means every call re-checks) has elapsed and
the store version moved. Revoked keys and archived projects compile to
named refusals — `401 revoked_key`, `403 project_archived` — in the one
auth table, consulted ahead of `unknown_key`, so an operator can tell
theft from typo and history from absence. Archive is terminal in v1; the
un-archive that would reopen the question of what its keys resume meaning
is deferred with the audit trail.

**The surfaces take a `PlaneSource`, not a plane.** The ripple that makes
every router re-resolve per request shipped as a one-method trait
(`plane(now_ms) -> Arc<ControlPlane>`) with the five composition seams
generic over it. `ControlDirectory` is the live impl and the only one a
production build can name; `ControlPlane` serves as its own fixed source
strictly behind the `test-support` feature, so pre-M8 test fixtures stay
valid while a bare plane at a production call site is a missing-impl
compile error rather than a silent loss of revocation propagation. That
fence — not convenience — is why the shim shape was rejected and the
trait shape ruled in.

**The reconciliation view's arithmetic, sharpened by a proven hazard.**
During M8's understanding pass a probe proved that a `balance()` read
carrying the wrong `BudgetWindow` permanently destroys committed spend
(the window roll in `settle_time` zeroes the account on a Total→Monthly
mismatch; Monthly→Total silently reinterprets a month as a lifetime).
Two rulings follow. The view calls `balance` only with the membership's
`BudgetTerms` taken verbatim from the compiled admission — never
constructed fresh. And `PATCH` of a project's budget window is refused
(`400 window_change_unsupported`) naming that mechanism; a window
migration is a deliberate future design, not a field edit.
The view itself: `committed_usd`, `held_usd`, `measured_usd`,
`seat_tokens`, `drift_usd` — separate fields, no total anywhere,
`drift_usd = committed − measured`, negative on a lost settle and never
clamped. Every dollar column carries a basis stamp, and there are four
bases, not two: `ledger` (windowed committed), `unenforced` (a budgetless
membership the engine never grants against — nulls, never `0.0`),
`no_keys` (a membership with no admission at all, which is not the same
claim), and `archived` (committed null while measured stays real, because
spend history outliving the project is the reason archiving is not
deletion). `seat_tokens` is the dollar-free column that keeps a
pass-through project from reading as under-billed; structural drift is
disclosed rather than merged — under `ProjectPaidOnly`, user-paid spend
appears in measured and never in committed, a label rather than a bug
until the fold learns to read `DecisionRecord.payer` (deferred, named).
Two more honesty notes the implementation surfaced: measured dollars are
priced from the live rate card at snapshot time, so a catalog whose
prices disagree with what turns were settled at reads as drift — a
configuration mismatch wearing drift's clothes; and the fold is
process-local, so a restart legitimately sends drift positive until the
log is re-folded. Both are why the columns carry their basis instead of
asking the reader to trust a bare number.

**Credential CRUD did not ship, loudly.** `POST /v1/admin/credentials`
refuses OAuth-shaped input with `400 oauth_credentials_unsupported` (the
plan's own refusal, now HTTP-reachable) and everything else with
`501 credential_crud_not_available` naming the config-file mechanism that
remains authoritative. The sealed store (XChaCha20-Poly1305 under
`ROUNDHOUSE_CONTROL_KEY`) stays deferred; its unlock is the durable
directory store above.

**Still deferred, by name** (unchanged from §10.8, restated so M8 is not
read as having quietly delivered them): admin audit trail (admin writes
are unattributed — `KeyScope::Admin` deliberately carries no identity),
key rotation, per-key rate limiting, pagination, rate-card editing,
MCP-overlay durability, un-archive.

**Thermo-nuclear review outcomes.** A dedicated adversarial pass over the
admin plane above found five defects worth fixing before this milestone
closes, all fixed here rather than deferred, plus deferrals sharpened
where the review found the existing note incomplete.

A revoked-only membership — every key it ever had now revoked, with real
spend already in the ledger — was reporting `no_keys`, the same basis as
a membership that never held a key at all: the compiled plane forgets a
revoked hash exactly the way it forgets one never minted, and the
reconciliation view was reading that forgetting as "nothing to report"
instead of "nothing left to enforce." Fixed with a fourth basis,
`revoked_keys`, that carries the real figure rather than blanking it, and
— the harder half — real terms behind it rather than a second hand-built
`BudgetTerms`. The module's verbatim-terms rule (above) says a balance
may only be read under the admission's own terms because a wrong
`BudgetWindow` permanently destroys committed spend; a revoked-only
membership has no admission left to take terms from, so
`ControlDirectory::membership_terms` now derives them by calling the
compiler's own project-budget-plus-allocation pairing directly, off the
directory's own rows — the same bytes a live admission would have
carried, from the same code, never a parallel construction. `no_keys`
now means only "never held a key"; `revoked_keys` means "held one, spent
against it, holds none now" — two different operator questions that one
basis was answering with one wrong-shaped `null`.

A settle read two live facts off the *current* `Admission` — whether the
turn was budgeted at all, and which `BudgetCounts` mode was in force —
rather than off the log, contradicting this module's own stated rule
that a settle is priced from the log alone. Both are now recorded on
`DecisionRecord` at decision time, in one field
(`budget_draw: Option<BudgetCounts>`, `#[serde(default)]`, same treatment
as `Usage::reasoning_tokens`) rather than two, because a flag beside a
basis would allow a state that is a lie ("not budgeted, and drawn on the
project-paid-only basis" describes nothing) — the same argument
`BudgetState` already made once. `Engine::settle` and `repair_settle`
gate on the log's own `budget_draw`, never on `admission.budget`; a
`None → Some` budget `PATCH` still only governs turns decided after it
lands, and an old log with no field on it defaults toward *not*
recharging — the one direction a default is allowed to fail in here.
Replay of an existing log is byte-identical: the field is a serde
default, not a rewrite.

`deny_unknown_fields` now covers `ProjectEntry` and `UserEntry`, closing
the one pair of entry shapes the boundary had missed — a misspelled
`credential` for `credentials`, or any other stray top-level key on an
admin-created project or user, was compiling and silently discarding the
field rather than refusing to load, with no route anywhere that echoes
`policy`, `validate`, `credentials` or a limit back to an operator who
might otherwise notice. The module doc now says outright that the file
shares the API's strictness; a stray key in `ROUNDHOUSE_CONTROL_PLANE` is
a boot refusal by the same design.

Two gaps the review named but that stay open past this milestone, each
with its shape recorded rather than silently missing: the admin API still
has no route that reads a project's `policy`, `validate` or `credentials`
back — the review's own reproduction for the `deny_unknown_fields` gap
above is what surfaced this, since the only way to *notice* a dropped
field is to ask for it back and be refused the asking. And the directory
store is still `MemoryDirectoryStore` alone (per this addendum's own
placement ruling above), which the review sharpened into a boot-time
signal rather than a silent gap: when `ROUNDHOUSE_REDIS_URL` durable-backs
sessions and spend but the directory stays in memory, boot now warns
loudly, naming exactly what an operator is trusting to survive a restart
that will not (admin-created projects, users and keys; archive
tombstones) and what losing it risks (a recreated project id silently
inheriting an archived tenant's committed spend from the ledger that did
survive).

Two more findings were ruled real but deliberately left as recorded
deferrals rather than fixes, because closing either now would be
overreach relative to what this milestone needs: `admin_auth_layer`
resolves the plane once to authorize a request, and every handler behind
it independently resolves the directory again for its own work, so a
request can in principle be authorized against one snapshot and executed
against a version that moved microseconds later — accepted, with the
real fix (resolve once in the layer, thread it via request extensions)
named at the layer's own doc for whoever revisits it. And
`GET /v1/admin/projects/{p}/budget` issues one ledger balance read per
budgeted member — N *mutating* round-trips, since a balance read rolls a
lapsed window over — with no pagination bounding N by design; every
figure it produces is still correct, so this is a documented cost rather
than a defect, and a single project-scoped ledger read is deferred by
name (it needs a new `SpendLedger` method, coverage in that trait's
contract suite, and the matching Redis-Lua change — real work, not a
one-line hoist).

## Addendum (2026-08-21): M9 rulings — the real binary, and what it disproved

Recorded at M9 implementation time, after the thermo-nuclear review. Where
this addendum and the sections above disagree, the addendum wins. The
evidence it rests on is `research/codex-0.146.0-vs-pin-vigilance.md` (the
binary diffed against the pin and the §3 ruling rev) and the gated suite
`crates/roundhouse-server/tests/codex_e2e.rs`, which drives `codex-cli
0.146.0` against a bound port. Every claim below that names a codex path
names it at `e363b08`, the tree that binary was built from.

**The ladder closes, and §10 open item 1 with it.** The three M9 tests in
§9 are green against a real binary: codex executes our synthetic
`fetch_steer` call over the real `/mcp` service (rmcp 3.1.3 server
answering codex's rmcp 1.8.0 client, the one pairing no source reading
could settle), appends the output, resends the call and its output, and
the session does not fork. The `arguments` string comes back byte-for-byte
— the capture carried Python's `", "` spacing through a real client, which
no re-serialization preserves, so the M4 invariant at `wire.rs:298-312` is
now a measured fact rather than a cited one. The resend is structural, not
byte-identical: codex re-serializes in its own field order and drops any
item `id` without an interior underscore (`core/src/client.rs:927-933`),
which is why `fc_<response_id>` survives and why the suite asserts on
parsed fields with `arguments` as the one byte-exact comparison. The
documented-assumption block in `roundhouse-mcp/src/lib.rs` is retired into
a verified block that restates both facts against `e363b08` and keeps the
pin-era citations as history, the way §3's own history entry does.

**The binary is older than the pin, and the §3 negative does not hold for
it.** `codex --version` on the test box is 0.146.0 (`e363b08`, 2026-07-28);
the Cargo pin is `6344a65` (2026-08-13) and §3's `requires_openai_auth`
ruling was read from `3b45c29` (2026-08-19). Neither the binary nor the pin
is an ancestor of the other. The guard §3 leans on — "leave the flag unset
and codex attaches nothing" (`auth.rs:205-207` @ `3b45c29`) — **does not
exist at either**: `resolve_provider_auth` at `e363b08` is
`model-provider/src/auth.rs:179-196`, and it runs `env_key` /
`experimental_bearer_token` first and then attaches whatever ambient
`CodexAuth` sits in `CODEX_HOME`, flag or no flag. This settles the question
§3's history entry left open: the guard post-dates `6344a65`. The original
`6344a65`-era ruling was correct for the pin and for this binary; the
2026-08-19 refutation is correct only for newer revisions. Two consequences
are now rules: **never emit a `requires_openai_auth = false` stanza without
`env_key`** — against this binary that is not "send nothing", it is "send
whatever you are logged in as" — and the harness runs every child with a
cleared environment and a credential-free `CODEX_HOME`, with a test on the
built environment rather than on the wire, because the wire cannot see a
credential that was available but never consulted.

**The catalog pin belongs in both stanzas.** §3 put `model_catalog_json`
only under the pass-through stanza on the reasoning that only that route
fetches `GET {base_url}/models`. At `e363b08` the fetch is gated on the
*ambient auth mode* in `CODEX_HOME` (`models-manager/src/manager.rs:413-417`,
`models_endpoint.rs:67-72`), never on the flag, so a BYOK stanza on a box
holding a ChatGPT `auth.json` fetches too. The generator emits the pin for
both auth kinds; a pinned catalog also swaps in `StaticModelsManager`,
which has no network path at all, and that is what makes the suite
hermetic by construction. The catalog entry is written against `e363b08`'s
`ModelInfo` (twelve required keys), pins `shell_type = "shell_command"`,
an explicit `context_window`, and `supports_search_tool = false` — the
last because `canonical_item` refuses `tool_search_call` and copying an
upstream catalog entry would reopen that 422 path with no code change.

**The reference config is a library function, not a fixture.**
`roundhouse_server::codex_launch` is the Direct-topology config from the
round-2 launch-surface ruling: one env var feeds both `env_key` and the
`X-Roundhouse-Key` header (derived from `TURN_KEY_HEADER`, not retyped);
`requires_openai_auth` is set by the route's auth kind, mirroring
Switchyard's `caller_auth_kind`; the MCP stanza's table key is literally
`roundhouse` because codex builds the namespace as `mcp__{key}` and the
dialect emits `mcp__roundhouse`; the mount path and the API prefix are the
router's own constants. The pass-through kind carries a precondition §3 did
not state: a completed `codex login` in `CODEX_HOME`. Without it the flag
changes nothing, the request arrives with no `Authorization` at all, and
roundhouse degrades to local-only rather than refusing — the silent
failure, now named in the stanza's own comment. The generator refuses the
three input shapes whose output would be silently wrong (a relative catalog
path, a base URL without the API prefix, a trailing slash). What it does
not yet have is an operator entry point — no CLI subcommand or admin route
produces it — and that is **deferred by name**: whether it is a `roundhouse`
subcommand or an admin-API read beside key minting is a surface design
question, not this milestone's.

**Codex cancels an unannotated tool call, and the steer becomes a
cancellation notice.** Under `codex exec`, `approval_policy` is forced to
`never` (`exec/src/lib.rs:427`), and `requires_mcp_tool_approval` treats a
tool with no MCP annotations as destructive and open-world
(`core/src/mcp_tool_call.rs` @ `e363b08`), so the first real-binary steer
was answered with `"user cancelled MCP tool call"` — and roundhouse's log
recorded a fulfilled steer whose content the agent never saw. Three
rulings. The generated MCP stanza carries
`default_tools_approval_mode = "approve"` as the Direct topology's
defense-in-depth; scoping it per tool to `fetch_steer` was proposed and
refused, because under the forced `never` a writer tool with
`read_only_hint: false` still needs the grant and the overlays would
silently stop working. The real fix is truthful annotations on every
descriptor — `read_only_hint` on the three reads, `destructive_hint` and
`open_world_hint` false on all eight (overlays only narrow; the surface
reaches nothing but roundhouse's own plane) — so a client we never handed a
config to auto-runs the reads under its default mode. And the fold no
longer marks a steer fulfilled on a cancellation or codex's synthesized
`"aborted"` filler: the open steer closes (bookkeeping), the turn stays
eligible for validation, and the existing intervention ladder bounds what
follows.

**Half of the default signals were dead against a real client.** Codex
wraps every tool result — `Wall time: …\nOutput:\n…` for MCP, a
`Chunk ID` / `Process exited` block for exec — before it becomes a
`function_call_output`. `reads_as_failure` anchored on the first bytes
could never match, and `NoProgressRepeat` hashed the jittering wall time,
so neither `ToolFailureStreak` nor `NoProgressRepeat` could fire on a real
transcript. The wrapper is stripped at one seam in `exchange.rs` before
either signal reads an output; the stored item stays the client's verbatim
bytes, because prefix admission depends on them. Non-codex outputs hash
identically before and after, so existing logs fold the same.

**§10 open item 2, decided on evidence.** The steered turn reported the
judge's side-call usage verbatim — 1100/300/47 against a ~40 KB body — and
codex folded it into its session total without complaint. That
reassurance measured the wrong number. Codex's compaction gate and its
`get_context_remaining` tool read `last_token_usage`, which is *replaced*
on every response (`protocol/src/protocol.rs:2108-2111`,
`core/src/context_manager/history.rs:297-314`,
`core/src/session/context_window.rs:27-50`), so on the steered turn the
client believed its live context was ~1147 tokens when the history it was
about to resend was ~5700 — a five-fold under-report, one turn wide, on
exactly the turn it has just been told to change approach.
`Usage::default()` (the pre-specified fallback) is strictly worse: it
collapses the same number to the trailing-items estimate. **Ruling: the
wire and the ledger stop sharing one number.** On a steered turn
`response.completed.usage` reports the turn's context contribution — the
admitted request's input as the engine's tokenizer estimates it, and the
emitted call's size as output — while the log books exactly what it booked
before: the judge's usage on the turn record, the side call on its own
model row, so the dashboard's pricing is unchanged. The evidence block the
suite prints (`M9-USAGE-EVIDENCE`) now carries the ratio between the
steered turn's reported input and the next request's real input so the
gap is visible in the output rather than reconstructed from four blocks.

**Other rulings, briefly.** A pass-through request whose `env_http_headers`
codex dropped silently (unset or blank variable — `build_header_map` never
errors, unlike `env_key`) arrives with only the seat's `Authorization`; it
is now refused `missing_key` naming the dedicated header, not
`malformed_key` naming a credential the operator never meant as a key.
Codex sends `_meta.threadId` and the turn-metadata session id on every MCP
`tools/call`; the surface still ignores it — the `init_session` trick stays
the client-agnostic path and reading `_meta` is a codex-native shortcut
deferred to a plan of its own. `canonical_item` refuses eight of the twelve
item types a 0.146.0 client can resend; the suite exercises the only
conversation shape in which none occurs, and the live test that names them
is the tripwire for the day one does.

**What the harness proves and what it does not.** Hermetic: loopback only,
a static catalog, no login, a cleared environment. The `ForwardedOpenAiLogin`
stanza is driven with a fake `auth.json` — enough to see the seat's bearer
and our key ride one request — but no real ChatGPT login has been forwarded
through this code. The `auto_compact_token_limit` is `null`, so the
compaction path itself is never reached; the §10.2 ruling rests on the
source and on the measured gap, not on an observed compaction. Revocation
between runs is tested. `CODEX_HOME` lives under `target/` as a precaution,
not a measured necessity: the temp-dir symlink refusal the dive predicted
did not reproduce and cannot be observed by a harness that never dispatches
a sandboxed shell command. And `codex --version` is printed and warned on,
not asserted: a suite that silently passes against 0.146.0 and silently
changes meaning against the next release is the failure CLAUDE.md's
vigilance rule exists to prevent, and the Cargo pin stays at `6344a65`
until its own diff-and-map pass.

## Addendum (2026-09-03): the deferred Redis `DirectoryStore`, decided

M8 deferred the Redis `DirectoryStore` with its placement "decided then"
— either the records move to core with a dated amendment of
`control/mod.rs`'s placement note, or the implementation lands in this
crate over its own Redis handle. D2 (`PLAN-frontier-selection.md`, R16–R19,
evidence in `research/roundhouse-admin-directory-1b85d64.md`) took
neither: the *contract* moves to core as a versioned opaque document
(`load` / `commit(expected_version, bytes)` / `version`, the shape this
module's trait already has over whole records), `roundhouse-store-redis`
implements it as a fifth key family under one key with a compare-and-set,
and `ControlDirectory`, its records, `KeyScope` and the compiler stay here
beside the resolver — the placement note stays true of the record and
gains a dated line saying its bytes did not need to stay. The seam lands
first (M16.0: async trait, compile outside the write guard), the store
second (M16.1), and with it the boot warning and the flag that gates it
are deleted rather than moved, because no memory-backed Redis branch
remains. The "still deferred" list above is unchanged by this: audit
trail, key rotation, per-key rate limiting, pagination, rate-card editing
and un-archive stay deferred by name; MCP-overlay durability and the
sealed credential store gain a contract they can ride on and keep their
own questions.

## Addendum (2026-09-04): D3 — what the durable directory unlocked

M8 deferred three things "to the same unlock" as a durable directory —
the MCP overlay maps' durability, the sealed credential store, and, with
the audit trail, un-archive — and M16 landed the unlock. D3 rules on them,
on the tree at `1d016f2`, from two evidence documents, every claim pinned
and independently re-derived:

- `research/mcp-overlay-and-sealed-credentials-1d016f2.md` — what the
  control store holds and what the engine spends, what a restart and a
  second node lose, which durable shape each map is, what sealing needs and
  where a sealed blob could ride, and what Relay does with credentials.
- `research/unarchive-admin-identity-and-node-status-1d016f2.md` — what
  archiving does, why un-archive was deferred, what its keys and windows
  would resume as, what an attributed admin write needs, and what audit
  material exists.

**R-O1 — the overlay is durable, correlation-shaped, and re-derived where
it is spent.** The engine spends exactly one thing from the control store
per turn: the overlay, whose loss on a restart or a node hop widens the
turn back to the key's ceiling — never past it, and visibly in the digest —
but silently, and against a promise the surface makes out loud: `prefer`
and `status` answer with the digest the next decision will carry, and
M14.1 made session identity deployment-wide while the state keyed by it
stayed node-local, so a node hop now falsifies that promise where before
it would more often have refused. The overlay becomes a key family of its
own in the store crate, keyed per session with the one-day staleness bound
the control store already chose from consequence, and the memory table it
already has. It is not a straight port, and the evidence names both
halves: the narrowing's patterns are stored as the strings the agent sent
and re-parsed, never re-resolved against the reader's catalog (a narrowing
that grew to cover a model added later is a widening with an agent-authored
trigger); and the write-time guarantee that a narrowing leaves something
routable held only because the catalog outlived every overlay, so the
engine's admission re-derives it — a stored narrowing that admits nothing
under this node's catalog is set aside with a typed reason, never
silently, never widened. The engine's read goes async as the directory's
did.

**R-O2 — the intent and the outcome move into the session log.** The
intent is read on the turn it was declared and never spent; the outcome is
written and read by nothing in production. Both are agent-authored text
with the shape M10.0 gave the steer when it moved the steer into the log to
kill a node-local second source of truth. They become control items in the
log — additive variants, the M11.1 discipline — durable and replayable for
free, visible to the validator from the log it already folds, and gone from
the control store. The binding family, which nothing in production resolves
from, stays process-local by ruling and says so; whether a duplicate
binding id is a defect is undecided because nothing reads one.

**R-O3 — a sealed credential is its own document, and a node that cannot
open it does not serve.** The credential *reference* already rides the
directory document; the *material* is what an environment variable per
node cannot distribute. It rides a sibling document family under the
document contract — its own key, its own lineage, its own ceiling — sealed
with an authenticated cipher under `ROUNDHOUSE_CONTROL_KEY`, with the
sealing key's id as a fifth, defaulted axis of the fingerprint so a reader
can name which key a document was sealed under. The first cryptographic
dependency in the tree is a watched addition: pinned, its unlock condition
written beside the pin. A node that cannot open the document refuses the
boot naming the key id, and on refresh keeps the last good plane and
records it, because a plane compiled without a credential admits keys whose
every dispatch will fail — the failure furthest from its cause. What
sealing does not buy is stated plainly: the sealing key has exactly the
distribution problem the provider key had, one secret per node out of
band, and that is the residue this design accepts rather than hides.

**R-U1 — un-archive resumes the identity and not the keys.** Archiving
sets one field and cascades nothing: every key of an archived project keeps
its own row unrevoked and is refused only by a derivation at compile time,
so an un-archive that cleared the field would silently re-admit every key
that was live when the project closed — the question the deferral could
not answer. It is answered conservatively: an un-archive revokes, as part
of the same commit, every turn key that was live at archive time, so
resumption is empty by construction and the operator re-mints; archive
itself stays non-destructive. The record keeps the closed intervals, so a
Monthly budget window that zeroed across the gap is visible rather than
inferred, and the reconciliation view carries the gap; a Total budget
resumes with its lifetime figure, said so. The mechanism is one more
mutation arm and one compare-and-set, now that tombstones survive and a
document has a lineage.

**R-U2 — the admin scope carries an identity, and the audit trail is a
stream.** `KeyScope::Admin` carries no principal because M8 had no key
record to name; the record exists, its id is derived from the hash and
minted for file-declared keys too, and open mode never produces the admin
scope at all, so a required identity on that arm costs the open-mode
default nothing. An actor field on the records would attribute creation
only and leave every patch, archive and revocation unattributed, so the
attribution is a log: an append-shaped family in the store crate, the
session log's shape, one entry per admin mutation carrying the actor's key
id, the mutation, its targets, and the lineage and version the commit
produced — the one field that orders admin writes across nodes. The file
still names an admin key by its hash alone; a label is a file-format
question left where it is.

**Still deferred, by name.** Key rotation, per-key rate limiting,
pagination and rate-card editing are unchanged. Per-key credentials stay a
thing only the file can say until the sealed store lands.

The rungs this opens are recorded in `PLAN-anthropic-messages.md`.
