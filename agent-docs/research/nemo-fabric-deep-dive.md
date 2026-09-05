<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# NVIDIA NeMo Fabric ↔ Roundhouse: what is duplicated, and what the Rust crate would buy

> **Status: evidence base.** A read-only deep dive of NeMo Fabric at
> `6d9ebc3` against this tree at `4fa34d0`, produced to answer two questions
> from the product owner: *does roundhouse re-invent bits of NeMo Fabric?*
> and *should roundhouse migrate to its Rust library, `nemo-fabric-core`?*
> The ruling that synthesizes this into direction is
> `../synergies/nemo-fabric.md`, which this document exists to justify. Every
> code claim carries a file:line into one of the trees below; every negative
> claim carries the search that proved it.

Sources:

- **fabric** = `NVIDIA/NeMo-Fabric` @ `6d9ebc35bf00c28c322d61b6aba16be5c819ef89`
  (2026-09-04, "Merge pull request #272"). Workspace version `0.3.0`
  (`fabric:Cargo.toml:17`), edition 2024, no `rust-version`, no
  `rust-toolchain` file. Newest tag `v0.3.0-beta.2`. Three Rust crates:
  `crates/fabric-core` (published as `nemo-fabric-core`), `crates/fabric-cli`,
  `crates/fabric-python` (pyo3). Python SDK under `sdk/python`, adapters under
  `adapters/python` and `adapters/typescript`.
- **rh** = this repository @ `4fa34d0`.
- **relay** = `NVIDIA/NeMo-Relay` @ `ba60230` (main, 2026-09-04, the
  `release/0.8` forward-merge), a shallow clone made for one question in §7.
- **openai-codex 0.144.4** and **openai-codex-cli-bin 0.144.4**, downloaded
  from PyPI for the experiment in §6. The SDK source is not in the fabric tree;
  the app-server is a compiled binary. Nothing about its internals is claimed
  here beyond what it put on the wire.

How it was produced: four independent deep reads (the crate itself; the launch
surface; the type vocabulary; the integration shapes), each fact-checked by a
separate agent that re-ran every stated search and re-opened every cited line
(47 spot checks; 31 held exactly, 16 were off by a few lines or a miscount,
none reversed a conclusion — the corrected citations are used below), then a
completeness pass that named seven gaps, six of which were closed. The one
that stayed open is marked.

---

## A) Fabric in one page

**What it is.** "NeMo Fabric gives applications and platforms one
configurable, observable way to run agent harnesses and custom agents"
(`fabric:README.md:19-21`). A *consumer* (an application, an evaluation
system, an RL rollout platform) hands Fabric a typed `FabricConfig`; Fabric
resolves an *adapter*; the adapter translates the config and lifecycle into
the native model of an *Adapter Target* — Hermes Agent, Codex, Claude Code,
LangChain Deep Agents, mini-SWE-agent, Pi, or a custom agent — and returns a
normalized `RunResult` with artifacts and telemetry references
(`fabric:README.md:25-62`). NeMo Relay appears in its execution-flow diagram
as the telemetry sink, not as a hop in the model path (`fabric:README.md:46-62`).

**Where the Rust is.** `nemo-fabric-core` is nine files, 13,511 lines with
inline tests (`wc -l crates/fabric-core/src/*.rs`), 128 top-level public items
across `lib.rs`, `config.rs`, `runtime.rs`. Its direct dependencies are five:
`jsonschema 0.49`, `schemars 1`, `serde`, `serde_json`, `thiserror 2`
(`fabric:crates/fabric-core/Cargo.toml:16-21`; `fabric:Cargo.toml:32-36`).

| Module | Lines | What it is |
|---|---|---|
| `config` | 6692 | `FabricConfig` and ~60 sub-structs, adapter and target descriptor loading and discovery, `validate_config` (private), `resolve_run_plan_from_config` → `RunPlan` |
| `runtime` | 4221 | `run_plan` / `start_runtime` / `invoke_runtime` / `invoke_openai_stream` / `stop_runtime`; the adapter-host process supervisor; the OpenAI-stream types |
| `doctor` | 661 | `doctor_plan(&RunPlan) -> DoctorReport`, pure |
| `schema` | 619 | `schemars` JSON Schema generation for 19 public types |
| `agent_config` | 499 | `AgentConfig`, the projected southbound config an adapter receives |
| `agent_execution` | 346 | `AgentRunRequest` / `AgentRunResult` / `AgentUsage` / `AgentArtifact` |
| `error` | 316 | `FabricError`, 30 variants |
| `adapter_contract` | 100 | `ADAPTER_CONTRACT_VERSION = "fabric.adapter/v1alpha2"` and 18 extension points |

**The crate has a pure half and an impure half.** Plan, diagnose and
schema-generate run with no interpreter and no adapter process:
`resolve_run_plan_from_config` (`fabric:crates/fabric-core/src/config.rs:2257`),
`doctor_plan` (`fabric:crates/fabric-core/src/doctor.rs:56`),
`generate_schema` (`fabric:crates/fabric-core/src/schema.rs:175`). Execution
does not. `uses_local_host` accepts only `AdapterKind::{Process, Python}`
(`fabric:crates/fabric-core/src/runtime.rs:945-950`); `Http` and
`NativePlugin` are declared in the enum
(`fabric:crates/fabric-core/src/config.rs:947-958`) and return
`UnsupportedRuntimeAdapter` at all four public entry points (`start_runtime`
:840, `invoke_runtime` :857, `invoke_openai_stream` :886-889, `stop_runtime`
:939). A `Python` adapter is always `<python> -m <module>`
(`fabric:crates/fabric-core/src/runtime.rs:1854-1876`), with the interpreter
resolved from `harness.settings.python`, `harness.settings.python_env`,
`ADAPTER_PYTHON`, `VIRTUAL_ENV`, an interpreter beside the executable, then
bare `python3` (`fabric:crates/fabric-core/src/runtime.rs:2396-2467`).

**Negative: no harness can be run from Rust alone.**
`find adapters -name '*.rs' | wc -l` (fabric) → 0.
`grep -rho '"adapter_kind": *"[a-z_]*"' adapters | sort | uniq -c` (fabric) →
6 `python`, 1 `process` (Pi, whose runner is `node dist/cli.js`,
`fabric:adapters/typescript/pi/pi.fabric-adapter.json:3-8`). The CLI's
credential-free `scripted` preset is `python3 run.py`
(`fabric:crates/fabric-cli/assets/adapters/scripted/scripted.fabric-adapter.json:4-8`).
Adapter-side typed bindings ship for Python and TypeScript only
(`fabric:schemas/SCHEMA.md:53-64`). Nothing schedules a change: the release
notes list "remotely hosted adapter services are not included" as a current
limitation (`fabric:docs/about-nemo-fabric/release-notes.mdx:91`), the README
roadmap names a "Remote-agent thin-client adapter" — a Python adapter for a
remote *agent*, not a transport (`fabric:README.md:278-285`) — and a search of
the public issue and PR trackers on 2026-09-05 for `http adapter`,
`AdapterKind`, `remote adapter` returned no issue and no relevant PR.

**The adapter host protocol.** Newline-delimited JSON over the child's
stdin/stdout (`fabric:crates/fabric-core/src/runtime.rs:1885-1888`,
`:1761-1789`), four operations — `start`, `invoke`, `invoke_openai_stream`,
`stop` (`:634-641`) — with timeouts of 90 s to start, one hour per invoke by
default, 10 s to stop (`:38-43`). Live child processes are held in a
process-global `static LOCAL_HOSTS` keyed by `runtime_id` (`:70-71`);
`RuntimeHandle` is a serializable token that resolves only inside the process
that created it. The crate is fully synchronous: `rg -n 'async fn|\.await'
crates/fabric-core/src/*.rs` (fabric) → 0, and the 93-crate transitive tree
(`cargo tree -p nemo-fabric-core --edges normal`) has zero matches for
`tokio|reqwest|axum|async-std|smol|futures`. Core never binds a socket:
non-test `std::net` matches → 0 (the two hits are inside `#[cfg(test)]` from
`runtime.rs:2940`).

**The OpenAI stream surface faces the other way.** `invoke_openai_stream`
takes an `OpenAiStreamTransport { port, token }` the *caller* supplies
(`fabric:crates/fabric-core/src/runtime.rs:864-869`); `OpenAiStreamHost` has
one variant, documented "SDK-owned IPv4 loopback listener" (`:417-422`); the
listener is Python (`fabric:sdk/python/nemo-fabric-runtime/src/nemo_fabric/openai_streaming.py:78,132-135`).
It is a progressive-output channel for one agent run, dialed *out* by the
adapter into the consumer. It is not a model endpoint a harness calls, and it
is the opposite direction from roundhouse's `/v1/responses`.

**The Python SDK is not thin.** 5,675 lines under
`sdk/python/nemo-fabric-runtime/src/nemo_fabric/*.py`; the multi-turn
`Runtime` state machine (`runtime.py`, 582 lines) and the streaming consumer
(`openai_streaming.py`, 788 lines) exist only in Python. The pyo3 binding
exposes eight JSON-string functions (`fabric:crates/fabric-python/src/lib.rs:178-187`)
and spawns a Python subprocess to ask `sysconfig` where installed descriptors
live (`:245-250`). The Rust core is the config compiler and the process
supervisor; the Python SDK is the runtime.

**Descriptor discovery is not portable to a crates.io consumer.**
`repository_adapter_dir()` bakes `CARGO_MANIFEST_DIR/../../adapters` at
compile time (`fabric:crates/fabric-core/src/config.rs:727-730`), a missing
path is skipped silently (`:527-532`), and the tree calls this a temporary
packaging workaround with an unmet removal condition (`fabric:TODO.md:12-38`).
A Rust consumer must pass descriptors through `discovery.local_paths` or the
`#[doc(hidden)]` `..._with_adapter_directories` variant (`:2270`).

---

## B) What the product owner recognized, located

The quickstart in the question is the **Hermes** quickstart
(`adapter_id="nvidia.fabric.hermes"`, `fabric:README.md:114-134`). Its
`RuntimeConfig(max_turns=1)` would fail planning against Codex: the
compatibility matrix says `runtime.max_turns` is "No" for Codex
(`fabric:adapters/README.md:137`), and "No" means "an explicitly configured
value fails planning instead of being ignored" (`:107-109`).

The four roundhouse candidates for "something like this in our bootstrap
binary":

| Candidate | What it is | Analogue? |
|---|---|---|
| `rh:crates/roundhouse-server/src/main.rs` | The server's composition root: "Deliberately thin: everything interesting is a seam, and this only chooses which implementation of each seam to instantiate" (`:6-9`). Reads `ROUNDHOUSE_*` and binds a socket (`:611`). Runs no agent. | No |
| `rh:crates/roundhouse-server/src/codex_launch.rs` (+ `codex_launch/skills.rs`) | "What an operator hands a Codex client so that client hooks up to this deployment without being modified. Two files come out of here: the `config.toml` … and the model catalog" (`:4-9`). | **Yes — the only one.** A config emitter for one harness. |
| `rh:use-cases/cache-aware-routing/run.py` | An HTTP load driver: POSTs `/v1/responses` with `x-roundhouse-key`, replays `turns.jsonl` per membership with a caller-chosen `prompt_cache_key`, asserts cached-token fractions, reads `/v1/metrics` (`:67-113`, `:157-173`, `:187-202`). Roundhouse is the *server* in this script. | No |
| `rh:vault/launch_roundhouse.py` | Execs `roundhouse-server` with Vault-resolved `ROUNDHOUSE_*` variables (`:79-98`). Launches the service the harness is pointed at, never a harness. | No |

`rg -l 'config\.toml|CODEX_HOME|SKILL\.md|mcp_servers\.' crates/` (rh) → 12
files; ten are the MCP surface and its tests, and the two that produce
harness-facing config are `codex_launch.rs` and `skills.rs`. Their test
modules start at `:613` of 1080 and `:357` of 609, so the overlap is **968
non-test lines** against roughly 71,000 lines of crate source. Roundhouse has
never referenced Fabric: `rg -n 'nemo-fabric' --glob '!target/**' .` (rh) → 0.

---

## C) Overlap table

Legend — **DUP** = the same idea implemented twice; **CONV** = the same idea,
convergent, in different form; **COMP** = different layer, meshes; **DISJ** =
no contact; **FALSE FRIEND** = a shared noun for a different idea.

| # | Roundhouse | Fabric | Class | Note |
|---|---|---|---|---|
| 1 | `codex_launch.rs`: hand-templated `config.toml` written into `CODEX_HOME` | Codex adapter: a request-scoped config dict passed to `thread_start(config=...)` — "Build request-scoped Codex config without writing a user profile" (`fabric:adapters/python/codex/src/nemo_fabric_adapters/codex/adapter.py:816`); never writes a `config.toml` (`rg -c 'config\.toml'` on the adapter → 0) | **CONV** | Same TOML vocabulary (`model_providers.<name>.{base_url,env_key,wire_api}`, `mcp_servers.<key>`), two injection points. §D is the field-by-field. |
| 2 | `DEFAULT_MODEL_SLUG = "roundhouse-local"`, a safety property (`rh:codex_launch.rs:108-115`) | `selected_model` passes the slug verbatim for a custom provider (`fabric:.../codex/adapter.py:500-507`); no default, no warning | **CONV**, roundhouse ahead | §6 shows why the slug matters at 0.144.4. |
| 3 | `model_catalog_json` under both auth kinds (`rh:codex_launch.rs:234-240`, `:424`, `:477-550`) | none: `rg -n 'model_catalog' -g '!*.lock' .` (fabric) → 0 | **DISJ** | Loses `supports_search_tool: false` and `include_skills_usage_instructions: true` (`rh:codex_launch.rs:510-546`). |
| 4 | `requires_openai_auth` + `env_key` as one block per auth kind (`rh:codex_launch.rs:381-405`) | `env_key` always, the flag never (`rg -n 'requires_openai_auth' .` → 0) | **CONV** for `RoundhouseKey`; **DISJ** for `ForwardedOpenAiLogin` | The forwarded stanza needs the flag `true` and *no* `env_key` (`rh:codex_launch.rs:389-404`, pinned at `:693-701`); Fabric requires `api_key_env` for every non-`openai` provider (`fabric:.../codex/adapter.py:517-526`, `:540`) and `config_overrides` can set leaves, never delete one (`:671-694`). |
| 5 | `[mcp_servers.roundhouse] bearer_token_env_var` (`rh:codex_launch.rs:448-450`) | `custom_headers` with `$VAR` expanded into the payload (`fabric:adapters/python/common/src/nemo_fabric_adapters/common/utils.py:96-102`; asserted at `fabric:tests/adapters/test_codex_adapter.py:484` and `:508`) | **CONV**, opposite property on disk | Fabric's default path writes the secret's value into the JSON-RPC config and blanks the variable in the child env. `bearer_token_env_var` is restorable only through `config_overrides`. |
| 6 | `default_tools_approval_mode = "approve"` and the annotation-cancel ruling (`rh:codex_launch.rs:135-171`) | none (`rg -c 'default_tools_approval_mode'` → 0); a thread-level `approval_mode ∈ {auto_review, deny_all}` (`fabric:.../codex/adapter.py:67-71`, `:566-576`) | **DISJ** | Not fatal: all eight roundhouse tools carry `destructive_hint: false` (`grep -c destructive_hint crates/roundhouse-mcp/src/tools.rs` (rh) → 9), which flips codex 0.146.0's `Auto` branch to "no approval needed" (`rh:codex_launch.rs:160-166`). Unproven for the 0.144.4 app-server. |
| 7 | `skills::skill_files()` — *generates* `skills/<name>/SKILL.md` from the MCP descriptors (`rh:crates/roundhouse-server/src/codex_launch/skills.rs:96-98`, `:230-238`) | `SkillConfig.paths` — *registers* existing directories via `skills/extraRoots/set` (`fabric:.../codex/adapter.py:258-307`) | **COMP** | The directory contract matches exactly: a directory, containing `SKILL.md`, unique basename (`fabric:.../codex/adapter.py:258-284`). Zero changes on either side. |
| 8 | `FrontierModelSpec{provider, model, wire_protocol, cache_model, pricing, quality_prior, base_ttft_ms, …}` and `catalog_config` providers `{base_url, routes, auth.env, extra_headers}` (`rh:crates/roundhouse-fleet/src/frontier.rs:33-51`; `rh:crates/roundhouse-server/src/catalog_config/providers.rs:60-144`) | `ModelConfig{provider, model, temperature, api_key_env, base_url, settings}` (`fabric:crates/fabric-core/src/config.rs:960-982`) | **FALSE FRIEND** | Fabric's is the endpoint a harness should call — the thing roundhouse *emits* to a client. Roundhouse's is a price-and-quality sheet the router picks among per turn (`rh:frontier.rs:130-155`). Fabric selects one role per run: "More than one role without `default` fails planning" (`fabric:adapters/README.md:147-148`). |
| 9 | `Usage{input, cached_input, output, reasoning, accounting}` with `Accounting::{Reported, Estimated}` (`rh:crates/roundhouse-core/src/event.rs:31-79`) | `AgentUsage{input_tokens, output_tokens, total_tokens, cost_usd}` (`fabric:crates/fabric-core/src/agent_execution.rs:85-103`); `RunUsage` re-projects it (`fabric:.../runtime.rs:176-192`, `:1632-1640`) | **COMP**, roundhouse richer | `rg -ni 'cached'` in fabric-core → 1 (an OAuth token-cache buffer); `reasoning` → 0; `estimat` → 0. `cost_usd` is "when reported by the provider", passed through at `runtime.rs:1637`, never computed. |
| 10 | `roundhouse-relay`: ATOF, ATIF v1.7, `LlmOptimizationSummary` **produced** from the log; three routes (`rh:crates/roundhouse-server/src/relay_api.rs:90-94`) | `RelayConfig` / `RelayAtofConfig` / `RelayAtifConfig`: **knobs** handed to a Relay gateway the adapter starts (`fabric:crates/fabric-core/src/config.rs:1277-1452`; `fabric:.../codex/adapter.py:1209`) | **COMP** — two ends of one pipe | Fabric is a producer-by-delegation and a filename-level consumer: `promote_relay_artifacts_to_manifest` accepts `kind ∈ {"atof","atif"}`, requires a local `path.exists()`, never opens the file (`fabric:.../runtime.rs:2494-2526`). `LlmOptimizationSummary` → 0 matches tree-wide. |
| 11 | `TurnPolicy`, budgets, fair-use windows, the validate/steer loop, the judge (`rh:crates/roundhouse-core/src/control/policy.rs:532-542`; `validate/mod.rs:483-497`) | none | **DISJ** | `rg -ni` over `crates/fabric-core/src`: `price\|pricing` 0, `quality` 0, `ttft\|latency` 0, `steer` 0, `judge` 0, `append-only` 0, `lease\|fenc` 0 real; `budget` 3, all one test fixture's opaque `max_budget_usd` pass-through (`config.rs:5903`, `:6343`). |
| 12 | `RoutingPolicy`, `Candidate`, `ProviderRoutes` | `CapabilityRoute`, `candidate` | **FALSE FRIEND** | "This target describes execution ownership, **not network routing**" (`fabric:crates/fabric-core/src/config.rs:3852-3854`). Fabric's `candidate` is a duplicate adapter descriptor (`config.rs:634-704`) or an interpreter-path fallback (`runtime.rs`). |
| 13 | `RuntimeConfig`-shaped limits: `TriggerConfig`, `ReviewLimits` (`rh:crates/roundhouse-core/src/validate/trigger.rs:385-396`) | `RuntimeConfig{max_turns, timeout_seconds, input_schema, output_schema, artifacts}` (`fabric:config.rs:984-1014`) | **FALSE FRIEND** | Fabric bounds one invocation of one harness; roundhouse bounds its own interjections into a tenant's sessions. |
| 14 | boot-time load-or-die cross-checks (`rh:crates/roundhouse-server/src/control_config/crosscheck.rs:1-59`) | `doctor_plan` → `Pass/Warn/Fail` report (`fabric:doctor.rs:44-76`), several checks "declared but not probed" (`:336-352`) | **COMP** in intent, disjoint in content | Fabric checks one plan against one adapter's declared needs; roundhouse checks two operator files against each other and refuses to start. |
| 15 | MCP mount: streamable HTTP, bearer turn key, GET → 405, all three annotations | `McpTransport::StreamableHttp`, `custom_headers`, `McpAuthenticationConfig::{OAuth2, ServiceAccount}` (`fabric:config.rs:1096-1103`, `:1155-1210`, `:1238-1240`) | **COMP** | Neither auth variant is a static bearer, and the Codex adapter rejects most of their fields (`fabric:.../codex/adapter.py:206-240`); `custom_headers` is the bridge. `exposure = "fabric_managed"` is "No; not implemented" for every adapter (`fabric:adapters/README.md:139`). |
| 16 | `enforce_usage_reporting` (`rh:crates/roundhouse-fleet/src/usage.rs:98`) | none — the Remote Agent adapter sends `stream: true` and no `stream_options` (`fabric:adapters/python/remote-agent/src/nemo_fabric_adapters/remote_agent/adapter.py:246-252`) | **COMP**, roundhouse ahead | The silent zero-token failure `rh:README.md:619-623` names, on Fabric's own adapter. |

---

## D) The launch surface, field by field

Roundhouse writes a file an unmodified `codex` reads out of `CODEX_HOME`
(`rh:codex_launch.rs:4-10`). Fabric drives the Codex Python SDK, whose
`thread_start(config=...)` owns and spawns its own pinned `codex app-server`
(`fabric:.../codex/adapter.py:1131`, `:1151`, `:1308`;
`fabric:adapters/python/codex/README.md:71-79`). "NeMo Fabric does not execute
`codex` for agent turns"; "A `codex` command on `PATH` does not replace the
SDK-owned runtime" (`fabric:docs/integrations/harness/codex.mdx:8-10`,
`:133-137`). Pin: `openai-codex==0.144.4`
(`fabric:adapters/python/codex/pyproject.toml:35`), which depends on a
separate compiled `openai-codex-cli-bin` (`fabric:uv.lock:2999-3023`).

| Setting | Roundhouse writes | Fabric writes or can carry | Consequence |
|---|---|---|---|
| `base_url` | `[model_providers.roundhouse] base_url`, refused unless it ends in `/v1` (`rh:codex_launch.rs:248-254`, `:431`) | `base_url.rstrip("/")`, required for any non-`openai` provider, no suffix check (`fabric:.../codex/adapter.py:527-545`) | Equal when the operator writes `/v1`. A bare origin plans, starts, and 404s every turn. |
| `wire_api` | `"responses"` (`:432`) | `"responses"`, hard-coded (`:541`) | Identical. |
| `env_key` | `ROUNDHOUSE_API_KEY` by default (`:105`, `:387`) | `api_key_env`, required, and the variable must be set or the runtime refuses to start (`:517-526`, `:540`) | Fabric is stricter. |
| `requires_openai_auth` | Both kinds (`:381-405`) | Never; `config_overrides` only | `RoundhouseKey` is reproducible because `env_key` decides at 0.146.0 (`rh:codex_launch.rs:35-50`). `ForwardedOpenAiLogin` is not expressible (row 4 of §C). |
| `env_http_headers` | `x-roundhouse-key = ROUNDHOUSE_API_KEY` (`:441-442`) | `config_overrides["model_providers.roundhouse.env_http_headers"]` | Redundant beside a roundhouse key; load-bearing only for forwarding, which is inexpressible anyway. |
| `model_catalog_json` | Both kinds, absolute-path-checked (`:234-240`, `:328-332`, `:424`) | Never; `config_overrides["model_catalog_json"]`, untested | §6 measures what the missing pin costs. |
| model slug | `roundhouse-local` (`:108-115`) | verbatim (`:500-507`) | §6. |
| `mcp_servers` | `url` + `bearer_token_env_var` (`:448-450`) | `{"url"}` + `http_headers` from expanded `custom_headers` (`:194-205`), or OAuth (`:207-232`) | Secret lands in the payload unless `config_overrides["mcp_servers.roundhouse.bearer_token_env_var"]` is set. |
| `default_tools_approval_mode` | `"approve"` (`:461`) | `config_overrides`; nesting proven by `fabric:tests/adapters/test_codex_adapter.py:472-474` and `:479-495` | See §C row 6. |
| skills | `$CODEX_HOME/skills/<name>/SKILL.md`, the deprecated root, on purpose (`rh:skills.rs:85-98`) | `skills/extraRoots/set`, process-scoped (`fabric:.../codex/adapter.py:287-308`) | Complementary; Fabric's is the cleaner loader. |
| sandbox / approval | not in the config; e2e flags (`rh:crates/roundhouse-server/tests/codex_e2e.rs:1187-1204`) | first-class settings with schema defaults (`fabric:.../codex/adapter.py:62-71`, `:555-576`) | Fabric ahead. |
| launch | `codex exec` / `exec resume --last`, `env_clear()` + five-key allowlist (`rh:codex_e2e.rs:1176-1229`) | `AsyncCodex(config=CodexConfig(codex_bin, cwd, env))` → `thread_start` → `thread.turn` (`:867-890`, `:1126-1171`); `CODEX_HOME` redirected to a fresh `custom-provider-home` for any non-`openai` provider (`:626-630`, asserted `fabric:tests/adapters/test_codex_adapter.py:1237-1241`) | Two binaries. The fresh `CODEX_HOME` makes roundhouse's ambient-login hazard (`rh:codex_launch.rs:35-50`) structurally unreachable — a property a file generator cannot have. |
| conversation identity | `prompt_cache_key`, required with 422 (`rh:crates/roundhouse-server/src/responses_api.rs:255-263`) | `thread.id`; Fabric never names `prompt_cache_key` (2 tree-wide matches, both vendored licence text in `ATTRIBUTIONS-Node.md`) | Settled in §6. |
| Relay insertion | not roundhouse's | rewrites the codex-facing `base_url` to `http://127.0.0.1:<port>` with no `/v1` and hands the real URL to the gateway as `--openai-base-url` (`fabric:.../codex/adapter.py:795-806`, `:832-843`; `fabric:adapters/python/common/src/nemo_fabric_adapters/common/relay_gateway.py:174-175`) | §7. |

**Can a `FabricConfig` express the roundhouse stanza?** The `RoundhouseKey`
stanza, yes. `config_toml()` (`rh:codex_launch.rs:366-468`) writes fourteen
key/value lines. Seven have a Fabric equivalent: three as the same bytes
(`base_url`, `wire_api`, `env_key`), and `model`, `model_provider`, the
provider `name`, and the MCP `url` by another route (`thread_start`
arguments and the `mcp_servers` translation). The other seven —
`model_catalog_json`, `requires_openai_auth`, `env_http_headers`,
`supports_websockets`, `bearer_token_env_var`, `default_tools_approval_mode`,
`use_agent_identity` — Fabric never emits (`rg -c '<term>' -g '!*.lock' .`
(fabric) → 0 files for each) and reach the client only through
`harness.settings.config_overrides` — a dict of dotted keys,
schema-validated (`propertyNames.pattern = "^[^.]+(?:\.[^.]+)*$"`), split and
nested by `_apply_config_overrides` *after* the provider and MCP layers so an
override wins (`fabric:.../codex/adapter.py:671-694`, `:815-830`). The
`ForwardedOpenAiLogin` stanza, no.

**Claude Code.** Fabric's Claude adapter configures the harness through
`ANTHROPIC_API_KEY` and `ANTHROPIC_BASE_URL`, stripping `/v1` for a
non-`anthropic` provider (`fabric:adapters/python/claude/src/nemo_fabric_adapters/claude/adapter.py:236-240`,
`:270-274`); "The configured endpoint must implement the Anthropic Messages
protocol" (`fabric:docs/integrations/harness/claude.mdx:127-129`). Roundhouse
serves no `/v1/messages`: a sweep of every `.route(` literal in
`rh:crates/roundhouse-server/src` (20 of them) finds no `messages` and no
`models`, and the binary asserts `expect_err("this build has no Anthropic
Messages client")` (`rh:crates/roundhouse-server/src/main.rs:1021-1043`).
Claude Code via Fabric hits a 404 on its first turn. This is the standing
Messages-surface ruling (`../synergies/ecosystem-round-2.md`), unchanged.

---

## E) What `codex_e2e.rs` tests now, and what the ownership argument rests on

The sentence that grounded the launch-surface dedup — "the one test M9 exists
for — a real codex binary executing our synthetic tool call — cannot be
delegated to a launcher we do not own" (`rh:codex_launch.rs:19-28`;
`../synergies/ecosystem-round-2.md`) — names a test that no longer exists.
M10.0 T7 deleted both tool-call tests because the steer became text
(`rh:crates/roundhouse-server/tests/codex_e2e.rs:21-33`). Of the nine gated
tests today, three are bound to `codex exec` + `CODEX_HOME` semantics —
`exec resume --last` (`:1464`), `$CODEX_HOME/skills` (`:1959`), a crafted
`auth.json` for the forwarded login (`:2036`) — and the other six assert
claims any driver that continues a conversation and records request bodies
could satisfy. The version-vigilance probe is a printed WARNING against
`VERIFIED_VERSION = "codex-cli 0.146.0"` (`:278`, `:834-845`); the Fabric
path has no `--version` probe, because the binary is a transitive pin of a
`pip install`.

---

## 6) Experiment: what the pinned app-server puts on the wire

The highest-stakes unknown in every dive was whether the SDK-owned app-server
sends `prompt_cache_key`, because roundhouse refuses a `/v1/responses` request
without one. Neither tree could answer it. The gap-closing stage installed the
two published wheels (`openai-codex==0.144.4`, `openai-codex-cli-bin==0.144.4`,
128 MB, the exact pin at `fabric:adapters/python/codex/pyproject.toml:35`)
into a scratch venv and drove the SDK the way Fabric's adapter does —
`Codex(CodexConfig(env))` with a fresh, never-logged-in `CODEX_HOME` (mirroring
`fabric:.../codex/adapter.py:626-630`), `thread_start(model=…,
model_provider="roundhouse", config={"model_providers": {"roundhouse": {"name",
"base_url": "<mock>/v1", "env_key": "MOCK_TEST_API_KEY", "wire_api":
"responses"}}})` (the exact dict `custom_model_provider_config` builds,
`fabric:.../codex/adapter.py:535-544`), two `thread.run` calls — against a
mock Responses server whose SSE shapes were copied from
`fabric:tests/_utils/mock_api_server.py` and which records every payload and
counts `GET /v1/models`. Three runs: `model="mock-model"` on the custom
provider; `model="gpt-5.4"` on the custom provider; `model="gpt-5.4"` on the
built-in `openai` provider. The recorded payloads were re-read independently
of the agent that ran them.

| Question | Result, all three runs unless stated |
|---|---|
| Is `prompt_cache_key` sent? | **Yes**, on both turns. |
| Same value across turns? | **Yes** — one UUID, equal to `client_metadata.thread_id` / `session_id`. |
| Is history resent as a prefix? | **Yes.** Turn 2's `input` begins with turn 1's `input` byte-for-byte (3 items), then the assistant reply and the new user message (5 items). `previous_response_id` absent; `store: false`; `stream: true`. |
| Which credential? | `authorization: Bearer <value of env_key>`. `user-agent: codex_python_sdk/0.144.4`. |
| `GET /v1/models`? | **0 hits** in all three runs — env-key auth, fresh `CODEX_HOME`, no catalog pin. Not tested: the ChatGPT-login ambient mode, which is the mode `rh:codex_launch.rs:51-59` says gates the fetch differently. |
| `tool_search` in `tools`? | **Keyed on the model slug, not the provider.** `mock-model`: 10 tools, no `tool_search`. `gpt-5.4`: 11 tools including `{"type": "tool_search"}` and `web_search` — identically on the custom `roundhouse` provider and on the built-in one. |

What this settles, and what it does not:

- **A Fabric-driven Codex meets roundhouse's admission contract.** The
  session is named; the resent history is a prefix. The Fabric-driven shape
  is viable on the wire.
- **The `roundhouse-local` slug is load-bearing under Fabric.** Fabric passes
  the slug verbatim and has no default; a natural `ModelConfig(model="gpt-5.4")`
  puts `tool_search` in `tools` on every turn against roundhouse, and a
  `tool_search_call` item is one of the eleven this surface 422s
  (`rh:crates/roundhouse-server/src/responses_api/wire.rs:531-565`). The
  mechanism differs from the one `rh:codex_launch.rs:108-115` describes
  (`use_responses_lite` → `AdditionalTools` in `input`): what was observed at
  0.144.4 is a tool *definition* in `tools`, gated by catalog metadata for a
  recognized slug. Both land in the same 422 once the routed model answers
  with the tool.
- **The catalog pin's `/models` half is moot for env-key auth at 0.144.4**,
  and its `supports_search_tool: false` half is replaced by the slug rule.
- **Unverified at 0.144.4**: `resolve_provider_auth` ignoring
  `requires_openai_auth`; annotations flipping the `Auto` approval branch;
  `include_skills_usage_instructions`. Also unverified: the same facts at any
  *other* pin — Fabric will move `openai-codex` on its own schedule.

The driver, so the run can be repeated when the pin moves (the mock server is
`fabric:tests/_utils/mock_api_server.py` with a `GET /_requests` dump and a
`/v1/models` counter):

```python
import os, tempfile
from openai_codex import Codex
from openai_codex.client import CodexConfig

codex_home = tempfile.mkdtemp(prefix="codex-home-")   # never logged in
env = os.environ | {"MOCK_TEST_API_KEY": "test", "CODEX_HOME": codex_home,
                    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE": "codex_python_sdk"}
config = {"model_providers": {"roundhouse": {
    "name": "roundhouse", "base_url": f"{MOCK}/v1",
    "env_key": "MOCK_TEST_API_KEY", "wire_api": "responses"}}}
codex = Codex(CodexConfig(env=env))
thread = codex.thread_start(model="mock-model", model_provider="roundhouse", config=config)
thread.run("Remember this value: NONCE"); thread.run("Reply with only the value.")
codex.close()
# then GET {MOCK}/_requests and compare payload[1]["input"][:len(payload[0]["input"])]
```

---

## 7) Relay's gateway and the missing `/v1`

When `telemetry.providers.relay` is on, Fabric points codex at
`http://127.0.0.1:<port>` with no `/v1` (`fabric:.../codex/adapter.py:803`,
`:832-843`) — the exact shape roundhouse's generator refuses because the
mismatch is silent (`rh:codex_launch.rs:248-254`). Read at NeMo Relay
`ba60230`: the gateway's `normalize_openai_path_for_base` guarantees exactly
one `/v1` segment on the outbound URL for every combination of base-with-`/v1`
and path-with-`/v1` (`relay:crates/cli/src/gateway/routes.rs:195-203`, intent
stated at `:234-238`), `ProviderRoute::from_path` accepts both `/responses`
and `/v1/responses` (`:51-64`), and a unit test exercises the base-has-no-`/v1`
case: `openai_upstream_url_accepts_origin_or_v1_base`
(`relay:crates/cli/tests/coverage/shared/gateway_tests.rs:536-568`). The
override that bypasses normalization fires only for a ChatGPT-OAuth-shaped
credential (`relay:crates/cli/src/agents/shared/alignment.rs:447-459`), not the
API-key auth this chain uses. Caveat: read at main (the 0.8 line); Fabric's
Harbor image requires a Relay CLI `>=0.7.2,<0.8`
(`fabric:examples/harbor/README.md:76-77`), and the 0.7 line was not read.

---

## 8) Dependency weight, publication, stability

- **Transitive set: 93 crates.** Against `rh:Cargo.lock`: 20 new (all from
  the `jsonschema` 0.49 subtree: `jsonschema`, `jsonschema-regex`,
  `jsonschema-value`, `referencing`, `fancy-regex`, `fluent-uri`, `fraction`,
  `num*`, `uuid-simd`, `vsimd`, …), 50 present at the identical version, 23 at
  a different version that unifies, and one genuine split: `strum` 0.28
  beside rh's 0.27.2. `schemars` unifies (fabric locks 1.2.1, rh carries
  1.2.2; both `1.x`). No MSRV to honor; CI is `toolchain: stable`.
- **Publication.** `crates.io/api/v1/crates/nemo-fabric-core`, fetched twice:
  `max_version = 0.3.0-beta.2` (2026-09-04), `max_stable_version = 0.2.0`
  (2026-08-19); `0.1.0` yanked. **`0.3.0` is not published**; alpha tags are
  built and deliberately not published (`fabric:RELEASING.md`, "What CI Does
  on a Tag Push"). A pin today is `= "0.2.0"`, `= "0.3.0-beta.2"`, or a git rev.
- **Stability.** `RELEASING.md` makes no API-stability promise
  (`grep -rn -iE 'semver|breaking|stability|stable|experimental|alpha'
  RELEASING.md` → 30 lines, all tag and release mechanics); there is no
  `CHANGELOG.md` by policy. The public item set of `lib.rs`+`config.rs`+`runtime.rs`
  is byte-identical from `v0.2.0` through HEAD (128 items). `v0.1.1` → HEAD
  (four weeks) was +6849/−556 lines with a contract bump
  (`fabric.adapter/v1alpha1` → `v1alpha2`) and two renames
  (`AdapterDescriptorSource` → `DescriptorSource`, `RelayOtlpConfig` →
  `RelayOpenTelemetryConfig`). 22 commits touched `crates/fabric-core/src`
  since 2026-08-01.
- **`= "0.2.0"` would carry the four structs the quickstart names.**
  `FabricConfig`, `HarnessConfig`, `ModelConfig`, `RuntimeConfig` are
  byte-identical between `v0.2.0` (`8101897`) and HEAD; the only `pub` field
  added to `config.rs` since is `AdapterConfigSupport.system_instruction_modes`
  (`:807`), a descriptor-side type.
- **`FabricConfig.schema_version` is an unvalidated `String` in Rust**
  (`fabric:crates/fabric-core/src/config.rs:34`; `grep -rn schema_version
  crates/fabric-core/src/` → 8 matches, none a comparison outside tests). The
  canonical `"fabric.agent/v1alpha1"` lives in the Python SDK
  (`fabric:sdk/python/nemo-fabric-runtime/src/nemo_fabric/models.py:1031`).
  The published JSON Schema types it as a bare string too
  (`fabric:schemas/sdk/agent.schema.json:1518-1521`; required keys
  `["schema_version", "metadata", "runtime"]` at `:1567-1570`).
- **What a runtime dependency would call.** `validate_config` is `pub(crate)`
  (`fabric:config.rs:1804`). Plan and doctor need descriptors on disk. The
  lifecycle functions spawn Python. The honest list is one serde type,
  `FabricConfig` and its nested config structs, plus optionally the schema
  generator.

---

## 9) The Harbor path

M10.4 asks for solve rate from Harbor and the cost and routing narrative from
roundhouse's dashboard, and names Switchyard's per-task cost attribution gap
(`rh:agent-docs/PLAN-frontier-selection.md:68-73`, `:275-287`). Fabric's
Harbor integration (`nemo_fabric.integrations.harbor:FabricAgent`) runs the
adapter and harness inside the task container from a `FabricConfig` the host
serializes (`fabric:examples/harbor/README.md:27-65`), accepts
`--ak fabric_model_base_url=<url>` → `models.default.base_url` (`:102`),
`--ak fabric_environment_env` (`:106`), `--mcp-config` (`:100`), and backfills
Harbor's `AgentContext` from the Relay ATIF's `final_metrics`:
`n_input_tokens`, `n_cache_tokens`, `n_output_tokens`, `cost_usd`
(`fabric:sdk/python/nemo-fabric-runtime/src/nemo_fabric/integrations/harbor/fabric_agent.py:594-600`)
— conditional on `fabric_telemetry=relay` and a Relay CLI in the task image
(`:113`; `fabric:examples/harbor/README.md:76-77`). It supplies no routing
narrative: `routing_decision` → 0 files in fabric.

**Open: can the task container reach a roundhouse outside it?** Not settled by
either tree. The task spec's only network knob is `network_mode = "public"`
(`fabric:examples/harbor/calculator/task/task.toml:23`); every checked-in
`fabric_model_base_url` targets a public HTTPS endpoint; Harbor 0.18.0 is a
PyPI pin, not vendored (`fabric:uv.lock:1117-1120`); a Daytona sandbox is
remote by construction. This is the first check before a Fabric arm is planned.

---

## 10) Standing rulings, re-read against these facts

- **The dependency rule** is "`nemo-relay-types`, nothing else", justified by
  weight (`../synergies/nemo-relay.md`), widened once to admit
  `switchyard-protocol` — rev-pinned, for one narrow role, a typed-contract
  crate. Both admissions are contract crates; neither is an execution crate.
  The widening has never been exercised: `grep -rn switchyard --include=Cargo.toml -r .`
  (rh) → 0. `nemo-fabric-core` would be the third admission and the sixth
  watched dependency under `rh:CLAUDE.md`'s vigilance rule, on a project that
  bumped its contract version and renamed two public types inside one month.
- **The launch-surface dedup** kept Direct with roundhouse's own minimal
  config. Fabric's Codex adapter is a fourth implementation of that surface.
  The test the dedup's sentence names is gone (§E); the ownership case now
  rests on the three `CODEX_HOME`-bound tests, the forwarded-login stanza
  Fabric cannot express, the four safety lines Fabric has no field for, and
  version identity against a binary an operator installs.
- **The Messages-surface ruling** is unchanged by anything in Fabric (§D).
- **"The agent's own stack is not modified"** (`rh:CLAUDE.md`) does not
  survive the Fabric Codex adapter unamended: the operator runs the
  app-server `openai-codex` pins, not the `codex` they had. A Fabric-driven
  deployment needs that clause read as "no roundhouse code inside the
  harness", which is weaker.

---

## 11) Verdict candidates (not ranked — the synthesis decides)

- **V1 — Nothing changes.** Direct stays the reference, Fabric is documented
  as a neighbor, no code moves.
- **V2 — Fabric-driven as a third supported topology, documented with its
  requirement list**, no dependency. Justified by §6.
- **V3 — Roundhouse emits a `FabricConfig`** beside `config.toml` from the
  deferred operator entry point, with a dev-only conformance check (the codex
  crates precedent: a wire oracle pinned as a dev-dependency).
- **V4 — Replace `codex_launch` with Fabric.** Costs §C rows 2–6, §E, §8.
- **V5 — `nemo-fabric-core` as a runtime dependency.** Buys one serde type
  (§8).
- **V6 — Fabric + Harbor as an M10.4 arm.** Gated on §9's open question.
- **V7 — Contribute upstream**: the slug/`tool_search` finding, the
  cached/reasoning split in `AgentUsage`, the MCP annotation and
  `default_tools_approval_mode` ruling, `bearer_token_env_var` indirection for
  MCP headers, a `/v1` suffix check, `enforce_usage_reporting` on the Remote
  Agent adapter, the capability gate.

### Three facts the synthesis should weight heavily

1. **Fabric owns no part of the turn** (§C rows 8–13). The round-2 dedup
   verdict — nobody in the survey owns the turn — extends to Fabric.
2. **Every executing Fabric shape needs a Python interpreter in the process
   tree** (§A). Emitting a config is the only shape with no Python at run time.
3. **The Fabric-driven wire path works** (§6), and the one property that makes
   it safe is the slug roundhouse chooses for the operator and Fabric does not.
