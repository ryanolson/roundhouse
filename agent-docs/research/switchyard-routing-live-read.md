<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

> **Status: evidence.** Produced 2026-08-24 as M10 stage 0. Switchyard's live routing vocabulary, calibrated profiles, benchmark harness, and steering mechanism, read at 053a61e.
> Clone/snapshot paths named inside refer to the research session's own
> workspace; the revisions and URLs are what pin each claim. The ruling this
> evidence supports is agent-docs/PLAN-frontier-selection.md.

# M10 Dive 1 — switchyard-recipes

**Rev discipline.** Every claim below is `path:line@053a61e` against
`scratchpad/upstream/switchyard`, HEAD = `053a61e2c43ba15f0772952ec3b3060c24b317f2`
("fix: re-emit captured provider extensions when encoding to the Responses
format (#509)", authored 2026-08-21 16:45 UTC). `git fetch origin --prune` run
today, 2026-08-24: **`origin/main` did not move** — 053a61e is today's HEAD. The
fetch pulled one new branch, `origin/bbednarski/relay-plugin-runner`, and moved
`gh-pages`; neither is a research target and neither is cited here. Workspace
version is `0.2.0` (`Cargo.toml:18@053a61e`, `pyproject.toml:7@053a61e`); tags
present: `v0.0.1 v0.1.0 v0.2.0 v0.2.0-rc.1 v0.2.0-rc2`. No cargo was run.

**Roundhouse-side citations carry a second rev.** Anything cited as
`crates/…` or `agent-docs/…` without `@053a61e` is against
`/home/ryan/repos/roundhouse/.claude/worktrees/roundhouse-m9-codex-e2e-66222c`
at `dbfd4fd` (branch `claude/m9-codex-e2e`, tree clean) — **except** the §3
citations marked `@cc1245a` / `@fe73e5f`, which are on sibling branches that are
not merged into `dbfd4fd`. See §3.0.

**A prior dive already covered part of this ground at the same upstream rev.**
`agent-docs/research/relay-switchyard-dedup-deep-dive.md@fe73e5f` (branch
`claude/synergy-round-3`, "Synergy round 3: re-read every neighbour before the
follow-ons land", 2026-08-21) re-read Switchyard at `053a61e` and carries dated
corrections on the scorers, the advisor gate, the escalation module's
visibility, and `requires_openai_auth`. **This dive does not re-litigate any of
it** — §3 cites its ruling rather than re-deriving one. What round 3 did *not*
cover, and what is new here: `benchmark/` and the Harbor harness (§2), the
calibrated routing profiles (§2.3), `dev-server/config.toml` and the sol/luna
wiring (§1.5), `POST /v1/decision` (§5.2), the litellm sol/kimi-k3 example
(§5.3), and the recipe-vocabulary question itself (§0). A `grep` of that
document for `benchmark/`, `harbor`, `openrouter`, `routing-profiles`, `tau2`,
`v1/decision`, `dev-server`, `kimi`, `terminal-bench`, `recipe` returns hits
only in an unrelated pricing-provenance section (`:810-830@fe73e5f`) and one
fact-check footnote.

**The brief's "2026-08-21 'stage router integration' commits" do not exist under
that name.** `git log --since=2026-08-18@053a61e` has no such subject. The
2026-08-21 commits are `053a61e` (#509 Responses provider extensions) and
`a8c4d2ed` (#505 subagent awareness). The stage-router-plus-litellm integration
the brief likely means is `examples/experimental/litellm/` (§5.3), which is not
dated to 2026-08-21 in the log.

---

## 0. Headline: "recipe" is dead vocabulary, and the live noun is not a synonym

The product owner's phrasing — "model-select per Switchyard guidance/recipes",
"one of Switchyard's best recipes" — **does not name a live Switchyard concept.**
`git grep -in recipe` over the whole tree returns exactly three hits, and two of
them are the CHANGELOG recording the thing's removal:

- `CHANGELOG.md:241-243@053a61e` (under `## [0.1.0] — Initial release`, heading at
  `:211`): "**Python library** — `SwitchyardRecipes` (`passthrough_recipe`,
  `random_routing_recipe`, `cascade_recipe`, `deterministic_routing_recipe`, …)".
- `CHANGELOG.md:34-45@053a61e` (`## [Unreleased]` → `### Removed`) removes the
  whole surface that carried it: "**Python coding-agent launcher CLI** — the
  `switchyard` command, its Claude Code, Codex, and OpenClaw wrappers, and the
  shared launcher runtime are removed" (`:36-38`), and "**Deprecated Python
  server stack** — `switchyard serve`, YAML route bundles, the FastAPI endpoints
  and legacy chain, the `switchyard-components` crate, and their compatibility
  PyO3 bindings are removed. Use `switchyard-server` with native TOML
  deployments." (`:39-42`).
- The third hit is an unrelated English word in a judge prompt
  (`crates/libsy/src/prompts/escalation/prompt.md:46@053a61e`).

Removal is confirmed in the tree, not only in prose: commit `43fc1a71`
(2026-08-20) "chore: Remove Python launchers and `switchyard` CLI (#501)". So
`switchyard launch codex` — the one-command "point Codex at Switchyard" path the
0.1.0 CHANGELOG advertised (`CHANGELOG.md:233-238@053a61e`) — **no longer
exists.** Their instruction now is: connect clients directly to the standalone
native server.

**The live vocabulary, in their words** — three nouns, one file:

| Layer | TOML table | What it is |
|---|---|---|
| **LLM client** | `[llm_clients.<name>]` | how to reach a provider: `format` (`openai_chat` / `openai_responses` / `anthropic_messages`), `base_url`, `api_key_env`, `forward_auth`, `extra_headers`, `max_retries` |
| **Target** | `[targets.<name>]` | a model on a client: `id` (exact upstream model id), `llm_client`, `extra_body` |
| **Route** | `[routes.<name>]` | the algorithm + its targets. `id` is the model name clients send |

`docs/routing_algorithms/overview.md:22-56@053a61e`;
`docs/reference/toml_schema.md:40-96@053a61e`; the enum itself is
`RouteConfig` at `crates/switchyard-server/src/config.rs:464-578@053a61e`.

**The route `type` set as of today** (`config.rs:466-578@053a61e`, serde
`tag = "type", rename_all = "snake_case", deny_unknown_fields`):
`noop`, `random`, `passthrough`, `llm_classifier`, `stage_router`, `advisor`.

Renamed-vs-removed, against the 0.1.0 list at `CHANGELOG.md:224-230@053a61e`
(`model`, `passthrough`, `random_routing`, `cascade`, `deterministic`,
`latency_service`, `noop`):

- `passthrough`, `noop` — survive under the same name.
- `random_routing` → `random`.
- `deterministic` (LLM-as-classifier) → `llm_classifier` with
  `mode = "capability"`.
- `cascade` → **gone as a route type**; its signal-driven escalation idea is now
  split two ways: per-turn bidirectional signal routing is `stage_router`, and
  latching escalation is `llm_classifier` with `mode = "escalation"`.
- `model`, `latency_service` — **no surviving type of that name.**
- New since: `advisor` (`CHANGELOG.md:11-18@053a61e`), and `subagents` as a
  nested policy on `passthrough`/`stage_router`.

**Doc drift worth noting:** `docs/reference/toml_schema.md@053a61e` documents
`noop`/`passthrough`/`random`/`llm_classifier`/`stage_router` (`:98-212`) but
**does not document the `advisor` route at all**, even though `config.rs:542-577`
accepts it and `docs/routing_algorithms/overview.md:20` and
`advisor_gate_routing.md` describe it. The schema reference is one route behind
the binary.

**Therefore:** when M10 says "per Switchyard guidance/recipes", the honest
translation is **"per a Switchyard `[routes.*]` TOML stanza of a named `type`"**,
and the closest thing to a shipped "recipe" is the small set of *calibrated,
checked-in configuration files* in §2 below. Those are real, they carry measured
numbers in their comment headers, and one of them cites a published NVIDIA blog
result. That is what "one of Switchyard's best recipes" can defensibly mean.

---

## 1. The stage router: schema, tiers, and a real example

### 1.1 The tier names are `Capable` / `Efficient`, labelled `strong` / `weak`

The brief guessed `EFFICIENT/STANDARD/PREMIUM`. The real type is a **two-variant**
enum — there is no third tier anywhere in libsy:

```rust
// crates/libsy/src/algorithms/util/stage.rs:71-91@053a61e
pub enum Tier { Efficient, Capable }
impl Tier {
    fn label(self) -> &'static str {
        match self { Self::Capable => "strong", Self::Efficient => "weak" }
    }
}
```

The comment at `:81-84` states why the label differs from the variant: "Stable
label for stats … independent of what the tiers' targets are called. These are
the strings the capability route reports too, so a deployment running both sees
one tier vocabulary." So **`Capable`/`Efficient` is the internal axis,
`strong`/`weak` is the reported vocabulary, and the deployment's own target names
are a third naming layer** bound by `StageTargets` (`stage.rs:93-131@053a61e`).

Supporting types, all in `crates/libsy/src/algorithms/util/stage.rs@053a61e`:

| Item | Line | Shape |
|---|---|---|
| `Tier` | `:73-78` | `Efficient` \| `Capable` |
| `StageTargets` | `:99-131` | `{capable: ModelId, efficient: ModelId}` + `name(tier)`, `label_for(target) -> Option<&'static str>` |
| `PickerMode` | `:136-151` | `CapableFirst` \| `EfficientFirst` (serde snake_case); `default_tier()` |
| `DecisionSource` | `:171-198` | `Override`, `TestsPassed`, `Dimensions`, `Ambiguous`, `LlmClassifier`, `FallOpen` → labels `override`, `tests_passed`, `dimensions`, `ambiguous`, `llm-classifier`, `fall_open` |
| `ScoreResult` | `:202-207` | `{score: f64 in (-1,+1), confidence: f64 = score.abs()}` |
| `CodingAgentDimensions` | `:211-220` | `{severity, spinning, exploring, production_intensity}` |
| `PickOutcome` | `:225-247` | `Resolved{tier, source, score, confidence}` \| `ConsultClassifier{score, confidence, default_tier}` |
| `HandoffNoteConfig` | `:439-493` | `{escalation_note, deescalation_note: Option, only_on_wrong_signal_escalation: bool = true}` |

### 1.2 The scoring constants, verbatim

`crates/libsy/src/algorithms/util/stage.rs:36-48@053a61e`:

```rust
const STALL_MIN_TURN_DEPTH: u32 = 8;   // below this, no-write turns are normal exploration
const SCORE_GAIN:        f64 = 5.0;    // tanh gain; without it confidence caps near ±0.20
const HARD_SEVERITY:     f64 = 0.7;    // normalises severity to one signal unit
const SIGNAL_UNIT:       f64 = 0.10;   // one maxed signal's weight
const SEVERITY_CRITICAL: f32 = 1.0;    // forces capable regardless of the scorer
```

Scoring (`:323-335@053a61e`) is one line of arithmetic:

```
raw   = 0.10 * (severity/0.7 + spinning + exploring - production_intensity)
score = tanh(5.0 * raw)          // signed: + → capable, − → efficient
confidence = |score|
```

The doc comment at `:317-322` gives the operator reading: "`~0.3` escalates on
one signal, `~0.5` needs about one-and-a-half, `~0.7` needs two to corroborate."
The test `one_signal_scores_below_half` (`:639-650@053a61e`) pins it: a single
maxed severity signal is `≈0.46`, deliberately just under `0.5`.

`dimensions_from_signal` (`:250-272@053a61e`) — the projection:

```
recent_ops   = recent_write + recent_edit + recent_read + recent_todowrite
deep_enough  = turn_depth >= 8
no_production= recent_write == 0 && recent_edit == 0
investigating= recent_read >= 1 || recent_todowrite >= 1
spinning  = deep_enough && no_production && !investigating   → 1.0/0.0
exploring = deep_enough && no_production &&  investigating   → 1.0/0.0
production_intensity = (recent_write + recent_edit) / recent_ops   // 0 if denom 0
severity  = signal.severity as f64
```

`spinning` and `exploring` are mutually exclusive by construction — the comment at
`:258-259` says why: "at most one fires — no double-counting on the production
axis."

### 1.3 `pick_tier` — the four ordered rules

`crates/libsy/src/algorithms/util/stage.rs:373-408@053a61e`, doc at `:357-372`:

1. **Escalate (hard override)** → `Resolved{Capable, Override, score: 0.0,
   confidence: Some(1.0)}`. Fires when `should_escalate` (`:339-347`): `compacted
   == true` **or** `severity >= 1.0`. The compaction rationale (`:340-342`):
   "Compaction wipes the accumulated signals, so a task that had escalated would
   snap back to efficient — a context big enough to overflow belongs capable."
2. **De-escalate (hard shortcut)** → `Resolved{Efficient, TestsPassed, 0.0,
   None}`. `should_deescalate` (`:351-355`): `tests_passed && (recent_write +
   recent_edit) >= 1 && severity <= 0.0`.
3. **Scorer** → if `confidence >= confidence_threshold`, `Resolved{sign(score),
   Dimensions, …}`.
4. **Fall open** → `ConsultClassifier{score, confidence, default_tier:
   mode.default_tier()}`.

Escalate is checked *before* de-escalate on purpose (`:366-368`): "a critical
error still wins on a turn whose tests also happened to pass."

`pick_tier` is **pure, sync, and deterministic** — the async classifier lives in
the caller (`:370-372`). That is what makes it portable.

### 1.4 The assembled route

`crates/libsy/src/algorithms/stage.rs:119-208@053a61e`. `StageRouter` is a
`FallThrough<State>` cascade of exactly three (or four) stages, built in
`build_route` (`:152-208`):

1. `ToolSignalProcessor{recent_window}` (request-side processor, `:176-178`)
2. `StageClassifier` (the pure scorer, `:172-175`)
3. *(optional)* `LlmTaskClassifier` in `LlmClassifierConfig::Capability` mode,
   wrapped in `SourceStamp{source: LlmClassifier}` (`:184-196`) — note `:188-192`
   passes tiers "efficient first, capable second", the same order the standalone
   capability route uses
4. `DefaultTarget::new(fall_open)` stamped `FallOpen` (`:199-202`) — "Nothing
   behind this, so the turn lands on the picker's default tier"
5. A post-decision `SystemPromptProcessor` for per-tier system prompts (`:206`)

Validation: `confidence_threshold` outside `[0.0, 1.0]` is an
`AlgorithmError` at construction (`:157-164`), not at first request.

**API-drift check (CLAUDE.md's "Switchyard changed its core API three times in one
week" warning).** Two changes since the M9-era reads, both real:

- `Algorithm::route` now returns `Result<RoutingOutcome>`, not the bare final
  `Result` — `crates/libsy/src/algorithms/stage.rs:142-148@053a61e`
  (`async fn route(self: Arc<Self>, driver: Driver, request: Request) ->
  Result<crate::RoutingOutcome>`); CHANGELOG `:29-32@053a61e` (#459), landed
  `0cf6439f` 2026-08-18.
- LLM-classifier routing is now available through native PyO3 bindings
  (CHANGELOG `:26@053a61e`, #465), and `session_affinity` was **replaced by
  `classify_trigger`** (`c7b648d0` 2026-08-20, #487) — so any roundhouse note
  citing `session_affinity` is stale. Values are `every_request` (default),
  `user_turn`, `new_session` (`docs/reference/toml_schema.md:158@053a61e`).

`crates/libsy/src/algorithms/util/tool_signals.rs@053a61e` is **byte-identical**
to the copy M9 saved at `scratchpad/tool_signals_upstream.rs` (`diff` clean).
`stage.rs` is what moved, not the signal extractor.

### 1.5 One real, shipped example

`dev-server/config.toml@053a61e` — their own dev deployment, and the single most
M10-relevant file in the tree because **it is already written in sol/luna
vocabulary**:

```toml
# dev-server/config.toml:1-51@053a61e
schema_version = 1

[llm_clients.inference_hub]
format = "openai_responses"
base_url = "https://inference-api.nvidia.com/v1"
api_key_env = "NVIDIA_API_KEY"

[targets.capable]
id = "openai/openai/gpt-5.6-sol"
llm_client = "inference_hub"
extra_body = { reasoning = { effort = "medium" } }

[targets.efficient]
id = "openai/openai/gpt-5.6-luna"
llm_client = "inference_hub"
extra_body = { reasoning = { effort = "medium" } }

[routes.stage]
id = "switchyard/stage"
type = "stage_router"
capable_target = "capable"
efficient_target = "efficient"
picker = "efficient_first"
confidence_threshold = 0.5
recent_turn_window = 3

[routes.stage.handoff_notes]
escalation_note = "[router-guidance] A weaker model was handling this task and showed signs of stalling, looping, or repeated errors on the preceding steps, so control was escalated to you, a stronger model. Re-examine the current state directly and do not simply repeat the previous approach."
only_on_wrong_signal_escalation = true
```

Same file also defines `random`, `passthrough`, `noop`, `llm_classifier`
(capability, `base_threshold = 0.5`, `classify_trigger = "new_session"`,
`message_hash_fallback = true`) and `advisor` (`executor_target = "efficient"`,
`advisor_target = "capable"`, `max_reviews = 3`, `gate_stall_turns = 30`,
`gate_min_tool_results = 3`) routes over the same two targets
(`dev-server/config.toml:24-75@053a61e`). Note **sol is the capable tier and luna
the efficient tier** in their own configuration.

**Direct M10 read-across (unprompted, but it is the same vocabulary).** The
escalation note at `:50` is *literally a text instruction spliced into the
forwarded request* — see §5.1. That is the mechanism M10 is pivoting toward,
already in production configuration upstream.

---

## 2. Do they ship a benchmark harness? Yes — and it runs **codex against OpenRouter**

**Answer: yes, a real one.** `benchmark/` is a Harbor (Terminal-Bench) runner,
not notebooks and not "the litellm demos".

### 2.1 What the harness is

`benchmark/README.md@053a61e`, titled "Harbor Benchmarks" (`:4`). Its two paths
(`:9-14`): **direct upstream** (Harbor calls the provider, Switchyard disabled)
and **Switchyard routing** (Harbor calls Switchyard, which routes across two
tiers). Entry point is `bash benchmark/run-baseline.sh` and the Switchyard arm is
selected purely by passing `--server-config`.

Task sets it supports (`benchmark/README.md:86-140@053a61e`), all through
`benchmark/prepare_harbor_dataset.py`:

- `openthoughts-tblite@2.0` (the default; "Terminal-Bench Lite")
- `terminal-bench/terminal-bench-2` and `terminal-bench/terminal-bench-2-1`
- `cais/swebenchpro` (SWE-Bench Pro)
- `benchmark/tb_lite_subset_20.txt` — "Curated 20-task iteration subset for
  openthoughts-tblite@2.0 … Used for fast inner-loop A/B tests of routing logic.
  Same 20 every run so deltas are apples-to-apples" (`:1-4@053a61e`), fed via
  `--task-list-file`.

**The agent under test is Codex.** Every checked-in smoke command in the README
passes `--agent codex` (`benchmark/README.md:149-157, 170-179, 205-212, 241-262`),
and the pinned agent versions are baked into the task images:
`CODEX_VERSION=0.144.5`, `CLAUDE_CODE_VERSION=2.1.211`, `OPENCODE_VERSION=1.18.3`,
`NODE_VERSION=20.11.1`, `HERMES_VERSION=3c27eb62…` (`benchmark/agent-versions.env:4-15@053a61e`).
*Note for the codex-pin-vigilance file: 0.144.5 is older than both the box binary
(0.146.0/e363b08) and our Cargo pin (6344a65).*

**The provider is OpenRouter by default.** "The checked-in smoke commands use
OpenRouter's OpenAI-compatible endpoint by default: `export OPENROUTER_API_KEY`"
(`benchmark/README.md:36-40@053a61e`), with
`${UPSTREAM_BASE_URL:-https://openrouter.ai/api/v1}` as the direct-run fallback
(`:146`).

**Closed-book mode is the default and it is enforced by a proxy**
(`benchmark/README.md:198-232@053a61e`): "the proxy allows Switchyard/model
traffic, blocks public cheat sources such as `raw.githubusercontent.com`, strips
hosted web/search/code tools from model API payloads, and adds agent-specific
web-disable settings where supported" (`:214-216`). Implementation lives at
`benchmark/closed_book_proxy/proxy/`. Harbor itself is patched
(`benchmark/patches/harbor-agent-patches.diff`, 25 KB) with a reverse-check
preflight so a stale patch fails before launch (`:63-65`).

### 2.2 What a run reports

Run artifacts under `benchmark/tb_runs/` (`benchmark/README.md:267-289@053a61e`):

| Artifact | Content |
|---|---|
| `run_manifest.json` | command, git state, Harbor patch provenance, dataset digest, copied server config, book-mode settings, agent version pins, log paths, final Harbor status |
| `jobs/<job>/result.json` | Harbor's own outcome — this is where **solve rate** lives |
| `jobs/<job>/<task>/agent/trajectory.json` | per-task agent trajectory |
| `server_metrics_final.prom` | final `/metrics` snapshot |
| `routing_stats_final.json` | final `/v1/stats`: "model and tier calls, errors, tokens, and latency" |

**Explicit limitation, in their words** (`:287-289@053a61e`): "Neither artifact
provides task or trial attribution. … `routing_requests.jsonl` and
`routing_stats_by_task.json` are **not** produced by the Rust server." So
per-task cost attribution is *not* available from the Rust server today —
anything M10 claims per-task must be derived on our side.

A **separate** latency benchmark exists and is not the same thing:
`scripts/benchmark_routing_algorithms.py@053a61e` compares routes with oha +
NVIDIA AIPerf and reports `oha_latency_p50/p99_ms`,
`aiperf_request_latency_p50_ms`, `aiperf_ttft_p50/p99_ms`, `aiperf_itl_p50_ms`,
`aiperf_output_tokens_per_second_per_user_p50`, plus `*_delta_ms` / `_delta_pct`
against a direct baseline and a `routing_overhead_avg_ms` histogram delta from
`/v1/stats` (`:131-157, 461@053a61e`). That is **routing overhead**, not quality.
There is also a 48-hour release soak (`docs/operations/soak_test.md@053a61e`)
whose scenario catalog includes `stage-transitions`, `classifier-mix`,
`context-overflow`, and `failure-pressure` (`:29-46`).

### 2.3 "One of Switchyard's best recipes" — what it can operationally mean

Six checked-in configs, in two directories, and they are **not** interchangeable.
Three are neutral benchmark wiring; three carry *measured operating points in
their comment headers*. If M10 cites "a best recipe", it should cite one of the
latter three and quote the caveat that comes with it.

**`benchmark/server-configs/` — smoke/baseline wiring, no calibration claims:**

| File | Shape |
|---|---|
| `tb-lite-llm-classifier-opus-kimi-gemini.toml` | `llm_classifier` (capability, implicit), `openai_chat` on OpenRouter. classifier `google/gemini-3.5-flash`, strong `anthropic/claude-opus-4.7`, weak `moonshotai/kimi-k2.7-code`, `base_threshold = 0.5`, `threshold_step = 0.0`, `classify_trigger = "new_session"`, `message_hash_fallback = true` (`:7-35@053a61e`) |
| `tb-lite-single-gpt-5-5.toml` | `passthrough` → `openai/gpt-5.5`, **`format = "openai_responses"`** (`:6-18@053a61e`) |
| `tb-lite-single-opus-4-7.toml` | `passthrough` → `anthropic/claude-opus-4.7`, `openai_chat` (`:6-18@053a61e`) |

**`benchmark/routing-profiles/` — the calibrated ones.** These carry numbers.

**(a) `tb21-escalation-opus-glm-deepseek.toml` — the closest thing to "their best
recipe", because it is the configuration behind a *published* result.** Header
(`:4-18@053a61e`):

> "Escalation-router deployment behind the v0.2.0 Terminal-Bench 2.1 efficiency
> results (https://developer.nvidia.com/blog/route-ai-agent-workloads-across-models-with-nvidia-nemo-switchyard/).
> This is the configuration as benchmarked, with one change: the published runs
> were served through NVIDIA-internal inference endpoints, replaced here with
> OpenRouter equivalents so the deployment is publicly runnable. Routing-algorithm
> parameters are exactly as run; absolute solve rates may shift slightly across
> serving stacks."

Body (`:20-57@053a61e`): strong `anthropic/claude-opus-4.8` (reasoning effort
`high`), weak `z-ai/glm-5.2` (effort `high`), judge **`deepseek/deepseek-v4-flash`**
(reasoning `enabled = false`), `type = "llm_classifier"` with
`escalation = { confirmations = 2, recent_turn_window = 28, window_message_chars = 500 }`.
Comment at `:48-49`: "Every conversation starts on the weak tier. The judge
reviews the trajectory each turn, and two consecutive escalate verdicts latch the
session onto the strong tier."

⚠ **The file's own header says it does not load on HEAD. Reading the loader says
it does.** Header (`:15-18@053a61e`): "The `escalation` block is part of the
v0.2.0 server config schema only; **it is not accepted by the current
`switchyard-server`.** Run from the tag: `git checkout v0.2.0-rc.1`".

**The header appears to be stale, and three independent reads say so.** Traced
through `config.rs@053a61e`:

1. The schema reference states the grandfather clause outright: "**Existing
   configurations that contain `escalation` but omit `mode` remain valid**"
   (`docs/reference/toml_schema.md:175@053a61e`). This profile omits `mode`.
2. Mode selection defaults on exactly that shape:
   `(None, true) => ClassifierMode::Escalation` (`config.rs:803@053a61e`), and
   `routing_target_names` branches the same way,
   `config.mode.unwrap_or(if config.escalation.is_some() { Escalation } else {
   Capability })` (`:657-661`).
3. **The one field that looked disqualifying is guarded by `mode.is_some()`.**
   The escalation arm rejects capability settings only when `mode` was written
   explicitly (`config.rs:865-874@053a61e`): `if mode.is_some() &&
   (base_threshold.is_some() || threshold_step.is_some() || *message_hash_fallback
   || recent_turn_window.is_some())`. With `mode` omitted, the profile's
   `base_threshold = 0.5` is **silently ignored rather than rejected**. Its other
   two escalation-arm gates also pass: `classify_trigger` is unset and defaults
   to `EveryRequest` (`:860-864`), and `reject_custom_fields` finds no `targets` /
   `default_target` / `response_schema` / `policy` (`:852-859`).

The header predates the `mode` key — it landed `d52722e5` (2026-08-19, #490);
`mode` and the grandfather clause came with the custom-mode work
(`b3a906eb`, 2026-08-20, #499).

**What would settle it, and what M10 should do:** `switchyard-server --config
benchmark/routing-profiles/tb21-escalation-opus-glm-deepseek.toml --dry-run`
against a HEAD build. I did not run it (no cargo in this dive), so this is a
code-read ruling, not an executed one. **Do not carry "the best-documented recipe
does not load" forward as an action item** — the balance of evidence is that it
does, and the one-command check is cheap.

**(b)(c) `tau2-telecom-custom-opus-qwen-{balanced,aggressive}.toml`** — landed as
`b3a906eb` (2026-08-20, #499). Both are `llm_classifier` with `mode = "custom"`,
`classify_trigger = "user_turn"`, `recent_turn_window = 6`,
`default_target = "strong"`, a frozen JSON-schema verdict
`{route, confidence, abstain}` and `[routes.*.policy] type = "target_selector",
selector = "/route"`. Measured operating points, quoted:

- balanced: "tau2-bench, telecom customer support (multi-turn, tool-using), full
  114 tasks … **0.903 +/- 0.071 solve rate at 45% of turns served by the weak
  tier**" (`tau2-telecom-custom-opus-qwen-balanced.toml:8-12@053a61e`)
- aggressive: "**0.891 +/- 0.029 solve rate at approximately 85% of turns served
  by the weak tier**" (`…-aggressive.toml:8-12@053a61e`)

Both headers repeat the same two disclaimers, and M10 must carry them if it cites
the numbers (`…-balanced.toml:14-27@053a61e`): the routing parameters and rubric
are exactly as run, but "**The model wiring is not**: the measured runs used
NVIDIA-internal endpoints and an on-device weak tier, replaced here with the
closest publicly reachable OpenRouter models … treat the figures above as the
operating point of the calibrated pair, not a prediction for this file as
configured"; and "These numbers describe this tier pair on this traffic. A
different pair of models, or a different traffic domain, will sit at a different
operating point: **recalibrate rather than assuming the thresholds transfer**."

**(d) The threshold that a stage-router recipe would cite.** There is no
calibrated stage-router *file* in `benchmark/`, but the doc states where `0.5`
came from: "Derived from **SWE-Bench Pro Python-75** calibration"
(`docs/routing_algorithms/stage_router_routing.md:116@053a61e`, repeated `:126`),
and `:124-173` gives the full re-calibration protocol — a pure-capable run over
~40–75 tasks, a ~20-task pure-efficient probe stratified across four quadrant
candidates, then RESCUE (`capable-fail ∩ efficient-pass`) / LOSS (`capable-pass ∩
efficient-fail`) / SAFE / HARD quadrants, choosing "the lowest threshold that
rescues the RESCUE quadrant without over-escalating the LOSS quadrant" (`:164-166`).

⚠ **`capable_first` is explicitly unbenchmarked**
(`stage_router_routing.md:77-84@053a61e`): "Every published threshold and routing
result comes from `efficient_first` runs. `capable_first` works and the server
accepts it, but it has not been benchmarked, so there are no calibrated
thresholds for it and no measured accuracy or cost figures … The server logs a
warning at startup when a route selects it." **This matters to M10 directly:**
"mimic a user on a sol-only session; roundhouse reroutes a fraction of calls" is
a *capable-first* shape, which is the arm Switchyard has no numbers for. A
faithful "best recipe" citation is `efficient_first`; a sol-default session is
our own experiment, not theirs.

---

## 3. `pick_tier` / `score_signal` / `dimensions_from_signal` as routing components

### 3.0 Correction: the ruling exists, and so does the port — on sibling branches

My first pass searched only `agent-docs/` on `claude/m9-codex-e2e` and reported
both as absent. **That negative was wrong**, and the search that finds them is
`git log --all --grep` plus `git show <branch>:<path>`. Both live on branches not
merged into `dbfd4fd`:

**The ruling is verbatim, dated, and says exactly what the brief says**
(`agent-docs/research/relay-switchyard-dedup-deep-dive.md@fe73e5f`, in the
`*[2026-08-21, four corrections and one sharpening, all against 053a61e:]*`
block):

> "***The scorers do not belong in the `Signal` seam at all.** `pick_tier`
> answers "which model tier" (`stage.rs:373-405`); `Signal::detect` answers
> "state a fact about trouble" and returns `Option<String>`
> (`trigger.rs:150-155`). Porting `pick_tier` behind `Signal` would be a
> category error, and `SignalFired::fact`'s own rule — "never a suggestion"
> (`trigger.rs:79-88`) — forbids the output shape. **The extractor is what the
> `Signal` seam wants; the scorers, if they are wanted anywhere, belong beside
> `routing/policy.rs`.**"*

**The port has landed** — commit `cc1245a` (branch `claude/toolsignals-port`,
2026-08-21) "Port Switchyard's ToolSignals into the Signal seam; read codex's
exit code as a fact": `crates/roundhouse-core/src/validate/tool_signals.rs`
(+1,458), plus changes to `validate/{exchange,trigger,brief,mod}.rs`. Its dated
addendum (`agent-docs/synergies/ecosystem-round-2.md:82-102@cc1245a`) records
what shipped:

- **Twelve of fourteen fields ported**: `severity`, `no_error_streak`, the four
  cumulative counts, the four windowed counts, `pure_bash_streak`,
  `tests_passed`.
- **`turn_depth` and `compacted` refused**, each with its reason inline —
  "our evidence carries exchanges, not messages, and a compacted conversation
  forks onto a fresh session" (`scratchpad/pr-c.md:5`). The addendum notes the
  irony that round 2's own list had named `compacted` as a field worth having,
  "and it is the one the re-read ruled dead three ways."
- **Two new signals registered in `default_signals()` and no more**:
  `SignalKind::ErrorSeverity` (a named failure at HARD or worse in the last three
  results — kept alongside `ToolFailureStreak`, which is anchored and needs a
  consecutive run, so the two are orthogonal) and `SignalKind::PureBashStreak`
  (four consecutive calls that neither read, wrote, edited, nor planned).
- **The scorers were refused**, per the ruling above.
- The port also forced `exchange::exec_exit_code` and closed a
  `reads_as_failure` blindness it exposed in our own tree, test-first.

**Two further corrections to my own §3 and to the round-2 evidence:**

- **`ToolSignals` has 14 fields, not 16.** Round 3 already caught this
  ("***Sixteen fields is 14***", `@fe73e5f`); I repeated the stale number from
  `relay-switchyard-dedup-deep-dive.md:181` on `dbfd4fd`. Counting
  `tool_signals.rs:206-246@053a61e` gives 14: `severity`, `no_error_streak`,
  `edit_count`, `write_count`, `read_count`, `todowrite_count`,
  `recent_edit_count`, `recent_write_count`, `recent_read_count`,
  `recent_todowrite_count`, `pure_bash_streak`, `tests_passed`, `turn_depth`,
  `compacted`.
- **"Unliftable" is the wrong frame for the scorers, right for the extractor.**
  Round 3 (`@fe73e5f`): the scorers and their types *are* re-exported at
  `lib.rs:34, 38-42@053a61e`, every `ToolSignals` field is `pub`, and the struct
  derives `Default` — so the three scorers are callable from a struct literal
  without ever building a `switchyard_protocol::Request`. What is unreachable is
  `mod tool_signals`'s `pub(crate)` interior (`algorithms/util.rs:13@053a61e`):
  `ToolSignalProcessor`, `classify_text`, and the whole `ERROR_PATTERNS` /
  tool-name / test-phrase table set. **The extractor must be re-implemented
  either way; the scorers need not be.** That reverses the cost picture in §3.4
  and is why the port shipped the extractor and skipped the scorers.

**What §3.2–3.4 below is therefore about.** Not "can we port the scorers" — the
answer is a settled *no* for the `Signal` seam. It is the open question the
ruling deferred: **what a scorer sitting beside `routing/policy.rs` would need,
which is a different question with a different answer, because `RoutingContext`
and `Evidence` are different types on different seams.**

### 3.2 Inputs: what the three functions need vs. what roundhouse has

The scorers' *only* input is `&ToolSignals`. `ToolSignals`'s only input is
`&switchyard_protocol::Request` (`tool_signals.rs:253-255@053a61e`,
`ToolSignals::from_request(&Request, Option<usize>)`), i.e. **the raw request
message array**, windowed to the last `recent_window` tool results
(`DEFAULT_RECENT_WINDOW = 3`, `:197@053a61e`).

The 14 fields (`tool_signals.rs:206-246@053a61e`) — but the three scorers read
only **eight** of them:

| Read by the scorers | Field | Used for |
|---|---|---|
| ✓ | `severity: f32` | `dimensions.severity`; `>= 1.0` is the hard override |
| ✓ | `compacted: bool` | the other half of the hard override |
| ✓ | `tests_passed: bool` | the de-escalate shortcut |
| ✓ | `turn_depth: u32` | `deep_enough` gate (`>= 8`) |
| ✓ | `recent_write_count` | `no_production`, `production_intensity`, de-escalate |
| ✓ | `recent_edit_count` | same |
| ✓ | `recent_read_count` | `investigating`, `recent_ops` |
| ✓ | `recent_todowrite_count` | `investigating`, `recent_ops` |
| ✗ | `no_error_streak`, `edit_count`, `write_count`, `read_count`, `todowrite_count`, `pure_bash_streak` | not scored — `pure_bash_streak`'s own doc says "Surfaced in the classifier state summary; **not scored directly**" (`:231-233`) |

**What `RoutingContext` supplies today**
(`crates/roundhouse-core/src/routing/mod.rs:164-193` in the M9 worktree):
`session_id`, `turn_index: u64`, `isl_tokens: usize`, `candidates: &[Candidate]`,
`ledger: &CacheLedger`, `turn_policy: &TurnPolicy`,
`frontier_history: &FrontierHistory`, `budget: &TurnBudget`.

**The gap, stated plainly: `RoutingContext` carries no message content and no
tool-result history at all.** Of the eight scorer inputs, roundhouse's routing
seam can supply exactly one approximately — `turn_index` is a plausible stand-in
for `turn_depth`, and even that is not the same measure (Switchyard's
`turn_depth` is a *message-count* proxy whose own doc warns it is "Wire-format
dependent (Anthropic batches tool results into fewer messages than OpenAI-chat),
so gates keyed on it are approximate across request origins",
`tool_signals.rs:236-239@053a61e`) — **and `turn_depth` is one of the two fields
the port explicitly refused** (§3.0). Zero of the seven tool-derived fields are
reachable from `RoutingContext`.

The tool history *does* exist on our side, but on the **validate** seam, not the
routing seam: `Evidence<'a> {exchanges: Vec<Exchange>, turn_tokens: &'a [u64]}`,
built by `Evidence::of(&SessionState)`
(`crates/roundhouse-core/src/validate/trigger.rs:127-144`), consumed by
`trait Signal { fn kind(&self) -> SignalKind; fn detect(&self, &Evidence<'_>) -> Option<String>; }`
(`:154-158`). On `dbfd4fd` that seam carries four signals — `NoProgressRepeat`,
`PingPong`, `ToolFailureStreak`, `CostAnomaly` (`SignalKind`, `:62-71`); on
`cc1245a` it carries six, adding `ErrorSeverity` and `PureBashStreak` over a
ported 12-field `validate/tool_signals.rs`.

**So the shape of the remaining problem is a seam crossing, not a port.** Twelve
of the fourteen fields are already computed on `cc1245a` — but on `SessionState`,
behind `Evidence`, for a trait whose contract is "state a fact, never a
suggestion". A tier scorer beside `routing/policy.rs` needs those same numbers as
*data* at routing time. Nothing today carries them from one seam to the other,
and `SignalFired::fact` is a `String`, deliberately not a struct
(`trigger.rs:79-88`), so the existing output is not a usable carrier.

### 3.3 Output: mapping a tier choice onto roundhouse

`PickOutcome` → what roundhouse would need to do with it:

| `PickOutcome` | Switchyard's action | Roundhouse equivalent |
|---|---|---|
| `Resolved{Capable, …}` | route to `StageTargets::capable` | choose the frontier/strong `Candidate` from `Admitted::pool()` |
| `Resolved{Efficient, …}` | route to `StageTargets::efficient` | choose the cheap `Candidate` |
| `ConsultClassifier{…, default_tier}` | fall through the cascade | no cascade seam exists; a policy would have to inline the judge call or take the default |

The output side is the easy half: a `Tier` is a two-way choice over a named pair,
and `Admitted::mint` already exists as the one place a `Decision`'s coupled
fields are filled (`routing/mod.rs:218-224`). `DecisionSource` maps naturally onto
the `rationale` string a decision already carries.

### 3.4 What a scorer beside `routing/policy.rs` still needs

Scored against `cc1245a`, not `dbfd4fd` — **items 2, 4 and 5 are already done**
by the landed port and are listed here only so nobody re-does them:

- ~~windowed severity scale~~ — **done** (item 2 below), 0.3/0.7/1.0 table ported.
- ~~`tests_passed` detector~~ — **done** (item 4 below).
- ~~codex tool-name taxonomy~~ — **done** (item 5 below); the port additionally
  found and fixed a codex-specific defect our own tree had
  (`reads_as_failure` blind to exec exit codes).
- **`compacted` is refused, not pending** (item 3 below) — the hard-override half
  of `pick_tier` therefore has no input on our side by ruling, not by omission.

The live items, in dependency order:

1. **Tool-result history at routing time — the only live blocker.**
   `RoutingContext` would need either the request's normalized messages or a
   pre-computed signal struct. Note the *shape* of Switchyard's solution: they do
   not put signals in the routing context either — a request-side
   `ToolSignalProcessor` writes `state.tool_signals` and the classifier reads it
   off `State` (`stage.rs:176-178@053a61e`, `util/stage.rs:556-561@053a61e`).
   Roundhouse's `cc1245a` already computes the equivalent from `SessionState`;
   what is missing is a carrier from that computation to `RoutingContext`.
   Everything below this line is already built or already ruled.
2. **A windowed severity scale.** Our `ToolFailureStreak` is a boolean-ish streak
   over `Exchange`; theirs is `0.0 clean / 0.3 soft (exit_nonzero) / 0.7 hard /
   1.0 critical`, **windowed** so "an error persists through the recovery turns
   instead of clearing the instant the next result is clean"
   (`tool_signals.rs:207-211@053a61e`). The classification tables that produce
   0.3/0.7/1.0 are the bulk of the ~1,100 lines and are the actual porting cost.
3. **A `compacted` detector.** Fires the hard override and is self-latching
   ("the summary stays in the context prefix on every subsequent turn",
   `tool_signals.rs:240-245@053a61e`). Roundhouse has no compaction-marker
   detector; for a codex session this is a codex-specific prefix pattern and
   overlaps M9's prefix-admission work.
4. **A `tests_passed` detector.** "At least one of the last three tool results
   matched a test-pass pattern" (`:234-235@053a61e`) — pattern matching on tool
   output text.
5. **Tool-name taxonomy for codex specifically.** Their extractor already carries
   it: "`update_plan` is codex's equivalent of `todowrite`"
   (`tool_signals.rs:150@053a61e`) and "`shell_command` is codex's; `shell` /
   `local_shell_call`" (`:154@053a61e`), with tests
   `codex_update_plan_classifies_as_plan` (`:972`) and
   `codex_shell_command_runs_bash_pattern_match` (`:977`). A port that skipped
   this would score a codex session as pure `Other`/bash and never populate
   `production_intensity`, `spinning`, or `exploring` — i.e. it would silently
   fall open on every turn.

**Non-blockers, worth recording:** the three functions are `pub` and re-exported
(`lib.rs:38-42@053a61e`), pure, sync, allocation-light, and depend on nothing but
`&ToolSignals` — no OTel, no async, no `Driver`. Since every `ToolSignals` field
is `pub` and the struct derives `Default`, **a scorer can be re-written in ~40
lines from §1.2–1.3 above with no upstream dependency at all** — the arithmetic
is five constants and two functions. The port-not-crate call
(`agent-docs/synergies/ecosystem-round-2.md:77-81`) stands, and round 3 made it
countable: roundhouse has none of `opentelemetry`, `jsonschema`, `jsonptr`,
`regex`, or `parking_lot` as a normal dependency, libsy needs all five
(`crates/libsy/Cargo.toml:19-38@053a61e`), and libsy pins OTel 0.32 against the
0.31 already in our lock via `codex-http-client` — which
`crates/roundhouse-server/Cargo.toml:88-98` states must not reach the shipped
binary. Two OTel API majors in one test binary is the cost of the crate route
(`relay-switchyard-dedup-deep-dive.md@fe73e5f`). Apache-2.0 throughout;
attribution follows the judge-prompt precedent, pinned by a test on `cc1245a`.

---

## 4. Escalation / fallback / error semantics — three distinct mechanisms

They must not be collapsed. **There is no single "the router escalates on
error".** Four layers, from the transport up:

### 4.1 Transport retry (per target, inside one attempt loop)

`crates/libsy-llm-client/src/client.rs:236-289@053a61e`. Budget is
`max_retries + 1` attempts; `DEFAULT_MAX_RETRIES = 2`
(`crates/libsy-llm-client/src/backend.rs:18@053a61e`), configurable `0..=10` per
`[llm_clients.*]` (`docs/reference/toml_schema.md:49@053a61e`).

Retryable set (`client.rs:587-596@053a61e`): `LlmClientError::Transport`,
`LlmClientError::Timeout`, and `UpstreamHttp` whose status is retryable —
`is_retryable_http_status(s) = s == 408 || s == 429 || (500..=599)`
(`crates/libsy-llm-client/src/metrics.rs:41-43@053a61e`). Backoff: `Retry-After`
wins if present, capped at `MAX_RETRY_AFTER = 60 s` (`client.rs:56, 599-610@053a61e`);
otherwise `INITIAL_RETRY_DELAY = 250 ms` doubling to `MAX_RETRY_BACKOFF = 2 s`
(`client.rs:54-55, 612-620@053a61e`). Streaming body failures happen **after**
the retry boundary and are not retried (`client.rs:326@053a61e`).

**This is same-target retry. It never changes tier.**

### 4.2 Cross-target fallback — context overflow ONLY

`docs/operations/context_window.md@053a61e`: "When an upstream rejects a request
because the prompt exceeds the model's context window, Switchyard calls **the
remaining targets on the same route in configured order**, stopping when one
answers or every target has been tried. Fallback applies only to the current
request. An overflow is not remembered across turns" (`:3-8`).

**The trigger is narrow and stated as such** (`:12-18@053a61e`): HTTP **400**
*and* a body identifying a context-length error — `error.code ==
"context_length_exceeded"`, or a message containing `maximum context length` /
`prompt is too long`. "An overflow reported any other way — HTTP 413 or 422, for
example — is **not** recognized as one. The request fails on the spot with the
upstream's status code."

There is **no eviction key and no configuration** — "A route falls through
whenever it has more than one target" (`:22-23`). When every target overflows:
HTTP 400, `code = context_length_exceeded` (`:54-57`).

**So: a 500, a 429 past its retry budget, a refusal, or a timeout does NOT try
another candidate.** It fails the request. Only a recognized context overflow
crosses targets.

### 4.3 Stage router — per-turn, bidirectional, stateless

Already covered in §1.3. The properties that matter for Q4:

- **Both directions, every turn.** Unlike escalation mode, a stage-routed turn
  can go back down to efficient (`should_deescalate`, and a negative score).
- **Stateless.** "Nothing tracks the previous tier" — the test
  `every_turn_the_signals_drive_carries_the_note`
  (`crates/libsy/src/algorithms/util/stage.rs:871-884@053a61e`) asserts exactly
  this, and the `HandoffNoteConfig` doc says the notes are "Stateless: a note
  describes the turn's own signals, so every turn they drive carries one"
  (`:436-438`).
- **Judge failure falls open, never latches.** `a_judge_that_cannot_tell_lands_on_the_picker_default`
  (`crates/libsy/src/algorithms/stage.rs:557-566@053a61e`) drives a judge
  returning `p_solve = 42.0` and asserts the turn lands on `weak`. The
  `DefaultTarget` terminal classifier at `:199-202` is what makes "a turn is
  never left unrouted" true by construction.
- **The judge's verdict is never pinned to the session** —
  `the_judges_verdict_is_not_pinned_to_the_session` (`stage.rs:532-555@053a61e`)
  asserts two undecided turns produce two judge calls with independent answers.
- **A resolved turn never pays for the judge** —
  `a_decisive_signal_never_reaches_the_judge` (`stage.rs:517-530@053a61e`).

### 4.4 Escalation mode — sticky latch, weak-first, judge fails open

`docs/routing_algorithms/escalation_router_routing.md:55-91@053a61e`, five steps,
their numbering:

1. Call the weak target, **buffer** its reply.
2. Append that reply and ask the judge to rule on the *completed* turn — "The
   judge therefore rates work the weak model actually did, not a prediction about
   work it might do" (`:59-61`).
3. Escalate verdict → increment a consecutive streak; decline → reset to zero.
4. Streak `< confirmations` → serve the buffered weak reply. Cost: one weak call
   + one judge call, **no strong call**.
5. Streak `>= confirmations` → **discard** the buffered weak reply and serve
   strong. Cost: weak + judge + strong.

A latched session routes straight to strong with **no judge call** (`:72`).
Defaults are "the benchmarked configuration, so a bare `escalation = {}` is a
valid, tuned route" (`:101-102`): `confirmations = 2`, `recent_turn_window = 28`,
`window_message_chars = 500` (`:104-108`).

**Judge failure semantics, verbatim** (`:89-91@053a61e`): "A judge that times
out, errors, or returns an unparseable verdict **fails open**: the turn serves the
buffered weak reply and the existing streak is **held** rather than cleared. A
judge failure never creates a strong-tier latch."

⚠ **Session identity is load-bearing** (`:110-113@053a61e`): "`2` or higher
requires a session identity, because the streak is retained per session — without
one, every turn starts from zero and the route **never latches**. Clients supply
it with `x-switchyard-session-id`." Roundhouse owns the session id already, so
this is a mapping question, not a gap — but a naive proxy that dropped the header
would silently disable the entire mechanism.

### 4.5 Refusal — a normalized concept that **no router reads**

Q4 names refusal explicitly, so the negative deserves the search behind it.

A refusal is normalized. `StopReason::ContentFilter` is a protocol variant —
"Provider safety or content filtering stopped generation"
(`crates/protocol/src/llm.rs:412-425@053a61e`) — and the codecs round-trip it in
both directions: OpenAI `"content_filter"` ↔ `ContentFilter`
(`crates/switchyard-translation/src/codecs/openai_chat/buffered.rs:1284, 1295@053a61e`,
also `crates/protocol/src/stream.rs:465@053a61e`) and Anthropic `"refusal"` ↔
`ContentFilter` (`…/codecs/anthropic/buffered.rs:1116, 1127, 1141@053a61e`, the
#370 / `fbb4eea3` fix the CHANGELOG records at `:59-62`). There is a
`ContentBlock::Refusal` content variant too.

**Nothing routes on it.** `grep -rn StopReason crates/libsy/src
crates/switchyard-server/src@053a61e` returns only: `noop.rs:10, 39`
(constructs `EndTurn`), `advisor_gate/turn.rs:10, 103` and its tests (reads
`ToolUse`, to decide whether a turn is terminal). `grep -rn ContentFilter
crates/ --include=*.rs@053a61e` returns only the translation codecs, the protocol
enum, and one observability label
(`crates/libsy-llm-client/src/observability.rs:155@053a61e`, which turns it into
the metric string `"content_filter"`). **No classifier, no judge, no router, and
no retry path branches on `ContentFilter`.**

The only way a refusal influences routing is *as text*: `ContentBlock::Refusal`'s
string is read by the escalation judge's transcript builder
(`crates/libsy/src/algorithms/util/escalation.rs:188, 201@053a61e` — the comment
at `:188` says `Message::text_content` is deliberately not used "it keeps only
text and refusal") and by the tool-signal text extractor
(`crates/libsy/src/algorithms/util/tool_signals.rs:400@053a61e`). So a refusal
can move a judge verdict or a severity score, but only by being words a judge
reads — never as a typed event that reroutes, retries, or changes tier.

### 4.6 Advisor gate — the third escalation shape (new in `[Unreleased]`)

`CHANGELOG.md:11-18@053a61e`, `docs/routing_algorithms/advisor_gate_routing.md`,
`config.rs:542-577@053a61e`. One executor serves every client-visible turn; a
judge-only advisor reviews terminal turns. **APPROVE** releases the buffered turn;
**REDO** discards it and feeds the advisor's plan back to the executor. Carries
per-session review budgets scoped by `proxy_x_session_id`, stall checkpoints
(`gate_stall_turns`, `gate_min_tool_results`), a pattern trigger for
text-protocol harnesses (`gate_trigger = "pattern"` + `gate_trigger_pattern`),
middle-out transcript truncation (`transcript_max_chars`), and
`fail_open` (default true).

⚠ **`fail_open` collides with a roundhouse invariant.** The M9-era fact-check
already flagged this: the "fail-open/verdict:\"APPROVE\" mechanic … directly
collides with roundhouse's no-fail-open invariant"
(`agent-docs/research/relay-switchyard-dedup-deep-dive.md:1076`). The advisor
route makes that mechanic a first-class, default-on route type. If M10 adopts any
advisor-gate idea, that invariant needs an explicit ruling first.

### 4.7 Summary table — what happens on what

| Event | Same-target retry | Try another target | Change tier | Fail the turn |
|---|:--:|:--:|:--:|:--:|
| Transport error / timeout | ✓ (≤ `max_retries`) | ✗ | ✗ | after budget |
| HTTP 408 / 429 / 5xx | ✓ | ✗ | ✗ | after budget |
| HTTP 400 + context-length body | ✗ | **✓ remaining targets, configured order** | incidentally | only if all overflow |
| HTTP 413 / 422 | ✗ | ✗ | ✗ | ✓ immediately |
| Model *refusal* / `StopReason::ContentFilter` (§4.5) | ✗ | ✗ | ✗ | ✗ — served as the answer; no router reads the variant |
| Judge/classifier error or unparseable verdict | (its own client's retries) | ✗ | ✗ | ✗ — **fails open** to default tier / buffered weak reply |
| Tool-result error signals (severity, spinning) | — | — | **✓ stage router, per turn** | ✗ |
| Sustained trouble across turns | — | — | **✓ escalation mode, latches at `confirmations`** | ✗ |

**Note the empty column:** *nothing* in Switchyard routes a failed dispatch to a
different tier. Tier movement is driven by signals and judges, never by transport
failure. A "fallback to sol" in M10's sense — sol as the *recovery* target when
kimi errors — has **no upstream precedent** outside the context-overflow path.

---

## 5. Anything else answering "how would Switchyard route a codex session across
sol/terra/luna"

### 5.1 Their steering mechanism is already a text instruction, not a tool call

This is the single most useful finding for M10's steering pivot.

`HandoffNoteConfig` (`crates/libsy/src/algorithms/util/stage.rs:439-493@053a61e`)
carries `escalation_note`, an optional `deescalation_note`, and
`only_on_wrong_signal_escalation` (default **true**). The note is applied by
`prompts::append_note(request, note)` (`:534-541`), and the tests pin what that
means: `trailing_text` reads `request.llm_request.messages.last()` and the
assertion is `Some(format!("hi|{ESCALATION}"))`
(`:842-848, 858-869@053a61e`) — **the note is appended to the trailing user turn's
text content of the forwarded request.**

Three design properties they state, each of which M10 will otherwise rediscover:

1. **The note rides in the forwarded request only, never in the caller's
   conversation, so notes cannot accumulate across turns** (`:436-438@053a61e`).
2. **It is gated by default**, and the reason is spelled out: an ungated note
   "can tell the capable model the efficient one was stalling when it wasn't"
   (`:452-455`, and again at `:479-481`). Only `DecisionSource::Override` and
   `DecisionSource::Dimensions` — the *signal-driven* sources — qualify
   (`:482-487`). An ambiguous fall-open carries no note
   (`no_escalation_note_on_an_ambiguous_turn_when_gated`, `:787-793`).
3. **There is a de-escalation note too** — the hand-back to the efficient tier is
   also narrated when configured (`:489-491`, test at `:803-809`).

The production wording (`dev-server/config.toml:50@053a61e`) is worth copying as
a starting point, prefix and all: `"[router-guidance] A weaker model was handling
this task and showed signs of stalling, looping, or repeated errors on the
preceding steps, so control was escalated to you, a stronger model. Re-examine
the current state directly and do not simply repeat the previous approach."`

Note what that note does **not** contain: no guidance text about the *skipped
request*, because Switchyard never skips a request — it substitutes the target.
M10's "guidance + the skipped request restated" is a strictly larger payload than
anything upstream sends, and the `only_on_wrong_signal_escalation` rationale
(don't narrate a switch that didn't happen for the reason you're claiming) is the
failure mode to design against.

A **second**, separate injection surface exists: per-tier system prompts
(`capable_system_prompt` / `efficient_system_prompt`,
`config.rs:530-534@053a61e`), applied by a post-decision `SystemPromptProcessor`
"so it applies to the target the cascade settled on, whichever classifier picked
it" (`crates/libsy/src/algorithms/stage.rs:204-206@053a61e`). Notes are per-switch
and stateless; system prompts are per-tier and every turn.

### 5.2 `POST /v1/decision` — routing decision without serving

`crates/switchyard-server/src/lib.rs:587, 649-714@053a61e` (landed `21076644`,
2026-08-19, #456). Request: `{input_format: WireFormat, request: <provider
request JSON>}` (`:650-655`, `deny_unknown_fields`). Response
(`:121-142@053a61e`):

```rust
struct DecisionResponse { selected: DecisionTargetResponse, fallbacks: Vec<DecisionTargetResponse>, response: Option<Value> }
struct DecisionTargetResponse { target: &str, model: &ModelId, llm_client: { format: WireFormat, base_url: &str }, extra_body: &BTreeMap<String, Value> }
```

Doc comment at `:657`: "Selects a target while still allowing the algorithm's
classifier and judge calls" — i.e. the judge/classifier side calls *do* happen and
are paid for; only the answer-model call is skipped
(`run_decision_only`, `:717-718@053a61e` — "Completes routing-time calls and
returns the outcome without serving its answer target").

**Why this matters to M10.** The product sentence says roundhouse "owns the turn
(the durable log, policy, budgets, routing, steering)". `/v1/decision` is the one
Switchyard surface that hands over a routing *verdict* — selected model, its
base_url, wire format, `extra_body`, **plus an ordered fallback list** — without
taking the turn. It is the shape that lets roundhouse consult Switchyard guidance
while keeping dispatch, and it is strictly more informative than a header on a
proxied response. It also leaks no credential (`DecisionLlmClientResponse` is
documented as "Non-secret client settings needed to call a selected model",
`:137-138`). Note the caveat at `:698-700`: "The request moved into the decision
run, so its namespace mapping is gone by here. A Codex tool call in this preview
keeps its qualified name."

### 5.3 The kimi-k3 / sol pairing already exists upstream

`examples/experimental/litellm/@053a61e` — marked "**Experimental integration:**
This example and its Python APIs are experimental and may change without notice"
(`README.md:3-4`) — wires exactly the M10 pair:

```yaml
# examples/experimental/litellm/litellm-config.yaml:1-12@053a61e
model_list:
  - model_name: strong
    litellm_params: { model: openrouter/openai/gpt-5.6-sol,        api_key: os.environ/OPENROUTER_API_KEY }
  - model_name: fast
    litellm_params: { model: openrouter/moonshotai/kimi-k3,        api_key: os.environ/OPENROUTER_API_KEY }
litellm_settings: { drop_params: true }
```

Route (`examples/experimental/litellm/benchmark-route.toml:18-25@053a61e`):
`type = "stage_router"`, `capable_target = "strong"`, `efficient_target = "fast"`,
`picker = "efficient_first"`, `confidence_threshold = 0.5`,
`recent_turn_window = 3`; the client reaches litellm over `openai_chat` at
`http://litellm:4000/v1` with a dummy `authorization = "Bearer not-needed"`
(`:3-8`).

Their own description of the split (`README.md:11-17@053a61e`): "LiteLLM provides
the OpenAI-compatible gateway, model aliases, and OpenRouter provider
integration. Switchyard's Stage router makes the routing decision from the coding
agent's recent tool history. … they let an application keep routing policy in
Switchyard while LiteLLM owns model access." That is precisely the
policy/access split M10 is proposing, with roundhouse in LiteLLM's chair.

Pins: `litellm==1.92.0`, `ghcr.io/berriai/litellm:v1.92.0`, Python 3.12 (the
pinned LiteLLM release cannot build on 3.14) (`README.md:38-49@053a61e`).

**`terra` appears nowhere in the tree.** `git grep -in terra` returns **0 hits**
across the whole repo @053a61e. `git grep -in "\bsol\b"` (lock files excluded)
returns six, all `gpt-5.6-sol`: `dev-server/config.toml:13`,
`examples/experimental/litellm/{README.md:40,331,354, litellm-config.yaml:4,
tests/test_gateway_config.py:37}`. `luna` appears once,
`dev-server/config.toml:18`. A three-tier sol/terra/luna arrangement has **no upstream
precedent** — and cannot be expressed as one `stage_router` route at all, since
`Tier` is a two-variant enum (§1.1). Their answer to "more than two targets" is
`llm_classifier` with `mode = "custom"`, which takes `targets: Vec<String>`
(`config.rs:400-411@053a61e`) and a JSON-Pointer `target_selector` policy —
the tau2 profiles are the worked example, and even those use only two targets.

### 5.4 They translate for codex, and they know its tool vocabulary

- **Codex tool names are first-class in the signal extractor** —
  `update_plan` → plan, `shell_command` → bash-pattern, `shell` /
  `local_shell_call` (`tool_signals.rs:150-154@053a61e`, tests `:972-1009`).
- **Codex MCP namespaces survive translation** — `crates/switchyard-translation/src/codex_namespaces.rs@053a61e`,
  added by `c7beccd4` (2026-08-20, #384) "fix: preserve Codex MCP namespaces
  through translation".
- **Codex delegated work is recognized** — `crates/libsy/src/algorithms/util/subagent.rs:132, 216@053a61e`:
  "Delegated *work* only. A harness maintenance turn (e.g. Codex `compact`)
  carries…" and "Codex delegated-work kinds." Generalized to all algorithms by
  `a8c4d2ed` (2026-08-21, #505).
- **`GET /v1/models` advertises reasoning support specifically for Codex** — the
  `reasoning` route key is documented as "Whether `GET /v1/models` advertises
  reasoning support **to Codex direct-provider discovery**. Unset routes are
  advertised as non-reasoning" (`docs/reference/toml_schema.md:96@053a61e`).
  Directly relevant to M9's launch-surface work: this is a *models-list* field
  a codex client reads.
- **`forward_auth`** (`docs/reference/toml_schema.md:54-76@053a61e`) remains the
  design reference M7 was told to cite: mutually exclusive with `api_key_env`;
  OpenAI clients forward `authorization`, `chatgpt-account-id`,
  `x-openai-fedramp`; Anthropic clients forward `authorization` or `x-api-key`
  plus `oauth-*` values from `anthropic-beta` while removing all other inbound
  beta values; "Forwarding clients do not follow HTTP redirects"; and the server
  rejects an Anthropic forwarding route called through an OpenAI endpoint (and
  vice versa) *before* calling an upstream. Base-URL validation was added
  `6730be82` (2026-08-21, #405).

### 5.5 Their own "when not to use" lists — read these before adopting

- Stage router (`stage_router_routing.md:282-290@053a61e`): not for single-model
  deployments, not for probabilistic A/B splits (use `random`), and **"No
  tool-result history. Stage-router needs meaningful tool-call traffic … For pure
  chat-completion workloads every ambiguous request lands on the picker's default
  tier."**
- Escalation (`escalation_router_routing.md:162-173@053a61e`): not for one-shot
  requests, not for traffic without session identity, not for fixed-ratio
  experiments, not for per-turn bidirectional movement (use stage router), and
  **not for latency-critical traffic — "An unlatched turn waits for the weak call
  and then the judge call."** For a codex agent loop, that is two serial upstream
  round-trips on every unlatched turn, which is the co-optimization axis
  roundhouse's own product sentence refuses to drop.

### 5.6 Observability they expose (what an M10 benchmark could read)

- Response header `x-model-router-selected-model`
  (`stage_router_routing.md:263-267@053a61e`). The older
  `x-model-router-rationale` header was **deleted** (`395c2026`, 2026-08-18,
  #473) — do not cite it.
- `/v1/stats` (alias `/v1/routing/stats`) and Prometheus `/metrics`.
  Stage-router-specific metric families
  (`crates/switchyard-server/src/stats/algorithms/stage_router.rs:11-17@053a61e`):
  `switchyard_stage_router_routing_decisions_total` (labelled
  `decision_source` × `target_name`), plus histograms `…_score`, `…_confidence`,
  `…_severity`, `…_spinning`, `…_exploring`, `…_production_intensity`. JSON
  projection is `{routing_decisions: {source → {total, targets{…}}}, scoring:
  {score, confidence, dimensions{severity, spinning, exploring,
  production_intensity}}}` with `MetricSummary {count, mean}` (`:41-77`).
  Histogram boundaries are fixed in libsy so every host exports the same
  distributions (`util/stage.rs:66-69@053a61e`): score buckets
  `[-1, -0.75, -0.5, -0.25, 0, 0.25, 0.5, 0.75, 1]`, unit buckets
  `[0, 0.1, 0.25, 0.5, 0.75, 0.9, 1]`.
- Escalation judge calls "are recorded in the classifier stats bucket, so their
  token cost and latency remain visible as routing overhead"
  (`escalation_router_routing.md:152-155@053a61e`) — the honesty discipline
  roundhouse's savings dashboard already requires.

---

## 6. Open risks and things M10 must decide, not inherit

1. **"Recipe" needs replacing in the M10 brief.** Say "`[routes.*]` stanza of
   type X" or name a specific checked-in profile file. `SwitchyardRecipes` is a
   removed 0.1.0 Python API (`CHANGELOG.md:241-243@053a61e`, removed at `:39-42`).
2. **One `--dry-run` to close a documentation contradiction.**
   `tb21-escalation-opus-glm-deepseek.toml:15-18@053a61e` says it "is not
   accepted by the current `switchyard-server`"; `toml_schema.md:175@053a61e` and
   the loader (`config.rs:803, 865-874@053a61e`) say the opposite, and the header
   predates the `mode` key. My reading says **it loads** (§2.3a). Settle it with
   `switchyard-server --config … --dry-run` against a HEAD build before either
   claim is carried into a plan. Not a blocker either way — three other
   calibrated profiles are unambiguous.
3. **The sol-default direction is the unbenchmarked arm.** "Mimic a user on a
   sol-only session; roundhouse reroutes a fraction of calls" = `capable_first`,
   which upstream marks experimental with "no calibrated thresholds … and no
   measured accuracy or cost figures" and logs a startup warning
   (`stage_router_routing.md:77-84@053a61e`). Every published Switchyard number
   is `efficient_first`.
4. **Three tiers are not expressible in their two-tier router.** `Tier` has two
   variants (`util/stage.rs:73-78@053a61e`). sol/terra/luna needs `llm_classifier`
   `mode = "custom"` (`targets: Vec<String>` + JSON-Pointer selector), for which
   the only calibrated examples are still two-target.
5. **"Fallback to sol" has no upstream analogue.** Nothing in Switchyard reroutes
   a *failed* dispatch to another tier; only a recognized HTTP-400 context
   overflow crosses targets (`docs/operations/context_window.md:12-18@053a61e`).
   Roundhouse would be inventing this, and should say so.
6. **The advisor route's `fail_open` default collides with roundhouse's
   no-fail-open invariant** — flagged in round 2 and re-derived in detail by
   round 3, which found the collision is sharper than "both fail open": ours
   releases the turn *without recording a verdict*, theirs writes
   `verdict: "APPROVE"` into the audit line on a failed consult
   (`relay-switchyard-dedup-deep-dive.md:487, 1020@fe73e5f`, citing
   `advisor_gate.rs:132, 147, 494-506`). Now a default-on route type
   (`config.rs:575-576@053a61e`). Needs a ruling before adoption, not during.
7. **The `ToolSignals` port is on `claude/toolsignals-port` (`cc1245a`), not on
   `claude/m9-codex-e2e`.** Whichever branch M10 cuts from decides whether the
   12 ported fields exist. If M10 branches from `main` (`1daf8d5`), it has
   *neither* the port nor the M9 work. **Merge order is an M10 prerequisite, not
   an implementation detail** — four sibling branches
   (`toolsignals-port`, `synergy-round-3`, `s2-relay-emission`,
   `agentic-api-mcp-compat`, plus `readme-refresh`) are unmerged as of `dbfd4fd`.
8. **`RoutingContext` cannot feed the scorers, and the ported signals are on the
   wrong seam to help.** Seven of the eight scorer inputs are tool-derived; none
   is reachable from `crates/roundhouse-core/src/routing/mod.rs:164-193`. The
   port put them behind `Evidence::of(&SessionState)` on the *validate* seam
   (`validate/trigger.rs:127-144`), whose `Signal::detect` returns
   `Option<String>` and whose `SignalFired::fact` rule forbids a suggestion
   (`trigger.rs:79-88`). **Carrying tool facts from validate to routing as
   structured data is the open design question the ruling deferred**, and this
   dive does not settle it. It is the one thing standing between roundhouse and
   a stage-router equivalent.
9. **~~Codex tool-name taxonomy~~ — closed by the port.** `cc1245a` ported the
   taxonomy and, in doing so, found a real defect in our own tree
   (`reads_as_failure` blind to codex exec exit codes), ruled test-first. Retained
   here only as the reason a *future* re-port must not skip it: without
   `update_plan` / `shell_command` / `local_shell_call` classification
   (`tool_signals.rs:150-154@053a61e`) a codex session scores as all-`Other`.
10. **Pin vigilance, two items.** (a) Switchyard's benchmark pins
    `CODEX_VERSION=0.144.5` (`benchmark/agent-versions.env:5@053a61e`), older than
    both this box's binary (0.146.0 / `e363b08`) and our Cargo pin (`6344a65`) —
    a third point on the codex version fan. (b) `session_affinity` was replaced by
    `classify_trigger` (`c7b648d0`, #487) and `Algorithm::route` now returns
    `RoutingOutcome` (`0cf6439f`, #459); both post-date earlier roundhouse reads.
