# roundhouse's `/mcp` behind agentic-api — a compatibility test

This example points [vLLM agentic-api](https://github.com/vllm-project/agentic-api)'s
MCP client at roundhouse's control surface and drives one scripted turn through
it. It is a **compatibility test, not a demonstration of the product**: it proves
our `/mcp` endpoint survives a second gateway's MCP client, its tool-name
flattening, and its bearer forwarding. It proves nothing about routing, because
in this topology roundhouse never sees the turn.

Read the last section before quoting any of this at anyone.

## The four topologies, and which one this is

| | The Responses turn goes to | The MCP client is | Model-visible tool name | Runs? | Do the control tools mean anything? |
|---|---|---|---|---|---|
| **A** | roundhouse | codex | namespace `mcp__roundhouse` + bare `status` | yes | **yes** — roundhouse owns the turn |
| **A′** | agentic-api | codex | `agentic_ns__mcp__roundhouse__status` | yes | no |
| **B** | agentic-api | agentic-api, from `~/.agentic-api/config.toml` | `mcp__roundhouse__status` | **no** — needs a request-side `type: "mcp"` tool, which codex never emits | (moot) |
| **C** | agentic-api | agentic-api, from the request body | `mcp__roundhouse__status` | yes, from a non-codex client | no |

**This example is C.** Configuration A is the one that carries the product
sentence, and it is tested against a real `codex` binary elsewhere in this
repository. C exists because it is the only way to put a *different*
implementation's MCP client in front of our server, and a surface that has only
ever been spoken to by one client is a surface with one client's bugs baked into
it.

## What is running

```
curl ──▶ agentic-api gateway :3000 ──▶ fixture-upstream.py :8000   (the "model")
                   │
                   └────────────────▶ mcp-proxy.py :8090 ──▶ roundhouse /mcp :8080
```

* **`fixture-upstream.py`** is the model. It answers `POST /v1/responses` with
  Responses-API SSE: on turn 1 a `function_call` naming whatever MCP tool
  agentic-api forwarded to it, on turn 2 a text answer quoting the tool output it
  got back. There is no GPU and no vLLM anywhere in this example.
* **`mcp-proxy.py`** sits between agentic-api and roundhouse and writes every
  JSON-RPC exchange to a capture file. It is there because the assertion the
  example has to make — *the bearer from the request body reached roundhouse* —
  is a claim about the wire, and roundhouse does not (and should not) log a
  caller's key. The proxy records `sha256(secret)`, which is the same form
  `control-plane.json` carries, so the check is hash-against-hash and no
  credential is written to disk.
* **`control-plane.json`** is what makes `/mcp` demand a key at all. Point
  roundhouse at no control plane and every caller resolves to the open
  principal — which would prove nothing about forwarding.
* **`request.json`** is the request. Note `"type": "mcp"` with `server_url`,
  `authorization`, and `require_approval: "never"` — the last is *mandatory* for
  a server the gateway did not configure itself, and `"never"` is the only value
  agentic-api accepts
  (`crates/agentic-server-core/src/tool/executors.rs:275-295` @ `e35fbb2`).
  `server_url` names the capture proxy on `:8090`, not roundhouse itself —
  against a bare roundhouse it is `:8080`, and `run.sh` rewrites the port anyway
  so the two never drift. Either way it is loopback, which agentic-api admits
  unconditionally, so no `AGENTIC_MCP_ALLOWED_HOSTS` entry is needed; a
  roundhouse on any other host would need one (`tool/mcp/pool.rs:164-210`).

`parallel_tool_calls: true` in the request is deliberate. At agentic-api's
previous pin that combination was **rejected** alongside a built-in tool; PR #197
made the gateway force `parallel_tool_calls = false` upstream instead
(`types/request_response.rs:125-131` @ `e35fbb2`). The run asserts the forced
`false` arrives at the fixture, so the dependency on that fix is demonstrated
rather than asserted.

## Running it

agentic-api pins `channel = "1.98.0"` and this repository pins `1.96.1`, so it is
built separately, out of this tree, with its own target directory:

```bash
git clone https://github.com/vllm-project/agentic-api /tmp/agentic-api
cd /tmp/agentic-api && git checkout e35fbb2
CARGO_TARGET_DIR=/tmp/agentic-target rustup run 1.98.0 cargo build -p agentic-server
```

Then, from anywhere:

```bash
export AGENTIC_BIN=/tmp/agentic-target/debug      # holds `agentic` and `agentic-server`
bash examples/agentic-api-mcp/run.sh
```

`run.sh` builds `roundhouse-server` itself unless `ROUNDHOUSE_BIN` is set, starts
the four processes, POSTs the request, prints the SSE stream, checks nine
assertions, and tears everything down on exit. Ports are overridable
(`RH_PORT`, `PROXY_PORT`, `UPSTREAM_PORT`, `GATEWAY_PORT`); the whole transcript
is left in `$WORK` (a temp dir by default) as `mcp-capture.jsonl`,
`upstream-capture.jsonl`, `sse.txt`, `roundhouse.log`, `agentic.log`.

Exit status is 0 only if every assertion passed.

## What a passing run proves

1. agentic-api's **rmcp 1.8.0** client completes `initialize` → `tools/list` →
   `tools/call` against roundhouse's **rmcp 3.1.3** server, both settling on
   protocol version `2025-06-18`.
2. The request's `authorization` field arrives on the MCP leg as
   `Authorization: Bearer <key>` on *every* request in the session, and it is the
   key the control plane knows.
3. `tools/list` answers with all eight of our tools; the five the request's
   `allowed_tools` admits are forwarded to the model as `mcp__roundhouse__<tool>`
   — unhashed, unsanitized, no collisions, every name under the 64-character cap.
4. A tool result — including an **error** result — is fed back into the model's
   context as an ordinary tool output, and the model's next turn can quote it.

## What it does not prove — read this part

**The control tools have no session to answer about, and the run shows exactly
that.** `status` answers:

```
this key has no conversation yet; start a turn before asking about one
```

That is correct behaviour, not a bug. Every session-scoped tool resolves a
conversation from the caller's own `prompt_cache_key`, and in configuration C the
Responses turn went to agentic-api — roundhouse never routed it, so there is no
log to read. The gateway renders the refusal as an `mcp_call` with
`status: "failed"` and an `mcp_tool_execution_error`, feeds
`{"error": "..."}` back to the model, and the turn completes normally.

Also worth being plain about:

* **The client is a `curl` script**, not a coding agent. Codex cannot produce
  this request: its `ToolSpec` has no `mcp` arm (codex `6344a65`, carried from
  the round-3 dive — see `agent-docs/research/agentic-api-mcp-compat.md`), which
  is why row B is unreachable through codex and row C needs a non-codex client.
* **The "model" is a fixture** that always calls the tool. Nothing here is
  evidence that a real model would choose to.
* **`read_only` is reported as `false` for `status`.** agentic-api derives it
  from the tool's `read_only_hint` annotation and defaults to `false` when there
  is none (`tool/mcp/handler.rs:248-262` @ `e35fbb2`); this branch publishes no
  annotations. Downstream of M9, which does, the same run reports `true`.
