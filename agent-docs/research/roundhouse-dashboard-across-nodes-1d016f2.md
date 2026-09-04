<!-- SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# DIVE D3-1 — The dashboard across nodes: what the fold holds, how it is fed, what a deployment-wide answer would cost, and the three shapes it could take

**Read pinned to roundhouse `1d016f2`** ("M17 thermo-nuclear review: seven
findings, all valid; the engine's join is dialect-aware", 2026-09-04). Plan
documents cited at the same revision. Dated 2026-09-04. Every claim carries a
`file:line`; every negative names what was searched.

---

## 0. What this document is for

D1 left one question open by name: "Whether the metrics fold gets an
aggregator across nodes, which is the dashboard's P2 question and not a
correlation one" (`agent-docs/PLAN-frontier-selection.md:491-492`). The state
inventory established that the fold is process-local and that nothing replays
sessions into it at boot
(`agent-docs/research/roundhouse-state-inventory-7c5369a.md:521-545`). This
dive re-verifies that at `1d016f2`, establishes what the fold actually holds
and how it is fed, what the dashboard renders and which of its numbers are
node-local, what the Redis session store can and cannot enumerate, and prices
the three shapes a cross-node aggregate could take.

The headline is in §7: **the middle shape — a durable fold state per node,
merged at read — is unsound as ordinarily stated**, because the fold's
idempotency lives in a per-fold watermark map and `Counters::absorb` has no
idempotency at all, while failover guarantees two nodes fold overlapping
prefixes of the same log.

---

## 1. What the fold holds

`MetricsFold` (`crates/roundhouse-core/src/metrics/fold.rs:412-489`) is
**one accumulator**, keyed per principal, plus five pieces of bookkeeping:

| Field | Type | Holds | `fold.rs` |
|---|---|---|---|
| `by_principal` | `BTreeMap<PrincipalKey, BTreeMap<ModelKey, Counters>>` | the only token accumulator | `:427` |
| `principal_of_session` | `HashMap<SessionId, PrincipalKey>` | attribution, learned from `SessionCreated` | `:439` |
| `watermarks` | `HashMap<SessionId, u64>` | highest seq folded per session — **the idempotency guarantee** | `:449` |
| `pending` | `HashMap<ResponseId, Pending>` | dispatches awaiting a terminal event | `:450` |
| `response_of_turn` / `turn_of_response` | `HashMap` | supersession of abandoned dispatches | `:465-466` |
| `turns_of_principal` | `BTreeMap<PrincipalKey, u64>` | turns admitted, per principal | `:472` |
| `window_of_principal` | `BTreeMap<PrincipalKey, (u64, u64)>` | first/last event time per principal | `:479` |
| `validations` | `BTreeMap<PrincipalKey, BTreeMap<Arm, ValidationTally>>` | the control accumulator | `:488` |

`Counters` (`fold.rs:94-190`) is money-free by design — "prices are
configuration and they change, so folding dollars in here would freeze
whatever rate card happened to be loaded" (`fold.rs:88-92`) — and carries
`calls`, `estimated_calls`, two `Counted` pots (`billed` and `seat`,
`fold.rs:102-120`), `quoted_alternative_usd`, `side_calls`,
`abandoned_side_calls`, `failed_attempts`, `provider_reported_usd`,
`provider_reported_calls`, and `declared_baseline`.

**There is no deployment-wide copy.** `fold.rs:415-423` states that as the
design: "A second accumulator would have to be written by the same code path
on pain of drift … Deployment answers are summed out of these rows on the way
out instead (see `Self::view`), so drift is not unlikely, it is
unrepresentable." The summing is `MetricsFold::summed_rows`
(`fold.rs:936-947`) over `Counters::absorb` (`fold.rs:280-292`), and
`MetricsFold::view` (`fold.rs:855-926`) resolves the three scopes —
`Deployment`, `Principal`, `Project` (`fold.rs:361-381`).

### 1.1 Idempotency is per-fold, keyed by `(session, seq)`

`MetricsFold::apply` (`fold.rs:544-549`) takes the session's watermark first
and returns `false` for any `seq <= watermark`. `fold.rs:406-411` states the
purpose: "That is what lets a live feed and a rebuild-from-log coexist without
double counting." The watermark map "gains an entry per session and never
loses one, which makes it the fastest-growing state in this struct … It cannot
simply be pruned: it *is* the idempotency guarantee" (`fold.rs:440-449`).

**This is the load-bearing fact for §7.** The idempotency is a property of
*one* `MetricsFold`, held in a map the `Counters` do not carry. `Counters` are
sums with the event identity discarded, and `Counters::absorb` is unguarded
addition (`fold.rs:280-292`).

---

## 2. How the fold is fed — one production path, and only one

`MetricsRecorder` (`metrics/mod.rs:150-158`) is "Process-wide metrics,
maintained as sessions run", an `Arc<RwLock<MetricsFold>>`.
`MetricsRecorder::record` (`metrics/mod.rs:168-175`) calls `fold.extend`.

The only production caller is the `SessionObserver` impl
(`metrics/mod.rs:247-251`). Searched: `git grep -n 'metrics\.record\|recorder\.record\|\.observe('`
over `crates` at `1d016f2` returns, outside the fold's own
`DeclaredBaseline::observe` (`fold.rs:232`, `:645`), exactly three sites —
`session.rs:1063`, `session.rs:1261`, and `metrics_api.rs:227` (a test
fixture, inside `#[cfg(test)] mod tests` beginning at `metrics_api.rs:195`). [fact-check 2026-09-04: a second test fixture, `tests/tenancy_attribution.rs:778`, also calls `record` on a fresh recorder; the production path is exactly as stated]

The two live sites are both inside `Session`:

- **Replay on open** — `SessionState::project` (`session.rs:1041-1068`) reads
  the log in `REPLAY_BATCH = 1024` (`session.rs:28`) chunks from `cursor = 0`
  (`session.rs:1053`) and hands each batch to the observer
  (`session.rs:1062-1064`).
- **Every commit** — `session.rs:1253-1262`, after the projection.

The observer is attached at exactly one production site:
`Engine::run_turn` (`crates/roundhouse-server/src/engine.rs:1084-1092`), which
passes `Arc::clone(&self.metrics) as Arc<dyn SessionObserver>`
(`engine.rs:1090`). Searched `open_observed` over `crates`: the only non-test
occurrences are the definition (`session.rs:1122`), the `None` delegation from
`Session::open` (`session.rs:1110`), and `engine.rs:1084`. The MCP control
surface projects with `None` (`mcp_api.rs:338`) precisely so a reader does not
take the lease (`session.rs:1033-1040`).

**Negative (re-verified at `1d016f2`): nothing at boot, and nothing outside a
turn, replays sessions into the fold.** Searched `replay|rebuild|warm` over
`crates/roundhouse-server/src/main.rs` and `engine.rs`: `main.rs` has one hit
and it is `tracing::callsite::rebuild_interest_cache()` (`main.rs:1628`).
`main.rs` wires `engine.metrics()` into two routers (`main.rs:1046-1056`) and
nothing else. This confirms the inventory's §9.2 negative
(`roundhouse-state-inventory-7c5369a.md:534-545`) at the new revision; the line
numbers it quotes have moved — the observer wiring is now `engine.rs:1090` and
the comment it calls `engine.rs:1071-1074` is now `engine.rs:1080-1083`.

### 2.1 The two promises, side by side

- **`metrics/mod.rs:14-19`** (the module's own claim): "A live process feeds it
  each event as it is appended and answers `/v1/metrics` from memory; a process
  that wants to rebuild — after a restart, or to check the live numbers —
  replays the log through the identical fold and must get the identical answer.
  That equivalence is what `MetricsFold` is tested on."
- **`engine.rs:1080-1083`** (what the code actually does): "Observed rather
  than plain: the session feeds the metrics fold both its replay and its
  subsequent commits, so a node that restarts and picks a session back up
  recovers that session's accounting instead of reporting only what it served
  since booting."

The second is strictly narrower and is the true one. [fact-check 2026-09-04: the module doc's per-session rebuild *is* exercised — `Session::open_observed` replays each session's log through the same fold on every open — so the two are not in contradiction; what nothing exercises is a deployment-wide rebuild at boot, which §10's first negative establishes on its own] The equivalence in the
first *is* tested — `a_rebuild_from_the_log_matches_a_live_feed`
(`metrics/mod.rs:317-348` in the test module; assertions at `:343-348`),
`a_replayed_log_folds_exactly_once` (`metrics/mod.rs:290-316`), and end-to-end
in `replaying_a_log_recovers_the_principal`
(`crates/roundhouse-server/tests/tenancy_attribution.rs:753-802`, byte-compare
at `:781-787`). What is not tested, because nothing does it, is a *boot-time*
or *on-demand* rebuild. The test at `tenancy_attribution.rs:778` reaches the
log through a helper handed a literal session id (`"acme/ada/work"`), which is
exactly the capability §5 shows the store does not have.

---

## 3. What the dashboard renders, and which numbers are node-local

`GET /v1/metrics` returns one `MetricsSnapshot`
(`crates/roundhouse-server/src/metrics_api.rs:75`, `:84-121`);
`GET /v1/metrics/dashboard` returns `dashboard.html` inlined by `include_str!`
(`metrics_api.rs:38`, `:186-193`). The page is a static asset that fetches
`/v1/metrics` from the browser (`dashboard.html:802-815`), polling on a timer
(`:825-827`).

The document's own module doc states the source: "The numbers come from
`MetricsRecorder`, which every session has been feeding as it commits, so
answering a request here is a fold already done rather than a sweep over the
log" (`metrics_api.rs:6-11`).

**Every number on that page is this process's.** `MetricsSnapshot`
(`metrics/snapshot.rs:414-465`) carries `generated_at_ms`,
`first_event_at_ms`, `last_event_at_ms`, `sessions`, `turns`, `calls`,
`tokens`, `seat_tokens`, `savings`, `coverage`, two coverage fractions,
`models`, `providers`, `serving_modes`, `capability_band` and
`quality_prior_citation`. Scoping is by principal or project
(`metrics_api.rs:138-170`) — never by node.

**Negative: no node identity anywhere in the metrics surfaces.** Searched
`node_id|node` over `crates/roundhouse-core/src/metrics`,
`crates/roundhouse-server/src/metrics_api.rs` and
`crates/roundhouse-server/src/dashboard.html`: **zero hits**. The engine, by
contrast, says it plainly: "Folds every event this node commits into the
dashboard's aggregates" (`engine.rs:770`) and "The running token and dollar
aggregates for everything this node served" (`engine.rs:944`).

### 3.1 The savings columns and their basis

`Savings` (`snapshot.rs:346-411`) is "decomposed by how much each part can be
trusted", and `metrics/mod.rs:29-46` states the rule:

| Column | Basis | Where |
|---|---|---|
| `frontier_spend_usd` (+ `_measured` / `_estimated` split) | measured tokens × published rate card | `snapshot.rs:361-365` |
| `cache_savings_usd` | measured — a discount a provider actually applied | `snapshot.rs:366-373` |
| `routing_savings_usd` | **counterfactual** — priced against the correlary chosen in `pricing` | `snapshot.rs:374-381` |
| `routing_savings_at_decision_usd` | independent cross-check from the router's own quotes; **not added in** | `snapshot.rs:382-392` |
| `total_usd` | `cache_savings_usd + routing_savings_usd` only | `snapshot.rs:393-394` |
| `provider_reported_usd` | the upstream's own arithmetic, `Option<f64>`; **added into nothing** | `snapshot.rs:395-410` |
| `seat_tokens` | tokens with no dollar beside them — a forwarded seat has no rate card | `snapshot.rs:429-439`, `metrics/mod.rs:48-54` |

The page renders these as the hero split — "Provider cache discount
`measured`", "Served locally instead `estimated`", "Router's own quote
`cross-check`" (`dashboard.html:269-289`, values bound at `:720-723`) — plus
four KPIs (`dashboard.html:300-320`, bound at `:733-759`) and three tables:
models (`:333-356`), providers (`:364-375`), and "Local models and their
correlaries" with a `Basis` column (`:384-398`, rendered at `:607-633`).

The `Basis` chips are `PricedBasis::{Declared, Inferred}`
(`metrics/pricing.rs:181-189`) or `Correlary::Unpriced`
(`pricing.rs:214-215`), gated by `DEFAULT_CAPABILITY_BAND = 0.10`
(`pricing.rs:379`) and echoed on the wire as `capability_band`
(`snapshot.rs:453-455`).

### 3.2 The window label is a per-process claim rendered as a deployment one

`dashboard.html:767-770` sets the header to `since <first_event_at_ms>`.
`first_event_at_ms` for `Scope::Deployment` is
`self.window_of_principal.values().map(|(f, _)| *f).min()`
(`fold.rs:865`) — the earliest event **this process folded**, which after a
restart is the first event of whichever session a turn happened to re-open.
The page prints it with no qualification. This is the one place the node-local
fold is presented to a human as a deployment-wide fact without a basis label.

### 3.3 The dashboard cannot be read at all in `Configured` mode today

`metrics_api.rs:180-185`: "in `Configured` mode a browser navigating here sends
no key, so the fetch is refused and the page renders its own error … That is
honest but not usable; giving the page somewhere to put a key is a later
milestone's work." Worth recording beside the P2 question because any
cross-node work that lands before it inherits an unusable page.

---

## 4. The drift column, and where the shared ledger already answers part of this

`GET /v1/admin/projects/{p}/budget`
(`crates/roundhouse-server/src/admin_api/reconciliation.rs:398`, mounted at
`admin_api.rs:128`) is the one place a **durable, shared** figure and the
**process-local** fold are put side by side, each under a stamp.

- `committed_usd` — from `SpendLedger::balance`, over a real budget window,
  stamp `ledger` (`reconciliation.rs:78-81`, `:262`). Its Redis implementation
  is a shared key family (`roundhouse-store-redis/src/lib.rs:24`), so this is
  **deployment-wide and durable**.
- `measured_usd` — `metrics.snapshot_for_project(...).savings.frontier_spend_usd`
  (`reconciliation.rs:416`, `:563`; per member at `:444-449`), stamp
  `process-fold`, window `lifetime`.
- `drift_usd` = `committed - measured` (`reconciliation.rs:552`, `:582`).
- `provider_reported_usd` — a third answer, deliberately outside the difference
  (`reconciliation.rs:272-292`).

The stamp for the measured column says the node-locality out loud
(`reconciliation.rs:105-119`): "**Lifetime, and only ever lifetime.** The
fold's watermarks cannot be pruned without event-time windowing, so it has no
way to answer 'this month' … It is also *process*-lifetime: the fold is in
memory and starts empty after a restart, which is why the basis names the
process and not the log."

**Where the two disagree by design** is enumerated at
`reconciliation.rs:221-247`: negative drift has three causes — a failed settle,
a process restarted between dispatch and settle, and, ordinarily, "nothing
wrong at all" because the engine writes the terminal usage event before it
settles the ledger, with `held_usd` as the discriminator. The M8 addendum
states the same thing in the plan
(`agent-docs/PLAN-agentic-control-plane.md:1355-1357`): the fold is
"process-local, so a restart legitimately sends drift positive until the log is
re-folded."

**What the ledger already answers, and what it does not.** The spend ledger is
shared and durable, and it answers *dollars committed against a ceiling, per
membership, per budget window*. `SpendLedger` has three operations —
`open_grant`, `settle_grant`, `balance` (`roundhouse-core/src/control/spend.rs:311-333`).
It carries **no tokens, no per-model rows, no serving-mode split, no cache
figures, no counterfactual, and no enumeration** — `balance` takes a
`BalanceQuery` for one membership (`spend.rs:255`), and the admin view's own
cost note records that this is N mutating round-trips per project
(`reconciliation.rs:379-397`). So the ledger already answers the *deployment-wide
frontier spend per project*; every other column on the dashboard — tokens,
cache hit rate, coverage, savings, per-model and per-provider rows,
`failed_attempts`, `seat_tokens` — has no shared counterpart at all.

---

## 5. What the Redis session store can and cannot enumerate

**Negative: there is no session listing, no scan, and no index of sessions per
project. Every read is by id.**

- `SessionStore` (`roundhouse-core/src/store.rs:71-157`) has seven methods:
  `create_session`, `acquire_lease`, `renew_lease`, `release_lease`,
  `is_leased`, `append_events(lease, kinds)`, `read_events(session_id,
  after_seq, limit)`, `last_seq(session_id)`. Every read names a
  `&SessionId`. There is no `list`, no `iter`, no cursor.
- The Redis layout is three keys per session, all hash-tagged on the session id
  (`roundhouse-store-redis/src/lib.rs:10-14`): `…:sess:{<id>}:meta` (string),
  `…:sess:{<id>}:lease` (hash), `…:sess:{<id>}:log` (stream). Key builders at
  `lib.rs:252`, `:260`, `:268`, all through `keys::build_key`
  (`keys.rs:229-241`).
- Searched `SCAN|scan_match|"KEYS"` over `crates/roundhouse-store-redis` and
  `crates/roundhouse-core`: **no hits** in either crate (the only `keys(`
  matches are an unrelated `ProviderKeys` helper in
  `control/credential/access.rs`). There is no `SMEMBERS`-style index either —
  the five key families are enumerated at `lib.rs:21-27` and none of them is a
  session index.
- The correlation family does not help: its three maps are keyed
  `corr:gen:{<cache key>}`, `corr:call:{<principal>}:<tool_use_id>`,
  `corr:thread:{<principal>}:<thread_id>` (`correlation.rs:15-19`), each
  mapping **to** a session, with a TTL, and "a principal's bindings cannot be
  read in one command — and nothing reads them that way"
  (`correlation.rs:33-35`). They also expire (`CALL_BINDING_TTL_MS`,
  `THREAD_BINDING_TTL_MS`, `correlation.rs:106`, `:110`), so they are not a
  durable census of anything.
- The directory document holds tenancy — projects, users, memberships, keys —
  not sessions (`roundhouse-core/src/control/directory.rs:9-13`).

**Consequence.** Shape 1 below ("replay every session at query time") does not
merely cost a sweep; it has no starting point. A deployment cannot answer
"what sessions exist" without adding a sixth key family or a `SCAN` over
`<ns>:v1:sess:*`, which the store crate has deliberately never done.

---

## 6. What is *not* measured on any node: the "time" leg

The product sentence claims co-optimisation of function, cost and **time**.
The router weighs time: `Candidate::expected_ttft_ms`
(`roundhouse-core/src/routing/mod.rs:139`) enters the score with weight
`ttft: 0.25` (`routing/policy.rs:80`, `:90`, normalized at `:165`, applied at
`:172`).

**Negative: no observed latency reaches the fold, the snapshot or the page.**
Searched `ttft|latency|time_to_first|ms_to_first` (case-insensitive) over
`crates/roundhouse-core/src/metrics` and
`crates/roundhouse-server/src/dashboard.html`: exactly one hit, and it is a
test fixture constructing a `Candidate` (`fold.rs:1094`, inside
`pub(super) mod tests` which begins at `fold.rs:1062`).

The engine records that the quantity **is** derivable from the log:
"what makes TTFT a measured quantity — the first `OutputTextDelta.at_ms` in
the log minus the `Routed.at_ms` before it — instead of the model's own
estimate of itself. On a turn that fell forward, 'the `Routed` before it' is
the *last* one" (`engine.rs:1490-1499`). But `MetricsFold::apply` ignores
`OutputTextDelta` in its catch-all arm (`fold.rs:816-820`), and
`MetricsSnapshot` has no latency field (`snapshot.rs:414-465`).

So the co-optimisation claim's third leg is unmeasured on the dashboard
*before* the cross-node question is reached. Cost is measured per node;
quality enters only as the configured `quality_prior` behind the capability
gate (`pricing.rs:379`, `snapshot.rs:453-455`); time is not measured at all.

---

## 7. The three shapes a cross-node aggregate could take

### Shape 1 — replay every session at query time

**What it needs.** A session census (§5: does not exist), then
`read_events` per session in 1024-event batches (`session.rs:28`, `:1055`)
into a fresh `MetricsFold`, then `MetricsSnapshot::build`.

**What it gets right.** Perfect: the fold is idempotent by `(session, seq)`
(`fold.rs:544-549`) and the equivalence is already tested
(`metrics/mod.rs:317-348`; `tenancy_attribution.rs:753-802`). Correlary
inference over the whole deployment's traffic is the *right* inference for a
deployment-wide document (see §7.4).

**Cost.** O(total events in the deployment's history) per poll, against the
same Redis carrying the write path. The metrics surface's own contract forbids
this: "A dashboard polling every few seconds must not cost the store anything,
or watching the fleet becomes a load on the fleet" (`metrics_api.rs:10-11`).
It also requires a new enumeration capability on `SessionStore`, which means a
new contract-suite obligation for both backends
(`roundhouse-core/src/store/contract.rs` is the suite; the pattern is stated at
`control/spend.rs:307-309`).

**Verdict.** Correct and unaffordable as a poll. Defensible only as an
explicit, rate-limited "recompute" action — which is what the module doc's "or
to check the live numbers" (`metrics/mod.rs:16-17`) actually describes, and
which nothing implements.

### Shape 2 — a durable fold state per node, merged at read

**This is the shape that does not work as usually stated, and the reason is
structural.**

1. **The merge that exists is `Counters::absorb` (`fold.rs:280-292`), and it is
   unguarded addition.** It is documented as "The one definition of what
   merging two rows means, and the reason a deployment-wide row can be
   *derived* from its tenants'" — derived from *disjoint* per-principal rows of
   **one** fold, which is exactly why it needs no idempotency.
2. **Two nodes' folds are not disjoint; they overlap by construction.** On
   failover, `run_turn` opens the session with `open_observed`
   (`engine.rs:1084`) which projects from `cursor = 0` (`session.rs:1053`) and
   feeds **every** batch to the observer (`session.rs:1062-1064`). So node B's
   fold contains every event node A already folded for that session. The
   comment at `engine.rs:1080-1083` names this as the feature it is.
3. **Therefore summing two nodes' `Counters` double-counts every shared event**,
   and the identity needed to deduplicate — `(session, seq)` — was discarded
   when the events became sums.

A sound version of shape 2 must ship the **watermarks** with the counters and
merge by `max(watermark)` — but that does not repair the counters, because a
row that folded seq 1..100 and a row that folded seq 1..80 cannot have the
overlap subtracted from a sum. The only sound per-node durable state is one
whose rows are keyed by something a session belongs to exactly once, which
means partitioning by session and not by node — at which point it is no longer
"per node".

**Second cost, if it were repaired.** The state is not small and does not
shrink. `watermarks` "gains an entry per session and never loses one … It
cannot simply be pruned: it *is* the idempotency guarantee, and forgetting a
session means re-folding its log on the next replay. Bounding it means
windowing on event time" (`fold.rs:440-449`). `principal_of_session` grows with
it (`fold.rs:438`), and abandoned-dispatch residue never drains for a turn
abandoned and never retried (`fold.rs:462-464`).

**Third cost.** The node id is a fresh UUID per process:
`node_id: format!("node_{}", uuid::Uuid::new_v4().simple())`
(`engine.rs:491`), and `main.rs` takes it from the default
(`main.rs:946-961`, `..EngineConfig::default()` at `:960`). **Negative:
searched `ROUNDHOUSE_NODE|NODE_ID` over `crates` — no hits**; there is no way
to configure a stable node identity. A per-node durable key would therefore
mint a new key on every restart and orphan the old one, with no sweeper. The
lease's own requirement is the opposite one — "It must be unique for every live
engine" (`engine.rs:451-454`) — so making it stable is a change with its own
argument to win.

### Shape 3 — a shared running aggregate, written per turn

The fair-use ledger is the worked precedent: per-window running sums
maintained on write, decayed lazily on read, with the pruning owned by
`record_draw` (`roundhouse-store-redis/src/fair_use.rs:57-72`), keys
`…:fairuse:{<project>}:p` and `…:fairuse:{<project>}:m:<user>`
(`fair_use.rs:11-14`). Its measured steady state is "one `HMGET` per (scope,
window) checked … amortised O(1)" (`fair_use.rs:69-72`), with a bounded worst
case chunked at 400 fields per `HMGET` (`fair_use.rs:74-83`).

**What it would cost on the hot path.** One additional shared-store write per
settled call, at the `settle` seam (`fold.rs:1037-1059`) — which is where the
spend ledger's `settle_grant` already goes, so the round trip is not a new
class of dependency. The dashboard read then becomes a read of counters
rather than a fold.

**What it gives up — and this is the whole of `fold.rs:17-25` and
`fold.rs:415-423`.** It is *precisely* the second accumulator the fold's design
refuses: "two accumulators fed at two sites drift the first time one path
returns early, silently and permanently, and the drift shows up as a project's
bill disagreeing with the deployment's. One accumulator cannot disagree with
itself." And `metrics/mod.rs:6-12`: "A counter incremented alongside the log
would drift the first time a turn failed between the two writes, and the drift
would be silent and permanent."

The mitigation the codebase already uses for exactly this hazard is the drift
column (§4): publish both, stamp each with its basis, and never reconcile them
into one number (`reconciliation.rs:50-58`, `:211-219`). A shared running
aggregate is defensible **only** if it is published as a third stamped basis
beside the fold, never as a replacement for it — the same discipline
`provider_reported_usd` already rides under (`snapshot.rs:395-410`).

**What it cannot carry.** Not everything on the dashboard is a sum.
`DeclaredBaseline` is a three-state collapse whose merge is order-independent
but not additive (`fold.rs:207-244`, `absorb` at `:229-235`) — expressible in a
Lua script but not in an `HINCRBY`. `window_of_principal` is a min/max pair
(`fold.rs:584-585`). `sessions` is a **cardinality**, counted off the watermark
map (`fold.rs:863`), and a shared counter cannot count distinct sessions
without holding the set.

### 7.4 The pricing walk must happen *after* the merge, not before

Whatever shape is chosen, merging finished `MetricsSnapshot`s is wrong, and
the reason is `ScopeView::frontier_shapes` (`fold.rs:526-535`): the correlary
for a local model is inferred **from the scope's own hosted traffic** — "Off
this view's own rows, so a scoped report infers its counterfactual from its own
traffic. Reading the deployment's shapes into one tenant's report would price
that tenant's local turns against a similarity argument built out of somebody
else's prompts — a number that moves when a neighbour's workload changes"
(`fold.rs:518-525`). Two nodes serving different traffic mixes can therefore
pick **different reference models** for the same local model, so their
`routing_savings_usd` are not commensurable and their sum is not a saving.
`MetricsSnapshot::build` is the scope seam (`metrics/mod.rs:185-189`); the
merge has to be below it, at `Counters`.

---

## 8. What the product sentence needs to be true of a deployment

Read against `CLAUDE.md`'s one-sentence product statement, the numbers that
must hold **across a deployment rather than a node** are:

| Number | Today | Where |
|---|---|---|
| frontier spend, per project, against a ceiling | **deployment-wide already** — the shared spend ledger | `store-redis/src/lib.rs:24`; `spend.rs:311-333` |
| `savings.frontier_spend_usd` on the dashboard | node-local | `engine.rs:944`; `reconciliation.rs:105-119` |
| `savings.cache_savings_usd` (measured discount) | node-local | `snapshot.rs:366-373` |
| `savings.routing_savings_usd` (the counterfactual the whole claim rests on) | node-local **and scope-dependent** | `snapshot.rs:374-381`; `fold.rs:518-535` |
| tokens, coverage, per-model and per-provider rows | node-local | `snapshot.rs:414-465` |
| `seat_tokens` | node-local | `snapshot.rs:429-439` |
| `failed_attempts` per model ("which target is failing") | node-local — and this one is *most* misleading per node, since a tier failing on one node and not another is the diagnosis | `fold.rs:987-1006` |
| observed TTFT | **not measured anywhere** (§6) | `fold.rs:816-820`; `engine.rs:1490-1499` |
| validation arm comparison (`ValidationTally`) | node-local, and deliberately off the snapshot | `metrics/mod.rs:220-234`; `fold.rs:480-488` |

The asymmetry worth putting in front of the product owner: **cost is already
deployment-wide where it binds** (the ledger refuses a turn correctly across
nodes), and it is only the *reporting* of cost that is per node. Quality and
time are not measured across a deployment because they are not measured at all
— quality is configuration (`FrontierModelSpec`'s "configuration, not
measurement", cited in `CLAUDE.md`), and time is derivable but underived.

---

## 9. What the plans have already deferred, restated with its pin

- **The open question itself**: "Whether the metrics fold gets an aggregator
  across nodes, which is the dashboard's P2 question and not a correlation one"
  — `PLAN-frontier-selection.md:491-492`.
- **R19's per-node status surface**: a node whose fingerprint differs "warns
  with a typed reason naming what differs, once per stored version" and records
  "the version it could not take beside the version it serves; that pair is the
  first row of a per-node status surface roundhouse does not yet have, deferred
  by name" — `PLAN-frontier-selection.md:571-587`, restated as an open question
  at `:596-597`.
- **Negative: no per-node status or health surface exists.** Searched
  `"/health|/v1/health|healthz|/status` over `crates/roundhouse-server/src`:
  no hits. The full route list at `1d016f2` (searched `.route(` over
  `crates/roundhouse-server/src`) is admin (`admin_api.rs:118-141`), native
  sessions (`http.rs:141-146`), MCP (`mcp_api.rs:410`), Messages
  (`messages_api.rs:250-254`), metrics (`metrics_api.rs:75-76`), Relay reads
  (`relay_api.rs:90-92`), Responses (`responses_api.rs:215`). Nothing reports a
  node.
- **The M8 "still deferred, by name" list** — `PLAN-agentic-control-plane.md:1367-1371`
  — and the M16 addendum's restatement that MCP-overlay durability and the
  sealed credential store "gain a contract they can ride on and keep their own
  questions" (`:1642-1646`).

The contract they would ride is `DocumentStore`
(`roundhouse-core/src/control/directory.rs:163-191`): `load` / `commit(expected_version, bytes)`
/ `version`, opaque bytes, identity `(lineage, version)`
(`directory.rs:28-58`), with an 8 MiB refusal enforced at the server boundary
before any wire call (`control_config/directory/document.rs:91-112`). For
completeness of the D3 comparison: **a metrics fold state is the wrong shape
for that contract** — the directory ceiling is sized for "a few thousand keys"
at "roughly 330 bytes per key record" (`document.rs:91-100`), while the fold's
`watermarks` and `principal_of_session` grow one entry per session forever
(`fold.rs:440-449`), and every commit is a whole-document compare-and-set.

---

## 10. Negatives, collected

1. **Nothing replays sessions into the metrics fold at boot or outside a turn.**
   Searched: `open_observed` and `SessionState::project` over `crates` (only
   production observer site is `engine.rs:1084`); `replay|rebuild|warm` over
   `main.rs` and `engine.rs`; `metrics\.record|recorder\.record|\.observe(` over
   `crates`. Re-verifies `roundhouse-state-inventory-7c5369a.md:534-545` at
   `1d016f2`.
2. **No node identity appears in the metrics fold, the metrics API or the
   dashboard.** Searched `node_id|node` over
   `crates/roundhouse-core/src/metrics`, `metrics_api.rs`, `dashboard.html` —
   zero hits.
3. **No way to configure a stable node id.** Searched
   `ROUNDHOUSE_NODE|NODE_ID` over `crates` — no hits; `main.rs:960` takes
   `EngineConfig::default()`, which mints a UUID (`engine.rs:491`).
4. **No session enumeration, scan, or per-project session index.**
   `SessionStore` (`store.rs:71-157`) is by-id only; searched
   `SCAN|scan_match|"KEYS"` over `crates/roundhouse-store-redis` and
   `crates/roundhouse-core` — no hits; the five key families
   (`store-redis/src/lib.rs:21-27`, `keys.rs:53-65`) include no index family.
5. **No observed latency or TTFT is folded or published.** Searched
   `ttft|latency|time_to_first|ms_to_first` over
   `crates/roundhouse-core/src/metrics` and `dashboard.html` — one hit, a test
   fixture (`fold.rs:1094`). `OutputTextDelta` is ignored by the fold
   (`fold.rs:816-820`).
6. **No per-node status or health endpoint.** Searched
   `"/health|/v1/health|healthz|/status` over `crates/roundhouse-server/src` —
   no hits; the route inventory in §9 is complete.
7. **No cross-node metrics aggregation of any kind exists.** Searched
   `aggregat` over `crates/roundhouse-core/src` and
   `crates/roundhouse-server/src`: every hit is about summing rows *within* one
   fold (`fold.rs:406`, `snapshot.rs:277-325`, `metrics_api.rs:318`) or is the
   engine's own "this node" wording (`engine.rs:770`, `:944`).

---

## 11. Open questions this evidence does not settle

1. **Is the deployment-wide document the one that must be right, or the
   per-node one?** If the operator's question is "what is this fleet costing
   me", the ledger already answers the binding half (§4) and the honest cheap
   move may be to *label* the dashboard as node-local (fixing §3.2) rather than
   to aggregate. That is a product call.
2. **If shape 3 is taken, what is the aggregate's stamp?** The codebase's own
   discipline says a second accumulator is published beside the fold under its
   own basis and never merged into it (`reconciliation.rs:50-58`). Naming that
   basis — and deciding whether the dashboard hero reads from it or from the
   fold — is the design decision, not the Lua.
3. **Does making `node_id` configurable belong to this round or to R19's status
   surface?** Both need it; the lease needs it to stay unique per *live* engine
   (`engine.rs:451-454`), which a configured-and-restarted node still is.
4. **Is TTFT part of "the dashboard across nodes" or a rung of its own?** It is
   derivable from the log today (`engine.rs:1490-1499`) and would be folded
   from `OutputTextDelta`, which the fold currently ignores — a change to the
   fold's event vocabulary, independent of where the fold lives.

---

## Fact-check (2026-09-04)

An independent re-derivation of every negative and every high-stakes claim above, from the primary sources at the pinned revision (roundhouse `1d016f2`; Relay at the 0.8.2 registry sources), by a second reader who did not write this document. Verdicts: 23 verified, 2 corrected, 0 unestablished.

Fact-checked all 7 negatives and 18 high/medium-stakes claims for D3 dive dashboard-across-nodes against roundhouse@1d016f2. All 7 negatives hold (no node identity, no enumeration, no TTFT fold, no health route, no cross-node aggregation, no boot/out-of-turn replay) though N1 and N4's stated grep methodology each omits one immaterial hit (a doc comment, a test-only KEYS assertion). Of 18 claims: 16 verified exactly at their cited lines. Two are corrected: C4 undercounts MetricsRecorder::record's other call sites (there are two test fixtures, not one — tests/tenancy_attribution.rs:778 is missing from the citation). C5 overstates a doc/code contradiction — the module doc's rebuild-from-log description is in fact exercised by Session::open_observed/SessionState::project on every session-open, so "no code exercises it" is wrong; the engine's comment restates rather than corrects the doc, and the real narrower fact (no boot-time bulk rebuild) is what N1 already covers. Full evidence trail with every file:line re-derived independently is in the notes file.

Corrections, each also applied above as a dated bracketed note:

- **C4: MetricsRecorder::record has exactly one production caller; only other call site is a test fixture** — Production path confirmed (mod.rs:168/247-250, session.rs:1062-1064/:1261). But there are TWO other call sites, both test fixtures: metrics_api.rs:227 (cited) and roundhouse-server/tests/tenancy_attribution.rs:778 (uncited, rebuilt.record(...) on a fresh MetricsRecorder).
- **C5: module doc claims a rebuild-from-log path no code exercises; engine comment states the narrower truth** — metrics/mod.rs:14-19 describes exactly the mechanism Session::open_observed/SessionState::project executes on every session-open (verified under C3), so the doc's claim IS exercised, per-session; engine.rs:1080-1083 restates rather than corrects it. The real absent thing (deployment-wide bulk rebuild at boot) is what N1 independently establishes, not a doc/code contradiction.
