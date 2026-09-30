# Configuration reference

This appendix lists every environment variable that Roundhouse and `topham` read, and every HTTP route that the server mounts. The server takes no command-line flags. Environment variables and two JSON files configure it.

## Server variables

The `roundhouse` binary reads these variables at boot. A variable that is set but unusable stops the boot. Roundhouse does not fall back to a default, because a fallback runs under settings that nobody chose.

| Variable | Meaning | If unset |
|---|---|---|
| `ROUNDHOUSE_ADDR` | Address to bind, as `host:port`. A value that does not parse stops the boot. | `127.0.0.1:8080` |
| `ROUNDHOUSE_CATALOG` | Path to the catalog JSON. See [Configure providers and the catalog](../guides/catalog.md). | The offline echo stub serves every turn, and every price is zero. |
| `ROUNDHOUSE_CONTROL_PLANE` | Path to the control-plane JSON. See [Configure tenancy and keys](../guides/tenancy.md). | Open mode. Every request is the `default/default` principal, and no key is required. |
| `ROUNDHOUSE_FRONTIER_UPSTREAM` | The value `openai_responses` switches on dispatch to real providers. Each catalog entry names its own wire. Any other value stops the boot. | The echo stub answers. No request leaves the process. |
| `ROUNDHOUSE_OPENAI_API_BASE` | Base URL for a stored key on the built-in `openai` provider. | `https://api.openai.com/v1` |
| `ROUNDHOUSE_OPENAI_PASS_THROUGH_BASE` | Base URL for a forwarded ChatGPT login on the built-in `openai` provider. | `https://chatgpt.com/backend-api/codex` |
| `ROUNDHOUSE_JUDGE_MODEL` | The catalog entry that the validate loop uses as its judge, written as `provider/model`. | No judge. A control plane that enrolls a project in validation then stops the boot. |
| `ROUNDHOUSE_CLASSIFY_CONFIG` | Path to the background classification JSON. An unreadable file or an invalid field stops the boot. | Classification is off. No turn content leaves the deployment. |
| `ROUNDHOUSE_REDIS_URL` | A `redis://` URL. It selects Redis for sessions, committed spend, fair-use windows, conversation correlation, the admin directory, and the learner store. | Every one of these lives in process memory and is lost on exit. |
| `ROUNDHOUSE_REDIS_NAMESPACE` | The namespace that every shared Redis key is built under. A value that is set but blank stops the boot. Roundhouse reads it even when no Redis URL is set. | `rh` |
| `RUST_LOG` | Log filter for the `tracing` subscriber. | `info` |

`ROUNDHOUSE_OPENAI_API_BASE` and `ROUNDHOUSE_OPENAI_PASS_THROUGH_BASE` have no effect when the catalog defines an `openai` provider itself. The definition takes precedence, and Roundhouse logs a warning for each shadowed variable.

### Variables that the files name

Some files hold the name of a variable, not a value. The server reads the variable that the file names.

| Named by | Read by | Meaning |
|---|---|---|
| `providers.<name>.auth.env` in the catalog | The server, for each provider | The key for that provider. A provider with no key anywhere in the environment gives a boot warning. |
| Credential entries in the control-plane file | The server, at load | A provider key for a deployment, a project, or a member. |
| `auth.env` in the classification file | The server, at load | The credential for the classification service. |

The name of the variable can be any valid variable name. A name that an environment cannot hold is refused at load.

### Variables for build and tools

| Variable | Read by | Meaning | If unset |
|---|---|---|---|
| `ROUNDHOUSE_SOURCE_COMMIT` | The build of `roundhouse-server` | The source commit that `learner-calibrate` records in its sidecar. It is read at compile time. | The record names the crate version. |
| `TOPHAM_BUILD_COMMIT` | The build of `topham` | The commit that `topham --version` prints. It is set by the build script. | |
| `OPENROUTER_API_KEY` | `import-benchmarks` | The key for `GET /api/v1/benchmarks`. Any valid OpenRouter key works. | The tool stops. |
| The name in the manifest | `learner-calibrate` | The variable that holds the Redis URL. The manifest names it. A URL is never written into the manifest. | The tool stops. |
| `HOSTNAME` | `learner-calibrate` | The host name that goes into the sidecar. | `/etc/hostname`, then `unknown` |

## Topham variables

`topham` reads the environment once, at the start, into a map. It never writes to its own environment.

| Variable | Meaning | If unset |
|---|---|---|
| `ROUNDHOUSE_ADMIN_KEY` | The admin key that `topham mint` uses. A turn key is refused. | `topham mint` stops. |
| The `key-env` of the profile | The turn key of the profile. The profile names the variable and never holds the value. | `topham plan` and `topham launch` refuse. The default name is `ROUNDHOUSE_API_KEY`. |
| `XDG_CONFIG_HOME` | Where profiles live: `$XDG_CONFIG_HOME/topham/profiles/<name>.toml`. A relative or empty value counts as unset. | `$HOME/.config` |
| `XDG_DATA_HOME` | Where per-profile generated files live, such as a generated `CODEX_HOME`. A relative or empty value counts as unset. | `$HOME/.local/share` |
| `HOME` | Fallback for both XDG directories, and for the user settings file of Claude Code. It must be an absolute path. | The XDG variables are then required. |
| `CLAUDE_CONFIG_DIR` | Moves the user settings file of Claude Code. `topham plan` reads that file to look for `apiKeyHelper`. | `$HOME/.claude` |
| `PATH` | Finds the agent and NeMo Relay. The Relay preflight uses it. | |
| `NEMO_RELAY_ANTHROPIC_BASE_URL`, `NEMO_RELAY_OPENAI_BASE_URL` | Relay overrides the upstream of the agent with this variable, above an explicit config file. A chained launch refuses when the variable that matches its agent is set. | |
| `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY` | Cloud-provider selectors. They make Claude Code ignore its base URL, so no turn reaches Roundhouse. A launch refuses when one is set. | |
| `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR`, `CLAUDE_CODE_REMOTE`, `ANTHROPIC_API_KEY` | Inputs that change how the launched Claude Code authenticates. Which of them a launch refuses depends on the auth kind of the profile. | |

The last three rows list variables that must not be set. The launcher refuses. Otherwise the client bypasses Roundhouse, and nothing reports it. The setting `apiKeyHelper` in the settings files has the same effect. See [Launch with topham](../guides/topham.md).

### Variables that topham sets on the launched client

| Variable | Set on | Value |
|---|---|---|
| `ANTHROPIC_BASE_URL` | Claude Code | The deployment root, with no `/v1`. |
| `ANTHROPIC_CUSTOM_HEADERS` | Claude Code | A block of headers that carries the turn-key header. |
| `ANTHROPIC_API_KEY` | Claude Code, with `roundhouse-key` auth | A fixed sentinel. It is not a credential. |
| `DISABLE_AUTOUPDATER` | Claude Code | `1`. An update during a session can replace the binary that the dialect was verified against. |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` | Claude Code | `1` |
| `CODEX_HOME` | Codex | A per-profile directory under `XDG_DATA_HOME`. It holds the generated `config.toml`. |

## Test variables

These variables only matter for tests. Gated suites do not run unless you opt in. See [Testing](../development/testing.md).

| Variable | Meaning | If unset |
|---|---|---|
| `ROUNDHOUSE_TEST_REDIS_URL` | A reachable Redis for the Redis-backed suites. After you opt in with `--include-ignored`, an unreachable URL panics. A silent skip is how a suite stops running unseen. | The Redis suites stay ignored. |
| `ROUNDHOUSE_TEST_CODEX_BIN` | The `codex` binary that the `e2e-codex` suite drives. | `codex` on `PATH` |
| `ROUNDHOUSE_TEST_CLAUDE_BIN` | The `claude` binary that the `e2e-claude` suite drives. | `claude` on `PATH` |
| `ROUNDHOUSE_TEST_RELAY_BIN` | The `nemo-relay` binary for the chained tests of `e2e-claude`. | Those tests fail with a message that names the variable. |
| `ROUNDHOUSE_TEST_TOPHAM_BIN` | The built `topham` for the closure tests, for example `$PWD/target/debug/topham`. There is no `PATH` fallback. `topham` is installed nowhere, so a bare name finds whatever a developer happens to have. | Those tests fail with a message that names the variable. |
| `ROUNDHOUSE_PROBE_CATALOG` | Catalog for the live cache probe. See [Measure cache reuse](../guides/cache-reuse.md). | The live probe fails before any request. |
| `ROUNDHOUSE_PROBE_MODEL` | The pinned `provider/model` for the live cache probe. | The live probe fails before any request. |
| `ROUNDHOUSE_PROBE_LIMIT_USD` | The spend cap for the live cache probe, in US dollars. | The live probe fails before any request. |
| `ROUNDHOUSE_BLESS_GOLDEN_LABELS` | Set to `1` to rewrite the golden file of session labels instead of comparing with it. | The test compares. |

## HTTP endpoints

The server mounts these routes. Every route except the dashboard page needs a key when a control plane is configured. In Open mode, no key is needed anywhere, and the admin plane refuses every request before it reads a header.

A turn key goes in `Authorization: Bearer` or in the `x-roundhouse-key` header. An admin key on a turn route gets a 403 with `wrong_key_kind`.

### Turn surfaces

| Method and path | Surface | Purpose |
|---|---|---|
| `POST /v1/responses` | OpenAI Responses | Serve a turn for Codex and other Responses clients. The client sends its whole history, and Roundhouse admits the suffix. |
| `POST /v1/messages` | Anthropic Messages | Serve a turn for Claude Code and other Messages clients. |
| `POST /v1/messages/count_tokens` | Anthropic Messages | Count tokens with the process tokenizer. The count is an estimate. |
| `POST /mcp` | MCP control surface | Streamable HTTP for the eight control tools. A `GET` answers 405. |
| `POST /v1/sessions` | Native transport | Create a session. |
| `POST /v1/sessions/{session_id}/responses` | Native transport | Run a turn in a session. |
| `GET /v1/sessions/{session_id}/events` | Native transport | Read the event log as SSE. |

### Reads

| Method and path | Purpose |
|---|---|
| `GET /v1/sessions/{session_id}/atof` | The ATOF event stream of the session, as NDJSON. |
| `GET /v1/sessions/{session_id}/trajectory` | One ATIF v1.7 trajectory of the session. |
| `GET /v1/sessions/{session_id}/optimization` | One `LlmOptimizationSummary` for each turn of the session. |
| `GET /v1/metrics` | The metrics document as JSON. An admin key gets the whole deployment. A turn key gets its own membership. |
| `GET /v1/metrics/dashboard` | The dashboard page. It needs no key. It fetches `/v1/metrics`, and that request needs one. |

The session reads check that the session id is in the namespace of the caller before they touch the store. The answer is the same whether the session exists or not.

### Admin plane

The admin routes need an admin key. They are the only routes that write tenancy.

| Method and path | Purpose |
|---|---|
| `POST /v1/admin/projects` | Create a project. |
| `GET /v1/admin/projects` | List projects. |
| `GET /v1/admin/projects/{project}` | Read a project. |
| `PATCH /v1/admin/projects/{project}` | Change a project. |
| `DELETE /v1/admin/projects/{project}` | Archive a project. There is no route that deletes one. |
| `GET /v1/admin/projects/{project}/budget` | The reconciliation view: `committed_usd`, `measured_usd`, and `drift_usd`. |
| `GET /v1/admin/projects/{project}/members` | List the members of a project. |
| `PUT /v1/admin/projects/{project}/members/{user}` | Create or replace a membership. |
| `DELETE /v1/admin/projects/{project}/members/{user}` | Delete a membership. |
| `POST /v1/admin/projects/{project}/members/{user}/keys` | Mint a turn key. The secret is returned once. |
| `POST /v1/admin/users` | Create a user. |
| `GET /v1/admin/users` | List users. |
| `POST /v1/admin/keys` | Mint an admin key. The secret is returned once. |
| `GET /v1/admin/keys` | List keys. |
| `GET /v1/admin/keys/{key_id}` | Read a key. |
| `DELETE /v1/admin/keys/{key_id}` | Revoke a key. A second call does nothing more. |
| `POST /v1/admin/credentials` | Refused. It answers 501 `credential_crud_not_available`, or a 400 for a body that looks like an OAuth token. |

The server has no health route, no readiness route, and no `/v1/models`.
