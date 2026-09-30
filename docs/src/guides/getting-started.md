# Getting started

This chapter tells you how to build Roundhouse, start the server, and send a first turn. It then points you to the guide for your agent.

## Prerequisites

- The Rust toolchain that `rust-toolchain.toml` pins (1.96.1). `rustup` selects it in this repository.
- The system `libzmq` development package. `dynamo-kv-router` needs it through its `standalone-selection` feature.
- Network access to GitHub. The first build clones `ai-dynamo/dynamo` to resolve the pinned Dynamo crates, and it is slow.

```bash
apt-get install -y libzmq3-dev   # Debian or Ubuntu
brew install zeromq              # macOS
```

## Build and test

1. Build the server, `roundhouse`:

   ```bash
   cargo build -p roundhouse-server
   ```

2. Build the operator launcher, `topham`:

   ```bash
   cargo build -p topham
   ```

3. Run the workspace tests under a time limit:

   ```bash
   timeout 900 cargo test --workspace
   ```

Always use `timeout`. A hung test stops the whole cargo run without a message, and a bounded run exits 124 instead. The default run needs no GPU, worker process, or network. See [Testing](../development/testing.md) for the opt-in suites.

## Start the server

```bash
cargo run -p roundhouse-server --bin roundhouse
```

The server listens on `127.0.0.1:8080`. With no other configuration, it runs in offline mode:

- An echo stub answers every turn with the text `frontier answer`. No request leaves the process.
- The catalog has one free `echo/echo` model, so every price on the dashboard is zero.
- No control plane is loaded. Every request is served as the built-in `default/default` membership, with no key. The admin plane refuses every request.
- Sessions, spend, and tenancy live in process memory and are lost when the process stops.

The boot log states each of these facts with the name of the variable that changes it.

## Send a first turn

Send one Anthropic Messages request:

```bash
curl -s http://127.0.0.1:8080/v1/messages \
  -H 'content-type: application/json' \
  -d '{"model":"any","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}'
```

The reply is a Messages response with the text `frontier answer`. A request with no session header gets a new anonymous session. See [Sessions and the event log](../concepts/sessions.md#naming-a-conversation).

To see the event log, use the native surface. Create a session and keep the `session_id` from the reply:

```bash
curl -s -X POST http://127.0.0.1:8080/v1/sessions
```

Send a turn to that session. The reply is a server-sent event stream of the log:

```bash
curl -s -N http://127.0.0.1:8080/v1/sessions/SESSION_ID/responses \
  -H 'content-type: application/json' \
  -d '{"turn_id":"t1","input":[{"role":"user","text":"hello"}]}'
```

The stream shows `session_created`, `turn_started`, `item_appended`, and `routed`, then the output events. The `routed` event holds the routing decision: the chosen target, each considered candidate, and the rationale.

To replay the log later, open `GET /v1/sessions/SESSION_ID/events`, with `?starting_after=N` to start after sequence number `N`. The dashboard is at `http://127.0.0.1:8080/v1/metrics/dashboard`. See [Metrics and the dashboard](../operations/metrics.md).

## Configure the server

The server reads its configuration from environment variables only. It has no command-line flags. A first run needs these:

| Variable | Default | Effect |
|---|---|---|
| `ROUNDHOUSE_ADDR` | `127.0.0.1:8080` | The `host:port` to bind. |
| `ROUNDHOUSE_CATALOG` | the echo catalog | Path to the catalog JSON: hosted models, prices, and providers. |
| `ROUNDHOUSE_FRONTIER_UPSTREAM` | echo stub | `openai_responses` sends turns to real providers. Any other value stops the boot. |
| `ROUNDHOUSE_CONTROL_PLANE` | open plane | Path to the control plane JSON: projects, users, keys, and policy. |
| `ROUNDHOUSE_REDIS_URL` | process memory | A `redis://` URL for all durable state. |

A file variable that is set but cannot be read stops the process, and so does an unreachable Redis URL. A fallback would serve every turn under prices, tenancy, or storage that nobody chose. `RUST_LOG` sets the log filter and defaults to `info`. The [Configuration reference](../appendix/configuration.md) lists every variable.

## Add a catalog and real providers

1. Copy `examples/catalog.example.json`. Replace each placeholder model name and price with the current published values.
2. Start the server with `ROUNDHOUSE_CATALOG` set to your file. The echo stub still answers, and the router chooses among your catalog models and prices them. This is a priced dry run.
3. Put each provider key in the variable that the provider's `auth.env` names, for example `OPENROUTER_API_KEY`.
4. Set `ROUNDHOUSE_FRONTIER_UPSTREAM=openai_responses` and restart the server.

The boot refuses a catalog model whose provider has no definition, except the built-in `openai`. It also refuses a provider whose models speak two dialects. See [Configure providers and the catalog](catalog.md).

## Add a control plane

Without a control plane, any client can use the server with no key. For a shared deployment, load one. See [Configure tenancy and keys](tenancy.md). The shipped `examples/control-plane.example.json` assumes a local Dynamo fleet. The `roundhouse` binary attaches none, so the boot refuses that file until you remove its local promises.

## Connect an agent

Read [Hook up Codex](codex.md) or [Hook up Claude Code](claude-code.md). An operator usually starts from a saved profile with [Launch with topham](topham.md). For durable state across restarts and nodes, read [Deploy with Redis](../operations/redis.md).
