// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What an operator created over the API, and how it composes with the file.
//!
//! # Config before CRUD, and one compiler
//!
//! A deployment has two ways to say who may spend what: the
//! `ROUNDHOUSE_CONTROL_PLANE` file, and — from this milestone — `POST
//! /v1/admin/...`. The obvious failure is that they disagree, and the obvious
//! defence (reconcile them at read time) is the wrong one: reconciliation is a
//! rule somebody has to write down, and every rule of that shape has a case it
//! gets wrong quietly.
//!
//! What is done instead is that **admin-created entities are expressed in the
//! same vocabulary the file is** — [`ProjectEntry`], [`UserEntry`],
//! [`KeyEntry`] — and the two halves are concatenated into one
//! [`ControlPlaneConfig`] and compiled by
//! [`ControlPlaneConfig::validate`], the boundary that has judged every
//! boot-loaded key since M2. A runtime-minted key is therefore not *similar
//! to* a configured one, it is one. There is no second compiler for the two to
//! drift apart in, and a policy an operator could not write in the file is a
//! policy they cannot `POST` either.
//!
//! # One owner per entity
//!
//! Provenance is not decoration. Every row is owned either by the file
//! ([`Provenance::Config`]) or by the API ([`Provenance::Admin`]), and:
//!
//! - the API may **create** entities that *reference* file-owned ones — a
//!   membership in a configured project, a user in a configured deployment;
//! - the API may **never mutate** a file-owned entity. Every such attempt is
//!   refused naming `ROUNDHOUSE_CONTROL_PLANE`, because the remedy is to edit
//!   the file, and a surface that quietly shadowed the file would make the next
//!   restart a silent rollback;
//! - an API create whose identity collides with a file-owned one — a project
//!   id, a user id, a key hash, a `(project, user)` membership pair — is
//!   refused for the same reason: two owners is exactly the state the rule
//!   exists to prevent.
//!
//! It follows that the *store* below holds admin-created rows only. File-owned
//! rows are projected from the file itself, on every read, by
//! [`ControlDirectory::view`] — so a file edited between restarts is
//! authoritative on the next boot rather than fighting a stale copy of itself
//! in a database.
//!
//! It also follows that **API lockout is impossible**, and the argument is worth
//! stating exactly rather than loosely. A file-declared admin key cannot be
//! revoked here, so a deployment whose file declares one always retains it. A
//! deployment whose file declares *none* is not the exception it looks like:
//! `admin_keys` is optional, such a file loads, and `ControlPlane::scope` can
//! then never yield `KeyScope::Admin` — so there is no admin plane to reach,
//! nothing to mint the first API-owned admin key with, and no lockout to
//! suffer. Either way no "refuse to revoke the last key" special case is
//! needed, and adding one would be guarding a state no sequence of calls
//! reaches.
//!
//! # Revocation, staleness, and the two clocks
//!
//! Revocation is a tombstone and never a delete: the row keeps its hash and
//! gains a `revoked_at_ms`, and the compiled plane refuses that hash by name —
//! `revoked_key`, not `unknown_key`. See [`AuthError::RevokedKey`] for why the
//! distinction is the point of keeping the row.
//!
//! A write recompiles and swaps this node's snapshot immediately, so on the
//! node that performed it a revocation is effective on the next request. Any
//! *other* node is serving a snapshot compiled before the write, and
//! [`ControlDirectory::plane`] bounds how long it may: after
//! `admission_cache_ttl_ms` it re-reads the store's version, and recompiles if
//! it moved. That bound is written in the same file the keys are, because it is
//! the operator's choice of how long a leaked key survives its own revocation.
//!
//! # What is deferred, and what would unblock it
//!
//! [`MemoryDirectoryStore`] is the only backing store in this milestone, which
//! means admin-created tenancy dies with the process and a two-node deployment
//! has two directories that never converge. That is honest for M8, whose admin
//! plane is a single-node surface, and it is exactly the shape of the M2 choice
//! between [`MemoryStore`](roundhouse_core::store::MemoryStore) and Redis.
//!
//! **The unlock condition, so the next person does not have to re-derive it:**
//! a durable store is wanted the moment admin-created tenancy has to outlive a
//! restart or be seen by a second node. D2 ruled the placement: the
//! implementation lands in this crate over the Redis handle `main.rs` already
//! opens, and the records stay next to the resolver — so
//! `core/src/control/mod.rs`'s standing note that a key record "will arrive
//! next to the resolver, not here" stands rather than needing an amendment.
//!
//! # What M16.0 landed, and what M16.1 still owes (2026-09-03)
//!
//! The constraint this doc used to state as a warning has been discharged.
//! [`DirectoryStore`] *was* a synchronous trait called under `current`'s write
//! lock alongside a full `compile()` — fine while `load()` was
//! [`MemoryDirectoryStore`]'s in-memory clone, a real stall once it is a
//! network round trip — and the warning was that a durable store needs two
//! changes together, not one. Both landed here, in this rung, before any
//! durable store exists to blame them on:
//!
//! - **the trait is async** (R-D1). `load`, `commit` and `version` are
//!   `async fn` behind `#[async_trait]`, `PlaneSource::plane` is async with
//!   them, and every surface awaits its plane. `current` is still a `std`
//!   lock and is never held across an await — which the compiler enforces,
//!   because a `std` guard is not `Send` and a [`PlaneSource`] future must be.
//!   The write mutex, which *is* held across the store's `load` and `commit`,
//!   is a `tokio` one.
//! - **the refresh runs outside every lock** (R-D2, R-D3). See
//!   [`Managed::compiled`]: three brief windows, the `refreshed_at_ms` stamp as
//!   the single-flight token, publication conditional on the loaded version
//!   being newer, and one uniform TTL of backoff behind every kind of refresh
//!   failure.
//!
//! **What M16.1 owes.** The seam is ready and the store behind it is still
//! [`MemoryDirectoryStore`], so nothing about durability has changed yet:
//! admin-created tenancy still dies with the process, and
//! `recreating_an_archived_project_after_a_restart_inherits_its_spend` in
//! `tests/admin_api.rs` is still ignored with its reason still true. Three
//! things are left, and they are the whole of it:
//!
//! - **the Redis store itself**, over `roundhouse-store-redis`'s one key
//!   builder rather than a key format spelled a second time in this crate —
//!   which means `build_key` and `KeyFamily` become reachable from here, or the
//!   implementation moves to where they already are;
//! - **`Serialize` on the records**, which today they have only half of: the
//!   config entries they wrap derive `Deserialize` because a file is read and
//!   never written. Mechanical, and small;
//! - **the boot re-order.** `main.rs` builds this directory before it opens any
//!   backend, and the directory's construction *is* the boot check — so the
//!   `Some` arm that picks between stores has to come after the backend
//!   handle exists, and `control_plane_file_configured` (the flag the
//!   memory-store warning branches on) has to move with it.
//!
//! [`AuthError::RevokedKey`]: super::AuthError::RevokedKey

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use roundhouse_core::control::BudgetTerms;

use super::budget::budget_terms;
use super::config::{
    ControlPlaneConfig, DEFAULT_ADMISSION_CACHE_TTL_MS, KeyEntry, key_entry_label,
    project_entry_label,
};
use super::crosscheck::CrossChecks;
use super::{ControlPlane, KeyKind, KeyRefusal, MintedKey, mint_key};

pub mod mutation;
pub mod records;
pub mod store;

pub use mutation::{DirectoryError, DirectoryMutation, KeyFingerprint, ProjectPatch};
pub use records::{
    ApiKeyRecord, DirectoryRecords, DirectoryView, EntityKind, KeyRecordScope, MembershipRecord,
    MembershipRole, ProjectRecord, Provenance, UserRecord, key_id,
};
pub use store::{DirectoryStore, MemoryDirectoryStore, StoreFailure, VersionedRecords};

// ---------------------------------------------------------------------------
// Where a surface gets its plane
// ---------------------------------------------------------------------------

/// Where a surface gets its compiled plane, once per request.
///
/// **The one seam between "who may do this" and "when was that decided".** Every
/// router takes an `Arc<dyn PlaneSource>` and asks it at the top of each
/// handler, rather than capturing an [`Arc<ControlPlane>`] at mount time. That
/// is the whole mechanism by which a key revoked over the admin plane stops
/// serving turns: a plane is a value compiled at one instant, and a router
/// holding one would go on honouring it for the life of the process.
///
/// A trait rather than the concrete [`ControlDirectory`] because there is a
/// second, deliberately weaker implementation — see the one on [`ControlPlane`]
/// itself, which is compiled only under the `test-support` feature. Keeping the
/// weak one behind a feature is what makes "this call site silently lost
/// revocation" a build error in production rather than a property nobody
/// notices: a bare plane handed to a router in `main.rs` does not compile.
#[async_trait]
pub trait PlaneSource: Send + Sync + 'static {
    /// The plane this request is judged against.
    ///
    /// `now_ms` is the caller's clock rather than one read inside, for the
    /// reason every other seam in this crate takes it: a staleness bound that
    /// cannot be moved from a test is a staleness bound nothing pins.
    ///
    /// `async` since M16.0 (R-D1), because the refresh behind it may be a
    /// round trip to a durable [`DirectoryStore`]. `#[async_trait]` for the
    /// reason that trait gives: every surface holds this as
    /// `Arc<dyn PlaneSource>`, and a native `async fn` is not dyn compatible.
    async fn plane(&self, now_ms: u64) -> Arc<ControlPlane>;
}

#[async_trait]
impl PlaneSource for ControlDirectory {
    /// The live implementation, and production's only one.
    async fn plane(&self, now_ms: u64) -> Arc<ControlPlane> {
        ControlDirectory::plane(self, now_ms).await
    }
}

/// A plane that is its own source, and therefore never changes.
///
/// **Test support only, and the feature gate is the point.** This answers the
/// same value at every instant, so a surface mounted over it can never see a
/// revocation, a new project, or a raised limit — which is exactly what every
/// integration suite written before the admin plane means by "the control plane
/// of this deployment", and exactly what a production composition root must not
/// be able to reach for by accident. Compiled under `test-support`, which the
/// crate turns on for its own dev builds and for nothing else, so a `main.rs`
/// that passed a bare plane fails to compile rather than quietly serving a
/// snapshot forever.
///
/// A deployment that genuinely has nothing to administer — no
/// `ROUNDHOUSE_CONTROL_PLANE`, so no root of trust and no admin plane — is not
/// this: it gets [`ControlDirectory::open`], which is a real directory and
/// answers the admin surface's mode question as itself.
///
/// It also clones the whole plane per request, which is a second reason it does
/// not belong in a shipped binary and a non-reason in a fixture: the trait hands
/// back an owned `Arc` because the live implementation mints a fresh one on
/// refresh, and a value that is its own source has nothing to hand back but a
/// copy.
#[cfg(feature = "test-support")]
#[async_trait]
impl PlaneSource for ControlPlane {
    async fn plane(&self, _now_ms: u64) -> Arc<ControlPlane> {
        Arc::new(self.clone())
    }
}

// ---------------------------------------------------------------------------
// What the file owns
// ---------------------------------------------------------------------------

/// The identities `ROUNDHOUSE_CONTROL_PLANE` declares.
///
/// Resolved once, at construction: the file is read at boot and cannot change
/// under a running process, so re-deriving this per write would be work with no
/// question attached. It is what every provenance check is asked.
#[derive(Debug, Default)]
struct ConfigIdentities {
    projects: HashSet<String>,
    users: HashSet<String>,
    /// `(project, user)` for every membership the file's `keys` array implies.
    memberships: HashSet<(String, String)>,
    /// Every hash the file declares, turn and admin alike.
    hashes: HashSet<String>,
}

impl ConfigIdentities {
    fn of(config: &ControlPlaneConfig) -> Self {
        Self {
            projects: config
                .projects
                .iter()
                .map(|project| project.id.clone())
                .collect(),
            users: config.users.iter().map(|user| user.id.clone()).collect(),
            memberships: config
                .keys
                .iter()
                .map(|key| (key.project.clone(), key.user.clone()))
                .collect(),
            hashes: config
                .keys
                .iter()
                .map(|key| key.key_sha256.clone())
                .chain(config.admin_keys.iter().cloned())
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// The directory
// ---------------------------------------------------------------------------

/// One node's compiled answer, and what it was compiled from.
struct Compiled {
    version: u64,
    records: Arc<DirectoryRecords>,
    plane: Arc<ControlPlane>,
    /// When this node last confirmed its snapshot against the store — not when
    /// it last recompiled. A confirmed-unchanged snapshot is as fresh as a
    /// rebuilt one, and treating it otherwise would recompile a quiet
    /// deployment once per TTL forever.
    refreshed_at_ms: u64,
}

/// The file, the API's records, and the compiled plane the two produce.
///
/// **Not named `ControlStore`.** [`roundhouse_mcp::ControlStore`] already
/// exists, holds a completely different thing (an agent's per-session overlay),
/// and is composed into the same process a few lines from where this is —
/// two types of that name in one composition root is a confusion nobody
/// deserves.
///
/// In this crate rather than in `roundhouse-core`, honoring the standing note
/// in `core/src/control/mod.rs`: a key record is a fact about *credentials*,
/// core deliberately knows nothing about credentials, and the record belongs
/// next to the resolver that turns a secret into an identity. That resolver is
/// [`ControlPlane`], one module up.
///
/// **Every surface holds one of these rather than an [`Arc<ControlPlane>`]**,
/// including the surfaces of a deployment that has no admin plane at all. A
/// plane is a value compiled once; a revocation has to reach the *turn*
/// surfaces, which is the entire point of revoking a key, and it can only do
/// that if what a handler resolves against is a directory it re-asks per
/// request. Two wirings — planes here, a directory there — would be a
/// deployment where a revoked key kept serving on whichever surface had not
/// been converted.
pub struct ControlDirectory {
    backing: Backing,
}

/// Whether this directory has anything to administer.
///
/// The split exists because [`ControlPlane::Open`] is a real deployment: it has
/// no file, so there is no root of trust an admin key could have been issued
/// from (see [`AuthError::AdminRequiresControlPlane`]), nothing for a store to
/// hold, and no compile to run. Rather than making the composition root wire
/// two shapes, an open deployment gets a directory whose answer never changes.
///
/// [`AuthError::AdminRequiresControlPlane`]: super::AuthError::AdminRequiresControlPlane
enum Backing {
    /// A plane nothing here can change.
    Fixed(Arc<ControlPlane>),
    /// A file, a store, and the snapshot the two compile to.
    ///
    /// Boxed so the enum stays pointer-sized: a fixed directory is one `Arc` and
    /// a managed one is a whole config, a store handle and two locks. Every
    /// surface holds this behind an `Arc` already, so the extra indirection is
    /// one that no request path can notice.
    Managed(Box<Managed>),
}

/// The managed half: what [`ControlDirectory`]'s doc describes.
struct Managed {
    /// The file's entries, kept whole so a merge is a clone-and-extend rather
    /// than a reconstruction. Its `turn_keys` table is rebuilt by every
    /// compile and is never read from this copy.
    file: ControlPlaneConfig,
    /// What a compile failure names. The path from `ROUNDHOUSE_CONTROL_PLANE`
    /// on a real deployment, so a refused `PATCH` and a refused boot point at
    /// the same document.
    path: String,
    config: ConfigIdentities,
    store: Arc<dyn DirectoryStore>,
    checks: CrossChecks,
    ttl_ms: u64,
    /// A `std` lock, and deliberately still one after M16.0 made the refresh
    /// async: nothing here is ever held across an await, which is a property
    /// the compiler checks rather than one a reader has to trust. A `std`
    /// guard is not `Send`, a [`PlaneSource`] future must be, so a refresh that
    /// tried to hold this across `load` would not compile — see
    /// [`Self::compiled`] for the three windows it is taken in.
    current: RwLock<Compiled>,
    /// Held across read-validate-commit, so a single node never races itself.
    ///
    /// With this, [`StoreFailure::Concurrent`] can only be another *node*, which
    /// is what makes it a meaningful answer rather than a lock this process
    /// forgot to take.
    ///
    /// A `tokio` mutex since M16.0 (R-D1): the span it guards now contains two
    /// awaits — the store's `load` and its `commit` — and a `std` guard held
    /// across an await parks a runtime worker on a lock a task, not a thread,
    /// is waiting for.
    write: tokio::sync::Mutex<()>,
}

impl ControlDirectory {
    /// Compile the file and whatever the store already holds.
    ///
    /// Fails if the two together do not compile — which, on a fresh
    /// [`MemoryDirectoryStore`], can only mean the file itself does not, and
    /// that has already stopped the boot by the time this is called.
    pub async fn new(
        file: ControlPlaneConfig,
        path: impl Into<String>,
        store: Arc<dyn DirectoryStore>,
        checks: CrossChecks,
        now_ms: u64,
    ) -> Result<Self, DirectoryError> {
        Ok(Self {
            backing: Backing::Managed(Box::new(
                Managed::new(file, path, store, checks, now_ms).await?,
            )),
        })
    }

    /// A directory over a plane nothing can change.
    ///
    /// **Two callers, and the second is why this is `pub`.** The composition
    /// root builds one for a deployment that set no `ROUNDHOUSE_CONTROL_PLANE`
    /// — see [`Backing`]. Every integration suite that is *not* about the admin
    /// plane builds one too, and the alternative there is worse than verbose:
    /// a managed directory needs [`CrossChecks`], which means each of those
    /// suites would have to quote a candidate list, and their fixtures would
    /// then start failing `refuse_policies_that_admit_nothing` over policies
    /// that were never the subject of the test. A fixed directory keeps a suite
    /// in exactly the checks it was written against.
    pub fn fixed(plane: ControlPlane) -> Arc<Self> {
        Arc::new(Self {
            backing: Backing::Fixed(Arc::new(plane)),
        })
    }

    /// The directory an unconfigured deployment runs on: [`ControlPlane::Open`]
    /// and no admin plane.
    pub fn open() -> Arc<Self> {
        Self::fixed(ControlPlane::Open)
    }

    /// The plane every surface authenticates against, refreshed if it is due.
    ///
    /// See [`Managed::compiled`] for the refresh rule. A fixed directory answers
    /// the one plane it was built with, and the clock is ignored rather than
    /// consulted: there is nothing behind it that could have moved.
    pub async fn plane(&self, now_ms: u64) -> Arc<ControlPlane> {
        match &self.backing {
            Backing::Fixed(plane) => Arc::clone(plane),
            Backing::Managed(managed) => managed.plane(now_ms).await,
        }
    }

    /// Every entity this deployment has, whoever owns it.
    ///
    /// Empty for a fixed directory, which is the accurate answer and not a
    /// placeholder: nothing can reach this on an open deployment, because the
    /// admin surface refuses that mode before any route runs.
    pub async fn view(&self, now_ms: u64) -> DirectoryView {
        self.snapshot(now_ms).await.1
    }

    /// The compiled plane and the entity listing, **at one version**.
    ///
    /// For the caller that needs both and has to explain what it read: the
    /// reconciliation view lists a project's memberships out of the listing and
    /// resolves each one's admission out of the plane, and taking those from two
    /// calls means taking them from two lock acquisitions. A write landing
    /// between them leaves a document whose members are a version ahead of the
    /// plane their admissions were resolved against — a member with a live key
    /// reported as having no admission at all, which that view spells as a row
    /// with no figures. Both halves leave through one guard here, so the answer
    /// is one version's answer, whichever version that turns out to be.
    ///
    /// Freshness is deliberately *not* the promise: a snapshot taken just before
    /// a write describes the state before it, and that is a correct answer to
    /// "what was true when this request arrived". What cannot happen is a
    /// document assembled from two of them.
    pub async fn snapshot(&self, now_ms: u64) -> (Arc<ControlPlane>, DirectoryView) {
        match &self.backing {
            Backing::Fixed(plane) => (
                Arc::clone(plane),
                DirectoryView {
                    projects: Vec::new(),
                    users: Vec::new(),
                    memberships: Vec::new(),
                    keys: Vec::new(),
                },
            ),
            Backing::Managed(managed) => managed.snapshot(now_ms).await,
        }
    }

    /// The [`BudgetTerms`] a membership's admission carries, derived from the
    /// rows rather than taken off a live key.
    ///
    /// **For the membership whose keys have all been revoked**, which is the one
    /// case where the two differ: its spend is still in the ledger and still
    /// binds its project's ceiling, but there is no admission left to read terms
    /// from. Terms invented at the reporting end would be a second authority
    /// over the window a balance is read under — and since a balance read rolls
    /// a lapsed window, the reporting end is exactly where that mistake hands a
    /// month's committed spend back. So they are derived here, through the same
    /// [`budget_terms`] pairing [`ControlPlaneConfig::validate`] runs for a live
    /// key, over the same project block and the same membership allocation the
    /// next compile would read: the same bytes the admission carried before the
    /// key was revoked.
    ///
    /// `Ok(None)` means the project has no budget — there was never a ledger
    /// position — and not that the terms could not be worked out.
    ///
    /// A fixed directory is refused rather than answered `None`, for the reason
    /// [`Self::managed`] gives: `None` would render as "this project has no
    /// budget", which is a claim about a deployment this call cannot see.
    pub fn membership_terms(
        &self,
        project: &ProjectRecord,
        membership: &MembershipRecord,
    ) -> Result<Option<BudgetTerms>, DirectoryError> {
        self.managed()?.membership_terms(project, membership)
    }

    /// Apply one change: validate it, compile the whole control plane it would
    /// produce, and only then write. See [`Managed::apply`].
    pub async fn apply(
        &self,
        mutation: DirectoryMutation,
        now_ms: u64,
    ) -> Result<Arc<DirectoryRecords>, DirectoryError> {
        self.managed()?.apply(mutation, now_ms).await
    }

    /// Mint a turn key for one membership and record it in one write.
    pub async fn mint_turn_key(
        &self,
        project: &str,
        user: &str,
        now_ms: u64,
    ) -> Result<MintedKey, DirectoryError> {
        self.managed()?.mint_turn_key(project, user, now_ms).await
    }

    /// Mint an admin key. See [`Managed::mint_turn_key`] on why this is one call.
    pub async fn mint_admin_key(&self, now_ms: u64) -> Result<MintedKey, DirectoryError> {
        self.managed()?.mint_admin_key(now_ms).await
    }

    /// The version this node last compiled, or `0` for a directory with nothing
    /// behind it to version.
    pub async fn version(&self, now_ms: u64) -> u64 {
        match &self.backing {
            Backing::Fixed(_) => 0,
            Backing::Managed(managed) => managed.version(now_ms).await,
        }
    }

    /// The writable half, or the refusal that says there is none.
    ///
    /// **Defence in depth rather than a path.** The admin router refuses
    /// [`ControlPlane::Open`] on its own mode check before any handler runs, so
    /// no request reaches a write on a fixed directory; this is what stops a
    /// future caller that forgets the gate from writing into a deployment that
    /// has no admin plane, and it answers with the same row that gate does.
    fn managed(&self) -> Result<&Managed, DirectoryError> {
        match &self.backing {
            Backing::Fixed(_) => Err(DirectoryError::NoAdminPlane),
            Backing::Managed(managed) => Ok(managed),
        }
    }
}

impl Managed {
    async fn new(
        file: ControlPlaneConfig,
        path: impl Into<String>,
        store: Arc<dyn DirectoryStore>,
        checks: CrossChecks,
        now_ms: u64,
    ) -> Result<Self, DirectoryError> {
        let path = path.into();
        let config = ConfigIdentities::of(&file);
        let ttl_ms = file
            .admission_cache_ttl_ms
            .unwrap_or(DEFAULT_ADMISSION_CACHE_TTL_MS);
        let loaded = store.load().await?;
        let plane = compile(&file, &path, &checks, &loaded.records)?;
        Ok(Self {
            file,
            path,
            config,
            store,
            checks,
            ttl_ms,
            current: RwLock::new(Compiled {
                version: loaded.version,
                records: Arc::new(loaded.records),
                plane,
                refreshed_at_ms: now_ms,
            }),
            write: tokio::sync::Mutex::new(()),
        })
    }

    /// The compiled plane and the records it was compiled from, refreshed if it
    /// is due — **and taken together**.
    ///
    /// The pair rather than either half alone, because the pairing is where a
    /// caller that needs both can go wrong: two calls take two locks, and
    /// nothing stops a write landing between them. Every return below hands back
    /// both fields out of the guard it is already holding, so no caller can
    /// assemble an answer from two versions even by accident. See
    /// [`ControlDirectory::snapshot`] for what that costs a reader who wanted
    /// the newest state instead.
    ///
    /// **Two conditions, and both are load-bearing.** The TTL alone would
    /// recompile a quiet deployment forever; the version alone would make every
    /// admission a store read. Together they cost one cheap version read per
    /// TTL when nothing is happening, and recompile exactly when something has.
    ///
    /// The elapsed test is `>=` rather than `>` so that a TTL of zero means what
    /// an operator writing zero means — refresh on every call — rather than
    /// "refresh on every call after the first millisecond".
    ///
    /// # Three windows, and no lock across a round trip (M16.0, R-D2)
    ///
    /// Until M16.0 the whole refresh — the version read, the load and the
    /// compile — ran under one write guard, and the doc here argued that was
    /// the cheaper side of a trade: compiling outside the lock would have every
    /// request that arrived during a refresh compile its own copy of the same
    /// plane. **That trade inverts the moment `load()` is a network round
    /// trip.** What the guard was buying was single flight; what it now costs
    /// is every concurrent admission queued behind one store request. So the
    /// two are separated: single flight is kept, and the lock is not what
    /// provides it.
    ///
    /// `current` is therefore taken three times and never held across an await:
    ///
    /// 1. a read, to answer "is a refresh even due";
    /// 2. a write, to re-ask that question and — if it is still true — stamp
    ///    `refreshed_at_ms`;
    /// 3. a write, to publish what was loaded.
    ///
    /// **The stamp is the single-flight token.** The first caller past the TTL
    /// writes it and goes to the store; every caller behind it re-reads the
    /// stamp in window two, finds it fresh, and serves the plane this node
    /// already has. That is the same one-refresh-per-TTL the old write guard
    /// enforced, without anything waiting on the store to learn it. A caller
    /// that arrives while a refresh is in flight is *deliberately* answered
    /// from the current plane rather than made to wait for the newer one: it
    /// was going to be answered from a plane up to one TTL old anyway, and the
    /// staleness bound is the promise, not the freshness.
    ///
    /// **Publishing is conditional on the version.** With the load outside the
    /// lock, two refreshes can be in flight at once and can finish out of
    /// order, so "I have finished" is not a reason to install anything: the
    /// slower one may be carrying the older records. A publish that ignored
    /// that would let a revocation arrive and then un-arrive, which is worse
    /// than one that arrives a TTL late. Newer wins, whoever got back first.
    /// [`Self::apply`] publishes under the same rule.
    ///
    /// **A refresh that fails keeps serving the last good plane**, and says so
    /// in the log. The alternative is a node that stops authenticating anything
    /// because the store blinked or because a variable moved out of the
    /// environment, which converts a degraded control plane into an outage. What
    /// it costs is that a revocation does not propagate while the failure lasts
    /// — which is why it is a warning and not a debug line.
    ///
    /// **Every failure backs off one TTL, and that is now uniform** (R-D3). The
    /// stamp lands in window two, ahead of the first fallible call, so a failed
    /// `version()`, a failed `load()` and a plane that will not compile all
    /// wait the same TTL before the next attempt. Before M16.0 the version read
    /// returned ahead of the stamp and was retried on *every* admission — the
    /// cheapest failure was the one retried hardest, and its warning fired once
    /// per request instead of once per TTL. The backoff is deliberate for the
    /// same reason it always was: the two ways a refresh fails here are a store
    /// outage and a config the environment can no longer satisfy, and both are
    /// failures that *last*. The price is unchanged — a revocation made during
    /// the failure can take up to two TTLs instead of one.
    async fn compiled(&self, now_ms: u64) -> (Arc<ControlPlane>, Arc<DirectoryRecords>) {
        // Window one. A read, because the common answer is "not due" and that
        // answer must not serialize a node's admissions against each other.
        {
            let current = self.read_current();
            if now_ms.saturating_sub(current.refreshed_at_ms) < self.ttl_ms {
                return taken(&current);
            }
        }
        // Window two: claim the refresh, or discover somebody else has. The
        // same test as window one, re-asked under a write guard — which is what
        // makes exactly one of a burst of concurrent callers the one that pays.
        // The re-ask guards a scheduling gap no scripted clock can reach (two
        // callers both past window one before either stamps), so no test
        // drives it: deleting it leaves the suite green and the race open.
        let claimed_version = {
            let mut current = self.write_current();
            if now_ms.saturating_sub(current.refreshed_at_ms) < self.ttl_ms {
                return taken(&current);
            }
            current.refreshed_at_ms = now_ms;
            current.version
        };
        // From here to window three no guard is held, which is the whole point:
        // both of the calls below may be round trips, and the compile between
        // them is the CPU cost this used to make every concurrent admission
        // wait behind.
        let version = match self.store.version().await {
            Ok(version) => version,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the control directory could not be re-read; serving the last compiled \
                     control plane, so a revocation made elsewhere is not yet in force here"
                );
                return taken(&self.read_current());
            }
        };
        if version == claimed_version {
            return taken(&self.read_current());
        }
        let loaded = match self.store.load().await {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the control directory's version moved but its records could not be read; \
                     serving the last compiled control plane"
                );
                return taken(&self.read_current());
            }
        };
        let plane = match self.compile(&loaded.records) {
            Ok(plane) => plane,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the control directory changed but the new state does not compile on this \
                     node; serving the last compiled control plane"
                );
                return taken(&self.read_current());
            }
        };
        // Window three. `>` and not `!=`: a refresh that finished late with
        // older records must leave the newer plane where it is, and hand its
        // own caller the newer one too.
        let mut current = self.write_current();
        if loaded.version > current.version {
            current.version = loaded.version;
            current.records = Arc::new(loaded.records);
            current.plane = plane;
        }
        taken(&current)
    }

    /// The plane alone, for the surfaces that only authenticate. See
    /// [`Self::compiled`].
    async fn plane(&self, now_ms: u64) -> Arc<ControlPlane> {
        self.compiled(now_ms).await.0
    }

    /// Every entity this deployment has, whoever owns it.
    ///
    /// File-owned rows are projected here rather than copied into the store —
    /// see the module doc — which is also why this is the only way to list
    /// them: [`ControlPlane::configured`] discards the config's entries as it
    /// builds its lookup tables, so the compiled plane cannot answer "what
    /// projects are there" at all.
    ///
    /// Refreshed through [`Self::compiled`], so a list is exactly as stale as an
    /// admission taken at the same instant — a `GET` that read straight from
    /// the store would show an operator rows this node is not yet
    /// authenticating against, which is a worse kind of wrong than being one
    /// TTL behind.
    ///
    /// The plane comes back beside it because it is the *same* compile, not a
    /// second one taken at the same instant: a caller that reads both — see
    /// [`ControlDirectory::snapshot`] — must not be able to get them from two
    /// versions, and the only way to promise that is to hand them over together.
    async fn snapshot(&self, now_ms: u64) -> (Arc<ControlPlane>, DirectoryView) {
        let (plane, records) = self.compiled(now_ms).await;
        (plane, self.listing(&records))
    }

    /// The file's rows merged with the store's, for records already taken.
    ///
    /// Separate from [`Self::snapshot`] so the projection cannot reach for the
    /// lock a second time: it is handed the records it is to project, and has no
    /// way to read a newer set halfway through building a listing out of them.
    fn listing(&self, records: &DirectoryRecords) -> DirectoryView {
        let mut view = DirectoryView {
            projects: self
                .file
                .projects
                .iter()
                .map(|entry| ProjectRecord {
                    entry: entry.clone(),
                    provenance: Provenance::Config,
                    created_at_ms: None,
                    archived_at_ms: None,
                })
                .collect(),
            users: self
                .file
                .users
                .iter()
                .map(|entry| UserRecord {
                    entry: entry.clone(),
                    provenance: Provenance::Config,
                    created_at_ms: None,
                })
                .collect(),
            memberships: Vec::new(),
            keys: Vec::new(),
        };
        // The file's memberships are implied by its keys, so two keys for one
        // person in one project are one membership — deduped here rather than
        // listed twice, which is what an operator rotating a secret would
        // otherwise see.
        let mut seen: HashSet<(&str, &str)> = HashSet::new();
        for key in &self.file.keys {
            if seen.insert((key.project.as_str(), key.user.as_str())) {
                view.memberships.push(MembershipRecord {
                    project: key.project.clone(),
                    user: key.user.clone(),
                    role: None,
                    allocation: None,
                    overrides: None,
                    provenance: Provenance::Config,
                    created_at_ms: None,
                });
            }
            view.keys.push(ApiKeyRecord {
                id: key_id(&key.key_sha256),
                key_sha256: key.key_sha256.clone(),
                display_tail: None,
                scope: KeyRecordScope::Turn {
                    project: key.project.clone(),
                    user: key.user.clone(),
                },
                provenance: Provenance::Config,
                created_at_ms: None,
                revoked_at_ms: None,
                // The one place a member window can come from: the file
                // declares it on the key entry, and no mutation writes one.
                fair_use: key.fair_use.clone(),
            });
        }
        for hash in &self.file.admin_keys {
            view.keys.push(ApiKeyRecord {
                id: key_id(hash),
                key_sha256: hash.clone(),
                display_tail: None,
                scope: KeyRecordScope::Admin,
                provenance: Provenance::Config,
                created_at_ms: None,
                revoked_at_ms: None,
                // An admin key belongs to no membership, so there is no scope a
                // rolling ceiling could be drawn against.
                fair_use: None,
            });
        }
        view.projects.extend(records.projects.iter().cloned());
        view.users.extend(records.users.iter().cloned());
        view.memberships.extend(records.memberships.iter().cloned());
        view.keys.extend(records.keys.iter().cloned());
        view
    }

    /// Apply one change: validate it, compile the whole control plane it would
    /// produce, and only then write.
    ///
    /// **In that order, always.** A store that took the write first would let
    /// an operator persist a configuration this deployment refuses to start
    /// under, and the symptom would be a process that will not come back up
    /// after the next restart — the failure furthest in time from its cause.
    ///
    /// The cascade in [`DirectoryMutation::DeleteMembership`] is part of this
    /// and not a convenience: a key whose membership is gone has no policy, no
    /// budget and no principal to resolve to, so leaving it live would be a
    /// secret that authenticates as nothing. It is *revoked* rather than
    /// deleted, so the operator who removed the member can still see that the
    /// key existed and stopped working, which is the question they will have.
    async fn apply(
        &self,
        mutation: DirectoryMutation,
        now_ms: u64,
    ) -> Result<Arc<DirectoryRecords>, DirectoryError> {
        let _write = self.write.lock().await;
        let loaded = self.store.load().await?;
        let mut next = loaded.records.clone();
        self.mutate(&mut next, mutation, now_ms)?;
        let plane = self.compile(&next)?;
        let version = self.store.commit(loaded.version, next.clone()).await?;
        let records = Arc::new(next);
        // Published under the same version rule a refresh uses, and for the
        // same reason: a refresh started before this write may still be in
        // flight with older records, and whichever of the two finishes last
        // must not be the one that decides. The commit above is what makes
        // this write the newer of the pair — a store that had moved under it
        // would have answered `Concurrent` rather than a version.
        {
            let mut current = self.write_current();
            if version > current.version {
                *current = Compiled {
                    version,
                    records: Arc::clone(&records),
                    plane,
                    refreshed_at_ms: now_ms,
                };
            }
        }
        Ok(records)
    }

    /// Mint a turn key for one membership and record it in one write.
    ///
    /// The secret is returned and nothing keeps it. Minting and applying are
    /// one call rather than two so a caller cannot hand a secret to an operator
    /// and then fail to store its hash — which is a key that works nowhere and
    /// looks, from the operator's side, exactly like one that works.
    async fn mint_turn_key(
        &self,
        project: &str,
        user: &str,
        now_ms: u64,
    ) -> Result<MintedKey, DirectoryError> {
        let minted = mint_key(KeyKind::Turn)?;
        self.apply(
            DirectoryMutation::MintTurnKey {
                project: project.to_string(),
                user: user.to_string(),
                key: KeyFingerprint::from(&minted),
            },
            now_ms,
        )
        .await?;
        Ok(minted)
    }

    /// Mint an admin key. See [`Self::mint_turn_key`] on why this is one call.
    async fn mint_admin_key(&self, now_ms: u64) -> Result<MintedKey, DirectoryError> {
        let minted = mint_key(KeyKind::Admin)?;
        self.apply(
            DirectoryMutation::MintAdminKey {
                key: KeyFingerprint::from(&minted),
            },
            now_ms,
        )
        .await?;
        Ok(minted)
    }

    /// See [`ControlDirectory::membership_terms`].
    ///
    /// Both halves go through the resolvers the compiler uses — and name the
    /// entry the way it names it — so a project block that would refuse a boot
    /// refuses here with the same sentence rather than with a second wording of
    /// the same complaint.
    fn membership_terms(
        &self,
        project: &ProjectRecord,
        membership: &MembershipRecord,
    ) -> Result<Option<BudgetTerms>, DirectoryError> {
        let budget = project
            .entry
            .budget
            .as_ref()
            .map(|budget| budget.to_budget(&self.path, &project_entry_label(project.id())))
            .transpose()?;
        let allocation = membership
            .allocation
            .as_ref()
            .map(|allocation| {
                allocation.to_allocation(
                    &self.path,
                    &key_entry_label(&membership.project, &membership.user),
                )
            })
            .transpose()?;
        Ok(budget_terms(budget, allocation))
    }

    /// The version this node last compiled. Observability, and the seam the
    /// staleness tests read.
    async fn version(&self, now_ms: u64) -> u64 {
        let _ = self.plane(now_ms).await;
        self.read_current().version
    }

    // -----------------------------------------------------------------------
    // The compile
    // -----------------------------------------------------------------------

    /// The file's entries plus the API's, judged by the one compiler, with the
    /// tombstones compiled in beside the live tables.
    ///
    /// Every key is built *from its membership* here rather than carrying a
    /// copy of that membership's entitlements in its own record. That is what
    /// makes two keys of one membership impossible to disagree — an
    /// `UpsertMembership` re-stamps all of them at the next compile, because
    /// there was never a second copy to update.
    fn compile(&self, records: &DirectoryRecords) -> Result<Arc<ControlPlane>, DirectoryError> {
        compile(&self.file, &self.path, &self.checks, records)
    }

    // -----------------------------------------------------------------------
    // The mutations
    // -----------------------------------------------------------------------

    /// Apply one mutation to a copy of the records, refusing anything the
    /// ownership rules forbid.
    ///
    /// Pure, and separate from the compile that follows it, because the two
    /// answer different questions: this one asks "may this caller do this",
    /// which is about provenance and identity, and the compile asks "would the
    /// result serve", which is about policy and the catalog. Answering both in
    /// one pass is how a 409 comes to be reported as a 422.
    fn mutate(
        &self,
        records: &mut DirectoryRecords,
        mutation: DirectoryMutation,
        now_ms: u64,
    ) -> Result<(), DirectoryError> {
        match mutation {
            DirectoryMutation::CreateProject { entry } => {
                self.refuse_taken(records, EntityKind::Project, &entry.id)?;
                records.projects.push(ProjectRecord {
                    entry,
                    provenance: Provenance::Admin,
                    created_at_ms: Some(now_ms),
                    archived_at_ms: None,
                });
            }
            DirectoryMutation::PatchProject { id, patch } => {
                self.refuse_config_project(&id)?;
                let project = records
                    .project(&id)
                    .ok_or_else(|| DirectoryError::UnknownProject { id: id.clone() })?;
                if project.is_archived() {
                    return Err(DirectoryError::ProjectIsArchived { id });
                }
                // After the three questions above, which are all "may this
                // project be patched at all" and are answered the same way
                // whatever the body said. Before the window guard, because a
                // nulled `budget` has no window to compare.
                if let Some(axis) = patch.explicit_null_axis() {
                    return Err(DirectoryError::NullPatchUnsupported { project: id, axis });
                }
                // Asked before anything is written, because the answer is about
                // the *transition* and there is nothing to compare against once
                // the new budget is in place.
                if let (Some(current), Some(next)) = (
                    &project.entry.budget,
                    patch.budget.as_ref().and_then(Option::as_ref),
                ) && current.window != next.window
                {
                    return Err(DirectoryError::WindowChangeUnsupported {
                        project: id,
                        from: current.window,
                        to: next.window,
                    });
                }
                let project = records
                    .project_mut(&id)
                    .expect("the project was found immutably a few lines above");
                // `Some(Some(value))` is the only arm that writes: `None` is an
                // absent field, and `Some(None)` was refused above rather than
                // reaching here — see [`ProjectPatch`].
                if let Some(Some(name)) = patch.name {
                    project.entry.name = Some(name);
                }
                if let Some(Some(policy)) = patch.policy {
                    project.entry.policy = Some(policy);
                }
                if let Some(Some(budget)) = patch.budget {
                    project.entry.budget = Some(budget);
                }
                // No transition guard above for this one, unlike `budget`: see
                // [`ProjectPatch::fair_use`] — a fair-use window has no
                // committed spend to reinterpret, so there is nothing a change
                // of window could destroy.
                if let Some(Some(fair_use)) = patch.fair_use {
                    project.entry.fair_use = Some(fair_use);
                }
                if let Some(Some(validate)) = patch.validate {
                    project.entry.validate = Some(validate);
                }
                if let Some(Some(credentials)) = patch.credentials {
                    project.entry.credentials = Some(credentials);
                }
            }
            DirectoryMutation::ArchiveProject { id } => {
                self.refuse_config_project(&id)?;
                let project = records
                    .project_mut(&id)
                    .ok_or_else(|| DirectoryError::UnknownProject { id: id.clone() })?;
                if project.is_archived() {
                    return Err(DirectoryError::ProjectIsArchived { id });
                }
                project.archived_at_ms = Some(now_ms);
            }
            DirectoryMutation::CreateUser { entry } => {
                self.refuse_taken(records, EntityKind::User, &entry.id)?;
                records.users.push(UserRecord {
                    entry,
                    provenance: Provenance::Admin,
                    created_at_ms: Some(now_ms),
                });
            }
            DirectoryMutation::UpsertMembership {
                project,
                user,
                role,
                allocation,
                overrides,
            } => {
                self.refuse_absent_project(records, &project)?;
                self.refuse_absent_user(records, &user)?;
                self.refuse_config_membership(&project, &user)?;
                match records
                    .memberships
                    .iter_mut()
                    .find(|membership| membership.names(&project, &user))
                {
                    Some(existing) => {
                        existing.role = Some(role);
                        existing.allocation = allocation;
                        existing.overrides = overrides;
                    }
                    None => records.memberships.push(MembershipRecord {
                        project,
                        user,
                        role: Some(role),
                        allocation,
                        overrides,
                        provenance: Provenance::Admin,
                        created_at_ms: Some(now_ms),
                    }),
                }
            }
            DirectoryMutation::DeleteMembership { project, user } => {
                self.refuse_config_membership(&project, &user)?;
                if records.membership(&project, &user).is_none() {
                    return Err(DirectoryError::UnknownMembership { project, user });
                }
                records
                    .memberships
                    .retain(|membership| !membership.names(&project, &user));
                // The cascade. See `apply`: a key whose membership is gone
                // resolves to nothing, and a tombstone is the only answer that
                // stays explicable afterwards.
                for key in &mut records.keys {
                    let mints_this_membership = matches!(
                        &key.scope,
                        KeyRecordScope::Turn { project: p, user: u } if *p == project && *u == user
                    );
                    if mints_this_membership && !key.is_revoked() {
                        key.revoked_at_ms = Some(now_ms);
                    }
                }
            }
            DirectoryMutation::MintTurnKey { project, user, key } => {
                self.refuse_config_membership(&project, &user)?;
                self.refuse_archived(records, &project)?;
                if records.membership(&project, &user).is_none() {
                    return Err(DirectoryError::UnknownMembership { project, user });
                }
                self.refuse_taken_hash(records, &key.key_sha256)?;
                records.keys.push(ApiKeyRecord {
                    id: key_id(&key.key_sha256),
                    key_sha256: key.key_sha256,
                    display_tail: Some(key.display_tail),
                    scope: KeyRecordScope::Turn { project, user },
                    provenance: Provenance::Admin,
                    created_at_ms: Some(now_ms),
                    revoked_at_ms: None,
                    // No route writes a member window; see the field's doc.
                    fair_use: None,
                });
            }
            DirectoryMutation::MintAdminKey { key } => {
                self.refuse_taken_hash(records, &key.key_sha256)?;
                records.keys.push(ApiKeyRecord {
                    id: key_id(&key.key_sha256),
                    key_sha256: key.key_sha256,
                    display_tail: Some(key.display_tail),
                    scope: KeyRecordScope::Admin,
                    provenance: Provenance::Admin,
                    created_at_ms: Some(now_ms),
                    revoked_at_ms: None,
                    fair_use: None,
                });
            }
            DirectoryMutation::RevokeKey { id } => {
                // A file-declared key is refused here, and that refusal is what
                // makes locking this deployment out of its own admin plane
                // impossible: the root of trust it booted with is not something
                // this API can remove. No "refuse to revoke the last key" rule
                // is needed, because there is no sequence of calls that reaches
                // the state such a rule would guard.
                if self.config.hashes.iter().any(|hash| key_id(hash) == id) {
                    return Err(DirectoryError::ConfigOwned {
                        kind: EntityKind::Key,
                        id,
                    });
                }
                let key = records
                    .keys
                    .iter_mut()
                    .find(|key| key.id == id)
                    .ok_or_else(|| DirectoryError::UnknownKey { id: id.clone() })?;
                // Idempotent: a second `DELETE` of a revoked key is the same
                // request arriving twice, and answering 404 or 409 to it would
                // make a retry after a dropped response look like a bug.
                if !key.is_revoked() {
                    key.revoked_at_ms = Some(now_ms);
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // The ownership rules, spelled once each
    // -----------------------------------------------------------------------

    /// Refuse a create whose identity is already somebody's.
    ///
    /// The file is checked first on purpose: an id the file owns must report
    /// `ConfigOwned` — which names the file and the remedy — rather than a bare
    /// collision, which would send an operator looking for an API row that does
    /// not exist.
    fn refuse_taken(
        &self,
        records: &DirectoryRecords,
        kind: EntityKind,
        id: &str,
    ) -> Result<(), DirectoryError> {
        let owned_by_file = match kind {
            EntityKind::Project => self.config.projects.contains(id),
            EntityKind::User => self.config.users.contains(id),
            _ => false,
        };
        if owned_by_file {
            return Err(DirectoryError::ConfigOwned {
                kind,
                id: id.to_string(),
            });
        }
        let taken = match kind {
            // An archived project still holds its id — see
            // `ProjectRecord::archived_at_ms` — so this deliberately does not
            // skip archived rows. Re-creating a project under a closed
            // project's id would join two tenants' spend histories under one
            // name.
            EntityKind::Project => records.project(id).is_some(),
            EntityKind::User => records.user(id).is_some(),
            _ => false,
        };
        if taken {
            return Err(DirectoryError::IdentityCollision {
                kind,
                id: id.to_string(),
            });
        }
        Ok(())
    }

    fn refuse_taken_hash(
        &self,
        records: &DirectoryRecords,
        key_sha256: &str,
    ) -> Result<(), DirectoryError> {
        // Vanishingly unlikely and checked anyway: the compiler would refuse a
        // duplicate hash with a message about a *file* an operator did not
        // write the second key in, and this is the one place that can say what
        // actually happened.
        if self.config.hashes.contains(key_sha256) || records.holds_hash(key_sha256) {
            return Err(DirectoryError::IdentityCollision {
                kind: EntityKind::Key,
                id: key_id(key_sha256),
            });
        }
        Ok(())
    }

    fn refuse_config_project(&self, id: &str) -> Result<(), DirectoryError> {
        if self.config.projects.contains(id) {
            return Err(DirectoryError::ConfigOwned {
                kind: EntityKind::Project,
                id: id.to_string(),
            });
        }
        Ok(())
    }

    /// Refuse a change to a membership the file declares.
    ///
    /// Minting a key under one counts as a change, and that is the sharp case:
    /// the file decides who may authenticate as that membership and with what
    /// entitlements, and a key added here would be a second answer to the first
    /// question that the next restart silently discards.
    fn refuse_config_membership(&self, project: &str, user: &str) -> Result<(), DirectoryError> {
        if self
            .config
            .memberships
            .contains(&(project.to_string(), user.to_string()))
        {
            return Err(DirectoryError::ConfigOwned {
                kind: EntityKind::Membership,
                id: format!("{project}/{user}"),
            });
        }
        Ok(())
    }

    /// Refuse a reference to a project neither half declares.
    ///
    /// A configured project is a perfectly good target — the API may *create*
    /// entities referencing what the file owns, it just may not edit it.
    fn refuse_absent_project(
        &self,
        records: &DirectoryRecords,
        id: &str,
    ) -> Result<(), DirectoryError> {
        if self.config.projects.contains(id) {
            return Ok(());
        }
        match records.project(id) {
            None => Err(DirectoryError::UnknownProject { id: id.to_string() }),
            Some(project) if project.is_archived() => {
                Err(DirectoryError::ProjectIsArchived { id: id.to_string() })
            }
            Some(_) => Ok(()),
        }
    }

    fn refuse_archived(&self, records: &DirectoryRecords, id: &str) -> Result<(), DirectoryError> {
        match records.project(id) {
            Some(project) if project.is_archived() => {
                Err(DirectoryError::ProjectIsArchived { id: id.to_string() })
            }
            _ => Ok(()),
        }
    }

    fn refuse_absent_user(
        &self,
        records: &DirectoryRecords,
        id: &str,
    ) -> Result<(), DirectoryError> {
        if self.config.users.contains(id) || records.user(id).is_some() {
            return Ok(());
        }
        Err(DirectoryError::UnknownUser { id: id.to_string() })
    }

    // -----------------------------------------------------------------------

    fn read_current(&self) -> std::sync::RwLockReadGuard<'_, Compiled> {
        self.current
            .read()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn write_current(&self) -> std::sync::RwLockWriteGuard<'_, Compiled> {
        self.current
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// Both halves of one compiled answer, out of the guard the caller is holding.
///
/// A free function taking `&Compiled` rather than a method on [`Managed`], so
/// that it is spelled the same at every return in [`Managed::compiled`] and
/// cannot be written as anything but a read of one already-held guard — which is
/// the whole property that function exists to have.
fn taken(current: &Compiled) -> (Arc<ControlPlane>, Arc<DirectoryRecords>) {
    (Arc::clone(&current.plane), Arc::clone(&current.records))
}

/// See [`ControlDirectory::compile`].
///
/// A free function rather than only a method so that [`ControlDirectory::new`]
/// can compile *before* it constructs. The method-only shape needed a
/// placeholder in the field it was about to fill, and the only available
/// placeholder was [`ControlPlane::Open`] — a value that means "no key is
/// required anywhere", parked for a few instructions inside the type whose
/// whole job is to require one.
fn compile(
    file: &ControlPlaneConfig,
    path: &str,
    checks: &CrossChecks,
    records: &DirectoryRecords,
) -> Result<Arc<ControlPlane>, DirectoryError> {
    let mut merged = file.clone();
    let archived: HashSet<&str> = records
        .projects
        .iter()
        .filter(|project| project.is_archived())
        .map(ProjectRecord::id)
        .collect();

    for project in &records.projects {
        // An archived project is left out of the compiled config entirely,
        // rather than compiled and then filtered: leaving it in would need
        // every reader of a project — the policy cross-check, the ambiguity
        // check, the router — to remember that some projects do not count.
        if !project.is_archived() {
            merged.projects.push(project.entry.clone());
        }
    }
    merged
        .users
        .extend(records.users.iter().map(|user| user.entry.clone()));

    let mut refusals: HashMap<String, KeyRefusal> = HashMap::new();
    for key in &records.keys {
        if key.is_revoked() {
            refusals.insert(key.key_sha256.clone(), KeyRefusal::Revoked);
            continue;
        }
        match &key.scope {
            KeyRecordScope::Admin => merged.admin_keys.push(key.key_sha256.clone()),
            KeyRecordScope::Turn { project, user } => {
                // Refused by its own row rather than by its membership's
                // absence: `project_archived` and `revoked_key` are told
                // apart because their remedies are opposite, and a key that
                // simply vanished from the table would be `unknown_key`,
                // which is neither.
                if archived.contains(project.as_str()) {
                    refusals.insert(key.key_sha256.clone(), KeyRefusal::ProjectArchived);
                    continue;
                }
                let membership = records.membership(project, user).ok_or_else(|| {
                    DirectoryError::Inconsistent {
                        detail: format!(
                            "key `{}` is minted under a membership (`{project}`, `{user}`) \
                             that no record names, and every route that removes a membership \
                             revokes its keys",
                            key.id
                        ),
                    }
                })?;
                merged.keys.push(KeyEntry {
                    project: project.clone(),
                    user: user.clone(),
                    key_sha256: key.key_sha256.clone(),
                    // Read off the membership on every compile. A copy on
                    // the key record would be the second place a
                    // membership's entitlements lived, and the first
                    // `UpsertMembership` after a second key was minted
                    // would leave the two disagreeing — which
                    // `ControlPlane::membership` refuses to describe at all.
                    overrides: membership.overrides.clone(),
                    allocation: membership.allocation.clone(),
                    // No per-key fair-use windows for the same reason as
                    // `credentials` below: M10.1 adds no admin-plane CRUD for
                    // them, so a member's own rolling ceiling stays a thing
                    // only the file can say. The project's windows still apply
                    // to an admin-minted key, because those are read off the
                    // project record every compile.
                    fair_use: None,
                    // No per-key credentials: M8 has no credential CRUD, so
                    // a member's own provider keys stay a thing only the
                    // file can say. See the milestone's R9.
                    credentials: None,
                });
            }
        }
    }

    merged.validate(path)?;
    let plane = ControlPlane::configured_with_refusals(merged, refusals);
    checks.refuse(&plane)?;
    Ok(Arc::new(plane))
}

#[cfg(test)]
mod tests;
