# Configuration reference

This appendix lists every environment variable that Roundhouse and `topham` read, and every HTTP route that the server mounts. The server takes no command-line flags, because a flag parser in the composition root is where deployment concerns start to leak in.

## Server variables

The `roundhouse` binary reads these at boot. A variable that is set but unusable stops the boot, because a fallback would run under settings nobody chose.

| Variable | Meaning | If unset |
|---|---|---|
| `ROUNDHOUSE_ADDR` | Address to bind, as `host:port`. | `127.0.0.1:8080` |
| `ROUNDHOUSE_CATALOG` | Path to the catalog JSON. See [Configure providers and the catalog](../guides/catalog.md). | The offline echo stub serves every turn at a price of zero. |
| `ROUNDHOUSE_CONTROL_PLANE` | Path to the control-plane JSON. See [Configure tenancy and keys](../guides/tenancy.md). | Open mode: every request is `default/default`, with no key. |
| `ROUNDHOUSE_FRONTIER_UPSTREAM` | `openai_responses` switches on dispatch to real providers; each catalog entry names its own wire. Any other value stops the boot. | The echo stub answers, and no request leaves the process. |
| `ROUNDHOUSE_OPENAI_API_BASE` | Base URL for a stored key on the built-in `openai` provider. | `https://api.openai.com/v1` |
| `ROUNDHOUSE_OPENAI_PASS_THROUGH_BASE` | Base URL for a forwarded ChatGPT login on the built-in `openai` provider. | `https://chatgpt.com/backend-api/codex` |
| `ROUNDHOUSE_JUDGE_MODEL` | The validate loop's judge, as a catalog `provider/model`. A value with no `/`, or that names no catalog entry, counts as unset. | No judge. A project enrolled in validation then stops the boot. The judge sends no credential, so on a real provider client every review is abandoned (see [Limitations](limitations.md#validate-and-steer)). |
| `ROUNDHOUSE_CLASSIFY_CONFIG` | Path to the background classification JSON. See [Metrics and the dashboard](../operations/metrics.md). | Classification is off, and no turn content leaves the deployment. |
| `ROUNDHOUSE_REDIS_URL` | A `redis://` URL for sessions, committed spend, fair-use windows, conversation correlation, the admin directory, and the learner store. | All of these live in process memory and are lost on exit. |
| `ROUNDHOUSE_REDIS_NAMESPACE` | The prefix of every shared Redis key. A blank value stops the boot, because it would collide with the default. It is read even with no Redis URL, so a typo fails at the boot that introduced it. | `rh` |
| `RUST_LOG` | Log filter for the `tracing` subscriber. | `info` |

An `openai` provider that the catalog defines takes precedence over the two `ROUNDHOUSE_OPENAI_*` variables, with a warning for each one that is set.

### Variables that the files name

These files hold a variable name, and the server reads that variable. A name that an environment cannot hold is refused at load.

| Named by | Meaning |
|---|---|
| `providers.<name>.auth.env` in the catalog | The stored key for that provider. When dispatch is on, an unset variable gives a boot warning. |
| Credential entries in the control-plane file | A provider key for a deployment, a project, or a member. |
| `auth.env` in the classification file | The credential for the classification service. |

### Variables for build and tools

| Variable | Read by | Meaning | If unset |
|---|---|---|---|
| `ROUNDHOUSE_SOURCE_COMMIT` | The `roundhouse-server` build, at compile time | The commit that `learner-calibrate` records in its sidecar. | The crate version. |
| `TOPHAM_BUILD_COMMIT` | The `topham` build | The commit that `topham --version` prints. Set by the build script. | |
| `OPENROUTER_API_KEY` | `import-benchmarks` | Any valid OpenRouter key, for `GET /api/v1/benchmarks`. | The tool stops. |
| The name in the manifest | `learner-calibrate` | The variable that holds the Redis URL, so the manifest never holds it. | The tool stops. |
| `HOSTNAME` | `learner-calibrate` | The host name in the sidecar. | `/etc/hostname`, then `unknown` |

## Topham variables

`topham` reads the environment once, at the start, and never writes to it.

| Variable | Meaning | If unset |
|---|---|---|
| `ROUNDHOUSE_ADMIN_KEY` | The admin key for `topham mint`. A turn key is refused. | `topham mint` stops. |
| The profile's `key-env` | The profile's turn key. The profile holds the name, never the value. | `topham plan` and `topham launch` refuse. The default name is `ROUNDHOUSE_API_KEY`. |
| `XDG_CONFIG_HOME` | Profiles live at `$XDG_CONFIG_HOME/topham/profiles/<name>.toml`. A relative or empty value counts as unset. | `$HOME/.config` |
| `XDG_DATA_HOME` | Per-profile generated files, such as a generated `CODEX_HOME`. A relative or empty value counts as unset. | `$HOME/.local/share` |
| `HOME` | Fallback for both XDG directories and for the Claude Code user settings file. It must be absolute. | The XDG variables are required. |
| `CLAUDE_CONFIG_DIR` | Moves the Claude Code user settings file, which `topham plan` reads for `apiKeyHelper`. | `$HOME/.claude` |
| `PATH` | Finds the agent and NeMo Relay. | |
| `NEMO_RELAY_ANTHROPIC_BASE_URL`, `NEMO_RELAY_OPENAI_BASE_URL` | Overrides the agent's upstream in Relay, above its config file. A chained launch refuses when the one for its agent is set. | |
| `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY` | Make Claude Code ignore its base URL, so no turn reaches Roundhouse. A launch refuses when one is set. | |
| `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR`, `CLAUDE_CODE_REMOTE`, `ANTHROPIC_API_KEY` | Change how Claude Code authenticates. Which ones a launch refuses depends on the profile's auth kind. | |

The launcher refuses the last three rows, and `apiKeyHelper` in a settings file, because each makes the client bypass Roundhouse with nothing to report it. See [Launch with topham](../guides/topham.md).

### Variables that topham sets on the launched client

| Variable | Set on | Value |
|---|---|---|
| `ANTHROPIC_BASE_URL` | Claude Code | The deployment root, with no `/v1`. |
| `ANTHROPIC_CUSTOM_HEADERS` | Claude Code | A header block that carries the turn key. |
| `ANTHROPIC_API_KEY` | Claude Code, with `roundhouse-key` auth | A fixed sentinel, not a credential. |
| `DISABLE_AUTOUPDATER` | Claude Code | `1`, so no update replaces the binary that the dialect was verified against. |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` | Claude Code | `1` |
| `CODEX_HOME` | Codex | A per-profile directory under `XDG_DATA_HOME` with the generated `config.toml`. |
| `PATH`, `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` | The Relay preflight child | A cleared environment: `PATH` is carried over, and the rest point at one scratch directory, so the preflight reads no configuration nobody wrote for it. |

## Test variables

Gated suites run only when you opt in. See [Testing](../development/testing.md).

| Variable | Meaning | If unset |
|---|---|---|
| `ROUNDHOUSE_TEST_REDIS_URL` | A Redis for the Redis-backed suites. Once you opt in with `--include-ignored`, an unreachable URL panics instead of silently skipping. | The Redis suites stay ignored. |
| `ROUNDHOUSE_TEST_CODEX_BIN` | The `codex` binary for the `e2e-codex` suite. | `codex` on `PATH` |
| `ROUNDHOUSE_TEST_CLAUDE_BIN` | The `claude` binary for the `e2e-claude` suite. | `claude` on `PATH` |
| `ROUNDHOUSE_TEST_RELAY_BIN` | The `nemo-relay` binary for the chained `e2e-claude` tests. | Those tests fail and name the variable. |
| `ROUNDHOUSE_TEST_TOPHAM_BIN` | A built `topham` for the closure tests, such as `$PWD/target/debug/topham`. No `PATH` fallback, because `topham` is installed nowhere. | Those tests fail and name the variable. |
| `ROUNDHOUSE_PROBE_CATALOG` | Catalog for the live cache probe. See [Measure cache reuse](../guides/cache-reuse.md). | The live probe fails before any request. |
| `ROUNDHOUSE_PROBE_MODEL` | The pinned `provider/model` for the live cache probe. | The live probe fails before any request. |
| `ROUNDHOUSE_PROBE_LIMIT_USD` | The live cache probe's spend cap, in US dollars. | The live probe fails before any request. |
| `ROUNDHOUSE_BLESS_GOLDEN_LABELS` | `1` rewrites the golden session-label file instead of comparing with it. | The test compares. |

## HTTP endpoints

With a control plane, every route except the dashboard page needs a key. In Open mode no route needs one, the admin plane refuses every request, and `/mcp` accepts only a loopback `Host`.

A turn key goes in `Authorization: Bearer` or in the `x-roundhouse-key` header. An admin key on a turn route gets 403 `wrong_key_kind`.

### Turn surfaces

| Method and path | Surface | Purpose |
|---|---|---|
| `POST /v1/responses` | OpenAI Responses | A turn for Codex and other Responses clients. The client sends its whole history, and Roundhouse admits the suffix. |
| `POST /v1/messages` | Anthropic Messages | A turn for Claude Code and other Messages clients. |
| `POST /v1/messages/count_tokens` | Anthropic Messages | An estimated token count from the process tokenizer. |
| `POST /mcp` | MCP control surface | Streamable HTTP for the eight control tools. `GET` answers 405. |
| `POST /v1/sessions` | Native transport | Create a session. |
| `POST /v1/sessions/{session_id}/responses` | Native transport | Run a turn in a session. |
| `GET /v1/sessions/{session_id}/events` | Native transport | The event log as SSE. |

### Reads

| Method and path | Purpose |
|---|---|
| `GET /v1/sessions/{session_id}/atof` | The session's ATOF event stream, as NDJSON. |
| `GET /v1/sessions/{session_id}/trajectory` | One ATIF v1.7 trajectory of the session. |
| `GET /v1/sessions/{session_id}/optimization` | One `LlmOptimizationSummary` per turn. |
| `GET /v1/metrics` | The metrics document. An admin key sees the deployment, a turn key its own membership. |
| `GET /v1/metrics/dashboard` | The dashboard page, with no key. Its fetch of `/v1/metrics` needs one. |

A session read checks that the id is in the caller's namespace before it touches the store. It answers the same whether the session exists or not.

### Admin plane

These routes need an admin key and are the only routes that write tenancy.

| Method and path | Purpose |
|---|---|
| `POST /v1/admin/projects` | Create a project. |
| `GET /v1/admin/projects` | List projects. |
| `GET /v1/admin/projects/{project}` | Read a project. |
| `PATCH /v1/admin/projects/{project}` | Change a project. |
| `DELETE /v1/admin/projects/{project}` | Archive a project. No route deletes one. |
| `GET /v1/admin/projects/{project}/budget` | The reconciliation view. See [Metrics and the dashboard](../operations/metrics.md#the-reconciliation-view). |
| `GET /v1/admin/projects/{project}/members` | List a project's members. |
| `PUT /v1/admin/projects/{project}/members/{user}` | Create or replace a membership. |
| `DELETE /v1/admin/projects/{project}/members/{user}` | Delete a membership. |
| `POST /v1/admin/projects/{project}/members/{user}/keys` | Mint a turn key. The secret is returned once. |
| `POST /v1/admin/users` | Create a user. |
| `GET /v1/admin/users` | List users. |
| `POST /v1/admin/keys` | Mint an admin key. The secret is returned once. |
| `GET /v1/admin/keys` | List keys. |
| `GET /v1/admin/keys/{key_id}` | Read a key. |
| `DELETE /v1/admin/keys/{key_id}` | Revoke a key. A repeat also answers 204. |
| `POST /v1/admin/credentials` | Refused: 501 `credential_crud_not_available`, or 400 `oauth_credentials_unsupported` for an OAuth-shaped body. |

There is no health route, no readiness route, and no `/v1/models`.
