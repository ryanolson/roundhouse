// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Where admin-created tenancy lives: the seam, and nothing that implements
//! it.
//!
//! # There is no `MemoryDirectoryStore` any more (M16.1, R-D7)
//!
//! Until this rung this file also held a `Mutex` over the records and a
//! counter, and every directory test in the workspace ran against it — which
//! meant the round trip a real deployment performs on every read and every
//! write was exercised by nothing, and could only ever have been pinned by a
//! suite somebody remembered to write a second time.
//!
//! The one implementation now is
//! [`DocumentDirectoryStore`](super::DocumentDirectoryStore) over a
//! [`DocumentStore`](roundhouse_core::control::DocumentStore): a deployment
//! runs it over Redis, and every fixture runs it over `MemoryDocumentStore`,
//! so the codec is on the path of every test there is rather than beside them.
//! The compare-and-set this file's own tests used to drive lives where it is
//! implemented — `roundhouse-core`'s document-store contract, run against both
//! backends — and the adapter's *delegation* of it is pinned beside the
//! adapter.

use async_trait::async_trait;

use super::document::CompiledUnder;
use super::records::DirectoryRecords;

/// The records as they stood, the version they stood at, and what the node
/// that wrote them compiled against.
///
/// The first two travel together because a write is only safe against the
/// version it read — see [`DirectoryStore::commit`].
///
/// **The third rides along rather than being fetched separately (M16.1,
/// R-D9)**, and the alternative is what makes it worth stating: the divergence
/// check needs the *stored* fingerprint of the exact version it is about, so a
/// second call to ask "and what was that written under" would be a second
/// round trip answering about whatever the store held by then — a node warning
/// about inputs that belong to a version it never compiled. One read, one
/// answer, one version. A backend with nothing to fingerprint answers
/// [`CompiledUnder::default`], which is not a missing value but a real claim:
/// *this writer declared no inputs*.
#[derive(Debug, Clone)]
pub struct VersionedRecords {
    pub records: DirectoryRecords,
    pub version: u64,
    /// What the writer of *this version* said it compiled against. Compared
    /// against [`DirectoryStore::compiled_under`] by the reader; see
    /// [`ControlDirectory::divergence`](super::ControlDirectory::divergence).
    pub compiled_under: CompiledUnder,
}

/// Why a store could not answer.
///
/// Deliberately about the *store* and never about the request: a mutation that
/// is wrong is a [`DirectoryError`] decided before the store is touched. What
/// is left here is the two things a backend can say that no caller could have
/// prevented.
#[derive(Debug, thiserror::Error)]
pub enum StoreFailure {
    #[error(
        "the directory moved from version {expected} to {found} while this change was being \
         validated, so it was not applied -- read the current state and try again"
    )]
    Concurrent { expected: u64, found: u64 },
    #[error("the directory store is unavailable: {0}")]
    Unavailable(String),
}

/// Where admin-created tenancy lives.
///
/// # Why this is `load`/`commit` and not `apply(mutation)`
///
/// The obvious shape is a store that takes a mutation and applies it. It is not
/// implementable here, and the reason is the module doc's first rule: every
/// mutation is judged by [`ControlPlaneConfig::validate`] and by the
/// deployment's cross-checks, and that judgement sits *between* reading the
/// current records and writing the new ones. A store that applied mutations
/// internally would have to either compile a control plane itself — putting the
/// compiler behind this trait, where a Redis implementation would have to
/// reimplement it — or commit state that nothing had validated.
///
/// So the seam is a read, a validation the caller performs, and a
/// compare-and-set write. A backend that cannot do compare-and-set cannot
/// implement this trait, which is the right requirement: two nodes writing
/// tenancy without one is how a revocation gets overwritten by a concurrent
/// rename.
///
/// # Why every method is `async` (M16.0, R-D1)
///
/// The three were synchronous for as long as an in-memory `Mutex` was the only
/// implementation, and a clone of a `Vec` behind a lock is not something a
/// caller can usefully await. A durable store makes each of them a
/// network round trip, and a synchronous trait leaves exactly two ways to call
/// one from the async surfaces above: block a runtime worker on it, or hand it
/// to `spawn_blocking` and pay a thread per admission. Both are worse than the
/// honest shape, and neither could be swapped for it later without touching
/// every caller anyway — so the seam moved once, in M16.0, while the only
/// implementation was still the in-memory one and nothing about the move could
/// be blamed on a backend that did not exist yet.
///
/// `#[async_trait]` rather than a native `async fn` in a trait, matching
/// `roundhouse_core::control::CorrelationMaps` one crate over: a native
/// `async fn` is not dyn compatible at this toolchain, and this trait exists to
/// be held as `Arc<dyn DirectoryStore>` — one directory, whichever backing
/// store the boot chose.
///
/// # The version is monotone, by contract (R-D2′)
///
/// **An implementation's version only ever goes up.** Every [`commit`] returns
/// a version strictly greater than any version this store has previously
/// returned from [`commit`] or answered from [`version`], and [`version`] never
/// answers something lower than it answered before. A store that breaks that
/// has *regressed* — restored from a backup, flushed, or failed over to a
/// replica that had not caught up — and a regression is not something the
/// callers above can prevent; it is something they have to be able to name.
///
/// This is stated here rather than discovered per backend because the whole
/// refresh rung turns on it: [`ControlDirectory::plane`] publishes by comparing
/// versions, so under a store whose numbers can go down, "newer wins" silently
/// means "the store's current truth loses, forever". The requirement is
/// inherited by [`DocumentStore`](roundhouse_core::control::DocumentStore),
/// which is where both backends satisfy it: `MemoryDocumentStore`
/// structurally, with one counter only ever incremented, and the Redis store
/// deliberately, with a script that writes the version and the document it
/// describes together.
///
/// What the directory does when it happens anyway is not this trait's business
/// and is described where it is decided ([`ControlDirectory::plane`]'s refresh
/// doc): the store is the shared truth, so a regression is adopted and named,
/// never silently discarded. The alternative was tried and is worse — a node
/// that quietly kept its own higher version would drop its *own* admin writes
/// while answering them `2xx`.
///
/// [`commit`]: DirectoryStore::commit
/// [`version`]: DirectoryStore::version
/// [`ControlDirectory::plane`]: super::ControlDirectory::plane
#[async_trait]
pub trait DirectoryStore: Send + Sync + 'static {
    /// Every admin-created record, and the version they were read at.
    async fn load(&self) -> Result<VersionedRecords, StoreFailure>;

    /// Replace the records, if and only if the store is still at
    /// `expected_version`. Returns the new version, which is strictly greater
    /// than every version this store has handed out before — see the trait
    /// doc's monotone requirement.
    async fn commit(
        &self,
        expected_version: u64,
        records: DirectoryRecords,
    ) -> Result<u64, StoreFailure>;

    /// The current version, without paying to read the records.
    ///
    /// The cheap half of [`ControlDirectory::plane`]'s refresh: a node past its
    /// TTL asks this first and recompiles only if the answer moved, so a quiet
    /// deployment costs one version read per TTL rather than a compile.
    ///
    /// Never lower than an answer this store has already given — the trait
    /// doc's monotone requirement, which is what lets a caller read "lower than
    /// last time" as a regression rather than as an ordinary write.
    async fn version(&self) -> Result<u64, StoreFailure>;

    /// What *this node* compiles against, to be compared with what a loaded
    /// version was written against (M16.1, R-D9).
    ///
    /// Synchronous and defaulted, deliberately. Synchronous because it is not
    /// a question for the backend at all — it is a property of the handle,
    /// fixed for its life, since the file is read once at boot and the catalog
    /// and the fleet are what this process was built with; making it `async`
    /// would invite an implementation that went and asked something. Defaulted
    /// to the empty fingerprint because a store that declares no inputs is a
    /// coherent answer — every fixture is one — and forcing each double to
    /// spell `CompiledUnder::default()` would be ceremony that also made
    /// adding the method a change to every test file.
    fn compiled_under(&self) -> CompiledUnder {
        CompiledUnder::default()
    }
}
