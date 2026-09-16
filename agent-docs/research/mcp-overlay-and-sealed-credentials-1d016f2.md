# The MCP overlay maps and the sealed credential store, under the document contract

**A read pinned to a revision.** roundhouse at `1d016f2` ("M17 thermo-nuclear
review: seven findings, all valid; the engine's join is dialect-aware"); every
`crates/…` and `agent-docs/…` citation below is a line of that revision, read
with `git show 1d016f2:<path>`, never the working tree. NeMo Relay citations are
the published registry sources at
`/root/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/nemo-relay-*-0.8.2`,
cited as "0.8.2 registry". Dated 2026-09-04.

Evidence for D3, dive 2. Nothing here rules; the rulings are the orchestrator's.

---

## 1. What `roundhouse_mcp::ControlStore` holds

### 1.1 Four families, one `Mutex`, one process

`ControlStore` is a single `Mutex<Inner>` (`crates/roundhouse-mcp/src/store.rs:203-205`)
over four `HashMap`s and a sweep cursor
(`crates/roundhouse-mcp/src/store.rs:207-216`):

| # | Family | Key | Value | Declared at |
|---|---|---|---|---|
| 1 | overlays | `SessionId` | `OverlayEntry { overlay: SessionOverlay, written_at_ms }` | `store.rs:209`, entry at `:224-228` |
| 2 | intents | `SessionId` | `IntentRecord { goal, plan_steps, done_when, declared_at_ms }` | `store.rs:210`, record at `:166-172` |
| 3 | outcomes | `SessionId` | `OutcomeRecord { session, principal, outcome, note, reported_at_ms }` | `store.rs:211`, record at `:188-199` |
| 4 | bindings | `BindingId` (`rhb_` + 32 lowercase hex) | `SessionBinding { principal, session, minted_at_ms }` | `store.rs:212`, record at `:151-156`; prefix at `:118`; mint at `:130-133` |

The module doc names the store's own posture without hedging: "Every family
here is a `HashMap` behind a `Mutex` in one process"
(`crates/roundhouse-mcp/src/store.rs:8`), and the cost is stated rather than
left to be discovered — "an overlay does not survive a process restart, and in
a multi-node deployment it applies only on the node that took the MCP call"
(`store.rs:22-24`). The engine's own half repeats it: `ControlStore` is "a
`HashMap` in this process: node-local, lost on restart, and shared with the MCP
surface that mounts beside this engine"
(`crates/roundhouse-server/src/engine/control.rs:10-12`).

**A documentation divergence worth carrying into the ruling.** The module title
and three later paragraphs still say the store holds *steer payloads*
(`store.rs:4`, `:27-30`, `:34-35`, `:74-81`), but `Inner` holds
`overlays, intents, outcomes, bindings` (`store.rs:209-212`); the steer payload
was deleted by M10.0 and the correction is recorded on `OutcomeRecord` itself
(`store.rs:176-187`) and in the composition root (`main.rs:891-896`). The
state inventory flagged the same drift at its own pin
(`agent-docs/research/roundhouse-state-inventory-7c5369a.md:524-531`); it is
still there at `1d016f2`.

### 1.2 Who writes, who reads, and what the engine spends

**Writers — all five write-annotated MCP tools.** The surface declares eight
tools (`crates/roundhouse-mcp/src/tools.rs:196, 216, 234, 256, 287, 310, 322, 343`),
of which three are reads and five write to this store — the split is asserted as
a literal so a ninth tool arrives unclassified
(`crates/roundhouse-mcp/tests/tool_surface.rs:1706-1709`):

| Tool | Store call | Site |
|---|---|---|
| `prefer` | `set_mode_axis` | `plane.rs:212` (via `install`, `:195-216`) |
| `set_quality_floor` | `set_floor_axis` | `plane.rs:213` |
| `declare_intent` | `set_intent`, then read back | `plane.rs:417-434` |
| `report_outcome` | `record_outcome` | `store.rs:395-414` |
| `init_session` | `bind_session` | `plane.rs:388-390` |

`status` reads without spending (`plane.rs:367`, rendering at `:377`).

**Readers — and only two of the four families have a production reader.**

- **overlays** — read twice: `ControlStore::overlay` for rendering
  (`store.rs:264-269`, called at `plane.rs:208`, `:367`, `:455`) and
  `ControlStore::consume_overlay` for spending (`store.rs:344-358`).
- **intents** — read by the engine at the interjection seam
  (`engine/control.rs:165-175`, `control.rs:168`), handed to the validator as
  `Objective::Declared`; called from `engine.rs:1198`.
- **outcomes** — `outcome_for` (`store.rs:417-419`) has **no caller outside
  this crate's own tests**. Established by `git grep -n "outcome_for" 1d016f2 -- crates`,
  whose only non-test hits are the definition and its own unit test at
  `store.rs:650-672`.
- **bindings** — `binding_in_log` (`store.rs:501-515`) likewise has **no
  production caller**; the surface says so in the tool's own answer
  ("nothing in this deployment resolves a session from a binding yet",
  `plane.rs:400-404`). `binding_in_items` (`store.rs:568-570`) is used only by
  `crates/roundhouse-server/tests/mcp_surface.rs:1753, 1766, 1801`.

**What the engine spends at the start of a turn.** Exactly one thing: the
overlay. `Engine::narrowed_admission` (`engine/control.rs:115-128`) calls
`consume_overlay` and composes the result through `TurnPolicy::narrow`
(`control.rs:127`); it is called from the `Interjection::Proceed` arm of
`run_turn` at `crates/roundhouse-server/src/engine.rs:1352`, after the dedup
short-circuit and before `plan`, so "the turn routed under the overlay is the
turn that spent it" holds by construction (`engine.rs:1331-1352`,
`engine/control.rs:56-77`). The intent is read on the same turn but is *not*
spent (`engine/control.rs:165-175`); nothing else in the store is touched by a
turn.

### 1.3 Sweep discipline, and what it does *not* bound

One retention for four families: `RETENTION_MS = 24h`
(`store.rs:103`), deliberately not tunable — "a knob here would be a per-family
lifecycle in disguise, which is M8's" (`store.rs:101-102`). The sweep is
rate-limited to once per `SWEEP_INTERVAL_MS = 60_000`
(`store.rs:110`) and drops by age across all four maps in one pass
(`store.rs:236-250`).

Two properties of the sweep matter for any durable design:

- **It runs on the four writes that carry a clock and on nothing else** —
  `mutate_axis` (`store.rs:318`), `set_intent` (`store.rs:363`),
  `record_outcome` (`store.rs:411`), `bind_session` (`store.rs:442`).
  `consume_overlay` takes no clock and does not sweep (`store.rs:344-358`); no
  read sweeps. A node whose agents stop calling MCP tools stops sweeping.
- **There is no capacity cap at all.** `sweep` only `retain`s by age
  (`store.rs:242-249`); no constant in the file bounds a map's size. The bound
  is therefore *24 hours × arrival rate*, not a count — which is a weaker bound
  than the correlation family's, where a table is bounded by *both*
  `REMEMBERED_CALLS = 4096` / `REMEMBERED_THREADS = 1024` and a staleness bound,
  and "neither bound waits on the other"
  (`crates/roundhouse-core/src/control/correlation.rs:187-211`, `:66-81`).

A leak the sweep bounds rather than closes is documented at `store.rs:59-71`:
`Conversations::commit` rebinds a client's cache key to a fresh `SessionId` when
the resent history disagrees with the log, and every family here is keyed by the
*pre-fork* id, so the agent's standing narrowing silently stops applying and the
old records are orphaned until the sweep collects them
(`crates/roundhouse-server/src/conversations.rs:468, 484`).

The lock recovers from poisoning rather than propagating
(`store.rs:517-528`) — a projection whose loss degrades to the ceiling is worth
serving from possibly-stale state.

---

## 2. What a restart loses, family by family — and the "silent policy reversion"

### 2.1 The ruling on the claim, against the engine's read site

**Confirmed in substance, partially refuted in its adjective.** The mechanism is
real and bounded; "silent" is true only in the narrow sense that nothing
*announces* it — two observables exist, and one of them is a promise the surface
already made and that the restart falsifies.

**Confirmed — the mechanism.** With no overlay, `narrowed_admission` takes the
`else` arm and returns `admission.clone()` unchanged
(`engine/control.rs:120-126`). The admission is the key's own compiled ceiling,
so the turn routes under the deployment's ceiling rather than under the agent's
narrowing. Composition is `TurnPolicy::narrow`, which is total and can only
shrink (`engine/control.rs:81-85`, `crates/roundhouse-mcp/src/overlay.rs:8-11`),
so the reversion **widens back to the ceiling and never past it**
(`store.rs:24-26`). No path exists by which losing an overlay admits a target
the key does not.

**Partially refuted — it is not unobservable.**

1. **The audit trail.** Every routed turn writes a `DecisionRecord` carrying
   `turn_policy_digest`, "the audit trail's answer to *under what constraints was
   this chosen?*, recorded on the decision itself so a policy change is visible
   on the very next routing event with no side channel able to disagree with it"
   (`crates/roundhouse-core/src/routing/mod.rs:555-568`). The digest of the
   effective policy differs before and after the loss. The store's own doc leans
   on exactly this: "the audit trail shows the change through
   `turn_policy_digest` either way" (`store.rs:26-27`).
2. **`status`.** It renders the overlay out of the store
   (`plane.rs:367`, `:377`), so an agent that asks after a restart sees an empty
   overlay rather than a stale one.

**And the sharper form of the claim, which the framing understates.** Both
overlay writers answer with `policy_digest: effective.digest()`
(`plane.rs:229-236`), and the engine states the promise those answers make:
"`status`'s promise — that the digest it reports is the string the next
`DecisionRecord` will carry" (`engine/control.rs:72-73`). A restart between the
`prefer` call and the next turn makes that promise false, and **nothing refuses,
warns, or records the falsification**: `narrowed_admission` has no `tracing` call
on either arm (`engine/control.rs:115-128`). So the honest statement is not
"a silent policy reversion" but *a broken promise the tool already made, whose
only trace is a digest an operator would have to go and compare*.

### 2.2 Per family

| Family | Lost on restart | Consequence | Derivable from anything durable? |
|---|---|---|---|
| overlays | yes | routing widens to the key's ceiling, never past it (`store.rs:24-26`, `engine/control.rs:14-18`) | no — it is an agent's request held nowhere else |
| intents | yes | `objective()` degrades to `Objective::from_items` over the log — "precision in a brief, never a routing decision" (`engine/control.rs:160-164`) | partially: the fallback is a real objective, just a worse one |
| outcomes | yes | nothing: no production reader (§1.2) | no |
| bindings | yes | the join fails closed — the `rhb_…` *token* survives in the session log, the record it resolves to does not (`store.rs:531-559`) | half: `binding_ids_in_items` re-finds the token, nothing re-mints the record |
| sweep cursor | resets to 0 | the first write of a process's life sweeps (`store.rs:213-215`) | — |

**A second, live hole the restart framing hides.** `plane.rs:180-186` states
that "the *leaves something routable* guarantee is enforced at write time only,
and it holds because the catalog outlives every overlay written against it",
and names durability as exactly where that stops being true: "M8 is where that
stops being true — a durable overlay survives the restart that reloads the
catalog — and the place to re-derive is the seam that can see the empty set,
which is the engine's own admission path". So a durable overlay is not a
straight port: it requires a re-derivation at `narrowed_admission` that does not
exist today.

---

## 3. What a second node cannot see

Everything in §1: a `ControlStore` is minted inside `serve` at
`crates/roundhouse-server/src/main.rs:911` (`Arc::new(ControlStore::new())`) and
handed to the engine (`main.rs:993`) and to the MCP router
(`main.rs:1067-1082`, argument at `:1081`). It is **not** one of the families
`shared_backend::open` chooses. That function is the deployment's one switch and
it now covers five families — sessions, spend, fair use, correlation and the
admin directory (`crates/roundhouse-server/src/shared_backend.rs:4-5`, `:18-23`,
`:37-43`, `:60-69`) — and the MCP store is in none of them (established by
reading the `use` list at `shared_backend.rs:48-58` and by
`git grep -n "ControlStore" 1d016f2 -- crates/roundhouse-server/src`, whose only
composition-root hit is `main.rs:911`).

**M14.1 made the split sharper rather than milder, and that is the argument for
acting now.** Session *identity* became durable: the correlation maps moved to
Redis, so "a client that reconnected to another node kept its cache key and lost
its generation" stopped being true and M12.1's F9 refusal now means "never bound
*anywhere*" (`crates/roundhouse-store-redis/src/correlation.rs:7-13`). The MCP
surface resolves a conversation through those shared tables
(`crates/roundhouse-mcp/src/reads.rs:301-336`; "the tables are in a store shared
across nodes now, and an unreachable one is not an unknown id",
`reads.rs:262-264`). The consequence: a `prefer` on node A and the next turn on
node B now resolve to the **same `SessionId`** and node B finds **no overlay** —
where before M14.1 node B would more often have refused the MCP call outright.
The identity is deployment-wide; the state keyed by it is not.

---

## 4. Which durable shape each map is, and what each placement costs

Two shapes exist in the tree, and they are genuinely different contracts.

### 4.1 The correlation family (M14.1/M14.2) — per-principal keyed rows with a staleness bound

- **Contract**: `CorrelationMaps`, six async methods, `Send + Sync + 'static`,
  `#[async_trait]` for dyn compatibility
  (`crates/roundhouse-core/src/control/correlation.rs:237-306`).
- **Errors**: one arm, `Backend` — no "not found", because "a caller can do
  nothing different with any of them" and distinguishing them on a shared store
  would be an enumeration oracle across tenants (`correlation.rs:213-228`).
- **Bounds**: two constants named in core so both backends age against the same
  number — `CALL_BINDING_STALENESS_MS = 6h` (`correlation.rs:153`),
  `THREAD_BINDING_STALENESS_MS = 7d` (`correlation.rs:164`) — plus capacity caps
  (`correlation.rs:202`, `:211`). The mechanism differs by backend: Redis hands
  it to `PEXPIRE`, memory enforces it in `AgedTable` (`correlation.rs:66-81`).
- **Keys**: one Redis key per binding, `rh:v1:corr:call:{<principal>}:<tool_use_id>`
  and `rh:v1:corr:thread:{<principal>}:<thread_id>`, plus
  `rh:v1:corr:gen:{<key>}` (`crates/roundhouse-store-redis/src/correlation.rs:15-19`).
  One key per binding beat one hash per principal for exactly one reason: **a
  hash field cannot expire** (`store-redis/src/correlation.rs:21-35`).
- **Scripts**: one, for `bind_call` only; generation and thread writes are plain
  `SET` (`store-redis/src/correlation.rs:61-76`).
- **Contract assertions**: 10
  (`crates/roundhouse-core/src/control/correlation/contract.rs`, `pub async fn`
  at `:85, 121, 143, 180, 214, 247, 271, 302, 346, 397`), instantiated against
  both backends by `correlation_maps_contract_suite!`.

### 4.2 The directory family (M16.1) — one versioned opaque document

- **Contract**: `DocumentStore`, three async methods over `Vec<u8>`, "no opinion
  about the bytes" (`crates/roundhouse-core/src/control/directory.rs:12-13`,
  `:162-185`). Identity is `(lineage, version)`, not version alone
  (`directory.rs:28-58`, `DocumentVersion` at `:122-126`).
- **Errors**: two arms — `Concurrent { expected, found }` and `Unavailable`
  (`directory.rs:135-144`), mapped one-for-one to the seam above
  (`control_config/directory/document.rs:420-427`) because `409` and `503` are
  different answers.
- **Bounds**: **none by time.** "There is no `PEXPIRE` anywhere in this module,
  and that absence is the decision — a TTL on this key would silently
  un-configure a deployment that had a quiet week"
  (`crates/roundhouse-store-redis/src/directory/scripts.rs:18-23`).
- **Keys**: exactly one for the whole deployment, `rh:v1:dir:records`, a hash of
  three fields (`version`, `lineage`, `document`), no Cluster hash tag
  (`store-redis/src/directory.rs:14-16`, `:18-43`; fields at `:130-138`).
- **Scripts**: one, `commit`, the only operation with a condition in it; `load`
  and `version` are each one `HMGET` (`store-redis/src/directory.rs:69-78`,
  script at `store-redis/src/directory/scripts.rs:70`).
- **Contract assertions**: 9
  (`crates/roundhouse-core/src/control/directory/contract.rs`, `pub async fn`
  at `:56, 84, 130, 176, 207, 268, 322, 365, 412`).
- **Codec**: entirely in the server, `control_config/directory/document.rs`
  ("the only place in the workspace that turns `DirectoryRecords` into those
  bytes", `document.rs:7-17`); envelope
  `{ "schema": 1, "records": …, "compiled_under": … }` (`document.rs:21-23`).

### 4.3 Which shape each of the four maps is

Read against the two contracts, the four maps do not answer alike.

**Overlays, intents, outcomes are correlation-shaped, not document-shaped.**
All three are per-session (`store.rs:209-211`), all three are already swept by
age (`store.rs:236-250`), and all three are written on a *tool call* — a
per-request write, not an admin mutation. The document family has no expiry by
ruling (`store-redis/src/directory/scripts.rs:18-23`), so putting a swept
per-session map into it means implementing the sweep *inside the document*, i.e.
a compare-and-set rewrite of the whole deployment's row on every expiry — the
opposite of what one-key-per-binding bought
(`store-redis/src/correlation.rs:21-35`). It also puts a model-callable write
(`init_session` "is a write a model can call in a loop", `store.rs:46-47`) on the
same single key the admin plane compare-and-sets, where every such write
contends with every other node's admin mutation.

**Bindings are correlation-shaped and are nearly a fifth correlation map.**
`SessionBinding { principal, session, minted_at_ms }` keyed by an opaque minted
id is structurally `bind_call`/`session_of_call` with a different id source
(`store.rs:151-156` against `correlation.rs:266-283`), including the same
tenancy discipline — `ControlStore::binding` filters on principal *and* session
so a pasted foreign id is inert (`store.rs:462-484`).

**Costs of the correlation placement**, stated in the units the brief asks for:

- *Keys*: three or four new key shapes under `KeyFamily` — the enum is closed and
  a sixth variant is a compile-level change
  (`crates/roundhouse-store-redis/src/keys.rs:52-65`), each with its own
  `version()` arm (`keys.rs:104-112`). Natural shapes: `rh:v1:mcp:overlay:{<session>}`,
  `…:intent:{…}`, `…:binding:{<principal>}:<rhb id>`.
- *Scripts*: plausibly **zero**. None of the four writes has a condition that
  must not be evaluated against a value another node is replacing — overlays
  replace per axis, intents replace wholesale, outcomes replace wholesale
  ("a second report replaces the first", `store.rs:186-187`). Only
  `bind_session`'s idempotence is a read-then-write (`store.rs:443-449`), and it
  is a scan over the map rather than a keyed lookup — durably it wants a second
  key (`owner → binding id`) or a script, which is the one design choice here.
- *Contract assertions*: on the correlation family's precedent, ~8-12 in a
  `mcp_control_store_contract_suite!` — the two backends' bound enforcement is
  the thing that must be asserted about both (`correlation.rs:66-81`).
- *Signature churn*: **this is the real cost.** Every `ControlStore` method is
  sync today (`store.rs:264, 289, 301, 344, 361, 378, 395, 417, 435, 473, 501`),
  and both engine readers are sync too (`narrowed_admission` at
  `engine/control.rs:115`, `objective` at `:165`). Durability makes them async,
  exactly as M16.0's R-D1 made `DirectoryStore` async
  (`control_config/directory.rs:209`), which pushes an `.await` into `run_turn`'s
  hot path at `engine.rs:1352` and into the interjection seam at `engine.rs:1198`.
- *Serde that does not exist*: `SessionOverlay`, `TimedOverlay`, `ModeNarrowing`
  derive only `Debug, Clone, Default, PartialEq`
  (`crates/roundhouse-mcp/src/overlay.rs:149-153`, `:131-139`, `:90-95`) [fact-check 2026-09-04: `Default` is derived at `:149` (`SessionOverlay`) only; `ModeNarrowing` and `TimedOverlay` derive `Debug, Clone, PartialEq`; no serde on any of the three, as stated], and the
  resolved `allow` filter is a `TargetFilter`, which is **deliberately neither
  `Serialize` nor `Deserialize`** — "a `Deserialize` impl here is therefore not
  a convenience but a second door into the same room, and the one behind it
  produces the worse error — so it is gone, and this paragraph is here so it does
  not come back as an obvious-looking addition"
  (`crates/roundhouse-core/src/control/policy.rs:97-103`). There is also **no
  accessor that reads the patterns back out** (`policy.rs` `pub fn` list:
  `allow_all`, `parse`, `matches` only, `:142, 152, 217`). So a durable overlay
  must either store the pattern strings alongside and re-`parse` on load (needs
  an additive accessor) or re-resolve the mode against the *reader's* catalog —
  and the latter is explicitly forbidden: "an overlay is a *narrowing*, and a
  narrowing that silently grew to cover a model an operator added an hour later
  would be a widening with an agent-authored trigger"
  (`overlay.rs:33-38`).
- *A bound already written*: `AgedTable` is `pub` in a `pub mod`
  (`correlation.rs:355`, `control/mod.rs:73`), so the memory half of a durable
  overlay map can adopt the existing count+age table without a new type — it is
  reachable today as `roundhouse_core::control::correlation::AgedTable`,
  though it is not re-exported from `control` (`control/mod.rs:88-91`).

**Costs of the document placement** (a sibling document, per D2's "R16's
document contract can carry as sibling documents",
`agent-docs/PLAN-frontier-selection.md:592-594`):

- *Keys*: one new key (`rh:v1:<family>:records`), a sixth `KeyFamily` variant,
  and the note that a document family carries no hash tag deliberately
  (`store-redis/src/directory.rs:18-36`).
- *Scripts*: one `commit` per document — the `DocumentStore` trait is not
  parameterised by key, so a second document means either a second
  implementation or a constructor that takes the family
  (`store-redis/src/directory.rs:117-121` takes `KeyNamespace` only).
- *Contract assertions*: the existing 9 re-instantiated via
  `document_store_contract_suite!` (`directory/contract.rs:469`) — cheap, because
  the suite is already written and backend-agnostic.
- *The mismatch*: a compare-and-set over one key for a per-session, model-callable,
  expiring write is the wrong contract, for the reasons in §4.3 above.

---

## 5. The sealed credential store

### 5.1 What the plans say

- The BYOK section: "Secrets are sealed (XChaCha20-Poly1305 under a key from
  `ROUNDHOUSE_CONTROL_KEY`) in the control store — or, in the config-file phase,
  named by env var and never inlined. They reach the transport **on the quote**"
  (`agent-docs/PLAN-agentic-control-plane.md:393-395`), with the quote carrying
  "a redacting handle, resolved to plaintext only inside the client's `execute`"
  (`:400-401`).
- The M8 addendum: "The sealed store (XChaCha20-Poly1305 under
  `ROUNDHOUSE_CONTROL_KEY`) stays deferred; its unlock is the durable directory
  store above" (`PLAN-agentic-control-plane.md:1363-1365`), listed among
  "Still deferred, by name" beside MCP-overlay durability
  (`:1367-1371`).
- The 2026-09-03 addendum, after D2: "MCP-overlay durability and the sealed
  credential store gain a contract they can ride on and keep their own
  questions" (`PLAN-agentic-control-plane.md:1644-1646`).
- D2's own open list: both "R16's document contract can carry as sibling
  documents but which are separate rungs with separate questions — the overlay
  maps are per-session and swept, and a credential document needs the key it is
  sealed under" (`agent-docs/PLAN-frontier-selection.md:588-597`).
- The prior deep dive recorded the same two under the same unlock
  (`agent-docs/research/roundhouse-admin-directory-1b85d64.md:231`).

### 5.2 What exists in the tree

**The refusal route.** `POST /v1/admin/credentials` is registered
(`crates/roundhouse-server/src/admin_api.rs:141`) and exists purely to say the
capability is absent (`admin_api.rs:36-40`, `:729-746`). Two refusals: an
OAuth-shaped body is `400 oauth_credentials_unsupported` and is refused "on its
own terms, permanently as far as this milestone is concerned"
(`admin_api.rs:737-742`, message at `:756-762`); anything else is
`501 credential_crud_not_available` whose message names the mechanism that
remains authoritative and says "a sealed store this API could write into is
deferred" (`admin_api.rs:768-775`). OAuth shape is decided recursively on six
field spellings (`admin_api.rs:783-790`).

**The type that names the future arm.** `CredentialRef` has one variant,
`EnvVar { name }` (`crates/roundhouse-core/src/control/credential.rs:105-109`),
and the doc above it is the design record: "§3's sealed store —
XChaCha20-Poly1305 under a key from `ROUNDHOUSE_CONTROL_KEY` — needs three
things that do not exist yet: a control *store* to hold ciphertext (M8's admin
plane writes it), the key material, and a decrypt seam. An arm nothing can
construct and nothing can open is not a smaller version of that … Adding it is
additive — every match on this enum is inside this crate"
(`credential.rs:96-104`).

**The config half that does ship.** `CredentialsConfig` names an env var and
never a secret (`crates/roundhouse-server/src/control_config/credentials.rs:39-71`;
`ProviderCredentialConfig.env_var` is "**Not the secret**", `:60-61`).
`CredentialRef::env_var` makes the rule structural rather than conventional —
the alphabet `[A-Za-z_][A-Za-z0-9_]*` excludes a character every credential
format in circulation carries, "`sk-…` and `at-…` a hyphen, a JWT two dots, a
sealed blob base64's `+/=`" (`credential.rs:30-36`, `:125-140`, length bound at
`:116`). Resolution reads the variable **at boot** so an unset variable stops the
process rather than failing one tenant's turns
(`control_config/credentials.rs:113-119`, `std::env::var` at `:139`).

### 5.3 How a credential reaches a frontier dispatch today

1. **Compile.** `ControlPlaneConfig::compile` builds a `TurnCredentials` per key
   — `unrestricted()` where no tier declared a block, `configured(mode,
   deployment, project, user)` where one did
   (`control_config/config.rs:1170-1193`), stored on the `Admission`
   (`config.rs:1197-1204`). Not declaring the block anywhere leaves every quoted
   provider in the candidate set, which is what stops the milestone silently
   re-routing every existing workload to local (`config.rs:1160-1172`).
2. **Filter before `choose()`.** `admission.credentials.reachable(candidates)` at
   `crates/roundhouse-server/src/engine.rs:2217`, placed before routing for two
   stated reasons — the payer must be stampable on the `DecisionRecord`, and "a
   saving must never be priced against a model the caller could not reach"
   (`crates/roundhouse-core/src/control/credential/access.rs:6-27`). Local
   candidates need no credential, so a missing one degrades rather than fails
   (`access.rs:29-36`, `engine.rs:2214`).
3. **Resolve once per attempt.** `access_for(&target)` yields
   `ProviderAccess { credential: TurnCredential, payer: Payer }`
   (`engine.rs:2457-2460`; type at `access.rs:54-59`). `None` is unreachable
   because `reachable` already made the target unchoosable
   (`engine.rs:2449-2452`).
4. **Ride the quote.** The credential is a field on the quote because "this is
   the only argument `execute` receives" (`engine.rs:2824-2831`).
5. **Reveal inside the client.** The Anthropic client calls
   `credential.require_api_key(provider)`
   (`crates/roundhouse-fleet/src/anthropic_messages.rs:499`) and puts it on
   `x-api-key` bare or `Authorization: Bearer` per the route's auth style
   (`anthropic_messages.rs:505-511`; header constant at `:113`). `require_api_key`
   is the only path to plaintext for a stored secret
   (`crates/roundhouse-core/src/control/credential/secret.rs:275`).
6. **Redaction is a type property.** `Secret`'s `Debug`, `Display` and
   `Serialize` all render an eight-hex-character domain-separated fingerprint
   (`secret.rs:4-27`, domain at `:46`); it is not `Deserialize` and must not be
   ("a `Secret` that could be deserialized is a `Secret` that can appear inline
   in a control-plane file", `secret.rs:51-59`); it is not `PartialEq`
   (`secret.rs:61-67`); and its plaintext is **not** zeroed on drop, stated
   rather than approximated because doing it honestly needs `zeroize`
   (`secret.rs:69-74`).
7. **Pass-through is the other path and stores nothing.** `Secret::held` skips
   the API-key shape check because what pass-through forwards *is* an OAuth token
   by construction (`secret.rs:101-110`); the forwarded types are neither
   `Serialize` nor `Deserialize`, "and that is the point … a deserializable one
   is a credential that can arrive from a store"
   (`crates/roundhouse-core/src/control/credential/forwarded.rs:31-35`); the
   per-provider header allowlist is closed and fail-closed (`forwarded.rs:23-29`).

### 5.4 What sealing would need

- **Key material.** `ROUNDHOUSE_CONTROL_KEY` appears **nowhere in the code** —
  only in three plan lines and two doc comments
  (`PLAN-agentic-control-plane.md:394, 1364`, `credential.rs:97`,
  `PLAN-frontier-selection.md` via the deferral list). Established by
  `git grep -rn "ROUNDHOUSE_CONTROL_KEY" 1d016f2 -- crates agent-docs`.
- **An AEAD implementation.** There is **no AEAD, ChaCha, libsodium or `age`
  crate anywhere in the workspace**. Established by grepping every
  `Cargo.toml` at `1d016f2` for `chacha|aead|crypto_box|orion|dryoc|sodium|age =|rustcrypto`
  — zero hits. What is present is `getrandom` (32 bytes from the system CSPRNG
  for key minting), `sha2` and `hex` (control-plane key hashing), all declared
  with the note that they add "no new crate to the build, only a name the control
  plane depends on deliberately" (`Cargo.toml`, the `getrandom`/`hex` block
  above the `serde` line). XChaCha20-Poly1305 would be the **first genuinely new
  cryptographic dependency** in the tree, and by CLAUDE.md's Dynamo-parity habit
  it needs a pin story of its own.
- **Rotation.** Nothing exists: "key rotation" is on the still-deferred list
  (`PLAN-agentic-control-plane.md:1367-1371`), and the admin plane has "no audit
  trail, no key rotation" (`admin_api.rs:36-40`). A sealed store makes rotation
  load-bearing rather than optional — a re-key is a rewrite of every sealed blob,
  which under the document contract is one compare-and-set of the whole document.
- **What the plane holds in memory.** Today: plaintext `Secret`s inside the
  compiled `ControlPlane`, shared rather than owned precisely so an `Admission`
  clone per request does not copy "every plaintext secret on the heap per
  request" (`access.rs:88-99`). Sealing changes *where the plaintext comes from*,
  not whether it is resident: the decrypt seam would run at compile time and the
  compiled plane would hold the same `Secret`s. Note that the compiled plane is
  rebuilt on **every directory refresh**, not only at boot
  (`control_config/directory.rs:1094`, refresh cadence
  `DEFAULT_ADMISSION_CACHE_TTL_MS = 30_000`,
  `control_config/config.rs:815`) — so `credentials.rs:113-119`'s "at boot, not
  at first use" is true of the *file* half and understates the directory half,
  where `std::env::var` runs again on every recompile.
- **What the document would carry.** Ciphertext plus a nonce, a key id, and an
  AEAD tag. The one thing it must not carry is anything a `Secret` can be
  deserialized from — `Secret: !Deserialize` is the invariant
  (`secret.rs:51-59`), so a sealed arm needs a *different* deserializable type
  (the sealed blob) whose only exit is a decrypt function that calls
  `Secret::api_key`, preserving the OAuth refusal at the one constructor
  (`credential.rs:19-24`, `secret.rs:84-99`).

**The reference half already rides the document today, and this is the strongest
single fact for the ruling.** `ProjectRecord` embeds `ProjectEntry` verbatim —
"there is no second spelling of a project's policy for the two halves to disagree
in" (`control_config/directory/records.rs:124-133`) — and `ProjectEntry` carries
`credentials: Option<CredentialsConfig>` (`control_config/config.rs:114-122`),
which is `Serialize` (`credentials.rs:40-41`). `POST /v1/admin/projects` parses a
full `ProjectEntry` (`admin_api.rs:407`). So an admin can **already** write a
durable, deployment-wide credential *reference* naming an env var, and the
compile resolves it against each node's own environment
(`config.rs:1182-1187` → `credentials.rs:119-149`). What the sealed store adds is
not the reference but the *material* — and the material is exactly what the env
var scheme cannot distribute across nodes: a node whose environment lacks the
named variable fails the compile, keeps its last good plane and records a
refused version (`control_config/directory.rs:1094-1115`).

The one place credentials are deliberately *withheld* from directory records is
per-key: an admin-minted key compiles with `credentials: None` because "M8 has no
credential CRUD, so a member's own provider keys stay a thing only the file can
say" (`control_config/directory.rs:2067-2070`; the parallel `fair_use: None` note
at `:2060-2066`). `MembershipRecord` and `ApiKeyRecord` carry no credentials
field at all (`records.rs:195-227`, `:259-291`).

---

## 6. Can a sealed blob ride the directory document, or does it need its own key?

**Size: not the constraint.** The adapter refuses above
`DIRECTORY_DOCUMENT_CEILING_BYTES = 8 MiB`
(`control_config/directory/document.rs:112`, enforced at `:386-394`), sized
against "roughly 330 bytes per key record" and a real document of "hundreds of
kilobytes" (`document.rs:91-100`). A sealed API key is ~100 bytes of ciphertext
plus a 24-byte nonce and a 16-byte tag; even ten thousand of them is single-digit
megabytes at the *outside*, and the store contract already asserts a 3 MiB
document round-trips byte-exact
(`crates/roundhouse-core/src/control/directory/contract.rs:412-437`). The
ceiling is not what decides this.

**The fingerprint: `CompiledUnder` has no axis for a sealing key.** Its four
fields are `file_sha256`, `catalog`, `fleet`, `admission_cache_ttl_ms`
(`document.rs:128-148`), and `differs_from` reports exactly those four
(`document.rs:166-181`, `DivergentInput` at `:191-200`). Sealing adds a fifth
input a reader can disagree about — *which `ROUNDHOUSE_CONTROL_KEY` this node
holds* — and a node holding the wrong key cannot decrypt, which is not a
"warn and keep serving" divergence but a compile failure. Adding a key **id**
(never the key) as a fifth `#[serde(default)]` field is additive and would make
the disagreement nameable; every field is already defaulted precisely so a
document written by a build with a smaller fingerprint still loads
(`document.rs:123-127`).

**An older node reading a newer document: the answer depends on *where* the blob
goes, and the two answers are opposite.**

- *A new top-level envelope key*: tolerated. The envelope is deliberately **not**
  `deny_unknown_fields` — "a future build adding a fourth top-level key must not
  break the older half of a fleet during a rolling upgrade, and an envelope key
  an older build does not recognise is by construction something it does not
  need" (`document.rs:43-47`; struct at `:237-245`). An older node ignores the
  sealed section and compiles the tenancy it understands.
- *A new field inside a record*: fatal, deployment-wide. "The rows inside keep
  the file vocabulary's `deny_unknown_fields`, and that is a deliberate
  asymmetry" — a build that adds a field to a config entry "has changed what a
  stored document can contain, so it bumps `schema`, and an older node then
  refuses that document by name instead of quietly dropping the field"
  (`document.rs:49-58`). The refusal is total: `decode` probes `schema` first and
  returns `Unavailable` for anything above `DIRECTORY_DOCUMENT_SCHEMA = 1`
  (`document.rs:439-453`, constant at `:86`), which at boot stops the process and
  on a running node keeps the last good plane and records a refused version
  (`control_config/directory.rs:1094-1115`). So an in-record sealed field costs a
  rolling upgrade the whole directory, not just the credentials — the failure
  mode is "half the fleet is serving stale tenancy" over a credential change.

**Two further arguments the tree makes against one key:**

1. **Blast radius.** "This family has exactly one key, so a backend that got the
   CAS wrong would not lose one tenant's row, it would lose the deployment's
   tenancy" (`crates/roundhouse-core/src/control/directory.rs:74-77`). Adding
   credential writes to that key multiplies the write rate against the one
   compare-and-set whose loser is a lost revocation
   (`store-redis/src/directory/scripts.rs:8-16`).
2. **Rotation is a whole-document rewrite.** Re-keying every sealed blob under
   the shared document means one `commit` carrying the entire tenancy; under a
   separate key it is a rewrite of the credential document alone.

**What a separate document costs**, concretely: one `KeyFamily` variant
(`store-redis/src/keys.rs:52-65`) with its own `version()` arm (`:104-112`); one
more Lua `commit` or a `RedisDocumentStore` constructor parameterised by family
(today it takes only a namespace, `store-redis/src/directory.rs:117-121`); the
existing 9-assertion `document_store_contract_suite!` re-instantiated
(`directory/contract.rs:469`); and a second codec module beside
`control_config/directory/document.rs` with its own `schema` number, so a
credential-vocabulary change does not bump the *directory's* schema and refuse a
whole fleet's tenancy.

---

## 7. What Relay 0.8.2 does with credentials

**Pass-through, plus a process-env fallback, and no store.**

- The gateway authenticates the *wrapper* with a random per-invocation token and
  **consumes it before intercepts run**, "preserving a clear boundary between
  wrapper authentication and credentials intended for an upstream model"
  (0.8.2 registry, `nemo-relay-cli-0.8.2/src/provider_auth.rs:4-8`). The token is
  32 bytes from `ring`'s `SystemRandom`, prefixed `nrp_` (`provider_auth.rs:22`,
  `:29-39`), compared in constant time (`:94-107`, `:184-186`).
- Provider credentials are recognised on four headers —
  `authorization`, `x-api-key`, `api-key`, `anthropic-api-key`
  (`provider_auth.rs:23`, `:170-175`) — and their presence is tracked as a
  *disposition*, never captured:
  `SourceCredentialDisposition::{RelayProxyCredential { provider_credential_present }, ProviderCredential, Absent}`
  (`provider_auth.rs:127-132`). The enum holds a `bool`, not a value.
- Forwarding: if the inbound request already carries any of those four headers,
  Relay injects nothing and the caller's own credential goes upstream
  (`nemo-relay-cli-0.8.2/src/gateway/mod.rs:1073-1079`). Only when the request is
  unauthenticated *and* `allow_environment_provider_auth` is set does it read
  `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` from the process environment and put it
  on `Authorization: Bearer` or `x-api-key` respectively
  (`gateway/mod.rs:1083-1108`). The env lookup is injected as a closure
  (`gateway/mod.rs:1053`, `:1059-1068`) so it can be tested without mutating
  process env.
- `remove_provider_credentials` strips all four before any path that must not
  carry them (`provider_auth.rs:177-182`, called at `gateway/mod.rs:891`).
- The core crate keeps credentials out of observability: seven header names are
  removed from the event-only copy of every request
  (`nemo-relay-0.8.2/src/api/llm.rs:54`, `:587-592`, `:903-905`), and plugin
  destinations reference their own credentials **by env-var name** with a `_var`
  suffix, "so multiple destinations can each carry their own credentials without
  leaking" (`nemo-relay-0.8.2/src/observability/plugin_component.rs:541`,
  `:556-557`) — the same posture as roundhouse's `CredentialRef::EnvVar`.

**Negative:** nothing in Relay 0.8.2 persists a provider credential. Established
by `grep -rn -i "api_key|credential"` across `nemo-relay-0.8.2/src` and
`nemo-relay-cli-0.8.2/src`: every hit is a header name, a strip, a disposition,
an env-var *name*, or a constant-time comparison of Relay's own loopback token.
There is no sealing, no encryption at rest, and no credential store to model a
roundhouse one on.

---

## 8. The negatives, and how each was established

1. **No AEAD / sealing crate exists in the workspace.** Iterated every
   `Cargo.toml` at `1d016f2` (`git ls-tree -r --name-only 1d016f2 | grep Cargo.toml$`)
   and grepped each for `chacha|aead|crypto_box|orion|dryoc|sodium|age =|rustcrypto`:
   zero hits.
2. **`ROUNDHOUSE_CONTROL_KEY` appears in no code path.**
   `git grep -rn "ROUNDHOUSE_CONTROL_KEY|XChaCha|sealed" 1d016f2 -- crates agent-docs`
   returns two plan lines (`PLAN-agentic-control-plane.md:393-394`, `:1363-1364`),
   one core doc comment (`credential.rs:97`), two server refusal strings
   (`admin_api.rs:734`, `:773`), and prose. No variable read, no constant.
3. **`ControlStore` is not one of the shared-backend families.** Read the `use`
   list and doc table of `shared_backend.rs:37-58`; grepped `ControlStore`
   across `crates/roundhouse-server/src` — the only composition-root construction
   is `main.rs:911`, inside `serve`.
4. **`outcomes` and `binding_in_log` have no production reader.**
   `git grep -n "outcome_for|binding_in_log" 1d016f2 -- crates`: the only hits
   outside `roundhouse-mcp`'s own tests are the definitions and doc references.
5. **No `#[test]` anywhere asserts an overlay is lost across a restart.**
   `git grep -rn "restart" 1d016f2 -- crates/roundhouse-mcp crates/roundhouse-server/tests`
   returns only doc prose in the mcp crate and the *directory* tombstone tests
   (`crates/roundhouse-server/tests/admin_api.rs:1235`, `:1291`). The node-local
   loss is documented, never enforced.
6. **`ControlStore` has no capacity cap.** Read the whole of `sweep`
   (`store.rs:236-250`) and the file's constants (`store.rs:103`, `:110`,
   `:118`): the only bound is by age.
7. **No HTTP route exposes divergence, regression, or the refused version.**
   `git grep -n "last_regression|\.divergence(|refused_version" 1d016f2 -- crates`
   returns hits only inside `control_config/directory.rs` and its tests; the
   five routers' `.route(` registrations
   (`admin_api.rs:118-141`, `http.rs:141-146`, `mcp_api.rs:410`,
   `messages_api.rs:250-254`, `metrics_api.rs:75-76`, `relay_api.rs:90-92`,
   `responses_api.rs:215`) contain no status route. R19's served/refused pair
   is recorded (`directory.rs:1112`) and readable only from inside the process.
8. **`SessionOverlay` and `TargetFilter` cannot be serialized.** Derive lists at
   `overlay.rs:149`, `:131`, `:90` and `policy.rs:102`; the `TargetFilter`
   omission is deliberate and documented (`policy.rs:97-101`), and there is no
   accessor returning its patterns (`policy.rs` `pub fn` list).
9. **No directory record carries a secret.** Field lists at
   `records.rs:131-157, 171-175, 195-227, 259-291`: no credential field on
   `MembershipRecord` or `ApiKeyRecord`; admin-minted keys compile with
   `credentials: None` (`directory.rs:2067-2070`). `ProjectEntry.credentials`
   *is* stored, but it can only name an env var
   (`credentials.rs:59-71`, `credential.rs:125-140`).
10. **Relay 0.8.2 stores no provider credential.** §7, established by grepping
    both Relay crates' `src` trees for `api_key|credential`.

---

## 9. Open questions the evidence does not settle

1. **Does the overlay want durability at all, or a re-derivation?**
   `plane.rs:180-186` says a durable overlay breaks the write-time
   "leaves something routable" guarantee and names the fix site
   (`narrowed_admission`). Durability and that re-derivation are one rung, not
   two, and the evidence does not say which is the larger half.
2. **What the staleness bound for an overlay should be.** The correlation family
   derived 6h and 7d from what the bound is *about*
   (`correlation.rs:140-164`); `RETENTION_MS` is one day for four families
   chosen from the consequence in each direction (`store.rs:95-102`). A durable
   per-session overlay is arguably shorter than either, and nothing in the tree
   argues the number.
3. **Whether `bind_session`'s idempotence survives durability without a script.**
   Today it is a scan (`store.rs:435-449`) justified by the map being small and
   swept; durably it is either a second key or a compare-and-set, and the
   correlation family's precedent (one script, for the one condition that must
   not race, `store-redis/src/correlation.rs:61-76`) argues both ways depending
   on whether a duplicate `rhb_` id is a defect or a nuisance — and no production
   reader resolves one either way (§8.4).
4. **Whether the intent should be a log item rather than a store row.** M10.0
   moved the steer guidance into the session log precisely to stop a node-local
   second source of truth (`store.rs:176-182`). The intent has the same shape —
   a string an agent declared, read once per turn — and moving it would make its
   durability free, at the cost of the log carrying agent-authored text the
   validator then reads.
5. **Whether a sealed blob's key id belongs in `CompiledUnder`.** A fifth
   `#[serde(default)]` axis is additive (`document.rs:123-127`), but a node that
   cannot decrypt is a compile failure rather than a divergence to warn about,
   and R19's rule is "record, never refuse"
   (`PLAN-frontier-selection.md:571-586`) — the two do not obviously compose.
6. **How a sealed store distributes the sealing key itself.** The env-var scheme
   pushes exactly this problem down one level: `ROUNDHOUSE_CONTROL_KEY` has the
   same per-node-environment distribution as `ANTHROPIC_API_KEY` does today, so a
   sealed store buys durability of the *material* and buys nothing about how the
   node gets the one secret it still needs out of band.

---

## Fact-check (2026-09-04)

An independent re-derivation of every negative and every high-stakes claim above, from the primary sources at the pinned revision (roundhouse `1d016f2`; Relay at the 0.8.2 registry sources), by a second reader who did not write this document. Verdicts: 30 verified, 1 corrected, 0 unestablished.

Re-derived all 10 negatives and every high/medium-stakes claim from primary sources at roundhouse 1d016f2 and Relay 0.8.2 registry sources, read-only. All 10 negatives verified exactly against fresh greps. All 22 numbered claims verified — every cited file:line opened and matched the asserted content, including the deep call-chain trace for claim 18 (credentials.rs "at boot" doc understates directory.rs's per-refresh env-var re-read: traced resolve -> to_project/to_tier -> ControlPlaneConfig::validate -> directory.rs compile()'s `merged.validate(path)?` -> called every refresh via directory.rs:1094) and claim 2's engine.rs:1352/1198 spend-once-per-turn mechanism. One minor imprecision found, confined to a negative's own how-established narration (not the claims list): it attributes `Default` derive to overlay.rs:90 and :131 (ModeNarrowing, TimedOverlay) alongside :149 (SessionOverlay), but only :149 actually derives Default — the substantive "no serde" claim is unaffected. Full evidence written to overlay-and-credentials-factcheck.md.

Corrections, each also applied above as a dated bracketed note:

- **Negative: SessionOverlay/TimedOverlay/ModeNarrowing carry no serde derives; TargetFilter deliberately not Serialize/Deserialize with no pattern accessor** — Substance verified: overlay.rs:90,131,149 derive only Debug/Clone/(Default only at :149)/PartialEq, no serde; policy.rs:102 confirms TargetFilter has no Serialize/Deserialize and no accessor. Minor error in the negative's own how-established text: it says all three lines carry Default, but only :149 (SessionOverlay) does — :90 (ModeNarrowing) and :131 (TimedOverlay) do not derive Default. Does not affect any numbered claim.
