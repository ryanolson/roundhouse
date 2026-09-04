# Un-archive, admin identity, and a per-node status surface

*A read of roundhouse at `1d016f2` — `roundhouse-server`'s
`control_config` (the directory, its records, mutations and auth),
`admin_api.rs` and the five other routers, `main.rs` and
`shared_backend.rs`, `roundhouse-core`'s `control` and `metrics`, and the
M8/D1/D2/M16.1 addenda in `agent-docs/`. NeMo Relay is read at the 0.8.2
registry sources. Dated 2026-09-04.*

Evidence for D3's third question: **what does archiving actually do today,
why was un-archive deferred, what would an attributed admin write need, and
what could a per-node status surface carry that nothing reports now?** Every
claim carries a `file:line` at `1d016f2` unless it names another pin; the
negatives name what was searched.

---

## 1. What archiving does today

**Archive is one field on one row, set by one mutation arm.**
`DirectoryMutation::ArchiveProject { id }` is the ninth of nine arms
(`control_config/directory/mutation.rs:178-182`, arms at `:170-210`), bound
to `DELETE /v1/admin/projects/{project}`
(`admin_api.rs:122-127`, handler `:497-506`) and answering `204` with no
body (`admin_api.rs:492-496`). The mutate arm refuses a file-owned project,
refuses a second archive of an already-archived one, and then does exactly
one thing — `project.archived_at_ms = Some(now_ms)`
(`control_config/directory.rs:1610-1619`).

**It touches nothing else.** The arm writes no key, no membership, no
ledger row. Contrast `DeleteMembership`, three arms above it, which
explicitly cascades: it retains-out the membership and then walks
`records.keys` setting `revoked_at_ms` on every unrevoked key minted under
it (`directory.rs:1659-1678`). Archive has no such loop. So every key of an
archived project keeps `revoked_at_ms: None` and stays, on the record, a
*live* key.

**What an archived project's key is refused with.** The compile step
collects archived ids into a set, leaves archived projects out of the merged
config entirely rather than filtering them later
(`directory.rs:2000-2015`), and for each unrevoked turn key whose project is
in that set inserts `KeyRefusal::ProjectArchived` into the refusals map
(`directory.rs:2026-2037`). `KeyRefusal` has exactly two arms — `Revoked`
and `ProjectArchived` — and converts to `AuthError`
(`control_config/mod.rs:476-491`). `ControlPlane::authenticate` consults the
refusals map *after* the admin and turn tables and *before* `UnknownKey`
(`control_config/mod.rs:1144-1159`). The wire answer is
`403 project_archived` — 403 rather than 401 because "the key is intact and
would authenticate, and it is the *project* that has been closed"
(`control_config/auth.rs:97-110`, code and status at `:143` and `:156`).

**Three other refusals name the archived project.** `PatchProject` refuses
an archived project (`directory.rs:1557-1559`); `MintTurnKey` calls
`refuse_archived` before anything else (`directory.rs:1682`, helper at
`:1856-1863`); `UpsertMembership` goes through `refuse_absent_project`,
which returns `ProjectIsArchived` rather than `UnknownProject` for a row
that exists and is closed (`directory.rs:1839-1854`). The error text says
archiving "is final in this milestone"
(`mutation.rs:233-237`).

**The id stays retired, deliberately and across restarts.** `refuse_taken`
does *not* skip archived rows, with the reason in the comment: "Re-creating
a project under a closed project's id would join two tenants' spend
histories under one name" (`directory.rs:1768-1783`). Since M16.1 that
tombstone is durable — the directory is the fifth family `shared_backend::
open` chooses (`shared_backend.rs:178-184`, `:313-320`), and the test that
waited eight milestones for it,
`recreating_an_archived_project_after_a_restart_inherits_its_spend`, is live
(`tests/admin_api.rs:1291`, its full account of the hazard at `:1265-1289`),
with the real-Redis version in `tests/directory_backend_boot.rs:26`.

## 2. Why un-archive was deferred, in the code's own words

`ProjectRecord::archived_at_ms` carries the whole deferral
(`control_config/directory/records.rs:142-156`):

> **Archived, never deleted.** A project's spend history outlives the
> project — the ledger's rows are keyed by principal and do not vanish — so
> a deployment that dropped the row would answer `unknown_key` for a
> membership its own ledger still has numbers for […]
>
> There is no un-archive route in M8, so this is terminal and the id stays
> taken. That is deliberate rather than unfinished: un-archiving has to
> decide what happens to the keys that were refused while the project was
> closed, and that question has no obviously right answer to guess at here.

The plans restate it three times and never widen it:
`agent-docs/PLAN-agentic-control-plane.md:1312-1314` ("Archive is terminal
in v1; the un-archive that would reopen the question of what its keys resume
meaning is deferred with the audit trail"), `:1367-1371` (the "still
deferred, by name" list), and D2's own leave-open list at
`agent-docs/PLAN-frontier-selection.md:588-590` — "durable tombstones are
its precondition, not its answer, and the keys-refused-while-closed question
is as open as M8 left it". The D2 evidence document put the same question as
its sixth open one (`agent-docs/research/roundhouse-admin-directory-1b85d64
.md:315-319`).

**One remedy sentence in the tree already assumes un-archive exists.**
`AuthError::ProjectArchived`'s doc tells the operator the two remedies are
"un-archive the project, or move the member to a live one"
(`control_config/auth.rs:100-102`). The first of those is not a route.

## 3. What "what its keys resume meaning" would have to decide

Three separate resumptions, and they have different shapes.

**(a) The key refused while closed.** Because archive runs no cascade
(§1), an un-archive that only cleared `archived_at_ms` would silently
re-admit *every* key that was live at archive time: the compile step's
`archived.contains(project)` branch stops firing and the key falls straight
through to `merged.keys.push(KeyEntry { … })` (`directory.rs:2028-2071`).
Nothing distinguishes a key the operator meant to keep from one they
believed dead, because `403 project_archived` is not a state on the row — it
is derived, per compile, from the project's field. The row itself records
only `revoked_at_ms`, still `None` (`records.rs:280-282`,
`:294-298`). The two available spellings are therefore "resume everything"
(no record of intent, a fleet of secrets that go live at once) and "revoke
on archive, resume nothing" (which is a change to *archive*, not to
un-archive, and would make the operation destructive where it is currently
reversible in principle).

**(b) The budget window that elapsed.** Committed spend is keyed by
principal in a ledger the archive never touched. `BudgetWindow` has two arms
— `Total` (life of the project) and `Monthly` (resets at each UTC calendar
month boundary) — `control/budget.rs:105-112`. The reset is lazy:
`ProjectAccount::settle_time` runs at the top of every operation and, if
`window_start_ms(window, now_ms)` is past the stored `window_started_ms`,
zeroes `committed_usd` and clears every member's row
(`control/spend.rs:423-439`, `window_start_ms` at `:387-391`). So a project
archived in January and un-archived in March resumes with a *zeroed* month
if it is `Monthly`, and with its entire lifetime total intact if it is
`Total` — with no field anywhere saying the gap happened. (The same
asymmetry is what makes `WindowChangeUnsupported` a refusal at all:
`mutation.rs:268-280`.)

**(c) The fair-use sums that decayed.** Fair-use draws are bucketed by
wall-clock index with no window in the key, and pruned to the widest
configured window: `FairUseWindow::SevenDays.span_ms()` is the horizon and
`prune` splits off everything older (`control/fair_use.rs:567-579`, windows
and spans at `:250-275`). [fact-check 2026-09-04: the bucketing itself is `record` at `:561-566`; the pruning citation stands] So a project archived for more than seven days
un-archives with every rolling counter genuinely at zero — which is
arguably correct (no draws happened) but is not a decision anyone recorded,
and it is the one of the three that needs no un-archive machinery at all.

**What has changed since the deferral was written.** Two things, and both
are preconditions rather than answers:

- **Tombstones survive.** The identity-collision refusal that keeps the id
  retired now reads a durable row (§1), so an un-archive is a state
  transition on a document with a version rather than on a `Mutex` that
  outlived nothing (`directory.rs:73-107` records the change).
- **The document has a lineage.** A document's identity is `(lineage,
  version)`, not version alone (`roundhouse-core/src/control/directory.rs:
  28-60`), and the directory carries the pair on `Compiled`
  (`directory.rs:387-393`) and reports a regression with a typed cause when
  either moves backwards (`directory.rs:301-339`). An un-archive is an
  ordinary commit under that contract — one more arm on
  `DirectoryMutation`, one compare-and-set — so the *mechanism* costs
  nothing new; the whole cost is in (a)–(c).

## 4. Admin identity: what carries none, and what would have to change

**`KeyScope::Admin` deliberately carries no identity.** The enum has two
arms; `Turn(Admission)` carries the whole admission, and `Admin` is a unit
arm documented "Deliberately carries no principal: an admin acts on the
deployment, not from inside a project" (`control_config/mod.rs:612-619`).
The resolver produces it from a set membership test and nothing else: `if
admin_keys.contains(&hash) { return Ok(KeyScope::Admin); }`
(`control_config/mod.rs:1144-1146`), over `admin_keys: HashSet<String>` —
"`sha256(secret)` hex, for keys with no membership to spend as"
(`:644-645`). The file's own vocabulary is as bare: `admin_keys:
Vec<String>`, "Hashes of admin secrets. Unlike `keys`, these name no
membership" (`control_config/config.rs:408-411`).

**The M8 reasoning is one level down, in core.**
`roundhouse-core/src/control/mod.rs:14-29` states both halves: "A
[`Principal`] carries no key […] an unconfigured deployment, which
authenticates nothing, has none to give. The choice is between an optional
key id that is always absent on the open path and a type where the question
cannot be asked", and then "A key *record* — the id an audit line or a
revocation names — has no producer yet; it arrives with the admin plane, and
it will arrive next to the resolver too, not here." The record arrived; the
audit line did not.

**Every admin write is unattributed.** All four record types carry
`provenance: Provenance::Admin` and `created_at_ms`, and no actor of any
kind: `ProjectRecord` (`records.rs:130-157`), `UserRecord` (`:170-176`),
`MembershipRecord` (`:194-228`), `ApiKeyRecord` (`:258-292`). `Provenance`
itself is a two-arm enum — `Config | Admin` — answering "who may edit this",
not who did (`records.rs:43-56`).

**`DirectoryMutation` has nowhere to put an actor either.** The nine arms
carry only the entity data (`mutation.rs:170-210`), and the public entry
points take `(mutation, now_ms)` and `(project, user, now_ms)` with no
caller identity (`directory.rs:652-673`). The admin router's layer resolves
`KeyScope::Admin` and then drops it — `Ok(KeyScope::Admin) => next.run
(request).await` — putting nothing into request extensions
(`admin_api.rs:187-204`, route table `:117-141`).

**What an attributed admin write would need, and what it would cost.**

1. **A key id on the admin scope.** The id already exists and is already
   derivable for a file-declared key: `key_id(sha)` is `key_` plus the first
   sixteen hex characters of the hash, "derived rather than drawn, and that
   is what lets a *file-declared* key have an id at all — the file carries
   no id field, and adding one would make every existing deployment's config
   incomplete" (`records.rs:300-313`). `listing` already mints exactly that
   id for every file-declared admin key when it projects the file into the
   view (`directory.rs:1255-1268`), and `RevokeKey` already matches a
   file-declared key by `key_id(hash) == id` (`directory.rs:1719-1724`). So
   `KeyScope::Admin { key_id: String }` needs **no file format change** —
   the identity is a function of the hash the file already carries.
2. **An actor field on the four record types.** Additive and cheap by the
   records' own forward-compatibility rule: "Every optional field carries
   `#[serde(default)]`, and that is the forward-compatibility rule rather
   than tidiness" (`records.rs:23-33`), and the envelope tolerates unknown
   keys while a record does not (`agent-docs/PLAN-anthropic-messages.md:
   2438-2445`). But an actor *on the record* answers only "who created
   this". `PatchProject`, `ArchiveProject`, `RevokeKey` and the
   `UpsertMembership` update branch mutate rows in place
   (`directory.rs:1552-1619`, `:1643-1647`, `:1712-1736`), so a create-only
   attribution leaves every later edit unattributed — which is the
   difference between an actor field and an audit log.
3. **The file-declared admin key's identity.** It has a derived id (above)
   and nothing else — no name, no label, no `created_at_ms` (`listing`
   stamps `created_at_ms: None` for every projected row, `directory.rs:
   1255-1268`, and the reason is stated on `ProjectRecord::created_at_ms`:
   the file does not date its entries and an invented timestamp would be
   indistinguishable from one an operator could rely on,
   `records.rs:135-141`). So an audit line can say *which* admin key acted
   and never *who holds it*; naming a person needs a field the file does not
   have.
4. **What it costs the open-mode default: nothing.** `ControlPlane::Open`
   never yields `KeyScope::Admin` at all — `authenticate`'s open arm returns
   `KeyScope::Turn(Admission::open())` unconditionally
   (`control_config/mod.rs:1122-1123`) — and the admin router refuses open
   mode on the mode check before it reads a header
   (`admin_api.rs:192-195`, `AuthError::AdminRequiresControlPlane` at
   `control_config/auth.rs:111-129`). The core doc's argument against an
   optional key id is about `Principal`, which is on the *turn* path
   (`control/mod.rs:14-22`); `KeyScope::Admin` is only ever produced by a
   configured plane, so a required field on that arm is never absent. The
   directory's own "API lockout is impossible" argument is unaffected:
   it turns on a file-declared admin key being unrevocable, not on the scope
   being identity-free (`directory.rs:47-56`).

## 5. What an audit trail would consist of, and what exists to build it from

**What exists.** Turn-level provenance is thorough. `DecisionRecord` is
written to the session log before execution and carries the winner, the
losers, the policy name, the rationale, `turn_policy_digest` ("the audit
trail's answer to 'under what constraints was this chosen?'"), `budget_state`
and the rate card in force (`roundhouse-core/src/routing/mod.rs:542-600`).
`explain_last_route` republishes that as an agent-readable tool — "the audit
trail as a tool, minus the money" — carrying `chosen`, `rationale`,
`routing_policy`, `budget_state`, `turn_policy_digest` and `considered`
(`roundhouse-mcp/src/surface.rs:497-526`, the tool at `:213`, its plane
implementation at `roundhouse-mcp/src/plane.rs:593-609`). The plan names it
as the audit trail's read surface
(`agent-docs/PLAN-agentic-control-plane.md:706`), and the steer path is
audited through the same digest
(`roundhouse-mcp/src/store.rs:27`, `overlay.rs:185`).

**What does not exist: any record of an admin mutation.** The admin module
doc says so in one line — "No audit trail, no key rotation, no rate
limiting, no pagination, no rate-card editing, and no credential CRUD"
(`admin_api.rs:36-40`). Nothing writes one: `SessionEventKind` has sixteen
variants and every one is about a turn, a side call or a validation
(`roundhouse-core/src/event.rs:216-542`), and there is no second log. The
directory writes only the records document; the only trace an admin write
leaves is the state it produced plus `created_at_ms` on rows it created —
so a `PATCH`, an archive, a revoke and a membership delete leave *no* trace
of having happened beyond their effect, and a revoke leaves only
`revoked_at_ms` with no actor (`records.rs:280-282`).

**So an audit trail would consist of** (this is a shape, not a ruling): a
per-mutation row carrying the actor id from §4.1, the mutation arm, the
entity it named, `now_ms`, and the `(lineage, version)` the commit produced —
the last being the one field that makes an admin log comparable across nodes,
since the version is already the directory's own ordering
(`roundhouse-core/src/control/directory.rs:28-60`). Whether it rides the
same document (bounded, and every write rewrites the whole document —
`store.rs`'s contract is whole-document compare-and-set) or a sibling append
family is exactly the placement question D2 ruled for the records
themselves (`agent-docs/PLAN-frontier-selection.md:515-542`).

## 6. What a node can say about itself today

**`ControlDirectory::status()` is the one assembled answer.** It returns
`DirectoryStatus { served_version, refused_version, divergence,
divergences_named }` (`directory.rs:715-727`, type at `:341-365`), one
accessor rather than three "because the three are only meaningful together:
'serving version 4' is reassuring on its own and alarming beside 'refused
version 5'". It never refreshes — "it reports what this node has already
observed, and a read of past events that went to the store could observe a
new one on the way" (`:712-714`). A `Fixed` directory (open mode) answers
all zeros/`None` (`:719-724`); the managed arm reads `current` and the
divergence state under two locks (`:856-868`).

- `served_version` — the version this node last compiled (`:355-356`,
  `Compiled::version` at `:388`).
- `refused_version` — "the newest version this node loaded and could
  **not** compile", kept beside the served one because "a node one version
  behind because nothing has changed and a node one version behind because
  it refuses what changed are opposite situations that look identical from
  `version` alone" (`:414-430`).
- `divergence` — the typed `DirectoryDivergence { version, differs }`, where
  `differs` is a `Vec<DivergentInput>` over `file | catalog | fleet |
  admission_cache_ttl_ms` (`control_config/directory/document.rs:189-229`,
  `CompiledUnder::differs_from` at `:166-181`). Named once per stored
  version and never refused (`directory.rs:788-853`).
- `divergences_named` — the count, "which is what makes 'warned once'
  observable without a log harness" (`:362-364`).

**Beside it, `last_regression()`** returns `DirectoryRegression { from, to,
cause }` with `RegressionCause::{Version, Lineage { from, to }}` — the store
answering a lower version or a different run of its counter
(`directory.rs:684-700`, types at `:301-339`).

**The metrics fold is node-local state.** `MetricsRecorder` is
`{ fold: Arc<RwLock<MetricsFold>> }` (`roundhouse-core/src/metrics/mod.rs:
156-158`), fed as a `SessionObserver` (`:247-251`), and `/v1/metrics`
answers from it with no store read at all — "the numbers come from
`MetricsRecorder`, which every session has been feeding as it commits, so
answering a request here is a fold already done rather than a sweep over the
log" (`metrics_api.rs:6-11`, routes at `:75-76`). The fold is *derivable*
from the log by replay but nothing at boot performs that replay — the D1
inventory's row 15 states both halves and its §"The metrics fold's
recomputability is a property, not a boot path"
(`agent-docs/research/roundhouse-state-inventory-7c5369a.md:67`, `:533-540`,
per-node reading at `:186`).

**The `shared_backend` arm chosen.** `SharedBackend::{Shared { url },
PerProcess}` (`shared_backend.rs:99-112`) resolved once
(`:143-148`) into `Backends::{Shared, PerProcess}` (`:163-194`), logged once
at boot: the shared arm as an `info!` naming what is shared and never the
URL, because a `redis://` URL may carry credentials
(`:302-312`); the per-process arm as a `warn!` that names the archived
project's tombstone by name (`:322-332`).

**There is a node identity, minted per process.** `EngineConfig::node_id` —
"Identity presented to the session lease. It must be unique for every live
engine. The default mints one so two server processes cannot accidentally
present themselves as one holder" (`roundhouse-server/src/engine.rs:449-455`),
defaulting to `format!("node_{}", uuid::Uuid::new_v4().simple())` (`:488-491`),
and `main.rs` takes that default (`main.rs:946-961`, `..EngineConfig::default()`
at `:960`). It reaches `Session::open_observed` as the lease holder
(`engine.rs:1087`, `Lease { session_id, node_id, fencing_token, expires_at_ms }`
at `roundhouse-core/src/store.rs:46-63`). It is a *tenure* identity, fresh on
every restart, not a stable node name.

**The boot warnings that were deleted.** M16.1 deleted the
`control_plane_file_configured` flag and the long warning it gated — at
`3cb62bd` it lived at `main.rs:809-825` and told the operator that "sessions
and committed spend just became durable in Redis, but admin-created tenancy
… still lives only in memory", naming the tombstone hazard and the
not-yet-built `DirectoryStore`. Both are gone at `1d016f2`; what stands in
their place is a comment recording the deletion — "Both are deleted, because
the gap is closed rather than because it stopped mattering"
(`main.rs:736-749`). D2 ruled the deletion in advance
(`agent-docs/PLAN-frontier-selection.md:563-569`). What survives is the
per-process arm's warning in `shared_backend.rs:322-332`, which is about a
deployment with no Redis at all.

## 7. No HTTP route reports any of it

The deployment mounts six routers, merged in `main.rs:1041-1104`, with
seventeen routes between them:

| Router | Routes | `file:line` |
|---|---|---|
| `http::router` | `POST /v1/sessions`, `POST /v1/sessions/{id}/responses`, `GET /v1/sessions/{id}/events` | `http.rs:141-149` |
| `metrics_api` | `GET /v1/metrics`, `GET /v1/metrics/dashboard` | `metrics_api.rs:75-76` |
| `admin_api` | ten `/v1/admin/…` routes | `admin_api.rs:117-141` |
| `relay_api` | `GET /v1/sessions/{id}/{atof,trajectory,optimization}` | `relay_api.rs:90-95` |
| `mcp_api` | `POST` at `MCP_MOUNT_PATH` | `mcp_api.rs:410-413` |
| `responses_api` / `messages_api` | `POST /v1/responses`; `POST /v1/messages`, `POST /v1/messages/count_tokens` | `responses_api.rs:215-218`, `messages_api.rs:250-257` |

**Negative — nothing calls `ControlDirectory::status()` or
`last_regression()` outside their own module's tests.** `git grep -n
"status()" 1d016f2 -- crates` returns, for these, only
`directory.rs:725` (the delegation) and twenty-odd assertions in
`control_config/directory/tests.rs:3003-3282`; `git grep -n
"last_regression" 1d016f2 -- crates` returns only `directory.rs` itself and
`directory/tests.rs:2466-2679`. `DirectoryStatus` appears in exactly three
non-test places: its definition (`directory.rs:354`) and two re-exports
(`control_config/mod.rs:123`, `lib.rs:100`).

**Negative — there is no health, readiness or status route.** `git grep -in
"healthz\|/health\|readyz\|liveness\|/status\b" 1d016f2 -- crates` matches
only two prose uses of the word "liveness" in
`prefix_admission.rs:603` and `tests/messages_api_surface.rs:2734`. The
route table above is the whole surface.

**Negative — nothing on the metrics surface is node-aware.** `git grep -n
"node" 1d016f2 -- crates/roundhouse-core/src/metrics/snapshot.rs
crates/roundhouse-server/src/metrics_api.rs
crates/roundhouse-server/src/dashboard.html` returns nothing: the word does
not occur in the snapshot type, the router, or the served page. A reader of
`/v1/metrics/dashboard` is given no signal that they are seeing one node's
slice.

**Negative — `node_id` is not reachable from any router.** It lives on
`EngineConfig` (`engine.rs:455`) and is passed to the lease
(`engine.rs:1087`); the six routers take a store/transport, a
`MetricsRecorder` + `MetricsConfig` + `PlaneSource`, a `ControlDirectory` +
ledger, or an MCP surface — none takes the engine or its config
(`main.rs:1041-1104`). `git grep -n "node_id" 1d016f2 --
crates/roundhouse-server/src` returns three hits, all in `engine.rs`. [fact-check 2026-09-04: the grep returns nine hits, six of them test parameters; and `http::router`, `responses_router` and `messages_router` do take `Arc<Engine>` (`main.rs:1041-1104`) — what holds is that no handler reads `config.node_id` and the engine has no accessor for it, and the conclusion stands on that]

## 8. What Relay 0.8.2 exposes, as a precedent

**`GET /healthz`, first route on the gateway router**
(`nemo-relay-cli-0.8.2/src/server/mod.rs:635`, router at `:632-651`; 0.8.2
registry). Its body is five fields — `status`, `service`, `version`,
`bootstrap_protocol`, `instance_id` — with `200`/`409` distinguishing "ok"
from "incompatible" (`:794-808`, handler at `:758-793`).

Three properties are worth taking, because they are what make it more than a
liveness ping:

- **It is an identity probe, not only a health probe.** The client
  classifies a response into `RelayHealth::{Compatible, Incompatible,
  Foreign, Unavailable}` (`src/gateway/client.rs:29-34`), and
  `classify_health_response` rejects anything whose `service` is not
  `nemo-relay` or whose `bootstrap_protocol` differs as `Foreign`
  (`:463-500`). That is how a launcher tells "our gateway" from "some other
  process on this port".
- **It carries a per-instance id** used to tell one live instance from
  another (`:253-269`, `compatible_instance_id`), validated non-empty and
  ≤128 chars (`:492-498`).
- **It is the readiness gate for the launcher** — "gateway did not become
  ready at {}/healthz" (`src/process/launcher.rs:837`) — and an
  authenticated heartbeat that refreshes idle activity (`state.touch()` at
  `src/server/mod.rs:787`; the test naming that rule is
  `tests/coverage/shared/server_tests.rs:637`).

The adaptive crate has a second, unrelated `health` — a cache-store
readiness future (`nemo-relay-adaptive-0.8.2/src/response_cache/store.rs:105`,
`:292`, `:418`) — which is a store probe, not an HTTP surface.

## 9. What a minimal surface would carry, and who would read it

Assembled from §6 alone — every field below already exists as a value on the
process; none needs new bookkeeping:

| Field | Source | `file:line` |
|---|---|---|
| node id | `EngineConfig::node_id` (per-tenure, not stable) | `engine.rs:449-455`, `:488-491` |
| served / refused version | `DirectoryStatus` | `directory.rs:353-365` |
| lineage | `Compiled::lineage` (not on `DirectoryStatus` today) | `directory.rs:387-393` |
| divergence + count | `DirectoryStatus::{divergence, divergences_named}` | `directory.rs:361-364`, `document.rs:189-229` |
| last regression | `ControlDirectory::last_regression()` | `directory.rs:684-700`, `:301-339` |
| shared vs per-process | the `Backends` arm | `shared_backend.rs:163-194` |
| plane mode | `ControlPlane::{Open, Configured}` | `control_config/mod.rs:630-668` |

Note one gap: **`DirectoryStatus` carries the served version but not the
lineage**, though `Compiled` holds both beside each other precisely because
"a store that lost its key answers a version this node may well have claimed
before, and only the lineage tells the two apart"
(`directory.rs:389-393`). Two nodes' `served_version: 4` are not comparable
without it.

The three readers the brief names, and what each needs:

- **An operator**, mid-rollout, asking "is this node current, and if not,
  why" — needs the served/refused pair and the typed `differs` list, which
  is exactly the pair R19 said "is the first row of a per-node status
  surface roundhouse does not yet have"
  (`agent-docs/PLAN-frontier-selection.md:582-586`, restated in the M16.1
  ruling at `agent-docs/PLAN-anthropic-messages.md:2430-2434`).
- **A load balancer**, needing a cheap unauthenticated boolean. Note the
  tension: everything in the table above is deployment state, and every
  existing router is gated (the admin layer on `KeyScope::Admin`,
  `admin_api.rs:187-204`; metrics on a `PlaneSource`,
  `metrics_api.rs:44-49`). Relay's answer is to serve `/healthz`
  unauthenticated but to make the *body* prove identity rather than
  disclose state (`server/mod.rs:794-808`), which is the shape that would
  let a probe distinguish roundhouse from a foreign occupant without
  publishing a version an anonymous caller has no business reading.
- **The dashboard**, which today shows one node's fold with nothing saying
  so (§7's third negative). The status fields are what would let it label
  its own slice — which is the seam where this question meets D3's first
  one.

---

## Negatives, and what was searched

1. **Nothing un-archives a project, and nothing deletes one.**
   `DirectoryMutation` has nine arms (`mutation.rs:170-210`); the router
   binds ten routes and `DELETE /projects/{id}` goes to `archive_project`
   (`admin_api.rs:117-141`, `:497-506`). No arm clears `archived_at_ms`:
   the field is written in exactly two places, `Some(now_ms)` in the archive
   arm (`directory.rs:1618`) and `None` at create (`:1549`) and in the
   file's projection (`:1206`).
2. **Archiving revokes no key.** The archive arm is three statements and
   touches no `keys` (`directory.rs:1610-1619`); the only cascade in the
   file is `DeleteMembership`'s (`:1667-1678`).
3. **Nothing attributes an admin write.** All four record types read in full
   (`records.rs:130-292`): `provenance` and timestamps, no principal, key id
   or actor. `DirectoryMutation`'s arms carry no actor (`mutation.rs:170-210`)
   and `apply`/`mint_*` take no caller (`directory.rs:652-673`). The admin
   layer drops the resolved scope (`admin_api.rs:197`).
4. **There is no admin mutation log.** `git grep -in "audit" 1d016f2 --
   crates/roundhouse-server/src` returns only the turn-side audit vocabulary
   and one line saying the trail does not exist (`admin_api.rs:38`).
   `SessionEventKind`'s sixteen variants are all turn-scoped
   (`roundhouse-core/src/event.rs:216-542`).
5. **No HTTP route reports directory status, divergence or regression.**
   `git grep -n "status()" 1d016f2 -- crates` and `git grep -n
   "DirectoryStatus\|last_regression" 1d016f2 -- crates`: every hit outside
   `control_config/directory.rs` is either a re-export
   (`control_config/mod.rs:123`, `lib.rs:100`) or an assertion in
   `control_config/directory/tests.rs`.
6. **No health/readiness/status route exists.** `git grep -in
   "healthz\|/health\|readyz\|liveness\|/status\b" 1d016f2 -- crates` — two
   prose matches, no route. The seventeen routes are enumerated in §7 from
   `git grep -n "\.route(" 1d016f2 -- crates/roundhouse-server/src`.
7. **Nothing in the metrics surface is node-aware.** `git grep -n "node"
   1d016f2 --` over `metrics/snapshot.rs`, `metrics_api.rs` and
   `dashboard.html`: no matches.
8. **`node_id` is not exposed.** `git grep -n "node_id" 1d016f2 --
   crates/roundhouse-server/src` — three hits, all `engine.rs`. [fact-check 2026-09-04: nine hits; see the note in §7 — no handler reads it]
9. **No stable node name exists.** `EngineConfig::default()` mints a fresh
   `node_{uuid}` per process (`engine.rs:488-491`) and `main.rs` does not
   override it (`main.rs:946-961`); `git grep -n
   "node_id\|instance_id\|NodeId" 1d016f2 -- crates/roundhouse-server/src
   crates/roundhouse-core/src` finds no configured or persisted node
   identity anywhere. [fact-check 2026-09-04: that grep used git's basic regex, where `\|` is a literal, and matched nothing by accident; re-run with `-E` it returns 28 hits across the two crates, none a configured or persisted identity beyond the per-tenure `EngineConfig::node_id` and the `Lease` that carries it — the conclusion stands, on that search]
10. **The file cannot name an admin key.** `admin_keys: Vec<String>` of
    hashes only (`control_config/config.rs:408-411`); `listing` stamps
    `created_at_ms: None` on every projected row (`directory.rs:1255-1268`),
    so a file-declared admin key has a derived id and no other attribute.

## Open questions this evidence does not settle

1. Whether un-archive resumes keys wholesale, or archive gains a cascade so
   that resumption is empty by construction — the second is a change to a
   shipped, currently-reversible operation.
2. Whether an un-archive records the closed interval (an `archived_at_ms` /
   `unarchived_at_ms` pair, or a list) so §3(b)'s zeroed month is visible
   rather than inferred, and whether the reconciliation view should say so.
3. Whether attribution lands as an actor field on records (create-only) or
   as a mutation log (covers patch/archive/revoke), and if a log, whether it
   rides the records document — every commit rewrites the whole document —
   or a sibling append family.
4. Whether `DirectoryStatus` gains the lineage, without which two nodes'
   version numbers are not comparable (`directory.rs:389-393`).
5. Whether a status surface is authenticated (matching every other router)
   or split the way Relay's `/healthz` is — an unauthenticated identity
   probe plus an authenticated detail read.
6. Whether the node identity a status surface reports should be the
   per-tenure `node_{uuid}` or a configured, restart-stable name; the former
   makes two restarts of one machine look like two nodes.

---

## Fact-check (2026-09-04)

An independent re-derivation of every negative and every high-stakes claim above, from the primary sources at the pinned revision (roundhouse `1d016f2`; Relay at the 0.8.2 registry sources), by a second reader who did not write this document. Verdicts: 27 verified, 3 corrected, 0 unestablished.

Re-derived all 20 numbered claims and all 10 negatives from source at 1d016f2 (plus Relay 0.8.2 registry). All 20 numbered claims verified, citations accurate to within a line or two, quoted text found verbatim (records.rs deferral, core/mod.rs Principal doc, auth.rs remedy sentence, PLAN quotes, Relay healthz body/RelayHealth enum all confirmed). One minor citation-range nit: claim 6's fair-use citation covers only prune(), not the bucketing in record() (561-566). 8 of 10 negatives verified exactly by re-running the cited greps. Two negatives need correction: negative 8 ("node_id not reachable from any router") has a wrong grep count (9 hits in roundhouse-server/src, not 3 — 6 are in test files) and a false claim that no router takes the engine (http::router, responses_api, messages_api all take Arc<Engine>); the underlying conclusion (no handler reads/exposes node_id) is independently confirmed true via a more targeted grep, but the stated mechanism is wrong. Negative 9 ("no stable node name") cites a grep with an unescaped "|" that is literal (not alternation) under git's default BRE, so it returns zero hits by syntax accident rather than by a genuine three-way search; re-run with -E it returns 28 hits, none of which is a second persisted node identity, so the conclusion survives but the evidence as written doesn't establish it. Full notes with line-by-line citations in the factcheck file.

Corrections, each also applied above as a dated bracketed note:

- **node_id is not reachable from any router (draft: three grep hits, all engine.rs; none of the six router constructors takes the engine).** — git grep -n node_id in roundhouse-server/src returns 9 hits, not 3 (6 more in mcp_api/tests.rs and prefix_admission/tests.rs, unrelated test params). Also main.rs:1041-1104 shows http::router, responses_api::responses_router, and messages_api::messages_router all take Arc::clone(&engine) — 'none takes the engine' is false. However the actual conclusion (no handler reads/exposes node_id) is independently true: grep for config.node_id/.node_id in http.rs, responses_api.rs, messages_api.rs returns zero hits, and engine.rs has no pub node_id() accessor.
- **There is no stable, restart-surviving node name anywhere (draft cites a git grep for 'node_id|instance_id|NodeId' finding nothing).** — Ran the cited command literally: git's default BRE treats '|' as a literal character (no -E), so it searches a nonexistent 19-char literal string and returns nothing by syntax accident, not by a real three-way search. Re-run with -E it returns 28 hits across roundhouse-server/src and roundhouse-core/src; inspected all — none is a second/persisted/configured node identity distinct from the per-tenure EngineConfig::node_id and Lease.node_id that carries it, so the stated conclusion survives, but the cited evidence doesn't actually establish it.
- **Fair-use sums decay to zero on their own, needing no un-archive machinery.** — Substance verified (prune() at fair_use.rs:567-579 splits off buckets older than SevenDays.span_ms()), but the cited range doesn't cover the bucketing mechanism (record() at :561-566) the claim also describes; :250-275 for FairUseWindow/span_ms is accurate.
