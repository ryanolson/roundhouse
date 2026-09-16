<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Relay 0.8.2's proxy posture: what it keeps, where, for which promise — and what it deliberately does not keep

> **Status: evidence base for D1** (the state-spectrum design round, frontier plan
> R10). A read-only read of the **published 0.8.2 registry sources** against
> roundhouse at **7c5369a**. It answers one question and does not rule on it:
> *what does a proxy that made the opposite state choice from us actually keep?*
> The ruling — P0 proxy / P1 ephemeral / P2 durable, and how much of Relay's
> posture to adopt — is the orchestrator's.
>
> Dated **2026-09-02**.

**Pins.** Relay: the published crates under
`/root/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/` —
`nemo-relay-0.8.2`, `nemo-relay-cli-0.8.2`, `nemo-relay-types-0.8.2`,
`nemo-relay-adaptive-0.8.2`, `nemo-relay-plugin-0.8.2`,
`nemo-relay-worker-proto-0.8.2`, `nemo-relay-pii-redaction-0.8.2`. Cited as
"0.8.2 registry" with a crate-relative path. Roundhouse: `7c5369a`. Claude Code:
the byte-exact fixtures under
`crates/roundhouse-server/tests/fixtures/claude-2.1.257-*.json`.

**Relation to the prior reads.** `nemo-relay-deep-dive.md` (§C.3, §C.5) read the
adaptive loop and the policy model at `c37b551`;
`nemo-relay-0.8.0-published-read.md` read interception, the launcher, the wire
types, and (Finding 4.2) *config* persistence, with a 2026-09-01 addendum on
0.8.2's chained-topology hazards. **None of them read the state model.** This
document is that read; where it touches ground the earlier reads covered, it says
so and does not repeat them.

---

## 0) The read in one page

Relay 0.8.2's gateway is a **single-process, loopback-only, idle-suicidal proxy
whose per-conversation state is a `HashMap` behind a `Mutex` that no restart and
no second process can see** — and that is not an accident of maturity, it is the
posture. The gateway refuses a non-loopback bind outright
(`nemo-relay-cli-0.8.2/src/server/mod.rs:92-97`), its session table is
`Arc<Mutex<HashMap<String, Session>>>` (`src/sessions/mod.rs:64-71`), its
sessions are swept 30 seconds after their last activity
(`src/sessions/idle.rs:18-19`), and the process itself shuts down after an
operator-set idle window (`src/server/mod.rs:843-895`).

What survives a request boundary at all is: (a) a **scope tree** for
observability correlation, (b) a small **correlation cache** with a 300-second
TTL, (c) an **ATIF trajectory JSON file** rewritten whole at each turn boundary,
(d) **learner state** in the optional adaptive plugin's backend (memory by
default, Redis opt-in), and (e) five small **bootstrap files** that arbitrate
which process owns the port. Nothing else. There is no conversation log, no
prefix admission, no generation, no idempotency key, no resumption cursor, no
writer lease, no spend ledger, no quota, and no read-back API of any kind.

The single sharpest datum for D1 is not any of those absences but a *refusal*:
Relay's response cache **bypasses every request that carries server-side
conversation state** — `store`, `previous_response_id`, `conversation`,
`container`, and any Responses-shaped body that does not explicitly say
`store: false`
(`nemo-relay-adaptive-0.8.2/src/response_cache/key.rs:128-147`). Relay does not
merely decline to keep the conversation; it treats a protocol that implies
server-side conversation state as *out of its jurisdiction*. A roundhouse
Responses-surface request would be bypassed by that rule on every turn.

---

## 1) The state inventory

Everything Relay 0.8.2 retains past the end of one HTTP request, what it is for,
and what it is written in.

| What | Backing | Lifetime | Ends when | Promise it serves |
|---|---|---|---|---|
| `SessionManager.inner: HashMap<String, Session>` | process memory, `tokio::Mutex` (`cli/src/sessions/mod.rs:64-71`) | process | idle sweep at 30 s, shutdown, or `GatewaySessionFinish::Close` | scope lineage: which agent/turn/subagent an LLM call belongs under |
| `Session.turn_index`, `agent_scope`, `turn_scope`, `subagents`, `subagent_stacks` | same (`cli/src/sessions/mod.rs:154-185`) | one agent run | turn/agent close | ATOF scope tree, parent UUIDs |
| `Session.last_turn_llm_output: Option<Value>` | same (`:164`) | **one turn — the previous response only** | next turn | tool-hint extraction from the last response |
| `Session.pending_llm_hints` / `pending_tool_hints` | same (`:176-177`) | `LLM_HINT_TTL` / `TOOL_HINT_TTL` = 300 s (`:47-48`), pruned in `cleanup_correlation_state` (`:1846-1859`) | TTL or turn boundary (`clear_correlation_state`, `:1475-1479`) | matching a hook-delivered event to the gateway call it describes |
| `Session.llm_request_affinity: HashMap<String, Option<String>>` | same (`:180`) | turn | `clear_correlation_state` (`:1478`); entries dropped when their subagent ends (`:1648-1649`) | which parallel subagent owns an unhinted provider call |
| `Session.last_llm_owner` | same (`:181`) | `LAST_OWNER_TTL` = 300 s (`:49`) | TTL / turn | sticky ownership fallback |
| `SessionManager.authenticated_owners: HashMap<String,String>` | same (`:66`) | session | `release_closed_owner_ids` (`cli/src/sessions/idle.rs:68-81`) | a hook client may not reparent a session it does not own |
| `SessionAlignmentState` (child-session aliases, pending subagent starts) | same (`:67-69`, `cli/src/agents/shared/alignment.rs:287`) | until the child's terminal event | `clear_for_ended_subagent` | Codex child threads become subagents of their parent |
| `AtifExporter.events: Vec<Event>` | process memory, **unbounded, no eviction** (`nemo-relay-0.8.2/src/observability/atif.rs:334-337`) | the trajectory | `AtifExporter::clear` (`:430-434`), which the dispatcher calls at scope end | the trajectory document |
| **ATIF trajectory JSON** | one file per top-level scope, whole-file atomic rewrite (`core/src/observability/plugin_component.rs:3238-3240` → `atomic_private_write`, `core/src/observability/private_file.rs:106-137`) | disk, forever | never — nothing prunes it | the only durable record of a run |
| ATIF remote copies | HTTP or S3 object store, `AtifStorageConfig` (`core/src/observability/plugin_component.rs:526-548`) | remote, forever | never | same, off-box |
| Operational logs | size-rotating files, `retained_files` bounded (`core/src/logging/rotation.rs:10-33`) | bounded | rotation | diagnostics |
| OTLP spans/logs/metrics | pushed to a collector every 60 s (`core/src/observability/otel_metrics.rs:44`) | none locally | — | observability export |
| Adaptive `RunRecord`s, plans, tries, accumulators, ACG observations + stability | `InMemoryBackend` (default) or `RedisBackend` (opt-in) (`adaptive/src/storage/memory.rs:22-29`, `adaptive/src/redis.rs:1-19`) | process, or Redis **with no TTL and no trim** | nothing | the adaptive learner |
| Response-cache entries | `InMemoryCacheStore` (byte-budgeted, insertion-order eviction) or `RedisCacheStore` (`SET … PX`) (`adaptive/src/response_cache/store.rs:141,307-318`) | `ttl_seconds` | TTL | exact-match replay |
| Bootstrap owner record, lock, recovery record, ready file | five files under `$XDG_CONFIG/…/bootstrap/` (`cli/src/bootstrap/state.rs:103-113`) | until the owner exits (`OwnerGuard::drop`, `:74-79`) | process exit | one gateway per user per endpoint |
| Config | `config.toml` (user, then system) + `plugins.toml` + env, deep-merged at boot (`cli/src/configuration/mod.rs:1145-1155`, `:1188-1207`) | boot-time snapshot | restart | everything else |

Two things are conspicuous by their absence from that table: **no embedded
database of any kind**, and **no per-principal accounting**.

---

## 2) What one request costs when Relay remembers nothing

The gateway handler is 12 lines (`cli/src/gateway/mod.rs:71-83`): touch the
idle clock, authorize, buffer the body once, resolve a session, run the managed
pipeline. Per request, with no memory at all, Relay:

1. **Rejects browser-origin requests**, then verifies-and-strips the loopback
   proxy credential; the client's own provider credential is left untouched
   (`cli/src/server/mod.rs:540-582`, `cli/src/provider_auth.rs:50-80`).
2. **Injects an upstream credential only if the client sent none** —
   `configured_auth_header`, else `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` from
   process env (`cli/src/gateway/mod.rs:1069-1107`). There is no key store, no
   per-project scope, no rotation: pass-through first, one process-wide env
   fallback.
3. **Extracts identifiers from headers and body** — session id, subagent id,
   `conversation_id`, `generation_id`, `request_id`
   (`cli/src/gateway/request.rs:165-208`).
4. **Forwards the body verbatim** unless a plugin request-intercept mutated
   `LlmRequest.content`, in which case it is re-serialized wholesale
   (`cli/src/gateway/mod.rs:927-943` — the alphabetizing hazard the 2026-09-01
   addendum §A.2 already recorded).
5. **Streams the response back**, decoding each SSE event to JSON for the runtime
   and re-encoding it for the client (`cli/src/gateway/mod.rs:64-70` states the
   trade-off explicitly).
6. **Emits events.** Everything Relay "knows" about the turn leaves as events.

Steps 1, 2, 3, 4 and 6 are *exactly* the P0 mode's job description — auth,
routing, metering — and Relay does all of them with no state whatever.

### `generation_id` is not a generation

`LlmGatewayStart` carries a `generation_id`
(`cli/src/sessions/types.rs:20`), read from `x-nemo-relay-generation-id` or
`generation_id` / `generationId` / `generation.id` in the body
(`cli/src/gateway/request.rs:185-190`). It is **not** roundhouse's fork counter.
Its only consumer is `hint_match_score`, where it contributes 4 points to
matching a hook-delivered hint against a gateway call
(`cli/src/sessions/correlation.rs:21-27`). It is an observability-vendor
"generation" (one model call), passed through, never minted, never incremented,
never compared to a log. Searching `nemo-relay-cli-0.8.2/src` and
`nemo-relay-0.8.2/src` for `fork(ed)?_session|generation_counter|#g[0-9]` returns
**zero** hits.

---

## 3) Correlation without a log: how Relay decides "which conversation is this"

This is the load-bearing comparison for D1, and it fits in twenty lines of
Relay's source (`cli/src/sessions/correlation.rs:329-348`):

```
1. an explicit session id from the client        → Retain that session
2. else, if exactly one session is active/recent → Retain that one
3. else, if there are no sessions at all         → the stable synthetic root "gateway-gateway"
4. else (several active, no join key)            → a fresh "gateway-isolated-{uuidv7}", Closed at end of call
```

The explicit id comes, in precedence order, from `x-nemo-relay-session-id`, then
`x-claude-code-session-id`, then Codex's `client_metadata.session_id` or
`prompt_cache_key` in the body (`cli/src/agents/shared/alignment.rs:399-410`,
`:424-427`; `cli/src/agents/codex/alignment.rs:48-62`). Step 2's "active or
recent" is the 30-second idle window (`cli/src/sessions/mod.rs:976-981`).

**Three properties of this design worth naming:**

- **Identity is asserted by the client or inferred from ambient uniqueness; it is
  never verified against anything.** No history is compared, no hash is checked.
- **Ambiguity produces isolation, not a guess.** Step 4 is the honest branch: two
  agents running at once with no join key each get a throwaway root rather than
  being cross-correlated. The comment says so verbatim.
- **Step 3 is a *stable* synthetic root.** A pure proxy with no agents attached
  puts every call in one bucket named `gateway-gateway`
  (`cli/src/events/mod.rs:18-24` for the string) — deliberately, "the stable
  synthetic root preserves pure-proxy continuity".

The header Relay reads first for Claude Code is real: the byte-exact 2.1.257
capture carries `x-claude-code-session-id` on every `/v1/messages?beta=true`
request, stable across turns within a session
(`crates/roundhouse-server/tests/fixtures/claude-2.1.257-headers.json`, and the
MCP capture likewise). Roundhouse already reads the same header
(`crates/roundhouse-server/src/messages_api/wire.rs:67`). The two projects agree
on the *key*; they disagree entirely on what to do with it.

---

## 4) Restart mid-session, and a second proxy instance

### A second instance is prevented, not coordinated

Relay does not run two gateways. `GatewaySpec::acquire`
(`cli/src/bootstrap/mod.rs:86-98`, `:168-266`) takes a per-user **file lock** on
the endpoint (`cli/src/bootstrap/state.rs:147-179`), probes the listener, and
then:

- **Compatible** (same version, same bootstrap protocol, matching fingerprint) →
  *reuse the existing process* and log `gateway_reused`
  (`cli/src/bootstrap/mod.rs:267-285`).
- **Incompatible** (Relay, wrong version/config) → error telling the operator to
  stop it or wait for idle shutdown (`cli/src/bootstrap/mod.rs:293-297`); a version-mismatched
  Relay-*owned* gateway is instead shut down and replaced
  (`cli/src/bootstrap/state.rs:237-249`).
- **Foreign** (something else on the port) → refuse (`cli/src/bootstrap/mod.rs:287-291`).
- **Unavailable** → spawn a detached child, wait for its ready file, hand it to a
  reaper (`cli/src/bootstrap/mod.rs:299-393`, ready file read at `:469-505`).

The default endpoint is a fixed `127.0.0.1:47632`
(`cli/src/bootstrap/mod.rs:38-39`), and a non-loopback bind is a hard error
(`cli/src/server/mod.rs:92-97`). **There is no multi-node story to have.**

### A restart mid-session loses the session and keeps the trajectory

Concretely, when the gateway process dies and comes back:

- **Lost:** every entry of `SessionManager` — scope tree, turn index, subagent
  aliases, correlation hints, affinity map, authenticated owners. The
  `AtifExporter` event buffers go with them.
- **Kept:** the ATIF trajectory file(s) already written. Because ATIF is
  snapshotted at *turn* boundaries and `export()` is non-destructive, "multi-turn
  sessions write progressively complete trajectories … each snapshot is a
  cumulative superset of prior writes" (`cli/src/events/mod.rs:31-37`,
  `core/src/observability/atif.rs:396-402`). A crash loses at most the turn in
  flight.
- **Kept:** adaptive learner state, *if* a Redis backend was configured; the
  default `in_memory` backend loses it (`adaptive/src/config.rs:79-92`).
- **Not resumed:** nothing. A post-restart request re-enters
  `gateway_session_for_call` cold and lands on branch 1 (client-supplied id — a
  *new*, empty `Session` under the same name) or branch 3.

The one restart mechanism Relay does have is about the *port*, not the
conversation: `GatewaySpec::recover` (`cli/src/bootstrap/mod.rs:101-127`,
`:208-266`), called only from the MCP lifecycle lease
(`cli/src/mcp/gateway.rs:263-291`). It writes a `RecoveryRecord`
(`from_instance`, `endpoint_url`, `to_instance`) under the startup lock *before*
spawning the replacement, so two overlapping MCP clients cannot each start one,
then rewrites it with the new instance id. It is a leader-election crumb, not a
session cursor.

**Bearing on the M12.1 handoff (a).** Roundhouse's failure mode after a restart —
`bind` re-derives generation zero, a disagreeing history forks to a re-derived
`#g1`, and the fork arm appends the claimed history whole onto a pre-restart
`#g1` log, duplicating a prefix — has **no Relay analogue at all**, because Relay
has neither the log, the generation, nor the append. Relay's equivalent cost is
paid in the opposite currency: the trajectory *is* correct across the restart
(cumulative snapshots), and the *correlation* is simply gone. That is the trade
D1 is choosing between, stated at its extremes.

---

## 5) Three negatives, precisely stated

### 5.1 No durable conversation log — and no embedded database anywhere

Searching all seven 0.8.2 crates (`src/`, `tests/`, `Cargo.toml`) for
`rusqlite|sqlx|sled|rocksdb|duckdb|sqlite|libsql` returns **one** hit, and it is
a negative test asserting that `"sqlite"` is rejected as an unknown backend kind
(`adaptive/tests/integration/response_cache_tests.rs:617`). The complete set of
Relay's own persistent artifacts is the ATIF trajectory files, the rotating
operational logs, the five bootstrap files, the config files, and — only when
configured — the adaptive Redis keys.

The ATIF file is the closest thing to a durable record of a conversation, and it
is worth being exact about what it is *not*: a whole JSON document, rewritten in
full (temp file, `sync_all`, rename —
`core/src/observability/private_file.rs:113-126`) at each turn boundary, keyed by
the top-level scope UUID via a `{session_id}` placeholder the config validator
requires (`core/src/observability/plugin_component.rs:459-462`). It has no
sequence numbers, no per-item identity, no writer identity, no cursor, and
nothing reads it back into the runtime. It is an *artifact*, not a store.

### 5.2 No prefix admission

Nothing in Relay compares a client's claimed history against a stored one.
Searching all three main crates' `src/` for
`prefix_match|history_match|admit_prefix|claimed_history|common_prefix|diverge`
returns four hits: one unrelated comment, and three in ACG's cache-miss
diagnostics.

Those three are the closest analogue and are instructive precisely because of
how far short they fall. ACG retains a rolling window of `PromptIR`
observations per learning key and, when a provider prompt-cache hit fails to
materialise, reports `CacheMissEvidence::PrefixMismatch { first_mismatch_span_id,
sequence_index, expected_hash_prefix, actual_hash_prefix }`
(`adaptive/src/acg/telemetry.rs:81-91`, `:313-336`). So Relay *does* hash prompt
spans and *does* notice when a stable prefix diverges — but:

- it is **per span of one prompt**, not per conversation item;
- it is **diagnostic**: the output is a `summary` and a `recommendation`
  ("Move or extract the mismatching block after the stable prefix",
  `:326-328`), never a session decision;
- it never forks, never rejects, never renames a conversation;
- the comparison is against a *statistical exemplar* of many observed prompts,
  not against a committed log of one conversation.

### 5.3 No control or read-back surface at all

The gateway's complete route table is 14 routes
(`cli/src/server/mod.rs:634-648`): `/healthz`, two `/bootstrap/*`, two
`/hooks/*`, and nine provider-proxy paths. **Not one of them reads back state.**
There is no `/sessions`, no `/usage`, no `/trajectories`, no admin plane.
`/healthz` returns version and instance id and nothing else
(`:758-800`).

And Relay's MCP server exposes **zero tools**: `tools/list` returns
`{"tools": []}` (`cli/src/mcp/protocol.rs:72-76`), and the session loop does
nothing but hold a liveness lease and forward protocol frames
(`cli/src/mcp/session.rs:13-32`). Relay registers an MCP server with Claude Code
purely so the agent's lifecycle keeps the gateway alive — the exact opposite of
roundhouse's M12 use of MCP as a control surface.

Correspondingly: no idempotency key is read or written anywhere (the three
`idempot` hits are about plugin registration and hook-file merging); no
resumption cursor exists; and the only "lease" in the tree is the gateway
liveness lease and a plugin-mutation lock (`core/src/plugin.rs:1573`,
`cli/src/mcp/gateway.rs:4`), never a session writer lease.

---

## 6) The adaptive loop: what feedback is stored, and where it stops being cheap

§C.3 of the deep dive described the loop's *shape* at `c37b551`; this is its
*storage*, at 0.8.2.

**The backend is chosen by a string, and the default is memory.**
`StateConfig { backend: BackendSpec { kind, config } }`, `BackendSpec::default()
= in_memory` (`adaptive/src/config.rs:62-92`). The CLI compiles the adaptive
crate with `features = ["redis-backend"]` (`nemo-relay-cli-0.8.2/Cargo.toml:124-125`),
so Redis is *available* in every shipped CLI but *off* unless configured — the
same "no Redis by default" shape as roundhouse's P1.

**What goes in.** `RunRecord { id, agent_id, calls: Vec<CallRecord>, started_at,
ended_at }` (`adaptive/src/types/records.rs:65-79`), where each `CallRecord`
optionally carries `annotated_request` and `annotated_response`
(`:55-61`) — the **full prompt and completion**, retained for ACG analysis.

**Where it goes, and the retention story.** The Redis layout is
`{prefix}runs:{agent}:{run}` (a JSON blob), `{prefix}runs_index:{agent}` (a LIST,
`RPUSH` per run), plus `plan:`, `trie:`, `accumulators:`, `acg_observations:` and
stability keys (`adaptive/src/redis.rs:9-19`, `:127-146`, `:354-373`).
Grepping `adaptive/src/redis.rs` for `EXPIRE|expire|LTRIM|ltrim` returns **zero**
hits: **the adaptive Redis backend sets no TTL and trims no list.** The run index
grows without bound for the life of the key space.

The one bound that does exist is in the learner, not the store: ACG keeps a
`VecDeque` of at most `observation_window` `PromptIR`s per learning key,
defaulting to **100** (`adaptive/src/acg_learner.rs:91-107`,
`adaptive/src/config.rs:273-275`), and `PromptBlock.content` is a raw `String`
(`adaptive/src/acg/prompt_ir.rs:126-146`). So with ACG plus Redis, Relay durably
stores up to 100 raw prompt snapshots per profile with no expiry and no pruning
pass. `acg/retention.rs` is *not* a mitigation: its `RetentionTier` recommends a
provider prompt-cache TTL from observed session durations
(`adaptive/src/acg/retention.rs:47-70`); nothing in it governs stored state.

**Three postures from this subsystem that are directly adoptable, and are the
most useful thing in this document for D1:**

- **The shared store is fail-open with a deadline.** `REDIS_OP_TIMEOUT = 2 s`
  (`adaptive/src/response_cache/store.rs:355-357`, "A hung Redis peer must
  degrade to fail-open, not block the request"), and a store error becomes a
  cache *miss* with `CacheReason::StoreError`, then a live call
  (`adaptive/src/response_cache/intercept.rs:209-216`, `:313-320`).
- **The shared namespace is declared, not derived, and an empty one is
  rejected.** "One response-cache namespace must not span mutually untrusted
  tenants or upstreams. The empty default is an unconfigured sentinel rejected
  when the response-cache section is enabled"
  (`adaptive/src/config.rs:201-206`).
- **A schema version is folded into every key** so a shape change makes old
  entries unreachable rather than misread (`CACHE_SCHEMA_VERSION`,
  `adaptive/src/response_cache/store.rs:27-33`).

---

## 7) Metering and accounting: Relay exports, it does not account

**Pricing is configuration, not source** — the same rule roundhouse states in
`roundhouse-fleet/src/frontier.rs`. A `PricingCatalog { version, entries }` is
loaded inline or from a JSON file (`core/src/codec/model_pricing.rs:84-89`,
`:154-167`), installed into a *process-global* `RwLock<Arc<PricingResolver>>`
(`:23-24`) by a plugin (`core/src/plugins/model_pricing.rs:65-95`). A
`ModelPricing` entry carries `provider`, `model_id`, `aliases`, `currency`,
`unit`, `rates` / `rate_schedule`, `prompt_cache`, **`pricing_as_of`** and
**`pricing_source`** (`core/src/codec/model_pricing.rs:267-293`), and duplicate
aliases are a hard error (`PricingCatalogError::DuplicateModelAlias`, `:38-43`).

Two convergences and one divergence with roundhouse's catalog worth recording:
Relay likewise **rejects a duplicate key** and likewise **records provenance and
a date** on every entry. But `ModelPricing` has **no capability field** — no
`quality_prior`, no band. Relay's `LlmOptimizationSummary.baseline_model` is
whatever a plugin asserted; roundhouse's capability gate
(`crates/roundhouse-core/src/metrics/pricing.rs:33-35`, `:58-60`, `:363`,
`:373-386`) has no Relay counterpart. That remains roundhouse's strongest
upstream contribution, unchanged at 0.8.2.

**Per-call optimization accounting exists and is bounded, but nothing shipped
produces it.** `LlmOptimizationRecorder` is created per managed LLM call
(`core/src/api/llm.rs:1675`, `:1899`) with hard bounds — 64 contributions,
16 KB each, 256 KB total, 64 attempts before the recorder seals itself
(`core/src/api/optimization.rs:22-31`). Grepping the CLI, adaptive and plugin
crates for a producer finds **none**: the contributors are third-party plugins
via the SDK, and the summary's only consumers are the OTLP / OpenInference
projections (`core/src/observability/mod.rs:178-283`).

**Aggregation happens in exactly one place, and it is a document, not a ledger.**
`compute_final_metrics` folds a trajectory's steps into
`AtifFinalMetrics { total_*_tokens, total_cost_usd, total_steps }`
(`core/src/observability/atif.rs:1462-1546`) and writes it into the ATIF file.
Nothing reads it back.

**Everything else is push-only OTLP.** Counters, up-down counters, gauges and
histograms, default temporality `Cumulative` ("accumulate values from process
start"), exported every 60 s, capped at 256 instruments and 2 000 cardinality
(`core/src/observability/otel_metrics.rs:44-59`, `:890-1005`). A restart resets
every counter; the collector is expected to cope.

**And there is no enforcement of anything.** Grepping all three crates for
`budget|spend_limit|quota|rate_limit|fair.?use|cost_cap|spending` returns only
byte/entry budgets for plugin snapshots and attestations — plus exactly one
substantive hit: `RoutingPolicy.session_cost_cap: Option<f64>`
(`adaptive/src/acg/policy.rs:118-121`), which appears **once in the entire
crate**, i.e. it is declared and never read. The deep dive's finding that
"nothing enforces it" is re-confirmed at 0.8.2.

---

## 8) How far Relay trusts the client to carry history: completely

Relay's contract with the agent is that **the request is the conversation**. The
gateway buffers the body once and forwards those bytes; the only path by which
the forwarded body differs from the received body is a plugin request-intercept
mutating `LlmRequest.content`, after which the whole value is re-serialized
(`cli/src/gateway/mod.rs:927-943`). Nothing Relay ships appends, removes or
reorders messages: ACG's Anthropic translation emits *hint directives* —
`CacheBreakpoint` and `ApplyTtl` on `cache_control` annotations
(`adaptive/src/acg/translation/anthropic.rs:250-286`) — and the adaptive-hints
intercept adds a header. The one interception that touches content at all is the
PII-redaction crate, and it is stateless: "structure-preserving removal"
(`nemo-relay-pii-redaction-0.8.2/src/trajectory.rs:1-4`) with no token vault, no
reversible mapping, nothing kept (grepping its `src/*.rs` for
`vault|persist|reversib` returns nothing; the only `Mutex`es are a test mutex
and a backend-provider slot).

The corollary is §0's headline. `stateful_request_bypass_reason`
(`adaptive/src/response_cache/key.rs:128-147`) refuses to cache a request that
sets `store` truthily, or carries `previous_response_id`, or carries
`conversation` or `container`, or looks like a Responses-surface body (`input` /
`instructions` / object `prompt`) without `store: false`. Each gets its own
reason code — `StatefulStore`, `StatefulPreviousResponseId`,
`StatefulConversation`. Relay's rule is that **the moment the protocol implies
someone else is holding the conversation, Relay's own memory is unsafe**. That
is a principled position, and it is worth D1 noticing that it is *also* an
argument for roundhouse's log: the state has to live somewhere, and Relay's
answer is "in the client".

---

## 9) The two lists D1 asked for

### 9.1 What a roundhouse P0 mode gets for free by adopting Relay's posture

Each of these is a design decision Relay has already made, tested and shipped
under Apache-2.0, that a P0 mode would otherwise have to relitigate:

1. **Session identity by client assertion, with isolation on ambiguity.** The
   four-branch `gateway_session_for_call` (`cli/src/sessions/correlation.rs:329-348`)
   is a complete answer to "which conversation is this" that needs no store: an
   explicit id, else sole-active, else a stable proxy root, else a throwaway root.
   Roundhouse's `Conversations::resolve` already reaches the same *refusal*
   posture from the other direction (M12.1 F9); branch 4 is the P0 spelling of it.
2. **Pass-through credentials with a single env fallback, and nothing stored.**
   `inject_provider_auth_with_env` (`cli/src/gateway/mod.rs:1069-1107`) —
   client key wins, config header next, env last, and never both.
3. **A loopback-only bind as an invariant, enforced at boot** with an error
   naming the address (`cli/src/server/mod.rs:92-97`). P0's blast radius is a
   config check, not a deployment doc.
4. **One process per user per endpoint, arbitrated by a file lock + owner record
   + ready file, with reuse-if-compatible and refuse-if-foreign**
   (`cli/src/bootstrap/state.rs:103-179`, `cli/src/bootstrap/mod.rs:168-266`).
   This is the whole "what if two of me are running" problem, solved without a
   store — and it is exactly the problem `topham` will meet.
5. **Idle suicide as the state-bounding mechanism.** Sessions swept at 30 s
   (`cli/src/sessions/idle.rs:18-19`); the process itself exits after an
   operator-set idle window (`cli/src/server/mod.rs:843-895`). Process state that
   is *designed* to be lost costs nothing to lose.
6. **TTL-and-turn-boundary bounds on all correlation state** — 300 s hint TTLs
   plus an explicit `clear_correlation_state` at every turn boundary
   (`cli/src/sessions/mod.rs:47-49`, `:1475-1479`, `:1846-1859`). A P0 mode's
   node-local tables want a bound; this is a shipped one, and it is *different*
   from roundhouse's capacity bound on the call and thread tables — TTL bounds
   staleness, capacity bounds memory, and a P0 mode plausibly wants both.
7. **Fail-open with a 2-second deadline on any shared store**
   (`adaptive/src/response_cache/store.rs:355-357`,
   `adaptive/src/response_cache/intercept.rs:209-216`) — with the degradation
   *recorded* as a typed reason rather than swallowed.
8. **A declared, non-empty tenancy namespace on any shared store, rejected when
   empty** (`adaptive/src/config.rs:201-206`), and a schema
   version folded into every key (`store.rs:27-33`).
9. **The trajectory as an artifact rather than a store**: cumulative,
   non-destructive snapshots at turn boundaries, whole-file atomic writes, one
   file per top-level scope with a mandatory `{session_id}` in the template
   (`cli/src/events/mod.rs:31-37`; `core/src/observability/plugin_component.rs:459-462`;
   `core/src/observability/private_file.rs:106-137`). A P0 mode that keeps no log
   can still leave a defensible record.
10. **Pricing as configuration with `pricing_as_of` + `pricing_source` on every
    entry, and duplicate aliases as a hard error**
    (`core/src/codec/model_pricing.rs:267-293`, `:38-43`).
11. **Two config layers plus env, deep-merged at boot, with legacy shapes as hard
    errors** — search path lowest-to-highest at
    `cli/src/configuration/mod.rs:1188-1207`, merge and apply at `:1144-1155`,
    the two legacy-shape rejections at `:1129-1143` — the 0.8.0 Finding 4.2
    shape, re-verified unchanged at 0.8.2.

### 9.2 What roundhouse already does that Relay has no analogue for

Not "does better" — *has no counterpart to*, at any point in the 0.8.2 tree:

| Roundhouse | Relay 0.8.2 |
|---|---|
| A durable session log with a total order (`seq`), read back into routing and answers | nothing; the ATIF file is write-only |
| **Prefix admission** — comparing a client's claimed history against the committed log (`crates/roundhouse-server/src/responses_api.rs:569-605`) | nothing (§5.2); ACG's span-hash mismatch is diagnostic only |
| **Generations / forking** — `#g{n}` when a client edits its own history (`crates/roundhouse-server/src/conversations.rs:364-430`) | nothing; `generation_id` is an unrelated pass-through field (§2) |
| **Exact correlation tables** — tool-use id and thread id bound to the session that emitted them (`conversations.rs:139`, `:278`, `:459`, `:499`) | best-effort *scoring*: `hint_match_score` weights subagent 8, conversation/generation/request 4 each, model 1, and a tie is treated as ambiguous (`cli/src/sessions/correlation.rs:8-33`) |
| **A refusal when this node never bound the name** (M12.1 F9) | not applicable: no cross-node question exists (loopback-only) |
| **A writer lease per session**, and a turn admitted under it | nothing |
| **A spend ledger with holds and settlement** (`crates/roundhouse-core/src/control/spend.rs`) | nothing; per-call cost is stamped and exported (§7) |
| **Fair-use windows across nodes** — one hash per (scope, bucket), `PEXPIRE` at the widest window (M13) | nothing; `session_cost_cap` is declared and never read |
| **A capability gate on the savings comparison** (`crates/roundhouse-core/src/metrics/pricing.rs:33-35`) | no capability field in the pricing catalog |
| **An MCP control surface** — `prefer`, `status`, `declare_intent` naming a conversation | `tools/list` returns `[]` (`cli/src/mcp/protocol.rs:72-76`) |
| **Steering** — answering a turn with guidance | no primitive can synthesize model output (deep dive §C.5, unchanged) |
| **A narrow-only policy lattice** | ordered plugin priorities; a later intercept can widen |
| **Per-project / per-user credential scopes** | one process-wide env fallback (`cli/src/gateway/mod.rs:1083-1090`) |
| **Idempotent retry, replay, reconciliation** | none of the three exists (§5.3) |
| **A multi-node deployment at all** | a non-loopback bind is a hard error (`cli/src/server/mod.rs:92-97`) |

---

## 10) Notes on the prior reads

- **[2026-09-02] `nemo-relay-0.8.0-published-read.md` Finding 4.2 holds at
  0.8.2** for the config layering (user then system `config.toml`, sibling
  `plugins.toml`, env last): `cli/src/configuration/mod.rs:1144-1155` and
  `:1188-1207`. What that finding did not say, and this one does: the config
  snapshot is taken at boot and there is **no route or API that mutates it at
  runtime** — the 14-route table (`cli/src/server/mod.rs:634-648`) contains no
  admin surface.
- **[2026-09-02] `nemo-relay-deep-dive.md` §C.3's five adaptive behaviours are
  intact at 0.8.2**, and its observation that ACG's `RoutingPolicy.session_cost_cap`
  is enforced by nothing is re-confirmed: exactly one occurrence in the crate
  (`adaptive/src/acg/policy.rs:121`). §C.3 did not record the *storage*
  consequences, which §6 above adds: no TTL and no trim on the adaptive Redis
  keys, and up to 100 raw prompt snapshots per profile retained by ACG.
- **[2026-09-02] Dependency vigilance.** `nemo-relay-adaptive-0.8.2` declares
  `redis = "1.1"` (caret, optional, `tokio-comp` + `connection-manager`) and the
  CLI enables it (`nemo-relay-adaptive-0.8.2/Cargo.toml:68-76`,
  `nemo-relay-cli-0.8.2/Cargo.toml:124-125`). Roundhouse pins `redis = "1.2"`
  with the ceiling recorded in the manifest (`Cargo.toml:147-155`, 1.3.0+ carries
  a target-gated `tokio ^1.51`). The caret resolves under roundhouse's ceiling,
  so a dependency on the adaptive crate would not move the pin today. This should
  be re-checked at the next `nemo-relay-*` bump.

---

## 11) What this evidence cannot settle

1. **Whether Relay's ATIF snapshot cadence is affordable at roundhouse's turn
   rate.** Each snapshot serializes the whole trajectory and rewrites the file;
   the cost is O(conversation) per turn, i.e. O(n²) over a session. Nothing in
   the tree measures it, and this read did not run anything.
2. **Whether the 30-second idle sweep is tuned or arbitrary.** It is a bare
   `const` with no comment justifying 30 (`cli/src/sessions/idle.rs:18`).
3. **What a `nemo-relay-plugin` contributor actually records** into
   `LlmOptimizationRecorder`. Nothing in the published crates produces a
   contribution, so the bounds in `core/src/api/optimization.rs:22-31` are
   untested against a real producer here.
4. **Whether the ACG observation window's raw-content retention is intended or
   incidental.** `PromptBlock.content: String` plus a no-TTL Redis `SET` is a
   fact; whether NVIDIA regards it as a retention posture is not readable from
   the source. If roundhouse ever adopts the adaptive crate, this is the question
   to ask upstream first.
5. **Whether Relay would accept the capability gate.** §7 establishes the gap
   exists; whether a `quality_prior`-shaped field is welcome in `ModelPricing` is
   an upstream conversation, not a code fact.

---

## Fact-check (2026-09-02)

An independent re-derivation of every negative and every high-stakes claim above, from the primary sources at the pinned revisions, by a second reader who did not write this document. Verdicts: 20 verified, 0 corrected, 0 unestablished.

Independently re-derived all 10 negative claims and all 8 high-stakes claims from D1 dive relay-proxy-posture against Relay 0.8.2 registry sources and roundhouse 7c5369a; also spot-checked 2 medium claims (9, 11). Every claim verified — same facts, same or compatible line ranges. Two immaterial precision notes (not corrections): claim 1's process-exit-on-idle is gated behind an explicit NEMO_RELAY_PLUGIN_IDLE_TIMEOUT_SECS env var rather than unconditional (still "operator-set"); negative 6's /healthz body returns two more harmless fields (status, service, bootstrap_protocol) beyond "version plus instance id." No claim required correction; none was unestablished. Full evidence draft written to scratchpad. Grep methodology for all negatives reproduced independently and matched the draft's hit counts and locations exactly, including the single sqlite hit (a negative test) and the single session_cost_cap hit (declared, never read).
