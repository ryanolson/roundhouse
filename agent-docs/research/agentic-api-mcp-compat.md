# agentic-api ↔ roundhouse `/mcp`: the compatibility test, run

> **Status:** run and green, 2026-08-21.
> **Revisions:** agentic-api `e35fbb294ecb6bf2d4d7367236f9e454bcee928a` (HEAD,
> "fix: serialize unsupported parallel tool calls (#197)"); roundhouse
> `1daf8d5` (M8 merge, PR #5) plus the example directory this document reports
> on; rustc `1.98.0 (88d9e12ae 2026-08-18)` for agentic-api, `1.96.1` for
> roundhouse. No GPU, no vLLM, no Python beyond the standard library.
> **Artifacts:** `examples/agentic-api-mcp/` — `run.sh` reproduces everything
> below and exits non-zero if any assertion regresses.
> **Ruling this discharges:** the round-3 addendum at
> `../synergies/ecosystem-round-2.md:241` @ `fe73e5f` (branch
> `claude/synergy-round-3`, unmerged when this was written — the round-2 text on
> this branch still says the opposite) — *"the agentic-api leg survives as a
> compatibility test, not a first proof … the write-up must say plainly that
> the client is a script and the tools answer about a turn roundhouse did not
> route."*

## What was run

Configuration C of the round-3 topology table: a scripted `curl` client sends
agentic-api a Responses request declaring roundhouse's `/mcp` as a request-side
`type: "mcp"` tool. agentic-api is the MCP client; roundhouse is only an MCP
server; the Responses turn never touches roundhouse.

```
curl ──▶ agentic-api :3000 ──▶ fixture-upstream.py :8000        (stands in for the model)
               │
               └────────────▶ mcp-proxy.py :8090 ──▶ roundhouse /mcp :8080
```

Four processes, all loopback, all bounded by `timeout`, torn down by an EXIT
trap. `agentic serve --upstream http://127.0.0.1:8000 --skip-llm-ready-check`
per the CLI added in their PR #188 (`crates/agentic-server/src/bin/agentic.rs`
@ `e35fbb2`). The upstream fixture is a 305-line stdlib HTTP server that answers
`POST /v1/responses` with Responses-API SSE — a `function_call` on turn 1, a text
answer quoting the tool output on turn 2. agentic-api's own
`scripts/container-fixture-server.py` could not be used: it 422s unless the last
input item is exactly the one prompt it was recorded against, `model == "gpt-4o"`,
and `stream is False` (`scripts/container-fixture-server.py:43-58` @ `e35fbb2`).

The reverse proxy exists because the load-bearing claim — *the bearer in the
request body reached roundhouse* — is a claim about the wire, and roundhouse does
not log a caller's key. It records `sha256(secret)`, the same form
`control-plane.json` carries, so the assertion is hash-against-hash and no
credential lands in the transcript.

Build times on this box, both cold: agentic-api `1m 14s`, roundhouse `1m 01s`.

### Assertions, all green

| | Assertion | Result |
|---|---|---|
| i.a | roundhouse `/mcp` received `initialize` | PASS |
| i.b | … received `tools/list` | PASS |
| i.c | … received `tools/call` for `status` | PASS |
| i.d | every `/mcp` request carried the configured turn key | PASS — 4/4 |
| i.e | `tools/list` answered with roundhouse's control tools | PASS — all 8 |
| ii.a | agentic-api forwarded our tools as `mcp__<label>__<tool>` | PASS |
| ii.b | the gateway forced `parallel_tool_calls=false` upstream | PASS |
| iii.a | the client's final text carries the tool's own output | PASS |
| iii.b | the gateway answered `200`, `curl` exited `0` | PASS |

## The transcript

Captured by the proxy; secrets replaced by their length and hash, which is how
they were recorded in the first place.

**`initialize` — rmcp 1.8.0 client, rmcp 3.1.3 server, one protocol version.**

```
POST /mcp   authorization={"scheme":"Bearer","secret_len":51,"secret_prefix":"rh_turn_",
                           "secret_sha256":"e37f9a0cabf9…2804"}
→ {"jsonrpc":"2.0","id":0,"method":"initialize","params":{
     "capabilities":{},"clientInfo":{"name":"agentic-api","version":"0.3.0"},
     "protocolVersion":"2025-06-18"}}
← 200 application/json
  {"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-06-18",
     "capabilities":{"tools":{}},
     "serverInfo":{"name":"roundhouse","version":"0.1.0"},
     "instructions":"Roundhouse routes this conversation between local and hosted models. …"}}
```

Then `notifications/initialized` (answered `202`, no body), `tools/list`, and:

```
→ {"jsonrpc":"2.0","id":2,"method":"tools/call",
   "params":{"_meta":{"progressToken":1},"arguments":{},"name":"status"}}
← {"jsonrpc":"2.0","id":2,"result":{
     "content":[{"type":"text",
       "text":"this key has no conversation yet; start a turn before asking about one"}],
     "isError":true}}
```

Every one of the four requests carried
`Authorization: Bearer <51 chars, sha256 e37f9a0cabf9…2804>`, matching
`control-plane.json`'s `key_sha256` exactly.

**What the model saw.** The fixture captured agentic-api's upstream request; the
`tools` array it forwards is the evidence for the flattening claim, not a
constant in the fixture:

```json
["mcp__roundhouse__status", "mcp__roundhouse__declare_intent", "mcp__roundhouse__prefer",
 "mcp__roundhouse__set_quality_floor", "mcp__roundhouse__explain_last_route"]
```

Five, not eight, because the request's `allowed_tools` is a filter over the
discovered set (`tool/executors.rs:250-262` @ `e35fbb2`); `tools/list` itself
returned all eight. `parallel_tool_calls` arrived as `false` although the client
sent `true` — see §"The #197 dependency".

**Round two's input**, as the gateway rebuilt it:

```json
[{"type":"message","role":"user","content":"Ask roundhouse what it is routing me to."},
 {"type":"function_call","id":"fc_fixture_1","call_id":"call_fixture_1",
  "name":"mcp__roundhouse__status","arguments":"{}","status":"completed"},
 {"type":"function_call_output","call_id":"call_fixture_1",
  "output":"{\"error\":\"this key has no conversation yet; start a turn before asking about one\"}"}]
```

**What the client got**, trimmed to the three items that matter:

```
response.output_item.done  item.type=mcp_list_tools   server_label=roundhouse  (5 tools)
response.output_item.done  item.type=mcp_call         name=status  status=failed
    error={"type":"mcp_tool_execution_error",
           "content":[{"type":"text","text":"this key has no conversation yet; …"}]}
response.output_text.done  text=ROUNDHOUSE_STATUS={"error":"this key has no conversation yet; …"}
response.completed         status=completed   HTTP 200
```

## What this proves

**1. rmcp 1.8.0 client against rmcp 3.1.3 server, through a second gateway.**
agentic-api pins `rmcp 1.8.0` (`Cargo.lock:2368` @ `e35fbb2`); roundhouse's
server is `rmcp 3.1.3`. Both offered `2025-06-18` — the version
`crates/roundhouse-mcp/src/transport.rs:140-144` pins the semantics to — so the
negotiation never had to reconcile anything. This was the riskiest unknown going
in and it is now retired with a transcript rather than an argument.

A second fact, and the reason the pairing had so little to reconcile:
roundhouse's transport is configured stateless —
`config.legacy_session_mode = false` and a `NeverSessionManager`
(`crates/roundhouse-mcp/src/transport.rs:236-254`), which is what makes `GET /mcp`
a `405` and issues no `Mcp-Session-Id`. The run bears that out: the server log
shows a freshly initialized service per POST finishing `quit_reason=Closed`, and
no `mcp-session-id` header appeared in either direction in any of the four
exchanges. The 1.8 client neither sent one nor minded its absence, so nothing
session-shaped — the part of the streamable-HTTP spec the two rmcp majors are
most likely to disagree about — was exercised in this run.

**2. Bearer forwarding works, and it is the request's field that does it.**
`authorization` on the request-side `mcp` tool becomes
`Authorization: Bearer <value>` on the MCP leg (`tool/mcp/pool.rs:156-162` @
`e35fbb2`), which is exactly what roundhouse's `/mcp` gate reads
(`crates/roundhouse-server/src/mcp_api.rs:356-372`). All four requests of the
session carried it, including the `notifications/initialized` notification.

**3. Tool-name flattening is lossless for our names.**
`internal_mcp_tool_name` builds `mcp__{server_label}__{tool}`, sanitizes
non-`[A-Za-z0-9_-]` to `_`, and on overflow past 64 characters or on collision
truncates and appends `__{10 hex}` of an FNV-1a hash
(`tool/mcp/handler.rs:380-404` @ `e35fbb2`). Our longest name flattens to
`mcp__roundhouse__explain_last_route`, 35 characters, all `[a-z_]`: no hashing,
no sanitization, no collisions. The identity survives back the other way —
`mcp_call` carries `{server_label, name}` — so the model's call reaches
roundhouse as bare `status`, which is what the `tools/call` capture shows.

**4. `require_approval: "never"` is what governs approval, and it is mandatory.**
Verified in source rather than assumed:
`validate_mcp_execution_options` rejects any value other than `"never"` with
*"approval gating is not yet supported"*, and rejects an **omitted** value for a
server the gateway did not configure
(`tool/executors.rs:275-295` @ `e35fbb2`). Nothing in that path consults the
tool's annotations; `read_only_hint` is read in exactly one place, and only to
populate a field of the `mcp_list_tools` output item
(`tool/mcp/handler.rs:248-262`). So a tool carrying `annotations: None` — which
is every tool on this branch — is dispatched identically to an annotated one.

**5. An error result round-trips as data, not as an abort.** roundhouse renders a
`SurfaceError` as a `CallToolResult` with `isError: true` rather than a JSON-RPC
error (`crates/roundhouse-mcp/src/transport.rs:127-134`). agentic-api turns that
into `Err(ToolError::Execution(text))` (`tool/mcp/handler.rs:342-364`), which
`execute_gateway_call_with_timeout` converts into
`{"error": "<text>"}` fed back as an ordinary `function_call_output` with
`GatewayCallStatus::Failed` (`executor/gateway.rs:140-146,197-204`). The loop
continues; the turn completes `200`.

## What this does not prove

**The control tools have no session to answer about.** `status` answered:

> this key has no conversation yet; start a turn before asking about one

That is the correct answer and the reason this leg is a compatibility test rather
than a first proof. Every session-scoped tool resolves a conversation from the
caller's own `prompt_cache_key` — the argument is documented that way on all six
tools that take it (`crates/roundhouse-mcp/src/tools.rs:80-90`, one function so the eight spellings cannot drift) —
and `ControlReads::resolve_session` answers `SurfaceError::NoSession` for a
principal with no session at all rather than inventing one
(`crates/roundhouse-mcp/src/reads.rs:62-66`). In configuration C the Responses
turn went to agentic-api. roundhouse never routed it, so there is no log to read
and nothing to report. A `status` that answered anyway would be worse.

**The client is a script.** `curl` posting a JSON file. Codex cannot produce this
request: its `ToolSpec` is a closed five-arm enum with no `mcp` arm
(`codex-rs/tools/src/tool_spec.rs:21-56` @ codex `6344a65`, byte-identical at
`e363b08`) — carried from the round-3 dive rather than re-derived here, and stale
the day either codex pin moves. That is why configuration B is unreachable
through codex and C needs a non-codex client.

**The model is a fixture** that always calls the tool. Nothing here says a real
model would choose to.

**`read_only` reads `false` for a read-only tool.** agentic-api publishes
`annotations: {"read_only": <read_only_hint ?? false>}` on each discovered tool
(`tool/mcp/handler.rs:248-262` @ `e35fbb2`), and this branch's `/mcp` publishes
`annotations: None` — visible in the server log's `ListToolsResult`. So the model
is told `status` may have side effects, when its description says it "changes
nothing". Not a defect on either side and not worth a production change here: M9
adds `read_only_hint: true` to `status` and publishes the annotations
(`crates/roundhouse-mcp/src/tools.rs:148`,
`crates/roundhouse-mcp/src/transport.rs:105-114` @ `dbfd4fd`, branch
`claude/m9-codex-e2e`), so the same run downstream of that merge reports
`true`. Recorded because it is the
one observable difference this branch's `/mcp` has from the one that will ship,
and re-running the example is how anyone confirms the fix.

## The #197 dependency, demonstrated rather than asserted

`request.json` sends `parallel_tool_calls: true` alongside a built-in tool on
purpose. At agentic-api's round-2 pin (`d59d4b4`) that combination was rejected
outright — *"parallel_tool_calls must be false when using built-in tools"*. PRs
#191 → #194 (revert) → #197 landed the answer: `to_upstream_request` now sets
`parallel_tool_calls = Some(false)` unconditionally and never errors
(`types/request_response.rs:125-131` @ `e35fbb2`), with a startup warning —
observed in the run's `agentic.log` — reading *"parallel tool calls are not
supported; requests are serialized by the gateway"*. The fixture captured
`parallel_tool_calls: false` in the body it received, and `run.sh` asserts it. **A
run of this example against the pinned revision would have 400'd.**

## The contribution item, still standing

`[mcp_servers.*].headers` in agentic-api's `~/.agentic-api/config.toml` takes a
**literal** bearer with no environment indirection
(`tool/mcp/pool.rs:15-40` @ `e35fbb2`), while the same file's `web_search` entry
gets `api_key_env` and the generated file's banner claims *"Secret values are
read from referenced environment variables and are not written here"*
(`crates/agentic-server/src/config_file.rs:16,98`). Configuration B — the
gateway-configured path — therefore cannot be deployed with a real key without
writing it to disk.

This run does not exercise that path (it uses the request-side declaration, where
the credential is in the request body and never persisted), but it does
strengthen the case: the run demonstrates that a request-declared bearer reaches
roundhouse intact, so the only thing standing between configuration B and a
working deployment is where the key is allowed to live. `bearer_token_env_var` /
`env_headers` on `McpServerEntry::Http`, with codex's own `mcp_types.rs:511-529`
as the precedent, remains a smaller and better first upstream PR than the rustls
feature.

## Two ceilings to carry forward

**The toolchain.** agentic-api's `rust-toolchain.toml` pins `channel = "1.98.0"`;
roundhouse pins `1.96.1`. The published `agentic-server-core` carries no
`rust-version`, so cargo would not warn — a dependency line would simply fail to
build. This example sidesteps it by building agentic-api out of tree with
`rustup run 1.98.0` and its own `CARGO_TARGET_DIR`, which is also why `run.sh`
takes `AGENTIC_BIN` rather than deriving anything. Per the unlock-condition rule,
the condition that frees a future dependency line is roundhouse moving to 1.98.0
or later, not agentic-api relaxing anything.

**The revision.** Everything above is `e35fbb2`. `#197` is one commit old at the
time of writing and the API it changed had been rewritten twice in three days
(#191, #194). A re-run is the cheapest possible re-verification, which is the
reason `run.sh` asserts rather than prints.
