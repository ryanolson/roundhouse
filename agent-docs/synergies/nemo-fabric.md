<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# NeMo Fabric ↔ Roundhouse: the synergy ruling

> **Status: direction.** The synthesis of `../research/nemo-fabric-deep-dive.md`
> (NeMo Fabric @ `6d9ebc3`, 2026-09-04; this tree @ `4fa34d0`) into a plan,
> answering the product owner's two questions: does roundhouse re-invent bits
> of NeMo Fabric, and should roundhouse migrate to its Rust library. The deep
> dive is the evidence base — every claim relied on below carries a file:line
> there — and this document is the ruling. It extends `nemo-relay.md` and
> `ecosystem-round-2.md`; where they disagree with this one on Fabric, this
> one wins. The ruling was itself fact-checked against the trees and reviewed
> adversarially against its evidence before it was committed; the edits that
> review demanded are in.

## The two answers, first

**Does roundhouse re-invent bits of NeMo Fabric? One file's worth, on the
launch surface, and it is convergence rather than drift.** The only
roundhouse code that answers Fabric's question — what configuration a coding
harness needs to reach an endpoint — is `codex_launch.rs` and its `skills.rs`:
968 non-test lines against roughly 71,000 lines of crate source. Fabric's
Codex adapter writes the same `model_providers` and `mcp_servers` vocabulary
through a different injection point (a dict handed to the Codex SDK's
`thread_start`, never a `config.toml`). Of the fourteen key/value lines
roundhouse's stanza writes, seven have a Fabric equivalent (three as the same
bytes — `base_url`, `wire_api`, `env_key` — and `model`, `model_provider`,
the provider `name` and the MCP `url` by another route); the other seven
(`model_catalog_json`, `requires_openai_auth`, `env_http_headers`,
`supports_websockets`, `bearer_token_env_var`, `default_tools_approval_mode`,
`use_agent_identity`) have no Fabric field and reach the client only through
the untyped `config_overrides` escape hatch; and one variant — the forwarded
ChatGPT-login stanza — Fabric cannot express at all. Four of those seven
exist because their absence fails silently in the dangerous direction:
`requires_openai_auth` beside `env_key` (an ambient login attached to our
`base_url`), `model_catalog_json` (invented model metadata), `default_tools_approval_mode`
(a cancelled steer), and `bearer_token_env_var` (the secret in a file).
Everything roundhouse actually is has no Fabric vocabulary: no price, no
quality prior, no cache model, no TTFT, no budget it enforces (only an opaque
`max_budget_usd` passed through to a harness), no session log, no lease, no
routing, no steer, no judge, no cached or reasoning tokens, no
measured-versus-estimated distinction. The nouns that look shared are false
friends (`route` is execution ownership, `candidate` is a duplicate
descriptor, `ModelConfig` is the endpoint a harness calls — which is what
roundhouse *emits*, not what it holds). The "bootstrap binary" the owner had
in mind is not an analogue: `main.rs` composes the server and runs no agent;
the use-case driver is an HTTP load generator with roundhouse as the server;
the vault launcher launches the server. And the quickstart recognized is the
Hermes one — its `max_turns=1` fails planning against Codex outright.

**Should roundhouse migrate to `nemo-fabric-core`? No — not as a runtime
dependency, and not as a replacement for the launch surface.** The crate is a
config compiler plus a process supervisor for Python and Node adapter hosts.
No harness runs from Rust alone; the multi-turn runtime and the streaming
consumer exist only in the Python SDK. As a dependency it buys one serde type
(`validate_config` is private, plan and doctor need adapter descriptors on
disk through a compile-time-path workaround, the lifecycle spawns a Python
interpreter, the crate is blocking and keeps live children in a process-global
static) for 20 new crates from the `jsonschema` subtree and two version
splits (`strum` 0.28 beside our 0.27.2, `bit-vec` 0.8 beside our 0.9.1), on a
project with no published `0.3.0`, no API-stability promise, and a contract
bump plus two renames in the last month. Replacing `codex_launch` would give
up the catalog pin, the `/v1` refusal, the secret-never-in-the-file property
for MCP headers, the forwarded-login stanza, and the identity of the binary
the real-binary suite verifies — the app-server Fabric pins is a different
artefact from the `codex-cli 0.146.0` the suite runs, separately versioned,
and not installable from `PATH` by design.

**What changed by looking.** The one fact that could have killed every
Fabric-adjacent shape did not. The app-server Fabric pins
(`openai-codex==0.144.4`), driven through the SDK with the exact provider
dict Fabric's `custom_model_provider_config` builds and a fresh `CODEX_HOME`
the way Fabric's adapter arranges one, sends `prompt_cache_key` on every
turn, keeps it stable, and resends history as a byte-identical prefix —
measured, not read (deep dive §6). The adapter itself, its system
instruction, sandbox, skill roots and `thread.turn` options were not
exercised, and the peer was a mock Responses server, not roundhouse. So the
Fabric-driven shape **meets this surface's admission preconditions on the
wire**; an end-to-end run against a real roundhouse is still unmeasured. The
same runs found the property that keeps the shape safe: with a real OpenAI
slug the app-server adds a `tool_search` tool *definition* to every request,
on a custom provider as much as on the built-in one. Roundhouse ignores
`tools`, but a later turn can then resend a `tool_search_call` /
`tool_search_output` pair, and that item type is one of the eleven this
surface 422s — taking the whole turn down. Roundhouse chooses
`roundhouse-local` for the operator; Fabric passes whatever they typed.

## The ruling

**Independence, with Fabric-driven as a third supported topology, Codex-only
today.** Roundhouse takes no `nemo-fabric-core` dependency at run time and
keeps generating its own Codex config. Fabric is recognized as the
harness-launching neighbor Relay's ruling assigned that seam to — the same
box, a second occupant — and roundhouse buys interoperability the way it did
with Relay: at the format layer, by emitting what Fabric reads, and by
contributing back what Fabric lacks. The division of labor gains one clause:

> **Fabric launches the harness, Relay instruments it, roundhouse owns the
> turn, Dynamo owns the metal.**

That clause amends the seam assignment `nemo-relay.md` and `CLAUDE.md`'s
product paragraph currently state as "Relay owns the harness". Relay still
owns the *instrumentation* of a harness; Fabric owns *launching* one.
Rewording `CLAUDE.md` is the owner's call and is listed under costs.

### Why not the alternatives

- **Migrating the launcher to Fabric (V4) inverts four properties this tree
  wrote down as load-bearing.** The catalog pin (`model_catalog_json`, no
  Fabric field), the `/v1` suffix refusal (Fabric only strips a trailing
  slash), the secret never in the file (Fabric's default MCP path expands the
  variable's *value* into the JSON-RPC payload and blanks it in the child
  env), and the forwarded ChatGPT login (needs `requires_openai_auth = true`
  with *no* `env_key`; Fabric always writes `env_key` and its overrides cannot
  delete a key). It also replaces the operator's `codex` with an SDK-pinned
  app-server, which is not "the agent's own stack is not modified".
- **`nemo-fabric-core` as a runtime dependency (V5) is one serde type for
  twenty crates.** The dependency rule admits typed-contract crates by weight
  and role; this would be the first execution crate and the sixth watched
  dependency, with the vigilance obligations that carries, for a struct
  roundhouse can write in `serde_json` and check against Fabric's published
  schema in a test.
- **Fabric as roundhouse's harness driver in the real-binary suite** would
  test a binary nobody can `--version` and that Fabric moves on its own
  schedule. Honest accounting of the suite: twelve tests sit under the
  `e2e-codex` feature, nine spawn the binary, and six of those nine assert
  claims any driver that continues a conversation and records request bodies
  could satisfy. The ownership case rests on the other three — `exec resume
  --last`, `$CODEX_HOME/skills`, the crafted `auth.json` for the forwarded
  login — plus the forwarded stanza Fabric cannot express, the four
  silent-failure lines it has no field for, and version identity against a
  binary the operator installs. That is enough to keep the launcher; it is not
  "cannot be delegated", and the documents that said so are corrected below.
- **Roundhouse as a Fabric Adapter Target** is a category error the Remote
  Agent adapter proves by what it POSTs: a text transcript with no
  `prompt_cache_key`, refused on the first request. Roundhouse is not an agent.

## The plan

Five items, none a milestone of its own. Each lands inside the work it serves.

### F1 — Correct the record (now, with this ruling)

- The launch-surface dedup's sentence in `ecosystem-round-2.md` and in
  `codex_launch.rs`'s module doc names a test that M10.0 deleted. The
  ownership case is restated above. The addendum on `ecosystem-round-2.md`
  records it; the source comment is a follow-up.
- `DEFAULT_MODEL_SLUG`'s doc comment describes the trap as `use_responses_lite`
  putting `AdditionalTools` into `input`. What was measured at 0.144.4 is a
  `tool_search` definition in `tools`, keyed on the slug, which becomes a
  refused item on the next turn. Same 422, different mechanism; the comment
  gains the measured one as a second sentence. Follow-up, with the one above.
- **Fabric-driven is the third topology**, beside Direct and Chained:
  Codex-SDK app-server → Fabric → roundhouse → {Dynamo | frontier}. Supported
  once a deployment wants it, documented with the requirement list in F2,
  never required to build or test roundhouse. Three limits are part of the
  documentation, not footnotes to it:
  1. **Codex only.** Fabric's Claude adapter needs an Anthropic Messages
     endpoint and roundhouse serves none; Claude Code through Fabric 404s on
     its first turn. The Messages-surface ruling stands.
  2. **A weaker transparency claim, stated.** `CLAUDE.md`'s "the agent's own
     stack is not modified" reads, for this topology, as "no roundhouse code
     inside the harness": the operator runs the app-server Fabric pins, not
     the `codex` they had.
  3. **When `telemetry.providers.relay` is on it is Chained assembled by
     Fabric**, and the four chain guards in `nemo-relay.md` apply unchanged.
     One is now evidenced rather than assumed: Relay's gateway reconstitutes
     the `/v1` Fabric strips, for every configured-base and path combination
     on the API-key chain (deep dive §7 — read at Relay main, the 0.9 line;
     the `>=0.7.2,<0.8` line Fabric's Harbor image requires was not read).
- **The slug rule is stated for Fabric users.** `models.default.model` must
  be a slug the Codex catalog does not recognize. `roundhouse-local` is the
  documented value; a real OpenAI slug adds `tool_search` on every turn.

### F2 — Emit a `FabricConfig` from the deferred operator entry point (with that entry point, not before)

The operator entry point that produces the generated Codex config is deferred
by name (`PLAN-agentic-control-plane.md`, M9 addendum). When it lands, it
emits a third artifact beside `config.toml` and the catalog: a `FabricConfig`
JSON, `schema_version: "fabric.agent/v1alpha1"`, built from the same four
constants (`API_PREFIX`, `TURN_KEY_HEADER`, `MCP_MOUNT_PATH`,
`DEFAULT_MODEL_SLUG`), so a Fabric consumer hooks up without re-deriving any
of them:

```
harness.adapter_id            = "nvidia.fabric.codex"
models.default                = { provider: "roundhouse", model: "roundhouse-local",
                                  api_key_env: "ROUNDHOUSE_API_KEY", base_url: "<origin>/v1" }
mcp.servers.roundhouse        = { transport: "streamable-http", url: "<origin>/mcp",
                                  exposure: "harness_native" }
harness.settings.config_overrides:
  "mcp_servers.roundhouse.bearer_token_env_var"       = "ROUNDHOUSE_API_KEY"   # keeps the secret out of the payload
  "mcp_servers.roundhouse.default_tools_approval_mode" = "approve"             # the belt; annotations are the braces
  "model_providers.roundhouse.requires_openai_auth"    = false                  # documentary at 0.146.0; unverified on the app-server
  "model_providers.roundhouse.env_http_headers"        = { "x-roundhouse-key": "ROUNDHOUSE_API_KEY" }
                                                        # redundant beside a roundhouse key; kept so both artifacts say the same thing
skills.paths                  = [ "<dir>/skills/<name>" … ]                     # the generated leaf directories, unchanged
```

`ForwardedOpenAiLogin` is refused for this artifact with the reason (Fabric
always writes `env_key`). `runtime.max_turns` and `tools.enabled/blocked`
are not written (Codex: fails planning). `model_catalog_json` is not written
either, and the reason is stated: the `GET /models` fetch is gated on the
ambient ChatGPT login mode, which is exactly the mode this artifact refuses,
and under env-key auth with a fresh `CODEX_HOME` the fetch was not observed.
What dropping the catalog does cost is `include_skills_usage_instructions`
(see F5).

**Conformance without a shipped dependency.** The emitted JSON is checked in
a unit test the way the Responses surface is checked against the codex
crates: an oracle pinned as a *dev-dependency*, never shipped. The primary
oracle is a vendored copy of `fabric:schemas/sdk/agent.schema.json` at
`6d9ebc3`, validated with `jsonschema` as a dev-dependency, because it covers
the whole document. The alternative — `nemo-fabric-core = "=0.2.0"` (the only
published stable) deserializing the emitted bytes into `FabricConfig` — is
valid only after diffing `McpServerConfig`, `SkillConfig` and
`HarnessConfig.settings` between `v0.2.0` and the tree read, since the
evidence proves byte-identity for `FabricConfig`, `HarnessConfig`,
`ModelConfig` and `RuntimeConfig` only. Three costs are booked honestly
rather than waved away: either oracle pulls the same 20-crate `jsonschema`
subtree and the same two version splits into `Cargo.lock`'s dev graph (what
changes is that none of it reaches the shipped binary); on the codex-crates
precedent a conformance oracle *is* a watched dependency under `CLAUDE.md`'s
rule, so landing F2 makes Fabric the sixth; and the unlock condition — move
the pin when `0.3.0` publishes, re-diffing the structs first — is recorded
here now and **must be restated in `Cargo.toml` beside the pin when it lands**,
the way the redis 1.2.4 comment does it.

### F3 — Fabric + Harbor as an M10.4 arm (evaluate at M10.4 planning; gated)

Fabric's Harbor integration already does the thing M10.4 says Switchyard's
runner cannot: per-task `n_input_tokens`, `n_cache_tokens`, `n_output_tokens`,
`cost_usd` backfilled into Harbor's result context from the Relay ATIF. It is
an *option* for the roundhouse arm, not a replacement for the Switchyard
harness the plan adopts, and it is gated on three things: whether the task
container can reach a roundhouse outside it (Docker depends on Harbor's
`network_mode` mapping, which is not vendored; Daytona cannot); the
backfill's own preconditions, `fabric_telemetry=relay` and a Relay CLI pinned
`>=0.7.2,<0.8` inside the task image; and the fact that the `/v1`
reconstitution evidence was read on the 0.9 line, so that Relay pin inherits
the unread-line caveat. Two costs are ruled in advance: Fabric's numbers are
a second accounting authority and `nemo-relay.md` already ruled that ours is
the authoritative one, so the Fabric figures are reported beside the
dashboard's, never in place of them; and the arm adds a fourth codex version
to the vigilance file, so R6 applies.

### F4 — Contribute back (opened when F1 lands; none blocks anything here)

Each is a gap the deep dive verified as absent, and each is small:

- **The slug/`tool_search` finding**, with the experiment: a custom Responses
  provider given a catalog slug gets `tool_search` on every request. Fabric's
  Codex adapter has no default and no warning.
- **Cached and reasoning tokens on `AgentUsage`.** Harbor's own context wants
  `n_cache_tokens`, and Fabric backfills it from ATIF because `RunUsage`
  cannot carry it.
- **`bearer_token_env_var` for MCP headers**, so a static bearer keeps
  environment-variable indirection instead of landing in the payload.
- **The MCP annotation and `default_tools_approval_mode` ruling** — an
  unannotated tool cancels under `approval_policy = never`; Fabric has no
  annotation vocabulary and no per-server approval field.
- **A `/v1` suffix check on `base_url`** for Responses-compatible providers.
- **`enforce_usage_reporting` on the Remote Agent adapter**, which sends
  `stream: true` with no `stream_options` and will record zero-token turns
  against a strict upstream.
- **The capability gate**, the same contribution already on Relay's list,
  now with a second consumer whose only money field is a provider-reported
  scalar.

### F5 — Skills compose today, degraded by one flag

Roundhouse emits `skills/<name>/SKILL.md` with unique names; Fabric registers
leaf directories through `skills/extraRoots/set`. Handing the generated leaf
directories to `skills.paths` needs zero changes on either side, and Fabric's
loader avoids the deprecated `$CODEX_HOME/skills` root roundhouse is forced to
use. What the Fabric path loses with the catalog is
`include_skills_usage_instructions: true`: the directories register, but the
model may be handed a bare file list with no instruction that reading one is
how a skill is used. Restoring it needs `config_overrides["model_catalog_json"]`
(untested) or an upstream field. Until one of those is proven, F5 is
"composes, degraded", and the F2 artifact says so where it writes the paths.

## Re-verify before relying (the vigilance triggers)

- **Before any Fabric-driven deployment**: run a Fabric-driven Codex
  end-to-end against a real roundhouse (the experiment stopped at the SDK and
  a mock); re-derive at Fabric's current `openai-codex` pin the three 0.146.0
  rulings the experiment did not reach — `resolve_provider_auth` ignoring
  `requires_openai_auth`; annotations flipping the `Auto` approval branch;
  `include_skills_usage_instructions` — and re-run the §6 driver. Fabric
  moves the pin on its own schedule and there is no `--version` probe on that
  path.
- **When `nemo-fabric-core 0.3.0` publishes**: re-diff `FabricConfig`,
  `HarnessConfig`, `ModelConfig`, `RuntimeConfig`, `McpServerConfig` and
  `SkillConfig` against the dev pin, and move it.
- **If Fabric ships `AdapterKind::Http` or a Rust adapter path**: the
  "roundhouse ships a Fabric adapter" shape stops needing a Python host and
  becomes a different proposition. As of 2026-09-05 nothing in the tree, the
  roadmap, or the public tracker schedules it.
- **If Fabric grows a portable session layer**: its schema notes name
  "normalized trajectory structures and policy hooks for auditability" as
  deferred. That is the slot roundhouse's log would fill; watch it.
- **If Fabric's Claude adapter gains a Responses-compatible path, or the
  Messages-surface ruling changes**: the Codex-only limit on the third
  topology lifts.

## What this buys, and what it costs

Bought: a third way to hook up, whose admission preconditions are proven on
the wire, that reaches every Fabric consumer — Harbor and the eval and
rollout platforms Fabric fronts — with the same four constants roundhouse
already owns; a per-task attribution path for M10.4 that closes a gap the
plan names; seven concrete upstream contributions, one of them measured; and
the two genuinely novel things in this tree — the budgeted turn and the
steering primitive — untouched, because Fabric has no vocabulary for either.

Cost: one more neighbor to watch — by role today, and by manifest the day F2
lands its dev-only oracle pin, at which point Fabric is the sixth watched
dependency under `CLAUDE.md`'s rule with the read-upstream-before-it-lands
and re-verify-before-milestones obligations that carries; a documented
topology whose transparency claim is weaker than Direct's and says so in
its own requirement list; and two things for the owner to decide: whether
`CLAUDE.md`'s watched-dependency paragraph should name NeMo Fabric beside
Relay (this ruling recommends it), and whether the product paragraph's
"Relay owns the harness" is reworded to the four-clause division above.
