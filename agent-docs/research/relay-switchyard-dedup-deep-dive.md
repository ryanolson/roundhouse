<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

> **Status: evidence base.** Produced 2026-08-19 against NVIDIA-NeMo/NeMo-Relay @ ca08901 and NVIDIA-NeMo/Switchyard @ 5341f71 (both fetched 2026-08-19), with the
> roundhouse tree read for comparison. The ruling that synthesizes this into
> direction is `../synergies/ecosystem-round-2.md`, which this document exists to justify.
> An independent fact-checker re-derived the highest-stakes claims from the
> pinned trees; its verdicts and any corrections are appended. Per
> `agent-docs/README.md`, this snapshot gains dated bracketed notes when the
> world moves - never silent rewrites.

# Relay + Switchyard: the use-heavily / dedup re-evaluation

> **Status: evidence.** A read-only deep dive over fresh clones, produced to
> re-weigh the standing ruling (`SYNERGY-nemo-relay.md`) against the product
> owner's moved directive — *use both heavily wherever they make sense, dedup
> roundhouse's efforts with theirs*. This document reports what is there. It
> does not rule; the synthesis does. Every claim carries a `file:line` into a
> pinned tree or a URL.

**Pins, all fetched 2026-08-19.**

| Tree | Rev | Date (UTC) | Version | Prior pin (standing ruling) |
|---|---|---|---|---|
| `NVIDIA-NeMo/NeMo-Relay` | `ca08901` | 2026-08-19 17:06 | 0.8.0 (`Cargo.toml:24`) | `c37b551` (2026-08-18 18:42) |
| `NVIDIA-NeMo/Switchyard` | `5341f71` | 2026-08-19 15:44 | 0.2.0 (`Cargo.toml:17`) | `47babb1` (2026-08-18 22:38) |
| roundhouse | `c168de8` + uncommitted M6 review-fix diff | — | 0.1.0 | — |

Both external clones are shallow: Switchyard history is visible from
2026-08-11 (`2bef154`), Relay from 2026-08-03 (`a65e482`). Churn figures below
are therefore *lower bounds* over an 8-day and 16-day window respectively.

Sizes, for scale: Relay 391,383 lines of Rust across 12 crates; Switchyard
46,412 lines of Rust across 7 crates plus 3,025 lines of Python; roundhouse
55,709 lines across 5 crates.

---

## 0) The one-pager: what these two are, at these revs

**NeMo Relay** is an observability-and-control *runtime for agent processes*.
Its centre is a process-global middleware + event runtime (`crates/core`,
~156k lines) plus a CLI that launches Codex/Claude Code with injected config
and stands a loopback gateway in the provider path. It owns hooks, scopes,
codecs, ATOF/ATIF export, and PII redaction. It owns no conversation state, no
tenancy, no budgets, no spend. Its MCP server answers `tools/list` with `[]`
(`crates/cli/src/mcp/protocol.rs:72-76`) — it exists only to keep a shared
gateway alive.

**Switchyard** is a *provider-neutral routing library and inference proxy*.
`switchyard-libsy` (13,711 lines) is a set of composable routing `Algorithm`s
driven by a host; `switchyard-protocol` (2,482 lines) is the normalized
request/response/metadata contract; `switchyard-server` (5,933 lines) is an
axum proxy serving OpenAI Chat, OpenAI Responses and Anthropic Messages from
one TOML deployment; `switchyard launch` (Python, 2,503 lines) is a
coding-agent launcher for Codex, Claude Code and OpenClaw. Since the standing
ruling was written it has grown an **advisor-gate review algorithm** and a
**caller-credential pass-through mode**. It has no tenancy, no budgets, no
spend ledger, no principals, and no MCP surface at all (`grep -rniE
'budget|quota|tenant|principal|spend' crates/` returns only retry budgets,
token budgets, and the advisor gate's review counters).

**The division that both trees still respect.** Relay owns the harness;
Switchyard owns the route and now the proxy; neither owns the turn. Roundhouse
owns the turn — durable log, prefix admission, tenancy, budgets, spend,
steering, MCP.

---

## 1) What changed upstream since our pins

### 1.1 The honest headline: almost nothing moved, but a lot was not previously seen

**Relay `c37b551..ca08901` — 6 commits, and none of them touch a crate the
adoption calculus depends on.** `git diff --name-only c37b551..ca08901 | cut
-d/ -f1-2 | sort -u` yields `crates/core`, `crates/ffi`, `crates/node`,
`crates/plugin`, `crates/python`, `crates/worker`, `crates/worker-proto`, plus
docs/go/python. **`crates/cli`, `crates/types`, `crates/adaptive`,
`crates/switchyard` and `crates/pii-redaction` are byte-identical between the
two revs.** Every Relay claim in `SYNERGY-nemo-relay-deep-dive.md` §C.1–C.5
therefore stands verbatim at `ca08901`, re-verified below where it is
load-bearing.

The six: `#808` nested DeepAgents agent scopes (Python integration only),
`#810` h2 security patch, `#809` Event metadata injection for external plugins,
`#807` ChatNVIDIA tool-call preservation (Python), `#802` promote selected
Event metadata to OTel attributes, `#773` correlate managed tool calls by
external id.

**Switchyard `47babb1..5341f71` — 6 commits, and the public Rust surface is
unchanged.** `git diff 47babb1..5341f71 -- crates/libsy/src/lib.rs
crates/protocol/src/lib.rs` produces **empty output**. The delta is: `#484`
Python macOS tests, `#477` OpenClaw argument forwarding, `#474` populate the
`switchyard.router_retry_recovered` metric, `#479` align Python libsy streaming
contracts, `#387` preserve reasoning order in mixed stream chunks, `#462`
decode Anthropic structured output into requests.

So the 24-hour delta changes nothing. What follows in §1.2 is a set of
**findings the standing ruling did not have** — present at `47babb1`/`c37b551`
too, but not read. They are reported as corrections, not as upstream movement,
and that distinction matters for how the synthesis weighs them.

### 1.2 Eleven findings the standing ruling does not carry

1. **The published `v0.2.0` tag and main are two different libraries wearing
   one version number.** `crates/libsy/Cargo.toml` declares
   `version.workspace = true` → `0.2.0`; every doc pins
   `switchyard-libsy = { git = "…", tag = "v0.2.0" }` (`README.md:110`,
   `docs/getting_started.md:251`, `docs/reference/rust_api.md:26`,
   `crates/libsy/README.md:15`). crates.io has exactly one version, `0.2.0`,
   published **2026-08-10** (`https://crates.io/api/v1/crates/switchyard-libsy`).
   The tag's `lib.rs` (fetched from
   `https://raw.githubusercontent.com/NVIDIA-NeMo/Switchyard/refs/tags/v0.2.0/crates/libsy/src/lib.rs`)
   exports from `core::algorithm`: `Algorithm, CallLlmRequest, Driver,
   LlmCallObservation, LlmTarget, LlmTargetSet, RoutedRequest, RunObservation,
   RunObserver, Step, StepStream` — eleven names. Main exports `Algorithm,
   CallModel, Driver, RoutingOutcome, Step, StepStream, drive`
   (`crates/libsy/src/lib.rs:8`) — seven. **Four survive by name; seven were
   deleted; three are new.** The tag also has `NoopDecision`,
   `PassthroughDecision`, `RandomDecision` and a public `initialize_metrics()`,
   all gone on main; main has `AdvisorGate`, `AdvisorGateConfig`, `GateTrigger`
   and `ClassifierResponseFormat`, none of which exist at the tag. **`AdvisorGate`
   is not in any published release** — the CHANGELOG lists advisor-gate routing
   under `## [Unreleased]` (`CHANGELOG.md:7-19`).
2. **Switchyard ships its own coding-agent launcher.**
   `switchyard/cli/launch_command.py` plus
   `switchyard/cli/launchers/{codex_cli_launcher.py:248,
   claude_code_launcher.py:218, openclaw_launcher.py:269, shell_tui.py:419,
   cost_estimator.py:500, launcher_runtime.py:251, …}` — 2,503 Python lines
   that host an in-process native Rust server and drive Codex/Claude/OpenClaw
   against it. This is a **second** NVIDIA implementation of the launch surface
   roundhouse's M9 plans and Relay already ships. *[2026-08-21: **deleted
   upstream.** `43fc1a7` ("chore: Remove Python launchers and `switchyard`
   CLI", #501) removes `switchyard/cli/launch_command.py`, all of
   `switchyard/cli/launchers/`, `switchyard_cli.py`, and their tests — every
   file cited in this finding. There is no `switchyard launch` at HEAD
   (`053a61e`), and the three-launch-surface count in E2 is now two.]*
3. **`requires_openai_auth` is a switch upstream, not a constant.** Relay
   hardcodes `requires_openai_auth=true`
   (`crates/cli/src/agents/codex/launch.rs:199-205`). Switchyard's launcher
   sets it *conditionally*: `true` when the route forwards the caller's own
   OpenAI login, and `false` + `env_key="OPENAI_API_KEY"` (with a dummy
   `OPENAI_API_KEY="switchyard"`) otherwise
   (`switchyard/cli/launchers/codex_cli_launcher.py:79-91, 97-102`), selected
   from `server.caller_auth_kind(display_model)`
   (`codex_cli_launcher.py:169`, `native_server.py:56-58`,
   `crates/switchyard-server/src/lib.rs:279-283`). This is the missing input to
   PLAN §3's open caveat (`PLAN-agentic-control-plane.md:437-447`).
   *[2026-08-21: the Python half of this evidence is gone. `43fc1a7` (#501)
   deleted `codex_cli_launcher.py`, and `git grep requires_openai_auth HEAD`
   in Switchyard now returns nothing — **the flag appears nowhere in the tree
   at `053a61e`**. What survives is the *input* to the conditional, promoted to
   a public Rust API: `ServerHandle::caller_auth_kind(model) -> Option<&str>`
   (`crates/switchyard-server/src/lib.rs:324-329` @ `053a61e`), fed by
   `ClientFormat::caller_auth_kind` (`config.rs:338-343`: Anthropic-Messages →
   Anthropic, OpenAI-Chat/Responses → OpenAI) and by the route-level check that
   one route may not forward two credential families (`config.rs:213-221`). So
   the *hypothesis* — that this is a route property — is still supported, but
   Switchyard no longer states the codex-side conclusion anywhere. M7's
   verification item can cite `caller_auth_kind` as the route-property
   evidence; it can no longer cite a Switchyard line that writes
   `requires_openai_auth`, and any citation of `codex_cli_launcher.py` must be
   rev-qualified to `5341f71` or earlier.]*
4. **Switchyard has a production pass-through-auth implementation.**
   `LlmClientConfig.forward_auth: bool` (`crates/switchyard-server/src/config.rs:254`),
   mutually exclusive with `api_key_env` (`config.rs:873-877`); a route may not
   forward two providers' credentials (`config.rs:195-203`); the forwarded set is
   `authorization`, `chatgpt-account-id`, `x-openai-fedramp` for OpenAI and
   `authorization`/`x-api-key` (+ filtered `anthropic-beta` oauth values) for
   Anthropic (`crates/libsy-llm-client/src/backend.rs:190-210`); forwarding uses a
   **separate reqwest client with redirects disabled** so a credential cannot be
   moved to another origin (`crates/libsy-llm-client/src/client.rs:115-118,
   299-303`); values are marked `sensitive_header(...)`; and
   `redact_forwarded_auth` scrubs an echoed credential out of an upstream error
   body before it is returned or logged (`backend.rs:213-241`). Documented at
   `docs/reference/toml_schema.md:54-78` and `docs/getting_started.md:158-163`.
5. **Switchyard already implements roundhouse's `enforce_usage_reporting`
   rule.** `ensure_openai_stream_usage` adds `stream_options.include_usage =
   true` when `stream: true` and the key is absent, and **never overrides an
   explicit caller choice** (`.entry(...).or_insert(...)`) —
   `crates/libsy-llm-client/src/client.rs:786-806`. That is roundhouse's rule
   (`crates/roundhouse-fleet/src/usage.rs:98`) arrived at independently. Relay
   still lacks it: its only `stream_options` handling is classification as
   "portable" in the deprecated client
   (`crates/nemo-relay/crates/switchyard/src/translation.rs:168-173`).
6. **`crate::algorithms::escalation` does not exist.**
   `crates/libsy/src/algorithms/util/escalation.rs:7-8` documents "the
   confirmation policy … lives with the assembled algorithm in
   `crate::algorithms::escalation`", but `crates/libsy/src/algorithms.rs:9-16`
   lists only `advisor_gate, fall_through, llm_class, noop, passthrough, rand,
   stage, util`, and the path never existed in the visible history. The
   escalation router reaches the public surface **only** through
   `LlmTaskClassifier::new(LlmClassifierConfig::Escalation { … })`
   (`crates/libsy/src/algorithms/llm_class.rs:615-627, 649`). `build_judge`,
   `EscalationJudge`, `EscalationPolicy` and `EscalationClassifier` are all
   `pub(crate)`/private (`escalation.rs:99, 112, 119, 146`;
   `llm_class.rs:477`); only `EscalationJudgeConfig` is exported
   (`lib.rs:30`).
7. **`ToolSignals` is a public, pure, no-model-call trouble detector** — 16
   fields including windowed error `severity`, `no_error_streak`,
   `recent_edit/write/read/todowrite_count`, `pure_bash_streak`,
   `tests_passed`, `turn_depth` and `compacted`
   (`crates/libsy/src/algorithms/util/tool_signals.rs:206-246`), built by
   `ToolSignals::from_request(&Request, Option<usize>)`
   (`tool_signals.rs:253`), exported at `lib.rs:38`. Scoring is equally pure
   and public: `dimensions_from_signal` (`util/stage.rs:250`), `score_signal`
   (`stage.rs:323`), `pick_tier` (`stage.rs:373`). *`ToolSignalProcessor`
   (`tool_signals.rs:277`) is **not** re-exported — `mod algorithms;` is
   private at `lib.rs:17`, so only the explicit `pub use` list is reachable.*
   *[2026-08-21: **the count is 14, not 16** — `severity`, `no_error_streak`,
   `edit_count`, `write_count`, `read_count`, `todowrite_count`, the four
   `recent_*` counters, `pure_bash_streak`, `tests_passed`, `turn_depth`,
   `compacted` (`tool_signals.rs:206-246` @ `053a61e`). Everything else in this
   finding re-verified verbatim. Both files are **byte-identical** to
   `5341f71` (`md5 822def5b…` for `tool_signals.rs`, `da0b2402…` for
   `util/stage.rs`); `tool_signals.rs` was last touched 2026-08-03 by a
   docs-only commit (`e012780`) and `util/stage.rs` 2026-08-12 (`48b3b71`), so
   the port target did not move across 23 commits. `ToolSignals` and
   `DEFAULT_RECENT_WINDOW` are at `lib.rs:34`, the scorers at `lib.rs:38-42`.]*
8. **`switchyard-protocol::Metadata` normalizes every coding-agent correlation
   header roundhouse cares about.** `Metadata::from_headers`
   (`crates/protocol/src/metadata.rs:200-224`) resolves 15 fields —
   `session_id, agent_id, parent_agent_id, is_subagent, is_delegated_work,
   agent_kind, agent_role, task_id, task_kind, turn_id, session_final,
   correlation_id, served_model, extra_metadata, http_headers, wire_format`
   (`metadata.rs:159-196`) — from an alias table covering
   `x-codex-turn-metadata.{session_id,thread_id,parent_thread_id,turn_id,subagent_kind,agent_role,task_id,task_kind}`,
   `x-claude-code-{session,agent,parent-agent}-id`, `x-nemo-relay-session-id`,
   `x-dynamo-{session-id,parent-session-id,session-final}`, `x-openai-subagent`,
   and bare `session-id`/`thread-id`/`x-request-id`
   (`metadata.rs:15-65`). *[2026-08-21: the enumerated list is 16 names, not
   15 — an off-by-one in this document, not a change upstream; `Metadata` still
   carries exactly those 16 `pub` fields at `metadata.rs:160-196` @ `053a61e`
   and `crates/protocol/Cargo.toml` still names the same six dependencies, so
   the S-a cost figure is unchanged. What *did* move is the derivation: a new
   alias `x-codex-turn-metadata.thread_source` (`metadata.rs:19`) now makes
   `is_subagent` and `is_delegated_work` fire on **current** codex child
   lineage — `thread_source == "subagent"` **and** a parent id together
   (`metadata.rs:258-275`) — because, in upstream's words, "current Codex
   releases identify spawned children through lineage rather than
   `subagent_kind`". Adoption S-a therefore gets sub-agent detection for the
   codex the M9 box actually runs, which the `subagent_kind` path alone would
   have missed.]*
9. **ATIF is not in `nemo-relay-types`.** `AtifTrajectory` and
   `ATIF_SCHEMA_VERSION = "ATIF-v1.7"` live in
   `nemo-relay/crates/core/src/observability/atif.rs:55` — the heavy core
   crate. ATOF (`ATOF_VERSION = "0.1"`,
   `crates/types/src/api/event.rs:36`) and `LlmOptimizationSummary`
   (`crates/types/src/codec/optimization.rs:255`) *are* in types. So
   "emit ATOF/ATIF via `nemo-relay-types`" is half true; the ATIF structs must
   be re-implemented, exactly as the deep dive's D.1 row 2 said.
   *[2026-08-21 @ NeMo-Relay `1a54812`: **reaffirmed, still `ATIF-v1.7`, and one
   correction to what "the spec is published" means.** `ATIF_SCHEMA_VERSION` is
   still at `crates/core/src/observability/atif.rs:55`, `atif.rs` is
   **byte-identical** between `ca08901` and HEAD (3,645 lines both), and
   `git grep 'ATIF-v1\.'` over HEAD returns only `v1.7` — no v1.8. But the
   "afternoon of re-implementation from a published spec" (synergies/nemo-relay
   S2) rests on a source that is *Apache-2.0 Rust code*, not a normative field
   schema: `docs/configure-plugins/observability/atif.mdx` (529 lines) is a
   `plugins.toml` configuration doc, and the only field-level documents are
   `atif.rs` itself and NeMo-Agent-Toolkit's
   `packages/nvidia_nat_atif/atif-step-extra-guide.md` +
   `atof-to-atif-conversion-guide.md` (@ `c933737`). So the re-implementation is
   an attribution question of the same shape as the M6 judge-prompt port, not
   merely typing. Also: **the struct count is twelve, not "~15"** (D.1 row 2 of
   the older dive) — `AtifTrajectory:301`, `AtifAgentInfo:63`, `AtifStep:81`,
   `AtifMetrics:122`, `AtifFinalMetrics:151`, `AtifToolCall:174`,
   `AtifObservation:188`, `AtifObservationResult:195`,
   `AtifSubagentTrajectoryRef:212`, `AtifAncestry:226`, `AtifInvocationInfo:244`,
   `AtifStepExtra:267`; `AtifExporter:345` is the host-side plugin, not a wire
   type. The Python side adds a thirteenth, `AtifToolCallExtra`
   (`nvidia_nat_atif/src/nat/atif/atif_step_extra.py:168`), with no Rust
   counterpart. And the ATOF half of the sentence needs one qualifier of its
   own: `ATOF_VERSION = "0.1"` is in the published 0.7.3
   (`0.7.3:src/api/event.rs:34`), but `METRIC_DATA_SCHEMA_VERSION` — cited by
   the older dive at `event.rs:45` — is **0.8-only**, so which ATOF constants
   are "in types" depends on which version you pin. See finding 11's note.]*
10. **Relay's `PricingCatalog` is also core-only.**
    `crates/core/src/codec/model_pricing.rs:84` (825 lines), with
    `aliases:274`, `rate_schedule:286`, `prompt_cache:288`,
    `pricing_as_of:290`, `pricing_source:292`, and non-empty validation of the
    two provenance fields at `:389-390`. There is no standalone pricing crate.
    `nemo-relay-pii-redaction` *is* a separate crate but hard-depends on
    `nemo-relay` core (`crates/pii-redaction/Cargo.toml:21`).
11. **`nemo-relay-types` on crates.io is one minor behind the tree.**
    max_version **0.7.3**, published 2026-08-14
    (`https://crates.io/api/v1/crates/nemo-relay-types`); the tree is 0.8.0 and
    unpublished. `crates/types/src` carries a `feat!` breaking commit in the
    window (`56158e4 feat!: add canonical tool execution results (#575)`,
    2026-08-13).
    *[2026-08-21: **superseded — 0.8 is published now, and the unlock condition
    in the round-2 ruling has fired.** `nemo-relay-types 0.8.0-rc.1` went to
    crates.io at **2026-08-21T02:53:49Z**, about twenty hours before this
    re-read (`https://index.crates.io/ne/mo/nemo-relay-types`, cross-checked
    against the v1 API with a `User-Agent` — without one crates.io returns a
    data-access-policy error, not data). `max_stable_version` is still `0.7.3`;
    `max_version` and `newest_version` are `0.8.0-rc.1`; 0.8.0-final is not out,
    and **that** is the condition to record next to whatever pin we take. Its
    twelve source files are **byte-identical to `ca08901:crates/types/src`**,
    verified file by file — so the `56158e4` `feat!` this finding names is now
    on crates.io. The substance of "crates.io lags the tree" survives, because
    the tree is at workspace `0.9.0` (`HEAD:Cargo.toml:23`) and `api/registry.rs`
    (+105) landed after `0.8.0-rc.1` was cut; only the numbers moved. Two facts
    that decide whether the pin must actually move: **(a)**
    `codec/optimization.rs` is **byte-identical across 0.7.3, 0.8.0-rc.1 and
    HEAD**, as is the entire ATOF envelope (`BaseEvent`, `ScopeEvent`,
    `MarkEvent`, `Event`, `DataSchema`, `ScopeCategory`) — so for emitting
    summaries and scope/mark events, 0.7.3 is not a compromise, it is
    equivalent; **(b)** the whole metric-mark surface — `MetricEnvelope`,
    `MetricMeasurement`, `InstrumentDescriptor`, `MetricAttributes`,
    `validate_metric_measurements`, `LogSeverity`, and the constants
    `METRIC_DATA_SCHEMA_NAME`/`_VERSION` — exists **only from 0.8**
    (`0.8.0-rc.1:src/api/event.rs:36-723`; `0.7.3:src/api/event.rs` is 865 lines
    to 0.8's 1,577 and has none of them). The 0.7.3 pin is sufficient for S2 as
    written and insufficient the moment metrics marks are in scope. Other
    0.8-only content, none of it load-bearing for an emit-only consumer:
    `CategoryProfile.tool_result_annotation` (the `feat!` itself, additive
    `Option<Json>`), `api/tool.rs`'s `ToolExecutionResult` +
    `TOOL_EXECUTION_RESULT_SCHEMA`, `codec/identity.rs` (74 lines, a whole new
    module), `ApiSpecificResponse::{OCIGenAI, GeminiGenerateContent}`, and one
    behavioural tightening — `CostEstimate::total_or_component_sum` now returns
    `None` unless `source == ProviderReported`
    (`0.8.0-rc.1:src/codec/response.rs:151-158`), i.e. Relay independently
    hardened the same measured/estimated boundary this document's §5 argues
    about.]*

---

## 2) Heavy-adoption candidates, with API evidence and an honest ledger

Each candidate below answers four questions: **what the API actually is**,
**the dedup win** (what we delete or never build, in files/lines/tests), **the
coupling cost** (churn quantified, breaking changes listed), and **the
invariant strain** (which roundhouse invariant it pushes on).

Roundhouse's four load-bearing invariants, for reference:
single-writer log (`crates/roundhouse-core/src/store/contract.rs:46-131`,
lease-fenced), narrow-only policy (`TurnPolicy::narrow` is total and can only
shrink — `validate/verdict.rs:311-343`), measured/estimated separation
(`metrics/mod.rs:34-38`, `Accounting::{Measured,Estimated}`), and no-fail-open
(`interject.rs:236-241` — the `Interjector` trait has no error arm *by
construction*; and `validate/mod.rs:26-31`).

---

### (a) `switchyard-libsy` as a real `RoutingPolicy` implementation

**What the API is.** Our seam:
```rust
// crates/roundhouse-core/src/routing/mod.rs:536-542
pub trait RoutingPolicy: Send + Sync {
    fn name(&self) -> &str;
    async fn choose(&self, ctx: &RoutingContext<'_>) -> Result<Decision, RoutingError>;
}
```
Theirs:
```rust
// switchyard crates/libsy/src/core/algorithm.rs:353-362
pub trait Algorithm: Send + Sync + 'static {
    fn name(&self) -> &str;
    async fn route(self: Arc<Self>, driver: Driver, request: Request) -> Result<RoutingOutcome>;
}
```
`RoutingOutcome` (`algorithm.rs:64-72`) carries `selected_model_id`,
`fallback_models`, the rewritten `request`, and `response: Option<Response>`.
`RoutingOutcome::route_to` (`:79`) is the host-drives-the-call constructor;
`RoutingOutcome::answered` (`:95`) is the algorithm-already-called-the-model
one. `drive(algorithm, request, serve)` (`:231`) is the host loop that serves
each `Step::CallModel` (`:211`).

**The decisive structural split.** `grep -c call_model` per algorithm at
`5341f71`:

| Algorithm | `call_model` refs | Can be a pure decision function? |
|---|---|---|
| `Noop`, `Passthrough`, `Random`, `StageRouter`, `FallThrough`, `AffinityRouter`, `StageClassifier` | 0 | **yes** |
| `LlmTaskClassifier` (`llm_class.rs`) | 1 (+1 in `util/llm_judge.rs`) | no — calls the judge and the efficient tier |
| `AdvisorGate` | 2 | no — owns the executor call |

So the libsy algorithms that fit behind `RoutingPolicy::choose` are exactly the
ones whose logic roundhouse already has (affinity, tiering, random), and the
ones carrying novel content are precisely the ones that must own dispatch.

**What our `RoutingContext` would lose.** `switchyard_protocol::Request` is
`{ llm_request, raw_request: Option<Value>, metadata: Option<Metadata> }`
(`crates/protocol/src/envelope.rs:12-21`). It has **no slot** for any of
`RoutingContext`'s quantitative fields
(`crates/roundhouse-core/src/routing/mod.rs:162-190`): `isl_tokens`,
`candidates: &[Candidate]` (each with `expected_prefill_tokens`,
`matched_prefix_tokens`, `expected_ttft_ms`, `expected_cost_usd`,
`quality_prior`, `load` — `mod.rs:121-147`), `ledger: &CacheLedger`,
`turn_policy: &TurnPolicy`, `frontier_history`, `budget: &TurnBudget`. The only
escape hatch is `Metadata.extra_metadata: Option<BTreeMap<String,String>>`
(`metadata.rs:190`) — stringly-typed. `LlmRequest` (`protocol/src/llm.rs:306-334`)
carries model/instructions/messages/tools/sampling/output/reasoning/stream and
nothing about price or capacity.

**What it would gain.** `StageRouter`'s signal-driven tier selection
(`stage.rs:590` lines, plus `util/stage.rs:934`), `AffinityRouter`
(`util/affinity.rs:771`), handoff notes (`HandoffNoteConfig`,
`util/stage.rs:440-470`) with the `only_on_wrong_signal_escalation` default
that avoids telling a model it was escalated to on an ambiguous turn
(`stage.rs:451-455`).

**Dedup win, honestly.** `crates/roundhouse-core/src/routing/policy.rs` is 999
lines total, of which `AffinityPolicy` is `:97-210` and `EscalationPolicy` is
`:227-289` — roughly **190 lines of production code**, the rest being
`Weights`, docs, and a 700-line test module. `policy_routing.rs` (837 lines)
tests behaviour we would still have to test through the adapter. The
realistic deletion is ~190 lines and zero tests, because every test asserts a
roundhouse-specific property (budget state, overflow valve, policy clamp) that
libsy has no vocabulary for.

**Coupling cost.** `switchyard-libsy` deps (`crates/libsy/Cargo.toml:20-37`):
`async-trait, serde, serde_json, futures, http, jsonschema 0.49.4, jsonptr
0.8.1, opentelemetry 0.32 (metrics), parking_lot, rand 0.10, regex,
switchyard-protocol, thiserror, tokio, tokio-stream 0.1, tracing,
tracing-opentelemetry 0.33`. Against roundhouse's `Cargo.lock`: `jsonschema`
and `jsonptr` are **new** to the graph; `opentelemetry 0.32` and
`tracing-opentelemetry 0.33` are **new to the production graph** — our lock has
`opentelemetry 0.31.0` (`Cargo.lock:3193-3204`) pulled only by
`codex-http-client`, a **dev**-dependency, so adopting libsy puts two
semver-incompatible OTel lines in one tree. `rand` already resolves at both
0.9.5 and 0.10.2 (`Cargo.lock:3925, 3935`), so that unifies.
`switchyard-protocol` alone is far lighter: `async-trait, futures, http, serde,
serde_json, thiserror` (`crates/protocol/Cargo.toml:18-24`) — every one already
in our tree.

**Invariant strain.** *Single-writer log*: `libsy::State`
(`crates/libsy/src/core/state.rs:1-34`) is still 34 lines, still no
`Serialize`, no store trait, no snapshot — re-verified at `5341f71`. Session
state is a process-local `Mutex<HashMap<String, SessionState<S>>>` with a
one-hour TTL and a one-hour cleanup interval
(`crates/libsy/src/algorithms/fall_through.rs:40, 46, 49, 296-306`), and
`AffinityRouter` adds a second `Mutex<HashMap<RoutingIdentity, ModelId>>`
(`util/affinity.rs:58`). The README's blocker sentence
(`roundhouse/README.md:239-244`) stands re-verified. *Narrow-only*: `Algorithm`
returns a target directly; nothing composes through a lattice, so the clamp
would have to be re-applied outside libsy — which is what our
`Admitted::decide` already exists to make unavoidable
(`routing/mod.rs:225-245`).

---

### (b) `AdvisorGate::new` beside/instead of our M6 `Validator`

**What the API is.**
```rust
// switchyard crates/libsy/src/algorithms/advisor_gate.rs:198
pub fn new(executor: ModelId, advisor: ModelId, config: AdvisorGateConfig) -> Result<Self>
```
`AdvisorGateConfig` (`:103-133`): `reviewer_system_prompt`,
`redo_feedback_prefix`, `gate_trigger: GateTrigger`, `max_reviews`,
`gate_stall_turns`, `gate_min_tool_results`, `advisor_max_tokens`,
`advisor_temperature`, `transcript_max_chars`, `fail_open`. `GateTrigger`
(`:92-99`) is `NoToolCall | Pattern(String)`.

**The structural blocker.** `AdvisorGate` implements `Algorithm`, and its
`route` **calls the executor itself**:
```rust
// advisor_gate.rs:336-339
let response = driver.call_model(request.clone(), vec![self.executor.clone()]).await?;
let turn = buffer_turn(self.executor.as_str(), response).await?;
```
Roundhouse's `Interjector` is consulted **before the turn is planned**, is
denied the candidate list on purpose, and cannot dispatch
(`crates/roundhouse-core/src/interject.rs:19-25, 165-170`). Our
`RoutingPolicy::choose` is required to be pure and returns a `Decision`, not a
`Response` (`routing/mod.rs:530-535`). **`AdvisorGate` fits neither seam
without inverting control of dispatch** — it is a *replacement for the engine's
turn loop*, not an occupant of a hole in it.

**Field-by-field: what it would replace, and what would be lost.**

| Roundhouse M6 | `AdvisorGate` equivalent | Verdict |
|---|---|---|
| `Trigger` + 4 pure signals (`NoProgressRepeat`, `PingPong`, `ToolFailureStreak`, `CostAnomaly` — `validate/trigger.rs:62-78, 168-333`) | `GateTrigger::NoToolCall` (+`gate_min_tool_results`) \| `Pattern(regex)` \| `gate_stall_turns` (`advisor_gate.rs:344-357`) | **Different axis.** Theirs is a *boundary* trigger (the turn ended); ours are *trajectory* signals. Complementary; neither subsumes the other. |
| Per-session budget: `TriggerConfig{tokens_between_validations: 20_000, cooldown_ms: 60_000, max_consecutive_interventions: 2, max_validations_per_session: 8}` read out of the log (`trigger.rs:349-377`) | `max_reviews` per `ScopeKey` in `Mutex<GateState>` with `MAX_TRACKED_SCOPES = 1_024` and LRU-ish eviction (`advisor_gate.rs:84, 179-182, 260-279`) | **Lost:** replay-stability. Ours is a projection of the log and survives restart exactly; theirs re-arms on process restart, which its own comment accepts as "rare, harmless" (`:82-84`). |
| Node budget: `ReviewBudget{max_in_flight: 8, max_consecutive_failures: 3, breaker_cooldown_ms: 60_000}` with CAS reservation and a **half-open breaker** (`validate/mod.rs:241-405`) | `try_reserve`/`refund_failure` + hardcoded `MAX_FAILED_CONSULTS: u32 = 3` (`advisor_gate.rs:81, 260-289`) | **Ours is the superset.** Theirs has no in-flight cap, no re-arm, and the failure cap is a `const` not a knob — once tripped, that scope never consults again for the life of the process. |
| Brief: `ValidationBrief::build/render` (`validate/brief.rs:152-230`), bounded, step-indexed, with `Objective` | `review_transcript` = `serde_json::to_string(messages)` + middle-drop at `transcript_max_chars` (default 200_000) (`advisor_gate/transcript.rs:35-73`) | **Lost:** the step index the judge's `Divergence.at_step` refers to (`verdict.rs:70-79`), and the declared objective. |
| Verdict: strict typed JSON, `#[serde(deny_unknown_fields)]`, unknown-field refusal, missing-field refusal, range-checked `confidence` (`validate/verdict.rs:52-118`) | Anchored regex over free prose: `(?i)^[\s*_#>"'(\[`]*(?:(?:final\s+)?verdict\s*:\s*…)?(APPROVE\|REDO)\b` (`transcript.rs:18-19`), plan = the remainder of the reply (`:84-97`) | **Lost:** the injection-hardened structured verdict. Their REDO plan is **fed to the executor verbatim** (`advisor_gate.rs:424-427`, and the prompt says so: `reviewer-system-prompt.md` "it will receive your words verbatim"). Ours forbids that by construction — `verdict.rs:17-27`: "The judge's prose never reaches the agent — not quoted, not fenced, not truncated, not at all." |
| Action map: `Continue \| Escalate \| Steer \| Halt`, evidence-ordered, clamped through `TurnPolicy::narrow` (`verdict.rs:249-292, 311-343, 377-420`) | One action: REDO — append the discarded turn's text as an assistant message plus the plan as a user message, then re-invoke (`advisor_gate.rs:414-433`) | **Lost:** escalation (change *who serves*), the synthetic tool call, and the policy clamp. |
| Arms `Live \| Shadow \| Placebo`, stamped in `SessionCreated`, hashed per session (`validate/arm.rs:51-196`) | **none** | **Lost entirely.** Confirmed absent from both libsy judge algorithms at `5341f71`. |
| Spend integration: judge call reserves against the budget ledger *before* the deadline and settles on every exit path (`roundhouse-server/src/judge.rs:461-497`) | Review counters and OTel token counters only (`advisor_gate/telemetry.rs:23-67`); **no dollars anywhere in the Rust tree** | **Lost:** the whole money seam. |
| `Interjector` has no error arm; a failure is a logged fact plus `Proceed` (`interject.rs:236-241`) | `fail_open: bool`, **default `true`** (`advisor_gate.rs:132, 147`); on failure the buffered turn passes through as an implicit APPROVE (`:494-506`) | **Direct collision with no-fail-open.** Note the shapes differ: our "release the turn" is *not* an approval — no verdict is recorded, the record is marked (`validate/mod.rs:26-31`). Theirs records `verdict: "APPROVE"` in the audit line on a failed consult (`advisor_gate.rs:499-505`). |

**What it would genuinely give us.** Two things, both already ported as ideas
in S5 and both re-verified present: **discarded-work accounting**
(`record_discarded` / `emit_discarded_audit`, `telemetry.rs:45-67, 123-148` —
counts input/cached/cache_creation/output tokens for a turn the client never
saw) and the **anchored verdict parse** (`transcript.rs:14-19`, whose comment
states the exact failure our `verdict.rs:29-31` cites). Also new-to-us:
`gate_stall_turns` as a mid-task checkpoint that fires *even on a tool-call
turn* (`advisor_gate.rs:341-347`) for an executor that grinds without ever
declaring completion, latched per conversation on a hash of the first user
message (`stall_key`, `:633-644`).

**Dedup win.** If `AdvisorGate` replaced our validate loop wholesale:
`crates/roundhouse-core/src/validate/` is 7,853 lines across 8 modules + the
181-line judge prompt + `roundhouse-server/src/judge.rs` (1,047) +
`tests/validate_loop.rs` (1,548). But the table above says the replacement is
not like-for-like on any row that matters. A *partial* adoption — the gate's
trigger and stall checkpoint as two more `Signal` implementations behind
`Validator::with_signals` (`validate/mod.rs:513`) — costs nothing and deletes
nothing.

**Coupling cost.** `REVIEWER_SYSTEM_PROMPT` and `REDO_FEEDBACK_PREFIX` are
`pub const` at `advisor_gate.rs:59-67` but **not re-exported at the crate root**
(`lib.rs:19` exports only `AdvisorGate, AdvisorGateConfig, GateTrigger`) — they
are reachable only as the field values of `AdvisorGateConfig::default()`
(`:135-150`). `AdvisorGate` exists in **no published release** (finding 1.2.1).

---

### (c) `EscalationClassifier` via `LlmTaskClassifier` as the latch-on-trouble `EscalationPolicy`

**What the API is.** Publicly constructible:
```rust
// switchyard crates/libsy/src/algorithms/llm_class.rs:602-627, 649
pub enum LlmClassifierConfig {
    Capability { … },
    Escalation {
        judge_target: ModelId, efficient_target: ModelId, capable_target: ModelId,
        contract: ClassifierContractConfig, config: EscalationJudgeConfig,
        max_output_tokens: u64,
    },
    Custom { … },
}
impl LlmTaskClassifier { pub fn new(config: LlmClassifierConfig) -> Result<Self> }
```
`EscalationJudgeConfig` (`util/escalation.rs:48-58`, exported at `lib.rs:30`):
`confirmations` (default 2), `recent_turn_window` (28),
`window_message_chars` (500).

**How it works.** `EscalationClassifier::score`
(`llm_class.rs:477-584`): a confirmed session stays capable with no judge call
(`:502-505`); otherwise it calls the **efficient** model, buffers the response,
appends the reply to the transcript, asks the judge, and on a confirmed streak
**discards the efficient response** and routes to capable (`:568-573`). The
streak lives in `State.extra["escalation_streak"]` as a `Count`
(`llm_class.rs:448, 452-456`). The outage arm holds:
```rust
// llm_class.rs:568-571
Some(score) if score.target == self.capable => (true, held + 1),
Some(_)                                     => (false, 0),
None                                        => (false, held),   // outage HOLDS the streak
```
The judge itself is a `StructuredJudge` with a JSON-schema `response_format`
(`util/llm_judge.rs:169`, `util/classifier_contract.rs:15-57`), a 178-line
prompt (`crates/libsy/src/prompts/escalation/prompt.md`) and a 22-line schema,
condensing the trajectory with per-role caps — `SYSTEM_CHARS = 1_000`,
`FIRST_USER_CHARS = 2_000`, `MAX_REQUEST_CHARS = 18_000`
(`util/escalation.rs:34-40, 243-302`).

**Dedup win.** Our `EscalationPolicy` is 63 lines of production code
(`crates/roundhouse-core/src/routing/policy.rs:227-289`) and its docstring
already names this exact algorithm as the richer version it approximates
(`policy.rs:214-226`). Adopting it deletes those 63 lines and closes the
`audit_every` caveat — **but only if we also accept that libsy owns the
efficient-tier call**, which is candidate (b)'s blocker again.

**Coupling cost.** `EscalationClassifier` is private; only the `LlmClassifierConfig`
enum reaches us, and it is `#[non_exhaustive]` (`llm_class.rs:604`) — so a new
variant upstream is not itself breaking, but the `Escalation` variant's field
set is. That variant did not exist in the v0.2.0 tag's shape (`ClassifierContractConfig`
existed; `ClassifierResponseFormat` did not).

**Invariant strain.** *Single-writer log*: the streak is `State.extra`, i.e.
the process-local map with a one-hour TTL (§2a). A roundhouse session that
crashes and replays would lose its latch, which is precisely the property our
`FrontierHistory` projection exists to make impossible. *Measured/estimated
separation*: the discarded efficient turn is **paid for and never accounted** —
unlike `AdvisorGate`, `EscalationClassifier` has no `record_discarded` call at
all (`grep 'record_discarded' llm_class.rs` → no match). *No-fail-open*: the
outage arm holding the streak is the right instinct and matches ours; but
`ContextWindowExceeded` and transport failures on the efficient tier fall
through to capable silently (`llm_class.rs:522-534`), which is a fail-*open* on
cost rather than on quality.

---

### (d) Relay CLI as the blessed front end — chained topology promoted to default

**Re-verified at `ca08901`, all three E1 hazards live.**

1. **The upstream override still routes around a configured base URL.**
   `crates/cli/src/gateway/request.rs:63-70` computes
   `gateway_upstream_url_override(...)` **first** and only falls back to
   `provider.upstream_url(config, path_and_query)`. That override is
   `crates/cli/src/gateway/routes.rs:189-220` →
   `crates/cli/src/agents/shared/alignment.rs:424-436` →
   `crates/cli/src/agents/codex/alignment.rs:85-93`:
   ```rust
   (is_openai_route(route) && has_chatgpt_auth_token(headers) && !has_replacement_key)
       .then(|| chatgpt_upstream_url(path_and_query))
   ```
   with `CHATGPT_CODEX_BASE_URL = "https://chatgpt.com/backend-api/codex"`
   (`alignment.rs:23`). **It does not consult the configured base URL at all.**
   So `nemo-relay --openai-base-url https://roundhouse.internal/v1 codex`, run
   by an operator on ChatGPT device login with no `OPENAI_API_KEY`, sends the
   turn to `chatgpt.com` and roundhouse never sees it.
2. **The JWT strip is the mutually exclusive other half.**
   `strip_chatgpt_auth_for_openai_route` (`alignment.rs:97-110`) removes the
   `Authorization` header when `has_replacement_key` — and
   `has_openai_replacement_auth` is true when
   `allow_environment_provider_auth && openai-family route && (configured
   auth header || non-empty OPENAI_API_KEY)` (`routes.rs:257-270`). So the two
   guards partition the space: **JWT + no key → route around us; JWT + key →
   the key pays instead of the seat.** Under roundhouse's `PassThrough`
   (`PLAN-agentic-control-plane.md:419-433`) the second case silently changes
   who pays, and nothing on the wire tells us it happened.
3. **`requires_openai_auth = true` is still hardcoded**
   (`crates/cli/src/agents/codex/launch.rs:199-205`), against PLAN §3's
   leave-unset ruling read at codex `6344a65`
   (`PLAN-agentic-control-plane.md:437-447`). Switchyard's conditional
   treatment (finding 1.2.3) is the third data point and suggests the flag is a
   *route property* — set it when the upstream should receive the caller's own
   login — rather than a client-version fact. That reframing, if right, makes
   both prior readings correct for their own configuration and turns M7's first
   verification item into a design decision rather than a contradiction.

**A fourth hazard, new.** Relay decodes and reserializes JSON on the managed
path (`crates/cli/src/gateway/request.rs:96-99` and the "Option B" re-encode at
`gateway/mod.rs:70-76`). Our prefix admission compares role + content only
(`crates/roundhouse-server/src/responses_api.rs:353-370`), which *should* be
robust — but Switchyard's own tree records what goes wrong when a proxy mutates
a normalized request and a codec replays the preserved inbound body instead:
`drop_exact_replay` (`crates/libsy/src/algorithms/util/prompts.rs:56-71`),
whose comment says "a future processor that mutates the request and forgets the
call reintroduces SWITCH-1224, silently and without a failing test". That is a
live class of bug in exactly the chained position, and it argues the S3 guard-3
re-encoded-history test is not optional.

*[2026-08-21: **this hazard fired upstream, twice, and one instance would have
broken a roundhouse chain outright.*** *Switchyard's Responses encoder replays
the preserved inbound body verbatim when source and target format match
(`codecs/responses/buffered.rs:118-131`, default policy `InMemory` at
`policy.rs:88`), so a pure Responses→Responses hop was always lossless — but
any hop that mutates the IR calls `drop_exact_replay` and falls to the
reconstructing encoder, and until `053a61e` ("fix: re-emit captured provider
extensions when encoding to the Responses format", #509, **2026-08-21 16:45
UTC — the current HEAD**) that encoder had no extensions allowlist and dropped
`prompt_cache_key` on the floor. Roundhouse does not merely read
`prompt_cache_key`: it **requires** it and answers 422 without one
("`prompt_cache_key` is required: it names the session",
`responses_api.rs:236-240`). So `codex → Switchyard (with a target system
prompt) → roundhouse` returned 422 on every turn at any Switchyard revision
before today. Fails loudly rather than forking sessions silently, which is the
good direction — but it is a total chain break, and the fix is one commit old.
The second instance is `0acde7b` (#439, 2026-08-20): serde_json's default
BTreeMap-backed `Map` alphabetized every proxied JSON object, which is semantic
for `response_format.json_schema` on order-enforcing backends; fixed by
enabling `preserve_order`. Not a roundhouse defect today — this surface reads a
typed request and forwards no `response_format` (`responses_api.rs:174-196`) —
but it is the named hazard for the day it does, and roundhouse does not enable
`preserve_order`.]*

**What the topology deletes from M9.** M9's stated burden
(`PLAN-agentic-control-plane.md:1024-1032`) is "a generated config with both
entries sharing one env var, a scripted task, a forced steer" plus the three
tests. Relay's launch surface that would replace the config generation is
`crates/cli/src/agents/` — 6,301 lines, of which `codex/launch.rs` is 235 and
`claude/launch.rs` 202. **But the one test M9 exists for —
`a_real_codex_binary_executes_our_synthetic_tool_call_and_returns_its_output`
— is not deleted by any front end**, because Relay cannot inject a tool call
(§4) and the assumption being closed is about Codex's dispatch and resend, not
about who wrote the config.

**A second front end now exists.** Switchyard's `switchyard launch
codex|claude|openclaw` (finding 1.2.2) does the same job in 2,503 lines of
Python, hosting a native Rust server in-process (`launchers/native_server.py`).
It carries a guard Relay's does not: `wait_for_proxy_ready` deliberately
bypasses `HTTP_PROXY`/`NO_PROXY` for the loopback health probe, with a
regression test (`tests/test_launcher_proxy_bypass.py:22-40`).

---

### (e) Emitting ATOF / ATIF / `LlmOptimizationSummary` now via `nemo-relay-types`

**Weight re-checked at `ca08901`.** `crates/types/Cargo.toml:19-26`:
`bitflags 2`, `chrono 0.4` (default-features off), `schemars 0.8` (optional,
off by default), `serde 1`, `serde_json 1`, `typed-builder 0.23.2`,
`uuid` (workspace, `=1.18.1` at `Cargo.toml:44` — **identical to roundhouse's
pin**, `roundhouse/Cargo.toml:80`). 4,506 lines. This is still the only cheap
Relay import.
*[2026-08-21: three corrections, one of which changes the cost. **(i) The
`uuid` claim no longer holds.** Roundhouse declares
`uuid = { version = "1.18.1", features = ["v4","serde"] }` at
`Cargo.toml:95` (the line moved from `:80`) — a *caret* requirement, currently
resolved to **1.24.0** (`Cargo.lock:5758-5759`). Relay's is an **exact** pin,
`uuid = "=1.18.1"` (`HEAD:Cargo.toml:41`, and the same in both published
tarballs), so adopting the crate forces the whole graph down six releases and
imposes a ceiling: any future dependency wanting `uuid ≥ 1.19` becomes
unresolvable while the pin stands. It *does* resolve today — every uuid
dependent in our lock is satisfied by 1.18.1 (`moka 0.12.16` → `1.1`,
`rmcp 3.1.3` and `ts-rs 11.1.0` → `1`, `rama-http 0.3.0-alpha.4` → `1.18`,
codex `6344a65` → `1`, Dynamo `ac7b751` → `^1.18.1`), argued from requirement
strings rather than a resolver run since no cargo build was available. This is
exactly the CLAUDE.md "record the unlock condition next to the pin" case and
belongs in the manifest comment the way redis 1.2.4 names Dynamo's
`tokio = "=1.48.0"`. **(ii) The line count is a method difference, not an
error to fix**: `crates/types` measures **4,659** lines including tests,
fixtures and README at *both* `c37b551` and `ca08901`, and **3,555** for
`src/` alone; the published 0.7.3 tarball is **2,549**. **(iii) Two new crate
names, not zero**: `typed-builder 0.23.2` → `typed-builder-macro =0.23.2` →
`proc-macro2`/`quote`/`syn`, the last three already in our lock. `bitflags`'s
`serde` feature is additive over our existing 2.13.1, and `schemars` stays off
(`default = []`). Still no TLS, no reqwest, no OpenSSL, no OTel — "the only
cheap Relay import" is intact. **MSRV is undeclared**: no `rust-version` key in
either tarball and `rust_version: null` on the crates.io API for every
version, so do not assert 1.96.1; edition 2024 implies ≥1.85 and 1.96.1 is
merely Relay's dev toolchain.]*

**What is actually in it.** ATOF: `ATOF_VERSION = "0.1"`
(`crates/types/src/api/event.rs:36`), `METRIC_DATA_SCHEMA_VERSION = "1"`
(`:45`). Savings: `LlmOptimizationSummary` (`crates/types/src/codec/optimization.rs:255`)
with `status:261`, `limitations:264`, `baseline_model:267`,
`effective_model:270`, `estimated_cost_saved:287`;
`LlmOptimizationEvidenceQuality` (`:143`); `LlmOptimizationContribution`
(`:180`).
*[2026-08-21: **all of these line numbers are the tree's (= 0.8's); against the
published 0.7.3 the optimization ones hold exactly and the ATOF ones do not.**
`codec/optimization.rs` is byte-identical across 0.7.3, 0.8.0-rc.1 and HEAD, so
`:255/:261/:264/:267/:270/:287/:143/:180` all resolve in the 0.7.3 tarball too.
`api/event.rs` does not: `ATOF_VERSION` is at `:34` in 0.7.3 (not `:36`), and
**`METRIC_DATA_SCHEMA_VERSION` does not exist there at all** — it and the
entire metric-mark surface arrive with 0.8. Two further facts the field list
above does not show, both load-bearing for the contribution plan and for S4's
"carry the gate's result in `limitations[]`": **`limitations` is a free-form
`Vec<String>`** (`optimization.rs:264`, doc "machine-readable reasons"), so the
plan is expressible — Relay's own producer uses a snake_case vocabulary
(`missing_baseline_model`, `missing_baseline_pricing`, `cost_currency_mismatch`,
…) with an `add_limitation` seam that inserts arbitrary strings
(`HEAD:crates/core/src/api/optimization.rs:290,309`); but **`status` is derived,
not chosen** — Relay writes
`status: if limitations.is_empty() { Complete } else { Partial }` (`:841-845`),
so every roundhouse summary that names an `Unpriced` correlary publishes as
`Partial`. Also worth recording as the sanctioned extension point, since it is
in the types crate and not core: `LlmOptimizationPayload`
(`optimization.rs:171-176`) with `const SCHEMA_NAME`/`SCHEMA_VERSION`, consumed
by `Contribution::with_payload` (`:230`), is where `capability_band`,
`PricedBasis`, `routing_savings_at_decision_usd` and `seat_tokens` — none of
which have a `LlmOptimizationSummary` field at any version — can ride typed.]*

**What is not.** ATIF (`ATIF_SCHEMA_VERSION = "ATIF-v1.7"`,
`crates/core/src/observability/atif.rs:55`) is in **`nemo-relay` core**, which
the dependency rule forbids. Both format versions are unchanged since the deep
dive — ATOF still 0.1, ATIF still v1.7 — which is the one genuinely *stable*
thing in either tree.

**Coupling cost.** crates.io max is **0.7.3** (2026-08-14) against a tree at
0.8.0, so a crates.io pin lags the tree by a minor and by at least one `feat!`
(`56158e4`, canonical tool execution results, 2026-08-13). Churn on
`crates/types/src` is **6 commits in 16 days** — the lowest-churn surface in
either external tree, and roughly a fifth of libsy's rate (§3.1).
*[2026-08-21: **the "lags by a minor and a `feat!`" cost is gone** — see the
note at finding 11. `0.8.0-rc.1` (published 2026-08-21T02:53:49Z) is
byte-identical to `ca08901:crates/types/src`, so the `feat!` is on crates.io.
The three forms of the pin, priced, since round-2's version-identity rule
("pin a git rev, never a version or a tag") and S2's "pin the published 0.7.3"
appear to disagree — they do not, narrowly: the rule's justification is that a
tag or a version can *rename* an API over time, and crates.io versions are
immutable, so `=x.y.z` there is as reproducible as a rev. What is never
reproducible is a caret against a 0.x crate.
**A** `nemo-relay-types = { version = "=0.7.3", default-features = false }` —
stable channel, byte-equal to HEAD for optimization + ATOF envelope, no metric
marks. **B** `= "=0.8.0-rc.1"` — note the `=` is mandatory: a bare `"0.8"`
will not resolve to a pre-release. **C**
`{ git = "https://github.com/NVIDIA/NeMo-Relay", rev = "513b7da" }` (the
`0.8.0-rc.1` tag) — the only form that can reach `api/registry.rs` and the only
one that needs no upstream release to move. Note the repository URL while it is
in front of us: the crate metadata says `NVIDIA/NeMo-Relay`
(`0.7.3/Cargo.toml:23`), **not** `NVIDIA-NeMo/NeMo-Relay`, which 404s;
Switchyard really is under `NVIDIA-NeMo/`. Whichever form, the manifest comment
must carry both the `uuid = "=1.18.1"` ceiling (note at the top of this
subsection) and the unlock condition **"`nemo-relay-types 0.8.0` final on
crates.io"** — time-sensitive, because HEAD is a `release/0.8` merge dated
today and the repo cuts a `0.8.0-alpha.YYYYMMDD` tag daily
(`0.8.0-alpha.20260802` … `0.8.0-alpha.20260821`).]*

**Invariant strain: none, with one caveat.** Emission is downstream of the log,
so nothing about the single writer changes. The caveat is
measured/estimated: `LlmOptimizationSummary.baseline_model` is `Option<LlmOptimizationModel>`
with no gate (§5), so *our* emitted summary must carry our
`Accounting::{Measured,Estimated}` split into `LlmOptimizationEvidenceQuality`
faithfully or we would be publishing a weaker claim than our own dashboard makes.

---

### (f) Relay's pricing catalog and PII redaction "as crates, not schemas"

**Both fail the dependency rule, for the same reason.**

*Pricing.* There is no pricing crate. `PricingCatalog` and the entire schema
live in `crates/core/src/codec/model_pricing.rs` (825 lines, `PricingCatalog`
at `:84`); the plugin wrapper is `crates/core/src/plugins/model_pricing.rs`
(95 lines) and the CLI surface is `crates/cli/src/plugins/pricing.rs`
(291 lines). Taking it as a crate means taking `nemo-relay` core (OTel ×3,
tonic, libloading, object_store). **Copying the schema remains the only path**,
exactly as S1 already ruled. What the copy is worth is unchanged and confirmed:
`aliases` (`:274`), `rate_schedule: Option<TokenRateSchedule>` (`:286`),
`prompt_cache: PromptCachePricing` with
`read_accounting: IncludedInPromptTokens|Separate` (`:288, :573-575`),
`pricing_as_of` (`:290`) and `pricing_source` (`:292`), both **validated
non-empty at load** (`:389-390`). Roundhouse's `FrontierModelSpec`
(`crates/roundhouse-fleet/src/frontier.rs:30-48`) and `CatalogConfig`
(`crates/roundhouse-server/src/catalog_config.rs:49-65`) still have **neither
provenance field** — the gap CLAUDE.md's OpenRouter-import rule already demands
be closed.

*PII redaction.* `nemo-relay-pii-redaction` **is** a separate crate (10,999
lines) but `crates/pii-redaction/Cargo.toml:21` reads `nemo-relay.workspace =
true` — a hard dependency on core. It also pins `sha2 = "0.11"` (`:25`) against
roundhouse's `sha2 = "0.10"` (`roundhouse/Cargo.toml:71`), a semver-major
duplicate.

*Third data point, unflattering to everyone.* Switchyard's pricing is a
**hardcoded rate card in Python source** —
`switchyard/cli/launchers/cost_estimator.py`, 500 lines, `MODEL_PRICING:
dict[str, ModelPriceData]` at `:56`, with provenance in *comments* ("OpenRouter
reference list price (June 2026)" `:84`; "an average of OpenRouter provider
rates captured in late July 2026" `:127-135`; "verified 2026-07-27" `:197`) and
no versioned fields. That is the exact failure mode
`roundhouse/CLAUDE.md`'s "rate cards never go in source" rule and
`roundhouse-fleet/src/frontier.rs:60-63` were written against.

---

### (g) `switchyard-protocol` alone — the candidate the brief did not name

Not in the brief, but it is the cheapest real adoption in either tree and it
lands on a gap roundhouse has today.

**What it is.** 2,482 lines; deps `async-trait, futures, http, serde,
serde_json, thiserror` (`crates/protocol/Cargo.toml:18-24`) — **every one
already in roundhouse's graph, none at a conflicting major**. Published on
crates.io at 0.2.0 and 0.1.0
(`https://crates.io/api/v1/crates/switchyard-protocol`).

**What it buys.** `Metadata::from_headers` (`crates/protocol/src/metadata.rs:200-224`)
normalizes the whole coding-agent correlation-header space in one call
(finding 1.2.8), including the three Codex headers PLAN §3 already relies on
(`PLAN-agentic-control-plane.md:459-462`) plus sub-agent lineage roundhouse has
no vocabulary for at all: `parent_agent_id`, `is_subagent`,
`is_delegated_work`, `agent_kind`, `agent_role`, `task_kind`, `session_final`
(`metadata.rs:159-196`). Roundhouse today parses no correlation headers —
`crates/roundhouse-server/src/http.rs` reads only `Authorization`
(`control_config/mod.rs:543-560`) and a resume cursor (`http.rs:456`).

**The type-mapping cost is near zero.** `switchyard_protocol::Role`
(`crates/protocol/src/llm.rs:16-27`) is the same five variants as
`roundhouse_core::item::Role` (`crates/roundhouse-core/src/item.rs:16-22`), in
the same order. `ItemContent::{Text, ToolCall, ToolResult}`
(`item.rs:44-56`) maps 1:1 onto `ContentBlock::{Text, ToolCall, ToolResult}`
(`llm.rs:78-83, 196-214`); the only conversion is `arguments: String` ↔
`arguments: Value`, one `serde_json::from_str`. An adapter is ~40 lines.

**Coupling cost.** 9 commits touching `crates/protocol/src` in 8 days, two of
them breaking (`#457` typed HTTP status codes, `#459` `RoutingOutcome`).
`Metadata` itself changed three times (`#431`, `#422`, `#377`).

---

### (h) `ToolSignals` as an M6 trigger signal — the second unnamed candidate

**What it is.** `ToolSignals::from_request(&Request, Option<usize>) -> ToolSignals`
(`crates/libsy/src/algorithms/util/tool_signals.rs:253`), exported at
`lib.rs:38`. Sixteen fields (`:206-246`), all computed from the request's own
tool history, **no model call**. Plus three public pure scorers:
`dimensions_from_signal` (`util/stage.rs:250`), `score_signal` (`:323`),
`pick_tier` (`:373`).

**How it compares to our four signals.** Ours
(`crates/roundhouse-core/src/validate/trigger.rs:62-78, 168-333`):
`NoProgressRepeat`, `PingPong`, `ToolFailureStreak`, `CostAnomaly`. Theirs adds
things we cannot currently see: windowed error `severity` that persists through
recovery turns rather than clearing on the next clean result (`:207-211`);
`spinning` vs `exploring` as separate dimensions (`stage.rs:214-217`);
`tests_passed` (`:234-235`); and **`compacted`** — the request carries a
context-compaction summary, which is self-latching because the summary stays in
the prefix (`:240-245`). Our `Validator::with_signals`
(`validate/mod.rs:509-516`) is the seam that admits them additively with no
gate change.

**Coupling cost.** These live in `switchyard-libsy`, so §2a's dependency weight
applies in full (jsonschema, jsonptr, OTel 0.32, tracing-opentelemetry 0.33) —
for four pure functions. Re-implementing them from the published source (~1,100
lines in `tool_signals.rs`, Apache-2.0) is the port-not-crate call the ruling
already applies to ACG stability.

*[2026-08-21, four corrections and one sharpening, all against `053a61e`:*

- ***Sixteen fields is 14** (see the note on finding 7). Both source files are
  byte-identical to `5341f71`.*
- ***"Unliftable" is too broad as stated in `validate/prompt.rs:22-25`.** For
  this asset the items are re-exported at the crate root:
  `ToolSignals`/`DEFAULT_RECENT_WINDOW` (`lib.rs:34`) and
  `CodingAgentDimensions`, `PickOutcome`, `PickerMode`, `ScoreResult`, `Tier`,
  `dimensions_from_signal`, `pick_tier`, `score_signal` (`lib.rs:38-42`).
  Every `ToolSignals` field is `pub` and the struct derives `Default`, so the
  three scorers are callable from a struct literal without ever constructing a
  `switchyard_protocol::Request`. The half that **is** unliftable is the one
  that matters: `mod tool_signals` is `pub(crate)` (`algorithms/util.rs:13`),
  so `ToolSignalProcessor`, `classify_text`, and the whole `ERROR_PATTERNS` /
  tool-name / test-phrase table set are unreachable — the extractor must be
  re-implemented either way. The port-not-crate call therefore stands, but on
  the correct half of the asset.*
- ***The dependency cost is now countable, and higher than "§2a".** Roundhouse
  has **none** of `opentelemetry`, `jsonschema`, `jsonptr`, `regex`, or
  `parking_lot` as a normal dependency (workspace `Cargo.toml`), and libsy
  needs all five (`crates/libsy/Cargo.toml:19-38`). Worse for the crate route:
  `opentelemetry 0.31` is already in roundhouse's lock (`Cargo.lock:3193`) via
  `codex-http-client`, and the manifest states in terms that that graph "must
  not reach the shipped binary" and is dev-only by design
  (`crates/roundhouse-server/Cargo.toml:88-98`). libsy pins the 0.32 line, so
  the crate route puts two OTel API majors — two separate global meter
  providers — into one test binary. Same argument round 2 used against rmcp
  1.8 vs 3.1.*
- ***The scorers do not belong in the `Signal` seam at all.**
  `pick_tier` answers "which model tier" (`stage.rs:373-405`); `Signal::detect`
  answers "state a fact about trouble" and returns `Option<String>`
  (`trigger.rs:150-155`). Porting `pick_tier` behind `Signal` would be a
  category error, and `SignalFired::fact`'s own rule — "never a suggestion"
  (`trigger.rs:79-88`) — forbids the output shape. The extractor is what the
  `Signal` seam wants; the scorers, if they are wanted anywhere, belong beside
  `routing/policy.rs`. The field-by-field port table is in the 2026-08-21
  section at the end of this document.*
- *Sharpening: **the codex exec header is neither wholly noise nor wholly
  signal, and a port that treats it as either is wrong in a different
  direction.** Switchyard's `exit_nonzero` pattern matches the bare substring
  `"exited with code"` with no digit constraint (`tool_signals.rs:84-94`) and
  matching is unanchored `contains` (`:530-541`); codex writes `Process exited
  with code {exit_code}` whenever an exec call has one — **zero or non-zero**
  (`core/src/tools/context.rs:443-470` @ `e363b08`, `response_text`). So a port
  fed `Exchange::output` verbatim scores SOFT `0.3` on every exec result
  including exit 0 and pins `no_error_streak` at 0 forever (`:543-553`). But a
  port fed only `tool_output_body(output)` cannot see the exit code at all —
  `Process exited with code ` is one of the sections that function strips
  (`validate/exchange.rs:171-188`). The correct shape is neither: read the
  **exit code from the header as a structured fact**, and run the
  **error-pattern table over the body**. The mechanism is not F04's — F04 was an
  anchored matcher the header *suppressed*, this is a `contains` matcher the
  header *manufactures*, and the remedies differ accordingly.*
- *A finding this turned up that is **not** about the port, recorded separately
  so it is not folded into one: **`reads_as_failure` has the same blind spot
  today.** It asks `tool_output_body(output)` (`exchange.rs:252-253`) and then tests
  anchored markers or a structured `error` / `success: false`, so a codex **exec**
  result that exited non-zero with empty or non-error-shaped stdout reads as
  **clean** — a `grep` with no match, a `test` that is false, a `diff` that found
  differences. Reproduced against the real header shape: `"Chunk ID: 1\nWall
  time: 0.0210 seconds\nProcess exited with code 1\nOutput:\n"` strips to the
  empty string and `reads_as_failure` returns `false`, so `ToolFailureStreak`
  cannot fire on it. **Exec-only**: MCP results carry `Wall time: {:.4}
  seconds\nOutput:` and no exit-code section at all
  (`core/src/tools/context.rs:118-138` @ `e363b08`), so there is nothing to lose
  there — and `is_undelivered_tool_result` is *correct* to strip, since its
  three sentinels are body text the header would otherwise stop the equality
  test from ever matching. The strip is not uniformly suspect; it is missing one
  structured fact only exec results carry. A claim, not a ruling — it wants a
  failing test first per `CLAUDE.md`, and the fix (an exit-code accessor beside
  `tool_output_body`, correctly `None` for MCP results) is the same seam the
  port needs anyway.]*

---

## 3) Cross-cutting: churn, interop, invariant strain

### 3.1 Churn, quantified

| Surface | Commits | Window | Rate | Breaking changes in window |
|---|---|---|---|---|
| `switchyard/crates/libsy/src` | 14 | 8 days (2026-08-11→19) | **1.75/day** | 7 (below) |
| `switchyard/crates/protocol/src` | 9 | 8 days | **1.1/day** | 2 (`#457`, `#459`) |
| `nemo-relay/crates/types/src` | 6 | 16 days (2026-08-03→19) | **0.4/day** | 1 (`56158e4 feat!`) |
| `nemo-relay` repo-wide `feat!`/BREAKING | 2 | 16 days | 0.13/day | `#575`, `#751` |
| `nemo-relay/crates/{cli,types,adaptive,switchyard,pii-redaction}` since our pin | **0** | 1 day | 0 | 0 |

**Every breaking change to libsy's public API in the visible window:**

| Commit | Date | What broke |
|---|---|---|
| `48b3b71 #373` | 08-12 | `ModelId` type introduced — target names change type across the API |
| `224287b #393` | 08-13 | selected `ModelId` stamped onto `LlmRequest` much earlier |
| `a17efa9 #413` | 08-13 | `Decision::reasoning` demoted to a log message |
| `9ad6744 #431` | 08-14 | retry logic moved libsy → libsy-llm-client (`Driver` method set) |
| `05533a5 #457` | 08-17 | protocol switched to typed HTTP status codes |
| `0cf6439 #459` | 08-18 | **`Algorithm::route` returns `RoutingOutcome`; `Decision` deleted from the protocol crate** |
| `c7c07d5 #471` | 08-18 | a callback deleted; default log level changed |

**The sharpest single number.** Of the eleven `core::algorithm` exports at the
tag every Switchyard doc tells you to pin (`v0.2.0`, 2026-08-10), **four
survive by name on main nine days later**; seven were deleted and three added
(finding 1.2.1). Both call themselves `0.2.0`.

### 3.2 Dependency and version interop

| Dependency | roundhouse | Switchyard | Relay | Resolves? |
|---|---|---|---|---|
| toolchain | 1.96.1 (`rust-toolchain.toml`) | 1.96.1 | 1.96.1 | ✅ identical |
| edition | 2024 | 2024 | 2024 | ✅ |
| `tokio` | `1.48` caret → **1.48.0** under `dynamo-mocker`'s exact pin (`Cargo.toml:74-77`) | `1` full (`Cargo.toml:45`) | (workspace, unpinned in root) | ✅ any 1.x unifies |
| `redis` | `1.2` (`Cargo.toml:97`) | — (none) | `^1.1` | ✅ unified since S1 |
| `axum` | `=0.8.4` **exact** (`Cargo.toml:59`) | (in `switchyard-server` only) | 0.8 | ✅ but ours is exact — a `=0.8.4` against any transitive `0.8.5+` requirement fails |
| `reqwest` | `0.12.24` (`Cargo.toml:83`) | **`0.13.4`** (`Cargo.toml:36`) | 0.12 | ❌ **semver-incompatible.** Affects `switchyard-llm-client` and `switchyard-server` only; `switchyard-libsy` and `switchyard-protocol` carry no reqwest |
| `uuid` | `1.18.1` (`Cargo.toml:80`) | — | `=1.18.1` (`Cargo.toml:44`) | ✅ identical |
| `sha2` | `0.10` (`Cargo.toml:71`) | — | `0.11` in pii-redaction | ⚠️ duplicate major |
| `opentelemetry` | **0.31.0, dev-only** via `codex-http-client` (`Cargo.lock:3193`) | **0.32** in libsy (`crates/libsy/Cargo.toml:31`) | 0.32 workspace | ⚠️ adopting libsy puts 0.31 (dev) + 0.32 (prod) in one lock |
| `tracing-opentelemetry` | — | **0.33** | — | ⚠️ new |
| `jsonschema` / `jsonptr` | — | `0.49.4` / `0.8.1` | — | ⚠️ new to the graph |
| `rand` | 0.9.5 + 0.10.2 already in lock | `0.10` | — | ✅ |
| OpenSSL | banned | banned (rustls) | banned | ✅ |

### 3.3 Invariant strain, by candidate

| | single-writer log | narrow-only policy | measured/estimated | no-fail-open |
|---|---|---|---|---|
| (a) libsy as `RoutingPolicy` | **high** — `State` unserializable, 1h-TTL process map (`state.rs:1-34`, `fall_through.rs:40-49`) | medium — no lattice; clamp must stay outside | low | low |
| (b) `AdvisorGate` | **high** — `Mutex<GateState>` + 1,024-scope eviction (`advisor_gate.rs:84, 179-182`) | **high** — REDO mutates the conversation with judge prose verbatim (`:424-427`) | medium — tokens counted, dollars never | **direct conflict** — `fail_open: true` by default (`:147`) and a failed consult logs `verdict: "APPROVE"` (`:499-505`) |
| (c) `EscalationClassifier` | **high** — streak in `State.extra` (`llm_class.rs:448`) | medium — routes to a named target, no ceiling | **high** — discarded efficient turn is paid and unaccounted (no `record_discarded`) | low — outage *holds* the streak (`:571`), which agrees with us |
| (d) chained Relay front end | low (ours stays authoritative) | low | **high** — JWT strip changes who pays invisibly (`alignment.rs:97-110`) | **high** — upstream override can route around us entirely (`alignment.rs:85-93`) |
| (e) emit via `nemo-relay-types` | none | none | low — must carry our split into `EvidenceQuality` faithfully | none |
| (f) pricing/PII as crates | none | none | none | none — *blocked on the dependency rule, not on invariants* |
| (g) `switchyard-protocol` | none | none | none | none |
| (h) `ToolSignals` | none — pure | none | none | none |

---

## 4) What we keep regardless

Verified absent from **both** trees at `ca08901` / `5341f71`:

1. **Append-only per-session event log with a fenced single-writer lease.**
   Roundhouse: `crates/roundhouse-core/src/store/contract.rs:46-131`
   (`acquire_lease`/`renew_lease`/`release_lease`, not-found outranks
   lease-lost). Relay: a fire-and-forget subscriber bus, in-memory, no
   ordering guarantee beyond FIFO. Switchyard: the nearest thing is
   `crates/switchyard-server/src/routing_log.rs` — a **local-file JSONL of
   per-request token counts** (`RoutingRecord{ts, task, trial_id, session_id,
   model, tier, prompt/cached/cache_creation/completion/reasoning/total
   tokens}`, `:51-65`), timestamp-ordered, no `seq`, no lease, no items, no
   replay-as-conversation.
2. **Conversation ownership, delta upload, prefix admission.**
   `suffix_after`/`same_item`, `crates/roundhouse-server/src/responses_api.rs:353-370`.
   Neither tree has an analogue; Relay explicitly *bypasses* stateful requests
   (`crates/adaptive/src/response_cache/key.rs:115-135`).
3. **Tenancy, budgets, spend ledger, credentials as first-class.**
   `crates/roundhouse-core/src/control/{policy.rs, budget.rs, spend.rs,
   spend/contract.rs}` (1,629 + 620 + 757 + 924 lines),
   `crates/roundhouse-store-redis/src/spend/scripts.rs` (705). Switchyard:
   `grep -rniE 'budget|quota|tenant|principal|spend'` over `crates/` returns
   only retry budgets, output-token budgets, and advisor review counters. Relay
   has one credential concept, a per-invocation loopback proxy token
   (`crates/cli/src/provider_auth.rs:18-38`). **Neither has dollars in its Rust
   tree at all.**
4. **Steering — the synthetic tool call.** `Interjection::Complete{item, usage,
   guidance, record}` (`crates/roundhouse-core/src/interject.rs:132-137`),
   `STEER_TOOL = "fetch_steer"`, `STEER_CALL_PREFIX = "rhsteer_"`
   (`validate/mod.rs:99, 106`), provenance-stamped so a client cannot forge one
   (`item.rs:88-93`). In libsy, `ContentBlock::ToolCall` is **only ever read**
   in production code — `grep 'ContentBlock::ToolCall'` returns matches in
   `tool_signals.rs:351`, `escalation.rs:204`, `llm_class.rs:126`,
   `advisor_gate/turn.rs:107` (all pattern matches) and four construction sites
   that are all inside `#[cfg(test)]` modules. The nearest upstream analogue is
   text injection: `append_note` (`util/prompts.rs:42-54`) and the advisor
   gate's REDO message append. Relay's hook adapters always allow
   (`crates/cli/src/agents/claude/adapter.rs:46-53`).
5. **The MCP control surface.** Eight tools —
   `TOOL_NAMES: [&str; 8] = ["status", "init_session", "declare_intent",
   "prefer", "set_quality_floor", "fetch_steer", "report_outcome",
   "explain_last_route"]` (`crates/roundhouse-mcp/src/tools.rs:47-56`), 5,341
   lines across the crate. Relay's MCP returns `{"tools": []}`
   (`crates/cli/src/mcp/protocol.rs:72-76`) — re-verified at `ca08901`.
   **Switchyard has no MCP at all** (`grep -rli mcp crates/ switchyard/` →
   no matches).
6. **Arm instrumentation.** `Arm::{Live, Shadow, Placebo}`
   (`validate/arm.rs:51-78`), `ArmShares` (`:121-143`), per-session hashed
   assignment (`:105`), `placebo_intervenes` on hashed timing (`:196`).
   Neither libsy judge algorithm has a shadow, placebo, or observe-only arm;
   re-confirmed at `5341f71`.
7. **The capability gate.** `ShadowPricing::capability_band`
   (`crates/roundhouse-core/src/metrics/pricing.rs:353, 404-410`), applied
   before shape at `:463-474`, with `unit_interval` validation of every
   configured prior (`catalog_config.rs:68-86`). `grep -rn 'quality_prior|capability_band'`
   returns **zero hits in both external trees**.
8. **Realized cache evidence.** `TokenBreakdown::cache_hit_ratio` measured from
   provider-reported cached tokens (`metrics/snapshot.rs:63-70`) and
   `CacheLedger` (`routing/ledger.rs:194-268`). ACG still plans from an
   *assumed* `expected_reads` (`nemo-relay/crates/adaptive/src/acg/economics.rs:24,
   45, 89`); `grep 'realized|observed_hit|hit_ratio'` over `crates/adaptive/src/acg/`
   returns nothing.
9. **A dedup'd turn identity and byte-identical replay.** `turn_id`
   content-hash dedup and the `Replaying{cursor, bound}` follower
   (`responses_api.rs:385-395`). Relay's `response_cache` is an independently
   keyed exact-match cache that bypasses stateful requests; Switchyard has no
   cache.

---

## 5) Dedup in the other direction: what we built that upstream still wants

| # | Roundhouse asset | Relay @ `ca08901` | Switchyard @ `5341f71` | Status |
|---|---|---|---|---|
| R1 | **`enforce_usage_reporting`** (`crates/roundhouse-fleet/src/usage.rs:98`, add-never-override) | **still absent** — only classifies `stream_options` as portable in the deprecated client (`crates/switchyard/src/translation.rs:168-173`) | **already has it**, independently: `ensure_openai_stream_usage` (`crates/libsy-llm-client/src/client.rs:786-806`) | **Halved.** The contribution target is Relay only — and Switchyard's implementation is the precedent that makes the case, not a competitor |
| R2 | **Capability gate on `baseline_model`** (`metrics/pricing.rs:353, 463-474`) | `LlmOptimizationSummary.baseline_model` still ungated (`crates/types/src/codec/optimization.rs:267`); zero `quality_prior`/`capability_band` in the tree | no savings vocabulary at all — zero hits for `baseline_model`, `estimated_cost_saved`, `optimization_summary` | **Unchanged and still the cleanest trade.** Relay has the schema and lacks the safeguard; we have the safeguard |
| R3 | **Realized cache evidence** as ACG's `expected_reads` (`snapshot.rs:63`, `ledger.rs:244`) | ACG unchanged; `expected_reads` still an assumption (`acg/economics.rs:24, 45, 89`); no realized/measured hit ratio anywhere | n/a — no cache economics | **Unchanged** |
| R4 | **Discarded-work priced in dollars** (`judge.rs:461-497` reserve/settle) | n/a | counts tokens only (`advisor_gate/telemetry.rs:45-67`); `/v1/stats` exposes `DiscardedStatsSnapshot{turns, tokens}` (`switchyard-server/src/stats/algorithms/advisor_gate.rs:49-54`) with the comment "the client never saw the turn, so terminal usage accounting never priced it" | **New.** Switchyard has *named* the hole and cannot fill it — it has no pricing in Rust. We can |
| R5 | **A durable, cold-replayable trajectory source** | ATIF producer is in-memory (`atif.rs`) | routing log is a flat JSONL of usage rows, no items | **Unchanged** |
| R6 | **The steering primitive** | can block a tool, cannot inject one | can append text, cannot inject a call (§4.4) | **Unchanged** |
| R7 | **Replay-stable review budgets** (`TriggerConfig` as a log projection, `trigger.rs:349-377`; `ReviewBudget` half-open breaker, `validate/mod.rs:318-360`) | n/a | `Mutex<GateState>` re-arms on restart, hardcoded `MAX_FAILED_CONSULTS`, no in-flight cap, no re-arm (`advisor_gate.rs:81, 260-289`) | **New.** A direct, small upstream contribution to `AdvisorGate` |
| R8 | **Rate cards out of source** (`frontier.rs:60-63`, `ROUNDHOUSE_CATALOG`) | catalog is config-driven with validated provenance — **Relay is ahead of us here** | hardcoded in `cost_estimator.py:56`, provenance in comments | **Mixed.** We are behind Relay and ahead of Switchyard |

Two things we *should* take rather than give, on the same axis:

- **Structured output on the wire.** Switchyard's judge sets
  `response_format` from a JSON-schema contract
  (`util/llm_judge.rs:169`, `util/classifier_contract.rs:15-57`). Roundhouse's
  judge asks for JSON **in the prompt only** — `grep -rn 'response_format|json_schema'`
  over `validate/`, `judge.rs` and `roundhouse-fleet/src/` returns nothing; the
  only instruction is `judge-system-prompt.md:132` ("Return exactly one JSON
  object and nothing else"). Our parser is strict (`verdict.rs:106-118`), so the
  failure mode is a wasted consult rather than a wrong verdict — but a wasted
  consult still costs money and trips the breaker.
- **The boundary trigger.** Still not in `SignalKind`
  (`trigger.rs:62-78`: four variants, none of them "the turn ended with no tool
  call"). S5 named it; it is open.

---

## 6) Seams, both directions, at these revs

### 6.1 Them → us (heaviest first by value/cost ratio)

| Seam | Mechanism | Cost | What we get |
|---|---|---|---|
| **S-a** `switchyard-protocol::Metadata::from_headers` | one crate, 6 deps all present (`crates/protocol/Cargo.toml:18-24`) | **S** | Every coding-agent correlation header normalized (`metadata.rs:15-65, 200-224`), sub-agent lineage we have no vocabulary for, Dynamo header aliases for free |
| **S-b** `LlmOptimizationSummary` + ATOF via `nemo-relay-types` | one crate, 7 light deps, `uuid` identical | **S** | The savings story as a published NVIDIA type; an existing ATOF→ATIF converter downstream *[2026-08-21: two qualifiers. "`uuid` identical" is **false** as of this re-read — ours is a caret resolved to 1.24.0, theirs an exact `=1.18.1`; see §2(e). And "an existing ATOF→ATIF converter downstream" is true but weaker than it sounds. The converter is `NeMo-Agent-Toolkit@c933737:packages/nvidia_nat_atif/src/nat/atof/scripts/atof_to_atif_converter.py` (1,039 lines, still shipping, now with eight test modules and committed worked examples), but its `MARK_EXTRACTOR_REGISTRY` ships **empty** (`extractors.py:757`), so a `data_schema` we declare for routing marks resolves to the default `NatRoleMarkExtractor` and our payload lands as a JSON *string* in a `system` step (`converter:638-643`) unless `data.role ∈ {user,system,agent}`. Nothing is dropped; nothing is structured either. The only converter path that carries `data_schema` into ATIF `step.extra` is a `category: "context"` scope-end (`converter:672-673`) — so if structural preservation with no upstream PR is the goal, that is the seam, not the mark path.]* |
| **S-c** `ToolSignals` + `score_signal`/`pick_tier` | port ~1,100 lines (Apache-2.0) or take libsy whole | **M** port / **L** crate | Four to six new no-model-call trigger signals including `compacted` and windowed `severity` *[2026-08-21: 12 of the 14 fields port; `turn_depth` and `compacted` do not — `Evidence` carries no message count, and `exchanges()` drops `ItemContent::Text` outright (`validate/exchange.rs:91`) so the compaction scan has no input at all. `compacted` is additionally premised on the summary self-latching in the prefix, and that premise is false here: roundhouse *forks* a compacted conversation onto a fresh empty session (`responses_api.rs:330-340`), leaving no prefix to latch onto. `tests_passed` ports as a *gate* condition, not a `Signal` — the seam has no vocabulary for "quiet". Crate route re-costed above: five dependency families roundhouse does not have, plus an OTel major split.]* |
| **S-d** Advisor-gate mechanisms as ideas | already ported (S5): two-counter budget, anchored parse, discarded-work accounting, injection defense — `judge-system-prompt.md:1-21` records the attribution at rev `47babb1` | **done** | — |
| **S-e** `forward_auth` as the M7 pass-through *design reference* | read, don't depend | **S** | The redirect-none client, the mutual-exclusion check, the per-provider forwarded-header set, `redact_forwarded_auth`, and the route-level provider check — a working answer to four questions M7 has to answer anyway |
| **S-f** `AgentHints.osl` as grant-sizing input | read the header `x-nemo-relay-adaptive-agent-hints` when a Relay sits in front | **S** | Better `expected_output_tokens` for the budget grant |
| **S-g** ACG stability as a fifth trigger | port the analysis (`acg_learner.rs:56-90`) | **M** | Orthogonal no-model-call signal |
| **S-h** Pricing-catalog **schema** copy (provenance fields) | copy ~300 lines | **M** | `pricing_as_of`/`pricing_source`, tiered rates, aliases, `read_accounting` — closes the CLAUDE.md OpenRouter-import gap |

### 6.2 Us → them

| Seam | Target | Cost | What they get |
|---|---|---|---|
| **T-1** `enforce_usage_reporting` | **Relay only** (Switchyard already has it) | **S**, one file | Stops silently recording zero-token, zero-dollar streaming calls |
| **T-2** Capability gate for `baseline_model` | Relay `crates/types/src/codec/optimization.rs` | **S** | Savings accounting stops accepting a 7B priced against a flagship |
| **T-3** Realized cache evidence → `expected_reads` | Relay `crates/adaptive/src/acg` | **M** | ACG plans from measurement where a roundhouse is in the path |
| **T-4** Replay-stable + re-arming review budget | Switchyard `AdvisorGate` | **S** | An in-flight cap, a configurable failure cap, and a half-open breaker instead of a permanent per-scope kill |
| **T-5** Discarded-work priced, not just counted | Switchyard `advisor_gate/telemetry.rs` | **M** (needs a rate card they do not have) | The hole their own comment names |
| **T-6** Structured verdict instead of a prose scan | Switchyard `AdvisorGate` | **M** | An advisor whose plan cannot be prompt-injected into the executor verbatim |
| **T-7** Rate cards out of source | Switchyard `cost_estimator.py` | **S** | Versioned provenance instead of dated comments |

---

## 7) Risks

**E1 (chained topology) — unchanged and re-verified, now with a fourth
hazard.** The upstream override (`alignment.rs:85-93` via
`shared/alignment.rs:424-436` via `gateway/request.rs:63-70`) ignores the
configured base URL entirely; the JWT strip (`alignment.rs:97-110`) is its
mutually exclusive complement. Fourth hazard: the decode/re-encode path
(`gateway/request.rs:96-99`) against our prefix admission, with Switchyard's
own `drop_exact_replay` (`util/prompts.rs:56-71`, "reintroduces SWITCH-1224,
silently and without a failing test") as the standing evidence that this class
of bug is real in exactly this position.

**E2 — three launch surfaces, one product.** Relay's `crates/cli/src/agents`
(6,301 lines, Rust), Switchyard's `switchyard/cli/launchers` (2,503 lines,
Python), and roundhouse's planned M9 config generation all do the same thing
and disagree about `requires_openai_auth`. Adopting one does not delete the
other two's existence; it picks which one a deployment is told to use.
*[2026-08-21: **two, not three.** `43fc1a7` (#501) deleted the Switchyard
launcher and with it the only implementation that set `requires_openai_auth`
conditionally, so the disagreement is back to Relay's hardcoded `true` against
PLAN §3's leave-unset — with `caller_auth_kind` surviving as the *reason* to
believe it is a route property (see the note at finding 1.2.3). Nothing about
this shrinks M9: the one test M9 exists for is untouched by any front end.]*

**E3 — version identity collision.** `switchyard-libsy 0.2.0` on crates.io
(2026-08-10), `tag v0.2.0` in every doc, and `main` all answer to "0.2.0" and
expose three different APIs (§1.2.1). A `Cargo.toml` line saying
`switchyard-libsy = "0.2.0"` is not a reproducible statement of what you
depend on. Any adoption must pin a **rev**, not a version or a tag — which is
the same posture as our Dynamo pin, and costs the same maintenance.

**E4 — two OpenTelemetry lines.** Our lock has 0.31.0 (dev-only, via
`codex-http-client`, `Cargo.lock:3193`); libsy demands 0.32
(`crates/libsy/Cargo.toml:31`) plus `tracing-opentelemetry 0.33`. Both would
coexist; nothing breaks, but the binary grows and a future OTel adoption
inherits a fork in the road.

**E5 — `reqwest` 0.12 vs 0.13.** `switchyard-server` and
`switchyard-llm-client` are on 0.13.4 (`Cargo.toml:36`); we are on 0.12.24.
Not resolvable by unification. It bounds any adoption to
`switchyard-libsy` + `switchyard-protocol` and rules out the client and server
crates outright — which happens to also be the boundary where their session
state stops being process-local.

**E6 — fail-open is the upstream default in both judge paths.**
`AdvisorGateConfig::fail_open` defaults `true` (`advisor_gate.rs:147`) and a
failed consult is audited as `verdict: "APPROVE"` (`:499-505`). Our
`Interjector` cannot express failure at all (`interject.rs:236-241`) and a
timed-out validator is "marked, never free" (`validate/mod.rs:29-31`). These
are not tunings of one design; they are opposite answers to "what does a broken
checker mean".

**E7 — the maturity asymmetry runs both ways now.** Relay is 0.8.0, ten
published crates, versioned docs, 0.4 breaking commits/day on types.
Switchyard is 0.2.0 with 1.75 commits/day on libsy and an unreleased flagship
algorithm. Roundhouse is 0.1.0. Coupling to Relay buys a slow-moving format;
coupling to Switchyard buys a fast-moving library.

**E8 — the deprecated client is still in Relay's tree.**
`nemo-relay/crates/switchyard` unchanged at `ca08901`, still a client for a
Decision API that Switchyard main does not serve. A reader searching NVIDIA
source for "switchyard" still finds the wrong artifact first; our README fix
(`roundhouse/README.md:234-244`) remains load-bearing.

---

## 8) Open questions the synthesis must decide

1. **Is `requires_openai_auth` a client-version fact or a route property?**
   Three implementations now: ours says leave unset (read at codex `6344a65`,
   `PLAN:437-447`), Relay hardcodes `true` (`launch.rs:201`), Switchyard sets it
   from `caller_auth_kind` (`codex_cli_launcher.py:79-91`). If the third reading
   is right, both prior readings are correct for their own configuration and the
   "contradiction" dissolves into a decision. This is still M7's first
   verification item; it now has a hypothesis to test rather than only a
   conflict to resolve.
2. **Does "use heavily" mean adopting a crate, or adopting a design?** The two
   Switchyard assets with the most novel content — `AdvisorGate` and
   `EscalationClassifier` — both require owning dispatch, which neither of our
   seams grants. The two with the least novel content — `AffinityRouter`,
   `StageRouter` — fit our trait but duplicate what we have. If heavy adoption
   means crates, the answer is `switchyard-protocol` and nothing else; if it
   means designs, the surface is much larger.
3. **Which front end is blessed, given there are now two?** Relay's Rust CLI
   with hook-level visibility and two live routing hazards, or Switchyard's
   Python launcher with a proxy-bypass guard and no hooks. Or roundhouse's own
   M9 config generation, which the one test M9 exists for does not let us skip
   either way. *[2026-08-21: **the question is now binary — Switchyard deleted
   its launcher** (`43fc1a7`, #501). The round-2 ruling had already declined to
   bless it; upstream has since agreed by removing it. The
   `wait_for_proxy_ready` proxy-bypass guard cited as its one advantage over
   Relay's went with it, so if that guard is worth having it has to be
   re-derived rather than pointed at.]*
4. **Do we take `switchyard-protocol` as the header front door?** It is the
   cheapest adoption in either tree, lands on a real gap, and its `Role`/content
   types map 1:1 onto ours — but it churned 9 times in 8 days and its published
   version is not its main version.
5. **Is `nemo-relay-types` pinned to crates.io 0.7.3 or to a git rev of 0.8.0?**
   The types we want (`LlmOptimizationSummary`, ATOF envelope) exist in both;
   the tree carries one `feat!` beyond the published crate.
   *[2026-08-21: **the question now has a decidable shape, and it is not about
   the version.** Both `codec/optimization.rs` and the whole ATOF envelope are
   byte-identical across 0.7.3, 0.8.0-rc.1 and HEAD, so for S2 as written
   ("ATOF events from the session log … `LlmOptimizationSummary`/`Contribution`
   for the savings story") `=0.7.3` is equivalent, not a compromise. The pin
   must move only if S2's "with M6's metrics work" means emitting
   Relay-conformant **metric marks**: `MetricEnvelope` and the
   `METRIC_DATA_SCHEMA_NAME`/`_VERSION` vocabulary — the pairing the older
   dive's overlap row 20 makes against our metrics fold — are 0.8-only. So the
   question to put to the ruling is "does S2 emit metric marks?", and the
   version follows from the answer.]*
6. **Does emitting `LlmOptimizationSummary` without the gate upstream weaken our
   own claim?** Their `baseline_model` is ungated by design. Publishing into
   that schema means our number sits beside numbers produced without the
   safeguard, in a format that cannot distinguish them — unless T-2 lands first
   or we carry the gate's result in `limitations[]`.
7. **What is the standing of the `libsy::State` blocker now that we know the
   session map, the affinity map and the gate ledger are three separate
   process-local structures?** The README says "in-memory with no pluggable
   persistence" (`README.md:239-244`). That is now three in-memory stores, none
   of which has a trait seam. Is the blocker "add persistence upstream" (a
   contribution) or "keep it behind our trait" (the standing ruling)?
8. **Does the discovery that Switchyard independently implemented
   `enforce_usage_reporting` change the contribution plan or validate it?** S4
   listed it as our most valuable transplant. It is still Relay's gap — but it
   is no longer *our* idea alone, and citing Switchyard's implementation is
   probably a better argument to Relay than citing ours.

---

### Appendix: files read, for traceability

**Switchyard @ `5341f71`** — `CHANGELOG.md`, `Cargo.toml`, `rust-toolchain.toml`,
`crates/libsy/{Cargo.toml, src/lib.rs, src/core/{algorithm.rs, state.rs,
processor.rs}, src/algorithms.rs, src/algorithms/advisor_gate.rs,
src/algorithms/advisor_gate/{transcript.rs, turn.rs, telemetry.rs},
src/algorithms/llm_class.rs, src/algorithms/util/{escalation.rs,
tool_signals.rs, stage.rs, prompts.rs}, src/prompts/**}`,
`crates/protocol/{Cargo.toml, src/lib.rs, src/envelope.rs, src/metadata.rs,
src/llm.rs}`, `crates/libsy-llm-client/src/{client.rs, backend.rs}`,
`crates/switchyard-server/src/{lib.rs, config.rs, routing_log.rs,
stats/algorithms/advisor_gate.rs}`, `switchyard/cli/launchers/{codex_cli_launcher.py,
native_server.py, cost_estimator.py}`, `tests/test_launcher_proxy_bypass.py`,
`docs/{getting_started.md, reference/toml_schema.md}`.

**NeMo Relay @ `ca08901`** — `Cargo.toml`, `rust-toolchain.toml`,
`crates/types/{Cargo.toml, src/api/event.rs, src/codec/optimization.rs}`,
`crates/core/src/{observability/atif.rs, codec/model_pricing.rs,
plugins/model_pricing.rs}`, `crates/cli/src/{mcp/protocol.rs,
gateway/{routes.rs, request.rs}, agents/shared/alignment.rs,
agents/codex/{alignment.rs, launch.rs}}`, `crates/adaptive/src/acg/economics.rs`,
`crates/pii-redaction/Cargo.toml`, `crates/switchyard/src/translation.rs`.

**roundhouse @ `c168de8` + uncommitted M6 review-fix diff** — `CLAUDE.md`,
`README.md`, `PLAN-agentic-control-plane.md`, `SYNERGY-nemo-relay.md`,
`SYNERGY-nemo-relay-deep-dive.md`, `agent-docs/README.md`, `Cargo.toml`,
`Cargo.lock`, `crates/roundhouse-core/src/{interject.rs, item.rs,
routing/{mod.rs, policy.rs, ledger.rs}, validate/{mod.rs, trigger.rs,
verdict.rs, arm.rs, brief.rs, prompt.rs, prompts/judge-system-prompt.md},
metrics/{mod.rs, pricing.rs, snapshot.rs}, store/contract.rs}`,
`crates/roundhouse-fleet/src/{frontier.rs, usage.rs}`,
`crates/roundhouse-server/src/{engine.rs, judge.rs, responses_api.rs,
catalog_config.rs, control_config/mod.rs}`, `crates/roundhouse-mcp/src/tools.rs`.

**External URLs** — `https://crates.io/api/v1/crates/switchyard-libsy`,
`https://crates.io/api/v1/crates/switchyard-protocol`,
`https://crates.io/api/v1/crates/nemo-relay-types`,
`https://raw.githubusercontent.com/NVIDIA-NeMo/Switchyard/refs/tags/v0.2.0/crates/libsy/src/lib.rs`.


---

## Appendix: independent verification (2026-08-19)

**[CONFIRMED]** Relay's MCP server answers tools/list with [] (crates/cli/src/mcp/protocol.rs:72-76) -- it exists only to keep a shared gateway alive.

Read crates/cli/src/mcp/protocol.rs in nemo-relay@ca08901: lines 72-76 are exactly `Some("tools/list") => Some(json!({... "result": { "tools": [] } }))`. Line numbers match precisely.

**[CONFIRMED]** Switchyard has no MCP surface at all (grep -rli mcp crates/ switchyard/ -> no matches).

Ran `grep -rli mcp crates/ switchyard/` in switchyard@5341f71: exit code 1, no output. Broader repo grep found only unrelated 'mcp' hits in benchmark/ and examples/experimental/ (a package name and a LiteLLM MCP-skip comment), outside the two directories the report scoped its grep to -- doesn't contradict the precise claim as stated.

**[CONFIRMED]** AdvisorGateConfig.fail_open defaults to true, and a failed advisor consult in fail-open mode is audited with verdict: "APPROVE" (advisor_gate.rs:132/147, 494-506, ~499-505).

crates/libsy/src/algorithms/advisor_gate.rs:147 `fail_open: true` in Default impl; lines 499-505 show `emit_review_audit(ReviewAudit { verdict: "APPROVE", error: Some(error.to_string()), ... })` inside the `if !self.config.fail_open { return Err(...) }` else-path (the fail-open branch), immediately followed by `return Ok(ConsultOutcome::Failed)`. Exact line numbers match report.

**[CONFIRMED]** crate::algorithms::escalation does not exist as a module; EscalationJudge, EscalationPolicy, EscalationClassifier, build_judge are all pub(crate) or private -- only EscalationJudgeConfig is exported at lib.rs:30.

crates/libsy/src/algorithms.rs lists exactly 8 pub mods: advisor_gate, fall_through, llm_class, noop, passthrough, rand, stage, util -- no 'escalation'. escalation.rs:99 `pub(crate) struct EscalationInput`, :112 `pub(crate) type EscalationJudge`, :119 `pub(crate) struct EscalationPolicy`, :146 `pub(crate) fn build_judge` -- exact line matches. llm_class.rs:477 `struct EscalationClassifier` has no pub qualifier at all (private). lib.rs:30 exports only `algorithms::util::escalation::EscalationJudgeConfig`.

**[CONFIRMED]** Algorithm::route signature is `async fn route(self: Arc<Self>, driver: Driver, request: Request) -> Result<RoutingOutcome>`, and RoutingOutcome::route_to / ::answered are the two constructors, at algorithm.rs:353-362 and :64-95.

crates/libsy/src/core/algorithm.rs:353 `pub trait Algorithm...{`, :357 `fn name(&self) -> &str;`, :362 `async fn route(self: Arc<Self>, driver: Driver, request: Request) -> Result<RoutingOutcome>;`. RoutingOutcome struct at :64, route_to at :79, answered at :95 -- all exact matches.

**[CONFIRMED]** requires_openai_auth is hardcoded true in Relay's Codex launch config (launch.rs:199-205) but set conditionally in Switchyard's launcher based on use_openai_auth (codex_cli_launcher.py:79-91).

nemo-relay crates/cli/src/agents/codex/launch.rs:199 `fn gateway_provider_config` whose format! string at :201 embeds the literal `requires_openai_auth=true` unconditionally. switchyard/cli/launchers/codex_cli_launcher.py:79-91 shows `if use_openai_auth: ...requires_openai_auth=true... else: ...env_key=OPENAI_API_KEY..., requires_openai_auth=false`.

**[CONFIRMED]** Roundhouse's RoutingPolicy trait is `fn name(&self) -> &str; async fn choose(&self, ctx: &RoutingContext<'_>) -> Result<Decision, RoutingError>;` at routing/mod.rs:536-542.

crates/roundhouse-core/src/routing/mod.rs: trait block spans lines 534(#[async_trait])-540, with `fn name(&self) -> &str;` and `async fn choose(&self, ctx: &RoutingContext<'_>) -> Result<Decision, RoutingError>;` exactly as quoted. Off by ~1 line from the cited 536-542 start, immaterial.

**[CONFIRMED]** Dependency weight mismatch: Switchyard's workspace reqwest is 0.13.4 (new to the graph vs roundhouse's 0.12.24), and switchyard-libsy pulls opentelemetry 0.32 / tracing-opentelemetry 0.33 against roundhouse's opentelemetry 0.31.0 which reaches the lock only via a dev-dependency (codex-http-client).

switchyard Cargo.toml:36 `reqwest = { version = "0.13.4", ... }`; roundhouse Cargo.toml:82 `reqwest = { version = "0.12.24", ... }`. switchyard crates/libsy/Cargo.toml:30 `opentelemetry = { version = "0.32", ... }`, :39 `tracing-opentelemetry.workspace = true` (0.33 per report, not independently re-verified but plausible from same workspace pin). roundhouse Cargo.lock:3193-3195 opentelemetry 0.31.0, pulled by codex-http-client (Cargo.lock:693-716), and codex-api/codex-client are declared under [dev-dependencies] in crates/roundhouse-server/Cargo.toml (section starts line 44, codex-api at line 65), matching the 'dev-only' claim.

**[CONFIRMED]** libsy::State is 34 lines, no Serialize/store trait/snapshot; session state is a process-local Mutex<HashMap<String,SessionState<S>>> with a one-hour TTL and one-hour cleanup interval (fall_through.rs:40,46,49); AffinityRouter adds a second Mutex<HashMap<RoutingIdentity,ModelId>> (affinity.rs:58).

crates/libsy/src/core/state.rs is exactly 34 lines; State struct has `#[derive(Debug, Clone, Default)]` only, no Serialize. fall_through.rs:40 `type SessionStates<S> = Mutex<HashMap<String, SessionState<S>>>;`, :46 and :49 both `Duration::from_secs(60 * 60)` for TTL and cleanup interval. util/affinity.rs:58 `assignments: Mutex<HashMap<RoutingIdentity, ModelId>>,`. All line numbers match exactly.

### Checker's confidence statement

I re-derived 9 of the highest-stakes claims directly against the pinned trees (nemo-relay@ca08901, switchyard@5341f71, roundhouse@current) rather than trusting the document's prose, covering every category the task flagged as dangerous: two 'X does not exist' negatives (Relay MCP returns empty tools/list; Switchyard has no MCP surface in its core dirs), one public/private-visibility claim (the escalation module and its four private types), two exact API-shape quotes (the Algorithm trait and roundhouse's own RoutingPolicy trait), one stateful-vs-stateless claim central to three separate invariant-strain rulings (libsy::State plus two process-local Mutex<HashMap> session stores with 1-hour TTLs), one cross-repo protocol/config claim (requires_openai_auth hardcoded vs conditional), and one dependency-weight claim (reqwest 0.13.4 vs 0.12.24, opentelemetry 0.32 vs dev-only 0.31.0) plus the fail-open/verdict:\"APPROVE\" mechanic that directly collides with roundhouse's no-fail-open invariant. Every single one checked out exactly as stated, down to specific line numbers -- the document's file:line citations are trustworthy to an unusual degree of precision everywhere I looked, with zero drift even on multi-part compound claims. I did not verify: the crates.io-fetched claims (finding 1.2.1's 'eleven exports at tag v0.2.0 vs seven on main', the v0.7.3/0.8.0 nemo-relay-types version claim, published-package data) since those require live network fetches against crates.io/raw.githubusercontent.com rather than the pinned local clones; the large churn/commit-count tables in section 3.1 and the git-log-derived 'six commits, byte-identical crates' claims in section 1.1 (plausible and internally consistent but not independently re-run); the ~40 other individual file:line citations scattered through sections 2, 4, and 5 (e.g., TokenBreakdown::cache_hit_ratio, the MCP tool-name array, ToolSignals' 16 fields, the Cargo.lock rand/uuid unification rows). Given the flawless hit rate on a stratified, adversarially-chosen sample spanning every dangerous category, and that this author's citation discipline showed no near-misses or rounding even under close scrutiny, I'd treat the unchecked remainder as highly likely accurate -- but I would still spot-check any single claim from it before it individually decided a close call, since I only sampled roughly 9 of well over 150 file:line citations in the full document."


---

## Re-read 2026-08-21 — Switchyard half, `5341f71` → `053a61e`

Vigilance pass under `CLAUDE.md`'s synergy rule, before the ToolSignals port
lands. Fresh full clone; **`053a61e`** ("fix: re-emit captured provider
extensions when encoding to the Responses format", #509, 2026-08-21 16:45 UTC)
is `origin/main` HEAD, **23 commits** and 2 days past the pin, 128 files,
+10,527 / −4,440. Every claim below carries `file:line@053a61e` unless it
names another rev. The Relay half of this document is re-read separately; only
the Switchyard-side statements are touched here.

**The port target did not move.** `crates/libsy/src/algorithms/util/tool_signals.rs`
and `util/stage.rs` are byte-identical to `5341f71` (md5 `822def5b…` /
`da0b2402…`), last touched 2026-08-03 (`e012780`, docs-only) and 2026-08-12
(`48b3b71`). A port written against the pin is a port written against HEAD.

**Re-checked and unchanged:** `ToolSignals::from_request` at `tool_signals.rs:253`;
the three scorers at `util/stage.rs:250` / `:323` / `:373`; `ToolSignalProcessor`
still unreachable (`algorithms/util.rs:13`, `pub(crate) mod tool_signals`);
`switchyard-protocol`'s six dependencies (`crates/protocol/Cargo.toml:18-24`);
`Metadata`'s 16 `pub` fields (`metadata.rs:160-196`); toolchain `1.96.1` and
edition 2024, identical to ours; Apache-2.0 with the same
`SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES`
header both trees carry.

**Corrected:** `ToolSignals` has **14** fields, not 16 (finding 7, §(h), S-c).
`Metadata::from_headers` resolves **16** named fields, not 15 — this
document's own list already had 16 (finding 8). Both are this document's
arithmetic, not upstream movement.

**Moved, with product consequences** — each noted at its claim above:

| # | What moved | Where it lands |
|---|---|---|
| `43fc1a7` (#501) | Python launchers + `switchyard` CLI **deleted** (2,503 lines); `requires_openai_auth` now appears **nowhere** in the tree | findings 1.2.2 / 1.2.3, E2, open question 3 |
| — | `caller_auth_kind` survives as public Rust (`switchyard-server/src/lib.rs:324-329`, `config.rs:213-221, 338-343`) | the route-property hypothesis keeps its evidence; M7's citation changes |
| `053a61e` (#509) | Responses encoder gained the extensions allowlist incl. `prompt_cache_key` (`codecs/responses/buffered.rs:1476-1495`, `copy_responses_request_extensions`) — **before today, an IR-mutating Switchyard hop stripped it, and roundhouse 422s without it** (`responses_api.rs:236-240`) | the "fourth hazard" paragraph |
| `0acde7b` (#439) | `serde_json` `preserve_order` enabled — object key order is semantic for `response_format.json_schema` | same paragraph |
| `c7beccd` (#384) | new `switchyard-translation/src/codex_namespaces.rs` — codex's `namespace` container flattened to `<namespace>__<tool>` and restored on the way back | see below |
| `#505`, `metadata.rs:19,258-275` | `x-codex-turn-metadata.thread_source` — current codex marks children by lineage, not `subagent_kind` | finding 8, adoption S-a |

**The one genuinely new asset for us: `codex_namespaces.rs`.** Switchyard
independently arrived at roundhouse's own tool-call dialect problem and solved
the half we deferred. Codex dispatches on `(name, namespace)` and expects the
`namespace` field back on the call it receives
(`codex_namespaces.rs:4-25@053a61e`); an OpenAI-compatible upstream takes flat
`function` tools only, so their request codec flattens each container to
`<namespace>__<tool>` (`:33,42-43`) and carries the reverse mapping in
`ProviderExtensions` under a prefixed key so no codec leaks it outbound
(`:39,56-61`). Their separator is `__` and their worked example is
`mcp__open_websearch__search` (`libsy-llm-client/src/client.rs:2057-2122`) —
the exact construction `roundhouse-server/src/dialect.rs:29-33` names as the
future flat spelling it owes canonicalization a reverse mapping for. When the
second `ClientDialect` arm lands, this file is its design reference, and the
`ProviderExtensions` trick is the answer to "where does the mapping live
without a provider-neutral type growing a codex field".

**Negative claims re-verified, one with corrected phrasing.** Switchyard still
has **no MCP server and no MCP client**: `grep -rn 'rmcp|tools/list|jsonrpc'`
over `crates/` returns nothing at `053a61e`. But the 2026-08-19 checker's
grep-shaped restatement — `grep -rli mcp crates/ switchyard/` → no matches —
is now **stale**: it hits three files (`codex_namespaces.rs`,
`libsy-llm-client/src/client.rs`, `switchyard-server/tests/server.rs`), all of
them namespace *string* handling. The substance holds; the test for it does
not.

**Also landed, watched not adopted:** `switchyard-soak` (a new 3,200-line
scenario-driven soak harness, `crates/switchyard-soak/`); `algorithms/subagent.rs`
+ `util/subagent.rs` (sub-agent routing built on the `thread_source` lineage
above); `util/robustness.rs` (`safe_error_summary` — an exhaustive match that
strips upstream bodies and model replies out of judge-path logs, deliberately
not defaulting to `to_string()` so a new error variant cannot start leaking by
omission; the same discipline `validate/` should be held to);
`switchyard-server` gained a `/decision` endpoint (#456) and base-URL
validation (#405); and `6aed489` (#492) moved
`usage_metrics.rs` to commit usage **before** yielding the terminal event,
because a Responses client may drop the stream the instant `response.completed`
arrives and the wrapper never resumes — structurally absent here, since
roundhouse settles every turn at the engine's one settle seam
(`engine.rs:735-800`) rather than in a stream wrapper, but it is the failure
mode any pass-through metering would inherit.

### The Relay↔Switchyard seam, from the Switchyard side

Prompted by the Relay re-read (Relay deleted its built-in Switchyard
integration in `88d1b1b`, ~4,700 lines, and its migration guide points at
"Switchyard 0.3.0" shipping a dynamic plugin). From this tree:

- **Switchyard main is `0.2.0`, not 0.3.x** (`Cargo.toml:18@053a61e`,
  `[workspace.package] version = "0.2.0"`). Latest tag in the repo is
  `v0.2.0`; there is no 0.3 tag or branch.
- **No Relay plugin crate exists on main.** Workspace members at HEAD are
  libsy, libsy-llm-client, protocol, switchyard-py, switchyard-server,
  switchyard-skill-distillation, switchyard-soak, switchyard-translation
  (`Cargo.toml:6-15`) — no plugin. `grep -rn 'extern "C"|no_mangle'` over
  `crates/` returns nothing, and the words "relay"/"NeMo Relay" appear
  **zero** times in `CHANGELOG.md`, `docs/`, or `README.md`. The only Relay
  presence on main is two correlation-header constants,
  `x-nemo-relay-session-id` and `x-nemo-relay-subagent-id`
  (`crates/protocol/src/metadata.rs:39-40`).
- **The plugin exists, unmerged.** `origin/feature/nemo-relay-plugin-owned-http-client`
  (tip `06dd8ea`, 2026-08-20 22:02 −0700; 6 commits ahead of main, 22 behind)
  adds `crates/switchyard-nemo-relay-plugin/` — 4,461 lines over its merge
  base, opened by `a608c33` "feat(relay): add Switchyard-owned HTTP dynamic
  plugin". It is `crate-type = ["cdylib"]` and `publish = false`, depending on
  `nemo-relay-plugin` plus switchyard-libsy / -llm-client / -protocol /
  -translation (`crates/switchyard-nemo-relay-plugin/Cargo.toml:15-30@06dd8ea`),
  and it carries its own `relay-plugin.toml`, `config.schema.json`, and a
  bundle-packaging script. Two older Relay-integration branches
  (`topic/nemo-relay-integration`, `rlempka/libsy-relay-integration`) are 250+
  commits stale.
- **What that means for us.** The seam is **open on both sides right now**:
  Relay rejects the old config, and the replacement is an unmerged branch
  under a version Switchyard has not cut. So the chained topology this
  document costed as "Relay with Switchyard inside" is not a shipping
  configuration at either project's HEAD, which removes the last standing
  argument against the round-2 launch-surface ruling rather than changing it —
  roundhouse's own M9 config generation is, by elimination, the only front end
  that works end to end today. **The ToolSignals port is untouched**: its two
  files are byte-identical across all of this, the plugin branch does not edit
  them, and `publish = false` + `cdylib` means the plugin produces no library
  artifact anyone could consume in place of the port. If anything it hardens
  the port-not-crate call — the only consumable route to those scorers is
  still `switchyard-libsy` whole, with the five dependency families and the
  OTel major split priced above.

### The `ToolSignals` port table

Switchyard field (`tool_signals.rs:206-246@053a61e`) → what supplies it in
roundhouse (`validate/exchange.rs`, `validate/trigger.rs` @ this tree). Twelve
of fourteen port; the two that do not are the two that read something
`Evidence` does not carry.

| Switchyard field | Derivation upstream | Roundhouse input | Verdict |
|---|---|---|---|
| `severity: f32` | max of `classify_text` over the last `recent_window` tool-result texts (`:416-423`); table at `:32-95`, `SOFT 0.3 / HARD 0.7 / CRITICAL 1.0` (`:25-27`) | `Evidence::exchanges[..].output`, **via `tool_output_body`** | ports — new `SignalKind::ErrorSeverity`. *Orthogonal to* `ToolFailureStreak`, not wider than it: `reads_as_failure` catches `{"success": false}` / `{"error": …}` that no `ERROR_PATTERN` matches, and `ERROR_PATTERNS` catches `traceback (most recent call last)` mid-body that the anchored check misses. The new part is the *window* — this fires on 1-of-3 where the streak needs 3 consecutive |
| `no_error_streak: u32` | trailing run of clean results (`:543-553`) | same | ports; the trigger's first "things are fine" quantity |
| `edit_count` / `write_count` / `read_count` / `todowrite_count` | `classify_tool_call` over every call (`:302-331`, `:441-477`) | `Exchange::name` + `Exchange::arguments` | ports — but `arguments` is a `String` here and their `command_of` reads `Value["command"]` (`:390-395`), so the port owes a `serde_json::from_str` first |
| the four `recent_*` counters | same, over the last `recent_window` calls (`:430-476`) | same | ports |
| `pure_bash_streak: u32` | trailing run of `Other`-category calls (`:441-449`) | same | ports; the "build pit" proxy we have no vocabulary for |
| `tests_passed: bool` | `TEST_PASS_PHRASES` in the window, minus `TEST_FAILURE_LITERAL` and `has_nonzero_failure_count` (`:555-598`) | stripped body | ports as a **gate condition, not a `Signal`** — `Signal::detect` only ever says "trouble" (`trigger.rs:150-155`); a "the turn is settling" fact has no slot |
| `turn_depth: u32` | `messages.len()` (`:343,379`) | **nothing** — `Evidence` carries `exchanges` + `turn_tokens` only (`trigger.rs:127-133`) | not portable without widening `Evidence`; upstream itself calls it "wire-format dependent" and "approximate across request origins" (`:236-238`), so a count of *exchanges* is the better roundhouse quantity anyway |
| `compacted: bool` | any `Text` block contains `"session is being continued"` (`:371-373,386`) | **nothing, three ways** — `exchanges()` drops `ItemContent::Text` outright (`exchange.rs:91`) so the scan has no input; the marker is Claude Code's, not codex's; and the self-latching premise fails because roundhouse forks a compacted conversation onto a fresh empty session (`responses_api.rs:330-340`), leaving no prefix to latch onto. M9's generated config also disables *both* codex compaction paths — `auto_compact_token_limit: null` (`codex_launch.rs:496`) for the local one, provider `name = "Roundhouse"` rather than `"OpenAI"` for the remote one (`codex_launch.rs:415-419`) — though neither excludes a user-invoked compact | not portable as written; revisit only if a Claude Code surface lands |

Two consequences the table makes concrete:

1. **The codex exec header is half signal and half noise, and the port must
   split it** — the error-pattern table runs over `tool_output_body(output)`,
   while the exit code is read *from* the header as a structured fact rather
   than pattern-matched out of it. Feeding the raw string in fires
   `exit_nonzero` on every exec including exit 0; feeding only the body loses
   the exit status entirely. See §(h), including the separate
   `reads_as_failure` claim the same accessor would close.
2. **`dimensions_from_signal` / `score_signal` / `pick_tier` do not port into
   the `Signal` seam at all.** `spinning` and `exploring` are gated on
   `turn_depth >= STALL_MIN_TURN_DEPTH` (`stage.rs:36,250-274`), so they are
   blocked on the row above; and `pick_tier` answers "which tier", which is a
   `routing/policy.rs` question, not a `validate/trigger.rs` one. What the
   `Signal` seam wants is the extractor.

**Attribution form for a code port.** There is no precedent in this tree —
`validate/prompt.rs` is a *text* port (HTML-comment header in the `.md`, plus
`the_attribution_travels_with_the_file_and_not_with_the_prompt` pinning repo,
40-char rev, both source paths and the licence), and
`control/credential/forwarded.rs` / `fleet/src/openai_responses.rs:25` are
design references cited in module docs with no header. For Rust carrying
Switchyard's *logic*, the form that matches the house is prompt.rs's:
a module-doc `# Attribution` block naming `NVIDIA Switchyard (Apache-2.0)`,
the full 40-char rev the port was read at, and
`crates/libsy/src/algorithms/util/tool_signals.rs`, plus a test asserting the
citation survives an edit. Both trees carry the identical
`SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES`
line and the same Apache-2.0, so what attribution owes here is **provenance and
revision**, not a third-party copyright notice — and the revision is the half
that rots, which is why it is pinned by a test rather than left in prose.

---

## Re-read 2026-08-21 — Relay half, `ca08901` → `1a54812`

Upstream re-cloned fresh. **The repository is `github.com/NVIDIA/NeMo-Relay`;
`NVIDIA-NeMo/NeMo-Relay` 404s** — the crate metadata is the authority
(`nemo-relay-types-0.7.3/Cargo.toml:23`), and it is worth writing down because
Switchyard genuinely is under `NVIDIA-NeMo/`, so the two NeMo neighbours sit in
different GitHub orgs.

| rev | date | what |
|---|---|---|
| `1a54812` | 2026-08-21 17:22 −0400 | HEAD of `origin/main`, "Merge pull request #855 from NVIDIA/release/0.8". Workspace `version = "0.9.0"` (`Cargo.toml:23`); `rust-toolchain.toml:5` still `1.96.1`. |
| `513b7da` | 2026-08-20 21:32 −0500 | tag `0.8.0-rc.1` (`ca08901` is an ancestor) |
| `ca08901` | 2026-08-19 | this document's pin — 42 commits, 135 files, +13,203/−6,643 behind HEAD |
| `c37b551` | 2026-08-18 | `nemo-relay-deep-dive.md`'s pin — 48 commits behind |

Secondary tree for the converter question:
`github.com/NVIDIA/NeMo-Agent-Toolkit` @ `c933737` (2026-08-17). Crate tarballs
`nemo-relay-types-0.7.3` and `-0.8.0-rc.1` from `static.crates.io`. **No cargo
invocation** — the box has one build lock shared with two concurrent dives — so
every dependency claim is derived from manifests, the sparse index and
roundhouse's own `Cargo.lock`, and the `uuid` resolution below is argued from
requirement strings rather than demonstrated by a resolver run.

### What moved

1. **`nemo-relay-types 0.8.0-rc.1` was published**, 2026-08-21T02:53:49Z —
   about twenty hours before this pass, and byte-identical to
   `ca08901:crates/types/src` across all twelve files. The round-2 ruling's
   "move when they publish" condition has fired. Details and the three forms
   of the pin: the notes at finding 11 and §2(e) above.
2. **Relay deleted `crates/switchyard` entirely.** `88d1b1b` (2026-08-19),
   `refactor(switchyard)!: remove built-in service integration (#811)` —
   removes the `nemo-relay-switchyard` crate, the CLI `switchyard` feature and
   the service-backed component (~4,700 lines); a config carrying
   `[[components]] kind = "switchyard"` is now **rejected** with a migration
   diagnostic (`HEAD:crates/cli/src/server/mod.rs`). Three citations in
   `nemo-relay-deep-dive.md` — overlap rows 5 and 10, and "Three facts" #2 —
   point at files that no longer exist at HEAD; they remain valid at
   `ca08901`/`c37b551` and are bracketed in place there.
   `HEAD:docs/reference/migration-guides.mdx:75-101` hands the integration to
   "Switchyard 0.3.0"; the PR body names `NVIDIA-NeMo/Switchyard#270`. The
   Switchyard half of this document, above, records what that looks like from
   the other side — main is still `0.2.0` and the replacement plugin is an
   unmerged branch, so the seam is open on both sides at once.
3. **`crates/types/src/api/registry.rs`** (+105) and `api/mod.rs` (+2) — a new
   public module in the types crate, landed **after** `0.8.0-rc.1` was cut, so
   only a git-rev pin reaches it.

### What did not move, checked because something depended on it

- **The Codex/Claude launch surface is byte-identical.**
  `git diff --stat ca08901 HEAD -- crates/cli/src/agents/codex/ crates/cli/src/agents/claude/ crates/cli/src/provider_auth.rs`
  is **empty**; widened to `c37b551..HEAD` and including `gateway/`, the whole
  diff is one line in `crates/cli/src/gateway/mod.rs`. Every citation both
  dives make here still resolves verbatim at HEAD: `requires_openai_auth=true`
  hardcoded at `crates/cli/src/agents/codex/launch.rs:201`;
  `CHATGPT_CODEX_BASE_URL` at `alignment.rs:23`;
  `chatgpt_upstream_url_if_needed` at `:85`; `has_chatgpt_auth_token`
  (`Bearer eyJ` / `Bearer at-`) at `:121-125`; the 32-byte `nrp_` token minted
  and consumed at `provider_auth.rs:21,37,46`. **So S3's chain guards and M7's
  `requires_openai_auth` evidence need no re-derivation on the Relay side.**
  The version gate is also unmoved — `minimum_version: (0, 143, 0)` for Codex
  (`crates/cli/src/agents/codex/mod.rs:22`), `(2, 1, 121)` for Claude Code;
  the box's `codex-cli 0.146.0` passes.
- **ATIF is byte-identical**, `atif.rs` 3,645 lines at both revisions, still
  `ATIF-v1.7`, still in core. See the note at finding 9.
- **ATOF is still `0.1`**, and the envelope types a producer needs —
  `BaseEvent`, `ScopeEvent`, `MarkEvent`, `Event`, `DataSchema`,
  `ScopeCategory` — are byte-identical between published 0.7.3 and 0.8.0-rc.1.
  The only 0.8 change anywhere in the envelope is one additive optional field,
  `CategoryProfile.tool_result_annotation`.
- **`codec/optimization.rs` is byte-identical across 0.7.3, 0.8.0-rc.1 and
  HEAD** — the savings surface S2 wants has not moved at all since before our
  first pin.

### A second Codex launch surface neither dive recorded

Not a change — it predates both pins — but a gap in the evidence that the M9
launch-surface comparison should close. `crates/cli/src/agents/codex/host.rs`
(added `2e4ebd2`, **2026-07-14**, "feat(cli)!: add MCP-managed shared gateway
for coding agents (#395)") is a *persistent installer* alongside the argv
`--config` path both dives describe. It rewrites `~/.codex/config.toml` in
place with `toml_edit`, atomically and privately, with backup, restore and
uninstall (`host.rs:48,126,163,1732,1772`). Two differences from the argv path
matter. The provider is named `"NeMo Relay"` rather than
`"NeMo Relay OpenAI"`, and the credential travels as a **static**
`http_headers = { "x-nemo-relay-client-token" = <token> }` written to disk
(`host.rs:844-846`; the constant is at
`crates/cli/src/configuration/mod.rs:480`) rather than the argv path's
env-indirect `env_http_headers` (`launch.rs:199-205`). Everything else is
shared: `requires_openai_auth = true`, `wire_api = "responses"`,
`supports_websockets = false`, `features.hooks = true`, `multi_agent_v2`
disabled (`host.rs:838-841`).

The reason to record it: roundhouse's `codex_launch.rs` writes a Codex config
too, and Relay has already solved the hardest part of the persistent variant —
**uninstalling cleanly from a file the user also edits**. It keeps a
challenge/`client_token` proof so it can tell installer-owned fields from user
edits (`host.rs:800-830`, `codex_provider_has_only_generated_fields`), and the
backup-refresh comment at `:860-870` states the failure mode exactly:
*"Reusing the whole file as the new backup would make those generated fields
survive uninstall."* That is a design reference for free, in the same class as
Switchyard's `forward_auth` (row S-e).

### The ATOF→ATIF converter, re-checked

`NeMo-Agent-Toolkit@c933737` ships `packages/nvidia_nat_atif/` — a 1,039-line
converter (`src/nat/atof/scripts/atof_to_atif_converter.py`), pydantic models
of `AtifStepExtra`, eight test modules including `test_spec_compliance.py` and
`test_atif_v17_validators.py`, committed worked examples, and a second bridge
at `nvidia_nat_core/src/nat/experimental/relay_telemetry_bridge.py`. It is
alive and larger than when the ruling cited it.

What it expects for custom marks, precisely, since S2 promises it "consumes
them without new code": `data_schema` is `{name, version}` and dispatch is
**per event** across three registries — LLM, tool, mark
(`extractors.py:753-757`, resolvers at `:791-820`). `MARK_EXTRACTOR_REGISTRY`
and `TOOL_EXTRACTOR_REGISTRY` ship **empty**, and an unregistered
`(name, version)` falls through to `NatRoleMarkExtractor` (`:751`), which lifts
a mark to a sourced step only when `data.role ∈ {"user","system","agent"}` and
otherwise emits `{"source": "system", "message": json.dumps(data)}`
(`converter:638-643`). So the promise holds in the weak sense — the trajectory
stays valid, nothing is dropped — and fails in the sense the ruling meant:
routing facts arrive stringified and structurally invisible. Three options,
priced: emit `data.role: "system"` plus `data.content` (zero code, still
unstructured); register a mark extractor upstream (~20 lines of Python, but a
PR in their repo); or emit routing decisions as **`category: "context"`
scope-ends**, which is the only converter path that copies `data_schema` into
ATIF `step.extra` verbatim (`converter:672-673`). One asymmetry worth knowing:
only the *mark* path degrades gracefully — an LLM scope event whose non-empty
`data` yields nothing raises `ShapeMismatchError` (`converter:82-95`), a hard
failure.

### Everything re-checked this pass

crates.io version ladder and publish dates (sparse index +
`crates.io/api/v1`, the latter needing a `User-Agent` or it returns a
data-access-policy error); `nemo-relay-types` 0.7.3 vs 0.8.0-rc.1 vs HEAD,
file by file and public-item by public-item; `codec/optimization.rs`
field-for-field; `Usage`/`CostEstimate`/`CostSource`; `limitations` typing and
Relay's own limitation vocabulary and `status` derivation
(`HEAD:crates/core/src/api/optimization.rs:290,309,841-845`); the ATOF
envelope types; the dependency list, TLS posture, MSRV and the `uuid` exact
pin against every uuid dependent in roundhouse's lock; `atif.rs` byte-identity
and its twelve struct definitions with fields; the ATIF and ATOF doc
locations at HEAD; the ATOF→ATIF converter and its three registries;
`crates/cli/src/agents/**` and `provider_auth.rs` byte-identity across both
pins; the codex/claude version gates; and the full `ca08901..HEAD` file-level
diff, which is how the `crates/switchyard` deletion surfaced.

**Not checked**: whether Relay treats `0.8.0-rc.1` as API-stable; the
`crates/worker` / `worker-proto` ABI churn (+375 lines, out-of-process plugin
transport, which no roundhouse plan touches); the Node/Python/FFI binding
churn that makes up most of the remaining diff; and the resolver behaviour of
the `uuid = "=1.18.1"` pin, which a later exclusive stage can settle in
seconds with `cargo update -p uuid --precise 1.18.1`.

### The exact tables the port re-implements

Transcribed verbatim from `crates/libsy/src/algorithms/util/tool_signals.rs@053a61e`
so the port does not need the clone. All matching is `text.to_lowercase().contains(sub)`
(`:530-541`) unless noted; severities are `SOFT = 0.3`, `HARD = 0.7`,
`CRITICAL = 1.0` (`:25-27`).

**`ERROR_PATTERNS` (`:32-95`)** — max severity across matches wins.

| Name | Sev | Substrings (lower-cased) |
|---|---|---|
| `oom` | CRITICAL | `out of memory`, `memoryerror`, `cannot allocate memory` |
| `connection_refused` | CRITICAL | `connection refused`, `connectionrefusederror`, `econnrefused` |
| `traceback` | HARD | `traceback (most recent call last)` |
| `import_error` | HARD | `modulenotfounderror:`, `importerror:`, `no module named ` |
| `cmd_not_found` | HARD | `command not found`, `not found\n`, `/usr/bin/env: ` |
| `assertion` | HARD | `assertionerror` |
| `value_error` | HARD | `valueerror:` |
| `syntax_error` | HARD | `syntaxerror:` |
| `timeout` | HARD | `timed out`, `timeouterror`, `timeout expired`, `deadline exceeded` |
| `no_such_file` | HARD | `filenotfounderror:`, `no such file or directory`, `file does not exist` |
| `exit_nonzero` | SOFT | `exit code 1`, `exit code 2`, `exit status 1`, `returned non-zero`, `exited with code` |

Two entries carry upstream provenance worth carrying forward. `no_such_file`'s
third substring is anchored as `file does not exist` rather than a bare `does
not exist` — upstream's comment records it as "trace-mined across 1006 local
trajectories at 22 true / 2 false positives" (`:77-80`). And `exit_nonzero` is
the row the codex header breaks: see the split rule in §(h).

**Tool-name tables (`:97-163`)** — matched against `name.to_lowercase()` exactly.

- `WRITE_TOOL_NAMES` (`:107`): `write`, `create_file`, `new_file`, `write_file`
- `EDIT_TOOL_NAMES` (`:97-105`): `edit`, `multiedit`, `notebookedit`, `str_replace`, `str_replace_based_edit_tool`, `text_editor`, `patch`
- `READ_TOOL_NAMES` (`:147`): `read`, `view`, `read_file`, `search_files`
- `PLAN_TOOL_NAMES` (`:151`): `todowrite`, `todo_write`, `todo`, **`update_plan`** — upstream's comment: "`update_plan` is codex's equivalent of `todowrite`"
- `BASH_TOOL_NAMES` (`:157-163`): `bash` (claude-code), **`shell_command` (codex)**, `shell`, **`local_shell_call`**, `terminal` (hermes)

**Bash-command patterns** — applied to the lower-cased `arguments["command"]`
only for a `BASH_TOOL_NAMES` call, in this order, first match wins
(`:302-331`); write/edit redirection deliberately trumps a read-like operand.

- `BASH_WRITE_PATTERNS` (`:112-126`): `cat >`, `cat >>`, `echo >`, `echo >>`, `tee `, `printf >`, `printf >>`, `> /`, `>> /`, `<< 'eof'`, `<<eof`, `<<'eof'`, `<< eof`
- `BASH_EDIT_PATTERNS` (`:128-138`): `sed -i`, `sed --in-place`, `awk -i inplace`, `awk 'inplace=1'`, `patch `, `patch -p`, `perl -i`, `perl -p -i`, `perl -pi`
- `BASH_READ_PATTERNS` (`:142-145`): `cat /`, `cat ./`, `cat ../`, `grep `, `ls `, `ls -`, `find `, `head `, `tail `, `wc `, `diff `, `which `, `ps `, `df `, `du `, `stat `, `file `, `less `, `more `

Anything matching none of the five categories is `ToolCategory::Other`, which
is what `pure_bash_streak` counts.

**Test-outcome markers (`:165-189`, evaluated at `:555-598`)** — a window
result counts as a pass iff it contains a pass phrase **and** no failure
literal **and** no non-zero failure count. Upstream states the bias
explicitly: "Prefer false negatives: `tests_passed` routes the picker to
EFFICIENT, so a false positive would drop tier on an unfinished task"
(`:165-166`).

- `TEST_PASS_PHRASES` (`:167-178`): `" passed"`, `"passed in"`, `"tests passed"`, `"all tests passed"`, `"test ok"`, `"test result: ok"`, `"passed.\n"`, `"tests pass"`, `"\nok "` (newline-anchored, for `go test`), `"✓ "`
- `TEST_FAILURE_LITERAL` (`:184`): `"✗ "`, `"fatal:"`, `"assertionerror"`, `"error:"`
- `NUMERIC_FAILURE_KEYWORDS` (`:189`): `failed`, `failure`, `failures`, `errors`, `error` — each trips only when a **non-zero** integer precedes it modulo whitespace *and* a non-alphanumeric follows it (`has_nonzero_failure_count`, `:570-598`), so cargo's `0 failed`, go's `0 errors` and `errored` mid-word do not count

**Compaction marker (`:386`)**: `COMPACTION_MARKER = "session is being
continued"`, matched case-insensitively against every `ContentBlock::Text` in
the conversation — Claude Code's compaction preamble, not codex's. See the
port table for why this row does not port.

**Window default (`:197`)**: `DEFAULT_RECENT_WINDOW = 3`, overridable per call
via `ToolSignals::from_request(request, Some(n))`.

**A checked non-finding, recorded because negatives are the dangerous claims.**
`tests_passed` does **not** inherit the header problem. `TEST_PASS_PHRASES`
contains the unanchored `" passed"`, and `TEST_FAILURE_LITERAL` /
`NUMERIC_FAILURE_KEYWORDS` contain `"error:"`, `"fatal:"`, `errors`, `failed`
— so the question is whether codex's header can either fake a pass or veto a
real one. Run rather than reasoned: prepending the full exec header
(`Chunk ID: 1` / `Wall time: 1.0000 seconds` / `Process exited with code 0` /
`Output:`) to a clean cargo summary (`test result: ok. 42 passed; 0 failed; 0
ignored`) yields `tests_passed = true` **both with and without** the header —
identical pass hits, no failure literal, no non-zero count. The reason is
structural: `has_nonzero_failure_count` pairs digits with a keyword only across
*whitespace* (`tool_signals.rs:570-598`), and every digit in the header is
followed by ` seconds`, `\n`, or `Output:` — never by a failure keyword. So
this row ports over either string. Only the severity/streak rows need the
header split.

