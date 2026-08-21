<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Codex 0.146.0 vs. the pin: vigilance reconciliation

> **Status: evidence.** Produced 2026-08-21, checking every Codex-behavior
> claim `PLAN-agentic-control-plane.md` §3 makes against the binary M9
> drives, the Cargo pin the plan was read from, and the newer rev the
> plan's own open question pointed at. An independent Sonnet fact-check
> re-derived claims 1, 3, 5, 6, 7, 8, 9, 10, 12, and 13 against the pinned
> checkouts: zero substantive refutations, one file:line correction
> (applied below). Per `agent-docs/README.md` this is a pinned snapshot,
> updated only by dated addenda. The ruling it supports is the 2026-08-21
> M9 addendum to `../PLAN-agentic-control-plane.md` §3.

**Revisions** (`B` = binary, `P` = Cargo pin, `R` = the plan's ruling rev):

| tag | rev | date | role |
|---|---|---|---|
| **B** | `e363b08` (`rust-v0.146.0`) | 2026-07-28 | binary M9 drives (`codex --version` → `codex-cli 0.146.0`) |
| **P** | `6344a65` | 2026-08-13 | Cargo pin (`crates/roundhouse-server/Cargo.toml`) |
| **R** | `3b45c29` | 2026-08-19 | rev PLAN §3's `requires_openai_auth` ruling was read from |

Neither B nor P is an ancestor of the other; merge-base `95637f7` (verified
here — `git merge-base` resolves it, `--is-ancestor` exits non-zero both
directions against the same blobless clone) — siblings off a common
ancestor, so R can add behavior neither endpoint had.

**Method: diff-first.** Every claim path: checkouts compared byte-for-byte
before reading; an empty diff is the verdict. Byte-identical B↔P:
`model-provider/src/auth.rs`, `model-provider/src/models_endpoint.rs`,
`exec/src/exec_events.rs` (M9's `--json` assertions pin-safe for free). R is
read only via `git show 3b45c29:<path>` against a blobless clone, which the
report describes as "blobless and history-shallow for these paths" — so
claim 14's census is `diff -u` hunks, not a commit log. The commit-graph
queries below (merge-base, `--is-ancestor`) resolve fine against that same
clone; it is blob content, not the graph, that fetches lazily.

**Headline.** Claim 1 is CHANGED in the dangerous direction, and claim 3's
"only the pass-through route fetches `/models`" falls out of the same root
cause. The `resolve_provider_auth` guard PLAN §3 leans on — "leave
`requires_openai_auth` unset and codex attaches **nothing**" — does not exist
in the binary or at the pin; added between `6344a65` and `3b45c29`, settling
the plan's own open question ("whether `auth.rs:205-207` post-dates
`6344a65` or was simply missed"): it post-dates the pin. The M-era ruling
was correct for the pin and the binary; the 08-19 refutation is correct only
for revs newer than both.

---

## Claim 1 — `requires_openai_auth`: existence, default, forwarding, negative

### Verdict: **field SAME; forwarding SAME; the negative ("false attaches nothing") is CHANGED — ABSENT at 0.146.0 and ABSENT at the pin, present only at `3b45c29`.**

Field on `ModelProviderInfo`, `#[serde(default)] pub requires_openai_auth:
bool` (`model-provider-info/src/lib.rs:137` @B, `:136` @P, identical, default
`false`; struct differs by one item B↔P — only B has
`supports_remote_compaction()`, `lib.rs:422-424` @B, claim 7). Forwarding when
`true` is confirmed: `resolve_provider_auth`
(`model-provider/src/auth.rs:179-196` @B — corrected from the report's
`198-215`, which names the next function, `resolve_provider_auth_for_scope`)
falls through to `auth_provider_from_auth`, which for
Chatgpt/ChatgptAuthTokens/PersonalAccessToken builds a `BearerAuthProvider`
(`Authorization: Bearer`, `ChatGPT-Account-ID`, `X-OpenAI-Fedramp`); a custom
`base_url` is honoured.

The negative is the change. `auth.rs` is byte-identical B↔P (718 lines both).
At `3b45c29` (849 lines) `resolve_provider_auth` gains:

```rust
if !provider.requires_openai_auth && provider.auth.is_none() {
    return Ok(unauthenticated_auth_provider());   // R's auth.rs:205-207
}
```

— absent at B/P, where the function is bearer-check-then-fallback with no
flag read at all. `requires_openai_auth = false` with no
`env_key`/`experimental_bearer_token`/`auth`/`aws` does **not** suppress an
ambient credential at 0.146.0; the flag governs the login prompt and models
fetch, not header suppression, and the five probe/control tests the plan
cites as proof exist only in the `3b45c29` diff hunk. Nor is the caller side
a hidden gate: `AuthManager::shared_from_config`
(`login/src/auth/manager.rs:2321-2335` @B) loads `auth.json` regardless of
the flag and feeds it ungated into `resolve_provider_auth`
(`provider.rs:276-283, 171-178` @B); the one predicate that resembles the
`3b45c29` guard, `provider_uses_first_party_auth_path`
(`provider.rs:223-230` @B), only gates the Agent-Identity bootstrap path — so
`3b45c29` is a new suppression, not a relocation.

Where the flag does still gate: the account/login surface
(`account_state`, `provider.rs:285-325` @B, skips the TUI login screen,
`tui/src/lib.rs:1916, 2034` @B) and, with `requires_openai_auth = true` and
no login, silence — no `Authorization` at all, one auth-recovery retry on a
401 (`client.rs:2090-2110`); `exec` cannot refresh a ChatGPT token
(`exec/src/lib.rs:1739`).

## Claim 2 — `env_key` precedence; `validate()` does not reject the pair

### Verdict: **SAME (precedence holds at 0.146.0, for a different reason than at `3b45c29`); `validate()` SAME.**

`bearer_auth_for_provider` (`auth.rs:267-279` @B) runs `provider.api_key()`
then `experimental_bearer_token` before `resolve_provider_auth` ever touches
ambient `auth` — `env_key` wins over a forwarded login at B exactly as at R.
`api_key()` (`model-provider-info/src/lib.rs:286-302` @B): unset/whitespace
`env_key` is the one loud failure (`CodexErr::EnvVar`). `validate()`
(`lib.rs:174-212` @B, `:173-211` @P, byte-identical) rejects `aws` +
{`env_key`, `experimental_bearer_token`, `auth`, `requires_openai_auth`,
`supports_websockets`} and `auth` + {`env_key`, `experimental_bearer_token`,
`requires_openai_auth`} — **not** `env_key` + `requires_openai_auth = true`,
at either rev. The mutual exclusion is still ours to enforce.

## Claim 3 — `model_catalog_json`, the `/models` fetch, catalog schema

### Verdict: **key SAME (and the plan's line number matches B exactly); fetch mechanism SAME; pinning the catalog skips it entirely. But "only the `requires_openai_auth = true` route fetches `/models`" is CHANGED — the gate is the ambient auth mode, not the flag, so a BYOK stanza can fetch too.**

`config/src/config_toml.rs:355` @B (`:353` @P): `model_catalog_json:
Option<AbsolutePathBuf>`, resolved against the config base dir when relative
(`core/src/config/mod.rs:1909-1917` @B). `true` causes `GET
{base_url}/models` before the first turn, conditionally: session construction
always asks the models manager to list (`core/src/session/mod.rs:604-618`
@B), gated by `should_refresh_models()`
(`models-manager/src/manager.rs:413-414` @B — report's `:415-417`, off by 2,
immaterial) = `uses_codex_backend() ||
has_command_auth()` — true for
Chatgpt/ChatgptAuthTokens/Headers/AgentIdentity/PersonalAccessToken, false
for ApiKey/BedrockApiKey (`protocol/src/auth.rs:46-56` @B), against
`MODELS_ENDPOINT = "/models"` with resolved provider auth attached
(`models_endpoint.rs:39, 74-116` @B, byte-identical to P), 5 s timeout,
failures logged not fatal.

Pinning the catalog skips it completely — a swap, not a short-circuit:
`ModelProvider::models_manager` (`provider.rs:328-348` @B) returns
`StaticModelsManager` with no network path at all when `config.model_catalog`
is `Some` (`manager.rs:483-540` @B). The gate is not the flag, though:
`should_refresh_models()` never reads `requires_openai_auth`, only the
ambient `CodexAuth` in `CODEX_HOME` (`uses_codex_backend`,
`models_endpoint.rs:67-72` @B, same ungated `auth_manager.auth()` as claim 1)
— a BYOK stanza (flag `false` + `env_key`) on a box holding a ChatGPT
`auth.json` **will** fetch, carrying the `env_key` bearer. Both stanzas need
`model_catalog_json`, not just one.

Schema: `{"models": [ModelInfo, ...]}` (`protocol/src/openai_models.rs:600-604`
@B); `load_catalog_json` hard-errors on non-JSON or empty `models`
(`core/src/config/mod.rs:1934-1954` @B). `ModelInfo`
(`openai_models.rs:369-452` @B) has no `deny_unknown_fields` — extra keys
ignored, missing `Option<T>` default `None`. Twelve keys have no default and
aren't `Option` (`slug`, `display_name`, `supported_reasoning_levels`,
`shell_type`, `visibility`, `supported_in_api`, `priority`,
`base_instructions`, `support_verbosity`, `truncation_policy`,
`supports_parallel_tool_calls`, `experimental_supported_tools`); `shell_type` ∈
`default | local | unified_exec | disabled | shell_command`
(`openai_models.rs:277-284` @B) decides which shell tool the model sees
(claim 9). `ModelInfo` differs B↔P — validate against **B**.

## Claim 4 — `env_http_headers` and `http_headers`

### Verdict: **SAME.**

Both fields on `ModelProviderInfo` at B (`model-provider-info/src/lib.rs:115,
120`) and P, identical doc comments. `build_header_map` inserts
`http_headers` then `env_http_headers` (skipping unset/empty), on the
**provider**, independent of the auth provider — auth headers ride via
`AuthProvider::add_auth_headers`, so `X-Roundhouse-Key` sits beside a
pass-through `Authorization` unchanged. One 0.146.0-only wrinkle:
`uses_openai_actor_authorization()` (`lib.rs:408-416` @B) is
`!requires_openai_auth && http_headers` contains a non-empty
`OPENAI_ACTOR_AUTHORIZATION_HEADER` — M9 must not name a header that collides.

## Claim 5 — `wire_api = "responses"`; `supports_websockets`; unknown keys

### Verdict: **`wire_api` spelling SAME and now the ONLY legal value; `supports_websockets` SAME; unknown keys IGNORED by serde, but `--strict-config` turns them into errors.**

`WireApi` (`model-provider-info/src/lib.rs:55-84` @B) is one-variant:
`"responses"` → `Ok`, `"chat"` → named hard error, else → unknown-variant
error; identical at P. `supports_websockets: bool` (`#[serde(default)]`,
`lib.rs:139` @B/`:138` @P) also gates `Client::responses_websocket_enabled`
(`core/src/client.rs:938-946` @B) — leaving it false keeps M9 on SSE.
`ModelProviderInfo`/`ConfigToml` carry `#[schemars(deny_unknown_fields)]` —
schemars, not serde (`lib.rs:57`) — so an unrecognised `config.toml` key is
silently ignored at load; the attribute only feeds `--strict-config`
(`exec/src/cli.rs:19-21` @B, default `false`), which M9 controls.

## Claim 6 — `[features] use_agent_identity`

### Verdict: **SAME — key exists at 0.146.0, stage `UnderDevelopment`, `default_enabled: false`. Emitting it is safe; an unknown feature key only warns (or errors under `--strict-config`).**

`features/src/lib.rs:1440-1445` @B: `FeatureSpec { id: UseAgentIdentity, key:
"use_agent_identity", stage: UnderDevelopment, default_enabled: false }`; same
at `:1474-1479` @P. Confirmed live: `codex features list` prints
`use_agent_identity  under development  false`. Unknown key: `warn!` and
continue (`lib.rs:537` @B); hard error only under `--strict-config`. Also
live and `stable/true`, worth knowing for a hermetic run:
`remote_compaction_v2`, `unified_exec`, `shell_tool`, `multi_agent`, `apps`.

## Claim 7 — `is_openai()` name match

### Verdict: **SAME, and the blast radius is slightly LARGER at 0.146.0 than at the pin.**

`model-provider-info/src/lib.rs:404-406` @B: `self.name ==
OPENAI_PROVIDER_NAME` (`"OpenAI"`, `:35`) — exact, case-sensitive, against
**`name`**, not the table key. Consumers that misfire if named `"OpenAI"`:
`core/src/client.rs:849-853` stops stripping
`internal_chat_message_metadata_passthrough`; `client.rs:1370-1373`
(`responses_request_compression`) switches to zstd with ChatGPT-backed auth;
`core/src/compact.rs:108` gates remote compaction via
`supports_remote_compaction()`, which exists **only at B** (`lib.rs:421-424`,
absent at P) — one more reason "do not name it OpenAI" is load-bearing here.
Also gated: `session/turn.rs:900, 2034`, `realtime_conversation.rs:1576`,
plus TUI/web-search/image-generation extensions.

## Claim 8 — `[mcp_servers.*]` with `url` + `bearer_token_env_var`

### Verdict: **SAME — streamable HTTP, `Authorization: Bearer <$ENV>`.**

`config/src/mcp_types.rs:428-462` @B, `StreamableHttp { url,
bearer_token_env_var, http_headers, env_http_headers }`. TOML surface
`RawMcpServerConfig` (`mcp_types.rs:246-300` @B; `bearer_token_env_var` at
`:267`, drifts to `:307` @P — positions differ "slightly by struct" per the
fact-check, field/variant unchanged). `McpServerAuth` (`:131-137` @B): a
configured bearer token/header always takes precedence over ChatGPT/OAuth.
B→P adds pin-only material (`omit_tools_from`, `oauth_credential_name`) —
nothing M9 needs.

## Claim 9 — tool dispatch: `function_call`, `namespace`, MCP names, built-ins

### Verdict: **`namespace` field SAME; dispatch key `(namespace, name)` SAME; MCP model-visible name is `mcp__<server>__` + `<tool>` concatenated with NO separator — a `ToolName` *pair* on the wire, not one flat string; built-ins SAME.**

`FunctionCall` (`protocol/src/models.rs:861-877` @B): `id`, `name`,
`namespace: Option<String>`, `arguments: String` (raw JSON, never parsed),
`call_id`. Dispatch (`core/src/tools/router.rs:129-141` @B) builds
`ToolName::new(namespace, name)`; registry (`registry.rs:427-484` @B) keys on
the pair, a miss returns `FunctionCallError::RespondToModel` — model told "no
such tool", turn continues, a usable negative control. `Display for ToolName`
concatenates `"{namespace}{name}"` with no delimiter (`tool_name.rs:37-44`
@B). MCP naming (`codex-mcp/src/tools.rs` @B): prefix `"mcp__"` (`:22`),
delimiter `"__"` (`:225`), 64-byte max (`:226`); sanitised, deduplicated,
SHA1-suffixed on collision, truncated to fit — **M9 cannot guess a roundhouse
MCP tool's model-visible name from config alone**; read it back from the
request's `tools` array, or steer a built-in.

| name | where | note |
|---|---|---|
| `shell_command` | `handlers/shell/shell_command.rs:142` | visible when `shell_type` ∈ {default, local, shell_command} |
| `exec_command`/`write_stdin` | `unified_exec/process_manager.rs:1222`, `handlers/unified_exec/write_stdin.rs:35` | visible when `shell_type = unified_exec`; `shell_command` stays registered dispatch-only (`spec_plan.rs:698`) |
| `apply_patch` | `handlers/apply_patch.rs:331` | |
| `view_image` | `handlers/view_image.rs:68` | needs a real file path |
| `update_plan` | `spec_plan.rs:736` | gated on `config.update_plan_enabled` |
| `read_mcp_resource` et al. | `handlers/mcp_resource/*` | only registered when MCP servers exist |

No tool named `shell`/`container.exec` — legacy spelling `shell_command`;
params (`protocol/src/models.rs:1801-1822` @B) are `{"command": "...",
"workdir": null, "timeout_ms": 10000}` — **a single shell string, not
argv**. Cheapest forced steer: `shell_command` with `{"command":"echo
<marker>"}`, no MCP server needed, produces a `CommandExecutionItem`
(`command`, `aggregated_output`, `exit_code`, `status`); cost is it goes
through the sandbox (claim 13).

## Claim 10 — the resend path

### Verdict: **SAME in substance; do not assert bytes. `arguments` is verbatim, `namespace` survives, field ORDER is codex's, and `id` is DROPPED unless it contains an underscore.**

`arguments` stored as `String`, never parsed on resend (`models.rs:868-871`
@B) — returns character-for-character. `namespace` survives
(`skip_serializing_if`). Serialization follows struct order — `id, name,
namespace, arguments, call_id, internal_chat_message_metadata_passthrough` —
so byte comparison fails on ordering alone; **assert on parsed values.** `id`
dropped unless "prefixed": `prepare_response_items_for_request`
(`core/src/client.rs:927-933` @B, runs on every request path) drops any `id`
failing `split_once('_')` with non-empty halves
(`protocol/src/response_item_id.rs:36-39` @B) — `fc_abc123` survives, a bare
UUID does not. `internal_chat_message_metadata_passthrough` is stripped for
every non-`is_openai()` provider (`client.rs:849-853`). Missing outputs are
synthesised: `ensure_call_outputs_present`
(`core/src/context_manager/normalize.rs:20-120` @B) inserts an `"aborted"`
`FunctionCallOutput` after any unanswered `FunctionCall`, deterministic
synthetic id. No reordering beyond that insertion.

## Claim 11 — `response.completed.usage`

### Verdict: **SAME. Usage accumulates into a session total, drives auto-compaction and `turn.completed` JSON; an inflated number pushes the client toward compaction.**

`TokenUsage` (`protocol/src/protocol.rs:2056-2070` @B): `input_tokens`,
`cached_input_tokens`, `cache_write_input_tokens`, `output_tokens`,
`reasoning_output_tokens`, `total_tokens`. On `ResponseEvent::Completed`
(`core/src/session/turn.rs:2341-2374` @B): `total_token_usage += last;
last_token_usage = last` (`protocol.rs:2108-2112` @B), feeds
`ensure_auto_compact_window_server_prefill_from_usage`, then
`record_rollout_budget_usage` (can abort a turn under `rollout_budget`,
inert/`false` at 0.146.0). Auto-compaction reads the total against
`ModelInfo::auto_compact_token_limit()` (`min(configured, 90% of
context_window)`, `openai_models.rs:459-470` @B). Folding judge-side usage
into `response.completed.usage` inflates `turn.completed.usage`, the session
total, and brings compaction forward — silently and self-reinforcingly; it
does not error.

## Claim 12 — `codex exec` at 0.146.0

### Verdict: **mostly SAME, with two flag CHANGES in the "the plan assumes it exists" direction: `-a/--ask-for-approval` is NOT an `exec` flag, and `--full-auto` is a hidden removed-flag trap.**

Verified by running the binary. `-a/--ask-for-approval` exists only on the
top-level (TUI) `codex`; `exec` hard-codes `approval_policy:
Some(AskForApproval::Never)` (`exec/src/lib.rs:427` @B). `--full-auto` is a
hidden trap: `removed_full_auto` (`exec/src/cli.rs:42-50` @B, `hide = true`),
warns and maps to `SandboxMode::WorkspaceWrite`; at **P it is gone
entirely** — an unknown-argument error there. `CODEX_HOME`
(`utils/home-dir/src/lib.rs:14` @B) roots `config.toml`, `auth.json`,
profiles, arg0 temp dir — no `--config-file` flag; `-c key=value` overrides
work. `exec` reads `auth.json` from `CODEX_HOME` the same way;
`enforce_login_restrictions` (`exec/src/lib.rs:487-495` @B) fails only on a
forced login-method mismatch, not absent credentials — per claim 1, whatever
ambient `CodexAuth` resolves is still attached.

`exec/src/exec_events.rs` is byte-identical B↔P: `thread.started`,
`turn.started`, `turn.completed {usage}`, `turn.failed`,
`item.started|updated|completed {item}` (`agent_message`, `reasoning`,
`command_execution {command, aggregated_output, exit_code, status}`,
`file_change`), `error`. "Forced tool executed, final message" reads as
`item.completed` `command_execution` (`exit_code == 0`, marker in
`aggregated_output`), then `agent_message`, then `turn.completed`;
`-o/--output-last-message <FILE>` beats scraping JSONL.

## Claim 13 — sandbox under `codex exec` on Linux

### Verdict: **SAME mechanism (bubblewrap + seccomp via `codex-linux-sandbox` self-re-exec), default `read-only` ⇒ sandbox ON. A separate 0.146.0-specific hazard makes a `/tmp`-rooted `CODEX_HOME` actively dangerous for a sandboxed run.**

`get_platform_sandbox` (`sandboxing/src/manager.rs:60-74` @B) returns
`LinuxSeccomp`, re-execing under basename `codex-linux-sandbox`
(`landlock.rs:6, 18` @B). `SandboxMode` defaults `ReadOnly`
(`protocol/src/config_types.rs:86-89` @B); `should_require_platform_sandbox`
(`policy_transforms.rs:512-533` @B) is true for any `Restricted` policy
without full-disk write — a forced `shell_command` under default `exec` runs
inside bwrap+seccomp.

The 0.146.0-specific hazard: `prepare_path_entry_for_codex_aliases`
(`arg0/src/lib.rs:333-349` @B) refuses, in release builds, to create arg0
helper symlinks — including `codex-linux-sandbox` — when `CODEX_HOME` is
under `std::env::temp_dir()` (reproduced on every invocation with
`CODEX_HOME` under `/tmp/...`; only warns). `linux_sandbox_exe_path`
(`arg0/src/lib.rs:332-343` @B) then falls back to `current_exe` (basename
`codex`), and arg0 dispatch is by basename (`arg0/src/lib.rs:95`); whether
that still dispatches turns on a runtime probe for
`bwrap --argv0` support (`linux-sandbox/src/launcher.rs:101-121` @B) — M9
should not have to know which bubblewrap the box resolves, hence: don't root
`CODEX_HOME` under `/tmp`. `--dangerously-bypass-approvals-and-sandbox`
(`SandboxMode::DangerFullAccess`, `exec/src/lib.rs:296` @B) disables the
sandbox; approvals are already `Never` in `exec`.

## Claim 14 — the rest of the B↔P delta on the claim paths

### Verdict: **no other change alters how an agent hooks up, what a turn costs, or where a route can go. The two that touch M9's surface are `--full-auto` (claim 12) and `supports_remote_compaction` (claim 7).**

Byte-identical, SAME by construction: `model-provider/src/auth.rs`,
`model-provider/src/models_endpoint.rs`, `exec/src/exec_events.rs`.
`config/src/config_toml.rs` (102 diff lines): only at B, `[debug]`/lockfile
types (removed at P); only at P, `responses_api_metadata` and `[goals]
max_goal_token_budget` — a P-written config carrying `responses_api_metadata`
is silently ignored by 0.146.0; `model_catalog_json`/`model_providers`
unchanged. `exec/src/cli.rs`: `--full-auto` removed at P, `codex exec fork`
added — nothing M9 needs. `protocol/src/models.rs` (305 diff lines):
`FunctionCall`/`CustomToolCall` `namespace` and `FunctionCallOutputPayload`
unchanged. `core/src/client.rs` (202 diff lines): `prepare_response_items_for_request`,
the `is_openai` strip, `responses_request_compression`, 401 retry are
equivalent at both. As noted in Method, the report could not walk history
against the blobless clone; the file census and targeted `diff -u` hunks
stand in its place.

---

## Consequences for M9's generated config and harness

1. **[claim 1, doc-changing]** Drop PLAN §3's "unset attaches nothing" guarantee against this binary — true only for codex ≥ ~2026-08-14; needs a dated addendum plus the claim-3 correction.
2. **[claim 1]** Never write `requires_openai_auth = false` without `env_key` — at 0.146.0 that sends whatever ambient credential is logged in.
3. **[claims 1, 12]** Run the harness with a hermetic, credential-free `CODEX_HOME`, or the test silently forwards a real bearer.
4. **[claim 3, doc-changing]** Emit `model_catalog_json` in **both** generated stanzas — a BYOK stanza can fetch the catalog too.
5. **[claim 3]** Generate the catalog against 0.146.0's `ModelInfo`, twelve required keys, `{"models":[…]}`; validate against **B**.
6. **[claims 3, 9]** Pin `"shell_type":"shell_command"` deliberately — it picks the tool M9 steers.
7. **[claim 9]** Prefer a built-in `shell_command` steer over a synthetic MCP `function_call` — the MCP name is not derivable from config alone.
8. **[claim 9]** A registry miss on `(namespace, name)` is a soft failure — use it as the negative control.
9. **[claim 10]** Assert resent items on parsed values, never bytes — codex re-serializes in its own struct order.
10. **[claim 10]** Mint FunctionCall ids as `<prefix>_<suffix>` if echoed-back ids matter; expect a synthetic `"aborted"` output otherwise.
11. **[claim 13]** `CODEX_HOME` must not live under `/tmp` — release builds skip the `codex-linux-sandbox` alias there; use the worktree (e.g. `target/m9-codex-home/`) or `$HOME` instead.
12. **[claim 13]** For a hermetic forced steer, pass `--dangerously-bypass-approvals-and-sandbox` — default `read-only` depends on the box's user-namespace configuration.
13. **[claim 12]** Generate: `CODEX_HOME=<dir> codex exec --json -C <dir> --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox -o <file> "<prompt>"`. No `-a`, no `--full-auto`.
14. **[claims 5, 6]** Skip `--strict-config` unless unknown keys should be fatal — usable, not a default to inherit.
15. **[claim 7]** Provider `name` must not be `"OpenAI"` — blast radius is wider at 0.146.0 (zstd compression too). `name = "Roundhouse"` is correct.
16. **[claim 11]** Folding judge-side usage into `response.completed.usage` silently brings the client's auto-compaction forward.
17. **[claim 8]** The MCP stanza needs no change for 0.146.0 — same contract as at the pin.
18. **[general]** Re-read this against the actual test-box binary before M9 lands; `codex --version` belongs in the harness's assertions.

---

## Fact-check disposition

An independent Sonnet pass re-ran every check in claims 1, 3, 5, 6, 7, 8, 9,
10, 12, and 13 against the two full checkouts and the blobless `R` clone,
without reusing the report's own quoted output as evidence. Verdict on all:
**CONFIRMED**, with one correction and several immaterial drifts.

**The one correction, applied throughout above:** `resolve_provider_auth` —
the function this entire evidence base turns on — is at
`model-provider/src/auth.rs:179-196` @B, not `:198-215` as the original
report stated; `198-215` names the *next*, also short-circuit-free function,
`resolve_provider_auth_for_scope`. The substantive claim (no
`requires_openai_auth` gate at B/P) is unaffected — only the citation moves.

**Immaterial, left as stated:** `manager.rs:415-417` → actually `:413-414`;
`mcp_types.rs` field positions differ "slightly by struct" between the
report's and the fact-check's readings. The fact-check ruled these did not
warrant changing the document; no other claim required correction.
