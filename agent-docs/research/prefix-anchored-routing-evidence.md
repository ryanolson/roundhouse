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

- `spawn_agent` v2 accepts `fork_turns`, default `all` (from `session-identity-evidence.md` §3.3, fact-checked).
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
| `x-dynamo-session-id`, `x-dynamo-parent-session-id`, `x-dynamo-session-final` | [dynamo@ac7b751 lib/llm/src/protocols/agents.rs:14-16] | Session metadata. The router reads no `session_id` for placement (`session-identity-evidence.md` §11, claim 6). |

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

#### Summary of anything not a clean VERIFIED

Every claim checked — 1 through 9 (1, 2, 3, 4, 5, 6, 7, 8, and the six spot-checked negatives in 9) — rules
**VERIFIED**. None ruled PARTIALLY, REFUTED, or UNCHECKABLE. The one place evidence is *stronger* than the
document states (not a correction, a tightening): claim 8's Dynamo mocker check. The doc says the mocker's
test "reads only `prompt_tokens` and `completion_tokens`" and infers no producer of `cached_tokens` from
that; direct inspection shows the mocker explicitly constructs `prompt_tokens_details: None` in the two
places (`lib/mocker/servers/vllm/tests/sidecar.rs`, `lib/mocker/servers/sglang/tests/sidecar.rs`) where that
field is populated at all, and `PromptTokensDetails::Some(...)` is never constructed anywhere under
`lib/mocker`. This is a tightening of the same negative, not a different finding.

## 14. Deployment load and session detection, 2026-09-29

This section supports the addendum "design revision after the second ruling" in `../synergies/prefix-anchored-routing-proposal.md`. It was read against the same revisions as the rest of this document: Roundhouse `2dd40dd`, Dynamo `ac7b751`, and Codex `6344a65`. No code changed. No Cargo command ran. No deployment ran. The claim tags are the tags of section 1.

### 14.0 Result

- **No Dynamo surface gives a caller outside a deployment one number for the KV load of that deployment.** The frontend `/health` lists instances and no load. `/busy_threshold` returns thresholds only (sections 14.1 and 14.2).
- **The frontend `/metrics` exposes the KV router's own view of in-flight KV blocks per worker**, as `dynamo_frontend_worker_active_decode_blocks`. The router sets it at each request lifecycle event. It exists only when the frontend runs the KV router (section 14.3).
- **The workers publish their engine-reported used blocks (`kv_used_blocks`) on the event plane.** The frontend receives them and uses them for its overload gate, but exports them on no HTTP surface (section 14.4).
- **The event plane needs a Dynamo `DistributedRuntime`.** Roundhouse deliberately carries no `dynamo-runtime` (section 14.5).
- **The capacity gauge has only a `model` label.** It holds the block count of one worker, not a sum (section 14.6).
- **A Dynamo frontend refuses an overload with HTTP 529 by default, and sends no `Retry-After`** (section 14.8).
- **Roundhouse derives session labels in two server modules today.** No code reads `user-agent`, `originator`, or the attribution block as a client signal (section 14.10).

### 14.1 Frontend routes that a caller outside the deployment can reach

| Route | Source | What it returns | Load? |
|---|---|---|---|
| `/metrics` | [dynamo@ac7b751 lib/llm/src/http/service/metrics.rs:2175-2190], mounted with the system routes [dynamo@ac7b751 lib/llm/src/http/service/service_v2.rs:1167-1184] | Prometheus text of the frontend registry | Yes, per worker (sections 14.3, 14.6) |
| `/health` | [dynamo@ac7b751 lib/llm/src/http/service/health.rs:63-98] | 503 until ready, then the instance list from discovery | No |
| `/live` | [dynamo@ac7b751 lib/llm/src/http/service/health.rs:40-61] | 503 while shutting down, else 200 | No |
| `/busy_threshold` (GET, POST) | [dynamo@ac7b751 lib/llm/src/http/service/busy_threshold.rs:1-45], mounted only when the admin API is on [service_v2.rs:1185-1194] | The configured thresholds per model | No. Thresholds, not the loads they compare. |
| `/v1/models` | [service_v2.rs:1174-1181] | The model list | No |

The frontend all these routes are on is the same HTTP service as the inference API, so a caller that can dispatch to the deployment can also scrape it.

### 14.2 The standalone selection service

- A deployment that runs the kv-router selection service as an HTTP server exposes `GET /loads` [dynamo@ac7b751 lib/kv-router/src/services/selection/server.rs:330]. It returns one `ModelLoadResponse` per model and routing group, with a `PotentialLoad` per worker and rank [dynamo@ac7b751 lib/kv-router/src/services/selection/core/mod.rs:1125-1152], [dynamo@ac7b751 lib/kv-router/src/services/selection/types.rs:546-552].
- `PotentialLoad` carries `potential_prefill_tokens`, `potential_decode_blocks`, and `active_requests` [dynamo@ac7b751 lib/kv-router/src/protocols.rs:613-620]. With an empty prompt, "potential" is the current load.
- It carries no capacity. Roundhouse's embedded fleet reads the same call in process and uses only `potential_prefill_tokens`. Its comment says that a capacity denominator (`total_kv_blocks`) is not in the catalog [rh crates/roundhouse-fleet/src/local.rs:410-425].
- This surface is not the standard Dynamo frontend. A deployment has it only if it runs that service.

### 14.3 The router-side gauge: `dynamo_frontend_worker_active_decode_blocks`

- **Definition.** `WorkerLoadMetrics` holds two gauges, `dynamo_frontend_worker_active_decode_blocks` and `dynamo_frontend_worker_active_prefill_tokens`, with labels `worker_id`, `dp_rank`, `worker_type` [dynamo@ac7b751 lib/llm/src/kv_router/metrics.rs:472-526]. The frontend registers them on its HTTP registry [dynamo@ac7b751 lib/llm/src/http/service/service_v2.rs:1108-1116].
- **Who sets it.** The KV router's active-sequence tracker. `observe_worker_load_snapshot` computes the worker's active blocks and calls `observe_load` [dynamo@ac7b751 lib/kv-router/src/sequences/multi_worker.rs:519-538], which sets the gauges [dynamo@ac7b751 lib/llm/src/kv_router/sequence.rs:155-169]. It runs after add, prefill completion, output blocks, and free [multi_worker.rs:905, :1078, :1204, :1264, :1289].
- **Correction to a Dynamo comment.** The comment at [service_v2.rs:1112-1113] says that `KvWorkerMonitor` updates these gauges. It does not. The monitor only removes them when a worker leaves [dynamo@ac7b751 lib/llm/src/discovery/worker_monitor.rs:60-80]. The router's sequence tracker sets them.
- **Freshness.** The gauge moves in the same call that books or frees the request. A scrape therefore lags only by its own interval.
- **What it measures.** Blocks of in-flight requests as the router predicts them, not blocks the engine reports. Three defaults shape it [dynamo@ac7b751 lib/kv-router/src/scheduling/config.rs:810-813]:
  - `router_track_active_blocks: true`. The gauge exists.
  - `router_track_output_blocks: false`. Output blocks are not added. **The gauge under-counts decode growth.**
  - `router_assume_kv_reuse: true`. Shared prefix blocks among in-flight requests are counted once.
- **Several frontends.** `router_replica_sync` is `false` by default [config.rs:810]. Each frontend then sees only its own traffic. With replica sync on, a frontend applies peer events and calls the same `observe_worker_load_snapshot` [dynamo@ac7b751 lib/kv-router/src/sequences/replica_sync.rs:265-278]. So each frontend gauge then shows the peer traffic too. That the peer view is complete was not run.
- **Only in KV router mode.** Nothing else sets the gauge. A frontend in round-robin or random mode exposes the metric name with no series.
- **Unverified.** That a Dynamo mocker behind a KV-router frontend fills this gauge was not run. The router sets it without regard to the backend, so it is expected.

### 14.4 The worker-reported signal: `kv_used_blocks`

- **Definition.** `ActiveLoad` has `kv_used_blocks: Option<u64>`, "Total KV blocks currently in use on the worker … the authoritative signal for backend KV occupancy used by overload detection" [dynamo@ac7b751 lib/kv-router/src/protocols.rs:671-690].
- **Transport.** A worker publishes it on the event plane subject `kv_metrics` [dynamo@ac7b751 lib/llm/src/kv_router.rs:215], [dynamo@ac7b751 lib/llm/src/kv_router/publisher/worker_metrics.rs:59-63]. The publisher sends only on change, after a 1 ms debounce [worker_metrics.rs:71-110].
- **Producers.** vLLM sets it to `num_gpu_block * scheduler_stats.kv_cache_usage` in each stat-logger record [dynamo@ac7b751 components/src/dynamo/vllm/publisher.py:52-65]. TensorRT-LLM publishes its active block count [dynamo@ac7b751 components/src/dynamo/trtllm/publisher.py:687-701]. SGLang publishes its own value [dynamo@ac7b751 components/src/dynamo/sglang/publisher.py:229].
- **Unverified: what vLLM counts as used.** Whether vLLM's `kv_cache_usage` counts evictable prefix-cached blocks as used was not read. The vLLM source is not in the pin. If it does, the value climbs toward 1.0 on every warm deployment and stops separating deployments.
- **The frontend receives it.** `KvWorkerMonitor` subscribes to `kv_metrics` [worker_monitor.rs:739]. It stores `kv_used_blocks` and `kv_total_blocks` per rank in `WorkerLoadState` [worker_monitor.rs:256-263, :351-371]. It fills `kv_total_blocks` from the worker's runtime configuration [worker_monitor.rs:870-890].
- **The frontend does not export it.** No gauge is set from `WorkerLoadState`. The monitor uses it only for the overload set [worker_monitor.rs:92-112, :386-420].
- **The mocker.** `kv_used_blocks` has no producer under `lib/mocker/src` (a grep for the name found none).

### 14.5 The event plane is not reachable without the Dynamo runtime

- `EventSubscriber::for_endpoint` and `EventPublisher::for_endpoint` take an `Endpoint` and use its `DistributedRuntime` for the transport and the scope [dynamo@ac7b751 lib/runtime/src/transports/event_plane/mod.rs:287-307]. The transports are NATS and ZMQ [dynamo@ac7b751 lib/runtime/src/transports/event_plane/]. Discovery is part of the runtime.
- Roundhouse deliberately depends on neither `dynamo-llm` nor `dynamo-runtime` [rh Cargo.toml:41-46].
- Dynamo's own consumers of load read it from inside the deployment. The planner subscribes to forward-pass metrics (`forward-pass-metrics`) on the event plane [dynamo@ac7b751 components/src/dynamo/planner/environment/metrics_provider/runtime_provider.py:131-160], [dynamo@ac7b751 lib/llm/src/fpm_publisher.rs:30]. The experimental ThunderAgent router reads capacity from model cards through the same runtime [dynamo@ac7b751 components/src/dynamo/thunderagent_router/capacity.py:18-87].
- Dynamo's global router, the one Dynamo layer above several pools, selects a pool from a configured grid over ISL, TTFT, and ITL targets, not from load [dynamo@ac7b751 components/src/dynamo/global_router/pool_selection.py:153, :224, :308], [dynamo@ac7b751 components/src/dynamo/global_router/README.md:5-14].

### 14.6 Capacity and block size on the frontend

- `dynamo_frontend_model_total_kv_blocks` has one label, `model`. It is "Total KV cache blocks available for a worker serving the model" [dynamo@ac7b751 lib/llm/src/http/service/metrics.rs:912-918]. Each runtime-configuration update overwrites it [metrics.rs:1200-1208]. **It is the capacity of one worker, last writer wins.** In disaggregated serving, a prefill worker can be the last writer. This was not traced to a run.
- The runtime value is per data-parallel rank. vLLM divides its block count by the rank count [dynamo@ac7b751 components/src/dynamo/vllm/main.py:716].
- `dynamo_frontend_model_kv_cache_block_size` gives the block size per model [metrics.rs:950].
- Each worker also exposes `dynamo_component_total_blocks` and `dynamo_component_gpu_cache_usage_percent` per rank on its own system port [dynamo@ac7b751 components/src/dynamo/common/utils/prometheus.py:382-395], [dynamo@ac7b751 lib/runtime/src/metrics/prometheus_names.rs:821-827]. A caller must know every worker address to read them.

### 14.7 Summary of load surfaces

| Surface | Quantity | Scope | Freshness | How Roundhouse reads it |
|---|---|---|---|---|
| Frontend `/metrics`, `worker_active_decode_blocks` | Router-predicted in-flight blocks, prompt blocks only by default | Per worker and rank, one frontend's view | Each lifecycle event | Pull (HTTP scrape) |
| Frontend `/metrics`, `model_total_kv_blocks` | Blocks of one worker | Per model | Each runtime configuration | Pull |
| Frontend `/metrics`, `model_kv_cache_block_size` | Tokens per block | Per model | Each card | Pull |
| Event plane `kv_metrics`, `kv_used_blocks` | Engine-reported used blocks | Per worker and rank | Each engine stat record, 1 ms debounce | Subscription, needs `dynamo-runtime` |
| Event plane `forward-pass-metrics` | Scheduled and queued KV tokens per forward pass (vLLM, mocker) | Per worker | Each forward pass, 1 s idle heartbeat | Subscription, needs `dynamo-runtime` |
| Worker system `/metrics` | `gpu_cache_usage_percent`, `total_blocks` | Per worker and rank | Each engine stat record | Pull, per worker address |
| Selection service `GET /loads` | Potential prefill tokens and decode blocks | Per worker and rank | Each lifecycle event | Pull, only where that service runs |
| `/health` | Instance list | Deployment | On request | Pull. No load. |

### 14.8 Overload refusal at the frontend

- The frontend refuses with the status in `DYN_HTTP_OVERLOAD_STATUS_CODE`, default 529 [dynamo@ac7b751 lib/llm/src/http/service/error.rs:10-28]. It does so when all workers of a model are over their busy thresholds [dynamo@ac7b751 lib/llm/src/http/service/busy_threshold.rs:5-9].
- A grep of `lib/llm/src` for `retry-after` (any case) found nothing. **The frontend sends no `Retry-After`.**
- The busy gate is off unless a threshold is set [worker_monitor.rs:598-605].

### 14.9 Codex client signals at the pin

- Codex sends `session-id` and `thread-id` from `build_session_headers` [codex@6344a65 codex-rs/codex-api/src/requests/headers.rs:5-14] and `x-openai-subagent` for a sub-agent [codex@6344a65 codex-rs/codex-api/src/endpoint/responses.rs:92-93].
- The default originator is `codex_cli_rs` [codex@6344a65 codex-rs/login/src/auth/default_client.rs:40]. `default_headers` inserts `originator` and a Codex `user-agent` [default_client.rs:330-340].
- **Unverified.** Whether the model client for a custom provider base URL sends `default_headers` was not traced. A loopback capture closes it.

### 14.10 Roundhouse session derivation at `2dd40dd`

| Fact | Source |
|---|---|
| The Messages label reads `x-claude-code-agent-id`, then `x-claude-code-session-id`, then `metadata.user_id`. It returns `None` when none is present. | `session_key` [rh crates/roundhouse-server/src/messages_api/wire.rs:236-255], constants [wire.rs:67, :78] |
| The label is scoped as `anthropic_messages/{session}` or `anthropic_messages/{session}/agent/{agent}` | `scoped` [wire.rs:286-291], `DIALECT_NAMESPACE` [wire.rs:101] |
| `session_component` parses both shipped `metadata.user_id` shapes | [wire.rs:299-323] |
| The Responses label is `thread-id`, then `session-id`, then `prompt_cache_key`. None of the three gives a 422. | `RequestContext::from_request` [rh crates/roundhouse-server/src/request_context.rs:23-47], `conversation_key` [request_context.rs:49-54] |
| `prefix_fingerprint` is the fallback `prompt_cache_key` | [request_context.rs:43, :75-90] |
| An anonymous Messages request gets a fresh key per request | `anonymous_key` [rh crates/roundhouse-server/src/messages_api.rs:546-552] |
| The principal prefix is added by `ControlPlane::qualify` | [rh crates/roundhouse-server/src/control_config/mod.rs:902-907] |
| **A second spelling of the dialect namespace** is in core, so that core can tell a Messages session from its key | `MESSAGES_SESSION_SEGMENT` [rh crates/roundhouse-core/src/validate/control_call.rs:142-151, :197] |
| `CreateMessageParams` is a server type | [wire.rs:120] |
| The attribution block is `system[0]` in the fixtures, `x-anthropic-billing-header: cc_version=2.1.257.1f2; cc_entrypoint=sdk-cli;` | [fixture claude-2.1.257-turn-1.json] |
| A test pins the attribution block as an ordinary canonical item, so that no strip happens at canonicalization | `the_live_client_body_canonicalizes_block_by_block` [wire.rs:814] |
| Leading system blocks become `Developer` items | `mark_turn_configuration` [wire.rs:472] |
| No code under `crates/*/src` reads `user-agent`, `originator`, `cc_version`, or `x-anthropic-billing-header` as a signal | `git grep` at `2dd40dd`. Hits are doc comments and tests only. |
| `roundhouse-core` has no `http`, `axum`, or `redis` dependency, but has `tokio` and `dynamo-kv-router` | [rh crates/roundhouse-core/Cargo.toml:11-32] |
| `item.rs` has no async code | `git grep` for `async fn`, `.await`, `tokio` in [rh crates/roundhouse-core/src/item.rs]: no hits |

**The fixtures.** `crates/roundhouse-server/tests/fixtures/` holds 11 files. Eight are Claude Code request bodies. Three are header captures, each a JSON array of `{path, headers}`.

- The header captures carry `user-agent: claude-cli/2.1.257 (external, sdk-cli)`, `x-app: cli`, and `x-claude-code-session-id` [fixture claude-2.1.257-headers.json].
- `messages_api_surface.rs` includes the bodies and two header captures by relative path [rh crates/roundhouse-server/tests/messages_api_surface.rs:141-152, :209, :4501-4502].
- `claude_launch/control_surface.rs` includes the MCP wire capture [rh crates/roundhouse-server/src/claude_launch/control_surface.rs:219].
- `claude-2.1.257-mcp-headers.json` is included by no test.

### 14.12 The router's predicted hit rate

- At routing, the KV router records the overlap blocks and the input blocks of each request, and observes their ratio in the `kv_hit_rate` histogram of its request metrics [dynamo@ac7b751 lib/llm/src/kv_router/push_router.rs:327-339], [dynamo@ac7b751 lib/llm/src/protocols/common/timing.rs:342-350].
- The histogram is named with `router_metric(KV_HIT_RATE)`, that is `router_kv_hit_rate`, "Predicted KV cache hit rate at routing time (0.0-1.0)", on the runtime metrics registry [dynamo@ac7b751 lib/llm/src/kv_router/metrics.rs:57-59, :921-928].
- **Unverified.** The full exposed name, and whether the frontend `/metrics` includes it, were not run. The frontend includes the runtime registry only when `drt_metrics` is set [dynamo@ac7b751 lib/llm/src/http/service/metrics.rs:2170-2172].
- The same ratio is also returned per request. `nvext.extra_fields: ["timing"]` selects the timing info [dynamo@ac7b751 lib/llm/src/protocols/common/extensions.rs:571-572], and that info carries `kv_hit_rate` [timing.rs:660-690].
- It is the router's prediction, not a measurement by the engine.

### 14.11 Open evidence added by this section

| Question | What closes it |
|---|---|
| Does vLLM's `kv_cache_usage` count evictable prefix-cached blocks as used? | A read of the vLLM block pool at the version the Dynamo pin installs, or a run that holds a warm prefix with no request in flight. |
| Does a mocker behind a KV-router frontend fill `worker_active_decode_blocks`? | The M3 stub run in the addendum. |
| With `router_replica_sync` on, does each frontend's gauge include all peer traffic? | A two-frontend mocker run. |
| In disaggregated serving, which worker last writes `model_total_kv_blocks`? | A disaggregated run, or catalog capacity (the addendum makes the catalog value win). |
| Does Codex send `originator` to a custom provider? | A loopback capture at the pin line. |
| Which `worker_type` label does the router give an aggregated worker? | A captured frontend exposition in M3. |
| Is `router_kv_hit_rate` on the frontend `/metrics`, and under which full name? | The M3 mocker run. |

### 14.13 Fact-check of section 14, 2026-09-29

An independent read-only re-derivation verified all eight checked claims against the same pins. The Codex citations above first lacked the `codex-rs/` prefix. They are corrected. The crate-cycle argument is verified in its premises only, because the crate does not exist yet. The ledger follows.

| # | Claim | Ruling | Evidence | Correction |
|---|---|---|---|---|
| 1 | No Dynamo frontend surface gives an outside caller one KV-load number; `/health` lists instances only; `/busy_threshold` returns thresholds only | VERIFIED | Exhaustive `grep -rn '\.route('` over `lib/llm/src/http/service/` finds only: metrics, busy_threshold (get/post), health, live, realtime ws, anthropic messages/models, generate, and the full OpenAI set (completions/chat/embeddings/classify/pooling/batches/models/responses/images/videos/audio). None of the inference or admin endpoints return an aggregate load number. `busy_threshold.rs:5-13` doc comment confirms thresholds-only; `health.rs` returns instance list / readiness only. | None. |
| 2 | `dynamo_frontend_worker_active_decode_blocks`: per-worker, set by KV router on every request lifecycle event, only exists in KV-router mode, excludes output blocks by default, per-frontend view without replica sync | VERIFIED | `WorkerLoadMetrics` labels `worker_id, dp_rank, worker_type` at `lib/llm/src/kv_router/metrics.rs:472-526` (exact). `observe_load` at `sequence.rs:155-169` (exact), called from `observe_worker_load_snapshot` at `multi_worker.rs:519-536` (evidence said 519-538, off by 2 lines, immaterial), called at lines 905, 1078, 1204, 1264, 1289 (all confirmed by grep) and also via `replica_sync.rs:264-268` (evidence said 265-278, function `flush_replica_batch_effects` confirmed calling the same method). Defaults at `config.rs:810-813` confirmed byte-for-byte: `router_replica_sync: false`, `router_track_active_blocks: true`, `router_track_output_blocks: false`, `router_assume_kv_reuse: true`. The "correction to a Dynamo comment" claim is also verified: `service_v2.rs:1113` literally says "updated by KvWorkerMonitor when receiving ActiveLoad events", but `worker_monitor.rs`'s only touch of `WORKER_LOAD_METRICS` is `cleanup_worker_metrics` (remove on worker departure, lines ~60-78); the actual `.set()` happens in the router's sequence tracker, not the monitor. | None. |
| 3 | Workers publish `kv_used_blocks` only on the event plane, no HTTP/metrics export | VERIFIED | `ActiveLoad.kv_used_blocks` doc comment at `protocols.rs:684-690` matches verbatim: "Total KV blocks currently in use on the worker ... the authoritative signal for backend KV occupancy used by overload detection." Publish subject `KV_METRICS_SUBJECT = "kv_metrics"` at `kv_router.rs:215` (exact). `grep -rn kv_used_blocks lib/llm/src/http/` returns zero hits — confirms no HTTP export. Frontend receiver `WorkerLoadState.kv_used_blocks`/`kv_total_blocks` fields confirmed at `worker_monitor.rs:256-263` (exact), filled from runtime config at `worker_monitor.rs:870-890` region (exact match of the `total_kv_blocks` population loop), subscription confirmed at `worker_monitor.rs:739` (exact — `EventSubscriber::for_endpoint(endpoint, KV_METRICS_SUBJECT)`). Mocker producer check: `grep -rn kv_used_blocks lib/mocker/src/` returns zero hits, confirming "no producer under `lib/mocker/src`." | None. |
| 4 | Capacity gauge (`model_total_kv_blocks`) is one worker's value, last writer wins | VERIFIED | `metrics.rs:908-918` (evidence cites 912-918, close): single label `["model"]`, doc string verbatim "Total KV cache blocks available for a worker serving the model". `update_runtime_config_metrics` (called at `metrics.rs:1227` from `update_metrics_from_mdc`) does an unconditional `.with_label_values(&[model_name]).set(...)` on every call — confirmed last-writer-wins semantics, no aggregation, no worker-id label to distinguish sources. | None. |
| 5 | Dynamo refuses overload with 529, no `Retry-After` | VERIFIED | `error.rs:19-21` (evidence cites 10-28, contains this): `StatusCode::from_u16(529)` is the hardcoded default, configurable only via `DYN_HTTP_OVERLOAD_STATUS_CODE` env var. `grep -rin 'retry-after\|retry_after' lib/llm/src/` returns zero hits anywhere in the frontend crate. `busy_threshold.rs:5-9` confirms the 529 fires "when all workers for a model exceed their thresholds." Gate-off condition confirmed at `worker_monitor.rs:598-605` (exact): `is_configured()` disables all per-field checks when no threshold is set. | None. |
| 6a | Crate-cycle risk: moving the unkeyed chain primitive out of `roundhouse-core` into `roundhouse-session-id` would cycle, because `ContextAssembler::push` (in core) needs to extend the chain from a render it already computes | VERIFIED (design reasoning, premises confirmed) | `context.rs:216` is exactly `ContextAssembler::push`, which calls `self.tokenizer.encode(&item.render())` — confirmed the render-then-extend shape the argument depends on. `roundhouse-core/Cargo.toml` confirmed to depend on `dynamo-kv-router` (workspace, default-features=false) and `tokio`, but not `http`/`axum`/`redis` — matching the dependency-table row in §14.10 and the premise that core is deliberately thin. The proposed `roundhouse-session-id` crate does not yet exist in the tree (confirmed absent from workspace members and via filesystem search), so this is an unbuilt design claim, not an existing fact — but the architectural premises it rests on (core's actual deps, and where `push`/`render` live) are all verified. | The chain-primitive-in-core vs. keyed-identity-in-new-crate split is a proposed plan, not yet implemented; nothing to falsify against running code. |
| 6b | Dialect namespace is spelled twice: `DIALECT_NAMESPACE` in the server and `MESSAGES_SESSION_SEGMENT` in core | VERIFIED | `messages_api/wire.rs:101`: `const DIALECT_NAMESPACE: &str = "anthropic_messages";` (exact line). `roundhouse-core/src/validate/control_call.rs:197`: `const MESSAGES_SESSION_SEGMENT: &str = "anthropic_messages";` (exact line), referenced at line 145 inside `of_session_key` at line 142 (exact). Same string, two independent named constants in two crates, with a comment at control_call.rs acknowledging the duplication ("Spelled here rather than imported because the server names it a crate above"). | None. |
| 7 | File:line citations for code the crate would absorb | VERIFIED, all exact | `wire.rs`: `SESSION_HEADER` :67, `AGENT_HEADER` :78, `session_key` :236, `scoped` :286, `session_component` :299 — all exact line matches. `request_context.rs`: `from_request` :23, `conversation_key` :49, `prefix_fingerprint` :75 — all exact. `messages_api.rs`: `anonymous_key` :546 — exact. `control_config/mod.rs`: `qualify` :902 — exact. `item.rs`: `grep -n 'async fn\|\.await\|tokio'` returns zero hits — confirms no async code. Fixture citations: `claude-2.1.257-turn-1.json` `system[0]` is exactly `{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.257.1f2; cc_entrypoint=sdk-cli;"}` (byte-exact match). `wire.rs:472` is exactly `fn mark_turn_configuration`; `wire.rs:814` is exactly `fn the_live_client_body_canonicalizes_block_by_block`. Fixture directory has 11 files: 3 header captures (`claude-2.1.251-headers.json`, `claude-2.1.257-headers.json`, `claude-2.1.257-mcp-headers.json`) and 8 bodies — matches "Eight are Claude Code request bodies. Three are header captures." `messages_api_surface.rs` includes `2.1.251-headers.json` at line 143 and `2.1.257-headers.json` at line 152 (evidence cited 141-152, exact within range); `TURN_THREE_CURRENT` at line 209 confirmed. `claude_launch/control_surface.rs:219` confirmed as the sole includer of `claude-2.1.257-mcp-wire.json`. `claude-2.1.257-mcp-headers.json` confirmed included by no `.rs` file anywhere in the tree (grep across all `*.rs` for the filename finds zero `include_str!`/path references). `git grep` for `user-agent`, `originator`, `cc_version`, `x-anthropic-billing-header` under `crates/` finds hits only in `tests/fixtures/*.json`, `wire.rs`'s own `#[cfg(test)]` module (lines 820, 842, inside the test starting at 814), and `messages_api_surface.rs` test code — zero hits in non-test `src` paths outside the fixture/test surface. `originator` has zero hits anywhere in `crates/`. | Minor: the summary row phrasing "None of the three gives a 422" is confusing as written — the actual behavior (confirmed in code) is that a 422 fires only when *all three* of `thread-id`/`session-id`/`prompt_cache_key` are absent; presence of any one avoids it. Not a factual error, just an ambiguous sentence. |
| 8a | `roundhouse-fleet/src/local.rs:410-425` comment: capacity denominator (`total_kv_blocks`) not in the catalog, uses `potential_prefill_tokens` | VERIFIED | Exact text match: "normalizing it would take a capacity denominator (`total_kv_blocks`) the catalog does not carry," and `load_for` returns `load.potential_prefill_tokens as f64`. | None. |
| 8b | `GET /loads` on the standalone selection service; `PotentialLoad` has exactly `potential_prefill_tokens`, `potential_decode_blocks`, `active_requests` | VERIFIED | `server.rs:330`: `.route("/loads", get(loads))` exact. `protocols.rs:611-620`: `PotentialLoad` struct fields match exactly (worker_id, dp_rank, potential_prefill_tokens, potential_decode_blocks, active_requests). | None. |
| 8c | Event plane needs `DistributedRuntime`; Roundhouse deliberately depends on neither `dynamo-llm` nor `dynamo-runtime` | VERIFIED | `rh Cargo.toml:41-46` region contains the exact comment "Deliberately NOT depending on `dynamo-llm`" and states `dynamo-kv-router` under `standalone-selection` "carries no `dynamo-runtime` dependency, enforced upstream by `lib/kv-router/src/services/CLAUDE.md`." The pinned rev in this dependency block (`ac7b7513790ef1d619b46f805aea03c9f21200ba`) matches the Dynamo pin under test. | None. |
| 8d | `router_kv_hit_rate` histogram, "Predicted KV cache hit rate at routing time (0.0-1.0)", recorded via `push_router.rs` | VERIFIED | `metrics.rs:921-927`: doc string verbatim match, built via `router_metric(frontend_service::KV_HIT_RATE)` = `"router_" + KV_HIT_RATE` (confirms the stated full name `router_kv_hit_rate`). `push_router.rs:326-337`: `tracker.record_kv_hit(...)` and `guard.request_metrics().kv_hit_rate.observe(hit_rate)` confirmed inside the `if let Some(ref tracker)` block, gated exactly as evidence describes. | None. |
| 8e | `admin_api.rs:26-33`: a mutation affects the next admission and nothing in flight | VERIFIED | Doc comment at `admin_api.rs:27-33` is a close paraphrase/match: "The next admission, and nothing in flight. A turn resolves its policy, its budget and its credentials once, at admission, and holds them for its whole life..." | None. |
| 8f (new negative found) | Codex citations in evidence §14.9 (`codex@6344a65 codex-api/src/requests/headers.rs`, `codex-api/src/endpoint/responses.rs`, `login/src/auth/default_client.rs`) | PARTIALLY VERIFIED | The cited paths do **not** resolve at the pin as literally written — `~/.cargo/git/checkouts/codex-9eee5d47a939c68c/6344a65/codex-api/...` does not exist. The actual files live one directory deeper, under a `codex-rs/` workspace root: `codex-rs/codex-api/src/requests/headers.rs`, `codex-rs/codex-api/src/endpoint/responses.rs`, `codex-rs/login/src/auth/default_client.rs`. Once that prefix is supplied, every claimed line and quote checks out exactly: `build_session_headers` in `headers.rs` (session-id/thread-id headers), `x-openai-subagent` insert at `responses.rs:92-93` (exact), `DEFAULT_ORIGINATOR: &str = "codex_cli_rs"` at `default_client.rs:40` (exact), and `default_headers()` inserting `originator` and a user-agent starting at `default_client.rs:330` (exact). | The evidence document's Codex file citations are missing the `codex-rs/` path prefix throughout §14.9. Content and line numbers are otherwise accurate once that prefix is restored — this is a citation-formatting defect, not a substantive error. |

#### Summary of anything not plain VERIFIED

- **Claim 6a** (crate-cycle argument): the reasoning and its cited premises (core's actual dependencies, `ContextAssembler::push` calling `item.render()` at `context.rs:216`) are all confirmed, but the target crate (`roundhouse-session-id`) does not exist yet — this is unbuilt design, not a fact about running code, so it is verified only as "the stated premises are true," not as "the crate compiles this way."
- **Claim 7** row on 422 behavior: phrasing "None of the three gives a 422" is ambiguous; actual mechanism (422 only when thread-id, session-id, and prompt_cache_key are all absent) is correct once clarified.
- **Claim 8f** (new negative): every Codex file:line citation in evidence §14.9 is missing a `codex-rs/` directory prefix and will not resolve as written against the pinned checkout; the underlying content, once the correct path is used, is accurate down to the line number.

No claim was refuted. No claim was uncheckable. `crates/roundhouse-core/src/control/credential/` was not read, per instructions, and nothing in this fact-check depends on it.

## 15. Sequence identity, compaction on the wire, and invalidation surfaces, 2026-09-29

This section supports the addendum "sequence identity, compaction invalidation, and the dispatch ledger" in `../synergies/prefix-anchored-routing-proposal.md`. No code changed. No Cargo command ran. No client ran against a provider. No deployment ran.

### 15.0 Result

- **Codex has three compaction paths at the pin, and Roundhouse gets the local one.** The path depends on the provider name. Roundhouse's generated configuration names the provider `Roundhouse`, so Codex summarizes through an ordinary `/responses` request (sections 15.2, 15.3).
- **Codex marks a compaction exactly, twice.** The summarization request carries `request_kind: "compaction"` in `x-codex-turn-metadata`. The first request after a successful compaction carries a larger window number in `x-codex-window-id` (section 15.2).
- **Roundhouse serves no `/v1/responses/compact` route and refuses the compaction item types with 422** (section 15.4).
- **Claude Code 2.1.284 marks a compaction exactly, but only when gateway hint headers are on.** The summarization request carries `x-claude-code-compaction`. The first main-thread request after a successful compaction carries `x-claude-code-context-compacted`. Behind a custom base URL, the headers need `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` or a remote flag (section 15.5).
- **Claude Code also marks a compaction in content, at every version on this box.** The first message after a compaction starts with a fixed sentence. The summarization request ends with a user message that starts with a fixed sentence (section 15.5).
- **Both clients summarize with a request that appends to the old history.** Prefix admission therefore lands that request on the old generation. The next request then forks a new generation whether the compaction succeeded or not (section 15.6).
- **Codex `session-id` and the default `prompt_cache_key` name the whole agent family, not one sequence.** Dynamo's own header map uses Codex `session-id` as its session (section 15.7).
- **Dynamo at the pin has no endpoint that invalidates one sequence.** The nearest constructs are an unconsumed `x-dynamo-session-final` hint and a per-worker `clear_kv_blocks` control that flushes the whole prefix cache (section 15.8).
- **The Roundhouse log already holds what a return curve needs, except the client kind** (section 15.9).
- **The Dynamo mocker's prefill time depends on the cached prefix.** So a TTFT-based hit estimate is testable against the mocker, which does not report cached tokens (section 15.10).

### 15.1 Method and claim tags

The tags of section 1 apply. This section adds one tag:

- **[claude@2.1.284 +N]**: the byte offset `N` of the quoted text in the installed Claude Code executable `~/.local/share/claude/versions/2.1.284`. The file is a Bun single-file executable with minified JavaScript inside. Its SHA-256 starts with `3dd0f96d7ada4631`. The offsets come from `ccgrep.py`, which sits beside this document. Run it with the executable path as its first argument. It searches the file as bytes and prints a window around each hit. The executable was not run.

Limits of the bundle read:

- A bundle read is source-level evidence of one version. It is not a wire capture. No Claude Code compaction request was captured at any version.
- The Roundhouse fixtures are 2.1.251 and 2.1.257. No bundle of those versions is on this box. The oldest bundle on this box is 2.1.270.
- Minified names such as `vsn` or `pzt` change between builds. The quoted strings and the header names are the stable part.

### 15.2 Codex: the three compaction paths at the pin

**Selection.**

| Path | Request | Chosen when | Source |
|---|---|---|---|
| Local | `POST {base}/responses`, streamed. The whole history plus one user message with the summarization prompt. | The provider's remote compaction support is `Unsupported` | [codex@6344a65 codex-rs/core/src/tasks/compact.rs:60-77], [codex-rs/core/src/session/turn.rs:1223-1235] |
| Remote v1 | `POST {base}/responses/compact`, unary. The whole history. The server returns the compacted items. | Support is `V1`, or `V2` with the `remote_compaction_v2` feature off | [codex@6344a65 codex-rs/core/src/tasks/compact.rs:52-58], [codex-rs/core/src/client.rs:162, :552-658] |
| Remote v2 | `POST {base}/responses`, streamed. The whole history plus one `compaction_trigger` input item. The server returns exactly one `compaction` output item. | Support is `V2` and `remote_compaction_v2` is on. It is on by default. | [codex@6344a65 codex-rs/core/src/tasks/compact.rs:41-51], [codex-rs/core/src/compact_remote_v2_attempt.rs:77], [codex-rs/core/src/compact_remote_v2.rs:400-457], [codex-rs/features/src/lib.rs:1462-1465] |

- A configured provider gets `V2` only when `is_openai()` is true or the base URL is an Azure Responses URL. Every other configured provider gets `Unsupported` [codex@6344a65 codex-rs/model-provider/src/provider.rs:299-313].
- `is_openai()` compares the provider **name** with `"OpenAI"` [codex@6344a65 codex-rs/model-provider-info/src/lib.rs:34, :403-405]. The built-in OpenAI provider keeps that name when `openai_base_url` overrides its URL [codex-rs/model-provider-info/src/lib.rs:331-333, :434-437].
- The pin's own test pins a custom URL to `Unsupported` [codex@6344a65 codex-rs/model-provider/src/provider.rs:596-625].
- A fourth path exists under the `token_budget` feature. It installs a new context window with no summarization request at all [codex@6344a65 codex-rs/core/src/compact_token_budget.rs:21-24, :82]. The feature is `UnderDevelopment` and off by default [codex-rs/features/src/lib.rs:1348-1351].

**Triggers.**

- Manual: `/compact` runs `CompactTask` [codex@6344a65 codex-rs/core/src/tasks/compact.rs:28-85].
- Automatic, before a turn: when the token limit is reached [codex@6344a65 codex-rs/core/src/session/turn.rs:1005-1020].
- Automatic, in the middle of a turn: after a tool output pushes the context over the limit [codex@6344a65 codex-rs/core/src/session/turn.rs:452-477].
- The automatic limit is 90% of the model's context window, or the configured limit if that is smaller [codex@6344a65 codex-rs/protocol/src/openai_models.rs:472-483], [codex-rs/core/src/session/context_window.rs:32-35, :74-79].
- Roundhouse's generated catalog states a 272,000-token window and a `null` compaction limit [rh crates/roundhouse-server/src/codex_launch.rs:171, :493-497]. At the pin's rule that is an automatic compaction at 244,800 tokens. **The catalog is written for the Codex 0.146.0 binary on this box, not for the pin.** The 90% rule was read at the pin only.

**What reaches the wire.**

| Path | `x-codex-turn-metadata` header | Body `client_metadata["x-codex-turn-metadata"]` | `implementation` value |
|---|---|---|---|
| Local | Yes | Yes | `responses` |
| Remote v1 | Yes | No. The v1 body type has no `client_metadata` field. | `responses_compact` |
| Remote v2 | Yes | Yes | `responses_compaction_v2` |

- `CompactionTurnMetadata` has five fields: `trigger` (`manual`, `auto`), `reason` (`user_requested`, `context_limit`, `model_downshift`, `comp_hash_changed`), `implementation`, `phase` (`standalone_turn`, `pre_turn`, `mid_turn`), and `strategy` (`memento`, `prefix_compaction`) [codex@6344a65 codex-rs/core/src/responses_metadata.rs:86-115], [codex-rs/analytics/src/facts.rs:379-417].
- A compaction request sets `request_kind: "compaction"` and a `compaction` object in the turn metadata payload [codex@6344a65 codex-rs/core/src/responses_metadata.rs:134-150, :343-382].
- The header is inserted by `compatibility_headers`, the body field by `client_metadata` [codex@6344a65 codex-rs/core/src/responses_metadata.rs:274-341]. The local path builds the metadata at [codex-rs/core/src/compact.rs:178-179, :265-269]. The v1 path adds the compatibility headers at [codex-rs/core/src/client.rs:621], and its body type is `CompactionInput` [codex-rs/codex-api/src/common.rs:28-43]. The v2 path stamps its value at [codex-rs/core/src/compact_remote_v2.rs:83, :117].
- The streamed request body carries `client_metadata` [codex@6344a65 codex-rs/core/src/client.rs:921-938].
- The pin's test asserts `request_kind == "compaction"` and the full `compaction` object on a local compaction request. It asserts that the next request has `request_kind == "turn"`, no `compaction` object, and a different `window_id` [codex@6344a65 codex-rs/core/tests/suite/compact.rs:3672, :3786-3838].

**The window number is the exact post-compaction marker.**

- `x-codex-window-id` is `"{thread_id}:{window_number}"` [codex@6344a65 codex-rs/core/src/session/mod.rs:3670-3675], sent as a compatibility header [codex-rs/core/src/responses_metadata.rs:315].
- Local compaction advances the window only after the summarization stream completes [codex@6344a65 codex-rs/core/src/compact.rs:282-360]. Remote v1 and v2 advance it only after a successful attempt [codex-rs/core/src/compact_remote.rs:268], [codex-rs/core/src/compact_remote_v2.rs:304]. The token-budget path advances it too [codex-rs/core/src/state/session.rs:215-219].
- A failed compaction does not advance the window. A mid-turn failure ends the turn with an error and leaves the history as it was [codex@6344a65 codex-rs/core/src/session/turn.rs:466-474].
- A resume restores the stored window number rather than advancing it [codex@6344a65 codex-rs/core/src/session/mod.rs:1494], [codex-rs/core/src/session/session.rs:1413].

**What the history looks like after compaction.** The pin's snapshots show `input` only. `instructions` stay unchanged and travel beside `input`. Roundhouse stores them as canonical item 0 [rh crates/roundhouse-server/src/responses_api/wire.rs:46].

| Case | Last request before | First request after | Source |
|---|---|---|---|
| Manual, local | developer permissions, environment context, `first manual turn`, reply, summarization prompt | `first manual turn`, summary, developer permissions, environment context, `second manual turn` | [codex@6344a65 codex-rs/core/tests/suite/snapshots/all__suite__compact__manual_compact_with_history_shapes.snap:8-19] |
| Before a turn, local | permissions, environment, `USER_ONE`, reply, `USER_TWO`, reply, summarization prompt | `USER_ONE`, `USER_TWO`, summary, permissions, environment diff, `USER_THREE` | [...pre_turn_compaction_including_incoming_shapes.snap:8-24] |
| Mid-turn, local | permissions, environment, user, function call, function output, summarization prompt | permissions, environment, user, summary | [...mid_turn_compaction_shapes.snap:8-19] |
| Rollback past a compaction | compacted layout | Still the compacted layout. The rollback does not restore the pre-compaction history. | [codex@6344a65 codex-rs/core/tests/suite/snapshots/all__suite__compact_resume_fork__rollback_past_compaction_shapes.snap:6-27] |

- Local compaction keeps the most recent real user messages up to 20,000 tokens, then appends the summary as a user message [codex@6344a65 codex-rs/core/src/compact.rs:57, :639-717].
- The summary text starts with the fixed `SUMMARY_PREFIX` ("Another language model started to solve this problem and produced a summary of its thinking process. …") and a newline [codex@6344a65 codex-rs/core/src/compact.rs:349-351], [codex-rs/prompts/templates/compact/summary_prefix.md:1]. The summarization prompt starts with "You are performing a CONTEXT CHECKPOINT COMPACTION." [codex-rs/prompts/templates/compact/prompt.md:1]. A configured `compact_prompt` replaces the prompt but not the prefix [codex-rs/core/src/compact.rs:118-123].
- Before a turn and for `/compact`, Codex drops the initial context and injects it again on the next regular turn. In the middle of a turn, it inserts the initial context before the last real user message [codex@6344a65 codex-rs/core/src/compact.rs:59-74, :362-367], [codex-rs/core/src/compact.rs:571-637].
- Remote compaction keeps user messages, assistant messages, and the `compaction` item. It drops developer messages [codex@6344a65 codex-rs/core/src/compact_remote.rs:355-397]. Remote v2 keeps up to 64,000 tokens of retained messages [codex-rs/core/src/compact_remote_v2.rs:65, :459-487].

**Consequence for the KV lineage, derived from the snapshots.** In Roundhouse's canonical order, a pre-turn or manual compaction diverges at item 1. The first `input` item was the permissions developer message and becomes a retained user message. So the old and new sequences share only `instructions` and the tools. A mid-turn compaction with one user message shares the instructions, the initial context, and that user message. It diverges at the first tool call.

### 15.3 Codex behind Roundhouse

- Roundhouse's generated `config.toml` names the provider `Roundhouse`. Its comment says that the name `OpenAI` turns on the routing-hint header, remote compaction, and zstd compression, "Roundhouse serves none of the three" [rh crates/roundhouse-server/src/codex_launch.rs:416-420]. A test pins the name [rh codex_launch.rs:737-748].
- **With the generated configuration**, Codex uses local compaction. The summarization request is an ordinary `POST /v1/responses` turn.
- **With a hand-written configuration named `OpenAI`**, remote v2 is on by default. The request carries a `compaction_trigger` input item, which Roundhouse refuses with 422 (section 15.4). With v2 off, the request goes to `/v1/responses/compact`, which has no route (section 15.4).
  - A manual compaction then reports an error. A pre-turn compaction fails the turn. A mid-turn compaction ends the turn with an error [codex@6344a65 codex-rs/core/src/session/turn.rs:466-474, :1011-1020].
  - This failure behavior is derived from source. It was not run.
- `prompt_cache_key` defaults to `session_id`, the family root. The one override at the pin is the guardian reviewer, which uses `guardian:{parent_thread_id}` [codex@6344a65 codex-rs/core/src/client.rs:484-488], [codex-rs/core/src/session/session.rs:1279-1284], [codex-rs/core/src/guardian/review_session.rs:276-288].

### 15.4 Roundhouse at `2dd40dd`: routes, refusals, and what admission reports

| Fact | Source |
|---|---|
| The Responses surface mounts one route, `POST /v1/responses`. No `/v1/responses/compact` route exists, and the server adds no fallback. Axum's default answer for an unknown path is 404 (not run). | [rh crates/roundhouse-server/src/responses_api.rs:176, :208-212], [rh crates/roundhouse-server/src/main.rs:1177] |
| `compaction`, `compaction_trigger`, and `context_compaction` input items are refused with 422 and a message that names the type | [rh crates/roundhouse-server/src/responses_api/wire.rs:124-126], test `the_item_types_a_real_client_can_resend_are_named` [rh crates/roundhouse-server/src/responses_api/wire/tests.rs:506] |
| `x-codex-window-id` is read as an observation. `observe_context` reports `window_changed`, node-local and in memory only. | [rh crates/roundhouse-server/src/request_context.rs:30], [rh crates/roundhouse-server/src/conversations.rs:583-614] |
| The turn-metadata `thread_id` is parsed, and nothing else from that header | `codex_thread_id` [rh crates/roundhouse-server/src/responses_api.rs:562-570] |
| A generation is a session id: the key alone for generation 0, `{key}#g{n}` after | `bound_session` [rh crates/roundhouse-server/src/conversations.rs:780-785] |
| `bind_prefix` returns the session, the delta, and one flag: a fresh generation opened after a disagreement | [rh crates/roundhouse-server/src/prefix_admission.rs:175, :184-253] |
| `Probe::Disagrees` carries no data. Admission does not know where the claim left the stored history, or which generation it left. | [rh prefix_admission.rs:466-479, :529-535] |
| A claim is admitted only when it agrees with the stored history on their whole overlap. A shorter claim is a retry. | `admit`, `suffix_after` [rh prefix_admission.rs:781-810] |
| A fresh generation starts with no ledger state, "priced cold" | [rh prefix_admission.rs:229-242] |
| `CacheLedger::invalidate` exists for compaction, and no production code calls it. A fresh generation is a new session, so its ledger starts empty anyway. | [rh crates/roundhouse-core/src/routing/ledger.rs:20-24, :555-557], its only caller is the test [rh ledger.rs:1038-1049] |

### 15.5 Claude Code: compaction in the 2.1.284 bundle

**The summarization request.**

- The prompt starts with a fixed block, `CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.` [claude@2.1.284 +206385457]. `Noe` joins that block with the summary instructions, `Your task is to create a detailed summary of the conversation so far, …` [claude@2.1.284 +206398013], [+206388512].
- **The cache-sharing path, on by default.** When the flag `tengu_compact_cache_prefix` is on (default `true`) [claude@2.1.284 +206451579], the summary runs as a one-turn fork with `querySource: "compact"`. It passes `cacheSafeParams`, so it keeps the main thread's system prompt, tools, and messages. It appends one user message with the prompt. Tool use is denied [+206462607]. So this request is the old history plus one user message.
- **The fallback path.** It uses the system prompt `You are a helpful AI assistant tasked with summarizing conversations.`. It drops the tools when `stripNonEssential` is set, and sets `enablePromptCaching: false` [claude@2.1.284 +206466151, +206466996].
- A reactive compaction, after a prompt-too-long refusal, uses the same fork with `forkLabel: "reactive-compact"` [claude@2.1.284 +206401727].
- A hook can supply the replacement history. Then no summarization request is sent, and the compaction still arms the post-compaction marker [claude@2.1.284 +206423569].

**The first request after compaction.**

- The summary message starts with `This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.` One optional Artifact sentence can come before it [claude@2.1.284 +206398620].
- Optional tails follow: a transcript path, `Recent messages are preserved verbatim.`, and a note that the head was truncated [claude@2.1.284 +206398620].
- The 2.1.270 bundle carries the same sentence and the same fallback system prompt (a `grep -c` of each literal).

**Gateway hint headers.**

| Header | Set on | Values | Source |
|---|---|---|---|
| `x-claude-code-compaction` | The summarization request | `auto`, `manual`, or `reactive` | Constants [claude@2.1.284 +205616041], writer `$It` [+206611938], values `Woe` [+206423790] |
| `x-claude-code-context-compacted` | The next main-thread request after a successful compaction, once | Same | Armed by `pzt` after success [+206422059, +206455259], consumed by `VVr` [+197062748], read per query [+206679786] |
| `x-cc-compaction-request`, `x-cc-context-compacted` | The same requests, first-party only | Same | [+205616041], [+206611938] |

- **The gate.** The `x-claude-code-*` pair is sent when `vsn()` is true. `vsn()` returns the value of `CLAUDE_CODE_GATEWAY_HINT_HEADERS` when that variable is set. Otherwise it is true for the first-party Anthropic URL, false for a non-first-party provider, and the remote flag `tengu_splendid_sutton` (default `false`) otherwise [claude@2.1.284 +203364143]. The `x-cc-*` pair needs `Gl()`, which is false when `ANTHROPIC_BASE_URL` points away from Anthropic [+198217798].
- **Behind Roundhouse**, `ANTHROPIC_BASE_URL` is Roundhouse [rh crates/roundhouse-server/src/claude_launch.rs:290]. So the `x-claude-code-*` pair arrives only with `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`, or when the remote flag is on for that user. The remote flag cannot be read from this box.
- The bundle's changelog says to "opt in with `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`" for these gateway hint headers, under 2.1.283 [claude@2.1.284 +213681747].
- The post-compaction header is armed only when the compaction ran on the main thread (`Yb(querySource, agentId)`) [+206422059, +206455259]. A sub-agent compaction arms nothing.
- **Version range.** A `grep -c` of each header name finds none in 2.1.270 and finds both in 2.1.276, 2.1.278, 2.1.280, 2.1.281, and 2.1.284. The Roundhouse fixtures (2.1.251, 2.1.257) are older than all of these.

**Identity headers in the same bundle.**

- The client factory sets `x-claude-code-agent-id` when its agent context has an `agentId`, and `x-claude-code-parent-agent-id` when it has a `parentAgentId` [claude@2.1.284 +203366550]. The session header constant is `X-Claude-Code-Session-Id` [+199902704].
- Earlier evidence recorded both agent headers as "documented only, never captured" (`session-identity-evidence.md` §4.1). The bundle shows the code that sends them at 2.1.284. It is still not a capture.
- **Unverified.** Whether the compaction fork runs with its own `agentId`, and so sends its own `x-claude-code-agent-id`, was not traced.

### 15.6 How a compaction lands in prefix admission (derived, not run)

1. **The summarization request is an append.** For Codex local compaction, the request is the old `input` plus one user message (section 15.2). For the Claude Code cache-sharing path, it is the old messages plus one user message (section 15.5). The claim agrees with the stored generation and extends it. So admission lands it on the old generation with a delta of that one message [rh prefix_admission.rs:781-810].
2. **The summary answer is appended to that generation** as an ordinary response.
3. **The next request forks, in every case.**
   - After a successful compaction, the claim drops history. It disagrees at canonical item 1 for Codex (section 15.2), and at the first history item for Claude Code.
   - After a failed or abandoned compaction, the client resends the old history without the summarization message. The stored generation holds that message, so the overlap disagrees at that position and the claim forks too.
   - The provisional-item rule does not help here. It skips only items stamped by a response that did not complete [rh prefix_admission.rs:577-591, :699-736]. The summarization prompt is an unstamped client item.
4. **A Claude Code fallback summary replaces the configuration run in place.** The leading system blocks are configuration and are replaced, not compared [rh prefix_admission.rs:746-794]. So the fallback request still lands on the old generation.

So "a new generation opened" is not by itself proof that a compaction succeeded. What tells the two cases apart is where the new claim leaves the old history. A failed compaction leaves it at the summarization message. A successful one leaves it near the start.

### 15.7 Session and sequence fields

| Field | Client | Names | Source |
|---|---|---|---|
| `session-id` header | Codex | The root thread of the agent family. Every sub-agent sends the same value. | `session-identity-evidence.md` §3.1, §3.2, §11 claim 3 (fact-checked) |
| `thread-id` header, turn metadata `thread_id` | Codex | One thread, one member | Same |
| `x-codex-parent-thread-id`, turn metadata `parent_thread_id`, `forked_from_thread_id` | Codex | The parent, the fork origin | Same |
| `x-openai-subagent`, turn metadata `subagent_kind` | Codex | The kind of sub-agent | Same |
| `prompt_cache_key` | Codex | `session_id` by default, so the family root | Section 15.3 |
| `x-codex-window-id` | Codex | `{thread_id}:{window_number}`. The number advances on each successful compaction. | Section 15.2 |
| turn metadata `request_kind`, `compaction` | Codex | The purpose of one request | Section 15.2 |
| `x-claude-code-session-id` | Claude Code | The session. In-process sub-agents inherit it. | `session-identity-evidence.md` §4 |
| `x-claude-code-agent-id`, `x-claude-code-parent-agent-id` | Claude Code | The member, the nested parent | Section 15.5 |
| `x-claude-code-compaction`, `x-claude-code-context-compacted` | Claude Code | The purpose of one request, the first request after a compaction | Section 15.5 |

**Dynamo's own reading of these fields.**

- Dynamo's header map takes Codex `session-id` as the session, with no child header [dynamo@ac7b751 lib/llm/src/protocols/agents.rs:33-38]. For Claude Code it takes the agent id as the session and falls back to the session header [agents.rs:26-32, :70-93]. `x-dynamo-session-id` wins over all of them [agents.rs:59-68].
- Dynamo's documentation asks for one id per "reasoning and tool-use chain" and says that a sub-agent can have its own id [dynamo@ac7b751 docs/fern/pages/use-cases/agents/session-ids.mdx:8, :18-20]. Its Codex tab says only that `session-id` maps to `session_id` [session-ids.mdx:53].
- So a Codex family arrives at Dynamo as one session when Dynamo reads the Codex header itself.
- Dynamo's session affinity pins a session id to a worker for a TTL. It is off unless `--router-session-affinity-ttl-secs` is set. It holds at most 65,536 entries [dynamo@ac7b751 lib/llm/src/session_affinity/mod.rs:16-33], [components/src/dynamo/frontend/frontend_args.py:118-122]. The key is the header-derived id [dynamo@ac7b751 lib/llm/src/protocols/common/extensions.rs:287-292].

### 15.8 Invalidation surfaces at the Dynamo pin

- **No endpoint takes a sequence id and releases its blocks.** The frontend route list in section 14.13 claim 1 has no such route. A grep of `lib/llm/src/http` and `components/src/dynamo` for `invalidat`, `evict`, `clear_kv`, and `/v1/sessions` found only the items below and unrelated caches. **This negative needs an independent fact-check.**
- **`x-dynamo-session-final: true`** is documented as "a dedicated minimal request when the session ends", so that "lifecycle-aware consumers can release per-session state" [dynamo@ac7b751 docs/fern/pages/use-cases/agents/session-ids.mdx:36-38]. The frontend turns it into `kv_hints: { evict_session: true }` [dynamo@ac7b751 lib/llm/src/protocols/common/extensions.rs:67-69, :254-266]. A grep of every `.rs` and `.py` file in the Dynamo tree found no reader of `evict_session` or `kv_hints`. The only hits are that construction, the request-trace records, and tests. **This negative needs an independent fact-check.** It is also in-band: it rides on an inference request.
- **`clear_kv_blocks`** is a per-worker control endpoint. vLLM calls `reset_prefix_cache(reset_connector=True)`, which clears the whole prefix cache of that worker [dynamo@ac7b751 components/src/dynamo/vllm/handlers.py:2030-2040]. SGLang refuses while any request is active [components/src/dynamo/sglang/request_handlers/handler_base.py:737-760]. A test reaches it on a worker system port at `/engine/control/clear_kv_blocks` [dynamo@ac7b751 tests/utils/payloads.py:681-690]. It is not on the frontend and it is not scoped to a sequence.
- **A precedent for an admin-gated frontend route.** `/busy_threshold` is mounted only when the admin API is on (section 14.1).

### 15.9 What the Roundhouse log gives a return curve

| Need | Source in the log | Source |
|---|---|---|
| Time of each event | `SessionEvent.at_ms`, the appending node's wall clock | [rh crates/roundhouse-core/src/event.rs:831-837] |
| Start of a turn | `TurnStarted` | [rh event.rs:335-338] |
| How a turn ended | The last assistant `ItemAppended` of the turn (a tool call or text), and `ResponseCompleted.stop_reason`, an open provider string that is often `None` | [rh event.rs:340-342, :353-423] |
| Size of the turn | `ResponseCompleted.usage`: input, cached input, output. `cache_read_source` says whether the cached count is a measurement. | [rh event.rs:44, :95-126] |
| The lineage across generations | The session id is `{label}` or `{label}#g{n}` | Section 15.4 |
| Principal and arm | `SessionCreated.principal`, `SessionCreated.arm` | [rh event.rs:290-333] |
| **Client kind** | **Not recorded.** `SessionCreated` has `model_policy`, `principal`, and `arm` only. | [rh event.rs:290-333] |
| A close signal | **None found.** The MCP surface is stateless (`NeverSessionManager`), so no MCP session ends when a client exits. This read found no close signal from Codex at the pin or from Claude Code 2.1.284. The search was not exhaustive. | [rh crates/roundhouse-mcp/src/transport.rs:396] |

### 15.10 The mocker's prefill time

- The mocker's performance model computes prefill time from `isl - prefix`, the tokens that are not cached [dynamo@ac7b751 lib/mocker/src/common/perf_model.rs:226-237]. Both scheduler models pass the mean cached prefix [lib/mocker/src/scheduler/vllm/core.rs:2657], [lib/mocker/src/scheduler/sglang/core.rs:831-834], and a request passes its `cached_tokens` [lib/mocker/src/common/protocols.rs:333].
- So the mocker's time to first token falls with a cache hit, while its usage reports no cached count (section 13, claim 8).
- **Unverified.** The size of that drop under the mocker's speedup ratio was not run.

### 15.11 Open evidence added by this section

| Question | What closes it |
|---|---|
| A captured Claude Code compaction: the summarization request, the first request after it, and both hint headers with `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` | A loopback capture with `claude-code-wire-probe.py` at 2.1.276 or later |
| Does the Claude Code compaction fork send its own `x-claude-code-agent-id`? | The same capture |
| Does the Claude Code attribution fingerprint change after a compaction, because the first user text changes? | The same capture. The design does not depend on it. |
| A captured Codex local compaction behind Roundhouse at the pin line | A loopback capture of `/compact` and of an automatic compaction |
| No Dynamo reader of `evict_session`, and no per-sequence invalidation route | An independent fact-check of both negatives |
| Is the remote flag `tengu_splendid_sutton` on for any user? | Not closable from outside Anthropic. The launch sets the variable instead. |
| The size of the mocker's TTFT drop on a cache hit, under its speedup ratio | A two-request mocker run |

### 15.12 Fact-check of section 15, 2026-09-29

An independent read-only re-derivation verified all ten checked claims against the same pins and the 2.1.284 executable. One caveat: the Dynamo pin history has ancestor commits for a `trajectory_final` / `evict_trajectory` feature, but the source at the pin uses `session_final` / `evict_session`, and only test assertions read `evict_session`. A later grep for "trajectory" at the pin finds nothing. The ledger follows.

| # | Claim | Ruling | Evidence (file:line) | Notes / correction |
|---|---|---|---|---|
| 1 | Codex: every sub-agent sends the root's `session-id`; default `prompt_cache_key` equals it; `thread-id` is per thread | **VERIFIED** | `agent/control.rs:104-112` (doc comment: "every sub-agents from a common root share the same session ID"), `agent/control.rs:159` (`session_id(&self) -> SessionId`), `client.rs:484-488` (`prompt_cache_key` = `prompt_cache_key_override.unwrap_or(responses_metadata.session_id.clone())`), `codex-api/src/requests/headers.rs:5-13` (`build_session_headers` inserts literal `session-id` and `thread-id` headers), `core/src/client.rs:621-624` (calls `build_session_headers(session_id, thread_id)` with `responses_metadata.session_id`/`.thread_id`), `turn_metadata.rs:101-160` (`thread_id: String` is a distinct per-`CodexResponsesMetadata` field, sourced from a fresh `ThreadId` per spawned thread, `agent/control/spawn.rs`) | The doc comment on `AgentControl` is an explicit, load-bearing statement of exactly this design decision — strongest possible confirmation short of a wire capture. |
| 2 | Codex: `x-codex-window-id` exists and its number increases after a successful compaction | **VERIFIED** | `session/mod.rs:3670` (`current_window_id` = `format!("{thread_id}:{window_number}")`, exact line match to evidence), `client.rs:148` (`X_CODEX_WINDOW_ID_HEADER = "x-codex-window-id"`), `state/session.rs:187-219` (`auto_compact_window_number`/`advance_auto_compact_window` delegate to an `AutoCompactWindow` counter), `compact.rs:282-349` (`advance_auto_compact_window().await` is called only after the retry loop reaches `Ok(())`, i.e., only after the summarization stream completes) | Local-path advance point confirmed directly; remote v1/v2 advance points (`compact_remote.rs:268`, `compact_remote_v2.rs:304`) were not re-read line-for-line but the local-path mechanism generalizes and the pattern (advance after success, not before) is structurally the same in each module. |
| 3a | Local compaction request carries `request_kind: "compaction"` in turn metadata | **VERIFIED** | `responses_metadata.rs:134-138` (`CodexResponsesRequestKind::Compaction(metadata) => ("compaction", Some(metadata))`), `responses_metadata.rs:343-364` (`turn_metadata_payload` maps this into the `CodexTurnMetadataPayload.request_kind` field, key `"request_kind"` at line 31) | |
| 3b | Non-OpenAI provider name compacts locally through `/v1/responses`, not `/responses/compact` | **VERIFIED** | `model-provider/src/provider.rs:299-309` (`capabilities()`: `RemoteCompactionSupport::V2` iff `is_openai() \|\| is_azure_responses_provider(...)`, else `Unsupported`), `model-provider-info/src/lib.rs:403-405` (`is_openai(&self) -> bool { self.name == OPENAI_PROVIDER_NAME }`, `OPENAI_PROVIDER_NAME = "OpenAI"`), `core/src/tasks/compact.rs:38-70` (the branch: `V2` → `compact_remote_v2` streamed via ordinary responses request with a `compaction_trigger` item; `V1 \| V2-without-feature` → `compact_remote::run_remote_compact_task` which posts to `RESPONSES_COMPACT_ENDPOINT` = `/responses/compact` (`client.rs:162`); `Unsupported` → `crate::compact::run_compact_task`, which sends an ordinary turn through the normal client session (no special endpoint constant exists for it) | Also directly confirmed the pin's own test `configured_provider_remote_compaction_matches_provider_support` (`provider.rs:596-625`) pins a custom base URL/name to `Unsupported`. |
| 4 | Auto-compaction threshold is 90% of the model's context window at the pin | **VERIFIED** | `protocol/src/openai_models.rs:472-473` (`auto_compact_token_limit`: `(context_window * 9) / 10`), `core/src/session/context_window.rs:20-79` (`context_window_token_status` uses this limit, plus a config override taking the min, to decide `token_limit_reached`) | |
| 5 | Roundhouse `2dd40dd` has no `/v1/responses/compact` route and refuses compaction item types with 422 | **VERIFIED** | `responses_api.rs` (`responses_router` registers exactly one route, `{API_PREFIX}/responses` → `create_response`; no second route), `main.rs` (only `responses_api::responses_router(...)` is merged — no compact route anywhere), `responses_api/wire.rs` (`canonical_item`'s catch-all `other => Err(ApiError::unprocessable(format!("input item type \`{other}\` is not supported")))`), `http.rs:245-247,261-263` (`ApiError::unprocessable`/`unprocessable_named` both set `StatusCode::UNPROCESSABLE_ENTITY` = 422), `responses_api/wire/tests.rs` (`the_item_types_a_real_client_can_resend_are_named` test lists `"compaction"`, `"compaction_trigger"`, `"context_compaction"` in `refused_types`, asserting each 422s and names the type) | |
| 6 | Roundhouse `SessionId` is `label#g{n}`; a history rewrite starts a new generation | **VERIFIED** | `conversations.rs:780-785` (`bound_session`: `0 => SessionId::new(key)`, `n => SessionId::new(format!("{key}#g{n}"))`, doc comment confirms generation 0 is the bare key), `prefix_admission.rs` (`bind_prefix`'s `Search::Fresh { generation, history_rewritten }` arm opens a new generation via `open_fresh(...)` and returns the `history_rewritten` flag; doc comment explicitly states the returned flag "reports a fresh generation opened after a prefix disagreement") | |
| 7a | Claude Code 2.1.284: the first message after compaction starts with a fixed sentence | **VERIFIED (quoted exactly)** | `ccgrep.py` hit at byte offset `+206398620`: `"This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation."` — exact byte-offset match to the evidence doc's citation | |
| 7b | `x-claude-code-context-compacted` is sent only when `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` (or the equivalent gate) | **VERIFIED** | `+203364166`: `function vsn(){let e=a.CLAUDE_CODE_GATEWAY_HINT_HEADERS;if(e!==void 0)return e;if(Gl())return!0;if(Oe()!=="firstParty")return!1;return x("tengu_splendid_sutton",!1)}`; `+205616140`: `ict="x-claude-code-context-compacted"` constant defined alongside `x-cc-context-compacted`, `x-claude-code-compaction`; `+206611938`: writer `$It(e,n,r){...let s=Gl(),g=vsn();if(n!==void 0){if(s)e[ect]=n;if(g)e[ict]=n}...}` — `ict` (the header in question) is gated strictly on `g = vsn()`; `+197062748`: `pzt`/`VVr` arm/consume `pendingContextCompacted`; `+206679786`: `VVr()` is only called `if Ss(h.querySource)==="main" && Yb(...)`, i.e., only on the main-thread request after a successful compaction | All five cross-referenced offsets in the evidence doc line up into one coherent mechanism: explicit env-var gate, first-party/remote-flag fallback, main-thread-only arming. |
| 8a | Nothing reads `evict_session` at the Dynamo pin | **VERIFIED, with a historical caveat** | `lib/llm/src/protocols/common/extensions.rs:68` (field def), `:257-258` (constructed from `session_final`), and a repo-wide `grep -rn "\.kv_hints\b"` shows the field is touched **only inside test assertions** (`extensions.rs:1064,1197,1204`) — no production code path reads it. | Caveat found during re-derivation: `git log` on the Dynamo checkout shows commits #10172/#10214/#10800/#10808 (a "trajectory identity" feature with `evict_trajectory`/`trajectory_final`, described in `docs/fern/.../dynamo-v1-3-0.mdx`) are ancestors of the pin `ac7b751`, but a `grep -rln "trajectory"` over `.rs`/`.py` files at the pin finds **no such code** — only two unrelated hits. This is most likely an earlier terminology (`trajectory`→`session`) that was renamed in later commits; it does not change the ruling (the *current* `evict_session`/`kv_hints` field, whatever its ancestry, still has no reader), but it means the changelog entry and the live code use different vocabulary for what may be the same lineage, which is worth a note in case a future search greps for "trajectory" and wrongly concludes the feature doesn't exist at all. |
| 8b | No route invalidates one sequence | **VERIFIED** | Repo-wide grep of `lib/llm/src/http` and `components/src/dynamo` for `.route(` calls containing session/sequence/invalidate/evict/clear found none; `clear_kv_blocks` confirmed as a per-worker, non-frontend control call: `components/src/dynamo/vllm/handlers.py:2029-2038` (`reset_prefix_cache(reset_connector=True)`, whole-cache flush), `components/src/dynamo/sglang/request_handlers/handler_base.py:737-760` (`Cannot clear KV cache while requests are active` refusal when `tokenizer_manager.rid_to_state` is non-empty), reached only via `/engine/control/clear_kv_blocks` in `tests/utils/payloads.py:683-690` (a worker-scoped test endpoint, not a frontend route) | |
| 8c | Dynamo's header map reads Codex `session-id` as the session | **VERIFIED** | `lib/llm/src/protocols/agents.rs:11` (`HEADER_CODEX_SESSION_ID = "session-id"`), `:24-42` (`AGENT_HEADER_MAPPINGS` entry for Codex has `root_session_header: HEADER_CODEX_SESSION_ID, child_session_header: None, parent_session_header: None`), `:60-90` (`agent_context_header_values`: when no `x-dynamo-session-id` is present, falls through the mapping table and for Codex uses the root header value directly as `session_id`, with no child header to distinguish sub-agents) | |
| 9 | The summarization request is an append to the old history (lands on the old generation) | **VERIFIED** | `compact.rs:239-245` (`run_compact_task_inner_impl`): `let mut history = sess.clone_history().await; history.record_items(&[initial_input_for_turn.into()], ...)` — the full existing history is cloned, then exactly one item (the summarization prompt) is appended via `record_items`. Nothing is removed before the request is sent; the loop that follows (`compact.rs:270+`) sends `history.clone().for_prompt(...)` as the turn's `input`. | This is a clean, direct confirmation: the local-compaction request literally is "clone old history, append one item," which is definitionally what prefix admission would treat as a continuation of the existing generation. |
| 10a | `CacheLedger::invalidate` exists, and no production code calls it — only a test does | **VERIFIED** | `roundhouse-core/src/routing/ledger.rs:20-24` (module doc: "`CacheLedger::invalidate` exists for that case and must be called whenever the assembler rewrites history"), `:555-557` (`pub fn invalidate(&mut self) { self.state.clear(); }`), repo-wide `git grep "\.invalidate()"` across all `.rs` files in the whole tree at `2dd40dd` returns exactly one hit: `ledger.rs:1049`, which is inside `#[test] fn invalidation_clears_warm_prefixes_after_a_compaction()` | |
| 10b | Roundhouse's generated Codex config names the provider `Roundhouse` (never `OpenAI`), documented in the config comment and pinned by a test | **VERIFIED** | `codex_launch.rs` (embedded config template): comment `# Never "OpenAI": codex matches this *name* (not the table key) to decide whether to attach its routing-hint header, use remote compaction, and zstd-compress the request body. Roundhouse serves none of the three.` followed by `name = "Roundhouse"`; test `the_provider_name_is_not_openai` asserts `name.to_ascii_lowercase() != "openai"` with a matching rationale string | |
| 10c | Roundhouse's catalog states a 272,000-token window and a `null` compaction limit | **VERIFIED** | `codex_launch.rs`: `pub const CONTEXT_WINDOW_TOKENS: u64 = 272_000;` with a doc comment explaining it matches "0.146.0's own fallback metadata", and `"auto_compact_token_limit": null` in the generated model-info JSON with a comment explaining why (a judge-turn compaction limit would make the client rewrite its own history off a number describing a side call) | |
| 10d | `/responses/compact` is a distinct endpoint used only by remote v1, never by local compaction | **VERIFIED** | `core/src/client.rs:162`: `const RESPONSES_COMPACT_ENDPOINT: &str = "/responses/compact";`, used only from the remote-v1 path (`compact_remote.rs`); the local path (`compact.rs`) sends its request through the ordinary turn-streaming client session with no reference to this constant, i.e., through the normal `/responses` (or websocket-equivalent) turn path | |

#### Summary of anything not a clean VERIFIED

Everything above is **VERIFIED**. The only wrinkle is inside claim 8a: the Dynamo pin's git
history contains ancestor commits for a `trajectory_final`/`evict_trajectory` feature (per
`docs/fern/pages/reference/general/releases/dynamo-v1-3-0.mdx`) that does not appear anywhere in
the pin's actual `.rs`/`.py` source tree — the working code instead has `session_final`/
`evict_session`. This looks like a rename across the feature's lifetime, not a contradiction, and
it does not change the ruling that nothing at the pin reads `evict_session` today. It's flagged
here only so a later reader who greps for "trajectory" and finds nothing doesn't mistakenly
conclude the changelog entry is fabricated — the code exists, under a different name.

## Note, 2026-09-29: the Roundhouse revision this evidence was read at

[Every `[rh path:line]` claim above was read at `2dd40dd`. That commit is an ancestor of `ai/learner-m8-engine` (remote tip `b32b51e`), the tip of the open stack #18 → #21 → #22 → #23 → #24–#31. It is not on `main` `e521855`, which this document's PR targets. A re-derivation against `main` found:

- **Missing on `main`, present on the stack:**
  - `CacheReadSource` and `Usage.cache_read_source`
  - `metrics/cache_evidence.rs` and `cache_reuse_evidence`
  - the marker-aware, seedable `CacheLedger`
  - `ContextAssembler::tokens_through`
  - `routing/learn/*`, `min_sessions` and `LevelKey`
  - four `SessionEventKind` variants
- **Moved, but behaving as claimed:** for example, `Engine::admitted_input_tokens` is at `engine.rs:1766` on `main`, and `CacheLedger::invalidate` is at `ledger.rs:400`.
- **PR #17, already on `main`, adds code this document does not mention:**
  - `bind_prefix`'s `history_rewritten` flag
  - `Conversations::observe_context` and the `x-roundhouse-context-signal` header
  - `RequestContext.session_id` and `thread_id`, which are forwarded to frontier Responses
- **Precision:** the proposal's §21.9 says the ledger's compare runs "in one Redis script, as the correlation maps already do". That holds only for tool-call bindings (`BIND_CALL` in `crates/roundhouse-store-redis/src/correlation/scripts.rs`). The generation map is a plain write by design, and `roundhouse-core/src/control/correlation.rs:38-49` explains why it rejects a compare-and-set.

The snapshot above is unchanged. The rebaselined table is §1 of `../PLAN-session-sequence-identity.md`. Its citations are derived again when the stack merges.]

## Note, 2026-09-29: Dynamo main against the pin

[A re-read of Dynamo main `6822bab` against the pin `ac7b751` is recorded in `../synergies/dynamo-sequence-lifecycle-upstream.md` §2, with citations. Corrections to this snapshot:

- **§15.8.** A reader of `session_final` existed at the pin: the Python ThunderAgent router drops scheduler state on it. None of the readers releases KV. At main, `kv_hints.evict_session` is gone (#13134), and the native ThunderAgent plugin drops state and then runs the request.
- **§14.1.** At both revisions the admin API is on by default and has no authentication. `/busy_threshold` is a precedent for how to mount a route, not for security.
- **§15.7.** Dynamo main maps Codex `thread-id`, not `session-id`, as its session. This does not affect Roundhouse, because `x-dynamo-session-id` takes precedence.
- **§13 claim 8.** It holds at the pin. At main the mocker fills `cached_tokens` for its vLLM scheduler model (#12711), but not for its SGLang model.]
