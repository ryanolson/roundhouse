<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Roundhouse

Roundhouse is a stateful front-end for agentic coding clients. It sits in front of [Dynamo](https://github.com/ai-dynamo/dynamo).

An unmodified coding agent, such as Codex or Claude Code, connects to Roundhouse. Through it, the agent reaches models that Dynamo serves locally and the public endpoints of frontier labs. Roundhouse routes each turn on quality, cost, and time to solution together. The agent's own stack does not change.

**Documentation:** https://ryanolson.github.io/roundhouse/ (source in [`docs/`](docs/src/SUMMARY.md)).

## Why

A coding agent sends its full conversation again on every turn. With 100k tokens of context and hundreds of turns, that resend is the main cost of agentic work: bytes on the wire, prefill compute, and money.

Roundhouse keeps the conversation in a durable, append-only log per session. It admits only the new part of each request. Because it knows the exact token prefix, it can also ask which engine already holds that prefix in its KV cache, and route the turn there. State is what makes the routing possible.

## What it does

- Serves the OpenAI Responses API (`/v1/responses`) for Codex and the Anthropic Messages API (`/v1/messages`) for Claude Code, with pass-through auth.
- Keeps one append-only event log per session, in memory or in Redis, with a fenced single-writer lease.
- Routes each turn between local Dynamo workers (through Dynamo's embedded selection service) and frontier providers, on one cache-adjusted cost axis. The shipped binary attaches no local fleet yet; local routing runs in the library and its tests.
- Enforces per-project and per-key policy, budgets, and fair-use windows.
- Exposes a control surface to the agent as an MCP server at `/mcp`.
- Reports cost, savings, and latency on `/v1/metrics` and a dashboard, and emits NeMo Relay's ATOF and ATIF formats from the same log.
- Launches Codex or Claude Code against it with `topham`, from a saved profile.
- Checks agent turns with a judge and steers the agent with text when a turn goes wrong. The judge sends no credential yet, so on a real provider every check is skipped.

The [limitations](docs/src/appendix/limitations.md) chapter lists what it does not do yet.

## Quick start

Requirements: the Rust toolchain in `rust-toolchain.toml` and the system ZeroMQ library.

```bash
apt-get install -y libzmq3-dev
timeout 900 cargo test --workspace
```

The first build clones `ai-dynamo/dynamo` to resolve the pinned Dynamo crates. The default test run needs no GPU, no network, and no worker processes.

Run the server offline. An echo stub answers every turn, and no request leaves the process:

```bash
cargo run -p roundhouse-server --bin roundhouse
```

It listens on `127.0.0.1:8080` (`ROUNDHOUSE_ADDR` changes it). [Getting started](docs/src/guides/getting-started.md) sends a first turn, adds real providers, and points Codex or Claude Code at it.

## Workspace

| Crate | Contents |
|---|---|
| `roundhouse-core` | Session state machine, event log, lease, routing, control vocabulary, validate and steer, metrics |
| `roundhouse-fleet` | Local Dynamo fleet (embedded selection service) and frontier provider clients |
| `roundhouse-mcp` | The control surface as an MCP server |
| `roundhouse-relay` | NeMo Relay's published formats, produced from the session log |
| `roundhouse-sequence-id` | Session and sequence identity read from client requests |
| `roundhouse-store-redis` | Redis Streams session store and spend ledger |
| `roundhouse-server` | Turn engine, HTTP surfaces, admin plane, and the binary |
| `topham` | Operator entry point: profiles, `plan`, `launch`, `relay`, `mint` |

See [Workspace crates](docs/src/development/architecture.md) for the dependency graph.

## Documentation

The book in `docs/` is the documentation. Build it and check its links with:

```bash
bash scripts/build-book.sh
```

## License

Apache-2.0. See [LICENSE](LICENSE).
