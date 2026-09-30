# Getting started

This chapter tells you how to build Roundhouse, start the server, and send a first turn. It then points you to the guide for your agent.

## Prerequisites

- The Rust toolchain that `rust-toolchain.toml` pins (1.96.1). `rustup` selects it automatically in this repository.
- The system `libzmq` development package. `dynamo-kv-router` needs it through its `standalone-selection` feature.
- Network access to GitHub for the first build. The build clones `ai-dynamo/dynamo` to resolve the pinned Dynamo crates.

Install `libzmq`:

```bash
apt-get install -y libzmq3-dev   # Debian or Ubuntu
brew install zeromq              # macOS
```

The first build takes a long time because of the Dynamo clone. Later builds use the cached checkout.

## Build and test

1. Build the server binary, `roundhouse`:

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

Always use `timeout` for a test run. A hung test stops the whole cargo run without a message. A bounded run changes "stalled for hours" into `exit 124` in minutes. If the run exits with 124, suspect the newest test or the newest change.

The default test run needs no GPU, no worker process, and no network. The selection service runs inside the test binary. Redis suites and real-client suites are opt-in. See [Testing](../development/testing.md).

## Start the server

Start the server with no configuration:

```bash
cargo run -p roundhouse-server --bin roundhouse
```

The server listens on `127.0.0.1:8080`. To use a different address, set `ROUNDHOUSE_ADDR`, for example `ROUNDHOUSE_ADDR=0.0.0.0:8080`.

With no configuration, the server runs in offline mode:

- An echo stub answers every turn with the text `frontier answer`. No request leaves the process.
- The catalog has one free `echo/echo` model, so every price on the dashboard is zero.
- No control plane is loaded. Every request is served as the built-in `default/default` membership, with no key. The admin plane refuses every request.
- Sessions, spend, and tenancy live in process memory and are lost when the process stops.

The boot log says each of these facts, with the name of the variable that changes it.

## Send a first turn

Send one Anthropic Messages request:

```bash
curl -s http://127.0.0.1:8080/v1/messages \
  -H 'content-type: application/json' \
  -d '{"model":"any","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}'
```

The reply is a Messages response with the text `frontier answer`. A request with no session header gets a new anonymous session. See [Sessions and the event log](../concepts/sessions.md#naming-a-conversation).

To see the event log, use the native surface. Create a session, and keep the `session_id` from the reply:

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

To replay the log later, open `GET /v1/sessions/SESSION_ID/events`. Add `?starting_after=N` to start after sequence number `N`.

Open the dashboard at `http://127.0.0.1:8080/v1/metrics/dashboard`. See [Metrics and the dashboard](../operations/metrics.md).

## Configure the server

The server reads its configuration from environment variables only. It has no command-line flags.

| Variable | Default | Effect |
|---|---|---|
| `ROUNDHOUSE_ADDR` | `127.0.0.1:8080` | The `host:port` to bind. |
| `ROUNDHOUSE_CATALOG` | the echo catalog | Path to the catalog JSON: hosted models, prices, and providers. |
| `ROUNDHOUSE_FRONTIER_UPSTREAM` | echo stub | `openai_responses` sends turns to real providers. Each provider's wire dialect comes from the catalog. Any other value stops the boot. |
| `ROUNDHOUSE_CONTROL_PLANE` | open plane | Path to the control plane JSON: projects, users, memberships, keys, and policy. |
| `ROUNDHOUSE_REDIS_URL` | process memory | A `redis://` URL for all durable state. |
| `ROUNDHOUSE_REDIS_NAMESPACE` | `rh` | The key prefix in Redis. A blank value stops the boot. |
| `ROUNDHOUSE_JUDGE_MODEL` | no judge | The catalog model, as `provider/model`, that judges validated turns. |
| `ROUNDHOUSE_CLASSIFY_CONFIG` | off | Path to the background turn classification configuration. |
| `ROUNDHOUSE_OPENAI_API_BASE` | published endpoint | Where a stored key authenticates for the built-in `openai` provider. |
| `ROUNDHOUSE_OPENAI_PASS_THROUGH_BASE` | published endpoint | Where a forwarded ChatGPT login authenticates. |
| `RUST_LOG` | `info` | The log filter. |

A file variable that is set but cannot be read stops the process. The server does not fall back to a default. A fallback serves every turn under prices, tenancy, or storage that nobody chose. The same rule applies to a Redis URL that is set but cannot be reached. The [Configuration reference](../appendix/configuration.md) lists every variable.

## Add a catalog and real providers

1. Copy `examples/catalog.example.json` to a file of your own.
2. Replace each placeholder model name and each price with the current published values.
3. Start the server with `ROUNDHOUSE_CATALOG` set to your file.

With only `ROUNDHOUSE_CATALOG` set, the echo stub still answers every turn. The router chooses among your catalog models and prices them. This mode is a priced dry run.

4. Put each provider key in the variable that the provider's `auth.env` names, for example `OPENROUTER_API_KEY`.
5. Set `ROUNDHOUSE_FRONTIER_UPSTREAM=openai_responses` and restart the server.

The boot refuses a catalog model whose provider has no definition, and a provider whose models speak two dialects. See [Configure providers and the catalog](catalog.md).

## Add a control plane

Without a control plane, any client can use the server with no key. For a shared deployment, load a control plane. See [Configure tenancy and keys](tenancy.md).

The shipped `examples/control-plane.example.json` describes a deployment with a local Dynamo fleet. The `roundhouse` binary attaches no fleet. So the boot refuses that file: its frontier cadence promises local service when the window is spent, and no local capacity exists. Remove the local promises from your copy, or wire in a fleet.

## Connect an agent

- Codex: read [Hook up Codex](codex.md).
- Claude Code: read [Hook up Claude Code](claude-code.md).
- Either agent, from a saved profile: read [Launch with topham](topham.md). This is the usual path for an operator.

For durable state across restarts and nodes, read [Deploy with Redis](../operations/redis.md).
