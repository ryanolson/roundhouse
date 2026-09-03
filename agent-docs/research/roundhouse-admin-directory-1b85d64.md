# The admin directory, and what a durable one has to be

*A read of roundhouse at `1b85d64` — `roundhouse-server`'s
`control_config::directory`, `roundhouse-store-redis`, `main.rs`, and the
M8 and D1 addenda in `agent-docs/`. Dated 2026-09-03.*

Evidence for D2's first question: **what does the admin directory store, how
does it change, what does a restart or a second node break, and what does a
durable `DirectoryStore` have to guarantee that the other three Redis
families do not?** Every claim carries a `file:line` at `1b85d64`; the
negatives name what was searched. The claim list, negatives and open questions
are the dive's, reproduced as returned and independently re-derived (see the
fact-check at the end); the connecting prose is the orchestrator's, because the
dive's own narrative file was overwritten by the checker's evidence before it
was copied here — the claims are the evidence, and they survived intact.

---

## 1. What the directory stores

**Four `Vec`s, each wrapping the file's own entry.** Every row the directory
stores is one of `ProjectRecord`, `UserRecord`, `MembershipRecord`,
`ApiKeyRecord` — each wrapping the config entry the file would carry rather
than restating its fields, so there is no separate budget row, fair-use row or
key-policy row. A project's budget and fair use are fields of its wrapped
`ProjectEntry`; a member's allocation and overrides are fields of
`MembershipRecord`; a member's fair-use window only ever arrives from the file
(`ApiKeyRecord::fair_use` is documented "Always `None` for an API-minted key
and for an admin key: there is no route that writes a member window").
— `control_config/directory/records.rs:290-296` (`DirectoryRecords`),
`:107-142` (`ProjectRecord`), `:144-156` (`UserRecord`), `:168-198`
(`MembershipRecord`), `:227-263` (`ApiKeyRecord`), `:6-9` and `:104-106`
(wraps rather than restates), `:249-255` (member fair use is file-only).

**A key row keeps a hash and a tail, and revocation is a tombstone.** An
`ApiKeyRecord` keeps only `key_sha256` and a four-character `display_tail` —
"There is no field the plaintext could go in" — and revocation is a
`revoked_at_ms` tombstone compiled into a named `KeyRefusal::Revoked` rather
than a row delete, so the compiled plane answers `revoked_key` rather than
`unknown_key`.
— `records.rs:221-263` (esp. `:223-226`, `:234`, `:248`);
`control_config/directory.rs:58-63` and `:1273-1290` (compile turns a revoked
row into `KeyRefusal::Revoked`).

## 2. How it changes after boot

**Nine mutations, ten routes, one order.** The admin plane mutates the
directory through nine `DirectoryMutation` arms carried by ten routes, and
every write follows one order: take the write mutex, `store.load()`, mutate a
clone, compile the whole would-be control plane, `store.commit(expected_version,
next)`, then swap this node's snapshot. The order is stated as load-bearing —
a store that took the write first would persist a configuration the deployment
refuses to start under, "the failure furthest in time from its cause". The
write mutex is held across read-validate-commit "so a single node never races
itself. With this, `StoreFailure::Concurrent` can only be another *node*."
— `control_config/directory/mutation.rs:170-210` (the nine arms);
`admin_api.rs:117-142` (the routes); `directory.rs:751-770` (`apply`),
`:737-750` (the order and the `DeleteMembership` cascade), `:325-330` (the
write mutex).

**Versioning and cross-row atomicity are already solved by the trait's
shape.** `VersionedRecords` pairs the records with a `u64` that starts at 0 and
increments per commit; `version()` is the cheap half of the refresh; and
`commit` replaces the entire `DirectoryRecords` — so `DeleteMembership`'s
cascade (remove an edge, revoke N keys) is one commit by construction. A
row-per-entity Redis layout would create an atomicity problem the trait does
not currently have. "A backend that cannot do compare-and-set cannot implement
this trait, which is the right requirement."
— `control_config/directory/store.rs:14-18`, `:55-70` (esp. `:49-54` on CAS,
`:59-62` whole-records commit, `:64-69` cheap version), `:72-76`;
`directory.rs:757-761`, `:996-1016` (the cascade, inside one `mutate`).

## 3. What a restart breaks: the tombstone hazard, mechanically

**`refuse_taken` reads the memory store, which every boot rebuilds.** The only
thing keeping a closed project id retired is `refuse_taken`, which deliberately
does not skip archived rows ("Re-creating a project under a closed project's id
would join two tenants' spend histories under one name") and reads
`records.project(id)` from the memory store — which `main.rs` constructs
unconditionally on every boot, before it has even read `ROUNDHOUSE_REDIS_URL`.
— `directory.rs:1088-1122` (esp. `:1105-1114` and the comment at
`:1106-1110`); `records.rs:305-307`; `main.rs:711-723` (the directory built)
and `:794-797` (`REDIS_VAR` first read, via `shared_backend::open`).

**The ledger rows that survive are keyed by bare ids.** Spend keys are
`rh:v1:spend:{<project_id>}:account` with a `member:<user_id>` field per
member plus `:holds` and `:watermarks`; fair use is
`rh:v1:fairuse:{<project_id>}:p` and `:m:<user_id>`; correlation call and
thread bindings are tagged by a length-prefixed `(project, user)` principal.
No generation, epoch or creation stamp is anywhere in a key, so a recreated id
re-joins all of them.
— `roundhouse-store-redis/src/spend.rs:15-19` and `:59-81`;
`fair_use.rs:11-14` and `:161-179`; `correlation.rs:15-19` and `:214`.

**`main.rs` already ships the honest signal.** Whenever a control-plane file
is configured and the Redis arm was chosen, boot warns that an archived
project's tombstone lives only in memory and that losing it lets the id be
recreated, "silently joining the new tenant to the old one's spend history in
the ledger that DID survive. The fix is a durable DirectoryStore, not yet
built". The flag it branches on, `control_plane_file_configured`, is
documented as needing to move "the day a durable `DirectoryStore` lands and
the `Some` arm below picks between stores".
— `main.rs:807-822` (the warning), `:703-710` (the flag must move with the
store).

**The failing test already exists, ignored.**
`recreating_an_archived_project_after_a_restart_inherits_its_spend` asserts
`committed_usd == 0.0` for the recreated project and that the budget document
carries no field disclosing the inherited history, with the ignore attribute
naming "a durable DirectoryStore replacing MemoryDirectoryStore" as its unlock
and promising it "goes green the day that store lands, with no change to the
test itself".
— `tests/admin_api.rs:1337-1399`; ignore text at `:1338-1352`; assertions at
`:1385-1398`.

*Accuracy nit, absorbed here rather than repeated:* that ignore attribute
cites `main.rs ~341-353` and `~396-427`, which at `1b85d64` are
`main.rs:711-723` and `:799-822`. The line numbers are stale; the claim they
describe is not. — `tests/admin_api.rs:1338-1347` against `main.rs:711-723`
and `:799-822`.

## 4. How a change propagates between nodes: a poll, and only a poll

`Managed::compiled` refreshes on two conditions together — elapsed ≥
`admission_cache_ttl_ms`, *and* the store's version moved — and the TTL comes
from the same file the keys are in, defaulting to 30 s. It is documented as
the staleness bound on a revocation, and the only one. A failed refresh still
stamps `refreshed_at_ms` as a deliberate backoff, at the stated price that a
revocation can take up to two TTLs instead of one; a refresh that loads but
does not compile warns and keeps serving the old plane.
— `directory.rs:578-621` (esp. `:546-554`, `:563-571`, `:597`), `:608-612`;
`control_config/config.rs:457-476` and `:804-814`.

## 5. The two placements, and what each costs

**Both are written down verbatim, and the choice was deferred rather than
guessed.** (A) The records move into `roundhouse-core` beside the session and
spend contracts, and `roundhouse-store-redis` implements `DirectoryStore` the
way it implements the other two — which contradicts `core/src/control/mod.rs`'s
standing note that a key record "will arrive next to the resolver, not here",
and so needs a dated amendment. (B) The implementation lands in
`roundhouse-server` over the Redis handle `main.rs` already opens, and the
records stay where the resolver is. M8's own addendum ruled *for* the
`control/mod.rs` note over an earlier §8 table that would have put the
directory in core.
— `directory.rs:81-99`; `core/src/control/mod.rs:24-29`;
`agent-docs/PLAN-agentic-control-plane.md:1257-1274`.

**Placement B has two costs the deferral note does not spell out.** The Redis
key machinery is crate-private to `roundhouse-store-redis` — `build_key` and
`KeyFamily` are `pub(crate)`, `connect_manager` is a private `async fn` — so
the server crate would either need them made public or would spell its own key
format, which is exactly the state R-S3 removed. And the Redis handle does not
exist when the directory is constructed: `main.rs` builds the directory at
line 711 and opens backends at 794-797, so B requires re-ordering a boot whose
directory construction *is* the boot check.
— `roundhouse-store-redis/src/keys.rs:53`, `:217`, `:8-19` (R-S3's
one-builder rule); `lib.rs:142-161`; `main.rs:711` vs `:794-797`, and
`:686-690`.

**Both placements inherit one constraint written as two changes that must land
together.** `DirectoryStore` is a synchronous trait called under the write lock
alongside a full `compile()`, so a durable store needs the trait to become
async *and* the refresh path to stop compiling under the guard — landing the
first without the second "would durable-back the store and then hold every
concurrent admission behind one Redis round-trip on every TTL-driven refresh".
The refresh path is `Managed::compiled`, which takes the write guard at `:596`
and holds it across `store.load()` at `:601` and `compile()` at `:602`.
— `directory.rs:101-112` (the constraint), `:578-621` (the path), `:573-577`
(the trade that inverts once `load()` is a network call);
`control_config/directory/store.rs:55-70` (three synchronous methods).

**The records have half the serde they need.** Across `control_config` the
only `Serialize` is on `FairUseConfig` and `FairUseWindowConfig`, whose own doc
says it is the exception; `ProjectEntry`, `UserEntry`, `KeyEntry`,
`PolicyConfig`, `BudgetConfig`, `AllocationConfig`, `ValidateConfig`,
`CredentialsConfig` and `ControlPlaneConfig` all derive `Deserialize` only,
because a file is read and never written.
— `directory.rs:96-99`; `fair_use.rs:32-41`, `:42`, `:54`; `config.rs:69`,
`:189`, `:202`, `:254`, `:390`; `budget.rs:82`, `:90`, `:169`;
`validate.rs:46`, `:115`; `credentials.rs:40`, `:57`.

**What the store crate's other four families give a fifth for free.** One key
builder producing `<namespace>:<version>:<family>[:<part>]`; a closed
`KeyFamily` enum so a typo does not compile; a per-family schema version so one
family's v2 does not orphan the other four; a `KeyNamespace` validated once at
construction against blank, brace, colon and whitespace; one `connect_manager`
carrying the measured ~2 s outage bound; the `__contract_suite` recursion
behind all four family suites; and `shared_backend::open`'s single `match`
that the boot suites actually call.
— `keys.rs:207-229`, `:52-58` and `:74-82`, `:84-100`, `:103-205`;
`lib.rs:142-161` and `:91-140`; `core/src/contract_macro.rs:38-56`;
`roundhouse-server/src/shared_backend.rs:153-176` and the `open` match.

## 6. What a durable directory must guarantee that no other family does

**A compiled plane every node agrees on, whose *other* input is not in the
store.** The store holds admin-created rows only; file-owned rows are
projected from the file on every read; and the file, the `CrossChecks` built
from this process's catalog and fleet, and the TTL are all per-process. So a
durable store makes two states reachable for the first time: two nodes
compiling different planes from identical records with nothing comparing
their files, and a mutation validated on node A being un-compilable on node B,
where B warns once per TTL and keeps serving the old plane forever.
— `directory.rs:41-45`, `:657-735` (the file's projection), `:213-245` and
`:513` (`ConfigIdentities` per process), `:314-323` (file, path, checks, TTL
on `Managed`), `:608-612` (the warn-and-serve path); `main.rs:657-684`
(`CrossChecks` from this process's catalog and fleet); `crosscheck.rs:65-82`
(`refuse_policies_that_admit_nothing`, the check most likely to diverge).

**The file/store relationship is already ruled, and it is neither "file as
seed" nor "store as truth".** Reconciliation at read time was considered and
rejected by name ("a rule somebody has to write down, and every rule of that
shape has a case it gets wrong quietly"); admin entities are expressed in the
file's own vocabulary and concatenated into one `ControlPlaneConfig` compiled
by the one `validate`; every row has exactly one owner; and the file is never
copied into the store, so an edit between restarts is authoritative on the
next boot. Two properties a durable store must not break: bootstrap stays
file-only (Open mode refuses every admin write with
`admin_requires_control_plane`), and API lockout stays impossible — both rest
on file-declared admin keys being unrevocable through the API.
— `directory.rs:6-45` (esp. `:10-13`, `:15-23`, `:25-39`, `:41-45`), `:47-56`,
`:1049-1061` (a file-declared hash is refused `ConfigOwned`);
`admin_api.rs:192-195`; `PLAN-agentic-control-plane.md:1292-1298`.

## 7. What M8 deferred to this unlock, by name

M8's addendum defers three things whose unlock is this store: the Redis
`DirectoryStore` itself, with the placement "decided then"; MCP-overlay
durability (`roundhouse_mcp::ControlStore`'s maps, "deferred to the same
unlock"); and the sealed credential store under `ROUNDHOUSE_CONTROL_KEY`,
whose "unlock is the durable directory store above". Separately listed as
still deferred and *not* unlocked by it: admin audit trail, key rotation,
per-key rate limiting, pagination, rate-card editing, un-archive.
— `PLAN-agentic-control-plane.md:1268-1274`, `:1292-1294`, `:1355-1361`,
`:1363-1367`; `PLAN-frontier-selection.md:477-485` (D1's R15).

## 8. Negatives — what nothing does

1. **Nothing implements `DirectoryStore` durably.** `git grep -n "impl
   DirectoryStore\|dyn DirectoryStore\|DirectoryStore for" 1b85d64 -- crates`
   returns only in-process implementations: `MemoryDirectoryStore`
   (`directory/store.rs:88`), the `WriteBetweenReads` test fake
   (`directory/tests.rs:1597`) and `ArmedStore` (`tests/admin_api.rs:817`).
   `git ls-tree -r --name-only 1b85d64 | grep store-redis` lists
   `correlation.rs`, `fair_use.rs`, `keys.rs`, `lib.rs`, `scripts.rs`,
   `spend.rs`, `test_support.rs` and no directory module.
2. **Nothing serializes a directory record.** `git grep -n "Serialize" 1b85d64
   -- crates/roundhouse-server/src/control_config` matches only `fair_use.rs`
   (`:24`, `:32`, `:42`, `:54`) and the prose at `directory.rs:96`; the derive
   lines of every wrapped entry were read individually (§5).
3. **Nothing watches for a directory change.** `git grep -rn
   "pubsub\|PubSub\|SUBSCRIBE\|psubscribe\|notify_keyspace" 1b85d64 -- crates`
   returns nothing. The only propagation is the TTL-plus-version poll (§4).
4. **There is no `DirectoryStore` contract suite.** `git grep -n
   "macro_rules! .*contract_suite" 1b85d64 -- crates` yields exactly four family
   macros — store (`core/src/store/contract.rs:495`), spend (`:885` of its
   contract), fair use (`:608`), correlation (`:512`) — plus the shared
   `__contract_suite` plumbing (`core/src/contract_macro.rs:38`). The
   directory's tests are `#[cfg(test)]` unit tests against a concrete
   `ControlDirectory` (`directory/tests.rs:80-87`).
5. **No shared Redis key carries a project generation, epoch or creation
   stamp.** Every key builder in the crate was read (`spend.rs:59-81`,
   `fair_use.rs:161-179`, `correlation.rs:155-189` with the principal tag at
   `:214`, and the module-doc key tables at `spend.rs:15-19`,
   `fair_use.rs:11-14`, `correlation.rs:15-19`, `lib.rs:10-14`). Each takes
   only ids. `created_at_ms` exists only on the directory rows
   (`records.rs:117`), which are what does not survive.
6. **No route un-archives a project, and no route deletes one.**
   `DirectoryMutation` has exactly nine arms (`mutation.rs:170-210`);
   `admin_router` mounts ten routes, `DELETE /projects/{id}` bound to
   `archive_project` (`admin_api.rs:117-142`, `:493-501`). `records.rs:127-130`
   states the omission is deliberate: un-archiving "has to decide what happens
   to the keys that were refused while the project was closed".
7. **No admin write is attributed to anyone.** `records.rs:107-263` read in
   full: `provenance` (`Config` | `Admin`) and timestamps, no principal, key id
   or actor. `PLAN-agentic-control-plane.md:1363-1365` lists the audit trail
   as still deferred because `KeyScope::Admin` deliberately carries no
   identity.
8. **Nothing compares two nodes' control-plane files.** `ConfigIdentities` is
   derived per process from the local file (`directory.rs:224-244`, called
   once at `:513`) and holds only id and hash sets; neither `DirectoryRecords`
   (`records.rs:290-296`) nor `VersionedRecords` (`store.rs:14-18`) has a field
   that could carry a fingerprint, and the store is documented as holding
   admin-created rows only (`directory.rs:41-45`).
9. **`main.rs` never chooses between directory stores.** `main.rs:711-723`:
   the `Some(file)` arm always passes `Arc::new(MemoryDirectoryStore::new())`
   and the `None` arm is `ControlDirectory::open()`. One arm, unconditional;
   the comment at `:703-710` says the flag must move the day that changes.

## 9. Open questions, left for the ruling

1. Whether the durable directory becomes a fifth `KeyFamily` in
   `roundhouse-store-redis` (`keys.rs:52-58`) or a key space of its own; if
   the fifth, whether `build_key` and `KeyFamily` are made `pub` (they are
   `pub(crate)` at `keys.rs:53` and `:217`) or the records move to core so the
   implementation can live inside the store crate.
2. Whether the boot re-orders so backends open before the directory compiles
   (`main.rs:711` vs `:794-797`), and what a Redis that is up for the session
   store but refuses the directory read should do to a boot whose directory
   construction *is* the boot check (`main.rs:686-690`).
3. Whether node-divergent compile failure stays a per-node `tracing::warn`
   that keeps serving the last good plane (`directory.rs:608-612`) or becomes
   something an operator can see; no surface today reports one node's compile
   failure, and today the state is unreachable because no two processes share
   records.
4. Whether the per-node inputs outside the store — the
   `ROUNDHOUSE_CONTROL_PLANE` file, the `CrossChecks` built from this process's
   catalog and fleet, and the TTL — gain a fingerprint in the store, and
   whether a mismatch refuses the node's boot or only warns.
5. Whether the async-trait plus lock-span change (`directory.rs:101-112`)
   lands as its own rung before any Redis implementation, since it is a
   behaviour-preserving refactor of `Managed::compiled` that
   `MemoryDirectoryStore` can be judged under first.
6. Whether the durable store changes anything about the
   `ProjectRecord::archived_at_ms` terminal-archive ruling
   (`records.rs:127-130`): once tombstones survive, un-archive becomes a
   question an operator will actually ask, and its deferral reason was that the
   keys-refused-while-closed question had no obviously right answer.

---

## Fact-check (2026-09-03)

An independent re-derivation of every negative and every high-stakes claim
above, from the primary sources at `1b85d64` (`git show 1b85d64:<path>` and
`git grep … 1b85d64 -- crates`, never the draft under review), by a second
reader who did not write this document. Verdicts: 26 verified, 0 corrected, 0
unestablished.

Independently re-derived every negative and every high-stakes claim in the
durable-directory draft against roundhouse@1b85d64, the pinned
roundhouse-store-redis tree, and the pinned agent-docs files. All checked out
exactly, including precise file:line citations (structs, trait signatures, doc
comments quoted verbatim, boot ordering, test assertions, and PLAN-doc line
ranges). Zero corrections and zero unestablished claims found. The draft's own
self-flagged accuracy nit (stale line numbers in the R8 ignored test's
attribute text: cites main.rs ~341-353/~396-427, actual is 711-723/799-822)
is confirmed accurate as described.

Two re-derivations worth keeping beside the claims they confirm:

- **The write order, verbatim.** `apply` (`directory.rs:751-770`) reads
  exactly: `let _write = self.write.lock()…; let loaded = self.store.load()?;
  let mut next = loaded.records.clone(); self.mutate(&mut next, mutation,
  now_ms)?; let plane = self.compile(&next)?; let version =
  self.store.commit(loaded.version, next.clone())?;` then the snapshot swap.
- **The boot ordering.** `REDIS_VAR` is confirmed `"ROUNDHOUSE_REDIS_URL"` at
  `shared_backend.rs:59`, first read at `main.rs:794-797` inside
  `shared_backend::open`, after the directory is constructed at `:711-723`;
  the warning body at `:807-822` sits inside the `Backends::Shared` arm
  reached from `:799`.
