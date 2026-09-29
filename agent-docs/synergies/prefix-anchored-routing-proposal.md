<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Prefix-anchored routing to sticky Dynamo pools

> **Status: proposal, 2026-09-29. Not ruled.** Written to the owner's ruling in the addendum of `program-identity-proposal.md`, which it replaces. Evidence: `../research/prefix-anchored-routing-evidence.md` (cited as "evidence §n"). Written against Roundhouse `2dd40dd`, Codex pin `6344a65`, and Dynamo pin `ac7b751`. Section 12 lists the questions for the owner. When the owner rules, add a dated addendum. Do not rewrite this text.

## 0. Summary

- **The unit is KV overlap.** Roundhouse computes a chain of digests over the canonical items of the prompt it dispatches, one link per item. It stores the chain value at two points of each turn: the end of the dispatched prompt, and the end of the settled response. These are **tips**.
- **The per-item digest is built on `Item::render()`.** Prefix admission has no digest of its own. It compares items structurally, and structural agreement implies equal renders. The render is also what the turn id, the token buffer, and the admitted token count already use (evidence §2).
- **A session label is the conversation key** that prefix admission already resolves. A known label routes to its bound pool with no content lookup. An unseen label triggers one batched longest-match lookup of its prompt's boundaries against stored tips.
- **The anchor** is the identity of a KV lineage. An unseen label that matches a stored tip inherits that tip's anchor and pool. An unseen label that matches nothing is **new**, and its anchor is the keyed tip of its own first prompt. The anchor is stored once and never recomputed.
- **Why not a fixed first-N hash.** A Claude Code session changed item 2 of its own prompt between two turns (evidence §5). A hash recomputed at any N of 3 or more therefore drifts inside one session. A hash stored once is only the label binding again. Longest match needs no client knowledge and also returns how much overlaps.
- **Pools are sticky.** A pool is a Dynamo deployment with its own router. Roundhouse keeps a label on its pool. It moves a label only when the pool is down, when policy or budget refuses it, or when the guard rail refuses a new session.
- **The guard rail limits net-new prefill per pool per window.** Net-new prefill is admitted input tokens minus the cached tokens that are certain. An uncertain hit counts as net-new. The charge is corrected from the pool's measured `cached_tokens` after the response.
- **First measurable gain (M3):** the measured cache-hit ratio per pool, from Dynamo's `usage.prompt_tokens_details.cached_tokens` recorded as `CacheReadSource::Provider`, in `cache_reuse_evidence`. A two-pool mocker replay compares label stickiness against a load-only pool choice.

## 1. Terms

- **Item**: one canonical conversation item, as the surfaces canonicalize it.
- **Render**: `Item::render()` [rh crates/roundhouse-core/src/item.rs:419].
- **Chain value** `c_i`: the unkeyed digest of items `0..=i`. It stays in process memory.
- **Tip**: the keyed lookup key of a chain value at the end of a dispatched prompt or a settled response. Only tips are stored.
- **Label**: the qualified conversation key, `ControlPlane::qualify(principal, name)`, without the `#g{n}` generation suffix [rh crates/roundhouse-server/src/prefix_admission.rs:184].
- **Anchor**: a 16-byte keyed value that names one KV lineage. Forks that resend a lineage's prompt share its anchor.
- **Pool**: a Dynamo deployment with its own router. The embedded fleet is one pool whose router runs in process.
- **Class** of a first turn: **continuation** (known label), **inherited** (unseen label, tip matched), or **new** (unseen label, no tip matched).

## 2. The anchor: exact form

### 2.1 Reuse, not a second spelling

| Existing encoding | Relation to this design |
|---|---|
| `same_item` (prefix admission) | Not a digest. The chain must not split two claims that admission treats as one conversation. `same_item(a, b)` implies `render(a) == render(b)`, because the render leaves out `namespace` and `response_id` (evidence §2). The first test of M1 pins this. |
| `Item::render()` | **The per-item encoding the chain hashes.** It is already computed per item by `ContextAssembler::push`, `admitted_input_tokens`, and `turn_id_for` [rh crates/roundhouse-core/src/context.rs:216], [rh crates/roundhouse-server/src/engine.rs:1918], [rh crates/roundhouse-server/src/responses_api/wire.rs:221]. |
| `turn_id_for` (FNV-1a) | Unchanged. It is a durable idempotency id, and a change would make an in-flight retry miss its response. FNV is also unkeyed and not collision resistant, so it cannot be a routing key. |
| `prefix_fingerprint` | **A second spelling.** It serializes `(role, content)` with serde and includes `namespace`, so it can split two claims that `same_item` joins. M2 re-derives it from the chain (section 12, question 10). Its value is forwarded upstream as the fallback `prompt_cache_key` [rh engine.rs:3023], so the change moves that value once. |

### 2.2 Definition

```text
d_i  = SHA-256("rh-item-v1\0" || u64be(len(r_i)) || r_i)       r_i = items[i].render()
c_0  = SHA-256("rh-chain-v1\0" || d_0)
c_i  = SHA-256(c_{i-1} || d_i)                                   unkeyed, in memory only
t    = SHA-256(canonical JSON of the declared tools)            zero bytes when none are declared
k_P  = HMAC-SHA256(K, "rh-prefix-scope-v1\0" || namespace(P))   K = deployment secret, P = principal
L_i  = HMAC-SHA256(k_P, "rh-prefix-tip-v1\0" || t || c_i)[..16] the stored tip key
```

- **Where it is computed.** `ContextAssembler::push` already renders each item. It extends the chain from the same string, so the engine gets `c_i` beside `tokens_through(i)` [rh context.rs:231]. The handler uses the same core function over the claim of an unseen label. One function has two callers. There is no second copy.
- **Tools are not in the chain.** Tools are not items, and a tool change must not change the chain that the item history carries. The tools digest `t` enters only the tip key. So a tool change makes a new tip, as it makes a new provider cache prefix (evidence §8). The cost is that a session which appends MCP tools matches nothing that an earlier tool set wrote. That under-claims overlap, which is the safe direction.
- **Configuration is in the chain.** The leading configuration run is part of the dispatched prompt. When a client rewrites it, the KV beyond the edit is lost on every target, so the chain must break there too. A chain that excludes volatile configuration claims overlap that no cache holds.
- **The anchor.** At the first turn of an unseen label, the anchor is the anchor stored on the deepest matched tip. If no tip matches, the anchor is `L_{m-1}` over the first dispatched prompt of `m` items. The anchor is written once, in the label binding and in `SessionCreated`, and is copied to every later generation of the label.

### 2.3 The measured divergence index, per client

| Client | First item that differs between two sessions | Items that make a first request unique | Source |
|---|---|---|---|
| Claude Code 2.1.257 | 0, a 12-bit fingerprint of the typed prompt. Reliable at 4, the typed prompt. | All 6 of the first request | Measured, evidence §3 |
| Codex (pin `6344a65`) | 2 when the working directory or date differ, else 3 (the typed prompt) | All 4 of the first request (5 with multi-agent v2) | Derived from source and tests, evidence §6 |

The first request always ends just after the first typed prompt plus client context. So "the number of items that makes the sequence unique" is, in practice, the whole first request. The tip of the first request is exactly the owner's `hash(cat(message[i]) for i in 0..N)`, and it needs no per-client N.

## 3. Fixed anchor or longest match

| Question | (a) Fixed anchor at a unique boundary N | (b) Longest match over stored tips |
|---|---|---|
| Needs client knowledge | Yes. N is 4 for Codex, 5 or 6 for Claude Code, and moves with versions and features. | No. |
| Stable inside one session | No, if recomputed. Claude Code item 2 changed between turn 1 and turn 2, so any N of 3 or more drifts (evidence §5). If stored once, it is the label binding. | The label binding carries identity. The chain only has to match for unseen labels. |
| Answers "how much overlaps" | No. Yes or no only. | Yes. The matched tip carries its token count. |
| Fork under a new name | Joins only if its first N items equal the parent's. | Joins at the deepest stored tip it resends. |
| Codex v2 full-history fork | Splits at item 2 (evidence §7). | Splits at item 2. Correct for KV. |
| Router state | The anchor per label. | Tips with a TTL (section 4). |
| Reads per turn | 0 | 0 for a known label. One batched read for an unseen label. |

**Recommendation: (b) for placement, with the anchor found by (b) and stored once.** Both need router state. Only (b) is general, and only (b) produces the matched-token count that the price and the guard rail consume.

## 4. Stored state

```mermaid
sequenceDiagram
    participant H as Handler
    participant A as bind_prefix
    participant M as CorrelationMaps (memo, then Redis)
    participant E as Engine
    participant P as Pool
    H->>A: principal, name, claimed items
    A-->>H: session, delta, fresh or landed
    alt label known
        H->>M: label(key), memo hit on a warm node
        M-->>H: anchor, pool
    else label unseen
        H->>H: chain over the claim, tip keys L_0..L_{m-1}
        H->>M: one batched GET of the tip keys
        M-->>H: deepest hit (anchor, pool, tokens, at) or none
        H->>M: set label(key) = anchor, pool
    end
    H->>E: run_turn(session, delta, anchor, class, matched tokens)
    E->>E: quotes, policy, guard rail
    E->>M: write the dispatched tip (async)
    E->>P: dispatch
    P-->>E: stream, usage with cached_tokens
    E->>M: write the settled tip (async)
```

| Family | Key | Value | Size | Bound and eviction | Where |
|---|---|---|---|---|---|
| `label` | Qualified conversation key, no generation | anchor (16 B), pool id, `bound_at_ms`, `last_turn_at_ms` | About 60 B value | TTL `THREAD_BINDING_STALENESS_MS` (7 days), refreshed per turn [rh crates/roundhouse-core/src/control/correlation.rs:164]. Node memo capped at 4,096, like the generation memo. | Memory or Redis, the implementation the deployment already configures |
| `prefix_tip` | `L_i`, 16 B | anchor (16 B), the dispatch target's `Target::ledger_key` (a pool or a frontier model), `tokens` (u32, the local count through item i), `at_ms` | About 80 B value, about 160 B per Redis entry with overhead (estimate) | TTL 1 hour, refreshed on write. Node memo capped at 65,536 entries (about 6.5 MB), oldest first. | Same |

- **Only tips are stored.** A preamble is never a tip, because no request ends there. So "new" means that no stored tip matched. No "shared" flag and no write race are needed.
- **Growth.** Two tips per turn. At 10,000 turns per hour, about 20,000 live Redis entries, about 3.2 MB (estimate, not measured).
- **The tip names a target, not only a pool.** A fork of a session that was served by a frontier model inherits that model's warm prefix in its ledger seed, and a fork of a pool session inherits the pool.
- **Tip `tokens` leave out the tool declaration.** The ledger's `isl_tokens` include it [rh engine.rs:1918]. So a seed from a tip under-claims the warm prefix by the tool tokens, which is the safe direction.
- **Per-turn cost, known label.** One SHA-256 over the render bytes of each item, inside the assembler rebuild that already renders and tokenizes every item [rh engine.rs:2135]. Zero store reads for identity on a warm node. Two tip writes, pipelined, after dispatch and after the terminal. A turn routed to a pool also pays the guard rail's one window read and one write (section 8.5).
- **Per-turn cost, unseen label.** One chain over the claim (about 26 µs for a fresh Claude Code request at the measured 2.4 GB/s, evidence §11), one HMAC per boundary, and one batched `MGET` of up to 4,096 of the deepest boundaries. The node memo answers first.
- **Loss.** A lost tip costs one cold placement, which is today's behavior. A lost label binding makes the label unseen, and the longest match then finds its own last tip.

## 5. Message boundaries or token blocks

**Recommendation: message boundaries.**

- They are model-agnostic. One chain serves every pool and every frontier target. Provider caches also match exact prefixes, and a message boundary is where Roundhouse already places Anthropic block markers.
- They are computed before a target is chosen. Block hashes need the pool's tokenizer and block size, so a block chain is per pool, and a longest match across pools needs one chain per tokenizer.
- The cost is granularity. An edit inside an item loses the whole item. The Claude Code turn 1 to turn 2 pair shares 5,096 bytes at byte level and 162 bytes at item level (evidence §5). That under-claims, which is the safe direction.

**What Dynamo block hashes add when a pool exposes them.** Inside a pool, the router already matches blocks and can move KV east-west. Roundhouse does not need its own block index for a remote pool. Where a pool answers a residency query (the embedded fleet's `effective_prefill_tokens` [rh crates/roundhouse-fleet/src/local.rs:149]), that answer is the certain cache hit for the guard rail and replaces the message-level prediction for that turn.

## 6. Session labels

- **The label is the name prefix admission already resolves.** Responses: `thread-id`, then `session-id`, then `prompt_cache_key`. Messages: the Claude session header or `metadata.user_id`, scoped by agent id [rh crates/roundhouse-server/src/messages_api/wire.rs:236, :286]. A Claude sub-agent and a Codex sub-agent thread are therefore their own labels, and so new requests, as the owner ruled.
- **Unseen means no `label` entry anywhere.** The generation map cannot answer this question, because `bind_prefix` commits generation zero of a fresh key before the label is read [rh prefix_admission.rs:241, :426-441]. A node that never served the label reads through its memo to the store, as for generations. An expired or lost binding makes the label unseen again, and the longest match then finds the label's own last tip. That restores the same anchor and target.
- **History rewrite.** A new generation (`#g{n}`) or a compaction keeps the label, so it keeps the pool and the anchor. The chain of the new prompt writes new tips under the same anchor. The ledger of the new generation stays cold, as today [rh prefix_admission.rs:227-240]. The pool still holds the configuration prefix.
- **Client resets.** Claude Code `/clear` mints a new session id, so a new label. Its first request matches no tip, so it is new. That agrees with the KV unit.
- **Headerless clients.** An anonymous Messages request gets a fresh key per request [rh crates/roundhouse-server/src/messages_api.rs:546]. Each request is an unseen label, and its longest match finds the previous request's settled tip. So a headerless client gets pool stickiness without a name. Roundhouse writes no `label` entry for an anonymous key, because that key never repeats. Log continuity for such a client is unchanged.

## 7. Pool stickiness

### 7.1 Preference order

1. **Continuation**: the label's pool.
2. **Inherited**: the pool of the deepest matched tip. The label is bound to it.
3. **New**: a pool in its bring-up window with room on its rail (section 8.3). Else the pool with the most room on its rail.

### 7.2 When stickiness yields

| Condition | Action |
|---|---|
| Policy, budget, or the T6 egress rule refuses the pool | The filter wins. Stickiness is a preference inside the admitted set, never an override. |
| Pool unavailable (health check, connect failure, a 503 before the first byte) | Fail over to another pool of the same egress class under the same deadline. Mark the pool down for a cooldown. Rebind the label. |
| The rail refuses a **new** session | Next pool with room. No KV is lost by the move. |
| The rail refuses a **continuation** or **inherited** session | Stay. Wait for room up to a bounded time. A move prefills the whole prefix again elsewhere, which is the load the rail limits. Refuse when the wait ends (section 8.4). |
| A local-only session | Only pools of the local egress class. Never a frontier target. |

After a move, the label is bound to the new pool. The move costs one cold prefill, once.

### 7.3 What the learner and the rules picker see

Quotes, not a new key. A pool candidate's `expected_prefill_tokens` is its input minus the **expected** cached tokens, from the session ledger and the pool's `CacheModel` [rh crates/roundhouse-core/src/routing/ledger.rs:569]. On the first turn of an inherited label, the ledger is seeded with the matched tip's tokens and time for that pool only. The label's pool is warm in the quote and other pools are cold, so stickiness shows in price, TTFT, and cost. `LevelKey` and the rules picker do not change.

Two numbers, two purposes. The **price** uses the expected hit, so that it is unbiased (the cost-signal ruling). The **guard rail** uses only the certain hit, so that it errs toward over-counting load (section 8.1).

## 8. Bring-up and the guard rail

### 8.1 The quantity

```text
net_new(turn, pool) = admitted_input_tokens(turn) - certain_cached(turn, pool)

certain_cached =
    isl - effective_prefill_tokens     when the pool answered a residency query (embedded fleet)
    expected_cached_tokens             when the pool's CacheModel is Deterministic and elapsed < ttl
    0                                  otherwise, including every remote pool without a residency answer
```

- `admitted_input_tokens` is `Engine::admitted_input_tokens`, tools included [rh engine.rs:1918].
- **Reconciliation.** After the response, the window's charge for that turn is replaced by `prompt_tokens - cached_tokens` when the pool's count is `CacheReadSource::Provider` [rh crates/roundhouse-core/src/event.rs:105]. An unmeasured count keeps the full charge.
- **The bound on over-charge.** Before reconciliation, the over-charge is at most the cached prefix of each in-flight request on that pool.
- This mirrors the cost rule. An overstated hit undercounts net-new prefill and can flood a fresh instance. So only a certain hit reduces the charge.

### 8.2 Instance or pool

| Limit | Where it applies | What Roundhouse observes |
|---|---|---|
| Guard rail | **Per pool**, in Roundhouse, before dispatch. Roundhouse controls what enters each pool. | Its own charges. After dispatch, the measured `cached_tokens` and the serving `prefill_worker_id` and `decode_worker_id` from `nvext.extra_fields: ["worker_id"]` (evidence §9). |
| Per-instance prefill load | **Inside the pool**, in the Dynamo router, which books prefill load per worker. | Nothing before dispatch for a remote pool. The embedded fleet quote carries `load` per worker [rh local.rs:158]. |
| Bring-up | **Per pool** in Roundhouse, when a pool is added. Per instance inside a pool is Dynamo's. | The pool's `ready_at_ms` from configuration. Instance membership only after dispatch, from `worker_id`. |

Roundhouse cannot see an instance of a remote pool before dispatch without a worker hint. So this design applies both limits per pool, and asks the owner about hints (section 12, question 1).

### 8.3 Bring-up

- A pool record carries `ready_at_ms`. For a ramp window after it (configured, for example 5 minutes), the pool's rail limit rises in a straight line from a configured start to its full value.
- New sessions prefer a ramping pool while it has room. Continuing and inherited sessions never move to it, because a move re-prefills their prefix.
- The ramp limit is what staggers prefill on the new pool. Without it, every new session in the window lands at once.

### 8.4 When the rail trips

1. A new session goes to the next pool with room.
2. A continuing or inherited session waits for room, up to `min(max_rail_wait_ms, remaining turn deadline)`.
3. When no pool can take the turn, the rail removes all pool candidates. A frontier candidate that policy admits can still serve the turn, unless the session is local-only.
4. When no candidate remains, the turn is refused.

**The tie to D5.** The rail runs at routing, inside the spawned turn. Today a refusal there reaches the client as an in-stream failure, because the surfaces return the stream before the turn routes [rh crates/roundhouse-server/src/responses_api.rs:399, :466]. The recommendation for D5 is to hold the response headers until the engine records its routing decision or refuses, and for a pool also until the pool returns its response headers. The client waits for the first byte in both cases, so time to first token does not change. Then a rail refusal, or a pool's own 429 with no candidate left, can reach the client as HTTP 429 with `Retry-After`. Codex does not retry a 429 on its own (`retry_429: false`) and reads only the `usage_limit_reached` body with `resets_at` (evidence §10). The body for Codex is an owner question (section 12, question 12).

### 8.5 Where the rail's window lives

- **A shared rolling window per pool**, in the store that the deployment already configures, with the fair-use shape: check before dispatch, record after the terminal [rh crates/roundhouse-server/src/http.rs:645], [rh crates/roundhouse-server/src/engine/fair_use.rs:127]. A node-local window lets N nodes admit N times the limit, which defeats the rail.
- **Cost.** One window read at routing and one write at the terminal, per turn routed to a pool. Fair use already pays the same shape per turn.
- **Store outage.** The rail falls back to a node-local window with the limit divided by the configured node count, and logs once per outage. This differs from fair use, which fails closed. A capacity rail that refuses every pool turn during a store outage turns a store fault into a serving outage.

## 9. Scope and privacy

- **Keyed per principal.** `k_P` is derived from a deployment secret and the principal namespace that `ControlPlane::qualify` already uses. Two principals with the same content get unrelated tips. A lookup never crosses a principal.
- **Not reversible.** The store holds only 16-byte HMAC outputs. The unkeyed chain value `c_i` never leaves process memory, so a store reader cannot extend a chain to test guessed content. Without the secret, nobody can compute a tip from content.
- **Never exported.** Tips and anchors never go upstream, into metrics labels, into MCP answers, or into a frontier request. Logs carry at most the first 8 hex characters of an anchor.
- **No secret, no index.** Without the deployment secret, the tip families stay off and labels still work, because a label is a name and not a digest.
- **Rotation.** A new secret changes every tip. Labels survive, so continuing sessions keep their pools. Unseen labels are new until their tips are written again.

## 10. Learner impact (D8)

The learner groups by the anchor, which is the KV lineage that routing uses.

| Site | Today | Change |
|---|---|---|
| Arm assignment | `Arm::for_session` [rh crates/roundhouse-core/src/validate/arm.rs:105] | `Arm::for_anchor`, with `ASSIGNMENT_VERSION` `v2`. A fork that shares its parent's KV shares its parent's arm. |
| `min_sessions` at the gate | Distinct sessions [rh crates/roundhouse-core/src/routing/learn/gate.rs:215] | Distinct anchors. The key name is an owner question (section 12, question 11). |
| Estimand bootstrap | Clustered by session | Clustered by anchor |
| Exploration draw | Per session and response [rh crates/roundhouse-core/src/routing/learn/explore.rs:45] | No change |
| Learning entries | No lineage field | `anchor` with a serde default, for offline analysis |

Arm by anchor works because the anchor is known when the session is created (section 11).

## 11. Durability (D3, D4)

- **D3.** `SessionCreated` gains `anchor: Option<Anchor>` with a serde default. This is the forward-only door that `principal` and `arm` already use. A new event kind would make an older build fail to read the log, because `SessionEventKind` has no catch-all variant. A replay restores the anchor from the first event. The label binding can be rebuilt from it.
- **D4.** Label bindings and tips are soft state in `CorrelationMaps`, memory or Redis. The Redis entry is the shared answer, and the node memo is a write-through cache. A loss costs one cold placement.
- The per-session ledger stays a projection of the session log. The seed on an inherited first turn is recorded on the `Routed` decision that uses it, so a replay prices it the same way.

## 12. Questions for the owner

1. **Worker hints (unruled).** Recommend none toward a remote pool. The pool's router decides and can move KV east-west. The embedded fleet keeps its current in-process selection, which is that pool's router.
2. **Egress class of a pool (D6, unruled).** Recommend an explicit `egress: local | external` per pool in the catalog, with no default. A configuration without it is refused at load. Failover between pools of one class is allowed before the first byte.
3. **Instance or pool for bring-up and the rail (unruled).** Recommend per pool in Roundhouse, per instance in Dynamo (section 8.2). If the owner wants instance-level bring-up for remote pools, the only lever is `x-dynamo-worker-instance-id`, which is a worker hint.
4. **D5.** Recommend holding the response headers until the routing decision or refusal, and for a pool until its response headers (section 8.4).
5. **D7, headers toward a pool.** Recommend `x-dynamo-session-id` set to a keyed digest of the label, derived apart from the anchor. Send no parent header and no llm-d header. The pool is a Dynamo router.
6. **D9, the spawn-argument link.** Recommend dropping it. Fresh spawns are new requests by ruling.
7. **D10, opencode.** Recommend no opencode name in this workstream. Section 6 gives headerless Messages clients pool stickiness through tips. Whether a Responses request with no name is refused stays as is.
8. **New. Identical first requests share an anchor.** Two sessions with byte-identical first requests match each other's tip and share KV, so they share an anchor and an arm. Recommend accepting this.
9. **New. The Claude Code attribution block toward pools.** It is item 0, and it varies with the first prompt, so two sessions share 60 bytes in Roundhouse's item order instead of the system prompt (evidence §3, §8). Recommend dropping it from the dispatch projection toward non-Anthropic targets, only when detected exactly (the literal `x-anthropic-billing-header: cc_version=` block at `system[0]`). The canonical form and admission do not change. This is a client-specific step gated on exact detection, as the owner allowed.
10. **New. `prefix_fingerprint`.** Recommend re-deriving it from the chain in M2. This changes the forwarded fallback `prompt_cache_key` once, for Responses requests that send none.
11. **New. The `min_sessions` key.** Recommend renaming it `min_anchors`, with a load-time refusal of the old key that names the new one.
12. **New. The Codex body for a rail 429.** `usage_limit_reached` is the only 429 that Codex reads, and the rail does clear on its own. Recommend using it with `resets_at` set to the time the window has room.
13. **New. Long warm sessions on a remote pool.** Every turn is charged its full input until reconciliation (section 8.1). Recommend accepting this for M4 and deciding again with the M4 numbers.
14. **New. The deployment secret.** Recommend one secret for all nodes, from the existing secret configuration and never from the catalog.

## 13. Milestones

Each milestone is one PR, cut from `main`. Tests come first. Every run is bounded by a timeout.

**M1: the prefix chain (core, no call site that routes).**

- Tests first:
  - `same_item_agreement_implies_equal_item_digests`: property test over role, content, stamps, and the namespace rule.
  - `a_response_stamp_does_not_move_the_chain`.
  - `an_opaque_block_digests_the_same_in_any_key_order`.
  - `the_assembler_chain_equals_a_chain_recomputed_from_scratch`.
  - `two_principals_never_share_a_tip_key`.
  - `claude_fixture_divergence_is_pinned`: over the pinned 2.1.257 fixtures, two sessions share 0 item links, turn 1 to turn 2 shares 2, turn 2 to turn 3 and the tool loop share all.
- Change: the chain function beside `Item::render`, and the chain in `ContextAssembler`. HMAC-SHA256 through `hmac 0.12`, the RustCrypto crate of the `sha2 0.10` family.
- Done means: the tests pass, and a benchmark reports chain time against tokenization time per 100 KB with `crates/roundhouse-server/tests/data/tinyllama-tokenizer.json`.

**M2: labels and tips, in shadow.**

- Tests first:
  - `a_label_without_a_binding_costs_one_batched_tip_read` and `a_bound_label_costs_no_identity_read` (counters).
  - `an_expired_binding_finds_its_own_last_tip`.
  - `an_anonymous_key_writes_no_label`.
  - `a_fork_under_a_new_name_inherits_the_anchor`: a fixture body resent under a new session header.
  - `a_new_prompt_under_a_new_name_is_new`.
  - `a_history_rewrite_keeps_the_label_pool_and_anchor`.
  - `a_replay_restores_the_anchor_from_session_created`, and a `SessionCreated` written without the field still reads.
  - `an_expired_tip_is_not_matched`.
  - The `CorrelationMaps` contract suite for both new families, memory and Redis.
- Change: the `label` and `prefix_tip` families, `SessionCreated.anchor`, the lookup in the handler, tip writes in the engine, and `prefix_fingerprint` from the chain. Routing does not change.
- Done means: a replay of `use-cases/cache-aware-routing/turns.jsonl` reports class counts and a match-depth histogram, with numbers.

**M3: the Dynamo pool target with label stickiness (first measurable gain).**

- Tests first:
  - Against a stub pool: no client header survives, `nvext.extra_fields` asks for `worker_id`, and `cached_tokens` decodes as `CacheReadSource::Provider`.
  - `a_label_stays_on_its_pool_across_two_nodes` (two engines, one Redis map).
  - `a_policy_refusal_beats_stickiness`.
  - `a_local_only_session_never_leaves_the_local_class`.
  - `a_pool_503_before_the_first_byte_fails_over_and_rebinds`.
  - `an_inherited_first_turn_is_priced_warm_on_the_matched_pool_only`.
- Change: `Target::Pool`, the pool catalog with `egress` and `CacheModel`, the preference order of section 7, and failover between pools of one class.
- First test: `the_pool_reports_cached_tokens`. The Dynamo mocker at the pin does not appear to fill `prompt_tokens_details.cached_tokens` (evidence §9). Either an upstream mocker change fills it from the mocker's own prefill cost, or the run uses a real backend on an owner-approved machine.
- Done means: a two-pool run reports `cache_reuse_evidence` (`observed_total / paired`, measured reads only) per pool row, with stickiness on and with a load-only pool choice. It also reports predicted against observed. The gain is the difference, with numbers.

**M4: the guard rail, bring-up, and D5.**

- Tests first:
  - `an_uncertain_hit_is_charged_as_net_new`.
  - `a_measured_cached_count_reconciles_the_charge`.
  - `a_new_session_moves_when_the_rail_refuses`.
  - `a_continuing_session_waits_then_is_refused_with_retry_after`.
  - `a_ramping_pool_takes_new_sessions_and_never_continuing_ones`.
  - `the_headers_are_held_until_the_routing_decision`, with TTFT unchanged in the stub.
- Done means: a mocker run that brings up a second pool under load shows the net-new prefill per window on the new pool staying under its ramp limit, with numbers.

**M5: learner scope (D8).**

- Tests first: `forks_share_an_arm`, `the_gate_counts_distinct_anchors`, and `the_bootstrap_clusters_by_anchor`.
- Done means: a new assignment version, and the calibrator report names the cluster unit.

## 14. Relation to the rejected proposal

| Rejected text | Now |
|---|---|
| Program id from lineage headers and the emitted-item link | Replaced by the anchor from prefix match. Headers and the link are not used. The owner allows them only as accelerators under exact client detection, and none is needed for M1 to M5. |
| `ProgramRecord`, phases, sibling counts | Dropped. The pool's router owns placement inside the pool. |
| `ProgramBound`, `LedgerSeeded` events | Replaced by `SessionCreated.anchor` and the seed on the `Routed` decision. No new event kind. |
| llm-d trusted-hop headers | Out of scope. A pool is a Dynamo deployment. |
| Worker hints (P4) | Question 1. None by default. |

## 15. Out of scope

- Any change to prefix admission, to the conversation name, or to the refusal for a Responses request with no name.
- A Roundhouse block index for remote pools.
- Queue ordering inside a pool.

## Addendum, 2026-09-29: owner ruling on section 12, first round

- **Question 3 and question 1: accepted as recommended.** Bring-up and the net-new prefill rail apply per pool in Roundhouse and per instance in Dynamo. Roundhouse sends no worker hint toward a remote pool.
- **Question 4 (D5): accepted.** Roundhouse holds the response headers until the routing decision or the refusal, and for a pool until the response headers of the pool. A rail refusal or an upstream 429 then reaches the client as HTTP 429 with `Retry-After`.
- **Question 9: accepted, with a follow-up.** Roundhouse strips the Claude Code attribution block from the dispatch projection toward non-Anthropic targets, and only on the exact match. The owner asked whether the block can serve as a Claude session id. The orchestrator answer: not as an id. It is 12 bits, so two concurrent sessions collide at a probability of 1 in 4,096 per pair, and every prompt that is shorter than 5 characters maps to one value for each version. Exact detection of the block is a reliable signal that the client is Claude Code. Claude Code already sends `x-claude-code-session-id`, which is the exact label.
- **Question 2 (D6, the egress class of a pool): open.** The owner asked for more detail before a ruling.
