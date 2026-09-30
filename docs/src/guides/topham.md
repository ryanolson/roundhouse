# Launch with topham

This chapter tells how to use `topham`, the operator entry point that turns a saved profile into a running Codex or Claude Code client. It covers profiles, the subcommands, the refusals, the interactive screen, and the tests that prove the launcher.

## What topham is

`codex_launch` and `claude_launch` are library functions. `topham` is one binary, above the server in the dependency graph, that calls them for an operator. See [Hook up Codex](codex.md) and [Hook up Claude Code](claude-code.md) for what they write.

```bash
topham mint --profile work --project acme --user ada   # prints an export line
export ROUNDHOUSE_API_KEY=rh_turn_…                    # the key rides the environment
topham plan work                                       # what it resolves to, and spawns nothing
topham launch work -- -p "hello"                       # becomes the client
topham relay chained -- -p "hello"                     # becomes nemo-relay running the client
topham                                                 # plan, launch and relay, on a screen
```

`launch` and `relay` replace the `topham` process with the client (`exec`). `launch` serves the Direct topology and `relay` serves the Chained one. `relay --relay <path>` names a Relay binary, `nemo-relay` on `PATH` by default. The `--` before the client arguments is mandatory: `topham launch work -p hello` is a parse error that names the separator. `plan`, `launch`, and the screen all call `plan::resolve`, so a dry run cannot describe a launch different from the one that follows.

## Profiles

A profile names things and never holds a secret. It is TOML at `$XDG_CONFIG_HOME/topham/profiles/<name>.toml`, else `$HOME/.config/topham/profiles/<name>.toml`. An empty or relative XDG value counts as unset. A profile name is one filename: ASCII letters, digits, `-`, `_`, and `.`, with no leading `.` or `-`.

```toml
agent = "claude"                            # claude | codex
deployment-root = "http://127.0.0.1:8080"   # the root, with no /v1
auth = "roundhouse-key"                     # roundhouse-key | forwarded-login
key-env = "ROUNDHOUSE_API_KEY"              # a name, never a value
topology = "direct"                         # direct | chained
strict-mcp = false                          # claude only: drop other MCP servers
```

| Field | Default | Meaning |
|---|---|---|
| `agent` | required | `claude` or `codex`. |
| `deployment-root` | required | The address Roundhouse serves on, with no `/v1`. Claude Code gets the root, and Codex gets the root plus `/v1`. One field means a profile cannot name two deployments. |
| `auth` | `roundhouse-key` | `roundhouse-key` or `forwarded-login`. |
| `key-env` | `ROUNDHOUSE_API_KEY` | The name of the variable the turn key is read from. |
| `topology` | `direct` | `direct` or `chained`. |
| `strict-mcp` | `false` | Claude only. Adds `--strict-mcp-config`, which drops every other MCP configuration. A Codex profile that sets it to `true` is refused. |
| `model` | `roundhouse-local` | Codex only. The model slug. A Claude profile that sets it is refused. |
| `model-catalog-path` | beside the config | Codex only. An absolute path to a catalog that the operator maintains. When set, `topham` writes no catalog. A Claude profile that sets it is refused. |

Unknown fields are refused, because each field changes where a client posts turns or which credential it presents, and a misspelled field launches with the default and looks correct.

**A profile that holds a secret is refused on load, naming the field.** The check runs on the raw text before deserialization, as a substring match for `rh_turn_`, `rh_admin_`, and the sentinel. It finds a key inside an `export` line, a URL, an unknown field, or a truncated paste. A key used as a table key is reported as `<a key>`, and a parse error never quotes the offending line. A configuration directory ends up in a dotfile repository, where nothing can tell a leaked copy from a live credential.

Generated files live per profile under `$XDG_DATA_HOME/topham/<name>/`, else `$HOME/.local/share/topham/<name>/`:

| Path | Content |
|---|---|
| `codex-home/config.toml` | The generated Codex config. |
| `codex-home/model-catalog.json` | The generated catalog, unless the profile names one. |
| `relay/relay-config.toml` | The generated Relay config for a chained launch. |

The root is per profile, because two profiles that share a root share the `auth.json` of a `codex login` and the Relay config of a chained launch.

## topham mint

`topham mint` posts to `/v1/admin/projects/{p}/members/{u}/keys` with the admin key from `ROUNDHOUSE_ADMIN_KEY`. It prints the `export` line for the `key-env` of the profile and writes nothing to disk. The admin plane stores only a hash, so the secret is returned once. See [Configure tenancy and keys](tenancy.md). The project and the member are arguments, because a copy of a membership in a profile goes wrong when a member moves.

## topham plan

`topham plan` prints the whole resolution, redacted by the `Debug` of the generators. The turn key renders as `redacted:<fingerprint>`, and a declared ambient variable renders as `<set>`. The generated argv prints one argument per line with the key variable **unexpanded**, so it shows what is passed, not what it becomes. The signage is named by its length, and the Codex files are listed by path.

`plan` refuses the same things as `launch`, because a dry run that resolves differently from the real launch is worse than none. Its closing notes state the limits that no refusal can close. See [Hook up Claude Code](claude-code.md#limits) and [Hook up Codex](codex.md#limits).

## topham launch

`topham launch` resolves the profile, writes the Codex files, and replaces itself with the client (`exec`). Every refusal happens before the `exec`, because after it this process is gone. The child gets the environment of the operator with the generated values applied over it, after `env_clear`. So a generated variable beats an ambient one of the same name, and an unrelated ambient variable survives.

| Agent | What the launch adds |
|---|---|
| Claude Code | The generated map, plus `DISABLE_AUTOUPDATER=1` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`. Leading argv: `--mcp-config` and `--append-system-prompt`. |
| Codex | `CODEX_HOME` pointed at the directory of the profile, and the key variable. Two files in that directory. |

The policy variables overwrite an ambient value. An update in mid-session swaps the binary whose wire behavior was verified, and the telemetry concerns a session that is not Anthropic's to see.

Generated files are overwritten on every launch, so an edit to a generated file lasts one run. Each file is written beside the old one and renamed over it. A client that opens `config.toml` during a second launch then never reads a half-written file, and Codex answers an empty config with a default OpenAI provider.

## Refusals

A shell function that exports three variables catches none of these. Each one fails by *running*, with every turn still answered.

- **A suppressor in the environment.** For example, a `forwarded-login` profile beside an ambient `CLAUDE_CODE_USE_VERTEX` forwards nothing. `topham` hands every ambient variable that `ClaudeLaunch::must_be_unset` names to the generator, and the generator refuses. See [the suppressor table](claude-code.md#the-suppressor-table).
- **No turn key exported.** This applies to every profile, because the key names the client to Roundhouse even under `forwarded-login`. Without it, a `roundhouse-key` client presents no credential, and Roundhouse degrades the turn to local-only routing. Roundhouse refuses a `forwarded-login` client as `missing_key`.
- **A key variable that the launch writes itself** (`KeyEnvIsGenerated`). With `key-env = "ANTHROPIC_API_KEY"`, the registration expands to the sentinel, so every control call is refused while inference keeps working.
- **The wrong subcommand for the topology.** `topham launch` on a chained profile, and `topham relay` on a direct one, are each refused and name the other subcommand.
- **A duplicated generated flag.** An operator argv that repeats `--mcp-config` or `--append-system-prompt` is refused. Either order runs: one loses the control surface and the other loses the servers of the operator, and nothing reports either.
- **A settings file that overrides the launch.** For Claude profiles, `topham launch` and `topham plan` read `$CLAUDE_CONFIG_DIR/settings.json` (else `$HOME/.claude/settings.json`), `./.claude/settings.json`, and `./.claude/settings.local.json`. They refuse a file whose `env` block overrides a generated variable or sets a suppressor, or which sets `apiKeyHelper`, and they name the file and the key. A settings `env` block replaces the inherited value, so the file outranks the launch. A persistent `nemo-relay install claude-code` writes such a block. A file that does not parse is refused, not skipped, and only the names in an `env` block are read.
- **A re-aimed Relay upstream.** See [topham relay](#topham-relay).

The managed-policy settings file of an administrator is not read. It is outside the control of the operator, and its path depends on the platform in a way that nothing here verified.

## topham relay

`topham relay` runs the same launch with NeMo Relay in the middle and the same generated map. The program it becomes is `nemo-relay run --agent <agent> --config <relay-config.toml> -- <argv>`. `roundhouse_server::relay_handoff` renders the config, and the gated suite uses the same renderer. See [NeMo Relay formats](../operations/relay-formats.md).

Relay resolves its upstream from layers, and two of them are outside `--config`. Both re-aim the launch silently and send the turn key to the wrong place (Relay 0.8.0, 0.8.2):

- **The system layer** `/etc/nemo-relay/config.toml` is folded in after `--config` and wins on any key both name. `topham relay` runs `nemo-relay run --dry-run` with a cleared environment, reads what Relay resolved, and refuses a re-aimed upstream.
- **The environment layer** sits above `--config`: `NEMO_RELAY_ANTHROPIC_BASE_URL` for Claude and `NEMO_RELAY_OPENAI_BASE_URL` for Codex. `topham relay` refuses it against the captured environment (`UpstreamOverriddenByEnv`) when the value differs from the wanted upstream. The preflight cannot see it, because it clears the environment on purpose.
 **The launch itself is not isolated.** The preflight points the XDG variables of Relay at its own scratch. The launch does not, because the `plugins.toml` of the operator (exporters, pricing, PII) is the reason to run chained. The two resolve under different environments, which is why the second refusal exists. The banner and the preflight report go to stderr, because `claude -p --output-format json` prints exactly one JSON document on stdout.

## The interactive screen

`topham` with no subcommand gives you plan, launch and relay, on a screen. It has a profile list, a field editor, a plan pane rendered from the same redacted resolution, and launch and relay actions. Save writes the profile file with the refusal that loading applies. Mint is not on the screen, because it takes tenancy arguments that a profile does not carry.

The screen holds no state that the profile files do not hold. Its state changes are pure functions over key events, tested without a terminal, so only the draw-and-read loop is untested. If stdout is not a terminal, `topham` refuses, names the subcommands, and writes nothing to stdout. The check is on stdout because the terminal library opens `/dev/tty`, and without it the screen is drawn into a redirected file.

## What proves it

The `topham` suite covers the profile round trip, the secret refusal, and whole-output plan snapshots for both agents and both auth kinds. It also covers the `must_be_unset` refusal, the environment layering, and `mint` against the real `admin_router` on a loopback socket.

Four gated tests drive a real client through a real `topham`: `a_real_client_launched_through_topham_hooks_up_on_direct`, `a_real_client_handed_to_relay_through_topham_hooks_up_chained`, `a_real_client_reaches_the_control_surface_through_the_turn`, and `a_real_codex_launched_through_topham_hooks_up` (never run, see [Limits](#limits)). The `topham` child gets only a turn key, its home directories, and a `PATH`, so a launcher that resolved the profile wrongly cannot pass by inheriting anything.

Build `topham` first, then run the suite of [Hook up Claude Code](claude-code.md#the-gated-real-binary-suite) with `ROUNDHOUSE_TEST_TOPHAM_BIN=$PWD/target/debug/topham`. See [Testing](../development/testing.md). The variable names a *freshly built* binary, because a stale one reports green for code nobody compiled. `topham --version` prints the commit that the binary was built from, and the suite warns when that commit differs from `HEAD`.

## Limits

- **Chained Codex is unproven.** `topham plan` states this and does not refuse it. See [Hook up Codex](codex.md#chaining-codex-through-nemo-relay).
- **A control call chained through Relay is untested**, and an interactive Claude Code session asks once before it uses the key. See [Hook up Claude Code](claude-code.md#limits).
- **The Codex launch test has never run.** No `codex` binary was available where it was written.
- **The managed-policy settings file is not read.**
