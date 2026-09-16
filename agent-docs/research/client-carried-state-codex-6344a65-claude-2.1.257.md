<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# DIVE D1-3 — What the two clients carry themselves: what a stateless node can know from one request

**Read date: 2026-09-02.** Revisions this read is pinned to:

| Source | Pin |
|---|---|
| roundhouse | `7c5369a` ("M13: the Redis fair-use ledger"); the working tree has moved, so every roundhouse citation is `git -C /home/user/roundhouse show 7c5369a:<path>` |
| codex | `6344a65` (`/root/.cargo/git/checkouts/codex-9eee5d47a939c68c/6344a65`), paths below are relative to `codex-rs/` |
| Claude Code | 2.1.257, via the byte-exact captures at `crates/roundhouse-server/tests/fixtures/claude-2.1.257-*.json` (committed at `7c5369a`, pretty-printed, so line numbers below are real) |

Scope: the D1 state-spectrum question — **what a P0 proxy node could answer from
a single request with no durable log and no process memory**, and where each
client stops giving it enough. Read-only; no cargo was run.

**Method note on the two clients' asymmetry.** Claude Code is read from
captures because the binary cannot be re-run here; codex is read from source
because **this tree holds no byte-exact codex fixture** — established by
`git ls-tree -r --name-only 7c5369a -- crates/roundhouse-server/tests`, whose
`fixtures/` entries are all `claude-2.1.251-*` and `claude-2.1.257-*`. Every
codex claim below is therefore a source claim at `6344a65` and is marked as
such.

---

## 1. Does every turn carry the full history?

**Yes on both surfaces, and on both it is structural rather than incidental.**

### 1.1 Codex on Responses — no server-side handle exists to carry

- `ResponsesApiRequest` (`codex-api/src/common.rs:251-275`) has exactly these
  fields: `model`, `instructions`, `input`, `tools`, `tool_choice`,
  `parallel_tool_calls`, `reasoning`, `store`, `stream`, `stream_options`,
  `include`, `service_tier`, `prompt_cache_key`, `text`, `client_metadata`.
  **There is no `previous_response_id`** — the field exists only on the
  WebSocket variant `ResponseCreateWsRequest` (`codex-api/src/common.rs:307`)
  and is hard-wired `None` at the conversion (`codex-api/src/common.rs:282`).
- `store: false` is a literal at the single construction site
  (`core/src/client.rs:931`). Codex asks the server to persist nothing.
- `input` is the whole conversation: `build_responses_request` takes
  `prompt.get_formatted_input_for_request(...)` (`core/src/client.rs:853`),
  whose `Prompt::input` is documented "Conversation context input items"
  (`core/src/client_common.rs:20-21`) and is filled per attempt from
  `sess.clone_history().await.for_prompt(...)`
  (`core/src/session/turn.rs:1353-1359`).
- Item ids are actively **stripped** before send: `prepare_response_items_for_request`
  clears any id that is not prefixed (`core/src/client.rs:943-949`), so history
  items carry no stable server-side handle either.

**Consequence for P0.** A codex turn is self-contained. A node with no log can
serve it by forwarding `input` verbatim. What it cannot do without state is
*verify* the claim — see §5.

### 1.2 Claude Code on Messages — every prior turn is resent verbatim

From the three-turn `--continue` capture, `messages` grows by one item per turn
and every prior item is present:

| fixture | `messages` items |
|---|---|
| `claude-2.1.257-turn-1.json` | 2 — `user`(list, 2 blocks), `system`(list, `cache_control`) |
| `claude-2.1.257-turn-2-continue.json` | 5 — the two above, then `assistant`(list), `user`("and again", **bare string**), `system`(list, `cache_control`) |
| `claude-2.1.257-turn-3-continue.json` | 8 — all of turn 2's, with index 4's notice **flattened to a bare string**, then `assistant`, `user`("once more"), `system`(list, `cache_control`) |

The re-serialization rule (already recorded at
`agent-docs/research/claude-code-client-surface.md` §5.7.1 and re-verified here
against the committed fixtures): the item that carried the `cache_control`
breakpoint on turn *n* arrives on turn *n+1* as a bare string with no
`cache_control`, and a fresh trailing `system` notice carries the breakpoint
forward. The *text* is byte-stable; the *container* is not.

**This matters for a stateless proxy more than for a stateful one.** A prefix
check that compares containers rather than content forks every conversation on
turn 3. roundhouse compares role and content only
(`crates/roundhouse-server/src/responses_api.rs:899-900`, `same_item`).

**No server-side handle on this surface either.** Key-shaped searches over all
eight 2.1.257 fixtures — `grep -c '"<key>"'` for each of `prompt_cache_key`,
`previous_response_id`, `idempotency_key`, `Idempotency-Key`, `store`,
`conversation_id`, `thread_id`, `turn_id`, `parent_tool_use_id`, `agent_id` —
return **0 in every file for every key**.

---

## 2. What identifiers ride each request, and which are stable

### 2.1 Claude Code on `/v1/messages` (2.1.257 captures)

Twenty-one headers, identical set and order across both turns
(`claude-2.1.257-headers.json:4-25` and `:30-51`). The identity-bearing ones:

| carrier | value in the capture | stability |
|---|---|---|
| `x-claude-code-session-id` header | `c0cb70b6-…-1b8a60b7c4d8` (`claude-2.1.257-headers.json:8` and `:34`) | **stable across turns of a `--continue` chain**; identical on both turns of the capture, and identical to the `session_id` inside `metadata.user_id` |
| `metadata.user_id` (body) | a JSON-**string** holding an object: `{"device_id":…,"account_uuid":"","session_id":"c0cb70b6-…"}` (`claude-2.1.257-turn-1.json:722-724`, `claude-2.1.257-turn-3-continue.json:756-758`) | **stable**; byte-identical on turns 1 and 3 |
| `x-stainless-retry-count` | `"0"` on both turns (`claude-2.1.257-headers.json:13`, `:39`) | per-attempt (see §4) |
| `anthropic-beta` | drops `context-1m-2025-08-07` between turn 1 and turn 2 (`:17` vs `:43`) | **per-request, not stable** |
| `content-length` | 64358 → 64511 | per-request |
| `tool_use.id` / `tool_result.tool_use_id` | `toolu_mock_001` (`claude-2.1.257-mcp-turn-2-toolresult.json`, messages[2] and messages[3]) | per call; minted upstream, not by the client |

The MCP capture used a different CLI process and shows a different session uuid
(`54269458-…`, `claude-2.1.257-mcp-headers.json:8`, `:34`) — stable within that
process's two turns.

**Negatives, each by exhaustive search of all three header fixtures:**
`grep -c "x-claude-code-agent-id\|x-claude-code-parent-agent-id"` over
`claude-2.1.257-headers.json`, `claude-2.1.257-mcp-headers.json` and
`claude-2.1.257-mcp-wire.json` returns **0, 0, 0**. roundhouse *reads*
`x-claude-code-agent-id` (`crates/roundhouse-server/src/messages_api/wire.rs:78`,
consumed at `:237-241`) and uses it to give a subagent a sibling name
(`…/agent/{id}`, `wire.rs:286-291`), but **no capture in this tree exercises
that path**. The client-surface evidence's own reading is that in-process
subagents share the parent's session id
(`agent-docs/research/claude-code-client-surface.md` §4.3, "In-process subagents
share the parent's id").

### 2.2 Codex on `/v1/responses` (source at `6344a65`)

Codex sends **more identity than roundhouse currently reads**, in three places.

**(a) The body.** `prompt_cache_key` = `responses_metadata.session_id`
(`core/src/client.rs:484-488`, `:921`). The override path
(`with_prompt_cache_key_override`, `:476-482`) is used only for guardian review
sessions (`core/src/session/session.rs:1279-1281`).

**(b) Bare headers on the HTTP path**, added by the endpoint itself:

- `x-client-request-id` = the **thread id** (`codex-api/src/endpoint/responses.rs:88-90`)
- `session-id` and `thread-id` (`codex-api/src/endpoint/responses.rs:91`, via
  `build_session_headers`, `codex-api/src/requests/headers.rs:5-14`), filled
  from `responses_metadata.{session_id, thread_id}`
  (`core/src/client.rs:1193-1194`)
- `x-openai-subagent`, **only** for a `SessionSource::SubAgent`
  (`codex-api/src/endpoint/responses.rs:92-94`, `codex-api/src/requests/headers.rs:16-31`;
  values `review`, `compact`, `memory_consolidation`, `collab_spawn`, or a label)

**(c) `x-codex-turn-metadata`**, a JSON object header, attached unconditionally
on the HTTP path (`core/src/client.rs:1202-1205` inside `build_responses_options`,
→ `build_responses_compatibility_headers` `:761-776` →
`CodexResponsesMetadata::compatibility_headers` `core/src/responses_metadata.rs:313-341`).
No provider gate. Its payload (`CodexTurnMetadataPayload`,
`core/src/responses_metadata.rs:469-514`, filled at `:343-372`) carries, when
the turn has identity: `installation_id`, `session_id`, `thread_id`, `turn_id`,
`window_id`, `request_kind`, `forked_from_thread_id`, `parent_thread_id`,
`parent_turn_id`, `root_turn_id`, `subagent_kind`, `thread_source`, `sandbox`,
`sandbox_mode`, `auto_review_enabled`, `node_repl_*`, `workspaces`,
`turn_started_at_unix_ms`, `compaction`, plus a flattened `extra` map. The
header form omits `tool_namespaces_info` deliberately, to stay bounded
(`core/src/responses_metadata.rs:316-329`, comment: "Keep the unbounded tool
inventory in client_metadata only so HTTP and WebSocket compatibility headers
remain bounded").

`client_metadata` (`core/src/responses_metadata.rs:274-311`) carries the same
ids as *body* fields but only on the WebSocket transport
(`ResponseCreateWsRequest.client_metadata`, `codex-api/src/common.rs:328`); the
HTTP `ResponsesApiRequest` also has the field (`codex-api/src/common.rs:274`)
and it is populated at `core/src/client.rs:938`, so on HTTP it rides the body
too.

**Stability, and the root-vs-subagent split (R-M9's fact, re-verified at the pin):**

| id | scope | source |
|---|---|---|
| `session_id` (= `prompt_cache_key`) | **the whole agent family** — root and every subagent share it | `AgentControl`'s own comment, "every sub-agents from a common root share the same session ID" (`core/src/agent/control.rs:104-107`); taken by any non-root source at `core/src/session/session.rs:671-677` |
| `thread_id` | **per thread** — the root's and each subagent's differ | `TurnMetadataState::new(session_id, thread_id, …)` at `core/src/session/turn_context.rs:618-622`, emitted as `THREAD_ID_KEY` at `core/src/responses_metadata.rs:281` and `:356` |
| `turn_id` | **per turn**, optional | `core/src/responses_metadata.rs:284-286`, `:357-359` |
| `installation_id`, `window_id` | per install / per window | `:277-282` |
| `parent_thread_id`, `forked_from_thread_id`, `root_turn_id` | per family topology | `:293-304`, `:362-365` |

**What roundhouse reads today, and the gap.** `codex_thread_id`
(`crates/roundhouse-server/src/responses_api.rs:531-539`) parses
`x-codex-turn-metadata` and takes **`thread_id` and nothing else**. A
`git grep` at `7c5369a` over `crates/` for `"thread-id"`, `"session-id"`,
`x-openai-subagent`, `x-client-request-id`, `x-codex-parent-thread-id`,
`x-codex-window-id`, `x-codex-installation-id` returns exactly one hit, and it
is a test rig name (`crates/roundhouse-server/tests/codex_e2e.rs:1588`,
`Rig::start("thread-id")`). **None of those carriers is read.**

---

## 3. The MCP control surface: what a `tools/call` carries

### 3.1 Claude Code (byte-exact, `claude-2.1.257-mcp-wire.json`)

Five requests in order: `initialize` (id 0), `notifications/initialized`, a
`GET /mcp` SSE open, `tools/list` (id 1), `tools/call` (id 2). The `tools/call`
body in full (`:113-125`):

```json
{"method":"tools/call",
 "params":{"name":"status","arguments":{},
   "_meta":{"claudecode/toolUseId":"toolu_mock_001","progressToken":2}},
 "jsonrpc":"2.0","id":2}
```

Its headers (`:101-112`): `accept`, `accept-encoding: identity`,
`content-type`, `user-agent: claude-code/2.1.257 (sdk-cli)`,
`mcp-protocol-version: 2025-11-25`, `mcp-session-id` (the **stub server's**
value, echoed back), the deployment's own auth header
(`x-roundhouse-key`, `:108`), `connection`, `host`, `content-length`.

**The exhaustive negative that decides the P0 question for this client.** The
`tools/call` request carries **no** `x-claude-code-session-id`, **no**
`metadata.user_id`, **no** conversation name of any kind. Established by
reading the whole 127-line fixture: the only per-request identifiers on request
5 are the auth header, the server-issued `mcp-session-id`, and
`_meta["claudecode/toolUseId"]`.

- The `mcp-session-id` is useless to roundhouse, which **issues none**:
  `NeverSessionManager` + `legacy_session_mode = false`
  (`crates/roundhouse-mcp/src/transport.rs:345`, `:352`), documented at
  `:26-34` — "no `Mcp-Session-Id` is issued, no session state is held, and
  `GET /mcp` answers 405". Pinned by
  `crates/roundhouse-server/tests/mcp_surface.rs:968` and `:996`.
- So the tool-use id is the **only** correlator, and it is only a correlator
  against a table roundhouse wrote itself.

**That table is written at stream time, in process memory, on the serving node.**
`Conversations::bind_call` is called from exactly one non-test site: the
Messages follower, as it projects an emitted `ToolCall`
(`crates/roundhouse-server/src/messages_api/follower.rs:256-263`; the comment
at `:242-255` says why it is the only moment both halves are in one place).
`git grep bind_call 7c5369a -- crates` finds no other production caller.

**And the id is not self-describing.** On the Messages surface the
`tool_use.id` is minted upstream, not by roundhouse — the capture's
`toolu_mock_001` is the mock's string, and the call table's own doc names the
case where a local backend numbers calls `call_0`, `call_1` per response and
two of one principal's sessions collide
(`crates/roundhouse-server/src/conversations.rs:126-141`, resolved to
`CallSite::Ambiguous` at `:213-225`).

### 3.2 Codex (source at `6344a65`) — far more rides the call than we read

The model-driven MCP dispatch assembles `params._meta` in three layers, all in
`core/src/mcp_tool_call.rs`:

1. `build_mcp_tool_call_request_meta` (`:1175-1221`) inserts
   - `"callId"` — the model's own function-call id (`:1182-1185`)
   - **`"x-codex-turn-metadata"` — the whole turn-metadata object as a `_meta`
     value** (`:1194-1197`), built by
     `current_meta_value_for_mcp_request` (`core/src/turn_metadata.rs:183-222`),
     which is `turn_metadata_payload()` minus `tool_namespaces_info` (`:189`),
     minus `parent_turn_id` and `root_turn_id` (`:193-194`), plus `model` and
     `reasoning_effort` (`:195-209`). **It therefore still carries `session_id`,
     `thread_id`, `turn_id`, `window_id`, `installation_id`,
     `parent_thread_id`, `forked_from_thread_id`, `subagent_kind`.**
2. `with_mcp_tool_call_thread_id_meta` (`:1223-1245`) inserts `"threadId"` =
   `sess.thread_id`, called **unconditionally** at `:468-471`.
3. `augment_mcp_tool_request_meta_with_sandbox_state` (`:472-477`).

The same `threadId` insert exists on the app-server path
(`app-server/src/request_processors/mcp_processor.rs:496`, `:511-531`, key
constant at `:5`) — so both codex MCP entrypoints stamp it.

**What roundhouse reads.** `RoundhouseMcp::call_tool` builds
`Correlators { thread_id, tool_use_id }` from exactly two `_meta` keys:
`"threadId"` (`crates/roundhouse-mcp/src/transport.rs:150`, read at `:176-178`)
and `"claudecode/toolUseId"` (`:130`, `:166-168`), both via `meta_string`
(`:195-202`), assembled at `:299-302`. **`_meta["x-codex-turn-metadata"]` and
`_meta["callId"]` are read by nothing** — `git grep` at `7c5369a` for
`x-codex-turn-metadata` over `crates/` finds only the Responses-surface header
constant (`crates/roundhouse-server/src/responses_api.rs:491`) and its tests.

### 3.3 What a correct `status`/`prefer` answer needs on the server side

The resolver is one provided method,
`ControlReads::resolve_session` (`crates/roundhouse-mcp/src/reads.rs:240-291`),
whose order is: explicit `conversation` argument → `thread_id` (table first at
`:263`, then as a *name* at `:265`) → `tool_use_id` (`:272-275`) → `latest`
(`:286-289`), with a contradiction refusal at `:277-283`.

Of its four inputs, **exactly one is derivable from the request alone**:

| input | stateless? | why |
|---|---|---|
| `conversation` argument, resolved by `named_session` | **partly** — `Conversations::resolve` (`crates/roundhouse-server/src/conversations.rs:431-435`) refuses a key this node never bound; before M12.1 F9 it answered generation zero | needs the generation map |
| `thread_id` via `session_of_thread` (`conversations.rs:510-512`) | **no** — node-local `ThreadTable`, capped at 1024 per principal (`conversations.rs:305`), written only in the Responses `bind` (`responses_api.rs:482-485`) | |
| `tool_use_id` via `session_of_call` | **no** — node-local `CallTable`, capped at 4096 per principal (`conversations.rs:183`) | |
| `latest_session` (`conversations.rs:438-440`) | **no** — node-local `latest` map | |

**But the session id itself is derivable.** `bound_session`
(`crates/roundhouse-server/src/conversations.rs:533-538`) is
`SessionId::new(key)` at generation zero and `"{key}#g{n}"` above it, and `key`
is `plane.qualify(principal, cache_key)` (`responses_api.rs:584`). So:

> **For a never-forked conversation, the roundhouse session id is a pure
> function of (principal, client-supplied cache key).** The *only* per-conversation
> state a node needs to answer a codex control call exactly is the fork
> generation counter — and a codex `tools/call` already carries the cache key,
> in `_meta["x-codex-turn-metadata"].session_id`, which nothing reads.

Claude Code has no equivalent: its `tools/call` carries nothing from which the
Messages session key (`anthropic_messages/<session>` or
`anthropic_messages/<session>/agent/<id>`, `messages_api/wire.rs:286-291`) can
be rebuilt. Note the surface already records that a Claude model passing its own
session id in the `conversation` argument resolves to nothing, because the
argument is qualified through the Responses namespacing
(`crates/roundhouse-mcp/src/reads.rs:148-157`).

---

## 4. Cut-off responses and retries: no idempotency key on either side

### 4.1 Codex

- **No idempotency key on the inference path.** `grep -rn "Idempotency-Key\|idempotency"`
  over the whole `codex-rs` tree returns hits only in
  `app-server/src/request_processors/account_processor/rate_limit_resets.rs:49-73`
  and its tests — the rate-limit-reset-credit redemption flow, an account API,
  not `/v1/responses`.
- **A retry is identifier-identical.** The HTTP stream loop rebuilds the request
  from the same `prompt` and the same `responses_metadata` on every pass
  (`core/src/client.rs:1455` `loop {`, `:1481-1488` `build_responses_request`),
  so `prompt_cache_key`, `thread_id`, `turn_id`, `session_id` and `input` are
  the same bytes on attempt 2 as on attempt 1.
- **The one per-attempt marker is opt-in and off by default.**
  `x-codex-inference-call-id` (`rollout-trace/src/inference.rs:28`) is inserted
  at `:155-167` with a *fresh* uuid per attempt (`:130`) — an attempt
  discriminator, not a dedup key — and only when the thread trace is enabled,
  which requires the `CODEX_ROLLOUT_TRACE_ROOT` environment variable
  (`rollout-trace/src/thread.rs:44`, `:107`; disabled otherwise at `:363-364`).

### 4.2 Claude Code

- Key-shaped search over all eight 2.1.257 fixtures finds **no** `idempotency_key`
  or `Idempotency-Key`, in body or headers.
- The only retry signal is `x-stainless-retry-count`, `"0"` on both captured
  turns (`claude-2.1.257-headers.json:13`, `:39`) — the Stainless SDK's own
  attempt counter, not a dedup key. A retried request would carry `"1"` and
  otherwise the same bytes.

### 4.3 What roundhouse does instead — and it needs the log

Both clients' honest retries are handled by **content**, not by a key:

- `suffix_after` (`crates/roundhouse-server/src/responses_api.rs:884-891`)
  treats a `claimed` shorter than `stored` as the ordinary retry and yields an
  empty suffix; its doc at `:879-883` says "the turn id will deduplicate it onto
  the response that answer belongs to".
- The turn id is a hash of the whole canonicalized conversation
  (`responses_api.rs:844-848`; `messages_api/wire.rs:492`,
  `responses_api/wire.rs:199`), so a byte-identical retry dedups and a turn
  whose configuration moved is a new turn.
- Two further classes of provisional item exist precisely because a cut-off
  response leaves a hole in what the client resends: a partial committed by
  `mark_incomplete` (M11.1 F2) and an emitted tool call with no terminal at all
  (M11.2a F3) — both excluded from what a claim is checked against
  (`responses_api.rs:658-700`).

**All of that is a function of the stored log.** A P0 node with no log cannot
deduplicate a retry at all: it would re-dispatch and re-charge. That is the
sharpest cost of the P0 rung, and neither client offers a key that would let a
proxy avoid it.

---

## 5. Prefix admission, and what state it actually costs

Admission needs the stored conversation
(`admit`, `crates/roundhouse-server/src/responses_api.rs:862-875`;
`stored_conversation`, `:801-822`). Two node-local structures sit in front of it:

**`Conversations::generations`** — since M12.1 F9 it holds **one entry per
cache key this node has served**, not one per key that forked, because presence
is what distinguishes "never bound here" from "bound at generation zero"
(`conversations.rs:364-378`, rationale at `:366-373` and in the field doc at
`:79-100`). The module doc calls this out as the honest price of the F9 fix.

**The restart / duplicated-prefix hazard (M12.1 handoff (a)), confirmed in code.**
After a restart the generation map is empty, so `bind` re-derives generation
zero (`conversations.rs:374-375` with `bound_session`'s `0 => SessionId::new(key)`
at `:533-538`). A client whose history had already forked to `#g1` before the
restart disagrees with the generation-zero log, so `bind_prefix` forks
(`responses_api.rs:602-604`) — back onto `#g1`, whose log **already holds that
history**. The fork arm returns `Ok((session_id, claimed))` on the premise
stated at `:594-595` ("It gets a fresh internal session, which is empty and so
agrees trivially; no second check is needed") — false for a re-derived `#g1`.
The cost is a duplicated prefix, not a wrong session.

**Node-local correlation (handoff (c)), quantified.** `CallTable` 4096 entries
per principal (`conversations.rs:183`), `ThreadTable` 1024 per principal
(`:305`); both `HashMap`s behind one `Mutex` (`:66-68`) with no reaping of a
quiet principal (`:178-182`). A restart or a different node loses both, and a
lost entry costs one MCP call falling back — to the R-M7 named path for a
thread, to `latest` for a call.

---

## 6. What a promise needs and the clients will never send

Stated as negatives, each with what was searched.

1. **A per-thread / per-subagent marker on Claude Code's Messages wire.**
   `x-claude-code-agent-id` and `x-claude-code-parent-agent-id` appear in
   **zero** of the three 2.1.257 header/wire fixtures (searched by
   `grep -c` over each file). roundhouse reads the header
   (`messages_api/wire.rs:78`, `:237-241`) and would use it
   (`wire.rs:286-291`), but nothing in this tree has observed it. The
   client-surface read has in-process subagents inheriting the parent's session
   id (§4.3 of that document) — so today two of a Claude Code session's agents
   are one roundhouse session and their turns interleave.

2. **A settle-time cost, from either client.** Neither request shape has a
   usage or cost field: the Claude body's top-level keys across all five turn
   fixtures are exactly `context_management, max_tokens, messages, metadata,
   model, output_config, stream, system, thinking, tools`; codex's are the
   fifteen `ResponsesApiRequest` fields at `codex-api/src/common.rs:251-275`.
   Codex's own `turn.completed.usage` is a client-side event
   (`crates/roundhouse-server/tests/codex_e2e.rs:1217-1223` reads it from the
   client's stdout, not from a request). Every cost figure must be derived
   server-side from the upstream stream — which is a proxy-compatible
   obligation, but not a stateless one if it must be *accumulated* across turns.

3. **A conversation name on any MCP `tools/call` from Claude Code.** §3.1.

4. **An idempotency key, from either client, on any surface.** §4.

5. **A `previous_response_id` or `store: true` from codex.** §1.1 — so a
   server-side conversation store would be *ignored* by the client even if
   roundhouse offered one.

---

## 7. Summary table: what one request alone establishes

| question | Codex on Responses | Claude Code on Messages | Claude Code on `/mcp` |
|---|---|---|---|
| full history present? | yes (§1.1) | yes (§1.2) | n/a |
| conversation named? | yes — `prompt_cache_key` (body) | yes — header or `metadata.user_id` | **no** (§3.1) |
| thread / subagent distinguishable? | yes — `x-codex-turn-metadata.thread_id`, plus `thread-id`, `x-openai-subagent` headers (§2.2) | **no** (§6.1) | n/a |
| turn distinguishable? | yes — `turn_id` in the header payload | no | n/a |
| retry distinguishable? | only via opt-in `x-codex-inference-call-id` (§4.1) | only `x-stainless-retry-count` (§4.2) | n/a |
| session id derivable with no state? | **yes at generation zero** — `bound_session` is `SessionId::new(qualify(principal, cache_key))` (§3.3) | yes at generation zero, same rule, from header or `user_id` | **no** — needs the call table (§3.1) |
| what still needs state | fork generation; retry dedup (the log) | same | the call table, in full |

---

## 8. Open questions this read does not settle

1. **Does codex's `x-codex-turn-metadata` on the MCP `_meta` behave for a
   subagent the way the header does?** `current_meta_value_for_mcp_request`
   removes `parent_turn_id` and `root_turn_id`
   (`core/src/turn_metadata.rs:193-194`) but keeps `parent_thread_id` and
   `session_id`. Whether a subagent's MCP call carries the *family's*
   `session_id` (making it a usable cache key) is implied by
   `core/src/session/session.rs:671-677` but was not observed on a wire capture
   — this tree has no codex fixture.
2. **Which codex topologies set `x-openai-subagent`.** It covers
   `SessionSource::SubAgent` only (`codex-api/src/requests/headers.rs:16-31`);
   whether a multi-agent spawn that `is_non_root_agent()` accepts is always a
   `SubAgent` source was not traced.
3. **Whether `_meta["claudecode/toolUseId"]` is a versioned contract.** Named
   as open already in `claude-code-client-surface.md` §5.8; unchanged by this
   read.
4. **What Claude Code does when roundhouse answers `GET /mcp` with 405.** The
   capture's stub answered the GET and issued an `Mcp-Session-Id`
   (`claude-2.1.257-mcp-wire.json:59-74`, `:48`); roundhouse does neither
   (`crates/roundhouse-mcp/src/transport.rs:351-352`). The M12 closure test
   asserts the loop works against the real surface, but the client's behaviour
   on the 405 was not itself captured.
5. **Whether `metadata.user_id`'s `account_uuid` is ever non-empty.** It is `""`
   in every committed 2.1.257 fixture (§2.1) — an artefact of the cleared-env
   API-key capture topology, not established as a property of the client.

---

## Fact-check (2026-09-02)

An independent re-derivation of every negative and every high-stakes claim above, from the primary sources at the pinned revisions, by a second reader who did not write this document. Verdicts: 25 verified, 0 corrected, 0 unestablished.

Independently re-derived every claim in the batch (10 negatives, 7 high-stakes, 4 medium spot-checked) against primary sources at the pinned revisions (roundhouse 7c5369a, codex 6344a65, Claude Code 2.1.257 fixtures). All verified — exact or immaterially-adjacent line numbers throughout. No factual corrections needed. One methodological note on negative 5 (codex bare-header search): the draft's "exactly one hit" claim only reproduces when the search uses quoted string-literal tokens ("session-id", "thread-id", etc.); a bare substring grep false-hits on x-claude-code-session-id and similar compound headers. Full evidence at /tmp/claude-0/-home-user-roundhouse/d6addde3-2039-5f5e-8af5-d560d8c0b623/scratchpad/d1/what-the-clients-carry-factcheck.md.
