<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Dynamo sequence lifecycle: an upstream proposal

> **Status: draft for the owner to file, 2026-09-29.** Sections 2 to 7 are
> written for Dynamo maintainers who do not know Roundhouse. Section 1 is the
> Roundhouse side: why Roundhouse needs this, and what it does until Dynamo
> acts. Every Dynamo claim is cited at both the Roundhouse pin
> `ac7b7513790ef1d619b46f805aea03c9f21200ba` (2026-08-08) and Dynamo `main`
> at `6822babc5c542350127e490f4fb8f2042855559c` (2026-09-29). The two are 966
> commits apart, and several claims changed between them (§1.4).

Citation forms: `[dynamo@ac7b751 path:line]` is the pin.
`[dynamo@6822bab path:line]` is `main`. `[aisimulate@v0.12.0 path:line]` is
the mocker engine crate that Dynamo `main` pins
[dynamo@6822bab Cargo.toml:59-60]. Roundhouse references name the section of
`prefix-anchored-routing-proposal.md` (the proposal) or
`../research/prefix-anchored-routing-evidence.md` (the evidence).

## 1. Status and preface (Roundhouse side)

### 1.1 What Roundhouse sends today

Roundhouse is a router in front of several Dynamo deployments. Each agent
conversation lineage that Roundhouse tracks is one **sequence**: one
append-only KV lineage. A compaction or a history rewrite starts a new
sequence. The old one is the predecessor, the new one is the successor
(proposal §18.1, §19.2).

Roundhouse sends exactly one identity downstream:

```text
x-dynamo-session-id: hex(HMAC-SHA256(K, "rh-sequence-v1\0" || principal ns || surface || label || generation)[..16])
```

- 32 hex characters, keyed, stable across Roundhouse nodes (proposal §18.6).
- No client header is forwarded. No parent header is sent (proposal Q21).
- Roundhouse does not tell Dynamo how to invalidate. It only states that one
  id will not be reused (the owner's direction, proposal addendum of
  2026-09-29).

### 1.2 Why Roundhouse needs release

A compacted Codex or Claude Code conversation never resends the tail of its
predecessor. The predecessor's KV then sits in the engine until LRU pushes it
out. On an agent-heavy deployment, those dead tails compete with live
sequences for the same blocks. Roundhouse is the only party that sees the
compaction as a fact (proposal §19.1, §19.2). Dynamo sees two unrelated ids.

A lost release costs memory, never correctness. A later request that reuses a
released id is ordinary work (proposal §19.3, §20.4 rule 3). That is why this
is a hint and not a protocol Roundhouse's correctness depends on.

### 1.3 What Roundhouse does until Dynamo acts

**In-band now, endpoint later** (owner ruling, proposal addendum
"readiness review", item 4):

- **Transport today:** Dynamo's own documented mechanism, a dedicated minimal
  request carrying `x-dynamo-session-final: true` and the predecessor's id
  [dynamo@ac7b751 docs/fern/pages/use-cases/agents/session-ids.mdx:36-38].
  The same text is unchanged on `main`
  [dynamo@6822bab docs/fern/pages/use-cases/agents/session-ids.mdx:36-38].
- **Off by default, shadow when enabled.** In shadow, Roundhouse classifies
  and logs the release it would send, and sends nothing. The reason is §2:
  nothing in Dynamo releases KV on that header, and a real request costs a
  scheduling slot and a small prefill (§3.1).
- **Unlock for live in-band sends, per deployment:** the deployment
  acknowledges that it handled the request as a lifecycle request and did not
  run inference on it (§3.1, the `x-dynamo-session-final-handled` response
  header proposed there). Until then, a live send is pure cost.
- **Unlock for the endpoint:** `POST /v1/sequences/invalidate` answers `202`
  on the deployment (§3.2). A `404` keeps the in-band path.
- **Unlock for measurement (ask 3):** a Dynamo pin at or after `8970b35248`
  (§1.4). Moving the pin is a synergy-dependency upgrade under `CLAUDE.md`:
  diff first, map each change, and write a dated addendum.
- The **owner files this** with the Dynamo team. The ready-to-file body is §7.
- Questions **Q28** (a bearer token per deployment, TLS off loopback) and
  **Q29** (one invalidation URL per deployment; Dynamo decides the fan-out to
  workers) stand as ruled.

The effective spec for this workstream is `../PLAN-session-sequence-identity.md`
(written in parallel with this document). The design record is proposal
§18 to §20. Roadmap placement is the addendum of 2026-09-29 in
`../ROADMAP-agentic-platform.md`.

### 1.4 What changed at Dynamo `main` since the pin

This is the part that changes the plan. Each item is cited in §2.

| # | Change | Effect on this proposal |
|---|---|---|
| 1 | **`kv_hints.evict_session` no longer exists.** `KvHints { evict_session }` and the `AgentContext.kv_hints` field were removed by `8b030e0ec1` (#13134, 2026-09-08). The name `kv_hint` now means a typed KV *transfer* hint. | The roadmap addendum as first committed said "shadow until Dynamo consumes `kv_hints.evict_session`". That condition can never be met, and both the roadmap addendum and the plan now use the unlock of §1.3. The unlock is now "Dynamo releases KV on `session_final`" (§1.3). Ask 1 is phrased against `agent_context.session_final`, which survives. |
| 2 | **The native ThunderAgent plugin consumes `session_final`** (`cde7c9a8c5`, #15164, 2026-09-23). It drops its program-table entry. It releases no KV, and it still dispatches the request to a worker. The Python prototype at the pin did the same drop, but short-circuited the request. | `session_final` has a reader now, but not the one Roundhouse needs. Ask 1 builds on this consumer path rather than inventing one. |
| 3 | **Engines now receive the session id.** vLLM (`43b6344b87`, #14428), SGLang (`b2d456f2bd`, #14611, for session-aware radix ownership), and TensorRT-LLM (conversation affinity, already at the pin). None receives `session_final`. | Ask 1's engine half has a place to land: the id already reaches the engine. |
| 4 | **The router can track per-session block lineage** (`SessionPrefixIndexer`, `adfe2aa1b1`, #13807, 2026-09-16, off by default). Its nodes carry frontier reference counts. It has no per-session removal. It evicts sessions only by LRU. | The shared-prefix bookkeeping ask 1 needs exists at the router level. Ask 1 asks for a removal entry point. |
| 5 | **The mocker fills `usage.prompt_tokens_details.cached_tokens`** for its vLLM scheduler model (`8970b35248`, #12711, 2026-08-10, two days after the pin). Its SGLang scheduler model still reports none. | Ask 3 shrinks to SGLang parity plus an HTTP-level test. |
| 6 | **Dynamo reads Codex `thread-id`, not `session-id`,** as the Codex session (`3b7675444c`, #12331, 2026-08-11). | No effect: Roundhouse sends `x-dynamo-session-id`, which wins over every native header. The evidence's §15.7 note about Codex families sharing a worker is pin-only. |
| 7 | **Session affinity gained hard and soft modes** and a subagent group binding (`052c1e2853`, #13907). Neither reacts to `session_final`. | Ask 1 includes dropping the affinity binding. |
| 8 | Nothing changed for asks 2 and 4. No release route exists, and `kv_used_blocks` is still not exported. | Asks 2 and 4 stand as written. |

Two corrections to the evidence, to be recorded there as dated notes:

- Evidence §15.8 and §15.12 claim 8a are right that nothing at the pin reads
  `evict_session` or `kv_hints`. But the pin already had one reader of
  `session_final`: the Python ThunderAgent router
  [dynamo@ac7b751 components/src/dynamo/thunderagent_router/__main__.py:57-60, :183-193].
  It releases scheduler state only.
- Evidence §14.1 says `/busy_threshold` is mounted only when the admin API is
  on. That is true, but the admin API is **on by default** and the route has
  no authentication [dynamo@ac7b751 lib/llm/src/http/service/service_v2.rs:669],
  [dynamo@6822bab lib/llm/src/http/service/service_v2.rs:805-809]. It is a
  precedent for mounting, not for security (§5).

## 2. What Dynamo has today

### 2.1 Method

- Blobless clone of `github.com/ai-dynamo/dynamo`, with worktrees at the pin
  and at `main`. `aisimulate` cloned at tag `v0.12.0`.
- Greps over `.rs`, `.py`, `.md`, and `.mdx`, excluding tests where a claim is
  about production code. Every negative below is a grep, stated with its
  pattern.
- `git log -S` and `git log --grep` for history. GitHub issue and PR titles
  were searched. Issue bodies could not be read from this session, so issue
  claims are titles only (§2.10).
- Nothing was built or run.

### 2.2 Summary

| Surface | Pin `ac7b751` | `main` `6822bab` |
|---|---|---|
| `x-dynamo-session-id` → `agent_context.session_id` | Yes. Wins over native headers. | Same [agents.rs:71-84] |
| `x-dynamo-session-final` → `agent_context.session_final` | Yes | Same [agents.rs:21, :72] |
| `session_final` → `kv_hints.evict_session` | Yes. Constructed, never read outside tests. | **Removed** (#13134) |
| Readers of `session_final` | Python ThunderAgent router: drops program state, short-circuits the request | Python prototype (same) and **native ThunderAgent plugin**: drops program state, then dispatches the request |
| Session id forwarded to engines | TensorRT-LLM only | vLLM, SGLang, TensorRT-LLM |
| `session_final` forwarded to engines | No | No |
| Per-session KV release anywhere | No | No |
| Router-side per-session block lineage | No | `SessionPrefixIndexer`, off by default, LRU-only session removal |
| Session affinity | TTL table, off unless a TTL is set | Same, plus hard/soft modes and a subagent group key. No `session_final` handling. |
| Mocker `cached_tokens` over HTTP | Never filled | Filled by the vLLM model. SGLang model still `None`. |
| `kv_used_blocks` / `kv_total_blocks` as gauges | No | No |
| Frontend release route | None | None |

### 2.3 The identity path

- **Pin.** The header map reads `x-dynamo-session-id` first, then native
  headers. For Codex it takes `session-id`
  [dynamo@ac7b751 lib/llm/src/protocols/agents.rs:11, :34, :60]. The frontend
  builds `AgentContext` and sets `kv_hints: { evict_session: true }` when
  `session_final` is `true`
  [dynamo@ac7b751 lib/llm/src/protocols/common/extensions.rs:64-69, :81-90, :255-266].
- **Main.** `x-dynamo-session-id` still wins
  [dynamo@6822bab lib/llm/src/protocols/agents.rs:71-84]. Codex now maps
  `thread-id` [agents.rs:37-42]. A Codex compaction request (turn metadata
  `request_kind: "compaction"`) sets `agent_context.compaction`
  [agents.rs:73-74, :118-123], [dynamo@6822bab lib/llm/src/protocols/common/extensions.rs:81, :122-125].
  `AgentContext` keeps `session_final` and has no `kv_hints`
  [extensions.rs:108-133, :366-376].
- **The removal.** `8b030e0ec1` (#13134, "define typed KV hint contract")
  deletes `KvHints`, `evict_session`, and the construction. The replacement
  `KvHint` is a versioned action envelope, protocol `0.1`, whose one action
  today is `kv.fetch@1.0`, a transfer from a source worker
  [dynamo@6822bab lib/kv-router/src/kv_hints.rs:4, :23-24, :38-43, :82-86].
  The router attaches it to the selected backend request
  [dynamo@6822bab lib/llm/src/kv_router/routing_host/kv.rs:543],
  [dynamo@6822bab lib/llm/src/protocols/common/preprocessor.rs:446]. A code
  comment says the names will change to "the matching KVCC names"
  [kv_hints.rs:15].
- **Docs.** The documentation still calls session identity "passive
  metadata" that does not change placement unless a session-aware policy is
  configured [dynamo@6822bab docs/fern/pages/use-cases/agents/session-ids.mdx:80].

### 2.4 Who reads `session_final`

- **Python ThunderAgent router, pin and main.** It ends the program and
  returns without forwarding: "A request marked session_final just releases
  the program from the table and is NOT forwarded to the engine"
  [dynamo@ac7b751 components/src/dynamo/thunderagent_router/__main__.py:183-193],
  [dynamo@6822bab components/src/dynamo/thunderagent_router/__main__.py:197-207].
- **Native ThunderAgent plugin, main only.** The classifier registers the
  request with its `session_final` flag
  [dynamo@6822bab lib/router-plugins/builtin/src/thunderagent/request_classifier/mod.rs:294].
  At admission, `begin_session_final` removes the program and then **releases
  the request** to the program's assigned worker
  [dynamo@6822bab lib/router-plugins/builtin/src/thunderagent/request_classifier/scheduler.rs:469-470, :531-547].
  So the request is dispatched and served like any other. Its docs say the
  final request "releases the retained program state; otherwise idle
  retention expires it"
  [dynamo@6822bab docs/fern/pages/use-cases/agents/thunderagent-program-scheduler.md:38].
- **Both release scheduler bookkeeping only.** The docs are explicit that the
  plugin's estimate "is not physical KV occupancy"
  [thunderagent-program-scheduler.md:51].
- **The worker selection API exposes it** to custom policies as
  `SessionContext::session_final()`
  [dynamo@6822bab lib/kv-router/src/scheduling/types.rs:320-358],
  [dynamo@6822bab lib/llm/src/kv_router.rs:373-398].
- **Nobody emits it in-tree.** The only emitter named is an external harness
  switch, `DYN_AGENT_SESSION_FINAL`, in a benchmark recipe
  [dynamo@6822bab components/src/dynamo/thunderagent_router/README.md:169-237].
  "Dedicated minimal request" is not defined anywhere.

### 2.5 KV release surfaces (negatives)

- **No per-session release, pin or main.** A grep of `.rs` and `.py` for
  `(invalidat|release|evict|forget|expire)_(session|sequence)`,
  `(session|sequence)s?_(invalidat|release|evict|forget|close|end)`,
  `/v1/(sessions|sequences)`, `close_session`, and `end_session`, excluding
  tests, finds only:
  - `release_session` in KVBM, whose `SessionId` is a transfer-session UUID,
    not an agent session
    [dynamo@6822bab lib/kvbm-engine/src/leader/instance.rs:600-608],
    [dynamo@6822bab lib/kvbm-engine/src/leader/session/mod.rs:99]. Same at the pin.
  - the pin's `evict_session` field (§2.3), and unrelated queue expiry.
- **No release route.** The route strings under `lib/llm/src/http/service/`
  at `main` add `/generate` and `/native/sglang` since the pin. Both are
  inference. No lifecycle route exists.
- **No engine receives `session_final`.**
  - vLLM passes `session_id` to `engine_client.generate`
    [dynamo@6822bab components/src/dynamo/vllm/handlers.py:3388],
    [dynamo@6822bab components/src/dynamo/common/backend/agent_context.py:20-29].
  - SGLang forwards `session_id` and `parent_session_id` only
    [dynamo@6822bab components/src/dynamo/sglang/agent_session.py:26, :55-60].
    With `--enable-session-radix-cache` (SGLang 0.5.15 or later), that id
    gives "session-aware radix ownership" and self-registers
    [dynamo@6822bab components/src/dynamo/sglang/AGENTS.md:372-392]. At the
    pin, Dynamo did not forward it
    [dynamo@ac7b751 components/src/dynamo/sglang/CLAUDE.md:316-321].
  - TensorRT-LLM forwards the id as `ConversationParams` for attention-DP rank
    affinity
    [dynamo@6822bab components/src/dynamo/trtllm/conversation_affinity.py:19-21].
  - What each engine does with the id beyond this was not read. The engine
    sources are not in the Dynamo tree.
- **`clear_kv_blocks`** is still a per-worker control call that flushes the
  whole prefix cache (evidence §15.8 at the pin;
  [dynamo@6822bab lib/backend-common/src/worker.rs:2344] at `main`). It is not
  scoped to a session.

### 2.6 Router-side lineage: `SessionPrefixIndexer` (main only)

- "Session lineage tracked independently of physical cache eviction"
  [dynamo@6822bab lib/kv-router/src/session_prefix_index.rs:4].
- Each node carries `frontier_refs` and `child_count` [:27-33]. Sessions map
  to per-worker frontier nodes [:139-160]. Removed-block events update it
  [:263].
- Sessions leave only by LRU at 16,384 sessions [:18, :404-420]. There is no
  per-session removal in its public API [:116-300].
- Off by default: `enable_session_prefix_index: false`
  [dynamo@6822bab lib/kv-router/src/scheduling/config.rs:880-882], flag
  `--enable-session-prefix-index`
  [dynamo@6822bab components/src/dynamo/common/configuration/groups/kv_router_args.py:665-666].

This is the natural place to compute "blocks referenced only by this
session" at the router, if the engines cannot.

### 2.7 Session affinity

- A TTL table. Off unless `--router-session-affinity-ttl-secs` is set
  [dynamo@6822bab lib/llm/src/session_affinity/mod.rs:32-43]. At most 65,536
  entries, ids up to 256 bytes
  [dynamo@6822bab lib/kv-router/src/services/selection/affinity.rs:28-30].
- Modes `hard` (the default) and `soft` [affinity.rs:33-41].
- A request with a parent session id binds under a group key of the parent
  [dynamo@6822bab lib/llm/src/kv_router/routing_host.rs:585-605]. Roundhouse
  sends no parent header, so this does not apply to it.
- No `session_final` handling at the pin or at `main` (grep of
  `lib/llm/src/session_affinity/` and `affinity.rs`). A final request
  *refreshes* the binding it should end.

### 2.8 Cached tokens over HTTP

- **Pin.** The mocker never constructs `PromptTokensDetails` (evidence §13,
  claim 8). Its sidecar tests set `prompt_tokens_details: None`.
- **Main.** `8970b35248` (#12711) adds `usage_with_cached_tokens`, which fills
  `prompt_tokens_details.cached_tokens` from the scheduler's admission truth
  [dynamo@6822bab lib/llm/src/mocker.rs:197-218]. The first chunk carries it
  [mocker.rs:1137-1144].
- The scheduler now lives in `aisimulate-core` `=0.12.0`. Its vLLM model sets
  the value [aisimulate@v0.12.0 crates/core/src/engine/scheduler/vllm/core.rs:70, :2297-2310].
  Its SGLang model sets `cached_tokens: None` on every output
  [aisimulate@v0.12.0 crates/core/src/engine/scheduler/sglang/decode.rs:255, :370].
- The frontend copies backend `prompt_tokens_details` into the response usage
  [dynamo@6822bab lib/llm/src/protocols/openai/delta_common.rs:153-158]. The
  Anthropic and Responses converters read it
  [dynamo@6822bab lib/llm/src/protocols/anthropic/stream_converter.rs:1466],
  [dynamo@6822bab lib/llm/src/protocols/openai/responses/mod.rs:1357].
- No HTTP-level test asserting `cached_tokens` on a mocker response was found.
  The mocker's own test asserts the engine output
  [dynamo@6822bab lib/llm/src/mocker.rs:1482-1495].

### 2.9 Engine-reported KV gauges

- `WorkerLoadMetrics` still has only `active_decode_blocks` and
  `active_prefill_tokens`
  [dynamo@6822bab lib/llm/src/kv_router/metrics.rs:619-622]; the pin is the
  same [dynamo@ac7b751 lib/llm/src/kv_router/metrics.rs:474-477].
- The frontend stores `kv_used_blocks` and `kv_total_blocks` per rank
  [dynamo@6822bab lib/llm/src/discovery/worker_monitor.rs:249-252], writes them
  in `update_from_active_load` [:439] and from runtime configuration [:888],
  and uses them for the overload latch only. No gauge is set from them. A grep
  for metric names containing `kv_used`, `used_blocks`, or `kv_blocks` finds
  only `model_total_kv_blocks`
  [dynamo@6822bab lib/runtime/src/metrics/prometheus_names.rs:259].
- The mocker produces no `kv_used_blocks`, in the Dynamo tree or in
  `aisimulate` `v0.12.0` (grep).

### 2.10 History, and existing upstream discussion

- **The `trajectory` naming.** Release notes for 1.3.0 describe
  `trajectory_final` and `kv_hints.evict_trajectory`
  [dynamo@6822bab docs/fern/pages/reference/general/releases/dynamo-v1-3-0.mdx:119].
  `8f607389b9` (#10896, 2026-06-23) renamed trajectory to session. No `.rs` or
  `.py` file at the pin or at `main` contains `evict_trajectory` or
  `trajectory_final`. A search for "trajectory" finds the old name only in that
  release note.
- **SGLang explicit sessions were removed.** Before `5407083c16` (#10214,
  2026-06-23, an ancestor of the pin), the SGLang worker had `open_session` and
  `close_session`, the latter documented as "Close a streaming session and
  release its KV resources". The same commit deleted the router's sticky
  session lifecycle module. Dynamo moved from explicit per-session lifecycle
  to passive session ids. This proposal asks for a narrow release again. §6
  asks why the explicit path was dropped, so the ask does not repeat a
  rejected design.
- **Issues and PRs found by title** (bodies not readable from this session):
  - #11673, open, "DEP: KV Cache Controller". The `kv_hints.rs` comment about
    "KVCC names" suggests this is where a release action belongs.
  - #13010, closed, "[draft] Programmatic KV-cache control from Dynamo".
  - #10697, open, "Router: Track historic kv cache blocks active in last 120s".
  - No issue or PR titled for per-session KV release was found.

### 2.11 Not verified

- What vLLM, SGLang, or TensorRT-LLM do with a session id at the engine, and
  whether any can release by it. The engine sources are not in the tree.
- Whether vLLM's `kv_cache_usage` counts evictable prefix-cached blocks as used
  (evidence §14.11, still open).
- The bodies of #11673 and #13010.

## 3. The asks

### 3.0 The contract all four asks share

A release is **one statement: "id S will not be reused by this caller."**

- **Advisory.** Dynamo may free, deprioritize, offload, or ignore. The caller
  never assumes a physical effect.
- **Shared-prefix safe.** Only blocks that no live session and no in-flight
  request references may be freed. A system prompt shared by a thousand
  sessions stays.
- **Never a refusal.** A later request that carries S is served as ordinary
  work. It is cold for whatever was freed. Nothing rejects it.
- **Idempotent.** A second release of S, or a release of an unknown S, is a
  successful no-op.
- **Ordered after the successor.** The caller sends a release only after the
  successor's first request has received response headers from the same
  deployment. The successor then holds its shared prefix under its own id
  before the predecessor lets go.
- **Opaque ids.** S is a 32-character hex string. It carries no content and
  no label (§5).

### 3.1 Ask 1: release KV on `session_final`

**Problem.** `x-dynamo-session-final: true` is documented as the way to let
"lifecycle-aware consumers release per-session state"
[session-ids.mdx:36-38]. No consumer releases KV (§2.4, §2.5). At `main` the
native ThunderAgent plugin runs the final request as inference. The session
affinity binding is refreshed by it (§2.7). So a caller that follows the
documentation today pays for a request and gets nothing.

**Proposed behavior.** When a request carries `session_final: true` for id S:

1. **Router-local state ends.** Drop the session affinity binding for S.
   End S in the ThunderAgent program table (as today). Remove S from the
   `SessionPrefixIndexer` when it is enabled.
2. **Engine release is fanned out** to every worker and rank that holds a
   binding or frontier for S. Each engine releases the blocks referenced only
   by S, by its own reference counts, or moves them to the front of its
   eviction order. An engine that cannot release by session ignores it.
3. **The request is not run.** No prefill, no decode, no worker slot. This
   is the Python prototype's behavior today, and it should be the only
   behavior.
4. **The frontend answers at once** with a well-formed empty response and the
   header `x-dynamo-session-final-handled: true`. A caller can then tell a
   deployment that handled the release from one that served ordinary
   inference.

**What "minimal request" should mean.** Propose defining it in the docs:

```http
POST /v1/chat/completions
x-dynamo-session-id: a1b2c3d4e5f60718293a4b5c6d7e8f90
x-dynamo-session-final: true
content-type: application/json

{"model": "<served model>", "messages": [{"role": "user", "content": ""}], "max_tokens": 1}
```

- Only `model` needs to be meaningful. It selects the pool whose workers hold
  S. The messages are ignored.
- Response: `200`, an empty assistant message, `finish_reason: "stop"`,
  `usage` all zero, and `x-dynamo-session-final-handled: true`.
- The same on `/v1/responses` and `/v1/messages`.
- If S is unknown, or its bound worker is gone, the answer is the same `200`.
  Hard affinity mode must not turn a final request for a dead binding into an
  error.

**Where the engine half could land.** Dynamo `main` already has a versioned
per-request KV action envelope, `KvHint`, with one action, `kv.fetch@1.0`
(§2.3). A `kv.release@1.0` action with payload `{"session_id": S}` would use
the same wire to the worker, with no new vocabulary. Whether that belongs in
the KV Cache Controller work (#11673) is §6 question 1.

**Semantics.**

- Idempotent, advisory, never a refusal (§3.0).
- **In flight.** If a request carrying S is still running, release after it
  ends.
- **Shared prefix.** Only blocks whose sole referent is S, with no live
  frontier from another session and no in-flight request, are released. At
  the router, that is a `SessionPrefixIndexer` node whose `frontier_refs` and
  `child_count` fall to zero after S is removed.
- **Reuse after release.** A later request carrying S starts a fresh binding
  and is served normally.

**How to test it.** Everything below runs on the mocker, which already emits
KV events and (at `main`) cached-token usage.

- Two sessions A and B share a system prompt. Each has its own tail. Send a
  final for A. Assert that `Removed` KV events cover only A's tail hashes. Then
  B's next request reports the same `cached_tokens` as before.
- The final request produces no engine request (a mocker request counter
  stays unchanged), and the response carries the handled header.
- Send the final twice, and once for an unknown id. All three answer `200`.
  The second produces no events.
- A final sent while a request on A is in flight releases after that request
  ends.
- With `--router-session-affinity-ttl-secs` set, the binding for A is gone
  after the final. A new request on A is placed afresh.

**What Roundhouse does with it.** Roundhouse leaves shadow for a deployment
when its final request comes back with the handled header. It sends one final
request per superseded sequence per deployment that served it within the last
hour (proposal §20.7). It counts sent, handled, and not handled.

### 3.2 Ask 2: an out-of-band release endpoint

**Problem.** The in-band path of ask 1 is enough for a close. It is a poor fit
for everything else:

- **The superseded sequence has no next request.** After a compaction, the
  client only ever sends the successor. The final request must be invented by
  the caller.
- **An inference request is the wrong carrier.** It is authenticated, rate
  limited, and admitted like inference. Under overload it can be queued behind
  real work or refused with `529` (evidence §14.8), which is exactly when a
  release matters most.
- **Batching.** A burst of compactions produces many releases. One request
  per id is one round trip per id.
- **Off the request path.** A caller must not delay a user turn to deliver a
  hint. A separate endpoint lets delivery live in a background outbox with
  its own retry.
- **The successor.** A final request has no field for the successor. The
  endpoint can carry it, so a tiering policy can keep the shared prefix warm.

**Proposed API.**

```http
POST /v1/sequences/invalidate
Authorization: Bearer <deployment token>
content-type: application/json
```

```json
{
  "version": 1,
  "id": "0f3c9a4e6b21d7c85a90e1f2b3c4d5e6",
  "sequence": "a1b2c3d4e5f60718293a4b5c6d7e8f90",
  "generation": 2,
  "successor": "11223344556677889900aabbccddeeff",
  "reason": "compaction",
  "detected_by": "codex_window",
  "issued_at_ms": 1790000000000
}
```

| Field | Meaning | Dynamo must read it? |
|---|---|---|
| `version` | Contract version, `1` | Yes |
| `id` | Deterministic message id. A retry carries the same id. | Yes, for idempotency |
| `sequence` | The id that will not be reused: the value its requests carried in `x-dynamo-session-id` | Yes |
| `successor` | The id of the sequence that replaced it, or `null` for a close | Optional. A hint to keep the shared prefix. |
| `generation` | The caller's generation number | No. For logs. |
| `reason` | `compaction`, `rewrite`, or `closed` | No. For metrics. |
| `detected_by` | The caller's signal name | No. For metrics. |
| `issued_at_ms` | The caller's clock at enqueue | No |

- **Batch form:** `{"invalidations": [ ... ]}`, up to 256 messages.
- **Answers.** `202 {"accepted": n}`. An unknown `sequence` is accepted.
  `401` for a bad token. `429` or `503` means retry. `404` means the route is
  not mounted.
- **Mounting.** Behind the admin switch, like `/busy_threshold`
  [dynamo@6822bab lib/llm/src/http/service/service_v2.rs:1411-1419]. But see
  §5: that switch is on by default and unauthenticated, so this route needs its
  own bearer token and should be off unless a token is configured.
- **Recipients.** One URL per deployment. Dynamo decides how one frontend
  reaches every worker that holds the sequence, including across frontends
  when replica sync is on.

**Semantics.**

- The same effect as ask 1 for each `sequence` (§3.0, §3.1).
- **Idempotency by `id`.** A second message with one `id` is a no-op.
- **Ordering.** The caller sends only after the successor's first request has
  response headers. If a request on `sequence` is in flight, release after it
  ends.
- **`successor` and shared blocks.** Dynamo keeps shared blocks by its own
  reference counts. The successor already references them by the time the
  message arrives. The caller does not send a retained-prefix length, because
  its token counts do not map onto the deployment's template, tokenizer, or
  block size (proposal §19.4, Q27).
- **Reuse after release** is ordinary work.
- **Lost messages** cost memory until idle expiry. Nothing else.

**How to test it.** The ask 1 tests, driven through the endpoint, plus:

- A batch of 256 with duplicates releases each sequence once.
- A bad token gives `401` and no events. A missing token configuration leaves
  the route unmounted (`404`).
- With `successor` set, the blocks shared with the successor are not in the
  `Removed` events, even when the successor ran on another worker.

**What Roundhouse does with it.** Roundhouse's outbox posts batches, at least
once, with bounded retry (at once, then 1 s, 4 s, 16 s) and a cap of 10,000
queued messages (proposal §20.3, §20.5). A `404` marks the deployment as
having no route for 10 minutes, and the in-band path of ask 1 remains.

### 3.3 Ask 3: cached tokens from the mocker (reshaped)

**Status at `main`.** Mostly done. The mocker's vLLM model fills
`usage.prompt_tokens_details.cached_tokens` (§2.8). The original ask is
withdrawn for vLLM.

**What remains.**

- **SGLang parity.** The SGLang scheduler model in `aisimulate-core` reports
  `cached_tokens: None` [decode.rs:255, :370]. Fill it from the radix match
  length at admission, as the vLLM model does.
- **An HTTP-level test.** Two requests with a shared prefix, through the
  frontend, on `/v1/chat/completions`, `/v1/responses`, and `/v1/messages`.
  The second reports a non-zero `cached_tokens`. This guards the frontend
  copy [delta_common.rs:153-158], which no test found here covers end to end.

**Why it matters to routers.** A router in front of a deployment can only see
HTTP. `cached_tokens` is the one per-request measurement of a prefix hit. The
router's `kv_hit_rate` is a prediction (evidence §14.12). The mocker's
prefill time already falls on a hit (evidence §15.10). Reporting the count
makes the mocker a ground truth for routing experiments.

**What Roundhouse does with it.** Roundhouse measures its placement gain
against observed cached tokens on the mocker. That needs a Dynamo pin at or
after `8970b35248` (§1.3).

### 3.4 Ask 4: engine-reported KV occupancy as frontend gauges

**Problem.** The frontend holds the engine-reported `kv_used_blocks` and
`kv_total_blocks` per worker and rank, and exports neither (§2.9). The only
exported block gauge is the router's prediction, which leaves out output
blocks by default and cannot see engine eviction (evidence §14.3).

**Proposed behavior.** Two gauges, labeled like the existing
`worker_active_decode_blocks` (`worker_id`, `dp_rank`, `worker_type`):

```text
dynamo_frontend_worker_kv_used_blocks
dynamo_frontend_worker_kv_total_blocks
```

- Set where `update_from_active_load` and the runtime-configuration path write
  `WorkerLoadState` [worker_monitor.rs:439, :888].
- Removed in `cleanup_worker_metrics` with the other worker gauges
  [dynamo@6822bab lib/llm/src/discovery/worker_monitor.rs:36-47].
- **The vLLM question.** If vLLM's `kv_cache_usage` counts evictable
  prefix-cached blocks as used, `kv_used_blocks` saturates on every warm
  worker (evidence §14.4, §14.11). If engines can report the evictable count,
  a third gauge `dynamo_frontend_worker_kv_evictable_blocks` would make the
  first two usable. §6 asks.

**How to test it.** A mocker producer for `kv_used_blocks` (none exists,
§2.9), then a scrape asserting the two gauges per rank, and their removal when
a worker leaves.

**What Roundhouse does with it.** Calibration only. Roundhouse places by its
own in-flight accounting. It would log this signal beside that accounting,
and never place on it until the vLLM question is answered (proposal §16.5,
Q20).

## 4. Compatibility with the roadmap's lifecycle contract

`../ROADMAP-agentic-platform.md` §5 sets the direction for Roundhouse's cache
lifecycle vocabulary: `SessionCacheKey`, `CacheBundleRef`, and
`CacheLifecycleHint { Archive, Warm, Cancel }`, with required semantics. This
proposal must be the first concrete instance of that direction, not a third
vocabulary. The mapping:

| Roadmap §5 | This proposal | Notes |
|---|---|---|
| `SessionCacheKey.principal` | Inside the digest | The principal namespace is hashed into S (§1.1). Two principals never share an id. |
| `SessionCacheKey.session` | S, the sequence digest | One Roundhouse `SessionId` is one sequence (roadmap addendum, K2 row). |
| `CacheBundleRef.generation` | Inside the digest, and `generation` in the message | **Generation fencing by construction.** Each generation is a new id, so a release of generation 1 cannot touch generation 2. The field is informational. |
| `SessionCacheKey.model_layout` | Not sent | **Gap.** The id does not name a model. One release covers S on every model of the deployment. That is safe, because S names one lineage, and a lineage on two models is two unrelated block sets. K2 still owns the field. |
| `SessionCacheKey.reuse_scope` | Not sent | **Gap.** Tenant and adapter scope stay inside Dynamo. Roundhouse's HMAC key is per deployment, so ids never collide across callers who do not share the key. |
| `CacheLifecycleHint::Cancel` | Release (asks 1 and 2) | **Closest, but not equal.** `Cancel` withdraws a pending archive or warm. A release asserts the content is dead. Roadmap §4.3 already separates "confirmed-dead content", which "follows the KVBM release/compaction rules". K2 should name this as `Cancel` with a dead flag or as a fourth variant. This proposal does not decide that. |
| `CacheLifecycleHint::Archive`, `Warm` | Not asked | Out of scope. A future `kv.archive` or `kv.warm` action could use the same `KvHint` envelope. |
| — | `successor` | **Gap in the roadmap.** §5 has no lineage edge. Roadmap §4.3 mentions "shared anchors or poison-lineage dependencies". `successor` is that edge for compaction. K2 should carry it. |
| "authenticated, advisory, idempotent, and safe to replay" | Bearer token, advisory, idempotent by `id` | Matches. |
| "acknowledgements distinguish accepted, rejected, superseded, and committed" | `202 {"accepted": n}` only | **Gap.** No committed event. A `released_blocks` count in the answer, or a KV event tagged with S, would close it. §6 asks. |
| "raw block hashes remain inside the cache/fleet boundary" | No hashes cross | Matches. The caller never sees or sends a block hash. |
| "Narwhal/KVBM may reject, delay, or partially satisfy a hint" | Advisory; Dynamo may ignore | Matches. |

## 5. Security and privacy

- **Opaque ids.** S is a keyed 16-byte HMAC in hex. Without the key, a label
  cannot be turned into S, and S reveals nothing about the conversation.
  Dynamo's request traces record session ids
  [session-ids.mdx:80]. With a keyed digest they record nothing readable.
- **No content.** No message carries a label, a prompt, a principal name, or
  a session header. `reason` and `detected_by` are fixed enums.
- **In-band authentication** is whatever the inference route has. A client
  that can send inference with S can already release S by sending a final
  request. That is acceptable only because a release is advisory and a
  128-bit id is not guessable.
- **Out-of-band authentication** must be stronger, because one call can name
  256 ids. The frontend admin API is on by default and `/busy_threshold` has
  no authentication
  [dynamo@6822bab lib/llm/src/http/service/service_v2.rs:805-809, :1293-1294].
  So the release route should require `Authorization: Bearer <token>`, with
  the token from deployment secret configuration, and stay unmounted when no
  token is set.
- **Roundhouse's side (Q28).** One token per deployment, from the existing
  secret configuration and never from the catalog. TLS for any address that
  is not loopback. Roundhouse refuses to load an `http://` URL that is not
  loopback (proposal §20.6).
- **Worst case of a stolen token:** an attacker releases warm KV for ids it
  has learned. The cost is prefill on the next turn. No request is refused,
  and no content leaks.
- **What a release reveals:** that some sequence was compacted or closed, and
  when. It does not reveal whose, or what.

## 6. Open questions for Dynamo maintainers

1. **Where should release live?** In the frontend lifecycle path, as a
   `KvHint` action such as `kv.release@1.0`, or in the KV Cache Controller
   work (#11673)? The `kv_hints.rs` comment about "KVCC names" suggests the
   last.
2. **Why were explicit SGLang sessions and the sticky lifecycle removed**
   in #10214? If the reason was the fragility of explicit open and close, a
   release-only hint with a no-op on unknown ids may avoid it. If the reason
   was something else, this proposal should know.
3. **Can the engines release by session id?** Does vLLM's `session_id`
   argument, SGLang's session radix cache, or TensorRT-LLM's
   `ConversationParams` support a release or an eviction-priority change? If
   not, is a router-only release (affinity, program table, prefix index) still
   worth shipping?
4. **Which reference count is authoritative** for "referenced only by S": the
   engine's block reference counts, or the router's `SessionPrefixIndexer`
   frontier counts?
5. **Should the final request short-circuit?** The Python prototype does. The
   native plugin dispatches. Is there a case where running it is wanted?
6. **Fan-out across frontends.** With several frontends and replica sync, how
   does one release reach the worker that holds S (Q29)?
7. **Hard affinity.** Should a final request for an id whose bound worker is
   gone succeed as a no-op? This proposal asks yes.
8. **Acknowledgement.** Would Dynamo report released block counts, or tag
   `Removed` KV events with the session that caused them?
9. **vLLM occupancy.** Does `kv_cache_usage` count evictable prefix-cached
   blocks as used? Can engines report an evictable count?
10. **A successor on the in-band path.** Is an `x-dynamo-successor-session-id`
    header worth adding, or is the endpoint the place for it?
11. **A canonical compaction marker.** `main` maps Codex turn metadata into
    `agent_context.compaction`. A router that forwards no client headers
    cannot supply it. Is an `x-dynamo-*` form wanted, or is release enough?

## 7. Ready-to-file issue body

```markdown
### Summary

Agent clients (Codex, Claude Code) compact or rewrite their history. After
that, the old conversation's KV is never reused, but Dynamo cannot know it:
it sees two unrelated session ids. A router in front of Dynamo can see the
compaction. We would like a way to say "session id S will not be reused",
and have Dynamo release or deprioritize the blocks that only S references.

Today `x-dynamo-session-final: true` is documented as the way to let
lifecycle-aware consumers release per-session state (session-ids.mdx). At
main (6822bab), its readers release scheduler state only. The pin-era
`kv_hints.evict_session` was removed in #13134. No engine receives
`session_final`. The native ThunderAgent plugin dispatches the final request
as inference, and session affinity refreshes the binding it should end.

### Contract (all asks)

- Advisory. Dynamo may free, deprioritize, or ignore.
- Shared-prefix safe: free only blocks no live session or in-flight request
  references.
- Never a refusal: a later request with S is ordinary work.
- Idempotent. Unknown ids are a no-op success.
- The caller sends a release only after the successor's first request has
  response headers.
- Ids are opaque 32-hex-character keyed digests. No content.

### Ask 1: release KV on `session_final`

On a request with `x-dynamo-session-final: true` for S:
1. Drop S's session-affinity binding, ThunderAgent program, and
   SessionPrefixIndexer entry.
2. Fan out to the workers holding S; each releases blocks referenced only
   by S, or moves them to the front of its eviction order. A `KvHint`
   action such as `kv.release@1.0` could carry this.
3. Do not run the request (no prefill, no decode).
4. Answer 200 with an empty completion, zero usage, and
   `x-dynamo-session-final-handled: true`.
Please also define "dedicated minimal request" in the docs: only `model`
matters; messages are ignored.

### Ask 2: `POST /v1/sequences/invalidate`

Out of band, batched (up to 256), bearer-authenticated, off unless a token
is configured:
{"version":1,"id":"<msg id>","sequence":"<S>","successor":"<S' or null>",
 "generation":2,"reason":"compaction|rewrite|closed","issued_at_ms":...}
202 {"accepted": n}; 401 bad token; 429/503 retry; 404 not mounted.
Idempotent by `id`. Needed because a superseded sequence has no next
request to carry a header, an inference request is admitted like inference
(and refused with 529 under overload), releases come in bursts, and the
successor id helps keep the shared prefix.

### Ask 3: mocker `cached_tokens` parity

#12711 fills `usage.prompt_tokens_details.cached_tokens` for the vLLM
scheduler model. The SGLang model in aisimulate-core 0.12.0 still reports
None. Please fill it, and add an HTTP-level test on chat, responses, and
messages.

### Ask 4: engine KV occupancy gauges

Export `kv_used_blocks` / `kv_total_blocks` from `WorkerLoadState` as
`dynamo_frontend_worker_kv_used_blocks` and
`dynamo_frontend_worker_kv_total_blocks` {worker_id, dp_rank, worker_type},
set in `update_from_active_load` and the runtime-config path, removed in
`cleanup_worker_metrics`. Open question: does vLLM count evictable
prefix-cached blocks as used?

### Tests we would expect

Mocker, two sessions sharing a prefix: a release of A yields `Removed`
events for A's tail only; B's `cached_tokens` is unchanged; the final
request reaches no engine; duplicates and unknown ids produce no events.

### Related

#11673 (KV Cache Controller), #13134, #13807, #14428, #14611, #15164,
#10214.
```
