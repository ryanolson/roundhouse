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

Facts verified at the pinned rev `6344a65` that make this buildable as
configuration, not protocol work:

- Codex attaches its ChatGPT login bearer (and `ChatGPT-Account-ID`) to a
  **custom** `model_providers.*` base_url unconditionally — the auth
  resolution wraps whatever `CodexAuth` is active unless the provider
  declares its own `env_key` (`model-provider/src/auth.rs:186-203`). Leave
  `requires_openai_auth` unset: that keeps the plain bearer path and avoids
  the Agent-Identity bootstrap, which would call `auth.openai.com` directly.
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
- The one real gap: MCP handshake headers are static/env-sourced, so an MCP
  connection cannot carry the codex session id natively. Correlating the MCP
  channel to a conversation uses the init-tool trick: `init_session` returns
  a minted id in its tool *output*, the client appends that output to its
  conversation, and the next turn's resent history carries it into the log —
  the session whose log holds the id is the session that made the call.

The enterprise client config this implies, in full:

```toml
model_provider = "roundhouse"

[model_providers.roundhouse]
name = "Roundhouse"
base_url = "https://roundhouse.internal.example.com/v1"
wire_api = "responses"

[model_providers.roundhouse.env_http_headers]
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
retryable; the side-call's cost, if reported, is still booked. (New exposure
the review named: this seam turns a zero-latency window into a network round
trip. Handled by the same failure vocabulary, tested explicitly.)

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

The directive the agent eventually sees is roundhouse's rendering of the
structured verdict, never the judge's prose passed through: a judge whose free
text reaches the agent verbatim is a judge that can be prompt-injected into
escalating. (The brief itself renders transcript content compactly and
hashed where possible for the same reason — §10 keeps this on the risk
register rather than calling it solved.)

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
  detected capability **and** a zero consecutive-intervention count — injected
  guidance is the weakest, most oscillation-prone action in the literature
  and the protocol-heavy path is the last resort on purpose.
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
no namespace): `canonical_item` already ignores `namespace` and `id` on the
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
2. **Steered-turn usage on the wire.** Reporting the judge's usage keeps
   totals honest but could interact with Codex's own context bookkeeping; the
   fallback (`Usage::default()` + dashboard-only line) is specified. Decide on
   M9 evidence.
3. **Monthly windows vs the lifetime fold.** Enforced on the ledger in v1;
   `measured_usd` cannot window until the fold gains event-time buckets
   (`fold.rs:83-90` names the constraint). The reconciliation view labels the
   difference until then.
4. **Multi-node `measured_usd`.** Each node folds what it served;
   `committed_usd` is global. Per-node reporting or a fold-merge endpoint —
   decide before multi-node, alongside the README's existing per-process
   metrics caveat.
5. **Judge prompt-injection.** The brief renders transcript content compacted
   and hashed, the verdict is structured with no action field, directives are
   roundhouse-rendered, and interventions are capped — but adversarial content
   in the transcript influencing `on_track` is not *solved*, only bounded.
   The Shadow arm is also the measurement instrument here.
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
