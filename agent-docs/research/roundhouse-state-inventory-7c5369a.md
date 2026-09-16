<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# DIVE D1-1 — The state inventory of roundhouse at `7c5369a`: every piece of state, which promise it serves, and what it costs on restart and on a second node

> **Status:** evidence only. Every ruling-shaped question is left in §10.
> **Date of read:** 2026-09-02.
> **Pin (primary):** roundhouse at **`7c5369a`** ("M13: the Redis fair-use
> ledger"). The working tree has moved past it (four dirty/untracked paths under
> `roundhouse-store-redis` and `roundhouse-server/tests`), so **every roundhouse
> citation below was read through `git show 7c5369a:<path>`**, not from the
> working tree. Line numbers are line numbers *in the blob at that revision*.
> **Pin (Relay):** `nemo-relay` and `nemo-relay-cli` **0.8.2**, published
> registry sources under
> `/root/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/nemo-relay-0.8.2`
> and `…/nemo-relay-cli-0.8.2` — cited as "0.8.2 registry".
> **Pin (codex):** `6344a65` — cited only where roundhouse's own code quotes it.
> **Method:** read-only. No cargo command was run and no process was started; a
> thermo-nuclear review owns cargo on this box. Every compile-level claim is
> made by citing the code rather than by building it.

---

## 0. What this document is for

D1 rules on a declared mode spectrum — **P0 proxy** (auth + routing + metering,
no durable log), **P1 ephemeral** (today's no-Redis default), **P2 durable**
(Redis behind whatever earns it) — and on how much of Relay's proxy posture to
adopt. That ruling needs one thing first: an honest list of *what state exists*,
where it lives, which product promise it is the mechanism for, and what it costs
when the process dies or when the client's next request lands on a different
node.

This is that list. §1 is the inventory. §2–§4 are the three axes the inventory
rows are scored on. §5 answers the P0 question. §6 answers the minimum-P2
question for the three M12.1 handoffs. §7 reads the composition root. §8 reads
Relay. §9 records two drifts found on the way.

---

## 1. The inventory

Nine store families exist at `7c5369a`. Two are durable-capable (session log,
spend ledger); one is durable-capable but only conditionally wired (fair use);
six are process-local with no durable implementation at all.

### 1.1 Summary table

| # | State | Type & site | Store family | Promise it serves | On restart | On a second node | Derivable? |
|---|---|---|---|---|---|---|---|
| 1 | Session event log | `SessionStore` trait, `store.rs:72` | session log (Memory or Redis) | replay/audit; prefix admission; idempotent retry; exact settle; routing warmth; savings dashboard | **P1:** all of it gone (`MemoryStore`, `store.rs:191-192`). **P2:** survives | **P1:** invisible. **P2:** shared, fenced by lease | — (it *is* the truth) |
| 2 | Session lease | `Lease{node_id, fencing_token, expires_at_ms}`, `store.rs:52-63` | session log | single-writer / no split-brain log | lapses by TTL; successor acquires | the whole point: fences the predecessor | derivable from store |
| 3 | `SessionState` projection | `session.rs:317-375` (`items`, `ledger`, `turn_index`, `completed_turns`, `open_turns`, `configuration`, `pending_routings`, `frontier_history`) | none — rebuilt per open | prefix admission; idempotent retry; routing warmth; escalation/validate arm | rebuilt from the log on `Session::open` (`session.rs:4-10`) | rebuilt identically on any node holding the lease | **derivable from the log** |
| 4 | Committed spend + holds + settle watermarks | `ProjectAccount{committed_usd, member_committed_usd, holds, watermarks, window_started_ms}`, `spend.rs:406-421`; `MemorySpendLedger`, `spend.rs:465-467` | spend ledger (Memory or Redis; Redis keys at `store-redis/src/spend.rs:15-19`) | budgets; exact settle; idempotent retry of a settle; drift reconciliation | **P1:** month's committed spend forgotten, budget handed back (`main.rs:18-21`). **P2:** survives | **P1:** two independent ceilings. **P2:** one atomic ceiling | ledger figure is *not* derivable from the log — it is the enforcement counter, deliberately not `measured_usd` (`spend.rs:6-12`) |
| 5 | Rolling fair-use buckets | `MemoryFairUseLedger{scopes: Mutex<HashMap<(ProjectId, Option<UserId>), Buckets>>}`, `fair_use.rs:466-467`; Redis layout at `store-redis/src/fair_use.rs:11-14` | fair-use ledger | fair use (rolling 5h/24h/7d ceilings) | **Memory:** every counter resets (`fair_use.rs:61-66`). **Redis:** survives, `PEXPIRE`-pruned | **Memory:** "a project capped at 2M tokens per 5 hours can draw 2M through each" (`main.rs:796-798`). **Redis:** one shared ceiling | not derivable from the log (a draw is recorded, never replayed) |
| 6 | `Conversations::generations` | `HashMap<String, u32>`, `conversations.rs:100` | Conversations (node-local) | prefix admission of resent history — which `#gN` a cache key names | **lost**; `bind` re-derives generation zero (`conversations.rs:374`, `bound_session` `:533-538`) | **not visible**; a reader on a fresh node refuses (`resolve` → `None`, `:431-435`) | **only knowable by having watched** (it counts prefix-check failures) |
| 7 | `Conversations::latest` | `HashMap<Principal, SessionId>`, `conversations.rs:102` | Conversations | an MCP call that names no conversation | lost; next unnamed MCP call gets `SurfaceError::NoSession` (`reads.rs:286-289`) | not visible; same refusal | **only knowable by having watched** |
| 8 | `Conversations::calls` (`CallTable`) | `HashMap<Principal, PrincipalCalls>` + per-principal `VecDeque` cap 4096, `conversations.rs:139-183` | Conversations | exact correlation of an MCP `tools/call` to its conversation (Claude Code's `_meta["claudecode/toolUseId"]`) | lost; falls back to `latest` | not visible; per-node only | **only knowable by having watched** — written at the moment the call is streamed (`follower.rs:256-263`) |
| 9 | `Conversations::threads` (`ThreadTable`) | `HashMap<Principal, PrincipalThreads>` + per-principal `VecDeque` cap 1024, `conversations.rs:278-305` | Conversations | exact correlation for codex subagents (`_meta.threadId` ↔ `x-codex-turn-metadata`) | lost; falls back to the R-M7 named path then `latest` | not visible; per-node only | **only knowable by having watched** — "the thread id is not in the log" (`conversations.rs:488-489`) |
| 10 | Overlays | `HashMap<SessionId, OverlayEntry>`, `mcp/src/store.rs:203` | ControlStore (node-local) | steering and overlays — an agent's standing narrowing | lost; widens back to the deployment ceiling, never past it (`mcp/src/store.rs:17-24`) | applies only on the node that took the MCP call (same cite) | not derivable — an agent's request, held nowhere else |
| 11 | Intents | `HashMap<SessionId, IntentRecord>`, `mcp/src/store.rs:204` | ControlStore | `declare_intent` → routing priors for the next turn | lost | node-local | not derivable |
| 12 | Advisory outcomes | `HashMap<SessionId, OutcomeRecord>`, `mcp/src/store.rs:205` | ControlStore | the agent's self-reported result of a steer | lost | node-local | not derivable |
| 13 | Session bindings | `HashMap<BindingId, SessionBinding>`, `mcp/src/store.rs:206` | ControlStore | correlation of an MCP connection to a conversation via an `rhb_…` token the client echoes into its own history | lost — but the *token* survives in the log, so the join fails closed | node-local | **half-derivable**: `binding_ids_in_items` scans the log for the token (`mcp/src/store.rs:542`), the record it resolves to is process state |
| 14 | ControlStore sweep cursor | `next_sweep_at_ms: u64`, `mcp/src/store.rs:209`; `RETENTION_MS = 24h`, `:97` | ControlStore | bounds families 10–13 | resets to zero → first write of the process sweeps | node-local | — |
| 15 | Metrics fold | `MetricsRecorder{fold: Arc<RwLock<MetricsFold>>}`, `metrics/mod.rs:156-158`, fed as a `SessionObserver` (`:247-251`) from `Session::open_observed` (`engine.rs:1075-1083`) | node-local aggregate | the savings dashboard; drift reconciliation's `measured_usd` half | **partially recovers**: a session re-opened after a restart replays into the fold (`engine.rs:1071-1074`); a session never re-opened is simply absent from the dashboard | **each node has its own** `/v1/metrics` (`metrics_api.rs:6-11`) | **derivable from the log** by replaying it through the identical fold (`metrics/mod.rs:14-19`) — but nothing at boot does that replay |
| 16 | Compiled control plane + admin directory | `Managed{current: RwLock<Compiled>, version}`, `control_config/directory.rs:311-324, :253`; store trait at `directory/store.rs:55-60`; only impl `MemoryDirectoryStore` (`directory.rs:75-79`) | admin directory (memory only) | tenancy, budgets, key revocation | **admin-created tenancy dies**, and the specific hazard is named: an archived project's tombstone is lost and its id can be recreated, "silently joining the new tenant to the old one's spend history in the ledger that DID survive" (`main.rs:838-849`) | two directories that never converge (`directory.rs:75-79`); revocation bounded by `admission_cache_ttl_ms` (`directory.rs:65-71`) | file-owned rows are re-projected from the file every read (`directory.rs:41-45`); admin-owned rows are not derivable from anything |
| 17 | Turn gates | `Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>`, `engine.rs:847`; "Entries are never removed" (`:845-846`) | engine process state | one turn at a time per session *within a node* | lost (harmless) | **no cross-node equivalent** — the lease's fencing token is the only cross-node authority (`engine.rs:842-844`) | — |
| 18 | `unread_recipe` warn-once | `std::sync::Once`, `engine.rs:865` | engine process state | operator honesty about an unread `tiers` recipe | re-fires once | per node | — |
| 19 | Cache ledger seed | `CacheLedger::new()` + catalog, `engine.rs:1014-1018`, folded per session in `SessionState::ledger` (`session.rs:325`) | none — per-open | routing warmth (`p_hit(elapsed) * last_prefix_tokens`, `routing/ledger.rs:6-15`) | rebuilt from the log; a fork resets it to cold, stated as the conservative direction (`responses_api.rs:596-601`) | rebuilt identically wherever the lease is | **derivable from the log** |
| 20 | Per-request SSE follower | `MessagesFollower` phase/cursor/queue, `messages_api/follower.rs:20-48` | request-scoped | streaming a turn as it commits | dies with the connection | irrelevant — it follows the log, and a second node can follow the same log under P2 | derivable from the log |

### 1.2 Rows that are not state at all, recorded so the list is closed

- **`topham`** (`crates/topham/src/lib.rs:4-30`) is a launcher that `execve`s
  the agent; a profile "names things and never holds one" (`:32-38`) and refuses
  a file carrying a key (`profile.rs:14-21`). It holds no server state and is
  not on the spectrum.
- **`roundhouse-relay`** (`relay/src/lib.rs:22-33`) produces ATOF/ATIF/summary
  as **pure functions of the log**: "No clock, no `Uuid::new_v4`, no ambient
  configuration". It adds no state; it consumes row 1.
- **Catalog, provider clients, judge spec, tokenizer** are boot configuration
  (`main.rs:642`, `:994-1000`), not state.

---

## 2. The derivability axis

Three classes, and the class is what decides whether a mode change is a
degradation or a deletion.

**(a) Derivable from the request** — the client resends it. Prefix admission
exists *because* agentic clients resend their whole history every turn
(`responses_api.rs:19-21`). The conversation content itself is in this class:
under any mode, the client can re-supply it.

**(b) Derivable from the log** — rows 3, 15, 19, 20, and the `rhb_` token half
of row 13. `store.rs:6-10` states the rule: "Conversation items and the routing
ledger are *projections* of the event log rather than separately stored
collections, so there is exactly one write path and no way for the log and the
materialized state to disagree after a crash." `session.rs:4-10` restates it for
the session, `metrics/mod.rs:14-19` for the dashboard.

**(c) Only knowable by having watched** — rows 6, 7, 8, 9, 10, 11, 12, and the
record half of 13. These are the ones a P0 proxy cannot reconstruct at any
price, and the ones the M12.1 review's three handoffs are about. The module doc
says it for the thread table in one sentence: "Nothing later can reconstruct the
pairing — the thread id is not in the log, and the cache key that is in it is
the whole agent family's rather than this thread's" (`conversations.rs:488-489`).

Row 4 (committed spend) is its own case: **not derivable from the log by
design**. `spend.rs:6-12` — "This is not the answer to 'what did this project
spend.' That number is `measured_usd`, folded from session logs … This is
`committed_usd`, an enforcement counter — and naming the two differently and
never summing them is the design". The reconciliation view exists to show the
two side by side (`admin_api/reconciliation.rs:4-5`).

---

## 3. What breaks on restart, quoting the code

**The session log, under P1.** `MemoryStore{sessions: Arc<RwLock<HashMap<…>>>}`
(`store.rs:191-192`) and the boot warning: "no Redis configured; sessions and
committed spend are in-memory and die with this process" (`main.rs:866-870`).

**The budget, under P1.** `main.rs:16-21`: a URL set but unreachable stops the
process, because falling back to memory "would demote it for the ledger too: a
process that forgets a month's spend on restart hands the budget back while the
log that proves it was spent survives."

**Generations, under every mode.** `bind` writes generation zero for an unknown
key (`conversations.rs:374`) and `bound_session` maps generation zero to the key
verbatim (`conversations.rs:533-538`), so "the common case is a conversation that
never forked, and it re-binds to the same log". A conversation that *had* forked
before the restart does not: it re-derives at zero, and the first disagreeing
history forks it to a freshly re-derived `#g1` — **M12.1 handoff (a)**, below.

**`latest`, calls and threads.** All three are `HashMap`s behind one `Mutex`
(`conversations.rs:74`, `:100-108`). The module doc's honest cost statement is
`conversations.rs:41-51`.

**Overlays, intents, outcomes, bindings.** `mcp/src/store.rs:17-24` — "an
overlay does not survive a process restart … Both are acceptable *because of
what an overlay is* — a narrowing, so losing one widens back to the deployment's
ceiling and never past it".

**Admin-created tenancy.** The most consequential restart loss at this revision,
and it is a *cross-family* one: the tombstone is lost while the ledger survives
(`main.rs:838-849`).

**The dashboard, partially.** `engine.rs:1071-1074`: the session is opened
*observed*, "so a node that restarts and picks a session back up recovers that
session's accounting instead of reporting only what it served since booting."
Nothing replays sessions that are never re-opened — **negative claim, see §9.2.**

---

## 4. What breaks on a second node (the reconnect case)

**Session log and spend ledger:** correct under P2, because the lease fences
(`store.rs:46-51`) and each spend operation is one atomic script
(`store-redis/src/spend.rs:6-13`). Under P1 the second node sees nothing.

**Fair use:** correct under Redis *and a configured `fair_use` block*; otherwise
two ceilings (`main.rs:794-802`).

**Everything in `Conversations` and `ControlStore`:** node-local by
construction. `conversations.rs:41-51` states the whole cost: "a client that
reconnects to another node keeps its cache key and loses its generation, which
re-derives on the first request that disagrees with the log; and an MCP call on
a node that has served none of this principal's turns is refused rather than
answered — whether it named a conversation, correlated one, or omitted both.
Refusals and re-derivations, never a wrong session served quietly."

That last property is **new at M12.1 and load-bearing**: `resolve` returns `None`
for a key this node holds no binding for (`conversations.rs:431-435`), because
"a generation-zero id *exists in the shared store* whenever any node ever created
it — so a call landing on a fresh node was served the pre-fork log with a 200 on
it while another node held the conversation the client was actually in"
(`conversations.rs:53-63`).

**The admin directory:** "a two-node deployment has two directories that never
converge" (`directory.rs:75-79`).

**The dashboard:** each node folds only what it served (`metrics_api.rs:6-11`,
`engine.rs:776`), so `/v1/metrics` on two nodes gives two partial answers with no
aggregator.

**Turn gates:** per node only; the lease is the cross-node authority
(`engine.rs:840-846`).

---

## 5. Under a P0 proxy that keeps no log

P0 is "auth + routing + metering, no durable log". Scoring each promise against
the inventory:

### 5.1 Survives as-is

- **Auth, tenancy resolution and policy narrowing.** `ControlPlane::scope` is a
  hash-and-look-up against compiled configuration (`control_config/mod.rs:33-44,
  :46-57`); nothing in it reads a log.
- **Routing *decision* itself, minus warmth.** `AffinityPolicy`/`StagePolicy`
  take a candidate list and a policy (`main.rs:1044-1046`); the catalog is
  configuration.
- **Fair use.** `record_draw`/`would_exceed` never read the log
  (`fair_use.rs:37-45`); the buckets are their own store.
- **Budgets as an admission ceiling.** `open_grant` is one atomic operation over
  the ledger (`spend.rs:21-28`). The *grant* half survives P0 intact.
- **Pass-through auth and the MCP tool surface's own shape.** Neither reads a log.
- **Relay-format emission of a live stream**, if and only if the proxy holds the
  turn's events in memory long enough — but see §5.3, because
  `relay/src/lib.rs:15-20` explicitly contrasts roundhouse's log with Relay's
  in-memory exporter and calls the log "a strictly *better producer*".

### 5.2 Degrades to a stated guess

- **Prefix admission of resent history.** `bind_prefix` checks the client's claim
  against `stored_conversation(store, …)` (`responses_api.rs:587`). With no log
  there is nothing to check against, so the honest P0 posture is *admit whatever
  is claimed* — which is a guess that the client is telling the truth. Concretely
  it costs the fork detection: a compaction becomes invisible, and the routing
  ledger's warm-prefix claim (§5.3) is then made against a prompt that changed
  shape — the exact mispricing `responses_api.rs:596-601` says forking exists to
  avoid.
- **Routing warmth.** `CacheLedger` reconstructs an unqueryable cache from "what
  we last sent to that target, how long ago" (`routing/ledger.rs:6-9`). A P0
  proxy can keep that per-connection in memory; what it loses is the
  *append-only* premise the model rests on — "Within one session we send the
  whole conversation every turn, so whatever we sent to a target last time is a
  *prefix* of what we are about to send" (`routing/ledger.rs:11-15`) — because
  without §5.2's admission check it cannot know the premise still holds.
  `invalidate` exists for exactly that case (`:19-21`) and would have no trigger.
- **Idempotent retry.** Dedup is `completed_response_for(&turn_id)` off the
  session's own projection (`session.rs:1301-1315`, `:1000-1002`). In-memory per
  connection it works for the life of that connection; across a reconnect it
  degrades to "run the turn again", and the client is billed twice. The usage a
  dedup must replay came from the provider and cannot be recomputed
  (`session.rs:67-72`).
- **The savings dashboard.** The fold is already a live in-memory aggregate
  (`metrics/mod.rs:156-158`), so a P0 proxy can produce the same numbers for
  traffic it saw. What it loses is the recomputability claim — "a process that
  wants to rebuild … replays the log through the identical fold and must get the
  identical answer" (`metrics/mod.rs:14-19`) — so the dashboard becomes an
  unauditable counter rather than a projection.
- **Correlation of an MCP call to its conversation.** Rows 8 and 9 are already
  process-local and already survive nothing; a P0 proxy is no worse *for a single
  process*. The degradation is that under P0 there is no `Conversations::resolve`
  refusal to fall back on, because there is no shared store for a generation-zero
  id to exist in — so the "refusals and re-derivations, never a wrong session
  served quietly" guarantee (`conversations.rs:50-51`) has nothing to stand on.

### 5.3 Simply gone

- **Replay/audit.** ATOF, ATIF v1.7 and `LlmOptimizationSummary` are produced by
  cold replay of a finished session (`relay/src/lib.rs:24-33`: "These documents
  are produced by *cold replay* of a finished session, so two exports that
  disagreed would mean the log was not the source of truth after all"). No log,
  no cold replay, and the crate's central claim — that roundhouse is a better
  ATIF producer than Relay's own exporter because Relay's "accumulates in memory
  and is lost with the process" (`relay/src/lib.rs:15-20`) — inverts.
- **Exact settle.** The settle is driven off `SessionState::last_settlement`
  (`session.rs:1009-1013`) and re-driven by "the replay every session already
  performs when it is next opened" (`spend.rs:39-41`). With no log there is no
  replay, so a process that dies between dispatch and settle leaves a hold that
  lapses by TTL and **spend that is never applied at all** — not the bounded
  "one turn per dead session" the ledger accepts today (`spend.rs:43-48`).
- **Drift reconciliation.** It is definitionally the comparison of the ledger's
  `committed_usd` against the log's `measured_usd`
  (`admin_api/reconciliation.rs:4-5`, `spend.rs:6-12`). Delete one column and the
  view is not degraded, it is meaningless.
- **Steering as it exists today.** M10.0 moved the correction's payload *into*
  the log (`main.rs:955-960`: "the correction is a conversation item now … so it
  lives in the session log with everything else"), and `fetch_steer` is now "a
  pure read of the log, and a restart costs nothing" (`mcp/src/reads.rs:46-50`).
  Under P0 the steer would have to move back out of the log into node-local
  state — i.e. P0 *reverts* a shipped improvement rather than merely not
  extending it.
- **The `rhb_` binding join.** `binding_in_log` resolves tokens found in the
  session's items (`mcp/src/store.rs:495-509`); with no items there is nothing to
  scan. (Note it has no production caller at this revision — `mcp/src/store.rs:487-494`.)

---

## 6. The minimum P2-durable set that makes handoffs (a), (b), (c) go away

The three handoffs, restated from `PLAN-anthropic-messages.md:1140-1149`.

### (a) The re-derived `#g1` with a duplicated prefix

**Mechanism, in code.** After a restart `generations` is empty, so `bind`
inserts zero (`conversations.rs:374`) and `bound_session(key, 0)` returns the key
verbatim (`conversations.rs:533-538`). If the client's history then disagrees
with *that* log, `fork` increments to 1 and returns `key#g1`
(`conversations.rs:404-411`) — which may be a session id that **already exists**
from before the restart. The fork arm then does:

```
let session_id = conversations.fork(principal, &key);
create_session(engine, &session_id).await?;
Ok((session_id, claimed))
```

(`responses_api.rs:602-604`) — it returns `claimed` **whole**, with no `admit`
check, on the stated premise that "It gets a fresh internal session, which is
empty and so agrees trivially; no second check is needed"
(`responses_api.rs:594-595`). `create_session` discards the "already existed"
bool (`responses_api.rs:612-616`; the store's `create_session` returns `false` if
it existed, `store.rs:73-78`). So the pre-restart `#g1` log gets the claimed
history appended on top of the history it already holds: **a duplicated prefix,
not a wrong session.**

**Minimum durable fix.** Either of two, and they are not equivalent:

1. **Durable `generations`** — one `(namespaced_key → u32)` mapping in the
   shared store. This removes the *cause*: the generation never re-derives, so
   the fork lands on a genuinely fresh `#g2`. It also fixes (b) and (c)'s
   node-locality for the *named* path in one write. Cost: one store read on the
   admission path of every turn, where today it is a `HashMap` lookup under a
   `Mutex` (`conversations.rs:365-377`).
2. **Make the fork arm check** — run `admit` against the forked-to session
   instead of assuming emptiness. This removes the *symptom* with no durable
   state at all, and is strictly smaller; it does not fix (b) or (c). It is
   arguably correct independent of the mode ruling, since the premise
   "a fresh internal session … is empty" is false for any id the store already
   holds, restart or not.

The evidence does not decide between them; §10 keeps it open.

### (b) `generations` holds one entry per cache key served

**Mechanism.** `bind` writes rather than reads-with-a-default, and the comment
says why: "the entry's *presence* is what tells a reader this node bound the key
at all (M12.1 review, F9). A `get(..).unwrap_or(0)` here would leave every
never-forked conversation indistinguishable — to `resolve`, on the very node
serving its turns — from a key this process has never heard of. The cost is one
entry per distinct cache key served instead of one per key that forked"
(`conversations.rs:366-373`).

So (b) is not a bug; it is a **growth profile**: an unbounded `HashMap<String,
u32>` keyed by `{project}/{user}/{cache_key}`, with no cap and no sweep — unlike
`CallTable` (cap 4096, `conversations.rs:183`), `ThreadTable` (cap 1024,
`:305`), and `ControlStore` (24h sweep, `mcp/src/store.rs:97`, `:230-244`).
`generations` is the **only** one of the six node-local families with neither a
cap nor a retention sweep — searched: `conversations.rs` contains exactly two
`while … .len() > …` eviction loops (`:226-231`, `:344-349`), both in the call
and thread tables, and `Inner` has no sweep method.

**Minimum durable fix.** The same durable `(key → generation)` mapping as (a).
Once presence is a fact about the *deployment* rather than about this process,
the entry is no longer doing double duty as "did I bind this", and the map is
the store's problem — "the same growth profile the store already carries a log
for" (`PLAN-anthropic-messages.md:1147-1149`). Note the fix is *not* a cap: a cap
on `generations` would silently re-derive a live conversation's generation to
zero, which is (a) again on a busy node.

### (c) Thread and call tables are node-local, so exact correlation is per node

**Mechanism.** Both are per-principal `HashMap`s inside `Conversations`
(`conversations.rs:104-108`), written at the two moments the pairing is knowable:
`bind_call` from the streaming emission (`follower.rs:256-263`) and `bind_thread`
from the Responses ingest after `bind_prefix` decided the session
(`responses_api.rs:482-485`).

**Minimum durable fix.** A durable `(principal, tool_use_id) → SessionId` map and
a durable `(principal, thread_id) → SessionId` map, both with the *same* TTL
semantics the in-process caps stand in for. Three properties must survive the
move, and each is already written down as a decision:

- **Partitioned by principal** — a local backend that numbers calls `call_0`,
  `call_1` hands the same string to every conversation it serves
  (`conversations.rs:126-137`).
- **`Ambiguous` is a remembered state, not a deletion** — "an id dropped from the
  table reads as never-seen, so the *next* binding of the same colliding id would
  look like a first one and start answering confidently again"
  (`conversations.rs:158-163`).
- **Threads rebind, calls do not** — "A thread id names a *conversation*, and a
  conversation legitimately moves: every fork mints a new session for the same
  thread" (`conversations.rs:266-276`).

### 6.1 The minimum P2 set, stated once

**Three durable maps, and nothing else, closes (a), (b) and (c):**

| Map | Key | Value | Closes |
|---|---|---|---|
| generations | `{project}/{user}/{cache_key}` | `u32` | (a), (b) |
| calls | `(Principal, tool_use_id)` | `SessionId` \| `Ambiguous` | (c) |
| threads | `(Principal, thread_id)` | `SessionId` | (c) |

`latest` (row 7) is **not** in the minimum set: no handoff names it, and it is
the one row the module doc already calls a guess weighed as one
(`mcp/src/reads.rs:90-94`). Neither is `ControlStore` (rows 10–13): its loss
widens to the ceiling and never past it (`mcp/src/store.rs:17-24`), and its
durable shape is explicitly M8's to decide (`mcp/src/store.rs:6-14`). The admin
directory (row 16) is a *separate* and larger durability gap with its own written
unlock condition and its own two-placement decision
(`directory.rs:81-110`) — it is not part of the D1 minimum, but §9.1 notes it is
the one restart loss that can corrupt a surviving store.

---

## 7. What `ROUNDHOUSE_REDIS_URL` switches today, and what it does not

`REDIS_VAR = "ROUNDHOUSE_REDIS_URL"` (`main.rs:95`). The composition root reads
it once (`main.rs:781`) and branches (`main.rs:805-885`).

**It switches, all at once (`main.rs:11-16`):**

1. the **session store** — `RedisSessionStore::connect` (`main.rs:807`);
2. the **spend ledger** — `RedisSpendLedger::connect` (`main.rs:810`);
3. the **fair-use ledger**, *conditionally* — only when some membership also
   configures a `fair_use` block (`fair_use_backend`, `main.rs:920-925`), because
   "connecting a third handle for a counter nothing will read would be a startup
   dependency bought for nothing" (`main.rs:915-919`).

An unreachable URL **stops the process** rather than falling back
(`main.rs:16-21`), and the reason given is that a silent demotion would break the
one property the variable promises.

**It does not switch — and these are the P1 rows that stay P1 no matter what is
in the variable:**

- `Conversations` — minted unconditionally inside `serve`, on both arms
  (`main.rs:974`);
- `ControlStore` — likewise (`main.rs:975`);
- the **admin directory**, which stays `MemoryDirectoryStore` on both arms
  (`main.rs:715-719`), and whose loss beside a surviving ledger is warned about
  explicitly on the Redis arm only (`main.rs:835-850`);
- the **metrics fold**, owned by the engine (`engine.rs:776`, `:927`);
- **turn gates** (`engine.rs:932`).

`serve` is generic over `S: SessionStore` and takes `spend` and `fair_use` as
trait objects (`main.rs:962-973`); everything else it constructs itself. **So the
durability seam is exactly three arguments wide**, and the six node-local
families are not reachable from the variable at all.

---

## 8. Relay's proxy posture at 0.8.2, and what there is to adopt

Read to answer "how much of Relay's proxy posture to adopt rather than rebuild".

**Relay's gateway is a proxy with in-process session state and nothing durable.**
`nemo-relay-cli-0.8.2/src/sessions/mod.rs:64-72`:

```
pub(crate) struct SessionManager {
    inner: Arc<Mutex<HashMap<String, Session>>>,
    authenticated_owners: Arc<Mutex<HashMap<String, String>>>,
    alignment: Arc<Mutex<SessionAlignmentState>>,
    default_config: GatewayConfig,
}
```

Three `Arc<Mutex<HashMap<…>>>` and a config [fact-check 2026-09-02: two of the three are literally `HashMap`-typed; the third, `alignment`, is `Arc<Mutex<SessionAlignmentState>>`, a named struct wrapping two `HashMap`s (`alignment.rs:287-289`) — three map-shaped node-local stores, as stated] — structurally the same posture as
roundhouse's `Conversations` and `ControlStore`, with no store behind any of it.

**Its eviction is an idle sweep, not a cap.** `AGENT_IDLE_TIMEOUT = 30s` and
`AGENT_IDLE_SWEEP_INTERVAL = 5s` (`nemo-relay-cli-0.8.2/src/sessions/idle.rs:18-19`),
run by `close_idle_sessions_from_parts` (`:36-45`). That is a *third* bounding
discipline beside roundhouse's two (per-principal LRU caps in `Conversations`,
a 24h retention sweep in `ControlStore`), and it is the one shaped for a proxy:
it bounds by *liveness* rather than by count or age.

**Its correlation is scored, not exact.** `hint_match_score`
(`nemo-relay-cli-0.8.2/src/sessions/correlation.rs:8-34`) adds weights —
subagent/agent 8, conversation 4, generation 4, request 4, model 1 — and
tool-call hints are extracted from three provider response shapes by scanning
(`:38-47`). This is the direct counterpart of roundhouse's `CallTable`/
`ThreadTable`, and the contrast is the interesting part: **roundhouse's is exact
because it binds an id it emitted itself** (`conversations.rs:24-35`,
`follower.rs:242-263`), where Relay's is a best-match over hints because it never
emitted them.

**Relay stamps its own routing identity onto the upstream request.**
`enrich_routing_identity_headers` clears reserved headers and inserts
`x-nemo-relay-session-id`, `x-nemo-relay-agent-kind`, `x-nemo-relay-turn-id`
(`nemo-relay-cli-0.8.2/src/sessions/mod.rs:85-108`). That is a mechanism
roundhouse does not have an equivalent of, and it is what lets a downstream
component correlate without any shared table.

**Relay's own durability, searched.** `grep -rn "redis\|durable\|persist"` over
`nemo-relay-0.8.2/src` returns six hits, all in `src/plugin/dynamic*` [fact-check 2026-09-02: these hits and `atif.rs` are in the core crate `nemo-relay-0.8.2`, not `nemo-relay-cli-0.8.2`; the facts stand] (a plugin
registry record: `plugin/dynamic.rs:6, :327, :347`;
`plugin/dynamic/registry.rs:26, :35`; `plugin/dynamic/manifest.rs:382`).
**Negative claim: there is no session store, no log, no Redis and no persistence
of any conversation state anywhere in `nemo-relay` 0.8.2.** Its ATIF exporter is
`state: Arc<Mutex<AtifExporterState>>` (`nemo-relay-0.8.2/src/observability/atif.rs:346`)
with per-`Uuid` `HashMap`s (`:1622-1628`), which is exactly what
`roundhouse-relay/src/lib.rs:15-20` characterises as "accumulates in memory and
is lost with the process".

**What that means for D1, without ruling:** Relay *is* the P0/P1 reference
implementation. Adopting its posture means adopting in-process maps with an idle
sweep and scored correlation; roundhouse today has strictly more (exact
correlation from self-emitted ids, plus a durable log under two of nine
families). The adoptable pieces are the **idle-sweep bounding discipline** and
the **routing-identity header stamping**; the piece that is *not* adoptable
without loss is the scored correlation, because roundhouse's exact binding is the
thing M12/M12.1 shipped and is the thing the three handoffs are about.

---

## 9. Two things found on the way

### 9.1 The one restart loss that corrupts a surviving store

Every other node-local loss degrades safely (widen to ceiling, refuse, re-derive).
The admin directory does not: `main.rs:838-849` states that losing an archived
project's tombstone lets the id be recreated, "silently joining the new tenant to
the old one's spend history in the ledger that DID survive." That is a
node-local loss producing a **wrong number in a durable store**, and it is the
only instance of that shape in the inventory. It is warned about only on the
Redis arm — which is correct (there is no surviving ledger to corrupt on the
memory arm), and worth noting because it means the hazard *appears* when a
deployment upgrades P1 → P2 partially.

### 9.2 Two documentation drifts

- **`roundhouse-mcp/src/store.rs`'s module doc is stale on one of its four
  families.** It says "overlays, intents, steer payloads, bindings"
  (`:4`, `:26-34`, `:69-75`), but `Inner` holds `overlays, intents, outcomes,
  bindings` (`:203-206`) — `grep -n steer` over the file returns only doc-comment
  hits, no field and no method. `main.rs:955-960` records the supersession
  ("The steer used to be the second half of that sentence and is not any more …
  this store holds only overlays, intents, bindings and the advisory outcome"),
  so the fact is written down — in the other crate. Low stakes; it matters for
  D1 only because "ControlStore's four families" is one of the inventory columns
  and the two spellings name different fours.
- **The metrics fold's recomputability is a property, not a boot path.**
  `metrics/mod.rs:14-19` says a process "that wants to rebuild — after a restart,
  or to check the live numbers — replays the log through the identical fold".
  Searched for a caller: `MetricsRecorder::record` is invoked from exactly one
  production site, `SessionObserver::observe` (`metrics/mod.rs:247-251`), wired
  at `engine.rs:1081` inside `run_turn`. **Negative claim: nothing at boot, and
  nothing outside a turn, replays sessions into the fold.** So after a restart
  the dashboard shows only sessions that have since been re-opened by a turn —
  which is what `engine.rs:1071-1074` actually promises, narrower than what
  `metrics/mod.rs:14-19` reads as.

---

## 10. Open questions, left for the ruling

1. **(a) is fixable two ways at different prices** (§6, handoff (a)): durable
   generations, or an `admit` check on the fork arm. The second is smaller and
   correct on its own terms; the first is the only one that also closes (b) and
   the named-path half of (c). Whether to take the cheap fix *now* and the
   durable map *later* is a sequencing decision, not an evidence one.
2. **Where the three durable maps live.** The same two-placement question the
   directory store already wrote down (`directory.rs:81-110`) — into
   `roundhouse-core` beside the session and spend contracts, or into the server
   crate over the handle `main.rs` already opens. `control/mod.rs`'s standing
   note that a key record "will arrive next to the resolver, not here" is cited
   there as the thing a move would contradict.
3. **Whether a durable `generations` read belongs on the turn admission path.**
   Today it is a `Mutex<HashMap>` lookup (`conversations.rs:365-377`) on every
   request of every turn; a store read there is a per-turn round trip on the
   hottest path in the system.
4. **Whether `latest` should ever be durable.** It is the only row whose whole
   contract is "a guess, and `resolve_session` weighs it as one"
   (`mcp/src/reads.rs:90-94`). Making it durable makes a guess more confident
   without making it more correct.
5. **Whether P0 is coherent at all given §5.3.** Four promises are not degraded
   but deleted under P0 (replay/audit, exact settle, drift reconciliation, steer
   as a log item), and one of them — the steer — would be a *reversion* of M10.0
   rather than a non-extension. Whether P0 is a shippable mode or only a
   description of what roundhouse would be if it stopped being roundhouse is a
   ruling, not an observation.
6. **The idle-sweep discipline.** Relay bounds by liveness (30s idle,
   `nemo-relay-cli-0.8.2/src/sessions/idle.rs:18-19`); roundhouse bounds by count
   (`conversations.rs:183`, `:305`) and by age (`mcp/src/store.rs:97`), and
   bounds `generations` not at all. Whether to adopt a third discipline or fix
   the unbounded map with one of the two that exist is open.

---

## Fact-check (2026-09-02)

An independent re-derivation of every negative and every high-stakes claim above, from the primary sources at the pinned revisions, by a second reader who did not write this document. Verdicts: 20 verified, 2 corrected, 0 unestablished.

Re-derived all 5 negatives and all 17 claims (12 high, 4 medium, 1 low) independently from source at roundhouse 7c5369a and the 0.8.2 registry sources. All 5 negatives verified exactly, including exact grep-hit counts matching the draft's stated search. All 17 claims verified, with two minor corrections: (1) negative-3's cited file (atif.rs, the six redis/durable/persist hits) is in the core crate nemo-relay-0.8.2, not nemo-relay-cli-0.8.2 as the citation prefix implies -- facts are otherwise exactly right once read from the correct crate; (2) medium claim 12's SessionManager has only two of its three Arc<Mutex<...>> fields literally typed as Arc<Mutex<HashMap<..>>> -- the third (alignment) is Arc<Mutex<SessionAlignmentState>>, a named struct that itself wraps two HashMaps; the "three map-shaped node-local stores plus a config" characterization still holds. No claim was found false or unestablished. Evidence file: /tmp/claude-0/-home-user-roundhouse/d6addde3-2039-5f5e-8af5-d560d8c0b623/scratchpad/d1/roundhouse-state-inventory-factcheck.md

Corrections, each also applied above as a dated bracketed note:

- **Negative: nemo-relay 0.8.2 has no store/log/Redis/persistence; ATIF exporter is in-memory** — True, but atif.rs and the six redis|durable|persist hits (all in plugin/dynamic*) are in crate nemo-relay-0.8.2 (core), not nemo-relay-cli-0.8.2 as the citation path implies. AtifExporterState is Arc<Mutex<..>> with per-Uuid HashMaps; src/ has no store/session/persistence module.
- **Relay 0.8.2 SessionManager is three Arc<Mutex<HashMap<..>>> plus config; 30s idle timeout/5s sweep; weighted hint scoring (8/4/4/4/1)** — sessions/mod.rs:63-71: only 2 of 3 Arc<Mutex<..>> fields are literally HashMap-typed; the third (alignment) is Arc<Mutex<SessionAlignmentState>>, a named struct wrapping two HashMaps (alignment.rs:287-289). Idle timeout/sweep (idle.rs:18-19) and hint weights (correlation.rs:8-30) confirmed exactly as stated.
