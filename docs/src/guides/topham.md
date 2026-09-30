# Launch with topham

This chapter tells how to use `topham`, the operator entry point that turns a saved profile into a running Codex or Claude Code client. It covers profiles, the subcommands, the refusals, the interactive screen, and the tests that prove the launcher.

## What topham is

The generators `codex_launch` and `claude_launch` are library functions. `topham` is one binary, above the server in the dependency graph, that calls them for an operator. See [Hook up Codex](codex.md) and [Hook up Claude Code](claude-code.md) for what the generators write.

```bash
topham mint --profile work --project acme --user ada   # prints an export line
export ROUNDHOUSE_API_KEY=rh_turn_…                    # the key rides the environment
topham plan work                                       # what it resolves to; spawns nothing
topham launch work -- -p "hello"                       # becomes the client
topham relay chained -- -p "hello"                     # becomes nemo-relay running the client
topham                                                 # plan, launch and relay, on a screen
```

| Command | What it does |
|---|---|
| `topham plan <profile>` | Prints the resolved launch with every secret redacted. Spawns nothing. |
| `topham launch <profile> -- <argv>` | Writes any Codex files, then replaces itself with the client. Direct topology only. |
| `topham relay <profile> [--relay <path>] -- <argv>` | Replaces itself with `nemo-relay run` wrapping the client. Chained topology only. `--relay` names a Relay binary, which is `nemo-relay` on `PATH` by default. |
| `topham mint --profile <p> --project <P> --user <U>` | Mints a turn key over the admin API and prints its export line. |
| `topham` | Opens the interactive screen. |

The `--` before the client arguments is mandatory. `topham launch work -p hello` is a parse error that names the separator. The separator keeps `topham` from reading `-p` as one of its own flags.

With no subcommand, `topham` gives you plan, launch and relay, on a screen that fronts the same code paths a script uses.

`plan`, `launch`, and the screen all call one function, `plan::resolve`. So a dry run cannot describe a launch different from the one that then happens.

## Profiles

A profile names things and never holds a secret. It is TOML at `$XDG_CONFIG_HOME/topham/profiles/<name>.toml`, else `$HOME/.config/topham/profiles/<name>.toml`. NeMo Relay follows the same rule. An empty or relative XDG value counts as unset.

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
| `deployment-root` | required | The address Roundhouse serves on, with no `/v1`. Each generator gets the shape it needs: Claude Code gets the root, and Codex gets the root plus `/v1`. One field means a profile cannot name two deployments. |
| `auth` | `roundhouse-key` | `roundhouse-key` or `forwarded-login`. |
| `key-env` | `ROUNDHOUSE_API_KEY` | The name of the variable the turn key is read from. |
| `topology` | `direct` | `direct` or `chained`. |
| `strict-mcp` | `false` | Claude only. Adds `--strict-mcp-config`, which drops every other MCP configuration. Refused on a Codex profile. |
| `model` | `roundhouse-local` | Codex only. The model slug. A real OpenAI slug resolves client metadata that the Responses surface refuses. |
| `model-catalog-path` | beside the config | Codex only. An absolute path to a catalog the operator maintains. When set, `topham` does not write a catalog. |

Unknown fields are refused, because each field changes where a client posts turns or which credential it presents. A misspelled field otherwise launches with the default and looks correct.

**A profile that holds a secret is refused on load, naming the field.** The check runs before deserialization, over every key and value in the document, as a substring match. It finds `rh_turn_` keys, `rh_admin_` keys, and the sentinel. So a key inside an `export` line, a URL, or an unknown field is still found. A key pasted as a table key is reported as `<a key>`, never as itself. A configuration directory is what ends up in a dotfile repository, and nothing downstream can tell that copy from a live credential.

A profile name is one filename: ASCII letters, digits, `-`, `_`, and `.`, with no leading `.` or `-`.

Generated files live per profile under `$XDG_DATA_HOME/topham/<name>/`, else `$HOME/.local/share/topham/<name>/`:

| Path | Content |
|---|---|
| `codex-home/config.toml` | The generated Codex config. |
| `codex-home/model-catalog.json` | The generated catalog, unless the profile names one. |
| `relay-config.toml` | The generated Relay config for a chained launch. |

The root is per profile, not per machine. Two profiles that share a root also share the `auth.json` that a `codex login` writes.

## topham mint

`topham mint` posts to `/v1/admin/projects/{p}/members/{u}/keys` with an admin key from `ROUNDHOUSE_ADMIN_KEY`. It prints the `export` line for the profile's `key-env`. It writes nothing to disk. The minted secret is returned once, because the admin plane stores only a hash. See [Configure tenancy and keys](tenancy.md).

The project and the member are arguments, not profile fields. A membership is a fact about the control-plane directory. A copy in a profile becomes wrong the first time a member moves.

## topham plan

`topham plan` prints the whole resolution with every secret redacted. The redaction is the generators' own `Debug`, not a copy in the launcher:

- The turn key renders as `redacted:<fingerprint>`, and a declared ambient variable renders as `<set>`.
- The generated argv prints one argument per line, with the key variable **unexpanded**. It shows what is passed, not what it becomes.
- The signage is named by its length, not printed.
- For Codex, the generated files are listed by path, not printed.

`plan` refuses the same things `launch` refuses. A dry run that resolves differently from the real launch is worse than no dry run, because operators believe it.

The plan ends with notes: the limits that no refusal can close. They are properties of the client, not of Roundhouse:

- Under a subscription login, an interactive Claude Code session asks once before it uses the API key.
- A headless session needs `--allowedTools` naming a control tool before it will call one.
- A forwarded login needs a completed `claude` login or `codex login` first.
- An administrator's managed-policy settings file is not read.
- A chained Codex run is unproven. See [Limits](#limits).

## topham launch

`topham launch` resolves the profile, writes the Codex files if there are any, and replaces itself with the client (`exec`). Every refusal happens before the `exec`, because after it this process is gone.

The child environment is the operator's environment with the generated values applied over it. The child gets the whole map after `env_clear`. So the layering rule is a property of a map that a test can read: a generated variable beats an ambient one of the same name, and an unrelated ambient variable survives.

| Agent | What the launch adds |
|---|---|
| Claude Code | The generated map, plus `DISABLE_AUTOUPDATER=1` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`. Leading argv: `--mcp-config` and `--append-system-prompt`. |
| Codex | `CODEX_HOME` pointed at the profile's own directory, plus the key variable copied through. Two files in that directory. |

The deployment policy variables are set because an update mid-session swaps the binary whose wire behavior was verified. The telemetry is about a session that is not Anthropic's to see. They overwrite an ambient value of the same name.

`topham launch` does not write the Codex skill files that `codex_launch` can emit.

## Refusals

A shell function that exports three variables catches none of these. Each one fails by *running*, with every turn still answered.

- **A suppressor in the environment.** For example, a `forwarded-login` profile beside an ambient `CLAUDE_CODE_USE_VERTEX` forwards nothing. `topham` hands every ambient variable named by `ClaudeLaunch::must_be_unset` to the generator, and the generator refuses. The table is the generator's own, not a copy. See [the suppressor table](claude-code.md#the-suppressor-table).
- **No turn key exported.** A `roundhouse-key` launch with no key reaches Roundhouse with no credential. Roundhouse admits it and degrades the turn to local-only routing. Refused when the profile is resolved.
- **A key variable that the launch writes itself** (`KeyEnvIsGenerated`). For example, with `key-env = "ANTHROPIC_API_KEY"`, the registration expands to the sentinel. Every control call is then refused while inference keeps working.
- **The wrong subcommand for the topology.** `topham launch` on a chained profile, and `topham relay` on a direct one, are each refused and name the other subcommand.
- **A duplicated generated flag.** An operator argv that repeats `--mcp-config` or `--append-system-prompt` is refused. Either order of a duplicate runs: one loses the control surface and the other loses the operator's own servers, and nothing reports either.
- **A settings file that overrides the launch.** `topham launch` and `topham plan` read the settings files the client itself loads:
  - `$CLAUDE_CONFIG_DIR/settings.json`, else `$HOME/.claude/settings.json`
  - `./.claude/settings.json`
  - `./.claude/settings.local.json`.

  They refuse a file whose `env` block overrides a generated variable or sets a suppressor, or which sets `apiKeyHelper`. The refusal names the file and the key. Claude Code applies a settings `env` block by replacing the inherited value, so the file outranks the launch. A persistent `nemo-relay install claude-code` writes exactly such a block. A file that does not parse is refused, not skipped. Only names are read from the `env` block, never values.
- **A re-aimed Relay upstream.** See [topham relay](#topham-relay).

The administrator's managed-policy settings file is not read. It is outside the operator's control, and its path is platform-specific in a way that nothing here verified. The plan notes state it.

## topham relay

`topham relay` runs the same launch with NeMo Relay in the middle, and hands the client the same generated map. The program it becomes is `nemo-relay run --agent <agent> --config <relay-config.toml> -- <argv>`. The Relay config is rendered by `roundhouse_server::relay_handoff`, which the gated suite also uses.

Relay resolves its upstream from layers, and two of them are outside `--config`. Both re-aim the launch silently and send the turn key to the wrong place.

| Layer | Precedence | Refusal |
|---|---|---|
| `/etc/nemo-relay/config.toml` | Folded in after `--config`. Wins on any key both name. | `topham relay` runs `nemo-relay run --dry-run` with a cleared environment, reads what Relay resolved, and refuses a re-aimed upstream. |
| `NEMO_RELAY_ANTHROPIC_BASE_URL` | Above `--config`. | Refused directly against the captured environment (`UpstreamOverriddenByEnv`). The preflight cannot see it, because it clears the environment on purpose. |

These layer facts were read at Relay 0.8.0 and 0.8.2. The auth-header variable has no command-line flag. Relay does not validate base URLs.

**The launch itself is not isolated.** The preflight points Relay's XDG variables at its own scratch. The launch does not, because the operator's `plugins.toml` (exporters, pricing, PII) is the reason to run chained. The user config layer is replaced by `--config` in either case. The cost is that the preflight and the launch resolve under different environments, and the gap is exactly the environment layer. That is why the second refusal exists.

The `topham relay` banner and the preflight report go to stderr. The client owns stdout, and `claude -p --output-format json` prints exactly one JSON document there.

NeMo Relay 0.8.0 has 13 subcommands, a line-prompt wizard, and no terminal screen. Its "profile" is only a label header. `topham` adds saved profiles that carry the agent, deployment root, and auth kind, plus a screen over plan, launch, and relay.

## The interactive screen

`topham` with no subcommand opens a screen with:

- the profile list
- an editor for the profile fields
- a plan pane rendered from the same redacted resolution
- launch and relay actions.

Show, launch, and relay are the `plan`, `launch`, and `relay` subcommands. Save writes the profile file, with the same refusal that loading it applies, which is what an operator's own editor does. Mint is not on the screen, because it takes tenancy arguments that a profile does not carry. The screen holds no state that the profile files do not. The list is re-read from the directory after every write. The state changes are pure functions over key events and are tested without a terminal. Only the draw-and-read loop is left untested.

If stdout is not a terminal, `topham` with no subcommand refuses, names the subcommands, and writes nothing to stdout. The check is on stdout. The terminal library opens `/dev/tty`, so without this check it draws the screen into a redirected file.

## What proves it

The `topham` suite covers:

- the profile round trip and the secret refusal
- whole-output plan snapshots for both agents and both auth kinds
- the `must_be_unset` refusal, naming the variable
- the environment layering: a generated variable beats an ambient one of the same name, and an unrelated ambient variable survives
- `mint` against the real `admin_router` on a loopback socket.

The gated real-binary suites drive the real clients through a real `topham`:

| Test | Topology |
|---|---|
| `a_real_client_launched_through_topham_hooks_up_on_direct` | `topham launch`, Claude Code, Direct |
| `a_real_client_handed_to_relay_through_topham_hooks_up_chained` | `topham relay`, Claude Code, Chained |
| `a_real_client_reaches_the_control_surface_through_the_turn` | `topham launch`, Claude Code, with the control surface |
| `a_real_codex_launched_through_topham_hooks_up` | `topham launch`, Codex, Direct |

Each asserts at the Roundhouse edge what the hand-built tests assert. The `topham` child gets only a turn key, its home directories, and a `PATH`. It gets **no `ANTHROPIC_*` variable** in the Claude Code tests, and **no `CODEX_HOME` and no config file** in the Codex test. A launcher that resolved the profile wrongly cannot pass by inheriting anything. The control run adds the argv half, because only the launcher registers the `/mcp` mount with the client.

Build `topham`, then run the Claude Code suite:

```bash
cargo build -p topham
ROUNDHOUSE_TEST_TOPHAM_BIN=$PWD/target/debug/topham \
ROUNDHOUSE_TEST_CLAUDE_BIN=… ROUNDHOUSE_TEST_RELAY_BIN=… \
    timeout 900 cargo test -p roundhouse-server --features e2e-claude \
    --test claude_e2e -- --include-ignored --test-threads=1
```

A missing `ROUNDHOUSE_TEST_TOPHAM_BIN` under `--include-ignored` is a loud panic that names the variable. It names a *freshly built* binary on purpose, because a stale one reports green for code nobody compiled. `topham --version` prints the commit the binary was built from. The suite compares it with `HEAD` and warns when they differ.

## Limits

- **Chained Codex is unproven.** Relay adds `--config model_provider="nemo-relay-openai"` to the codex argv, and a codex `--config` override outranks the generated `config.toml`. So the turn-key header that the config names is not what the client presents. Roundhouse sees a credential-less turn, admits it, and degrades it to local-only routing. `topham plan` states this on that profile and does not refuse it. The remedy, Relay's own `openai_auth_header`, is the untested fallback wiring. Chained Claude Code does not have this problem.
- **A control call chained through Relay is untested.** Only the Direct control run exists.
- **The Codex launch test has never run.** No `codex` binary was available where `a_real_codex_launched_through_topham_hooks_up` was written.
- **An interactive Claude Code session asks once** before it uses the API key under a subscription login. It also asks before it calls an `mcp__roundhouse__*` tool. `topham plan` states both prompts.
- **The managed-policy settings file is not read.**
