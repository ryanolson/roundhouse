<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Prefix-anchored routing: measurements and code reads

> **Status: evidence base, 2026-09-29.** Read against Roundhouse `2dd40dd` (branch `ai/learner-m8-engine`), the Codex Cargo pin `6344a655a5966f92e009a74928fb0559b41f9093`, and the Dynamo Cargo pin `ac7b7513790ef1d619b46f805aea03c9f21200ba`. No code changed. No Cargo command ran. No paid provider was called. The ruling that uses this evidence is `../synergies/prefix-anchored-routing-proposal.md`. Add dated notes when an upstream changes. Do not silently revise this snapshot.

## 0. Result

- **Prefix admission uses no digest.** It compares items structurally with `same_item`. The one per-item encoding that three existing readers share is `Item::render()`. Structural agreement under `same_item` implies equal renders (section 2).
- **Claude Code sessions of one version diverge at canonical item 0**, the attribution block. That block carries a 12-bit fingerprint of three characters of the first typed prompt. It is a weak discriminator. The reliable divergence point is the typed prompt at item 4. In Anthropic cache order, two sessions share 45,990 bytes of tool schemas first (about 11.5k tokens, estimated at bytes/4) (section 3).
- **One Claude Code session changed its configuration run between two process invocations.** Item 2 (the main system prompt) and the `Bash` tool description lost the text ` (1M context)` between turn 1 and turn 2. Any hash that includes item 2 changes inside one session (section 5).
- **Codex sessions diverge at the first typed prompt**, canonical item 3, when the working directory, the date, and the configuration agree. They diverge at item 2 when the working directory or the date differ. This is derived from the pin's source and its own tests, not from a capture (section 6).
- **A Codex multi-agent v2 full-history fork is not a byte prefix of its parent.** The copy drops the root usage-hint developer message. Multi-agent v2 is off by default at the pin (section 7).
- **A Dynamo frontend can report the backend's cached-token count and the serving worker ids.** A Roundhouse decoder that reads that count can mark it `CacheReadSource::Provider`, which is the only source that the cache evidence counts as measured (sections 9 and 10).

| Client | Pair | First differing canonical item | Shared bytes, Roundhouse item order | Shared bytes, Anthropic order (tools, system, messages) |
|---|---|---|---|---|
| Claude Code 2.1.257 | Two sessions, different prompt, working directory, day, MCP tools | 0 (fingerprint), then 2 (working directory), 3 (date), 4 (prompt) | 60 | 45,990 |
| Claude Code 2.1.257 | One session, turn 1 to turn 2 (new process) | 2 (model name text) | 5,096 | 4,199 |
| Claude Code 2.1.257 | One session, turn 2 to turn 3 | none (append only) | 17,223 (all) | 63,667 |
| Claude Code 2.1.257 | One session, tool loop in one process | none (append only) | 17,249 (all) | 56,861 |
| Claude Code 2.1.251 against 2.1.257 | Same prompt, two versions | 0 (version string) | 58 | 396 |
| Codex (derived) | Two sessions, same working directory, day, configuration | 3 (prompt) | not measured | not applicable |

## 1. Method and claim tags

- **[rh path:line]**: Roundhouse source at `2dd40dd`, read with `git show 2dd40dd:<path>` because another agent mutates files in this tree.
- **[codex@6344a65 path:line]**: read in `~/.cargo/git/checkouts/codex-9eee5d47a939c68c/6344a65/codex-rs`. The Codex binary on this box is older and on another line (`codex-0.146.0-vs-pin-vigilance.md`).
- **[dynamo@ac7b751 path:line]**: read in `~/.cargo/git/checkouts/dynamo-66ea943fd73cd568/ac7b751`.
- **[fixture name]**: a Claude Code request body pinned in `crates/roundhouse-server/tests/fixtures/`.
- **[measured]**: an output of `prefix-divergence-measure.py`, which sits beside this document. Run it with the fixture directory as its only argument. It prints every number in sections 3 to 5.
- **[doc]**: a claim relayed from another research document. Its own evidence rules apply.

Limits of the measurement:

- The script approximates `messages_api::wire::canonicalize` [rh crates/roundhouse-server/src/messages_api/wire.rs:327] and `Item::render` [rh crates/roundhouse-core/src/item.rs:210, :419] in Python. Byte counts are for that approximation. They are not the bytes of the Rust render. The divergence positions do not depend on the difference, because one deterministic encoding is applied to every fixture alike.
- Token counts are estimates at bytes divided by 4. No tokenizer ran.
- The "Anthropic order" byte counts concatenate a sorted-key compact JSON of `tools`, `system`, and `messages`. They are not the bytes that the client sent on the wire.
- The Claude fixtures are synthetic prompts ("say hi", "call the status tool") sent to a loopback mock. Real prompts are longer. The positions of the shared preamble do not change with prompt length.

## 2. What Roundhouse hashes today

| Encoding | Where | What it covers | Keyed |
|---|---|---|---|
| `Item::render()` | [rh crates/roundhouse-core/src/item.rs:210, :419] | One item as `<|role|>` plus a content render. A tool call leaves out `namespace` [rh item.rs:233]. An opaque block renders as a SHA-256 of its canonical JSON [rh item.rs:289]. `response_id` is not part of it. | No |
| Turn id | `turn_id_for` [rh crates/roundhouse-server/src/responses_api/wire.rs:221] | FNV-1a over the concatenated renders of all items. The Messages surface delegates to it. | No |
| Token buffer | `ContextAssembler::push` [rh crates/roundhouse-core/src/context.rs:216] | `encode(item.render())` per item, then Dynamo block and sequence hashes over the tokens. | No |
| Admitted input tokens | `Engine::admitted_input_tokens` [rh crates/roundhouse-server/src/engine.rs:1918] | The same per-item render, tokenized, plus the tool declaration tokens [rh engine.rs:1971]. | No |
| `prefix_fingerprint` | [rh crates/roundhouse-server/src/request_context.rs:75] | SHA-256 over `serde_json` of `(role, content)` for the leading system, developer, and first user items, under `roundhouse-prefix-v1`. `namespace` is part of the serde form when present. | No |
| Prefix admission | `same_item` [rh crates/roundhouse-server/src/prefix_admission.rs:819] | No hash. Structural comparison of role and content. `response_id` is ignored. A stored `None` namespace agrees with any claimed namespace [rh prefix_admission.rs:879]. | Not applicable |

**The invariant a new digest needs.** `same_item(a, b)` compares role and content. The only relaxation is the namespace rule. `Item::render` leaves out both `namespace` and `response_id`. So `same_item(a, b)` implies `a.render() == b.render()`. The converse is false: two tool calls that differ only by a present namespace render alike. That is safe for a routing key, because `call_id` still separates two calls in one conversation. `prefix_fingerprint` does not have this property. A stored `None` namespace and a claimed `Some` agree under `same_item` and produce different serde forms.

**Where `prefix_fingerprint` goes.** When a Responses request has no `prompt_cache_key`, the fingerprint becomes `RequestContext::prompt_cache_key` [rh request_context.rs:43]. The engine forwards that value upstream as `prompt_cache_key` [rh engine.rs:3023]. `observe_context` compares it within one process [rh crates/roundhouse-server/src/conversations.rs:583, :609]. No log event stores it.

**Configuration and history are admitted under different rules.** A leading run of `Developer` items is turn configuration and is replaced in place, not compared strictly [rh crates/roundhouse-core/src/session.rs:261, :266], [rh prefix_admission.rs:781]. The Messages surface marks the leading system blocks as `Developer` [rh messages_api/wire.rs:472]. The Responses surface stores `instructions` as a `System` item [rh responses_api/wire.rs:46], so a Codex request has no configuration run and is admitted strictly. The Messages surface drops the trailing `<total_tokens>` budget notice [rh messages_api/wire.rs:393].

**Tools are not items.** The tool declaration is counted as input but is never part of an item, the admission check, or a block hash [rh engine.rs:1936-1960, the doc comment on `declaration_tokens`].

**Per-turn work that already exists.** The engine rebuilds the assembler from all session items on every turn [rh engine.rs:2135, :2020]. That call renders and tokenizes every item. `turn_id_for` renders every item of the claim a second time in the handler.

## 3. Claude Code: where two sessions diverge

Canonical items of `claude-2.1.257-turn-1.json` [measured]:

| Index | Role | Render bytes | Content |
|---|---|---|---|
| 0 | developer | 87 | `x-anthropic-billing-header: cc_version=2.1.257.1f2; cc_entrypoint=sdk-cli;` |
| 1 | developer | 75 | "You are a Claude agent, built on Anthropic's Claude Agent SDK." |
| 2 | developer | 9,670 | Main system prompt. Includes the working directory and the model name. |
| 3 | user | 314 | `<system-reminder>` with `Today's date is 2026-09-01.` |
| 4 | user | 14 | The typed prompt. |
| 5 | system | 7,106 | Interior system message: agent types and skills list. History, not configuration. |

Tools: 21 schemas, 45,991 bytes of compact JSON. The MCP session sends 23 schemas, 46,416 bytes.

Two sessions of 2.1.257 (`turn-1` against `mcp-turn-1`) [measured]:

- Item 0 differs at byte 60: fingerprint `1f2` against `d56`.
- Item 1 is equal.
- Item 2 differs at byte 2,505: the working directory path.
- Item 3 differs at byte 136: the date.
- Item 4 differs: the typed prompt.
- Item 5 is equal.
- The tool arrays differ only because the MCP session appends two tools at the end. The shorter array is a byte prefix of the longer one, except for its closing bracket.

**Item 0 is a weak discriminator.** The fingerprint is three hex characters (12 bits). `claude-code-client-surface.md` §4.4 read the formula from the 2.1.42 bundle: SHA-256 of a fixed salt, characters 4, 7, and 20 of the first user text, and the version, truncated to three hex characters. The script reproduces the observed suffixes `1f2`, `d56`, and `6bb` exactly from the typed prompt (not from the `<system-reminder>` block) and the version [measured]. Consequences:

- Two different prompts collide with probability 1/4,096 when their three sampled characters differ, and always when the characters agree.
- Every prompt shorter than 5 characters samples `0` at all three positions and maps to one value per version.
- The block is stable for the life of a conversation and changes when the first user message changes.

`claude-code-client-surface.md` §4.4 also relays a documentation claim that `api.anthropic.com` strips this block by position. That claim was not verified. Roundhouse stores the block as ordinary configuration and renders it first on every target.

## 4. Claude Code: two versions

`claude-2.1.251-turn-1.json` against `claude-2.1.257-turn-1.json`, same prompt "say hi" [measured]:

- Item 0 differs at byte 58: the version string.
- Items 2, 3, and 5 differ. The skills list and several tool descriptions changed between the versions.
- The tool arrays share only 396 bytes. `Agent` is the first tool and its description changed.

A client upgrade therefore starts a new prefix for every session, in both orders.

## 5. Claude Code: volatile content inside one session

`claude-2.1.257-turn-1.json` to `turn-2-continue.json` (one session id, a second process started with `--continue`) [measured]:

- Item 2 differs at byte 4,934. The text ` (1M context)` and `[1m]` are removed. This is the model name in the system prompt.
- The `Bash` tool description loses the same ` (1M context)` text. The tool arrays differ at byte 4,199.
- Items 0, 1, 3, 4, and 5 are equal. The date reminder at item 3 is history and is resent unchanged.
- A message-boundary chain matches 2 items (162 bytes). The Roundhouse item render shares 5,096 bytes before the edit.

`turn-2-continue.json` to `turn-3-continue.json`, and the tool loop inside one process (`mcp-turn-1` to `mcp-turn-2-toolresult`), are append-only [measured]. The chain matches every earlier item, and the tool arrays are equal.

The 2.1.251 pair shows the same edit at item 2, byte 4,929 [measured].

**What changes and where it sits.**

| Volatile content | Position | Admission rule | Changes inside one session |
|---|---|---|---|
| Attribution fingerprint | Item 0, configuration | Replaced in place | No, until the first user message changes |
| Working directory, model name | Item 2, configuration | Replaced in place | Yes, between process invocations |
| Date | Item 3, history | Strict | No. The first reminder is frozen in the transcript. |
| Budget notice `<total_tokens>` | Trailing system message | Dropped at canonicalization | Yes, every request |
| Tool descriptions (model name in `Bash`) | Tool array, not an item | Not compared | Yes, between process invocations |
| MCP tool list | End of the tool array | Not compared | When the MCP configuration changes |

## 6. Codex: where two sessions diverge (derived, not captured)

The request layout at the pin:

- `instructions` is the base prompt for the model. Candidate files at the pin are 6,647 to 23,988 bytes (`core/gpt_5_codex_prompt.md`, `core/prompt_with_apply_patch_instructions.md`, and `models-manager/prompt.md` at 20,903 bytes). Which file a given model uses was not traced. Roundhouse stores it as canonical item 0 (`System`).
- `input` starts with the initial context. `build_initial_context_with_world_state` emits, in this order: the developer bundle, each separate developer section, the multi-agent mode message, and one contextual user message [codex@6344a65 core/src/session/mod.rs:3449, :3610-3629].
- The contextual user message carries the `AGENTS.md` user instructions and `<environment_context>`. The environment context includes the working directory, the shell, `current_date` as `%Y-%m-%d`, and the time zone [codex@6344a65 core/src/session/world_state.rs:201-218], [codex@6344a65 core/src/context/world_state/environment.rs:15-44].
- The pin's own test asserts three input items on the first request, "permissions + cached contextual user prefix + user msg", and asserts that the second request repeats them as a prefix [codex@6344a65 core/tests/suite/prompt_caching.rs:278-366, :325-327].
- Configuration changes between turns are appended, not rewritten. The snapshot `core/tests/suite/snapshots/all__suite__model_visible_layout__model_visible_layout_turn_overrides.snap` shows a second request that keeps items 00 to 02 and appends the new personality, permissions, and environment context at 04 to 06. A date change is a diff that is appended [codex@6344a65 core/src/context/world_state/environment.rs:112, :141].

Canonical items for a Codex root thread (multi-agent v2 off):

| Index | Role | Content | Differs between sessions when |
|---|---|---|---|
| 0 | system | `instructions` | The model or the Codex version differs |
| 1 | developer | Permissions, skills, and other developer sections | The configuration differs |
| 2 | user | `AGENTS.md` and environment context | The working directory, the repository, or the date differs |
| 3 | user | The typed prompt | Always, except for identical prompts |

So N = 4 items makes a Codex root request unique when the working directory, the date, and the configuration agree. With multi-agent v2 on, a standalone usage-hint developer message comes before the contextual user message [codex@6344a65 core/src/session/tests.rs:9331-9350], and the typed prompt moves to item 4.

The tool schemas were not measured for Codex. A live capture at the pin line is open evidence (section 12).

## 7. Codex: the fork copy is not a byte prefix

- `spawn_agent` v2 accepts `fork_turns`, default `all` (from `program-identity-evidence.md` §3.3, fact-checked).
- For a full-history fork, the child history is the parent's rollout with three edits. `AgentMessage` items are removed. Developer messages that equal a multi-agent v2 usage-hint text, or that match the current-time reminder, are removed. The parent's developer instructions are replaced by sub-agent instructions when a role or an override applies [codex@6344a65 core/src/agent/control/spawn.rs:86-101, :684-700, :719-760].
- The root usage hint is a standalone developer message placed before the contextual user message (section 6). When the root carried it, the child's canonical items diverge from the parent's at that index, which is item 2.
- `MultiAgentV2` is `Stage::Stable` with `default_enabled: false` [codex@6344a65 features/src/lib.rs:1077-1082]. The v1 `Collab` feature is on by default [codex@6344a65 features/src/lib.rs:1071-1076]. A v1 spawn forks history only with `fork_context: true`, and the v2 usage-hint filter list is empty for v1 [codex@6344a65 core/src/agent/control/spawn.rs:684-700].

Consequence for a KV-overlap unit: a v2 full-history fork matches its parent through item 1 only. At message boundaries it is a new request. A v1 fork with `fork_context: true` keeps the parent's items apart from agent messages and reminders. Whether its prefix then matches the parent's was not captured.

## 8. Two prompt orders

- **Anthropic** caches `tools`, then `system`, then `messages` (`agent-session-context-claude-wire-addendum-2026-09-16.md` §5). A tool change invalidates the whole cache hierarchy.
- **Roundhouse's embedded local path** renders items only. Tools are not part of the local prompt [rh crates/roundhouse-core/src/context.rs:216]. The item render is described as "a placeholder pending per-model chat templates" [rh item.rs:210, doc comment].
- **A Dynamo frontend** applies the model's chat template to messages and tools. Where the template places the tools was not read. Most templates put them near the start of the system area.

In both orders that include tools, the tools come before the attribution block. Under Roundhouse's item order, the attribution block is the first content, so two Claude Code sessions share only 60 bytes [measured].

## 9. The Dynamo frontend surface at `ac7b751`

| Feature | Source | Use for this design |
|---|---|---|
| `x-dynamo-worker-instance-id` pins a decode worker | [dynamo@ac7b751 lib/llm/src/protocols/common/extensions.rs:239, :297-367] | A worker hint. Not used unless the owner rules for hints. |
| `x-tenant-id` | [dynamo@ac7b751 lib/llm/src/protocols/common/extensions.rs:245, :345] | A tenant label toward the pool. Its effect in the router was not traced. |
| `nvext.extra_fields: ["worker_id"]` returns `prefill_worker_id` and `decode_worker_id` | [dynamo@ac7b751 lib/llm/src/protocols/common/extensions.rs:163-175, :571] | Roundhouse can observe which instance served a request, after dispatch. |
| `usage.prompt_tokens_details.cached_tokens`, copied from the backend's `completion_usage` | [dynamo@ac7b751 lib/llm/src/protocols/openai/chat_completions/delta.rs:259-269], [lib/llm/src/preprocessor.rs:3075], [lib/llm/src/protocols/anthropic/stream_converter.rs:1029-1032] | An observed cache count from the engine. Whether every backend fills it was not established. A grep of `lib/mocker` for `prompt_tokens_details` found no producer. The mocker's vLLM server test reads only `prompt_tokens` and `completion_tokens` from `completion_usage` [dynamo@ac7b751 lib/mocker/servers/vllm/tests/sidecar.rs:153-154]. So the mocker at the pin does not appear to report cached tokens. |
| `x-dynamo-session-id`, `x-dynamo-parent-session-id`, `x-dynamo-session-final` | [dynamo@ac7b751 lib/llm/src/protocols/agents.rs:14-16] | Session metadata. The router reads no `session_id` for placement (`program-identity-evidence.md` §11, claim 6). |

## 10. Roundhouse state the design reuses

- **Cache ledger.** One `TargetState` per target per session, keyed by `Target::ledger_key` [rh crates/roundhouse-core/src/routing/ledger.rs:422], [rh crates/roundhouse-core/src/routing/mod.rs:135]. `expected_cached_tokens` is `hit_probability(elapsed, prefix) * prefix` [rh ledger.rs:569, :61]. `CacheModel` has `Deterministic`, `InactivityDecay`, and `Observed` [rh ledger.rs:36]. A local worker is `Observed` and never predicted by the ledger.
- **Cache evidence.** `CacheEvidence` sums predicted and observed ratios per model row [rh crates/roundhouse-core/src/metrics/cache_evidence.rs:22, :62]. Only `CacheReadSource::Provider` counts as measured. `Derived`, the local path's own credit, does not [rh crates/roundhouse-core/src/event.rs:105-125].
- **Correlation maps.** Generations, calls, and threads, with a memory and a Redis implementation and one contract suite [rh crates/roundhouse-core/src/control/correlation.rs:238]. Call bindings expire after 6 hours and thread bindings after 7 days [rh correlation.rs:153, :164]. `generation(key)` returns `None` for a key that no node ever committed, which is "never bound anywhere" [rh correlation.rs:247].
- **Generation memo.** A node-local write-through memo capped at 4,096 keys [rh crates/roundhouse-server/src/conversations.rs:182, :353].
- **Embedded fleet.** Roundhouse sends its own block and sequence hashes and never sets `pinned_worker` or `allowed_worker_ids` [rh crates/roundhouse-fleet/src/local.rs:131-132]. A quote returns `effective_prefill_tokens`, `longest_matched_tokens`, and `load` [rh local.rs:149-158].
- **Target kinds.** `Local { worker_id, dp_rank, model }` and `Frontier { provider, model }` only [rh routing/mod.rs:66]. No pool target exists.
- **Failover.** A local target does not fail over [rh engine.rs:2597].
- **Where a 429 is expressible.** Fair use refuses at the transport's admission, before the turn is spawned [rh crates/roundhouse-server/src/http.rs:645], [rh crates/roundhouse-server/src/engine/fair_use.rs:127]. Both surfaces spawn the turn and return the stream at once [rh crates/roundhouse-server/src/responses_api.rs:399, :466], [rh crates/roundhouse-server/src/messages_api.rs:439, :521].
- **Codex and 429.** `retry_429: false` in the default provider information [codex@6344a65 model-provider-info/src/lib.rs:267]. The retry policy retries a 429 only when that flag is set [codex@6344a65 codex-client/src/retry.rs:17, :29]. The Roundhouse fair-use refusal therefore uses the one body Codex reads, `error.type == "usage_limit_reached"` with `resets_at` [rh http.rs:686-702]. Whether the Anthropic SDK inside Claude Code honors `Retry-After` was not read.
- **Learner scope.** Arm assignment per session [rh crates/roundhouse-core/src/validate/arm.rs:105], exploration draw per session and response [rh crates/roundhouse-core/src/routing/learn/explore.rs:45], and `min_sessions` at the gate [rh crates/roundhouse-core/src/routing/learn/gate.rs:215].

## 11. Hash cost

- SHA-256 through Python `hashlib` (OpenSSL) on this aarch64 host, which reports the `sha2` CPU feature: 7.1 µs for 17,000 bytes and 241 µs for 600,000 bytes, about 2.4 GB/s [measured, one run, not repeated]. The Rust `sha2 0.10` crate was not benchmarked.
- A fresh Claude Code first request renders to about 17 KB of items plus 46 KB of tools. A digest of both takes about 26 µs at that rate.
- The engine already renders and tokenizes every item on every turn [rh engine.rs:2135]. Tokenizer throughput was not measured here.

## 12. Open evidence

| Question | What closes it |
|---|---|
| Codex canonical items and tool bytes from a live request at the pin line | A loopback capture in the style of `claude-code-wire-probe.py`, with text replaced by lengths and digests. |
| Codex v1 `fork_context: true` child: is its item run a prefix of the parent's? | A capture of a parent and a forked child against a loopback mock. |
| Claude Code Task sub-agent request | Still open (`agent-session-context-claude-wire-addendum-2026-09-16.md` §8). The design treats it as a new request, so it does not block. |
| Does every Dynamo backend fill `prompt_tokens_details.cached_tokens`? Does the mocker? | A real backend run against the frontend at the pin, and a full read of the mocker's output path. |
| Does `api.anthropic.com` strip the attribution block? | A first-party source. Only relevant to the frontier prediction. |
| Rust `sha2` throughput and tokenizer throughput on the serving host | The M1 benchmark in the proposal. |

## 13. Fact-check, 2026-09-29

An independent read-only re-derivation checked nine claims against Roundhouse `2dd40dd`, the Codex pin and the Dynamo pin, and reran `prefix-divergence-measure.py` against the fixtures. Every claim is verified. One is stronger than stated: the Dynamo mocker never constructs `PromptTokensDetails`, and its sidecar tests set `prompt_tokens_details: None` explicitly. The ledger follows.

| # | Claim | Ruling | Evidence | Correction |
|---|---|---|---|---|
| 1 | Prefix admission has no digest; compares with `same_item` (`prefix_admission.rs:819`) | VERIFIED | `same_item` is defined at `crates/roundhouse-server/src/prefix_admission.rs:819` in this tree (`fn same_item(stored: &Item, claimed: &Item) -> bool { stored.role == claimed.role && same_content(...) }`). No hash construction anywhere in the file; `same_content`/`same_namespace`/`same_items` are all structural comparisons. | The line number is exact for this checkout; the doc itself notes line numbers are `2dd40dd`-specific. |
| 2 | `Item::render()` leaves out `namespace` and `response_id`; `same_item`-equal implies render-equal, with no counterexample | VERIFIED | `crates/roundhouse-core/src/item.rs`: `Item::render` (line 419) = `<\|role\|>` + `content.render()` (line 210) — never touches `Item.response_id` (field at line 320). `ItemContent::render`'s `ToolCall` arm explicitly discards `namespace` (`namespace: _` at line 233). Enumerated every field `same_item`/`same_content`/`same_namespace` ignore or normalize: (a) `Item.response_id` — ignored by `same_item` (compares only `role`+`content`) and absent from `render()`; (b) `ItemContent::ToolCall.namespace` — normalized asymmetrically by `same_namespace` (stored `None` agrees with any claim) and excluded from `render()`. Every other field compared by `same_content`'s derived-`PartialEq` fallback (`Text.text`, `ToolResult.{call_id,output}`, `Thinking.{thinking,signature}`, `RedactedThinking.data`, `Opaque.{block_type,block}`) also appears in `render()`. No field is included in `render()` that `same_item` ignores, so no counterexample exists. | None — the evidence doc's own reasoning (item.rs:233 comment, item.rs:210 doc comment) matches; this check just enumerates it exhaustively rather than trusting the comment. |
| 3 | `Item::render()` is already used by the turn id, the token buffer, and the admitted token count | VERIFIED | Three call sites, exact: `turn_id_for` at `crates/roundhouse-server/src/responses_api/wire.rs:221`, loop body `item.render()` at `:228`. `ContextAssembler::push` at `crates/roundhouse-core/src/context.rs:216`, `self.tokenizer.encode(&item.render())` at `:217` (a fourth, non-doc-cited use at `:277` inside `rendered()`, same pattern). `Engine::admitted_input_tokens` at `crates/roundhouse-server/src/engine.rs:1918`, `item.render()` at `:1926` (and a second admitted-tokens helper at `:3459` doing the same). | None. |
| 4 | `prefix_fingerprint` is a second spelling of the same idea; where it's hashed and whether it becomes the forwarded `prompt_cache_key` | VERIFIED | `prefix_fingerprint` at `crates/roundhouse-server/src/request_context.rs:75`: SHA-256 of `b"roundhouse-prefix-v1\0"` + length-prefixed `serde_json::to_vec(&(role, content))` for each leading `System`/`Developer`/`User` item, stopping after (and including) the first `User` item. It becomes `RequestContext::prompt_cache_key` only as a fallback (`.unwrap_or(&prefix_fingerprint)` at `:43`) when the request supplied no `cache_key`. Forwarded upstream as `prompt_cache_key` at `engine.rs:3023` (`prompt_cache_key: request_context.map(|c| c.prompt_cache_key.clone()).unwrap_or_else(|| session_id.to_string())`). Compared within one process by `observe_context` at `conversations.rs:583` (fn def) / `:596,609` (comparison), an in-memory `HashMap` keyed by conversation key — not a durable log or event. No log event stores it (grepped the whole tree for `prefix_fingerprint`; only `request_context.rs` and `conversations.rs` reference it). | None. |
| 5 | `prefix-divergence-measure.py` reproduces the evidence doc's numbers | VERIFIED | Script contains no network imports/calls (`difflib`, `hashlib`, `json`, `sys`, `pathlib` only). Ran it against `crates/roundhouse-server/tests/fixtures` in this worktree. Output matches the doc exactly: item 0 diverges at byte 60 (`1f2`→`d56`) for "two sessions, one version"; item 4 (typed prompt) is in the differing set for that same pair, and is the *only* consistently-differing item across all compared pairs, i.e. the reliable divergence point; "Anthropic cache order" shared-bytes table shows `two sessions, one version (2.1.257 A vs B): shared 45990 B`, matching "45,990 bytes of shared tool definitions" exactly; "same session, next turn (2.1.257 t1->t2)" shows only item 2 differing (`' (1M context)'`/`'[1m]'` deleted), i.e. one session changing item 2 between two of its own turns. | None — every cited number reproduces bit-for-bit. |
| 6 | The Claude Code attribution block is a 12-bit fingerprint of the typed prompt; check the formula against the fixture | VERIFIED | Script's `fp()` computes `sha256("59cf53e54c78" + chars[4,7,20-or-pad] of the first typed-text block + version-base)[:3]` (3 hex chars = 12 bits) and reproduces the header's own `cc_version=...` suffix exactly for all 7 fixtures (`1f2`, `d56`, `6bb` each recovered from their own fixture). This matches the doc's description ("SHA-256 of a fixed salt, characters 4, 7, and 20 of the first user text, and the version, truncated to three hex characters") and its claim that the suffixes are reproduced "from the typed prompt (not from the `<system-reminder>` block)". | None. |
| 7 | Codex divergence points (item 3 vs item 2), v2 fork drops root usage-hint, multi-agent v2 off by default | VERIFIED | Pin confirmed at `6344a655a5966f92e009a74928fb0559b41f9093`. `build_initial_context_with_world_state` at `core/src/session/mod.rs:3449` emits, in order: developer bundle, separate developer sections, (v2 only) `initial_multi_agent_mode` message, contextual user message (`:3608-3628`) — matches the doc's claimed order exactly, including that the usage-hint sits *before* the contextual user message. `core/tests/suite/prompt_caching.rs::prefixes_context_and_instructions_once_and_consistently_across_requests` (function starts line 277) asserts `input1.len() == 3` with the literal message `"expected permissions + cached contextual user prefix + user msg"`, and asserts request 2's first 3 items equal request 1's 3 items — the N=3-input / N=4-total-canonical-item claim is exact. Snapshot `core/tests/suite/snapshots/all__suite__model_visible_layout__model_visible_layout_turn_overrides.snap` shows a second request keeping items `00`-`02` and appending new personality/permissions/environment at `04`-`06` — confirms "configuration changes are appended, not rewritten." Fork: `is_fork_excluded_developer_message` (`core/src/agent/control/spawn.rs`, ~line 86) drops any developer message matching `CurrentTimeReminder::matches_text` or a v2 usage-hint text; `retain_forked_item` (~line 719) also drops every `ResponseItem::AgentMessage`; the v2-only usage-hint filter list is built only `if multi_agent_version == MultiAgentVersion::V2` (~line 684), empty for v1. `features/src/lib.rs`: `Feature::MultiAgentV2` is `stage: Stage::Stable, default_enabled: false`; `Feature::Collab` (v1) is `stage: Stage::Stable, default_enabled: true`. Test `build_initial_context_adds_multi_agent_v2_root_usage_hint_as_developer_message` (`core/src/session/tests.rs`, ~line 9330) confirms the root usage hint is a standalone developer message. | None — every sub-claim reproduces exactly, including exact line-number citations (within a few lines, consistent with a large file where an unrelated earlier edit could shift line counts slightly; content and ordering match precisely). |
| 8 | The Dynamo mocker at the pin does not fill `cached_tokens` | VERIFIED, and the evidence for it is stronger than the doc states | Dynamo pin confirmed at `ac7b7513790ef1d619b46f805aea03c9f21200ba`. Grepped `lib/mocker` for `prompt_tokens_details`: two hits, both inside `lib/mocker/servers/{vllm,sglang}/tests/sidecar.rs`, both constructing `PrefillResult { ..., prompt_tokens_details: None }` — i.e. the mocker's own test harness explicitly nulls the field rather than merely omitting it. `PromptTokensDetails` (the `Some(...)` type) is never constructed anywhere under `lib/mocker`. The mocker's own usage assertion at `lib/mocker/servers/vllm/tests/sidecar.rs:153-154` (`let usage = terminal.completion_usage.as_ref().unwrap(); assert_eq!((usage.prompt_tokens, usage.completion_tokens), (4, 3));`) reads only `prompt_tokens`/`completion_tokens`, never `prompt_tokens_details`. | Minor strengthening, not a correction: the doc says "the mocker's vLLM server test reads only `prompt_tokens` and `completion_tokens`" and infers no producer of `cached_tokens` from that; the more direct evidence is that the mocker explicitly sets `prompt_tokens_details: None` where the field is populated at all. |
| 9a | `local.rs` never sets `pinned_worker` or `allowed_worker_ids` | VERIFIED | `crates/roundhouse-fleet/src/local.rs:131-132`: `pinned_worker: None, allowed_worker_ids: None,` — literal. | None. |
| 9b | A local target does not fail over (`engine.rs:2597`) | VERIFIED | Comment at `engine.rs` (the cited line range): "**A local target does not fail over**, and that is this rung's stated scope rather than an oversight: local capacity fails for reasons a second worker shares..." — verbatim in the source, not paraphrased. | None. |
| 9c | `Target` has only `Local` and `Frontier` variants, no pool target | VERIFIED | `crates/roundhouse-core/src/routing/mod.rs`: `pub enum Target { Local { worker_id, dp_rank, model }, Frontier { provider, model } }` — exactly two variants. | None. |
| 9d | `generation(key)` returns `None` for a key no node ever committed | VERIFIED | `crates/roundhouse-core/src/control/correlation.rs`, `CorrelationMaps::generation` doc comment: "The generation `key` was last committed at, or `None` if no node ever committed one." — near-verbatim match to the evidence doc's paraphrase. | None. |
| 9e | `retry_429: false` in Codex's default provider info; retry policy retries 429 only when set | VERIFIED | `model-provider-info/src/lib.rs:267`: `retry_429: false,` inside the constructed `ApiRetryConfig`. `codex-client/src/retry.rs:17` declares `pub retry_429: bool` on `RetryOn`; `:29` gates `(self.retry_429 && status.as_u16() == 429)` inside `should_retry`. Both cited line numbers are exact. | None. |
| 9f | Fair-use 429 body uses `error.type == "usage_limit_reached"` with `resets_at`; codex hardcodes `retry_429: false` everywhere | VERIFIED | `crates/roundhouse-server/src/http.rs` (cited range): a code comment states this exact rationale and cites `codex-api::api_bridge::map_api_error`, and the JSON body constructed a few lines later is `{"type": "usage_limit_reached", "scope": ..., "window": ..., "quantity": ..., ...}`. | The claim that the Anthropic SDK's `Retry-After` handling "was not read" is honestly flagged as open in the evidence doc itself — not re-checked here, out of scope for a 10-minute negative check. |

## Summary of anything not a clean VERIFIED

Every claim checked — 1 through 9 (1, 2, 3, 4, 5, 6, 7, 8, and the six spot-checked negatives in 9) — rules
**VERIFIED**. None ruled PARTIALLY, REFUTED, or UNCHECKABLE. The one place evidence is *stronger* than the
document states (not a correction, a tightening): claim 8's Dynamo mocker check. The doc says the mocker's
test "reads only `prompt_tokens` and `completion_tokens`" and infers no producer of `cached_tokens` from
that; direct inspection shows the mocker explicitly constructs `prompt_tokens_details: None` in the two
places (`lib/mocker/servers/vllm/tests/sidecar.rs`, `lib/mocker/servers/sglang/tests/sidecar.rs`) where that
field is populated at all, and `PromptTokensDetails::Some(...)` is never constructed anywhere under
`lib/mocker`. This is a tightening of the same negative, not a different finding.
